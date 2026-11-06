use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;

use anyhow::{anyhow, Result};
use flate2::{Decompress, FlushDecompress, Status};

use hashbrown::HashMap;

use super::delta_cache::{request_times, DeltaCache, DeltaCacheStats, RequestTimes};
use crate::error::ParseError;
use crate::models::container::ObjectHash;
use crate::models::{ObjectCopy, ObjectKind};

/// Object type constants from Git pack format. The four whole-object codes
/// are mapped by `ObjectKind::from_pack_type`; they are kept here, where the
/// format is documented, and used by the pack-building test helpers.
#[allow(dead_code)]
const OBJ_COMMIT: u8 = 1;
#[allow(dead_code)]
const OBJ_TREE: u8 = 2;
#[allow(dead_code)]
const OBJ_BLOB: u8 = 3;
#[allow(dead_code)]
const OBJ_TAG: u8 = 4;
const OBJ_OFS_DELTA: u8 = 6;
const OBJ_REF_DELTA: u8 = 7;

/// Maximum delta chain depth to prevent infinite recursion
pub(super) const MAX_DELTA_DEPTH: usize = 64;

/// Verify the pack file header
pub fn verify_pack_header(data: &[u8]) -> Result<()> {
    if data.len() < 12 {
        return Err(anyhow!("Pack file too small"));
    }

    let signature: [u8; 4] = data[0..4].try_into()?;
    if &signature != b"PACK" {
        return Err(ParseError::InvalidPackHeader(signature).into());
    }

    let version = u32::from_be_bytes(data[4..8].try_into()?);
    if version != 2 && version != 3 {
        return Err(ParseError::UnsupportedPackVersion(version).into());
    }

    Ok(())
}

/// Inflates the entries of one pack that pass 2 reads, by position: all the
/// reads of a pack, or one work unit of them. A delta is applied to its
/// base, which comes from the cache when it is there and is otherwise built
/// from the pack. A built entry goes into the cache if it has a request
/// still to come. Only the first build of a delta requests its base, as
/// `count_requests` counts; a later build, after an eviction, fetches the
/// base without a request, so the counts stay correct.
pub struct PackReader<'a> {
    data: &'a [u8],
    index: &'a PackIndex,
    graph: &'a DeltaGraph,
    /// The part of `container.copies` for this pack
    copies: &'a [ObjectCopy],
    /// Each position that the reads request
    slots: HashMap<u32, Slot>,
    /// The time of each request: see `request_times`
    times: Vec<u32>,
    cache: DeltaCache,
    /// The result of a read that is not cached
    out: Vec<u8>,
    /// One decoder for all entries, reset for each
    inflater: Decompress,
}

/// The requests of one position still to come
struct Slot {
    /// The times of its requests end at `times[end]`
    end: u32,
    /// Requests still to come
    left: u32,
    /// True once the position has been built one time
    built: bool,
}

/// The bytes of an entry
enum Bytes {
    /// In the cache, at this position
    Cached(usize),
    /// Not in the cache
    Owned(Vec<u8>),
}

impl<'a> PackReader<'a> {
    /// A reader for `reads` of a pack, which must then be read in that
    /// order. `copies` is the part of `container.copies` for the pack, so
    /// the copy at `copies[i]` is the entry at position `i`, and `limit` is
    /// the byte limit of the delta cache.
    pub fn new(
        data: &'a [u8],
        index: &'a PackIndex,
        graph: &'a DeltaGraph,
        copies: &'a [ObjectCopy],
        reads: impl Iterator<Item = usize> + Clone,
        limit: u64,
    ) -> Self {
        let RequestTimes { requests, times } = request_times(graph, reads);
        let slots = requests
            .into_iter()
            .map(|(c, r)| {
                let slot = Slot {
                    end: r.end,
                    left: r.count,
                    built: false,
                };
                (c, slot)
            })
            .collect();
        Self {
            data,
            index,
            graph,
            copies,
            slots,
            times,
            cache: DeltaCache::new(limit),
            out: Vec::new(),
            inflater: Decompress::new(true),
        }
    }

    /// The inflated, delta-resolved content of the entry at position `pos`
    pub fn read(&mut self, pos: usize) -> Result<&[u8]> {
        Ok(match self.request(pos, 0)? {
            Bytes::Cached(p) => self.cache.get(p as u32).unwrap(),
            Bytes::Owned(bytes) => {
                self.out = bytes;
                &self.out
            }
        })
    }

    /// Counts kept by the delta cache, with `left` set to the entries in it
    pub fn stats(&self) -> DeltaCacheStats {
        self.cache.stats
    }

    /// One request for the entry at `c`. The steps are in the same order as
    /// in `simulate_cache`, so that with no eviction the cache peaks are
    /// equal to the simulated peaks.
    fn request(&mut self, c: usize, depth: usize) -> Result<Bytes> {
        if self.cache.get(c as u32).is_some() {
            self.cache.stats.hits += 1;
            let slot = self.slot(c);
            debug_assert!(slot.left > 0, "count_requests counted too few");
            slot.left -= 1;
            if slot.left == 0 {
                // The last request: the entry leaves the cache without a copy
                return Ok(Bytes::Owned(self.cache.take(c as u32).unwrap()));
            }
            let next = self.next(c);
            self.cache.set_next(c as u32, next);
            return Ok(Bytes::Cached(c));
        }
        let first = !self.slot(c).built;
        if !first {
            // Evicted, bypassed or too large: built again
            self.cache.stats.misses += 1;
        }
        let bytes = self.build(c, first, depth)?;
        let slot = self.slot(c);
        slot.built = true;
        debug_assert!(slot.left > 0, "count_requests counted too few");
        slot.left -= 1;
        Ok(self.keep(c, bytes))
    }

    /// The bytes of `c` for a second build of a delta on it. Not a counted
    /// request: only the first build of a delta requests its base.
    fn fetch(&mut self, c: usize, depth: usize) -> Result<Bytes> {
        if self.cache.get(c as u32).is_some() {
            self.cache.stats.hits += 1;
            return Ok(Bytes::Cached(c));
        }
        self.cache.stats.misses += 1;
        let bytes = self.build(c, false, depth)?;
        Ok(self.keep(c, bytes))
    }

    /// Cache the bytes of `c` if it has a request still to come
    fn keep(&mut self, c: usize, bytes: Vec<u8>) -> Bytes {
        if self.slot(c).left > 0 {
            let next = self.next(c);
            match self.cache.insert(c as u32, bytes, next) {
                Ok(()) => Bytes::Cached(c),
                Err(bytes) => Bytes::Owned(bytes),
            }
        } else {
            Bytes::Owned(bytes)
        }
    }

    /// The requests of `c`, a position that the reads request
    fn slot(&mut self, c: usize) -> &mut Slot {
        self.slots
            .get_mut(&(c as u32))
            .expect("the reads request this position")
    }

    /// Time of the next request of `c`. Only when a request is still to come.
    fn next(&mut self, c: usize) -> u32 {
        let slot = self.slot(c);
        let at = slot.end - slot.left;
        self.times[at as usize]
    }

    /// Inflate the entry at `c` and, for a delta, apply it to its base. The
    /// base is requested for a first build and fetched for a later one.
    fn build(&mut self, c: usize, first: bool, depth: usize) -> Result<Vec<u8>> {
        if depth > MAX_DELTA_DEPTH {
            return Err(anyhow!(
                "Delta chain too deep (>{}) at offset {}",
                MAX_DELTA_DEPTH,
                self.offset(c)
            ));
        }
        match self.graph.base(c) {
            None => {
                let (start, size) = self.body_start(c)?;
                inflate(&mut self.inflater, &self.data[start..], size)
            }
            Some(b) => {
                let (start, size) = self.delta_start(c)?;
                let delta = inflate(&mut self.inflater, &self.data[start..], size)?;
                let base = if first {
                    self.request(b, depth + 1)?
                } else {
                    self.fetch(b, depth + 1)?
                };
                // Used before the next insert, which could evict it
                let base: &[u8] = match &base {
                    Bytes::Cached(p) => self.cache.get(*p as u32).unwrap(),
                    Bytes::Owned(bytes) => bytes,
                };
                apply_delta(base, &delta)
            }
        }
    }

    fn offset(&self, pos: usize) -> usize {
        self.copies[pos].offset().expect("pack copy has an offset") as usize
    }

