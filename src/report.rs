//! The analysis that `init --report` prints
//!
//! The passes use only the parsed objects and the attribution, paths and
//! reachability results in memory. A database build does not run them.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use git_dag_parser_core::models::{ObjectContainer, ObjectId, RefKind};
use git_dag_parser_core::parser::ParseResult;
use git_dag_parser_core::size::format_size;
use rayon::prelude::*;

use crate::attribution::Attribution;
use crate::paths::{Path, Paths};
use crate::reachability::{Liveness, Reachability};

/// The largest objects and the commits that added them, as lines for the
/// summary
pub fn analyze(
    parsed: &ParseResult,
    attribution: &Attribution,
    paths: &Paths,
    reachability: &Reachability,
) -> Vec<String> {
    let c = &parsed.container;
    let path = |p: Option<u32>| match p {
        Some(0) => "(root)".to_string(),
        Some(p) => String::from_utf8_lossy(&paths.bytes(p, &parsed.names)).into_owned(),
        None => "(no path)".to_string(),
    };
    let commit = |i: usize| {
        let commit = &c.commits[i];
        format!("{} {}", short(&commit.hash), date(commit.commit_time))
    };
    let added_by =
        |owner: Option<u32>| owner.map_or("(no commit)".to_string(), |o| commit(o as usize));
    let blob_line = |size: u64, i: usize| {
        format!(
            "  {:>10} {:>10} on disk  {:<11}  {}  {}",
            format_size(size),
            format_size(c.blobs[i].size_disk),
            format!("{:?}", reachability.blob[i]),
            added_by(attribution.blob_introduced_by(i)),
            path(paths.blob_path(i))
        )
    };
    let mut out = Vec::new();

    out.push("Largest blobs (size, disk, state, added by, path):".to_string());
    for (size, i) in largest(5, c.blobs.par_iter().map(|b| (b.size, b.index))) {
        out.push(blob_line(size, i));
    }

    out.push("Largest blobs that no ref keeps:".to_string());
    let not_ref = c
        .blobs
        .par_iter()
        .filter(|b| reachability.blob[b.index] != Liveness::Ref);
    let not_ref = largest(3, not_ref.map(|b| (b.size, b.index)));
    if not_ref.is_empty() {
        out.push("  (none: refs keep all blobs)".to_string());
    }
    for (size, i) in not_ref {
        out.push(blob_line(size, i));
    }

    out.push("Largest trees (size, entries, state, added by, path):".to_string());
    for (size, i) in largest(3, c.trees.par_iter().map(|t| (t.size, t.index))) {
        out.push(format!(
            "  {:>10} {:>8} entries  {:<11}  {}  {}",
            format_size(size),
            parsed.tree_entries.children(i).0.len(),
            format!("{:?}", reachability.tree[i]),
            added_by(attribution.tree_introduced_by(i)),
            path(paths.tree_path(i))
        ));
    }

    // The blobs that each commit adds and each path holds
    let by_commit = sums(c.commits.len());
    let by_path = sums(paths.paths.len());
    c.blobs.par_iter().for_each(|b| {
        if let Some(o) = attribution.blob_introduced_by(b.index) {
            by_commit[o as usize].add(1, b.size, b.size_disk);
        }
        if let Some(p) = paths.blob_path(b.index) {
            by_path[p as usize].add(1, b.size, b.size_disk);
        }
    });

    out.push("Commits that add the most blob data:".to_string());
    let bytes = by_commit.par_iter().map(|s| s.get().1);
    for (size, i) in largest(3, bytes.enumerate().map(|(i, b)| (b, i))) {
        out.push(format!(
            "  {:>10} in {:>7} blobs  {}",
            format_size(size),
            by_commit[i].get().0,
            commit(i)
        ));
    }

    out.push("Paths with the most blob versions:".to_string());
    let counts = by_path.par_iter().map(|s| s.get().0);
    for (count, i) in largest(3, counts.enumerate().map(|(i, n)| (n, i))) {
        let (_, size, disk) = by_path[i].get();
        out.push(format!(
            "  {:>7} versions {:>10} {:>10} on disk  {}",
            count,
            format_size(size),
            format_size(disk),
            path(Some(i as u32))
        ));
    }

    let types = [
        (
            "commits",
            totals(c.commits.par_iter().map(|o| (o.size, o.size_disk))),
        ),
        (
            "trees",
            totals(c.trees.par_iter().map(|o| (o.size, o.size_disk))),
        ),
        (
            "blobs",
            totals(c.blobs.par_iter().map(|o| (o.size, o.size_disk))),
        ),
        (
            "tags",
            totals(c.tags.par_iter().map(|o| (o.size, o.size_disk))),
        ),
    ];
    let all_disk: u64 = types.iter().map(|(_, (_, _, disk))| disk).sum();
    let share = |disk: u64| 100.0 * disk as f64 / all_disk.max(1) as f64;
    out.push("Objects of each type (count, size, disk):".to_string());
    for (name, (count, size, disk)) in types {
        out.push(format!(
            "  {:>10} {:<7} {:>10} {:>10} on disk ({:>4.1}%)",
            count,
            name,
            format_size(size),
            format_size(disk),
            share(disk)
        ));
    }

    let dirs = directories(parsed, paths);
    let dir_line = |d: &Dir, i: usize| {
        format!(
            "  {:>10} on disk ({:>4.1}%)  {:>7} trees {:>10} ({} entries)  {:>7} blobs {:>10}  {}",
            format_size(d.disk()),
            share(d.disk()),
            d.trees,
            format_size(d.tree_disk),
            d.entries,
            d.blobs,
            format_size(d.blob_disk),
            path(Some(i as u32))
        )
    };
    out.push(
        "Directories with the most data on disk (its trees, the blobs directly in it):".to_string(),
    );
    let disk = (0..dirs.direct.len()).into_par_iter();
    for (_, i) in largest(5, disk.map(|i| (dirs.direct[i].disk(), i))) {
        out.push(dir_line(&dirs.direct[i], i));
    }
    out.push("Directories with the most data on disk, with all subdirectories:".to_string());
    // Skip the root: it holds all the trees and blobs that have a path
    let disk = (1..dirs.subtree.len()).into_par_iter();
    for (_, i) in largest(5, disk.map(|i| (dirs.subtree[i].disk(), i))) {
        out.push(dir_line(&dirs.subtree[i], i));
    }

    out.push("Refs of each kind:".to_string());
    for kind in [
        RefKind::Branch,
        RefKind::Remote,
        RefKind::Tag,
        RefKind::Stash,
        RefKind::Head,
        RefKind::Other,
    ] {
        let count = c.refs.iter().filter(|r| r.kind == kind).count();
        out.push(format!("  {count:>10} {kind:?}"));
    }

    let o = overhead(c);
    out.push(
        "Copies on disk that are not the primary copy (pack overlap, loose copies):".to_string(),
    );
    out.push(format!(
        "  {:>10} copies {:>10} on disk ({:>4.1}%), of these {} loose copies of packed objects {}",
        o.extra_copies,
        format_size(o.extra_disk),
        share(o.extra_disk),
        o.loose_packed,
        format_size(o.loose_packed_disk)
    ));

    out.push("Largest objects stored without a delta (disk, size, object, path):".to_string());
    let whole = c
        .copies
        .par_iter()
        .enumerate()
        .filter(|(_, copy)| copy.is_primary && !copy.is_delta)
        .map(|(i, copy)| (copy.size_disk, i));
    for (disk, i) in largest(5, whole) {
        let id = c.copies[i].object();
        let (size, hash) = object(c, id);
        let at = match id {
            ObjectId::Tree(t) => path(paths.tree_path(t)),
            ObjectId::Blob(b) => path(paths.blob_path(b)),
            ObjectId::Commit(_) | ObjectId::Tag(_) => String::new(),
        };
        out.push(format!(
            "  {:>10} {:>10}  {:<6} {}  {}",
            format_size(disk),
            format_size(size),
            format!("{:?}", id.kind()).to_lowercase(),
            short(hash),
            at
        ));
    }
    out
}

