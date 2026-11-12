//! The reachability state (liveness) of each object
//!
//! The roots are the ref targets (`Ref`), the objects of the index
//! (`IndexOnly`) and the old and new values of each reflog entry
//! (`ReflogOnly`). An object gets the strongest state of any root that
//! reaches it. An object that no root reaches is `Unreachable`: `git gc`
//! can remove it.

use std::sync::atomic::{AtomicU8, Ordering::Relaxed};

use git_dag_parser_core::models::container::{FileKind, TreeEntries};
use git_dag_parser_core::models::{ObjectContainer, ObjectId};
use git_dag_parser_core::parser::ParseResult;
use rayon::prelude::*;

use crate::attribution::Attribution;

/// Entries of one tree that one task processes, as in the owner
/// calculation of step 5
#[cfg(not(test))]
const ENTRY_CHUNK: usize = 1 << 14;
/// Small, so that the tests cut small trees into chunks
#[cfg(test)]
const ENTRY_CHUNK: usize = 2;

const TREE: u8 = FileKind::Tree as u8;

/// The reachability state of an object, weakest first. A reflog entry
/// expires; an index entry stays until the user changes the index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum Liveness {
    Unreachable = 0,
    ReflogOnly = 1,
    IndexOnly = 2,
    Ref = 3,
}

impl Liveness {
    /// All states, weakest first
    pub const ALL: [Liveness; 4] = [
        Liveness::Unreachable,
        Liveness::ReflogOnly,
        Liveness::IndexOnly,
        Liveness::Ref,
    ];
}

/// The liveness of each object, by container index of each type
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reachability {
    pub commit: Vec<Liveness>,
    pub tree: Vec<Liveness>,
    pub blob: Vec<Liveness>,
    pub tag: Vec<Liveness>,
}

/// Number of objects and their `size_disk` sum in one liveness state
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Total {
    pub count: usize,
    pub size_disk: u64,
}

impl Reachability {
    /// The liveness of each object of `parsed`. The commit ranks of
    /// `attribution` are a topological order: a parent has a lower rank
    /// than its children.
    pub fn calculate(parsed: &ParseResult, attribution: &Attribution) -> Self {
        Self::from_parts(&parsed.container, &parsed.tree_entries, &attribution.rank)
    }

