#![allow(clippy::expect_used)]

use std::collections::BTreeMap;
use std::sync::Arc;

use authz_resolver_sdk::{AuthZResolverApi, PolicyEnforcer};
use gts::GtsTypeId;
use quota_enforcement_sdk::testing::InMemoryStorage;
use quota_enforcement_sdk::{
    CapPatch, Decision, DecisionResult, EnforcementMode, IdempotencyScope, IdempotencySubjectKey,
    IdempotencyWrite, LeaseState, MetricKind, NotificationEventKind, OperationType, PayloadHash,
    PeriodType, PolicyId, QuotaDebitPlan, QuotaEnforcementStoragePluginV1, QuotaId, QuotaSource,
    QuotaStatus, QuotaType, SubjectRef, ValidityWindow,
};
use serde_json::{Value, json};
use time::OffsetDateTime;
use toolkit_security::AccessScope;
use uuid::Uuid;

use super::QuotaManagement;
use crate::domain::admission::Admission;
use crate::domain::catalog::{CatalogBuilder, CatalogConfig, ProjectionContractCatalog};
use crate::domain::error::{DomainError, ResourceKind};
use crate::domain::ports::metric_registry::{MetricDescriptor, MetricMode};
use crate::domain::ports::metrics::{DenialReason, ValidationReason, ValidationSurface};
use crate::domain::quotas::request::{
    CreateQuotaRequest, ListQuotasRequest, Presence, UpdateQuotaRequest,
};
use crate::domain::quotas::validation::{QuotaLimits, RATE_QUOTAS};
use crate::domain::tokens;
use crate::test_support::{
    DenyAllPdp, FakeContractRegistry, FakeMetricRegistry, LLM_TENANT_PROJECTION,
    LLM_TOKEN_CONSTRAINT, LLM_TOKEN_CONSTRAINT_V2, LLM_USER_PROJECTION, METRIC_OTHER,
    METRIC_TOKENS, PermitTenantsPdp, RecordingMetrics, ctx, gated_counter, tenant,
};

fn type_id(raw: &str) -> GtsTypeId {
    GtsTypeId::try_new(raw).expect("type id")
}

fn limits() -> QuotaLimits {
    QuotaLimits {
        metadata_max_bytes: 4096,
        list_max_limit: 500,
        list_max_ids: 100,
    }
}

struct Harness {
    catalog: ProjectionContractCatalog,
    admission: Admission,
    storage: Arc<InMemoryStorage>,
    registry: Arc<FakeContractRegistry>,
    metric_registry: Arc<FakeMetricRegistry>,
    metrics: Arc<RecordingMetrics>,
    limits: QuotaLimits,
}

impl Harness {
    /// Both `llm_gateway` projections, both metrics classified, a permitting PDP.
    async fn new() -> Self {
        Self::build(
            Arc::new(PermitTenantsPdp::new(vec![tenant().as_uuid()])),
            vec![type_id(LLM_USER_PROJECTION), type_id(LLM_TENANT_PROJECTION)],
        )
        .await
    }

    async fn with_pdp(pdp: Arc<dyn AuthZResolverApi>) -> Self {
        Self::build(
            pdp,
            vec![type_id(LLM_USER_PROJECTION), type_id(LLM_TENANT_PROJECTION)],
        )
        .await
    }

    async fn build(pdp: Arc<dyn AuthZResolverApi>, subject_projections: Vec<GtsTypeId>) -> Self {
        Self::over(
            pdp,
            subject_projections,
            Arc::new(FakeContractRegistry::llm_gateway()),
            Arc::new(InMemoryStorage::new()),
        )
        .await
    }

    /// A later process over the same storage: `registry` is what its
    /// catalogue is built from.
    async fn restarted_with(&self, registry: FakeContractRegistry) -> Self {
        Self::over(
            Arc::new(PermitTenantsPdp::new(vec![tenant().as_uuid()])),
            vec![type_id(LLM_USER_PROJECTION), type_id(LLM_TENANT_PROJECTION)],
            Arc::new(registry),
            self.storage.clone(),
        )
        .await
    }

