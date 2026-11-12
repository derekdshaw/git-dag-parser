# git-dag-parser

git-dag-parser reads the object store of a Git repository directly. It does not start `git` processes. Its parser reads the pack files and the loose objects.

The tool helps you find the cause of repository bloat. It finds the commit that introduced each object, and the path where that commit added it. It also finds the objects that only a reflog or the index keeps, and the objects that `git gc` can remove.

## Project condition

The project is not complete. At this time, only the `init` command operates.

- `init` parses all objects in the repository, finds the commit that introduced each tree and each blob and the path where it was added, finds the reachability state of each object, and shows a summary. It does not write a file.
- `analyze` and `query` show only a message. When the project is complete, these commands will read a SQLite database that `init` writes.

## Terms

The documents and the source code use these terms.

**Git storage**

| Term | Meaning |
|---|---|
| Object | A commit, a tree, a blob, or a tag. Its hash identifies it. |
| Object store | The `objects` directory of the Git directory. It contains the loose objects and the pack files. |
| Loose object | An object in a file of its own, `objects/xx/yyyy...`. The file is one zlib stream. |
| Pack file, pack | A file `objects/pack/pack-*.pack` that contains many objects. |
| Pack index | The `.idx` file of a pack. It gives the offset of each object in the pack, by hash. |
| Pack entry, entry | One object in a pack: a header and a zlib stream. The position of an entry is its place in the pack, in offset order. |
| Delta | A pack entry that keeps an object as the changes to a different object, its base. |
| Base | The object that a delta changes. A base can also be a delta. |
| Delta chain | A delta, its base, the base of that base, and so on, to an entry that is not a delta. To make a delta's object, the parser makes each object of the chain. |
| Delta family | An entry that is not a delta, and all deltas whose chains end at that entry. |

**The parse**

| Term | Meaning |
|---|---|
| Copy | One place on disk that contains an object: a pack entry or a loose object. An object can have more than one copy. |
| Primary copy | The one copy of an object that the parser reads. |
| Index | The position of an object among the objects of its type, sorted by hash. The parser refers to objects by index, not by hash. |
| Pass 1 | The first read of the object store. It reads only the object headers: the type, the size, and the copies of each object. |
| Pass 2 | The second read. It reads the bodies of commits, trees, and tags. It does not read blob bodies. |
| Read | In pass 2: the decompression and parse of the body of one primary copy. |
| Work unit, unit | A group of reads that one thread does from start to end. A unit is whole delta families of one pack, in offset order, or a group of loose objects. Thus a thread never needs a base that a different thread made. |
| Worker | One thread of pass 2. Each worker takes units from one shared queue, and keeps its results apart from the other workers. |
| Pack reader | The part that reads the entries of one unit. It has its own zlib decoder and its own delta cache. |
| Delta cache | Keeps the bases that the later reads of a unit need, so that the reader does not make them again. Each worker gets an equal share of the byte limit. |
| Hit, miss | A hit: the base is in the delta cache. A miss: the cache removed the base because it was full, and the reader makes the base again. |
| Tree entries | The entries of all trees: for each entry, the index of its child, its name, and its kind. |
| Name store | Keeps one copy of each different entry name. A tree entry refers to its name by a `NameId`. |
| Merge | The last step of pass 2. It puts the results of all workers together: one name store, and the tree entries in tree index order. |

**Attribution**

| Term | Meaning |
|---|---|
| Corrected date | A commit time that is always larger than the corrected date of each parent: the larger of the committer time and the largest corrected date of the parents plus 1. |
| Rank | The position of a commit when the commits are sorted by (corrected date, hash). Rank 0 is first. An ancestor always has a lower rank than its descendants. |
| Owner, introduced by | For a tree or a blob: the commit with the lowest rank that can reach it. |
| Frontier | The trees that the threads process in one round of the owner calculation. |
| In-degree | For a tree: the number of tree entries that point to it. |

**Paths and reachability**

