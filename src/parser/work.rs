//! The units of work of pass 2.
//!
//! Each delta has one base, so the deltas of a pack form a forest: a
//! family is a root (an entry that is not a delta) and all the deltas built
//! on it, directly or through other deltas. A worker that reads whole
//! families never needs a base that another worker built, so no base is
//! built twice. Families are small (the largest linux family has 0.14% of
//! the reads), so they can be grouped into units of about equal size.

use super::pack::DeltaGraph;

/// The root of the family of each position: the position at the end of its
/// base chain. A position that is not a delta is its own root. Pass 1
/// rejects a cycle of bases, so each chain ends.
pub fn family_roots(graph: &DeltaGraph) -> Vec<u32> {
    // NO_BASE marks a root that is not known yet; no position has that value
    const UNKNOWN: u32 = DeltaGraph::NO_BASE;
    let mut root = vec![UNKNOWN; graph.len()];
    let mut chain = Vec::new();
    for pos in 0..graph.len() {
        // Up the chain to a position with a known root, or to the root
        let mut x = pos;
        while root[x] == UNKNOWN {
            match graph.base(x) {
                Some(b) => {
                    chain.push(x);
                    x = b;
                }
                None => root[x] = x as u32,
            }
        }
        let r = root[x];
        for y in chain.drain(..) {
            root[y] = r;
        }
    }
    root
}

/// The reads of a pack, grouped into units of whole families. A unit takes
/// families, in the order of their first read, until it has `unit_reads`
/// reads or more; a family is never split, so a unit can have more. The
/// reads of each unit are in ascending position order, so a worker reads
/// its part of the pack from the start to the end.
pub fn work_units(
    graph: &DeltaGraph,
    reads: impl Iterator<Item = usize> + Clone,
    unit_reads: usize,
) -> Vec<Vec<u32>> {
    const NONE: u32 = u32::MAX;
    let root = family_roots(graph);
    // Reads of each family, at the position of its root
    let mut family_reads = vec![0u32; graph.len()];
    for p in reads.clone() {
        family_reads[root[p] as usize] += 1;
    }
    // The unit of each family, at the position of its root
    let mut unit_of = vec![NONE; graph.len()];
    let mut units: Vec<Vec<u32>> = Vec::new();
    let mut filled = 0usize;
    for p in reads {
        let r = root[p] as usize;
        if unit_of[r] == NONE {
            if units.is_empty() || filled >= unit_reads {
                units.push(Vec::new());
                filled = 0;
            }
            unit_of[r] = (units.len() - 1) as u32;
            filled += family_reads[r] as usize;
        }
        units[unit_of[r] as usize].push(p as u32);
    }
    units
}

#[cfg(test)]
mod tests {
    use super::*;

    const NO: u32 = DeltaGraph::NO_BASE;

    fn graph(bases: &[u32]) -> DeltaGraph {
        DeltaGraph::from_bases(bases.to_vec())
    }

    fn units(bases: &[u32], reads: &[usize], unit_reads: usize) -> Vec<Vec<u32>> {
        work_units(&graph(bases), reads.iter().copied(), unit_reads)
    }

    #[test]
    fn family_roots_give_each_position_the_end_of_its_chain() {
        // 0 ← 1 ← 2, 3 ← 4, 0 ← 5
        assert_eq!(
            family_roots(&graph(&[NO, 0, 1, NO, 3, 0])),
            vec![0, 0, 0, 3, 3, 0]
        );
    }

    #[test]
    fn family_roots_follow_a_ref_delta_to_a_later_base() {
        assert_eq!(family_roots(&graph(&[1, NO])), vec![1, 1]);
    }

    #[test]
    fn work_units_never_split_a_family() {
        // Families {0, 1, 2} and {3, 4}; one read per unit is the target
        let u = units(&[NO, 0, 1, NO, 3], &[0, 1, 2, 3, 4], 1);
        assert_eq!(u, vec![vec![0, 1, 2], vec![3, 4]]);
    }

    #[test]
    fn work_units_fill_to_the_target_read_count() {
        // Seven families of one read each
        let u = units(&[NO; 7], &[0, 1, 2, 3, 4, 5, 6], 3);
        assert_eq!(u, vec![vec![0, 1, 2], vec![3, 4, 5], vec![6]]);
    }

    #[test]
    fn work_units_take_families_in_the_order_of_their_first_read() {
        // Families {0, 2, 4} and {1, 3}: the second unit starts at read 1,
        // and each unit keeps its reads in ascending order
        let u = units(&[NO, NO, 0, 1, 2], &[0, 1, 2, 3, 4], 2);
        assert_eq!(u, vec![vec![0, 2, 4], vec![1, 3]]);
    }

    #[test]
    fn work_units_keep_a_family_whose_root_is_not_read() {
        // 0 is a base that is not read (a copy that is not primary); its
        // deltas 1 and 2 are still one family
        let u = units(&[NO, 0, 0, NO], &[1, 2, 3], 1);
        assert_eq!(u, vec![vec![1, 2], vec![3]]);
    }

    #[test]
    fn work_units_cover_each_read_one_time() {
        let bases = [NO, 0, NO, 2, 1, NO, 3, 5, NO, 8];
        let reads = [0, 1, 2, 3, 4, 6, 7, 8, 9];
        for unit_reads in [1, 2, 3, 5, 100] {
            let u = units(&bases, &reads, unit_reads);
            assert!(u.iter().all(|unit| unit.windows(2).all(|w| w[0] < w[1])));
            let mut all: Vec<u32> = u.concat();
            all.sort_unstable();
            let want: Vec<u32> = reads.iter().map(|&r| r as u32).collect();
            assert_eq!(all, want, "unit_reads {unit_reads}");
        }
    }

    #[test]
    fn work_units_of_no_reads_are_empty() {
        assert!(units(&[NO, 0], &[], 4).is_empty());
    }
}
