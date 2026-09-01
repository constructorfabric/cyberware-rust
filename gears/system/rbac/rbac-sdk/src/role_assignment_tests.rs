//! Serialisation round-trips for [`super::RoleAssignment`] +
//! [`super::PrincipalType`] every-variant coverage.

#![allow(clippy::expect_used)]

use std::str::FromStr;

use uuid::Uuid;

use super::{PrincipalType, RoleAssignment, UnknownPrincipalType};
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

fn sample_role_assignment() -> RoleAssignment {
    RoleAssignment::new(
        Uuid::nil(),
        Uuid::nil(),
        "user-1",
        PrincipalType::User,
        Scope::tenant(uuid::uuid!("11111111-2222-3333-4444-555555555555")),
        chrono::DateTime::<chrono::Utc>::UNIX_EPOCH,
        chrono::DateTime::<chrono::Utc>::UNIX_EPOCH,
        "alice",
    )
}

#[test]
fn role_assignment_round_trips() {
    round_trip(&sample_role_assignment());
}

/// `new` MUST NOT invent display names: they are resolved on the read
/// path and attached afterwards, so a freshly constructed assignment
/// carries neither. Passing `None` to a setter is equally legal — the
/// hydrator forwards whatever it managed to resolve, including nothing.
#[test]
fn new_leaves_display_names_unset_and_setters_populate_them() {
    let bare = sample_role_assignment();
    assert!(
        bare.principal_name.is_none(),
        "new() must not invent a name"
    );
    assert!(bare.created_by_name.is_none());
    assert!(bare.role_definition_name.is_none());

    let named = bare
        .with_principal_name(Some("Ada Lovelace".to_owned()))
        .with_created_by_name(Some("Grace Hopper".to_owned()))
        .with_role_definition_name(Some("Tenant Administrator".to_owned()));
    assert_eq!(named.principal_name.as_deref(), Some("Ada Lovelace"));
    assert_eq!(named.created_by_name.as_deref(), Some("Grace Hopper"));
    assert_eq!(
        named.role_definition_name.as_deref(),
        Some("Tenant Administrator")
    );

    assert!(named.with_principal_name(None).principal_name.is_none());
}

/// An unresolved name is *absent* from the wire, never `null`: the
/// `skip_serializing_if` on all three name fields is what the REST DTO
/// contract downstream relies on, so it is asserted here rather than
/// assumed.
#[test]
fn unresolved_display_names_are_absent_from_the_wire() {
    let value = serde_json::to_value(sample_role_assignment()).expect("serialize");
    assert!(
        value.get("principal_name").is_none(),
        "an unresolved principal name MUST be omitted; got: {value}"
    );
    assert!(
        value.get("created_by_name").is_none(),
        "an unresolved author name MUST be omitted; got: {value}"
    );
    assert!(
        value.get("role_definition_name").is_none(),
        "an unresolved role name MUST be omitted; got: {value}"
    );

    // A resolved name still round-trips losslessly (`serde(default)`
    // pairs with the skip so the absent case deserialises back to None).
    round_trip(
        &sample_role_assignment()
            .with_principal_name(Some("Ada Lovelace".to_owned()))
            .with_created_by_name(Some("Grace Hopper".to_owned()))
            .with_role_definition_name(Some("Tenant Administrator".to_owned())),
    );
}

#[test]
fn principal_type_round_trips_every_variant() {
    for variant in [
        PrincipalType::User,
        PrincipalType::Group,
        PrincipalType::ServicePrincipal,
    ] {
        round_trip(&variant);
    }
}

#[test]
fn principal_type_as_str_wire_form_stable() {
    assert_eq!(PrincipalType::User.as_str(), "User");
    assert_eq!(PrincipalType::Group.as_str(), "Group");
    assert_eq!(PrincipalType::ServicePrincipal.as_str(), "ServicePrincipal");
}

#[test]
fn principal_type_from_str_recognises_canonical_tags() {
    assert_eq!(
        PrincipalType::from_str("User").expect("ok"),
        PrincipalType::User
    );
    assert_eq!(
        PrincipalType::from_str("Group").expect("ok"),
        PrincipalType::Group
    );
    assert_eq!(
        PrincipalType::from_str("ServicePrincipal").expect("ok"),
        PrincipalType::ServicePrincipal
    );
}

#[test]
fn principal_type_from_str_rejects_unknown_tag_with_value() {
    let err = PrincipalType::from_str("Robot").expect_err("MUST reject");
    let UnknownPrincipalType(value) = err;
    assert_eq!(value, "Robot");
}
