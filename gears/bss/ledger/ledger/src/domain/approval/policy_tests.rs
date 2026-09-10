//! Unit tests for the pure dual-control threshold policy.

use super::*;
use crate::domain::instant::utc_ymd_hms;
use time::OffsetDateTime;

fn date(y: i32, m: u32, d: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, m, d).unwrap()
}

fn ts(y: i32, m: u32, d: u32) -> OffsetDateTime {
    utc_ymd_hms(y, m, d, 0, 0, 0)
}

fn version(eff: OffsetDateTime, version: i64, d2: i64, a6: i32) -> PolicyVersion {
    PolicyVersion {
        effective_from: eff,
        version,
        policy: DualControlPolicy {
            d2_threshold_minor: d2,
            a6_backdating_biz_days: a6,
            pending_ttl_seconds: DEFAULT_PENDING_TTL_SECONDS,
        },
    }
}

#[test]
fn resolve_empty_yields_ratified_defaults() {
    assert_eq!(
        resolve_policy(&[], ts(2026, 6, 25)),
        DualControlPolicy::DEFAULT
    );
}

#[test]
fn resolve_picks_latest_effective_from() {
    let versions = [
        version(ts(2026, 1, 1), 1, 50_000, 5),
        version(ts(2026, 6, 1), 2, 200_000, 10),
    ];
    let p = resolve_policy(&versions, ts(2026, 6, 25));
    assert_eq!(p.d2_threshold_minor, 200_000);
    assert_eq!(p.a6_backdating_biz_days, 10);
}

#[test]
fn resolve_breaks_effective_tie_on_highest_version() {
    let versions = [
        version(ts(2026, 6, 1), 1, 50_000, 5),
        version(ts(2026, 6, 1), 2, 300_000, 7),
    ];
    let p = resolve_policy(&versions, ts(2026, 6, 25));
    assert_eq!(p.d2_threshold_minor, 300_000);
}

#[test]
fn resolve_ignores_not_yet_effective_versions() {
    let versions = [
        version(ts(2026, 1, 1), 1, 50_000, 5),
        version(ts(2026, 12, 1), 2, 999_999, 30),
    ];
    let p = resolve_policy(&versions, ts(2026, 6, 25));
    assert_eq!(
        p.d2_threshold_minor, 50_000,
        "future version must not apply"
    );
}

fn amount_op(kind: ApprovalKind, usd_eq_minor: Option<i64>) -> OperationFacts {
    OperationFacts {
        kind,
        amount_usd_eq_minor: usd_eq_minor,
        effective_at: None,
        has_outstanding_balance: false,
    }
}

#[test]
fn amount_kinds_gate_at_or_above_threshold() {
    let policy = DualControlPolicy::DEFAULT; // d2 = 100_000
    let today = date(2026, 6, 25);
    for kind in [
        ApprovalKind::Reverse,
        ApprovalKind::CreditGrant,
        ApprovalKind::ChargebackLoss,
        ApprovalKind::RecognitionScheduleChange,
        // A refund shares the SAME D2 row as the other money-out kinds (Group D).
        ApprovalKind::Refund,
        // A governed manual adjustment shares the SAME D2 row (Group 5 / Phase 3).
        ApprovalKind::ManualAdjustment,
        // Credit + debit notes share the SAME D2 row (Slice 3 §5 D1–D2, Z6-1).
        ApprovalKind::CreditNote,
        ApprovalKind::DebitNote,
    ] {
        // At the threshold → gated (>=).
        assert!(requires_dual_control(
            &amount_op(kind, Some(100_000)),
            policy,
            today
        ));
        // Just below → single-actor.
        assert!(!requires_dual_control(
            &amount_op(kind, Some(99_999)),
            policy,
            today
        ));
        // Well above → gated.
        assert!(requires_dual_control(
            &amount_op(kind, Some(5_000_000)),
            policy,
            today
        ));
        // No amount known → not gated by amount.
        assert!(!requires_dual_control(
            &amount_op(kind, None),
            policy,
            today
        ));
    }
}

#[test]
fn material_backdating_gates_beyond_a6_window() {
    let policy = DualControlPolicy::DEFAULT; // a6 = 5 business days
    let backdate = |eff: NaiveDate, today: NaiveDate| {
        requires_dual_control(
            &OperationFacts {
                kind: ApprovalKind::MaterialBackdating,
                amount_usd_eq_minor: None,
                effective_at: Some(eff),
                has_outstanding_balance: false,
            },
            policy,
            today,
        )
    };
    // 2024-01-01 is a Monday. Exactly 5 business days later is Mon 2024-01-08 →
    // at the window, NOT beyond → single-actor.
    assert!(!backdate(date(2024, 1, 1), date(2024, 1, 8)));
    // One more business day (Tue 2024-01-09) → 6 > 5 → gated.
    assert!(backdate(date(2024, 1, 1), date(2024, 1, 9)));
    // Same day → 0 business days → not gated.
    assert!(!backdate(date(2024, 1, 9), date(2024, 1, 9)));
}