/// (count, size, disk) of objects given as (size, disk)
fn totals(objects: impl ParallelIterator<Item = (u64, u64)>) -> (u64, u64, u64) {
    objects
        .map(|(size, disk)| (1, size, disk))
        .reduce(|| (0, 0, 0), |a, b| (a.0 + b.0, a.1 + b.1, a.2 + b.2))
}

/// (size, hash) of an object
fn object(c: &ObjectContainer, id: ObjectId) -> (u64, &[u8]) {
    match id {
        ObjectId::Commit(i) => (c.commits[i].size, &c.commits[i].hash),
        ObjectId::Tree(i) => (c.trees[i].size, &c.trees[i].hash),
        ObjectId::Blob(i) => (c.blobs[i].size, &c.blobs[i].hash),
        ObjectId::Tag(i) => (c.tags[i].size, &c.tags[i].hash),
    }
}

/// The trees and blobs of one directory path
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Dir {
    /// Versions of the directory tree
    pub trees: u64,
    /// Entries of all these trees
    pub entries: u64,
    pub tree_disk: u64,
    pub blobs: u64,
    pub blob_bytes: u64,
    pub blob_disk: u64,
}

impl Dir {
    pub fn disk(&self) -> u64 {
        self.tree_disk + self.blob_disk
    }

