#![allow(clippy::unwrap_used, clippy::expect_used, clippy::too_many_lines)]

mod common;

use std::sync::Arc;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use github_mirror::api::rest::routes::{ConcreteService, register_routes};
use github_mirror::domain::ports::github::{FetchedRepository, Listing, ListingCompleteness};
use github_mirror::domain::repo::{
    ContributorRecord, IssueRecord, IssueTimelineEventRecord, RepoRecord,
};
use toolkit::api::OpenApiRegistryImpl;
use toolkit_security::SecurityContext;
use tower::ServiceExt;
use uuid::Uuid;

fn router_for(service: Arc<ConcreteService>, ctx: SecurityContext) -> Router {
    let openapi = OpenApiRegistryImpl::new();
    register_routes(Router::new(), &openapi, service).layer(axum::Extension(ctx))
}

async fn body_json(response: axum::http::Response<Body>) -> serde_json::Value {
    let bytes = to_bytes(response.into_body(), 1_000_000).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

async fn post(router: Router, uri: &str) -> axum::http::Response<Body> {
    let request = Request::builder()
        .method("POST")
        .uri(uri)
        .body(Body::empty())
        .unwrap();
    router.oneshot(request).await.unwrap()
}

async fn get(router: Router, uri: &str) -> axum::http::Response<Body> {
    let request = Request::builder().uri(uri).body(Body::empty()).unwrap();
    router.oneshot(request).await.unwrap()
}

#[tokio::test]
async fn sync_fills_all_twenty_six_tables_and_reads_serve_them() {
    let ctx = common::caller_in(Uuid::new_v4());
    let db = common::inmem_db().await;
    let service = common::service_with_github(
        db,
        "https://api.github.com",
        Arc::new(common::FakeGithub {
            result: Some(common::fetched_repository()),
        }),
    );
    let router = router_for(service, ctx);

    let response = post(
        router.clone(),
        "/github-mirror/v1/repos/rust-lang/rust/sync",
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let summary = body_json(response).await;
    assert_eq!(summary["repository"], "rust-lang/rust");
    assert_eq!(summary["issues_synced"], 1);
    assert_eq!(summary["pull_requests_synced"], 1);
    assert_eq!(summary["commits_synced"], 2);
    assert_eq!(summary["comments_synced"], 1);
    assert_eq!(summary["review_comments_synced"], 1);
    assert_eq!(summary["reviews_synced"], 1);
    assert_eq!(summary["labels_synced"], 1);
    assert_eq!(summary["milestones_synced"], 1);
    assert_eq!(summary["releases_synced"], 1);
    assert_eq!(summary["branches_synced"], 1);
    assert_eq!(summary["contributors_synced"], 1);
    assert_eq!(summary["workflow_runs_synced"], 1);
    assert_eq!(summary["pull_request_files_synced"], 1);
    assert_eq!(summary["tags_synced"], 1);
    assert_eq!(summary["commit_files_synced"], 1);
    assert_eq!(summary["review_threads_synced"], 1);
    assert_eq!(summary["commit_comments_synced"], 1);
    assert_eq!(summary["issue_events_synced"], 1);
    assert_eq!(summary["deployments_synced"], 1);
    assert_eq!(summary["pull_request_commits_synced"], 1);
    assert_eq!(summary["commit_statuses_synced"], 1);
    assert_eq!(summary["workflow_jobs_synced"], 1);
    assert_eq!(summary["issue_reactions_synced"], 1);
    assert_eq!(summary["check_runs_synced"], 1);
    assert_eq!(summary["issue_timeline_synced"], 1);

    let repos = body_json(get(router.clone(), "/github-mirror/v1/repos").await).await;
    assert_eq!(repos["items"][0]["full_name"], "rust-lang/rust");

    let issues = body_json(get(router.clone(), "/repos/rust-lang/rust/issues").await).await;
    assert_eq!(issues.as_array().expect("items").len(), 1);

    let pulls = body_json(get(router.clone(), "/repos/rust-lang/rust/pulls").await).await;
    assert_eq!(pulls.as_array().expect("items").len(), 1);

    let commits = body_json(get(router.clone(), "/repos/rust-lang/rust/commits").await).await;
    let router2 = router;
    assert_eq!(commits.as_array().expect("items").len(), 2);
    assert_eq!(commits[0]["sha"], "c2");

    let comments =
        body_json(get(router2.clone(), "/repos/rust-lang/rust/issues/11/comments").await).await;
    assert_eq!(comments.as_array().expect("items").len(), 1);
    assert_eq!(comments[0]["user"]["login"], "carol");

    let review_comments =
        body_json(get(router2.clone(), "/repos/rust-lang/rust/pulls/12/comments").await).await;
    assert_eq!(review_comments.as_array().expect("items").len(), 1);
    assert_eq!(review_comments[0]["user"]["login"], "dave");
    assert_eq!(review_comments[0]["path"], "src/lib.rs");

    let reviews =
        body_json(get(router2.clone(), "/repos/rust-lang/rust/pulls/12/reviews").await).await;
    assert_eq!(reviews.as_array().expect("items").len(), 1);
    assert_eq!(reviews[0]["user"]["login"], "erin");
    assert_eq!(reviews[0]["state"], "APPROVED");

    let labels = body_json(get(router2.clone(), "/repos/rust-lang/rust/labels").await).await;
    assert_eq!(labels.as_array().expect("items").len(), 1);
    assert_eq!(labels[0]["name"], "bug");
    assert_eq!(labels[0]["default"], true);

    let milestones =
        body_json(get(router2.clone(), "/repos/rust-lang/rust/milestones").await).await;
    assert_eq!(milestones.as_array().expect("items").len(), 1);
    assert_eq!(milestones[0]["title"], "v1.0");
    assert_eq!(milestones[0]["closed_issues"], 7);

    let releases = body_json(get(router2.clone(), "/repos/rust-lang/rust/releases").await).await;
    assert_eq!(releases.as_array().expect("items").len(), 1);
    assert_eq!(releases[0]["tag_name"], "v1.0.0");
    assert_eq!(releases[0]["author"]["login"], "erin");

    let branches = body_json(get(router2.clone(), "/repos/rust-lang/rust/branches").await).await;
    assert_eq!(branches.as_array().expect("items").len(), 1);
    assert_eq!(branches[0]["name"], "master");
    assert_eq!(branches[0]["commit"]["sha"], "c2");

    let contributors =
        body_json(get(router2.clone(), "/repos/rust-lang/rust/contributors").await).await;
    assert_eq!(contributors.as_array().expect("items").len(), 1);
    assert_eq!(contributors[0]["login"], "alice");

    let workflow_runs =
        body_json(get(router2.clone(), "/repos/rust-lang/rust/actions/runs").await).await;
    assert_eq!(
        workflow_runs["workflow_runs"]
            .as_array()
            .expect("items")
            .len(),
        1
    );
    assert_eq!(workflow_runs["workflow_runs"][0]["run_number"], 300);
    assert_eq!(workflow_runs["workflow_runs"][0]["conclusion"], "success");

    let pull_files =
        body_json(get(router2.clone(), "/repos/rust-lang/rust/pulls/12/files").await).await;
    assert_eq!(pull_files.as_array().expect("items").len(), 1);
    assert_eq!(pull_files[0]["filename"], "src/lib.rs");
    assert_eq!(pull_files[0]["additions"], 10);

    let tags = body_json(get(router2.clone(), "/repos/rust-lang/rust/tags").await).await;
    assert_eq!(tags.as_array().expect("items").len(), 1);
    assert_eq!(tags[0]["name"], "v1.0.0");
    assert_eq!(tags[0]["commit"]["sha"], "c1");

    let commit_files = body_json(
        get(
            router2.clone(),
            "/github-mirror/v1/repos/rust-lang/rust/commits/c1/files",
        )
        .await,
    )
    .await;
    assert_eq!(commit_files["items"].as_array().expect("items").len(), 1);
    assert_eq!(commit_files["items"][0]["filename"], "src/lib.rs");
    assert_eq!(commit_files["items"][0]["additions"], 4);

    let threads = body_json(
        get(
            router2.clone(),
            "/github-mirror/v1/repos/rust-lang/rust/pulls/12/threads",
        )
        .await,
    )
    .await;
    assert_eq!(threads["items"].as_array().expect("items").len(), 1);
    assert_eq!(threads["items"][0]["is_resolved"], true);
    assert_eq!(threads["items"][0]["resolved_by"], "erin");

    let commit_comments =
        body_json(get(router2.clone(), "/repos/rust-lang/rust/commits/c1/comments").await).await;
    let commit_comments = commit_comments.as_array().expect("items");
    assert_eq!(commit_comments.len(), 1);
    assert_eq!(commit_comments[0]["user"]["login"], "frank");
    assert_eq!(commit_comments[0]["commit_id"], "c1");

    let issue_events =
        body_json(get(router2.clone(), "/repos/rust-lang/rust/issues/11/events").await).await;
    let issue_events = issue_events.as_array().expect("items");
    assert_eq!(issue_events.len(), 1);
    assert_eq!(issue_events[0]["event"], "labeled");
    assert_eq!(issue_events[0]["label"]["name"], "bug");

    let deployments =
        body_json(get(router2.clone(), "/repos/rust-lang/rust/deployments").await).await;
    let deployments = deployments.as_array().expect("items");
    assert_eq!(deployments.len(), 1);
    assert_eq!(deployments[0]["environment"], "production");
    assert_eq!(deployments[0]["ref"], "master");
    assert_eq!(deployments[0]["creator"]["login"], "heidi");

    let pull_commits =
        body_json(get(router2.clone(), "/repos/rust-lang/rust/pulls/12/commits").await).await;
    let pull_commits = pull_commits.as_array().expect("items");
    assert_eq!(pull_commits.len(), 1);
    assert_eq!(pull_commits[0]["sha"], "pc1");
    assert_eq!(pull_commits[0]["commit"]["message"], "pr commit");
    assert_eq!(pull_commits[0]["author"]["login"], "ivan");

    let statuses =
        body_json(get(router2.clone(), "/repos/rust-lang/rust/commits/c1/statuses").await).await;
    let statuses = statuses.as_array().expect("items");
    assert_eq!(statuses.len(), 1);
    assert_eq!(statuses[0]["state"], "success");
    assert_eq!(statuses[0]["context"], "ci/build");
    assert_eq!(statuses[0]["creator"]["login"], "judy");

    let jobs =
        body_json(get(router2.clone(), "/repos/rust-lang/rust/actions/runs/7/jobs").await).await;
    assert_eq!(jobs["total_count"], 1);
    let jobs = jobs["jobs"].as_array().expect("jobs");
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0]["id"], 910);
    assert_eq!(jobs[0]["name"], "build");
    assert_eq!(jobs[0]["steps"][0]["name"], "Checkout");

    let reactions =
        body_json(get(router2.clone(), "/repos/rust-lang/rust/issues/11/reactions").await).await;
    let reactions = reactions.as_array().expect("items");
    assert_eq!(reactions.len(), 1);
    assert_eq!(reactions[0]["id"], 555);
    assert_eq!(reactions[0]["content"], "heart");
    assert_eq!(reactions[0]["user"]["login"], "kate");

    let checks = body_json(
        get(
            router2.clone(),
            "/repos/rust-lang/rust/commits/c1/check-runs",
        )
        .await,
    )
    .await;
    assert_eq!(checks["total_count"], 1);
    let checks = checks["check_runs"].as_array().expect("check_runs");
    assert_eq!(checks.len(), 1);
    assert_eq!(checks[0]["id"], 771);
    assert_eq!(checks[0]["name"], "clippy");
    assert_eq!(checks[0]["check_suite"]["id"], 900);
    assert_eq!(checks[0]["app"]["slug"], "github-actions");
    assert_eq!(checks[0]["output"]["title"], "no warnings");

    let timeline = body_json(get(router2, "/repos/rust-lang/rust/issues/11/timeline").await).await;
    let timeline = timeline.as_array().expect("items");
    assert_eq!(timeline.len(), 1);
    assert_eq!(timeline[0]["event"], "labeled");
    assert_eq!(timeline[0]["label"]["name"], "bug");
}

