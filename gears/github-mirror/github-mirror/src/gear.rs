use std::collections::VecDeque;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use async_trait::async_trait;
use axum::Router;
use tokio::sync::mpsc;
use tokio::task::{JoinHandle, JoinSet};
use tokio_util::sync::CancellationToken;
use toolkit::api::OpenApiRegistry;
use toolkit::contracts::RunnableCapability;
use toolkit::{Gear, GearCtx, Healthcheck, HealthcheckResult, RestApiCapability};
use tracing::{info, warn};
use uuid::Uuid;

use authz_resolver_sdk::{AuthZResolverApi, PolicyEnforcer};
use github_mirror_sdk::GithubMirrorClientV1;

use crate::api::rest::routes;
use crate::config::GithubMirrorConfig;
use crate::domain::local_client::LocalClient;
use crate::domain::ports::github::GithubPort;
use crate::domain::service::{Service, ServiceConfig, SyncJob};
use crate::infra::github::client::GithubClient;
use crate::infra::storage::sea_orm_repo::{
    SeaOrmBranchRepository, SeaOrmCheckRunRepository, SeaOrmCommentRepository,
    SeaOrmCommitCommentRepository, SeaOrmCommitFileRepository, SeaOrmCommitRepository,
    SeaOrmCommitStatusRepository, SeaOrmContributorRepository, SeaOrmDeploymentRepository,
    SeaOrmEntityFingerprintRepository, SeaOrmHttpCache, SeaOrmIssueEventRepository,
    SeaOrmIssueReactionRepository, SeaOrmIssueRepository, SeaOrmIssueTimelineRepository,
    SeaOrmLabelRepository, SeaOrmMilestoneRepository, SeaOrmPullRequestCommitRepository,
    SeaOrmPullRequestFileRepository, SeaOrmPullRequestRepository, SeaOrmReleaseRepository,
    SeaOrmRepoRepository, SeaOrmRepoSyncStatusRepository, SeaOrmReviewCommentRepository,
    SeaOrmReviewRepository, SeaOrmReviewThreadRepository, SeaOrmSyncSessionRepository,
    SeaOrmSyncWatermarkRepository, SeaOrmSyncWriter, SeaOrmTagRepository,
    SeaOrmWorkflowJobRepository, SeaOrmWorkflowRunRepository,
};

type ConcreteService = Service;

// This attribute is the one place the gear's name is written:
// `service::GEAR_NAME` aliases the `MODULE_NAME` const it generates.
#[toolkit::gear(
    name = "github-mirror",
    deps = [authz_resolver],
    capabilities = [rest, db, stateful]
)]
#[derive(Default)]
pub struct GithubMirrorGear {
    service: OnceLock<Arc<ConcreteService>>,
    sync_cancel_token: Mutex<Option<CancellationToken>>,
    sync_handle: Mutex<Option<JoinHandle<()>>>,
}

impl toolkit::contracts::DatabaseCapability for GithubMirrorGear {
    fn migrations(&self) -> Vec<Box<dyn sea_orm_migration::MigrationTrait>> {
        use sea_orm_migration::MigratorTrait;
        crate::infra::storage::migrations::Migrator::migrations()
    }
}

#[async_trait]
impl Gear for GithubMirrorGear {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let cfg: GithubMirrorConfig = ctx.config_or_default()?;
        // Fails startup on a malformed or non-HTTP base URL rather than
        // letting every later fetch build garbage requests from it.
        cfg.resolved_api_base_url()
            .map_err(|e| anyhow::anyhow!("invalid github-mirror config: {e}"))?;
        info!(gear = Self::MODULE_NAME, api_base_url = %cfg.api_base_url, "Initializing gear");

