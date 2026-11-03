//! Pass 2 of parsing: inflate the bodies of commits, trees and tags from
//! each object's primary copy. Blobs are never inflated.
//!
//! Copies are visited in the order pass 1 recorded them, which is pack by
//! pack and offset-ascending within each pack, so every pack is read
//! sequentially.

use std::time::Instant;

use anyhow::{Context, Result};
use hashbrown::HashMap;
use indicatif::ProgressStyle;
use tracing::info;

use super::content::{parse_commit, parse_tag, parse_tree};
use super::loose::read_loose_object;
use super::pack::PackReader;
use super::scan::Scanned;
use crate::error::ParseError;
use crate::models::container::{CommitRaw, ObjectHash, TreeEntries, TreeEntriesBuilder};
use crate::models::{NameStore, ObjectId, ObjectKind};
use crate::size::format_size;

/// Commit headers, tree entries, and the names the entries refer to
pub struct Bodies {
    pub commits_raw: HashMap<ObjectHash, CommitRaw>,
    /// Every tree's entries, with children resolved to indices
    pub tree_entries: TreeEntries,
    /// The entry names that `tree_entries` refers to
    pub names: NameStore,
}

/// Read every commit, tree and tag body and fill in what pass 1 could not
/// know: commit headers, tree entries, and tag names and targets.
/// `delta_cache_limit` is the byte limit of the delta cache; one pack is
/// read at a time, so each pack's reader gets the whole limit.
pub fn read_bodies(scanned: &mut Scanned, delta_cache_limit: u64) -> Result<Bodies> {
    let start = Instant::now();
    let Scanned {
        scan,
        packs,
        objects_dir,
    } = scanned;

    let mut commits_raw: HashMap<ObjectHash, CommitRaw> =
        HashMap::with_capacity(scan.container.commits.len());
    let mut trees = TreeEntriesBuilder::new(scan.container.trees.len());
    let mut names = NameStore::new();

    let to_read =
        scan.container.commits.len() + scan.container.trees.len() + scan.container.tags.len();
    let progress = super::progress_bar(to_read as u64);
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
        let id = copy.object();
        if id.kind() == ObjectKind::Blob || scan.primary(id) != pos {
            continue;
        }

        let hash = scan.container.hash_of(id);
        let loose_body;
        let body: &[u8] = match copy.pack() {
            Some(pack_idx) => {
                let open = &packs[pack_idx];
                if reader.as_ref().is_none_or(|(p, _)| *p != pack_idx) {
                    if let Some((p, r)) = &reader {
                        log_cache(&packs[*p].file.pack.name, r);
                    }
                    let copies = &scan.container.copies
                        [open.first_copy..open.first_copy + open.index.entries.len()];
                    reader = Some((
                        pack_idx,
                        PackReader::new(
                            &open.data,
                            &open.index,
                            &open.graph,
                            copies,
                            delta_cache_limit,
                        ),
                    ));
                }
                let (_, r) = reader.as_mut().unwrap();
                let offset = copy.offset().unwrap_or(0);
                r.read(pos - open.first_copy).with_context(|| {
                    format!(
                        "Failed to inflate {} at offset {} of {}",
                        hex(&hash),
                        offset,
                        open.file.pack.name
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
                loose_body = body;
                &loose_body
            }
        };

        match id {
            ObjectId::Commit(i) => {
                let raw =
                    parse_commit(body, i).with_context(|| format!("Bad commit {}", hex(&hash)))?;
                commits_raw.insert(hash, raw);
            }
            ObjectId::Tree(i) => {
                parse_tree(body, i, &mut names, &scan.index, &mut trees)
                    .with_context(|| format!("Bad tree {}", hex(&hash)))?;
            }
            ObjectId::Tag(i) => {
                let raw = parse_tag(body).with_context(|| format!("Bad tag {}", hex(&hash)))?;
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
    let tree_entries = trees.finish();
    info!(
        "Pass 2 (bodies): {} commits, {} trees ({} entries), {} tags in {:?}",
        commits_raw.len(),
        tree_entries.len(),
        tree_entries.entry_count(),
        scan.container.tags.len(),
        start.elapsed()
    );

    Ok(Bodies {
        commits_raw,
        tree_entries,
        names,
    })
}

fn log_cache(pack_name: &str, reader: &PackReader) {
    let s = reader.stats();
    info!(
        "{}: delta cache {} hits, {} misses, {} evictions, {} too large, {} bypassed, peak {} ({} bytes, {} entries), {} left",
        pack_name,
        s.hits,
        s.misses,
        s.evictions,
        s.too_large,
        s.bypassed,
        format_size(s.peak_bytes),
        s.peak_bytes,
        s.peak_entries,
        s.left
    );
    // Each entry is dropped at its last request
    debug_assert_eq!(s.left, 0, "{pack_name}: delta cache entries left");
}

/// `objects/xx/yyyy...` for a hash
fn loose_path(objects_dir: &std::path::Path, hash: &ObjectHash) -> std::path::PathBuf {
    let hex = hex(hash);
    objects_dir.join(&hex[..2]).join(&hex[2..])
}

fn hex(hash: &ObjectHash) -> String {
    hash.iter().map(|b| format!("{b:02x}")).collect()
}
