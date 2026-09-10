//! Layer-1 unit tests for [`crate::wait`] — TESTING.md §2, `wait.rs` row.

use cluster_sdk::ClusterError;

use super::WaitPolicy;

#[test]
fn no_wait_replicas_is_no_policy_at_all() {
    // Whatever the timeout says: `wait_replicas` is the switch, and
    // `wait_timeout_ms` carries a default even when nothing reads it.
    assert_eq!(
        WaitPolicy::from_config(None, 1_000).expect("an absent policy cannot fail"),
        WaitPolicy::Disabled
    );
}

#[test]
fn a_configured_policy_carries_both_operator_values() {
    let policy = WaitPolicy::from_config(Some(2), 2_500).expect("2500 ms fits in an i64");
    let WaitPolicy::Enabled(target) = policy else {
        panic!("a configured `wait_replicas` must enable the policy, got {policy:?}");
    };
    // Rendered rather than destructured: the fields are private precisely so
    // nothing outside `wait.rs` reads them, and the `WAIT` line is what a short
    // count reports back to the operator.
    assert_eq!(
        format!("{target:?}"),
        "WaitTarget { replicas: 2, timeout_ms: 2500 }"
    );
}

#[test]
fn a_wait_timeout_too_large_for_wait_fails_startup_rather_than_clamping() {
    // The failure this type exists to make impossible: `WAIT`'s timeout argument
    // is signed, so a `u64` past `i64::MAX` has no honest rendering. Clamping it
    // would turn an unreadable config into `WAIT 1 9223372036854775807` — a
    // ~292-million-year deadline nobody asked for and no operator would see.
    let rejected = WaitPolicy::from_config(Some(1), u64::MAX);
    assert!(
        matches!(
            rejected,
            Err(ClusterError::InvalidConfig { ref reason })
                if reason.contains("wait_timeout_ms") && reason.contains("18446744073709551615")
        ),
        "an unrepresentable wait_timeout_ms must be rejected as config, got {rejected:?}"
    );
}

#[test]
fn the_largest_representable_timeout_is_still_accepted() {
    // The boundary is `i64::MAX`, not something short of it: rejecting a value
    // `WAIT` can express would be its own bug.
    let policy = WaitPolicy::from_config(Some(1), i64::MAX.unsigned_abs());
    assert!(
        matches!(policy, Ok(WaitPolicy::Enabled(_))),
        "i64::MAX ms is representable and must be accepted, got {policy:?}"
    );
}
