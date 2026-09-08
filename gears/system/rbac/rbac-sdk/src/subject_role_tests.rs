//! Serialisation round-trips, non-exhaustive coverage, and
//! invariant tests for the evaluator-facing types in [`super`].

#![allow(clippy::expect_used, clippy::panic)]

use uuid::Uuid;

use super::{
    DenyReason, EffectivePermission, EvaluatePermissionRequest, EvaluatePermissionResponse,
    GetSubjectRolesRequest, GetSubjectRolesResponse, PermissionDenied, PermissionGranted,
    PermissionResult, PermissionScopeType, ScopeProvenanceError, SubjectRole,
};
use crate::permission_rule::PermissionRule;
use crate::role_assignment::PrincipalType;
use crate::scope::Scope;

fn round_trip<T>(value: &T) -> T
where
    T: serde::Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug,
{
    let bytes = serde_json::to_vec(value).expect("serialize");
    let back: T = serde_json::from_slice(&bytes).expect("deserialize");
    assert_eq!(value, &back, "round-trip must be lossless");
    back
}

fn sample_permission_rule() -> PermissionRule {
    PermissionRule::new("read", "gts.cf.resources.compute.vm.v1~")
}

fn sample_scope() -> Scope {
    Scope::tenant(uuid::uuid!("11111111-2222-3333-4444-555555555555"))
}

fn sample_subject_role() -> SubjectRole {
    SubjectRole::new(
        Uuid::nil(),
        Uuid::nil(),
        "Auditor",
        vec![sample_permission_rule()],
        vec![],
        sample_scope(),
        false,
        "user-1",
        PrincipalType::User,
    )
}

fn sample_effective_permission() -> EffectivePermission {
    EffectivePermission::new(
        sample_permission_rule(),
        Uuid::nil(),
        Uuid::nil(),
        "Auditor",
        sample_scope(),
        false,
    )
}

