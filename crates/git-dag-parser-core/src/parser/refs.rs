//! Reads refs from `packed-refs`, loose files under `refs/`, and a detached
//! `HEAD`. Symbolic refs are skipped: the ref they name is read on its own.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};

use super::content::hex_to_hash;
use crate::models::container::ObjectHash;

/// Every direct ref as (name, target hash), sorted by name
pub fn read_refs(git_dir: &Path) -> Result<Vec<(Vec<u8>, ObjectHash)>> {
    let mut refs = BTreeMap::new();

    let packed = git_dir.join("packed-refs");
    if packed.is_file() {
        let data = fs::read(&packed).context("Failed to read packed-refs")?;
        refs.extend(parse_packed_refs(&data)?);
    }

    // Updating a ref writes a loose file without rewriting packed-refs, so a
    // loose ref replaces a packed one of the same name.
    read_loose_refs(&git_dir.join("refs"), b"refs", &mut refs)?;

    let head = fs::read(git_dir.join("HEAD")).context("Failed to read HEAD")?;
    if let Some(hash) = parse_loose_ref(&head)? {
        refs.insert(b"HEAD".to_vec(), hash);
    }

    Ok(refs.into_iter().collect())
}

/// Lines are `<hex> <name>`. `#` starts the header line, and `^<hex>` lines
/// give the peeled target of the tag above; tag objects are parsed directly,
/// so those are skipped.
fn parse_packed_refs(data: &[u8]) -> Result<Vec<(Vec<u8>, ObjectHash)>> {
    data.split(|&b| b == b'\n')
        .filter(|line| !line.is_empty() && line[0] != b'#' && line[0] != b'^')
        .map(|line| {
            let (hex, name) = line.split_at(line.len().min(40));
            Ok((name.trim_ascii().to_vec(), hex_to_hash(hex)?))
        })
        .collect()
}

/// The target of a loose ref file, or None for a symbolic ref (`ref: <name>`)
fn parse_loose_ref(data: &[u8]) -> Result<Option<ObjectHash>> {
    let data = data.trim_ascii();
    if data.starts_with(b"ref: ") {
        return Ok(None);
    }
    Ok(Some(hex_to_hash(data)?))
}

fn read_loose_refs(
    dir: &Path,
    prefix: &[u8],
    refs: &mut BTreeMap<Vec<u8>, ObjectHash>,
) -> Result<()> {
    if !dir.is_dir() {
        return Ok(());
    }

    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let file_name = entry.file_name();
        let file_name = file_name.as_encoded_bytes();
        // Left behind by an interrupted update; not a ref.
        if file_name.ends_with(b".lock") {
            continue;
        }

        let mut name = prefix.to_vec();
        name.push(b'/');
        name.extend_from_slice(file_name);

        let path = entry.path();
        if path.is_dir() {
            read_loose_refs(&path, &name, refs)?;
        } else {
            let data = fs::read(&path)?;
            let hash = parse_loose_ref(&data)
                .with_context(|| format!("Invalid ref file {}", path.display()))?;
            if let Some(hash) = hash {
                refs.insert(name, hash);
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "1111111111111111111111111111111111111111";
    const B: &str = "2222222222222222222222222222222222222222";
    const C: &str = "3333333333333333333333333333333333333333";

    fn hash(hex: &str) -> ObjectHash {
        hex_to_hash(hex.as_bytes()).unwrap()
    }

    #[test]
    fn parse_packed_refs_skips_header_and_peeled_lines() {
        let data = format!(
            "# pack-refs with: peeled fully-peeled sorted \n\
             {A} refs/heads/main\n\
             {B} refs/tags/v1\n\
             ^{C}\n"
        );

        let refs = parse_packed_refs(data.as_bytes()).unwrap();

        assert_eq!(
            refs,
            vec![
                (b"refs/heads/main".to_vec(), hash(A)),
                (b"refs/tags/v1".to_vec(), hash(B)),
            ]
        );
    }

    #[test]
    fn parse_loose_ref_reads_hash_and_skips_symbolic() {
        assert_eq!(
            parse_loose_ref(format!("{A}\n").as_bytes()).unwrap(),
            Some(hash(A))
        );
        assert_eq!(parse_loose_ref(b"ref: refs/heads/main\n").unwrap(), None);
    }

    #[test]
    fn read_refs_prefers_loose_over_packed() {
        let dir = tempfile::tempdir().unwrap();
        let git_dir = dir.path();
        fs::write(
            git_dir.join("packed-refs"),
            format!("{A} refs/heads/main\n{B} refs/tags/v1\n"),
        )
        .unwrap();
        fs::create_dir_all(git_dir.join("refs/heads/feature")).unwrap();
        fs::write(git_dir.join("refs/heads/main"), format!("{C}\n")).unwrap();
        fs::write(git_dir.join("refs/heads/feature/x"), format!("{B}\n")).unwrap();
        fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();

        let refs = read_refs(git_dir).unwrap();

        assert_eq!(
            refs,
            vec![
                (b"refs/heads/feature/x".to_vec(), hash(B)),
                (b"refs/heads/main".to_vec(), hash(C)),
                (b"refs/tags/v1".to_vec(), hash(B)),
            ]
        );
    }

    #[test]
    fn read_refs_includes_detached_head() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("HEAD"), format!("{A}\n")).unwrap();

        let refs = read_refs(dir.path()).unwrap();

        assert_eq!(refs, vec![(b"HEAD".to_vec(), hash(A))]);
    }
}
