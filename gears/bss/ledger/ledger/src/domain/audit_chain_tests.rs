//! Unit tests for the secured-audit canonical encoder ([`super`]):
//! determinism, `before_after` key-order independence, prev_hash sensitivity,
//! and genesis tenant-binding.

#![allow(clippy::doc_markdown)]

use serde_json::json;
use uuid::Uuid;

use crate::domain::instant::from_unix;

use super::{AuditHashInput, audit_genesis_prev_hash};

/// Test helper: hash and unwrap. `audit_row_hash` is fallible only on a
/// `before_after` serialize failure, which is unreachable for the in-memory
/// `Value`s these tests build.
fn h(input: &AuditHashInput, prev: &[u8; 32]) -> [u8; 32] {
    super::audit_row_hash(input, prev).expect("canonicalize")
}

fn input(before_after: &serde_json::Value) -> AuditHashInput<'_> {
    AuditHashInput {
        audit_id: Uuid::from_u128(1),
        tenant_id: Uuid::from_u128(2),
        event_type: "metadata-change",
        actor_ref: Some("actor-7"),
        reason_code: Some("rc-1"),
        correlation_id: Some(Uuid::from_u128(9)),
        at_utc: from_unix(1_750_000_000, 0).unwrap(),
        before_after,
    }
}

const PREV: [u8; 32] = [7u8; 32];

#[test]
fn deterministic() {
    let ba = json!({"a": 1, "b": 2});
    assert_eq!(h(&input(&ba), &PREV), h(&input(&ba), &PREV));
}

/// The same JSON content written with the keys in a different source order must
/// hash identically — the canonical encoder sorts object keys, so the canonical
/// bytes are order-independent (regardless of the `preserve_order` feature).
#[test]
fn before_after_key_order_independent() {
    let a = json!({"alpha": 1, "beta": 2, "gamma": 3});
    let b = json!({"gamma": 3, "beta": 2, "alpha": 1});
    assert_eq!(h(&input(&a), &PREV), h(&input(&b), &PREV));
}

/// A different `before_after` VALUE must change the hash.
#[test]
fn before_after_value_sensitive() {
    let a = json!({"k": 1});
    let b = json!({"k": 2});
    assert_ne!(h(&input(&a), &PREV), h(&input(&b), &PREV));
}

#[test]
fn prev_hash_sensitive() {
    let ba = json!({});
    assert_ne!(h(&input(&ba), &[1u8; 32]), h(&input(&ba), &[2u8; 32]));
}

#[test]
fn event_type_sensitive() {
    let ba = json!({});
    let mut other = input(&ba);
    other.event_type = "erasure";
    assert_ne!(h(&input(&ba), &PREV), h(&other, &PREV));
}

/// `None` actor and an empty-string actor must hash differently (NULL-safety).
#[test]
fn actor_none_vs_empty_string() {
    let ba = json!({});
    let mut none_actor = input(&ba);
    none_actor.actor_ref = None;
    let mut empty_actor = input(&ba);
    empty_actor.actor_ref = Some("");
    assert_ne!(h(&none_actor, &PREV), h(&empty_actor, &PREV));
}

/// Encoding guard: the canonical encoder emits object keys SORTED regardless of
/// whether another workspace crate enabled serde_json's `preserve_order` feature
/// (which makes `serde_json::Map` insertion-ordered). The audit chain's
/// byte-reproducibility must not depend on that global toggle, so we assert the
/// encoder itself canonicalizes rather than assuming the default map ordering.
#[test]
fn canonical_json_sorts_keys_regardless_of_preserve_order() {
    let v = json!({ "b": 1, "a": 2, "c": 3 });
    let bytes = super::canonical_json(&v).expect("canonicalize");
    assert_eq!(
        String::from_utf8(bytes).expect("utf8"),
        r#"{"a":2,"b":1,"c":3}"#,
        "canonical_json must sort object keys independent of preserve_order"
    );
}

#[test]
fn genesis_is_tenant_bound() {
    assert_ne!(
        audit_genesis_prev_hash(Uuid::from_u128(1)),
        audit_genesis_prev_hash(Uuid::from_u128(2))
    );
}

/// The byte-reproducibility vector for the audit chain (mirrors
/// `chain_tests.rs::byte_reproducibility_vector`). Pins the exact hash of a
/// fixed `AuditHashInput` so an UNINTENDED encoding drift (a reordered/added
/// field, a changed [`super::AUDIT_DOMAIN_SEP`]) breaks CI — which the
/// `deterministic` test (same input twice in one build) cannot catch. The audit
/// chain backs the secured-audit store's tamper evidence, so a silent drift
/// would invalidate every persisted `row_hash`. REGENERATE only on an
/// intentional encoding change: run once, paste the printed hex into `EXPECTED`.
#[test]
fn audit_byte_reproducibility_vector() {
    use std::fmt::Write as _;
    const EXPECTED: &str = "6ff8e36cd242da3ed8d1764d819d34fcc8246027b3a24b609a272cff153c0bb1";
    let ba = json!({"k": 1});
    let digest = h(&input(&ba), &[0u8; 32]);
    let mut hex = String::with_capacity(64);
    for b in digest {
        let _ = write!(hex, "{b:02x}");
    }
    assert_eq!(hex, EXPECTED, "audit-chain encoding changed — got {hex}");
}
