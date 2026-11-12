# Architecture

This document tells how git-dag-parser reads a Git repository.

The parser reads the Git object store directly. It does not start `git` processes. It reads the pack files and the loose objects in two passes:

1. Pass 1 reads only the object headers. It finds all objects, their types, their sizes, and all copies of each object on disk.
2. Pass 2 reads the bodies of commits, trees, and tags. It does not read blob bodies.

After the two passes, the parser connects commits, tags, and refs to the objects that they refer to. It also reads the reflogs and the index file. Pass 2, and the header read and the sorts by hash of pass 1, use more than one thread. The other steps of the parse use one thread.

After the parse, the program ranks the commits and finds the commit that introduced each tree and each blob (see "Attribution"). Then it finds the path where each tree and blob was added (see "Paths"), and the liveness of each object (see "Reachability").

For the meaning of terms such as work unit, worker, and delta family, refer to "Terms" in [README.md](README.md).

## Crates

The repository is a Cargo workspace with three crates:

| Crate | Path | Contents |
|---|---|---|
| `git-dag-parser` | The root | The program: the command-line arguments, the commands, the attribution, the paths, the reachability, the progress bar, and `DagError`. |
| `git-dag-parser-core` | `crates/git-dag-parser-core` | The parser library: the parser, the data model, `find_git_dir`, the byte sizes, `ParseError`, and the benchmarks. |
| `test-fixtures` | `crates/test-fixtures` | The `Repo` test fixtures: small repositories that the tests make with `git`, and the output of `git` for them. Cargo does not publish this crate. The other two crates use it only in their tests. |

The library reads the object store and gives the object graph: all objects, and the edges from commits, tags, refs, and tree entries to other objects, as indices. The calculations on this graph go in the program. The library does not draw progress bars, and it does not set the memory allocator or the number of threads. The program does these things.

## Source files

The program, at the root:

| File | Contents |
|---|---|
| `src/main.rs` | Sets the memory allocator, the log output, and the number of threads. Then it runs the command. |
| `src/cli.rs` | The command-line arguments (`clap`). |
| `src/progress.rs` | `ProgressLayer`, which draws the progress bar of pass 2 from the progress events of the library. |
| `src/commands/init.rs` | The `init` command. It finds the Git directory with `find_git_dir`, parses the objects, calculates the attribution, the paths, and the reachability, and shows the summary. |
| `src/attribution/mod.rs` | `Attribution`: the corrected date and the rank of each commit, and the commit that introduced each tree and blob. |
| `src/attribution/rank.rs` | The corrected dates and the rank order of the commits. |
| `src/attribution/owner.rs` | The owner of each tree and blob, with the frontier method. |
| `src/paths.rs` | `Paths`: the path table, and the path id of each tree and blob. |
| `src/reachability.rs` | `Liveness` and `Reachability`: the liveness of each object, and the totals of each state. |
| `src/commands/analyze.rs`, `src/commands/query.rs` | Commands that show only a message at this time. |
| `src/error.rs` | `DagError`. |

The library, in `crates/git-dag-parser-core`:

