//! Pass 1 of parsing: find every pack and loose file, read object headers
//! only, and decide which on-disk copy of each object is its primary.
//!
//! Packs are read in offset order so each is touched sequentially from
//! front to back. While reading, an object gets a provisional index the
//! first time its hash is seen. When the scan ends, each kind is sorted by
//! hash, so the final index does not depend on how the objects are packed.

use std::cmp::Reverse;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Instant, UNIX_EPOCH};

use anyhow::{Context, Result};
use memmap2::Mmap;
use tracing::info;

use super::content::hex_to_hash;
use super::loose::read_loose_header;
use super::pack::{scan_pack, verify_pack_header, PackIndex};
use crate::models::container::{HashIndex, ObjectHash};
use crate::models::{
    Blob, Commit, ObjectContainer, ObjectCopy, ObjectId, ObjectKind, Pack, Tag, Tree,
};

/// A usable pack: one with both a `.pack` and an `.idx`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackFile {
    pub pack: Pack,
    pub pack_path: PathBuf,
    pub idx_path: PathBuf,
}

/// Find every pack in `objects/pack/` that has an index, in name order, and
/// read its size, mtime and sidecar flags. `Pack::index` is the position in
/// the returned list.
pub fn discover_packs(pack_dir: &Path) -> Result<Vec<PackFile>> {
    if !pack_dir.is_dir() {
        return Ok(Vec::new());
    }

    let mut found = Vec::new();
    for entry in fs::read_dir(pack_dir).context("Failed to read pack directory")? {
        let idx_path = entry?.path();
        if idx_path.extension().is_none_or(|ext| ext != "idx") {
            continue;
        }
        let pack_path = idx_path.with_extension("pack");
        let Ok(meta) = fs::metadata(&pack_path) else {
            continue; // an .idx without its .pack
        };
        let name = pack_path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        found.push((name, pack_path, idx_path, meta.len(), mtime_seconds(&meta)?));
    }

    found.sort_by(|a, b| a.0.cmp(&b.0));

    Ok(found
        .into_iter()
        .enumerate()
        .map(|(index, (name, pack_path, idx_path, size, mtime))| {
            let mut pack = Pack::new(index, name, size, mtime);
            pack.keep = pack_path.with_extension("keep").exists();
            pack.cruft = pack_path.with_extension("mtimes").exists();
            pack.promisor = pack_path.with_extension("promisor").exists();
            PackFile {
                pack,
                pack_path,
                idx_path,
            }
        })
        .collect())
}

/// Modification time as seconds since the Unix epoch
pub fn mtime_seconds(meta: &fs::Metadata) -> Result<i64> {
    let modified = meta
        .modified()
        .context("Filesystem does not report mtime")?;
    Ok(match modified.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        Err(e) => -(e.duration().as_secs() as i64),
    })
}

/// One on-disk copy as found in pass 1, before it is tied to an object
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScannedCopy {
    pub hash: ObjectHash,
    pub kind: ObjectKind,
    /// Inflated size
    pub size: u64,
    /// Pack index, or None for a loose file
    pub pack: Option<usize>,
    /// Offset in the pack; 0 for a loose file
    pub offset: u64,
    pub size_disk: u64,
    pub is_delta: bool,
    /// mtime of the pack or loose file, seconds since the Unix epoch
    pub mtime: i64,
}

/// The rule for choosing an object's primary copy: a packed copy over a
/// loose one; a pack that is neither cruft nor promisor over one that is;
/// a newer pack over an older one; and finally the pack that sorts first by
/// name, so the choice is stable. An object has at most one loose copy, so a
/// loose copy is only ever ranked against packed ones.
fn rank<'a>(copy: &ObjectCopy, packs: &'a [Pack]) -> (bool, bool, i64, Reverse<&'a str>) {
    match copy.pack() {
        Some(p) => {
            let pack = &packs[p];
            (
                true,
                !pack.cruft && !pack.promisor,
                pack.mtime,
                Reverse(pack.name.as_str()),
            )
        }
        None => (false, true, 0, Reverse("")),
    }
}

