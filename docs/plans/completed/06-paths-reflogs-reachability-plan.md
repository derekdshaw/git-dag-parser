# Step 6: Paths, reflogs, and reachability

## Summary

This step adds four things:

1. **Paths.** Each tree and blob gets the path where its owner commit added it. A path table stores each path one time.
2. **Reflogs.** The parser reads the reflog files in `.git/logs/`. A reflog entry can keep old commits alive after no ref points to them.
3. **The index.** The parser reads `.git/index`. `git gc` does not remove blobs and trees that the index uses, also files that a user staged but did not commit.
4. **Reachability.** Each object gets one of four states:
   - reachable from a ref,
   - kept only by the index,
   - reachable only from a reflog entry,
   - not reachable.

With paths, the tool can answer "which directory made the most tree versions". With reachability, the tool can show objects that `git reflog expire` and `git gc` can remove.

## Project and status before this step

- Repository: this repository, a Cargo workspace (step 11). The binary crate `git-dag-parser` is at the root. The parser library `git-dag-parser-core` is in `crates/git-dag-parser-core`. The `Repo` test fixtures are in `crates/test-fixtures`.
- The parser work of this step (reflogs and the index) goes in the library crate. The paths and the reachability go in the application crate. Their results stay in the application, next to `Attribution` (step 5). Do not add fields for them to the library types `Commit`, `Tree`, `Blob`, or `Tag`, or to the parse result struct.
- Purpose of the tool: find the cause of repository bloat. Trace large objects back to the commits and paths that added them.
- Completed steps: 1 (tests), 2 (sizes and names), 3 (hashes, parents, tags, refs), 4 (two-pass read, hash-ordered indices, object copies), 4.1 (compact tree entries, name store, bounded delta cache, parallel parse), 5 (commit rank and owner), 10 (CI and releases), 11 (parser library crate).
- Data that this step uses:
  - `Attribution` (step 5, `src/attribution/`): the owner commit of each tree and blob (`tree_introduced_by`, `blob_introduced_by`), and the commit `rank`.
  - `TreeEntries` (step 4.1) with `child`, `name` (`NameId`), and `mode`. `NameStore` (step 4.1) gives the name bytes.
  - Both are fields of the parse result struct (step 4.1, part 8), not of `ObjectContainer`.
  - `ObjectContainer.refs` with `target: Option<ObjectId>` (step 3). `Tag.target` (step 3).
  - The parallel frontier code in `src/attribution/owner.rs` (step 5).
- The JSON output (`init --output`) and `serde` were removed after step 4.1, part 1. The container is not serialized.

## Words used in this plan

- **Name**: one entry name in one tree, for example `main.rs` or `parser`. Git keeps only names. The `NameStore` (step 4.1) keeps each distinct name one time.
- **Path**: the names from the root of a commit tree to a file or directory, for example `src/parser/main.rs`. Git does not store paths. You get a path when you go down from a root tree. The root tree has the empty path.
- **Introducing parent**: for an object `o` with owner commit `c`, a tree `T` that contains `o` and also has owner `c`.
- **Reflog entry**: one line in a file under `.git/logs/`. The format is `<old hash> <new hash> <name> <<email>> <time> <tz>\t<message>`.
- **Index**: the file `.git/index`. It lists the staged files with their blob hashes. Its `TREE` extension (cache-tree) lists tree hashes for directories.
- **Liveness**: the reachability state of an object. The values are `Ref`, `IndexOnly`, `ReflogOnly`, and `Unreachable`.

## Decisions already made

