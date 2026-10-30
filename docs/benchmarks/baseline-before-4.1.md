# Performance baseline before step 4.1

Measured 2026-10-27 on commit `e6c013a` ("Shrink ObjectCopy from 72 to 32 bytes"), the last commit before step 4.1 (memory and I/O at scale). Use these numbers to evaluate 4.1: rerun the same repositories with the same harness and compare.

## Summary

- **The parser cannot finish large repositories.** linux (10.4M objects) passed 48 GB of heap during pass 2 and was stopped. Pass 2 keeps every tree entry in memory at about **140 bytes per entry**. linux has about 393M entries (an estimate; the real count, measured after step 4.1 part 2, is 528M), which would need roughly 55 GB.
- **Pass 2 memory grows with tree data.** git-ms (1M objects, 5.9 GB of tree data) grows from 0.3 GB to 18 GB during pass 2 and peaks at 19.3 GB.
- **Pass 1 is 3–4× slower than git** reading the same headers (`git cat-file --batch-all-objects --unordered --batch-check`) on native storage. It runs on one core. The profile shows the cause: for each delta it builds a new zlib decoder and its Huffman tables only to read a few bytes.
- **Decompression is the largest CPU cost.** On the main thread, about 30% (git-ms) to 60% (linux) of samples are in zlib (`miniz_oxide`), in copies around it (`memcpy`), or in decoder setup (`Decompress::new`, `memset`).
- **After pass 2, building the static relationships and resolving tree entries cost 20 s on git-ms** (25% of the run).
- **Kernel time is high:** 45 s of git-ms's 77 s, with 5.7M minor page faults. The profile shows allocation churn on the user side: `Vec`s that grow while bodies are inflated, a copy of the cached base on each delta-cache hit, and 3.5 s in `_exit` to release the heap.
- **Each run uses 1 to 1.5 cores** of the 128 available.

## Environment

| Item | Value |
|---|---|
| Machine | WSL2, kernel 6.6.87.2-microsoft-standard-WSL2, 128 vCPUs, 125 GiB RAM, 32 GiB swap |
| Toolchain | rustc 1.96.0, git 2.43.0 |
| Build | `cargo build --release` with the project profile (`opt-level = 3`, `lto = true`, `codegen-units = 1`) plus `CARGO_PROFILE_RELEASE_DEBUG=true` (symbols only) |
| Command | `git-dag-parser init --repo <path>` with no `--output`, so nothing is written. `RUST_LOG=warn`. |
| `/mnt/d` storage | 9p (drvfs) mount of Windows `D:`. Sequential read 133 MB/s. First random read about 0.6 ms. About 1.8 ms per small file (metadata). |
| Native storage | ext4 inside the WSL VM (`/dev/sdd`). Copies of `.git` in `~/perfrepos/{lage,zed,git-ms,vscode,linux}` (6.7 GB). Keep them for the 4.1 comparison. |
| Tools | `hyperfine` 1.19.0 (repeated timings; later runs use 2.0.0, see below), `/usr/bin/time -v`, `/proc/<pid>/status` sampling every 0.2 s (`harness.py`), `samply` 0.13.1 CPU profiles (`hotspots.py`). The scripts are in the repository in `scripts/benchmarks/`. `samply` needs `kernel.perf_event_paranoid` at 1 or lower; the default is 2. |
| Cache state | Warm page cache unless noted. Caches could not be dropped (needs root). |

Copy speed of each `.git` from `/mnt/d` to native storage: zed 85 MB/s, git-ms 91 MB/s, vscode 66 MB/s, linux 78 MB/s, lage 2 MB/s (lage is mostly small files).

## Repositories

A shallow clone was not used. git-ms has a multi-pack-index and a commit-graph; the parser ignores both. None of the selected repositories is a partial clone.

### Repositories

