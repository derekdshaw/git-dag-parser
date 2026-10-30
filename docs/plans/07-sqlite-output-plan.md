# Step 7: SQLite output

## Summary

This step writes the graph to a SQLite database file.

- At this time, `init` writes no file. It shows only a summary. The JSON output (`init --output`, `save_dag`, and `serde`) was removed after step 4.1, part 1.
- After this step, `init` writes one SQLite file. Each object type has its own table. The row id of an object is its index in the container.
- The `analyze` and `query` commands (step 8) and a possible user interface read this file.

## Project and status before this step

- Repository: `/home/derekdshaw/r/git-dag-parser` (Rust, binary and library crate).
- Purpose of the tool: find the cause of repository bloat. Trace large objects back to the commits and paths that added them.
- Completed steps: 1 (tests), 2 (sizes and names), 3 (hashes, parents, tags, refs), 4 (two-pass read, hash-ordered indices, object copies), 4.1 (compact tree entries, name store, bounded delta cache, parallel parse), 5 (commit rank and owner), 6 (paths, reflogs, liveness).
- The in-memory model is in `src/models/`. Each container index is in hash order, so the row ids do not change between runs on the same repository.
- The parse result struct (step 4.1, part 8) holds `ObjectContainer`, `TreeEntries`, the `NameStore`, and the paths (step 6). `ObjectContainer` does not hold a `NameId`.

## Decision needed before this step

The tree edge tables (`tree_subtrees`, `tree_blobs`) have one row for each tree entry of each tree version. A large test monorepo has approximately 1.2 billion tree entries (an estimate at 50 bytes of tree data for each entry; on linux the real count was 528 million, 34% more than this estimate gave, so plan for about 1.6 billion). With a name blob in each row, these tables can be 30 GB or more, and the write can take a long time.

The bloat reports in step 8 do not need these tables. They use `introduced_by_commit_id` and `introduced_at_path_id`. The tables are necessary only for questions such as "which trees contain this blob".

Ask the user to select one option:

1. **Recommended:** do not write the edge tables by default. Add an option `--tree-edges` that writes them. When they are written, store a `name_id` that points to a `names` table, not the name bytes.
2. Always write the edge tables, with a `names` table.

Record the decision in the step report.

## Decisions already made

- Use one table for each object type. Do not use one shared `objects` table. Each container maps directly to one table, and the index is the row id.
- References that can point to more than one type (tag target, ref target, reflog target, object copy) use two columns: `*_type` and `*_id`. These columns have no foreign key.
- A view `objects` joins the four object tables with `UNION ALL`. Use it for queries such as "largest object of any type".
- A tree edge stores the entry name and mode. It does not store a path id.
- Write names from the `NameStore` of step 4.1. Do not copy them to new `Vec<u8>` values before the write.
- Do not write `NameId` values (step 4.1). Give the rows of the `names` table ids in the sort order of the name bytes. Make a table from `NameId` to this id for the write.
- A path in memory is a parent and a name (step 6). Write `paths.name` from the `NameStore`. Make `paths.full_path` from the parent chain during the write.
- `liveness` values: 0 = `Unreachable`, 1 = `ReflogOnly`, 2 = `IndexOnly`, 3 = `Ref`. Write these values to the `meta` table too.
- Store hashes as 20-byte `BLOB` values. Put a unique index on each `hash` column.
- Store names, paths, and messages as `BLOB` values, because git does not require UTF-8.
- Use the `rusqlite` crate with the `bundled` feature.

## Schema

