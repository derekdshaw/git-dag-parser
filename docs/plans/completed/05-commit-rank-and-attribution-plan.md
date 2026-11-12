# Step 5: Commit rank and parallel attribution

All items are done (2026-11-12). See "Result of this step".

## Summary

This step finds the commit that added each tree and each blob. This commit is the **owner** of the object.

- Until step 4.1, `build_relationships` (`src/parser/relationships.rs`) compared each commit with its first parent. Step 4.1 removed it, because nothing used its results. That method had these problems:
  - A merge commit gets the objects of the other branch too. The tool counts these objects two times.
  - A renamed or copied file looks like a new object.
  - An empty commit looks like it added its root tree.
  - A commit with a missing parent looks like it added its full tree.
- After this step, each object has exactly one owner. The owner is the first commit, in a fixed order, that can reach the object.
- The calculation runs in parallel. It does not walk each commit tree. It moves through the tree graph one level at a time.

## Project and status before this step

- Repository: this repository, a Cargo workspace (step 11). The binary crate `git-dag-parser` is at the root. The parser library `git-dag-parser-core` is in `crates/git-dag-parser-core`. The `Repo` test fixtures are in `crates/test-fixtures`.
- Purpose of the tool: find the cause of repository bloat. Trace large objects back to the commits and paths that added them.
- Completed steps: 1 (tests), 2 (sizes and names), 3 (hashes, parents, tags, refs), 4 (two-pass read, hash-ordered indices, object copies), 4.1 (compact tree entries, name store, bounded delta cache, parallel parse), 10 (CI and releases), 11 (parser library crate).
- Step 11 is done. The parser is the library crate `git-dag-parser-core`, and `src/attribution/` goes in the application crate at the root. The integration tests of this step are in the root `tests/`. They use the `Repo` fixtures of `crates/test-fixtures`, a dev-dependency of the application crate.
- The results of this step stay in the application crate. Do not add fields to `Commit`, `Tree`, or `Blob`: these types are in the library and hold only the parse output. `src/attribution/` returns a struct `Attribution` with arrays in container order (index `i` is commit, tree, or blob `i`).
- Data that this step uses, from `ParseResult` (`container` and `tree_entries`):
  - `Commit.tree` (`Option<usize>`, index of the root tree, `None` if the tree is not in the store), `Commit.parents` (`Vec<usize>`, only parents in the store), `Commit.boundary`, `Commit.commit_time` (`i64`), `Commit.hash`.
  - `TreeEntries` (step 4.1): private flat arrays `start` (`u64`), `child` (`u32`), `name` (`NameId`), and `mode` (`FileKind` as one byte). `entries(tree)` gives `(Option<usize>, NameId, FileKind)` for each entry of one tree. The `FileKind` tells if the child is a tree or a blob. The child is `None` (`TreeEntries::NO_CHILD`) for a submodule and for a child that is not in the store.
  - Indices are in hash order. Thus the order of commit indices is the hash order.
  - The model uses `usize` for indices. The arrays of this step use `u32` (see "Memory").
- Memory: a large monorepo can have approximately 3.9 million trees, 4.9 million blobs, and 1.2 billion tree entries (an estimate at 50 bytes of tree data for each entry; on linux the real count was 528 million, 34% more than this estimate gave, so plan for about 1.6 billion). Do not copy the entries. Use only `u32` and atomic `u32` arrays with one value for each object.
- Step 4.1 removed `build_relationships` (`src/parser/relationships.rs`, `WalkResult`, `Commit.tree_deps`, `Commit.blob_deps`, `Tree.commits`, `Blob.commits`, `Tree.paths`, `Blob.paths`). At this time, `init` runs only the parse. This step adds the rank and owner calculation after the parse.

## Words used in this plan

- **Corrected date**: a commit time that is always larger than the corrected date of each parent.
  - `corrected(c) = max(commit_time(c), max(corrected(p) for each parent p) + 1)`
  - Git commit-graph files use the same value.
