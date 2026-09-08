#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
use super::*;
use crate::test_support::EvaluationRequestBuilder;

/// Validation only consults the trusted set to spare a configured system
/// actor the subject-type check, so most cases here pass the fixture set and
/// a couple pin the empty-set behaviour explicitly.
fn trusted() -> crate::domain::subject_type::TrustedSystemActors {
    crate::test_support::trusted_actors::trusted_actors()
}

/// Assert both the rejection VARIANT and the message it renders.
///
/// The variant is what carries the `invalid_request` classification, so
/// pinning it here is what keeps a future edit from turning a client fault
/// into a fail-closed system fault; the message is a separate wire contract.
fn assert_rejected(result: Result<(), PluginError>, expected: &PluginError, expected_msg: &str) {
    match result {
        Err(err) => {
            assert_eq!(&err, expected);
            assert_eq!(err.to_string(), expected_msg);
        }
        Ok(()) => panic!("expected {expected:?}, got Ok(())"),
    }
}

#[test]
fn user_subject_with_valid_fields_passes() {
    let request = EvaluationRequestBuilder::default().build();
    assert!(validate(&request, &trusted()).is_ok());
}

#[test]
fn service_principal_subject_passes() {
    let request = EvaluationRequestBuilder::default()
        .with_subject_type(Some(
            "gts.cf.core.security.subject_service_principal.v1~".to_owned(),
        ))
        .build();
    assert!(validate(&request, &trusted()).is_ok());
}

#[test]
fn absent_subject_type_passes_validation() {
    // Absent subject_type is valid — it defaults to User in map_subject_type
    // (mirrors RBAC). Only a present-but-unrecognized value is rejected.
    let request = EvaluationRequestBuilder::default()
        .with_subject_type(None)
        .build();
    assert!(validate(&request, &trusted()).is_ok());
}

#[test]
fn u_09_unknown_subject_type() {
    let request = EvaluationRequestBuilder::default()
        .with_subject_type(Some("gts.cf.core.security.subject_group.v1~".to_owned()))
        .build();
    assert_rejected(
        validate(&request, &trusted()),
        &PluginError::UnknownSubjectType {
            value: "gts.cf.core.security.subject_group.v1~".to_owned(),
        },
        "unknown subject type: gts.cf.core.security.subject_group.v1~",
    );
}

#[test]
fn u_10_empty_action_name() {
    let request = EvaluationRequestBuilder::default()
        .with_action_name("")
        .build();
    assert_rejected(
        validate(&request, &trusted()),
        &PluginError::InvalidOperationEmpty,
        "invalid operation: empty",
    );
}

#[test]
fn u_11_action_name_asterisk_wildcard() {
    let request = EvaluationRequestBuilder::default()
        .with_action_name("read*")
        .build();
    assert_rejected(
        validate(&request, &trusted()),
        &PluginError::InvalidOperationWildcard,
        "invalid operation: wildcards not allowed",
    );
}

#[test]
fn u_11_action_name_question_wildcard() {
    let request = EvaluationRequestBuilder::default()
        .with_action_name("rea?")
        .build();
    assert_rejected(
        validate(&request, &trusted()),
        &PluginError::InvalidOperationWildcard,
        "invalid operation: wildcards not allowed",
    );
}

#[test]
fn missing_resource_type_is_empty_string() {
    let request = EvaluationRequestBuilder::default()
        .with_resource_type("")
        .build();
    assert_rejected(
        validate(&request, &trusted()),
        &PluginError::MissingResourceType,
        "missing resource type",
    );
}

#[test]
fn short_circuits_on_first_failure() {
    // Both subject and action are invalid — must surface the subject error
    // because validation runs in the documented order. (Absent subject_type is
    // now valid, so use a present-but-unrecognized value to fail the subject step.)
    let request = EvaluationRequestBuilder::default()
        .with_subject_type(Some("bogus-type".to_owned()))
        .with_action_name("read*")
        .build();
    assert_rejected(
        validate(&request, &trusted()),
        &PluginError::UnknownSubjectType {
            value: "bogus-type".to_owned(),
        },
        "unknown subject type: bogus-type",
    );
}
