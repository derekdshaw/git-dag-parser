//! Parsers for commit, tree and tag object bodies, shared by the pack and loose
//! object readers. Bodies are handled as bytes: git does not require file
//! names, author names or messages to be UTF-8.

use anyhow::Result;

use crate::error::ParseError;
use crate::models::container::{CommitRaw, FileKind, HashIndex, ObjectHash, TreeEntriesBuilder};
use crate::models::NameStore;

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

/// Parse tree body entries: `<octal mode> <name>\0<20-byte hash>`, repeated,
/// recording them in `out` as the entries of tree `tree`. Entry names are
/// interned in `names`, and each child hash is resolved to its index in
/// `index`, which is complete before any body is read.
pub fn parse_tree(
    data: &[u8],
    tree: usize,
    names: &mut NameStore,
    index: &HashIndex,
    out: &mut TreeEntriesBuilder,
) -> Result<()> {
    out.begin_tree(tree);
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

        let name = names.intern(&data[pos..pos + null_pos])?;
        pos += null_pos + 1;

        if pos + 20 > data.len() {
            return Err(ParseError::InvalidObjectFormat("tree entry truncated".into()).into());
        }

        let hash: ObjectHash = data[pos..pos + 20].try_into()?;
        pos += 20;

        let kind = FileKind::from_mode(mode).unwrap_or(FileKind::Regular);
        let child = if kind.is_tree() {
            index.tree(&hash)
        } else if kind.is_blob() {
            index.blob(&hash)
        } else {
            None // Submodules reference commits in other repositories
        };
        out.push(child, name, kind);
    }

    Ok(())
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

#[cfg(feature = "bench")]
pub(crate) mod benches {
    use divan::counter::BytesCount;
    use divan::{black_box, Bencher};

    use crate::models::container::{HashIndex, TreeEntriesBuilder};
    use crate::models::{NameStore, ObjectId};

    /// A hash index holding every blob the bodies reference, so child
    /// lookups hit, as almost all do in a real parse
    fn blob_index(bodies: &[&[u8]]) -> HashIndex {
        let mut index = HashIndex::new();
        for body in bodies {
            let mut pos = 0;
            while pos < body.len() {
                let null = pos + body[pos..].iter().position(|&b| b == 0).unwrap();
                let hash: [u8; 20] = body[null + 1..null + 21].try_into().unwrap();
                let next = index.len();
                if !index.contains(&hash) {
                    index.insert(hash, ObjectId::Blob(next));
                }
                pos = null + 21;
            }
        }
        index
    }

    /// A tree body with `entries` regular files, like one large directory.
    /// Each entry is 42 bytes: `100644 file_NNNNNN.rs\0` and a 20-byte hash.
    pub(crate) fn synthetic_tree(entries: usize) -> Vec<u8> {
        let mut body = Vec::with_capacity(entries * 42);
        for i in 0..entries {
            body.extend_from_slice(format!("100644 file_{i:06}.rs\0").as_bytes());
            let mut hash = [0u8; 20];
            hash[..8]
                .copy_from_slice(&(i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15).to_le_bytes());
            body.extend_from_slice(&hash);
        }
        body
    }

    // 84,000 entries is about the size of the largest directory in a large
    // monorepo measured during development.
    //
    // Every name is new to the store: the cost of interning on a miss.
    #[divan::bench(args = [100, 1_000, 84_000])]
    fn parse_tree_new_names(bencher: Bencher, entries: usize) {
        let body = synthetic_tree(entries);
        let index = blob_index(&[&body]);
        bencher
            .counter(BytesCount::of_slice(&body))
            .with_inputs(|| (NameStore::new(), TreeEntriesBuilder::new(1)))
            .bench_local_refs(|(names, out)| {
                super::parse_tree(black_box(&body), 0, names, &index, out).unwrap()
            });
    }

    // Every name is already in the store, as when a later version of a
    // directory is parsed: the cost of interning on a hit. The store also
    // holds 100,000 other names, as it would after parsing a large
    // repository. At 84,000 entries every lookup lands on a different part
    // of the table: the worst case, not the common one (see
    // `parse_tree_history`).
    #[divan::bench(args = [100, 1_000, 84_000])]
    fn parse_tree_known_names(bencher: Bencher, entries: usize) {
        let body = synthetic_tree(entries);
        let index = blob_index(&[&body]);
        let mut names = NameStore::new();
        for i in 0..100_000 {
            names.intern(format!("other_{i:06}.rs").as_bytes()).unwrap();
        }
        super::parse_tree(
            &body,
            0,
            &mut names,
            &index,
            &mut TreeEntriesBuilder::new(1),
        )
        .unwrap();
        bencher
            .counter(BytesCount::of_slice(&body))
            .with_inputs(|| TreeEntriesBuilder::new(1))
            .bench_local_refs(|out| {
                super::parse_tree(black_box(&body), 0, &mut names, &index, out).unwrap()
            });
    }

    /// A tree body for directory `dir` with `entries` regular files whose
    /// names no other directory uses
    fn directory_tree(dir: usize, entries: usize) -> Vec<u8> {
        let mut body = Vec::new();
        for i in 0..entries {
            body.extend_from_slice(format!("100644 d{dir:04}_file_{i:02}.rs\0").as_bytes());
            body.extend_from_slice(&[i as u8; 20]);
        }
        body
    }