```sql
CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
-- keys: tool_version, repo_path, git_dir, created_at, hash_algorithm

CREATE TABLE commits (
  id INTEGER PRIMARY KEY, hash BLOB NOT NULL UNIQUE,
  size INTEGER NOT NULL, size_disk INTEGER NOT NULL,
  tree_id INTEGER REFERENCES trees(id),
  commit_time INTEGER NOT NULL, corrected_date INTEGER NOT NULL, rank INTEGER NOT NULL,
  boundary INTEGER NOT NULL, liveness INTEGER NOT NULL);

CREATE TABLE commit_parents (
  commit_id INTEGER NOT NULL REFERENCES commits(id),
  parent_id INTEGER NOT NULL REFERENCES commits(id),
  ordinal INTEGER NOT NULL, PRIMARY KEY (commit_id, ordinal));

CREATE TABLE trees (
  id INTEGER PRIMARY KEY, hash BLOB NOT NULL UNIQUE,
  size INTEGER NOT NULL, size_disk INTEGER NOT NULL,
  introduced_by_commit_id INTEGER REFERENCES commits(id),
  introduced_at_path_id INTEGER REFERENCES paths(id),
  liveness INTEGER NOT NULL);

CREATE TABLE blobs (
  id INTEGER PRIMARY KEY, hash BLOB NOT NULL UNIQUE,
  size INTEGER NOT NULL, size_disk INTEGER NOT NULL,
  introduced_by_commit_id INTEGER REFERENCES commits(id),
  introduced_at_path_id INTEGER REFERENCES paths(id),
  liveness INTEGER NOT NULL);

-- Only with --tree-edges (see "Decision needed"):
CREATE TABLE names (id INTEGER PRIMARY KEY, name BLOB NOT NULL UNIQUE);

CREATE TABLE tree_subtrees (
  tree_id INTEGER NOT NULL REFERENCES trees(id),
  subtree_id INTEGER NOT NULL REFERENCES trees(id),
  name_id INTEGER NOT NULL REFERENCES names(id));

CREATE TABLE tree_blobs (
  tree_id INTEGER NOT NULL REFERENCES trees(id),
  blob_id INTEGER NOT NULL REFERENCES blobs(id),
  name_id INTEGER NOT NULL REFERENCES names(id), mode INTEGER NOT NULL);

CREATE TABLE tags (
  id INTEGER PRIMARY KEY, hash BLOB NOT NULL UNIQUE,
  size INTEGER NOT NULL, size_disk INTEGER NOT NULL, name BLOB NOT NULL,
  target_hash BLOB NOT NULL, target_type TEXT, target_id INTEGER,
  liveness INTEGER NOT NULL);

CREATE TABLE refs (
  id INTEGER PRIMARY KEY, name BLOB NOT NULL UNIQUE, kind TEXT NOT NULL,
  target_hash BLOB NOT NULL, target_type TEXT, target_id INTEGER);

CREATE TABLE reflog_entries (
  id INTEGER PRIMARY KEY, ref_name BLOB NOT NULL,
  old_hash BLOB NOT NULL, new_hash BLOB NOT NULL,
  old_type TEXT, old_id INTEGER, new_type TEXT, new_id INTEGER,
  time INTEGER NOT NULL, message BLOB NOT NULL);

CREATE TABLE index_roots (object_type TEXT NOT NULL, object_id INTEGER NOT NULL);

CREATE TABLE paths (
  id INTEGER PRIMARY KEY, parent_id INTEGER REFERENCES paths(id),
  name BLOB NOT NULL, full_path BLOB NOT NULL UNIQUE, depth INTEGER NOT NULL);

CREATE TABLE packs (
  id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE, size INTEGER NOT NULL,
  mtime INTEGER NOT NULL, keep INTEGER NOT NULL, cruft INTEGER NOT NULL,
  promisor INTEGER NOT NULL);

CREATE TABLE object_copies (
  object_type TEXT NOT NULL, object_id INTEGER NOT NULL,
  pack_id INTEGER REFERENCES packs(id),  -- NULL for a loose copy
  pack_offset INTEGER, size_disk INTEGER NOT NULL,
  is_delta INTEGER NOT NULL, mtime INTEGER NOT NULL, is_primary INTEGER NOT NULL);

CREATE VIEW objects AS
  SELECT 'commit' AS type, id, hash, size, size_disk, liveness FROM commits
  UNION ALL SELECT 'tree', id, hash, size, size_disk, liveness FROM trees
  UNION ALL SELECT 'blob', id, hash, size, size_disk, liveness FROM blobs
  UNION ALL SELECT 'tag', id, hash, size, size_disk, liveness FROM tags;
```

Add indexes after the data is in the tables:

- `trees(introduced_by_commit_id)`, `trees(introduced_at_path_id)`, and the same for `blobs`.
- `paths(parent_id)`, `commit_parents(parent_id)`, `tree_subtrees(subtree_id)`, `tree_blobs(blob_id)`.
- `object_copies(object_type, object_id)`.

## Work to do

1. Add `rusqlite` with the `bundled` feature to `Cargo.toml`.
2. Make `src/store/mod.rs` with `write_database(parsed, path) -> Result<()>`. `parsed` is the parse result struct (step 4.1, part 8). The container alone is not sufficient: the names, the paths, and `TreeEntries` are not in it. Export the function from `src/lib.rs`.
3. Put the schema in one SQL string or file. Run it with `execute_batch`.
4. Write all rows in one transaction. Use prepared statements.
5. Set `PRAGMA journal_mode = OFF` and `PRAGMA synchronous = OFF` for the write. The tool makes the file again on each run, so a failed write is not a problem.
6. If the output file exists, remove it first.
7. In `init`, add the option `--output <file>` for the database path. The default is `<repo>/.git-dag.db`. The help text gives this default.
8. Show the time of the database write in the `init` output.

## Tests

Write the tests first. Make sure that they fail before you add the new code.

Unit tests (in `src/store/mod.rs`):

- Write a small hand-made parse result to an in-memory database (`Connection::open_in_memory`). Read each table back. Make sure that each row has the correct values.
- The `objects` view returns all four types.

Integration tests (in `tests/`, with `Repo` fixtures):

- Run the full `init` on a fixture. Open the database.
- The row count of each table equals the container count, and equals the git count for objects.
- Each `hash` in each object table is in `git cat-file --batch-all-objects`.
- A query for the largest blob gives the correct hash, the hash of the introducing commit, and the correct path. Get the expected values from git.

## Make sure the result is correct on real repositories

- Run `init` on `engraph` and the large test monorepo. Write the database for the monorepo to the session scratchpad or to a native Linux path, not to `/mnt/d`. Do not write to the monorepo.
- Record the database size and the write time, with and without `--tree-edges`.
- Run three example queries in the `sqlite3` tool. Show the results to the user.

## Out of scope

- Do not add report commands. Step 8 does that.
- Do not add a user interface.

## Rules for this work

- Do one step in one commit.
- Show the diff to the user before the commit. Do not commit until the user approves.
- Do not put a "Co-Authored-By" line or other tagline in the commit message.
- Put unit tests in the source file. Put tests that use real git repositories in `tests/`.
- Update `README.md` and `ARCHITECTURE.md` to match the code after this step. Write them in ASD-STE100 Simplified Technical English, the same as the current files.
- At the end, tell the user what this step does and what the next step is.

## Prompt to start this step

> In /home/derekdshaw/r/git-dag-parser, do step 7 from docs/plans/07-sqlite-output-plan.md. Write the tests first and show me that they fail. Then do the work, show me the diff, and do not commit until I approve.
