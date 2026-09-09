use async_trait::async_trait;
use chrono::{DateTime, Utc};
use github_mirror_sdk::{
    Branch, CheckRun, Comment, Commit, CommitComment, CommitFile, CommitStatus, Contributor,
    Deployment, Issue, IssueEvent, IssueReaction, IssueTimelineEvent, Label, Milestone,
    PullRequest, PullRequestCommit, PullRequestFile, Release, Repo, Review, ReviewComment,
    ReviewThread, SyncSummary, Tag, WorkflowJob, WorkflowRun,
};
use toolkit_macros::domain_model;
use toolkit_odata::{ODataQuery, Page};
use toolkit_security::AccessScope;
use uuid::Uuid;

use super::error::DomainError;
use super::ports::github::FetchedRepository;

/// Write-side record for a mirrored repository (what sync knows about it).
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoRecord {
    pub id: i64,
    /// GitHub's GraphQL global id for this entity (DESIGN's `node_id`).
    pub node_id: Option<String>,
    pub owner: String,
    pub name: String,
    pub full_name: String,
    pub default_branch: String,
    pub private: bool,
    pub pushed_at: Option<String>,
    pub stars: i64,
    pub forks: i64,
    pub description: Option<String>,
    pub clone_url: Option<String>,
}

#[async_trait]
pub trait RepoRepository: Send + Sync {
    async fn upsert(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        record: RepoRecord,
    ) -> Result<Repo, DomainError>;

    /// One page of mirrored repositories, honouring the caller's `OData`
    /// `$filter`, `$orderby`, `$top` and cursor.
    async fn list(
        &self,
        scope: &AccessScope,
        query: &ODataQuery,
    ) -> Result<Page<Repo>, DomainError>;

    /// One offset-addressed page, for the GitHub-compatible surface, which
    /// numbers its pages instead of carrying a cursor.
    async fn list_window(
        &self,
        scope: &AccessScope,
        window: PageWindow,
    ) -> Result<Vec<Repo>, DomainError>;

    async fn find_by_full_name(
        &self,
        scope: &AccessScope,
        full_name: &str,
    ) -> Result<Option<Repo>, DomainError>;
}

/// Write-side record for a mirrored issue (pull requests included).
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueRecord {
    pub id: i64,
    /// GitHub's GraphQL global id for this entity (DESIGN's `node_id`).
    pub node_id: Option<String>,
    pub repo_id: i64,
    pub number: i64,
    pub title: String,
    pub body: Option<String>,
    pub state: String,
    pub is_pull_request: bool,
    pub created_at: String,
    pub updated_at: String,
    pub closed_at: Option<String>,
    pub html_url: Option<String>,
    /// Who opened it; GitHub's `user`.
    pub author_login: Option<String>,
    /// The author as GitHub's own `user` object, JSON; `author_login` above
    /// is the same person as an indexable identity.
    pub author_json: Option<String>,
    /// Assignee logins as a JSON array, and the labels it carries.
    pub assignees_json: Option<String>,
    pub labels_json: Option<String>,
    /// How many comments GitHub reports on it.
    pub comments_count: Option<i64>,
    pub locked: Option<bool>,
}

/// The slice of a listing a caller asked for: how many rows, and how many to
/// skip first. The skip is pushed into SQL so a request for page 50 does not
/// read the 49 pages before it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageWindow {
    limit: u64,
    offset: u64,
}

impl PageWindow {
    /// Largest number of rows a window may skip.
    ///
    /// The bound belongs to the type rather than to one caller: every
    /// listing turns `offset` straight into SQL `OFFSET`, so any surface
    /// that builds a window - a handler, the local client, an SDK consumer
    /// - is held to the same limit.
    ///
    /// The value is where GitHub stops as well. Measured on
    /// `/repos/rust-lang/rust/issues`, `per_page=100&page=99` (offset
    /// 9,800) answers 200 and `per_page=100&page=100` (offset 9,900)
    /// answers 422, so a client paging the mirror reaches as far as it
    /// would upstream.
    pub const MAX_OFFSET: u64 = 9_900;

    /// Most rows a window may ask for at once.
    ///
    /// `offset` alone is not enough of a guard: the row count becomes SQL
    /// `LIMIT`, so an unbounded one loads a whole table into memory. The
    /// REST surface caps a page at 100; this is the ceiling for the wider
    /// internal reads, such as the contributor merge.
    pub const MAX_LIMIT: u64 = 10_000;

