//! The read order of pass 2 and the delta cache requests that it makes.
//!
//! Pass 1 gives the base of each delta in a `DeltaGraph`, so the requests
//! for each entry of a pack are known before pass 2 reads the pack. A cached
//! entry is needed only until its last request.

use std::collections::BinaryHeap;

use anyhow::{anyhow, Result};
use hashbrown::{HashMap, HashSet};

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

/// Call `f(c, p)` for each request: `c` is the requested entry, and `p` is
/// the read that makes the request. A read requests its entry, and the
/// first build of an entry requests its base. In one read an entry gets at
/// most one request. `first_build(c)` marks `c` as built and is true only
/// the first time, so that a pack-wide array or a map for a unit can keep
/// the marks.
fn for_each_request(
    graph: &DeltaGraph,
    reads: impl Iterator<Item = usize>,
    mut first_build: impl FnMut(usize) -> bool,
    mut f: impl FnMut(usize, usize),
) {
    for p in reads {
        f(p, p);
        let mut c = p;
        // Only the first build of an entry requests its base
        while first_build(c) {
            match graph.base(c) {
                None => break,
                Some(b) => {
                    f(b, p);
                    c = b;
                }
            }
        }
    }
}

/// Number of requests for each position: one for each read, and one to the
/// base of each entry for its first build
pub fn count_requests(graph: &DeltaGraph, reads: impl Iterator<Item = usize>) -> Vec<u32> {
    let mut requests = vec![0u32; graph.len()];
    let mut built = vec![false; graph.len()];
    for_each_request(
        graph,
        reads,
        |c| !std::mem::replace(&mut built[c], true),
        |c, _| requests[c] += 1,
    );
    requests
}

/// The requests of one position: see `RequestTimes`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Requests {
    /// The times of the requests are `times[end - count..end]`
    pub end: u32,
    pub count: u32,
}

/// The requests of a set of reads and their times. The time of a request
/// is the read position `p` that makes it, so the times of a position are
/// in ascending order and no two are equal. Only the positions that the
/// reads request are in `requests`, so a work unit does not need arrays as
/// large as its pack.
pub struct RequestTimes {
    pub requests: HashMap<u32, Requests>,
    pub times: Vec<u32>,
}

