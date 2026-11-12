use hashbrown::HashMap;
use rayon::prelude::*;

use super::name_store::{NameId, NameMap};
use super::objects::{Blob, Commit, ObjectCopy, ObjectId, Pack, Ref, Tag, Tree};

/// Length in bytes of a SHA-1 object hash
pub const HASH_LEN: usize = 20;

/// Length of an object hash in hex, as in commits, tags, refs and loose
/// object file names
pub const HEX_HASH_LEN: usize = HASH_LEN * 2;

/// Hash type alias for Git object hashes (`HASH_LEN` bytes for SHA-1)
pub type ObjectHash = [u8; HASH_LEN];

/// Container for all parsed Git objects
#[derive(Debug, Default)]
pub struct ObjectContainer {
    /// All commit objects
    pub commits: Vec<Commit>,
    /// All tree objects
    pub trees: Vec<Tree>,
    /// All blob objects
    pub blobs: Vec<Blob>,
    /// All tag objects
    pub tags: Vec<Tag>,
    /// All refs that point directly at an object
    pub refs: Vec<Ref>,
    /// All pack files in the object store
    pub packs: Vec<Pack>,
    /// Every on-disk copy of every object, across packs and loose files
    pub copies: Vec<ObjectCopy>,
    /// Modification time of each loose copy, by object, sorted by object.
    /// An object has at most one loose copy.
    pub loose_mtimes: Vec<(ObjectId, i64)>,
}

impl ObjectContainer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Pre-allocate capacity based on expected object counts
    #[allow(dead_code)]
    pub fn with_capacity(commits: usize, trees: usize, blobs: usize, tags: usize) -> Self {
        Self {
            commits: Vec::with_capacity(commits),
            trees: Vec::with_capacity(trees),
            blobs: Vec::with_capacity(blobs),
            tags: Vec::with_capacity(tags),
            refs: Vec::new(),
            packs: Vec::new(),
            copies: Vec::new(),
            loose_mtimes: Vec::new(),
        }
    }

    /// Modification time of the file holding a copy: its pack file, or its
    /// loose object file. Seconds since the Unix epoch.
    pub fn copy_mtime(&self, copy: &ObjectCopy) -> i64 {
        match copy.pack() {
            Some(pack) => self.packs[pack].mtime,
            None => {
                let i = self
                    .loose_mtimes
                    .binary_search_by_key(&copy.object(), |&(id, _)| id)
                    .expect("every loose copy has an mtime");
                self.loose_mtimes[i].1
            }
        }
    }

    /// Hash of the object an id refers to
    pub fn hash_of(&self, id: ObjectId) -> ObjectHash {
        match id {
            ObjectId::Commit(i) => self.commits[i].hash,
            ObjectId::Tree(i) => self.trees[i].hash,
            ObjectId::Blob(i) => self.blobs[i].hash,
            ObjectId::Tag(i) => self.tags[i].hash,
        }
    }

    /// Inflated size of the object an id refers to
    pub fn object_size(&self, id: ObjectId) -> u64 {
        match id {
            ObjectId::Commit(i) => self.commits[i].size,
            ObjectId::Tree(i) => self.trees[i].size,
            ObjectId::Blob(i) => self.blobs[i].size,
            ObjectId::Tag(i) => self.tags[i].size,
        }
    }
}

/// Index for looking up objects by hash. One map for all kinds, since pass
/// 1 must ask "have I seen this hash?" once per copy and building hash maps
/// is the most expensive part of that pass.
#[derive(Debug, Default)]
pub struct HashIndex {
    map: HashMap<ObjectHash, ObjectId>,
}

impl HashIndex {
    pub fn new() -> Self {
        Self::default()
    }

    /// Pre-allocate for an expected number of distinct objects
    pub fn with_capacity(objects: usize) -> Self {
        Self {
            map: HashMap::with_capacity(objects),
        }
    }

    /// Record an object, returning the previous id if the hash was known
    pub fn insert(&mut self, hash: ObjectHash, id: ObjectId) -> Option<ObjectId> {
        self.map.insert(hash, id)
    }