- A path is its parent path plus one name. The path table is a prefix tree on the `NameStore`: each path has a `parent`, a `name` (`NameId`), and a `depth`. Its id is its position in the table. `src/parser/main.rs` is the path with the name `main.rs` and the parent `src/parser`.
- Do not keep the full path bytes in memory. Do not make a second `NameStore` for paths. Make the full bytes from the parent chain only when you need them: for the sort in item 4, and for the database write in step 7.
- Give path ids in the sort order of the full path bytes. Then the ids do not change between runs.
- `ObjectContainer` does not hold a `NameId`. The paths are a result of the application, not of the parse. Keep them in a struct of the application crate, for example `Paths { paths: Vec<Path>, tree_path: Vec<u32>, blob_path: Vec<u32> }`. For each tree and blob, keep only the path id.
- An object gets its path from its introducing parent. If there are many introducing parents, use the lowest (tree index, entry index). Tree indices are in hash order, so the result does not change between runs.
- A tree edge stores the entry name, not a path id. One tree can be at many paths, so a path is a fact about the object owner, not about the edge.
- Reflogs are not an object type. Reflog entries are roots for reachability, the same as refs.
- The stash reflog (`logs/refs/stash`) holds older stash entries. Read it like other reflogs.
- The index is a root, the same as refs and reflogs. A large monorepo can have an index with 154,900 entries and a `TREE` extension.
- Liveness order: `Unreachable < ReflogOnly < IndexOnly < Ref`. A reflog entry expires. An index entry stays until the user changes the index.

## Why the introducing parent always exists

Let object `o` have owner `c`, and let `o` not be the root tree of `c`. Commit `c` reaches `o` through a tree `T`. If `T` had an owner with a lower rank than `c`, then that commit also reaches `o`. Then `o` would have that lower owner, not `c`. Thus `T` has owner `c`.

## Work to do

### Paths

1. Add a `Path` type in the application crate: `parent: Option<u32>`, `name: NameId`, `depth: u32`. The id of a path is its position in `paths`. Do not keep a `Vec<u8>` for each path. Keep `paths: Vec<Path>` in the paths struct, not in the parse result struct or in `ObjectContainer`.
2. Find the introducing parent of each tree and blob:
   1. Use `AtomicU64` for each object. Pack (tree index, entry index) into the value. Set each value to `u64::MAX`.
   2. In parallel, for each tree `T` and each entry: if `owner(T) == owner(child)`, do `fetch_min` with (T, entry index).
   3. If an object is the root tree of its owner commit, it has the empty path and no introducing parent.
3. Calculate the path of each object from the path of its introducing parent plus the entry name. Process trees from the root down. Use the frontier order from step 5, or keep a cache.
4. Make the path table. Sort the paths by their full path bytes. Make these bytes from the parent chain for the sort, then drop them. Give the ids in that order, and change each `parent` to the new id.
5. Keep the path id of each tree and blob (`introduced_at_path`) in two `u32` arrays of the paths struct. `u32::MAX` means no path. Add methods that return `Option<u32>`.

### Reflogs

6. Make `crates/git-dag-parser-core/src/parser/reflogs.rs`. Read `logs/HEAD` and all files under `logs/refs/`. Skip files that end in `.lock`.
7. Add a `ReflogEntry` model in `crates/git-dag-parser-core/src/models/`: `ref_name`, `old_hash`, `new_hash`, `old_target`, `new_target` (`Option<ObjectId>`), `time`, `message` (bytes).
8. An all-zero hash means "no object". Resolve it to `None`.
9. Add `reflog_entries: Vec<ReflogEntry>` to `ObjectContainer`.

### Index

10. Make `crates/git-dag-parser-core/src/parser/index.rs`. Read `git_dir/index`. Support index versions 2, 3, and 4. Version 4 compresses the path of each entry against the previous entry.
11. From each entry, get the blob hash (or the tree hash for a sparse directory entry, mode `040000`).
12. Read the `TREE` extension. Get the hash of each valid cache-tree node.
13. If the index has a `link` extension (split index), also read `git_dir/sharedindex.<hash>`.
14. Add `index_roots: Vec<ObjectId>` to `ObjectContainer`. Ignore hashes that are not in the store, for example submodule commits.

### Reachability

15. Add a `Liveness` enum in the application crate with the order `Unreachable < ReflogOnly < IndexOnly < Ref`. Keep the liveness of each commit, tree, blob, and tag in one array for each type (one byte for each object), in a struct of the application.
16. Mark the roots:
    - Each ref target gets `Ref`.
    - Each index root gets `IndexOnly`.
    - Each reflog `old_target` and `new_target` gets `ReflogOnly`.
    - Follow tag targets. A tag gives its liveness to its target.
