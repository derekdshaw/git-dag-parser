# Benchmarks

This folder keeps benchmark results so that changes can be compared with earlier runs. The scripts are in `scripts/benchmarks/`.

`baseline-before-4.1.md` records the whole-program measurements (time, heap, CPU profiles) at commit `e6c013a`, before step 4.1. The divan run of this code is `20261103T005415Z-eb425b2-baseline`. `eb425b2` is the first commit with the benchmarks. It also has `2a1689f`, which reports progress through `tracing`.

## Micro benchmarks (`cargo bench`)

The benchmarks use [divan](https://docs.rs/divan). Each one is defined next to the code it measures, in a `#[cfg(feature = "bench")] mod benches` block, so it can call private functions. `crates/git-dag-parser-core/benches/parser.rs` runs them and installs divan's allocation profiler, which reports allocations, reallocations, and bytes for each iteration.

| Benchmark | Measures |
|---|---|
| `pack/delta_result_size_1000` | Pass 1: read the result size of 1,000 compressed deltas |
| `pack/decompress_data/{4096,65536,1048576}` | Pass 2: inflate tree bodies of 4 KB, 64 KB, and 1 MB |
| `pack/apply_delta/{4096,65536,1048576}` | Pass 2: rebuild a tree from its base and a delta |
| `pack/delta_cache_hit` | Pass 2: get a cached delta base from the delta cache. It must make 0 allocations |
| `content/parse_tree_new_names/{100,1000,84000}` | Parse a tree body with this many entries, every name new to the `NameStore` (the miss path) |
| `content/parse_tree_known_names/{100,1000,84000}` | The same, with every name already interned and 100,000 other names in the store (the hit path, as when a later version of a directory is parsed) |
| `content/parse_tree_history/{1,8}` | Parse 2,000 directories of 50 entries (100,000 names in the store) in a scattered order, each one this many times in a row: the common case, where consecutive trees often share names |
| `name_store/intern_hit/{1000,10000,100000,1000000}` | Look up every name in a store of this size in a scattered order: the cost of a cold lookup as the table outgrows the caches |
| `name_store/get/{1000,100000,1000000}` | Get the bytes of every name in a store of this size by id, in the same scattered order |
| `content/parse_commit` | Parse a merge commit body |
| `scan/record_and_finish/100000` | Pass 1 bookkeeping: record 100,000 copies, then sort by hash |
| `parse_objects_repo` | A full parse of a real repository (ignored by default) |

The inputs are synthetic and are checked once before timing. `parse_objects_repo` needs a repository: set `GDP_BENCH_REPO` and pass `--include-ignored`.

For `parse_objects_repo`, use zed at the commit in "Repositories" (below). If you use a different repository or a different commit, you cannot compare the `parse_objects_repo` results with the runs in this folder.

```
cargo bench -p git-dag-parser-core --features bench --bench parser                       # all micro benchmarks
cargo bench -p git-dag-parser-core --features bench --bench parser -- decompress_data    # only names that match
GDP_BENCH_REPO=<repo> cargo bench -p git-dag-parser-core --features bench --bench parser -- --include-ignored parse_objects_repo
```

The library reports progress through `tracing`. The benchmark binary does not install a subscriber, so benchmark output has no progress lines.

## Keep and compare results

divan has no saved baselines and no machine-readable output. `scripts/benchmarks/bench.py` runs the benchmarks, saves the table in this folder with the run details (commit, uncommitted changes, rustc and divan versions, CPU model, CPU count, kernel), and saves a parsed JSON copy:

```
python3 scripts/benchmarks/bench.py run --label <name>
GDP_BENCH_REPO=/path/to/zed.git python3 scripts/benchmarks/bench.py run --label <name> -- --include-ignored
python3 scripts/benchmarks/bench.py compare docs/benchmarks/<old>.json docs/benchmarks/<new>.json
```

File names are `<UTC time>-<commit>[-dirty][-<label>]`. `dirty` means the working tree had uncommitted changes. `compare` prints the median time and the allocations for each benchmark in both runs.

How to read a comparison:

- Only compare runs from the same computer. The header of each file records the CPU model, the CPU count, and the kernel.
- Allocation counts and bytes do not change between runs of the same code. Compare them exactly. The exception is `parse_objects_repo` from the run `parallel-pass-2` on: divan counts only the thread that runs the benchmark, and pass 2 allocates on worker threads. Its counts are too low and can change between runs. For the memory of a parse, use the harness (below).
- Times vary between runs of the same code. Most medians stay within 5%, but some move by 10–35% (`parse_tree_*`, `delta_result_size_1000`, `apply_delta/65536`, `record_and_finish`). Run each side more than once before you trust a time change, and confirm end-to-end changes with instruction counts from `hyperfine` (below).

## Repositories

The benchmarks and the oracle test use real repositories. These repositories are not in this project. Get them, then give their paths in these environment variables:

- `GDP_BENCH_REPO`: the repository for `parse_objects_repo`. Use zed at the commit below.
- `GDP_ORACLE_REPOS`: the repositories for `crates/git-dag-parser-core/tests/git_oracle.rs`, separated by `:`.

### zed for `parse_objects_repo`

All the runs in this folder use zed at commit `504a7f2cf25b817cda3998cab93473adef15f93c`. A fetch of only this commit gives the same objects on each computer. To get it, use these commands:

```
git init --bare zed.git
git -C zed.git fetch --no-tags https://github.com/zed-industries/zed.git 504a7f2cf25b817cda3998cab93473adef15f93c:refs/heads/main
```

Then set `GDP_BENCH_REPO` to the path of `zed.git`.

If you change this commit, run all the saved benchmarks again on the new commit, and write the new commit here.

The runs in this folder were made again on 2026-11-03 with this zed commit. Each run is on the commit that it measures, so no run is `dirty`. The earlier runs used a different zed clone. They are only in the Git history.

### Large repositories

Step 4.1 measures these repositories with `harness.py`, `hyperfine`, and the oracle test. Each one is a full clone (all branches) from the date of its commit, on native storage. A new clone has more objects, so compare a new measurement only with measurements on the same clone.

| Repository | URL | HEAD | Objects | Packs |
|---|---|---|---:|---:|
| lage | https://github.com/microsoft/lage | `5586934f687f` (2022-10-08) | 9,294 | 2 |
| zed | https://github.com/zed-industries/zed | `597e5f8304ee` (2024-11-26) | 286,276 | 1 |
| git-ms | https://github.com/microsoft/git | `3d796bd35183` (2022-12-12) | 1,014,510 | 5 |
| vscode | https://github.com/microsoft/vscode | `5bca50de0625` (2024-09-10) | 1,656,020 | 2 |
| linux | https://github.com/torvalds/linux | `75aa74d52f43` (2024-10-18) | 10,456,133 | 1 |

To compare the parse with git on these repositories, use this command:

```
GDP_ORACLE_REPOS=/path/to/zed:/path/to/vscode:/path/to/git-ms \
    cargo test --release -p git-dag-parser-core --test git_oracle -- --ignored --nocapture
```

## Whole-program measurements

These scripts measure the release binary on real repositories:

- `scripts/benchmarks/harness.py` runs `git-dag-parser init` once. It records wall time, CPU time, and page faults (from `/usr/bin/time -v`), samples the heap (`RssAnon`) every 0.2 s, reads the pass 1 and pass 2 timings from the info logs and the `Parse:`, `Attribution:`, `Paths:`, `Reachability:`, and `Total:` timings from the stdout summary, gives the peak heap of each phase window (`pass1`, `pass2`, `resolve_refs`, `attribution`, `paths`, `reachability`, `summary`), and stops the run above a heap limit or a time limit. A window shorter than 0.2 s can have no sample; then its peak is `null`. For example, the `paths` window of git-ms (0.10 s) can have no sample.
  ```
  python3 scripts/benchmarks/harness.py <label> <repo> <out.json> --bin target/release/git-dag-parser --limit-gb 48
  ```
  The harness gives the program only the `RUST_LOG` and `PATH` environment variables. To set a different number of pass 2 threads, give `--bin` a script that sets `RAYON_NUM_THREADS` and starts the program with `exec`. `exec` keeps the process ID, so the harness still samples the program:
  ```
  #!/bin/sh
  exec env RAYON_NUM_THREADS=8 /path/to/git-dag-parser "$@"
  ```
- `hyperfine` 2.0 gives repeated timings and hardware counters. Instruction counts usually change by about 0.01% between runs (up to 0.15%), so they show the effect of a code change better than time does:
  ```
  hyperfine --warmup 1 --runs 5 --env 'RUST_LOG=warn' \
      --metrics time_wall_clock,time_cpu,instructions,cpu_cycles,cache_misses,branch_misses,memory_peak_resident:MiB \
      -n before "<old-bin> init --repo <repo>" -n after "<new-bin> init --repo <repo>"
  ```
- `scripts/benchmarks/hotspots.py` summarizes a [samply](https://github.com/mstange/samply) CPU profile: self time, inclusive time, and the callers of chosen functions. Record from a build with frame pointers:
  ```
  RUSTFLAGS="-C force-frame-pointers=yes" CARGO_PROFILE_RELEASE_DEBUG=true CARGO_TARGET_DIR=target/prof cargo build --release
  RUST_LOG=warn samply record --save-only --unstable-presymbolicate -o prof.json.gz -- target/prof/release/git-dag-parser init --repo <repo>
  python3 scripts/benchmarks/hotspots.py prof.json.gz --main-only --top 25 --callers _memcpy_avx_unaligned_erms
  ```

Hardware counters and `samply` need `kernel.perf_event_paranoid` at 1 or lower: `echo 1 | sudo tee /proc/sys/kernel/perf_event_paranoid`.

To compare a benchmark between two commits, run the two binaries in turn on one core: `taskset -c 8 <bench-binary> --bench --sample-count 200 --sample-size 10000 <name>`. Runs that are not on one core can hide a difference of 15%. If a benchmark changes but its code does not, compare the address of its loops in the two binaries: a small loop that moves across a 64-byte boundary can change the result by this much. To profile a benchmark on one core, put `taskset` after `samply record --`, not before `samply`, and give `hotspots.py` the name of the benchmark binary with `--thread-prefix`.
