#![allow(clippy::expect_used)]

use gts::GtsTypeId;
use quota_enforcement_sdk::{MetricId, SUBJECT_BASE};
use serde_json::Value;

use crate::domain::catalog::{
    CatalogBuilder, CatalogConfig, ProjectionContractCatalog, check_projection_reference,
};
use crate::domain::error::DomainError;
use crate::domain::ports::metrics::{ValidationReason, ValidationSurface};
use crate::domain::tokens;
use crate::test_support::{
    FakeContractRegistry, LLM_MODEL_RESOURCE, LLM_TENANT_PROJECTION, LLM_USER_PROJECTION,
    METRIC_OTHER, METRIC_TOKENS, RecordingMetrics, llm_gateway_documents, resolve_documents,
};

fn type_id(raw: &str) -> GtsTypeId {
    GtsTypeId::try_new(raw).expect("type id")
}

/// A catalogue configured with the user projection only.
async fn user_only_catalog() -> ProjectionContractCatalog {
    let registry = FakeContractRegistry::llm_gateway();
    let metrics = RecordingMetrics::default();
    CatalogBuilder::new(&registry, &metrics)
        .build(&CatalogConfig {
            subject_projections: vec![type_id(LLM_USER_PROJECTION)],
            resource_projections: Vec::new(),
        })
        .await
        .expect("catalogue")
}

fn snapshot(id: &str) -> crate::domain::ports::contracts::RegisteredType {
    resolve_documents(&llm_gateway_documents())
        .into_iter()
        .find(|r| r.id.as_ref() == id)
        .expect("fixture")
}

#[tokio::test]
async fn a_configured_projection_admitting_the_metric_passes() {
    let catalog = user_only_catalog().await;
    let metrics = RecordingMetrics::default();
    check_projection_reference(
        &catalog,
        &metrics,
        ValidationSurface::Arbitration,
        Some(&snapshot(LLM_USER_PROJECTION)),
        &type_id(LLM_USER_PROJECTION),
        Some(&MetricId::parse(METRIC_TOKENS).expect("metric")),
    )
    .expect("pass");
    check_projection_reference(
        &catalog,
        &metrics,
        ValidationSurface::PolicyPair,
        Some(&snapshot(LLM_USER_PROJECTION)),
        &type_id(LLM_USER_PROJECTION),
        None,
    )
    .expect("a Policy reference carries no metric");
    assert!(metrics.contract_failures().is_empty());
    assert!(metrics.admitted_violations().is_empty());
}

#[tokio::test]
async fn an_unregistered_reference_is_not_registered() {
    let catalog = user_only_catalog().await;
    let metrics = RecordingMetrics::default();
    let projection = type_id("gts.cf.core.qe.subj.v1~cf.nobody.owner.user.v1~");
    let err = check_projection_reference(
        &catalog,
        &metrics,
        ValidationSurface::Arbitration,
        None,
        &projection,
        None,
    )
    .expect_err("unregistered");
    assert_eq!(
        err,
        DomainError::ProjectionNotRegistered {
            projection: projection.to_string(),
        }
    );
    assert_eq!(
        metrics.contract_failures(),
        vec![(
            ValidationSurface::Arbitration,
            ValidationReason::Unregistered
        )]
    );
}

#[tokio::test]
async fn an_abstract_non_subject_or_unknown_scope_reference_is_invalid() {
    let catalog = user_only_catalog().await;
    let metrics = RecordingMetrics::default();
    let invalid = |snap: &crate::domain::ports::contracts::RegisteredType| {
        check_projection_reference(
            &catalog,
            &metrics,
            ValidationSurface::PolicyPair,
            Some(snap),
            &snap.id,
            None,
        )
    };

    let base = resolve_documents(&[])
        .into_iter()
        .find(|r| r.id.as_ref() == SUBJECT_BASE)
        .expect("base");
    assert_eq!(
        invalid(&base),
        Err(DomainError::InvalidArgument {
            field: "subject.projection_type",
            reason: tokens::PROJECTION_INVALID,
        })
    );
    assert!(
        invalid(&snapshot(LLM_MODEL_RESOURCE)).is_err(),
        "not a subject"
    );
    let mut bad_scope = snapshot(LLM_TENANT_PROJECTION);
    bad_scope.effective_traits["scope"] = Value::String(METRIC_TOKENS.to_owned());
    assert!(invalid(&bad_scope).is_err(), "unknown scope");
    assert_eq!(
        metrics.contract_failures(),
        vec![
            (ValidationSurface::PolicyPair, ValidationReason::Abstract),
            (ValidationSurface::PolicyPair, ValidationReason::NotDerived),
            (
                ValidationSurface::PolicyPair,
                ValidationReason::ScopeInvalid
            ),
        ]
    );
}

#[tokio::test]
async fn a_registered_projection_outside_the_catalogue_is_not_resolvable() {
    let catalog = user_only_catalog().await;
    let metrics = RecordingMetrics::default();
    let err = check_projection_reference(
        &catalog,
        &metrics,
        ValidationSurface::Arbitration,
        Some(&snapshot(LLM_TENANT_PROJECTION)),
        &type_id(LLM_TENANT_PROJECTION),
        Some(&MetricId::parse(METRIC_TOKENS).expect("metric")),
    )
    .expect_err("registered, valid, not configured");
    assert_eq!(
        err,
        DomainError::ProjectionNotResolvable {
            projection: LLM_TENANT_PROJECTION.to_owned(),
        }
    );
    assert_eq!(
        metrics.contract_failures(),
        vec![(
            ValidationSurface::Arbitration,
            ValidationReason::ProjectionNotResolvable
        )]
    );
}

#[tokio::test]
async fn a_quota_on_a_metric_the_projection_does_not_admit_is_rejected() {
    let catalog = user_only_catalog().await;
    let metrics = RecordingMetrics::default();
    let err = check_projection_reference(
        &catalog,
        &metrics,
        ValidationSurface::Arbitration,
        Some(&snapshot(LLM_USER_PROJECTION)),
        &type_id(LLM_USER_PROJECTION),
        Some(&MetricId::parse(METRIC_OTHER).expect("metric")),
    )
    .expect_err("not admitted");
    assert_eq!(
        err,
        DomainError::InvalidArgument {
            field: "metric",
            reason: tokens::METRIC_NOT_ADMITTED,
        }
    );
    assert_eq!(
        metrics.admitted_violations(),
        vec![ValidationSurface::Arbitration]
    );
    assert!(metrics.contract_failures().is_empty());
}
