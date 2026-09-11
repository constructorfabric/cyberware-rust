//! Directory API - contract for service discovery and instance resolution

use anyhow::Result;
use async_trait::async_trait;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use uuid::Uuid;

use crate::runtime::{Endpoint, GearInstance, GearManager, GrpcServiceNameConflict};

/// Compute a content token for an `OpenAPI` document, used to detect changes.
///
/// This is a change-detection token (like a k8s `resourceVersion`), **not** a
/// security digest: the edge only ever compares it against the token from the
/// previous poll of the *same* directory to decide whether the document
/// changed. A non-cryptographic `std` hash is therefore sufficient and avoids a
/// dependency.
///
/// Determinism is scoped to a single binary: `DefaultHasher::new()` uses fixed
/// keys, so identical input yields the same token for the lifetime of one
/// directory process. `std` does **not** guarantee the algorithm across Rust
/// toolchain versions, so a rebuilt/upgraded directory may emit a different
/// token for the same spec — which is benign here, because tokens are only ever
/// compared within one directory's poll stream (at worst an upgrade triggers a
/// single spurious refresh). The token must therefore not be persisted or
/// compared across binaries.
fn openapi_spec_hash(spec: &str) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    spec.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

// Re-export all types from contracts - this is the single source of truth
pub use cf_system_sdks::directory::{
    DirectoryClient, DirectoryInvalidArgument, DirectoryNotFound, DirectoryServiceNameConflict,
    GrpcServiceInfo, InstanceState, LabelSelector, RegisterInstanceInfo, ServiceEndpoint,
    ServiceInstanceInfo,
};

/// Project the live runtime [`crate::runtime::InstanceState`] onto the domain
/// [`InstanceState`] carried on `ServiceInstanceInfo`, so `resolve_by_labels`
/// callers can apply their own health policy from a single resolve.
fn runtime_state_to_domain(state: crate::runtime::InstanceState) -> InstanceState {
    use crate::runtime::InstanceState as Rt;
    match state {
        Rt::Registered => InstanceState::Registered,
        Rt::Ready => InstanceState::Ready,
        Rt::Healthy => InstanceState::Healthy,
        Rt::Quarantined => InstanceState::Quarantined,
        Rt::Draining => InstanceState::Draining,
    }
}

/// Project a live [`GearInstance`] into a [`ServiceInstanceInfo`].
///
/// `endpoint` prefers gRPC and falls back to REST; an instance advertising
/// neither yields `endpoint: None` (never an empty-URI sentinel), which the
/// label paths surface as *no-endpoint, not an error*. Always **spec-free**:
/// only the `OpenAPI` spec *hash* rides along, never the document (fetched via
/// `get_openapi_spec`).
fn project_instance(gear: &str, inst: &GearInstance) -> ServiceInstanceInfo {
    let endpoint = inst
        .grpc_services
        .values()
        .next()
        .or(inst.rest_endpoint.as_ref())
        .map(|ep| ServiceEndpoint::new(ep.uri.clone()));

    ServiceInstanceInfo::new(gear, inst.instance_id.to_string())
        .with_endpoint(endpoint)
        .with_version(inst.version.clone())
        .with_rest_endpoint(
            inst.rest_endpoint
                .as_ref()
                .map(|ep| ServiceEndpoint::new(ep.uri.clone())),
        )
        .with_openapi_spec_hash(inst.openapi_spec.as_deref().map(openapi_spec_hash))
        // Carry every published gRPC service back so the directory-register
        // phase can augment (not clobber) this instance when it adds a REST
        // endpoint.
        .with_grpc_services(
            inst.grpc_services
                .iter()
                .map(|(name, e)| (name.clone(), ServiceEndpoint::new(e.uri.clone())))
                .collect(),
        )
        .with_labels(inst.labels.clone())
        .with_state(runtime_state_to_domain(inst.state()))
}

