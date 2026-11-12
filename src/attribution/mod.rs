//! The commit that introduced each tree and blob
//!
//! Commits are ranked by (corrected date, hash). The owner of a tree or
//! blob is the commit with the lowest rank that reaches it. An ancestor
//! always has a lower rank than its descendants, so a merge never owns the
//! objects that a parent already has.

mod owner;
mod rank;

use anyhow::Result;
use git_dag_parser_core::models::container::TreeEntries;
use git_dag_parser_core::models::Commit;
use git_dag_parser_core::parser::ParseResult;
use rayon::prelude::*;

/// `tree_introduced_by` and `blob_introduced_by` value of an object that no
/// commit reaches
pub const NOT_INTRODUCED: u32 = u32::MAX;

/// The rank of each commit and the commit that introduced each tree and
/// blob, as arrays in container order
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attribution {
    /// Corrected date of each commit, by commit index
    pub corrected_date: Vec<i64>,
    /// Rank of each commit, by commit index
    pub rank: Vec<u32>,
    /// Index of the commit that introduced each tree, or `NOT_INTRODUCED`
    pub tree_introduced_by: Vec<u32>,
    /// Index of the commit that introduced each blob, or `NOT_INTRODUCED`
    pub blob_introduced_by: Vec<u32>,
    /// Number of commits whose corrected date is not their committer time
    pub corrected_commits: usize,
    /// Number of frontier rounds of the owner calculation
    pub rounds: usize,
}

impl Attribution {
    /// Rank the commits of `parsed` and find the owner of each tree and blob
    pub fn calculate(parsed: &ParseResult) -> Result<Self> {
        Self::from_parts(
            &parsed.container.commits,
            &parsed.tree_entries,
            parsed.container.blobs.len(),
        )
    }

    /// `calculate` from the commits, the tree entries and the blob count
    pub(crate) fn from_parts(
        commits: &[Commit],
        entries: &TreeEntries,
        blobs: usize,
    ) -> Result<Self> {
        let corrected_date = rank::corrected_dates(commits)?;
        let order = rank::rank_order(&corrected_date);
        let mut rank = vec![0u32; commits.len()];
        for (r, &c) in order.iter().enumerate() {
            rank[c as usize] = r as u32;
        }
        let corrected_commits = commits
            .iter()
            .zip(&corrected_date)
            .filter(|(c, &d)| c.commit_time != d)
            .count();

        let owners = owner::owners(commits, &rank, entries, blobs);
        // From the rank of the owner to its commit index
        let commit_of = |owners: Vec<u32>| -> Vec<u32> {
            owners
                .into_par_iter()
                .map(|r| {
                    if r == owner::NO_OWNER {
                        NOT_INTRODUCED
                    } else {
                        order[r as usize]
                    }
                })
                .collect()
        };
        Ok(Self {
            corrected_date,
            rank,
            tree_introduced_by: commit_of(owners.tree),
            blob_introduced_by: commit_of(owners.blob),
            corrected_commits,
            rounds: owners.rounds,
        })
    }

    /// The commit that introduced tree `i`, or None if no commit reaches it
    pub fn tree_introduced_by(&self, i: usize) -> Option<u32> {
        let c = self.tree_introduced_by[i];
        (c != NOT_INTRODUCED).then_some(c)
    }

    /// The commit that introduced blob `i`, or None if no commit reaches it
    pub fn blob_introduced_by(&self, i: usize) -> Option<u32> {
        let c = self.blob_introduced_by[i];
        (c != NOT_INTRODUCED).then_some(c)
    }
}
