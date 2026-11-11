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

/// Return an error if the repository uses SHA-256 object hashes. The parser
/// reads only SHA-1 hashes of `HASH_LEN` bytes, so it would read a SHA-256
/// store wrong.
pub fn check_object_format(git_dir: &Path) -> Result<()> {
    let config = match std::fs::read(git_dir.join("config")) {
        Ok(bytes) => bytes,
        // Git reads a repository with no config file as SHA-1
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).context("Failed to read the git config"),
    };
    if uses_sha256(&String::from_utf8_lossy(&config)) {
        return Err(ParseError::UnsupportedObjectFormat.into());
    }
    Ok(())
}

/// True if `config` sets `extensions.objectFormat` to `sha256`. Section and
/// key names in git config are not case-sensitive; the value is.
fn uses_sha256(config: &str) -> bool {
    let mut in_extensions = false;
    for line in config.lines() {
        let line = line.trim();
        if let Some(header) = line.strip_prefix('[') {
            // A section with a subsection, such as `[remote "x"]`, is not
            // `[extensions]`.
            in_extensions = header
                .split_once(']')
                .is_some_and(|(name, _)| name.trim().eq_ignore_ascii_case("extensions"));
            continue;
        }
        if !in_extensions {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.trim().eq_ignore_ascii_case("objectformat") {
            let value = value.split(['#', ';']).next().unwrap_or_default();
            return value.trim().trim_matches('"') == "sha256";
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extensions_object_format_sha256_is_sha256() {
        let config =
            "[core]\n\trepositoryformatversion = 1\n[extensions]\n\tobjectformat = sha256\n";
        assert!(uses_sha256(config));
    }

    #[test]
    fn section_and_key_names_are_not_case_sensitive() {
        let config = "[Extensions]\n\tobjectFormat = sha256\n";
        assert!(uses_sha256(config));
    }

    #[test]
    fn a_config_without_the_key_is_sha1() {
        let config = "[core]\n\trepositoryformatversion = 0\n\tbare = false\n";
        assert!(!uses_sha256(config));
    }

    #[test]
    fn the_key_in_another_section_is_ignored() {
        let config =
            "[core]\n\tobjectformat = sha256\n[remote \"extensions\"]\n\tobjectformat = sha256\n";
        assert!(!uses_sha256(config));
    }

    #[test]
    fn comments_and_quotes_around_the_value_are_ignored() {
        assert!(uses_sha256(
            "[extensions]\n\tobjectformat = \"sha256\" # new format\n"
        ));
        assert!(!uses_sha256("[extensions]\n\t# objectformat = sha256\n"));
    }
}
