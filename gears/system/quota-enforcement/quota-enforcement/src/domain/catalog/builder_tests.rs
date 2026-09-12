#![allow(clippy::expect_used)]

use std::collections::HashSet;

use gts::GtsTypeId;
use quota_enforcement_sdk::{
    METRIC_BASE_TYPE, MetricId, ProjectionBinding, SCOPE_TYPE, SUBJECT_BASE, SubjectScope,
};
use serde_json::{Value, json};

use crate::domain::catalog::CatalogMiss;
use crate::domain::catalog::{CatalogBuilder, CatalogConfig, ProjectionContractCatalog};
use crate::domain::error::DomainError;
use crate::domain::ports::contracts::RegisteredType;
use crate::domain::ports::metrics::{ValidationReason, ValidationSurface};
use crate::test_support::{
    FakeContractRegistry, LLM_MODEL_RESOURCE, LLM_REQUEST_COUNT_REQUEST, LLM_TENANT_PROJECTION,
    LLM_TOKEN_CONSTRAINT, LLM_TOKEN_REQUEST, LLM_USER_PROJECTION, METRIC_OTHER, METRIC_REQUESTS,
    METRIC_TOKENS, MIXIN_REQUEST, RecordingMetrics, llm_gateway_documents, resolve_documents,
};

fn type_id(raw: &str) -> GtsTypeId {
    GtsTypeId::try_new(raw).expect("type id")
}

fn metric(raw: &str) -> MetricId {
    MetricId::parse(raw).expect("metric id")
}

fn llm_config() -> CatalogConfig {
    CatalogConfig {
        subject_projections: vec![type_id(LLM_USER_PROJECTION), type_id(LLM_TENANT_PROJECTION)],
        resource_projections: vec![type_id(LLM_MODEL_RESOURCE)],
    }
}

async fn build(
    registry: &FakeContractRegistry,
    config: &CatalogConfig,
) -> (
    Result<ProjectionContractCatalog, DomainError>,
    RecordingMetrics,
) {
    let metrics = RecordingMetrics::default();
    let outcome = CatalogBuilder::new(registry, &metrics).build(config).await;
    (outcome, metrics)
}

/// The resolved `llm_gateway` type `id`, for fixtures that alter it.
fn resolved(id: &str) -> RegisteredType {
    resolve_documents(&llm_gateway_documents())
        .into_iter()
        .find(|r| r.id.as_ref() == id)
        .expect("fixture type")
}

/// Asserts a rejection with `reason`, recorded once on the bootstrap surface.
fn assert_rejected(
    outcome: Result<ProjectionContractCatalog, DomainError>,
    metrics: &RecordingMetrics,
    reason: ValidationReason,
    subject_contains: &str,
) {
    match outcome {
        Err(DomainError::CatalogInvalid {
            reason: got,
            subject,
        }) => {
            assert_eq!(got, reason, "{subject}");
            assert!(subject.contains(subject_contains), "{subject}");
        }
        Err(other) => panic!("expected CatalogInvalid({reason}), got {other:?}"),
        Ok(_) => panic!("expected CatalogInvalid({reason}), got a catalogue"),
    }
    assert_eq!(
        metrics.contract_failures().last(),
        Some(&(ValidationSurface::Bootstrap, reason)),
        "the rejection is recorded on the bootstrap surface"
    );
}

