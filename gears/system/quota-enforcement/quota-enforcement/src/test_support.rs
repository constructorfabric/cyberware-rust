//! Shared fakes for the gear's unit tests: PDP doubles, a recording metrics
//! sink, plugin fixtures that stand in for registered plugin instances, and a
//! cluster wired over the standalone backend.

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
use cluster::{ClusterHandle, ClusterWiring, ProfileBackends};
use cluster_sdk::{
    ClusterError, ClusterProfile, ElectionConfig, LeaderElectionBackend, LeaderElectionFeatures,
    LeaderStatus, LeaderWatch,
};
use quota_enforcement_sdk::testing::InMemoryStorage;
use quota_enforcement_sdk::{
    QuotaEnforcementStoragePluginSpecV1, QuotaEnforcementStoragePluginV1, TenantId,
};
use serde_json::json;
use standalone_cluster_plugin::{StandaloneClusterHandle, StandaloneClusterPlugin};
use tokio_util::sync::CancellationToken;
use toolkit::client_hub::{ClientHub, ClientScope};
use toolkit::gts::PluginV1;
use toolkit_canonical_errors::CanonicalError;
use toolkit_security::{PlatformSecurityContext, SecurityContext, pep_properties};
use types_registry_sdk::testing::{MockTypesRegistryClient, make_test_instance};
use types_registry_sdk::{GtsInstance, TypesRegistryClient};
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::ports::coordination::{
    CoordinatorBinding, LeaderWork, SingletonCoordinator, SingletonScope,
};
use crate::domain::ports::metrics::{DenialReason, QeMetrics};
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
