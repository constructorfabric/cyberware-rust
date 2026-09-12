#![allow(clippy::unwrap_used, clippy::expect_used, clippy::too_many_lines)]

use github_mirror::domain::error::DomainError;
use github_mirror::domain::ports::github::{
    CommitListing, FetchOptions, FetchedRepository, GithubPort, IssueDetailWants, IssueListing,
    Listing, ListingCompleteness, PullListing,
};
use github_mirror::domain::repo::ContributorRecord;
use github_mirror::domain::scope::{CollectionMode, ScopeConfig};
use github_mirror::infra::github::cache::{CacheKey, CachedResponse, HttpCache};
use github_mirror::infra::github::client::GithubClient;
use httpmock::MockServer;
use serde_json::json;

/// An RFC3339 literal as the instant the mirror stores.
fn instant(raw: &str) -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::parse_from_rfc3339(raw)
        .expect("test timestamps must be valid RFC3339")
        .with_timezone(&chrono::Utc)
}

fn gh_repo_json() -> serde_json::Value {
    json!({
        "id": 42,
        "name": "rust",
        "full_name": "rust-lang/rust",
        "owner": { "login": "rust-lang" },
        "default_branch": "master",
        "private": false,
        "pushed_at": "2026-08-20T00:00:00Z",
        "stargazers_count": 100_000,
        "forks_count": 13_000,
        "clone_url": "https://github.com/rust-lang/rust.git",
        "description": "the compiler"
    })
}

fn gh_issues_json() -> serde_json::Value {
    json!([
        {
            "id": 1, "number": 11, "title": "an issue", "body": "text",
            "user": { "id": 71, "login": "alice", "type": "User",
                      "avatar_url": "https://avatars.githubusercontent.com/u/71",
                      "html_url": "https://github.com/alice" },
            "assignees": [ { "id": 73, "login": "carol", "type": "User" } ],
            "state": "open", "created_at": "2026-08-20T00:00:00Z",
            "updated_at": "2026-08-20T00:00:00Z", "closed_at": null,
            "html_url": "https://github.com/rust-lang/rust/issues/11"
        },
        {
            "id": 2, "number": 12, "title": "a pr shown as issue",
            "state": "open", "pull_request": {},
            "created_at": "2026-08-20T00:00:00Z",
            "updated_at": "2026-08-20T00:00:00Z"
        }
    ])
}

fn gh_pulls_json() -> serde_json::Value {
    json!([
        {
            "id": 3, "number": 13, "title": "a pr", "state": "open",
            "user": { "id": 75, "login": "erin", "type": "User" },
            "draft": true, "merged_at": null,
            "head": { "sha": "h1", "ref": "feature" },
            "base": { "sha": "b1", "ref": "master" },
            "html_url": "https://github.com/rust-lang/rust/pull/12",
            "created_at": "2026-08-20T00:00:00Z",
            "updated_at": "2026-08-20T00:00:00Z", "closed_at": null
        }
    ])
}

fn gh_commits_json() -> serde_json::Value {
    json!([
        {
            "sha": "c1",
            "commit": {
                "message": "first",
                "author": { "date": "2026-08-19T00:00:00Z" },
                "committer": { "date": "2026-08-19T00:00:00Z" }
            },
            "author": { "id": 71, "login": "alice", "type": "User" },
            "committer": { "id": 72, "login": "bob", "type": "User" }
        }
    ])
}

fn gh_comments_json() -> serde_json::Value {
    json!([
        {
            "id": 7,
            "user": { "id": 73, "login": "carol", "type": "User" },
            "body": "looks good",
            "created_at": "2026-08-20T00:00:00Z",
            "updated_at": "2026-08-20T00:00:00Z",
            "html_url": "https://github.com/rust-lang/rust/issues/11#issuecomment-7",
            "issue_url": "https://api.github.com/repos/rust-lang/rust/issues/11"
        }
    ])
}

fn gh_review_comments_json() -> serde_json::Value {
    json!([
        {
            "id": 21,
            "user": { "id": 74, "login": "dave", "type": "User" },
            "body": "rename this",
            "path": "src/lib.rs",
            "diff_hunk": "@@ -1 +1 @@",
            "in_reply_to_id": null,
            "commit_id": "h1",
            "created_at": "2026-08-20T00:00:00Z",
            "updated_at": "2026-08-20T00:00:00Z",
            "html_url": "https://github.com/rust-lang/rust/pull/13#discussion_r21",
            "pull_request_url": "https://api.github.com/repos/rust-lang/rust/pulls/13"
        }
    ])
}

fn gh_reviews_json() -> serde_json::Value {
    json!([
        {
            "id": 31,
            "user": { "id": 75, "login": "erin", "type": "User" },
            "state": "APPROVED",
            "body": "ship it",
            "commit_id": "h1",
            "submitted_at": "2026-08-20T00:00:00Z",
            "html_url": "https://github.com/rust-lang/rust/pull/13#pullrequestreview-31"
        }
    ])
}

fn gh_labels_json() -> serde_json::Value {
    json!([
        {
            "id": 41,
            "name": "bug",
            "color": "d73a4a",
            "default": true,
            "description": "Something is not working"
        }
    ])
}

fn gh_milestones_json() -> serde_json::Value {
    json!([
        {
            "id": 51,
            "number": 1,
            "title": "v1.0",
            "state": "open",
            "description": "first stable",
            "open_issues": 3,
            "closed_issues": 7,
            "due_on": "2026-09-30T00:00:00Z",
            "created_at": "2026-08-20T00:00:00Z",
            "updated_at": "2026-08-20T00:00:00Z",
            "closed_at": null,
            "html_url": "https://github.com/rust-lang/rust/milestone/1"
        }
    ])
}

fn gh_releases_json() -> serde_json::Value {
    json!([
        {
            "id": 61,
            "tag_name": "v1.0.0",
            "name": "First stable",
            "draft": false,
            "prerelease": false,
            "body": "changelog",
            "author": { "login": "erin" },
            "created_at": "2026-08-20T00:00:00Z",
            "published_at": "2026-08-20T00:00:00Z",
            "html_url": "https://github.com/rust-lang/rust/releases/tag/v1.0.0"
        }
    ])
}

fn gh_branches_json() -> serde_json::Value {
    json!([
        {
            "name": "master",
            "commit": { "sha": "c1" },
            "protected": true
        }
    ])
}

fn gh_workflow_runs_json() -> serde_json::Value {
    json!({
        "total_count": 1,
        "workflow_runs": [
            {
                "id": 81,
                "workflow_id": 8,
                "run_number": 300,
                "run_attempt": 2,
                "name": "CI",
                "event": "push",
                "status": "completed",
                "conclusion": "success",
                "head_branch": "master",
                "head_sha": "c1",
                "actor": { "login": "alice" },
                "created_at": "2026-08-20T00:00:00Z",
                "updated_at": "2026-08-20T00:00:00Z",
                "html_url": "https://github.com/rust-lang/rust/actions/runs/81"
            }
        ]
    })
}

fn gh_check_runs_json() -> serde_json::Value {
    json!({
        "total_count": 1,
        "check_runs": [
            {
                "id": 771,
                "head_sha": "c1",
                "name": "clippy",
                "status": "completed",
                "conclusion": "success",
                "started_at": "2026-08-20T00:00:00Z",
                "completed_at": "2026-08-20T00:03:00Z",
                "html_url": "https://github.com/rust-lang/rust/runs/771",
                "details_url": "https://ci.example.com/771",
                "check_suite": { "id": 900 },
                "app": { "slug": "github-actions", "name": "GitHub Actions" },
                "output": {
                    "title": "no warnings",
                    "summary": "clippy is happy",
                    "annotations_count": 0
                }
            }
        ]
    })
}

