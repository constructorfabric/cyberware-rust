//! Gear declaration of quota-enforcement.
//!
//! `init` wires the PEP boundary and the domain service. The lifecycle entry
//! runs the fail-closed bootstrap before the ready signal. The REST surface
//! mounts into the platform `api-gateway`; the readiness check reports the
//! bootstrap state.

use std::sync::{Arc, OnceLock};

use anyhow::Context as _;
use async_trait::async_trait;
use authz_resolver_sdk::{AuthZResolverApi, PolicyEnforcer};
use tokio_util::sync::CancellationToken;
use toolkit::api::OpenApiRegistry;
use toolkit::client_hub::ClientHub;
use toolkit::context::GearCtx;
use toolkit::lifecycle::ReadySignal;
use toolkit::{Gear, Healthcheck, RestApiCapability};
use tracing::info;

use crate::api::healthcheck::ReadinessCheck;
use crate::api::rest::routes;
use crate::config::QuotaEnforcementConfig;
use crate::domain::{Admission, Bootstrap, PluginBinding, Readiness, Service};
use crate::infra::metrics;

const LOG_TARGET: &str = "qe.lifecycle";

/// Quota Enforcement gear.
///
/// The gear owns no database: persistence lives behind the storage plugin.
/// It is stateful because the bootstrap runs in the lifecycle entry and later
/// features host their sweepers there under child cancellation tokens.
// @cpt-dod:cpt-cf-quota-enforcement-dod-workspace-crates:p1
#[toolkit::gear(
    name = "quota-enforcement",
    deps = [authz_resolver, types_registry],
    capabilities = [rest, stateful],
    lifecycle(entry = "serve", stop_timeout = "30s", await_ready)
)]
pub struct QuotaEnforcementGear {
    service: OnceLock<Arc<Service>>,
    bootstrap: OnceLock<Bootstrap>,
    hub: OnceLock<Arc<ClientHub>>,
}

impl Default for QuotaEnforcementGear {
    fn default() -> Self {
        Self {
            service: OnceLock::new(),
            bootstrap: OnceLock::new(),
            hub: OnceLock::new(),
        }
    }
}

impl QuotaEnforcementGear {
    /// The domain service, once `init` ran.
    #[must_use]
    pub fn service(&self) -> Option<Arc<Service>> {
        self.service.get().cloned()
    }

    /// Lifecycle entry: bootstrap, signal ready, then idle until shutdown.
    ///
    /// Later features spawn their background tasks here under child tokens of
    /// `cancel`, after the ready signal.
    ///
    /// # Errors
    ///
    /// Returns an error when bootstrap fails or shutdown interrupts it. The
    /// ready signal is never sent in that case, so the gear is never marked
    /// running.
    pub(crate) async fn serve(
        self: Arc<Self>,
        cancel: CancellationToken,
        ready: ReadySignal,
    ) -> anyhow::Result<()> {
        let service = self
            .service
            .get()
            .cloned()
            .context("quota-enforcement: serve invoked before init")?;
        let bootstrap = self
            .bootstrap
            .get()
            .context("quota-enforcement: serve invoked before init")?;

        let bound = tokio::select! {
            biased;
            () = cancel.cancelled() => {
                anyhow::bail!("quota-enforcement: shutdown requested during bootstrap");
            }
            outcome = bootstrap.run() => outcome.context("quota-enforcement bootstrap failed")?,
        };
        service
            .bind(bound)
            .context("quota-enforcement: publish bootstrapped dependencies")?;

        ready.notify();
        info!(target: LOG_TARGET, "quota-enforcement is ready");

        cancel.cancelled().await;
        info!(target: LOG_TARGET, "quota-enforcement is stopping");
        Ok(())
    }
}

#[async_trait]
impl Gear for QuotaEnforcementGear {
    #[tracing::instrument(skip_all, fields(storage_vendor, coordination_vendor))]
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        if self.service.get().is_some() {
            anyhow::bail!("{} gear already initialized", Self::MODULE_NAME);
        }
        let cfg: QuotaEnforcementConfig = ctx.config_or_default()?;
        cfg.validate()?;
        tracing::Span::current().record("storage_vendor", cfg.storage_vendor.as_str());
        tracing::Span::current().record("coordination_vendor", cfg.coordination_vendor.as_str());

        // PEP boundary: the PDP client is a hard dependency. Without it the gear
        // fails init and never serves a permissive decision.
        let hub = ctx.client_hub();
        let authz: Arc<dyn AuthZResolverApi> = hub
            .get::<dyn AuthZResolverApi>()
            .with_context(|| format!("{} requires an authz-resolver client", Self::MODULE_NAME))?;
        let enforcer = PolicyEnforcer::new(authz);

        let metrics = metrics::build_default_adapter(&cfg.metrics);
        let readiness = Arc::new(Readiness::new());
        let admission = Admission::new(enforcer, metrics);
        let service = Arc::new(Service::new(admission, readiness.clone()));

        let binding = PluginBinding::new(
            hub.clone(),
            cfg.storage_vendor.clone(),
            cfg.coordination_vendor.clone(),
        );
        let bootstrap = Bootstrap::new(binding, hub.clone(), cfg.probe_lock_ttl(), readiness);

        self.hub
            .set(hub)
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;
        self.bootstrap
            .set(bootstrap)
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;
        self.service
            .set(service)
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;

        info!(target: LOG_TARGET, "quota-enforcement initialised; bootstrap runs in the lifecycle entry");
        Ok(())
    }
}

impl RestApiCapability for QuotaEnforcementGear {
    // @cpt-flow:cpt-cf-quota-enforcement-flow-gear-bootstrap:p1
    fn register_rest(
        &self,
        _ctx: &GearCtx,
        router: axum::Router,
        openapi: &dyn OpenApiRegistry,
    ) -> anyhow::Result<axum::Router> {
        // @cpt-begin:cpt-cf-quota-enforcement-flow-gear-bootstrap:p1:inst-boot-rest
        let service = self
            .service
            .get()
            .cloned()
            .context("quota-enforcement: register_rest invoked before init")?;
        Ok(routes::register_routes(router, openapi, service))
        // @cpt-end:cpt-cf-quota-enforcement-flow-gear-bootstrap:p1:inst-boot-rest
    }

    fn healthcheck(&self, _ctx: &GearCtx) -> Option<Arc<dyn Healthcheck>> {
        let service = self.service.get()?;
        Some(Arc::new(ReadinessCheck::new(service.readiness().clone())))
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "gear_tests.rs"]
mod gear_tests;
