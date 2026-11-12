use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tracing::info;

use git_dag_parser_core::parser::{self, DeltaCacheStats, ParseOptions, ParseResult};
use git_dag_parser_core::repo;
use git_dag_parser_core::size::format_size;

use crate::attribution::{Attribution, NOT_INTRODUCED};
use crate::paths::Paths;
use crate::reachability::{Liveness, Reachability};

/// Run the init command: parse a Git repository and summarize what it holds
pub fn run(repo_path: &Path, options: &ParseOptions) -> Result<()> {
    let start = Instant::now();

    info!("Initializing DAG from repository: {}", repo_path.display());

    // Validate repository
    let git_dir = repo::find_git_dir(repo_path)?;
    info!("Found .git directory: {}", git_dir.display());

    // Parse all objects from pack files and loose objects
    info!(
        "Parsing Git objects (delta cache {})...",
        format_size(options.delta_cache_limit)
    );
    let parse_start = Instant::now();
    let parsed =
        parser::parse_objects_with(&git_dir, options).context("Failed to parse Git objects")?;
    let parse_elapsed = parse_start.elapsed();

    info!("Ranking commits and finding the commit that introduced each object...");
    let attribution_start = Instant::now();
    let attribution = Attribution::calculate(&parsed).context("Failed to rank the commits")?;
    let attribution_elapsed = attribution_start.elapsed();
    info!(
        "Attribution: {} frontier rounds in {:?}",
        attribution.rounds, attribution_elapsed
    );

    info!("Finding the path where each object was introduced...");
    let paths_start = Instant::now();
    let paths = Paths::calculate(&parsed, &attribution);
    let paths_elapsed = paths_start.elapsed();

    info!("Finding the objects that refs, the index and reflogs keep...");
    let reachability_start = Instant::now();
    let reachability = Reachability::calculate(&parsed, &attribution);
    let reachability_elapsed = reachability_start.elapsed();

    let analysis_start = Instant::now();
    let analysis = analyze(&parsed, &attribution, &paths, &reachability);
    let analysis_elapsed = analysis_start.elapsed();

    print_summary(
        &parsed,
        &attribution,
        &paths,
        &reachability,
        &analysis,
        Timing {
            parse: parse_elapsed,
            attribution: attribution_elapsed,
            paths: paths_elapsed,
            reachability: reachability_elapsed,
            analysis: analysis_elapsed,
            total: start.elapsed(),
        },
    );

    Ok(())
}

/// Times of the phases of `init`
struct Timing {
    parse: Duration,
    attribution: Duration,
    paths: Duration,
    reachability: Duration,
    analysis: Duration,
    total: Duration,
}

/// The largest objects and the commits that added them, as lines for the
/// summary
fn analyze(
    parsed: &ParseResult,
    attribution: &Attribution,
    paths: &Paths,
    reachability: &Reachability,
) -> Vec<String> {
    let c = &parsed.container;
    let path = |p: Option<u32>| match p {
        Some(0) => "(root)".to_string(),
        Some(p) => String::from_utf8_lossy(&paths.bytes(p, &parsed.names)).into_owned(),
        None => "(no path)".to_string(),
    };
    let commit = |i: usize| {
        let commit = &c.commits[i];
        format!("{} {}", short(&commit.hash), date(commit.commit_time))
    };
    let added_by =
        |owner: Option<u32>| owner.map_or("(no commit)".to_string(), |o| commit(o as usize));
    let blob_line = |size: u64, i: usize| {
        format!(
            "  {:>10} {:>10} on disk  {:<11}  {}  {}",
            format_size(size),
            format_size(c.blobs[i].size_disk),
            format!("{:?}", reachability.blob[i]),
            added_by(attribution.blob_introduced_by(i)),
            path(paths.blob_path(i))
        )
    };
    let mut out = Vec::new();

    out.push("Largest blobs (size, disk, state, added by, path):".to_string());
    for (size, i) in largest(5, c.blobs.iter().map(|b| (b.size, b.index))) {
        out.push(blob_line(size, i));
    }

    out.push("Largest blobs that no ref keeps:".to_string());
    let not_ref = c
        .blobs
        .iter()
        .filter(|b| reachability.blob[b.index] != Liveness::Ref);
    let not_ref = largest(3, not_ref.map(|b| (b.size, b.index)));
    if not_ref.is_empty() {
        out.push("  (none: refs keep all blobs)".to_string());
    }
    for (size, i) in not_ref {
        out.push(blob_line(size, i));
    }

    out.push("Largest trees (size, entries, state, added by, path):".to_string());
    for (size, i) in largest(3, c.trees.iter().map(|t| (t.size, t.index))) {
        out.push(format!(
            "  {:>10} {:>8} entries  {:<11}  {}  {}",
            format_size(size),
            parsed.tree_entries.children(i).0.len(),
            format!("{:?}", reachability.tree[i]),
            added_by(attribution.tree_introduced_by(i)),
            path(paths.tree_path(i))
        ));
    }

    // The blob count and bytes that each commit adds and each path holds
    let mut by_commit = vec![(0u32, 0u64); c.commits.len()];
    let mut by_path = vec![(0u32, 0u64, 0u64); paths.paths.len()];
    for b in &c.blobs {
        if let Some(o) = attribution.blob_introduced_by(b.index) {
            by_commit[o as usize].0 += 1;
            by_commit[o as usize].1 += b.size;
        }
        if let Some(p) = paths.blob_path(b.index) {
            by_path[p as usize].0 += 1;
            by_path[p as usize].1 += b.size;
            by_path[p as usize].2 += b.size_disk;
        }
    }

    out.push("Commits that add the most blob data:".to_string());
    for (size, i) in largest(3, by_commit.iter().map(|&(_, size)| size).zip(0..)) {
        out.push(format!(
            "  {:>10} in {:>7} blobs  {}",
            format_size(size),
            by_commit[i].0,
            commit(i)
        ));
    }

    out.push("Paths with the most blob versions:".to_string());
    let counts = by_path.iter().map(|&(count, _, _)| u64::from(count));
    for (count, i) in largest(3, counts.zip(0..)) {
        out.push(format!(
            "  {:>7} versions {:>10} {:>10} on disk  {}",
            count,
            format_size(by_path[i].1),
            format_size(by_path[i].2),
            path(Some(i as u32))
        ));
    }
    out
}

