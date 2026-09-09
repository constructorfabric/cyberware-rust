//! Unit tests for the Plugin Host binding (`Service::get_plugin` /
//! `resolve_plugin`), mirroring the credstore reference
//! (`modules/credstore/credstore/src/domain/service_tests.rs`).
//!
//! Coverage:
//! - resolve + cache (warm call reuses the cached id; same scoped Arc) —
//!   flow `inst-binding-lazy-resolve` / `inst-binding-return-handle`,
//!   algo `inst-algo-binding-get-or-init` / `inst-algo-binding-return`.
//! - registry-unavailable retries on the next call (no error caching) —
//!   algo `inst-algo-binding-catch` / `inst-algo-binding-registry-unavailable`.
//! - `PluginNotFound` on no-match / vendor mismatch —
//!   algo `inst-algo-binding-plugin-not-found`.
//! - `PluginUnavailable` when the scoped slot is empty —
//!   flow `inst-binding-try-get-scoped`, algo `inst-algo-binding-try-get-scoped`.
//! - monotonic binding for the Service lifetime (`reset` exercised in tests only).

use std::sync::Arc;

use authz_resolver_sdk::PolicyEnforcer;
use toolkit::client_hub::{ClientHub, ClientScope};
use types_registry_sdk::TypesRegistryClient;
use types_registry_sdk::testing::{
    MockTypesRegistryClient, internal as canonical_internal, make_test_instance,
};
use usage_collector_sdk::{UsageCollectorPluginSpecV1, UsageCollectorPluginV1};

use super::*;
use crate::domain::test_support::{MockPlugin, UnreachableResolver, enforcer_for};

/// Dummy enforcer for tests that never reach the PDP path
/// (binding / plugin-host tests). An unreachable PDP transport never matters
/// when no authz call is made.
fn dummy_enforcer() -> PolicyEnforcer {
    enforcer_for(Arc::new(UnreachableResolver))
}

// ── helpers ──────────────────────────────────────────────────────────────

fn empty_hub() -> Arc<ClientHub> {
    Arc::new(ClientHub::default())
}

/// Build the GTS instance ID string for a usage-collector storage-plugin test
/// instance: schema prefix + a 5-token instance suffix.
fn test_instance_id() -> String {
    format!(
        "{}test.usage_collector.mock.instance.v1",
        UsageCollectorPluginSpecV1::gts_type_id()
    )
}

/// JSON content for a `PluginV1<UsageCollectorPluginSpecV1>` instance that
/// `choose_plugin_instance` can successfully parse.
fn plugin_content(gts_id: &str, vendor: &str) -> serde_json::Value {
    serde_json::json!({
        "id": gts_id,
        "vendor": vendor,
        "priority": 0,
        "properties": {}
    })
}

/// Wires a counting `MockTypesRegistryClient` and a scoped plugin into a
/// `ClientHub`. Returns `(hub, registry_arc)` so tests can inspect
/// `list_instance_calls()`.
fn hub_with_counting_registry_and_plugin(
    instance_id: &str,
    vendor: &str,
    plugin: Arc<dyn UsageCollectorPluginV1>,
) -> (Arc<ClientHub>, Arc<MockTypesRegistryClient>) {
    let hub = Arc::new(ClientHub::default());

    let instance = make_test_instance(instance_id, plugin_content(instance_id, vendor));
    let registry = Arc::new(MockTypesRegistryClient::new().with_instances([instance]));
    hub.register::<dyn TypesRegistryClient>(registry.clone() as Arc<dyn TypesRegistryClient>);

    hub.register_scoped::<dyn UsageCollectorPluginV1>(ClientScope::gts_id(instance_id), plugin);

    (hub, registry)
}

fn hub_with_registry_and_plugin(
    instance_id: &str,
    vendor: &str,
    plugin: Arc<dyn UsageCollectorPluginV1>,
) -> Arc<ClientHub> {
    hub_with_counting_registry_and_plugin(instance_id, vendor, plugin).0
}

// ── resolve + cache ───────────────────────────────────────────────────────

// Covers flow `inst-binding-lazy-resolve` / `inst-binding-return-handle` and
// algo `inst-algo-binding-get-or-init` / `inst-algo-binding-resolve-plugin` /
// `inst-algo-binding-return`: the first dispatch resolves single-flight and the
// warm call reuses the cached id (no extra registry round-trip) and the same
// scoped Arc.
#[tokio::test]
async fn get_plugin_resolves_then_caches_resolved_instance() {
    let instance_id = test_instance_id();
    let (hub, registry) =
        hub_with_counting_registry_and_plugin(&instance_id, "constructorfabric", MockPlugin::arc());

    let svc = Service::new(hub, "constructorfabric".into(), dummy_enforcer());
    let p1 = svc.get_plugin().await.unwrap();
    let p2 = svc.get_plugin().await.unwrap();

    assert_eq!(
        registry.list_instance_calls(),
        1,
        "resolve_plugin must run exactly once; the warm call must use the cached id"
    );
    assert!(
        Arc::ptr_eq(&p1, &p2),
        "both calls must return the same scoped plugin Arc (cached binding)"
    );
}

// ── registry-unavailable retry (no error caching) ──────────────────────────

// Covers algo `inst-algo-binding-catch` / `inst-algo-binding-registry-unavailable`:
// a failing registry surfaces `TypesRegistryUnavailable`, the selector cache
// stays empty, and the NEXT dispatch retries (proven by list_instance_calls == 2).
#[tokio::test]
async fn get_plugin_retries_resolution_on_each_call_when_registry_fails() {
    let hub = Arc::new(ClientHub::default());
    let registry =
        Arc::new(MockTypesRegistryClient::new().with_list_error(canonical_internal("unavailable")));
    hub.register::<dyn TypesRegistryClient>(registry.clone() as Arc<dyn TypesRegistryClient>);

    let svc = Service::new(hub, "constructorfabric".into(), dummy_enforcer());

    let err = svc.get_plugin().await.err().expect("expected Err");
    assert!(
        matches!(err, DomainError::TypesRegistryUnavailable(_)),
        "expected TypesRegistryUnavailable, got: {err:?}"
    );
    assert!(svc.get_plugin().await.is_err());

    assert_eq!(
        registry.list_instance_calls(),
        2,
        "the selector must not cache errors; each dispatch must re-attempt resolution"
    );
}

// Covers algo `inst-algo-binding-registry-unavailable` for the missing-registry
// case: an empty hub (no registered TypesRegistryClient) surfaces
// `TypesRegistryUnavailable` from the explicit hub.get map_err.
#[tokio::test]
async fn get_plugin_returns_registry_unavailable_when_hub_empty() {
    let svc = Service::new(empty_hub(), "constructorfabric".into(), dummy_enforcer());
    let err = svc.get_plugin().await.err().expect("expected Err");
    assert!(
        matches!(err, DomainError::TypesRegistryUnavailable(_)),
        "expected TypesRegistryUnavailable, got: {err:?}"
    );
}

// ── PluginNotFound ─────────────────────────────────────────────────────────

// Covers algo `inst-algo-binding-plugin-not-found`: no registered instances ->
// `choose_plugin_instance` finds no match -> `PluginNotFound`.
#[tokio::test]
async fn get_plugin_returns_plugin_not_found_when_no_instances() {
    let hub = Arc::new(ClientHub::default());
    let registry: Arc<dyn TypesRegistryClient> = Arc::new(MockTypesRegistryClient::new());
    hub.register::<dyn TypesRegistryClient>(registry);

    let svc = Service::new(hub, "constructorfabric".into(), dummy_enforcer());
    let err = svc.get_plugin().await.err().expect("expected Err");
    assert!(
        matches!(err, DomainError::PluginNotFound { .. }),
        "expected PluginNotFound, got: {err:?}"
    );
}

// Covers algo `inst-algo-binding-plugin-not-found`: an instance exists but the
// vendor does not match the configured vendor -> `PluginNotFound`.
#[tokio::test]
async fn get_plugin_returns_plugin_not_found_when_vendor_mismatch() {
    let instance_id = test_instance_id();
    let hub = Arc::new(ClientHub::default());
    let instance = make_test_instance(&instance_id, plugin_content(&instance_id, "other-vendor"));
    let registry: Arc<dyn TypesRegistryClient> =
        Arc::new(MockTypesRegistryClient::new().with_instances([instance]));
    hub.register::<dyn TypesRegistryClient>(registry);

    let svc = Service::new(hub, "constructorfabric".into(), dummy_enforcer());
    let err = svc.get_plugin().await.err().expect("expected Err");
    assert!(
        matches!(err, DomainError::PluginNotFound { .. }),
        "expected PluginNotFound, got: {err:?}"
    );
}

// Covers algo `inst-algo-binding-resolve-plugin` malformed-content path:
// `choose_plugin_instance` fails to deserialize -> `InvalidPluginInstance`.
#[tokio::test]
async fn get_plugin_returns_invalid_when_content_malformed() {
    let instance_id = test_instance_id();
    let hub = Arc::new(ClientHub::default());
    let instance = make_test_instance(
        &instance_id,
        serde_json::json!({ "not": "valid-plugin-content" }),
    );
    let registry: Arc<dyn TypesRegistryClient> =
        Arc::new(MockTypesRegistryClient::new().with_instances([instance]));
    hub.register::<dyn TypesRegistryClient>(registry);

    let svc = Service::new(hub, "constructorfabric".into(), dummy_enforcer());
    let err = svc.get_plugin().await.err().expect("expected Err");
    assert!(
        matches!(err, DomainError::InvalidPluginInstance { .. }),
        "expected InvalidPluginInstance, got: {err:?}"
    );
}

// ── PluginUnavailable (empty scoped slot) ──────────────────────────────────

// Covers flow `inst-binding-try-get-scoped` and algo
// `inst-algo-binding-try-get-scoped`: the registry resolves successfully but the
// scoped client is absent -> `try_get_scoped` returns None -> `PluginUnavailable`.
#[tokio::test]
async fn get_plugin_returns_unavailable_when_scoped_slot_empty() {
    let instance_id = test_instance_id();
    let hub = Arc::new(ClientHub::default());
    let instance = make_test_instance(
        &instance_id,
        plugin_content(&instance_id, "constructorfabric"),
    );
    let registry: Arc<dyn TypesRegistryClient> =
        Arc::new(MockTypesRegistryClient::new().with_instances([instance]));
    hub.register::<dyn TypesRegistryClient>(registry);

    let svc = Service::new(hub, "constructorfabric".into(), dummy_enforcer());
    let err = svc.get_plugin().await.err().expect("expected Err");
    assert!(
        matches!(err, DomainError::PluginUnavailable { .. }),
        "expected PluginUnavailable, got: {err:?}"
    );
}

// ── monotonic binding for the Service lifetime ─────────────────────────────

// Covers algo `inst-algo-binding-get-or-init` caching semantics: the binding is
// monotonic for the Service lifetime. `GtsPluginSelector::reset` is exercised
// ONLY in unit tests (there is no runtime config-change channel); after reset the
// next dispatch re-resolves, proving the cache is the only re-resolution trigger.
#[tokio::test]
async fn binding_is_monotonic_until_selector_reset() {
    let instance_id = test_instance_id();
    let (hub, registry) =
        hub_with_counting_registry_and_plugin(&instance_id, "constructorfabric", MockPlugin::arc());

    let svc = Service::new(hub, "constructorfabric".into(), dummy_enforcer());

    // Two warm dispatches reuse the cached binding (monotonic).
    let _ = svc.get_plugin().await.unwrap();
    let _ = svc.get_plugin().await.unwrap();
    assert_eq!(
        registry.list_instance_calls(),
        1,
        "binding must be monotonic: no re-resolution without an explicit reset"
    );

    // Test-only reset clears the cache; the next dispatch re-resolves.
    assert!(
        svc.selector_reset_for_test().await,
        "reset must report a previously-cached value"
    );
    let _ = svc.get_plugin().await.unwrap();
    assert_eq!(
        registry.list_instance_calls(),
        2,
        "after a test-only reset the next dispatch must re-resolve"
    );
}

// Resolved handle identity is stable across a vendor that selects the lowest
// priority. Verifies the scoped Arc returned by the warm path matches the wired
// mock instance (`hub_with_registry_and_plugin` returns the hub only).
#[tokio::test]
async fn get_plugin_returns_registered_scoped_handle() {
    let instance_id = test_instance_id();
    let hub = hub_with_registry_and_plugin(&instance_id, "constructorfabric", MockPlugin::arc());

    let svc = Service::new(hub, "constructorfabric".into(), dummy_enforcer());
    let resolved = svc.get_plugin().await;
    assert!(
        resolved.is_ok(),
        "expected a resolved scoped handle, got: {:?}",
        resolved.err()
    );
}

// ═════════════════════════════════════════════════════════════════════════════
//  Register UsageType — Phase 4 service body tests
// ═════════════════════════════════════════════════════════════════════════════
//
// Coverage (each test pins one or more register-flow `inst-*` instructions and
// the load-bearing invariants from the Phase 4 acceptance criteria):
//
// - happy path counter prefix — full Ok pipeline.
// - happy path gauge prefix — same as above with a gauge `gts_id`.
// - PDP deny short-circuits BEFORE shape validation AND BEFORE plugin dispatch;
//   the plugin is never touched.
// - invalid `metadata_fields` (duplicate, empty string) returns
//   `InvalidMetadataField` / `DuplicateMetadataField`; plugin never invoked.
// - plugin `UsageTypeAlreadyExists` surfaces as
//   `UsageCollectorError::AlreadyExists`.
// - plugin transport error (Timeout, Internal) lifts to the platform error
//   envelope.

mod create_usage_type_tests {
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use toolkit_gts::gts_id;

    use std::collections::BTreeSet;

    use async_trait::async_trait;
    use toolkit::client_hub::{ClientHub, ClientScope};
    use toolkit_odata::{ODataQuery, Page as ODataPage};
    use toolkit_security::SecurityContext;
    use types_registry_sdk::TypesRegistryClient;
    use types_registry_sdk::testing::{MockTypesRegistryClient, make_test_instance};
    use usage_collector_sdk::{
        AggregationResult, AggregationSpec, MetadataFilter, MetadataKey, UsageCollectorClientV1,
        UsageCollectorError, UsageCollectorPluginError, UsageCollectorPluginSpecV1,
        UsageCollectorPluginV1, UsageKind, UsageRecord, UsageType, UsageTypeGtsId,
    };
    use uuid::Uuid;

    use crate::domain::Service;
    use crate::domain::local_client::UsageCollectorLocalClient;
    use crate::domain::test_support::{CountingAllowAllResolver, DenyAllResolver, enforcer_for};

    const COUNTER_ID: &str = gts_id!("cf.core.uc.usage_record.v1~example.usage._.bytes_in.v1");
    const GAUGE_ID: &str = gts_id!("cf.core.uc.usage_record.v1~example.usage._.cpu_load.v1");

    fn keyset<const N: usize>(values: [&str; N]) -> BTreeSet<MetadataKey> {
        values
            .into_iter()
            .map(|v| MetadataKey::new(v).expect("valid metadata key"))
            .collect()
    }

    /// Programmable register-stub plugin. Each `RegisterUsageType` call drains
    /// one response from the queue (in order of insertion) so the test
    /// scenarios can pin the exact plugin outcome under test. All other SPI
    /// methods return a contract-violation — any accidental dispatch shows up
    /// as an obvious test failure.
    enum RegisterStubResponse {
        Ok(UsageType),
        Err(UsageCollectorPluginError),
    }

    struct RegisterStubPlugin {
        register_calls: AtomicUsize,
        responses: Mutex<Vec<RegisterStubResponse>>,
    }

    impl RegisterStubPlugin {
        fn new(responses: Vec<RegisterStubResponse>) -> Arc<Self> {
            Arc::new(Self {
                register_calls: AtomicUsize::new(0),
                responses: Mutex::new(responses),
            })
        }

