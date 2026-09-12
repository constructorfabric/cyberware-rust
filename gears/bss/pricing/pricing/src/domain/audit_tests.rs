//! What the audit encoding promises a verification job seven years from now.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use serde_json::json;
use uuid::Uuid;

use super::{
    AUDIT_DOMAIN_SEP, AuditAction, AuditRecord, AuditSubjectKind, audit_row_hash,
    genesis_prev_hash, hex32, subject_state,
};
use crate::domain::instant::utc_ymd_hms;
use time::OffsetDateTime;

const TENANT: Uuid = Uuid::from_u128(0x0000_0000_0000_0000_0000_0000_0000_0001);
const CHAIN: Uuid = Uuid::from_u128(0x0000_0000_0000_0000_0000_0000_0000_0002);
const ACTOR: Uuid = Uuid::from_u128(0x0000_0000_0000_0000_0000_0000_0000_0003);
const APPROVAL: Uuid = Uuid::from_u128(0x0000_0000_0000_0000_0000_0000_0000_0004);
const CORRELATION: Uuid = Uuid::from_u128(0x0000_0000_0000_0000_0000_0000_0000_0005);

fn at() -> OffsetDateTime {
    utc_ymd_hms(2026, 8, 3, 12, 0, 0)
}

fn record<'a>(
    subject_ref: &'a str,
    before: Option<&'a serde_json::Value>,
    after: Option<&'a serde_json::Value>,
) -> AuditRecord<'a> {
    AuditRecord {
        tenant_id: TENANT,
        chain_id: CHAIN,
        seq: 0,
        recorded_at: at(),
        actor_principal_id: ACTOR,
        action: AuditAction::Publish,
        subject_kind: AuditSubjectKind::PlanRevision,
        subject_ref,
        before_state: before,
        after_state: after,
        approval_ref: Some(APPROVAL),
        correlation_id: Some(CORRELATION),
    }
}

// ---------------------------------------------------------------------------
// The persisted tokens.
// ---------------------------------------------------------------------------

/// Split from the subject kinds below, and the split is mechanical rather than a
/// change of subject: `clippy::cognitive_complexity` caps a function at 20 and
/// `bulk_operation`'s arrival (D-158) made this one 21. Every assertion is the one
/// it was; nothing is derived that was a literal.
///
/// `Membership` then took the same pair to 22 on a concurrent branch, which split
/// it a second time under different names. Two independent vocabularies were
/// always two claims; the lint's margin is only what made that visible.
#[test]
fn the_persisted_action_tokens_are_asserted_against_literals() {
    // Not derived from the variant identifiers, exactly as `CatalogEvent`'s
    // are not: an Auditor surface with a seven-year horizon reads these
    // strings, so a variant rename must not silently rename a stored token.
    assert_eq!(AuditAction::Create.as_str(), "create");
    assert_eq!(AuditAction::Update.as_str(), "update");
    assert_eq!(AuditAction::Delete.as_str(), "delete");
    assert_eq!(AuditAction::Abandon.as_str(), "abandon");
    assert_eq!(AuditAction::Publish.as_str(), "publish");
    assert_eq!(AuditAction::Submit.as_str(), "submit");
    assert_eq!(AuditAction::Approve.as_str(), "approve");
    assert_eq!(AuditAction::Reject.as_str(), "reject");
    assert_eq!(AuditAction::Withdraw.as_str(), "withdraw");
    assert_eq!(AuditAction::Deny.as_str(), "deny");
    assert_eq!(AuditAction::Retire.as_str(), "retire");
    assert_eq!(AuditAction::Migrate.as_str(), "migrate");
    assert_eq!(
        AuditAction::ALL,
        &[
            AuditAction::Create,
            AuditAction::Update,
            AuditAction::Delete,
            AuditAction::Abandon,
            AuditAction::Publish,
            AuditAction::Submit,
            AuditAction::Approve,
            AuditAction::Reject,
            AuditAction::Withdraw,
            AuditAction::Deny,
            AuditAction::Retire,
            AuditAction::Migrate,
        ]
    );
}

