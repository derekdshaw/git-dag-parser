//! Test fixtures: small repositories built with the git CLI, and git's own
//! view of their object store to compare the parser against.

// Each test binary uses a different subset of these helpers.
#![allow(dead_code)]

use std::cell::Cell;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use git_dag_parser::models::container::ObjectHash;
use git_dag_parser::models::ObjectContainer;
use tempfile::TempDir;

/// Type and sizes of one object, as reported by `git cat-file --batch-check`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectInfo {
    pub kind: String,
    pub size: u64,
    pub size_disk: u64,
}

/// Fields of one commit as git reports them, with hashes in hex
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitInfo {
    pub tree: Option<String>,
    pub parents: Vec<String>,
    pub commit_time: i64,
}

pub struct Repo {
    _tmp: TempDir,
    pub work: PathBuf,
    msg_file: PathBuf,
    /// Advanced on every git call so each commit gets its own timestamps
    clock: Cell<i64>,
}

impl Repo {
    pub fn new() -> Self {
        let tmp = TempDir::new().unwrap();
        let work = tmp.path().join("repo");
        fs::create_dir(&work).unwrap();
        // Kept outside the work tree so `git add -A` never picks it up.
        let msg_file = tmp.path().join("msg");
        let repo = Self {
            _tmp: tmp,
            work,
            msg_file,
            clock: Cell::new(1_700_000_000),
        };
        repo.git(&["init", "-q", "-b", "main"]);
        repo
    }

    /// Several commits over a nested tree, fully repacked so that blobs and
    /// trees are deltified, plus an annotated tag and one loose commit.
    pub fn with_history() -> Self {
        let repo = Self::new();
        let mut big: String = (0..2000).map(|i| format!("line {i}\n")).collect();
        for round in 0..6 {
            big.push_str(&format!("change {round}\n"));
            repo.write("big.txt", &big);
            repo.write(
                format!("src/nested/file{round}.rs"),
                format!("fn f{round}() {{}}\n"),
            );
            repo.write("src/lib.rs", format!("// rev {round}\n"));
            repo.commit(format!("commit {round}"));
        }
        repo.git(&["tag", "-a", "v1", "-m", "release v1"]);
        repo.git(&["repack", "-a", "-d", "-f", "--depth=50", "--window=50", "-q"]);

        repo.write("loose.txt", "only in a loose object\n");
        repo.commit("loose commit");
        repo
    }

    /// `with_history`, then a second full pack written without deleting the
    /// first, so most objects exist in two packs (and some also loose).
    pub fn with_overlapping_packs() -> Self {
        let repo = Self::with_history();
        repo.git(&["repack", "-a", "-q"]);
        repo
    }

    /// A Latin-1 file name and a Latin-1 commit message, all packed.
    pub fn with_non_utf8() -> Self {
        let repo = Self::new();
        repo.write("readme.txt", "hello\n");
        repo.commit("first");
        repo.write(OsStr::from_bytes(b"caf\xe9.txt"), "latin-1 name\n");
        // Without a declared encoding git re-encodes the message to UTF-8;
        // with one it stores the bytes as given, as older repos have them.
        fs::write(&repo.msg_file, b"caf\xe9 latin-1 message\n").unwrap();
        repo.git(&["add", "-A"]);
        repo.git(&[
            "-c",
            "i18n.commitEncoding=ISO-8859-1",
            "commit",
            "-q",
            "-F",
            repo.msg_file.to_str().unwrap(),
        ]);
        repo.write("after.txt", "after\n");
        repo.commit("third");
        repo.git(&["repack", "-a", "-d", "-q"]);
        repo
    }