| Repo | Commits | Trees | Blobs | Tags | Copies | Pack data (GB) | Tree data, uncompressed (GB) |
|---|---:|---:|---:|---:|---:|---:|---:|
| lage | 851 | 3,659 | 4,309 | 344 | 9,295 | 0.01 | – |
| httpie | 1,826 | 4,014 | 4,277 | 2 | 10,119 | 0.01 | – |
| tonic | 1,162 | 6,461 | 4,462 | 0 | 12,085 | 0.08 | – |
| cpp_client_telemetry | 5,636 | 17,519 | 12,440 | 0 | 35,595 | 0.07 | – |
| zed | 28,780 | 164,485 | 93,010 | 1 | 286,276 | 0.12 | 0.12 |
| git-ms | 216,599 | 368,707 | 206,612 | 1,009 | 1,014,510 | 0.5 | 5.92 |
| vscode | 134,232 | 1,112,371 | 407,657 | 53 | 1,656,020 | 0.92 | 0.75 |
| linux | 1,309,717 | 6,303,183 | 2,842,379 | 854 | 10,456,133 | 5.04 | 18.28 |

Tree data is the sum of uncompressed tree sizes from `git cat-file --batch-all-objects --batch-check='%(objecttype) %(objectsize)'`. Entry counts in this report are estimates at 50 bytes of tree data for each entry. The real counts, measured after step 4.1 part 2, are higher: linux 528M, git-ms 147M, vscode 20M, zed 3.6M.

## Results

### Small and medium repositories: wall time (hyperfine, 1 warmup + 5 runs)

| Repo | `/mnt/d` mean ± σ (s) | native mean ± σ (s) | user (s) | sys (s) |
|---|---:|---:|---:|---:|
| lage | 0.725 ± 0.020 | 0.121 ± 0.001 | 0.11 | 0.15 |
| httpie | 0.246 ± 0.010 | – | 0.14 | 0.16 |
| tonic | 0.253 ± 0.013 | – | 0.15 | 0.11 |
| cpp_client_telemetry | 0.873 ± 0.017 | – | 0.72 | 0.50 |
| zed | 6.333 ± 0.080 | 5.590 ± 0.081 | 5.70 | 2.76 |

User and sys times are from the `/mnt/d` runs. On `/mnt/d`, 0.49 s of lage's 0.73 s is spent reading its 266 ref and reflog files (1.8 ms each across the mount). Native, this takes 4 ms.

### Hardware counters (hyperfine 2.0)

Measured 2026-10-28 with `hyperfine` 2.0.0 on the same binary, with `kernel.perf_event_paranoid = 1`. 1 warmup run, then 5 runs (3 for vscode and git-ms). Counters include all threads. Peak RSS includes mapped pack pages.

| Repo | Storage | Runs | Wall (s) mean ± σ | CPU (s) | User (s) | Sys (s) | Instructions (B) | Cycles (B) | IPC | Cache misses (M) | Branch misses (M) | Peak RSS (MiB) |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| lage | /mnt/d | 5 | 0.916 ± 0.048 | 0.36 | 0.09 | 0.27 | 0.59 | 0.41 | 1.45 | 3.2 | 2.0 | 17 |
| lage | native | 5 | 0.139 ± 0.008 | 0.21 | 0.11 | 0.10 | 0.59 | 0.40 | 1.49 | 2.6 | 1.9 | 18 |
| httpie | /mnt/d | 5 | 0.277 ± 0.008 | 0.40 | 0.15 | 0.25 | 0.69 | 0.55 | 1.24 | 4.8 | 2.3 | 18 |
| tonic | /mnt/d | 5 | 0.265 ± 0.017 | 0.30 | 0.14 | 0.16 | 0.82 | 0.57 | 1.45 | 3.6 | 2.1 | 18 |
| cpp_client_telemetry | /mnt/d | 5 | 0.923 ± 0.039 | 1.45 | 0.63 | 0.81 | 2.85 | 2.52 | 1.13 | 23.1 | 9.1 | 88 |
| zed | /mnt/d | 5 | 6.453 ± 0.074 | 9.53 | 5.63 | 3.90 | 24.68 | 21.20 | 1.16 | 165.8 | 83.9 | 681 |
| zed | native | 5 | 5.373 ± 0.067 | 9.10 | 5.47 | 3.64 | 24.69 | 20.84 | 1.18 | 163.5 | 82.1 | 683 |
| vscode | native | 3 | 36.875 ± 0.406 | 57.76 | 40.10 | 17.66 | 152.33 | 153.41 | 0.99 | 1179.0 | 541.2 | 4,251 |
| vscode | /mnt/d | 3 | 46.191 ± 0.249 | 61.71 | 41.11 | 20.60 | 152.33 | 156.21 | 0.98 | 1235.5 | 555.3 | 4,242 |
| git-ms | native | 3 | 76.296 ± 1.563 | 114.46 | 61.65 | 52.81 | 260.25 | 235.52 | 1.11 | 2911.2 | 917.3 | 19,658 |
| git-ms | /mnt/d | 3 | 79.949 ± 0.413 | 114.86 | 61.49 | 53.37 | 260.42 | 235.08 | 1.11 | 2869.8 | 917.6 | 19,656 |

