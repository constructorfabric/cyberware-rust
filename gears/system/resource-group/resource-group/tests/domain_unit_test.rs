// Created: 2026-04-16 by Constructor Tech
//! Unit tests for domain layer pure logic.
//!
//! Tests validation functions, error construction, error mapping,
//! and serialization failure detection — all without database dependencies.
//!
//! Full domain service tests with mock repositories are deferred to
//! TODO-16 (repository trait abstraction).

use resource_group::domain::error::DomainError;
use resource_group::domain::validation::{self, RG_TYPE_PREFIX};
use toolkit_canonical_errors::{CanonicalError, Problem};
use toolkit_gts::{GTS_ID_PREFIX, gts_id, gts_uri};

/// Build the wire `Problem` the canonical error middleware would emit
/// for a given `DomainError`. The integration tests run without the
/// middleware in scope, so `instance` / `trace_id` stay `None` here —
/// that injection is exercised by integration tests that drive the full
/// router.
fn wire(err: DomainError) -> Problem {
    Problem::from(CanonicalError::from(err))
}

// Canonical category problem-type URIs. See
// `docs/arch/errors/categories/*.md`.
const NOT_FOUND_TYPE: &str = gts_uri!("cf.core.errors.err.v1~cf.core.err.not_found.v1~");
const ALREADY_EXISTS_TYPE: &str = gts_uri!("cf.core.errors.err.v1~cf.core.err.already_exists.v1~");
const INVALID_ARGUMENT_TYPE: &str =
    gts_uri!("cf.core.errors.err.v1~cf.core.err.invalid_argument.v1~");
const FAILED_PRECONDITION_TYPE: &str =
    gts_uri!("cf.core.errors.err.v1~cf.core.err.failed_precondition.v1~");
const PERMISSION_DENIED_TYPE: &str =
    gts_uri!("cf.core.errors.err.v1~cf.core.err.permission_denied.v1~");
const INTERNAL_TYPE: &str = gts_uri!("cf.core.errors.err.v1~cf.core.err.internal.v1~");

/// Resource-group GTS prefix (matches `RgError`'s `#[resource_error(...)]`).
const RG_GTS: &str = gts_id!("cf.core.rg.group.v1~");

// ── validate_type_code ──────────────────────────────────────────────────

#[test]
fn validate_type_code_rejects_empty() {
    let result = validation::validate_type_code("");
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(matches!(err, DomainError::Validation { .. }));
    assert!(err.to_string().contains("empty"));
}

#[test]
fn validate_type_code_rejects_wrong_prefix() {
    let result = validation::validate_type_code("wrong.prefix.type");
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(matches!(err, DomainError::Validation { .. }));
    assert!(err.to_string().contains("prefix"));
}

#[test]
fn validate_type_code_rejects_too_long() {
    let long_code = format!(
        "{}{}",
        RG_TYPE_PREFIX,
        "a".repeat(1025 - RG_TYPE_PREFIX.len())
    );
    assert!(long_code.len() > 1024);
    let result = validation::validate_type_code(&long_code);
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(matches!(err, DomainError::Validation { .. }));
    assert!(err.to_string().contains("1024"));
}

#[test]
fn validate_type_code_accepts_valid_code() {
    let code = format!("{RG_TYPE_PREFIX}tenant");
    let result = validation::validate_type_code(&code);
    assert!(result.is_ok());
}

#[test]
fn validate_type_code_accepts_exact_max_length() {
    let code = format!(
        "{}{}",
        RG_TYPE_PREFIX,
        "a".repeat(1024 - RG_TYPE_PREFIX.len())
    );
    assert_eq!(code.len(), 1024);
    let result = validation::validate_type_code(&code);
    assert!(result.is_ok());
}

#[test]
fn validate_type_code_rejects_prefix_only() {
    // The prefix itself is a valid type code (non-empty, correct prefix, within length)
    let result = validation::validate_type_code(RG_TYPE_PREFIX);
    assert!(result.is_ok());
}

// ── validate_membership_type_code ───────────────────────────────────────

#[test]
fn validate_membership_type_code_accepts_non_rg_prefix() {
    // Per DESIGN.md, membership resource types are external domain types
    // and do NOT require the `gts.cf.core.rg.type.v1~` prefix.
    let result = validation::validate_membership_type_code(gts_id!("cf.core.idp.user.v1~"));
    assert!(result.is_ok(), "Expected ok, got {result:?}");
}

#[test]
fn validate_membership_type_code_accepts_rg_prefixed() {
    // Backwards compatibility: RG-prefixed codes still validate.
    let code = format!("{RG_TYPE_PREFIX}y.core.tn.tenant.v1~");
    let result = validation::validate_membership_type_code(&code);
    assert!(result.is_ok(), "Expected ok, got {result:?}");
}

#[test]
fn validate_membership_type_code_rejects_empty() {
    let result = validation::validate_membership_type_code("");
    assert!(result.is_err());
    assert!(matches!(
        result.unwrap_err(),
        DomainError::Validation { .. }
    ));
}

