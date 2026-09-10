// Updated: 2026-09-10 — rewritten for the `tenant_id/value_id` immutable
// value store (ADR-0006); out-of-band seeding withdrawn.
use uuid::Uuid;

use credstore_sdk::{CredStoreError, SecretValue, TenantId, ValueId};

use crate::config::StaticCredStorePluginConfig;

use super::Service;

fn tid() -> TenantId {
    TenantId(Uuid::new_v4())
}
fn vid() -> ValueId {
    ValueId::new_v4()
}

fn svc() -> Service {
    Service::from_config(&StaticCredStorePluginConfig::default()).expect("config builds")
}

#[track_caller]
fn assert_value(v: Option<SecretValue>, expected: &str) {
    assert_eq!(
        v.expect("value present").as_bytes(),
        expected.as_bytes(),
        "secret value mismatch"
    );
}

#[test]
fn starts_empty() {
    let s = svc();
    assert!(s.get_value(&tid(), &vid()).is_none());
}

#[test]
fn put_then_get_roundtrips() {
    let s = svc();
    let (t, v) = (tid(), vid());
    s.put_value(&t, &v, SecretValue::from("hello"))
        .expect("first put");
    assert_value(s.get_value(&t, &v), "hello");
}

#[test]
fn put_is_scoped_to_its_tenant_and_id() {
    let s = svc();
    let (t1, t2, v) = (tid(), tid(), vid());
    s.put_value(&t1, &v, SecretValue::from("t1-val"))
        .expect("put");
    // Same value_id under a different tenant is a distinct entry.
    assert!(s.get_value(&t2, &v).is_none());
    assert!(s.get_value(&t1, &vid()).is_none());
}

#[test]
fn put_on_existing_id_is_conflict() {
    // Immutability guard: the gear never reissues a value_id it already
    // wrote, so a second `put` at the same id is a contract violation the
    // plugin rejects defensively.
    let s = svc();
    let (t, v) = (tid(), vid());
    s.put_value(&t, &v, SecretValue::from("first"))
        .expect("first put succeeds");
    let err = s
        .put_value(&t, &v, SecretValue::from("second"))
        .expect_err("second put at the same id must conflict");
    assert!(matches!(err, CredStoreError::Conflict));
    // The original value is untouched.
    assert_value(s.get_value(&t, &v), "first");
}

#[test]
fn delete_removes_value() {
    let s = svc();
    let (t, v) = (tid(), vid());
    s.put_value(&t, &v, SecretValue::from("v")).expect("put");
    s.delete_value(&t, &v);
    assert!(s.get_value(&t, &v).is_none());
}

#[test]
fn delete_missing_is_noop() {
    let s = svc();
    let (t, v) = (tid(), vid());
    s.delete_value(&t, &v);
    assert!(s.get_value(&t, &v).is_none());
}

#[test]
fn delete_then_put_a_fresh_id_succeeds() {
    // Deleting one version never blocks writing a different (fresh) id —
    // there is no shared key to collide on (ADR-0006).
    let s = svc();
    let t = tid();
    let old = vid();
    s.put_value(&t, &old, SecretValue::from("old"))
        .expect("put old");
    s.delete_value(&t, &old);
    let new = vid();
    s.put_value(&t, &new, SecretValue::from("new"))
        .expect("put new (fresh id) after deleting the old one");
    assert_value(s.get_value(&t, &new), "new");
    assert!(s.get_value(&t, &old).is_none());
}
