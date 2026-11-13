//! The path where the owner commit added each tree and blob, compared with
//! git

use std::collections::HashMap;

use git_dag_parser::attribution::Attribution;
use git_dag_parser::paths::Paths;
use git_dag_parser_core::parser::{parse_objects, ParseResult};
use test_fixtures::{hex, Repo};

fn calculate(repo: &Repo) -> (ParseResult, Attribution, Paths) {
    let parsed = parse_objects(&repo.git_dir()).unwrap();
    let attribution = Attribution::calculate(&parsed).unwrap();
    let paths = Paths::calculate(&parsed, &attribution);
    (parsed, attribution, paths)
}

/// Every path of every tree and blob in the tree of `commit`, by hex hash,
/// from `git ls-tree -r -t`. The root tree is not in the list.
fn git_paths(repo: &Repo, commit: &str) -> HashMap<String, Vec<Vec<u8>>> {
    let out = repo.git(&["ls-tree", "-r", "-t", "-z", "--full-tree", commit]);
    let mut paths: HashMap<String, Vec<Vec<u8>>> = HashMap::new();
    for record in out.split(|&b| b == 0).filter(|r| !r.is_empty()) {
        // <mode> SP <type> SP <hash> TAB <path>
        let tab = record.iter().position(|&b| b == b'\t').unwrap();
        let fields = String::from_utf8(record[..tab].to_vec()).unwrap();
        let fields: Vec<&str> = fields.split(' ').collect();
        if fields[1] != "commit" {
            let path = record[tab + 1..].to_vec();
            paths.entry(fields[2].to_string()).or_default().push(path);
        }
    }
    paths
}

/// Compare `introduced_at_path` of each object with git. An object at two
/// paths in its owner commit is skipped, because git can select the other
/// path. Returns the number of objects compared.
fn compare_with_git(repo: &Repo) -> usize {
    let (parsed, a, paths) = calculate(repo);
    let c = &parsed.container;
    let mut by_commit: HashMap<u32, HashMap<String, Vec<Vec<u8>>>> = HashMap::new();
    let mut compared = 0;
    let objects = c
        .trees
        .iter()
        .map(|t| {
            (
                t.hash,
                a.tree_introduced_by(t.index),
                paths.tree_path(t.index),
            )
        })
        .chain(c.blobs.iter().map(|b| {
            (
                b.hash,
                a.blob_introduced_by(b.index),
                paths.blob_path(b.index),
            )
        }));
    for (hash, owner, path) in objects {
        let Some(owner) = owner else {
            assert_eq!(path, None, "{} has no owner", hex(&hash));
            continue;
        };
        let path = path.unwrap_or_else(|| panic!("{} has an owner, but no path", hex(&hash)));
        let got = paths.bytes(path, &parsed.names);
        let commit = &c.commits[owner as usize];
        if commit.tree.is_some_and(|t| c.trees[t].hash == hash) {
            assert_eq!(got, b"", "{} is the root tree of its owner", hex(&hash));
            compared += 1;
            continue;
        }
        let git = by_commit
            .entry(owner)
            .or_insert_with(|| git_paths(repo, &hex(&commit.hash)));
        let want = &git[&hex(&hash)];
        if want.len() == 1 {
            assert_eq!(
                String::from_utf8_lossy(&got),
                String::from_utf8_lossy(&want[0]),
                "path of {}",
                hex(&hash)
            );
            compared += 1;
        }
    }
    compared
}

#[test]
fn introduced_at_path_matches_git_ls_tree() {
    for repo in [
        Repo::with_attribution_history(),
        Repo::with_history(),
        Repo::with_refs_and_merges(),
        Repo::with_non_utf8(),
        Repo::with_reachability().0,
    ] {
        assert!(compare_with_git(&repo) > 5);
    }
}

#[test]
fn a_moved_file_keeps_the_path_where_it_was_added() {
    let repo = Repo::with_attribution_history();
    let (parsed, _, paths) = calculate(&repo);
    let blob = String::from_utf8(repo.git(&["rev-parse", "HEAD:c.txt"])).unwrap();
    let b = parsed
        .container
        .blobs
        .iter()
        .find(|b| hex(&b.hash) == blob.trim())
        .unwrap();
    let path = paths.blob_path(b.index).unwrap();
    assert_eq!(paths.bytes(path, &parsed.names), b"a.txt");
}

#[test]
fn the_path_table_is_sorted_by_bytes_and_each_parent_is_a_prefix() {
    let (parsed, _, paths) = calculate(&Repo::with_history());
    let bytes: Vec<Vec<u8>> = (0..paths.paths.len() as u32)
        .map(|id| paths.bytes(id, &parsed.names))
        .collect();
    assert_eq!(bytes[0], b"");
    assert!(
        bytes.windows(2).all(|w| w[0] < w[1]),
        "sorted, no duplicates"
    );
    for (id, path) in paths.paths.iter().enumerate().skip(1) {
        let parent = path.parent.unwrap() as usize;
        let name = parsed.names.get(path.name.unwrap());
        let mut want = bytes[parent].clone();
        if parent != 0 {
            want.push(b'/');
        }
        want.extend_from_slice(name);
        assert_eq!(bytes[id], want);
        assert_eq!(path.depth, paths.paths[parent].depth + 1);
    }
    let used = paths
        .tree_path
        .iter()
        .chain(&paths.blob_path)
        .filter(|&&p| p != git_dag_parser::paths::NO_PATH)
        .collect::<std::collections::HashSet<_>>();
    // Each path is the path of an object or the parent of a path
    assert!(used.len() <= paths.paths.len());
    assert!(paths.paths.len() > 10);
}

#[test]
fn objects_that_only_tags_reach_have_no_path() {
    let repo = Repo::with_attribution_history();
    let (parsed, _, paths) = calculate(&repo);
    let only = String::from_utf8(repo.git(&["rev-parse", "tree-tag^{}:only.txt"])).unwrap();
    let b = parsed
        .container
        .blobs
        .iter()
        .find(|b| hex(&b.hash) == only.trim())
        .unwrap();
    assert_eq!(paths.blob_path(b.index), None);
}
