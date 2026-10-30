# Step 10: GitHub CI and releases

## Summary

This step adds a GitHub Actions workflow, `.github/workflows/ci.yml`, based on the workflow in `~/r/engraph`.

- On each pull request and each push to `main`, CI checks the format, runs clippy, and runs the tests on Linux, macOS, and Windows.
- On each pull request and each push to `main`, CI also builds and packages the release binary for five targets, as a dry run. The archives are workflow artifacts that expire after 7 days. CI does not make a tag or a GitHub release.
- Only a manual run of the workflow (`workflow_dispatch` on `main`) publishes a release. That run builds the five targets, makes the tag `v<version>` from `Cargo.toml`, and publishes a GitHub release with one archive for each target.
- At this time, the tests do not compile on Windows, and one test fixture cannot work on macOS. This step makes the tests portable first.

## Project and status before this step

- Repository: `/home/derekdshaw/r/git-dag-parser` (Rust, one binary and library crate, edition 2021, version `0.1.0`).
- There is no `.github/` directory, no `v*` tag, and no GitHub release. The repository is public.
- `intaglio` needs Rust 1.85 or later (its `rust-version`). `Cargo.toml` has no `rust-version`.
- The tests run `git` to make fixture repositories. All GitHub runners have `git`.
- `tests/git_oracle.rs` is ignored by default. It stays ignored in CI.
- The benchmarks are in `#[cfg(feature = "bench")]` modules in the library. A default build does not compile them.

## Local build results (2026-10-30)

Host: WSL2 on x86_64. Windows was built with the Windows Rust toolchain (1.96.0) and MSVC 14.44.

| Target | `cargo check --all-targets --features bench` | Release link | Run |
|---|---|---|---|
| `x86_64-unknown-linux-gnu` | pass | pass | pass: `cargo test`, fmt, clippy (also with `--features bench`) |
| `aarch64-unknown-linux-gnu` | pass | pass, ELF aarch64 | not run: no aarch64 glibc on the host |
| `aarch64-unknown-linux-musl` (extra check) | – | pass, static ELF aarch64 | pass: under `qemu-aarch64`, the `init` summary for this repository was equal to the x86_64 summary |
| `x86_64-apple-darwin` | pass | pass, Mach-O x86_64 | not possible on this host |
| `aarch64-apple-darwin` | pass | pass, Mach-O arm64 | not possible on this host |
| `x86_64-pc-windows-msvc` | **fail** for the test targets (see below). The library and binary pass. | pass, PE32+ | pass: the `init` summary was equal to the Linux summary. The 67 unit tests pass. |

aarch64 Linux and macOS were linked locally with a different linker than CI uses. Thus CI is the first real test of those build jobs.

### Problems found

1. **The integration tests do not compile on Windows.** `tests/common/mod.rs:11` imports `std::os::unix::ffi::OsStrExt`, and line 104 uses `OsStr::from_bytes` to make a Latin-1 file name:
   ```
   error[E0433]: cannot find `unix` in `os`
     --> tests/common/mod.rs:11:14
   error[E0599]: no associated function or constant named `from_bytes` found for struct `OsStr` in the current scope
     --> tests/common/mod.rs:104:27
   ```
2. **The same fixture cannot work on macOS.** `Repo::with_non_utf8` writes the file `caf\xe9.txt` to disk. APFS does not accept file names that are not UTF-8. Windows file names are UTF-16, so Windows cannot accept it either.
3. **One test fails on Windows after problem 1 is repaired.** `primary_copy_is_in_the_newest_pack` (`tests/parse.rs:263`) sets the time of a pack file with `fs::File::open(path).unwrap().set_modified(time)`. Git makes pack files read-only. On Windows, a handle from `File::open` cannot change the file times: `PermissionDenied`, "Access is denied." On Linux the owner can do this through a read-only handle.

A trial repair of all three (in a temporary copy, not committed) made all tests pass on Windows (MSVC) and on Linux:

