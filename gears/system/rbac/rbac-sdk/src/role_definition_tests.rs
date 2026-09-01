//! Serialisation round-trip for [`super::RoleDefinition`].

#![allow(clippy::expect_used)]

use uuid::Uuid;

use super::RoleDefinition;
use crate::permission_rule::PermissionRule;
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

fn sample_role_definition() -> RoleDefinition {
    RoleDefinition::new(
        Uuid::nil(),
        "Auditor",
        Some("Read-only auditor role".to_owned()),
        false,
        vec![PermissionRule::new(
            "read",
            "gts.cf.resources.compute.vm.v1~",
        )],
        vec![],
        vec![Scope::tenant(uuid::uuid!(
            "11111111-2222-3333-4444-555555555555"
        ))],
        Some(Uuid::nil()),
        chrono::DateTime::<chrono::Utc>::UNIX_EPOCH,
        chrono::DateTime::<chrono::Utc>::UNIX_EPOCH,
        "alice",
    )
}

#[test]
fn role_definition_round_trips() {
    round_trip(&sample_role_definition());
}

/// `assignable_scopes` is typed `Vec<Scope>` in Rust but must stay an array of
/// canonical path strings on the wire — the field was `Vec<String>` and no
/// consumer may have to change to keep reading it.
#[test]
fn assignable_scopes_serialize_as_canonical_path_strings() {
    let value = serde_json::to_value(sample_role_definition()).expect("serialize");
    assert_eq!(
        value.get("assignable_scopes"),
        Some(&serde_json::json!([
            "/tenants/11111111-2222-3333-4444-555555555555"
        ])),
        "typed scopes must serialize as the same strings the field carried before"
    );

    // And the same JSON still deserializes — an older producer's payload.
    let back: RoleDefinition = serde_json::from_value(value).expect("deserialize");
    assert_eq!(
        back.assignable_scopes,
        vec![Scope::tenant(uuid::uuid!(
            "11111111-2222-3333-4444-555555555555"
        ))]
    );
}

// `assignment_count`'s contract is the omit-vs-zero WIRE shape, pinned by
// `absent_count_is_omitted_while_zero_is_emitted` below. A separate test that
// set `Some(3)` and read `Some(3)` back protected nothing the type system did
// not already guarantee.

/// The wire shape distinguishes "no count" from "zero": an unset count emits
/// no key at all, while `Some(0)` emits `0`. A client that rendered a missing
/// key as `0` would report a role as unused to a caller who merely cannot see
/// its assignments, so the omission has to be observable.
#[test]
fn absent_count_is_omitted_while_zero_is_emitted() {
    let bare = serde_json::to_value(sample_role_definition()).expect("serialize");
    assert!(
        bare.get("assignment_count").is_none(),
        "an unresolved count MUST be omitted, never null; body={bare}"
    );

    let zero = serde_json::to_value(sample_role_definition().with_assignment_count(Some(0)))
        .expect("serialize");
    assert_eq!(
        zero.get("assignment_count"),
        Some(&serde_json::json!(0)),
        "a real zero MUST reach the wire; body={zero}"
    );

    round_trip(&sample_role_definition().with_assignment_count(Some(42)));
}
