use super::*;
use sea_orm::ActiveValue;

use crate::domain::builtin_roles_catalog::CANONICAL_BUILTIN_ROLES;

/// The built-in targets a deployment gets without configuring anything.
fn default_targets() -> crate::config::BuiltinRoleTargets {
    crate::config::BuiltinRoleTargets::default()
}

fn fixed_now() -> chrono::DateTime<Utc> {
    chrono::DateTime::<Utc>::UNIX_EPOCH
}

/// Read a `Set(_)`/`Unchanged(_)` value or panic if the column is `NotSet`.
macro_rules! read_set {
    ($field:expr) => {
        match &$field {
            ActiveValue::Set(v) | ActiveValue::Unchanged(v) => v.clone(),
            ActiveValue::NotSet => {
                panic!("active value MUST be Set: {}", stringify!($field))
            }
        }
    };
}

#[test]
fn permissions_json_serialises_to_design_shape() {
    let owner = &CANONICAL_BUILTIN_ROLES[0];
    let permissions = role_permissions_json(owner, &default_targets());
    let expected = serde_json::json!([
        { "operation": "*", "target_type": "gts.cf.*" }
    ]);
    assert_eq!(permissions, expected);
}

#[test]
fn assignable_scopes_json_is_a_string_array() {
    for role in CANONICAL_BUILTIN_ROLES {
        let scopes = role_assignable_scopes_json(role);
        assert_eq!(scopes, serde_json::json!(["/"]));
    }
}

#[test]
// Asserts every column of the seeded ActiveModel for every canonical role;
// the branch count is the invariant being pinned, not incidental logic.
#[allow(clippy::cognitive_complexity)]
fn seeder_constructs_payload_with_built_in_invariants_for_every_role() {
    let now = fixed_now();
    for role in CANONICAL_BUILTIN_ROLES {
        let am = build_role_definition_active_model(role, now, &default_targets());
        assert_eq!(read_set!(am.id), role.id);
        assert_eq!(read_set!(am.name), role.name);
        assert_eq!(read_set!(am.description), Some(role.description.to_owned()));
        assert!(
            read_set!(am.is_built_in),
            "every built-in MUST carry is_built_in = true"
        );
        assert_eq!(
            read_set!(am.owner_tenant_id),
            None,
            "built-ins MUST have NULL owner_tenant_id (DB CHECK \
             constraint)"
        );
        assert_eq!(
            read_set!(am.not_permissions),
            serde_json::json!([]),
            "v1 built-ins have no subtractive rules (storage keeps two \
             JSONB columns; the in-memory wire shape unifies them under \
             `rules`)"
        );
        assert_eq!(
            read_set!(am.assignable_scopes),
            serde_json::json!(["/"]),
            "v1 built-ins are platform-wide"
        );
        assert_eq!(read_set!(am.created_by), SYSTEM_CREATED_BY);
        assert_eq!(read_set!(am.created_at), now);
        assert_eq!(read_set!(am.updated_at), now);
    }
}

#[test]
fn seeder_payload_permissions_match_design_table_per_role() {
    let now = fixed_now();
    let expected: [(&str, JsonValue); 4] = [
        (
            "Owner",
            serde_json::json!([
                { "operation": "*", "target_type": "gts.cf.*" }
            ]),
        ),
        (
            "Contributor",
            serde_json::json!([
                { "operation": "*", "target_type": "gts.cf.resources.*" }
            ]),
        ),
        (
            "Reader",
            serde_json::json!([
                { "operation": "read", "target_type": "gts.cf.resources.*" }
            ]),
        ),
        (
            "User Access Administrator",
            serde_json::json!([
                {
                    "operation": "*",
                    "target_type": "gts.cf.core.rbac.role_assignment.v1~"
                },
                {
                    "operation": "read",
                    "target_type": "gts.cf.core.rbac.role_definition.v1~"
                },
            ]),
        ),
    ];
    for (expected_name, expected_json) in expected {
        let role = CANONICAL_BUILTIN_ROLES
            .iter()
            .find(|r| r.name == expected_name)
            .expect("role must be in the canonical catalog");
        let am = build_role_definition_active_model(role, now, &default_targets());
        assert_eq!(read_set!(am.permissions), expected_json);
    }
}

/// Compile-time pin: `BuiltinRoleSeeder` MUST remain a ZST so it cannot
/// gain a permission-catalog field that would route the seeder through
/// the catalog and break the built-in exemption invariant.
#[test]
fn seeder_does_not_consult_permission_catalog() {
    assert_eq!(
        std::mem::size_of::<BuiltinRoleSeeder>(),
        0,
        "BuiltinRoleSeeder MUST remain a ZST \u{2014} adding state (such as an \
         `Arc<dyn PermissionCatalog>` field) would route the seeder \
         through the catalog and break the built-in exemption invariant \
         (built-in exemption)."
    );
    let _discarded = BuiltinRoleSeeder::new();
}

