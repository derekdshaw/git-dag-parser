mod bodies;
mod content;
mod loose;
mod pack;
mod refs;
mod scan;

use std::path::Path;

use anyhow::Result;
use hashbrown::HashMap;
use indicatif::ProgressBar;
use tracing::Level;

use crate::models::container::{CommitRaw, HashIndex, ObjectHash, TreeEntries};
use crate::models::{NameStore, ObjectContainer, Ref};

/// Raw commit data, every tree's entries, the tree entry names, the hash
/// index, and the container
pub type ParsedObjects = (
    HashMap<ObjectHash, CommitRaw>,
    TreeEntries,
    NameStore,
    HashIndex,
    ObjectContainer,
);

/// Parse all Git objects from a repository's object store
///
/// Two passes over the object store:
/// 1. Headers only, packs in offset order: every object, its kind and
///    sizes, every on-disk copy, and which copy is primary.
/// 2. Bodies of commits, trees and tags from their primary copies. Tree
///    entry children are resolved to indices as the bodies are read, since
///    pass 1 indexed every object.
///
/// Returns:
/// - Raw commit data
/// - Every tree's entries
/// - Tree entry names, which the tree entries refer to by id
/// - Hash index (for resolving hashes to indices)
/// - Object container, with commit trees and parents, tag targets and refs
///   resolved to indices
pub fn parse_objects(git_dir: &Path) -> Result<ParsedObjects> {
    let mut scanned = scan::scan_repository(git_dir)?;
    let bodies::Bodies {
        commits_raw,
        tree_entries,
        names,
    } = bodies::read_bodies(&mut scanned)?;
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

    Ok((commits_raw, tree_entries, names, hash_index, container))
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
    /// `GDP_BENCH_REPO=~/perfrepos/zed cargo bench --features bench --bench parser -- --include-ignored parse_objects_repo`
    #[divan::bench(ignore, sample_count = 3, sample_size = 1)]
    fn parse_objects_repo() -> super::ParsedObjects {
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
