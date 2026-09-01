#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use super::*;
use crate::domain::deny::error_codes::{CONSTRAINTS_UNAVAILABLE_V1, INSUFFICIENT_PERMISSIONS_V1};
use crate::test_support::EvaluationRequestBuilder;

fn cfg_with_max(max_expansion_ids: usize) -> AuthZResolverPluginConfig {
    let mut config = AuthZResolverPluginConfig::default();
    config.capability_degradation.max_expansion_ids = max_expansion_ids;
    config
}

fn default_cfg() -> AuthZResolverPluginConfig {
    AuthZResolverPluginConfig::default()
}

fn request_with(supported: Vec<&str>) -> EvaluationRequest {
    EvaluationRequestBuilder::default()
        .with_supported_properties(supported.into_iter().map(String::from).collect())
        .build()
}

fn unwrap_allow(outcome: ConstraintOutcome) -> Vec<Constraint> {
    match outcome {
        ConstraintOutcome::Allow(constraints) => constraints,
        ConstraintOutcome::Deny(response) => panic!(
            "expected Allow, got Deny({:?})",
            response.context.deny_reason
        ),
    }
}

fn unwrap_deny(outcome: ConstraintOutcome) -> EvaluationResponse {
    match outcome {
        ConstraintOutcome::Deny(response) => response,
        ConstraintOutcome::Allow(constraints) => {
            panic!("expected Deny, got Allow({constraints:?})")
        }
    }
}

fn single_predicate(constraints: Vec<Constraint>) -> Predicate {
    assert_eq!(constraints.len(), 1, "expected exactly one constraint");
    let mut predicates = constraints.into_iter().next().unwrap().predicates;
    assert_eq!(predicates.len(), 1, "expected exactly one predicate");
    predicates.remove(0)
}

/// Assert a constraint's predicates contain an `In(id, …)` of the expected
/// length. Group constraints are tenant-paired (two predicates), so this
/// searches rather than indexing position 0.
fn assert_in_on_id(predicates: &[Predicate], expected_len: usize) {
    let in_pred = predicates
        .iter()
        .find_map(|p| match p {
            Predicate::In(ip) if ip.property == RESOURCE_ID => Some(ip),
            _ => None,
        })
        .expect("expected an In(id, ...) predicate");
    assert_eq!(in_pred.values.len(), expected_len, "id In-list length");
}

/// Assert a (group) constraint AND-pairs an `owner_tenant_id` predicate
/// (`Eq` or `In`) carrying exactly `expected` — the `RESOURCE_GROUP_MODEL.md`
/// "tenant constraint always applies alongside group predicates" invariant.
///
/// The VALUE check is the point: on the `Combined` path the pairing must carry
/// the GROUP's owning tenant, not the request's tenant. Matching on the property
/// name alone would accept a constraint paired with the requesting tenant — the
/// exact cross-tenant leak this pairing exists to prevent.
fn assert_tenant_paired(predicates: &[Predicate], expected: &[Uuid]) {
    let paired = predicates.iter().any(|p| match p {
        // Mirrors `tenant_predicate`: Eq for one tenant, In for several.
        Predicate::Eq(e) => {
            e.property == OWNER_TENANT_ID
                && expected.len() == 1
                && e.value == serde_json::json!(expected[0])
        }
        Predicate::In(i) => {
            i.property == OWNER_TENANT_ID
                && i.values.len() == expected.len()
                && expected
                    .iter()
                    .all(|t| i.values.contains(&serde_json::json!(t)))
        }
        _ => false,
    });
    assert!(
        paired,
        "group constraint must AND-pair owner_tenant_id = {expected:?} (got {predicates:?})"
    );
}

/// Pull the single group constraint's predicate list, asserting it is the
/// tenant-paired two-predicate shape.
fn group_constraint_predicates(constraints: Vec<Constraint>) -> Vec<Predicate> {
    assert_eq!(
        constraints.len(),
        1,
        "expected exactly one group constraint"
    );
    let predicates = constraints.into_iter().next().unwrap().predicates;
    assert_eq!(
        predicates.len(),
        2,
        "group constraint must AND id + owner_tenant_id"
    );
    predicates
}

