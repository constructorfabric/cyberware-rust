//! Unit tests for the licensing contract validation pipeline.

use gts::GtsTypeId;
use serde_json::json;

use super::*;
use crate::domain::test_support::{
    BARE_RESOURCE, FailureKind, FakeContractRegistry, MODEL_USAGE_RESOURCE, RESOURCE_BASE,
    SUBJECT_BASE, TENANT_SUBJECT, TestModelUsageResourceV1, USER_SUBJECT, bare_resource_schema,
    conforming_request, model_usage_schema, request, resource, resource_base_schema, subject,
    subject_base_schema, tenant_subject_schema, user_subject_schema,
};

fn validator(registry: FakeContractRegistry) -> ContractValidator {
    ContractValidator::new(Arc::new(registry))
}

fn conforming_validator() -> ContractValidator {
    validator(FakeContractRegistry::with_test_contracts())
}

async fn violations_of(
    validator: &ContractValidator,
    req: &LicenseCheckRequest,
) -> Vec<FieldViolation> {
    match validator.validate(req).await {
        Err(DomainError::ContractViolation { violations }) => violations,
        other => panic!("expected ContractViolation, got: {other:?}"),
    }
}

fn reasons(violations: &[FieldViolation]) -> Vec<&str> {
    violations.iter().map(|v| v.reason.as_str()).collect()
}

fn find<'a>(violations: &'a [FieldViolation], reason: &str) -> &'a FieldViolation {
    violations
        .iter()
        .find(|v| v.reason == reason)
        .unwrap_or_else(|| panic!("no {reason} violation in {violations:?}"))
}

#[tokio::test]
async fn conforming_request_passes() {
    let result = conforming_validator().validate(&conforming_request()).await;
    assert!(result.is_ok(), "expected Ok, got: {result:?}");
}

/// An id-less contract object targets the whole type, and must validate exactly
/// like one naming an instance.
#[tokio::test]
async fn both_identity_forms_pass() {
    let req = request(
        subject(USER_SUBJECT, None, json!({ "category": "internal" })),
        resource(
            MODEL_USAGE_RESOURCE,
            None,
            json!({ "model_vendor": "openai", "model_name": "gpt-4o" }),
        ),
    );
    let result = conforming_validator().validate(&req).await;
    assert!(result.is_ok(), "id-less check must validate: {result:?}");
}

#[tokio::test]
async fn each_contract_is_resolved_once_per_check() {
    let registry = Arc::new(FakeContractRegistry::with_test_contracts());
    let validator = ContractValidator::new(registry.clone());
    validator
        .validate(&conforming_request())
        .await
        .expect("conforming request");
    assert_eq!(registry.calls(), 2, "one lookup per contract object");
}

#[tokio::test]
async fn metadata_type_mismatch_is_reported_at_the_offending_property() {
    let req = request(
        subject(USER_SUBJECT, Some("acme-admin"), json!({ "category": 42 })),
        resource(
            MODEL_USAGE_RESOURCE,
            Some("gpt-4o"),
            json!({ "model_vendor": "openai", "model_name": "gpt-4o" }),
        ),
    );
    let violations = violations_of(&conforming_validator(), &req).await;
    let violation = find(&violations, field::SCHEMA_MISMATCH);
    assert_eq!(violation.field, "subject/metadata/category");
    assert!(
        violation.description.contains(USER_SUBJECT),
        "description must name the contract judged against: {violation:?}"
    );
    assert!(
        violation.description.contains("string"),
        "description must carry the schema complaint: {violation:?}"
    );
}

#[tokio::test]
async fn missing_required_metadata_property_is_reported() {
    let req = request(
        subject(USER_SUBJECT, None, json!({ "category": "internal" })),
        resource(
            MODEL_USAGE_RESOURCE,
            None,
            json!({ "model_vendor": "openai" }),
        ),
    );
    let violations = violations_of(&conforming_validator(), &req).await;
    let violation = find(&violations, field::SCHEMA_MISMATCH);
    // A `required` failure points at the containing object; the missing
    // property is named in the description.
    assert_eq!(violation.field, "resource/metadata");
    assert!(
        violation.description.contains("model_name"),
        "description must name the missing property: {violation:?}"
    );
}

