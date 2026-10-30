# git-dag-parser

git-dag-parser reads the object store of a Git repository directly. It does not start `git` processes. Its parser reads the pack files and the loose objects.

The tool helps you find the cause of repository bloat. When the project is complete, the tool will connect each large object to the commit and the path that added it.

## Project condition

The project is not complete. At this time, only the `init` command operates.

- `init` parses all objects in the repository and shows a summary. It does not write a file.
- `analyze` and `query` show only a message. When the project is complete, these commands will read a SQLite database that `init` writes.

## Necessary tools

- Rust 1.85 or a subsequent version.
- Git, only for the tests. The tests use the `git` command to make small repositories.

## Compile the program

To compile the program, use this command:

```
cargo build --release
```

The program is `target/release/git-dag-parser`.

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
Commits: 27
Trees:   92
Blobs:   193
Tags:    0
Total:   312
Names:   49 distinct, 0.7 KB

=== Timing ===
Parse:  135.732657ms
Total:  135.737749ms
```

- `Commits`, `Trees`, `Blobs`, and `Tags` show the number of different objects of each type. The program counts each object one time, also when more than one pack file contains it.
- `Names` shows the number of different file names and directory names in all trees. It also shows the total length of these names in kilobytes.
- `Parse` shows the time that the program uses to parse the objects. `Total` shows the time for the full command.

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
cargo test
```

- The unit tests are in the source files, in `#[cfg(test)]` modules.
- The integration tests are in `tests/`. Each test makes a small repository with `git`. Then it compares the parser output with the output of `git`.

The `git_oracle` test parses large repositories and compares the result with `git`. Cargo does not run this test unless you tell it to, because the test uses repositories that are not in the project. To run it, use this command:

```
GDP_ORACLE_REPOS=/path/to/repo1:/path/to/repo2 cargo test --release --test git_oracle -- --ignored --nocapture
```

## Benchmarks

For the benchmarks, and for the scripts that measure the program on large repositories, refer to [docs/benchmarks/README.md](docs/benchmarks/README.md).

## Architecture

For data about how the parser operates, refer to [ARCHITECTURE.md](ARCHITECTURE.md).

## License

MIT. Refer to [LICENSE](LICENSE).