        fn register_calls(&self) -> usize {
            self.register_calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl UsageCollectorPluginV1 for RegisterStubPlugin {
        async fn create_usage_type(
            &self,
            _usage_type: UsageType,
        ) -> Result<UsageType, UsageCollectorPluginError> {
            self.register_calls.fetch_add(1, Ordering::SeqCst);
            let mut guard = self.responses.lock().expect("rwlock not poisoned");
            if guard.is_empty() {
                return Err(UsageCollectorPluginError::internal(
                    "test_fake: RegisterStubPlugin: no programmed response remaining",
                ));
            }
            match guard.remove(0) {
                RegisterStubResponse::Ok(usage_type) => Ok(usage_type),
                RegisterStubResponse::Err(err) => Err(err),
            }
        }

        async fn create_usage_record(
            &self,
            _record: UsageRecord,
        ) -> Result<UsageRecord, UsageCollectorPluginError> {
            Err(UsageCollectorPluginError::internal(
                "test_fake: RegisterStubPlugin: create_usage_record must not be called",
            ))
        }

        async fn create_usage_records(
            &self,
            _records: Vec<UsageRecord>,
        ) -> Result<Vec<Result<UsageRecord, UsageCollectorPluginError>>, UsageCollectorPluginError>
        {
            Err(UsageCollectorPluginError::internal(
                "test_fake: RegisterStubPlugin: create_usage_records must not be called",
            ))
        }

        async fn query_aggregated_usage_records(
            &self,
            _gts_id: UsageTypeGtsId,
            _query: &ODataQuery,
            _metadata_filter: &[MetadataFilter],
            _aggregation: AggregationSpec,
        ) -> Result<AggregationResult, UsageCollectorPluginError> {
            Err(UsageCollectorPluginError::internal(
                "test_fake: RegisterStubPlugin: query_aggregated_usage_records must not be called",
            ))
        }

        async fn list_usage_records(
            &self,
            _gts_id: UsageTypeGtsId,
            _query: &ODataQuery,
            _metadata_filter: &[MetadataFilter],
        ) -> Result<ODataPage<UsageRecord>, UsageCollectorPluginError> {
            Err(UsageCollectorPluginError::internal(
                "test_fake: RegisterStubPlugin: list_usage_records must not be called",
            ))
        }

        async fn deactivate_usage_record(
            &self,
            _id: Uuid,
        ) -> Result<(), UsageCollectorPluginError> {
            Err(UsageCollectorPluginError::internal(
                "test_fake: RegisterStubPlugin: deactivate_usage_record must not be called",
            ))
        }

        async fn get_usage_type(
            &self,
            _gts_id: UsageTypeGtsId,
        ) -> Result<UsageType, UsageCollectorPluginError> {
            Err(UsageCollectorPluginError::internal(
                "test_fake: RegisterStubPlugin: get_usage_type must not be called",
            ))
        }

        async fn list_usage_types(
            &self,
            _query: &ODataQuery,
        ) -> Result<ODataPage<UsageType>, UsageCollectorPluginError> {
            Err(UsageCollectorPluginError::internal(
                "test_fake: RegisterStubPlugin: list_usage_types must not be called",
            ))
        }

        async fn delete_usage_type(
            &self,
            _gts_id: UsageTypeGtsId,
        ) -> Result<(), UsageCollectorPluginError> {
            Err(UsageCollectorPluginError::internal(
                "test_fake: RegisterStubPlugin: delete_usage_type must not be called",
            ))
        }

        async fn get_usage_record(
            &self,
            _id: Uuid,
        ) -> Result<UsageRecord, UsageCollectorPluginError> {
            Err(UsageCollectorPluginError::internal(
                "test_fake: RegisterStubPlugin: get_usage_record must not be called",
            ))
        }
    }

    fn counter_gts_id() -> UsageTypeGtsId {
        UsageTypeGtsId::new(COUNTER_ID).expect("valid counter gts_id")
    }

    fn gauge_gts_id() -> UsageTypeGtsId {
        UsageTypeGtsId::new(GAUGE_ID).expect("valid gauge gts_id")
    }

    fn record_for(
        gts_id: UsageTypeGtsId,
        kind: UsageKind,
        metadata_fields: BTreeSet<MetadataKey>,
    ) -> UsageType {
        UsageType {
            gts_id,
            kind,
            metadata_fields,
        }
    }

    fn test_instance_id_for(suffix: &str) -> String {
        format!("{}{suffix}", UsageCollectorPluginSpecV1::gts_type_id())
    }

    fn plugin_content(gts_id: &str, vendor: &str) -> serde_json::Value {
        serde_json::json!({
            "id": gts_id,
            "vendor": vendor,
            "priority": 0,
            "properties": {}
        })
    }

    /// Build a `ClientHub` carrying a registry that resolves to the supplied
    /// scoped plugin under the configured vendor.
    fn hub_with(plugin: Arc<dyn UsageCollectorPluginV1>, suffix: &str) -> Arc<ClientHub> {
        let hub = Arc::new(ClientHub::default());
        let instance_id = test_instance_id_for(suffix);
        let instance = make_test_instance(
            &instance_id,
            plugin_content(&instance_id, "constructorfabric"),
        );
        let registry: Arc<dyn TypesRegistryClient> =
            Arc::new(MockTypesRegistryClient::new().with_instances([instance]));
        hub.register::<dyn TypesRegistryClient>(registry);
        hub.register_scoped::<dyn UsageCollectorPluginV1>(
            ClientScope::gts_id(&instance_id),
            plugin,
        );
        hub
    }

    fn authenticated_ctx() -> SecurityContext {
        SecurityContext::builder()
            .subject_id(Uuid::from_u128(1))
            .subject_tenant_id(Uuid::from_u128(2))
            .subject_type("user")
            .build()
            .expect("authenticated context")
    }

    /// Build a service wired with a permit-by-default PDP and the supplied
    /// plugin stub.
    fn service_with_permit(plugin: Arc<dyn UsageCollectorPluginV1>, suffix: &str) -> Arc<Service> {
        let hub = hub_with(plugin, suffix);
        let resolver = CountingAllowAllResolver::new();
        let enforcer = enforcer_for(resolver);
        Arc::new(Service::new(hub, "constructorfabric".into(), enforcer))
    }

    /// Build a service wired with a deny-by-default PDP plus the supplied
    /// plugin stub. The PDP deny test asserts that the plugin (and the
    /// validation function) are never reached on this path.
    fn service_with_deny(plugin: Arc<dyn UsageCollectorPluginV1>, suffix: &str) -> Arc<Service> {
        let hub = hub_with(plugin, suffix);
        let resolver = Arc::new(DenyAllResolver);
        let enforcer = enforcer_for(resolver);
        Arc::new(Service::new(hub, "constructorfabric".into(), enforcer))
    }

    // ── happy path: counter prefix ──────────────────────────────────────────
    #[tokio::test]
    async fn create_usage_type_happy_path_counter_prefix() {
        let gts_id = counter_gts_id();
        let metadata_fields = keyset(["region", "az"]);
        let plugin = RegisterStubPlugin::new(vec![RegisterStubResponse::Ok(record_for(
            gts_id.clone(),
            UsageKind::Counter,
            metadata_fields.clone(),
        ))]);
        let svc = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.usage_collector.register.happy_counter.v1",
        );

        let ctx = authenticated_ctx();
        let input = UsageType {
            gts_id: gts_id.clone(),
            kind: UsageKind::Counter,
            metadata_fields: metadata_fields.clone(),
        };

        let record = svc
            .create_usage_type(&ctx, input)
            .await
            .expect("happy-path counter registration must succeed");

        assert_eq!(record.gts_id, gts_id);
        assert_eq!(record.metadata_fields, metadata_fields);
        assert_eq!(
            plugin.register_calls(),
            1,
            "plugin SPI must be called exactly once on the happy path"
        );
    }

    // ── happy path: gauge prefix ────────────────────────────────────────────
    #[tokio::test]
    async fn create_usage_type_happy_path_gauge_prefix() {
        let gts_id = gauge_gts_id();
        let metadata_fields: BTreeSet<MetadataKey> = BTreeSet::new();
        let plugin = RegisterStubPlugin::new(vec![RegisterStubResponse::Ok(record_for(
            gts_id.clone(),
            UsageKind::Gauge,
            metadata_fields.clone(),
        ))]);
        let svc = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.usage_collector.register.happy_gauge.v1",
        );

        let ctx = authenticated_ctx();
        let input = UsageType {
            gts_id: gts_id.clone(),
            kind: UsageKind::Gauge,
            metadata_fields: metadata_fields.clone(),
        };

        let record = svc
            .create_usage_type(&ctx, input)
            .await
            .expect("happy-path gauge registration must succeed");

        assert!(record.is_gauge());
        assert_eq!(record.metadata_fields, metadata_fields);
        assert_eq!(plugin.register_calls(), 1);
    }

    // ── PDP deny short-circuits before shape and before plugin dispatch ────
    #[tokio::test]
    async fn create_usage_type_pdp_deny_short_circuits() {
        let plugin = RegisterStubPlugin::new(vec![
            // The plugin MUST NOT be called on the deny path. We program
            // an Ok response that, if accidentally drained, would let the
            // test pass silently — so we leave the queue EMPTY and the
            // stub will surface a contract-violation if dispatched.
        ]);
        let svc = service_with_deny(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.usage_collector.register.deny.v1",
        );

        let ctx = authenticated_ctx();
        let input = UsageType {
            gts_id: counter_gts_id(),
            kind: UsageKind::Counter,
            metadata_fields: keyset(["region"]),
        };

        let err = svc
            .create_usage_type(&ctx, input)
            .await
            .expect_err("PDP deny must short-circuit with an authorization error");

        assert!(
            matches!(err, UsageCollectorError::PermissionDenied { .. }),
            "expected PermissionDenied, got: {err:?}"
        );
        assert_eq!(
            plugin.register_calls(),
            0,
            "PDP deny must short-circuit before plugin dispatch"
        );
    }

    // Shape validation for `metadata_fields` (duplicates / empty strings /
    // NUL bytes) moved out of the service layer: the SDK newtype
    // `BTreeSet<MetadataKey>` enforces those invariants at construction, and
    // the REST handler converts the wire shape via
    // `metadata_fields_from_wire`. See `domain::validation::tests` for the
    // wire-boundary coverage and `crate::api::rest::handlers::usage_types`
    // for the REST plumbing.

    // ── plugin `UsageTypeAlreadyExists` ────────────────────────────────────
    #[tokio::test]
    async fn create_usage_type_plugin_already_exists_surfaces_already_exists() {
        let gts_id = counter_gts_id();
        let plugin = RegisterStubPlugin::new(vec![RegisterStubResponse::Err(
            UsageCollectorPluginError::UsageTypeAlreadyExists {
                gts_id: gts_id.clone(),
            },
        )]);
        let svc = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.usage_collector.register.already_exists.v1",
        );

        let ctx = authenticated_ctx();
        let input = UsageType {
            gts_id: gts_id.clone(),
            kind: UsageKind::Counter,
            metadata_fields: keyset(["region"]),
        };

        let err = svc
            .create_usage_type(&ctx, input)
            .await
            .expect_err("plugin must surface UsageTypeAlreadyExists");

        match err {
            UsageCollectorError::AlreadyExists { name: g, .. } => {
                assert_eq!(g, gts_id.as_ref());
            }
            other => panic!("expected AlreadyExists, got {other:?}"),
        }
        assert_eq!(
            plugin.register_calls(),
            1,
            "plugin SPI is called exactly once even on the duplicate path"
        );
    }

    // ── plugin transport / availability error ──────────────────────────────
    #[tokio::test]
    async fn create_usage_type_plugin_transient_returns_service_unavailable() {
        let plugin = RegisterStubPlugin::new(vec![RegisterStubResponse::Err(
            UsageCollectorPluginError::transient("downstream connection reset"),
        )]);
        let svc = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.usage_collector.register.transient.v1",
        );

        let ctx = authenticated_ctx();
        let input = UsageType {
            gts_id: counter_gts_id(),
            kind: UsageKind::Counter,
            metadata_fields: keyset(["region"]),
        };

        let err = svc
            .create_usage_type(&ctx, input)
            .await
            .expect_err("plugin transient failure must surface a retryable envelope");

        match &err {
            UsageCollectorError::ServiceUnavailable { detail, .. } => {
                assert_eq!(detail, "downstream connection reset");
            }
            other => panic!("expected ServiceUnavailable, got: {other:?}"),
        }
        assert!(err.is_retryable(), "Transient lift must be retryable");
    }

    #[tokio::test]
    async fn create_usage_type_plugin_internal_returns_internal() {
        let plugin = RegisterStubPlugin::new(vec![RegisterStubResponse::Err(
            UsageCollectorPluginError::internal("io: disk full"),
        )]);
        let svc = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.usage_collector.register.plugin_internal.v1",
        );

        let ctx = authenticated_ctx();
        let input = UsageType {
            gts_id: counter_gts_id(),
            kind: UsageKind::Counter,
            metadata_fields: keyset(["region"]),
        };

        let err = svc
            .create_usage_type(&ctx, input)
            .await
            .expect_err("plugin backend error must lift to Internal");

        assert!(
            matches!(err, UsageCollectorError::Internal { .. }),
            "expected Internal, got: {err:?}"
        );
    }

    // ── round-trip through the local client (SDK trait impl) ───────────────
    //
    // Pins the `inst-register-usage-type-submit` block marker entry: the SDK
    // trait method is a thin wrapper that traverses the same gateway
    // service path the REST handler will use.
    #[tokio::test]
    async fn local_client_create_usage_type_delegates_into_service() {
        let gts_id = counter_gts_id();
        let plugin = RegisterStubPlugin::new(vec![RegisterStubResponse::Ok(record_for(
            gts_id.clone(),
            UsageKind::Counter,
            keyset(["region"]),
        ))]);
        let svc = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.usage_collector.register.local_client.v1",
        );

        let client = UsageCollectorLocalClient::new(svc);
        let ctx = authenticated_ctx();
        let record = client
            .create_usage_type(
                &ctx,
                UsageType {
                    gts_id: gts_id.clone(),
                    kind: UsageKind::Counter,
                    metadata_fields: keyset(["region"]),
                },
            )
            .await
            .expect("trait-surface delegation must succeed");

        assert_eq!(record.gts_id, gts_id);
        assert_eq!(plugin.register_calls(), 1);
    }
}

// ═════════════════════════════════════════════════════════════════════════════
//  Read / List / Delete UsageType — Service catalog-dispatch tests
// ═════════════════════════════════════════════════════════════════════════════
//
// Each catalog-dispatch method on `Service` follows the same shape:
//
//   1. authorize_usage_type(ctx, ...) — PDP gate via the shared helper.
//      Deny / unavailable short-circuit BEFORE the plugin is touched.
//   2. get_plugin() — lazy `GtsPluginSelector` resolution.
//   3. plugin.<method>(...) — SPI dispatch with the same error-envelope
//      mapping used everywhere else in the service.
//
// `get_usage_type` surfaces the plugin's `UsageTypeNotFound` verbatim as
// `UsageCollectorError::NotFound { gts_id }`.
// `delete_usage_type` surfaces the plugin's FK-rejection variant as
// `UsageCollectorError::Conflict { ... }`.

mod catalog_dispatch_tests {
    use std::collections::BTreeSet;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use toolkit_gts::gts_id;

    use async_trait::async_trait;
    use toolkit::client_hub::{ClientHub, ClientScope};
    use toolkit_odata::{ODataQuery, Page as ODataPage};
    use toolkit_security::SecurityContext;
    use types_registry_sdk::TypesRegistryClient;
    use types_registry_sdk::testing::{MockTypesRegistryClient, make_test_instance};
    use usage_collector_sdk::{
        AggregationResult, AggregationSpec, ConflictReason, MetadataFilter, MetadataKey,
        USAGE_TYPE_RESOURCE, UsageCollectorError, UsageCollectorPluginError,
        UsageCollectorPluginSpecV1, UsageCollectorPluginV1, UsageKind, UsageRecord, UsageType,
        UsageTypeGtsId,
    };
    use uuid::Uuid;

    use crate::domain::Service;
    use crate::domain::test_support::{CountingAllowAllResolver, DenyAllResolver, enforcer_for};

    fn keyset<const N: usize>(values: [&str; N]) -> BTreeSet<MetadataKey> {
        values
            .into_iter()
            .map(|v| MetadataKey::new(v).expect("valid metadata key"))
            .collect()
    }

    const COUNTER_ID: &str = gts_id!("cf.core.uc.usage_record.v1~example.usage._.bytes_in.v1");

    // ── programmable stub plugin: per-op response queues + call counters ────

    enum ReadResponse {
        Ok(UsageType),
        Err(UsageCollectorPluginError),
    }

    enum ListResponse {
        Ok(ODataPage<UsageType>),
        Err(UsageCollectorPluginError),
    }

    enum DeleteResponse {
        Ok,
        Err(UsageCollectorPluginError),
    }

    struct CatalogStubPlugin {
        read_calls: AtomicUsize,
        list_calls: AtomicUsize,
        delete_calls: AtomicUsize,
        read_queue: Mutex<Vec<ReadResponse>>,
        list_queue: Mutex<Vec<ListResponse>>,
        delete_queue: Mutex<Vec<DeleteResponse>>,
        last_delete_input: Mutex<Option<UsageTypeGtsId>>,
    }