/// True if `candidate` should replace `current` as an object's primary copy
pub fn better_copy(candidate: &ObjectCopy, current: &ObjectCopy, packs: &[Pack]) -> bool {
    rank(candidate, packs) > rank(current, packs)
}

/// Everything pass 1 learns: objects with their sizes, every copy, and which
/// copy of each object is primary
pub struct Scan {
    pub container: ObjectContainer,
    pub index: HashIndex,
    /// Position in `container.copies` of each object's primary copy, per
    /// kind, indexed by the object's index
    primary: [Vec<u32>; 4],
}

impl Scan {
    pub fn new(packs: Vec<Pack>, expected_objects: usize) -> Self {
        let mut container = ObjectContainer::new();
        container.packs = packs;
        container.copies = Vec::with_capacity(expected_objects);
        Self {
            container,
            index: HashIndex::with_capacity(expected_objects),
            primary: Default::default(),
        }
    }

    /// Add a copy. The first copy of a hash creates the object and assigns
    /// its index; every copy is kept; the primary follows `better_copy`.
    pub fn record(&mut self, found: ScannedCopy) {
        let (id, is_new) = match self.index.get(&found.hash) {
            Some(id) => (id, false),
            None => (self.create(&found), true),
        };

        let copy = match found.pack {
            Some(pack) => {
                ObjectCopy::packed(id, pack, found.offset, found.size_disk, found.is_delta)
            }
            None => {
                self.container.loose_mtimes.push((id, found.mtime));
                ObjectCopy::loose(id, found.size_disk)
            }
        };
        let pos = self.container.copies.len();
        self.container.copies.push(copy);

        let slot = &mut self.primary[id.kind() as usize];
        if is_new {
            slot.push(pos as u32);
        } else if better_copy(
            &copy,
            &self.container.copies[slot[id.index()] as usize],
            &self.container.packs,
        ) {
            slot[id.index()] = pos as u32;
            self.set_size_disk(id, copy.size_disk);
        }
    }

    /// Put each kind's objects in hash order, so an object's index does not
    /// depend on which packs or loose files it was read from, and mark each
    /// object's primary copy.
    pub fn finish(&mut self) {
        let c = &mut self.container;
        let new_index = [
            sort_by_hash(&mut c.commits, |o| o.hash, |o| &mut o.index),
            sort_by_hash(&mut c.trees, |o| o.hash, |o| &mut o.index),
            sort_by_hash(&mut c.blobs, |o| o.hash, |o| &mut o.index),
            sort_by_hash(&mut c.tags, |o| o.hash, |o| &mut o.index),
        ];

        for (kind, map) in new_index.iter().enumerate() {
            let old = std::mem::take(&mut self.primary[kind]);
            let mut primary = vec![0; old.len()];
            for (old_index, pos) in old.into_iter().enumerate() {
                primary[map[old_index]] = pos;
            }
            self.primary[kind] = primary;
        }

        let renumber =
            |id: ObjectId| ObjectId::new(id.kind(), new_index[id.kind() as usize][id.index()]);
        for copy in &mut c.copies {
            copy.set_object(renumber(copy.object()));
        }
        for (id, _) in &mut c.loose_mtimes {
            *id = renumber(*id);
        }
        c.loose_mtimes.sort_unstable();
        for positions in &self.primary {
            for &pos in positions {
                c.copies[pos as usize].is_primary = true;
            }
        }

        let mut index = HashIndex::with_capacity(self.index.len());
        index_all(&mut index, &c.commits, |o| o.hash, ObjectId::Commit);
        index_all(&mut index, &c.trees, |o| o.hash, ObjectId::Tree);
        index_all(&mut index, &c.blobs, |o| o.hash, ObjectId::Blob);
        index_all(&mut index, &c.tags, |o| o.hash, ObjectId::Tag);
        self.index = index;
    }

    /// Position in `container.copies` of the object's primary copy
    pub fn primary(&self, id: ObjectId) -> usize {
        self.primary[id.kind() as usize][id.index()] as usize
    }