/// [`the_persisted_action_tokens_are_asserted_against_literals`]'s other half:
/// the persisted tokens are literals, asserted against literals, so a variant
/// rename cannot silently rename a stored one.
#[test]
fn the_persisted_subject_kind_tokens_are_asserted_against_literals() {
    assert_eq!(AuditSubjectKind::PlanRevision.as_str(), "plan_revision");
    assert_eq!(AuditSubjectKind::PriceUnit.as_str(), "price_unit");
    assert_eq!(AuditSubjectKind::Window.as_str(), "window");
    assert_eq!(AuditSubjectKind::Policy.as_str(), "policy");
    assert_eq!(AuditSubjectKind::Overlay.as_str(), "overlay");
    assert_eq!(AuditSubjectKind::BulkOperation.as_str(), "bulk_operation");
    assert_eq!(AuditSubjectKind::Membership.as_str(), "membership");
    assert_eq!(AuditSubjectKind::TaxonomyValue.as_str(), "taxonomy_value");
    assert_eq!(
        AuditSubjectKind::ALL,
        &[
            AuditSubjectKind::PlanRevision,
            AuditSubjectKind::PriceUnit,
            AuditSubjectKind::Window,
            AuditSubjectKind::Policy,
            AuditSubjectKind::Overlay,
            AuditSubjectKind::BulkOperation,
            AuditSubjectKind::Membership,
            AuditSubjectKind::TaxonomyValue,
        ]
    );
}

/// Every variant reaches [`AuditAction::ALL`], enforced by the compiler.
///
/// The literal assertion above compares `ALL` to a hand-written list, so a
/// variant added to the enum and omitted from `ALL` leaves both lists agreeing
/// and every test in this file green. `ALL` is the operand of
/// `sqlite_publish_commit::every_declared_action_has_a_production_writer` and of
/// `sqlite_audit_chain`'s token-to-variant lookup, so a variant missing from it
/// escapes the writer census and is unresolvable on read.
///
/// The `match` is the gate, in `events_tests::every_variant_is_in_the_roster`'s
/// shape: **no `_ =>` arm**, so a new variant stops this file compiling until
/// somebody names it here, and naming it here is what puts it in front of the
/// membership assertion.
#[test]
fn every_action_is_in_the_roster() {
    fn rostered(action: AuditAction) -> bool {
        // No `_ =>` arm, deliberately: this match is the gate.
        match action {
            AuditAction::Create
            | AuditAction::Update
            | AuditAction::Delete
            | AuditAction::Abandon
            | AuditAction::Publish
            | AuditAction::Submit
            | AuditAction::Approve
            | AuditAction::Reject
            | AuditAction::Withdraw
            | AuditAction::Deny
            | AuditAction::Retire
            | AuditAction::Migrate => AuditAction::ALL.contains(&action),
        }
    }

    let every = [
        AuditAction::Create,
        AuditAction::Update,
        AuditAction::Delete,
        AuditAction::Abandon,
        AuditAction::Publish,
        AuditAction::Submit,
        AuditAction::Approve,
        AuditAction::Reject,
        AuditAction::Withdraw,
        AuditAction::Deny,
        AuditAction::Retire,
        AuditAction::Migrate,
    ];
    for action in every {
        assert!(rostered(action), "{action:?} is not in AuditAction::ALL");
    }
    // The match and the array are one list twice, and this is what keeps them so:
    // a variant named in the match and forgotten here would leave the array short
    // of `ALL`.
    assert_eq!(every.len(), AuditAction::ALL.len());
}

/// [`every_action_is_in_the_roster`]'s other half, for the same reason: `ALL` is
/// what `pricing_approval`'s and `pricing_audit_log`'s one enumeration is read
/// through, and a variant outside it is storable by nothing.
#[test]
fn every_subject_kind_is_in_the_roster() {
    fn rostered(kind: AuditSubjectKind) -> bool {
        // No `_ =>` arm, deliberately: this match is the gate.
        match kind {
            AuditSubjectKind::PlanRevision
            | AuditSubjectKind::PriceUnit
            | AuditSubjectKind::Window
            | AuditSubjectKind::Policy
            | AuditSubjectKind::Overlay
            | AuditSubjectKind::BulkOperation
            | AuditSubjectKind::Membership
            | AuditSubjectKind::TaxonomyValue => AuditSubjectKind::ALL.contains(&kind),
        }
    }

    let every = [
        AuditSubjectKind::PlanRevision,
        AuditSubjectKind::PriceUnit,
        AuditSubjectKind::Window,
        AuditSubjectKind::Policy,
        AuditSubjectKind::Overlay,
        AuditSubjectKind::BulkOperation,
        AuditSubjectKind::Membership,
        AuditSubjectKind::TaxonomyValue,
    ];
    for kind in every {
        assert!(rostered(kind), "{kind:?} is not in AuditSubjectKind::ALL");
    }
    assert_eq!(every.len(), AuditSubjectKind::ALL.len());
}