/// Local implementation of `DirectoryClient` that delegates to `GearManager`
///
/// This is the in-process implementation used by gears running in the same
/// process as the gear orchestrator.
pub struct LocalDirectoryClient {
    mgr: Arc<GearManager>,
}

impl LocalDirectoryClient {
    #[must_use]
    pub fn new(mgr: Arc<GearManager>) -> Self {
        Self { mgr }
    }
}

#[async_trait]
impl DirectoryClient for LocalDirectoryClient {
    // Every lookup below is an in-memory `GearManager` map read, so `None` can
    // only ever mean "nothing registered under that name" — this client has no
    // backend and therefore no failure mode. Returning the typed
    // `DirectoryNotFound` sentinel rather than a bare `anyhow` is what lets
    // `DirectoryEndpointResolver` report `Ok(None)` ("provider not up yet")
    // instead of `Err` ("the directory is broken"); the difference decides
    // whether a routine startup race is logged at `debug` or `warn`.
    async fn resolve_grpc_service(&self, service_name: &str) -> Result<ServiceEndpoint> {
        if let Some((_gear, _inst, ep)) = self.mgr.pick_service_round_robin(service_name) {
            return Ok(ServiceEndpoint::new(ep.uri));
        }

        Err(DirectoryNotFound::new(format!("service {service_name}")).into())
    }

    async fn resolve_rest_service(&self, gear_name: &str) -> Result<ServiceEndpoint> {
        if let Some(ep) = self.mgr.pick_rest_endpoint_round_robin(gear_name) {
            return Ok(ServiceEndpoint::new(ep.uri));
        }

        Err(DirectoryNotFound::new(format!("gear {gear_name}")).into())
    }

    async fn get_openapi_spec(&self, gear_name: &str) -> Result<String> {
        self.mgr.openapi_spec_of(gear_name).ok_or_else(|| {
            DirectoryNotFound::new(format!("openapi spec for gear {gear_name}")).into()
        })
    }

    async fn list_instances(&self, gear: &str) -> Result<Vec<ServiceInstanceInfo>> {
        // Base label-targeting enumeration: project every instance, including
        // endpoint-less ones (no-endpoint contract; see `project_instance`).
        Ok(self
            .mgr
            .instances_of(gear)
            .iter()
            .map(|inst| project_instance(gear, inst))
            .collect())
    }

    async fn resolve_by_labels(
        &self,
        gear: &str,
        selector: &LabelSelector,
    ) -> Result<Vec<ServiceInstanceInfo>> {
        // Filter by selector only: no health/endpoint filtering, so a matched
        // endpoint-less instance is still returned for the caller to fall back on.
        Ok(self
            .mgr
            .instances_of(gear)
            .iter()
            .map(|inst| project_instance(gear, inst))
            .filter(|i| selector.matches(&i.labels))
            .collect())
    }

    async fn list_all_instances(&self) -> Result<Vec<ServiceInstanceInfo>> {
        // Edge snapshot: drop labels (shared `without_labels`) and, unlike the
        // label paths, skip endpoint-less instances — the edge needs a dialable
        // URI — with a `debug!` so the omission is observable.
        Ok(self
            .mgr
            .all_instances()
            .iter()
            .filter_map(|inst| {
                let info = project_instance(&inst.gear, inst);
                if info.endpoint.is_none() {
                    tracing::debug!(
                        gear = %inst.gear,
                        instance_id = %inst.instance_id,
                        "skipping instance with no gRPC or REST endpoint from cross-gear snapshot"
                    );
                    return None;
                }
                Some(info.without_labels())
            })
            .collect())
    }

