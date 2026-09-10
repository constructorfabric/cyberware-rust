//! Shared test infrastructure for domain-layer unit tests.
//!
//! GTS registry: use `MockTypesRegistryClient` and `make_test_instance` from
//! `types_registry_sdk::testing` directly (gated on the `test-util`
//! dev-dependency feature).
//!
//! PDP: this module exposes `AuthZResolverApi` fakes plus `PolicyEnforcer`
//! constructors that let tests pin every outcome (permit + constraints, deny,
//! empty-constraints fail-closed, transport unreachable, and no-cache via the
//! per-call counting resolvers).
//!
//! Every public helper in this module is test-only — `.lock().expect("…")`
//! on the internal mutexes is fine because a poisoned mutex inside a unit
//! test indicates an unrecoverable test failure anyway. The
//! `missing_panics_doc` lint would force boilerplate `# Panics` sections
//! on every setter/getter; suppressing it module-wide keeps the helpers
//! readable without diluting the production-code expectation.
#![allow(clippy::missing_panics_doc)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use authz_resolver_sdk::constraints::Constraint;
use authz_resolver_sdk::models::{
    EvaluationRequest, EvaluationResponse, EvaluationResponseContext,
};
use authz_resolver_sdk::{AuthZResolverApi, PolicyEnforcer};
use toolkit::api::canonical_prelude::CanonicalError;
use toolkit_odata::{ODataQuery, Page as ODataPage};
use toolkit_security::{PlatformSecurityContext, pep_properties};
use usage_collector_sdk::{
    AggregationResult, AggregationSpec, MetadataFilter, UsageCollectorPluginError,
    UsageCollectorPluginV1, UsageRecord, UsageType, UsageTypeGtsId,
};
use uuid::Uuid;

/// Minimal mock storage-plugin client.
///
/// The `UsageCollectorPluginV1` SPI surface carries nine methods. The mock
/// here exists purely so the Plugin Host can resolve a concrete
/// `Arc<dyn UsageCollectorPluginV1>` from `ClientHub` and so cache tests can
/// assert `Arc::ptr_eq` on the resolved handle. Every method returns a
/// deterministic `Internal("test_fake: …")` so any test that accidentally
/// dispatches through the mock surfaces an obvious failure; downstream
/// features reshape the mock surface when test bodies need richer fakes.
pub struct MockPlugin;

impl MockPlugin {
    /// Returns a fresh mock plugin as a scoped trait object.
    #[must_use]
    pub fn arc() -> Arc<dyn UsageCollectorPluginV1> {
        Arc::new(Self)
    }
}

#[async_trait]
impl UsageCollectorPluginV1 for MockPlugin {
    async fn create_usage_record(
        &self,
        _record: UsageRecord,
    ) -> Result<UsageRecord, UsageCollectorPluginError> {
        Err(UsageCollectorPluginError::internal(
            "test_fake: MockPlugin::create_usage_record not implemented",
        ))
    }

    async fn create_usage_records(
        &self,
        _records: Vec<UsageRecord>,
    ) -> Result<Vec<Result<UsageRecord, UsageCollectorPluginError>>, UsageCollectorPluginError>
    {
        Err(UsageCollectorPluginError::internal(
            "test_fake: MockPlugin::create_usage_records not implemented",
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
            "test_fake: MockPlugin::query_aggregated_usage_records not implemented",
        ))
    }

    async fn list_usage_records(
        &self,
        _gts_id: UsageTypeGtsId,
        _query: &ODataQuery,
        _metadata_filter: &[MetadataFilter],
    ) -> Result<ODataPage<UsageRecord>, UsageCollectorPluginError> {
        Err(UsageCollectorPluginError::internal(
            "test_fake: MockPlugin::list_usage_records not implemented",
        ))
    }

    async fn deactivate_usage_record(&self, _id: Uuid) -> Result<(), UsageCollectorPluginError> {
        Err(UsageCollectorPluginError::internal(
            "test_fake: MockPlugin::deactivate_usage_record not implemented",
        ))
    }

    async fn create_usage_type(
        &self,
        _usage_type: UsageType,
    ) -> Result<UsageType, UsageCollectorPluginError> {
        Err(UsageCollectorPluginError::internal(
            "test_fake: MockPlugin::create_usage_type not implemented",
        ))
    }

    async fn get_usage_type(
        &self,
        _gts_id: UsageTypeGtsId,
    ) -> Result<UsageType, UsageCollectorPluginError> {
        Err(UsageCollectorPluginError::internal(
            "test_fake: MockPlugin::get_usage_type not implemented",
        ))
    }

    async fn list_usage_types(
        &self,
        _query: &ODataQuery,
    ) -> Result<ODataPage<UsageType>, UsageCollectorPluginError> {
        Err(UsageCollectorPluginError::internal(
            "test_fake: MockPlugin::list_usage_types not implemented",
        ))
    }

    async fn delete_usage_type(
        &self,
        _gts_id: UsageTypeGtsId,
    ) -> Result<(), UsageCollectorPluginError> {
        Err(UsageCollectorPluginError::internal(
            "test_fake: MockPlugin::delete_usage_type not implemented",
        ))
    }

    async fn get_usage_record(&self, _id: Uuid) -> Result<UsageRecord, UsageCollectorPluginError> {
        Err(UsageCollectorPluginError::internal(
            "test_fake: MockPlugin::get_usage_record not implemented",
        ))
    }
}

// ── PDP (authz-resolver) mocks ─────────────────────────────────────────────

