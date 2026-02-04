use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;

use anyhow::{anyhow, Context, Result};
use flate2::read::ZlibDecoder;
use hashbrown::HashMap;
use tracing::{debug, info};

use crate::error::ParseError;
use crate::models::container::{CommitRaw, HashIndex, ObjectHash, TreeEntry, TreeRaw};
use crate::models::{Blob, Commit, ObjectContainer, Tag, Tree};

/// Object type constants from Git pack format
const OBJ_COMMIT: u8 = 1;
const OBJ_TREE: u8 = 2;
const OBJ_BLOB: u8 = 3;
const OBJ_TAG: u8 = 4;
const OBJ_OFS_DELTA: u8 = 6;
const OBJ_REF_DELTA: u8 = 7;

/// Parse all pack files in the given directory
pub fn parse_pack_files(
    pack_dir: &Path,
    commits_raw: &mut HashMap<ObjectHash, CommitRaw>,
    trees_raw: &mut HashMap<ObjectHash, TreeRaw>,
    hash_index: &mut HashIndex,
    container: &mut ObjectContainer,
) -> Result<()> {
    // Find all .idx files (each has a corresponding .pack file)
    let entries = std::fs::read_dir(pack_dir).context("Failed to read pack directory")?;

    for entry in entries {
        let entry = entry?;
        let path = entry.path();

        if path.extension().is_some_and(|ext| ext == "idx") {
            let pack_path = path.with_extension("pack");
            if pack_path.exists() {
                info!("Parsing pack file: {}", pack_path.display());
                parse_pack_file(&pack_path, &path, commits_raw, trees_raw, hash_index, container)?;
            }
        }
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
    // Parse the pack index to get object locations
    let index = PackIndex::parse(idx_path)?;
    debug!("Pack index contains {} objects", index.entries.len());

    // Memory-map the pack file for efficient random access
    let pack_file = File::open(pack_path)?;
    let pack_mmap = unsafe { memmap2::Mmap::map(&pack_file)? };

    // Verify pack header
    verify_pack_header(&pack_mmap)?;

    // Parse each object in the pack
    for entry in &index.entries {
        parse_pack_object(
            &pack_mmap,
            &index,
            entry,
            commits_raw,
            trees_raw,
            hash_index,
            container,
        )?;
    }

    Ok(())
}

/// Verify the pack file header
fn verify_pack_header(data: &[u8]) -> Result<()> {
    if data.len() < 12 {
        return Err(anyhow!("Pack file too small"));
    }

    // Check signature
    let signature: [u8; 4] = data[0..4].try_into()?;
    if &signature != b"PACK" {
        return Err(ParseError::InvalidPackHeader(signature).into());
    }

    // Check version
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
    entry: &PackIndexEntry,
    commits_raw: &mut HashMap<ObjectHash, CommitRaw>,
    trees_raw: &mut HashMap<ObjectHash, TreeRaw>,
    hash_index: &mut HashIndex,
    container: &mut ObjectContainer,
) -> Result<()> {
    let offset = entry.offset as usize;
    let (obj_type, obj_size, header_len) = parse_object_header(&pack_data[offset..])?;

    let data_start = offset + header_len;

    // Handle delta objects
    let (final_type, obj_data) = match obj_type {
        OBJ_OFS_DELTA => {
            let (base_offset, delta_header_len) = parse_ofs_delta_header(&pack_data[data_start..])?;
            let abs_base_offset = offset - base_offset;
            let delta_data = decompress_data(&pack_data[data_start + delta_header_len..])?;
            let base_data = resolve_object_at_offset(pack_data, index, abs_base_offset as u64)?;
            let resolved = apply_delta(&base_data.1, &delta_data)?;
            (base_data.0, resolved)
        }
        OBJ_REF_DELTA => {
            let base_hash: ObjectHash = pack_data[data_start..data_start + 20].try_into()?;
            let delta_data = decompress_data(&pack_data[data_start + 20..])?;

            // Find base object by hash
            let base_entry = index
                .entries
                .iter()
                .find(|e| e.hash == base_hash)
                .ok_or_else(|| ParseError::ObjectNotFound(hex::encode(base_hash)))?;

            let base_data = resolve_object_at_offset(pack_data, index, base_entry.offset)?;
            let resolved = apply_delta(&base_data.1, &delta_data)?;
            (base_data.0, resolved)
        }
        _ => {
            let obj_data = decompress_data(&pack_data[data_start..])?;
            (obj_type, obj_data)
        }
    };

    // Store the object based on its type
    let disk_size = entry.crc32; // Using CRC field as approximate disk size for now
    store_object(
        final_type,
        obj_size as u32,
        disk_size,
        &obj_data,
        &entry.hash,
        commits_raw,
        trees_raw,
        hash_index,
        container,
    )
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
) -> Result<(u8, Vec<u8>)> {
    let offset = offset as usize;
    let (obj_type, _obj_size, header_len) = parse_object_header(&pack_data[offset..])?;

    let data_start = offset + header_len;

    match obj_type {
        OBJ_OFS_DELTA => {
            let (base_offset, delta_header_len) = parse_ofs_delta_header(&pack_data[data_start..])?;
            let abs_base_offset = offset - base_offset;
            let delta_data = decompress_data(&pack_data[data_start + delta_header_len..])?;
            let base_data = resolve_object_at_offset(pack_data, index, abs_base_offset as u64)?;
            let resolved = apply_delta(&base_data.1, &delta_data)?;
            Ok((base_data.0, resolved))
        }
        OBJ_REF_DELTA => {
            let base_hash: ObjectHash = pack_data[data_start..data_start + 20].try_into()?;
            let delta_data = decompress_data(&pack_data[data_start + 20..])?;

            let base_entry = index
                .entries
                .iter()
                .find(|e| e.hash == base_hash)
                .ok_or_else(|| ParseError::ObjectNotFound(hex::encode(base_hash)))?;

            let base_data = resolve_object_at_offset(pack_data, index, base_entry.offset)?;
            let resolved = apply_delta(&base_data.1, &delta_data)?;
            Ok((base_data.0, resolved))
        }
        _ => {
            let obj_data = decompress_data(&pack_data[data_start..])?;
            Ok((obj_type, obj_data))
        }
    }
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

            result.extend_from_slice(&base[copy_offset..copy_offset + copy_size]);
        } else if cmd != 0 {
            // Insert instruction
            let insert_size = cmd as usize;
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
    size: u32,
    size_disk: u32,
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
            container.commits.push(Commit::new(index, size, size_disk));
            hash_index.commits.insert(*hash, index);

            let raw = parse_commit_content(data, index)?;
            commits_raw.insert(*hash, raw);
        }
        OBJ_TREE => {
            let index = container.trees.len();
            container.trees.push(Tree::new(index, size, size_disk));
            hash_index.trees.insert(*hash, index);

            let entries = parse_tree_content(data)?;
            trees_raw.insert(*hash, TreeRaw { index, entries });
        }
        OBJ_BLOB => {
            let index = container.blobs.len();
            container.blobs.push(Blob::new(index, size, size_disk));
            hash_index.blobs.insert(*hash, index);
            // No raw data needed for blobs
        }
        OBJ_TAG => {
            let index = container.tags.len();
            let mut tag = Tag::new(index, size, size_disk);

            // Parse tag name from content
            if let Ok(content) = std::str::from_utf8(data) {
                for line in content.lines() {
                    if let Some(name) = line.strip_prefix("tag ") {
                        tag.name = Some(name.to_string());
                        break;
                    }
                }
            }

            container.tags.push(tag);
            hash_index.tags.insert(*hash, index);
        }
        _ => {
            // Ignore unknown types
        }
    }

    Ok(())
}