/// The base envelope reaches validation through the derived contract's `allOf`
/// reference — without that, bounds the base declares (here a non-empty instance
/// id) would go unchecked.
#[tokio::test]
async fn base_envelope_bounds_are_enforced_through_the_derived_contract() {
    let req = request(
        subject(USER_SUBJECT, None, json!({ "category": "internal" })),
        resource(
            MODEL_USAGE_RESOURCE,
            Some(""),
            json!({ "model_vendor": "openai", "model_name": "gpt-4o" }),
        ),
    );
    let violations = violations_of(&conforming_validator(), &req).await;
    let violation = find(&violations, field::SCHEMA_MISMATCH);
    assert_eq!(violation.field, "resource/id");
}

#[tokio::test]
async fn unregistered_contract_is_reported() {
    let validator = validator(FakeContractRegistry::new().with_contracts([model_usage_schema()]));
    let violations = violations_of(&validator, &conforming_request()).await;
    let violation = find(&violations, field::CONTRACT_NOT_REGISTERED);
    assert_eq!(violation.field, field::SUBJECT_TYPE_FIELD);
    assert!(violation.description.contains(USER_SUBJECT));
}

/// An abstract base is a shape every contract shares, not something a check can
/// instantiate.
#[tokio::test]
async fn abstract_licensing_base_cannot_be_checked() {
    let validator = validator(
        FakeContractRegistry::new().with_contracts([subject_base_schema(), resource_base_schema()]),
    );
    let req = request(
        subject(SUBJECT_BASE, None, json!({})),
        resource(RESOURCE_BASE, None, json!({})),
    );
    let violations = violations_of(&validator, &req).await;
    assert_eq!(
        reasons(&violations),
        vec![field::CONTRACT_ABSTRACT, field::CONTRACT_ABSTRACT],
        "both slots must be rejected as abstract: {violations:?}"
    );
}

/// A Resource contract in the subject slot resolves fine but descends from the
/// wrong licensing base.
#[tokio::test]
async fn contract_from_the_wrong_licensing_base_is_reported() {
    let validator = conforming_validator();
    let req = request(
        subject(
            MODEL_USAGE_RESOURCE,
            None,
            json!({ "model_vendor": "openai", "model_name": "gpt-4o" }),
        ),
        resource(
            MODEL_USAGE_RESOURCE,
            None,
            json!({ "model_vendor": "openai", "model_name": "gpt-4o" }),
        ),
    );
    let violations = violations_of(&validator, &req).await;
    let violation = find(&violations, field::CONTRACT_NOT_DERIVED);
    assert_eq!(violation.field, field::SUBJECT_TYPE_FIELD);
    assert!(
        violation.description.contains(SUBJECT_BASE),
        "description must name the base that was expected: {violation:?}"
    );
}

#[tokio::test]
async fn subject_outside_admitted_subjects_is_a_violation_not_a_denial() {
    let validator = validator(
        FakeContractRegistry::new().with_contracts([tenant_subject_schema(), model_usage_schema()]),
    );
    let req = request(
        subject(TENANT_SUBJECT, None, json!({ "tier": "gold" })),
        resource(
            MODEL_USAGE_RESOURCE,
            Some("gpt-4o"),
            json!({ "model_vendor": "openai", "model_name": "gpt-4o" }),
        ),
    );
    let violations = violations_of(&validator, &req).await;
    let violation = find(&violations, field::SUBJECT_NOT_ADMITTED);
    assert_eq!(violation.field, field::SUBJECT_TYPE_FIELD);
    assert!(
        violation.description.contains(USER_SUBJECT),
        "description must list what the contract does admit: {violation:?}"
    );
}

/// A contract that declares no `admitted_subjects` inherits the abstract base's
/// empty list, which admits nobody — "not configured yet" is not a thing.
#[tokio::test]
async fn contract_without_admitted_subjects_rejects_every_check() {
    let validator = validator(
        FakeContractRegistry::new().with_contracts([user_subject_schema(), bare_resource_schema()]),
    );
    let req = request(
        subject(USER_SUBJECT, None, json!({ "category": "internal" })),
        resource(
            BARE_RESOURCE,
            Some("gpt-4o"),
            json!({ "model_name": "gpt-4o" }),
        ),
    );
    let violations = violations_of(&validator, &req).await;
    assert_eq!(reasons(&violations), vec![field::SUBJECT_NOT_ADMITTED]);
}