    async fn register_instance(&self, info: RegisterInstanceInfo) -> Result<()> {
        // Parse instance_id from string to Uuid
        let instance_id = Uuid::parse_str(&info.instance_id).map_err(|e| {
            // A malformed instance_id is a client error, not a server fault:
            // return the typed `DirectoryInvalidArgument` so a gRPC front-end
            // maps it to `InvalidArgument` (and the caller stops retrying)
            // rather than a bare `anyhow!` reported as `Internal`.
            anyhow::Error::from(DirectoryInvalidArgument::new(format!(
                "invalid instance_id '{}': {e}",
                info.instance_id
            )))
        })?;

        // Validate labels at the store boundary, not just at the gRPC
        // front-end: this is the single point every registration path (remote
        // gRPC, in-process, or a future transport) funnels through, so
        // enforcing the shared rules here means an invalid label can never be
        // stored regardless of how it arrived. The typed error maps to
        // `InvalidArgument` at the wire boundary.
        cf_system_sdks::directory::validate_labels(&info.labels)
            .map_err(|e| anyhow::Error::from(DirectoryInvalidArgument::new(e.to_string())))?;

        // Build a GearInstance from RegisterInstanceInfo
        let mut instance = GearInstance::new(info.gear.clone(), instance_id);

        // Apply version if provided
        if let Some(version) = info.version {
            instance = instance.with_version(version);
        }

        // Add all gRPC services
        for (service_name, endpoint) in info.grpc_services {
            instance = instance.with_grpc_service(service_name, Endpoint::from_uri(endpoint.uri));
        }

        // Apply REST endpoint if provided
        if let Some(rest) = info.rest_endpoint {
            instance = instance.with_rest_endpoint(Endpoint::from_uri(rest.uri));
        }

        // Apply OpenAPI spec if provided
        if let Some(spec) = info.openapi_spec {
            instance = instance.with_openapi_spec(spec);
        }

        // Apply stable addressing labels if provided
        if !info.labels.is_empty() {
            instance = instance.with_labels(info.labels);
        }

        // Register the instance, enforcing single-gear ownership of every gRPC
        // service name it advertises atomically (check + insert under one lock)
        // so two gears cannot race to claim the same name. A conflict is a typed
        // sentinel the gRPC boundary maps to `permission_denied`.
        self.mgr.register_instance(Arc::new(instance)).map_err(
            |GrpcServiceNameConflict {
                 service_name,
                 owner,
             }| {
                anyhow::Error::from(DirectoryServiceNameConflict::new(service_name, owner))
            },
        )?;

        Ok(())
    }

    async fn deregister_instance(&self, gear: &str, instance_id: &str) -> Result<()> {
        let instance_id = Uuid::parse_str(instance_id)
            .map_err(|e| anyhow::anyhow!("Invalid instance_id '{instance_id}': {e}"))?;
        self.mgr.deregister(gear, instance_id);
        Ok(())
    }

