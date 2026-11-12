//! The index roots and the reflog entries, compared with git

use std::collections::{BTreeMap, BTreeSet};
use std::fs;

use git_dag_parser_core::parser::{parse_objects, ParseResult};
use test_fixtures::{hex, Repo};

fn parse(repo: &Repo) -> ParseResult {
    parse_objects(&repo.git_dir()).unwrap()
}

fn index_root_hashes(parsed: &ParseResult) -> BTreeSet<String> {
    let c = &parsed.container;
    c.index_roots
        .iter()
        .map(|&id| hex(&c.hash_of(id)))
        .collect()
}

/// Oracle: the hash of each index entry that is not a gitlink, from
/// `git ls-files --stage`, and each tree of the cache-tree after `git
/// write-tree` (the root tree and all its subtrees)
fn git_index_roots(repo: &Repo) -> BTreeSet<String> {
    let root = String::from_utf8(repo.git(&["write-tree"])).unwrap();
    let root = root.trim();
    let mut roots: BTreeSet<String> = String::from_utf8(repo.git(&["ls-files", "--stage"]))
        .unwrap()
        .lines()
        .filter(|l| !l.starts_with("160000 "))
        .map(|l| l.split(' ').nth(1).unwrap().to_string())
        .collect();
    roots.insert(root.to_string());
    let trees = repo.git(&["ls-tree", "-r", "-d", "--format=%(objectname)", root]);
    roots.extend(String::from_utf8(trees).unwrap().lines().map(String::from));
    roots
}

fn index_version(repo: &Repo) -> u32 {
    let data = fs::read(repo.git_dir().join("index")).unwrap();
    u32::from_be_bytes(data[4..8].try_into().unwrap())
}

#[test]
fn index_roots_match_git_for_index_versions_2_3_and_4() {
    let repo = Repo::with_history();
    let expected = git_index_roots(&repo);
    assert!(expected.len() > 5);
    assert_eq!(index_version(&repo), 2);
    assert_eq!(index_root_hashes(&parse(&repo)), expected);

    // skip-worktree is an extended flag, so git writes version 3
    repo.git(&["update-index", "--skip-worktree", "src/lib.rs"]);
    let expected = git_index_roots(&repo);
    assert_eq!(index_version(&repo), 3);
    assert_eq!(index_root_hashes(&parse(&repo)), expected);

    repo.git(&["update-index", "--index-version", "4"]);
    let expected = git_index_roots(&repo);
    assert_eq!(index_version(&repo), 4);
    assert_eq!(index_root_hashes(&parse(&repo)), expected);
}

#[test]
fn a_split_index_deletes_and_replaces_entries_of_the_shared_index() {
    let repo = Repo::with_history();
    repo.git(&["update-index", "--split-index"]);
    // Keep the shared index, so that the changes go to the bitmaps
    let keep = ["-c", "splitIndex.maxPercentChange=100"];
    let git = |args: &[&str]| repo.git(&[&keep[..], args].concat());
    let old_lib = String::from_utf8(repo.git(&["rev-parse", ":src/lib.rs"])).unwrap();
    let big = String::from_utf8(repo.git(&["rev-parse", ":big.txt"])).unwrap();
    git(&["rm", "-q", "--cached", "big.txt"]);
    repo.write("src/lib.rs", "// changed in the split index\n");
    git(&["add", "src/lib.rs"]);
    repo.write("added.txt", "added in the split index\n");
    git(&["add", "added.txt"]);

    let shared: Vec<_> = fs::read_dir(repo.git_dir())
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|n| n.starts_with("sharedindex."))
        .collect();
    assert_eq!(shared.len(), 1, "{shared:?}");
    let shared = fs::read(repo.git_dir().join(&shared[0])).unwrap();
    assert!(
        shared.windows(7).any(|w| w == b"big.txt"),
        "the shared index must still have the deleted entry"
    );

    let parsed = parse(&repo);
    let got = index_root_hashes(&parsed);
    let expected: BTreeSet<String> = String::from_utf8(repo.git(&["ls-files", "--stage"]))
        .unwrap()
        .lines()
        .map(|l| l.split(' ').nth(1).unwrap().to_string())
        .collect();
    let blobs: BTreeSet<String> = parsed
        .container
        .index_roots
        .iter()
        .filter(|id| matches!(id, git_dag_parser_core::models::ObjectId::Blob(_)))
        .map(|&id| hex(&parsed.container.hash_of(id)))
        .collect();
    assert_eq!(blobs, expected);
    assert!(!got.contains(big.trim()), "deleted from the index");
    assert!(!got.contains(old_lib.trim()), "replaced in the index");
}