// -- Tenant variants -------------------------------------------------

#[test]
fn u_22_tenant_direct_emits_eq() {
    let id = Uuid::from_u128(0xABCD);
    let request = request_with(vec!["owner_tenant_id"]);
    let outcome = generate_constraints(
        &Materialization::TenantDirect { tenant_id: id },
        &request,
        &default_cfg(),
    );
    let predicate = single_predicate(unwrap_allow(outcome));
    match predicate {
        Predicate::Eq(eq) => {
            assert_eq!(eq.property, OWNER_TENANT_ID);
            assert_eq!(eq.value, serde_json::json!(id));
        }
        other => panic!("expected Predicate::Eq, got {other:?}"),
    }
}

#[test]
fn empty_tenant_subtree_fails_closed() {
    // A TenantSubtree that materialized to zero tenants (e.g. the granted root
    // was non-active and excluded by the status filter, with no matching
    // descendants) must DENY, not emit an empty In(owner_tenant_id, []).
    let request = request_with(vec!["owner_tenant_id"]);
    let outcome = generate_constraints(
        &Materialization::TenantSubtree { tenant_ids: vec![] },
        &request,
        &default_cfg(),
    );
    let response = unwrap_deny(outcome);
    assert_eq!(
        response.context.deny_reason.unwrap().error_code,
        INSUFFICIENT_PERMISSIONS_V1
    );
}

#[test]
fn u_21_tenant_subtree_emits_in_with_all_ids() {
    let ids = vec![Uuid::from_u128(1), Uuid::from_u128(2), Uuid::from_u128(3)];
    let request = request_with(vec!["owner_tenant_id"]);
    let outcome = generate_constraints(
        &Materialization::TenantSubtree {
            tenant_ids: ids.clone(),
        },
        &request,
        &default_cfg(),
    );
    let predicate = single_predicate(unwrap_allow(outcome));
    match predicate {
        Predicate::In(in_pred) => {
            assert_eq!(in_pred.property, OWNER_TENANT_ID);
            assert_eq!(in_pred.values.len(), 3);
            for id in &ids {
                assert!(in_pred.values.contains(&serde_json::json!(id)));
            }
        }
        other => panic!("expected Predicate::In, got {other:?}"),
    }
}

#[test]
fn u_23_single_element_subtree_emits_eq() {
    // Per the tenant_predicate rule: len == 1 → Eq (not In).
    let id = Uuid::from_u128(42);
    let request = request_with(vec!["owner_tenant_id"]);
    let outcome = generate_constraints(
        &Materialization::TenantSubtree {
            tenant_ids: vec![id],
        },
        &request,
        &default_cfg(),
    );
    let predicate = single_predicate(unwrap_allow(outcome));
    match predicate {
        Predicate::Eq(eq) => assert_eq!(eq.value, serde_json::json!(id)),
        other => panic!("expected Predicate::Eq for single-tenant subtree, got {other:?}"),
    }
}

// -- TenantSubtreePushdown (capability-driven InTenantSubtree, #12) ---

fn pushdown(root: Uuid, status: Vec<TenantStatus>) -> Materialization {
    Materialization::TenantSubtreePushdown {
        root_tenant_id: root,
        barrier_mode: BarrierMode::Respect,
        status,
    }
}

fn in_tenant_subtree_preds(constraints: &[Constraint]) -> Vec<&InTenantSubtreePredicate> {
    constraints
        .iter()
        .flat_map(|c| &c.predicates)
        .filter_map(|p| match p {
            Predicate::InTenantSubtree(p) => Some(p),
            _ => None,
        })
        .collect()
}

