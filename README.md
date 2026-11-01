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

For each pull request and each push to `main`, the CI workflow (`.github/workflows/ci.yml`) runs the tests, `cargo clippy`, and `cargo fmt --check` on Linux, macOS, and Windows.

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
