#![allow(dead_code, clippy::too_many_lines)]

use std::sync::Arc;

use async_trait::async_trait;
use authz_resolver_sdk::{
    AuthZResolverApi, PolicyEnforcer,
    constraints::{Constraint, InPredicate, Predicate},
    models::{EvaluationRequest, EvaluationResponse, EvaluationResponseContext},
};
use github_mirror::domain::error::DomainError;
use github_mirror::domain::ports::github::{
    ActionsListing, CommitDetail, CommitListing, DeclaredCounts, FetchOptions, FetchedRepository,
    GithubPort, IssueDetail, IssueDetailWants, IssueListing, Listing, ListingCompleteness,
    MetadataListing, PullDetail, PullListing,
};
use github_mirror::domain::repo::{
    BranchRecord, CheckRunRecord, CommentRecord, CommitCommentRecord, CommitFileRecord,
    CommitRecord, CommitStatusRecord, ContributorRecord, DeploymentRecord, IssueEventRecord,
    IssueReactionRecord, IssueRecord, IssueTimelineEventRecord, LabelRecord, MilestoneRecord,
    PullRequestCommitRecord, PullRequestFileRecord, PullRequestRecord, ReleaseRecord, RepoRecord,
    ReviewCommentRecord, ReviewRecord, ReviewThreadRecord, TagRecord, WorkflowJobRecord,
    WorkflowRunRecord,
};
use github_mirror::domain::service::{Service, ServiceConfig, SyncJob};
use github_mirror::infra::storage::migrations::Migrator;
use github_mirror::infra::storage::sea_orm_repo::{
    SeaOrmBranchRepository, SeaOrmCheckRunRepository, SeaOrmCommentRepository,
    SeaOrmCommitCommentRepository, SeaOrmCommitFileRepository, SeaOrmCommitRepository,
    SeaOrmCommitStatusRepository, SeaOrmContributorRepository, SeaOrmDeploymentRepository,
    SeaOrmEntityFingerprintRepository, SeaOrmIssueEventRepository, SeaOrmIssueReactionRepository,
    SeaOrmIssueRepository, SeaOrmIssueTimelineRepository, SeaOrmLabelRepository,
    SeaOrmMilestoneRepository, SeaOrmPullRequestCommitRepository, SeaOrmPullRequestFileRepository,
    SeaOrmPullRequestRepository, SeaOrmReleaseRepository, SeaOrmRepoRepository,
    SeaOrmRepoSyncStatusRepository, SeaOrmReviewCommentRepository, SeaOrmReviewRepository,
    SeaOrmReviewThreadRepository, SeaOrmSyncSessionRepository, SeaOrmSyncWatermarkRepository,
    SeaOrmSyncWriter, SeaOrmTagRepository, SeaOrmWorkflowJobRepository,
    SeaOrmWorkflowRunRepository,
};
use toolkit::api::canonical_prelude::CanonicalError;
use toolkit::{ClientHub, ConfigProvider, GearCtx};
use toolkit_db::migration_runner::run_migrations_for_testing;
use toolkit_db::{ConnectOpts, DBProvider, Db, connect_db};
use toolkit_security::{PlatformSecurityContext, SecurityContext, pep_properties};
use uuid::Uuid;

pub type ConcreteService = Service;

/// PDP fake: allows everything, constrained to the caller's tenant.
pub struct MockAuthZResolver;

#[async_trait]
impl AuthZResolverApi for MockAuthZResolver {
    async fn evaluate(
        &self,
        _ctx: PlatformSecurityContext,
        request: EvaluationRequest,
    ) -> Result<EvaluationResponse, CanonicalError> {
        let root_id = request
            .context
            .tenant_context
            .as_ref()
            .and_then(|tc| tc.root_id)
            .or_else(|| {
                request
                    .subject
                    .properties
                    .get("tenant_id")
                    .and_then(|v| v.as_str())
                    .and_then(|s| Uuid::parse_str(s).ok())
            })
            .ok_or_else(|| CanonicalError::internal("tenant context is required").create())?;

        let predicates = vec![Predicate::In(InPredicate::new(
            pep_properties::OWNER_TENANT_ID,
            [root_id],
        ))];

        Ok(EvaluationResponse {
            decision: true,
            context: EvaluationResponseContext {
                constraints: vec![Constraint { predicates }],
                ..Default::default()
            },
        })
    }
}

