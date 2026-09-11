//! Gear definition for `GearOrchestrator`

use anyhow::Result;
use async_trait::async_trait;
use std::sync::{Arc, OnceLock};

use toolkit::DirectoryClient;
use toolkit::context::GearCtx;
use toolkit::contracts::{
    GrpcServiceCapability, OpenApiRegistry, RegisterGrpcServiceFn, RestApiCapability,
    SystemCapability,
};
use toolkit::directory::LocalDirectoryClient;
use toolkit::registry::GearRegistry;
use toolkit::runtime::GearManager;

use cf_system_sdks::directory::DIRECTORY_SERVICE_NAME;

use crate::config::OrchestratorConfig;
use crate::domain::authz::RegistrationPolicy;
use crate::domain::service::GearsService;
use crate::server;

/// Gear Orchestrator - system gear for service discovery
///
/// This gear:
/// - Provides `DirectoryClient` to the `ClientHub` for in-process gears
/// - Exposes `DirectoryService` gRPC service via `grpc-hub`
/// - Tracks gear instances and provides service resolution
/// - Exposes REST API to list all registered gears
#[toolkit::gear(
    name = "gear-orchestrator",
    capabilities = [grpc, system, rest],
    client = cf_system_sdks::directory::DirectoryClient
)]
pub struct GearOrchestrator {
    directory_api: OnceLock<Arc<dyn DirectoryClient>>,
    gear_manager: OnceLock<Arc<GearManager>>,
    gears_service: OnceLock<Arc<GearsService>>,
    policy: OnceLock<RegistrationPolicy>,
}

impl Default for GearOrchestrator {
    fn default() -> Self {
        Self {
            directory_api: OnceLock::new(),
            gear_manager: OnceLock::new(),
            gears_service: OnceLock::new(),
            policy: OnceLock::new(),
        }
    }
}

#[async_trait]
impl SystemCapability for GearOrchestrator {
    fn pre_init(&self, sys: &toolkit::runtime::SystemContext) -> anyhow::Result<()> {
        self.gear_manager
            .set(Arc::clone(&sys.gear_manager))
            .map_err(|_| anyhow::anyhow!("GearManager already set (pre_init called twice?)"))?;
        Ok(())
    }
}

#[async_trait]
impl toolkit::Gear for GearOrchestrator {
    async fn init(&self, ctx: &GearCtx) -> Result<()> {
        // Read the (optional) authorization config: which peers may act on any
        // gear's registration, plus the namespace / trust-domain allowlists.
        // Absent config yields empty sets, i.e. every gear may act only on its
        // own name. Whether a *token-less* request is rejected is not configured
        // here: it is decided per-request from the `PlatformAuthEnforced` marker
        // the enforcement layer stamps (see `DirectoryServiceImpl`), so it stays
        // in lockstep with the listener without a drift-prone knob.
        let cfg: OrchestratorConfig = ctx.config_or_default()?;
        self.policy
            .set(RegistrationPolicy {
                trusted_registrars: cfg.trusted_registrars.into_iter().collect(),
                platform_namespaces: cfg.platform_namespaces.into_iter().collect(),
                trust_domains: cfg.trust_domains.into_iter().collect(),
            })
            .map_err(|_| anyhow::anyhow!("registration policy already set (init called twice?)"))?;

        // Build the compiled-gear catalog for the gears service.
        let registry = GearRegistry::discover_and_build()
            .map_err(|e| anyhow::anyhow!("Failed to build gear registry: {e}"))?;

        // Use the injected GearManager to create the DirectoryClient
        let manager = self
            .gear_manager
            .get()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("GearManager not wired into GearOrchestrator"))?;

        // Pin gRPC-service-name ownership from config so a name can only be
        // advertised by its configured owner, regardless of registration order.
        manager.set_grpc_service_owners(cfg.grpc_service_owners);

        let api_impl: Arc<dyn DirectoryClient> =
            Arc::new(LocalDirectoryClient::new(manager.clone()));

        // Register in ClientHub directly
        ctx.client_hub()
            .register::<dyn DirectoryClient>(api_impl.clone());

        self.directory_api
            .set(api_impl)
            .map_err(|_| anyhow::anyhow!("DirectoryClient already set (init called twice?)"))?;

        // Create the GearsService from the catalog built above.
        let gears_service = Arc::new(GearsService::new(&registry, manager));
        self.gears_service
            .set(gears_service)
            .map_err(|_| anyhow::anyhow!("GearsService already set (init called twice?)"))?;

        tracing::info!("GearOrchestrator initialized");

        Ok(())
    }
}

impl RestApiCapability for GearOrchestrator {
    fn register_rest(
        &self,
        _ctx: &GearCtx,
        router: axum::Router,
        openapi: &dyn OpenApiRegistry,
    ) -> Result<axum::Router> {
        let service = Arc::clone(
            self.gears_service
                .get()
                .ok_or_else(|| anyhow::anyhow!("GearsService not initialized"))?,
        );

        let router = crate::api::rest::routes::register_routes(router, openapi, service);

        tracing::info!("GearOrchestrator REST routes registered");
        Ok(router)
    }
}

