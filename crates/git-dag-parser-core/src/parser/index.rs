//! Reads `.git/index`, versions 2, 3 and 4, and gives the objects that it
//! keeps alive, as `git rev-list --indexed-objects` does: the hash of each
//! entry (a blob, or a tree for a sparse directory), the trees of the valid
//! cache-tree nodes (`TREE`), and the blobs of resolve-undo (`REUC`).
//! Submodule entries (gitlinks) are not objects of this repository.
//!
//! A split index (`link` extension) keeps most entries in
//! `sharedindex.<hash>`. Its delete and replace bitmaps say which shared
//! entries the index removes or replaces.

use std::fs;
use std::io::ErrorKind;
use std::path::Path;

use anyhow::{bail, ensure, Context, Result};

use super::bodies::hex;
use crate::models::container::{ObjectHash, HASH_LEN};

/// The hashes of all objects that the index keeps, in file order, with
/// duplicates. No index file gives an empty list.
pub fn read_index(git_dir: &Path) -> Result<Vec<ObjectHash>> {
    let Some(index) = read_file(&git_dir.join("index"))? else {
        return Ok(Vec::new());
    };
    let shared = match &index.link {
        Some(link) if link.shared != [0; HASH_LEN] => {
            let path = git_dir.join(format!("sharedindex.{}", hex(&link.shared)));
            let shared = read_file(&path)?;
            ensure!(
                shared.is_some(),
                "The shared index {} is missing",
                path.display()
            );
            shared
        }
        _ => None,
    };
    roots(index, shared)
}

/// The parsed index file at `path`, or None if there is no such file
fn read_file(path: &Path) -> Result<Option<IndexFile>> {
    let data = match fs::read(path) {
        Ok(data) => data,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("Failed to read {}", path.display())),
    };
    let index = parse_index(&data).with_context(|| format!("Invalid index {}", path.display()))?;
    Ok(Some(index))
}

/// One index entry: its hash and mode
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Entry {
    hash: ObjectHash,
    mode: u32,
}

impl Entry {
    /// False for a gitlink: a commit of a submodule, not of this repository
    fn is_object(&self) -> bool {
        self.mode & 0o170000 != 0o160000
    }
}

/// The `link` extension of a split index
#[derive(Debug, Default, PartialEq, Eq)]
struct Link {
    /// Hash in the name of the shared index file; all zeros if none
    shared: ObjectHash,
    /// Shared entries that this index deletes
    delete: Vec<bool>,
    /// Shared entries that this index replaces, in order, with its first
    /// entries
    replace: Vec<bool>,
}

/// What one index file holds
#[derive(Debug, Default, PartialEq, Eq)]
struct IndexFile {
    entries: Vec<Entry>,
    /// Hashes of the `TREE` and `REUC` extensions
    extension_hashes: Vec<ObjectHash>,
    link: Option<Link>,
}

/// Size of the fixed part of an entry: times, dev, ino, mode, uid, gid,
/// size (40 bytes), hash, flags
const ENTRY_FIXED: usize = 40 + HASH_LEN + 2;
/// The flag of an entry with 2 more bytes of flags (version 3 and 4)
const EXTENDED: u16 = 0x4000;
/// The name length bits of the flags; 0xFFF for a name of that length or
/// more
const NAME_LENGTH: u16 = 0x0FFF;

fn be32(data: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(data[at..at + 4].try_into().expect("4 bytes"))
}

/// The position of the first NUL in `data`
fn nul(data: &[u8]) -> Result<usize> {
    data.iter()
        .position(|&b| b == 0)
        .context("A name has no NUL at the end")
}

