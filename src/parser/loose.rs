use std::fs::File;
use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result};
use flate2::read::ZlibDecoder;
use hashbrown::HashMap;
use tracing::debug;

use super::content::{hex_to_hash, parse_commit, parse_tag, parse_tree};
use crate::error::ParseError;
use crate::models::container::{CommitRaw, HashIndex, ObjectHash, TreeRaw};
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
            let hash = hex_to_hash(hash_hex.as_bytes())?;

            // Skip if already indexed (from pack files)
            if hash_index.contains(&hash) {
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
    let file_size = file.metadata()?.len();
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

    let size: u64 = parts
        .next()
        .ok_or_else(|| ParseError::InvalidObjectFormat("missing size".into()))?
        .parse()
        .map_err(|_| ParseError::InvalidObjectFormat("invalid size".into()))?;

    let content = &data[null_pos + 1..];

    match obj_type {
        "commit" => {
            let index = container.commits.len();
            container
                .commits
                .push(Commit::new(index, *hash, size, file_size));
            hash_index.commits.insert(*hash, index);

            let raw = parse_commit(content, index)?;
            commits_raw.insert(*hash, raw);
        }
        "tree" => {
            let index = container.trees.len();
            container
                .trees
                .push(Tree::new(index, *hash, size, file_size));
            hash_index.trees.insert(*hash, index);

            let entries = parse_tree(content)?;
            trees_raw.insert(*hash, TreeRaw { index, entries });
        }
        "blob" => {
            let index = container.blobs.len();
            container
                .blobs
                .push(Blob::new(index, *hash, size, file_size));
            hash_index.blobs.insert(*hash, index);
        }
        "tag" => {
            let index = container.tags.len();
            let raw = parse_tag(content)?;
            container
                .tags
                .push(Tag::new(index, *hash, size, file_size, raw.name, raw.target));
            hash_index.tags.insert(*hash, index);
        }
        _ => {
            // Ignore unknown types
        }
    }

    Ok(())
}
