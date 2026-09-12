//! Handlers of the mirror's REST surfaces.
//!
//! The GitHub-compatible handlers (PRD §5.8) return GitHub-shaped bodies
//! with `page`/`per_page` pagination and a `Link` response header. The
//! extended handlers under `/github-mirror/v1/` keep the platform shapes.

use std::sync::Arc;

use axum::extract::{Path, Query};
use axum::http::{HeaderMap, HeaderValue, header};
use axum::{Json, extract::Extension};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use toolkit::api::canonical_prelude::*;
use toolkit_odata::{ODataQuery, Page};
use toolkit_security::SecurityContext;
use url::form_urlencoded;

use crate::api::rest::routes::ConcreteService;
use crate::domain::error::DomainError;
use crate::domain::repo::{IssueState, ListingDirection, ListingFilter, ListingSort, PageWindow};
use crate::domain::scope::{CollectionMode, ScopeConfig, SyncScope};
use crate::domain::validate::{validate_commit_sha, validate_repo_path};

use super::dto::{
    AuthenticatedUserDto, BranchDto, CacheClearedDto, CheckRunDto, CheckRunsPageDto, CommentDto,
    CommitCommentDto, CommitDto, CommitFileDto, CommitStatsDto, CommitStatusDto, ContributorDto,
    DeploymentDto, GithubMirrorHealthDto, IssueDto, IssueEventDto, IssueReactionDto,
    IssueTimelineEventDto, LabelDto, MilestoneDto, PullRequestDto, PullRequestFileDto, ReleaseDto,
    RepoDto, RepoSyncStatusDto, ResumeAcceptedDto, ReviewCommentDto, ReviewDto, ReviewThreadDto,
    SyncAcceptedDto, SyncSessionDto, TagDto, WorkflowJobDto, WorkflowJobsPageDto, WorkflowRunDto,
    WorkflowRunsPageDto,
};

const DEFAULT_PER_PAGE: u64 = 30;
const MAX_PER_PAGE: u64 = 100;
/// Longest a filter value may be.
///
/// The values GitHub accepts here are words and one timestamp: `closed`,
/// `updated`, `asc`, `2026-01-01T00:00:00Z`. The bound matters because these
/// are echoed back in the `Link` header, and the listings that ignore a
/// filter still carry it there, so without a cap a caller could name its own
/// response-header size.
const MAX_FILTER_VALUE: usize = 64;

/// `?force=true` bypasses the HTTP cache (PRD §5.2 force mode): every request
/// goes out without its stored validator, so nothing is served from cache.
///
/// The remaining fields narrow what the run collects (PRD §5.4, §5.19). Any
/// field left out keeps the gear's configured default, and `include`
/// restricts the object types to exactly the ones named.
#[derive(Debug, Default, Deserialize)]
pub struct SyncQuery {
    pub force: Option<bool>,
    /// Comma-separated object types to collect, e.g.
    /// `issues,pull_requests,commits`. Omit to collect the configured set.
    pub include: Option<String>,
    /// `all` / `open` / `none` for workflow runs and CI checks.
    pub actions_scope: Option<String>,
    /// `all` / `open` / `none` for reactions.
    pub reactions_scope: Option<String>,
    /// `all` / `open` / `none` for timeline events.
    pub timeline_scope: Option<String>,
    /// RFC3339 instant; closed issues and pull requests older than this are
    /// not collected.
    pub since: Option<String>,
}

impl SyncQuery {
    /// The scope this request asks for, or `None` to use the gear's default.
    ///
    /// # Errors
    /// `Validation` when a mode or an object type does not parse.
    fn scope(&self, default: ScopeConfig) -> Result<Option<ScopeConfig>, DomainError> {
        if self.include.is_none()
            && self.actions_scope.is_none()
            && self.reactions_scope.is_none()
            && self.timeline_scope.is_none()
        {
            return Ok(None);
        }

        let mut scope = default;
        if let Some(include) = self.include.as_deref() {
            scope.objects = objects_from_include(include)?;
        }
        if let Some(mode) = self.actions_scope.as_deref() {
            scope.collection.actions = CollectionMode::parse(mode)?;
        }
        if let Some(mode) = self.reactions_scope.as_deref() {
            scope.collection.reactions = CollectionMode::parse(mode)?;
        }
        if let Some(mode) = self.timeline_scope.as_deref() {
            scope.collection.timeline = CollectionMode::parse(mode)?;
        }
        Ok(Some(scope))
    }

