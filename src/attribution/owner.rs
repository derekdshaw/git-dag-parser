//! The owner of each tree and blob: the rank of the first commit, in rank
//! order, that reaches it
//!
//! `owner(o) = min(rank(c) for each commit c with root tree o,
//!                 owner(T) for each tree T that contains o)`
//!
//! The calculation moves through the tree graph one level at a time. A
//! tree joins the frontier when all trees that contain it are done, so its
//! owner is final when its entries are processed. `min` gives the same
//! result in any thread order.

use std::sync::atomic::{AtomicU32, Ordering::Relaxed};

use git_dag_parser_core::models::container::{FileKind, TreeEntries};
use git_dag_parser_core::models::Commit;
use rayon::prelude::*;

/// Owner value of an object that no commit reaches
pub(crate) const NO_OWNER: u32 = u32::MAX;

/// The rank of the owner of each tree and blob
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Owners {
    /// By tree index; `NO_OWNER` if no commit reaches the tree
    pub tree: Vec<u32>,
    /// By blob index; `NO_OWNER` if no commit reaches the blob
    pub blob: Vec<u32>,
    /// Number of frontier rounds
    pub rounds: usize,
}

/// Entries of one tree that one task processes. A large tree (a monorepo
/// can have one with about 84,000 entries) is cut into tasks of this size.
#[cfg(not(test))]
const ENTRY_CHUNK: usize = 1 << 14;
/// Small, so that the tests cut small trees into chunks
#[cfg(test)]
const ENTRY_CHUNK: usize = 2;

const TREE: u8 = FileKind::Tree as u8;

/// The owners of the trees of `entries` and of `blobs` blobs. `rank` gives
/// the rank of each commit, by commit index.
///
/// The values are atomic because many trees contain the same child, and
/// different threads process these trees at the same time. All operations
/// are `Relaxed`: each round of `rayon` work returns only after all its
/// tasks are done, so the next round sees all writes.
pub(crate) fn owners(
    commits: &[Commit],
    rank: &[u32],
    entries: &TreeEntries,
    blobs: usize,
) -> Owners {
    let trees = entries.len();
    let new = |n: usize| -> Vec<AtomicU32> {
        (0..n)
            .into_par_iter()
            .map(|_| AtomicU32::new(NO_OWNER))
            .collect()
    };
    let tree_owner = new(trees);
    let blob_owner = new(blobs);

    for c in commits {
        if let Some(root) = c.tree {
            tree_owner[root].fetch_min(rank[c.index], Relaxed);
        }
    }

    // The number of entries that point to each tree, duplicates included
    let in_degree: Vec<AtomicU32> = (0..trees)
        .into_par_iter()
        .map(|_| AtomicU32::new(0))
        .collect();
    (0..trees).into_par_iter().for_each(|t| {
        let (child, kind) = entries.children(t);
        for (&c, &k) in child.iter().zip(kind) {
            if k == TREE && c != TreeEntries::NO_CHILD {
                in_degree[c as usize].fetch_add(1, Relaxed);
            }
        }
    });

    // The first frontier: all trees that no tree contains, also the trees
    // that no commit reaches
    let mut frontier: Vec<u32> = (0..trees as u32)
        .into_par_iter()
        .filter(|&t| in_degree[t as usize].load(Relaxed) == 0)
        .collect();
    let mut rounds = 0;
    while !frontier.is_empty() {
        rounds += 1;
        // One task for each chunk of each frontier tree
        let tasks: Vec<(u32, u32)> = frontier
            .par_iter()
            .flat_map_iter(|&t| {
                let len = entries.children(t as usize).0.len();
                (0..len.div_ceil(ENTRY_CHUNK) as u32).map(move |chunk| (t, chunk))
            })
            .collect();
        frontier = tasks
            .par_iter()
            .fold(Vec::new, |mut next, &(t, chunk)| {
                let owner = tree_owner[t as usize].load(Relaxed);
                let (child, kind) = entries.children(t as usize);
                let lo = chunk as usize * ENTRY_CHUNK;
                let hi = child.len().min(lo + ENTRY_CHUNK);
                for (&c, &k) in child[lo..hi].iter().zip(&kind[lo..hi]) {
                    if c == TreeEntries::NO_CHILD {
                        continue;
                    }
                    let target = if k == TREE {
                        &tree_owner[c as usize]
                    } else {
                        &blob_owner[c as usize]
                    };
                    // Most children that many trees share already have
                    // their owner: a read costs less than a `fetch_min`,
                    // which takes the cache line from the other cores
                    if owner < target.load(Relaxed) {
                        target.fetch_min(owner, Relaxed);
                    }
                    if k == TREE && in_degree[c as usize].fetch_sub(1, Relaxed) == 1 {
                        next.push(c);
                    }
                }
                next
            })
            .reduce(Vec::new, |mut a, mut b| {
                a.append(&mut b);
                a
            });
    }

    let values =
        |v: Vec<AtomicU32>| -> Vec<u32> { v.into_par_iter().map(AtomicU32::into_inner).collect() };
    Owners {
        tree: values(tree_owner),
        blob: values(blob_owner),
        rounds,
    }
}