/// Build a permit `EvaluationResponse` carrying a single
/// `property = value` string-equality constraint. Compiles (against a resource
/// type that lists `property` as supported) to a non-empty `AccessScope`, so it
/// exercises the permit-with-constraints `Ok` path through
/// `require_constraints(true)`.
#[must_use]
pub fn permit_with_string_constraint(property: &'static str, value: String) -> EvaluationResponse {
    use authz_resolver_sdk::constraints::{EqPredicate, Predicate};

    EvaluationResponse {
        decision: true,
        context: EvaluationResponseContext {
            constraints: vec![Constraint {
                predicates: vec![Predicate::Eq(EqPredicate::new(property, value))],
            }],
            deny_reason: None,
        },
    }
}

/// Build a permit `EvaluationResponse` scoped to the request's own
/// `OWNER_TENANT_ID`, simulating a tenant-scoped grant that authorizes
/// exactly the tenant the record under test names.
///
/// The per-record (`usage_record`) authz path runs under
/// `require_constraints(true)` and applies an attribution gate
/// (`authz::scope_admits_attribution_tuple`). A plain empty-constraints permit (e.g.
/// [`CountingAllowAllResolver`]) fails closed as
/// `EnforcerError::CompileFailed` there, and a fixed-tenant constraint (e.g.
/// [`permit_with_string_constraint`]) would only satisfy the gate for one
/// hard-coded tenant. Echoing the request's `OWNER_TENANT_ID` back as the
/// granted scope compiles to a non-empty `AccessScope` AND satisfies the gate
/// for ANY record tenant a test picks. When the request carries no
/// `OWNER_TENANT_ID` (the subject-only catalog surface, which runs under
/// `require_constraints(false)`), it falls back to an empty-constraints
/// `allow_all` permit — the legitimate happy-path there.
#[must_use]
pub fn permit_scoped_to_request_tenant(request: &EvaluationRequest) -> EvaluationResponse {
    match request
        .resource
        .properties
        .get(pep_properties::OWNER_TENANT_ID)
        .and_then(serde_json::Value::as_str)
    {
        Some(tenant) => {
            permit_with_string_constraint(pep_properties::OWNER_TENANT_ID, tenant.to_owned())
        }
        None => EvaluationResponse {
            decision: true,
            context: EvaluationResponseContext {
                constraints: Vec::new(),
                deny_reason: None,
            },
        },
    }
}

/// Counting PDP fake that permits and scopes the grant to the request's own
/// `OWNER_TENANT_ID` (see [`permit_scoped_to_request_tenant`]), recording the
/// call count so per-record dedup tests can still assert the exact number of
/// PDP round-trips. Use this for the per-record (`usage_record`) paths under
/// `require_constraints(true)` where [`CountingAllowAllResolver`] would fail
/// closed.
#[derive(Debug, Default)]
pub struct CountingTenantPermitResolver {
    calls: AtomicUsize,
}

impl CountingTenantPermitResolver {
    /// Build a tenant-scoped permit resolver.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Number of `evaluate` calls observed so far.
    #[must_use]
    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl AuthZResolverApi for CountingTenantPermitResolver {
    async fn evaluate(
        &self,
        _ctx: PlatformSecurityContext,
        request: EvaluationRequest,
    ) -> Result<EvaluationResponse, CanonicalError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(permit_scoped_to_request_tenant(&request))
    }
}

/// Counting PDP fake that always permits with a fixed string-equality
/// constraint and records how many times it was called, so the no-cache test
/// can assert that two identical authorize calls each hit the resolver.
#[derive(Debug)]
pub struct CountingPermitResolver {
    property: &'static str,
    value: String,
    calls: AtomicUsize,
}

impl CountingPermitResolver {
    /// Build a resolver that permits with a single `property = value` string
    /// constraint.
    #[must_use]
    pub fn new(property: &'static str, value: String) -> Arc<Self> {
        Arc::new(Self {
            property,
            value,
            calls: AtomicUsize::new(0),
        })
    }

    /// Number of `evaluate` calls observed so far.
    #[must_use]
    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl AuthZResolverApi for CountingPermitResolver {
    async fn evaluate(
        &self,
        _ctx: PlatformSecurityContext,
        _request: EvaluationRequest,
    ) -> Result<EvaluationResponse, CanonicalError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(permit_with_string_constraint(
            self.property,
            self.value.clone(),
        ))
    }
}

/// Counting PDP fake that always permits with NO constraints (an
/// `allow_all` decision) and records call counts. Use this whenever the
/// resource type under test declares no supported PEP attributes — a
/// constraint-bearing permit (e.g. [`CountingPermitResolver`]) would fail
/// to compile under such a resource type, surfacing as
/// `EnforcerError::CompileFailed`.
#[derive(Debug, Default)]
pub struct CountingAllowAllResolver {
    calls: AtomicUsize,
}

impl CountingAllowAllResolver {
    /// Build an `allow_all` permit resolver.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Number of `evaluate` calls observed so far.
    #[must_use]
    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl AuthZResolverApi for CountingAllowAllResolver {
    async fn evaluate(
        &self,
        _ctx: PlatformSecurityContext,
        _request: EvaluationRequest,
    ) -> Result<EvaluationResponse, CanonicalError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(EvaluationResponse {
            decision: true,
            context: EvaluationResponseContext {
                constraints: Vec::new(),
                deny_reason: None,
            },
        })
    }
}

/// PDP fake that captures the most-recent [`EvaluationRequest`] it received
/// (so tests can inspect the request shape the PEP composed) AND scopes its
/// permit to the request's own `OWNER_TENANT_ID` (see
/// [`permit_scoped_to_request_tenant`]). Used by the `authz_tests`
/// equivalence regression to prove the per-record and per-tuple PDP composers
/// emit byte-identical requests for the same input; the tenant-scoped permit
/// is what lets those calls return `Ok` under the per-record gate's
/// `require_constraints(true)` posture (a plain empty-constraints permit would
/// fail closed there).
#[derive(Debug, Default)]
pub struct CapturingTenantPermitResolver {
    last_request: std::sync::Mutex<Option<EvaluationRequest>>,
}

