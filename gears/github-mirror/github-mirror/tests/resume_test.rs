#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use github_mirror::domain::error::DomainError;
use github_mirror::domain::ports::github::{
    ActionsListing, CommitDetail, CommitListing, FetchOptions, GithubPort, IssueDetail,
    IssueDetailWants, IssueListing, MetadataListing, PullDetail, PullListing,
};
use github_mirror::domain::repo::{PageWindow, RepoRecord, WorkflowJobRecord};
use github_mirror_sdk::SyncSummary;
use tokio_util::sync::CancellationToken;
use toolkit_odata::ODataQuery;
use toolkit_security::SecurityContext;
use uuid::Uuid;

/// The fixture fake, but the first listing call trips `cancel` so the run is
/// stopped after Discovery has already written the repository row.
struct StopsAfterDiscovery {
    inner: common::FakeGithub,
    cancel: CancellationToken,
}

#[async_trait]
impl GithubPort for StopsAfterDiscovery {
    async fn fetch_repository_metadata(
        &self,
        owner: &str,
        name: &str,
        options: &FetchOptions,
    ) -> Result<RepoRecord, DomainError> {
        self.inner
            .fetch_repository_metadata(owner, name, options)
            .await
    }

    #[allow(clippy::too_many_arguments)]
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
        self.cancel.cancel();
        self.inner
            .list_issues(
                owner,
                name,
                repo_id,
                updated_after,
                page1_etag,
                continue_from,
                options,
            )
            .await
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
        self.inner
            .refine_issue(owner, name, repo_id, number, wants, options)
            .await
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
        self.inner
            .list_pull_requests(owner, name, repo_id, page1_etag, continue_from, options)
            .await
    }

    async fn refine_pull_request(
        &self,
        owner: &str,
        name: &str,
        repo_id: i64,
        number: i64,
        options: &FetchOptions,
    ) -> Result<PullDetail, DomainError> {
        self.inner
            .refine_pull_request(owner, name, repo_id, number, options)
            .await
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
        self.inner
            .list_commits(
                owner,
                name,
                repo_id,
                updated_after,
                page1_etag,
                continue_from,
                options,
            )
            .await
    }

    async fn refine_commit(
        &self,
        owner: &str,
        name: &str,
        repo_id: i64,
        sha: &str,
        with_ci: bool,
        options: &FetchOptions,
    ) -> Result<CommitDetail, DomainError> {
        self.inner
            .refine_commit(owner, name, repo_id, sha, with_ci, options)
            .await
    }

    async fn list_metadata(
        &self,
        owner: &str,
        name: &str,
        repo_id: i64,
        options: &FetchOptions,
    ) -> Result<MetadataListing, DomainError> {
        self.inner
            .list_metadata(owner, name, repo_id, options)
            .await
    }

    async fn list_actions(
        &self,
        owner: &str,
        name: &str,
        repo_id: i64,
        options: &FetchOptions,
    ) -> Result<ActionsListing, DomainError> {
        self.inner.list_actions(owner, name, repo_id, options).await
    }

    async fn refine_workflow_run(
        &self,
        owner: &str,
        name: &str,
        repo_id: i64,
        run_id: i64,
        options: &FetchOptions,
    ) -> Result<Vec<WorkflowJobRecord>, DomainError> {
        self.inner
            .refine_workflow_run(owner, name, repo_id, run_id, options)
            .await
    }

    async fn clear_cache(
        &self,
        tenant_id: Uuid,
        owner: &str,
        name: Option<&str>,
    ) -> Result<u64, DomainError> {
        self.inner.clear_cache(tenant_id, owner, name).await
    }
}

const OWNER: &str = "rust-lang";
const NAME: &str = "rust";

async fn mirrored(
    service: &common::ConcreteService,
    ctx: &SecurityContext,
) -> Vec<(&'static str, usize)> {
    let window = PageWindow::first(50);
    let filter = github_mirror::domain::repo::ListingFilter::default();
    vec![
        (
            "issues",
            service
                .list_issues(ctx, OWNER, NAME, window, filter)
                .await
                .expect("issues must list")
                .0
                .items
                .len(),
        ),
        (
            "pull_requests",
            service
                .list_pull_requests(ctx, OWNER, NAME, window, filter)
                .await
                .expect("pull requests must list")
                .0
                .items
                .len(),
        ),
        (
            "commits",
            service
                .list_commits(ctx, OWNER, NAME, window, None)
                .await
                .expect("commits must list")
                .0
                .items
                .len(),
        ),
        (
            "labels",
            service
                .list_labels(ctx, OWNER, NAME, window)
                .await
                .expect("labels must list")
                .items
                .len(),
        ),
        (
            "branches",
            service
                .list_branches(ctx, OWNER, NAME, window)
                .await
                .expect("branches must list")
                .items
                .len(),
        ),
    ]
}

