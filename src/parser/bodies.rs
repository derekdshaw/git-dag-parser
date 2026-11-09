//! Pass 2 of parsing: inflate the bodies of commits, trees and tags from
//! each object's primary copy. Blobs are never inflated.
//!
//! The reads are cut into work units: whole delta families of one pack, or
//! a run of loose objects. A rayon pool reads the units of all packs and
//! the loose objects as one queue, each pack unit in ascending offset
//! order. Each thread gives a `Parsed` with its own names; `merge` puts
//! them together in thread order.

use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use indicatif::{ProgressBar, ProgressStyle};
use rayon::prelude::*;
use tracing::info;

use super::content::{parse_commit, parse_tag, parse_tree, CommitRaw, TagRaw};
use super::delta_cache::{pack_reads, DeltaCacheStats};
use super::loose::read_loose_object;
use super::pack::PackReader;
use super::scan::{OpenPack, Scan, Scanned};
use super::work::{unit_reads, work_units};
use crate::error::ParseError;
use crate::models::container::{HashIndex, ObjectHash, TreeEntries, TreeEntriesBuilder};
use crate::models::{NameStore, ObjectId, ObjectKind};
use crate::size::format_size;

/// Commit headers, tree entries, and the names the entries refer to
pub struct Bodies {
    pub commits_raw: Vec<CommitRaw>,
    /// Every tree's entries, with children resolved to indices
    pub tree_entries: TreeEntries,
    /// The entry names that `tree_entries` refers to
    pub names: NameStore,
    /// The delta cache counts of each pack, in pack order
    pub delta_cache: Vec<DeltaCacheStats>,
    /// Time of pass 2
    pub elapsed: Duration,
}

/// The reads of one work unit
enum Unit {
    /// Positions in pack `pack` of whole delta families, ascending
    Pack { pack: usize, reads: Vec<u32> },
    /// Positions in `container.copies` of primary loose copies
    Loose(Vec<usize>),
}

/// Read every commit, tree and tag body and fill in what pass 1 could not
/// know: commit headers, tree entries, and tag names and targets.
///
/// Runs on the current rayon pool. Each thread's readers get an equal share
/// of `delta_cache_limit`, so the caches of all threads stay within it.
pub fn read_bodies(scanned: &mut Scanned, delta_cache_limit: u64) -> Result<Bodies> {
    let start = Instant::now();
    let threads = rayon::current_num_threads();
    let Scanned {
        scan,
        packs,
        objects_dir,
        ..
    } = scanned;

    let to_read =
        scan.container.commits.len() + scan.container.trees.len() + scan.container.tags.len();
    let progress = super::progress_bar(to_read as u64);
    progress.set_style(
        ProgressStyle::default_bar()
            .template("  Reading bodies [{bar:40.cyan/blue}] {pos}/{len} ({percent}%) [{elapsed_precise}] {msg}")
            .expect("valid template")
            .progress_chars("#>-"),
    );

    let units = units(scan, packs, unit_reads(to_read, threads));
    // One for each thread of the pool, and one for the calling thread: rayon
    // runs a unit there when it does not split the units, as with one unit.
    // A thread locks only its own.
    let workers: Vec<Mutex<Parsed>> = (0..=threads).map(|_| Mutex::default()).collect();
    let cache_limit = delta_cache_limit / threads as u64;
    let (scan_ref, packs_ref) = (&*scan, &*packs);
    let unit_stats = units
        .par_iter()
        .map(|unit| {
            let thread = rayon::current_thread_index().unwrap_or(threads);
            let mut parsed = workers[thread].lock().expect("no worker panicked");
            match unit {
                Unit::Pack { pack, reads } => {
                    let stats = read_pack_unit(
                        scan_ref,
                        &packs_ref[*pack],
                        reads,
                        cache_limit,
                        &mut parsed,
                        &progress,
                    )?;
                    Ok(Some((*pack, stats)))
                }
                Unit::Loose(positions) => {
                    read_loose_unit(scan_ref, objects_dir, positions, &mut parsed, &progress)?;
                    Ok(None)
                }
            }
        })
        .collect::<Result<Vec<_>>>()?;
    progress.finish_and_clear();

    // Units and summed cache stats of each pack
    let mut pack_stats = vec![(0, DeltaCacheStats::default()); packs.len()];
    for (pack, stats) in unit_stats.into_iter().flatten() {
        pack_stats[pack].0 += 1;
        pack_stats[pack].1.add(&stats);
    }
    for (open, (count, stats)) in packs.iter().zip(&pack_stats) {
        log_cache(&open.file.pack.name, *count, stats);
    }

    let parts = workers
        .into_iter()
        .map(|w| w.into_inner().expect("no worker panicked"))
        .collect();
    let (mut bodies, tags) = merge(
        parts,
        scan.container.commits.len(),
        scan.container.trees.len(),
    )?;
    for (i, raw) in tags {
        let tag = &mut scan.container.tags[i];
        tag.name = raw.name;
        tag.target_hash = raw.target;
    }
    bodies.delta_cache = pack_stats.into_iter().map(|(_, stats)| stats).collect();
    bodies.elapsed = start.elapsed();
    info!(
        "Pass 2 (bodies): {} commits, {} trees ({} entries), {} tags on {} threads in {:?}",
        bodies.commits_raw.len(),
        bodies.tree_entries.len(),
        bodies.tree_entries.entry_count(),
        scan.container.tags.len(),
        threads,
        bodies.elapsed
    );

    Ok(bodies)
}

