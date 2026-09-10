//! GitHub Mirror SDK
//!
//! This crate provides the public API for the github-mirror gear:
//! - `GithubMirrorClientV1` trait for inter-gear communication
//! - Model types (`MirrorStatus`, `Repo`)
//!
//! Trait methods return `Result<_, CanonicalError>` — the same
//! canonical-at-boundary pattern as `simple-user-settings-sdk` (ADR 0005,
//! Pattern 1): consumers either propagate the canonical error or match on
//! its categories directly.
//!
//! Consumers obtain the client from `ClientHub`:
//! ```ignore
//! let mirror = hub.get::<dyn GithubMirrorClientV1>()?;
//! let status = mirror.status(&ctx).await?;
//! ```

#![forbid(unsafe_code)]

pub mod api;
pub mod models;

pub use api::GithubMirrorClientV1;
pub use models::{
    Actor, Branch, CheckRun, Comment, Commit, CommitComment, CommitFile, CommitStatus, Contributor,
    Deployment, Issue, IssueEvent, IssueReaction, IssueTimelineEvent, Label, LabelRef, Milestone,
    MirrorStatus, PullRequest, PullRequestCommit, PullRequestFile, Release, ReleaseAsset, Repo,
    Review, ReviewComment, ReviewThread, SyncSummary, Tag, WorkflowJob, WorkflowRun, WorkflowStep,
};