/// PDP fake that denies everything: for exercising the deny path.
pub struct DenyAllAuthZResolver;

#[async_trait]
impl AuthZResolverApi for DenyAllAuthZResolver {
    async fn evaluate(
        &self,
        _ctx: PlatformSecurityContext,
        _request: EvaluationRequest,
    ) -> Result<EvaluationResponse, CanonicalError> {
        Ok(EvaluationResponse {
            decision: false,
            context: EvaluationResponseContext::default(),
        })
    }
}

/// GitHub fake: serves a pre-baked repository, sliced per port call, or
/// `NotFound` when empty.
pub struct FakeGithub {
    pub result: Option<FetchedRepository>,
}

impl FakeGithub {
    fn fixture(&self) -> Result<&FetchedRepository, DomainError> {
        self.result.as_ref().ok_or(DomainError::NotFound)
    }
}

/// What GitHub's `?since=` does to a listing: keep an entity when its stamp
/// is at or after the bound. No stamp, or no bound, keeps it.
fn not_before(stamp: Option<&str>, since: Option<chrono::DateTime<chrono::Utc>>) -> bool {
    let (Some(stamp), Some(since)) = (stamp, since) else {
        return true;
    };
    chrono::DateTime::parse_from_rfc3339(stamp)
        .is_ok_and(|at| at.with_timezone(&chrono::Utc) >= since)
}

/// The fixture's completeness narrowed to the listings one port method
/// actually walked, the way the real client reports only its own families.
fn only(complete: &ListingCompleteness, listings: &[Listing]) -> ListingCompleteness {
    let mut narrowed = ListingCompleteness::none();
    for listing in listings {
        narrowed.set(*listing, complete.is_complete(*listing));
    }
    narrowed
}

#[async_trait]
impl GithubPort for FakeGithub {
    async fn fetch_repository_metadata(
        &self,
        _owner: &str,
        _name: &str,
        _options: &FetchOptions,
    ) -> Result<RepoRecord, DomainError> {
        Ok(self.fixture()?.repository.clone())
    }

    #[allow(clippy::too_many_arguments)]
    async fn list_issues(
        &self,
        _owner: &str,
        _name: &str,
        _repo_id: i64,
        updated_after: Option<chrono::DateTime<chrono::Utc>>,
        _page1_etag: Option<&str>,
        _continue_from: Option<&str>,
        _options: &FetchOptions,
    ) -> Result<IssueListing, DomainError> {
        let f = self.fixture()?;
        Ok(IssueListing {
            complete: if updated_after.is_some() {
                ListingCompleteness::none()
            } else {
                only(&f.complete, &[Listing::Issues, Listing::Comments])
            },
            issues: f
                .issues
                .iter()
                .filter(|i| not_before(Some(&i.updated_at), updated_after))
                .cloned()
                .collect(),
            comments: f
                .comments
                .iter()
                .filter(|c| not_before(Some(&c.updated_at), updated_after))
                .cloned()
                .collect(),
            issue_events: f.issue_events.clone(),
            contributors: f.contributors.clone(),
            page1_etag: None,
            unchanged: false,
            swept_to_end: true,
            next: None,
        })
    }

    async fn refine_issue(
        &self,
        _owner: &str,
        _name: &str,
        _repo_id: i64,
        number: i64,
        wants: IssueDetailWants,
        _options: &FetchOptions,
    ) -> Result<IssueDetail, DomainError> {
        let f = self.fixture()?;
        Ok(IssueDetail {
            issue_number: number,
            reactions: if wants.reactions {
                f.issue_reactions
                    .iter()
                    .filter(|r| r.issue_number == number)
                    .cloned()
                    .collect()
            } else {
                Vec::new()
            },
            timeline: if wants.timeline {
                f.issue_timeline
                    .iter()
                    .filter(|e| e.issue_number == number)
                    .cloned()
                    .collect()
            } else {
                Vec::new()
            },
        })
    }

