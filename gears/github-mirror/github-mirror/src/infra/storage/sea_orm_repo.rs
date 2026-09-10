use std::sync::Arc;

use strum::IntoEnumIterator;

use async_trait::async_trait;
use chrono::Utc;

use crate::domain::ports::github::{FetchedRepository, Listing, ListingCompleteness};
use crate::domain::repo::{PageWindow, SyncWriter};
use crate::domain::service::DbProvider;
use crate::infra::storage::odata_mapper::{
    CommitFileField, CommitFileODataMapper, RepoField, RepoODataMapper, ReviewThreadField,
    ReviewThreadODataMapper,
};
use chrono::SubsecRound as _;
use github_mirror_sdk::{
    Branch, CheckRun, Comment, Commit, CommitComment, CommitFile, CommitStatus, Contributor,
    Deployment, Issue, IssueEvent, IssueReaction, IssueTimelineEvent, Label, Milestone,
    PullRequest, PullRequestCommit, PullRequestFile, Release, Repo, Review, ReviewComment,
    ReviewThread, SyncSummary, Tag, WorkflowJob, WorkflowRun,
};
use sea_orm::prelude::DateTimeUtc;
use sea_orm::{ActiveValue, ColumnTrait, EntityTrait, Order};
use toolkit_db::odata::sea_orm_filter::{LimitCfg, paginate_odata};
use toolkit_db::secure::{
    DBRunner, ScopeError, SecureDeleteExt, SecureEntityExt, SecureInsertExt, SecureOnConflict,
};
use toolkit_odata::{ODataQuery, Page, SortDir};
use toolkit_security::AccessScope;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::repo::{
    BranchRecord, BranchRepository, CheckRunRecord, CheckRunRepository, CommentRecord,
    CommentRepository, CommitCommentRecord, CommitCommentRepository, CommitFileRecord,
    CommitFileRepository, CommitRecord, CommitRepository, CommitStatusRecord,
    CommitStatusRepository, ContributorRecord, ContributorRepository, DeploymentRecord,
    DeploymentRepository, IssueEventRecord, IssueEventRepository, IssueReactionRecord,
    IssueReactionRepository, IssueRecord, IssueRepository, IssueTimelineEventRecord,
    IssueTimelineRepository, LabelRecord, LabelRepository, ListingDirection, ListingFilter,
    ListingSort, MilestoneRecord, MilestoneRepository, PullRequestCommitRecord,
    PullRequestCommitRepository, PullRequestFileRecord, PullRequestFileRepository,
    PullRequestRecord, PullRequestRepository, ReleaseRecord, ReleaseRepository, RepoRecord,
    RepoRepository, ReviewCommentRecord, ReviewCommentRepository, ReviewRecord, ReviewRepository,
    ReviewThreadRecord, ReviewThreadRepository, TagRecord, TagRepository, WorkflowJobRecord,
    WorkflowJobRepository, WorkflowRunRecord, WorkflowRunRepository,
};

use super::mapper::{
    StoredActor, StoredAsset, StoredLabel, StoredRow, StoredStep, decode, decode_list,
    timeline_payload,
};

use super::entity::branches::{self, Entity as BranchEntity};
use super::entity::check_runs::{self, Entity as CheckRunEntity};
use super::entity::comments::{self, Entity as CommentEntity};
use super::entity::commit_comments::{self, Entity as CommitCommentEntity};
use super::entity::commit_files::{self, Entity as CommitFileEntity};
use super::entity::commit_statuses::{self, Entity as CommitStatusEntity};
use super::entity::commits::{self, Entity as CommitEntity};
use super::entity::contributors::{self, Entity as ContributorEntity};
use super::entity::deployments::{self, Entity as DeploymentEntity};
use super::entity::issue_events::{self, Entity as IssueEventEntity};
use super::entity::issue_reactions::{self, Entity as IssueReactionEntity};
use super::entity::issue_timeline::{self, Entity as IssueTimelineEntity};
use super::entity::issues::{self, Entity as IssueEntity};
use super::entity::labels::{self, Entity as LabelEntity};
use super::entity::milestones::{self, Entity as MilestoneEntity};
use super::entity::pull_request_commits::{self, Entity as PullRequestCommitEntity};
use super::entity::pull_request_files::{self, Entity as PullRequestFileEntity};
use super::entity::pull_requests::{self, Entity as PullRequestEntity};
use super::entity::releases::{self, Entity as ReleaseEntity};
use super::entity::repositories::{self, Entity as RepoEntity};
use super::entity::review_comments::{self, Entity as ReviewCommentEntity};
use super::entity::review_threads::{self, Entity as ReviewThreadEntity};
use super::entity::reviews::{self, Entity as ReviewEntity};
use super::entity::tags::{self, Entity as TagEntity};
use super::entity::workflow_jobs::{self, Entity as WorkflowJobEntity};
use super::entity::workflow_runs::{self, Entity as WorkflowRunEntity};

pub struct SeaOrmRepoRepository {
    db: Arc<DbProvider>,
}

impl SeaOrmRepoRepository {
    #[must_use]
    pub fn new(db: Arc<DbProvider>) -> Self {
        Self { db }
    }
}

