//! Pure period-id math: derive the current `period_id` from an instant and the
//! next `period_id` from a `YYYYMM` string. No `chrono-tz` (decision 1) — the
//! next period is plain integer arithmetic on `YYYYMM`. Free functions only (no
//! `#[domain_model]`), no infrastructure imports (DE0301).

use time::OffsetDateTime;

/// Derive the `period_id` for a UTC instant as `"%Y%m"` (`YYYYMM`).
#[must_use]
pub fn period_id_for(now: OffsetDateTime) -> String {
    crate::domain::instant::yyyymm(now)
}

/// Increment a `YYYYMM` `period_id` by one month (December rolls into the next
/// January). Returns `None` when the input is not a valid 6-character `YYYYMM`
/// (month in `1..=12`). Pure integer arithmetic — no chrono date math.
#[must_use]
pub fn next_period_id(period_id: &str) -> Option<String> {
    if period_id.len() != 6 {
        return None;
    }
    let year: i32 = period_id.get(0..4)?.parse().ok()?;
    let month: u32 = period_id.get(4..6)?.parse().ok()?;
    if !(1..=12).contains(&month) {
        return None;
    }
    let (next_year, next_month) = if month == 12 {
        (year + 1, 1)
    } else {
        (year, month + 1)
    };
    Some(format!("{next_year:04}{next_month:02}"))
}

/// Advance a `YYYYMM` `period_id` by `k` whole months (`k == 0` returns the
/// input unchanged after validation). Returns `None` when the input is not a
/// valid 6-character `YYYYMM`. Pure integer arithmetic on the month-of-epoch
/// (`year * 12 + (month - 1)`), so it stays consistent with
/// [`next_period_id`] (`period_id_plus(p, 1) == next_period_id(p)`) without a
/// chrono date — the recognition `ScheduleBuilder` uses it to lay out the N
/// consecutive fiscal periods of a straight-line schedule from its first
/// period.
#[must_use]
pub fn period_id_plus(period_id: &str, k: u32) -> Option<String> {
    if period_id.len() != 6 {
        return None;
    }
    let year: i32 = period_id.get(0..4)?.parse().ok()?;
    let month: u32 = period_id.get(4..6)?.parse().ok()?;
    if !(1..=12).contains(&month) {
        return None;
    }
    // Months since year 0 (0-based month), then add k and re-split. `i64`
    // arithmetic so a large `k` (capped by the segment ceiling) can never
    // overflow the intermediate.
    let total = i64::from(year) * 12 + i64::from(month - 1) + i64::from(k);
    let next_year = total.div_euclid(12);
    let next_month = total.rem_euclid(12) + 1;
    Some(format!("{next_year:04}{next_month:02}"))
}

/// Decrement a `YYYYMM` `period_id` by one month (January rolls back into the
/// previous December). Returns `None` when the input is not a valid 6-character
/// `YYYYMM`. The revaluation job reverses the period immediately preceding the
/// current open one with this.
#[must_use]
pub fn previous_period_id(period_id: &str) -> Option<String> {
    if period_id.len() != 6 {
        return None;
    }
    let year: i32 = period_id.get(0..4)?.parse().ok()?;
    let month: u32 = period_id.get(4..6)?.parse().ok()?;
    if !(1..=12).contains(&month) {
        return None;
    }
    let (prev_year, prev_month) = if month == 1 {
        (year - 1, 12)
    } else {
        (year, month - 1)
    };
    Some(format!("{prev_year:04}{prev_month:02}"))
}

/// The UTC instant at which `period_id` (`YYYYMM`) BEGINS — `YYYY-MM-01T00:00:00Z`.
/// Pure UTC month arithmetic (decision 1 — no `chrono-tz`); returns `None` when
/// the input is not a valid 6-character `YYYYMM`.
#[must_use]
pub fn period_start_utc(period_id: &str) -> Option<OffsetDateTime> {
    if period_id.len() != 6 {
        return None;
    }
    let year: i32 = period_id.get(0..4)?.parse().ok()?;
    let month: u32 = period_id.get(4..6)?.parse().ok()?;
    if !(1..=12).contains(&month) {
        return None;
    }
    crate::domain::instant::try_utc_ymd_hms(year, month, 1, 0, 0, 0)
}

/// The UTC instant at which `period_id` (`YYYYMM`) ENDS — the first instant of
/// the following month (`period_start_utc(next_period_id(period_id))`). The
/// unrealized-revaluation run uses this as the period-end `as_of` for the rate
/// resolve (the rate in effect at period close, design §4.5). Returns `None`
/// when the input is not a valid 6-character `YYYYMM`.
#[must_use]
pub fn period_end_utc(period_id: &str) -> Option<OffsetDateTime> {
    period_start_utc(&next_period_id(period_id)?)
}

#[cfg(test)]
#[path = "period_tests.rs"]
mod period_tests;
