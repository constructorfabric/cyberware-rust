//! The gear's own extended endpoints under `/github-mirror/v1/`
//! (PRD §5.9): health, mirrored-repository listing, the throwaway sync
//! entry point, and the read slices GitHub has no endpoint for.

use axum::Router;
use axum::http::StatusCode;
use toolkit::api::operation_builder::OperationBuilderODataExt;
use toolkit::api::{OpenApiRegistry, OperationBuilder};

use crate::infra::storage::odata_mapper::{CommitFileField, RepoField, ReviewThreadField};

use crate::api::rest::routes::{API_TAG, License};
use crate::api::rest::{dto, handlers};

pub fn register_routes(mut router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    router = OperationBuilder::get("/github-mirror/v1/health")
        .operation_id("github_mirror.v1.health")
        .summary("GitHub Mirror health")
        .description("Reports that the github-mirror gear is loaded and serving requests")
        .tag(API_TAG)
        .anonymous()
        .handler(handlers::health)
        .json_response_with_schema::<dto::GithubMirrorHealthDto>(
            openapi,
            StatusCode::OK,
            "Gear is healthy",
        )
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/github-mirror/v1/repos")
        .operation_id("github_mirror.v1.list_repos")
        .summary("List mirrored repositories")
        .description(
            "Returns the GitHub repositories held in the local mirror for the caller's tenant",
        )
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .query_param("limit", false, "Maximum number of repositories to return")
        .query_param("cursor", false, "Cursor for pagination")
        .handler(handlers::list_repos)
        .json_response_with_schema::<toolkit_odata::Page<dto::RepoDto>>(
            openapi,
            StatusCode::OK,
            "Paginated list of mirrored repositories",
        )
        .with_odata_filter::<RepoField>()
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::post("/github-mirror/v1/repos/{owner}/{name}/sync")
        .operation_id("github_mirror.v1.sync_repository")
        .summary("Sync a repository from GitHub into the mirror")
        .description(
            "Queues a sync of the repository and answers immediately with a session id.              The background worker fetches the repository plus the first page of its              entities from GitHub and upserts them into the caller's tenant mirror;              poll the session for the outcome. No pagination, conditional requests, or              rate-limit budgeting yet.",
        )
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("owner", "Repo owner login")
        .path_param("name", "Repo name")
        .query_param("force", false, "Bypass the HTTP cache and re-fetch everything")
        .query_param(
            "include",
            false,
            "Comma-separated object types to collect, e.g. `issues,pull_requests`",
        )
        .query_param("actions_scope", false, "`all`, `open` or `none` for CI results")
        .query_param("reactions_scope", false, "`all`, `open` or `none` for reactions")
        .query_param("timeline_scope", false, "`all`, `open` or `none` for timeline events")
        .handler(handlers::sync_repository)
        .json_response_with_schema::<dto::SyncAcceptedDto>(
            openapi,
            StatusCode::ACCEPTED,
            "Sync queued; the body carries the session id to poll",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::delete("/github-mirror/v1/cache")
        .operation_id("github_mirror.v1.clear_cache")
        .summary("Drop cached GitHub responses for an owner or a repository")
        .description(
            "DESIGN 4's `clear_cache`. Removes raw cached responses so the next sync              re-fetches instead of revalidating; the mirrored rows themselves are left              untouched. Give `repo=owner/name` for one repository or `owner=X` for              everything mirrored under that owner.",
        )
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .query_param("owner", false, "Clear every repository of this owner")
        .query_param("repo", false, "Clear only this `owner/name` repository")
        .handler(handlers::clear_cache)
        .json_response_with_schema::<dto::CacheClearedDto>(
            openapi,
            StatusCode::OK,
            "How many cached responses were removed",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::post("/github-mirror/v1/sync/resume")
        .operation_id("github_mirror.v1.resume_syncs")
        .summary("Re-run repositories still marked in_progress")
        .description(
            "PRD 5.2's resume operation. Resume is a re-run, not a restore: each              repository still marked `in_progress` is queued for a fresh sync, and the              cache plus change-detection state are what make that cheap. Answers              immediately with one session id per repository. Pass `repo=owner/name`              to resume a single repository; a repository that is not `in_progress`              has nothing to resume and comes back with `resumed: 0`.",
        )
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .query_param("repo", false, "Resume only this `owner/name` repository")
        .query_param("force", false, "Bypass the HTTP cache and re-fetch everything")
        .handler(handlers::resume_syncs)
        .json_response_with_schema::<dto::ResumeAcceptedDto>(
            openapi,
            StatusCode::ACCEPTED,
            "Resume queued; the body carries one session id per repository",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/github-mirror/v1/sync-status")
        .operation_id("github_mirror.v1.list_repo_sync_status")
        .summary("Per-repository run status and last-sync time")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .query_param("status", false, "Only `in_progress` or only `complete`")
        .query_param("limit", false, "Maximum number of repositories to return")
        .handler(handlers::list_repo_sync_status)
        .json_response_with_schema::<dto::RepoSyncStatusDto>(
            openapi,
            StatusCode::OK,
            "Paginated per-repository run statuses",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .register(router, openapi);

    register_session_routes(router, openapi)
}

/// Session, run-status and per-entity read routes.
///
/// Split from [`register_routes`] only to stay under the 200-line cap; the two
/// halves are one registration pass.
fn register_session_routes(mut router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    router = OperationBuilder::get("/github-mirror/v1/sessions")
        .operation_id("github_mirror.v1.list_sync_sessions")
        .summary("List sync sessions, newest first")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .query_param("limit", false, "Maximum number of sessions to return")
        .handler(handlers::list_sync_sessions)
        .json_response_with_schema::<dto::SyncSessionDto>(
            openapi,
            StatusCode::OK,
            "Paginated list of sync sessions",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/github-mirror/v1/sessions/{id}")
        .operation_id("github_mirror.v1.get_sync_session")
        .summary("One sync session by id")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "Session id, as returned by the sync endpoint")
        .handler(handlers::get_sync_session)
        .json_response_with_schema::<dto::SyncSessionDto>(
            openapi,
            StatusCode::OK,
            "The session's current status, progress and duration",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/github-mirror/v1/repos/{owner}/{name}/commits/{sha}/files")
        .operation_id("github_mirror.v1.list_commit_files")
        .summary("List mirrored changed files of a commit")
        .description(
            "Returns the changed files held in the local mirror for the tenant, by file name",
        )
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("owner", "Repo owner login")
        .path_param("name", "Repo name")
        .path_param("sha", "Commit SHA")
        .query_param("limit", false, "Maximum number of files to return")
        .handler(handlers::list_commit_files)
        .json_response_with_schema::<toolkit_odata::Page<dto::CommitFileDto>>(
            openapi,
            StatusCode::OK,
            "Paginated list of mirrored commit files",
        )
        .with_odata_filter::<CommitFileField>()
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/github-mirror/v1/repos/{owner}/{name}/pulls/{number}/threads")
        .operation_id("github_mirror.v1.list_review_threads")
        .summary("List mirrored review threads of a pull request")
        .description(
            "Returns review conversation threads (resolved state included) held in the local              mirror for the tenant",
        )
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("owner", "Repo owner login")
        .path_param("name", "Repo name")
        .path_param("number", "Pull request number")
        .query_param("limit", false, "Maximum number of threads to return")
        .handler(handlers::list_review_threads)
        .json_response_with_schema::<toolkit_odata::Page<dto::ReviewThreadDto>>(
            openapi,
            StatusCode::OK,
            "Paginated list of mirrored review threads",
        )
        .with_odata_filter::<ReviewThreadField>()
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router
}