17. Give liveness to commits. Walk parent edges from the roots with a queue. A commit gets the maximum liveness of any root or child that reaches it.
18. Give liveness to trees and blobs. Use the same frontier rounds as in step 5. Use `fetch_max` instead of `fetch_min`. Seed each root tree with the liveness of its commits.
   Also seed each index root tree and blob with `IndexOnly`.
19. Show the totals in the `init` output: count and `size_disk` for each liveness state.

## Tests

Write the tests first. Make sure that they fail before you add the new code.

Unit tests (in the source files):

- Path table: paths sort by bytes, ids follow that order, and parent ids are correct.
- Introducing parent: a blob at two paths in one commit gets the lower (tree index, entry index).
- Reflog line: parse the hashes, the time, and a non-UTF-8 message. An all-zero old hash gives `None`.
- Index: parse a small index of version 2 and of version 4, with a `TREE` extension. Get the correct blob and tree hashes.
- Liveness: on a small hand-made graph, `Ref` is stronger than `IndexOnly`, `IndexOnly` is stronger than `ReflogOnly`, and an object with no root is `Unreachable`.

Integration tests (with `Repo` fixtures from `crates/test-fixtures`; the paths and reachability tests in the root `tests/`, the index and reflog tests in `crates/git-dag-parser-core/tests/`):

- Paths: for each commit `c` in rank order, run `git rev-list --objects c --not <commits with lower rank>`. This prints a path for each object. Compare it with `introduced_at_path`. Skip objects that are at two paths in one commit, because git can select a different path.
- Reachability fixture:
  - `git commit --amend` (the old commit is reflog only),
  - `git reset --hard HEAD~1` (the removed commit is reflog only),
  - `git stash` two times, then `git stash drop` (the dropped stash is not reachable),
  - `git hash-object -w` on a new file (a blob that is not reachable),
  - `git add` on a new file, then `git write-tree`, with no commit (the blob and the new trees are index only).
- Reachability oracle:
  - `git rev-list --objects --all` gives the `Ref` set.
  - `git rev-list --objects --all --indexed-objects` gives `Ref` plus `IndexOnly`.
  - `git rev-list --objects --all --indexed-objects --reflog` gives `Ref`, `IndexOnly`, and `ReflogOnly`.
  - All other objects are `Unreachable`.
- Index oracle: `git ls-files --stage` gives each entry hash. Run the test with `git update-index --index-version 4` too.
- Reflog oracle: `git reflog show --all --format='%gd %H'` gives each entry.

## Make sure the result is correct on real repositories

- Use a small program in the session scratchpad. Run the reachability oracle on this repository and on the repositories in `docs/benchmarks/README.md`.
- Report the bytes in each liveness state for each repository.

## Result of this step

Done on 2026-11-12, on the branch `plan-06`.

- Library: `src/parser/reflogs.rs` and `src/parser/index.rs`, `ReflogEntry` in `src/models/objects.rs`, and `reflog_entries` and `index_roots` in `ObjectContainer`. `TreeEntries::names(tree)` gives the `name` slice of one tree. `NameId` has `Ord` (the order of the ids), for the sort of the path keys.
- Program: `src/paths.rs` (`Paths`, `Path`, `NO_PATH`) and `src/reachability.rs` (`Liveness`, `Reachability`, `Total`). `init` shows the number of distinct paths, the roots, the count and `size_disk` of each liveness state, and the `Paths:` and `Reachability:` times. `harness.py` reads the two times and adds the windows `paths` and `reachability`.
- Decisions made with the user during this step:
  - Row 0 of the path table is the empty path: `Path.name` is `Option<NameId>`, and only row 0 has `None`. A top-level path has the parent `Some(0)`.
  - A split index is read exactly: the parser decodes the EWAH delete and replace bitmaps. On an index of 155,000 entries, `read_index` uses 8–16 ms, with or without a split index, so the decode is not a slow part.
  - The resolve-undo (`REUC`) blobs are index roots, as in `git rev-list --indexed-objects` and `git gc`.
