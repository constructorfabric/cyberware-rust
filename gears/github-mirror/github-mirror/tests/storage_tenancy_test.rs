#![allow(clippy::unwrap_used, clippy::expect_used, clippy::too_many_lines)]

mod common;

use github_mirror::domain::ports::github::FetchOptions;
use github_mirror::domain::repo::{ListingFilter, PageWindow, RepoRecord};
use github_mirror::domain::scope::{CollectionMode, ScopeConfig};
use github_mirror::domain::service::SyncProgress;
use toolkit_odata::ODataQuery;
use uuid::Uuid;

const OWNER: &str = "rust-lang";
const NAME: &str = "rust";
const ISSUE_NUMBER: i64 = 11;
const PULL_NUMBER: i64 = 12;
const COMMIT_SHA: &str = "c1";
const RUN_ID: i64 = 81;

fn collect_everything() -> ScopeConfig {
    let mut scope = ScopeConfig::default();
    scope.collection.actions = CollectionMode::All;
    scope.collection.reactions = CollectionMode::All;
    scope.collection.timeline = CollectionMode::All;
    scope
}

fn repo(id: i64, name: &str) -> RepoRecord {
    RepoRecord {
        node_id: None,
        id,
        owner: "acme".to_owned(),
        name: name.to_owned(),
        full_name: format!("acme/{name}"),
        default_branch: "main".to_owned(),
        private: true,
        pushed_at: Some("2026-08-18T00:00:00Z".to_owned()),
        stars: 7,
        forks: 2,
        description: None,
        clone_url: None,
    }
}

#[tokio::test]
async fn two_tenants_can_mirror_the_same_repository_without_collision() {
    let service = common::service("https://api.github.com").await;
    let tenant_a = common::caller_in(Uuid::new_v4());
    let tenant_b = common::caller_in(Uuid::new_v4());

    service
        .upsert_repo(&tenant_a, repo(500, "shared"))
        .await
        .unwrap_or_else(|e| panic!("tenant A upsert must succeed: {e}"));
    service
        .upsert_repo(&tenant_b, repo(500, "shared"))
        .await
        .unwrap_or_else(|e| panic!("tenant B upsert of the same repo id must succeed: {e}"));

    let page_a = service
        .list_repos(&tenant_a, &ODataQuery::default())
        .await
        .unwrap_or_else(|e| panic!("tenant A list must succeed: {e}"));
    let page_b = service
        .list_repos(&tenant_b, &ODataQuery::default())
        .await
        .unwrap_or_else(|e| panic!("tenant B list must succeed: {e}"));

    assert_eq!(page_a.items.len(), 1);
    assert_eq!(page_b.items.len(), 1);
}

#[tokio::test]
async fn queries_are_tenant_scoped() {
    let service = common::service("https://api.github.com").await;
    let tenant_a = common::caller_in(Uuid::new_v4());
    let tenant_b = common::caller_in(Uuid::new_v4());

    service
        .upsert_repo(&tenant_a, repo(1, "only-a"))
        .await
        .unwrap_or_else(|e| panic!("upsert must succeed: {e}"));

    let page_b = service
        .list_repos(&tenant_b, &ODataQuery::default())
        .await
        .unwrap_or_else(|e| panic!("list must succeed: {e}"));

    assert!(page_b.items.is_empty());
}

#[tokio::test]
async fn upsert_is_idempotent_and_updates_fields() {
    let service = common::service("https://api.github.com").await;
    let tenant = common::caller_in(Uuid::new_v4());

    service
        .upsert_repo(&tenant, repo(9, "thing"))
        .await
        .unwrap_or_else(|e| panic!("first upsert must succeed: {e}"));

    let mut updated = repo(9, "thing");
    updated.stars = 42;
    updated.description = Some("now described".to_owned());
    service
        .upsert_repo(&tenant, updated)
        .await
        .unwrap_or_else(|e| panic!("second upsert must succeed: {e}"));

    let page = service
        .list_repos(&tenant, &ODataQuery::default())
        .await
        .unwrap_or_else(|e| panic!("list must succeed: {e}"));

    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].description.as_deref(), Some("now described"));
}

#[tokio::test]
async fn list_respects_query_limit() {
    let service = common::service("https://api.github.com").await;
    let tenant = common::caller_in(Uuid::new_v4());

    for i in 0..5 {
        service
            .upsert_repo(&tenant, repo(i, &format!("repo-{i}")))
            .await
            .unwrap_or_else(|e| panic!("upsert must succeed: {e}"));
    }

    let page = service
        .list_repos(&tenant, &ODataQuery::default().with_limit(2))
        .await
        .unwrap_or_else(|e| panic!("list must succeed: {e}"));

    assert_eq!(page.items.len(), 2);
    assert_eq!(page.page_info.limit, 2);
}

