mod bodies;
mod content;
mod delta_cache;
mod loose;
mod pack;
mod refs;
mod scan;
mod work;

use std::path::Path;

use anyhow::Result;
use hashbrown::HashMap;
use indicatif::ProgressBar;
use tracing::Level;

use crate::models::container::{CommitRaw, HashIndex, ObjectHash, TreeEntries};
use crate::models::{NameStore, ObjectContainer, Ref};

/// The result of a parse
pub struct ParseResult {
    /// Every tree's entries
    pub tree_entries: TreeEntries,
    /// Tree entry names, which the tree entries refer to by id
    pub names: NameStore,
    /// Hash index, for resolving hashes to indices
    pub hash_index: HashIndex,
    /// Object container, with commit trees and parents, tag targets and refs
    /// resolved to indices
    pub container: ObjectContainer,
}

/// Options of a parse
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseOptions {
    /// Byte limit of the delta cache of pass 2. 0 caches nothing.
    pub delta_cache_limit: u64,
}

impl Default for ParseOptions {
    fn default() -> Self {
        Self {
            delta_cache_limit: 1 << 30,
        }
    }
}

/// Parse all Git objects from a repository's object store with the default
/// options
pub fn parse_objects(git_dir: &Path) -> Result<ParseResult> {
    parse_objects_with(git_dir, &ParseOptions::default())
}

/// Parse all Git objects from a repository's object store
///
/// Two passes over the object store:
/// 1. Headers only, packs in offset order: every object, its kind and
///    sizes, every on-disk copy, and which copy is primary.
/// 2. Bodies of commits, trees and tags from their primary copies. Tree
///    entry children are resolved to indices as the bodies are read, since
///    pass 1 indexed every object.
pub fn parse_objects_with(git_dir: &Path, options: &ParseOptions) -> Result<ParseResult> {
    let mut scanned = scan::scan_repository(git_dir)?;
    let bodies::Bodies {
        commits_raw,
        tree_entries,
        names,
    } = bodies::read_bodies(&mut scanned, options.delta_cache_limit)?;
    // Packs are closed here; everything needed is in memory.
    let scan::Scan {
        mut container,
        index: hash_index,
        ..
    } = scanned.scan;

    resolve_commits(&commits_raw, &hash_index, &mut container);
    resolve_tags(&hash_index, &mut container);
    for (name, target_hash) in refs::read_refs(git_dir)? {
        let mut r = Ref::new(name, target_hash);
        r.target = hash_index.get(&target_hash);
        container.refs.push(r);
    }

    Ok(ParseResult {
        tree_entries,
        names,
        hash_index,
        container,
    })
}

/// The default number of threads of pass 1 and 2 on a machine with `hardware`
/// threads: a quarter of them, but up to 8 on a small machine, always
/// keeping one free. On 128 threads, pass 2 is no faster past about 32, and
/// each thread adds to the peak heap (about 15 MB on vscode).
pub fn default_threads(hardware: usize) -> usize {
    (hardware / 4).max(hardware.saturating_sub(1).min(8)).max(1)
}

/// A progress bar of `len` steps that draws only when info logging is on.
/// Progress is reported through the program's logger like the other
/// messages, so a caller with no logger, such as the benchmarks, sees none.
fn progress_bar(len: u64) -> ProgressBar {
    if tracing::enabled!(Level::INFO) {
        ProgressBar::new(len)
    } else {
        ProgressBar::hidden()
    }
}

/// Resolve each commit's root tree and parents to indices
fn resolve_commits(
    commits_raw: &HashMap<ObjectHash, CommitRaw>,
    hash_index: &HashIndex,
    container: &mut ObjectContainer,
) {
    for raw in commits_raw.values() {
        let commit = &mut container.commits[raw.index];
        commit.tree = hash_index.tree(&raw.root_tree_hash);
        commit.parents = raw
            .parent_hashes
            .iter()
            .filter_map(|hash| hash_index.commit(hash))
            .collect();
        commit.boundary = commit.parents.len() < raw.parent_hashes.len();
        commit.commit_time = raw.commit_time;
    }
}

/// Resolve each annotated tag's target to an object id
fn resolve_tags(hash_index: &HashIndex, container: &mut ObjectContainer) {
    for tag in &mut container.tags {
        tag.target = hash_index.get(&tag.target_hash);
    }
}

#[cfg(feature = "bench")]
mod benches {
    use std::path::Path;

    /// A full parse of a real repository. Ignored by default because it
    /// needs a repository and takes seconds per run:
    ///
    /// `GDP_BENCH_REPO=/path/to/zed.git cargo bench --features bench --bench parser -- --include-ignored parse_objects_repo`
    ///
    /// `docs/benchmarks/README.md` gives the zed commit to use.
    ///
    /// Do not use its allocation counts for the memory of a parse. divan
    /// counts only the allocations of the thread that runs the benchmark,
    /// and pass 2 allocates most of its memory on the worker threads. Thus
    /// the counts and the peak are too low; use the harness for the peak
    /// heap. The time is correct. The benchmark runs on rayon's default
    /// pool (all hardware threads), not the thread count of the binary.
    #[divan::bench(ignore, sample_count = 3, sample_size = 1)]
    fn parse_objects_repo() -> super::ParseResult {
        let repo =
            std::env::var("GDP_BENCH_REPO").expect("set GDP_BENCH_REPO to a repository path");
        let path = Path::new(&repo);
        let git_dir = if path.join(".git").is_dir() {
            path.join(".git")
        } else {
            path.to_path_buf()
        };
        super::parse_objects(&git_dir).expect("parse_objects failed")
    }
}

#[cfg(test)]
mod tests {
    use super::default_threads;

    #[test]
    fn default_threads_are_a_quarter_of_a_large_machine() {
        assert_eq!(default_threads(128), 32);
        assert_eq!(default_threads(64), 16);
        assert_eq!(default_threads(32), 8);
    }

    #[test]
    fn default_threads_keep_one_thread_free_on_a_small_machine() {
        assert_eq!(default_threads(16), 8);
        assert_eq!(default_threads(8), 7);
        assert_eq!(default_threads(4), 3);
        assert_eq!(default_threads(2), 1);
        assert_eq!(default_threads(1), 1);
    }
}