| File | Contents |
|---|---|
| `src/repo.rs` | `find_git_dir`, which finds the Git directory of a repository, and `check_object_format`, which stops the parse of a SHA-256 repository. |
| `src/parser/mod.rs` | `parse_objects`: pass 1, pass 2, and the last steps. |
| `src/parser/scan.rs` | Pass 1. |
| `src/parser/bodies.rs` | Pass 2: the work queue, the workers, and the merge of their results. |
| `src/parser/work.rs` | The delta families of a pack, the work units, and the size of a unit. |
| `src/parser/pack.rs` | Pack index files, pack entry headers, zlib streams, deltas, and `PackReader`. |
| `src/parser/delta_cache.rs` | The delta cache, the request times of the bases, and the simulation of the cache in pass 1. |
| `src/parser/loose.rs` | Loose object headers and bodies. |
| `src/parser/content.rs` | The parsers for commit, tree, and tag bodies, and `CommitRaw`. |
| `src/parser/refs.rs` | The refs from `packed-refs`, from loose ref files, and from a detached `HEAD`. |
| `src/parser/reflogs.rs` | The reflog entries of `logs/HEAD` and of the files under `logs/refs/`. |
| `src/parser/index.rs` | The objects that the index file keeps, from index versions 2, 3, and 4, the `TREE` and `REUC` extensions, and a split index. |
| `src/models/objects.rs` | `Commit`, `Tree`, `Blob`, `Tag`, `Ref`, `ReflogEntry`, `Pack`, `ObjectCopy`, and `ObjectId`. |
| `src/models/container.rs` | `ObjectContainer`, `HashIndex`, `FileKind`, `TreeEntries`, and `TreeEntriesBuilder`. |
| `src/models/name_store.rs` | `NameStore`, `NameId`, and `NameMap`. |
| `src/size.rs` | Byte sizes on the command line and in messages. |
| `src/error.rs` | `ParseError`. |
| `benches/parser.rs` | Runs the benchmarks (see "Tests"). |

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
 |   read reflogs (logs/), old and new hashes -> indices         |
 |   read the index file, its object hashes -> indices           |
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

#### Progress

Pass 2 sends its progress as `tracing` events at the `info` level, with the target `PROGRESS_TARGET` (`git_dag_parser_core::progress`):

- At the start, one event with the number of reads (`total`).
- After each work unit, one event with the number of reads of the unit (`done`).
- At the end, one event with `finish`.

Pass 2 sends one event for each unit, not for each read, because tens of millions of events make pass 2 slower. The program draws the progress bar from these events (`ProgressLayer`). The log does not show these events. If the log level is lower than `info`, the program does not get the events, and it does not show a progress bar. Pass 1 does not send progress events.

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
2. The `CommitRaw` records of all workers go into one array, and the tag names and targets go into the `Tag` objects.
3. `TreeEntries::merge` puts the entries of all workers in tree index sequence. It changes each `NameId` to the merged `NameId` in the same step. It does this for one array at a time, and it releases the worker arrays after it copies them. This decreases the maximum memory. It copies each array in parallel, in runs of about one million entries.

The `NameId` of a name can be different from one parse to the next, because it depends on which worker found the name first. The tree entries are the same for each parse and each number of threads.

### Last steps

After pass 2, `parse_objects` does these steps:

1. For each commit, it finds the index of the root tree and the index of each parent. If a parent is not in the object store, it sets `boundary` to `true`. This occurs at the edge of a shallow clone.
2. For each annotated tag, it finds the `ObjectId` of the target.
3. It reads the refs. Then it finds the `ObjectId` of the target of each ref.
4. It reads the reflogs. Then it finds the `ObjectId` of the old hash and of the new hash of each reflog entry.
5. It reads the index file. Then it finds the `ObjectId` of each hash that the index file keeps. It sorts these ids and removes the duplicates.

`read_refs` reads `packed-refs`, the loose ref files in `refs/`, and a detached `HEAD`. A loose ref replaces a packed ref with the same name, because Git writes a loose file when it updates a ref. The parser does not keep symbolic refs, because it reads the ref that a symbolic ref refers to.

`read_reflogs` reads `logs/HEAD` and all files under `logs/refs/`, also `logs/refs/stash`, which keeps the older stash entries. It does not read files that end in `.lock`. The logs are in ref name sequence, and the entries of one log are in file sequence (the oldest first). An all-zero hash means that the ref was made or deleted: the entry has no object for it. A line that is not valid is not used, as in Git, and the log shows a warning. The parser does not read the reflogs of linked work trees (`worktrees/*/logs`).

`read_index` reads `index`. If there is no index file, as in a bare repository, it gives no hashes. It gives the hashes that `git rev-list --indexed-objects` uses:

