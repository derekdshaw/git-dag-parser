mod common;

use std::collections::HashMap;
use std::fs;

use common::{hex, CommitInfo, Repo};
use git_dag_parser::models::{ObjectId, RefKind};
use git_dag_parser::parser::parse_objects;

#[test]
fn commit_trees_parents_and_times_match_git() {
    let repo = Repo::with_refs_and_merges();
    let expected = repo.git_commits();
    assert!(
        expected.values().any(|c| c.parents.len() >= 2),
        "fixture must contain a merge"
    );
    assert!(
        expected.len() > repo.reachable_commit_count(),
        "fixture must contain an unreachable commit"
    );

    let (_, _, _, container) = parse_objects(&repo.git_dir()).unwrap();

    let actual: HashMap<String, CommitInfo> = container
        .commits
        .iter()
        .map(|c| {
            let info = CommitInfo {
                tree: c.tree.map(|t| hex(&container.trees[t].hash)),
                parents: c
                    .parents
                    .iter()
                    .map(|&p| hex(&container.commits[p].hash))
                    .collect(),
                commit_time: c.commit_time,
            };
            (hex(&c.hash), info)
        })
        .collect();
    assert_eq!(actual, expected);
    assert!(container.commits.iter().all(|c| !c.boundary));
}

#[test]
fn annotated_tag_names_and_targets_match_git() {
    let repo = Repo::with_refs_and_merges();
    let expected = repo.git_tags();

    let (_, _, _, container) = parse_objects(&repo.git_dir()).unwrap();

    assert!(
        container
            .tags
            .iter()
            .any(|t| matches!(t.target, Some(ObjectId::Tree(_)))),
        "fixture must contain a tag on a tree"
    );
    let actual: HashMap<String, (String, String)> = container
        .tags
        .iter()
        .map(|t| {
            let target = t
                .target
                .map(|id| hex(&container.hash_of(id)))
                .unwrap_or_default();
            (
                hex(&t.hash),
                (String::from_utf8_lossy(&t.name).into_owned(), target),
            )
        })
        .collect();
    assert_eq!(actual, expected);
}

#[test]
fn refs_match_git() {
    let repo = Repo::with_refs_and_merges();
    let expected = repo.git_refs();
    let packed = fs::read_to_string(repo.git_dir().join("packed-refs")).unwrap();
    assert!(
        !packed.contains(&format!("{} refs/heads/main", expected["refs/heads/main"])),
        "fixture must leave a stale main in packed-refs"
    );

    let (_, _, _, container) = parse_objects(&repo.git_dir()).unwrap();

    let actual: HashMap<String, String> = container
        .refs
        .iter()
        .filter(|r| r.kind != RefKind::Head)
        .map(|r| {
            let target = r
                .target
                .map(|id| hex(&container.hash_of(id)))
                .unwrap_or_default();
            (String::from_utf8_lossy(&r.name).into_owned(), target)
        })
        .collect();
    assert_eq!(actual, expected);

    let kinds: HashMap<&[u8], RefKind> = container
        .refs
        .iter()
        .map(|r| (r.name.as_slice(), r.kind))
        .collect();
    assert_eq!(kinds[b"refs/heads/feature".as_slice()], RefKind::Branch);
    assert_eq!(
        kinds[b"refs/remotes/origin/main".as_slice()],
        RefKind::Remote
    );
    assert_eq!(kinds[b"refs/tags/v1".as_slice()], RefKind::Tag);
    assert_eq!(kinds[b"refs/stash".as_slice()], RefKind::Stash);
}
