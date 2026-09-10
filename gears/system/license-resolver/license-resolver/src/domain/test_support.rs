//! Shared test infrastructure for domain-layer unit tests.
//!
//! The licensing contracts the fakes are exercised against live in
//! [`crate::test_contracts`]; they are re-exported here so a test file needs one
//! import. For the plugin-discovery registry, use `MockTypesRegistryClient` and
//! `make_test_instance` from `types_registry_sdk::testing` directly.

#![allow(clippy::missing_panics_doc)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use gts::GtsTypeId;
use license_resolver_sdk::{
    LicenseCheckContext, LicenseCheckRequest, LicenseDecision, LicenseResolverError,
    LicenseResolverPluginClient, LicenseResolverPluginSpecV1, Resource, Subject,
};
use serde_json::{Map, Value, json};
use toolkit::client_hub::{ClientHub, ClientScope};
use toolkit_macros::domain_model;
use types_registry_sdk::testing::{MockTypesRegistryClient, make_test_instance};
use types_registry_sdk::{GtsTypeSchema, TypesRegistryClient};
use uuid::Uuid;

use super::ports::{
    CheckOutcome, ContractRegistry, ContractRegistryError, LicenseMetrics, ViolationKind,
};
pub use crate::test_contracts::{
    BARE_RESOURCE, MODEL_USAGE_RESOURCE, RESOURCE_BASE, SUBJECT_BASE, TENANT_SUBJECT,
    TestModelUsageResourceV1, USER_SUBJECT, bare_resource_schema, model_usage_schema,
    resource_base_schema, subject_base_schema, tenant_subject_schema, user_subject_schema,
};

#[must_use]
pub fn test_tenant() -> Uuid {
    Uuid::from_u128(0x5eed_0000_0000_0000_0000_0000_0000_0001)
}

fn metadata(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => map,
        other => panic!("contract metadata must be a JSON object, got {other}"),
    }
}

#[must_use]
pub fn subject(type_id: &str, id: Option<&str>, meta: Value) -> Subject {
    Subject {
        gts_type: GtsTypeId::new(type_id),
        id: id.map(ToOwned::to_owned),
        metadata: metadata(meta),
    }
}

#[must_use]
pub fn resource(type_id: &str, id: Option<&str>, meta: Value) -> Resource {
    Resource {
        gts_type: GtsTypeId::new(type_id),
        id: id.map(ToOwned::to_owned),
        metadata: metadata(meta),
    }
}

#[must_use]
pub fn request(subject: Subject, resource: Resource) -> LicenseCheckRequest {
    let context = LicenseCheckContext::builder()
        .tenant_id(test_tenant())
        .build()
        .expect("tenant is set");
    LicenseCheckRequest::new(subject, resource, context)
}

/// A request that conforms to both test contracts.
#[must_use]
pub fn conforming_request() -> LicenseCheckRequest {
    request(
        subject(
            USER_SUBJECT,
            Some("acme-admin"),
            json!({ "category": "internal" }),
        ),
        resource(
            MODEL_USAGE_RESOURCE,
            Some("gpt-4o"),
            json!({ "model_vendor": "openai", "model_name": "gpt-4o" }),
        ),
    )
}

/// How a [`FakeContractRegistry`] lookup fails, when it is set to fail.
#[domain_model]
#[derive(Debug, Clone, Copy)]
pub enum FailureKind {
    /// The id is not a well-formed GTS type id.
    Malformed,
    /// The registry could not answer.
    Unavailable,
}

/// In-memory [`ContractRegistry`], counting lookups.
///
/// An id that was not registered resolves to
/// [`ContractRegistryError::Unregistered`] — the same answer the real adapter
/// derives from a registry `NotFound`.
#[domain_model]
#[derive(Default)]
pub struct FakeContractRegistry {
    contracts: HashMap<String, GtsTypeSchema>,
    always_fails: Option<FailureKind>,
    calls: AtomicUsize,
}

impl FakeContractRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn with_contracts(mut self, items: impl IntoIterator<Item = GtsTypeSchema>) -> Self {
        for schema in items {
            self.contracts
                .insert(schema.type_id.as_ref().to_owned(), schema);
        }
        self
    }

    #[must_use]
    pub fn failing(kind: FailureKind) -> Self {
        Self {
            always_fails: Some(kind),
            ..Self::default()
        }
    }

    /// Registry holding both conforming test contracts.
    #[must_use]
    pub fn with_test_contracts() -> Self {
        Self::new().with_contracts([user_subject_schema(), model_usage_schema()])
    }

    #[must_use]
    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl ContractRegistry for FakeContractRegistry {
    async fn contract_schema(&self, type_id: &str) -> Result<GtsTypeSchema, ContractRegistryError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        match self.always_fails {
            Some(FailureKind::Malformed) => {
                return Err(ContractRegistryError::MalformedTypeId(
                    "missing vendor".to_owned(),
                ));
            }
            Some(FailureKind::Unavailable) => {
                return Err(ContractRegistryError::Unavailable(
                    "registry down".to_owned(),
                ));
            }
            None => {}
        }
        self.contracts
            .get(type_id)
            .cloned()
            .ok_or(ContractRegistryError::Unregistered)
    }
}