#[tokio::test]
async fn the_llm_gateway_set_builds_a_catalogue_with_the_reverse_index() {
    let registry = FakeContractRegistry::llm_gateway();
    let (outcome, metrics) = build(&registry, &llm_config()).await;
    let catalog = outcome.expect("the reviewed owner set is consistent");
    assert!(metrics.contract_failures().is_empty());

    let tokens = metric(METRIC_TOKENS);
    let requests = metric(METRIC_REQUESTS);
    assert_eq!(
        catalog
            .map_subject(&tokens, &SubjectScope::user())
            .expect("user admits tokens")
            .as_ref(),
        LLM_USER_PROJECTION
    );
    assert_eq!(
        catalog
            .map_subject(&requests, &SubjectScope::tenant())
            .expect("tenant admits requests")
            .as_ref(),
        LLM_TENANT_PROJECTION
    );
    assert_eq!(
        catalog.map_subject(&metric(METRIC_OTHER), &SubjectScope::user()),
        Err(CatalogMiss::MetricNotAdmitted)
    );
    let group = SubjectScope::parse("gts.cf.core.qe.scope.v1~cf.core.qe.group.v1").expect("scope");
    assert_eq!(
        catalog.map_subject(&tokens, &group),
        Err(CatalogMiss::KindUnknown)
    );

    let request = catalog
        .request_contract(&tokens)
        .expect("one request contract per admitted metric");
    assert_eq!(request.type_id.as_ref(), LLM_TOKEN_REQUEST);
    assert_eq!(
        request.constraint.reference.type_id.as_ref(),
        LLM_TOKEN_CONSTRAINT
    );
    assert_eq!(request.constraint.reference.version, 1);
    assert!(
        catalog
            .resource_projection(&type_id(LLM_MODEL_RESOURCE))
            .is_some()
    );
    assert!(catalog.admits(&type_id(LLM_USER_PROJECTION), &tokens));
    assert!(!catalog.admits(&type_id(LLM_MODEL_RESOURCE), &tokens));
    assert!(catalog.knows_scope(&SubjectScope::user()));
    assert!(registry.read_calls() > 0);
}

#[tokio::test]
async fn scope_cascade_is_the_intended_arrangement_and_a_kind_miss_is_distinct() {
    // Both llm_gateway scopes admit both metrics: two distinct scopes of one
    // owner never collide. A tenant-only configuration then knows no user scope
    // for those metrics.
    let registry = FakeContractRegistry::llm_gateway();
    let tenant_only = CatalogConfig {
        subject_projections: vec![type_id(LLM_TENANT_PROJECTION)],
        resource_projections: Vec::new(),
    };
    let (outcome, _) = build(&registry, &tenant_only).await;
    let catalog = outcome.expect("one scope is a valid configuration");
    assert_eq!(
        catalog.map_subject(&metric(METRIC_TOKENS), &SubjectScope::user()),
        Err(CatalogMiss::KindUnknown),
        "no configured projection declares the user scope"
    );
}

#[tokio::test]
async fn an_empty_configuration_builds_an_empty_catalogue_without_discovery() {
    let registry = FakeContractRegistry::llm_gateway();
    let (outcome, _) = build(&registry, &CatalogConfig::default()).await;
    let catalog = outcome.expect("nothing configured, nothing to check");
    assert_eq!(registry.read_calls(), 0, "no projection, no registry read");
    assert_eq!(catalog.subject_projections().count(), 0);
    assert_eq!(catalog.admitted_metrics().count(), 0);
    assert_eq!(
        catalog.map_subject(&metric(METRIC_TOKENS), &SubjectScope::tenant()),
        Err(CatalogMiss::MetricNotAdmitted)
    );
}

#[tokio::test]
async fn an_unregistered_abstract_or_foreign_projection_is_rejected() {
    let registry = FakeContractRegistry::llm_gateway();
    let with = |id: &str| CatalogConfig {
        subject_projections: vec![type_id(id)],
        resource_projections: Vec::new(),
    };

    let (outcome, metrics) = build(
        &registry,
        &with("gts.cf.core.qe.subj.v1~cf.nobody.owner.user.v1~"),
    )
    .await;
    assert_rejected(outcome, &metrics, ValidationReason::Unregistered, "nobody");

    let (outcome, metrics) = build(&registry, &with(SUBJECT_BASE)).await;
    assert_rejected(outcome, &metrics, ValidationReason::Abstract, SUBJECT_BASE);

    let (outcome, metrics) = build(&registry, &with(LLM_MODEL_RESOURCE)).await;
    assert_rejected(
        outcome,
        &metrics,
        ValidationReason::NotDerived,
        LLM_MODEL_RESOURCE,
    );
}