#[test]
fn pushdown_emits_in_tenant_subtree_on_both_supported_properties() {
    let root = Uuid::from_u128(0xFEED);
    let request = request_with(vec!["owner_tenant_id", "id"]);
    let constraints = unwrap_allow(generate_constraints(
        &pushdown(root, vec![TenantStatus::Active]),
        &request,
        &default_cfg(),
    ));
    // One constraint per supported tenant-shaped property, OR-combined.
    assert_eq!(
        constraints.len(),
        2,
        "expected one InTenantSubtree per supported property"
    );
    let preds = in_tenant_subtree_preds(&constraints);
    let props: Vec<&str> = preds.iter().map(|p| p.property.as_str()).collect();
    assert!(props.contains(&OWNER_TENANT_ID));
    assert!(props.contains(&RESOURCE_ID));
    for p in &preds {
        assert_eq!(p.root_tenant_id, serde_json::json!(root));
    }
}

#[test]
fn pushdown_only_owner_tenant_id_supported_emits_single_predicate() {
    let request = request_with(vec!["owner_tenant_id"]);
    let constraints = unwrap_allow(generate_constraints(
        &pushdown(Uuid::from_u128(1), vec![TenantStatus::Active]),
        &request,
        &default_cfg(),
    ));
    match single_predicate(constraints) {
        Predicate::InTenantSubtree(p) => assert_eq!(p.property, OWNER_TENANT_ID),
        other => panic!("expected InTenantSubtree, got {other:?}"),
    }
}

#[test]
fn pushdown_only_resource_id_supported_binds_no_tenant_entity() {
    // The no_tenant entity case (e.g. AM `tenants`, scoped by its own `id`):
    // the eager path emits only owner_tenant_id and gets dropped by SecureORM;
    // push-down binds InTenantSubtree(id, root) instead.
    let request = request_with(vec!["id"]);
    let constraints = unwrap_allow(generate_constraints(
        &pushdown(Uuid::from_u128(2), vec![TenantStatus::Active]),
        &request,
        &default_cfg(),
    ));
    match single_predicate(constraints) {
        Predicate::InTenantSubtree(p) => assert_eq!(p.property, RESOURCE_ID),
        other => panic!("expected InTenantSubtree, got {other:?}"),
    }
}

#[test]
fn pushdown_no_supported_tenant_property_fails_closed() {
    // Resolver already skipped — no eager fallback, so deny rather than allow-all.
    let request = request_with(vec!["something_else"]);
    let response = unwrap_deny(generate_constraints(
        &pushdown(Uuid::from_u128(3), vec![TenantStatus::Active]),
        &request,
        &default_cfg(),
    ));
    assert_eq!(
        response.context.deny_reason.unwrap().error_code,
        INSUFFICIENT_PERMISSIONS_V1
    );
}

#[test]
fn pushdown_exempt_from_expansion_threshold() {
    // No ID list to bound — even max_expansion_ids=0 must not deny.
    let request = request_with(vec!["owner_tenant_id"]);
    let outcome = generate_constraints(
        &pushdown(Uuid::from_u128(4), vec![TenantStatus::Active]),
        &request,
        &cfg_with_max(0),
    );
    assert!(matches!(outcome, ConstraintOutcome::Allow(_)));
}

#[test]
fn pushdown_forwards_barrier_mode_and_status() {
    let request = request_with(vec!["owner_tenant_id"]);
    let materialization = Materialization::TenantSubtreePushdown {
        root_tenant_id: Uuid::from_u128(5),
        barrier_mode: BarrierMode::Ignore,
        status: vec![TenantStatus::Active, TenantStatus::Suspended],
    };
    let constraints = unwrap_allow(generate_constraints(
        &materialization,
        &request,
        &default_cfg(),
    ));
    match single_predicate(constraints) {
        Predicate::InTenantSubtree(p) => {
            assert_eq!(p.barrier_mode, BarrierMode::Ignore);
            assert_eq!(
                p.descendant_status,
                vec![TenantStatus::Active, TenantStatus::Suspended]
            );
        }
        other => panic!("expected InTenantSubtree, got {other:?}"),
    }
}

// -- GroupSubtree (group always uses In) -----------------------------