    impl CatalogStubPlugin {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                read_calls: AtomicUsize::new(0),
                list_calls: AtomicUsize::new(0),
                delete_calls: AtomicUsize::new(0),
                read_queue: Mutex::new(Vec::new()),
                list_queue: Mutex::new(Vec::new()),
                delete_queue: Mutex::new(Vec::new()),
                last_delete_input: Mutex::new(None),
            })
        }

        fn with_read(self: Arc<Self>, response: ReadResponse) -> Arc<Self> {
            self.read_queue.lock().unwrap().push(response);
            self
        }

        fn with_list(self: Arc<Self>, response: ListResponse) -> Arc<Self> {
            self.list_queue.lock().unwrap().push(response);
            self
        }

        fn with_delete(self: Arc<Self>, response: DeleteResponse) -> Arc<Self> {
            self.delete_queue.lock().unwrap().push(response);
            self
        }

        fn read_calls(&self) -> usize {
            self.read_calls.load(Ordering::SeqCst)
        }
        fn list_calls(&self) -> usize {
            self.list_calls.load(Ordering::SeqCst)
        }
        fn delete_calls(&self) -> usize {
            self.delete_calls.load(Ordering::SeqCst)
        }

        fn last_delete_input(&self) -> Option<UsageTypeGtsId> {
            self.last_delete_input.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl UsageCollectorPluginV1 for CatalogStubPlugin {
        async fn create_usage_type(
            &self,
            _usage_type: UsageType,
        ) -> Result<UsageType, UsageCollectorPluginError> {
            Err(UsageCollectorPluginError::internal(
                "test_fake: CatalogStubPlugin: create_usage_type must not be called",
            ))
        }

        async fn create_usage_record(
            &self,
            _record: UsageRecord,
        ) -> Result<UsageRecord, UsageCollectorPluginError> {
            Err(UsageCollectorPluginError::internal(
                "test_fake: CatalogStubPlugin: create_usage_record must not be called",
            ))
        }

        async fn create_usage_records(
            &self,
            _records: Vec<UsageRecord>,
        ) -> Result<Vec<Result<UsageRecord, UsageCollectorPluginError>>, UsageCollectorPluginError>
        {
            Err(UsageCollectorPluginError::internal(
                "test_fake: CatalogStubPlugin: create_usage_records must not be called",
            ))
        }

        async fn query_aggregated_usage_records(
            &self,
            _gts_id: UsageTypeGtsId,
            _query: &ODataQuery,
            _metadata_filter: &[MetadataFilter],
            _aggregation: AggregationSpec,
        ) -> Result<AggregationResult, UsageCollectorPluginError> {
            Err(UsageCollectorPluginError::internal(
                "test_fake: CatalogStubPlugin: query_aggregated_usage_records must not be called",
            ))
        }

        async fn list_usage_records(
            &self,
            _gts_id: UsageTypeGtsId,
            _query: &ODataQuery,
            _metadata_filter: &[MetadataFilter],
        ) -> Result<ODataPage<UsageRecord>, UsageCollectorPluginError> {
            Err(UsageCollectorPluginError::internal(
                "test_fake: CatalogStubPlugin: list_usage_records must not be called",
            ))
        }

        async fn deactivate_usage_record(
            &self,
            _id: Uuid,
        ) -> Result<(), UsageCollectorPluginError> {
            Err(UsageCollectorPluginError::internal(
                "test_fake: CatalogStubPlugin: deactivate_usage_record must not be called",
            ))
        }

        async fn get_usage_type(
            &self,
            _gts_id: UsageTypeGtsId,
        ) -> Result<UsageType, UsageCollectorPluginError> {
            self.read_calls.fetch_add(1, Ordering::SeqCst);
            let mut q = self.read_queue.lock().unwrap();
            if q.is_empty() {
                return Err(UsageCollectorPluginError::internal(
                    "test_fake: CatalogStubPlugin: no programmed read response",
                ));
            }
            match q.remove(0) {
                ReadResponse::Ok(v) => Ok(v),
                ReadResponse::Err(e) => Err(e),
            }
        }

        async fn list_usage_types(
            &self,
            _query: &ODataQuery,
        ) -> Result<ODataPage<UsageType>, UsageCollectorPluginError> {
            self.list_calls.fetch_add(1, Ordering::SeqCst);
            let mut q = self.list_queue.lock().unwrap();
            if q.is_empty() {
                return Err(UsageCollectorPluginError::internal(
                    "test_fake: CatalogStubPlugin: no programmed list response",
                ));
            }
            match q.remove(0) {
                ListResponse::Ok(p) => Ok(p),
                ListResponse::Err(e) => Err(e),
            }
        }

        async fn delete_usage_type(
            &self,
            gts_id: UsageTypeGtsId,
        ) -> Result<(), UsageCollectorPluginError> {
            self.delete_calls.fetch_add(1, Ordering::SeqCst);
            *self.last_delete_input.lock().unwrap() = Some(gts_id);
            let mut q = self.delete_queue.lock().unwrap();
            if q.is_empty() {
                return Err(UsageCollectorPluginError::internal(
                    "test_fake: CatalogStubPlugin: no programmed delete response",
                ));
            }
            match q.remove(0) {
                DeleteResponse::Ok => Ok(()),
                DeleteResponse::Err(e) => Err(e),
            }
        }

        async fn get_usage_record(
            &self,
            _id: Uuid,
        ) -> Result<UsageRecord, UsageCollectorPluginError> {
            Err(UsageCollectorPluginError::internal(
                "test_fake: CatalogStubPlugin: get_usage_record must not be called",
            ))
        }
    }

    // ── shared helpers (mirror create_usage_type_tests for symmetry) ─────

    fn counter_gts_id() -> UsageTypeGtsId {
        UsageTypeGtsId::new(COUNTER_ID).expect("valid counter gts_id")
    }

    fn sample_record() -> UsageType {
        UsageType {
            gts_id: counter_gts_id(),
            kind: UsageKind::Counter,
            metadata_fields: keyset(["region"]),
        }
    }

    fn test_instance_id_for(suffix: &str) -> String {
        format!("{}{suffix}", UsageCollectorPluginSpecV1::gts_type_id())
    }

    fn plugin_content(gts_id: &str, vendor: &str) -> serde_json::Value {
        serde_json::json!({
            "id": gts_id,
            "vendor": vendor,
            "priority": 0,
            "properties": {}
        })
    }

    fn hub_with(plugin: Arc<dyn UsageCollectorPluginV1>, suffix: &str) -> Arc<ClientHub> {
        let hub = Arc::new(ClientHub::default());
        let instance_id = test_instance_id_for(suffix);
        let instance = make_test_instance(
            &instance_id,
            plugin_content(&instance_id, "constructorfabric"),
        );
        let registry: Arc<dyn TypesRegistryClient> =
            Arc::new(MockTypesRegistryClient::new().with_instances([instance]));
        hub.register::<dyn TypesRegistryClient>(registry);
        hub.register_scoped::<dyn UsageCollectorPluginV1>(
            ClientScope::gts_id(&instance_id),
            plugin,
        );
        hub
    }

    fn authenticated_ctx() -> SecurityContext {
        SecurityContext::builder()
            .subject_id(Uuid::from_u128(1))
            .subject_tenant_id(Uuid::from_u128(2))
            .subject_type("user")
            .build()
            .expect("authenticated context")
    }

    fn service_with_permit(plugin: Arc<dyn UsageCollectorPluginV1>, suffix: &str) -> Arc<Service> {
        let hub = hub_with(plugin, suffix);
        let resolver = CountingAllowAllResolver::new();
        let enforcer = enforcer_for(resolver);
        Arc::new(Service::new(hub, "constructorfabric".into(), enforcer))
    }

    fn service_with_deny(plugin: Arc<dyn UsageCollectorPluginV1>, suffix: &str) -> Arc<Service> {
        let hub = hub_with(plugin, suffix);
        let resolver = Arc::new(DenyAllResolver);
        let enforcer = enforcer_for(resolver);
        Arc::new(Service::new(hub, "constructorfabric".into(), enforcer))
    }

    fn service_with_unreachable_pdp(
        plugin: Arc<dyn UsageCollectorPluginV1>,
        suffix: &str,
    ) -> Arc<Service> {
        let hub = hub_with(plugin, suffix);
        Arc::new(Service::new(
            hub,
            "constructorfabric".into(),
            super::dummy_enforcer(),
        ))
    }

    // ── get_usage_type ────────────────────────────────────────────────────

    #[tokio::test]
    async fn get_usage_type_pdp_deny_short_circuits() {
        let plugin = CatalogStubPlugin::new();
        let svc = service_with_deny(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.usage_collector.read.deny.v1",
        );
        let err = svc
            .get_usage_type(&authenticated_ctx(), counter_gts_id())
            .await
            .expect_err("deny must short-circuit");
        assert!(
            matches!(err, UsageCollectorError::PermissionDenied { .. }),
            "expected PermissionDenied, got: {err:?}"
        );
        assert_eq!(
            plugin.read_calls(),
            0,
            "deny path must not dispatch through the plugin"
        );
    }

    #[tokio::test]
    async fn get_usage_type_pdp_unavailable_when_enforcer_unwired() {
        let plugin = CatalogStubPlugin::new();
        let svc = service_with_unreachable_pdp(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.usage_collector.read.unavailable.v1",
        );
        let err = svc
            .get_usage_type(&authenticated_ctx(), counter_gts_id())
            .await
            .expect_err("unwired enforcer must surface AuthorizationUnavailable");
        assert!(
            matches!(err, UsageCollectorError::ServiceUnavailable { .. }),
            "expected ServiceUnavailable, got: {err:?}"
        );
        assert_eq!(plugin.read_calls(), 0);
    }

    #[tokio::test]
    async fn get_usage_type_happy_path_returns_record() {
        let plugin = CatalogStubPlugin::new().with_read(ReadResponse::Ok(sample_record()));
        let svc = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.usage_collector.read.happy.v1",
        );
        let record = svc
            .get_usage_type(&authenticated_ctx(), counter_gts_id())
            .await
            .expect("happy-path read must succeed");
        assert_eq!(record, sample_record());
        assert_eq!(plugin.read_calls(), 1);
    }

    #[tokio::test]
    async fn get_usage_type_plugin_miss_lifts_to_not_found() {
        let plugin = CatalogStubPlugin::new().with_read(ReadResponse::Err(
            UsageCollectorPluginError::UsageTypeNotFound {
                gts_id: counter_gts_id(),
            },
        ));
        let svc = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.usage_collector.read.miss.v1",
        );
        let err = svc
            .get_usage_type(&authenticated_ctx(), counter_gts_id())
            .await
            .expect_err("plugin UsageTypeNotFound must lift to SDK NotFound");
        assert!(
            matches!(
                err,
                UsageCollectorError::NotFound { ref resource_type, ref name, .. }
                    if resource_type == USAGE_TYPE_RESOURCE && name == counter_gts_id().as_ref()
            ),
            "expected NotFound, got: {err:?}"
        );
        assert_eq!(plugin.read_calls(), 1);
    }

    #[tokio::test]
    async fn get_usage_type_plugin_error_returns_platform_envelope() {
        let plugin = CatalogStubPlugin::new().with_read(ReadResponse::Err(
            UsageCollectorPluginError::internal("io: disk full"),
        ));
        let svc = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.usage_collector.read.plugin_err.v1",
        );
        let err = svc
            .get_usage_type(&authenticated_ctx(), counter_gts_id())
            .await
            .expect_err("plugin backend error must lift to Internal");
        assert!(
            matches!(err, UsageCollectorError::Internal { .. }),
            "expected Internal, got: {err:?}"
        );
        assert_eq!(plugin.read_calls(), 1);
    }

    // ── list_usage_types ───────────────────────────────────────────────────

    #[tokio::test]
    async fn list_usage_types_pdp_deny_short_circuits() {
        let plugin = CatalogStubPlugin::new();
        let svc = service_with_deny(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.usage_collector.list.deny.v1",
        );
        let err = svc
            .list_usage_types(&authenticated_ctx(), &ODataQuery::default())
            .await
            .expect_err("deny must short-circuit");
        assert!(matches!(err, UsageCollectorError::PermissionDenied { .. }));
        assert_eq!(plugin.list_calls(), 0);
    }

    #[tokio::test]
    async fn list_usage_types_pdp_unavailable_when_enforcer_unwired() {
        let plugin = CatalogStubPlugin::new();
        let svc = service_with_unreachable_pdp(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.usage_collector.list.unavailable.v1",
        );
        let err = svc
            .list_usage_types(&authenticated_ctx(), &ODataQuery::default())
            .await
            .expect_err("unwired enforcer must surface AuthorizationUnavailable");
        assert!(matches!(
            err,
            UsageCollectorError::ServiceUnavailable { .. }
        ));
        assert_eq!(plugin.list_calls(), 0);
    }

    #[tokio::test]
    async fn list_usage_types_happy_path_returns_page() {
        let page_info = toolkit_odata::PageInfo {
            next_cursor: Some("next-token".to_owned()),
            prev_cursor: None,
            limit: 25,
        };
        let page = ODataPage::new(vec![sample_record()], page_info.clone());
        let plugin = CatalogStubPlugin::new().with_list(ListResponse::Ok(page));
        let svc = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.usage_collector.list.happy.v1",
        );
        let result = svc
            .list_usage_types(&authenticated_ctx(), &ODataQuery::default())
            .await
            .expect("happy-path list must succeed");
        assert_eq!(result.items, vec![sample_record()]);
        assert_eq!(result.page_info.next_cursor, page_info.next_cursor);
        assert_eq!(result.page_info.prev_cursor, page_info.prev_cursor);
        assert_eq!(result.page_info.limit, page_info.limit);
        assert_eq!(plugin.list_calls(), 1);
    }

    #[tokio::test]
    async fn list_usage_types_plugin_error_returns_platform_envelope() {
        let plugin = CatalogStubPlugin::new().with_list(ListResponse::Err(
            UsageCollectorPluginError::internal("io: disk full"),
        ));
        let svc = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.usage_collector.list.plugin_err.v1",
        );
        let err = svc
            .list_usage_types(&authenticated_ctx(), &ODataQuery::default())
            .await
            .expect_err("plugin backend error must lift to Internal");
        assert!(matches!(err, UsageCollectorError::Internal { .. }));
        assert_eq!(plugin.list_calls(), 1);
    }

    // ── delete_usage_type ──────────────────────────────────────────────────

    #[tokio::test]
    async fn delete_usage_type_pdp_deny_short_circuits() {
        let plugin = CatalogStubPlugin::new();
        let svc = service_with_deny(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.usage_collector.delete.deny.v1",
        );
        let err = svc
            .delete_usage_type(&authenticated_ctx(), counter_gts_id())
            .await
            .expect_err("deny must short-circuit");
        assert!(matches!(err, UsageCollectorError::PermissionDenied { .. }));
        assert_eq!(plugin.delete_calls(), 0);
    }

    #[tokio::test]
    async fn delete_usage_type_pdp_unavailable_when_enforcer_unwired() {
        let plugin = CatalogStubPlugin::new();
        let svc = service_with_unreachable_pdp(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.usage_collector.delete.unavailable.v1",
        );
        let err = svc
            .delete_usage_type(&authenticated_ctx(), counter_gts_id())
            .await
            .expect_err("unwired enforcer must surface AuthorizationUnavailable");
        assert!(matches!(
            err,
            UsageCollectorError::ServiceUnavailable { .. }
        ));
        assert_eq!(plugin.delete_calls(), 0);
    }

    #[tokio::test]
    async fn delete_usage_type_happy_path() {
        let plugin = CatalogStubPlugin::new().with_delete(DeleteResponse::Ok);
        let svc = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.usage_collector.delete.happy.v1",
        );
        svc.delete_usage_type(&authenticated_ctx(), counter_gts_id())
            .await
            .expect("happy-path delete must succeed");
        assert_eq!(plugin.delete_calls(), 1);
        assert_eq!(
            plugin.last_delete_input(),
            Some(counter_gts_id()),
            "service must forward the caller's gts_id verbatim to the plugin",
        );
    }

    #[tokio::test]
    async fn delete_usage_type_plugin_fk_referenced_surfaces_referenced_envelope() {
        let plugin = CatalogStubPlugin::new().with_delete(DeleteResponse::Err(
            UsageCollectorPluginError::UsageTypeReferenced {
                gts_id: counter_gts_id(),
                sample_ref_count: 3,
            },
        ));
        let svc = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.usage_collector.delete.referenced.v1",
        );
        let err = svc
            .delete_usage_type(&authenticated_ctx(), counter_gts_id())
            .await
            .expect_err("FK-referenced delete must surface UsageTypeReferenced");
        assert!(
            matches!(
                err,
                UsageCollectorError::Conflict {
                    reason: ConflictReason::UsageTypeReferenced,
                    ref name,
                    ref detail,
                    ..
                } if name == counter_gts_id().as_ref()
                    && detail.contains("referenced by 3 samples")
            ),
            "expected Conflict(UsageTypeReferenced), got: {err:?}"
        );
        assert_eq!(plugin.delete_calls(), 1);
    }

    #[tokio::test]
    async fn delete_usage_type_plugin_not_found_surfaces_not_found_envelope() {
        let plugin = CatalogStubPlugin::new().with_delete(DeleteResponse::Err(
            UsageCollectorPluginError::UsageTypeNotFound {
                gts_id: counter_gts_id(),
            },
        ));
        let svc = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.usage_collector.delete.not_found.v1",
        );
        let err = svc
            .delete_usage_type(&authenticated_ctx(), counter_gts_id())
            .await
            .expect_err("missing target must surface NotFound");
        assert!(
            matches!(
                err,
                UsageCollectorError::NotFound { ref resource_type, ref name, .. }
                    if resource_type == USAGE_TYPE_RESOURCE && name == counter_gts_id().as_ref()
            ),
            "expected NotFound, got: {err:?}"
        );
        assert_eq!(plugin.delete_calls(), 1);
    }
}

// ─── event-deactivation feature ──────────────────────────────────────
//
// `Service::deactivate_usage_record` is the host-side gateway for the
// event-deactivation feature: PDP authz preflight, lazy plugin resolution,
// Plugin SPI Method 5 dispatch, and 1:1 outcome mapping of the plugin
// result taxonomy onto the SDK envelope. Tests pin:
//
// - PDP `deny` short-circuits before the plugin is reached.
// - PDP transport failure (`unreachable`) fails closed before the plugin
//   is reached.
// - The plugin's `Ok(())` propagates to the SDK as `Ok(())`.
// - The plugin's `UsageRecordNotFound { id }` lifts to
//   `UsageCollectorError::NotFound { id }` (carrying the id).
// - The plugin's `UsageRecordAlreadyInactive { id }` lifts to
//   `UsageCollectorError::Conflict { id }` (carrying the id).
mod deactivate_usage_record_tests {
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use toolkit_gts::gts_id;

    use async_trait::async_trait;
    use toolkit::client_hub::{ClientHub, ClientScope};
    use toolkit_odata::{ODataQuery, Page as ODataPage};
    use toolkit_security::SecurityContext;
    use types_registry_sdk::TypesRegistryClient;
    use types_registry_sdk::testing::{MockTypesRegistryClient, make_test_instance};
    use usage_collector_sdk::{
        AggregationResult, AggregationSpec, ConflictReason, MetadataFilter, USAGE_RECORD_RESOURCE,
        UsageCollectorError, UsageCollectorPluginError, UsageCollectorPluginSpecV1,
        UsageCollectorPluginV1, UsageRecord, UsageType, UsageTypeGtsId,
    };
    use uuid::Uuid;

    use crate::domain::Service;
    use crate::domain::test_support::{
        CountingTenantPermitResolver, DenyAllResolver, UnreachableResolver, enforcer_for,
    };

    /// Programmable deactivate-stub plugin. Each `deactivate_usage_record`
    /// call drains one response from the queue (FIFO) and records the call
    /// count so tests can pin the exact plugin outcome under test AND verify
    /// the gateway dispatched (or did not dispatch) the SPI capability.
    /// All other SPI methods return a contract-violation — any accidental
    /// dispatch shows up as an obvious test failure, EXCEPT
    /// `get_usage_record` (Method 10), which the deactivation gateway
    /// pre-fetches before the PDP check; tests seed its response through
    /// [`DeactivateStubPlugin::with_get_record`].
    enum DeactivateResponse {
        Ok,
        Err(UsageCollectorPluginError),
    }

    /// Prefetch outcome for [`DeactivateStubPlugin::get_usage_record`]. The
    /// default `Found(_)` lets tests reach the deactivate-SPI call; tests
    /// that drive the `prefetch → NotFound` or `prefetch → plugin error`
    /// branches override via [`DeactivateStubPlugin::with_get_record`].
    enum GetRecordOutcome {
        Found(Box<UsageRecord>),
        NotFound,
        Transient,
    }

    struct DeactivateStubPlugin {
        deactivate_calls: AtomicUsize,
        last_id: Mutex<Option<Uuid>>,
        responses: Mutex<Vec<DeactivateResponse>>,
        get_record_outcome: Mutex<GetRecordOutcome>,
        get_record_calls: AtomicUsize,
    }

    impl DeactivateStubPlugin {
        fn new(responses: Vec<DeactivateResponse>) -> Arc<Self> {
            Arc::new(Self {
                deactivate_calls: AtomicUsize::new(0),
                last_id: Mutex::new(None),
                responses: Mutex::new(responses),
                get_record_outcome: Mutex::new(GetRecordOutcome::Found(Box::new(
                    sample_loaded_record(),
                ))),
                get_record_calls: AtomicUsize::new(0),
            })
        }

        fn with_get_record(self: &Arc<Self>, outcome: GetRecordOutcome) {
            *self.get_record_outcome.lock().expect("mutex not poisoned") = outcome;
        }

        fn deactivate_calls(&self) -> usize {
            self.deactivate_calls.load(Ordering::SeqCst)
        }

        #[allow(dead_code)]
        fn get_record_calls(&self) -> usize {
            self.get_record_calls.load(Ordering::SeqCst)
        }

        fn last_id(&self) -> Option<Uuid> {
            *self.last_id.lock().expect("mutex not poisoned")
        }
    }

    fn sample_loaded_record() -> UsageRecord {
        use time::OffsetDateTime;
        use usage_collector_sdk::{IdempotencyKey, ResourceRef, UsageRecordStatus, UsageTypeGtsId};
        UsageRecord {
            id: Uuid::from_u128(0xAAAA_AAAA),
            gts_id: UsageTypeGtsId::new(gts_id!(
                "cf.core.uc.usage_record.v1~cf.mini_chat._.tokens_consumed.v1"
            ))
            .expect("valid usage_record-derived gts_id"),
            tenant_id: Uuid::from_u128(2),
            resource_ref: ResourceRef::new("rsc-stub", "compute.vm").expect("valid resource ref"),
            subject_ref: None,
            metadata: std::collections::BTreeMap::new(),
            value: rust_decimal::Decimal::from(1),
            idempotency_key: IdempotencyKey::new("idem-stub").expect("valid idempotency key"),
            corrects_id: None,
            status: UsageRecordStatus::Active,
            created_at: OffsetDateTime::UNIX_EPOCH,
        }
    }

    #[async_trait]
    impl UsageCollectorPluginV1 for DeactivateStubPlugin {
        async fn deactivate_usage_record(&self, id: Uuid) -> Result<(), UsageCollectorPluginError> {
            self.deactivate_calls.fetch_add(1, Ordering::SeqCst);
            *self.last_id.lock().expect("mutex not poisoned") = Some(id);
            let mut q = self.responses.lock().expect("mutex not poisoned");
            if q.is_empty() {
                return Err(UsageCollectorPluginError::internal(
                    "test_fake: DeactivateStubPlugin: no programmed response remaining",
                ));
            }
            match q.remove(0) {
                DeactivateResponse::Ok => Ok(()),
                DeactivateResponse::Err(err) => Err(err),
            }
        }

        async fn create_usage_record(
            &self,
            _record: UsageRecord,
        ) -> Result<UsageRecord, UsageCollectorPluginError> {
            Err(UsageCollectorPluginError::internal(
                "test_fake: DeactivateStubPlugin: create_usage_record must not be called",
            ))
        }

        async fn create_usage_records(
            &self,
            _records: Vec<UsageRecord>,
        ) -> Result<Vec<Result<UsageRecord, UsageCollectorPluginError>>, UsageCollectorPluginError>
        {
            Err(UsageCollectorPluginError::internal(
                "test_fake: DeactivateStubPlugin: create_usage_records must not be called",
            ))
        }