/// The work units of all packs, in pack order, then the primary loose
/// copies of commits, trees and tags in runs of `unit_reads`
fn units(scan: &Scan, packs: &[OpenPack], unit_reads: usize) -> Vec<Unit> {
    let mut units = Vec::new();
    for (pack, open) in packs.iter().enumerate() {
        let copies =
            &scan.container.copies[open.first_copy..open.first_copy + open.index.entries.len()];
        units.extend(
            work_units(&open.graph, pack_reads(copies), unit_reads)
                .into_iter()
                .map(|reads| Unit::Pack { pack, reads }),
        );
    }
    let loose: Vec<usize> = scan
        .container
        .copies
        .iter()
        .enumerate()
        .filter(|(pos, copy)| {
            let id = copy.object();
            copy.pack().is_none() && id.kind() != ObjectKind::Blob && scan.primary(id) == *pos
        })
        .map(|(pos, _)| pos)
        .collect();
    units.extend(loose.chunks(unit_reads).map(|c| Unit::Loose(c.to_vec())));
    units
}

/// Read the bodies at positions `reads` of pack `open` into `parsed`, with
/// a reader whose delta cache holds at most `cache_limit` bytes
fn read_pack_unit(
    scan: &Scan,
    open: &OpenPack,
    reads: &[u32],
    cache_limit: u64,
    parsed: &mut Parsed,
    progress: &ProgressBar,
) -> Result<DeltaCacheStats> {
    let copies =
        &scan.container.copies[open.first_copy..open.first_copy + open.index.entries.len()];
    let reads = reads.iter().map(|&p| p as usize);
    let mut reader = PackReader::new(
        &open.data,
        &open.index,
        &open.graph,
        copies,
        reads.clone(),
        cache_limit,
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
    Ok(reader.stats())
}

/// Read the loose objects of the copies at `positions` into `parsed`
fn read_loose_unit(
    scan: &Scan,
    objects_dir: &Path,
    positions: &[usize],
    parsed: &mut Parsed,
    progress: &ProgressBar,
) -> Result<()> {
    for &pos in positions {
        let id = scan.container.copies[pos].object();
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
    Ok(())
}

/// What the bodies one worker reads give, before it goes into the
/// container: commit headers, tree entries in read order with the worker's
/// own names, and each tag's index with its name and target. Sized by what
/// the worker reads, not by the repository.
///
/// One for each worker, not for each work unit: the entry arrays of a
/// worker are a few large allocations. One set for each unit (1,735 on
/// linux) made pass 2 peak 3.4 GB higher, because the unused capacity of
/// medium allocations is resident.
#[derive(Default)]
struct Parsed {
    commits_raw: Vec<CommitRaw>,
    trees: TreeEntriesBuilder,
    names: NameStore,
    tags: Vec<(usize, TagRaw)>,
}

impl Parsed {
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
                self.commits_raw.push(raw);
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

/// The bodies of all workers, merged in the order of `parts`: their name
/// stores in one store, and the tree entries of `trees` trees in tree
/// index order with their names mapped to it. Also returns the tag records.
fn merge(
    parts: Vec<Parsed>,
    commits: usize,
    trees: usize,
) -> Result<(Bodies, Vec<(usize, TagRaw)>)> {
    let start = Instant::now();
    let count = parts.len();
    let mut names = NameStore::new();
    let mut commits_raw = Vec::with_capacity(commits);
    let mut builders = Vec::with_capacity(count);
    let mut maps = Vec::with_capacity(count);
    let mut tags = Vec::new();
    for part in parts {
        maps.push(names.merge(&part.names)?);
        commits_raw.extend(part.commits_raw);
        builders.push(part.trees);
        tags.extend(part.tags);
    }
    let entries_start = Instant::now();
    let tree_entries = TreeEntries::merge(trees, builders.into_iter().zip(&maps).collect());
    info!(
        "Merged {} workers ({} names) in {:?}, tree entries in {:?}",
        count,
        names.len(),
        start.elapsed(),
        entries_start.elapsed()
    );
    Ok((
        Bodies {
            commits_raw,
            tree_entries,
            names,
            delta_cache: Vec::new(),
            elapsed: Duration::ZERO,
        },
        tags,
    ))
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
fn loose_path(objects_dir: &Path, hash: &ObjectHash) -> std::path::PathBuf {
    let hex = hex(hash);
    objects_dir.join(&hex[..2]).join(&hex[2..])
}

fn hex(hash: &ObjectHash) -> String {
    hash.iter().map(|b| format!("{b:02x}")).collect()
}