    fn add(&mut self, other: &Dir) {
        self.trees += other.trees;
        self.entries += other.entries;
        self.tree_disk += other.tree_disk;
        self.blobs += other.blobs;
        self.blob_bytes += other.blob_bytes;
        self.blob_disk += other.blob_disk;
    }
}

/// The totals of each path id
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Directories {
    /// The trees at the path and the blobs directly in it
    pub direct: Vec<Dir>,
    /// `direct` plus all subdirectories
    pub subtree: Vec<Dir>,
}

/// The trees and blobs of each directory, by the path where the owner
/// commit added them
///
/// Each version of a directory is a new tree, so a directory with many
/// entries and many changes costs disk space even if each blob in it is
/// small and has one version. The tree totals show this.
pub fn directories(parsed: &ParseResult, paths: &Paths) -> Directories {
    let c = &parsed.container;
    let dir_trees = sums(paths.paths.len());
    let dir_blobs = sums(paths.paths.len());
    c.blobs.par_iter().for_each(|b| {
        let p = paths.blob_path(b.index);
        if let Some(parent) = p.and_then(|p| paths.paths[p as usize].parent) {
            dir_blobs[parent as usize].add(1, b.size, b.size_disk);
        }
    });
    // Each commit has a root tree, so on linux 1.3 M trees add to the same
    // counters, and the atomic adds of all threads to them took most of the
    // time. Each chunk adds each of its paths one time.
    c.trees.par_chunks(1 << 14).for_each(|chunk| {
        let mut local: Vec<(u32, u64, u64)> = chunk
            .iter()
            .filter_map(|t| {
                let entries = parsed.tree_entries.children(t.index).0.len() as u64;
                Some((paths.tree_path(t.index)?, entries, t.size_disk))
            })
            .collect();
        local.sort_unstable_by_key(|&(p, _, _)| p);
        for run in local.chunk_by(|a, b| a.0 == b.0) {
            let entries = run.iter().map(|r| r.1).sum();
            let disk = run.iter().map(|r| r.2).sum();
            dir_trees[run[0].0 as usize].add(run.len() as u64, entries, disk);
        }
    });
    let direct: Vec<Dir> = dir_trees
        .par_iter()
        .zip(&dir_blobs)
        .map(|(t, b)| {
            let (trees, entries, tree_disk) = t.get();
            let (blobs, blob_bytes, blob_disk) = b.get();
            Dir {
                trees,
                entries,
                tree_disk,
                blobs,
                blob_bytes,
                blob_disk,
            }
        })
        .collect();
    let subtree = subtree(&paths.paths, &direct);
    Directories { direct, subtree }
}

/// Add each path to its parent, from the last path id to the first
///
/// The ids follow the order of the full path bytes, and a parent path is a
/// prefix of its child, so a parent has a lower id and gets all of its
/// children before it is added to its own parent.
fn subtree(paths: &[Path], direct: &[Dir]) -> Vec<Dir> {
    let mut total = direct.to_vec();
    for i in (0..total.len()).rev() {
        if let Some(parent) = paths[i].parent {
            let child = total[i];
            total[parent as usize].add(&child);
        }
    }
    total
}

/// Copies on disk that are not the primary copy of their object
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Overhead {
    pub extra_copies: u64,
    pub extra_disk: u64,
    /// Loose copies of objects that a pack also contains
    pub loose_packed: u64,
    pub loose_packed_disk: u64,
}

pub fn overhead(container: &ObjectContainer) -> Overhead {
    container
        .copies
        .par_iter()
        .filter(|copy| !copy.is_primary)
        .map(|copy| {
            // A packed copy is always primary over a loose copy, so a loose
            // copy that is not primary has a packed copy
            let loose = u64::from(copy.is_loose());
            Overhead {
                extra_copies: 1,
                extra_disk: copy.size_disk,
                loose_packed: loose,
                loose_packed_disk: loose * copy.size_disk,
            }
        })
        .reduce(Overhead::default, |a, b| Overhead {
            extra_copies: a.extra_copies + b.extra_copies,
            extra_disk: a.extra_disk + b.extra_disk,
            loose_packed: a.loose_packed + b.loose_packed,
            loose_packed_disk: a.loose_packed_disk + b.loose_packed_disk,
        })
}