    /// Change the id of each object in place. The hashes do not change, so
    /// the map does not need to be built again.
    pub fn renumber(&mut self, f: impl Fn(ObjectId) -> ObjectId) {
        for id in self.map.values_mut() {
            *id = f(*id);
        }
    }

    /// Find an object of any type by hash
    pub fn get(&self, hash: &ObjectHash) -> Option<ObjectId> {
        self.map.get(hash).copied()
    }

    /// True if an object with this hash is indexed under any type
    pub fn contains(&self, hash: &ObjectHash) -> bool {
        self.map.contains_key(hash)
    }

    /// Number of distinct objects
    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Index of the commit with this hash; None if absent or not a commit
    pub fn commit(&self, hash: &ObjectHash) -> Option<usize> {
        match self.get(hash) {
            Some(ObjectId::Commit(i)) => Some(i),
            _ => None,
        }
    }

    /// Index of the tree with this hash; None if absent or not a tree
    pub fn tree(&self, hash: &ObjectHash) -> Option<usize> {
        match self.get(hash) {
            Some(ObjectId::Tree(i)) => Some(i),
            _ => None,
        }
    }

    /// Index of the blob with this hash; None if absent or not a blob
    pub fn blob(&self, hash: &ObjectHash) -> Option<usize> {
        match self.get(hash) {
            Some(ObjectId::Blob(i)) => Some(i),
            _ => None,
        }
    }

    /// Index of the tag with this hash; None if absent or not a tag
    pub fn tag(&self, hash: &ObjectHash) -> Option<usize> {
        match self.get(hash) {
            Some(ObjectId::Tag(i)) => Some(i),
            _ => None,
        }
    }
}

/// Git file mode constants
pub mod mode {
    pub const TREE: u32 = 0o040000;
    pub const REGULAR: u32 = 0o100644;
    pub const EXECUTABLE: u32 = 0o100755;
    pub const SYMLINK: u32 = 0o120000;
    pub const SUBMODULE: u32 = 0o160000;
}

/// The kind of object a tree entry references. One byte, since every tree
/// entry stores one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum FileKind {
    /// Directory (tree object)
    Tree = 0,
    /// Regular file
    Regular = 1,
    /// Executable file
    Executable = 2,
    /// Symbolic link
    Symlink = 3,
    /// Gitlink (submodule reference)
    Submodule = 4,
}

impl FileKind {
    /// Parse a mode value into a FileKind
    #[inline]
    pub fn from_mode(mode: u32) -> Option<Self> {
        match mode {
            mode::TREE => Some(Self::Tree),
            mode::REGULAR => Some(Self::Regular),
            mode::EXECUTABLE => Some(Self::Executable),
            mode::SYMLINK => Some(Self::Symlink),
            mode::SUBMODULE => Some(Self::Submodule),
            _ => None,
        }
    }

    /// The kind stored as `kind as u8`. Only `TreeEntries` stores kinds as
    /// bytes, so this stays in this module.
    #[inline]
    fn from_byte(byte: u8) -> Self {
        match byte {
            0 => Self::Tree,
            1 => Self::Regular,
            2 => Self::Executable,
            3 => Self::Symlink,
            4 => Self::Submodule,
            _ => unreachable!("byte came from `FileKind as u8`"),
        }
    }

    /// Returns true if this is a tree (directory)
    #[inline]
    pub fn is_tree(self) -> bool {
        matches!(self, Self::Tree)
    }

    /// Returns true if this is a blob (file, symlink, but not submodule)
    #[inline]
    pub fn is_blob(self) -> bool {
        matches!(self, Self::Regular | Self::Executable | Self::Symlink)
    }

    /// Returns the raw mode value
    #[inline]
    pub fn mode(self) -> u32 {
        match self {
            Self::Tree => mode::TREE,
            Self::Regular => mode::REGULAR,
            Self::Executable => mode::EXECUTABLE,
            Self::Symlink => mode::SYMLINK,
            Self::Submodule => mode::SUBMODULE,
        }
    }
}