- `with_non_utf8` puts the Latin-1 name into the index with `git update-index --add --index-info`, and gives the entry on stdin as bytes (`100644 <blob hash>\tcaf\xe9.txt\n`). It gets the blob hash from `git hash-object -w` on a file outside the work tree. No file system has to accept the name. The next commit stages only `after.txt`, because `git add -A` would remove the Latin-1 entry that is not on disk.
- `Repo` gets `git_with_input(args, input)`, which writes `input` to the stdin of `git`. `git(args)` calls it with no input.
- The test opens the pack file with the attribute-write right on Windows (`OpenOptionsExt::access_mode(FILE_WRITE_ATTRIBUTES)`, value `0x100`) and with `File::open` on other platforms.

## Decision needed before this step

Ask the user to select:

1. **Archive contents.** engraph puts install scripts and a document in each archive. This project has none. Recommended: the binary, `README.md`, and `LICENSE` only.

## Decisions already made

- A merge to `main` never publishes a release. The user publishes a release with a manual run of the workflow (Actions tab, "Run workflow", or `gh workflow run ci.yml --ref main`). This is different from engraph, which tags and publishes on each push to `main` that has a new version.
- Pull requests run the full build matrix as a dry run, so that the cross-compile jobs are tested before a release.
- Base the workflow on the engraph workflow. Use the same action versions: `actions/checkout@v5`, `dtolnay/rust-toolchain@stable`, `actions/cache@v5`, `actions/upload-artifact@v5`, `actions/download-artifact@v5`, `softprops/action-gh-release@v2`.
- Build these five targets, the same as engraph:

  | Target | Runner | Archive |
  |---|---|---|
  | `x86_64-unknown-linux-gnu` | `ubuntu-latest` | `tar.gz` |
  | `aarch64-unknown-linux-gnu` | `ubuntu-latest`, with `gcc-aarch64-linux-gnu` | `tar.gz` |
  | `x86_64-apple-darwin` | `macos-latest` | `tar.gz` |
  | `aarch64-apple-darwin` | `macos-latest` | `tar.gz` |
  | `x86_64-pc-windows-msvc` | `windows-latest` | `zip` |

- Archive names: `git-dag-parser-v<version>-<target>.<tar.gz|zip>`.
- Make the tests portable. Do not skip tests on Windows or macOS.
- Do not copy the engraph jobs `codegraph-live` and `otel-build`. Add a `bench` job in their place (see item 4).

The jobs:

| Job | Runs on | Does |
|---|---|---|
| `test` | each event | fmt, clippy, build, and tests on Linux, macOS, and Windows |
| `bench` | each event | clippy with `--features bench` on Linux |
| `version` | each event | reads the version from `Cargo.toml` |
| `build-release` | each event, after `test`, `bench`, and `version` | builds and packages the five targets, uploads each archive as a workflow artifact |
| `publish-release` | only `workflow_dispatch` on `main`, after `build-release` | makes the tag and publishes the GitHub release |

## Work to do

