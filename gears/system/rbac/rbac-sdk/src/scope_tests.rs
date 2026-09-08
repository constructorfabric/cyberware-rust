//! Serialisation + parse round-trip tests for [`super::Scope`].

#![allow(clippy::expect_used)]

use uuid::Uuid;

use super::{Scope, ScopeParseError};

fn round_trip<T>(value: &T) -> T
where
    T: serde::Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug,
{
    let bytes = serde_json::to_vec(value).expect("serialize");
    let back: T = serde_json::from_slice(&bytes).expect("deserialize");
    assert_eq!(value, &back, "round-trip must be lossless");
    back
}

#[test]
fn root_scope_round_trips() {
    round_trip(&Scope::root());
}

#[test]
fn tenant_scope_round_trips() {
    round_trip(&Scope::tenant(Uuid::nil()));
}

#[test]
fn resource_group_scope_round_trips() {
    round_trip(&Scope::resource_group(Uuid::nil(), Uuid::nil()));
}

#[test]
fn path_form_round_trips_through_parse() {
    let scopes = [
        Scope::root(),
        Scope::tenant(uuid::uuid!("11111111-2222-3333-4444-555555555555")),
        Scope::resource_group(
            uuid::uuid!("11111111-2222-3333-4444-555555555555"),
            uuid::uuid!("22222222-2222-2222-2222-222222222222"),
        ),
    ];
    for scope in scopes {
        let path = scope.path();
        let parsed = Scope::parse(&path).expect("parse");
        assert_eq!(scope, parsed, "path-form round-trip");
    }
}

#[test]
fn parse_rejects_invalid_format() {
    let err = Scope::parse("not-a-scope").expect_err("MUST reject");
    assert!(matches!(err, ScopeParseError::InvalidFormat(_)));
}

#[test]
fn parse_rejects_invalid_tenant_uuid() {
    let err = Scope::parse("/tenants/not-a-uuid").expect_err("MUST reject");
    assert!(matches!(err, ScopeParseError::InvalidTenantUuid(_)));
}

#[test]
fn parse_rejects_invalid_resource_group_uuid() {
    let err =
        Scope::parse("/tenants/11111111-2222-3333-4444-555555555555/resourceGroups/not-a-uuid")
            .expect_err("MUST reject");
    assert!(matches!(err, ScopeParseError::InvalidResourceGroupUuid(_)));
}

#[test]
fn is_ancestor_of_root_matches_everything() {
    let root = Scope::root();
    assert!(root.is_ancestor_of(&Scope::tenant(Uuid::nil())));
    assert!(root.is_ancestor_of(&Scope::resource_group(Uuid::nil(), Uuid::nil())));
    assert!(root.is_ancestor_of(&Scope::root()));
}

#[test]
fn is_ancestor_of_is_not_symmetric() {
    let tenant = Scope::tenant(uuid::uuid!("11111111-2222-3333-4444-555555555555"));
    let rg = Scope::resource_group(
        uuid::uuid!("11111111-2222-3333-4444-555555555555"),
        uuid::uuid!("22222222-2222-2222-2222-222222222222"),
    );
    assert!(tenant.is_ancestor_of(&rg));
    assert!(!rg.is_ancestor_of(&tenant));
}