    /// A window within [`Self::MAX_LIMIT`] and [`Self::MAX_OFFSET`], the
    /// only way to build one that skips rows.
    ///
    /// This is the constructor for a caller-supplied page, so an out-of-range
    /// value is refused rather than adjusted: a caller that asked for
    /// something the mirror will not serve should be told, not handed a
    /// different answer silently. [`Self::first`] clamps instead, because its
    /// argument is a constant in this crate rather than a request.
    ///
    /// # Errors
    /// `Validation` when the row count or the offset is past its limit.
    pub fn bounded(limit: u64, offset: u64) -> Result<Self, DomainError> {
        if limit > Self::MAX_LIMIT {
            return Err(DomainError::Validation {
                field: "per_page".to_owned(),
                message: format!("a page may hold {} rows at most", Self::MAX_LIMIT),
            });
        }
        if offset > Self::MAX_OFFSET {
            return Err(DomainError::Validation {
                field: "page".to_owned(),
                message: format!(
                    "page-based pagination reaches row {} at most; narrow the                      listing with a filter or ask for a smaller per_page",
                    Self::MAX_OFFSET
                ),
            });
        }
        Ok(Self { limit, offset })
    }

    /// The first `limit` rows, clamped to [`Self::MAX_LIMIT`].
    ///
    /// Clamped rather than refused because every caller passes a constant
    /// from this crate, so an over-large value is a bug to cap rather than a
    /// request to reject; [`Self::bounded`] is the one that answers a caller.
    #[must_use]
    pub const fn first(limit: u64) -> Self {
        Self {
            limit: if limit > Self::MAX_LIMIT {
                Self::MAX_LIMIT
            } else {
                limit
            },
            offset: 0,
        }
    }

    /// How many rows to read.
    #[must_use]
    pub const fn limit(self) -> u64 {
        self.limit
    }

    /// How many rows to skip first.
    #[must_use]
    pub const fn offset(self) -> u64 {
        self.offset
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "a panic in these tests is the failure report"
)]
mod page_window_tests {
    use super::PageWindow;
    use crate::domain::error::DomainError;

    #[test]
    fn an_ordinary_window_keeps_both_numbers() {
        let window = PageWindow::bounded(30, 60).unwrap();
        assert_eq!(window.limit(), 30);
        assert_eq!(window.offset(), 60);
    }

    #[test]
    fn the_limits_are_inclusive() {
        let window = PageWindow::bounded(PageWindow::MAX_LIMIT, PageWindow::MAX_OFFSET).unwrap();
        assert_eq!(window.limit(), PageWindow::MAX_LIMIT);
        assert_eq!(window.offset(), PageWindow::MAX_OFFSET);
    }

    #[test]
    fn a_caller_supplied_window_past_a_limit_is_refused() {
        let too_many = PageWindow::bounded(PageWindow::MAX_LIMIT + 1, 0);
        assert!(matches!(
            too_many,
            Err(DomainError::Validation { ref field, .. }) if field == "per_page"
        ));

        let too_far = PageWindow::bounded(30, PageWindow::MAX_OFFSET + 1);
        assert!(matches!(
            too_far,
            Err(DomainError::Validation { ref field, .. }) if field == "page"
        ));
    }

    #[test]
    fn first_clamps_instead_of_refusing() {
        assert_eq!(PageWindow::first(50).limit(), 50);
        assert_eq!(PageWindow::first(50).offset(), 0);
        assert_eq!(
            PageWindow::first(PageWindow::MAX_LIMIT * 2).limit(),
            PageWindow::MAX_LIMIT,
            "its argument is a constant in this crate, so it is capped rather than refused"
        );
    }
}

/// GitHub's list-endpoint filters for issues and pull requests.
///
/// `sort`/`direction` are honored; the filters GitHub also accepts but the
/// mirror does not yet apply (`labels`, `assignee`, `creator`, `mentioned`,
/// `milestone`) are recorded as unsupported in PRD 4.3 rather than silently
/// ignored here.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ListingFilter {
    /// The state to keep, or `None` for every state.
    pub state: Option<IssueState>,
    /// `created` (GitHub's default) or `updated`.
    pub sort: ListingSort,
    /// Ascending or descending; GitHub defaults to descending.
    pub direction: ListingDirection,
    /// Only rows updated at or after this instant.
    pub since: Option<DateTime<Utc>>,
}

/// The state an issue or pull request can be listed by.
///
/// A parsed enum rather than the raw query string: an unrecognised `state`
/// used to reach SQL and return an empty page, which reads as "no such
/// issues" instead of "no such state".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IssueState {
    Open,
    Closed,
}

impl IssueState {
    /// The value stored in the `state` column.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Closed => "closed",
        }
    }

    /// GitHub's `state` query value.
    ///
    /// # Errors
    /// `Validation` when the value is neither `open` nor `closed`; `all` is
    /// the caller's business, not this type's.
    pub fn parse(raw: &str) -> Result<Self, DomainError> {
        match raw {
            "open" => Ok(Self::Open),
            "closed" => Ok(Self::Closed),
            other => Err(DomainError::Validation {
                field: "state".to_owned(),
                message: format!("`{other}` is not a state; use open, closed or all"),
            }),
        }
    }
}

/// The sort keys GitHub offers that the mirror stores a column for.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ListingSort {
    #[default]
    Created,
    Updated,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ListingDirection {
    #[default]
    Desc,
    Asc,
}