        let db = Arc::new(ctx.db_required()?);
        let repo = Arc::new(SeaOrmRepoRepository::new(Arc::clone(&db)));
        let issues = Arc::new(SeaOrmIssueRepository::new(Arc::clone(&db)));
        let pull_requests = Arc::new(SeaOrmPullRequestRepository::new(Arc::clone(&db)));
        let commits = Arc::new(SeaOrmCommitRepository::new(Arc::clone(&db)));
        let comments = Arc::new(SeaOrmCommentRepository::new(Arc::clone(&db)));
        let review_comments = Arc::new(SeaOrmReviewCommentRepository::new(Arc::clone(&db)));
        let reviews = Arc::new(SeaOrmReviewRepository::new(Arc::clone(&db)));
        let labels = Arc::new(SeaOrmLabelRepository::new(Arc::clone(&db)));
        let milestones = Arc::new(SeaOrmMilestoneRepository::new(Arc::clone(&db)));
        let releases = Arc::new(SeaOrmReleaseRepository::new(Arc::clone(&db)));
        let branches = Arc::new(SeaOrmBranchRepository::new(Arc::clone(&db)));
        let contributors = Arc::new(SeaOrmContributorRepository::new(Arc::clone(&db)));
        let workflow_runs = Arc::new(SeaOrmWorkflowRunRepository::new(Arc::clone(&db)));
        let pull_request_files = Arc::new(SeaOrmPullRequestFileRepository::new(Arc::clone(&db)));
        let tags = Arc::new(SeaOrmTagRepository::new(Arc::clone(&db)));
        let commit_files = Arc::new(SeaOrmCommitFileRepository::new(Arc::clone(&db)));
        let review_threads = Arc::new(SeaOrmReviewThreadRepository::new(Arc::clone(&db)));
        let commit_comments = Arc::new(SeaOrmCommitCommentRepository::new(Arc::clone(&db)));
        let issue_events = Arc::new(SeaOrmIssueEventRepository::new(Arc::clone(&db)));
        let deployments = Arc::new(SeaOrmDeploymentRepository::new(Arc::clone(&db)));
        let pull_request_commits =
            Arc::new(SeaOrmPullRequestCommitRepository::new(Arc::clone(&db)));
        let commit_statuses = Arc::new(SeaOrmCommitStatusRepository::new(Arc::clone(&db)));
        let workflow_jobs = Arc::new(SeaOrmWorkflowJobRepository::new(Arc::clone(&db)));
        let issue_reactions = Arc::new(SeaOrmIssueReactionRepository::new(Arc::clone(&db)));
        let check_runs = Arc::new(SeaOrmCheckRunRepository::new(Arc::clone(&db)));
        let issue_timeline = Arc::new(SeaOrmIssueTimelineRepository::new(Arc::clone(&db)));
        let sync_sessions = Arc::new(SeaOrmSyncSessionRepository::new(Arc::clone(&db)));
        let repo_sync_status = Arc::new(SeaOrmRepoSyncStatusRepository::new(Arc::clone(&db)));
        // Conditional requests: a stored ETag replayed as If-None-Match turns a
        // repeat sync into 304s, which GitHub does not charge against the rate
        // limit (#4630).
        let http_cache = Arc::new(SeaOrmHttpCache::new(
            Arc::clone(&db),
            cfg.resolved_compression()?,
        ));
        let github: Arc<dyn GithubPort> = Arc::new(
            GithubClient::with_cache(cfg.api_base_url.clone(), cfg.resolved_token()?, http_cache)?
                .with_max_concurrent_requests(cfg.max_concurrent_requests),
        );

        let authz = ctx
            .client_hub()
            .get::<dyn AuthZResolverApi>()
            .map_err(|e| anyhow::anyhow!("failed to get AuthZ resolver: {e}"))?;
        let policy_enforcer = PolicyEnforcer::new(authz);

