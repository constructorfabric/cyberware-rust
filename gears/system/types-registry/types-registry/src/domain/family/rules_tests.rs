//! Which identifier decides each rule. Pure: no database, no clock.
//!
//! `family_test.rs` drives the same rules through the worker against real rows;
//! this file pins the arithmetic underneath, where a wrong spelling would make a
//! rule silently never fire rather than fail loudly.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use super::{VersionProbe, version_probe};
use toolkit_gts::gts_id;

fn probe(id: &str) -> VersionProbe {
    let parsed = gts::GtsId::try_new(id).unwrap_or_else(|e| panic!("{id}: {e}"));
    version_probe(&parsed).unwrap_or_else(|| panic!("{id} has a readable version"))
}

/// A major-only candidate is decided by `vM.0~` alone — not by `vM.1~`, and not by
/// another major.
#[test]
fn a_major_only_candidate_is_blocked_by_the_first_minor() {
    assert_eq!(
        probe(gts_id!("cf.core.example.thing.v1~")),
        VersionProbe::MajorOnly {
            blocker: gts_id!("cf.core.example.thing.v1.0~").to_owned(),
        },
    );
}

/// `vM.0~` opens its major: one blocker, no predecessor.
#[test]
fn the_first_minor_has_a_blocker_and_no_predecessor() {
    assert_eq!(
        probe(gts_id!("cf.core.example.thing.v1.0~")),
        VersionProbe::FirstMinor {
            blocker: gts_id!("cf.core.example.thing.v1~").to_owned(),
        },
    );
}

/// A later minor asks both questions, and its predecessor is `n - 1` in the **same**
/// major.
#[test]
fn a_later_minor_asks_both_questions() {
    assert_eq!(
        probe(gts_id!("cf.core.example.thing.v2.4~")),
        VersionProbe::LaterMinor {
            blocker: gts_id!("cf.core.example.thing.v2~").to_owned(),
            predecessor: gts_id!("cf.core.example.thing.v2.3~").to_owned(),
        },
    );
}

/// The probes carry the candidate's **own** kind marker. An Instance probing a
/// `~`-suffixed identifier would ask about a Type Schema — a different entity that
/// the family rules would then read as a sibling.
#[test]
fn an_instance_probes_instance_spellings() {
    assert_eq!(
        probe(gts_id!("cf.core.example.thing.v1~cf.core.example.first.v2")),
        VersionProbe::MajorOnly {
            blocker: gts_id!("cf.core.example.thing.v1~cf.core.example.first.v2.0").to_owned(),
        },
    );
}

/// A minor in a **preceding** segment is part of the identity, not the version, so
/// it survives into every probe verbatim — the same property `key_tests.rs` pins
/// for the family key itself.
#[test]
fn a_preceding_segment_minor_survives_into_the_probes() {
    assert_eq!(
        probe(gts_id!(
            "acme.crm.customer.type.v1.4~acme.crm.premium.type.v3.1~"
        )),
        VersionProbe::LaterMinor {
            blocker: gts_id!("acme.crm.customer.type.v1.4~acme.crm.premium.type.v3~").to_owned(),
            predecessor: gts_id!("acme.crm.customer.type.v1.4~acme.crm.premium.type.v3.0~")
                .to_owned(),
        },
    );
}

/// Every probe names a member of the candidate's **own** family: the rules are
/// keyed lookups inside one family, and a probe that left it would let an unrelated
/// entity decide.
#[test]
fn every_probe_stays_inside_the_candidate_family() {
    for id in [
        gts_id!("cf.core.example.thing.v1~"),
        gts_id!("cf.core.example.thing.v1.0~"),
        gts_id!("cf.core.example.thing.v9.7~"),
    ] {
        let parsed = gts::GtsId::try_new(id).expect("fixture");
        let expected = super::super::family_key(&parsed);
        let probes = match probe(id) {
            VersionProbe::MajorOnly { blocker } | VersionProbe::FirstMinor { blocker } => {
                vec![blocker]
            }
            VersionProbe::LaterMinor {
                blocker,
                predecessor,
            } => vec![blocker, predecessor],
        };
        for probed in probes {
            let reparsed = gts::GtsId::try_new(&probed)
                .unwrap_or_else(|e| panic!("{id} probed an unparsable '{probed}': {e}"));
            assert_eq!(
                super::super::family_key(&reparsed),
                expected,
                "{id} probed '{probed}', which is in another family",
            );
        }
    }
}

