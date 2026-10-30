# Architecture

This document tells how git-dag-parser reads a Git repository.

The parser reads the Git object store directly. It does not start `git` processes. It reads the pack files and the loose objects in two passes:

1. Pass 1 reads only the object headers. It finds all objects, their types, their sizes, and all copies of each object on disk.
2. Pass 2 reads the bodies of commits, trees, and tags. It does not read blob bodies.

After the two passes, the parser connects commits, tags, and refs to the objects that they refer to. The parser uses one thread.

## Source files

| File | Contents |
|---|---|
| `src/main.rs` | Sets the log level and runs the command. |
| `src/cli.rs` | The command-line arguments (`clap`). |
| `src/commands/init.rs` | The `init` command. It finds the Git directory, parses the objects, and shows the summary. |
| `src/commands/analyze.rs`, `src/commands/query.rs` | Commands that show only a message at this time. |
| `src/parser/mod.rs` | `parse_objects`: pass 1, pass 2, and the last steps. |
| `src/parser/scan.rs` | Pass 1. |
| `src/parser/bodies.rs` | Pass 2. |
| `src/parser/pack.rs` | Pack index files, pack entry headers, zlib streams, deltas, and the delta cache. |
| `src/parser/loose.rs` | Loose object headers and bodies. |
| `src/parser/content.rs` | The parsers for commit, tree, and tag bodies. |
| `src/parser/refs.rs` | The refs from `packed-refs`, from loose ref files, and from a detached `HEAD`. |
| `src/models/objects.rs` | `Commit`, `Tree`, `Blob`, `Tag`, `Ref`, `Pack`, `ObjectCopy`, and `ObjectId`. |
| `src/models/container.rs` | `ObjectContainer`, `HashIndex`, `CommitRaw`, `FileKind`, and `TreeEntries`. |
| `src/models/name_store.rs` | `NameStore` and `NameId`. |
| `src/error.rs` | The error types. |

## Parse sequence

```
  objects/pack/*.idx, *.pack             objects/xx/yyyy... (loose)
               |                                   |
               v                                   v
 +---------------------------------------------------------------+
 | Pass 1: headers (scan.rs)                                     |
 |   read each pack index, open each pack file with mmap         |
 |   read the pack entry headers in offset sequence              |
 |   read the loose object headers                               |
 |   select the primary copy of each object                      |
 |   sort the objects of each type by hash                       |
 +---------------------------------------------------------------+
               |  ObjectContainer, HashIndex, open pack files
               v
 +---------------------------------------------------------------+
 | Pass 2: bodies (bodies.rs)                                    |
 |   decompress commits, trees, and tags (not blobs)             |
 |   commit body -> CommitRaw                                    |
 |   tree body   -> TreeEntries, NameStore                       |
 |   tag body    -> Tag name and target hash                     |
 +---------------------------------------------------------------+
               |
               v
 +---------------------------------------------------------------+
 | Last steps (parser/mod.rs)                                    |
 |   commit root tree and parents -> indices                     |
 |   tag targets -> indices                                      |
 |   read refs, ref targets -> indices                           |
 +---------------------------------------------------------------+
               |
               v
         ParsedObjects
```

### Pass 1: object headers

Pass 1 (`scan_repository`) finds all pack files in `objects/pack/`. It uses only a pack file that has an `.idx` file. It sorts the pack files by name.

For each pack file, pass 1 reads the `.idx` file and opens the `.pack` file with `mmap`. The pack files stay open until `parse_objects` is complete. If a different process changes a pack file while the program parses the repository, the result is not correct and the program can stop.

Pass 1 reads the header of each pack entry in offset sequence. Thus it reads each pack file one time, from the start to the end. A pack entry header gives the object type and the object size. Pass 1 does not decompress the object bodies.

A delta entry does not give the object type or the object size directly:

- To find the object type, pass 1 follows the chain of delta bases to an entry that is not a delta. For an offset delta, the base is always at a lower offset. Thus pass 1 knows the type of the base before it reads the delta.
- To find the object size, pass 1 decompresses only the first bytes of the delta. These bytes contain the size of the object that the delta makes.

Then pass 1 reads the header of each loose object. A loose object is one zlib stream. Pass 1 decompresses only the start of the stream, which contains the header.

#### Primary copies

An object can have more than one copy on disk. For example, two pack files can contain the same object. Pass 1 records each copy in an `ObjectCopy`. Then it selects one copy of each object as the primary copy. These conditions select the primary copy, in this sequence:

1. A copy in a pack file is better than a loose copy.
2. A copy in a usual pack file is better than a copy in a cruft pack file or a promisor pack file.
3. A copy in a pack file with a higher `mtime` is better than a copy in a pack file with a lower `mtime`.
4. If the `mtime` values are equal, the copy in the pack file that is first in name sequence is better.

An object gets its `size_disk` from its primary copy. Pass 2 reads the body of an object from its primary copy.

#### Object indices