#[test]
fn every_token_is_its_own_and_the_slice_names_each_once() {
    // The vocabulary is a set of *discriminators*: two variants sharing a token
    // would make an Auditor unable to tell two acts apart, and the stored string
    // is all a reader of `pricing_audit_log` has — the column holds `as_str`'s
    // output and this crate has no reverse map, so a duplicate is not resolved to
    // the wrong variant, it is unresolvable. (An earlier version of this comment
    // blamed "`read_token`'s linear search over `ALL`"; no such function exists
    // here or anywhere in the crate.) Asserted over the whole slice rather than
    // pairwise, so a token added without a fresh spelling is caught by the roster
    // rather than by whoever remembers.
    let tokens: std::collections::BTreeSet<&str> =
        AuditAction::ALL.iter().map(|a| a.as_str()).collect();
    assert_eq!(
        tokens.len(),
        AuditAction::ALL.len(),
        "two actions share a stored token"
    );
    // Both halves of D-158's `snake_case` rule, which is what keeps a token from
    // ever colliding with a frozen `CatalogEvent` name like `PlanPublished`.
    for action in AuditAction::ALL {
        let token = action.as_str();
        assert!(
            token.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
            "{token} is not snake_case"
        );
    }
}

#[test]
fn a_delete_is_not_an_abandon_and_the_tokens_say_so() {
    // D-145's whole point in one assertion. A discarded draft revision is
    // **flipped**, so its `(plan_id, revision)` name stays consumed and the row
    // survives as a tombstone; a discarded draft price row is **deleted**, and
    // `inst-ps-nodelete` keeps that off a published one. Two acts with two
    // different consequences for the durable name, so two tokens - and an
    // auditor reading one for the other would read a permanent name as reusable.
    assert_ne!(AuditAction::Abandon, AuditAction::Delete);
    assert_ne!(
        AuditAction::Abandon.as_str(),
        AuditAction::Delete.as_str(),
        "and the stored tokens differ too, which is what the reader sees"
    );
}

#[test]
fn a_subject_state_says_what_a_row_looked_like_and_nothing_more() {
    // One rendering for every audited subject in the crate. The pending ref rides
    // only where there is one - the publish commit's after-state - because it is
    // what connects that mutation to the addressability it produced.
    let plain = subject_state(crate::domain::lifecycle::LifecycleState::Draft, 3, None);
    assert_eq!(
        plain,
        serde_json::json!({ "lifecycleState": "draft", "rowVersion": 3 })
    );

    let published = subject_state(
        crate::domain::lifecycle::LifecycleState::Published,
        4,
        Some("pend-9"),
    );
    assert_eq!(
        published.get("pendingVersionRef"),
        Some(&serde_json::json!("pend-9"))
    );
}

#[test]
fn the_domain_separation_tag_is_this_gears_own() {
    assert_eq!(AUDIT_DOMAIN_SEP, b"VHP-BSS-PRICING-AUDIT-v1\x1f");
}

// ---------------------------------------------------------------------------
// The frozen byte-repro vector.
// ---------------------------------------------------------------------------

#[test]
fn a_fixed_record_hashes_to_its_frozen_digest() {
    // The test that makes an accidental encoding change loud. If this fails
    // and the change was deliberate, bump `AUDIT_DOMAIN_SEP` and regenerate
    // this vector; if it was not, the chain just stopped verifying.
    let before = json!({"lifecycle_state": "draft", "row_version": 0});
    let after = json!({"lifecycle_state": "published", "row_version": 1});
    let rec = record(
        "00000000-0000-0000-0000-000000000002/1",
        Some(&before),
        Some(&after),
    );
    let prev = [7_u8; 32];

    let digest = audit_row_hash(&rec, &prev).expect("hash a fixed record");

    assert_eq!(
        hex32(&digest),
        "fdc0d35bbcae82609b60fd96c76e103a9b92b38c47045aba8a309333ff44bdca"
    );
}

#[test]
fn the_digest_is_a_full_thirty_two_bytes() {
    let rec = record("plan/1", None, None);
    let digest = audit_row_hash(&rec, &[0_u8; 32]).expect("hash");

    assert_eq!(digest.len(), 32);
    assert_eq!(hex32(&digest).len(), 64);
}

// ---------------------------------------------------------------------------
// Framing: two field boundaries can never collide.
// ---------------------------------------------------------------------------

#[test]
fn two_field_boundaries_over_one_byte_string_never_collide() {
    // Without the length prefixes these two preimages are identical: the
    // concatenated field bytes are the same and only the boundary moved. A
    // chain that cannot tell them apart can be forged by moving a character
    // across a field border, with every link still verifying.
    let mut split_early = Vec::new();
    super::put_str(&mut split_early, "a");
    super::put_str(&mut split_early, "bc");

    let mut split_late = Vec::new();
    super::put_str(&mut split_late, "ab");
    super::put_str(&mut split_late, "c");

    assert_ne!(super::digest32(&split_early), super::digest32(&split_late));
}

