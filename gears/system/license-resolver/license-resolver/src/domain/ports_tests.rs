//! Unit tests for the metric-label vocabularies.

use super::*;

/// Each resolver reason keeps its own label, and the label is the wire code
/// itself, so a dashboard built on `reason` keeps working.
#[test]
fn every_resolver_reason_keeps_its_own_label() {
    for reason in [
        field::CONTRACT_NOT_REGISTERED,
        field::CONTRACT_TYPE_MALFORMED,
        field::CONTRACT_NOT_DERIVED,
        field::CONTRACT_ABSTRACT,
        field::SCHEMA_MISMATCH,
        field::SUBJECT_NOT_ADMITTED,
    ] {
        assert_eq!(
            ViolationKind::from(reason).as_label(),
            reason,
            "reason {reason} must label as itself"
        );
    }
}

/// The label space is closed by construction: anything a backend raises, or a
/// newer SDK adds, lands in one bucket instead of widening the metric.
#[test]
fn any_foreign_reason_collapses_into_one_label() {
    for foreign in [
        "BACKEND_SPECIFIC_CODE",
        "CONTRACT_EXPIRED",
        "",
        "model_name=gpt-4o rejected upstream",
    ] {
        assert_eq!(
            ViolationKind::from(foreign).as_label(),
            "other",
            "foreign reason {foreign:?} must not become its own label"
        );
    }
}

#[test]
fn violation_labels_are_distinct() {
    let labels = [
        ViolationKind::ContractNotRegistered,
        ViolationKind::ContractTypeMalformed,
        ViolationKind::ContractNotDerived,
        ViolationKind::ContractAbstract,
        ViolationKind::SchemaMismatch,
        ViolationKind::SubjectNotAdmitted,
        ViolationKind::Other,
    ]
    .map(ViolationKind::as_label);
    let mut unique = labels.to_vec();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(unique.len(), labels.len(), "labels must be distinct");
}

#[test]
fn check_outcome_labels_are_distinct() {
    let labels = [
        CheckOutcome::Granted,
        CheckOutcome::NotGranted,
        CheckOutcome::InvalidRequest,
        CheckOutcome::NoPlugin,
        CheckOutcome::Unavailable,
        CheckOutcome::Unauthorized,
        CheckOutcome::Error,
    ]
    .map(CheckOutcome::as_label);
    let mut unique = labels.to_vec();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(unique.len(), labels.len(), "labels must be distinct");
}

#[test]
fn every_domain_error_maps_onto_its_check_outcome() {
    use crate::domain::DomainError;

    let cases = [
        (
            DomainError::ContractViolation { violations: vec![] },
            CheckOutcome::InvalidRequest,
        ),
        (
            DomainError::PluginNotFound {
                vendor: "acme".to_owned(),
            },
            CheckOutcome::NoPlugin,
        ),
        (
            DomainError::TypesRegistryUnavailable("down".to_owned()),
            CheckOutcome::Unavailable,
        ),
        (
            DomainError::PluginUnavailable {
                gts_id: "gts.x".to_owned(),
                reason: "gone".to_owned(),
            },
            CheckOutcome::Unavailable,
        ),
        (DomainError::Unauthorized, CheckOutcome::Unauthorized),
        (
            DomainError::InvalidPluginInstance {
                gts_id: "gts.x".to_owned(),
                reason: "bad".to_owned(),
            },
            CheckOutcome::Error,
        ),
        (
            DomainError::ContractUnusable {
                type_id: "gts.x~".to_owned(),
                reason: "bad".to_owned(),
            },
            CheckOutcome::Error,
        ),
        (
            DomainError::Internal("boom".to_owned()),
            CheckOutcome::Error,
        ),
    ];
    for (err, expected) in cases {
        assert_eq!(CheckOutcome::from(&err), expected, "for {err:?}");
    }
}