impl ListingSort {
    /// GitHub's `sort` value, or the default when the caller sent none.
    ///
    /// # Errors
    /// `Validation` when the value is not a sort key, the same way an
    /// unknown `state` is refused: GitHub answers a mistyped `sort` with a
    /// validation error rather than quietly sorting by something else.
    pub fn parse(raw: Option<&str>) -> Result<Self, DomainError> {
        match raw {
            None | Some("created") => Ok(Self::Created),
            Some("updated") => Ok(Self::Updated),
            Some(other) => Err(DomainError::Validation {
                field: "sort".to_owned(),
                message: format!("`{other}` is not a sort key; use created or updated"),
            }),
        }
    }
}

impl ListingDirection {
    /// GitHub's `direction` value, or the default when the caller sent none.
    ///
    /// # Errors
    /// `Validation` when the value is neither `asc` nor `desc`.
    pub fn parse(raw: Option<&str>) -> Result<Self, DomainError> {
        match raw {
            None | Some("desc") => Ok(Self::Desc),
            Some("asc") => Ok(Self::Asc),
            Some(other) => Err(DomainError::Validation {
                field: "direction".to_owned(),
                message: format!("`{other}` is not a direction; use asc or desc"),
            }),
        }
    }
}

#[async_trait]
pub trait IssueRepository: Send + Sync {
    /// One page and the total that describes it.
    ///
    /// The total spans every page, so it cannot be read off one, and both
    /// statements run on one transaction: the `Link` header's `rel="last"`
    /// cannot be computed from a table state the returned rows never saw.
    async fn page_by_repo(
        &self,
        scope: &AccessScope,
        repo_id: i64,
        window: PageWindow,
        filter: ListingFilter,
    ) -> Result<(Vec<Issue>, u64), DomainError>;
    async fn upsert(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        record: IssueRecord,
    ) -> Result<Issue, DomainError>;

    /// `state`: GitHub's `open`/`closed` filter, or `None` for every state.
    async fn list_by_repo(
        &self,
        scope: &AccessScope,
        repo_id: i64,
        window: PageWindow,
        filter: ListingFilter,
    ) -> Result<Vec<Issue>, DomainError>;

    async fn find_by_number(
        &self,
        scope: &AccessScope,
        repo_id: i64,
        number: i64,
    ) -> Result<Option<Issue>, DomainError>;
    /// Hard-delete this repo's rows whose `extracted_at` predates
    /// `extracted_before` — rows the sync that set the watermark did not
    /// see. Only called for a listing fetched to completion; a truncated or
    /// scope-disabled listing proves nothing about absence.
    async fn delete_stale(
        &self,
        scope: &AccessScope,
        repo_id: i64,
        extracted_before: DateTime<Utc>,
    ) -> Result<u64, DomainError>;
}

/// Write-side record for a mirrored pull request.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRequestRecord {
    pub id: i64,
    /// GitHub's GraphQL global id for this entity (DESIGN's `node_id`).
    pub node_id: Option<String>,
    pub repo_id: i64,
    pub number: i64,
    pub title: String,
    pub body: Option<String>,
    pub state: String,
    pub draft: bool,
    pub merged: bool,
    pub head_sha: Option<String>,
    pub base_sha: Option<String>,
    pub lines_added: i64,
    pub lines_removed: i64,
    pub created_at: String,
    pub updated_at: String,
    pub closed_at: Option<String>,
    pub merged_at: Option<String>,
    pub html_url: Option<String>,
    /// Branch names of the pull request's head and base.
    pub head_ref: Option<String>,
    pub base_ref: Option<String>,
    /// Who opened it; GitHub's `user`.
    pub author_login: Option<String>,
    /// The author as GitHub's own `user` object, JSON; `author_login` above
    /// is the same person as an indexable identity.
    pub author_json: Option<String>,
    /// Assignee logins as a JSON array, and the labels it carries.
    pub assignees_json: Option<String>,
    pub labels_json: Option<String>,
    /// How many comments GitHub reports on it.
    pub comments_count: Option<i64>,
    pub locked: Option<bool>,
    /// Reviewers requested on the pull request, as a JSON array.
    pub requested_reviewers_json: Option<String>,
}

#[async_trait]
pub trait PullRequestRepository: Send + Sync {
    /// One page and the total that describes it, read on one transaction.
    async fn page_by_repo(
        &self,
        scope: &AccessScope,
        repo_id: i64,
        window: PageWindow,
        filter: ListingFilter,
    ) -> Result<(Vec<PullRequest>, u64), DomainError>;
    async fn upsert(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        record: PullRequestRecord,
    ) -> Result<PullRequest, DomainError>;

    /// `state`: GitHub's `open`/`closed` filter, or `None` for every state.
    /// A merged pull request is `closed` upstream, so no extra case is needed.
    async fn list_by_repo(
        &self,
        scope: &AccessScope,
        repo_id: i64,
        window: PageWindow,
        filter: ListingFilter,
    ) -> Result<Vec<PullRequest>, DomainError>;

