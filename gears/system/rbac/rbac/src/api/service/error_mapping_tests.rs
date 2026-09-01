//! Pinning tests for `From<DomainError> for RbacServiceError`.
//!
//! ## What these tests pin
//!
//! Most of the per-variant tests in this file are field-preservation
//! pins (the underlying `From` impl is an exhaustive `match`, so the
//! variant table itself is enforced by the type system at compile
//! time). They pin the field-roundtrip contract: `RoleDefinitionNameTaken`
//! preserves `name` + `owner_tenant_id`; `StaleEtag` preserves
//! `current_etag`; `Aborted` carries the conflict detail across the
//! `Conflict` lift; etc. A regression that re-shuffles the field
//! mapping (e.g. cross-wires `current_etag` into a different variant)
//! still compiles but trips the per-field assert.
//!
//! `every_domain_variant_maps_to_a_handled_sdk_variant` is the
//! coverage probe: it walks a representative `DomainError` per
//! discriminant and asserts none of them fall through to an
//! `RbacServiceError::Internal { message: "unmapped …" }`-style
//! placeholder. Catches a `DomainError` variant being added without
//! an arm in `error_mapping.rs`.
//!
//! The `scope_within_owner_tenant` helper moved into the domain
//! services (item #7); its unit tests moved with it.

#![allow(clippy::panic)]

use rbac_sdk::error::RbacServiceError;
use uuid::{Uuid, uuid};

use crate::domain::error::DomainError;

const SAMPLE_ROLE_ID: Uuid = uuid!("11111111-1111-1111-1111-111111111111");
const SAMPLE_TENANT_ID: Uuid = uuid!("22222222-2222-2222-2222-222222222222");

// -----------------------------------------------------------------------------
// DomainError → RbacServiceError — one arm per AIP-193 category.
// -----------------------------------------------------------------------------

#[test]
fn validation_maps_to_validation_with_detail() {
    let mapped = RbacServiceError::from(DomainError::Validation {
        detail: "bad input".into(),
    });
    match mapped {
        RbacServiceError::Validation { message } => assert_eq!(message, "bad input"),
        other => panic!("expected Validation, got {other:?}"),
    }
}

#[test]
fn invalid_permission_rule_maps_to_invalid_permission_rule() {
    let mapped = RbacServiceError::from(DomainError::InvalidPermissionRule {
        detail: "bad rule".into(),
    });
    match mapped {
        RbacServiceError::InvalidPermissionRule { detail } => assert_eq!(detail, "bad rule"),
        other => panic!("expected InvalidPermissionRule, got {other:?}"),
    }
}

#[test]
fn invalid_scope_format_maps_to_invalid_scope_format() {
    let mapped = RbacServiceError::from(DomainError::InvalidScopeFormat {
        scope: "not-a-scope".into(),
    });
    match mapped {
        RbacServiceError::InvalidScopeFormat { scope } => assert_eq!(scope, "not-a-scope"),
        other => panic!("expected InvalidScopeFormat, got {other:?}"),
    }
}

#[test]
fn role_definition_not_found_maps_to_role_definition_not_found() {
    let mapped = RbacServiceError::from(DomainError::RoleDefinitionNotFound { id: SAMPLE_ROLE_ID });
    match mapped {
        RbacServiceError::RoleDefinitionNotFound { id } => assert_eq!(id, SAMPLE_ROLE_ID),
        other => panic!("expected RoleDefinitionNotFound, got {other:?}"),
    }
}

#[test]
fn scope_not_found_maps_to_scope_not_found() {
    let mapped = RbacServiceError::from(DomainError::ScopeNotFound {
        scope: "/tenants/zzz".into(),
    });
    match mapped {
        RbacServiceError::ScopeNotFound { scope } => assert_eq!(scope, "/tenants/zzz"),
        other => panic!("expected ScopeNotFound, got {other:?}"),
    }
}