    fn create(&mut self, found: &ScannedCopy) -> ObjectId {
        let c = &mut self.container;
        let (hash, size, size_disk) = (found.hash, found.size, found.size_disk);
        let id = match found.kind {
            ObjectKind::Commit => {
                let i = c.commits.len();
                c.commits.push(Commit::new(i, hash, size, size_disk));
                ObjectId::Commit(i)
            }
            ObjectKind::Tree => {
                let i = c.trees.len();
                c.trees.push(Tree::new(i, hash, size, size_disk));
                ObjectId::Tree(i)
            }
            ObjectKind::Blob => {
                let i = c.blobs.len();
                c.blobs.push(Blob::new(i, hash, size, size_disk));
                ObjectId::Blob(i)
            }
            ObjectKind::Tag => {
                let i = c.tags.len();
                c.tags.push(Tag::new(i, hash, size, size_disk));
                ObjectId::Tag(i)
            }
        };
        self.index.insert(hash, id);
        id
    }

    fn set_size_disk(&mut self, id: ObjectId, size_disk: u64) {
        let c = &mut self.container;
        match id {
            ObjectId::Commit(i) => c.commits[i].size_disk = size_disk,
            ObjectId::Tree(i) => c.trees[i].size_disk = size_disk,
            ObjectId::Blob(i) => c.blobs[i].size_disk = size_disk,
            ObjectId::Tag(i) => c.tags[i].size_disk = size_disk,
        }
    }
}

/// Sort objects by hash and renumber them. Returns the new index of each
/// object, by its old index.
fn sort_by_hash<T>(
    objects: &mut [T],
    hash: impl Fn(&T) -> ObjectHash,
    index: impl Fn(&mut T) -> &mut usize,
) -> Vec<usize> {
    objects.sort_unstable_by_key(|o| hash(o));
    let mut new_index = vec![0; objects.len()];
    for (pos, object) in objects.iter_mut().enumerate() {
        let slot = index(object);
        new_index[*slot] = pos;
        *slot = pos;
    }
    new_index
}

fn index_all<T>(
    index: &mut HashIndex,
    objects: &[T],
    hash: impl Fn(&T) -> ObjectHash,
    id: impl Fn(usize) -> ObjectId,
) {
    for (i, object) in objects.iter().enumerate() {
        index.insert(hash(object), id(i));
    }
}

/// A pack held open across both passes
pub struct OpenPack {
    pub file: PackFile,
    pub index: PackIndex,
    pub data: Mmap,
}

/// The result of pass 1, with the packs still open for pass 2
pub struct Scanned {
    pub scan: Scan,
    pub packs: Vec<OpenPack>,
    pub objects_dir: PathBuf,
}