/// The `None` branch, which no other test reaches: a UUID tail carries no major,
/// and `None` is what makes `admits_new_member` refuse rather than evaluate the
/// shape and contiguity rules. A change here would silently move which rules run,
/// and every other test would stay green.
#[test]
fn a_versionless_tail_has_no_probe() {
    let with_tail = format!(
        "{}550e8400-e29b-41d4-a716-446655440000",
        gts_id!("cf.core.example.thing.v1~")
    );
    let parsed = gts::GtsId::try_new(&with_tail).expect("a UUID tail parses");
    assert_eq!(version_probe(&parsed), None);
}

// ---------------------------------------------------------------------------
// Refusal messages
// ---------------------------------------------------------------------------
//
// `unit::commit_creation` builds the item failure from `refusal.to_string()`, so
// this `Display` is the wire-visible diagnosis: swapping two of its fields inverts
// what a caller is told while every rule still fires, which no rule test can catch.

use super::FamilyRefusal;
use crate::domain::admission::AdmissionFailureReason;
use crate::domain::enums::EntityKind;
use crate::domain::family::family_key;

fn key_of(id: &str) -> crate::domain::family::FamilyKey {
    family_key(&gts::GtsId::try_new(id).expect("fixture"))
}

/// The candidate's kind and the family's are in the positions that name them.
/// Reversed, the message would tell a caller to fix the entity that is already
/// correct.
#[test]
fn a_kind_conflict_names_the_candidate_before_the_family() {
    let refusal = FamilyRefusal::KindConflict {
        gts_id: gts_id!("cf.core.example.thing.v2~").to_owned(),
        family_key: key_of(gts_id!("cf.core.example.thing.v2~")),
        candidate: EntityKind::TypeSchema,
        existing: EntityKind::Instance,
    };
    assert_eq!(refusal.reason(), AdmissionFailureReason::FamilyKindConflict);
    assert_eq!(
        refusal.to_string(),
        "'gts.cf.core.example.thing.v2~' is a Type Schema, but version family \
         'gts.cf.core.example.thing' already holds Registered Instance members; a family holds one \
         kind",
    );
}

#[test]
fn a_shape_conflict_names_the_member_that_already_decided_the_major() {
    let refusal = FamilyRefusal::MinorShape {
        gts_id: gts_id!("cf.core.example.thing.v1.0~").to_owned(),
        conflicting: gts_id!("cf.core.example.thing.v1~").to_owned(),
    };
    assert_eq!(
        refusal.reason(),
        AdmissionFailureReason::FamilyShapeConflict
    );
    assert_eq!(
        refusal.to_string(),
        "'gts.cf.core.example.thing.v1.0~' cannot join the major that \
         'gts.cf.core.example.thing.v1~' already spells the other way; within one major either \
         every member carries a minor or none does",
    );
}

#[test]
fn a_missing_predecessor_names_the_predecessor_and_not_the_candidate() {
    let refusal = FamilyRefusal::MissingPredecessor {
        gts_id: gts_id!("cf.core.example.thing.v1.2~").to_owned(),
        predecessor: gts_id!("cf.core.example.thing.v1.1~").to_owned(),
    };
    assert_eq!(refusal.reason(), AdmissionFailureReason::MissingPredecessor);
    assert_eq!(
        refusal.to_string(),
        "'gts.cf.core.example.thing.v1.2~' requires its predecessor \
         'gts.cf.core.example.thing.v1.1~'; the minors of a major are contiguous and open at M.0",
    );
}

/// The fail-closed refusal: reachable only if acceptance let a versionless
/// identifier through, and it must say that rather than name a sibling it could
/// not derive.
#[test]
fn an_unreadable_version_refuses_without_naming_a_sibling() {
    let refusal = FamilyRefusal::UnreadableVersion {
        gts_id: "cf.core.example.thing.v1~550e8400-e29b-41d4-a716-446655440000".to_owned(),
    };
    assert_eq!(refusal.reason(), AdmissionFailureReason::UnreadableVersion);
    assert_eq!(
        refusal.to_string(),
        "'cf.core.example.thing.v1~550e8400-e29b-41d4-a716-446655440000' names no readable \
         version in its last segment, so the family's shape and contiguity rules have nothing \
         to compare it against",
    );
}

/// `EntityKind`'s wire spelling is reachable only through the message above, so it
/// is pinned here beside it.
#[test]
fn entity_kinds_spell_themselves_the_way_the_messages_read() {
    assert_eq!(EntityKind::TypeSchema.to_string(), "Type Schema");
    assert_eq!(EntityKind::Instance.to_string(), "Registered Instance");
}