impl CapturingTenantPermitResolver {
    /// Build a capturing tenant-scoped permit resolver.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Returns and clears the most-recently captured request.
    #[must_use]
    pub fn take_last_request(&self) -> Option<EvaluationRequest> {
        self.last_request.lock().expect("mutex").take()
    }
}

#[async_trait]
impl AuthZResolverApi for CapturingTenantPermitResolver {
    async fn evaluate(
        &self,
        _ctx: PlatformSecurityContext,
        request: EvaluationRequest,
    ) -> Result<EvaluationResponse, CanonicalError> {
        let response = permit_scoped_to_request_tenant(&request);
        *self.last_request.lock().expect("mutex") = Some(request);
        Ok(response)
    }
}

/// PDP fake that permits but returns an EMPTY constraint set. With
/// `require_constraints(true)` the PEP fails this closed as
/// `EnforcerError::CompileFailed` (`empty_constraints`).
#[derive(Debug, Default)]
pub struct PermitEmptyConstraintsResolver;

#[async_trait]
impl AuthZResolverApi for PermitEmptyConstraintsResolver {
    async fn evaluate(
        &self,
        _ctx: PlatformSecurityContext,
        _request: EvaluationRequest,
    ) -> Result<EvaluationResponse, CanonicalError> {
        Ok(EvaluationResponse {
            decision: true,
            context: EvaluationResponseContext {
                constraints: Vec::new(),
                deny_reason: None,
            },
        })
    }
}

/// PDP fake that denies every evaluation (`decision: false`), surfacing as
/// `EnforcerError::Denied` (`deny`).
#[derive(Debug, Default)]
pub struct DenyAllResolver;

#[async_trait]
impl AuthZResolverApi for DenyAllResolver {
    async fn evaluate(
        &self,
        _ctx: PlatformSecurityContext,
        _request: EvaluationRequest,
    ) -> Result<EvaluationResponse, CanonicalError> {
        Ok(EvaluationResponse {
            decision: false,
            context: EvaluationResponseContext {
                constraints: Vec::new(),
                deny_reason: None,
            },
        })
    }
}

/// PDP fake whose transport is unreachable: every evaluation returns
/// `AuthZResolverError::ServiceUnavailable`, surfacing as
/// `EnforcerError::EvaluationFailed` (`unreachable`).
#[derive(Debug, Default)]
pub struct UnreachableResolver;

#[async_trait]
impl AuthZResolverApi for UnreachableResolver {
    async fn evaluate(
        &self,
        _ctx: PlatformSecurityContext,
        _request: EvaluationRequest,
    ) -> Result<EvaluationResponse, CanonicalError> {
        Err(CanonicalError::service_unavailable()
            .with_detail(
                "usage-collector test fake: simulated authz-resolver transport failure".to_owned(),
            )
            .create())
    }
}

/// PDP fake that combines an unreachable transport with a call counter,
/// so handler tests asserting a pre-service short-circuit can pin
/// `calls() == 0` as direct evidence the service path was never reached.
#[derive(Debug, Default)]
pub struct CountingUnreachableResolver {
    calls: AtomicUsize,
}

impl CountingUnreachableResolver {
    /// Build a counting unreachable resolver.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Number of `evaluate` calls observed so far.
    #[must_use]
    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl AuthZResolverApi for CountingUnreachableResolver {
    async fn evaluate(
        &self,
        _ctx: PlatformSecurityContext,
        _request: EvaluationRequest,
    ) -> Result<EvaluationResponse, CanonicalError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Err(CanonicalError::service_unavailable()
            .with_detail(
                "usage-collector test fake: simulated authz-resolver transport failure".to_owned(),
            )
            .create())
    }
}

/// Wrap a `dyn AuthZResolverApi` in a `PolicyEnforcer` for tests, mirroring
/// the production `module.rs` wiring (`PolicyEnforcer::new(authz)`). No
/// capabilities are advertised, matching production (`module.rs:74`):
/// `usage_record` is a flat resource that does not advertise
/// `Capability::TenantHierarchy`, so the PDP expands a caller's tenant closure
/// eagerly into a flat `OWNER_TENANT_ID In [..]` constraint rather than pushing
/// it down as an `InTenantSubtree` predicate (see
/// [`crate::domain::authz::authorize_list_usage_records`]). A test that needs a
/// hierarchy-aware enforcer must build one explicitly and assert the
/// `InTenantSubtree` fail-closed behavior at that call site.
#[must_use]
pub fn enforcer_for(authz: Arc<dyn AuthZResolverApi>) -> PolicyEnforcer {
    PolicyEnforcer::new(authz)
}

// ── Plugin Host wiring helpers (`ClientHub` + scoped plugin) ────────────────

use toolkit::client_hub::{ClientHub, ClientScope};
use toolkit_security::SecurityContext;
use types_registry_sdk::TypesRegistryClient;
use types_registry_sdk::testing::{MockTypesRegistryClient, make_test_instance};
use usage_collector_sdk::UsageCollectorPluginSpecV1;

use crate::domain::Service;

/// Build a usage-collector storage-plugin instance id under the schema
/// prefix advertised by [`UsageCollectorPluginSpecV1`], with `suffix` as
/// the five-token instance tail (e.g. `"test.happy_path.records.v1"`).
#[must_use]
pub fn usage_collector_instance_id(suffix: &str) -> String {
    format!("{}{suffix}", UsageCollectorPluginSpecV1::gts_type_id())
}

fn plugin_instance_content(gts_id: &str, vendor: &str) -> serde_json::Value {
    serde_json::json!({
        "id": gts_id,
        "vendor": vendor,
        "priority": 0,
        "properties": {}
    })
}