    /// `calculate` from the container (objects, refs, reflog entries and
    /// index roots), the tree entries, and the rank of each commit
    ///
    /// Commits: after the roots are marked, one scan from the highest rank
    /// to the lowest gives each parent the strongest state of its
    /// children. A commit has its final state when the scan gets to it.
    ///
    /// Trees and blobs: the walk from the roots of the strongest state goes
    /// first. A walk from the roots of a weaker state stops at an object
    /// that already has a state, because the stronger walk also reached
    /// all objects below it. Thus each tree is walked at most one time,
    /// and no topological order is necessary.
    pub(crate) fn from_parts(
        container: &ObjectContainer,
        entries: &TreeEntries,
        rank: &[u32],
    ) -> Self {
        let c = container;
        let mut tag = vec![0u8; c.tags.len()];
        let mut by_rank = vec![0u8; c.commits.len()];
        let new = |n: usize| -> Vec<AtomicU8> {
            (0..n).into_par_iter().map(|_| AtomicU8::new(0)).collect()
        };
        let tree = new(c.trees.len());
        let blob = new(c.blobs.len());
        // The trees to walk for each state. A tree goes in a list only
        // from the `fetch_max` that raised it to that state.
        let mut walk: [Vec<u32>; 4] = Default::default();
        let mut mark_tree = |t: usize, l: u8| {
            if tree[t].fetch_max(l, Relaxed) < l {
                walk[l as usize].push(t as u32);
            }
        };

        let refs = c.refs.iter().filter_map(|r| r.target);
        let reflogs = c
            .reflog_entries
            .iter()
            .flat_map(|e| [e.old_target, e.new_target])
            .flatten();
        let roots = refs
            .map(|id| (Liveness::Ref, id))
            .chain(c.index_roots.iter().map(|&id| (Liveness::IndexOnly, id)))
            .chain(reflogs.map(|id| (Liveness::ReflogOnly, id)));
        for (level, root) in roots {
            let l = level as u8;
            // A tag gives its state to its target, also to another tag
            let mut next = Some(root);
            while let Some(id) = next.take() {
                match id {
                    ObjectId::Tag(i) if tag[i] < l => {
                        tag[i] = l;
                        next = c.tags[i].target;
                    }
                    ObjectId::Commit(i) => {
                        let r = rank[i] as usize;
                        by_rank[r] = by_rank[r].max(l);
                    }
                    ObjectId::Tree(i) => mark_tree(i, l),
                    ObjectId::Blob(i) => {
                        blob[i].fetch_max(l, Relaxed);
                    }
                    ObjectId::Tag(_) => {}
                }
            }
        }

        // The parents of each commit as ranks, in rank order: the parents
        // of rank `r` are at `first[r]..first[r + 1]`. They are read from
        // all threads, because each `Commit` is a random read. The scan on
        // one thread then reads them in order.
        let mut order = vec![0u32; c.commits.len()];
        for (i, &r) in rank.iter().enumerate() {
            order[r as usize] = i as u32;
        }
        let counts: Vec<usize> = order
            .par_iter()
            .map(|&i| c.commits[i as usize].parents.len())
            .collect();
        let mut first = Vec::with_capacity(order.len() + 1);
        first.push(0);
        for count in counts {
            first.push(first[first.len() - 1] + count);
        }
        let parents: Vec<u32> = order
            .par_iter()
            .flat_map_iter(|&i| c.commits[i as usize].parents.iter().map(|&p| rank[p]))
            .collect();
        for r in (0..by_rank.len()).rev() {
            let l = by_rank[r];
            if l > 0 {
                for &p in &parents[first[r]..first[r + 1]] {
                    let p = &mut by_rank[p as usize];
                    *p = (*p).max(l);
                }
            }
        }
        let commit: Vec<u8> = rank.par_iter().map(|&r| by_rank[r as usize]).collect();

        let root_trees: Vec<(u8, u32)> = c
            .commits
            .par_iter()
            .zip(&commit)
            .filter_map(|(commit, &l)| {
                let t = commit.tree?;
                (l > 0 && tree[t].fetch_max(l, Relaxed) < l).then_some((l, t as u32))
            })
            .collect();
        for (l, t) in root_trees {
            walk[l as usize].push(t);
        }

        for level in [Liveness::Ref, Liveness::IndexOnly, Liveness::ReflogOnly] {
            let l = level as u8;
            // A stronger walk can have raised a tree of this list
            let frontier: Vec<u32> = std::mem::take(&mut walk[l as usize])
                .into_par_iter()
                .filter(|&t| tree[t as usize].load(Relaxed) == l)
                .collect();
            walk_trees(entries, &tree, &blob, frontier, l);
        }

        let state = |v: u8| Liveness::ALL[v as usize];
        let states = |v: Vec<AtomicU8>| -> Vec<Liveness> {
            v.into_par_iter().map(|a| state(a.into_inner())).collect()
        };
        Self {
            commit: commit.into_par_iter().map(state).collect(),
            tree: states(tree),
            blob: states(blob),
            tag: tag.into_iter().map(state).collect(),
        }
    }

    /// Count and `size_disk` of all objects in each state, by
    /// `Liveness as usize`
    pub fn totals(&self, container: &ObjectContainer) -> [Total; 4] {
        let c = container;
        let mut totals = [Total::default(); 4];
        let sizes = c
            .commits
            .iter()
            .map(|o| o.size_disk)
            .zip(&self.commit)
            .chain(c.trees.iter().map(|o| o.size_disk).zip(&self.tree))
            .chain(c.blobs.iter().map(|o| o.size_disk).zip(&self.blob))
            .chain(c.tags.iter().map(|o| o.size_disk).zip(&self.tag));
        for (size_disk, &l) in sizes {
            let total = &mut totals[l as usize];
            total.count += 1;
            total.size_disk += size_disk;
        }
        totals
    }
}