        async fn query_aggregated_usage_records(
            &self,
            _gts_id: UsageTypeGtsId,
            _query: &ODataQuery,
            _metadata_filter: &[MetadataFilter],
            _aggregation: AggregationSpec,
        ) -> Result<AggregationResult, UsageCollectorPluginError> {
            Err(UsageCollectorPluginError::internal(
                "test_fake: DeactivateStubPlugin: query_aggregated_usage_records must not be called",
            ))
        }

        async fn list_usage_records(
            &self,
            _gts_id: UsageTypeGtsId,
            _query: &ODataQuery,
            _metadata_filter: &[MetadataFilter],
        ) -> Result<ODataPage<UsageRecord>, UsageCollectorPluginError> {
            Err(UsageCollectorPluginError::internal(
                "test_fake: DeactivateStubPlugin: list_usage_records must not be called",
            ))
        }

        async fn create_usage_type(
            &self,
            _usage_type: UsageType,
        ) -> Result<UsageType, UsageCollectorPluginError> {
            Err(UsageCollectorPluginError::internal(
                "test_fake: DeactivateStubPlugin: create_usage_type must not be called",
            ))
        }

        async fn get_usage_type(
            &self,
            _gts_id: UsageTypeGtsId,
        ) -> Result<UsageType, UsageCollectorPluginError> {
            Err(UsageCollectorPluginError::internal(
                "test_fake: DeactivateStubPlugin: get_usage_type must not be called",
            ))
        }

        async fn list_usage_types(
            &self,
            _query: &ODataQuery,
        ) -> Result<ODataPage<UsageType>, UsageCollectorPluginError> {
            Err(UsageCollectorPluginError::internal(
                "test_fake: DeactivateStubPlugin: list_usage_types must not be called",
            ))
        }

        async fn delete_usage_type(
            &self,
            _gts_id: UsageTypeGtsId,
        ) -> Result<(), UsageCollectorPluginError> {
            Err(UsageCollectorPluginError::internal(
                "test_fake: DeactivateStubPlugin: delete_usage_type must not be called",
            ))
        }

        async fn get_usage_record(
            &self,
            id: Uuid,
        ) -> Result<UsageRecord, UsageCollectorPluginError> {
            self.get_record_calls.fetch_add(1, Ordering::SeqCst);
            let outcome = self.get_record_outcome.lock().expect("mutex not poisoned");
            match &*outcome {
                GetRecordOutcome::Found(record) => Ok((**record).clone()),
                GetRecordOutcome::NotFound => {
                    Err(UsageCollectorPluginError::UsageRecordNotFound { id })
                }
                GetRecordOutcome::Transient => Err(UsageCollectorPluginError::transient(
                    "test_fake: DeactivateStubPlugin: prefetch transient",
                )),
            }
        }
    }

    fn test_instance_id_for(suffix: &str) -> String {
        format!("{}{suffix}", UsageCollectorPluginSpecV1::gts_type_id())
    }

    fn plugin_content(gts_id: &str, vendor: &str) -> serde_json::Value {
        serde_json::json!({
            "id": gts_id,
            "vendor": vendor,
            "priority": 0,
            "properties": {}
        })
    }

    fn hub_with(plugin: Arc<dyn UsageCollectorPluginV1>, suffix: &str) -> Arc<ClientHub> {
        let hub = Arc::new(ClientHub::default());
        let instance_id = test_instance_id_for(suffix);
        let instance = make_test_instance(
            &instance_id,
            plugin_content(&instance_id, "constructorfabric"),
        );
        let registry: Arc<dyn TypesRegistryClient> =
            Arc::new(MockTypesRegistryClient::new().with_instances([instance]));
        hub.register::<dyn TypesRegistryClient>(registry);
        hub.register_scoped::<dyn UsageCollectorPluginV1>(
            ClientScope::gts_id(&instance_id),
            plugin,
        );
        hub
    }

    fn authenticated_ctx() -> SecurityContext {
        SecurityContext::builder()
            .subject_id(Uuid::from_u128(1))
            .subject_tenant_id(Uuid::from_u128(2))
            .subject_type("user")
            .build()
            .expect("authenticated context")
    }

    fn service_with_permit(plugin: Arc<dyn UsageCollectorPluginV1>, suffix: &str) -> Arc<Service> {
        let hub = hub_with(plugin, suffix);
        let enforcer = enforcer_for(CountingTenantPermitResolver::new());
        Arc::new(Service::new(hub, "constructorfabric".into(), enforcer))
    }

    fn service_with_deny(plugin: Arc<dyn UsageCollectorPluginV1>, suffix: &str) -> Arc<Service> {
        let hub = hub_with(plugin, suffix);
        let enforcer = enforcer_for(Arc::new(DenyAllResolver));
        Arc::new(Service::new(hub, "constructorfabric".into(), enforcer))
    }

    fn service_with_unreachable_pdp(
        plugin: Arc<dyn UsageCollectorPluginV1>,
        suffix: &str,
    ) -> Arc<Service> {
        let hub = hub_with(plugin, suffix);
        let enforcer = enforcer_for(Arc::new(UnreachableResolver));
        Arc::new(Service::new(hub, "constructorfabric".into(), enforcer))
    }

    // ── Happy path: plugin `Ok(())` propagates ────────────────────────

    #[tokio::test]
    async fn deactivate_usage_record_propagates_plugin_ok_to_caller() {
        let plugin = DeactivateStubPlugin::new(vec![DeactivateResponse::Ok]);
        let svc = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.usage_collector.deactivate.ok.v1",
        );
        let id = Uuid::from_u128(0x00C0_FFEE);

        svc.deactivate_usage_record(&authenticated_ctx(), id)
            .await
            .expect("plugin Ok(()) MUST propagate as SDK Ok(())");

        assert_eq!(
            plugin.deactivate_calls(),
            1,
            "the SPI capability MUST be invoked exactly once on the success path",
        );
        assert_eq!(
            plugin.last_id(),
            Some(id),
            "the gateway MUST forward the target id verbatim to the plugin",
        );
    }

    // ── PDP deny collapses to NotFound BEFORE plugin dispatch ─────────

    #[tokio::test]
    async fn deactivate_usage_record_pdp_deny_collapses_to_not_found_before_plugin() {
        let plugin = DeactivateStubPlugin::new(vec![]);
        let svc = service_with_deny(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.usage_collector.deactivate.deny.v1",
        );

        let err = svc
            .deactivate_usage_record(&authenticated_ctx(), Uuid::from_u128(0xFEED))
            .await
            .expect_err("PDP deny MUST surface as NotFound");

        // Denial collapses to NotFound (existence-oracle guard), never PermissionDenied.
        assert!(
            matches!(err, UsageCollectorError::NotFound { .. }),
            "expected NotFound, got {err:?}",
        );
        assert_eq!(
            plugin.deactivate_calls(),
            0,
            "PDP deny MUST short-circuit BEFORE any Plugin SPI dispatch",
        );
    }

    // ── PDP transport failure fails closed BEFORE plugin dispatch ─────

    #[tokio::test]
    async fn deactivate_usage_record_pdp_unreachable_fails_closed_before_plugin() {
        let plugin = DeactivateStubPlugin::new(vec![]);
        let svc = service_with_unreachable_pdp(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.usage_collector.deactivate.unreachable.v1",
        );

        let err = svc
            .deactivate_usage_record(&authenticated_ctx(), Uuid::from_u128(0xFACE))
            .await
            .expect_err("unreachable PDP transport MUST fail closed");

        assert!(
            matches!(err, UsageCollectorError::ServiceUnavailable { .. }),
            "expected ServiceUnavailable (PDP transport failure), got {err:?}",
        );
        assert_eq!(
            plugin.deactivate_calls(),
            0,
            "PDP transport failure MUST fail closed BEFORE any Plugin SPI dispatch",
        );
    }

    // ── Plugin `UsageRecordNotFound { id }` → SDK `NotFound(id)` ───────

    #[tokio::test]
    async fn deactivate_usage_record_plugin_not_found_lifts_to_sdk_not_found() {
        let id = Uuid::from_u128(0xDEAD_BEEF);
        let plugin = DeactivateStubPlugin::new(vec![DeactivateResponse::Err(
            UsageCollectorPluginError::UsageRecordNotFound { id },
        )]);
        let svc = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.usage_collector.deactivate.notfound.v1",
        );

        let err = svc
            .deactivate_usage_record(&authenticated_ctx(), id)
            .await
            .expect_err("unknown id MUST surface as NotFound");

        assert!(
            matches!(
                err,
                UsageCollectorError::NotFound { ref resource_type, ref name, .. }
                    if resource_type == USAGE_RECORD_RESOURCE && name == &id.to_string()
            ),
            "expected NotFound carrying the target id, got {err:?}",
        );
        assert_eq!(plugin.deactivate_calls(), 1);
    }

    // ── Plugin `UsageRecordAlreadyInactive { id }` →
    //     SDK `AlreadyInactive { id }` ──────────────────────────────────

    #[tokio::test]
    async fn deactivate_usage_record_plugin_already_inactive_lifts_to_sdk_already_inactive() {
        // `cpt-cf-usage-collector-dod-event-deactivation-entity-deactivation-status`:
        // a second deactivation against an already-inactive record MUST surface
        // the actionable `AlreadyInactive` SDK variant. The canonical lift then
        // emits HTTP 409 with `context.reason="ALREADY_INACTIVE"`.
        let id = Uuid::from_u128(0xCAFE_BABE);
        let plugin = DeactivateStubPlugin::new(vec![DeactivateResponse::Err(
            UsageCollectorPluginError::UsageRecordAlreadyInactive { id },
        )]);
        let svc = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.usage_collector.deactivate.already_inactive.v1",
        );

        let err = svc
            .deactivate_usage_record(&authenticated_ctx(), id)
            .await
            .expect_err("already-inactive target MUST surface as AlreadyInactive");

        assert!(
            matches!(
                err,
                UsageCollectorError::Conflict {
                    reason: ConflictReason::AlreadyInactive,
                    ref name,
                    ..
                } if name == &id.to_string()
            ),
            "expected Conflict(AlreadyInactive) carrying the target id, got {err:?}",
        );
        assert_eq!(plugin.deactivate_calls(), 1);
    }

    // ── Plugin transport / readiness fault → 503 envelope ─────────────

    #[tokio::test]
    async fn deactivate_usage_record_plugin_transient_lifts_to_service_unavailable_envelope() {
        let plugin = DeactivateStubPlugin::new(vec![DeactivateResponse::Err(
            UsageCollectorPluginError::transient("downstream connection reset"),
        )]);
        let svc = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.usage_collector.deactivate.transient.v1",
        );

        let err = svc
            .deactivate_usage_record(&authenticated_ctx(), Uuid::from_u128(0x01))
            .await
            .expect_err("plugin Transient MUST lift to ServiceUnavailable");

        match &err {
            UsageCollectorError::ServiceUnavailable { detail, .. } => {
                assert_eq!(detail, "downstream connection reset");
            }
            other => panic!("expected ServiceUnavailable, got {other:?}"),
        }
        assert!(err.is_retryable());
    }

    // ── Prefetch returns Err(UsageRecordNotFound) → NotFound, never reaches
    //     PDP or Method 5 dispatch (the host-side pre-PDP existence check) ──

    #[tokio::test]
    async fn deactivate_usage_record_prefetch_not_found_skips_pdp_and_spi() {
        // `cpt-cf-usage-collector-flow-event-deactivation-deactivate-record`
        // step `inst-deactivate-record-prefetch-not-found`: a missing target
        // surfaces as `NotFound(id)` BEFORE the PDP authz call AND BEFORE
        // the SPI Method 5 dispatch. The PDP is deny-all to prove the
        // prefetch-not-found path short-circuits past it.
        let id = Uuid::from_u128(0xDEAD_F00D);
        let plugin = DeactivateStubPlugin::new(vec![]);
        plugin.with_get_record(GetRecordOutcome::NotFound);
        let svc = service_with_deny(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.usage_collector.deactivate.prefetch_not_found.v1",
        );

        let err = svc
            .deactivate_usage_record(&authenticated_ctx(), id)
            .await
            .expect_err("prefetch UsageRecordNotFound MUST surface as NotFound");

        assert!(
            matches!(
                err,
                UsageCollectorError::NotFound { ref resource_type, ref name, .. }
                    if resource_type == USAGE_RECORD_RESOURCE && name == &id.to_string()
            ),
            "expected NotFound carrying the target id, got {err:?}",
        );
        assert_eq!(
            plugin.deactivate_calls(),
            0,
            "prefetch-not-found MUST short-circuit BEFORE any Method 5 dispatch",
        );
    }

    // ── Prefetch surfaces a plugin transport fault → propagates via the
    //     From-impl chain (host does NOT swallow the variant) ────────────

    #[tokio::test]
    async fn deactivate_usage_record_prefetch_transient_propagates() {
        // A storage fault during prefetch propagates verbatim through the
        // From-impl chain; the deactivate handler MUST NOT silently retry
        // and MUST NOT dispatch the deactivate SPI call.
        let plugin = DeactivateStubPlugin::new(vec![]);
        plugin.with_get_record(GetRecordOutcome::Transient);
        let svc = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.usage_collector.deactivate.prefetch_transient.v1",
        );

        let err = svc
            .deactivate_usage_record(&authenticated_ctx(), Uuid::from_u128(0xBEEF))
            .await
            .expect_err("prefetch Transient MUST lift to ServiceUnavailable");

        assert!(
            matches!(err, UsageCollectorError::ServiceUnavailable { .. }),
            "expected ServiceUnavailable from prefetch, got {err:?}",
        );
        assert_eq!(
            plugin.deactivate_calls(),
            0,
            "prefetch fault MUST short-circuit BEFORE deactivate SPI dispatch",
        );
    }
}

// ── PDP dedup pre-pass in `create_usage_records` ───────────────────────────
//
// Pins the intra-batch dedup behavior described in
// `cpt-cf-usage-collector-algo-usage-emission-attribution-and-pdp-authorization`
// instructions `inst-algo-attrib-dedup-tuple-key` and
// `inst-algo-attrib-bounded-fanout`: records sharing the same attribution
// tuple `(tenant_id, resource_type, resource_id, subject_id, subject_type)`
// MUST collapse to a single PDP `evaluate` round-trip, projected onto every
// input index in the group.
#[cfg(test)]
mod pdp_dedup_tests {
    use std::collections::BTreeMap;
    use std::collections::BTreeSet;
    use std::sync::Arc;
    use toolkit_gts::gts_id;

    use time::OffsetDateTime;
    use usage_collector_sdk::{
        CreateUsageRecord, IdempotencyKey, ResourceRef, SubjectRef, UsageCollectorPluginV1,
        UsageKind, UsageRecord, UsageRecordStatus, UsageType, UsageTypeGtsId,
    };
    use uuid::Uuid;

    use crate::domain::test_support::{
        HappyPathPlugin, authenticated_ctx, permit_scoped_to_request_tenant,
        service_with_counting_permit,
    };

    const HAPPY_GTS_ID: &str =
        gts_id!("cf.core.uc.usage_record.v1~cf.mini_chat._.tokens_consumed.v1");

    fn happy_usage_type() -> UsageType {
        UsageType {
            gts_id: UsageTypeGtsId::new(HAPPY_GTS_ID).expect("valid gts_id"),
            kind: UsageKind::Counter,
            metadata_fields: BTreeSet::new(),
        }
    }

    fn persisted_record(tenant_id: Uuid, resource_id: &str, idem: &str) -> UsageRecord {
        UsageRecord {
            id: Uuid::new_v4(),
            gts_id: UsageTypeGtsId::new(HAPPY_GTS_ID).expect("valid gts_id"),
            tenant_id,
            resource_ref: ResourceRef::new(resource_id, "compute.vm").expect("valid resource ref"),
            subject_ref: None,
            metadata: BTreeMap::new(),
            value: rust_decimal::Decimal::from(1),
            idempotency_key: IdempotencyKey::new(idem).expect("valid idempotency key"),
            corrects_id: None,
            status: UsageRecordStatus::Active,
            created_at: OffsetDateTime::UNIX_EPOCH,
        }
    }

    fn input_record(tenant_id: Uuid, resource_id: &str, idem: &str) -> CreateUsageRecord {
        // Distinct `idem` values keep records distinct even when they share an
        // attribution tuple; the create surface is identity-free (the id is
        // derived from the dedup key inside the service).
        CreateUsageRecord {
            gts_id: UsageTypeGtsId::new(HAPPY_GTS_ID).expect("valid gts_id"),
            tenant_id,
            resource_ref: ResourceRef::new(resource_id, "compute.vm").expect("valid resource ref"),
            subject_ref: None,
            metadata: BTreeMap::new(),
            value: rust_decimal::Decimal::from(1),
            idempotency_key: IdempotencyKey::new(idem).expect("valid idempotency key"),
            corrects_id: None,
            created_at: OffsetDateTime::UNIX_EPOCH,
        }
    }

    fn input_record_with_subject(
        tenant_id: Uuid,
        resource_id: &str,
        subject_id: &str,
        idem: &str,
    ) -> CreateUsageRecord {
        let mut r = input_record(tenant_id, resource_id, idem);
        r.subject_ref =
            Some(SubjectRef::new(subject_id, None::<String>).expect("valid subject ref"));
        r
    }

    /// Five records, identical attribution tuple → exactly one PDP
    /// `evaluate` round-trip. This pins
    /// `inst-algo-attrib-dedup-tuple-key`.
    #[tokio::test]
    async fn create_usage_records_collapses_pdp_calls_for_identical_attribution_tuple() {
        let plugin = HappyPathPlugin::new();
        plugin.set_get_usage_type(happy_usage_type());

        let tenant_id = Uuid::from_u128(0xAA);
        let input: Vec<CreateUsageRecord> = (0..5)
            .map(|i| input_record(tenant_id, "rsc-shared", &format!("idem-shared-{i}")))
            .collect();

        plugin.set_create_records(
            input
                .iter()
                .map(|r| Ok(persisted_record(r.tenant_id, "rsc-shared", "idem-persist")))
                .collect(),
        );

        let (service, resolver) = service_with_counting_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.pdp_dedup.shared_tuple.records.v1",
        );

        let results = service
            .create_usage_records(&authenticated_ctx(), input)
            .await
            .expect("batch dispatch succeeded");
        assert_eq!(results.len(), 5);
        assert!(
            results.iter().all(Result::is_ok),
            "every record MUST be accepted under a permit-by-default PDP",
        );