#[tokio::test]
async fn a_pdp_denial_surfaces_as_forbidden() {
    use std::sync::Arc;

    use github_mirror::domain::error::DomainError;

    let db = common::inmem_db().await;
    let allowed = common::service_over(db.clone(), "https://api.github.com");
    let tenant = common::caller_in(Uuid::new_v4());
    allowed
        .upsert_repo(&tenant, repo(600, "guarded"))
        .await
        .unwrap_or_else(|e| panic!("seed upsert must succeed: {e}"));

    let denied = common::service_with_enforcer(
        db,
        "https://api.github.com",
        Arc::new(common::FakeGithub { result: None }),
        common::deny_enforcer(),
    );
    let err = denied
        .list_repos(&tenant, &ODataQuery::default())
        .await
        .expect_err("a denying PDP must not let the list through");

    assert!(
        matches!(err, DomainError::Forbidden(_)),
        "expected Forbidden, got {err:?}"
    );
}

#[tokio::test]
async fn every_child_listing_of_a_shared_repository_stays_with_its_tenant() {
    use std::sync::Arc;

    let fixture = common::fetched_repository();
    let service = common::service_with_github(
        common::inmem_db().await,
        "https://api.github.com",
        Arc::new(common::FakeGithub {
            result: Some(fixture.clone()),
        }),
    );
    let tenant_a = common::caller_in(Uuid::new_v4());
    let tenant_b = common::caller_in(Uuid::new_v4());

    for (tenant, who) in [(&tenant_a, "tenant A"), (&tenant_b, "tenant B")] {
        service
            .sync_repository(
                tenant,
                OWNER,
                NAME,
                &FetchOptions {
                    tenant_id: tenant.subject_tenant_id(),
                    scope: collect_everything(),
                    force: false,
                    since: None,
                },
                &SyncProgress::new(),
                &tokio_util::sync::CancellationToken::new(),
            )
            .await
            .unwrap_or_else(|e| panic!("{who} must be able to sync the shared repository: {e}"));
    }

    let window = PageWindow::first(50);
    let query = ODataQuery::default();
    let filter = ListingFilter::default();

    for (tenant, who) in [(&tenant_a, "tenant A"), (&tenant_b, "tenant B")] {
        let counted: Vec<(&str, usize, usize)> = vec![
            (
                "repositories",
                1,
                service
                    .list_repos(tenant, &query)
                    .await
                    .expect("repositories must list")
                    .items
                    .len(),
            ),
            (
                "issues",
                fixture.issues.len(),
                service
                    .list_issues(tenant, OWNER, NAME, window, filter)
                    .await
                    .expect("issues must list")
                    .0
                    .items
                    .len(),
            ),
            (
                "pull requests",
                fixture.pull_requests.len(),
                service
                    .list_pull_requests(tenant, OWNER, NAME, window, filter)
                    .await
                    .expect("pull requests must list")
                    .0
                    .items
                    .len(),
            ),
            (
                "commits",
                fixture.commits.len(),
                service
                    .list_commits(tenant, OWNER, NAME, window, None)
                    .await
                    .expect("commits must list")
                    .0
                    .items
                    .len(),
            ),
            (
                "comments",
                fixture.comments.len(),
                service
                    .list_comments(tenant, OWNER, NAME, ISSUE_NUMBER, window)
                    .await
                    .expect("comments must list")
                    .items
                    .len(),
            ),
            (
                "review comments",
                fixture.review_comments.len(),
                service
                    .list_review_comments(tenant, OWNER, NAME, PULL_NUMBER, window)
                    .await
                    .expect("review comments must list")
                    .items
                    .len(),
            ),
            (
                "reviews",
                fixture.reviews.len(),
                service
                    .list_reviews(tenant, OWNER, NAME, PULL_NUMBER, window)
                    .await
                    .expect("reviews must list")
                    .items
                    .len(),
            ),
            (
                "labels",
                fixture.labels.len(),
                service
                    .list_labels(tenant, OWNER, NAME, window)
                    .await
                    .expect("labels must list")
                    .items
                    .len(),
            ),
            (
                "milestones",
                fixture.milestones.len(),
                service
                    .list_milestones(tenant, OWNER, NAME, window)
                    .await
                    .expect("milestones must list")
                    .items
                    .len(),
            ),
            (
                "releases",
                fixture.releases.len(),
                service
                    .list_releases(tenant, OWNER, NAME, window)
                    .await
                    .expect("releases must list")
                    .items
                    .len(),
            ),
            (
                "branches",
                fixture.branches.len(),
                service
                    .list_branches(tenant, OWNER, NAME, window)
                    .await
                    .expect("branches must list")
                    .items
                    .len(),
            ),
            (
                "contributors",
                fixture.contributors.len(),
                service
                    .list_contributors(tenant, OWNER, NAME, window)
                    .await
                    .expect("contributors must list")
                    .items
                    .len(),
            ),
            (
                "workflow runs",
                fixture.workflow_runs.len(),
                service
                    .list_workflow_runs(tenant, OWNER, NAME, window)
                    .await
                    .expect("workflow runs must list")
                    .0
                    .items
                    .len(),
            ),
            (
                "pull request files",
                fixture.pull_request_files.len(),
                service
                    .list_pull_request_files(tenant, OWNER, NAME, PULL_NUMBER, window)
                    .await
                    .expect("pull request files must list")
                    .items
                    .len(),
            ),
            (
                "tags",
                fixture.tags.len(),
                service
                    .list_tags(tenant, OWNER, NAME, window)
                    .await
                    .expect("tags must list")
                    .items
                    .len(),
            ),
            (
                "commit files",
                fixture.commit_files.len(),
                service
                    .list_commit_files(tenant, OWNER, NAME, COMMIT_SHA, &query)
                    .await
                    .expect("commit files must list")
                    .items
                    .len(),
            ),
            (
                "review threads",
                fixture.review_threads.len(),
                service
                    .list_review_threads(tenant, OWNER, NAME, PULL_NUMBER, &query)
                    .await
                    .expect("review threads must list")
                    .items
                    .len(),
            ),
            (
                "commit comments",
                fixture.commit_comments.len(),
                service
                    .list_commit_comments(tenant, OWNER, NAME, COMMIT_SHA, window)
                    .await
                    .expect("commit comments must list")
                    .items
                    .len(),
            ),
            (
                "issue events",
                fixture.issue_events.len(),
                service
                    .list_issue_events(tenant, OWNER, NAME, ISSUE_NUMBER, window)
                    .await
                    .expect("issue events must list")
                    .items
                    .len(),
            ),
            (
                "deployments",
                fixture.deployments.len(),
                service
                    .list_deployments(tenant, OWNER, NAME, window)
                    .await
                    .expect("deployments must list")
                    .items
                    .len(),
            ),
            (
                "pull request commits",
                fixture.pull_request_commits.len(),
                service
                    .list_pull_request_commits(tenant, OWNER, NAME, PULL_NUMBER, window)
                    .await
                    .expect("pull request commits must list")
                    .items
                    .len(),
            ),
            (
                "commit statuses",
                fixture.commit_statuses.len(),
                service
                    .list_commit_statuses(tenant, OWNER, NAME, COMMIT_SHA, window)
                    .await
                    .expect("commit statuses must list")
                    .items
                    .len(),
            ),
            (
                "workflow jobs",
                fixture.workflow_jobs.len(),
                service
                    .list_workflow_jobs(tenant, OWNER, NAME, RUN_ID, window)
                    .await
                    .expect("workflow jobs must list")
                    .0
                    .items
                    .len(),
            ),
            (
                "issue reactions",
                fixture.issue_reactions.len(),
                service
                    .list_issue_reactions(tenant, OWNER, NAME, ISSUE_NUMBER, window)
                    .await
                    .expect("issue reactions must list")
                    .items
                    .len(),
            ),
            (
                "check runs",
                fixture.check_runs.len(),
                service
                    .list_check_runs(tenant, OWNER, NAME, COMMIT_SHA, window)
                    .await
                    .expect("check runs must list")
                    .0
                    .items
                    .len(),
            ),
            (
                "issue timeline events",
                fixture.issue_timeline.len(),
                service
                    .list_issue_timeline(tenant, OWNER, NAME, ISSUE_NUMBER, window)
                    .await
                    .expect("issue timeline must list")
                    .items
                    .len(),
            ),
        ];

        for (listing, expected, got) in counted {
            assert_eq!(
                got, expected,
                "{who} must see only its own {listing} of the shared repository"
            );
        }
    }
}