    // The common case: many small directories, each parsed in several
    // versions. 2,000 directories of 50 entries give the store 100,000
    // names, about as many as linux. The directories come in a scattered
    // order, and each one is parsed `versions` times in a row, as when the
    // versions of a directory sit next to each other in a delta chain.
    // With 1 version no tree reuses the names of the tree before it.
    #[divan::bench(args = [1, 8])]
    fn parse_tree_history(bencher: Bencher, versions: usize) {
        const DIRS: usize = 2_000;
        const ENTRIES: usize = 50;
        let bodies: Vec<Vec<u8>> = (0..DIRS).map(|d| directory_tree(d, ENTRIES)).collect();
        let body_refs: Vec<&[u8]> = bodies.iter().map(|b| b.as_slice()).collect();
        let index = blob_index(&body_refs);
        // 7919 is prime, so this visits every directory once
        let order: Vec<(usize, &[u8])> = (0..DIRS)
            .flat_map(|i| {
                let dir = (i * 7919) % DIRS;
                std::iter::repeat_n((dir, bodies[dir].as_slice()), versions)
            })
            .collect();
        let mut names = NameStore::new();
        let mut warm = TreeEntriesBuilder::new(DIRS);
        for (dir, body) in bodies.iter().enumerate() {
            super::parse_tree(body, dir, &mut names, &index, &mut warm).unwrap();
        }
        bencher
            .counter(BytesCount::new(
                order.iter().map(|(_, b)| b.len()).sum::<usize>(),
            ))
            .with_inputs(|| TreeEntriesBuilder::new(DIRS))
            .bench_local_refs(|out| {
                for &(dir, body) in &order {
                    super::parse_tree(black_box(body), dir, &mut names, &index, out).unwrap();
                }
            });
    }

    #[divan::bench]
    fn parse_commit(bencher: Bencher) {
        let body = b"tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\n\
            parent 0123456789abcdef0123456789abcdef01234567\n\
            parent 89abcdef0123456789abcdef0123456789abcdef\n\
            author A U Thor <author@example.com> 1700000000 +0000\n\
            committer C O Mitter <committer@example.com> 1700000100 +0000\n\
            \n\
            Merge branch 'feature'\n";
        bencher.bench_local(|| super::parse_commit(black_box(body), 0).unwrap());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::ObjectId;

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
    fn parse_tree_resolves_children_and_keeps_names_as_bytes() {
        let blob_hash = [0x11; 20];
        let tree_hash = [0x22; 20];
        let mut body = Vec::new();
        body.extend_from_slice(b"100644 caf\xe9.txt\0");
        body.extend_from_slice(&blob_hash);
        body.extend_from_slice(b"40000 src\0");
        body.extend_from_slice(&tree_hash);

        let mut index = HashIndex::new();
        index.insert(blob_hash, ObjectId::Blob(4));
        index.insert(tree_hash, ObjectId::Tree(2));
        let mut names = NameStore::new();
        let mut out = TreeEntriesBuilder::new(1);
        parse_tree(&body, 0, &mut names, &index, &mut out).unwrap();
        let entries = out.finish();

        let parsed: Vec<_> = entries.entries(0).collect();
        assert_eq!(parsed.len(), 2);
        let (child, name, kind) = parsed[0];
        assert_eq!(names.get(name), b"caf\xe9.txt");
        assert_eq!((child, kind), (Some(4), FileKind::Regular));
        let (child, name, kind) = parsed[1];
        assert_eq!(names.get(name), b"src");
        assert_eq!((child, kind), (Some(2), FileKind::Tree));
    }

    #[test]
    fn parse_tree_leaves_submodules_and_missing_objects_childless() {
        let submodule_hash = [0x33; 20];
        let missing_hash = [0x44; 20];
        let mut body = Vec::new();
        body.extend_from_slice(b"160000 vendored\0");
        body.extend_from_slice(&submodule_hash);
        body.extend_from_slice(b"100644 absent.txt\0");
        body.extend_from_slice(&missing_hash);

        // The submodule's commit being indexed must not give it a child
        let mut index = HashIndex::new();
        index.insert(submodule_hash, ObjectId::Commit(0));
        let mut names = NameStore::new();
        let mut out = TreeEntriesBuilder::new(1);
        parse_tree(&body, 0, &mut names, &index, &mut out).unwrap();
        let entries = out.finish();

        let kinds: Vec<_> = entries
            .entries(0)
            .map(|(child, _, kind)| (child, kind))
            .collect();
        assert_eq!(
            kinds,
            vec![(None, FileKind::Submodule), (None, FileKind::Regular)]
        );
    }

    #[test]
    fn parse_tree_interns_repeated_names_once() {
        let mut body = Vec::new();
        body.extend_from_slice(b"100644 a\0");
        body.extend_from_slice(&[1; 20]);
        body.extend_from_slice(b"100644 b\0");
        body.extend_from_slice(&[2; 20]);
        let index = HashIndex::new();
        let mut names = NameStore::new();
        let mut out = TreeEntriesBuilder::new(2);

        parse_tree(&body, 0, &mut names, &index, &mut out).unwrap();
        parse_tree(&body, 1, &mut names, &index, &mut out).unwrap();
        let entries = out.finish();

        let first: Vec<_> = entries.entries(0).map(|(_, name, _)| name).collect();
        let second: Vec<_> = entries.entries(1).map(|(_, name, _)| name).collect();
        assert_eq!(names.len(), 2);
        assert_eq!(first, second);
        assert_ne!(first[0], first[1]);
    }
}
