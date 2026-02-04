use hashbrown::HashSet;
use serde::{Deserialize, Serialize};

/// A Git commit object with its relationships
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Commit {
    /// Index in the commits array (for fast lookups)
    pub index: usize,
    /// Uncompressed object size
    pub size: u32,
    /// Compressed size on disk
    pub size_disk: u32,
    /// Indices of trees this commit references (includes all trees in the tree hierarchy)
    pub tree_deps: Vec<usize>,
    /// Indices of blobs this commit references (includes all blobs in the tree hierarchy)
    pub blob_deps: Vec<usize>,
    /// Indices of tags pointing to this commit
    pub tag_deps: Vec<usize>,
    /// Lightweight tag names (not tag objects)
    pub lightweight_tags: Vec<String>,
}

impl Commit {
    pub fn new(index: usize, size: u32, size_disk: u32) -> Self {
        Self {
            index,
            size,
            size_disk,
            tree_deps: Vec::new(),
            blob_deps: Vec::new(),
            tag_deps: Vec::new(),
            lightweight_tags: Vec::new(),
        }
    }
}

/// A Git tree object with its relationships
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tree {
    /// Index in the trees array
    pub index: usize,
    /// Uncompressed object size
    pub size: u32,
    /// Compressed size on disk
    pub size_disk: u32,
    /// Paths where this tree appears (a tree can appear at multiple paths across commits)
    pub paths: HashSet<String>,
    /// Indices of commits that reference this tree
    pub commits: Vec<usize>,
    /// Indices of blobs directly contained in this tree (not recursive)
    pub blobs: Vec<usize>,
    /// Indices of subtrees directly contained in this tree (not recursive)
    pub subtrees: Vec<usize>,
}

impl Tree {
    pub fn new(index: usize, size: u32, size_disk: u32) -> Self {
        Self {
            index,
            size,
            size_disk,
            paths: HashSet::new(),
            commits: Vec::new(),
            blobs: Vec::new(),
            subtrees: Vec::new(),
        }
    }
}

/// A Git blob object with its relationships
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Blob {
    /// Index in the blobs array
    pub index: usize,
    /// Uncompressed object size
    pub size: u32,
    /// Compressed size on disk
    pub size_disk: u32,
    /// Paths where this blob appears
    pub paths: HashSet<String>,
    /// Indices of commits that reference this blob
    pub commits: Vec<usize>,
    /// Indices of trees that directly contain this blob
    pub trees: Vec<usize>,
}

impl Blob {
    pub fn new(index: usize, size: u32, size_disk: u32) -> Self {
        Self {
            index,
            size,
            size_disk,
            paths: HashSet::new(),
            commits: Vec::new(),
            trees: Vec::new(),
        }
    }
}

/// A Git tag object with its relationships
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tag {
    /// Index in the tags array
    pub index: usize,
    /// Uncompressed object size
    pub size: u32,
    /// Compressed size on disk
    pub size_disk: u32,
    /// Tag name
    pub name: Option<String>,
    /// Index of the commit this tag points to
    pub commit_index: Option<usize>,
}

impl Tag {
    pub fn new(index: usize, size: u32, size_disk: u32) -> Self {
        Self {
            index,
            size,
            size_disk,
            name: None,
            commit_index: None,
        }
    }
}
