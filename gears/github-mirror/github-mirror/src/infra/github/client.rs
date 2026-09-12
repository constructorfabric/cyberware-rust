use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use tokio::sync::{Semaphore, SemaphorePermit};
use tokio::time::Instant;

use crate::domain::error::DomainError;
use crate::domain::ports::github::{
    ActionsListing, CommitDetail, CommitListing, DeclaredCounts, FetchOptions, GithubPort,
    IssueDetail, IssueDetailWants, IssueListing, Listing, MetadataListing, PullDetail, PullListing,
};
use crate::domain::repo::{
    BranchRecord, CheckRunRecord, CommentRecord, CommitCommentRecord, CommitFileRecord,
    CommitRecord, CommitStatusRecord, ContributorRecord, DeploymentRecord, IssueEventRecord,
    IssueReactionRecord, IssueRecord, IssueTimelineEventRecord, LabelRecord, MilestoneRecord,
    PullRequestCommitRecord, PullRequestFileRecord, PullRequestRecord, ReleaseRecord, RepoRecord,
    ReviewCommentRecord, ReviewRecord, ReviewThreadRecord, TagRecord, WorkflowJobRecord,
    WorkflowRunRecord,
};
use crate::infra::github::cache::{CacheKey, CachedResponse, HttpCache, NoCache};
use crate::infra::github::pagination::parse_link_next;
use crate::redact::redacted_word;

/// Items asked for per request. GitHub's maximum, so a listing of a given
/// size costs the fewest requests.
const FIRST_PAGE_SIZE: u32 = 100;
const ACCEPT_JSON: &str = "application/vnd.github+json";

fn within_since(state: &str, updated_at: &str, since: Option<DateTime<Utc>>) -> bool {
    let Some(since) = since else {
        return true;
    };
    if state == "open" {
        return true;
    }
    DateTime::parse_from_rfc3339(updated_at).is_ok_and(|at| at.with_timezone(&Utc) >= since)
}

fn updated_after_param(updated_after: Option<DateTime<Utc>>) -> String {
    updated_after.map_or_else(String::new, |at| {
        format!(
            "&since={}",
            at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
        )
    })
}

/// One fetched page: the decoded body, the `rel="next"` link if the listing
/// continues, and the response's `ETag`.
struct FetchedPage<T> {
    parsed: T,
    next: Option<String>,
    etag: Option<String>,
}

/// One listing an Indexing family walks: the path it ends with, and its
/// first page. The URL to continue from is matched back to its stage by that
/// path tail rather than by the whole URL, because GitHub's `rel="next"` links may name the
/// repository by id instead of by owner and name.
struct Stage {
    tail: &'static str,
    first: String,
}

fn stage_of(stages: &[Stage], url: &str) -> Result<usize, DomainError> {
    let path = url.split('?').next().unwrap_or(url);
    stages
        .iter()
        .position(|stage| path.ends_with(stage.tail))
        .ok_or_else(|| {
            DomainError::internal(format!(
                "cannot continue from an unknown listing URL: {}",
                redacted_word(url)
            ))
        })
}

/// Where to continue after a page of `stage`: the page's own `rel="next"`,
/// else the first page of the following stage, else nothing.
fn continue_after(stages: &[Stage], stage: usize, page_next: Option<String>) -> Option<String> {
    page_next.or_else(|| stages.get(stage + 1).map(|next| next.first.clone()))
}

const USER_AGENT: &str = concat!("cf-gears-github-mirror/", env!("CARGO_PKG_VERSION"));

/// The REST API version every request pins (DESIGN 3.5). Without it the
/// response schema follows GitHub's default, which can change under us.
const GITHUB_API_VERSION: &str = "2022-11-28";
/// Times in a row GitHub may answer one request with a rate limit before the
/// task gives up. A limit is a wait, not an error, so this is generous: at
/// [`MAX_RETRY_SLEEP`] a request rides out a whole hourly window.
const RATE_LIMIT_RETRIES: u32 = 30;
/// Requests in flight a client allows before the gear config says otherwise.
/// Matches the PRD's "parallelism <= 8" rate-limit threshold.
const DEFAULT_MAX_CONCURRENT_REQUESTS: usize = 8;
/// Longest single back-off sleep, whatever `Retry-After` or the reset stamp
/// asks for; an exhausted hourly window is re-checked at this pace.
const MAX_RETRY_SLEEP: std::time::Duration = std::time::Duration::from_mins(5);

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
    std::time::Duration::from_secs(seconds).min(MAX_RETRY_SLEEP)
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

/// The `rel="next"` URL from a response's `Link` header, if it advertises one.
fn next_link(headers: &reqwest::header::HeaderMap) -> Option<String> {
    header_string(headers, "link")
        .as_deref()
        .and_then(parse_link_next)
}