    /// # Errors
    /// `Validation` when `since` is not an RFC3339 instant.
    fn since(&self) -> Result<Option<DateTime<Utc>>, DomainError> {
        let Some(raw) = self.since.as_deref() else {
            return Ok(None);
        };
        DateTime::parse_from_rfc3339(raw)
            .map(|at| Some(at.with_timezone(&Utc)))
            .map_err(|e| DomainError::Validation {
                field: "since".to_owned(),
                message: format!("`{raw}` is not an RFC3339 instant: {e}"),
            })
    }
}

/// Build an object scope enabling exactly the comma-separated types named.
fn objects_from_include(include: &str) -> Result<SyncScope, DomainError> {
    let mut scope = SyncScope::none();
    for raw in include.split(',') {
        let name = raw.trim().to_ascii_lowercase();
        if name.is_empty() {
            continue;
        }
        match name.as_str() {
            "issues" => scope.issues = true,
            "pull_requests" | "pulls" => scope.pull_requests = true,
            "commits" => scope.commits = true,
            "releases" => scope.releases = true,
            "branches" => scope.branches = true,
            "labels" => scope.labels = true,
            "milestones" => scope.milestones = true,
            "github_actions" | "actions" => scope.github_actions = true,
            "contributors" => scope.contributors = true,
            "security" => scope.security = true,
            other => {
                return Err(DomainError::Validation {
                    field: "include".to_owned(),
                    message: format!("unknown object type `{other}`"),
                });
            }
        }
    }
    Ok(scope)
}

/// `?owner=X` clears everything mirrored for that owner; `?repo=owner/name`
/// narrows it to one repository.
#[derive(Debug, Default, Deserialize)]
pub struct CacheClearQuery {
    pub owner: Option<String>,
    pub repo: Option<String>,
}

/// `?repo=owner/name` narrows a resume to one repository; omitting it resumes
/// every repository the caller's tenant left `in_progress`.
#[derive(Debug, Default, Deserialize)]
pub struct ResumeQuery {
    pub repo: Option<String>,
    pub force: Option<bool>,
}

/// `?status=in_progress` narrows a run-status listing.
#[derive(Debug, Default, Deserialize)]
pub struct RunStatusQuery {
    pub status: Option<String>,
}

/// GitHub-style pagination query (`?page=2&per_page=50`), plus the `state`,
/// `sort`, `direction` and `since` filters the issue and pull listings accept.
#[derive(Debug, Deserialize)]
pub struct GithubPageQuery {
    pub page: Option<u64>,
    pub per_page: Option<u64>,
    pub state: Option<String>,
    pub sort: Option<String>,
    pub direction: Option<String>,
    pub since: Option<String>,
}

impl GithubPageQuery {
    /// The state to filter on, following GitHub: no `state` means `open`,
    /// `all` means no filter.
    ///
    /// # Errors
    /// `Validation` when the value is not a state GitHub knows.
    fn state_filter(&self) -> Result<Option<IssueState>, DomainError> {
        match self.state.as_deref() {
            None => Ok(Some(IssueState::Open)),
            Some("all") => Ok(None),
            Some(state) => IssueState::parse(state).map(Some),
        }
    }

    fn since_filter(&self) -> Result<Option<DateTime<Utc>>, DomainError> {
        self.since
            .as_deref()
            .map(|raw| {
                DateTime::parse_from_rfc3339(raw)
                    .map(|at| at.with_timezone(&Utc))
                    .map_err(|e| DomainError::Validation {
                        field: "since".to_owned(),
                        message: format!("`{raw}` is not an RFC3339 timestamp: {e}"),
                    })
            })
            .transpose()
    }

    /// The whole listing filter, with GitHub's defaults for anything the
    /// caller left out.
    ///
    /// # Errors
    /// `Validation` when `state`, `sort`, `direction` or `since` does not
    /// parse.
    fn listing_filter(&self) -> Result<ListingFilter, DomainError> {
        Ok(ListingFilter {
            state: self.state_filter()?,
            sort: ListingSort::parse(self.sort.as_deref())?,
            direction: ListingDirection::parse(self.direction.as_deref())?,
            since: self.since_filter()?,
        })
    }