    async fn find_by_number(
        &self,
        scope: &AccessScope,
        repo_id: i64,
        number: i64,
    ) -> Result<Option<PullRequest>, DomainError>;
    /// Hard-delete this repo's rows whose `extracted_at` predates
    /// `extracted_before` — rows the sync that set the watermark did not
    /// see. Only called for a listing fetched to completion; a truncated or
    /// scope-disabled listing proves nothing about absence.
    async fn delete_stale(
        &self,
        scope: &AccessScope,
        repo_id: i64,
        extracted_before: DateTime<Utc>,
    ) -> Result<u64, DomainError>;
}

/// Write-side record for a mirrored commit.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitRecord {
    pub repo_id: i64,
    pub sha: String,
    pub message: String,
    pub author_login: Option<String>,
    pub committer_login: Option<String>,
    pub authored_at: Option<String>,
    pub committed_at: Option<String>,
    pub additions: i64,
    pub deletions: i64,
}

#[async_trait]
pub trait CommitRepository: Send + Sync {
    /// One page and the total that describes it, read on one transaction.
    async fn page_by_repo(
        &self,
        scope: &AccessScope,
        repo_id: i64,
        window: PageWindow,
        since: Option<DateTime<Utc>>,
    ) -> Result<(Vec<Commit>, u64), DomainError>;
    async fn upsert(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        record: CommitRecord,
    ) -> Result<Commit, DomainError>;

    /// Newest first, keeping only commits committed at or after `since`
    /// when one is given - GitHub's own `?since=` on this listing.
    async fn list_by_repo(
        &self,
        scope: &AccessScope,
        repo_id: i64,
        window: PageWindow,
        since: Option<DateTime<Utc>>,
    ) -> Result<Vec<Commit>, DomainError>;

    async fn find_by_sha(
        &self,
        scope: &AccessScope,
        repo_id: i64,
        sha: &str,
    ) -> Result<Option<Commit>, DomainError>;
    /// Hard-delete this repo's rows whose `extracted_at` predates
    /// `extracted_before` — rows the sync that set the watermark did not
    /// see. Only called for a listing fetched to completion; a truncated or
    /// scope-disabled listing proves nothing about absence.
    async fn delete_stale(
        &self,
        scope: &AccessScope,
        repo_id: i64,
        extracted_before: DateTime<Utc>,
    ) -> Result<u64, DomainError>;
}

/// Write-side record for a mirrored issue/PR comment.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommentRecord {
    pub id: i64,
    pub repo_id: i64,
    pub issue_number: i64,
    pub author_login: Option<String>,
    pub body: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub html_url: Option<String>,
}

#[async_trait]
pub trait CommentRepository: Send + Sync {
    async fn upsert(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        record: CommentRecord,
    ) -> Result<Comment, DomainError>;

    async fn list_by_issue(
        &self,
        scope: &AccessScope,
        repo_id: i64,
        issue_number: i64,
        window: PageWindow,
    ) -> Result<Vec<Comment>, DomainError>;
    /// Hard-delete this repo's rows whose `extracted_at` predates
    /// `extracted_before` — rows the sync that set the watermark did not
    /// see. Only called for a listing fetched to completion; a truncated or
    /// scope-disabled listing proves nothing about absence.
    async fn delete_stale(
        &self,
        scope: &AccessScope,
        repo_id: i64,
        extracted_before: DateTime<Utc>,
    ) -> Result<u64, DomainError>;
}

/// Write-side record for a mirrored PR review comment.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewCommentRecord {
    pub id: i64,
    pub repo_id: i64,
    pub pull_number: i64,
    pub author_login: Option<String>,
    pub body: Option<String>,
    pub path: Option<String>,
    pub diff_hunk: Option<String>,
    pub in_reply_to_id: Option<i64>,
    pub commit_id: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub html_url: Option<String>,
    /// Line position in the current diff. `None` once GitHub considers the
    /// commented-on line outdated (superseded by a later push).
    pub position: Option<i64>,
    /// Line position at comment-creation time — GitHub's own stable anchor
    /// for resolving where a comment pointed before later force-pushes.
    pub original_position: Option<i64>,
    /// GitHub's current diff anchors, replacing `position`: the line and
    /// side a comment sits on, plus the start of a multi-line selection.
    pub line: Option<i64>,
    pub original_line: Option<i64>,
    pub start_line: Option<i64>,
    pub original_start_line: Option<i64>,
    pub side: Option<String>,
    pub start_side: Option<String>,
    pub subject_type: Option<String>,
    /// The review this inline comment belongs to, when it belongs to one.
    pub pull_request_review_id: Option<i64>,
}

#[async_trait]
pub trait ReviewCommentRepository: Send + Sync {
    async fn upsert(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        record: ReviewCommentRecord,
    ) -> Result<ReviewComment, DomainError>;