/// The `k` items with the largest key, largest first. Of equal keys, the
/// lowest index comes first.
fn largest(k: usize, items: impl Iterator<Item = (u64, usize)>) -> Vec<(u64, usize)> {
    let mut heap = BinaryHeap::with_capacity(k + 1);
    for (key, i) in items {
        let item = (key, Reverse(i));
        if heap.len() < k {
            heap.push(Reverse(item));
        } else if heap.peek().is_some_and(|Reverse(min)| item > *min) {
            heap.pop();
            heap.push(Reverse(item));
        }
    }
    heap.into_sorted_vec()
        .into_iter()
        .map(|Reverse((key, Reverse(i)))| (key, i))
        .collect()
}

/// The first 10 hex digits of a hash
fn short(hash: &[u8]) -> String {
    hash[..5].iter().map(|b| format!("{b:02x}")).collect()
}

/// The UTC date of a Unix time, as YYYY-MM-DD (the days-to-civil algorithm
/// of Howard Hinnant)
fn date(time: i64) -> String {
    let z = time.div_euclid(86_400) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

/// Print a summary of the parsed objects
///
/// `scripts/benchmarks/harness.py` reads the `Parse:`, `Attribution:`,
/// `Paths:`, `Reachability:` and `Total:` lines of the timing section, and
/// takes the last `Total:` line. Keep them last.
fn print_summary(
    parsed: &ParseResult,
    attribution: &Attribution,
    paths: &Paths,
    reachability: &Reachability,
    analysis: &[String],
    timing: Timing,
) {
    let ParseResult {
        tree_entries,
        names,
        container,
        delta_cache,
        pass1,
        pass2,
        ..
    } = parsed;
    println!("\n=== DAG Summary ===");
    println!("Commits: {}", container.commits.len());
    println!("Trees:   {}", container.trees.len());
    println!("Blobs:   {}", container.blobs.len());
    println!("Tags:    {}", container.tags.len());
    println!(
        "Total:   {}",
        container.commits.len()
            + container.trees.len()
            + container.blobs.len()
            + container.tags.len()
    );
    println!(
        "Names:   {} distinct, {:.1} KB",
        names.len(),
        names.total_bytes() as f64 / 1024.0
    );
    println!("Tree entries: {}", tree_entries.entry_count());
    let mut cache = DeltaCacheStats::default();
    for stats in delta_cache {
        cache.add(stats);
    }
    println!(
        "Delta cache: {} hits, {} misses, {} evictions in {} packs",
        cache.hits,
        cache.misses,
        cache.evictions,
        delta_cache.len()
    );
    let introduced = |owners: &[u32]| owners.iter().filter(|&&c| c != NOT_INTRODUCED).count();
    println!(
        "Introduced by a commit: {} of {} trees, {} of {} blobs",
        introduced(&attribution.tree_introduced_by),
        container.trees.len(),
        introduced(&attribution.blob_introduced_by),
        container.blobs.len()
    );
    println!(
        "Corrected commit dates: {} of {} commits",
        attribution.corrected_commits,
        container.commits.len()
    );
    println!("Distinct paths: {}", paths.paths.len());
    println!(
        "Roots: {} refs, {} reflog entries, {} index objects",
        container.refs.len(),
        container.reflog_entries.len(),
        container.index_roots.len()
    );
    let totals = reachability.totals(container);
    for l in Liveness::ALL.into_iter().rev() {
        let total = totals[l as usize];
        println!(
            "{:<12} {} objects, {} on disk",
            format!("{l:?}:"),
            total.count,
            format_size(total.size_disk)
        );
    }

    println!("\n=== Analysis ===");
    for line in analysis {
        println!("{line}");
    }

    println!("\n=== Timing ===");
    println!("Pass 1: {:?}", pass1);
    println!("Pass 2: {:?}", pass2);
    println!("Parse:  {:?}", timing.parse);
    println!("Attribution: {:?}", timing.attribution);
    println!("Paths: {:?}", timing.paths);
    println!("Reachability: {:?}", timing.reachability);
    println!("Analysis: {:?}", timing.analysis);
    println!("Total:  {:?}", timing.total);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn largest_gives_the_k_largest_keys_and_the_lowest_index_on_a_tie() {
        let items = [(5, 0), (9, 1), (5, 2), (1, 3), (9, 4), (7, 5)];
        assert_eq!(
            largest(4, items.into_iter()),
            [(9, 1), (9, 4), (7, 5), (5, 0)]
        );
        assert_eq!(largest(10, items.into_iter()).len(), 6);
        assert!(largest(0, items.into_iter()).is_empty());
    }

    #[test]
    fn date_gives_the_utc_day() {
        assert_eq!(date(0), "1970-01-01");
        assert_eq!(date(951_782_400), "2000-02-29");
        assert_eq!(date(1_762_905_599), "2025-11-11");
        assert_eq!(date(-1), "1969-12-31");
    }
}