    async fn send_heartbeat(&self, gear: &str, instance_id: &str) -> Result<()> {
        let instance_id = Uuid::parse_str(instance_id)
            .map_err(|e| anyhow::anyhow!("Invalid instance_id '{instance_id}': {e}"))?;
        self.mgr
            .update_heartbeat(gear, instance_id, std::time::Instant::now());
        Ok(())
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_resolve_grpc_service_not_found() {
        let dir = Arc::new(GearManager::new());
        let api = LocalDirectoryClient::new(dir);

        let err = api
            .resolve_grpc_service("nonexistent.Service")
            .await
            .unwrap_err();
        // Asserting `is_err()` alone would pass for a bare `anyhow` too, which
        // is what this client used to return — and which
        // `DirectoryEndpointResolver` reads as "the directory is broken".
        assert!(
            err.downcast_ref::<DirectoryNotFound>().is_some(),
            "expected the typed not-found sentinel, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn test_register_instance_via_api() {
        let dir = Arc::new(GearManager::new());
        let api = LocalDirectoryClient::new(dir.clone());

        let instance_id = Uuid::new_v4();
        // Register an instance through the API
        let register_info = RegisterInstanceInfo::new("test_gear", instance_id.to_string())
            .with_grpc_services(vec![(
                "test.Service".to_owned(),
                ServiceEndpoint::http("127.0.0.1", 8001),
            )])
            .with_version("1.0.0");

        api.register_instance(register_info).await.unwrap();

        // Verify the instance was registered
        let instances = dir.instances_of("test_gear");
        assert_eq!(instances.len(), 1);
        assert_eq!(instances[0].instance_id, instance_id);
        assert_eq!(instances[0].version, Some("1.0.0".to_owned()));
        assert!(instances[0].grpc_services.contains_key("test.Service"));
    }

    #[tokio::test]
    async fn register_rejects_invalid_labels_at_store_boundary() {
        // Validation is enforced at the store, not only at the gRPC front-end:
        // an invalid label registered in-process must be rejected here (typed
        // as DirectoryInvalidArgument) and never reach the manager.
        let dir = Arc::new(GearManager::new());
        let api = LocalDirectoryClient::new(Arc::clone(&dir));

        let err = api
            .register_instance(
                RegisterInstanceInfo::new("worker", Uuid::new_v4().to_string())
                    .with_rest_endpoint(ServiceEndpoint::new("http://worker:8080"))
                    .with_labels(labels(&[("bad key", "7")])),
            )
            .await
            .expect_err("an invalid label must be rejected at the store boundary");
        assert!(
            err.downcast_ref::<DirectoryInvalidArgument>().is_some(),
            "store-boundary label rejection must be typed InvalidArgument, got: {err}"
        );
        assert!(
            dir.instances_of("worker").is_empty(),
            "a rejected registration must not be stored"
        );
    }

    #[tokio::test]
    async fn test_register_and_resolve_rest_and_openapi() {
        let dir = Arc::new(GearManager::new());
        let api = LocalDirectoryClient::new(dir.clone());

        let instance_id = Uuid::new_v4();
        let register_info = RegisterInstanceInfo::new("billing", instance_id.to_string())
            .with_version("1.0.0")
            .with_rest_endpoint(ServiceEndpoint::http("billing", 8080))
            .with_openapi_spec("{\"openapi\":\"3.1.0\"}");

        api.register_instance(register_info).await.unwrap();

        // REST endpoint resolves to the registered base URL.
        let resolved = api.resolve_rest_service("billing").await.unwrap();
        assert_eq!(resolved.uri, concat!("http", "://billing:8080"));

        // OpenAPI spec can be retrieved.
        let spec = api.get_openapi_spec("billing").await.unwrap();
        assert!(spec.contains("openapi"));
    }

    #[tokio::test]
    async fn test_resolve_rest_and_openapi_not_found() {
        let dir = Arc::new(GearManager::new());
        let api = LocalDirectoryClient::new(dir);

        // The typed sentinel is what distinguishes "provider not up yet" from
        // a directory failure — see `DirectoryEndpointResolver`.
        let rest_err = api.resolve_rest_service("missing").await.unwrap_err();
        assert!(
            rest_err.downcast_ref::<DirectoryNotFound>().is_some(),
            "expected the typed not-found sentinel, got: {rest_err:?}"
        );

        let spec_err = api.get_openapi_spec("missing").await.unwrap_err();
        assert!(
            spec_err.downcast_ref::<DirectoryNotFound>().is_some(),
            "expected the typed not-found sentinel, got: {spec_err:?}"
        );
    }

    #[tokio::test]
    async fn test_deregister_instance_via_api() {
        let dir = Arc::new(GearManager::new());
        let api = LocalDirectoryClient::new(dir.clone());

        let instance_id = Uuid::new_v4();
        // Register an instance first
        let inst = Arc::new(GearInstance::new("test_gear", instance_id));
        dir.register_instance(inst).unwrap();

        // Verify it exists
        assert_eq!(dir.instances_of("test_gear").len(), 1);

        // Deregister via API
        api.deregister_instance("test_gear", &instance_id.to_string())
            .await
            .unwrap();

        // Verify it's gone
        assert_eq!(dir.instances_of("test_gear").len(), 0);
    }

    #[tokio::test]
    async fn test_send_heartbeat_via_api() {
        use crate::runtime::InstanceState;

        let dir = Arc::new(GearManager::new());
        let api = LocalDirectoryClient::new(dir.clone());

        let instance_id = Uuid::new_v4();
        // Register an instance first
        let inst = Arc::new(GearInstance::new("test_gear", instance_id));
        dir.register_instance(inst).unwrap();

        // Verify initial state is Registered
        let instances = dir.instances_of("test_gear");
        assert_eq!(instances[0].state(), InstanceState::Registered);

        // Send heartbeat via API
        api.send_heartbeat("test_gear", &instance_id.to_string())
            .await
            .unwrap();

        // Verify state transitioned to Healthy
        let instances = dir.instances_of("test_gear");
        assert_eq!(instances[0].state(), InstanceState::Healthy);
    }

    #[tokio::test]
    async fn test_list_all_instances_across_gears() {
        let dir = Arc::new(GearManager::new());
        let api = LocalDirectoryClient::new(Arc::clone(&dir));

        // Two REST-only OoP gears (no gRPC services) + one gRPC-only gear.
        for (gear, port) in [("billing", 8080u16), ("catalog", 8081u16)] {
            api.register_instance(
                RegisterInstanceInfo::new(gear, Uuid::new_v4().to_string())
                    .with_version("1.0.0")
                    .with_rest_endpoint(ServiceEndpoint::http(gear, port))
                    .with_openapi_spec(format!("{{\"openapi\":\"3.1.0\",\"x\":\"{gear}\"}}")),
            )
            .await
            .unwrap();
        }

        // gRPC-only gear: gRPC service metadata and no REST endpoint / spec. This
        // exercises gRPC endpoint selection in `list_all_instances` (which prefers
        // a gRPC endpoint for the primary `endpoint`).
        api.register_instance(
            RegisterInstanceInfo::new("reporting", Uuid::new_v4().to_string())
                .with_grpc_services(vec![(
                    "reporting.Service".to_owned(),
                    ServiceEndpoint::new("http://reporting:7000"),
                )])
                .with_version("1.0.0"),
        )
        .await
        .unwrap();

        let all = api.list_all_instances().await.unwrap();
        assert_eq!(all.len(), 3);

        let billing = all.iter().find(|i| i.gear == "billing").expect("billing");
        assert_eq!(
            billing.rest_endpoint.as_ref().map(|e| e.uri.as_str()),
            Some("http://billing:8080")
        );
        // The cross-gear snapshot never inlines the OpenAPI document; it carries
        // only the hash, and consumers fetch the document per gear via
        // `get_openapi_spec`.
        assert!(billing.openapi_spec_hash.is_some());
        assert!(
            api.get_openapi_spec("billing")
                .await
                .expect("billing spec")
                .contains("billing")
        );

        // The gRPC-only gear resolves its primary endpoint from gRPC metadata and
        // carries no REST endpoint or OpenAPI spec.
        let reporting = all
            .iter()
            .find(|i| i.gear == "reporting")
            .expect("reporting");
        assert_eq!(
            reporting.endpoint.as_ref().map(|e| e.uri.as_str()),
            Some("http://reporting:7000")
        );
        assert!(reporting.rest_endpoint.is_none());
        assert!(reporting.openapi_spec_hash.is_none());
    }

    fn labels(pairs: &[(&str, &str)]) -> std::collections::BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[tokio::test]
    async fn register_carries_labels_into_list_instances() {
        let dir = Arc::new(GearManager::new());
        let api = LocalDirectoryClient::new(Arc::clone(&dir));

        api.register_instance(
            RegisterInstanceInfo::new("worker", Uuid::new_v4().to_string())
                .with_grpc_services(vec![(
                    "worker.Svc".to_owned(),
                    ServiceEndpoint::new("http://worker:7000"),
                )])
                .with_labels(labels(&[("shard", "7")])),
        )
        .await
        .unwrap();

        let listed = api.list_instances("worker").await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].labels.get("shard"), Some(&"7".to_owned()));
    }