fn gh_issue_timeline_json() -> serde_json::Value {
    json!([
        {
            "event": "labeled",
            "actor": { "login": "kate" },
            "label": { "name": "bug" },
            "created_at": "2026-08-20T00:00:00Z"
        },
        {
            "event": "committed",
            "sha": "c1",
            "message": "fix it",
            "author": { "name": "Ivan", "date": "2026-08-20T01:00:00Z" }
        }
    ])
}

fn gh_issue_reactions_json() -> serde_json::Value {
    json!([
        {
            "id": 555,
            "content": "heart",
            "user": { "login": "kate" },
            "created_at": "2026-08-20T00:00:00Z"
        }
    ])
}

fn gh_workflow_jobs_json() -> serde_json::Value {
    json!({
        "total_count": 1,
        "jobs": [
            {
                "id": 910,
                "run_id": 81,
                "run_attempt": 2,
                "name": "build",
                "status": "completed",
                "conclusion": "success",
                "head_sha": "c1",
                "runner_name": "ubuntu-latest",
                "started_at": "2026-08-20T00:00:00Z",
                "completed_at": "2026-08-20T00:05:00Z",
                "html_url": "https://github.com/rust-lang/rust/actions/runs/81/job/910",
                "steps": [
                    { "name": "Checkout", "status": "completed", "conclusion": "success", "number": 1 }
                ]
            }
        ]
    })
}

fn gh_pull_files_json() -> serde_json::Value {
    json!([
        {
            "filename": "src/lib.rs",
            "status": "modified",
            "additions": 10,
            "deletions": 2,
            "changes": 12,
            "sha": "blob1"
        },
        {
            "filename": "README.md",
            "status": "renamed",
            "additions": 1,
            "deletions": 0,
            "changes": 1,
            "previous_filename": "README.rst",
            "sha": "blob2"
        }
    ])
}

fn gh_tags_json() -> serde_json::Value {
    json!([
        {
            "name": "v1.0.0",
            "commit": { "sha": "c1" }
        }
    ])
}

fn gh_commit_detail_json() -> serde_json::Value {
    json!({
        "sha": "c1",
        "commit": {
            "message": "first",
            "author": { "date": "2026-08-19T00:00:00Z" },
            "committer": { "date": "2026-08-19T00:00:00Z" }
        },
        "author": { "id": 71, "login": "alice", "type": "User" },
        "committer": { "id": 72, "login": "bob", "type": "User" },
        "stats": { "additions": 4, "deletions": 1, "total": 5 },
        "files": [
            {
                "filename": "src/lib.rs",
                "status": "modified",
                "additions": 4,
                "deletions": 1,
                "changes": 5,
                "sha": "blob9"
            }
        ]
    })
}

fn gh_review_threads_json() -> serde_json::Value {
    json!({
        "data": {
            "repository": {
                "pullRequest": {
                    "reviewThreads": {
                        "nodes": [
                            {
                                "id": "PRRT_thread1",
                                "isResolved": true,
                                "isOutdated": false,
                                "path": "src/lib.rs",
                                "line": 10,
                                "resolvedBy": { "login": "erin" },
                                "comments": { "totalCount": 3 }
                            }
                        ]
                    }
                }
            }
        }
    })
}

fn gh_commit_comments_json() -> serde_json::Value {
    json!([
        {
            "id": 91,
            "user": { "login": "frank" },
            "commit_id": "c1",
            "path": null,
            "position": null,
            "body": "nice commit",
            "created_at": "2026-08-20T00:00:00Z",
            "updated_at": "2026-08-20T00:00:00Z",
            "html_url": "https://github.com/rust-lang/rust/commit/c1#commitcomment-91"
        }
    ])
}

fn gh_issue_events_json() -> serde_json::Value {
    json!([
        {
            "id": 101,
            "event": "labeled",
            "actor": { "login": "grace" },
            "label": { "name": "bug" },
            "commit_id": null,
            "created_at": "2026-08-20T00:00:00Z",
            "issue": { "number": 11 }
        }
    ])
}

fn gh_deployments_json() -> serde_json::Value {
    json!([
        {
            "id": 111,
            "ref": "master",
            "sha": "c2",
            "environment": "production",
            "task": "deploy",
            "description": "ship",
            "creator": { "login": "heidi" },
            "created_at": "2026-08-20T00:00:00Z",
            "updated_at": "2026-08-20T00:00:00Z"
        }
    ])
}

fn gh_pull_commits_json() -> serde_json::Value {
    json!([
        {
            "sha": "pc1",
            "commit": {
                "message": "pr commit",
                "author": { "date": "2026-08-20T00:00:00Z" },
                "committer": { "date": "2026-08-20T00:00:00Z" }
            },
            "author": { "login": "ivan" },
            "committer": { "login": "ivan" }
        }
    ])
}

fn gh_commit_statuses_json() -> serde_json::Value {
    json!([
        {
            "id": 121,
            "state": "success",
            "context": "ci/build",
            "description": "build passed",
            "target_url": "https://ci.example.com/1",
            "creator": { "login": "judy" },
            "created_at": "2026-08-20T00:00:00Z",
            "updated_at": "2026-08-20T00:00:00Z"
        }
    ])
}

/// Fetch options for a test: a fresh tenant, no force, the given scope.
fn opts(scope: ScopeConfig) -> FetchOptions {
    FetchOptions {
        tenant_id: uuid::Uuid::new_v4(),
        scope,
        force: false,
        since: None,
    }
}

/// The type default with timeline turned on, so every family the client maps
/// is exercised; a stock deployment leaves timeline off (PRD §5.2).
fn full_scope() -> ScopeConfig {
    let mut scope = github_mirror::config::GithubMirrorConfig::default().scope;
    scope.collection.timeline = CollectionMode::Open;
    scope
}

/// Everything the sync's tasks would fetch for one repository, gathered into
/// the one-value shape these tests assert on: the port is called the way the
/// phases call it — listings first, then one refinement per entity.
/// Every page of the issue family, merged, the way the worker's loop sees it.
#[allow(clippy::too_many_arguments)]
async fn walk_issues(
    client: &GithubClient,
    owner: &str,
    name: &str,
    repo_id: i64,
    updated_after: Option<chrono::DateTime<chrono::Utc>>,
    page1_etag: Option<&str>,
    options: &FetchOptions,
) -> Result<IssueListing, DomainError> {
    let mut all = IssueListing::default();
    let mut continue_from: Option<String> = None;
    loop {
        let page = client
            .list_issues(
                owner,
                name,
                repo_id,
                updated_after,
                page1_etag,
                continue_from.as_deref(),
                options,
            )
            .await?;
        all.complete.absorb(&page.complete);
        all.issues.extend(page.issues);
        all.comments.extend(page.comments);
        all.issue_events.extend(page.issue_events);
        all.contributors.extend(page.contributors);
        all.swept_to_end |= page.swept_to_end;
        all.unchanged |= page.unchanged;
        if all.page1_etag.is_none() {
            all.page1_etag = page.page1_etag;
        }
        match page.next {
            Some(next) => continue_from = Some(next),
            None => return Ok(all),
        }
    }
}

