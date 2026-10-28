//! Pass 2 of parsing: inflate the bodies of commits, trees and tags from
//! each object's primary copy. Blobs are never inflated.
//!
//! Copies are visited in the order pass 1 recorded them, which is pack by
//! pack and offset-ascending within each pack, so every pack is read
//! sequentially and delta bases are almost always already cached.

use std::time::Instant;

use anyhow::{Context, Result};
use hashbrown::HashMap;
use indicatif::{ProgressBar, ProgressStyle};
use tracing::info;

use super::content::{parse_commit, parse_tag, parse_tree};
use super::loose::read_loose_object;
use super::pack::PackReader;
use super::scan::Scanned;
use crate::error::ParseError;
use crate::models::container::{CommitRaw, ObjectHash, TreeRaw};
use crate::models::{ObjectId, ObjectKind};

/// Header data of commits and trees, keyed by hash, for relationship
/// building
pub struct Bodies {
    pub commits_raw: HashMap<ObjectHash, CommitRaw>,
    pub trees_raw: HashMap<ObjectHash, TreeRaw>,
}

/// Read every commit, tree and tag body and fill in what pass 1 could not
/// know: commit headers, tree entries, and tag names and targets.
pub fn read_bodies(scanned: &mut Scanned) -> Result<Bodies> {
    let start = Instant::now();
    let Scanned {
        scan,
        packs,
        objects_dir,
    } = scanned;

    let mut bodies = Bodies {
        commits_raw: HashMap::with_capacity(scan.container.commits.len()),
        trees_raw: HashMap::with_capacity(scan.container.trees.len()),
    };

    let to_read =
        scan.container.commits.len() + scan.container.trees.len() + scan.container.tags.len();
    let progress = ProgressBar::new(to_read as u64);
    progress.set_style(
        ProgressStyle::default_bar()
            .template("  Reading bodies [{bar:40.cyan/blue}] {pos}/{len} ({percent}%) [{elapsed_precise}] {msg}")
            .expect("valid template")
            .progress_chars("#>-"),
    );

    // The reader for the pack the current run of copies belongs to
    let mut reader: Option<(usize, PackReader)> = None;

    for pos in 0..scan.container.copies.len() {
        let copy = scan.container.copies[pos];
        let id = copy.object;
        if id.kind() == ObjectKind::Blob || scan.primary(id) != pos {
            continue;
        }

        let hash = scan.container.hash_of(id);
        let body = match copy.pack {
            Some(pack_idx) => {
                if reader.as_ref().is_none_or(|(p, _)| *p != pack_idx) {
                    if let Some((p, r)) = &reader {
                        log_cache(&packs[*p].file.pack.name, r);
                    }
                    let open = &packs[pack_idx];
                    reader = Some((pack_idx, PackReader::new(&open.data, &open.index)));
                }
                let (_, r) = reader.as_mut().unwrap();
                let offset = copy.offset.unwrap_or(0);
                r.read(offset).with_context(|| {
                    format!(
                        "Failed to inflate {} at offset {} of {}",
                        hex(&hash),
                        offset,
                        packs[pack_idx].file.pack.name
                    )
                })?
            }
            None => {
                let path = loose_path(objects_dir, &hash);
                let (kind, body) = read_loose_object(&path)
                    .with_context(|| format!("Failed to read loose object {}", hex(&hash)))?;
                if kind != id.kind() {
                    return Err(ParseError::InvalidObjectFormat(format!(
                        "loose object {} is a {} but was scanned as a {}",
                        hex(&hash),
                        kind.name(),
                        id.kind().name()
                    ))
                    .into());
                }
                body
            }
        };

        match id {
            ObjectId::Commit(i) => {
                let raw =
                    parse_commit(&body, i).with_context(|| format!("Bad commit {}", hex(&hash)))?;
                bodies.commits_raw.insert(hash, raw);
            }
            ObjectId::Tree(i) => {
                let entries =
                    parse_tree(&body).with_context(|| format!("Bad tree {}", hex(&hash)))?;
                bodies.trees_raw.insert(hash, TreeRaw { index: i, entries });
            }
            ObjectId::Tag(i) => {
                let raw = parse_tag(&body).with_context(|| format!("Bad tag {}", hex(&hash)))?;
                let tag = &mut scan.container.tags[i];
                tag.name = raw.name;
                tag.target_hash = raw.target;
            }
            ObjectId::Blob(_) => unreachable!("blobs are skipped above"),
        }
        progress.inc(1);
    }
    if let Some((p, r)) = &reader {
        log_cache(&packs[*p].file.pack.name, r);
    }

    progress.finish_and_clear();
    println!(
        "Pass 2 (bodies): {} commits, {} trees, {} tags in {:?}",
        bodies.commits_raw.len(),
        bodies.trees_raw.len(),
        scan.container.tags.len(),
        start.elapsed()
    );

    Ok(bodies)
}

fn log_cache(pack_name: &str, reader: &PackReader) {
    let (cached, blocks, bytes) = reader.cache_stats();
    info!(
        "{}: {} delta bases cached in {} blocks ({:.2} MB)",
        pack_name,
        cached,
        blocks,
        bytes as f64 / (1024.0 * 1024.0)
    );
}

/// `objects/xx/yyyy...` for a hash
fn loose_path(objects_dir: &std::path::Path, hash: &ObjectHash) -> std::path::PathBuf {
    let hex = hex(hash);
    objects_dir.join(&hex[..2]).join(&hex[2..])
}

fn hex(hash: &ObjectHash) -> String {
    hash.iter().map(|b| format!("{b:02x}")).collect()
}
