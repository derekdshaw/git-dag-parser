# Architecture

This document tells how git-dag-parser reads a Git repository.

The parser reads the Git object store directly. It does not start `git` processes. It reads the pack files and the loose objects in two passes:

1. Pass 1 reads only the object headers. It finds all objects, their types, their sizes, and all copies of each object on disk.
2. Pass 2 reads the bodies of commits, trees, and tags. It does not read blob bodies.

After the two passes, the parser connects commits, tags, and refs to the objects that they refer to. Pass 2, and the header read and the sorts by hash of pass 1, use more than one thread. The other steps use one thread.

For the meaning of terms such as work unit, worker, and delta family, refer to "Terms" in [README.md](README.md).

## Source files

| File | Contents |
|---|---|
| `src/main.rs` | Sets the memory allocator, the log level, and the number of threads. Then it runs the command. |
| `src/cli.rs` | The command-line arguments (`clap`). |
| `src/commands/init.rs` | The `init` command. It finds the Git directory with `find_git_dir`, parses the objects, and shows the summary. |
| `src/repo.rs` | `find_git_dir`, which finds the Git directory of a repository. |
| `src/commands/analyze.rs`, `src/commands/query.rs` | Commands that show only a message at this time. |
| `src/parser/mod.rs` | `parse_objects`: pass 1, pass 2, and the last steps. |
| `src/parser/scan.rs` | Pass 1. |
| `src/parser/bodies.rs` | Pass 2: the work queue, the workers, and the merge of their results. |
| `src/parser/work.rs` | The delta families of a pack, the work units, and the size of a unit. |
| `src/parser/pack.rs` | Pack index files, pack entry headers, zlib streams, deltas, and `PackReader`. |
| `src/parser/delta_cache.rs` | The delta cache, the request times of the bases, and the simulation of the cache in pass 1. |
| `src/parser/loose.rs` | Loose object headers and bodies. |
| `src/parser/content.rs` | The parsers for commit, tree, and tag bodies, and `CommitRaw`. |
| `src/parser/refs.rs` | The refs from `packed-refs`, from loose ref files, and from a detached `HEAD`. |
| `src/models/objects.rs` | `Commit`, `Tree`, `Blob`, `Tag`, `Ref`, `Pack`, `ObjectCopy`, and `ObjectId`. |
| `src/models/container.rs` | `ObjectContainer`, `HashIndex`, `FileKind`, `TreeEntries`, and `TreeEntriesBuilder`. |
| `src/models/name_store.rs` | `NameStore`, `NameId`, and `NameMap`. |
| `src/error.rs` | The error types. |

## Parse sequence

```
  objects/pack/*.idx, *.pack             objects/xx/yyyy... (loose)
               |                                   |
               v                                   v
 +---------------------------------------------------------------+
 | Pass 1: headers (scan.rs)                                     |
 |   read each pack index, open each pack file with mmap         |
 |   read the pack entry headers, on more than one thread        |
 |   find the object type of each entry, in offset sequence      |
 |   read the loose object headers                               |
 |   select the primary copy of each object                      |
 |   sort the objects of each type by hash, on more than one     |
 |   thread                                                      |
 +---------------------------------------------------------------+
               |  ObjectContainer, HashIndex, open pack files
               v
 +---------------------------------------------------------------+
 | Pass 2: bodies (bodies.rs), on more than one thread           |
 |   cut the reads into work units of whole delta families       |
 |   each worker takes units from one queue and, for each read:  |
 |     decompress commits, trees, and tags (not blobs)           |
 |     commit body -> CommitRaw                                  |
 |     tree body   -> its TreeEntriesBuilder, its NameStore      |
 |     tag body    -> Tag name and target hash                   |
 |   merge: one NameStore, TreeEntries in tree index order       |
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
          ParseResult
```

### Pass 1: object headers

Pass 1 (`scan_repository`) finds all pack files in `objects/pack/`. It uses only a pack file that has an `.idx` file. It sorts the pack files by name.

For each pack file, pass 1 reads the `.idx` file and opens the `.pack` file with `mmap`. The pack files stay open until `parse_objects` is complete. If a different process changes a pack file while the program parses the repository, the result is not correct and the program can stop.

Pass 1 reads the header of each pack entry on the `rayon` thread pool, the same pool as pass 2. It cuts the entries, in offset sequence, into chunks of 4,096, and each task reads one chunk from the start to the end. The chunks do not depend on the number of threads. A pack entry header gives the object type and the object size. Pass 1 does not decompress the object bodies.