    async fn list_pull_requests(
        &self,
        _owner: &str,
        _name: &str,
        _repo_id: i64,
        _page1_etag: Option<&str>,
        _continue_from: Option<&str>,
        _options: &FetchOptions,
    ) -> Result<PullListing, DomainError> {
        let f = self.fixture()?;
        Ok(PullListing {
            complete: only(
                &f.complete,
                &[Listing::PullRequests, Listing::ReviewComments],
            ),
            pull_requests: f.pull_requests.clone(),
            review_comments: f.review_comments.clone(),
            contributors: Vec::new(),
            page1_etag: None,
            unchanged: false,
            swept_to_end: true,
            next: None,
        })
    }

    async fn refine_pull_request(
        &self,
        _owner: &str,
        _name: &str,
        _repo_id: i64,
        number: i64,
        _options: &FetchOptions,
    ) -> Result<PullDetail, DomainError> {
        let f = self.fixture()?;
        let pull_request = f
            .pull_requests
            .iter()
            .find(|p| p.number == number)
            .cloned()
            .ok_or(DomainError::NotFound)?;
        Ok(PullDetail {
            pull_request,
            reviews: f
                .reviews
                .iter()
                .filter(|r| r.pull_number == number)
                .cloned()
                .collect(),
            files: f
                .pull_request_files
                .iter()
                .filter(|r| r.pull_number == number)
                .cloned()
                .collect(),
            commits: f
                .pull_request_commits
                .iter()
                .filter(|r| r.pull_number == number)
                .cloned()
                .collect(),
            review_threads: f
                .review_threads
                .iter()
                .filter(|r| r.pull_number == number)
                .cloned()
                .collect(),
            declared: DeclaredCounts::default(),
            contributors: Vec::new(),
        })
    }

    #[allow(clippy::too_many_arguments)]
    async fn list_commits(
        &self,
        _owner: &str,
        _name: &str,
        _repo_id: i64,
        updated_after: Option<chrono::DateTime<chrono::Utc>>,
        _page1_etag: Option<&str>,
        _continue_from: Option<&str>,
        _options: &FetchOptions,
    ) -> Result<CommitListing, DomainError> {
        let f = self.fixture()?;
        Ok(CommitListing {
            complete: if updated_after.is_some() {
                ListingCompleteness::none()
            } else {
                only(&f.complete, &[Listing::Commits])
            },
            commits: f
                .commits
                .iter()
                .filter(|c| not_before(c.committed_at.as_deref(), updated_after))
                .cloned()
                .collect(),
            commit_comments: f.commit_comments.clone(),
            contributors: Vec::new(),
            page1_etag: None,
            unchanged: false,
            swept_to_end: true,
            next: None,
        })
    }

    async fn refine_commit(
        &self,
        _owner: &str,
        _name: &str,
        _repo_id: i64,
        sha: &str,
        with_ci: bool,
        _options: &FetchOptions,
    ) -> Result<CommitDetail, DomainError> {
        let f = self.fixture()?;
        let commit = f
            .commits
            .iter()
            .find(|c| c.sha == sha)
            .cloned()
            .ok_or(DomainError::NotFound)?;
        Ok(CommitDetail {
            commit,
            files: f
                .commit_files
                .iter()
                .filter(|r| r.commit_sha == sha)
                .cloned()
                .collect(),
            statuses: if with_ci {
                f.commit_statuses
                    .iter()
                    .filter(|r| r.commit_sha == sha)
                    .cloned()
                    .collect()
            } else {
                Vec::new()
            },
            check_runs: if with_ci {
                f.check_runs
                    .iter()
                    .filter(|r| r.head_sha == sha)
                    .cloned()
                    .collect()
            } else {
                Vec::new()
            },
        })
    }

    async fn list_metadata(
        &self,
        _owner: &str,
        _name: &str,
        _repo_id: i64,
        _options: &FetchOptions,
    ) -> Result<MetadataListing, DomainError> {
        let f = self.fixture()?;
        Ok(MetadataListing {
            complete: only(
                &f.complete,
                &[
                    Listing::Labels,
                    Listing::Milestones,
                    Listing::Releases,
                    Listing::Branches,
                    Listing::Tags,
                ],
            ),
            labels: f.labels.clone(),
            milestones: f.milestones.clone(),
            releases: f.releases.clone(),
            branches: f.branches.clone(),
            tags: f.tags.clone(),
        })
    }

