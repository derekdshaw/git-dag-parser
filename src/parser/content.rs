//! Parsers for commit, tree and tag object bodies, shared by the pack and loose
//! object readers. Bodies are handled as bytes: git does not require file
//! names, author names or messages to be UTF-8.

use anyhow::Result;

use crate::error::ParseError;
use crate::models::container::{CommitRaw, ObjectHash, TreeEntry};

/// Fields of an annotated tag body
#[derive(Debug)]
pub struct TagRaw {
    /// Hash from the `object` header
    pub target: ObjectHash,
    /// Name from the `tag` header
    pub name: Vec<u8>,
}

/// Header lines of a commit or tag body. Headers end at the first blank
/// line; the message after it is never read.
fn header_lines(data: &[u8]) -> impl Iterator<Item = &[u8]> {
    data.split(|&b| b == b'\n').take_while(|l| !l.is_empty())
}

/// Extract the root tree, parent hashes and committer time from a commit body
pub fn parse_commit(data: &[u8], index: usize) -> Result<CommitRaw> {
    let mut tree_hash = None;
    let mut parent_hashes = Vec::new();
    let mut commit_time = None;

    for line in header_lines(data) {
        if let Some(hex) = line.strip_prefix(b"tree ") {
            tree_hash = Some(hex_to_hash(hex)?);
        } else if let Some(hex) = line.strip_prefix(b"parent ") {
            parent_hashes.push(hex_to_hash(hex)?);
        } else if let Some(ident) = line.strip_prefix(b"committer ") {
            commit_time = Some(parse_ident_time(ident)?);
        }
    }

    Ok(CommitRaw {
        index,
        root_tree_hash: tree_hash
            .ok_or_else(|| ParseError::InvalidObjectFormat("commit missing tree".into()))?,
        parent_hashes,
        commit_time: commit_time
            .ok_or_else(|| ParseError::InvalidObjectFormat("commit missing committer".into()))?,
    })
}

/// Timestamp from an identity line: `<name> <<email>> <seconds> <tz>`. Read
/// from the right, since names may contain spaces.
fn parse_ident_time(ident: &[u8]) -> Result<i64> {
    let seconds = ident
        .rsplit(|&b| b == b' ')
        .nth(1)
        .ok_or_else(|| ParseError::InvalidObjectFormat("identity missing timestamp".into()))?;
    std::str::from_utf8(seconds)?
        .parse()
        .map_err(|_| ParseError::InvalidObjectFormat("invalid timestamp".into()).into())
}

/// Extract the target hash and name from an annotated tag body
pub fn parse_tag(data: &[u8]) -> Result<TagRaw> {
    let mut target = None;
    let mut name = Vec::new();

    for line in header_lines(data) {
        if let Some(hex) = line.strip_prefix(b"object ") {
            target = Some(hex_to_hash(hex)?);
        } else if let Some(tag_name) = line.strip_prefix(b"tag ") {
            name = tag_name.to_vec();
        }
    }

    Ok(TagRaw {
        target: target
            .ok_or_else(|| ParseError::InvalidObjectFormat("tag missing object".into()))?,
        name,
    })
}

/// Parse tree body entries: `<octal mode> <name>\0<20-byte hash>`, repeated
pub fn parse_tree(data: &[u8]) -> Result<Vec<TreeEntry>> {
    let mut entries = Vec::new();
    let mut pos = 0;

    while pos < data.len() {
        let space_pos = data[pos..]
            .iter()
            .position(|&b| b == b' ')
            .ok_or_else(|| ParseError::InvalidObjectFormat("tree entry missing space".into()))?;

        let mode_str = std::str::from_utf8(&data[pos..pos + space_pos])?;
        let mode = u32::from_str_radix(mode_str, 8)
            .map_err(|_| ParseError::InvalidObjectFormat("invalid mode".into()))?;
        pos += space_pos + 1;

        let null_pos = data[pos..]
            .iter()
            .position(|&b| b == 0)
            .ok_or_else(|| ParseError::InvalidObjectFormat("tree entry missing null".into()))?;

        let name = data[pos..pos + null_pos].to_vec();
        pos += null_pos + 1;

        if pos + 20 > data.len() {
            return Err(ParseError::InvalidObjectFormat("tree entry truncated".into()).into());
        }

        let hash: ObjectHash = data[pos..pos + 20].try_into()?;
        pos += 20;

        entries.push(TreeEntry::new(mode, name, hash));
    }

    Ok(entries)
}