#[test]
fn resolve_undo_blobs_of_a_resolved_conflict_are_index_roots() {
    let repo = Repo::new();
    repo.write("f.txt", "base\n");
    repo.commit("base");
    repo.git(&["checkout", "-q", "-b", "side"]);
    repo.write("f.txt", "theirs\n");
    repo.commit("theirs");
    repo.git(&["checkout", "-q", "main"]);
    repo.write("f.txt", "ours\n");
    repo.commit("ours");
    let out = std::process::Command::new("git")
        .current_dir(&repo.work)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args([
            "-c",
            "user.name=T",
            "-c",
            "user.email=t@e",
            "merge",
            "-q",
            "side",
        ])
        .output()
        .unwrap();
    assert!(!out.status.success(), "the merge must conflict");
    let stages: Vec<String> = ["1", "2", "3"]
        .iter()
        .map(|s| {
            let hash = repo.git(&["rev-parse", &format!(":{s}:f.txt")]);
            String::from_utf8(hash).unwrap().trim().to_string()
        })
        .collect();
    repo.write("f.txt", "resolved\n");
    repo.git(&["add", "f.txt"]);

    let got = index_root_hashes(&parse(&repo));
    for (stage, hash) in stages.iter().enumerate() {
        assert!(got.contains(hash), "stage {} of f.txt", stage + 1);
    }
    let resolved = String::from_utf8(repo.git(&["rev-parse", ":f.txt"])).unwrap();
    assert!(got.contains(resolved.trim()));
}

#[test]
fn a_bare_repository_has_no_index_roots() {
    let repo = Repo::with_history();
    let bare = repo.work.parent().unwrap().join("bare.git");
    repo.git(&["clone", "-q", "--bare", ".", bare.to_str().unwrap()]);
    let parsed = parse_objects(&bare).unwrap();
    assert!(parsed.container.index_roots.is_empty());
}

#[test]
fn reflog_entries_match_git_reflog_show() {
    let (repo, _) = Repo::with_reachability();
    let parsed = parse(&repo);
    let c = &parsed.container;

    // Oracle: the new hash of each entry of each log, newest first
    let mut expected: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let out = repo.git(&["reflog", "show", "--all", "--format=%gD %H"]);
    for line in String::from_utf8(out).unwrap().lines() {
        let (selector, hash) = line.split_once(' ').unwrap();
        let (name, n) = selector.trim_end_matches('}').split_once("@{").unwrap();
        let n: usize = n.parse().unwrap();
        let log = expected.entry(name.to_string()).or_default();
        if log.len() <= n {
            log.resize(n + 1, String::new());
        }
        log[n] = hash.to_string();
    }
    assert!(expected.contains_key("HEAD") && expected.contains_key("refs/stash"));

    let mut got: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for e in &c.reflog_entries {
        let name = String::from_utf8(e.ref_name.clone()).unwrap();
        got.entry(name).or_default().insert(0, hex(&e.new_hash));
    }
    assert_eq!(got, expected);

    for e in &c.reflog_entries {
        assert_eq!(e.new_target.map(|id| c.hash_of(id)), Some(e.new_hash));
        match e.old_target {
            Some(id) => assert_eq!(c.hash_of(id), e.old_hash),
            None => assert_eq!(e.old_hash, [0; 20], "the old commit is in the store"),
        }
        assert!(e.time > 1_700_000_000);
        assert!(!e.message.is_empty());
    }
}
