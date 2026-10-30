# Step 8: Reports and the git gc savings estimate

## Summary

This step adds the reports that answer the main question of the tool: "Why is this repository large?"

- At this time, the `analyze` and `query` commands do nothing (`src/commands/analyze.rs`, `src/commands/query.rs`).
- After this step, `analyze` reads the SQLite database from step 7 and shows these reports:
  1. Directories with the most tree versions and the most bytes.
  2. Commits that added the most bytes.
  3. The largest blobs, with the commit and the path that added each one.
  4. Bytes for each liveness state (`Ref`, `IndexOnly`, `ReflogOnly`, `Unreachable`). `IndexOnly` shows files that a user staged but did not commit.
  5. Duplicate copies of objects.
  6. An estimate of the disk space that `git gc` can remove.
- The `query` command runs one SQL statement on the database and shows the result.

## Project and status before this step

- Repository: `/home/derekdshaw/r/git-dag-parser` (Rust, binary and library crate).
- Purpose of the tool: find the cause of repository bloat. Trace large objects back to the commits and paths that added them.
- Completed steps: 1 (tests), 2 (sizes and names), 3 (hashes, parents, tags, refs), 4 (two-pass read, hash-ordered indices, object copies), 4.1 (compact tree entries, name store, bounded delta cache, parallel parse), 5 (commit rank and owner), 6 (paths, reflogs, liveness), 7 (SQLite output).
- The database schema is in `src/store/` (step 7). Important tables: `trees`, `blobs`, `commits`, `paths`, `packs`, `object_copies`, `reflog_entries`, `refs`.

## Words used in this plan

- **Inflated size** (`size`): the size of the object when it is not compressed. This affects checkout and memory.
- **Disk size** (`size_disk`): the bytes that the object uses in the store. This affects clone and fetch.
- **Savings range**: a low value and a high value. The real savings after `git gc` are possibly between them.
- **Prune limit**: the age after which `git gc` removes unreachable objects. The default is 2 weeks (`gc.pruneExpire`).
- **Reflog limit**: the age after which `git reflog expire` removes reflog entries. The defaults are 90 days for reachable entries (`gc.reflogExpire`) and 30 days for unreachable entries (`gc.reflogExpireUnreachable`).

## Decisions already made

- Show duplicates as a separate report. Do not add them to other totals.
- Give the `git gc` savings as a range, not one number. Git does not tell which copy of a duplicate it keeps.
- Do not include keep packs (`packs.keep = 1`) in any savings. `git gc` does not repack them.
- Show both inflated size and disk size in each report.

## The savings estimate

Calculate each category separately. Show each category in the report.

| Category | What `git gc` does | Estimate |
|---|---|---|
| Object in two or more packs | Keeps one copy | Low: total of all copies minus the largest copy. High: total minus the smallest copy. |
| Loose copy of an object that is also in a pack | `git prune-packed` removes the loose copy | Exact: size of the loose file. |
| Reachable object with only a loose copy | Puts it in a pack, compressed and possibly as a delta | Unknown. Show the count and the loose bytes only. |
| Unreachable object | Removes it when it is older than the prune limit and no reflog entry keeps it | Total of copies older than the prune limit. Use the loose file mtime, the pack mtime, or the cruft pack `.mtimes` value. |
| Object that only reflog entries keep | Removed after `git reflog expire` and then `git gc` | Total of copies where all reflog entries that keep it are older than the reflog limit. |
| Object that only the index keeps (`IndexOnly`) | Not removed | None. Show the bytes separately, because staged large files are a possible cause of bloat. |

Read `gc.pruneExpire`, `gc.reflogExpire`, and `gc.reflogExpireUnreachable` from the repository config if they are set. Otherwise use the git defaults. Also let the user set the limits on the command line (for example `--prune-expire now`).

Tell the user these limits of the estimate:

- When git removes a copy, other deltas that use it as a base can change size.
- A new repack can find better deltas. This can make the result smaller than the estimate.

## Work to do

1. Make `src/reports/` with one function for each report. Each function takes a `rusqlite::Connection` and returns rows. Do the calculations in SQL where possible.
2. Change `analyze` to open the database and show all reports as text tables. Show sizes in human units (KB, MB, GB) and in bytes.
3. Add options to `analyze`: `--top N` (default 20), `--prune-expire <age>`, `--reflog-expire <age>`, `--reflog-expire-unreachable <age>`.
4. Change `query` to run one SQL statement from the command line and show the rows. Show binary hashes as hex.
5. Example SQL for report 1 (directories by tree versions):

   ```sql
   SELECT p.full_path, COUNT(*) AS versions, SUM(t.size) AS bytes, SUM(t.size_disk) AS disk_bytes
   FROM trees t JOIN paths p ON p.id = t.introduced_at_path_id
   GROUP BY p.id ORDER BY disk_bytes DESC LIMIT ?;
   ```

## Tests

Write the tests first. Make sure that they fail before you add the new code.

Unit tests (in the `src/reports/` files):

- Each report on a small hand-made database (`Connection::open_in_memory`) gives the expected rows.
- The savings range for one object with three copies of sizes 10, 20, and 30 is low 30 and high 50.
- Keep packs give no savings.

Integration tests (in `tests/`, with `Repo` fixtures):

- Make a fixture with known bloat: a directory with many files and many small changes, and one large blob.
- Report 1 shows that directory first. Report 3 shows the large blob, its commit, and its path.
- GC estimate test:
  1. Make a fixture with overlapping packs, loose copies of packed objects, and unreachable objects.
  2. Run `init` and `analyze --prune-expire now`. Record the savings range.
  3. Measure the store size with `git count-objects -v` (`size` plus `size-pack`, in KB).
  4. Run `git gc --prune=now`. Measure the store size again.
  5. The real savings must be in the estimated range, plus or minus a small tolerance for repack changes. Record the tolerance in the test.

## Make sure the result is correct on real repositories

- Run `init` and `analyze` on `engraph` in `/home/derekdshaw/r`. It has overlapping packs.
- Run `init` and `analyze` on the large test monorepo. Do not write to it. A manual calculation from its pack indexes found 232,401 objects in more than one pack (up to 6 copies), 4.11 GB of duplicate copies, and a savings range of 2.43 GB to 2.64 GB. The duplicate report must agree with these values.
- Copy `engraph` to the scratchpad. Do not copy the monorepo (65 GB). Run `git gc --prune=now` on the copy. Compare the real savings with the estimate. Do not run `git gc` on the original repositories.
- Show the reports to the user.

## Out of scope

- Do not add a user interface.
- Do not change the database schema unless a report needs a new column. If it does, tell the user first.

## Rules for this work

- Do one step in one commit.
- Show the diff to the user before the commit. Do not commit until the user approves.
- Do not put a "Co-Authored-By" line or other tagline in the commit message.
- Put unit tests in the source file. Put tests that use real git repositories in `tests/`.
- Update `README.md` and `ARCHITECTURE.md` to match the code after this step. Write them in ASD-STE100 Simplified Technical English, the same as the current files.
- At the end, tell the user what this step does and what the next work is.

## Prompt to start this step

> In /home/derekdshaw/r/git-dag-parser, do step 8 from docs/plans/08-reports-and-gc-estimate-plan.md. Write the tests first and show me that they fail. Then do the work, show me the diff, and do not commit until I approve.