/// Smoke test on Owner: `permissions` is `[{operation, target_type}]`,
/// `assignable_scopes` is a non-empty string array. Locks the two JSONB
/// column shapes to the JSON Schema.
#[test]
fn seeder_payload_serialises_to_design_jsonb_column_shape() {
    let now = fixed_now();
    let owner = &CANONICAL_BUILTIN_ROLES[0];
    let am = build_role_definition_active_model(owner, now, &default_targets());

    let permissions = read_set!(am.permissions);
    let arr = permissions
        .as_array()
        .expect("permissions JSONB MUST be an array");
    // Owner grants exactly one vendor wildcard: gts.cf.*.
    assert_eq!(arr.len(), 1);
    for entry in arr {
        let rule = entry
            .as_object()
            .expect("permission rule MUST be an object");
        assert!(rule.contains_key("operation"));
        assert!(rule.contains_key("target_type"));
    }

    let scopes = read_set!(am.assignable_scopes);
    let arr = scopes
        .as_array()
        .expect("assignable_scopes JSONB MUST be an array");
    assert!(
        !arr.is_empty(),
        "assignable_scopes MUST be non-empty per DB CHECK"
    );
    assert!(arr.iter().all(serde_json::Value::is_string));
}

/// `role_count()` MUST be derived from the catalog, never a literal. This is
/// the guard that lets every integration test drop its hardcoded roster size:
/// if someone later replaces the body with a constant, this fails the moment
/// a role is added.
#[test]
fn role_count_agrees_with_the_canonical_catalog() {
    assert_eq!(
        BuiltinRoleSeeder::role_count(true),
        CANONICAL_BUILTIN_ROLES.len(),
        "role_count() MUST derive from CANONICAL_BUILTIN_ROLES, not a literal",
    );
}

/// `role_names()` MUST preserve catalog order (ascending `id` — the seeder's
/// lock-ordering invariant) and contain no duplicates. Duplicate built-in
/// names would violate the `uq_role_name_builtin` partial unique index at
/// seed time, so catching it here is cheaper than a Postgres round-trip.
#[test]
fn role_names_follow_catalog_order_and_are_unique() {
    let names = BuiltinRoleSeeder::role_names(true);
    let catalog: Vec<&str> = CANONICAL_BUILTIN_ROLES
        .iter()
        .map(|role| role.name)
        .collect();
    assert_eq!(
        names, catalog,
        "role_names() MUST preserve catalog (ascending-id) order",
    );

    let unique: std::collections::BTreeSet<&str> = names.iter().copied().collect();
    assert_eq!(
        unique.len(),
        names.len(),
        "built-in role names MUST be unique (uq_role_name_builtin); got {names:?}",
    );
}

// ---------------------------------------------------------------------------
// Integration-role gating
// ---------------------------------------------------------------------------

/// With integration roles off the seeder upserts only the core roster, and
/// none of the roles it seeds targets another gear's resource types.
#[test]
fn core_roster_excludes_integration_roles() {
    let core = BuiltinRoleSeeder::role_names(false);
    let all = BuiltinRoleSeeder::role_names(true);
    assert!(
        core.len() < all.len(),
        "the catalog must still carry at least one integration role"
    );
    for name in ["Credstore Secret Operator", "Usage Emitter"] {
        assert!(
            !core.contains(&name),
            "{name} targets another gear and MUST be gated"
        );
        assert!(all.contains(&name), "{name} MUST appear once gating is on");
    }
    for name in [
        "Owner",
        "Contributor",
        "Reader",
        "User Access Administrator",
    ] {
        assert!(core.contains(&name), "{name} is core and MUST always seed");
    }
    assert_eq!(BuiltinRoleSeeder::role_count(false), core.len());
    assert_eq!(BuiltinRoleSeeder::role_count(true), all.len());
}

/// A grant can only name a role the deployment actually seeds — the lookup is
/// what `RbacServiceConfig::validate` uses to refuse a dangling privilege.
#[test]
fn role_id_lookup_respects_the_integration_gate() {
    assert!(
        BuiltinRoleSeeder::role_id_by_name("Credstore Secret Operator", false).is_none(),
        "an ungated integration role MUST NOT resolve"
    );
    assert!(
        BuiltinRoleSeeder::role_id_by_name("Credstore Secret Operator", true).is_some(),
        "the same role MUST resolve once gating is on"
    );
    assert!(
        BuiltinRoleSeeder::role_id_by_name("Owner", false).is_some(),
        "a core role MUST resolve regardless of the gate"
    );
    assert!(
        BuiltinRoleSeeder::role_id_by_name("No Such Role", true).is_none(),
        "an unknown name MUST NOT resolve"
    );
}