async fn walk_pulls(
    client: &GithubClient,
    owner: &str,
    name: &str,
    repo_id: i64,
    page1_etag: Option<&str>,
    options: &FetchOptions,
) -> Result<PullListing, DomainError> {
    let mut all = PullListing::default();
    let mut continue_from: Option<String> = None;
    loop {
        let page = client
            .list_pull_requests(
                owner,
                name,
                repo_id,
                page1_etag,
                continue_from.as_deref(),
                options,
            )
            .await?;
        all.complete.absorb(&page.complete);
        all.pull_requests.extend(page.pull_requests);
        all.review_comments.extend(page.review_comments);
        all.contributors.extend(page.contributors);
        all.swept_to_end |= page.swept_to_end;
        all.unchanged |= page.unchanged;
        if all.page1_etag.is_none() {
            all.page1_etag = page.page1_etag;
        }
        match page.next {
            Some(next) => continue_from = Some(next),
            None => return Ok(all),
        }
    }
}

async fn walk_commits(
    client: &GithubClient,
    owner: &str,
    name: &str,
    repo_id: i64,
    updated_after: Option<chrono::DateTime<chrono::Utc>>,
    page1_etag: Option<&str>,
    options: &FetchOptions,
) -> Result<CommitListing, DomainError> {
    let mut all = CommitListing::default();
    let mut continue_from: Option<String> = None;
    loop {
        let page = client
            .list_commits(
                owner,
                name,
                repo_id,
                updated_after,
                page1_etag,
                continue_from.as_deref(),
                options,
            )
            .await?;
        all.complete.absorb(&page.complete);
        all.commits.extend(page.commits);
        all.commit_comments.extend(page.commit_comments);
        all.contributors.extend(page.contributors);
        all.swept_to_end |= page.swept_to_end;
        all.unchanged |= page.unchanged;
        if all.page1_etag.is_none() {
            all.page1_etag = page.page1_etag;
        }
        match page.next {
            Some(next) => continue_from = Some(next),
            None => return Ok(all),
        }
    }
}

async fn fetch_repository(
    client: &GithubClient,
    owner: &str,
    name: &str,
    options: &FetchOptions,
) -> Result<FetchedRepository, DomainError> {
    let repository = client
        .fetch_repository_metadata(owner, name, options)
        .await?;
    let repo_id = repository.id;
    let collection = options.scope.collection;

    let issues = walk_issues(client, owner, name, repo_id, None, None, options).await?;
    let pulls = walk_pulls(client, owner, name, repo_id, None, options).await?;
    let commits = walk_commits(client, owner, name, repo_id, None, None, options).await?;
    let meta = client.list_metadata(owner, name, repo_id, options).await?;
    let actions = client.list_actions(owner, name, repo_id, options).await?;

    let mut complete = ListingCompleteness::none();
    for part in [
        &issues.complete,
        &pulls.complete,
        &commits.complete,
        &meta.complete,
    ] {
        complete.absorb(part);
    }
    let mut people: Vec<ContributorRecord> = Vec::new();
    people.extend(issues.contributors);
    people.extend(pulls.contributors);
    people.extend(commits.contributors);

    let (mut issue_reactions, mut issue_timeline) = (Vec::new(), Vec::new());
    for issue in &issues.issues {
        let open = issue.state == "open";
        let wants = IssueDetailWants {
            reactions: collection.reactions.includes(open),
            timeline: collection.timeline.includes(open),
        };
        if !(wants.reactions || wants.timeline) {
            continue;
        }
        let detail = client
            .refine_issue(owner, name, repo_id, issue.number, wants, options)
            .await?;
        issue_reactions.extend(detail.reactions);
        issue_timeline.extend(detail.timeline);
    }

    let mut pull_requests = Vec::new();
    let (mut reviews, mut pull_request_files, mut pull_request_commits, mut review_threads) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for pull in &pulls.pull_requests {
        let detail = client
            .refine_pull_request(owner, name, repo_id, pull.number, options)
            .await?;
        pull_requests.push(detail.pull_request);
        reviews.extend(detail.reviews);
        pull_request_files.extend(detail.files);
        pull_request_commits.extend(detail.commits);
        review_threads.extend(detail.review_threads);
        people.extend(detail.contributors);
    }

    let with_ci = collection.actions != CollectionMode::None;
    let mut commit_records = Vec::new();
    let (mut commit_files, mut commit_statuses, mut check_runs) =
        (Vec::new(), Vec::new(), Vec::new());
    for commit in &commits.commits {
        let detail = client
            .refine_commit(owner, name, repo_id, &commit.sha, with_ci, options)
            .await?;
        commit_records.push(detail.commit);
        commit_files.extend(detail.files);
        commit_statuses.extend(detail.statuses);
        check_runs.extend(detail.check_runs);
    }

    let mut workflow_jobs = Vec::new();
    if with_ci {
        for run in &actions.workflow_runs {
            workflow_jobs.extend(
                client
                    .refine_workflow_run(owner, name, repo_id, run.id, options)
                    .await?,
            );
        }
    }

    Ok(FetchedRepository {
        repository,
        complete,
        issues: issues.issues,
        pull_requests,
        commits: commit_records,
        comments: issues.comments,
        review_comments: pulls.review_comments,
        reviews,
        labels: meta.labels,
        milestones: meta.milestones,
        releases: meta.releases,
        branches: meta.branches,
        contributors: merge_people(people),
        workflow_runs: actions.workflow_runs,
        pull_request_files,
        tags: meta.tags,
        commit_files,
        review_threads,
        commit_comments: commits.commit_comments,
        issue_events: issues.issue_events,
        deployments: actions.deployments,
        pull_request_commits,
        commit_statuses,
        workflow_jobs,
        issue_reactions,
        check_runs,
        issue_timeline,
    })
}

/// One record per person across families: roles unioned and sorted, the
/// seen-at window widened, ordered by user id — what the writer's merge does.
fn merge_people(records: Vec<ContributorRecord>) -> Vec<ContributorRecord> {
    let mut by_user: std::collections::BTreeMap<i64, ContributorRecord> =
        std::collections::BTreeMap::new();
    for record in records {
        match by_user.entry(record.user_id) {
            std::collections::btree_map::Entry::Vacant(slot) => {
                slot.insert(record);
            }
            std::collections::btree_map::Entry::Occupied(mut slot) => {
                let mine = slot.get_mut();
                for role in record.roles {
                    if !mine.roles.contains(&role) {
                        mine.roles.push(role);
                    }
                }
                mine.first_seen_at = match (mine.first_seen_at, record.first_seen_at) {
                    (Some(a), Some(b)) => Some(a.min(b)),
                    (a, b) => a.or(b),
                };
                mine.last_seen_at = mine.last_seen_at.max(record.last_seen_at);
            }
        }
    }
    by_user
        .into_values()
        .map(|mut record| {
            record.roles.sort();
            record
        })
        .collect()
}