    async fn list_actions(
        &self,
        _owner: &str,
        _name: &str,
        _repo_id: i64,
        _options: &FetchOptions,
    ) -> Result<ActionsListing, DomainError> {
        let f = self.fixture()?;
        Ok(ActionsListing {
            workflow_runs: f.workflow_runs.clone(),
            deployments: f.deployments.clone(),
        })
    }

    async fn refine_workflow_run(
        &self,
        _owner: &str,
        _name: &str,
        _repo_id: i64,
        run_id: i64,
        _options: &FetchOptions,
    ) -> Result<Vec<WorkflowJobRecord>, DomainError> {
        Ok(self
            .fixture()?
            .workflow_jobs
            .iter()
            .filter(|j| j.run_id == run_id)
            .cloned()
            .collect())
    }

    async fn clear_cache(
        &self,
        _tenant_id: Uuid,
        _owner: &str,
        _name: Option<&str>,
    ) -> Result<u64, DomainError> {
        Ok(0)
    }
}

pub async fn inmem_db() -> Db {
    use sea_orm_migration::MigratorTrait;

    let opts = ConnectOpts {
        max_conns: Some(1),
        min_conns: Some(1),
        ..Default::default()
    };
    let db = connect_db("sqlite::memory:", opts)
        .await
        .unwrap_or_else(|e| panic!("in-memory database must connect: {e}"));

    run_migrations_for_testing(&db, Migrator::migrations())
        .await
        .unwrap_or_else(|e| panic!("migrations must apply: {e}"));

    db
}

pub fn enforcer() -> PolicyEnforcer {
    let authz: Arc<dyn AuthZResolverApi> = Arc::new(MockAuthZResolver);
    PolicyEnforcer::new(authz)
}

pub fn deny_enforcer() -> PolicyEnforcer {
    let authz: Arc<dyn AuthZResolverApi> = Arc::new(DenyAllAuthZResolver);
    PolicyEnforcer::new(authz)
}

pub fn service_with_github(
    db: Db,
    api_base_url: &str,
    github: Arc<dyn GithubPort>,
) -> Arc<ConcreteService> {
    service_with_enforcer(db, api_base_url, github, enforcer())
}

pub fn service_with_enforcer(
    db: Db,
    api_base_url: &str,
    github: Arc<dyn GithubPort>,
    policy_enforcer: PolicyEnforcer,
) -> Arc<ConcreteService> {
    let db = Arc::new(DBProvider::new(db));
    Arc::new(Service::new(
        Arc::clone(&db),
        Arc::new(SeaOrmRepoRepository::new(Arc::clone(&db))),
        Arc::new(SeaOrmIssueRepository::new(Arc::clone(&db))),
        Arc::new(SeaOrmPullRequestRepository::new(Arc::clone(&db))),
        Arc::new(SeaOrmCommitRepository::new(Arc::clone(&db))),
        Arc::new(SeaOrmCommentRepository::new(Arc::clone(&db))),
        Arc::new(SeaOrmReviewCommentRepository::new(Arc::clone(&db))),
        Arc::new(SeaOrmReviewRepository::new(Arc::clone(&db))),
        Arc::new(SeaOrmLabelRepository::new(Arc::clone(&db))),
        Arc::new(SeaOrmMilestoneRepository::new(Arc::clone(&db))),
        Arc::new(SeaOrmReleaseRepository::new(Arc::clone(&db))),
        Arc::new(SeaOrmBranchRepository::new(Arc::clone(&db))),
        Arc::new(SeaOrmContributorRepository::new(Arc::clone(&db))),
        Arc::new(SeaOrmWorkflowRunRepository::new(Arc::clone(&db))),
        Arc::new(SeaOrmPullRequestFileRepository::new(Arc::clone(&db))),
        Arc::new(SeaOrmTagRepository::new(Arc::clone(&db))),
        Arc::new(SeaOrmCommitFileRepository::new(Arc::clone(&db))),
        Arc::new(SeaOrmReviewThreadRepository::new(Arc::clone(&db))),
        Arc::new(SeaOrmCommitCommentRepository::new(Arc::clone(&db))),
        Arc::new(SeaOrmIssueEventRepository::new(Arc::clone(&db))),
        Arc::new(SeaOrmDeploymentRepository::new(Arc::clone(&db))),
        Arc::new(SeaOrmPullRequestCommitRepository::new(Arc::clone(&db))),
        Arc::new(SeaOrmCommitStatusRepository::new(Arc::clone(&db))),
        Arc::new(SeaOrmWorkflowJobRepository::new(Arc::clone(&db))),
        Arc::new(SeaOrmIssueReactionRepository::new(Arc::clone(&db))),
        Arc::new(SeaOrmCheckRunRepository::new(Arc::clone(&db))),
        Arc::new(SeaOrmIssueTimelineRepository::new(Arc::clone(&db))),
        Arc::new(SeaOrmSyncSessionRepository::new(Arc::clone(&db))),
        Arc::new(SeaOrmRepoSyncStatusRepository::new(Arc::clone(&db))),
        Arc::new(SeaOrmSyncWriter::new(Arc::clone(&db))),
        Arc::new(SeaOrmEntityFingerprintRepository::new(Arc::clone(&db))),
        Arc::new(SeaOrmSyncWatermarkRepository::new(Arc::clone(&db))),
        github,
        policy_enforcer,
        ServiceConfig {
            api_base_url: api_base_url.to_owned(),
            scope: github_mirror::domain::scope::ScopeConfig::default(),
            max_concurrent_syncs: 1,
            max_concurrent_tasks: 1,
        },
    ))
}

