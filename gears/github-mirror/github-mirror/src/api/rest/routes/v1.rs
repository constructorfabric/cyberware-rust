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
            "Fetches the repository plus the first page of its entities from GitHub and              upserts them into the caller's tenant mirror. First slice of the sync engine:              no pagination, conditional requests, or rate-limit budgeting yet.",
        )
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("owner", "Repo owner login")
        .path_param("name", "Repo name")
        .handler(handlers::sync_repository)
        .json_response_with_schema::<dto::SyncSummaryDto>(
            openapi,
            StatusCode::OK,
            "Repo synced into the mirror",
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