- The hash of each entry. This is usually a blob. For a sparse directory entry (mode `040000`), it is a tree. A submodule entry (mode `160000`) refers to a commit in a different repository, thus the parser does not use it.
- The tree hash of each valid node of the cache-tree (`TREE` extension). A node that is not valid has the entry count -1 and no hash.
- The blob hashes of resolve-undo (`REUC` extension). Git keeps the stages of a resolved conflict there, and `git gc` keeps these blobs.

The parser reads index versions 2, 3, and 4. In version 3 and 4, an entry with the extended flag has 2 more bytes of flags. Version 4 compresses the name of each entry against the name of the entry before it. The parser does not make the names, because it uses only the hashes. A split index (`link` extension) keeps most entries in `sharedindex.<hash>`. The `link` extension has two bitmaps in EWAH format: the shared entries that the index deletes, and the shared entries that the first entries of the index replace. The parser uses the shared entries that the index does not delete or replace, and all entries of the index. On an index of 155,000 entries, `read_index` uses approximately 10 ms, with or without a split index. Most of this time is for the read of the file. The parser does not read the index files of linked work trees (`worktrees/*/index`). Hashes that are not in the object store get no `ObjectId`.

### Result

`parse_objects` gives a `ParseResult` struct:

| Field | Contents |
|---|---|
| `tree_entries` | The entries of all trees. |
| `names` | The entry names that `tree_entries` refers to. |
| `hash_index` | The `ObjectId` of each hash. |
| `container` | All objects, copies, pack files, refs, reflog entries, and index roots. |
| `delta_cache` | The delta cache hits, misses, and evictions of pass 2 for each pack, in the sequence of `container.packs`. |
| `pass1`, `pass2` | The time of pass 1 and of pass 2. |

The `CommitRaw` records are not in the result. After the last steps, each `Commit` has its tree, parents, and commit time as indices and values.

## Attribution

After the parse, `init` calls `Attribution::calculate` (`src/attribution/`). It finds the commit that introduced each tree and each blob. This commit is the **owner** of the object.

### Rank

The **corrected date** of a commit is the larger of these two values:

- its committer time,
- the largest corrected date of its parents, plus 1.

Thus the corrected date of a commit is always larger than the corrected date of each parent. Git commit-graph files use the same value. `rank.rs` calculates it with Kahn's algorithm: a commit is done when all its parents are done. It does not use recursion. A parent that is not in the object store is not in `Commit.parents`, thus a commit at a shallow clone boundary starts from its own time. If the commit graph has a cycle, the calculation stops with an error.

The **rank** of a commit is its position when the commits are sorted by (corrected date, hash). The commit indices are in hash order, so the sort uses (corrected date, index). An ancestor always has a lower rank than its descendants. The hash is used only for commits that are not ancestors of each other.

The summary shows the number of commits whose corrected date is not equal to their committer time. This number is not only the commits with a clock error. It also includes each commit that has a parent with the same committer time, to the second, as when a tool such as `git am` or `git rebase` makes many commits in one second. On linux (2026-11-12), 593,467 of 1,309,717 commits have a corrected date: 466,462 have a parent with the same time, and only 807 have a parent with a later time.

### Owner

The owner of a tree or blob is the commit with the lowest rank that can reach it:

```
owner(o) = min( rank(c) for each commit c with root tree o,
                owner(T) for each tree T that contains o )
```

A merge never owns an object that a parent has, because the parent has a lower rank. A rename, an empty commit, and a revert own no old blobs. An object that only an annotated tag reaches, and an object that nothing reaches, has no owner (`NOT_INTRODUCED`).

### Frontier method

`owner.rs` does not walk the tree of each commit. It moves through the tree graph one level at a time:

1. It sets the owner of each root tree to the lowest rank of the commits that have that tree.
2. It counts the **in-degree** of each tree: the number of tree entries that point to it, also duplicates.
3. The first **frontier** is all trees with in-degree 0. This includes the trees that no commit reaches.
4. In each round, the threads process the entries of the frontier trees. For each entry of tree `T`, the owner of the child becomes the minimum of its value and the owner of `T`. If the child is a tree, its in-degree decreases by 1. A tree whose in-degree becomes 0 goes into the next frontier.
5. The calculation stops when the frontier is empty.

