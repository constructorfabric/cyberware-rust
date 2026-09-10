//! Public models for the github-mirror gear.
//!
//! Transport-agnostic data structures defining the contract between the
//! github-mirror gear and its consumers. All models carry `#[domain_model]`
//! so infrastructure types cannot leak into them.

use toolkit_macros::domain_model;

/// A GitHub account as it appears embedded in an issue, pull request or
/// review - GitHub's `user` object.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Actor {
    pub login: String,
    /// GitHub's numeric account id; absent on rows mirrored before it was
    /// stored.
    pub id: Option<i64>,
    pub node_id: Option<String>,
    /// `User`, `Bot` or `Organization`.
    pub account_type: Option<String>,
    pub avatar_url: Option<String>,
    pub html_url: Option<String>,
    pub site_admin: Option<bool>,
}

/// A label as GitHub embeds it in an issue or pull request.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabelRef {
    pub id: Option<i64>,
    pub node_id: Option<String>,
    pub name: String,
    /// The chip colour a client paints the label with.
    pub color: Option<String>,
    pub is_default: Option<bool>,
    pub description: Option<String>,
}

/// A file attached to a release.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseAsset {
    pub id: Option<i64>,
    pub node_id: Option<String>,
    pub name: String,
    pub label: Option<String>,
    pub content_type: Option<String>,
    pub size: Option<i64>,
    pub download_count: Option<i64>,
    pub browser_download_url: Option<String>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
}

/// One step of a workflow job.
#[domain_model]
#[derive(Debug, Clone, PartialEq)]
pub struct WorkflowStep {
    pub name: String,
    /// `queued`, `in_progress` or `completed`.
    pub status: Option<String>,
    /// `success`, `failure`, `skipped`, ... - absent until completed.
    pub conclusion: Option<String>,
    /// Position in the job, 1-based.
    pub number: Option<i64>,
    pub started_at: Option<String>,
    pub completed_at: Option<String>,
    /// Step fields GitHub sends that the typed ones above do not name.
    ///
    /// The mirror stores GitHub's step payload whole, so a field it does
    /// not model still reaches the caller.
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// Runtime identity of the mirror gear.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MirrorStatus {
    pub gear: String,
    pub version: String,
    pub api_base_url: String,
}

/// A mirrored GitHub repository (minimal read-slice shape).
///
/// Field set intentionally starts small — it mirrors what the first
/// read-slice (`GET /github-mirror/v1/repos`) serves from the local store
/// and grows as further entity fields are ported.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repo {
    /// GitHub's numeric repository id.
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
    /// HTTPS clone URL as GitHub reported it.
    pub clone_url: Option<String>,
}

impl Repo {
    /// The HTTPS clone URL, deriving GitHub's canonical form when the row was
    /// mirrored before the column existed. Domain rule, not wire formatting:
    /// the mirror serves API metadata only, so the URL always points at the
    /// place the git objects actually live.
    #[must_use]
    pub fn clone_url_or_default(&self) -> String {
        self.clone_url
            .clone()
            .unwrap_or_else(|| format!("https://github.com/{}.git", self.full_name))
    }
}

/// A mirrored GitHub issue (read-slice shape).
///
/// GitHub's API treats pull requests as issues too — `is_pull_request`
/// carries that distinction so consumers can filter either way.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issue {
    /// GitHub's numeric issue id.
    pub id: i64,
    /// GitHub's GraphQL global id for this entity (DESIGN's `node_id`).
    pub node_id: Option<String>,
    /// Owning repository's GitHub id.
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
    /// The author as GitHub's own `user` object; `author_login` above is the
    /// same person as an indexable identity.
    pub author: Option<Actor>,
    /// Who it is assigned to, and the labels it carries.
    pub assignees: Vec<Actor>,
    pub labels: Vec<LabelRef>,
    /// How many comments GitHub reports on it.
    pub comments_count: Option<i64>,
    pub locked: Option<bool>,
}