When pass 1 finds an object for the first time, it gives the object a temporary index. At the end of pass 1, `Scan::finish` sorts the objects of each type by hash. The position of an object in the sorted array is its last index.

Thus the index of an object does not change when Git moves objects into different pack files. `Scan::finish` also changes all temporary indices in the copies to the last indices. Then it makes a new `HashIndex`.

### Pass 2: object bodies

Pass 2 (`read_bodies`) reads the body of each commit, tree, and tag from its primary copy. It does not read blob bodies.

Pass 2 reads the copies in the sequence that pass 1 recorded them. This sequence is one pack file after the other, in offset sequence in each pack file. The loose copies come last. Thus pass 2 also reads each pack file from the start to the end.

For a copy in a pack file, `PackReader` decompresses the entry. If the entry is a delta, `PackReader` makes the base object and applies the delta to it. `PackReader` keeps each object that it makes for a delta base in a cache:

- The cache keeps the data in blocks of 64 MB (`DeltaArena`). Thus the cache does not make many small allocations.
- A base is usually at a lower offset than its delta. Thus the cache usually contains the base when pass 2 reads the delta.
- The cache has no size limit. Pass 2 releases the cache when it starts the next pack file.

Pass 2 makes these data from the bodies:

| Object type | Data |
|---|---|
| Commit | A `CommitRaw`: the root tree hash, the parent hashes, and the commit time. |
| Tree | The tree entries in `TreeEntries`, and the entry names in the `NameStore`. |
| Tag | The tag name and the target hash, in the `Tag`. |

#### Tree entries

`parse_tree` reads the entries of a tree body. For each entry, it does these steps:

1. It puts the entry name in the `NameStore` and gets a `NameId`.
2. It finds the index of the child object in the `HashIndex`. This is possible because the `HashIndex` contains all objects at the end of pass 1.
3. It records the child index, the `NameId`, and the `FileKind` in the `TreeEntriesBuilder`.

A submodule entry has no child index, because it refers to a commit in a different repository. An entry has no child index also when its child is not in the object store.

Pass 2 reads the trees in pack file sequence, not in index sequence. The `TreeEntriesBuilder` records the entries in the sequence that it gets them. At the end of pass 2, `TreeEntriesBuilder::finish` puts the entries in tree index sequence. It does this for one array at a time and releases each source array after it copies it. This decreases the maximum memory that the copy uses.

### Last steps

After pass 2, `parse_objects` does these steps:

1. For each commit, it finds the index of the root tree and the index of each parent. If a parent is not in the object store, it sets `boundary` to `true`. This occurs at the edge of a shallow clone.
2. For each annotated tag, it finds the `ObjectId` of the target.
3. It reads the refs. Then it finds the `ObjectId` of the target of each ref.

`read_refs` reads `packed-refs`, the loose ref files in `refs/`, and a detached `HEAD`. A loose ref replaces a packed ref with the same name, because Git writes a loose file when it updates a ref. The parser does not keep symbolic refs, because it reads the ref that a symbolic ref refers to.

### Result

`parse_objects` gives a `ParsedObjects` tuple:

| Item | Contents |
|---|---|
| `HashMap<ObjectHash, CommitRaw>` | The hashes from each commit header. |
| `TreeEntries` | The entries of all trees. |
| `NameStore` | The entry names that `TreeEntries` refers to. |
| `HashIndex` | The `ObjectId` of each hash. |
| `ObjectContainer` | All objects, copies, pack files, and refs. |

## Data model

### Indices, not hashes

All objects refer to other objects by index, not by hash. An index is the position of the object in the array for its type. An `ObjectId` is an index together with its object type, for example `ObjectId::Tree(5)`.

A hash is 20 bytes. An index is 4 bytes or 8 bytes. Thus indices use less memory than hashes. Also, the program can go directly to an object, without a hash map.

### ObjectContainer

`ObjectContainer` contains these arrays:

| Field | Contents |
|---|---|
| `commits` | Each `Commit`: hash, sizes, root tree, parents, `boundary`, and commit time. |
| `trees` | Each `Tree`: hash and sizes. |
| `blobs` | Each `Blob`: hash and sizes. |
| `tags` | Each annotated `Tag`: hash, sizes, name, and target. |
| `refs` | Each `Ref`: name, type (branch, remote, tag, stash, `HEAD`, or other), and target. |
| `packs` | Each `Pack`: name, size, `mtime`, and the `.keep`, cruft, and promisor flags. |
| `copies` | Each `ObjectCopy`. |
| `loose_mtimes` | The `mtime` of each loose copy, sorted by `ObjectId`. |

Each object has two sizes:

- `size` is the size of the object after the program decompresses it. For a delta, it is the size of the object that the delta makes.
- `size_disk` is the size of the primary copy on disk.

### ObjectCopy

An `ObjectCopy` is one copy of an object on disk. It records the object, the pack file and the offset (or "loose"), the size on disk, and if the copy is a delta. It also records if the copy is the primary copy.

