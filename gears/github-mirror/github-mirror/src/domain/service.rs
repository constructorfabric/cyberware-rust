use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use authz_resolver_sdk::PolicyEnforcer;
use authz_resolver_sdk::pep::{AccessRequest, ResourceType};
use chrono::{DateTime, Utc};
use github_mirror_sdk::{
    Branch, CheckRun, Comment, Commit, CommitComment, CommitFile, CommitStatus, Contributor,
    Deployment, Issue, IssueEvent, IssueReaction, IssueTimelineEvent, Label, Milestone,
    MirrorStatus, PullRequest, PullRequestCommit, PullRequestFile, Release, Repo, Review,
    ReviewComment, ReviewThread, SyncSummary, Tag, WorkflowJob, WorkflowRun,
};
use tokio::sync::{Mutex, mpsc};
use tokio_util::sync::CancellationToken;
use toolkit_macros::domain_model;
use toolkit_odata::{ODataQuery, Page, PageInfo};
use toolkit_security::{AccessScope, SecurityContext, pep_properties};
use uuid::Uuid;

use super::error::DomainError;
use super::ports::github::{FetchOptions, GithubPort};
use super::repo::{
    BranchRecord, BranchRepository, CheckRunRecord, CheckRunRepository, CommentRecord,
    CommentRepository, CommitCommentRecord, CommitCommentRepository, CommitFileRecord,
    CommitFileRepository, CommitRecord, CommitRepository, CommitStatusRecord,
    CommitStatusRepository, ContributorRecord, ContributorRepository, DeploymentRecord,
    DeploymentRepository, EntityFingerprintRepository, IssueEventRecord, IssueEventRepository,
    IssueReactionRecord, IssueReactionRepository, IssueRecord, IssueRepository,
    IssueTimelineEventRecord, IssueTimelineRepository, LabelRecord, LabelRepository, ListingFilter,
    MilestoneRecord, MilestoneRepository, PageWindow, PullRequestCommitRecord,
    PullRequestCommitRepository, PullRequestFileRecord, PullRequestFileRepository,
    PullRequestRecord, PullRequestRepository, ReleaseRecord, ReleaseRepository, RepoRecord,
    RepoRepository, RepoSyncStatusRecord, RepoSyncStatusRepository, ReviewCommentRecord,
    ReviewCommentRepository, ReviewRecord, ReviewRepository, ReviewThreadRecord,
    ReviewThreadRepository, SyncSessionRecord, SyncSessionRepository, SyncWatermarkRepository,
    SyncWriter, TagRecord, TagRepository, WorkflowJobRecord, WorkflowJobRepository,
    WorkflowRunRecord, WorkflowRunRepository,
};
use super::scope::ScopeConfig;
use super::sync::{
    ChangeGate, MirrorWorker, RepoPhaseRunner, RunState, SweepWatermark, TaskPhase, Worker,
    sweep_families,
};
use super::validate::{repo_full_name, validate_commit_sha};

/// The gear's name, taken from the `#[toolkit::gear]` attribute so the
/// literal exists in exactly one place.
pub const GEAR_NAME: &str = crate::gear::GithubMirrorGear::MODULE_NAME;

const DEFAULT_LIST_LIMIT: u64 = 50;

