use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;

use anyhow::{anyhow, Result};
use flate2::read::ZlibDecoder;
use flate2::{Decompress, FlushDecompress};
use hashbrown::HashMap;

use crate::error::ParseError;
use crate::models::container::ObjectHash;
use crate::models::ObjectKind;

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
const MAX_DELTA_DEPTH: usize = 64;

/// Block size for the delta arena (64 MB per block)
const ARENA_BLOCK_SIZE: usize = 64 * 1024 * 1024;

/// Arena allocator for delta cache data.
/// Uses fixed-size blocks that never reallocate. When a block fills up,
/// a new one is allocated. Dropping frees a handful of large blocks
/// instead of hundreds of thousands of small Vec<u8>.
struct DeltaArena {
    blocks: Vec<Vec<u8>>,
    block_size: usize,
}

impl DeltaArena {
    fn new(block_size: usize) -> Self {
        Self {
            blocks: vec![Vec::with_capacity(block_size)],
            block_size,
        }
    }

    /// Store bytes in the arena, return (block_index, offset, len)
    fn store(&mut self, bytes: &[u8]) -> (u16, u32, u32) {
        let len = bytes.len();

        // Check if current block has room
        let needs_new_block = {
            let current = self.blocks.last().unwrap();
            current.len() + len > self.block_size
        };

        if needs_new_block {
            // For oversized items, allocate a block big enough
            let cap = self.block_size.max(len);
            self.blocks.push(Vec::with_capacity(cap));
        }

        let block_idx = self.blocks.len() - 1;
        let block = &mut self.blocks[block_idx];
        let offset = block.len() as u32;
        block.extend_from_slice(bytes);

        (block_idx as u16, offset, len as u32)
    }

    /// Get a slice from the arena
    fn get(&self, block_idx: u16, offset: u32, len: u32) -> &[u8] {
        &self.blocks[block_idx as usize][offset as usize..(offset as usize + len as usize)]
    }

    /// Number of entries (blocks) allocated
    fn block_count(&self) -> usize {
        self.blocks.len()
    }

    /// Total bytes stored across all blocks
    fn total_bytes(&self) -> usize {
        self.blocks.iter().map(|b| b.len()).sum()
    }
}

/// Cache entry: (object_type, block_index, arena_offset, arena_len)
type DeltaCacheEntry = (u8, u16, u32, u32);

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

/// Inflates entries of one pack, keeping resolved delta bases in an arena
/// so a chain is only walked once. Entries read in offset order get the
/// best reuse, since an OFS delta's base always precedes it.
pub struct PackReader<'a> {
    data: &'a [u8],
    index: &'a PackIndex,
    arena: DeltaArena,
    cache: HashMap<u64, DeltaCacheEntry>,
}

impl<'a> PackReader<'a> {
    pub fn new(data: &'a [u8], index: &'a PackIndex) -> Self {
        Self {
            data,
            index,
            arena: DeltaArena::new(ARENA_BLOCK_SIZE),
            cache: HashMap::new(),
        }
    }

    /// The inflated, delta-resolved content of the entry at `offset`
    pub fn read(&mut self, offset: u64) -> Result<Vec<u8>> {
        let offset = offset as usize;
        let (obj_type, _, header_len) = parse_object_header(&self.data[offset..])?;
        let data_start = offset + header_len;

        match obj_type {
            OBJ_OFS_DELTA | OBJ_REF_DELTA => {
                let (base_offset, payload_start) =
                    delta_base(self.data, self.index, offset, obj_type, data_start)?;
                let delta = decompress_data(&self.data[payload_start..])?;
                // Only bases are cached; the result itself is returned once.
                let (_, base) = resolve_object_at_offset(
                    self.data,
                    self.index,
                    base_offset as u64,
                    &mut self.arena,
                    &mut self.cache,
                    0,
                )?;
                apply_delta(&base, &delta)
            }
            _ => decompress_data(&self.data[data_start..]),
        }
    }

