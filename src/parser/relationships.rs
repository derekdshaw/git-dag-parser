use std::sync::Arc;
use std::thread;

use anyhow::Result;
use crossbeam_channel::bounded;
use hashbrown::HashMap;
use indicatif::ProgressStyle;
use tracing::info;

use crate::models::container::{CommitRaw, HashIndex, ObjectHash, TreeEntry, TreeRaw, WalkResult};
use crate::models::{NameId, NameStore, ObjectContainer};

/// Shared read-only data passed to worker threads (no cloning, just Arc)
struct WorkerContext {
    trees_raw: HashMap<ObjectHash, TreeRaw>,
    commits_raw: HashMap<ObjectHash, CommitRaw>,
    names: NameStore,
}

/// Build commit↔tree and commit↔blob relationships using parallel workers
///
/// Uses tree-diff approach: for each commit, compare its tree to its parent's tree
/// and only record objects that are NEW or CHANGED in that commit. This is equivalent
/// to `git rev-list --objects <commit>~1..<commit>` but done in-process.
///
/// Takes ownership of commits_raw and trees_raw to avoid expensive clones.
/// These are wrapped in a single Arc and shared read-only across all workers.
pub fn build_relationships(
    commits_raw: HashMap<ObjectHash, CommitRaw>,
    trees_raw: HashMap<ObjectHash, TreeRaw>,
    names: NameStore,
    _hash_index: &HashIndex,
    container: &mut ObjectContainer,
) -> Result<()> {
    let num_workers = num_cpus::get();
    let total_commits = commits_raw.len();

    info!(
        "Building relationships: {} commits, {} workers",
        total_commits, num_workers
    );

    // Collect lightweight commit info for partitioning BEFORE moving into Arc
    let commit_work: Vec<(usize, ObjectHash, Vec<ObjectHash>)> = commits_raw
        .values()
        .map(|raw| (raw.index, raw.root_tree_hash, raw.parent_hashes.clone()))
        .collect();

    // Move data into Arc once - zero clones of the big HashMaps
    let context = Arc::new(WorkerContext {
        trees_raw,
        commits_raw,
        names,
    });

    // Progress bar
    let progress = super::progress_bar(total_commits as u64);
    progress.set_style(
        ProgressStyle::default_bar()
            .template("{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} ({percent}%) ({eta}) {msg}")
            .expect("valid template")
            .progress_chars("#>-"),
    );

    // Channel for results (bounded to prevent memory blowup)
    let (tx, rx) = bounded::<WalkResult>(num_workers * 4);

    let chunk_size = commit_work.len().div_ceil(num_workers);

    // Spawn worker threads
    let workers: Vec<_> = commit_work
        .chunks(chunk_size)
        .enumerate()
        .map(|(worker_id, chunk)| {
            let tx = tx.clone();
            let context = Arc::clone(&context);
            let chunk: Vec<_> = chunk.to_vec();

            thread::spawn(move || {
                if let Some(core_ids) = core_affinity::get_core_ids() {
                    if worker_id < core_ids.len() {
                        core_affinity::set_for_current(core_ids[worker_id]);
                    }
                }

                for (commit_idx, root_tree_hash, parent_hashes) in chunk {
                    let result =
                        diff_commit_tree(commit_idx, &root_tree_hash, &parent_hashes, &context);

                    if tx.send(result).is_err() {
                        break;
                    }
                }
            })
        })
        .collect();

    drop(tx);

    // Main thread: merge results as they arrive
    let mut processed = 0u64;
    let mut total_trees = 0u64;
    let mut total_blobs = 0u64;

    for result in rx {
        total_trees += result.trees_visited.len() as u64;
        total_blobs += result.blobs_visited.len() as u64;
        merge_walk_result(result, container);
        processed += 1;
        progress.set_position(processed);

        if processed.is_multiple_of(5000) {
            let avg_trees = total_trees / processed;
            let avg_blobs = total_blobs / processed;
            progress.set_message(format!("avg: {}t/{}b per commit", avg_trees, avg_blobs,));
        }
    }

    for worker in workers {
        let _ = worker.join();
    }

    progress.finish_with_message(format!(
        "done - {} trees, {} blobs linked",
        total_trees, total_blobs,
    ));

    info!("Built relationships for {} commits", processed);

    Ok(())
}

