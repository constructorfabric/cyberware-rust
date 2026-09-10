#![allow(clippy::expect_used)]

use std::sync::Arc;

use authz_resolver_sdk::AuthZResolverApi;
use gts::GtsTypeId;
use quota_enforcement_sdk::testing::{InMemoryStorage, quota_draft};
use quota_enforcement_sdk::{
    CONTRACT_MAJOR, MetricId, QuotaEnforcementStoragePluginV1, SCOPE_TENANT, SCOPE_USER,
    StorageError, SubjectRef, SubjectScope,
};
use tokio_util::sync::CancellationToken;
use toolkit::ClientHub;
use toolkit_security::AccessScope;

use super::{Bootstrap, CatalogBinding};
use crate::domain::catalog::CatalogConfig;
use crate::domain::error::{Dependency, DomainError};
use crate::domain::plugins::PluginBinding;
use crate::domain::ports::contracts::ContractRegistry;
use crate::domain::ports::coordination::SingletonScope;
use crate::domain::ports::metric_registry::MetricRegistry;
use crate::domain::ports::metrics::{ValidationReason, ValidationSurface};
use crate::domain::readiness::{Readiness, ReadinessState};
use crate::infra::pdp_probe::PdpReachability;
use crate::infra::types_registry::TypesRegistryContracts;
use crate::test_support::{
    DenyAllPdp, FailingPdp, FakeContractRegistry, FakeMetricRegistry, LLM_MODEL_RESOURCE,
    LLM_TENANT_PROJECTION, LLM_TOKEN_CONSTRAINT, LLM_TOKEN_REQUEST, LLM_USER_PROJECTION,
    METRIC_OTHER, METRIC_TOKENS, PermitTenantsPdp, RecordingMetrics, StaticCoordinatorBinding, ctx,
    hub_with, idle_work, in_process_registry, llm_gateway_documents, metric_base_documents,
    register_storage, storage_instance, tenant,
};

fn type_id(raw: &str) -> GtsTypeId {
    GtsTypeId::try_new(raw).expect("type id")
}

fn llm_config() -> CatalogConfig {
    CatalogConfig {
        subject_projections: vec![type_id(LLM_USER_PROJECTION), type_id(LLM_TENANT_PROJECTION)],
        resource_projections: vec![type_id(LLM_MODEL_RESOURCE)],
    }
}

struct Harness {
    hub: Arc<ClientHub>,
    storage: Arc<InMemoryStorage>,
    coordinator: Arc<StaticCoordinatorBinding>,
    pdp: Arc<dyn AuthZResolverApi>,
    registry: Arc<dyn ContractRegistry>,
    metric_registry: Arc<FakeMetricRegistry>,
    config: CatalogConfig,
    metrics: Arc<RecordingMetrics>,
    readiness: Arc<Readiness>,
}

fn permitting_pdp() -> Arc<PermitTenantsPdp> {
    Arc::new(PermitTenantsPdp::new(vec![tenant().as_uuid()]))
}

/// A harness over the `llm_gateway` fake registry with both projections configured.
fn harness(
    storage: Arc<InMemoryStorage>,
    register_client: bool,
    pdp: Arc<dyn AuthZResolverApi>,
) -> Harness {
    harness_with_registry(
        storage,
        register_client,
        pdp,
        Arc::new(FakeContractRegistry::llm_gateway()),
    )
}

fn harness_with_registry(
    storage: Arc<InMemoryStorage>,
    register_client: bool,
    pdp: Arc<dyn AuthZResolverApi>,
    registry: Arc<dyn ContractRegistry>,
) -> Harness {
    let storage_fixture = storage_instance("cf.core._.qe_db_storage.v1", "acme", 100);
    let hub = hub_with(&[&storage_fixture]);
    if register_client {
        register_storage(&hub, &storage_fixture, storage.clone());
    }
    Harness {
        hub,
        storage,
        coordinator: StaticCoordinatorBinding::ok(),
        pdp,
        registry,
        metric_registry: Arc::new(FakeMetricRegistry::classified()),
        config: llm_config(),
        metrics: Arc::new(RecordingMetrics::default()),
        readiness: Arc::new(Readiness::new()),
    }
}