#[test]
fn payer_closure_gated_only_with_outstanding_balance() {
    let policy = DualControlPolicy::DEFAULT;
    let today = date(2026, 6, 25);
    let with_balance = OperationFacts {
        kind: ApprovalKind::PayerClosure,
        amount_usd_eq_minor: None,
        effective_at: None,
        has_outstanding_balance: true,
    };
    let clean = OperationFacts {
        has_outstanding_balance: false,
        ..with_balance
    };
    assert!(requires_dual_control(&with_balance, policy, today));
    assert!(!requires_dual_control(&clean, policy, today));
}

#[test]
fn period_reopen_is_always_gated() {
    let policy = DualControlPolicy::DEFAULT;
    let op = OperationFacts {
        kind: ApprovalKind::PeriodReopen,
        amount_usd_eq_minor: None,
        effective_at: None,
        has_outstanding_balance: false,
    };
    assert!(requires_dual_control(&op, policy, date(2026, 6, 25)));
}

#[test]
fn validate_config_accepts_in_range_and_rejects_out_of_range() {
    // In range (the ratified defaults + bounds).
    assert!(validate_config(100_000, 5, DEFAULT_PENDING_TTL_SECONDS).is_ok());
    assert!(validate_config(D2_MIN_MINOR, A6_MIN_DAYS, 1).is_ok());
    assert!(validate_config(D2_MAX_MINOR, A6_MAX_DAYS, 1).is_ok());
    // Out of range → rejected, no clamp.
    assert_eq!(
        validate_config(D2_MIN_MINOR - 1, 5, 1),
        Err(PolicyConfigError::D2OutOfRange(D2_MIN_MINOR - 1))
    );
    assert_eq!(
        validate_config(D2_MAX_MINOR + 1, 5, 1),
        Err(PolicyConfigError::D2OutOfRange(D2_MAX_MINOR + 1))
    );
    assert_eq!(
        validate_config(100_000, 0, 1),
        Err(PolicyConfigError::A6OutOfRange(0))
    );
    assert_eq!(
        validate_config(100_000, 31, 1),
        Err(PolicyConfigError::A6OutOfRange(31))
    );
    assert_eq!(
        validate_config(100_000, 5, 0),
        Err(PolicyConfigError::TtlNotPositive(0))
    );
}

#[test]
fn business_days_skips_weekends() {
    // Mon 2024-01-01 → Mon 2024-01-08 spans one weekend → 5 business days.
    assert_eq!(business_days_between(date(2024, 1, 1), date(2024, 1, 8)), 5);
    // Backwards / same day → 0.
    assert_eq!(business_days_between(date(2024, 1, 8), date(2024, 1, 1)), 0);
    assert_eq!(business_days_between(date(2024, 1, 1), date(2024, 1, 1)), 0);
    // Fri 2024-01-05 → Mon 2024-01-08: Sat/Sun skipped, only Mon counts → 1.
    assert_eq!(business_days_between(date(2024, 1, 5), date(2024, 1, 8)), 1);
}

#[test]
fn effective_version_returns_the_row_in_force() {
    let versions = [
        version(ts(2026, 6, 1), 1, 50_000, 5),
        version(ts(2026, 6, 20), 2, 200_000, 7),
    ];
    let v = effective_version(&versions, ts(2026, 6, 25)).expect("a version is in force");
    assert_eq!(v.version, 2);
    assert_eq!(v.policy.d2_threshold_minor, 200_000);
    assert_eq!(v.effective_from, ts(2026, 6, 20));
}

#[test]
fn effective_version_breaks_effective_tie_on_highest_version() {
    let versions = [
        version(ts(2026, 6, 20), 1, 50_000, 5),
        version(ts(2026, 6, 20), 2, 200_000, 7),
    ];
    let v = effective_version(&versions, ts(2026, 6, 25)).expect("a version is in force");
    assert_eq!(v.version, 2);
    assert_eq!(v.policy.d2_threshold_minor, 200_000);
}

#[test]
fn effective_version_none_when_no_row_applies() {
    // No rows at all → None (the caller falls back to the platform defaults).
    assert!(effective_version(&[], ts(2026, 6, 25)).is_none());
    // A not-yet-effective row does not apply.
    let future = [version(ts(2026, 7, 1), 1, 50_000, 5)];
    assert!(effective_version(&future, ts(2026, 6, 25)).is_none());
}

#[test]
fn effective_version_agrees_with_resolve_policy() {
    let versions = [
        version(ts(2026, 6, 1), 1, 50_000, 5),
        version(ts(2026, 6, 20), 2, 200_000, 7),
    ];
    let now = ts(2026, 6, 25);
    // resolve_policy is effective_version's thresholds, defaults when none.
    assert_eq!(
        resolve_policy(&versions, now),
        effective_version(&versions, now).expect("in force").policy
    );
    assert_eq!(resolve_policy(&[], now), DualControlPolicy::DEFAULT);
}
