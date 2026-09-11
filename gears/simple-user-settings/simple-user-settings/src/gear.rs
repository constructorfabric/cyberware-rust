use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use axum::Router;
use toolkit::api::OpenApiRegistry;
use toolkit::{Gear, GearCtx};
use toolkit_db::DBProvider;
use toolkit_db::DbError;
use tracing::info;

use authz_resolver_sdk::{AuthZResolverApi, PolicyEnforcer};

use simple_user_settings_sdk::{SettingsOwnerResolver, SimpleUserSettingsClientV1};

use crate::api::rest::routes;
use crate::config::SettingsConfig;
use crate::domain::local_client::LocalClient;
use crate::domain::service::{Service, ServiceConfig};
use crate::infra::storage::sea_orm_repo::SeaOrmSettingsRepository;

/// Type alias for the concrete service type with ORM repository.
type ConcreteService = Service<SeaOrmSettingsRepository>;

/// Who a caller is, as the deployment understands it.
///
/// Looked up in the REST phase rather than in `init`: the resolver comes from
/// another gear and this gear does not depend on it, so init order guarantees
/// nothing — whereas every gear's `init` has run before any gear's REST phase.
/// A deployment that publishes none keeps the token subject as the key, which
/// is what this gear has always done.
fn owner_resolver(ctx: &GearCtx) -> Option<Arc<dyn SettingsOwnerResolver>> {
    let resolver = ctx.client_hub().get::<dyn SettingsOwnerResolver>().ok();
    if resolver.is_some() {
        info!("Settings gear: settings are keyed by the deployment's own user resolver");
    }
    resolver
}

#[toolkit::gear(
    name = "simple-user-settings",
    deps = [authz_resolver],
    capabilities = [rest, db]
)]
pub struct SettingsGear {
    service: OnceLock<Arc<ConcreteService>>,
}

impl Default for SettingsGear {
    fn default() -> Self {
        Self {
            service: OnceLock::new(),
        }
    }
}

impl toolkit::contracts::DatabaseCapability for SettingsGear {
    fn migrations(&self) -> Vec<Box<dyn sea_orm_migration::MigrationTrait>> {
        use sea_orm_migration::MigratorTrait;
        info!("Providing settings database migrations");
        crate::infra::storage::migrations::Migrator::migrations()
    }
}

#[async_trait]
impl Gear for SettingsGear {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let cfg: SettingsConfig = ctx.config_or_default()?;

        let db: Arc<DBProvider<DbError>> = Arc::new(ctx.db_required()?);

        // Repository no longer stores connection - uses &impl DBRunner per-method
        let repo = Arc::new(SeaOrmSettingsRepository::new());

        // Fetch AuthZ resolver from ClientHub
        let authz = ctx
            .client_hub()
            .get::<dyn AuthZResolverApi>()
            .map_err(|e| anyhow::anyhow!("failed to get AuthZ resolver: {e}"))?;
        let policy_enforcer = PolicyEnforcer::new(authz);

        let service_config = ServiceConfig {
            max_field_length: cfg.max_field_length,
        };
        let service = Arc::new(Service::new(db, repo, policy_enforcer, service_config));
        self.service
            .set(service.clone())
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;

        let local_client: Arc<dyn SimpleUserSettingsClientV1> = Arc::new(LocalClient::new(service));
        ctx.client_hub().register(local_client);

        Ok(())
    }
}

#[async_trait]
impl toolkit::contracts::RestApiCapability for SettingsGear {
    fn register_rest(
        &self,
        ctx: &GearCtx,
        router: Router,
        openapi: &dyn OpenApiRegistry,
    ) -> anyhow::Result<Router> {
        info!("Settings gear: register_rest called");
        let service = self
            .service
            .get()
            .ok_or_else(|| anyhow::anyhow!("Service not initialized"))?
            .clone();

        service.attach_owner_resolver(owner_resolver(ctx));

        let router = routes::register_routes(router, openapi, service);
        info!("Settings gear: REST routes registered successfully");
        Ok(router)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_settings_gear_default() {
        let gear = SettingsGear::default();
        assert!(gear.service.get().is_none());
    }

    #[test]
    fn test_settings_gear_multiple_defaults_empty_service() {
        let gear = SettingsGear::default();
        let other = SettingsGear::default();
        assert!(other.service.get().is_none());
        assert!(gear.service.get().is_none());
    }
}