        assert_eq!(
            resolver.calls(),
            1,
            "5 records sharing the same attribution tuple MUST collapse to a single PDP evaluate call (intra-batch dedup); observed {} calls",
            resolver.calls(),
        );
    }

    /// Three records, three distinct `resource_id` values → exactly three
    /// PDP calls (each tuple key is unique, no dedup possible). Pins the
    /// "one call per distinct tuple" contract from `inst-algo-attrib-bounded-fanout`.
    #[tokio::test]
    async fn create_usage_records_issues_one_pdp_call_per_distinct_attribution_tuple() {
        let plugin = HappyPathPlugin::new();
        plugin.set_get_usage_type(happy_usage_type());

        let tenant_id = Uuid::from_u128(0xBB);
        let input = vec![
            input_record(tenant_id, "rsc-A", "idem-A"),
            input_record(tenant_id, "rsc-B", "idem-B"),
            input_record(tenant_id, "rsc-C", "idem-C"),
        ];

        plugin.set_create_records(
            input
                .iter()
                .map(|r| {
                    Ok(persisted_record(
                        r.tenant_id,
                        r.resource_ref.resource_id(),
                        r.idempotency_key.as_str(),
                    ))
                })
                .collect(),
        );

        let (service, resolver) = service_with_counting_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.pdp_dedup.distinct_tuples.records.v1",
        );

        let results = service
            .create_usage_records(&authenticated_ctx(), input)
            .await
            .expect("batch dispatch succeeded");
        assert_eq!(results.len(), 3);
        assert!(results.iter().all(Result::is_ok));

        assert_eq!(
            resolver.calls(),
            3,
            "3 distinct attribution tuples MUST produce 3 PDP evaluate calls; observed {} calls",
            resolver.calls(),
        );
    }

    /// Mixed batch (two records share tuple A, two share tuple B) → exactly
    /// two PDP calls. Pins the projection step: the second call's decision
    /// MUST apply to every input index whose tuple key matches.
    #[tokio::test]
    async fn create_usage_records_projects_pdp_decision_across_shared_tuple_groups() {
        let plugin = HappyPathPlugin::new();
        plugin.set_get_usage_type(happy_usage_type());

        let tenant_id = Uuid::from_u128(0xCC);
        let input = vec![
            input_record(tenant_id, "rsc-A", "idem-A-0"),
            input_record(tenant_id, "rsc-B", "idem-B-0"),
            input_record(tenant_id, "rsc-A", "idem-A-1"),
            input_record(tenant_id, "rsc-B", "idem-B-1"),
        ];

        plugin.set_create_records(
            input
                .iter()
                .map(|r| {
                    Ok(persisted_record(
                        r.tenant_id,
                        r.resource_ref.resource_id(),
                        r.idempotency_key.as_str(),
                    ))
                })
                .collect(),
        );

        let (service, resolver) = service_with_counting_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.pdp_dedup.mixed_groups.records.v1",
        );

        let results = service
            .create_usage_records(&authenticated_ctx(), input)
            .await
            .expect("batch dispatch succeeded");
        assert_eq!(results.len(), 4);
        assert!(results.iter().all(Result::is_ok));

        assert_eq!(
            resolver.calls(),
            2,
            "2 distinct attribution tuples carrying 4 records MUST produce 2 PDP evaluate calls; observed {} calls",
            resolver.calls(),
        );
    }

    /// Deny-side projection. When PDP DENIES one tuple group, every input
    /// index in that group MUST surface as a rejected record; every input
    /// index in a permitted tuple group MUST proceed. Pins the deny half
    /// of `inst-algo-attrib-dedup-tuple-key`, the sibling of
    /// `create_usage_records_projects_pdp_decision_across_shared_tuple_groups`
    /// for permits.
    #[tokio::test]
    async fn create_usage_records_projects_pdp_deny_across_shared_tuple_groups() {
        use async_trait::async_trait;
        use authz_resolver_sdk::AuthZResolverApi;
        use authz_resolver_sdk::models::{
            DenyReason, EvaluationRequest, EvaluationResponse, EvaluationResponseContext,
        };
        use toolkit::api::canonical_prelude::CanonicalError;

        use crate::domain::authz::usage_record::PROP_RESOURCE_ID;
        use crate::domain::service::Service;
        use crate::domain::test_support::{enforcer_for, hub_with_plugin};
        use toolkit_security::PlatformSecurityContext;

        /// Resolver that denies every evaluate request whose composed
        /// `resource_id` matches `deny_resource_id` and permits all others.
        /// The PEP composer at `domain/authz.rs` populates the request's
        /// resource properties with `PROP_RESOURCE_ID` from the attribution
        /// tuple, so this resolver discriminates per-tuple.
        struct DenyOneResourceResolver {
            deny_resource_id: String,
        }

        #[async_trait]
        impl AuthZResolverApi for DenyOneResourceResolver {
            async fn evaluate(
                &self,
                _ctx: PlatformSecurityContext,
                request: EvaluationRequest,
            ) -> Result<EvaluationResponse, CanonicalError> {
                let matches_deny = request
                    .resource
                    .properties
                    .get(PROP_RESOURCE_ID)
                    .and_then(serde_json::Value::as_str)
                    == Some(self.deny_resource_id.as_str());
                if matches_deny {
                    return Ok(EvaluationResponse {
                        decision: false,
                        context: EvaluationResponseContext {
                            constraints: Vec::new(),
                            deny_reason: Some(DenyReason {
                                error_code: "test-deny".to_owned(),
                                details: None,
                            }),
                        },
                    });
                }
                // Permit: scope the grant to the record's own tenant so the
                // per-record gate (`require_constraints(true)`) admits it,
                // rather than an empty-constraints permit that would now fail
                // closed as `CompileFailed`.
                Ok(permit_scoped_to_request_tenant(&request))
            }
        }

        let plugin = HappyPathPlugin::new();
        plugin.set_get_usage_type(happy_usage_type());

        let tenant_id = Uuid::from_u128(0xEE);
        // Two tuple groups: rsc-DENY (denied) and rsc-OK (permitted), two
        // records per group. The deny MUST project across both rsc-DENY
        // indices; the permit MUST project across both rsc-OK indices.
        let input = vec![
            input_record(tenant_id, "rsc-DENY", "idem-D-0"),
            input_record(tenant_id, "rsc-OK", "idem-K-0"),
            input_record(tenant_id, "rsc-DENY", "idem-D-1"),
            input_record(tenant_id, "rsc-OK", "idem-K-1"),
        ];

        // Only the two permitted records reach the plugin SPI; programme
        // exactly those persisted responses.
        plugin.set_create_records(
            input
                .iter()
                .filter(|r| r.resource_ref.resource_id() == "rsc-OK")
                .map(|r| {
                    Ok(persisted_record(
                        r.tenant_id,
                        r.resource_ref.resource_id(),
                        r.idempotency_key.as_str(),
                    ))
                })
                .collect(),
        );

        let hub = hub_with_plugin(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.pdp_dedup.deny_projection.records.v1",
            "constructorfabric",
        );
        let enforcer = enforcer_for(Arc::new(DenyOneResourceResolver {
            deny_resource_id: "rsc-DENY".to_owned(),
        }));
        let service = Arc::new(Service::new(hub, "constructorfabric".to_owned(), enforcer));

        let results = service
            .create_usage_records(&authenticated_ctx(), input)
            .await
            .expect("batch dispatch succeeded - per-record outcomes carry deny");

        assert_eq!(results.len(), 4);
        // Indices 0 and 2 carry rsc-DENY; both MUST surface
        // `UsageCollectorError::PermissionDenied` from the projected deny.
        for (idx, label) in [(0_usize, "rsc-DENY first"), (2, "rsc-DENY second")] {
            let err = results[idx]
                .as_ref()
                .expect_err(&format!("{label} record MUST surface a per-record deny"));
            assert!(
                matches!(
                    err,
                    usage_collector_sdk::UsageCollectorError::PermissionDenied { .. }
                ),
                "{label}: expected PermissionDenied, got {err:?}",
            );
        }
        // Indices 1 and 3 carry rsc-OK; both MUST be accepted.
        for (idx, label) in [(1_usize, "rsc-OK first"), (3, "rsc-OK second")] {
            results[idx]
                .as_ref()
                .unwrap_or_else(|err| panic!("{label} record MUST be accepted (got {err:?})"));
        }
    }

    /// Subject-presence asymmetry: a record WITH a `subject_ref` and a record
    /// WITHOUT one MUST be treated as distinct attribution tuples (`subject_id`
    /// is part of the tuple key only when present).
    #[tokio::test]
    async fn create_usage_records_distinguishes_subject_presence_in_tuple_key() {
        let plugin = HappyPathPlugin::new();
        plugin.set_get_usage_type(happy_usage_type());

        let tenant_id = Uuid::from_u128(0xDD);
        let input = vec![
            input_record(tenant_id, "rsc-X", "idem-X-0"),
            input_record_with_subject(tenant_id, "rsc-X", "subject-1", "idem-X-1"),
            input_record_with_subject(tenant_id, "rsc-X", "subject-1", "idem-X-2"),
        ];

        plugin.set_create_records(
            input
                .iter()
                .map(|r| {
                    Ok(persisted_record(
                        r.tenant_id,
                        r.resource_ref.resource_id(),
                        r.idempotency_key.as_str(),
                    ))
                })
                .collect(),
        );

        let (service, resolver) = service_with_counting_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.pdp_dedup.subject_presence.records.v1",
        );

        let results = service
            .create_usage_records(&authenticated_ctx(), input)
            .await
            .expect("batch dispatch succeeded");
        assert_eq!(results.len(), 3);
        assert!(results.iter().all(Result::is_ok));

        assert_eq!(
            resolver.calls(),
            2,
            "subject-absent and subject-present records form distinct tuple keys; observed {} calls",
            resolver.calls(),
        );
    }
}

// ── gts_id dedup pre-pass in `create_usage_records` ────────────────────────
//
// Pins the intra-batch catalog-lookup dedup behavior described in
// `cpt-cf-usage-collector-algo-usage-emission-catalog-existence-and-kind-lookup`
// instructions `inst-algo-catalog-dedup-gts-id` and
// `inst-algo-catalog-bounded-fanout`: records sharing the same
// `UsageType` `gts_id` MUST collapse to a single `get_usage_type` SPI
// round-trip, projected onto every input index referencing that id.
#[cfg(test)]
mod gts_id_dedup_tests {
    use std::collections::BTreeMap;
    use std::collections::BTreeSet;
    use std::sync::Arc;
    use toolkit_gts::gts_id;

    use time::OffsetDateTime;
    use usage_collector_sdk::{
        CreateUsageRecord, IdempotencyKey, ResourceRef, USAGE_TYPE_RESOURCE, UsageCollectorError,
        UsageCollectorPluginV1, UsageKind, UsageRecord, UsageRecordStatus, UsageType,
        UsageTypeGtsId,
    };
    use uuid::Uuid;

    use crate::domain::test_support::{HappyPathPlugin, authenticated_ctx, service_with_permit};

    const GTS_A: &str = gts_id!("cf.core.uc.usage_record.v1~cf.mini_chat._.tokens_consumed.v1");
    const GTS_B: &str = gts_id!("cf.core.uc.usage_record.v1~cf.mini_chat._.tokens_emitted.v1");
    const GTS_C: &str = gts_id!("cf.core.uc.usage_record.v1~cf.mini_chat._.tokens_buffered.v1");

    fn counter_usage_type() -> UsageType {
        UsageType {
            // The host validation only reads `kind` and `metadata_fields`; the
            // `gts_id` field is not compared against record.gts_id, so a
            // singular Counter UsageType stands in for every gts_id under test.
            gts_id: UsageTypeGtsId::new(GTS_A).expect("valid gts_id"),
            kind: UsageKind::Counter,
            metadata_fields: BTreeSet::new(),
        }
    }

    fn record_for(gts: &str, tenant_id: Uuid, idem: &str) -> CreateUsageRecord {
        CreateUsageRecord {
            gts_id: UsageTypeGtsId::new(gts).expect("valid gts_id"),
            tenant_id,
            resource_ref: ResourceRef::new("rsc-gts-dedup", "compute.vm")
                .expect("valid resource ref"),
            subject_ref: None,
            metadata: BTreeMap::new(),
            value: rust_decimal::Decimal::from(1),
            idempotency_key: IdempotencyKey::new(idem).expect("valid idempotency key"),
            corrects_id: None,
            created_at: OffsetDateTime::UNIX_EPOCH,
        }
    }

    fn persisted_for(input: &CreateUsageRecord) -> UsageRecord {
        UsageRecord {
            id: Uuid::new_v4(),
            gts_id: input.gts_id.clone(),
            tenant_id: input.tenant_id,
            resource_ref: input.resource_ref.clone(),
            subject_ref: input.subject_ref.clone(),
            metadata: input.metadata.clone(),
            value: input.value,
            idempotency_key: input.idempotency_key.clone(),
            corrects_id: input.corrects_id,
            status: UsageRecordStatus::Active,
            created_at: input.created_at,
        }
    }

    /// Five records, identical `gts_id` → exactly one `get_usage_type`
    /// SPI dispatch (intra-batch dedup of catalog lookups).
    #[tokio::test]
    async fn create_usage_records_collapses_get_usage_type_calls_for_same_gts_id() {
        let plugin = HappyPathPlugin::new();
        plugin.set_get_usage_type(counter_usage_type());

        let tenant_id = Uuid::from_u128(0xEE);
        let input: Vec<CreateUsageRecord> = (0..5)
            .map(|i| record_for(GTS_A, tenant_id, &format!("idem-gts-{i}")))
            .collect();

        plugin.set_create_records(input.iter().map(|r| Ok(persisted_for(r))).collect());

        let service = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.gts_dedup.shared.records.v1",
        );

        let results = service
            .create_usage_records(&authenticated_ctx(), input)
            .await
            .expect("batch dispatch succeeded");
        assert_eq!(results.len(), 5);
        assert!(
            results.iter().all(Result::is_ok),
            "every record MUST be accepted under a permit-by-default PDP",
        );

        assert_eq!(
            plugin.get_usage_type_calls(),
            1,
            "5 records sharing one gts_id MUST collapse to a single get_usage_type SPI \
             dispatch (intra-batch catalog dedup); observed {} calls",
            plugin.get_usage_type_calls(),
        );
    }

    /// Three records, three distinct `gts_id`s → exactly three
    /// `get_usage_type` dispatches (one per distinct id).
    #[tokio::test]
    async fn create_usage_records_issues_one_get_usage_type_call_per_distinct_gts_id() {
        let plugin = HappyPathPlugin::new();
        plugin.set_get_usage_type(counter_usage_type());

        let tenant_id = Uuid::from_u128(0xFF);
        let input = vec![
            record_for(GTS_A, tenant_id, "idem-A"),
            record_for(GTS_B, tenant_id, "idem-B"),
            record_for(GTS_C, tenant_id, "idem-C"),
        ];

        plugin.set_create_records(input.iter().map(|r| Ok(persisted_for(r))).collect());

        let service = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.gts_dedup.distinct.records.v1",
        );

        let results = service
            .create_usage_records(&authenticated_ctx(), input)
            .await
            .expect("batch dispatch succeeded");
        assert_eq!(results.len(), 3);
        assert!(results.iter().all(Result::is_ok));

        assert_eq!(
            plugin.get_usage_type_calls(),
            3,
            "3 distinct gts_ids MUST produce 3 get_usage_type SPI dispatches; \
             observed {} calls",
            plugin.get_usage_type_calls(),
        );

        let mut seen: Vec<String> = plugin
            .get_usage_type_inputs()
            .into_iter()
            .map(|id| id.to_string())
            .collect();
        seen.sort();
        let mut expected = vec![GTS_A.to_owned(), GTS_B.to_owned(), GTS_C.to_owned()];
        expected.sort();
        assert_eq!(
            seen, expected,
            "the deduped fan-out MUST ask the plugin for exactly the distinct gts_ids",
        );
    }

    /// Mixed batch: `gts_id` A is known, `gts_id` B is not-found. Two
    /// distinct ids → 2 SPI dispatches; records sharing the unknown id
    /// are all rejected with `UsageTypeNotFound`, records sharing the
    /// known id are accepted.
    #[tokio::test]
    async fn create_usage_records_projects_not_found_to_every_record_sharing_unknown_gts_id() {
        let plugin = HappyPathPlugin::new();
        plugin.set_get_usage_type(counter_usage_type());
        plugin.set_get_usage_type_not_found(UsageTypeGtsId::new(GTS_B).expect("valid gts_id"));

        let tenant_id = Uuid::from_u128(0x11);
        let input = vec![
            record_for(GTS_A, tenant_id, "idem-A-0"),
            record_for(GTS_B, tenant_id, "idem-B-0"),
            record_for(GTS_A, tenant_id, "idem-A-1"),
            record_for(GTS_B, tenant_id, "idem-B-1"),
        ];

        // Only the two known-gts_id records reach the SPI; program two
        // accepted-persist responses.
        let accepted: Vec<_> = input
            .iter()
            .filter(|r| r.gts_id.to_string() == GTS_A)
            .map(|r| Ok(persisted_for(r)))
            .collect();
        plugin.set_create_records(accepted);

        let service = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.gts_dedup.mixed.records.v1",
        );

        let results = service
            .create_usage_records(&authenticated_ctx(), input)
            .await
            .expect("batch dispatch succeeded");
        assert_eq!(results.len(), 4);

        assert!(
            results[0].is_ok(),
            "record at index 0 (gts_id A) MUST be accepted, got {:?}",
            results[0],
        );
        assert!(
            results[2].is_ok(),
            "record at index 2 (gts_id A) MUST be accepted, got {:?}",
            results[2],
        );
        for (idx, expected_id) in [(1usize, GTS_B), (3usize, GTS_B)] {
            match results[idx].as_ref() {
                Err(UsageCollectorError::NotFound {
                    resource_type,
                    name,
                    ..
                }) => {
                    assert_eq!(resource_type, USAGE_TYPE_RESOURCE);
                    assert_eq!(
                        name, expected_id,
                        "record at index {idx} MUST be rejected with NotFound \
                         carrying the unknown gts_id",
                    );
                }
                other => panic!(
                    "record at index {idx} (gts_id B) MUST surface NotFound, \
                     got {other:?}",
                ),
            }
        }

        assert_eq!(
            plugin.get_usage_type_calls(),
            2,
            "2 distinct gts_ids carrying 4 records MUST produce 2 get_usage_type \
             dispatches; observed {} calls",
            plugin.get_usage_type_calls(),
        );
    }
}

// ── corrects_id L1 dedup pre-pass in `create_usage_records` ────────────────
//
// Pins the intra-batch L1-lookup dedup behavior described in
// `cpt-cf-usage-collector-algo-usage-emission-semantics-enforcement-on-ingest-v2`
// instructions `inst-algo-semantics-l1-dedup` and
// `inst-algo-semantics-l1-bounded-fanout`: records sharing the same
// `corrects_id` MUST collapse to a single `get_usage_record` SPI
// round-trip, and ordinary (non-compensation) records MUST NOT trigger
// any L1 lookup at all.
#[cfg(test)]
mod corrects_id_dedup_tests {
    use std::collections::BTreeMap;
    use std::collections::BTreeSet;
    use std::sync::Arc;
    use toolkit_gts::gts_id;

    use time::OffsetDateTime;
    use usage_collector_sdk::{
        CreateUsageRecord, IdempotencyKey, ResourceRef, USAGE_RECORD_RESOURCE, UsageCollectorError,
        UsageCollectorPluginV1, UsageKind, UsageRecord, UsageRecordStatus, UsageType,
        UsageTypeGtsId,
    };
    use uuid::Uuid;

    use crate::domain::test_support::{HappyPathPlugin, authenticated_ctx, service_with_permit};

    const COUNTER_GTS_ID: &str =
        gts_id!("cf.core.uc.usage_record.v1~cf.mini_chat._.tokens_consumed.v1");

    fn counter_usage_type() -> UsageType {
        UsageType {
            gts_id: UsageTypeGtsId::new(COUNTER_GTS_ID).expect("valid gts_id"),
            kind: UsageKind::Counter,
            metadata_fields: BTreeSet::new(),
        }
    }

