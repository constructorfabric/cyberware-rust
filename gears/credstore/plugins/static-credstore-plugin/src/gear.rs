//! `ToolKit` gear registration for the static credential backend.
//!
//! Loads and validates configuration, registers its GTS plugin instance, and
//! publishes a scoped `CredStorePluginClientV1` through `ClientHub`.

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use credstore_sdk::{CredStorePluginClientV1, CredStorePluginSpecV1};
use toolkit::Gear;
use toolkit::client_hub::ClientScope;
use toolkit::context::GearCtx;
use toolkit::gts::PluginV1;
use tracing::info;
use types_registry_sdk::{RegisterResult, TypesRegistryClient};

use crate::config::StaticCredStorePluginConfig;
use crate::domain::Service;

/// Static credstore plugin gear.
///
/// Serves pre-configured secrets from YAML configuration for development and testing.
#[toolkit::gear(
    name = "static-credstore-plugin",
    deps = [types_registry]
)]
pub struct StaticCredStorePlugin {
    service: OnceLock<Arc<Service>>,
}

impl Default for StaticCredStorePlugin {
    fn default() -> Self {
        Self {
            service: OnceLock::new(),
        }
    }
}

#[async_trait]
impl Gear for StaticCredStorePlugin {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        // Load configuration
        let cfg: StaticCredStorePluginConfig = ctx.config_expanded_or_default()?;

        info!(
            vendor = %cfg.vendor,
            priority = cfg.priority,
            "Loaded plugin configuration"
        );

        // Create service from config (validate early, before registration).
        let service = Arc::new(Service::from_config(&cfg)?);

        // Build registration payload and instance id for this plugin.
        let (instance_id, instance_json) = PluginV1::<CredStorePluginSpecV1>::build_registration(
            "cf.core._.static_credstore.v1",
            cfg.vendor.clone(),
            cfg.priority,
        )?;

        // Publish to types-registry.
        let registry = ctx.client_hub().get::<dyn TypesRegistryClient>()?;
        let results = registry.register(vec![instance_json]).await?;
        RegisterResult::ensure_all_ok(&results)?;

        // All fallible steps done — commit service to shared state
        self.service
            .set(service.clone())
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;

        // Register scoped client in ClientHub
        let api: Arc<dyn CredStorePluginClientV1> = service;
        ctx.client_hub()
            .register_scoped::<dyn CredStorePluginClientV1>(ClientScope::gts_id(&instance_id), api);

        info!(instance_id = %instance_id);
        Ok(())
    }
}
