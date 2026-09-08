//! Shared fakes for the gear's unit tests: PDP doubles, a recording metrics
//! sink, plugin fixtures that stand in for registered plugin instances, a
//! cluster wired over the standalone backend, and the projection contract
//! fixtures: the `llm_gateway` owner set of ADR-0007, a real in-process
//! `types-registry`, and an in-memory `ContractRegistry` fake for domain tests.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::missing_panics_doc,
    dead_code,
    reason = "test support"
)]

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use authz_resolver_sdk::AuthZResolverApi;
use authz_resolver_sdk::constraints::{
    Constraint, InPredicate, InTenantSubtreePredicate, Predicate,
};
use authz_resolver_sdk::models::{
    DenyReason, EvaluationRequest, EvaluationResponse, EvaluationResponseContext, Resource,
};
use cluster::{ClusterHandle, ClusterWiring, ProfileBackends};
use cluster_sdk::{
    ClusterError, ClusterProfile, ElectionConfig, LeaderElectionBackend, LeaderElectionFeatures,
    LeaderStatus, LeaderWatch,
};
use gts::{GTS_ID_URI_PREFIX, GtsId, GtsInstanceId, GtsStore, GtsTypeId};
use quota_enforcement_sdk::testing::InMemoryStorage;
use quota_enforcement_sdk::{
    METRIC_BASE_TYPE, OwnedDefinition, QuotaEnforcementStoragePluginSpecV1,
    QuotaEnforcementStoragePluginV1, SCOPE_TENANT, SCOPE_TYPE, SCOPE_USER, TenantId,
    owned_definitions,
};
use serde_json::{Value, json};
use standalone_cluster_plugin::{StandaloneClusterHandle, StandaloneClusterPlugin};
use tokio_util::sync::CancellationToken;
use toolkit::client_hub::{ClientHub, ClientScope};
use toolkit::gts::PluginV1;
use toolkit_canonical_errors::CanonicalError;
use toolkit_security::{PlatformSecurityContext, SecurityContext, pep_properties};
use types_registry::config::TypesRegistryConfig;
use types_registry::domain::TypesRegistryService;
use types_registry::domain::local_client::TypesRegistryLocalClient;
use types_registry::infra::InMemoryGtsRepository;
use types_registry_sdk::testing::{MockTypesRegistryClient, make_test_instance};
use types_registry_sdk::{GtsInstance, GtsTypeSchema, RegisterResult, TypesRegistryClient};
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::ports::contracts::{ContractRegistry, DiscoveredType, RegisteredType};
use crate::domain::ports::coordination::{
    CoordinatorBinding, LeaderWork, SingletonCoordinator, SingletonScope,
};
use crate::domain::ports::metrics::{DenialReason, QeMetrics, ValidationReason, ValidationSurface};
use crate::infra::cluster_coordination::QuotaEnforcementProfile;

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
    last_resource: Mutex<Option<Resource>>,
}

impl PermitTenantsPdp {
    pub fn new(tenants: Vec<Uuid>) -> Self {
        Self {
            tenants,
            calls: AtomicUsize::new(0),
            last_resource_id: Mutex::new(None),
            last_resource: Mutex::new(None),
        }
    }

    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    pub fn last_resource_id(&self) -> Option<String> {
        self.last_resource_id.lock().expect("lock").clone()
    }

