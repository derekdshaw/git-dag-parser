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
    pub fn with_capacity(commits: usize, trees: usize, blobs: usize, tags: usize) -> Self {
        Self {
            commits: HashMap::with_capacity(commits),
            trees: HashMap::with_capacity(trees),
            blobs: HashMap::with_capacity(blobs),
            tags: HashMap::with_capacity(tags),
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
}

/// Raw tree data extracted during Phase 1 parsing
#[derive(Debug, Clone)]
pub struct TreeRaw {
    /// Index in the ObjectContainer.trees array
    pub index: usize,
    /// Entries in this tree (files and subdirectories)
    pub entries: Vec<TreeEntry>,
}

/// A single entry in a tree object
#[derive(Debug, Clone)]
pub struct TreeEntry {
    /// File mode (e.g., 100644 for regular file, 040000 for directory)
    pub mode: u32,
    /// Entry name (filename or directory name)
    pub name: String,
    /// Hash of the referenced object (blob or subtree)
    pub hash: ObjectHash,
    /// Resolved index of the child object (set after all objects are indexed)
    pub child_idx: Option<usize>,
    /// True if this entry is a tree (directory), false if blob (file)
    pub is_tree: bool,
}

impl TreeEntry {
    /// Create a new tree entry
    pub fn new(mode: u32, name: String, hash: ObjectHash) -> Self {
        let is_tree = mode == 0o040000;
        Self {
            mode,
            name,
            hash,
            child_idx: None,
            is_tree,
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
