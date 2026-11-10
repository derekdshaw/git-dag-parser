use std::collections::{HashMap, HashSet};
use std::fs;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use git_dag_parser_core::models::ObjectId;
use git_dag_parser_core::parser::{parse_objects, parse_objects_with, ParseOptions, ParseResult};
use test_fixtures::{count_by_kind, hex, parsed_objects, Repo};

#[test]
fn every_object_matches_git_type_and_sizes() {
    let repo = Repo::with_history();
    assert!(
        repo.deltified_count() > 0,
        "fixture must contain deltified objects for this test to mean anything"
    );

    let container = parse_objects(&repo.git_dir()).unwrap().container;

    let expected = repo.git_objects();
    let actual = parsed_objects(&container);
    let mut mismatches: Vec<String> = expected
        .iter()
        .filter(|(hash, want)| actual.get(*hash) != Some(*want))
        .map(|(hash, want)| format!("{hash}: git {want:?}, parsed {:?}", actual.get(hash)))
        .collect();
    mismatches.sort();
    assert!(
        mismatches.is_empty(),
        "{} of {} objects differ:\n{}",
        mismatches.len(),
        expected.len(),
        mismatches.join("\n")
    );
}

#[test]
fn objects_in_multiple_packs_are_counted_once() {
    let repo = Repo::with_overlapping_packs();
    assert!(
        repo.pack_count() >= 2,
        "fixture must have overlapping packs"
    );

    let container = parse_objects(&repo.git_dir()).unwrap().container;

    let parsed: HashMap<String, usize> = [
        ("commit", container.commits.len()),
        ("tree", container.trees.len()),
        ("blob", container.blobs.len()),
        ("tag", container.tags.len()),
    ]
    .into_iter()
    .filter(|&(_, n)| n > 0)
    .map(|(kind, n)| (kind.to_string(), n))
    .collect();
    assert_eq!(parsed, count_by_kind(&repo.git_objects()));
}

#[test]
fn non_utf8_names_and_messages_are_parsed() {
    let repo = Repo::with_non_utf8();

    let ParseResult {
        tree_entries,
        container,
        ..
    } = parse_objects(&repo.git_dir()).unwrap();

    for commit in &container.commits {
        assert!(
            commit.tree.is_some() && commit.commit_time > 0,
            "commit {} should have its header parsed",
            hex(&commit.hash)
        );
    }
    for tree in &container.trees {
        assert_eq!(
            tree_entries.entries(tree.index).count() > 0,
            tree.size > 0,
            "tree {} should have its entries parsed",
            hex(&tree.hash)
        );
    }
}

/// Each tree's entries, read through `TreeEntries` and the name store, equal
/// `git ls-tree` of that tree: same order, names, modes and child hashes.
#[test]
fn tree_entries_match_git_ls_tree() {
    for repo in [
        Repo::with_history(),
        Repo::with_non_utf8(),
        Repo::with_refs_and_merges(),
    ] {
        let ParseResult {
            tree_entries,
            names,
            container,
            ..
        } = parse_objects(&repo.git_dir()).unwrap();

        for tree in &container.trees {
            let ours: Vec<(u32, Vec<u8>, String)> = tree_entries
                .entries(tree.index)
                .map(|(child, name, kind)| {
                    let child = child.expect("fixtures have no submodules or missing objects");
                    let hash = if kind.is_tree() {
                        container.trees[child].hash
                    } else {
                        container.blobs[child].hash
                    };
                    (kind.mode(), names.get(name).to_vec(), hex(&hash))
                })
                .collect();

            // `<mode> <type> <hash>\t<name>\0`, repeated
            let listing = repo.git(&["ls-tree", "-z", &hex(&tree.hash)]);
            let git: Vec<(u32, Vec<u8>, String)> = listing
                .split(|&b| b == 0)
                .filter(|line| !line.is_empty())
                .map(|line| {
                    let tab = line.iter().position(|&b| b == b'\t').unwrap();
                    let meta = std::str::from_utf8(&line[..tab]).unwrap();
                    let fields: Vec<&str> = meta.split(' ').collect();
                    let mode = u32::from_str_radix(fields[0], 8).unwrap();
                    (mode, line[tab + 1..].to_vec(), fields[2].to_string())
                })
                .collect();

            assert_eq!(ours, git, "entries of tree {}", hex(&tree.hash));
        }
    }
}

#[test]
fn hash_index_points_at_each_object() {
    let repo = Repo::with_history();

    let ParseResult {
        hash_index,
        container,
        ..
    } = parse_objects(&repo.git_dir()).unwrap();

    for c in &container.commits {
        assert_eq!(hash_index.commit(&c.hash), Some(c.index));
    }
    for t in &container.trees {
        assert_eq!(hash_index.tree(&t.hash), Some(t.index));
    }
    for b in &container.blobs {
        assert_eq!(hash_index.blob(&b.hash), Some(b.index));
    }
    for t in &container.tags {
        assert_eq!(hash_index.tag(&t.hash), Some(t.index));
    }
    let total = container.commits.len()
        + container.trees.len()
        + container.blobs.len()
        + container.tags.len();
    assert_eq!(hash_index.len(), total);
}

