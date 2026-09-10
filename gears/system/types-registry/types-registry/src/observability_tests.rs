//! The span constructors' contract: fields, their spellings, and what is absent before the facts
//! are known.

use crate::domain::enums::OperationKind;

#[test]
#[tracing_test::traced_test]
fn the_operation_span_carries_the_operation_id_kind_and_dry_run_mode() {
    let operation_id = uuid::Uuid::from_u128(0x1234);
    let span = super::operation_span(operation_id);
    super::record_operation_facts(&span, OperationKind::Registration, false);
    let _entered = span.enter();
    tracing::info!("probe");

    assert!(logs_contain(&operation_id.to_string()));
    assert!(logs_contain(r#"kind="registration""#));
    assert!(logs_contain("dry_run=false"));
}

#[test]
#[tracing_test::traced_test]
fn the_operation_span_shows_no_kind_before_the_operation_is_read() {
    let span = super::operation_span(uuid::Uuid::from_u128(0x99));
    let _entered = span.enter();
    tracing::info!("probe");

    assert!(!logs_contain("kind="));
}

#[test]
#[tracing_test::traced_test]
fn the_unit_span_carries_the_candidate_identifier_beside_the_operation_facts() {
    let operation_id = uuid::Uuid::from_u128(0x5678);
    let span = super::unit_span(
        operation_id,
        "cf.core.example.type.v1~",
        OperationKind::Registration,
        true,
        42,
    );
    let _entered = span.enter();
    tracing::info!("probe");

    assert!(logs_contain(r#"gts_id="cf.core.example.type.v1~""#));
    assert!(logs_contain(&operation_id.to_string()));
    assert!(logs_contain(r#"kind="registration""#));
    assert!(logs_contain("dry_run=true"));
    assert!(logs_contain("operation_item_id=42"));
}

#[test]
#[tracing_test::traced_test]
fn a_deletion_operation_is_labelled_deletion() {
    let span = super::operation_span(uuid::Uuid::from_u128(0x7));
    super::record_operation_facts(&span, OperationKind::Deletion, false);
    let _entered = span.enter();
    tracing::info!("probe");

    assert!(logs_contain(r#"kind="deletion""#));
}