/// The requests and their times, in two passes over `reads`: the first
/// counts, the second writes the times. When `left` requests of `c` are
/// still to come, the next one is `times[end - left]`.
pub fn request_times(
    graph: &DeltaGraph,
    reads: impl Iterator<Item = usize> + Clone,
) -> RequestTimes {
    let mut requests: HashMap<u32, Requests> = HashMap::new();
    let mut built: HashSet<u32> = HashSet::new();
    for_each_request(
        graph,
        reads.clone(),
        |c| built.insert(c as u32),
        |c, _| {
            requests
                .entry(c as u32)
                .or_insert(Requests { end: 0, count: 0 })
                .count += 1
        },
    );

    // Lay the times of each position out one after the other. While the
    // times are written, `end` is where the next one goes.
    let mut total = 0u64;
    for r in requests.values_mut() {
        r.end = u32::try_from(total).expect("request count fits in u32");
        total += u64::from(r.count);
    }
    let mut times = vec![0u32; total as usize];
    built.clear();
    for_each_request(
        graph,
        reads,
        |c| built.insert(c as u32),
        |c, p| {
            let r = requests
                .get_mut(&(c as u32))
                .expect("counted in the first pass");
            times[r.end as usize] = u32::try_from(p).expect("read position fits in u32");
            r.end += 1;
        },
    );

    RequestTimes { requests, times }
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

/// Counts kept by a `DeltaCache`
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct DeltaCacheStats {
    pub hits: u64,
    pub misses: u64,
    /// Entries removed to make room for another
    pub evictions: u64,
    /// Entries not inserted because they are larger than the limit
    pub too_large: u64,
    /// Entries not inserted because every cached entry is requested sooner
    pub bypassed: u64,
    pub peak_bytes: u64,
    pub peak_entries: usize,
    /// Entries in the cache now
    pub left: usize,
}

impl DeltaCacheStats {
    /// Add the counts of another cache, such as another unit of the same
    /// pack. The peaks are the larger of the two: the units of one worker
    /// do not hold entries at the same time.
    pub fn add(&mut self, other: &DeltaCacheStats) {
        self.hits += other.hits;
        self.misses += other.misses;
        self.evictions += other.evictions;
        self.too_large += other.too_large;
        self.bypassed += other.bypassed;
        self.peak_bytes = self.peak_bytes.max(other.peak_bytes);
        self.peak_entries = self.peak_entries.max(other.peak_entries);
        self.left += other.left;
    }
}

/// The resolved entries of one pack that pass 2 needs again, by position,
/// within a byte limit.
///
/// Each entry is kept with the time of its next request. When the cache is
/// full, the entries whose next requests are farthest away go first; if
/// the new entry is the farthest, it is not cached and nothing goes. `heap`
/// orders the entries for that rule. An item whose time is not the entry's
/// current time, or whose entry is gone, is old and is skipped.
#[derive(Debug)]
pub struct DeltaCache {
    /// The bytes of each entry and the time of its next request
    entries: HashMap<u32, (Vec<u8>, u32)>,
    /// Sum of `len()` of all entries
    bytes: u64,
    limit: u64,
    /// (next request, position) of each entry, newest time last pushed
    heap: BinaryHeap<(u32, u32)>,
    pub stats: DeltaCacheStats,
}

impl DeltaCache {
    /// An empty cache that holds at most `limit` bytes
    pub fn new(limit: u64) -> Self {
        Self {
            entries: HashMap::new(),
            bytes: 0,
            limit,
            heap: BinaryHeap::new(),
            stats: DeltaCacheStats::default(),
        }
    }

    /// The bytes of the entry at `pos`, if it is cached
    pub fn get(&self, pos: u32) -> Option<&[u8]> {
        self.entries.get(&pos).map(|(bytes, _)| bytes.as_slice())
    }

    /// Remove the entry at `pos` and give its bytes to the caller. Its heap
    /// items become stale.
    pub fn take(&mut self, pos: u32) -> Option<Vec<u8>> {
        let (bytes, _) = self.entries.remove(&pos)?;
        self.bytes -= bytes.len() as u64;
        self.stats.left = self.entries.len();
        Some(bytes)
    }

    /// Cache `bytes` as the entry at `pos`, next requested at time `next`.
    /// Makes room by evicting the entries requested farthest away, as long
    /// as they are requested later than `next`. Gives `bytes` back if they
    /// are not cached: too large for the cache, or requested no sooner than
    /// every cached entry that would have to go.
    pub fn insert(&mut self, pos: u32, bytes: Vec<u8>, next: u32) -> Result<(), Vec<u8>> {
        let len = bytes.len() as u64;
        if len > self.limit {
            self.stats.too_large += 1;
            return Err(bytes);
        }
        if let Some((old, _)) = self.entries.remove(&pos) {
            self.bytes -= old.len() as u64;
        }
        if self.bytes + len > self.limit && !self.make_room(self.bytes + len - self.limit, next) {
            return Err(bytes);
        }
        self.bytes += len;
        self.entries.insert(pos, (bytes, next));
        self.heap.push((next, pos));
        self.compact_heap();
        self.stats.left = self.entries.len();
        self.stats.peak_bytes = self.stats.peak_bytes.max(self.bytes);
        self.stats.peak_entries = self.stats.peak_entries.max(self.entries.len());
        Ok(())
    }

    /// Free `need` bytes for an entry next requested at `next`, farthest
    /// next request first. No entry goes until enough entries requested
    /// later than `next` are found; otherwise every entry stays and the
    /// result is false.
    fn make_room(&mut self, need: u64, next: u32) -> bool {
        let mut victims = Vec::new();
        let mut freed = 0u64;
        while freed < need {
            let (t, p) = self.pop_current();
            if t <= next {
                // p is needed no later than the new entry: keep them all
                self.heap.push((t, p));
                self.heap.extend(victims);
                self.stats.bypassed += 1;
                return false;
            }
            freed += self.entries[&p].0.len() as u64;
            victims.push((t, p));
        }
        for (_, p) in victims {
            let (evicted, _) = self.entries.remove(&p).unwrap();
            self.bytes -= evicted.len() as u64;
            self.stats.evictions += 1;
        }
        true
    }

    /// Pop the item with the farthest next request that is not old
    fn pop_current(&mut self) -> (u32, u32) {
        loop {
            let (t, p) = self
                .heap
                .pop()
                .expect("every cached entry has a current heap item");
            if self
                .entries
                .get(&p)
                .is_some_and(|&(_, current)| current == t)
            {
                return (t, p);
            }
        }
    }

    /// Make the heap again from the entries when old items are most of it
    fn compact_heap(&mut self) {
        if self.heap.len() > 2 * self.entries.len() {
            self.heap = self
                .entries
                .iter()
                .map(|(&p, &(_, next))| (next, p))
                .collect();
        }
    }

    /// Set the time of the next request of the entry at `pos`, which must
    /// be cached. Its earlier heap item becomes stale.
    pub fn set_next(&mut self, pos: u32, next: u32) {
        let entry = self
            .entries
            .get_mut(&pos)
            .expect("set_next requires a cached entry");
        entry.1 = next;
        self.heap.push((next, pos));
        self.compact_heap();
    }
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

    fn cache(limit: u64) -> DeltaCache {
        DeltaCache::new(limit)
    }

    fn bytes(n: usize) -> Vec<u8> {
        vec![b'x'; n]
    }

    #[test]
    fn insert_then_get_and_take() {
        let mut c = cache(100);
        assert_eq!(c.insert(3, b"abc".to_vec(), 10), Ok(()));

        assert_eq!(c.get(3), Some(&b"abc"[..]));
        assert_eq!(c.get(4), None);
        assert_eq!(c.stats.left, 1);

        assert_eq!(c.take(3), Some(b"abc".to_vec()));
        assert_eq!(c.get(3), None);
        assert_eq!(c.take(3), None);
        assert_eq!(c.bytes, 0);
        assert_eq!(c.stats.left, 0);
    }

    #[test]
    fn an_entry_larger_than_the_limit_is_given_back() {
        let mut c = cache(10);
        assert_eq!(c.insert(0, bytes(11), 1), Err(bytes(11)));
        assert_eq!(c.stats.too_large, 1);
        assert_eq!(c.stats.evictions, 0);
        assert_eq!(c.get(0), None);
    }

    #[test]
    fn the_entry_requested_farthest_away_is_evicted_first() {
        let mut c = cache(10);
        c.insert(A as u32, bytes(4), 30).unwrap();
        c.insert(B as u32, bytes(4), 10).unwrap();
        // Needs 2 more bytes; A is requested later than B and the new entry
        c.insert(C as u32, bytes(4), 20).unwrap();

        assert_eq!(c.get(A as u32), None);
        assert!(c.get(B as u32).is_some());
        assert!(c.get(C as u32).is_some());
        assert_eq!(c.bytes, 8);
        assert_eq!(c.stats.evictions, 1);
        assert_eq!(c.stats.bypassed, 0);
    }

    #[test]
    fn eviction_continues_until_there_is_room() {
        let mut c = cache(10);
        c.insert(A as u32, bytes(3), 30).unwrap();
        c.insert(B as u32, bytes(3), 20).unwrap();
        c.insert(C as u32, bytes(3), 10).unwrap();
        c.insert(D as u32, bytes(7), 5).unwrap();

        assert_eq!(c.get(A as u32), None);
        assert_eq!(c.get(B as u32), None);
        assert!(c.get(C as u32).is_some());
        assert!(c.get(D as u32).is_some());
        assert_eq!(c.stats.evictions, 2);
    }

    #[test]
    fn a_new_entry_requested_no_sooner_than_the_cached_ones_is_bypassed() {
        let mut c = cache(10);
        c.insert(A as u32, bytes(5), 10).unwrap();
        c.insert(B as u32, bytes(5), 20).unwrap();

        assert_eq!(c.insert(C as u32, bytes(1), 30), Err(bytes(1)));
        assert_eq!(c.insert(D as u32, bytes(1), 20), Err(bytes(1)), "a tie");

        assert_eq!(c.stats.bypassed, 2);
        assert_eq!(c.stats.evictions, 0);
        assert!(c.get(A as u32).is_some());
        assert!(c.get(B as u32).is_some());
        assert_eq!(c.bytes, 10);
    }

    #[test]
    fn set_next_changes_the_eviction_order() {
        let mut c = cache(10);
        c.insert(A as u32, bytes(5), 30).unwrap();
        c.insert(B as u32, bytes(5), 10).unwrap();
        // A is now requested soonest; its (30, A) heap item is stale
        c.set_next(A as u32, 5);

        // Without the change, A would go; with it, B is the farthest
        c.insert(C as u32, bytes(5), 8).unwrap();

        assert!(c.get(A as u32).is_some());
        assert_eq!(c.get(B as u32), None);
        assert!(c.get(C as u32).is_some());
        assert_eq!(c.stats.evictions, 1);
    }

    #[test]
    fn a_taken_entry_leaves_only_stale_heap_items() {
        let mut c = cache(10);
        c.insert(A as u32, bytes(5), 30).unwrap();
        c.insert(B as u32, bytes(5), 20).unwrap();
        c.take(A as u32);
        c.insert(A as u32, bytes(5), 1).unwrap();

        // Room is needed: the stale (30, A) item is skipped, not evicted
        c.insert(C as u32, bytes(5), 10).unwrap();

        assert!(c.get(A as u32).is_some());
        assert_eq!(c.get(B as u32), None);
        assert!(c.get(C as u32).is_some());
        assert_eq!(c.stats.evictions, 1);
    }

    #[test]
    fn inserting_at_a_cached_position_replaces_the_entry() {
        let mut c = cache(10);
        c.insert(A as u32, bytes(6), 10).unwrap();
        c.insert(A as u32, bytes(8), 10).unwrap();

        assert_eq!(c.get(A as u32).map(<[u8]>::len), Some(8));
        assert_eq!(c.bytes, 8);
        assert_eq!(c.stats.left, 1);
        assert_eq!(c.stats.evictions, 0);
    }

    #[test]
    fn peaks_record_the_most_held_at_one_time() {
        let mut c = cache(10);
        c.insert(A as u32, bytes(4), 10).unwrap();
        c.insert(B as u32, bytes(4), 20).unwrap();
        c.take(A as u32);
        c.take(B as u32);
        c.insert(C as u32, bytes(1), 30).unwrap();

        assert_eq!(c.stats.peak_bytes, 8);
        assert_eq!(c.stats.peak_entries, 2);
        assert_eq!(c.stats.left, 1);
    }

    #[test]
    #[should_panic(expected = "requires a cached entry")]
    fn set_next_panics_for_an_entry_that_is_not_cached() {
        let mut c = cache(10);
        c.set_next(A as u32, 1);
    }

    /// The times of each position. Also checks that `requests` is the
    /// result of `count_requests` and gives the number of times.
    fn times(bases: &[u32], reads: &[usize]) -> Vec<Vec<u32>> {
        let graph = DeltaGraph::from_bases(bases.to_vec());
        let RequestTimes { requests, times } = request_times(&graph, reads.iter().copied());
        let counts = count_requests(&graph, reads.iter().copied());
        (0..graph.len())
            .map(|c| match requests.get(&(c as u32)) {
                Some(r) => {
                    assert_eq!(r.count, counts[c], "position {c}");
                    times[(r.end - r.count) as usize..r.end as usize].to_vec()
                }
                None => {
                    assert_eq!(counts[c], 0, "position {c} is requested");
                    Vec::new()
                }
            })
            .collect()
    }

    #[test]
    fn request_times_keep_only_the_positions_that_are_requested() {
        // A, B ← C, and a read of C only: A is not requested
        let RequestTimes { requests, times } =
            request_times(&DeltaGraph::from_bases(vec![NO, NO, 1]), [C].into_iter());
        let mut keys: Vec<u32> = requests.keys().copied().collect();
        keys.sort_unstable();
        assert_eq!(keys, vec![B as u32, C as u32]);
        assert_eq!(times.len(), 2);
    }

    #[test]
    fn stats_add_sums_the_counts_and_keeps_the_larger_peaks() {
        let mut a = DeltaCacheStats {
            hits: 1,
            misses: 2,
            evictions: 3,
            too_large: 4,
            bypassed: 5,
            peak_bytes: 100,
            peak_entries: 7,
            left: 0,
        };
        a.add(&DeltaCacheStats {
            hits: 10,
            misses: 20,
            evictions: 30,
            too_large: 40,
            bypassed: 50,
            peak_bytes: 60,
            peak_entries: 9,
            left: 1,
        });
        assert_eq!(
            a,
            DeltaCacheStats {
                hits: 11,
                misses: 22,
                evictions: 33,
                too_large: 44,
                bypassed: 55,
                peak_bytes: 100,
                peak_entries: 9,
                left: 1,
            }
        );
    }

    #[test]
    fn request_times_gives_each_request_the_position_of_its_read() {
        // A ← B; reads A, B: the read of B also requests A for its first build
        assert_eq!(times(&[NO, 0], &[A, B]), vec![vec![0, 1], vec![1]]);
    }

    #[test]
    fn request_times_gives_a_shared_base_a_time_for_each_delta() {
        // A ← B, A ← C; reads A, B, C: the time of a request is the read
        assert_eq!(
            times(&[NO, 0, 0], &[A, B, C]),
            vec![vec![0, 1, 2], vec![1], vec![2]]
        );
    }

    #[test]
    fn request_times_of_a_ref_delta_to_a_later_base() {
        // B at position 0 is a REF delta to A at position 1; reads B, A:
        // the read of B requests A before the read of A
        assert_eq!(times(&[1, NO], &[0, 1]), vec![vec![0], vec![0, 1]]);
    }

    #[test]
    fn request_times_of_a_chain_share_the_time_of_the_read() {
        // A ← B ← C, B ← D, with only C and D read: A and B get the time of
        // C, the read that first builds them; the read of D adds a request
        // to B at D's time
        assert_eq!(
            times(&[NO, 0, 1, 1], &[C, D]),
            vec![vec![2], vec![2, 3], vec![2], vec![3]]
        );
    }

    #[test]
    fn request_times_gives_a_position_that_is_not_read_no_times() {
        // A ← B, and B is a blob, so `pack_reads` does not give it
        assert_eq!(times(&[NO, 0], &[]), vec![Vec::<u32>::new(), vec![]]);
    }

    #[test]
    fn request_times_gives_an_entry_that_is_not_read_and_not_a_base_no_times() {
        assert_eq!(times(&[NO, NO], &[B]), vec![vec![], vec![1]]);
    }

    #[test]
    fn request_times_are_ascending_and_distinct_for_each_position() {
        // A ← B, A ← C, B ← D; every position read
        for (c, own) in times(&[NO, 0, 0, 1], &[A, B, C, D]).iter().enumerate() {
            assert!(own.windows(2).all(|w| w[0] < w[1]), "position {c}: {own:?}");
        }
    }

    #[test]
    fn insert_evicts_nothing_when_the_new_entry_would_not_be_cached() {
        // X (next 100) and Y (next 20) fill the cache. N (next 50) needs
        // both to go, but Y is needed sooner than N: X must stay too.
        let mut c = cache(20);
        c.insert(A as u32, bytes(10), 100).unwrap();
        c.insert(B as u32, bytes(10), 20).unwrap();

        assert_eq!(c.insert(C as u32, bytes(20), 50), Err(bytes(20)));

        assert!(c.get(A as u32).is_some(), "X was not evicted for nothing");
        assert!(c.get(B as u32).is_some());
        assert_eq!(c.stats.evictions, 0);
        assert_eq!(c.stats.bypassed, 1);
        // The heap still orders both for the next insert
        c.insert(D as u32, bytes(10), 10).unwrap();
        assert_eq!(c.get(A as u32), None, "X, the farthest, goes for D");
        assert!(c.get(B as u32).is_some());
        assert_eq!(c.stats.evictions, 1);
    }

    #[test]
    fn rule_b_evicts_the_entry_with_the_farthest_next_request() {
        let mut c = cache(30);
        c.insert(A as u32, bytes(10), 5).unwrap();
        c.insert(B as u32, bytes(10), 9).unwrap();
        c.insert(C as u32, bytes(10), 7).unwrap();

        c.insert(D as u32, bytes(10), 6).unwrap();

        assert_eq!(c.get(B as u32), None);
        assert!(c.get(A as u32).is_some());
        assert!(c.get(C as u32).is_some());
        assert!(c.get(D as u32).is_some());
        assert_eq!(c.stats.evictions, 1);
        assert_eq!(c.stats.bypassed, 0);
    }

    #[test]
    fn rule_b_caches_a_new_entry_that_is_not_the_farthest() {
        // X (next 100) and Y (next 20); N (next 50) needs only X to go
        let mut c = cache(20);
        c.insert(A as u32, bytes(10), 100).unwrap();
        c.insert(B as u32, bytes(10), 20).unwrap();

        c.insert(C as u32, bytes(10), 50).unwrap();

        assert_eq!(c.get(A as u32), None);
        assert!(c.get(B as u32).is_some());
        assert!(c.get(C as u32).is_some());
        assert_eq!(c.stats.evictions, 1);
        assert_eq!(c.stats.bypassed, 0);
    }

    #[test]
    fn rule_b_keeps_the_cached_entry_for_equal_times() {
        let mut c = cache(20);
        c.insert(A as u32, bytes(20), 30).unwrap();

        assert_eq!(c.insert(B as u32, bytes(10), 30), Err(bytes(10)));

        assert!(c.get(A as u32).is_some());
        assert_eq!(c.stats.evictions, 0);
        assert_eq!(c.stats.bypassed, 1);
    }

    #[test]
    fn rule_b_uses_the_time_from_set_next() {
        // P (next 3) and Q (next 10); after set_next P is the farthest, and
        // the old item (3, P) is skipped
        let mut c = cache(10);
        c.insert(A as u32, bytes(5), 3).unwrap();
        c.insert(B as u32, bytes(5), 10).unwrap();
        c.set_next(A as u32, 20);

        c.insert(C as u32, bytes(5), 1).unwrap();

        assert_eq!(c.get(A as u32), None);
        assert!(c.get(B as u32).is_some());
        assert_eq!(c.stats.evictions, 1);
    }

    #[test]
    fn rule_b_after_take_evicts_the_farthest_entry_that_is_left() {
        let mut c = cache(10);
        c.insert(A as u32, bytes(5), 30).unwrap();
        c.insert(B as u32, bytes(5), 20).unwrap();
        c.take(A as u32);
        c.insert(C as u32, bytes(5), 10).unwrap();

        c.insert(D as u32, bytes(5), 5).unwrap();

        assert_eq!(c.get(B as u32), None);
        assert!(c.get(C as u32).is_some());
        assert!(c.get(D as u32).is_some());
        assert_eq!(c.stats.evictions, 1);
    }

    #[test]
    fn equal_next_requests_evict_the_larger_position_first() {
        let mut c = cache(10);
        c.insert(A as u32, bytes(5), 30).unwrap();
        c.insert(B as u32, bytes(5), 30).unwrap();

        c.insert(C as u32, bytes(5), 10).unwrap();

        assert!(c.get(A as u32).is_some());
        assert_eq!(c.get(B as u32), None);
    }

    #[test]
    fn the_heap_stays_at_most_twice_the_entry_count() {
        let mut c = cache(100);
        c.insert(A as u32, bytes(1), 1).unwrap();
        c.insert(B as u32, bytes(1), 2).unwrap();
        for next in 3..1000 {
            c.set_next(A as u32, next);
            assert!(c.heap.len() <= 2 * c.entries.len(), "heap {}", c.heap.len());
        }
        // The current items survive compaction: A (999) goes before B (2)
        c.insert(C as u32, bytes(99), 500).unwrap();
        assert_eq!(c.get(A as u32), None);
        assert!(c.get(B as u32).is_some());
        assert!(c.get(C as u32).is_some());
    }
}
