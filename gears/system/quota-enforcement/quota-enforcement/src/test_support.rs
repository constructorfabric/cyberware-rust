//! Shared fakes for the gear's unit tests: PDP doubles, a recording metrics
//! sink, and plugin fixtures that stand in for registered plugin instances.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::missing_panics_doc,
    dead_code,
    reason = "test support"
)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use authz_resolver_sdk::AuthZResolverApi;
use authz_resolver_sdk::constraints::{Constraint, InPredicate, Predicate};
use authz_resolver_sdk::models::{
    DenyReason, EvaluationRequest, EvaluationResponse, EvaluationResponseContext,
};
use quota_enforcement_sdk::testing::{InMemoryCoordination, InMemoryStorage};
use quota_enforcement_sdk::{
    CoordinationPluginV1, QuotaEnforcementCoordinationPluginSpecV1,
    QuotaEnforcementStoragePluginSpecV1, QuotaEnforcementStoragePluginV1, TenantId,
};
use serde_json::json;
use toolkit::client_hub::{ClientHub, ClientScope};
use toolkit::gts::PluginV1;
use toolkit_canonical_errors::CanonicalError;
use toolkit_security::{PlatformSecurityContext, SecurityContext, pep_properties};
use types_registry_sdk::testing::{MockTypesRegistryClient, make_test_instance};
use types_registry_sdk::{GtsInstance, TypesRegistryClient};
use uuid::Uuid;

use crate::domain::ports::metrics::{DenialReason, QeMetrics};

/// The tenant every fixture belongs to.
pub fn tenant() -> TenantId {
    TenantId::new(Uuid::from_u128(0x7e57_0000_0000_0000_0000_0000_0000_0001))
}

/// An authenticated service principal in the fixture tenant.
pub fn ctx() -> SecurityContext {
    SecurityContext::builder()
        .subject_id(Uuid::from_u128(0x5eed))
        .subject_tenant_id(tenant().as_uuid())
        .build()
        .expect("test security context")
}

// ---------------------------------------------------------------------------
// PDP doubles
// ---------------------------------------------------------------------------

/// Permits every request with an `owner_tenant_id IN (tenants)` constraint.
pub struct PermitTenantsPdp {
    tenants: Vec<Uuid>,
    calls: AtomicUsize,
    last_resource_id: Mutex<Option<String>>,
}

impl PermitTenantsPdp {
    pub fn new(tenants: Vec<Uuid>) -> Self {
        Self {
            tenants,
            calls: AtomicUsize::new(0),
            last_resource_id: Mutex::new(None),
        }
    }

    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    pub fn last_resource_id(&self) -> Option<String> {
        self.last_resource_id.lock().expect("lock").clone()
    }
}

#[async_trait]
impl AuthZResolverApi for PermitTenantsPdp {
    async fn evaluate(
        &self,
        _ctx: PlatformSecurityContext,
        request: EvaluationRequest,
    ) -> Result<EvaluationResponse, CanonicalError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        *self.last_resource_id.lock().expect("lock") = request.resource.id.map(|id| id.to_string());
        Ok(EvaluationResponse {
            decision: true,
            context: EvaluationResponseContext {
                constraints: vec![Constraint {
                    predicates: vec![Predicate::In(InPredicate::new(
                        pep_properties::OWNER_TENANT_ID,
                        self.tenants.clone(),
                    ))],
                }],
                ..EvaluationResponseContext::default()
            },
        })
    }
}

/// Permits without any constraint. Under `require_constraints` the PEP must
/// fail closed on it.
pub struct PermitUnconstrainedPdp;

#[async_trait]
impl AuthZResolverApi for PermitUnconstrainedPdp {
    async fn evaluate(
        &self,
        _ctx: PlatformSecurityContext,
        _request: EvaluationRequest,
    ) -> Result<EvaluationResponse, CanonicalError> {
        Ok(EvaluationResponse {
            decision: true,
            context: EvaluationResponseContext::default(),
        })
    }
}

/// Denies every request.
pub struct DenyAllPdp;

#[async_trait]
impl AuthZResolverApi for DenyAllPdp {
    async fn evaluate(
        &self,
        _ctx: PlatformSecurityContext,
        _request: EvaluationRequest,
    ) -> Result<EvaluationResponse, CanonicalError> {
        Ok(EvaluationResponse {
            decision: false,
            context: EvaluationResponseContext {
                constraints: Vec::new(),
                deny_reason: Some(DenyReason {
                    error_code: "NO_GRANT".to_owned(),
                    details: None,
                }),
            },
        })
    }
}

/// Unreachable PDP.
pub struct FailingPdp;