fn bootstrap(h: &Harness) -> Bootstrap {
    Bootstrap::new(
        PluginBinding::new(h.hub.clone(), "acme".to_owned()),
        h.coordinator.clone(),
        Arc::new(PdpReachability::new(h.pdp.clone())),
        CatalogBinding {
            registry: h.registry.clone(),
            config: h.config.clone(),
        },
        h.metric_registry.clone() as Arc<dyn MetricRegistry>,
        h.metrics.clone(),
        h.readiness.clone(),
    )
}

fn failed_on(h: &Harness, dependency: Dependency) {
    match h.readiness.snapshot() {
        ReadinessState::Failed {
            dependency: got, ..
        } => assert_eq!(got, dependency),
        other => panic!("expected a {dependency} failure, got {other:?}"),
    }
}

#[tokio::test]
async fn a_complete_environment_bootstraps_resolves_the_coordinator_and_becomes_ready() {
    let pdp = permitting_pdp();
    let fake = Arc::new(FakeContractRegistry::llm_gateway());
    let h = harness_with_registry(
        Arc::new(InMemoryStorage::new()),
        true,
        pdp.clone(),
        fake.clone(),
    );
    let bound = bootstrap(&h).run().await.expect("bootstrap succeeds");

    assert!(h.readiness.is_ready());
    assert_eq!(pdp.calls(), 1, "the PDP probe made one round trip");
    assert_eq!(h.storage.bootstrap_calls(), 1);
    assert_eq!(
        h.storage.seeded_defaults().map(|d| d.max_active_leases),
        Some(1000),
        "the foundation bundle seeded the PRD defaults"
    );
    assert_eq!(
        h.storage.bootstrapped_bundle().map(|b| b.contract_major),
        Some(CONTRACT_MAJOR)
    );
    assert_eq!(
        h.coordinator.calls(),
        1,
        "the cluster binding resolved once"
    );

    // The QE-owned definitions were asserted, and the catalogue is published.
    assert_eq!(fake.ensure_registered_calls(), 1);
    let registered = fake.last_registered();
    assert_eq!(registered.len(), 7);
    assert!(registered.contains(&SCOPE_USER) && registered.contains(&SCOPE_TENANT));
    let tokens = MetricId::parse(METRIC_TOKENS).expect("metric");
    assert_eq!(
        bound
            .catalog
            .map_subject(&tokens, &SubjectScope::user())
            .expect("mapped")
            .as_ref(),
        LLM_USER_PROJECTION
    );
    assert_eq!(
        bound
            .catalog
            .request_contract(&tokens)
            .map(|c| c.type_id.as_ref()),
        Some(LLM_TOKEN_REQUEST)
    );
    assert!(h.metrics.contract_failures().is_empty());

    let shutdown = CancellationToken::new();
    shutdown.cancel();
    bound
        .coordinator
        .run_while_leader(SingletonScope::LeaseSweeper, shutdown, idle_work())
        .await
        .expect("the bound coordinator is usable");
}

#[tokio::test]
async fn a_schema_mismatch_fails_bootstrap_on_the_storage_dependency() {
    let h = harness(
        Arc::new(InMemoryStorage::with_installed_schema_major(
            CONTRACT_MAJOR + 1,
        )),
        true,
        permitting_pdp(),
    );
    let err = bootstrap(&h).run().await.err().expect("mismatch");
    assert_eq!(
        err,
        DomainError::SchemaVersionMismatch {
            installed: CONTRACT_MAJOR + 1,
            expected: CONTRACT_MAJOR,
        }
    );
    failed_on(&h, Dependency::Storage);
    assert_eq!(h.coordinator.calls(), 0, "later steps never run");
}