#[test]
fn role_definition_name_taken_preserves_name_and_tenant() {
    let mapped = RbacServiceError::from(DomainError::RoleDefinitionNameTaken {
        name: "Reader".into(),
        owner_tenant_id: Some(SAMPLE_TENANT_ID),
    });
    match mapped {
        RbacServiceError::RoleDefinitionNameTaken {
            name,
            owner_tenant_id,
        } => {
            assert_eq!(name, "Reader");
            assert_eq!(owner_tenant_id, Some(SAMPLE_TENANT_ID));
        }
        other => panic!("expected RoleDefinitionNameTaken, got {other:?}"),
    }
}

#[test]
fn role_definition_name_reserved_by_builtin_preserves_name() {
    let mapped = RbacServiceError::from(DomainError::RoleDefinitionNameReservedByBuiltin {
        name: "Owner".into(),
    });
    match mapped {
        RbacServiceError::RoleDefinitionNameReservedByBuiltin { name } => {
            assert_eq!(name, "Owner");
        }
        other => panic!("expected RoleDefinitionNameReservedByBuiltin, got {other:?}"),
    }
}

#[test]
fn role_definition_assignments_exist_preserves_id() {
    let mapped = RbacServiceError::from(DomainError::RoleDefinitionAssignmentsExist {
        role_definition_id: SAMPLE_ROLE_ID,
    });
    match mapped {
        RbacServiceError::RoleDefinitionAssignmentsExist { role_definition_id } => {
            assert_eq!(role_definition_id, SAMPLE_ROLE_ID);
        }
        other => panic!("expected RoleDefinitionAssignmentsExist, got {other:?}"),
    }
}

#[test]
fn aborted_maps_to_conflict_with_detail() {
    let mapped = RbacServiceError::from(DomainError::Aborted {
        reason: "SERIALIZATION_CONFLICT".into(),
        detail: "retry budget exhausted".into(),
    });
    match mapped {
        RbacServiceError::Conflict { message } => assert_eq!(message, "retry budget exhausted"),
        other => panic!("expected Conflict, got {other:?}"),
    }
}

#[test]
fn stale_etag_maps_to_optimistic_concurrency_stale() {
    let mapped = RbacServiceError::from(DomainError::StaleEtag {
        current_etag: "W/\"abc123\"".into(),
    });
    match mapped {
        RbacServiceError::OptimisticConcurrencyStale { current_etag } => {
            assert_eq!(current_etag, "W/\"abc123\"");
        }
        other => panic!("expected OptimisticConcurrencyStale, got {other:?}"),
    }
}

#[test]
fn authorization_denied_maps_to_authorization_denied() {
    let mapped = RbacServiceError::from(DomainError::authorization_denied("write denied"));
    match mapped {
        RbacServiceError::AuthorizationDenied { message } => assert_eq!(message, "write denied"),
        other => panic!("expected AuthorizationDenied, got {other:?}"),
    }
}

#[test]
fn dependency_unavailable_carries_static_dependency_name() {
    let mapped = RbacServiceError::from(DomainError::DependencyUnavailable {
        dependency: "TypesRegistryClient",
    });
    match mapped {
        RbacServiceError::DependencyUnavailable { dependency } => {
            assert_eq!(dependency, "TypesRegistryClient");
        }
        other => panic!("expected DependencyUnavailable, got {other:?}"),
    }
}

#[test]
fn internal_preserves_diagnostic() {
    let mapped = RbacServiceError::from(DomainError::internal("db borked"));
    match mapped {
        RbacServiceError::Internal { message } => assert_eq!(message, "db borked"),
        other => panic!("expected Internal, got {other:?}"),
    }
}

#[test]
fn service_unavailable_keeps_its_own_variant_with_detail() {
    // A transient outage must NOT collapse into Internal: that turns a
    // retryable 503 into a 500 telling the caller not to retry.
    let mapped = RbacServiceError::from(DomainError::service_unavailable("pool exhausted"));
    match mapped {
        RbacServiceError::ServiceUnavailable {
            detail,
            retry_after_seconds,
        } => {
            assert_eq!(detail, "pool exhausted");
            assert_eq!(retry_after_seconds, None);
        }
        other => panic!("expected ServiceUnavailable, got {other:?}"),
    }
}