#[cfg(test)]
mod tests {
    use git_dag_parser_core::models::container::{FileKind, TreeEntriesBuilder, HASH_LEN};
    use git_dag_parser_core::models::{NameMap, NameStore};

    use super::*;

    /// A tree entry: a child tree, a blob, or a submodule
    #[derive(Clone, Copy)]
    enum E {
        T(usize),
        B(usize),
        Sub,
    }

    /// Tree entries with one entry list for each tree, in tree index order
    fn tree_entries(trees: &[Vec<E>]) -> TreeEntries {
        let mut names = NameStore::new();
        let name = names.intern(b"x").unwrap();
        let mut builder = TreeEntriesBuilder::new();
        for (t, entries) in trees.iter().enumerate() {
            builder.begin_tree(t);
            for &e in entries {
                match e {
                    E::T(i) => builder.push(Some(i), name, FileKind::Tree),
                    E::B(i) => builder.push(Some(i), name, FileKind::Regular),
                    E::Sub => builder.push(None, name, FileKind::Submodule),
                }
            }
        }
        TreeEntries::merge(
            trees.len(),
            vec![(builder, &NameMap::identity(names.len()))],
        )
    }

    /// Commits with these root trees, by commit index
    fn commits(roots: &[Option<usize>]) -> Vec<Commit> {
        roots
            .iter()
            .enumerate()
            .map(|(i, &tree)| {
                let mut c = Commit::new(i, [i as u8; HASH_LEN], 0, 0);
                c.tree = tree;
                c
            })
            .collect()
    }

    /// The owners from a walk of each commit in rank order with a "seen"
    /// set: the first commit that reaches an object owns it
    fn sequential_owners(
        commits: &[Commit],
        rank: &[u32],
        entries: &TreeEntries,
        blobs: usize,
    ) -> (Vec<u32>, Vec<u32>) {
        let mut order: Vec<usize> = (0..commits.len()).collect();
        order.sort_by_key(|&c| rank[c]);
        let mut tree = vec![NO_OWNER; entries.len()];
        let mut blob = vec![NO_OWNER; blobs];
        for c in order {
            let mut stack: Vec<usize> = commits[c].tree.into_iter().collect();
            while let Some(t) = stack.pop() {
                if tree[t] != NO_OWNER {
                    continue;
                }
                tree[t] = rank[c];
                for (child, _, kind) in entries.entries(t) {
                    match (child, kind.is_tree()) {
                        (Some(i), true) => stack.push(i),
                        (Some(i), false) if blob[i] == NO_OWNER => blob[i] = rank[c],
                        _ => {}
                    }
                }
            }
        }
        (tree, blob)
    }

    #[test]
    fn the_lower_rank_owns_a_shared_subtree() {
        // Commit 0 (rank 1) has root 0 with blob 1; commit 1 (rank 0) has
        // root 1. Both roots contain subtree 2 with blob 0.
        let entries = tree_entries(&[vec![E::T(2), E::B(1)], vec![E::T(2)], vec![E::B(0)]]);
        let o = owners(&commits(&[Some(0), Some(1)]), &[1, 0], &entries, 2);
        assert_eq!(o.tree, vec![1, 0, 0]);
        assert_eq!(o.blob, vec![0, 1]);
    }

    #[test]
    fn an_object_at_two_paths_of_one_tree_has_one_owner() {
        // Root 0 has blob 0 two times and subtree 1 two times
        let entries = tree_entries(&[vec![E::B(0), E::B(0), E::T(1), E::T(1)], vec![E::B(1)]]);
        let o = owners(&commits(&[Some(0)]), &[0], &entries, 2);
        assert_eq!(o.tree, vec![0, 0]);
        assert_eq!(o.blob, vec![0, 0]);
    }