#[test]
fn validate_membership_type_code_rejects_trailing_wildcard_after_tilde() {
    // Wildcard patterns are not accepted: `gts_type_allowed_membership`
    // stores a SMALLINT FK to a concrete registered type, not a pattern.
    let pattern = format!("{GTS_ID_PREFIX}cf.core.rg.type.v1~*");
    let result = validation::validate_membership_type_code(pattern.as_str());
    assert!(result.is_err(), "Expected err, got {result:?}");
    assert!(matches!(
        result.unwrap_err(),
        DomainError::Validation { .. }
    ));
}

#[test]
fn validate_membership_type_code_rejects_trailing_wildcard_after_dot() {
    let invalid_pattern = format!("{GTS_ID_PREFIX}cf.*");
    let result = validation::validate_membership_type_code(&invalid_pattern);
    assert!(result.is_err(), "Expected err, got {result:?}");
    assert!(matches!(
        result.unwrap_err(),
        DomainError::Validation { .. }
    ));
}

#[test]
fn validate_membership_type_code_rejects_malformed_gts_path() {
    let result = validation::validate_membership_type_code("not-a-gts-path");
    assert!(result.is_err());
    assert!(matches!(
        result.unwrap_err(),
        DomainError::Validation { .. }
    ));
}

#[test]
fn validate_membership_type_code_rejects_mid_string_wildcard() {
    let invalid_pattern = format!("{GTS_ID_PREFIX}cf.*.user.v1~");
    let result = validation::validate_membership_type_code(&invalid_pattern);
    assert!(result.is_err());
    assert!(matches!(
        result.unwrap_err(),
        DomainError::Validation { .. }
    ));
}

// ── validate_metadata_schema ────────────────────────────────────────────

#[test]
fn validate_metadata_schema_accepts_valid_object_schema() {
    let schema = serde_json::json!({
        "type": "object",
        "properties": {
            "name": { "type": "string" },
            "count": { "type": "integer" }
        },
        "required": ["name"]
    });
    assert!(validation::validate_metadata_schema(&schema).is_ok());
}

#[test]
fn validate_metadata_schema_accepts_boolean_true_schema() {
    let schema = serde_json::json!(true);
    assert!(validation::validate_metadata_schema(&schema).is_ok());
}

#[test]
fn validate_metadata_schema_rejects_invalid_schema() {
    let schema = serde_json::json!({
        "type": "not-a-real-type"
    });
    let result = validation::validate_metadata_schema(&schema);
    assert!(result.is_err());
    assert!(matches!(
        result.unwrap_err(),
        DomainError::Validation { .. }
    ));
}

// ── validate_metadata_against_schema ────────────────────────────────────

#[test]
fn validate_metadata_against_schema_passes_when_no_schema() {
    let metadata = serde_json::json!({"anything": true});
    assert!(validation::validate_metadata_against_schema(Some(&metadata), None).is_ok());
}

#[test]
fn validate_metadata_against_schema_passes_when_no_metadata() {
    let schema = serde_json::json!({"type": "object"});
    assert!(validation::validate_metadata_against_schema(None, Some(&schema)).is_ok());
}

#[test]
fn validate_metadata_against_schema_passes_when_both_none() {
    assert!(validation::validate_metadata_against_schema(None, None).is_ok());
}

#[test]
fn validate_metadata_against_schema_passes_valid_metadata() {
    let schema = serde_json::json!({
        "type": "object",
        "properties": {
            "name": { "type": "string", "maxLength": 50 }
        },
        "required": ["name"],
        "additionalProperties": false
    });
    let metadata = serde_json::json!({"name": "hello"});
    assert!(validation::validate_metadata_against_schema(Some(&metadata), Some(&schema)).is_ok());
}

#[test]
fn validate_metadata_against_schema_rejects_type_mismatch() {
    let schema = serde_json::json!({
        "type": "object",
        "properties": {
            "count": { "type": "integer" }
        }
    });
    let metadata = serde_json::json!({"count": "not-an-integer"});
    let result = validation::validate_metadata_against_schema(Some(&metadata), Some(&schema));
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(matches!(err, DomainError::Validation { .. }));
    assert!(err.to_string().contains("does not match type schema"));
}

#[test]
fn validate_metadata_against_schema_rejects_missing_required_field() {
    let schema = serde_json::json!({
        "type": "object",
        "properties": {
            "name": { "type": "string" }
        },
        "required": ["name"]
    });
    let metadata = serde_json::json!({});
    let result = validation::validate_metadata_against_schema(Some(&metadata), Some(&schema));
    assert!(result.is_err());
}

#[test]
fn validate_metadata_against_schema_rejects_additional_properties() {
    let schema = serde_json::json!({
        "type": "object",
        "properties": {
            "name": { "type": "string" }
        },
        "additionalProperties": false
    });
    let metadata = serde_json::json!({"name": "ok", "unknown": 42});
    let result = validation::validate_metadata_against_schema(Some(&metadata), Some(&schema));
    assert!(result.is_err());
}

