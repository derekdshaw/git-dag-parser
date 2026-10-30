# Step 4: Hash-ordered indices and object copies

## Summary

This step makes the object indices stable, and it records each stored copy of each object.

- The parser gives each object an index. The index is the position of the object in its container (`commits`, `trees`, `blobs`, `tags`).
- At this time, the index order comes from the order of files in the directory. This order can change between runs.
- After this step, the parser sorts each container by object hash. The index then is the same for each run on the same repository.
- At this time, the parser keeps only the first copy of an object. Other copies in other packs or loose files are lost.
- After this step, the parser records each copy. A later step uses these records to estimate the disk space that `git gc` can remove.

Later steps need this step. Step 5 (commit rank and attribution) and step 6 (paths) use the index as a tie-break. The tie-break must not change between runs.

This step also divides the read into two passes. Pass 1 reads only headers and gives each object its index. Pass 2 reads the content. Step 4.1 needs this order: it stores tree entries as small indices, so all indices must exist before any tree is parsed.

## Project and status before this step

- Repository: `/home/derekdshaw/r/git-dag-parser` (Rust, binary and library crate).
- Purpose of the tool: find the cause of repository bloat. Trace large objects back to the commits and paths that added them.
- Completed steps:
  - Step 1 (`151e24b`): library target, git fixture builder, and git oracle tests in `tests/`.
  - Step 2 (`62d6733`): correct `size` and `size_disk`, one node for each object in overlapping packs, names as bytes.
  - Step 3 (`8cdd22c`): hashes on objects, commit parents and times, tag targets, refs.
- Important files:
  - `src/parser/mod.rs`: `parse_objects` reads packs, then loose objects, then resolves indices and refs.
  - `src/parser/pack.rs`: `parse_pack_files`, `parse_pack_file`, `parse_pack_object`, `disk_sizes`.
  - `src/parser/loose.rs`: `parse_loose_objects`.
  - `src/models/container.rs`: `ObjectContainer`, `HashIndex`, `CommitRaw`, `TreeRaw`, `TreeEntry`.
  - `src/models/objects.rs`: `Commit`, `Tree`, `Blob`, `Tag`, `ObjectId`, `Ref`.
  - `tests/common/mod.rs`: fixture builder `Repo` and git oracles.

## Words used in this plan

- **Copy**: one stored instance of an object. A copy is in a pack file or in a loose file.
- **Primary copy**: the copy that gives the object its `size_disk` value.
- **Duplicate**: an object with two or more copies.
- **Keep pack**: a pack with a `.keep` file next to it. `git gc` does not repack it.
- **Cruft pack**: a pack with a `.mtimes` file next to it. It holds unreachable objects.
- **Promisor pack**: a pack with a `.promisor` file next to it. A partial clone makes these.

## Decisions already made

- Sort each container by hash. The index of an object is its position in the sorted container.
- Use the hash, not the index, to identify an object across different runs. An index changes when the repository gets new objects.
- Record all copies. Show duplicates as a separate item in reports.
- Select the primary copy with a fixed rule:
  1. A pack copy comes before a loose copy.
  2. A copy in a pack that is not cruft and not promisor comes before a copy in a cruft or promisor pack. (Added during the work. The user approved it.)
  3. A copy in a newer pack (larger mtime) comes before a copy in an older pack.
  4. If two packs have the same mtime, use the pack name order.
- This rule is close to the rule that git uses. If git selects a different copy, record the difference in the step report. Do not change the rule to match git in all cases.

## Work to do

1. Add a `Pack` model to `src/models/objects.rs`. Include these fields:
   - `index`, `name` (file name of the `.pack` file), `size` (bytes), `mtime` (seconds).
   - `keep`, `cruft`, `promisor` (booleans from the sidecar files).
2. Add an `ObjectCopy` model. Include these fields:
   - `object` (`ObjectId`), `pack` (`Option<usize>`, `None` for a loose copy).
   - `offset` (`Option<u64>`, pack copies only), `size_disk` (`u64`).
   - `is_delta` (`bool`), `mtime` (seconds, from the pack file or the loose file).
