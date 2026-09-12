//! The gear's [`Worker`]: one implementation handling every phase, fetching
//! through the GitHub port and writing through the sync writer.
//!
//! Discovery fetches the repository row and seeds one Indexing task per
//! enabled family. Each Indexing task walks its family's listings, writes
//! them, and seeds one Refinement task per entity that has per-entity
//! sub-resources to fetch. Each Refinement task fetches and writes exactly one
//! entity's detail, in its own transaction, so a run interrupted anywhere
//! leaves nothing half-written.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use async_trait::async_trait;
use chrono::Utc;
use github_mirror_sdk::{CountDrift, SyncSummary};
use toolkit_security::AccessScope;
use uuid::Uuid;

use super::change_gate::{self, ChangeGate, GateInputs, entities};
use super::runner::REPOSITORY_ENTITY;
use super::sweep_watermark::{SweepWatermark, high_water, is_stale, sweep_families};
use super::task::{ExtractionTask, NewTask, TaskPhase, TaskPriority};
use super::verification::{CountGap, GapOutcome, pull_gaps};
use super::worker::{Worker, WorkerContext};
use crate::domain::error::DomainError;
use crate::domain::ports::github::{
    FetchOptions, GithubPort, IssueDetailWants, ListingCompleteness,
};
use crate::domain::repo::{
    CommitRecord, IssueRecord, PullRequestRecord, SyncWriter, WorkflowRunRecord,
};
use crate::domain::scope::CollectionMode;

/// Entity types of the Indexing tasks, one per family the scope can enable.
mod families {
    pub const ISSUES: &str = "issues";
    pub const PULL_REQUESTS: &str = "pull_requests";
    pub const COMMITS: &str = "commits";
    pub const METADATA: &str = "metadata";
    pub const ACTIONS: &str = "actions";
}

/// Everything the tasks of one run share.
///
/// `repo_id` is learned by Discovery and read by every later task; the
/// completeness flags and the summary are accumulated as tasks finish and
/// read by the service once the run is over.
pub struct RunState {
    pub session_id: Uuid,
    pub scope: AccessScope,
    pub tenant_id: Uuid,
    pub owner: String,
    pub name: String,
    pub options: FetchOptions,
    repo_id: OnceLock<i64>,
    complete: Mutex<ListingCompleteness>,
    swept: Mutex<HashMap<&'static str, Option<String>>>,
    summary: Mutex<SyncSummary>,
    drift: Mutex<Vec<CountDrift>>,
}

impl RunState {
    #[must_use]
    pub fn new(
        session_id: Uuid,
        scope: AccessScope,
        tenant_id: Uuid,
        owner: &str,
        name: &str,
        options: FetchOptions,
    ) -> Self {
        Self {
            session_id,
            scope,
            tenant_id,
            owner: owner.to_owned(),
            name: name.to_owned(),
            options,
            repo_id: OnceLock::new(),
            complete: Mutex::new(ListingCompleteness::none()),
            swept: Mutex::new(HashMap::new()),
            summary: Mutex::new(SyncSummary {
                repository: format!("{owner}/{name}"),
                ..SyncSummary::default()
            }),
            drift: Mutex::new(Vec::new()),
        }
    }

    /// GitHub's id for the repository, once Discovery has run.
    ///
    /// # Errors
    /// `Internal` when asked before Discovery — a scheduling bug, since every
    /// other phase is seeded by it.
    pub fn repo_id(&self) -> Result<i64, DomainError> {
        self.repo_id
            .get()
            .copied()
            .ok_or_else(|| DomainError::internal("repository was not discovered before indexing"))
    }