#[tokio::test]
async fn fetch_repository_maps_github_payloads_into_records() {
    let server = MockServer::start_async().await;
    server
        .mock_async(|when, then| {
            when.method("GET").path("/repos/rust-lang/rust");
            then.status(200).json_body(gh_repo_json());
        })
        .await;
    server
        .mock_async(|when, then| {
            when.method("GET").path("/repos/rust-lang/rust/issues");
            then.status(200).json_body(gh_issues_json());
        })
        .await;
    server
        .mock_async(|when, then| {
            when.method("GET").path("/repos/rust-lang/rust/pulls");
            then.status(200).json_body(gh_pulls_json());
        })
        .await;
    server
        .mock_async(|when, then| {
            when.method("GET").path("/repos/rust-lang/rust/pulls/13");
            then.status(200).json_body(gh_pulls_json()[0].clone());
        })
        .await;
    server
        .mock_async(|when, then| {
            when.method("GET").path("/repos/rust-lang/rust/commits");
            then.status(200).json_body(gh_commits_json());
        })
        .await;
    server
        .mock_async(|when, then| {
            when.method("GET")
                .path("/repos/rust-lang/rust/issues/comments");
            then.status(200).json_body(gh_comments_json());
        })
        .await;
    server
        .mock_async(|when, then| {
            when.method("GET")
                .path("/repos/rust-lang/rust/pulls/comments");
            then.status(200).json_body(gh_review_comments_json());
        })
        .await;
    server
        .mock_async(|when, then| {
            when.method("GET")
                .path("/repos/rust-lang/rust/pulls/13/reviews");
            then.status(200).json_body(gh_reviews_json());
        })
        .await;
    server
        .mock_async(|when, then| {
            when.method("GET")
                .path("/repos/rust-lang/rust/pulls/13/files");
            then.status(200).json_body(gh_pull_files_json());
        })
        .await;
    server
        .mock_async(|when, then| {
            when.method("GET")
                .path("/repos/rust-lang/rust/pulls/13/commits");
            then.status(200).json_body(gh_pull_commits_json());
        })
        .await;
    server
        .mock_async(|when, then| {
            when.method("GET").path("/repos/rust-lang/rust/labels");
            then.status(200).json_body(gh_labels_json());
        })
        .await;
    server
        .mock_async(|when, then| {
            when.method("GET").path("/repos/rust-lang/rust/milestones");
            then.status(200).json_body(gh_milestones_json());
        })
        .await;
    server
        .mock_async(|when, then| {
            when.method("GET").path("/repos/rust-lang/rust/releases");
            then.status(200).json_body(gh_releases_json());
        })
        .await;
    server
        .mock_async(|when, then| {
            when.method("GET").path("/repos/rust-lang/rust/branches");
            then.status(200).json_body(gh_branches_json());
        })
        .await;
    server
        .mock_async(|when, then| {
            when.method("GET").path("/repos/rust-lang/rust/tags");
            then.status(200).json_body(gh_tags_json());
        })
        .await;
    server
        .mock_async(|when, then| {
            when.method("GET").path("/repos/rust-lang/rust/comments");
            then.status(200).json_body(gh_commit_comments_json());
        })
        .await;
    server
        .mock_async(|when, then| {
            when.method("GET")
                .path("/repos/rust-lang/rust/issues/events");
            then.status(200).json_body(gh_issue_events_json());
        })
        .await;
    server
        .mock_async(|when, then| {
            when.method("GET").path("/repos/rust-lang/rust/deployments");
            then.status(200).json_body(gh_deployments_json());
        })
        .await;
    server
        .mock_async(|when, then| {
            when.method("POST").path("/graphql");
            then.status(200).json_body(gh_review_threads_json());
        })
        .await;
    server
        .mock_async(|when, then| {
            when.method("GET").path("/repos/rust-lang/rust/commits/c1");
            then.status(200).json_body(gh_commit_detail_json());
        })
        .await;
    server
        .mock_async(|when, then| {
            when.method("GET")
                .path("/repos/rust-lang/rust/commits/c1/statuses");
            then.status(200).json_body(gh_commit_statuses_json());
        })
        .await;
    server
        .mock_async(|when, then| {
            when.method("GET")
                .path("/repos/rust-lang/rust/commits/c1/check-runs");
            then.status(200).json_body(gh_check_runs_json());
        })
        .await;
    server
        .mock_async(|when, then| {
            when.method("GET")
                .path("/repos/rust-lang/rust/actions/runs");
            then.status(200).json_body(gh_workflow_runs_json());
        })
        .await;
    server
        .mock_async(|when, then| {
            when.method("GET")
                .path("/repos/rust-lang/rust/actions/runs/81/jobs");
            then.status(200).json_body(gh_workflow_jobs_json());
        })
        .await;
    server
        .mock_async(|when, then| {
            when.method("GET")
                .path("/repos/rust-lang/rust/issues/11/reactions");
            then.status(200).json_body(gh_issue_reactions_json());
        })
        .await;
    server
        .mock_async(|when, then| {
            when.method("GET")
                .path("/repos/rust-lang/rust/issues/12/reactions");
            then.status(200).json_body(json!([]));
        })
        .await;
    server
        .mock_async(|when, then| {
            when.method("GET")
                .path("/repos/rust-lang/rust/issues/11/timeline");
            then.status(200).json_body(gh_issue_timeline_json());
        })
        .await;
    server
        .mock_async(|when, then| {
            when.method("GET")
                .path("/repos/rust-lang/rust/issues/12/timeline");
            then.status(200).json_body(json!([]));
        })
        .await;

    let client =
        GithubClient::new(server.base_url(), Some("tok".to_owned())).expect("client must build");
    let fetched = fetch_repository(&client, "rust-lang", "rust", &opts(full_scope()))
        .await
        .expect("fetch must succeed");

    assert_eq!(fetched.repository.id, 42);
    assert_eq!(fetched.repository.owner, "rust-lang");
    assert_eq!(fetched.repository.full_name, "rust-lang/rust");
    assert_eq!(
        fetched.repository.clone_url.as_deref(),
        Some("https://github.com/rust-lang/rust.git")
    );
    assert_eq!(fetched.repository.stars, 100_000);

    assert_eq!(fetched.issues.len(), 2);
    assert!(!fetched.issues[0].is_pull_request);
    assert!(fetched.issues[1].is_pull_request);

    assert_eq!(fetched.pull_requests.len(), 1);
    assert!(fetched.pull_requests[0].draft);
    assert!(!fetched.pull_requests[0].merged);
    assert_eq!(fetched.pull_requests[0].head_sha.as_deref(), Some("h1"));
    assert_eq!(
        fetched.pull_requests[0].head_ref.as_deref(),
        Some("feature")
    );
    assert_eq!(fetched.pull_requests[0].base_ref.as_deref(), Some("master"));
    assert_eq!(
        fetched.pull_requests[0].html_url.as_deref(),
        Some("https://github.com/rust-lang/rust/pull/12")
    );

    assert_eq!(fetched.commits.len(), 1);
    assert_eq!(fetched.commits[0].sha, "c1");
    assert_eq!(fetched.commits[0].author_login.as_deref(), Some("alice"));
    assert_eq!(fetched.commits[0].committer_login.as_deref(), Some("bob"));
    assert_eq!(
        fetched.commits[0].committed_at.as_deref(),
        Some("2026-08-19T00:00:00Z")
    );

    assert_eq!(fetched.comments.len(), 1);
    assert_eq!(fetched.comments[0].issue_number, 11);
    assert_eq!(fetched.comments[0].author_login.as_deref(), Some("carol"));

    assert_eq!(fetched.review_comments.len(), 1);
    assert_eq!(fetched.review_comments[0].pull_number, 13);
    assert_eq!(
        fetched.review_comments[0].author_login.as_deref(),
        Some("dave")
    );
    assert_eq!(
        fetched.review_comments[0].path.as_deref(),
        Some("src/lib.rs")
    );

    assert_eq!(fetched.reviews.len(), 1);
    assert_eq!(fetched.reviews[0].pull_number, 13);
    assert_eq!(fetched.reviews[0].state, "APPROVED");
    assert_eq!(fetched.reviews[0].author_login.as_deref(), Some("erin"));

    assert_eq!(fetched.labels.len(), 1);
    assert_eq!(fetched.labels[0].name, "bug");
    assert!(fetched.labels[0].is_default);
    assert_eq!(
        fetched.labels[0].description.as_deref(),
        Some("Something is not working")
    );

    assert_eq!(fetched.milestones.len(), 1);
    assert_eq!(fetched.milestones[0].number, 1);
    assert_eq!(fetched.milestones[0].title, "v1.0");
    assert_eq!(fetched.milestones[0].open_issues, 3);
    assert_eq!(fetched.milestones[0].closed_issues, 7);

    assert_eq!(fetched.releases.len(), 1);
    assert_eq!(fetched.releases[0].tag_name, "v1.0.0");
    assert!(!fetched.releases[0].draft);
    assert_eq!(fetched.releases[0].author_login.as_deref(), Some("erin"));

    assert_eq!(fetched.branches.len(), 1);
    assert_eq!(fetched.branches[0].name, "master");
    assert_eq!(fetched.branches[0].commit_sha, "c1");
    assert!(fetched.branches[0].protected);

    assert_eq!(fetched.tags.len(), 1);
    assert_eq!(fetched.tags[0].name, "v1.0.0");
    assert_eq!(fetched.tags[0].commit_sha, "c1");

    assert_eq!(fetched.commit_files.len(), 1);
    assert_eq!(fetched.commit_files[0].commit_sha, "c1");
    assert_eq!(fetched.commit_files[0].filename, "src/lib.rs");
    assert_eq!(fetched.commits[0].additions, 4);
    assert_eq!(fetched.commits[0].deletions, 1);

    assert_eq!(fetched.commit_statuses.len(), 1);
    assert_eq!(fetched.commit_statuses[0].id, 121);
    assert_eq!(fetched.commit_statuses[0].commit_sha, "c1");
    assert_eq!(fetched.commit_statuses[0].context, "ci/build");
    assert_eq!(
        fetched.commit_statuses[0].creator_login.as_deref(),
        Some("judy")
    );

    assert_eq!(fetched.pull_request_commits.len(), 1);
    assert_eq!(fetched.pull_request_commits[0].pull_number, 13);
    assert_eq!(fetched.pull_request_commits[0].sha, "pc1");
    assert_eq!(
        fetched.pull_request_commits[0].author_login.as_deref(),
        Some("ivan")
    );

    assert_eq!(fetched.deployments.len(), 1);
    assert_eq!(fetched.deployments[0].id, 111);
    assert_eq!(fetched.deployments[0].environment, "production");
    assert_eq!(fetched.deployments[0].git_ref, "master");
    assert_eq!(
        fetched.deployments[0].creator_login.as_deref(),
        Some("heidi")
    );

    assert_eq!(fetched.issue_events.len(), 1);
    assert_eq!(fetched.issue_events[0].id, 101);
    assert_eq!(fetched.issue_events[0].issue_number, 11);
    assert_eq!(fetched.issue_events[0].event, "labeled");
    assert_eq!(fetched.issue_events[0].label_name.as_deref(), Some("bug"));

    assert_eq!(fetched.commit_comments.len(), 1);
    assert_eq!(fetched.commit_comments[0].id, 91);
    assert_eq!(fetched.commit_comments[0].commit_sha, "c1");
    assert_eq!(
        fetched.commit_comments[0].author_login.as_deref(),
        Some("frank")
    );

    assert_eq!(fetched.review_threads.len(), 1);
    assert_eq!(fetched.review_threads[0].id, "PRRT_thread1");
    assert_eq!(fetched.review_threads[0].pull_number, 13);
    assert!(fetched.review_threads[0].is_resolved);
    assert_eq!(
        fetched.review_threads[0].resolved_by.as_deref(),
        Some("erin")
    );
    assert_eq!(fetched.review_threads[0].comments_count, 3);

    // Contributors are derived from the user objects in the entities above,
    // never from `/repos/{owner}/{name}/contributors` — which this server
    // does not serve, so a request for it would fail the fetch outright.
    let people: std::collections::HashMap<i64, _> = fetched
        .contributors
        .iter()
        .map(|c| (c.user_id, c))
        .collect();
    assert_eq!(people.len(), 5, "alice, bob, carol, dave, erin");

    let alice = people.get(&71).expect("alice");
    assert_eq!(alice.login.as_deref(), Some("alice"));
    assert_eq!(
        alice.roles,
        vec!["author".to_owned()],
        "issue author and commit author are both PRD's `author` role"
    );
    assert_eq!(alice.account_type, "User");
    assert_eq!(
        alice.avatar_url.as_deref(),
        Some("https://avatars.githubusercontent.com/u/71"),
        "profile details ride along with the embedded user object"
    );
    assert_eq!(
        alice.first_seen_at,
        Some(instant("2026-08-19T00:00:00Z")),
        "the commit predates the issue"
    );
    assert_eq!(alice.last_seen_at, Some(instant("2026-08-20T00:00:00Z")));

    assert_eq!(people.get(&72).expect("bob").roles, vec!["committer"]);
    assert_eq!(
        people.get(&73).expect("carol").roles,
        vec!["assignee".to_owned(), "commenter".to_owned()]
    );
    assert_eq!(people.get(&74).expect("dave").roles, vec!["commenter"]);
    assert_eq!(
        people.get(&75).expect("erin").roles,
        vec!["author".to_owned(), "reviewer".to_owned()]
    );

    assert_eq!(fetched.pull_request_files.len(), 2);
    assert_eq!(fetched.pull_request_files[0].pull_number, 13);
    assert_eq!(fetched.pull_request_files[0].filename, "src/lib.rs");
    assert_eq!(fetched.pull_request_files[0].additions, 10);
    assert_eq!(
        fetched.pull_request_files[1].previous_filename.as_deref(),
        Some("README.rst")
    );
    assert_eq!(fetched.pull_requests[0].lines_added, 11);
    assert_eq!(fetched.pull_requests[0].lines_removed, 2);

    assert_eq!(fetched.issue_timeline.len(), 2);
    assert_eq!(fetched.issue_timeline[0].position, 0);
    assert_eq!(fetched.issue_timeline[0].event, "labeled");
    assert_eq!(fetched.issue_timeline[0].issue_number, 11);
    assert_eq!(
        fetched.issue_timeline[0].actor_login.as_deref(),
        Some("kate")
    );
    assert!(
        fetched.issue_timeline[0].payload_json.contains("\"bug\""),
        "the whole GitHub entry must be kept, label included"
    );
    assert_eq!(fetched.issue_timeline[1].position, 1);
    assert_eq!(fetched.issue_timeline[1].event, "committed");
    assert!(
        fetched.issue_timeline[1].created_at.is_none(),
        "a committed entry carries no created_at of its own"
    );

    assert_eq!(fetched.check_runs.len(), 1);
    assert_eq!(fetched.check_runs[0].id, 771);
    assert_eq!(fetched.check_runs[0].head_sha, "c1");
    assert_eq!(fetched.check_runs[0].name, "clippy");
    assert_eq!(fetched.check_runs[0].check_suite_id, Some(900));
    assert_eq!(
        fetched.check_runs[0].app_slug.as_deref(),
        Some("github-actions")
    );
    assert_eq!(
        fetched.check_runs[0].output_title.as_deref(),
        Some("no warnings")
    );

    assert_eq!(fetched.issue_reactions.len(), 1);
    assert_eq!(fetched.issue_reactions[0].id, 555);
    assert_eq!(fetched.issue_reactions[0].issue_number, 11);
    assert_eq!(fetched.issue_reactions[0].content, "heart");
    assert_eq!(
        fetched.issue_reactions[0].user_login.as_deref(),
        Some("kate")
    );

    assert_eq!(fetched.workflow_jobs.len(), 1);
    assert_eq!(fetched.workflow_jobs[0].id, 910);
    assert_eq!(fetched.workflow_jobs[0].run_id, 81);
    assert_eq!(fetched.workflow_jobs[0].name, "build");
    assert_eq!(
        fetched.workflow_jobs[0].runner_name.as_deref(),
        Some("ubuntu-latest")
    );
    assert!(
        fetched.workflow_jobs[0]
            .steps_json
            .as_deref()
            .expect("steps must be stored")
            .contains("Checkout"),
        "the raw GitHub steps array must be kept verbatim"
    );

    assert_eq!(fetched.workflow_runs.len(), 1);
    assert_eq!(fetched.workflow_runs[0].id, 81);
    assert_eq!(fetched.workflow_runs[0].run_number, 300);
    assert_eq!(fetched.workflow_runs[0].run_attempt, 2);
    assert_eq!(
        fetched.workflow_runs[0].conclusion.as_deref(),
        Some("success")
    );
    assert_eq!(
        fetched.workflow_runs[0].actor_login.as_deref(),
        Some("alice")
    );
}