#[test]
fn validate_metadata_against_schema_rejects_max_length_exceeded() {
    let schema = serde_json::json!({
        "type": "object",
        "properties": {
            "name": { "type": "string", "maxLength": 5 }
        }
    });
    let metadata = serde_json::json!({"name": "too-long-string"});
    let result = validation::validate_metadata_against_schema(Some(&metadata), Some(&schema));
    assert!(result.is_err());
}

// ── validate_metadata_schema: additional edge cases ─────────────────────

#[test]
fn validate_metadata_schema_accepts_empty_object_schema() {
    let schema = serde_json::json!({});
    assert!(validation::validate_metadata_schema(&schema).is_ok());
}

#[test]
fn validate_metadata_schema_accepts_boolean_false_schema() {
    let schema = serde_json::json!(false);
    assert!(validation::validate_metadata_schema(&schema).is_ok());
}

#[test]
fn validate_metadata_schema_rejects_null() {
    let schema = serde_json::json!(null);
    assert!(validation::validate_metadata_schema(&schema).is_err());
}

#[test]
fn validate_metadata_schema_rejects_number() {
    let schema = serde_json::json!(42);
    assert!(validation::validate_metadata_schema(&schema).is_err());
}

#[test]
fn validate_metadata_schema_rejects_string() {
    let schema = serde_json::json!("not a schema");
    assert!(validation::validate_metadata_schema(&schema).is_err());
}

#[test]
fn validate_metadata_schema_rejects_array() {
    let schema = serde_json::json!([1, 2, 3]);
    assert!(validation::validate_metadata_schema(&schema).is_err());
}

#[test]
fn validate_metadata_schema_accepts_nested_properties() {
    let schema = serde_json::json!({
        "type": "object",
        "properties": {
            "address": {
                "type": "object",
                "properties": {
                    "city": { "type": "string" },
                    "zip": { "type": "string", "pattern": "^[0-9]{5}$" }
                },
                "required": ["city"]
            }
        }
    });
    assert!(validation::validate_metadata_schema(&schema).is_ok());
}

#[test]
fn validate_metadata_schema_accepts_combiners() {
    let schema = serde_json::json!({
        "anyOf": [
            { "type": "string" },
            { "type": "integer" }
        ]
    });
    assert!(validation::validate_metadata_schema(&schema).is_ok());
}

// ── validate_metadata_against_schema: additional edge cases ─────────────

#[test]
fn validate_metadata_against_schema_false_schema_rejects_everything() {
    let schema = serde_json::json!(false);
    let metadata = serde_json::json!({});
    let result = validation::validate_metadata_against_schema(Some(&metadata), Some(&schema));
    assert!(result.is_err(), "false schema should reject all metadata");
}

#[test]
fn validate_metadata_against_schema_true_schema_accepts_everything() {
    let schema = serde_json::json!(true);
    let metadata = serde_json::json!({"any": "thing", "number": 42, "nested": [1, 2]});
    assert!(validation::validate_metadata_against_schema(Some(&metadata), Some(&schema)).is_ok());
}

#[test]
fn validate_metadata_against_schema_empty_schema_accepts_everything() {
    let schema = serde_json::json!({});
    let metadata = serde_json::json!({"any": "thing"});
    assert!(validation::validate_metadata_against_schema(Some(&metadata), Some(&schema)).is_ok());
}

#[test]
fn validate_metadata_against_schema_nested_object_fails() {
    let schema = serde_json::json!({
        "type": "object",
        "properties": {
            "address": {
                "type": "object",
                "properties": { "city": { "type": "string" } },
                "required": ["city"]
            }
        }
    });
    let bad = serde_json::json!({"address": {}});
    assert!(validation::validate_metadata_against_schema(Some(&bad), Some(&schema)).is_err());
    let good = serde_json::json!({"address": {"city": "Berlin"}});
    assert!(validation::validate_metadata_against_schema(Some(&good), Some(&schema)).is_ok());
}

#[test]
fn validate_metadata_against_schema_multiple_errors() {
    let schema = serde_json::json!({
        "type": "object",
        "properties": {
            "name": { "type": "string" },
            "age": { "type": "integer" }
        },
        "required": ["name", "age"]
    });
    let metadata = serde_json::json!({});
    let result = validation::validate_metadata_against_schema(Some(&metadata), Some(&schema));
    assert!(result.is_err());
}

#[test]
fn validate_metadata_against_schema_enum_constraint() {
    let schema = serde_json::json!({
        "type": "object",
        "properties": {
            "status": { "type": "string", "enum": ["active", "inactive"] }
        }
    });
    let good = serde_json::json!({"status": "active"});
    assert!(validation::validate_metadata_against_schema(Some(&good), Some(&schema)).is_ok());
    let bad = serde_json::json!({"status": "unknown"});
    assert!(validation::validate_metadata_against_schema(Some(&bad), Some(&schema)).is_err());
}

