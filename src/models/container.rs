use hashbrown::HashMap;
use serde::{Deserialize, Serialize};

use super::objects::{Blob, Commit, Tag, Tree};

/// Hash type alias for Git object hashes (20 bytes for SHA-1)
pub type ObjectHash = [u8; 20];

/// Container for all parsed Git objects and their relationships
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct ObjectContainer {
    /// All commit objects
    pub commits: Vec<Commit>,
    /// All tree objects
    pub trees: Vec<Tree>,
    /// All blob objects
    pub blobs: Vec<Blob>,
    /// All tag objects
    pub tags: Vec<Tag>,
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
        }
    }
}

/// Index for looking up objects by hash
#[derive(Debug, Default)]
pub struct HashIndex {
    pub commits: HashMap<ObjectHash, usize>,
    pub trees: HashMap<ObjectHash, usize>,
    pub blobs: HashMap<ObjectHash, usize>,
    pub tags: HashMap<ObjectHash, usize>,
}

impl HashIndex {
    pub fn new() -> Self {
        Self::default()
    }

    /// Pre-allocate capacity based on expected object counts
    #[allow(dead_code)]
    pub fn with_capacity(commits: usize, trees: usize, blobs: usize, tags: usize) -> Self {
        Self {
            commits: HashMap::with_capacity(commits),
            trees: HashMap::with_capacity(trees),
            blobs: HashMap::with_capacity(blobs),
            tags: HashMap::with_capacity(tags),
        }
    }

    /// True if an object with this hash is indexed under any type
    pub fn contains(&self, hash: &ObjectHash) -> bool {
        self.commits.contains_key(hash)
            || self.trees.contains_key(hash)
            || self.blobs.contains_key(hash)
            || self.tags.contains_key(hash)
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
    /// Entry name (filename or directory name), as raw bytes since git
    /// does not require names to be UTF-8
    pub name: Vec<u8>,
    /// Hash of the referenced object (blob or subtree)
    pub hash: ObjectHash,
    /// Resolved index of the child object (set after all objects are indexed)
    pub child_idx: Option<usize>,
    /// The kind of entry (tree, regular file, executable, symlink, submodule)
    pub kind: FileKind,
}

impl TreeEntry {
    /// Create a new tree entry
    pub fn new(mode: u32, name: Vec<u8>, hash: ObjectHash) -> Self {
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
        index.trees.insert([1; 20], 0);
        index.tags.insert([2; 20], 0);

        assert!(index.contains(&[1; 20]));
        assert!(index.contains(&[2; 20]));
        assert!(!index.contains(&[3; 20]));
    }
}