#[async_trait]
impl AuthZResolverApi for FailingPdp {
    async fn evaluate(
        &self,
        _ctx: PlatformSecurityContext,
        _request: EvaluationRequest,
    ) -> Result<EvaluationResponse, CanonicalError> {
        Err(CanonicalError::internal("PDP unavailable").create())
    }
}

/// Registers a PDP double as the `authz-resolver` client.
pub fn register_pdp(hub: &Arc<ClientHub>, pdp: Arc<dyn AuthZResolverApi>) {
    hub.register::<dyn AuthZResolverApi>(pdp);
}

// ---------------------------------------------------------------------------
// Metrics double
// ---------------------------------------------------------------------------

/// Records every denial reason in order.
#[derive(Default)]
pub struct RecordingMetrics {
    denials: Mutex<Vec<DenialReason>>,
}

impl RecordingMetrics {
    pub fn denials(&self) -> Vec<DenialReason> {
        self.denials.lock().expect("lock").clone()
    }
}

impl QeMetrics for RecordingMetrics {
    fn record_denial(&self, reason: DenialReason) {
        self.denials.lock().expect("lock").push(reason);
    }
}

// ---------------------------------------------------------------------------
// Plugin fixtures
// ---------------------------------------------------------------------------

/// A registered plugin instance as the types registry would list it.
pub struct PluginFixture {
    /// Full GTS instance id.
    pub instance_id: String,
    /// Registry entity.
    pub entity: GtsInstance,
}

/// A storage plugin instance.
pub fn storage_instance(segment: &str, vendor: &str, priority: i16) -> PluginFixture {
    let (id, payload) = PluginV1::<QuotaEnforcementStoragePluginSpecV1>::build_registration(
        segment, vendor, priority,
    )
    .expect("registration payload");
    PluginFixture {
        instance_id: id.to_string(),
        entity: make_test_instance(id.as_ref(), payload),
    }
}

/// A coordination plugin instance.
pub fn coordination_instance(segment: &str, vendor: &str, priority: i16) -> PluginFixture {
    let (id, payload) = PluginV1::<QuotaEnforcementCoordinationPluginSpecV1>::build_registration(
        segment, vendor, priority,
    )
    .expect("registration payload");
    PluginFixture {
        instance_id: id.to_string(),
        entity: make_test_instance(id.as_ref(), payload),
    }
}

impl PluginFixture {
    /// A storage instance whose content is not a plugin spec.
    pub fn malformed_storage(segment: &str) -> Self {
        let (id, _) =
            PluginV1::<QuotaEnforcementStoragePluginSpecV1>::build_registration(segment, "acme", 1)
                .expect("registration payload");
        let broken = json!({ "id": id.to_string(), "priority": "not-a-number" });
        Self {
            instance_id: id.to_string(),
            entity: make_test_instance(id.as_ref(), broken),
        }
    }
}

/// A hub whose types registry lists `fixtures`.
pub fn hub_with(fixtures: &[&PluginFixture]) -> Arc<ClientHub> {
    let registry =
        MockTypesRegistryClient::new().with_instances(fixtures.iter().map(|f| f.entity.clone()));
    let hub = Arc::new(ClientHub::new());
    let client: Arc<dyn TypesRegistryClient> = Arc::new(registry);
    hub.register::<dyn TypesRegistryClient>(client);
    hub
}

/// A hub whose types registry fails every listing with `err`.
pub fn hub_with_failing_registry(err: CanonicalError) -> Arc<ClientHub> {
    let registry = MockTypesRegistryClient::new().with_list_error(err);
    let hub = Arc::new(ClientHub::new());
    let client: Arc<dyn TypesRegistryClient> = Arc::new(registry);
    hub.register::<dyn TypesRegistryClient>(client);
    hub
}

/// Registers a storage double as the scoped client of `fixture`.
pub fn register_storage(
    hub: &Arc<ClientHub>,
    fixture: &PluginFixture,
    storage: Arc<InMemoryStorage>,
) {
    let api: Arc<dyn QuotaEnforcementStoragePluginV1> = storage;
    hub.register_scoped::<dyn QuotaEnforcementStoragePluginV1>(
        ClientScope::gts_id(&fixture.instance_id),
        api,
    );
}

/// Registers a coordination double as the scoped client of `fixture`.
pub fn register_coordination(
    hub: &Arc<ClientHub>,
    fixture: &PluginFixture,
    coordination: Arc<InMemoryCoordination>,
) {
    let api: Arc<dyn CoordinationPluginV1> = coordination;
    hub.register_scoped::<dyn CoordinationPluginV1>(ClientScope::gts_id(&fixture.instance_id), api);
}
