mod loose;
mod pack;
mod relationships;

use std::path::Path;
use std::time::Instant;

use anyhow::Result;
use hashbrown::HashMap;

use crate::models::container::{CommitRaw, HashIndex, ObjectHash, TreeRaw};
use crate::models::ObjectContainer;

pub use relationships::build_relationships;

/// Parse all Git objects from a repository's object store
///
/// This function reads both pack files and loose objects to build:
/// - Raw commit data (for tree walking in Phase 2)
/// - Raw tree data (for tree walking in Phase 2)
/// - Hash index (for resolving hashes to indices)
/// - Object container (with empty relationship vectors)
///
/// Static relationships (tree→blob, tree→subtree, blob→tree) are also
/// built during this phase since they're inherent to the tree structure.
pub fn parse_objects(
    git_dir: &Path,
) -> Result<(
    HashMap<ObjectHash, CommitRaw>,
    HashMap<ObjectHash, TreeRaw>,
    HashIndex,
    ObjectContainer,
)> {
    let mut commits_raw: HashMap<ObjectHash, CommitRaw> = HashMap::new();
    let mut trees_raw: HashMap<ObjectHash, TreeRaw> = HashMap::new();
    let mut hash_index = HashIndex::new();
    let mut container = ObjectContainer::new();

    // Parse pack files
    let pack_dir = git_dir.join("objects").join("pack");
    if pack_dir.is_dir() {
        pack::parse_pack_files(
            &pack_dir,
            &mut commits_raw,
            &mut trees_raw,
            &mut hash_index,
            &mut container,
        )?;
    }

    // Parse loose objects
    let objects_dir = git_dir.join("objects");
    if objects_dir.is_dir() {
        loose::parse_loose_objects(
            &objects_dir,
            &mut commits_raw,
            &mut trees_raw,
            &mut hash_index,
            &mut container,
        )?;
    }

    // Resolve tree entry indices now that all objects are indexed
    let resolve_start = Instant::now();
    resolve_tree_entries(&mut trees_raw, &hash_index);
    println!("Resolved tree entry indices in {:?}", resolve_start.elapsed());

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
                hash_index.trees.get(&entry.hash).copied()
            } else if entry.kind.is_blob() {
                hash_index.blobs.get(&entry.hash).copied()
            } else {
                None // Submodules reference external commits
            };
        }
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