/// Wire a fresh [`ClientHub`] with a `MockTypesRegistryClient` advertising
/// one usage-collector plugin instance and a scoped client binding the
/// supplied `plugin` under that instance id.
#[must_use]
pub fn hub_with_plugin(
    plugin: Arc<dyn UsageCollectorPluginV1>,
    suffix: &str,
    vendor: &str,
) -> Arc<ClientHub> {
    let hub = Arc::new(ClientHub::default());
    let instance_id = usage_collector_instance_id(suffix);
    let instance = make_test_instance(&instance_id, plugin_instance_content(&instance_id, vendor));
    let registry: Arc<dyn TypesRegistryClient> =
        Arc::new(MockTypesRegistryClient::new().with_instances([instance]));
    hub.register::<dyn TypesRegistryClient>(registry);
    hub.register_scoped::<dyn UsageCollectorPluginV1>(ClientScope::gts_id(&instance_id), plugin);
    hub
}

/// Build a [`Service`] wired against a permit-by-default PDP and the
/// supplied plugin stub, registered under the `constructorfabric` vendor.
///
/// The PDP fake ([`CountingTenantPermitResolver`]) scopes its permit to the
/// request's own `OWNER_TENANT_ID`, so per-record paths under
/// `require_constraints(true)` pass the tenant gate for whatever tenant the
/// record names, while the subject-only catalog surface (no `OWNER_TENANT_ID`,
/// `require_constraints(false)`) still gets an `allow_all` permit.
#[must_use]
pub fn service_with_permit(plugin: Arc<dyn UsageCollectorPluginV1>, suffix: &str) -> Arc<Service> {
    let hub = hub_with_plugin(plugin, suffix, "constructorfabric");
    let enforcer = enforcer_for(CountingTenantPermitResolver::new());
    Arc::new(Service::new(hub, "constructorfabric".to_owned(), enforcer))
}

/// Variant of [`service_with_permit`] that exposes the underlying
/// [`CountingTenantPermitResolver`] so tests can assert the exact number of
/// PDP `evaluate` round-trips the service issued.
#[must_use]
pub fn service_with_counting_permit(
    plugin: Arc<dyn UsageCollectorPluginV1>,
    suffix: &str,
) -> (Arc<Service>, Arc<CountingTenantPermitResolver>) {
    let hub = hub_with_plugin(plugin, suffix, "constructorfabric");
    let resolver = CountingTenantPermitResolver::new();
    let enforcer = enforcer_for(Arc::clone(&resolver) as Arc<dyn AuthZResolverApi>);
    let service = Arc::new(Service::new(hub, "constructorfabric".to_owned(), enforcer));
    (service, resolver)
}

// ── Metrics-instrumented Service builders + in-memory readback ──────────────
//
// These wire a `Service` with a real [`UcMetricsMeter`] bound to a local
// `SdkMeterProvider` + `InMemoryMetricExporter`, so emission tests can call a
// service method, `force_flush()` the returned provider, and read back the
// exported instruments. `opentelemetry_sdk` is a dev-dependency; this module
// is test-only.

use opentelemetry::metrics::MeterProvider as _;
use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData};
use opentelemetry_sdk::metrics::{InMemoryMetricExporter, PeriodicReader, SdkMeterProvider};

use crate::infra::metrics::UcMetricsMeter;

/// Build a fresh local `SdkMeterProvider` + `InMemoryMetricExporter` and a
/// `UcMetricsMeter` (prefix `uc`) bound to it.
#[must_use]
pub fn local_metrics() -> (
    Arc<UcMetricsMeter>,
    SdkMeterProvider,
    InMemoryMetricExporter,
) {
    let exporter = InMemoryMetricExporter::default();
    let provider = SdkMeterProvider::builder()
        .with_reader(PeriodicReader::builder(exporter.clone()).build())
        .build();
    let metrics = Arc::new(UcMetricsMeter::new(
        &provider.meter("usage-collector"),
        "uc",
    ));
    (metrics, provider, exporter)
}

/// A [`Service`] wired against `resolver` + `plugin` (under `constructorfabric`) with
/// a real metrics adapter bound to the returned provider/exporter.
#[must_use]
pub fn service_with_metrics(
    plugin: Arc<dyn UsageCollectorPluginV1>,
    suffix: &str,
    resolver: Arc<dyn AuthZResolverApi>,
) -> (Arc<Service>, SdkMeterProvider, InMemoryMetricExporter) {
    let hub = hub_with_plugin(plugin, suffix, "constructorfabric");
    let (metrics, provider, exporter) = local_metrics();
    let service = Arc::new(Service::new_with_metrics(
        hub,
        "constructorfabric".to_owned(),
        enforcer_for(resolver),
        metrics,
    ));
    (service, provider, exporter)
}

/// Wire a [`ClientHub`] whose types-registry advertises one usage-collector
/// plugin instance but does **not** register a scoped client under it, so
/// `Service::get_plugin` resolves an instance id yet fails with
/// `PluginUnavailable` — the structural-unready path.
#[must_use]
pub fn hub_registry_only(suffix: &str, vendor: &str) -> Arc<ClientHub> {
    let hub = Arc::new(ClientHub::default());
    let instance_id = usage_collector_instance_id(suffix);
    let instance = make_test_instance(&instance_id, plugin_instance_content(&instance_id, vendor));
    let registry: Arc<dyn TypesRegistryClient> =
        Arc::new(MockTypesRegistryClient::new().with_instances([instance]));
    hub.register::<dyn TypesRegistryClient>(registry);
    hub
}