/// The current instant as RFC3339 text.
///
/// Session and run-status rows still store their timestamps as text (their
/// tables predate the typed columns), so they format here; `extracted_at` and
/// the contributor window are real timestamps and never pass through this.
fn now_rfc3339() -> String {
    Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// Release the per-repo sync lock, logging a failed release rather than
/// turning it into the sync's outcome: the guard's `Drop` has already queued
/// a best-effort release, and the sync succeeded or failed on its own merits.
async fn release_sync_lock(lock: toolkit_db::DbLockGuard, lock_key: &str) {
    if let Err(e) = lock.release().await {
        tracing::warn!(lock_key, error = %e, "sync advisory lock release failed");
    }
}

pub(crate) type DbProvider = toolkit_db::DBProvider<toolkit_db::DbError>;

pub(crate) const REPO_RESOURCE: ResourceType = ResourceType::from_static(
    "github_mirror.repo",
    &[pep_properties::OWNER_TENANT_ID, pep_properties::RESOURCE_ID],
);

pub(crate) const ISSUE_RESOURCE: ResourceType = ResourceType::from_static(
    "github_mirror.issue",
    &[pep_properties::OWNER_TENANT_ID, pep_properties::RESOURCE_ID],
);

pub(crate) const PULL_REQUEST_RESOURCE: ResourceType = ResourceType::from_static(
    "github_mirror.pull_request",
    &[pep_properties::OWNER_TENANT_ID, pep_properties::RESOURCE_ID],
);

pub(crate) const COMMIT_RESOURCE: ResourceType = ResourceType::from_static(
    "github_mirror.commit",
    &[pep_properties::OWNER_TENANT_ID, pep_properties::RESOURCE_ID],
);

pub(crate) const COMMENT_RESOURCE: ResourceType = ResourceType::from_static(
    "github_mirror.comment",
    &[pep_properties::OWNER_TENANT_ID, pep_properties::RESOURCE_ID],
);

pub(crate) const REVIEW_COMMENT_RESOURCE: ResourceType = ResourceType::from_static(
    "github_mirror.review_comment",
    &[pep_properties::OWNER_TENANT_ID, pep_properties::RESOURCE_ID],
);

pub(crate) const REVIEW_RESOURCE: ResourceType = ResourceType::from_static(
    "github_mirror.review",
    &[pep_properties::OWNER_TENANT_ID, pep_properties::RESOURCE_ID],
);

pub(crate) const LABEL_RESOURCE: ResourceType = ResourceType::from_static(
    "github_mirror.label",
    &[pep_properties::OWNER_TENANT_ID, pep_properties::RESOURCE_ID],
);

pub(crate) const MILESTONE_RESOURCE: ResourceType = ResourceType::from_static(
    "github_mirror.milestone",
    &[pep_properties::OWNER_TENANT_ID, pep_properties::RESOURCE_ID],
);

pub(crate) const RELEASE_RESOURCE: ResourceType = ResourceType::from_static(
    "github_mirror.release",
    &[pep_properties::OWNER_TENANT_ID, pep_properties::RESOURCE_ID],
);

pub(crate) const BRANCH_RESOURCE: ResourceType = ResourceType::from_static(
    "github_mirror.branch",
    &[pep_properties::OWNER_TENANT_ID, pep_properties::RESOURCE_ID],
);

pub(crate) const CONTRIBUTOR_RESOURCE: ResourceType = ResourceType::from_static(
    "github_mirror.contributor",
    &[pep_properties::OWNER_TENANT_ID, pep_properties::RESOURCE_ID],
);

pub(crate) const WORKFLOW_RUN_RESOURCE: ResourceType = ResourceType::from_static(
    "github_mirror.workflow_run",
    &[pep_properties::OWNER_TENANT_ID, pep_properties::RESOURCE_ID],
);

pub(crate) const PULL_REQUEST_FILE_RESOURCE: ResourceType = ResourceType::from_static(
    "github_mirror.pull_request_file",
    &[pep_properties::OWNER_TENANT_ID, pep_properties::RESOURCE_ID],
);

pub(crate) const TAG_RESOURCE: ResourceType = ResourceType::from_static(
    "github_mirror.tag",
    &[pep_properties::OWNER_TENANT_ID, pep_properties::RESOURCE_ID],
);

pub(crate) const COMMIT_FILE_RESOURCE: ResourceType = ResourceType::from_static(
    "github_mirror.commit_file",
    &[pep_properties::OWNER_TENANT_ID, pep_properties::RESOURCE_ID],
);

pub(crate) const REVIEW_THREAD_RESOURCE: ResourceType = ResourceType::from_static(
    "github_mirror.review_thread",
    &[pep_properties::OWNER_TENANT_ID, pep_properties::RESOURCE_ID],
);

pub(crate) const COMMIT_COMMENT_RESOURCE: ResourceType = ResourceType::from_static(
    "github_mirror.commit_comment",
    &[pep_properties::OWNER_TENANT_ID, pep_properties::RESOURCE_ID],
);

pub(crate) const ISSUE_EVENT_RESOURCE: ResourceType = ResourceType::from_static(
    "github_mirror.issue_event",
    &[pep_properties::OWNER_TENANT_ID, pep_properties::RESOURCE_ID],
);

pub(crate) const DEPLOYMENT_RESOURCE: ResourceType = ResourceType::from_static(
    "github_mirror.deployment",
    &[pep_properties::OWNER_TENANT_ID, pep_properties::RESOURCE_ID],
);

pub(crate) const PULL_REQUEST_COMMIT_RESOURCE: ResourceType = ResourceType::from_static(
    "github_mirror.pull_request_commit",
    &[pep_properties::OWNER_TENANT_ID, pep_properties::RESOURCE_ID],
);

pub(crate) const COMMIT_STATUS_RESOURCE: ResourceType = ResourceType::from_static(
    "github_mirror.commit_status",
    &[pep_properties::OWNER_TENANT_ID, pep_properties::RESOURCE_ID],
);

pub(crate) const WORKFLOW_JOB_RESOURCE: ResourceType = ResourceType::from_static(
    "github_mirror.workflow_job",
    &[pep_properties::OWNER_TENANT_ID, pep_properties::RESOURCE_ID],
);

pub(crate) const ISSUE_REACTION_RESOURCE: ResourceType = ResourceType::from_static(
    "github_mirror.issue_reaction",
    &[pep_properties::OWNER_TENANT_ID, pep_properties::RESOURCE_ID],
);

pub(crate) const CHECK_RUN_RESOURCE: ResourceType = ResourceType::from_static(
    "github_mirror.check_run",
    &[pep_properties::OWNER_TENANT_ID, pep_properties::RESOURCE_ID],
);

pub(crate) const ISSUE_TIMELINE_RESOURCE: ResourceType = ResourceType::from_static(
    "github_mirror.issue_timeline",
    &[pep_properties::OWNER_TENANT_ID, pep_properties::RESOURCE_ID],
);

/// Session lifecycle vocabulary, written into `gm_sync_sessions.state`.
/// Session lifecycle vocabulary, written into `gm_sync_sessions.status`.
///
/// `IN_PROGRESS`, `COMPLETE` and `FAILED` are DESIGN §3.7's three states.
/// `QUEUED` and `INTERRUPTED` are additions the background worker needs:
/// the design assumed a synchronous library call, where neither can occur.
pub(crate) mod session_states {
    pub const QUEUED: &str = "queued";
    pub const IN_PROGRESS: &str = "in_progress";
    pub const COMPLETE: &str = "complete";
    pub const FAILED: &str = "failed";
    pub const INTERRUPTED: &str = "interrupted";
}

/// Progress once every mirrored table has been written.
const PROGRESS_STORED: u8 = 95;
/// How often the run persists its progress while it is working.
const HEARTBEAT_SECS: u64 = 2;

/// Most repositories one resume call will re-queue.
const RESUME_LIMIT: u64 = 500;

/// Phase progress of one sync, published through a shared atomic so the
/// heartbeat can read it without touching the running fetch (DESIGN §4
/// "Progress"). Values are clamped monotonically non-decreasing.
///
/// While the phases run, `RepoPhaseRunner` publishes the phase-weighted
/// estimate into this atomic after every task it finishes; the milestones
/// below only cover what happens after the runner returns.
#[derive(Debug)]
pub struct SyncProgress {
    percent: Arc<AtomicU8>,
}

impl Default for SyncProgress {
    fn default() -> Self {
        Self::new()
    }
}

impl SyncProgress {
    #[must_use]
    pub fn new() -> Self {
        Self {
            percent: Arc::new(AtomicU8::new(0)),
        }
    }

    /// A second handle on the same counter, for the heartbeat to read.
    #[must_use]
    pub fn handle(&self) -> Arc<AtomicU8> {
        Arc::clone(&self.percent)
    }

    #[must_use]
    pub fn percent(&self) -> u8 {
        self.percent.load(Ordering::Relaxed)
    }

    fn raise_to(&self, value: u8) {
        self.percent.fetch_max(value, Ordering::Relaxed);
    }

    /// Every mirrored table has been written.
    pub(crate) fn stored(&self) {
        self.raise_to(PROGRESS_STORED);
    }

    /// The run is over, whatever its outcome.
    pub(crate) fn finished(&self) {
        self.raise_to(100);
    }
}

/// How many enqueued syncs may wait for the worker before `POST /sync` starts
/// rejecting. One repository at a time is the current worker concurrency, so
/// this is the depth of the backlog, not of the parallelism.
const SYNC_QUEUE_DEPTH: usize = 64;

/// One unit of background work: sync `owner/name` on behalf of `ctx`, and
/// record the outcome against the session row created at enqueue time.
///
/// The caller's [`SecurityContext`] travels with the job because the work
/// outlives the request that asked for it, and every write it makes is still
/// tenant-scoped through the same policy enforcer.
#[derive(Debug, Clone)]
pub struct SyncJob {
    pub session_id: Uuid,
    pub ctx: SecurityContext,
    pub owner: String,
    pub name: String,
    /// What this run collects. Resolved at enqueue time from the request, or
    /// from the gear config when the request says nothing.
    pub scope: ScopeConfig,
    /// Bypass the HTTP cache entirely (PRD §5.2 force mode).
    ///
    /// Carried end to end but **not yet honoured**: the gear has no `ETag` or
    /// `Last-Modified` cache to bypass, so every sync is already a full fetch.
    /// It starts having an effect when conditional requests land (#4630).
    pub force: bool,
    /// Oldest closed entity worth collecting, from the request.
    pub since: Option<DateTime<Utc>>,
}

/// Per-repository run status, the durable half of resume-by-rescan.
/// PRD §5.2 defines exactly two values.
pub(crate) mod repo_run_states {
    pub const IN_PROGRESS: &str = "in_progress";
    pub const COMPLETE: &str = "complete";
}

pub(crate) const REPO_SYNC_STATUS_RESOURCE: ResourceType = ResourceType::from_static(
    "github_mirror.repo_sync_status",
    &[pep_properties::OWNER_TENANT_ID, pep_properties::RESOURCE_ID],
);

pub(crate) const SYNC_SESSION_RESOURCE: ResourceType = ResourceType::from_static(
    "github_mirror.sync_session",
    &[pep_properties::OWNER_TENANT_ID, pep_properties::RESOURCE_ID],
);

pub(crate) const SYNC_RESOURCE: ResourceType =
    ResourceType::from_static("github_mirror.sync", &[pep_properties::OWNER_TENANT_ID]);

pub(crate) mod actions {
    pub const LIST: &str = "list";
    pub const UPSERT: &str = "upsert";
    pub const SYNC: &str = "sync";
    pub const GET: &str = "get";
}

#[domain_model]
#[derive(Debug, Clone)]
pub struct ServiceConfig {
    pub api_base_url: String,
    /// What a sync collects when the request does not say.
    pub scope: ScopeConfig,
    /// How many repositories may sync at the same time.
    pub max_concurrent_syncs: usize,
    /// How many tasks one repository's sync runs at the same time.
    pub max_concurrent_tasks: usize,
}

#[domain_model]
pub struct Service {
    db: Arc<DbProvider>,
    repo: Arc<dyn RepoRepository>,
    issues: Arc<dyn IssueRepository>,
    pull_requests: Arc<dyn PullRequestRepository>,
    commits: Arc<dyn CommitRepository>,
    comments: Arc<dyn CommentRepository>,
    review_comments: Arc<dyn ReviewCommentRepository>,
    reviews: Arc<dyn ReviewRepository>,
    labels: Arc<dyn LabelRepository>,
    milestones: Arc<dyn MilestoneRepository>,
    releases: Arc<dyn ReleaseRepository>,
    branches: Arc<dyn BranchRepository>,
    contributors: Arc<dyn ContributorRepository>,
    workflow_runs: Arc<dyn WorkflowRunRepository>,
    pull_request_files: Arc<dyn PullRequestFileRepository>,
    tags: Arc<dyn TagRepository>,
    commit_files: Arc<dyn CommitFileRepository>,
    review_threads: Arc<dyn ReviewThreadRepository>,
    commit_comments: Arc<dyn CommitCommentRepository>,
    issue_events: Arc<dyn IssueEventRepository>,
    deployments: Arc<dyn DeploymentRepository>,
    pull_request_commits: Arc<dyn PullRequestCommitRepository>,
    commit_statuses: Arc<dyn CommitStatusRepository>,
    workflow_jobs: Arc<dyn WorkflowJobRepository>,
    issue_reactions: Arc<dyn IssueReactionRepository>,
    check_runs: Arc<dyn CheckRunRepository>,
    issue_timeline: Arc<dyn IssueTimelineRepository>,
    sync_sessions: Arc<dyn SyncSessionRepository>,
    repo_sync_status: Arc<dyn RepoSyncStatusRepository>,
    sync_writer: Arc<dyn SyncWriter>,
    change_gate: Arc<ChangeGate>,
    sweep_watermark: Arc<SweepWatermark>,
    github: Arc<dyn GithubPort>,
    policy_enforcer: PolicyEnforcer,
    config: ServiceConfig,
    sync_tx: mpsc::Sender<SyncJob>,
    sync_rx: Arc<Mutex<Option<mpsc::Receiver<SyncJob>>>>,
    in_flight: Arc<Mutex<HashMap<InFlightKey, Uuid>>>,
}

/// One repository of one tenant: what a queued or running sync occupies.
type InFlightKey = (Uuid, String);

/// Manual `Clone`: every field is an `Arc` (cheap refcount bump) or already
/// `Clone` (`PolicyEnforcer`, `ServiceConfig`). A `#[derive(Clone)]` would add
/// a spurious `T: Clone` bound to each of the 26 repository generics even
/// though `Arc<T>: Clone` never needs one, so it is written out by hand.
///
/// Exists so a caller can obtain an owned handle to hand into a `'static`
/// closure (e.g. a DB transaction) without borrowing `&self` across it.
impl Clone for Service {
    fn clone(&self) -> Self {
        Self {
            db: Arc::clone(&self.db),
            repo: Arc::clone(&self.repo),
            issues: Arc::clone(&self.issues),
            pull_requests: Arc::clone(&self.pull_requests),
            commits: Arc::clone(&self.commits),
            comments: Arc::clone(&self.comments),
            review_comments: Arc::clone(&self.review_comments),
            reviews: Arc::clone(&self.reviews),
            labels: Arc::clone(&self.labels),
            milestones: Arc::clone(&self.milestones),
            releases: Arc::clone(&self.releases),
            branches: Arc::clone(&self.branches),
            contributors: Arc::clone(&self.contributors),
            workflow_runs: Arc::clone(&self.workflow_runs),
            pull_request_files: Arc::clone(&self.pull_request_files),
            tags: Arc::clone(&self.tags),
            commit_files: Arc::clone(&self.commit_files),
            review_threads: Arc::clone(&self.review_threads),
            commit_comments: Arc::clone(&self.commit_comments),
            issue_events: Arc::clone(&self.issue_events),
            deployments: Arc::clone(&self.deployments),
            pull_request_commits: Arc::clone(&self.pull_request_commits),
            commit_statuses: Arc::clone(&self.commit_statuses),
            workflow_jobs: Arc::clone(&self.workflow_jobs),
            issue_reactions: Arc::clone(&self.issue_reactions),
            check_runs: Arc::clone(&self.check_runs),
            issue_timeline: Arc::clone(&self.issue_timeline),
            sync_sessions: Arc::clone(&self.sync_sessions),
            repo_sync_status: Arc::clone(&self.repo_sync_status),
            sync_writer: Arc::clone(&self.sync_writer),
            change_gate: Arc::clone(&self.change_gate),
            sweep_watermark: Arc::clone(&self.sweep_watermark),
            github: Arc::clone(&self.github),
            policy_enforcer: self.policy_enforcer.clone(),
            config: self.config.clone(),
            sync_tx: self.sync_tx.clone(),
            sync_rx: Arc::clone(&self.sync_rx),
            in_flight: Arc::clone(&self.in_flight),
        }
    }
}

impl Service {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        db: Arc<DbProvider>,
        repo: Arc<dyn RepoRepository>,
        issues: Arc<dyn IssueRepository>,
        pull_requests: Arc<dyn PullRequestRepository>,
        commits: Arc<dyn CommitRepository>,
        comments: Arc<dyn CommentRepository>,
        review_comments: Arc<dyn ReviewCommentRepository>,
        reviews: Arc<dyn ReviewRepository>,
        labels: Arc<dyn LabelRepository>,
        milestones: Arc<dyn MilestoneRepository>,
        releases: Arc<dyn ReleaseRepository>,
        branches: Arc<dyn BranchRepository>,
        contributors: Arc<dyn ContributorRepository>,
        workflow_runs: Arc<dyn WorkflowRunRepository>,
        pull_request_files: Arc<dyn PullRequestFileRepository>,
        tags: Arc<dyn TagRepository>,
        commit_files: Arc<dyn CommitFileRepository>,
        review_threads: Arc<dyn ReviewThreadRepository>,
        commit_comments: Arc<dyn CommitCommentRepository>,
        issue_events: Arc<dyn IssueEventRepository>,
        deployments: Arc<dyn DeploymentRepository>,
        pull_request_commits: Arc<dyn PullRequestCommitRepository>,
        commit_statuses: Arc<dyn CommitStatusRepository>,
        workflow_jobs: Arc<dyn WorkflowJobRepository>,
        issue_reactions: Arc<dyn IssueReactionRepository>,
        check_runs: Arc<dyn CheckRunRepository>,
        issue_timeline: Arc<dyn IssueTimelineRepository>,
        sync_sessions: Arc<dyn SyncSessionRepository>,
        repo_sync_status: Arc<dyn RepoSyncStatusRepository>,
        sync_writer: Arc<dyn SyncWriter>,
        fingerprints: Arc<dyn EntityFingerprintRepository>,
        watermark_store: Arc<dyn SyncWatermarkRepository>,
        github: Arc<dyn GithubPort>,
        policy_enforcer: PolicyEnforcer,
        config: ServiceConfig,
    ) -> Self {
        let (sync_tx, sync_rx) = mpsc::channel(SYNC_QUEUE_DEPTH);
        Self {
            db,
            repo,
            issues,
            pull_requests,
            commits,
            comments,
            review_comments,
            reviews,
            labels,
            milestones,
            releases,
            branches,
            contributors,
            workflow_runs,
            pull_request_files,
            tags,
            commit_files,
            review_threads,
            commit_comments,
            issue_events,
            deployments,
            pull_request_commits,
            commit_statuses,
            workflow_jobs,
            issue_reactions,
            check_runs,
            issue_timeline,
            sync_sessions,
            repo_sync_status,
            sync_writer,
            change_gate: Arc::new(ChangeGate::new(fingerprints)),
            sweep_watermark: Arc::new(SweepWatermark::new(watermark_store)),
            github,
            policy_enforcer,
            config,
            sync_tx,
            sync_rx: Arc::new(Mutex::new(Some(sync_rx))),
            in_flight: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Fetch one mirrored repository (`owner/name`), tenant-scoped.
    ///
    /// # Errors
    /// `DomainError::NotFound` when the repository is not mirrored for this
    /// tenant; `Forbidden`/`Database`/`Internal` as usual.
    pub async fn get_repo(
        &self,
        ctx: &SecurityContext,
        owner: &str,
        name: &str,
    ) -> Result<Repo, DomainError> {
        let scope = self
            .policy_enforcer
            .access_scope_with(
                ctx,
                &REPO_RESOURCE,
                actions::LIST,
                None,
                &AccessRequest::new()
                    .resource_property(pep_properties::OWNER_TENANT_ID, ctx.subject_tenant_id()),
            )
            .await?;

        let full_name = repo_full_name(owner, name)?;
        self.repo
            .find_by_full_name(&scope, &full_name)
            .await?
            .ok_or(DomainError::NotFound)
    }

    /// Fetch one mirrored issue by number, tenant-scoped.
    ///
    /// # Errors
    /// `DomainError::NotFound` when the repository or issue is not mirrored;
    /// `Forbidden`/`Database`/`Internal` as usual.
    pub async fn get_issue(
        &self,
        ctx: &SecurityContext,
        owner: &str,
        name: &str,
        number: i64,
    ) -> Result<Issue, DomainError> {
        let scope = self
            .policy_enforcer
            .access_scope_with(
                ctx,
                &ISSUE_RESOURCE,
                actions::LIST,
                None,
                &AccessRequest::new()
                    .resource_property(pep_properties::OWNER_TENANT_ID, ctx.subject_tenant_id()),
            )
            .await?;

        let full_name = repo_full_name(owner, name)?;
        let repository = self
            .repo
            .find_by_full_name(&scope, &full_name)
            .await?
            .ok_or(DomainError::NotFound)?;

        self.issues
            .find_by_number(&scope, repository.id, number)
            .await?
            .ok_or(DomainError::NotFound)
    }

    /// Fetch one mirrored pull request by number, tenant-scoped.
    ///
    /// # Errors
    /// `DomainError::NotFound` when the repository or pull request is not
    /// mirrored; `Forbidden`/`Database`/`Internal` as usual.
    pub async fn get_pull_request(
        &self,
        ctx: &SecurityContext,
        owner: &str,
        name: &str,
        number: i64,
    ) -> Result<PullRequest, DomainError> {
        let scope = self
            .policy_enforcer
            .access_scope_with(
                ctx,
                &PULL_REQUEST_RESOURCE,
                actions::LIST,
                None,
                &AccessRequest::new()
                    .resource_property(pep_properties::OWNER_TENANT_ID, ctx.subject_tenant_id()),
            )
            .await?;

        let full_name = repo_full_name(owner, name)?;
        let repository = self
            .repo
            .find_by_full_name(&scope, &full_name)
            .await?
            .ok_or(DomainError::NotFound)?;

        self.pull_requests
            .find_by_number(&scope, repository.id, number)
            .await?
            .ok_or(DomainError::NotFound)
    }

    /// Fetch one mirrored commit by SHA, tenant-scoped.
    ///
    /// # Errors
    /// `DomainError::NotFound` when the repository or commit is not mirrored;
    /// `Forbidden`/`Database`/`Internal` as usual.
    pub async fn get_commit(
        &self,
        ctx: &SecurityContext,
        owner: &str,
        name: &str,
        sha: &str,
    ) -> Result<Commit, DomainError> {
        validate_commit_sha(sha)?;
        let scope = self
            .policy_enforcer
            .access_scope_with(
                ctx,
                &COMMIT_RESOURCE,
                actions::LIST,
                None,
                &AccessRequest::new()
                    .resource_property(pep_properties::OWNER_TENANT_ID, ctx.subject_tenant_id()),
            )
            .await?;

        let full_name = repo_full_name(owner, name)?;
        let repository = self
            .repo
            .find_by_full_name(&scope, &full_name)
            .await?
            .ok_or(DomainError::NotFound)?;

        self.commits
            .find_by_sha(&scope, repository.id, sha)
            .await?
            .ok_or(DomainError::NotFound)
    }

    #[must_use]
    pub fn status(&self) -> MirrorStatus {
        MirrorStatus {
            gear: GEAR_NAME.to_owned(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            api_base_url: self.config.api_base_url.clone(),
        }
    }

    /// List mirrored repositories visible to the caller's tenant.
    ///
    /// # Errors
    /// Returns `DomainError::Forbidden` when the PDP denies access and
    /// `DomainError::Database`/`Internal` on storage failures.
    pub async fn list_repos(
        &self,
        ctx: &SecurityContext,
        query: &ODataQuery,
    ) -> Result<Page<Repo>, DomainError> {
        let scope = self
            .policy_enforcer
            .access_scope_with(
                ctx,
                &REPO_RESOURCE,
                actions::LIST,
                None,
                &AccessRequest::new()
                    .resource_property(pep_properties::OWNER_TENANT_ID, ctx.subject_tenant_id()),
            )
            .await?;

        self.repo.list(&scope, query).await
    }

    /// One numbered page of mirrored repositories, for the
    /// GitHub-compatible `GET /user/repos`.
    ///
    /// # Errors
    /// `Forbidden` on PDP denial, `Database`/`Internal` on storage failures.
    pub async fn list_repos_page(
        &self,
        ctx: &SecurityContext,
        window: PageWindow,
    ) -> Result<Vec<Repo>, DomainError> {
        let scope = self
            .policy_enforcer
            .access_scope_with(
                ctx,
                &REPO_RESOURCE,
                actions::LIST,
                None,
                &AccessRequest::new()
                    .resource_property(pep_properties::OWNER_TENANT_ID, ctx.subject_tenant_id()),
            )
            .await?;

        self.repo.list_window(&scope, window).await
    }

    /// Insert or update a mirrored repository row for the caller's tenant.
    ///
    /// # Errors
    /// Returns `DomainError::Forbidden` when the PDP denies access and
    /// `DomainError::Database`/`Internal` on storage failures.
    pub async fn upsert_repo(
        &self,
        ctx: &SecurityContext,
        record: RepoRecord,
    ) -> Result<Repo, DomainError> {
        let tenant_id = ctx.subject_tenant_id();

        let scope = self
            .policy_enforcer
            .access_scope_with(
                ctx,
                &REPO_RESOURCE,
                actions::UPSERT,
                None,
                &AccessRequest::new().resource_property(pep_properties::OWNER_TENANT_ID, tenant_id),
            )
            .await?;

        self.repo.upsert(&scope, tenant_id, record).await
    }

    /// The repository's issues, filtered as GitHub's list endpoint filters.
    ///
    /// `filter`: GitHub's list-endpoint filters (state, sort, direction,
    /// since). The handler applies GitHub's own defaults.
    ///
    /// # Errors
    /// `NotFound` when the repository is not mirrored; `Forbidden`/`Database`
    /// as usual.
    pub async fn list_issues(
        &self,
        ctx: &SecurityContext,
        owner: &str,
        name: &str,
        window: PageWindow,
        filter: ListingFilter,
    ) -> Result<(Page<Issue>, u64), DomainError> {
        let scope = self
            .policy_enforcer
            .access_scope_with(
                ctx,
                &ISSUE_RESOURCE,
                actions::LIST,
                None,
                &AccessRequest::new()
                    .resource_property(pep_properties::OWNER_TENANT_ID, ctx.subject_tenant_id()),
            )
            .await?;

        let full_name = repo_full_name(owner, name)?;
        let repository = self
            .repo
            .find_by_full_name(&scope, &full_name)
            .await?
            .ok_or(DomainError::NotFound)?;

        let (items, total) = self
            .issues
            .page_by_repo(&scope, repository.id, window, filter)
            .await?;

        // Counted on the scope and repository already resolved above: the
        // GitHub-compatible listings report a total, and doing it here saves
        // a second policy evaluation and repository lookup per request.

        Ok((
            Page::new(
                items,
                PageInfo {
                    next_cursor: None,
                    prev_cursor: None,
                    limit: window.limit(),
                },
            ),
            total,
        ))
    }

    /// Insert or update a mirrored issue row for the caller's tenant.
    ///
    /// The owning repository must already be mirrored (`DomainError::NotFound`
    /// otherwise) so issues can never dangle.
    ///
    /// # Errors
    /// `Forbidden`/`Database`/`Internal` as usual.
    pub async fn upsert_issue(
        &self,
        ctx: &SecurityContext,
        owner: &str,
        name: &str,
        record: IssueRecord,
    ) -> Result<Issue, DomainError> {
        let tenant_id = ctx.subject_tenant_id();

        let scope = self
            .policy_enforcer
            .access_scope_with(
                ctx,
                &ISSUE_RESOURCE,
                actions::UPSERT,
                None,
                &AccessRequest::new().resource_property(pep_properties::OWNER_TENANT_ID, tenant_id),
            )
            .await?;

        let full_name = repo_full_name(owner, name)?;
        let repository = self
            .repo
            .find_by_full_name(&scope, &full_name)
            .await?
            .ok_or(DomainError::NotFound)?;

        let record = IssueRecord {
            repo_id: repository.id,
            ..record
        };
        self.issues.upsert(&scope, tenant_id, record).await
    }

    /// The repository's pull requests, filtered as GitHub's list endpoint
    /// filters.
    ///
    /// `filter`: GitHub's list-endpoint filters (state, sort, direction,
    /// since). The handler applies GitHub's own defaults.
    ///
    /// # Errors
    /// `NotFound` when the repository is not mirrored; `Forbidden`/`Database`
    /// as usual.
    pub async fn list_pull_requests(
        &self,
        ctx: &SecurityContext,
        owner: &str,
        name: &str,
        window: PageWindow,
        filter: ListingFilter,
    ) -> Result<(Page<PullRequest>, u64), DomainError> {
        let scope = self
            .policy_enforcer
            .access_scope_with(
                ctx,
                &PULL_REQUEST_RESOURCE,
                actions::LIST,
                None,
                &AccessRequest::new()
                    .resource_property(pep_properties::OWNER_TENANT_ID, ctx.subject_tenant_id()),
            )
            .await?;

        let full_name = repo_full_name(owner, name)?;
        let repository = self
            .repo
            .find_by_full_name(&scope, &full_name)
            .await?
            .ok_or(DomainError::NotFound)?;

        let (items, total) = self
            .pull_requests
            .page_by_repo(&scope, repository.id, window, filter)
            .await?;

        // Counted on the scope and repository already resolved above: the
        // GitHub-compatible listings report a total, and doing it here saves
        // a second policy evaluation and repository lookup per request.

        Ok((
            Page::new(
                items,
                PageInfo {
                    next_cursor: None,
                    prev_cursor: None,
                    limit: window.limit(),
                },
            ),
            total,
        ))
    }

    /// Insert or update a mirrored pull-request row for the caller's tenant.
    ///
    /// The owning repository must already be mirrored (`DomainError::NotFound`
    /// otherwise).
    ///
    /// # Errors
    /// `Forbidden`/`Database`/`Internal` as usual.
    pub async fn upsert_pull_request(
        &self,
        ctx: &SecurityContext,
        owner: &str,
        name: &str,
        record: PullRequestRecord,
    ) -> Result<PullRequest, DomainError> {
        let tenant_id = ctx.subject_tenant_id();

        let scope = self
            .policy_enforcer
            .access_scope_with(
                ctx,
                &PULL_REQUEST_RESOURCE,
                actions::UPSERT,
                None,
                &AccessRequest::new().resource_property(pep_properties::OWNER_TENANT_ID, tenant_id),
            )
            .await?;

        let full_name = repo_full_name(owner, name)?;
        let repository = self
            .repo
            .find_by_full_name(&scope, &full_name)
            .await?
            .ok_or(DomainError::NotFound)?;

        let record = PullRequestRecord {
            repo_id: repository.id,
            ..record
        };
        self.pull_requests.upsert(&scope, tenant_id, record).await
    }

    /// The repository's mirrored commits, newest first.
    ///
    /// # Errors
    /// `NotFound` when the repository is not mirrored; `Forbidden`/`Database`
    /// as usual.
    pub async fn list_commits(
        &self,
        ctx: &SecurityContext,
        owner: &str,
        name: &str,
        window: PageWindow,
        since: Option<DateTime<Utc>>,
    ) -> Result<(Page<Commit>, u64), DomainError> {
        let scope = self
            .policy_enforcer
            .access_scope_with(
                ctx,
                &COMMIT_RESOURCE,
                actions::LIST,
                None,
                &AccessRequest::new()
                    .resource_property(pep_properties::OWNER_TENANT_ID, ctx.subject_tenant_id()),
            )
            .await?;

        let full_name = repo_full_name(owner, name)?;
        let repository = self
            .repo
            .find_by_full_name(&scope, &full_name)
            .await?
            .ok_or(DomainError::NotFound)?;

        let (items, total) = self
            .commits
            .page_by_repo(&scope, repository.id, window, since)
            .await?;

        // Counted on the scope and repository already resolved above: the
        // GitHub-compatible listings report a total, and doing it here saves
        // a second policy evaluation and repository lookup per request.

        Ok((
            Page::new(
                items,
                PageInfo {
                    next_cursor: None,
                    prev_cursor: None,
                    limit: window.limit(),
                },
            ),
            total,
        ))
    }

    /// Insert or update a mirrored commit row for the caller's tenant.
    ///
    /// The owning repository must already be mirrored (`DomainError::NotFound`
    /// otherwise).
    ///
    /// # Errors
    /// `Forbidden`/`Database`/`Internal` as usual.
    pub async fn upsert_commit(
        &self,
        ctx: &SecurityContext,
        owner: &str,
        name: &str,
        record: CommitRecord,
    ) -> Result<Commit, DomainError> {
        let tenant_id = ctx.subject_tenant_id();

        let scope = self
            .policy_enforcer
            .access_scope_with(
                ctx,
                &COMMIT_RESOURCE,
                actions::UPSERT,
                None,
                &AccessRequest::new().resource_property(pep_properties::OWNER_TENANT_ID, tenant_id),
            )
            .await?;

        let full_name = repo_full_name(owner, name)?;
        let repository = self
            .repo
            .find_by_full_name(&scope, &full_name)
            .await?
            .ok_or(DomainError::NotFound)?;

        let record = CommitRecord {
            repo_id: repository.id,
            ..record
        };
        self.commits.upsert(&scope, tenant_id, record).await
    }

    /// List mirrored comments of one issue/PR (`owner/name` + number),
    /// tenant-scoped, oldest first.
    ///
    /// # Errors
    /// `DomainError::NotFound` when the repository is not mirrored for this
    /// tenant; `Forbidden`/`Database`/`Internal` as usual.
    pub async fn list_comments(
        &self,
        ctx: &SecurityContext,
        owner: &str,
        name: &str,
        issue_number: i64,
        window: PageWindow,
    ) -> Result<Page<Comment>, DomainError> {
        let scope = self
            .policy_enforcer
            .access_scope_with(
                ctx,
                &COMMENT_RESOURCE,
                actions::LIST,
                None,
                &AccessRequest::new()
                    .resource_property(pep_properties::OWNER_TENANT_ID, ctx.subject_tenant_id()),
            )
            .await?;

        let full_name = repo_full_name(owner, name)?;
        let repository = self
            .repo
            .find_by_full_name(&scope, &full_name)
            .await?
            .ok_or(DomainError::NotFound)?;

        let items = self
            .comments
            .list_by_issue(&scope, repository.id, issue_number, window)
            .await?;

        Ok(Page::new(
            items,
            PageInfo {
                next_cursor: None,
                prev_cursor: None,
                limit: window.limit(),
            },
        ))
    }

    /// Insert or update a mirrored comment row for the caller's tenant.
    ///
    /// # Errors
    /// `DomainError::NotFound` when the repository is not mirrored;
    /// `Forbidden`/`Database`/`Internal` as usual.
    pub async fn upsert_comment(
        &self,
        ctx: &SecurityContext,
        owner: &str,
        name: &str,
        record: CommentRecord,
    ) -> Result<Comment, DomainError> {
        let tenant_id = ctx.subject_tenant_id();

        let scope = self
            .policy_enforcer
            .access_scope_with(
                ctx,
                &COMMENT_RESOURCE,
                actions::UPSERT,
                None,
                &AccessRequest::new().resource_property(pep_properties::OWNER_TENANT_ID, tenant_id),
            )
            .await?;

        let full_name = repo_full_name(owner, name)?;
        let repository = self
            .repo
            .find_by_full_name(&scope, &full_name)
            .await?
            .ok_or(DomainError::NotFound)?;

        let record = CommentRecord {
            repo_id: repository.id,
            ..record
        };
        self.comments.upsert(&scope, tenant_id, record).await
    }

    /// List mirrored review comments of one pull request, tenant-scoped,
    /// oldest first.
    ///
    /// # Errors
    /// `DomainError::NotFound` when the repository is not mirrored for this
    /// tenant; `Forbidden`/`Database`/`Internal` as usual.
    pub async fn list_review_comments(
        &self,
        ctx: &SecurityContext,
        owner: &str,
        name: &str,
        pull_number: i64,
        window: PageWindow,
    ) -> Result<Page<ReviewComment>, DomainError> {
        let scope = self
            .policy_enforcer
            .access_scope_with(
                ctx,
                &REVIEW_COMMENT_RESOURCE,
                actions::LIST,
                None,
                &AccessRequest::new()
                    .resource_property(pep_properties::OWNER_TENANT_ID, ctx.subject_tenant_id()),
            )
            .await?;

        let full_name = repo_full_name(owner, name)?;
        let repository = self
            .repo
            .find_by_full_name(&scope, &full_name)
            .await?
            .ok_or(DomainError::NotFound)?;

        let items = self
            .review_comments
            .list_by_pull(&scope, repository.id, pull_number, window)
            .await?;

        Ok(Page::new(
            items,
            PageInfo {
                next_cursor: None,
                prev_cursor: None,
                limit: window.limit(),
            },
        ))
    }

    /// Insert or update a mirrored review-comment row for the caller's tenant.
    ///
    /// # Errors
    /// `DomainError::NotFound` when the repository is not mirrored;
    /// `Forbidden`/`Database`/`Internal` as usual.
    pub async fn upsert_review_comment(
        &self,
        ctx: &SecurityContext,
        owner: &str,
        name: &str,
        record: ReviewCommentRecord,
    ) -> Result<ReviewComment, DomainError> {
        let tenant_id = ctx.subject_tenant_id();

        let scope = self
            .policy_enforcer
            .access_scope_with(
                ctx,
                &REVIEW_COMMENT_RESOURCE,
                actions::UPSERT,
                None,
                &AccessRequest::new().resource_property(pep_properties::OWNER_TENANT_ID, tenant_id),
            )
            .await?;

        let full_name = repo_full_name(owner, name)?;
        let repository = self
            .repo
            .find_by_full_name(&scope, &full_name)
            .await?
            .ok_or(DomainError::NotFound)?;

        let record = ReviewCommentRecord {
            repo_id: repository.id,
            ..record
        };
        self.review_comments.upsert(&scope, tenant_id, record).await
    }

    /// List mirrored reviews of one pull request, tenant-scoped, oldest
    /// first (by review id).
    ///
    /// # Errors
    /// `DomainError::NotFound` when the repository is not mirrored for this
    /// tenant; `Forbidden`/`Database`/`Internal` as usual.
    pub async fn list_reviews(
        &self,
        ctx: &SecurityContext,
        owner: &str,
        name: &str,
        pull_number: i64,
        window: PageWindow,
    ) -> Result<Page<Review>, DomainError> {
        let scope = self
            .policy_enforcer
            .access_scope_with(
                ctx,
                &REVIEW_RESOURCE,
                actions::LIST,
                None,
                &AccessRequest::new()
                    .resource_property(pep_properties::OWNER_TENANT_ID, ctx.subject_tenant_id()),
            )
            .await?;

        let full_name = repo_full_name(owner, name)?;
        let repository = self
            .repo
            .find_by_full_name(&scope, &full_name)
            .await?
            .ok_or(DomainError::NotFound)?;

        let items = self
            .reviews
            .list_by_pull(&scope, repository.id, pull_number, window)
            .await?;

        Ok(Page::new(
            items,
            PageInfo {
                next_cursor: None,
                prev_cursor: None,
                limit: window.limit(),
            },
        ))
    }

    /// Insert or update a mirrored review row for the caller's tenant.
    ///
    /// # Errors
    /// `DomainError::NotFound` when the repository is not mirrored;
    /// `Forbidden`/`Database`/`Internal` as usual.
    pub async fn upsert_review(
        &self,
        ctx: &SecurityContext,
        owner: &str,
        name: &str,
        record: ReviewRecord,
    ) -> Result<Review, DomainError> {
        let tenant_id = ctx.subject_tenant_id();

        let scope = self
            .policy_enforcer
            .access_scope_with(
                ctx,
                &REVIEW_RESOURCE,
                actions::UPSERT,
                None,
                &AccessRequest::new().resource_property(pep_properties::OWNER_TENANT_ID, tenant_id),
            )
            .await?;

        let full_name = repo_full_name(owner, name)?;
        let repository = self
            .repo
            .find_by_full_name(&scope, &full_name)
            .await?
            .ok_or(DomainError::NotFound)?;

        let record = ReviewRecord {
            repo_id: repository.id,
            ..record
        };
        self.reviews.upsert(&scope, tenant_id, record).await
    }

    /// List mirrored labels of one repository (`owner/name`), tenant-scoped,
    /// by name.
    ///
    /// # Errors
    /// `DomainError::NotFound` when the repository is not mirrored for this
    /// tenant; `Forbidden`/`Database`/`Internal` as usual.
    pub async fn list_labels(
        &self,
        ctx: &SecurityContext,
        owner: &str,
        name: &str,
        window: PageWindow,
    ) -> Result<Page<Label>, DomainError> {
        let scope = self
            .policy_enforcer
            .access_scope_with(
                ctx,
                &LABEL_RESOURCE,
                actions::LIST,
                None,
                &AccessRequest::new()
                    .resource_property(pep_properties::OWNER_TENANT_ID, ctx.subject_tenant_id()),
            )
            .await?;

        let full_name = repo_full_name(owner, name)?;
        let repository = self
            .repo
            .find_by_full_name(&scope, &full_name)
            .await?
            .ok_or(DomainError::NotFound)?;

        let items = self
            .labels
            .list_by_repo(&scope, repository.id, window)
            .await?;

        Ok(Page::new(
            items,
            PageInfo {
                next_cursor: None,
                prev_cursor: None,
                limit: window.limit(),
            },
        ))
    }

    /// Insert or update a mirrored label row for the caller's tenant.
    ///
    /// # Errors
    /// `DomainError::NotFound` when the repository is not mirrored;
    /// `Forbidden`/`Database`/`Internal` as usual.
    pub async fn upsert_label(
        &self,
        ctx: &SecurityContext,
        owner: &str,
        name: &str,
        record: LabelRecord,
    ) -> Result<Label, DomainError> {
        let tenant_id = ctx.subject_tenant_id();

        let scope = self
            .policy_enforcer
            .access_scope_with(
                ctx,
                &LABEL_RESOURCE,
                actions::UPSERT,
                None,
                &AccessRequest::new().resource_property(pep_properties::OWNER_TENANT_ID, tenant_id),
            )
            .await?;

        let full_name = repo_full_name(owner, name)?;
        let repository = self
            .repo
            .find_by_full_name(&scope, &full_name)
            .await?
            .ok_or(DomainError::NotFound)?;

        let record = LabelRecord {
            repo_id: repository.id,
            ..record
        };
        self.labels.upsert(&scope, tenant_id, record).await
    }

    /// List mirrored milestones of one repository (`owner/name`),
    /// tenant-scoped, by milestone number.
    ///
    /// # Errors
    /// `DomainError::NotFound` when the repository is not mirrored for this
    /// tenant; `Forbidden`/`Database`/`Internal` as usual.
    pub async fn list_milestones(
        &self,
        ctx: &SecurityContext,
        owner: &str,
        name: &str,
        window: PageWindow,
    ) -> Result<Page<Milestone>, DomainError> {
        let scope = self
            .policy_enforcer
            .access_scope_with(
                ctx,
                &MILESTONE_RESOURCE,
                actions::LIST,
                None,
                &AccessRequest::new()
                    .resource_property(pep_properties::OWNER_TENANT_ID, ctx.subject_tenant_id()),
            )
            .await?;

        let full_name = repo_full_name(owner, name)?;
        let repository = self
            .repo
            .find_by_full_name(&scope, &full_name)
            .await?
            .ok_or(DomainError::NotFound)?;

        let items = self
            .milestones
            .list_by_repo(&scope, repository.id, window)
            .await?;

        Ok(Page::new(
            items,
            PageInfo {
                next_cursor: None,
                prev_cursor: None,
                limit: window.limit(),
            },
        ))
    }

    /// Insert or update a mirrored milestone row for the caller's tenant.
    ///
    /// # Errors
    /// `DomainError::NotFound` when the repository is not mirrored;
    /// `Forbidden`/`Database`/`Internal` as usual.
    pub async fn upsert_milestone(
        &self,
        ctx: &SecurityContext,
        owner: &str,
        name: &str,
        record: MilestoneRecord,
    ) -> Result<Milestone, DomainError> {
        let tenant_id = ctx.subject_tenant_id();

        let scope = self
            .policy_enforcer
            .access_scope_with(
                ctx,
                &MILESTONE_RESOURCE,
                actions::UPSERT,
                None,
                &AccessRequest::new().resource_property(pep_properties::OWNER_TENANT_ID, tenant_id),
            )
            .await?;

        let full_name = repo_full_name(owner, name)?;
        let repository = self
            .repo
            .find_by_full_name(&scope, &full_name)
            .await?
            .ok_or(DomainError::NotFound)?;

        let record = MilestoneRecord {
            repo_id: repository.id,
            ..record
        };
        self.milestones.upsert(&scope, tenant_id, record).await
    }

    /// List mirrored releases of one repository (`owner/name`),
    /// tenant-scoped, newest first.
    ///
    /// # Errors
    /// `DomainError::NotFound` when the repository is not mirrored for this
    /// tenant; `Forbidden`/`Database`/`Internal` as usual.
    pub async fn list_releases(
        &self,
        ctx: &SecurityContext,
        owner: &str,
        name: &str,
        window: PageWindow,
    ) -> Result<Page<Release>, DomainError> {
        let scope = self
            .policy_enforcer
            .access_scope_with(
                ctx,
                &RELEASE_RESOURCE,
                actions::LIST,
                None,
                &AccessRequest::new()
                    .resource_property(pep_properties::OWNER_TENANT_ID, ctx.subject_tenant_id()),
            )
            .await?;

        let full_name = repo_full_name(owner, name)?;
        let repository = self
            .repo
            .find_by_full_name(&scope, &full_name)
            .await?
            .ok_or(DomainError::NotFound)?;

        let items = self
            .releases
            .list_by_repo(&scope, repository.id, window)
            .await?;

        Ok(Page::new(
            items,
            PageInfo {
                next_cursor: None,
                prev_cursor: None,
                limit: window.limit(),
            },
        ))
    }

    /// Insert or update a mirrored release row for the caller's tenant.
    ///
    /// # Errors
    /// `DomainError::NotFound` when the repository is not mirrored;
    /// `Forbidden`/`Database`/`Internal` as usual.
    pub async fn upsert_release(
        &self,
        ctx: &SecurityContext,
        owner: &str,
        name: &str,
        record: ReleaseRecord,
    ) -> Result<Release, DomainError> {
        let tenant_id = ctx.subject_tenant_id();

        let scope = self
            .policy_enforcer
            .access_scope_with(
                ctx,
                &RELEASE_RESOURCE,
                actions::UPSERT,
                None,
                &AccessRequest::new().resource_property(pep_properties::OWNER_TENANT_ID, tenant_id),
            )
            .await?;

        let full_name = repo_full_name(owner, name)?;
        let repository = self
            .repo
            .find_by_full_name(&scope, &full_name)
            .await?
            .ok_or(DomainError::NotFound)?;

        let record = ReleaseRecord {
            repo_id: repository.id,
            ..record
        };
        self.releases.upsert(&scope, tenant_id, record).await
    }

    /// List mirrored branch heads of one repository (`owner/name`),
    /// tenant-scoped, by name.
    ///
    /// # Errors
    /// `DomainError::NotFound` when the repository is not mirrored for this
    /// tenant; `Forbidden`/`Database`/`Internal` as usual.
    pub async fn list_branches(
        &self,
        ctx: &SecurityContext,
        owner: &str,
        name: &str,
        window: PageWindow,
    ) -> Result<Page<Branch>, DomainError> {
        let scope = self
            .policy_enforcer
            .access_scope_with(
                ctx,
                &BRANCH_RESOURCE,
                actions::LIST,
                None,
                &AccessRequest::new()
                    .resource_property(pep_properties::OWNER_TENANT_ID, ctx.subject_tenant_id()),
            )
            .await?;

        let full_name = repo_full_name(owner, name)?;
        let repository = self
            .repo
            .find_by_full_name(&scope, &full_name)
            .await?
            .ok_or(DomainError::NotFound)?;

        let items = self
            .branches
            .list_by_repo(&scope, repository.id, window)
            .await?;

        Ok(Page::new(
            items,
            PageInfo {
                next_cursor: None,
                prev_cursor: None,
                limit: window.limit(),
            },
        ))
    }

    /// Insert or update a mirrored branch-head row for the caller's tenant.
    ///
    /// # Errors
    /// `DomainError::NotFound` when the repository is not mirrored;
    /// `Forbidden`/`Database`/`Internal` as usual.
    pub async fn upsert_branch(
        &self,
        ctx: &SecurityContext,
        owner: &str,
        name: &str,
        record: BranchRecord,
    ) -> Result<Branch, DomainError> {
        let tenant_id = ctx.subject_tenant_id();

        let scope = self
            .policy_enforcer
            .access_scope_with(
                ctx,
                &BRANCH_RESOURCE,
                actions::UPSERT,
                None,
                &AccessRequest::new().resource_property(pep_properties::OWNER_TENANT_ID, tenant_id),
            )
            .await?;

        let full_name = repo_full_name(owner, name)?;
        let repository = self
            .repo
            .find_by_full_name(&scope, &full_name)
            .await?
            .ok_or(DomainError::NotFound)?;

        let record = BranchRecord {
            repo_id: repository.id,
            ..record
        };
        self.branches.upsert(&scope, tenant_id, record).await
    }

    /// List mirrored contributors of one repository (`owner/name`),
    /// tenant-scoped, most contributions first.
    ///
    /// # Errors
    /// `DomainError::NotFound` when the repository is not mirrored for this
    /// tenant; `Forbidden`/`Database`/`Internal` as usual.
    pub async fn list_contributors(
        &self,
        ctx: &SecurityContext,
        owner: &str,
        name: &str,
        window: PageWindow,
    ) -> Result<Page<Contributor>, DomainError> {
        let scope = self
            .policy_enforcer
            .access_scope_with(
                ctx,
                &CONTRIBUTOR_RESOURCE,
                actions::LIST,
                None,
                &AccessRequest::new()
                    .resource_property(pep_properties::OWNER_TENANT_ID, ctx.subject_tenant_id()),
            )
            .await?;

        let full_name = repo_full_name(owner, name)?;
        let repository = self
            .repo
            .find_by_full_name(&scope, &full_name)
            .await?
            .ok_or(DomainError::NotFound)?;

        let items = self
            .contributors
            .list_by_repo(&scope, repository.id, window)
            .await?;

        Ok(Page::new(
            items,
            PageInfo {
                next_cursor: None,
                prev_cursor: None,
                limit: window.limit(),
            },
        ))
    }

    /// Insert or update a mirrored contributor row for the caller's tenant.
    ///
    /// # Errors
    /// `DomainError::NotFound` when the repository is not mirrored;
    /// `Forbidden`/`Database`/`Internal` as usual.
    pub async fn upsert_contributor(
        &self,
        ctx: &SecurityContext,
        owner: &str,
        name: &str,
        record: ContributorRecord,
    ) -> Result<Contributor, DomainError> {
        let tenant_id = ctx.subject_tenant_id();

        let scope = self
            .policy_enforcer
            .access_scope_with(
                ctx,
                &CONTRIBUTOR_RESOURCE,
                actions::UPSERT,
                None,
                &AccessRequest::new().resource_property(pep_properties::OWNER_TENANT_ID, tenant_id),
            )
            .await?;

        let full_name = repo_full_name(owner, name)?;
        let repository = self
            .repo
            .find_by_full_name(&scope, &full_name)
            .await?
            .ok_or(DomainError::NotFound)?;

        let record = ContributorRecord {
            repo_id: repository.id,
            ..record
        };
        self.contributors.upsert(&scope, tenant_id, record).await
    }

    /// # Errors
    /// `NotFound` when the repository is not mirrored; `Forbidden`/`Database`
    /// as usual.
    pub async fn list_workflow_runs(
        &self,
        ctx: &SecurityContext,
        owner: &str,
        name: &str,
        window: PageWindow,
    ) -> Result<(Page<WorkflowRun>, u64), DomainError> {
        let scope = self
            .policy_enforcer
            .access_scope_with(
                ctx,
                &WORKFLOW_RUN_RESOURCE,
                actions::LIST,
                None,
                &AccessRequest::new()
                    .resource_property(pep_properties::OWNER_TENANT_ID, ctx.subject_tenant_id()),
            )
            .await?;

        let full_name = repo_full_name(owner, name)?;
        let repository = self
            .repo
            .find_by_full_name(&scope, &full_name)
            .await?
            .ok_or(DomainError::NotFound)?;

        // Both statements run on one transaction, and on the scope and
        // repository already resolved above: the total describes the page it
        // is returned with, and one policy evaluation covers both.
        let (items, total) = self
            .workflow_runs
            .page_by_repo(&scope, repository.id, window)
            .await?;

        Ok((
            Page::new(
                items,
                PageInfo {
                    next_cursor: None,
                    prev_cursor: None,
                    limit: window.limit(),
                },
            ),
            total,
        ))
    }

    /// Insert or update a mirrored workflow-run row for the caller's tenant.
    ///
    /// # Errors
    /// `DomainError::NotFound` when the repository is not mirrored;
    /// `Forbidden`/`Database`/`Internal` as usual.
    pub async fn upsert_workflow_run(
        &self,
        ctx: &SecurityContext,
        owner: &str,
        name: &str,
        record: WorkflowRunRecord,
    ) -> Result<WorkflowRun, DomainError> {
        let tenant_id = ctx.subject_tenant_id();

        let scope = self
            .policy_enforcer
            .access_scope_with(
                ctx,
                &WORKFLOW_RUN_RESOURCE,
                actions::UPSERT,
                None,
                &AccessRequest::new().resource_property(pep_properties::OWNER_TENANT_ID, tenant_id),
            )
            .await?;

        let full_name = repo_full_name(owner, name)?;
        let repository = self
            .repo
            .find_by_full_name(&scope, &full_name)
            .await?
            .ok_or(DomainError::NotFound)?;

        let record = WorkflowRunRecord {
            repo_id: repository.id,
            ..record
        };
        self.workflow_runs.upsert(&scope, tenant_id, record).await
    }

    /// List mirrored changed files of one pull request, tenant-scoped, by
    /// file name.
    ///
    /// # Errors
    /// `DomainError::NotFound` when the repository is not mirrored for this
    /// tenant; `Forbidden`/`Database`/`Internal` as usual.
    pub async fn list_pull_request_files(
        &self,
        ctx: &SecurityContext,
        owner: &str,
        name: &str,
        pull_number: i64,
        window: PageWindow,
    ) -> Result<Page<PullRequestFile>, DomainError> {
        let scope = self
            .policy_enforcer
            .access_scope_with(
                ctx,
                &PULL_REQUEST_FILE_RESOURCE,
                actions::LIST,
                None,
                &AccessRequest::new()
                    .resource_property(pep_properties::OWNER_TENANT_ID, ctx.subject_tenant_id()),
            )
            .await?;

        let full_name = repo_full_name(owner, name)?;
        let repository = self
            .repo
            .find_by_full_name(&scope, &full_name)
            .await?
            .ok_or(DomainError::NotFound)?;

        let items = self
            .pull_request_files
            .list_by_pull(&scope, repository.id, pull_number, window)
            .await?;

        Ok(Page::new(
            items,
            PageInfo {
                next_cursor: None,
                prev_cursor: None,
                limit: window.limit(),
            },
        ))
    }

    /// Insert or update a mirrored pull-request-file row for the caller's
    /// tenant.
    ///
    /// # Errors
    /// `DomainError::NotFound` when the repository is not mirrored;
    /// `Forbidden`/`Database`/`Internal` as usual.
    pub async fn upsert_pull_request_file(
        &self,
        ctx: &SecurityContext,
        owner: &str,
        name: &str,
        record: PullRequestFileRecord,
    ) -> Result<PullRequestFile, DomainError> {
        let tenant_id = ctx.subject_tenant_id();

        let scope = self
            .policy_enforcer
            .access_scope_with(
                ctx,
                &PULL_REQUEST_FILE_RESOURCE,
                actions::UPSERT,
                None,
                &AccessRequest::new().resource_property(pep_properties::OWNER_TENANT_ID, tenant_id),
            )
            .await?;

        let full_name = repo_full_name(owner, name)?;
        let repository = self
            .repo
            .find_by_full_name(&scope, &full_name)
            .await?
            .ok_or(DomainError::NotFound)?;

        let record = PullRequestFileRecord {
            repo_id: repository.id,
            ..record
        };
        self.pull_request_files
            .upsert(&scope, tenant_id, record)
            .await
    }

    /// List mirrored tags of one repository (`owner/name`), tenant-scoped,
    /// by name.
    ///
    /// # Errors
    /// `DomainError::NotFound` when the repository is not mirrored for this
    /// tenant; `Forbidden`/`Database`/`Internal` as usual.
    pub async fn list_tags(
        &self,
        ctx: &SecurityContext,
        owner: &str,
        name: &str,
        window: PageWindow,
    ) -> Result<Page<Tag>, DomainError> {
        let scope = self
            .policy_enforcer
            .access_scope_with(
                ctx,
                &TAG_RESOURCE,
                actions::LIST,
                None,
                &AccessRequest::new()
                    .resource_property(pep_properties::OWNER_TENANT_ID, ctx.subject_tenant_id()),
            )
            .await?;

        let full_name = repo_full_name(owner, name)?;
        let repository = self
            .repo
            .find_by_full_name(&scope, &full_name)
            .await?
            .ok_or(DomainError::NotFound)?;

        let items = self
            .tags
            .list_by_repo(&scope, repository.id, window)
            .await?;

        Ok(Page::new(
            items,
            PageInfo {
                next_cursor: None,
                prev_cursor: None,
                limit: window.limit(),
            },
        ))
    }

    /// Insert or update a mirrored tag row for the caller's tenant.
    ///
    /// # Errors
    /// `DomainError::NotFound` when the repository is not mirrored;
    /// `Forbidden`/`Database`/`Internal` as usual.
    pub async fn upsert_tag(
        &self,
        ctx: &SecurityContext,
        owner: &str,
        name: &str,
        record: TagRecord,
    ) -> Result<Tag, DomainError> {
        let tenant_id = ctx.subject_tenant_id();

        let scope = self
            .policy_enforcer
            .access_scope_with(
                ctx,
                &TAG_RESOURCE,
                actions::UPSERT,
                None,
                &AccessRequest::new().resource_property(pep_properties::OWNER_TENANT_ID, tenant_id),
            )
            .await?;

        let full_name = repo_full_name(owner, name)?;
        let repository = self
            .repo
            .find_by_full_name(&scope, &full_name)
            .await?
            .ok_or(DomainError::NotFound)?;

        let record = TagRecord {
            repo_id: repository.id,
            ..record
        };
        self.tags.upsert(&scope, tenant_id, record).await
    }

    /// List mirrored changed files of one commit, tenant-scoped, by file
    /// name.
    ///
    /// # Errors
    /// `DomainError::NotFound` when the repository is not mirrored for this
    /// tenant; `Forbidden`/`Database`/`Internal` as usual.
    pub async fn list_commit_files(
        &self,
        ctx: &SecurityContext,
        owner: &str,
        name: &str,
        commit_sha: &str,
        query: &ODataQuery,
    ) -> Result<Page<CommitFile>, DomainError> {
        validate_commit_sha(commit_sha)?;
        let scope = self
            .policy_enforcer
            .access_scope_with(
                ctx,
                &COMMIT_FILE_RESOURCE,
                actions::LIST,
                None,
                &AccessRequest::new()
                    .resource_property(pep_properties::OWNER_TENANT_ID, ctx.subject_tenant_id()),
            )
            .await?;

        let full_name = repo_full_name(owner, name)?;
        let repository = self
            .repo
            .find_by_full_name(&scope, &full_name)
            .await?
            .ok_or(DomainError::NotFound)?;

        self.commit_files
            .list_by_commit(&scope, repository.id, commit_sha, query)
            .await
    }

    /// Insert or update a mirrored commit-file row for the caller's tenant.
    ///
    /// # Errors
    /// `DomainError::NotFound` when the repository is not mirrored;
    /// `Forbidden`/`Database`/`Internal` as usual.
    pub async fn upsert_commit_file(
        &self,
        ctx: &SecurityContext,
        owner: &str,
        name: &str,
        record: CommitFileRecord,
    ) -> Result<CommitFile, DomainError> {
        let tenant_id = ctx.subject_tenant_id();

        let scope = self
            .policy_enforcer
            .access_scope_with(
                ctx,
                &COMMIT_FILE_RESOURCE,
                actions::UPSERT,
                None,
                &AccessRequest::new().resource_property(pep_properties::OWNER_TENANT_ID, tenant_id),
            )
            .await?;

        let full_name = repo_full_name(owner, name)?;
        let repository = self
            .repo
            .find_by_full_name(&scope, &full_name)
            .await?
            .ok_or(DomainError::NotFound)?;

        let record = CommitFileRecord {
            repo_id: repository.id,
            ..record
        };
        self.commit_files.upsert(&scope, tenant_id, record).await
    }

    /// List mirrored review threads of one pull request, tenant-scoped, by
    /// thread id.
    ///
    /// # Errors
    /// `DomainError::NotFound` when the repository is not mirrored for this
    /// tenant; `Forbidden`/`Database`/`Internal` as usual.
    pub async fn list_review_threads(
        &self,
        ctx: &SecurityContext,
        owner: &str,
        name: &str,
        pull_number: i64,
        query: &ODataQuery,
    ) -> Result<Page<ReviewThread>, DomainError> {
        let scope = self
            .policy_enforcer
            .access_scope_with(
                ctx,
                &REVIEW_THREAD_RESOURCE,
                actions::LIST,
                None,
                &AccessRequest::new()
                    .resource_property(pep_properties::OWNER_TENANT_ID, ctx.subject_tenant_id()),
            )
            .await?;

        let full_name = repo_full_name(owner, name)?;
        let repository = self
            .repo
            .find_by_full_name(&scope, &full_name)
            .await?
            .ok_or(DomainError::NotFound)?;

        self.review_threads
            .list_by_pull(&scope, repository.id, pull_number, query)
            .await
    }

    /// Insert or update a mirrored review-thread row for the caller's tenant.
    ///
    /// # Errors
    /// `DomainError::NotFound` when the repository is not mirrored;
    /// `Forbidden`/`Database`/`Internal` as usual.
    pub async fn upsert_review_thread(
        &self,
        ctx: &SecurityContext,
        owner: &str,
        name: &str,
        record: ReviewThreadRecord,
    ) -> Result<ReviewThread, DomainError> {
        let tenant_id = ctx.subject_tenant_id();

        let scope = self
            .policy_enforcer
            .access_scope_with(
                ctx,
                &REVIEW_THREAD_RESOURCE,
                actions::UPSERT,
                None,
                &AccessRequest::new().resource_property(pep_properties::OWNER_TENANT_ID, tenant_id),
            )
            .await?;

        let full_name = repo_full_name(owner, name)?;
        let repository = self
            .repo
            .find_by_full_name(&scope, &full_name)
            .await?
            .ok_or(DomainError::NotFound)?;

        let record = ReviewThreadRecord {
            repo_id: repository.id,
            ..record
        };
        self.review_threads.upsert(&scope, tenant_id, record).await
    }

    /// List mirrored comments of one commit, tenant-scoped, oldest first.
    ///
    /// # Errors
    /// `DomainError::NotFound` when the repository is not mirrored for this
    /// tenant; `Forbidden`/`Database`/`Internal` as usual.
    pub async fn list_commit_comments(
        &self,
        ctx: &SecurityContext,
        owner: &str,
        name: &str,
        commit_sha: &str,
        window: PageWindow,
    ) -> Result<Page<CommitComment>, DomainError> {
        validate_commit_sha(commit_sha)?;
        let scope = self
            .policy_enforcer
            .access_scope_with(
                ctx,
                &COMMIT_COMMENT_RESOURCE,
                actions::LIST,
                None,
                &AccessRequest::new()
                    .resource_property(pep_properties::OWNER_TENANT_ID, ctx.subject_tenant_id()),
            )
            .await?;

        let full_name = repo_full_name(owner, name)?;
        let repository = self
            .repo
            .find_by_full_name(&scope, &full_name)
            .await?
            .ok_or(DomainError::NotFound)?;

        let items = self
            .commit_comments
            .list_by_commit(&scope, repository.id, commit_sha, window)
            .await?;

        Ok(Page::new(
            items,
            PageInfo {
                next_cursor: None,
                prev_cursor: None,
                limit: window.limit(),
            },
        ))
    }

    /// Insert or update a mirrored commit-comment row for the caller's
    /// tenant.
    ///
    /// # Errors
    /// `DomainError::NotFound` when the repository is not mirrored;
    /// `Forbidden`/`Database`/`Internal` as usual.
    pub async fn upsert_commit_comment(
        &self,
        ctx: &SecurityContext,
        owner: &str,
        name: &str,
        record: CommitCommentRecord,
    ) -> Result<CommitComment, DomainError> {
        let tenant_id = ctx.subject_tenant_id();

        let scope = self
            .policy_enforcer
            .access_scope_with(
                ctx,
                &COMMIT_COMMENT_RESOURCE,
                actions::UPSERT,
                None,
                &AccessRequest::new().resource_property(pep_properties::OWNER_TENANT_ID, tenant_id),
            )
            .await?;

        let full_name = repo_full_name(owner, name)?;
        let repository = self
            .repo
            .find_by_full_name(&scope, &full_name)
            .await?
            .ok_or(DomainError::NotFound)?;

        let record = CommitCommentRecord {
            repo_id: repository.id,
            ..record
        };
        self.commit_comments.upsert(&scope, tenant_id, record).await
    }

    /// List mirrored events of one issue, tenant-scoped, oldest first.
    ///
    /// # Errors
    /// `DomainError::NotFound` when the repository is not mirrored for this
    /// tenant; `Forbidden`/`Database`/`Internal` as usual.
    pub async fn list_issue_events(
        &self,
        ctx: &SecurityContext,
        owner: &str,
        name: &str,
        issue_number: i64,
        window: PageWindow,
    ) -> Result<Page<IssueEvent>, DomainError> {
        let scope = self
            .policy_enforcer
            .access_scope_with(
                ctx,
                &ISSUE_EVENT_RESOURCE,
                actions::LIST,
                None,
                &AccessRequest::new()
                    .resource_property(pep_properties::OWNER_TENANT_ID, ctx.subject_tenant_id()),
            )
            .await?;

        let full_name = repo_full_name(owner, name)?;
        let repository = self
            .repo
            .find_by_full_name(&scope, &full_name)
            .await?
            .ok_or(DomainError::NotFound)?;

        let items = self
            .issue_events
            .list_by_issue(&scope, repository.id, issue_number, window)
            .await?;

        Ok(Page::new(
            items,
            PageInfo {
                next_cursor: None,
                prev_cursor: None,
                limit: window.limit(),
            },
        ))
    }

    /// Insert or update a mirrored issue-event row for the caller's tenant.
    ///
    /// # Errors
    /// `DomainError::NotFound` when the repository is not mirrored;
    /// `Forbidden`/`Database`/`Internal` as usual.
    pub async fn upsert_issue_event(
        &self,
        ctx: &SecurityContext,
        owner: &str,
        name: &str,
        record: IssueEventRecord,
    ) -> Result<IssueEvent, DomainError> {
        let tenant_id = ctx.subject_tenant_id();

        let scope = self
            .policy_enforcer
            .access_scope_with(
                ctx,
                &ISSUE_EVENT_RESOURCE,
                actions::UPSERT,
                None,
                &AccessRequest::new().resource_property(pep_properties::OWNER_TENANT_ID, tenant_id),
            )
            .await?;

        let full_name = repo_full_name(owner, name)?;
        let repository = self
            .repo
            .find_by_full_name(&scope, &full_name)
            .await?
            .ok_or(DomainError::NotFound)?;

        let record = IssueEventRecord {
            repo_id: repository.id,
            ..record
        };
        self.issue_events.upsert(&scope, tenant_id, record).await
    }

    /// List mirrored deployments of one repository (`owner/name`),
    /// tenant-scoped, newest first.
    ///
    /// # Errors
    /// `DomainError::NotFound` when the repository is not mirrored for this
    /// tenant; `Forbidden`/`Database`/`Internal` as usual.
    pub async fn list_deployments(
        &self,
        ctx: &SecurityContext,
        owner: &str,
        name: &str,
        window: PageWindow,
    ) -> Result<Page<Deployment>, DomainError> {
        let scope = self
            .policy_enforcer
            .access_scope_with(
                ctx,
                &DEPLOYMENT_RESOURCE,
                actions::LIST,
                None,
                &AccessRequest::new()
                    .resource_property(pep_properties::OWNER_TENANT_ID, ctx.subject_tenant_id()),
            )
            .await?;

        let full_name = repo_full_name(owner, name)?;
        let repository = self
            .repo
            .find_by_full_name(&scope, &full_name)
            .await?
            .ok_or(DomainError::NotFound)?;

        let items = self
            .deployments
            .list_by_repo(&scope, repository.id, window)
            .await?;

        Ok(Page::new(
            items,
            PageInfo {
                next_cursor: None,
                prev_cursor: None,
                limit: window.limit(),
            },
        ))
    }

    /// Insert or update a mirrored deployment row for the caller's tenant.
    ///
    /// # Errors
    /// `DomainError::NotFound` when the repository is not mirrored;
    /// `Forbidden`/`Database`/`Internal` as usual.
    pub async fn upsert_deployment(
        &self,
        ctx: &SecurityContext,
        owner: &str,
        name: &str,
        record: DeploymentRecord,
    ) -> Result<Deployment, DomainError> {
        let tenant_id = ctx.subject_tenant_id();

        let scope = self
            .policy_enforcer
            .access_scope_with(
                ctx,
                &DEPLOYMENT_RESOURCE,
                actions::UPSERT,
                None,
                &AccessRequest::new().resource_property(pep_properties::OWNER_TENANT_ID, tenant_id),
            )
            .await?;

        let full_name = repo_full_name(owner, name)?;
        let repository = self
            .repo
            .find_by_full_name(&scope, &full_name)
            .await?
            .ok_or(DomainError::NotFound)?;

        let record = DeploymentRecord {
            repo_id: repository.id,
            ..record
        };
        self.deployments.upsert(&scope, tenant_id, record).await
    }

    /// List the mirrored commits of one pull request, tenant-scoped,
    /// oldest first.
    ///
    /// # Errors
    /// `DomainError::NotFound` when the repository is not mirrored for this
    /// tenant; `Forbidden`/`Database`/`Internal` as usual.
    pub async fn list_pull_request_commits(
        &self,
        ctx: &SecurityContext,
        owner: &str,
        name: &str,
        pull_number: i64,
        window: PageWindow,
    ) -> Result<Page<PullRequestCommit>, DomainError> {
        let scope = self
            .policy_enforcer
            .access_scope_with(
                ctx,
                &PULL_REQUEST_COMMIT_RESOURCE,
                actions::LIST,
                None,
                &AccessRequest::new()
                    .resource_property(pep_properties::OWNER_TENANT_ID, ctx.subject_tenant_id()),
            )
            .await?;

        let full_name = repo_full_name(owner, name)?;
        let repository = self
            .repo
            .find_by_full_name(&scope, &full_name)
            .await?
            .ok_or(DomainError::NotFound)?;

        let items = self
            .pull_request_commits
            .list_by_pull(&scope, repository.id, pull_number, window)
            .await?;

        Ok(Page::new(
            items,
            PageInfo {
                next_cursor: None,
                prev_cursor: None,
                limit: window.limit(),
            },
        ))
    }

    /// Insert or update a mirrored pull-request-commit row for the caller's
    /// tenant.
    ///
    /// # Errors
    /// `DomainError::NotFound` when the repository is not mirrored;
    /// `Forbidden`/`Database`/`Internal` as usual.
    pub async fn upsert_pull_request_commit(
        &self,
        ctx: &SecurityContext,
        owner: &str,
        name: &str,
        record: PullRequestCommitRecord,
    ) -> Result<PullRequestCommit, DomainError> {
        let tenant_id = ctx.subject_tenant_id();

        let scope = self
            .policy_enforcer
            .access_scope_with(
                ctx,
                &PULL_REQUEST_COMMIT_RESOURCE,
                actions::UPSERT,
                None,
                &AccessRequest::new().resource_property(pep_properties::OWNER_TENANT_ID, tenant_id),
            )
            .await?;

        let full_name = repo_full_name(owner, name)?;
        let repository = self
            .repo
            .find_by_full_name(&scope, &full_name)
            .await?
            .ok_or(DomainError::NotFound)?;

        let record = PullRequestCommitRecord {
            repo_id: repository.id,
            ..record
        };
        self.pull_request_commits
            .upsert(&scope, tenant_id, record)
            .await
    }

    /// List mirrored statuses of one commit, tenant-scoped, newest first.
    ///
    /// # Errors
    /// `DomainError::NotFound` when the repository is not mirrored for this
    /// tenant; `Forbidden`/`Database`/`Internal` as usual.
    pub async fn list_commit_statuses(
        &self,
        ctx: &SecurityContext,
        owner: &str,
        name: &str,
        commit_sha: &str,
        window: PageWindow,
    ) -> Result<Page<CommitStatus>, DomainError> {
        validate_commit_sha(commit_sha)?;
        let scope = self
            .policy_enforcer
            .access_scope_with(
                ctx,
                &COMMIT_STATUS_RESOURCE,
                actions::LIST,
                None,
                &AccessRequest::new()
                    .resource_property(pep_properties::OWNER_TENANT_ID, ctx.subject_tenant_id()),
            )
            .await?;

        let full_name = repo_full_name(owner, name)?;
        let repository = self
            .repo
            .find_by_full_name(&scope, &full_name)
            .await?
            .ok_or(DomainError::NotFound)?;

        let items = self
            .commit_statuses
            .list_by_commit(&scope, repository.id, commit_sha, window)
            .await?;

        Ok(Page::new(
            items,
            PageInfo {
                next_cursor: None,
                prev_cursor: None,
                limit: window.limit(),
            },
        ))
    }

    /// Insert or update a mirrored commit-status row for the caller's
    /// tenant.
    ///
    /// # Errors
    /// `DomainError::NotFound` when the repository is not mirrored;
    /// `Forbidden`/`Database`/`Internal` as usual.
    pub async fn upsert_commit_status(
        &self,
        ctx: &SecurityContext,
        owner: &str,
        name: &str,
        record: CommitStatusRecord,
    ) -> Result<CommitStatus, DomainError> {
        let tenant_id = ctx.subject_tenant_id();

        let scope = self
            .policy_enforcer
            .access_scope_with(
                ctx,
                &COMMIT_STATUS_RESOURCE,
                actions::UPSERT,
                None,
                &AccessRequest::new().resource_property(pep_properties::OWNER_TENANT_ID, tenant_id),
            )
            .await?;

        let full_name = repo_full_name(owner, name)?;
        let repository = self
            .repo
            .find_by_full_name(&scope, &full_name)
            .await?
            .ok_or(DomainError::NotFound)?;

        let record = CommitStatusRecord {
            repo_id: repository.id,
            ..record
        };
        self.commit_statuses.upsert(&scope, tenant_id, record).await
    }

    /// List mirrored jobs of one workflow run, tenant-scoped, by job id.
    ///
    /// # Errors
    /// `DomainError::NotFound` when the repository is not mirrored for this
    /// tenant; `Forbidden`/`Database`/`Internal` as usual.
    pub async fn list_workflow_jobs(
        &self,
        ctx: &SecurityContext,
        owner: &str,
        name: &str,
        run_id: i64,
        window: PageWindow,
    ) -> Result<(Page<WorkflowJob>, u64), DomainError> {
        let scope = self
            .policy_enforcer
            .access_scope_with(
                ctx,
                &WORKFLOW_JOB_RESOURCE,
                actions::LIST,
                None,
                &AccessRequest::new()
                    .resource_property(pep_properties::OWNER_TENANT_ID, ctx.subject_tenant_id()),
            )
            .await?;

        let full_name = repo_full_name(owner, name)?;
        let repository = self
            .repo
            .find_by_full_name(&scope, &full_name)
            .await?
            .ok_or(DomainError::NotFound)?;

        // Both statements run on one transaction, and on the scope and
        // repository already resolved above.
        let (items, total) = self
            .workflow_jobs
            .page_by_run(&scope, repository.id, run_id, window)
            .await?;

        Ok((
            Page::new(
                items,
                PageInfo {
                    next_cursor: None,
                    prev_cursor: None,
                    limit: window.limit(),
                },
            ),
            total,
        ))
    }

    /// Insert or update a mirrored workflow-job row for the caller's tenant.
    ///
    /// # Errors
    /// `DomainError::NotFound` when the repository is not mirrored;
    /// `Forbidden`/`Database`/`Internal` as usual.
    pub async fn upsert_workflow_job(
        &self,
        ctx: &SecurityContext,
        owner: &str,
        name: &str,
        record: WorkflowJobRecord,
    ) -> Result<WorkflowJob, DomainError> {
        let tenant_id = ctx.subject_tenant_id();

        let scope = self
            .policy_enforcer
            .access_scope_with(
                ctx,
                &WORKFLOW_JOB_RESOURCE,
                actions::UPSERT,
                None,
                &AccessRequest::new().resource_property(pep_properties::OWNER_TENANT_ID, tenant_id),
            )
            .await?;

        let full_name = repo_full_name(owner, name)?;
        let repository = self
            .repo
            .find_by_full_name(&scope, &full_name)
            .await?
            .ok_or(DomainError::NotFound)?;

        let record = WorkflowJobRecord {
            repo_id: repository.id,
            ..record
        };
        self.workflow_jobs.upsert(&scope, tenant_id, record).await
    }

    /// List mirrored reactions of one issue or pull request, tenant-scoped.
    ///
    /// # Errors
    /// `DomainError::NotFound` when the repository is not mirrored for this
    /// tenant; `Forbidden`/`Database`/`Internal` as usual.
    pub async fn list_issue_reactions(
        &self,
        ctx: &SecurityContext,
        owner: &str,
        name: &str,
        issue_number: i64,
        window: PageWindow,
    ) -> Result<Page<IssueReaction>, DomainError> {
        let scope = self
            .policy_enforcer
            .access_scope_with(
                ctx,
                &ISSUE_REACTION_RESOURCE,
                actions::LIST,
                None,
                &AccessRequest::new()
                    .resource_property(pep_properties::OWNER_TENANT_ID, ctx.subject_tenant_id()),
            )
            .await?;

        let full_name = repo_full_name(owner, name)?;
        let repository = self
            .repo
            .find_by_full_name(&scope, &full_name)
            .await?
            .ok_or(DomainError::NotFound)?;

        let items = self
            .issue_reactions
            .list_by_issue(&scope, repository.id, issue_number, window)
            .await?;

        Ok(Page::new(
            items,
            PageInfo {
                next_cursor: None,
                prev_cursor: None,
                limit: window.limit(),
            },
        ))
    }

    /// Insert or update a mirrored issue-reaction row for the caller's tenant.
    ///
    /// # Errors
    /// `DomainError::NotFound` when the repository is not mirrored;
    /// `Forbidden`/`Database`/`Internal` as usual.
    pub async fn upsert_issue_reaction(
        &self,
        ctx: &SecurityContext,
        owner: &str,
        name: &str,
        record: IssueReactionRecord,
    ) -> Result<IssueReaction, DomainError> {
        let tenant_id = ctx.subject_tenant_id();

        let scope = self
            .policy_enforcer
            .access_scope_with(
                ctx,
                &ISSUE_REACTION_RESOURCE,
                actions::UPSERT,
                None,
                &AccessRequest::new().resource_property(pep_properties::OWNER_TENANT_ID, tenant_id),
            )
            .await?;

        let full_name = repo_full_name(owner, name)?;
        let repository = self
            .repo
            .find_by_full_name(&scope, &full_name)
            .await?
            .ok_or(DomainError::NotFound)?;

        let record = IssueReactionRecord {
            repo_id: repository.id,
            ..record
        };
        self.issue_reactions.upsert(&scope, tenant_id, record).await
    }

    /// List mirrored check runs of one commit, tenant-scoped, by check-run id.
    ///
    /// # Errors
    /// `DomainError::NotFound` when the repository is not mirrored for this
    /// tenant; `Forbidden`/`Database`/`Internal` as usual.
    pub async fn list_check_runs(
        &self,
        ctx: &SecurityContext,
        owner: &str,
        name: &str,
        head_sha: &str,
        window: PageWindow,
    ) -> Result<(Page<CheckRun>, u64), DomainError> {
        validate_commit_sha(head_sha)?;
        let scope = self
            .policy_enforcer
            .access_scope_with(
                ctx,
                &CHECK_RUN_RESOURCE,
                actions::LIST,
                None,
                &AccessRequest::new()
                    .resource_property(pep_properties::OWNER_TENANT_ID, ctx.subject_tenant_id()),
            )
            .await?;

        let full_name = repo_full_name(owner, name)?;
        let repository = self
            .repo
            .find_by_full_name(&scope, &full_name)
            .await?
            .ok_or(DomainError::NotFound)?;

        // Both statements run on one transaction, and on the scope and
        // repository already resolved above.
        let (items, total) = self
            .check_runs
            .page_by_commit(&scope, repository.id, head_sha, window)
            .await?;

        Ok((
            Page::new(
                items,
                PageInfo {
                    next_cursor: None,
                    prev_cursor: None,
                    limit: window.limit(),
                },
            ),
            total,
        ))
    }

    /// Insert or update a mirrored check-run row for the caller's tenant.
    ///
    /// # Errors
    /// `DomainError::NotFound` when the repository is not mirrored;
    /// `Forbidden`/`Database`/`Internal` as usual.
    pub async fn upsert_check_run(
        &self,
        ctx: &SecurityContext,
        owner: &str,
        name: &str,
        record: CheckRunRecord,
    ) -> Result<CheckRun, DomainError> {
        let tenant_id = ctx.subject_tenant_id();

        let scope = self
            .policy_enforcer
            .access_scope_with(
                ctx,
                &CHECK_RUN_RESOURCE,
                actions::UPSERT,
                None,
                &AccessRequest::new().resource_property(pep_properties::OWNER_TENANT_ID, tenant_id),
            )
            .await?;

        let full_name = repo_full_name(owner, name)?;
        let repository = self
            .repo
            .find_by_full_name(&scope, &full_name)
            .await?
            .ok_or(DomainError::NotFound)?;

        let record = CheckRunRecord {
            repo_id: repository.id,
            ..record
        };
        self.check_runs.upsert(&scope, tenant_id, record).await
    }

    /// List the mirrored timeline of one issue or pull request, in the order
    /// GitHub served it, tenant-scoped.
    ///
    /// # Errors
    /// `DomainError::NotFound` when the repository is not mirrored for this
    /// tenant; `Forbidden`/`Database`/`Internal` as usual.
    pub async fn list_issue_timeline(
        &self,
        ctx: &SecurityContext,
        owner: &str,
        name: &str,
        issue_number: i64,
        window: PageWindow,
    ) -> Result<Page<IssueTimelineEvent>, DomainError> {
        let scope = self
            .policy_enforcer
            .access_scope_with(
                ctx,
                &ISSUE_TIMELINE_RESOURCE,
                actions::LIST,
                None,
                &AccessRequest::new()
                    .resource_property(pep_properties::OWNER_TENANT_ID, ctx.subject_tenant_id()),
            )
            .await?;

        let full_name = repo_full_name(owner, name)?;
        let repository = self
            .repo
            .find_by_full_name(&scope, &full_name)
            .await?
            .ok_or(DomainError::NotFound)?;

        let items = self
            .issue_timeline
            .list_by_issue(&scope, repository.id, issue_number, window)
            .await?;

        Ok(Page::new(
            items,
            PageInfo {
                next_cursor: None,
                prev_cursor: None,
                limit: window.limit(),
            },
        ))
    }

    /// Insert or update one mirrored timeline entry for the caller's tenant.
    ///
    /// # Errors
    /// `DomainError::NotFound` when the repository is not mirrored;
    /// `Forbidden`/`Database`/`Internal` as usual.
    pub async fn upsert_issue_timeline_event(
        &self,
        ctx: &SecurityContext,
        owner: &str,
        name: &str,
        record: IssueTimelineEventRecord,
    ) -> Result<IssueTimelineEvent, DomainError> {
        let tenant_id = ctx.subject_tenant_id();

        let scope = self
            .policy_enforcer
            .access_scope_with(
                ctx,
                &ISSUE_TIMELINE_RESOURCE,
                actions::UPSERT,
                None,
                &AccessRequest::new().resource_property(pep_properties::OWNER_TENANT_ID, tenant_id),
            )
            .await?;

        let full_name = repo_full_name(owner, name)?;
        let repository = self
            .repo
            .find_by_full_name(&scope, &full_name)
            .await?
            .ok_or(DomainError::NotFound)?;

        let record = IssueTimelineEventRecord {
            repo_id: repository.id,
            ..record
        };
        self.issue_timeline.upsert(&scope, tenant_id, record).await
    }

    /// Run one sync of `owner/name` and record it as a session: a durable
    /// `gm_sync_sessions` row that survives the request and is readable via
    /// [`Self::get_session`]. The sync itself still runs inline — slice 3 of
    /// gears-rust#4632 moves it to a background task and this method starts
    /// answering before the work finishes.
    ///
    /// # Errors
    /// Whatever [`Self::sync_repository`] returns; the session row records
    /// the failure before the error propagates.
    /// Drop cached GitHub responses for one owner or one repository.
    ///
    /// DESIGN §4's `clear_cache(session, scope)`. The mirrored rows are left
    /// alone: this only discards the raw responses, so the next sync re-fetches
    /// rather than revalidating.
    ///
    /// # Errors
    /// `Forbidden`/`Database` as usual.
    pub async fn clear_cache(
        &self,
        ctx: &SecurityContext,
        owner: &str,
        name: Option<&str>,
    ) -> Result<u64, DomainError> {
        // Clearing a tenant's own cache is a sync-scoped action: it changes
        // nothing anyone can read, only what the next sync will re-fetch.
        let tenant_id = ctx.subject_tenant_id();
        self.policy_enforcer
            .access_scope_with(
                ctx,
                &SYNC_RESOURCE,
                actions::SYNC,
                None,
                &AccessRequest::new().resource_property(pep_properties::OWNER_TENANT_ID, tenant_id),
            )
            .await?;

        let removed = self.github.clear_cache(tenant_id, owner, name).await?;
        tracing::info!(
            owner,
            repository = name,
            removed,
            "cleared cached responses"
        );
        Ok(removed)
    }

    /// What a sync collects when the request does not narrow it.
    #[must_use]
    pub fn default_scope(&self) -> ScopeConfig {
        self.config.scope
    }

    /// How many repositories the sync worker pool runs at once. Zero reads as
    /// one: a pool with no workers would accept syncs and never run them.
    #[must_use]
    pub fn max_concurrent_syncs(&self) -> usize {
        self.config.max_concurrent_syncs.max(1)
    }

    /// Scope for writing this tenant's session rows.
    async fn session_scope(
        &self,
        ctx: &SecurityContext,
        action: &str,
    ) -> Result<AccessScope, DomainError> {
        let tenant_id = ctx.subject_tenant_id();
        Ok(self
            .policy_enforcer
            .access_scope_with(
                ctx,
                &SYNC_SESSION_RESOURCE,
                action,
                None,
                &AccessRequest::new().resource_property(pep_properties::OWNER_TENANT_ID, tenant_id),
            )
            .await?)
    }

    /// Scope for this tenant's per-repository run-status rows.
    async fn repo_status_scope(
        &self,
        ctx: &SecurityContext,
        action: &str,
    ) -> Result<AccessScope, DomainError> {
        let tenant_id = ctx.subject_tenant_id();
        Ok(self
            .policy_enforcer
            .access_scope_with(
                ctx,
                &REPO_SYNC_STATUS_RESOURCE,
                action,
                None,
                &AccessRequest::new().resource_property(pep_properties::OWNER_TENANT_ID, tenant_id),
            )
            .await?)
    }

    /// Write the repository's run status, preserving whatever a previous run
    /// recorded in the fields this transition does not own.
    async fn mark_repo_status(
        &self,
        ctx: &SecurityContext,
        repo_full_name: &str,
        session_id: Uuid,
        status: &str,
        synced_at: Option<String>,
    ) -> Result<(), DomainError> {
        let tenant_id = ctx.subject_tenant_id();
        let scope = self.repo_status_scope(ctx, actions::UPSERT).await?;

        let previous = self.repo_sync_status.find(&scope, repo_full_name).await?;
        let record = RepoSyncStatusRecord {
            repo_full_name: repo_full_name.to_owned(),
            repo_id: previous.as_ref().and_then(|p| p.repo_id),
            status: status.to_owned(),
            last_session_id: Some(session_id),
            last_synced_at: synced_at.or_else(|| previous.and_then(|p| p.last_synced_at)),
        };
        self.repo_sync_status
            .upsert(&scope, tenant_id, record)
            .await?;
        Ok(())
    }

    /// The tenant's per-repository run statuses, optionally one status only.
    ///
    /// # Errors
    /// `Forbidden`/`Database`/`Internal` as usual.
    pub async fn list_repo_sync_status(
        &self,
        ctx: &SecurityContext,
        query: &ODataQuery,
        status: Option<&str>,
    ) -> Result<Page<RepoSyncStatusRecord>, DomainError> {
        let scope = self.repo_status_scope(ctx, actions::LIST).await?;

        let limit = query.limit.unwrap_or(DEFAULT_LIST_LIMIT);
        let items = self.repo_sync_status.list(&scope, status, limit).await?;

        Ok(Page::new(
            items,
            PageInfo {
                next_cursor: None,
                prev_cursor: None,
                limit,
            },
        ))
    }

    /// The repositories a resume should re-run: one named slug, or every
    /// repository the scope can see that is still `in_progress`.
    async fn repos_awaiting_resume(
        &self,
        ctx: &SecurityContext,
        only: Option<&str>,
    ) -> Result<Vec<RepoSyncStatusRecord>, DomainError> {
        let scope = self.repo_status_scope(ctx, actions::LIST).await?;

        let Some(slug) = only else {
            return self
                .repo_sync_status
                .list(&scope, Some(repo_run_states::IN_PROGRESS), RESUME_LIMIT)
                .await;
        };

        Ok(self
            .repo_sync_status
            .find(&scope, slug)
            .await?
            .filter(|r| r.status == repo_run_states::IN_PROGRESS)
            .map_or_else(Vec::new, |r| vec![r]))
    }

    /// Re-run every repository this tenant still has marked `in_progress`.
    ///
    /// This is PRD §5.2's resume operation. Resume is a re-run, not a restore:
    /// nothing about the interrupted run is replayed, and re-running is cheap
    /// only once the `ETag` cache and change-detection state exist (#4630 and
    /// #4632 slice 6) — until then it costs a full sync.
    ///
    /// Resume takes no per-run scope: `ALGORITHMS.md` §8 has it resolve scope
    /// from configuration only, so a resumed run collects whatever the gear
    /// is configured to collect.
    ///
    /// `only` narrows the operation to one `owner/name` slug, matching the
    /// documented CLI's `resume <ORG/REPO>`. A repository that is not
    /// `in_progress` has nothing to resume and yields an empty result rather
    /// than a fresh sync — asking for that is what `POST /sync` is for.
    ///
    /// A repository whose sync is already queued or running resumes into that
    /// run rather than a second one, so calling resume twice is harmless.
    ///
    /// # Errors
    /// `Forbidden`/`Database` as usual. A repository that cannot be queued is
    /// skipped, so one full queue does not abandon the rest.
    pub async fn resume_incomplete_syncs(
        &self,
        ctx: &SecurityContext,
        only: Option<&str>,
        force: bool,
    ) -> Result<Vec<Uuid>, DomainError> {
        let pending = self.repos_awaiting_resume(ctx, only).await?;

        let mut resumed = Vec::with_capacity(pending.len());
        for repo in pending {
            let Some((owner, name)) = repo.repo_full_name.split_once('/') else {
                tracing::warn!(
                    repository = %repo.repo_full_name,
                    "run-status row has no owner/name slug; skipping"
                );
                continue;
            };
            match self.enqueue_sync(ctx, owner, name, None, force, None).await {
                Ok(session_id) => resumed.push(session_id),
                Err(e) => tracing::warn!(
                    repository = %repo.repo_full_name,
                    error = %e,
                    "could not queue a resume for this repository"
                ),
            }
        }

        Ok(resumed)
    }

    /// Record a sync request and hand it to the background worker.
    ///
    /// Returns as soon as the `queued` row is durable — the fetch itself
    /// happens later, on the gear's background task. Poll
    /// [`Self::get_session`] with the returned id to watch it finish.
    ///
    /// A repository already queued or running collapses into that run: the
    /// caller gets its session id back instead of a second session, so a
    /// double-click, a retry and a resume of the same repository cost one
    /// sync. `force` does not split the two — the run in flight is already
    /// fetching the repository.
    ///
    /// # Errors
    /// `Forbidden`/`Database` as usual, or `Internal` when the queue is full
    /// or the background worker is not running; in both cases the session is
    /// left behind in `failed` rather than silently dropped.
    pub async fn enqueue_sync(
        &self,
        ctx: &SecurityContext,
        owner: &str,
        name: &str,
        sync_scope: Option<ScopeConfig>,
        force: bool,
        since: Option<DateTime<Utc>>,
    ) -> Result<Uuid, DomainError> {
        let sync_scope = sync_scope.unwrap_or(self.config.scope);
        sync_scope.validate()?;
        let tenant_id = ctx.subject_tenant_id();
        let scope = self.session_scope(ctx, actions::UPSERT).await?;
        let key = (tenant_id, format!("{owner}/{name}"));
        let id = Uuid::new_v4();
        {
            // Claimed under the lock so two concurrent requests cannot both
            // decide they are the first.
            let mut in_flight = self.in_flight.lock().await;
            if let Some(running) = in_flight.get(&key) {
                tracing::debug!(
                    repository = %key.1,
                    session_id = %running,
                    "sync already in flight; collapsing into it"
                );
                return Ok(*running);
            }
            in_flight.insert(key.clone(), id);
        }

        let mut session = SyncSessionRecord {
            id,
            repo_full_name: format!("{owner}/{name}"),
            repo_id: None,
            status: session_states::QUEUED.to_owned(),
            progress_percent: 0,
            error: None,
            summary_json: None,
            created_at: now_rfc3339(),
            started_at: None,
            ended_at: None,
        };
        self.sync_sessions
            .upsert(&scope, tenant_id, session.clone())
            .await?;
        self.mark_repo_status(
            ctx,
            &session.repo_full_name,
            id,
            repo_run_states::IN_PROGRESS,
            None,
        )
        .await?;

        let job = SyncJob {
            session_id: id,
            ctx: ctx.clone(),
            owner: owner.to_owned(),
            name: name.to_owned(),
            scope: sync_scope,
            force,
            since,
        };
        if let Err(e) = self.sync_tx.try_send(job) {
            self.release_in_flight(&key).await;
            let reason = format!("sync could not be queued: {e}");
            session_states::FAILED.clone_into(&mut session.status);
            session.ended_at = Some(now_rfc3339());
            session.error = Some(reason.clone());
            self.sync_sessions
                .upsert(&scope, tenant_id, session)
                .await?;
            return Err(DomainError::internal(reason));
        }

        Ok(id)
    }

    /// Give up a repository's claim so the next request queues a fresh sync.
    async fn release_in_flight(&self, key: &InFlightKey) {
        self.in_flight.lock().await.remove(key);
    }

    /// Take sole ownership of the job stream. The gear's background task calls
    /// this once at startup; every later call sees `None`.
    pub async fn take_sync_receiver(&self) -> Option<mpsc::Receiver<SyncJob>> {
        self.sync_rx.lock().await.take()
    }

    /// Run one queued job to completion and record the outcome on its session.
    ///
    /// Errors from the sync land in the session row rather than propagating —
    /// nobody is waiting on the return value, so a failed run must still be
    /// visible through the sessions API.
    ///
    /// # Errors
    /// Only failures to *persist* the outcome, which the caller logs.
    pub async fn run_sync_job(
        &self,
        job: &SyncJob,
        cancel: &CancellationToken,
    ) -> Result<(), DomainError> {
        let outcome = self.run_sync_job_inner(job, cancel).await;
        // Released on every path, including the error ones: a claim that
        // outlived its job would block the repository until restart.
        self.release_in_flight(&(
            job.ctx.subject_tenant_id(),
            format!("{}/{}", job.owner, job.name),
        ))
        .await;
        outcome
    }

    /// The body of [`Self::run_sync_job`], minus the in-flight bookkeeping.
    async fn run_sync_job_inner(
        &self,
        job: &SyncJob,
        cancel: &CancellationToken,
    ) -> Result<(), DomainError> {
        let tenant_id = job.ctx.subject_tenant_id();
        let scope = self.session_scope(&job.ctx, actions::UPSERT).await?;

        let mut session = self
            .sync_sessions
            .find_by_id(&scope, job.session_id)
            .await?
            .ok_or(DomainError::NotFound)?;
        session_states::IN_PROGRESS.clone_into(&mut session.status);
        session.started_at = Some(now_rfc3339());
        self.sync_sessions
            .upsert(&scope, tenant_id, session.clone())
            .await?;

        let progress = SyncProgress::new();
        let outcome = self
            .sync_with_heartbeat(job, &progress, &session, cancel)
            .await;

        let completed = outcome.is_ok();
        match outcome {
            Ok(summary) => {
                progress.finished();
                session_states::COMPLETE.clone_into(&mut session.status);
                session.summary_json = serde_json::to_string(&summary).ok();
            }
            Err(e) => {
                let ended_as = if cancel.is_cancelled() {
                    session_states::INTERRUPTED
                } else {
                    session_states::FAILED
                };
                ended_as.clone_into(&mut session.status);
                session.error = Some(e.to_string());
            }
        }
        session.progress_percent = i32::from(progress.percent());
        session.ended_at = Some(now_rfc3339());
        let repo_full_name = session.repo_full_name.clone();
        self.sync_sessions
            .upsert(&scope, tenant_id, session)
            .await?;

        // A run that failed leaves the repository `in_progress` on purpose:
        // that is the marker the resume operation looks for (PRD §5.2).
        if completed {
            self.mark_repo_status(
                &job.ctx,
                &repo_full_name,
                job.session_id,
                repo_run_states::COMPLETE,
                Some(now_rfc3339()),
            )
            .await?;
        }

        Ok(())
    }

    /// Run the sync while a ticker writes its progress to the session row.
    ///
    /// DESIGN §4 has progress "published via a shared atomic" and persisted
    /// "incrementally ... via a heartbeat (also stamping `ended_at`)", so a
    /// caller polling the session sees the run advance and can compute its
    /// duration before it ends. A heartbeat that fails to write is logged and
    /// skipped — losing a progress sample must not fail the sync.
    async fn sync_with_heartbeat(
        &self,
        job: &SyncJob,
        progress: &SyncProgress,
        session: &SyncSessionRecord,
        cancel: &CancellationToken,
    ) -> Result<SyncSummary, DomainError> {
        let percent = progress.handle();
        let options = FetchOptions {
            tenant_id: job.ctx.subject_tenant_id(),
            scope: job.scope,
            force: job.force,
            since: job.since,
        };
        let sync =
            self.sync_repository(&job.ctx, &job.owner, &job.name, &options, progress, cancel);
        let mut sync = std::pin::pin!(sync);

        loop {
            tokio::select! {
                outcome = &mut sync => return outcome,
                () = tokio::time::sleep(std::time::Duration::from_secs(HEARTBEAT_SECS)) => {
                    let mut beat = session.clone();
                    beat.progress_percent = i32::from(percent.load(Ordering::Relaxed));
                    beat.ended_at = Some(now_rfc3339());
                    if let Err(e) = self.save_session_progress(&job.ctx, beat).await {
                        tracing::warn!(
                            session_id = %job.session_id,
                            error = %e,
                            "sync heartbeat could not persist progress"
                        );
                    }
                }
            }
        }
    }

    /// One heartbeat write, on its own scope.
    async fn save_session_progress(
        &self,
        ctx: &SecurityContext,
        session: SyncSessionRecord,
    ) -> Result<(), DomainError> {
        let scope = self.session_scope(ctx, actions::UPSERT).await?;
        self.sync_sessions
            .upsert(&scope, ctx.subject_tenant_id(), session)
            .await?;
        Ok(())
    }

    /// Close out sessions left mid-flight by a previous process.
    ///
    /// The queue is in-memory, so a `queued` or `in_progress` row that survives a
    /// restart has no worker behind it and never will. Called once at startup,
    /// across every tenant — hence the unconstrained scope, which is why this
    /// takes no [`SecurityContext`] and is not reachable from the API.
    ///
    /// # Errors
    /// `Database` when the sweep cannot read or write the session table.
    pub async fn sweep_interrupted_sessions(&self) -> Result<usize, DomainError> {
        let scope = AccessScope::allow_all();

        let stale = self
            .sync_sessions
            .list_by_statuses(
                &scope,
                &[
                    session_states::QUEUED,
                    session_states::IN_PROGRESS,
                    session_states::INTERRUPTED,
                ],
            )
            .await?;

        let mut count = 0;
        for (tenant_id, mut session) in stale {
            self.release_stale_sync_lock(tenant_id, &session.repo_full_name)
                .await;
            if session.status == session_states::INTERRUPTED {
                continue;
            }
            count += 1;
            session_states::INTERRUPTED.clone_into(&mut session.status);
            session.ended_at = Some(now_rfc3339());
            session.error = Some("the server restarted while this sync was in flight".to_owned());
            self.sync_sessions
                .upsert(&scope, tenant_id, session)
                .await?;
        }

        Ok(count)
    }

    /// Drop the per-repo sync lock marker a dead process left for `repo`, if
    /// one is there. Safe at start-up only: this process holds no sync lock
    /// yet, and the file backend is used by one process per `SQLite` file.
    async fn release_stale_sync_lock(&self, tenant_id: Uuid, repo: &str) {
        let Some((owner, name)) = repo.split_once('/') else {
            return;
        };
        let lock_key = format!("sync/{tenant_id}/{owner}/{name}");
        match self.db.db().break_stale_lock(GEAR_NAME, &lock_key).await {
            Ok(true) => tracing::info!(
                repository = repo,
                "released the sync lock a dead process left behind"
            ),
            Ok(false) => {}
            Err(e) => tracing::warn!(
                repository = repo,
                error = %e,
                "could not release a stale sync lock"
            ),
        }
    }

    /// One sync session by id, tenant-scoped.
    ///
    /// # Errors
    /// `DomainError::NotFound` when the session does not exist for this
    /// tenant; `Forbidden`/`Database`/`Internal` as usual.
    pub async fn get_session(
        &self,
        ctx: &SecurityContext,
        id: Uuid,
    ) -> Result<SyncSessionRecord, DomainError> {
        let scope = self
            .policy_enforcer
            .access_scope_with(
                ctx,
                &SYNC_SESSION_RESOURCE,
                actions::GET,
                None,
                &AccessRequest::new()
                    .resource_property(pep_properties::OWNER_TENANT_ID, ctx.subject_tenant_id()),
            )
            .await?;
        self.sync_sessions
            .find_by_id(&scope, id)
            .await?
            .ok_or(DomainError::NotFound)
    }

    /// The tenant's sync sessions, newest first.
    ///
    /// # Errors
    /// `Forbidden`/`Database`/`Internal` as usual.
    pub async fn list_sessions(
        &self,
        ctx: &SecurityContext,
        query: &ODataQuery,
    ) -> Result<Page<SyncSessionRecord>, DomainError> {
        let scope = self
            .policy_enforcer
            .access_scope_with(
                ctx,
                &SYNC_SESSION_RESOURCE,
                actions::LIST,
                None,
                &AccessRequest::new()
                    .resource_property(pep_properties::OWNER_TENANT_ID, ctx.subject_tenant_id()),
            )
            .await?;

        let limit = query.limit.unwrap_or(DEFAULT_LIST_LIMIT);
        let items = self.sync_sessions.list_recent(&scope, limit).await?;

        Ok(Page::new(
            items,
            PageInfo {
                next_cursor: None,
                prev_cursor: None,
                limit,
            },
        ))
    }

    /// Cheap DB reachability probe for the platform's readiness aggregation
    /// (`RestApiCapability::healthcheck`): acquiring a pooled connection, no
    /// query, so a routine `/readyz` poll costs nothing beyond a pool-handle
    /// acquisition.
    #[must_use]
    pub(crate) fn db_reachable(&self) -> bool {
        self.db.conn().is_ok()
    }

    /// Sync one repository from GitHub into the mirror: the 5-phase runner
    /// (DESIGN §4) under the per-repo advisory lock, then the deletion pass.
    ///
    /// Every task writes its own transaction, so a run that stops partway
    /// leaves the tables consistent and the next run picks up where it left
    /// off (ADR-0001: resume by re-running, no persisted task state).
    ///
    /// # Errors
    /// `DomainError::NotFound` when GitHub does not know the repository,
    /// `Conflict` when a sync of it is already running, `Forbidden` on PDP
    /// denial, `Internal` when tasks failed or the run outlived its budget.
    pub async fn sync_repository(
        &self,
        ctx: &SecurityContext,
        owner: &str,
        name: &str,
        options: &FetchOptions,
        progress: &SyncProgress,
        cancel: &CancellationToken,
    ) -> Result<SyncSummary, DomainError> {
        let tenant_id = ctx.subject_tenant_id();

        let scope = self
            .policy_enforcer
            .access_scope_with(
                ctx,
                &SYNC_RESOURCE,
                actions::SYNC,
                None,
                &AccessRequest::new().resource_property(pep_properties::OWNER_TENANT_ID, tenant_id),
            )
            .await?;

        let lock_key = format!("sync/{tenant_id}/{owner}/{name}");
        let sync_lock = match self.db.db().lock(GEAR_NAME, &lock_key).await {
            Ok(guard) => guard,
            Err(toolkit_db::DbError::Lock(toolkit_db::DbLockError::AlreadyHeld { .. })) => {
                // Logged as well as returned: on the file-marker backend the
                // library takes no lock back automatically (a TTL or a PID
                // check can steal a live one), so a marker left by a killed
                // process keeps answering 409 until someone removes it. The
                // key is what an operator needs to find it.
                // The repository, not the composite key: the key carries the
                // tenant id, which is an authorization identifier and not
                // something every log consumer needs to see.
                tracing::warn!(
                    owner,
                    name,
                    "sync lock already held; a repeated 409 with no sync                      running means a stale marker for this repository"
                );
                return Err(DomainError::Conflict(format!(
                    "a sync for {owner}/{name} is already running"
                )));
            }
            Err(e) => return Err(DomainError::Database(e)),
        };

        let run = Arc::new(RunState::new(
            Uuid::new_v4(),
            scope,
            tenant_id,
            owner,
            name,
            *options,
        ));
        let outcome = self.run_phases(&run, progress, cancel).await;

        // Deterministic unlock on the way out; a failed release is only
        // logged — the guard's Drop already queued a best-effort release,
        // and the sync itself succeeded or failed on its own merits.
        release_sync_lock(sync_lock, &lock_key).await;
        outcome
    }

    /// The part of a sync that runs under the lock: phases, then reconciliation.
    async fn run_phases(
        &self,
        run: &Arc<RunState>,
        progress: &SyncProgress,
        cancel: &CancellationToken,
    ) -> Result<SyncSummary, DomainError> {
        // Captured before any row is written: every upsert in this sync stamps
        // `extracted_at` with a later instant, so "extracted_at < watermark"
        // identifies exactly the rows this sync did not touch.
        let watermark = Utc::now();

        let worker: Arc<dyn Worker> = Arc::new(MirrorWorker::new(
            Arc::clone(&self.github),
            Arc::clone(&self.sync_writer),
            Arc::clone(&self.change_gate),
            Arc::clone(&self.sweep_watermark),
            Arc::clone(run),
        ));
        let runner = RepoPhaseRunner::new(
            vec![worker],
            run.session_id,
            run.tenant_id,
            self.config.max_concurrent_tasks,
            cancel.child_token(),
            progress.handle(),
        );
        let mut report = runner.run().await;
        // Discovery failing is the repository failing: GitHub's own answer
        // (404, 403 ...) is the sync's outcome, not a task statistic.
        if let Some(discovery) = report
            .failures
            .iter()
            .position(|failure| failure.phase == TaskPhase::Discovery)
        {
            return Err(report.failures.swap_remove(discovery).error);
        }
        if !report.failures.is_empty() {
            let detail: Vec<String> = report.failures.iter().map(ToString::to_string).collect();
            return Err(DomainError::internal(format!(
                "{} of {} sync tasks failed: {}",
                report.tasks_failed(),
                report.tasks_done + report.tasks_failed(),
                detail.join("; ")
            )));
        }

        if report.cancelled {
            return Err(DomainError::internal(format!(
                "the sync of {}/{} was interrupted before it finished",
                run.owner, run.name
            )));
        }

        for family in [
            sweep_families::ISSUES,
            sweep_families::PULL_REQUESTS,
            sweep_families::COMMITS,
        ] {
            if run.is_swept(family) {
                self.sweep_watermark
                    .promote(
                        &run.scope,
                        run.tenant_id,
                        run.repo_id()?,
                        family,
                        run.swept_page1_etag(family),
                    )
                    .await?;
            }
        }

        let stale_rows_deleted = self
            .sync_writer
            .reconcile_stale(&run.scope, run.repo_id()?, &run.completeness(), watermark)
            .await?;
        if stale_rows_deleted > 0 {
            tracing::info!(
                repository = %format!("{}/{}", run.owner, run.name),
                stale_rows_deleted,
                "reconciled upstream deletions"
            );
        }
        progress.stored();

        let mut summary = run.summary();
        summary.stale_rows_deleted = stale_rows_deleted;
        summary.accepted_drift = run.drift();
        Ok(summary)
    }
}
