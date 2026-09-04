//! Plugin binding: select the storage plugin by vendor and resolve its scoped
//! `ClientHub` client.
//!
//! Selection goes through the types registry (`GtsPluginSelector` semantics:
//! same vendor, lowest priority wins). Nothing is cached here; bootstrap
//! resolves once and the service keeps the handle.
//!
//! Singleton coordination is not a plugin of this gear. The platform `cluster`
//! gear provides it, and `infra::cluster_coordination` resolves it (ADR-0006).

use std::sync::Arc;

use quota_enforcement_sdk::{QuotaEnforcementStoragePluginSpecV1, QuotaEnforcementStoragePluginV1};
use toolkit::client_hub::{ClientHub, ClientScope};
use toolkit::plugins::choose_plugin_instance;
use toolkit_macros::domain_model;
use types_registry_sdk::{InstanceQuery, TypesRegistryClient};

use super::error::{DomainError, PluginKind};

/// Vendor-driven plugin resolution.
#[domain_model]
pub struct PluginBinding {
    hub: Arc<ClientHub>,
    storage_vendor: String,
}

impl PluginBinding {
    /// Bind to the hub with the configured storage vendor.
    #[must_use]
    pub fn new(hub: Arc<ClientHub>, storage_vendor: String) -> Self {
        Self {
            hub,
            storage_vendor,
        }
    }

    /// Resolve the active storage plugin.
    ///
    /// # Errors
    ///
    /// - [`DomainError::TypesRegistryUnavailable`] when the registry cannot answer.
    /// - [`DomainError::PluginNotFound`] when no instance matches the vendor.
    /// - [`DomainError::InvalidPluginInstance`] when an instance is malformed.
    /// - [`DomainError::PluginClientNotRegistered`] when the instance exists
    ///   but its scoped client does not.
    pub async fn resolve_storage(
        &self,
    ) -> Result<Arc<dyn QuotaEnforcementStoragePluginV1>, DomainError> {
        let kind = PluginKind::Storage;
        let gts_id = self
            .select::<QuotaEnforcementStoragePluginSpecV1>(kind, &self.storage_vendor)
            .await?;
        self.hub
            .try_get_scoped::<dyn QuotaEnforcementStoragePluginV1>(&ClientScope::gts_id(&gts_id))
            .ok_or(DomainError::PluginClientNotRegistered { kind, gts_id })
    }

    async fn select<P>(&self, kind: PluginKind, vendor: &str) -> Result<String, DomainError>
    where
        P: for<'de> gts::GtsDeserialize<'de> + gts::GtsSchema,
    {
        let registry = self
            .hub
            .get::<dyn TypesRegistryClient>()
            .map_err(|e| DomainError::TypesRegistryUnavailable(e.to_string()))?;
        let type_id = <P as gts::GtsSchema>::TYPE_ID;
        let instances = registry
            .list_instances(InstanceQuery::new().with_pattern(format!("{type_id}*")))
            .await
            .map_err(|e| DomainError::TypesRegistryUnavailable(e.to_string()))?;
        // A registry answers the pattern query with instances of this spec. The
        // prefix filter keeps the selection correct against a registry that
        // ignores the pattern, so a foreign instance never fails deserialization.
        let candidates = instances
            .iter()
            .filter(|e| e.id.as_ref().starts_with(type_id))
            .map(|e| (e.id.as_ref(), &e.object));
        let gts_id = choose_plugin_instance::<P>(vendor, candidates)
            .map_err(|e| DomainError::plugin_selection(kind, e))?;
        tracing::info!(
            target: "qe.bootstrap",
            plugin = %kind,
            vendor,
            instance_id = %gts_id,
            "selected plugin instance"
        );
        Ok(gts_id)
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "plugins_tests.rs"]
mod plugins_tests;
