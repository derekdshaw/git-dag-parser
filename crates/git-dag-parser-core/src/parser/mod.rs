mod bodies;
mod content;
mod delta_cache;
mod index;
mod loose;
mod pack;
mod reflogs;
mod refs;
mod scan;
mod work;

use std::path::Path;
use std::time::Duration;

use anyhow::Result;

use crate::models::container::{HashIndex, TreeEntries};
use crate::models::{NameStore, ObjectContainer, Ref};
use content::CommitRaw;

pub use delta_cache::DeltaCacheStats;

/// Target of the `info` events that report the progress of pass 2. The
/// parser draws no progress bar; a subscriber can draw one from these
/// events. Their fields:
/// - `total`: the number of bodies to read, at the start; the message names
///   the work
/// - `done`: the number of bodies read since the last event
/// - `finish`: `true` at the end
pub const PROGRESS_TARGET: &str = "git_dag_parser_core::progress";

/// The result of a parse
pub struct ParseResult {
    /// Every tree's entries
    pub tree_entries: TreeEntries,
    /// Tree entry names, which the tree entries refer to by id
    pub names: NameStore,
    /// Hash index, for resolving hashes to indices
    pub hash_index: HashIndex,
    /// Object container, with commit trees and parents, tag targets, refs,
    /// reflog entries and index roots resolved to indices
    pub container: ObjectContainer,
    /// The delta cache counts of pass 2 for each pack, in the order of
    /// `container.packs`
    pub delta_cache: Vec<DeltaCacheStats>,
    /// Time of pass 1 (headers)
    pub pass1: Duration,
    /// Time of pass 2 (bodies)
    pub pass2: Duration,
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
    crate::repo::check_object_format(git_dir)?;
    let mut scanned = scan::scan_repository(git_dir)?;
    let pass1 = scanned.elapsed;
    let bodies::Bodies {
        commits_raw,
        tree_entries,
        names,
        delta_cache,
        elapsed: pass2,
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
    container.reflog_entries = reflogs::read_reflogs(git_dir)?;
    reflogs::resolve(&mut container.reflog_entries, &hash_index);
    let mut index_roots: Vec<_> = index::read_index(git_dir)?
        .iter()
        .filter_map(|hash| hash_index.get(hash))
        .collect();
    index_roots.sort_unstable();
    index_roots.dedup();
    container.index_roots = index_roots;

    Ok(ParseResult {
        tree_entries,
        names,
        hash_index,
        container,
        delta_cache,
        pass1,
        pass2,
    })
}

/// The default number of threads of pass 1 and 2 on a machine with `hardware`
/// threads: a quarter of them, but up to 8 on a small machine, always
/// keeping one free. On 128 threads, pass 2 is no faster past about 32, and
/// each thread adds to the peak heap (about 15 MB on vscode).
pub fn default_threads(hardware: usize) -> usize {
    (hardware / 4).max(hardware.saturating_sub(1).min(8)).max(1)
}

/// Resolve each commit's root tree and parents to indices
fn resolve_commits(
    commits_raw: &[CommitRaw],
    hash_index: &HashIndex,
    container: &mut ObjectContainer,
) {
    for raw in commits_raw {
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
    /// `GDP_BENCH_REPO=/path/to/zed.git cargo bench -p git-dag-parser-core --features bench --bench parser -- --include-ignored parse_objects_repo`
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