#[tokio::test]
async fn github_404_maps_to_not_found() {
    let server = MockServer::start_async().await;
    server
        .mock_async(|when, then| {
            when.method("GET").path("/repos/acme/nope");
            then.status(404).json_body(json!({"message": "Not Found"}));
        })
        .await;

    let client = GithubClient::new(server.base_url(), None).expect("client must build");
    let result = fetch_repository(&client, "acme", "nope", &opts(ScopeConfig::default())).await;

    assert!(matches!(result, Err(DomainError::NotFound)));
}

#[tokio::test]
async fn github_server_error_maps_to_internal() {
    let server = MockServer::start_async().await;
    server
        .mock_async(|when, then| {
            when.method("GET").path("/repos/acme/flaky");
            then.status(503);
        })
        .await;

    let client = GithubClient::new(server.base_url(), None).expect("client must build");
    let result = fetch_repository(&client, "acme", "flaky", &opts(ScopeConfig::default())).await;

    assert!(matches!(result, Err(DomainError::Internal(_))));
}

#[tokio::test]
async fn malformed_json_maps_to_internal() {
    let server = MockServer::start_async().await;
    server
        .mock_async(|when, then| {
            when.method("GET").path("/repos/acme/garbage");
            then.status(200).body("not json");
        })
        .await;

    let client = GithubClient::new(server.base_url(), None).expect("client must build");
    let result = fetch_repository(&client, "acme", "garbage", &opts(ScopeConfig::default())).await;

    assert!(matches!(result, Err(DomainError::Internal(_))));
}

