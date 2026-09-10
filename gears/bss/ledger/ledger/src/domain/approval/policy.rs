//! Pure dual-control threshold policy (§4.2). Given the tenant's effective-dated
//! policy versions and an operation's facts, decide whether a governed mutation
//! must go through the preparer→approver flow; and validate tenant config against
//! the ratified ranges. No FX and no clock here — the caller passes the
//! USD-equivalent (computed with the *operation's own* rate snapshot, DC10) and
//! the current date, so the whole module is deterministic and unit-testable.

use chrono::{Datelike, NaiveDate, Weekday};
use toolkit_macros::domain_model;

use super::ApprovalKind;
use time::OffsetDateTime;

/// Ratified platform defaults applied when a tenant has no policy row:
/// D2 = 1000 USD (scale 2) = `100_000` minor (DECISIONS D-1); A6 = 5 business days
/// (foundation §1.4); pending TTL = 7 days (DC12).
pub const DEFAULT_D2_THRESHOLD_MINOR: i64 = 100_000;
pub const DEFAULT_A6_BACKDATING_BIZ_DAYS: i32 = 5;
pub const DEFAULT_PENDING_TTL_SECONDS: i64 = 7 * 24 * 60 * 60;

/// D2 tenant-config bounds in USD-eq minor: [100 .. 1,000,000] USD (DECISIONS D-1).
pub const D2_MIN_MINOR: i64 = 10_000;
pub const D2_MAX_MINOR: i64 = 100_000_000;
/// A6 tenant-config bounds in business days: [1 .. 30] (foundation §1.4).
pub const A6_MIN_DAYS: i32 = 1;
pub const A6_MAX_DAYS: i32 = 30;

/// The resolved thresholds in effect for a tenant at a point in time.
#[domain_model]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DualControlPolicy {
    pub d2_threshold_minor: i64,
    pub a6_backdating_biz_days: i32,
    pub pending_ttl_seconds: i64,
}

impl DualControlPolicy {
    /// The ratified platform defaults (used when a tenant has no policy row).
    pub const DEFAULT: Self = Self {
        d2_threshold_minor: DEFAULT_D2_THRESHOLD_MINOR,
        a6_backdating_biz_days: DEFAULT_A6_BACKDATING_BIZ_DAYS,
        pending_ttl_seconds: DEFAULT_PENDING_TTL_SECONDS,
    };
}

/// One effective-dated policy version (a `ledger_dual_control_policy` row).
#[domain_model]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PolicyVersion {
    pub effective_from: OffsetDateTime,
    pub version: i64,
    pub policy: DualControlPolicy,
}

/// Rejected tenant policy config (out of the ratified range — no silent clamp).
#[domain_model]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PolicyConfigError {
    D2OutOfRange(i64),
    A6OutOfRange(i32),
    TtlNotPositive(i64),
}

/// The facts a threshold check needs. This module only COMPARES — it does no FX.
///
/// **DC10 / FX.** `amount_usd_eq_minor` is the operation's amount valued in the
/// threshold (FUNCTIONAL / reporting) currency. Callers pass the operation's
/// TRANSACTION-currency minor; the dual-control gate (`ApprovalService::gate`)
/// translates it to the tenant's functional currency at the current rate before
/// this module compares — reading the operation currency off the `ApprovalIntent`
/// (`ApprovalIntent::transaction_currency`). A single-currency tenant (or a
/// same-currency op) compares unchanged. This module itself does NO FX — it only
/// compares. Residual: `Reverse` / `RecognitionScheduleChange` derive their
/// comparand at gate time and carry no currency on the stored intent, so they keep
/// the transaction-currency comparand (single-currency-correct) until the currency
/// rides those intents — for those kinds the threshold compares transaction-currency
/// minor, which is exact only while the tenant is single-currency.
#[domain_model]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OperationFacts {
    pub kind: ApprovalKind,
    /// The D2 comparand for amount-gated kinds (reverse / credit-grant / chargeback
    /// / recognition-schedule-change); `None` for non-amount kinds. Transaction-
    /// currency minor today; functional/USD-eq once the FX slice lands (type doc).
    pub amount_usd_eq_minor: Option<i64>,
    /// For `MaterialBackdating`: the effective date of the backdated post.
    pub effective_at: Option<NaiveDate>,
    /// For `PayerClosure`: whether the payer holds a non-zero AR or a positive
    /// customer balance at closure.
    pub has_outstanding_balance: bool,
}

/// The policy *version* in effect at `now`: the row with the greatest
/// `effective_from <= now`, highest `version` on a tie; `None` when no row
/// applies (the caller then falls back to the ratified
/// [`DualControlPolicy::DEFAULT`]). Carries the `version` + `effective_from`
/// provenance the read surface renders — [`resolve_policy`] keeps only the
/// resolved thresholds.
#[must_use]
pub fn effective_version(versions: &[PolicyVersion], now: OffsetDateTime) -> Option<PolicyVersion> {
    versions
        .iter()
        .filter(|v| v.effective_from <= now)
        .max_by(|a, b| {
            a.effective_from
                .cmp(&b.effective_from)
                .then(a.version.cmp(&b.version))
        })
        .copied()
}

