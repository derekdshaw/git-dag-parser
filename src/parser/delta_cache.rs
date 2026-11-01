//! The read order of pass 2 and the delta cache requests that it makes.
//!
//! Pass 1 gives the base of each delta in a `DeltaGraph`, so the requests
//! for each entry of a pack are known before pass 2 reads the pack. A cached
//! entry is needed only until its last request.

use anyhow::{anyhow, Result};

use super::pack::{DeltaGraph, MAX_DELTA_DEPTH};
use crate::models::{ObjectCopy, ObjectKind};

/// Positions that pass 2 reads in one pack: the primary copies that are not
/// blobs, in ascending order. `copies` is the part of `container.copies`
/// for the pack.
pub fn pack_reads(copies: &[ObjectCopy]) -> impl Iterator<Item = usize> + Clone + '_ {
    copies
        .iter()
        .enumerate()
        .filter(|(_, copy)| copy.is_primary && copy.object().kind() != ObjectKind::Blob)
        .map(|(pos, _)| pos)
}

/// Number of requests for each position: one for each read, and one to the
/// base of each entry for its first build
pub fn count_requests(graph: &DeltaGraph, reads: impl Iterator<Item = usize>) -> Vec<u32> {
    let mut requests = vec![0u32; graph.len()];
    let mut needed = vec![false; graph.len()];
    for p in reads {
        requests[p] += 1;
        let mut c = p;
        // Only the first build of an entry requests its base
        while !needed[c] {
            needed[c] = true;
            match graph.base(c) {
                None => break,
                Some(b) => {
                    requests[b] += 1;
                    c = b;
                }
            }
        }
    }
    requests
}

/// The result of `simulate_cache` for one pack
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CacheSimulation {
    /// Total number of requests
    pub requests: u64,
    pub peak_bytes: u64,
    pub peak_entries: usize,
    /// Entries in the cache after the last read; 0 if the counts are correct
    pub left: usize,
}

/// State of one position in `simulate_cache`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum State {
    NotBuilt,
    Live,
    /// Built and not live. With no byte limit, no request comes to it again.
    Done,
}