/// The point of the scope is the request budget: a disabled object type must
/// cost no GitHub call at all, not merely produce an empty result.
#[tokio::test]
async fn a_narrow_scope_skips_the_calls_it_does_not_need() {
    let server = MockServer::start_async().await;
    server
        .mock_async(|when, then| {
            when.method("GET").path("/repos/rust-lang/rust");
            then.status(200).json_body(gh_repo_json());
        })
        .await;
    let labels = server
        .mock_async(|when, then| {
            when.method("GET").path("/repos/rust-lang/rust/labels");
            then.status(200).json_body(gh_labels_json());
        })
        .await;
    let issues = server
        .mock_async(|when, then| {
            when.method("GET").path("/repos/rust-lang/rust/issues");
            then.status(200).json_body(gh_issues_json());
        })
        .await;
    let commits = server
        .mock_async(|when, then| {
            when.method("GET").path("/repos/rust-lang/rust/commits");
            then.status(200).json_body(gh_commits_json());
        })
        .await;

    let mut scope = ScopeConfig::default();
    scope.objects = github_mirror::domain::scope::SyncScope::none();
    scope.objects.labels = true;

    let client = GithubClient::new(server.base_url(), None).expect("client must build");
    let fetched = fetch_repository(&client, "rust-lang", "rust", &opts(scope))
        .await
        .expect("fetch must succeed");

    labels.assert_calls_async(1).await;
    issues.assert_calls_async(0).await;
    commits.assert_calls_async(0).await;

    assert!(!fetched.labels.is_empty(), "labels were in scope");
    assert!(fetched.issues.is_empty());
    assert!(fetched.commits.is_empty());
    assert!(fetched.pull_requests.is_empty());
    assert!(fetched.workflow_runs.is_empty());
    assert!(fetched.contributors.is_empty());
}

/// A trivial in-memory cache, standing in for the `SeaORM` one.
#[derive(Default)]
struct MemCache {
    entries: std::sync::Mutex<std::collections::HashMap<String, CachedResponse>>,
}

#[async_trait::async_trait]
impl HttpCache for MemCache {
    async fn get(
        &self,
        _tenant_id: uuid::Uuid,
        key: &CacheKey,
    ) -> Result<Option<CachedResponse>, DomainError> {
        Ok(self.entries.lock().unwrap().get(key.as_str()).cloned())
    }

    async fn put(
        &self,
        _tenant_id: uuid::Uuid,
        key: &CacheKey,
        _url: &str,
        entry: CachedResponse,
    ) -> Result<(), DomainError> {
        self.entries
            .lock()
            .unwrap()
            .insert(key.as_str().to_owned(), entry);
        Ok(())
    }

    async fn clear(&self, _tenant_id: uuid::Uuid, _url_prefix: &str) -> Result<u64, DomainError> {
        let mut entries = self.entries.lock().unwrap();
        let removed = entries.len() as u64;
        entries.clear();
        Ok(removed)
    }
}

/// Only the repository endpoint is in scope, so one sync is exactly one call.
fn repo_only_scope() -> ScopeConfig {
    ScopeConfig {
        objects: github_mirror::domain::scope::SyncScope::none(),
        ..ScopeConfig::default()
    }
}