    fn referenced_original(tenant_id: Uuid) -> UsageRecord {
        // The L1 verifier checks (corrects_id IS NULL, identity-tuple match,
        // status=Active) against this row, so the compensation records under
        // test must mirror its (tenant, gts_id, resource_ref, subject_ref)
        // shape. `set_get_record` returns this same row for any id the
        // host looks up — that's fine because verify_l1_corrects_id reads
        // identity fields, not id.
        UsageRecord {
            id: Uuid::from_u128(0xDEAD_BEEF),
            gts_id: UsageTypeGtsId::new(COUNTER_GTS_ID).expect("valid gts_id"),
            tenant_id,
            resource_ref: ResourceRef::new("rsc-comp", "compute.vm").expect("valid resource ref"),
            subject_ref: None,
            metadata: BTreeMap::new(),
            value: rust_decimal::Decimal::from(10),
            idempotency_key: IdempotencyKey::new("idem-original").expect("valid idempotency key"),
            corrects_id: None,
            status: UsageRecordStatus::Active,
            created_at: OffsetDateTime::UNIX_EPOCH,
        }
    }

    fn compensation_for(tenant_id: Uuid, corrects_id: Uuid, idem: &str) -> CreateUsageRecord {
        CreateUsageRecord {
            gts_id: UsageTypeGtsId::new(COUNTER_GTS_ID).expect("valid gts_id"),
            tenant_id,
            resource_ref: ResourceRef::new("rsc-comp", "compute.vm").expect("valid resource ref"),
            subject_ref: None,
            metadata: BTreeMap::new(),
            value: rust_decimal::Decimal::from(-1),
            idempotency_key: IdempotencyKey::new(idem).expect("valid idempotency key"),
            corrects_id: Some(corrects_id),
            created_at: OffsetDateTime::UNIX_EPOCH,
        }
    }

    fn ordinary_record(tenant_id: Uuid, idem: &str) -> CreateUsageRecord {
        CreateUsageRecord {
            gts_id: UsageTypeGtsId::new(COUNTER_GTS_ID).expect("valid gts_id"),
            tenant_id,
            resource_ref: ResourceRef::new("rsc-comp", "compute.vm").expect("valid resource ref"),
            subject_ref: None,
            metadata: BTreeMap::new(),
            value: rust_decimal::Decimal::from(1),
            idempotency_key: IdempotencyKey::new(idem).expect("valid idempotency key"),
            corrects_id: None,
            created_at: OffsetDateTime::UNIX_EPOCH,
        }
    }

    // The plugin echo just needs a valid persisted `UsageRecord`; the create
    // input projects to one via the same derivation the service applies.
    fn persisted_echo(record: &CreateUsageRecord) -> UsageRecord {
        record.clone().into_usage_record()
    }

    /// Five compensations sharing one `corrects_id` MUST collapse to a
    /// single `get_usage_record` SPI round-trip.
    #[tokio::test]
    async fn create_usage_records_collapses_get_usage_record_calls_for_shared_corrects_id() {
        let plugin = HappyPathPlugin::new();
        plugin.set_get_usage_type(counter_usage_type());

        let tenant_id = Uuid::from_u128(0x501);
        plugin.set_get_record(referenced_original(tenant_id));

        let corrects_id = Uuid::from_u128(0x601);
        let input: Vec<CreateUsageRecord> = (0..5)
            .map(|i| compensation_for(tenant_id, corrects_id, &format!("idem-comp-{i}")))
            .collect();

        plugin.set_create_records(input.iter().map(|r| Ok(persisted_echo(r))).collect());

        let service = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.l1_dedup.shared.records.v1",
        );

        let results = service
            .create_usage_records(&authenticated_ctx(), input)
            .await
            .expect("batch dispatch succeeded");
        assert_eq!(results.len(), 5);
        assert!(
            results.iter().all(Result::is_ok),
            "every compensation MUST be accepted: {results:?}",
        );

        assert_eq!(
            plugin.get_usage_record_calls(),
            1,
            "5 compensations sharing one corrects_id MUST collapse to a single \
             get_usage_record SPI dispatch (intra-batch L1 dedup); observed {} calls",
            plugin.get_usage_record_calls(),
        );
    }

    /// Three distinct `corrects_id`s MUST produce three `get_usage_record`
    /// dispatches.
    #[tokio::test]
    async fn create_usage_records_issues_one_get_usage_record_call_per_distinct_corrects_id() {
        let plugin = HappyPathPlugin::new();
        plugin.set_get_usage_type(counter_usage_type());

        let tenant_id = Uuid::from_u128(0x502);
        plugin.set_get_record(referenced_original(tenant_id));

        let corrects_id_a = Uuid::from_u128(0x602);
        let corrects_id_b = Uuid::from_u128(0x603);
        let corrects_id_c = Uuid::from_u128(0x604);

        let input = vec![
            compensation_for(tenant_id, corrects_id_a, "idem-A"),
            compensation_for(tenant_id, corrects_id_b, "idem-B"),
            compensation_for(tenant_id, corrects_id_c, "idem-C"),
        ];

        plugin.set_create_records(input.iter().map(|r| Ok(persisted_echo(r))).collect());

        let service = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.l1_dedup.distinct.records.v1",
        );

        let results = service
            .create_usage_records(&authenticated_ctx(), input)
            .await
            .expect("batch dispatch succeeded");
        assert_eq!(results.len(), 3);
        assert!(results.iter().all(Result::is_ok));

        assert_eq!(
            plugin.get_usage_record_calls(),
            3,
            "3 distinct corrects_ids MUST produce 3 get_usage_record SPI dispatches; \
             observed {} calls",
            plugin.get_usage_record_calls(),
        );

        let mut seen = plugin.get_usage_record_inputs();
        seen.sort();
        let mut expected = vec![corrects_id_a, corrects_id_b, corrects_id_c];
        expected.sort();
        assert_eq!(
            seen, expected,
            "the deduped fan-out MUST ask the plugin for exactly the distinct corrects_ids",
        );
    }

    /// Ordinary records (no `corrects_id`) MUST NOT trigger any L1 lookup.
    #[tokio::test]
    async fn create_usage_records_skips_l1_lookup_when_no_record_has_corrects_id() {
        let plugin = HappyPathPlugin::new();
        plugin.set_get_usage_type(counter_usage_type());

        let tenant_id = Uuid::from_u128(0x503);
        let input: Vec<CreateUsageRecord> = (0..5)
            .map(|i| ordinary_record(tenant_id, &format!("idem-ord-{i}")))
            .collect();

        plugin.set_create_records(input.iter().map(|r| Ok(persisted_echo(r))).collect());

        let service = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.l1_dedup.no_corrects.records.v1",
        );

        let results = service
            .create_usage_records(&authenticated_ctx(), input)
            .await
            .expect("batch dispatch succeeded");
        assert_eq!(results.len(), 5);
        assert!(results.iter().all(Result::is_ok));

        assert_eq!(
            plugin.get_usage_record_calls(),
            0,
            "ordinary records (corrects_id IS NULL) MUST NOT trigger L1 lookups; \
             observed {} get_usage_record dispatches",
            plugin.get_usage_record_calls(),
        );
    }

    /// L1 not-found for one `corrects_id` MUST project to every record
    /// sharing it (rejected as `CorrectsIdNotFound`), while records
    /// referencing a different known `corrects_id` are still accepted.
    #[tokio::test]
    async fn create_usage_records_projects_l1_not_found_to_every_record_sharing_unknown_corrects_id()
     {
        let plugin = HappyPathPlugin::new();
        plugin.set_get_usage_type(counter_usage_type());

        let tenant_id = Uuid::from_u128(0x504);
        plugin.set_get_record(referenced_original(tenant_id));

        let corrects_id_good = Uuid::from_u128(0x605);
        let corrects_id_bad = Uuid::from_u128(0x606);
        plugin.set_get_usage_record_not_found(corrects_id_bad);

        let input = vec![
            compensation_for(tenant_id, corrects_id_bad, "idem-bad-0"),
            compensation_for(tenant_id, corrects_id_good, "idem-good-0"),
            compensation_for(tenant_id, corrects_id_bad, "idem-bad-1"),
        ];

        // Only the known-good record reaches the persist SPI; program one
        // accepted response.
        plugin.set_create_records(vec![Ok(persisted_echo(&input[1]))]);

        let service = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.l1_dedup.mixed_not_found.records.v1",
        );

        let results = service
            .create_usage_records(&authenticated_ctx(), input)
            .await
            .expect("batch dispatch succeeded");
        assert_eq!(results.len(), 3);

        assert!(
            results[1].is_ok(),
            "record at index 1 (known corrects_id) MUST be accepted, got {:?}",
            results[1],
        );
        for idx in [0usize, 2] {
            match results[idx].as_ref() {
                Err(UsageCollectorError::NotFound {
                    resource_type,
                    detail,
                    ..
                }) if resource_type == USAGE_RECORD_RESOURCE && detail.contains("corrects_id") => {
                    assert!(
                        detail.contains(&corrects_id_bad.to_string()),
                        "record at index {idx} MUST surface CorrectsIdNotFound carrying \
                         the unknown corrects_id",
                    );
                }
                other => panic!(
                    "record at index {idx} MUST be rejected as CorrectsIdNotFound, got {other:?}",
                ),
            }
        }

        assert_eq!(
            plugin.get_usage_record_calls(),
            2,
            "2 distinct corrects_ids carrying 3 records MUST produce 2 get_usage_record \
             dispatches; observed {} calls",
            plugin.get_usage_record_calls(),
        );
    }
}

// ─── usage-emission feature (read-by-id) ─────────────────────────────────
//
// `Service::get_usage_record` is the host-side gateway for the read-by-id
// surface of the usage-emission feature: lazy plugin resolution, Plugin
// SPI Method 10 `get_usage_record` prefetch (so PDP can authorize over
// the loaded attribution tuple), PDP authz, and 1:1 outcome mapping of
// the plugin result taxonomy onto the SDK envelope. The pre-PDP fetch
// mirrors the deactivation gateway pattern in
// `cpt-cf-usage-collector-flow-event-deactivation-deactivate-record`:
// the handler has only `id` at the boundary and needs the row to compose
// the attribution-tuple PDP request. Tests pin:
//
// - Happy path: prefetch succeeds, PDP permits, the loaded record is
//   returned verbatim. Exactly one `get_usage_record` SPI dispatch.
// - Prefetch returns `UsageRecordNotFound { id }` → lifted to
//   `UsageCollectorError::NotFound { id }` BEFORE the PDP
//   step (the PDP is deny-all to prove the short-circuit).
// - PDP `deny` short-circuits AFTER the prefetch but BEFORE the record
//   is handed back to the caller.
// - PDP transport failure (`unreachable`) fails closed AFTER the
//   prefetch but BEFORE the record is handed back.
// - Prefetch transient error lifts to `ServiceUnavailable`.
mod get_usage_record_tests {
    use std::sync::Arc;
    use toolkit_gts::gts_id;

    use usage_collector_sdk::{
        USAGE_RECORD_RESOURCE, UsageCollectorError, UsageCollectorPluginV1, UsageRecord,
        UsageRecordStatus,
    };
    use uuid::Uuid;

    use crate::domain::Service;
    use crate::domain::test_support::{
        DenyAllResolver, HappyPathPlugin, UnreachableResolver, authenticated_ctx, enforcer_for,
        hub_with_plugin, service_with_permit,
    };

    const HAPPY_RECORD_GTS_ID: &str =
        gts_id!("cf.core.uc.usage_record.v1~cf.mini_chat._.tokens_consumed.v1");

    fn sample_persisted_record(id: Uuid, tenant_id: Uuid) -> UsageRecord {
        use std::collections::BTreeMap;
        use time::OffsetDateTime;
        use usage_collector_sdk::{IdempotencyKey, ResourceRef, UsageTypeGtsId};
        UsageRecord {
            id,
            gts_id: UsageTypeGtsId::new(HAPPY_RECORD_GTS_ID).expect("valid gts_id"),
            tenant_id,
            resource_ref: ResourceRef::new("rsc-happy", "compute.vm").expect("valid resource ref"),
            subject_ref: None,
            metadata: BTreeMap::new(),
            value: rust_decimal::Decimal::from(1),
            idempotency_key: IdempotencyKey::new("idem-happy").expect("valid idempotency key"),
            corrects_id: None,
            status: UsageRecordStatus::Active,
            created_at: OffsetDateTime::UNIX_EPOCH,
        }
    }

    /// Happy path: prefetch returns the row, PDP permits, the service
    /// returns the loaded record verbatim. Exactly one
    /// `get_usage_record` SPI round-trip.
    #[tokio::test]
    async fn get_usage_record_happy_path_returns_loaded_record() {
        let plugin = HappyPathPlugin::new();
        let target = Uuid::from_u128(0x00C0_FFEE);
        let tenant_id = Uuid::from_u128(2);
        plugin.set_get_record(sample_persisted_record(target, tenant_id));

        let svc = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.usage_collector.get_record.happy.v1",
        );

        let record = svc
            .get_usage_record(&authenticated_ctx(), target)
            .await
            .expect("happy-path read MUST succeed");

        assert_eq!(record.id, target);
        assert_eq!(record.tenant_id, tenant_id);
        assert_eq!(
            plugin.get_usage_record_calls(),
            1,
            "exactly one SPI prefetch on the happy path; observed {} calls",
            plugin.get_usage_record_calls(),
        );
        assert_eq!(
            plugin.get_usage_record_inputs().last().copied(),
            Some(target),
            "gateway MUST forward the target id verbatim to the plugin",
        );
    }

    /// Prefetch `UsageRecordNotFound { id }` short-circuits BEFORE the
    /// PDP step (the PDP is deny-all and would otherwise mask this
    /// outcome as `Authorization`). Mirrors the deactivate gateway's
    /// `inst-deactivate-record-prefetch-not-found` test.
    #[tokio::test]
    async fn get_usage_record_prefetch_not_found_skips_pdp() {
        let plugin = HappyPathPlugin::new();
        let target = Uuid::from_u128(0xDEAD_F00D);
        plugin.set_get_usage_record_not_found(target);

        // Deny-all PDP: if the gateway reached the PDP step, the surface
        // error would be `Authorization`, not `UsageRecordNotFound`.
        let hub = hub_with_plugin(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.usage_collector.get_record.prefetch_not_found.v1",
            "constructorfabric",
        );
        let enforcer = enforcer_for(Arc::new(DenyAllResolver));
        let svc = Arc::new(Service::new(hub, "constructorfabric".to_owned(), enforcer));

        let err = svc
            .get_usage_record(&authenticated_ctx(), target)
            .await
            .expect_err("prefetch UsageRecordNotFound MUST surface as NotFound");

        assert!(
            matches!(
                err,
                UsageCollectorError::NotFound { ref resource_type, ref name, .. }
                    if resource_type == USAGE_RECORD_RESOURCE && name == &target.to_string()
            ),
            "expected NotFound carrying the target id, got {err:?}",
        );
    }

    /// PDP deny collapses to `NotFound` AFTER the prefetch.
    #[tokio::test]
    async fn get_usage_record_pdp_deny_collapses_to_not_found_after_prefetch() {
        let plugin = HappyPathPlugin::new();
        let target = Uuid::from_u128(0xFEED);
        let tenant_id = Uuid::from_u128(2);
        plugin.set_get_record(sample_persisted_record(target, tenant_id));

        let hub = hub_with_plugin(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.usage_collector.get_record.pdp_deny.v1",
            "constructorfabric",
        );
        let enforcer = enforcer_for(Arc::new(DenyAllResolver));
        let svc = Arc::new(Service::new(hub, "constructorfabric".to_owned(), enforcer));

        let err = svc
            .get_usage_record(&authenticated_ctx(), target)
            .await
            .expect_err("PDP deny MUST surface as NotFound");

        assert!(
            matches!(err, UsageCollectorError::NotFound { .. }),
            "expected NotFound, got {err:?}",
        );
        assert_eq!(
            plugin.get_usage_record_calls(),
            1,
            "prefetch DID happen (it's the only way to load the attribution tuple)",
        );
    }

    /// PDP transport failure fails closed AFTER the prefetch.
    #[tokio::test]
    async fn get_usage_record_pdp_unreachable_fails_closed_after_prefetch() {
        let plugin = HappyPathPlugin::new();
        let target = Uuid::from_u128(0xFACE);
        let tenant_id = Uuid::from_u128(2);
        plugin.set_get_record(sample_persisted_record(target, tenant_id));

        let hub = hub_with_plugin(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.usage_collector.get_record.pdp_unreachable.v1",
            "constructorfabric",
        );
        let enforcer = enforcer_for(Arc::new(UnreachableResolver));
        let svc = Arc::new(Service::new(hub, "constructorfabric".to_owned(), enforcer));

        let err = svc
            .get_usage_record(&authenticated_ctx(), target)
            .await
            .expect_err("unreachable PDP transport MUST fail closed");

        assert!(
            matches!(err, UsageCollectorError::ServiceUnavailable { .. }),
            "expected ServiceUnavailable, got {err:?}",
        );
        assert!(
            plugin.get_usage_record_calls() >= 1,
            "prefetch MUST run before PDP transport failure causes the fail-closed lift",
        );
    }

    /// Prefetch transient failure lifts through the canonical chain to
    /// `ServiceUnavailable` — same envelope the deactivate gateway emits
    /// on a prefetch transient.
    #[tokio::test]
    async fn get_usage_record_prefetch_transient_lifts_to_service_unavailable() {
        use usage_collector_sdk::UsageCollectorPluginError;

        // Build a tiny one-shot stub that always returns Transient on
        // get_usage_record; HappyPathPlugin doesn't expose a transient
        // path so we use the existing pattern with the deactivate-stub
        // approach folded down to a minimal inline plugin.
        struct TransientGetPlugin;

        #[async_trait::async_trait]
        impl UsageCollectorPluginV1 for TransientGetPlugin {
            async fn create_usage_record(
                &self,
                _record: UsageRecord,
            ) -> Result<UsageRecord, UsageCollectorPluginError> {
                Err(UsageCollectorPluginError::internal(
                    "test_fake: TransientGetPlugin: create_usage_record must not be called",
                ))
            }
            async fn create_usage_records(
                &self,
                _records: Vec<UsageRecord>,
            ) -> Result<
                Vec<Result<UsageRecord, UsageCollectorPluginError>>,
                UsageCollectorPluginError,
            > {
                Err(UsageCollectorPluginError::internal(
                    "test_fake: TransientGetPlugin: create_usage_records must not be called",
                ))
            }
            async fn query_aggregated_usage_records(
                &self,
                _gts_id: usage_collector_sdk::UsageTypeGtsId,
                _query: &toolkit_odata::ODataQuery,
                _metadata_filter: &[usage_collector_sdk::MetadataFilter],
                _aggregation: usage_collector_sdk::AggregationSpec,
            ) -> Result<usage_collector_sdk::AggregationResult, UsageCollectorPluginError>
            {
                Err(UsageCollectorPluginError::internal(
                    "test_fake: TransientGetPlugin: query_aggregated_usage_records must not be called",
                ))
            }
            async fn list_usage_records(
                &self,
                _gts_id: usage_collector_sdk::UsageTypeGtsId,
                _query: &toolkit_odata::ODataQuery,
                _metadata_filter: &[usage_collector_sdk::MetadataFilter],
            ) -> Result<toolkit_odata::Page<UsageRecord>, UsageCollectorPluginError> {
                Err(UsageCollectorPluginError::internal(
                    "test_fake: TransientGetPlugin: list_usage_records must not be called",
                ))
            }
            async fn deactivate_usage_record(
                &self,
                _id: Uuid,
            ) -> Result<(), UsageCollectorPluginError> {
                Err(UsageCollectorPluginError::internal(
                    "test_fake: TransientGetPlugin: deactivate_usage_record must not be called",
                ))
            }
            async fn create_usage_type(
                &self,
                _usage_type: usage_collector_sdk::UsageType,
            ) -> Result<usage_collector_sdk::UsageType, UsageCollectorPluginError> {
                Err(UsageCollectorPluginError::internal(
                    "test_fake: TransientGetPlugin: create_usage_type must not be called",
                ))
            }
            async fn get_usage_type(
                &self,
                _gts_id: usage_collector_sdk::UsageTypeGtsId,
            ) -> Result<usage_collector_sdk::UsageType, UsageCollectorPluginError> {
                Err(UsageCollectorPluginError::internal(
                    "test_fake: TransientGetPlugin: get_usage_type must not be called",
                ))
            }
            async fn list_usage_types(
                &self,
                _query: &toolkit_odata::ODataQuery,
            ) -> Result<
                toolkit_odata::Page<usage_collector_sdk::UsageType>,
                UsageCollectorPluginError,
            > {
                Err(UsageCollectorPluginError::internal(
                    "test_fake: TransientGetPlugin: list_usage_types must not be called",
                ))
            }
            async fn delete_usage_type(
                &self,
                _gts_id: usage_collector_sdk::UsageTypeGtsId,
            ) -> Result<(), UsageCollectorPluginError> {
                Err(UsageCollectorPluginError::internal(
                    "test_fake: TransientGetPlugin: delete_usage_type must not be called",
                ))
            }
            async fn get_usage_record(
                &self,
                _id: Uuid,
            ) -> Result<UsageRecord, UsageCollectorPluginError> {
                Err(UsageCollectorPluginError::transient(
                    "test_fake: TransientGetPlugin: simulated prefetch transient",
                ))
            }
        }

        let plugin: Arc<dyn UsageCollectorPluginV1> = Arc::new(TransientGetPlugin);
        let svc = service_with_permit(plugin, "test.usage_collector.get_record.transient.v1");

        let err = svc
            .get_usage_record(&authenticated_ctx(), Uuid::from_u128(0x01))
            .await
            .expect_err("prefetch Transient MUST lift to ServiceUnavailable");

        assert!(
            matches!(err, UsageCollectorError::ServiceUnavailable { .. }),
            "expected ServiceUnavailable from prefetch, got {err:?}",
        );
        assert!(err.is_retryable());
    }
}

