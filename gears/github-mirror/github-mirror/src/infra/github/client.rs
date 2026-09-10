use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::Deserialize;

use crate::domain::error::DomainError;
use crate::domain::ports::github::{FetchedRepository, GithubPort, Listing, ListingCompleteness};
use crate::domain::repo::{
    BranchRecord, CheckRunRecord, CommentRecord, CommitCommentRecord, CommitFileRecord,
    CommitRecord, CommitStatusRecord, ContributorRecord, DeploymentRecord, IssueEventRecord,
    IssueReactionRecord, IssueRecord, IssueTimelineEventRecord, LabelRecord, MilestoneRecord,
    PullRequestCommitRecord, PullRequestFileRecord, PullRequestRecord, ReleaseRecord, RepoRecord,
    ReviewCommentRecord, ReviewRecord, ReviewThreadRecord, TagRecord, WorkflowJobRecord,
    WorkflowRunRecord,
};
use crate::infra::github::pagination::parse_link_next;
use crate::redact::redacted_word;

const FIRST_PAGE_SIZE: u32 = 50;
/// GitHub serves reviews and changed files only per pull request, so
/// sync-lite fetches them for the first few pulls of the page to keep the
/// call count bounded.
const PER_PULL_SYNC_CAP: usize = 10;
/// Commit stats and files come only from the per-commit detail endpoint,
/// fetched for the first few commits of the page for the same reason.
const PER_COMMIT_SYNC_CAP: usize = 10;
/// Jobs are only reachable per workflow run, so the sync walks the first
/// few runs of the page for the same reason.
const PER_RUN_SYNC_CAP: usize = 10;
/// Reactions are only reachable per issue, so the sync walks the first few
/// issues of the page for the same reason.
const PER_ISSUE_SYNC_CAP: usize = 10;
const USER_AGENT: &str = concat!("cf-gears-github-mirror/", env!("CARGO_PKG_VERSION"));

/// The REST API version every request pins (DESIGN 3.5). Without it the
/// response schema follows GitHub's default, which can change under us.
const GITHUB_API_VERSION: &str = "2022-11-28";
/// Attempts after the first request when GitHub answers with a rate limit.
const RATE_LIMIT_RETRIES: u32 = 3;
/// Longest single back-off sleep, whatever `Retry-After` asks for.
const MAX_RETRY_SLEEP: std::time::Duration = std::time::Duration::from_mins(1);

/// Whether a `403` is GitHub's rate limiter rather than an authorization
/// refusal: rate-limit responses carry `Retry-After` or an exhausted
/// `x-ratelimit-remaining`.
fn is_rate_limited(headers: &reqwest::header::HeaderMap) -> bool {
    headers.contains_key("retry-after")
        || header_string(headers, "x-ratelimit-remaining").as_deref() == Some("0")
}

/// How long to wait before retrying a rate-limited request: `Retry-After`
/// when present, else time until `x-ratelimit-reset`, else exponential in
/// the attempt number — always capped at [`MAX_RETRY_SLEEP`].
fn retry_delay(headers: &reqwest::header::HeaderMap, attempt: u32) -> std::time::Duration {
    let seconds = header_string(headers, "retry-after")
        .and_then(|v| v.parse::<u64>().ok())
        .or_else(|| {
            let reset = header_string(headers, "x-ratelimit-reset")?
                .parse::<i64>()
                .ok()?;
            let now = time::OffsetDateTime::now_utc().unix_timestamp();
            u64::try_from(reset - now).ok()
        })
        .unwrap_or(1u64 << attempt);
    std::time::Duration::from_secs(seconds.max(1)).min(MAX_RETRY_SLEEP)
}

/// One response header as an owned string, when it is present and printable.
fn header_string(headers: &reqwest::header::HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(ToOwned::to_owned)
}
/// Time to establish the TCP/TLS connection to GitHub.
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
/// Time budget for one REST/GraphQL call. A sync makes ~115 of these, so an
/// unbounded client would let one hung request stall the whole sync — this is
/// independent of any edge-level (e.g. api-gateway) timeout the gear does not
/// control.
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_mins(1);

/// Minimal GitHub REST client — increment 1 of gears-rust#4630.
///
/// No conditional requests, pagination, or rate-limit admission yet; those
/// arrive as #4630 completes. The token comes from gear config as a temporary
/// shortcut until credstore integration (#4534).
pub struct GithubClient {
    http: reqwest::Client,
    api_base_url: String,
    token: Option<String>,
}

impl GithubClient {
    /// # Errors
    /// Returns `DomainError::Internal` when the underlying HTTP client cannot
    /// be constructed.
    pub fn new(api_base_url: String, token: Option<String>) -> Result<Self, DomainError> {
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|e| DomainError::internal(format!("failed to build HTTP client: {e}")))?;
        Ok(Self {
            http,
            api_base_url,
            token,
        })
    }

    async fn get_json<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T, DomainError> {
        Ok(self.get_with_headers(path).await?.0)
    }

    /// GET a list endpoint, also reporting whether the listing is complete:
    /// a response whose `Link` header advertises no next page IS the whole
    /// listing. Only a complete listing may reconcile deletions.
    async fn get_list<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
    ) -> Result<(Vec<T>, bool), DomainError> {
        let (items, headers) = self.get_with_headers(path).await?;
        let complete = header_string(&headers, "link")
            .as_deref()
            .and_then(parse_link_next)
            .is_none();
        Ok((items, complete))
    }

    /// GET `path`, waiting out secondary rate limits and classifying
    /// credential refusals, then hand back the body and response headers.
    async fn get_with_headers<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
    ) -> Result<(T, reqwest::header::HeaderMap), DomainError> {
        let url = format!("{}{path}", self.api_base_url.trim_end_matches('/'));

        // Secondary rate limits (403 with rate headers, or 429) are waited
        // out and retried a few times instead of failing the whole sync on
        // the spot; full admission control is #4630's remaining half.
        let mut attempt: u32 = 0;
        let (response, rate_limited) = loop {
            let mut request = self
                .http
                .get(&url)
                .header("Accept", "application/vnd.github+json")
                .header("X-GitHub-Api-Version", GITHUB_API_VERSION);
            if let Some(token) = &self.token {
                request = request.bearer_auth(token);
            }
            let response = request
                .send()
                .await
                .map_err(|e| DomainError::internal(format!("GitHub request failed: {e}")))?;

            let status = response.status();
            let rate_limited = status == reqwest::StatusCode::TOO_MANY_REQUESTS
                || (status == reqwest::StatusCode::FORBIDDEN
                    && is_rate_limited(response.headers()));
            if rate_limited && attempt < RATE_LIMIT_RETRIES {
                let delay = retry_delay(response.headers(), attempt);
                tracing::warn!(
                    url = %redacted_word(&url),
                    %status,
                    attempt,
                    delay_secs = delay.as_secs(),
                    "GitHub rate limit hit; backing off before retrying"
                );
                tokio::time::sleep(delay).await;
                attempt += 1;
                continue;
            }
            break (response, rate_limited);
        };

        let status = response.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(DomainError::NotFound);
        }
        if rate_limited {
            return Err(DomainError::internal(format!(
                "GitHub rate limit persisted through {RATE_LIMIT_RETRIES} retries for {path}"
            )));
        }
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            // Not a rate limit (checked above): the mirror's own token no
            // longer sees this resource — the repo went private, the token
            // was revoked, or its scopes shrank.
            return Err(DomainError::AccessLost(format!(
                "GitHub answered {status} for {path}"
            )));
        }
        if !status.is_success() {
            return Err(DomainError::internal(format!(
                "GitHub responded with {status} for {path}"
            )));
        }

        let headers = response.headers().clone();
        let parsed = response
            .json::<T>()
            .await
            .map_err(|e| DomainError::internal(format!("GitHub response decode failed: {e}")))?;
        Ok((parsed, headers))
    }

    async fn post_graphql(
        &self,
        query: &str,
        variables: serde_json::Value,
    ) -> Result<serde_json::Value, DomainError> {
        let url = format!("{}/graphql", self.api_base_url.trim_end_matches('/'));

        let mut attempt: u32 = 0;
        let response = loop {
            let mut request = self
                .http
                .post(&url)
                .json(&serde_json::json!({ "query": query, "variables": variables }));
            if let Some(token) = &self.token {
                request = request.bearer_auth(token);
            }
            let response = request.send().await.map_err(|e| {
                DomainError::internal(format!("GitHub GraphQL request failed: {e}"))
            })?;

            let status = response.status();
            let rate_limited = status == reqwest::StatusCode::TOO_MANY_REQUESTS
                || (status == reqwest::StatusCode::FORBIDDEN
                    && is_rate_limited(response.headers()));
            if rate_limited && attempt < RATE_LIMIT_RETRIES {
                let delay = retry_delay(response.headers(), attempt);
                tracing::warn!(
                    %status,
                    attempt,
                    delay_secs = delay.as_secs(),
                    "GitHub GraphQL rate limit hit; backing off before retrying"
                );
                tokio::time::sleep(delay).await;
                attempt += 1;
                continue;
            }
            break response;
        };

        let status = response.status();
        if !status.is_success() {
            return Err(DomainError::internal(format!(
                "GitHub GraphQL responded with {status}"
            )));
        }

        let body: serde_json::Value = response
            .json()
            .await
            .map_err(|e| DomainError::internal(format!("GitHub GraphQL decode failed: {e}")))?;

        if let Some(errors) = body.get("errors").and_then(serde_json::Value::as_array)
            && !errors.is_empty()
        {
            return Err(DomainError::internal(format!(
                "GitHub GraphQL errors: {errors:?}"
            )));
        }

        Ok(body)
    }
}