#[tokio::test]
async fn a_stored_etag_turns_the_next_sync_into_a_free_304() {
    let server = MockServer::start_async().await;
    let first = server
        .mock_async(|when, then| {
            when.method("GET")
                .path("/repos/rust-lang/rust")
                .is_true(|req| {
                    !req.headers()
                        .iter()
                        .any(|(k, _)| k.as_str() == "if-none-match")
                });
            then.status(200)
                .header("etag", "W/\"deadbeef\"")
                .json_body(gh_repo_json());
        })
        .await;
    let revalidated = server
        .mock_async(|when, then| {
            when.method("GET")
                .path("/repos/rust-lang/rust")
                .header("if-none-match", "W/\"deadbeef\"");
            then.status(304);
        })
        .await;

    let cache = std::sync::Arc::new(MemCache::default());
    let client = GithubClient::with_cache(server.base_url(), None, cache.clone())
        .expect("client must build");

    let scope = repo_only_scope();
    let tenant = uuid::Uuid::new_v4();
    let options = FetchOptions {
        tenant_id: tenant,
        scope,
        force: false,
        since: None,
    };

    let fresh = fetch_repository(&client, "rust-lang", "rust", &options)
        .await
        .expect("first fetch");
    first.assert_calls_async(1).await;
    revalidated.assert_calls_async(0).await;

    let cached = fetch_repository(&client, "rust-lang", "rust", &options)
        .await
        .expect("second fetch");
    revalidated.assert_calls_async(1).await;
    first.assert_calls_async(1).await;

    assert_eq!(
        fresh.repository, cached.repository,
        "the 304 must reproduce the body byte for byte"
    );

    let forced = FetchOptions {
        force: true,
        ..options
    };
    fetch_repository(&client, "rust-lang", "rust", &forced)
        .await
        .expect("forced fetch");
    first.assert_calls_async(2).await;
}