A delta entry does not give the object type or the object size directly:

- To find the object type, pass 1 uses the type of the base. The base can be in a different chunk. Thus pass 1 finds the types after it reads all headers, on one thread, in offset sequence. For an offset delta, the base is always at a lower offset, so its type is already known. A ref delta can have a base at a higher offset. Then pass 1 follows the chain of delta bases to an entry that is not a delta.
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

When pass 1 finds an object for the first time, it gives the object a temporary index. At the end of pass 1, `Scan::finish` sorts the objects of each type by hash, on the `rayon` thread pool. The position of an object in the sorted array is its last index. `Scan::finish` then changes the temporary index to the last index in the copies and in the `HashIndex`. It does not build the `HashIndex` again.

Thus the index of an object does not change when Git moves objects into different pack files. `Scan::finish` also changes all temporary indices in the copies to the last indices. Then it makes a new `HashIndex`.

### Pass 2: object bodies

Pass 2 (`read_bodies`) reads the body of each commit, tree, and tag from its primary copy. It does not read blob bodies.

#### Work units

Pass 2 cuts the reads into work units (`units`, `work::work_units`):

- Each delta has one base. Thus the deltas of a pack make a set of trees, the delta families. A family is an entry that is not a delta, and all the deltas whose chains end at it. `family_roots` finds the family of each entry.
- A unit contains whole families of one pack. Thus a worker never needs a base that a different worker made, and no base is made two times. `work_units` puts families in a unit, in the sequence of their first read, until the unit has the target number of reads. A family is never cut, so a unit can have more reads than the target. The reads of a unit are in offset sequence.
- The loose copies are in units of their own, after the units of the packs.
- `unit_reads` sets the target: about 8 units for each thread, and at most 4,096 reads. Families are small: the largest family of linux has 0.14% of its reads.

#### Workers

The units of all packs and the loose units are one queue. The `rayon` thread pool reads the units in parallel. The workers borrow the open pack files and the result of pass 1, and they only read them. No worker changes the `ObjectContainer` during pass 2.

Each worker keeps its results in its own `Parsed`: the `CommitRaw` records, the tag names and targets, a `TreeEntriesBuilder`, and a `NameStore`. Workers do not share a name store, and they do not use locks for names. A `Parsed` is for one worker, not for one unit: its arrays are a few large allocations. One set for each unit (1,735 units for linux) made the peak memory 3.4 GB higher.

`main.rs` sets the number of threads (`parser::default_threads`): a quarter of the hardware threads, but up to 8 on a small computer, and always one fewer than the computer has. On 128 hardware threads, pass 2 is not faster with more than about 32 threads, and each thread adds to the peak memory. The `RAYON_NUM_THREADS` environment variable sets a different number.

#### Pack reader and delta cache

For each pack unit, the worker makes a `PackReader`. The reader has one zlib decoder. It resets the decoder for each entry, and it allocates the output one time, with the size from the entry header. If the entry is a delta, `PackReader` makes the base object and applies the delta to it. `PackReader` keeps the base objects in a delta cache (`DeltaCache`):

- Pass 1 records the base of each delta (`DeltaGraph`). Before a reader reads its unit, `request_times` calculates when the unit needs each base. It keeps these times in a map with only the entries that the unit needs. The cache keeps a base only until its last request.
- The cache has a byte limit. The limit of all workers together is 1 GiB, or the value of `init --delta-cache-size`. Each reader gets an equal share. When the cache is full, it removes the bases that the unit needs last. If the unit needs the new base after all the bases in the cache, the cache does not keep the new base.
- A cache hit gives a reference to the data in the cache. It does not copy the data.
- A unit needs much less cache than a full pack. For linux, the largest peak of a unit is 30 MB, and the peak of the full pack in one cache was 1.18 GB.

The log shows the hits, misses, and evictions of each pack, summed over its units, and the largest peak of one unit. The simulation of pass 1 (`simulate_cache`) is for a full pack in one cache. Thus its peak is larger than the peak of a unit.

The program uses `mimalloc` as its memory allocator. With the glibc allocator, pass 2 was slow, because the delta cache releases bases of many sizes in a mixed sequence. `main.rs` also sets the mimalloc purge delay to 0. With the default delay, mimalloc gives released memory back to the operating system only after 1 to 10 seconds. In that time, the workers release and allocate much memory: on 32 threads, linux used 12.1 GB in pass 2 with the default delay, and 8.4 GB with no delay.