#[test]
fn an_absent_field_and_an_empty_one_hash_differently() {
    // The NULL-safe half of the same property: `None` is a bare marker and an
    // empty value is a marker plus a zero length, so a nulled column and a
    // cleared one are distinguishable.
    let empty = json!({});
    let prev = [0_u8; 32];

    let absent = audit_row_hash(&record("plan/1", None, None), &prev).expect("hash absent");
    let present =
        audit_row_hash(&record("plan/1", Some(&empty), None), &prev).expect("hash present");

    assert_ne!(absent, present);
}

// ---------------------------------------------------------------------------
// The jsonb columns are hashed canonically.
// ---------------------------------------------------------------------------

#[test]
fn json_key_order_does_not_change_the_hash() {
    // `jsonb` does not preserve key order, so a value written one way and read
    // back another is the normal case. Hashing the emitted order would make
    // the verification job report a break that never happened.
    let mut forward = serde_json::Map::new();
    forward.insert("alpha".to_owned(), json!(1));
    forward.insert("beta".to_owned(), json!(2));
    let mut backward = serde_json::Map::new();
    backward.insert("beta".to_owned(), json!(2));
    backward.insert("alpha".to_owned(), json!(1));

    let forward = serde_json::Value::Object(forward);
    let backward = serde_json::Value::Object(backward);
    let prev = [0_u8; 32];

    assert_eq!(
        audit_row_hash(&record("plan/1", Some(&forward), None), &prev).expect("forward"),
        audit_row_hash(&record("plan/1", Some(&backward), None), &prev).expect("backward")
    );
}

#[test]
fn nested_json_key_order_does_not_change_the_hash_either() {
    let forward = json!({"outer": {"a": 1, "b": [ {"x": 1, "y": 2} ]}});
    let backward = json!({"outer": {"b": [ {"y": 2, "x": 1} ], "a": 1}});
    let prev = [0_u8; 32];

    assert_eq!(
        audit_row_hash(&record("plan/1", Some(&forward), None), &prev).expect("forward"),
        audit_row_hash(&record("plan/1", Some(&backward), None), &prev).expect("backward")
    );
}

#[test]
fn json_array_order_does_change_the_hash() {
    // Order is semantic in a JSON array, so canonicalization must not sort it.
    let left = json!({"ids": [1, 2]});
    let right = json!({"ids": [2, 1]});
    let prev = [0_u8; 32];

    assert_ne!(
        audit_row_hash(&record("plan/1", Some(&left), None), &prev).expect("left"),
        audit_row_hash(&record("plan/1", Some(&right), None), &prev).expect("right")
    );
}

// ---------------------------------------------------------------------------
// Genesis is bound to the segment.
// ---------------------------------------------------------------------------

#[test]
fn genesis_differs_per_chain_within_one_tenant() {
    // A genesis bound to the tenant alone would give every one of its segments
    // the same first link, and a whole segment could then be transplanted onto
    // another aggregate undetectably.
    assert_ne!(
        genesis_prev_hash(TENANT, Uuid::from_u128(10)),
        genesis_prev_hash(TENANT, Uuid::from_u128(11))
    );
}

#[test]
fn genesis_differs_per_tenant_for_one_chain() {
    assert_ne!(
        genesis_prev_hash(Uuid::from_u128(10), CHAIN),
        genesis_prev_hash(Uuid::from_u128(11), CHAIN)
    );
}

#[test]
fn genesis_is_stable_for_one_segment() {
    assert_eq!(
        genesis_prev_hash(TENANT, CHAIN),
        genesis_prev_hash(TENANT, CHAIN)
    );
}

#[test]
fn a_genesis_seed_is_not_a_row_hash_of_the_same_inputs() {
    // The two encodings share a domain-separation tag, so the seed and a first
    // row must be distinguishable by their content and not by luck.
    let rec = record("plan/1", None, None);
    let seeded = genesis_prev_hash(TENANT, CHAIN);
    let hashed = audit_row_hash(&rec, &seeded).expect("hash");

    assert_ne!(seeded, hashed);
}

// ---------------------------------------------------------------------------
// Every field is in the encoding.
// ---------------------------------------------------------------------------