/// Parse commit content to extract tree and parent hashes
fn parse_commit_content(data: &[u8], index: usize) -> Result<CommitRaw> {
    let text = std::str::from_utf8(data)?;
    let mut tree_hash = None;
    let mut parent_hashes = Vec::new();

    for line in text.lines() {
        if line.is_empty() {
            break; // End of headers
        }

        if let Some(hash_str) = line.strip_prefix("tree ") {
            tree_hash = Some(hex_to_hash(hash_str)?);
        } else if let Some(hash_str) = line.strip_prefix("parent ") {
            parent_hashes.push(hex_to_hash(hash_str)?);
        }
    }

    Ok(CommitRaw {
        index,
        root_tree_hash: tree_hash
            .ok_or_else(|| ParseError::InvalidObjectFormat("commit missing tree".into()))?,
        parent_hashes,
    })
}

/// Parse tree content to extract entries
fn parse_tree_content(data: &[u8]) -> Result<Vec<TreeEntry>> {
    let mut entries = Vec::new();
    let mut pos = 0;

    while pos < data.len() {
        // Parse mode (ASCII digits until space)
        let space_pos = data[pos..]
            .iter()
            .position(|&b| b == b' ')
            .ok_or_else(|| ParseError::InvalidObjectFormat("tree entry missing space".into()))?;

        let mode_str = std::str::from_utf8(&data[pos..pos + space_pos])?;
        let mode = u32::from_str_radix(mode_str, 8)
            .map_err(|_| ParseError::InvalidObjectFormat("invalid mode".into()))?;
        pos += space_pos + 1;

        // Parse name (until null byte)
        let null_pos = data[pos..]
            .iter()
            .position(|&b| b == 0)
            .ok_or_else(|| ParseError::InvalidObjectFormat("tree entry missing null".into()))?;

        let name = std::str::from_utf8(&data[pos..pos + null_pos])?.to_string();
        pos += null_pos + 1;

        // Parse hash (20 bytes binary)
        if pos + 20 > data.len() {
            return Err(ParseError::InvalidObjectFormat("tree entry truncated".into()).into());
        }

        let hash: ObjectHash = data[pos..pos + 20].try_into()?;
        pos += 20;

        entries.push(TreeEntry::new(mode, name, hash));
    }

    Ok(entries)
}