- Instruction counts are the same on `/mnt/d` and native storage. Between runs they vary by 0.004–0.015% (httpie 0.15%), against 0.5–6.4% for wall time. Use them to compare code changes. Wall time also includes storage speed and noise.
- IPC (instructions per cycle) is low, between 1.0 and 1.5. The work waits for memory, which agrees with the cache misses and the large `memcpy` share in the profiles.
- linux is not in this table, because it stops at the 48 GB limit.

### Phase breakdown (seconds)

Single runs from the harness. `Other` is phase 1 minus the named parts: reading refs and resolving commits and tags. `Phase 2 (old walk)` is the per-commit diff in `relationships.rs`, which step 5 replaces.

| Repo | Storage | Wall | Pass 1 | Pass 2 | Resolve entries | Static relationships | Other | Phase 2 (old walk) | User | Sys | CPU % |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| zed | native | 5.8 | 1.59 | 1.88 | 0.17 | 0.20 | 0.11 | 1.40 | 5.8 | 2.5 | 145 |
| zed | /mnt/d | 6.4 | 2.18 | 1.93 | 0.15 | 0.20 | 0.07 | 1.43 | 5.5 | 3.0 | 134 |
| vscode | native | 38.5 | 9.40 | 13.58 | 1.65 | 1.77 | 0.20 | 9.32 | 40.1 | 14.3 | 130 |
| vscode | /mnt/d | 44.5 | 14.84 | 13.45 | 1.66 | 1.70 | 0.60 | 9.39 | 40.5 | 15.3 | 116 |
| git-ms | native | 77.2 | 5.29 | 35.41 | 5.28 | 14.46 | 0.13 | 12.27 | 62.9 | 44.6 | 128 |
| git-ms | /mnt/d | 80.4 | 9.39 | 34.08 | 5.47 | 14.61 | 0.28 | 12.18 | 63.2 | 45.0 | 119 |
| linux | native | 184.4 (stopped) | 62.03 | – | – | – | – | – | 135.4 | 69.3 | 98 |

Repeat runs agreed within about 5% (zed, vscode, git-ms: 2–4 runs each; all are in the data folder).

### Memory (MB)

`Peak heap` is the largest `RssAnon` sample. The phase columns are the largest sample in each phase's time window. `Max RSS` also counts mapped pack pages, which the system can release.

| Repo | Storage | Peak heap | Peak in pass 1 | Peak in pass 2 | Peak in resolve/refs/static | Peak in phase 2 | Max RSS incl. mapped packs |
|---|---|---:|---:|---:|---:|---:|---:|
| zed | native | 620 | 27 | 557 | 545 | 620 | 684 |
| vscode | native | 3,746 | 494 | 3,234 | 3,343 | 3,746 | 4,247 |
| git-ms | native | 19,267 | 280 | 18,052 | 19,156 | 19,267 | 19,661 |
| linux | native | 49,163 (stopped) | 3,363 | over 48,000 | – | – | 54,283 |

The heap grew in pass 2 at about 390 MB/s for linux (from 3.4 GB at 62 s to 48 GB at 176 s).

### Pass 1 compared with git

Both read every object's type and size. Native storage, warm cache.

| Repo | Pass 1 (s) | `git cat-file --batch-all-objects --unordered --batch-check` (s) | Ratio |
|---|---:|---:|---:|
| zed | 1.45 | 0.39 | 3.7× |
| git-ms | 5.20 | 1.22 | 4.3× |
| vscode | 8.00 | 2.27 | 3.5× |
| linux | 62.0 | 21.2 | 2.9× |

linux pass 1 used 98% of one core: it is CPU-bound. The scan of its single 5.2 GB pack took 56.8 s.

### Other measurements