#[test]
fn packs_match_files_on_disk() {
    let repo = Repo::with_overlapping_packs();
    let mut pack_paths = repo.pack_files("pack");
    pack_paths.sort();
    assert!(pack_paths.len() >= 2);
    fs::write(pack_paths[1].with_extension("keep"), "").unwrap();

    let container = parse_objects(&repo.git_dir()).unwrap().container;

    assert_eq!(container.packs.len(), pack_paths.len());
    for (i, (pack, path)) in container.packs.iter().zip(&pack_paths).enumerate() {
        let meta = fs::metadata(path).unwrap();
        assert_eq!(pack.index, i);
        assert_eq!(pack.name, path.file_name().unwrap().to_string_lossy());
        assert_eq!(pack.size, meta.len());
        let mtime = meta
            .modified()
            .unwrap()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        assert_eq!(pack.mtime, mtime);
        assert_eq!(pack.keep, i == 1);
        assert!(!pack.cruft);
        assert!(!pack.promisor);
    }
}

#[test]
fn every_on_disk_copy_is_recorded() {
    let repo = Repo::with_overlapping_packs();
    let expected_counts = repo.pack_object_counts();
    let loose_count = repo.loose_object_count();
    assert!(loose_count > 0, "fixture must have loose objects");

    let container = parse_objects(&repo.git_dir()).unwrap().container;

    for pack in &container.packs {
        let in_pack: Vec<_> = container
            .copies
            .iter()
            .filter(|c| c.pack() == Some(pack.index))
            .collect();
        assert_eq!(
            in_pack.len(),
            expected_counts[&pack.name],
            "copies in {}",
            pack.name
        );
        assert!(in_pack.iter().all(|c| c.offset().is_some()));
        assert!(in_pack
            .iter()
            .all(|c| container.copy_mtime(c) == pack.mtime));
        let mut offsets: Vec<u64> = in_pack.iter().map(|c| c.offset().unwrap()).collect();
        assert!(
            offsets.windows(2).all(|w| w[0] < w[1]),
            "copies are in offset order"
        );
        offsets.dedup();
        assert_eq!(offsets.len(), in_pack.len());
    }

    let loose: Vec<_> = container.copies.iter().filter(|c| c.is_loose()).collect();
    assert_eq!(loose.len(), loose_count);
    assert!(loose.iter().all(|c| c.offset().is_none() && !c.is_delta));
    for copy in &loose {
        let hash = hex(&container.hash_of(copy.object()));
        let path = repo
            .git_dir()
            .join("objects")
            .join(&hash[..2])
            .join(&hash[2..]);
        let mtime = fs::metadata(&path)
            .unwrap()
            .modified()
            .unwrap()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        assert_eq!(container.copy_mtime(copy), mtime, "loose {hash}");
    }

    let deltas = container.copies.iter().filter(|c| c.is_delta).count();
    assert_eq!(deltas, repo.deltified_count());

    let total_objects = container.commits.len()
        + container.trees.len()
        + container.blobs.len()
        + container.tags.len();
    let with_copies: HashSet<_> = container.copies.iter().map(|c| c.object()).collect();
    assert_eq!(
        with_copies.len(),
        total_objects,
        "every object has at least one copy"
    );
}

