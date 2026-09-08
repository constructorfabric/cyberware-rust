//! Serialisation + variant round-trip tests for [`super::PermissionRule`]
//! and [`super::Action`].

#![allow(clippy::expect_used)]

use std::str::FromStr;

use super::{Action, PermissionRule, UnknownAction};

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
fn permission_rule_round_trips() {
    let rule = PermissionRule::new("read", "gts.cf.resources.compute.vm.v1~");
    round_trip(&rule);
}

#[test]
fn action_as_str_wire_form_stable() {
    assert_eq!(Action::Read.as_str(), "read");
    assert_eq!(Action::Write.as_str(), "write");
    assert_eq!(Action::Delete.as_str(), "delete");
    assert_eq!(Action::Wildcard.as_str(), "*");
}

#[test]
fn action_round_trips_every_variant() {
    for variant in [
        Action::Read,
        Action::Write,
        Action::Delete,
        Action::Wildcard,
    ] {
        round_trip(&variant);
    }
}

#[test]
fn action_from_str_recognises_canonical_forms() {
    assert_eq!(Action::from_str("read").expect("ok"), Action::Read);
    assert_eq!(Action::from_str("write").expect("ok"), Action::Write);
    assert_eq!(Action::from_str("delete").expect("ok"), Action::Delete);
    assert_eq!(Action::from_str("*").expect("ok"), Action::Wildcard);
}

#[test]
fn action_from_str_rejects_unknown_verb_with_value() {
    let err = Action::from_str("fly").expect_err("MUST reject");
    let UnknownAction(value) = err;
    assert_eq!(value, "fly");
}

/// `with_action` is the typed door to the same rule `new` builds from a string.
///
/// Asserting `rule.operation == "read"` alone read as an echo of the argument;
/// what the constructor is FOR is that the two doors cannot drift — an
/// `Action` lowered by `with_action` must produce a rule byte-identical to the
/// one a caller spells by hand, including on the wire.
#[test]
fn with_action_agrees_with_the_string_constructor_and_the_wire_form() {
    for action in [
        Action::Read,
        Action::Write,
        Action::Delete,
        Action::Wildcard,
    ] {
        let typed = PermissionRule::with_action(action, "gts.cf.x.y.v1~");
        let spelled = PermissionRule::new(action.as_str(), "gts.cf.x.y.v1~");
        assert_eq!(
            typed,
            spelled,
            "with_action({action:?}) must build the same rule as new({:?}, ..)",
            action.as_str()
        );
        assert_eq!(
            serde_json::to_value(&typed).expect("serialise"),
            serde_json::json!({
                "operation": action.as_str(),
                "target_type": "gts.cf.x.y.v1~",
            }),
            "the rule's wire form must carry the canonical verb"
        );
    }
}

/// `Action`'s serde form must be the canonical verb, not the variant name.
///
/// The derived impls emitted `"Read"`/`"Wildcard"` while `as_str`,
/// `Display` and `FromStr` all use `"read"`/`"*"`. A public type with two
/// string encodings is a trap for the first consumer to put an `Action` in
/// a payload, so the encoding is pinned here.
#[test]
fn action_serializes_as_the_canonical_verb() {
    for (action, wire) in [
        (Action::Read, "\"read\""),
        (Action::Write, "\"write\""),
        (Action::Delete, "\"delete\""),
        (Action::Wildcard, "\"*\""),
    ] {
        assert_eq!(
            serde_json::to_string(&action).expect("Action serializes"),
            wire,
            "Action must serialize as its canonical verb, matching as_str()"
        );
        assert_eq!(
            serde_json::from_str::<Action>(wire).expect("Action round-trips"),
            action,
            "the canonical verb must deserialize back to the same variant"
        );
    }
}

/// The variant-name spelling is rejected, not accepted alongside the canonical
/// one — two accepted spellings would be the same ambiguity in a different
/// place.
#[test]
fn action_rejects_the_variant_name_form() {
    assert!(
        serde_json::from_str::<Action>("\"Read\"").is_err(),
        "the variant-name spelling must not deserialize"
    );
}