#[tokio::test]
async fn a_missing_storage_client_fails_bootstrap_before_the_cluster_resolve() {
    let h = harness(Arc::new(InMemoryStorage::new()), false, permitting_pdp());
    let err = bootstrap(&h)
        .run()
        .await
        .err()
        .expect("client not registered");
    assert!(
        matches!(err, DomainError::PluginClientNotRegistered { .. }),
        "{err:?}"
    );
    failed_on(&h, Dependency::Storage);
    assert_eq!(h.storage.bootstrap_calls(), 0);
    assert_eq!(h.coordinator.calls(), 0);
}

#[tokio::test]
async fn a_failing_cluster_resolve_fails_bootstrap_on_the_cluster_dependency() {
    let mut h = harness(Arc::new(InMemoryStorage::new()), true, permitting_pdp());
    h.coordinator = StaticCoordinatorBinding::failing(DomainError::ClusterUnavailable(
        "no backend bound for profile `quota-enforcement`".to_owned(),
    ));
    let err = bootstrap(&h).run().await.err().expect("resolve fails");
    assert!(matches!(err, DomainError::ClusterUnavailable(_)), "{err:?}");
    match h.readiness.snapshot() {
        ReadinessState::Failed { dependency, reason } => {
            assert_eq!(dependency, Dependency::Cluster);
            assert!(reason.contains("quota-enforcement"), "{reason}");
        }
        other => panic!("expected a cluster failure, got {other:?}"),
    }
    assert_eq!(
        h.storage.bootstrap_calls(),
        1,
        "storage bootstrap ran before the cluster resolve"
    );
}

#[tokio::test]
async fn a_storage_backend_outage_at_bootstrap_is_unavailability() {
    let storage = Arc::new(InMemoryStorage::new());
    storage.fail_with(StorageError::Unavailable("db down".into()));
    let h = harness(storage, true, permitting_pdp());
    let err = bootstrap(&h).run().await.err().expect("storage down");
    assert_eq!(err, DomainError::BackendUnavailable("db down".to_owned()));
}

#[tokio::test]
async fn a_registered_but_failing_pdp_fails_bootstrap_after_the_cluster_resolve() {
    // The client exists (init accepted it); the PDP behind it does not answer.
    // Registration alone must not make the gear ready.
    let h = harness(Arc::new(InMemoryStorage::new()), true, Arc::new(FailingPdp));
    let err = bootstrap(&h).run().await.err().expect("PDP down");
    assert!(matches!(err, DomainError::PdpUnavailable(_)), "{err:?}");
    failed_on(&h, Dependency::Pdp);
    assert_eq!(h.coordinator.calls(), 1, "the cluster resolve completed");
}

#[tokio::test]
async fn a_denying_pdp_is_reachable_and_bootstrap_completes() {
    // The probe principal has no tenant, so a real PDP denies it. The probe
    // measures reachability, not permission.
    let h = harness(Arc::new(InMemoryStorage::new()), true, Arc::new(DenyAllPdp));
    bootstrap(&h).run().await.expect("a denial is an answer");
    assert!(h.readiness.is_ready());
}

#[tokio::test]
async fn an_unreachable_registry_fails_bootstrap_on_the_registry_dependency() {
    let fake = Arc::new(FakeContractRegistry::llm_gateway());
    fake.fail_all();
    let h = harness_with_registry(
        Arc::new(InMemoryStorage::new()),
        true,
        permitting_pdp(),
        fake,
    );
    let err = bootstrap(&h).run().await.err().expect("registry down");
    assert!(
        matches!(err, DomainError::TypesRegistryUnavailable(_)),
        "{err:?}"
    );
    failed_on(&h, Dependency::TypesRegistry);
    assert_eq!(h.storage.bootstrap_calls(), 1, "storage came first");
    assert_eq!(
        h.coordinator.calls(),
        0,
        "the catalogue precedes the cluster resolve"
    );
}