fn effective_permission_at(scope: Scope) -> EffectivePermission {
    EffectivePermission::new(
        sample_permission_rule(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        "Scoped reader",
        scope,
        false,
    )
}

#[test]
fn permission_granted_derives_tenant_scope_from_assignment() {
    let tenant_id = Uuid::new_v4();
    let granted =
        PermissionGranted::from_grants(vec![effective_permission_at(Scope::tenant(tenant_id))])
            .expect("a tenant assignment has valid scope provenance");

    assert!(matches!(
        granted.scope_type,
        PermissionScopeType::TenantSubtree { root_tenant_id } if root_tenant_id == tenant_id
    ));
    assert_eq!(granted.validate_scope_provenance(), Ok(()));
}

#[test]
fn permission_granted_derives_group_scope_for_collection_grant() {
    let tenant_id = Uuid::new_v4();
    let group_id = Uuid::new_v4();
    let granted = PermissionGranted::from_grants(vec![effective_permission_at(
        Scope::resource_group(tenant_id, group_id),
    )])
    .expect("a resource-group assignment has valid scope provenance");

    assert_eq!(
        granted.scope_type,
        PermissionScopeType::GroupSubtree {
            root_group_ids: vec![group_id]
        }
    );
    assert_eq!(granted.validate_scope_provenance(), Ok(()));
}

#[test]
fn permission_granted_derives_global_only_from_root_assignment() {
    let granted = PermissionGranted::from_grants(vec![effective_permission_at(Scope::root())])
        .expect("a root assignment has valid scope provenance");

    assert_eq!(granted.scope_type, PermissionScopeType::Global);
    assert_eq!(granted.validate_scope_provenance(), Ok(()));
}

#[test]
fn permission_granted_merges_groups_and_combines_tenant_scope() {
    let tenant_id = Uuid::new_v4();
    let group_a = Uuid::new_v4();
    let group_b = Uuid::new_v4();
    let granted = PermissionGranted::from_grants(vec![
        effective_permission_at(Scope::tenant(tenant_id)),
        effective_permission_at(Scope::resource_group(tenant_id, group_b)),
        effective_permission_at(Scope::resource_group(tenant_id, group_a)),
    ])
    .expect("mixed scoped assignments have valid aggregate provenance");

    let PermissionScopeType::Combined { scopes } = granted.scope_type else {
        panic!("tenant and resource-group assignments must produce Combined");
    };
    assert_eq!(scopes.len(), 2);
    assert_eq!(
        scopes[0],
        PermissionScopeType::TenantSubtree {
            root_tenant_id: tenant_id
        }
    );
    let PermissionScopeType::GroupSubtree { root_group_ids } = &scopes[1] else {
        panic!("second aggregate leg must be GroupSubtree");
    };
    let mut expected = vec![group_a, group_b];
    expected.sort_unstable();
    assert_eq!(root_group_ids, &expected);
}

#[test]
fn permission_granted_aggregate_is_independent_of_grant_order() {
    let tenant_a = Uuid::from_u128(2);
    let tenant_b = Uuid::from_u128(1);
    let group_a = Uuid::from_u128(4);
    let group_b = Uuid::from_u128(3);
    let scopes = [
        Scope::resource_group(tenant_a, group_a),
        Scope::tenant(tenant_a),
        Scope::resource_group(tenant_b, group_b),
        Scope::tenant(tenant_b),
    ];
    let forward = PermissionGranted::from_grants(
        scopes
            .iter()
            .cloned()
            .map(effective_permission_at)
            .collect(),
    )
    .expect("non-empty scoped grants must aggregate");
    let reverse = PermissionGranted::from_grants(
        scopes
            .iter()
            .rev()
            .cloned()
            .map(effective_permission_at)
            .collect(),
    )
    .expect("the reversed grant set must aggregate");

    assert_eq!(forward.scope_type, reverse.scope_type);
    assert_eq!(
        forward.scope_type,
        PermissionScopeType::Combined {
            scopes: vec![
                PermissionScopeType::TenantSubtree {
                    root_tenant_id: tenant_b,
                },
                PermissionScopeType::TenantSubtree {
                    root_tenant_id: tenant_a,
                },
                PermissionScopeType::GroupSubtree {
                    root_group_ids: vec![group_b, group_a],
                },
            ],
        }
    );
}

#[test]
fn permission_granted_accepts_equivalent_noncanonical_aggregate_order() {
    let tenant_id = Uuid::from_u128(1);
    let group_a = Uuid::from_u128(2);
    let group_b = Uuid::from_u128(3);
    let grants = vec![
        effective_permission_at(Scope::tenant(tenant_id)),
        effective_permission_at(Scope::resource_group(tenant_id, group_a)),
        effective_permission_at(Scope::resource_group(tenant_id, group_b)),
    ];
    let supplied_by_older_producer = PermissionGranted::new(
        grants,
        PermissionScopeType::Combined {
            scopes: vec![
                PermissionScopeType::GroupSubtree {
                    root_group_ids: vec![group_b, group_a],
                },
                PermissionScopeType::TenantSubtree {
                    root_tenant_id: tenant_id,
                },
            ],
        },
    );

    assert_eq!(
        supplied_by_older_producer.validate_scope_provenance(),
        Ok(()),
        "equivalent aggregate ordering must remain compatible across producer versions"
    );
}

#[test]
fn permission_granted_rejects_empty_normal_allow() {
    assert_eq!(
        PermissionGranted::from_grants(Vec::new()),
        Err(ScopeProvenanceError::EmptyGrants)
    );
}

#[test]
fn permission_granted_rejects_forged_global_for_scoped_assignment() {
    let forged = PermissionGranted::new(
        vec![effective_permission_at(Scope::tenant(Uuid::new_v4()))],
        PermissionScopeType::Global,
    );

    assert_eq!(
        forged.validate_scope_provenance(),
        Err(ScopeProvenanceError::AggregateMismatch)
    );
}

#[test]
fn permission_granted_rejects_wrong_tenant_and_group_roots() {
    let tenant_forgery = PermissionGranted::new(
        vec![effective_permission_at(Scope::tenant(Uuid::new_v4()))],
        PermissionScopeType::TenantSubtree {
            root_tenant_id: Uuid::new_v4(),
        },
    );
    assert_eq!(
        tenant_forgery.validate_scope_provenance(),
        Err(ScopeProvenanceError::AggregateMismatch)
    );

    let group_forgery = PermissionGranted::new(
        vec![effective_permission_at(Scope::resource_group(
            Uuid::new_v4(),
            Uuid::new_v4(),
        ))],
        PermissionScopeType::GroupSubtree {
            root_group_ids: vec![Uuid::new_v4()],
        },
    );
    assert_eq!(
        group_forgery.validate_scope_provenance(),
        Err(ScopeProvenanceError::AggregateMismatch)
    );
}

// `#[non_exhaustive]` has no effect inside the defining crate, so an in-crate
// `match` with a `_ =>` arm under `#[allow(unreachable_patterns)]` and no
// assertion could not fail however the enum changed. The contract is a
// CONSUMER-side one and is checked where a consumer lives:
// `tests/non_exhaustive_consumer.rs`.

/// Renaming or removing a field on either DTO makes this stop compiling; the
/// assertions make it more than that, so a field silently changing meaning is
/// visible at runtime too rather than only at the type level.
#[test]
fn request_dtos_construct_with_all_required_fields() {
    let tenant = Uuid::from_u128(0x5EED);

    let get = GetSubjectRolesRequest {
        subject_id: "subject-1".to_owned(),
        principal_type: PrincipalType::User,
        context_scope: Scope::tenant(tenant),
        include_group_roles: false,
    };
    assert_eq!(get.subject_id, "subject-1");
    assert_eq!(get.principal_type, PrincipalType::User);
    assert_eq!(get.context_scope, Scope::tenant(tenant));
    assert!(!get.include_group_roles);

    let eval = EvaluatePermissionRequest {
        subject_id: "subject-1".to_owned(),
        principal_type: PrincipalType::ServicePrincipal,
        operation: "read".to_owned(),
        context_scope: Scope::tenant(tenant),
        resource_type: "gts.cf.resources.compute.vm.v1~".to_owned(),
    };
    assert_eq!(eval.subject_id, "subject-1");
    assert_eq!(eval.principal_type, PrincipalType::ServicePrincipal);
    assert_eq!(eval.operation, "read");
    assert_eq!(eval.context_scope, Scope::tenant(tenant));
    assert_eq!(eval.resource_type, "gts.cf.resources.compute.vm.v1~");
}

#[test]
fn subject_role_round_trips() {
    round_trip(&sample_subject_role());
}

#[test]
fn effective_permission_round_trips() {
    round_trip(&sample_effective_permission());
}

#[test]
fn get_subject_roles_request_response_round_trip() {
    let request = GetSubjectRolesRequest {
        subject_id: "user-1".to_owned(),
        principal_type: PrincipalType::User,
        context_scope: Scope::tenant(Uuid::nil()),
        include_group_roles: true,
    };
    round_trip(&request);
    let response = GetSubjectRolesResponse {
        roles: vec![sample_subject_role()],
    };
    round_trip(&response);
}

#[test]
fn evaluate_permission_request_response_round_trip_allowed() {
    let request = EvaluatePermissionRequest {
        subject_id: "user-1".to_owned(),
        principal_type: PrincipalType::User,
        operation: "read".to_owned(),
        context_scope: Scope::tenant(Uuid::nil()),
        resource_type: "gts.cf.resources.compute.vm.v1~".to_owned(),
    };
    round_trip(&request);
    let response =
        EvaluatePermissionResponse::from_result(PermissionResult::Allowed(PermissionGranted {
            grants: vec![sample_effective_permission()],
            scope_type: PermissionScopeType::Global,
        }));
    round_trip(&response);
}

#[test]
fn evaluate_permission_request_response_round_trip_denied() {
    let response =
        EvaluatePermissionResponse::from_result(PermissionResult::Denied(PermissionDenied {
            reason: DenyReason::NoMatchingPermission,
        }));
    round_trip(&response);
}

/// `allowed()` is derived from the variant — the invariant holds by
/// construction across every `PermissionResult` variant.
#[test]
fn allowed_is_derived_from_the_result_variant() {
    let allowed_response =
        EvaluatePermissionResponse::from_result(PermissionResult::Allowed(PermissionGranted {
            grants: vec![sample_effective_permission()],
            scope_type: PermissionScopeType::Global,
        }));
    assert!(allowed_response.allowed());
    assert!(matches!(
        allowed_response.result,
        PermissionResult::Allowed(_)
    ));

    let denied_response =
        EvaluatePermissionResponse::from_result(PermissionResult::Denied(PermissionDenied {
            reason: DenyReason::NoMatchingPermission,
        }));
    assert!(!denied_response.allowed());
    assert!(matches!(
        denied_response.result,
        PermissionResult::Denied(_)
    ));
}

/// A payload that CLAIMS an allow next to a `Denied` result cannot produce a
/// contradictory value: there is no `allowed` field to deserialize into, so the
/// extra key is ignored and `allowed()` still reads the discriminant.
///
/// This is the shape the old design could not refuse — `{allowed: true, result:
/// Denied(_)}` deserialized straight through the derived impl, and any caller
/// that trusted the bool saw a deny as an allow.
#[test]
fn a_payload_claiming_allowed_beside_a_denied_result_is_still_denied() {
    // Built from a real deny so the payload tracks the actual wire shape, then
    // labelled `allowed: true` the way the old struct would have accepted.
    let denied =
        EvaluatePermissionResponse::from_result(PermissionResult::Denied(PermissionDenied {
            reason: DenyReason::NoMatchingPermission,
        }));
    let mut hostile = serde_json::to_value(&denied).expect("serialise");
    hostile
        .as_object_mut()
        .expect("response serialises as a JSON object")
        .insert("allowed".to_owned(), serde_json::Value::Bool(true));

    let parsed: EvaluatePermissionResponse =
        serde_json::from_value(hostile).expect("the stray key must be ignored, not fatal");
    assert!(
        !parsed.allowed(),
        "a Denied result MUST read as a deny however the payload was labelled"
    );
    assert!(matches!(parsed.result, PermissionResult::Denied(_)));
}

/// The wire form carries no `allowed` key at all — the decision is spelled once.
#[test]
fn the_wire_form_does_not_carry_a_separate_allowed_flag() {
    let response =
        EvaluatePermissionResponse::from_result(PermissionResult::Denied(PermissionDenied {
            reason: DenyReason::NoMatchingPermission,
        }));
    let value = serde_json::to_value(&response).expect("serialise");
    assert!(
        value.get("allowed").is_none(),
        "a second encoding of the decision must not reach the wire; body={value}"
    );
}

/// Serde round-trip keeps `allowed()` agreeing with `result`.
#[test]
fn round_trip_preserves_the_derived_allowed_reading() {
    for response in [
        EvaluatePermissionResponse::from_result(PermissionResult::Allowed(PermissionGranted {
            grants: vec![sample_effective_permission()],
            scope_type: PermissionScopeType::Global,
        })),
        EvaluatePermissionResponse::from_result(PermissionResult::Denied(PermissionDenied {
            reason: DenyReason::NoMatchingPermission,
        })),
    ] {
        let serialised = serde_json::to_string(&response).expect("serialise");
        let parsed: EvaluatePermissionResponse =
            serde_json::from_str(&serialised).expect("deserialise");
        assert_eq!(
            parsed.allowed(),
            matches!(parsed.result, PermissionResult::Allowed(_)),
            "round-trip dropped the allowed/result agreement for {response:?}"
        );
    }
}

#[test]
fn deny_reason_round_trips_every_variant() {
    for variant in [
        DenyReason::NoMatchingPermission,
        DenyReason::NotPermissionExclusion,
    ] {
        round_trip(&variant);
    }
}

#[test]
fn permission_scope_type_round_trips_every_active_and_reserved_variant() {
    // Reserved variants (`TenantDirect`, `ExplicitGroups`) MUST round-trip
    // identically. The fail-closed mapping to deny happens AFTER deserialisation.
    let id = Uuid::nil();
    let variants = [
        PermissionScopeType::Global,
        PermissionScopeType::TenantSubtree { root_tenant_id: id },
        PermissionScopeType::TenantDirect { tenant_id: id },
        PermissionScopeType::GroupSubtree {
            root_group_ids: vec![id],
        },
        PermissionScopeType::ExplicitGroups {
            group_ids: vec![id],
        },
        PermissionScopeType::Combined {
            scopes: vec![PermissionScopeType::Global],
        },
    ];
    for variant in variants {
        round_trip(&variant);
    }
}

#[test]
fn permission_result_serde_tag_is_pascal_case() {
    // Lock the wire format: tagged representation uses PascalCase for
    // `Allowed` / `Denied`.
    let allowed = PermissionResult::Allowed(PermissionGranted {
        grants: vec![],
        scope_type: PermissionScopeType::Global,
    });
    let json = serde_json::to_string(&allowed).expect("serialize");
    assert!(json.contains("\"type\":\"Allowed\""));
    assert!(!json.contains("\"type\":\"allowed\""));

    let denied = PermissionResult::Denied(PermissionDenied {
        reason: DenyReason::NoMatchingPermission,
    });
    let json = serde_json::to_string(&denied).expect("serialize");
    assert!(json.contains("\"type\":\"Denied\""));
}