fn parse_index(data: &[u8]) -> Result<IndexFile> {
    ensure!(data.len() >= 12 + HASH_LEN, "The file is too short");
    ensure!(&data[..4] == b"DIRC", "The signature is not DIRC");
    let version = be32(data, 4);
    ensure!(
        (2..=4).contains(&version),
        "Index version {version} is not supported"
    );
    let count = be32(data, 8) as usize;
    // The file ends with the hash of its contents
    let end = data.len() - HASH_LEN;

    let mut at = 12;
    let mut entries = Vec::with_capacity(count);
    for _ in 0..count {
        ensure!(at + ENTRY_FIXED <= end, "Entry at {at} is past the end");
        let mode = be32(data, at + 24);
        let hash = data[at + 40..at + 40 + HASH_LEN]
            .try_into()
            .expect("hash length");
        let flags = u16::from_be_bytes([data[at + 60], data[at + 61]]);
        let mut name = at + ENTRY_FIXED;
        if version >= 3 && flags & EXTENDED != 0 {
            name += 2;
        }
        ensure!(name < end, "Entry at {at} is past the end");
        at = if version == 4 {
            // A varint (how many bytes of the previous name to remove),
            // then the rest of the name with a NUL; no padding. Only the
            // hash is used, so the name is not built.
            let mut suffix = name;
            while suffix < end && data[suffix] & 0x80 != 0 {
                suffix += 1;
            }
            suffix += 1;
            ensure!(suffix <= end, "Entry at {at} is past the end");
            suffix + nul(&data[suffix..end])? + 1
        } else {
            let length = match flags & NAME_LENGTH {
                NAME_LENGTH => nul(&data[name..end])?,
                length => length as usize,
            };
            // 1 to 8 NULs make the entry a multiple of 8 bytes
            at + ((name - at + length + 8) & !7)
        };
        ensure!(at <= end, "Entry is past the end");
        entries.push(Entry { hash, mode });
    }

    let mut extension_hashes = Vec::new();
    let mut link = None;
    while at < end {
        ensure!(at + 8 <= end, "Extension header at {at} is past the end");
        let size = be32(data, at + 4) as usize;
        let body = data
            .get(at + 8..at + 8 + size)
            .filter(|_| at + 8 + size <= end)
            .with_context(|| format!("Extension at {at} is past the end"))?;
        match &data[at..at + 4] {
            b"TREE" => cache_tree(body, &mut extension_hashes)?,
            b"REUC" => resolve_undo(body, &mut extension_hashes)?,
            b"link" => link = Some(read_link(body)?),
            // Other extensions (untracked cache, fsmonitor, end of index
            // entries, sparse directories) have no object hashes
            _ => {}
        }
        at += 8 + size;
    }
    Ok(IndexFile {
        entries,
        extension_hashes,
        link,
    })
}

fn take_hash(data: &mut &[u8]) -> Result<ObjectHash> {
    ensure!(data.len() >= HASH_LEN, "A hash is past the end");
    let (hash, rest) = data.split_at(HASH_LEN);
    *data = rest;
    Ok(hash.try_into().expect("hash length"))
}

/// The bytes up to `end`, and `data` moves past `end`
fn take_until<'a>(data: &mut &'a [u8], end: u8) -> Result<&'a [u8]> {
    let at = data
        .iter()
        .position(|&b| b == end)
        .context("A field has no end")?;
    let field = &data[..at];
    *data = &data[at + 1..];
    Ok(field)
}

/// The cache-tree nodes: `<path>\0<entry count> <subtree count>\n`, then
/// the tree hash if the entry count is not -1 (a node that is not valid)
fn cache_tree(mut data: &[u8], hashes: &mut Vec<ObjectHash>) -> Result<()> {
    while !data.is_empty() {
        take_until(&mut data, 0)?;
        let entries: i64 = std::str::from_utf8(take_until(&mut data, b' ')?)?
            .parse()
            .context("The entry count is not a number")?;
        take_until(&mut data, b'\n')?;
        if entries >= 0 {
            hashes.push(take_hash(&mut data)?);
        }
    }
    Ok(())
}