    async fn over(
        pdp: Arc<dyn AuthZResolverApi>,
        subject_projections: Vec<GtsTypeId>,
        registry: Arc<FakeContractRegistry>,
        storage: Arc<InMemoryStorage>,
    ) -> Self {
        let metrics = Arc::new(RecordingMetrics::default());
        let catalog = CatalogBuilder::new(registry.as_ref(), metrics.as_ref())
            .build(&CatalogConfig {
                subject_projections,
                resource_projections: Vec::new(),
            })
            .await
            .expect("catalogue");
        Self {
            catalog,
            admission: Admission::new(PolicyEnforcer::new(pdp), metrics.clone()),
            storage,
            registry,
            metric_registry: Arc::new(FakeMetricRegistry::classified()),
            metrics,
            limits: limits(),
        }
    }

    fn quotas(&self) -> QuotaManagement<'_> {
        QuotaManagement::new(
            &self.admission,
            &self.catalog,
            self.storage.as_ref(),
            self.registry.as_ref(),
            self.metric_registry.as_ref(),
            self.metrics.as_ref(),
            self.limits,
        )
    }

    async fn create(&self, request: CreateQuotaRequest) -> QuotaId {
        self.quotas()
            .create(&ctx(), request)
            .await
            .expect("create")
            .quota
            .id
    }
}

fn user_subject(id: &str) -> SubjectRef {
    SubjectRef {
        projection_type: type_id(LLM_USER_PROJECTION),
        subject_id: id.to_owned(),
    }
}

fn metadata(value: &Value) -> serde_json::Map<String, Value> {
    value.as_object().cloned().expect("object")
}

/// A consumption Quota on the user projection with conforming metadata.
fn request() -> CreateQuotaRequest {
    CreateQuotaRequest {
        tenant_id: tenant(),
        subject: user_subject("u1"),
        metric: METRIC_TOKENS.to_owned(),
        quota_type: QuotaType::Consumption,
        period: Presence::Value(PeriodType::Month),
        enforcement_mode: EnforcementMode::Hard,
        cap: Some(100),
        notification_thresholds: vec![50],
        validity_window: None,
        fail_open_hint: false,
        metadata: Some(metadata(&json!({ "regions": ["eu"], "weight": 5 }))),
        source: QuotaSource::Operator,
    }
}

fn idem(op: OperationType, key: &str) -> IdempotencyWrite {
    IdempotencyWrite {
        scope: IdempotencyScope {
            tenant_id: tenant(),
            subject_key: IdempotencySubjectKey::from_bytes([1; 32]),
            operation_type: op,
            key: key.to_owned(),
        },
        payload_hash: PayloadHash::from_bytes([1; 32]),
        decision: Decision {
            result: DecisionResult::Allowed,
            debit_plan: BTreeMap::new(),
            diagnostics: BTreeMap::new(),
        },
        engine_id: "most-restrictive-wins".to_owned(),
        policy_id: PolicyId::global(),
        policy_version: 1,
    }
}

fn applicable(
    id: QuotaId,
) -> (
    quota_enforcement_sdk::ApplicableQuotas,
    BTreeMap<QuotaId, QuotaDebitPlan>,
) {
    (
        quota_enforcement_sdk::ApplicableQuotas {
            tenant_id: tenant(),
            subjects: vec![user_subject("u1")],
            metric: quota_enforcement_sdk::MetricId::parse(METRIC_TOKENS).expect("metric"),
        },
        BTreeMap::from([(id, QuotaDebitPlan { amount: 60 })]),
    )
}

fn scope() -> AccessScope {
    AccessScope::for_tenant(tenant().as_uuid())
}

// --- create ------------------------------------------------------------------