    /// Start of the zlib stream of a whole entry, and the object size from
    /// its header
    fn body_start(&self, pos: usize) -> Result<(usize, usize)> {
        let offset = self.offset(pos);
        let (_, size, header_len) = parse_object_header(&self.data[offset..])?;
        Ok((offset + header_len, size as usize))
    }

    /// Start of the zlib stream of a delta entry, after its base reference,
    /// and the size of the delta data from its header
    fn delta_start(&self, pos: usize) -> Result<(usize, usize)> {
        let offset = self.offset(pos);
        let (obj_type, size, header_len) = parse_object_header(&self.data[offset..])?;
        let (base_offset, payload_start) =
            delta_base(self.data, self.index, offset, obj_type, offset + header_len)?;
        debug_assert_eq!(
            base_offset,
            self.offset(self.graph.base(pos).unwrap()),
            "the graph and the entry agree on the base"
        );
        Ok((payload_start, size as usize))
    }
}

/// One pack entry as found by reading headers only, without inflating bodies
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScannedEntry {
    pub hash: ObjectHash,
    /// Byte offset of the entry in the pack
    pub offset: u64,
    /// Bytes the entry occupies on disk, up to the next entry or the trailer
    pub size_disk: u64,
    /// Kind of the object; for a delta, the kind at the end of its base chain
    pub kind: ObjectKind,
    /// Inflated size of the object; for a delta, the size the delta produces
    pub size: u64,
    pub is_delta: bool,
}

/// The base of each entry of one pack, by position
#[derive(Debug, Default)]
pub struct DeltaGraph {
    /// Position of each entry's base; `NO_BASE` for an entry that is not a delta
    base: Vec<u32>,
}

impl DeltaGraph {
    pub const NO_BASE: u32 = u32::MAX;

    #[cfg(test)]
    pub(super) fn from_bases(base: Vec<u32>) -> Self {
        Self { base }
    }

    /// Number of entries
    pub fn len(&self) -> usize {
        self.base.len()
    }

    /// Position of the base of the entry at `pos`, or None if the entry is not a delta
    pub fn base(&self, pos: usize) -> Option<usize> {
        let base = self.base[pos];
        (base != Self::NO_BASE).then_some(base as usize)
    }
}

/// Read every entry's header in offset order, so the pack is touched
/// sequentially from front to back. Nothing is inflated except the first
/// few bytes of each delta, which hold the result size.
///
/// Scanning in offset order also means an OFS delta's base has always been
/// seen already, so its kind is a lookup rather than a walk up the chain.
/// The returned graph gives the position of each delta's base.
pub fn scan_pack(pack_data: &[u8], index: &PackIndex) -> Result<(Vec<ScannedEntry>, DeltaGraph)> {
    if pack_data.len() < 12 + 20 {
        return Err(anyhow!("Pack file too small"));
    }
    let pack_end = (pack_data.len() - 20) as u64;

    let mut order: Vec<usize> = (0..index.entries.len()).collect();
    order.sort_unstable_by_key(|&i| index.entries[i].offset);
    let offsets: Vec<u64> = order.iter().map(|&i| index.entries[i].offset).collect();

    // Kind of each entry in `order`, filled as the scan proceeds
    let mut kinds: Vec<Option<ObjectKind>> = vec![None; order.len()];
    let mut entries = Vec::with_capacity(order.len());
    let mut base = Vec::with_capacity(order.len());
    let mut inflater = Decompress::new(true);

    for (pos, &i) in order.iter().enumerate() {
        let entry = &index.entries[i];
        let offset = entry.offset as usize;
        let (obj_type, obj_size, header_len) = parse_object_header(&pack_data[offset..])?;
        let data_start = offset + header_len;
        let size_disk = offsets.get(pos + 1).copied().unwrap_or(pack_end) - entry.offset;

        let (kind, size, is_delta) = match obj_type {
            OBJ_OFS_DELTA | OBJ_REF_DELTA => {
                let (base_offset, payload_start) =
                    delta_base(pack_data, index, offset, obj_type, data_start)?;
                // A base offset that is not the start of an entry means the
                // pack is damaged
                let p = offsets
                    .binary_search(&(base_offset as u64))
                    .map_err(|_| ParseError::DeltaBaseNotFound(base_offset as u64))?;
                let kind = match kinds[p] {
                    Some(kind) => kind,
                    // A REF delta may point forward to a base not yet scanned
                    None => pack_kind(resolved_type(pack_data, index, base_offset)?)?,
                };
                base.push(p as u32);
                (
                    kind,
                    delta_result_size(&mut inflater, &pack_data[payload_start..])?,
                    true,
                )
            }
            _ => {
                base.push(DeltaGraph::NO_BASE);
                (pack_kind(obj_type)?, obj_size, false)
            }
        };

        kinds[pos] = Some(kind);
        entries.push(ScannedEntry {
            hash: entry.hash,
            offset: entry.offset,
            size_disk,
            kind,
            size,
            is_delta,
        });
    }

    Ok((entries, DeltaGraph { base }))
}

/// Kind for a non-delta pack type code
fn pack_kind(obj_type: u8) -> Result<ObjectKind> {
    ObjectKind::from_pack_type(obj_type)
        .ok_or_else(|| ParseError::InvalidObjectType(obj_type).into())
}

/// Locate a delta's base object and the start of its zlib-compressed payload
fn delta_base(
    pack_data: &[u8],
    index: &PackIndex,
    offset: usize,
    obj_type: u8,
    data_start: usize,
) -> Result<(usize, usize)> {
    if obj_type == OBJ_OFS_DELTA {
        let (relative, header_len) = parse_ofs_delta_header(&pack_data[data_start..])?;
        Ok((offset - relative, data_start + header_len))
    } else {
        let base_hash: ObjectHash = pack_data[data_start..data_start + 20].try_into()?;
        let base_offset = index
            .offset_of(&base_hash)
            .ok_or_else(|| ParseError::ObjectNotFound(hex::encode(base_hash)))?;
        Ok((base_offset as usize, data_start + 20))
    }
}

/// Type of the object at the end of a delta chain, found by following base
/// pointers through object headers without inflating anything
fn resolved_type(pack_data: &[u8], index: &PackIndex, mut offset: usize) -> Result<u8> {
    for _ in 0..=MAX_DELTA_DEPTH {
        let (obj_type, _, header_len) = parse_object_header(&pack_data[offset..])?;
        if obj_type != OBJ_OFS_DELTA && obj_type != OBJ_REF_DELTA {
            return Ok(obj_type);
        }
        (offset, _) = delta_base(pack_data, index, offset, obj_type, offset + header_len)?;
    }
    Err(anyhow!(
        "Delta chain too deep (>{}) at offset {}",
        MAX_DELTA_DEPTH,
        offset
    ))
}

/// Size of the object a delta produces: the second varint of the delta,
/// after the base size. Only the first few bytes are inflated, with a
/// decoder that is reset and reused for every delta.
fn delta_result_size(inflater: &mut Decompress, compressed: &[u8]) -> Result<u64> {
    // Two varints of at most 10 bytes each
    let mut prefix = [0u8; 20];
    inflater.reset(true);
    inflater
        .decompress(compressed, &mut prefix, FlushDecompress::None)
        .map_err(|e| ParseError::DecompressionError(e.to_string()))?;
    let len = inflater.total_out() as usize;

    let (_, base_len) = read_varint(&prefix[..len])?;
    let (size, _) = read_varint(&prefix[base_len..len])?;
    Ok(size)
}

/// Parse the variable-length object header
fn parse_object_header(data: &[u8]) -> Result<(u8, u64, usize)> {
    let mut pos = 0;
    let mut byte = data[pos];
    pos += 1;

    let obj_type = (byte >> 4) & 0x07;
    let mut size = (byte & 0x0f) as u64;
    let mut shift = 4;

    while byte & 0x80 != 0 {
        byte = data[pos];
        pos += 1;
        size |= ((byte & 0x7f) as u64) << shift;
        shift += 7;
    }

    Ok((obj_type, size, pos))
}

/// Parse the offset for OFS_DELTA objects
fn parse_ofs_delta_header(data: &[u8]) -> Result<(usize, usize)> {
    let mut pos = 0;
    let mut byte = data[pos];
    pos += 1;

    let mut offset = (byte & 0x7f) as usize;

    while byte & 0x80 != 0 {
        byte = data[pos];
        pos += 1;
        offset = ((offset + 1) << 7) | ((byte & 0x7f) as usize);
    }

    Ok((offset, pos))
}