    /// Every filter parameter the caller sent, with the name it arrived under.
    fn filter_values(&self) -> [(&'static str, Option<&str>); 4] {
        [
            ("state", self.state.as_deref()),
            ("sort", self.sort.as_deref()),
            ("direction", self.direction.as_deref()),
            ("since", self.since.as_deref()),
        ]
    }

    /// # Errors
    /// `Validation` when a filter value is longer than
    /// [`MAX_FILTER_VALUE`], which no value GitHub accepts is.
    fn validate_filter_lengths(&self) -> Result<(), DomainError> {
        for (field, value) in self.filter_values() {
            if value.is_some_and(|value| value.len() > MAX_FILTER_VALUE) {
                return Err(DomainError::Validation {
                    field: field.to_owned(),
                    message: format!("must be {MAX_FILTER_VALUE} characters or fewer"),
                });
            }
        }
        Ok(())
    }

    /// The filter parameters the caller actually sent, rendered back as a
    /// query string so a `Link` header keeps them: following `rel="next"`
    /// must walk the same filtered listing, not the unfiltered default.
    fn filter_query(&self) -> String {
        let mut query = String::new();
        for (key, value) in self.filter_values() {
            if let Some(value) = value {
                let encoded: String = form_urlencoded::byte_serialize(value.as_bytes()).collect();
                query.push('&');
                query.push_str(key);
                query.push('=');
                query.push_str(&encoded);
            }
        }
        query
    }
}

struct GithubPage {
    page: u64,
    per_page: u64,
    filters: String,
    window: PageWindow,
}

impl GithubPageQuery {
    /// # Errors
    /// `Validation` when a filter value is over-long, or when the requested
    /// page starts past [`PageWindow::MAX_OFFSET`].
    fn normalized(&self) -> Result<GithubPage, DomainError> {
        self.validate_filter_lengths()?;
        let page = self.page.filter(|p| *p >= 1).unwrap_or(1);
        let per_page = self
            .per_page
            .filter(|p| *p >= 1)
            .unwrap_or(DEFAULT_PER_PAGE)
            .min(MAX_PER_PAGE);
        let window =
            PageWindow::bounded(per_page, page.saturating_sub(1).saturating_mul(per_page))?;

        Ok(GithubPage {
            page,
            per_page,
            filters: self.filter_query(),
            window,
        })
    }
}

impl GithubPage {
    /// The rows this page needs, as an offset the database applies: asking
    /// for page 50 reads one page, not fifty. Bounded when it was built, in
    /// [`GithubPageQuery::normalized`].
    const fn window(&self) -> PageWindow {
        self.window
    }

    fn convert<T, D: From<T>>(items: Vec<T>) -> Vec<D> {
        items.into_iter().map(D::from).collect()
    }

    fn link_header(&self, path: &str, returned: usize) -> HeaderMap {
        self.link_header_with_total(path, returned, None)
    }