async fn service_for() -> (Arc<common::ConcreteService>, common::SyncPump) {
    let service = common::service_with_github(
        common::inmem_db().await,
        "https://api.github.com",
        Arc::new(common::FakeGithub {
            result: Some(common::fetched_repository()),
        }),
    );
    let pump = common::SyncPump::take(&service).await;
    (service, pump)
}

#[tokio::test]
async fn an_interrupted_sync_resumes_to_the_state_an_uninterrupted_one_reaches() {
    let ctx = common::caller_in(Uuid::new_v4());

    let (clean, mut clean_pump) = service_for().await;
    clean
        .enqueue_sync(&ctx, OWNER, NAME, None, false, None)
        .await
        .expect("the first sync must queue");
    assert_eq!(clean_pump.drain(&clean).await, 1);
    let expected = mirrored(&clean, &ctx).await;

    let stopped = CancellationToken::new();
    let service = common::service_with_github(
        common::inmem_db().await,
        "https://api.github.com",
        Arc::new(StopsAfterDiscovery {
            inner: common::FakeGithub {
                result: Some(common::fetched_repository()),
            },
            cancel: stopped.clone(),
        }),
    );
    let mut pump = common::SyncPump::take(&service).await;
    service
        .enqueue_sync(&ctx, OWNER, NAME, None, false, None)
        .await
        .expect("the interrupted sync must queue");
    assert_eq!(pump.drain_under(&service, &stopped).await, 1);

    let sessions = service
        .list_sessions(&ctx, &ODataQuery::default())
        .await
        .expect("sessions must list");
    assert_eq!(
        sessions.items[0].status, "interrupted",
        "an interrupted run must not report success"
    );

    let statuses = service
        .list_repo_sync_status(&ctx, &ODataQuery::default(), None)
        .await
        .expect("run statuses must list");
    assert_eq!(
        statuses.items[0].status, "in_progress",
        "the repository stays in progress, which is what resume looks for"
    );

    let resumed = service
        .resume_incomplete_syncs(&ctx, None, false)
        .await
        .expect("resume must queue the repository again");
    assert_eq!(resumed.len(), 1);
    assert_eq!(pump.drain(&service).await, 1);

    assert_eq!(
        mirrored(&service, &ctx).await,
        expected,
        "a resumed sync must reach the same state as one that ran straight through"
    );

    let statuses = service
        .list_repo_sync_status(&ctx, &ODataQuery::default(), None)
        .await
        .expect("run statuses must list");
    assert_eq!(statuses.items[0].status, "complete");
}

#[tokio::test]
async fn a_repository_that_finished_has_nothing_to_resume() {
    let ctx = common::caller_in(Uuid::new_v4());
    let (service, mut pump) = service_for().await;

    service
        .enqueue_sync(&ctx, OWNER, NAME, None, false, None)
        .await
        .expect("the sync must queue");
    assert_eq!(pump.drain(&service).await, 1);

    let resumed = service
        .resume_incomplete_syncs(&ctx, None, false)
        .await
        .expect("resume must succeed");

    assert!(
        resumed.is_empty(),
        "a completed repository is not resumed; asking for that is what POST /sync is for"
    );
}

#[tokio::test]
async fn one_tenant_cannot_resume_another_tenants_repository() {
    let owner = common::caller_in(Uuid::new_v4());
    let (service, mut pump) = service_for().await;

    service
        .enqueue_sync(&owner, OWNER, NAME, None, false, None)
        .await
        .expect("the sync must queue");
    let stopped = CancellationToken::new();
    stopped.cancel();
    assert_eq!(pump.drain_under(&service, &stopped).await, 1);

    let stranger = common::caller_in(Uuid::new_v4());
    let resumed = service
        .resume_incomplete_syncs(&stranger, None, false)
        .await
        .expect("resume must succeed");

    assert!(
        resumed.is_empty(),
        "the interrupted repository belongs to another tenant"
    );
}

/// The fixture fake with a listing that carries a page-one `ETag`, and one
/// refinement that fails the first time it is asked.
struct ListingWithEtag {
    inner: common::FakeGithub,
    etag: &'static str,
    fail_first_refine: AtomicBool,
}

#[async_trait]
impl GithubPort for ListingWithEtag {
    async fn fetch_repository_metadata(
        &self,
        owner: &str,
        name: &str,
        options: &FetchOptions,
    ) -> Result<RepoRecord, DomainError> {
        self.inner
            .fetch_repository_metadata(owner, name, options)
            .await
    }

