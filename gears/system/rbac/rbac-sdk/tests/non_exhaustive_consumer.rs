//! Compile-only consumer-side smoke test for the SDK's `#[non_exhaustive]`
//! ergonomics.
//!
//! Runs as its own crate so the match arms here see `#[non_exhaustive]` the
//! way external consumers do. Removing a wildcard arm or switching any
//! constructor call to a struct literal MUST be a compile error.

use chrono::{DateTime, Utc};
use rbac_sdk::error::RbacServiceError;
use rbac_sdk::models::{
    DenyReason, EffectivePermission, EvaluatePermissionRequest, EvaluatePermissionResponse,
    GetSubjectRolesRequest, GetSubjectRolesResponse, PermissionDenied, PermissionGranted,
    PermissionResult, PermissionRule, PermissionScopeType, PrincipalType, RoleAssignment,
    RoleDefinition, Scope, ScopeProvenanceError, SubjectRole,
};
use uuid::Uuid;

#[test]
fn permission_scope_type_requires_wildcard_arm_for_consumers() {
    let samples = [
        PermissionScopeType::Global,
        PermissionScopeType::TenantSubtree {
            root_tenant_id: Uuid::nil(),
        },
        PermissionScopeType::TenantDirect {
            tenant_id: Uuid::nil(),
        },
        PermissionScopeType::GroupSubtree {
            root_group_ids: vec![],
        },
        PermissionScopeType::ExplicitGroups { group_ids: vec![] },
        PermissionScopeType::Combined { scopes: vec![] },
    ];

    for sample in samples {
        let _label: &'static str = match sample {
            PermissionScopeType::Global => "global",
            PermissionScopeType::TenantSubtree { .. } => "tenant_subtree",
            PermissionScopeType::TenantDirect { .. } => "tenant_direct",
            PermissionScopeType::GroupSubtree { .. } => "group_subtree",
            PermissionScopeType::ExplicitGroups { .. } => "explicit_groups",
            PermissionScopeType::Combined { .. } => "combined",
            _ => "unknown",
        };
    }
}

#[test]
fn principal_type_requires_wildcard_arm_for_consumers() {
    let samples = [
        PrincipalType::User,
        PrincipalType::Group,
        PrincipalType::ServicePrincipal,
    ];
    for sample in samples {
        let _label: &'static str = match sample {
            PrincipalType::User => "user",
            PrincipalType::Group => "group",
            PrincipalType::ServicePrincipal => "service_principal",
            _ => "unknown",
        };
    }
}

#[test]
fn deny_reason_requires_wildcard_arm_for_consumers() {
    let samples = [
        DenyReason::NoMatchingPermission,
        DenyReason::NotPermissionExclusion,
    ];
    for sample in samples {
        let _label: &'static str = match sample {
            DenyReason::NoMatchingPermission => "no_match",
            DenyReason::NotPermissionExclusion => "exclusion",
            _ => "unknown",
        };
    }
}

#[test]
fn permission_result_requires_wildcard_arm_for_consumers() {
    let allowed = PermissionResult::Allowed(PermissionGranted::new(
        Vec::new(),
        PermissionScopeType::Global,
    ));
    let denied = PermissionResult::Denied(PermissionDenied::new(DenyReason::NoMatchingPermission));

    for sample in [allowed, denied] {
        let _label: &'static str = match sample {
            PermissionResult::Allowed(_) => "allowed",
            PermissionResult::Denied(_) => "denied",
            _ => "unknown",
        };
    }
}

#[test]
fn rbac_service_error_requires_wildcard_arm_for_consumers() {
    // Constructor methods are preferred so callers stay future-proof against
    // `#[non_exhaustive]` retroactively landing on a variant.
    let samples = [
        RbacServiceError::RoleDefinitionNotFound { id: Uuid::nil() },
        RbacServiceError::RoleAssignmentNotFound { id: Uuid::nil() },
        RbacServiceError::validation("bad input"),
        RbacServiceError::authorization_denied("denied"),
        RbacServiceError::conflict("duplicate"),
        RbacServiceError::dependency_unavailable("TenantResolverClient"),
        RbacServiceError::internal("oops"),
    ];

    for sample in samples {
        let _label: &'static str = match sample {
            RbacServiceError::RoleDefinitionNotFound { .. } => "role_def_not_found",
            RbacServiceError::RoleAssignmentNotFound { .. } => "role_assignment_not_found",
            RbacServiceError::Validation { .. } => "validation",
            RbacServiceError::AuthorizationDenied { .. } => "auth_denied",
            RbacServiceError::Conflict { .. } => "conflict",
            RbacServiceError::DependencyUnavailable { .. } => "dependency_unavailable",
            RbacServiceError::Internal { .. } => "internal",
            _ => "unknown",
        };
    }
}

/// A consumer can build a `PermissionRule` and put it on the wire.
///
/// Reading `rule.operation` back after passing `"read"` in echoed the argument.
/// The contract a consumer actually depends on is the serialised shape — field
/// names included, since those are what a payload is parsed against.
#[test]
fn permission_rule_round_trips_on_the_wire_from_a_consumer() {
    let rule = PermissionRule::new("read", "gts.cf.resources.compute.vm.v1~");

    let wire = serde_json::to_value(&rule).expect("serialise");
    assert_eq!(
        wire,
        serde_json::json!({
            "operation": "read",
            "target_type": "gts.cf.resources.compute.vm.v1~",
        }),
        "the public wire shape must stay (operation, target_type)"
    );

    let parsed: PermissionRule = serde_json::from_value(wire).expect("deserialise");
    assert_eq!(parsed, rule, "a consumer's rule must survive a round-trip");
}