#[test]
fn u_25_group_subtree_emits_in_on_id_paired_with_tenant() {
    let ids = vec![Uuid::from_u128(1), Uuid::from_u128(2), Uuid::from_u128(3)];
    let tenant = Uuid::from_u128(0xAA);
    // owner_tenant_id MUST be supported now — group constraints are tenant-paired.
    let request = request_with(vec!["id", "owner_tenant_id"]);
    let outcome = generate_constraints(
        &Materialization::GroupSubtree {
            resource_ids: ids.clone(),
            owner_tenant_ids: vec![tenant],
        },
        &request,
        &default_cfg(),
    );
    let predicates = group_constraint_predicates(unwrap_allow(outcome));
    assert_in_on_id(&predicates, 3);
    assert_tenant_paired(&predicates, &[tenant]);
    // The id list still carries every resource id.
    let in_pred = predicates
        .iter()
        .find_map(|p| match p {
            Predicate::In(ip) if ip.property == RESOURCE_ID => Some(ip),
            _ => None,
        })
        .unwrap();
    for id in &ids {
        assert!(in_pred.values.contains(&serde_json::json!(id)));
    }
}

#[test]
fn empty_group_subtree_fails_closed() {
    // A GroupSubtree whose group has zero member resources must DENY, not emit
    // an empty In(id, []). The PEP compiler rejects an empty-In value list as a
    // contract violation and returns Internal/500, so an empty member set must
    // fail closed here (the grant covers no resources) rather than reach it.
    let tenant = Uuid::from_u128(0xAA);
    let request = request_with(vec!["id", "owner_tenant_id"]);
    let outcome = generate_constraints(
        &Materialization::GroupSubtree {
            resource_ids: vec![],
            owner_tenant_ids: vec![tenant],
        },
        &request,
        &default_cfg(),
    );
    let response = unwrap_deny(outcome);
    assert_eq!(
        response.context.deny_reason.unwrap().error_code,
        INSUFFICIENT_PERMISSIONS_V1
    );
}

#[test]
fn single_resource_group_subtree_still_emits_in_paired_with_tenant() {
    let id = Uuid::from_u128(7);
    let tenant = Uuid::from_u128(0xBB);
    let request = request_with(vec!["id", "owner_tenant_id"]);
    let outcome = generate_constraints(
        &Materialization::GroupSubtree {
            resource_ids: vec![id],
            owner_tenant_ids: vec![tenant],
        },
        &request,
        &default_cfg(),
    );
    let predicates = group_constraint_predicates(unwrap_allow(outcome));
    assert_in_on_id(&predicates, 1);
    assert_tenant_paired(&predicates, &[tenant]);
}

#[test]
fn group_subtree_without_owning_tenant_fails_closed() {
    // Defense-in-depth: a group materialization that somehow reaches the
    // generator with no owning tenant must NOT emit a tenant-less group
    // constraint — it fails closed with an insufficient_permissions deny.
    let request = request_with(vec!["id", "owner_tenant_id"]);
    let outcome = generate_constraints(
        &Materialization::GroupSubtree {
            resource_ids: vec![Uuid::from_u128(1)],
            owner_tenant_ids: vec![],
        },
        &request,
        &default_cfg(),
    );
    let response = unwrap_deny(outcome);
    assert_eq!(
        response.context.deny_reason.unwrap().error_code,
        INSUFFICIENT_PERMISSIONS_V1
    );
}

#[test]
fn group_subtree_requires_owner_tenant_id_in_supported_properties() {
    // A PEP that only supports "id" (not "owner_tenant_id") cannot receive a
    // group-scope allow: the tenant-paired predicate is mandatory, so the
    // decision fails closed with unsupported_property.
    let request = request_with(vec!["id"]);
    let outcome = generate_constraints(
        &Materialization::GroupSubtree {
            resource_ids: vec![Uuid::from_u128(1)],
            owner_tenant_ids: vec![Uuid::from_u128(0xCC)],
        },
        &request,
        &default_cfg(),
    );
    let response = unwrap_deny(outcome);
    assert_eq!(
        response.context.deny_reason.unwrap().error_code,
        UNSUPPORTED_PROPERTY_V1
    );
}