    /// Merged history, unreachable and multi-parent commits, and every kind
    /// of ref, with refs split between packed-refs and loose files.
    pub fn with_refs_and_merges() -> Self {
        let repo = Self::new();
        repo.write("a.txt", "1\n");
        repo.commit("base");
        repo.git(&["checkout", "-q", "-b", "feature"]);
        repo.write("b.txt", "feature\n");
        repo.commit("feature work");
        repo.git(&["checkout", "-q", "main"]);
        repo.write("a.txt", "2\n");
        repo.commit("main work");
        repo.git(&["merge", "-q", "--no-ff", "-m", "merge feature", "feature"]);

        repo.git(&["tag", "light"]);
        repo.git(&["tag", "-a", "v1", "-m", "release v1"]);
        repo.git(&["tag", "-a", "tree-tag", "-m", "tag of a tree", "HEAD^{tree}"]);
        repo.git(&["update-ref", "refs/remotes/origin/main", "HEAD"]);
        repo.git(&[
            "symbolic-ref",
            "refs/remotes/origin/HEAD",
            "refs/remotes/origin/main",
        ]);
        repo.git(&["pack-refs", "--all"]);
        repo.git(&["repack", "-a", "-d", "-q"]);

        // packed-refs now holds a stale main; the loose ref must win.
        repo.write("a.txt", "3\n");
        repo.commit("after pack-refs");
        // The commit being amended stays in the store, unreachable from refs.
        repo.git(&["commit", "-q", "--amend", "-m", "amended"]);
        // A stash is a commit with two or three parents.
        repo.write("a.txt", "dirty\n");
        repo.git(&["stash", "-q"]);
        repo
    }

    pub fn git_dir(&self) -> PathBuf {
        self.work.join(".git")
    }

    /// Run git in the work tree with a hermetic config. Panics on failure.
    pub fn git(&self, args: &[&str]) -> Vec<u8> {
        let now = self.clock.get() + 100;
        self.clock.set(now);
        // Author time differs from committer time so a parser reading the
        // wrong one is caught.
        let out = Command::new("git")
            .current_dir(&self.work)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_DATE", format!("@{} +0000", now - 30))
            .env("GIT_COMMITTER_DATE", format!("@{now} +0000"))
            .args(["-c", "user.name=Test", "-c", "user.email=test@example.com"])
            .args(["-c", "gc.auto=0", "-c", "commit.gpgsign=false"])
            .args(args)
            .output()
            .expect("git must be installed to run these tests");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        out.stdout
    }

