use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use sea_orm_migration::MigrationTrait;
use toolkit::api::OpenApiRegistry;
use toolkit::{DatabaseCapability, Gear, GearCtx, RestApiCapability, SseBroadcaster};
use toolkit_db::DBProvider;
use toolkit_db::DbError;
use toolkit_http::HttpClient;
use tracing::{debug, info};
use url::Url;

// Import the client trait from SDK
#[allow(unused_imports)]
use users_info_sdk::UsersInfoClientV1;

// Import AuthZ resolver for authorization (PEP flow). The client is consumed
// via `#[toolkit::consumes]` (below) and resolved lazily by the PolicyEnforcer,
// so this gear works identically in-process and out-of-process.
use authz_resolver_sdk::PolicyEnforcer;

use crate::api::rest::dto::UserEvent;
use crate::api::rest::routes;
use crate::api::rest::sse_adapter::SseUserEventPublisher;
use crate::config::UsersInfoConfig;
use crate::domain::events::UserDomainEvent;
use crate::domain::local_client::client::UsersInfoLocalClient;
use crate::domain::ports::{AuditPort, EventPublisher, UsersMetricsPort};
use crate::domain::service::{AppServices, ServiceConfig};
use crate::infra::audit::HttpAuditClient;
use crate::infra::metrics::UsersMetricsMeter;
use crate::infra::storage::{OrmAddressesRepository, OrmCitiesRepository, OrmUsersRepository};

/// Type alias for the concrete `AppServices` type used with ORM repositories.
/// This lives in the composition root (gear.rs) to avoid infra dependencies in domain.
/// May be converted to `AppState` if we need additional fields like metrics, config and etc
pub(crate) type ConcreteAppServices =
    AppServices<OrmUsersRepository, OrmCitiesRepository, OrmAddressesRepository>;

/// Main gear struct with DDD-light layout and proper `ClientHub` integration
#[toolkit::gear(
    name = "users-info",
    capabilities = [db, rest]
)]
#[toolkit::consumes(contract = authz_resolver_sdk::AuthZResolverApi, from = "authz-resolver")]
pub struct UsersInfo {
    // Keep the domain service behind OnceLock for set-once access.
    // AppServices contains the db_handle and provides db() for per-request Db instances.
    service: OnceLock<Arc<ConcreteAppServices>>,
    // SSE broadcaster for user events
    sse: SseBroadcaster<UserEvent>,
}

impl Default for UsersInfo {
    fn default() -> Self {
        Self {
            service: OnceLock::new(),
            sse: SseBroadcaster::new(1024),
        }
    }
}

#[async_trait]
impl Gear for UsersInfo {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        // Load gear configuration using new API
        let cfg: UsersInfoConfig = ctx.config_or_default()?;
        debug!(
            "Loaded users_info config: default_page_size={}, max_page_size={}",
            cfg.default_page_size, cfg.max_page_size
        );

        // Acquire DB capability (secure wrapper, no DbHandle exposed to gears)
        let db: Arc<DBProvider<DbError>> = Arc::new(ctx.db_required()?);

        // Create event publisher adapter that bridges domain events to SSE
        let publisher: Arc<dyn EventPublisher<UserDomainEvent>> =
            Arc::new(SseUserEventPublisher::new(self.sse.clone()));

        // Build HTTP client with OTEL tracing enabled
        let http_client = HttpClient::builder()
            .with_otel()
            .build()
            .map_err(|e| anyhow::anyhow!("failed to build HTTP client: {e}"))?;

        // Parse audit service URLs from config
        let audit_base = Url::parse(&cfg.audit_base_url)
            .map_err(|e| anyhow::anyhow!("invalid audit_base_url: {e}"))?;
        let notify_base = Url::parse(&cfg.notifications_base_url)
            .map_err(|e| anyhow::anyhow!("invalid notifications_base_url: {e}"))?;

        // Create audit adapter
        let audit_adapter: Arc<dyn AuditPort> =
            Arc::new(HttpAuditClient::new(http_client, audit_base, notify_base));

        // AuthZ resolver is consumed via `#[toolkit::consumes]`; the resolving
        // client is wired by the runtime's proxy-wiring phase (after `init`), so
        // the enforcer resolves it lazily from the ClientHub at request time.
        // This is the single code path for both in-process and OoP deployments.
        let enforcer = PolicyEnforcer::from_hub(ctx.client_hub());

        let service_config = ServiceConfig {
            max_display_name_length: 100,
            default_page_size: cfg.default_page_size,
            max_page_size: cfg.max_page_size,
        };

        // Create repository implementations
        let limit_cfg = service_config.limit_cfg();
        let users_repo = OrmUsersRepository::new(limit_cfg);
        let cities_repo = OrmCitiesRepository::new(limit_cfg);
        let addresses_repo = OrmAddressesRepository::new(limit_cfg);

        // Create metrics adapter
        let scope =
            opentelemetry::InstrumentationScope::builder(Self::MODULE_NAME.to_owned()).build();
        let metrics: Arc<dyn UsersMetricsPort> = Arc::new(UsersMetricsMeter::new(
            &opentelemetry::global::meter_with_scope(scope),
        ));

        // Create services with repository dependencies
        let services = Arc::new(AppServices::new(
            users_repo,
            cities_repo,
            addresses_repo,
            db,
            publisher,
            audit_adapter,
            enforcer,
            service_config,
            metrics,
        ));

        self.service
            .set(services.clone())
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;

        // Create local client adapter that implements object-safe UsersInfoClientV1
        let local = UsersInfoLocalClient::new(services);

        // Register under the SDK trait for transport-agnostic consumption
        ctx.client_hub()
            .register::<dyn UsersInfoClientV1>(Arc::new(local));

        Ok(())
    }
}

impl DatabaseCapability for UsersInfo {
    fn migrations(&self) -> Vec<Box<dyn MigrationTrait>> {
        use sea_orm_migration::MigratorTrait;
        info!("Providing users_info database migrations");
        crate::infra::storage::migrations::Migrator::migrations()
    }
}

impl RestApiCapability for UsersInfo {
    fn register_rest(
        &self,
        _ctx: &GearCtx,
        router: axum::Router,
        openapi: &dyn OpenApiRegistry,
    ) -> anyhow::Result<axum::Router> {
        info!("Registering users_info REST routes");

        let service = self
            .service
            .get()
            .ok_or_else(|| anyhow::anyhow!("Service not initialized"))?
            .clone();

        let router = routes::register_routes(router, openapi, service);

        // Register SSE route with per-route Extension
        let router = routes::register_users_sse_route(router, openapi, self.sse.clone());

        info!("Users REST routes registered successfully");
        Ok(router)
    }
}