#[cfg(test)]
mod private_helpers_tests {
    //! Unit coverage for the private service-module helpers
    //! (`compose_query_with_scope`). Lives alongside the SDK-surface tests
    //! in this file so the codebase keeps one test module per source.

    use toolkit_odata::ODataQuery;
    use toolkit_odata::ast::{CompareOperator, Expr, Value};
    use toolkit_security::{AccessScope, ScopeConstraint, ScopeFilter, pep_properties};
    use usage_collector_sdk::UsageCollectorError;
    use uuid::Uuid;

    use crate::domain::query::compose_query_with_scope;

    #[test]
    fn allow_all_scope_is_denied_fail_closed() {
        // An `allow_all` scope on the LIST/aggregate path is a degenerate
        // empty-predicate permit, not a happy-path grant. Composition MUST fail
        // closed rather than pass the user filter through unscoped (which would
        // return every tenant's records).
        let user_filter = Expr::Compare(
            Box::new(Expr::Identifier("resource_type".into())),
            CompareOperator::Eq,
            Box::new(Expr::Value(Value::String("compute.vm".into()))),
        );
        let mut user_query = ODataQuery::new();
        user_query.filter = Some(Box::new(user_filter));
        user_query.limit = Some(50);

        let err = compose_query_with_scope(&user_query, &AccessScope::allow_all())
            .expect_err("allow_all -> PermissionDenied");
        assert!(
            matches!(err, UsageCollectorError::PermissionDenied { .. }),
            "allow_all must surface as PermissionDenied, got {err:?}",
        );
    }

    #[test]
    fn empty_user_filter_yields_scope_filter_alone() {
        let scope = AccessScope::single(ScopeConstraint::new(vec![ScopeFilter::eq(
            pep_properties::OWNER_TENANT_ID,
            Uuid::from_u128(0xAA),
        )]));
        let composed = compose_query_with_scope(&ODataQuery::new(), &scope).expect("happy path");
        let f = composed.filter().expect("scope-only filter");
        assert!(matches!(f, Expr::Compare(..)));
    }

    #[test]
    fn user_filter_and_scope_are_and_merged() {
        let user_filter = Expr::Compare(
            Box::new(Expr::Identifier("status".into())),
            CompareOperator::Eq,
            Box::new(Expr::Value(Value::String("active".into()))),
        );
        let mut user_query = ODataQuery::new();
        user_query.filter = Some(Box::new(user_filter));

        let scope = AccessScope::single(ScopeConstraint::new(vec![ScopeFilter::eq(
            pep_properties::OWNER_TENANT_ID,
            Uuid::from_u128(0xBB),
        )]));
        let composed = compose_query_with_scope(&user_query, &scope).expect("happy path");
        let f = composed.filter().expect("merged filter");
        assert!(matches!(f, Expr::And(..)));
    }

    #[test]
    fn deny_all_scope_short_circuits_to_authorization_error() {
        let err = compose_query_with_scope(&ODataQuery::new(), &AccessScope::deny_all())
            .expect_err("deny_all -> PermissionDenied");
        assert!(
            matches!(err, UsageCollectorError::PermissionDenied { .. }),
            "deny_all must surface as PermissionDenied, got {err:?}",
        );
    }
}

// The plural `create_usage_records` path is exercised by the `pdp_dedup`,
// `gts_id_dedup`, and `corrects_id_dedup` modules; the singular path has
// its own PDP / catalog / semantics / L1 / SPI sequencing in
// `service.rs::create_usage_record` and was uncovered. These tests pin one
// outcome per stage: PDP deny, plugin-reported transient on the persist
// SPI, semantics violations (negative counter + gauge compensation),
// L1 corrects_id not-found, L1 corrects_id wrong-scope, and the happy
// path. The mirror keeps a single source-of-truth for what "every stage
// rejects with its locked SDK envelope" means for the singular flow.

mod create_usage_record_path_tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::sync::Arc;
    use toolkit_gts::gts_id;

    use time::OffsetDateTime;
    use usage_collector_sdk::{
        ConflictReason, CreateUsageRecord, IdempotencyKey, ResourceRef, USAGE_RECORD_RESOURCE,
        UsageCollectorError, UsageCollectorPluginError, UsageCollectorPluginV1, UsageKind,
        UsageRecord, UsageRecordStatus, UsageType, UsageTypeGtsId, ValidationReason,
    };
    use uuid::Uuid;

    use crate::domain::Service;
    use crate::domain::test_support::{
        DenyAllResolver, HappyPathPlugin, authenticated_ctx, enforcer_for, hub_with_plugin,
        service_with_permit,
    };

    const COUNTER_GTS_ID: &str =
        gts_id!("cf.core.uc.usage_record.v1~cf.mini_chat._.tokens_consumed.v1");
    const GAUGE_GTS_ID: &str =
        gts_id!("cf.core.uc.usage_record.v1~cf.mini_chat._.tokens_inflight.v1");

    fn counter_usage_type() -> UsageType {
        UsageType {
            gts_id: UsageTypeGtsId::new(COUNTER_GTS_ID).expect("valid gts_id"),
            kind: UsageKind::Counter,
            metadata_fields: BTreeSet::new(),
        }
    }

    fn gauge_usage_type() -> UsageType {
        UsageType {
            gts_id: UsageTypeGtsId::new(GAUGE_GTS_ID).expect("valid gts_id"),
            kind: UsageKind::Gauge,
            metadata_fields: BTreeSet::new(),
        }
    }

    /// Build a counter ordinary record (no `corrects_id`) with the given
    /// `tenant_id` and `value`. Used as the base shape every test in this
    /// module shapes — call sites mutate `value` / `gts_id` /
    /// `corrects_id` to drive the per-stage outcome.
    fn counter_record(tenant_id: Uuid, value: i64, idem: &str) -> CreateUsageRecord {
        CreateUsageRecord {
            gts_id: UsageTypeGtsId::new(COUNTER_GTS_ID).expect("valid gts_id"),
            tenant_id,
            resource_ref: ResourceRef::new("rsc-singular", "compute.vm")
                .expect("valid resource ref"),
            subject_ref: None,
            metadata: BTreeMap::new(),
            value: rust_decimal::Decimal::from(value),
            idempotency_key: IdempotencyKey::new(idem).expect("valid idempotency key"),
            corrects_id: None,
            created_at: OffsetDateTime::UNIX_EPOCH,
        }
    }

    fn gauge_compensation(tenant_id: Uuid, corrects_id: Uuid, idem: &str) -> CreateUsageRecord {
        CreateUsageRecord {
            gts_id: UsageTypeGtsId::new(GAUGE_GTS_ID).expect("valid gts_id"),
            tenant_id,
            resource_ref: ResourceRef::new("rsc-singular", "compute.vm")
                .expect("valid resource ref"),
            subject_ref: None,
            metadata: BTreeMap::new(),
            // Negative value passes the counter-compensation gate; the
            // gauge-kind check is what rejects this — both gates would
            // otherwise mask which one fired.
            value: rust_decimal::Decimal::from(-1),
            idempotency_key: IdempotencyKey::new(idem).expect("valid idempotency key"),
            corrects_id: Some(corrects_id),
            created_at: OffsetDateTime::UNIX_EPOCH,
        }
    }

    fn counter_compensation(tenant_id: Uuid, corrects_id: Uuid, idem: &str) -> CreateUsageRecord {
        let mut r = counter_record(tenant_id, -1, idem);
        r.corrects_id = Some(corrects_id);
        r
    }

    /// Build a `Service` wired against a deny-all PDP. The deny path
    /// MUST short-circuit before any plugin SPI dispatch; the plugin
    /// stub is left unprogrammed so a leaked SPI call surfaces as a
    /// `not_programmed` `Internal` (distinct from the expected
    /// `Authorization` envelope) and fails the test loudly.
    fn service_with_deny(plugin: Arc<dyn UsageCollectorPluginV1>, suffix: &str) -> Arc<Service> {
        let hub = hub_with_plugin(plugin, suffix, "constructorfabric");
        let enforcer = enforcer_for(Arc::new(DenyAllResolver) as _);
        Arc::new(Service::new(hub, "constructorfabric".to_owned(), enforcer))
    }

    /// PDP deny ⇒ `Authorization` envelope, **no** catalog / semantics /
    /// SPI dispatch. The unprogrammed plugin asserts the short-circuit:
    /// any leaked SPI call would surface as `Internal("not programmed")`
    /// instead of `Authorization` and the assertion below would fail.
    #[tokio::test]
    async fn create_usage_record_pdp_deny_returns_authorization_before_any_spi_dispatch() {
        let plugin = HappyPathPlugin::new();
        let service = service_with_deny(
            Arc::clone(&plugin) as _,
            "test.singular.pdp_deny.records.v1",
        );

        let record = counter_record(Uuid::from_u128(0x701), 1, "idem-pdp-deny");

        let err = service
            .create_usage_record(&authenticated_ctx(), record)
            .await
            .expect_err("PDP deny MUST surface as Err");

        assert!(
            matches!(err, UsageCollectorError::PermissionDenied { .. }),
            "PDP deny MUST surface as `PermissionDenied`, got {err:?}",
        );
        assert_eq!(
            plugin.get_usage_type_calls(),
            0,
            "PDP deny MUST short-circuit before the catalog lookup",
        );
        assert_eq!(
            plugin.get_usage_record_calls(),
            0,
            "PDP deny MUST short-circuit before any L1 lookup",
        );
        assert!(
            plugin.last_create_records_input().is_none(),
            "PDP deny MUST short-circuit before any persist SPI dispatch",
        );
    }

    /// Plugin-reported `Transient` from the persist SPI ⇒
    /// `ServiceUnavailable` at the SDK boundary, with the
    /// `retry_after_seconds` hint forwarded verbatim. Pins the lift
    /// path documented on `UsageCollectorPluginError::transient_with_retry`.
    #[tokio::test]
    async fn create_usage_record_plugin_transient_lifts_to_service_unavailable() {
        let plugin = HappyPathPlugin::new();
        plugin.set_get_usage_type(counter_usage_type());
        plugin.set_create_record_err(UsageCollectorPluginError::transient_with_retry(
            "downstream backend timed out",
            Some(13),
        ));

        let service = service_with_permit(
            Arc::clone(&plugin) as _,
            "test.singular.plugin_transient.records.v1",
        );

        let record = counter_record(Uuid::from_u128(0x702), 1, "idem-plugin-transient");

        let err = service
            .create_usage_record(&authenticated_ctx(), record)
            .await
            .expect_err("plugin transient MUST surface as Err");

        match err {
            UsageCollectorError::ServiceUnavailable {
                detail,
                retry_after_seconds,
            } => {
                assert_eq!(detail, "downstream backend timed out");
                assert_eq!(retry_after_seconds, Some(13));
            }
            other => panic!(
                "plugin Transient MUST lift to `ServiceUnavailable` with the \
                 retry hint forwarded; got {other:?}",
            ),
        }
    }

    /// Counter ordinary record with `value < 0` ⇒ `NegativeCounterValue`
    /// (the four-cell semantics matrix's `counter + corrects_id NULL`
    /// cell rejects negatives) and the persist SPI is never reached.
    #[tokio::test]
    async fn create_usage_record_negative_counter_value_rejected_before_persist() {
        let plugin = HappyPathPlugin::new();
        plugin.set_get_usage_type(counter_usage_type());

        let service = service_with_permit(
            Arc::clone(&plugin) as _,
            "test.singular.semantics_negative_counter.records.v1",
        );

        let record = counter_record(Uuid::from_u128(0x703), -1, "idem-neg-counter");

        let err = service
            .create_usage_record(&authenticated_ctx(), record)
            .await
            .expect_err("negative counter value MUST surface as Err");

        assert!(
            matches!(
                err,
                UsageCollectorError::InvalidArgument {
                    reason: ValidationReason::SemanticsViolation,
                    ref detail,
                    ..
                } if detail.contains("value >= 0")
            ),
            "counter ordinary record with value < 0 MUST surface as \
             `NegativeCounterValue`; got {err:?}",
        );
        assert!(
            plugin.last_create_records_input().is_none(),
            "semantics violation MUST short-circuit before the persist SPI",
        );
    }

    /// Gauge + `corrects_id` SET (a compensation against a gauge) ⇒
    /// `GaugeCompensationRejected`. The catalog returned a gauge, so the
    /// service rejects the compensation in the four-cell matrix's
    /// `gauge + corrects_id SET` cell. The L1 lookup is NOT performed —
    /// the kind-based rejection fires inside `validate_record_semantics`
    /// before the L1 step.
    #[tokio::test]
    async fn create_usage_record_gauge_compensation_rejected_before_l1_lookup() {
        let plugin = HappyPathPlugin::new();
        plugin.set_get_usage_type(gauge_usage_type());

        let service = service_with_permit(
            Arc::clone(&plugin) as _,
            "test.singular.gauge_compensation.records.v1",
        );

        let corrects_id = Uuid::from_u128(0x800);
        let record = gauge_compensation(Uuid::from_u128(0x704), corrects_id, "idem-gauge-comp");

        let err = service
            .create_usage_record(&authenticated_ctx(), record)
            .await
            .expect_err("gauge compensation MUST surface as Err");

        assert!(
            matches!(
                err,
                UsageCollectorError::InvalidArgument {
                    reason: ValidationReason::GaugeCompensationRejected,
                    ..
                }
            ),
            "compensation against a gauge usage type MUST surface as \
             `GaugeCompensationRejected`; got {err:?}",
        );
        assert_eq!(
            plugin.get_usage_record_calls(),
            0,
            "the kind-based gauge-compensation rejection MUST fire \
             before the L1 corrects_id lookup",
        );
    }

    /// `corrects_id` references a uuid the plugin does not have ⇒
    /// `CorrectsIdNotFound`. Pins the L1 referential rule 1 lift on the
    /// singular path (the plural path has its own coverage in
    /// `corrects_id_dedup_tests`).
    #[tokio::test]
    async fn create_usage_record_l1_corrects_id_not_found_returns_typed_error() {
        let plugin = HappyPathPlugin::new();
        plugin.set_get_usage_type(counter_usage_type());

        let missing = Uuid::from_u128(0x801);
        plugin.set_get_usage_record_not_found(missing);

        let service = service_with_permit(
            Arc::clone(&plugin) as _,
            "test.singular.l1_not_found.records.v1",
        );

        let record = counter_compensation(Uuid::from_u128(0x705), missing, "idem-l1-missing");

        let err = service
            .create_usage_record(&authenticated_ctx(), record)
            .await
            .expect_err("unknown corrects_id MUST surface as Err");

        assert!(
            matches!(
                err,
                UsageCollectorError::NotFound { ref resource_type, ref detail, .. }
                    if resource_type == USAGE_RECORD_RESOURCE
                        && detail.contains("corrects_id")
                        && detail.contains(&missing.to_string())
            ),
            "L1 referential rule 1 MUST surface as `CorrectsIdNotFound` \
             carrying the caller-supplied corrects_id; got {err:?}",
        );
        assert!(
            plugin.last_create_records_input().is_none(),
            "L1 not-found MUST short-circuit before the persist SPI",
        );
    }

    /// `corrects_id` references a row in a different `tenant_id` ⇒
    /// `CorrectsIdWrongScope`. Pins the L1 referential rule 3 lift on
    /// the singular path: the verifier reads identity-tuple fields
    /// (tenant, `gts_id`, `resource_ref`, `subject_ref`) off the referenced
    /// row and rejects on the first mismatch.
    #[tokio::test]
    async fn create_usage_record_l1_corrects_id_wrong_scope_returns_typed_error() {
        let plugin = HappyPathPlugin::new();
        plugin.set_get_usage_type(counter_usage_type());

        let referenced_tenant = Uuid::from_u128(0x901);
        let other_tenant = Uuid::from_u128(0x902);
        let corrects_id = Uuid::from_u128(0x802);

        // Referenced row sits in `referenced_tenant`; the incoming
        // compensation will be shaped under `other_tenant` so the
        // identity-tuple comparison fails on the tenant axis.
        let referenced = UsageRecord {
            id: corrects_id,
            gts_id: UsageTypeGtsId::new(COUNTER_GTS_ID).expect("valid gts_id"),
            tenant_id: referenced_tenant,
            resource_ref: ResourceRef::new("rsc-singular", "compute.vm")
                .expect("valid resource ref"),
            subject_ref: None,
            metadata: BTreeMap::new(),
            value: rust_decimal::Decimal::from(10),
            idempotency_key: IdempotencyKey::new("idem-referenced").expect("valid idempotency key"),
            corrects_id: None,
            status: UsageRecordStatus::Active,
            created_at: OffsetDateTime::UNIX_EPOCH,
        };
        plugin.set_get_record(referenced);

        let service = service_with_permit(
            Arc::clone(&plugin) as _,
            "test.singular.l1_wrong_scope.records.v1",
        );

        let record = counter_compensation(other_tenant, corrects_id, "idem-l1-wrong-scope");

        let err = service
            .create_usage_record(&authenticated_ctx(), record)
            .await
            .expect_err("cross-tenant corrects_id MUST surface as Err");

        assert!(
            matches!(
                err,
                UsageCollectorError::Conflict {
                    reason: ConflictReason::CorrectsIdWrongScope,
                    ref name,
                    ref detail,
                    ..
                } if name == &corrects_id.to_string()
                    || detail.contains(&corrects_id.to_string()),
            ),
            "L1 referential rule 3 MUST surface as `CorrectsIdWrongScope` \
             carrying the caller-supplied corrects_id; got {err:?}",
        );
        assert!(
            plugin.last_create_records_input().is_none(),
            "L1 wrong-scope MUST short-circuit before the persist SPI",
        );
    }

    /// Happy path: PDP permit + catalog hit + ordinary counter semantics +
    /// metadata validation pass + persist SPI returns the persisted echo
    /// ⇒ `Ok(persisted_record)`. The persisted echo's `id` differs
    /// from the input so the returned record can be distinguished from
    /// the caller-supplied one.
    #[tokio::test]
    async fn create_usage_record_happy_path_returns_persisted_echo() {
        let plugin = HappyPathPlugin::new();
        plugin.set_get_usage_type(counter_usage_type());

        let mut persisted =
            counter_record(Uuid::from_u128(0xCAFE), 1, "idem-happy-persist").into_usage_record();
        // Distinguish persisted from input — the plugin's persisted echo
        // carries a different id than the record the service derives and
        // dispatches.
        persisted.id = Uuid::from_u128(0xDEAD_C0DE);
        plugin.set_create_record(persisted.clone());

        let service = service_with_permit(
            Arc::clone(&plugin) as _,
            "test.singular.happy_path.records.v1",
        );

        let input = counter_record(Uuid::from_u128(0x706), 1, "idem-happy-input");

        let returned = service
            .create_usage_record(&authenticated_ctx(), input)
            .await
            .expect("happy path MUST return the persisted record");

        assert_eq!(
            returned.id, persisted.id,
            "the returned record MUST be the plugin's persisted echo \
             (not the caller's input)",
        );
        assert_eq!(
            plugin.get_usage_type_calls(),
            1,
            "happy path MUST issue exactly one catalog lookup",
        );
        assert_eq!(
            plugin.get_usage_record_calls(),
            0,
            "ordinary counter records (corrects_id IS NULL) MUST NOT \
             trigger an L1 lookup",
        );
    }
}