    #[tokio::test]
    async fn resolve_by_labels_selects_matching_instances() {
        let dir = Arc::new(GearManager::new());
        let api = LocalDirectoryClient::new(Arc::clone(&dir));

        for (id, shard) in [("a", "7"), ("b", "8"), ("c", "7")] {
            api.register_instance(
                RegisterInstanceInfo::new("worker", Uuid::new_v4().to_string())
                    .with_grpc_services(vec![(
                        format!("worker.{id}"),
                        ServiceEndpoint::new(format!("http://worker-{id}:7000")),
                    )])
                    .with_labels(labels(&[("shard", shard)])),
            )
            .await
            .unwrap();
        }

        let matched = api
            .resolve_by_labels("worker", &LabelSelector::new().with("shard", "7"))
            .await
            .unwrap();
        assert_eq!(matched.len(), 2, "two instances carry shard=7");
        assert!(
            matched
                .iter()
                .all(|i| i.labels.get("shard") == Some(&"7".to_owned()))
        );
    }

    #[tokio::test]
    async fn labelless_reregister_preserves_labels_through_api() {
        let dir = Arc::new(GearManager::new());
        let api = LocalDirectoryClient::new(Arc::clone(&dir));

        let instance_id = Uuid::new_v4().to_string();
        api.register_instance(
            RegisterInstanceInfo::new("worker", instance_id.clone())
                .with_rest_endpoint(ServiceEndpoint::new("http://worker:8080"))
                .with_labels(labels(&[("shard", "7")])),
        )
        .await
        .unwrap();

        // A label-less re-registration (e.g. self-heal) must not wipe the shard.
        api.register_instance(
            RegisterInstanceInfo::new("worker", instance_id)
                .with_rest_endpoint(ServiceEndpoint::new("http://worker:8080"))
                .with_version("2.0.0"),
        )
        .await
        .unwrap();

        let listed = api.list_instances("worker").await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(
            listed[0].labels.get("shard"),
            Some(&"7".to_owned()),
            "label-less re-registration must not wipe stored labels"
        );

        // And it is still selectable by the shard label.
        let matched = api
            .resolve_by_labels("worker", &LabelSelector::new().with("shard", "7"))
            .await
            .unwrap();
        assert_eq!(matched.len(), 1);
    }