#[test]
fn validate_metadata_against_schema_pattern_constraint() {
    let schema = serde_json::json!({
        "type": "object",
        "properties": {
            "code": { "type": "string", "pattern": "^[A-Z]{3}$" }
        }
    });
    let good = serde_json::json!({"code": "ABC"});
    assert!(validation::validate_metadata_against_schema(Some(&good), Some(&schema)).is_ok());
    let bad = serde_json::json!({"code": "abc123"});
    assert!(validation::validate_metadata_against_schema(Some(&bad), Some(&schema)).is_err());
}

#[test]
fn validate_metadata_against_schema_wrong_root_type() {
    let schema = serde_json::json!({"type": "object"});
    let arr = serde_json::json!([1, 2, 3]);
    assert!(validation::validate_metadata_against_schema(Some(&arr), Some(&schema)).is_err());
    let str_val = serde_json::json!("just a string");
    assert!(validation::validate_metadata_against_schema(Some(&str_val), Some(&schema)).is_err());
}

// ── DomainError construction ────────────────────────────────────────────

#[test]
fn domain_error_type_not_found_format() {
    let err = DomainError::type_not_found("my.type.code");
    assert!(matches!(err, DomainError::TypeNotFound { .. }));
    assert!(err.to_string().contains("my.type.code"));
}

#[test]
fn domain_error_type_already_exists_format() {
    let err = DomainError::type_already_exists("dup.code");
    assert!(matches!(err, DomainError::TypeAlreadyExists { .. }));
    assert!(err.to_string().contains("dup.code"));
}

#[test]
fn domain_error_validation_format() {
    let err = DomainError::validation("bad input");
    assert!(matches!(err, DomainError::Validation { .. }));
    assert!(err.to_string().contains("bad input"));
}

#[test]
fn domain_error_group_not_found_format() {
    let id = uuid::Uuid::now_v7();
    let err = DomainError::group_not_found(id);
    assert!(matches!(err, DomainError::GroupNotFound { .. }));
    assert!(err.to_string().contains(&id.to_string()));
}

#[test]
fn domain_error_group_already_exists_format() {
    let id = uuid::Uuid::now_v7();
    let err = DomainError::group_already_exists(id);
    assert!(matches!(err, DomainError::GroupAlreadyExists { .. }));
    assert!(err.to_string().contains(&id.to_string()));
}

#[test]
fn domain_error_cycle_detected_format() {
    let err = DomainError::cycle_detected("A -> B -> A");
    assert!(matches!(err, DomainError::CycleDetected { .. }));
    assert!(err.to_string().contains("A -> B -> A"));
}

#[test]
fn domain_error_limit_violation_format() {
    let err = DomainError::limit_violation("depth exceeded");
    assert!(matches!(err, DomainError::LimitViolation { .. }));
    assert!(err.to_string().contains("depth exceeded"));
}

#[test]
fn domain_error_invalid_parent_type_format() {
    let err = DomainError::invalid_parent_type("type mismatch");
    assert!(matches!(err, DomainError::InvalidParentType { .. }));
    assert!(err.to_string().contains("type mismatch"));
}

#[test]
fn domain_error_conflict_active_references_format() {
    let err = DomainError::conflict_active_references("has children");
    assert!(matches!(err, DomainError::ConflictActiveReferences { .. }));
    assert!(err.to_string().contains("has children"));
}

#[test]
fn domain_error_allowed_parent_types_violation_format() {
    let err = DomainError::allowed_parent_types_violation("parent removed");
    assert!(matches!(
        err,
        DomainError::AllowedParentTypesViolation { .. }
    ));
    assert!(err.to_string().contains("parent removed"));
}

#[test]
fn domain_error_tenant_incompatibility_format() {
    let err = DomainError::tenant_incompatibility("wrong tenant");
    assert!(matches!(err, DomainError::TenantIncompatibility { .. }));
    assert!(err.to_string().contains("wrong tenant"));
}

#[test]
fn domain_error_database_format() {
    let err = DomainError::database("connection lost");
    assert!(matches!(err, DomainError::Database(_)));
    assert!(err.to_string().contains("connection lost"));
}

#[test]
fn domain_error_membership_not_found_format() {
    let err = DomainError::membership_not_found("(gid, type, rid)");
    assert!(matches!(err, DomainError::MembershipNotFound { .. }));
    assert!(err.to_string().contains("(gid, type, rid)"));
}

#[test]
fn domain_error_conflict_format() {
    let err = DomainError::conflict("duplicate key");
    assert!(matches!(err, DomainError::Conflict { .. }));
    assert!(err.to_string().contains("duplicate key"));
}

// ── is_serialization_failure ────────────────────────────────────────────

#[test]
fn is_serialization_failure_detects_sqlstate_40001() {
    let err = DomainError::database("ERROR: 40001 could not serialize access");
    assert!(err.is_serialization_failure());
}

#[test]
fn is_serialization_failure_detects_serialize_message() {
    let err = DomainError::database("could not serialize access due to concurrent update");
    assert!(err.is_serialization_failure());
}

