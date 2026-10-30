use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result};
use tracing::info;

use crate::error::DagError;
use crate::models::{NameStore, ObjectContainer};
use crate::parser;

/// Run the init command: parse a Git repository and summarize what it holds
pub fn run(repo_path: &Path) -> Result<()> {
    let start = Instant::now();

    info!("Initializing DAG from repository: {}", repo_path.display());

    // Validate repository
    let git_dir = find_git_dir(repo_path)?;
    info!("Found .git directory: {}", git_dir.display());

    // Parse all objects from pack files and loose objects
    info!("Parsing Git objects...");
    let parse_start = Instant::now();
    let (_commits_raw, _tree_entries, names, _hash_index, container) =
        parser::parse_objects(&git_dir).context("Failed to parse Git objects")?;
    let parse_elapsed = parse_start.elapsed();

    print_summary(&container, &names, parse_elapsed, start.elapsed());

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

/// Print a summary of the parsed objects
fn print_summary(
    container: &ObjectContainer,
    names: &NameStore,
    parse: std::time::Duration,
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
        names.len(),
        names.total_bytes() as f64 / 1024.0
    );

    println!("\n=== Timing ===");
    println!("Parse:  {:?}", parse);
    println!("Total:  {:?}", total);
}