    #[tokio::test]
    async fn resolve_by_labels_carries_live_serving_state() {
        let dir = Arc::new(GearManager::new());
        let api = LocalDirectoryClient::new(Arc::clone(&dir));

        let instance_id = Uuid::new_v4();
        api.register_instance(
            RegisterInstanceInfo::new("worker", instance_id.to_string())
                .with_grpc_services(vec![(
                    "worker.Svc".to_owned(),
                    ServiceEndpoint::new("http://worker:7000"),
                )])
                .with_labels(labels(&[("shard", "7")])),
        )
        .await
        .unwrap();

        // A freshly-registered instance is not yet serving.
        let matched = api
            .resolve_by_labels("worker", &LabelSelector::new().with("shard", "7"))
            .await
            .unwrap();
        assert_eq!(matched.len(), 1);
        assert_eq!(matched[0].state, InstanceState::Registered);
        assert!(!matched[0].state.is_serving());

        // A heartbeat transitions it to Healthy; the state must be projected
        // onto the resolve result so a caller can filter on it.
        dir.update_heartbeat("worker", instance_id, std::time::Instant::now());
        let matched = api
            .resolve_by_labels("worker", &LabelSelector::new().with("shard", "7"))
            .await
            .unwrap();
        assert_eq!(matched[0].state, InstanceState::Healthy);
        assert!(matched[0].state.is_serving());
    }