#[tokio::test]
async fn a_conflicting_owned_definition_fails_bootstrap_on_the_catalogue_dependency() {
    let fake = Arc::new(FakeContractRegistry::llm_gateway());
    fake.fail_registration_with(DomainError::CatalogInvalid {
        reason: ValidationReason::DefinitionConflict,
        subject: "gts.cf.core.qe.scope.v1~".to_owned(),
    });
    let h = harness_with_registry(
        Arc::new(InMemoryStorage::new()),
        true,
        permitting_pdp(),
        fake,
    );
    let err = bootstrap(&h).run().await.err().expect("conflict");
    assert!(
        matches!(
            err,
            DomainError::CatalogInvalid {
                reason: ValidationReason::DefinitionConflict,
                ..
            }
        ),
        "{err:?}"
    );
    failed_on(&h, Dependency::Catalog);
    assert!(!h.readiness.is_ready());
}

#[tokio::test]
async fn an_inconsistent_catalogue_fails_bootstrap_and_never_marks_ready() {
    let fake = Arc::new(FakeContractRegistry::llm_gateway());
    fake.remove_type(LLM_TOKEN_CONSTRAINT);
    let h = harness_with_registry(
        Arc::new(InMemoryStorage::new()),
        true,
        permitting_pdp(),
        fake,
    );
    let err = bootstrap(&h).run().await.err().expect("inconsistent");
    assert!(
        matches!(
            err,
            DomainError::CatalogInvalid {
                reason: ValidationReason::ConstraintInvalid,
                ..
            }
        ),
        "{err:?}"
    );
    failed_on(&h, Dependency::Catalog);
    assert_eq!(
        h.metrics.contract_failures(),
        vec![(
            ValidationSurface::Bootstrap,
            ValidationReason::ConstraintInvalid
        )]
    );
    assert_eq!(h.coordinator.calls(), 0);
}

#[tokio::test]
async fn an_active_quota_the_catalogue_no_longer_admits_fails_bootstrap() {
    let storage = Arc::new(InMemoryStorage::new());
    let mut stranded = quota_draft(
        SubjectRef {
            projection_type: type_id(LLM_USER_PROJECTION),
            subject_id: "u-1".to_owned(),
        },
        Some(5),
    );
    stranded.metric = MetricId::parse(METRIC_OTHER).expect("metric");
    storage
        .create_quota(&ctx(), &AccessScope::allow_all(), stranded, &[])
        .await
        .expect("seeded");
    let h = harness(storage, true, permitting_pdp());
    let err = bootstrap(&h).run().await.err().expect("stranded Quota");
    assert!(
        matches!(
            err,
            DomainError::CatalogInvalid {
                reason: ValidationReason::IncompatibleState,
                ..
            }
        ),
        "{err:?}"
    );
    failed_on(&h, Dependency::Catalog);
    assert_eq!(h.coordinator.calls(), 0);
}

#[tokio::test]
async fn a_compatible_active_quota_passes_the_compatibility_check() {
    let storage = Arc::new(InMemoryStorage::new());
    let mut bound_quota = quota_draft(
        SubjectRef {
            projection_type: type_id(LLM_USER_PROJECTION),
            subject_id: "u-1".to_owned(),
        },
        Some(5),
    );
    bound_quota.metric = MetricId::parse(METRIC_TOKENS).expect("metric");
    storage
        .create_quota(&ctx(), &AccessScope::allow_all(), bound_quota, &[])
        .await
        .expect("seeded");
    let h = harness(storage, true, permitting_pdp());
    bootstrap(&h)
        .run()
        .await
        .expect("the catalogue admits the binding");
    assert!(h.readiness.is_ready());
}

