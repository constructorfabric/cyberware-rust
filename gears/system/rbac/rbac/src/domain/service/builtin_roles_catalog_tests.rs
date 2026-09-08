// Test-only: domain → infra reach is intentional for the test fixture so
// the JSON helper from the seeder can be reused; production code paths
// obey the layer split.
#![allow(unknown_lints, de0301_no_infra_in_domain)]

use crate::infra::seeder::role_permissions_json;

use super::*;

/// The built-in targets a deployment gets without configuring anything.
fn default_targets() -> crate::config::BuiltinRoleTargets {
    crate::config::BuiltinRoleTargets::default()
}

#[test]
fn catalog_is_sorted_by_ascending_id() {
    // Lock-ordering invariant the seeder relies on. Tested as a unit
    // test so it fires in release builds and CI.
    for pair in CANONICAL_BUILTIN_ROLES.windows(2) {
        assert!(
            pair[0].id < pair[1].id,
            "CANONICAL_BUILTIN_ROLES MUST be sorted ascending by id; \
             violation at {} (id {}) → {} (id {})",
            pair[0].name,
            pair[0].id,
            pair[1].name,
            pair[1].id,
        );
    }
}

#[test]
fn catalog_ids_match_design_table() {
    // Pin every canonical built-in UUID. Changing any of these is a hard
    // cross-deployment break.
    let expected: [(&str, Uuid); 6] = [
        (
            "Owner",
            Uuid::from_u128(0x0195_f2b6_0001_7000_8000_0000_0000_0001_u128),
        ),
        (
            "Contributor",
            Uuid::from_u128(0x0195_f2b6_0002_7000_8000_0000_0000_0002_u128),
        ),
        (
            "Reader",
            Uuid::from_u128(0x0195_f2b6_0003_7000_8000_0000_0000_0003_u128),
        ),
        (
            "User Access Administrator",
            Uuid::from_u128(0x0195_f2b6_0004_7000_8000_0000_0000_0004_u128),
        ),
        (
            "Credstore Secret Operator",
            Uuid::from_u128(0x0195_f2b6_0005_7000_8000_0000_0000_0005_u128),
        ),
        (
            "Usage Emitter",
            Uuid::from_u128(0x0195_f2b6_0006_7000_8000_0000_0000_0006_u128),
        ),
    ];
    // `zip` truncates to the shorter side, so without this a roster entry
    // missing from `expected` would silently go unchecked.
    assert_eq!(
        expected.len(),
        CANONICAL_BUILTIN_ROLES.len(),
        "every canonical built-in MUST be pinned here; pinned {} vs catalog {}",
        expected.len(),
        CANONICAL_BUILTIN_ROLES.len(),
    );
    for ((expected_name, expected_id), role) in expected.iter().zip(CANONICAL_BUILTIN_ROLES.iter())
    {
        assert_eq!(role.name, *expected_name);
        assert_eq!(role.id, *expected_id);
    }
}

#[test]
fn user_access_administrator_has_two_permission_rules() {
    let user_access_admin = CANONICAL_BUILTIN_ROLES
        .iter()
        .find(|r| r.name == "User Access Administrator")
        .expect("User Access Administrator MUST be in the canonical catalog");
    assert_eq!(user_access_admin.permission_rules.len(), 2);
}

