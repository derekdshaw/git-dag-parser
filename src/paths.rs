//! The path where the owner commit added each tree and blob
//!
//! The path of an object comes from its introducing parent: a tree with
//! the same owner that contains it. The proof that this tree exists: if
//! each tree that contains `o` in its owner commit `c` had an owner of a
//! lower rank, that commit would also reach `o`, and would own it.
//!
//! The path table is a prefix tree on the `NameStore`. Row 0 is the empty
//! path of the root trees. The ids follow the order of the full path bytes,
//! so they do not change between runs.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering::Relaxed};

use git_dag_parser_core::models::container::{FileKind, TreeEntries};
use git_dag_parser_core::models::{Commit, NameId, NameStore};
use git_dag_parser_core::parser::ParseResult;
use rayon::prelude::*;

use crate::attribution::{Attribution, NOT_INTRODUCED};

/// `tree_path` and `blob_path` value of an object that no commit reaches
pub const NO_PATH: u32 = u32::MAX;

const TREE: u8 = FileKind::Tree as u8;

/// The introducing parent value of an object that has none
const NO_PARENT: u64 = u64::MAX;

/// The bit of an object in a sort key that is a blob, not a tree
const BLOB: u32 = 1 << 31;

/// One path: its parent path plus one name
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Path {
    /// The parent path; None only for the empty path (row 0)
    pub parent: Option<u32>,
    /// The last name; None only for the empty path
    pub name: Option<NameId>,
    /// Number of names: 0 for the empty path, 1 for `src`
    pub depth: u32,
}

/// The path table and the path id of each tree and blob, in container
/// order
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    /// By path id, in the order of the full path bytes
    pub paths: Vec<Path>,
    /// Path id of each tree, or `NO_PATH`
    pub tree_path: Vec<u32>,
    /// Path id of each blob, or `NO_PATH`
    pub blob_path: Vec<u32>,
}

impl Paths {
    /// The path of each tree and blob of `parsed`, from the owners of
    /// `attribution`
    pub fn calculate(parsed: &ParseResult, attribution: &Attribution) -> Self {
        Self::from_parts(
            &parsed.container.commits,
            &parsed.tree_entries,
            &parsed.names,
            &attribution.tree_introduced_by,
            &attribution.blob_introduced_by,
        )
    }

    /// `calculate` from the commits, the tree entries, the names, and the
    /// owner commit index of each tree and blob (`NOT_INTRODUCED` for none)
    pub(crate) fn from_parts(
        commits: &[Commit],
        entries: &TreeEntries,
        names: &NameStore,
        tree_owner: &[u32],
        blob_owner: &[u32],
    ) -> Self {
        let trees = entries.len();
        let blobs = blob_owner.len();
        assert!(
            trees < BLOB as usize && blobs < BLOB as usize,
            "tree and blob indices fit in 31 bits"
        );
        let (tree_parent, blob_parent) = introducing_parents(entries, tree_owner, blob_owner);
        let is_root = |t: usize| {
            let owner = tree_owner[t];
            owner != NOT_INTRODUCED && commits[owner as usize].tree == Some(t)
        };
        let tree_depth = tree_depths(&tree_parent, is_root);

        let blob_depth: Vec<u32> = blob_parent
            .par_iter()
            .map(|&v| match v {
                NO_PARENT => NO_PATH,
                v => match tree_depth[(v >> 32) as usize] {
                    NO_PATH => NO_PATH,
                    d => d + 1,
                },
            })
            .collect();
        let max_depth = tree_depth
            .par_iter()
            .chain(&blob_depth)
            .filter(|&&d| d != NO_PATH)
            .max()
            .copied()
            .unwrap_or(0);

        // Temporary path ids, in the order the paths are made. Row 0 is the
        // empty path of the root trees.
        let mut table = vec![Path {
            parent: None,
            name: None,
            depth: 0,
        }];
        let tree_path: Vec<AtomicU32> = (0..trees)
            .into_par_iter()
            .map(|t| AtomicU32::new(if tree_depth[t] == 0 { 0 } else { NO_PATH }))
            .collect();
        let blob_path: Vec<AtomicU32> = (0..blobs)
            .into_par_iter()
            .map(|_| AtomicU32::new(NO_PATH))
            .collect();

        // The paths of depth `d` are made after the paths of depth `d - 1`,
        // which are their parents. All objects with the same parent path
        // have the same depth, so a sort of one depth finds all objects
        // with the same (parent path, name). `Relaxed` is enough: each
        // parallel step returns only after all its tasks are done.
        for depth in 1..=max_depth {
            let trees_at = (0..trees)
                .into_par_iter()
                .filter(|&t| tree_depth[t] == depth)
                .map(|t| (tree_parent[t], t as u32));
            let blobs_at = (0..blobs)
                .into_par_iter()
                .filter(|&b| blob_depth[b] == depth)
                .map(|b| (blob_parent[b], b as u32 | BLOB));
            let mut keyed: Vec<(u32, NameId, u32)> = trees_at
                .chain(blobs_at)
                .map(|(parent, object)| {
                    let (tree, entry) = ((parent >> 32) as usize, parent as u32 as usize);
                    let name = entries.names(tree)[entry];
                    (tree_path[tree].load(Relaxed), name, object)
                })
                .collect();
            keyed.par_sort_unstable();
            let key = |i: usize| (keyed[i].0, keyed[i].1);
            let starts: Vec<usize> = (0..keyed.len())
                .into_par_iter()
                .filter(|&i| i == 0 || key(i) != key(i - 1))
                .collect();
            let base = table.len();
            table.par_extend(starts.par_iter().map(|&i| Path {
                parent: Some(keyed[i].0),
                name: Some(keyed[i].1),
                depth,
            }));
            starts.par_iter().enumerate().for_each(|(run, &start)| {
                let end = starts.get(run + 1).copied().unwrap_or(keyed.len());
                let id = (base + run) as u32;
                for &(_, _, object) in &keyed[start..end] {
                    if object & BLOB == 0 {
                        tree_path[object as usize].store(id, Relaxed);
                    } else {
                        blob_path[(object & !BLOB) as usize].store(id, Relaxed);
                    }
                }
            });
        }

        let (paths, new_id) = sort_table(&table, names);
        let renumber = |ids: Vec<AtomicU32>| -> Vec<u32> {
            ids.into_par_iter()
                .map(|p| match p.into_inner() {
                    NO_PATH => NO_PATH,
                    p => new_id[p as usize],
                })
                .collect()
        };
        Self {
            paths,
            tree_path: renumber(tree_path),
            blob_path: renumber(blob_path),
        }
    }

