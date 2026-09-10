//! `ToolKit` gear declaration, dependency wiring, and managed lifecycle.
//!
//! Initialization builds the domain service and registers its SDK client and
//! REST routes. The lifecycle entry runs no resident background loop
//! (ADR-0006 D7): hygiene (expired-row and garbage-collection sweeps) is the
//! periodic `credstore gc` maintenance job, invoked by an operator-chosen
//! scheduler outside this gear's own process lifetime (Phase 3), not a timer
//! in `serve`.

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use authz_resolver_sdk::{AuthZResolverApi, PolicyEnforcer};
use tenant_resolver_sdk::TenantResolverClient;
use tokio_util::sync::CancellationToken;
use toolkit::api::OpenApiRegistry;
use toolkit::contracts::{DatabaseCapability, SystemCapability};
use toolkit::lifecycle::ReadySignal;
use toolkit::{Gear, GearCtx, RestApiCapability};
use toolkit_db::DBProvider;
use tracing::info;

use types_registry_sdk::TypesRegistryClient;

use crate::client::CredStoreLocalClient;
use crate::config::CredStoreConfig;
use crate::domain::ports::metrics::CredStoreMetricsPort;
use crate::domain::secret::service::{GcSettings, Service};
use crate::infra::metrics::CredStoreMetricsMeter;
use crate::infra::plugin_select::GtsCredStorePluginSelector;
use crate::infra::storage::repo_impl::SecretRepoImpl;
use crate::infra::tenant_resolver::TenantResolverDir;
use crate::infra::types_registry::GtsSecretTypeResolver;

// `system` capability is required in this platform: consumers like `oagw` are
// system modules and resolve `CredStoreClientV1` from the ClientHub during their
// `init`. System modules initialize before non-system ones
// (`modules_by_system_priority`), so credstore must also be a system module to
// register its client before those consumers init.
#[toolkit::gear(
    name = "credstore",
    deps = [authz_resolver, tenant_resolver, types_registry],
    capabilities = [system, db, rest, stateful],
    lifecycle(entry = "serve", stop_timeout = "30s", await_ready)
)]
pub struct CredStoreGear {
    service: OnceLock<Arc<Service>>,
}

impl Default for CredStoreGear {
    fn default() -> Self {
        Self {
            service: OnceLock::new(),
        }
    }
}

impl CredStoreGear {
    #[allow(
        clippy::redundant_pub_crate,
        reason = "module-private serve entry-point invoked by the toolkit runtime"
    )]
    pub(crate) async fn serve(
        self: Arc<Self>,
        cancel: CancellationToken,
        ready: ReadySignal,
    ) -> anyhow::Result<()> {
        if self.service.get().is_none() {
            anyhow::bail!("credstore: serve invoked before init");
        }

        ready.notify();
        info!(
            target: "credstore.lifecycle",
            "credstore gear serving; no resident background work - hygiene runs via the \
             `credstore gc` maintenance job on an operator-chosen schedule"
        );

        cancel.cancelled().await;

        info!(target: "credstore.lifecycle", "credstore lifecycle cancelled");
        Ok(())
    }
}

#[async_trait]
impl Gear for CredStoreGear {
    #[tracing::instrument(skip_all, fields(module = "credstore"))]
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let cfg: CredStoreConfig = ctx.config_or_default()?;
        cfg.validate()
            .map_err(|err| anyhow::anyhow!("credstore config invalid: {err}"))?;
        info!(vendor = %cfg.vendor, "initializing credstore module");

        let db_raw = ctx.db_required()?;
        let db: Arc<DBProvider<crate::domain::error::DomainError>> =
            Arc::new(DBProvider::new(db_raw.db()));

        let repo = Arc::new(SecretRepoImpl::new(Arc::clone(&db)));

        let authz_client = ctx
            .client_hub()
            .get::<dyn AuthZResolverApi>()
            .map_err(|e| anyhow::anyhow!("failed to get AuthZResolverApi: {e}"))?;
        // No PDP capabilities: credstore projects no authorization tables
        // (no `tenant_closure`), so the PDP hands it pre-expanded, flat
        // tenant predicates (AUTHZ_USAGE_SCENARIOS S09–S11).
        let enforcer = PolicyEnforcer::new(authz_client);
        info!("authz-resolver client resolved from client hub; PolicyEnforcer wired");

        let tr_client = ctx
            .client_hub()
            .get::<dyn TenantResolverClient>()
            .map_err(|e| anyhow::anyhow!("failed to get TenantResolverClient: {e}"))?;

        let metrics: Arc<dyn CredStoreMetricsPort> = Arc::new(CredStoreMetricsMeter::from_global());

        let dir = Arc::new(TenantResolverDir::new(
            tr_client,
            Arc::clone(&metrics),
            cfg.hierarchy.ancestor_cache_ttl_secs,
        ));

        let plugins = Arc::new(GtsCredStorePluginSelector::new(
            ctx.client_hub(),
            cfg.vendor.clone(),
        ));

        // Fail-closed: without the types-registry client no secret type can
        // be resolved, so credstore must not come up (types-registry is a
        // hard `deps` and initializes first).
        let registry = ctx
            .client_hub()
            .get::<dyn TypesRegistryClient>()
            .map_err(|e| anyhow::anyhow!("failed to get TypesRegistryClient: {e}"))?;
        let types = Arc::new(GtsSecretTypeResolver::new(registry, Arc::clone(&metrics)));
        info!("types-registry client resolved from client hub; secret-type resolver wired");

        let svc = Arc::new(Service::new(
            repo,
            dir,
            enforcer,
            plugins,
            types,
            metrics,
            GcSettings {
                pending_max_age_secs: cfg.gc.pending_max_age_secs,
                batch_size: cfg.gc.batch_size,
            },
        ));

        self.service
            .set(Arc::clone(&svc))
            .map_err(|_| anyhow::anyhow!("{} module already initialized", Self::MODULE_NAME))?;

        let client: Arc<dyn credstore_sdk::CredStoreClientV1> =
            Arc::new(CredStoreLocalClient::new(svc));
        ctx.client_hub()
            .register::<dyn credstore_sdk::CredStoreClientV1>(client);

        info!("credstore module initialized");
        Ok(())
    }
}

// Empty system capability: credstore needs no pre_init/post_init work, only the
// system-priority init ordering (see the module attribute above).
impl SystemCapability for CredStoreGear {}

impl DatabaseCapability for CredStoreGear {
    fn migrations(&self) -> Vec<Box<dyn sea_orm_migration::MigrationTrait>> {
        use sea_orm_migration::MigratorTrait;
        info!("providing credstore database migrations");
        crate::infra::storage::migrations::Migrator::migrations()
    }
}

impl RestApiCapability for CredStoreGear {
    fn register_rest(
        &self,
        _ctx: &GearCtx,
        router: axum::Router,
        openapi: &dyn OpenApiRegistry,
    ) -> anyhow::Result<axum::Router> {
        info!("registering credstore REST routes");
        let svc = self
            .service
            .get()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("credstore Service not initialized"))?;
        let router = crate::api::rest::register_routes(router, openapi, svc);
        info!("credstore REST routes registered");
        Ok(router)
    }
}