- These parts are different from the text above:
  - The paths oracle uses `git ls-tree -r -t` of each owner commit, not `git rev-list --objects c --not <lower>`, for the reason that step 5 found. An object at two paths in its owner commit is skipped.
  - Item 3: the paths are made in sequence of depth. `tree_depths` finds the depth of each tree from the introducing parents. Then one sort for each depth finds the objects with the same (parent path, name).
  - Item 17: the commits get their liveness from one scan in falling rank order, on an array of the parents in rank order. The rank is a topological order. Thus `Reachability::calculate` takes `&Attribution`. A walk in commit index order used approximately 200 ms on linux; the scan uses much less.
  - Item 18: the trees do not use the frontier rounds of step 5. The walk of the strongest state goes first, and a weaker walk stops at objects that already have a state. Each tree is walked at most one time, and no in-degree pass over all tree entries is necessary.
- The reachability oracle (`git rev-list --objects --all`, `--indexed-objects`, `--reflog`) gives the same liveness for each object on this repository, lage, zed, git-ms, vscode, linux, and a large monorepo. A sample of up to 100 commits of each repository gives the same paths as `git ls-tree`.
- The path table, the path ids, and the liveness of each object are the same on 1, 2, 8, and 32 threads (a digest of all of them, on vscode, git-ms, and this repository).
- Bytes in each liveness state (`size_disk`):

| Repository | Ref | IndexOnly | ReflogOnly | Unreachable |
|---|---|---|---|---|
| this repository | 793 objects, 2.17 MB | 0 | 16 objects, 85.04 KB | 122 objects, 491.57 KB |
| lage | 9,163 objects, 14.05 MB | 0 | 0 | 0 |
| zed | 286,276 objects, 127.95 MB | 0 | 0 | 0 |
| git-ms | 781,136 objects, 377.88 MB | 0 | 0 | 11,791 objects, 2.24 MB |
| vscode | 1,654,196 objects, 933.81 MB | 0 | 117 objects, 29.06 KB | 0 |
| linux | 10,456,133 objects, 5.04 GB | 0 | 0 | 0 |
| A large monorepo | 9,193,215 objects, 60.09 GB | 0 | 12,174 objects, 42.10 MB | 2,012 objects, 4.66 MB |

  The work trees of these clones have no staged changes, so no object is `IndexOnly`.
- Harness on 32 threads (2026-11-12): Paths 0.24 s on vscode, 0.10 s on git-ms, 0.82 s on linux, 0.93 s on the large monorepo; Reachability 0.16 s, 0.11 s, 0.50 s, 0.46 s. The peak heap of `init` did not change (pass 2 has it).

## Out of scope

- Do not write the database. Step 7 does that.
- Do not calculate reflog age limits or `git gc` savings. Step 8 does that.
- Do not read reflogs or indexes of linked worktrees (`worktrees/*/logs`, `worktrees/*/index`). Fix A in the standalone fixes plan adds them.

## Rules for this work

- Do one step in one commit.
- Show the diff to the user before the commit. Do not commit until the user approves.
- Do not put a "Co-Authored-By" line or other tagline in the commit message.
- Put unit tests in the source file. Put tests that use real git repositories in the `tests/` folder of the crate that has the code: `tests/` at the root for the application, `crates/git-dag-parser-core/tests/` for the parser. Both use the `Repo` fixtures of `crates/test-fixtures`.
- Update `README.md` and `ARCHITECTURE.md` to match the code after this step. Write them in ASD-STE100 Simplified Technical English, the same as the current files.
- At the end, tell the user what this step does and what the next step is.

## Prompt to start this step

> In this repository, do step 6 from docs/plans/06-paths-reflogs-reachability-plan.md. Write the tests first and show me that they fail. Then do the work, show me the diff, and do not commit until I approve.