/// The entries of every tree, as parallel arrays indexed by entry position.
/// Tree `t`'s entries are at positions `start[t]..start[t + 1]`, in body
/// order. Children are stored as resolved indices, not hashes, so an entry
/// is about 9 bytes: for a history like linux (~528 million entries) that
/// is about 4.7 GB.
#[derive(Debug, Default)]
pub struct TreeEntries {
    /// Position of each tree's first entry, by tree index, with a final
    /// entry-count sentinel: `start.len()` is the tree count plus one
    start: Vec<u64>,
    /// Index of the entry's child among trees or blobs (`kind` says which);
    /// `NO_CHILD` for a submodule or a child that is not in the store
    child: Vec<u32>,
    /// The entry's name
    name: Vec<NameId>,
    /// The entry's `FileKind`, as one byte
    mode: Vec<u8>,
}

impl TreeEntries {
    /// `child` value for an entry whose child has no index: a submodule, or
    /// an object missing from the store
    pub const NO_CHILD: u32 = u32::MAX;

    /// Number of trees
    pub fn len(&self) -> usize {
        self.start.len().saturating_sub(1)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Number of entries across all trees
    pub fn entry_count(&self) -> usize {
        self.child.len()
    }

    /// The entries of tree `tree` in body order, as (child, name, kind).
    /// The child is an index among trees if the kind is a tree, otherwise
    /// among blobs; None for a submodule or a child not in the store.
    #[inline]
    pub fn entries(
        &self,
        tree: usize,
    ) -> impl Iterator<Item = (Option<usize>, NameId, FileKind)> + '_ {
        let range = self.start[tree] as usize..self.start[tree + 1] as usize;
        range.map(move |e| {
            let child = self.child[e];
            (
                (child != Self::NO_CHILD).then_some(child as usize),
                self.name[e],
                FileKind::from_byte(self.mode[e]),
            )
        })
    }

    /// The children and kinds of the entries of tree `tree`, in body order,
    /// for work over many entries that needs no names. A kind is
    /// `FileKind as u8`. A child is `NO_CHILD`, or an index among trees if
    /// the kind is `FileKind::Tree as u8`, otherwise among blobs.
    #[inline]
    pub fn children(&self, tree: usize) -> (&[u32], &[u8]) {
        let range = self.start[tree] as usize..self.start[tree + 1] as usize;
        (&self.child[range.clone()], &self.mode[range])
    }
}

/// Records the entries of the trees one worker reads, in the order it
/// reads them, with names from the worker's own `NameStore`.
/// `TreeEntries::merge` lays the entries of all workers out in tree index
/// order. A tree that is never recorded has no entries.
#[derive(Debug, Default)]
pub struct TreeEntriesBuilder {
    /// Entries in the order they were recorded; the merge reorders them
    child: Vec<u32>,
    name: Vec<NameId>,
    mode: Vec<u8>,
    /// Each recorded tree with its entries in the arrays: (tree, first
    /// position, count). Only the trees this builder records, so that a
    /// builder for a few trees stays small.
    spans: Vec<(u32, u64, u32)>,
    /// The tree entries are currently recorded for, with its first position
    open: Option<(usize, u64)>,
}

impl TreeEntriesBuilder {
    /// An empty builder
    pub fn new() -> Self {
        Self::default()
    }

    /// Start recording the entries of `tree`: pushes until the next
    /// `begin_tree` or the merge belong to it.
    pub fn begin_tree(&mut self, tree: usize) {
        self.close();
        self.open = Some((tree, self.child.len() as u64));
    }

    /// Add the next entry of the tree begun last
    pub fn push(&mut self, child: Option<usize>, name: NameId, kind: FileKind) {
        debug_assert!(self.open.is_some(), "push requires begin_tree");
        self.child.push(match child {
            Some(i) => u32::try_from(i).expect("child index fits in u32"),
            None => TreeEntries::NO_CHILD,
        });
        self.name.push(name);
        self.mode.push(kind as u8);
    }

