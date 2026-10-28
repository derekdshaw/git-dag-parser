mod common;

use std::collections::{HashMap, HashSet};
use std::fs;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use common::{count_by_kind, hex, parsed_objects, Repo};
use git_dag_parser::models::ObjectId;
use git_dag_parser::parser::parse_objects;

#[test]
fn every_object_matches_git_type_and_sizes() {
    let repo = Repo::with_history();
    assert!(
        repo.deltified_count() > 0,
        "fixture must contain deltified objects for this test to mean anything"
    );

    let (_, _, _, container) = parse_objects(&repo.git_dir()).unwrap();

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
    assert!(repo.pack_count() >= 2, "fixture must have overlapping packs");

    let (_, _, _, container) = parse_objects(&repo.git_dir()).unwrap();

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

    let (commits_raw, trees_raw, _, container) = parse_objects(&repo.git_dir()).unwrap();

    assert_eq!(
        commits_raw.len(),
        container.commits.len(),
        "every commit should have its header parsed"
    );
    assert_eq!(
        trees_raw.len(),
        container.trees.len(),
        "every tree should have its entries parsed"
    );
}

#[test]
fn hash_index_points_at_each_object() {
    let repo = Repo::with_history();

    let (_, _, hash_index, container) = parse_objects(&repo.git_dir()).unwrap();

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

    let (_, _, _, container) = parse_objects(&repo.git_dir()).unwrap();

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

    let (_, _, _, container) = parse_objects(&repo.git_dir()).unwrap();

    for pack in &container.packs {
        let in_pack: Vec<_> = container
            .copies
            .iter()
            .filter(|c| c.pack == Some(pack.index))
            .collect();
        assert_eq!(
            in_pack.len(),
            expected_counts[&pack.name],
            "copies in {}",
            pack.name
        );
        assert!(in_pack.iter().all(|c| c.offset.is_some()));
        assert!(in_pack.iter().all(|c| c.mtime == pack.mtime));
        let mut offsets: Vec<u64> = in_pack.iter().map(|c| c.offset.unwrap()).collect();
        assert!(
            offsets.windows(2).all(|w| w[0] < w[1]),
            "copies are in offset order"
        );
        offsets.dedup();
        assert_eq!(offsets.len(), in_pack.len());
    }

    let loose: Vec<_> = container.copies.iter().filter(|c| c.is_loose()).collect();
    assert_eq!(loose.len(), loose_count);
    assert!(loose.iter().all(|c| c.offset.is_none() && !c.is_delta));

    let deltas = container.copies.iter().filter(|c| c.is_delta).count();
    assert_eq!(deltas, repo.deltified_count());

    let total_objects = container.commits.len()
        + container.trees.len()
        + container.blobs.len()
        + container.tags.len();
    let with_copies: HashSet<_> = container.copies.iter().map(|c| c.object).collect();
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
        // Packs are written read-only; the owner may still set times.
        fs::File::open(path).unwrap().set_modified(time).unwrap();
    }

    let (_, _, _, container) = parse_objects(&repo.git_dir()).unwrap();

    let newest = container
        .packs
        .iter()
        .max_by_key(|p| (p.mtime, std::cmp::Reverse(&p.name)))
        .unwrap();
    assert_eq!(newest.index, container.packs.len() - 1);
    let mut primaries: HashMap<ObjectId, usize> = HashMap::new();
    for copy in container.copies.iter().filter(|c| c.is_primary) {
        *primaries.entry(copy.object).or_default() += 1;
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
        .filter(|c| c.pack == Some(newest.index))
        .collect();
    assert!(!newest_copies.is_empty());
    for copy in newest_copies {
        assert!(
            copy.is_primary,
            "{:?} should use its copy in the newest pack",
            copy.object
        );
        let size_disk = match copy.object {
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

    let (_, _, _, container) = parse_objects(&repo.git_dir()).unwrap();

    let commits: Vec<_> = container.commits.iter().map(|o| (o.index, o.hash)).collect();
    let trees: Vec<_> = container.trees.iter().map(|o| (o.index, o.hash)).collect();
    let blobs: Vec<_> = container.blobs.iter().map(|o| (o.index, o.hash)).collect();
    let tags: Vec<_> = container.tags.iter().map(|o| (o.index, o.hash)).collect();
    for (kind, objects) in [("commit", commits), ("tree", trees), ("blob", blobs), ("tag", tags)] {
        assert!(
            objects.iter().enumerate().all(|(pos, &(index, _))| pos == index),
            "{kind} index equals position"
        );
        assert!(
            objects.windows(2).all(|w| w[0].1 < w[1].1),
            "{kind}s are in hash order"
        );
    }
}

/// Hash to object id for every object in a container
fn ids_by_hash(container: &git_dag_parser::models::ObjectContainer) -> HashMap<String, ObjectId> {
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
    let (_, _, _, before) = parse_objects(&repo.git_dir()).unwrap();
    let packs_before = repo.pack_files("pack");

    // Packs all objects, including the loose ones, into one new pack
    repo.git(&["repack", "-a", "-d", "-f", "--window=0", "-q"]);
    assert_ne!(
        repo.pack_files("pack"),
        packs_before,
        "fixture must rewrite the packs"
    );
    let (_, _, _, after) = parse_objects(&repo.git_dir()).unwrap();

    assert_eq!(ids_by_hash(&after), ids_by_hash(&before));
}

#[test]
fn copy_disk_sizes_match_verify_pack() {
    let repo = Repo::with_overlapping_packs();
    let expected = repo.verify_pack_entries();

    let (_, _, _, container) = parse_objects(&repo.git_dir()).unwrap();

    let packed: Vec<_> = container.copies.iter().filter(|c| !c.is_loose()).collect();
    assert_eq!(packed.len(), expected.len());
    for copy in packed {
        let pack = &container.packs[copy.pack.unwrap()];
        let key = (pack.name.clone(), copy.offset.unwrap());
        let (hash, size_in_pack) = &expected[&key];
        assert_eq!(hex(&container.hash_of(copy.object)), *hash, "{key:?}");
        assert_eq!(copy.size_disk, *size_in_pack, "{key:?}");
    }
}

#[test]
fn cruft_pack_holds_the_unreachable_blob() {
    let repo = Repo::with_cruft_pack();
    let dangling = String::from_utf8(repo.git(&["hash-object", "dangling.txt"])).unwrap();
    let dangling = dangling.trim();

    let (_, _, _, container) = parse_objects(&repo.git_dir()).unwrap();

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
        .filter(|c| c.object == ObjectId::Blob(blob.index))
        .collect();
    assert_eq!(copies.len(), 1);
    assert_eq!(copies[0].pack, Some(cruft[0].index));
    assert!(copies[0].is_primary);
}