    /// (cached bases, arena blocks, arena bytes)
    pub fn cache_stats(&self) -> (usize, usize, usize) {
        (
            self.cache.len(),
            self.arena.block_count(),
            self.arena.total_bytes(),
        )
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

/// Read every entry's header in offset order, so the pack is touched
/// sequentially from front to back. Nothing is inflated except the first
/// few bytes of each delta, which hold the result size.
///
/// Scanning in offset order also means an OFS delta's base has always been
/// seen already, so its kind is a lookup rather than a walk up the chain.
pub fn scan_pack(pack_data: &[u8], index: &PackIndex) -> Result<Vec<ScannedEntry>> {
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
                let kind = match offsets.binary_search(&(base_offset as u64)) {
                    Ok(p) if kinds[p].is_some() => kinds[p].unwrap(),
                    // A REF delta may point forward to a base not yet scanned
                    _ => pack_kind(resolved_type(pack_data, index, base_offset)?)?,
                };
                (
                    kind,
                    delta_result_size(&mut inflater, &pack_data[payload_start..])?,
                    true,
                )
            }
            _ => (pack_kind(obj_type)?, obj_size, false),
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

    Ok(entries)
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

/// Decompress zlib-compressed data
fn decompress_data(data: &[u8]) -> Result<Vec<u8>> {
    let mut decoder = ZlibDecoder::new(data);
    let mut result = Vec::new();
    decoder
        .read_to_end(&mut result)
        .map_err(|e| ParseError::DecompressionError(e.to_string()))?;
    Ok(result)
}

/// Resolve an object at a specific offset (for delta resolution)
fn resolve_object_at_offset(
    pack_data: &[u8],
    index: &PackIndex,
    offset: u64,
    delta_arena: &mut DeltaArena,
    delta_cache: &mut HashMap<u64, DeltaCacheEntry>,
    depth: usize,
) -> Result<(u8, Vec<u8>)> {
    if depth > MAX_DELTA_DEPTH {
        return Err(anyhow!(
            "Delta chain too deep (>{}) at offset {}",
            MAX_DELTA_DEPTH,
            offset
        ));
    }

    // Check cache first - copy data out of arena
    if let Some(&(obj_type, blk, off, len)) = delta_cache.get(&offset) {
        let data = delta_arena.get(blk, off, len).to_vec();
        return Ok((obj_type, data));
    }

    let offset_usize = offset as usize;
    let (obj_type, _obj_size, header_len) = parse_object_header(&pack_data[offset_usize..])?;

    let data_start = offset_usize + header_len;

    let result = match obj_type {
        OBJ_OFS_DELTA => {
            let (base_offset_relative, delta_header_len) =
                parse_ofs_delta_header(&pack_data[data_start..])?;
            let abs_base_offset = offset_usize - base_offset_relative;
            let delta_data = decompress_data(&pack_data[data_start + delta_header_len..])?;
            let base_data = resolve_object_at_offset(
                pack_data,
                index,
                abs_base_offset as u64,
                delta_arena,
                delta_cache,
                depth + 1,
            )?;
            let resolved = apply_delta(&base_data.1, &delta_data)?;
            (base_data.0, resolved)
        }
        OBJ_REF_DELTA => {
            let base_hash: ObjectHash = pack_data[data_start..data_start + 20].try_into()?;
            let delta_data = decompress_data(&pack_data[data_start + 20..])?;

            let base_offset = index
                .offset_of(&base_hash)
                .ok_or_else(|| ParseError::ObjectNotFound(hex::encode(base_hash)))?;

            let base_data = resolve_object_at_offset(
                pack_data,
                index,
                base_offset,
                delta_arena,
                delta_cache,
                depth + 1,
            )?;
            let resolved = apply_delta(&base_data.1, &delta_data)?;
            (base_data.0, resolved)
        }
        _ => {
            let obj_data = decompress_data(&pack_data[data_start..])?;
            (obj_type, obj_data)
        }
    };

    // Cache the resolved object in the arena
    let (blk, off, len) = delta_arena.store(&result.1);
    delta_cache.insert(offset, (result.0, blk, off, len));

    Ok(result)
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
        assert_eq!(super::decompress_data(&compressed).unwrap(), body);
        bencher
            .counter(BytesCount::of_slice(&body))
            .bench_local(|| super::decompress_data(black_box(&compressed)).unwrap());
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
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use flate2::write::ZlibEncoder;
    use flate2::Compression;

    use super::*;
    use crate::models::ObjectKind;

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

        let entries = scan_pack(&data, &index).unwrap();

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

        let entries = scan_pack(&data, &index).unwrap();

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
    fn pack_reader_inflates_whole_objects_and_resolves_delta_chains() {
        let mut pack = PackBuilder::new();
        let o_base = pack.add([1; 20], OBJ_TREE, b"base tree");
        let o_d1 = pack.add_ofs_delta([2; 20], o_base, b"first delta result");
        let o_d2 = pack.add_ofs_delta([3; 20], o_d1, b"second, on the first");
        let o_ref = pack.add_ref_delta([4; 20], [1; 20], b"ref delta result");
        let (data, index) = pack.finish();

        let mut reader = PackReader::new(&data, &index);

        assert_eq!(reader.read(o_base).unwrap(), b"base tree");
        assert_eq!(reader.read(o_d1).unwrap(), b"first delta result");
        assert_eq!(reader.read(o_d2).unwrap(), b"second, on the first");
        assert_eq!(reader.read(o_ref).unwrap(), b"ref delta result");
        // Only bases are cached: the base tree and the first delta
        assert_eq!(reader.cache_stats().0, 2);
    }

    #[test]
    fn scan_pack_does_not_inflate_whole_objects() {
        let mut pack = PackBuilder::new();
        pack.add_corrupt([1; 20], OBJ_BLOB, 123_456);
        pack.add_corrupt([2; 20], OBJ_TREE, 77);
        let (data, index) = pack.finish();

        let entries = scan_pack(&data, &index).unwrap();

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