#[test]
fn primary_copy_is_in_the_newest_pack() {
    let repo = Repo::with_overlapping_packs();
    let mut pack_paths = repo.pack_files("pack");
    pack_paths.sort();
    // Make the ages unambiguous: the first pack by name is the old one
    let now = SystemTime::now();
    for (i, path) in pack_paths.iter().enumerate() {
        let time = if i == 0 {
            now - Duration::from_secs(3600)
        } else {
            now
        };
        // Packs are written read-only. On Unix the owner may still set times
        // through a read-only handle; Windows needs FILE_WRITE_ATTRIBUTES.
        #[cfg(windows)]
        let file = {
            use std::os::windows::fs::OpenOptionsExt;
            fs::OpenOptions::new()
                .access_mode(0x100)
                .open(path)
                .unwrap()
        };
        #[cfg(not(windows))]
        let file = fs::File::open(path).unwrap();
        file.set_modified(time).unwrap();
    }

    let container = parse_objects(&repo.git_dir()).unwrap().container;

    let newest = container
        .packs
        .iter()
        .max_by_key(|p| (p.mtime, std::cmp::Reverse(&p.name)))
        .unwrap();
    assert_eq!(newest.index, container.packs.len() - 1);
    let mut primaries: HashMap<ObjectId, usize> = HashMap::new();
    for copy in container.copies.iter().filter(|c| c.is_primary) {
        *primaries.entry(copy.object()).or_default() += 1;
    }
    assert!(
        primaries.values().all(|&n| n == 1),
        "at most one primary copy per object"
    );
    let total = container.commits.len()
        + container.trees.len()
        + container.blobs.len()
        + container.tags.len();
    assert_eq!(primaries.len(), total, "every object has a primary copy");

    let newest_copies: Vec<_> = container
        .copies
        .iter()
        .filter(|c| c.pack() == Some(newest.index))
        .collect();
    assert!(!newest_copies.is_empty());
    for copy in newest_copies {
        assert!(
            copy.is_primary,
            "{:?} should use its copy in the newest pack",
            copy.object()
        );
        let size_disk = match copy.object() {
            ObjectId::Commit(i) => container.commits[i].size_disk,
            ObjectId::Tree(i) => container.trees[i].size_disk,
            ObjectId::Blob(i) => container.blobs[i].size_disk,
            ObjectId::Tag(i) => container.tags[i].size_disk,
        };
        assert_eq!(size_disk, copy.size_disk);
    }
}

#[test]
fn objects_are_in_hash_order() {
    let repo = Repo::with_overlapping_packs();

    let container = parse_objects(&repo.git_dir()).unwrap().container;

    let commits: Vec<_> = container
        .commits
        .iter()
        .map(|o| (o.index, o.hash))
        .collect();
    let trees: Vec<_> = container.trees.iter().map(|o| (o.index, o.hash)).collect();
    let blobs: Vec<_> = container.blobs.iter().map(|o| (o.index, o.hash)).collect();
    let tags: Vec<_> = container.tags.iter().map(|o| (o.index, o.hash)).collect();
    for (kind, objects) in [
        ("commit", commits),
        ("tree", trees),
        ("blob", blobs),
        ("tag", tags),
    ] {
        assert!(
            objects
                .iter()
                .enumerate()
                .all(|(pos, &(index, _))| pos == index),
            "{kind} index equals position"
        );
        assert!(
            objects.windows(2).all(|w| w[0].1 < w[1].1),
            "{kind}s are in hash order"
        );
    }
}

/// Hash to object id for every object in a container
fn ids_by_hash(
    container: &git_dag_parser_core::models::ObjectContainer,
) -> HashMap<String, ObjectId> {
    let mut ids = HashMap::new();
    for o in &container.commits {
        ids.insert(hex(&o.hash), ObjectId::Commit(o.index));
    }
    for o in &container.trees {
        ids.insert(hex(&o.hash), ObjectId::Tree(o.index));
    }
    for o in &container.blobs {
        ids.insert(hex(&o.hash), ObjectId::Blob(o.index));
    }
    for o in &container.tags {
        ids.insert(hex(&o.hash), ObjectId::Tag(o.index));
    }
    ids
}

#[test]
fn indices_do_not_change_when_packs_are_rewritten() {
    let repo = Repo::with_history();
    let before = parse_objects(&repo.git_dir()).unwrap().container;
    let packs_before = repo.pack_files("pack");

    // Packs all objects, including the loose ones, into one new pack
    repo.git(&["repack", "-a", "-d", "-f", "--window=0", "-q"]);
    assert_ne!(
        repo.pack_files("pack"),
        packs_before,
        "fixture must rewrite the packs"
    );
    let after = parse_objects(&repo.git_dir()).unwrap().container;

    assert_eq!(ids_by_hash(&after), ids_by_hash(&before));
}

#[test]
fn copy_disk_sizes_match_verify_pack() {
    let repo = Repo::with_overlapping_packs();
    let expected = repo.verify_pack_entries();

    let container = parse_objects(&repo.git_dir()).unwrap().container;

    let packed: Vec<_> = container.copies.iter().filter(|c| !c.is_loose()).collect();
    assert_eq!(packed.len(), expected.len());
    for copy in packed {
        let pack = &container.packs[copy.pack().unwrap()];
        let key = (pack.name.clone(), copy.offset().unwrap());
        let (hash, size_in_pack) = &expected[&key];
        assert_eq!(hex(&container.hash_of(copy.object())), *hash, "{key:?}");
        assert_eq!(copy.size_disk, *size_in_pack, "{key:?}");
    }
}

