use std::path::Path;
use std::time::Instant;

use anyhow::{Context, Result};
use tracing::info;

use git_dag_parser_core::parser::{self, DeltaCacheStats, ParseOptions, ParseResult};
use git_dag_parser_core::repo;
use git_dag_parser_core::size::format_size;

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

    print_summary(&parsed, parse_elapsed, start.elapsed());

    Ok(())
}

/// Print a summary of the parsed objects
///
/// `scripts/benchmarks/harness.py` reads the `Parse:` and `Total:` lines of
/// the timing section, and takes the last `Total:` line. Keep them last.
fn print_summary(parsed: &ParseResult, parse: std::time::Duration, total: std::time::Duration) {
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

    println!("\n=== Timing ===");
    println!("Pass 1: {:?}", pass1);
    println!("Pass 2: {:?}", pass2);
    println!("Parse:  {:?}", parse);
    println!("Total:  {:?}", total);
}
