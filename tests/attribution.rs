//! The commit that introduced each tree and blob, compared with git and
//! with a sequential walk

use std::collections::{HashMap, HashSet};

use git_dag_parser::attribution::Attribution;
use git_dag_parser_core::parser::{parse_objects, ParseResult};
use test_fixtures::{hex, Repo};

fn attribute(repo: &Repo) -> (ParseResult, Attribution) {
    let parsed = parse_objects(&repo.git_dir()).unwrap();
    let attribution = Attribution::calculate(&parsed).unwrap();
    (parsed, attribution)
}

/// Commit indices in rank order
fn rank_order(a: &Attribution) -> Vec<usize> {
    let mut order: Vec<usize> = (0..a.rank.len()).collect();
    order.sort_by_key(|&c| a.rank[c]);
    order
}

/// Hex hash of each tree and blob, with the hex hash of the commit that
/// introduced it, or None
fn introduced_by(parsed: &ParseResult, a: &Attribution) -> HashMap<String, Option<String>> {
    let c = &parsed.container;
    let commit = |i: Option<u32>| i.map(|i| hex(&c.commits[i as usize].hash));
    let trees = c
        .trees
        .iter()
        .map(|t| (hex(&t.hash), commit(a.tree_introduced_by(t.index))));
    let blobs = c
        .blobs
        .iter()
        .map(|b| (hex(&b.hash), commit(a.blob_introduced_by(b.index))));
    trees.chain(blobs).collect()
}

/// Oracle 2: walk each commit in rank order with a "seen" set. The first
/// commit that reaches an object introduced it. A tree that is seen is not
/// walked again: an earlier commit already reached all of it.
fn sequential_introduced_by(
    parsed: &ParseResult,
    a: &Attribution,
) -> (Vec<Option<u32>>, Vec<Option<u32>>) {
    let c = &parsed.container;
    let mut tree = vec![None; c.trees.len()];
    let mut blob = vec![None; c.blobs.len()];
    for commit in rank_order(a) {
        let mut stack: Vec<usize> = c.commits[commit].tree.into_iter().collect();
        while let Some(t) = stack.pop() {
            if tree[t].is_some() {
                continue;
            }
            tree[t] = Some(commit as u32);
            for (child, _, kind) in parsed.tree_entries.entries(t) {
                match (child, kind.is_tree()) {
                    (Some(i), true) => stack.push(i),
                    (Some(i), false) => {
                        blob[i].get_or_insert(commit as u32);
                    }
                    _ => {}
                }
            }
        }
    }
    (tree, blob)
}

#[test]
fn introduced_by_matches_git_rev_list() {
    let repo = Repo::with_attribution_history();
    let (parsed, a) = attribute(&repo);
    let commits: Vec<String> = parsed
        .container
        .commits
        .iter()
        .map(|c| hex(&c.hash))
        .collect();
    let commit_set: HashSet<&String> = commits.iter().collect();

    // Oracle 1: the objects that commit c reaches and no commit of a lower
    // rank reaches. `rev-list --objects c --not <lower>` cannot give this:
    // it takes away only the objects of the lower commits that are
    // parents of the commits it walks, not those of a sibling branch.
    let mut expected: HashMap<String, Option<String>> = introduced_by(&parsed, &a)
        .into_keys()
        .map(|object| (object, None))
        .collect();
    let mut seen = HashSet::new();
    for c in rank_order(&a) {
        let out = repo.git(&["rev-list", "--objects", "--no-object-names", &commits[c]]);
        for object in String::from_utf8(out).unwrap().lines() {
            if !commit_set.contains(&object.to_string()) && seen.insert(object.to_string()) {
                assert_eq!(
                    expected.insert(object.to_string(), Some(commits[c].clone())),
                    Some(None)
                );
            }
        }
    }
    assert_eq!(introduced_by(&parsed, &a), expected);
}

