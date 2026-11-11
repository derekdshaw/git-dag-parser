# Standalone fixes

## Summary

This plan has seven small fixes. Each fix is independent of the others. Each fix is one commit.

| Fix                            | Problem                                                                                   | When to do it                                              |
| ------------------------------ | ----------------------------------------------------------------------------------------- | ---------------------------------------------------------- |
| A. Linked worktrees            | The tool finds no objects in a linked worktree.                                           | At any time.                                               |
| B. Alternates                  | The tool does not read objects from other object stores. Edges to those objects are lost. | After step 4.                                              |
| C. SHA-256 repositories        | The tool reads a SHA-256 repository as SHA-1. The result is wrong and has no error.       | At any time.                                               |
| D. Empty repository            | Resolved by step 4.1 part 2. An empty repository no longer causes a panic. No test checks it. | At any time: only add the tests. |
| E. Documentation               | Mostly resolved. The `updated-docs` branch rewrites `ARCHITECTURE.md` and adds `README.md`. | After step 5: add "introduced by" and the rank.            |
| F. Progress without a terminal | A long run shows no progress when the output is not a terminal.                           | At any time. After step 11, mostly a change to the binary. |
| G. `init --help` text          | Resolved. The JSON removal (PR #1) removed `--output`. Step 7 adds it again with a default. | Nothing to do.                                             |

## Project and status

- Repository: this repository, a Cargo workspace (step 11). The binary crate `git-dag-parser` is at the root. The parser library `git-dag-parser-core` is in `crates/git-dag-parser-core`. The `Repo` test fixtures are in `crates/test-fixtures`.
- Purpose of the tool: find the cause of repository bloat. Trace large objects back to the commits and paths that added them.
- Completed steps at the time of this plan: 1 (tests), 2 (sizes and names), 3 (hashes, parents, tags, refs). Commit `8cdd22c`.
- Completed steps now (2026-10-30): also 4 (two-pass read, hash-ordered indices, object copies) and 4.1 parts 1 and 2 (name store, flat tree entries, `build_relationships` removed). Commit `eacf1b0` on main.
- Completed steps on 2026-11-09: also all of 4.1, 10 (CI and releases), and 11 (parser library crate).
- Other plans in this folder: steps 4.1 and 5 to 8. Some fixes here depend on them. See the table above.
- Important files:
  - `crates/git-dag-parser-core/src/repo.rs`: `find_git_dir` finds the git directory. Step 11 moved it to the library.
  - `crates/git-dag-parser-core/src/parser/mod.rs`: `parse_objects(git_dir)` reads `objects/pack`, loose objects, and refs.
  - `crates/git-dag-parser-core/src/parser/scan.rs`: `list_loose_objects` reads only `objects/xx/` directories with two hex digits, so it skips `objects/info` and `objects/pack`. `discover_packs` reads `objects/pack/`.
  - `crates/git-dag-parser-core/src/parser/refs.rs`: `read_refs(git_dir)` reads `packed-refs`, `refs/`, and `HEAD`.
  - `crates/test-fixtures/src/lib.rs`: the `Repo` fixture builder and git oracles.
  - `src/progress.rs` (binary): `ProgressLayer` draws the progress bar from the parser's `tracing` events.

## Rules for this work

- Do one fix in one commit.
- Write the test first. Make sure that it fails before you repair the code.
- Show the diff to the user before the commit. Do not commit until the user approves.
- Do not put a "Co-Authored-By" line or other tagline in the commit message.
- Put unit tests in the source file. Put tests that use real git repositories in the `tests/` folder of the crate that has the code: `tests/` at the root for the application, `crates/git-dag-parser-core/tests/` for the parser. Both use the `Repo` fixtures of `crates/test-fixtures`.
- At the end, tell the user what the fix does and which fixes remain.

---

## Fix A: Linked worktrees

### Problem

A linked worktree has a `.git` file, not a `.git` directory. The file contains `gitdir: <path>`. The path points to `<main>/.git/worktrees/<name>`.

That directory has only the data for one worktree: `HEAD`, `index`, and `logs/HEAD`. It has a `commondir` file. The `commondir` file gives the path to the main `.git` directory (usually `../..`). The objects, `refs/`, `packed-refs`, and `logs/refs/` are in the main directory.

At this time, `find_git_dir` returns the worktree directory. `parse_objects` then finds no objects.

Also, the `HEAD` of each linked worktree can keep commits alive. `git gc` uses these as roots. At this time, the tool reads only the `HEAD` of one directory.

### Work to do

1. (Done in step 11: `find_git_dir` is in `crates/git-dag-parser-core/src/repo.rs`.)
2. Return two paths:
   - `git_dir`: the directory for this worktree. `HEAD` is here.
   - `common_dir`: the directory that has `objects/`, `refs/`, and `packed-refs`. Read `git_dir/commondir` if it exists. The path in that file is relative to `git_dir`. If the file does not exist, `common_dir` is `git_dir`.
3. Change `parse_objects` to use `common_dir` for objects and refs, and `git_dir` for `HEAD`.
4. Read the `HEAD` of each linked worktree: `common_dir/worktrees/*/HEAD`. If it is detached, add it as a ref with the name `worktrees/<name>/HEAD` and kind `Head`.
5. If step 6 is complete, also read `common_dir/worktrees/*/logs/HEAD` as reflogs, and `common_dir/worktrees/*/index` as index roots. `git gc` keeps the objects of each worktree index.

### Tests

- Unit test in `crates/git-dag-parser-core/src/repo.rs`: a temporary directory with a `.git` file and a `commondir` file gives the correct `git_dir` and `common_dir`.
- Integration test: make a fixture, then run `git worktree add ../wt`. Run the parser on the `wt` directory. The object set equals `git cat-file --batch-all-objects` in the main repository.
- Integration test: in the worktree, run `git checkout --detach HEAD~1`. The refs include `worktrees/wt/HEAD` with the correct target.

### Prompt to start this fix

> In this repository, do fix A (linked worktrees) from docs/plans/09-standalone-fixes-plan.md. Write the tests first and show me that they fail. Then do the work, show me the diff, and do not commit until I approve.

---

## Fix B: Alternates

### Problem

A repository can use objects from other object stores. The file `objects/info/alternates` lists these stores, one path on each line. `git clone --shared`, `git clone --reference`, and some hosting servers make this file.

At this time, the parser does not read this file. A tree entry that points to an object in an alternate store gets `TreeEntries::NO_CHILD` as its child. The tool loses the edge, and the object is not in the graph.

The objects in an alternate store do not use disk space in this repository. Reports must not count them in the size of this repository, and `git gc` in this repository does not remove them.

### Records to change

This fix adds an `external` flag to the `Pack` and `ObjectCopy` records.

### Work to do

1. Read `objects/info/alternates` in `common_dir`. Ignore empty lines and lines that start with `#`.
2. A relative path is relative to the `objects` directory that has the `alternates` file.
3. Read the `alternates` file of each alternate store too. Stop at a depth of 5, the same as git. Do not read one store two times.
4. Read the packs and loose objects of each alternate store.
5. Add `external: bool` to `Pack` and to loose `ObjectCopy` records. Set it to `true` for copies from an alternate store.
6. Select a local copy as the primary copy before an external copy.
7. If step 8 is complete, do not count external copies in sizes or savings. Show the external total as a separate line.

### Tests

- Unit test: parse an `alternates` file with an absolute path, a relative path, a comment, and an empty line.
- Integration test: make a fixture. Run `git clone --shared <fixture> <clone>`. Make one new commit in the clone. Run the parser on the clone.
  - The object set equals `git cat-file --batch-all-objects` in the clone. This command includes alternate objects.
  - No tree entry in the clone has `TreeEntries::NO_CHILD` as its child, except submodule entries.
  - Only the copies of the new commit objects are local (`external = false`).

### Prompt to start this fix

> In this repository, do fix B (alternates) from docs/plans/09-standalone-fixes-plan.md. Write the tests first and show me that they fail. Then do the work, show me the diff, and do not commit until I approve.

---

## Fix C: SHA-256 repositories

### Problem

Git can make repositories that use SHA-256 hashes (`git init --object-format=sha256`). The hashes are 32 bytes. The tool uses 20-byte hashes everywhere (`pub type ObjectHash = [u8; 20]` in `crates/git-dag-parser-core/src/models/container.rs`).

At this time, the tool reads a SHA-256 repository with the wrong hash length. The result is wrong, and the tool does not show an error.

### Decision for this fix

Do not add SHA-256 support. Stop with a clear error. SHA-256 support is a large change. Do it only if a user needs it.

### Work to do

1. Read `common_dir/config` (or `git_dir/config` if fix A is not complete).
2. Find the `objectformat` key in the `[extensions]` section. Git config keys and section names are not case-sensitive.
3. If the value is `sha256`, return an error. Use this text: `SHA-256 repositories are not supported (extensions.objectFormat = sha256)`.
4. Do this check before the parser reads any object.

### Tests

- Unit test: a config text with `[extensions]` and `objectFormat = sha256` gives SHA-256. A config text without it gives SHA-1. Use mixed case in one test.
- Integration test: run `git init --object-format=sha256` and make one commit. `parse_objects` returns an error that contains `SHA-256`.

### Prompt to start this fix

> In this repository, do fix C (SHA-256 repositories) from docs/plans/09-standalone-fixes-plan.md. Write the tests first and show me that they fail. Then do the work, show me the diff, and do not commit until I approve.

---

## Fix D: Empty repository

### Problem

Before step 4.1 part 2, a repository with no commits caused a panic in `build_relationships` (`src/parser/relationships.rs`): the chunk size was 0, and `.chunks(0)` panics.

Step 4.1 part 2 (commit `eacf1b0`) removed `relationships.rs`. On 2026-10-30, `init` completed on an empty repository (0 objects) and on a repository with one blob and no commits. No test checks this.

### Work to do

1. Add the tests below.
2. Make sure that the step 5 code, if it exists, does not panic with zero commits, zero trees, or zero blobs.

### Tests

- Integration test: run `git init` with no commits. Run the full `init` command code on it. It completes, and all containers are empty.
- Integration test: run `git init`, then `git hash-object -w` on one file. There is one blob, no commits, and no error.

### Prompt to start this fix

> In this repository, do fix D (empty repository) from docs/plans/09-standalone-fixes-plan.md. Write the tests first and show me that they fail. Then do the work, show me the diff, and do not commit until I approve.

---

## Fix E: Documentation

### Problem

Before 2026-10-30, `ARCHITECTURE.md` described the old per-commit walk (`tree_deps`, `blob_deps`, "reachable from this commit", worker threads and channels). The tool does not record reachable objects for each commit. After step 5, it records the commit that **introduced** each object.

The `updated-docs` branch (commit `60816bd`) fixes most of this:

- It replaces `ARCHITECTURE.md` with a description of the current parser: the two passes, the primary copy, hash order, `TreeEntries`, and the `NameStore`. It removes the old walk, the relationships tables, the threading model, and the performance numbers that nobody measured.
- It adds `README.md`.
- Both files use ASD-STE100 Simplified Technical English.
- No doc comment in `src/` says "reachable" for a commit relation.

What remains: after step 5, describe "introduced by" and the commit rank (corrected commit date, then hash). After step 6, describe liveness. Steps 5 to 8 each update the two files (see the rules in each plan).

### Work to do

1. Do this fix only if a step did not update the documents. Write only about code that exists at that time.
2. Add a section about "introduced by", the commit rank, and liveness, for the steps that are complete.
3. Add the frontier method from step 5 to the description of threads, if step 5 is complete.
4. If step 7 is complete, describe the SQLite output and show the schema.
5. Keep only measured numbers, and give the repository and the date for each.
6. Write in ASD-STE100 Simplified Technical English.

### Tests

- There are no code tests for this fix.
- Make sure that each type name, field name, and command option in the document exists in the code. Use `rg` to find each name.

### Prompt to start this fix

> In this repository, do fix E (documentation) from docs/plans/09-standalone-fixes-plan.md. Show me the diff, and do not commit until I approve.

---

## Fix F: Progress without a terminal

### Problem

Before step 11, the parser drew `indicatif` progress bars. Now the parser sends the progress of pass 2 as `tracing` events (target `PROGRESS_TARGET`), and the binary draws the bar in `ProgressLayer` (`src/progress.rs`). Pass 1 sends no progress events. `indicatif` hides a progress bar when the output is not a terminal. In a run on a large repository with the output sent to a file, the last line was `Pack header: version 2, <count> objects declared`. For the next 15 minutes there was no output. The user cannot see if the run makes progress or has stopped.

### Work to do

1. In `ProgressLayer`, when the output is not a terminal, write one progress line at a fixed interval (for example each 10 seconds). Use `tracing` at `info` level.
   - If pass 1 also needs progress, send events from pass 1 in the library, the same as pass 2 does.
2. Each line shows the phase, the objects done, the total, the rate, and the `RssAnon` value.
3. When the output is a terminal, keep the progress bars.
4. Write the bars and the lines to stderr. Keep stdout for results.
5. At this time, the `tracing` log lines go to stdout, and `scripts/benchmarks/harness.py` reads the phase times from stdout. If the logs move to stderr, change `harness.py` to read stderr in the same commit.

### Tests

- Unit test: the function that makes the progress line gives the expected text for known values.
- Integration test (in the root `tests/`): run the binary on a fixture with stdout and stderr sent to pipes. Use a short interval. Make sure that stderr has at least one progress line.

### Prompt to start this fix

> In this repository, do fix F (progress without a terminal) from docs/plans/09-standalone-fixes-plan.md. Write the tests first and show me that they fail. Then do the work, show me the diff, and do not commit until I approve.

---

## Fix G: `init --help` text

Resolved. The JSON removal (PR #1) removed the `--output` option, so the help text no longer gives a wrong default. Step 7 adds `--output` again for the database. Step 7 makes its help text give the default, `<repo>/.git-dag.db`.
