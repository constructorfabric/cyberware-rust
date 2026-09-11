#![allow(clippy::expect_used)]

use gts::GtsTypeId;
use quota_enforcement_sdk::MetricId;
use serde_json::{Map, Value, json};

use super::validate_metadata;
use crate::domain::catalog::{
    CatalogBuilder, CatalogConfig, ConstraintContract, ProjectionContractCatalog,
};
use crate::domain::error::DomainError;
use crate::domain::ports::metrics::{ValidationReason, ValidationSurface};
use crate::domain::tokens;
use crate::test_support::{
    FakeContractRegistry, LLM_TENANT_PROJECTION, LLM_TOKEN_CONSTRAINT, LLM_USER_PROJECTION,
    METRIC_TOKENS, RecordingMetrics,
};

async fn catalog() -> ProjectionContractCatalog {
    let registry = FakeContractRegistry::llm_gateway();
    let metrics = RecordingMetrics::default();
    CatalogBuilder::new(&registry, &metrics)
        .build(&CatalogConfig {
            subject_projections: vec![
                GtsTypeId::try_new(LLM_USER_PROJECTION).expect("type"),
                GtsTypeId::try_new(LLM_TENANT_PROJECTION).expect("type"),
            ],
            resource_projections: Vec::new(),
        })
        .await
        .expect("catalogue")
}

fn token_constraint(catalog: &ProjectionContractCatalog) -> &ConstraintContract {
    &catalog
        .request_contract(&MetricId::parse(METRIC_TOKENS).expect("metric"))
        .expect("token request contract")
        .constraint
}

fn object(value: &Value) -> Map<String, Value> {
    value.as_object().cloned().expect("object")
}

#[tokio::test]
async fn conforming_metadata_returns_the_contract_reference_and_records_nothing() {
    let catalog = catalog().await;
    let metrics = RecordingMetrics::default();
    let reference = validate_metadata(
        &object(&json!({ "regions": ["eu"], "weight": 5 })),
        token_constraint(&catalog),
        4096,
        &metrics,
    )
    .expect("conforming");
    assert_eq!(reference.type_id.as_ref(), LLM_TOKEN_CONSTRAINT);
    assert_eq!(reference.version, 1);
    assert!(metrics.contract_failures().is_empty());
}

#[tokio::test]
async fn a_contract_violation_is_a_failed_precondition_recorded_on_the_arbitration_surface() {
    let catalog = catalog().await;
    let metrics = RecordingMetrics::default();
    let err = validate_metadata(
        &object(&json!({ "regions": ["eu"], "weight": "heavy" })),
        token_constraint(&catalog),
        4096,
        &metrics,
    )
    .expect_err("the contract types weight");
    assert_eq!(
        err,
        DomainError::ConstraintContractMismatch {
            contract: LLM_TOKEN_CONSTRAINT.to_owned()
        }
    );
    assert_eq!(
        metrics.contract_failures(),
        vec![(
            ValidationSurface::Arbitration,
            ValidationReason::SchemaViolation
        )]
    );
}

#[tokio::test]
async fn the_size_limit_applies_to_the_canonical_json_and_is_checked_first() {
    let catalog = catalog().await;
    let metrics = RecordingMetrics::default();
    let metadata = object(&json!({ "regions": ["eu"], "weight": 5 }));
    let exact = serde_json::to_vec(&metadata).expect("json").len();
    validate_metadata(&metadata, token_constraint(&catalog), exact, &metrics)
        .expect("exactly at the limit");
    let err = validate_metadata(&metadata, token_constraint(&catalog), exact - 1, &metrics)
        .expect_err("one byte over");
    assert_eq!(
        err,
        DomainError::InvalidArgument {
            field: "metadata",
            reason: tokens::METADATA_TOO_LARGE
        }
    );
    assert!(
        metrics.contract_failures().is_empty(),
        "a size rejection is not a contract violation"
    );
    // Key order does not change the measured size. The size is the invariant,
    // not the bytes: whether `serde_json::Map` sorts keys or keeps insertion
    // order depends on whose `preserve_order` feature is unified into the
    // build, and reordering keys cannot change the byte count either way.
    let reordered = object(&json!({ "b": 1, "a": 2 }));
    let ordered = object(&json!({ "a": 2, "b": 1 }));
    assert_eq!(
        serde_json::to_vec(&reordered).expect("json").len(),
        serde_json::to_vec(&ordered).expect("json").len()
    );
}