#[test]
fn role_definition_constructs_via_new_from_consumer() {
    let now: DateTime<Utc> = DateTime::<Utc>::UNIX_EPOCH;
    let role = RoleDefinition::new(
        Uuid::nil(),
        "Auditor",
        Some("Read-only auditor".to_owned()),
        false,
        vec![PermissionRule::new(
            "read",
            "gts.cf.resources.compute.vm.v1~",
        )],
        vec![PermissionRule::new(
            "delete",
            "gts.cf.resources.compute.vm.v1~",
        )],
        vec![Scope::root()],
        Some(Uuid::nil()),
        now,
        now,
        "alice",
    );
    assert_eq!(role.name, "Auditor");
    assert!(!role.is_built_in);
    assert_eq!(role.permissions.len(), 1);
    assert_eq!(role.not_permissions.len(), 1);
}

#[test]
fn role_assignment_constructs_via_new_from_consumer() {
    let now: DateTime<Utc> = DateTime::<Utc>::UNIX_EPOCH;
    let assignment = RoleAssignment::new(
        Uuid::nil(),
        Uuid::nil(),
        "subject-1",
        PrincipalType::User,
        rbac_sdk::models::Scope::tenant(uuid::uuid!("11111111-2222-3333-4444-555555555555")),
        now,
        now,
        "alice",
    );
    assert_eq!(assignment.principal_id, "subject-1");
    assert!(matches!(assignment.principal_type, PrincipalType::User));
    assert!(
        assignment.principal_name.is_none(),
        "`new` must leave the read-path display names unset"
    );
    assert!(
        assignment.role_definition_name.is_none(),
        "`new` must leave the read-path display names unset"
    );

    // Display names arrive through chainable setters, never as extra
    // `new` arguments — this is the consumer-side guard that widening
    // the read model stays source-compatible for external callers. Every
    // name added later MUST arrive the same way, hence the third one
    // being exercised here too.
    let named = assignment
        .with_principal_name(Some("Ada Lovelace".to_owned()))
        .with_role_definition_name(Some("Tenant Administrator".to_owned()));
    assert_eq!(named.principal_name.as_deref(), Some("Ada Lovelace"));
    assert_eq!(
        named.role_definition_name.as_deref(),
        Some("Tenant Administrator")
    );
    assert!(named.created_by_name.is_none());
}

#[test]
fn subject_role_constructs_via_new_from_consumer() {
    let role = SubjectRole::new(
        Uuid::nil(),
        Uuid::nil(),
        "Auditor",
        Vec::new(),
        Vec::new(),
        rbac_sdk::models::Scope::Root,
        false,
        "subject-1",
        PrincipalType::User,
    );
    assert_eq!(role.role_name, "Auditor");
}

#[test]
fn effective_permission_constructs_via_new_from_consumer() {
    let grant = EffectivePermission::new(
        PermissionRule::new("read", "gts.cf.resources.compute.vm.v1~"),
        Uuid::nil(),
        Uuid::nil(),
        "Auditor",
        rbac_sdk::models::Scope::Root,
        false,
    );
    assert_eq!(grant.role_name, "Auditor");
}

#[test]
fn permission_granted_and_denied_construct_via_new_from_consumer() {
    let granted = PermissionGranted::new(Vec::new(), PermissionScopeType::Global);
    let denied = PermissionDenied::new(DenyReason::NoMatchingPermission);
    assert!(granted.grants.is_empty());
    assert!(matches!(denied.reason, DenyReason::NoMatchingPermission));
}

#[test]
fn permission_granted_derives_and_validates_scope_from_consumer() -> Result<(), ScopeProvenanceError>
{
    let tenant_id = Uuid::new_v4();
    let grant = EffectivePermission::new(
        PermissionRule::new("read", "gts.cf.resources.compute.vm.v1~"),
        Uuid::new_v4(),
        Uuid::new_v4(),
        "Tenant Reader",
        rbac_sdk::models::Scope::tenant(tenant_id),
        false,
    );
    let granted = PermissionGranted::from_grants(vec![grant])?;
    assert_eq!(
        granted.scope_type,
        PermissionScopeType::TenantSubtree {
            root_tenant_id: tenant_id
        }
    );
    assert_eq!(granted.validate_scope_provenance(), Ok(()));

    assert!(matches!(
        PermissionGranted::from_grants(Vec::new()),
        Err(ScopeProvenanceError::EmptyGrants)
    ));
    let error = ScopeProvenanceError::EmptyGrants;
    let _label = match error {
        ScopeProvenanceError::EmptyGrants => "empty",
        ScopeProvenanceError::AggregateMismatch => "mismatch",
        _ => "unknown",
    };
    Ok(())
}

#[test]
fn request_response_dtos_construct_via_new_from_consumer() {
    let get_request = GetSubjectRolesRequest::new(
        "subject-1",
        PrincipalType::User,
        rbac_sdk::models::Scope::tenant(Uuid::nil()),
        false,
    );
    let _get_response = GetSubjectRolesResponse::new(Vec::new());
    let eval_request = EvaluatePermissionRequest::new(
        "subject-1",
        PrincipalType::ServicePrincipal,
        "read",
        rbac_sdk::models::Scope::tenant(Uuid::nil()),
        "gts.cf.resources.compute.vm.v1~",
    );
    let _eval_response = EvaluatePermissionResponse::from_result(PermissionResult::Denied(
        PermissionDenied::new(DenyReason::NoMatchingPermission),
    ));

    assert_eq!(get_request.subject_id, "subject-1");
    assert_eq!(eval_request.operation, "read");
}