    #[allow(clippy::too_many_arguments)]
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
        if page1_etag == Some(self.etag) {
            return Ok(IssueListing {
                page1_etag: Some(self.etag.to_owned()),
                unchanged: true,
                ..IssueListing::default()
            });
        }
        let mut listing = self
            .inner
            .list_issues(
                owner,
                name,
                repo_id,
                updated_after,
                page1_etag,
                continue_from,
                options,
            )
            .await?;
        listing.page1_etag = Some(self.etag.to_owned());
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
        if self.fail_first_refine.swap(false, Ordering::SeqCst) {
            return Err(DomainError::internal("GitHub answered 502 once"));
        }
        self.inner
            .refine_issue(owner, name, repo_id, number, wants, options)
            .await
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
        self.inner
            .list_pull_requests(owner, name, repo_id, page1_etag, continue_from, options)
            .await
    }

    async fn refine_pull_request(
        &self,
        owner: &str,
        name: &str,
        repo_id: i64,
        number: i64,
        options: &FetchOptions,
    ) -> Result<PullDetail, DomainError> {
        self.inner
            .refine_pull_request(owner, name, repo_id, number, options)
            .await
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
        self.inner
            .list_commits(
                owner,
                name,
                repo_id,
                updated_after,
                page1_etag,
                continue_from,
                options,
            )
            .await
    }

    async fn refine_commit(
        &self,
        owner: &str,
        name: &str,
        repo_id: i64,
        sha: &str,
        with_ci: bool,
        options: &FetchOptions,
    ) -> Result<CommitDetail, DomainError> {
        self.inner
            .refine_commit(owner, name, repo_id, sha, with_ci, options)
            .await
    }

    async fn list_metadata(
        &self,
        owner: &str,
        name: &str,
        repo_id: i64,
        options: &FetchOptions,
    ) -> Result<MetadataListing, DomainError> {
        self.inner
            .list_metadata(owner, name, repo_id, options)
            .await
    }

    async fn list_actions(
        &self,
        owner: &str,
        name: &str,
        repo_id: i64,
        options: &FetchOptions,
    ) -> Result<ActionsListing, DomainError> {
        self.inner.list_actions(owner, name, repo_id, options).await
    }

    async fn refine_workflow_run(
        &self,
        owner: &str,
        name: &str,
        repo_id: i64,
        run_id: i64,
        options: &FetchOptions,
    ) -> Result<Vec<WorkflowJobRecord>, DomainError> {
        self.inner
            .refine_workflow_run(owner, name, repo_id, run_id, options)
            .await
    }

    async fn clear_cache(
        &self,
        tenant_id: Uuid,
        owner: &str,
        name: Option<&str>,
    ) -> Result<u64, DomainError> {
        self.inner.clear_cache(tenant_id, owner, name).await
    }
}

#[tokio::test]
async fn a_refinement_left_pending_is_finished_by_the_next_sync_even_when_the_listing_did_not_change()
 {
    let ctx = common::caller_in(Uuid::new_v4());
    let service = common::service_with_github(
        common::inmem_db().await,
        "https://api.github.com",
        Arc::new(ListingWithEtag {
            inner: common::FakeGithub {
                result: Some(common::fetched_repository()),
            },
            etag: "W/\"issues-page-one\"",
            fail_first_refine: AtomicBool::new(true),
        }),
    );
    let mut pump = common::SyncPump::take(&service).await;

    let first = service
        .enqueue_sync(&ctx, OWNER, NAME, None, false, None)
        .await
        .expect("the first sync must queue");
    assert_eq!(pump.drain(&service).await, 1);
    let failed = service
        .get_session(&ctx, first)
        .await
        .expect("the first session must exist");
    assert_eq!(
        failed.status, "failed",
        "one refinement failed, so the run did"
    );

    let second = service
        .enqueue_sync(&ctx, OWNER, NAME, None, false, None)
        .await
        .expect("the second sync must queue");
    assert_eq!(pump.drain(&service).await, 1);
    let finished = service
        .get_session(&ctx, second)
        .await
        .expect("the second session must exist");
    assert_eq!(finished.status, "complete");
    let summary: SyncSummary = serde_json::from_str(
        finished
            .summary_json
            .as_deref()
            .expect("a complete run carries a summary"),
    )
    .expect("the summary must parse");
    assert_eq!(
        summary.issue_reactions_synced, 1,
        "the issue the first run left pending must be refined now, even though \
         page one of the listing did not change"
    );
}