    async fn list_by_pull(
        &self,
        scope: &AccessScope,
        repo_id: i64,
        pull_number: i64,
        window: PageWindow,
    ) -> Result<Vec<ReviewComment>, DomainError>;
    /// Hard-delete this repo's rows whose `extracted_at` predates
    /// `extracted_before` — rows the sync that set the watermark did not
    /// see. Only called for a listing fetched to completion; a truncated or
    /// scope-disabled listing proves nothing about absence.
    async fn delete_stale(
        &self,
        scope: &AccessScope,
        repo_id: i64,
        extracted_before: DateTime<Utc>,
    ) -> Result<u64, DomainError>;
}

/// Write-side record for a mirrored PR review.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewRecord {
    pub id: i64,
    pub repo_id: i64,
    pub pull_number: i64,
    pub author_login: Option<String>,
    pub state: String,
    pub body: Option<String>,
    pub commit_id: Option<String>,
    pub submitted_at: Option<String>,
    pub html_url: Option<String>,
}

#[async_trait]
pub trait ReviewRepository: Send + Sync {
    async fn upsert(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        record: ReviewRecord,
    ) -> Result<Review, DomainError>;

    async fn list_by_pull(
        &self,
        scope: &AccessScope,
        repo_id: i64,
        pull_number: i64,
        window: PageWindow,
    ) -> Result<Vec<Review>, DomainError>;
}

/// Write-side record for a mirrored label.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabelRecord {
    pub id: i64,
    pub repo_id: i64,
    pub name: String,
    pub color: String,
    pub is_default: bool,
    pub description: Option<String>,
}

#[async_trait]
pub trait LabelRepository: Send + Sync {
    async fn upsert(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        record: LabelRecord,
    ) -> Result<Label, DomainError>;

    async fn list_by_repo(
        &self,
        scope: &AccessScope,
        repo_id: i64,
        window: PageWindow,
    ) -> Result<Vec<Label>, DomainError>;
    /// Hard-delete this repo's rows whose `extracted_at` predates
    /// `extracted_before` — rows the sync that set the watermark did not
    /// see. Only called for a listing fetched to completion; a truncated or
    /// scope-disabled listing proves nothing about absence.
    async fn delete_stale(
        &self,
        scope: &AccessScope,
        repo_id: i64,
        extracted_before: DateTime<Utc>,
    ) -> Result<u64, DomainError>;
}

/// Write-side record for a mirrored milestone.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MilestoneRecord {
    pub id: i64,
    pub repo_id: i64,
    pub number: i64,
    pub title: String,
    pub state: String,
    pub description: Option<String>,
    pub open_issues: i64,
    pub closed_issues: i64,
    pub due_on: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub closed_at: Option<String>,
    pub html_url: Option<String>,
}

#[async_trait]
pub trait MilestoneRepository: Send + Sync {
    async fn upsert(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        record: MilestoneRecord,
    ) -> Result<Milestone, DomainError>;

    async fn list_by_repo(
        &self,
        scope: &AccessScope,
        repo_id: i64,
        window: PageWindow,
    ) -> Result<Vec<Milestone>, DomainError>;
    /// Hard-delete this repo's rows whose `extracted_at` predates
    /// `extracted_before` — rows the sync that set the watermark did not
    /// see. Only called for a listing fetched to completion; a truncated or
    /// scope-disabled listing proves nothing about absence.
    async fn delete_stale(
        &self,
        scope: &AccessScope,
        repo_id: i64,
        extracted_before: DateTime<Utc>,
    ) -> Result<u64, DomainError>;
}

/// Write-side record for a mirrored release.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseRecord {
    pub id: i64,
    pub repo_id: i64,
    pub tag_name: String,
    pub name: Option<String>,
    pub draft: bool,
    pub prerelease: bool,
    pub body: Option<String>,
    pub author_login: Option<String>,
    pub created_at: String,
    pub published_at: Option<String>,
    pub html_url: Option<String>,
    /// The release's assets as raw JSON (`name`, `browser_download_url`,
    /// `size` per asset); `None` when the release has none.
    pub assets_json: Option<String>,
}

#[async_trait]
pub trait ReleaseRepository: Send + Sync {
    async fn upsert(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        record: ReleaseRecord,
    ) -> Result<Release, DomainError>;

    async fn list_by_repo(
        &self,
        scope: &AccessScope,
        repo_id: i64,
        window: PageWindow,
    ) -> Result<Vec<Release>, DomainError>;
    /// Hard-delete this repo's rows whose `extracted_at` predates
    /// `extracted_before` — rows the sync that set the watermark did not
    /// see. Only called for a listing fetched to completion; a truncated or
    /// scope-disabled listing proves nothing about absence.
    async fn delete_stale(
        &self,
        scope: &AccessScope,
        repo_id: i64,
        extracted_before: DateTime<Utc>,
    ) -> Result<u64, DomainError>;
}

/// Write-side record for a mirrored branch head.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchRecord {
    pub repo_id: i64,
    pub name: String,
    pub commit_sha: String,
    pub protected: bool,
}