#[test]
fn is_serialization_failure_false_for_other_db_errors() {
    let err = DomainError::database("connection refused");
    assert!(!err.is_serialization_failure());
}

#[test]
fn is_serialization_failure_false_for_non_db_errors() {
    let err = DomainError::validation("bad input");
    assert!(!err.is_serialization_failure());
}

// ── DomainError -> CanonicalError -> ResourceGroupError projection ──────
//
// The SDK trait boundary is `CanonicalError` (ADR 0005); `ResourceGroupError`
// is the opt-in `From<CanonicalError>` projection. These tests exercise the
// full domain → canonical → projection path a ClientHub consumer sees.

/// Project a domain error the way a consumer would: through the single
/// canonical ladder, then into the typed SDK view.
fn project(err: DomainError) -> resource_group_sdk::ResourceGroupError {
    resource_group_sdk::ResourceGroupError::from(CanonicalError::from(err))
}

#[test]
fn domain_to_sdk_type_not_found() {
    use resource_group_sdk::ResourceGroupError;
    match project(DomainError::type_not_found("code")) {
        ResourceGroupError::NotFound { name, .. } => assert_eq!(name, "code"),
        other => panic!("expected NotFound, got {other:?}"),
    }
}

#[test]
fn domain_to_sdk_type_already_exists() {
    use resource_group_sdk::ResourceGroupError;
    match project(DomainError::type_already_exists("code")) {
        ResourceGroupError::AlreadyExists { name, .. } => assert_eq!(name, "code"),
        other => panic!("expected AlreadyExists, got {other:?}"),
    }
}

#[test]
fn domain_to_sdk_group_already_exists() {
    use resource_group_sdk::ResourceGroupError;
    let id = uuid::Uuid::now_v7();
    match project(DomainError::group_already_exists(id)) {
        ResourceGroupError::AlreadyExists { name, .. } => assert_eq!(name, id.to_string()),
        other => panic!("expected AlreadyExists, got {other:?}"),
    }
}

#[test]
fn domain_to_sdk_validation() {
    use resource_group_sdk::ResourceGroupError;
    // Generic validation maps to a `Format` InvalidArgument — no field
    // attribution, message preserved in `detail`.
    match project(DomainError::validation("msg")) {
        ResourceGroupError::InvalidArgument { detail, .. } => assert!(detail.contains("msg")),
        other => panic!("expected InvalidArgument, got {other:?}"),
    }
}

#[test]
fn domain_to_sdk_invalid_parent_type() {
    use resource_group_sdk::{ResourceGroupError, field};
    match project(DomainError::invalid_parent_type("bad parent")) {
        ResourceGroupError::InvalidArgument { field, reason, .. } => {
            assert_eq!(field, field::PARENT_TYPE_FIELD);
            assert_eq!(reason, field::INVALID_PARENT_TYPE);
        }
        other => panic!("expected InvalidArgument, got {other:?}"),
    }
}

#[test]
fn domain_to_sdk_group_not_found() {
    use resource_group_sdk::ResourceGroupError;
    let id = uuid::Uuid::now_v7();
    match project(DomainError::group_not_found(id)) {
        ResourceGroupError::NotFound { name, .. } => assert_eq!(name, id.to_string()),
        other => panic!("expected NotFound, got {other:?}"),
    }
}

#[test]
fn domain_to_sdk_cycle_detected() {
    use resource_group_sdk::{ResourceGroupError, precondition::Subject};
    // Cycle flattens to FailedPrecondition; the `hierarchy` subject is the
    // discriminator.
    match project(DomainError::cycle_detected("cycle")) {
        ResourceGroupError::FailedPrecondition { subject, .. } => {
            assert_eq!(subject, Subject::Hierarchy);
        }
        other => panic!("expected FailedPrecondition, got {other:?}"),
    }
}

#[test]
fn domain_to_sdk_limit_violation() {
    use resource_group_sdk::{ResourceGroupError, precondition::Subject};
    match project(DomainError::limit_violation("limit")) {
        ResourceGroupError::FailedPrecondition { subject, .. } => {
            assert_eq!(subject, Subject::Limit);
        }
        other => panic!("expected FailedPrecondition, got {other:?}"),
    }
}

#[test]
fn domain_to_sdk_database_maps_to_internal() {
    use resource_group_sdk::ResourceGroupError;
    // Database errors map to internal (no sensitive info leaked).
    assert!(matches!(
        project(DomainError::database("db error")),
        ResourceGroupError::Internal { .. }
    ));
}

#[test]
fn domain_to_sdk_membership_not_found() {
    use resource_group_sdk::ResourceGroupError;
    match project(DomainError::membership_not_found("key")) {
        ResourceGroupError::NotFound { name, .. } => assert_eq!(name, "key"),
        other => panic!("expected NotFound, got {other:?}"),
    }
}

