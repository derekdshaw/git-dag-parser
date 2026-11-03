# Step 11: Move the DAG parsing into a separate library

## Summary

This step moves the code that parses the Git object store into its own library crate. The binary crate keeps the CLI and the commands.

- At this time, almost all the code is parsing. There is no relationship graph code: step 4.1 part 2 deleted `relationships.rs`. Steps 5 to 8 add the graph code again.
- The parser already builds the DAG. It resolves commit trees, commit parents, tag targets, and tree entry children to indices. The output of the library is this resolved object graph: the nodes and the edges.
- The calculations on the graph (rank, owner, paths, reachability, SQLite, reports) stay in the application.
- Do this step after step 4.1 part 3b and before step 5. Then `src/attribution/` (step 5) starts on the correct side of the boundary.

## Project and status before this step

Lines per file (2026-11-01):

| Area | Files | Lines | Side |
|---|---|---|---|
| Pass 1: finds packs and loose objects, sizes, all on-disk copies, primary copy | `src/parser/scan.rs` | 822 | parsing |
| Pack index and pack read, delta decode, `DeltaGraph` | `src/parser/pack.rs` | 1188 | parsing |
| Pass 2 and its delta cache | `src/parser/bodies.rs`, `src/parser/delta_cache.rs` | 459 | parsing |
| Commit, tree, and tag body parsers | `src/parser/content.rs` | 454 | parsing |
| Loose objects, refs | `src/parser/loose.rs`, `src/parser/refs.rs` | 391 | parsing |
| Entry point, hash to index resolution | `src/parser/mod.rs` | 127 | parsing |
| Output model: `ObjectContainer`, `HashIndex`, `TreeEntries`, `NameStore`, object structs | `src/models/*` | 1334 | parsing (output types) |
| `ParseError` | `src/error.rs` | about 40 | parsing |
| CLI, `init`, `analyze`, `query`, `DagError` | `src/cli.rs`, `src/main.rs`, `src/commands/*`, `src/error.rs` | about 240 | application |

The parsing side is about 4,800 of 5,050 lines.

The parser builds these edges:

- `resolve_commits` (`src/parser/mod.rs:82`): the root tree and the parents of each commit.
- `resolve_tags` (`src/parser/mod.rs:101`): the target of each annotated tag.
- Pass 2: the child of each tree entry, in `TreeEntries`.

### Work in later plans, by side

| Plan | Work | Side |
|---|---|---|
| 05 | Rank and owner, `src/attribution/` | application |
| 06 | Path table on `TreeEntries` and `NameStore`, reachability states | application |
| 06 | `src/parser/reflogs.rs`, `src/parser/index.rs` | library |
| 07 | SQLite output | application |
| 08 | Reports | application |
| 09 A | `src/repo.rs` with `find_git_dir` | library |
| 09 B | Alternates | library |
| 09 C | SHA-256 repositories | library |

Plans 05 to 09 refer to paths such as `src/parser/...`. This step must update these paths.

## Coupling to repair before the split

1. **The public API is a tuple of five items.** `src/parser/mod.rs:21`:
   ```rust
   pub type ParsedObjects = (
       HashMap<ObjectHash, CommitRaw>,
       TreeEntries,
       NameStore,
       HashIndex,
       ObjectContainer,
   );
   ```
   A library must return a struct with named fields. `CommitRaw` has data that `Commit` also holds after the resolution. Decide if `CommitRaw` stays public. Its comment is also not correct: `src/models/container.rs:164` says "Raw commit data extracted during Phase 1 parsing", but pass 2 reads the commit bodies.
2. **The library draws progress bars.** `src/parser/mod.rs:73` has `fn progress_bar(len: u64) -> ProgressBar`, and `src/parser/bodies.rs:12` has `use indicatif::ProgressStyle;`. A library must send progress through a callback or through `tracing` events only. The binary draws the bars.
3. **The library uses `anyhow`.** All eight files in `src/parser/` use `anyhow` and `.context(...)`. The rule for libraries is `thiserror`. Each `.context` call becomes a `ParseError` variant. This is the largest item.
4. **`find_git_dir` is in the binary.** It is at `src/commands/init.rs:34`. A library user must have it. Plan 09 fix A moves it to `src/repo.rs`.
5. **The hash width is in the public types.** `src/models/container.rs:7` has `pub type ObjectHash = [u8; 20];`. SHA-256 support (plan 09 fix C) changes this type. After the library has other users, this change breaks their code.
6. **Some data is only for the bloat analysis.** `ObjectCopy`, `Pack`, `loose_mtimes`, `size_disk`, and the primary copy are data about the object store. Keep them in the library. A general user pays for them: one 32-byte `ObjectCopy` for each on-disk copy, millions for a large repository. Do not make this optional in this step.