/// Pass 1: read every pack index, every pack's headers in offset order, and
/// every loose object's header. Packs come first, in name order, so their
/// copies sit in `container.copies` grouped by pack and sorted by offset.
pub fn scan_repository(git_dir: &Path) -> Result<Scanned> {
    let start = Instant::now();
    let objects_dir = git_dir.join("objects");

    // Step 1: indexes and loose file names, so the hash index can be sized
    // once instead of growing
    let mut packs = Vec::new();
    for file in discover_packs(&objects_dir.join("pack"))? {
        let index = PackIndex::parse(&file.idx_path)
            .with_context(|| format!("Failed to parse index of {}", file.pack.name))?;
        let data = unsafe { Mmap::map(&fs::File::open(&file.pack_path)?)? };
        verify_pack_header(&data).with_context(|| format!("Bad pack {}", file.pack.name))?;
        packs.push(OpenPack { file, index, data });
    }
    let loose_files = list_loose_objects(&objects_dir)?;

    let packed_entries: usize = packs.iter().map(|p| p.index.entries.len()).sum();
    let expected = packed_entries + loose_files.len();
    info!(
        "Found {} pack(s) with {} entries and {} loose objects ({:?})",
        packs.len(),
        packed_entries,
        loose_files.len(),
        start.elapsed()
    );

    let mut scan = Scan::new(
        packs.iter().map(|p| p.file.pack.clone()).collect(),
        expected,
    );

    // Step 2: pack headers in offset order
    for open in &packs {
        let pack_start = Instant::now();
        let pack = &open.file.pack;
        for entry in scan_pack(&open.data, &open.index)
            .with_context(|| format!("Failed to scan {}", pack.name))?
        {
            scan.record(ScannedCopy {
                hash: entry.hash,
                kind: entry.kind,
                size: entry.size,
                pack: Some(pack.index),
                offset: entry.offset,
                size_disk: entry.size_disk,
                is_delta: entry.is_delta,
                mtime: pack.mtime,
            });
        }
        info!(
            "Scanned {} ({:.2} MB, {} entries) in {:?}",
            pack.name,
            pack.size as f64 / (1024.0 * 1024.0),
            open.index.entries.len(),
            pack_start.elapsed()
        );
    }

    // Step 3: loose object headers
    for (hash, path) in &loose_files {
        let meta = fs::metadata(path)?;
        let (kind, size) = read_loose_header(path)
            .with_context(|| format!("Failed to read loose object {}", path.display()))?;
        scan.record(ScannedCopy {
            hash: *hash,
            kind,
            size,
            pack: None,
            offset: 0,
            size_disk: meta.len(),
            is_delta: false,
            mtime: mtime_seconds(&meta)?,
        });
    }

    scan.finish();

    let c = &scan.container;
    info!(
        "Pass 1 (headers): C:{} T:{} B:{} t:{} from {} copies in {:?}",
        c.commits.len(),
        c.trees.len(),
        c.blobs.len(),
        c.tags.len(),
        c.copies.len(),
        start.elapsed()
    );

    Ok(Scanned {
        scan,
        packs,
        objects_dir,
    })
}

/// Every loose object file under `objects/xx/`, with its hash
fn list_loose_objects(objects_dir: &Path) -> Result<Vec<(ObjectHash, PathBuf)>> {
    let mut found = Vec::new();
    if !objects_dir.is_dir() {
        return Ok(found);
    }
    for entry in fs::read_dir(objects_dir)? {
        let dir = entry?.path();
        let Some(prefix) = dir.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !dir.is_dir() || prefix.len() != 2 || !prefix.bytes().all(|b| b.is_ascii_hexdigit()) {
            continue;
        }
        for entry in fs::read_dir(&dir)? {
            let path = entry?.path();
            let Some(rest) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if !path.is_file() || rest.len() != 38 || !rest.bytes().all(|b| b.is_ascii_hexdigit()) {
                continue;
            }
            let hash = hex_to_hash(format!("{prefix}{rest}").as_bytes())?;
            found.push((hash, path));
        }
    }
    Ok(found)
}

#[cfg(feature = "bench")]
mod benches {
    use divan::counter::ItemsCount;
    use divan::Bencher;

    use super::{Scan, ScannedCopy};
    use crate::models::{ObjectKind, Pack};

    /// `n` distinct objects in one pack, arriving in pseudo-random hash
    /// order as they do in offset order: 10% commits, 40% trees, 50% blobs
    fn copies(n: usize) -> Vec<ScannedCopy> {
        (0..n)
            .map(|i| {
                let mut hash = [0u8; 20];
                hash[..8]
                    .copy_from_slice(&(i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15).to_be_bytes());
                let kind = match i % 10 {
                    0 => ObjectKind::Commit,
                    1..=4 => ObjectKind::Tree,
                    _ => ObjectKind::Blob,
                };
                ScannedCopy {
                    hash,
                    kind,
                    size: 200,
                    pack: Some(0),
                    offset: 12 + 100 * i as u64,
                    size_disk: 100,
                    is_delta: i % 2 == 0,
                    mtime: 1_700_000_000,
                }
            })
            .collect()
    }