An `ObjectCopy` is 32 bytes, because a large repository can have more than 10 million copies. For example, the Linux repository has approximately 10.5 million copies. Thus the `ObjectCopy` does not contain the `mtime`. A copy in a pack file uses the `mtime` of its `Pack`. A loose copy has its `mtime` in `loose_mtimes`.

### HashIndex

`HashIndex` is one hash map from object hash to `ObjectId`, for all object types. Pass 1 uses it to find if it knows the hash of each copy. Pass 2 uses it to find the child index of each tree entry. The last steps use it to find the targets of commits, tags, and refs.

### TreeEntries

`TreeEntries` contains the entries of all trees in four parallel arrays:

```
start: [0, 3, 5, ...]              tree 0 = entries 0..3, tree 1 = entries 3..5
child: [12, 7, NO_CHILD, 40, ...]  index of the child tree or child blob
name:  [n4, n9, n2, n4, ...]       NameId of the entry name
mode:  [1, 0, 4, 1, ...]           FileKind, one byte
```

The entries of tree `t` are at positions `start[t]..start[t + 1]`, in the sequence of the tree body. `TreeEntries::entries(t)` gives each entry as (child index, `NameId`, `FileKind`). The `FileKind` tells if the child index is a tree index or a blob index.

Each entry uses 9 bytes: 4 bytes for the child, 4 bytes for the name, and 1 byte for the kind. A large history can have more than 100 million tree entries. For example, the Linux history has approximately 393 million tree entries, which use approximately 3.5 GB.

### NameStore

Each version of a directory has the same entry names again. Thus a large history has more than 100 million tree entries, but only approximately 100,000 different names.

The `NameStore` keeps one copy of each different name. Each tree entry keeps a 4-byte `NameId` that refers to its name. The `NameStore` uses the `intaglio` symbol table with the `ahash` hash function.

The `NameStore` keeps names as bytes, because Git does not make it necessary for names to be UTF-8. A `NameId` is correct only with the `NameStore` that made it. Thus `ParsedObjects` keeps the `NameStore` with `TreeEntries`, and `ObjectContainer` does not contain a `NameId`.

## Git object formats

### Loose objects

Location: `.git/objects/xx/yyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyy`

A loose object is one zlib stream. When you decompress the stream, the result has this format:

```
<type> <size>\0<content>
```

### Pack files

Location: `.git/objects/pack/pack-<hash>.pack`, with the index in `pack-<hash>.idx`.

The pack header is 12 bytes:

```
"PACK"         4 bytes
Version        4 bytes, big-endian (2 or 3)
Object count   4 bytes, big-endian
```

Each pack entry has this format:

```
Type and size       variable length: a 3-bit type and a variable-length size
Delta base          only for a delta: an offset (type 6) or a hash (type 7)
Compressed data     zlib
```

These are the type values:

| Value | Type |
|---|---|
| 1 | Commit |
| 2 | Tree |
| 3 | Blob |
| 4 | Tag |
| 6 | Offset delta (`OBJ_OFS_DELTA`). The base is at a lower offset in the same pack file. |
| 7 | Ref delta (`OBJ_REF_DELTA`). The delta gives the hash of the base. The pack index gives the offset of that hash. |

### Commit body

```
tree <40-hex hash>
parent <40-hex hash>       (zero or more)
author <name> <email> <timestamp> <timezone>
committer <name> <email> <timestamp> <timezone>

<commit message>
```

The parser reads the `tree`, `parent`, and `committer` lines. It does not read the message.

### Tree body (binary)

```
<mode> <name>\0<20-byte hash>
<mode> <name>\0<20-byte hash>
...
```

These are the modes:

| Mode | `FileKind` | Child |
|---|---|---|
| `40000` | `Tree` | A tree |
| `100644` | `Regular` | A blob |
| `100755` | `Executable` | A blob |
| `120000` | `Symlink` | A blob |
| `160000` | `Submodule` | A commit in a different repository |

The parser records an entry with a different mode as `Regular`.

### Tag body

```
object <40-hex hash>
type <object type>
tag <tag name>
tagger <name> <email> <timestamp> <timezone>

<tag message>
```

The parser reads the `object` and `tag` lines.

## Errors

The parser uses `anyhow` errors. For most incorrect data in the object store, the error is a `ParseError` (`thiserror`). The two passes and the commands add the object hash or the file name to each error.

## Tests

The tests are at three levels:

- Unit tests are in `#[cfg(test)]` modules in the source files. They use data that the test makes.
- Integration tests in `tests/` make small repositories with `git`. They compare the result of the parser with the output of `git`.
- The `git_oracle` test parses large repositories and compares the result with `git`. It runs only when you tell Cargo to run it.

The benchmarks are in `#[cfg(feature = "bench")]` modules in the source files. For more data, refer to [docs/benchmarks/README.md](docs/benchmarks/README.md).