        let service = Arc::new(Service::new(
            Arc::clone(&db),
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
            Arc::new(SeaOrmSyncWriter::new(Arc::clone(&db))),
            Arc::new(SeaOrmEntityFingerprintRepository::new(Arc::clone(&db))),
            Arc::new(SeaOrmSyncWatermarkRepository::new(Arc::clone(&db))),
            github,
            policy_enforcer,
            ServiceConfig {
                api_base_url: cfg.api_base_url,
                scope: cfg.scope,
                max_concurrent_syncs: cfg.max_concurrent_syncs,
                max_concurrent_tasks: cfg.max_concurrent_tasks,
            },
        ));

        self.service
            .set(service.clone())
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;

        let client: Arc<dyn GithubMirrorClientV1> = Arc::new(LocalClient::new(service));
        ctx.client_hub()
            .register::<dyn GithubMirrorClientV1>(client);

        Ok(())
    }
}

/// Take a lock, keeping the data even if a previous holder panicked.
///
/// Both mutexes guard a single `Option` that only `start` and `stop` touch,
/// so a poisoned one holds nothing half-written and refusing to start over it
/// would be worse than carrying on.
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Jobs the pool parks while every worker is busy. Matches the sync channel's
/// own depth, so a caller starts seeing the queue-full error at roughly twice
/// that many outstanding syncs rather than never.
const SYNC_BACKLOG_LIMIT: usize = 64;

/// Jobs waiting for a free worker, held one queue per tenant.
///
/// The pool takes the next job from the next tenant in turn, so a tenant that
/// queues fifty repositories delays only itself: every other tenant still gets
/// a worker on its next turn (PRD §6.1 "prevent starvation and ensure fair
/// scheduling"). Within one tenant the order stays first in, first out.
///
/// The per-entity `TaskQueue` the phase runner needs (#4632 slice 6) sits one
/// level below this one: this queue orders whole repository syncs.
#[derive(Default)]
struct SyncQueue {
    /// Front = the tenant whose turn is next; each entry is that tenant's
    /// jobs, oldest first.
    queue: VecDeque<(Uuid, VecDeque<SyncJob>)>,
}

impl SyncQueue {
    fn enqueue(&mut self, job: SyncJob) {
        let tenant_id = job.ctx.subject_tenant_id();
        match self.queue.iter_mut().find(|(id, _)| *id == tenant_id) {
            Some((_, jobs)) => jobs.push_back(job),
            None => self.queue.push_back((tenant_id, VecDeque::from([job]))),
        }
    }

    fn claim_next(&mut self) -> Option<SyncJob> {
        let (tenant_id, mut jobs) = self.queue.pop_front()?;
        let job = jobs.pop_front()?;
        if !jobs.is_empty() {
            self.queue.push_back((tenant_id, jobs));
        }
        Some(job)
    }

    fn len(&self) -> usize {
        self.queue.iter().map(|(_, jobs)| jobs.len()).sum()
    }
}

/// What woke the pool loop.
enum PoolEvent {
    /// The gear is stopping.
    Cancelled,
    /// A caller queued another sync.
    Queued(SyncJob),
    /// The job channel closed; no more syncs will arrive.
    QueueClosed,
    /// A running sync ended.
    Finished(Result<(), tokio::task::JoinError>),
}

/// Runs queued repository syncs, up to `max_concurrent` at a time.
///
/// The counterpart of the reference implementation's `RepoPhaseRunner`, one
/// level up: that one runs the phases of a single repository, this one runs
/// whole repositories.
struct SyncPoolRunner {
    service: Arc<ConcreteService>,
    /// Jobs as `enqueue_sync` posted them.
    jobs: mpsc::Receiver<SyncJob>,
    max_concurrent: usize,
    cancel: CancellationToken,
}

impl SyncPoolRunner {
    /// Start syncs until every worker is busy or nothing is waiting.
    fn fill_workers(&self, queue: &mut SyncQueue, in_flight: &mut JoinSet<()>) {
        while in_flight.len() < self.max_concurrent {
            let Some(job) = queue.claim_next() else { break };
            let service = self.service.clone();
            let cancel = self.cancel.clone();
            in_flight.spawn(async move {
                if let Err(e) = service.run_sync_job(&job, &cancel).await {
                    warn!(
                        session_id = %job.session_id,
                        repository = %format!("{}/{}", job.owner, job.name),
                        error = %e,
                        "sync outcome could not be recorded"
                    );
                }
            });
        }
    }