#[test]
fn introduced_by_matches_a_sequential_walk() {
    for repo in [
        Repo::with_attribution_history(),
        Repo::with_history(),
        Repo::with_refs_and_merges(),
        Repo::with_non_utf8(),
    ] {
        let (parsed, a) = attribute(&repo);
        let (tree, blob) = sequential_introduced_by(&parsed, &a);
        let trees: Vec<Option<u32>> = (0..tree.len()).map(|i| a.tree_introduced_by(i)).collect();
        let blobs: Vec<Option<u32>> = (0..blob.len()).map(|i| a.blob_introduced_by(i)).collect();
        assert_eq!(trees, tree);
        assert_eq!(blobs, blob);
    }
}

#[test]
fn a_rename_an_empty_commit_a_merge_and_a_revert_introduce_no_old_blobs() {
    let repo = Repo::with_attribution_history();
    let (parsed, a) = attribute(&repo);
    let subjects = repo.commits_by_subject();
    let index = |subject: &str| {
        let hash = &subjects[subject];
        parsed
            .container
            .commits
            .iter()
            .position(|c| &hex(&c.hash) == hash)
            .unwrap() as u32
    };
    let blobs_of = |commit: u32| {
        (0..a.blob_introduced_by.len())
            .filter(|&b| a.blob_introduced_by(b) == Some(commit))
            .count()
    };
    let trees_of = |commit: u32| {
        (0..a.tree_introduced_by.len())
            .filter(|&t| a.tree_introduced_by(t) == Some(commit))
            .count()
    };

    assert_eq!(blobs_of(index("base")), 2);
    assert_eq!(blobs_of(index("rename a to c")), 0);
    assert_eq!(
        trees_of(index("rename a to c")),
        1,
        "only the new root tree"
    );
    assert_eq!((blobs_of(index("empty")), trees_of(index("empty"))), (0, 0));
    // The side branch has the lower rank, so it introduced the shared file
    assert_eq!(blobs_of(index("same file on side")), 1);
    assert_eq!(blobs_of(index("same file on main")), 0);
    assert_eq!(blobs_of(index("merge side")), 0);
    assert_eq!(blobs_of(index("change b")), 1);
    assert_eq!(blobs_of(index("Revert \"change b\"")), 0);
}

#[test]
fn objects_that_only_tags_reach_are_not_introduced() {
    let repo = Repo::with_attribution_history();
    let (parsed, a) = attribute(&repo);
    let by_hash = introduced_by(&parsed, &a);
    for tag in ["blob-tag", "tree-tag"] {
        let peeled = String::from_utf8(repo.git(&["rev-parse", &format!("{tag}^{{}}")])).unwrap();
        assert_eq!(by_hash[peeled.trim()], None, "{tag}");
    }
    let only = String::from_utf8(repo.git(&["rev-parse", "tree-tag^{}:only.txt"])).unwrap();
    assert_eq!(by_hash[only.trim()], None);
}

#[test]
fn ranks_are_in_corrected_date_order_and_no_clock_is_wrong() {
    let repo = Repo::with_attribution_history();
    let (parsed, a) = attribute(&repo);
    let order = rank_order(&a);
    assert!(order
        .windows(2)
        .all(|w| a.corrected_date[w[0]] <= a.corrected_date[w[1]]));
    // The fixture gives each commit a later time than its parents
    assert_eq!(a.corrected_commits, 0);
    for c in &parsed.container.commits {
        assert_eq!(a.corrected_date[c.index], c.commit_time);
    }
}

#[test]
fn a_repository_with_no_commits_introduces_nothing() {
    let repo = Repo::new();
    let (_, a) = attribute(&repo);
    assert!(
        a.rank.is_empty() && a.tree_introduced_by.is_empty() && a.blob_introduced_by.is_empty()
    );

    repo.write("f.txt", "one blob\n");
    repo.git(&["hash-object", "-w", "f.txt"]);
    let (parsed, a) = attribute(&repo);
    assert_eq!(parsed.container.blobs.len(), 1);
    assert_eq!(a.blob_introduced_by(0), None);
}
