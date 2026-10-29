use hashbrown::HashMap;

use super::name_store::NameId;
use super::objects::{Blob, Commit, ObjectCopy, ObjectId, Pack, Ref, Tag, Tree};

/// Hash type alias for Git object hashes (20 bytes for SHA-1)
pub type ObjectHash = [u8; 20];

/// Container for all parsed Git objects and their relationships
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

/// Raw tree data extracted during Phase 1 parsing
#[derive(Debug, Clone)]
pub struct TreeRaw {
    /// Index in the ObjectContainer.trees array
    pub index: usize,
    /// Entries in this tree (files and subdirectories)
    pub entries: Vec<TreeEntry>,
}

/// Git file mode constants
pub mod mode {
    pub const TREE: u32 = 0o040000;
    pub const REGULAR: u32 = 0o100644;
    pub const EXECUTABLE: u32 = 0o100755;
    pub const SYMLINK: u32 = 0o120000;
    pub const SUBMODULE: u32 = 0o160000;
}

/// The kind of object a tree entry references
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum FileKind {
    /// Directory (tree object)
    Tree = mode::TREE,
    /// Regular file
    Regular = mode::REGULAR,
    /// Executable file
    Executable = mode::EXECUTABLE,
    /// Symbolic link
    Symlink = mode::SYMLINK,
    /// Gitlink (submodule reference)
    Submodule = mode::SUBMODULE,
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
        self as u32
    }
}

/// A single entry in a tree object
#[derive(Debug, Clone)]
pub struct TreeEntry {
    /// Entry name (filename or directory name), interned in the parse's
    /// `NameStore`. Names are kept as bytes there, since git does not
    /// require them to be UTF-8.
    pub name: NameId,
    /// Hash of the referenced object (blob or subtree)
    pub hash: ObjectHash,
    /// Resolved index of the child object (set after all objects are indexed)
    pub child_idx: Option<usize>,
    /// The kind of entry (tree, regular file, executable, symlink, submodule)
    pub kind: FileKind,
}

impl TreeEntry {
    /// Create a new tree entry
    pub fn new(mode: u32, name: NameId, hash: ObjectHash) -> Self {
        let kind = FileKind::from_mode(mode).unwrap_or(FileKind::Regular);
        Self {
            name,
            hash,
            child_idx: None,
            kind,
        }
    }
}

/// Result of walking a single commit's tree (returned by worker threads)
#[derive(Debug)]
pub struct WalkResult {
    /// Index of the commit that was walked
    pub commit_idx: usize,
    /// Trees visited during the walk: (tree_idx, path)
    pub trees_visited: Vec<(usize, String)>,
    /// Blobs visited during the walk: (blob_idx, path)
    pub blobs_visited: Vec<(usize, String)>,
}

impl WalkResult {
    pub fn new(commit_idx: usize) -> Self {
        Self {
            commit_idx,
            trees_visited: Vec::new(),
            blobs_visited: Vec::new(),
        }
    }
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