    /// Wait for the next thing to happen: cancellation, a new job, or a
    /// finished sync.
    async fn next_event(
        &mut self,
        parked: usize,
        in_flight: &mut JoinSet<()>,
        draining: bool,
    ) -> PoolEvent {
        tokio::select! {
            () = self.cancel.cancelled(), if !draining => PoolEvent::Cancelled,
            // Stop reading once as many jobs are parked as the channel itself
            // holds, so backpressure still reaches the caller.
            received = self.jobs.recv(), if !draining && parked < SYNC_BACKLOG_LIMIT => {
                received.map_or(PoolEvent::QueueClosed, PoolEvent::Queued)
            }
            Some(joined) = in_flight.join_next(), if !in_flight.is_empty() => {
                PoolEvent::Finished(joined)
            }
        }
    }

    /// Act on one event and report whether the pool should stop taking work.
    fn handle_event(
        event: PoolEvent,
        queue: &mut SyncQueue,
        in_flight: usize,
        draining: bool,
    ) -> bool {
        match event {
            PoolEvent::Cancelled => Self::report_stopping(in_flight),
            PoolEvent::Queued(job) => {
                queue.enqueue(job);
                draining
            }
            PoolEvent::QueueClosed => Self::report_queue_closed(),
            PoolEvent::Finished(joined) => {
                Self::report_finished(&joined);
                draining
            }
        }
    }

    /// Split out because each `tracing` macro counts against
    /// `clippy::cognitive_complexity`, which caps `handle_event` at 20.
    fn report_stopping(in_flight: usize) -> bool {
        info!(
            in_flight,
            "github-mirror sync pool stopping; letting running syncs finish"
        );
        true
    }

    fn report_queue_closed() -> bool {
        info!("github-mirror sync queue closed");
        true
    }

    fn report_finished(joined: &Result<(), tokio::task::JoinError>) {
        if let Err(e) = joined {
            warn!(error = %e, "sync worker task did not finish cleanly");
        }
    }

    async fn run(mut self) {
        let mut in_flight: JoinSet<()> = JoinSet::new();
        let mut queue = SyncQueue::default();
        // Set once the pool stops taking new work - either the gear is
        // stopping or the job channel closed. In-flight syncs still finish.
        let mut draining = false;

        loop {
            if !draining {
                self.fill_workers(&mut queue, &mut in_flight);
            }
            if draining && in_flight.is_empty() {
                break;
            }
            let event = self.next_event(queue.len(), &mut in_flight, draining).await;
            draining = Self::handle_event(event, &mut queue, in_flight.len(), draining);
        }
        info!("github-mirror sync pool stopped");
    }
}

