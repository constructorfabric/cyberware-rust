//! Gear declaration of the storage plugin.

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use toolkit::Gear;
use toolkit::context::GearCtx;
use toolkit::contracts::DatabaseCapability;
use tracing::info;

use crate::config::StoragePluginConfig;
use crate::domain::StoragePlugin;
use crate::infra::storage::SqlFoundationStore;

/// GTS instance segment this backend will register under once the full
/// `QuotaEnforcementStoragePluginV1` implementation is wired.
pub const INSTANCE_SEGMENT: &str = "cf.core._.qe_db_storage.v1";

/// Storage plugin gear.
///
/// `init` validates the configuration and binds the plugin skeleton to the
/// gear's database. The runtime applies the migrations before `init` through
/// [`DatabaseCapability`]. No scoped client is published yet: the foundation
/// Definition of Done forbids a partial trait implementation, so the gear
/// resolves no storage plugin until the last primitive lands.
// @cpt-dod:cpt-cf-quota-enforcement-dod-workspace-crates:p1
#[toolkit::gear(name = "quota-enforcement-storage-plugin", capabilities = [db])]
pub struct StoragePluginGear {
    plugin: OnceLock<Arc<StoragePlugin>>,
}

impl Default for StoragePluginGear {
    fn default() -> Self {
        Self {
            plugin: OnceLock::new(),
        }
    }
}

impl StoragePluginGear {
    /// The bound plugin skeleton, once `init` ran.
    #[must_use]
    pub fn plugin(&self) -> Option<Arc<StoragePlugin>> {
        self.plugin.get().cloned()
    }
}

#[async_trait]
impl Gear for StoragePluginGear {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        if self.plugin.get().is_some() {
            anyhow::bail!("{} gear already initialized", Self::MODULE_NAME);
        }
        let cfg: StoragePluginConfig = ctx.config_or_default()?;
        cfg.validate()?;

        let db = ctx.db_required()?;
        let plugin = Arc::new(StoragePlugin::new(Arc::new(SqlFoundationStore::new(
            db.db(),
        ))));
        self.plugin
            .set(plugin)
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;

        info!(
            vendor = %cfg.vendor,
            priority = cfg.priority,
            instance_segment = INSTANCE_SEGMENT,
            "storage plugin foundation initialised; client registration waits for the complete contract"
        );
        Ok(())
    }
}

impl DatabaseCapability for StoragePluginGear {
    fn migrations(&self) -> Vec<Box<dyn sea_orm_migration::MigrationTrait>> {
        use sea_orm_migration::MigratorTrait;
        crate::infra::storage::Migrator::migrations()
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "gear_tests.rs"]
mod gear_tests;