#[test]
fn service_unavailable_carries_the_retry_hint_across_the_boundary() {
    let mapped = RbacServiceError::from(DomainError::ServiceUnavailable {
        detail: "tenant resolver timed out".into(),
        retry_after: Some(std::time::Duration::from_secs(30)),
        cause: None,
    });
    match mapped {
        RbacServiceError::ServiceUnavailable {
            retry_after_seconds,
            ..
        } => assert_eq!(retry_after_seconds, Some(30)),
        other => panic!("expected ServiceUnavailable, got {other:?}"),
    }
}

// `scope_within_owner_tenant` is covered in the domain service modules,
// alongside its helper.

// -----------------------------------------------------------------------------
// Discriminant-coverage probe.
//
// One representative input per `DomainError` discriminant; the assert
// is that NONE of them map to `RbacServiceError::Internal { message }`
// with the canary substring "unmapped" / "TODO". The mapper is an
// exhaustive `match`, so a new `DomainError` variant added without a
// corresponding arm would fail to compile — but the canary still
// guards against a lazy arm that just panics into `internal(...)` to
// silence the type checker.
// -----------------------------------------------------------------------------

#[test]
fn every_domain_variant_maps_to_a_handled_sdk_variant() {
    use std::time::Duration;

    let nil = Uuid::nil();
    let inputs: Vec<DomainError> = vec![
        DomainError::Validation { detail: "v".into() },
        DomainError::InvalidPermissionRule { detail: "p".into() },
        DomainError::InvalidScopeFormat { scope: "/".into() },
        DomainError::RoleDefinitionNotFound { id: nil },
        DomainError::RoleAssignmentNotFound { id: nil },
        DomainError::ScopeNotFound { scope: "/".into() },
        DomainError::GroupPrincipalNotFound { principal_id: nil },
        DomainError::RoleDefinitionNameTaken {
            name: "n".into(),
            owner_tenant_id: Some(nil),
        },
        DomainError::RoleDefinitionNameReservedByBuiltin {
            name: "Owner".into(),
        },
        DomainError::RoleAssignmentDuplicate {
            role_definition_id: nil,
            principal_type: rbac_sdk::models::PrincipalType::User,
            principal_id: "x".into(),
            scope: "/".into(),
        },
        DomainError::AlreadyExists {
            detail: "dup".into(),
        },
        DomainError::BuiltInRoleNotModifiable {
            role_definition_id: nil,
        },
        DomainError::Aborted {
            reason: "SERIALIZATION_CONFLICT".into(),
            detail: "retry".into(),
        },
        DomainError::StaleEtag {
            current_etag: "e".into(),
        },
        DomainError::RoleDefinitionAssignmentsExist {
            role_definition_id: nil,
        },
        DomainError::RoleDefinitionMissing {
            role_definition_id: nil,
        },
        DomainError::Conflict { detail: "c".into() },
        DomainError::ScopeNotWithinAssignableScopes {
            scope: "/x".into(),
            assignable_scopes: vec!["/".into()],
        },
        DomainError::GroupPrincipalRootScopeForbidden,
        DomainError::OptimisticConcurrencyMissing,
        DomainError::OwnerTenantRequired,
        DomainError::OwnerTenantMismatch,
        DomainError::AuthorizationDenied {
            detail: "denied".into(),
            cause: None,
        },
        DomainError::ServiceUnavailable {
            detail: "down".into(),
            retry_after: Some(Duration::from_secs(1)),
            cause: None,
        },
        DomainError::DependencyUnavailable {
            dependency: "TenantResolverClient",
        },
        DomainError::UnsupportedOperation {
            detail: "nope".into(),
        },
        DomainError::Internal {
            diagnostic: "boom".into(),
            cause: None,
        },
    ];

    for input in inputs {
        let label = format!("{input:?}");
        let mapped = RbacServiceError::from(input);
        if let RbacServiceError::Internal { ref message } = mapped {
            assert!(
                !message.contains("unmapped") && !message.contains("TODO"),
                "DomainError `{label}` mapped to a placeholder Internal: {message}"
            );
        }
    }
}