/// Admissibility describes a pair, so it stays silent while either half is
/// unknown.
#[tokio::test]
async fn admissibility_is_silent_when_the_subject_contract_is_unknown() {
    let validator = validator(FakeContractRegistry::new().with_contracts([model_usage_schema()]));
    let violations = violations_of(&validator, &conforming_request()).await;
    assert_eq!(reasons(&violations), vec![field::CONTRACT_NOT_REGISTERED]);
}

/// A trait of the wrong shape is catalog drift an operator must fix, so it is
/// never reported as the caller's fault.
#[tokio::test]
async fn malformed_admitted_subjects_trait_is_catalog_drift() {
    let mut body = <TestModelUsageResourceV1 as GtsSchema>::gts_schema_with_refs();
    body["x-gts-traits"]["admitted_subjects"] = json!("not-an-array");
    let drifted = GtsTypeSchema::try_new(
        GtsTypeId::new(MODEL_USAGE_RESOURCE),
        body,
        None,
        Some(Arc::new(resource_base_schema())),
    )
    .expect("drifted contract is still a well-formed type-schema");

    let validator =
        validator(FakeContractRegistry::new().with_contracts([user_subject_schema(), drifted]));
    let err = validator
        .validate(&conforming_request())
        .await
        .expect_err("a drifted trait must not pass");
    assert!(
        matches!(err, DomainError::ContractUnusable { .. }),
        "expected ContractUnusable, got: {err:?}"
    );
}

/// Drift guard on the trait key this gear reads out of the published base.
#[test]
fn admitted_subjects_trait_matches_the_published_base() {
    let base = <LicenseResourceV1<()> as GtsSchema>::gts_schema_with_refs();
    assert!(
        base.pointer(&format!(
            "/x-gts-traits-schema/properties/{ADMITTED_SUBJECTS_TRAIT}"
        ))
        .is_some(),
        "the Resource base must declare the trait this gear reads:\n{base:#}"
    );
}

/// An unreachable catalog is cannot-determine, not a bad request: reporting it as
/// a violation would blame the caller for an outage.
#[tokio::test]
async fn registry_unavailable_is_not_a_violation() {
    let validator = validator(FakeContractRegistry::failing(FailureKind::Unavailable));
    let err = validator
        .validate(&conforming_request())
        .await
        .expect_err("an unreachable registry must fail the check");
    assert!(
        matches!(err, DomainError::TypesRegistryUnavailable(_)),
        "expected TypesRegistryUnavailable, got: {err:?}"
    );
}

#[tokio::test]
async fn malformed_contract_type_is_reported_as_a_violation() {
    let validator = validator(FakeContractRegistry::failing(FailureKind::Malformed));
    let violations = violations_of(&validator, &conforming_request()).await;
    let violation = find(&violations, field::CONTRACT_TYPE_MALFORMED);
    assert_eq!(violation.field, field::RESOURCE_TYPE_FIELD);
}

/// Violations are accumulated, not reported one per round-trip: a caller fixing
/// its request assembly sees every problem at once.
#[tokio::test]
async fn all_violations_are_collected() {
    let validator = validator(FakeContractRegistry::new().with_contracts([model_usage_schema()]));
    let req = request(
        subject(USER_SUBJECT, None, json!({ "category": "internal" })),
        resource(MODEL_USAGE_RESOURCE, None, json!({ "model_vendor": 7 })),
    );
    let violations = violations_of(&validator, &req).await;
    let found = reasons(&violations);
    assert!(
        found.contains(&field::CONTRACT_NOT_REGISTERED),
        "missing the unregistered-subject violation: {found:?}"
    );
    assert!(
        found
            .iter()
            .filter(|r| **r == field::SCHEMA_MISMATCH)
            .count()
            >= 2,
        "expected both the bad type and the missing property: {violations:?}"
    );
}