    #[test]
    fn a_tree_that_no_commit_reaches_has_no_owner() {
        // Tree 1 and its blob 1 are in the store, but no commit has them
        let entries = tree_entries(&[vec![E::B(0)], vec![E::B(1)]]);
        let o = owners(&commits(&[Some(0)]), &[0], &entries, 2);
        assert_eq!(o.tree, vec![0, NO_OWNER]);
        assert_eq!(o.blob, vec![0, NO_OWNER]);
    }

    #[test]
    fn a_root_tree_that_is_also_a_subtree_gets_the_lower_rank() {
        // Commit 0 (rank 1) has root 0, which contains tree 1. Commit 1
        // (rank 0) has root 1. Tree 1 is not in the first frontier.
        let entries = tree_entries(&[vec![E::T(1), E::B(0)], vec![E::T(2)], vec![E::B(1)]]);
        let o = owners(&commits(&[Some(0), Some(1)]), &[1, 0], &entries, 2);
        assert_eq!(o.tree, vec![1, 0, 0]);
        assert_eq!(o.blob, vec![1, 0]);
        // The other way around: the commit with root 0 is first
        let o = owners(&commits(&[Some(0), Some(1)]), &[0, 1], &entries, 2);
        assert_eq!(o.tree, vec![0, 0, 0]);
        assert_eq!(o.blob, vec![0, 0]);
    }

    #[test]
    fn submodules_and_commits_without_a_tree_are_ignored() {
        let entries = tree_entries(&[vec![E::Sub, E::B(0), E::Sub]]);
        let o = owners(&commits(&[None, Some(0)]), &[0, 1], &entries, 1);
        assert_eq!(o.tree, vec![1]);
        assert_eq!(o.blob, vec![1]);
    }

    #[test]
    fn rounds_are_the_depth_of_the_tree_graph() {
        // 0 -> 1 -> 2 -> 3, and 0 -> 3
        let entries = tree_entries(&[vec![E::T(1), E::T(3)], vec![E::T(2)], vec![E::T(3)], vec![]]);
        let o = owners(&commits(&[Some(0)]), &[0], &entries, 0);
        assert_eq!(o.tree, vec![0; 4]);
        assert_eq!(o.rounds, 4);
    }

    #[test]
    fn no_commits_no_trees_and_no_blobs() {
        let o = owners(&[], &[], &tree_entries(&[]), 0);
        assert_eq!(
            o,
            Owners {
                tree: vec![],
                blob: vec![],
                rounds: 0
            }
        );
        let o = owners(&[], &[], &tree_entries(&[vec![E::B(0)]]), 1);
        assert_eq!((o.tree, o.blob), (vec![NO_OWNER], vec![NO_OWNER]));
    }

    /// A pseudo-random tree graph: tree `i` contains only trees with a
    /// larger index, so there is no cycle. Some trees have many entries, so
    /// that they are cut into chunks.
    fn random_graph(
        trees: usize,
        blobs: usize,
        commits: usize,
    ) -> (TreeEntries, Vec<Commit>, Vec<u32>) {
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = move |n: usize| {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((state >> 33) as usize) % n
        };
        let lists: Vec<Vec<E>> = (0..trees)
            .map(|t| {
                let len = if t % 17 == 0 { 40 } else { next(6) };
                (0..len)
                    .map(|_| match next(4) {
                        0 if t + 1 < trees => E::T(t + 1 + next(trees - t - 1)),
                        3 => E::Sub,
                        _ => E::B(next(blobs)),
                    })
                    .collect()
            })
            .collect();
        let roots: Vec<Option<usize>> = (0..commits)
            .map(|_| (next(10) > 0).then(|| next(trees)))
            .collect();
        // A permutation of 0..commits
        let rank = (0..commits)
            .map(|c| ((c * 7 + 3) % commits) as u32)
            .collect();
        (tree_entries(&lists), self::commits(&roots), rank)
    }

    #[test]
    fn owners_equal_a_sequential_walk_on_1_2_and_8_threads() {
        let (entries, commits, rank) = random_graph(300, 400, 50);
        let expected = sequential_owners(&commits, &rank, &entries, 400);
        assert!(
            expected.0.contains(&NO_OWNER),
            "some trees must be unreached"
        );
        for threads in [1, 2, 8] {
            let pool = rayon::ThreadPoolBuilder::new()
                .num_threads(threads)
                .build()
                .unwrap();
            let o = pool.install(|| owners(&commits, &rank, &entries, 400));
            assert_eq!((o.tree, o.blob), expected, "{threads} threads");
        }
    }
}
