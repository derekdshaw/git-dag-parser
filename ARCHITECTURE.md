# Git DAG Parser Architecture

A fast Git repository analyzer that directly parses Git's object store to build relationship graphs between commits, trees, and blobs.

## Overview

This tool replaces the traditional approach of spawning `git` processes with direct parsing of Git's internal data structures (pack files and loose objects). This eliminates process spawning overhead and enables building relationships during parsing.

## Goals

1. **Eliminate process spawning** - No `git rev-list` or `git cat-file` calls
2. **Single-pass I/O** - Memory-map pack files, read once
3. **Build relationships during parsing** - No separate dependency resolution phase
4. **Lock-free parallelism** - Workers return results, main thread merges
5. **Rich relationships** - Bidirectional links (Tree↔Blob, Commit↔Tree, etc.)

## Architecture

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                              PHASE 1: PARSE                                  │
│                           (Single-threaded I/O)                              │
│                                                                              │
│  ┌─────────────────┐    ┌─────────────────┐    ┌─────────────────┐          │
│  │ Pack Index      │    │ Pack File       │    │ Loose Objects   │          │
│  │ (.idx)          │    │ (.pack)         │    │ (.git/objects/) │          │
│  └────────┬────────┘    └────────┬────────┘    └────────┬────────┘          │
│           │                      │                      │                    │
│           └──────────────────────┼──────────────────────┘                    │
│                                  ▼                                           │
│                    ┌─────────────────────────┐                               │
│                    │   Object Parser         │                               │
│                    │                         │                               │
│                    │   - Decompress zlib     │                               │
│                    │   - Parse headers       │                               │
│                    │   - Resolve deltas      │                               │
│                    └────────────┬────────────┘                               │
│                                 │                                            │
│           ┌─────────────────────┼─────────────────────┐                      │
│           ▼                     ▼                     ▼                      │
│  ┌─────────────────┐  ┌─────────────────┐  ┌─────────────────┐              │
│  │ commits_raw     │  │ trees_raw       │  │ ObjectContainer │              │
│  │ HashMap<Hash,   │  │ HashMap<Hash,   │  │ commits[]       │              │
│  │   CommitRaw>    │  │   TreeRaw>      │  │ trees[]         │              │
│  │                 │  │                 │  │ blobs[]         │              │
│  └─────────────────┘  └─────────────────┘  │ tags[]          │              │
│                                            └─────────────────┘              │
│                                                                              │
│  Static relationships built during Phase 1:                                  │
│    - Tree → Blob (direct children)                                          │
│    - Tree → Subtree (direct children)                                       │
│    - Blob → Tree (reverse, which trees contain this blob)                   │
└─────────────────────────────────────────────────────────────────────────────┘
                                 │
                                 ▼