3. Add `packs: Vec<Pack>` and `copies: Vec<ObjectCopy>` to `ObjectContainer`.
4. Change `parse_objects` to read the repository in two passes.
   - **Pass 1 (headers only):**
     1. Read all pack indexes and all loose object file names.
     2. For each pack, read the object headers in offset order. Do not use hash order. Hash order makes random reads, which are very slow on large packs.
     3. Get the type of each object from its header. For a delta, follow the base chain through the headers (`resolved_type` in `src/parser/pack.rs`). Get the size from the delta header (`delta_result_size`).
     4. For a loose object, inflate only the `<type> <size>\0` header.
     5. Make a list of copies for each hash. Select the primary copy with the rule above.
     6. Sort the hashes of each type. Give each object its index from that order. Fill `HashIndex`.
   - **Pass 2 (content):**
     1. Inflate only commits, trees, and tags. Use the primary copy. Never inflate blobs.
     2. Resolve each tree entry hash and each commit parent hash to an index during the parse.
5. Do not give an object an index during pass 2. All indices must exist before pass 2 starts. Then step 4.1 can store tree entries as small indices, not as 20-byte hashes.
6. Show the time of pass 1 and pass 2 separately.
7. Sort `packs` by name. Keep `copies` in read order: pack by pack in name order, offset order in each pack, then loose copies. Pass 2 reads in this order. (Changed during the work. The user approved it.)
8. Keep the fail-on-error rule from step 2. A copy that does not parse stops the run.
9. Remove the "first copy wins" check in `parse_pack_object` (`hash_index.contains`). The new primary-copy rule replaces it.

## Tests

Write the tests first. Make sure that they fail before you change the parser.

Unit tests (put them in a `#[cfg(test)] mod tests` in the same source file):

- The primary-copy rule: pack before loose, newer pack before older pack, name order for equal mtime.
- The index assignment: given hashes in random order, the indices follow hash order.
- The pack flags: a temporary directory with `.keep`, `.mtimes`, and `.promisor` files gives the correct flags.

Integration tests (put them in `tests/`, and use the `Repo` fixtures):

- For each container, the objects are in hash order, and `object.index` equals the position.
- Two parses of the same fixture give the same hashes at the same indices.
- With `Repo::with_overlapping_packs()`, the copies equal the git data:
  - Pack copies: for each `.idx` file, use `git verify-pack -v`. Each line gives the hash, the size in the pack, and the offset.
  - Loose copies: list the files under `.git/objects/??/`.
- A fixture with a `.keep` file and a cruft pack (`git repack --cruft -d`) gives the correct pack flags.

Use positive tests. Do not write many negative tests.

## Make sure the result is correct on real repositories

Use a small program in the session scratchpad. Do not add it to the project.

- Run it on these repositories in `/home/derekdshaw/r`: `engraph`, `agent-work-visualizer`, `mnemosyne`, `home-auction`, `git-dag-parser`.
- Compare each copy with `git verify-pack -v` and the loose files.
- Compare each object `size_disk` with `git cat-file --batch-all-objects --batch-check='%(objectname) %(objectsize:disk)'`.
- Before this step, `engraph` had 12 objects with a different `size_disk`. All were duplicates. Report the new count of differences.

## Next step

Step 4.1 (memory and I/O at scale): compact tree entries, a name store, a delta cache with a size limit, and a parallel pass 2.

## Out of scope

- Do not calculate the `git gc` estimate. Step 8 does that.
- Do not write the database. Step 7 does that.
- Do not replace `HashIndex` with a binary search. Do that only if memory becomes a problem.
- Do not change the tree entry storage, the delta cache, or the thread use. Step 4.1 does that.

## Rules for this work

- Do one step in one commit.
- Show the diff to the user before the commit. Do not commit until the user approves.
- Do not put a "Co-Authored-By" line or other tagline in the commit message.
- Put unit tests in the source file. Put tests that use real git repositories in `tests/`.
- At the end, tell the user what this step does and what the next step is.

## Prompt to start this step

> In /home/derekdshaw/r/git-dag-parser, do step 4 from docs/plans/completed/04-hash-order-and-object-copies-plan.md. Write the tests first and show me that they fail. Then do the work, show me the diff, and do not commit until I approve.
