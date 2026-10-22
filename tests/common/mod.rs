//! Test fixtures: small repositories built with the git CLI, and git's own
//! view of their object store to compare the parser against.

// Each test binary uses a different subset of these helpers.
#![allow(dead_code)]

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use git_dag_parser::models::container::{HashIndex, ObjectHash};
use git_dag_parser::models::ObjectContainer;
use tempfile::TempDir;

/// Type and sizes of one object, as reported by `git cat-file --batch-check`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectInfo {
    pub kind: String,
    pub size: u64,
    pub size_disk: u64,
}

pub struct Repo {
    _tmp: TempDir,
    pub work: PathBuf,
    msg_file: PathBuf,
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

    pub fn git_dir(&self) -> PathBuf {
        self.work.join(".git")
    }

    /// Run git in the work tree with a hermetic config. Panics on failure.
    pub fn git(&self, args: &[&str]) -> Vec<u8> {
        let out = Command::new("git")
            .current_dir(&self.work)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_DATE", "2024-01-01T00:00:00Z")
            .env("GIT_COMMITTER_DATE", "2024-01-01T00:00:00Z")
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

/// The parser's view of every object, keyed by hex hash, in the same shape
/// as `Repo::git_objects`.
pub fn parsed_objects(index: &HashIndex, container: &ObjectContainer) -> HashMap<String, ObjectInfo> {
    let info = |kind: &str, size: u32, size_disk: u32| ObjectInfo {
        kind: kind.to_string(),
        size: size.into(),
        size_disk: size_disk.into(),
    };
    let mut objects = HashMap::new();
    for (hash, &i) in &index.commits {
        let c = &container.commits[i];
        objects.insert(hex(hash), info("commit", c.size, c.size_disk));
    }
    for (hash, &i) in &index.trees {
        let t = &container.trees[i];
        objects.insert(hex(hash), info("tree", t.size, t.size_disk));
    }
    for (hash, &i) in &index.blobs {
        let b = &container.blobs[i];
        objects.insert(hex(hash), info("blob", b.size, b.size_disk));
    }
    for (hash, &i) in &index.tags {
        let t = &container.tags[i];
        objects.insert(hex(hash), info("tag", t.size, t.size_disk));
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
