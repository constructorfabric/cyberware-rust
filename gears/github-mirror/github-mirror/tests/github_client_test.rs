#![allow(clippy::unwrap_used, clippy::expect_used, clippy::too_many_lines)]

use github_mirror::domain::error::DomainError;
use github_mirror::domain::ports::github::GithubPort;
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
    let fetched = client
        .fetch_repository("rust-lang", "rust")
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
    let result = client.fetch_repository("acme", "nope").await;

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
    let result = client.fetch_repository("acme", "flaky").await;

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
    let result = client.fetch_repository("acme", "garbage").await;

    assert!(matches!(result, Err(DomainError::Internal(_))));
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
    let result = client.fetch_repository("acme", "private").await;

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
    let result = client.fetch_repository("acme", "gone").await;

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
    let result = client.fetch_repository("acme", "busy").await;

    assert!(
        matches!(result, Err(DomainError::Internal(_))),
        "a rate limit that never clears fails after the retries, got {result:?}"
    );
    assert!(
        limited.calls_async().await > 1,
        "the client must retry a rate-limited response rather than give up on the first"
    );
}
