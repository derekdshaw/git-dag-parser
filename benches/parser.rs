//! Benchmark runner. The benchmarks are defined next to the code they
//! measure, in `#[cfg(feature = "bench")] mod benches` blocks in the library.
//!
//! Run with `cargo bench --features bench`.

// Links the library so its registered benchmarks are found.
use git_dag_parser as _;

// Counts allocations, reallocations and bytes for each benchmark iteration.
#[global_allocator]
static ALLOC: divan::AllocProfiler = divan::AllocProfiler::system();

fn main() {
    divan::main();
}