/// The resolve-undo records: `<path>\0`, the octal modes of stages 1 to 3
/// with a NUL after each, then a hash for each mode that is not 0
fn resolve_undo(mut data: &[u8], hashes: &mut Vec<ObjectHash>) -> Result<()> {
    while !data.is_empty() {
        take_until(&mut data, 0)?;
        let mut modes = [0u32; 3];
        for mode in &mut modes {
            let field = std::str::from_utf8(take_until(&mut data, 0)?)?;
            *mode = u32::from_str_radix(field, 8).context("A mode is not an octal number")?;
        }
        for _ in modes.iter().filter(|&&m| m != 0) {
            hashes.push(take_hash(&mut data)?);
        }
    }
    Ok(())
}

/// The hash of the shared index, then the delete and replace bitmaps
fn read_link(mut data: &[u8]) -> Result<Link> {
    let shared = take_hash(&mut data)?;
    if data.is_empty() {
        return Ok(Link {
            shared,
            ..Link::default()
        });
    }
    let (delete, used) = read_ewah(data)?;
    let (replace, _) = read_ewah(&data[used..])?;
    Ok(Link {
        shared,
        delete,
        replace,
    })
}

/// The roots of an index and, for a split index, of its shared index
fn roots(index: IndexFile, shared: Option<IndexFile>) -> Result<Vec<ObjectHash>> {
    let mut roots = Vec::new();
    match index.link {
        Some(link) if link.shared != [0; HASH_LEN] => {
            let Some(shared) = shared else {
                bail!("A split index needs its shared index");
            };
            let bit = |bits: &[bool], i: usize| bits.get(i).copied().unwrap_or(false);
            // The first entries of the split index replace, in order, the
            // shared entries with a replace bit. The other entries are new.
            let mut split = index.entries.iter();
            for (i, entry) in shared.entries.iter().enumerate() {
                let entry = if bit(&link.replace, i) {
                    split
                        .next()
                        .context("The split index has too few replacement entries")?
                } else {
                    entry
                };
                if !bit(&link.delete, i) && entry.is_object() {
                    roots.push(entry.hash);
                }
            }
            roots.extend(split.filter(|e| e.is_object()).map(|e| e.hash));
        }
        _ => roots.extend(
            index
                .entries
                .iter()
                .filter(|e| e.is_object())
                .map(|e| e.hash),
        ),
    }
    // The extensions of a shared index are not used, as in git
    roots.extend(index.extension_hashes);
    Ok(roots)
}