#[tokio::test]
async fn ac1_create_persists_an_active_row_and_enqueues_the_created_event() {
    let h = Harness::new().await;
    let view = h.quotas().create(&ctx(), request()).await.expect("create");
    assert_eq!(view.quota.status, QuotaStatus::Active);
    assert_eq!(view.quota.record_version, 1);
    assert_eq!(
        view.quota.constraint_contract.type_id.as_ref(),
        LLM_TOKEN_CONSTRAINT
    );
    assert_eq!(view.metric_kind, Some(MetricKind::Counter));
    assert!(view.currently_within_window);
    let stored = h.storage.quota(view.quota.id).expect("stored");
    assert_eq!(stored.metadata, view.quota.metadata);
    let events = h.storage.events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].kind, NotificationEventKind::QuotaChanged);
    assert_eq!(events[0].quota_id, Some(view.quota.id));
    assert_eq!(events[0].payload, json!({ "change_kind": "created" }));
    assert_eq!(
        h.metric_registry.calls(),
        1,
        "the registry is consulted once, before storage, never after"
    );
}

#[tokio::test]
async fn ac2_an_unregistered_metric_and_an_unreachable_registry_fail_closed_before_storage() {
    let h = Harness::new().await;
    let err = h
        .quotas()
        .create(
            &ctx(),
            CreateQuotaRequest {
                metric: METRIC_OTHER.to_owned(),
                ..request()
            },
        )
        .await
        .expect_err("unknown metric");
    assert_eq!(
        err,
        DomainError::MetricNotRegistered {
            metric: METRIC_OTHER.to_owned()
        }
    );
    h.metric_registry.fail_all();
    let err = h
        .quotas()
        .create(&ctx(), request())
        .await
        .expect_err("registry down");
    assert!(
        matches!(err, DomainError::TypesRegistryUnavailable(_)),
        "{err:?}"
    );
    assert!(h.storage.events().is_empty(), "nothing persisted");
}

#[tokio::test]
async fn ac3_membership_and_subject_scope_are_enforced_before_persistence() {
    let tenant_only = Harness::build(
        Arc::new(PermitTenantsPdp::new(vec![tenant().as_uuid()])),
        vec![type_id(LLM_TENANT_PROJECTION)],
    )
    .await;
    let err = tenant_only
        .quotas()
        .create(&ctx(), request())
        .await
        .expect_err("user projection registered but not configured");
    assert_eq!(
        err,
        DomainError::ProjectionNotResolvable {
            projection: LLM_USER_PROJECTION.to_owned()
        }
    );

    let h = Harness::new().await;
    h.metric_registry.add(METRIC_OTHER, gated_counter());
    let err = h
        .quotas()
        .create(
            &ctx(),
            CreateQuotaRequest {
                metric: METRIC_OTHER.to_owned(),
                ..request()
            },
        )
        .await
        .expect_err("registered metric the projection does not admit");
    assert_eq!(
        err,
        DomainError::InvalidArgument {
            field: "metric",
            reason: tokens::METRIC_NOT_ADMITTED
        }
    );
    assert_eq!(
        h.metrics.admitted_violations(),
        vec![ValidationSurface::Arbitration]
    );

    let tenant_subject = |id: String| CreateQuotaRequest {
        subject: SubjectRef {
            projection_type: type_id(LLM_TENANT_PROJECTION),
            subject_id: id,
        },
        ..request()
    };
    let err = h
        .quotas()
        .create(&ctx(), tenant_subject("someone-else".to_owned()))
        .await
        .expect_err("tenant scope demands the tenant id");
    assert_eq!(
        err,
        DomainError::InvalidArgument {
            field: "subject.subject_id",
            reason: tokens::SUBJECT_SCOPE_VIOLATION
        }
    );
    h.quotas()
        .create(&ctx(), tenant_subject(tenant().to_string()))
        .await
        .expect("the tenant itself");
    assert!(
        h.storage.events().len() == 1,
        "only the accepted create persisted"
    );
}

