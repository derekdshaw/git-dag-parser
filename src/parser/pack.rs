use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;
use std::time::Instant;

use anyhow::{anyhow, Context, Result};
use flate2::read::ZlibDecoder;
use hashbrown::HashMap;
use indicatif::{ProgressBar, ProgressStyle};
use tracing::debug;

use super::content::{parse_commit, parse_tag, parse_tree};
use crate::error::ParseError;
use crate::models::container::{CommitRaw, HashIndex, ObjectHash, TreeRaw};
use crate::models::{Blob, Commit, ObjectContainer, Tag, Tree};

/// Object type constants from Git pack format
const OBJ_COMMIT: u8 = 1;
const OBJ_TREE: u8 = 2;
const OBJ_BLOB: u8 = 3;
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

fn type_name(t: u8) -> &'static str {
    match t {
        OBJ_COMMIT => "commit",
        OBJ_TREE => "tree",
        OBJ_BLOB => "blob",
        OBJ_TAG => "tag",
        OBJ_OFS_DELTA => "ofs-delta",
        OBJ_REF_DELTA => "ref-delta",
        _ => "unknown",
    }
}

/// Parse all pack files in the given directory
pub fn parse_pack_files(
    pack_dir: &Path,
    commits_raw: &mut HashMap<ObjectHash, CommitRaw>,
    trees_raw: &mut HashMap<ObjectHash, TreeRaw>,
    hash_index: &mut HashIndex,
    container: &mut ObjectContainer,
) -> Result<()> {
    let entries = std::fs::read_dir(pack_dir).context("Failed to read pack directory")?;

    let mut pack_files: Vec<(std::path::PathBuf, std::path::PathBuf)> = Vec::new();
    for entry in entries {
        let entry = entry?;
        let path = entry.path();

        if path.extension().is_some_and(|ext| ext == "idx") {
            let pack_path = path.with_extension("pack");
            if pack_path.exists() {
                pack_files.push((pack_path, path));
            }
        }
    }

    println!("Found {} pack file(s)", pack_files.len());

    for (pack_path, idx_path) in &pack_files {
        let pack_size = std::fs::metadata(pack_path)?.len();
        println!(
            "Parsing pack: {} ({:.2} MB)",
            pack_path.file_name().unwrap_or_default().to_string_lossy(),
            pack_size as f64 / (1024.0 * 1024.0)
        );
        parse_pack_file(pack_path, idx_path, commits_raw, trees_raw, hash_index, container)?;
    }

    Ok(())
}

