mod bodies;
mod content;
mod loose;
mod pack;
mod refs;
mod relationships;
mod scan;

use std::path::Path;
use std::time::Instant;

use anyhow::Result;
use hashbrown::HashMap;

use crate::models::container::{CommitRaw, HashIndex, ObjectHash, TreeRaw};
use crate::models::{ObjectContainer, Ref};

pub use relationships::build_relationships;

/// Raw commit and tree data, the hash index, and the container
pub type ParsedObjects = (
    HashMap<ObjectHash, CommitRaw>,
    HashMap<ObjectHash, TreeRaw>,
    HashIndex,
    ObjectContainer,
);

/// Parse all Git objects from a repository's object store
///
/// Two passes over the object store:
/// 1. Headers only, packs in offset order: every object, its kind and
///    sizes, every on-disk copy, and which copy is primary.
/// 2. Bodies of commits, trees and tags from their primary copies.
///
/// Returns:
/// - Raw commit data (for tree walking in Phase 2)
/// - Raw tree data (for tree walking in Phase 2)
/// - Hash index (for resolving hashes to indices)
/// - Object container (with empty relationship vectors), with commit trees
///   and parents, tag targets and refs resolved to indices
///
/// Static relationships (tree→blob, tree→subtree, blob→tree) are also
/// built here since they're inherent to the tree structure.
pub fn parse_objects(git_dir: &Path) -> Result<ParsedObjects> {
    let mut scanned = scan::scan_repository(git_dir)?;
    let bodies::Bodies {
        commits_raw,
        mut trees_raw,
    } = bodies::read_bodies(&mut scanned)?;
    // Packs are closed here; everything needed is in memory.
    let scan::Scan {
        mut container,
        index: hash_index,
        ..
    } = scanned.scan;

    // Resolve tree entry indices now that all objects are indexed
    let resolve_start = Instant::now();
    resolve_tree_entries(&mut trees_raw, &hash_index);
    println!("Resolved tree entry indices in {:?}", resolve_start.elapsed());

    resolve_commits(&commits_raw, &hash_index, &mut container);
    resolve_tags(&hash_index, &mut container);
    for (name, target_hash) in refs::read_refs(git_dir)? {
        let mut r = Ref::new(name, target_hash);
        r.target = hash_index.get(&target_hash);
        container.refs.push(r);
    }

    // Build static tree↔blob relationships
    let static_start = Instant::now();
    build_static_relationships(&trees_raw, &mut container);
    println!("Built static relationships in {:?}", static_start.elapsed());

    Ok((commits_raw, trees_raw, hash_index, container))
}

/// Resolve tree entry child indices after all objects have been indexed
fn resolve_tree_entries(trees_raw: &mut HashMap<ObjectHash, TreeRaw>, hash_index: &HashIndex) {
    for tree_raw in trees_raw.values_mut() {
        for entry in &mut tree_raw.entries {
            entry.child_idx = if entry.kind.is_tree() {
                hash_index.tree(&entry.hash)
            } else if entry.kind.is_blob() {
                hash_index.blob(&entry.hash)
            } else {
                None // Submodules reference external commits
            };
        }
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

/// Build static relationships that are inherent to the tree structure
/// These don't depend on which commits reference the trees
fn build_static_relationships(
    trees_raw: &HashMap<ObjectHash, TreeRaw>,
    container: &mut ObjectContainer,
) {
    for tree_raw in trees_raw.values() {
        let tree_idx = tree_raw.index;

        for entry in &tree_raw.entries {
            if let Some(child_idx) = entry.child_idx {
                if entry.kind.is_tree() {
                    // Tree → Subtree relationship
                    container.trees[tree_idx].subtrees.push(child_idx);
                } else {
                    // Tree → Blob relationship (regular files, executables, symlinks)
                    container.trees[tree_idx].blobs.push(child_idx);
                    // Blob → Tree relationship (reverse)
                    container.blobs[child_idx].trees.push(tree_idx);
                }
            }
        }
    }
}