#[async_trait]
pub trait BranchRepository: Send + Sync {
    async fn upsert(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        record: BranchRecord,
    ) -> Result<Branch, DomainError>;

    async fn list_by_repo(
        &self,
        scope: &AccessScope,
        repo_id: i64,
        window: PageWindow,
    ) -> Result<Vec<Branch>, DomainError>;
    /// Hard-delete this repo's rows whose `extracted_at` predates
    /// `extracted_before` — rows the sync that set the watermark did not
    /// see. Only called for a listing fetched to completion; a truncated or
    /// scope-disabled listing proves nothing about absence.
    async fn delete_stale(
        &self,
        scope: &AccessScope,
        repo_id: i64,
        extracted_before: DateTime<Utc>,
    ) -> Result<u64, DomainError>;
}

/// Write-side record for a mirrored contributor.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContributorRecord {
    pub repo_id: i64,
    pub user_id: i64,
    /// `None` for anonymous contributors.
    pub login: Option<String>,
    pub account_type: String,
    pub avatar_url: Option<String>,
    pub html_url: Option<String>,
    /// PRD 5.2's association roles: `author`, `assignee`, `reviewer`,
    /// `commenter`, `committer`. Sorted and deduplicated; unioned across
    /// syncs, never replaced.
    pub roles: Vec<String>,
    /// When this person was first and last seen in mirrored data.
    pub first_seen_at: Option<DateTime<Utc>>,
    pub last_seen_at: Option<DateTime<Utc>>,
}

#[async_trait]
pub trait ContributorRepository: Send + Sync {
    async fn upsert(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        record: ContributorRecord,
    ) -> Result<Contributor, DomainError>;

    async fn list_by_repo(
        &self,
        scope: &AccessScope,
        repo_id: i64,
        window: PageWindow,
    ) -> Result<Vec<Contributor>, DomainError>;
}

/// Write-side record for a mirrored workflow run.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowRunRecord {
    pub id: i64,
    pub repo_id: i64,
    pub workflow_id: i64,
    pub run_number: i64,
    pub run_attempt: i64,
    pub name: Option<String>,
    pub event: String,
    pub status: Option<String>,
    pub conclusion: Option<String>,
    pub head_branch: Option<String>,
    pub head_sha: String,
    pub created_at: String,
    pub updated_at: String,
    pub html_url: Option<String>,
    pub actor_login: Option<String>,
}

#[async_trait]
pub trait WorkflowRunRepository: Send + Sync {
    /// How many runs this repository has in total, for GitHub's
    /// `total_count`, which spans every page rather than the current one.
    /// One page and the total that describes it, read on one transaction.
    async fn page_by_repo(
        &self,
        scope: &AccessScope,
        repo_id: i64,
        window: PageWindow,
    ) -> Result<(Vec<WorkflowRun>, u64), DomainError>;
    async fn upsert(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        record: WorkflowRunRecord,
    ) -> Result<WorkflowRun, DomainError>;

    async fn list_by_repo(
        &self,
        scope: &AccessScope,
        repo_id: i64,
        window: PageWindow,
    ) -> Result<Vec<WorkflowRun>, DomainError>;
}

/// Write-side record for a mirrored pull-request file.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRequestFileRecord {
    pub repo_id: i64,
    pub pull_number: i64,
    pub filename: String,
    pub status: String,
    pub additions: i64,
    pub deletions: i64,
    pub changes: i64,
    pub previous_filename: Option<String>,
    pub sha: Option<String>,
    /// The file's unified diff as GitHub returned it; `None` when GitHub
    /// omitted it, which it does for very large diffs.
    pub patch: Option<String>,
}

#[async_trait]
pub trait PullRequestFileRepository: Send + Sync {
    async fn upsert(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        record: PullRequestFileRecord,
    ) -> Result<PullRequestFile, DomainError>;

    async fn list_by_pull(
        &self,
        scope: &AccessScope,
        repo_id: i64,
        pull_number: i64,
        window: PageWindow,
    ) -> Result<Vec<PullRequestFile>, DomainError>;
}

/// Write-side record for a mirrored tag.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagRecord {
    pub repo_id: i64,
    pub name: String,
    pub commit_sha: String,
}

#[async_trait]
pub trait TagRepository: Send + Sync {
    async fn upsert(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        record: TagRecord,
    ) -> Result<Tag, DomainError>;

    async fn list_by_repo(
        &self,
        scope: &AccessScope,
        repo_id: i64,
        window: PageWindow,
    ) -> Result<Vec<Tag>, DomainError>;
    /// Hard-delete this repo's rows whose `extracted_at` predates
    /// `extracted_before` — rows the sync that set the watermark did not
    /// see. Only called for a listing fetched to completion; a truncated or
    /// scope-disabled listing proves nothing about absence.
    async fn delete_stale(
        &self,
        scope: &AccessScope,
        repo_id: i64,
        extracted_before: DateTime<Utc>,
    ) -> Result<u64, DomainError>;
}