// -- Combined variants -----------------------------------------------

#[test]
fn u_27_combined_with_one_tenant_and_resources_emits_eq_plus_in() {
    let t1 = Uuid::from_u128(0xA);
    let group_tenant = Uuid::from_u128(0xB);
    let resources = vec![Uuid::from_u128(1), Uuid::from_u128(2)];
    let request = request_with(vec!["owner_tenant_id", "id"]);
    let outcome = generate_constraints(
        &Materialization::Combined {
            tenant_ids: vec![t1],
            resource_ids: resources,
            group_owner_tenant_ids: vec![group_tenant],
        },
        &request,
        &default_cfg(),
    );
    let constraints = unwrap_allow(outcome);
    assert_eq!(constraints.len(), 2);
    // First constraint: the OR'd tenant side — Eq(owner_tenant_id, T1).
    match &constraints[0].predicates[0] {
        Predicate::Eq(eq) => {
            assert_eq!(eq.property, OWNER_TENANT_ID);
            assert_eq!(eq.value, serde_json::json!(t1));
        }
        other => panic!("expected Eq on tenant, got {other:?}"),
    }
    // Second constraint: the tenant-PAIRED group side — In(id, [res…]) AND a
    // tenant predicate for the GROUP's owning tenant (not t1).
    let group_predicates = &constraints[1].predicates;
    assert_eq!(
        group_predicates.len(),
        2,
        "group side must be tenant-paired"
    );
    assert_in_on_id(group_predicates, 2);
    assert_tenant_paired(group_predicates, &[group_tenant]);
}

#[test]
fn combined_multi_tenant_uses_in_on_owner_tenant_id() {
    let request = request_with(vec!["owner_tenant_id", "id"]);
    let outcome = generate_constraints(
        &Materialization::Combined {
            tenant_ids: vec![Uuid::from_u128(1), Uuid::from_u128(2)],
            resource_ids: vec![Uuid::from_u128(3)],
            group_owner_tenant_ids: vec![Uuid::from_u128(9)],
        },
        &request,
        &default_cfg(),
    );
    let constraints = unwrap_allow(outcome);
    assert_eq!(constraints.len(), 2);
    assert!(
        matches!(&constraints[0].predicates[0], Predicate::In(p) if p.property == OWNER_TENANT_ID)
    );
    // Group side stays tenant-paired.
    assert_tenant_paired(&constraints[1].predicates, &[Uuid::from_u128(9)]);
}

#[test]
fn combined_only_tenants_emits_single_tenant_constraint() {
    let request = request_with(vec!["owner_tenant_id"]);
    let outcome = generate_constraints(
        &Materialization::Combined {
            tenant_ids: vec![Uuid::from_u128(1), Uuid::from_u128(2)],
            resource_ids: vec![],
            group_owner_tenant_ids: vec![],
        },
        &request,
        &default_cfg(),
    );
    let constraints = unwrap_allow(outcome);
    assert_eq!(constraints.len(), 1);
    assert!(
        matches!(&constraints[0].predicates[0], Predicate::In(p) if p.property == OWNER_TENANT_ID)
    );
}

#[test]
fn combined_only_resources_emits_single_tenant_paired_group_constraint() {
    let request = request_with(vec!["id", "owner_tenant_id"]);
    let outcome = generate_constraints(
        &Materialization::Combined {
            tenant_ids: vec![],
            resource_ids: vec![Uuid::from_u128(1)],
            group_owner_tenant_ids: vec![Uuid::from_u128(0xD)],
        },
        &request,
        &default_cfg(),
    );
    let predicates = group_constraint_predicates(unwrap_allow(outcome));
    assert_in_on_id(&predicates, 1);
    assert_tenant_paired(&predicates, &[Uuid::from_u128(0xD)]);
}