#[tokio::test]
async fn a_projection_whose_scope_trait_is_not_a_registered_scope_is_rejected() {
    let registry = FakeContractRegistry::llm_gateway();
    let mut not_a_scope = resolved(LLM_USER_PROJECTION);
    not_a_scope.effective_traits["scope"] = Value::String(METRIC_TOKENS.to_owned());
    registry.add_type(not_a_scope);
    let (outcome, metrics) = build(&registry, &llm_config()).await;
    assert_rejected(
        outcome,
        &metrics,
        ValidationReason::ScopeInvalid,
        LLM_USER_PROJECTION,
    );

    let registry = FakeContractRegistry::llm_gateway();
    let mut unregistered_scope = resolved(LLM_USER_PROJECTION);
    unregistered_scope.effective_traits["scope"] =
        Value::String("gts.cf.core.qe.scope.v1~cf.core.qe.group.v1".to_owned());
    registry.add_type(unregistered_scope);
    let (outcome, metrics) = build(&registry, &llm_config()).await;
    assert_rejected(outcome, &metrics, ValidationReason::ScopeInvalid, "group");
}

#[tokio::test]
async fn admitted_metrics_must_be_registered_instances_of_the_metric_base() {
    let registry = FakeContractRegistry::llm_gateway();
    registry.remove_instance(METRIC_REQUESTS);
    let (outcome, metrics) = build(&registry, &llm_config()).await;
    assert_rejected(
        outcome,
        &metrics,
        ValidationReason::MetricUnregistered,
        METRIC_REQUESTS,
    );

    let registry = FakeContractRegistry::llm_gateway();
    registry.add_instance(METRIC_REQUESTS, SCOPE_TYPE);
    let (outcome, metrics) = build(&registry, &llm_config()).await;
    assert_rejected(
        outcome,
        &metrics,
        ValidationReason::MetricNotInstance,
        METRIC_REQUESTS,
    );

    let registry = FakeContractRegistry::llm_gateway();
    let mut foreign = resolved(LLM_USER_PROJECTION);
    foreign.effective_traits["admitted_metrics"] = json!([SCOPE_TYPE]);
    registry.add_type(foreign);
    let (outcome, metrics) = build(&registry, &llm_config()).await;
    assert_rejected(
        outcome,
        &metrics,
        ValidationReason::MetricNotInstance,
        SCOPE_TYPE,
    );
}

#[tokio::test]
async fn two_projections_admitting_one_metric_at_one_scope_are_rejected_not_ranked() {
    let registry = FakeContractRegistry::llm_gateway();
    let mut rival = resolved(LLM_USER_PROJECTION);
    rival.id = type_id("gts.cf.core.qe.subj.v1~cf.other.owner.user.v1~");
    registry.add_type(rival.clone());
    let config = CatalogConfig {
        subject_projections: vec![type_id(LLM_USER_PROJECTION), rival.id.clone()],
        resource_projections: Vec::new(),
    };
    let (outcome, metrics) = build(&registry, &config).await;
    assert_rejected(outcome, &metrics, ValidationReason::DuplicatePair, "at ");
    assert_eq!(
        metrics.admitted_violations(),
        vec![ValidationSurface::Bootstrap],
        "a duplicate pair is also a projection/metric incompatibility"
    );
}

#[tokio::test]
async fn every_admitted_metric_needs_exactly_one_request_contract() {
    let registry = FakeContractRegistry::llm_gateway();
    registry.remove_type(LLM_REQUEST_COUNT_REQUEST);
    let (outcome, metrics) = build(&registry, &llm_config()).await;
    assert_rejected(
        outcome,
        &metrics,
        ValidationReason::RequestContractMissing,
        METRIC_REQUESTS,
    );

    let registry = FakeContractRegistry::llm_gateway();
    let mut second = resolved(LLM_TOKEN_REQUEST);
    second.id = type_id("gts.cf.core.qe.request.v1~cf.other.owner.token.v1~");
    registry.add_type(second);
    let (outcome, metrics) = build(&registry, &llm_config()).await;
    assert_rejected(
        outcome,
        &metrics,
        ValidationReason::RequestContractAmbiguous,
        METRIC_TOKENS,
    );
}

#[tokio::test]
async fn the_attached_constraint_contract_must_be_a_registered_concrete_constraint() {
    let registry = FakeContractRegistry::llm_gateway();
    registry.remove_type(LLM_TOKEN_CONSTRAINT);
    let (outcome, metrics) = build(&registry, &llm_config()).await;
    assert_rejected(
        outcome,
        &metrics,
        ValidationReason::ConstraintInvalid,
        LLM_TOKEN_CONSTRAINT,
    );

    let registry = FakeContractRegistry::llm_gateway();
    let mut points_at_resource = resolved(LLM_TOKEN_REQUEST);
    points_at_resource.effective_traits["constraint_contract"] =
        Value::String(LLM_MODEL_RESOURCE.to_owned());
    registry.add_type(points_at_resource);
    let (outcome, metrics) = build(&registry, &llm_config()).await;
    assert_rejected(
        outcome,
        &metrics,
        ValidationReason::ConstraintInvalid,
        LLM_MODEL_RESOURCE,
    );
}