A tree goes into a frontier only when all trees that contain it are done. Thus its owner is final when the threads process its entries. Each entry is processed two times in all: one time for the in-degree, and one time in a round. The number of rounds is the depth of the tree graph.

The owner and in-degree values are `AtomicU32`, because many trees contain the same child, and different threads process these trees at the same time. All atomic operations use `Ordering::Relaxed`: each round returns only after all its tasks are done, so the next round sees all writes. Before a `fetch_min`, a thread reads the value, and does the `fetch_min` only if the new value is smaller. Most shared children get their owner from the first tree, so most entries do only a read. A tree with many entries is cut into tasks of `ENTRY_CHUNK` (16,384) entries. The result is the same for each number of threads, because `min` gives the same result in any sequence.

### Measured times

The `Attribution:` line of `init` on 32 threads (harness, 2026-11-12):

| Repository | Commits | Trees | Blobs | Tree entries | Rounds | Attribution | Parse |
|---|---|---|---|---|---|---|---|
| vscode | 134,232 | 1,112,371 | 407,657 | 20,296,692 | 12 | 0.18 s | 1.70 s |
| git-ms | 216,599 | 368,707 | 206,612 | 147,106,148 | 8 | 0.15 s | 3.21 s |
| linux | 1,309,717 | 6,303,183 | 2,842,379 | 527,520,874 | 14 | 1.60 s | 12.43 s |
| A large monorepo | ~0.29 million | ~4.1 million | ~4.8 million | ~663 million | 17 | 0.83 s | 28–39 s |

The peak heap of `init` did not change: the parse has the peak, and the attribution arrays use less memory than pass 2 used. On linux, the peak `RssAnon` is 9.03 GB, and 6.50 GB in the attribution. On linux, the corrected dates use 0.6 s of the 1.6 s, because the commit indices are in hash order and each step of Kahn's algorithm goes to a different part of memory.

## Paths

After the attribution, `init` calls `Paths::calculate` (`src/paths.rs`). It finds the path where the owner commit added each tree and each blob (`introduced_at_path`).

### Introducing parent

An object gets its path from its **introducing parent**: a tree that contains the object and has the same owner. This tree always exists for an object that is not the root tree of its owner. Let object `o` have owner `c`. Commit `c` reaches `o` through a tree `T`. If `T` had an owner with a lower rank than `c`, then that commit also reaches `o`, and `o` would have that lower owner. Thus `T` has owner `c`.

The root tree of the owner commit has the empty path. An object that has no owner has no path (`NO_PATH`). If an object has more than one introducing parent, the parent with the lowest (tree index, entry index) gives the path. Tree indices are in hash order, thus the result does not change between runs.

`introducing_parents` finds the introducing parent of all objects in one pass over all tree entries, on all threads. For each entry of tree `T` whose child has the same owner as `T`, it does a `fetch_min` of (`T` << 32 | entry index) on an `AtomicU64` of the child. As in the owner calculation, a thread reads the value first, and does the `fetch_min` only if the new value is smaller.

### Path table

A path is its parent path plus one name. The path table is a prefix tree on the `NameStore`: each `Path` has a `parent`, a `name` (a `NameId`), and a `depth`. Row 0 is the empty path, with no parent, no name, and depth 0. The id of a path is its row. The table does not keep the bytes of the paths.

The calculation makes the paths in sequence of depth:

1. `tree_depths` finds the depth of the path of each tree. Each thread goes up the introducing parents to a tree with a known depth, then sets the depth of each tree on the way.
2. For each depth `d` from 1, the threads make a key (parent path, name, object) for each tree and blob of depth `d`, and sort the keys. The parent paths have depth `d - 1`, thus they have ids. All objects with the same parent path have the same depth, so one sort finds all objects with the same (parent path, name). Each different (parent path, name) gets a new row.
3. `sort_table` makes the full bytes of each path from the bytes of its parent, sorts the rows by these bytes, and gives the new ids in that sequence. Then it releases the bytes. Thus the ids do not change between runs.