#[tokio::test]
async fn ac4_negative_caps_fail_before_the_pdp_while_zero_and_unbounded_are_counted() {
    let pdp = Arc::new(PermitTenantsPdp::new(vec![tenant().as_uuid()]));
    let h = Harness::with_pdp(pdp.clone()).await;
    let err = h
        .quotas()
        .create(
            &ctx(),
            CreateQuotaRequest {
                cap: Some(-1),
                ..request()
            },
        )
        .await
        .expect_err("negative");
    assert_eq!(err, DomainError::CapMustBeNonNegative { cap: -1 });
    assert_eq!(pdp.calls(), 0, "shape fails before the PDP");
    assert_eq!(h.metrics.denials(), vec![DenialReason::InvalidArgument]);

    h.create(CreateQuotaRequest {
        cap: Some(0),
        ..request()
    })
    .await;
    h.create(CreateQuotaRequest {
        cap: None,
        notification_thresholds: Vec::new(),
        ..request()
    })
    .await;
    let counts = h.storage.read_active_quota_counts().await.expect("counts");
    assert_eq!((counts.cap_zero, counts.cap_unbounded), (1, 1));
}

#[tokio::test]
async fn ac5_thresholds_require_a_bounded_cap_at_create_and_at_update() {
    let h = Harness::new().await;
    let err = h
        .quotas()
        .create(
            &ctx(),
            CreateQuotaRequest {
                cap: None,
                ..request()
            },
        )
        .await
        .expect_err("thresholds on unbounded");
    assert_eq!(err, DomainError::ThresholdsRequireBoundedCap);

    let id = h.create(request()).await;
    let err = h
        .quotas()
        .update(
            &ctx(),
            id,
            UpdateQuotaRequest {
                cap: Presence::Null,
                ..UpdateQuotaRequest::default()
            },
        )
        .await
        .expect_err("unbinding a row that keeps thresholds");
    assert_eq!(err, DomainError::ThresholdsRequireBoundedCap);
    let unbounded = h
        .create(CreateQuotaRequest {
            cap: None,
            notification_thresholds: Vec::new(),
            ..request()
        })
        .await;
    let err = h
        .quotas()
        .update(
            &ctx(),
            unbounded,
            UpdateQuotaRequest {
                notification_thresholds: Some(vec![50]),
                ..UpdateQuotaRequest::default()
            },
        )
        .await
        .expect_err("thresholds on an unbounded row");
    assert_eq!(err, DomainError::ThresholdsRequireBoundedCap);
}

#[tokio::test]
async fn ac6_several_quotas_per_subject_and_metric_are_accepted() {
    let h = Harness::new().await;
    let first = h.create(request()).await;
    let second = h.create(request()).await;
    assert_ne!(first, second);
}

#[tokio::test]
async fn ac7_rate_is_unimplemented_on_create_and_update_before_any_other_gate() {
    let pdp = Arc::new(PermitTenantsPdp::new(vec![tenant().as_uuid()]));
    let h = Harness::with_pdp(pdp.clone()).await;
    let err = h
        .quotas()
        .create(
            &ctx(),
            CreateQuotaRequest {
                quota_type: QuotaType::Rate,
                period: Presence::Absent,
                ..request()
            },
        )
        .await
        .expect_err("rate create");
    assert_eq!(
        err,
        DomainError::NotYetImplemented {
            feature: RATE_QUOTAS
        }
    );
    let id = h.create(request()).await;
    let err = h
        .quotas()
        .update(
            &ctx(),
            id,
            UpdateQuotaRequest {
                metric: Presence::Value(json!(METRIC_OTHER)),
                quota_type: Presence::Value(json!(QuotaType::Rate.as_gts_id())),
                ..UpdateQuotaRequest::default()
            },
        )
        .await
        .expect_err("rate update");
    assert_eq!(
        err,
        DomainError::NotYetImplemented {
            feature: RATE_QUOTAS
        }
    );
    assert_eq!(pdp.calls(), 1, "only the accepted create reached the PDP");
    assert!(
        h.metrics.denials().is_empty(),
        "a reserved capability is not an invalid-argument denial"
    );
}