/// A count and two byte totals that many threads add to
#[derive(Default)]
struct Sum {
    count: AtomicU64,
    bytes: AtomicU64,
    disk: AtomicU64,
}

impl Sum {
    fn add(&self, count: u64, bytes: u64, disk: u64) {
        self.count.fetch_add(count, Relaxed);
        self.bytes.fetch_add(bytes, Relaxed);
        self.disk.fetch_add(disk, Relaxed);
    }

    /// (count, bytes, disk)
    fn get(&self) -> (u64, u64, u64) {
        (
            self.count.load(Relaxed),
            self.bytes.load(Relaxed),
            self.disk.load(Relaxed),
        )
    }
}

fn sums(n: usize) -> Vec<Sum> {
    (0..n).into_par_iter().map(|_| Sum::default()).collect()
}

type Top = BinaryHeap<Reverse<(u64, Reverse<usize>)>>;

/// The `k` items with the largest key, largest first. Of equal keys, the
/// lowest index comes first.
fn largest(k: usize, items: impl ParallelIterator<Item = (u64, usize)>) -> Vec<(u64, usize)> {
    let offer = |heap: &mut Top, item: (u64, Reverse<usize>)| {
        if heap.len() < k {
            heap.push(Reverse(item));
        } else if heap.peek().is_some_and(|Reverse(min)| item > *min) {
            heap.pop();
            heap.push(Reverse(item));
        }
    };
    items
        .fold(Top::new, |mut heap, (key, i)| {
            offer(&mut heap, (key, Reverse(i)));
            heap
        })
        .reduce(Top::new, |mut a, b| {
            for Reverse(item) in b {
                offer(&mut a, item);
            }
            a
        })
        .into_sorted_vec()
        .into_iter()
        .map(|Reverse((key, Reverse(i)))| (key, i))
        .collect()
}

/// The first 10 hex digits of a hash
fn short(hash: &[u8]) -> String {
    hash[..5].iter().map(|b| format!("{b:02x}")).collect()
}

/// The UTC date of a Unix time, as YYYY-MM-DD (the days-to-civil algorithm
/// of Howard Hinnant)
fn date(time: i64) -> String {
    let z = time.div_euclid(86_400) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

#[cfg(test)]
mod tests {
    use git_dag_parser_core::models::NameStore;

    use super::*;

    #[test]
    fn largest_gives_the_k_largest_keys_and_the_lowest_index_on_a_tie() {
        let items = [(5, 0), (9, 1), (5, 2), (1, 3), (9, 4), (7, 5)];
        assert_eq!(
            largest(4, items.into_par_iter()),
            [(9, 1), (9, 4), (7, 5), (5, 0)]
        );
        assert_eq!(largest(10, items.into_par_iter()).len(), 6);
        assert!(largest(0, items.into_par_iter()).is_empty());
    }

    #[test]
    fn a_subtree_total_adds_only_the_paths_below_the_directory() {
        let mut names = NameStore::new();
        let mut path = |parent: u32, name: &[u8], depth: u32| Path {
            parent: Some(parent),
            name: Some(names.intern(name).unwrap()),
            depth,
        };
        // `src-x` sorts between `src` and `src/z`, but is not below `src`
        let table = [
            Path {
                parent: None,
                name: None,
                depth: 0,
            },
            path(0, b"src", 1),
            path(0, b"src-x", 1),
            path(1, b"z", 2),
        ];
        let dir = |n: u64| Dir {
            trees: n,
            entries: 10 * n,
            tree_disk: 100 * n,
            blobs: n,
            blob_bytes: 1000 * n,
            blob_disk: 10_000 * n,
        };
        let direct = [dir(1), dir(2), dir(4), dir(8)];

        let total = subtree(&table, &direct);

        assert_eq!(total, [dir(15), dir(10), dir(4), dir(8)]);
        assert_eq!(total[1].disk(), 10 * 10_100);
    }

    #[test]
    fn date_gives_the_utc_day() {
        assert_eq!(date(0), "1970-01-01");
        assert_eq!(date(951_782_400), "2000-02-29");
        assert_eq!(date(1_762_905_599), "2025-11-11");
        assert_eq!(date(-1), "1969-12-31");
    }
}