    /// Pass 1 bookkeeping: record every copy, then sort each kind by hash
    #[divan::bench(args = [100_000])]
    fn record_and_finish(bencher: Bencher, n: usize) {
        let copies = copies(n);
        let packs = vec![Pack::new(
            0,
            "pack-a.pack".to_string(),
            1 << 30,
            1_700_000_000,
        )];
        bencher
            .counter(ItemsCount::new(n))
            .with_inputs(|| Scan::new(packs.clone(), n))
            .bench_local_values(|mut scan| {
                for copy in &copies {
                    scan.record(*copy);
                }
                scan.finish();
                scan
            });
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::{Duration, SystemTime};

    use super::*;
    use crate::models::ObjectId;

    fn touch(path: &Path, contents: &[u8], mtime: i64) {
        fs::write(path, contents).unwrap();
        let time = SystemTime::UNIX_EPOCH + Duration::from_secs(mtime as u64);
        fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(time)
            .unwrap();
    }

    #[test]
    fn discover_packs_reads_file_facts_and_sidecars_in_name_order() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        // Listed out of name order to show the result is sorted
        touch(&d.join("pack-b.pack"), &[0; 300], 1_700_000_200);
        touch(&d.join("pack-b.idx"), b"idx", 1);
        touch(&d.join("pack-b.keep"), b"", 1);
        touch(&d.join("pack-a.pack"), &[0; 100], 1_700_000_100);
        touch(&d.join("pack-a.idx"), b"idx", 1);
        touch(&d.join("pack-a.mtimes"), b"", 1);
        touch(&d.join("pack-a.promisor"), b"", 1);
        // No index: unusable, skipped
        touch(&d.join("pack-c.pack"), &[0; 50], 1);
        // No pack: skipped
        touch(&d.join("pack-d.idx"), b"idx", 1);
        touch(&d.join("multi-pack-index"), b"", 1);

        let packs = discover_packs(d).unwrap();

        assert_eq!(packs.len(), 2);
        let a = &packs[0];
        assert_eq!(a.pack.index, 0);
        assert_eq!(a.pack.name, "pack-a.pack");
        assert_eq!(a.pack.size, 100);
        assert_eq!(a.pack.mtime, 1_700_000_100);
        assert!(!a.pack.keep);
        assert!(a.pack.cruft);
        assert!(a.pack.promisor);
        assert_eq!(a.pack_path, d.join("pack-a.pack"));
        assert_eq!(a.idx_path, d.join("pack-a.idx"));

        let b = &packs[1];
        assert_eq!(b.pack.index, 1);
        assert_eq!(b.pack.name, "pack-b.pack");
        assert_eq!(b.pack.size, 300);
        assert_eq!(b.pack.mtime, 1_700_000_200);
        assert!(b.pack.keep);
        assert!(!b.pack.cruft);
        assert!(!b.pack.promisor);
    }

    #[test]
    fn discover_packs_with_no_pack_dir_finds_nothing() {
        let dir = tempfile::tempdir().unwrap();
        assert!(discover_packs(&dir.path().join("missing"))
            .unwrap()
            .is_empty());
    }

    fn pack(index: usize, name: &str, mtime: i64) -> Pack {
        Pack::new(index, name.to_string(), 1000, mtime)
    }

    fn packed(pack: usize) -> ObjectCopy {
        ObjectCopy::packed(ObjectId::Blob(0), pack, 12, 8, false)
    }

    fn loose() -> ObjectCopy {
        ObjectCopy::loose(ObjectId::Blob(0), 8)
    }

    fn found(
        hash: u8,
        kind: ObjectKind,
        pack: Option<usize>,
        offset: u64,
        size_disk: u64,
    ) -> ScannedCopy {
        ScannedCopy {
            hash: [hash; 20],
            kind,
            size: 100,
            pack,
            offset,
            size_disk,
            is_delta: pack.is_some() && offset % 2 == 1,
            mtime: 50,
        }
    }