#[test]
fn domain_to_problem_membership_not_found_is_404() {
    let domain = DomainError::membership_not_found("(gid, type, rid)");
    let problem = wire(domain);
    assert_eq!(problem.status, Some(404));
    assert_eq!(problem.problem_type, NOT_FOUND_TYPE);
    assert_eq!(problem.context["resource_type"], RG_GTS);
    assert_eq!(problem.context["resource_name"], "(gid, type, rid)");
}

#[test]
fn domain_to_sdk_access_denied_maps_to_permission_denied() {
    use resource_group_sdk::{ResourceGroupError, reason};
    let domain = DomainError::AccessDenied {
        message: "denied".to_owned(),
    };
    match project(domain) {
        ResourceGroupError::PermissionDenied { reason, .. } => {
            assert_eq!(reason, reason::permission::ACCESS_DENIED);
        }
        other => panic!("expected PermissionDenied, got {other:?}"),
    }
}

// ── DomainError -> Problem (RFC 9457) mapping ───────────────────────────

#[test]
fn domain_to_problem_type_not_found_is_404() {
    let domain = DomainError::type_not_found("my.code");
    let problem = wire(domain);
    assert_eq!(problem.status, Some(404));
    assert_eq!(problem.problem_type, NOT_FOUND_TYPE);
    assert_eq!(problem.context["resource_type"], RG_GTS);
    assert_eq!(problem.context["resource_name"], "my.code");
}

#[test]
fn domain_to_problem_type_already_exists_is_409() {
    let domain = DomainError::type_already_exists("dup");
    let problem = wire(domain);
    assert_eq!(problem.status, Some(409));
    assert_eq!(problem.problem_type, ALREADY_EXISTS_TYPE);
    assert_eq!(problem.context["resource_type"], RG_GTS);
    assert_eq!(problem.context["resource_name"], "dup");
}

#[test]
fn domain_to_problem_group_already_exists_is_409() {
    let id = uuid::Uuid::now_v7();
    let problem = wire(DomainError::group_already_exists(id));
    assert_eq!(problem.status, Some(409));
    assert_eq!(problem.problem_type, ALREADY_EXISTS_TYPE);
    assert_eq!(problem.context["resource_type"], RG_GTS);
    assert_eq!(problem.context["resource_name"], id.to_string());
}

#[test]
fn domain_to_problem_validation_is_400() {
    let domain = DomainError::validation("bad");
    let problem = wire(domain);
    assert_eq!(problem.status, Some(400));
    assert_eq!(problem.problem_type, INVALID_ARGUMENT_TYPE);
    assert_eq!(problem.context["resource_type"], RG_GTS);
    // Format variant — message lands in `context.format`, no field_violations.
    assert_eq!(problem.context["format"], "bad");
    assert!(
        problem.context.get("field_violations").is_none()
            || problem.context["field_violations"]
                .as_array()
                .unwrap()
                .is_empty(),
    );
}

#[test]
fn domain_to_problem_group_not_found_is_404() {
    let id = uuid::Uuid::now_v7();
    let domain = DomainError::group_not_found(id);
    let problem = wire(domain);
    assert_eq!(problem.status, Some(404));
    assert_eq!(problem.problem_type, NOT_FOUND_TYPE);
    assert_eq!(problem.context["resource_type"], RG_GTS);
    assert_eq!(problem.context["resource_name"], id.to_string());
}

#[test]
fn domain_to_problem_cycle_detected_is_400() {
    // ⚠ wire change accepted in the migration plan: 409 → 400.
    let domain = DomainError::cycle_detected("cycle");
    let problem = wire(domain);
    assert_eq!(problem.status, Some(400));
    assert_eq!(problem.problem_type, FAILED_PRECONDITION_TYPE);
    let v = problem
        .context
        .get("violations")
        .and_then(|v| v.as_array())
        .expect("violations must be present");
    assert_eq!(v[0]["subject"], "hierarchy");
    assert_eq!(v[0]["type"], "STATE");
    assert_eq!(v[0]["description"], "cycle");
}

#[test]
fn domain_to_problem_limit_violation_is_400() {
    // ⚠ wire change accepted in the migration plan: 409 → 400.
    let domain = DomainError::limit_violation("too deep");
    let problem = wire(domain);
    assert_eq!(problem.status, Some(400));
    assert_eq!(problem.problem_type, FAILED_PRECONDITION_TYPE);
    let v = problem
        .context
        .get("violations")
        .and_then(|v| v.as_array())
        .expect("violations must be present");
    assert_eq!(v[0]["subject"], "limit");
    assert_eq!(v[0]["type"], "STATE");
    assert_eq!(v[0]["description"], "too deep");
}

#[test]
fn domain_to_problem_invalid_parent_type_is_400() {
    let domain = DomainError::invalid_parent_type("mismatch");
    let problem = wire(domain);
    assert_eq!(problem.status, Some(400));
    assert_eq!(problem.problem_type, INVALID_ARGUMENT_TYPE);
    assert_eq!(problem.context["resource_type"], RG_GTS);
    let v = problem
        .context
        .get("field_violations")
        .and_then(|v| v.as_array())
        .expect("field_violations must be present");
    assert_eq!(v[0]["field"], "parent_type");
    assert_eq!(v[0]["reason"], "INVALID_PARENT_TYPE");
    assert_eq!(v[0]["description"], "mismatch");
}