/// Parse a single pack file and its index
fn parse_pack_file(
    pack_path: &Path,
    idx_path: &Path,
    commits_raw: &mut HashMap<ObjectHash, CommitRaw>,
    trees_raw: &mut HashMap<ObjectHash, TreeRaw>,
    hash_index: &mut HashIndex,
    container: &mut ObjectContainer,
) -> Result<()> {
    let start = Instant::now();

    // Parse the pack index to get object locations
    let index = PackIndex::parse(idx_path)?;
    let total_objects = index.entries.len();
    println!("  Index parsed: {} objects ({:?})", total_objects, start.elapsed());

    // Build offset→index lookup for fast REF_DELTA and OFS_DELTA resolution
    println!("  Building hash lookup table...");
    let hash_to_offset: HashMap<ObjectHash, u64> = index
        .entries
        .iter()
        .map(|e| (e.hash, e.offset))
        .collect();

    // Memory-map the pack file for efficient random access
    let pack_file = File::open(pack_path)?;
    let pack_mmap = unsafe { memmap2::Mmap::map(&pack_file)? };
    println!("  Pack file memory-mapped: {} bytes", pack_mmap.len());

    // Verify pack header
    verify_pack_header(&pack_mmap)?;
    let header_obj_count = u32::from_be_bytes(pack_mmap[8..12].try_into()?);
    println!("  Pack header: version 2, {} objects declared", header_obj_count);

    // Delta resolution cache backed by block arena allocator
    let mut delta_arena = DeltaArena::new(ARENA_BLOCK_SIZE);
    let mut delta_cache: HashMap<u64, DeltaCacheEntry> = HashMap::new();

    // Progress bar
    let progress = ProgressBar::new(total_objects as u64);
    progress.set_style(
        ProgressStyle::default_bar()
            .template("  Parsing [{bar:40.cyan/blue}] {pos}/{len} ({percent}%) [{elapsed_precise}] {msg}")
            .expect("valid template")
            .progress_chars("#>-"),
    );

    // The pack trailer is a SHA-1 checksum of everything before it.
    let pack_end = (pack_mmap.len() - 20) as u64;
    let offsets: Vec<u64> = index.entries.iter().map(|e| e.offset).collect();
    let disk_sizes = disk_sizes(&offsets, pack_end);

    let mut parsed = 0u64;
    let mut skipped_blobs = 0u64;
    let mut type_counts: [u64; 8] = [0; 8];

    // Parse each object in the pack
    for (entry, &size_disk) in index.entries.iter().zip(&disk_sizes) {
        // Count raw object type (cheap - just read the header byte)
        let offset = entry.offset as usize;
        if offset < pack_mmap.len() {
            let raw_type = (pack_mmap[offset] >> 4) & 0x07;
            type_counts[raw_type as usize] += 1;
        }

        // A failed object would silently drop every edge below it, so stop.
        parse_pack_object(
            &pack_mmap,
            &index,
            &hash_to_offset,
            entry,
            size_disk,
            &mut delta_arena,
            &mut delta_cache,
            commits_raw,
            trees_raw,
            hash_index,
            container,
            &mut skipped_blobs,
        )
        .with_context(|| format!("Failed to parse object {}", hex::encode(entry.hash)))?;
        parsed += 1;

        progress.inc(1);

        // Update message periodically
        if parsed % 10000 == 0 {
            progress.set_message(format!(
                "C:{} T:{} B:{} t:{}",
                container.commits.len(),
                container.trees.len(),
                container.blobs.len(),
                container.tags.len(),
            ));
        }
    }

    progress.finish_with_message(format!(
        "done - C:{} T:{} B:{} t:{} delta-cache:{} blob-skip:{}",
        container.commits.len(),
        container.trees.len(),
        container.blobs.len(),
        container.tags.len(),
        delta_cache.len(),
        skipped_blobs,
    ));

    // Print type breakdown
    for (i, &count) in type_counts.iter().enumerate() {
        if count > 0 {
            println!("    {}: {}", type_name(i as u8), count);
        }
    }
    println!(
        "  Delta arena: {} blocks, {:.2} MB, {} cached entries",
        delta_arena.block_count(),
        delta_arena.total_bytes() as f64 / (1024.0 * 1024.0),
        delta_cache.len(),
    );
    println!("  Pack parsed in {:?}", start.elapsed());

    Ok(())
}