#[test]
fn cruft_pack_holds_the_unreachable_blob() {
    let repo = Repo::with_cruft_pack();
    let dangling = String::from_utf8(repo.git(&["hash-object", "dangling.txt"])).unwrap();
    let dangling = dangling.trim();

    let container = parse_objects(&repo.git_dir()).unwrap().container;

    let cruft: Vec<_> = container.packs.iter().filter(|p| p.cruft).collect();
    assert_eq!(cruft.len(), 1, "one cruft pack");
    let blob = container
        .blobs
        .iter()
        .find(|b| hex(&b.hash) == dangling)
        .expect("the unreachable blob is parsed");
    let copies: Vec<_> = container
        .copies
        .iter()
        .filter(|c| c.object() == ObjectId::Blob(blob.index))
        .collect();
    assert_eq!(copies.len(), 1);
    assert_eq!(copies[0].pack(), Some(cruft[0].index));
    assert!(copies[0].is_primary);
}

#[test]
fn a_one_byte_delta_cache_gives_the_same_parse() {
    for repo in [Repo::with_history(), Repo::with_overlapping_packs()] {
        assert!(
            repo.deltified_count() > 0,
            "fixture must contain deltified objects for this test to mean anything"
        );
        let ParseResult {
            tree_entries: entries,
            names,
            container,
            ..
        } = parse_objects(&repo.git_dir()).unwrap();
        let ParseResult {
            tree_entries: small_entries,
            names: small_names,
            container: small_container,
            ..
        } = parse_objects_with(
            &repo.git_dir(),
            &ParseOptions {
                delta_cache_limit: 1,
            },
        )
        .unwrap();

        // ObjectContainer has no PartialEq; its Debug output has every field
        assert_eq!(format!("{small_container:?}"), format!("{container:?}"));
        for (tree, t) in container.trees.iter().enumerate() {
            let small: Vec<_> = small_entries
                .entries(tree)
                .map(|(child, name, kind)| (child, small_names.get(name), kind))
                .collect();
            let full: Vec<_> = entries
                .entries(tree)
                .map(|(child, name, kind)| (child, names.get(name), kind))
                .collect();
            assert_eq!(small, full, "entries of tree {}", hex(&t.hash));
        }
    }
}

#[test]
fn the_parse_result_has_the_delta_cache_stats_of_each_pack() {
    let repo = Repo::with_overlapping_packs();
    let parsed = parse_objects(&repo.git_dir()).unwrap();
    assert!(
        parsed.container.packs.len() > 1,
        "fixture must have more than one pack"
    );
    assert_eq!(parsed.delta_cache.len(), parsed.container.packs.len());
    let hits: u64 = parsed.delta_cache.iter().map(|s| s.hits).sum();
    assert!(hits > 0, "{:?}", parsed.delta_cache);

    // Nothing fits in a 1-byte cache
    let small = parse_objects_with(
        &repo.git_dir(),
        &ParseOptions {
            delta_cache_limit: 1,
        },
    )
    .unwrap();
    assert_eq!(small.delta_cache.len(), small.container.packs.len());
    assert!(
        small.delta_cache.iter().all(|s| s.hits == 0),
        "{:?}",
        small.delta_cache
    );
}

/// A parse with the default options on a rayon pool of `threads` threads
fn parse_on_threads(repo: &Repo, threads: usize) -> ParseResult {
    let git_dir = repo.git_dir();
    rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .build()
        .unwrap()
        .install(|| parse_objects(&git_dir).unwrap())
}

/// A tree entry as (child, name bytes, mode)
type Entry = (Option<usize>, Vec<u8>, u32);

/// Everything a parse gives, in a form that two parses can be compared by:
/// the container's Debug output (ObjectContainer has no PartialEq), which
/// has each commit's tree, parents and time, and each tree's entries with
/// their names as bytes, since name ids depend on which worker interned a
/// name first
fn snapshot(parsed: &ParseResult) -> (String, Vec<Vec<Entry>>) {
    let ParseResult {
        tree_entries,
        names,
        container,
        ..
    } = parsed;
    let trees = (0..container.trees.len())
        .map(|tree| {
            tree_entries
                .entries(tree)
                .map(|(child, name, kind)| (child, names.get(name).to_vec(), kind.mode()))
                .collect()
        })
        .collect();
    (format!("{container:?}"), trees)
}

/// Fixtures with deltas, several packs, loose objects, non-UTF-8 names, and
/// tags
fn fixtures() -> [Repo; 4] {
    [
        Repo::with_history(),
        Repo::with_overlapping_packs(),
        Repo::with_non_utf8(),
        Repo::with_refs_and_merges(),
    ]
}

#[test]
fn a_parse_on_8_threads_equals_a_parse_on_1_thread() {
    for repo in fixtures() {
        let one = snapshot(&parse_on_threads(&repo, 1));
        let eight = snapshot(&parse_on_threads(&repo, 8));
        assert_eq!(eight, one);
    }
}

#[test]
fn two_parses_on_the_same_thread_count_are_equal() {
    for repo in fixtures() {
        let first = snapshot(&parse_on_threads(&repo, 8));
        let second = snapshot(&parse_on_threads(&repo, 8));
        assert_eq!(second, first);
    }
}