#[derive(Debug, Deserialize)]
struct GhOwner {
    login: String,
}

#[derive(Debug, Deserialize)]
struct GhRepository {
    clone_url: Option<String>,
    id: i64,
    #[serde(default)]
    node_id: Option<String>,
    name: String,
    full_name: String,
    owner: GhOwner,
    default_branch: String,
    private: bool,
    pushed_at: Option<String>,
    stargazers_count: i64,
    forks_count: i64,
    description: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GhIssue {
    id: i64,
    #[serde(default)]
    node_id: Option<String>,
    number: i64,
    title: String,
    body: Option<String>,
    state: String,
    #[serde(default)]
    user: Option<GhActor>,
    #[serde(default)]
    assignees: Vec<GhActor>,
    #[serde(default)]
    labels: Vec<GhLabelRef>,
    #[serde(default)]
    comments: Option<i64>,
    #[serde(default)]
    locked: Option<bool>,
    #[serde(default)]
    pull_request: Option<serde::de::IgnoredAny>,
    created_at: String,
    updated_at: String,
    closed_at: Option<String>,
    html_url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GhIssueReaction {
    id: i64,
    content: String,
    user: Option<GhActor>,
    created_at: String,
}

#[derive(Debug, Deserialize)]
struct GhCheckSuiteRef {
    id: i64,
}

#[derive(Debug, Deserialize)]
struct GhCheckApp {
    slug: Option<String>,
    name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GhCheckOutput {
    title: Option<String>,
    summary: Option<String>,
    #[serde(default)]
    annotations_count: i64,
}

#[derive(Debug, Deserialize)]
struct GhCheckRun {
    id: i64,
    head_sha: String,
    name: String,
    status: Option<String>,
    conclusion: Option<String>,
    started_at: Option<String>,
    completed_at: Option<String>,
    html_url: Option<String>,
    details_url: Option<String>,
    check_suite: Option<GhCheckSuiteRef>,
    app: Option<GhCheckApp>,
    output: Option<GhCheckOutput>,
}

/// `GET .../commits/{sha}/check-runs` wraps the list in an object, the way
/// the workflow-run and job listings do.
#[derive(Debug, Deserialize)]
struct GhCheckRunsPage {
    check_runs: Vec<GhCheckRun>,
}

#[derive(Debug, Deserialize)]
struct GhRef {
    sha: Option<String>,
    /// Branch name; GitHub calls it `ref`, which is a Rust keyword.
    #[serde(rename = "ref")]
    ref_name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GhPullRequest {
    id: i64,
    #[serde(default)]
    node_id: Option<String>,
    number: i64,
    title: String,
    body: Option<String>,
    state: String,
    #[serde(default)]
    user: Option<GhActor>,
    #[serde(default)]
    assignees: Vec<GhActor>,
    #[serde(default)]
    requested_reviewers: Vec<GhActor>,
    #[serde(default)]
    labels: Vec<GhLabelRef>,
    #[serde(default)]
    comments: Option<i64>,
    #[serde(default)]
    locked: Option<bool>,

    /// Present on the per-pull response, absent from the listing; when the
    /// payload carries them the record needs no file walk to be accurate.
    #[serde(default)]
    additions: Option<i64>,
    #[serde(default)]
    deletions: Option<i64>,
    draft: Option<bool>,
    merged_at: Option<String>,
    head: Option<GhRef>,
    base: Option<GhRef>,
    created_at: String,
    updated_at: String,
    closed_at: Option<String>,
    html_url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GhCommitPerson {
    date: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GhCommitDetails {
    message: String,
    author: Option<GhCommitPerson>,
    committer: Option<GhCommitPerson>,
}

#[derive(Debug, Deserialize)]
struct GhActor {
    /// Every real GitHub user object carries one; kept optional so a
    /// stripped-down or anonymous actor still yields its login.
    #[serde(default)]
    id: Option<i64>,
    login: String,
    #[serde(default)]
    node_id: Option<String>,
    #[serde(rename = "type", default)]
    user_type: Option<String>,
    #[serde(default)]
    avatar_url: Option<String>,
    #[serde(default)]
    html_url: Option<String>,
    #[serde(default)]
    site_admin: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct GhComment {
    id: i64,
    #[serde(default)]
    user: Option<GhActor>,
    body: Option<String>,
    created_at: String,
    updated_at: String,
    html_url: Option<String>,
    issue_url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GhReviewComment {
    id: i64,
    #[serde(default)]
    user: Option<GhActor>,
    body: Option<String>,
    path: Option<String>,
    diff_hunk: Option<String>,
    in_reply_to_id: Option<i64>,
    commit_id: Option<String>,
    created_at: String,
    updated_at: String,
    html_url: Option<String>,
    pull_request_url: Option<String>,
    /// Absent once GitHub considers the commented-on line outdated.
    position: Option<i64>,
    original_position: Option<i64>,
    /// GitHub's current diff anchors: the line and side a comment sits on,
    /// plus the start of a multi-line selection. `position` above is the
    /// deprecated single-line form GitHub still sends.
    #[serde(default)]
    line: Option<i64>,
    #[serde(default)]
    original_line: Option<i64>,
    #[serde(default)]
    start_line: Option<i64>,
    #[serde(default)]
    original_start_line: Option<i64>,
    #[serde(default)]
    side: Option<String>,
    #[serde(default)]
    start_side: Option<String>,
    #[serde(default)]
    subject_type: Option<String>,
    /// The review this inline comment belongs to; clients group comments
    /// under their review by it.
    #[serde(default)]
    pull_request_review_id: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct GhLabel {
    id: i64,
    name: String,
    color: String,
    #[serde(default, rename = "default")]
    is_default: bool,
    description: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GhMilestone {
    id: i64,
    number: i64,
    title: String,
    state: String,
    description: Option<String>,
    open_issues: i64,
    closed_issues: i64,
    due_on: Option<String>,
    created_at: String,
    updated_at: String,
    closed_at: Option<String>,
    html_url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GhRelease {
    id: i64,
    tag_name: String,
    name: Option<String>,
    draft: bool,
    prerelease: bool,
    body: Option<String>,
    #[serde(default)]
    author: Option<GhActor>,
    created_at: String,
    published_at: Option<String>,
    html_url: Option<String>,
    #[serde(default)]
    assets: Vec<GhReleaseAsset>,
}

/// A label as it appears embedded in an issue or pull-request payload.
///
/// Stored whole rather than by name: names are renameable, the id is not, and
/// the colour is what a client paints the chip with.
#[derive(Debug, serde::Serialize, Deserialize)]
struct GhLabelRef {
    #[serde(default)]
    id: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    node_id: Option<String>,
    name: String,
    #[serde(default)]
    color: Option<String>,
    #[serde(rename = "default", default, skip_serializing_if = "Option::is_none")]
    is_default: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    description: Option<String>,
}

/// The slice of a release asset the mirror keeps: enough to name it and
/// download it.
#[derive(Debug, serde::Serialize, Deserialize)]
struct GhReleaseAsset {
    name: String,
    browser_download_url: Option<String>,
    size: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct GhBranchCommit {
    sha: String,
}

#[derive(Debug, Deserialize)]
struct GhBranch {
    name: String,
    commit: GhBranchCommit,
    #[serde(default)]
    protected: bool,
}

#[derive(Debug, Deserialize)]
struct GhWorkflowRun {
    id: i64,
    workflow_id: i64,
    run_number: i64,
    run_attempt: Option<i64>,
    name: Option<String>,
    event: String,
    status: Option<String>,
    conclusion: Option<String>,
    head_branch: Option<String>,
    head_sha: String,
    #[serde(default)]
    actor: Option<GhActor>,
    created_at: String,
    updated_at: String,
    html_url: Option<String>,
}

/// `GET .../actions/runs` wraps the list in an object instead of returning a
/// bare array like every other list endpoint.
#[derive(Debug, Deserialize)]
struct GhWorkflowRunsPage {
    workflow_runs: Vec<GhWorkflowRun>,
}

#[derive(Debug, Deserialize)]
struct GhPullFile {
    filename: String,
    /// The file's unified diff; GitHub omits it for very large diffs.
    #[serde(default)]
    patch: Option<String>,
    status: String,
    additions: i64,
    deletions: i64,
    changes: i64,
    previous_filename: Option<String>,
    sha: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GhTag {
    name: String,
    commit: GhBranchCommit,
}

#[derive(Debug, Deserialize)]
struct GhCommitStats {
    additions: i64,
    deletions: i64,
}

#[derive(Debug, Deserialize)]
struct GhCommitDetail {
    stats: Option<GhCommitStats>,
    #[serde(default)]
    files: Vec<GhPullFile>,
}

#[derive(Debug, Deserialize)]
struct GhCommitComment {
    id: i64,
    #[serde(default)]
    user: Option<GhActor>,
    commit_id: String,
    path: Option<String>,
    position: Option<i64>,
    body: Option<String>,
    created_at: String,
    updated_at: String,
    html_url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GhEventLabel {
    name: String,
}

#[derive(Debug, Deserialize)]
struct GhEventMilestone {
    title: String,
}

#[derive(Debug, Deserialize)]
struct GhIssueEventIssue {
    number: i64,
}

#[derive(Debug, Deserialize)]
struct GhIssueEvent {
    id: i64,
    event: String,
    #[serde(default)]
    actor: Option<GhActor>,
    #[serde(default)]
    label: Option<GhEventLabel>,
    #[serde(default)]
    assignee: Option<GhActor>,
    #[serde(default)]
    milestone: Option<GhEventMilestone>,
    commit_id: Option<String>,
    created_at: String,
    #[serde(default)]
    issue: Option<GhIssueEventIssue>,
}

#[derive(Debug, Deserialize)]
struct GhDeployment {
    id: i64,
    #[serde(rename = "ref")]
    git_ref: String,
    sha: String,
    environment: String,
    task: String,
    description: Option<String>,
    #[serde(default)]
    creator: Option<GhActor>,
    created_at: String,
    updated_at: String,
}

#[derive(Debug, Deserialize)]
struct GhCommitStatus {
    id: i64,
    state: String,
    context: String,
    description: Option<String>,
    target_url: Option<String>,
    #[serde(default)]
    creator: Option<GhActor>,
    created_at: String,
    updated_at: String,
}

#[derive(Debug, Deserialize)]
struct GhWorkflowJob {
    id: i64,
    run_id: i64,
    run_attempt: Option<i64>,
    name: String,
    status: Option<String>,
    conclusion: Option<String>,
    head_sha: String,
    runner_name: Option<String>,
    started_at: Option<String>,
    completed_at: Option<String>,
    html_url: Option<String>,
    #[serde(default)]
    steps: Option<serde_json::Value>,
}

/// `GET .../actions/runs/{id}/jobs` wraps the list in an object, the way
/// the workflow-run listing does.
#[derive(Debug, Deserialize)]
struct GhWorkflowJobsPage {
    jobs: Vec<GhWorkflowJob>,
}

#[derive(Debug, Deserialize)]
struct GhReview {
    id: i64,
    #[serde(default)]
    user: Option<GhActor>,
    state: String,
    body: Option<String>,
    commit_id: Option<String>,
    submitted_at: Option<String>,
    html_url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GhCommit {
    sha: String,
    commit: GhCommitDetails,
    author: Option<GhActor>,
    committer: Option<GhActor>,
}

fn repository_record(r: GhRepository) -> RepoRecord {
    RepoRecord {
        id: r.id,
        node_id: r.node_id,
        owner: r.owner.login,
        name: r.name,
        full_name: r.full_name,
        default_branch: r.default_branch,
        private: r.private,
        pushed_at: r.pushed_at,
        stars: r.stargazers_count,
        forks: r.forks_count,
        description: r.description,
        clone_url: r.clone_url,
    }
}

fn issue_record(repo_id: i64, i: GhIssue) -> IssueRecord {
    let assignees_json = actors_json(&i.assignees);
    let labels_json = labels_json(&i.labels);
    IssueRecord {
        id: i.id,
        node_id: i.node_id,
        repo_id,
        number: i.number,
        title: i.title,
        body: i.body,
        state: i.state,
        is_pull_request: i.pull_request.is_some(),
        created_at: i.created_at,
        updated_at: i.updated_at,
        closed_at: i.closed_at,
        html_url: i.html_url,
        author_login: i.user.as_ref().map(|u| u.login.clone()),
        author_json: actor_json(i.user.as_ref()),
        assignees_json,
        labels_json,
        comments_count: i.comments,
        locked: i.locked,
    }
}

fn pull_request_record(repo_id: i64, p: GhPullRequest) -> PullRequestRecord {
    let head = p.head;
    let base = p.base;
    let assignees_json = actors_json(&p.assignees);
    let labels_json = labels_json(&p.labels);
    let requested_reviewers_json = actors_json(&p.requested_reviewers);

    PullRequestRecord {
        id: p.id,
        node_id: p.node_id,
        repo_id,
        number: p.number,
        title: p.title,
        body: p.body,
        state: p.state,
        draft: p.draft.unwrap_or(false),
        merged: p.merged_at.is_some(),
        head_sha: head.as_ref().and_then(|r| r.sha.clone()),
        base_sha: base.as_ref().and_then(|r| r.sha.clone()),
        lines_added: p.additions.unwrap_or(0),
        lines_removed: p.deletions.unwrap_or(0),
        created_at: p.created_at,
        updated_at: p.updated_at,
        closed_at: p.closed_at,
        merged_at: p.merged_at,
        html_url: p.html_url,
        head_ref: head.and_then(|r| r.ref_name),
        base_ref: base.and_then(|r| r.ref_name),
        author_login: p.user.as_ref().map(|u| u.login.clone()),
        author_json: actor_json(p.user.as_ref()),
        assignees_json,
        labels_json,
        comments_count: p.comments,
        locked: p.locked,
        requested_reviewers_json,
    }
}

/// The trailing number of an `.../issues/11` or `.../pulls/13` URL.
///
/// `None` when the URL is absent or does not end in a number: the row's
/// parent is then unknown, and storing it under number 0 would file it
/// against an issue that cannot exist.
fn issue_number_from_url(issue_url: Option<&str>) -> Option<i64> {
    issue_url
        .and_then(|u| u.rsplit('/').next())
        .and_then(|n| n.parse().ok())
}

fn comment_record(repo_id: i64, c: GhComment) -> Option<CommentRecord> {
    let Some(issue_number) = issue_number_from_url(c.issue_url.as_deref()) else {
        tracing::warn!(
            comment_id = c.id,
            issue_url = ?c.issue_url,
            "issue comment has no resolvable issue number; skipping"
        );
        return None;
    };
    Some(CommentRecord {
        id: c.id,
        repo_id,
        issue_number,
        author_login: c.user.map(|u| u.login),
        body: c.body,
        created_at: c.created_at,
        updated_at: c.updated_at,
        html_url: c.html_url,
    })
}

fn review_comment_record(repo_id: i64, c: GhReviewComment) -> Option<ReviewCommentRecord> {
    let Some(pull_number) = issue_number_from_url(c.pull_request_url.as_deref()) else {
        tracing::warn!(
            comment_id = c.id,
            pull_request_url = ?c.pull_request_url,
            "review comment has no resolvable pull number; skipping"
        );
        return None;
    };
    Some(ReviewCommentRecord {
        id: c.id,
        repo_id,
        pull_number,
        author_login: c.user.map(|u| u.login),
        body: c.body,
        path: c.path,
        diff_hunk: c.diff_hunk,
        in_reply_to_id: c.in_reply_to_id,
        commit_id: c.commit_id,
        created_at: c.created_at,
        updated_at: c.updated_at,
        html_url: c.html_url,
        position: c.position,
        original_position: c.original_position,
        line: c.line,
        original_line: c.original_line,
        start_line: c.start_line,
        original_start_line: c.original_start_line,
        side: c.side,
        start_side: c.start_side,
        subject_type: c.subject_type,
        pull_request_review_id: c.pull_request_review_id,
    })
}

fn label_record(repo_id: i64, l: GhLabel) -> LabelRecord {
    LabelRecord {
        id: l.id,
        repo_id,
        name: l.name,
        color: l.color,
        is_default: l.is_default,
        description: l.description,
    }
}

fn milestone_record(repo_id: i64, m: GhMilestone) -> MilestoneRecord {
    MilestoneRecord {
        id: m.id,
        repo_id,
        number: m.number,
        title: m.title,
        state: m.state,
        description: m.description,
        open_issues: m.open_issues,
        closed_issues: m.closed_issues,
        due_on: m.due_on,
        created_at: m.created_at,
        updated_at: m.updated_at,
        closed_at: m.closed_at,
        html_url: m.html_url,
    }
}

fn release_record(repo_id: i64, r: GhRelease) -> ReleaseRecord {
    // Kept as the raw asset slice, serialized: the mirror's job is to hand
    // the download URLs back out, not to model asset lifecycles.
    let assets_json = if r.assets.is_empty() {
        None
    } else {
        serde_json::to_string(&r.assets).ok()
    };
    ReleaseRecord {
        id: r.id,
        repo_id,
        tag_name: r.tag_name,
        name: r.name,
        draft: r.draft,
        prerelease: r.prerelease,
        body: r.body,
        author_login: r.author.map(|a| a.login),
        created_at: r.created_at,
        published_at: r.published_at,
        html_url: r.html_url,
        assets_json,
    }
}

fn branch_record(repo_id: i64, b: GhBranch) -> BranchRecord {
    BranchRecord {
        repo_id,
        name: b.name,
        commit_sha: b.commit.sha,
        protected: b.protected,
    }
}

/// Contributors as they appear across one fetch: PRD 5.2's derivation, run
/// over the entities the sync already downloaded. Reviewers join later, from
/// the per-pull fetch that owns the review objects.
fn derive_people(
    repo_id: i64,
    issues: &[GhIssue],
    pulls: &[GhPullRequest],
    commits: &[GhCommit],
    comments: &[GhComment],
    review_comments: &[GhReviewComment],
    commit_comments: &[GhCommitComment],
) -> DerivedContributors {
    let mut people = DerivedContributors::default();
    for issue in issues {
        people.track(
            repo_id,
            issue.user.as_ref(),
            roles::AUTHOR,
            Some(&issue.created_at),
        );
        for assignee in &issue.assignees {
            people.track(
                repo_id,
                Some(assignee),
                roles::ASSIGNEE,
                Some(&issue.created_at),
            );
        }
    }
    for pull in pulls {
        people.track(
            repo_id,
            pull.user.as_ref(),
            roles::AUTHOR,
            Some(&pull.created_at),
        );
        for assignee in &pull.assignees {
            people.track(
                repo_id,
                Some(assignee),
                roles::ASSIGNEE,
                Some(&pull.created_at),
            );
        }
        for reviewer in &pull.requested_reviewers {
            people.track(
                repo_id,
                Some(reviewer),
                roles::REVIEWER,
                Some(&pull.created_at),
            );
        }
    }
    for commit in commits {
        people.track(
            repo_id,
            commit.author.as_ref(),
            roles::AUTHOR,
            commit
                .commit
                .author
                .as_ref()
                .and_then(|p| p.date.as_deref()),
        );
        people.track(
            repo_id,
            commit.committer.as_ref(),
            roles::COMMITTER,
            commit
                .commit
                .committer
                .as_ref()
                .and_then(|p| p.date.as_deref()),
        );
    }
    for comment in comments {
        people.track(
            repo_id,
            comment.user.as_ref(),
            roles::COMMENTER,
            Some(&comment.created_at),
        );
    }
    for comment in review_comments {
        people.track(
            repo_id,
            comment.user.as_ref(),
            roles::COMMENTER,
            Some(&comment.created_at),
        );
    }
    for comment in commit_comments {
        people.track(
            repo_id,
            comment.user.as_ref(),
            roles::COMMENTER,
            Some(&comment.created_at),
        );
    }
    people
}

/// The capacities a person can be seen in. PRD 5.2 wants contributors
/// derived from the entities themselves, and the entity a user object came
/// out of is what names their role.
mod roles {
    pub const AUTHOR: &str = "author";
    pub const ASSIGNEE: &str = "assignee";
    pub const REVIEWER: &str = "reviewer";
    pub const COMMENTER: &str = "commenter";
    pub const COMMITTER: &str = "committer";
}

/// Contributors assembled from the user objects embedded in data the sync
/// already fetched — no `/contributors` request, which PRD 5.2 forbids.
///
/// Keyed by GitHub user id; an actor without one (anonymous or stripped) is
/// skipped, since the mirrored row is keyed by that id.
#[derive(Default)]
struct DerivedContributors {
    by_user: std::collections::HashMap<i64, ContributorRecord>,
}

impl DerivedContributors {
    /// Track one sighting: the person, the capacity, and when it happened.
    ///
    /// `at` is GitHub's own RFC3339 text; it is parsed here so the stored
    /// window is a real instant. An unparseable stamp is ignored rather than
    /// dropping the sighting.
    fn track(&mut self, repo_id: i64, actor: Option<&GhActor>, role: &str, at: Option<&str>) {
        let Some(actor) = actor else { return };
        let Some(user_id) = actor.id else { return };

        let entry = self
            .by_user
            .entry(user_id)
            .or_insert_with(|| ContributorRecord {
                repo_id,
                user_id,
                login: Some(actor.login.clone()),
                account_type: actor.user_type.clone().unwrap_or_else(|| "User".to_owned()),
                avatar_url: actor.avatar_url.clone(),
                html_url: actor.html_url.clone(),
                roles: Vec::new(),
                first_seen_at: None,
                last_seen_at: None,
            });

        if !entry.roles.iter().any(|r| r == role) {
            entry.roles.push(role.to_owned());
        }
        let at = at.and_then(parse_github_timestamp);
        merge_seen_window(entry, at, at);
    }

    /// Fold another family's sightings in: roles union, counts add, the
    /// seen-at window widens.
    fn absorb(&mut self, other: Self) {
        for (user_id, record) in other.by_user {
            match self.by_user.entry(user_id) {
                std::collections::hash_map::Entry::Vacant(slot) => {
                    slot.insert(record);
                }
                std::collections::hash_map::Entry::Occupied(mut slot) => {
                    let mine = slot.get_mut();
                    for role in record.roles {
                        if !mine.roles.contains(&role) {
                            mine.roles.push(role);
                        }
                    }
                    merge_seen_window(mine, record.first_seen_at, record.last_seen_at);
                }
            }
        }
    }

    /// Stable output: by user id, each record's roles sorted.
    fn into_records(self) -> Vec<ContributorRecord> {
        let mut records: Vec<ContributorRecord> = self.by_user.into_values().collect();
        for record in &mut records {
            record.roles.sort();
        }
        records.sort_by_key(|r| r.user_id);
        records
    }
}

/// A person as the mirror stores them inside an issue or pull row: the login
/// a client renders, plus the id it takes to join `gm_contributors`, since a
/// login can be renamed and later reused by someone else.
#[derive(Debug, serde::Serialize)]
struct StoredActor<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<i64>,
    login: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    node_id: Option<&'a str>,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    user_type: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    avatar_url: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    html_url: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    site_admin: Option<bool>,
}

impl<'a> StoredActor<'a> {
    fn of(actor: &'a GhActor) -> Self {
        Self {
            id: actor.id,
            login: actor.login.as_str(),
            node_id: actor.node_id.as_deref(),
            user_type: actor.user_type.as_deref(),
            avatar_url: actor.avatar_url.as_deref(),
            html_url: actor.html_url.as_deref(),
            site_admin: actor.site_admin,
        }
    }
}

/// One actor as the stored JSON object, for the author of an issue or pull.
fn actor_json(actor: Option<&GhActor>) -> Option<String> {
    actor.and_then(|a| serde_json::to_string(&StoredActor::of(a)).ok())
}

/// A list of actors as a JSON array, or `None` when the list is empty — the
/// column stays `NULL` rather than holding `[]`.
fn actors_json(actors: &[GhActor]) -> Option<String> {
    if actors.is_empty() {
        return None;
    }
    let stored: Vec<StoredActor<'_>> = actors.iter().map(StoredActor::of).collect();
    serde_json::to_string(&stored).ok()
}

/// Labels as a JSON array, on the same terms as [`actors_json`].
fn labels_json(labels: &[GhLabelRef]) -> Option<String> {
    if labels.is_empty() {
        return None;
    }
    serde_json::to_string(labels).ok()
}

/// GitHub's RFC3339 text as an instant; `None` when it does not parse.
fn parse_github_timestamp(raw: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|stamp| stamp.with_timezone(&Utc))
}

/// Widen a record's first/last-seen window with another observation.
fn merge_seen_window(
    record: &mut ContributorRecord,
    first_seen_at: Option<DateTime<Utc>>,
    last_seen_at: Option<DateTime<Utc>>,
) {
    if let Some(first) = first_seen_at
        && record.first_seen_at.is_none_or(|held| first < held)
    {
        record.first_seen_at = Some(first);
    }
    if let Some(last) = last_seen_at
        && record.last_seen_at.is_none_or(|held| last > held)
    {
        record.last_seen_at = Some(last);
    }
}

fn workflow_run_record(repo_id: i64, w: GhWorkflowRun) -> WorkflowRunRecord {
    WorkflowRunRecord {
        id: w.id,
        repo_id,
        workflow_id: w.workflow_id,
        run_number: w.run_number,
        run_attempt: w.run_attempt.unwrap_or(1),
        name: w.name,
        event: w.event,
        status: w.status,
        conclusion: w.conclusion,
        head_branch: w.head_branch,
        head_sha: w.head_sha,
        created_at: w.created_at,
        updated_at: w.updated_at,
        html_url: w.html_url,
        actor_login: w.actor.map(|a| a.login),
    }
}

fn pull_request_file_record(
    repo_id: i64,
    pull_number: i64,
    f: GhPullFile,
) -> PullRequestFileRecord {
    PullRequestFileRecord {
        repo_id,
        pull_number,
        filename: f.filename,
        status: f.status,
        additions: f.additions,
        deletions: f.deletions,
        changes: f.changes,
        previous_filename: f.previous_filename,
        sha: f.sha,
        patch: f.patch,
    }
}

fn tag_record(repo_id: i64, t: GhTag) -> TagRecord {
    TagRecord {
        repo_id,
        name: t.name,
        commit_sha: t.commit.sha,
    }
}

fn commit_file_record(repo_id: i64, commit_sha: &str, f: GhPullFile) -> CommitFileRecord {
    CommitFileRecord {
        repo_id,
        commit_sha: commit_sha.to_owned(),
        filename: f.filename,
        status: f.status,
        additions: f.additions,
        deletions: f.deletions,
        changes: f.changes,
        previous_filename: f.previous_filename,
        sha: f.sha,
    }
}

/// The query text is fixed; `owner`, `name` and the pull number travel as
/// GraphQL variables so no request value is ever spliced into the query
/// string itself.
const REVIEW_THREADS_QUERY: &str = "query($owner: String!, $name: String!, $number: Int!, $first: Int!) {      repository(owner: $owner, name: $name) {      pullRequest(number: $number) { reviewThreads(first: $first) {      nodes { id isResolved isOutdated path line resolvedBy { login }      comments(first: 1) { totalCount } } } } } }";

fn review_threads_variables(owner: &str, name: &str, pull_number: i64) -> serde_json::Value {
    serde_json::json!({ "owner": owner, "name": name, "number": pull_number, "first": FIRST_PAGE_SIZE })
}

fn review_thread_record(
    repo_id: i64,
    pull_number: i64,
    node: &serde_json::Value,
) -> Option<ReviewThreadRecord> {
    let id = node.get("id")?.as_str()?.to_owned();
    Some(ReviewThreadRecord {
        id,
        repo_id,
        pull_number,
        is_resolved: node["isResolved"].as_bool().unwrap_or(false),
        is_outdated: node["isOutdated"].as_bool().unwrap_or(false),
        path: node["path"].as_str().map(str::to_owned),
        line: node["line"].as_i64(),
        resolved_by: node["resolvedBy"]["login"].as_str().map(str::to_owned),
        comments_count: node["comments"]["totalCount"].as_i64().unwrap_or(0),
    })
}

fn commit_comment_record(repo_id: i64, c: GhCommitComment) -> CommitCommentRecord {
    CommitCommentRecord {
        id: c.id,
        repo_id,
        commit_sha: c.commit_id,
        path: c.path,
        position: c.position,
        author_login: c.user.map(|u| u.login),
        body: c.body,
        created_at: c.created_at,
        updated_at: c.updated_at,
        html_url: c.html_url,
    }
}

fn issue_event_record(repo_id: i64, e: GhIssueEvent) -> IssueEventRecord {
    IssueEventRecord {
        id: e.id,
        repo_id,
        issue_number: e.issue.map_or(0, |i| i.number),
        event: e.event,
        actor_login: e.actor.map(|a| a.login),
        label_name: e.label.map(|l| l.name),
        assignee_login: e.assignee.map(|a| a.login),
        milestone_title: e.milestone.map(|m| m.title),
        commit_id: e.commit_id,
        created_at: e.created_at,
    }
}

fn deployment_record(repo_id: i64, d: GhDeployment) -> DeploymentRecord {
    DeploymentRecord {
        id: d.id,
        repo_id,
        git_ref: d.git_ref,
        sha: d.sha,
        environment: d.environment,
        task: d.task,
        description: d.description,
        creator_login: d.creator.map(|c| c.login),
        created_at: d.created_at,
        updated_at: d.updated_at,
    }
}

fn pull_request_commit_record(
    repo_id: i64,
    pull_number: i64,
    c: GhCommit,
) -> PullRequestCommitRecord {
    PullRequestCommitRecord {
        repo_id,
        pull_number,
        sha: c.sha,
        message: c.commit.message,
        author_login: c.author.map(|a| a.login),
        committer_login: c.committer.map(|a| a.login),
        authored_at: c.commit.author.and_then(|p| p.date),
        committed_at: c.commit.committer.and_then(|p| p.date),
    }
}

fn commit_status_record(repo_id: i64, commit_sha: &str, s: GhCommitStatus) -> CommitStatusRecord {
    CommitStatusRecord {
        id: s.id,
        repo_id,
        commit_sha: commit_sha.to_owned(),
        state: s.state,
        context: s.context,
        description: s.description,
        target_url: s.target_url,
        creator_login: s.creator.map(|c| c.login),
        created_at: s.created_at,
        updated_at: s.updated_at,
    }
}

fn issue_reaction_record(
    repo_id: i64,
    issue_number: i64,
    r: GhIssueReaction,
) -> IssueReactionRecord {
    IssueReactionRecord {
        id: r.id,
        repo_id,
        issue_number,
        content: r.content,
        user_login: r.user.map(|u| u.login),
        created_at: r.created_at,
    }
}

fn check_run_record(repo_id: i64, c: GhCheckRun) -> CheckRunRecord {
    let (app_slug, app_name) = c.app.map_or((None, None), |a| (a.slug, a.name));
    let (output_title, output_summary, annotations_count) = c.output.map_or((None, None, 0), |o| {
        (o.title, o.summary, o.annotations_count)
    });

    CheckRunRecord {
        id: c.id,
        repo_id,
        head_sha: c.head_sha,
        name: c.name,
        status: c.status,
        conclusion: c.conclusion,
        started_at: c.started_at,
        completed_at: c.completed_at,
        html_url: c.html_url,
        details_url: c.details_url,
        check_suite_id: c.check_suite.map(|s| s.id),
        app_slug,
        app_name,
        output_title,
        output_summary,
        annotations_count,
    }
}

/// Timeline entries have no shared schema, so the record keeps GitHub's
/// object verbatim and lifts only what the mirror indexes on.
fn issue_timeline_record(
    repo_id: i64,
    issue_number: i64,
    position: usize,
    entry: &serde_json::Value,
) -> IssueTimelineEventRecord {
    let string_at = |key: &str| {
        entry
            .get(key)
            .and_then(serde_json::Value::as_str)
            .map(ToOwned::to_owned)
    };
    let login_of = |key: &str| {
        entry
            .get(key)
            .and_then(|v| v.get("login"))
            .and_then(serde_json::Value::as_str)
            .map(ToOwned::to_owned)
    };

    IssueTimelineEventRecord {
        repo_id,
        issue_number,
        position: i64::try_from(position).unwrap_or(i64::MAX),
        event: string_at("event").unwrap_or_else(|| "unknown".to_owned()),
        created_at: string_at("created_at"),
        actor_login: login_of("actor").or_else(|| login_of("user")),
        payload_json: entry.to_string(),
    }
}

fn workflow_job_record(repo_id: i64, j: GhWorkflowJob) -> WorkflowJobRecord {
    WorkflowJobRecord {
        id: j.id,
        repo_id,
        run_id: j.run_id,
        run_attempt: j.run_attempt.unwrap_or(1),
        name: j.name,
        status: j.status,
        conclusion: j.conclusion,
        head_sha: j.head_sha,
        runner_name: j.runner_name,
        started_at: j.started_at,
        completed_at: j.completed_at,
        html_url: j.html_url,
        steps_json: j.steps.map(|s| s.to_string()),
    }
}

fn review_record(repo_id: i64, pull_number: i64, r: GhReview) -> ReviewRecord {
    ReviewRecord {
        id: r.id,
        repo_id,
        pull_number,
        author_login: r.user.map(|u| u.login),
        state: r.state,
        body: r.body,
        commit_id: r.commit_id,
        submitted_at: r.submitted_at,
        html_url: r.html_url,
    }
}

fn commit_record(repo_id: i64, c: GhCommit) -> CommitRecord {
    CommitRecord {
        repo_id,
        sha: c.sha,
        message: c.commit.message,
        author_login: c.author.map(|a| a.login),
        committer_login: c.committer.map(|a| a.login),
        authored_at: c.commit.author.and_then(|p| p.date),
        committed_at: c.commit.committer.and_then(|p| p.date),
        additions: 0,
        deletions: 0,
    }
}

/// The per-commit slices of one sync pass.
struct CommitDetails {
    commit_files: Vec<CommitFileRecord>,
    commit_statuses: Vec<CommitStatusRecord>,
    check_runs: Vec<CheckRunRecord>,
}

/// The per-pull-request slices of one sync pass.
struct PullDetails {
    reviews: Vec<ReviewRecord>,
    pull_request_files: Vec<PullRequestFileRecord>,
    review_threads: Vec<ReviewThreadRecord>,
    pull_request_commits: Vec<PullRequestCommitRecord>,
    /// People seen reviewing, harvested here because the review objects do
    /// not survive the mapping to `ReviewRecord`.
    reviewers: DerivedContributors,
}

impl GithubClient {
    /// Fetch the per-commit detail slice, filling each record's line counts
    /// on the way, for the first `PER_COMMIT_SYNC_CAP` commits.
    async fn fetch_commit_details(
        &self,
        owner: &str,
        name: &str,
        repo_id: i64,
        commit_records: &mut [CommitRecord],
    ) -> Result<CommitDetails, DomainError> {
        let mut commit_files: Vec<CommitFileRecord> = Vec::new();
        let mut commit_statuses: Vec<CommitStatusRecord> = Vec::new();
        let mut check_runs: Vec<CheckRunRecord> = Vec::new();
        for commit in commit_records.iter_mut().take(PER_COMMIT_SYNC_CAP) {
            let detail: GhCommitDetail = self
                .get_json(&format!("/repos/{owner}/{name}/commits/{}", commit.sha))
                .await?;
            if let Some(stats) = detail.stats {
                commit.additions = stats.additions;
                commit.deletions = stats.deletions;
            }
            commit_files.extend(
                detail
                    .files
                    .into_iter()
                    .map(|f| commit_file_record(repo_id, &commit.sha, f)),
            );

            let statuses: Vec<GhCommitStatus> = self
                .get_json(&format!(
                    "/repos/{owner}/{name}/commits/{}/statuses?per_page={FIRST_PAGE_SIZE}",
                    commit.sha
                ))
                .await?;
            commit_statuses.extend(
                statuses
                    .into_iter()
                    .map(|s| commit_status_record(repo_id, &commit.sha, s)),
            );

            let checks: GhCheckRunsPage = self
                .get_json(&format!(
                    "/repos/{owner}/{name}/commits/{}/check-runs?per_page={FIRST_PAGE_SIZE}",
                    commit.sha
                ))
                .await?;
            check_runs.extend(
                checks
                    .check_runs
                    .into_iter()
                    .map(|c| check_run_record(repo_id, c)),
            );
        }

        Ok(CommitDetails {
            commit_files,
            commit_statuses,
            check_runs,
        })
    }

    /// Fetch the timeline of the first `PER_ISSUE_SYNC_CAP` issues. The
    /// entries stay raw JSON: the forty-odd event types share no schema.
    async fn fetch_issue_timeline(
        &self,
        owner: &str,
        name: &str,
        repo_id: i64,
        issues: &[IssueRecord],
    ) -> Result<Vec<IssueTimelineEventRecord>, DomainError> {
        let mut timeline: Vec<IssueTimelineEventRecord> = Vec::new();
        for issue in issues.iter().take(PER_ISSUE_SYNC_CAP) {
            let entries: Vec<serde_json::Value> = self
                .get_json(&format!(
                    "/repos/{owner}/{name}/issues/{}/timeline?per_page={FIRST_PAGE_SIZE}",
                    issue.number
                ))
                .await?;
            timeline.extend(entries.iter().enumerate().map(|(position, entry)| {
                issue_timeline_record(repo_id, issue.number, position, entry)
            }));
        }

        Ok(timeline)
    }

    /// Fetch the reactions of the first `PER_ISSUE_SYNC_CAP` issues; GitHub
    /// only exposes reactions per issue, so there is no repo-wide listing.
    async fn fetch_issue_reactions(
        &self,
        owner: &str,
        name: &str,
        repo_id: i64,
        issues: &[IssueRecord],
    ) -> Result<Vec<IssueReactionRecord>, DomainError> {
        let mut reactions: Vec<IssueReactionRecord> = Vec::new();
        for issue in issues.iter().take(PER_ISSUE_SYNC_CAP) {
            let page: Vec<GhIssueReaction> = self
                .get_json(&format!(
                    "/repos/{owner}/{name}/issues/{}/reactions?per_page={FIRST_PAGE_SIZE}",
                    issue.number
                ))
                .await?;
            reactions.extend(
                page.into_iter()
                    .map(|r| issue_reaction_record(repo_id, issue.number, r)),
            );
        }

        Ok(reactions)
    }

    /// Fetch the jobs of the first `PER_RUN_SYNC_CAP` workflow runs; GitHub
    /// only exposes jobs per run, so there is no repo-wide listing to use.
    async fn fetch_workflow_jobs(
        &self,
        owner: &str,
        name: &str,
        repo_id: i64,
        runs: &[GhWorkflowRun],
    ) -> Result<Vec<WorkflowJobRecord>, DomainError> {
        let mut jobs: Vec<WorkflowJobRecord> = Vec::new();
        for run in runs.iter().take(PER_RUN_SYNC_CAP) {
            let page: GhWorkflowJobsPage = self
                .get_json(&format!(
                    "/repos/{owner}/{name}/actions/runs/{}/jobs?per_page={FIRST_PAGE_SIZE}",
                    run.id
                ))
                .await?;
            jobs.extend(
                page.jobs
                    .into_iter()
                    .map(|j| workflow_job_record(repo_id, j)),
            );
        }

        Ok(jobs)
    }

    /// Fetch the per-pull-request slices for the first
    /// `PER_PULL_SYNC_CAP` pull requests, filling each record's line counts
    /// on the way.
    async fn fetch_pull_details(
        &self,
        owner: &str,
        name: &str,
        repo_id: i64,
        pull_records: &mut [PullRequestRecord],
    ) -> Result<PullDetails, DomainError> {
        let mut reviews: Vec<ReviewRecord> = Vec::new();
        let mut pull_request_files: Vec<PullRequestFileRecord> = Vec::new();
        let mut review_threads: Vec<ReviewThreadRecord> = Vec::new();
        let mut pull_request_commits: Vec<PullRequestCommitRecord> = Vec::new();
        let mut reviewers = DerivedContributors::default();
        for pull in pull_records.iter_mut().take(PER_PULL_SYNC_CAP) {
            let page: Vec<GhReview> = self
                .get_json(&format!(
                    "/repos/{owner}/{name}/pulls/{}/reviews?per_page={FIRST_PAGE_SIZE}",
                    pull.number
                ))
                .await?;
            for review in &page {
                reviewers.track(
                    repo_id,
                    review.user.as_ref(),
                    roles::REVIEWER,
                    review.submitted_at.as_deref(),
                );
            }
            reviews.extend(
                page.into_iter()
                    .map(|r| review_record(repo_id, pull.number, r)),
            );

            let files: Vec<GhPullFile> = self
                .get_json(&format!(
                    "/repos/{owner}/{name}/pulls/{}/files?per_page={FIRST_PAGE_SIZE}",
                    pull.number
                ))
                .await?;
            // Only when the payload did not carry the totals: a full file
            // walk is the fallback, not the source of truth.
            if pull.lines_added == 0 {
                pull.lines_added = files.iter().map(|f| f.additions).sum();
            }
            if pull.lines_removed == 0 {
                pull.lines_removed = files.iter().map(|f| f.deletions).sum();
            }
            pull_request_files.extend(
                files
                    .into_iter()
                    .map(|f| pull_request_file_record(repo_id, pull.number, f)),
            );

            let pull_commits: Vec<GhCommit> = self
                .get_json(&format!(
                    "/repos/{owner}/{name}/pulls/{}/commits?per_page={FIRST_PAGE_SIZE}",
                    pull.number
                ))
                .await?;
            pull_request_commits.extend(
                pull_commits
                    .into_iter()
                    .map(|c| pull_request_commit_record(repo_id, pull.number, c)),
            );

            // A GraphQL failure here must not veto everything already fetched for
            // this repo (REST issues, commits, other pulls' reviews/files/commits,
            // ...): review threads are one supplementary dataset among many, so a
            // failure is logged and this pull's threads are left empty rather than
            // propagated with `?`.
            match self
                .post_graphql(
                    REVIEW_THREADS_QUERY,
                    review_threads_variables(owner, name, pull.number),
                )
                .await
            {
                Ok(threads) => {
                    let nodes =
                        threads["data"]["repository"]["pullRequest"]["reviewThreads"]["nodes"]
                            .as_array()
                            .cloned()
                            .unwrap_or_default();
                    review_threads.extend(
                        nodes
                            .iter()
                            .filter_map(|n| review_thread_record(repo_id, pull.number, n)),
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        owner,
                        name,
                        pull_number = pull.number,
                        error = %e,
                        "review threads (GraphQL) failed for this pull request; sync continues without them"
                    );
                }
            }
        }
        Ok(PullDetails {
            reviews,
            pull_request_files,
            review_threads,
            pull_request_commits,
            reviewers,
        })
    }
}

#[async_trait]
impl GithubPort for GithubClient {
    async fn fetch_repository(
        &self,
        owner: &str,
        name: &str,
    ) -> Result<FetchedRepository, DomainError> {
        let repo: GhRepository = self.get_json(&format!("/repos/{owner}/{name}")).await?;
        let repo_id = repo.id;

        let (issues, issues_complete): (Vec<GhIssue>, bool) = self
            .get_list(&format!(
                "/repos/{owner}/{name}/issues?state=all&per_page={FIRST_PAGE_SIZE}"
            ))
            .await?;
        let (pulls, pulls_complete): (Vec<GhPullRequest>, bool) = self
            .get_list(&format!(
                "/repos/{owner}/{name}/pulls?state=all&per_page={FIRST_PAGE_SIZE}"
            ))
            .await?;
        let (commits, commits_complete): (Vec<GhCommit>, bool) = self
            .get_list(&format!(
                "/repos/{owner}/{name}/commits?per_page={FIRST_PAGE_SIZE}"
            ))
            .await?;
        let (comments, comments_complete): (Vec<GhComment>, bool) = self
            .get_list(&format!(
                "/repos/{owner}/{name}/issues/comments?per_page={FIRST_PAGE_SIZE}"
            ))
            .await?;
        let (review_comments, review_comments_complete): (Vec<GhReviewComment>, bool) = self
            .get_list(&format!(
                "/repos/{owner}/{name}/pulls/comments?per_page={FIRST_PAGE_SIZE}"
            ))
            .await?;

        let (labels, labels_complete): (Vec<GhLabel>, bool) = self
            .get_list(&format!(
                "/repos/{owner}/{name}/labels?per_page={FIRST_PAGE_SIZE}"
            ))
            .await?;

        let (milestones, milestones_complete): (Vec<GhMilestone>, bool) = self
            .get_list(&format!(
                "/repos/{owner}/{name}/milestones?state=all&per_page={FIRST_PAGE_SIZE}"
            ))
            .await?;

        let (releases, releases_complete): (Vec<GhRelease>, bool) = self
            .get_list(&format!(
                "/repos/{owner}/{name}/releases?per_page={FIRST_PAGE_SIZE}"
            ))
            .await?;

        let (branches, branches_complete): (Vec<GhBranch>, bool) = self
            .get_list(&format!(
                "/repos/{owner}/{name}/branches?per_page={FIRST_PAGE_SIZE}"
            ))
            .await?;

        let workflow_runs: GhWorkflowRunsPage = self
            .get_json(&format!(
                "/repos/{owner}/{name}/actions/runs?per_page={FIRST_PAGE_SIZE}"
            ))
            .await?;

        let (deployments, _deployments_complete): (Vec<GhDeployment>, bool) = self
            .get_list(&format!(
                "/repos/{owner}/{name}/deployments?per_page={FIRST_PAGE_SIZE}"
            ))
            .await?;

        let (issue_events, _issue_events_complete): (Vec<GhIssueEvent>, bool) = self
            .get_list(&format!(
                "/repos/{owner}/{name}/issues/events?per_page={FIRST_PAGE_SIZE}"
            ))
            .await?;

        let (commit_comments, _commit_comments_complete): (Vec<GhCommitComment>, bool) = self
            .get_list(&format!(
                "/repos/{owner}/{name}/comments?per_page={FIRST_PAGE_SIZE}"
            ))
            .await?;

        let (tags, tags_complete): (Vec<GhTag>, bool) = self
            .get_list(&format!(
                "/repos/{owner}/{name}/tags?per_page={FIRST_PAGE_SIZE}"
            ))
            .await?;

        // PRD 5.2: contributors are derived from the user objects embedded in
        // the entities above, so the set costs no extra request. Reviewers are
        // added further down, where the review objects are fetched.
        let mut people = derive_people(
            repo_id,
            &issues,
            &pulls,
            &commits,
            &comments,
            &review_comments,
            &commit_comments,
        );

        let issue_records: Vec<IssueRecord> = issues
            .into_iter()
            .map(|i| issue_record(repo_id, i))
            .collect();

        let issue_reactions = self
            .fetch_issue_reactions(owner, name, repo_id, &issue_records)
            .await?;

        let issue_timeline = self
            .fetch_issue_timeline(owner, name, repo_id, &issue_records)
            .await?;

        let mut commit_records: Vec<CommitRecord> = commits
            .into_iter()
            .map(|c| commit_record(repo_id, c))
            .collect();

        let workflow_jobs = self
            .fetch_workflow_jobs(owner, name, repo_id, &workflow_runs.workflow_runs)
            .await?;

        let CommitDetails {
            commit_files,
            commit_statuses,
            check_runs,
        } = self
            .fetch_commit_details(owner, name, repo_id, &mut commit_records)
            .await?;

        let mut pull_records: Vec<PullRequestRecord> = pulls
            .into_iter()
            .map(|p| pull_request_record(repo_id, p))
            .collect();

        let PullDetails {
            reviews,
            pull_request_files,
            review_threads,
            pull_request_commits,
            reviewers,
        } = self
            .fetch_pull_details(owner, name, repo_id, &mut pull_records)
            .await?;
        people.absorb(reviewers);

        let mut complete = ListingCompleteness::none();
        complete.set(Listing::Issues, issues_complete);
        complete.set(Listing::PullRequests, pulls_complete);
        complete.set(Listing::Commits, commits_complete);
        complete.set(Listing::Comments, comments_complete);
        complete.set(Listing::ReviewComments, review_comments_complete);
        complete.set(Listing::Labels, labels_complete);
        complete.set(Listing::Milestones, milestones_complete);
        complete.set(Listing::Releases, releases_complete);
        complete.set(Listing::Branches, branches_complete);
        complete.set(Listing::Tags, tags_complete);

        Ok(FetchedRepository {
            repository: repository_record(repo),
            complete,
            issues: issue_records,
            pull_requests: pull_records,
            commits: commit_records,
            comments: comments
                .into_iter()
                .filter_map(|c| comment_record(repo_id, c))
                .collect(),
            review_comments: review_comments
                .into_iter()
                .filter_map(|c| review_comment_record(repo_id, c))
                .collect(),
            reviews,
            labels: labels
                .into_iter()
                .map(|l| label_record(repo_id, l))
                .collect(),
            milestones: milestones
                .into_iter()
                .map(|m| milestone_record(repo_id, m))
                .collect(),
            releases: releases
                .into_iter()
                .map(|r| release_record(repo_id, r))
                .collect(),
            branches: branches
                .into_iter()
                .map(|b| branch_record(repo_id, b))
                .collect(),
            contributors: people.into_records(),
            workflow_runs: workflow_runs
                .workflow_runs
                .into_iter()
                .map(|w| workflow_run_record(repo_id, w))
                .collect(),
            pull_request_files,
            tags: tags.into_iter().map(|t| tag_record(repo_id, t)).collect(),
            commit_files,
            review_threads,
            commit_comments: commit_comments
                .into_iter()
                .map(|c| commit_comment_record(repo_id, c))
                .collect(),
            issue_events: issue_events
                .into_iter()
                .map(|e| issue_event_record(repo_id, e))
                .collect(),
            deployments: deployments
                .into_iter()
                .map(|d| deployment_record(repo_id, d))
                .collect(),
            pull_request_commits,
            commit_statuses,
            workflow_jobs,
            issue_reactions,
            check_runs,
            issue_timeline,
        })
    }
}