/// [`LicenseMetrics`] sink that records everything for assertions.
#[domain_model]
#[derive(Default)]
pub struct RecordingMetrics {
    checks: Mutex<Vec<(String, CheckOutcome)>>,
    latencies: Mutex<Vec<f64>>,
    violations: Mutex<Vec<ViolationKind>>,
}

impl RecordingMetrics {
    #[must_use]
    pub fn checks(&self) -> Vec<(String, CheckOutcome)> {
        self.checks.lock().expect("checks log poisoned").clone()
    }

    /// How many boundary-latency observations were recorded (the histogram
    /// carries no label to assert on).
    #[must_use]
    pub fn latency_count(&self) -> usize {
        self.latencies.lock().expect("latency log poisoned").len()
    }

    #[must_use]
    pub fn violation_kinds(&self) -> Vec<ViolationKind> {
        self.violations
            .lock()
            .expect("violation log poisoned")
            .clone()
    }
}

impl LicenseMetrics for RecordingMetrics {
    fn record_check(&self, contract_type: &str, outcome: CheckOutcome) {
        self.checks
            .lock()
            .expect("checks log poisoned")
            .push((contract_type.to_owned(), outcome));
    }

    fn record_resolver_latency(&self, millis: f64) {
        self.latencies
            .lock()
            .expect("latency log poisoned")
            .push(millis);
    }

    fn record_validation_failure(&self, kind: ViolationKind) {
        self.violations
            .lock()
            .expect("violation log poisoned")
            .push(kind);
    }
}

type PluginFn = Arc<dyn Fn() -> Result<LicenseDecision, LicenseResolverError> + Send + Sync>;

/// Backend plugin fake that records every request it was handed.
#[domain_model]
pub struct MockPlugin {
    handler: PluginFn,
    seen: Mutex<Vec<LicenseCheckRequest>>,
}

impl MockPlugin {
    fn with_handler(handler: PluginFn) -> Arc<Self> {
        Arc::new(Self {
            handler,
            seen: Mutex::new(Vec::new()),
        })
    }

    #[must_use]
    pub fn granting() -> Arc<Self> {
        Self::with_handler(Arc::new(|| {
            Ok(LicenseDecision::new(true).with_diagnostic("backend", "mock"))
        }))
    }

    #[must_use]
    pub fn denying() -> Arc<Self> {
        Self::with_handler(Arc::new(|| Ok(LicenseDecision::new(false))))
    }

    #[must_use]
    pub fn failing(err: LicenseResolverError) -> Arc<Self> {
        Self::with_handler(Arc::new(move || Err(err.clone())))
    }

    /// Requests the plugin received, in call order.
    #[must_use]
    pub fn seen(&self) -> Vec<LicenseCheckRequest> {
        self.seen.lock().expect("seen log poisoned").clone()
    }
}

#[async_trait]
impl LicenseResolverPluginClient for MockPlugin {
    async fn is_licensed(
        &self,
        request: LicenseCheckRequest,
    ) -> Result<LicenseDecision, LicenseResolverError> {
        self.seen.lock().expect("seen log poisoned").push(request);
        (self.handler)()
    }
}

#[must_use]
pub fn empty_hub() -> Arc<ClientHub> {
    Arc::new(ClientHub::default())
}

/// GTS instance id for a license resolver plugin test instance.
#[must_use]
pub fn test_instance_id() -> String {
    format!(
        "{}test.license_resolver.mock.instance.v1",
        LicenseResolverPluginSpecV1::gts_type_id()
    )
}

/// Plugin instance content `choose_plugin_instance` can parse.
#[must_use]
pub fn plugin_content(gts_id: &str, vendor: &str) -> Value {
    json!({ "id": gts_id, "vendor": vendor, "priority": 0, "properties": {} })
}

/// Hub carrying a types-registry holding one plugin instance, plus the scoped
/// plugin client itself.
#[must_use]
pub fn hub_with_registry_and_plugin(
    instance_id: &str,
    vendor: &str,
    plugin: Arc<dyn LicenseResolverPluginClient>,
) -> Arc<ClientHub> {
    let hub = counting_hub_with_registry_only(instance_id, vendor).0;
    hub.register_scoped::<dyn LicenseResolverPluginClient>(
        ClientScope::gts_id(instance_id),
        plugin,
    );
    hub
}

/// Hub whose types-registry advertises a plugin instance that never registered
/// its scoped client, plus the registry mock so a test can assert how often
/// plugin discovery ran.
#[must_use]
pub fn counting_hub_with_registry_only(
    instance_id: &str,
    vendor: &str,
) -> (Arc<ClientHub>, Arc<MockTypesRegistryClient>) {
    let hub = empty_hub();
    let instance = make_test_instance(instance_id, plugin_content(instance_id, vendor));
    let registry = Arc::new(MockTypesRegistryClient::new().with_instances([instance]));
    hub.register::<dyn TypesRegistryClient>(registry.clone() as Arc<dyn TypesRegistryClient>);
    (hub, registry)
}

#[must_use]
pub fn hub_without_plugin_instances() -> Arc<ClientHub> {
    let hub = empty_hub();
    let registry: Arc<dyn TypesRegistryClient> = Arc::new(MockTypesRegistryClient::new());
    hub.register::<dyn TypesRegistryClient>(registry);
    hub
}