/// Without the cap the error payload would grow with the caller's own input:
/// one violation per missing required property, and the caller names the
/// contract.
#[tokio::test]
async fn violations_are_capped_per_contract_object() {
    let over_cap = MAX_REPORTED_VIOLATIONS + 5;
    let properties: serde_json::Map<String, Value> = (0..over_cap)
        .map(|i| (format!("p{i}"), json!({ "type": "string" })))
        .collect();

    let mut body = <TestModelUsageResourceV1 as GtsSchema>::gts_schema_with_refs();
    let metadata = &mut body["allOf"][1]["properties"]["metadata"];
    metadata["required"] = json!(properties.keys().collect::<Vec<_>>());
    metadata["properties"] = Value::Object(properties);
    let wide = GtsTypeSchema::try_new(
        GtsTypeId::new(MODEL_USAGE_RESOURCE),
        body,
        None,
        Some(Arc::new(resource_base_schema())),
    )
    .expect("a contract with many required properties is well formed");

    let validator =
        validator(FakeContractRegistry::new().with_contracts([user_subject_schema(), wide]));
    let req = request(
        subject(USER_SUBJECT, None, json!({ "category": "internal" })),
        resource(MODEL_USAGE_RESOURCE, None, json!({})),
    );

    let violations = violations_of(&validator, &req).await;
    assert_eq!(
        violations.len(),
        MAX_REPORTED_VIOLATIONS,
        "{over_cap} missing properties must report as {MAX_REPORTED_VIOLATIONS} violations"
    );
}

#[tokio::test]
async fn uncompilable_contract_schema_is_catalog_drift() {
    let mut body = <TestModelUsageResourceV1 as GtsSchema>::gts_schema_with_refs();
    body["pattern"] = json!("[");
    let drifted = GtsTypeSchema::try_new(
        GtsTypeId::new(MODEL_USAGE_RESOURCE),
        body,
        None,
        Some(Arc::new(resource_base_schema())),
    )
    .expect("an uncompilable schema is still a well-formed type-schema");

    let validator =
        validator(FakeContractRegistry::new().with_contracts([user_subject_schema(), drifted]));
    let err = validator
        .validate(&conforming_request())
        .await
        .expect_err("an uncompilable schema must not pass");
    assert!(
        matches!(err, DomainError::ContractUnusable { ref reason, .. } if reason.contains("not a valid JSON Schema")),
        "expected ContractUnusable naming the compile failure, got: {err:?}"
    );
}

#[tokio::test]
async fn null_admitted_subjects_admits_nobody() {
    let mut body = <TestModelUsageResourceV1 as GtsSchema>::gts_schema_with_refs();
    body["x-gts-traits"]["admitted_subjects"] = json!(null);
    let drifted = GtsTypeSchema::try_new(
        GtsTypeId::new(MODEL_USAGE_RESOURCE),
        body,
        None,
        Some(Arc::new(resource_base_schema())),
    )
    .expect("a null trait is still a well-formed type-schema");

    let validator =
        validator(FakeContractRegistry::new().with_contracts([user_subject_schema(), drifted]));
    let violations = violations_of(&validator, &conforming_request()).await;
    assert_eq!(reasons(&violations), vec![field::SUBJECT_NOT_ADMITTED]);
}

#[tokio::test]
async fn non_string_admitted_subjects_entry_is_catalog_drift() {
    let mut body = <TestModelUsageResourceV1 as GtsSchema>::gts_schema_with_refs();
    body["x-gts-traits"]["admitted_subjects"] = json!([USER_SUBJECT, 42]);
    let drifted = GtsTypeSchema::try_new(
        GtsTypeId::new(MODEL_USAGE_RESOURCE),
        body,
        None,
        Some(Arc::new(resource_base_schema())),
    )
    .expect("a drifted trait is still a well-formed type-schema");

    let validator =
        validator(FakeContractRegistry::new().with_contracts([user_subject_schema(), drifted]));
    let err = validator
        .validate(&conforming_request())
        .await
        .expect_err("a non-string entry must not pass");
    assert!(
        matches!(err, DomainError::ContractUnusable { ref reason, .. } if reason.contains("non-string")),
        "expected ContractUnusable naming the non-string entry, got: {err:?}"
    );
}