┌─────────────────────────────────────────────────────────────────────────────┐
│                        PHASE 2: WALK & MERGE                                 │
│                    (Parallel workers + Single merger)                        │
│                                                                              │
│  ┌───────────────────────────────────────────────────────────────────────┐  │
│  │                           MAIN THREAD                                  │  │
│  │                                                                        │  │
│  │  1. Partition commits across workers                                   │  │
│  │  2. Spawn pinned worker threads                                        │  │
│  │  3. Receive WalkResults via channel                                    │  │
│  │  4. Merge into ObjectContainer (exclusive access, no locks)            │  │
│  └───────────────────────────────────────────────────────────────────────┘  │
│                                      │                                       │
│            ┌─────────────────────────┼─────────────────────────┐             │
│            │                         │                         │             │
│            ▼                         ▼                         ▼             │
│  ┌──────────────────┐    ┌──────────────────┐    ┌──────────────────┐       │
│  │ Worker 0 (CPU 0) │    │ Worker 1 (CPU 1) │    │ Worker N (CPU N) │       │
│  │                  │    │                  │    │                  │       │
│  │ READ-ONLY:       │    │ READ-ONLY:       │    │ READ-ONLY:       │       │
│  │  - trees_raw     │    │  - trees_raw     │    │  - trees_raw     │       │
│  │                  │    │                  │    │                  │       │
│  │ PURE FUNCTION:   │    │ PURE FUNCTION:   │    │ PURE FUNCTION:   │       │
│  │  walk_tree()     │    │  walk_tree()     │    │  walk_tree()     │       │
│  │       │          │    │       │          │    │       │          │       │
│  │       ▼          │    │       ▼          │    │       ▼          │       │
│  │  WalkResult      │    │  WalkResult      │    │  WalkResult      │       │
│  └────────┬─────────┘    └────────┬─────────┘    └────────┬─────────┘       │
│           │                       │                       │                  │
│           └───────────────────────┼───────────────────────┘                  │
│                                   │                                          │
│                                   ▼                                          │
│                      ┌────────────────────────┐                              │
│                      │   Channel (bounded)    │                              │
│                      └───────────┬────────────┘                              │
│                                  │                                           │
│                                  ▼                                           │
│                      ┌────────────────────────┐                              │
│                      │   Main Thread Merge    │                              │
│                      │   (no locks needed)    │                              │
│                      └────────────────────────┘                              │
└─────────────────────────────────────────────────────────────────────────────┘
```

## Git Object Formats

### Loose Objects

Location: `.git/objects/xx/yyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyy`

Format:
```
zlib_decompress → "<type> <size>\0<content>"
```

### Pack Files

Location: `.git/objects/pack/*.pack` with `.idx` index

Pack Header (12 bytes):
```
"PACK" (4 bytes)
Version (4 bytes, big-endian) - typically 2
Object count (4 bytes, big-endian)
```

Object Entry:
```
Type + Size (variable length, 3-bit type + variable size)
[Delta base reference if delta type]
Compressed data (zlib)
```

Object Types:
- 1 = OBJ_COMMIT
- 2 = OBJ_TREE
- 3 = OBJ_BLOB
- 4 = OBJ_TAG
- 6 = OBJ_OFS_DELTA (offset-based delta)
- 7 = OBJ_REF_DELTA (hash-based delta)

### Commit Object Content

```
tree <40-hex-sha1>
parent <40-hex-sha1>       (0 or more)
author <name> <email> <timestamp> <tz>
committer <name> <email> <timestamp> <tz>

<commit message>
```

### Tree Object Content (Binary)

```
<mode> <name>\0<20-byte-binary-sha1>
<mode> <name>\0<20-byte-binary-sha1>
...
```

Modes:
- `040000` - directory (subtree)
- `100644` - regular file
- `100755` - executable file
- `120000` - symbolic link

### Tag Object Content

```
object <40-hex-sha1>
type <object-type>
tag <tag-name>
tagger <name> <email> <timestamp> <tz>

<tag message>
```

## Data Structures

### Core Models

```rust
struct Commit {
    index: usize,
    size: u32,
    size_disk: u32,
    tree_deps: Vec<usize>,      // All trees reachable from this commit
    blob_deps: Vec<usize>,      // All blobs reachable from this commit
    tag_deps: Vec<usize>,       // Tags pointing to this commit
    lightweight_tags: Vec<String>,
}

struct Tree {
    index: usize,
    size: u32,
    size_disk: u32,
    paths: HashSet<String>,     // All paths where this tree appears
    commits: Vec<usize>,        // Commits that reference this tree
    blobs: Vec<usize>,          // Direct child blobs (static)
    subtrees: Vec<usize>,       // Direct child trees (static)
}

struct Blob {
    index: usize,
    size: u32,
    size_disk: u32,
    paths: HashSet<String>,     // All paths where this blob appears
    commits: Vec<usize>,        // Commits that reference this blob
    trees: Vec<usize>,          // Trees that directly contain this blob
}
```

### Intermediate Structures (Phase 1)

```rust
struct CommitRaw {
    index: usize,
    root_tree_hash: [u8; 20],
    parent_hashes: Vec<[u8; 20]>,
}

struct TreeRaw {
    index: usize,
    entries: Vec<TreeEntry>,
}

struct TreeEntry {
    mode: u32,
    name: String,
    hash: [u8; 20],
    child_idx: Option<usize>,   // Resolved after indexing
    is_tree: bool,
}
```

### Worker Result (Phase 2)

```rust
struct WalkResult {
    commit_idx: usize,
    trees_visited: Vec<(usize, String)>,   // (tree_idx, path)
    blobs_visited: Vec<(usize, String)>,   // (blob_idx, path)
}
```

## Relationships

### Built During Phase 1 (Static)

These relationships are inherent to the tree structure and don't change per-commit:

| Relationship | Description |
|--------------|-------------|
| Tree → Blob | Direct children of a tree |
| Tree → Subtree | Direct subdirectories of a tree |
| Blob → Tree | Reverse: which trees contain this blob |

### Built During Phase 2 (Dynamic)

These relationships depend on which commits reference which trees:

| Relationship | Description |
|--------------|-------------|
| Commit → Tree | All trees reachable from commit's root tree |
| Commit → Blob | All blobs reachable from commit's tree hierarchy |
| Tree → Commit | Reverse: which commits reference this tree |
| Blob → Commit | Reverse: which commits reference this blob |

## Threading Model

### Why Workers + Merge?

| Aspect | Rayon + Locks | Workers + Merge |
|--------|---------------|-----------------|
| Lock contention | High on hot trees | **Zero** |
| Cache locality | Poor (false sharing) | **Excellent** (pinned cores) |
| Worker complexity | Must handle locks | **Pure functions** |
| Debugging | Lock ordering issues | **Deterministic** |

### Implementation

1. **Main thread** partitions commits across N workers
2. **Worker threads**:
   - Pinned to CPU cores for cache locality
   - Have read-only access to `trees_raw`
   - Walk commit trees, producing `WalkResult`
   - Send results via bounded channel
3. **Main thread**:
   - Receives results from channel
   - Merges into `ObjectContainer` with exclusive access
   - No locks needed

## CLI Commands

### `init`

Parse a Git repository and build the relationship DAG.

```bash
git-dag-parser init --repo /path/to/repo
```

Phases:
1. Parse pack files and loose objects
2. Build static tree↔blob relationships
3. Walk commit trees with parallel workers
4. Merge results

### `analyze`

Generate reports from a parsed DAG.

```bash
git-dag-parser analyze --dag-file dag.bin
```

Reports:
- Largest blobs
- Most referenced trees
- Commit size contributions
- Path statistics

### `query`

Query the DAG for specific information.

```bash
git-dag-parser query --dag-file dag.bin --query "blob:abc123"
git-dag-parser query --dag-file dag.bin --query "path:src/main.rs"
```

## Performance Expectations

### Comparison with Git Process Spawning

| Metric | Git Processes | Direct Parsing |
|--------|--------------|----------------|
| Process spawns | O(commits) | 0 |
| I/O operations | Many | 1 mmap |
| Memory copies | Per process | Minimal |
| Lock contention | N/A | Zero |

### Expected Speedup

| Phase | Git Processes | Direct Parsing | Speedup |
|-------|---------------|----------------|---------|
| Object enumeration | ~1 process | 1 file scan | ~10x |
| Dependency building | ~10k processes | Pure compute | ~50-100x |
| Relationship building | Lock contention | Lock-free | ~2-5x |
| **Total** | Minutes | Seconds | **~20-50x** |

## Future Enhancements

1. **Incremental updates** - Only process new objects since last run
2. **Memory-mapped output** - For very large repositories
3. **Parallel pack parsing** - If multiple pack files exist
4. **Delta cache** - Cache resolved deltas for objects used as bases

## Dependencies

```toml
[dependencies]
clap = { version = "4", features = ["derive"] }
anyhow = "1"
thiserror = "1"
tracing = "0.1"
tracing-subscriber = "0.3"
indicatif = "0.17"
crossbeam-channel = "0.5"
rayon = "1.10"
memmap2 = "0.9"
flate2 = "1.0"
ahash = "0.8"
hashbrown = "0.14"
core_affinity = "0.8"
```