/// Export gRPC services to `grpc-hub`
#[async_trait]
impl GrpcServiceCapability for GearOrchestrator {
    async fn get_grpc_services(&self, _ctx: &GearCtx) -> Result<Vec<RegisterGrpcServiceFn>> {
        let api = self
            .directory_api
            .get()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("DirectoryClient not initialized"))?;

        let policy = self
            .policy
            .get()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("registration policy not initialized"))?;

        let directory_svc = server::make_directory_service(api, policy);

        Ok(vec![RegisterGrpcServiceFn {
            service_name: DIRECTORY_SERVICE_NAME,
            register: Box::new(move |routes| {
                routes.add_service(directory_svc.clone());
            }),
        }])
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use tokio_util::sync::CancellationToken;
    use toolkit::client_hub::ClientHub;
    use toolkit::config::ConfigProvider;
    use uuid::Uuid;

    struct StaticConfigProvider(serde_json::Value);
    impl ConfigProvider for StaticConfigProvider {
        fn get_gear_config(&self, gear_name: &str) -> Option<&serde_json::Value> {
            (gear_name == "gear-orchestrator").then_some(&self.0)
        }
    }

    /// Run `init` against a config section, returning the gear so tests can
    /// inspect the policy it built.
    async fn init_with_config(config: serde_json::Value) -> Result<GearOrchestrator> {
        let mut wrapped = serde_json::Map::new();
        wrapped.insert("config".to_owned(), config);
        let ctx = GearCtx::new(
            "gear-orchestrator",
            Uuid::new_v4(),
            Arc::new(StaticConfigProvider(serde_json::Value::Object(wrapped))),
            Arc::new(ClientHub::default()),
            CancellationToken::new(),
        );
        let gear = GearOrchestrator::default();
        let gear_manager = Arc::new(GearManager::new());
        gear.gear_manager
            .set(gear_manager)
            .map_err(|_| anyhow::anyhow!("gear_manager already set"))?;
        toolkit::Gear::init(&gear, &ctx).await?;
        Ok(gear)
    }

    fn string_set(values: &[&str]) -> std::collections::HashSet<String> {
        values.iter().map(|s| (*s).to_owned()).collect()
    }

    #[tokio::test]
    async fn init_succeeds_with_empty_config() {
        init_with_config(serde_json::json!({}))
            .await
            .expect("init with an empty config section (all defaults) must succeed");
    }

    #[tokio::test]
    async fn init_reads_trusted_registrars_config() {
        let gear = init_with_config(serde_json::json!({
            "trusted_registrars": ["flight-control", "platform-host"]
        }))
        .await
        .expect("init with a trusted_registrars config section must succeed");

        let policy = gear.policy.get().expect("init must build the policy");
        assert_eq!(
            policy.trusted_registrars,
            string_set(&["flight-control", "platform-host"]),
            "both configured registrars must reach the policy"
        );
    }

    #[tokio::test]
    async fn init_reads_namespace_and_trust_domain_config() {
        let gear = init_with_config(serde_json::json!({
            "platform_namespaces": ["toolkit", "platform"],
            "trust_domains": ["example.org"]
        }))
        .await
        .expect("init with platform_namespaces / trust_domains config must succeed");

        let policy = gear.policy.get().expect("init must build the policy");
        assert_eq!(
            policy.platform_namespaces,
            string_set(&["toolkit", "platform"]),
            "both configured namespaces must reach the policy"
        );
        assert_eq!(
            policy.trust_domains,
            string_set(&["example.org"]),
            "the configured trust domain must reach the policy"
        );
    }

    #[tokio::test]
    async fn init_installs_configured_grpc_service_owners() {
        use toolkit::runtime::{Endpoint, GearInstance};

        let service = "cf.authz.v1.AuthzService";
        let gear = init_with_config(serde_json::json!({
            "grpc_service_owners": { service: "authz" }
        }))
        .await
        .expect("init with a grpc_service_owners config section must succeed");

        let manager = gear
            .gear_manager
            .get()
            .expect("init must wire the GearManager");

        // A squatter registering first is rejected in favor of the configured
        // owner; the configured owner is admitted.
        let conflict = manager
            .register_instance(Arc::new(
                GearInstance::new("evil", Uuid::new_v4())
                    .with_grpc_service(service, Endpoint::http("127.0.0.1", 9001)),
            ))
            .expect_err("a non-owner must not claim a configured service name");
        assert_eq!(conflict.owner, "authz");
        assert!(manager.instances_of("evil").is_empty());

        manager
            .register_instance(Arc::new(
                GearInstance::new("authz", Uuid::new_v4())
                    .with_grpc_service(service, Endpoint::http("127.0.0.1", 9000)),
            ))
            .expect("the configured owner must be admitted");
    }

    #[tokio::test]
    async fn init_rejects_unknown_config_key() {
        // A misspelled key must fail loudly at startup rather than silently
        // deserialize to an empty set and deny the intended registrar at runtime.
        let err = init_with_config(serde_json::json!({
            "trusted_registrar": ["flight-control"]
        }))
        .await
        .map(|_| ())
        .unwrap_err();
        assert!(
            err.to_string().contains("trusted_registrar"),
            "expected the error to name the offending key, got {err}"
        );
    }
}