#[tokio::test]
async fn ac8_the_cap_guard_is_decided_by_storage_and_raises_bypass_it() {
    let h = Harness::new().await;
    let id = h.create(request()).await;
    let (applicable, plan) = applicable(id);
    h.storage
        .apply_debit_plan(
            &ctx(),
            &scope(),
            &applicable,
            &plan,
            &idem(OperationType::Debit, "d"),
            &[],
        )
        .await
        .expect("debit 60");
    let err = h
        .quotas()
        .update(
            &ctx(),
            id,
            UpdateQuotaRequest {
                cap: Presence::Value(50),
                ..UpdateQuotaRequest::default()
            },
        )
        .await
        .expect_err("below consumed");
    assert_eq!(
        err,
        DomainError::CapBelowConsumed {
            new_cap: 50,
            consumed: 60
        }
    );
    let raised = h
        .quotas()
        .update(
            &ctx(),
            id,
            UpdateQuotaRequest {
                cap: Presence::Value(200),
                ..UpdateQuotaRequest::default()
            },
        )
        .await
        .expect("a raise bypasses the guard");
    assert_eq!(raised.quota.cap, Some(200));
    assert_eq!(raised.quota.record_version, 2);
    let unbound = h
        .quotas()
        .update(
            &ctx(),
            id,
            UpdateQuotaRequest {
                cap: Presence::Null,
                notification_thresholds: Some(Vec::new()),
                ..UpdateQuotaRequest::default()
            },
        )
        .await
        .expect("numeric to unbounded bypasses the guard");
    assert_eq!(unbound.quota.cap, None);
}

#[tokio::test]
async fn ac9_immutable_fields_are_rejected_and_identity_survives_updates() {
    let h = Harness::new().await;
    let id = h.create(request()).await;
    for (field, patch) in [
        (
            "metric",
            UpdateQuotaRequest {
                metric: Presence::Null,
                ..UpdateQuotaRequest::default()
            },
        ),
        (
            "period",
            UpdateQuotaRequest {
                period: Presence::Value(json!("gts.cf.qe.period.type.v1~cf.qe.period.day.v1")),
                ..UpdateQuotaRequest::default()
            },
        ),
        (
            "subject",
            UpdateQuotaRequest {
                subject: Presence::Value(json!({ "subject_id": "u2" })),
                ..UpdateQuotaRequest::default()
            },
        ),
    ] {
        let err = h.quotas().update(&ctx(), id, patch).await.expect_err(field);
        assert_eq!(
            err,
            DomainError::InvalidArgument {
                field,
                reason: tokens::IMMUTABLE_FIELD
            }
        );
    }
    let updated = h
        .quotas()
        .update(
            &ctx(),
            id,
            UpdateQuotaRequest {
                fail_open_hint: Some(true),
                ..UpdateQuotaRequest::default()
            },
        )
        .await
        .expect("accepted");
    assert_eq!(updated.quota.id, id);
    assert_eq!(updated.quota.subject, user_subject("u1"));
    assert!(updated.quota.fail_open_hint);
    let events = h.storage.events();
    assert_eq!(
        events.last().map(|e| e.payload.clone()),
        Some(json!({ "change_kind": "updated" }))
    );
}