| Measurement | Value |
|---|---|
| Size of model types (`size_of`) | `Commit` 144, `Tree` 152, `Blob` 128, `Tag` 104, `ObjectCopy` 32, `ObjectId` 16, `CommitRaw` 64, `TreeRaw` 32, `TreeEntry` 64 bytes. Each `TreeEntry` also has a heap allocation for its name. |
| Heap per tree entry in pass 2 | git-ms: about 18 GB for about 127M entries = about 140 bytes. vscode: about 2.7 GB for about 16M entries. |
| Heap per object after pass 1 | linux: 3.36 GB for 10.46M objects = about 320 bytes |
| Delta cache (git-ms) | Largest pack: 121,358 bases, 1.80 GB. Second pack: 56,744 bases, 0.98 GB. The cache is dropped when the reader moves to the next pack. |
| Minor page faults | zed 0.20M, vscode 1.14M, git-ms 5.72M, linux 13.1M (stopped). Major faults: 0 on native storage. |

## CPU profiles

Recorded with `samply` 0.13.1 (1 ms sampling, `kernel.perf_event_paranoid = 1`) on a profiling build of the same commit with frame pointers (`RUSTFLAGS="-C force-frame-pointers=yes"`). Native storage. linux was stopped with SIGINT after 130 s: pass 1 (75 s under the profiler, 62 s without) plus about 55 s of pass 2. Summaries come from `hotspots.py`.

How to read the numbers:
- **Self time** is exact: it is the function that held the CPU.
- **Inclusive time** is too low for work done inside glibc (`memcpy`, `malloc`, `memset`). glibc has no frame pointers, so those samples do not reach their callers. About 26–36% of main-thread samples do not reach `main`.
- **LTO inlines most parser functions** into a few symbols. `init::run` contains `parse_objects`, `resolve_tree_entries`, `resolve_commits`, and `build_static_relationships`. `scan_repository` contains `scan_pack` and `delta_result_size`. `read_bodies` contains `PackReader::read` and `parse_tree`.

### CPU time per thread

| Repo | All threads (s) | Main thread (s) | Phase 2 workers (s) |
|---|---:|---:|---:|
| zed | 10.0 | 6.0 | 4.0 |
| vscode | 59.4 | 42.6 | 16.8 |
| git-ms | 113.1 | 85.0 | 28.1 |
| linux (130 s) | – | 141.5 | – |

The phase 2 workers (128 threads) spend most of their time in `syscall`: about 11 s on git-ms, mostly waits on the bounded channel to the main thread. `diff_trees` takes 4.4 s. On zed, starting the threads (`clone3`, `sched_setaffinity`) costs about 1.8 s. Step 5 removes this code.

### Main thread, top self time (% of main-thread samples)

| Function | zed | vscode | git-ms | linux (130 s) |
|---|---:|---:|---:|---:|
| `memcpy` (glibc) | 18.0 | 17.0 | 22.0 | 26.5 |
| `miniz_oxide::inflate::core::init_tree` | 12.1 | 11.4 | 2.9 | 15.6 |
| `miniz_oxide::inflate::core::decompress_fast` | 5.4 | 5.1 | 2.4 | 10.2 |
| `init::run` (inlined: resolve entries, static relationships, resolve commits) | 6.4 | 7.4 | 24.0 | – |
| `read_bodies` (inlined: pack reads, tree parse) | 3.8 | 4.6 | 7.9 | 6.8 |
| `scan_repository` (inlined: pass 1 scan) | 1.7 | 2.6 | – | 5.7 |
| malloc and free (`int_malloc`, `malloc`, `_free`, `int_free`, `unlink_chunk`, `malloc_consolidate`, `realloc`) | 17.5 | 19.4 | 14.7 | 5.7 (plus `brk` 3.6) |
| `memset` (glibc) | 4.8 | 4.4 | 3.1 | 6.1 |
| `HashIndex::insert` and hash map lookups | 3.2 | 3.2 | under 0.5 each | 5.9 |
| `_exit` (freeing the heap at process end) | 1.9 | 1.5 | 4.1 | – |

### Where the copies and allocations come from

Nearest callers of `memcpy` on the main thread:

| Caller | vscode | git-ms | linux |
|---|---:|---:|---:|
| `decompress_data` (`read_to_end` into a `Vec` that starts empty) | 37% | 47% | 27% |
| `flate2::zio::read` (copies through flate2's buffer) | 35% | 9% | 33% |
| `flate2::mem::Decompress::new` (a new decoder of about 40 KB for each stream) | 15% | 4% | 22% |
| `resolve_object_at_offset` (copies a cached base out of the cache on every hit) | 4% | 17% | 6% |
| `int_realloc` (a `Vec` grows) | 2% | 13% | 3% |
| `apply_delta` | – | 7% | 3% |

On linux, pass 1 (`scan_repository`) is 66.7 s inclusive:
- `delta_result_size` builds a new `ZlibDecoder` for each of about 9M deltas. `Decompress::new` alone is 12.1 s inclusive, mostly `memcpy` and `memset` of the decoder state.
- `init_tree` (building the Huffman tables for the first deflate block) is 22.0 s.
- `Scan::record` is 7.8 s, of which `HashIndex::insert` is 5.1 s.
- Sorting offsets is 3.0 s.

`std::io::default_read_to_end::small_probe_read` is 10–12% inclusive on vscode and linux. `read_to_end` starts with an empty `Vec`, but the pack header already gives the inflated size.

## Benchmarks (cargo bench)

`divan` benchmarks are defined next to the code they measure, in `#[cfg(feature = "bench")] mod benches` blocks in the library, so they can call private functions. `benches/parser.rs` runs them and installs `divan`'s allocation profiler. Inputs are synthetic and checked once before timing. `parse_objects_repo` parses a real repository and is ignored unless `--include-ignored` is passed with `GDP_BENCH_REPO` set.

Baseline (`docs/benchmarks/20261028T211041Z-e6c013a-dirty-baseline.json` in the repository; parser code at `e6c013a`, run 2026-10-28, rustc 1.99.1, divan 0.1.21; `parse_objects_repo` on `~/perfrepos/zed`):

| Benchmark | Median | Throughput | Allocations | Bytes allocated | Reallocations |
|---|---:|---:|---:|---:|---:|
| `pack/delta_result_size_1000` | 4.96 ms | 201.7 Kitem/s | 2,000 | 76.1 MB | – |
| `pack/decompress_data/4096` | 8.84 µs | 460.9 MB/s | 3 | 76.1 KB | 8 |
| `pack/decompress_data/65536` | 68.4 µs | 957.6 MB/s | 3 | 76.1 KB | 12 |
| `pack/decompress_data/1048576` | 1.1 ms | 955.1 MB/s | 3 | 76.1 KB | 16 |
| `pack/apply_delta/4096` | 95.1 ns | 43.03 GB/s | 1 | 4.09 KB | – |
| `pack/apply_delta/65536` | 1.04 µs | 62.69 GB/s | 1 | 65.5 KB | – |
| `pack/apply_delta/1048576` | 20.3 µs | 51.65 GB/s | 1 | 1.05 MB | – |
| `content/parse_tree/100` | 4.77 µs | 880.1 MB/s | 101 | 1.66 KB | 5 |
| `content/parse_tree/1000` | 38.2 µs | 1.099 GB/s | 1,001 | 14.2 KB | 8 |
| `content/parse_tree/84000` | 3.64 ms | 969.3 MB/s | 84,001 | 1.18 MB | 15 |
| `content/parse_commit` | 513 ns | – | 1 | 80 B | – |
| `scan/record_and_finish/100000` | 32.4 ms | 3.088 Mitem/s | 13 | 6.58 MB | 80 |
| `parse_objects_repo` | 3.29 s | – | 5,628,196 | 35.7 GB | 1,235,981 |

- `delta_result_size_1000`: each delta makes 2 allocations, 76 KB in total, for new zlib decoder state. This is the pass 1 cost that variant C (decoder reuse with `zlib-rs`) removes.
- `decompress_data`: the output `Vec` grows 8–16 times, because it starts empty.
- `parse_tree`: one allocation for each entry name. The 4.1 name store removes these.
- `parse_objects_repo` (zed): 5.6M allocations and 35.7 GB allocated for one parse of a 128 MB pack.
- Noise: across five runs of the same code on 2026-10-28, most medians varied by less than 5%. Some varied more: `delta_result_size_1000` 4.0–5.0 ms, `apply_delta/65536` 1.0–1.3 µs, `record_and_finish` 32–50 ms. Allocation counts did not change. Compare allocation counts exactly. For time, run each side more than once, and confirm end-to-end changes with `hyperfine` instruction counts.

How to run:

```
python3 scripts/benchmarks/bench.py run --label <name>              # micro benchmarks
GDP_BENCH_REPO=~/perfrepos/zed RUST_LOG=warn \
  python3 scripts/benchmarks/bench.py run --label <name> -- --include-ignored
python3 scripts/benchmarks/bench.py compare docs/benchmarks/<old>.json docs/benchmarks/<new>.json
cargo bench --features bench --bench parser -- <filter>                                  # quick run, no record
```

`bench.py run` saves the raw table with the commit, the dirty flag, rustc, the divan version, and the host, plus a parsed JSON file, in `docs/benchmarks/` in the repository. `compare` prints the median time and allocation change for each benchmark.

## Findings for step 4.1

Each finding links to a part of the 4.1 plan.

1. **Tree entry memory (4.1 parts 1 and 2).** Pass 2 uses about 140 bytes for each tree entry. This stops linux. The target design uses about 9 bytes for each entry (`u32` child, `NameId`, `u8` mode). For linux that is about 3.5 GB of entry arrays (4.7 GB with the real entry count), not about 55 GB.
2. **Static relationships and entry resolution (4.1 part 2).** 20 s of git-ms's 77 s. The profile agrees: 20.4 s of self time in `init::run`, where LTO inlines these functions. 4.1 removes `build_static_relationships` and resolves entries during the parse.
3. **Delta cache (4.1 part 3).** Up to 1.8 GB for one pack on git-ms, with no limit.
4. **Pass 1 speed.** 3–4× slower than `git cat-file`, and CPU-bound on one core. This is not yet a separate item in the 4.1 plan. Profile of linux pass 1 (66.7 s inclusive):
   - `delta_result_size` builds a new `ZlibDecoder` for each of about 9M deltas. `Decompress::new` is 12.1 s inclusive. Reuse one decoder and reset it between objects.
   - `miniz_oxide`'s `init_tree` (Huffman tables) is 22.0 s. git's zlib does the same work in less time. A faster backend (for example `flate2` with the `zlib-rs` feature) is worth a measurement.
   - `Scan::record` is 7.8 s (`HashIndex::insert` 5.1 s), and sorting offsets is 3.0 s.
   - The binary search for each delta's base kind does not show in the profile.
   - `scan_pack` collects each pack's entries in a `Vec<ScannedEntry>` before it records them. This costs memory more than time.
5. **Decompression in pass 2 (4.1 parts 3 and 4).** The profile shows four fixes:
   - `decompress_data` reads with `read_to_end` into an empty `Vec`. The pack header gives the inflated size, so allocate the `Vec` once with that size (`small_probe_read` is 10–12% inclusive; `int_realloc` is 13% of `memcpy` on git-ms).
   - Each object gets a new decoder (`Decompress::new`). Reuse one decoder for each worker.
   - The `flate2` `Read` adapter copies through its own buffer (`flate2::zio::read`, 9–35% of `memcpy`). Decompress directly into the output `Vec` with `Decompress::decompress_vec`.
   - Each delta-cache hit copies the base out of the cache (`resolve_object_at_offset`, 17% of `memcpy` on git-ms). Return a reference to the cached bytes.
6. **Kernel time and page faults.** git-ms: 45 s sys, 5.7M minor faults. The user-side causes are the allocations in finding 5 and 3.5 s in `_exit` releasing a 19 GB heap. A profile cannot show the page-fault time itself. Measure `sys` time and faults again after 4.1.
7. **Parallel pass 2 (4.1 part 4).** All runs use 1 to 1.5 cores of 128.
8. **Heap after pass 1.** About 320 bytes for each object on linux. `Tree`, `Commit`, and `Blob` are 128–152 bytes before any relationships are added, because of empty `Vec` and `HashSet` fields. Steps 5 and 6 remove most of these fields.
9. **`/mnt/d` limits.**
   - Each small file costs about 1.8 ms: 0.49 s of lage's 0.73 s is ref and reflog reads.

## Proposed goals for 4.1

These are proposals. Confirm them before the 4.1 evaluation.

| Repo | Baseline | Goal after 4.1 |
|---|---|---|
| linux (native) | stopped at 48 GB in pass 2 | completes; peak heap below 12 GB |
| git-ms (native) | 77 s, 19.3 GB | peak heap below 4 GB; phase 1 at most half of 60.6 s |
| vscode (native) | 38.5 s, 3.7 GB | peak heap below 1.5 GB; no slower |
| small and medium (hyperfine) | table above | no slower than 5% |

## How to repeat

1. Build the binary at the commit to measure:
   ```
   git worktree add --detach /tmp/gdp-eval <commit>
   cd /tmp/gdp-eval && CARGO_PROFILE_RELEASE_DEBUG=true CARGO_TARGET_DIR=/tmp/gdp-eval-target cargo build --release
   ```
2. One harness run (time, heap timeline, phase timings):
   ```
   python3 scripts/benchmarks/harness.py <label> <repo> <out.json> \
       --bin /tmp/gdp-eval-target/release/git-dag-parser --limit-gb 48 --timeout-s 3600
   ```
   It writes `<out>.json`, `<out>.stdout`, `<out>.stderr`, and `<out>.timeline.csv`. After the change that moved progress output to `tracing`, the phase timings come from the info logs, so the harness sets `RUST_LOG=info` by default (`HARNESS_LOG` changes it). The baseline runs used `RUST_LOG=warn`, when these lines were still printed with `println!`.
3. Small and medium repositories, with `hyperfine` 2.0 (installed 2026-10-28 with rustc 1.99.1):
   ```
   hyperfine --warmup 1 --runs 5 --env 'RUST_LOG=warn' \
       --metrics time_wall_clock,time_cpu,instructions,cpu_cycles,cache_misses,branch_misses,memory_peak_resident:MiB \
       --export-json <out>.json -n before "<old-bin> init --repo <repo>" -n after "<new-bin> init --repo <repo>"
   ```
   - Hardware counters (`instructions`, `cpu_cycles`, cache and branch misses) need `kernel.perf_event_paranoid` at 1 or lower. Instruction counts vary much less between runs than wall time, so they show the effect of a code change better, also on `/mnt/d`.
   - `memory_peak_resident` is peak RSS and includes mapped pack pages. Use the harness (`RssAnon`) for the heap.
   - Commands run without a shell by default in 2.0, so `-N` is no longer needed.
   - The 2.0 JSON format (schema v2) is different. The mean wall time is at `results[i].summary.time_wall_clock.mean`. The `*-hyperfine.json` files in this baseline use the 1.19 format (`results[i].mean`).
4. Git comparison for pass 1:
   ```
   time git -C <repo> cat-file --batch-all-objects --unordered --batch-check='%(objecttype) %(objectsize) %(objectsize:disk)' > /dev/null
   ```
5. CPU profile:
   1. Allow profiling until WSL restarts: `echo 1 | sudo tee /proc/sys/kernel/perf_event_paranoid` in a terminal, or `wsl.exe -u root -e sh -c 'echo 1 > /proc/sys/kernel/perf_event_paranoid'`.
   2. Build with frame pointers into a separate target directory:
      ```
      RUSTFLAGS="-C force-frame-pointers=yes" CARGO_PROFILE_RELEASE_DEBUG=true CARGO_TARGET_DIR=/tmp/gdp-prof-target cargo build --release
      ```
   3. Record, and stop long runs with SIGINT. `samply` saves the profile when it is interrupted.
      ```
      RUST_LOG=warn samply record --save-only --unstable-presymbolicate -o <out>.json.gz -- /tmp/gdp-prof-target/release/git-dag-parser init --repo <repo>
      RUST_LOG=warn timeout -s INT 130 samply record ...   # linux
      ```
   4. Summarize, or open it in the browser:
      ```
      python3 scripts/benchmarks/hotspots.py <out>.json.gz --main-only --top 25 --callers _memcpy_avx_unaligned_erms
      samply load <out>.json.gz
      ```
6. Do not write to the repositories under `/mnt/d`. Use `GIT_OPTIONAL_LOCKS=0` for git commands there.

## Data

The raw results (one JSON file per harness run, `*.timeline.csv` heap timelines, `*-hyperfine.json` exports) and the CPU profiles for zed, vscode, git-ms, and linux are not in this repository. They are too large, and they contain local paths.
