//! Finds the Git directory of a repository

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::error::ParseError;

/// Find the .git directory for a repository
pub fn find_git_dir(repo_path: &Path) -> Result<PathBuf> {
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

    Err(ParseError::NotAGitRepo(repo_path.display().to_string()).into())
}