#[test]
fn combined_both_empty_denies_constraints_unavailable() {
    let request = request_with(vec!["owner_tenant_id", "id"]);
    let outcome = generate_constraints(
        &Materialization::Combined {
            tenant_ids: vec![],
            resource_ids: vec![],
            group_owner_tenant_ids: vec![],
        },
        &request,
        &default_cfg(),
    );
    let response = unwrap_deny(outcome);
    assert!(!response.decision);
    assert_eq!(
        response
            .context
            .deny_reason
            .expect("deny reason")
            .error_code,
        CONSTRAINTS_UNAVAILABLE_V1
    );
}

// -- Materialization::Denied dispatch --------------------------------

#[test]
fn u_26_denied_dispatches_to_deny_response() {
    // Even with an empty supported_properties (which would otherwise
    // deny), Denied short-circuits first.
    let request = request_with(vec![]);
    let outcome = generate_constraints(
        &Materialization::Denied {
            error_code: INSUFFICIENT_PERMISSIONS_V1,
            details: Some("rbac returned reserved scope variant: TenantDirect".to_owned()),
        },
        &request,
        &default_cfg(),
    );
    let response = unwrap_deny(outcome);
    let reason = response.context.deny_reason.expect("populated");
    assert_eq!(reason.error_code, INSUFFICIENT_PERMISSIONS_V1);
    assert!(response.context.constraints.is_empty());
}

#[test]
fn denied_short_circuits_before_validation_or_threshold() {
    // Denied takes precedence: even with a tiny max_expansion_ids and
    // empty supported_properties, Denied returns its own error_code.
    let request = request_with(vec![]);
    let outcome = generate_constraints(
        &Materialization::Denied {
            error_code: INSUFFICIENT_PERMISSIONS_V1,
            details: None,
        },
        &request,
        &cfg_with_max(0),
    );
    let response = unwrap_deny(outcome);
    let reason = response.context.deny_reason.expect("populated");
    assert_eq!(reason.error_code, INSUFFICIENT_PERMISSIONS_V1);
}

// -- supported_properties validation ---------------------------------

#[test]
fn u_28_all_supported_properties_allow_passes() {
    let request = request_with(vec!["owner_tenant_id", "id"]);
    let outcome = generate_constraints(
        &Materialization::TenantSubtree {
            tenant_ids: vec![Uuid::from_u128(1), Uuid::from_u128(2)],
        },
        &request,
        &default_cfg(),
    );
    assert!(matches!(outcome, ConstraintOutcome::Allow(_)));
}

#[test]
fn u_29_missing_owner_tenant_id_denies_with_unsupported_property() {
    let request = request_with(vec!["id"]);
    let outcome = generate_constraints(
        &Materialization::TenantSubtree {
            tenant_ids: vec![Uuid::from_u128(1), Uuid::from_u128(2)],
        },
        &request,
        &default_cfg(),
    );
    let response = unwrap_deny(outcome);
    let reason = response.context.deny_reason.expect("populated");
    assert_eq!(reason.error_code, UNSUPPORTED_PROPERTY_V1);
}

#[test]
fn empty_supported_properties_denies_any_predicate() {
    let request = request_with(vec![]);
    let outcome = generate_constraints(
        &Materialization::TenantDirect {
            tenant_id: Uuid::from_u128(1),
        },
        &request,
        &default_cfg(),
    );
    let response = unwrap_deny(outcome);
    let reason = response.context.deny_reason.expect("populated");
    assert_eq!(reason.error_code, UNSUPPORTED_PROPERTY_V1);
}

// -- expansion threshold ---------------------------------------------

#[test]
fn u_30_tenant_subtree_over_threshold_denies() {
    let ids: Vec<Uuid> = (0..10_001_u128).map(Uuid::from_u128).collect();
    let request = request_with(vec!["owner_tenant_id"]);
    let outcome = generate_constraints(
        &Materialization::TenantSubtree { tenant_ids: ids },
        &request,
        &default_cfg(), // default = 10_000
    );
    let response = unwrap_deny(outcome);
    let reason = response.context.deny_reason.expect("populated");
    assert_eq!(reason.error_code, EXPANSION_INFEASIBLE_V1);
}