    /// Which listings this run walked to their end.
    #[must_use]
    pub fn completeness(&self) -> ListingCompleteness {
        self.complete
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Row counts so far, in the shape the session records.
    #[must_use]
    pub fn summary(&self) -> SyncSummary {
        self.summary
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn mark_complete(&self, complete: &ListingCompleteness) {
        self.complete
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .absorb(complete);
    }

    /// Record that `family`'s sweep reached its last page, so its watermark
    /// may be promoted. Independent of [`Self::completeness`]: a walk bounded
    /// by `updated_after` saw everything it asked for without seeing everything there
    /// is, so it may advance the watermark but not drive reconciliation.
    fn mark_swept(&self, family: &'static str, page1_etag: Option<String>) {
        self.swept
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(family, page1_etag);
    }

    #[must_use]
    pub fn is_swept(&self, family: &str) -> bool {
        self.swept
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains_key(family)
    }

    #[must_use]
    pub fn swept_page1_etag(&self, family: &str) -> Option<String> {
        self.swept
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(family)
            .cloned()
            .flatten()
    }

    pub fn accept_drift(&self, drift: CountDrift) {
        self.drift
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(drift);
    }

    #[must_use]
    pub fn drift(&self) -> Vec<CountDrift> {
        self.drift
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn tally(&self, add: impl FnOnce(&mut SyncSummary)) {
        add(&mut self.summary.lock().unwrap_or_else(PoisonError::into_inner));
    }
}

/// The worker behind every task of one repository sync.
pub struct MirrorWorker {
    github: Arc<dyn GithubPort>,
    writer: Arc<dyn SyncWriter>,
    gate: Arc<ChangeGate>,
    watermark: Arc<SweepWatermark>,
    run: Arc<RunState>,
}

impl MirrorWorker {
    #[must_use]
    pub fn new(
        github: Arc<dyn GithubPort>,
        writer: Arc<dyn SyncWriter>,
        gate: Arc<ChangeGate>,
        watermark: Arc<SweepWatermark>,
        run: Arc<RunState>,
    ) -> Self {
        Self {
            github,
            writer,
            gate,
            watermark,
            run,
        }
    }

    async fn needs_refinement(
        &self,
        family: &str,
        entity_id: &str,
        inputs: &GateInputs,
    ) -> Result<bool, DomainError> {
        let run = &self.run;
        let reason = self
            .gate
            .evaluate(
                &run.scope,
                run.tenant_id,
                run.repo_id()?,
                family,
                entity_id,
                inputs,
                Utc::now(),
                run.options.force,
            )
            .await?;
        if let Some(reason) = reason {
            tracing::debug!(family, entity_id, reason = reason.as_str(), "refining");
        }
        Ok(reason.is_some())
    }

    async fn mark_refined(&self, family: &str, entity_id: &str) -> Result<(), DomainError> {
        let run = &self.run;
        self.gate
            .mark_refined(
                &run.scope,
                run.tenant_id,
                run.repo_id()?,
                family,
                entity_id,
                Utc::now(),
            )
            .await
    }

    fn seed(
        &self,
        ctx: &WorkerContext,
        phase: TaskPhase,
        entity_type: &str,
        entity_id: Option<String>,
        priority: TaskPriority,
    ) {
        self.seed_attempt(ctx, phase, entity_type, entity_id, priority, 0);
    }

    fn seed_attempt(
        &self,
        ctx: &WorkerContext,
        phase: TaskPhase,
        entity_type: &str,
        entity_id: Option<String>,
        priority: TaskPriority,
        attempt: u32,
    ) {
        ctx.queue.enqueue_task(&NewTask {
            session_id: self.run.session_id,
            tenant_id: self.run.tenant_id,
            phase,
            entity_type: entity_type.to_owned(),
            entity_id,
            priority,
            attempt,
        });
    }

    async fn discover(&self, ctx: &WorkerContext) -> Result<(), DomainError> {
        let run = &self.run;
        let repository = self
            .github
            .fetch_repository_metadata(&run.owner, &run.name, &run.options)
            .await?;
        let stored = self
            .writer
            .write_repository(&run.scope, run.tenant_id, repository)
            .await?;
        run.repo_id.set(stored.id).map_err(|_| {
            DomainError::internal("the repository was discovered twice in one sync")
        })?;
        run.tally(|s| s.repository.clone_from(&stored.full_name));

        let objects = run.options.scope.objects;
        let seeds = [
            (
                families::PULL_REQUESTS,
                objects.pull_requests,
                TaskPriority::OPEN_PR,
            ),
            (families::ISSUES, objects.issues, TaskPriority::OPEN_ISSUE),
            (families::COMMITS, objects.commits, TaskPriority::GLOBAL),
            (
                families::METADATA,
                objects.labels || objects.milestones || objects.releases || objects.branches,
                TaskPriority::GLOBAL,
            ),
            (
                families::ACTIONS,
                objects.github_actions,
                TaskPriority::GLOBAL,
            ),
        ];
        for (family, enabled, priority) in seeds {
            if enabled {
                self.seed(ctx, TaskPhase::Indexing, family, None, priority);
            }
        }
        Ok(())
    }

    async fn index_issues(&self, ctx: &WorkerContext) -> Result<(), DomainError> {
        let run = &self.run;
        let repo_id = run.repo_id()?;
        let start = self
            .watermark
            .start_sweep(
                &run.scope,
                repo_id,
                sweep_families::ISSUES,
                run.options.force,
            )
            .await?;
        let updated_after = start.updated_after;
        let collection = run.options.scope.collection;
        let mut high = updated_after;
        let mut page1_etag: Option<String> = None;
        let mut swept: HashSet<i64> = HashSet::new();
        let mut continue_from: Option<String> = None;

        while !ctx.cancel.is_cancelled() {
            let listing = self
                .github
                .list_issues(
                    &run.owner,
                    &run.name,
                    repo_id,
                    updated_after,
                    start.page1_etag.as_deref(),
                    continue_from.as_deref(),
                    &run.options,
                )
                .await?;
            run.mark_complete(&listing.complete);
            if page1_etag.is_none() {
                page1_etag.clone_from(&listing.page1_etag);
            }
            if listing.swept_to_end {
                run.mark_swept(sweep_families::ISSUES, page1_etag.clone());
            }
            let seen: Vec<&str> = listing
                .issues
                .iter()
                .map(|i| i.updated_at.as_str())
                .collect();
            high = high_water(&seen, high);
            if listing.unchanged {
                return Ok(());
            }

            for issue in &listing.issues {
                if !swept.insert(issue.number) || is_stale(Some(&issue.updated_at), updated_after) {
                    continue;
                }
                let open = issue.state == "open";
                if !(collection.reactions.includes(open) || collection.timeline.includes(open)) {
                    continue;
                }
                let entity_id = issue.number.to_string();
                if !self
                    .needs_refinement(entities::ISSUE, &entity_id, &issue_inputs(issue))
                    .await?
                {
                    continue;
                }
                let priority = if open {
                    TaskPriority::OPEN_ISSUE
                } else {
                    TaskPriority::CLOSED_ISSUE
                };
                self.seed(
                    ctx,
                    TaskPhase::Refinement,
                    entities::ISSUE,
                    Some(entity_id),
                    priority,
                );
            }

            let (issues, comments, events, people) = (
                count(&listing.issues),
                count(&listing.comments),
                count(&listing.issue_events),
                count(&listing.contributors),
            );
            let next = listing.next.clone();
            self.writer
                .write_issue_listing(&run.scope, run.tenant_id, repo_id, listing)
                .await?;
            run.tally(|s| {
                s.issues_synced += issues;
                s.comments_synced += comments;
                s.issue_events_synced += events;
                s.contributors_synced += people;
            });
            match next {
                Some(next) => continue_from = Some(next),
                None => break,
            }
        }

        self.watermark
            .stage(
                &run.scope,
                run.tenant_id,
                repo_id,
                sweep_families::ISSUES,
                high,
            )
            .await?;
        Ok(())
    }

    async fn refine_issue(&self, task: &ExtractionTask) -> Result<(), DomainError> {
        let run = &self.run;
        let repo_id = run.repo_id()?;
        let number = entity_number(task)?;
        let open = task.priority.is_open_tier();
        let collection = run.options.scope.collection;
        let wants = IssueDetailWants {
            reactions: collection.reactions.includes(open),
            timeline: collection.timeline.includes(open),
        };
        let detail = self
            .github
            .refine_issue(&run.owner, &run.name, repo_id, number, wants, &run.options)
            .await?;
        let (reactions, timeline) = (count(&detail.reactions), count(&detail.timeline));
        self.writer
            .write_issue_detail(&run.scope, run.tenant_id, repo_id, detail)
            .await?;
        run.tally(|s| {
            s.issue_reactions_synced += reactions;
            s.issue_timeline_synced += timeline;
        });
        self.mark_refined(entities::ISSUE, &number.to_string())
            .await
    }

    async fn index_pull_requests(&self, ctx: &WorkerContext) -> Result<(), DomainError> {
        let run = &self.run;
        let repo_id = run.repo_id()?;
        let start = self
            .watermark
            .start_sweep(
                &run.scope,
                repo_id,
                sweep_families::PULL_REQUESTS,
                run.options.force,
            )
            .await?;
        let updated_after = start.updated_after;
        let mut high = updated_after;
        let mut page1_etag: Option<String> = None;
        let mut swept: HashSet<i64> = HashSet::new();
        let mut continue_from: Option<String> = None;

        while !ctx.cancel.is_cancelled() {
            let listing = self
                .github
                .list_pull_requests(
                    &run.owner,
                    &run.name,
                    repo_id,
                    start.page1_etag.as_deref(),
                    continue_from.as_deref(),
                    &run.options,
                )
                .await?;
            run.mark_complete(&listing.complete);
            if page1_etag.is_none() {
                page1_etag.clone_from(&listing.page1_etag);
            }
            if listing.swept_to_end {
                run.mark_swept(sweep_families::PULL_REQUESTS, page1_etag.clone());
            }
            let seen: Vec<&str> = listing
                .pull_requests
                .iter()
                .map(|p| p.updated_at.as_str())
                .collect();
            high = high_water(&seen, high);
            if listing.unchanged {
                return Ok(());
            }

            for pull in &listing.pull_requests {
                if !swept.insert(pull.number) || is_stale(Some(&pull.updated_at), updated_after) {
                    continue;
                }
                let entity_id = pull.number.to_string();
                if !self
                    .needs_refinement(entities::PULL_REQUEST, &entity_id, &pull_inputs(pull))
                    .await?
                {
                    continue;
                }
                let priority = if pull.state == "open" {
                    TaskPriority::OPEN_PR
                } else {
                    TaskPriority::CLOSED_PR
                };
                self.seed(
                    ctx,
                    TaskPhase::Refinement,
                    entities::PULL_REQUEST,
                    Some(entity_id),
                    priority,
                );
            }

            let (pulls, comments, people) = (
                count(&listing.pull_requests),
                count(&listing.review_comments),
                count(&listing.contributors),
            );
            let next = listing.next.clone();
            self.writer
                .write_pull_listing(&run.scope, run.tenant_id, repo_id, listing)
                .await?;
            run.tally(|s| {
                s.pull_requests_synced += pulls;
                s.review_comments_synced += comments;
                s.contributors_synced += people;
            });
            match next {
                Some(next) => continue_from = Some(next),
                None => break,
            }
        }

        self.watermark
            .stage(
                &run.scope,
                run.tenant_id,
                repo_id,
                sweep_families::PULL_REQUESTS,
                high,
            )
            .await?;
        Ok(())
    }

    async fn refine_pull_request(
        &self,
        ctx: &WorkerContext,
        task: &ExtractionTask,
    ) -> Result<(), DomainError> {
        let run = &self.run;
        let repo_id = run.repo_id()?;
        let number = entity_number(task)?;
        let detail = self
            .github
            .refine_pull_request(&run.owner, &run.name, repo_id, number, &run.options)
            .await?;
        let gaps = pull_gaps(&detail);
        let (reviews, files, commits, threads, people) = (
            count(&detail.reviews),
            count(&detail.files),
            count(&detail.commits),
            count(&detail.review_threads),
            count(&detail.contributors),
        );
        self.writer
            .write_pull_detail(&run.scope, run.tenant_id, repo_id, detail)
            .await?;
        run.tally(|s| {
            s.reviews_synced += reviews;
            s.pull_request_files_synced += files;
            s.pull_request_commits_synced += commits;
            s.review_threads_synced += threads;
            s.contributors_synced += people;
        });
        for gap in &gaps {
            self.report_gap(ctx, number, gap, task.attempt);
        }
        self.mark_refined(entities::PULL_REQUEST, &number.to_string())
            .await
    }

    fn report_gap(&self, ctx: &WorkerContext, number: i64, gap: &CountGap, attempt: u32) {
        let gap = CountGap {
            repair_attempts: attempt,
            ..gap.clone()
        };
        match gap.outcome() {
            GapOutcome::Complete => {}
            GapOutcome::Repair => {
                tracing::debug!(
                    pull = number,
                    entity_type = %gap.entity_type,
                    expected = gap.expected,
                    stored = gap.stored,
                    attempt = attempt + 1,
                    "repairing a short pull request walk"
                );
                self.seed_attempt(
                    ctx,
                    TaskPhase::Verification,
                    entities::PULL_REQUEST,
                    Some(number.to_string()),
                    TaskPriority::NORMAL,
                    attempt + 1,
                );
            }
            GapOutcome::AcceptedDrift => {
                self.run.accept_drift(CountDrift {
                    entity_type: gap.entity_type.clone(),
                    pull_number: number,
                    expected: gap.expected,
                    stored: gap.stored,
                    passes: attempt,
                });
                tracing::warn!(
                    pull = number,
                    entity_type = %gap.entity_type,
                    expected = gap.expected,
                    stored = gap.stored,
                    passes = attempt,
                    "accepting a pull request count gap GitHub will not serve"
                );
            }
        }
    }

    async fn index_commits(&self, ctx: &WorkerContext) -> Result<(), DomainError> {
        let run = &self.run;
        let repo_id = run.repo_id()?;
        let start = self
            .watermark
            .start_sweep(
                &run.scope,
                repo_id,
                sweep_families::COMMITS,
                run.options.force,
            )
            .await?;
        let updated_after = start.updated_after;
        let with_ci = run.options.scope.collection.actions != CollectionMode::None;
        let mut high = updated_after;
        let mut page1_etag: Option<String> = None;
        let mut swept: HashSet<String> = HashSet::new();
        let mut continue_from: Option<String> = None;

        while !ctx.cancel.is_cancelled() {
            let listing = self
                .github
                .list_commits(
                    &run.owner,
                    &run.name,
                    repo_id,
                    updated_after,
                    start.page1_etag.as_deref(),
                    continue_from.as_deref(),
                    &run.options,
                )
                .await?;
            run.mark_complete(&listing.complete);
            if page1_etag.is_none() {
                page1_etag.clone_from(&listing.page1_etag);
            }
            if listing.swept_to_end {
                run.mark_swept(sweep_families::COMMITS, page1_etag.clone());
            }
            let seen: Vec<&str> = listing
                .commits
                .iter()
                .filter_map(|c| c.committed_at.as_deref())
                .collect();
            high = high_water(&seen, high);
            if listing.unchanged {
                return Ok(());
            }

            for commit in &listing.commits {
                if !swept.insert(commit.sha.clone())
                    || is_stale(commit.committed_at.as_deref(), updated_after)
                {
                    continue;
                }
                if !self
                    .needs_refinement(
                        entities::COMMIT,
                        &commit.sha,
                        &commit_inputs(commit, with_ci),
                    )
                    .await?
                {
                    continue;
                }
                self.seed(
                    ctx,
                    TaskPhase::Refinement,
                    entities::COMMIT,
                    Some(commit.sha.clone()),
                    TaskPriority::NORMAL,
                );
            }

            let (commits, comments, people) = (
                count(&listing.commits),
                count(&listing.commit_comments),
                count(&listing.contributors),
            );
            let next = listing.next.clone();
            self.writer
                .write_commit_listing(&run.scope, run.tenant_id, repo_id, listing)
                .await?;
            run.tally(|s| {
                s.commits_synced += commits;
                s.commit_comments_synced += comments;
                s.contributors_synced += people;
            });
            match next {
                Some(next) => continue_from = Some(next),
                None => break,
            }
        }

        self.watermark
            .stage(
                &run.scope,
                run.tenant_id,
                repo_id,
                sweep_families::COMMITS,
                high,
            )
            .await?;
        Ok(())
    }

    async fn refine_commit(&self, task: &ExtractionTask) -> Result<(), DomainError> {
        let run = &self.run;
        let repo_id = run.repo_id()?;
        let sha = task
            .entity_id
            .as_deref()
            .ok_or_else(|| DomainError::internal("commit task without a SHA"))?;
        let with_ci = run.options.scope.collection.actions != CollectionMode::None;
        let detail = self
            .github
            .refine_commit(&run.owner, &run.name, repo_id, sha, with_ci, &run.options)
            .await?;
        let (files, statuses, checks) = (
            count(&detail.files),
            count(&detail.statuses),
            count(&detail.check_runs),
        );
        self.writer
            .write_commit_detail(&run.scope, run.tenant_id, detail)
            .await?;
        run.tally(|s| {
            s.commit_files_synced += files;
            s.commit_statuses_synced += statuses;
            s.check_runs_synced += checks;
        });
        self.mark_refined(entities::COMMIT, sha).await
    }

    async fn index_metadata(&self) -> Result<(), DomainError> {
        let run = &self.run;
        let repo_id = run.repo_id()?;
        let listing = self
            .github
            .list_metadata(&run.owner, &run.name, repo_id, &run.options)
            .await?;
        run.mark_complete(&listing.complete);
        let (labels, milestones, releases, branches, tags) = (
            count(&listing.labels),
            count(&listing.milestones),
            count(&listing.releases),
            count(&listing.branches),
            count(&listing.tags),
        );
        self.writer
            .write_metadata_listing(&run.scope, run.tenant_id, listing)
            .await?;
        run.tally(|s| {
            s.labels_synced += labels;
            s.milestones_synced += milestones;
            s.releases_synced += releases;
            s.branches_synced += branches;
            s.tags_synced += tags;
        });
        Ok(())
    }

    async fn index_actions(&self, ctx: &WorkerContext) -> Result<(), DomainError> {
        let run = &self.run;
        let repo_id = run.repo_id()?;
        let listing = self
            .github
            .list_actions(&run.owner, &run.name, repo_id, &run.options)
            .await?;

        if run.options.scope.collection.actions != CollectionMode::None {
            for workflow_run in &listing.workflow_runs {
                let entity_id = workflow_run.id.to_string();
                if !self
                    .needs_refinement(
                        entities::WORKFLOW_RUN,
                        &entity_id,
                        &workflow_run_inputs(workflow_run),
                    )
                    .await?
                {
                    continue;
                }
                self.seed(
                    ctx,
                    TaskPhase::Refinement,
                    entities::WORKFLOW_RUN,
                    Some(entity_id),
                    TaskPriority::NORMAL,
                );
            }
        }

        let (runs, deployments) = (count(&listing.workflow_runs), count(&listing.deployments));
        self.writer
            .write_actions_listing(&run.scope, run.tenant_id, listing)
            .await?;
        run.tally(|s| {
            s.workflow_runs_synced += runs;
            s.deployments_synced += deployments;
        });
        Ok(())
    }

    async fn refine_workflow_run(&self, task: &ExtractionTask) -> Result<(), DomainError> {
        let run = &self.run;
        let repo_id = run.repo_id()?;
        let run_id = entity_number(task)?;
        let jobs = self
            .github
            .refine_workflow_run(&run.owner, &run.name, repo_id, run_id, &run.options)
            .await?;
        let count = count(&jobs);
        self.writer
            .write_workflow_jobs(&run.scope, run.tenant_id, jobs)
            .await?;
        run.tally(|s| s.workflow_jobs_synced += count);
        self.mark_refined(entities::WORKFLOW_RUN, &run_id.to_string())
            .await
    }
}

#[async_trait]
impl Worker for MirrorWorker {
    fn handles(&self, phase: TaskPhase, entity_type: &str) -> bool {
        match phase {
            TaskPhase::Discovery => entity_type == REPOSITORY_ENTITY,
            TaskPhase::Indexing => matches!(
                entity_type,
                families::ISSUES
                    | families::PULL_REQUESTS
                    | families::COMMITS
                    | families::METADATA
                    | families::ACTIONS
            ),
            TaskPhase::Refinement => matches!(
                entity_type,
                entities::ISSUE
                    | entities::PULL_REQUEST
                    | entities::COMMIT
                    | entities::WORKFLOW_RUN
            ),
            TaskPhase::Verification => entity_type == entities::PULL_REQUEST,
            TaskPhase::ChangeDetection => false,
        }
    }

    async fn execute(&self, ctx: &WorkerContext, task: &ExtractionTask) -> Result<(), DomainError> {
        match (task.phase, task.entity_type.as_str()) {
            (TaskPhase::Discovery, _) => self.discover(ctx).await,
            (TaskPhase::Indexing, families::ISSUES) => self.index_issues(ctx).await,
            (TaskPhase::Indexing, families::PULL_REQUESTS) => self.index_pull_requests(ctx).await,
            (TaskPhase::Indexing, families::COMMITS) => self.index_commits(ctx).await,
            (TaskPhase::Indexing, families::METADATA) => self.index_metadata().await,
            (TaskPhase::Indexing, families::ACTIONS) => self.index_actions(ctx).await,
            (TaskPhase::Refinement, entities::ISSUE) => self.refine_issue(task).await,
            (TaskPhase::Refinement | TaskPhase::Verification, entities::PULL_REQUEST) => {
                self.refine_pull_request(ctx, task).await
            }
            (TaskPhase::Refinement, entities::COMMIT) => self.refine_commit(task).await,
            (TaskPhase::Refinement, entities::WORKFLOW_RUN) => self.refine_workflow_run(task).await,
            (phase, other) => Err(DomainError::internal(format!(
                "no handler for {phase} task of type {other}"
            ))),
        }
    }
}

fn issue_inputs(issue: &IssueRecord) -> GateInputs {
    GateInputs {
        fingerprint: change_gate::fingerprint(vec![
            ("updated_at", issue.updated_at.clone()),
            ("state", issue.state.clone()),
            ("closed_at", issue.closed_at.clone().unwrap_or_default()),
            ("labels", issue.labels_json.clone().unwrap_or_default()),
            (
                "assignees",
                issue.assignees_json.clone().unwrap_or_default(),
            ),
            ("locked", issue.locked.unwrap_or(false).to_string()),
        ]),
        child_counts_hash: change_gate::child_counts_hash(&[("comments", issue.comments_count)]),
        updated_at: Some(issue.updated_at.clone()),
        node_id: issue.node_id.clone(),
        terminal: issue.state != "open",
    }
}

fn pull_inputs(pull: &PullRequestRecord) -> GateInputs {
    GateInputs {
        fingerprint: change_gate::fingerprint(vec![
            ("updated_at", pull.updated_at.clone()),
            ("state", pull.state.clone()),
            ("draft", pull.draft.to_string()),
            ("merged", pull.merged.to_string()),
            ("merged_at", pull.merged_at.clone().unwrap_or_default()),
            ("closed_at", pull.closed_at.clone().unwrap_or_default()),
            ("head_sha", pull.head_sha.clone().unwrap_or_default()),
            ("base_sha", pull.base_sha.clone().unwrap_or_default()),
            ("labels", pull.labels_json.clone().unwrap_or_default()),
            ("assignees", pull.assignees_json.clone().unwrap_or_default()),
            (
                "reviewers",
                pull.requested_reviewers_json.clone().unwrap_or_default(),
            ),
        ]),
        child_counts_hash: change_gate::child_counts_hash(&[("comments", pull.comments_count)]),
        updated_at: Some(pull.updated_at.clone()),
        node_id: pull.node_id.clone(),
        terminal: pull.state != "open",
    }
}

fn commit_inputs(commit: &CommitRecord, with_ci: bool) -> GateInputs {
    GateInputs {
        fingerprint: change_gate::fingerprint(vec![("sha", commit.sha.clone())]),
        child_counts_hash: None,
        updated_at: commit.committed_at.clone(),
        node_id: None,
        terminal: !with_ci,
    }
}

fn workflow_run_inputs(run: &WorkflowRunRecord) -> GateInputs {
    GateInputs {
        fingerprint: change_gate::fingerprint(vec![
            ("updated_at", run.updated_at.clone()),
            ("status", run.status.clone().unwrap_or_default()),
            ("conclusion", run.conclusion.clone().unwrap_or_default()),
            ("run_attempt", run.run_attempt.to_string()),
            ("head_sha", run.head_sha.clone()),
        ]),
        child_counts_hash: None,
        updated_at: Some(run.updated_at.clone()),
        node_id: None,
        terminal: run.conclusion.is_some(),
    }
}
/// A number-keyed task's `entity_id`, parsed.
fn entity_number(task: &ExtractionTask) -> Result<i64, DomainError> {
    task.entity_id
        .as_deref()
        .and_then(|id| id.parse().ok())
        .ok_or_else(|| {
            DomainError::internal(format!(
                "{} task without a numeric entity id: {:?}",
                task.entity_type, task.entity_id
            ))
        })
}

fn count<T>(items: &[T]) -> u64 {
    u64::try_from(items.len()).unwrap_or(u64::MAX)
}
