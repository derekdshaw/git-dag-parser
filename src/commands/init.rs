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

    print_summary(
        &parsed,
        &attribution,
        &paths,
        &reachability,
        Timing {
            parse: parse_elapsed,
            attribution: attribution_elapsed,
            paths: paths_elapsed,
            reachability: reachability_elapsed,
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
    total: Duration,
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

    println!("\n=== Timing ===");
    println!("Pass 1: {:?}", pass1);
    println!("Pass 2: {:?}", pass2);
    println!("Parse:  {:?}", timing.parse);
    println!("Attribution: {:?}", timing.attribution);
    println!("Paths: {:?}", timing.paths);
    println!("Reachability: {:?}", timing.reachability);
    println!("Total:  {:?}", timing.total);
}