    #[test]
    fn record_assigns_indices_per_kind_in_first_seen_order() {
        let packs = vec![pack(0, "pack-a.pack", 100)];
        let mut scan = Scan::new(packs, 0);
        scan.record(found(1, ObjectKind::Blob, Some(0), 12, 5));
        scan.record(found(2, ObjectKind::Commit, Some(0), 17, 5));
        scan.record(found(3, ObjectKind::Blob, Some(0), 22, 5));
        scan.record(found(4, ObjectKind::Tree, Some(0), 27, 5));
        scan.record(found(5, ObjectKind::Tag, Some(0), 32, 5));

        assert_eq!(scan.index.get(&[1; 20]), Some(ObjectId::Blob(0)));
        assert_eq!(scan.index.get(&[3; 20]), Some(ObjectId::Blob(1)));
        assert_eq!(scan.index.get(&[2; 20]), Some(ObjectId::Commit(0)));
        assert_eq!(scan.index.get(&[4; 20]), Some(ObjectId::Tree(0)));
        assert_eq!(scan.index.get(&[5; 20]), Some(ObjectId::Tag(0)));

        let c = &scan.container;
        assert_eq!(
            (c.commits.len(), c.trees.len(), c.blobs.len(), c.tags.len()),
            (1, 1, 2, 1)
        );
        assert_eq!(c.blobs[1].hash, [3; 20]);
        assert_eq!(c.blobs[1].index, 1);
        assert_eq!((c.blobs[1].size, c.blobs[1].size_disk), (100, 5));
        assert_eq!(c.copies.len(), 5);
        assert_eq!(c.copies[3].object(), ObjectId::Tree(0));
        assert_eq!(c.copies[3].offset(), Some(27));
        assert!(c.copies[3].is_delta);
    }

    #[test]
    fn record_keeps_every_copy_and_picks_the_primary_by_the_rule() {
        // pack-b is newest; pack-a older; plus a loose copy
        let packs = vec![pack(0, "pack-a.pack", 100), pack(1, "pack-b.pack", 200)];
        let mut scan = Scan::new(packs, 0);
        scan.record(found(1, ObjectKind::Tree, Some(0), 12, 40));
        scan.record(found(1, ObjectKind::Tree, Some(1), 99, 30));
        scan.record(found(1, ObjectKind::Tree, None, 0, 70));

        assert_eq!(scan.container.trees.len(), 1, "one object, three copies");
        assert_eq!(scan.container.copies.len(), 3);
        assert!(scan
            .container
            .copies
            .iter()
            .all(|c| c.object() == ObjectId::Tree(0)));
        assert_eq!(
            scan.primary(ObjectId::Tree(0)),
            1,
            "copy in the newest pack"
        );
        assert_eq!(scan.container.trees[0].size_disk, 30);
    }

    #[test]
    fn record_keeps_the_index_when_a_better_copy_arrives_later() {
        let packs = vec![pack(0, "pack-old.pack", 100), pack(1, "pack-new.pack", 200)];
        let mut scan = Scan::new(packs, 0);
        scan.record(found(1, ObjectKind::Blob, Some(0), 12, 5));
        scan.record(found(2, ObjectKind::Blob, Some(0), 20, 5));
        scan.record(found(2, ObjectKind::Blob, Some(1), 12, 9));
        scan.record(found(1, ObjectKind::Blob, Some(1), 30, 9));

        assert_eq!(scan.index.get(&[1; 20]), Some(ObjectId::Blob(0)));
        assert_eq!(scan.index.get(&[2; 20]), Some(ObjectId::Blob(1)));
        assert_eq!(scan.primary(ObjectId::Blob(0)), 3);
        assert_eq!(scan.primary(ObjectId::Blob(1)), 2);
        assert_eq!(scan.container.blobs[0].size_disk, 9);
    }