/// Diff a commit's tree against its parent's tree to find changed objects
///
/// For root commits (no parents), walks the entire tree.
/// For normal commits, only walks subtrees that differ from the first parent.
fn diff_commit_tree(
    commit_idx: usize,
    root_tree_hash: &ObjectHash,
    parent_hashes: &[ObjectHash],
    context: &WorkerContext,
) -> WalkResult {
    let mut result = WalkResult::new(commit_idx);

    let current_tree = match context.trees_raw.get(root_tree_hash) {
        Some(t) => t,
        None => return result,
    };

    // Get parent's root tree (if this commit has a parent)
    let parent_tree = parent_hashes
        .first()
        .and_then(|parent_hash| context.commits_raw.get(parent_hash))
        .and_then(|parent_commit| context.trees_raw.get(&parent_commit.root_tree_hash));

    diff_trees(current_tree, parent_tree, "", context, &mut result);

    result
}

/// Recursively diff two trees and collect only changed/new entries
///
/// If parent_tree is None (root commit), all entries are considered new.
/// Otherwise, only entries whose hash differs from the parent are walked.
fn diff_trees(
    current: &TreeRaw,
    parent: Option<&TreeRaw>,
    path: &str,
    context: &WorkerContext,
    result: &mut WalkResult,
) {
    let trees_raw = &context.trees_raw;

    // Build a lookup of parent entries by name for fast comparison. Names
    // are interned, so equal names have equal ids.
    let parent_entries: Option<HashMap<NameId, &TreeEntry>> =
        parent.map(|p| p.entries.iter().map(|e| (e.name, e)).collect());

    // Record this tree as visited
    result.trees_visited.push((current.index, path.to_string()));

    for entry in &current.entries {
        // Check if this entry exists with the same hash in the parent
        let is_unchanged = parent_entries
            .as_ref()
            .and_then(|pe| pe.get(&entry.name))
            .is_some_and(|parent_entry| parent_entry.hash == entry.hash);

        if is_unchanged {
            continue; // Same hash = identical content, skip
        }

        // Paths stay String until path interning; non-UTF-8 names are lossy here.
        let name = String::from_utf8_lossy(context.names.get(entry.name));
        let child_path = if path.is_empty() {
            name.into_owned()
        } else {
            format!("{}/{}", path, name)
        };

        if entry.kind.is_tree() {
            // Subtree is new or changed - recurse with diff
            if let Some(child_tree) = trees_raw.get(&entry.hash) {
                // Find the parent's version of this subtree for narrower diff
                let parent_subtree = parent_entries
                    .as_ref()
                    .and_then(|pe| pe.get(&entry.name))
                    .filter(|pe| pe.kind.is_tree())
                    .and_then(|pe| trees_raw.get(&pe.hash));

                diff_trees(child_tree, parent_subtree, &child_path, context, result);
            }
        } else {
            // Blob is new or changed
            if let Some(blob_idx) = entry.child_idx {
                result.blobs_visited.push((blob_idx, child_path));
            }
        }
    }
}

/// Merge a WalkResult into the ObjectContainer
///
/// Runs on the main thread with exclusive access, no locks needed.
fn merge_walk_result(result: WalkResult, container: &mut ObjectContainer) {
    let commit_idx = result.commit_idx;

    let commit = &mut container.commits[commit_idx];

    for (tree_idx, path) in result.trees_visited {
        commit.tree_deps.push(tree_idx);

        let tree = &mut container.trees[tree_idx];
        tree.commits.push(commit_idx);
        tree.paths.insert(path);
    }

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