/// Give state `l` to all trees and blobs below `frontier` that have a
/// weaker state, on all threads, one tree level at a time. The trees of
/// `frontier` already have `l`. Each tree joins the next frontier only
/// from the thread whose `fetch_max` changed it, so it is walked one time.
/// `Relaxed` is enough: each round returns only after all its tasks are
/// done.
fn walk_trees(
    entries: &TreeEntries,
    tree: &[AtomicU8],
    blob: &[AtomicU8],
    mut frontier: Vec<u32>,
    l: u8,
) {
    while !frontier.is_empty() {
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
                let (child, kind) = entries.children(t as usize);
                let lo = chunk as usize * ENTRY_CHUNK;
                let hi = child.len().min(lo + ENTRY_CHUNK);
                for (&c, &k) in child[lo..hi].iter().zip(&kind[lo..hi]) {
                    if c == TreeEntries::NO_CHILD {
                        continue;
                    }
                    // Most children already have the state: a read costs
                    // less than a `fetch_max`
                    if k == TREE {
                        let target = &tree[c as usize];
                        if target.load(Relaxed) < l && target.fetch_max(l, Relaxed) < l {
                            next.push(c);
                        }
                    } else {
                        let target = &blob[c as usize];
                        if target.load(Relaxed) < l {
                            target.fetch_max(l, Relaxed);
                        }
                    }
                }
                next
            })
            .reduce(Vec::new, |mut a, mut b| {
                a.append(&mut b);
                a
            });
    }
}

#[cfg(test)]
mod tests {
    use git_dag_parser_core::models::container::{FileKind, TreeEntriesBuilder, HASH_LEN};
    use git_dag_parser_core::models::{
        Blob, Commit, NameMap, NameStore, ObjectId, Ref, ReflogEntry, Tag, Tree,
    };

    use super::Liveness::*;
    use super::*;

    #[derive(Clone, Copy)]
    enum E {
        T(usize),
        B(usize),
    }

    /// A container with these commits (parents, root tree), trees and blobs
    /// (sizes on disk are 10 times the index plus 1), and tags (target)
    fn graph(
        commits: &[(&[usize], usize)],
        trees: &[Vec<E>],
        blobs: usize,
        tags: &[ObjectId],
    ) -> (ObjectContainer, TreeEntries) {
        let mut c = ObjectContainer::new();
        for (i, &(parents, tree)) in commits.iter().enumerate() {
            let mut commit = Commit::new(i, [i as u8; HASH_LEN], 0, 10 * i as u64 + 1);
            commit.parents = parents.to_vec();
            commit.tree = Some(tree);
            c.commits.push(commit);
        }
        for i in 0..trees.len() {
            c.trees
                .push(Tree::new(i, [i as u8; HASH_LEN], 0, 10 * i as u64 + 1));
        }
        for i in 0..blobs {
            c.blobs
                .push(Blob::new(i, [i as u8; HASH_LEN], 0, 10 * i as u64 + 1));
        }
        for (i, &target) in tags.iter().enumerate() {
            let mut tag = Tag::new(i, [i as u8; HASH_LEN], 0, 10 * i as u64 + 1);
            tag.target = Some(target);
            c.tags.push(tag);
        }
        let mut names = NameStore::new();
        let name = names.intern(b"x").unwrap();
        let mut builder = TreeEntriesBuilder::new();
        for (t, entries) in trees.iter().enumerate() {
            builder.begin_tree(t);
            for &e in entries {
                match e {
                    E::T(i) => builder.push(Some(i), name, FileKind::Tree),
                    E::B(i) => builder.push(Some(i), name, FileKind::Regular),
                }
            }
        }
        let map = NameMap::identity(names.len());
        (c, TreeEntries::merge(trees.len(), vec![(builder, &map)]))
    }

    fn reflog(old: Option<ObjectId>, new: Option<ObjectId>) -> ReflogEntry {
        ReflogEntry {
            ref_name: b"HEAD".to_vec(),
            old_hash: [0; HASH_LEN],
            new_hash: [0; HASH_LEN],
            old_target: old,
            new_target: new,
            time: 0,
            message: Vec::new(),
        }
    }

    fn reference(target: ObjectId) -> Ref {
        let mut r = Ref::new(b"refs/heads/main".to_vec(), [0; HASH_LEN]);
        r.target = Some(target);
        r
    }

