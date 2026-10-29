//! Compares a full parse of real repositories with git. Ignored by default:
//! it needs repositories outside the test fixtures, and large ones take
//! minutes. Use a release build:
//!
//! ```text
//! GDP_ORACLE_REPOS=~/r/kaju:~/perfrepos/zed \
//!     cargo test --release --test git_oracle -- --ignored --nocapture
//! ```
//!
//! Paths are separated by `:`. Each one can be a work tree or a `.git`
//! directory. Git commands run with `GIT_OPTIONAL_LOCKS=0`, so nothing is
//! written to the repositories.

mod common;

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use common::{hex, parsed_objects};
use git_dag_parser::models::{ObjectContainer, RefKind};
use git_dag_parser::parser::parse_objects;

/// Mismatches shown for each check before the rest are only counted
const SHOW: usize = 10;

fn git(git_dir: &Path, args: &[&str], stdin: Option<Vec<u8>>) -> Vec<u8> {
    let mut child = Command::new("git")
        .arg("--git-dir")
        .arg(git_dir)
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("git must be installed");
    // Feed stdin from another thread so a large output cannot block the input
    let mut input = child.stdin.take().unwrap();
    let writer = std::thread::spawn(move || {
        if let Some(data) = stdin {
            input.write_all(&data).unwrap();
        }
    });
    let out = child.wait_with_output().unwrap();
    writer.join().unwrap();
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

fn git_dir_of(path: &Path) -> PathBuf {
    if path.join(".git").is_dir() {
        path.join(".git")
    } else {
        path.to_path_buf()
    }
}

/// Collects mismatches for one check, keeping the first few for the report
#[derive(Default)]
struct Check {
    count: usize,
    examples: Vec<String>,
}

impl Check {
    fn fail(&mut self, message: String) {
        self.count += 1;
        if self.examples.len() < SHOW {
            self.examples.push(message);
        }
    }

    fn report(&self, name: &str, out: &mut Vec<String>) {
        if self.count > 0 {
            out.push(format!("{name}: {} mismatches", self.count));
            out.extend(self.examples.iter().map(|e| format!("  {e}")));
        }
    }
}

/// Object types and sizes. `size_disk` must match for objects stored once;
/// for an object stored in several packs, git and the parser may measure
/// different copies, so those are only counted.
fn check_objects(git_dir: &Path, c: &ObjectContainer, out: &mut Vec<String>) {
    let listing = git(
        git_dir,
        &[
            "cat-file",
            "--batch-all-objects",
            "--batch-check=%(objectname) %(objecttype) %(objectsize) %(objectsize:disk)",
        ],
        None,
    );
    let ours = parsed_objects(c);
    let mut copies: HashMap<String, usize> = HashMap::new();
    for copy in &c.copies {
        *copies.entry(hex(&c.hash_of(copy.object()))).or_default() += 1;
    }

    let mut check = Check::default();
    let mut seen = HashSet::new();
    let mut duplicate_disk_differs = 0;
    for line in String::from_utf8(listing).unwrap().lines() {
        let f: Vec<&str> = line.split(' ').collect();
        let (hash, kind) = (f[0], f[1]);
        let size: u64 = f[2].parse().unwrap();
        let size_disk: u64 = f[3].parse().unwrap();
        seen.insert(hash.to_string());
        let Some(o) = ours.get(hash) else {
            check.fail(format!("{hash}: {kind} missing from the parse"));
            continue;
        };
        if o.kind != kind || o.size != size {
            check.fail(format!(
                "{hash}: git {kind} {size}, parsed {} {}",
                o.kind, o.size
            ));
        } else if o.size_disk != size_disk {
            if copies.get(hash).copied().unwrap_or(0) > 1 {
                duplicate_disk_differs += 1;
            } else {
                check.fail(format!(
                    "{hash}: size_disk git {size_disk}, parsed {}",
                    o.size_disk
                ));
            }
        }
    }
    for hash in ours.keys().filter(|h| !seen.contains(*h)) {
        check.fail(format!("{hash}: parsed but not in git"));
    }
    println!(
        "  objects: {} checked; {} stored more than once, {} of them with a different size_disk copy than git",
        seen.len(),
        copies.values().filter(|&&n| n > 1).count(),
        duplicate_disk_differs
    );
    check.report("objects", out);
}

/// Tree, parents (in order), and committer time of every commit
fn check_commits(git_dir: &Path, c: &ObjectContainer, out: &mut Vec<String>) {
    let input: String = c.commits.iter().map(|k| hex(&k.hash) + "\n").collect();
    let log = git(
        git_dir,
        &[
            "--no-replace-objects",
            "log",
            "--no-walk=unsorted",
            "--stdin",
            "--format=%H %T %ct %P",
        ],
        Some(input.into_bytes()),
    );
    let expected: HashMap<&str, &str> = std::str::from_utf8(&log)
        .unwrap()
        .lines()
        .map(|l| (&l[..40], l[41..].trim_end()))
        .collect();

    let mut check = Check::default();
    for k in &c.commits {
        let tree = k.tree.map(|t| hex(&c.trees[t].hash)).unwrap_or_default();
        let mut ours = format!("{tree} {}", k.commit_time);
        for &p in &k.parents {
            ours.push(' ');
            ours.push_str(&hex(&c.commits[p].hash));
        }
        let hash = hex(&k.hash);
        match expected.get(hash.as_str()) {
            Some(&want) if want == ours => {}
            want => check.fail(format!("{hash}: git {want:?}, parsed {ours:?}")),
        }
    }
    println!("  commits: {} checked", c.commits.len());
    check.report("commits", out);
}

/// Every direct ref and its target. HEAD and symbolic refs are left out,
/// as `for-each-ref` reports them.
fn check_refs(git_dir: &Path, c: &ObjectContainer, out: &mut Vec<String>) {
    let listing = git(
        git_dir,
        &[
            "for-each-ref",
            "--format=%(refname) %(objectname) %(symref)",
        ],
        None,
    );
    let expected: HashMap<String, String> = String::from_utf8(listing)
        .unwrap()
        .lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            (f.len() == 2).then(|| (f[0].to_string(), f[1].to_string()))
        })
        .collect();
    let ours: HashMap<String, String> = c
        .refs
        .iter()
        .filter(|r| r.kind != RefKind::Head)
        .map(|r| {
            let target = r.target.map(|t| hex(&c.hash_of(t))).unwrap_or_default();
            (String::from_utf8_lossy(&r.name).into_owned(), target)
        })
        .collect();

    let mut check = Check::default();
    for (name, want) in &expected {
        if ours.get(name) != Some(want) {
            check.fail(format!("{name}: git {want}, parsed {:?}", ours.get(name)));
        }
    }
    for name in ours.keys().filter(|n| !expected.contains_key(*n)) {
        check.fail(format!("{name}: parsed but not in git"));
    }
    println!("  refs: {} checked", expected.len());
    check.report("refs", out);
}

