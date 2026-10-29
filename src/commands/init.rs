use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result};
use tracing::info;

use crate::error::DagError;
use crate::models::ObjectContainer;
use crate::parser;

/// Run the init command: parse a Git repository and build the DAG
pub fn run(repo_path: &Path, output: &Option<PathBuf>) -> Result<()> {
    let start = Instant::now();

    info!("Initializing DAG from repository: {}", repo_path.display());

    // Validate repository
    let git_dir = find_git_dir(repo_path)?;
    info!("Found .git directory: {}", git_dir.display());

    // Phase 1: Parse all objects from pack files and loose objects
    info!("Phase 1: Parsing Git objects...");
    let phase1_start = Instant::now();
    let (commits_raw, trees_raw, names, hash_index, mut container) =
        parser::parse_objects(&git_dir).context("Failed to parse Git objects")?;
    let phase1_elapsed = phase1_start.elapsed();
    // Phase 2 takes the store, so count it now for the summary
    let name_count = names.len();
    let name_bytes = names.total_bytes();

    // Phase 2: Walk commit trees and build relationships
    info!("Phase 2: Building relationships...");
    let phase2_start = Instant::now();
    parser::build_relationships(commits_raw, trees_raw, names, &hash_index, &mut container)
        .context("Failed to build relationships")?;
    let phase2_elapsed = phase2_start.elapsed();

    // Save the DAG if output path specified
    if let Some(output_path) = output {
        info!("Saving DAG to: {}", output_path.display());
        save_dag(&container, output_path).context("Failed to save DAG")?;
        println!("DAG saved to: {}", output_path.display());
    }

    let total_elapsed = start.elapsed();
    print_summary(
        &container,
        (name_count, name_bytes),
        phase1_elapsed,
        phase2_elapsed,
        total_elapsed,
    );

    Ok(())
}

/// Find the .git directory for a repository
fn find_git_dir(repo_path: &Path) -> Result<PathBuf> {
    let git_dir = repo_path.join(".git");

    if git_dir.is_dir() {
        return Ok(git_dir);
    }

    // Check if repo_path itself is a bare repository
    if repo_path.join("objects").is_dir() && repo_path.join("refs").is_dir() {
        return Ok(repo_path.to_path_buf());
    }

    // Check if .git is a file (worktree)
    if git_dir.is_file() {
        let content = std::fs::read_to_string(&git_dir).context("Failed to read .git file")?;

        if let Some(path) = content.strip_prefix("gitdir: ") {
            let path = path.trim();
            let resolved = if Path::new(path).is_absolute() {
                PathBuf::from(path)
            } else {
                repo_path.join(path)
            };

            if resolved.is_dir() {
                return Ok(resolved);
            }
        }
    }

    Err(DagError::NotAGitRepo(repo_path.display().to_string()).into())
}

/// Save the DAG to a binary file
fn save_dag(container: &ObjectContainer, path: &Path) -> Result<()> {
    // TODO: Implement binary serialization
    let json = serde_json::to_string_pretty(container)
        .map_err(|e| DagError::Serialization(e.to_string()))?;

    std::fs::write(path, json)?;
    Ok(())
}

/// Print a summary of the parsed DAG
fn print_summary(
    container: &ObjectContainer,
    (name_count, name_bytes): (usize, usize),
    phase1: std::time::Duration,
    phase2: std::time::Duration,
    total: std::time::Duration,
) {
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
        name_count,
        name_bytes as f64 / 1024.0
    );

    println!("\n=== Timing ===");
    println!("Phase 1 (parse):          {:?}", phase1);
    println!("Phase 2 (relationships):  {:?}", phase2);
    println!("Total:                    {:?}", total);
}