#[tokio::test]
async fn ac10_metadata_is_validated_before_persistence_and_snapshotted() {
    let h = Harness::new().await;
    let small = Harness {
        limits: QuotaLimits {
            metadata_max_bytes: 4,
            ..limits()
        },
        ..Harness::new().await
    };
    let err = small
        .quotas()
        .create(&ctx(), request())
        .await
        .expect_err("oversize");
    assert_eq!(
        err,
        DomainError::InvalidArgument {
            field: "metadata",
            reason: tokens::METADATA_TOO_LARGE
        }
    );
    assert!(small.metrics.contract_failures().is_empty());

    let err = h
        .quotas()
        .create(
            &ctx(),
            CreateQuotaRequest {
                metadata: Some(metadata(&json!({ "regions": ["eu"], "weight": "heavy" }))),
                ..request()
            },
        )
        .await
        .expect_err("contract violation");
    assert_eq!(
        err,
        DomainError::ConstraintContractMismatch {
            contract: LLM_TOKEN_CONSTRAINT.to_owned()
        }
    );
    assert_eq!(
        h.metrics.contract_failures(),
        vec![(
            ValidationSurface::Arbitration,
            ValidationReason::SchemaViolation
        )]
    );
    assert!(h.storage.events().is_empty(), "rejected before persistence");

    let id = h.create(request()).await;
    let stored = h.storage.quota(id).expect("stored");
    assert_eq!(
        stored.constraint_contract.type_id.as_ref(),
        LLM_TOKEN_CONSTRAINT
    );
    assert_eq!(stored.constraint_contract.version, 1);
    let updated = h
        .quotas()
        .update(
            &ctx(),
            id,
            UpdateQuotaRequest {
                metadata: Some(metadata(&json!({ "regions": ["eu"], "weight": 9 }))),
                ..UpdateQuotaRequest::default()
            },
        )
        .await
        .expect("metadata-only update");
    assert_eq!(updated.quota.id, id);
    assert_eq!(updated.quota.metadata["weight"], json!(9));
    assert_eq!(
        updated.quota.constraint_contract,
        stored.constraint_contract
    );
    assert_eq!(
        h.storage.events().len(),
        2,
        "create and update each enqueued one event"
    );
}

/// A catalogue that moved between two processes: the stored reference names
/// the contract the current object was accepted against, never an older one.
#[tokio::test]
async fn a_metadata_update_after_a_catalogue_change_moves_the_stored_contract_reference() {
    let first = Harness::new().await;
    let id = first.create(request()).await;
    let created = first.storage.quota(id).expect("stored");
    assert_eq!(
        created.constraint_contract.type_id.as_ref(),
        LLM_TOKEN_CONSTRAINT
    );
    assert_eq!(created.constraint_contract.version, 1);

    let later = first
        .restarted_with(FakeContractRegistry::llm_gateway_v2())
        .await;
    let untouched = later
        .quotas()
        .update(
            &ctx(),
            id,
            UpdateQuotaRequest {
                fail_open_hint: Some(true),
                ..UpdateQuotaRequest::default()
            },
        )
        .await
        .expect("a patch without metadata");
    assert_eq!(
        untouched.quota.constraint_contract, created.constraint_contract,
        "only a metadata change is re-validated, so only it moves the reference"
    );

    let moved = later
        .quotas()
        .update(
            &ctx(),
            id,
            UpdateQuotaRequest {
                metadata: Some(metadata(&json!({ "regions": ["us"], "weight": 2 }))),
                ..UpdateQuotaRequest::default()
            },
        )
        .await
        .expect("metadata validated against the current catalogue");
    assert_eq!(moved.quota.metadata["weight"], json!(2));
    assert_eq!(
        moved.quota.constraint_contract.type_id.as_ref(),
        LLM_TOKEN_CONSTRAINT_V2
    );
    assert_eq!(moved.quota.constraint_contract.version, 2);
    let stored = later.storage.quota(id).expect("stored");
    assert_eq!(
        stored.constraint_contract, moved.quota.constraint_contract,
        "the row carries the reference its metadata was accepted against"
    );
    assert_eq!(stored.record_version, 3);
}