/// Run the delta cache of pass 2 with no byte limit, without inflating
/// anything. `size(pos)` is the size of the object at `pos`.
///
/// The steps are in the same order as in the pass 2 cache, so that with no
/// eviction its peaks are equal to the simulated peaks.
pub fn simulate_cache(
    graph: &DeltaGraph,
    reads: impl Iterator<Item = usize> + Clone,
    size: impl Fn(usize) -> u64,
) -> Result<CacheSimulation> {
    let mut requests = count_requests(graph, reads.clone());
    let total = requests.iter().map(|&r| r as u64).sum();
    let mut state = vec![State::NotBuilt; graph.len()];
    let mut stack = Vec::new();
    let (mut bytes, mut entries) = (0u64, 0usize);
    let (mut peak_bytes, mut peak_entries) = (0u64, 0usize);

    for p in reads {
        // Go up the chain to an entry that is built
        stack.clear();
        let mut c = Some(p);
        while let Some(x) = c {
            if state[x] != State::NotBuilt {
                break;
            }
            stack.push(x);
            if stack.len() > MAX_DELTA_DEPTH {
                return Err(anyhow!(
                    "Delta chain too deep (>{}) at position {}",
                    MAX_DELTA_DEPTH,
                    p
                ));
            }
            c = graph.base(x);
        }

        // The built entry is live: one request for it
        if let Some(x) = c {
            debug_assert!(state[x] == State::Live && requests[x] > 0);
            requests[x] -= 1;
            if requests[x] == 0 {
                state[x] = State::Done;
                bytes -= size(x);
                entries -= 1;
            }
        }

        // Build, deepest base first
        while let Some(x) = stack.pop() {
            requests[x] -= 1;
            if requests[x] > 0 {
                state[x] = State::Live;
                bytes += size(x);
                entries += 1;
                peak_bytes = peak_bytes.max(bytes);
                peak_entries = peak_entries.max(entries);
            } else {
                state[x] = State::Done;
            }
        }
    }

    Ok(CacheSimulation {
        requests: total,
        peak_bytes,
        peak_entries,
        left: entries,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::ObjectId;

    const NO: u32 = DeltaGraph::NO_BASE;

    // Positions of the entries in the tests
    const A: usize = 0;
    const B: usize = 1;
    const C: usize = 2;
    const D: usize = 3;

    fn requests(bases: &[u32], reads: &[usize]) -> Vec<u32> {
        count_requests(
            &DeltaGraph::from_bases(bases.to_vec()),
            reads.iter().copied(),
        )
    }

    #[test]
    fn a_read_of_an_entry_that_is_not_a_delta_is_one_request() {
        assert_eq!(requests(&[NO], &[A]), vec![1]);
    }

    #[test]
    fn a_read_of_a_chain_requests_each_base_one_time() {
        // A ← B ← C
        assert_eq!(requests(&[NO, 0, 1], &[C]), vec![1, 1, 1]);
    }

    #[test]
    fn a_shared_base_gets_one_request_from_each_delta() {
        // A ← B, A ← C
        assert_eq!(requests(&[NO, 0, 0], &[B, C]), vec![2, 1, 1]);
    }

    #[test]
    fn a_base_that_is_also_read_gets_both_requests() {
        // A ← B
        assert_eq!(requests(&[NO, 0], &[A, B]), vec![2, 1]);
    }

    #[test]
    fn a_ref_delta_to_a_later_base_requests_the_base() {
        // B at position 0 is a delta on A at position 1
        let (a, b) = (1, 0);
        let counts = requests(&[1, NO], &[b, a]);
        assert_eq!((counts[a], counts[b]), (2, 1));
    }

    #[test]
    fn a_base_built_for_one_delta_is_not_built_again_for_another() {
        // A ← B ← C, B ← D: the build of D requests B but not A
        assert_eq!(requests(&[NO, 0, 1, 1], &[C, D]), vec![1, 2, 1, 1]);
    }

    #[test]
    fn an_entry_that_is_not_read_gets_no_request() {
        // A ← B, and both are blobs
        let copies = [copy(ObjectId::Blob(0), true), copy(ObjectId::Blob(1), true)];
        let graph = DeltaGraph::from_bases(vec![NO, 0]);
        assert_eq!(count_requests(&graph, pack_reads(&copies)), vec![0, 0]);
    }

    fn copy(object: ObjectId, primary: bool) -> ObjectCopy {
        let mut copy = ObjectCopy::packed(object, 0, 12, 10, false);
        copy.is_primary = primary;
        copy
    }

    #[test]
    fn pack_reads_gives_the_primary_copies_that_are_not_blobs() {
        let copies = [
            copy(ObjectId::Blob(0), true),
            copy(ObjectId::Commit(0), true),
            copy(ObjectId::Tree(0), false),
            copy(ObjectId::Tree(1), true),
            copy(ObjectId::Blob(1), false),
            copy(ObjectId::Tag(0), true),
            copy(ObjectId::Commit(1), false),
        ];
        assert_eq!(pack_reads(&copies).collect::<Vec<_>>(), vec![1, 3, 5]);
    }

    fn simulate(bases: &[u32], reads: &[usize]) -> CacheSimulation {
        const SIZES: [u64; 4] = [100, 50, 70, 30];
        simulate_cache(
            &DeltaGraph::from_bases(bases.to_vec()),
            reads.iter().copied(),
            |pos| SIZES[pos],
        )
        .unwrap()
    }

    #[test]
    fn simulation_keeps_a_shared_base_until_its_last_request() {
        // A, A ← B, A ← C
        let sim = simulate(&[NO, 0, 0], &[A, B, C]);
        assert_eq!(
            sim,
            CacheSimulation {
                requests: 5,
                peak_bytes: 100,
                peak_entries: 1,
                left: 0
            }
        );
    }

    #[test]
    fn simulation_keeps_nothing_for_a_chain_read_one_time() {
        // A ← B ← C
        let sim = simulate(&[NO, 0, 1], &[C]);
        assert_eq!(
            sim,
            CacheSimulation {
                requests: 3,
                peak_bytes: 0,
                peak_entries: 0,
                left: 0
            }
        );
    }

    #[test]
    fn simulation_peak_counts_bases_that_are_live_at_one_time() {
        // A, B, A ← C, B ← D
        let sim = simulate(&[NO, NO, 0, 1], &[A, B, C, D]);
        assert_eq!(
            sim,
            CacheSimulation {
                requests: 6,
                peak_bytes: 150,
                peak_entries: 2,
                left: 0
            }
        );
    }

    #[test]
    fn simulation_rejects_a_chain_that_is_too_deep() {
        // Each entry is a delta on the entry before it
        let n = MAX_DELTA_DEPTH + 2;
        let bases = std::iter::once(NO).chain(0..n as u32 - 1).collect();
        let err = simulate_cache(
            &DeltaGraph::from_bases(bases),
            std::iter::once(n - 1),
            |_| 1,
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("too deep"),
            "unexpected error: {err}"
        );
    }
}