/// Resolve the policy in effect at `now`: the row with the greatest
/// `effective_from <= now`, highest `version` on a tie; the ratified defaults
/// when none applies.
#[must_use]
pub fn resolve_policy(versions: &[PolicyVersion], now: OffsetDateTime) -> DualControlPolicy {
    effective_version(versions, now).map_or(DualControlPolicy::DEFAULT, |v| v.policy)
}

/// Validate a tenant policy config against the ratified ranges (DECISIONS D-1,
/// foundation §1.4). Out-of-range is **rejected** — never clamped (DC9/DC11).
///
/// # Errors
/// [`PolicyConfigError`] when D2, A6, or the TTL is outside its allowed range.
pub fn validate_config(
    d2_threshold_minor: i64,
    a6_backdating_biz_days: i32,
    pending_ttl_seconds: i64,
) -> Result<(), PolicyConfigError> {
    if !(D2_MIN_MINOR..=D2_MAX_MINOR).contains(&d2_threshold_minor) {
        return Err(PolicyConfigError::D2OutOfRange(d2_threshold_minor));
    }
    if !(A6_MIN_DAYS..=A6_MAX_DAYS).contains(&a6_backdating_biz_days) {
        return Err(PolicyConfigError::A6OutOfRange(a6_backdating_biz_days));
    }
    if pending_ttl_seconds <= 0 {
        return Err(PolicyConfigError::TtlNotPositive(pending_ttl_seconds));
    }
    Ok(())
}

/// Decide whether `op` must go through dual-control under `policy`, given the
/// current date `today` (for the A6 backdating window). Per-policy (DC: only over
/// threshold) — below threshold the caller proceeds single-actor.
#[must_use]
pub fn requires_dual_control(
    op: &OperationFacts,
    policy: DualControlPolicy,
    today: NaiveDate,
) -> bool {
    match op.kind {
        // Amount-gated: USD-eq at or above the D2 threshold. A recognition schedule
        // change/cancel is sized by the un-recognized deferred remainder it re-plans
        // or strands (the gate reads it from the schedule, like `Reverse`).
        ApprovalKind::Reverse
        | ApprovalKind::CreditGrant
        | ApprovalKind::ChargebackLoss
        | ApprovalKind::RecognitionScheduleChange
        // A refund / credit-note is money-OUT; the D2 threshold gates it on the
        // cash returned (design §1.4 D2 — refunds & credit-notes above the
        // tenant-configurable amount need preparer/approver). Reuses the SAME D2
        // policy row as the other amount-gated kinds (no separate refund threshold).
        | ApprovalKind::Refund
        // A governed manual adjustment is a money-affecting governed posting; gated
        // on the gross adjustment amount (Σ DR == Σ CR), the SAME D2 row as the other
        // amount-gated kinds (no separate manual-adjustment threshold).
        | ApprovalKind::ManualAdjustment
        // A credit note is money-OUT (reduces AR / recognized revenue / seeds a
        // refundable wallet); a debit note is a money-affecting additional charge
        // that can book fresh revenue + a recognition schedule outside the normal
        // invoice flow. Both are material adjustments to a posted invoice, gated on
        // the note amount against the SAME D2 row (design §5 D1–D2). No separate
        // note threshold.
        | ApprovalKind::CreditNote
        | ApprovalKind::DebitNote => op
            .amount_usd_eq_minor
            // Gate on magnitude (VHP-1855 #11): a negative USD-eq amount must not
            // slip below the threshold and skip the gate. Defensive — the
            // amount-bearing kinds reject a non-positive amount upstream
            // (`build_grant_entry`, the dispute `CHECK`; the derived Reverse /
            // RecognitionScheduleChange amounts are `>= 0`) — but the gate must not
            // depend on those guards holding.
            .is_some_and(|amount| amount.saturating_abs() >= policy.d2_threshold_minor),
        // Open-period material backdating: effective date older than A6 biz days.
        ApprovalKind::MaterialBackdating => op
            .effective_at
            .is_some_and(|eff| business_days_between(eff, today) > policy.a6_backdating_biz_days),
        // Closing a payer that still holds a balance always needs sign-off.
        ApprovalKind::PayerClosure => op.has_outstanding_balance,
        // Reopening a closed fiscal period is always dual-control (Slice 7 prep).
        ApprovalKind::PeriodReopen => true,
    }
}

/// Count business days (Mon–Fri) after `from` up to and including `to`. `0` when
/// `from >= to`. MVP uses the Mon–Fri weekday rule; tenant holiday calendars
/// (foundation AC #20) are a follow-up.
#[must_use]
pub fn business_days_between(from: NaiveDate, to: NaiveDate) -> i32 {
    let mut count = 0;
    let mut day = from;
    while day < to {
        // `succ_opt` only returns `None` at the maximum representable date, far
        // outside any fiscal-period range — saturate there rather than panic.
        let Some(next) = day.succ_opt() else { break };
        day = next;
        if !matches!(day.weekday(), Weekday::Sat | Weekday::Sun) {
            count += 1;
        }
    }
    count
}

#[cfg(test)]
#[path = "policy_tests.rs"]
mod tests;