/// Write-side record for a mirrored commit file.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitFileRecord {
    pub repo_id: i64,
    pub commit_sha: String,
    pub filename: String,
    pub status: String,
    pub additions: i64,
    pub deletions: i64,
    pub changes: i64,
    pub previous_filename: Option<String>,
    pub sha: Option<String>,
}

#[async_trait]
pub trait CommitFileRepository: Send + Sync {
    async fn upsert(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        record: CommitFileRecord,
    ) -> Result<CommitFile, DomainError>;

    async fn list_by_commit(
        &self,
        scope: &AccessScope,
        repo_id: i64,
        commit_sha: &str,
        query: &ODataQuery,
    ) -> Result<Page<CommitFile>, DomainError>;
}

/// Write-side record for a mirrored review thread.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewThreadRecord {
    pub id: String,
    pub repo_id: i64,
    pub pull_number: i64,
    pub is_resolved: bool,
    pub is_outdated: bool,
    pub path: Option<String>,
    pub line: Option<i64>,
    pub resolved_by: Option<String>,
    pub comments_count: i64,
}

#[async_trait]
pub trait ReviewThreadRepository: Send + Sync {
    async fn upsert(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        record: ReviewThreadRecord,
    ) -> Result<ReviewThread, DomainError>;

    async fn list_by_pull(
        &self,
        scope: &AccessScope,
        repo_id: i64,
        pull_number: i64,
        query: &ODataQuery,
    ) -> Result<Page<ReviewThread>, DomainError>;
}

/// Write-side record for a mirrored commit comment.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitCommentRecord {
    pub id: i64,
    pub repo_id: i64,
    pub commit_sha: String,
    pub path: Option<String>,
    pub position: Option<i64>,
    pub author_login: Option<String>,
    pub body: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub html_url: Option<String>,
}

#[async_trait]
pub trait CommitCommentRepository: Send + Sync {
    async fn upsert(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        record: CommitCommentRecord,
    ) -> Result<CommitComment, DomainError>;

    async fn list_by_commit(
        &self,
        scope: &AccessScope,
        repo_id: i64,
        commit_sha: &str,
        window: PageWindow,
    ) -> Result<Vec<CommitComment>, DomainError>;
}

/// Write-side record for a mirrored issue event.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueEventRecord {
    pub id: i64,
    pub repo_id: i64,
    pub issue_number: i64,
    pub event: String,
    pub actor_login: Option<String>,
    pub label_name: Option<String>,
    pub assignee_login: Option<String>,
    pub milestone_title: Option<String>,
    pub commit_id: Option<String>,
    pub created_at: String,
}

#[async_trait]
pub trait IssueEventRepository: Send + Sync {
    async fn upsert(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        record: IssueEventRecord,
    ) -> Result<IssueEvent, DomainError>;

    async fn list_by_issue(
        &self,
        scope: &AccessScope,
        repo_id: i64,
        issue_number: i64,
        window: PageWindow,
    ) -> Result<Vec<IssueEvent>, DomainError>;
}

/// Write-side record for a mirrored deployment.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeploymentRecord {
    pub id: i64,
    pub repo_id: i64,
    pub git_ref: String,
    pub sha: String,
    pub environment: String,
    pub task: String,
    pub description: Option<String>,
    pub creator_login: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[async_trait]
pub trait DeploymentRepository: Send + Sync {
    async fn upsert(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        record: DeploymentRecord,
    ) -> Result<Deployment, DomainError>;

    async fn list_by_repo(
        &self,
        scope: &AccessScope,
        repo_id: i64,
        window: PageWindow,
    ) -> Result<Vec<Deployment>, DomainError>;
}

/// Write-side record for a commit of one pull request.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRequestCommitRecord {
    pub repo_id: i64,
    pub pull_number: i64,
    pub sha: String,
    pub message: String,
    pub author_login: Option<String>,
    pub committer_login: Option<String>,
    pub authored_at: Option<String>,
    pub committed_at: Option<String>,
}

#[async_trait]
pub trait PullRequestCommitRepository: Send + Sync {
    async fn upsert(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        record: PullRequestCommitRecord,
    ) -> Result<PullRequestCommit, DomainError>;

    async fn list_by_pull(
        &self,
        scope: &AccessScope,
        repo_id: i64,
        pull_number: i64,
        window: PageWindow,
    ) -> Result<Vec<PullRequestCommit>, DomainError>;
}

/// Write-side record for a mirrored commit status.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitStatusRecord {
    pub id: i64,
    pub repo_id: i64,
    pub commit_sha: String,
    pub state: String,
    pub context: String,
    pub description: Option<String>,
    pub target_url: Option<String>,
    pub creator_login: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[async_trait]
pub trait CommitStatusRepository: Send + Sync {
    async fn upsert(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        record: CommitStatusRecord,
    ) -> Result<CommitStatus, DomainError>;