    /// The `resource` of the last evaluation request, properties included.
    pub fn last_resource(&self) -> Option<Resource> {
        self.last_resource.lock().expect("lock").clone()
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
        *self.last_resource.lock().expect("lock") = Some(request.resource);
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

/// Permits exactly one attribution tuple: every expected resource property must
/// be present with the expected value. Anything else is denied, so a changed
/// tenant, subject, metric, or resource is `PermissionDenied`.
pub struct TupleMatchingPdp {
    expected: serde_json::Map<String, Value>,
    tenants: Vec<Uuid>,
    calls: AtomicUsize,
}

impl TupleMatchingPdp {
    pub fn new(expected: serde_json::Map<String, Value>, tenants: Vec<Uuid>) -> Self {
        Self {
            expected,
            tenants,
            calls: AtomicUsize::new(0),
        }
    }

    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl AuthZResolverApi for TupleMatchingPdp {
    async fn evaluate(
        &self,
        _ctx: PlatformSecurityContext,
        request: EvaluationRequest,
    ) -> Result<EvaluationResponse, CanonicalError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let matches = self
            .expected
            .iter()
            .all(|(key, value)| request.resource.properties.get(key) == Some(value));
        if !matches {
            return Ok(EvaluationResponse {
                decision: false,
                context: EvaluationResponseContext {
                    constraints: Vec::new(),
                    deny_reason: Some(DenyReason {
                        error_code: "TUPLE_NOT_AUTHORIZED".to_owned(),
                        details: None,
                    }),
                },
            });
        }
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

/// Permits every request with an `owner_tenant_id IN SUBTREE(root)` constraint,
/// the shape a hierarchy-aware PDP returns. The filter has no literal values in
/// memory; only `SecureConn` can evaluate it.
pub struct PermitSubtreePdp {
    root: Uuid,
}

impl PermitSubtreePdp {
    pub fn new(root: Uuid) -> Self {
        Self { root }
    }
}

#[async_trait]
impl AuthZResolverApi for PermitSubtreePdp {
    async fn evaluate(
        &self,
        _ctx: PlatformSecurityContext,
        _request: EvaluationRequest,
    ) -> Result<EvaluationResponse, CanonicalError> {
        Ok(EvaluationResponse {
            decision: true,
            context: EvaluationResponseContext {
                constraints: vec![Constraint {
                    predicates: vec![Predicate::InTenantSubtree(InTenantSubtreePredicate::new(
                        pep_properties::OWNER_TENANT_ID,
                        self.root,
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

/// A PDP that never answers.
pub struct HangingPdp;

#[async_trait]
impl AuthZResolverApi for HangingPdp {
    async fn evaluate(
        &self,
        _ctx: PlatformSecurityContext,
        _request: EvaluationRequest,
    ) -> Result<EvaluationResponse, CanonicalError> {
        std::future::pending().await
    }
}

/// Registers a PDP double as the `authz-resolver` client.
pub fn register_pdp(hub: &Arc<ClientHub>, pdp: Arc<dyn AuthZResolverApi>) {
    hub.register::<dyn AuthZResolverApi>(pdp);
}

// ---------------------------------------------------------------------------
// Metrics double
// ---------------------------------------------------------------------------

/// Records every emission in order.
#[derive(Default)]
pub struct RecordingMetrics {
    denials: Mutex<Vec<DenialReason>>,
    contract_failures: Mutex<Vec<(ValidationSurface, ValidationReason)>>,
    admitted_violations: Mutex<Vec<ValidationSurface>>,
}

impl RecordingMetrics {
    pub fn denials(&self) -> Vec<DenialReason> {
        self.denials.lock().expect("lock").clone()
    }

    /// `contract_validation_failures_total` emissions as `(surface, reason)`.
    pub fn contract_failures(&self) -> Vec<(ValidationSurface, ValidationReason)> {
        self.contract_failures.lock().expect("lock").clone()
    }

    /// `admitted_metric_violations_total` emissions by surface.
    pub fn admitted_violations(&self) -> Vec<ValidationSurface> {
        self.admitted_violations.lock().expect("lock").clone()
    }
}

impl QeMetrics for RecordingMetrics {
    fn record_denial(&self, reason: DenialReason) {
        self.denials.lock().expect("lock").push(reason);
    }

    fn record_contract_validation_failure(
        &self,
        surface: ValidationSurface,
        reason: ValidationReason,
    ) {
        self.contract_failures
            .lock()
            .expect("lock")
            .push((surface, reason));
    }

    fn record_admitted_metric_violation(&self, surface: ValidationSurface) {
        self.admitted_violations.lock().expect("lock").push(surface);
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

// ---------------------------------------------------------------------------
// Coordination doubles (domain tests)
// ---------------------------------------------------------------------------

/// A coordinator that never leads: it returns once `shutdown` fires.
pub struct NoopCoordinator;

#[async_trait]
impl SingletonCoordinator for NoopCoordinator {
    async fn run_while_leader(
        &self,
        _scope: SingletonScope,
        shutdown: CancellationToken,
        _work: LeaderWork,
    ) -> Result<(), DomainError> {
        shutdown.cancelled().await;
        Ok(())
    }
}

/// A binding that resolves to [`NoopCoordinator`], or fails with the injected
/// error. Counts the resolve calls.
pub struct StaticCoordinatorBinding {
    failure: Option<DomainError>,
    calls: AtomicUsize,
}

impl StaticCoordinatorBinding {
    /// Resolves successfully.
    pub fn ok() -> Arc<Self> {
        Arc::new(Self {
            failure: None,
            calls: AtomicUsize::new(0),
        })
    }

    /// Fails every resolve with `err`.
    pub fn failing(err: DomainError) -> Arc<Self> {
        Arc::new(Self {
            failure: Some(err),
            calls: AtomicUsize::new(0),
        })
    }

    /// Number of resolve calls.
    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl CoordinatorBinding for StaticCoordinatorBinding {
    async fn resolve(&self) -> Result<Arc<dyn SingletonCoordinator>, DomainError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        match &self.failure {
            Some(err) => Err(err.clone()),
            None => Ok(Arc::new(NoopCoordinator)),
        }
    }
}

/// A sweep body that does nothing and returns at once.
pub fn idle_work() -> LeaderWork {
    Arc::new(|_token: CancellationToken| Box::pin(async {}))
}

// ---------------------------------------------------------------------------
// Cluster fixture (infra and gear tests)
// ---------------------------------------------------------------------------

/// A cluster wired into a hub over the standalone backend. Stop it at the end
/// of the test: the cluster handle panics in debug builds when dropped without
/// `stop()`.
pub struct ClusterFixture {
    cluster: ClusterHandle,
    standalone: StandaloneClusterHandle,
}

impl ClusterFixture {
    /// Deregisters the backends and stops the standalone sweeper.
    pub async fn stop(self) {
        self.cluster.stop().await;
        self.standalone.stop().await;
    }
}

/// A profile the gear never resolves.
#[derive(Debug, Clone, Copy)]
pub struct OtherProfile;

impl ClusterProfile for OtherProfile {
    const NAME: &'static str = "other";
}

/// Wires the standalone cache under the `quota-enforcement` profile. Leader
/// election is the SDK default over that cache, which is linearizable.
pub fn wire_cluster(hub: &Arc<ClientHub>) -> ClusterFixture {
    wire_cluster_with(hub, QuotaEnforcementProfile, None)
}

/// Wires the standalone cache under `profile`, with an explicit leader-election
/// backend when `leader` is given.
pub fn wire_cluster_with<P: ClusterProfile>(
    hub: &Arc<ClientHub>,
    profile: P,
    leader: Option<Arc<dyn LeaderElectionBackend>>,
) -> ClusterFixture {
    let standalone = StandaloneClusterPlugin::builder()
        .build_and_start()
        .expect("standalone cluster backend");
    let mut backends = ProfileBackends::new(standalone.cache());
    if let Some(leader) = leader {
        backends = backends.with_leader_election(leader);
    }
    let cluster = ClusterWiring::builder(hub.clone())
        .profile(profile, backends)
        .build_and_start()
        .expect("cluster wiring");
    ClusterFixture {
        cluster,
        standalone,
    }
}

/// A leader-election backend that declares no linearizable election. It never
/// elects anyone; it exists to fail the `Linearizable` requirement at resolve.
pub struct AdvisoryOnlyLeader;

#[async_trait]
impl LeaderElectionBackend for AdvisoryOnlyLeader {
    fn features(&self) -> LeaderElectionFeatures {
        LeaderElectionFeatures::new(false)
    }

    async fn elect(&self, _name: &str) -> Result<LeaderWatch, ClusterError> {
        let (_sender, _resigns, watch) = LeaderWatch::channel(1, LeaderStatus::Follower);
        Ok(watch)
    }

    async fn elect_with_config(
        &self,
        name: &str,
        _config: ElectionConfig,
    ) -> Result<LeaderWatch, ClusterError> {
        self.elect(name).await
    }
}

// ---------------------------------------------------------------------------
// Projection contract fixtures (projection-contracts feature)
// ---------------------------------------------------------------------------

/// The JSON Schema dialect every GTS type declares.
pub const DRAFT7: &str = "http://json-schema.org/draft-07/schema#";

/// The `llm_gateway` owner set of ADR-0007 (`docs/schemas/examples`).
pub const LLM_USER_PROJECTION: &str = "gts.cf.core.qe.subj.v1~cf.genai.llm_gateway.user.v1~";
pub const LLM_TENANT_PROJECTION: &str = "gts.cf.core.qe.subj.v1~cf.genai.llm_gateway.tenant.v1~";
pub const LLM_TOKEN_REQUEST: &str = "gts.cf.core.qe.request.v1~cf.genai.llm_gateway.token.v1~";
pub const LLM_REQUEST_COUNT_REQUEST: &str =
    "gts.cf.core.qe.request.v1~cf.genai.llm_gateway.request_count.v1~";
pub const LLM_TOKEN_CONSTRAINT: &str =
    "gts.cf.core.qe.constraint.v1~cf.genai.llm_gateway.token_constraint.v1~";
pub const LLM_REQUEST_COUNT_CONSTRAINT: &str =
    "gts.cf.core.qe.constraint.v1~cf.genai.llm_gateway.request_count_constraint.v1~";
pub const LLM_MODEL_RESOURCE: &str = "gts.cf.core.qe.res.v1~cf.genai.llm_gateway.model.v1~";

/// The metrics the `llm_gateway` set admits, plus one it does not.
pub const METRIC_TOKENS: &str = "gts.cf.qe.metric.type.v1~cf.qe.metric.ai_tokens_input.v1";
pub const METRIC_REQUESTS: &str = "gts.cf.qe.metric.type.v1~cf.qe.metric.ai_requests.v1";
pub const METRIC_OTHER: &str = "gts.cf.qe.metric.type.v1~cf.qetest.metric.other.v1";

/// Test-only contracts exercising references and GTS-typed fields.
pub const MIXIN_TYPE: &str = "gts.cf.qetest.shared.region.v1~";
pub const MIXIN_REQUEST: &str = "gts.cf.core.qe.request.v1~cf.qetest.llm.mixin.v1~";
pub const TYPED_REQUEST: &str = "gts.cf.core.qe.request.v1~cf.qetest.llm.typed.v1~";
pub const TYPED_MODEL_BASE: &str = "gts.cf.qetest.models.model.v1~";

const EXAMPLE_DOCUMENTS: [&str; 7] = [
    include_str!(
        "../../docs/schemas/examples/gts.cf.core.qe.subj.v1~cf.genai.llm_gateway.user.v1~.schema.json"
    ),
    include_str!(
        "../../docs/schemas/examples/gts.cf.core.qe.subj.v1~cf.genai.llm_gateway.tenant.v1~.schema.json"
    ),
    include_str!(
        "../../docs/schemas/examples/gts.cf.core.qe.request.v1~cf.genai.llm_gateway.token.v1~.schema.json"
    ),
    include_str!(
        "../../docs/schemas/examples/gts.cf.core.qe.request.v1~cf.genai.llm_gateway.request_count.v1~.schema.json"
    ),
    include_str!(
        "../../docs/schemas/examples/gts.cf.core.qe.constraint.v1~cf.genai.llm_gateway.token_constraint.v1~.schema.json"
    ),
    include_str!(
        "../../docs/schemas/examples/gts.cf.core.qe.constraint.v1~cf.genai.llm_gateway.request_count_constraint.v1~.schema.json"
    ),
    include_str!(
        "../../docs/schemas/examples/gts.cf.core.qe.res.v1~cf.genai.llm_gateway.model.v1~.schema.json"
    ),
];

/// The seven `llm_gateway` documents.
pub fn llm_gateway_documents() -> Vec<Value> {
    EXAMPLE_DOCUMENTS
        .iter()
        .map(|raw| serde_json::from_str(raw).expect("reviewed example parses"))
        .collect()
}

/// A permissive test-only metric base plus the two `llm_gateway` metrics. The
/// platform base is registry-owned and defined nowhere in code yet.
pub fn metric_base_documents() -> Vec<Value> {
    vec![
        json!({
            "$id": format!("{GTS_ID_URI_PREFIX}{METRIC_BASE_TYPE}"),
            "$schema": DRAFT7,
            "description": "Test-only stand-in for the platform metric base.",
            "type": "object",
            "properties": {
                "id": { "type": "string" },
                "type": { "type": "string" }
            },
            "required": ["id", "type"]
        }),
        metric_instance_document(METRIC_TOKENS),
        metric_instance_document(METRIC_REQUESTS),
    ]
}

/// A metric instance under the test-only metric base.
pub fn metric_instance_document(id: &str) -> Value {
    json!({ "id": id, "type": METRIC_BASE_TYPE })
}

/// A root type other contracts mix in through a `gts://` reference.
pub fn mixin_type_document() -> Value {
    json!({
        "$id": format!("{GTS_ID_URI_PREFIX}{MIXIN_TYPE}"),
        "$schema": DRAFT7,
        "type": "object",
        "properties": {
            "region": { "type": "string", "enum": ["eu", "us"] }
        },
        "required": ["region"]
    })
}

/// A request contract whose metadata combines a mixin `gts://` reference and a
/// local `#/definitions` reference.
pub fn mixin_request_document(metric: &str) -> Value {
    json!({
        "$id": format!("{GTS_ID_URI_PREFIX}{MIXIN_REQUEST}"),
        "$schema": DRAFT7,
        "x-gts-traits": { "metric": metric, "constraint_contract": LLM_TOKEN_CONSTRAINT },
        "type": "object",
        "definitions": {
            "Timing": {
                "type": "object",
                "properties": { "when": { "type": "string", "format": "date-time" } }
            }
        },
        "allOf": [
            { "$ref": format!("{GTS_ID_URI_PREFIX}gts.cf.core.qe.request.v1~") },
            {
                "type": "object",
                "properties": {
                    "metadata": {
                        "allOf": [
                            { "$ref": format!("{GTS_ID_URI_PREFIX}{MIXIN_TYPE}") },
                            { "$ref": "#/definitions/Timing" }
                        ]
                    }
                }
            }
        ]
    })
}

/// A request contract whose metadata declares a GTS-typed field narrowed by
/// `x-gts-ref` and a `date-time` field.
pub fn typed_request_document(metric: &str) -> Value {
    json!({
        "$id": format!("{GTS_ID_URI_PREFIX}{TYPED_REQUEST}"),
        "$schema": DRAFT7,
        "x-gts-traits": { "metric": metric, "constraint_contract": LLM_TOKEN_CONSTRAINT },
        "type": "object",
        "allOf": [
            { "$ref": format!("{GTS_ID_URI_PREFIX}gts.cf.core.qe.request.v1~") },
            {
                "type": "object",
                "properties": {
                    "metadata": {
                        "type": "object",
                        "additionalProperties": false,
                        "properties": {
                            "model": {
                                "type": "string",
                                "format": "gts-instance-id",
                                "x-gts-ref": TYPED_MODEL_BASE
                            },
                            "when": { "type": "string", "format": "date-time" }
                        },
                        "required": ["model"]
                    }
                }
            }
        ]
    })
}

/// The GTS id of a type document (`$id` without the URI prefix) or of an
/// instance document (`id`).
pub fn document_id(document: &Value) -> String {
    document["$id"]
        .as_str()
        .map(|uri| {
            uri.strip_prefix(GTS_ID_URI_PREFIX)
                .unwrap_or(uri)
                .to_owned()
        })
        .or_else(|| document["id"].as_str().map(ToOwned::to_owned))
        .expect("a GTS document carries $id or id")
}

fn owned_type_documents() -> Vec<Value> {
    owned_definitions()
        .expect("owned definitions parse")
        .into_iter()
        .filter(|d| d.id.ends_with('~'))
        .map(|d| d.document)
        .collect()
}

/// `GtsTypeSchema` chains for `documents`, parents first. The QE bases are
/// always included, so callers pass derived contracts and mixins only.
pub fn schema_chain(documents: &[Value]) -> Vec<GtsTypeSchema> {
    let mut all = owned_type_documents();
    all.extend(documents.iter().cloned());
    let mut ordered: Vec<(String, Value)> = all.into_iter().map(|d| (document_id(&d), d)).collect();
    ordered.sort_by_key(|(id, _)| id.matches('~').count());
    let mut built: HashMap<String, Arc<GtsTypeSchema>> = HashMap::new();
    let mut out = Vec::new();
    for (id, document) in ordered {
        if built.contains_key(&id) {
            continue;
        }
        let parent = GtsTypeSchema::derive_parent_type_id(&id)
            .and_then(|pid| built.get(pid.as_ref()).cloned());
        let schema = GtsTypeSchema::try_new(GtsTypeId::new(&id), document, None, parent)
            .expect("fixture chain is well-formed");
        built.insert(id, Arc::new(schema.clone()));
        out.push(schema);
    }
    out
}

/// A read-only SDK mock serving `documents` (types, chains built) and
/// `instances` (instance documents). Its `register*` methods panic.
pub fn mock_registry(documents: &[Value], instances: &[Value]) -> MockTypesRegistryClient {
    MockTypesRegistryClient::new()
        .with_type_schemas(schema_chain(documents))
        .with_instances(
            instances
                .iter()
                .map(|doc| make_test_instance(&document_id(doc), doc.clone())),
        )
}

/// A real in-process `types-registry`, seeded in configuration phase with the
/// process inventory (the QE definitions among it) plus `extra`, then switched
/// to ready mode. Registration, inheritance, and ready-mode validation are the
/// real thing; nothing is mocked.
pub fn in_process_registry(extra: Vec<Value>) -> Arc<dyn TypesRegistryClient> {
    let config = TypesRegistryConfig::default();
    let repository = Arc::new(InMemoryGtsRepository::new(config.to_gts_config()));
    let service = Arc::new(TypesRegistryService::new(repository, config));
    let mut entries = toolkit::gts::all_inventory_type_schemas().expect("inventory type schemas");
    entries.extend(toolkit::gts::all_inventory_instances().expect("inventory instances"));
    entries.extend(extra);
    let results = service.register(entries);
    RegisterResult::ensure_all_ok(&results).expect("fixture entities register");
    service
        .switch_to_ready()
        .expect("fixture registry switches to ready");
    Arc::new(TypesRegistryLocalClient::new(service))
}

/// A hub whose types registry is `registry`, listing `fixtures` on top of
/// whatever `registry` holds is not needed: plugin instances are registered
/// through the same registry.
pub fn hub_with_registry(registry: Arc<dyn TypesRegistryClient>) -> Arc<ClientHub> {
    let hub = Arc::new(ClientHub::new());
    hub.register::<dyn TypesRegistryClient>(registry);
    hub
}

/// The plugin instance document of `fixture` as the registry accepts it.
pub fn plugin_instance_document(fixture: &PluginFixture) -> Value {
    fixture.entity.object.clone()
}

/// Resolves `documents` the way the production adapter does: a local
/// `GtsStore` holding the QE bases plus every document, then `validate_schema`
/// per type document. Instances are skipped.
pub fn resolve_documents(documents: &[Value]) -> Vec<RegisteredType> {
    let mut store = GtsStore::new();
    let mut ids = Vec::new();
    for document in owned_type_documents().iter().chain(documents.iter()) {
        let id = document_id(document);
        if id.ends_with('~') {
            store
                .register_schema(&id, document)
                .expect("fixture type registers");
            ids.push(id);
        }
    }
    ids.into_iter()
        .map(|id| {
            let resolved = store
                .validate_schema(&id)
                .unwrap_or_else(|e| panic!("{id}: {e}"));
            let mut ancestors: Vec<GtsTypeId> = GtsId::try_new(&id)
                .expect("type id")
                .chain_ids()
                .into_iter()
                .filter(|c| *c != id)
                .map(|c| GtsTypeId::new(&c))
                .collect();
            ancestors.reverse();
            RegisteredType {
                id: GtsTypeId::new(&id),
                is_abstract: resolved.is_abstract,
                ancestors,
                effective_traits: resolved.effective_traits,
                schema: resolved.schema,
            }
        })
        .collect()
}

/// An in-memory `ContractRegistry` for domain tests: resolved types, listed but
/// unresolvable types, instances, and failure injection.
#[derive(Default)]
pub struct FakeContractRegistry {
    types: Mutex<HashMap<String, RegisteredType>>,
    /// Listed by `derived_types`, failing in `type_schema`: a contract whose
    /// reference graph is broken.
    broken: Mutex<HashMap<String, DiscoveredType>>,
    instances: Mutex<HashMap<String, GtsTypeId>>,
    registered: Mutex<Vec<Vec<&'static str>>>,
    reads: AtomicUsize,
    fail_all: AtomicBool,
    registration_failure: Mutex<Option<DomainError>>,
}

impl FakeContractRegistry {
    /// Holds nothing.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Holds the QE bases, the `llm_gateway` set, and the two metric instances.
    pub fn llm_gateway() -> Self {
        let fake = Self::empty();
        for registered in resolve_documents(&llm_gateway_documents()) {
            fake.add_type(registered);
        }
        for metric in [METRIC_TOKENS, METRIC_REQUESTS] {
            fake.add_instance(metric, METRIC_BASE_TYPE);
        }
        for scope in [SCOPE_USER, SCOPE_TENANT] {
            fake.add_instance(scope, SCOPE_TYPE);
        }
        fake
    }

    pub fn add_type(&self, registered: RegisteredType) {
        self.types
            .lock()
            .expect("lock")
            .insert(registered.id.as_ref().to_owned(), registered);
    }

    pub fn remove_type(&self, id: &str) {
        self.types.lock().expect("lock").remove(id);
    }

    /// Lists `id` under its base but fails its resolution.
    pub fn add_broken(&self, id: &str, declared_traits: Value) {
        self.broken.lock().expect("lock").insert(
            id.to_owned(),
            DiscoveredType {
                id: GtsTypeId::new(id),
                is_abstract: false,
                declared_traits,
            },
        );
    }

    pub fn add_instance(&self, id: &str, type_id: &str) {
        self.instances
            .lock()
            .expect("lock")
            .insert(id.to_owned(), GtsTypeId::new(type_id));
    }

    pub fn remove_instance(&self, id: &str) {
        self.instances.lock().expect("lock").remove(id);
    }

    /// Number of `ensure_registered` calls.
    pub fn ensure_registered_calls(&self) -> usize {
        self.registered.lock().expect("lock").len()
    }

    /// The ids of the last `ensure_registered` call.
    pub fn last_registered(&self) -> Vec<&'static str> {
        self.registered
            .lock()
            .expect("lock")
            .last()
            .cloned()
            .unwrap_or_default()
    }

    /// Number of read calls (`type_schema`, `derived_types`, `instance_type`).
    pub fn read_calls(&self) -> usize {
        self.reads.load(Ordering::SeqCst)
    }

    /// Every later call fails as unavailable.
    pub fn fail_all(&self) {
        self.fail_all.store(true, Ordering::SeqCst);
    }

    /// `ensure_registered` fails with `err`.
    pub fn fail_registration_with(&self, err: DomainError) {
        *self.registration_failure.lock().expect("lock") = Some(err);
    }

    fn read(&self) -> Result<(), DomainError> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        if self.fail_all.load(Ordering::SeqCst) {
            return Err(DomainError::TypesRegistryUnavailable(
                "fake registry unavailable".to_owned(),
            ));
        }
        Ok(())
    }
}

#[async_trait]
impl ContractRegistry for FakeContractRegistry {
    async fn ensure_registered(&self, definitions: &[OwnedDefinition]) -> Result<(), DomainError> {
        if self.fail_all.load(Ordering::SeqCst) {
            return Err(DomainError::TypesRegistryUnavailable(
                "fake registry unavailable".to_owned(),
            ));
        }
        if let Some(err) = self.registration_failure.lock().expect("lock").clone() {
            return Err(err);
        }
        self.registered
            .lock()
            .expect("lock")
            .push(definitions.iter().map(|d| d.id).collect());
        Ok(())
    }

    async fn type_schema(&self, id: &GtsTypeId) -> Result<Option<RegisteredType>, DomainError> {
        self.read()?;
        if self.broken.lock().expect("lock").contains_key(id.as_ref()) {
            return Err(DomainError::TypesRegistryUnavailable(format!(
                "contract {id} did not resolve: references unregistered type"
            )));
        }
        Ok(self.types.lock().expect("lock").get(id.as_ref()).cloned())
    }

    async fn derived_types(&self, base: &GtsTypeId) -> Result<Vec<DiscoveredType>, DomainError> {
        self.read()?;
        let mut out: Vec<DiscoveredType> = self
            .types
            .lock()
            .expect("lock")
            .values()
            .filter(|t| t.derives_from(base.as_ref()))
            .map(|t| DiscoveredType {
                id: t.id.clone(),
                is_abstract: t.is_abstract,
                declared_traits: t.effective_traits.clone(),
            })
            .collect();
        out.extend(
            self.broken
                .lock()
                .expect("lock")
                .values()
                .filter(|d| d.id.as_ref().starts_with(base.as_ref()))
                .cloned(),
        );
        out.sort_by(|a, b| a.id.as_ref().cmp(b.id.as_ref()));
        Ok(out)
    }

    async fn instance_type(&self, id: &GtsInstanceId) -> Result<Option<GtsTypeId>, DomainError> {
        self.read()?;
        Ok(self
            .instances
            .lock()
            .expect("lock")
            .get(id.as_ref())
            .cloned())
    }
}

/// Ids of the types `documents` define, for assertions.
pub fn type_ids(documents: &[Value]) -> HashSet<String> {
    documents
        .iter()
        .map(document_id)
        .filter(|id| id.ends_with('~'))
        .collect()
}
