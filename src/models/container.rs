use hashbrown::HashMap;

use super::name_store::NameId;
use super::objects::{Blob, Commit, ObjectCopy, ObjectId, Pack, Ref, Tag, Tree};

/// Hash type alias for Git object hashes (20 bytes for SHA-1)
pub type ObjectHash = [u8; 20];

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

/// Raw commit data extracted during Phase 1 parsing
#[derive(Debug)]
pub struct CommitRaw {
    /// Index in the ObjectContainer.commits array
    pub index: usize,
    /// Hash of the root tree for this commit
    pub root_tree_hash: ObjectHash,
    /// Hashes of parent commits (for DAG structure)
    pub parent_hashes: Vec<ObjectHash>,
    /// Committer timestamp, seconds since the Unix epoch
    pub commit_time: i64,
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
}

/// Builds a `TreeEntries`. Trees can be recorded in any order, since pass 2
/// reads them pack by pack in offset order; `finish` lays the entries out
/// in tree index order. A tree that is never recorded has no entries.
#[derive(Debug)]
pub struct TreeEntriesBuilder {
    /// Entries in the order they were recorded; `finish` reorders them
    child: Vec<u32>,
    name: Vec<NameId>,
    mode: Vec<u8>,
    /// Each tree's entries in the arrival arrays: (first position, count)
    span: Vec<(u64, u32)>,
    /// The tree entries are currently recorded for, with its first position
    open: Option<(usize, u64)>,
}

impl TreeEntriesBuilder {
    /// A builder for the entries of `trees` trees
    pub fn new(trees: usize) -> Self {
        Self {
            child: Vec::new(),
            name: Vec::new(),
            mode: Vec::new(),
            span: vec![(0, 0); trees],
            open: None,
        }
    }

    /// Start recording the entries of `tree`: pushes until the next
    /// `begin_tree` or `finish` belong to it.
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
            self.span[tree] = (
                begin,
                u32::try_from(count).expect("tree entry count fits in u32"),
            );
        }
    }

    /// Lay the entries out in tree index order. The arrays are reordered
    /// one at a time, each source freed right after, to limit peak memory.
    pub fn finish(mut self) -> TreeEntries {
        self.close();
        let mut start = Vec::with_capacity(self.span.len() + 1);
        let mut total = 0u64;
        start.push(0);
        for &(_, count) in &self.span {
            total += u64::from(count);
            start.push(total);
        }
        let child = reorder(&self.span, std::mem::take(&mut self.child));
        let name = reorder(&self.span, std::mem::take(&mut self.name));
        let mode = reorder(&self.span, std::mem::take(&mut self.mode));
        TreeEntries {
            start,
            child,
            name,
            mode,
        }
    }
}

/// The elements of `source` picked out span by span, dropping `source`
fn reorder<T: Copy>(spans: &[(u64, u32)], source: Vec<T>) -> Vec<T> {
    let total: usize = spans.iter().map(|&(_, count)| count as usize).sum();
    let mut out = Vec::with_capacity(total);
    for &(begin, count) in spans {
        let begin = begin as usize;
        out.extend_from_slice(&source[begin..begin + count as usize]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_index_contains_finds_any_type() {
        let mut index = HashIndex::new();
        index.insert([1; 20], ObjectId::Tree(0));
        index.insert([2; 20], ObjectId::Tag(0));

        assert!(index.contains(&[1; 20]));
        assert!(index.contains(&[2; 20]));
        assert!(!index.contains(&[3; 20]));
        assert_eq!(index.len(), 2);
    }

    #[test]
    fn hash_index_get_returns_typed_id() {
        let mut index = HashIndex::new();
        index.insert([1; 20], ObjectId::Commit(4));
        index.insert([2; 20], ObjectId::Blob(9));

        assert_eq!(index.get(&[1; 20]), Some(ObjectId::Commit(4)));
        assert_eq!(index.get(&[2; 20]), Some(ObjectId::Blob(9)));
        assert_eq!(index.get(&[3; 20]), None);
    }

    #[test]
    fn hash_index_typed_lookups_only_match_their_own_kind() {
        let mut index = HashIndex::new();
        index.insert([1; 20], ObjectId::Commit(4));
        index.insert([2; 20], ObjectId::Tree(5));
        index.insert([3; 20], ObjectId::Blob(6));
        index.insert([4; 20], ObjectId::Tag(7));

        assert_eq!(index.commit(&[1; 20]), Some(4));
        assert_eq!(index.tree(&[2; 20]), Some(5));
        assert_eq!(index.blob(&[3; 20]), Some(6));
        assert_eq!(index.tag(&[4; 20]), Some(7));

        assert_eq!(index.tree(&[1; 20]), None);
        assert_eq!(index.commit(&[2; 20]), None);
        assert_eq!(index.blob(&[9; 20]), None);
    }

    /// Intern `name` in `store`
    fn name(store: &mut crate::models::NameStore, name: &[u8]) -> NameId {
        store.intern(name).unwrap()
    }

    #[test]
    fn tree_entries_lays_trees_out_in_index_order() {
        let mut names = crate::models::NameStore::new();
        let (a, b, c) = (
            name(&mut names, b"a"),
            name(&mut names, b"b"),
            name(&mut names, b"c"),
        );

        // Recorded out of index order, as pass 2 does
        let mut builder = TreeEntriesBuilder::new(3);
        builder.begin_tree(2);
        builder.push(Some(7), c, FileKind::Symlink);
        builder.begin_tree(0);
        builder.push(Some(1), a, FileKind::Tree);
        builder.push(Some(5), b, FileKind::Executable);
        builder.begin_tree(1);
        let entries = builder.finish();

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
        let mut names = crate::models::NameStore::new();
        let n = name(&mut names, b"sub");

        let mut builder = TreeEntriesBuilder::new(1);
        builder.begin_tree(0);
        builder.push(None, n, FileKind::Submodule);
        let entries = builder.finish();

        assert_eq!(
            entries.entries(0).collect::<Vec<_>>(),
            vec![(None, n, FileKind::Submodule)]
        );
    }

    #[test]
    fn tree_entries_never_recorded_tree_is_empty() {
        let entries = TreeEntriesBuilder::new(2).finish();
        assert_eq!(entries.len(), 2);
        assert!(!entries.is_empty());
        assert_eq!(entries.entry_count(), 0);
        assert_eq!(entries.entries(0).count(), 0);
        assert_eq!(entries.entries(1).count(), 0);
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
        container.trees.push(Tree::new(0, [5; 20], 0, 0));
        container.tags.push(Tag::new(0, [6; 20], 0, 0));

        assert_eq!(container.hash_of(ObjectId::Tree(0)), [5; 20]);
        assert_eq!(container.hash_of(ObjectId::Tag(0)), [6; 20]);
    }
}