/// A metrics-instrumented [`Service`] whose plugin binding is structurally
/// unready (registry advertises an instance, no scoped client registered).
#[must_use]
pub fn service_with_metrics_unready_plugin(
    suffix: &str,
    resolver: Arc<dyn AuthZResolverApi>,
) -> (Arc<Service>, SdkMeterProvider, InMemoryMetricExporter) {
    let hub = hub_registry_only(suffix, "constructorfabric");
    let (metrics, provider, exporter) = local_metrics();
    let service = Arc::new(Service::new_with_metrics(
        hub,
        "constructorfabric".to_owned(),
        enforcer_for(resolver),
        metrics,
    ));
    (service, provider, exporter)
}

/// Total summed value of a `u64` counter series, filtered to the data points
/// carrying `label_key == label_value`. Returns `0` when the instrument or
/// label is absent.
#[must_use]
pub fn counter_sum_with_label(
    exporter: &InMemoryMetricExporter,
    name: &str,
    label_key: &str,
    label_value: &str,
) -> u64 {
    let metrics = exporter.get_finished_metrics().expect("finished metrics");
    for rm in &metrics {
        for sm in rm.scope_metrics() {
            for metric in sm.metrics() {
                if metric.name() == name
                    && let AggregatedMetrics::U64(MetricData::Sum(sum)) = metric.data()
                {
                    return sum
                        .data_points()
                        .filter(|dp| {
                            dp.attributes().any(|kv| {
                                kv.key.as_str() == label_key && kv.value.as_str() == label_value
                            })
                        })
                        .map(opentelemetry_sdk::metrics::data::SumDataPoint::value)
                        .sum();
                }
            }
        }
    }
    0
}

/// Total sample count across all data points of an `f64` histogram. Returns
/// `0` when the instrument is absent.
#[must_use]
pub fn histogram_count(exporter: &InMemoryMetricExporter, name: &str) -> u64 {
    let metrics = exporter.get_finished_metrics().expect("finished metrics");
    for rm in &metrics {
        for sm in rm.scope_metrics() {
            for metric in sm.metrics() {
                if metric.name() == name
                    && let AggregatedMetrics::F64(MetricData::Histogram(h)) = metric.data()
                {
                    return h
                        .data_points()
                        .map(opentelemetry_sdk::metrics::data::HistogramDataPoint::count)
                        .sum();
                }
            }
        }
    }
    0
}

/// Total sample count across the `f64` histogram data points carrying
/// `label_key == label_value`. Returns `0` when the instrument or label is
/// absent. Use this (rather than [`histogram_count`]) when a single call drives
/// several dispatches through the same instrument and the assertion must pin the
/// per-`operation` sample — e.g. proving the emit-path SPI calls contribute a
/// `uc_plugin_call_duration_seconds{operation="create_usage_record"}` sample.
#[must_use]
pub fn histogram_count_with_label(
    exporter: &InMemoryMetricExporter,
    name: &str,
    label_key: &str,
    label_value: &str,
) -> u64 {
    let metrics = exporter.get_finished_metrics().expect("finished metrics");
    for rm in &metrics {
        for sm in rm.scope_metrics() {
            for metric in sm.metrics() {
                if metric.name() == name
                    && let AggregatedMetrics::F64(MetricData::Histogram(h)) = metric.data()
                {
                    return h
                        .data_points()
                        .filter(|dp| {
                            dp.attributes().any(|kv| {
                                kv.key.as_str() == label_key && kv.value.as_str() == label_value
                            })
                        })
                        .map(opentelemetry_sdk::metrics::data::HistogramDataPoint::count)
                        .sum();
                }
            }
        }
    }
    0
}

/// Sum of every value recorded into an `f64` histogram (across all data
/// points). Returns `0.0` when the instrument is absent. Use this — not
/// [`histogram_count`] (sample count) — to pin the observed *magnitude*, e.g.
/// the total bytes recorded into `uc_record_metadata_bytes`.
#[must_use]
pub fn histogram_sum(exporter: &InMemoryMetricExporter, name: &str) -> f64 {
    let metrics = exporter.get_finished_metrics().expect("finished metrics");
    for rm in &metrics {
        for sm in rm.scope_metrics() {
            for metric in sm.metrics() {
                if metric.name() == name
                    && let AggregatedMetrics::F64(MetricData::Histogram(h)) = metric.data()
                {
                    return h
                        .data_points()
                        .map(opentelemetry_sdk::metrics::data::HistogramDataPoint::sum)
                        .sum();
                }
            }
        }
    }
    0.0
}

/// Sum of the values recorded into the `f64` histogram data points carrying
/// `label_key == label_value`. Returns `0.0` when the instrument or label is
/// absent. Use this to pin the observed magnitude for a single label series —
/// e.g. the row count recorded into
/// `uc_query_result_rows{query_kind="raw"}` — which
/// [`histogram_count_with_label`] (sample count) cannot.
#[must_use]
pub fn histogram_sum_with_label(
    exporter: &InMemoryMetricExporter,
    name: &str,
    label_key: &str,
    label_value: &str,
) -> f64 {
    let metrics = exporter.get_finished_metrics().expect("finished metrics");
    for rm in &metrics {
        for sm in rm.scope_metrics() {
            for metric in sm.metrics() {
                if metric.name() == name
                    && let AggregatedMetrics::F64(MetricData::Histogram(h)) = metric.data()
                {
                    return h
                        .data_points()
                        .filter(|dp| {
                            dp.attributes().any(|kv| {
                                kv.key.as_str() == label_key && kv.value.as_str() == label_value
                            })
                        })
                        .map(opentelemetry_sdk::metrics::data::HistogramDataPoint::sum)
                        .sum();
                }
            }
        }
    }
    0.0
}

