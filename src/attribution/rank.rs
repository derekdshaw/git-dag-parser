//! The corrected date and the rank of each commit

use anyhow::{ensure, Result};
use git_dag_parser_core::models::Commit;

/// The corrected date of each commit, by commit index: the larger of its
/// committer time and one more than the corrected date of each parent.
/// Parents that are not in the store are not in `Commit.parents`, so a
/// commit at a shallow boundary starts from its own time.
pub(crate) fn corrected_dates(commits: &[Commit]) -> Result<Vec<i64>> {
    let n = commits.len();
    // The children of each commit, as one flat array: the children of `c`
    // are at `first[c]..first[c + 1]`. A parent listed two times has the
    // child two times, and the child waits for both.
    let mut first = vec![0usize; n + 1];
    for c in commits {
        for &p in &c.parents {
            first[p + 1] += 1;
        }
    }
    for i in 0..n {
        first[i + 1] += first[i];
    }
    let mut next = first.clone();
    let mut children = vec![0u32; first[n]];
    for (i, c) in commits.iter().enumerate() {
        for &p in &c.parents {
            children[next[p]] = i as u32;
            next[p] += 1;
        }
    }

    // Kahn's algorithm: a commit is done when all its parents are done
    let mut waiting: Vec<usize> = commits.iter().map(|c| c.parents.len()).collect();
    let mut ready: Vec<usize> = (0..n).filter(|&c| waiting[c] == 0).collect();
    let mut date: Vec<i64> = commits.iter().map(|c| c.commit_time).collect();
    let mut done = 0;
    while let Some(c) = ready.pop() {
        done += 1;
        for &k in &children[first[c]..first[c + 1]] {
            let k = k as usize;
            date[k] = date[k].max(date[c] + 1);
            waiting[k] -= 1;
            if waiting[k] == 0 {
                ready.push(k);
            }
        }
    }
    ensure!(
        done == n,
        "the commit graph has a cycle: {} of {n} commits have no date",
        n - done
    );
    Ok(date)
}

/// The commit indices sorted by (corrected date, index). Indices are in
/// hash order, so this is the order of (corrected date, hash). Position `r`
/// holds the commit of rank `r`.
pub(crate) fn rank_order(corrected: &[i64]) -> Vec<u32> {
    let n = u32::try_from(corrected.len()).expect("commit count fits in u32");
    let mut order: Vec<u32> = (0..n).collect();
    order.sort_unstable_by_key(|&c| (corrected[c as usize], c));
    order
}

#[cfg(test)]
mod tests {
    use git_dag_parser_core::models::container::HASH_LEN;

    use super::*;

    fn commit(index: usize, parents: &[usize], time: i64) -> Commit {
        let mut c = Commit::new(index, [index as u8; HASH_LEN], 0, 0);
        c.parents = parents.to_vec();
        c.commit_time = time;
        c
    }

    #[test]
    fn a_child_older_than_its_parent_gets_one_more_than_the_parent() {
        // P (1000) <- C (900) <- G (950)
        let commits = [
            commit(0, &[], 1000),
            commit(1, &[0], 900),
            commit(2, &[1], 950),
        ];
        assert_eq!(corrected_dates(&commits).unwrap(), vec![1000, 1001, 1002]);
    }

    #[test]
    fn a_merge_is_after_its_latest_parent() {
        let commits = [
            commit(0, &[], 100),
            commit(1, &[], 500),
            commit(2, &[0, 1], 200),
        ];
        assert_eq!(corrected_dates(&commits).unwrap(), vec![100, 500, 501]);
    }

    #[test]
    fn children_are_processed_after_all_their_parents() {
        // Index order is not topological order: 0 is the child of 2 and 1
        let commits = [
            commit(0, &[2, 1], 10),
            commit(1, &[2], 10),
            commit(2, &[], 10),
        ];
        assert_eq!(corrected_dates(&commits).unwrap(), vec![12, 11, 10]);
    }

    #[test]
    fn a_parent_listed_two_times_is_counted_two_times() {
        let commits = [commit(0, &[], 10), commit(1, &[0, 0], 5)];
        assert_eq!(corrected_dates(&commits).unwrap(), vec![10, 11]);
    }

    #[test]
    fn a_commit_with_a_missing_parent_uses_its_own_time() {
        // The parent is not in the store: `parents` is empty, `boundary` set
        let mut c = commit(0, &[], 700);
        c.boundary = true;
        assert_eq!(corrected_dates(&[c]).unwrap(), vec![700]);
    }

    #[test]
    fn a_cycle_is_an_error() {
        let commits = [commit(0, &[1], 10), commit(1, &[0], 10)];
        assert!(corrected_dates(&commits).is_err());
    }

    #[test]
    fn commits_with_the_same_corrected_date_sort_by_hash() {
        assert_eq!(rank_order(&[30, 20, 20, 10]), vec![3, 1, 2, 0]);
    }

    #[test]
    fn no_commits_give_no_dates_and_no_ranks() {
        assert!(corrected_dates(&[]).unwrap().is_empty());
        assert!(rank_order(&[]).is_empty());
    }
}
