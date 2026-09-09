//! Stored failure compatibility and bounded metric labels.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use super::{ItemFailure, reason_label};
use crate::domain::admission::AdmissionFailureReason;

#[test]
fn known_reasons_keep_their_wire_codes_and_metric_labels_after_storage() {
    for (reason, code) in [
        (
            AdmissionFailureReason::ActivationWriteSetExceeded,
            "activation_write_set_exceeded",
        ),
        (AdmissionFailureReason::AlreadyExists, "already_exists"),
        (
            AdmissionFailureReason::DependentInvalid,
            "dependent_invalid",
        ),
        (AdmissionFailureReason::EntityDeleted, "entity_deleted"),
        (
            AdmissionFailureReason::FamilyKindConflict,
            "family_kind_conflict",
        ),
        (
            AdmissionFailureReason::FamilyShapeConflict,
            "family_shape_conflict",
        ),
        (AdmissionFailureReason::InvalidDocument, "invalid_document"),
        (
            AdmissionFailureReason::InvalidIdentifier,
            "invalid_identifier",
        ),
        (AdmissionFailureReason::InvalidSchema, "invalid_schema"),
        (AdmissionFailureReason::InvalidValue, "invalid_value"),
        (
            AdmissionFailureReason::MissingPredecessor,
            "missing_predecessor",
        ),
        (
            AdmissionFailureReason::PreconditionFailed,
            "precondition_failed",
        ),
        (
            AdmissionFailureReason::ResolutionClosureExceeded,
            "resolution_closure_exceeded",
        ),
        (
            AdmissionFailureReason::ResolvedDocumentTooLarge,
            "resolved_document_too_large",
        ),
        (
            AdmissionFailureReason::RevalidationExhausted,
            "revalidation_exhausted",
        ),
        (
            AdmissionFailureReason::UnparsablePayload,
            "unparsable_payload",
        ),
        (
            AdmissionFailureReason::UnreadableVersion,
            "unreadable_version",
        ),
        (
            AdmissionFailureReason::UnrecognizedPayload,
            "unrecognized_payload",
        ),
    ] {
        let failure = ItemFailure::new(reason.clone(), "failure details".to_owned());
        let payload = failure.to_payload();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&payload).expect("JSON"),
            serde_json::json!({"reason": code, "message": "failure details"}),
        );
        let restored = ItemFailure::from_payload(&payload);
        assert_eq!(restored, failure);
        assert_eq!(reason_label(&reason), code);
        assert_eq!(reason_label(&restored.reason), code);
    }
}

#[test]
fn an_unknown_stored_reason_is_preserved_but_counts_under_other() {
    let payload = r#"{"reason":"future_refusal","message":"future details"}"#;
    let failure = ItemFailure::from_payload(payload);
    assert_eq!(
        failure.reason,
        AdmissionFailureReason::Unknown("future_refusal".to_owned()),
    );
    assert_eq!(reason_label(&failure.reason), "other");
    assert_eq!(failure.reason.as_str(), "future_refusal");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&failure.to_payload()).expect("JSON"),
        serde_json::from_str::<serde_json::Value>(payload).expect("JSON"),
    );
}

#[test]
fn malformed_payloads_keep_the_original_content_and_diagnostic_reason() {
    for (payload, reason) in [
        ("not JSON", AdmissionFailureReason::UnparsablePayload),
        (
            r#"{"reason":42,"message":"details"}"#,
            AdmissionFailureReason::UnrecognizedPayload,
        ),
        (
            r#"{"reason":"invalid_schema"}"#,
            AdmissionFailureReason::UnrecognizedPayload,
        ),
    ] {
        let failure = ItemFailure::from_payload(payload);
        assert_eq!(failure.reason, reason);
        assert_eq!(failure.message, payload);
        assert_eq!(ItemFailure::from_payload(&failure.to_payload()), failure);
    }
}
