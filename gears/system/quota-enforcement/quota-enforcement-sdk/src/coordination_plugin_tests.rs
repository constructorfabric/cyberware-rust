use std::time::Duration;

use time::OffsetDateTime;
use uuid::Uuid;

use super::{CoordinationError, Lock, LockScope};

#[test]
fn lock_scope_keys_are_distinct_and_stable() {
    let keys: Vec<&str> = LockScope::ALL.iter().map(|s| s.key()).collect();
    assert_eq!(keys, vec!["lease_sweeper", "retention_sweeper"]);
    assert_eq!(LockScope::LeaseSweeper.to_string(), "lease_sweeper");
}

#[test]
fn lock_scope_serde_uses_snake_case_and_rejects_unknown() {
    let json = serde_json::to_string(&LockScope::RetentionSweeper).expect("serialize");
    assert_eq!(json, "\"retention_sweeper\"");
    let back: LockScope = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back, LockScope::RetentionSweeper);
    let err = serde_json::from_str::<LockScope>("\"outbox\"");
    assert!(err.is_err(), "unknown scope must be rejected");
}

#[test]
fn lock_renew_by_is_one_third_of_ttl_after_acquisition() {
    let acquired = OffsetDateTime::from_unix_timestamp(1_700_000_000).expect("timestamp");
    let lock = Lock::new(
        LockScope::LeaseSweeper,
        Uuid::now_v7(),
        Duration::from_secs(90),
        acquired,
    );
    assert_eq!(lock.renew_by(), acquired + Duration::from_secs(30));
    assert_eq!(lock.ttl(), Duration::from_secs(90));
    assert_eq!(lock.scope(), LockScope::LeaseSweeper);
}

#[test]
fn coordination_error_messages_name_the_scope() {
    let conflict = CoordinationError::Conflict {
        scope: LockScope::LeaseSweeper,
    };
    assert!(conflict.to_string().contains("lease_sweeper"));
    let expired = CoordinationError::LockExpired {
        scope: LockScope::RetentionSweeper,
    };
    assert!(expired.to_string().contains("retention_sweeper"));
    assert_ne!(conflict, expired);
}