    /// Record the span of the tree being recorded, if any
    fn close(&mut self) {
        if let Some((tree, begin)) = self.open.take() {
            let count = self.child.len() as u64 - begin;
            self.spans.push((
                u32::try_from(tree).expect("tree index fits in u32"),
                begin,
                u32::try_from(count).expect("tree entry count fits in u32"),
            ));
        }
    }
}

impl TreeEntries {
    /// The entries of `trees` trees from the builders of all workers, in
    /// tree index order. Each builder comes with the map of its worker's
    /// name ids to the ids of the merged `NameStore`; the names are mapped
    /// in the same pass that reorders them. Each tree must be recorded by
    /// one builder at most.
    ///
    /// The arrays are gathered one at a time, and the builders' copy of
    /// each is freed right after, so the peak is the entries and one more
    /// array. Each array is gathered in parallel on the current rayon pool.
    pub fn merge(trees: usize, parts: Vec<(TreeEntriesBuilder, &NameMap)>) -> TreeEntries {
        let (mut builders, maps): (Vec<_>, Vec<_>) = parts.into_iter().unzip();
        // The builder of each tree and its span there; trees that are never
        // recorded keep a count of 0
        let mut spans = vec![(0u32, 0u64, 0u32); trees];
        for (b, builder) in builders.iter_mut().enumerate() {
            builder.close();
            for (tree, begin, count) in std::mem::take(&mut builder.spans) {
                debug_assert_eq!(spans[tree as usize].2, 0, "tree {tree} recorded twice");
                spans[tree as usize] = (b as u32, begin, count);
            }
        }
        let mut start = Vec::with_capacity(trees + 1);
        let mut total = 0u64;
        start.push(0);
        for &(_, _, count) in &spans {
            total += u64::from(count);
            start.push(total);
        }
        let Some(any_name) = builders.iter().find_map(|b| b.name.first().copied()) else {
            // No entries at all
            return TreeEntries {
                start,
                ..TreeEntries::default()
            };
        };
        let child = builders.iter_mut().map(|b| std::mem::take(&mut b.child));
        let child = gather(&start, &spans, child.collect(), 0, |out, _, c| {
            out.copy_from_slice(c)
        });
        let name = builders.iter_mut().map(|b| std::mem::take(&mut b.name));
        let name = gather(&start, &spans, name.collect(), any_name, |out, b, n| {
            maps[b].map_into(out, n)
        });
        let mode = builders.iter_mut().map(|b| std::mem::take(&mut b.mode));
        let mode = gather(&start, &spans, mode.collect(), 0, |out, _, m| {
            out.copy_from_slice(m)
        });
        TreeEntries {
            start,
            child,
            name,
            mode,
        }
    }
}

/// Entries of tree index order that one task of the gather fills
#[cfg(not(test))]
const GATHER_CHUNK: u64 = 1 << 20;
/// Small, so that the tests split their few entries into several tasks
#[cfg(test)]
const GATHER_CHUNK: u64 = 2;