/// GitHub REST client for the mirror (gears-rust#4630).
///
/// Conditional requests and `Link`-header pagination are in; per-token
/// rate-limit admission is not. The token comes from gear config as a
/// temporary shortcut until credstore integration (#4534).
pub struct GithubClient {
    http: reqwest::Client,
    api_base_url: String,
    token: Option<String>,
    cache: Arc<dyn HttpCache>,
    /// Ceiling on requests in flight, shared by every sync using this client.
    /// GitHub's secondary rate limit reacts to concurrency, so the ceiling is
    /// global rather than per sync.
    permits: Semaphore,
    /// Instant before which no request may be sent: set when any request is
    /// told to back off, so one rate limit pauses every task at once instead
    /// of each discovering it in turn.
    cooldown_until: Mutex<Option<Instant>>,
}

impl GithubClient {
    /// # Errors
    /// Returns `DomainError::Internal` when the underlying HTTP client cannot
    /// be constructed.
    pub fn new(api_base_url: String, token: Option<String>) -> Result<Self, DomainError> {
        Self::with_cache(api_base_url, token, Arc::new(NoCache))
    }

    /// A client that revalidates against `cache` instead of re-fetching.
    ///
    /// # Errors
    /// Returns `DomainError::Internal` when the underlying HTTP client cannot
    /// be constructed.
    pub fn with_cache(
        api_base_url: String,
        token: Option<String>,
        cache: Arc<dyn HttpCache>,
    ) -> Result<Self, DomainError> {
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
            cache,
            permits: Semaphore::new(DEFAULT_MAX_CONCURRENT_REQUESTS),
            cooldown_until: Mutex::new(None),
        })
    }

    /// Cap the requests this client keeps in flight at `max` (zero reads as
    /// one, so the client always makes progress).
    #[must_use]
    pub fn with_max_concurrent_requests(mut self, max: usize) -> Self {
        self.permits = Semaphore::new(max.max(1));
        self
    }

    /// A permit for one outbound request, held until the response body has
    /// been read.
    async fn request_permit(&self) -> Result<SemaphorePermit<'_>, DomainError> {
        self.wait_out_cooldown().await;
        self.permits
            .acquire()
            .await
            .map_err(|e| DomainError::internal(format!("request semaphore closed: {e}")))
    }

    /// Sleep until the shared cooldown has passed, re-checking in case another
    /// request pushed it further while this one slept.
    async fn wait_out_cooldown(&self) {
        loop {
            let deadline = *self
                .cooldown_until
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            match deadline {
                Some(until) if until > Instant::now() => tokio::time::sleep_until(until).await,
                _ => return,
            }
        }
    }

    /// Push the shared cooldown out to at least `delay` from now.
    fn extend_cooldown(&self, delay: std::time::Duration) {
        let deadline = Instant::now() + delay;
        let mut slot = self
            .cooldown_until
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        *slot = Some(slot.map_or(deadline, |current| current.max(deadline)));
    }

    /// The stored entry for this request, unless `force` says to ignore it.
    ///
    /// A cache read that fails is a warning, not an error: the worst case is a
    /// full fetch, which is what would have happened anyway.
    async fn cached_entry(
        &self,
        options: &FetchOptions,
        url: &str,
        key: &CacheKey,
    ) -> Option<CachedResponse> {
        if options.force {
            return None;
        }
        match self.cache.get(options.tenant_id, key).await {
            Ok(entry) => entry,
            Err(e) => {
                tracing::warn!(%url, error = %e, "cache read failed; fetching fresh");
                None
            }
        }
    }

    /// The GET request, carrying auth and whichever validator the entry holds.
    fn conditional_request(
        &self,
        url: &str,
        cached: Option<&CachedResponse>,
    ) -> reqwest::RequestBuilder {
        let mut request = self
            .http
            .get(url)
            .header("Accept", ACCEPT_JSON)
            .header("X-GitHub-Api-Version", GITHUB_API_VERSION);
        if let Some(token) = &self.token {
            request = request.bearer_auth(token);
        }
        match cached {
            Some(CachedResponse {
                etag: Some(etag), ..
            }) => request.header("If-None-Match", etag.clone()),
            Some(CachedResponse {
                last_modified: Some(modified),
                ..
            }) => request.header("If-Modified-Since", modified.clone()),
            _ => request,
        }
    }

    /// One response: what it parsed to, plus the `rel="next"` URL if the list
    /// continues.
    ///
    /// A rate limit (429, or a 403 carrying rate-limit headers) puts the whole
    /// client into a shared cooldown for as long as GitHub asks, then the
    /// request is retried; only [`RATE_LIMIT_RETRIES`] refusals in a row fail
    /// it. Adaptive concurrency (ADR-0003) is #4630's remaining half.
    async fn get_page<T: serde::de::DeserializeOwned>(
        &self,
        url: &str,
        options: &FetchOptions,
    ) -> Result<FetchedPage<T>, DomainError> {
        let key = CacheKey::compute("GET", url, ACCEPT_JSON);
        let cached = self.cached_entry(options, url, &key).await;

        let mut attempt: u32 = 0;
        // `_permit` lives until this function returns, so a request counts
        // against the ceiling until its body has been read. A retry gives its
        // permit up first: a request asleep on a backoff is not in flight.
        let (response, rate_limited, _permit) = loop {
            let permit = self.request_permit().await?;
            let response = self
                .conditional_request(url, cached.as_ref())
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
                    url = %redacted_word(url),
                    %status,
                    attempt,
                    delay_secs = delay.as_secs(),
                    "GitHub rate limit hit; backing off before retrying"
                );
                drop(permit);
                self.extend_cooldown(delay);
                attempt += 1;
                continue;
            }
            break (response, rate_limited, permit);
        };

        let status = response.status();
        if status == reqwest::StatusCode::NOT_MODIFIED {
            return Self::serve_from_cache(url, cached.as_ref(), response.headers());
        }
        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(DomainError::NotFound);
        }
        if rate_limited {
            return Err(DomainError::internal(format!(
                "GitHub rate limit persisted through {RATE_LIMIT_RETRIES} retries for {url}"
            )));
        }
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            // Not a rate limit (checked above): the mirror's own token no
            // longer sees this resource - the repo went private, the token
            // was revoked, or its scopes shrank.
            return Err(DomainError::AccessLost(format!(
                "GitHub answered {status} for {url}"
            )));
        }
        if !status.is_success() {
            return Err(DomainError::internal(format!(
                "GitHub responded with {status} for {url}"
            )));
        }

        let etag = header_string(response.headers(), "etag");
        let last_modified = header_string(response.headers(), "last-modified");
        let next_page = next_link(response.headers());
        let entry = CachedResponse {
            body: response
                .text()
                .await
                .map_err(|e| DomainError::internal(format!("GitHub response read failed: {e}")))?,
            etag,
            last_modified,
            next_page,
        };

        let parsed = serde_json::from_str(&entry.body)
            .map_err(|e| DomainError::internal(format!("GitHub response decode failed: {e}")))?;
        let next = entry.next_page.clone();
        let etag = entry.etag.clone();
        self.remember(options, url, &key, entry).await;
        Ok(FetchedPage { parsed, next, etag })
    }

    /// Serve a `304` from the stored entry.
    ///
    /// The `Link` header on the `304` wins when GitHub sends one; otherwise the
    /// entry's stored `next` is used, because losing it would silently truncate
    /// the listing to the pages already walked.
    fn serve_from_cache<T: serde::de::DeserializeOwned>(
        url: &str,
        cached: Option<&CachedResponse>,
        headers: &reqwest::header::HeaderMap,
    ) -> Result<FetchedPage<T>, DomainError> {
        let entry = cached.ok_or_else(|| {
            DomainError::internal(format!("GitHub answered 304 for {url} with nothing cached"))
        })?;
        tracing::debug!(%url, "304 Not Modified - served from cache, no quota spent");

        let parsed = serde_json::from_str(&entry.body).map_err(|e| {
            DomainError::internal(format!(
                "cached GitHub body for {url} no longer parses: {e}"
            ))
        })?;
        let next = next_link(headers).or_else(|| entry.next_page.clone());
        let etag = header_string(headers, "etag").or_else(|| entry.etag.clone());
        Ok(FetchedPage { parsed, next, etag })
    }

    /// GET `path`, revalidating against the cache when possible.
    ///
    /// A stored `ETag` is replayed as `If-None-Match`; GitHub answers `304`
    /// without charging a rate-limit unit and the cached body is returned.
    /// `options.force` skips the validator so the response is always fresh.
    async fn get_json<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        options: &FetchOptions,
    ) -> Result<T, DomainError> {
        let url = format!("{}{path}", self.api_base_url.trim_end_matches('/'));
        let fetched = self.get_page(&url, options).await?;
        Ok(fetched.parsed)
    }

    fn absolute(&self, path: &str) -> String {
        format!("{}{path}", self.api_base_url.trim_end_matches('/'))
    }

    /// GET `path` and every page after it, concatenated, plus whether the
    /// listing was walked to its end.
    ///
    /// Follows the `Link` header's `rel="next"` until it stops appearing.
    /// Without this a listing is silently truncated to whatever fits in one
    /// page, which is the single most misleading way a mirror can be wrong.
    /// The completeness flag is what lets a sync reconcile deletions: rows may
    /// only be removed for a listing that ran out of pages. Only the small
    /// families come through here; issues, pull requests and commits stream
    /// one page per port call instead, so a large repository is never held
    /// whole.
    async fn get_json_all<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        options: &FetchOptions,
    ) -> Result<(Vec<T>, bool), DomainError> {
        let mut url = self.absolute(path);
        let mut items: Vec<T> = Vec::new();
        loop {
            let fetched: FetchedPage<Vec<T>> = self.get_page(&url, options).await?;
            let mut batch = fetched.parsed;
            items.append(&mut batch);
            match fetched.next {
                Some(next) => url = next,
                None => return Ok((items, true)),
            }
        }
    }

    async fn get_json_all_wrapped<P: serde::de::DeserializeOwned, T>(
        &self,
        path: &str,
        options: &FetchOptions,
        unwrap: impl Fn(P) -> Vec<T>,
    ) -> Result<Vec<T>, DomainError> {
        let mut url = self.absolute(path);
        let mut items: Vec<T> = Vec::new();
        loop {
            let fetched: FetchedPage<P> = self.get_page(&url, options).await?;
            items.extend(unwrap(fetched.parsed));
            match fetched.next {
                Some(next) => url = next,
                None => return Ok(items),
            }
        }
    }

    /// Store a fresh response so the next request can revalidate it.
    ///
    /// Entries without a validator are dropped: the next request could not
    /// revalidate them and would re-fetch anyway, so keeping the body only
    /// costs storage. A failed write is a warning for the same reason.
    async fn remember(
        &self,
        options: &FetchOptions,
        url: &str,
        key: &CacheKey,
        entry: CachedResponse,
    ) {
        if !entry.is_revalidatable() {
            return;
        }
        if let Err(e) = self.cache.put(options.tenant_id, key, url, entry).await {
            tracing::warn!(%url, error = %e, "cache write failed; the next sync will re-fetch");
        }
    }

    async fn post_graphql(
        &self,
        query: &str,
        variables: serde_json::Value,
    ) -> Result<serde_json::Value, DomainError> {
        let url = format!("{}/graphql", self.api_base_url.trim_end_matches('/'));

        let mut attempt: u32 = 0;
        // GraphQL shares the REST ceiling: both spend the same token's budget.
        let (response, _permit) = loop {
            let permit = self.request_permit().await?;
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
                drop(permit);
                self.extend_cooldown(delay);
                attempt += 1;
                continue;
            }
            break (response, permit);
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
    #[serde(default, deserialize_with = "actor_or_none")]
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
    #[serde(default, deserialize_with = "actor_or_none")]
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
    #[serde(default, deserialize_with = "actor_or_none")]
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
    #[serde(default)]
    commits: Option<i64>,
    #[serde(default)]
    changed_files: Option<i64>,
    #[serde(default)]
    review_comments: Option<i64>,
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

fn actor_or_none<'de, D>(deserializer: D) -> Result<Option<GhActor>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value: Option<serde_json::Value> = Option::deserialize(deserializer)?;
    value
        .filter(|actor| actor.get("login").is_some_and(serde_json::Value::is_string))
        .map(GhActor::deserialize)
        .transpose()
        .map_err(serde::de::Error::custom)
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
    #[serde(default, deserialize_with = "actor_or_none")]
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
    #[serde(default, deserialize_with = "actor_or_none")]
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
    #[serde(default, deserialize_with = "actor_or_none")]
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
    #[serde(default, deserialize_with = "actor_or_none")]
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

/// `GET /repos/{owner}/{name}/commits/{sha}`: the listing entry's fields plus
/// the stats and changed files only the detail endpoint carries.
#[derive(Debug, Deserialize)]
struct GhCommitDetail {
    #[serde(flatten)]
    base: GhCommit,
    stats: Option<GhCommitStats>,
    #[serde(default)]
    files: Vec<GhPullFile>,
}

#[derive(Debug, Deserialize)]
struct GhCommitComment {
    id: i64,
    #[serde(default, deserialize_with = "actor_or_none")]
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
    #[serde(default, deserialize_with = "actor_or_none")]
    actor: Option<GhActor>,
    #[serde(default)]
    label: Option<GhEventLabel>,
    #[serde(default, deserialize_with = "actor_or_none")]
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
    #[serde(default, deserialize_with = "actor_or_none")]
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
    #[serde(default, deserialize_with = "actor_or_none")]
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
    #[serde(default, deserialize_with = "actor_or_none")]
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
    #[serde(default, deserialize_with = "actor_or_none")]
    author: Option<GhActor>,
    #[serde(default, deserialize_with = "actor_or_none")]
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

/// PRD 5.2's derivation, split the way the fetch is: each family harvests the
/// people out of the entities it downloaded, and `fetch_repository` folds the
/// three together. No `/contributors` request, which PRD 5.2 forbids.
fn derive_issue_people(
    repo_id: i64,
    issues: &[GhIssue],
    comments: &[GhComment],
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
    for comment in comments {
        people.track(
            repo_id,
            comment.user.as_ref(),
            roles::COMMENTER,
            Some(&comment.created_at),
        );
    }
    people
}

/// The pull-request half: authors, assignees, requested reviewers, and the
/// people who left inline comments. Reviewers who actually submitted a review
/// join from `fetch_pull_details`.
fn derive_pull_people(
    repo_id: i64,
    pulls: &[GhPullRequest],
    review_comments: &[GhReviewComment],
) -> DerivedContributors {
    let mut people = DerivedContributors::default();
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
    for comment in review_comments {
        people.track(
            repo_id,
            comment.user.as_ref(),
            roles::COMMENTER,
            Some(&comment.created_at),
        );
    }
    people
}

/// The commit half: the GitHub accounts behind `author` and `committer`, plus
/// commit commenters.
fn derive_commit_people(
    repo_id: i64,
    commits: &[GhCommit],
    commit_comments: &[GhCommitComment],
) -> DerivedContributors {
    let mut people = DerivedContributors::default();
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

#[async_trait]
impl GithubPort for GithubClient {
    async fn fetch_repository_metadata(
        &self,
        owner: &str,
        name: &str,
        options: &FetchOptions,
    ) -> Result<RepoRecord, DomainError> {
        let repo: GhRepository = self
            .get_json(&format!("/repos/{owner}/{name}"), options)
            .await?;
        Ok(repository_record(repo))
    }

    /// Issues plus the repo-wide comment and event listings. Reactions and
    /// the timeline are per-issue sub-resources and dominate the call count,
    /// so they belong to [`Self::refine_issue`].
    async fn list_issues(
        &self,
        owner: &str,
        name: &str,
        repo_id: i64,
        updated_after: Option<DateTime<Utc>>,
        page1_etag: Option<&str>,
        continue_from: Option<&str>,
        options: &FetchOptions,
    ) -> Result<IssueListing, DomainError> {
        if !options.scope.objects.issues {
            return Ok(IssueListing::default());
        }
        let bound = updated_after_param(updated_after);
        let stages = [
            Stage {
                tail: "/issues",
                first: self.absolute(&format!(
                    "/repos/{owner}/{name}/issues?state=all&sort=updated&direction=desc&per_page={FIRST_PAGE_SIZE}{bound}"
                )),
            },
            Stage {
                tail: "/issues/comments",
                first: self.absolute(&format!(
                    "/repos/{owner}/{name}/issues/comments?per_page={FIRST_PAGE_SIZE}{bound}"
                )),
            },
            Stage {
                tail: "/issues/events",
                first: self.absolute(&format!(
                    "/repos/{owner}/{name}/issues/events?per_page={FIRST_PAGE_SIZE}"
                )),
            },
        ];
        let url = continue_from.map_or_else(|| stages[0].first.clone(), str::to_owned);
        let stage = stage_of(&stages, &url)?;
        let bounded = updated_after.is_some() || options.since.is_some();

        let mut listing = IssueListing::default();
        match stage {
            0 => {
                let page: FetchedPage<Vec<GhIssue>> = self.get_page(&url, options).await?;
                if continue_from.is_none() {
                    listing.page1_etag.clone_from(&page.etag);
                    if page1_etag.is_some() && page.etag.as_deref() == page1_etag {
                        tracing::debug!(%url, "page one is unchanged; the sweep stops here");
                        listing.unchanged = true;
                        return Ok(listing);
                    }
                }
                listing.contributors =
                    derive_issue_people(repo_id, &page.parsed, &[]).into_records();
                listing.issues = page
                    .parsed
                    .into_iter()
                    .map(|i| issue_record(repo_id, i))
                    .filter(|i| within_since(&i.state, &i.updated_at, options.since))
                    .collect();
                listing
                    .complete
                    .set(Listing::Issues, page.next.is_none() && !bounded);
                listing.next = continue_after(&stages, stage, page.next);
            }
            1 => {
                let page: FetchedPage<Vec<GhComment>> = self.get_page(&url, options).await?;
                listing.contributors =
                    derive_issue_people(repo_id, &[], &page.parsed).into_records();
                listing.comments = page
                    .parsed
                    .into_iter()
                    .filter_map(|c| comment_record(repo_id, c))
                    .collect();
                listing
                    .complete
                    .set(Listing::Comments, page.next.is_none() && !bounded);
                listing.next = continue_after(&stages, stage, page.next);
            }
            _ => {
                let page: FetchedPage<Vec<GhIssueEvent>> = self.get_page(&url, options).await?;
                listing.issue_events = page
                    .parsed
                    .into_iter()
                    .map(|e| issue_event_record(repo_id, e))
                    .collect();
                listing.next = continue_after(&stages, stage, page.next);
            }
        }
        listing.swept_to_end = listing.next.is_none();
        Ok(listing)
    }

    async fn refine_issue(
        &self,
        owner: &str,
        name: &str,
        repo_id: i64,
        number: i64,
        wants: IssueDetailWants,
        options: &FetchOptions,
    ) -> Result<IssueDetail, DomainError> {
        let mut detail = IssueDetail {
            issue_number: number,
            ..IssueDetail::default()
        };
        if wants.reactions {
            let (page, _): (Vec<GhIssueReaction>, bool) = self
                .get_json_all(
                    &format!(
                        "/repos/{owner}/{name}/issues/{number}/reactions?per_page={FIRST_PAGE_SIZE}"
                    ),
                    options,
                )
                .await?;
            detail.reactions = page
                .into_iter()
                .map(|r| issue_reaction_record(repo_id, number, r))
                .collect();
        }
        if wants.timeline {
            // The entries stay raw JSON: the forty-odd event types share no
            // schema.
            let (entries, _): (Vec<serde_json::Value>, bool) = self
                .get_json_all(
                    &format!(
                        "/repos/{owner}/{name}/issues/{number}/timeline?per_page={FIRST_PAGE_SIZE}"
                    ),
                    options,
                )
                .await?;
            detail.timeline = entries
                .iter()
                .enumerate()
                .map(|(position, entry)| issue_timeline_record(repo_id, number, position, entry))
                .collect();
        }
        Ok(detail)
    }

    async fn list_pull_requests(
        &self,
        owner: &str,
        name: &str,
        repo_id: i64,
        page1_etag: Option<&str>,
        continue_from: Option<&str>,
        options: &FetchOptions,
    ) -> Result<PullListing, DomainError> {
        if !options.scope.objects.pull_requests {
            return Ok(PullListing::default());
        }
        let stages = [
            Stage {
                tail: "/pulls",
                first: self.absolute(&format!(
                    "/repos/{owner}/{name}/pulls?state=all&sort=updated&direction=desc&per_page={FIRST_PAGE_SIZE}"
                )),
            },
            Stage {
                tail: "/pulls/comments",
                first: self.absolute(&format!(
                    "/repos/{owner}/{name}/pulls/comments?per_page={FIRST_PAGE_SIZE}"
                )),
            },
        ];
        let url = continue_from.map_or_else(|| stages[0].first.clone(), str::to_owned);
        let stage = stage_of(&stages, &url)?;
        let bounded = options.since.is_some();

        let mut listing = PullListing::default();
        if stage == 0 {
            let page: FetchedPage<Vec<GhPullRequest>> = self.get_page(&url, options).await?;
            if continue_from.is_none() {
                listing.page1_etag.clone_from(&page.etag);
                if page1_etag.is_some() && page.etag.as_deref() == page1_etag {
                    tracing::debug!(%url, "page one is unchanged; the sweep stops here");
                    listing.unchanged = true;
                    return Ok(listing);
                }
            }
            listing.contributors = derive_pull_people(repo_id, &page.parsed, &[]).into_records();
            listing.pull_requests = page
                .parsed
                .into_iter()
                .map(|p| pull_request_record(repo_id, p))
                .filter(|p| within_since(&p.state, &p.updated_at, options.since))
                .collect();
            listing
                .complete
                .set(Listing::PullRequests, page.next.is_none() && !bounded);
            listing.next = continue_after(&stages, stage, page.next);
        } else {
            let page: FetchedPage<Vec<GhReviewComment>> = self.get_page(&url, options).await?;
            listing.contributors = derive_pull_people(repo_id, &[], &page.parsed).into_records();
            listing.review_comments = page
                .parsed
                .into_iter()
                .filter_map(|c| review_comment_record(repo_id, c))
                .collect();
            listing
                .complete
                .set(Listing::ReviewComments, page.next.is_none() && !bounded);
            listing.next = continue_after(&stages, stage, page.next);
        }
        listing.swept_to_end = listing.next.is_none();
        Ok(listing)
    }

    /// The pull's own detail record — the per-pull payload carries the line
    /// counts the listing omits — plus its reviews, files, commits and
    /// review threads.
    async fn refine_pull_request(
        &self,
        owner: &str,
        name: &str,
        repo_id: i64,
        number: i64,
        options: &FetchOptions,
    ) -> Result<PullDetail, DomainError> {
        let pull: GhPullRequest = self
            .get_json(&format!("/repos/{owner}/{name}/pulls/{number}"), options)
            .await?;
        let declared = DeclaredCounts {
            commits: pull.commits,
            files: pull.changed_files,
            review_comments: pull.review_comments,
        };
        let mut pull_request = pull_request_record(repo_id, pull);

        let mut reviewers = DerivedContributors::default();
        let (page, _): (Vec<GhReview>, bool) = self
            .get_json_all(
                &format!("/repos/{owner}/{name}/pulls/{number}/reviews?per_page={FIRST_PAGE_SIZE}"),
                options,
            )
            .await?;
        for review in &page {
            reviewers.track(
                repo_id,
                review.user.as_ref(),
                roles::REVIEWER,
                review.submitted_at.as_deref(),
            );
        }
        let reviews = page
            .into_iter()
            .map(|r| review_record(repo_id, number, r))
            .collect();

        let (files, _): (Vec<GhPullFile>, bool) = self
            .get_json_all(
                &format!("/repos/{owner}/{name}/pulls/{number}/files?per_page={FIRST_PAGE_SIZE}"),
                options,
            )
            .await?;
        // Only when the payload did not carry the totals: a full file walk is
        // the fallback, not the source of truth.
        if pull_request.lines_added == 0 {
            pull_request.lines_added = files.iter().map(|f| f.additions).sum();
        }
        if pull_request.lines_removed == 0 {
            pull_request.lines_removed = files.iter().map(|f| f.deletions).sum();
        }
        let files = files
            .into_iter()
            .map(|f| pull_request_file_record(repo_id, number, f))
            .collect();

        let (pull_commits, _): (Vec<GhCommit>, bool) = self
            .get_json_all(
                &format!("/repos/{owner}/{name}/pulls/{number}/commits?per_page={FIRST_PAGE_SIZE}"),
                options,
            )
            .await?;
        let commits = pull_commits
            .into_iter()
            .map(|c| pull_request_commit_record(repo_id, number, c))
            .collect();

        // A GraphQL failure must not veto the REST data already fetched for
        // this pull: review threads are one supplementary dataset among many,
        // so a failure is logged and the threads are left empty.
        let review_threads = match self
            .post_graphql(
                REVIEW_THREADS_QUERY,
                review_threads_variables(owner, name, number),
            )
            .await
        {
            Ok(threads) => threads["data"]["repository"]["pullRequest"]["reviewThreads"]["nodes"]
                .as_array()
                .map(|nodes| {
                    nodes
                        .iter()
                        .filter_map(|n| review_thread_record(repo_id, number, n))
                        .collect()
                })
                .unwrap_or_default(),
            Err(e) => {
                tracing::warn!(
                    owner,
                    name,
                    pull_number = number,
                    error = %e,
                    "review threads (GraphQL) failed for this pull request; sync continues without them"
                );
                Vec::new()
            }
        };

        Ok(PullDetail {
            pull_request,
            reviews,
            files,
            commits,
            review_threads,
            declared,
            contributors: reviewers.into_records(),
        })
    }

    #[allow(clippy::too_many_arguments)]
    async fn list_commits(
        &self,
        owner: &str,
        name: &str,
        repo_id: i64,
        updated_after: Option<DateTime<Utc>>,
        page1_etag: Option<&str>,
        continue_from: Option<&str>,
        options: &FetchOptions,
    ) -> Result<CommitListing, DomainError> {
        if !options.scope.objects.commits {
            return Ok(CommitListing::default());
        }
        let bound = updated_after_param(updated_after);
        let stages = [
            Stage {
                tail: "/commits",
                first: self.absolute(&format!(
                    "/repos/{owner}/{name}/commits?per_page={FIRST_PAGE_SIZE}{bound}"
                )),
            },
            Stage {
                tail: "/comments",
                first: self.absolute(&format!(
                    "/repos/{owner}/{name}/comments?per_page={FIRST_PAGE_SIZE}"
                )),
            },
        ];
        let url = continue_from.map_or_else(|| stages[0].first.clone(), str::to_owned);
        let stage = stage_of(&stages, &url)?;
        let bounded = updated_after.is_some();

        let mut listing = CommitListing::default();
        if stage == 0 {
            let page: FetchedPage<Vec<GhCommit>> = self.get_page(&url, options).await?;
            if continue_from.is_none() {
                listing.page1_etag.clone_from(&page.etag);
                if page1_etag.is_some() && page.etag.as_deref() == page1_etag {
                    tracing::debug!(%url, "page one is unchanged; the sweep stops here");
                    listing.unchanged = true;
                    return Ok(listing);
                }
            }
            listing.contributors = derive_commit_people(repo_id, &page.parsed, &[]).into_records();
            listing.commits = page
                .parsed
                .into_iter()
                .map(|c| commit_record(repo_id, c))
                .collect();
            listing
                .complete
                .set(Listing::Commits, page.next.is_none() && !bounded);
            listing.next = continue_after(&stages, stage, page.next);
        } else {
            let page: FetchedPage<Vec<GhCommitComment>> = self.get_page(&url, options).await?;
            listing.contributors = derive_commit_people(repo_id, &[], &page.parsed).into_records();
            listing.commit_comments = page
                .parsed
                .into_iter()
                .map(|c| commit_comment_record(repo_id, c))
                .collect();
            listing.next = continue_after(&stages, stage, page.next);
        }
        listing.swept_to_end = listing.next.is_none();
        Ok(listing)
    }

    /// The per-commit detail: the record with its stats, its files, and —
    /// when CI is in scope — its statuses and check runs.
    async fn refine_commit(
        &self,
        owner: &str,
        name: &str,
        repo_id: i64,
        sha: &str,
        with_ci: bool,
        options: &FetchOptions,
    ) -> Result<CommitDetail, DomainError> {
        let detail: GhCommitDetail = self
            .get_json(&format!("/repos/{owner}/{name}/commits/{sha}"), options)
            .await?;
        let mut commit = commit_record(repo_id, detail.base);
        if let Some(stats) = detail.stats {
            commit.additions = stats.additions;
            commit.deletions = stats.deletions;
        }
        let files = detail
            .files
            .into_iter()
            .map(|f| commit_file_record(repo_id, sha, f))
            .collect();

        let (mut statuses, mut check_runs) = (Vec::new(), Vec::new());
        if with_ci {
            let (page, _): (Vec<GhCommitStatus>, bool) = self
                .get_json_all(
                    &format!(
                        "/repos/{owner}/{name}/commits/{sha}/statuses?per_page={FIRST_PAGE_SIZE}"
                    ),
                    options,
                )
                .await?;
            statuses = page
                .into_iter()
                .map(|s| commit_status_record(repo_id, sha, s))
                .collect();

            let checks = self
                .get_json_all_wrapped(
                    &format!(
                        "/repos/{owner}/{name}/commits/{sha}/check-runs?per_page={FIRST_PAGE_SIZE}"
                    ),
                    options,
                    |page: GhCheckRunsPage| page.check_runs,
                )
                .await?;
            check_runs = checks
                .into_iter()
                .map(|c| check_run_record(repo_id, c))
                .collect();
        }

        Ok(CommitDetail {
            commit,
            files,
            statuses,
            check_runs,
        })
    }

    /// The cheap single-page list endpoints, each behind its own flag.
    async fn list_metadata(
        &self,
        owner: &str,
        name: &str,
        repo_id: i64,
        options: &FetchOptions,
    ) -> Result<MetadataListing, DomainError> {
        let mut listing = MetadataListing::default();

        if options.scope.objects.labels {
            let (labels, labels_complete): (Vec<GhLabel>, bool) = self
                .get_json_all(
                    &format!("/repos/{owner}/{name}/labels?per_page={FIRST_PAGE_SIZE}"),
                    options,
                )
                .await?;
            listing.complete.set(Listing::Labels, labels_complete);
            listing.labels = labels
                .into_iter()
                .map(|l| label_record(repo_id, l))
                .collect();
        }

        if options.scope.objects.milestones {
            let (milestones, milestones_complete): (Vec<GhMilestone>, bool) = self
                .get_json_all(
                    &format!(
                        "/repos/{owner}/{name}/milestones?state=all&per_page={FIRST_PAGE_SIZE}"
                    ),
                    options,
                )
                .await?;
            listing
                .complete
                .set(Listing::Milestones, milestones_complete);
            listing.milestones = milestones
                .into_iter()
                .map(|m| milestone_record(repo_id, m))
                .collect();
        }

        if options.scope.objects.releases {
            let (releases, releases_complete): (Vec<GhRelease>, bool) = self
                .get_json_all(
                    &format!("/repos/{owner}/{name}/releases?per_page={FIRST_PAGE_SIZE}"),
                    options,
                )
                .await?;
            listing.complete.set(Listing::Releases, releases_complete);
            listing.releases = releases
                .into_iter()
                .map(|r| release_record(repo_id, r))
                .collect();
        }

        if options.scope.objects.branches {
            let (branches, branches_complete): (Vec<GhBranch>, bool) = self
                .get_json_all(
                    &format!("/repos/{owner}/{name}/branches?per_page={FIRST_PAGE_SIZE}"),
                    options,
                )
                .await?;
            listing.complete.set(Listing::Branches, branches_complete);
            listing.branches = branches
                .into_iter()
                .map(|b| branch_record(repo_id, b))
                .collect();

            let (tags, tags_complete): (Vec<GhTag>, bool) = self
                .get_json_all(
                    &format!("/repos/{owner}/{name}/tags?per_page={FIRST_PAGE_SIZE}"),
                    options,
                )
                .await?;
            listing.complete.set(Listing::Tags, tags_complete);
            listing.tags = tags.into_iter().map(|t| tag_record(repo_id, t)).collect();
        }

        Ok(listing)
    }

    /// Workflow runs and deployments; jobs are per run, see
    /// [`Self::refine_workflow_run`].
    async fn list_actions(
        &self,
        owner: &str,
        name: &str,
        repo_id: i64,
        options: &FetchOptions,
    ) -> Result<ActionsListing, DomainError> {
        if !options.scope.objects.github_actions {
            return Ok(ActionsListing::default());
        }

        let runs = self
            .get_json_all_wrapped(
                &format!("/repos/{owner}/{name}/actions/runs?per_page={FIRST_PAGE_SIZE}"),
                options,
                |page: GhWorkflowRunsPage| page.workflow_runs,
            )
            .await?;
        let (deployments, _deployments_complete): (Vec<GhDeployment>, bool) = self
            .get_json_all(
                &format!("/repos/{owner}/{name}/deployments?per_page={FIRST_PAGE_SIZE}"),
                options,
            )
            .await?;

        Ok(ActionsListing {
            workflow_runs: runs
                .into_iter()
                .map(|w| workflow_run_record(repo_id, w))
                .collect(),
            deployments: deployments
                .into_iter()
                .map(|d| deployment_record(repo_id, d))
                .collect(),
        })
    }

    async fn refine_workflow_run(
        &self,
        owner: &str,
        name: &str,
        repo_id: i64,
        run_id: i64,
        options: &FetchOptions,
    ) -> Result<Vec<WorkflowJobRecord>, DomainError> {
        let jobs = self
            .get_json_all_wrapped(
                &format!(
                    "/repos/{owner}/{name}/actions/runs/{run_id}/jobs?per_page={FIRST_PAGE_SIZE}"
                ),
                options,
                |page: GhWorkflowJobsPage| page.jobs,
            )
            .await?;
        Ok(jobs
            .into_iter()
            .map(|j| workflow_job_record(repo_id, j))
            .collect())
    }

    async fn clear_cache(
        &self,
        tenant_id: uuid::Uuid,
        owner: &str,
        name: Option<&str>,
    ) -> Result<u64, DomainError> {
        let base = self.api_base_url.trim_end_matches('/');
        let prefix = match name {
            Some(name) => format!("{base}/repos/{owner}/{name}"),
            None => format!("{base}/repos/{owner}/"),
        };
        self.cache.clear(tenant_id, &prefix).await
    }
}