    /// The path id of tree `i`, or None if no commit reaches it
    pub fn tree_path(&self, i: usize) -> Option<u32> {
        let p = self.tree_path[i];
        (p != NO_PATH).then_some(p)
    }

    /// The path id of blob `i`, or None if no commit reaches it
    pub fn blob_path(&self, i: usize) -> Option<u32> {
        let p = self.blob_path[i];
        (p != NO_PATH).then_some(p)
    }

    /// The full bytes of path `id`, such as `src/parser/main.rs`
    pub fn bytes(&self, id: u32, names: &NameStore) -> Vec<u8> {
        let mut parts = Vec::with_capacity(self.paths[id as usize].depth as usize);
        let mut path = self.paths[id as usize];
        while let (Some(parent), Some(name)) = (path.parent, path.name) {
            parts.push(names.get(name));
            path = self.paths[parent as usize];
        }
        parts.reverse();
        parts.join(&b'/')
    }
}

/// The introducing parent of each tree and of each blob, as (tree index
/// << 32 | entry index), or `NO_PARENT`. Of many introducing parents, the
/// lowest value wins: tree indices are in hash order, so the result does
/// not change between runs.
fn introducing_parents(
    entries: &TreeEntries,
    tree_owner: &[u32],
    blob_owner: &[u32],
) -> (Vec<u64>, Vec<u64>) {
    let new = |n: usize| -> Vec<AtomicU64> {
        (0..n)
            .into_par_iter()
            .map(|_| AtomicU64::new(NO_PARENT))
            .collect()
    };
    let tree_parent = new(entries.len());
    let blob_parent = new(blob_owner.len());
    // `Relaxed` is enough: the values are read after the loop returns
    (0..entries.len()).into_par_iter().for_each(|t| {
        let owner = tree_owner[t];
        if owner == NOT_INTRODUCED {
            return;
        }
        let (child, kind) = entries.children(t);
        for (e, (&c, &k)) in child.iter().zip(kind).enumerate() {
            if c == TreeEntries::NO_CHILD {
                continue;
            }
            let (owners, parents) = if k == TREE {
                (tree_owner, &tree_parent)
            } else {
                (blob_owner, &blob_parent)
            };
            if owners[c as usize] == owner {
                let value = (t as u64) << 32 | e as u64;
                let parent = &parents[c as usize];
                if value < parent.load(Relaxed) {
                    parent.fetch_min(value, Relaxed);
                }
            }
        }
    });
    let values =
        |v: Vec<AtomicU64>| -> Vec<u64> { v.into_par_iter().map(AtomicU64::into_inner).collect() };
    (values(tree_parent), values(blob_parent))
}