/// Verify the pack file header
fn verify_pack_header(data: &[u8]) -> Result<()> {
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

/// Parse a single object from the pack file
fn parse_pack_object(
    pack_data: &[u8],
    index: &PackIndex,
    hash_to_offset: &HashMap<ObjectHash, u64>,
    entry: &PackIndexEntry,
    size_disk: u64,
    delta_arena: &mut DeltaArena,
    delta_cache: &mut HashMap<u64, DeltaCacheEntry>,
    commits_raw: &mut HashMap<ObjectHash, CommitRaw>,
    trees_raw: &mut HashMap<ObjectHash, TreeRaw>,
    hash_index: &mut HashIndex,
    container: &mut ObjectContainer,
    skipped_blobs: &mut u64,
) -> Result<()> {
    // Packs can overlap; the first copy parsed is the one recorded.
    if hash_index.contains(&entry.hash) {
        return Ok(());
    }

    let offset = entry.offset as usize;
    let (obj_type, obj_size, header_len) = parse_object_header(&pack_data[offset..])?;

    let data_start = offset + header_len;

    let delta = match obj_type {
        OBJ_OFS_DELTA | OBJ_REF_DELTA => Some(delta_base(
            pack_data,
            hash_to_offset,
            offset,
            obj_type,
            data_start,
        )?),
        _ => None,
    };

    // A delta's header size is the size of the delta, not of the object. The
    // object's size is in the delta's own header, and its type is the type
    // at the end of the base chain.
    let (final_type, size) = match delta {
        Some((base_offset, payload_start)) => (
            resolved_type(pack_data, hash_to_offset, base_offset)?,
            delta_result_size(&pack_data[payload_start..])?,
        ),
        None => (obj_type, obj_size),
    };

    // Blob content is never needed, so blobs are never inflated
    if final_type == OBJ_BLOB {
        let index_val = container.blobs.len();
        container
            .blobs
            .push(Blob::new(index_val, entry.hash, size, size_disk));
        hash_index.blobs.insert(entry.hash, index_val);
        *skipped_blobs += 1;
        return Ok(());
    }

    let obj_data = match delta {
        Some((base_offset, payload_start)) => {
            let delta_data = decompress_data(&pack_data[payload_start..])?;
            let (_, base_data) = resolve_object_at_offset(
                pack_data,
                index,
                hash_to_offset,
                base_offset as u64,
                delta_arena,
                delta_cache,
                0,
            )?;
            apply_delta(&base_data, &delta_data)?
        }
        None => decompress_data(&pack_data[data_start..])?,
    };

    store_object(
        final_type,
        size,
        size_disk,
        &obj_data,
        &entry.hash,
        commits_raw,
        trees_raw,
        hash_index,
        container,
    )
}

/// Locate a delta's base object and the start of its zlib-compressed payload
fn delta_base(
    pack_data: &[u8],
    hash_to_offset: &HashMap<ObjectHash, u64>,
    offset: usize,
    obj_type: u8,
    data_start: usize,
) -> Result<(usize, usize)> {
    if obj_type == OBJ_OFS_DELTA {
        let (relative, header_len) = parse_ofs_delta_header(&pack_data[data_start..])?;
        Ok((offset - relative, data_start + header_len))
    } else {
        let base_hash: ObjectHash = pack_data[data_start..data_start + 20].try_into()?;
        let base_offset = hash_to_offset
            .get(&base_hash)
            .ok_or_else(|| ParseError::ObjectNotFound(hex::encode(base_hash)))?;
        Ok((*base_offset as usize, data_start + 20))
    }
}

/// Type of the object at the end of a delta chain, found by following base
/// pointers through object headers without inflating anything
fn resolved_type(
    pack_data: &[u8],
    hash_to_offset: &HashMap<ObjectHash, u64>,
    mut offset: usize,
) -> Result<u8> {
    for _ in 0..=MAX_DELTA_DEPTH {
        let (obj_type, _, header_len) = parse_object_header(&pack_data[offset..])?;
        if obj_type != OBJ_OFS_DELTA && obj_type != OBJ_REF_DELTA {
            return Ok(obj_type);
        }
        (offset, _) = delta_base(
            pack_data,
            hash_to_offset,
            offset,
            obj_type,
            offset + header_len,
        )?;
    }
    Err(anyhow!(
        "Delta chain too deep (>{}) at offset {}",
        MAX_DELTA_DEPTH,
        offset
    ))
}

/// Size of the object a delta produces: the second varint of the delta,
/// after the base size. Only the first few bytes are inflated.
fn delta_result_size(compressed: &[u8]) -> Result<u64> {
    // Two varints of at most 10 bytes each
    let mut prefix = [0u8; 20];
    let mut decoder = ZlibDecoder::new(compressed);
    let mut len = 0;
    while len < prefix.len() {
        let n = decoder
            .read(&mut prefix[len..])
            .map_err(|e| ParseError::DecompressionError(e.to_string()))?;
        if n == 0 {
            break;
        }
        len += n;
    }

    let (_, base_len) = read_varint(&prefix[..len])?;
    let (size, _) = read_varint(&prefix[base_len..len])?;
    Ok(size)
}

/// On-disk size of each object: the distance from its offset to the next
/// object's offset, or to `pack_end` for the last one
fn disk_sizes(offsets: &[u64], pack_end: u64) -> Vec<u64> {
    let mut sorted = offsets.to_vec();
    sorted.sort_unstable();
    offsets
        .iter()
        .map(|&offset| {
            let next = sorted.partition_point(|&o| o <= offset);
            sorted.get(next).copied().unwrap_or(pack_end) - offset
        })
        .collect()
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
    hash_to_offset: &HashMap<ObjectHash, u64>,
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
                hash_to_offset,
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

            let base_offset = hash_to_offset
                .get(&base_hash)
                .ok_or_else(|| ParseError::ObjectNotFound(hex::encode(base_hash)))?;

            let base_data = resolve_object_at_offset(
                pack_data,
                index,
                hash_to_offset,
                *base_offset,
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

/// Store a parsed object in the appropriate container
fn store_object(
    obj_type: u8,
    size: u64,
    size_disk: u64,
    data: &[u8],
    hash: &ObjectHash,
    commits_raw: &mut HashMap<ObjectHash, CommitRaw>,
    trees_raw: &mut HashMap<ObjectHash, TreeRaw>,
    hash_index: &mut HashIndex,
    container: &mut ObjectContainer,
) -> Result<()> {
    match obj_type {
        OBJ_COMMIT => {
            let index = container.commits.len();
            container.commits.push(Commit::new(index, *hash, size, size_disk));
            hash_index.commits.insert(*hash, index);

            let raw = parse_commit(data, index)?;
            commits_raw.insert(*hash, raw);
        }
        OBJ_TREE => {
            let index = container.trees.len();
            container.trees.push(Tree::new(index, *hash, size, size_disk));
            hash_index.trees.insert(*hash, index);

            let entries = parse_tree(data)?;
            trees_raw.insert(*hash, TreeRaw { index, entries });
        }
        OBJ_BLOB => {
            let index = container.blobs.len();
            container.blobs.push(Blob::new(index, *hash, size, size_disk));
            hash_index.blobs.insert(*hash, index);
        }
        OBJ_TAG => {
            let index = container.tags.len();
            let raw = parse_tag(data)?;
            container
                .tags
                .push(Tag::new(index, *hash, size, size_disk, raw.name, raw.target));
            hash_index.tags.insert(*hash, index);
        }
        _ => {
            debug!("Ignoring unknown object type: {}", obj_type);
        }
    }

    Ok(())
}

/// Pack index structure
struct PackIndex {
    entries: Vec<PackIndexEntry>,
}

/// Single entry in a pack index
struct PackIndexEntry {
    hash: ObjectHash,
    offset: u64,
    crc32: u32,
}

impl PackIndex {
    /// Parse a pack index file (version 2 format)
    fn parse(path: &Path) -> Result<Self> {
        let start = Instant::now();
        let file = File::open(path)?;
        let mut reader = BufReader::new(file);
        let mut data = Vec::new();
        reader.read_to_end(&mut data)?;

        println!(
            "  Reading index: {} ({:.2} MB)",
            path.file_name().unwrap_or_default().to_string_lossy(),
            data.len() as f64 / (1024.0 * 1024.0)
        );

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

        println!("  Index contains {} objects", total_objects);

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
            let hash: ObjectHash =
                data[sha_start + i * 20..sha_start + (i + 1) * 20].try_into()?;

            let crc32 = u32::from_be_bytes(
                data[crc_start + i * 4..crc_start + (i + 1) * 4].try_into()?,
            );

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

            entries.push(PackIndexEntry { hash, offset, crc32 });
        }

        println!("  Index parsed in {:?}", start.elapsed());

        Ok(Self { entries })
    }
}

/// Hex encoding for error messages and debug output
mod hex {
    pub fn encode(data: impl AsRef<[u8]>) -> String {
        data.as_ref()
            .iter()
            .map(|b| format!("{:02x}", b))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use flate2::write::ZlibEncoder;
    use flate2::Compression;

    use super::*;

    fn zlib(data: &[u8]) -> Vec<u8> {
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(data).unwrap();
        encoder.finish().unwrap()
    }

    #[test]
    fn disk_sizes_measure_to_next_offset_in_pack_order() {
        // Index entries are in hash order, not offset order.
        assert_eq!(disk_sizes(&[100, 12, 40], 150), vec![50, 28, 60]);
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
        assert_eq!(delta_result_size(&zlib(&delta)).unwrap(), 300);
    }
}