// ── Batch size-cap guard in `create_usage_records` ─────────────────────────
//
// Pins the entry gate at `service.rs::create_usage_records` (the
// `actual == 0 || actual > MAX_BATCH_RECORDS` check,
// `inst-emit-batch-cap-check`): both out-of-bounds arms MUST reject with
// `invalid_batch_size` *before* any plugin dispatch.
#[cfg(test)]
mod batch_size_cap_tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::sync::Arc;
    use toolkit_gts::gts_id;

    use time::OffsetDateTime;
    use usage_collector_sdk::{
        CreateUsageRecord, IdempotencyKey, ResourceRef, UsageCollectorError,
        UsageCollectorPluginV1, UsageKind, UsageType, UsageTypeGtsId, ValidationReason,
    };
    use uuid::Uuid;

    use crate::domain::service::MAX_BATCH_RECORDS;
    use crate::domain::test_support::{HappyPathPlugin, authenticated_ctx, service_with_permit};

    const GTS_ID: &str = gts_id!("cf.core.uc.usage_record.v1~cf.mini_chat._.tokens_consumed.v1");

    fn counter_usage_type() -> UsageType {
        UsageType {
            gts_id: UsageTypeGtsId::new(GTS_ID).expect("valid gts_id"),
            kind: UsageKind::Counter,
            metadata_fields: BTreeSet::new(),
        }
    }

    fn input_record(idem: &str) -> CreateUsageRecord {
        CreateUsageRecord {
            gts_id: UsageTypeGtsId::new(GTS_ID).expect("valid gts_id"),
            tenant_id: Uuid::from_u128(1),
            resource_ref: ResourceRef::new("rsc-batch-cap", "compute.vm")
                .expect("valid resource ref"),
            subject_ref: None,
            metadata: BTreeMap::new(),
            value: rust_decimal::Decimal::from(1),
            idempotency_key: IdempotencyKey::new(idem).expect("valid idempotency key"),
            corrects_id: None,
            created_at: OffsetDateTime::UNIX_EPOCH,
        }
    }

    fn assert_invalid_batch_size(err: &UsageCollectorError) {
        assert!(
            matches!(
                err,
                UsageCollectorError::InvalidArgument {
                    reason: ValidationReason::Validation,
                    field,
                    ..
                } if field == "records"
            ),
            "expected invalid_batch_size InvalidArgument on `records`, got {err:?}",
        );
    }

    /// Empty batch (`actual == 0`) → `invalid_batch_size`, plugin untouched.
    #[tokio::test]
    async fn create_usage_records_rejects_empty_batch_without_dispatch() {
        let plugin = HappyPathPlugin::new();
        plugin.set_get_usage_type(counter_usage_type());

        let service = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.batch_cap.empty.records.v1",
        );

        let err = service
            .create_usage_records(&authenticated_ctx(), Vec::new())
            .await
            .expect_err("empty batch MUST reject");
        assert_invalid_batch_size(&err);

        assert!(
            plugin.last_create_records_input().is_none(),
            "empty batch MUST short-circuit before any plugin dispatch",
        );
        assert_eq!(
            plugin.get_usage_type_calls(),
            0,
            "empty batch MUST NOT issue a catalog lookup",
        );
    }

    /// Over-cap batch (`MAX_BATCH_RECORDS + 1`) → `invalid_batch_size`,
    /// plugin untouched.
    #[tokio::test]
    async fn create_usage_records_rejects_over_cap_batch_without_dispatch() {
        let plugin = HappyPathPlugin::new();
        plugin.set_get_usage_type(counter_usage_type());

        let service = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.batch_cap.over.records.v1",
        );

        let input: Vec<CreateUsageRecord> = (0..=MAX_BATCH_RECORDS)
            .map(|i| input_record(&format!("idem-cap-{i}")))
            .collect();
        assert_eq!(input.len(), MAX_BATCH_RECORDS + 1);

        let err = service
            .create_usage_records(&authenticated_ctx(), input)
            .await
            .expect_err("over-cap batch MUST reject");
        assert_invalid_batch_size(&err);

        assert!(
            plugin.last_create_records_input().is_none(),
            "over-cap batch MUST short-circuit before any plugin dispatch",
        );
        assert_eq!(
            plugin.get_usage_type_calls(),
            0,
            "over-cap batch MUST NOT issue a catalog lookup",
        );
    }
}

// ── Service-level derived-id stamp (in-process / SDK callers) ───────────────
//
// The create surface is identity-free (`CreateUsageRecord`): callers never
// supply an `id`. The domain `Service` is the single, guaranteed point where a
// submission acquires its identity — via
// `CreateUsageRecord::into_usage_record`, which derives the `id` from the dedup
// key. These tests drive the `Service` create methods directly (NOT through the
// REST handler) and assert the record the plugin RECEIVED carries the
// deterministic derivation, pinning that the service stamps the derived id on
// the dispatch path.
#[cfg(test)]
mod derived_id_stamp_tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::sync::Arc;

    use time::OffsetDateTime;
    use toolkit_gts::gts_id;
    use usage_collector_sdk::{
        CreateUsageRecord, IdempotencyKey, ResourceRef, UsageCollectorPluginV1, UsageKind,
        UsageType, UsageTypeGtsId, derive_usage_record_id,
    };
    use uuid::Uuid;

    use crate::domain::test_support::{HappyPathPlugin, authenticated_ctx, service_with_permit};

    const GTS_ID: &str = gts_id!("cf.core.uc.usage_record.v1~cf.mini_chat._.tokens_consumed.v1");

    fn counter_usage_type() -> UsageType {
        UsageType {
            gts_id: UsageTypeGtsId::new(GTS_ID).expect("valid gts_id"),
            kind: UsageKind::Counter,
            metadata_fields: BTreeSet::new(),
        }
    }

    /// Build a create submission with a known dedup key
    /// (`tenant_id` / `gts_id` / `idempotency_key` / `created_at`), so a
    /// passing assertion can only mean the service derived the dispatched
    /// record's id from it.
    fn input_record(tenant_id: Uuid, idem: &str) -> CreateUsageRecord {
        CreateUsageRecord {
            gts_id: UsageTypeGtsId::new(GTS_ID).expect("valid gts_id"),
            tenant_id,
            resource_ref: ResourceRef::new("rsc-derive", "compute.vm").expect("valid resource ref"),
            subject_ref: None,
            metadata: BTreeMap::new(),
            value: rust_decimal::Decimal::from(1),
            idempotency_key: IdempotencyKey::new(idem).expect("valid idempotency key"),
            corrects_id: None,
            created_at: OffsetDateTime::UNIX_EPOCH,
        }
    }

    /// Singular `create_usage_record`: the dispatched record's `id` MUST be the
    /// service-derived value.
    #[tokio::test]
    async fn create_usage_record_stamps_derived_id() {
        let plugin = HappyPathPlugin::new();
        plugin.set_get_usage_type(counter_usage_type());

        let tenant_id = Uuid::from_u128(0xD1);
        let idem = "idem-derive-singular";
        let input = input_record(tenant_id, idem);
        let expected = derive_usage_record_id(
            input.tenant_id,
            &input.gts_id,
            &input.idempotency_key,
            input.created_at,
        );

        // The plugin echoes back the record it was dispatched, so the persist
        // SPI succeeds; the assertion reads the CAPTURED dispatched record.
        plugin.set_create_record(input.clone().into_usage_record());

        let service = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.derived_id.singular.records.v1",
        );

        service
            .create_usage_record(&authenticated_ctx(), input)
            .await
            .expect("happy path MUST accept the record");

        let dispatched = plugin
            .last_create_record_input()
            .expect("plugin received the dispatched record");
        assert_eq!(
            dispatched.id, expected,
            "the SERVICE MUST stamp the dispatched record's id with \
             derive_usage_record_id(tenant_id, gts_id, idempotency_key, created_at) - this \
             guards the in-process (non-REST) caller path independently of the \
             handler",
        );
    }

    /// Batch `create_usage_records`: every dispatched record's `id` MUST be its
    /// own service-derived value.
    #[tokio::test]
    async fn create_usage_records_stamps_derived_id() {
        let plugin = HappyPathPlugin::new();
        plugin.set_get_usage_type(counter_usage_type());

        let tenant_id = Uuid::from_u128(0xD2);
        let idems = ["idem-derive-batch-0", "idem-derive-batch-1"];
        let input: Vec<CreateUsageRecord> = idems
            .iter()
            .map(|idem| input_record(tenant_id, idem))
            .collect();
        let expected: Vec<Uuid> = input
            .iter()
            .map(|r| {
                derive_usage_record_id(r.tenant_id, &r.gts_id, &r.idempotency_key, r.created_at)
            })
            .collect();

        plugin.set_create_records(
            input
                .iter()
                .cloned()
                .map(|r| Ok(r.into_usage_record()))
                .collect(),
        );

        let service = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.derived_id.batch.records.v1",
        );

        let results = service
            .create_usage_records(&authenticated_ctx(), input)
            .await
            .expect("batch dispatch succeeded");
        assert!(results.iter().all(Result::is_ok));

        let dispatched = plugin
            .last_create_records_input()
            .expect("plugin received the dispatched batch");
        let dispatched_ids: Vec<Uuid> = dispatched.iter().map(|r| r.id).collect();
        assert_eq!(
            dispatched_ids, expected,
            "the SERVICE MUST stamp each dispatched record's id with its own \
             derive_usage_record_id(tenant_id, gts_id, idempotency_key, created_at), \
             overwriting the caller-supplied ids - this guards the in-process \
             (non-REST) batch caller path independently of the handler",
        );
    }
}

mod aggregate_op_kind_enforcement_tests {
    use std::collections::BTreeSet;
    use std::sync::Arc;

    use toolkit_gts::gts_id;
    use toolkit_security::pep_properties;
    use usage_collector_sdk::{
        AggregationBucket, AggregationOp, AggregationResult, AggregationSpec,
        MAX_AGGREGATION_BUCKETS, UsageCollectorError, UsageCollectorPluginV1, UsageKind, UsageType,
        UsageTypeGtsId, ValidationReason,
    };
    use uuid::Uuid;

    use crate::domain::Service;
    use crate::domain::test_support::{
        CountingPermitResolver, HappyPathPlugin, authenticated_ctx, enforcer_for, hub_with_plugin,
    };

    const GTS_ID: &str = gts_id!("cf.core.uc.usage_record.v1~cf.mini_chat._.tokens_consumed.v1");

    fn gts_id() -> UsageTypeGtsId {
        UsageTypeGtsId::new(GTS_ID).expect("valid gts_id")
    }

    fn usage_type(kind: UsageKind) -> UsageType {
        UsageType {
            gts_id: gts_id(),
            kind,
            metadata_fields: BTreeSet::new(),
        }
    }

    fn spec(op: AggregationOp) -> AggregationSpec {
        AggregationSpec {
            op,
            group_by: Vec::new(),
        }
    }

    // Mirrors the proven aggregate-path handler-test wiring: a permit scoped to
    // the request's OWNER_TENANT_ID (uuid 2, matching `authenticated_ctx`), so
    // authz permits AND PDP-constraint composition succeeds — allowed pairs
    // reach the plugin dispatch.
    fn service_with_plugin(plugin: &Arc<HappyPathPlugin>, suffix: &str) -> Arc<Service> {
        let hub = hub_with_plugin(
            Arc::clone(plugin) as Arc<dyn UsageCollectorPluginV1>,
            suffix,
            "constructorfabric",
        );
        let resolver = CountingPermitResolver::new(
            pep_properties::OWNER_TENANT_ID,
            Uuid::from_u128(2).to_string(),
        );
        let enforcer = enforcer_for(Arc::clone(&resolver) as _);
        Arc::new(Service::new(hub, "constructorfabric".to_owned(), enforcer))
    }

    fn bounded_window() -> toolkit_odata::ODataQuery {
        let expr = toolkit_odata::parse_filter_string(
            "created_at ge 2026-01-01T00:00:00Z and created_at lt 2026-02-01T00:00:00Z",
        )
        .expect("filter parses")
        .into_expr();
        toolkit_odata::ODataQuery::from(Some(expr))
    }

    async fn run(
        kind: UsageKind,
        op: AggregationOp,
    ) -> Result<AggregationResult, UsageCollectorError> {
        let plugin = HappyPathPlugin::new();
        plugin.set_get_usage_type(usage_type(kind));
        plugin.set_query_aggregated_usage_records_response(AggregationResult { buckets: vec![] });
        let service = service_with_plugin(&plugin, "test.aggregate.opkind.guard.v1");
        service
            .query_aggregated_usage_records(
                &authenticated_ctx(),
                gts_id(),
                &bounded_window(),
                &[],
                spec(op),
            )
            .await
    }

    fn assert_op_not_allowed(result: Result<AggregationResult, UsageCollectorError>) {
        match result {
            Err(UsageCollectorError::InvalidArgument { reason, .. }) => {
                assert_eq!(reason, ValidationReason::OpNotAllowedForKind);
            }
            other => panic!("expected OP_NOT_ALLOWED_FOR_KIND 400, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn sum_on_gauge_is_rejected() {
        assert_op_not_allowed(run(UsageKind::Gauge, AggregationOp::Sum).await);
    }

    #[tokio::test]
    async fn min_max_avg_on_counter_are_rejected() {
        for op in [AggregationOp::Min, AggregationOp::Max, AggregationOp::Avg] {
            assert_op_not_allowed(run(UsageKind::Counter, op).await);
        }
    }

    #[tokio::test]
    async fn allowed_pairs_dispatch_to_plugin() {
        // COUNT on both kinds; SUM on counter; MIN/MAX/AVG on gauge → dispatched.
        for (kind, op) in [
            (UsageKind::Counter, AggregationOp::Sum),
            (UsageKind::Counter, AggregationOp::Count),
            (UsageKind::Gauge, AggregationOp::Count),
            (UsageKind::Gauge, AggregationOp::Min),
            (UsageKind::Gauge, AggregationOp::Max),
            (UsageKind::Gauge, AggregationOp::Avg),
        ] {
            run(kind, op)
                .await
                .unwrap_or_else(|e| panic!("({kind:?}, {op:?}) MUST dispatch, got {e:?}"));
        }
    }

    // Build an `AggregationResult` with exactly `n` empty-key buckets. Mirrors
    // what the plugin's `LIMIT MAX_AGGREGATION_BUCKETS + 1` produces on an
    // over-cardinality `group_by`.
    fn result_with_buckets(n: usize) -> AggregationResult {
        AggregationResult {
            buckets: (0..n)
                .map(|_| AggregationBucket {
                    key: Vec::new(),
                    value: None,
                })
                .collect(),
        }
    }

    async fn run_with_buckets(
        n: usize,
        suffix: &str,
    ) -> Result<AggregationResult, UsageCollectorError> {
        let plugin = HappyPathPlugin::new();
        plugin.set_get_usage_type(usage_type(UsageKind::Counter));
        plugin.set_query_aggregated_usage_records_response(result_with_buckets(n));
        let service = service_with_plugin(&plugin, suffix);
        service
            .query_aggregated_usage_records(
                &authenticated_ctx(),
                gts_id(),
                &bounded_window(),
                &[],
                spec(AggregationOp::Count),
            )
            .await
    }

    #[tokio::test]
    async fn over_cap_bucket_count_is_rejected() {
        // One bucket over the cap — exactly what the plugin's `LIMIT cap + 1`
        // yields on overflow — must lift to a client-fixable 400, not a page.
        match run_with_buckets(
            MAX_AGGREGATION_BUCKETS + 1,
            "test.aggregate.overcap.guard.v1",
        )
        .await
        {
            Err(UsageCollectorError::InvalidArgument { reason, .. }) => {
                assert_eq!(reason, ValidationReason::AggregationResultTooLarge);
            }
            other => panic!("expected AGGREGATION_RESULT_TOO_LARGE 400, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn at_cap_bucket_count_is_allowed() {
        // Exactly at the cap is the boundary — `>` must not reject it.
        let result = run_with_buckets(MAX_AGGREGATION_BUCKETS, "test.aggregate.atcap.guard.v1")
            .await
            .expect("a result exactly at the cap must be returned");
        assert_eq!(result.buckets.len(), MAX_AGGREGATION_BUCKETS);
    }

    #[tokio::test]
    async fn unregistered_gts_id_is_not_found_before_dispatch() {
        let plugin = HappyPathPlugin::new();
        plugin.set_get_usage_type_not_found(gts_id());
        let service = service_with_plugin(&plugin, "test.aggregate.notfound.guard.v1");
        let result = service
            .query_aggregated_usage_records(
                &authenticated_ctx(),
                gts_id(),
                &bounded_window(),
                &[],
                spec(AggregationOp::Sum),
            )
            .await;
        assert!(
            matches!(result, Err(UsageCollectorError::NotFound { .. })),
            "unregistered gts_id MUST surface as a pre-dispatch 404, got {result:?}",
        );
    }
}