pub fn service_over(db: Db, api_base_url: &str) -> Arc<ConcreteService> {
    service_with_github(db, api_base_url, Arc::new(FakeGithub { result: None }))
}

pub async fn service(api_base_url: &str) -> Arc<ConcreteService> {
    service_over(inmem_db().await, api_base_url)
}

pub struct StaticConfig {
    pub section: Option<serde_json::Value>,
}

impl ConfigProvider for StaticConfig {
    fn get_gear_config(&self, gear_name: &str) -> Option<&serde_json::Value> {
        if gear_name == "github-mirror" {
            self.section.as_ref()
        } else {
            None
        }
    }
}

/// A `GearCtx` good enough for `Gear::init`: config + hub with a fake PDP + an
/// in-memory database with migrations applied.
pub async fn gear_ctx(hub: Arc<ClientHub>, section: Option<serde_json::Value>) -> GearCtx {
    let authz: Arc<dyn AuthZResolverApi> = Arc::new(MockAuthZResolver);
    hub.register::<dyn AuthZResolverApi>(authz);

    GearCtx::new(
        "github-mirror",
        Uuid::new_v4(),
        Arc::new(StaticConfig { section }),
        hub,
        tokio_util::sync::CancellationToken::new(),
    )
    .with_db(DBProvider::new(inmem_db().await))
}

/// Stands in for the gear's background sync worker.
///
/// `POST /sync` only queues work now, so a test that wants the sync to have
/// happened takes the pump once and drains it after each request — the same
/// `run_sync_job` call the real worker makes, minus the task and the select
/// loop.
pub struct SyncPump {
    rx: tokio::sync::mpsc::Receiver<SyncJob>,
}

impl SyncPump {
    /// Claim the job stream. Panics if something already took it.
    pub async fn take(service: &ConcreteService) -> Self {
        Self {
            rx: service
                .take_sync_receiver()
                .await
                .expect("the job receiver must still be available"),
        }
    }

    /// Run every job queued so far, in order, and report how many there were.
    pub async fn drain(&mut self, service: &ConcreteService) -> usize {
        let mut ran = 0;
        while let Ok(job) = self.rx.try_recv() {
            service
                .run_sync_job(&job, &tokio_util::sync::CancellationToken::new())
                .await
                .expect("the session outcome must be recorded");
            ran += 1;
        }
        ran
    }

    /// Run every queued job under `cancel`, so a test can interrupt a sync.
    pub async fn drain_under(
        &mut self,
        service: &ConcreteService,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> usize {
        let mut ran = 0;
        while let Ok(job) = self.rx.try_recv() {
            service
                .run_sync_job(&job, cancel)
                .await
                .expect("the session outcome must be recorded");
            ran += 1;
        }
        ran
    }
}

pub fn caller_in(tenant_id: Uuid) -> SecurityContext {
    SecurityContext::builder()
        .subject_id(Uuid::new_v4())
        .subject_tenant_id(tenant_id)
        .build()
        .unwrap_or_else(|e| panic!("test caller context must build: {e}"))
}

pub fn caller() -> SecurityContext {
    caller_in(Uuid::new_v4())
}

/// An RFC3339 literal as the instant the mirror stores.
pub fn instant(raw: &str) -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::parse_from_rfc3339(raw)
        .expect("test timestamps must be valid RFC3339")
        .with_timezone(&chrono::Utc)
}