`Paths::bytes` makes the full bytes of one path from the parent chain.

## Reachability

After the paths, `init` calls `Reachability::calculate` (`src/reachability.rs`). It finds the **liveness** of each commit, tree, blob, and tag. The roots are:

- the target of each ref: `Ref`,
- each object of the index file: `IndexOnly`,
- the old and the new object of each reflog entry: `ReflogOnly`.

An object gets the strongest state of the roots that reach it. The sequence is `Unreachable < ReflogOnly < IndexOnly < Ref`: a reflog entry expires, but an index entry stays until the user changes the index. An object that no root reaches is `Unreachable`. The result is one byte for each object, in one array for each object type.

The calculation has three steps:

1. **Roots.** Each root gets its state. A tag gives its state to its target, also to a different tag.
2. **Commits.** The commit ranks of the attribution are a topological order: a parent has a lower rank than its children. The threads make an array of the parents of each commit as ranks, in rank order. Then one thread reads the commits from the highest rank to the lowest, and gives each parent the strongest state of its children. A commit has its final state when the scan gets to it. On linux, a walk of the parents in commit index order (hash order) used approximately 200 ms on one thread, because each step went to a different part of memory. In rank order, the scan reads the arrays in sequence.
3. **Trees and blobs.** Each commit gives its state to its root tree. Then the threads walk the trees one level at a time, as in the owner calculation, with `fetch_max`. The walk of the strongest state goes first. A walk of a weaker state stops at an object that already has a state, because the stronger walk also reached all objects below it. Thus each tree is walked at most one time, and the walk does not need the in-degree of the trees. A tree goes into the next level only from the thread whose `fetch_max` changed its state.

`Reachability::totals` gives the number of objects and the sum of `size_disk` in each state.

### Measured times of the paths and the reachability

The `Paths:` and `Reachability:` lines of `init` on 32 threads (harness, 2026-11-12):

| Repository | Distinct paths | Paths | Reachability | Attribution | Parse |
|---|---|---|---|---|---|
| vscode | 39,940 | 0.24 s | 0.16 s | 0.18 s | 1.76 s |
| git-ms | 6,976 | 0.10 s | 0.11 s | 0.17 s | 3.47 s |
| linux | 160,459 | 0.82 s | 0.50 s | 1.58 s | 12.80 s |
| A large monorepo | ~0.53 million | 0.93 s | 0.46 s | 0.73 s | 35.1 s |

The peak heap of `init` did not change: pass 2 has the peak. On linux, the peak `RssAnon` is 6.56 GB in the paths and 6.51 GB in the reachability, and 8.89 GB in pass 2.

On linux, these parts use most of the time:

- The pass over all tree entries that finds the introducing parents: approximately 0.31 s.
- The rounds by depth of the paths: approximately 0.38 s. Of this time, the selection of the objects of each depth and the make of their keys use approximately 0.21 s, the sorts 0.08 s, and the new rows and the path ids 0.10 s.
- The walk of the trees for the liveness: approximately 0.39 s.

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
| `reflog_entries` | Each `ReflogEntry`: ref name, old and new hash, old and new target, time, and message. |
| `index_roots` | The `ObjectId` of each object that the index file keeps, sorted, with no duplicates. |

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

The entries of tree `t` are at positions `start[t]..start[t + 1]`, in the sequence of the tree body. `TreeEntries::entries(t)` gives each entry as (child index, `NameId`, `FileKind`). The `FileKind` tells if the child index is a tree index or a blob index. `TreeEntries::children(t)` gives the `child` and `mode` slices of tree `t`, for work on many entries that does not use the names. `TreeEntries::names(t)` gives the `name` slice of tree `t`.

Each entry uses 9 bytes: 4 bytes for the child, 4 bytes for the name, and 1 byte for the kind. A large history can have more than 100 million tree entries. For example, the Linux history has approximately 528 million tree entries, which use approximately 4.7 GB.

