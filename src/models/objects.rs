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

impl ObjectId {
    pub fn new(kind: ObjectKind, index: usize) -> Self {
        match kind {
            ObjectKind::Commit => Self::Commit(index),
            ObjectKind::Tree => Self::Tree(index),
            ObjectKind::Blob => Self::Blob(index),
            ObjectKind::Tag => Self::Tag(index),
        }
    }

    pub fn kind(self) -> ObjectKind {
        match self {
            Self::Commit(_) => ObjectKind::Commit,
            Self::Tree(_) => ObjectKind::Tree,
            Self::Blob(_) => ObjectKind::Blob,
            Self::Tag(_) => ObjectKind::Tag,
        }
    }

    /// Index into the container array for this kind
    pub fn index(self) -> usize {
        match self {
            Self::Commit(i) | Self::Tree(i) | Self::Blob(i) | Self::Tag(i) => i,
        }
    }
}

/// The four kinds of object git stores. Pack entries name them by a 3-bit
/// code, loose objects by a word in their header; this is the common form.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ObjectKind {
    Commit,
    Tree,
    Blob,
    Tag,
}

impl ObjectKind {
    /// From a pack entry's type code. Delta codes (6 and 7) give None: a
    /// delta's kind is whatever its base chain ends in.
    pub fn from_pack_type(code: u8) -> Option<Self> {
        match code {
            1 => Some(Self::Commit),
            2 => Some(Self::Tree),
            3 => Some(Self::Blob),
            4 => Some(Self::Tag),
            _ => None,
        }
    }