/// Name and target of every annotated tag object, read with one
/// `git cat-file --batch` call
fn check_tags(git_dir: &Path, c: &ObjectContainer, out: &mut Vec<String>) {
    if c.tags.is_empty() {
        println!("  tags: none");
        return;
    }
    let input: String = c.tags.iter().map(|t| hex(&t.hash) + "\n").collect();
    let batch = git(git_dir, &["cat-file", "--batch"], Some(input.into_bytes()));

    // Each object is `<hash> <type> <size>\n<content>\n`
    let mut expected: HashMap<String, (Vec<u8>, String)> = HashMap::new();
    let mut pos = 0;
    while pos < batch.len() {
        let eol = pos + batch[pos..].iter().position(|&b| b == b'\n').unwrap();
        let header = std::str::from_utf8(&batch[pos..eol]).unwrap();
        let f: Vec<&str> = header.split(' ').collect();
        let size: usize = f[2].parse().unwrap();
        let body = &batch[eol + 1..eol + 1 + size];
        let mut name = Vec::new();
        let mut target = String::new();
        for line in body.split(|&b| b == b'\n').take_while(|l| !l.is_empty()) {
            if let Some(t) = line.strip_prefix(b"object ") {
                target = String::from_utf8_lossy(t).into_owned();
            } else if let Some(n) = line.strip_prefix(b"tag ") {
                name = n.to_vec();
            }
        }
        expected.insert(f[0].to_string(), (name, target));
        pos = eol + 1 + size + 1;
    }

    let mut check = Check::default();
    for t in &c.tags {
        let hash = hex(&t.hash);
        let target = t.target.map(|id| hex(&c.hash_of(id))).unwrap_or_default();
        let ours = (t.name.clone(), target);
        if expected.get(&hash) != Some(&ours) {
            check.fail(format!(
                "{hash}: git {:?}, parsed {ours:?}",
                expected.get(&hash)
            ));
        }
    }
    println!("  tags: {} checked", c.tags.len());
    check.report("tags", out);
}

/// Each kind is in hash order and each index is the object's position
fn check_index_order(c: &ObjectContainer, out: &mut Vec<String>) {
    let mut check = Check::default();
    let kinds: [(&str, Vec<_>); 4] = [
        (
            "commit",
            c.commits.iter().map(|o| (o.index, o.hash)).collect(),
        ),
        ("tree", c.trees.iter().map(|o| (o.index, o.hash)).collect()),
        ("blob", c.blobs.iter().map(|o| (o.index, o.hash)).collect()),
        ("tag", c.tags.iter().map(|o| (o.index, o.hash)).collect()),
    ];
    for (kind, objects) in &kinds {
        if !objects
            .iter()
            .enumerate()
            .all(|(pos, &(index, _))| pos == index)
        {
            check.fail(format!("{kind}: an index differs from its position"));
        }
        if !objects.windows(2).all(|w| w[0].1 < w[1].1) {
            check.fail(format!("{kind}: not in hash order"));
        }
    }
    check.report("index order", out);
}

#[test]
#[ignore = "needs GDP_ORACLE_REPOS; run with --ignored"]
fn parse_matches_git_on_real_repositories() {
    let repos = std::env::var("GDP_ORACLE_REPOS")
        .expect("set GDP_ORACLE_REPOS to repository paths separated by ':'");
    let mut failures = Vec::new();

    for repo in repos.split(':').filter(|r| !r.is_empty()) {
        let git_dir = git_dir_of(Path::new(repo));
        println!("{repo}");
        let (_, _, _, _, container) =
            parse_objects(&git_dir).unwrap_or_else(|e| panic!("parse of {repo} failed: {e:#}"));

        let mut out = Vec::new();
        check_objects(&git_dir, &container, &mut out);
        check_commits(&git_dir, &container, &mut out);
        check_refs(&git_dir, &container, &mut out);
        check_tags(&git_dir, &container, &mut out);
        check_index_order(&container, &mut out);

        if out.is_empty() {
            println!("  ok");
        } else {
            failures.push(format!("{repo}\n{}", out.join("\n")));
        }
    }

    assert!(failures.is_empty(), "\n{}", failures.join("\n"));
}
