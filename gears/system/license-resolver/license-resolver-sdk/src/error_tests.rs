//! Unit tests for `LicenseResolverError` and its canonical mapping.

use toolkit_canonical_errors::{CanonicalError, Problem};

use super::{FieldViolation, LicenseResolverError};
use crate::field;

#[test]
fn invalid_request_carries_canonical_field_violations() {
    let err = LicenseResolverError::InvalidRequest {
        violations: vec![
            FieldViolation::new(
                format!("{}/metadata/model_name", field::RESOURCE_FIELD),
                format!(
                    "{}: \"gpt4o\" is not of type \"integer\"",
                    toolkit_gts::gts_id!("cf.core.lic.res.v1~cf.genai.llm_gateway.model_usage.v1~")
                ),
                field::SCHEMA_MISMATCH,
            ),
            FieldViolation::new(
                field::SUBJECT_TYPE_FIELD,
                "gts.cf.core.lic.subj.v1~acme.user.v1~ is not registered",
                field::CONTRACT_NOT_REGISTERED,
            ),
        ],
    };

    // Display must not leak the violation contents.
    assert_eq!(err.to_string(), "invalid request: 2 violation(s)");

    let LicenseResolverError::InvalidRequest { violations } = &err else {
        panic!("expected InvalidRequest");
    };
    let json = serde_json::to_value(&violations[0]).unwrap();
    assert_eq!(
        json.get("reason").and_then(|v| v.as_str()),
        Some(field::SCHEMA_MISMATCH)
    );
    assert_eq!(
        json.get("field").and_then(|v| v.as_str()),
        Some("resource/metadata/model_name"),
        "`field` must be a role-rooted JSON pointer into the request"
    );
}

/// Pins every `reason` constant to the `Problem` JSON path a consumer reads it
/// from, so the vocabulary and the wire cannot drift.
#[test]
fn every_reason_constant_reaches_the_problem_wire() {
    let reasons = vec![
        field::CONTRACT_NOT_REGISTERED,
        field::CONTRACT_TYPE_MALFORMED,
        field::CONTRACT_NOT_DERIVED,
        field::CONTRACT_ABSTRACT,
        field::SCHEMA_MISMATCH,
        field::SUBJECT_NOT_ADMITTED,
    ];
    for reason in reasons {
        let err = LicenseResolverError::InvalidRequest {
            violations: vec![FieldViolation::new(
                field::SUBJECT_TYPE_FIELD,
                "rejected",
                reason,
            )],
        };
        let problem = Problem::from_error(&err.into()).expect("problem renders");
        let wire = serde_json::to_value(&problem).unwrap();
        assert_eq!(
            wire.pointer("/context/field_violations/0/reason")
                .and_then(|v| v.as_str()),
            Some(reason),
            "reason {reason} must land at context.field_violations[0].reason: {wire:#}"
        );
        assert_eq!(
            wire.pointer("/context/field_violations/0/field")
                .and_then(|v| v.as_str()),
            Some(field::SUBJECT_TYPE_FIELD)
        );
        assert_eq!(
            field::ValidationReason::from_wire(reason).as_wire(),
            reason,
            "typed view must round-trip the wire code"
        );
    }
}

#[test]
fn service_unavailable_reports_reason() {
    let err = LicenseResolverError::ServiceUnavailable("backend timeout".to_owned());
    assert_eq!(err.to_string(), "service unavailable: backend timeout");
}

#[test]
fn unauthorized_maps_to_permission_denied() {
    let canonical: CanonicalError = LicenseResolverError::Unauthorized.into();
    assert_eq!(canonical.status_code(), 403);
    assert!(
        canonical.gts_type().contains("permission_denied"),
        "unexpected gts type: {}",
        canonical.gts_type()
    );
}

#[test]
fn invalid_request_maps_to_invalid_argument_400() {
    let err = LicenseResolverError::InvalidRequest {
        violations: vec![
            FieldViolation::new(
                "subject/type",
                "contract type is required",
                "MISSING_DOMAIN_TYPE",
            ),
            FieldViolation::new(
                "resource/type",
                "contract type is required",
                "MISSING_DOMAIN_TYPE",
            ),
        ],
    };
    let canonical: CanonicalError = err.into();
    assert_eq!(canonical.status_code(), 400);
    assert!(canonical.gts_type().contains("invalid_argument"));

    let problem = Problem::from_error(&canonical).expect("problem renders");
    assert_eq!(problem.status, Some(400));
    let wire = serde_json::to_value(&problem).unwrap();
    assert_eq!(
        wire.pointer("/context/field_violations/1/field")
            .and_then(|v| v.as_str()),
        Some("resource/type"),
        "violations after the first must be folded onto the builder: {wire:#}"
    );
}

#[test]
fn empty_invalid_request_still_maps_to_400() {
    let canonical: CanonicalError =
        LicenseResolverError::InvalidRequest { violations: vec![] }.into();
    assert_eq!(canonical.status_code(), 400);
}

#[test]
fn service_unavailable_maps_to_503() {
    let diagnostic = "connection failed: postgres://secret@license-db.internal";
    let canonical: CanonicalError =
        LicenseResolverError::ServiceUnavailable(diagnostic.to_owned()).into();
    assert_eq!(canonical.status_code(), 503);

    let problem = Problem::from_error(&canonical).expect("problem renders");
    assert_eq!(problem.detail, "License service temporarily unavailable");
    assert!(
        !problem.detail.contains(diagnostic),
        "internal diagnostic must not be exposed in the public problem detail"
    );
}

#[test]
fn no_plugin_and_internal_map_to_500() {
    let no_plugin: CanonicalError = LicenseResolverError::NoPluginAvailable.into();
    assert_eq!(no_plugin.status_code(), 500);

    let internal: CanonicalError = LicenseResolverError::Internal("boom".to_owned()).into();
    assert_eq!(internal.status_code(), 500);
}