/// Drive the record field by field: each mutation must move the digest, so a
/// field silently dropped from the encoding fails here rather than years later
/// when a forged row verifies.
#[test]
fn changing_any_single_field_changes_the_hash() {
    let before = json!({"k": 1});
    let after = json!({"k": 2});
    let base = record("plan/1", Some(&before), Some(&after));
    let prev = [0_u8; 32];
    let baseline = audit_row_hash(&base, &prev).expect("baseline");

    let other = json!({"k": 99});
    let mutations: Vec<(&str, AuditRecord<'_>)> = vec![
        (
            "tenant_id",
            AuditRecord {
                tenant_id: Uuid::from_u128(99),
                ..base
            },
        ),
        (
            "chain_id",
            AuditRecord {
                chain_id: Uuid::from_u128(99),
                ..base
            },
        ),
        ("seq", AuditRecord { seq: 1, ..base }),
        (
            "recorded_at",
            AuditRecord {
                recorded_at: at() + time::Duration::milliseconds(1),
                ..base
            },
        ),
        (
            "actor_principal_id",
            AuditRecord {
                actor_principal_id: Uuid::from_u128(99),
                ..base
            },
        ),
        (
            "subject_ref",
            AuditRecord {
                subject_ref: "plan/2",
                ..base
            },
        ),
        (
            "before_state",
            AuditRecord {
                before_state: Some(&other),
                ..base
            },
        ),
        (
            "after_state",
            AuditRecord {
                after_state: Some(&other),
                ..base
            },
        ),
        (
            "approval_ref",
            AuditRecord {
                approval_ref: None,
                ..base
            },
        ),
        (
            "correlation_id",
            AuditRecord {
                correlation_id: None,
                ..base
            },
        ),
        // The two vocabulary tokens. They are in the table rather than left to
        // `the_action_and_the_subject_kind_are_in_the_hash` below, because that
        // one compares against a preimage that omits both and so cannot tell
        // "the token is framed" from "a constant is framed in its place": an
        // encoder writing `put_str(buf, "publish")` keeps the frozen vector and
        // every other assertion in this file green while a `deny` row and an
        // `approve` row on one segment hash identically -- a record alterable
        // along a whole dimension with every chain link still verifying.
        (
            "action",
            AuditRecord {
                action: AuditAction::Deny,
                ..base
            },
        ),
        (
            "subject_kind",
            AuditRecord {
                subject_kind: AuditSubjectKind::Policy,
                ..base
            },
        ),
    ];

    for (field, mutated) in mutations {
        let digest = audit_row_hash(&mutated, &prev).expect("hash a mutated record");
        assert_ne!(digest, baseline, "moving `{field}` must move the digest");
    }
}

#[test]
fn the_previous_link_is_in_the_hash() {
    let rec = record("plan/1", None, None);

    assert_ne!(
        audit_row_hash(&rec, &[0_u8; 32]).expect("first"),
        audit_row_hash(&rec, &[1_u8; 32]).expect("second")
    );
}

#[test]
fn the_action_and_the_subject_kind_are_in_the_hash() {
    // The **placement** half: the two tokens occupy their own frames between the
    // actor and the subject reference, so a record hashed without them is a
    // different digest. The *value* half -- that what is framed is the record's
    // own token and not a constant -- is the mutation table above, which is
    // where it belongs: this preimage omits both fields, so on its own it cannot
    // tell the two apart.
    //
    // (This comment used to say "both vocabularies hold one variant today". They
    // hold twelve and seven, and the sentence was the reason the mutation table
    // skipped them.)
    let rec = record("plan/1", None, None);
    let prev = [0_u8; 32];
    let with_tokens = audit_row_hash(&rec, &prev).expect("hash");

    let mut buf = Vec::new();
    buf.extend_from_slice(AUDIT_DOMAIN_SEP);
    super::put_uuid(&mut buf, rec.tenant_id);
    super::put_uuid(&mut buf, rec.chain_id);
    super::put_u64(&mut buf, rec.seq);
    super::put_i64(
        &mut buf,
        crate::domain::instant::timestamp_micros(rec.recorded_at),
    );
    super::put_uuid(&mut buf, rec.actor_principal_id);
    // The two tokens deliberately omitted.
    super::put_str(&mut buf, rec.subject_ref);
    super::put_opt_json(&mut buf, rec.before_state).expect("before");
    super::put_opt_json(&mut buf, rec.after_state).expect("after");
    super::put_opt_uuid(&mut buf, rec.approval_ref);
    super::put_opt_uuid(&mut buf, rec.correlation_id);
    super::put(&mut buf, &prev);
    let without_tokens = super::digest32(&buf);

    assert_ne!(with_tokens, without_tokens);
}
