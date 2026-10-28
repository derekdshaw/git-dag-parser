# Benchmarks

This folder keeps benchmark results so that changes can be compared with earlier runs. The scripts are in `scripts/benchmarks/`.

## Micro benchmarks (`cargo bench`)

The benchmarks use [divan](https://docs.rs/divan). Each one is defined next to the code it measures, in a `#[cfg(feature = "bench")] mod benches` block, so it can call private functions. `benches/parser.rs` runs them and installs divan's allocation profiler, which reports allocations, reallocations, and bytes for each iteration.

| Benchmark | Measures |
|---|---|
| `pack/delta_result_size_1000` | Pass 1: read the result size of 1,000 compressed deltas |
| `pack/decompress_data/{4096,65536,1048576}` | Pass 2: inflate tree bodies of 4 KB, 64 KB, and 1 MB |
| `pack/apply_delta/{4096,65536,1048576}` | Pass 2: rebuild a tree from its base and a delta |
| `content/parse_tree/{100,1000,84000}` | Parse a tree body with this many entries |
| `content/parse_commit` | Parse a merge commit body |
| `scan/record_and_finish/100000` | Pass 1 bookkeeping: record 100,000 copies, then sort by hash |
| `parse_objects_repo` | A full parse of a real repository (ignored by default) |

The inputs are synthetic and are checked once before timing. `parse_objects_repo` needs a repository: set `GDP_BENCH_REPO` and pass `--include-ignored`.

```
cargo bench --features bench --bench parser                       # all micro benchmarks
cargo bench --features bench --bench parser -- decompress_data    # only names that match
GDP_BENCH_REPO=<repo> cargo bench --features bench --bench parser -- --include-ignored parse_objects_repo
```

The library reports progress through `tracing`. The benchmark binary does not install a subscriber, so benchmark output has no progress lines.

## Keep and compare results

divan has no saved baselines and no machine-readable output. `scripts/benchmarks/bench.py` runs the benchmarks, saves the table in this folder with the run details (commit, uncommitted changes, rustc and divan versions, CPUs, kernel), and saves a parsed JSON copy:

```
python3 scripts/benchmarks/bench.py run --label <name>
GDP_BENCH_REPO=<repo> python3 scripts/benchmarks/bench.py run --label <name> -- --include-ignored
python3 scripts/benchmarks/bench.py compare docs/benchmarks/<old>.json docs/benchmarks/<new>.json
```

File names are `<UTC time>-<commit>[-dirty][-<label>]`. `dirty` means the working tree had uncommitted changes. `compare` prints the median time and the allocations for each benchmark in both runs.

How to read a comparison:

- Allocation counts and bytes do not change between runs of the same code. Compare them exactly.
- Times vary between runs of the same code. Most medians stay within 5%, but some move by 10–35% (`parse_tree`, `delta_result_size_1000`, `apply_delta/65536`, `record_and_finish`). Run each side more than once before you trust a time change, and confirm end-to-end changes with instruction counts from `hyperfine` (below).

## Whole-program measurements

These scripts measure the release binary on real repositories:

- `scripts/benchmarks/harness.py` runs `git-dag-parser init` once. It records wall time, CPU time, and page faults (from `/usr/bin/time -v`), samples the heap (`RssAnon`) every 0.2 s, reads the phase timings from the info logs, and stops the run above a heap limit or a time limit.
  ```
  python3 scripts/benchmarks/harness.py <label> <repo> <out.json> --bin target/release/git-dag-parser --limit-gb 48
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