/// A bitmap in git's EWAH format, as a bool for each bit, and the number of
/// bytes it used: the bit count, the word count, the 64-bit words, and the
/// position of the last marker word. Each marker word gives a run of
/// `run` words of all `run bit` (bits 1 to 32 and bit 0), then the number
/// of literal words that follow it (bits 33 to 63). Bit 0 of a literal
/// word is its first bit.
fn read_ewah(data: &[u8]) -> Result<(Vec<bool>, usize)> {
    ensure!(data.len() >= 8, "The bitmap header is past the end");
    let bits = be32(data, 0) as usize;
    let words = be32(data, 4) as usize;
    let used = 8 + words * 8 + 4;
    ensure!(data.len() >= used, "The bitmap is past the end");
    let word =
        |i: usize| u64::from_be_bytes(data[8 + i * 8..16 + i * 8].try_into().expect("8 bytes"));

    let mut out = vec![false; bits];
    let mut pos = 0usize;
    let mut i = 0;
    while i < words {
        let marker = word(i);
        i += 1;
        let run = ((marker >> 1) & 0xFFFF_FFFF) as usize * 64;
        if marker & 1 == 1 {
            out[pos.min(bits)..(pos + run).min(bits)].fill(true);
        }
        pos += run;
        let literals = (marker >> 33) as usize;
        ensure!(i + literals <= words, "The bitmap words are past the end");
        for _ in 0..literals {
            let mut w = word(i);
            i += 1;
            while w != 0 {
                let b = pos + w.trailing_zeros() as usize;
                if b < bits {
                    out[b] = true;
                }
                w &= w - 1;
            }
            pos += 64;
        }
    }
    Ok((out, used))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::models::container::HASH_LEN;

    const REGULAR: u32 = 0o100644;
    const GITLINK: u32 = 0o160000;
    const DIR: u32 = 0o040000;

    fn h(byte: u8) -> ObjectHash {
        [byte; HASH_LEN]
    }

    fn be32(v: u32) -> [u8; 4] {
        v.to_be_bytes()
    }

    /// git's varint (`encode_varint`), as version 4 uses for the strip count
    fn varint(mut value: usize) -> Vec<u8> {
        let mut out = vec![(value & 127) as u8];
        value >>= 7;
        while value > 0 {
            value -= 1;
            out.push(128 | (value & 127) as u8);
            value >>= 7;
        }
        out.reverse();
        out
    }

    /// An index file of `version` with these entries (mode, hash, name,
    /// extended flag) and extensions. The trailing checksum is not checked,
    /// so it is all zeros.
    fn index_file(
        version: u32,
        entries: &[(u32, ObjectHash, &[u8], bool)],
        extensions: &[(&[u8; 4], Vec<u8>)],
    ) -> Vec<u8> {
        let mut out = b"DIRC".to_vec();
        out.extend(be32(version));
        out.extend(be32(entries.len() as u32));
        let mut previous: &[u8] = b"";
        for &(mode, hash, name, extended) in entries {
            let start = out.len();
            out.extend([0u8; 24]); // ctime, mtime, dev, ino
            out.extend(be32(mode));
            out.extend([0u8; 12]); // uid, gid, size
            out.extend(hash);
            let mut flags = name.len().min(0xFFF) as u16;
            if extended {
                flags |= 0x4000;
            }
            out.extend(flags.to_be_bytes());
            if extended {
                out.extend([0x20, 0]); // intent-to-add
            }
            if version == 4 {
                let common = previous
                    .iter()
                    .zip(name)
                    .take_while(|(a, b)| a == b)
                    .count();
                out.extend(varint(previous.len() - common));
                out.extend(&name[common..]);
                out.push(0);
                previous = name;
            } else {
                out.extend(name);
                let len = out.len() - start;
                out.resize(start + (len + 8) / 8 * 8, 0);
            }
        }
        for (signature, data) in extensions {
            out.extend(*signature);
            out.extend(be32(data.len() as u32));
            out.extend(data);
        }
        out.extend([0u8; HASH_LEN]);
        out
    }

    /// One cache-tree node; `hash` is None for an invalid node
    fn tree_node(path: &[u8], entries: i32, subtrees: u32, hash: Option<ObjectHash>) -> Vec<u8> {
        let mut out = path.to_vec();
        out.push(0);
        out.extend(format!("{entries} {subtrees}\n").into_bytes());
        out.extend(hash.into_iter().flatten());
        out
    }

    /// An EWAH bitmap of `bits` bits with these bits set: one marker word,
    /// then all words as literal words
    fn ewah(bits: usize, set: &[usize]) -> Vec<u8> {
        let mut words = vec![0u64; bits.div_ceil(64)];
        for &b in set {
            words[b / 64] |= 1 << (b % 64);
        }
        let mut all = vec![(words.len() as u64) << 33];
        all.extend(words);
        ewah_words(bits, &all)
    }

    fn ewah_words(bits: usize, words: &[u64]) -> Vec<u8> {
        let mut out = be32(bits as u32).to_vec();
        out.extend(be32(words.len() as u32));
        for w in words {
            out.extend(w.to_be_bytes());
        }
        out.extend(be32(0));
        out
    }

    fn hashes(entries: &[Entry]) -> Vec<ObjectHash> {
        entries.iter().map(|e| e.hash).collect()
    }

    #[test]
    fn version_2_gives_the_entry_hashes_and_the_valid_cache_tree_trees() {
        let tree = [
            tree_node(b"", 3, 1, Some(h(10))),
            tree_node(b"src", -1, 1, None),
            tree_node(b"deep", 1, 0, Some(h(12))),
        ]
        .concat();
        let data = index_file(
            2,
            &[
                (REGULAR, h(1), b"a.txt", false),
                (REGULAR, h(2), b"src/deep/b.rs", false),
                (REGULAR, h(3), b"src/main_with_a_longer_name.rs", false),
            ],
            &[(b"TREE", tree)],
        );
        let index = parse_index(&data).unwrap();
        assert_eq!(hashes(&index.entries), vec![h(1), h(2), h(3)]);
        assert_eq!(index.entries[0].mode, REGULAR);
        assert_eq!(index.extension_hashes, vec![h(10), h(12)]);
        assert_eq!(index.link, None);
    }

    #[test]
    fn version_3_reads_entries_with_extended_flags() {
        let data = index_file(
            3,
            &[
                (REGULAR, h(1), b"a", false),
                (REGULAR, h(2), b"intent-to-add.txt", true),
                (REGULAR, h(3), b"z", false),
            ],
            &[],
        );
        assert_eq!(
            hashes(&parse_index(&data).unwrap().entries),
            vec![h(1), h(2), h(3)]
        );
    }

    #[test]
    fn version_4_reads_prefix_compressed_names() {
        let long = vec![b'x'; 200];
        let data = index_file(
            4,
            &[
                (REGULAR, h(1), b"src/a.rs", false),
                (REGULAR, h(2), b"src/b.rs", false),
                (REGULAR, h(3), &long, false),
                // Strips all 200 bytes: a varint of two bytes
                (REGULAR, h(4), b"y", false),
            ],
            &[(b"TREE", tree_node(b"", 4, 0, Some(h(9))))],
        );
        let index = parse_index(&data).unwrap();
        assert_eq!(hashes(&index.entries), vec![h(1), h(2), h(3), h(4)]);
        assert_eq!(index.extension_hashes, vec![h(9)]);
    }

    #[test]
    fn resolve_undo_gives_the_blob_of_each_stage_that_has_one() {
        let mut reuc = b"conflict.txt\x00100644\x00100644\x00100644\x00".to_vec();
        reuc.extend(h(21));
        reuc.extend(h(22));
        reuc.extend(h(23));
        // Deleted on one side: no stage 2
        reuc.extend(b"gone.txt\x00100644\x000\x00100644\x00");
        reuc.extend(h(24));
        reuc.extend(h(25));
        let data = index_file(2, &[(REGULAR, h(1), b"a", false)], &[(b"REUC", reuc)]);
        let index = parse_index(&data).unwrap();
        assert_eq!(
            index.extension_hashes,
            vec![h(21), h(22), h(23), h(24), h(25)]
        );
    }

    #[test]
    fn unknown_extensions_are_skipped() {
        let data = index_file(
            2,
            &[(REGULAR, h(1), b"a", false)],
            &[
                (b"UNTR", vec![7; 13]),
                (b"TREE", tree_node(b"", 1, 0, Some(h(8)))),
            ],
        );
        assert_eq!(parse_index(&data).unwrap().extension_hashes, vec![h(8)]);
    }

    #[test]
    fn roots_skip_gitlinks_and_keep_sparse_directory_trees() {
        let data = index_file(
            3,
            &[
                (REGULAR, h(1), b"a", false),
                (DIR, h(2), b"sparse/", true),
                (GITLINK, h(3), b"submodule", false),
            ],
            &[(b"TREE", tree_node(b"", 3, 0, Some(h(4))))],
        );
        let index = parse_index(&data).unwrap();
        assert_eq!(roots(index, None).unwrap(), vec![h(1), h(2), h(4)]);
    }

    #[test]
    fn a_bad_signature_or_version_is_an_error() {
        let mut data = index_file(2, &[], &[]);
        data[0] = b'X';
        assert!(parse_index(&data).is_err());
        assert!(parse_index(&index_file(5, &[], &[])).is_err());
        assert!(parse_index(b"DIRC").is_err());
    }

    #[test]
    fn read_ewah_decodes_literal_words_and_runs() {
        let (bits, used) = read_ewah(&ewah(71, &[1, 3, 70])).unwrap();
        assert_eq!(used, 12 + 3 * 8);
        let set: Vec<usize> = (0..bits.len()).filter(|&i| bits[i]).collect();
        assert_eq!(set, vec![1, 3, 70]);

        // A run of one word of ones, then one literal word, then a run of
        // two words of zeros with no literal words
        let marker = |run_bit: u64, run: u64, literals: u64| run_bit | run << 1 | literals << 33;
        let words = [marker(1, 1, 1), 0b101, marker(0, 2, 0)];
        let (bits, _) = read_ewah(&ewah_words(256, &words)).unwrap();
        assert_eq!(bits.len(), 256);
        let set: Vec<usize> = (0..bits.len()).filter(|&i| bits[i]).collect();
        let want: Vec<usize> = (0..64).chain([64, 66]).collect();
        assert_eq!(set, want);
    }

    #[test]
    fn a_split_index_deletes_and_replaces_shared_entries() {
        let shared = index_file(
            2,
            &[
                (REGULAR, h(1), b"a", false),
                (REGULAR, h(2), b"b", false),
                (REGULAR, h(3), b"c", false),
                (GITLINK, h(4), b"d", false),
                (REGULAR, h(5), b"e", false),
            ],
            &[],
        );
        // Delete b; replace c and e with the first two entries; add f
        let mut link = h(0xAB).to_vec();
        link.extend(ewah(5, &[1]));
        link.extend(ewah(5, &[2, 4]));
        let split = index_file(
            2,
            &[
                (REGULAR, h(13), b"", false),
                (REGULAR, h(15), b"", false),
                (REGULAR, h(16), b"f", false),
            ],
            &[(b"link", link), (b"TREE", tree_node(b"", -1, 0, None))],
        );
        let split = parse_index(&split).unwrap();
        let shared = parse_index(&shared).unwrap();
        let link = split.link.as_ref().unwrap();
        assert_eq!(link.shared, h(0xAB));
        assert_eq!(link.delete, vec![false, true, false, false, false]);
        assert_eq!(link.replace, vec![false, false, true, false, true]);
        let mut got = roots(split, Some(shared)).unwrap();
        got.sort();
        assert_eq!(got, vec![h(1), h(13), h(15), h(16)]);
    }

    #[test]
    fn a_split_index_without_its_shared_index_is_an_error() {
        let mut link = h(0xAB).to_vec();
        link.extend(ewah(0, &[]));
        link.extend(ewah(0, &[]));
        let split = index_file(2, &[], &[(b"link", link)]);
        let split = parse_index(&split).unwrap();
        assert!(roots(split, None).is_err());
    }

    #[test]
    fn read_index_reads_the_shared_index_of_a_split_index() {
        let dir = tempfile::tempdir().unwrap();
        let mut link = h(0xCD).to_vec();
        link.extend(ewah(2, &[0]));
        link.extend(ewah(2, &[]));
        fs::write(
            dir.path().join("index"),
            index_file(2, &[(REGULAR, h(7), b"new", false)], &[(b"link", link)]),
        )
        .unwrap();
        let shared_name = format!("sharedindex.{}", "cd".repeat(HASH_LEN));
        fs::write(
            dir.path().join(shared_name),
            index_file(
                2,
                &[
                    (REGULAR, h(1), b"deleted", false),
                    (REGULAR, h(2), b"kept", false),
                ],
                &[],
            ),
        )
        .unwrap();
        let mut got = read_index(dir.path()).unwrap();
        got.sort();
        assert_eq!(got, vec![h(2), h(7)]);
    }

    #[test]
    fn no_index_file_gives_no_roots() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_index(dir.path()).unwrap().is_empty());
    }
}