- **Rank**: the position of a commit in the list sorted by (corrected date, hash). Rank 0 is first.
- **Owner**: for a tree or blob, the commit with the lowest rank that can reach the object.
- **Frontier**: the set of trees that the calculation processes in one round.
- **In-degree**: for a tree, the number of tree entries that point to it.

## Decisions already made

- Rank commits by (corrected date, hash). Do not rank by generation number. Because the commit indices are in hash order, sort by (corrected date, commit index).
- Committer time alone is not safe. Clock errors and rebases can give a child an earlier time than its parent. The `+ 1` in the corrected date stops this problem.
- An ancestor always has a lower rank than its descendants. Thus a merge never owns objects that a parent already has.
- The hash tie-break applies only to commits that are not ancestors of each other.
- The result must not change between runs or with a different number of threads.

## Why the parallel method is correct

The owner of object `o` is:

```
owner(o) = min( rank(c) for each commit c with root tree o,
                owner(T) for each tree T that contains o )
```

A commit can reach `o` only if `o` is its root tree, or if the commit can reach a tree that contains `o`. Thus the formula gives the same result as a walk of each commit in rank order with a "seen" set. The `min` operation gives the same result in any thread order.

## Work to do

1. Make a new module `src/attribution/` in the application crate. Add `mod.rs`, `rank.rs`, and `owner.rs`. Export it from the root `src/lib.rs`. `mod.rs` has the struct `Attribution` and the function that calculates it from `&ParseResult`.
2. In `rank.rs`, calculate the corrected date:
   1. Count the children of each commit.
   2. Start with the commits that have no parents in the store.
   3. Use a queue (Kahn's algorithm). Do not use recursion. Large histories have millions of commits.
   4. Ignore parents that are not in the store. `Commit.boundary` already marks these commits.
3. Sort the commits by (corrected date, hash). Give each commit its rank.
4. Add `corrected_date: Vec<i64>` and `rank: Vec<u32>` (one value for each commit) to `Attribution`.
5. Count the commits where the corrected date is not equal to the committer time. Show this number in the output. It shows how much clock errors affect the result.
6. In `owner.rs`, calculate the owner of each tree and blob. Use `rayon` parallel iterators on borrowed data:
   - The workers borrow `&TreeEntries`, the commit ranks, and the `AtomicU32` arrays for owner and in-degree. `rayon` returns only after all workers are done, so the data does not have to be `'static`.
   - Do not use `Arc`, `thread::spawn`, or a channel. The workers write their results with atomic operations. Nothing is sent back, and nothing is merged.
   - The values must be atomic because many trees contain the same child, and different workers process these trees at the same time. Also, exactly one worker must see the in-degree of a child become 0, so that the child goes into the next frontier one time. Do not give each worker its own arrays and merge them after each round: on a large monorepo, full arrays are approximately 50 MB for each thread, and they must be merged after each round. Also, a list of `(child, owner)` pairs is approximately 5 GB.
   - Use `Ordering::Relaxed` for all atomic operations. The `rayon` call of each round returns only after all its workers are done, so all writes of a round are visible to the next round.
   - This calculation does not use the `NameStore`. Do not remove it: step 6 uses it for paths, and step 7 writes the names.
   1. Make `owner: Vec<AtomicU32>` for trees and for blobs. Set each value to `u32::MAX`.
   2. For each commit with a root tree, set `owner[root]` to the minimum of its value and the commit rank (`fetch_min`).
   3. Calculate the in-degree of each tree from `TreeEntries`. Count each entry, also duplicates. Use `Vec<AtomicU32>` and a parallel loop over the trees.
   4. Put all trees with in-degree 0 into the first frontier. This includes trees that no commit reaches.
   5. For each round, process the frontier trees in parallel (`rayon`). For each entry of tree `T`:
      - Do `owner[child].fetch_min(owner[T])`. First read `owner[child]` with `load`, and do `fetch_min` only if `owner[T]` is smaller. On x86, `fetch_min` is a `cmpxchg` loop, and it takes the cache line from the other cores. A child that many trees contain usually gets its minimum from the first tree, so most entries then do only a read.
      - A tree with many entries (a large monorepo can have one with approximately 84,000) is a large unit of work. Process its entries in parallel chunks. `entries(tree)` is an iterator, so add a method to `TreeEntries` that gives the `child` and `mode` slices of one tree. `TreeEntries` is in the library (`crates/git-dag-parser-core/src/models/container.rs`), so this method is a change to the library.
      - Tests must use a small chunk size, so that a small tree is cut into chunks. Use two module-level constants side by side, `#[cfg(not(test))]` and `#[cfg(test)]`, as `GATHER_CHUNK` does (`crates/git-dag-parser-core/src/models/container.rs`). A constant inside `mod tests` is not visible to the parent module.
      - If the child is a tree, decrease its in-degree by 1. If the in-degree becomes 0, put the child in the next frontier.
   6. Stop when the frontier is empty.
   7. Ignore entries with no child (`None` from `entries`, `NO_CHILD` in the slice). This includes all submodule entries.
7. Add `tree_introduced_by: Vec<u32>` and `blob_introduced_by: Vec<u32>` (commit index) to `Attribution`. Convert the rank to the commit index. `u32::MAX` means that the object has no owner. Add methods `tree_introduced_by(i)` and `blob_introduced_by(i)` that return `Option<u32>`, with `None` for `u32::MAX`.
8. There is no old walk to remove. Step 4.1 removed it. Step 6 adds paths again in a new form.
9. Update `src/commands/init.rs` to call the rank and owner calculation after the parse.
   - Show the number of commits from item 5 in the `=== DAG Summary ===` section.
   - Show the time as `Attribution: <time>` in the `=== Timing ===` section, after `Parse:` and before `Total:`. Use the `{:?}` format of `Duration`, as the other lines do. Step 4.1 renamed `Phase 1 (parse):` to `Parse:` and removed `Phase 2 (relationships)`.
   - `scripts/benchmarks/harness.py` reads the info log lines and the summary from stdout, and takes the last line that starts with `Total:`. Keep `Total:` as the last line.
   - In `harness.py`, add `attribution_s` (`^Attribution:\s+(\S+)$`). Change the window `resolve_refs` to end at `parse_s`, and add the window `attribution` from `parse_s` to `parse_s + attribution_s`.

## Tests

Write the tests first. Make sure that they fail before you add the new code.

Unit tests (in `rank.rs` and `owner.rs`):

- Corrected date with a clock error: P has time 1000. Its child C has time 900. C gets 1001. The child G of C has time 950. G gets 1002.
- Rank tie-break: two commits with the same corrected date sort by hash.
- A commit with a missing parent uses its own time.
- Owner on a small hand-made graph. Make the `TreeEntries` with `TreeEntriesBuilder` and `TreeEntries::merge`, as `merge_one` does in the tests of `crates/git-dag-parser-core/src/models/container.rs`. Both are public.
  - Two commits share a subtree. The lower rank owns it.
  - One blob is at two paths in one tree. It has one owner.
  - A tree that no commit reaches has no owner.
- The same graph gives the same result with 1, 2, and 8 threads. Use `rayon::ThreadPoolBuilder`.

Integration tests (in the root `tests/`, with new `Repo` fixtures in `crates/test-fixtures`):

- Make a fixture with these events:
  - a rename of a file,
  - an empty commit (`git commit --allow-empty`),
  - a merge (`--no-ff`),
  - the same new file on two branches,
  - a revert that brings back an old blob.
- Oracle 1: for each commit `c` in rank order, run `git rev-list --objects --no-object-names c --not <all commits with lower rank>`. The output is the set of objects that `c` owns. Compare with `introduced_by`.
- Oracle 2: write a simple sequential walk in the test helper. Walk commits in rank order. Keep a "seen" set. The first commit that reaches an object owns it. Compare with the parallel result.
- The rename, the empty commit, and the revert own no old blobs.
- A tree or blob that only an annotated tag reaches (for example, a tag of a blob) has no owner. Oracle 1 agrees, because it lists only objects that commits reach.
- A repository with no commits gives no owners and does not panic (see plan 09, "Fix D: Empty repository").

## Make sure the result is correct on real repositories

- Use a small program in the session scratchpad. Run oracle 2 on this repository and on the repositories in `docs/benchmarks/README.md`.
- Run the harness on vscode and git-ms. Compare the time of the rank and owner calculation with the old `build_relationships` (harness, commit `b50d421`): vscode 8.0 s, git-ms 10.3 s. For scale, the parse after step 4.1 is 1.3–1.5 s on vscode and 2.8–2.9 s on git-ms (32 threads).
- Run the harness on linux (see `docs/benchmarks/README.md`) with `--limit-gb 48`. Do not write to that repository. Record the number of frontier rounds, the time, the peak `RssAnon` of the `attribution` window, and the number of commits where the corrected date is not equal to the committer time. After step 4.1 the peak of the linux parse is 8.94–9.10 GB on 32 threads; each array of this step has 4 bytes for each tree, blob, or commit (8 bytes for the corrected date) (10.5 million objects on linux in all).
- `harness.py` gives `init` only `RUST_LOG` and `PATH`, so `RAYON_NUM_THREADS` does not change the thread count of its runs. It samples `RssAnon` every 0.2 s, which is too slow for a run of less than about 1 s, such as zed.

## Result of this step

Done on 2026-11-12, on the branch `plan-05`.

- `src/attribution/` (`mod.rs`, `rank.rs`, `owner.rs`) is in the application crate. `Attribution::calculate(&ParseResult)` gives `corrected_date`, `rank`, `tree_introduced_by`, `blob_introduced_by`, `corrected_commits`, and `rounds`.
- Library changes: `TreeEntries::children(tree)` gives the `child` and `mode` slices of one tree. `NameMap::identity` is public, so that the tests of the application can make `TreeEntries` with `TreeEntries::merge`.
- Oracle 1 is different from the text above. `git rev-list --objects c --not <lower>` takes away only the objects of the lower commits that are parents of the commits it walks. It does not take away the objects of a commit on a sibling branch. Thus the test gets the objects of each commit with `git rev-list --objects c`, and takes away the objects of all commits with a lower rank.
- Oracle 2 (a walk on one thread, with a "seen" set) gives the same result on this repository, lage, zed, vscode, git-ms, linux, and a large monorepo (approximately 4.1 million trees, 4.8 million blobs, and 663 million tree entries). The result is the same on 1, 2, 8, 16, and 32 threads.
- Harness on 32 threads (2026-11-12): vscode 0.18 s (12 rounds), git-ms 0.15 s (8 rounds), linux 1.60 s (14 rounds), the large monorepo 0.83 s (17 rounds). Before step 4.1, `build_relationships` used 8.0 s on vscode and 10.3 s on git-ms.
- Linux: the peak `RssAnon` of the run is 9.03 GB (in the parse), and 6.50 GB in the `attribution` window. 593,467 of 1,309,717 commits have a corrected date that is not their committer time. 466,462 of them have a parent with the same committer time, to the second. Only 807 have a parent with a later time.
- On linux, the corrected dates use 0.6 s of the 1.6 s. The commit indices are in hash order, so each step of Kahn's algorithm goes to a different part of memory.

## Out of scope

- Do not calculate paths (`introduced_at_path`). Step 6 does that.
- Do not calculate reachability from refs or reflogs. Step 6 does that.
- Do not change the output file format. Step 7 does that.

## Rules for this work

- Do one step in one commit.
- Show the diff to the user before the commit. Do not commit until the user approves.
- Do not put a "Co-Authored-By" line or other tagline in the commit message.
- Put unit tests in the source file. Put tests that use real git repositories in the `tests/` folder of the crate that has the code: `tests/` at the root for the application, `crates/git-dag-parser-core/tests/` for the parser. Both use the `Repo` fixtures of `crates/test-fixtures`.
- Update `README.md` and `ARCHITECTURE.md` to match the code after this step. Write them in ASD-STE100 Simplified Technical English, the same as the current files.
- At the end, tell the user what this step does and what the next step is.

## Prompt to start this step

> In this repository, do step 5 from docs/plans/05-commit-rank-and-attribution-plan.md. Write the tests first and show me that they fail. Then do the work, show me the diff, and do not commit until I approve.
