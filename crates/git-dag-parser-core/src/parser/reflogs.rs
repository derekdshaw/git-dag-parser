//! Reads the reflogs: `logs/HEAD` and every file under `logs/refs/`. Each
//! line is `<old hash> <new hash> <name> <<email>> <time> <tz>\t<message>`.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use tracing::warn;

use super::content::hex_to_hash;
use crate::models::container::{HashIndex, ObjectHash, HEX_HASH_LEN};
use crate::models::ReflogEntry;

/// Every reflog entry, with no targets resolved: the logs sorted by ref
/// name, the entries of each log in file order (oldest first). A line that
/// is not valid is skipped, as git does.
pub fn read_reflogs(git_dir: &Path) -> Result<Vec<ReflogEntry>> {
    let logs = git_dir.join("logs");
    let mut files = Vec::new();
    let head = logs.join("HEAD");
    if head.is_file() {
        files.push((b"HEAD".to_vec(), head));
    }
    log_files(&logs.join("refs"), b"refs", &mut files)?;
    files.sort();

    let mut entries = Vec::new();
    for (name, path) in files {
        let data = fs::read(&path).with_context(|| format!("Failed to read {}", path.display()))?;
        for line in data.split(|&b| b == b'\n').filter(|l| !l.is_empty()) {
            match parse_line(&name, line) {
                Some(entry) => entries.push(entry),
                None => warn!(
                    "Skipped a reflog line that is not valid in {}",
                    path.display()
                ),
            }
        }
    }
    Ok(entries)
}

/// Each log file under `dir` as (ref name, path). Files that end in
/// `.lock` are left by an interrupted update and are not logs.
fn log_files(
    dir: &Path,
    prefix: &[u8],
    files: &mut Vec<(Vec<u8>, std::path::PathBuf)>,
) -> Result<()> {
    if !dir.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let file_name = entry.file_name();
        let file_name = file_name.as_encoded_bytes();
        if file_name.ends_with(b".lock") {
            continue;
        }
        let mut name = prefix.to_vec();
        name.push(b'/');
        name.extend_from_slice(file_name);
        let path = entry.path();
        if path.is_dir() {
            log_files(&path, &name, files)?;
        } else {
            files.push((name, path));
        }
    }
    Ok(())
}

/// Set `old_target` and `new_target` of each entry. An all-zero hash means
/// "no object": the ref was created or deleted.
pub fn resolve(entries: &mut [ReflogEntry], index: &HashIndex) {
    let target = |hash: &ObjectHash| {
        if hash.iter().all(|&b| b == 0) {
            None
        } else {
            index.get(hash)
        }
    };
    for e in entries {
        e.old_target = target(&e.old_hash);
        e.new_target = target(&e.new_hash);
    }
}

