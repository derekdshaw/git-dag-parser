//! Pass 2 of parsing: inflate the bodies of commits, trees and tags from
//! each object's primary copy. Blobs are never inflated.
//!
//! Each pack is read in work units of whole delta families, and each unit
//! in ascending offset order. The loose objects come after the packs.

use std::time::Instant;

use anyhow::{Context, Result};
use hashbrown::HashMap;
use indicatif::ProgressStyle;
use tracing::info;

use super::content::{parse_commit, parse_tag, parse_tree, TagRaw};
use super::delta_cache::{pack_reads, DeltaCacheStats};
use super::loose::read_loose_object;
use super::pack::PackReader;
use super::scan::Scanned;
use super::work::work_units;
use crate::error::ParseError;
use crate::models::container::{CommitRaw, HashIndex, ObjectHash, TreeEntries, TreeEntriesBuilder};
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

/// Target number of reads in one work unit
const UNIT_READS: usize = 4096;

/// Read every commit, tree and tag body and fill in what pass 1 could not
/// know: commit headers, tree entries, and tag names and targets.
///
/// Each pack is read in work units of whole delta families (see `work`),
/// one unit at a time, so each unit's reader gets the whole
/// `delta_cache_limit`. The loose objects come after the packs.
pub fn read_bodies(scanned: &mut Scanned, delta_cache_limit: u64) -> Result<Bodies> {
    let start = Instant::now();
    let Scanned {
        scan,
        packs,
        objects_dir,
    } = scanned;

    let mut parsed = Parsed::new(scan.container.commits.len(), scan.container.trees.len());

    let to_read =
        scan.container.commits.len() + scan.container.trees.len() + scan.container.tags.len();
    let progress = super::progress_bar(to_read as u64);
    progress.set_style(
        ProgressStyle::default_bar()
            .template("  Reading bodies [{bar:40.cyan/blue}] {pos}/{len} ({percent}%) [{elapsed_precise}] {msg}")
            .expect("valid template")
            .progress_chars("#>-"),
    );

    for open in packs.iter() {
        let copies =
            &scan.container.copies[open.first_copy..open.first_copy + open.index.entries.len()];
        let units = work_units(&open.graph, pack_reads(copies), UNIT_READS);
        let mut stats = DeltaCacheStats::default();
        for unit in &units {
            let reads = unit.iter().map(|&p| p as usize);
            let mut reader = PackReader::new(
                &open.data,
                &open.index,
                &open.graph,
                copies,
                reads.clone(),
                delta_cache_limit,
            );
            for pos in reads {
                let id = copies[pos].object();
                let hash = scan.container.hash_of(id);
                let body = reader.read(pos).with_context(|| {
                    format!(
                        "Failed to inflate {} at offset {} of {}",
                        hex(&hash),
                        copies[pos].offset().unwrap_or(0),
                        open.file.pack.name
                    )
                })?;
                parsed.add(id, &hash, body, &scan.index)?;
                progress.inc(1);
            }
            stats.add(&reader.stats());
        }
        log_cache(&open.file.pack.name, units.len(), &stats);
    }

    for (pos, copy) in scan.container.copies.iter().enumerate() {
        let id = copy.object();
        if copy.pack().is_some() || id.kind() == ObjectKind::Blob || scan.primary(id) != pos {
            continue;
        }
        let hash = scan.container.hash_of(id);
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
        parsed.add(id, &hash, &body, &scan.index)?;
        progress.inc(1);
    }

    progress.finish_and_clear();
    let Parsed {
        commits_raw,
        trees,
        names,
        tags,
    } = parsed;
    for (i, raw) in tags {
        let tag = &mut scan.container.tags[i];
        tag.name = raw.name;
        tag.target_hash = raw.target;
    }
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

/// What the bodies give, before it goes into the container: commit headers,
/// tree entries and their names, and each tag's index with its name and
/// target
struct Parsed {
    commits_raw: HashMap<ObjectHash, CommitRaw>,
    trees: TreeEntriesBuilder,
    names: NameStore,
    tags: Vec<(usize, TagRaw)>,
}

impl Parsed {
    fn new(commits: usize, trees: usize) -> Self {
        Self {
            commits_raw: HashMap::with_capacity(commits),
            trees: TreeEntriesBuilder::new(trees),
            names: NameStore::new(),
            tags: Vec::new(),
        }
    }

    /// Parse the body of the object `id` with hash `hash`
    fn add(
        &mut self,
        id: ObjectId,
        hash: &ObjectHash,
        body: &[u8],
        index: &HashIndex,
    ) -> Result<()> {
        match id {
            ObjectId::Commit(i) => {
                let raw =
                    parse_commit(body, i).with_context(|| format!("Bad commit {}", hex(hash)))?;
                self.commits_raw.insert(*hash, raw);
            }
            ObjectId::Tree(i) => {
                parse_tree(body, i, &mut self.names, index, &mut self.trees)
                    .with_context(|| format!("Bad tree {}", hex(hash)))?;
            }
            ObjectId::Tag(i) => {
                let raw = parse_tag(body).with_context(|| format!("Bad tag {}", hex(hash)))?;
                self.tags.push((i, raw));
            }
            ObjectId::Blob(_) => unreachable!("pass 2 does not read blobs"),
        }
        Ok(())
    }
}

/// The delta cache counts of a pack: summed over its work units, with the
/// largest peak of one unit
fn log_cache(pack_name: &str, units: usize, s: &DeltaCacheStats) {
    info!(
        "{}: delta cache in {} units: {} hits, {} misses, {} evictions, {} too large, {} bypassed, largest unit peak {} ({} bytes, {} entries), {} left",
        pack_name,
        units,
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
