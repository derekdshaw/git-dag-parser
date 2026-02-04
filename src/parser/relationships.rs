use std::sync::Arc;
use std::thread;

use anyhow::Result;
use crossbeam_channel::{bounded, Sender};
use hashbrown::HashMap;
use indicatif::{ProgressBar, ProgressStyle};
use tracing::info;

use crate::models::container::{CommitRaw, HashIndex, ObjectHash, TreeRaw, WalkResult};
use crate::models::ObjectContainer;

/// Build commit↔tree and commit↔blob relationships using parallel workers
///
/// Architecture:
/// - Main thread partitions commits across worker threads
/// - Worker threads walk commit trees and produce WalkResults (pure functions, read-only)
/// - Worker threads send results via channel to main thread
/// - Main thread merges results into ObjectContainer (exclusive access, no locks)
pub fn build_relationships(
    commits_raw: &HashMap<ObjectHash, CommitRaw>,
    trees_raw: &HashMap<ObjectHash, TreeRaw>,
    hash_index: &HashIndex,
    container: &mut ObjectContainer,
) -> Result<()> {
    let num_workers = num_cpus::get();
    let total_commits = commits_raw.len();

    info!(
        "Building relationships with {} workers for {} commits",
        num_workers, total_commits
    );

    // Progress bar
    let progress = ProgressBar::new(total_commits as u64);
    progress.set_style(
        ProgressStyle::default_bar()
            .template("{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} ({eta})")
            .expect("valid template")
            .progress_chars("#>-"),
    );

    // Channel for results (bounded to prevent memory blowup)
    let (tx, rx) = bounded::<WalkResult>(num_workers * 4);

    // Wrap read-only data in Arc for sharing (clone the referenced data, not the reference)
    let trees_raw: Arc<HashMap<ObjectHash, TreeRaw>> = Arc::new((*trees_raw).clone());
    let hash_index_trees: Arc<HashMap<ObjectHash, usize>> = Arc::new(hash_index.trees.clone());

    // Collect commits for partitioning
    let commits: Vec<_> = commits_raw.values().collect();
    let chunk_size = (commits.len() + num_workers - 1) / num_workers;

    // Spawn worker threads
    let workers: Vec<_> = commits
        .chunks(chunk_size)
        .enumerate()
        .map(|(worker_id, commit_chunk)| {
            let tx = tx.clone();
            let trees_raw = Arc::clone(&trees_raw);
            let hash_index_trees = Arc::clone(&hash_index_trees);

            // Clone the commit data for this worker
            let commits: Vec<CommitRaw> = commit_chunk
                .iter()
                .map(|c| CommitRaw {
                    index: c.index,
                    root_tree_hash: c.root_tree_hash,
                    parent_hashes: c.parent_hashes.clone(),
                })
                .collect();

            thread::spawn(move || {
                // Optionally pin to CPU core for cache locality
                if let Some(core_ids) = core_affinity::get_core_ids() {
                    if worker_id < core_ids.len() {
                        core_affinity::set_for_current(core_ids[worker_id]);
                    }
                }

                worker_loop(commits, trees_raw, hash_index_trees, tx);
            })
        })
        .collect();

    // Drop the sender in main thread so channel closes when workers finish
    drop(tx);

    // Main thread: merge results as they arrive
    let mut processed = 0;
    for result in rx {
        merge_walk_result(result, container);
        processed += 1;
        progress.set_position(processed);
    }

    // Wait for all workers to complete
    for worker in workers {
        let _ = worker.join();
    }

    progress.finish_with_message("Done");
    info!("Built relationships for {} commits", processed);

    Ok(())
}

/// Worker thread loop: walk commit trees and send results
fn worker_loop(
    commits: Vec<CommitRaw>,
    trees_raw: Arc<HashMap<ObjectHash, TreeRaw>>,
    hash_index_trees: Arc<HashMap<ObjectHash, usize>>,
    tx: Sender<WalkResult>,
) {
    for commit in commits {
        let result = walk_tree_pure(commit.index, &commit.root_tree_hash, &trees_raw, &hash_index_trees);

        // If send fails, channel is closed, worker should exit
        if tx.send(result).is_err() {
            break;
        }
    }
}

/// Pure function to walk a commit's tree and collect visited objects
///
/// This function has no side effects - it only reads from shared immutable data
/// and returns a WalkResult containing the relationships discovered.
fn walk_tree_pure(
    commit_idx: usize,
    root_tree_hash: &ObjectHash,
    trees_raw: &HashMap<ObjectHash, TreeRaw>,
    hash_index_trees: &HashMap<ObjectHash, usize>,
) -> WalkResult {
    let mut result = WalkResult::new(commit_idx);

    // Start walking from the root tree
    if let Some(tree_raw) = trees_raw.get(root_tree_hash) {
        walk_recursive(tree_raw, "", trees_raw, hash_index_trees, &mut result);
    }

    result
}

/// Recursively walk a tree, collecting all trees and blobs visited
fn walk_recursive(
    tree: &TreeRaw,
    path: &str,
    trees_raw: &HashMap<ObjectHash, TreeRaw>,
    hash_index_trees: &HashMap<ObjectHash, usize>,
    result: &mut WalkResult,
) {
    // Record this tree
    result.trees_visited.push((tree.index, path.to_string()));

    // Process each entry in the tree
    for entry in &tree.entries {
        let child_path = if path.is_empty() {
            entry.name.clone()
        } else {
            format!("{}/{}", path, entry.name)
        };

        if entry.is_tree {
            // Recurse into subtree
            // First try to find by resolved index, then by hash lookup
            if let Some(subtree) = entry
                .child_idx
                .and_then(|_| trees_raw.get(&entry.hash))
                .or_else(|| {
                    hash_index_trees
                        .get(&entry.hash)
                        .and_then(|_| trees_raw.get(&entry.hash))
                })
            {
                walk_recursive(subtree, &child_path, trees_raw, hash_index_trees, result);
            }
        } else {
            // Record blob (use resolved child_idx if available)
            if let Some(blob_idx) = entry.child_idx {
                result.blobs_visited.push((blob_idx, child_path));
            }
        }
    }
}

/// Merge a WalkResult into the ObjectContainer
///
/// This runs on the main thread with exclusive access to the container,
/// so no locks are needed.
fn merge_walk_result(result: WalkResult, container: &mut ObjectContainer) {
    let commit_idx = result.commit_idx;

    // Get mutable reference to the commit
    let commit = &mut container.commits[commit_idx];

    // Add tree relationships
    for (tree_idx, path) in result.trees_visited {
        commit.tree_deps.push(tree_idx);

        let tree = &mut container.trees[tree_idx];
        tree.commits.push(commit_idx);
        tree.paths.insert(path);
    }

    // Add blob relationships
    for (blob_idx, path) in result.blobs_visited {
        commit.blob_deps.push(blob_idx);

        let blob = &mut container.blobs[blob_idx];
        blob.commits.push(commit_idx);
        blob.paths.insert(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_walk_result_new() {
        let result = WalkResult::new(42);
        assert_eq!(result.commit_idx, 42);
        assert!(result.trees_visited.is_empty());
        assert!(result.blobs_visited.is_empty());
    }
}