/// Inflate a zlib stream of known inflated size into one allocation of
/// that size. `inflater` is reset and reused for each object. The stream
/// must end and give exactly `size` bytes. `data` can go on after the
/// stream, as in a pack, where the next entry follows.
fn inflate(inflater: &mut Decompress, data: &[u8], size: usize) -> Result<Vec<u8>> {
    inflater.reset(true);
    let mut out = Vec::with_capacity(size);
    // Writes only into the spare capacity, so `out` is never reallocated
    let status = inflater
        .decompress_vec(data, &mut out, FlushDecompress::Finish)
        .map_err(|e| ParseError::DecompressionError(e.to_string()))?;
    if status != Status::StreamEnd {
        return Err(ParseError::DecompressionError(format!(
            "zlib stream did not end within the expected {size} bytes"
        ))
        .into());
    }
    if out.len() != size {
        return Err(ParseError::DecompressionError(format!(
            "zlib stream gave {} bytes, expected {size}",
            out.len()
        ))
        .into());
    }
    Ok(out)
}

/// Apply a delta to a base object
fn apply_delta(base: &[u8], delta: &[u8]) -> Result<Vec<u8>> {
    let mut pos = 0;

    // Read base size (variable length)
    let (_base_size, len) = read_varint(&delta[pos..])?;
    pos += len;

    // Read result size (variable length)
    let (result_size, len) = read_varint(&delta[pos..])?;
    pos += len;

    let mut result = Vec::with_capacity(result_size as usize);

    while pos < delta.len() {
        let cmd = delta[pos];
        pos += 1;

        if cmd & 0x80 != 0 {
            // Copy instruction
            let mut copy_offset = 0usize;
            let mut copy_size = 0usize;

            if cmd & 0x01 != 0 {
                copy_offset |= delta[pos] as usize;
                pos += 1;
            }
            if cmd & 0x02 != 0 {
                copy_offset |= (delta[pos] as usize) << 8;
                pos += 1;
            }
            if cmd & 0x04 != 0 {
                copy_offset |= (delta[pos] as usize) << 16;
                pos += 1;
            }
            if cmd & 0x08 != 0 {
                copy_offset |= (delta[pos] as usize) << 24;
                pos += 1;
            }

            if cmd & 0x10 != 0 {
                copy_size |= delta[pos] as usize;
                pos += 1;
            }
            if cmd & 0x20 != 0 {
                copy_size |= (delta[pos] as usize) << 8;
                pos += 1;
            }
            if cmd & 0x40 != 0 {
                copy_size |= (delta[pos] as usize) << 16;
                pos += 1;
            }

            if copy_size == 0 {
                copy_size = 0x10000;
            }

            if copy_offset + copy_size > base.len() {
                return Err(anyhow!(
                    "Delta copy out of bounds: offset={} size={} base_len={}",
                    copy_offset,
                    copy_size,
                    base.len()
                ));
            }

            result.extend_from_slice(&base[copy_offset..copy_offset + copy_size]);
        } else if cmd != 0 {
            // Insert instruction
            let insert_size = cmd as usize;
            if pos + insert_size > delta.len() {
                return Err(anyhow!(
                    "Delta insert out of bounds: pos={} size={} delta_len={}",
                    pos,
                    insert_size,
                    delta.len()
                ));
            }
            result.extend_from_slice(&delta[pos..pos + insert_size]);
            pos += insert_size;
        } else {
            return Err(ParseError::InvalidDeltaInstruction.into());
        }
    }

    Ok(result)
}

/// Read a variable-length integer
fn read_varint(data: &[u8]) -> Result<(u64, usize)> {
    let mut pos = 0;
    let mut value = 0u64;
    let mut shift = 0;

    loop {
        if pos >= data.len() {
            return Err(anyhow!("Varint extends past end of data"));
        }
        let byte = data[pos];
        pos += 1;
        value |= ((byte & 0x7f) as u64) << shift;
        if byte & 0x80 == 0 {
            break;
        }
        shift += 7;
    }

    Ok((value, pos))
}

/// Pack index structure. Entries are in hash order, as the `.idx` format
/// stores them, so hashes can be looked up by binary search.
pub struct PackIndex {
    pub entries: Vec<PackIndexEntry>,
}

impl PackIndex {
    /// Offset of the entry with this hash, if the pack has one
    fn offset_of(&self, hash: &ObjectHash) -> Option<u64> {
        self.entries
            .binary_search_by(|e| e.hash.cmp(hash))
            .ok()
            .map(|i| self.entries[i].offset)
    }
}

/// Single entry in a pack index
pub struct PackIndexEntry {
    pub hash: ObjectHash,
    pub offset: u64,
    #[allow(dead_code)]
    pub crc32: u32,
}

impl PackIndex {
    /// Parse a pack index file (version 2 format)
    pub fn parse(path: &Path) -> Result<Self> {
        let file = File::open(path)?;
        let mut reader = BufReader::new(file);
        let mut data = Vec::new();
        reader.read_to_end(&mut data)?;

        if data.len() < 8 {
            return Err(anyhow!("Index file too small"));
        }

        let magic = &data[0..4];
        let version = u32::from_be_bytes(data[4..8].try_into()?);

        if magic != b"\xfftOc" || version != 2 {
            return Err(anyhow!(
                "Unsupported index format (magic={:?}, version={})",
                magic,
                version
            ));
        }

        // Parse fanout table (256 entries, 4 bytes each)
        let fanout_end = 8 + 256 * 4;
        let total_objects =
            u32::from_be_bytes(data[fanout_end - 4..fanout_end].try_into()?) as usize;

        // Parse SHA-1 table
        let sha_start = fanout_end;
        let sha_end = sha_start + total_objects * 20;

        // Parse CRC32 table
        let crc_start = sha_end;
        let crc_end = crc_start + total_objects * 4;

        // Parse offset table
        let offset_start = crc_end;
        let offset_end = offset_start + total_objects * 4;

        // Large offset extension
        let large_offset_start = offset_end;

        let mut entries = Vec::with_capacity(total_objects);

        for i in 0..total_objects {
            let hash: ObjectHash = data[sha_start + i * 20..sha_start + (i + 1) * 20].try_into()?;

            let crc32 =
                u32::from_be_bytes(data[crc_start + i * 4..crc_start + (i + 1) * 4].try_into()?);

            let offset_raw = u32::from_be_bytes(
                data[offset_start + i * 4..offset_start + (i + 1) * 4].try_into()?,
            );

            let offset = if offset_raw & 0x80000000 != 0 {
                let large_idx = (offset_raw & 0x7fffffff) as usize;
                u64::from_be_bytes(
                    data[large_offset_start + large_idx * 8
                        ..large_offset_start + (large_idx + 1) * 8]
                        .try_into()?,
                )
            } else {
                offset_raw as u64
            };

            entries.push(PackIndexEntry {
                hash,
                offset,
                crc32,
            });
        }

        Ok(Self { entries })
    }
}

/// Hex encoding for error messages and debug output
mod hex {
    pub fn encode(data: impl AsRef<[u8]>) -> String {
        data.as_ref().iter().map(|b| format!("{:02x}", b)).collect()
    }
}

#[cfg(feature = "bench")]
mod benches {
    use std::io::Write;

    use divan::counter::{BytesCount, ItemsCount};
    use divan::{black_box, Bencher};
    use flate2::write::ZlibEncoder;
    use flate2::{Compression, Decompress};

    use crate::parser::content::benches::synthetic_tree;
    use crate::parser::delta_cache::DeltaCache;

    /// Bytes of one entry in `synthetic_tree`
    const ENTRY_BYTES: usize = 42;
    /// Entry inserted by `edit_delta`
    const NEW_ENTRY: &[u8] = b"100644 new_file.rs\0";

    fn zlib(data: &[u8]) -> Vec<u8> {
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(data).unwrap();
        encoder.finish().unwrap()
    }

    /// Size varint as used at the start of a delta: low 7 bits first
    fn size_varint(mut value: u64, out: &mut Vec<u8>) {
        loop {
            let byte = (value & 0x7f) as u8;
            value >>= 7;
            if value == 0 {
                out.push(byte);
                return;
            }
            out.push(byte | 0x80);
        }
    }