/// One reflog line without its newline, or None if it is not valid:
/// `<old> <new> <name> <<email>> <time> <tz>[\t<message>]`
fn parse_line(ref_name: &[u8], line: &[u8]) -> Option<ReflogEntry> {
    const HASHES: usize = 2 * HEX_HASH_LEN + 2;
    if line.len() < HASHES || line[HEX_HASH_LEN] != b' ' || line[HASHES - 1] != b' ' {
        return None;
    }
    let old_hash = hex_to_hash(&line[..HEX_HASH_LEN]).ok()?;
    let new_hash = hex_to_hash(&line[HEX_HASH_LEN + 1..HASHES - 1]).ok()?;
    let rest = &line[HASHES..];
    let (ident, message) = match rest.iter().position(|&b| b == b'\t') {
        Some(tab) => (&rest[..tab], &rest[tab + 1..]),
        None => (rest, &rest[rest.len()..]),
    };
    // The name may hold spaces, so read the time after the email
    let after_email = &ident[ident.iter().rposition(|&b| b == b'>')? + 1..];
    let mut fields = after_email.split(|&b| b == b' ').filter(|f| !f.is_empty());
    let time = std::str::from_utf8(fields.next()?).ok()?.parse().ok()?;
    Some(ReflogEntry {
        ref_name: ref_name.to_vec(),
        old_hash,
        new_hash,
        old_target: None,
        new_target: None,
        time,
        message: message.to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::models::container::HASH_LEN;
    use crate::models::ObjectId;
    use crate::parser::content::hex_to_hash;

    const ZERO: &str = "0000000000000000000000000000000000000000";
    const A: &str = "1111111111111111111111111111111111111111";
    const B: &str = "2222222222222222222222222222222222222222";

    fn hash(hex: &str) -> ObjectHash {
        hex_to_hash(hex.as_bytes()).unwrap()
    }

    #[test]
    fn parse_line_reads_the_hashes_the_time_and_a_non_utf8_message() {
        let mut line = format!("{A} {B} J. Doe <j@example.com> 1700000123 -0130\t").into_bytes();
        line.extend_from_slice(b"commit: caf\xe9");
        let e = parse_line(b"refs/heads/main", &line).unwrap();
        assert_eq!(e.ref_name, b"refs/heads/main");
        assert_eq!((e.old_hash, e.new_hash), (hash(A), hash(B)));
        assert_eq!(e.time, 1_700_000_123);
        assert_eq!(e.message, b"commit: caf\xe9");
        assert_eq!((e.old_target, e.new_target), (None, None));
    }

    #[test]
    fn parse_line_accepts_a_line_with_no_message() {
        let line = format!("{ZERO} {A} Doe <> 1700000000 +0000");
        let e = parse_line(b"HEAD", line.as_bytes()).unwrap();
        assert_eq!((e.old_hash, e.new_hash), ([0; HASH_LEN], hash(A)));
        assert_eq!(e.time, 1_700_000_000);
        assert!(e.message.is_empty());
    }

    #[test]
    fn parse_line_rejects_a_bad_line() {
        assert!(parse_line(b"HEAD", b"").is_none());
        assert!(parse_line(b"HEAD", format!("{A} {B}").as_bytes()).is_none());
        let no_time = format!("{A} {B} Doe <d@e>\tmsg");
        assert!(parse_line(b"HEAD", no_time.as_bytes()).is_none());
    }

    #[test]
    fn resolve_gives_none_for_an_all_zero_hash_and_a_missing_object() {
        let mut index = HashIndex::new();
        index.insert(hash(A), ObjectId::Commit(3));
        let line = format!("{ZERO} {A} Doe <d@e> 1 +0000\tcreated");
        let mut entries = vec![parse_line(b"HEAD", line.as_bytes()).unwrap()];
        let line = format!("{A} {B} Doe <d@e> 2 +0000\tmoved");
        entries.push(parse_line(b"HEAD", line.as_bytes()).unwrap());
        resolve(&mut entries, &index);
        assert_eq!(
            (entries[0].old_target, entries[0].new_target),
            (None, Some(ObjectId::Commit(3)))
        );
        assert_eq!(
            (entries[1].old_target, entries[1].new_target),
            (Some(ObjectId::Commit(3)), None)
        );
    }

    #[test]
    fn read_reflogs_reads_head_and_refs_sorted_and_skips_lock_files() {
        let dir = tempfile::tempdir().unwrap();
        let logs = dir.path().join("logs");
        fs::create_dir_all(logs.join("refs/heads/feature")).unwrap();
        let line = |old: &str, new: &str, t: u32| format!("{old} {new} D <d@e> {t} +0000\tm{t}\n");
        fs::write(logs.join("HEAD"), line(ZERO, A, 1) + &line(A, B, 2)).unwrap();
        fs::write(logs.join("refs/heads/main"), line(ZERO, A, 3)).unwrap();
        fs::write(logs.join("refs/heads/feature/x"), line(ZERO, B, 4)).unwrap();
        fs::write(logs.join("refs/heads/main.lock"), line(ZERO, B, 5)).unwrap();

        let entries = read_reflogs(dir.path()).unwrap();

        let got: Vec<(&[u8], i64)> = entries
            .iter()
            .map(|e| (e.ref_name.as_slice(), e.time))
            .collect();
        assert_eq!(
            got,
            vec![
                (&b"HEAD"[..], 1),
                (b"HEAD", 2),
                (b"refs/heads/feature/x", 4),
                (b"refs/heads/main", 3),
            ]
        );
    }

    #[test]
    fn read_reflogs_of_a_repository_with_no_logs_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_reflogs(dir.path()).unwrap().is_empty());
    }
}