/// An instant in the exact shape GitHub writes into the stored `updated_at`
/// text, so the comparison against that TEXT column is a like-for-like one.
///
/// GitHub's own stamps carry no fractional seconds, so a `since` that does
/// carry them is rounded up to the next whole second: `00:00:00.500Z` becomes
/// `00:00:01Z`, and a row stamped `00:00:00Z` is correctly left out. Truncating
/// instead would admit rows from up to a second before the asked-for instant.
fn github_instant(at: chrono::DateTime<chrono::Utc>) -> String {
    let at = if at.timestamp_subsec_nanos() == 0 {
        at
    } else {
        at.trunc_subsecs(0)
            .checked_add_signed(chrono::Duration::seconds(1))
            // Within a second of the largest representable instant, where
            // adding one would overflow. That instant is already past every
            // stored stamp, so it is the right bound to compare against.
            .unwrap_or(chrono::DateTime::<chrono::Utc>::MAX_UTC)
    };
    at.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// Page-size bounds for the `OData` listings: the platform default, capped so a
/// caller cannot ask for the whole table in one request.
const LIST_LIMITS: LimitCfg = LimitCfg {
    default: 50,
    max: 200,
};

fn map_odata_error(e: impl std::fmt::Display) -> DomainError {
    DomainError::internal(e.to_string())
}

fn map_scope_error(e: ScopeError) -> DomainError {
    match e {
        ScopeError::Denied(msg) => DomainError::forbidden(msg),
        ScopeError::Invalid(msg) => DomainError::internal(format!("scope invalid: {msg}")),
        ScopeError::Db(e) => DomainError::internal(format!("database error: {e}")),
        ScopeError::TenantNotInScope { tenant_id } => {
            DomainError::forbidden(format!("tenant {tenant_id} not in scope"))
        }
        // `ScopeError` is `#[non_exhaustive]`: variants this gear has no
        // specific answer for (today the graph-query refusals, which it can
        // never trigger) map to an internal error, like `Invalid`.
        other => DomainError::internal(format!("scope invalid: {other}")),
    }
}

fn active_model(tenant_id: Uuid, r: &RepoRecord) -> repositories::ActiveModel {
    repositories::ActiveModel {
        tenant_id: ActiveValue::Set(tenant_id),
        id: ActiveValue::Set(r.id),
        node_id: ActiveValue::Set(r.node_id.clone()),
        owner: ActiveValue::Set(r.owner.clone()),
        name: ActiveValue::Set(r.name.clone()),
        full_name: ActiveValue::Set(r.full_name.clone()),
        default_branch: ActiveValue::Set(r.default_branch.clone()),
        private: ActiveValue::Set(r.private),
        pushed_at: ActiveValue::Set(r.pushed_at.clone()),
        stars: ActiveValue::Set(r.stars),
        forks: ActiveValue::Set(r.forks),
        description: ActiveValue::Set(r.description.clone()),
        clone_url: ActiveValue::Set(r.clone_url.clone()),
        extracted_at: ActiveValue::Set(Some(Utc::now())),
    }
}

#[async_trait]
impl RepoRepository for SeaOrmRepoRepository {
    async fn upsert(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        record: RepoRecord,
    ) -> Result<Repo, DomainError> {
        let conn = self.db.conn()?;
        repo_upsert_in(&conn, scope, tenant_id, record).await
    }

    async fn list(
        &self,
        scope: &AccessScope,
        query: &ODataQuery,
    ) -> Result<Page<Repo>, DomainError> {
        let conn = self.db.conn()?;
        repo_list_in(&conn, scope, query).await
    }

    async fn list_window(
        &self,
        scope: &AccessScope,
        window: PageWindow,
    ) -> Result<Vec<Repo>, DomainError> {
        let conn = self.db.conn()?;
        repo_list_window_in(&conn, scope, window).await
    }

    async fn find_by_full_name(
        &self,
        scope: &AccessScope,
        full_name: &str,
    ) -> Result<Option<Repo>, DomainError> {
        let conn = self.db.conn()?;
        repo_find_by_full_name_in(&conn, scope, full_name).await
    }
}

pub struct SeaOrmIssueRepository {
    db: Arc<DbProvider>,
}

impl SeaOrmIssueRepository {
    #[must_use]
    pub fn new(db: Arc<DbProvider>) -> Self {
        Self { db }
    }
}

fn issue_active_model(tenant_id: Uuid, r: &IssueRecord) -> issues::ActiveModel {
    issues::ActiveModel {
        tenant_id: ActiveValue::Set(tenant_id),
        id: ActiveValue::Set(r.id),
        node_id: ActiveValue::Set(r.node_id.clone()),
        repo_id: ActiveValue::Set(r.repo_id),
        number: ActiveValue::Set(r.number),
        title: ActiveValue::Set(r.title.clone()),
        body: ActiveValue::Set(r.body.clone()),
        state: ActiveValue::Set(r.state.clone()),
        is_pull_request: ActiveValue::Set(r.is_pull_request),
        created_at: ActiveValue::Set(r.created_at.clone()),
        updated_at: ActiveValue::Set(r.updated_at.clone()),
        closed_at: ActiveValue::Set(r.closed_at.clone()),
        html_url: ActiveValue::Set(r.html_url.clone()),
        extracted_at: ActiveValue::Set(Some(Utc::now())),
        author_login: ActiveValue::Set(r.author_login.clone()),
        author_json: ActiveValue::Set(r.author_json.clone()),
        assignees_json: ActiveValue::Set(r.assignees_json.clone()),
        labels_json: ActiveValue::Set(r.labels_json.clone()),
        comments_count: ActiveValue::Set(r.comments_count),
        locked: ActiveValue::Set(r.locked),
    }
}

#[async_trait]
impl IssueRepository for SeaOrmIssueRepository {
    async fn page_by_repo(
        &self,
        scope: &AccessScope,
        repo_id: i64,
        window: PageWindow,
        filter: ListingFilter,
    ) -> Result<(Vec<Issue>, u64), DomainError> {
        let scope = scope.clone();
        self.db
            .db()
            .transaction_ref_mapped(move |tx| {
                Box::pin(async move {
                    let items = issue_list_by_repo_in(tx, &scope, repo_id, window, filter).await?;
                    let total = issue_count_by_repo_in(tx, &scope, repo_id, filter).await?;
                    Ok((items, total))
                })
            })
            .await
    }

    async fn delete_stale(
        &self,
        scope: &AccessScope,
        repo_id: i64,
        extracted_before: DateTimeUtc,
    ) -> Result<u64, DomainError> {
        let conn = self.db.conn()?;
        issue_delete_stale_in(&conn, scope, repo_id, extracted_before).await
    }

    async fn upsert(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        record: IssueRecord,
    ) -> Result<Issue, DomainError> {
        let conn = self.db.conn()?;
        issue_upsert_in(&conn, scope, tenant_id, record).await
    }

    async fn list_by_repo(
        &self,
        scope: &AccessScope,
        repo_id: i64,
        window: PageWindow,
        filter: ListingFilter,
    ) -> Result<Vec<Issue>, DomainError> {
        let conn = self.db.conn()?;
        issue_list_by_repo_in(&conn, scope, repo_id, window, filter).await
    }
    async fn find_by_number(
        &self,
        scope: &AccessScope,
        repo_id: i64,
        number: i64,
    ) -> Result<Option<Issue>, DomainError> {
        let conn = self.db.conn()?;
        issue_find_by_number_in(&conn, scope, repo_id, number).await
    }
}

pub struct SeaOrmPullRequestRepository {
    db: Arc<DbProvider>,
}

impl SeaOrmPullRequestRepository {
    #[must_use]
    pub fn new(db: Arc<DbProvider>) -> Self {
        Self { db }
    }
}

fn pull_request_active_model(tenant_id: Uuid, r: &PullRequestRecord) -> pull_requests::ActiveModel {
    pull_requests::ActiveModel {
        tenant_id: ActiveValue::Set(tenant_id),
        id: ActiveValue::Set(r.id),
        node_id: ActiveValue::Set(r.node_id.clone()),
        repo_id: ActiveValue::Set(r.repo_id),
        number: ActiveValue::Set(r.number),
        title: ActiveValue::Set(r.title.clone()),
        body: ActiveValue::Set(r.body.clone()),
        state: ActiveValue::Set(r.state.clone()),
        draft: ActiveValue::Set(r.draft),
        merged: ActiveValue::Set(r.merged),
        head_sha: ActiveValue::Set(r.head_sha.clone()),
        base_sha: ActiveValue::Set(r.base_sha.clone()),
        lines_added: ActiveValue::Set(r.lines_added),
        lines_removed: ActiveValue::Set(r.lines_removed),
        created_at: ActiveValue::Set(r.created_at.clone()),
        updated_at: ActiveValue::Set(r.updated_at.clone()),
        closed_at: ActiveValue::Set(r.closed_at.clone()),
        merged_at: ActiveValue::Set(r.merged_at.clone()),
        html_url: ActiveValue::Set(r.html_url.clone()),
        head_ref: ActiveValue::Set(r.head_ref.clone()),
        base_ref: ActiveValue::Set(r.base_ref.clone()),
        extracted_at: ActiveValue::Set(Some(Utc::now())),
        author_login: ActiveValue::Set(r.author_login.clone()),
        author_json: ActiveValue::Set(r.author_json.clone()),
        assignees_json: ActiveValue::Set(r.assignees_json.clone()),
        labels_json: ActiveValue::Set(r.labels_json.clone()),
        comments_count: ActiveValue::Set(r.comments_count),
        locked: ActiveValue::Set(r.locked),
        requested_reviewers_json: ActiveValue::Set(r.requested_reviewers_json.clone()),
    }
}

#[async_trait]
impl PullRequestRepository for SeaOrmPullRequestRepository {
    async fn page_by_repo(
        &self,
        scope: &AccessScope,
        repo_id: i64,
        window: PageWindow,
        filter: ListingFilter,
    ) -> Result<(Vec<PullRequest>, u64), DomainError> {
        let scope = scope.clone();
        self.db
            .db()
            .transaction_ref_mapped(move |tx| {
                Box::pin(async move {
                    let items =
                        pull_request_list_by_repo_in(tx, &scope, repo_id, window, filter).await?;
                    let total = pull_request_count_by_repo_in(tx, &scope, repo_id, filter).await?;
                    Ok((items, total))
                })
            })
            .await
    }

    async fn delete_stale(
        &self,
        scope: &AccessScope,
        repo_id: i64,
        extracted_before: DateTimeUtc,
    ) -> Result<u64, DomainError> {
        let conn = self.db.conn()?;
        pull_request_delete_stale_in(&conn, scope, repo_id, extracted_before).await
    }

    async fn upsert(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        record: PullRequestRecord,
    ) -> Result<PullRequest, DomainError> {
        let conn = self.db.conn()?;
        pull_request_upsert_in(&conn, scope, tenant_id, record).await
    }

    async fn list_by_repo(
        &self,
        scope: &AccessScope,
        repo_id: i64,
        window: PageWindow,
        filter: ListingFilter,
    ) -> Result<Vec<PullRequest>, DomainError> {
        let conn = self.db.conn()?;
        pull_request_list_by_repo_in(&conn, scope, repo_id, window, filter).await
    }
    async fn find_by_number(
        &self,
        scope: &AccessScope,
        repo_id: i64,
        number: i64,
    ) -> Result<Option<PullRequest>, DomainError> {
        let conn = self.db.conn()?;
        pull_request_find_by_number_in(&conn, scope, repo_id, number).await
    }
}

pub struct SeaOrmCommitRepository {
    db: Arc<DbProvider>,
}

impl SeaOrmCommitRepository {
    #[must_use]
    pub fn new(db: Arc<DbProvider>) -> Self {
        Self { db }
    }
}

fn commit_active_model(tenant_id: Uuid, r: &CommitRecord) -> commits::ActiveModel {
    commits::ActiveModel {
        tenant_id: ActiveValue::Set(tenant_id),
        repo_id: ActiveValue::Set(r.repo_id),
        sha: ActiveValue::Set(r.sha.clone()),
        message: ActiveValue::Set(r.message.clone()),
        author_login: ActiveValue::Set(r.author_login.clone()),
        committer_login: ActiveValue::Set(r.committer_login.clone()),
        authored_at: ActiveValue::Set(r.authored_at.clone()),
        committed_at: ActiveValue::Set(r.committed_at.clone()),
        additions: ActiveValue::Set(r.additions),
        deletions: ActiveValue::Set(r.deletions),
        extracted_at: ActiveValue::Set(Some(Utc::now())),
    }
}

#[async_trait]
impl CommitRepository for SeaOrmCommitRepository {
    async fn page_by_repo(
        &self,
        scope: &AccessScope,
        repo_id: i64,
        window: PageWindow,
        since: Option<DateTimeUtc>,
    ) -> Result<(Vec<Commit>, u64), DomainError> {
        let scope = scope.clone();
        self.db
            .db()
            .transaction_ref_mapped(move |tx| {
                Box::pin(async move {
                    let items = commit_list_by_repo_in(tx, &scope, repo_id, window, since).await?;
                    let total = commit_count_by_repo_in(tx, &scope, repo_id, since).await?;
                    Ok((items, total))
                })
            })
            .await
    }

    async fn delete_stale(
        &self,
        scope: &AccessScope,
        repo_id: i64,
        extracted_before: DateTimeUtc,
    ) -> Result<u64, DomainError> {
        let conn = self.db.conn()?;
        commit_delete_stale_in(&conn, scope, repo_id, extracted_before).await
    }

    async fn upsert(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        record: CommitRecord,
    ) -> Result<Commit, DomainError> {
        let conn = self.db.conn()?;
        commit_upsert_in(&conn, scope, tenant_id, record).await
    }

    async fn list_by_repo(
        &self,
        scope: &AccessScope,
        repo_id: i64,
        window: PageWindow,
        since: Option<DateTimeUtc>,
    ) -> Result<Vec<Commit>, DomainError> {
        let conn = self.db.conn()?;
        commit_list_by_repo_in(&conn, scope, repo_id, window, since).await
    }
    async fn find_by_sha(
        &self,
        scope: &AccessScope,
        repo_id: i64,
        sha: &str,
    ) -> Result<Option<Commit>, DomainError> {
        let conn = self.db.conn()?;
        commit_find_by_sha_in(&conn, scope, repo_id, sha).await
    }
}

pub struct SeaOrmCommentRepository {
    db: Arc<DbProvider>,
}

impl SeaOrmCommentRepository {
    #[must_use]
    pub fn new(db: Arc<DbProvider>) -> Self {
        Self { db }
    }
}

fn comment_active_model(tenant_id: Uuid, r: &CommentRecord) -> comments::ActiveModel {
    comments::ActiveModel {
        tenant_id: ActiveValue::Set(tenant_id),
        id: ActiveValue::Set(r.id),
        repo_id: ActiveValue::Set(r.repo_id),
        issue_number: ActiveValue::Set(r.issue_number),
        author_login: ActiveValue::Set(r.author_login.clone()),
        body: ActiveValue::Set(r.body.clone()),
        created_at: ActiveValue::Set(r.created_at.clone()),
        updated_at: ActiveValue::Set(r.updated_at.clone()),
        html_url: ActiveValue::Set(r.html_url.clone()),
        extracted_at: ActiveValue::Set(Some(Utc::now())),
    }
}

#[async_trait]
impl CommentRepository for SeaOrmCommentRepository {
    async fn delete_stale(
        &self,
        scope: &AccessScope,
        repo_id: i64,
        extracted_before: DateTimeUtc,
    ) -> Result<u64, DomainError> {
        let conn = self.db.conn()?;
        comment_delete_stale_in(&conn, scope, repo_id, extracted_before).await
    }

    async fn upsert(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        record: CommentRecord,
    ) -> Result<Comment, DomainError> {
        let conn = self.db.conn()?;
        comment_upsert_in(&conn, scope, tenant_id, record).await
    }

    async fn list_by_issue(
        &self,
        scope: &AccessScope,
        repo_id: i64,
        issue_number: i64,
        window: PageWindow,
    ) -> Result<Vec<Comment>, DomainError> {
        let conn = self.db.conn()?;
        comment_list_by_issue_in(&conn, scope, repo_id, issue_number, window).await
    }
}

pub struct SeaOrmReviewCommentRepository {
    db: Arc<DbProvider>,
}

impl SeaOrmReviewCommentRepository {
    #[must_use]
    pub fn new(db: Arc<DbProvider>) -> Self {
        Self { db }
    }
}

fn review_comment_active_model(
    tenant_id: Uuid,
    r: &ReviewCommentRecord,
) -> review_comments::ActiveModel {
    review_comments::ActiveModel {
        tenant_id: ActiveValue::Set(tenant_id),
        id: ActiveValue::Set(r.id),
        repo_id: ActiveValue::Set(r.repo_id),
        pull_number: ActiveValue::Set(r.pull_number),
        author_login: ActiveValue::Set(r.author_login.clone()),
        body: ActiveValue::Set(r.body.clone()),
        path: ActiveValue::Set(r.path.clone()),
        diff_hunk: ActiveValue::Set(r.diff_hunk.clone()),
        in_reply_to_id: ActiveValue::Set(r.in_reply_to_id),
        commit_id: ActiveValue::Set(r.commit_id.clone()),
        created_at: ActiveValue::Set(r.created_at.clone()),
        updated_at: ActiveValue::Set(r.updated_at.clone()),
        html_url: ActiveValue::Set(r.html_url.clone()),
        position: ActiveValue::Set(r.position),
        original_position: ActiveValue::Set(r.original_position),
        line: ActiveValue::Set(r.line),
        original_line: ActiveValue::Set(r.original_line),
        start_line: ActiveValue::Set(r.start_line),
        original_start_line: ActiveValue::Set(r.original_start_line),
        side: ActiveValue::Set(r.side.clone()),
        start_side: ActiveValue::Set(r.start_side.clone()),
        subject_type: ActiveValue::Set(r.subject_type.clone()),
        pull_request_review_id: ActiveValue::Set(r.pull_request_review_id),
        extracted_at: ActiveValue::Set(Some(Utc::now())),
    }
}

#[async_trait]
impl ReviewCommentRepository for SeaOrmReviewCommentRepository {
    async fn delete_stale(
        &self,
        scope: &AccessScope,
        repo_id: i64,
        extracted_before: DateTimeUtc,
    ) -> Result<u64, DomainError> {
        let conn = self.db.conn()?;
        review_comment_delete_stale_in(&conn, scope, repo_id, extracted_before).await
    }

    async fn upsert(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        record: ReviewCommentRecord,
    ) -> Result<ReviewComment, DomainError> {
        let conn = self.db.conn()?;
        review_comment_upsert_in(&conn, scope, tenant_id, record).await
    }

    async fn list_by_pull(
        &self,
        scope: &AccessScope,
        repo_id: i64,
        pull_number: i64,
        window: PageWindow,
    ) -> Result<Vec<ReviewComment>, DomainError> {
        let conn = self.db.conn()?;
        review_comment_list_by_pull_in(&conn, scope, repo_id, pull_number, window).await
    }
}

pub struct SeaOrmReviewRepository {
    db: Arc<DbProvider>,
}

impl SeaOrmReviewRepository {
    #[must_use]
    pub fn new(db: Arc<DbProvider>) -> Self {
        Self { db }
    }
}

fn review_active_model(tenant_id: Uuid, r: &ReviewRecord) -> reviews::ActiveModel {
    reviews::ActiveModel {
        tenant_id: ActiveValue::Set(tenant_id),
        id: ActiveValue::Set(r.id),
        repo_id: ActiveValue::Set(r.repo_id),
        pull_number: ActiveValue::Set(r.pull_number),
        author_login: ActiveValue::Set(r.author_login.clone()),
        state: ActiveValue::Set(r.state.clone()),
        body: ActiveValue::Set(r.body.clone()),
        commit_id: ActiveValue::Set(r.commit_id.clone()),
        submitted_at: ActiveValue::Set(r.submitted_at.clone()),
        html_url: ActiveValue::Set(r.html_url.clone()),
        extracted_at: ActiveValue::Set(Some(Utc::now())),
    }
}

#[async_trait]
impl ReviewRepository for SeaOrmReviewRepository {
    async fn upsert(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        record: ReviewRecord,
    ) -> Result<Review, DomainError> {
        let conn = self.db.conn()?;
        review_upsert_in(&conn, scope, tenant_id, record).await
    }

    async fn list_by_pull(
        &self,
        scope: &AccessScope,
        repo_id: i64,
        pull_number: i64,
        window: PageWindow,
    ) -> Result<Vec<Review>, DomainError> {
        let conn = self.db.conn()?;
        review_list_by_pull_in(&conn, scope, repo_id, pull_number, window).await
    }
}

pub struct SeaOrmLabelRepository {
    db: Arc<DbProvider>,
}

impl SeaOrmLabelRepository {
    #[must_use]
    pub fn new(db: Arc<DbProvider>) -> Self {
        Self { db }
    }
}

fn label_active_model(tenant_id: Uuid, r: &LabelRecord) -> labels::ActiveModel {
    labels::ActiveModel {
        tenant_id: ActiveValue::Set(tenant_id),
        id: ActiveValue::Set(r.id),
        repo_id: ActiveValue::Set(r.repo_id),
        name: ActiveValue::Set(r.name.clone()),
        color: ActiveValue::Set(r.color.clone()),
        is_default: ActiveValue::Set(r.is_default),
        description: ActiveValue::Set(r.description.clone()),
        extracted_at: ActiveValue::Set(Some(Utc::now())),
    }
}

#[async_trait]
impl LabelRepository for SeaOrmLabelRepository {
    async fn delete_stale(
        &self,
        scope: &AccessScope,
        repo_id: i64,
        extracted_before: DateTimeUtc,
    ) -> Result<u64, DomainError> {
        let conn = self.db.conn()?;
        label_delete_stale_in(&conn, scope, repo_id, extracted_before).await
    }

    async fn upsert(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        record: LabelRecord,
    ) -> Result<Label, DomainError> {
        let conn = self.db.conn()?;
        label_upsert_in(&conn, scope, tenant_id, record).await
    }

    async fn list_by_repo(
        &self,
        scope: &AccessScope,
        repo_id: i64,
        window: PageWindow,
    ) -> Result<Vec<Label>, DomainError> {
        let conn = self.db.conn()?;
        label_list_by_repo_in(&conn, scope, repo_id, window).await
    }
}

pub struct SeaOrmMilestoneRepository {
    db: Arc<DbProvider>,
}

impl SeaOrmMilestoneRepository {
    #[must_use]
    pub fn new(db: Arc<DbProvider>) -> Self {
        Self { db }
    }
}

fn milestone_active_model(tenant_id: Uuid, r: &MilestoneRecord) -> milestones::ActiveModel {
    milestones::ActiveModel {
        tenant_id: ActiveValue::Set(tenant_id),
        id: ActiveValue::Set(r.id),
        repo_id: ActiveValue::Set(r.repo_id),
        number: ActiveValue::Set(r.number),
        title: ActiveValue::Set(r.title.clone()),
        state: ActiveValue::Set(r.state.clone()),
        description: ActiveValue::Set(r.description.clone()),
        open_issues: ActiveValue::Set(r.open_issues),
        closed_issues: ActiveValue::Set(r.closed_issues),
        due_on: ActiveValue::Set(r.due_on.clone()),
        created_at: ActiveValue::Set(r.created_at.clone()),
        updated_at: ActiveValue::Set(r.updated_at.clone()),
        closed_at: ActiveValue::Set(r.closed_at.clone()),
        html_url: ActiveValue::Set(r.html_url.clone()),
        extracted_at: ActiveValue::Set(Some(Utc::now())),
    }
}

#[async_trait]
impl MilestoneRepository for SeaOrmMilestoneRepository {
    async fn delete_stale(
        &self,
        scope: &AccessScope,
        repo_id: i64,
        extracted_before: DateTimeUtc,
    ) -> Result<u64, DomainError> {
        let conn = self.db.conn()?;
        milestone_delete_stale_in(&conn, scope, repo_id, extracted_before).await
    }

    async fn upsert(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        record: MilestoneRecord,
    ) -> Result<Milestone, DomainError> {
        let conn = self.db.conn()?;
        milestone_upsert_in(&conn, scope, tenant_id, record).await
    }

    async fn list_by_repo(
        &self,
        scope: &AccessScope,
        repo_id: i64,
        window: PageWindow,
    ) -> Result<Vec<Milestone>, DomainError> {
        let conn = self.db.conn()?;
        milestone_list_by_repo_in(&conn, scope, repo_id, window).await
    }
}

pub struct SeaOrmReleaseRepository {
    db: Arc<DbProvider>,
}

impl SeaOrmReleaseRepository {
    #[must_use]
    pub fn new(db: Arc<DbProvider>) -> Self {
        Self { db }
    }
}

fn release_active_model(tenant_id: Uuid, r: &ReleaseRecord) -> releases::ActiveModel {
    releases::ActiveModel {
        tenant_id: ActiveValue::Set(tenant_id),
        id: ActiveValue::Set(r.id),
        repo_id: ActiveValue::Set(r.repo_id),
        tag_name: ActiveValue::Set(r.tag_name.clone()),
        name: ActiveValue::Set(r.name.clone()),
        draft: ActiveValue::Set(r.draft),
        prerelease: ActiveValue::Set(r.prerelease),
        body: ActiveValue::Set(r.body.clone()),
        author_login: ActiveValue::Set(r.author_login.clone()),
        created_at: ActiveValue::Set(r.created_at.clone()),
        published_at: ActiveValue::Set(r.published_at.clone()),
        html_url: ActiveValue::Set(r.html_url.clone()),
        assets_json: ActiveValue::Set(r.assets_json.clone()),
        extracted_at: ActiveValue::Set(Some(Utc::now())),
    }
}

#[async_trait]
impl ReleaseRepository for SeaOrmReleaseRepository {
    async fn delete_stale(
        &self,
        scope: &AccessScope,
        repo_id: i64,
        extracted_before: DateTimeUtc,
    ) -> Result<u64, DomainError> {
        let conn = self.db.conn()?;
        release_delete_stale_in(&conn, scope, repo_id, extracted_before).await
    }

    async fn upsert(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        record: ReleaseRecord,
    ) -> Result<Release, DomainError> {
        let conn = self.db.conn()?;
        release_upsert_in(&conn, scope, tenant_id, record).await
    }

    async fn list_by_repo(
        &self,
        scope: &AccessScope,
        repo_id: i64,
        window: PageWindow,
    ) -> Result<Vec<Release>, DomainError> {
        let conn = self.db.conn()?;
        release_list_by_repo_in(&conn, scope, repo_id, window).await
    }
}

pub struct SeaOrmBranchRepository {
    db: Arc<DbProvider>,
}

impl SeaOrmBranchRepository {
    #[must_use]
    pub fn new(db: Arc<DbProvider>) -> Self {
        Self { db }
    }
}

fn branch_active_model(tenant_id: Uuid, r: &BranchRecord) -> branches::ActiveModel {
    branches::ActiveModel {
        tenant_id: ActiveValue::Set(tenant_id),
        repo_id: ActiveValue::Set(r.repo_id),
        name: ActiveValue::Set(r.name.clone()),
        commit_sha: ActiveValue::Set(r.commit_sha.clone()),
        protected: ActiveValue::Set(r.protected),
        extracted_at: ActiveValue::Set(Some(Utc::now())),
    }
}

#[async_trait]
impl BranchRepository for SeaOrmBranchRepository {
    async fn delete_stale(
        &self,
        scope: &AccessScope,
        repo_id: i64,
        extracted_before: DateTimeUtc,
    ) -> Result<u64, DomainError> {
        let conn = self.db.conn()?;
        branch_delete_stale_in(&conn, scope, repo_id, extracted_before).await
    }

    async fn upsert(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        record: BranchRecord,
    ) -> Result<Branch, DomainError> {
        let conn = self.db.conn()?;
        branch_upsert_in(&conn, scope, tenant_id, record).await
    }

    async fn list_by_repo(
        &self,
        scope: &AccessScope,
        repo_id: i64,
        window: PageWindow,
    ) -> Result<Vec<Branch>, DomainError> {
        let conn = self.db.conn()?;
        branch_list_by_repo_in(&conn, scope, repo_id, window).await
    }
}

pub struct SeaOrmContributorRepository {
    db: Arc<DbProvider>,
}

impl SeaOrmContributorRepository {
    #[must_use]
    pub fn new(db: Arc<DbProvider>) -> Self {
        Self { db }
    }
}

fn contributor_active_model(tenant_id: Uuid, r: &ContributorRecord) -> contributors::ActiveModel {
    contributors::ActiveModel {
        tenant_id: ActiveValue::Set(tenant_id),
        repo_id: ActiveValue::Set(r.repo_id),
        user_id: ActiveValue::Set(r.user_id),
        // The column stays NOT NULL; anonymous contributors store ''.
        login: ActiveValue::Set(r.login.clone().unwrap_or_default()),
        account_type: ActiveValue::Set(r.account_type.clone()),
        avatar_url: ActiveValue::Set(r.avatar_url.clone()),
        html_url: ActiveValue::Set(r.html_url.clone()),
        // Comma-separated, as DESIGN's contributors table specifies; the
        // role names are fixed identifiers, so nothing needs escaping.
        roles: ActiveValue::Set(Some(r.roles.join(","))),
        first_seen_at: ActiveValue::Set(r.first_seen_at),
        last_seen_at: ActiveValue::Set(r.last_seen_at),
        extracted_at: ActiveValue::Set(Some(Utc::now())),
    }
}

#[async_trait]
impl ContributorRepository for SeaOrmContributorRepository {
    async fn upsert(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        record: ContributorRecord,
    ) -> Result<Contributor, DomainError> {
        let conn = self.db.conn()?;
        contributor_upsert_in(&conn, scope, tenant_id, record).await
    }

    async fn list_by_repo(
        &self,
        scope: &AccessScope,
        repo_id: i64,
        window: PageWindow,
    ) -> Result<Vec<Contributor>, DomainError> {
        let conn = self.db.conn()?;
        contributor_list_by_repo_in(&conn, scope, repo_id, window).await
    }
}

pub struct SeaOrmWorkflowRunRepository {
    db: Arc<DbProvider>,
}

impl SeaOrmWorkflowRunRepository {
    #[must_use]
    pub fn new(db: Arc<DbProvider>) -> Self {
        Self { db }
    }
}

fn workflow_run_active_model(tenant_id: Uuid, r: &WorkflowRunRecord) -> workflow_runs::ActiveModel {
    workflow_runs::ActiveModel {
        tenant_id: ActiveValue::Set(tenant_id),
        id: ActiveValue::Set(r.id),
        repo_id: ActiveValue::Set(r.repo_id),
        workflow_id: ActiveValue::Set(r.workflow_id),
        run_number: ActiveValue::Set(r.run_number),
        run_attempt: ActiveValue::Set(r.run_attempt),
        name: ActiveValue::Set(r.name.clone()),
        event: ActiveValue::Set(r.event.clone()),
        status: ActiveValue::Set(r.status.clone()),
        conclusion: ActiveValue::Set(r.conclusion.clone()),
        head_branch: ActiveValue::Set(r.head_branch.clone()),
        head_sha: ActiveValue::Set(r.head_sha.clone()),
        created_at: ActiveValue::Set(r.created_at.clone()),
        updated_at: ActiveValue::Set(r.updated_at.clone()),
        html_url: ActiveValue::Set(r.html_url.clone()),
        actor_login: ActiveValue::Set(r.actor_login.clone()),
        extracted_at: ActiveValue::Set(Some(Utc::now())),
    }
}

#[async_trait]
impl WorkflowRunRepository for SeaOrmWorkflowRunRepository {
    async fn page_by_repo(
        &self,
        scope: &AccessScope,
        repo_id: i64,
        window: PageWindow,
    ) -> Result<(Vec<WorkflowRun>, u64), DomainError> {
        let scope = scope.clone();
        self.db
            .db()
            .transaction_ref_mapped(move |tx| {
                Box::pin(async move {
                    let items = workflow_run_list_by_repo_in(tx, &scope, repo_id, window).await?;
                    let total = workflow_run_count_by_repo_in(tx, &scope, repo_id).await?;
                    Ok((items, total))
                })
            })
            .await
    }

    async fn upsert(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        record: WorkflowRunRecord,
    ) -> Result<WorkflowRun, DomainError> {
        let conn = self.db.conn()?;
        workflow_run_upsert_in(&conn, scope, tenant_id, record).await
    }

    async fn list_by_repo(
        &self,
        scope: &AccessScope,
        repo_id: i64,
        window: PageWindow,
    ) -> Result<Vec<WorkflowRun>, DomainError> {
        let conn = self.db.conn()?;
        workflow_run_list_by_repo_in(&conn, scope, repo_id, window).await
    }
}

pub struct SeaOrmPullRequestFileRepository {
    db: Arc<DbProvider>,
}

impl SeaOrmPullRequestFileRepository {
    #[must_use]
    pub fn new(db: Arc<DbProvider>) -> Self {
        Self { db }
    }
}

fn pull_request_file_active_model(
    tenant_id: Uuid,
    r: &PullRequestFileRecord,
) -> pull_request_files::ActiveModel {
    pull_request_files::ActiveModel {
        tenant_id: ActiveValue::Set(tenant_id),
        repo_id: ActiveValue::Set(r.repo_id),
        pull_number: ActiveValue::Set(r.pull_number),
        filename: ActiveValue::Set(r.filename.clone()),
        status: ActiveValue::Set(r.status.clone()),
        additions: ActiveValue::Set(r.additions),
        deletions: ActiveValue::Set(r.deletions),
        changes: ActiveValue::Set(r.changes),
        previous_filename: ActiveValue::Set(r.previous_filename.clone()),
        patch: ActiveValue::Set(r.patch.clone()),
        sha: ActiveValue::Set(r.sha.clone()),
        extracted_at: ActiveValue::Set(Some(Utc::now())),
    }
}

#[async_trait]
impl PullRequestFileRepository for SeaOrmPullRequestFileRepository {
    async fn upsert(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        record: PullRequestFileRecord,
    ) -> Result<PullRequestFile, DomainError> {
        let conn = self.db.conn()?;
        pull_request_file_upsert_in(&conn, scope, tenant_id, record).await
    }

    async fn list_by_pull(
        &self,
        scope: &AccessScope,
        repo_id: i64,
        pull_number: i64,
        window: PageWindow,
    ) -> Result<Vec<PullRequestFile>, DomainError> {
        let conn = self.db.conn()?;
        pull_request_file_list_by_pull_in(&conn, scope, repo_id, pull_number, window).await
    }
}

pub struct SeaOrmTagRepository {
    db: Arc<DbProvider>,
}

impl SeaOrmTagRepository {
    #[must_use]
    pub fn new(db: Arc<DbProvider>) -> Self {
        Self { db }
    }
}

fn tag_active_model(tenant_id: Uuid, r: &TagRecord) -> tags::ActiveModel {
    tags::ActiveModel {
        tenant_id: ActiveValue::Set(tenant_id),
        repo_id: ActiveValue::Set(r.repo_id),
        name: ActiveValue::Set(r.name.clone()),
        commit_sha: ActiveValue::Set(r.commit_sha.clone()),
        extracted_at: ActiveValue::Set(Some(Utc::now())),
    }
}

#[async_trait]
impl TagRepository for SeaOrmTagRepository {
    async fn delete_stale(
        &self,
        scope: &AccessScope,
        repo_id: i64,
        extracted_before: DateTimeUtc,
    ) -> Result<u64, DomainError> {
        let conn = self.db.conn()?;
        tag_delete_stale_in(&conn, scope, repo_id, extracted_before).await
    }

    async fn upsert(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        record: TagRecord,
    ) -> Result<Tag, DomainError> {
        let conn = self.db.conn()?;
        tag_upsert_in(&conn, scope, tenant_id, record).await
    }

    async fn list_by_repo(
        &self,
        scope: &AccessScope,
        repo_id: i64,
        window: PageWindow,
    ) -> Result<Vec<Tag>, DomainError> {
        let conn = self.db.conn()?;
        tag_list_by_repo_in(&conn, scope, repo_id, window).await
    }
}

pub struct SeaOrmCommitFileRepository {
    db: Arc<DbProvider>,
}

impl SeaOrmCommitFileRepository {
    #[must_use]
    pub fn new(db: Arc<DbProvider>) -> Self {
        Self { db }
    }
}

fn commit_file_active_model(tenant_id: Uuid, r: &CommitFileRecord) -> commit_files::ActiveModel {
    commit_files::ActiveModel {
        tenant_id: ActiveValue::Set(tenant_id),
        repo_id: ActiveValue::Set(r.repo_id),
        commit_sha: ActiveValue::Set(r.commit_sha.clone()),
        filename: ActiveValue::Set(r.filename.clone()),
        status: ActiveValue::Set(r.status.clone()),
        additions: ActiveValue::Set(r.additions),
        deletions: ActiveValue::Set(r.deletions),
        changes: ActiveValue::Set(r.changes),
        previous_filename: ActiveValue::Set(r.previous_filename.clone()),
        sha: ActiveValue::Set(r.sha.clone()),
        extracted_at: ActiveValue::Set(Some(Utc::now())),
    }
}

#[async_trait]
impl CommitFileRepository for SeaOrmCommitFileRepository {
    async fn upsert(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        record: CommitFileRecord,
    ) -> Result<CommitFile, DomainError> {
        let conn = self.db.conn()?;
        commit_file_upsert_in(&conn, scope, tenant_id, record).await
    }

    async fn list_by_commit(
        &self,
        scope: &AccessScope,
        repo_id: i64,
        commit_sha: &str,
        query: &ODataQuery,
    ) -> Result<Page<CommitFile>, DomainError> {
        let conn = self.db.conn()?;
        commit_file_list_by_commit_in(&conn, scope, repo_id, commit_sha, query).await
    }
}

pub struct SeaOrmReviewThreadRepository {
    db: Arc<DbProvider>,
}

impl SeaOrmReviewThreadRepository {
    #[must_use]
    pub fn new(db: Arc<DbProvider>) -> Self {
        Self { db }
    }
}

fn review_thread_active_model(
    tenant_id: Uuid,
    r: &ReviewThreadRecord,
) -> review_threads::ActiveModel {
    review_threads::ActiveModel {
        tenant_id: ActiveValue::Set(tenant_id),
        id: ActiveValue::Set(r.id.clone()),
        repo_id: ActiveValue::Set(r.repo_id),
        pull_number: ActiveValue::Set(r.pull_number),
        is_resolved: ActiveValue::Set(r.is_resolved),
        is_outdated: ActiveValue::Set(r.is_outdated),
        path: ActiveValue::Set(r.path.clone()),
        line: ActiveValue::Set(r.line),
        resolved_by: ActiveValue::Set(r.resolved_by.clone()),
        comments_count: ActiveValue::Set(r.comments_count),
        extracted_at: ActiveValue::Set(Some(Utc::now())),
    }
}

#[async_trait]
impl ReviewThreadRepository for SeaOrmReviewThreadRepository {
    async fn upsert(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        record: ReviewThreadRecord,
    ) -> Result<ReviewThread, DomainError> {
        let conn = self.db.conn()?;
        review_thread_upsert_in(&conn, scope, tenant_id, record).await
    }

    async fn list_by_pull(
        &self,
        scope: &AccessScope,
        repo_id: i64,
        pull_number: i64,
        query: &ODataQuery,
    ) -> Result<Page<ReviewThread>, DomainError> {
        let conn = self.db.conn()?;
        review_thread_list_by_pull_in(&conn, scope, repo_id, pull_number, query).await
    }
}

pub struct SeaOrmCommitCommentRepository {
    db: Arc<DbProvider>,
}

impl SeaOrmCommitCommentRepository {
    #[must_use]
    pub fn new(db: Arc<DbProvider>) -> Self {
        Self { db }
    }
}

fn commit_comment_active_model(
    tenant_id: Uuid,
    r: &CommitCommentRecord,
) -> commit_comments::ActiveModel {
    commit_comments::ActiveModel {
        tenant_id: ActiveValue::Set(tenant_id),
        id: ActiveValue::Set(r.id),
        repo_id: ActiveValue::Set(r.repo_id),
        commit_sha: ActiveValue::Set(r.commit_sha.clone()),
        path: ActiveValue::Set(r.path.clone()),
        position: ActiveValue::Set(r.position),
        author_login: ActiveValue::Set(r.author_login.clone()),
        body: ActiveValue::Set(r.body.clone()),
        created_at: ActiveValue::Set(r.created_at.clone()),
        updated_at: ActiveValue::Set(r.updated_at.clone()),
        html_url: ActiveValue::Set(r.html_url.clone()),
        extracted_at: ActiveValue::Set(Some(Utc::now())),
    }
}

#[async_trait]
impl CommitCommentRepository for SeaOrmCommitCommentRepository {
    async fn upsert(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        record: CommitCommentRecord,
    ) -> Result<CommitComment, DomainError> {
        let conn = self.db.conn()?;
        commit_comment_upsert_in(&conn, scope, tenant_id, record).await
    }

    async fn list_by_commit(
        &self,
        scope: &AccessScope,
        repo_id: i64,
        commit_sha: &str,
        window: PageWindow,
    ) -> Result<Vec<CommitComment>, DomainError> {
        let conn = self.db.conn()?;
        commit_comment_list_by_commit_in(&conn, scope, repo_id, commit_sha, window).await
    }
}

pub struct SeaOrmIssueEventRepository {
    db: Arc<DbProvider>,
}

impl SeaOrmIssueEventRepository {
    #[must_use]
    pub fn new(db: Arc<DbProvider>) -> Self {
        Self { db }
    }
}

fn issue_event_active_model(tenant_id: Uuid, r: &IssueEventRecord) -> issue_events::ActiveModel {
    issue_events::ActiveModel {
        tenant_id: ActiveValue::Set(tenant_id),
        id: ActiveValue::Set(r.id),
        repo_id: ActiveValue::Set(r.repo_id),
        issue_number: ActiveValue::Set(r.issue_number),
        event: ActiveValue::Set(r.event.clone()),
        actor_login: ActiveValue::Set(r.actor_login.clone()),
        label_name: ActiveValue::Set(r.label_name.clone()),
        assignee_login: ActiveValue::Set(r.assignee_login.clone()),
        milestone_title: ActiveValue::Set(r.milestone_title.clone()),
        commit_id: ActiveValue::Set(r.commit_id.clone()),
        created_at: ActiveValue::Set(r.created_at.clone()),
        extracted_at: ActiveValue::Set(Some(Utc::now())),
    }
}

#[async_trait]
impl IssueEventRepository for SeaOrmIssueEventRepository {
    async fn upsert(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        record: IssueEventRecord,
    ) -> Result<IssueEvent, DomainError> {
        let conn = self.db.conn()?;
        issue_event_upsert_in(&conn, scope, tenant_id, record).await
    }

    async fn list_by_issue(
        &self,
        scope: &AccessScope,
        repo_id: i64,
        issue_number: i64,
        window: PageWindow,
    ) -> Result<Vec<IssueEvent>, DomainError> {
        let conn = self.db.conn()?;
        issue_event_list_by_issue_in(&conn, scope, repo_id, issue_number, window).await
    }
}

pub struct SeaOrmDeploymentRepository {
    db: Arc<DbProvider>,
}

impl SeaOrmDeploymentRepository {
    #[must_use]
    pub fn new(db: Arc<DbProvider>) -> Self {
        Self { db }
    }
}

fn deployment_active_model(tenant_id: Uuid, r: &DeploymentRecord) -> deployments::ActiveModel {
    deployments::ActiveModel {
        tenant_id: ActiveValue::Set(tenant_id),
        id: ActiveValue::Set(r.id),
        repo_id: ActiveValue::Set(r.repo_id),
        git_ref: ActiveValue::Set(r.git_ref.clone()),
        sha: ActiveValue::Set(r.sha.clone()),
        environment: ActiveValue::Set(r.environment.clone()),
        task: ActiveValue::Set(r.task.clone()),
        description: ActiveValue::Set(r.description.clone()),
        creator_login: ActiveValue::Set(r.creator_login.clone()),
        created_at: ActiveValue::Set(r.created_at.clone()),
        updated_at: ActiveValue::Set(r.updated_at.clone()),
        extracted_at: ActiveValue::Set(Some(Utc::now())),
    }
}

#[async_trait]
impl DeploymentRepository for SeaOrmDeploymentRepository {
    async fn upsert(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        record: DeploymentRecord,
    ) -> Result<Deployment, DomainError> {
        let conn = self.db.conn()?;
        deployment_upsert_in(&conn, scope, tenant_id, record).await
    }

    async fn list_by_repo(
        &self,
        scope: &AccessScope,
        repo_id: i64,
        window: PageWindow,
    ) -> Result<Vec<Deployment>, DomainError> {
        let conn = self.db.conn()?;
        deployment_list_by_repo_in(&conn, scope, repo_id, window).await
    }
}

pub struct SeaOrmPullRequestCommitRepository {
    db: Arc<DbProvider>,
}

impl SeaOrmPullRequestCommitRepository {
    #[must_use]
    pub fn new(db: Arc<DbProvider>) -> Self {
        Self { db }
    }
}

fn pull_request_commit_active_model(
    tenant_id: Uuid,
    r: &PullRequestCommitRecord,
) -> pull_request_commits::ActiveModel {
    pull_request_commits::ActiveModel {
        tenant_id: ActiveValue::Set(tenant_id),
        repo_id: ActiveValue::Set(r.repo_id),
        pull_number: ActiveValue::Set(r.pull_number),
        sha: ActiveValue::Set(r.sha.clone()),
        message: ActiveValue::Set(r.message.clone()),
        author_login: ActiveValue::Set(r.author_login.clone()),
        committer_login: ActiveValue::Set(r.committer_login.clone()),
        authored_at: ActiveValue::Set(r.authored_at.clone()),
        committed_at: ActiveValue::Set(r.committed_at.clone()),
        extracted_at: ActiveValue::Set(Some(Utc::now())),
    }
}

#[async_trait]
impl PullRequestCommitRepository for SeaOrmPullRequestCommitRepository {
    async fn upsert(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        record: PullRequestCommitRecord,
    ) -> Result<PullRequestCommit, DomainError> {
        let conn = self.db.conn()?;
        pull_request_commit_upsert_in(&conn, scope, tenant_id, record).await
    }

    async fn list_by_pull(
        &self,
        scope: &AccessScope,
        repo_id: i64,
        pull_number: i64,
        window: PageWindow,
    ) -> Result<Vec<PullRequestCommit>, DomainError> {
        let conn = self.db.conn()?;
        pull_request_commit_list_by_pull_in(&conn, scope, repo_id, pull_number, window).await
    }
}

pub struct SeaOrmCommitStatusRepository {
    db: Arc<DbProvider>,
}

impl SeaOrmCommitStatusRepository {
    #[must_use]
    pub fn new(db: Arc<DbProvider>) -> Self {
        Self { db }
    }
}

fn commit_status_active_model(
    tenant_id: Uuid,
    r: &CommitStatusRecord,
) -> commit_statuses::ActiveModel {
    commit_statuses::ActiveModel {
        tenant_id: ActiveValue::Set(tenant_id),
        id: ActiveValue::Set(r.id),
        repo_id: ActiveValue::Set(r.repo_id),
        commit_sha: ActiveValue::Set(r.commit_sha.clone()),
        state: ActiveValue::Set(r.state.clone()),
        context: ActiveValue::Set(r.context.clone()),
        description: ActiveValue::Set(r.description.clone()),
        target_url: ActiveValue::Set(r.target_url.clone()),
        creator_login: ActiveValue::Set(r.creator_login.clone()),
        created_at: ActiveValue::Set(r.created_at.clone()),
        updated_at: ActiveValue::Set(r.updated_at.clone()),
        extracted_at: ActiveValue::Set(Some(Utc::now())),
    }
}

#[async_trait]
impl CommitStatusRepository for SeaOrmCommitStatusRepository {
    async fn upsert(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        record: CommitStatusRecord,
    ) -> Result<CommitStatus, DomainError> {
        let conn = self.db.conn()?;
        commit_status_upsert_in(&conn, scope, tenant_id, record).await
    }

    async fn list_by_commit(
        &self,
        scope: &AccessScope,
        repo_id: i64,
        commit_sha: &str,
        window: PageWindow,
    ) -> Result<Vec<CommitStatus>, DomainError> {
        let conn = self.db.conn()?;
        commit_status_list_by_commit_in(&conn, scope, repo_id, commit_sha, window).await
    }
}

pub struct SeaOrmWorkflowJobRepository {
    db: Arc<DbProvider>,
}

impl SeaOrmWorkflowJobRepository {
    #[must_use]
    pub fn new(db: Arc<DbProvider>) -> Self {
        Self { db }
    }
}

fn workflow_job_active_model(tenant_id: Uuid, r: &WorkflowJobRecord) -> workflow_jobs::ActiveModel {
    workflow_jobs::ActiveModel {
        tenant_id: ActiveValue::Set(tenant_id),
        id: ActiveValue::Set(r.id),
        repo_id: ActiveValue::Set(r.repo_id),
        run_id: ActiveValue::Set(r.run_id),
        run_attempt: ActiveValue::Set(r.run_attempt),
        name: ActiveValue::Set(r.name.clone()),
        status: ActiveValue::Set(r.status.clone()),
        conclusion: ActiveValue::Set(r.conclusion.clone()),
        head_sha: ActiveValue::Set(r.head_sha.clone()),
        runner_name: ActiveValue::Set(r.runner_name.clone()),
        started_at: ActiveValue::Set(r.started_at.clone()),
        completed_at: ActiveValue::Set(r.completed_at.clone()),
        html_url: ActiveValue::Set(r.html_url.clone()),
        steps_json: ActiveValue::Set(r.steps_json.clone()),
        extracted_at: ActiveValue::Set(Some(Utc::now())),
    }
}

#[async_trait]
impl WorkflowJobRepository for SeaOrmWorkflowJobRepository {
    async fn page_by_run(
        &self,
        scope: &AccessScope,
        repo_id: i64,
        run_id: i64,
        window: PageWindow,
    ) -> Result<(Vec<WorkflowJob>, u64), DomainError> {
        let scope = scope.clone();
        self.db
            .db()
            .transaction_ref_mapped(move |tx| {
                Box::pin(async move {
                    let items =
                        workflow_job_list_by_run_in(tx, &scope, repo_id, run_id, window).await?;
                    let total = workflow_job_count_by_run_in(tx, &scope, repo_id, run_id).await?;
                    Ok((items, total))
                })
            })
            .await
    }

    async fn upsert(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        record: WorkflowJobRecord,
    ) -> Result<WorkflowJob, DomainError> {
        let conn = self.db.conn()?;
        workflow_job_upsert_in(&conn, scope, tenant_id, record).await
    }

    async fn list_by_run(
        &self,
        scope: &AccessScope,
        repo_id: i64,
        run_id: i64,
        window: PageWindow,
    ) -> Result<Vec<WorkflowJob>, DomainError> {
        let conn = self.db.conn()?;
        workflow_job_list_by_run_in(&conn, scope, repo_id, run_id, window).await
    }
}

pub struct SeaOrmIssueReactionRepository {
    db: Arc<DbProvider>,
}

impl SeaOrmIssueReactionRepository {
    #[must_use]
    pub fn new(db: Arc<DbProvider>) -> Self {
        Self { db }
    }
}

fn issue_reaction_active_model(
    tenant_id: Uuid,
    r: &IssueReactionRecord,
) -> issue_reactions::ActiveModel {
    issue_reactions::ActiveModel {
        tenant_id: ActiveValue::Set(tenant_id),
        id: ActiveValue::Set(r.id),
        repo_id: ActiveValue::Set(r.repo_id),
        issue_number: ActiveValue::Set(r.issue_number),
        content: ActiveValue::Set(r.content.clone()),
        user_login: ActiveValue::Set(r.user_login.clone()),
        created_at: ActiveValue::Set(r.created_at.clone()),
        extracted_at: ActiveValue::Set(Some(Utc::now())),
    }
}

#[async_trait]
impl IssueReactionRepository for SeaOrmIssueReactionRepository {
    async fn upsert(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        record: IssueReactionRecord,
    ) -> Result<IssueReaction, DomainError> {
        let conn = self.db.conn()?;
        issue_reaction_upsert_in(&conn, scope, tenant_id, record).await
    }

    async fn list_by_issue(
        &self,
        scope: &AccessScope,
        repo_id: i64,
        issue_number: i64,
        window: PageWindow,
    ) -> Result<Vec<IssueReaction>, DomainError> {
        let conn = self.db.conn()?;
        issue_reaction_list_by_issue_in(&conn, scope, repo_id, issue_number, window).await
    }
}

pub struct SeaOrmCheckRunRepository {
    db: Arc<DbProvider>,
}

impl SeaOrmCheckRunRepository {
    #[must_use]
    pub fn new(db: Arc<DbProvider>) -> Self {
        Self { db }
    }
}

fn check_run_active_model(tenant_id: Uuid, r: &CheckRunRecord) -> check_runs::ActiveModel {
    check_runs::ActiveModel {
        tenant_id: ActiveValue::Set(tenant_id),
        id: ActiveValue::Set(r.id),
        repo_id: ActiveValue::Set(r.repo_id),
        head_sha: ActiveValue::Set(r.head_sha.clone()),
        name: ActiveValue::Set(r.name.clone()),
        status: ActiveValue::Set(r.status.clone()),
        conclusion: ActiveValue::Set(r.conclusion.clone()),
        started_at: ActiveValue::Set(r.started_at.clone()),
        completed_at: ActiveValue::Set(r.completed_at.clone()),
        html_url: ActiveValue::Set(r.html_url.clone()),
        details_url: ActiveValue::Set(r.details_url.clone()),
        check_suite_id: ActiveValue::Set(r.check_suite_id),
        app_slug: ActiveValue::Set(r.app_slug.clone()),
        app_name: ActiveValue::Set(r.app_name.clone()),
        output_title: ActiveValue::Set(r.output_title.clone()),
        output_summary: ActiveValue::Set(r.output_summary.clone()),
        annotations_count: ActiveValue::Set(r.annotations_count),
        extracted_at: ActiveValue::Set(Some(Utc::now())),
    }
}

#[async_trait]
impl CheckRunRepository for SeaOrmCheckRunRepository {
    async fn page_by_commit(
        &self,
        scope: &AccessScope,
        repo_id: i64,
        head_sha: &str,
        window: PageWindow,
    ) -> Result<(Vec<CheckRun>, u64), DomainError> {
        let scope = scope.clone();
        let head_sha = head_sha.to_owned();
        self.db
            .db()
            .transaction_ref_mapped(move |tx| {
                Box::pin(async move {
                    let items =
                        check_run_list_by_commit_in(tx, &scope, repo_id, &head_sha, window).await?;
                    let total =
                        check_run_count_by_commit_in(tx, &scope, repo_id, &head_sha).await?;
                    Ok((items, total))
                })
            })
            .await
    }

    async fn upsert(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        record: CheckRunRecord,
    ) -> Result<CheckRun, DomainError> {
        let conn = self.db.conn()?;
        check_run_upsert_in(&conn, scope, tenant_id, record).await
    }

    async fn list_by_commit(
        &self,
        scope: &AccessScope,
        repo_id: i64,
        head_sha: &str,
        window: PageWindow,
    ) -> Result<Vec<CheckRun>, DomainError> {
        let conn = self.db.conn()?;
        check_run_list_by_commit_in(&conn, scope, repo_id, head_sha, window).await
    }
}

pub struct SeaOrmIssueTimelineRepository {
    db: Arc<DbProvider>,
}

impl SeaOrmIssueTimelineRepository {
    #[must_use]
    pub fn new(db: Arc<DbProvider>) -> Self {
        Self { db }
    }
}

fn issue_timeline_active_model(
    tenant_id: Uuid,
    r: &IssueTimelineEventRecord,
) -> issue_timeline::ActiveModel {
    issue_timeline::ActiveModel {
        tenant_id: ActiveValue::Set(tenant_id),
        repo_id: ActiveValue::Set(r.repo_id),
        issue_number: ActiveValue::Set(r.issue_number),
        position: ActiveValue::Set(r.position),
        event: ActiveValue::Set(r.event.clone()),
        created_at: ActiveValue::Set(r.created_at.clone()),
        actor_login: ActiveValue::Set(r.actor_login.clone()),
        payload_json: ActiveValue::Set(r.payload_json.clone()),
        extracted_at: ActiveValue::Set(Some(Utc::now())),
    }
}

#[async_trait]
impl IssueTimelineRepository for SeaOrmIssueTimelineRepository {
    async fn delete_by_issues(
        &self,
        scope: &AccessScope,
        repo_id: i64,
        issue_numbers: &[i64],
    ) -> Result<u64, DomainError> {
        let conn = self.db.conn()?;
        issue_timeline_delete_by_issues_in(&conn, scope, repo_id, issue_numbers).await
    }

    async fn upsert(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        record: IssueTimelineEventRecord,
    ) -> Result<IssueTimelineEvent, DomainError> {
        let conn = self.db.conn()?;
        issue_timeline_upsert_in(&conn, scope, tenant_id, record).await
    }

    async fn list_by_issue(
        &self,
        scope: &AccessScope,
        repo_id: i64,
        issue_number: i64,
        window: PageWindow,
    ) -> Result<Vec<IssueTimelineEvent>, DomainError> {
        let conn = self.db.conn()?;
        issue_timeline_list_by_issue_in(&conn, scope, repo_id, issue_number, window).await
    }
}

async fn repo_upsert_in<C: DBRunner>(
    conn: &C,
    scope: &AccessScope,
    tenant_id: Uuid,
    record: RepoRecord,
) -> Result<Repo, DomainError> {
    let on_conflict = SecureOnConflict::<RepoEntity>::columns([
        repositories::Column::TenantId,
        repositories::Column::Id,
    ])
    .update_columns([
        repositories::Column::NodeId,
        repositories::Column::Owner,
        repositories::Column::Name,
        repositories::Column::FullName,
        repositories::Column::DefaultBranch,
        repositories::Column::Private,
        repositories::Column::PushedAt,
        repositories::Column::Stars,
        repositories::Column::Forks,
        repositories::Column::Description,
        repositories::Column::CloneUrl,
        repositories::Column::ExtractedAt,
    ])
    .map_err(map_scope_error)?;

    let model = active_model(tenant_id, &record);
    RepoEntity::insert(model.clone())
        .secure()
        .scope_with_model(scope, &model)
        .map_err(map_scope_error)?
        .on_conflict(on_conflict)
        .exec(conn)
        .await
        .map_err(map_scope_error)?;

    Ok(Repo {
        id: record.id,
        node_id: record.node_id,
        owner: record.owner,
        name: record.name,
        full_name: record.full_name,
        default_branch: record.default_branch,
        private: record.private,
        pushed_at: record.pushed_at,
        stars: record.stars,
        forks: record.forks,
        description: record.description,
        clone_url: record.clone_url,
    })
}

async fn repo_list_in<C: DBRunner>(
    conn: &C,
    scope: &AccessScope,
    query: &ODataQuery,
) -> Result<Page<Repo>, DomainError> {
    paginate_odata::<RepoField, RepoODataMapper, _, _, _, _>(
        RepoEntity::find().secure().scope_with(scope),
        conn,
        query,
        ("full_name", SortDir::Asc),
        LIST_LIMITS,
        Into::into,
    )
    .await
    .map_err(map_odata_error)
}

async fn repo_list_window_in<C: DBRunner>(
    conn: &C,
    scope: &AccessScope,
    window: PageWindow,
) -> Result<Vec<Repo>, DomainError> {
    let rows = RepoEntity::find()
        .secure()
        .scope_with(scope)
        .order_by(repositories::Column::FullName, Order::Asc)
        // Unique tie-break: two rows may share a full name, and equal sort
        // keys must not shuffle between adjacent page windows.
        .order_by(repositories::Column::Id, Order::Asc)
        .limit(window.limit())
        .offset(window.offset())
        .all(conn)
        .await
        .map_err(map_scope_error)?;

    Ok(rows.into_iter().map(Into::into).collect())
}

async fn repo_find_by_full_name_in<C: DBRunner>(
    conn: &C,
    scope: &AccessScope,
    full_name: &str,
) -> Result<Option<Repo>, DomainError> {
    let row = RepoEntity::find()
        .secure()
        .scope_with(scope)
        .filter(sea_orm::Condition::all().add(repositories::Column::FullName.eq(full_name)))
        .one(conn)
        .await
        .map_err(map_scope_error)?;

    Ok(row.map(Into::into))
}

async fn issue_count_by_repo_in<C: DBRunner>(
    conn: &C,
    scope: &AccessScope,
    repo_id: i64,
    filter: ListingFilter,
) -> Result<u64, DomainError> {
    let mut condition = sea_orm::Condition::all().add(issues::Column::RepoId.eq(repo_id));
    if let Some(state) = filter.state {
        condition = condition.add(issues::Column::State.eq(state.as_str()));
    }
    if let Some(since) = filter.since {
        condition = condition.add(issues::Column::UpdatedAt.gte(github_instant(since)));
    }
    IssueEntity::find()
        .secure()
        .scope_with(scope)
        .filter(condition)
        .count(conn)
        .await
        .map_err(map_scope_error)
}

async fn issue_delete_stale_in<C: DBRunner>(
    conn: &C,
    scope: &AccessScope,
    repo_id: i64,
    extracted_before: DateTimeUtc,
) -> Result<u64, DomainError> {
    // RFC3339 strings order lexicographically; the pre-column default ''
    // sorts before any stamp, so unstamped legacy rows count as stale.
    let result = IssueEntity::delete_many()
        .secure()
        .scope_with(scope)
        .filter(
            sea_orm::Condition::all()
                .add(issues::Column::RepoId.eq(repo_id))
                .add(
                    sea_orm::Condition::any()
                        .add(issues::Column::ExtractedAt.lt(extracted_before))
                        .add(issues::Column::ExtractedAt.is_null()),
                ),
        )
        .exec(conn)
        .await
        .map_err(map_scope_error)?;
    Ok(result.rows_affected)
}

async fn issue_upsert_in<C: DBRunner>(
    conn: &C,
    scope: &AccessScope,
    tenant_id: Uuid,
    record: IssueRecord,
) -> Result<Issue, DomainError> {
    let on_conflict =
        SecureOnConflict::<IssueEntity>::columns([issues::Column::TenantId, issues::Column::Id])
            .update_columns([
                issues::Column::AuthorLogin,
                issues::Column::AuthorJson,
                issues::Column::AssigneesJson,
                issues::Column::LabelsJson,
                issues::Column::CommentsCount,
                issues::Column::Locked,
                issues::Column::NodeId,
                issues::Column::RepoId,
                issues::Column::Number,
                issues::Column::Title,
                issues::Column::Body,
                issues::Column::State,
                issues::Column::IsPullRequest,
                issues::Column::CreatedAt,
                issues::Column::UpdatedAt,
                issues::Column::ClosedAt,
                issues::Column::HtmlUrl,
                issues::Column::ExtractedAt,
            ])
            .map_err(map_scope_error)?;

    let model = issue_active_model(tenant_id, &record);
    IssueEntity::insert(model.clone())
        .secure()
        .scope_with_model(scope, &model)
        .map_err(map_scope_error)?
        .on_conflict(on_conflict)
        .exec(conn)
        .await
        .map_err(map_scope_error)?;

    let row = StoredRow::new(record.repo_id, record.number);
    Ok(Issue {
        id: record.id,
        node_id: record.node_id,
        repo_id: record.repo_id,
        number: record.number,
        title: record.title,
        body: record.body,
        state: record.state,
        is_pull_request: record.is_pull_request,
        created_at: record.created_at,
        updated_at: record.updated_at,
        closed_at: record.closed_at,
        html_url: record.html_url,
        author_login: record.author_login,
        author: decode::<StoredActor>("author_json", row, record.author_json.as_deref())
            .map(Into::into),
        assignees: decode_list::<StoredActor, _>(
            "assignees_json",
            row,
            record.assignees_json.as_deref(),
        ),
        labels: decode_list::<StoredLabel, _>("labels_json", row, record.labels_json.as_deref()),
        comments_count: record.comments_count,
        locked: record.locked,
    })
}

async fn issue_list_by_repo_in<C: DBRunner>(
    conn: &C,
    scope: &AccessScope,
    repo_id: i64,
    window: PageWindow,
    filter: ListingFilter,
) -> Result<Vec<Issue>, DomainError> {
    let mut condition = sea_orm::Condition::all().add(issues::Column::RepoId.eq(repo_id));
    if let Some(state) = filter.state {
        condition = condition.add(issues::Column::State.eq(state.as_str()));
    }
    if let Some(since) = filter.since {
        condition = condition.add(issues::Column::UpdatedAt.gte(github_instant(since)));
    }
    let (sort_column, direction) = (
        match filter.sort {
            ListingSort::Created => issues::Column::CreatedAt,
            ListingSort::Updated => issues::Column::UpdatedAt,
        },
        match filter.direction {
            ListingDirection::Asc => Order::Asc,
            ListingDirection::Desc => Order::Desc,
        },
    );
    let rows = IssueEntity::find()
        .secure()
        .scope_with(scope)
        .filter(condition)
        .order_by(sort_column, direction)
        // Unique tie-break: equal sort keys must not shuffle page windows.
        .order_by(issues::Column::Number, Order::Asc)
        .order_by(issues::Column::Id, Order::Asc)
        .limit(window.limit())
        .offset(window.offset())
        .all(conn)
        .await
        .map_err(map_scope_error)?;

    Ok(rows.into_iter().map(Into::into).collect())
}

async fn issue_find_by_number_in<C: DBRunner>(
    conn: &C,
    scope: &AccessScope,
    repo_id: i64,
    number: i64,
) -> Result<Option<Issue>, DomainError> {
    let row = IssueEntity::find()
        .secure()
        .scope_with(scope)
        .filter(
            sea_orm::Condition::all()
                .add(issues::Column::RepoId.eq(repo_id))
                .add(issues::Column::Number.eq(number)),
        )
        .one(conn)
        .await
        .map_err(map_scope_error)?;

    Ok(row.map(Into::into))
}

async fn pull_request_count_by_repo_in<C: DBRunner>(
    conn: &C,
    scope: &AccessScope,
    repo_id: i64,
    filter: ListingFilter,
) -> Result<u64, DomainError> {
    let mut condition = sea_orm::Condition::all().add(pull_requests::Column::RepoId.eq(repo_id));
    if let Some(state) = filter.state {
        condition = condition.add(pull_requests::Column::State.eq(state.as_str()));
    }
    if let Some(since) = filter.since {
        condition = condition.add(pull_requests::Column::UpdatedAt.gte(github_instant(since)));
    }
    PullRequestEntity::find()
        .secure()
        .scope_with(scope)
        .filter(condition)
        .count(conn)
        .await
        .map_err(map_scope_error)
}

async fn pull_request_delete_stale_in<C: DBRunner>(
    conn: &C,
    scope: &AccessScope,
    repo_id: i64,
    extracted_before: DateTimeUtc,
) -> Result<u64, DomainError> {
    // RFC3339 strings order lexicographically; the pre-column default ''
    // sorts before any stamp, so unstamped legacy rows count as stale.
    let result = PullRequestEntity::delete_many()
        .secure()
        .scope_with(scope)
        .filter(
            sea_orm::Condition::all()
                .add(pull_requests::Column::RepoId.eq(repo_id))
                .add(
                    sea_orm::Condition::any()
                        .add(pull_requests::Column::ExtractedAt.lt(extracted_before))
                        .add(pull_requests::Column::ExtractedAt.is_null()),
                ),
        )
        .exec(conn)
        .await
        .map_err(map_scope_error)?;
    Ok(result.rows_affected)
}

async fn pull_request_upsert_in<C: DBRunner>(
    conn: &C,
    scope: &AccessScope,
    tenant_id: Uuid,
    record: PullRequestRecord,
) -> Result<PullRequest, DomainError> {
    let on_conflict = SecureOnConflict::<PullRequestEntity>::columns([
        pull_requests::Column::TenantId,
        pull_requests::Column::Id,
    ])
    .update_columns([
        pull_requests::Column::AuthorLogin,
        pull_requests::Column::AuthorJson,
        pull_requests::Column::AssigneesJson,
        pull_requests::Column::LabelsJson,
        pull_requests::Column::CommentsCount,
        pull_requests::Column::Locked,
        pull_requests::Column::RequestedReviewersJson,
        pull_requests::Column::NodeId,
        pull_requests::Column::RepoId,
        pull_requests::Column::Number,
        pull_requests::Column::Title,
        pull_requests::Column::Body,
        pull_requests::Column::State,
        pull_requests::Column::Draft,
        pull_requests::Column::Merged,
        pull_requests::Column::HeadSha,
        pull_requests::Column::BaseSha,
        pull_requests::Column::LinesAdded,
        pull_requests::Column::LinesRemoved,
        pull_requests::Column::CreatedAt,
        pull_requests::Column::UpdatedAt,
        pull_requests::Column::ClosedAt,
        pull_requests::Column::MergedAt,
        pull_requests::Column::HtmlUrl,
        pull_requests::Column::HeadRef,
        pull_requests::Column::BaseRef,
        pull_requests::Column::ExtractedAt,
    ])
    .map_err(map_scope_error)?;

    let model = pull_request_active_model(tenant_id, &record);
    PullRequestEntity::insert(model.clone())
        .secure()
        .scope_with_model(scope, &model)
        .map_err(map_scope_error)?
        .on_conflict(on_conflict)
        .exec(conn)
        .await
        .map_err(map_scope_error)?;

    let row = StoredRow::new(record.repo_id, record.number);
    Ok(PullRequest {
        id: record.id,
        node_id: record.node_id,
        repo_id: record.repo_id,
        number: record.number,
        title: record.title,
        body: record.body,
        state: record.state,
        draft: record.draft,
        merged: record.merged,
        head_sha: record.head_sha,
        base_sha: record.base_sha,
        lines_added: record.lines_added,
        lines_removed: record.lines_removed,
        created_at: record.created_at,
        updated_at: record.updated_at,
        closed_at: record.closed_at,
        merged_at: record.merged_at,
        html_url: record.html_url,
        head_ref: record.head_ref,
        base_ref: record.base_ref,
        author_login: record.author_login,
        author: decode::<StoredActor>("author_json", row, record.author_json.as_deref())
            .map(Into::into),
        assignees: decode_list::<StoredActor, _>(
            "assignees_json",
            row,
            record.assignees_json.as_deref(),
        ),
        labels: decode_list::<StoredLabel, _>("labels_json", row, record.labels_json.as_deref()),
        comments_count: record.comments_count,
        locked: record.locked,
        requested_reviewers: decode_list::<StoredActor, _>(
            "requested_reviewers_json",
            row,
            record.requested_reviewers_json.as_deref(),
        ),
    })
}

async fn pull_request_list_by_repo_in<C: DBRunner>(
    conn: &C,
    scope: &AccessScope,
    repo_id: i64,
    window: PageWindow,
    filter: ListingFilter,
) -> Result<Vec<PullRequest>, DomainError> {
    let mut condition = sea_orm::Condition::all().add(pull_requests::Column::RepoId.eq(repo_id));
    if let Some(state) = filter.state {
        condition = condition.add(pull_requests::Column::State.eq(state.as_str()));
    }
    if let Some(since) = filter.since {
        condition = condition.add(pull_requests::Column::UpdatedAt.gte(github_instant(since)));
    }
    let (sort_column, direction) = (
        match filter.sort {
            ListingSort::Created => pull_requests::Column::CreatedAt,
            ListingSort::Updated => pull_requests::Column::UpdatedAt,
        },
        match filter.direction {
            ListingDirection::Asc => Order::Asc,
            ListingDirection::Desc => Order::Desc,
        },
    );
    let rows = PullRequestEntity::find()
        .secure()
        .scope_with(scope)
        .filter(condition)
        .order_by(sort_column, direction)
        // Unique tie-break: equal sort keys must not shuffle page windows.
        .order_by(pull_requests::Column::Number, Order::Asc)
        .order_by(pull_requests::Column::Id, Order::Asc)
        .limit(window.limit())
        .offset(window.offset())
        .all(conn)
        .await
        .map_err(map_scope_error)?;

    Ok(rows.into_iter().map(Into::into).collect())
}

async fn pull_request_find_by_number_in<C: DBRunner>(
    conn: &C,
    scope: &AccessScope,
    repo_id: i64,
    number: i64,
) -> Result<Option<PullRequest>, DomainError> {
    let row = PullRequestEntity::find()
        .secure()
        .scope_with(scope)
        .filter(
            sea_orm::Condition::all()
                .add(pull_requests::Column::RepoId.eq(repo_id))
                .add(pull_requests::Column::Number.eq(number)),
        )
        .one(conn)
        .await
        .map_err(map_scope_error)?;

    Ok(row.map(Into::into))
}

async fn commit_count_by_repo_in<C: DBRunner>(
    conn: &C,
    scope: &AccessScope,
    repo_id: i64,
    since: Option<DateTimeUtc>,
) -> Result<u64, DomainError> {
    CommitEntity::find()
        .secure()
        .scope_with(scope)
        .filter(commit_listing_condition(repo_id, since))
        .count(conn)
        .await
        .map_err(map_scope_error)
}

async fn commit_delete_stale_in<C: DBRunner>(
    conn: &C,
    scope: &AccessScope,
    repo_id: i64,
    extracted_before: DateTimeUtc,
) -> Result<u64, DomainError> {
    // RFC3339 strings order lexicographically; the pre-column default ''
    // sorts before any stamp, so unstamped legacy rows count as stale.
    let result = CommitEntity::delete_many()
        .secure()
        .scope_with(scope)
        .filter(
            sea_orm::Condition::all()
                .add(commits::Column::RepoId.eq(repo_id))
                .add(
                    sea_orm::Condition::any()
                        .add(commits::Column::ExtractedAt.lt(extracted_before))
                        .add(commits::Column::ExtractedAt.is_null()),
                ),
        )
        .exec(conn)
        .await
        .map_err(map_scope_error)?;
    Ok(result.rows_affected)
}

async fn commit_upsert_in<C: DBRunner>(
    conn: &C,
    scope: &AccessScope,
    tenant_id: Uuid,
    record: CommitRecord,
) -> Result<Commit, DomainError> {
    let on_conflict = SecureOnConflict::<CommitEntity>::columns([
        commits::Column::TenantId,
        commits::Column::RepoId,
        commits::Column::Sha,
    ])
    .update_columns([
        commits::Column::Message,
        commits::Column::AuthorLogin,
        commits::Column::CommitterLogin,
        commits::Column::AuthoredAt,
        commits::Column::CommittedAt,
        commits::Column::Additions,
        commits::Column::Deletions,
        commits::Column::ExtractedAt,
    ])
    .map_err(map_scope_error)?;

    let model = commit_active_model(tenant_id, &record);
    CommitEntity::insert(model.clone())
        .secure()
        .scope_with_model(scope, &model)
        .map_err(map_scope_error)?
        .on_conflict(on_conflict)
        .exec(conn)
        .await
        .map_err(map_scope_error)?;

    Ok(Commit {
        repo_id: record.repo_id,
        sha: record.sha,
        message: record.message,
        author_login: record.author_login,
        committer_login: record.committer_login,
        authored_at: record.authored_at,
        committed_at: record.committed_at,
        additions: record.additions,
        deletions: record.deletions,
    })
}

/// One repository's commits, narrowed to those committed at or after
/// `since`. The count and the page must share it, or the two disagree.
fn commit_listing_condition(repo_id: i64, since: Option<DateTimeUtc>) -> sea_orm::Condition {
    let mut condition = sea_orm::Condition::all().add(commits::Column::RepoId.eq(repo_id));
    if let Some(since) = since {
        condition = condition.add(commits::Column::CommittedAt.gte(github_instant(since)));
    }
    condition
}

async fn commit_list_by_repo_in<C: DBRunner>(
    conn: &C,
    scope: &AccessScope,
    repo_id: i64,
    window: PageWindow,
    since: Option<DateTimeUtc>,
) -> Result<Vec<Commit>, DomainError> {
    let rows = CommitEntity::find()
        .secure()
        .scope_with(scope)
        .filter(commit_listing_condition(repo_id, since))
        .order_by(commits::Column::CommittedAt, Order::Desc)
        // Unique tie-break: equal sort keys must not shuffle page windows.
        .order_by(commits::Column::Sha, Order::Asc)
        .limit(window.limit())
        .offset(window.offset())
        .all(conn)
        .await
        .map_err(map_scope_error)?;

    Ok(rows.into_iter().map(Into::into).collect())
}

async fn commit_find_by_sha_in<C: DBRunner>(
    conn: &C,
    scope: &AccessScope,
    repo_id: i64,
    sha: &str,
) -> Result<Option<Commit>, DomainError> {
    let row = CommitEntity::find()
        .secure()
        .scope_with(scope)
        .filter(
            sea_orm::Condition::all()
                .add(commits::Column::RepoId.eq(repo_id))
                .add(commits::Column::Sha.eq(sha)),
        )
        .one(conn)
        .await
        .map_err(map_scope_error)?;

    Ok(row.map(Into::into))
}

async fn comment_delete_stale_in<C: DBRunner>(
    conn: &C,
    scope: &AccessScope,
    repo_id: i64,
    extracted_before: DateTimeUtc,
) -> Result<u64, DomainError> {
    // RFC3339 strings order lexicographically; the pre-column default ''
    // sorts before any stamp, so unstamped legacy rows count as stale.
    let result = CommentEntity::delete_many()
        .secure()
        .scope_with(scope)
        .filter(
            sea_orm::Condition::all()
                .add(comments::Column::RepoId.eq(repo_id))
                .add(
                    sea_orm::Condition::any()
                        .add(comments::Column::ExtractedAt.lt(extracted_before))
                        .add(comments::Column::ExtractedAt.is_null()),
                ),
        )
        .exec(conn)
        .await
        .map_err(map_scope_error)?;
    Ok(result.rows_affected)
}

async fn comment_upsert_in<C: DBRunner>(
    conn: &C,
    scope: &AccessScope,
    tenant_id: Uuid,
    record: CommentRecord,
) -> Result<Comment, DomainError> {
    let on_conflict = SecureOnConflict::<CommentEntity>::columns([
        comments::Column::TenantId,
        comments::Column::Id,
    ])
    .update_columns([
        comments::Column::RepoId,
        comments::Column::IssueNumber,
        comments::Column::AuthorLogin,
        comments::Column::Body,
        comments::Column::CreatedAt,
        comments::Column::UpdatedAt,
        comments::Column::HtmlUrl,
        comments::Column::ExtractedAt,
    ])
    .map_err(map_scope_error)?;

    let model = comment_active_model(tenant_id, &record);
    CommentEntity::insert(model.clone())
        .secure()
        .scope_with_model(scope, &model)
        .map_err(map_scope_error)?
        .on_conflict(on_conflict)
        .exec(conn)
        .await
        .map_err(map_scope_error)?;

    Ok(Comment {
        id: record.id,
        repo_id: record.repo_id,
        issue_number: record.issue_number,
        author_login: record.author_login,
        body: record.body,
        created_at: record.created_at,
        updated_at: record.updated_at,
        html_url: record.html_url,
    })
}

async fn comment_list_by_issue_in<C: DBRunner>(
    conn: &C,
    scope: &AccessScope,
    repo_id: i64,
    issue_number: i64,
    window: PageWindow,
) -> Result<Vec<Comment>, DomainError> {
    let rows = CommentEntity::find()
        .secure()
        .scope_with(scope)
        .filter(
            sea_orm::Condition::all()
                .add(comments::Column::RepoId.eq(repo_id))
                .add(comments::Column::IssueNumber.eq(issue_number)),
        )
        .order_by(comments::Column::CreatedAt, Order::Asc)
        // Unique tie-break: equal sort keys must not shuffle page windows.
        .order_by(comments::Column::Id, Order::Asc)
        .limit(window.limit())
        .offset(window.offset())
        .all(conn)
        .await
        .map_err(map_scope_error)?;

    Ok(rows.into_iter().map(Into::into).collect())
}

async fn review_comment_delete_stale_in<C: DBRunner>(
    conn: &C,
    scope: &AccessScope,
    repo_id: i64,
    extracted_before: DateTimeUtc,
) -> Result<u64, DomainError> {
    // RFC3339 strings order lexicographically; the pre-column default ''
    // sorts before any stamp, so unstamped legacy rows count as stale.
    let result = ReviewCommentEntity::delete_many()
        .secure()
        .scope_with(scope)
        .filter(
            sea_orm::Condition::all()
                .add(review_comments::Column::RepoId.eq(repo_id))
                .add(
                    sea_orm::Condition::any()
                        .add(review_comments::Column::ExtractedAt.lt(extracted_before))
                        .add(review_comments::Column::ExtractedAt.is_null()),
                ),
        )
        .exec(conn)
        .await
        .map_err(map_scope_error)?;
    Ok(result.rows_affected)
}

async fn review_comment_upsert_in<C: DBRunner>(
    conn: &C,
    scope: &AccessScope,
    tenant_id: Uuid,
    record: ReviewCommentRecord,
) -> Result<ReviewComment, DomainError> {
    let on_conflict = SecureOnConflict::<ReviewCommentEntity>::columns([
        review_comments::Column::TenantId,
        review_comments::Column::Id,
    ])
    .update_columns([
        review_comments::Column::RepoId,
        review_comments::Column::PullNumber,
        review_comments::Column::AuthorLogin,
        review_comments::Column::Body,
        review_comments::Column::Path,
        review_comments::Column::DiffHunk,
        review_comments::Column::InReplyToId,
        review_comments::Column::CommitId,
        review_comments::Column::CreatedAt,
        review_comments::Column::UpdatedAt,
        review_comments::Column::HtmlUrl,
        review_comments::Column::Position,
        review_comments::Column::Line,
        review_comments::Column::OriginalLine,
        review_comments::Column::StartLine,
        review_comments::Column::OriginalStartLine,
        review_comments::Column::Side,
        review_comments::Column::StartSide,
        review_comments::Column::SubjectType,
        review_comments::Column::OriginalPosition,
        review_comments::Column::PullRequestReviewId,
        review_comments::Column::ExtractedAt,
    ])
    .map_err(map_scope_error)?;

    let model = review_comment_active_model(tenant_id, &record);
    ReviewCommentEntity::insert(model.clone())
        .secure()
        .scope_with_model(scope, &model)
        .map_err(map_scope_error)?
        .on_conflict(on_conflict)
        .exec(conn)
        .await
        .map_err(map_scope_error)?;

    Ok(ReviewComment {
        id: record.id,
        repo_id: record.repo_id,
        pull_number: record.pull_number,
        author_login: record.author_login,
        body: record.body,
        path: record.path,
        diff_hunk: record.diff_hunk,
        in_reply_to_id: record.in_reply_to_id,
        commit_id: record.commit_id,
        created_at: record.created_at,
        updated_at: record.updated_at,
        html_url: record.html_url,
        position: record.position,
        original_position: record.original_position,
        line: record.line,
        original_line: record.original_line,
        start_line: record.start_line,
        original_start_line: record.original_start_line,
        side: record.side,
        start_side: record.start_side,
        subject_type: record.subject_type,
        pull_request_review_id: record.pull_request_review_id,
    })
}

async fn review_comment_list_by_pull_in<C: DBRunner>(
    conn: &C,
    scope: &AccessScope,
    repo_id: i64,
    pull_number: i64,
    window: PageWindow,
) -> Result<Vec<ReviewComment>, DomainError> {
    let rows = ReviewCommentEntity::find()
        .secure()
        .scope_with(scope)
        .filter(
            sea_orm::Condition::all()
                .add(review_comments::Column::RepoId.eq(repo_id))
                .add(review_comments::Column::PullNumber.eq(pull_number)),
        )
        .order_by(review_comments::Column::CreatedAt, Order::Asc)
        // Unique tie-break: equal sort keys must not shuffle page windows.
        .order_by(review_comments::Column::Id, Order::Asc)
        .limit(window.limit())
        .offset(window.offset())
        .all(conn)
        .await
        .map_err(map_scope_error)?;

    Ok(rows.into_iter().map(Into::into).collect())
}

async fn review_upsert_in<C: DBRunner>(
    conn: &C,
    scope: &AccessScope,
    tenant_id: Uuid,
    record: ReviewRecord,
) -> Result<Review, DomainError> {
    let on_conflict =
        SecureOnConflict::<ReviewEntity>::columns([reviews::Column::TenantId, reviews::Column::Id])
            .update_columns([
                reviews::Column::RepoId,
                reviews::Column::PullNumber,
                reviews::Column::AuthorLogin,
                reviews::Column::State,
                reviews::Column::Body,
                reviews::Column::CommitId,
                reviews::Column::SubmittedAt,
                reviews::Column::HtmlUrl,
                reviews::Column::ExtractedAt,
            ])
            .map_err(map_scope_error)?;

    let model = review_active_model(tenant_id, &record);
    ReviewEntity::insert(model.clone())
        .secure()
        .scope_with_model(scope, &model)
        .map_err(map_scope_error)?
        .on_conflict(on_conflict)
        .exec(conn)
        .await
        .map_err(map_scope_error)?;

    Ok(Review {
        id: record.id,
        repo_id: record.repo_id,
        pull_number: record.pull_number,
        author_login: record.author_login,
        state: record.state,
        body: record.body,
        commit_id: record.commit_id,
        submitted_at: record.submitted_at,
        html_url: record.html_url,
    })
}

async fn review_list_by_pull_in<C: DBRunner>(
    conn: &C,
    scope: &AccessScope,
    repo_id: i64,
    pull_number: i64,
    window: PageWindow,
) -> Result<Vec<Review>, DomainError> {
    let rows = ReviewEntity::find()
        .secure()
        .scope_with(scope)
        .filter(
            sea_orm::Condition::all()
                .add(reviews::Column::RepoId.eq(repo_id))
                .add(reviews::Column::PullNumber.eq(pull_number)),
        )
        .order_by(reviews::Column::Id, Order::Asc)
        .limit(window.limit())
        .offset(window.offset())
        .all(conn)
        .await
        .map_err(map_scope_error)?;

    Ok(rows.into_iter().map(Into::into).collect())
}

async fn label_delete_stale_in<C: DBRunner>(
    conn: &C,
    scope: &AccessScope,
    repo_id: i64,
    extracted_before: DateTimeUtc,
) -> Result<u64, DomainError> {
    // RFC3339 strings order lexicographically; the pre-column default ''
    // sorts before any stamp, so unstamped legacy rows count as stale.
    let result = LabelEntity::delete_many()
        .secure()
        .scope_with(scope)
        .filter(
            sea_orm::Condition::all()
                .add(labels::Column::RepoId.eq(repo_id))
                .add(
                    sea_orm::Condition::any()
                        .add(labels::Column::ExtractedAt.lt(extracted_before))
                        .add(labels::Column::ExtractedAt.is_null()),
                ),
        )
        .exec(conn)
        .await
        .map_err(map_scope_error)?;
    Ok(result.rows_affected)
}

async fn label_upsert_in<C: DBRunner>(
    conn: &C,
    scope: &AccessScope,
    tenant_id: Uuid,
    record: LabelRecord,
) -> Result<Label, DomainError> {
    let on_conflict =
        SecureOnConflict::<LabelEntity>::columns([labels::Column::TenantId, labels::Column::Id])
            .update_columns([
                labels::Column::RepoId,
                labels::Column::Name,
                labels::Column::Color,
                labels::Column::IsDefault,
                labels::Column::Description,
                labels::Column::ExtractedAt,
            ])
            .map_err(map_scope_error)?;

    let model = label_active_model(tenant_id, &record);
    LabelEntity::insert(model.clone())
        .secure()
        .scope_with_model(scope, &model)
        .map_err(map_scope_error)?
        .on_conflict(on_conflict)
        .exec(conn)
        .await
        .map_err(map_scope_error)?;

    Ok(Label {
        id: record.id,
        repo_id: record.repo_id,
        name: record.name,
        color: record.color,
        is_default: record.is_default,
        description: record.description,
    })
}

async fn label_list_by_repo_in<C: DBRunner>(
    conn: &C,
    scope: &AccessScope,
    repo_id: i64,
    window: PageWindow,
) -> Result<Vec<Label>, DomainError> {
    let rows = LabelEntity::find()
        .secure()
        .scope_with(scope)
        .filter(sea_orm::Condition::all().add(labels::Column::RepoId.eq(repo_id)))
        .order_by(labels::Column::Name, Order::Asc)
        .order_by(labels::Column::Id, Order::Asc)
        .limit(window.limit())
        .offset(window.offset())
        .all(conn)
        .await
        .map_err(map_scope_error)?;

    Ok(rows.into_iter().map(Into::into).collect())
}

async fn milestone_delete_stale_in<C: DBRunner>(
    conn: &C,
    scope: &AccessScope,
    repo_id: i64,
    extracted_before: DateTimeUtc,
) -> Result<u64, DomainError> {
    // RFC3339 strings order lexicographically; the pre-column default ''
    // sorts before any stamp, so unstamped legacy rows count as stale.
    let result = MilestoneEntity::delete_many()
        .secure()
        .scope_with(scope)
        .filter(
            sea_orm::Condition::all()
                .add(milestones::Column::RepoId.eq(repo_id))
                .add(
                    sea_orm::Condition::any()
                        .add(milestones::Column::ExtractedAt.lt(extracted_before))
                        .add(milestones::Column::ExtractedAt.is_null()),
                ),
        )
        .exec(conn)
        .await
        .map_err(map_scope_error)?;
    Ok(result.rows_affected)
}

async fn milestone_upsert_in<C: DBRunner>(
    conn: &C,
    scope: &AccessScope,
    tenant_id: Uuid,
    record: MilestoneRecord,
) -> Result<Milestone, DomainError> {
    let on_conflict = SecureOnConflict::<MilestoneEntity>::columns([
        milestones::Column::TenantId,
        milestones::Column::Id,
    ])
    .update_columns([
        milestones::Column::RepoId,
        milestones::Column::Number,
        milestones::Column::Title,
        milestones::Column::State,
        milestones::Column::Description,
        milestones::Column::OpenIssues,
        milestones::Column::ClosedIssues,
        milestones::Column::DueOn,
        milestones::Column::CreatedAt,
        milestones::Column::UpdatedAt,
        milestones::Column::ClosedAt,
        milestones::Column::HtmlUrl,
        milestones::Column::ExtractedAt,
    ])
    .map_err(map_scope_error)?;

    let model = milestone_active_model(tenant_id, &record);
    MilestoneEntity::insert(model.clone())
        .secure()
        .scope_with_model(scope, &model)
        .map_err(map_scope_error)?
        .on_conflict(on_conflict)
        .exec(conn)
        .await
        .map_err(map_scope_error)?;

    Ok(Milestone {
        id: record.id,
        repo_id: record.repo_id,
        number: record.number,
        title: record.title,
        state: record.state,
        description: record.description,
        open_issues: record.open_issues,
        closed_issues: record.closed_issues,
        due_on: record.due_on,
        created_at: record.created_at,
        updated_at: record.updated_at,
        closed_at: record.closed_at,
        html_url: record.html_url,
    })
}

async fn milestone_list_by_repo_in<C: DBRunner>(
    conn: &C,
    scope: &AccessScope,
    repo_id: i64,
    window: PageWindow,
) -> Result<Vec<Milestone>, DomainError> {
    let rows = MilestoneEntity::find()
        .secure()
        .scope_with(scope)
        .filter(sea_orm::Condition::all().add(milestones::Column::RepoId.eq(repo_id)))
        .order_by(milestones::Column::Number, Order::Asc)
        .order_by(milestones::Column::Id, Order::Asc)
        .limit(window.limit())
        .offset(window.offset())
        .all(conn)
        .await
        .map_err(map_scope_error)?;

    Ok(rows.into_iter().map(Into::into).collect())
}

async fn release_delete_stale_in<C: DBRunner>(
    conn: &C,
    scope: &AccessScope,
    repo_id: i64,
    extracted_before: DateTimeUtc,
) -> Result<u64, DomainError> {
    // RFC3339 strings order lexicographically; the pre-column default ''
    // sorts before any stamp, so unstamped legacy rows count as stale.
    let result = ReleaseEntity::delete_many()
        .secure()
        .scope_with(scope)
        .filter(
            sea_orm::Condition::all()
                .add(releases::Column::RepoId.eq(repo_id))
                .add(
                    sea_orm::Condition::any()
                        .add(releases::Column::ExtractedAt.lt(extracted_before))
                        .add(releases::Column::ExtractedAt.is_null()),
                ),
        )
        .exec(conn)
        .await
        .map_err(map_scope_error)?;
    Ok(result.rows_affected)
}

async fn release_upsert_in<C: DBRunner>(
    conn: &C,
    scope: &AccessScope,
    tenant_id: Uuid,
    record: ReleaseRecord,
) -> Result<Release, DomainError> {
    let on_conflict = SecureOnConflict::<ReleaseEntity>::columns([
        releases::Column::TenantId,
        releases::Column::Id,
    ])
    .update_columns([
        releases::Column::RepoId,
        releases::Column::TagName,
        releases::Column::Name,
        releases::Column::Draft,
        releases::Column::Prerelease,
        releases::Column::Body,
        releases::Column::AuthorLogin,
        releases::Column::CreatedAt,
        releases::Column::PublishedAt,
        releases::Column::HtmlUrl,
        releases::Column::AssetsJson,
        releases::Column::ExtractedAt,
    ])
    .map_err(map_scope_error)?;

    let model = release_active_model(tenant_id, &record);
    ReleaseEntity::insert(model.clone())
        .secure()
        .scope_with_model(scope, &model)
        .map_err(map_scope_error)?
        .on_conflict(on_conflict)
        .exec(conn)
        .await
        .map_err(map_scope_error)?;

    Ok(Release {
        id: record.id,
        repo_id: record.repo_id,
        tag_name: record.tag_name,
        name: record.name,
        draft: record.draft,
        prerelease: record.prerelease,
        body: record.body,
        author_login: record.author_login,
        created_at: record.created_at,
        published_at: record.published_at,
        html_url: record.html_url,
        assets: decode_list::<StoredAsset, _>(
            "assets_json",
            StoredRow::new(record.repo_id, record.id),
            record.assets_json.as_deref(),
        ),
    })
}

async fn release_list_by_repo_in<C: DBRunner>(
    conn: &C,
    scope: &AccessScope,
    repo_id: i64,
    window: PageWindow,
) -> Result<Vec<Release>, DomainError> {
    let rows = ReleaseEntity::find()
        .secure()
        .scope_with(scope)
        .filter(sea_orm::Condition::all().add(releases::Column::RepoId.eq(repo_id)))
        .order_by(releases::Column::CreatedAt, Order::Desc)
        // Unique tie-break: equal sort keys must not shuffle page windows.
        .order_by(releases::Column::Id, Order::Asc)
        .limit(window.limit())
        .offset(window.offset())
        .all(conn)
        .await
        .map_err(map_scope_error)?;

    Ok(rows.into_iter().map(Into::into).collect())
}

async fn branch_delete_stale_in<C: DBRunner>(
    conn: &C,
    scope: &AccessScope,
    repo_id: i64,
    extracted_before: DateTimeUtc,
) -> Result<u64, DomainError> {
    // RFC3339 strings order lexicographically; the pre-column default ''
    // sorts before any stamp, so unstamped legacy rows count as stale.
    let result = BranchEntity::delete_many()
        .secure()
        .scope_with(scope)
        .filter(
            sea_orm::Condition::all()
                .add(branches::Column::RepoId.eq(repo_id))
                .add(
                    sea_orm::Condition::any()
                        .add(branches::Column::ExtractedAt.lt(extracted_before))
                        .add(branches::Column::ExtractedAt.is_null()),
                ),
        )
        .exec(conn)
        .await
        .map_err(map_scope_error)?;
    Ok(result.rows_affected)
}

async fn branch_upsert_in<C: DBRunner>(
    conn: &C,
    scope: &AccessScope,
    tenant_id: Uuid,
    record: BranchRecord,
) -> Result<Branch, DomainError> {
    let on_conflict = SecureOnConflict::<BranchEntity>::columns([
        branches::Column::TenantId,
        branches::Column::RepoId,
        branches::Column::Name,
    ])
    .update_columns([
        branches::Column::CommitSha,
        branches::Column::Protected,
        branches::Column::ExtractedAt,
    ])
    .map_err(map_scope_error)?;

    let model = branch_active_model(tenant_id, &record);
    BranchEntity::insert(model.clone())
        .secure()
        .scope_with_model(scope, &model)
        .map_err(map_scope_error)?
        .on_conflict(on_conflict)
        .exec(conn)
        .await
        .map_err(map_scope_error)?;

    Ok(Branch {
        repo_id: record.repo_id,
        name: record.name,
        commit_sha: record.commit_sha,
        protected: record.protected,
    })
}

async fn branch_list_by_repo_in<C: DBRunner>(
    conn: &C,
    scope: &AccessScope,
    repo_id: i64,
    window: PageWindow,
) -> Result<Vec<Branch>, DomainError> {
    let rows = BranchEntity::find()
        .secure()
        .scope_with(scope)
        .filter(sea_orm::Condition::all().add(branches::Column::RepoId.eq(repo_id)))
        .order_by(branches::Column::Name, Order::Asc)
        .limit(window.limit())
        .offset(window.offset())
        .all(conn)
        .await
        .map_err(map_scope_error)?;

    Ok(rows.into_iter().map(Into::into).collect())
}

async fn contributor_upsert_in<C: DBRunner>(
    conn: &C,
    scope: &AccessScope,
    tenant_id: Uuid,
    record: ContributorRecord,
) -> Result<Contributor, DomainError> {
    let on_conflict = SecureOnConflict::<ContributorEntity>::columns([
        contributors::Column::TenantId,
        contributors::Column::RepoId,
        contributors::Column::UserId,
    ])
    .update_columns([
        contributors::Column::Login,
        contributors::Column::AccountType,
        contributors::Column::AvatarUrl,
        contributors::Column::HtmlUrl,
        contributors::Column::Roles,
        contributors::Column::FirstSeenAt,
        contributors::Column::LastSeenAt,
        contributors::Column::ExtractedAt,
    ])
    .map_err(map_scope_error)?;

    let model = contributor_active_model(tenant_id, &record);
    ContributorEntity::insert(model.clone())
        .secure()
        .scope_with_model(scope, &model)
        .map_err(map_scope_error)?
        .on_conflict(on_conflict)
        .exec(conn)
        .await
        .map_err(map_scope_error)?;

    Ok(Contributor {
        repo_id: record.repo_id,
        user_id: record.user_id,
        login: record.login,
        account_type: record.account_type,
        avatar_url: record.avatar_url,
        html_url: record.html_url,
        roles: record.roles,
        first_seen_at: record.first_seen_at,
        last_seen_at: record.last_seen_at,
    })
}

async fn contributor_list_by_repo_in<C: DBRunner>(
    conn: &C,
    scope: &AccessScope,
    repo_id: i64,
    window: PageWindow,
) -> Result<Vec<Contributor>, DomainError> {
    let rows = ContributorEntity::find()
        .secure()
        .scope_with(scope)
        .filter(sea_orm::Condition::all().add(contributors::Column::RepoId.eq(repo_id)))
        // Derived contributors carry no activity count to rank by, so
        // the unique key is the whole ordering.
        .order_by(contributors::Column::UserId, Order::Asc)
        .limit(window.limit())
        .offset(window.offset())
        .all(conn)
        .await
        .map_err(map_scope_error)?;

    Ok(rows.into_iter().map(Into::into).collect())
}

async fn workflow_run_count_by_repo_in<C: DBRunner>(
    conn: &C,
    scope: &AccessScope,
    repo_id: i64,
) -> Result<u64, DomainError> {
    WorkflowRunEntity::find()
        .secure()
        .scope_with(scope)
        .filter(sea_orm::Condition::all().add(workflow_runs::Column::RepoId.eq(repo_id)))
        .count(conn)
        .await
        .map_err(map_scope_error)
}

async fn workflow_run_upsert_in<C: DBRunner>(
    conn: &C,
    scope: &AccessScope,
    tenant_id: Uuid,
    record: WorkflowRunRecord,
) -> Result<WorkflowRun, DomainError> {
    let on_conflict = SecureOnConflict::<WorkflowRunEntity>::columns([
        workflow_runs::Column::TenantId,
        workflow_runs::Column::Id,
    ])
    .update_columns([
        workflow_runs::Column::RepoId,
        workflow_runs::Column::WorkflowId,
        workflow_runs::Column::RunNumber,
        workflow_runs::Column::RunAttempt,
        workflow_runs::Column::Name,
        workflow_runs::Column::Event,
        workflow_runs::Column::Status,
        workflow_runs::Column::Conclusion,
        workflow_runs::Column::HeadBranch,
        workflow_runs::Column::HeadSha,
        workflow_runs::Column::CreatedAt,
        workflow_runs::Column::UpdatedAt,
        workflow_runs::Column::HtmlUrl,
        workflow_runs::Column::ActorLogin,
        workflow_runs::Column::ExtractedAt,
    ])
    .map_err(map_scope_error)?;

    let model = workflow_run_active_model(tenant_id, &record);
    WorkflowRunEntity::insert(model.clone())
        .secure()
        .scope_with_model(scope, &model)
        .map_err(map_scope_error)?
        .on_conflict(on_conflict)
        .exec(conn)
        .await
        .map_err(map_scope_error)?;

    Ok(WorkflowRun {
        id: record.id,
        repo_id: record.repo_id,
        workflow_id: record.workflow_id,
        run_number: record.run_number,
        run_attempt: record.run_attempt,
        name: record.name,
        event: record.event,
        status: record.status,
        conclusion: record.conclusion,
        head_branch: record.head_branch,
        head_sha: record.head_sha,
        created_at: record.created_at,
        updated_at: record.updated_at,
        html_url: record.html_url,
        actor_login: record.actor_login,
    })
}

async fn workflow_run_list_by_repo_in<C: DBRunner>(
    conn: &C,
    scope: &AccessScope,
    repo_id: i64,
    window: PageWindow,
) -> Result<Vec<WorkflowRun>, DomainError> {
    let rows = WorkflowRunEntity::find()
        .secure()
        .scope_with(scope)
        .filter(sea_orm::Condition::all().add(workflow_runs::Column::RepoId.eq(repo_id)))
        .order_by(workflow_runs::Column::CreatedAt, Order::Desc)
        // Unique tie-break: equal sort keys must not shuffle page windows.
        .order_by(workflow_runs::Column::Id, Order::Asc)
        .limit(window.limit())
        .offset(window.offset())
        .all(conn)
        .await
        .map_err(map_scope_error)?;

    Ok(rows.into_iter().map(Into::into).collect())
}

async fn pull_request_file_upsert_in<C: DBRunner>(
    conn: &C,
    scope: &AccessScope,
    tenant_id: Uuid,
    record: PullRequestFileRecord,
) -> Result<PullRequestFile, DomainError> {
    let on_conflict = SecureOnConflict::<PullRequestFileEntity>::columns([
        pull_request_files::Column::TenantId,
        pull_request_files::Column::RepoId,
        pull_request_files::Column::PullNumber,
        pull_request_files::Column::Filename,
    ])
    .update_columns([
        pull_request_files::Column::Status,
        pull_request_files::Column::Additions,
        pull_request_files::Column::Deletions,
        pull_request_files::Column::Changes,
        pull_request_files::Column::PreviousFilename,
        pull_request_files::Column::Patch,
        pull_request_files::Column::Sha,
        pull_request_files::Column::ExtractedAt,
    ])
    .map_err(map_scope_error)?;

    let model = pull_request_file_active_model(tenant_id, &record);
    PullRequestFileEntity::insert(model.clone())
        .secure()
        .scope_with_model(scope, &model)
        .map_err(map_scope_error)?
        .on_conflict(on_conflict)
        .exec(conn)
        .await
        .map_err(map_scope_error)?;

    Ok(PullRequestFile {
        repo_id: record.repo_id,
        pull_number: record.pull_number,
        filename: record.filename,
        status: record.status,
        additions: record.additions,
        deletions: record.deletions,
        changes: record.changes,
        previous_filename: record.previous_filename,
        patch: record.patch,
        sha: record.sha,
    })
}

async fn pull_request_file_list_by_pull_in<C: DBRunner>(
    conn: &C,
    scope: &AccessScope,
    repo_id: i64,
    pull_number: i64,
    window: PageWindow,
) -> Result<Vec<PullRequestFile>, DomainError> {
    let rows = PullRequestFileEntity::find()
        .secure()
        .scope_with(scope)
        .filter(
            sea_orm::Condition::all()
                .add(pull_request_files::Column::RepoId.eq(repo_id))
                .add(pull_request_files::Column::PullNumber.eq(pull_number)),
        )
        .order_by(pull_request_files::Column::Filename, Order::Asc)
        .limit(window.limit())
        .offset(window.offset())
        .all(conn)
        .await
        .map_err(map_scope_error)?;

    Ok(rows.into_iter().map(Into::into).collect())
}

async fn tag_delete_stale_in<C: DBRunner>(
    conn: &C,
    scope: &AccessScope,
    repo_id: i64,
    extracted_before: DateTimeUtc,
) -> Result<u64, DomainError> {
    // RFC3339 strings order lexicographically; the pre-column default ''
    // sorts before any stamp, so unstamped legacy rows count as stale.
    let result = TagEntity::delete_many()
        .secure()
        .scope_with(scope)
        .filter(
            sea_orm::Condition::all()
                .add(tags::Column::RepoId.eq(repo_id))
                .add(
                    sea_orm::Condition::any()
                        .add(tags::Column::ExtractedAt.lt(extracted_before))
                        .add(tags::Column::ExtractedAt.is_null()),
                ),
        )
        .exec(conn)
        .await
        .map_err(map_scope_error)?;
    Ok(result.rows_affected)
}

async fn tag_upsert_in<C: DBRunner>(
    conn: &C,
    scope: &AccessScope,
    tenant_id: Uuid,
    record: TagRecord,
) -> Result<Tag, DomainError> {
    let on_conflict = SecureOnConflict::<TagEntity>::columns([
        tags::Column::TenantId,
        tags::Column::RepoId,
        tags::Column::Name,
    ])
    .update_columns([tags::Column::CommitSha, tags::Column::ExtractedAt])
    .map_err(map_scope_error)?;

    let model = tag_active_model(tenant_id, &record);
    TagEntity::insert(model.clone())
        .secure()
        .scope_with_model(scope, &model)
        .map_err(map_scope_error)?
        .on_conflict(on_conflict)
        .exec(conn)
        .await
        .map_err(map_scope_error)?;

    Ok(Tag {
        repo_id: record.repo_id,
        name: record.name,
        commit_sha: record.commit_sha,
    })
}

async fn tag_list_by_repo_in<C: DBRunner>(
    conn: &C,
    scope: &AccessScope,
    repo_id: i64,
    window: PageWindow,
) -> Result<Vec<Tag>, DomainError> {
    let rows = TagEntity::find()
        .secure()
        .scope_with(scope)
        .filter(sea_orm::Condition::all().add(tags::Column::RepoId.eq(repo_id)))
        .order_by(tags::Column::Name, Order::Asc)
        .limit(window.limit())
        .offset(window.offset())
        .all(conn)
        .await
        .map_err(map_scope_error)?;

    Ok(rows.into_iter().map(Into::into).collect())
}

async fn commit_file_upsert_in<C: DBRunner>(
    conn: &C,
    scope: &AccessScope,
    tenant_id: Uuid,
    record: CommitFileRecord,
) -> Result<CommitFile, DomainError> {
    let on_conflict = SecureOnConflict::<CommitFileEntity>::columns([
        commit_files::Column::TenantId,
        commit_files::Column::RepoId,
        commit_files::Column::CommitSha,
        commit_files::Column::Filename,
    ])
    .update_columns([
        commit_files::Column::Status,
        commit_files::Column::Additions,
        commit_files::Column::Deletions,
        commit_files::Column::Changes,
        commit_files::Column::PreviousFilename,
        commit_files::Column::Sha,
        commit_files::Column::ExtractedAt,
    ])
    .map_err(map_scope_error)?;

    let model = commit_file_active_model(tenant_id, &record);
    CommitFileEntity::insert(model.clone())
        .secure()
        .scope_with_model(scope, &model)
        .map_err(map_scope_error)?
        .on_conflict(on_conflict)
        .exec(conn)
        .await
        .map_err(map_scope_error)?;

    Ok(CommitFile {
        repo_id: record.repo_id,
        commit_sha: record.commit_sha,
        filename: record.filename,
        status: record.status,
        additions: record.additions,
        deletions: record.deletions,
        changes: record.changes,
        previous_filename: record.previous_filename,
        sha: record.sha,
    })
}

async fn commit_file_list_by_commit_in<C: DBRunner>(
    conn: &C,
    scope: &AccessScope,
    repo_id: i64,
    commit_sha: &str,
    query: &ODataQuery,
) -> Result<Page<CommitFile>, DomainError> {
    paginate_odata::<CommitFileField, CommitFileODataMapper, _, _, _, _>(
        CommitFileEntity::find().secure().scope_with(scope).filter(
            sea_orm::Condition::all()
                .add(commit_files::Column::RepoId.eq(repo_id))
                .add(commit_files::Column::CommitSha.eq(commit_sha)),
        ),
        conn,
        query,
        ("filename", SortDir::Asc),
        LIST_LIMITS,
        Into::into,
    )
    .await
    .map_err(map_odata_error)
}

async fn review_thread_upsert_in<C: DBRunner>(
    conn: &C,
    scope: &AccessScope,
    tenant_id: Uuid,
    record: ReviewThreadRecord,
) -> Result<ReviewThread, DomainError> {
    let on_conflict = SecureOnConflict::<ReviewThreadEntity>::columns([
        review_threads::Column::TenantId,
        review_threads::Column::Id,
    ])
    .update_columns([
        review_threads::Column::RepoId,
        review_threads::Column::PullNumber,
        review_threads::Column::IsResolved,
        review_threads::Column::IsOutdated,
        review_threads::Column::Path,
        review_threads::Column::Line,
        review_threads::Column::ResolvedBy,
        review_threads::Column::CommentsCount,
        review_threads::Column::ExtractedAt,
    ])
    .map_err(map_scope_error)?;

    let model = review_thread_active_model(tenant_id, &record);
    ReviewThreadEntity::insert(model.clone())
        .secure()
        .scope_with_model(scope, &model)
        .map_err(map_scope_error)?
        .on_conflict(on_conflict)
        .exec(conn)
        .await
        .map_err(map_scope_error)?;

    Ok(ReviewThread {
        id: record.id,
        repo_id: record.repo_id,
        pull_number: record.pull_number,
        is_resolved: record.is_resolved,
        is_outdated: record.is_outdated,
        path: record.path,
        line: record.line,
        resolved_by: record.resolved_by,
        comments_count: record.comments_count,
    })
}

async fn review_thread_list_by_pull_in<C: DBRunner>(
    conn: &C,
    scope: &AccessScope,
    repo_id: i64,
    pull_number: i64,
    query: &ODataQuery,
) -> Result<Page<ReviewThread>, DomainError> {
    paginate_odata::<ReviewThreadField, ReviewThreadODataMapper, _, _, _, _>(
        ReviewThreadEntity::find()
            .secure()
            .scope_with(scope)
            .filter(
                sea_orm::Condition::all()
                    .add(review_threads::Column::RepoId.eq(repo_id))
                    .add(review_threads::Column::PullNumber.eq(pull_number)),
            ),
        conn,
        query,
        ("id", SortDir::Asc),
        LIST_LIMITS,
        Into::into,
    )
    .await
    .map_err(map_odata_error)
}

async fn commit_comment_upsert_in<C: DBRunner>(
    conn: &C,
    scope: &AccessScope,
    tenant_id: Uuid,
    record: CommitCommentRecord,
) -> Result<CommitComment, DomainError> {
    let on_conflict = SecureOnConflict::<CommitCommentEntity>::columns([
        commit_comments::Column::TenantId,
        commit_comments::Column::Id,
    ])
    .update_columns([
        commit_comments::Column::RepoId,
        commit_comments::Column::CommitSha,
        commit_comments::Column::Path,
        commit_comments::Column::Position,
        commit_comments::Column::AuthorLogin,
        commit_comments::Column::Body,
        commit_comments::Column::CreatedAt,
        commit_comments::Column::UpdatedAt,
        commit_comments::Column::HtmlUrl,
        commit_comments::Column::ExtractedAt,
    ])
    .map_err(map_scope_error)?;

    let model = commit_comment_active_model(tenant_id, &record);
    CommitCommentEntity::insert(model.clone())
        .secure()
        .scope_with_model(scope, &model)
        .map_err(map_scope_error)?
        .on_conflict(on_conflict)
        .exec(conn)
        .await
        .map_err(map_scope_error)?;

    Ok(CommitComment {
        id: record.id,
        repo_id: record.repo_id,
        commit_sha: record.commit_sha,
        path: record.path,
        position: record.position,
        author_login: record.author_login,
        body: record.body,
        created_at: record.created_at,
        updated_at: record.updated_at,
        html_url: record.html_url,
    })
}

async fn commit_comment_list_by_commit_in<C: DBRunner>(
    conn: &C,
    scope: &AccessScope,
    repo_id: i64,
    commit_sha: &str,
    window: PageWindow,
) -> Result<Vec<CommitComment>, DomainError> {
    let rows = CommitCommentEntity::find()
        .secure()
        .scope_with(scope)
        .filter(
            sea_orm::Condition::all()
                .add(commit_comments::Column::RepoId.eq(repo_id))
                .add(commit_comments::Column::CommitSha.eq(commit_sha)),
        )
        .order_by(commit_comments::Column::CreatedAt, Order::Asc)
        // Unique tie-break: equal sort keys must not shuffle page windows.
        .order_by(commit_comments::Column::Id, Order::Asc)
        .limit(window.limit())
        .offset(window.offset())
        .all(conn)
        .await
        .map_err(map_scope_error)?;

    Ok(rows.into_iter().map(Into::into).collect())
}

async fn issue_event_upsert_in<C: DBRunner>(
    conn: &C,
    scope: &AccessScope,
    tenant_id: Uuid,
    record: IssueEventRecord,
) -> Result<IssueEvent, DomainError> {
    let on_conflict = SecureOnConflict::<IssueEventEntity>::columns([
        issue_events::Column::TenantId,
        issue_events::Column::Id,
    ])
    .update_columns([
        issue_events::Column::RepoId,
        issue_events::Column::IssueNumber,
        issue_events::Column::Event,
        issue_events::Column::ActorLogin,
        issue_events::Column::LabelName,
        issue_events::Column::AssigneeLogin,
        issue_events::Column::MilestoneTitle,
        issue_events::Column::CommitId,
        issue_events::Column::CreatedAt,
        issue_events::Column::ExtractedAt,
    ])
    .map_err(map_scope_error)?;

    let model = issue_event_active_model(tenant_id, &record);
    IssueEventEntity::insert(model.clone())
        .secure()
        .scope_with_model(scope, &model)
        .map_err(map_scope_error)?
        .on_conflict(on_conflict)
        .exec(conn)
        .await
        .map_err(map_scope_error)?;

    Ok(IssueEvent {
        id: record.id,
        repo_id: record.repo_id,
        issue_number: record.issue_number,
        event: record.event,
        actor_login: record.actor_login,
        label_name: record.label_name,
        assignee_login: record.assignee_login,
        milestone_title: record.milestone_title,
        commit_id: record.commit_id,
        created_at: record.created_at,
    })
}

async fn issue_event_list_by_issue_in<C: DBRunner>(
    conn: &C,
    scope: &AccessScope,
    repo_id: i64,
    issue_number: i64,
    window: PageWindow,
) -> Result<Vec<IssueEvent>, DomainError> {
    let rows = IssueEventEntity::find()
        .secure()
        .scope_with(scope)
        .filter(
            sea_orm::Condition::all()
                .add(issue_events::Column::RepoId.eq(repo_id))
                .add(issue_events::Column::IssueNumber.eq(issue_number)),
        )
        .order_by(issue_events::Column::CreatedAt, Order::Asc)
        // Unique tie-break: equal sort keys must not shuffle page windows.
        .order_by(issue_events::Column::Id, Order::Asc)
        .limit(window.limit())
        .offset(window.offset())
        .all(conn)
        .await
        .map_err(map_scope_error)?;

    Ok(rows.into_iter().map(Into::into).collect())
}

async fn deployment_upsert_in<C: DBRunner>(
    conn: &C,
    scope: &AccessScope,
    tenant_id: Uuid,
    record: DeploymentRecord,
) -> Result<Deployment, DomainError> {
    let on_conflict = SecureOnConflict::<DeploymentEntity>::columns([
        deployments::Column::TenantId,
        deployments::Column::Id,
    ])
    .update_columns([
        deployments::Column::RepoId,
        deployments::Column::GitRef,
        deployments::Column::Sha,
        deployments::Column::Environment,
        deployments::Column::Task,
        deployments::Column::Description,
        deployments::Column::CreatorLogin,
        deployments::Column::CreatedAt,
        deployments::Column::UpdatedAt,
        deployments::Column::ExtractedAt,
    ])
    .map_err(map_scope_error)?;

    let model = deployment_active_model(tenant_id, &record);
    DeploymentEntity::insert(model.clone())
        .secure()
        .scope_with_model(scope, &model)
        .map_err(map_scope_error)?
        .on_conflict(on_conflict)
        .exec(conn)
        .await
        .map_err(map_scope_error)?;

    Ok(Deployment {
        id: record.id,
        repo_id: record.repo_id,
        git_ref: record.git_ref,
        sha: record.sha,
        environment: record.environment,
        task: record.task,
        description: record.description,
        creator_login: record.creator_login,
        created_at: record.created_at,
        updated_at: record.updated_at,
    })
}

async fn deployment_list_by_repo_in<C: DBRunner>(
    conn: &C,
    scope: &AccessScope,
    repo_id: i64,
    window: PageWindow,
) -> Result<Vec<Deployment>, DomainError> {
    let rows = DeploymentEntity::find()
        .secure()
        .scope_with(scope)
        .filter(sea_orm::Condition::all().add(deployments::Column::RepoId.eq(repo_id)))
        .order_by(deployments::Column::CreatedAt, Order::Desc)
        // Unique tie-break: equal sort keys must not shuffle page windows.
        .order_by(deployments::Column::Id, Order::Asc)
        .limit(window.limit())
        .offset(window.offset())
        .all(conn)
        .await
        .map_err(map_scope_error)?;

    Ok(rows.into_iter().map(Into::into).collect())
}

async fn pull_request_commit_upsert_in<C: DBRunner>(
    conn: &C,
    scope: &AccessScope,
    tenant_id: Uuid,
    record: PullRequestCommitRecord,
) -> Result<PullRequestCommit, DomainError> {
    let on_conflict = SecureOnConflict::<PullRequestCommitEntity>::columns([
        pull_request_commits::Column::TenantId,
        pull_request_commits::Column::RepoId,
        pull_request_commits::Column::PullNumber,
        pull_request_commits::Column::Sha,
    ])
    .update_columns([
        pull_request_commits::Column::Message,
        pull_request_commits::Column::AuthorLogin,
        pull_request_commits::Column::CommitterLogin,
        pull_request_commits::Column::AuthoredAt,
        pull_request_commits::Column::CommittedAt,
        pull_request_commits::Column::ExtractedAt,
    ])
    .map_err(map_scope_error)?;

    let model = pull_request_commit_active_model(tenant_id, &record);
    PullRequestCommitEntity::insert(model.clone())
        .secure()
        .scope_with_model(scope, &model)
        .map_err(map_scope_error)?
        .on_conflict(on_conflict)
        .exec(conn)
        .await
        .map_err(map_scope_error)?;

    Ok(PullRequestCommit {
        repo_id: record.repo_id,
        pull_number: record.pull_number,
        sha: record.sha,
        message: record.message,
        author_login: record.author_login,
        committer_login: record.committer_login,
        authored_at: record.authored_at,
        committed_at: record.committed_at,
    })
}

async fn pull_request_commit_list_by_pull_in<C: DBRunner>(
    conn: &C,
    scope: &AccessScope,
    repo_id: i64,
    pull_number: i64,
    window: PageWindow,
) -> Result<Vec<PullRequestCommit>, DomainError> {
    let rows = PullRequestCommitEntity::find()
        .secure()
        .scope_with(scope)
        .filter(
            sea_orm::Condition::all()
                .add(pull_request_commits::Column::RepoId.eq(repo_id))
                .add(pull_request_commits::Column::PullNumber.eq(pull_number)),
        )
        .order_by(pull_request_commits::Column::CommittedAt, Order::Asc)
        // Unique tie-break: equal sort keys must not shuffle page windows.
        .order_by(pull_request_commits::Column::Sha, Order::Asc)
        .limit(window.limit())
        .offset(window.offset())
        .all(conn)
        .await
        .map_err(map_scope_error)?;

    Ok(rows.into_iter().map(Into::into).collect())
}

async fn commit_status_upsert_in<C: DBRunner>(
    conn: &C,
    scope: &AccessScope,
    tenant_id: Uuid,
    record: CommitStatusRecord,
) -> Result<CommitStatus, DomainError> {
    let on_conflict = SecureOnConflict::<CommitStatusEntity>::columns([
        commit_statuses::Column::TenantId,
        commit_statuses::Column::Id,
    ])
    .update_columns([
        commit_statuses::Column::RepoId,
        commit_statuses::Column::CommitSha,
        commit_statuses::Column::State,
        commit_statuses::Column::Context,
        commit_statuses::Column::Description,
        commit_statuses::Column::TargetUrl,
        commit_statuses::Column::CreatorLogin,
        commit_statuses::Column::CreatedAt,
        commit_statuses::Column::UpdatedAt,
        commit_statuses::Column::ExtractedAt,
    ])
    .map_err(map_scope_error)?;

    let model = commit_status_active_model(tenant_id, &record);
    CommitStatusEntity::insert(model.clone())
        .secure()
        .scope_with_model(scope, &model)
        .map_err(map_scope_error)?
        .on_conflict(on_conflict)
        .exec(conn)
        .await
        .map_err(map_scope_error)?;

    Ok(CommitStatus {
        id: record.id,
        repo_id: record.repo_id,
        commit_sha: record.commit_sha,
        state: record.state,
        context: record.context,
        description: record.description,
        target_url: record.target_url,
        creator_login: record.creator_login,
        created_at: record.created_at,
        updated_at: record.updated_at,
    })
}

async fn commit_status_list_by_commit_in<C: DBRunner>(
    conn: &C,
    scope: &AccessScope,
    repo_id: i64,
    commit_sha: &str,
    window: PageWindow,
) -> Result<Vec<CommitStatus>, DomainError> {
    let rows = CommitStatusEntity::find()
        .secure()
        .scope_with(scope)
        .filter(
            sea_orm::Condition::all()
                .add(commit_statuses::Column::RepoId.eq(repo_id))
                .add(commit_statuses::Column::CommitSha.eq(commit_sha)),
        )
        .order_by(commit_statuses::Column::CreatedAt, Order::Desc)
        // Unique tie-break: equal sort keys must not shuffle page windows.
        .order_by(commit_statuses::Column::Id, Order::Asc)
        .limit(window.limit())
        .offset(window.offset())
        .all(conn)
        .await
        .map_err(map_scope_error)?;

    Ok(rows.into_iter().map(Into::into).collect())
}

async fn workflow_job_count_by_run_in<C: DBRunner>(
    conn: &C,
    scope: &AccessScope,
    repo_id: i64,
    run_id: i64,
) -> Result<u64, DomainError> {
    WorkflowJobEntity::find()
        .secure()
        .scope_with(scope)
        .filter(
            sea_orm::Condition::all()
                .add(workflow_jobs::Column::RepoId.eq(repo_id))
                .add(workflow_jobs::Column::RunId.eq(run_id)),
        )
        .count(conn)
        .await
        .map_err(map_scope_error)
}

async fn workflow_job_upsert_in<C: DBRunner>(
    conn: &C,
    scope: &AccessScope,
    tenant_id: Uuid,
    record: WorkflowJobRecord,
) -> Result<WorkflowJob, DomainError> {
    let on_conflict = SecureOnConflict::<WorkflowJobEntity>::columns([
        workflow_jobs::Column::TenantId,
        workflow_jobs::Column::Id,
    ])
    .update_columns([
        workflow_jobs::Column::RepoId,
        workflow_jobs::Column::RunId,
        workflow_jobs::Column::RunAttempt,
        workflow_jobs::Column::Name,
        workflow_jobs::Column::Status,
        workflow_jobs::Column::Conclusion,
        workflow_jobs::Column::HeadSha,
        workflow_jobs::Column::RunnerName,
        workflow_jobs::Column::StartedAt,
        workflow_jobs::Column::CompletedAt,
        workflow_jobs::Column::HtmlUrl,
        workflow_jobs::Column::StepsJson,
        workflow_jobs::Column::ExtractedAt,
    ])
    .map_err(map_scope_error)?;

    let model = workflow_job_active_model(tenant_id, &record);
    WorkflowJobEntity::insert(model.clone())
        .secure()
        .scope_with_model(scope, &model)
        .map_err(map_scope_error)?
        .on_conflict(on_conflict)
        .exec(conn)
        .await
        .map_err(map_scope_error)?;

    Ok(WorkflowJob {
        id: record.id,
        repo_id: record.repo_id,
        run_id: record.run_id,
        run_attempt: record.run_attempt,
        name: record.name,
        status: record.status,
        conclusion: record.conclusion,
        head_sha: record.head_sha,
        runner_name: record.runner_name,
        started_at: record.started_at,
        completed_at: record.completed_at,
        html_url: record.html_url,
        steps: decode_list::<StoredStep, _>(
            "steps_json",
            StoredRow::new(record.repo_id, record.id),
            record.steps_json.as_deref(),
        ),
    })
}

async fn workflow_job_list_by_run_in<C: DBRunner>(
    conn: &C,
    scope: &AccessScope,
    repo_id: i64,
    run_id: i64,
    window: PageWindow,
) -> Result<Vec<WorkflowJob>, DomainError> {
    let rows = WorkflowJobEntity::find()
        .secure()
        .scope_with(scope)
        .filter(
            sea_orm::Condition::all()
                .add(workflow_jobs::Column::RepoId.eq(repo_id))
                .add(workflow_jobs::Column::RunId.eq(run_id)),
        )
        .order_by(workflow_jobs::Column::Id, Order::Asc)
        .limit(window.limit())
        .offset(window.offset())
        .all(conn)
        .await
        .map_err(map_scope_error)?;

    Ok(rows.into_iter().map(Into::into).collect())
}

async fn issue_reaction_upsert_in<C: DBRunner>(
    conn: &C,
    scope: &AccessScope,
    tenant_id: Uuid,
    record: IssueReactionRecord,
) -> Result<IssueReaction, DomainError> {
    let on_conflict = SecureOnConflict::<IssueReactionEntity>::columns([
        issue_reactions::Column::TenantId,
        issue_reactions::Column::Id,
    ])
    .update_columns([
        issue_reactions::Column::RepoId,
        issue_reactions::Column::IssueNumber,
        issue_reactions::Column::Content,
        issue_reactions::Column::UserLogin,
        issue_reactions::Column::CreatedAt,
        issue_reactions::Column::ExtractedAt,
    ])
    .map_err(map_scope_error)?;

    let model = issue_reaction_active_model(tenant_id, &record);
    IssueReactionEntity::insert(model.clone())
        .secure()
        .scope_with_model(scope, &model)
        .map_err(map_scope_error)?
        .on_conflict(on_conflict)
        .exec(conn)
        .await
        .map_err(map_scope_error)?;

    Ok(IssueReaction {
        id: record.id,
        repo_id: record.repo_id,
        issue_number: record.issue_number,
        content: record.content,
        user_login: record.user_login,
        created_at: record.created_at,
    })
}

async fn issue_reaction_list_by_issue_in<C: DBRunner>(
    conn: &C,
    scope: &AccessScope,
    repo_id: i64,
    issue_number: i64,
    window: PageWindow,
) -> Result<Vec<IssueReaction>, DomainError> {
    let rows = IssueReactionEntity::find()
        .secure()
        .scope_with(scope)
        .filter(
            sea_orm::Condition::all()
                .add(issue_reactions::Column::RepoId.eq(repo_id))
                .add(issue_reactions::Column::IssueNumber.eq(issue_number)),
        )
        .order_by(issue_reactions::Column::Id, Order::Asc)
        .limit(window.limit())
        .offset(window.offset())
        .all(conn)
        .await
        .map_err(map_scope_error)?;

    Ok(rows.into_iter().map(Into::into).collect())
}

async fn check_run_count_by_commit_in<C: DBRunner>(
    conn: &C,
    scope: &AccessScope,
    repo_id: i64,
    head_sha: &str,
) -> Result<u64, DomainError> {
    CheckRunEntity::find()
        .secure()
        .scope_with(scope)
        .filter(
            sea_orm::Condition::all()
                .add(check_runs::Column::RepoId.eq(repo_id))
                .add(check_runs::Column::HeadSha.eq(head_sha)),
        )
        .count(conn)
        .await
        .map_err(map_scope_error)
}

async fn check_run_upsert_in<C: DBRunner>(
    conn: &C,
    scope: &AccessScope,
    tenant_id: Uuid,
    record: CheckRunRecord,
) -> Result<CheckRun, DomainError> {
    let on_conflict = SecureOnConflict::<CheckRunEntity>::columns([
        check_runs::Column::TenantId,
        check_runs::Column::Id,
    ])
    .update_columns([
        check_runs::Column::RepoId,
        check_runs::Column::HeadSha,
        check_runs::Column::Name,
        check_runs::Column::Status,
        check_runs::Column::Conclusion,
        check_runs::Column::StartedAt,
        check_runs::Column::CompletedAt,
        check_runs::Column::HtmlUrl,
        check_runs::Column::DetailsUrl,
        check_runs::Column::CheckSuiteId,
        check_runs::Column::AppSlug,
        check_runs::Column::AppName,
        check_runs::Column::OutputTitle,
        check_runs::Column::OutputSummary,
        check_runs::Column::AnnotationsCount,
        check_runs::Column::ExtractedAt,
    ])
    .map_err(map_scope_error)?;

    let model = check_run_active_model(tenant_id, &record);
    CheckRunEntity::insert(model.clone())
        .secure()
        .scope_with_model(scope, &model)
        .map_err(map_scope_error)?
        .on_conflict(on_conflict)
        .exec(conn)
        .await
        .map_err(map_scope_error)?;

    Ok(CheckRun {
        id: record.id,
        repo_id: record.repo_id,
        head_sha: record.head_sha,
        name: record.name,
        status: record.status,
        conclusion: record.conclusion,
        started_at: record.started_at,
        completed_at: record.completed_at,
        html_url: record.html_url,
        details_url: record.details_url,
        check_suite_id: record.check_suite_id,
        app_slug: record.app_slug,
        app_name: record.app_name,
        output_title: record.output_title,
        output_summary: record.output_summary,
        annotations_count: record.annotations_count,
    })
}

async fn check_run_list_by_commit_in<C: DBRunner>(
    conn: &C,
    scope: &AccessScope,
    repo_id: i64,
    head_sha: &str,
    window: PageWindow,
) -> Result<Vec<CheckRun>, DomainError> {
    let rows = CheckRunEntity::find()
        .secure()
        .scope_with(scope)
        .filter(
            sea_orm::Condition::all()
                .add(check_runs::Column::RepoId.eq(repo_id))
                .add(check_runs::Column::HeadSha.eq(head_sha)),
        )
        .order_by(check_runs::Column::Id, Order::Asc)
        .limit(window.limit())
        .offset(window.offset())
        .all(conn)
        .await
        .map_err(map_scope_error)?;

    Ok(rows.into_iter().map(Into::into).collect())
}

async fn issue_timeline_delete_by_issues_in<C: DBRunner>(
    conn: &C,
    scope: &AccessScope,
    repo_id: i64,
    issue_numbers: &[i64],
) -> Result<u64, DomainError> {
    // `IN ()` is invalid SQL on some engines and means nothing on any.
    if issue_numbers.is_empty() {
        return Ok(0);
    }

    let result = IssueTimelineEntity::delete_many()
        .secure()
        .scope_with(scope)
        .filter(
            sea_orm::Condition::all()
                .add(issue_timeline::Column::RepoId.eq(repo_id))
                .add(issue_timeline::Column::IssueNumber.is_in(issue_numbers.iter().copied())),
        )
        .exec(conn)
        .await
        .map_err(map_scope_error)?;
    Ok(result.rows_affected)
}

async fn issue_timeline_upsert_in<C: DBRunner>(
    conn: &C,
    scope: &AccessScope,
    tenant_id: Uuid,
    record: IssueTimelineEventRecord,
) -> Result<IssueTimelineEvent, DomainError> {
    let on_conflict = SecureOnConflict::<IssueTimelineEntity>::columns([
        issue_timeline::Column::TenantId,
        issue_timeline::Column::RepoId,
        issue_timeline::Column::IssueNumber,
        issue_timeline::Column::Position,
    ])
    .update_columns([
        issue_timeline::Column::Event,
        issue_timeline::Column::CreatedAt,
        issue_timeline::Column::ActorLogin,
        issue_timeline::Column::PayloadJson,
        issue_timeline::Column::ExtractedAt,
    ])
    .map_err(map_scope_error)?;

    let model = issue_timeline_active_model(tenant_id, &record);
    IssueTimelineEntity::insert(model.clone())
        .secure()
        .scope_with_model(scope, &model)
        .map_err(map_scope_error)?
        .on_conflict(on_conflict)
        .exec(conn)
        .await
        .map_err(map_scope_error)?;

    Ok(IssueTimelineEvent {
        repo_id: record.repo_id,
        issue_number: record.issue_number,
        position: record.position,
        payload: timeline_payload(
            record.repo_id,
            record.issue_number,
            record.position,
            &record.event,
            Some(&record.payload_json),
        ),
        event: record.event,
        created_at: record.created_at,
        actor_login: record.actor_login,
    })
}

async fn issue_timeline_list_by_issue_in<C: DBRunner>(
    conn: &C,
    scope: &AccessScope,
    repo_id: i64,
    issue_number: i64,
    window: PageWindow,
) -> Result<Vec<IssueTimelineEvent>, DomainError> {
    let rows = IssueTimelineEntity::find()
        .secure()
        .scope_with(scope)
        .filter(
            sea_orm::Condition::all()
                .add(issue_timeline::Column::RepoId.eq(repo_id))
                .add(issue_timeline::Column::IssueNumber.eq(issue_number)),
        )
        .order_by(issue_timeline::Column::Position, Order::Asc)
        .limit(window.limit())
        .offset(window.offset())
        .all(conn)
        .await
        .map_err(map_scope_error)?;

    Ok(rows.into_iter().map(Into::into).collect())
}

/// How many stored contributors one merge reads back. A repository with more
/// distinct people than this loses nothing already written - the merge simply
/// cannot widen the rows it did not see.
const CONTRIBUTOR_MERGE_LIMIT: u64 = 10_000;

/// The earlier of two optional instants, ignoring a missing one.
fn earliest(a: Option<DateTimeUtc>, b: Option<DateTimeUtc>) -> Option<DateTimeUtc> {
    [a, b].into_iter().flatten().min()
}

/// One mirrored table's upsert pass: writes every fetched record and reports
/// how many rows it wrote.
macro_rules! sync_table {
    ($conn:expr, $scope:expr, $tenant:expr, $upsert:ident, $records:expr) => {{
        let mut synced: u64 = 0;
        for record in $records {
            $upsert($conn, $scope, $tenant, record).await?;
            synced += 1;
        }
        synced
    }};
}

async fn merge_known_contributors<C: DBRunner>(
    conn: &C,
    scope: &AccessScope,
    repo_id: i64,
    derived: Vec<ContributorRecord>,
) -> Result<Vec<ContributorRecord>, DomainError> {
    if derived.is_empty() {
        return Ok(derived);
    }

    let known = contributor_list_by_repo_in(
        conn,
        scope,
        repo_id,
        PageWindow::first(CONTRIBUTOR_MERGE_LIMIT),
    )
    .await?;
    let known: std::collections::HashMap<i64, Contributor> =
        known.into_iter().map(|c| (c.user_id, c)).collect();

    Ok(derived
        .into_iter()
        .map(|mut record| {
            let Some(stored) = known.get(&record.user_id) else {
                return record;
            };
            for role in &stored.roles {
                if !record.roles.iter().any(|held| held == role) {
                    record.roles.push(role.clone());
                }
            }
            record.roles.sort();
            record.first_seen_at = earliest(record.first_seen_at, stored.first_seen_at);
            record.last_seen_at = record.last_seen_at.max(stored.last_seen_at);
            record
        })
        .collect())
}

async fn reconcile_stale<C: DBRunner>(
    conn: &C,
    scope: &AccessScope,
    complete: &ListingCompleteness,
    repo_id: i64,
    watermark: DateTimeUtc,
) -> Result<u64, DomainError> {
    let mut deleted = 0;
    for listing in Listing::iter() {
        if !complete.is_complete(listing) {
            continue;
        }
        deleted += match listing {
            Listing::Issues => issue_delete_stale_in(conn, scope, repo_id, watermark).await,
            Listing::PullRequests => {
                pull_request_delete_stale_in(conn, scope, repo_id, watermark).await
            }
            Listing::Commits => commit_delete_stale_in(conn, scope, repo_id, watermark).await,
            Listing::Comments => comment_delete_stale_in(conn, scope, repo_id, watermark).await,
            Listing::ReviewComments => {
                review_comment_delete_stale_in(conn, scope, repo_id, watermark).await
            }
            Listing::Labels => label_delete_stale_in(conn, scope, repo_id, watermark).await,
            Listing::Milestones => milestone_delete_stale_in(conn, scope, repo_id, watermark).await,
            Listing::Releases => release_delete_stale_in(conn, scope, repo_id, watermark).await,
            Listing::Branches => branch_delete_stale_in(conn, scope, repo_id, watermark).await,
            Listing::Tags => tag_delete_stale_in(conn, scope, repo_id, watermark).await,
        }?;
    }
    Ok(deleted)
}

/// Writes one sync's whole result: all 26 tables plus the deletion pass, in a
/// single transaction, so a failure partway through cannot leave some tables
/// current and others stale.
pub struct SeaOrmSyncWriter {
    db: Arc<DbProvider>,
}

impl SeaOrmSyncWriter {
    #[must_use]
    pub fn new(db: Arc<DbProvider>) -> Self {
        Self { db }
    }
}

#[async_trait]
impl SyncWriter for SeaOrmSyncWriter {
    async fn write_sync(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        fetched: FetchedRepository,
        watermark: DateTimeUtc,
    ) -> Result<SyncSummary, DomainError> {
        let scope = scope.clone();
        let complete = fetched.complete.clone();
        self.db
            .db()
            .transaction_ref_mapped(move |tx| {
                Box::pin(async move {
                    let repository =
                        repo_upsert_in(tx, &scope, tenant_id, fetched.repository).await?;

                    let issues_synced =
                        sync_table!(tx, &scope, tenant_id, issue_upsert_in, fetched.issues);

                    let pull_requests_synced = sync_table!(
                        tx,
                        &scope,
                        tenant_id,
                        pull_request_upsert_in,
                        fetched.pull_requests
                    );

                    let commits_synced =
                        sync_table!(tx, &scope, tenant_id, commit_upsert_in, fetched.commits);

                    let comments_synced =
                        sync_table!(tx, &scope, tenant_id, comment_upsert_in, fetched.comments);

                    let review_comments_synced = sync_table!(
                        tx,
                        &scope,
                        tenant_id,
                        review_comment_upsert_in,
                        fetched.review_comments
                    );

                    let reviews_synced =
                        sync_table!(tx, &scope, tenant_id, review_upsert_in, fetched.reviews);

                    let labels_synced =
                        sync_table!(tx, &scope, tenant_id, label_upsert_in, fetched.labels);

                    let milestones_synced = sync_table!(
                        tx,
                        &scope,
                        tenant_id,
                        milestone_upsert_in,
                        fetched.milestones
                    );

                    let releases_synced =
                        sync_table!(tx, &scope, tenant_id, release_upsert_in, fetched.releases);

                    let branches_synced =
                        sync_table!(tx, &scope, tenant_id, branch_upsert_in, fetched.branches);

                    // Contributors are derived from whatever this sync
                    // happened to fetch, so writing them straight would
                    // narrow the set every time the scope narrows. Merge
                    // with what earlier syncs already learned instead.
                    let contributors =
                        merge_known_contributors(tx, &scope, repository.id, fetched.contributors)
                            .await?;
                    let contributors_synced =
                        sync_table!(tx, &scope, tenant_id, contributor_upsert_in, contributors);

                    let workflow_runs_synced = sync_table!(
                        tx,
                        &scope,
                        tenant_id,
                        workflow_run_upsert_in,
                        fetched.workflow_runs
                    );

                    let pull_request_files_synced = sync_table!(
                        tx,
                        &scope,
                        tenant_id,
                        pull_request_file_upsert_in,
                        fetched.pull_request_files
                    );

                    let tags_synced =
                        sync_table!(tx, &scope, tenant_id, tag_upsert_in, fetched.tags);

                    let commit_files_synced = sync_table!(
                        tx,
                        &scope,
                        tenant_id,
                        commit_file_upsert_in,
                        fetched.commit_files
                    );

                    let review_threads_synced = sync_table!(
                        tx,
                        &scope,
                        tenant_id,
                        review_thread_upsert_in,
                        fetched.review_threads
                    );

                    let commit_comments_synced = sync_table!(
                        tx,
                        &scope,
                        tenant_id,
                        commit_comment_upsert_in,
                        fetched.commit_comments
                    );

                    let issue_events_synced = sync_table!(
                        tx,
                        &scope,
                        tenant_id,
                        issue_event_upsert_in,
                        fetched.issue_events
                    );

                    let deployments_synced = sync_table!(
                        tx,
                        &scope,
                        tenant_id,
                        deployment_upsert_in,
                        fetched.deployments
                    );

                    let pull_request_commits_synced = sync_table!(
                        tx,
                        &scope,
                        tenant_id,
                        pull_request_commit_upsert_in,
                        fetched.pull_request_commits
                    );

                    let commit_statuses_synced = sync_table!(
                        tx,
                        &scope,
                        tenant_id,
                        commit_status_upsert_in,
                        fetched.commit_statuses
                    );

                    let workflow_jobs_synced = sync_table!(
                        tx,
                        &scope,
                        tenant_id,
                        workflow_job_upsert_in,
                        fetched.workflow_jobs
                    );

                    let issue_reactions_synced = sync_table!(
                        tx,
                        &scope,
                        tenant_id,
                        issue_reaction_upsert_in,
                        fetched.issue_reactions
                    );

                    let check_runs_synced = sync_table!(
                        tx,
                        &scope,
                        tenant_id,
                        check_run_upsert_in,
                        fetched.check_runs
                    );
                    // Rows are keyed by position, so a shorter timeline would
                    // leave the old tail behind: clear each fetched issue
                    // before rewriting it.
                    let refetched: Vec<i64> = fetched
                        .issue_timeline
                        .iter()
                        .map(|event| event.issue_number)
                        .collect::<std::collections::HashSet<_>>()
                        .into_iter()
                        .collect();
                    issue_timeline_delete_by_issues_in(tx, &scope, repository.id, &refetched)
                        .await?;
                    let issue_timeline_synced = sync_table!(
                        tx,
                        &scope,
                        tenant_id,
                        issue_timeline_upsert_in,
                        fetched.issue_timeline
                    );

                    let stale_rows_deleted =
                        reconcile_stale(tx, &scope, &complete, repository.id, watermark).await?;
                    if stale_rows_deleted > 0 {
                        tracing::info!(
                            repository = %repository.full_name,
                            stale_rows_deleted,
                            "reconciled upstream deletions"
                        );
                    }

                    Ok(SyncSummary {
                        repository: repository.full_name,
                        issues_synced,
                        pull_requests_synced,
                        commits_synced,
                        comments_synced,
                        review_comments_synced,
                        reviews_synced,
                        labels_synced,
                        milestones_synced,
                        releases_synced,
                        branches_synced,
                        contributors_synced,
                        workflow_runs_synced,
                        pull_request_files_synced,
                        tags_synced,
                        commit_files_synced,
                        review_threads_synced,
                        commit_comments_synced,
                        issue_events_synced,
                        deployments_synced,
                        pull_request_commits_synced,
                        commit_statuses_synced,
                        workflow_jobs_synced,
                        issue_reactions_synced,
                        check_runs_synced,
                        issue_timeline_synced,
                        stale_rows_deleted,
                    })
                })
            })
            .await
    }
}