/// The elements of `sources` picked out span by span, in parallel over
/// runs of trees of about `GATHER_CHUNK` entries. `copy` fills the place
/// of one span from it, with the index of its source. `fill` is any value,
/// overwritten by the spans. Drops `sources`.
fn gather<T: Copy + Send + Sync>(
    start: &[u64],
    spans: &[(u32, u64, u32)],
    sources: Vec<Vec<T>>,
    fill: T,
    copy: impl Fn(&mut [T], usize, &[T]) + Sync,
) -> Vec<T> {
    let mut out = vec![fill; *start.last().expect("start has a sentinel") as usize];
    // Cut the output at tree boundaries into runs of about GATHER_CHUNK
    let mut runs = Vec::new();
    let mut rest = out.as_mut_slice();
    let mut first = 0;
    while first < spans.len() {
        let mut end = first + 1;
        while end < spans.len() && start[end + 1] - start[first] <= GATHER_CHUNK {
            end += 1;
        }
        let (run, tail) =
            std::mem::take(&mut rest).split_at_mut((start[end] - start[first]) as usize);
        runs.push((first..end, run));
        rest = tail;
        first = end;
    }
    runs.into_par_iter().for_each(|(trees, run)| {
        let mut at = 0;
        for &(source, begin, count) in &spans[trees] {
            let (source, begin, count) = (source as usize, begin as usize, count as usize);
            // A tree that was never recorded has no source
            if count > 0 {
                copy(
                    &mut run[at..at + count],
                    source,
                    &sources[source][begin..begin + count],
                );
                at += count;
            }
        }
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::NameStore;

    #[test]
    fn hash_index_contains_finds_any_type() {
        let mut index = HashIndex::new();
        index.insert([1; HASH_LEN], ObjectId::Tree(0));
        index.insert([2; HASH_LEN], ObjectId::Tag(0));

        assert!(index.contains(&[1; HASH_LEN]));
        assert!(index.contains(&[2; HASH_LEN]));
        assert!(!index.contains(&[3; HASH_LEN]));
        assert_eq!(index.len(), 2);
    }

    #[test]
    fn hash_index_get_returns_typed_id() {
        let mut index = HashIndex::new();
        index.insert([1; HASH_LEN], ObjectId::Commit(4));
        index.insert([2; HASH_LEN], ObjectId::Blob(9));

        assert_eq!(index.get(&[1; HASH_LEN]), Some(ObjectId::Commit(4)));
        assert_eq!(index.get(&[2; HASH_LEN]), Some(ObjectId::Blob(9)));
        assert_eq!(index.get(&[3; HASH_LEN]), None);
    }

    #[test]
    fn hash_index_renumber_changes_each_id_and_keeps_each_hash() {
        let mut index = HashIndex::new();
        index.insert([1; HASH_LEN], ObjectId::Commit(4));
        index.insert([2; HASH_LEN], ObjectId::Blob(9));
        index.insert([3; HASH_LEN], ObjectId::Tree(0));

        index.renumber(|id| ObjectId::new(id.kind(), id.index() + 10));

        assert_eq!(index.len(), 3);
        assert_eq!(index.get(&[1; HASH_LEN]), Some(ObjectId::Commit(14)));
        assert_eq!(index.get(&[2; HASH_LEN]), Some(ObjectId::Blob(19)));
        assert_eq!(index.get(&[3; HASH_LEN]), Some(ObjectId::Tree(10)));
    }

    #[test]
    fn hash_index_typed_lookups_only_match_their_own_kind() {
        let mut index = HashIndex::new();
        index.insert([1; HASH_LEN], ObjectId::Commit(4));
        index.insert([2; HASH_LEN], ObjectId::Tree(5));
        index.insert([3; HASH_LEN], ObjectId::Blob(6));
        index.insert([4; HASH_LEN], ObjectId::Tag(7));

        assert_eq!(index.commit(&[1; HASH_LEN]), Some(4));
        assert_eq!(index.tree(&[2; HASH_LEN]), Some(5));
        assert_eq!(index.blob(&[3; HASH_LEN]), Some(6));
        assert_eq!(index.tag(&[4; HASH_LEN]), Some(7));

        assert_eq!(index.tree(&[1; HASH_LEN]), None);
        assert_eq!(index.commit(&[2; HASH_LEN]), None);
        assert_eq!(index.blob(&[9; HASH_LEN]), None);
    }

    /// Intern `name` in `store`
    fn name(store: &mut NameStore, name: &[u8]) -> NameId {
        store.intern(name).unwrap()
    }

    /// The entries of one builder whose name ids need no change
    fn merge_one(trees: usize, builder: TreeEntriesBuilder, names: &NameStore) -> TreeEntries {
        TreeEntries::merge(trees, vec![(builder, &NameMap::identity(names.len()))])
    }

    /// An entry as (child, name bytes, kind)
    type Resolved = (Option<usize>, Vec<u8>, FileKind);

    /// Each tree's entries with their names as bytes
    fn resolved(entries: &TreeEntries, names: &NameStore) -> Vec<Vec<Resolved>> {
        (0..entries.len())
            .map(|t| {
                entries
                    .entries(t)
                    .map(|(child, name, kind)| (child, names.get(name).to_vec(), kind))
                    .collect()
            })
            .collect()
    }

    #[test]
    fn children_gives_the_child_and_kind_slices_of_one_tree() {
        let mut names = NameStore::new();
        let a = name(&mut names, b"a");
        let mut builder = TreeEntriesBuilder::new();
        builder.begin_tree(1);
        builder.push(Some(4), a, FileKind::Tree);
        builder.push(None, a, FileKind::Submodule);
        builder.push(Some(2), a, FileKind::Regular);
        let entries = merge_one(2, builder, &names);

        assert_eq!(entries.children(0), (&[][..], &[][..]));
        assert_eq!(
            entries.children(1),
            (
                &[4, TreeEntries::NO_CHILD, 2][..],
                &[
                    FileKind::Tree as u8,
                    FileKind::Submodule as u8,
                    FileKind::Regular as u8
                ][..]
            )
        );
    }

    #[test]
    fn tree_entries_lays_trees_out_in_index_order() {
        let mut names = NameStore::new();
        let (a, b, c) = (
            name(&mut names, b"a"),
            name(&mut names, b"b"),
            name(&mut names, b"c"),
        );

        // Recorded out of index order, as pass 2 does
        let mut builder = TreeEntriesBuilder::new();
        builder.begin_tree(2);
        builder.push(Some(7), c, FileKind::Symlink);
        builder.begin_tree(0);
        builder.push(Some(1), a, FileKind::Tree);
        builder.push(Some(5), b, FileKind::Executable);
        builder.begin_tree(1);
        let entries = merge_one(3, builder, &names);

        assert_eq!(entries.len(), 3);
        assert_eq!(entries.entry_count(), 3);
        assert_eq!(
            entries.entries(0).collect::<Vec<_>>(),
            vec![
                (Some(1), a, FileKind::Tree),
                (Some(5), b, FileKind::Executable)
            ]
        );
        assert_eq!(entries.entries(1).count(), 0, "tree 1 is empty");
        assert_eq!(
            entries.entries(2).collect::<Vec<_>>(),
            vec![(Some(7), c, FileKind::Symlink)]
        );
    }

    #[test]
    fn tree_entries_childless_entry_has_no_child() {
        let mut names = NameStore::new();
        let n = name(&mut names, b"sub");

        let mut builder = TreeEntriesBuilder::new();
        builder.begin_tree(0);
        builder.push(None, n, FileKind::Submodule);
        let entries = merge_one(1, builder, &names);

        assert_eq!(
            entries.entries(0).collect::<Vec<_>>(),
            vec![(None, n, FileKind::Submodule)]
        );
    }

    #[test]
    fn tree_entries_never_recorded_tree_is_empty() {
        let entries = TreeEntries::merge(2, Vec::new());
        assert_eq!(entries.len(), 2);
        assert!(!entries.is_empty());
        assert_eq!(entries.entry_count(), 0);
        assert_eq!(entries.entries(0).count(), 0);
        assert_eq!(entries.entries(1).count(), 0);
    }

    /// Record tree `tree` with entries of (child, name, kind) in `builder`,
    /// interning the names in `names`
    fn record(
        builder: &mut TreeEntriesBuilder,
        names: &mut NameStore,
        tree: usize,
        entries: &[(Option<usize>, &[u8], FileKind)],
    ) {
        builder.begin_tree(tree);
        for &(child, n, kind) in entries {
            let id = name(names, n);
            builder.push(child, id, kind);
        }
    }

    #[test]
    fn tree_entries_merge_of_two_workers_equals_one_worker() {
        use FileKind::{Executable, Regular, Tree};
        let t0: &[(Option<usize>, &[u8], FileKind)] =
            &[(Some(1), b"src", Tree), (Some(4), b"README", Regular)];
        let t2: &[(Option<usize>, &[u8], FileKind)] = &[(Some(9), b"build.sh", Executable)];
        let t3: &[(Option<usize>, &[u8], FileKind)] =
            &[(Some(5), b"README", Regular), (None, b"lib", Tree)];
        let t4: &[(Option<usize>, &[u8], FileKind)] = &[];
        // Tree 1 is never recorded

        let mut one = TreeEntriesBuilder::new();
        let mut one_names = NameStore::new();
        for (tree, entries) in [(3, t3), (0, t0), (4, t4), (2, t2)] {
            record(&mut one, &mut one_names, tree, entries);
        }
        let want = resolved(&merge_one(5, one, &one_names), &one_names);

        // The same trees from two workers, each with its own names, interned
        // in another order. Worker a records its trees in two builders that
        // share its names.
        let mut a_names = NameStore::new();
        let mut a1 = TreeEntriesBuilder::new();
        record(&mut a1, &mut a_names, 2, t2);
        let mut a2 = TreeEntriesBuilder::new();
        record(&mut a2, &mut a_names, 3, t3);
        let (mut b, mut b_names) = (TreeEntriesBuilder::new(), NameStore::new());
        record(&mut b, &mut b_names, 4, t4);
        record(&mut b, &mut b_names, 0, t0);
        let mut names = NameStore::new();
        let a_map = names.merge(&a_names).unwrap();
        let b_map = names.merge(&b_names).unwrap();
        let entries = TreeEntries::merge(5, vec![(a1, &a_map), (b, &b_map), (a2, &a_map)]);

        assert_eq!(names.len(), 4, "src, README, build.sh, lib");
        assert_eq!(entries.len(), 5);
        assert_eq!(entries.entry_count(), 5);
        assert_eq!(resolved(&entries, &names), want);
    }

    #[test]
    fn file_kind_survives_the_byte_roundtrip() {
        for kind in [
            FileKind::Tree,
            FileKind::Regular,
            FileKind::Executable,
            FileKind::Symlink,
            FileKind::Submodule,
        ] {
            assert_eq!(FileKind::from_byte(kind as u8), kind);
            assert_eq!(FileKind::from_mode(kind.mode()), Some(kind));
        }
    }

    #[test]
    fn container_starts_with_no_packs_or_copies() {
        let container = ObjectContainer::new();
        assert!(container.packs.is_empty());
        assert!(container.copies.is_empty());

        let container = ObjectContainer::with_capacity(1, 2, 3, 4);
        assert!(container.packs.is_empty());
        assert!(container.copies.is_empty());
    }

    #[test]
    fn copy_mtime_reads_the_pack_or_the_loose_file() {
        let mut container = ObjectContainer::new();
        container
            .packs
            .push(Pack::new(0, "pack-a.pack".to_string(), 100, 1_700_000_000));
        let packed = ObjectCopy::packed(ObjectId::Blob(0), 0, 12, 5, false);
        let loose_a = ObjectCopy::loose(ObjectId::Blob(1), 9);
        let loose_b = ObjectCopy::loose(ObjectId::Tree(0), 9);
        container.loose_mtimes = vec![
            (ObjectId::Tree(0), 1_700_000_300),
            (ObjectId::Blob(1), 1_700_000_200),
        ];
        container.loose_mtimes.sort();

        assert_eq!(container.copy_mtime(&packed), 1_700_000_000);
        assert_eq!(container.copy_mtime(&loose_a), 1_700_000_200);
        assert_eq!(container.copy_mtime(&loose_b), 1_700_000_300);
    }

    #[test]
    fn hash_of_reads_the_right_container() {
        let mut container = ObjectContainer::new();
        container.trees.push(Tree::new(0, [5; HASH_LEN], 0, 0));
        container.tags.push(Tag::new(0, [6; HASH_LEN], 0, 0));

        assert_eq!(container.hash_of(ObjectId::Tree(0)), [5; HASH_LEN]);
        assert_eq!(container.hash_of(ObjectId::Tag(0)), [6; HASH_LEN]);
    }

    #[test]
    fn object_size_reads_the_right_container() {
        let mut container = ObjectContainer::new();
        container.blobs.push(Blob::new(0, [1; HASH_LEN], 7, 0));
        container.trees.push(Tree::new(0, [5; HASH_LEN], 11, 0));

        assert_eq!(container.object_size(ObjectId::Blob(0)), 7);
        assert_eq!(container.object_size(ObjectId::Tree(0)), 11);
    }
}
