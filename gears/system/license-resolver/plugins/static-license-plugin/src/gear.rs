//! Static license resolver plugin gear.

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use license_resolver_sdk::{LicenseResolverPluginClient, LicenseResolverPluginSpecV1};
use toolkit::Gear;
use toolkit::client_hub::ClientScope;
use toolkit::context::GearCtx;
use toolkit::gts::PluginV1;
use tracing::info;
use types_registry_sdk::{RegisterResult, TypesRegistryClient};

use crate::config::StaticLicensePluginConfig;
use crate::domain::Service;

/// GTS instance suffix identifying this backend implementation.
///
/// The full instance id is this appended to
/// [`LicenseResolverPluginSpecV1`]'s type id — that is the id the gateway
/// resolves the scoped client under, and the one a deployment check looks for in
/// types-registry.
pub const INSTANCE_SUFFIX: &str = "cf.builtin.static_license_resolver.plugin.v1";

/// Static license resolver plugin gear.
///
/// The gateway's SDK publishes the plugin spec via link-time inventory; this gear
/// registers its own instance (vendor + priority) and its scoped client.
#[toolkit::gear(
    name = "static-license-plugin",
    deps = [types_registry]
)]
pub struct StaticLicensePlugin {
    service: OnceLock<Arc<Service>>,
}

impl Default for StaticLicensePlugin {
    fn default() -> Self {
        Self {
            service: OnceLock::new(),
        }
    }
}

#[async_trait]
impl Gear for StaticLicensePlugin {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        if self.service.get().is_some() {
            anyhow::bail!("{} gear already initialized", Self::MODULE_NAME);
        }

        let cfg: StaticLicensePluginConfig = ctx.config_or_default()?;
        info!(
            vendor = %cfg.vendor,
            priority = cfg.priority,
            grant_count = cfg.grants.len(),
            "Loaded plugin configuration"
        );

        // Built before registering anything: an invalid rule set must abort
        // startup rather than advertise a backend that denies everything.
        let service = Arc::new(Service::from_config(&cfg)?);

        let (instance_id, instance_json) =
            PluginV1::<LicenseResolverPluginSpecV1>::build_registration(
                INSTANCE_SUFFIX,
                cfg.vendor.clone(),
                cfg.priority,
            )?;

        let registry = ctx.client_hub().get::<dyn TypesRegistryClient>()?;
        let results = registry.register(vec![instance_json]).await?;
        RegisterResult::ensure_all_ok(&results)?;

        self.service
            .set(service.clone())
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;

        let api: Arc<dyn LicenseResolverPluginClient> = service;
        ctx.client_hub()
            .register_scoped::<dyn LicenseResolverPluginClient>(
                ClientScope::gts_id(&instance_id),
                api,
            );

        info!(instance_id = %instance_id);
        Ok(())
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "gear_tests.rs"]
mod gear_tests;