/// A mirrored GitHub pull request (read-slice shape).
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRequest {
    /// GitHub's numeric pull-request id.
    pub id: i64,
    /// GitHub's GraphQL global id for this entity (DESIGN's `node_id`).
    pub node_id: Option<String>,
    /// Owning repository's GitHub id.
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
    /// The author as GitHub's own `user` object; `author_login` above is the
    /// same person as an indexable identity.
    pub author: Option<Actor>,
    /// Who it is assigned to, and the labels it carries.
    pub assignees: Vec<Actor>,
    pub labels: Vec<LabelRef>,
    /// How many comments GitHub reports on it.
    pub comments_count: Option<i64>,
    pub locked: Option<bool>,
    /// Reviewers requested on the pull request.
    pub requested_reviewers: Vec<Actor>,
}

/// A mirrored GitHub commit (read-slice shape).
///
/// Keyed by `(repo_id, sha)` — commits have no numeric GitHub id.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Commit {
    /// Owning repository's GitHub id.
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

/// Result of one sync pass.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncSummary {
    pub repository: String,
    pub issues_synced: u64,
    pub pull_requests_synced: u64,
    pub commits_synced: u64,
    pub comments_synced: u64,
    pub review_comments_synced: u64,
    pub reviews_synced: u64,
    pub labels_synced: u64,
    pub milestones_synced: u64,
    pub releases_synced: u64,
    pub branches_synced: u64,
    pub contributors_synced: u64,
    pub workflow_runs_synced: u64,
    pub pull_request_files_synced: u64,
    pub tags_synced: u64,
    pub commit_files_synced: u64,
    pub review_threads_synced: u64,
    pub commit_comments_synced: u64,
    pub issue_events_synced: u64,
    pub deployments_synced: u64,
    pub pull_request_commits_synced: u64,
    pub commit_statuses_synced: u64,
    pub workflow_jobs_synced: u64,
    pub issue_reactions_synced: u64,
    pub check_runs_synced: u64,
    pub issue_timeline_synced: u64,
    /// Rows hard-deleted because a complete listing no longer contained them.
    pub stale_rows_deleted: u64,
}

/// A mirrored GitHub issue/PR comment (read-slice shape).
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Comment {
    /// GitHub's numeric comment id.
    pub id: i64,
    /// Owning repository's GitHub id.
    pub repo_id: i64,
    /// Owning issue/PR number.
    pub issue_number: i64,
    pub author_login: Option<String>,
    pub body: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub html_url: Option<String>,
}