    pub fn write(&self, rel: impl AsRef<Path>, contents: impl AsRef<[u8]>) {
        let path = self.work.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    /// Stage everything and commit. The message is raw bytes so tests can
    /// write non-UTF-8 messages.
    pub fn commit(&self, message: impl AsRef<[u8]>) {
        fs::write(&self.msg_file, message).unwrap();
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "-F", self.msg_file.to_str().unwrap()]);
    }

    /// Every object in the store, keyed by hex hash.
    pub fn git_objects(&self) -> HashMap<String, ObjectInfo> {
        let out = self.git(&[
            "cat-file",
            "--batch-all-objects",
            "--batch-check=%(objectname) %(objecttype) %(objectsize) %(objectsize:disk)",
        ]);
        String::from_utf8(out)
            .unwrap()
            .lines()
            .map(|line| {
                let f: Vec<&str> = line.split(' ').collect();
                let info = ObjectInfo {
                    kind: f[1].to_string(),
                    size: f[2].parse().unwrap(),
                    size_disk: f[3].parse().unwrap(),
                };
                (f[0].to_string(), info)
            })
            .collect()
    }

    /// Tree, parents and committer time of every commit in the store,
    /// including ones no ref reaches
    pub fn git_commits(&self) -> HashMap<String, CommitInfo> {
        let mut args = vec![
            "--no-replace-objects",
            "log",
            "--no-walk=unsorted",
            "--format=%H %T %ct %P",
        ];
        let objects = self.git_objects();
        args.extend(
            objects
                .iter()
                .filter(|(_, info)| info.kind == "commit")
                .map(|(hash, _)| hash.as_str()),
        );
        String::from_utf8(self.git(&args))
            .unwrap()
            .lines()
            .map(|line| {
                let f: Vec<&str> = line.split_whitespace().collect();
                let info = CommitInfo {
                    tree: Some(f[1].to_string()),
                    parents: f[3..].iter().map(|p| p.to_string()).collect(),
                    commit_time: f[2].parse().unwrap(),
                };
                (f[0].to_string(), info)
            })
            .collect()
    }

    /// Number of commits reachable from any ref
    pub fn reachable_commit_count(&self) -> usize {
        let out = self.git(&["rev-list", "--all", "--count"]);
        String::from_utf8(out).unwrap().trim().parse().unwrap()
    }

    /// Name and target hash of every annotated tag object, keyed by tag hash
    pub fn git_tags(&self) -> HashMap<String, (String, String)> {
        self.git_objects()
            .into_iter()
            .filter(|(_, info)| info.kind == "tag")
            .map(|(hash, _)| {
                let body = String::from_utf8(self.git(&["cat-file", "tag", &hash])).unwrap();
                let header = |key: &str| {
                    body.lines()
                        .find_map(|l| l.strip_prefix(key))
                        .unwrap()
                        .to_string()
                };
                let fields = (header("tag "), header("object "));
                (hash, fields)
            })
            .collect()
    }

    /// Target hash of every direct ref, keyed by name. Symbolic refs and
    /// HEAD are left out, as `for-each-ref` reports them.
    pub fn git_refs(&self) -> HashMap<String, String> {
        let out = self.git(&[
            "for-each-ref",
            "--format=%(refname) %(objectname) %(symref)",
        ]);
        String::from_utf8(out)
            .unwrap()
            .lines()
            .filter_map(|line| {
                let f: Vec<&str> = line.split_whitespace().collect();
                (f.len() == 2).then(|| (f[0].to_string(), f[1].to_string()))
            })
            .collect()
    }

    pub fn pack_count(&self) -> usize {
        self.pack_files("pack").len()
    }

    /// Number of pack entries stored as deltas, across all packs.
    pub fn deltified_count(&self) -> usize {
        self.pack_files("idx")
            .iter()
            .map(|idx| {
                let out = self.git(&["verify-pack", "-v", idx.to_str().unwrap()]);
                // Delta entries have two extra fields: chain depth and base hash.
                String::from_utf8(out)
                    .unwrap()
                    .lines()
                    .filter(|line| {
                        let f: Vec<&str> = line.split_whitespace().collect();
                        f.len() == 7 && f[0].len() == 40
                    })
                    .count()
            })
            .sum()
    }

    fn pack_files(&self, ext: &str) -> Vec<PathBuf> {
        fs::read_dir(self.git_dir().join("objects/pack"))
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.extension() == Some(OsStr::new(ext)))
            .collect()
    }
}

pub fn hex(hash: &ObjectHash) -> String {
    hash.iter().map(|b| format!("{b:02x}")).collect()
}

/// The parser's view of every object, keyed by each object's own hash, in
/// the same shape as `Repo::git_objects`.
pub fn parsed_objects(container: &ObjectContainer) -> HashMap<String, ObjectInfo> {
    let info = |kind: &str, size: u64, size_disk: u64| ObjectInfo {
        kind: kind.to_string(),
        size,
        size_disk,
    };
    let mut objects = HashMap::new();
    for c in &container.commits {
        objects.insert(hex(&c.hash), info("commit", c.size, c.size_disk));
    }
    for t in &container.trees {
        objects.insert(hex(&t.hash), info("tree", t.size, t.size_disk));
    }
    for b in &container.blobs {
        objects.insert(hex(&b.hash), info("blob", b.size, b.size_disk));
    }
    for t in &container.tags {
        objects.insert(hex(&t.hash), info("tag", t.size, t.size_disk));
    }
    objects
}

pub fn count_by_kind(objects: &HashMap<String, ObjectInfo>) -> HashMap<String, usize> {
    let mut counts = HashMap::new();
    for info in objects.values() {
        *counts.entry(info.kind.clone()).or_insert(0) += 1;
    }
    counts
}