/// Convert 40 hex digits to a hash
pub fn hex_to_hash(hex: &[u8]) -> Result<ObjectHash> {
    if hex.len() != 40 {
        return Err(ParseError::InvalidObjectFormat("invalid hash length".into()).into());
    }

    let mut hash = [0u8; 20];
    for (byte, pair) in hash.iter_mut().zip(hex.chunks(2)) {
        let s = std::str::from_utf8(pair)?;
        *byte = u8::from_str_radix(s, 16)
            .map_err(|_| ParseError::InvalidObjectFormat("invalid hex".into()))?;
    }

    Ok(hash)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::container::FileKind;

    const TREE_HEX: &[u8] = b"4b825dc642cb6eb9a060e54bf8d69288fbee4904";
    const PARENT1_HEX: &[u8] = b"0123456789abcdef0123456789abcdef01234567";
    const PARENT2_HEX: &[u8] = b"89abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn hex_to_hash_decodes_all_bytes() {
        let hash = hex_to_hash(PARENT1_HEX).unwrap();
        assert_eq!(hash[0], 0x01);
        assert_eq!(hash[1], 0x23);
        assert_eq!(hash[19], 0x67);
    }

    #[test]
    fn parse_commit_reads_tree_and_parents_in_order() {
        let mut body = Vec::new();
        body.extend_from_slice(b"tree ");
        body.extend_from_slice(TREE_HEX);
        body.extend_from_slice(b"\nparent ");
        body.extend_from_slice(PARENT1_HEX);
        body.extend_from_slice(b"\nparent ");
        body.extend_from_slice(PARENT2_HEX);
        // Latin-1 names and message, as written with i18n.commitEncoding set.
        // Author and committer times differ so the right one must be picked.
        body.extend_from_slice(b"\nauthor Ren\xe9 Q <r@example.com> 1700000000 +0000\n");
        body.extend_from_slice(b"committer Ren\xe9 Q <r@example.com> 1700000555 -0700\n");
        body.extend_from_slice(b"encoding ISO-8859-1\n\ncaf\xe9\n");

        let raw = parse_commit(&body, 7).unwrap();

        assert_eq!(raw.index, 7);
        assert_eq!(raw.commit_time, 1700000555);
        assert_eq!(raw.root_tree_hash, hex_to_hash(TREE_HEX).unwrap());
        assert_eq!(
            raw.parent_hashes,
            vec![
                hex_to_hash(PARENT1_HEX).unwrap(),
                hex_to_hash(PARENT2_HEX).unwrap()
            ]
        );
    }

    #[test]
    fn parse_commit_stops_at_message() {
        let mut body = Vec::new();
        body.extend_from_slice(b"tree ");
        body.extend_from_slice(TREE_HEX);
        body.extend_from_slice(b"\ncommitter C <c@example.com> 1 +0000");
        body.extend_from_slice(b"\n\nparent ");
        body.extend_from_slice(PARENT1_HEX);
        body.push(b'\n');

        let raw = parse_commit(&body, 0).unwrap();

        assert!(raw.parent_hashes.is_empty());
    }

    #[test]
    fn parse_tag_reads_target_and_name() {
        let mut body = Vec::new();
        body.extend_from_slice(b"object ");
        body.extend_from_slice(TREE_HEX);
        body.extend_from_slice(b"\ntype tree\ntag v1.0\n");
        body.extend_from_slice(b"tagger T <t@example.com> 1700000000 +0000\n\nmessage\n");

        let raw = parse_tag(&body).unwrap();

        assert_eq!(raw.target, hex_to_hash(TREE_HEX).unwrap());
        assert_eq!(raw.name, b"v1.0");
    }

    #[test]
    fn parse_tree_keeps_names_as_bytes() {
        let blob_hash = [0x11; 20];
        let tree_hash = [0x22; 20];
        let mut body = Vec::new();
        body.extend_from_slice(b"100644 caf\xe9.txt\0");
        body.extend_from_slice(&blob_hash);
        body.extend_from_slice(b"40000 src\0");
        body.extend_from_slice(&tree_hash);

        let entries = parse_tree(&body).unwrap();

        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, b"caf\xe9.txt");
        assert_eq!(entries[0].hash, blob_hash);
        assert_eq!(entries[0].kind, FileKind::Regular);
        assert_eq!(entries[1].name, b"src");
        assert_eq!(entries[1].hash, tree_hash);
        assert_eq!(entries[1].kind, FileKind::Tree);
    }
}
