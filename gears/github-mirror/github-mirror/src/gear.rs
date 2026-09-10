use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use axum::Router;
use toolkit::api::OpenApiRegistry;
use toolkit::{Gear, GearCtx, Healthcheck, HealthcheckResult, RestApiCapability};
use tracing::info;

use authz_resolver_sdk::{AuthZResolverApi, PolicyEnforcer};
use github_mirror_sdk::GithubMirrorClientV1;

use crate::api::rest::routes;
use crate::config::GithubMirrorConfig;
use crate::domain::local_client::LocalClient;
use crate::domain::ports::github::GithubPort;
use crate::domain::service::{Service, ServiceConfig};
use crate::infra::github::client::GithubClient;
use crate::infra::storage::sea_orm_repo::{
    SeaOrmBranchRepository, SeaOrmCheckRunRepository, SeaOrmCommentRepository,
    SeaOrmCommitCommentRepository, SeaOrmCommitFileRepository, SeaOrmCommitRepository,
    SeaOrmCommitStatusRepository, SeaOrmContributorRepository, SeaOrmDeploymentRepository,
    SeaOrmIssueEventRepository, SeaOrmIssueReactionRepository, SeaOrmIssueRepository,
    SeaOrmIssueTimelineRepository, SeaOrmLabelRepository, SeaOrmMilestoneRepository,
    SeaOrmPullRequestCommitRepository, SeaOrmPullRequestFileRepository,
    SeaOrmPullRequestRepository, SeaOrmReleaseRepository, SeaOrmRepoRepository,
    SeaOrmReviewCommentRepository, SeaOrmReviewRepository, SeaOrmReviewThreadRepository,
    SeaOrmSyncWriter, SeaOrmTagRepository, SeaOrmWorkflowJobRepository,
    SeaOrmWorkflowRunRepository,
};

type ConcreteService = Service;

// This attribute is the one place the gear's name is written:
// `service::GEAR_NAME` aliases the `MODULE_NAME` const it generates.
#[toolkit::gear(
    name = "github-mirror",
    deps = [authz_resolver],
    capabilities = [rest, db]
)]
#[derive(Default)]
pub struct GithubMirrorGear {
    service: OnceLock<Arc<ConcreteService>>,
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
        let github: Arc<dyn GithubPort> = Arc::new(GithubClient::new(
            cfg.api_base_url.clone(),
            cfg.resolved_token()?,
        )?);

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
            Arc::new(SeaOrmSyncWriter::new(Arc::clone(&db))),
            github,
            policy_enforcer,
            ServiceConfig {
                api_base_url: cfg.api_base_url,
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
        assert_eq!(gear.migrations().len(), 38);
    }
}
