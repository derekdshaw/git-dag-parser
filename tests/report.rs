//! The report passes of `init --report`, compared with git

use std::collections::{HashMap, HashSet};
use std::fs;

use git_dag_parser::attribution::Attribution;
use git_dag_parser::paths::Paths;
use git_dag_parser::report;
use git_dag_parser_core::parser::parse_objects;
use test_fixtures::Repo;

/// Size and paths of each blob in any commit, from `git ls-tree -r -l`
fn git_blobs(repo: &Repo) -> HashMap<String, (u64, HashSet<Vec<u8>>)> {
    let commits = String::from_utf8(repo.git(&["rev-list", "--all"])).unwrap();
    let mut blobs: HashMap<String, (u64, HashSet<Vec<u8>>)> = HashMap::new();
    for commit in commits.lines() {
        let out = repo.git(&["ls-tree", "-r", "-l", "-z", "--full-tree", commit]);
        for record in out.split(|&b| b == 0).filter(|r| !r.is_empty()) {
            // <mode> SP blob SP <hash> SP+ <size> TAB <path>
            let tab = record.iter().position(|&b| b == b'\t').unwrap();
            let fields = String::from_utf8(record[..tab].to_vec()).unwrap();
            let fields: Vec<&str> = fields.split_whitespace().collect();
            if fields[1] == "blob" {
                let blob = blobs.entry(fields[2].to_string()).or_default();
                blob.0 = fields[3].parse().unwrap();
                blob.1.insert(record[tab + 1..].to_vec());
            }
        }
    }
    blobs
}

#[test]
fn a_subtree_total_is_the_sum_of_the_blobs_below_the_directory() {
    let repo = Repo::with_history();
    let blobs = git_blobs(&repo);
    // Then the path of each blob does not depend on its owner commit
    assert!(blobs.values().all(|(_, paths)| paths.len() == 1));

    let parsed = parse_objects(&repo.git_dir()).unwrap();
    let attribution = Attribution::calculate(&parsed).unwrap();
    let paths = Paths::calculate(&parsed, &attribution);
    let dirs = report::directories(&parsed, &paths);

    let mut compared = 0;
    for (p, got) in dirs.subtree.iter().enumerate() {
        let dir = paths.bytes(p as u32, &parsed.names);
        let mut prefix = dir.clone();
        if !prefix.is_empty() {
            prefix.push(b'/');
        }
        let below = blobs
            .values()
            .filter(|(_, paths)| paths.iter().all(|path| path.starts_with(&prefix)));
        let (count, bytes) = below.fold((0, 0), |(n, b), (size, _)| (n + 1, b + size));
        assert_eq!(
            (got.blobs, got.blob_bytes),
            (count, bytes),
            "{}",
            String::from_utf8_lossy(&dir)
        );
        compared += usize::from(count > 0);
    }
    // The root, src and src/nested
    assert_eq!(compared, 3);
    assert!(dirs.subtree[0].trees >= dirs.direct[0].trees);
}

#[test]
fn the_overhead_is_the_disk_size_of_the_copies_that_are_not_primary() {
    let repo = Repo::with_overlapping_packs();
    let objects = repo.git_dir().join("objects");

    // Every copy on disk: (hash, size on disk, is loose)
    let mut copies: Vec<(String, u64, bool)> = repo
        .verify_pack_entries()
        .into_values()
        .map(|(hash, size)| (hash, size, false))
        .collect();
    for dir in fs::read_dir(&objects).unwrap() {
        let dir = dir.unwrap().path();
        let prefix = dir.file_name().unwrap().to_string_lossy().into_owned();
        if dir.is_dir() && prefix.len() == 2 {
            for file in fs::read_dir(&dir).unwrap() {
                let file = file.unwrap();
                let hash = format!("{prefix}{}", file.file_name().to_string_lossy());
                copies.push((hash, file.metadata().unwrap().len(), true));
            }
        }
    }
    let packed: HashSet<&str> = copies
        .iter()
        .filter(|c| !c.2)
        .map(|c| c.0.as_str())
        .collect();
    let loose_packed: Vec<_> = copies
        .iter()
        .filter(|c| c.2 && packed.contains(c.0.as_str()))
        .collect();
    assert!(
        !loose_packed.is_empty(),
        "the fixture has a loose copy of a packed object"
    );

    let parsed = parse_objects(&repo.git_dir()).unwrap();
    let c = &parsed.container;
    let distinct = c.commits.len() + c.trees.len() + c.blobs.len() + c.tags.len();
    let primary_disk: u64 = c.commits.iter().map(|o| o.size_disk).sum::<u64>()
        + c.trees.iter().map(|o| o.size_disk).sum::<u64>()
        + c.blobs.iter().map(|o| o.size_disk).sum::<u64>()
        + c.tags.iter().map(|o| o.size_disk).sum::<u64>();

    assert_eq!(
        report::overhead(c),
        report::Overhead {
            extra_copies: (copies.len() - distinct) as u64,
            extra_disk: copies.iter().map(|c| c.1).sum::<u64>() - primary_disk,
            loose_packed: loose_packed.len() as u64,
            loose_packed_disk: loose_packed.iter().map(|c| c.1).sum(),
        }
    );
}