#[tokio::test]
async fn ac11_deactivation_resolves_active_leases_once_and_is_terminal() {
    let h = Harness::new().await;
    h.storage
        .bootstrap(&quota_enforcement_sdk::BootstrapBundle::foundation())
        .await
        .expect("bootstrap");
    let id = h.create(request()).await;
    let (applicable, plan) = applicable(id);
    let token = h
        .storage
        .acquire_lease(
            &ctx(),
            &scope(),
            &applicable,
            &plan,
            std::time::Duration::from_mins(1),
            &idem(OperationType::Reserve, "r"),
        )
        .await
        .expect("lease");
    let outcome = h.quotas().deactivate(&ctx(), id).await.expect("deactivate");
    assert_eq!(outcome.resolved_leases, vec![token]);
    assert_eq!(
        h.storage.lease_state(token),
        Some(LeaseState::ResolvedByDeactivation)
    );
    assert_eq!(
        h.storage.events().last().map(|e| e.payload.clone()),
        Some(json!({ "change_kind": "deactivated" }))
    );
    let again = h
        .quotas()
        .deactivate(&ctx(), id)
        .await
        .expect_err("terminal");
    assert_eq!(again, DomainError::QuotaDeactivated { id: id.to_string() });
    let patched = h
        .quotas()
        .update(
            &ctx(),
            id,
            UpdateQuotaRequest {
                fail_open_hint: Some(true),
                ..UpdateQuotaRequest::default()
            },
        )
        .await
        .expect_err("no patch after deactivation");
    assert_eq!(
        patched,
        DomainError::QuotaDeactivated { id: id.to_string() }
    );
}

#[tokio::test]
async fn ac12_deactivated_quotas_stay_readable_and_reads_are_pdp_gated() {
    let h = Harness::new().await;
    let id = h.create(request()).await;
    h.quotas().deactivate(&ctx(), id).await.expect("deactivate");
    let one = h.quotas().get(&ctx(), id).await.expect("get");
    assert_eq!(one.quota.status, QuotaStatus::Deactivated);
    assert_eq!(one.quota.metadata["weight"], json!(5));
    assert_eq!(one.metric_kind, Some(MetricKind::Counter));
    let page = h
        .quotas()
        .list(&ctx(), ListQuotasRequest::default())
        .await
        .expect("list");
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].quota.id, id);
    let missing = h
        .quotas()
        .get(&ctx(), QuotaId::new(Uuid::from_u128(404)))
        .await
        .expect_err("unknown");
    assert_eq!(
        missing,
        DomainError::NotFound {
            kind: ResourceKind::Quota,
            id: QuotaId::new(Uuid::from_u128(404)).to_string()
        }
    );

    let denied = Harness::with_pdp(Arc::new(DenyAllPdp)).await;
    let err = denied
        .quotas()
        .list(&ctx(), ListQuotasRequest::default())
        .await
        .expect_err("denied");
    assert!(matches!(err, DomainError::PdpDenied { .. }), "{err:?}");
    let err = denied
        .quotas()
        .create(&ctx(), request())
        .await
        .expect_err("denied create");
    assert!(matches!(err, DomainError::PdpDenied { .. }), "{err:?}");
    assert_eq!(
        denied.metric_registry.calls(),
        0,
        "the PDP answers before any registry call"
    );
    assert!(denied.storage.events().is_empty());
}

#[tokio::test]
async fn ac13_a_quota_on_a_direct_metric_is_accepted() {
    let h = Harness::new().await;
    h.metric_registry.add(
        METRIC_TOKENS,
        MetricDescriptor {
            kind: MetricKind::Counter,
            mode: MetricMode::Direct,
        },
    );
    let view = h
        .quotas()
        .create(&ctx(), request())
        .await
        .expect("accepted");
    assert_eq!(view.quota.status, QuotaStatus::Active);
}

#[tokio::test]
async fn ac14_a_past_validity_end_flips_the_flag_and_never_the_status() {
    let h = Harness::new().await;
    let past = OffsetDateTime::from_unix_timestamp(1_000).expect("timestamp");
    let id = h
        .create(CreateQuotaRequest {
            validity_window: Some(ValidityWindow {
                start: None,
                end: Some(past),
            }),
            ..request()
        })
        .await;
    let view = h.quotas().get(&ctx(), id).await.expect("get");
    assert_eq!(view.quota.status, QuotaStatus::Active);
    assert!(!view.currently_within_window);
}