/// The depth of the path of each tree: 0 for the root tree of its owner,
/// one more than its introducing parent for the others, and `NO_PATH` for a
/// tree with no owner. Each thread goes up the parents to a tree with a
/// known depth, then sets the depth of each tree on the way. Two threads
/// can set the same tree; they set the same value.
fn tree_depths(tree_parent: &[u64], is_root: impl Fn(usize) -> bool + Sync) -> Vec<u32> {
    const UNKNOWN: u32 = NO_PATH - 1;
    let depth: Vec<AtomicU32> = (0..tree_parent.len())
        .into_par_iter()
        .map(|t| {
            AtomicU32::new(if is_root(t) {
                0
            } else if tree_parent[t] == NO_PARENT {
                NO_PATH
            } else {
                UNKNOWN
            })
        })
        .collect();
    (0..tree_parent.len())
        .into_par_iter()
        .for_each_init(Vec::new, |chain, t| {
            chain.clear();
            let mut at = t;
            let mut d = loop {
                match depth[at].load(Relaxed) {
                    UNKNOWN => {
                        chain.push(at);
                        at = (tree_parent[at] >> 32) as usize;
                    }
                    d => break d,
                }
            };
            if d == NO_PATH {
                return;
            }
            for &t in chain.iter().rev() {
                d += 1;
                depth[t].store(d, Relaxed);
            }
        });
    // A tree is UNKNOWN here only if its parents end at a tree with no
    // owner, which the proof in the module comment does not allow
    depth
        .into_par_iter()
        .map(|d| match d.into_inner() {
            UNKNOWN => NO_PATH,
            d => d,
        })
        .collect()
}

/// The table in the order of the full path bytes, and the new id of each
/// row of `table`. A parent is before its children in `table`.
fn sort_table(table: &[Path], names: &NameStore) -> (Vec<Path>, Vec<u32>) {
    // The full bytes of all paths, one after the other; only for the sort
    let mut bytes = Vec::new();
    let mut start = Vec::with_capacity(table.len() + 1);
    start.push(0);
    for path in table {
        if let (Some(parent), Some(name)) = (path.parent, path.name) {
            let parent = parent as usize;
            bytes.extend_from_within(start[parent]..start[parent + 1]);
            if parent != 0 {
                bytes.push(b'/');
            }
            bytes.extend_from_slice(names.get(name));
        }
        start.push(bytes.len());
    }
    let full = |i: u32| &bytes[start[i as usize]..start[i as usize + 1]];
    let mut order: Vec<u32> = (0..table.len() as u32).collect();
    order.par_sort_unstable_by(|&a, &b| full(a).cmp(full(b)));

    let mut new_id = vec![0u32; table.len()];
    for (id, &old) in order.iter().enumerate() {
        new_id[old as usize] = id as u32;
    }
    let paths = order
        .par_iter()
        .map(|&old| {
            let path = table[old as usize];
            Path {
                parent: path.parent.map(|p| new_id[p as usize]),
                ..path
            }
        })
        .collect();
    (paths, new_id)
}

#[cfg(test)]
mod tests {
    use git_dag_parser_core::models::container::{FileKind, TreeEntriesBuilder, HASH_LEN};
    use git_dag_parser_core::models::NameMap;

    use super::*;
    use crate::attribution::NOT_INTRODUCED;