### NameStore

Each version of a directory has the same entry names again. Thus a large history has more than 100 million tree entries, but only approximately 100,000 different names.

The `NameStore` keeps one copy of each different name. Each tree entry keeps a 4-byte `NameId` that refers to its name. The `NameStore` uses the `intaglio` symbol table with the `ahash` hash function.

The `NameStore` keeps names as bytes, because Git does not make it necessary for names to be UTF-8. A `NameId` is correct only with the `NameStore` that made it. The order of `NameId` values is the order of the ids, not of the name bytes. In pass 2, each worker has its own `NameStore`, and the merge makes one store from them (see "Merge"). Thus `ParseResult` keeps the `NameStore` with `TreeEntries`, and `ObjectContainer` does not contain a `NameId`.

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

### Reflog line

Location: `.git/logs/HEAD` and `.git/logs/refs/...`, one line for each change of the ref.

```
<old 40-hex hash> <new 40-hex hash> <name> <<email>> <timestamp> <timezone>\t<message>
```

The tab and the message are optional. The parser reads the two hashes, the timestamp, and the message.

### Index file

Location: `.git/index`. All numbers are big-endian.

```
"DIRC"              4 bytes
Version             4 bytes (2, 3, or 4)
Entry count         4 bytes
Entries
Extensions          each: 4-byte signature, 4-byte size, data
Checksum            20 bytes
```

Each entry has 40 bytes of times, device, inode, mode, user, group, and size, then the 20-byte hash and 2 bytes of flags. In version 3 and 4, an entry with the extended flag (`0x4000`) has 2 more bytes of flags. Then the name:

- Version 2 and 3: the name and 1 to 8 NUL bytes, so that the entry length is a multiple of 8.
- Version 4: a variable-length number (how many bytes of the name before to remove), then the rest of the name and one NUL byte.

The parser uses these extensions:

| Signature | Contents |
|---|---|
| `TREE` | Cache-tree nodes: `<path>\0<entry count> <subtree count>\n`, then the tree hash if the entry count is not -1. |
| `REUC` | Resolve-undo records: `<path>\0`, the octal modes of stages 1, 2, and 3, each with a NUL byte, then one hash for each mode that is not 0. |
| `link` | Split index: the hash of the shared index file, then the delete bitmap and the replace bitmap in EWAH format. |

The parser does not use the other extensions, because they contain no object hashes.

## Errors

The library and the program use `anyhow` errors. For most incorrect data in the object store, the error is a `ParseError` (`thiserror`) of the library. If `find_git_dir` finds no Git directory, the error is also a `ParseError`. The parser reads only SHA-1 hashes. If the `config` file sets `extensions.objectFormat` to `sha256`, `parse_objects` stops with `ParseError::UnsupportedObjectFormat` before it reads an object. The two passes and the commands add the object hash or the file name to each error. `DagError` is for the errors of the program.

## Tests

The tests are at three levels:

- Unit tests are in `#[cfg(test)]` modules in the source files. They use data that the test makes.
- Integration tests in `crates/git-dag-parser-core/tests/` make small repositories with `git`, with the `Repo` fixtures of `crates/test-fixtures`. They compare the result of the parser with the output of `git`.
- Integration tests of the program in the root `tests/` use the same fixtures. `tests/attribution.rs` compares the attribution with `git rev-list` and with a walk of the commits in rank order on one thread. `tests/paths.rs` compares the path of each object with `git ls-tree`. `tests/reachability.rs` compares the liveness of each object with `git rev-list --all`, `--indexed-objects`, and `--reflog`.
- The `git_oracle` test parses large repositories and compares the result with `git`. It runs only when you tell Cargo to run it.

The CI workflow runs the unit tests and the integration tests on Linux, macOS, and Windows. It does not run `git_oracle` or the benchmarks.

The benchmarks are in `#[cfg(feature = "bench")]` modules in the source files of the library. For more data, refer to [docs/benchmarks/README.md](docs/benchmarks/README.md).
