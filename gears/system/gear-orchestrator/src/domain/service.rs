use std::collections::HashSet;
use std::sync::Arc;

use toolkit::registry::GearRegistry;
use toolkit::runtime::GearManager;
use toolkit_macros::domain_model;

use super::model::{DeploymentMode, GearInfo, InstanceInfo};

/// Lightweight compiled-gear metadata (owned data, no trait objects).
#[domain_model]
struct CompiledGear {
    name: String,
    capabilities: Vec<String>,
    deps: Vec<String>,
}

/// Service that assembles gear information from catalog and runtime data.
#[domain_model]
pub struct GearsService {
    /// Compiled gears snapshot (built once at init, immutable after).
    compiled: Vec<CompiledGear>,
    /// Runtime gear manager for live instance queries.
    gear_manager: Arc<GearManager>,
}

impl GearsService {
    /// Build from a live `GearRegistry` and a `GearManager`.
    ///
    /// Extracts gear metadata (names, deps, capability labels) from the registry
    /// and drops the registry afterwards — no trait objects are kept.
    #[must_use]
    pub fn new(registry: &GearRegistry, gear_manager: Arc<GearManager>) -> Self {
        let compiled: Vec<CompiledGear> = registry
            .gears()
            .iter()
            .map(|entry| CompiledGear {
                name: entry.name().to_owned(),
                capabilities: entry
                    .caps()
                    .labels()
                    .iter()
                    .map(|s| (*s).to_owned())
                    .collect(),
                deps: entry.deps().iter().map(|d| (*d).to_owned()).collect(),
            })
            .collect();

        Self {
            compiled,
            gear_manager,
        }
    }

    /// List all registered gears, merging compile-time catalog data with runtime instances.
    #[must_use]
    pub fn list_gears(&self) -> Vec<GearInfo> {
        let mut gears = Vec::new();
        let mut seen_names = HashSet::new();

        // 1. Emit all compiled-in gears from the catalog.
        for cm in &self.compiled {
            seen_names.insert(cm.name.clone());

            let instances = self.get_gear_instances(&cm.name);

            gears.push(GearInfo {
                name: cm.name.clone(),
                capabilities: cm.capabilities.clone(),
                dependencies: cm.deps.clone(),
                deployment_mode: DeploymentMode::CompiledIn,
                instances,
            });
        }

        // 2. Add any dynamically registered gears from GearManager
        //    that are not in the compiled catalog (external / out-of-process).
        for instance in self.gear_manager.all_instances() {
            if seen_names.contains(&instance.gear) {
                continue;
            }
            seen_names.insert(instance.gear.clone());

            let instances = self.get_gear_instances(&instance.gear);

            gears.push(GearInfo {
                name: instance.gear.clone(),
                capabilities: vec![],
                dependencies: vec![],
                deployment_mode: DeploymentMode::OutOfProcess,
                instances,
            });
        }

        // Sort by name for deterministic output
        gears.sort_by(|a, b| a.name.cmp(&b.name));

        gears
    }