#[tokio::test]
async fn sync_of_unknown_repository_returns_404() {
    let ctx = common::caller_in(Uuid::new_v4());
    let service = common::service("https://api.github.com").await;
    let router = router_for(service, ctx);

    let response = post(router, "/github-mirror/v1/repos/acme/nope/sync").await;

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn sync_is_idempotent() {
    let ctx = common::caller_in(Uuid::new_v4());
    let db = common::inmem_db().await;
    let service = common::service_with_github(
        db,
        "https://api.github.com",
        Arc::new(common::FakeGithub {
            result: Some(common::fetched_repository()),
        }),
    );
    let router = router_for(service, ctx);

    let listings = [
        "/repos/rust-lang/rust/issues",
        "/repos/rust-lang/rust/pulls",
        "/repos/rust-lang/rust/commits",
        "/repos/rust-lang/rust/labels",
        "/repos/rust-lang/rust/milestones",
        "/repos/rust-lang/rust/releases",
        "/repos/rust-lang/rust/branches",
        "/repos/rust-lang/rust/tags",
        "/repos/rust-lang/rust/contributors",
    ];

    let first = post(
        router.clone(),
        "/github-mirror/v1/repos/rust-lang/rust/sync",
    )
    .await;
    assert_eq!(first.status(), StatusCode::OK);

    let mut after_first = Vec::new();
    for path in listings {
        let response = get(router.clone(), path).await;
        assert_eq!(response.status(), StatusCode::OK, "{path} must list");
        let listed = body_json(response).await;
        after_first.push(
            listed
                .as_array()
                .unwrap_or_else(|| panic!("{path} must answer with an array"))
                .len(),
        );
    }

    let second = post(
        router.clone(),
        "/github-mirror/v1/repos/rust-lang/rust/sync",
    )
    .await;
    assert_eq!(second.status(), StatusCode::OK);

    let repos = body_json(get(router.clone(), "/github-mirror/v1/repos").await).await;
    assert_eq!(repos["items"].as_array().expect("items").len(), 1);

    for (path, expected) in listings.into_iter().zip(after_first) {
        let response = get(router.clone(), path).await;
        assert_eq!(response.status(), StatusCode::OK, "{path} must list");
        let listed = body_json(response).await;
        assert_eq!(
            listed
                .as_array()
                .unwrap_or_else(|| panic!("{path} must answer with an array"))
                .len(),
            expected,
            "{path} must hold the same rows after a second sync, not duplicates"
        );
    }
}

#[tokio::test]
async fn a_concurrent_sync_for_the_same_repo_is_rejected_with_a_conflict() {
    let ctx = common::caller_in(Uuid::new_v4());
    let tenant_id = ctx.subject_tenant_id();
    let db = common::inmem_db().await;
    let service = common::service_with_github(
        db.clone(),
        "https://api.github.com",
        Arc::new(common::FakeGithub {
            result: Some(common::fetched_repository()),
        }),
    );

    // Stand in for a sync already mid-flight: hold the exact per-repo
    // advisory lock the service takes, then ask for another sync of the
    // same repo.
    let held = db
        .lock("github-mirror", &format!("sync/{tenant_id}/rust-lang/rust"))
        .await
        .expect("test must be able to hold the sync lock");

    let router = router_for(service, ctx);
    let response = post(
        router.clone(),
        "/github-mirror/v1/repos/rust-lang/rust/sync",
    )
    .await;
    assert_eq!(
        response.status(),
        StatusCode::CONFLICT,
        "the second sync must be told a sync is already running, not race it"
    );

    // The body is the intended public detail, pinned here so it cannot drift:
    // the repository is the one the caller itself asked to sync, and it is
    // told which of the two things went wrong rather than a bare "conflict".
    let json = body_json(response).await;
    assert_eq!(
        json["detail"], "a sync for rust-lang/rust is already running",
        "the caller must learn a sync is running, and for which repo: {json:?}"
    );

    // A different repo is not blocked by this repo's lock.
    let other = post(
        router.clone(),
        "/github-mirror/v1/repos/rust-lang/cargo/sync",
    )
    .await;
    assert_eq!(other.status(), StatusCode::OK);

    held.release().await.expect("release must succeed");

    // Once the first sync finishes, the repo can be synced again.
    let retry = post(router, "/github-mirror/v1/repos/rust-lang/rust/sync").await;
    assert_eq!(retry.status(), StatusCode::OK);
}

/// A repo with exactly the given issues, everything else empty; `issues`
/// listing completeness as given.
fn recon_fetched(issue_ids: &[i64], issues_complete: bool) -> FetchedRepository {
    let mut result = common::fetched_repository();
    result.repository = RepoRecord {
        node_id: None,
        id: 77,
        owner: "acme".to_owned(),
        name: "recon".to_owned(),
        full_name: "acme/recon".to_owned(),
        default_branch: "main".to_owned(),
        private: false,
        pushed_at: None,
        stars: 0,
        forks: 0,
        description: None,
        clone_url: None,
    };
    result.issues = issue_ids
        .iter()
        .map(|id| IssueRecord {
            author_login: Some("alice".to_owned()),
            author_json: None,
            assignees_json: None,
            labels_json: None,
            comments_count: None,
            locked: None,
            node_id: None,
            id: *id,
            repo_id: 77,
            number: *id,
            title: format!("issue {id}"),
            body: None,
            state: "open".to_owned(),
            is_pull_request: false,
            created_at: "2026-08-20T00:00:00Z".to_owned(),
            updated_at: "2026-08-20T00:00:00Z".to_owned(),
            closed_at: None,
            html_url: None,
        })
        .collect();
    result.pull_requests = vec![];
    result.commits = vec![];
    result.comments = vec![];
    result.review_comments = vec![];
    result.reviews = vec![];
    result.labels = vec![];
    result.milestones = vec![];
    result.releases = vec![];
    result.branches = vec![];
    result.contributors = vec![];
    result.workflow_runs = vec![];
    result.pull_request_files = vec![];
    result.tags = vec![];
    result.commit_files = vec![];
    result.review_threads = vec![];
    result.commit_comments = vec![];
    result.issue_events = vec![];
    result.deployments = vec![];
    result.pull_request_commits = vec![];
    result.commit_statuses = vec![];
    result.workflow_jobs = vec![];
    result.issue_reactions = vec![];
    result.check_runs = vec![];
    result.issue_timeline = vec![];
    let mut complete = ListingCompleteness::all_complete();
    complete.set(Listing::Issues, issues_complete);
    result.complete = complete;
    result
}

/// Run one inline sync of acme/recon against the given upstream state.
async fn sync_recon(db: toolkit_db::Db, ctx: &SecurityContext, upstream: FetchedRepository) {
    let service = common::service_with_github(
        db,
        "https://api.github.com",
        Arc::new(common::FakeGithub {
            result: Some(upstream),
        }),
    );
    let router = router_for(service, ctx.clone());
    let response = post(router, "/github-mirror/v1/repos/acme/recon/sync").await;
    assert_eq!(response.status(), StatusCode::OK);
}

async fn recon_issue_ids(db: toolkit_db::Db, ctx: &SecurityContext) -> Vec<i64> {
    let service = common::service_with_github(
        db,
        "https://api.github.com",
        Arc::new(common::FakeGithub { result: None }),
    );
    let router = router_for(service, ctx.clone());
    let json = body_json(get(router, "/repos/acme/recon/issues?state=all").await).await;
    json.as_array()
        .expect("items")
        .iter()
        .map(|i| i["id"].as_i64().expect("id"))
        .collect()
}

#[tokio::test]
async fn reconciliation_deletes_upstream_removals_but_only_from_complete_listings() {
    let ctx = common::caller_in(Uuid::new_v4());
    let db = common::inmem_db().await;

    // Sync 1: issues 11 and 12 exist upstream.
    sync_recon(db.clone(), &ctx, recon_fetched(&[11, 12], true)).await;
    assert_eq!(recon_issue_ids(db.clone(), &ctx).await, vec![11, 12]);

    // Sync 2: issue 12 vanished upstream, but the listing was truncated —
    // absence proves nothing, so nothing may be deleted.
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    sync_recon(db.clone(), &ctx, recon_fetched(&[11], false)).await;
    assert_eq!(
        recon_issue_ids(db.clone(), &ctx).await,
        vec![11, 12],
        "a truncated listing must not reconcile deletions"
    );

    // Sync 3: same upstream state, complete listing — now 12 goes.
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    sync_recon(db.clone(), &ctx, recon_fetched(&[11], true)).await;
    assert_eq!(
        recon_issue_ids(db.clone(), &ctx).await,
        vec![11],
        "a complete listing reconciles the upstream deletion"
    );
}

/// A repo whose only content is one issue authored by `user_id`, with the
/// issues listing complete.
fn contributor_fetched(user_id: i64, login: &str, roles: &[&str], seen: &str) -> FetchedRepository {
    let mut result = recon_fetched(&[11], true);
    result.contributors = vec![ContributorRecord {
        repo_id: 77,
        user_id,
        login: Some(login.to_owned()),
        account_type: "User".to_owned(),
        avatar_url: None,
        html_url: None,
        roles: roles.iter().map(|r| (*r).to_owned()).collect(),
        first_seen_at: Some(common::instant(seen)),
        last_seen_at: Some(common::instant(seen)),
    }];
    result
}

#[tokio::test]
async fn derived_contributor_roles_accumulate_across_syncs() {
    let ctx = common::caller_in(Uuid::new_v4());
    let db = common::inmem_db().await;

    // A first sync sees alice only as an issue author.
    sync_recon(
        db.clone(),
        &ctx,
        contributor_fetched(71, "alice", &["author"], "2026-05-01T00:00:00Z"),
    )
    .await;

    // A later, narrower sync sees her only reviewing. Neither run knows the
    // whole story, so the stored row must hold both.
    sync_recon(
        db.clone(),
        &ctx,
        contributor_fetched(71, "alice", &["reviewer"], "2026-08-01T00:00:00Z"),
    )
    .await;

    let service = common::service_with_github(
        db,
        "https://api.github.com",
        Arc::new(common::FakeGithub { result: None }),
    );
    let router = router_for(service, ctx);
    let json = body_json(get(router, "/repos/acme/recon/contributors").await).await;
    let people = json.as_array().expect("items");

    assert_eq!(people.len(), 1, "the same person must not be duplicated");
    assert_eq!(people[0]["login"], "alice");
    assert_eq!(
        people[0]["roles"],
        serde_json::json!(["author", "reviewer"]),
        "a narrower run must not erase what an earlier one saw"
    );
    assert_eq!(
        people[0]["first_seen_at"], "2026-05-01T00:00:00Z",
        "the window keeps the earliest sighting"
    );
    assert_eq!(people[0]["last_seen_at"], "2026-08-01T00:00:00Z");
}

/// A repo whose only content is `events` timeline entries on issue 11.
fn timeline_fetched(events: &[&str]) -> FetchedRepository {
    let mut result = recon_fetched(&[11], true);
    result.issue_timeline = events
        .iter()
        .enumerate()
        .map(|(position, event)| IssueTimelineEventRecord {
            repo_id: 77,
            issue_number: 11,
            position: i64::try_from(position).expect("test positions are small"),
            event: (*event).to_owned(),
            created_at: Some("2026-08-20T00:00:00Z".to_owned()),
            actor_login: Some("kate".to_owned()),
            payload_json: format!("{{\"event\":\"{event}\"}}"),
        })
        .collect();
    result
}

#[tokio::test]
async fn a_shorter_timeline_does_not_leave_the_previous_tail_behind() {
    let ctx = common::caller_in(Uuid::new_v4());
    let db = common::inmem_db().await;

    // First sync sees four entries.
    sync_recon(
        db.clone(),
        &ctx,
        timeline_fetched(&["labeled", "commented", "assigned", "closed"]),
    )
    .await;

    // The comment is deleted upstream, so its entry disappears and everything
    // after it shifts down a position.
    sync_recon(
        db.clone(),
        &ctx,
        timeline_fetched(&["labeled", "assigned", "closed"]),
    )
    .await;

    let service = common::service_with_github(
        db,
        "https://api.github.com",
        Arc::new(common::FakeGithub { result: None }),
    );
    let router = router_for(service, ctx);
    let json = body_json(get(router, "/repos/acme/recon/issues/11/timeline").await).await;
    let events: Vec<&str> = json
        .as_array()
        .expect("items")
        .iter()
        .map(|e| e["event"].as_str().expect("event"))
        .collect();

    assert_eq!(
        events,
        vec!["labeled", "assigned", "closed"],
        "re-syncing a shorter timeline must not keep the old tail"
    );
}