    /// A tree entry with a name: a child tree or a blob
    #[derive(Clone, Copy)]
    enum E {
        T(usize, &'static str),
        B(usize, &'static str),
    }

    fn tree_entries(trees: &[Vec<E>]) -> (TreeEntries, NameStore) {
        let mut names = NameStore::new();
        let mut builder = TreeEntriesBuilder::new();
        for (t, entries) in trees.iter().enumerate() {
            builder.begin_tree(t);
            for &e in entries {
                match e {
                    E::T(i, n) => {
                        builder.push(Some(i), names.intern(n.as_bytes()).unwrap(), FileKind::Tree)
                    }
                    E::B(i, n) => builder.push(
                        Some(i),
                        names.intern(n.as_bytes()).unwrap(),
                        FileKind::Regular,
                    ),
                }
            }
        }
        let map = NameMap::identity(names.len());
        (
            TreeEntries::merge(trees.len(), vec![(builder, &map)]),
            names,
        )
    }

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

    fn path_of(paths: &Paths, names: &NameStore, id: Option<u32>) -> Option<String> {
        id.map(|id| String::from_utf8(paths.bytes(id, names)).unwrap())
    }

    #[test]
    fn a_blob_at_two_paths_in_one_commit_gets_the_lower_tree_and_entry_index() {
        // Root 0: `b/` (tree 1) and `z` (blob 0); tree 1: `y` (blob 0).
        // (tree 0, entry 1) is lower than (tree 1, entry 0).
        let (entries, names) =
            tree_entries(&[vec![E::T(1, "b"), E::B(0, "z")], vec![E::B(0, "y")]]);
        let p = Paths::from_parts(&commits(&[Some(0)]), &entries, &names, &[0, 0], &[0]);
        assert_eq!(path_of(&p, &names, p.blob_path(0)).as_deref(), Some("z"));
        assert_eq!(path_of(&p, &names, p.tree_path(0)).as_deref(), Some(""));
        assert_eq!(path_of(&p, &names, p.tree_path(1)).as_deref(), Some("b"));

        // Root 1: `d/` (tree 0) and `x` (blob 0); tree 0: `y` (blob 0).
        // The lower tree index wins, also when it is deeper.
        let (entries, names) =
            tree_entries(&[vec![E::B(0, "y")], vec![E::T(0, "d"), E::B(0, "x")]]);
        let p = Paths::from_parts(&commits(&[Some(1)]), &entries, &names, &[0, 0], &[0]);
        assert_eq!(path_of(&p, &names, p.blob_path(0)).as_deref(), Some("d/y"));
    }

    #[test]
    fn an_object_gets_its_path_from_a_parent_with_the_same_owner() {
        // Commit 0 (root 0) has `lib/` = tree 2 with `a` = blob 0. Commit 1
        // (root 1) moves tree 2 to `src/`, and adds `src2/` = tree 3 with
        // blob 0 and the new blob 1 at `b`.
        let (entries, names) = tree_entries(&[
            vec![E::T(2, "lib")],
            vec![E::T(2, "src"), E::T(3, "src2")],
            vec![E::B(0, "a")],
            vec![E::B(0, "a"), E::B(1, "b")],
        ]);
        let p = Paths::from_parts(
            &commits(&[Some(0), Some(1)]),
            &entries,
            &names,
            &[0, 1, 0, 1],
            &[0, 1],
        );
        let tree = |i| path_of(&p, &names, p.tree_path(i));
        assert_eq!(tree(1).as_deref(), Some(""));
        assert_eq!(tree(2).as_deref(), Some("lib"), "commit 0 added tree 2");
        assert_eq!(tree(3).as_deref(), Some("src2"));
        assert_eq!(
            path_of(&p, &names, p.blob_path(0)).as_deref(),
            Some("lib/a")
        );
        assert_eq!(
            path_of(&p, &names, p.blob_path(1)).as_deref(),
            Some("src2/b")
        );
    }

    #[test]
    fn an_object_with_no_owner_has_no_path() {
        let (entries, names) = tree_entries(&[vec![E::B(0, "a")], vec![E::B(1, "b")]]);
        let p = Paths::from_parts(
            &commits(&[Some(0)]),
            &entries,
            &names,
            &[0, NOT_INTRODUCED],
            &[0, NOT_INTRODUCED],
        );
        assert_eq!((p.tree_path(1), p.blob_path(1)), (None, None));
        assert!(p.tree_path(0).is_some() && p.blob_path(0).is_some());
    }

    #[test]
    fn path_ids_follow_the_order_of_the_full_path_bytes() {
        // `src-x` sorts before `src/z`: `-` is 0x2D and `/` is 0x2F
        let (entries, names) = tree_entries(&[
            vec![E::T(1, "src"), E::B(0, "src-x"), E::B(1, "a.txt")],
            vec![E::B(2, "z"), E::T(2, "deep")],
            vec![E::B(3, "f")],
        ]);
        let p = Paths::from_parts(
            &commits(&[Some(0)]),
            &entries,
            &names,
            &[0, 0, 0],
            &[0, 0, 0, 0],
        );
        let all: Vec<String> = (0..p.paths.len() as u32)
            .map(|id| String::from_utf8(p.bytes(id, &names)).unwrap())
            .collect();
        assert_eq!(
            all,
            vec![
                "",
                "a.txt",
                "src",
                "src-x",
                "src/deep",
                "src/deep/f",
                "src/z"
            ]
        );
        assert_eq!(
            p.paths[0],
            Path {
                parent: None,
                name: None,
                depth: 0
            }
        );
        let by_bytes = |s: &str| all.iter().position(|a| a == s).unwrap() as u32;
        for (id, path) in p.paths.iter().enumerate().skip(1) {
            let parent = path.parent.unwrap();
            let bytes = &all[id];
            let want = bytes.rsplit_once('/').map_or("", |(dir, _)| dir);
            assert_eq!(parent, by_bytes(want), "parent of {bytes}");
            assert_eq!(path.depth, p.paths[parent as usize].depth + 1);
        }
        assert_eq!(p.tree_path(2), Some(by_bytes("src/deep")));
        assert_eq!(p.blob_path(3), Some(by_bytes("src/deep/f")));
    }

    #[test]
    fn no_trees_and_no_blobs_give_only_the_empty_path_or_nothing() {
        let (entries, names) = tree_entries(&[]);
        let p = Paths::from_parts(&[], &entries, &names, &[], &[]);
        assert!(p.tree_path.is_empty() && p.blob_path.is_empty());
        assert!(p.paths.len() <= 1);
    }
}
