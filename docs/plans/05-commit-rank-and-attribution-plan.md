# Step 5: Commit rank and parallel attribution

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

- Repository: `/home/derekdshaw/r/git-dag-parser` (Rust, binary and library crate).
- Purpose of the tool: find the cause of repository bloat. Trace large objects back to the commits and paths that added them.
- Completed steps: 1 (tests), 2 (sizes and names), 3 (hashes, parents, tags, refs), 4 (two-pass read, hash-ordered indices, object copies), 4.1 (compact tree entries, name store, bounded delta cache, parallel parse).
- Data that this step uses:
  - `Commit.tree` (index of the root tree), `Commit.parents`, `Commit.boundary`, `Commit.commit_time`, `Commit.hash`.
  - `TreeEntries` (step 4.1): flat arrays `start`, `child` (`u32`), `name` (`NameId`), and `mode`. Use `entries(tree)` to read the entries of one tree.
  - Indices are in hash order (step 4).
- Memory: the large test monorepo has approximately 3.9 million trees, 4.9 million blobs, and 1.2 billion tree entries (an estimate at 50 bytes of tree data for each entry; on linux the real count was 528 million, 34% more than this estimate gave, so plan for about 1.6 billion). Do not copy the entries. Use only `u32` and atomic `u32` arrays with one value for each object.
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

- Rank commits by (corrected date, hash). Do not rank by generation number.
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

1. Make a new module `src/attribution/`. Add `mod.rs`, `rank.rs`, and `owner.rs`. Export it from `src/lib.rs`.
2. In `rank.rs`, calculate the corrected date:
   1. Count the children of each commit.
   2. Start with the commits that have no parents in the store.
   3. Use a queue (Kahn's algorithm). Do not use recursion. Large histories have millions of commits.
   4. Ignore parents that are not in the store. `Commit.boundary` already marks these commits.
3. Sort the commits by (corrected date, hash). Give each commit its rank.
4. Add `corrected_date: i64` and `rank: u32` to `Commit`.
5. Count the commits where the corrected date is not equal to the committer time. Show this number in the output. It shows how much clock errors affect the result.
6. In `owner.rs`, calculate the owner of each tree and blob. Use `rayon` parallel iterators on borrowed data:
   - The workers borrow `&TreeEntries`, the commit ranks, and the `AtomicU32` arrays for owner and in-degree. `rayon` returns only after all workers are done, so the data does not have to be `'static`.
   - Do not use `Arc`, `thread::spawn`, or a channel. The workers write their results with atomic operations. Nothing is sent back, and nothing is merged.
   - This calculation does not use the `NameStore`. Do not remove it: step 6 uses it for paths, and step 7 writes the names.
   1. Make `owner: Vec<AtomicU32>` for trees and for blobs. Set each value to `u32::MAX`.
   2. For each commit with a root tree, set `owner[root]` to the minimum of its value and the commit rank (`fetch_min`).
   3. Calculate the in-degree of each tree from `TreeEntries`. Count each entry, also duplicates. Use `Vec<AtomicU32>` and a parallel loop over the trees.
   4. Put all trees with in-degree 0 into the first frontier. This includes trees that no commit reaches.
   5. For each round, process the frontier trees in parallel (`rayon`). For each entry of tree `T`:
      - Do `owner[child].fetch_min(owner[T])`.
      - A tree with many entries (the test monorepo has one with approximately 84,000) is a large unit of work. Process its entries in parallel chunks.
      - If the child is a tree, decrease its in-degree by 1. If the in-degree becomes 0, put the child in the next frontier.
   6. Stop when the frontier is empty.
   7. Ignore submodule entries and entries with `child == u32::MAX`.
7. Add `introduced_by: Option<u32>` (commit index) to `Tree` and `Blob`. Convert the rank to the commit index. `u32::MAX` gives `None`.
8. There is no old walk to remove. Step 4.1 removed it. Step 6 adds paths again in a new form.
9. Update `src/commands/init.rs` to call the rank and owner calculation after the parse. Show its time as `Phase 2 (attribution): <time>`, and read this line in `scripts/benchmarks/harness.py` (step 4.1 removed the `Phase 2 (relationships)` line that timed `build_relationships`).

## Tests

Write the tests first. Make sure that they fail before you add the new code.

Unit tests (in `rank.rs` and `owner.rs`):

- Corrected date with a clock error: P has time 1000. Its child C has time 900. C gets 1001. The child G of C has time 950. G gets 1002.
- Rank tie-break: two commits with the same corrected date sort by hash.
- A commit with a missing parent uses its own time.
- Owner on a small hand-made graph:
  - Two commits share a subtree. The lower rank owns it.
  - One blob is at two paths in one tree. It has one owner.
  - A tree that no commit reaches has no owner.
- The same graph gives the same result with 1, 2, and 8 threads. Use `rayon::ThreadPoolBuilder`.

Integration tests (in `tests/`, with new `Repo` fixtures):

- Make a fixture with these events:
  - a rename of a file,
  - an empty commit (`git commit --allow-empty`),
  - a merge (`--no-ff`),
  - the same new file on two branches,
  - a revert that brings back an old blob.
- Oracle 1: for each commit `c` in rank order, run `git rev-list --objects --no-object-names c --not <all commits with lower rank>`. The output is the set of objects that `c` owns. Compare with `introduced_by`.
- Oracle 2: write a simple sequential walk in the test helper. Walk commits in rank order. Keep a "seen" set. The first commit that reaches an object owns it. Compare with the parallel result.
- The rename, the empty commit, and the revert own no old blobs.

## Make sure the result is correct on real repositories

- Use a small program in the session scratchpad. Run oracle 2 on `engraph`, `agent-work-visualizer`, `mnemosyne`, `home-auction`, and `git-dag-parser` in `/home/derekdshaw/r`.
- Run the harness on vscode and git-ms. Compare the time of the rank and owner calculation with the old `build_relationships` (harness, commit `b50d421`): vscode 8.0 s, git-ms 10.3 s.
- Run the new phase on the large test monorepo with a watchdog on `RssAnon` (see step 4.1). Do not write to that repository. Record the number of frontier rounds, the time, the peak `RssAnon`, and the number of commits where the corrected date is not equal to the committer time.

## Out of scope

- Do not calculate paths (`introduced_at_path`). Step 6 does that.
- Do not calculate reachability from refs or reflogs. Step 6 does that.
- Do not change the output file format. Step 7 does that.

## Rules for this work

- Do one step in one commit.
- Show the diff to the user before the commit. Do not commit until the user approves.
- Do not put a "Co-Authored-By" line or other tagline in the commit message.
- Put unit tests in the source file. Put tests that use real git repositories in `tests/`.
- Update `README.md` and `ARCHITECTURE.md` to match the code after this step. Write them in ASD-STE100 Simplified Technical English, the same as the current files.
- At the end, tell the user what this step does and what the next step is.

## Prompt to start this step

> In /home/derekdshaw/r/git-dag-parser, do step 5 from docs/plans/05-commit-rank-and-attribution-plan.md. Write the tests first and show me that they fail. Then do the work, show me the diff, and do not commit until I approve.
