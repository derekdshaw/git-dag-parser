//! The liveness of each object, compared with git rev-list

use std::collections::{HashMap, HashSet};

use git_dag_parser::attribution::Attribution;
use git_dag_parser::reachability::{Liveness, Reachability};
use git_dag_parser_core::parser::{parse_objects, ParseResult};
use test_fixtures::{hex, Repo};

fn calculate(repo: &Repo) -> (ParseResult, Reachability) {
    let parsed = parse_objects(&repo.git_dir()).unwrap();
    let attribution = Attribution::calculate(&parsed).unwrap();
    let reachability = Reachability::calculate(&parsed, &attribution);
    (parsed, reachability)
}

/// The liveness of each object by hex hash
fn liveness_by_hash(parsed: &ParseResult, r: &Reachability) -> HashMap<String, Liveness> {
    let c = &parsed.container;
    let commits = c.commits.iter().map(|o| (hex(&o.hash), r.commit[o.index]));
    let trees = c.trees.iter().map(|o| (hex(&o.hash), r.tree[o.index]));
    let blobs = c.blobs.iter().map(|o| (hex(&o.hash), r.blob[o.index]));
    let tags = c.tags.iter().map(|o| (hex(&o.hash), r.tag[o.index]));
    commits.chain(trees).chain(blobs).chain(tags).collect()
}

fn rev_list(repo: &Repo, extra: &[&str]) -> HashSet<String> {
    let mut args = vec!["rev-list", "--objects", "--no-object-names", "--all"];
    args.extend_from_slice(extra);
    String::from_utf8(repo.git(&args))
        .unwrap()
        .lines()
        .map(String::from)
        .collect()
}

/// Oracle: `Ref` from `rev-list --all`, then `IndexOnly` from
/// `--indexed-objects`, then `ReflogOnly` from `--reflog`
fn git_liveness(repo: &Repo, objects: impl Iterator<Item = String>) -> HashMap<String, Liveness> {
    let refs = rev_list(repo, &[]);
    let index = rev_list(repo, &["--indexed-objects"]);
    let reflog = rev_list(repo, &["--indexed-objects", "--reflog"]);
    objects
        .map(|o| {
            let l = if refs.contains(&o) {
                Liveness::Ref
            } else if index.contains(&o) {
                Liveness::IndexOnly
            } else if reflog.contains(&o) {
                Liveness::ReflogOnly
            } else {
                Liveness::Unreachable
            };
            (o, l)
        })
        .collect()
}

fn assert_matches_git(repo: &Repo) -> HashMap<String, Liveness> {
    let (parsed, r) = calculate(repo);
    let got = liveness_by_hash(&parsed, &r);
    let want = git_liveness(repo, got.keys().cloned());
    let mut diff: Vec<String> = got
        .iter()
        .filter(|(o, l)| want[*o] != **l)
        .map(|(o, l)| format!("{o}: got {l:?}, git {:?}", want[o]))
        .collect();
    diff.sort();
    assert!(diff.is_empty(), "{}", diff.join("\n"));
    got
}

#[test]
fn liveness_matches_git_rev_list() {
    for repo in [
        Repo::with_refs_and_merges(),
        Repo::with_attribution_history(),
        Repo::with_cruft_pack(),
        Repo::with_history(),
    ] {
        assert_matches_git(&repo);
    }
}

#[test]
fn amend_reset_stash_drop_hash_object_and_add_give_each_state() {
    let (repo, named) = Repo::with_reachability();
    let indexed = rev_list(&repo, &["--indexed-objects"]);
    assert!(
        indexed.contains(&named["index tree"]),
        "git write-tree must leave a valid cache-tree"
    );

    let got = assert_matches_git(&repo);
    let state = |name: &str| got[&named[name]];
    assert_eq!(state("amended"), Liveness::ReflogOnly);
    assert_eq!(state("reset"), Liveness::ReflogOnly);
    assert_eq!(state("reset blob"), Liveness::ReflogOnly);
    assert_eq!(state("dropped stash"), Liveness::Unreachable);
    assert_eq!(state("dropped stash blob"), Liveness::Unreachable);
    assert_eq!(state("dangling blob"), Liveness::Unreachable);
    assert_eq!(state("index blob"), Liveness::IndexOnly);
    assert_eq!(state("index tree"), Liveness::IndexOnly);
    let new_dir =
        String::from_utf8(repo.git(&["rev-parse", &format!("{}:new", named["index tree"])]))
            .unwrap();
    assert_eq!(got[new_dir.trim()], Liveness::IndexOnly);
    for l in Liveness::ALL {
        assert!(got.values().any(|&g| g == l), "no object is {l:?}");
    }
}

#[test]
fn totals_count_every_object_one_time() {
    let (repo, _) = Repo::with_reachability();
    let (parsed, r) = calculate(&repo);
    let c = &parsed.container;
    let totals = r.totals(c);
    let count: usize = totals.iter().map(|t| t.count).sum();
    assert_eq!(
        count,
        c.commits.len() + c.trees.len() + c.blobs.len() + c.tags.len()
    );
    let disk: u64 = totals.iter().map(|t| t.size_disk).sum();
    let all: u64 = c.commits.iter().map(|o| o.size_disk).sum::<u64>()
        + c.trees.iter().map(|o| o.size_disk).sum::<u64>()
        + c.blobs.iter().map(|o| o.size_disk).sum::<u64>()
        + c.tags.iter().map(|o| o.size_disk).sum::<u64>();
    assert_eq!(disk, all);
}