pub fn fetched_repository() -> FetchedRepository {
    FetchedRepository {
        complete: ListingCompleteness::all_complete(),
        repository: RepoRecord {
            node_id: None,
            id: 42,
            owner: "rust-lang".to_owned(),
            name: "rust".to_owned(),
            full_name: "rust-lang/rust".to_owned(),
            default_branch: "master".to_owned(),
            private: false,
            pushed_at: Some("2026-08-20T00:00:00Z".to_owned()),
            stars: 100_000,
            forks: 13_000,
            description: Some("the compiler".to_owned()),
            clone_url: None,
        },
        issues: vec![IssueRecord {
            author_login: Some("alice".to_owned()),
            author_json: None,
            assignees_json: None,
            labels_json: None,
            comments_count: None,
            locked: None,
            node_id: None,
            id: 1,
            repo_id: 42,
            number: 11,
            title: "an issue".to_owned(),
            body: None,
            state: "open".to_owned(),
            is_pull_request: false,
            created_at: "2026-08-20T00:00:00Z".to_owned(),
            updated_at: "2026-08-20T00:00:00Z".to_owned(),
            closed_at: None,
            html_url: None,
        }],
        pull_requests: vec![PullRequestRecord {
            author_login: Some("alice".to_owned()),
            author_json: None,
            assignees_json: None,
            labels_json: None,
            comments_count: None,
            locked: None,
            requested_reviewers_json: None,
            node_id: None,
            id: 2,
            repo_id: 42,
            number: 12,
            title: "a pr".to_owned(),
            body: None,
            state: "open".to_owned(),
            draft: false,
            merged: false,
            head_sha: Some("h1".to_owned()),
            base_sha: Some("b1".to_owned()),
            lines_added: 0,
            lines_removed: 0,
            created_at: "2026-08-20T00:00:00Z".to_owned(),
            updated_at: "2026-08-20T00:00:00Z".to_owned(),
            closed_at: None,
            merged_at: None,
            html_url: Some("https://github.com/rust-lang/rust/pull/12".to_owned()),
            head_ref: Some("feature".to_owned()),
            base_ref: Some("master".to_owned()),
        }],
        commits: vec![
            CommitRecord {
                repo_id: 42,
                sha: "c1".to_owned(),
                message: "first".to_owned(),
                author_login: None,
                committer_login: None,
                authored_at: None,
                committed_at: Some("2026-08-19T00:00:00Z".to_owned()),
                additions: 0,
                deletions: 0,
            },
            CommitRecord {
                repo_id: 42,
                sha: "c2".to_owned(),
                message: "second".to_owned(),
                author_login: None,
                committer_login: None,
                authored_at: None,
                committed_at: Some("2026-08-20T00:00:00Z".to_owned()),
                additions: 0,
                deletions: 0,
            },
        ],
        comments: vec![CommentRecord {
            id: 9,
            repo_id: 42,
            issue_number: 11,
            author_login: Some("carol".to_owned()),
            body: Some("looks good".to_owned()),
            created_at: "2026-08-20T00:00:00Z".to_owned(),
            updated_at: "2026-08-20T00:00:00Z".to_owned(),
            html_url: None,
        }],
        review_comments: vec![ReviewCommentRecord {
            id: 21,
            repo_id: 42,
            pull_number: 12,
            author_login: Some("dave".to_owned()),
            body: Some("rename this".to_owned()),
            path: Some("src/lib.rs".to_owned()),
            diff_hunk: None,
            in_reply_to_id: None,
            commit_id: Some("h1".to_owned()),
            created_at: "2026-08-20T00:00:00Z".to_owned(),
            updated_at: "2026-08-20T00:00:00Z".to_owned(),
            html_url: None,
            position: Some(7),
            original_position: Some(4),
            pull_request_review_id: Some(31),
            line: Some(12),
            original_line: Some(12),
            start_line: None,
            original_start_line: None,
            side: Some("RIGHT".to_owned()),
            start_side: None,
            subject_type: Some("line".to_owned()),
        }],
        reviews: vec![ReviewRecord {
            id: 31,
            repo_id: 42,
            pull_number: 12,
            author_login: Some("erin".to_owned()),
            state: "APPROVED".to_owned(),
            body: Some("ship it".to_owned()),
            commit_id: Some("h1".to_owned()),
            submitted_at: Some("2026-08-20T00:00:00Z".to_owned()),
            html_url: None,
        }],
        labels: vec![LabelRecord {
            id: 41,
            repo_id: 42,
            name: "bug".to_owned(),
            color: "d73a4a".to_owned(),
            is_default: true,
            description: Some("Something is not working".to_owned()),
        }],
        milestones: vec![MilestoneRecord {
            id: 51,
            repo_id: 42,
            number: 1,
            title: "v1.0".to_owned(),
            state: "open".to_owned(),
            description: Some("first stable".to_owned()),
            open_issues: 3,
            closed_issues: 7,
            due_on: Some("2026-09-30T00:00:00Z".to_owned()),
            created_at: "2026-08-20T00:00:00Z".to_owned(),
            updated_at: "2026-08-20T00:00:00Z".to_owned(),
            closed_at: None,
            html_url: None,
        }],
        releases: vec![ReleaseRecord {
            id: 61,
            repo_id: 42,
            tag_name: "v1.0.0".to_owned(),
            name: Some("First stable".to_owned()),
            draft: false,
            prerelease: false,
            body: Some("changelog".to_owned()),
            author_login: Some("erin".to_owned()),
            created_at: "2026-08-20T00:00:00Z".to_owned(),
            published_at: Some("2026-08-20T00:00:00Z".to_owned()),
            html_url: None,
            assets_json: None,
        }],
        branches: vec![BranchRecord {
            repo_id: 42,
            name: "master".to_owned(),
            commit_sha: "c2".to_owned(),
            protected: true,
        }],
        contributors: vec![ContributorRecord {
            repo_id: 42,
            user_id: 71,
            login: Some("alice".to_owned()),
            account_type: "User".to_owned(),
            avatar_url: None,
            html_url: None,
            roles: vec!["author".to_owned()],
            first_seen_at: Some(instant("2026-08-18T00:00:00Z")),
            last_seen_at: Some(instant("2026-08-20T00:00:00Z")),
        }],
        workflow_runs: vec![WorkflowRunRecord {
            id: 81,
            repo_id: 42,
            workflow_id: 8,
            run_number: 300,
            run_attempt: 1,
            name: Some("CI".to_owned()),
            event: "push".to_owned(),
            status: Some("completed".to_owned()),
            conclusion: Some("success".to_owned()),
            head_branch: Some("master".to_owned()),
            head_sha: "c2".to_owned(),
            created_at: "2026-08-20T00:00:00Z".to_owned(),
            updated_at: "2026-08-20T00:00:00Z".to_owned(),
            html_url: None,
            actor_login: Some("alice".to_owned()),
        }],
        pull_request_files: vec![PullRequestFileRecord {
            patch: None,
            repo_id: 42,
            pull_number: 12,
            filename: "src/lib.rs".to_owned(),
            status: "modified".to_owned(),
            additions: 10,
            deletions: 2,
            changes: 12,
            previous_filename: None,
            sha: Some("blob1".to_owned()),
        }],
        tags: vec![TagRecord {
            repo_id: 42,
            name: "v1.0.0".to_owned(),
            commit_sha: "c1".to_owned(),
        }],
        commit_files: vec![CommitFileRecord {
            repo_id: 42,
            commit_sha: "c1".to_owned(),
            filename: "src/lib.rs".to_owned(),
            status: "modified".to_owned(),
            additions: 4,
            deletions: 1,
            changes: 5,
            previous_filename: None,
            sha: Some("blob9".to_owned()),
        }],
        review_threads: vec![ReviewThreadRecord {
            id: "PRRT_thread1".to_owned(),
            repo_id: 42,
            pull_number: 12,
            is_resolved: true,
            is_outdated: false,
            path: Some("src/lib.rs".to_owned()),
            line: Some(10),
            resolved_by: Some("erin".to_owned()),
            comments_count: 3,
        }],
        commit_comments: vec![CommitCommentRecord {
            id: 91,
            repo_id: 42,
            commit_sha: "c1".to_owned(),
            path: None,
            position: None,
            author_login: Some("frank".to_owned()),
            body: Some("nice commit".to_owned()),
            created_at: "2026-08-20T00:00:00Z".to_owned(),
            updated_at: "2026-08-20T00:00:00Z".to_owned(),
            html_url: None,
        }],
        issue_events: vec![IssueEventRecord {
            id: 101,
            repo_id: 42,
            issue_number: 11,
            event: "labeled".to_owned(),
            actor_login: Some("grace".to_owned()),
            label_name: Some("bug".to_owned()),
            assignee_login: None,
            milestone_title: None,
            commit_id: None,
            created_at: "2026-08-20T00:00:00Z".to_owned(),
        }],
        deployments: vec![DeploymentRecord {
            id: 111,
            repo_id: 42,
            git_ref: "master".to_owned(),
            sha: "c2".to_owned(),
            environment: "production".to_owned(),
            task: "deploy".to_owned(),
            description: Some("ship".to_owned()),
            creator_login: Some("heidi".to_owned()),
            created_at: "2026-08-20T00:00:00Z".to_owned(),
            updated_at: "2026-08-20T00:00:00Z".to_owned(),
        }],
        pull_request_commits: vec![PullRequestCommitRecord {
            repo_id: 42,
            pull_number: 12,
            sha: "pc1".to_owned(),
            message: "pr commit".to_owned(),
            author_login: Some("ivan".to_owned()),
            committer_login: Some("ivan".to_owned()),
            authored_at: Some("2026-08-20T00:00:00Z".to_owned()),
            committed_at: Some("2026-08-20T00:00:00Z".to_owned()),
        }],
        commit_statuses: vec![CommitStatusRecord {
            id: 121,
            repo_id: 42,
            commit_sha: "c1".to_owned(),
            state: "success".to_owned(),
            context: "ci/build".to_owned(),
            description: Some("build passed".to_owned()),
            target_url: None,
            creator_login: Some("judy".to_owned()),
            created_at: "2026-08-20T00:00:00Z".to_owned(),
            updated_at: "2026-08-20T00:00:00Z".to_owned(),
        }],
        workflow_jobs: vec![WorkflowJobRecord {
            id: 910,
            repo_id: 42,
            run_id: 81,
            run_attempt: 1,
            name: "build".to_owned(),
            status: Some("completed".to_owned()),
            conclusion: Some("success".to_owned()),
            head_sha: "c1".to_owned(),
            runner_name: Some("ubuntu-latest".to_owned()),
            started_at: Some("2026-08-20T00:00:00Z".to_owned()),
            completed_at: Some("2026-08-20T00:05:00Z".to_owned()),
            html_url: None,
            steps_json: Some(r#"[{"name":"Checkout","number":1}]"#.to_owned()),
        }],
        issue_reactions: vec![IssueReactionRecord {
            id: 555,
            repo_id: 42,
            issue_number: 11,
            content: "heart".to_owned(),
            user_login: Some("kate".to_owned()),
            created_at: "2026-08-20T00:00:00Z".to_owned(),
        }],
        check_runs: vec![CheckRunRecord {
            id: 771,
            repo_id: 42,
            head_sha: "c1".to_owned(),
            name: "clippy".to_owned(),
            status: Some("completed".to_owned()),
            conclusion: Some("success".to_owned()),
            started_at: Some("2026-08-20T00:00:00Z".to_owned()),
            completed_at: Some("2026-08-20T00:03:00Z".to_owned()),
            html_url: None,
            details_url: None,
            check_suite_id: Some(900),
            app_slug: Some("github-actions".to_owned()),
            app_name: Some("GitHub Actions".to_owned()),
            output_title: Some("no warnings".to_owned()),
            output_summary: None,
            annotations_count: 0,
        }],
        issue_timeline: vec![IssueTimelineEventRecord {
            repo_id: 42,
            issue_number: 11,
            position: 0,
            event: "labeled".to_owned(),
            created_at: Some("2026-08-20T00:00:00Z".to_owned()),
            actor_login: Some("kate".to_owned()),
            payload_json: r#"{"event":"labeled","label":{"name":"bug"}}"#.to_owned(),
        }],
    }
}