#[tokio::test]
async fn an_active_quota_on_a_removed_metric_is_flagged_and_bootstrap_completes() {
    let storage = Arc::new(InMemoryStorage::new());
    let mut stranded = quota_draft(
        SubjectRef {
            projection_type: type_id(LLM_USER_PROJECTION),
            subject_id: "u-1".to_owned(),
        },
        Some(5),
    );
    stranded.metric = MetricId::parse(METRIC_TOKENS).expect("metric");
    storage
        .create_quota(&ctx(), &AccessScope::allow_all(), stranded, &[])
        .await
        .expect("seeded");
    let h = harness(storage, true, permitting_pdp());
    h.metric_registry.remove(METRIC_TOKENS);
    bootstrap(&h)
        .run()
        .await
        .expect("a removed metric is flagged, never fatal");
    assert!(h.readiness.is_ready());
    assert_eq!(
        h.metric_registry.calls(),
        1,
        "each distinct bound metric is looked up once"
    );
}

#[tokio::test]
async fn a_registry_that_does_not_answer_the_metric_scan_fails_on_the_registry_dependency() {
    let storage = Arc::new(InMemoryStorage::new());
    let mut bound_quota = quota_draft(
        SubjectRef {
            projection_type: type_id(LLM_USER_PROJECTION),
            subject_id: "u-1".to_owned(),
        },
        Some(5),
    );
    bound_quota.metric = MetricId::parse(METRIC_TOKENS).expect("metric");
    storage
        .create_quota(&ctx(), &AccessScope::allow_all(), bound_quota, &[])
        .await
        .expect("seeded");
    let h = harness(storage, true, permitting_pdp());
    h.metric_registry.fail_all();
    let err = bootstrap(&h).run().await.err().expect("registry outage");
    assert!(
        matches!(err, DomainError::TypesRegistryUnavailable(_)),
        "{err:?}"
    );
    failed_on(&h, Dependency::TypesRegistry);
    assert_eq!(
        h.coordinator.calls(),
        0,
        "the cluster resolve never ran after the failed scan"
    );
}

/// The mandatory real-registry test: registration, discovery, resolution,
/// compilation, and an idempotent restart, with nothing mocked between the
/// gear and `types-registry`.
#[tokio::test]
async fn the_real_registry_bootstraps_registers_discovers_compiles_and_restarts_idempotently() {
    let mut extra = llm_gateway_documents();
    extra.extend(metric_base_documents());
    let client = in_process_registry(extra);
    let registry: Arc<dyn ContractRegistry> = Arc::new(TypesRegistryContracts::new(client.clone()));

    let first = harness_with_registry(
        Arc::new(InMemoryStorage::new()),
        true,
        permitting_pdp(),
        registry.clone(),
    );
    let bound = bootstrap(&first).run().await.expect("first bootstrap");
    assert!(first.readiness.is_ready());
    let tokens = MetricId::parse(METRIC_TOKENS).expect("metric");
    assert_eq!(
        bound
            .catalog
            .map_subject(&tokens, &SubjectScope::tenant())
            .expect("discovered and mapped")
            .as_ref(),
        LLM_TENANT_PROJECTION
    );
    let request = bound
        .catalog
        .request_contract(&tokens)
        .expect("discovered from the registry listing");
    assert_eq!(request.type_id.as_ref(), LLM_TOKEN_REQUEST);
    request
        .contract
        .validate(&serde_json::json!({ "type": LLM_TOKEN_REQUEST, "metadata": { "region": "eu" } }))
        .expect("compiled from the resolved registry content");

    // A restart against the same registry re-asserts the QE definitions
    // (byte-identical: a silent success) and publishes an equivalent catalogue.
    let second = harness_with_registry(
        Arc::new(InMemoryStorage::new()),
        true,
        permitting_pdp(),
        registry,
    );
    let again = bootstrap(&second).run().await.expect("second bootstrap");
    assert!(second.readiness.is_ready());
    assert_eq!(
        again
            .catalog
            .map_subject(&tokens, &SubjectScope::user())
            .expect("mapped")
            .as_ref(),
        LLM_USER_PROJECTION
    );
    assert!(first.metrics.contract_failures().is_empty());
    assert!(second.metrics.contract_failures().is_empty());
}
