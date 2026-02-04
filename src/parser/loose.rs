use std::fs::File;
use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result};
use flate2::read::ZlibDecoder;
use hashbrown::HashMap;
use tracing::debug;

use crate::error::ParseError;
use crate::models::container::{CommitRaw, HashIndex, ObjectHash, TreeEntry, TreeRaw};
use crate::models::{Blob, Commit, ObjectContainer, Tag, Tree};

/// Parse all loose objects in the objects directory
pub fn parse_loose_objects(
    objects_dir: &Path,
    commits_raw: &mut HashMap<ObjectHash, CommitRaw>,
    trees_raw: &mut HashMap<ObjectHash, TreeRaw>,
    hash_index: &mut HashIndex,
    container: &mut ObjectContainer,
) -> Result<()> {
    // Loose objects are stored in subdirectories named by the first two hex chars
    // e.g., .git/objects/ab/cdef1234...
    for entry in std::fs::read_dir(objects_dir)? {
        let entry = entry?;
        let path = entry.path();

        // Skip non-directories and special directories
        if !path.is_dir() {
            continue;
        }

        let dir_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("");

        // Skip pack and info directories
        if dir_name == "pack" || dir_name == "info" {
            continue;
        }

        // Must be a 2-character hex directory
        if dir_name.len() != 2 || !dir_name.chars().all(|c| c.is_ascii_hexdigit()) {
            continue;
        }

        // Process all objects in this directory
        for obj_entry in std::fs::read_dir(&path)? {
            let obj_entry = obj_entry?;
            let obj_path = obj_entry.path();

            if !obj_path.is_file() {
                continue;
            }

            let obj_name = obj_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("");

            // Must be a 38-character hex filename
            if obj_name.len() != 38 || !obj_name.chars().all(|c| c.is_ascii_hexdigit()) {
                continue;
            }

            // Construct the full hash
            let hash_hex = format!("{}{}", dir_name, obj_name);
            let hash = hex_to_hash(&hash_hex)?;

            // Skip if already indexed (from pack files)
            if hash_index.commits.contains_key(&hash)
                || hash_index.trees.contains_key(&hash)
                || hash_index.blobs.contains_key(&hash)
                || hash_index.tags.contains_key(&hash)
            {
                continue;
            }

            debug!("Parsing loose object: {}", hash_hex);
            parse_loose_object(
                &obj_path,
                &hash,
                commits_raw,
                trees_raw,
                hash_index,
                container,
            )?;
        }
    }

    Ok(())
}

/// Parse a single loose object file
fn parse_loose_object(
    path: &Path,
    hash: &ObjectHash,
    commits_raw: &mut HashMap<ObjectHash, CommitRaw>,
    trees_raw: &mut HashMap<ObjectHash, TreeRaw>,
    hash_index: &mut HashIndex,
    container: &mut ObjectContainer,
) -> Result<()> {
    // Read and decompress the file
    let file = File::open(path).context("Failed to open loose object")?;
    let file_size = file.metadata()?.len() as u32;
    let mut decoder = ZlibDecoder::new(file);
    let mut data = Vec::new();
    decoder
        .read_to_end(&mut data)
        .map_err(|e| ParseError::DecompressionError(e.to_string()))?;

    // Parse header: "<type> <size>\0<content>"
    let null_pos = data
        .iter()
        .position(|&b| b == 0)
        .ok_or_else(|| ParseError::InvalidObjectFormat("missing null in header".into()))?;

    let header = std::str::from_utf8(&data[..null_pos])?;
    let mut parts = header.split(' ');

    let obj_type = parts
        .next()
        .ok_or_else(|| ParseError::InvalidObjectFormat("missing type".into()))?;

    let size: u32 = parts
        .next()
        .ok_or_else(|| ParseError::InvalidObjectFormat("missing size".into()))?
        .parse()
        .map_err(|_| ParseError::InvalidObjectFormat("invalid size".into()))?;

    let content = &data[null_pos + 1..];

    match obj_type {
        "commit" => {
            let index = container.commits.len();
            container.commits.push(Commit::new(index, size, file_size));
            hash_index.commits.insert(*hash, index);

            let raw = parse_commit_content(content, index)?;
            commits_raw.insert(*hash, raw);
        }
        "tree" => {
            let index = container.trees.len();
            container.trees.push(Tree::new(index, size, file_size));
            hash_index.trees.insert(*hash, index);

            let entries = parse_tree_content(content)?;
            trees_raw.insert(*hash, TreeRaw { index, entries });
        }
        "blob" => {
            let index = container.blobs.len();
            container.blobs.push(Blob::new(index, size, file_size));
            hash_index.blobs.insert(*hash, index);
        }
        "tag" => {
            let index = container.tags.len();
            let mut tag = Tag::new(index, size, file_size);

            // Parse tag name
            if let Ok(text) = std::str::from_utf8(content) {
                for line in text.lines() {
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
            break;
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
        // Parse mode
        let space_pos = data[pos..]
            .iter()
            .position(|&b| b == b' ')
            .ok_or_else(|| ParseError::InvalidObjectFormat("tree entry missing space".into()))?;

        let mode_str = std::str::from_utf8(&data[pos..pos + space_pos])?;
        let mode = u32::from_str_radix(mode_str, 8)
            .map_err(|_| ParseError::InvalidObjectFormat("invalid mode".into()))?;
        pos += space_pos + 1;

        // Parse name
        let null_pos = data[pos..]
            .iter()
            .position(|&b| b == 0)
            .ok_or_else(|| ParseError::InvalidObjectFormat("tree entry missing null".into()))?;

        let name = std::str::from_utf8(&data[pos..pos + null_pos])?.to_string();
        pos += null_pos + 1;

        // Parse hash
        if pos + 20 > data.len() {
            return Err(ParseError::InvalidObjectFormat("tree entry truncated".into()).into());
        }

        let hash: ObjectHash = data[pos..pos + 20].try_into()?;
        pos += 20;

        entries.push(TreeEntry::new(mode, name, hash));
    }

    Ok(entries)
}

/// Convert hex string to hash bytes
fn hex_to_hash(hex: &str) -> Result<ObjectHash> {
    if hex.len() != 40 {
        return Err(ParseError::InvalidObjectFormat("invalid hash length".into()).into());
    }

    let mut hash = [0u8; 20];
    for (i, chunk) in hex.as_bytes().chunks(2).enumerate() {
        let s = std::str::from_utf8(chunk)?;
        hash[i] = u8::from_str_radix(s, 16)
            .map_err(|_| ParseError::InvalidObjectFormat("invalid hex".into()))?;
    }

    Ok(hash)
}
