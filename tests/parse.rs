mod common;

use std::collections::HashMap;

use common::{count_by_kind, parsed_objects, Repo};
use git_dag_parser::parser::parse_objects;

#[test]
fn every_object_matches_git_type_and_sizes() {
    let repo = Repo::with_history();
    assert!(
        repo.deltified_count() > 0,
        "fixture must contain deltified objects for this test to mean anything"
    );

    let (_, _, hash_index, container) = parse_objects(&repo.git_dir()).unwrap();

    let expected = repo.git_objects();
    let actual = parsed_objects(&hash_index, &container);
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