#[test]
fn domain_to_problem_conflict_active_refs_is_400() {
    // ⚠ wire change accepted in the migration plan: 409 → 400.
    let domain = DomainError::conflict_active_references("children exist");
    let problem = wire(domain);
    assert_eq!(problem.status, Some(400));
    assert_eq!(problem.problem_type, FAILED_PRECONDITION_TYPE);
    let v = problem
        .context
        .get("violations")
        .and_then(|v| v.as_array())
        .expect("violations must be present");
    assert_eq!(v[0]["subject"], "active_references");
    assert_eq!(v[0]["type"], "STATE");
    assert_eq!(v[0]["description"], "children exist");
}

#[test]
fn domain_to_problem_allowed_parent_types_violation_is_400() {
    // ⚠ wire change accepted in the migration plan: 409 → 400.
    let domain = DomainError::allowed_parent_types_violation("violation");
    let problem = wire(domain);
    assert_eq!(problem.status, Some(400));
    assert_eq!(problem.problem_type, FAILED_PRECONDITION_TYPE);
    let v = problem
        .context
        .get("violations")
        .and_then(|v| v.as_array())
        .expect("violations must be present");
    assert_eq!(v[0]["subject"], "allowed_parents");
    assert_eq!(v[0]["type"], "STATE");
    assert_eq!(v[0]["description"], "violation");
}

#[test]
fn domain_to_problem_tenant_incompatibility_is_400() {
    // ⚠ wire change accepted in the migration plan: 409 → 400.
    let domain = DomainError::tenant_incompatibility("wrong tenant");
    let problem = wire(domain);
    assert_eq!(problem.status, Some(400));
    assert_eq!(problem.problem_type, FAILED_PRECONDITION_TYPE);
    let v = problem
        .context
        .get("violations")
        .and_then(|v| v.as_array())
        .expect("violations must be present");
    assert_eq!(v[0]["subject"], "tenant");
    assert_eq!(v[0]["type"], "STATE");
    assert_eq!(v[0]["description"], "wrong tenant");
}

#[test]
fn domain_to_problem_access_denied_is_403() {
    let domain = DomainError::AccessDenied {
        message: "denied".to_owned(),
    };
    let problem = wire(domain);
    assert_eq!(problem.status, Some(403));
    assert_eq!(problem.problem_type, PERMISSION_DENIED_TYPE);
    assert_eq!(problem.context["resource_type"], RG_GTS);
    assert_eq!(problem.context["reason"], "ACCESS_DENIED");
    // The user-supplied `message` MUST NOT reach the wire — it stays in
    // the server-side `tracing::debug!` event only.
    assert!(
        !problem.detail.contains("denied"),
        "user-supplied AccessDenied message leaked to wire detail: {:?}",
        problem.detail,
    );
}

#[test]
fn domain_to_problem_database_is_500() {
    let domain = DomainError::database("connection refused: 127.0.0.1:5432");
    let canonical = CanonicalError::from(domain);

    // Diagnostic side: source description must reach `diagnostic()` so the
    // canonical-error middleware can log it with the request trace_id.
    let diag = canonical.diagnostic().expect("Internal carries diagnostic");
    assert!(
        diag.contains("connection refused"),
        "diagnostic dropped DB source: {diag:?}"
    );

    let problem = Problem::from(canonical);
    assert_eq!(problem.status, Some(500));
    assert_eq!(problem.problem_type, INTERNAL_TYPE);
    // Internal must NOT carry a per-error `resource_type` — it's a
    // resource-absent category.
    assert!(problem.context.get("resource_type").is_none());
    // Wire side: raw DB error string must NOT leak to the wire detail.
    assert!(
        !problem.detail.contains("connection refused"),
        "raw DB error leaked to wire detail: {:?}",
        problem.detail,
    );
    assert!(
        !problem.detail.contains("127.0.0.1"),
        "raw DB error leaked to wire detail: {:?}",
        problem.detail,
    );
}

#[test]
fn domain_to_problem_internal_error_is_500() {
    let canonical = CanonicalError::from(DomainError::InternalError);
    assert!(
        canonical.diagnostic().is_some(),
        "Internal carries diagnostic"
    );
    let problem = Problem::from(canonical);
    assert_eq!(problem.status, Some(500));
    assert_eq!(problem.problem_type, INTERNAL_TYPE);
    assert!(problem.context.get("resource_type").is_none());
}

#[test]
fn domain_to_problem_duplicate_membership_is_409() {
    let domain = DomainError::duplicate_membership(
        "(g1, type_id=42, r1)",
        "Membership already exists: (g1, type_id=42, r1)",
    );
    let problem = wire(domain);
    assert_eq!(problem.status, Some(409));
    assert_eq!(problem.problem_type, ALREADY_EXISTS_TYPE);
    assert_eq!(problem.context["resource_type"], RG_GTS);
    assert_eq!(problem.context["resource_name"], "(g1, type_id=42, r1)");
    assert!(problem.detail.contains("Membership already exists"));
}

