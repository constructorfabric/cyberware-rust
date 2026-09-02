//! Gear declaration of the coordination plugin.

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use quota_enforcement_sdk::{CoordinationPluginV1, QuotaEnforcementCoordinationPluginSpecV1};
use toolkit::Gear;
use toolkit::client_hub::ClientScope;
use toolkit::context::GearCtx;
use toolkit::contracts::DatabaseCapability;
use toolkit::gts::PluginV1;
use tracing::info;
use types_registry_sdk::{RegisterResult, TypesRegistryClient};

use crate::config::CoordinationPluginConfig;
use crate::infra::DbCoordination;

/// GTS instance segment of this backend. The full instance id is the plugin
/// spec type id followed by this segment. The gear resolves the scoped client
/// under that id.
pub const INSTANCE_SEGMENT: &str = "cf.core._.qe_db_coordination.v1";

/// Database-backed coordination plugin gear.
///
/// `init` validates the configuration, binds the lock service to the gear's
/// database, publishes the plugin instance to the types registry, and
/// registers the scoped `CoordinationPluginV1` client. The runtime applies the
/// migrations before `init` through [`DatabaseCapability`].
#[toolkit::gear(
    name = "quota-enforcement-coordination-plugin",
    deps = [types_registry],
    capabilities = [db]
)]
pub struct CoordinationPlugin {
    service: OnceLock<Arc<DbCoordination>>,
}

impl Default for CoordinationPlugin {
    fn default() -> Self {
        Self {
            service: OnceLock::new(),
        }
    }
}

#[async_trait]
impl Gear for CoordinationPlugin {
    // @cpt-dod:cpt-cf-quota-enforcement-dod-coordination-default:p1
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        if self.service.get().is_some() {
            anyhow::bail!("{} gear already initialized", Self::MODULE_NAME);
        }
        let cfg: CoordinationPluginConfig = ctx.config_or_default()?;
        cfg.validate()?;

        let db = ctx.db_required()?;
        let service = Arc::new(DbCoordination::new(db.db()));

        let (instance_id, instance_json) =
            PluginV1::<QuotaEnforcementCoordinationPluginSpecV1>::build_registration(
                INSTANCE_SEGMENT,
                cfg.vendor.clone(),
                cfg.priority,
            )?;
        let registry = ctx.client_hub().get::<dyn TypesRegistryClient>()?;
        let results = registry.register(vec![instance_json]).await?;
        RegisterResult::ensure_all_ok(&results)?;

        self.service
            .set(service.clone())
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;

        let api: Arc<dyn CoordinationPluginV1> = service;
        ctx.client_hub()
            .register_scoped::<dyn CoordinationPluginV1>(ClientScope::gts_id(&instance_id), api);

        info!(
            instance_id = %instance_id,
            vendor = %cfg.vendor,
            priority = cfg.priority,
            "registered quota-enforcement coordination plugin instance"
        );
        Ok(())
    }
}

impl DatabaseCapability for CoordinationPlugin {
    fn migrations(&self) -> Vec<Box<dyn sea_orm_migration::MigrationTrait>> {
        use sea_orm_migration::MigratorTrait;
        crate::infra::storage::Migrator::migrations()
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "gear_tests.rs"]
mod gear_tests;