    /// Copy instruction: only the non-zero offset and size bytes are written
    fn copy(out: &mut Vec<u8>, offset: usize, size: usize) {
        let mut cmd = 0x80u8;
        let mut args = Vec::new();
        for i in 0..4 {
            let byte = (offset >> (8 * i)) as u8;
            if byte != 0 {
                cmd |= 1 << i;
                args.push(byte);
            }
        }
        for i in 0..3 {
            let byte = (size >> (8 * i)) as u8;
            if byte != 0 {
                cmd |= 0x10 << i;
                args.push(byte);
            }
        }
        out.push(cmd);
        out.extend_from_slice(&args);
    }

    /// A delta that rebuilds `base` with one new entry in the middle, the
    /// common shape of a tree delta. Returns the delta and the result.
    fn edit_delta(base: &[u8]) -> (Vec<u8>, Vec<u8>) {
        let half = base.len() / 2;
        let mut result = base[..half].to_vec();
        result.extend_from_slice(NEW_ENTRY);
        result.extend_from_slice(&base[half..]);

        let mut delta = Vec::new();
        size_varint(base.len() as u64, &mut delta);
        size_varint(result.len() as u64, &mut delta);
        copy(&mut delta, 0, half);
        delta.push(NEW_ENTRY.len() as u8);
        delta.extend_from_slice(NEW_ENTRY);
        copy(&mut delta, half, base.len() - half);
        (delta, result)
    }

    /// Pass 1 reads the result size of every delta: 1,000 compressed deltas
    /// against bases of 20 to 219 entries.
    #[divan::bench]
    fn delta_result_size_1000(bencher: Bencher) {
        let deltas: Vec<(Vec<u8>, u64)> = (0..1000)
            .map(|i| {
                let (delta, result) = edit_delta(&synthetic_tree(20 + i % 200));
                (zlib(&delta), result.len() as u64)
            })
            .collect();
        // One decoder for the whole scan, as `scan_pack` uses it
        let mut inflater = Decompress::new(true);
        for (compressed, size) in &deltas {
            assert_eq!(
                super::delta_result_size(&mut inflater, compressed).unwrap(),
                *size
            );
        }
        bencher
            .counter(ItemsCount::new(deltas.len()))
            .bench_local(|| {
                for (compressed, _) in &deltas {
                    black_box(
                        super::delta_result_size(&mut inflater, black_box(compressed)).unwrap(),
                    );
                }
            });
    }

    /// Pass 2 inflates every tree body
    #[divan::bench(args = [4 * 1024, 64 * 1024, 1024 * 1024])]
    fn decompress_data(bencher: Bencher, bytes: usize) {
        let body = synthetic_tree(bytes / ENTRY_BYTES);
        let compressed = zlib(&body);
        // One decoder for all objects, as each `PackReader` uses it
        let mut inflater = Decompress::new(true);
        assert_eq!(
            super::inflate(&mut inflater, &compressed, body.len()).unwrap(),
            body
        );
        bencher
            .counter(BytesCount::of_slice(&body))
            .bench_local(|| {
                super::inflate(&mut inflater, black_box(&compressed), body.len()).unwrap()
            });
    }

    /// Pass 2 rebuilds every deltified tree from its base
    #[divan::bench(args = [4 * 1024, 64 * 1024, 1024 * 1024])]
    fn apply_delta(bencher: Bencher, bytes: usize) {
        let base = synthetic_tree(bytes / ENTRY_BYTES);
        let (delta, result) = edit_delta(&base);
        assert_eq!(super::apply_delta(&base, &delta).unwrap(), result);
        bencher
            .counter(BytesCount::of_slice(&result))
            .bench_local(|| super::apply_delta(black_box(&base), black_box(&delta)).unwrap());
    }