#[tokio::test]
async fn an_unrelated_broken_contract_does_not_block_startup_but_a_selected_one_does() {
    // A request contract for a metric nobody configured admits, with a
    // reference graph that does not resolve: discovery lists it, nothing
    // resolves it.
    let registry = FakeContractRegistry::llm_gateway();
    registry.add_broken(
        MIXIN_REQUEST,
        json!({ "metric": METRIC_OTHER, "constraint_contract": LLM_TOKEN_CONSTRAINT }),
    );
    let (outcome, _) = build(&registry, &llm_config()).await;
    outcome.expect("an unrelated owner's breakage is not this deployment's problem");

    // The same breakage on the only contract of an admitted metric is.
    let registry = FakeContractRegistry::llm_gateway();
    registry.remove_type(LLM_TOKEN_REQUEST);
    registry.add_broken(
        MIXIN_REQUEST,
        json!({ "metric": METRIC_TOKENS, "constraint_contract": LLM_TOKEN_CONSTRAINT }),
    );
    let (outcome, _) = build(&registry, &llm_config()).await;
    assert!(
        matches!(outcome, Err(DomainError::TypesRegistryUnavailable(_))),
        "{outcome:?}"
    );
}

#[tokio::test]
async fn a_registry_failure_is_unavailability_not_a_catalogue_rejection() {
    let registry = FakeContractRegistry::llm_gateway();
    registry.fail_all();
    let (outcome, metrics) = build(&registry, &llm_config()).await;
    assert!(
        matches!(outcome, Err(DomainError::TypesRegistryUnavailable(_))),
        "{outcome:?}"
    );
    assert!(metrics.contract_failures().is_empty());
}

#[tokio::test]
async fn compatibility_holds_when_every_active_binding_is_admitted() {
    let registry = FakeContractRegistry::llm_gateway();
    let metrics = RecordingMetrics::default();
    let builder = CatalogBuilder::new(&registry, &metrics);
    let catalog = builder.build(&llm_config()).await.expect("catalogue");

    let admitted: HashSet<ProjectionBinding> = [
        ProjectionBinding {
            metric: metric(METRIC_TOKENS),
            projection_type: type_id(LLM_USER_PROJECTION),
        },
        ProjectionBinding {
            metric: metric(METRIC_REQUESTS),
            projection_type: type_id(LLM_TENANT_PROJECTION),
        },
    ]
    .into_iter()
    .collect();
    builder
        .check_compatibility(&catalog, &admitted)
        .expect("compatible");
    builder
        .check_compatibility(&catalog, &HashSet::new())
        .expect("no Quotas, nothing to strand");

    let stranded: HashSet<ProjectionBinding> = [ProjectionBinding {
        metric: metric(METRIC_OTHER),
        projection_type: type_id(LLM_USER_PROJECTION),
    }]
    .into_iter()
    .collect();
    let err = builder
        .check_compatibility(&catalog, &stranded)
        .expect_err("a Quota on a metric the projection no longer admits is stranded");
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

    let unconfigured: HashSet<ProjectionBinding> = [ProjectionBinding {
        metric: metric(METRIC_TOKENS),
        projection_type: type_id("gts.cf.core.qe.subj.v1~cf.other.owner.user.v1~"),
    }]
    .into_iter()
    .collect();
    assert!(
        builder
            .check_compatibility(&catalog, &unconfigured)
            .is_err(),
        "a Quota bound to a projection outside the catalogue is stranded"
    );
    assert_eq!(
        metrics.contract_failures(),
        vec![
            (
                ValidationSurface::Bootstrap,
                ValidationReason::IncompatibleState
            ),
            (
                ValidationSurface::Bootstrap,
                ValidationReason::IncompatibleState
            ),
        ]
    );
    let _ = METRIC_BASE_TYPE;
}