1. **Make the tests portable** (one commit, before the workflow):
   1. Make the three repairs in "Problems found". Remove the `std::os::unix` import.
   2. Run `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, and `cargo test` on Linux.
   3. Run `cargo test --target x86_64-pc-windows-msvc` with the Windows toolchain, in a temporary clone on the Windows side. Delete the clone after the run.
2. **Set the minimum Rust version.** Add `rust-version = "1.85"` to `[package]` in `Cargo.toml`. It agrees with `intaglio` and with `README.md`.
3. **Add `.github/workflows/ci.yml`, job `test`.** Matrix `ubuntu-latest`, `macos-latest`, `windows-latest`, with `fail-fast: false`. Steps: checkout, toolchain with `clippy` and `rustfmt`, cache, `cargo build --verbose`, `cargo test --verbose`, `cargo clippy --all-targets -- -D warnings`, `cargo fmt --check`.
4. **Job `bench`** (Linux only). `cargo clippy --all-targets --features bench -- -D warnings`. This compiles the benchmark modules, which no other job compiles. Do not run the benchmarks in CI: shared runners give times that are not stable.
5. **Job `version`.** Read the version from `[package]`, not from `[workspace.package]` as engraph does. The engraph `sed` gives an empty value here, and a tag named `v` would follow. Use: `sed -n '/^\[package\]/,/^\[/{s/^version = "\(.*\)"/\1/p;}' Cargo.toml`. Stop with an error if the value is empty. Give the output `version=v<version>`.
6. **Job `build-release`.** Copy the matrix and the steps from the engraph job, with these changes:
   - `needs: [test, bench, version]`, and no `if`. It runs on pull requests, on pushes to `main`, and on manual runs.
   - Change `--bin engraph` to `--bin git-dag-parser`.
   - In the package steps, copy only the binary, `README.md`, and `LICENSE` (or what the user selects). Use the version from the `version` job in the archive name.
   - In `actions/upload-artifact`, set `retention-days: 7`. These files are not a release.
7. **Job `publish-release`.** Replaces the engraph jobs `tag-and-release` and `publish-release`:
   - `if: github.event_name == 'workflow_dispatch' && github.ref == 'refs/heads/main'`, `needs: [version, build-release]`, `permissions: contents: write`.
   - Check out with `fetch-depth: 0`. If the tag `v<version>` exists, stop with an error that tells the user to change the version in `Cargo.toml`. Do not move or replace a tag.
   - Make the tag on the commit that was built (`github.sha`) and push it.
   - Download all artifacts and publish the release with `softprops/action-gh-release@v2`, the same as engraph (`generate_release_notes: true`, `prerelease` if the version contains `-`).
   - Add `workflow_dispatch:` to the `on:` section of the workflow.
8. **Update `README.md`.** Add a short section about the release archives, the platforms, and how to publish a release with a manual run. Write it in ASD-STE100 Simplified Technical English, the same as the current file.

## Tests

- Item 1 is the test change. All tests must pass on Linux and on Windows (MSVC) before the commit.
- CI is the only test of macOS and of the release builds:
  - Open a pull request. The `test` job must pass on all three platforms, and `build-release` must pass for all five targets. `publish-release` must show as skipped.
  - Download the Linux x86_64 artifact from the pull request run, extract it, and run `git-dag-parser init --repo .` on this repository. Do the same with the Windows artifact on Windows if it is possible.
  - After the merge, make sure that the push run made no tag and no release (`gh release list` is empty, and `git ls-remote --tags origin` has no `v*` tag).
  - Publish the first release only when the user asks for it. Then make sure that the release has five archives.
- Use `actionlint` on the workflow file before the commit if it is available.

## Out of scope

- Do not run the benchmarks or `git_oracle` in CI.
- Do not publish a release during this step unless the user asks for it.
- Do not add install scripts, Homebrew formulas, or `cargo publish`.
- Do not add a minimum-Rust-version CI job. Item 2 only records the version.
- Do not add `aarch64-pc-windows-msvc` or musl targets unless the user asks.

## Rules for this work

- Do item 1 in one commit, and the workflow (items 2 to 8) in a second commit.
- Show the diff to the user before each commit. Do not commit until the user approves.
- Do not put a "Co-Authored-By" line or other tagline in the commit message.
- Put unit tests in the source file. Put tests that use real git repositories in `tests/`.
- Update `README.md` and `ARCHITECTURE.md` to match the code after this step. Write them in ASD-STE100 Simplified Technical English, the same as the current files.
- At the end, tell the user what this step does and what the next step is.

## Prompt to start this step

> In /home/derekdshaw/r/git-dag-parser, do step 10 from docs/plans/10-github-ci-and-releases-plan.md. Ask me the decision first. Then make the tests portable, show me the diff, and do not commit until I approve.