    /// Pass 2 gets a cached base for each delta built on it. A hit must
    /// make 0 allocations.
    #[divan::bench]
    fn delta_cache_hit(bencher: Bencher) {
        let mut cache = DeltaCache::new(1 << 30);
        cache
            .insert(0, synthetic_tree(64 * 1024 / ENTRY_BYTES), 1)
            .unwrap();
        bencher.bench_local(|| black_box(cache.get(black_box(0)).unwrap().len()));
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use flate2::write::ZlibEncoder;
    use flate2::Compression;

    use super::*;
    use crate::models::{ObjectId, ObjectKind};
    use crate::parser::delta_cache::pack_reads;

    fn zlib(data: &[u8]) -> Vec<u8> {
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(data).unwrap();
        encoder.finish().unwrap()
    }

    /// `<type><size>` object header: 3-bit type and 4 bits of size in the
    /// first byte, 7 bits of size per continuation byte
    fn obj_header(kind: u8, mut size: u64) -> Vec<u8> {
        let mut out = Vec::new();
        let mut byte = (kind << 4) | (size & 0x0f) as u8;
        size >>= 4;
        while size > 0 {
            out.push(byte | 0x80);
            byte = (size & 0x7f) as u8;
            size >>= 7;
        }
        out.push(byte);
        out
    }

    /// OFS_DELTA base distance, big-endian 7-bit groups with git's "+1 per
    /// byte" bias (the inverse of `parse_ofs_delta_header`)
    fn ofs_encode(mut distance: u64) -> Vec<u8> {
        let mut buf = [0u8; 10];
        let mut pos = buf.len() - 1;
        buf[pos] = (distance & 0x7f) as u8;
        distance >>= 7;
        while distance > 0 {
            distance -= 1;
            pos -= 1;
            buf[pos] = 0x80 | (distance & 0x7f) as u8;
            distance >>= 7;
        }
        buf[pos..].to_vec()
    }

    fn varint(mut value: u64) -> Vec<u8> {
        let mut out = Vec::new();
        loop {
            let byte = (value & 0x7f) as u8;
            value >>= 7;
            if value == 0 {
                out.push(byte);
                return out;
            }
            out.push(byte | 0x80);
        }
    }

    /// A delta that ignores its base and inserts `result` wholesale
    fn insert_delta(base_size: u64, result: &[u8]) -> Vec<u8> {
        let mut delta = varint(base_size);
        delta.extend(varint(result.len() as u64));
        for chunk in result.chunks(127) {
            delta.push(chunk.len() as u8);
            delta.extend_from_slice(chunk);
        }
        delta
    }

    /// A delta whose result is the base without its first `suffix.len()`
    /// bytes, followed by `suffix`: the same length as the base, and
    /// different for each base, so a wrong base gives a wrong result
    fn shift_delta(base_size: u64, suffix: &[u8]) -> Vec<u8> {
        let shift = suffix.len() as u64;
        assert!(shift > 0 && shift < 256 && base_size > shift && base_size < 0x10000);
        let mut delta = varint(base_size);
        delta.extend(varint(base_size));
        // Copy: one offset byte, two size bytes
        let size = base_size - shift;
        delta.push(0x80 | 0x01 | 0x10 | 0x20);
        delta.push(shift as u8);
        delta.push(size as u8);
        delta.push((size >> 8) as u8);
        for chunk in suffix.chunks(127) {
            delta.push(chunk.len() as u8);
            delta.extend_from_slice(chunk);
        }
        delta
    }

    /// The result of `shift_delta(base.len(), suffix)` applied to `base`
    fn shifted(base: &[u8], suffix: &[u8]) -> Vec<u8> {
        [&base[suffix.len()..], suffix].concat()
    }

    /// Writes a version 2 pack and its index in memory
    struct PackBuilder {
        data: Vec<u8>,
        entries: Vec<PackIndexEntry>,
    }

    impl PackBuilder {
        fn new() -> Self {
            let mut data = b"PACK".to_vec();
            data.extend_from_slice(&2u32.to_be_bytes());
            data.extend_from_slice(&0u32.to_be_bytes()); // count, patched in finish()
            Self {
                data,
                entries: Vec::new(),
            }
        }

        fn push(&mut self, hash: ObjectHash, bytes: &[u8]) -> u64 {
            let offset = self.data.len() as u64;
            self.data.extend_from_slice(bytes);
            self.entries.push(PackIndexEntry {
                hash,
                offset,
                crc32: 0,
            });
            offset
        }

        /// A whole (non-delta) object
        fn add(&mut self, hash: ObjectHash, kind: u8, body: &[u8]) -> u64 {
            let mut bytes = obj_header(kind, body.len() as u64);
            bytes.extend(zlib(body));
            self.push(hash, &bytes)
        }

        /// A whole object whose payload is not valid zlib
        fn add_corrupt(&mut self, hash: ObjectHash, kind: u8, size: u64) -> u64 {
            let mut bytes = obj_header(kind, size);
            bytes.extend_from_slice(b"\xff\xff\xffnot zlib");
            self.push(hash, &bytes)
        }

        fn add_ofs_delta(&mut self, hash: ObjectHash, base_offset: u64, result: &[u8]) -> u64 {
            let delta = insert_delta(1, result);
            let offset = self.data.len() as u64;
            let mut bytes = obj_header(OBJ_OFS_DELTA, delta.len() as u64);
            bytes.extend(ofs_encode(offset - base_offset));
            bytes.extend(zlib(&delta));
            self.push(hash, &bytes)
        }

        /// An OFS delta on the entry at `base_offset`, whose body is
        /// `base`, with the result `shifted(base, suffix)`
        fn add_ofs_shift(
            &mut self,
            hash: ObjectHash,
            base_offset: u64,
            base: &[u8],
            suffix: &[u8],
        ) -> u64 {
            let delta = shift_delta(base.len() as u64, suffix);
            let offset = self.data.len() as u64;
            let mut bytes = obj_header(OBJ_OFS_DELTA, delta.len() as u64);
            bytes.extend(ofs_encode(offset - base_offset));
            bytes.extend(zlib(&delta));
            self.push(hash, &bytes)
        }

        /// A REF delta on the entry with hash `base_hash`, whose body is
        /// `base`, with the result `shifted(base, suffix)`
        fn add_ref_shift(
            &mut self,
            hash: ObjectHash,
            base_hash: ObjectHash,
            base: &[u8],
            suffix: &[u8],
        ) -> u64 {
            let delta = shift_delta(base.len() as u64, suffix);
            let mut bytes = obj_header(OBJ_REF_DELTA, delta.len() as u64);
            bytes.extend_from_slice(&base_hash);
            bytes.extend(zlib(&delta));
            self.push(hash, &bytes)
        }

        fn add_ref_delta(&mut self, hash: ObjectHash, base: ObjectHash, result: &[u8]) -> u64 {
            let delta = insert_delta(1, result);
            let mut bytes = obj_header(OBJ_REF_DELTA, delta.len() as u64);
            bytes.extend_from_slice(&base);
            bytes.extend(zlib(&delta));
            self.push(hash, &bytes)
        }

        /// Pack bytes (with a dummy 20-byte trailer) and a hash-sorted index
        fn finish(mut self) -> (Vec<u8>, PackIndex) {
            let count = self.entries.len() as u32;
            self.data[8..12].copy_from_slice(&count.to_be_bytes());
            self.data.extend_from_slice(&[0u8; 20]);
            self.entries.sort_by_key(|e| e.hash);
            (
                self.data,
                PackIndex {
                    entries: self.entries,
                },
            )
        }

        /// Write the pack and a version 2 `.idx` for it into `pack_dir`
        fn write(self, pack_dir: &Path) {
            let (data, index) = self.finish();
            let mut idx = b"\xfftOc".to_vec();
            idx.extend_from_slice(&2u32.to_be_bytes());
            for byte in 0..=255u8 {
                let count = index.entries.iter().filter(|e| e.hash[0] <= byte).count();
                idx.extend_from_slice(&(count as u32).to_be_bytes());
            }
            for e in &index.entries {
                idx.extend_from_slice(&e.hash);
            }
            for e in &index.entries {
                idx.extend_from_slice(&e.crc32.to_be_bytes());
            }
            for e in &index.entries {
                idx.extend_from_slice(&(e.offset as u32).to_be_bytes());
            }
            std::fs::create_dir_all(pack_dir).unwrap();
            std::fs::write(pack_dir.join("pack-test.pack"), data).unwrap();
            std::fs::write(pack_dir.join("pack-test.idx"), idx).unwrap();
        }
    }

    #[test]
    fn scan_pack_returns_entries_in_offset_order_with_disk_sizes() {
        let mut pack = PackBuilder::new();
        // Hash order (1, 5, 9) deliberately differs from offset order (9, 1, 5)
        let o_blob = pack.add([9; 20], OBJ_BLOB, b"hello");
        let o_tree = pack.add([1; 20], OBJ_TREE, b"100644 f\0aaaaaaaaaaaaaaaaaaaa");
        let o_commit = pack.add([5; 20], OBJ_COMMIT, b"tree x\n");
        let (data, index) = pack.finish();
        let pack_end = (data.len() - 20) as u64;

        let (entries, _) = scan_pack(&data, &index).unwrap();

        let offsets: Vec<u64> = entries.iter().map(|e| e.offset).collect();
        assert_eq!(offsets, vec![o_blob, o_tree, o_commit]);
        let hashes: Vec<ObjectHash> = entries.iter().map(|e| e.hash).collect();
        assert_eq!(hashes, vec![[9; 20], [1; 20], [5; 20]]);

        let kinds: Vec<ObjectKind> = entries.iter().map(|e| e.kind).collect();
        assert_eq!(
            kinds,
            vec![ObjectKind::Blob, ObjectKind::Tree, ObjectKind::Commit]
        );
        let sizes: Vec<u64> = entries.iter().map(|e| e.size).collect();
        assert_eq!(sizes, vec![5, 29, 7]);
        assert!(entries.iter().all(|e| !e.is_delta));

        let disk: Vec<u64> = entries.iter().map(|e| e.size_disk).collect();
        assert_eq!(
            disk,
            vec![o_tree - o_blob, o_commit - o_tree, pack_end - o_commit]
        );
    }

    #[test]
    fn scan_pack_resolves_delta_kinds_and_sizes_through_chains() {
        let mut pack = PackBuilder::new();
        let o_base = pack.add([1; 20], OBJ_BLOB, b"base blob");
        // OFS delta on the blob, and an OFS delta on that delta (depth 2)
        let o_d1 = pack.add_ofs_delta([2; 20], o_base, &[b'x'; 300]);
        let o_d2 = pack.add_ofs_delta([3; 20], o_d1, &[b'y'; 40]);
        // REF delta whose base comes later in the pack (forward reference)
        let o_ref = pack.add_ref_delta([4; 20], [6; 20], b"tree later\n");
        let o_commit = pack.add([6; 20], OBJ_COMMIT, b"tree ...\n");
        let (data, index) = pack.finish();

        let (entries, _) = scan_pack(&data, &index).unwrap();

        let by_offset = |o: u64| entries.iter().find(|e| e.offset == o).unwrap();
        let base = by_offset(o_base);
        assert_eq!(
            (base.kind, base.size, base.is_delta),
            (ObjectKind::Blob, 9, false)
        );
        let d1 = by_offset(o_d1);
        assert_eq!(
            (d1.kind, d1.size, d1.is_delta),
            (ObjectKind::Blob, 300, true)
        );
        let d2 = by_offset(o_d2);
        assert_eq!(
            (d2.kind, d2.size, d2.is_delta),
            (ObjectKind::Blob, 40, true)
        );
        let r = by_offset(o_ref);
        assert_eq!((r.kind, r.size, r.is_delta), (ObjectKind::Commit, 11, true));
        let c = by_offset(o_commit);
        assert_eq!((c.kind, c.size, c.is_delta), (ObjectKind::Commit, 9, false));
    }

    #[test]
    fn scan_pack_on_8_threads_equals_scan_pack_on_1_thread() {
        let mut pack = PackBuilder::new();
        // Hashes are out of offset order. With chunks of 2 positions, the
        // bases below are in the same chunk, an earlier one, or a later one.
        let o0 = pack.add([40; 20], OBJ_BLOB, b"base blob");
        let o1 = pack.add_ofs_delta([30; 20], o0, b"on the blob");
        // Base in the earlier chunk
        pack.add_ofs_delta([90; 20], o1, b"depth 2");
        // Base in a later chunk, and that base is itself a delta
        pack.add_ref_delta([10; 20], [60; 20], b"forward ref");
        let o4 = pack.add([80; 20], OBJ_COMMIT, b"tree x\n");
        pack.add_ofs_delta([20; 20], o4, b"on the commit");
        pack.add_ofs_delta([60; 20], o4, b"the base of the forward ref");
        // Base two chunks back
        pack.add_ref_delta([70; 20], [40; 20], b"backward ref");
        let o8 = pack.add([50; 20], OBJ_TREE, b"100644 f\0aaaaaaaaaaaaaaaaaaaa");
        pack.add_ofs_delta([100; 20], o8, b"on the tree");
        let (data, index) = pack.finish();

        let scan_on = |threads| {
            rayon::ThreadPoolBuilder::new()
                .num_threads(threads)
                .build()
                .unwrap()
                .install(|| scan_pack(&data, &index).unwrap())
        };
        let bases = |graph: &DeltaGraph| -> Vec<Option<usize>> {
            (0..graph.len()).map(|pos| graph.base(pos)).collect()
        };

        let (one, one_graph) = scan_on(1);
        let (eight, eight_graph) = scan_on(8);

        assert_eq!(eight, one);
        assert_eq!(bases(&eight_graph), bases(&one_graph));

        // The two scans could agree on a wrong result
        let kinds: Vec<ObjectKind> = one.iter().map(|e| e.kind).collect();
        let (b, c, t) = (ObjectKind::Blob, ObjectKind::Commit, ObjectKind::Tree);
        assert_eq!(kinds, vec![b, b, b, c, c, c, c, b, t, t]);
        assert_eq!(
            bases(&one_graph),
            vec![
                None,
                Some(0),
                Some(1),
                Some(6),
                None,
                Some(4),
                Some(4),
                Some(0),
                None,
                Some(8),
            ]
        );
        let sizes: Vec<u64> = one.iter().map(|e| e.size).collect();
        assert_eq!(sizes, vec![9, 11, 7, 11, 7, 13, 27, 12, 29, 11]);
    }

    #[test]
    fn pack_reader_inflates_whole_objects_and_resolves_delta_chains() {
        let mut pack = PackBuilder::new();
        let o_base = pack.add([1; 20], OBJ_TREE, b"base tree");
        let o_d1 = pack.add_ofs_delta([2; 20], o_base, b"first delta result");
        let o_d2 = pack.add_ofs_delta([3; 20], o_d1, b"second, on the first");
        let o_ref = pack.add_ref_delta([4; 20], [1; 20], b"ref delta result");
        let (data, index) = pack.finish();
        let (entries, graph) = scan_pack(&data, &index).unwrap();
        let copies: Vec<ObjectCopy> = entries
            .iter()
            .enumerate()
            .map(|(pos, e)| {
                let mut copy = ObjectCopy::packed(
                    ObjectId::new(e.kind, pos),
                    0,
                    e.offset,
                    e.size_disk,
                    e.is_delta,
                );
                copy.is_primary = true;
                copy
            })
            .collect();
        let position = |offset| entries.iter().position(|e| e.offset == offset).unwrap();

        let mut reader =
            PackReader::new(&data, &index, &graph, &copies, pack_reads(&copies), 1 << 20);

        assert_eq!(reader.read(position(o_base)).unwrap(), b"base tree");
        assert_eq!(reader.read(position(o_d1)).unwrap(), b"first delta result");
        assert_eq!(
            reader.read(position(o_d2)).unwrap(),
            b"second, on the first"
        );
        assert_eq!(reader.read(position(o_ref)).unwrap(), b"ref delta result");
        // Every base was cached when needed, and none is left after its
        // last request
        let stats = reader.stats();
        assert_eq!(stats.misses, 0);
        assert_eq!(stats.evictions, 0);
        assert_eq!(stats.left, 0);
    }

    /// What a `PackReader` needs besides the pack: the graph from pass 1
    /// and the pack's copies, all trees, with `primary[pos]` saying which
    /// pass 2 reads
    fn reader_inputs(
        data: &[u8],
        index: &PackIndex,
        primary: &[bool],
    ) -> (DeltaGraph, Vec<ObjectCopy>) {
        let (entries, graph) = scan_pack(data, index).unwrap();
        assert_eq!(entries.len(), primary.len());
        let copies = entries
            .iter()
            .zip(primary)
            .enumerate()
            .map(|(pos, (e, &is_primary))| {
                let mut copy = ObjectCopy::packed(
                    ObjectId::new(e.kind, pos),
                    0,
                    e.offset,
                    e.size_disk,
                    e.is_delta,
                );
                copy.is_primary = is_primary;
                copy
            })
            .collect();
        (graph, copies)
    }

    /// Length of every body in the reader tests, so that a cache limit of
    /// `TREE_LEN` holds exactly one entry
    const TREE_LEN: usize = 64;

    fn tree(n: u8) -> Vec<u8> {
        let mut body = format!("tree {n} ").into_bytes();
        body.resize(TREE_LEN, b'.');
        body
    }

    #[test]
    fn pack_reader_evicts_the_base_requested_farthest_away_and_rebuilds_it() {
        // Positions: A whole (not read); B on A; C on B; D on C; E on A.
        // Reads B, C, D, E at times 0..4. A is requested at times 0 and 3,
        // B at 0 and 1, C at 1 and 2.
        let a = tree(0);
        let b = shifted(&a, b"+b");
        let c = shifted(&b, b"+c");
        let d = shifted(&c, b"+d");
        let e = shifted(&a, b"+e");
        let mut pack = PackBuilder::new();
        let o_a = pack.add([1; 20], OBJ_TREE, &a);
        let o_b = pack.add_ofs_shift([2; 20], o_a, &a, b"+b");
        let o_c = pack.add_ofs_shift([3; 20], o_b, &b, b"+c");
        pack.add_ofs_shift([4; 20], o_c, &c, b"+d");
        pack.add_ofs_shift([5; 20], o_a, &a, b"+e");
        let (data, index) = pack.finish();
        let (graph, copies) = reader_inputs(&data, &index, &[false, true, true, true, true]);

        // Room for one entry
        let mut reader = PackReader::new(
            &data,
            &index,
            &graph,
            &copies,
            pack_reads(&copies),
            TREE_LEN as u64,
        );

        // Building B caches A (next 3), then B (next 1) evicts A
        assert_eq!(reader.read(1).unwrap(), b);
        assert_eq!(reader.stats().evictions, 1);
        // C takes B on its last request; D takes C
        assert_eq!(reader.read(2).unwrap(), c);
        assert_eq!(reader.read(3).unwrap(), d);
        assert_eq!(reader.stats().misses, 0);
        // E needs A again: built a second time
        assert_eq!(reader.read(4).unwrap(), e);

        let stats = reader.stats();
        assert_eq!(stats.hits, 2, "B for C, C for D");
        assert_eq!(stats.misses, 1, "A for E");
        assert_eq!(stats.evictions, 1);
        assert_eq!(stats.bypassed, 0);
        assert_eq!(stats.too_large, 0);
        assert_eq!(stats.peak_entries, 1);
        assert_eq!(stats.left, 0);
    }

    #[test]
    fn pack_reader_fetches_the_base_of_a_second_build_without_a_request() {
        // Positions: A whole (not read); B on A; D on A; C on B.
        // Reads B, D, C at times 0..3. A is requested at 0 and 1, B at 0
        // and 2. B is bypassed at time 0 because A (next 1) is needed
        // sooner, so building C rebuilds B, which fetches A.
        let a = tree(0);
        let b = shifted(&a, b"+b");
        let c = shifted(&b, b"+c");
        let d = shifted(&a, b"+d");
        let mut pack = PackBuilder::new();
        let o_a = pack.add([1; 20], OBJ_TREE, &a);
        let o_b = pack.add_ofs_shift([2; 20], o_a, &a, b"+b");
        pack.add_ofs_shift([3; 20], o_a, &a, b"+d");
        pack.add_ofs_shift([4; 20], o_b, &b, b"+c");
        let (data, index) = pack.finish();
        let (graph, copies) = reader_inputs(&data, &index, &[false, true, true, true]);

        let mut reader = PackReader::new(
            &data,
            &index,
            &graph,
            &copies,
            pack_reads(&copies),
            TREE_LEN as u64,
        );

        assert_eq!(reader.read(1).unwrap(), b);
        assert_eq!(reader.stats().bypassed, 1, "B, requested later than A");
        assert_eq!(reader.read(2).unwrap(), d);
        assert_eq!(reader.stats().hits, 1, "A for D, its last request");
        assert_eq!(reader.read(3).unwrap(), c);

        let stats = reader.stats();
        assert_eq!(stats.misses, 2, "B for C, then A for the rebuild of B");
        assert_eq!(stats.hits, 1);
        assert_eq!(stats.evictions, 0);
        assert_eq!(stats.left, 0);
    }

    #[test]
    fn pack_reader_works_with_a_cache_too_small_for_any_entry() {
        let a = tree(0);
        let b = shifted(&a, b"+b");
        let c = shifted(&b, b"+c");
        let mut pack = PackBuilder::new();
        let o_a = pack.add([1; 20], OBJ_TREE, &a);
        let o_b = pack.add_ofs_shift([2; 20], o_a, &a, b"+b");
        pack.add_ofs_shift([3; 20], o_b, &b, b"+c");
        let (data, index) = pack.finish();
        let (graph, copies) = reader_inputs(&data, &index, &[true, true, true]);

        let mut reader = PackReader::new(&data, &index, &graph, &copies, pack_reads(&copies), 1);

        assert_eq!(reader.read(0).unwrap(), a);
        assert_eq!(reader.read(1).unwrap(), b);
        assert_eq!(reader.read(2).unwrap(), c);

        let stats = reader.stats();
        assert_eq!(stats.hits, 0);
        assert_eq!(stats.too_large, 2, "A after its read, B after its read");
        assert_eq!(stats.misses, 3, "A for B; B for C, and A for that rebuild");
        assert_eq!(stats.peak_entries, 0);
        assert_eq!(stats.left, 0);
    }

    #[test]
    fn pack_reader_serves_a_read_from_the_cache_without_a_copy() {
        // A is read and then needed by B: after the read it stays cached
        let a = tree(0);
        let b = shifted(&a, b"+b");
        let mut pack = PackBuilder::new();
        let o_a = pack.add([1; 20], OBJ_TREE, &a);
        pack.add_ofs_shift([2; 20], o_a, &a, b"+b");
        let (data, index) = pack.finish();
        let (graph, copies) = reader_inputs(&data, &index, &[true, true]);

        let mut reader =
            PackReader::new(&data, &index, &graph, &copies, pack_reads(&copies), 1 << 20);

        assert_eq!(reader.read(0).unwrap(), a);
        assert_eq!(reader.stats().left, 1, "A waits for B");
        assert_eq!(reader.stats().peak_bytes, TREE_LEN as u64);
        assert_eq!(reader.read(1).unwrap(), b);
        assert_eq!(reader.stats().left, 0);
        assert_eq!(reader.stats().hits, 1);
    }

    #[test]
    fn pack_reader_rejects_a_delta_chain_that_is_too_deep() {
        let mut pack = PackBuilder::new();
        let mut offset = pack.add([0; 20], OBJ_TREE, b"root");
        let depth = MAX_DELTA_DEPTH + 1;
        for i in 1..=depth {
            let mut hash = [0u8; 20];
            hash[..8].copy_from_slice(&(i as u64).to_be_bytes());
            offset = pack.add_ofs_delta(hash, offset, b"next");
        }
        let (data, index) = pack.finish();
        let primary = vec![true; depth + 1];
        let (graph, copies) = reader_inputs(&data, &index, &primary);

        let mut reader =
            PackReader::new(&data, &index, &graph, &copies, pack_reads(&copies), 1 << 20);

        let err = reader.read(depth).unwrap_err().to_string();
        assert!(err.contains("too deep"), "unexpected error: {err}");
    }

    #[test]
    fn pack_reader_resolves_a_ref_delta_on_a_later_base() {
        // Position 0: R, a REF delta on A; position 1: A, a whole tree.
        // Reads R, A at times 0, 1. A is requested at 0 (the build of R)
        // and 1 (its read), so it is cached between them.
        let a = tree(0);
        let r = shifted(&a, b"+r");
        let mut pack = PackBuilder::new();
        pack.add_ref_shift([2; 20], [1; 20], &a, b"+r");
        pack.add([1; 20], OBJ_TREE, &a);
        let (data, index) = pack.finish();
        let (graph, copies) = reader_inputs(&data, &index, &[true, true]);
        assert_eq!(graph.base(0), Some(1), "the base comes after the delta");

        let mut reader =
            PackReader::new(&data, &index, &graph, &copies, pack_reads(&copies), 1 << 20);

        assert_eq!(reader.read(0).unwrap(), r);
        assert_eq!(reader.stats().left, 1, "A waits for its read");
        assert_eq!(reader.read(1).unwrap(), a);

        let stats = reader.stats();
        assert_eq!(stats.hits, 1, "A for its read");
        assert_eq!(stats.misses, 0);
        assert_eq!(stats.left, 0);
    }

    #[test]
    fn pack_reader_reads_each_work_unit_on_its_own() {
        // Two families: a, with deltas on it and on its deltas, and e, with
        // a REF delta that comes before it
        let a = tree(0);
        let b = shifted(&a, b"+b");
        let mut pack = PackBuilder::new();
        let o_a = pack.add([1; 20], OBJ_TREE, &a);
        let o_b = pack.add_ofs_shift([2; 20], o_a, &a, b"+b");
        pack.add_ofs_delta([3; 20], o_a, b"small, on a");
        let o_c = pack.add_ofs_shift([4; 20], o_b, &b, b"+c");
        pack.add_ref_delta([5; 20], [8; 20], b"ref delta on the later e");
        pack.add_ofs_delta([6; 20], o_c, &[b'x'; 300]);
        pack.add_ofs_delta([7; 20], o_b, b"another on b");
        pack.add([8; 20], OBJ_TREE, b"e, a whole tree that comes late");
        pack.add_ofs_delta([9; 20], o_a, &[b'y'; 200]);
        let (data, index) = pack.finish();
        let primary = [true, false, true, true, true, true, false, true, true];
        let (graph, copies) = reader_inputs(&data, &index, &primary);

        let mut whole =
            PackReader::new(&data, &index, &graph, &copies, pack_reads(&copies), 1 << 20);
        let expected: Vec<(usize, Vec<u8>)> = pack_reads(&copies)
            .map(|pos| (pos, whole.read(pos).unwrap().to_vec()))
            .collect();

        let units = crate::parser::work::work_units(&graph, pack_reads(&copies), 1);
        assert_eq!(units, vec![vec![0, 2, 3, 5, 8], vec![4, 7]]);
        let mut got = Vec::new();
        for unit in &units {
            let reads = unit.iter().map(|&p| p as usize);
            let mut reader =
                PackReader::new(&data, &index, &graph, &copies, reads.clone(), 1 << 20);
            for pos in reads {
                got.push((pos, reader.read(pos).unwrap().to_vec()));
            }
            // A family needs no base from another unit
            let stats = reader.stats();
            assert_eq!(stats.misses, 0);
            assert_eq!(stats.left, 0);
        }
        got.sort();
        assert_eq!(got, expected);

        // A unit keeps only the positions that it requests
        let e = units[1].iter().map(|&p| p as usize);
        let reader = PackReader::new(&data, &index, &graph, &copies, e, 1 << 20);
        let mut slots: Vec<u32> = reader.slots.keys().copied().collect();
        slots.sort_unstable();
        assert_eq!(slots, vec![4, 7]);
    }

    #[test]
    fn pack_reader_peaks_equal_the_simulation_when_nothing_is_evicted() {
        // Shared bases, chains, a forward REF delta, entries of several
        // sizes, and entries that are not read
        let a = tree(0);
        let b = shifted(&a, b"+b");
        let c = shifted(&b, b"+c");
        let mut pack = PackBuilder::new();
        let o_a = pack.add([1; 20], OBJ_TREE, &a);
        let o_b = pack.add_ofs_shift([2; 20], o_a, &a, b"+b");
        pack.add_ofs_delta([3; 20], o_a, b"small, on a");
        let o_c = pack.add_ofs_shift([4; 20], o_b, &b, b"+c");
        pack.add_ref_delta([5; 20], [8; 20], b"ref delta on the later e");
        pack.add_ofs_delta([6; 20], o_c, &[b'x'; 300]);
        pack.add_ofs_delta([7; 20], o_b, b"another on b");
        pack.add([8; 20], OBJ_TREE, b"e, a whole tree that comes late");
        pack.add_ofs_delta([9; 20], o_a, &[b'y'; 200]);
        let (data, index) = pack.finish();
        let primary = [true, false, true, true, true, true, false, true, true];
        let (graph, copies) = reader_inputs(&data, &index, &primary);
        let (entries, _) = scan_pack(&data, &index).unwrap();

        let sim = crate::parser::delta_cache::simulate_cache(
            &graph,
            crate::parser::delta_cache::pack_reads(&copies),
            |pos| entries[pos].size,
        )
        .unwrap();
        assert!(sim.peak_entries > 1, "the fixture exercises the cache");

        let mut reader =
            PackReader::new(&data, &index, &graph, &copies, pack_reads(&copies), 1 << 20);
        for pos in crate::parser::delta_cache::pack_reads(&copies) {
            let body = reader.read(pos).unwrap();
            if pos == 3 {
                assert_eq!(body, c, "a delta on a delta that was not read");
            }
        }

        let stats = reader.stats();
        assert_eq!(stats.misses, 0);
        assert_eq!(stats.evictions, 0);
        assert_eq!(stats.bypassed, 0);
        assert_eq!(stats.too_large, 0);
        assert_eq!(stats.peak_bytes, sim.peak_bytes);
        assert_eq!(stats.peak_entries, sim.peak_entries);
        assert_eq!(stats.left, sim.left);
        assert_eq!(stats.left, 0);
    }

    #[test]
    fn scan_pack_does_not_inflate_whole_objects() {
        let mut pack = PackBuilder::new();
        pack.add_corrupt([1; 20], OBJ_BLOB, 123_456);
        pack.add_corrupt([2; 20], OBJ_TREE, 77);
        let (data, index) = pack.finish();

        let (entries, _) = scan_pack(&data, &index).unwrap();

        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].size, 123_456);
        assert_eq!(entries[1].size, 77);
    }

    #[test]
    fn scan_pack_rejects_ref_delta_with_missing_base() {
        let mut pack = PackBuilder::new();
        pack.add_ref_delta([1; 20], [0xee; 20], b"orphan");
        let (data, index) = pack.finish();

        let err = scan_pack(&data, &index).unwrap_err().to_string();

        assert!(err.contains("eeeeeeee"), "unexpected error: {err}");
    }

    #[test]
    fn delta_graph_gives_the_position_of_each_base() {
        let mut pack = PackBuilder::new();
        let o_blob = pack.add([1; 20], OBJ_BLOB, b"base blob");
        // Position 1: OFS delta on position 0
        pack.add_ofs_delta([2; 20], o_blob, b"ofs result");
        // Position 2: REF delta on position 0, a base before it
        pack.add_ref_delta([3; 20], [1; 20], b"ref back");
        // Position 3: REF delta on position 4, a base after it
        pack.add_ref_delta([4; 20], [5; 20], b"ref forward");
        pack.add([5; 20], OBJ_TREE, b"later tree");
        let (data, index) = pack.finish();

        let (entries, graph) = scan_pack(&data, &index).unwrap();

        assert_eq!(graph.len(), 5);
        let bases: Vec<Option<usize>> = (0..graph.len()).map(|p| graph.base(p)).collect();
        assert_eq!(bases, vec![None, Some(0), Some(0), Some(4), None]);
        assert_eq!(entries[3].kind, ObjectKind::Tree, "kind of the later base");
    }

    #[test]
    fn scan_pack_rejects_ofs_delta_into_the_middle_of_an_entry() {
        let mut pack = PackBuilder::new();
        let o_blob = pack.add([1; 20], OBJ_BLOB, b"base blob");
        pack.add_ofs_delta([2; 20], o_blob + 1, b"bad base");
        let (data, index) = pack.finish();

        let err = scan_pack(&data, &index).unwrap_err();

        match err.downcast_ref::<ParseError>() {
            Some(ParseError::DeltaBaseNotFound(offset)) => assert_eq!(*offset, o_blob + 1),
            _ => panic!("unexpected error: {err}"),
        }
    }

    #[test]
    fn a_delta_chain_too_deep_for_the_simulation_does_not_stop_the_parse() {
        // 65 empty trees. Each is a REF delta to the next one, so the first
        // read walks the whole chain and the simulation stops at
        // MAX_DELTA_DEPTH. Pass 2 can read a chain of this length.
        let n = MAX_DELTA_DEPTH + 1;
        let hash = |i: usize| [i as u8 + 1; 20];
        let mut pack = PackBuilder::new();
        for i in 0..n - 1 {
            pack.add_ref_delta(hash(i), hash(i + 1), b"");
        }
        pack.add(hash(n - 1), OBJ_TREE, b"");
        let dir = tempfile::tempdir().unwrap();
        pack.write(&dir.path().join("objects/pack"));
        std::fs::write(dir.path().join("HEAD"), "ref: refs/heads/main\n").unwrap();

        let (_, tree_entries, _, _, container) = crate::parser::parse_objects(dir.path()).unwrap();
        assert_eq!(container.trees.len(), n);
        assert_eq!(tree_entries.entry_count(), 0);

        // The simulation of the same pack fails
        let scanned = crate::parser::scan::scan_repository(dir.path()).unwrap();
        let open = &scanned.packs[0];
        let copies = &scanned.scan.container.copies[open.first_copy..];
        let reads = crate::parser::delta_cache::pack_reads(copies);
        let err =
            crate::parser::delta_cache::simulate_cache(&open.graph, reads, |_| 1).unwrap_err();
        assert!(
            err.to_string().contains("too deep"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn pack_index_offset_of_uses_hash_order() {
        let mut pack = PackBuilder::new();
        let o9 = pack.add([9; 20], OBJ_BLOB, b"a");
        let o1 = pack.add([1; 20], OBJ_BLOB, b"b");
        let (_, index) = pack.finish();

        assert_eq!(index.offset_of(&[1; 20]), Some(o1));
        assert_eq!(index.offset_of(&[9; 20]), Some(o9));
        assert_eq!(index.offset_of(&[5; 20]), None);
    }

    #[test]
    fn read_varint_reads_multi_byte_values() {
        // 300 = 0b10_0101100, low 7 bits first
        assert_eq!(read_varint(&[0xac, 0x02, 0xff]).unwrap(), (300, 2));
    }

    /// Bytes that do not compress to a few bytes
    fn sample(len: usize, seed: u8) -> Vec<u8> {
        (0..len)
            .map(|i| (i * 31 + seed as usize) as u8 ^ (i >> 7) as u8)
            .collect()
    }

    #[test]
    fn inflate_gives_the_object_in_one_allocation_of_its_size() {
        let body = sample(5000, 1);
        let out = inflate(&mut Decompress::new(true), &zlib(&body), body.len()).unwrap();
        assert_eq!(out, body);
        assert_eq!(out.capacity(), body.len(), "allocated once, with the size");
    }

    #[test]
    fn inflate_stops_at_the_end_of_the_stream() {
        // In a pack, the next entry follows the stream
        let body = sample(300, 2);
        let mut data = zlib(&body);
        data.extend_from_slice(b"next entry");
        let out = inflate(&mut Decompress::new(true), &data, body.len()).unwrap();
        assert_eq!(out, body);
    }

    #[test]
    fn inflate_gives_an_empty_object() {
        let out = inflate(&mut Decompress::new(true), &zlib(b""), 0).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn inflate_reuses_one_decoder_for_each_object() {
        let mut inflater = Decompress::new(true);
        for (len, seed) in [(4000, 3), (17, 4), (70_000, 5)] {
            let body = sample(len, seed);
            assert_eq!(inflate(&mut inflater, &zlib(&body), len).unwrap(), body);
        }
        // An error does not spoil the decoder for the next object
        assert!(inflate(&mut inflater, &zlib(&sample(10, 6)), 11).is_err());
        let body = sample(10, 7);
        assert_eq!(inflate(&mut inflater, &zlib(&body), 10).unwrap(), body);
    }

    #[test]
    fn inflate_rejects_a_stream_shorter_than_the_size() {
        let body = sample(100, 8);
        assert!(inflate(&mut Decompress::new(true), &zlib(&body), 101).is_err());
    }

    #[test]
    fn inflate_rejects_a_stream_longer_than_the_size() {
        let body = sample(100, 9);
        assert!(inflate(&mut Decompress::new(true), &zlib(&body), 99).is_err());
    }

    #[test]
    fn inflate_rejects_a_stream_without_its_end() {
        // All the data is there, but the stream stops before its checksum
        let body = sample(100, 10);
        let data = zlib(&body);
        assert!(inflate(&mut Decompress::new(true), &data[..data.len() - 4], 100).is_err());
    }

    #[test]
    fn delta_result_size_reads_second_varint() {
        // Base size 5, result size 300, then a one-byte insert instruction
        let delta = [0x05, 0xac, 0x02, 0x01, b'x'];
        assert_eq!(
            delta_result_size(&mut Decompress::new(true), &zlib(&delta)).unwrap(),
            300
        );
    }
}