| Term | Meaning |
|---|---|
| Name | One entry name in one tree, for example `main.rs`. Git keeps only names. |
| Path | The names from the root tree of a commit to a file or a directory, for example `src/parser/main.rs`. Git does not keep paths. The root tree has the empty path. |
| Path table | Each different path one time, as its parent path and its last name. The id of a path is its position in the table, in the sequence of the full path bytes. |
| Introducing parent | For an object with owner `c`: a tree that contains the object and also has owner `c`. The object gets its path from this tree. |
| Reflog entry | One line of a file under `.git/logs/`: a ref changed from an old hash to a new hash. |
| Index file | The file `.git/index`. It lists the staged files with their blob hashes, and it can keep tree hashes (cache-tree) and the blobs of a resolved conflict (resolve-undo). |
| Root | An object that keeps other objects alive: a ref target, an object of the index file, or the old or new object of a reflog entry. |
| Liveness | The reachability state of an object: `Ref` (a ref reaches it), `IndexOnly` (only the index file reaches it), `ReflogOnly` (only a reflog entry reaches it), or `Unreachable` (no root reaches it, and `git gc` can remove it). An object gets the strongest state of the roots that reach it, in this sequence: `Ref`, `IndexOnly`, `ReflogOnly`. |

## Necessary tools

- Rust 1.85 or a subsequent version.
- Git, only for the tests. The tests use the `git` command to make small repositories.

## Compile the program

To compile the program, use this command:

```
cargo build --release
```

The program is `target/release/git-dag-parser`.

## Release archives

Each GitHub release has one archive for each of these platforms:

| Platform | Target | Archive |
|---|---|---|
| Linux x86_64 | `x86_64-unknown-linux-gnu` | `tar.gz` |
| Linux ARM64 | `aarch64-unknown-linux-gnu` | `tar.gz` |
| macOS x86_64 | `x86_64-apple-darwin` | `tar.gz` |
| macOS ARM64 | `aarch64-apple-darwin` | `tar.gz` |
| Windows x86_64 | `x86_64-pc-windows-msvc` | `zip` |

The name of each archive is `git-dag-parser-v<version>-<target>.<tar.gz|zip>`. Each archive contains the program, `README.md`, and `LICENSE`.

### Publish a release

A merge to `main` does not publish a release. To publish a release, do these steps:

1. Change `version` in the `[package]` section of `Cargo.toml`. Merge the change to `main`.
2. Start the CI workflow manually on `main`. Use "Run workflow" on the Actions tab, or use this command:

   ```
   gh workflow run ci.yml --ref main
   ```

The workflow builds the five archives. Then it makes the tag `v<version>` on the commit that it built, and publishes the release with the archives. If the tag exists, the workflow stops with an error. It does not change a tag that exists.

For each pull request and each push to `main`, the workflow also builds the five archives as a test. It does not make a tag or a release. The archives are workflow artifacts, and GitHub deletes them after 7 days.

## Parse a repository

To parse a repository, use this command:

```
target/release/git-dag-parser init --repo /path/to/repo
```

You can give `--repo` one of these types of repository:

- A work tree that contains a `.git` directory.
- A bare repository.
- A linked work tree, where `.git` is a file that refers to the Git directory.

If you do not use `--repo`, the program parses the repository in the current directory. The program does not look for `.git` in the parent directories.

After the program parses the repository, it shows a summary. This is an example summary for this repository:

```
=== DAG Summary ===
Commits: 104
Trees:   341
Blobs:   486
Tags:    0
Total:   931
Names:   98 distinct, 2.1 KB
Tree entries: 2251
Delta cache: 0 hits, 0 misses, 0 evictions in 0 packs
Introduced by a commit: 337 of 341 trees, 381 of 486 blobs
Corrected commit dates: 7 of 104 commits
Distinct paths: 134
Roots: 15 refs, 213 reflog entries, 100 index objects
Ref:         793 objects, 2.17 MB on disk
IndexOnly:   0 objects, 0 B on disk
ReflogOnly:  16 objects, 85.04 KB on disk
Unreachable: 122 objects, 491.57 KB on disk

=== Timing ===
Pass 1: 24.751838ms
Pass 2: 13.653169ms
Parse:  41.135282ms
Attribution: 45.108021ms
Paths: 75.936203ms
Reachability: 46.313318ms
Total:  208.514845ms
```