    /// From the type word of a loose object header
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "commit" => Some(Self::Commit),
            "tree" => Some(Self::Tree),
            "blob" => Some(Self::Blob),
            "tag" => Some(Self::Tag),
            _ => None,
        }
    }

    /// The type word as it appears in a loose object header
    pub fn name(self) -> &'static str {
        match self {
            Self::Commit => "commit",
            Self::Tree => "tree",
            Self::Blob => "blob",
            Self::Tag => "tag",
        }
    }
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
    /// A tag known only from its header. `name` and `target_hash` are filled
    /// in once the body has been read.
    pub fn new(index: usize, hash: ObjectHash, size: u64, size_disk: u64) -> Self {
        Self {
            index,
            hash,
            size,
            size_disk,
            name: Vec::new(),
            target_hash: [0; 20],
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

/// A pack file in `objects/pack/`, so that an object's on-disk copies can be
/// recorded and retrieved later
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pack {
    /// Index in the packs array
    pub index: usize,
    /// File name of the `.pack` file, such as `pack-<hex>.pack`
    pub name: String,
    /// Size of the `.pack` file in bytes
    pub size: u64,
    /// Modification time of the `.pack` file, seconds since the Unix epoch
    pub mtime: i64,
    /// A `.keep` sidecar exists: the pack must not be repacked away
    pub keep: bool,
    /// A `.mtimes` sidecar exists: the pack holds unreachable objects kept
    /// until they expire
    pub cruft: bool,
    /// A `.promisor` sidecar exists: the pack came from a partial clone and
    /// may reference objects that are not present
    pub promisor: bool,
}

impl Pack {
    /// Record a pack's file facts. Sidecar flags start false and are set from
    /// whichever sidecar files are found next to the pack.
    pub fn new(index: usize, name: String, size: u64, mtime: i64) -> Self {
        Self {
            index,
            name,
            size,
            mtime,
            keep: false,
            cruft: false,
            promisor: false,
        }
    }
}

/// One on-disk copy of an object. The same object can be stored in several
/// packs and as a loose file at once; each location is its own copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectCopy {
    /// The object this is a copy of
    pub object: ObjectId,
    /// Index into the packs array, or None for a loose copy
    pub pack: Option<usize>,
    /// Byte offset of the entry within the pack; None for a loose copy
    pub offset: Option<u64>,
    /// Compressed size on disk: the pack entry's span, or the loose file's size
    pub size_disk: u64,
    /// True if the pack entry is stored as a delta against another entry.
    /// Loose copies are always stored whole.
    pub is_delta: bool,
    /// Modification time of the file holding the copy (the pack file or the
    /// loose file), seconds since the Unix epoch
    pub mtime: i64,
    /// True for the one copy per object that gives it its `size_disk` and
    /// that its body is read from
    pub is_primary: bool,
}

impl ObjectCopy {
    /// A copy stored as an entry in a pack
    pub fn packed(
        object: ObjectId,
        pack: usize,
        offset: u64,
        size_disk: u64,
        is_delta: bool,
        mtime: i64,
    ) -> Self {
        Self {
            object,
            pack: Some(pack),
            offset: Some(offset),
            size_disk,
            is_delta,
            mtime,
            is_primary: false,
        }
    }

    /// A copy stored as a loose object file
    pub fn loose(object: ObjectId, size_disk: u64, mtime: i64) -> Self {
        Self {
            object,
            pack: None,
            offset: None,
            size_disk,
            is_delta: false,
            mtime,
            is_primary: false,
        }
    }

    /// True for a loose copy, false for one in a pack
    pub fn is_loose(&self) -> bool {
        self.pack.is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ref_kind_from_name_classifies_by_namespace() {
        assert_eq!(RefKind::from_name(b"refs/heads/main"), RefKind::Branch);
        assert_eq!(
            RefKind::from_name(b"refs/remotes/origin/main"),
            RefKind::Remote
        );
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

    #[test]
    fn object_id_splits_into_kind_and_index() {
        for kind in [
            ObjectKind::Commit,
            ObjectKind::Tree,
            ObjectKind::Blob,
            ObjectKind::Tag,
        ] {
            let id = ObjectId::new(kind, 42);
            assert_eq!(id.kind(), kind);
            assert_eq!(id.index(), 42);
        }
        assert_eq!(ObjectId::new(ObjectKind::Tree, 3), ObjectId::Tree(3));
    }

    #[test]
    fn object_kind_maps_pack_type_codes() {
        assert_eq!(ObjectKind::from_pack_type(1), Some(ObjectKind::Commit));
        assert_eq!(ObjectKind::from_pack_type(2), Some(ObjectKind::Tree));
        assert_eq!(ObjectKind::from_pack_type(3), Some(ObjectKind::Blob));
        assert_eq!(ObjectKind::from_pack_type(4), Some(ObjectKind::Tag));
        // 0 and 5 are unused; 6 and 7 are deltas, which have no kind of their own
        for code in [0, 5, 6, 7, 8, 255] {
            assert_eq!(ObjectKind::from_pack_type(code), None, "code {code}");
        }
    }

    #[test]
    fn object_kind_maps_loose_header_names() {
        for kind in [
            ObjectKind::Commit,
            ObjectKind::Tree,
            ObjectKind::Blob,
            ObjectKind::Tag,
        ] {
            assert_eq!(ObjectKind::from_name(kind.name()), Some(kind));
        }
        assert_eq!(ObjectKind::from_name("blobs"), None);
        assert_eq!(ObjectKind::from_name(""), None);
    }

    #[test]
    fn tag_new_leaves_body_fields_unread() {
        let t = Tag::new(2, [9; 20], 150, 120);

        assert_eq!(t.index, 2);
        assert_eq!(t.hash, [9; 20]);
        assert_eq!((t.size, t.size_disk), (150, 120));
        assert!(t.name.is_empty());
        assert_eq!(t.target_hash, [0; 20]);
        assert_eq!(t.target, None);
    }

    #[test]
    fn pack_new_records_file_facts_and_clears_sidecar_flags() {
        let p = Pack::new(3, "pack-0123abcd.pack".to_string(), 4096, 1_700_000_000);

        assert_eq!(p.index, 3);
        assert_eq!(p.name, "pack-0123abcd.pack");
        assert_eq!(p.size, 4096);
        assert_eq!(p.mtime, 1_700_000_000);
        assert!(!p.keep);
        assert!(!p.cruft);
        assert!(!p.promisor);
    }

    #[test]
    fn pack_sidecar_flags_are_independent() {
        let mut p = Pack::new(0, "pack-a.pack".to_string(), 1, 2);
        p.cruft = true;

        assert!(!p.keep);
        assert!(p.cruft);
        assert!(!p.promisor);
    }

    #[test]
    fn object_copy_packed_records_location_in_pack() {
        let c = ObjectCopy::packed(ObjectId::Blob(5), 2, 1234, 77, true, 1_700_000_000);

        assert_eq!(c.object, ObjectId::Blob(5));
        assert_eq!(c.pack, Some(2));
        assert_eq!(c.offset, Some(1234));
        assert_eq!(c.size_disk, 77);
        assert!(c.is_delta);
        assert_eq!(c.mtime, 1_700_000_000);
        assert!(!c.is_loose());
    }

    #[test]
    fn object_copy_loose_has_no_pack_offset_or_delta() {
        let c = ObjectCopy::loose(ObjectId::Commit(1), 250, 1_700_000_123);

        assert_eq!(c.object, ObjectId::Commit(1));
        assert_eq!(c.pack, None);
        assert_eq!(c.offset, None);
        assert_eq!(c.size_disk, 250);
        assert!(!c.is_delta);
        assert_eq!(c.mtime, 1_700_000_123);
        assert!(c.is_loose());
    }

    #[test]
    fn object_copy_round_trips_through_serde() {
        for copy in [
            ObjectCopy::packed(ObjectId::Tree(9), 0, u64::MAX, 10, false, 1),
            ObjectCopy::loose(ObjectId::Tag(0), 20, 2),
        ] {
            let json = serde_json::to_string(&copy).unwrap();
            let back: ObjectCopy = serde_json::from_str(&json).unwrap();
            assert_eq!(back, copy);
        }
    }

    #[test]
    fn pack_round_trips_through_serde() {
        let mut p = Pack::new(7, "pack-ffff.pack".to_string(), 123_456, 1_700_000_999);
        p.keep = true;
        p.promisor = true;

        let json = serde_json::to_string(&p).unwrap();
        let back: Pack = serde_json::from_str(&json).unwrap();

        assert_eq!(back, p);
    }
}
