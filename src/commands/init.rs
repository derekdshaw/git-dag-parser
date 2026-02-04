use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tracing::info;

use crate::error::DagError;
use crate::models::ObjectContainer;
use crate::parser;

/// Run the init command: parse a Git repository and build the DAG
pub fn run(repo_path: &Path, output: &Option<PathBuf>) -> Result<()> {
    info!("Initializing DAG from repository: {}", repo_path.display());

    // Validate repository
    let git_dir = find_git_dir(repo_path)?;
    info!("Found .git directory: {}", git_dir.display());

    // Determine output path
    let output_path = output
        .clone()
        .unwrap_or_else(|| repo_path.join(".git-dag.bin"));

    // Phase 1: Parse all objects from pack files and loose objects
    info!("Phase 1: Parsing Git objects...");
    let (commits_raw, trees_raw, hash_index, mut container) =
        parser::parse_objects(&git_dir).context("Failed to parse Git objects")?;

    info!(
        "Parsed {} commits, {} trees, {} blobs, {} tags",
        container.commits.len(),
        container.trees.len(),
        container.blobs.len(),
        container.tags.len()
    );

    // Phase 2: Walk commit trees and build relationships
    info!("Phase 2: Building relationships...");
    parser::build_relationships(&commits_raw, &trees_raw, &hash_index, &mut container)
        .context("Failed to build relationships")?;

    // Save the DAG
    info!("Saving DAG to: {}", output_path.display());
    save_dag(&container, &output_path).context("Failed to save DAG")?;

    info!("Done! DAG saved to: {}", output_path.display());
    print_summary(&container);

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
        let content = std::fs::read_to_string(&git_dir)
            .context("Failed to read .git file")?;

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
    // For now, just create a placeholder
    let json = serde_json::to_string_pretty(container)
        .map_err(|e| DagError::Serialization(e.to_string()))?;

    std::fs::write(path, json)?;
    Ok(())
}

/// Print a summary of the parsed DAG
fn print_summary(container: &ObjectContainer) {
    println!("\n=== DAG Summary ===");
    println!("Commits: {}", container.commits.len());
    println!("Trees:   {}", container.trees.len());
    println!("Blobs:   {}", container.blobs.len());
    println!("Tags:    {}", container.tags.len());

    // Calculate total sizes
    let total_size: u64 = container.commits.iter().map(|c| c.size as u64).sum::<u64>()
        + container.trees.iter().map(|t| t.size as u64).sum::<u64>()
        + container.blobs.iter().map(|b| b.size as u64).sum::<u64>()
        + container.tags.iter().map(|t| t.size as u64).sum::<u64>();

    let total_disk_size: u64 = container.commits.iter().map(|c| c.size_disk as u64).sum::<u64>()
        + container.trees.iter().map(|t| t.size_disk as u64).sum::<u64>()
        + container.blobs.iter().map(|b| b.size_disk as u64).sum::<u64>()
        + container.tags.iter().map(|t| t.size_disk as u64).sum::<u64>();

    println!(
        "\nTotal size:      {} ({:.2} MB)",
        total_size,
        total_size as f64 / (1024.0 * 1024.0)
    );
    println!(
        "Total disk size: {} ({:.2} MB)",
        total_disk_size,
        total_disk_size as f64 / (1024.0 * 1024.0)
    );
}