    /// The `Link` header, with `rel="last"` resolved from `total` when the
    /// listing can count itself.
    ///
    /// Without a total the last page is only knowable when the current one
    /// came back short, so on a full page `rel="last"` is omitted rather than
    /// guessed — GitHub itself always knows the total and always sends it.
    fn link_header_with_total(&self, path: &str, returned: usize, total: Option<u64>) -> HeaderMap {
        // Page-based paging stops at PageWindow::MAX_OFFSET, so a link
        // past it would advertise a page this gear refuses.
        let reachable_pages = PageWindow::MAX_OFFSET
            .checked_div(self.per_page)
            .map_or(1, |pages| pages.saturating_add(1));
        let last_page =
            total.map(|total| total.div_ceil(self.per_page).max(1).min(reachable_pages));
        let is_last_page =
            last_page.map_or(returned as u64 != self.per_page, |last| self.page >= last);
        let filters = self.filters.as_str();

        let mut links = Vec::new();
        if !is_last_page && self.page < reachable_pages {
            links.push(format!(
                "<{path}?page={}&per_page={}{filters}>; rel=\"next\"",
                self.page + 1,
                self.per_page
            ));
        }
        if self.page > 1 {
            links.push(format!(
                "<{path}?page={}&per_page={}{filters}>; rel=\"prev\"",
                self.page - 1,
                self.per_page
            ));
            links.push(format!(
                "<{path}?page=1&per_page={}{filters}>; rel=\"first\"",
                self.per_page
            ));
        }
        // With a total the last page is known outright; without one, only a
        // short page proves it is the end.
        if let Some(last) = last_page {
            links.push(format!(
                "<{path}?page={last}&per_page={}{filters}>; rel=\"last\"",
                self.per_page
            ));
        } else if is_last_page {
            links.push(format!(
                "<{path}?page={}&per_page={}{filters}>; rel=\"last\"",
                self.page, self.per_page
            ));
        }

        let mut headers = HeaderMap::new();
        if !links.is_empty()
            && let Ok(value) = HeaderValue::from_str(&links.join(", "))
        {
            headers.insert(header::LINK, value);
        }
        headers
    }
}

type GithubList<D> = ApiResult<(HeaderMap, JsonBody<Vec<D>>)>;

fn respond<D>(page: &GithubPage, path: &str, items: Vec<D>) -> (HeaderMap, JsonBody<Vec<D>>) {
    let headers = page.link_header(path, items.len());
    (headers, Json(items))
}

/// [`respond`] for a listing that knows how many rows it has in total, so the
/// `Link` header can name the last page even from a full one.
fn respond_counted<D>(
    page: &GithubPage,
    path: &str,
    items: Vec<D>,
    total: u64,
) -> (HeaderMap, JsonBody<Vec<D>>) {
    let headers = page.link_header_with_total(path, items.len(), Some(total));
    (headers, Json(items))
}

// The signature must be `async` for axum's `Handler` impl even though the
// status read is synchronous.
#[allow(clippy::unused_async)]
pub async fn health(
    Extension(svc): Extension<Arc<ConcreteService>>,
) -> ApiResult<JsonBody<GithubMirrorHealthDto>> {
    let status = svc.status();
    Ok(Json(status.into()))
}

pub async fn list_repos(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ConcreteService>>,
    OData(query): OData,
) -> ApiResult<JsonPage<RepoDto>> {
    let page: Page<_> = svc.list_repos(&ctx, &query).await?;
    Ok(Json(page.map_items(RepoDto::from)))
}

pub async fn sync_repository(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ConcreteService>>,
    Path((owner, name)): Path<(String, String)>,
    Query(query): Query<SyncQuery>,
) -> ApiResult<(StatusCode, JsonBody<SyncAcceptedDto>)> {
    validate_repo_path(&owner, &name)?;
    let scope = query.scope(svc.default_scope())?;
    let since = query.since()?;
    let session_id = svc
        .enqueue_sync(
            &ctx,
            &owner,
            &name,
            scope,
            query.force.unwrap_or(false),
            since,
        )
        .await?;
    Ok((
        StatusCode::ACCEPTED,
        Json(SyncAcceptedDto {
            session_id: session_id.to_string(),
            repository: format!("{owner}/{name}"),
            status: "queued".to_owned(),
        }),
    ))
}

pub async fn list_issues(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ConcreteService>>,
    Path((owner, name)): Path<(String, String)>,
    Query(query): Query<GithubPageQuery>,
) -> GithubList<IssueDto> {
    validate_repo_path(&owner, &name)?;
    let page = query.normalized()?;
    let (items, total) = svc
        .list_issues(&ctx, &owner, &name, page.window(), query.listing_filter()?)
        .await?;
    let items = items.items;
    let path = format!("/repos/{owner}/{name}/issues");
    Ok(respond_counted(
        &page,
        &path,
        GithubPage::convert(items),
        total,
    ))
}

pub async fn list_comments(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ConcreteService>>,
    Path((owner, name, number)): Path<(String, String, i64)>,
    Query(query): Query<GithubPageQuery>,
) -> GithubList<CommentDto> {
    validate_repo_path(&owner, &name)?;
    let page = query.normalized()?;
    let items = svc
        .list_comments(&ctx, &owner, &name, number, page.window())
        .await?
        .items;
    let path = format!("/repos/{owner}/{name}/issues/{number}/comments");
    Ok(respond(&page, &path, GithubPage::convert(items)))
}

pub async fn list_pull_requests(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ConcreteService>>,
    Path((owner, name)): Path<(String, String)>,
    Query(query): Query<GithubPageQuery>,
) -> GithubList<PullRequestDto> {
    validate_repo_path(&owner, &name)?;
    let page = query.normalized()?;
    let (items, total) = svc
        .list_pull_requests(&ctx, &owner, &name, page.window(), query.listing_filter()?)
        .await?;
    let items = items.items;
    let path = format!("/repos/{owner}/{name}/pulls");
    Ok(respond_counted(
        &page,
        &path,
        GithubPage::convert(items),
        total,
    ))
}

pub async fn list_reviews(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ConcreteService>>,
    Path((owner, name, number)): Path<(String, String, i64)>,
    Query(query): Query<GithubPageQuery>,
) -> GithubList<ReviewDto> {
    validate_repo_path(&owner, &name)?;
    let page = query.normalized()?;
    let items = svc
        .list_reviews(&ctx, &owner, &name, number, page.window())
        .await?
        .items;
    let path = format!("/repos/{owner}/{name}/pulls/{number}/reviews");
    Ok(respond(&page, &path, GithubPage::convert(items)))
}

pub async fn list_review_comments(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ConcreteService>>,
    Path((owner, name, number)): Path<(String, String, i64)>,
    Query(query): Query<GithubPageQuery>,
) -> GithubList<ReviewCommentDto> {
    validate_repo_path(&owner, &name)?;
    let page = query.normalized()?;
    let items = svc
        .list_review_comments(&ctx, &owner, &name, number, page.window())
        .await?
        .items;
    let path = format!("/repos/{owner}/{name}/pulls/{number}/comments");
    Ok(respond(&page, &path, GithubPage::convert(items)))
}

pub async fn list_pull_request_files(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ConcreteService>>,
    Path((owner, name, number)): Path<(String, String, i64)>,
    Query(query): Query<GithubPageQuery>,
) -> GithubList<PullRequestFileDto> {
    validate_repo_path(&owner, &name)?;
    let page = query.normalized()?;
    let items = svc
        .list_pull_request_files(&ctx, &owner, &name, number, page.window())
        .await?
        .items;
    let path = format!("/repos/{owner}/{name}/pulls/{number}/files");
    Ok(respond(&page, &path, GithubPage::convert(items)))
}

pub async fn list_commits(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ConcreteService>>,
    Path((owner, name)): Path<(String, String)>,
    Query(query): Query<GithubPageQuery>,
) -> GithubList<CommitDto> {
    validate_repo_path(&owner, &name)?;
    let page = query.normalized()?;
    let (items, total) = svc
        .list_commits(
            &ctx,
            &owner,
            &name,
            page.window(),
            query.listing_filter()?.since,
        )
        .await?;
    let items = items.items;
    let path = format!("/repos/{owner}/{name}/commits");
    Ok(respond_counted(
        &page,
        &path,
        GithubPage::convert(items),
        total,
    ))
}

pub async fn list_branches(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ConcreteService>>,
    Path((owner, name)): Path<(String, String)>,
    Query(query): Query<GithubPageQuery>,
) -> GithubList<BranchDto> {
    validate_repo_path(&owner, &name)?;
    let page = query.normalized()?;
    let items = svc
        .list_branches(&ctx, &owner, &name, page.window())
        .await?
        .items;
    let path = format!("/repos/{owner}/{name}/branches");
    Ok(respond(&page, &path, GithubPage::convert(items)))
}

pub async fn list_tags(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ConcreteService>>,
    Path((owner, name)): Path<(String, String)>,
    Query(query): Query<GithubPageQuery>,
) -> GithubList<TagDto> {
    validate_repo_path(&owner, &name)?;
    let page = query.normalized()?;
    let items = svc
        .list_tags(&ctx, &owner, &name, page.window())
        .await?
        .items;
    let path = format!("/repos/{owner}/{name}/tags");
    Ok(respond(&page, &path, GithubPage::convert(items)))
}

pub async fn list_releases(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ConcreteService>>,
    Path((owner, name)): Path<(String, String)>,
    Query(query): Query<GithubPageQuery>,
) -> GithubList<ReleaseDto> {
    validate_repo_path(&owner, &name)?;
    let page = query.normalized()?;
    let items = svc
        .list_releases(&ctx, &owner, &name, page.window())
        .await?
        .items;
    let path = format!("/repos/{owner}/{name}/releases");
    Ok(respond(&page, &path, GithubPage::convert(items)))
}

pub async fn list_milestones(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ConcreteService>>,
    Path((owner, name)): Path<(String, String)>,
    Query(query): Query<GithubPageQuery>,
) -> GithubList<MilestoneDto> {
    validate_repo_path(&owner, &name)?;
    let page = query.normalized()?;
    let items = svc
        .list_milestones(&ctx, &owner, &name, page.window())
        .await?
        .items;
    let path = format!("/repos/{owner}/{name}/milestones");
    Ok(respond(&page, &path, GithubPage::convert(items)))
}

pub async fn list_labels(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ConcreteService>>,
    Path((owner, name)): Path<(String, String)>,
    Query(query): Query<GithubPageQuery>,
) -> GithubList<LabelDto> {
    validate_repo_path(&owner, &name)?;
    let page = query.normalized()?;
    let items = svc
        .list_labels(&ctx, &owner, &name, page.window())
        .await?
        .items;
    let path = format!("/repos/{owner}/{name}/labels");
    Ok(respond(&page, &path, GithubPage::convert(items)))
}

pub async fn list_contributors(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ConcreteService>>,
    Path((owner, name)): Path<(String, String)>,
    Query(query): Query<GithubPageQuery>,
) -> GithubList<ContributorDto> {
    validate_repo_path(&owner, &name)?;
    let page = query.normalized()?;
    let items = svc
        .list_contributors(&ctx, &owner, &name, page.window())
        .await?
        .items;
    let path = format!("/repos/{owner}/{name}/contributors");
    Ok(respond(&page, &path, GithubPage::convert(items)))
}

pub async fn list_workflow_runs(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ConcreteService>>,
    Path((owner, name)): Path<(String, String)>,
    Query(query): Query<GithubPageQuery>,
) -> ApiResult<(HeaderMap, JsonBody<WorkflowRunsPageDto>)> {
    validate_repo_path(&owner, &name)?;
    let page = query.normalized()?;
    let (items, total) = svc
        .list_workflow_runs(&ctx, &owner, &name, page.window())
        .await?;
    let runs: Vec<WorkflowRunDto> = GithubPage::convert(items.items);
    let path = format!("/repos/{owner}/{name}/actions/runs");
    let headers = page.link_header_with_total(&path, runs.len(), Some(total));
    // GitHub's `total_count` spans every page, so it is a count, not the
    // length of the slice being served.
    let total_count = i64::try_from(total).unwrap_or(i64::MAX);
    Ok((
        headers,
        Json(WorkflowRunsPageDto {
            total_count,
            workflow_runs: runs,
        }),
    ))
}

pub async fn list_commit_files(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ConcreteService>>,
    Path((owner, name, sha)): Path<(String, String, String)>,
    OData(query): OData,
) -> ApiResult<JsonPage<CommitFileDto>> {
    validate_repo_path(&owner, &name)?;
    validate_commit_sha(&sha)?;
    let page: Page<_> = svc
        .list_commit_files(&ctx, &owner, &name, &sha, &query)
        .await?;
    Ok(Json(page.map_items(CommitFileDto::from)))
}

pub async fn list_review_threads(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ConcreteService>>,
    Path((owner, name, number)): Path<(String, String, i64)>,
    OData(query): OData,
) -> ApiResult<JsonPage<ReviewThreadDto>> {
    validate_repo_path(&owner, &name)?;
    let page: Page<_> = svc
        .list_review_threads(&ctx, &owner, &name, number, &query)
        .await?;
    Ok(Json(page.map_items(ReviewThreadDto::from)))
}

pub async fn get_repo(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ConcreteService>>,
    Path((owner, name)): Path<(String, String)>,
) -> ApiResult<JsonBody<RepoDto>> {
    validate_repo_path(&owner, &name)?;
    let repo = svc.get_repo(&ctx, &owner, &name).await?;
    Ok(Json(repo.into()))
}

pub async fn get_issue(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ConcreteService>>,
    Path((owner, name, number)): Path<(String, String, i64)>,
) -> ApiResult<JsonBody<IssueDto>> {
    validate_repo_path(&owner, &name)?;
    let issue = svc.get_issue(&ctx, &owner, &name, number).await?;
    Ok(Json(issue.into()))
}

pub async fn get_pull_request(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ConcreteService>>,
    Path((owner, name, number)): Path<(String, String, i64)>,
) -> ApiResult<JsonBody<PullRequestDto>> {
    validate_repo_path(&owner, &name)?;
    let pull = svc.get_pull_request(&ctx, &owner, &name, number).await?;
    Ok(Json(pull.into()))
}

pub async fn get_commit(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ConcreteService>>,
    Path((owner, name, sha)): Path<(String, String, String)>,
) -> ApiResult<JsonBody<CommitDto>> {
    validate_repo_path(&owner, &name)?;
    validate_commit_sha(&sha)?;
    let commit = svc.get_commit(&ctx, &owner, &name, &sha).await?;
    let files = svc
        .list_commit_files(&ctx, &owner, &name, &sha, &ODataQuery::default())
        .await?
        .items;

    let mut body = CommitDto::from(commit.clone());
    body.stats = Some(CommitStatsDto {
        additions: commit.additions,
        deletions: commit.deletions,
        total: commit.additions + commit.deletions,
    });
    body.files = files.into_iter().map(PullRequestFileDto::from).collect();
    Ok(Json(body))
}

pub async fn list_commit_comments(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ConcreteService>>,
    Path((owner, name, sha)): Path<(String, String, String)>,
    Query(query): Query<GithubPageQuery>,
) -> GithubList<CommitCommentDto> {
    validate_repo_path(&owner, &name)?;
    validate_commit_sha(&sha)?;
    let page = query.normalized()?;
    let items = svc
        .list_commit_comments(&ctx, &owner, &name, &sha, page.window())
        .await?
        .items;
    let path = format!("/repos/{owner}/{name}/commits/{sha}/comments");
    Ok(respond(&page, &path, GithubPage::convert(items)))
}

pub async fn list_issue_events(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ConcreteService>>,
    Path((owner, name, number)): Path<(String, String, i64)>,
    Query(query): Query<GithubPageQuery>,
) -> GithubList<IssueEventDto> {
    validate_repo_path(&owner, &name)?;
    let page = query.normalized()?;
    let items = svc
        .list_issue_events(&ctx, &owner, &name, number, page.window())
        .await?
        .items;
    let path = format!("/repos/{owner}/{name}/issues/{number}/events");
    Ok(respond(&page, &path, GithubPage::convert(items)))
}

pub async fn list_issue_reactions(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ConcreteService>>,
    Path((owner, name, number)): Path<(String, String, i64)>,
    Query(query): Query<GithubPageQuery>,
) -> GithubList<IssueReactionDto> {
    validate_repo_path(&owner, &name)?;
    let page = query.normalized()?;
    let items = svc
        .list_issue_reactions(&ctx, &owner, &name, number, page.window())
        .await?
        .items;
    let path = format!("/repos/{owner}/{name}/issues/{number}/reactions");
    Ok(respond(&page, &path, GithubPage::convert(items)))
}

pub async fn list_issue_timeline(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ConcreteService>>,
    Path((owner, name, number)): Path<(String, String, i64)>,
    Query(query): Query<GithubPageQuery>,
) -> GithubList<IssueTimelineEventDto> {
    validate_repo_path(&owner, &name)?;
    let page = query.normalized()?;
    let items = svc
        .list_issue_timeline(&ctx, &owner, &name, number, page.window())
        .await?
        .items;
    let path = format!("/repos/{owner}/{name}/issues/{number}/timeline");
    Ok(respond(&page, &path, GithubPage::convert(items)))
}

pub async fn list_deployments(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ConcreteService>>,
    Path((owner, name)): Path<(String, String)>,
    Query(query): Query<GithubPageQuery>,
) -> GithubList<DeploymentDto> {
    validate_repo_path(&owner, &name)?;
    let page = query.normalized()?;
    let items = svc
        .list_deployments(&ctx, &owner, &name, page.window())
        .await?
        .items;
    let path = format!("/repos/{owner}/{name}/deployments");
    Ok(respond(&page, &path, GithubPage::convert(items)))
}

pub async fn list_pull_request_commits(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ConcreteService>>,
    Path((owner, name, number)): Path<(String, String, i64)>,
    Query(query): Query<GithubPageQuery>,
) -> GithubList<CommitDto> {
    validate_repo_path(&owner, &name)?;
    let page = query.normalized()?;
    let items = svc
        .list_pull_request_commits(&ctx, &owner, &name, number, page.window())
        .await?
        .items;
    let path = format!("/repos/{owner}/{name}/pulls/{number}/commits");
    Ok(respond(&page, &path, GithubPage::convert(items)))
}

pub async fn list_commit_statuses(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ConcreteService>>,
    Path((owner, name, sha)): Path<(String, String, String)>,
    Query(query): Query<GithubPageQuery>,
) -> GithubList<CommitStatusDto> {
    validate_repo_path(&owner, &name)?;
    validate_commit_sha(&sha)?;
    let page = query.normalized()?;
    let items = svc
        .list_commit_statuses(&ctx, &owner, &name, &sha, page.window())
        .await?
        .items;
    let path = format!("/repos/{owner}/{name}/commits/{sha}/statuses");
    Ok(respond(&page, &path, GithubPage::convert(items)))
}

pub async fn list_workflow_jobs(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ConcreteService>>,
    Path((owner, name, run_id)): Path<(String, String, i64)>,
    Query(query): Query<GithubPageQuery>,
) -> ApiResult<(HeaderMap, JsonBody<WorkflowJobsPageDto>)> {
    validate_repo_path(&owner, &name)?;
    let page = query.normalized()?;
    let (items, total) = svc
        .list_workflow_jobs(&ctx, &owner, &name, run_id, page.window())
        .await?;
    let jobs: Vec<WorkflowJobDto> = GithubPage::convert(items.items);
    let path = format!("/repos/{owner}/{name}/actions/runs/{run_id}/jobs");
    let headers = page.link_header_with_total(&path, jobs.len(), Some(total));
    let total_count = i64::try_from(total).unwrap_or(i64::MAX);
    Ok((headers, Json(WorkflowJobsPageDto { total_count, jobs })))
}

pub async fn list_check_runs(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ConcreteService>>,
    Path((owner, name, sha)): Path<(String, String, String)>,
    Query(query): Query<GithubPageQuery>,
) -> ApiResult<(HeaderMap, JsonBody<CheckRunsPageDto>)> {
    validate_repo_path(&owner, &name)?;
    validate_commit_sha(&sha)?;
    let page = query.normalized()?;
    let (items, total) = svc
        .list_check_runs(&ctx, &owner, &name, &sha, page.window())
        .await?;
    let check_runs: Vec<CheckRunDto> = GithubPage::convert(items.items);
    let path = format!("/repos/{owner}/{name}/commits/{sha}/check-runs");
    let headers = page.link_header_with_total(&path, check_runs.len(), Some(total));
    let total_count = i64::try_from(total).unwrap_or(i64::MAX);
    Ok((
        headers,
        Json(CheckRunsPageDto {
            total_count,
            check_runs,
        }),
    ))
}

/// GitHub's `GET /user`, answered by the mirror on its own behalf: clients
/// call it to validate a connection before browsing repositories.
pub async fn get_authenticated_user(
    Extension(_ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ConcreteService>>,
) -> ApiResult<JsonBody<AuthenticatedUserDto>> {
    let status = svc.status();
    Ok(Json(AuthenticatedUserDto {
        id: 0,
        login: status.gear,
        name: Some(format!("GitHub Mirror {}", status.version)),
        user_type: "Bot".to_owned(),
    }))
}

/// GitHub's `GET /user/repos`. For the mirror, "the authenticated user's
/// repositories" are the repositories mirrored for the caller's tenant.
pub async fn list_user_repos(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ConcreteService>>,
    Query(query): Query<GithubPageQuery>,
) -> GithubList<RepoDto> {
    let page = query.normalized()?;
    let items = svc.list_repos_page(&ctx, page.window()).await?;
    Ok(respond(&page, "/user/repos", GithubPage::convert(items)))
}

pub async fn get_sync_session(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ConcreteService>>,
    Path(id): Path<uuid::Uuid>,
) -> ApiResult<JsonBody<SyncSessionDto>> {
    let session = svc.get_session(&ctx, id).await?;
    Ok(Json(SyncSessionDto::from(session)))
}

pub async fn list_sync_sessions(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ConcreteService>>,
    OData(query): OData,
) -> ApiResult<JsonPage<SyncSessionDto>> {
    let page: Page<_> = svc.list_sessions(&ctx, &query).await?;
    Ok(Json(page.map_items(SyncSessionDto::from)))
}

pub async fn resume_syncs(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ConcreteService>>,
    Query(query): Query<ResumeQuery>,
) -> ApiResult<(StatusCode, JsonBody<ResumeAcceptedDto>)> {
    let ids = svc
        .resume_incomplete_syncs(&ctx, query.repo.as_deref(), query.force.unwrap_or(false))
        .await?;
    Ok((
        StatusCode::ACCEPTED,
        Json(ResumeAcceptedDto {
            resumed: ids.len(),
            session_ids: ids.iter().map(ToString::to_string).collect(),
        }),
    ))
}

pub async fn list_repo_sync_status(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ConcreteService>>,
    OData(query): OData,
    Query(filter): Query<RunStatusQuery>,
) -> ApiResult<JsonPage<RepoSyncStatusDto>> {
    let page: Page<_> = svc
        .list_repo_sync_status(&ctx, &query, filter.status.as_deref())
        .await?;
    Ok(Json(page.map_items(RepoSyncStatusDto::from)))
}

pub async fn clear_cache(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ConcreteService>>,
    Query(query): Query<CacheClearQuery>,
) -> ApiResult<JsonBody<CacheClearedDto>> {
    let (owner, name) = match (&query.repo, &query.owner) {
        (Some(slug), _) => {
            let (owner, name) = slug
                .split_once('/')
                .ok_or_else(|| DomainError::Validation {
                    field: "repo".to_owned(),
                    message: format!("`{slug}` is not an owner/name slug"),
                })?;
            (owner.to_owned(), Some(name.to_owned()))
        }
        (None, Some(owner)) => (owner.clone(), None),
        (None, None) => {
            return Err(DomainError::Validation {
                field: "owner".to_owned(),
                message: "give `owner` or `repo`; clearing every tenant's cache is not offered"
                    .to_owned(),
            }
            .into());
        }
    };

    let entries_removed = svc.clear_cache(&ctx, &owner, name.as_deref()).await?;
    Ok(Json(CacheClearedDto {
        scope: name.map_or_else(|| owner.clone(), |name| format!("{owner}/{name}")),
        entries_removed,
    }))
}