## Decisions already made

- Use a Cargo workspace in this repository. Do not make a separate repository. A separate repository is useful only when a second project uses the library. Before that, it adds version changes in two repositories and slows each change.
- The library crate contains `src/parser/`, `src/models/`, `ParseError`, and the `bench` feature with the benchmarks. A possible path is `crates/git-dag-parser-core`.
- The binary crate stays at the root. It contains `src/cli.rs`, `src/main.rs`, `src/commands/`, and `DagError`.
- The integration tests (`tests/parse.rs`, `tests/model.rs`, `tests/common/`) and `tests/git_oracle.rs` move with the library.

## Open questions

Get answers from the user before the work starts:

1. Is the library for other projects, or only a clear boundary in this tool?
   - For other projects: do items 3 and 5 of "Coupling to repair" before the first release of the library.
   - Only a boundary: the workspace and items 1, 2, and 4 are sufficient. Items 3 and 5 can wait.
2. The name and the path of the library crate.
3. Is `CommitRaw` public (item 1)?
4. Progress: a callback, or `tracing` events only (item 2)?

## Work to do

Do the items in this sequence. Each item is one commit.

1. **Repair items 1, 2, and 4 in the current crate.** No files move.
   1. Replace `ParsedObjects` with a struct. Update `src/commands/init.rs` and the tests.
   2. Remove `indicatif` from `src/parser/`. Show the progress in the binary.
   3. Move `find_git_dir` to `src/repo.rs` (plan 09 fix A). If fix A is already done, skip this item.
   4. Run `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, `cargo clippy --all-targets --features bench -- -D warnings`, and `cargo test`.
2. **Make the workspace and move the files.**
   1. Add `[workspace]` to the root `Cargo.toml`, and make the library crate.
   2. Move `src/parser/`, `src/models/`, `ParseError`, the benchmarks, and the tests to the library crate. Move the dependencies that only the library uses.
   3. The binary crate uses the library crate as a path dependency.
   4. In `.github/workflows/ci.yml`, add `--workspace` to `cargo build`, `cargo test`, and `cargo clippy` in the `test` job and in the `bench` job. If the root is a package and also a workspace, `cargo test` without `--workspace` tests only the root crate. Add `--all` to `cargo fmt --check`.
   5. Make sure that `cargo build --release --bin git-dag-parser` gives the same binary name. The `build-release` job uses it.
   6. Update `scripts/benchmarks/` and `docs/benchmarks/README.md` if they use the old paths or `--features bench` on the root crate.
3. **Change the library errors to `thiserror` (item 3).** Only if the answer to open question 1 is "for other projects". Keep the object hash or the file name in each error, as `.context` does now.
4. **Update the plans and the documents.**
   1. Update the paths in plans 05 to 09.
   2. Update `README.md` and `ARCHITECTURE.md`. Write them in ASD-STE100 Simplified Technical English, the same as the current files.

## Tests

- Each commit: `cargo fmt --check`, `cargo clippy` with and without `--features bench`, and `cargo test`, all with `--workspace` after item 2.
- After item 2, `cargo test --workspace` runs the same number of tests as before the move.
- `git-dag-parser init --repo .` gives the same summary before and after the step.
- Run the benchmark harness on one large repository before and after item 2. The times must not change: the move must not change the code that runs.
- Open a pull request. CI must pass on Linux, macOS, and Windows, and `build-release` must pass for all five targets.

## Out of scope

- Do not publish the library to crates.io.
- Do not make the bloat data (item 6) optional.
- Do not change the hash type (item 5) in this step. Plan 09 fix C does that.
- Do not add the graph code from steps 5 to 8.

## Rules for this work

- Show the diff to the user before each commit. Do not commit until the user approves.
- Do not put a "Co-Authored-By" line or other tagline in the commit message.
- Put unit tests in the source file. Put tests that use real git repositories in `tests/` of the library crate.
- At the end, tell the user what this step does and what the next step is.

## Prompt to start this step

> In this repository, do step 11 from docs/plans/11-move-dag-parsing-to-lib-plan.md. First ask me the open questions. Show me the diff before each commit, and do not commit until I approve.
