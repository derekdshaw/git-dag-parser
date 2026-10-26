use hashbrown::HashSet;
use serde::{Deserialize, Serialize};

use super::container::ObjectHash;

/// A typed reference to an object: the variant names the container array,
/// the value is the index into it
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ObjectId {
    Commit(usize),
    Tree(usize),
    Blob(usize),
    Tag(usize),
}

/// A Git commit object with its relationships
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Commit {
    /// Index in the commits array (for fast lookups)
    pub index: usize,
    /// Object hash
    pub hash: ObjectHash,
    /// Uncompressed object size
    pub size: u64,
    /// Compressed size on disk
    pub size_disk: u64,
    /// Index of the root tree, if that tree is in the object store
    pub tree: Option<usize>,
    /// Indices of parent commits, in header order
    pub parents: Vec<usize>,
    /// True if any parent is missing from the object store, as at a shallow
    /// clone boundary
    pub boundary: bool,
    /// Committer timestamp, seconds since the Unix epoch
    pub commit_time: i64,
    /// Indices of trees this commit references (includes all trees in the tree hierarchy)
    pub tree_deps: Vec<usize>,
    /// Indices of blobs this commit references (includes all blobs in the tree hierarchy)
    pub blob_deps: Vec<usize>,
}

impl Commit {
    pub fn new(index: usize, hash: ObjectHash, size: u64, size_disk: u64) -> Self {
        Self {
            index,
            hash,
            size,
            size_disk,
            tree: None,
            parents: Vec::new(),
            boundary: false,
            commit_time: 0,
            tree_deps: Vec::new(),
            blob_deps: Vec::new(),
        }
    }
}

/// A Git tree object with its relationships
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tree {
    /// Index in the trees array
    pub index: usize,
    /// Object hash
    pub hash: ObjectHash,
    /// Uncompressed object size
    pub size: u64,
    /// Compressed size on disk
    pub size_disk: u64,
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
    pub fn new(index: usize, hash: ObjectHash, size: u64, size_disk: u64) -> Self {
        Self {
            index,
            hash,
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
    /// Object hash
    pub hash: ObjectHash,
    /// Uncompressed object size
    pub size: u64,
    /// Compressed size on disk
    pub size_disk: u64,
    /// Paths where this blob appears
    pub paths: HashSet<String>,
    /// Indices of commits that reference this blob
    pub commits: Vec<usize>,
    /// Indices of trees that directly contain this blob
    pub trees: Vec<usize>,
}

impl Blob {
    pub fn new(index: usize, hash: ObjectHash, size: u64, size_disk: u64) -> Self {
        Self {
            index,
            hash,
            size,
            size_disk,
            paths: HashSet::new(),
            commits: Vec::new(),
            trees: Vec::new(),
        }
    }
}

/// A Git annotated tag object
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tag {
    /// Index in the tags array
    pub index: usize,
    /// Object hash
    pub hash: ObjectHash,
    /// Uncompressed object size
    pub size: u64,
    /// Compressed size on disk
    pub size_disk: u64,
    /// Tag name from the `tag` header, as raw bytes
    pub name: Vec<u8>,
    /// Hash from the `object` header; kept so a missing target can be reported
    pub target_hash: ObjectHash,
    /// The tagged object (usually a commit), if it is in the object store
    pub target: Option<ObjectId>,
}

impl Tag {
    pub fn new(
        index: usize,
        hash: ObjectHash,
        size: u64,
        size_disk: u64,
        name: Vec<u8>,
        target_hash: ObjectHash,
    ) -> Self {
        Self {
            index,
            hash,
            size,
            size_disk,
            name,
            target_hash,
            target: None,
        }
    }
}

/// What a ref is, from its name
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RefKind {
    /// `refs/heads/*`
    Branch,
    /// `refs/remotes/*`
    Remote,
    /// `refs/tags/*`, lightweight or pointing at an annotated tag object
    Tag,
    /// `refs/stash`
    Stash,
    /// A detached `HEAD`
    Head,
    /// Anything else, such as `refs/notes/*`
    Other,
}

impl RefKind {
    pub fn from_name(name: &[u8]) -> Self {
        if name.starts_with(b"refs/heads/") {
            Self::Branch
        } else if name.starts_with(b"refs/remotes/") {
            Self::Remote
        } else if name.starts_with(b"refs/tags/") {
            Self::Tag
        } else if name == b"refs/stash" {
            Self::Stash
        } else if name == b"HEAD" {
            Self::Head
        } else {
            Self::Other
        }
    }
}

/// A ref: a name pointing directly at an object. Symbolic refs are not kept,
/// since the ref they name is recorded on its own.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Ref {
    /// Full ref name, such as `refs/heads/main`, as raw bytes
    pub name: Vec<u8>,
    pub kind: RefKind,
    /// Hash the ref points at; kept so a missing target can be reported
    pub target_hash: ObjectHash,
    /// The object the ref points at, if it is in the object store
    pub target: Option<ObjectId>,
}

impl Ref {
    pub fn new(name: Vec<u8>, target_hash: ObjectHash) -> Self {
        Self {
            kind: RefKind::from_name(&name),
            name,
            target_hash,
            target: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ref_kind_from_name_classifies_by_namespace() {
        assert_eq!(RefKind::from_name(b"refs/heads/main"), RefKind::Branch);
        assert_eq!(RefKind::from_name(b"refs/remotes/origin/main"), RefKind::Remote);
        assert_eq!(RefKind::from_name(b"refs/tags/v1"), RefKind::Tag);
        assert_eq!(RefKind::from_name(b"refs/stash"), RefKind::Stash);
        assert_eq!(RefKind::from_name(b"HEAD"), RefKind::Head);
        assert_eq!(RefKind::from_name(b"refs/notes/commits"), RefKind::Other);
    }

    #[test]
    fn ref_new_sets_kind_and_leaves_target_unresolved() {
        let r = Ref::new(b"refs/tags/v1".to_vec(), [7; 20]);
        assert_eq!(r.kind, RefKind::Tag);
        assert_eq!(r.target_hash, [7; 20]);
        assert_eq!(r.target, None);
    }
}