    #[test]
    fn finish_orders_each_kind_by_hash_and_remaps_references() {
        let packs = vec![pack(0, "pack-a.pack", 100), pack(1, "pack-b.pack", 200)];
        let mut scan = Scan::new(packs, 0);
        // Seen in reverse hash order within each kind
        scan.record(found(9, ObjectKind::Blob, Some(0), 12, 5));
        scan.record(found(4, ObjectKind::Blob, Some(0), 17, 6));
        scan.record(found(7, ObjectKind::Tree, Some(0), 22, 7));
        scan.record(found(2, ObjectKind::Tree, Some(0), 27, 8));
        // A better copy of blob 9, in the newer pack
        scan.record(found(9, ObjectKind::Blob, Some(1), 12, 3));

        scan.finish();

        let c = &scan.container;
        let blob_hashes: Vec<u8> = c.blobs.iter().map(|b| b.hash[0]).collect();
        let tree_hashes: Vec<u8> = c.trees.iter().map(|t| t.hash[0]).collect();
        assert_eq!(blob_hashes, vec![4, 9]);
        assert_eq!(tree_hashes, vec![2, 7]);
        assert!(c.blobs.iter().enumerate().all(|(i, b)| b.index == i));
        assert!(c.trees.iter().enumerate().all(|(i, t)| t.index == i));
        assert_eq!(scan.index.get(&[9; 20]), Some(ObjectId::Blob(1)));
        assert_eq!(scan.index.get(&[4; 20]), Some(ObjectId::Blob(0)));
        assert_eq!(scan.index.get(&[2; 20]), Some(ObjectId::Tree(0)));

        // Copies keep their recorded order but point at the new indices
        let objects: Vec<ObjectId> = c.copies.iter().map(|c| c.object()).collect();
        assert_eq!(
            objects,
            vec![
                ObjectId::Blob(1),
                ObjectId::Blob(0),
                ObjectId::Tree(1),
                ObjectId::Tree(0),
                ObjectId::Blob(1),
            ]
        );
        assert_eq!(scan.primary(ObjectId::Blob(1)), 4);
        assert_eq!(scan.primary(ObjectId::Blob(0)), 1);
        assert_eq!(c.blobs[1].size_disk, 3);
    }

    #[test]
    fn finish_marks_exactly_the_primary_copies() {
        let packs = vec![pack(0, "pack-a.pack", 100), pack(1, "pack-b.pack", 200)];
        let mut scan = Scan::new(packs, 0);
        scan.record(found(1, ObjectKind::Blob, Some(0), 12, 5));
        scan.record(found(1, ObjectKind::Blob, Some(1), 12, 5));
        scan.record(found(2, ObjectKind::Blob, None, 0, 9));

        scan.finish();

        let flags: Vec<bool> = scan.container.copies.iter().map(|c| c.is_primary).collect();
        assert_eq!(flags, vec![false, true, true]);
    }

    #[test]
    fn packed_copy_beats_loose_copy() {
        let packs = vec![pack(0, "pack-a.pack", 100)];
        assert!(better_copy(&packed(0), &loose(), &packs));
        assert!(!better_copy(&loose(), &packed(0), &packs));
    }

    #[test]
    fn healthy_pack_beats_cruft_or_promisor_pack_even_when_older() {
        let mut cruft = pack(0, "pack-a.pack", 900);
        cruft.cruft = true;
        let mut promisor = pack(1, "pack-b.pack", 900);
        promisor.promisor = true;
        let healthy = pack(2, "pack-c.pack", 100);
        let packs = vec![cruft, promisor, healthy];

        assert!(better_copy(&packed(2), &packed(0), &packs));
        assert!(better_copy(&packed(2), &packed(1), &packs));
        assert!(!better_copy(&packed(0), &packed(2), &packs));
        assert!(!better_copy(&packed(1), &packed(2), &packs));
    }

    #[test]
    fn newer_pack_beats_older_pack() {
        let packs = vec![pack(0, "pack-a.pack", 100), pack(1, "pack-b.pack", 200)];
        assert!(better_copy(&packed(1), &packed(0), &packs));
        assert!(!better_copy(&packed(0), &packed(1), &packs));
    }

    #[test]
    fn same_mtime_falls_back_to_name_order() {
        let packs = vec![pack(0, "pack-b.pack", 100), pack(1, "pack-a.pack", 100)];
        assert!(better_copy(&packed(1), &packed(0), &packs));
        assert!(!better_copy(&packed(0), &packed(1), &packs));
    }

    #[test]
    fn a_copy_is_not_better_than_an_equal_copy() {
        let packs = vec![pack(0, "pack-a.pack", 100)];
        assert!(!better_copy(&packed(0), &packed(0), &packs));
        assert!(!better_copy(&loose(), &loose(), &packs));
    }
}