/// Last recorded value of an `i64` gauge series. `None` when absent.
#[must_use]
pub fn gauge_last(exporter: &InMemoryMetricExporter, name: &str) -> Option<i64> {
    let metrics = exporter.get_finished_metrics().expect("finished metrics");
    for rm in &metrics {
        for sm in rm.scope_metrics() {
            for metric in sm.metrics() {
                if metric.name() == name
                    && let AggregatedMetrics::I64(MetricData::Gauge(g)) = metric.data()
                {
                    return g
                        .data_points()
                        .next()
                        .map(opentelemetry_sdk::metrics::data::GaugeDataPoint::value);
                }
            }
        }
    }
    None
}

/// Build an authenticated [`SecurityContext`] sufficient for PDP requests
/// composed from a [`UsageRecord`]'s attribution tuple.
#[must_use]
pub fn authenticated_ctx() -> SecurityContext {
    SecurityContext::builder()
        .subject_id(Uuid::from_u128(1))
        .subject_tenant_id(Uuid::from_u128(2))
        .subject_type("user")
        .build()
        .expect("authenticated context")
}

// ── HappyPathPlugin: programmable Ok-by-default SPI stub ────────────────────

use std::sync::Mutex;

/// Per-record outcome shape for a `create_usage_records` SPI batch
/// response — `Ok(persisted_record)` or
/// `Err(UsageCollectorPluginError)`. Factored out so the
/// `HappyPathPlugin` field type and the `set_create_records` parameter
/// type stay readable.
pub type CreateRecordsBatchResult = Vec<Result<UsageRecord, UsageCollectorPluginError>>;

/// Programmable plugin stub that returns the configured response for each
/// SPI method, defaulting to `UsageCollectorPluginError::internal("not
/// programmed")` for methods the test has not explicitly set up. Methods
/// also record their last-seen input so handler-level tests can verify
/// the service forwarded the expected argument.
///
/// The stub is `Arc<Self>` everywhere; interior state lives behind
/// `Mutex` so callers can program responses after construction.
pub struct HappyPathPlugin {
    create_record_response: Mutex<Option<Result<UsageRecord, UsageCollectorPluginError>>>,
    create_records_response: Mutex<Option<CreateRecordsBatchResult>>,
    deactivate_response: Mutex<Option<()>>,
    get_record_response: Mutex<Option<UsageRecord>>,
    create_usage_type_response: Mutex<Option<UsageType>>,
    get_usage_type_response: Mutex<Option<UsageType>>,
    /// `gts_id`s that should surface as
    /// `UsageCollectorPluginError::UsageTypeNotFound` instead of returning
    /// the default `get_usage_type_response`. Lets tests verify per-record
    /// not-found projection on the batch path.
    get_usage_type_not_found: Mutex<std::collections::BTreeSet<UsageTypeGtsId>>,
    list_usage_types_response: Mutex<Option<ODataPage<UsageType>>>,
    /// When set, `list_usage_types` never completes (returns a pending future),
    /// so tests can drive the bounded gauge-refresh timeout under paused time.
    list_usage_types_hang: Mutex<bool>,
    /// FIFO of pages returned by successive `list_usage_types` calls; when
    /// non-empty it takes precedence over `list_usage_types_response`, popping
    /// one page per call so tests can exercise full cursor pagination.
    list_usage_types_pages: Mutex<std::collections::VecDeque<ODataPage<UsageType>>>,
    list_usage_records_response: Mutex<Option<ODataPage<UsageRecord>>>,
    query_aggregated_usage_records_response: Mutex<Option<AggregationResult>>,
    delete_usage_type_response: Mutex<Option<()>>,

    create_record_input: Mutex<Option<UsageRecord>>,
    create_records_input: Mutex<Option<Vec<UsageRecord>>>,
    deactivate_input: Mutex<Option<Uuid>>,
    delete_usage_type_input: Mutex<Option<UsageTypeGtsId>>,
    create_usage_type_input: Mutex<Option<UsageType>>,
    /// Every `ODataQuery` ever forwarded to `list_usage_types`, in call
    /// order. Lets handler tests pin that the handler forwarded the
    /// query unchanged to the service (and hence to the SPI).
    list_usage_types_inputs: Mutex<Vec<ODataQuery>>,
    /// Every `gts_id` ever passed to `get_usage_type`, in call order.
    /// `len()` is the call count; the vec is forensic — tests can
    /// verify exactly which `gts_id`s the host looked up.
    get_usage_type_inputs: Mutex<Vec<UsageTypeGtsId>>,
    /// Every record `id` ever passed to `get_usage_record`, in call
    /// order. Drives the L1-corrects-id dedup tests the same way
    /// `get_usage_type_inputs` drives the catalog-dedup tests.
    get_usage_record_inputs: Mutex<Vec<Uuid>>,
    /// Record `id`s that should surface as
    /// `UsageCollectorPluginError::UsageRecordNotFound` instead of
    /// returning the default `get_record_response`.
    get_usage_record_not_found: Mutex<std::collections::BTreeSet<Uuid>>,
}

