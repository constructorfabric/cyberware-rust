use std::collections::HashSet;

use strum::IntoEnumIterator;

use async_trait::async_trait;
use toolkit_macros::domain_model;

use crate::domain::error::DomainError;
use crate::domain::repo::{
    BranchRecord, CheckRunRecord, CommentRecord, CommitCommentRecord, CommitFileRecord,
    CommitRecord, CommitStatusRecord, ContributorRecord, DeploymentRecord, IssueEventRecord,
    IssueReactionRecord, IssueRecord, IssueTimelineEventRecord, LabelRecord, MilestoneRecord,
    PullRequestCommitRecord, PullRequestFileRecord, PullRequestRecord, ReleaseRecord, RepoRecord,
    ReviewCommentRecord, ReviewRecord, ReviewThreadRecord, TagRecord, WorkflowJobRecord,
    WorkflowRunRecord,
};

/// A top-level listing the sync can reconcile deletions for.
///
/// One variant per family `reconcile_stale` deletes from: that match is
/// exhaustive, so a family added here has to be given a delete before the
/// crate compiles again. `EnumIter` supplies the iteration, so there is no
/// hand-written list to keep in step with the variants either.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, strum::EnumIter)]
pub enum Listing {
    Issues,
    PullRequests,
    Commits,
    Comments,
    ReviewComments,
    Labels,
    Milestones,
    Releases,
    Branches,
    Tags,
}

/// Which top-level listings this fetch walked to their final page.
///
/// Deletion reconciliation may only run against a listing that is provably
/// complete: "absent from a truncated page" says nothing about existence. A
/// listing is complete when the walk reached a response advertising no next
/// page, and incomplete when it stopped at the page cap with more to fetch.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ListingCompleteness {
    /// The listings that were read to the end. Keyed by [`Listing`] so it
    /// cannot drift out of step with the families themselves.
    complete: HashSet<Listing>,
}

impl ListingCompleteness {
    /// Nothing complete - the safe default, since it reconciles nothing.
    #[must_use]
    pub fn none() -> Self {
        Self::default()
    }

    /// Every listing complete — what a fake in tests reports.
    #[must_use]
    pub fn all_complete() -> Self {
        Self {
            complete: Listing::iter().collect(),
        }
    }

    /// Record whether `listing` was walked to its final page.
    pub fn set(&mut self, listing: Listing, complete: bool) {
        if complete {
            self.complete.insert(listing);
        } else {
            self.complete.remove(&listing);
        }
    }

    /// Whether `listing` may be reconciled against.
    #[must_use]
    pub fn is_complete(&self, listing: Listing) -> bool {
        self.complete.contains(&listing)
    }
}

/// What one sync-lite pass fetched from GitHub for a repository.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchedRepository {
    pub repository: RepoRecord,
    /// Which listings below are complete and therefore safe to reconcile
    /// deletions against.
    pub complete: ListingCompleteness,
    pub issues: Vec<IssueRecord>,
    pub pull_requests: Vec<PullRequestRecord>,
    pub commits: Vec<CommitRecord>,
    pub comments: Vec<CommentRecord>,
    pub review_comments: Vec<ReviewCommentRecord>,
    pub reviews: Vec<ReviewRecord>,
    pub labels: Vec<LabelRecord>,
    pub milestones: Vec<MilestoneRecord>,
    pub releases: Vec<ReleaseRecord>,
    pub branches: Vec<BranchRecord>,
    pub contributors: Vec<ContributorRecord>,
    pub workflow_runs: Vec<WorkflowRunRecord>,
    pub pull_request_files: Vec<PullRequestFileRecord>,
    pub tags: Vec<TagRecord>,
    pub commit_files: Vec<CommitFileRecord>,
    pub review_threads: Vec<ReviewThreadRecord>,
    pub commit_comments: Vec<CommitCommentRecord>,
    pub issue_events: Vec<IssueEventRecord>,
    pub deployments: Vec<DeploymentRecord>,
    pub pull_request_commits: Vec<PullRequestCommitRecord>,
    pub commit_statuses: Vec<CommitStatusRecord>,
    pub workflow_jobs: Vec<WorkflowJobRecord>,
    pub issue_reactions: Vec<IssueReactionRecord>,
    pub check_runs: Vec<CheckRunRecord>,
    pub issue_timeline: Vec<IssueTimelineEventRecord>,
}

/// Outbound port to GitHub's REST API (implemented in `infra/github`).
///
/// Fetches a repository and its mirrored families, walking each listing until
/// GitHub reports no next page or the walk hits its page cap.
#[async_trait]
pub trait GithubPort: Send + Sync {
    async fn fetch_repository(
        &self,
        owner: &str,
        name: &str,
    ) -> Result<FetchedRepository, DomainError>;
}