/// A mirrored GitHub pull-request review comment (inline diff comment).
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewComment {
    /// GitHub's numeric review-comment id.
    pub id: i64,
    /// Owning repository's GitHub id.
    pub repo_id: i64,
    /// Owning pull-request number.
    pub pull_number: i64,
    pub author_login: Option<String>,
    pub body: Option<String>,
    /// File path the comment is attached to.
    pub path: Option<String>,
    pub diff_hunk: Option<String>,
    /// Id of the comment this one replies to.
    pub in_reply_to_id: Option<i64>,
    /// Commit SHA the comment pins to.
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

/// A mirrored GitHub pull-request review (the verdict object).
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Review {
    /// GitHub's numeric review id.
    pub id: i64,
    /// Owning repository's GitHub id.
    pub repo_id: i64,
    /// Owning pull-request number.
    pub pull_number: i64,
    pub author_login: Option<String>,
    /// `APPROVED`, `CHANGES_REQUESTED`, `COMMENTED`, `DISMISSED`, or `PENDING`.
    pub state: String,
    pub body: Option<String>,
    /// Commit SHA the review pins to.
    pub commit_id: Option<String>,
    /// Absent while the review is still PENDING.
    pub submitted_at: Option<String>,
    pub html_url: Option<String>,
}

/// A mirrored GitHub issue/PR label (read-slice shape).
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Label {
    /// GitHub's numeric label id.
    pub id: i64,
    /// Owning repository's GitHub id.
    pub repo_id: i64,
    pub name: String,
    /// Hex color without the leading `#`.
    pub color: String,
    /// True for GitHub's default label set (bug, documentation, ...).
    pub is_default: bool,
    pub description: Option<String>,
}

/// A mirrored GitHub milestone (read-slice shape).
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Milestone {
    /// GitHub's numeric milestone id.
    pub id: i64,
    /// Owning repository's GitHub id.
    pub repo_id: i64,
    /// Milestone number within the repository.
    pub number: i64,
    pub title: String,
    /// open or closed.
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

/// A mirrored GitHub release (read-slice shape).
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    /// GitHub's numeric release id.
    pub id: i64,
    /// Owning repository's GitHub id.
    pub repo_id: i64,
    pub tag_name: String,
    pub name: Option<String>,
    pub draft: bool,
    pub prerelease: bool,
    pub body: Option<String>,
    pub author_login: Option<String>,
    pub created_at: String,
    /// Absent for drafts.
    pub published_at: Option<String>,
    pub html_url: Option<String>,
    /// The release's downloadable files; empty when it has none.
    pub assets: Vec<ReleaseAsset>,
}

/// A mirrored GitHub branch head (read-slice shape).
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Branch {
    /// Owning repository's GitHub id.
    pub repo_id: i64,
    /// Branch name — branches have no numeric GitHub id.
    pub name: String,
    /// SHA the branch currently points at.
    pub commit_sha: String,
    pub protected: bool,
}

/// A mirrored GitHub repository contributor (read-slice shape).
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Contributor {
    /// Owning repository's GitHub id.
    pub repo_id: i64,
    /// The contributor's GitHub user id.
    pub user_id: i64,
    /// `None` for anonymous contributors.
    pub login: Option<String>,
    /// User, Bot, or Organization.
    pub account_type: String,
    pub avatar_url: Option<String>,
    pub html_url: Option<String>,
    /// PRD 5.2's association roles for this person in this repository.
    pub roles: Vec<String>,
    /// When this person was first and last seen in mirrored data.
    pub first_seen_at: Option<chrono::DateTime<chrono::Utc>>,
    pub last_seen_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// A mirrored GitHub Actions workflow run (read-slice shape).
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowRun {
    /// GitHub's numeric run id.
    pub id: i64,
    /// Owning repository's GitHub id.
    pub repo_id: i64,
    /// Id of the workflow definition this run belongs to.
    pub workflow_id: i64,
    pub run_number: i64,
    /// Retry attempt, 1 for the first run.
    pub run_attempt: i64,
    pub name: Option<String>,
    /// Event that triggered the run (`push`, `pull_request`, ...).
    pub event: String,
    /// `queued`, `in_progress`, or `completed`.
    pub status: Option<String>,
    /// `success`, `failure`, `cancelled`, ... — absent until completed.
    pub conclusion: Option<String>,
    pub head_branch: Option<String>,
    pub head_sha: String,
    pub created_at: String,
    pub updated_at: String,
    pub html_url: Option<String>,
    pub actor_login: Option<String>,
}

/// A mirrored changed file of one pull request (read-slice shape).
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRequestFile {
    /// Owning repository's GitHub id.
    pub repo_id: i64,
    /// Owning pull-request number.
    pub pull_number: i64,
    /// Path of the file inside the repository — files have no numeric id.
    pub filename: String,
    /// added, modified, removed, or renamed.
    pub status: String,
    pub additions: i64,
    pub deletions: i64,
    pub changes: i64,
    /// Present when status is renamed.
    pub previous_filename: Option<String>,
    /// Blob SHA of the file version in this pull request.
    pub sha: Option<String>,
    /// The file's unified diff as GitHub returned it; `None` when GitHub
    /// omitted it, which it does for very large diffs.
    pub patch: Option<String>,
}

/// A mirrored GitHub tag (read-slice shape).
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tag {
    /// Owning repository's GitHub id.
    pub repo_id: i64,
    /// Tag name — tags have no numeric GitHub id.
    pub name: String,
    /// SHA the tag points at.
    pub commit_sha: String,
}

/// A mirrored changed file of one commit (read-slice shape).
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitFile {
    /// Owning repository's GitHub id.
    pub repo_id: i64,
    /// Owning commit SHA.
    pub commit_sha: String,
    /// Path of the file inside the repository — files have no numeric id.
    pub filename: String,
    /// added, modified, removed, or renamed.
    pub status: String,
    pub additions: i64,
    pub deletions: i64,
    pub changes: i64,
    /// Present when status is renamed.
    pub previous_filename: Option<String>,
    /// Blob SHA of the file version in this commit.
    pub sha: Option<String>,
}

/// A mirrored pull-request review conversation thread (read-slice shape).
///
/// Threads exist only in GitHub's GraphQL API; their ids are opaque node
/// strings, not numbers.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewThread {
    /// GraphQL node id of the thread.
    pub id: String,
    /// Owning repository's GitHub id.
    pub repo_id: i64,
    /// Owning pull-request number.
    pub pull_number: i64,
    pub is_resolved: bool,
    pub is_outdated: bool,
    /// File path the thread is anchored to.
    pub path: Option<String>,
    /// Line the thread is anchored to.
    pub line: Option<i64>,
    /// Login of whoever resolved the thread.
    pub resolved_by: Option<String>,
    pub comments_count: i64,
}

/// A mirrored comment left directly on a commit (read-slice shape).
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitComment {
    /// GitHub's numeric comment id.
    pub id: i64,
    /// Owning repository's GitHub id.
    pub repo_id: i64,
    /// SHA of the commit the comment is on.
    pub commit_sha: String,
    /// File path when the comment is pinned to a line.
    pub path: Option<String>,
    /// Diff position when the comment is pinned to a line.
    pub position: Option<i64>,
    pub author_login: Option<String>,
    pub body: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub html_url: Option<String>,
}

/// A mirrored issue event: the audit trail entry of an issue or pull
/// request (labeled, assigned, closed, reopened, ...).
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueEvent {
    /// GitHub's numeric event id.
    pub id: i64,
    /// Owning repository's GitHub id.
    pub repo_id: i64,
    /// Number of the issue or pull request the event belongs to.
    pub issue_number: i64,
    /// `labeled`, `assigned`, `closed`, `reopened`, `referenced`, ...
    pub event: String,
    pub actor_login: Option<String>,
    /// Label name for label events.
    pub label_name: Option<String>,
    /// Assignee login for assignment events.
    pub assignee_login: Option<String>,
    /// Milestone title for milestone events.
    pub milestone_title: Option<String>,
    /// Commit SHA for referenced/closed-by-commit events.
    pub commit_id: Option<String>,
    pub created_at: String,
}

/// A mirrored GitHub issue reaction (read-slice shape).
#[domain_model]
pub struct IssueReaction {
    /// GitHub's numeric reaction id.
    pub id: i64,
    /// Owning repository's GitHub id.
    pub repo_id: i64,
    /// Number of the issue or pull request the reaction belongs to.
    pub issue_number: i64,
    /// `+1`, `-1`, `laugh`, `confused`, `heart`, `hooray`, `rocket`, `eyes`.
    pub content: String,
    pub user_login: Option<String>,
    pub created_at: String,
}

/// A mirrored GitHub check run (read-slice shape).
#[domain_model]
pub struct CheckRun {
    /// GitHub's numeric check-run id.
    pub id: i64,
    /// Owning repository's GitHub id.
    pub repo_id: i64,
    /// SHA of the commit the check ran against.
    pub head_sha: String,
    pub name: String,
    /// `queued`, `in_progress`, or `completed`.
    pub status: Option<String>,
    /// `success`, `failure`, `neutral`, `cancelled`, ... — absent until completed.
    pub conclusion: Option<String>,
    pub started_at: Option<String>,
    pub completed_at: Option<String>,
    pub html_url: Option<String>,
    /// Where the app that produced the check shows its own details.
    pub details_url: Option<String>,
    /// Id of the check suite the run belongs to.
    pub check_suite_id: Option<i64>,
    /// Slug and name of the GitHub App that produced the check.
    pub app_slug: Option<String>,
    pub app_name: Option<String>,
    pub output_title: Option<String>,
    pub output_summary: Option<String>,
    pub annotations_count: i64,
}

impl CheckRun {
    /// Whether GitHub reported an owning App for this check run — the domain
    /// rule behind serving a nested `app` object or `null`.
    #[must_use]
    pub fn has_app(&self) -> bool {
        self.app_slug.is_some() || self.app_name.is_some()
    }
}

/// One entry of a mirrored GitHub issue timeline (read-slice shape).
///
/// The timeline mixes about forty event types whose payloads share almost
/// nothing, and several of them (`committed`, `cross-referenced`) carry no
/// numeric id at all. The mirror therefore keys an entry by its position
/// in the issue's timeline and keeps the GitHub object verbatim in
/// `payload`, so reads can serve back exactly what GitHub sent.
#[domain_model]
pub struct IssueTimelineEvent {
    /// Owning repository's GitHub id.
    pub repo_id: i64,
    /// Number of the issue or pull request the entry belongs to.
    pub issue_number: i64,
    /// Zero-based position in the timeline as GitHub ordered it.
    pub position: i64,
    /// `commented`, `committed`, `labeled`, `renamed`, `cross-referenced`, ...
    pub event: String,
    /// Absent on the event types that carry no timestamp of their own.
    pub created_at: Option<String>,
    pub actor_login: Option<String>,
    /// The whole GitHub timeline entry, as GitHub sent it.
    pub payload: serde_json::Value,
}

/// A mirrored GitHub deployment record (read-slice shape).
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Deployment {
    /// GitHub's numeric deployment id.
    pub id: i64,
    /// Owning repository's GitHub id.
    pub repo_id: i64,
    /// Branch, tag, or SHA that was deployed.
    pub git_ref: String,
    /// SHA the deployment points at.
    pub sha: String,
    /// Target environment (`production`, `staging`, ...).
    pub environment: String,
    /// Deployment task, `deploy` by default.
    pub task: String,
    pub description: Option<String>,
    pub creator_login: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// A commit that belongs to one pull request (read-slice shape).
///
/// The mirror keys these by `(repo, pull_number, sha)` because GitHub
/// serves them per pull request, separately from the repository's own
/// commit list.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRequestCommit {
    /// Owning repository's GitHub id.
    pub repo_id: i64,
    /// Owning pull-request number.
    pub pull_number: i64,
    pub sha: String,
    pub message: String,
    pub author_login: Option<String>,
    pub committer_login: Option<String>,
    pub authored_at: Option<String>,
    pub committed_at: Option<String>,
}

/// A mirrored commit status: one external check reported against a commit
/// (CI build, deploy gate, ...).
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitStatus {
    /// GitHub's numeric status id.
    pub id: i64,
    /// Owning repository's GitHub id.
    pub repo_id: i64,
    /// SHA the status was reported against.
    pub commit_sha: String,
    /// `error`, `failure`, `pending`, or `success`.
    pub state: String,
    /// Reporter-defined check name, unique per commit.
    pub context: String,
    pub description: Option<String>,
    pub target_url: Option<String>,
    pub creator_login: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// A mirrored GitHub Actions workflow job: one job of a workflow run.
#[domain_model]
#[derive(Debug, Clone, PartialEq)]
pub struct WorkflowJob {
    /// GitHub's numeric job id.
    pub id: i64,
    /// Owning repository's GitHub id.
    pub repo_id: i64,
    /// Id of the workflow run the job belongs to.
    pub run_id: i64,
    /// Retry attempt of the owning run.
    pub run_attempt: i64,
    pub name: String,
    /// `queued`, `in_progress`, or `completed`.
    pub status: Option<String>,
    /// `success`, `failure`, `cancelled`, ... — absent until completed.
    pub conclusion: Option<String>,
    pub head_sha: String,
    pub runner_name: Option<String>,
    pub started_at: Option<String>,
    pub completed_at: Option<String>,
    pub html_url: Option<String>,
    /// The job's steps, in the order GitHub lists them.
    pub steps: Vec<WorkflowStep>,
}