Pass 2 makes these data from the bodies:

| Object type | Data |
|---|---|
| Commit | A `CommitRaw`: the root tree hash, the parent hashes, and the commit time. |
| Tree | The tree entries in the worker's `TreeEntriesBuilder`, and the entry names in the worker's `NameStore`. |
| Tag | The tag name and the target hash. The merge writes them in the `Tag`. |

#### Tree entries

`parse_tree` reads the entries of a tree body. For each entry, it does these steps:

1. It puts the entry name in the worker's `NameStore` and gets a `NameId`.
2. It finds the index of the child object in the `HashIndex`. This is possible because the `HashIndex` contains all objects at the end of pass 1.
3. It records the child index, the `NameId`, and the `FileKind` in the worker's `TreeEntriesBuilder`.

A submodule entry has no child index, because it refers to a commit in a different repository. An entry has no child index also when its child is not in the object store.

A worker reads the trees in unit sequence, not in index sequence. Its `TreeEntriesBuilder` records the entries in the sequence that it gets them, and the span of each tree that it reads.

#### Merge

After all workers are done, `merge` puts their results together:

1. `NameStore::merge` puts the names of each worker's store in one store, in worker sequence. For each worker, it gives a `NameMap` from the worker's `NameId` values to the `NameId` values of the merged store.
2. The `CommitRaw` records go into one `HashMap`, and the tag names and targets go into the `Tag` objects.
3. `TreeEntries::merge` puts the entries of all workers in tree index sequence. It changes each `NameId` to the merged `NameId` in the same step. It does this for one array at a time, and it releases the worker arrays after it copies them. This decreases the maximum memory. It copies each array in parallel, in runs of about one million entries.

The `NameId` of a name can be different from one parse to the next, because it depends on which worker found the name first. The tree entries are the same for each parse and each number of threads.

### Last steps

After pass 2, `parse_objects` does these steps:

1. For each commit, it finds the index of the root tree and the index of each parent. If a parent is not in the object store, it sets `boundary` to `true`. This occurs at the edge of a shallow clone.
2. For each annotated tag, it finds the `ObjectId` of the target.
3. It reads the refs. Then it finds the `ObjectId` of the target of each ref.

`read_refs` reads `packed-refs`, the loose ref files in `refs/`, and a detached `HEAD`. A loose ref replaces a packed ref with the same name, because Git writes a loose file when it updates a ref. The parser does not keep symbolic refs, because it reads the ref that a symbolic ref refers to.

### Result

`parse_objects` gives a `ParseResult` struct:

| Field | Contents |
|---|---|
| `tree_entries` | The entries of all trees. |
| `names` | The entry names that `tree_entries` refers to. |
| `hash_index` | The `ObjectId` of each hash. |
| `container` | All objects, copies, pack files, and refs. |

The `CommitRaw` records are not in the result. After the last steps, each `Commit` has its tree, parents, and commit time as indices and values.

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

Each entry uses 9 bytes: 4 bytes for the child, 4 bytes for the name, and 1 byte for the kind. A large history can have more than 100 million tree entries. For example, the Linux history has approximately 528 million tree entries, which use approximately 4.7 GB.

### NameStore

Each version of a directory has the same entry names again. Thus a large history has more than 100 million tree entries, but only approximately 100,000 different names.

The `NameStore` keeps one copy of each different name. Each tree entry keeps a 4-byte `NameId` that refers to its name. The `NameStore` uses the `intaglio` symbol table with the `ahash` hash function.

The `NameStore` keeps names as bytes, because Git does not make it necessary for names to be UTF-8. A `NameId` is correct only with the `NameStore` that made it. In pass 2, each worker has its own `NameStore`, and the merge makes one store from them (see "Merge"). Thus `ParseResult` keeps the `NameStore` with `TreeEntries`, and `ObjectContainer` does not contain a `NameId`.

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

The CI workflow runs the unit tests and the integration tests on Linux, macOS, and Windows. It does not run `git_oracle` or the benchmarks.

The benchmarks are in `#[cfg(feature = "bench")]` modules in the source files. For more data, refer to [docs/benchmarks/README.md](docs/benchmarks/README.md).