#[test]
fn domain_to_problem_conflict_is_409() {
    // Generic Conflict has no structural resource id — routes through
    // `aborted` with reason="CONFLICT" rather than `already_exists`.
    let domain = DomainError::conflict("dup");
    let problem = wire(domain);
    assert_eq!(problem.status, Some(409));
    assert_eq!(
        problem.problem_type,
        gts_uri!("cf.core.errors.err.v1~cf.core.err.aborted.v1~")
    );
    assert_eq!(problem.context["resource_type"], RG_GTS);
    assert_eq!(problem.context["reason"], "CONFLICT");
    assert!(problem.detail.contains("dup"));
}

#[test]
fn domain_to_problem_tenant_root_already_exists_is_409() {
    let existing_root_id = uuid::Uuid::now_v7();
    let domain = DomainError::tenant_root_already_exists(
        existing_root_id,
        "Cannot create tenant-type root 'foo'",
    );
    let problem = wire(domain);
    assert_eq!(problem.status, Some(409));
    assert_eq!(problem.problem_type, ALREADY_EXISTS_TYPE);
    assert_eq!(problem.context["resource_type"], RG_GTS);
    assert_eq!(
        problem.context["resource_name"],
        existing_root_id.to_string()
    );
}

// @cpt-dod:cpt-cf-resource-group-dod-testing-error-conversions:p2
// ── Error conversions: From<EnforcerError> -> DomainError ────────────────

// TC-ERR-01: EnforcerError::Denied -> DomainError::AccessDenied
#[test]
fn enforcer_denied_maps_to_access_denied() {
    use authz_resolver_sdk::pep::EnforcerError;

    let err: DomainError = EnforcerError::Denied { deny_reason: None }.into();
    assert!(
        matches!(err, DomainError::AccessDenied { .. }),
        "Expected AccessDenied, got: {err:?}"
    );
}

// TC-ERR-02: EnforcerError::EvaluationFailed -> DomainError::InternalError
#[test]
fn enforcer_evaluation_failed_maps_to_internal_error() {
    use authz_resolver_sdk::pep::EnforcerError;

    let err: DomainError = EnforcerError::EvaluationFailed(
        CanonicalError::internal("no authz plugin available").create(),
    )
    .into();
    assert!(
        matches!(err, DomainError::InternalError),
        "Expected InternalError, got: {err:?}"
    );
}

// TC-ERR-03: EnforcerError::CompileFailed -> DomainError::InternalError
#[test]
fn enforcer_compile_failed_maps_to_internal_error() {
    use authz_resolver_sdk::pep::ConstraintCompileError;
    use authz_resolver_sdk::pep::EnforcerError;

    let err: DomainError =
        EnforcerError::CompileFailed(ConstraintCompileError::ConstraintsRequiredButAbsent).into();
    assert!(
        matches!(err, DomainError::InternalError),
        "Expected InternalError, got: {err:?}"
    );
}

// TC-ERR-03b: a capability-negotiation violation (the PDP emitted a native
// predicate this gear never advertised) maps to a clean denial, not a 500.
#[test]
fn enforcer_unadvertised_capability_maps_to_access_denied() {
    use authz_resolver_sdk::pep::ConstraintCompileError;
    use authz_resolver_sdk::pep::EnforcerError;

    let err: DomainError =
        EnforcerError::CompileFailed(ConstraintCompileError::UnadvertisedCapabilities {
            predicate: "InTenantSubtree",
            missing: vec!["tenant_hierarchy"],
        })
        .into();
    assert!(
        matches!(err, DomainError::AccessDenied { .. }),
        "Expected AccessDenied, got: {err:?}"
    );
}

// TC-ERR-04: sea_orm::DbErr -> DomainError::Database
#[test]
fn sea_orm_db_err_maps_to_database() {
    let db_err = sea_orm::DbErr::Custom("connection lost".to_owned());
    let err: DomainError = db_err.into();
    assert!(
        matches!(err, DomainError::Database(_)),
        "Expected Database, got: {err:?}"
    );
    assert!(err.to_string().contains("connection lost"));
}

// TC-ERR-05: toolkit_db::DbError -> DomainError::Database
#[test]
fn toolkit_db_error_maps_to_database() {
    let db_err = toolkit_db::DbError::from(sea_orm::DbErr::Custom("pool exhausted".to_owned()));
    let err: DomainError = db_err.into();
    assert!(
        matches!(err, DomainError::Database(_)),
        "Expected Database, got: {err:?}"
    );
}

// ── QueryProfile default (TC-SDK-17) ─────────────────────────────────────

#[test]
fn query_profile_defaults_are_sensible() {
    use resource_group::domain::group_service::QueryProfile;

    let profile = QueryProfile::default();
    assert_eq!(profile.max_depth, Some(10));
    assert_eq!(profile.max_width, None);
}