    #[test]
    fn the_order_is_unreachable_reflog_index_ref() {
        assert!(Unreachable < ReflogOnly && ReflogOnly < IndexOnly && IndexOnly < Ref);
        assert_eq!(Liveness::ALL.map(|l| l as usize), [0, 1, 2, 3]);
    }

    #[test]
    fn an_object_gets_the_strongest_state_of_any_root_that_reaches_it() {
        // Commits: 0 <- 1 <- 2 (main), 1 <- 3 (only a reflog), 4 (nothing)
        // Trees: 0 {b0}, 1 {t0, b1}, 2 {b2}, 3 {b3}, 4 {b4} (index),
        // 5 {b4} (root of commit 3). Blob 5 is in the index; blob 6 has
        // nothing.
        // Tags: 0 -> 1 -> tree 2 (ref), 2 -> commit 4 (nothing)
        let (mut c, entries) = graph(
            &[(&[], 0), (&[0], 1), (&[1], 1), (&[1], 5), (&[], 3)],
            &[
                vec![E::B(0)],
                vec![E::T(0), E::B(1)],
                vec![E::B(2)],
                vec![E::B(3)],
                vec![E::B(4)],
                vec![E::B(4)],
            ],
            7,
            &[ObjectId::Tag(1), ObjectId::Tree(2), ObjectId::Commit(4)],
        );
        c.refs = vec![reference(ObjectId::Commit(2)), reference(ObjectId::Tag(0))];
        c.index_roots = vec![ObjectId::Tree(4), ObjectId::Blob(1), ObjectId::Blob(5)];
        c.reflog_entries = vec![
            reflog(None, Some(ObjectId::Commit(1))),
            reflog(Some(ObjectId::Commit(3)), Some(ObjectId::Commit(2))),
        ];

        let r = Reachability::from_parts(&c, &entries, &[0, 1, 2, 3, 4]);

        assert_eq!(r.commit, vec![Ref, Ref, Ref, ReflogOnly, Unreachable]);
        assert_eq!(
            r.tree,
            vec![Ref, Ref, Ref, Unreachable, IndexOnly, ReflogOnly]
        );
        assert_eq!(
            r.blob,
            vec![
                Ref,
                Ref,
                Ref,
                Unreachable,
                IndexOnly,
                IndexOnly,
                Unreachable
            ]
        );
        assert_eq!(r.tag, vec![Ref, Ref, Unreachable]);

        let totals = r.totals(&c);
        // Unreachable: commit 4 (41), tree 3 (31), blobs 3 and 6 (31, 61),
        // tag 2 (21)
        assert_eq!(
            totals[Unreachable as usize],
            Total {
                count: 5,
                size_disk: 41 + 31 + 31 + 61 + 21
            }
        );
        assert_eq!(
            totals[ReflogOnly as usize],
            Total {
                count: 2,
                size_disk: 31 + 51
            }
        );
        assert_eq!(
            totals[IndexOnly as usize],
            Total {
                count: 3,
                size_disk: 41 + 41 + 51
            }
        );
        assert_eq!(totals.iter().map(|t| t.count).sum::<usize>(), 5 + 6 + 7 + 3);
    }

    #[test]
    fn a_root_that_is_not_in_the_store_is_ignored_and_no_roots_give_unreachable() {
        let (mut c, entries) = graph(&[(&[], 0)], &[vec![E::B(0)]], 1, &[]);
        c.refs = vec![Ref::new(b"refs/heads/gone".to_vec(), [9; HASH_LEN])];
        c.reflog_entries = vec![reflog(None, None)];
        let r = Reachability::from_parts(&c, &entries, &[0]);
        assert_eq!(
            (r.commit[0], r.tree[0], r.blob[0]),
            (Unreachable, Unreachable, Unreachable)
        );
    }

    #[test]
    fn a_large_tree_cut_into_chunks_gives_all_its_children() {
        // One root tree with more entries than one chunk
        let blobs = 3 * super::ENTRY_CHUNK + 1;
        let (mut c, entries) = graph(&[(&[], 0)], &[(0..blobs).map(E::B).collect()], blobs, &[]);
        c.refs = vec![reference(ObjectId::Commit(0))];
        let r = Reachability::from_parts(&c, &entries, &[0]);
        assert!(r.blob.iter().all(|&l| l == Ref));
    }
}