    #[tokio::test]
    async fn resolve_by_labels_omits_openapi_spec() {
        let dir = Arc::new(GearManager::new());
        let api = LocalDirectoryClient::new(Arc::clone(&dir));

        api.register_instance(
            RegisterInstanceInfo::new("worker", Uuid::new_v4().to_string())
                .with_rest_endpoint(ServiceEndpoint::new("http://worker:8080"))
                .with_openapi_spec("{\"openapi\":\"3.1.0\"}")
                .with_labels(labels(&[("shard", "7")])),
        )
        .await
        .unwrap();

        // The in-process label path is spec-free, matching the gRPC client.
        let matched = api
            .resolve_by_labels("worker", &LabelSelector::new().with("shard", "7"))
            .await
            .unwrap();
        assert_eq!(matched.len(), 1);
        assert!(
            matched[0].openapi_spec_hash.is_some(),
            "in-process resolve_by_labels carries only the spec hash, never the document"
        );

        // The plain enumeration path is spec-free too: only the hash rides
        // along, the document is fetched via `get_openapi_spec`.
        let listed = api.list_instances("worker").await.unwrap();
        assert!(
            listed[0].openapi_spec_hash.is_some(),
            "list_instances must carry the spec hash, never the document"
        );
    }

    #[tokio::test]
    async fn list_all_instances_skips_endpoint_less() {
        let dir = Arc::new(GearManager::new());
        let api = LocalDirectoryClient::new(Arc::clone(&dir));

        // One instance advertises a REST endpoint; the other advertises neither
        // gRPC nor REST and must be skipped (no empty-URI sentinel).
        api.register_instance(
            RegisterInstanceInfo::new("worker", Uuid::new_v4().to_string())
                .with_rest_endpoint(ServiceEndpoint::new("http://worker:8080")),
        )
        .await
        .unwrap();
        api.register_instance(RegisterInstanceInfo::new(
            "placeholder",
            Uuid::new_v4().to_string(),
        ))
        .await
        .unwrap();

        let all = api.list_all_instances().await.unwrap();
        assert_eq!(all.len(), 1, "the endpoint-less instance is skipped");
        assert_eq!(all[0].gear, "worker");
        assert!(
            all.iter()
                .all(|i| i.endpoint.as_ref().is_some_and(|e| !e.uri.is_empty())),
            "no instance may carry an absent or empty-URI endpoint"
        );
    }

    #[tokio::test]
    async fn label_path_returns_endpoint_less_match() {
        let dir = Arc::new(GearManager::new());
        let api = LocalDirectoryClient::new(Arc::clone(&dir));

        // A matched instance that has not advertised any endpoint yet: treated
        // as no-endpoint (not an error), so the label-targeting paths must still
        // return the match (endpoint absent) rather than dropping it — the
        // caller owns the fall-back decision.
        api.register_instance(
            RegisterInstanceInfo::new("worker", Uuid::new_v4().to_string())
                .with_labels(labels(&[("shard", "7")])),
        )
        .await
        .unwrap();

        let matched = api
            .resolve_by_labels("worker", &LabelSelector::new().with("shard", "7"))
            .await
            .unwrap();
        assert_eq!(
            matched.len(),
            1,
            "resolve_by_labels must return a matched instance even with no endpoint"
        );
        assert!(
            matched[0].endpoint.is_none(),
            "a no-endpoint match carries endpoint = None, not an empty-URI sentinel"
        );
        assert!(matched[0].rest_endpoint.is_none());
        assert_eq!(matched[0].labels.get("shard"), Some(&"7".to_owned()));

        // The base per-gear enumeration upholds the same contract.
        let listed = api.list_instances("worker").await.unwrap();
        assert_eq!(
            listed.len(),
            1,
            "list_instances must not drop endpoint-less instances"
        );
        assert!(listed[0].endpoint.is_none());
    }
}