impl HappyPathPlugin {
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            create_record_response: Mutex::new(None),
            create_records_response: Mutex::new(None),
            deactivate_response: Mutex::new(None),
            get_record_response: Mutex::new(None),
            create_usage_type_response: Mutex::new(None),
            get_usage_type_response: Mutex::new(None),
            get_usage_type_not_found: Mutex::new(std::collections::BTreeSet::new()),
            list_usage_types_response: Mutex::new(None),
            list_usage_types_hang: Mutex::new(false),
            list_usage_types_pages: Mutex::new(std::collections::VecDeque::new()),
            list_usage_records_response: Mutex::new(None),
            query_aggregated_usage_records_response: Mutex::new(None),
            delete_usage_type_response: Mutex::new(None),
            create_record_input: Mutex::new(None),
            create_records_input: Mutex::new(None),
            deactivate_input: Mutex::new(None),
            delete_usage_type_input: Mutex::new(None),
            create_usage_type_input: Mutex::new(None),
            list_usage_types_inputs: Mutex::new(Vec::new()),
            get_usage_type_inputs: Mutex::new(Vec::new()),
            get_usage_record_inputs: Mutex::new(Vec::new()),
            get_usage_record_not_found: Mutex::new(std::collections::BTreeSet::new()),
        })
    }

    pub fn set_create_record(&self, record: UsageRecord) {
        *self.create_record_response.lock().expect("mutex") = Some(Ok(record));
    }
    /// Program the singular `create_usage_record` SPI to return `err` on
    /// the next call. Used by tests that need to drive plugin-side
    /// failure modes (`Transient`, `IdempotencyConflict`, …) into the
    /// service's singular path.
    pub fn set_create_record_err(&self, err: UsageCollectorPluginError) {
        *self.create_record_response.lock().expect("mutex") = Some(Err(err));
    }
    pub fn set_create_records(&self, results: CreateRecordsBatchResult) {
        *self.create_records_response.lock().expect("mutex") = Some(results);
    }
    pub fn set_deactivate_ok(&self) {
        *self.deactivate_response.lock().expect("mutex") = Some(());
    }
    pub fn set_get_record(&self, record: UsageRecord) {
        *self.get_record_response.lock().expect("mutex") = Some(record);
    }
    pub fn set_create_usage_type(&self, ut: UsageType) {
        *self.create_usage_type_response.lock().expect("mutex") = Some(ut);
    }
    pub fn set_get_usage_type(&self, ut: UsageType) {
        *self.get_usage_type_response.lock().expect("mutex") = Some(ut);
    }
    /// Mark `gts_id` so the next (and every subsequent) `get_usage_type`
    /// call carrying it returns `UsageTypeNotFound` regardless of the
    /// default `get_usage_type_response`.
    pub fn set_get_usage_type_not_found(&self, gts_id: UsageTypeGtsId) {
        self.get_usage_type_not_found
            .lock()
            .expect("mutex")
            .insert(gts_id);
    }
    /// Every `gts_id` passed to `get_usage_type`, in call order.
    #[must_use]
    pub fn get_usage_type_inputs(&self) -> Vec<UsageTypeGtsId> {
        self.get_usage_type_inputs.lock().expect("mutex").clone()
    }
    /// Total number of `get_usage_type` SPI dispatches so far.
    #[must_use]
    pub fn get_usage_type_calls(&self) -> usize {
        self.get_usage_type_inputs.lock().expect("mutex").len()
    }
    /// Mark `id` so the next (and every subsequent) `get_usage_record`
    /// call carrying it returns `UsageRecordNotFound` regardless of the
    /// default `get_record_response`.
    pub fn set_get_usage_record_not_found(&self, id: Uuid) {
        self.get_usage_record_not_found
            .lock()
            .expect("mutex")
            .insert(id);
    }
    /// Every record `id` passed to `get_usage_record`, in call order.
    #[must_use]
    pub fn get_usage_record_inputs(&self) -> Vec<Uuid> {
        self.get_usage_record_inputs.lock().expect("mutex").clone()
    }
    /// Total number of `get_usage_record` SPI dispatches so far.
    #[must_use]
    pub fn get_usage_record_calls(&self) -> usize {
        self.get_usage_record_inputs.lock().expect("mutex").len()
    }
    pub fn set_list_usage_types(&self, page: ODataPage<UsageType>) {
        *self.list_usage_types_response.lock().expect("mutex") = Some(page);
    }
    /// Program a FIFO sequence of pages returned by successive
    /// `list_usage_types` calls. Takes precedence over the single-page
    /// `set_list_usage_types` response; used to exercise full cursor
    /// pagination (and its best-effort exhaustion) in the gauge refresh.
    pub fn set_list_usage_types_pages(&self, pages: Vec<ODataPage<UsageType>>) {
        *self.list_usage_types_pages.lock().expect("mutex") = pages.into();
    }
    /// Make every subsequent `list_usage_types` call hang forever (a
    /// never-completing future). Used to drive the bounded gauge-refresh
    /// timeout in `refresh_usage_types_gauge` without a wall-clock delay
    /// (pair with `#[tokio::test(start_paused = true)]`).
    pub fn set_list_usage_types_hang(&self) {
        *self.list_usage_types_hang.lock().expect("mutex") = true;
    }
    pub fn set_list_usage_records_response(&self, page: ODataPage<UsageRecord>) {
        *self.list_usage_records_response.lock().expect("mutex") = Some(page);
    }
    pub fn set_query_aggregated_usage_records_response(&self, result: AggregationResult) {
        *self
            .query_aggregated_usage_records_response
            .lock()
            .expect("mutex") = Some(result);
    }
    pub fn set_delete_usage_type_ok(&self) {
        *self.delete_usage_type_response.lock().expect("mutex") = Some(());
    }

    pub fn last_create_record_input(&self) -> Option<UsageRecord> {
        self.create_record_input.lock().expect("mutex").clone()
    }
    pub fn last_create_records_input(&self) -> Option<Vec<UsageRecord>> {
        self.create_records_input.lock().expect("mutex").clone()
    }
    pub fn last_deactivate_input(&self) -> Option<Uuid> {
        *self.deactivate_input.lock().expect("mutex")
    }
    pub fn last_delete_usage_type_input(&self) -> Option<UsageTypeGtsId> {
        self.delete_usage_type_input.lock().expect("mutex").clone()
    }
    pub fn last_create_usage_type_input(&self) -> Option<UsageType> {
        self.create_usage_type_input.lock().expect("mutex").clone()
    }
    /// Every `ODataQuery` passed to `list_usage_types`, in call order.
    #[must_use]
    pub fn list_usage_types_inputs(&self) -> Vec<ODataQuery> {
        self.list_usage_types_inputs.lock().expect("mutex").clone()
    }
    /// The most-recent `ODataQuery` passed to `list_usage_types`, or
    /// `None` if the SPI was never invoked.
    #[must_use]
    pub fn last_list_usage_types_input(&self) -> Option<ODataQuery> {
        self.list_usage_types_inputs
            .lock()
            .expect("mutex")
            .last()
            .cloned()
    }
}