    async fn list_by_commit(
        &self,
        scope: &AccessScope,
        repo_id: i64,
        commit_sha: &str,
        window: PageWindow,
    ) -> Result<Vec<CommitStatus>, DomainError>;
}

/// Write-side record for a mirrored workflow job.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowJobRecord {
    pub id: i64,
    pub repo_id: i64,
    pub run_id: i64,
    pub run_attempt: i64,
    pub name: String,
    pub status: Option<String>,
    pub conclusion: Option<String>,
    pub head_sha: String,
    pub runner_name: Option<String>,
    pub started_at: Option<String>,
    pub completed_at: Option<String>,
    pub html_url: Option<String>,
    pub steps_json: Option<String>,
}

#[async_trait]
pub trait WorkflowJobRepository: Send + Sync {
    /// How many jobs this run has in total, for GitHub's `total_count`.
    /// One page and the total that describes it, read on one transaction.
    async fn page_by_run(
        &self,
        scope: &AccessScope,
        repo_id: i64,
        run_id: i64,
        window: PageWindow,
    ) -> Result<(Vec<WorkflowJob>, u64), DomainError>;
    async fn upsert(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        record: WorkflowJobRecord,
    ) -> Result<WorkflowJob, DomainError>;

    async fn list_by_run(
        &self,
        scope: &AccessScope,
        repo_id: i64,
        run_id: i64,
        window: PageWindow,
    ) -> Result<Vec<WorkflowJob>, DomainError>;
}

/// Write-side record for a mirrored issue reaction.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueReactionRecord {
    pub id: i64,
    pub repo_id: i64,
    pub issue_number: i64,
    pub content: String,
    pub user_login: Option<String>,
    pub created_at: String,
}

#[async_trait]
pub trait IssueReactionRepository: Send + Sync {
    async fn upsert(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        record: IssueReactionRecord,
    ) -> Result<IssueReaction, DomainError>;

    async fn list_by_issue(
        &self,
        scope: &AccessScope,
        repo_id: i64,
        issue_number: i64,
        window: PageWindow,
    ) -> Result<Vec<IssueReaction>, DomainError>;
}

/// Write-side record for a mirrored check run.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckRunRecord {
    pub id: i64,
    pub repo_id: i64,
    pub head_sha: String,
    pub name: String,
    pub status: Option<String>,
    pub conclusion: Option<String>,
    pub started_at: Option<String>,
    pub completed_at: Option<String>,
    pub html_url: Option<String>,
    pub details_url: Option<String>,
    pub check_suite_id: Option<i64>,
    pub app_slug: Option<String>,
    pub app_name: Option<String>,
    pub output_title: Option<String>,
    pub output_summary: Option<String>,
    pub annotations_count: i64,
}

#[async_trait]
pub trait CheckRunRepository: Send + Sync {
    /// How many check runs this commit has in total, for GitHub's
    /// `total_count`.
    /// One page and the total that describes it, read on one transaction.
    async fn page_by_commit(
        &self,
        scope: &AccessScope,
        repo_id: i64,
        head_sha: &str,
        window: PageWindow,
    ) -> Result<(Vec<CheckRun>, u64), DomainError>;
    async fn upsert(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        record: CheckRunRecord,
    ) -> Result<CheckRun, DomainError>;

    async fn list_by_commit(
        &self,
        scope: &AccessScope,
        repo_id: i64,
        head_sha: &str,
        window: PageWindow,
    ) -> Result<Vec<CheckRun>, DomainError>;
}

/// Write-side record for one mirrored issue-timeline entry.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueTimelineEventRecord {
    pub repo_id: i64,
    pub issue_number: i64,
    pub position: i64,
    pub event: String,
    pub created_at: Option<String>,
    pub actor_login: Option<String>,
    pub payload_json: String,
}

#[async_trait]
pub trait IssueTimelineRepository: Send + Sync {
    async fn upsert(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        record: IssueTimelineEventRecord,
    ) -> Result<IssueTimelineEvent, DomainError>;

    async fn list_by_issue(
        &self,
        scope: &AccessScope,
        repo_id: i64,
        issue_number: i64,
        window: PageWindow,
    ) -> Result<Vec<IssueTimelineEvent>, DomainError>;

    /// Drop these issues' timelines before they are rewritten.
    ///
    /// Rows are keyed by their index in the fetched timeline, so a timeline
    /// that grew shorter upstream — a deleted comment removes its entry —
    /// would leave the tail of the previous, longer run behind. Clearing the
    /// issues first is what keeps a re-sync idempotent (PRD 5.3).
    async fn delete_by_issues(
        &self,
        scope: &AccessScope,
        repo_id: i64,
        issue_numbers: &[i64],
    ) -> Result<u64, DomainError>;
}

#[async_trait]
pub trait SyncWriter: Send + Sync {
    async fn write_sync(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        fetched: FetchedRepository,
        watermark: DateTime<Utc>,
    ) -> Result<SyncSummary, DomainError>;
}