/// Asserts the full per-role details (`permission_rules` target types +
/// operations, and `assignable_scopes`) for every canonical built-in.
/// Changing any value here is a hard cross-deployment break.
#[allow(clippy::panic)]
#[test]
// One assertion per (role x field) of the normative roster: the arm count
// IS the coverage. Splitting it per role would hide which field drifted.
#[allow(clippy::cognitive_complexity)]
fn roster_matches_normative_spec() {
    // These consts deliberately re-state the production constants rather
    // than referencing them so a silent rename triggers a test failure.
    const OWNER_OP: &str = "*";
    const OWNER_TARGET_WILDCARD: &str = "gts.cf.*";

    const CONTRIB_OP: &str = "*";
    const CONTRIB_TARGET_WILDCARD: &str = "gts.cf.resources.*";

    const READER_OP: &str = "read";
    const READER_TARGET_WILDCARD: &str = "gts.cf.resources.*";

    const UAA_ASSIGNMENT_OP: &str = "*";
    const UAA_ASSIGNMENT_TARGET: &str = "gts.cf.core.rbac.role_assignment.v1~";
    const UAA_DEFINITION_OP: &str = "read";
    const UAA_DEFINITION_TARGET: &str = "gts.cf.core.rbac.role_definition.v1~";

    const CREDSTORE_OP_READ: &str = "read";
    const CREDSTORE_OP_WRITE: &str = "write";
    const CREDSTORE_OP_DELETE: &str = "delete";
    const CREDSTORE_TARGET: &str = "gts.cf.core.credstore.secret.v1~";

    const USAGE_EMITTER_RECORD_OP: &str = "create";
    const USAGE_EMITTER_RECORD_TARGET: &str = "gts.cf.core.uc.usage_record.v1~";
    const USAGE_EMITTER_TYPE_OP: &str = "read";
    const USAGE_EMITTER_TYPE_TARGET: &str = "gts.cf.core.uc.usage_type.v1~";

    const ROOT_SCOPE: &str = "/";

    let [owner, contributor, reader, uaa, credstore_op, usage_emitter] = CANONICAL_BUILTIN_ROLES
    else {
        panic!("expected exactly 6 canonical built-in roles");
    };
    let targets = crate::config::BuiltinRoleTargets::default();

    assert_eq!(owner.name, "Owner");
    // One vendor wildcard, deliberately. A second rule appearing here means the
    // house-vendor grant came back. Targets are asserted as the DEFAULT config
    // resolves them, so the normative values stay pinned even though the
    // catalog stores slots.
    assert_eq!(owner.permission_rules.len(), 1);
    assert_eq!(owner.permission_rules[0].operation, OWNER_OP);
    assert_eq!(
        targets.resolve(owner.permission_rules[0].target),
        vec![OWNER_TARGET_WILDCARD]
    );
    assert_eq!(owner.assignable_scopes, &[ROOT_SCOPE]);

    assert_eq!(contributor.name, "Contributor");
    assert_eq!(contributor.permission_rules.len(), 1);
    assert_eq!(contributor.permission_rules[0].operation, CONTRIB_OP);
    assert_eq!(
        targets.resolve(contributor.permission_rules[0].target),
        vec![CONTRIB_TARGET_WILDCARD]
    );
    assert_eq!(contributor.assignable_scopes, &[ROOT_SCOPE]);

    assert_eq!(reader.name, "Reader");
    assert_eq!(reader.permission_rules.len(), 1);
    assert_eq!(reader.permission_rules[0].operation, READER_OP);
    assert_eq!(
        targets.resolve(reader.permission_rules[0].target),
        vec![READER_TARGET_WILDCARD]
    );
    assert_eq!(reader.assignable_scopes, &[ROOT_SCOPE]);

    assert_eq!(uaa.name, "User Access Administrator");
    assert_eq!(uaa.permission_rules.len(), 2);
    assert_eq!(uaa.permission_rules[0].operation, UAA_ASSIGNMENT_OP);
    assert_eq!(
        targets.resolve(uaa.permission_rules[0].target),
        vec![UAA_ASSIGNMENT_TARGET]
    );
    assert_eq!(uaa.permission_rules[1].operation, UAA_DEFINITION_OP);
    assert_eq!(
        targets.resolve(uaa.permission_rules[1].target),
        vec![UAA_DEFINITION_TARGET]
    );
    assert_eq!(uaa.assignable_scopes, &[ROOT_SCOPE]);

    assert_eq!(credstore_op.name, "Credstore Secret Operator");
    assert_eq!(credstore_op.permission_rules.len(), 3);
    assert_eq!(
        credstore_op.permission_rules[0].operation,
        CREDSTORE_OP_READ
    );
    assert_eq!(
        targets.resolve(credstore_op.permission_rules[0].target),
        vec![CREDSTORE_TARGET]
    );
    assert_eq!(
        credstore_op.permission_rules[1].operation,
        CREDSTORE_OP_WRITE
    );
    assert_eq!(
        targets.resolve(credstore_op.permission_rules[1].target),
        vec![CREDSTORE_TARGET]
    );
    assert_eq!(
        credstore_op.permission_rules[2].operation,
        CREDSTORE_OP_DELETE
    );
    assert_eq!(
        targets.resolve(credstore_op.permission_rules[2].target),
        vec![CREDSTORE_TARGET]
    );
    assert_eq!(credstore_op.assignable_scopes, &[ROOT_SCOPE]);

    assert_eq!(usage_emitter.name, "Usage Emitter");
    assert_eq!(usage_emitter.permission_rules.len(), 2);
    assert_eq!(
        usage_emitter.permission_rules[0].operation,
        USAGE_EMITTER_RECORD_OP
    );
    assert_eq!(
        targets.resolve(usage_emitter.permission_rules[0].target),
        vec![USAGE_EMITTER_RECORD_TARGET]
    );
    assert_eq!(
        usage_emitter.permission_rules[1].operation,
        USAGE_EMITTER_TYPE_OP
    );
    assert_eq!(
        targets.resolve(usage_emitter.permission_rules[1].target),
        vec![USAGE_EMITTER_TYPE_TARGET]
    );
    assert_eq!(usage_emitter.assignable_scopes, &[ROOT_SCOPE]);
}

/// Asserts that the JSONB array produced by [`role_permissions_json`] for the
/// User Access Administrator role has element order
/// `[{role_assignment.v1~ "*"}, {role_definition.v1~ "read"}]` so two
/// independent deployments produce byte-equivalent permission payloads.
#[test]
fn permission_payload_ordering_is_deterministic() {
    let uaa = CANONICAL_BUILTIN_ROLES
        .iter()
        .find(|r| r.name == "User Access Administrator")
        .expect("User Access Administrator MUST be in the canonical catalog");

    let payload = role_permissions_json(uaa, &default_targets());
    let arr = payload
        .as_array()
        .expect("permissions payload MUST be a JSON array");

    assert_eq!(
        arr.len(),
        2,
        "User Access Administrator MUST have exactly 2 permission rules"
    );

    // Rule 0: assignment wildcard (must come first — matches declaration order)
    assert_eq!(arr[0]["operation"], "*", "rule 0 operation");
    assert_eq!(
        arr[0]["target_type"], "gts.cf.core.rbac.role_assignment.v1~",
        "rule 0 target_type"
    );

    assert_eq!(arr[1]["operation"], "read", "rule 1 operation");
    assert_eq!(
        arr[1]["target_type"], "gts.cf.core.rbac.role_definition.v1~",
        "rule 1 target_type"
    );
}

#[test]
fn seeder_walk_order_is_strictly_ascending_by_id() {
    // The seeder walks `CANONICAL_BUILTIN_ROLES` in slice order, so the
    // slice MUST be ascending by id to keep the lock-acquisition order
    // consistent across concurrent seeders.
    let walk_ids: Vec<Uuid> = CANONICAL_BUILTIN_ROLES.iter().map(|r| r.id).collect();
    let mut sorted = walk_ids.clone();
    sorted.sort();
    assert_eq!(
        walk_ids, sorted,
        "the seeder walks the catalog in slice order; the slice MUST be \
         ascending by id so concurrent seeders take row locks in the \
         same sequence"
    );
}