#[test]
fn at_threshold_tenant_subtree_passes() {
    let ids: Vec<Uuid> = (0..10_000_u128).map(Uuid::from_u128).collect();
    let request = request_with(vec!["owner_tenant_id"]);
    let outcome = generate_constraints(
        &Materialization::TenantSubtree { tenant_ids: ids },
        &request,
        &default_cfg(),
    );
    assert!(matches!(outcome, ConstraintOutcome::Allow(_)));
}

#[test]
fn group_subtree_over_threshold_denies() {
    let ids: Vec<Uuid> = (0..10_001_u128).map(Uuid::from_u128).collect();
    let request = request_with(vec!["id"]);
    let outcome = generate_constraints(
        &Materialization::GroupSubtree {
            resource_ids: ids,
            owner_tenant_ids: vec![Uuid::from_u128(0xEE)],
        },
        &request,
        &default_cfg(),
    );
    let response = unwrap_deny(outcome);
    assert_eq!(
        response.context.deny_reason.unwrap().error_code,
        EXPANSION_INFEASIBLE_V1
    );
}

#[test]
fn combined_tenant_side_over_threshold_denies() {
    let ids: Vec<Uuid> = (0..10_001_u128).map(Uuid::from_u128).collect();
    let request = request_with(vec!["owner_tenant_id", "id"]);
    let outcome = generate_constraints(
        &Materialization::Combined {
            tenant_ids: ids,
            resource_ids: vec![],
            group_owner_tenant_ids: vec![],
        },
        &request,
        &default_cfg(),
    );
    let response = unwrap_deny(outcome);
    assert_eq!(
        response.context.deny_reason.unwrap().error_code,
        EXPANSION_INFEASIBLE_V1
    );
}

#[test]
fn combined_resource_side_over_threshold_denies() {
    let ids: Vec<Uuid> = (0..10_001_u128).map(Uuid::from_u128).collect();
    let request = request_with(vec!["owner_tenant_id", "id"]);
    let outcome = generate_constraints(
        &Materialization::Combined {
            tenant_ids: vec![],
            resource_ids: ids,
            group_owner_tenant_ids: vec![Uuid::from_u128(0xEE)],
        },
        &request,
        &default_cfg(),
    );
    let response = unwrap_deny(outcome);
    assert_eq!(
        response.context.deny_reason.unwrap().error_code,
        EXPANSION_INFEASIBLE_V1
    );
}

#[test]
fn operator_overridden_threshold_honored() {
    let ids: Vec<Uuid> = (0..5_001_u128).map(Uuid::from_u128).collect();
    let request = request_with(vec!["owner_tenant_id"]);
    let outcome = generate_constraints(
        &Materialization::TenantSubtree { tenant_ids: ids },
        &request,
        &cfg_with_max(5_000),
    );
    let response = unwrap_deny(outcome);
    assert_eq!(
        response.context.deny_reason.unwrap().error_code,
        EXPANSION_INFEASIBLE_V1
    );
}

#[test]
fn expansion_takes_precedence_over_unsupported_property() {
    // When both conditions could fire, expansion_infeasible wins.
    let ids: Vec<Uuid> = (0..10_001_u128).map(Uuid::from_u128).collect();
    let request = request_with(vec![]); // PEP supports no properties either
    let outcome = generate_constraints(
        &Materialization::TenantSubtree { tenant_ids: ids },
        &request,
        &default_cfg(),
    );
    let response = unwrap_deny(outcome);
    assert_eq!(
        response.context.deny_reason.unwrap().error_code,
        EXPANSION_INFEASIBLE_V1,
        "expansion deny must surface over unsupported_property"
    );
}

// -- Property name constants -----------------------------------------

#[test]
fn property_name_constants() {
    assert_eq!(OWNER_TENANT_ID, "owner_tenant_id");
    assert_eq!(RESOURCE_ID, "id");
}