#[async_trait]
impl RunnableCapability for GithubMirrorGear {
    /// Start the sync worker pool: up to `max_concurrent_syncs` repositories
    /// sync at once, drawn from the service's job queue a tenant at a time.
    ///
    /// Before it starts, sessions left `queued` or `running` by a previous
    /// process are closed out as `interrupted` — the queue lives in memory, so
    /// nothing will ever pick them up again. The sweep happens here rather
    /// than in [`Self::stop`] because a killed process never reaches `stop`.
    async fn start(&self, cancel: CancellationToken) -> anyhow::Result<()> {
        let service = self
            .service
            .get()
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "{} service not initialized - init() must run before start()",
                    Self::MODULE_NAME
                )
            })?
            .clone();

        match service.sweep_interrupted_sessions().await {
            Ok(0) => {}
            Ok(swept) => info!(sessions = swept, "closed out interrupted sync sessions"),
            Err(e) => warn!(error = %e, "could not sweep interrupted sync sessions"),
        }

        let Some(jobs) = service.take_sync_receiver().await else {
            anyhow::bail!("{} sync worker already started", Self::MODULE_NAME);
        };

        let new_cancel_token = cancel.child_token();
        let max_concurrent = service.max_concurrent_syncs();
        let runner = SyncPoolRunner {
            service,
            jobs,
            max_concurrent,
            cancel: new_cancel_token.clone(),
        };
        let handle = tokio::spawn(runner.run());

        // Claiming the token and rejecting a second `start` happen under one
        // lock, so two callers cannot both believe they are first.
        let mut cancel_token = lock(&self.sync_cancel_token);
        if cancel_token.is_some() {
            handle.abort();
            anyhow::bail!("{} sync worker already started", Self::MODULE_NAME);
        }
        *cancel_token = Some(new_cancel_token);

        let mut sync_handle = lock(&self.sync_handle);
        *sync_handle = Some(handle);

        info!("github-mirror sync worker started");
        Ok(())
    }

    /// Stop the pool. It takes no more jobs and finishes the syncs already
    /// running; jobs still waiting are dropped, and any sync the framework's
    /// deadline cuts short leaves its session row `running` until the next
    /// startup sweep marks it `interrupted`. Resuming that work is #4632
    /// slice 6.
    async fn stop(&self, deadline_token: CancellationToken) -> anyhow::Result<()> {
        if let Some(token) = lock(&self.sync_cancel_token).take() {
            token.cancel();
        }

        let handle = lock(&self.sync_handle).take();
        if let Some(handle) = handle {
            tokio::select! {
                result = handle => {
                    if let Err(e) = result
                        && !e.is_cancelled()
                    {
                        warn!(error = ?e, "github-mirror sync worker task failed");
                    }
                }
                () = deadline_token.cancelled() => {
                    info!("github-mirror sync worker stop cancelled by framework deadline");
                }
            }
        }
        Ok(())
    }
}

impl RestApiCapability for GithubMirrorGear {
    fn register_rest(
        &self,
        _ctx: &GearCtx,
        router: Router,
        openapi: &dyn OpenApiRegistry,
    ) -> anyhow::Result<Router> {
        let service = self
            .service
            .get()
            .ok_or_else(|| anyhow::anyhow!("Service not initialized"))?
            .clone();

        let router = routes::register_routes(router, openapi, service);
        info!(gear = Self::MODULE_NAME, "REST routes registered");
        Ok(router)
    }

    /// Reports through the platform's aggregated `/readyz`/`/health` rather
    /// than only the gear's own always-200 `GET /health` endpoint. `None`
    /// before `init()` runs mirrors `register_rest`'s own defensive check —
    /// in practice this method is only ever called afterward.
    fn healthcheck(&self, _ctx: &GearCtx) -> Option<Arc<dyn Healthcheck>> {
        let service = self.service.get()?.clone();
        Some(Arc::new(GithubMirrorHealthcheck { service }))
    }
}

struct GithubMirrorHealthcheck {
    service: Arc<ConcreteService>,
}

#[async_trait]
impl Healthcheck for GithubMirrorHealthcheck {
    fn name(&self) -> &'static str {
        GithubMirrorGear::MODULE_NAME
    }

    /// A pooled-connection acquisition, no query — enough to catch the DB
    /// being unreachable without adding load for every readiness probe.
    async fn check(&self) -> HealthcheckResult {
        if self.service.db_reachable() {
            HealthcheckResult::healthy()
        } else {
            HealthcheckResult::unhealthy("database unreachable")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_gear_has_no_service_until_init() {
        let gear = GithubMirrorGear::default();
        assert!(gear.service.get().is_none());
    }

    #[test]
    fn gear_provides_all_migrations() {
        use toolkit::contracts::DatabaseCapability;
        let gear = GithubMirrorGear::default();
        assert_eq!(gear.migrations().len(), 45);
    }
}