- `Commits`, `Trees`, `Blobs`, and `Tags` show the number of different objects of each type. The program counts each object one time, also when more than one pack file contains it.
- `Names` shows the number of different file names and directory names in all trees. It also shows the total length of these names in kilobytes.
- `Tree entries` shows the number of entries in all trees.
- `Delta cache` shows the delta cache hits, misses, and evictions of pass 2, for all packs together. In this example, the repository has only loose objects, thus there are no packs.
- `Introduced by a commit` shows the number of trees and blobs that a commit introduced. The other objects have no owner: no commit reaches them. For example, an annotated tag can refer to them, or an amended commit left them.
- `Corrected commit dates` shows the number of commits whose corrected date is not equal to their committer time. This includes each commit that has a parent with the same committer time, to the second. On linux, most of these commits are of this type, and only a few have a parent with a later time.
- `Distinct paths` shows the number of rows in the path table: the paths where the owner commits added their trees and blobs, and the parent directories of these paths. The empty path of the root trees is one of them.
- `Roots` shows the number of refs, the number of reflog entries in `logs/HEAD` and `logs/refs/`, and the number of different objects that the index file keeps.
- `Ref`, `IndexOnly`, `ReflogOnly`, and `Unreachable` show the number of objects in each liveness state, and the sum of their sizes on disk. The four lines count each object one time. `git reflog expire` and `git gc` can remove the `ReflogOnly` objects after their reflog entries expire. `git gc` can remove the `Unreachable` objects.
- `Pass 1` and `Pass 2` show the time of each pass. `Parse` shows the time that the program uses to parse the objects. `Attribution` shows the time of the rank and owner calculation. `Paths` shows the time of the path calculation. `Reachability` shows the time of the liveness calculation. `Total` shows the time for the full command.

### Threads and memory

Pass 2, the header read and the sorts by hash of pass 1, the owner calculation, the path calculation, and the walk of the trees in the liveness calculation use more than one thread. They use a quarter of the hardware threads of the computer. If that is fewer than 8, they use 8 threads. On a computer with 8 hardware threads or fewer, they use one thread fewer than the computer has, so that one stays free. For example, they use 32 threads on 128, 8 on 16, 7 on 8, and 3 on 4. To set a different number, set the `RAYON_NUM_THREADS` environment variable.

`--delta-cache-size` sets the byte limit of the delta caches of all threads together. The default is `1gb`. Each thread gets an equal share. A smaller limit uses less memory, but the program can be slower, because it makes more bases again.

```
RAYON_NUM_THREADS=8 target/release/git-dag-parser init --repo /path/to/repo --delta-cache-size 512mb
```

## Log messages

The program uses `tracing` to show progress messages and progress bars. The `RUST_LOG` environment variable sets the log level. If you do not set `RUST_LOG`, the level is `info`.

To show only warnings and errors, use this command:

```
RUST_LOG=warn target/release/git-dag-parser init --repo /path/to/repo
```

At the `warn` level, the program does not show progress bars.

## Tests

To run the tests, use this command:

```
cargo test --workspace
```

The repository is a Cargo workspace. Without `--workspace`, `cargo test` tests only the program, and not the parser library in `crates/git-dag-parser-core`.

- The unit tests are in the source files, in `#[cfg(test)]` modules.
- The integration tests of the parser are in `crates/git-dag-parser-core/tests/`. Each test makes a small repository with `git`. Then it compares the parser output with the output of `git`. The tests make the repositories with the `Repo` fixtures of `crates/test-fixtures`. `index_and_reflogs.rs` compares the index roots with `git ls-files --stage` and `git write-tree`, and the reflog entries with `git reflog show`.
- The integration tests of the program are in `tests/`. They use the same fixtures. `tests/attribution.rs` compares the commit that introduced each object with `git rev-list`. `tests/paths.rs` compares the path of each object with `git ls-tree`. `tests/reachability.rs` compares the liveness of each object with `git rev-list --all`, `--indexed-objects`, and `--reflog`.

For each pull request and each push to `main`, the CI workflow (`.github/workflows/ci.yml`) runs the tests, `cargo clippy`, and `cargo fmt --check` for all crates on Linux, macOS, and Windows.

The `git_oracle` test parses large repositories and compares the result with `git`. Cargo does not run this test unless you tell it to, because the test uses repositories that are not in the project. To run it, use this command:

```
GDP_ORACLE_REPOS=/path/to/repo1:/path/to/repo2 cargo test --release -p git-dag-parser-core --test git_oracle -- --ignored --nocapture
```

## Benchmarks

For the benchmarks, and for the scripts that measure the program on large repositories, refer to [docs/benchmarks/README.md](docs/benchmarks/README.md).

## Architecture

For data about how the parser operates, refer to [ARCHITECTURE.md](ARCHITECTURE.md).

## License

MIT. Refer to [LICENSE](LICENSE).