// --- precedence and the read path ---------------------------------------------

#[tokio::test]
async fn a_shape_failure_wins_over_a_denying_pdp_and_a_denial_over_the_registry() {
    let h = Harness::with_pdp(Arc::new(DenyAllPdp)).await;
    let err = h
        .quotas()
        .create(
            &ctx(),
            CreateQuotaRequest {
                subject: user_subject(""),
                ..request()
            },
        )
        .await
        .expect_err("shape");
    assert_eq!(
        err,
        DomainError::InvalidArgument {
            field: "subject.subject_id",
            reason: tokens::SUBJECT_ID_REQUIRED
        }
    );
    let err = h
        .quotas()
        .create(
            &ctx(),
            CreateQuotaRequest {
                metric: METRIC_OTHER.to_owned(),
                ..request()
            },
        )
        .await
        .expect_err("denied");
    assert!(matches!(err, DomainError::PdpDenied { .. }));
    assert_eq!(h.metric_registry.calls(), 0);
}

#[tokio::test]
async fn reads_fail_closed_on_a_cold_registry_and_tolerate_a_removed_metric() {
    let h = Harness::new().await;
    let id = h.create(request()).await;
    h.metric_registry.fail_all();
    let err = h
        .quotas()
        .get(&ctx(), id)
        .await
        .expect_err("cold cache, registry down");
    assert!(
        matches!(err, DomainError::TypesRegistryUnavailable(_)),
        "{err:?}"
    );
    h.metric_registry.recover();
    h.metric_registry.remove(METRIC_TOKENS);
    let view = h.quotas().get(&ctx(), id).await.expect("still readable");
    assert_eq!(view.metric_kind, None, "unknown, never defaulted");
    let page = h
        .quotas()
        .list(&ctx(), ListQuotasRequest::default())
        .await
        .expect("list");
    assert_eq!(page.items[0].metric_kind, None);
}

#[tokio::test]
async fn the_in_process_conversions_keep_caps_in_range() {
    let h = Harness::new().await;
    let spec = quota_enforcement_sdk::QuotaSpec {
        tenant_id: tenant(),
        subject: user_subject("u1"),
        metric: quota_enforcement_sdk::MetricId::parse(METRIC_TOKENS).expect("metric"),
        quota_type: QuotaType::Consumption,
        period: Some(PeriodType::Month),
        enforcement_mode: EnforcementMode::Hard,
        cap: Some(u64::MAX),
        notification_thresholds: Vec::new(),
        validity_window: None,
        fail_open_hint: false,
        metadata: serde_json::Map::new(),
        source: QuotaSource::Operator,
    };
    let err = CreateQuotaRequest::try_from(spec.clone()).expect_err("above i64::MAX");
    assert_eq!(
        err,
        DomainError::InvalidArgument {
            field: "cap",
            reason: tokens::CAP_OUT_OF_RANGE
        }
    );
    let max = CreateQuotaRequest::try_from(quota_enforcement_sdk::QuotaSpec {
        cap: Some(quota_enforcement_sdk::Quota::MAX_CAP),
        metadata: json!({ "regions": ["eu"], "weight": 5 })
            .as_object()
            .cloned()
            .expect("object"),
        ..spec
    })
    .expect("the largest supported cap");
    let view = h
        .quotas()
        .create(&ctx(), max)
        .await
        .expect("accepted end to end");
    assert_eq!(view.quota.cap, Some(quota_enforcement_sdk::Quota::MAX_CAP));
    let patch = quota_enforcement_sdk::QuotaPatch {
        cap: Some(CapPatch::Bounded(u64::MAX)),
        ..quota_enforcement_sdk::QuotaPatch::default()
    };
    let err = UpdateQuotaRequest::try_from(patch).expect_err("above i64::MAX");
    assert_eq!(
        err,
        DomainError::InvalidArgument {
            field: "cap",
            reason: tokens::CAP_OUT_OF_RANGE
        }
    );
}
