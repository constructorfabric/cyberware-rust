use std::sync::Arc;

use authz_resolver_sdk::PolicyEnforcer;
use authz_resolver_sdk::pep::{AccessRequest, ResourceType};
use chrono::{DateTime, Utc};
use github_mirror_sdk::{
    Branch, CheckRun, Comment, Commit, CommitComment, CommitFile, CommitStatus, Contributor,
    Deployment, Issue, IssueEvent, IssueReaction, IssueTimelineEvent, Label, Milestone,
    MirrorStatus, PullRequest, PullRequestCommit, PullRequestFile, Release, Repo, Review,
    ReviewComment, ReviewThread, SyncSummary, Tag, WorkflowJob, WorkflowRun,
};
use toolkit_macros::domain_model;
use toolkit_odata::{ODataQuery, Page, PageInfo};
use toolkit_security::{SecurityContext, pep_properties};

use super::error::DomainError;

use super::ports::github::GithubPort;
use super::repo::{
    BranchRecord, BranchRepository, CheckRunRecord, CheckRunRepository, CommentRecord,
    CommentRepository, CommitCommentRecord, CommitCommentRepository, CommitFileRecord,
    CommitFileRepository, CommitRecord, CommitRepository, CommitStatusRecord,
    CommitStatusRepository, ContributorRecord, ContributorRepository, DeploymentRecord,
    DeploymentRepository, IssueEventRecord, IssueEventRepository, IssueReactionRecord,
    IssueReactionRepository, IssueRecord, IssueRepository, IssueTimelineEventRecord,
    IssueTimelineRepository, LabelRecord, LabelRepository, ListingFilter, MilestoneRecord,
    MilestoneRepository, PageWindow, PullRequestCommitRecord, PullRequestCommitRepository,
    PullRequestFileRecord, PullRequestFileRepository, PullRequestRecord, PullRequestRepository,
    ReleaseRecord, ReleaseRepository, RepoRecord, RepoRepository, ReviewCommentRecord,
    ReviewCommentRepository, ReviewRecord, ReviewRepository, ReviewThreadRecord,
    ReviewThreadRepository, SyncWriter, TagRecord, TagRepository, WorkflowJobRecord,
    WorkflowJobRepository, WorkflowRunRecord, WorkflowRunRepository,
};
use super::validate::{repo_full_name, validate_commit_sha};

/// The gear's name, taken from the `#[toolkit::gear]` attribute so the
/// literal exists in exactly one place.
pub const GEAR_NAME: &str = crate::gear::GithubMirrorGear::MODULE_NAME;

/// Release the per-repo sync lock, logging a failed release rather than
/// turning it into the sync's outcome: the guard's `Drop` has already queued
/// a best-effort release, and the sync succeeded or failed on its own merits.
async fn release_sync_lock(lock: toolkit_db::DbLockGuard, lock_key: &str) {
    if let Err(e) = lock.release().await {
        tracing::warn!(lock_key, error = %e, "sync advisory lock release failed");
    }
}

/// Longest a single repository's fetch may run before the sync gives up.
///
/// The fetch is one long sequence of GitHub calls with their own per-request
/// timeouts and rate-limit back-offs; this is the only bound on the whole of
/// it, and it is what stops a slow upstream from holding the per-repo
/// advisory lock indefinitely.
const SYNC_FETCH_BUDGET: std::time::Duration = std::time::Duration::from_mins(30);

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

pub(crate) const SYNC_RESOURCE: ResourceType =
    ResourceType::from_static("github_mirror.sync", &[pep_properties::OWNER_TENANT_ID]);

pub(crate) mod actions {
    pub const LIST: &str = "list";
    pub const UPSERT: &str = "upsert";
    pub const SYNC: &str = "sync";
}

#[domain_model]
#[derive(Debug, Clone)]
pub struct ServiceConfig {
    pub api_base_url: String,
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
    sync_writer: Arc<dyn SyncWriter>,
    github: Arc<dyn GithubPort>,
    policy_enforcer: PolicyEnforcer,
    config: ServiceConfig,
}

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
            sync_writer: Arc::clone(&self.sync_writer),
            github: Arc::clone(&self.github),
            policy_enforcer: self.policy_enforcer.clone(),
            config: self.config.clone(),
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
        sync_writer: Arc<dyn SyncWriter>,
        github: Arc<dyn GithubPort>,
        policy_enforcer: PolicyEnforcer,
        config: ServiceConfig,
    ) -> Self {
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
            sync_writer,
            github,
            policy_enforcer,
            config,
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

    /// Cheap DB reachability probe for the platform's readiness aggregation
    /// (`RestApiCapability::healthcheck`): acquiring a pooled connection, no
    /// query, so a routine `/readyz` poll costs nothing beyond a pool-handle
    /// acquisition.
    #[must_use]
    pub(crate) fn db_reachable(&self) -> bool {
        self.db.conn().is_ok()
    }

    /// Fetch one repository from GitHub (first slice: repo + first page of
    /// issues, pull requests, and commits) and upsert it into the mirror.
    ///
    /// The REST face of the PRD's `sync_repo` entry point; the inline fetch
    /// is replaced by a queued sync session when the engine lands
    /// (gears-rust#4632).
    ///
    /// # Errors
    /// `DomainError::NotFound` when GitHub does not know the repository,
    /// `Forbidden` on PDP denial, `Internal` on GitHub/storage failures.
    // One `sync_table!` pass per mirrored table, in the order the PRD
    // lists them. `too_many_lines` is the 26 mechanical `T: 'static` bounds
    // the transaction closure needs below, not additional real logic.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cognitive_complexity,
        clippy::too_many_lines
    )]
    pub async fn sync_repository(
        &self,
        ctx: &SecurityContext,
        owner: &str,
        name: &str,
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

        let watermark = Utc::now();
        let fetched = match tokio::time::timeout(
            SYNC_FETCH_BUDGET,
            self.github.fetch_repository(owner, name),
        )
        .await
        {
            Ok(Ok(fetched)) => fetched,
            Ok(Err(fetch_error)) => {
                release_sync_lock(sync_lock, &lock_key).await;
                return Err(fetch_error);
            }
            Err(_elapsed) => {
                release_sync_lock(sync_lock, &lock_key).await;
                return Err(DomainError::internal(format!(
                    "the sync of {owner}/{name} ran past its {} second budget",
                    SYNC_FETCH_BUDGET.as_secs()
                )));
            }
        };

        let summary = self
            .sync_writer
            .write_sync(&scope, tenant_id, fetched, watermark)
            .await;

        // Deterministic unlock on the way out; a failed release is only
        // logged — the guard's Drop already queued a best-effort release,
        // and the sync itself succeeded or failed on its own merits.
        release_sync_lock(sync_lock, &lock_key).await;
        summary
    }
}