#[tokio::test]
async fn a_listing_follows_the_link_header_past_the_first_page() {
    let server = MockServer::start_async().await;
    server
        .mock_async(|when, then| {
            when.method("GET").path("/repos/rust-lang/rust");
            then.status(200).json_body(gh_repo_json());
        })
        .await;

    let page_two = format!("{}/repos/rust-lang/rust/labels?page=2", server.base_url());
    let first = server
        .mock_async(|when, then| {
            when.method("GET")
                .path("/repos/rust-lang/rust/labels")
                .query_param_exists("per_page");
            then.status(200)
                .header("link", format!("<{page_two}>; rel=\"next\""))
                .json_body(gh_labels_json());
        })
        .await;
    let second = server
        .mock_async(|when, then| {
            when.method("GET")
                .path("/repos/rust-lang/rust/labels")
                .query_param("page", "2");
            then.status(200).json_body(json!([{
                "id": 9_001, "name": "from-page-two", "color": "ffffff", "description": null
            }]));
        })
        .await;

    let client = GithubClient::new(server.base_url(), None).expect("client must build");
    let mut scope = repo_only_scope();
    scope.objects.labels = true;

    let fetched = fetch_repository(&client, "rust-lang", "rust", &opts(scope))
        .await
        .expect("fetch must succeed");

    first.assert_calls_async(1).await;
    second.assert_calls_async(1).await;
    assert!(
        fetched.labels.iter().any(|l| l.name == "from-page-two"),
        "the second page must be merged into the result, got {:?}",
        fetched.labels.iter().map(|l| &l.name).collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn unauthorized_maps_to_access_lost() {
    let server = MockServer::start_async().await;
    server
        .mock_async(|when, then| {
            when.method("GET").path("/repos/acme/private");
            then.status(401);
        })
        .await;

    let client = GithubClient::new(server.base_url(), None).expect("client must build");
    let result = fetch_repository(&client, "acme", "private", &opts(ScopeConfig::default())).await;

    assert!(
        matches!(result, Err(DomainError::AccessLost(_))),
        "a 401 means the mirror's own credentials stopped working, got {result:?}"
    );
}

#[tokio::test]
async fn plain_forbidden_maps_to_access_lost() {
    let server = MockServer::start_async().await;
    server
        .mock_async(|when, then| {
            when.method("GET").path("/repos/acme/gone");
            then.status(403);
        })
        .await;

    let client = GithubClient::new(server.base_url(), None).expect("client must build");
    let result = fetch_repository(&client, "acme", "gone", &opts(ScopeConfig::default())).await;

    assert!(
        matches!(result, Err(DomainError::AccessLost(_))),
        "a 403 without rate-limit headers is lost access, not a rate limit, got {result:?}"
    );
}

#[tokio::test]
async fn a_rate_limited_response_is_retried_before_giving_up() {
    let server = MockServer::start_async().await;
    let limited = server
        .mock_async(|when, then| {
            when.method("GET").path("/repos/acme/busy");
            then.status(403)
                .header("retry-after", "0")
                .header("x-ratelimit-remaining", "0");
        })
        .await;

    let client = GithubClient::new(server.base_url(), None).expect("client must build");
    let result = fetch_repository(&client, "acme", "busy", &opts(ScopeConfig::default())).await;

    assert!(
        matches!(result, Err(DomainError::Internal(_))),
        "a rate limit that never clears fails after the retries, got {result:?}"
    );
    assert!(
        limited.calls_async().await > 1,
        "the client must retry a rate-limited response rather than give up on the first"
    );
}

#[tokio::test]
async fn an_unchanged_first_page_stops_the_issue_sweep_before_page_two() {
    let server = MockServer::start_async().await;
    let page_two = format!("{}/repos/rust-lang/rust/issues?page=2", server.base_url());
    let first_page = server
        .mock_async(|when, then| {
            when.method("GET")
                .path("/repos/rust-lang/rust/issues")
                .query_param("sort", "updated")
                .query_param("direction", "desc")
                .query_param_exists("per_page");
            then.status(200)
                .header("etag", "W/\"issues-page-one\"")
                .header("link", format!("<{page_two}>; rel=\"next\""))
                .json_body(gh_issues_json());
        })
        .await;
    let second_page = server
        .mock_async(|when, then| {
            when.method("GET")
                .path("/repos/rust-lang/rust/issues")
                .query_param("page", "2");
            then.status(200).json_body(json!([{
                "id": 4, "number": 14, "title": "from page two", "state": "open",
                "created_at": "2026-08-19T00:00:00Z",
                "updated_at": "2026-08-19T00:00:00Z"
            }]));
        })
        .await;
    let comments = server
        .mock_async(|when, then| {
            when.method("GET")
                .path("/repos/rust-lang/rust/issues/comments");
            then.status(200).json_body(json!([]));
        })
        .await;
    let events = server
        .mock_async(|when, then| {
            when.method("GET")
                .path("/repos/rust-lang/rust/issues/events");
            then.status(200).json_body(json!([]));
        })
        .await;

    let client = GithubClient::new(server.base_url(), None).expect("client must build");
    let options = opts(ScopeConfig::default());

    let walked = walk_issues(&client, "rust-lang", "rust", 42, None, None, &options)
        .await
        .expect("the first sweep must walk");

    assert!(!walked.unchanged);
    assert_eq!(
        walked.page1_etag.as_deref(),
        Some("W/\"issues-page-one\""),
        "the sweep must carry page one's validator back for next time"
    );
    assert!(
        walked.issues.iter().any(|i| i.number == 14),
        "the walk must reach page two"
    );
    second_page.assert_calls_async(1).await;
    comments.assert_calls_async(1).await;

    let skipped = client
        .list_issues(
            "rust-lang",
            "rust",
            42,
            None,
            Some("W/\"issues-page-one\""),
            None,
            &options,
        )
        .await
        .expect("the second sweep must succeed");

    assert!(skipped.unchanged, "page one's validator did not change");
    assert!(skipped.issues.is_empty());
    assert_eq!(
        first_page.calls_async().await,
        2,
        "page one is still asked for; it is what the validator is read from"
    );
    second_page.assert_calls_async(1).await;
    comments.assert_calls_async(1).await;
    events.assert_calls_async(1).await;

    assert!(
        !skipped.complete.is_complete(Listing::Issues),
        "an unwalked listing must never count as complete, or reconciliation \
         would delete every issue it did not re-stamp"
    );
}

#[tokio::test]
async fn a_walk_bounded_by_the_watermark_is_swept_to_its_end_but_never_complete() {
    let server = MockServer::start_async().await;
    for (path, body) in [
        ("/repos/rust-lang/rust/issues", gh_issues_json()),
        ("/repos/rust-lang/rust/issues/comments", json!([])),
        ("/repos/rust-lang/rust/issues/events", json!([])),
        ("/repos/rust-lang/rust/commits", gh_commits_json()),
        ("/repos/rust-lang/rust/comments", json!([])),
    ] {
        server
            .mock_async(move |when, then| {
                when.method("GET").path(path);
                then.status(200).json_body(body);
            })
            .await;
    }

    let client = GithubClient::new(server.base_url(), None).expect("client must build");
    let options = opts(ScopeConfig::default());
    let watermark = Some(instant("2026-08-19T23:55:00Z"));

    let unbounded = walk_issues(&client, "rust-lang", "rust", 42, None, None, &options)
        .await
        .expect("the unbounded walk must succeed");
    assert!(
        unbounded.complete.is_complete(Listing::Issues),
        "with no bound, a walk that ran out of pages saw every issue there is"
    );

    let issues = walk_issues(&client, "rust-lang", "rust", 42, watermark, None, &options)
        .await
        .expect("the bounded walk must succeed");
    assert!(issues.swept_to_end, "the bounded walk ran out of pages too");
    assert!(
        !issues.complete.is_complete(Listing::Issues),
        "GitHub only returned issues updated since the bound, so absence from \
         this walk proves nothing and reconciliation must not delete on it"
    );
    assert!(!issues.complete.is_complete(Listing::Comments));

    let commits = walk_commits(&client, "rust-lang", "rust", 42, watermark, None, &options)
        .await
        .expect("the bounded commits walk must succeed");
    assert!(commits.swept_to_end);
    assert!(
        !commits.complete.is_complete(Listing::Commits),
        "the commits walk carries the same bound and the same rule"
    );
}

#[tokio::test]
async fn an_unchanged_first_page_stops_the_pull_and_commit_sweeps_too() {
    let server = MockServer::start_async().await;
    let pulls_page_two = format!("{}/repos/rust-lang/rust/pulls?page=2", server.base_url());
    let commits_page_two = format!("{}/repos/rust-lang/rust/commits?page=2", server.base_url());
    let pulls_first = server
        .mock_async(|when, then| {
            when.method("GET")
                .path("/repos/rust-lang/rust/pulls")
                .query_param("sort", "updated")
                .query_param_exists("per_page");
            then.status(200)
                .header("etag", "W/\"pulls-page-one\"")
                .header("link", format!("<{pulls_page_two}>; rel=\"next\""))
                .json_body(gh_pulls_json());
        })
        .await;
    let pulls_second = server
        .mock_async(|when, then| {
            when.method("GET")
                .path("/repos/rust-lang/rust/pulls")
                .query_param("page", "2");
            then.status(200).json_body(json!([]));
        })
        .await;
    let commits_first = server
        .mock_async(|when, then| {
            when.method("GET")
                .path("/repos/rust-lang/rust/commits")
                .query_param_exists("per_page");
            then.status(200)
                .header("etag", "W/\"commits-page-one\"")
                .header("link", format!("<{commits_page_two}>; rel=\"next\""))
                .json_body(gh_commits_json());
        })
        .await;
    let commits_second = server
        .mock_async(|when, then| {
            when.method("GET")
                .path("/repos/rust-lang/rust/commits")
                .query_param("page", "2");
            then.status(200).json_body(json!([]));
        })
        .await;
    for path in [
        "/repos/rust-lang/rust/pulls/comments",
        "/repos/rust-lang/rust/comments",
    ] {
        server
            .mock_async(move |when, then| {
                when.method("GET").path(path);
                then.status(200).json_body(json!([]));
            })
            .await;
    }

    let client = GithubClient::new(server.base_url(), None).expect("client must build");
    let options = opts(ScopeConfig::default());

    let pulls = walk_pulls(&client, "rust-lang", "rust", 42, None, &options)
        .await
        .expect("the first pull sweep must walk");
    assert_eq!(pulls.page1_etag.as_deref(), Some("W/\"pulls-page-one\""));
    pulls_second.assert_calls_async(1).await;
    let commits = walk_commits(&client, "rust-lang", "rust", 42, None, None, &options)
        .await
        .expect("the first commit sweep must walk");
    assert_eq!(
        commits.page1_etag.as_deref(),
        Some("W/\"commits-page-one\"")
    );
    commits_second.assert_calls_async(1).await;

    let pulls_again = client
        .list_pull_requests(
            "rust-lang",
            "rust",
            42,
            Some("W/\"pulls-page-one\""),
            None,
            &options,
        )
        .await
        .expect("the second pull sweep must succeed");
    assert!(
        pulls_again.unchanged,
        "page one of the pulls did not change"
    );
    assert!(
        !pulls_again.complete.is_complete(Listing::PullRequests),
        "an unwalked listing must never count as complete"
    );
    let commits_again = client
        .list_commits(
            "rust-lang",
            "rust",
            42,
            None,
            Some("W/\"commits-page-one\""),
            None,
            &options,
        )
        .await
        .expect("the second commit sweep must succeed");
    assert!(
        commits_again.unchanged,
        "page one of the commits did not change"
    );
    assert!(!commits_again.complete.is_complete(Listing::Commits));

    assert_eq!(pulls_first.calls_async().await, 2);
    assert_eq!(commits_first.calls_async().await, 2);
    pulls_second.assert_calls_async(1).await;
    commits_second.assert_calls_async(1).await;
}

#[tokio::test]
async fn a_rate_limit_seen_by_one_request_pauses_every_other_request() {
    let server = MockServer::start_async().await;
    let limited = server
        .mock_async(|when, then| {
            when.method("GET").path("/repos/acme/limited");
            then.status(403)
                .header("retry-after", "2")
                .header("x-ratelimit-remaining", "0");
        })
        .await;
    server
        .mock_async(|when, then| {
            when.method("GET").path("/repos/acme/free");
            then.status(200).json_body(gh_repo_json());
        })
        .await;

    let client =
        std::sync::Arc::new(GithubClient::new(server.base_url(), None).expect("client must build"));
    let options = opts(ScopeConfig::default());

    let first = {
        let client = std::sync::Arc::clone(&client);
        tokio::spawn(async move {
            client
                .fetch_repository_metadata("acme", "limited", &options)
                .await
        })
    };
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    limited.delete_async().await;
    server
        .mock_async(|when, then| {
            when.method("GET").path("/repos/acme/limited");
            then.status(200).json_body(gh_repo_json());
        })
        .await;

    let started = std::time::Instant::now();
    client
        .fetch_repository_metadata("acme", "free", &options)
        .await
        .expect("the free request must succeed once the cooldown has passed");
    let waited = started.elapsed();
    assert!(
        waited >= std::time::Duration::from_millis(1500),
        "a request that had nothing to do with the limit must still wait it out, waited {waited:?}"
    );

    first
        .await
        .expect("the limited request task must finish")
        .expect("the limited request must succeed on its retry");
}