fn not_programmed(method: &'static str) -> UsageCollectorPluginError {
    UsageCollectorPluginError::internal(format!("HappyPathPlugin::{method} not programmed"))
}

#[async_trait]
impl UsageCollectorPluginV1 for HappyPathPlugin {
    async fn create_usage_record(
        &self,
        record: UsageRecord,
    ) -> Result<UsageRecord, UsageCollectorPluginError> {
        *self.create_record_input.lock().expect("mutex") = Some(record);
        // `take()` (not `clone()`) — `UsageCollectorPluginError` is
        // intentionally `!Clone` so tests program one outcome per call.
        match self.create_record_response.lock().expect("mutex").take() {
            Some(outcome) => outcome,
            None => Err(not_programmed("create_usage_record")),
        }
    }

    async fn create_usage_records(
        &self,
        records: Vec<UsageRecord>,
    ) -> Result<Vec<Result<UsageRecord, UsageCollectorPluginError>>, UsageCollectorPluginError>
    {
        *self.create_records_input.lock().expect("mutex") = Some(records);
        self.create_records_response
            .lock()
            .expect("mutex")
            .take()
            .ok_or_else(|| not_programmed("create_usage_records"))
    }

    async fn query_aggregated_usage_records(
        &self,
        _gts_id: UsageTypeGtsId,
        _query: &ODataQuery,
        _metadata_filter: &[MetadataFilter],
        _aggregation: AggregationSpec,
    ) -> Result<AggregationResult, UsageCollectorPluginError> {
        self.query_aggregated_usage_records_response
            .lock()
            .expect("mutex")
            .clone()
            .ok_or_else(|| not_programmed("query_aggregated_usage_records"))
    }

    async fn list_usage_records(
        &self,
        _gts_id: UsageTypeGtsId,
        _query: &ODataQuery,
        _metadata_filter: &[MetadataFilter],
    ) -> Result<ODataPage<UsageRecord>, UsageCollectorPluginError> {
        self.list_usage_records_response
            .lock()
            .expect("mutex")
            .clone()
            .ok_or_else(|| not_programmed("list_usage_records"))
    }

    async fn deactivate_usage_record(&self, id: Uuid) -> Result<(), UsageCollectorPluginError> {
        *self.deactivate_input.lock().expect("mutex") = Some(id);
        self.deactivate_response
            .lock()
            .expect("mutex")
            .ok_or_else(|| not_programmed("deactivate_usage_record"))
    }

    async fn create_usage_type(
        &self,
        usage_type: UsageType,
    ) -> Result<UsageType, UsageCollectorPluginError> {
        *self.create_usage_type_input.lock().expect("mutex") = Some(usage_type);
        self.create_usage_type_response
            .lock()
            .expect("mutex")
            .clone()
            .ok_or_else(|| not_programmed("create_usage_type"))
    }

    async fn get_usage_type(
        &self,
        gts_id: UsageTypeGtsId,
    ) -> Result<UsageType, UsageCollectorPluginError> {
        self.get_usage_type_inputs
            .lock()
            .expect("mutex")
            .push(gts_id.clone());
        if self
            .get_usage_type_not_found
            .lock()
            .expect("mutex")
            .contains(&gts_id)
        {
            return Err(UsageCollectorPluginError::UsageTypeNotFound { gts_id });
        }
        self.get_usage_type_response
            .lock()
            .expect("mutex")
            .clone()
            .ok_or_else(|| not_programmed("get_usage_type"))
    }

    async fn list_usage_types(
        &self,
        query: &ODataQuery,
    ) -> Result<ODataPage<UsageType>, UsageCollectorPluginError> {
        self.list_usage_types_inputs
            .lock()
            .expect("mutex")
            .push(query.clone());
        if *self.list_usage_types_hang.lock().expect("mutex") {
            // Never resolves — under a timeout the caller sees `Elapsed`.
            std::future::pending::<()>().await;
        }
        // Multi-page queue takes precedence (full-pagination tests); pop one
        // page per call. Falls back to the single programmable response.
        if let Some(page) = self
            .list_usage_types_pages
            .lock()
            .expect("mutex")
            .pop_front()
        {
            return Ok(page);
        }
        self.list_usage_types_response
            .lock()
            .expect("mutex")
            .clone()
            .ok_or_else(|| not_programmed("list_usage_types"))
    }

    async fn delete_usage_type(
        &self,
        gts_id: UsageTypeGtsId,
    ) -> Result<(), UsageCollectorPluginError> {
        *self.delete_usage_type_input.lock().expect("mutex") = Some(gts_id);
        self.delete_usage_type_response
            .lock()
            .expect("mutex")
            .ok_or_else(|| not_programmed("delete_usage_type"))
    }

    async fn get_usage_record(&self, id: Uuid) -> Result<UsageRecord, UsageCollectorPluginError> {
        self.get_usage_record_inputs.lock().expect("mutex").push(id);
        if self
            .get_usage_record_not_found
            .lock()
            .expect("mutex")
            .contains(&id)
        {
            return Err(UsageCollectorPluginError::UsageRecordNotFound { id });
        }
        self.get_record_response
            .lock()
            .expect("mutex")
            .clone()
            .ok_or_else(|| not_programmed("get_usage_record"))
    }
}