    fn get_gear_instances(&self, gear_name: &str) -> Vec<InstanceInfo> {
        self.gear_manager
            .instances_of(gear_name)
            .into_iter()
            .map(|inst| {
                let grpc_services = inst
                    .grpc_services
                    .iter()
                    .map(|(name, ep)| (name.clone(), ep.uri.clone()))
                    .collect();

                InstanceInfo {
                    instance_id: inst.instance_id,
                    version: inst.version.clone(),
                    state: inst.state(),
                    grpc_services,
                }
            })
            .collect()
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use toolkit::registry::RegistryBuilder;
    use toolkit::runtime::{Endpoint, GearInstance, GearManager, InstanceState};
    use uuid::Uuid;

    // ---- Test helpers ----

    // (name, deps, has_rest, has_system)
    type GearSpec = (&'static str, &'static [&'static str], bool, bool);

    #[domain_model]
    #[derive(Default)]
    struct DummyCore;
    #[async_trait::async_trait]
    impl toolkit::Gear for DummyCore {
        async fn init(&self, _ctx: &toolkit::context::GearCtx) -> anyhow::Result<()> {
            Ok(())
        }
    }

    #[domain_model]
    #[derive(Default, Clone)]
    struct DummyRest;
    impl toolkit::contracts::RestApiCapability for DummyRest {
        fn register_rest(
            &self,
            _ctx: &toolkit::context::GearCtx,
            _router: axum::Router,
            _openapi: &dyn toolkit::api::OpenApiRegistry,
        ) -> anyhow::Result<axum::Router> {
            Ok(axum::Router::new())
        }
    }

    #[domain_model]
    #[derive(Default)]
    struct DummySystem;
    #[async_trait::async_trait]
    impl toolkit::contracts::SystemCapability for DummySystem {}

    fn build_registry(gears: &[GearSpec]) -> GearRegistry {
        let mut b = RegistryBuilder::default();
        for &(name, deps, has_rest, has_system) in gears {
            b.register_core_with_meta(name, deps, Arc::new(DummyCore));
            if has_rest {
                b.register_rest_with_meta(name, Arc::new(DummyRest));
            }
            if has_system {
                b.register_system_with_meta(name, Arc::new(DummySystem));
            }
        }
        b.build_topo_sorted().unwrap()
    }

    // ---- Tests ----

    #[test]
    fn list_compiled_in_gears_from_registry() {
        let registry = build_registry(&[
            ("api_gateway", &[], true, true),
            ("nodes_registry", &["api_gateway"], true, false),
        ]);
        let manager = Arc::new(GearManager::new());
        let svc = GearsService::new(&registry, manager);
        let gears = svc.list_gears();

        assert_eq!(gears.len(), 2);
        // Sorted by name
        assert_eq!(gears[0].name, "api_gateway");
        assert_eq!(gears[0].deployment_mode, DeploymentMode::CompiledIn);
        assert!(gears[0].capabilities.contains(&"rest".to_owned()));
        assert!(gears[0].capabilities.contains(&"system".to_owned()));
        assert!(gears[0].instances.is_empty());

        assert_eq!(gears[1].name, "nodes_registry");
        assert_eq!(gears[1].dependencies, vec!["api_gateway"]);
    }

    #[test]
    fn dynamic_external_instances_appear_as_out_of_process() {
        let registry = build_registry(&[]);
        let manager = Arc::new(GearManager::new());

        let instance = Arc::new(
            GearInstance::new("external_svc", Uuid::new_v4())
                .with_version("2.0.0")
                .with_grpc_service("ext.Service", Endpoint::http("127.0.0.1", 9001)),
        );
        manager.register_instance(instance).unwrap();

        let svc = GearsService::new(&registry, manager);
        let gears = svc.list_gears();

        assert_eq!(gears.len(), 1);
        assert_eq!(gears[0].name, "external_svc");
        assert_eq!(gears[0].deployment_mode, DeploymentMode::OutOfProcess);
        assert_eq!(gears[0].instances.len(), 1);
        assert_eq!(gears[0].instances[0].version, Some("2.0.0".to_owned()));
        assert!(
            gears[0].instances[0]
                .grpc_services
                .contains_key("ext.Service")
        );
    }

    #[test]
    fn compiled_in_gears_show_instances_from_manager() {
        let registry = build_registry(&[("grpc_hub", &[], false, true)]);
        let manager = Arc::new(GearManager::new());

        let instance =
            Arc::new(GearInstance::new("grpc_hub", Uuid::new_v4()).with_version("0.1.0"));
        manager.register_instance(instance).unwrap();

        let svc = GearsService::new(&registry, manager);
        let gears = svc.list_gears();

        assert_eq!(gears.len(), 1);
        assert_eq!(gears[0].name, "grpc_hub");
        assert_eq!(gears[0].deployment_mode, DeploymentMode::CompiledIn);
        assert_eq!(gears[0].instances.len(), 1);
    }

    #[test]
    fn instance_state_maps_correctly() {
        let registry = build_registry(&[]);
        let manager = Arc::new(GearManager::new());

        let instance = Arc::new(GearInstance::new("svc", Uuid::new_v4()));
        // Default state is Registered
        manager.register_instance(instance).unwrap();

        let svc = GearsService::new(&registry, manager);
        let gears = svc.list_gears();

        assert_eq!(gears[0].instances[0].state, InstanceState::Registered);
    }

    #[test]
    fn result_is_sorted_by_name() {
        let registry =
            build_registry(&[("zebra", &[], false, false), ("alpha", &[], false, false)]);
        let manager = Arc::new(GearManager::new());

        let svc = GearsService::new(&registry, manager);
        let gears = svc.list_gears();

        assert_eq!(gears[0].name, "alpha");
        assert_eq!(gears[1].name, "zebra");
    }
}
