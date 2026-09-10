//! `FiscalPeriodGuard` — reads the target `(tenant, legal_entity, period)`
//! row inside the posting transaction and asserts it is `OPEN`; a missing or
//! non-`OPEN` period is [`PeriodError::Closed`].
//!
//! Concurrent close vs post is guarded by `SERIALIZABLE` isolation, not a row
//! lock (`SecureORM` exposes none, and gears can't issue raw `FOR SHARE`). Both
//! [`crate::infra::posting::service::PostingService::post`] and
//! [`crate::infra::period_close::PeriodCloseService::close`] run under a
//! `SERIALIZABLE` transaction with retry: close's pre-close tie-out reads the
//! journal lines a concurrent post writes, so Postgres SSI detects the
//! overlap and aborts the loser, which retries. A close therefore can't certify
//! a period an in-flight entry is landing in — and it's distributed (SSI is
//! DB-enforced across replicas), the same shape AM uses for its workers.

use sea_orm::{ColumnTrait, Condition, EntityTrait, QueryFilter};
use toolkit_db::secure::{AccessScope, DbTx, SecureEntityExt};
use uuid::Uuid;

use crate::domain::status::PERIOD_STATUS_OPEN;
use crate::infra::storage::entity::fiscal_period;
use time::Duration;
use time::OffsetDateTime;

/// Warn band for clock skew between a post's `posted_at_utc` and the server
/// wall clock (design §3.2 `FiscalPeriodGuard`): skew beyond ±15 min raises a
/// `CLOCK_SKEW` Warn alarm but still posts.
const CLOCK_SKEW_WARN_MINUTES: i64 = 15;
/// Reject band (design §3.2): skew beyond ±24 h is quarantined
/// (`CLOCK_SKEW_QUARANTINE`) and must re-submit via the material-backdating
/// exception path.
const CLOCK_SKEW_REJECT_HOURS: i64 = 24;

/// Outcome of the clock-skew gate: how far a post's `posted_at_utc` is from the
/// server wall clock (design §3.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClockSkewVerdict {
    /// Within ±15 min — post normally.
    Ok,
    /// Beyond ±15 min but within ±24 h — post, but raise a `CLOCK_SKEW` Warn
    /// alarm (no rollback).
    Warn,
    /// Beyond ±24 h — reject with `CLOCK_SKEW_QUARANTINE`.
    Reject,
}

/// Classify the clock skew between a post's `posted_at_utc` and `now` (design
/// §3.2): `> ±24 h` rejects, `> ±15 min` warns, otherwise OK. Pure and
/// symmetric (a future- or past-skewed clock is treated identically), so it is
/// unit-tested without a clock.
#[must_use]
pub fn classify_clock_skew(posted_at_utc: OffsetDateTime, now: OffsetDateTime) -> ClockSkewVerdict {
    let skew = (now - posted_at_utc).abs();
    if skew > Duration::hours(CLOCK_SKEW_REJECT_HOURS) {
        ClockSkewVerdict::Reject
    } else if skew > Duration::minutes(CLOCK_SKEW_WARN_MINUTES) {
        ClockSkewVerdict::Warn
    } else {
        ClockSkewVerdict::Ok
    }
}

/// Period-gate outcome error.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PeriodError {
    /// The period is missing or not `OPEN` — posting is refused.
    #[error("fiscal period is closed or absent")]
    Closed,
    /// Underlying storage failure.
    #[error("fiscal period guard db error: {0}")]
    Db(String),
}

/// Guards posting against a closed/absent fiscal period.
#[derive(Clone, Default)]
pub struct FiscalPeriodGuard;

impl FiscalPeriodGuard {
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// Read the `(tenant, legal_entity, period_id)` row and assert it is
    /// `OPEN`.
    ///
    /// # Errors
    /// [`PeriodError::Closed`] if the row is absent or its status is not
    /// `OPEN`; [`PeriodError::Db`] on a storage failure.
    pub async fn pin_open(
        &self,
        txn: &DbTx<'_>,
        tenant: Uuid,
        legal_entity: Uuid,
        period_id: &str,
    ) -> Result<(), PeriodError> {
        let scope = AccessScope::for_tenant(tenant);

        let row = fiscal_period::Entity::find()
            .filter(
                Condition::all()
                    .add(fiscal_period::Column::TenantId.eq(tenant))
                    .add(fiscal_period::Column::LegalEntityId.eq(legal_entity))
                    .add(fiscal_period::Column::PeriodId.eq(period_id)),
            )
            .secure()
            .scope_with(&scope)
            .one(txn)
            .await
            .map_err(|e| PeriodError::Db(format!("pin_open: {e}")))?;

        match row {
            Some(p) if p.status == PERIOD_STATUS_OPEN => Ok(()),
            _ => Err(PeriodError::Closed),
        }
    }
}

#[cfg(test)]
mod tests {

    use super::{ClockSkewVerdict, classify_clock_skew};
    use time::{Duration, OffsetDateTime};

    #[test]
    fn within_fifteen_minutes_is_ok() {
        let now = OffsetDateTime::now_utc();
        assert_eq!(classify_clock_skew(now, now), ClockSkewVerdict::Ok);
        assert_eq!(
            classify_clock_skew(now - Duration::minutes(14), now),
            ClockSkewVerdict::Ok
        );
        // Boundary: exactly 15 min is still OK (strictly-greater warns).
        assert_eq!(
            classify_clock_skew(now - Duration::minutes(15), now),
            ClockSkewVerdict::Ok
        );
    }

    #[test]
    fn beyond_fifteen_minutes_warns_either_direction() {
        let now = OffsetDateTime::now_utc();
        assert_eq!(
            classify_clock_skew(now - Duration::minutes(16), now),
            ClockSkewVerdict::Warn,
        );
        // A future-skewed clock is treated identically to a past-skewed one.
        assert_eq!(
            classify_clock_skew(now + Duration::hours(3), now),
            ClockSkewVerdict::Warn,
        );
        // Boundary: exactly 24 h is still Warn (strictly-greater rejects).
        assert_eq!(
            classify_clock_skew(now - Duration::hours(24), now),
            ClockSkewVerdict::Warn,
        );
    }

    #[test]
    fn beyond_twentyfour_hours_rejects_either_direction() {
        let now = OffsetDateTime::now_utc();
        assert_eq!(
            classify_clock_skew(now - Duration::hours(25), now),
            ClockSkewVerdict::Reject,
        );
        assert_eq!(
            classify_clock_skew(now + Duration::hours(25), now),
            ClockSkewVerdict::Reject,
        );
    }
}
