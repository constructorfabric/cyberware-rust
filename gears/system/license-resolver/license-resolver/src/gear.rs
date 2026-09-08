//! License resolver gear.

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use license_resolver_sdk::LicenseResolverClient;
use toolkit::Gear;
use toolkit::context::GearCtx;
use toolkit::contracts::SystemCapability;
use tracing::info;

use crate::config::LicenseResolverConfig;
use crate::domain::{ContractValidator, LicenseResolverLocalClient, Service};
use crate::infra::{GtsContractRegistry, metrics};

/// License Resolver gear.
///
/// The licensing base types and the `LicenseResolverPluginSpecV1` schema reach
/// `types-registry` on their own via the `toolkit-gts` link-time inventory — no
/// per-init registration is needed here.
#[toolkit::gear(
    name = "license-resolver",
    deps = [types_registry],
    capabilities = [system]
)]
pub(crate) struct LicenseResolver {
    service: OnceLock<Arc<Service>>,
}

impl Default for LicenseResolver {
    fn default() -> Self {
        Self {
            service: OnceLock::new(),
        }
    }
}

// Marked as `system` so that init() runs in the system-gear phase, making the
// client available in ClientHub before other system gears that gate on a license.
impl SystemCapability for LicenseResolver {}

#[async_trait]
impl Gear for LicenseResolver {
    #[tracing::instrument(skip_all, fields(vendor))]
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let cfg: LicenseResolverConfig = ctx.config_or_default()?;
        tracing::Span::current().record("vendor", cfg.vendor.as_str());
        info!(vendor = %cfg.vendor);

        let hub = ctx.client_hub();
        let validator = ContractValidator::new(Arc::new(GtsContractRegistry::new(hub.clone())));
        let svc = Arc::new(Service::new(
            hub,
            cfg.vendor.clone(),
            validator,
            metrics::build_default_adapter(&cfg.vendor),
        ));
        self.service
            .set(svc.clone())
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;

        let api: Arc<dyn LicenseResolverClient> = Arc::new(LicenseResolverLocalClient::new(svc));
        ctx.client_hub().register::<dyn LicenseResolverClient>(api);

        Ok(())
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "gear_tests.rs"]
mod gear_tests;