/// Convert a hex string to a hash
fn hex_to_hash(hex: &str) -> Result<ObjectHash> {
    if hex.len() != 40 {
        return Err(ParseError::InvalidObjectFormat("invalid hash length".into()).into());
    }

    let mut hash = [0u8; 20];
    for (i, chunk) in hex.as_bytes().chunks(2).enumerate() {
        let s = std::str::from_utf8(chunk)?;
        hash[i] =
            u8::from_str_radix(s, 16).map_err(|_| ParseError::InvalidObjectFormat("invalid hex".into()))?;
    }

    Ok(hash)
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
        let file = File::open(path)?;
        let mut reader = BufReader::new(file);
        let mut data = Vec::new();
        reader.read_to_end(&mut data)?;

        // Check magic number and version
        if data.len() < 8 {
            return Err(anyhow!("Index file too small"));
        }

        let magic = &data[0..4];
        let version = u32::from_be_bytes(data[4..8].try_into()?);

        if magic != b"\xfftOc" || version != 2 {
            return Err(anyhow!("Unsupported index format"));
        }

        // Parse fanout table (256 entries, 4 bytes each)
        let fanout_end = 8 + 256 * 4;
        let total_objects = u32::from_be_bytes(data[fanout_end - 4..fanout_end].try_into()?) as usize;

        // Parse SHA-1 table
        let sha_start = fanout_end;
        let sha_end = sha_start + total_objects * 20;

        // Parse CRC32 table
        let crc_start = sha_end;
        let crc_end = crc_start + total_objects * 4;

        // Parse offset table (4 bytes each, may have 8-byte extension)
        let offset_start = crc_end;
        let offset_end = offset_start + total_objects * 4;

        // Check for large offsets
        let large_offset_start = offset_end;

        let mut entries = Vec::with_capacity(total_objects);

        for i in 0..total_objects {
            let hash: ObjectHash = data[sha_start + i * 20..sha_start + (i + 1) * 20]
                .try_into()?;

            let crc32 = u32::from_be_bytes(
                data[crc_start + i * 4..crc_start + (i + 1) * 4].try_into()?,
            );

            let offset_raw = u32::from_be_bytes(
                data[offset_start + i * 4..offset_start + (i + 1) * 4].try_into()?,
            );

            let offset = if offset_raw & 0x80000000 != 0 {
                // Large offset - look up in extension table
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

        Ok(Self { entries })
    }
}

// Re-export hex encoding for error messages
mod hex {
    pub fn encode(data: impl AsRef<[u8]>) -> String {
        data.as_ref()
            .iter()
            .map(|b| format!("{:02x}", b))
            .collect()
    }
}
