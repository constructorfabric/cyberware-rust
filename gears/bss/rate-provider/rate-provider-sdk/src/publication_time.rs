//! Guard on a fetched document's own publication timestamp.
//!
//! The ledger decides whether a stored rate may still be used by its **age** —
//! `now - as_of` against a staleness window. That check only protects against
//! timestamps in the past. A document dated in the future has a negative age, so
//! it reads as permanently fresh: a feed that published `as_of` a month ahead
//! would keep the ledger posting at that rate long after the real one moved, and
//! no staleness alarm would fire.
//!
//! Validation belongs to the **source plugin**, not the ledger: by then a rate is
//! just a row in the store, and the ledger cannot tell "the feed published a bad
//! timestamp" apart from "we have not synced in a while". Rejecting here is also
//! fail-safe — the document becomes an `Err`, so the composite falls through to
//! the next source instead of storing the bad value.

use bss_ledger_sdk::RateProviderError;
use time::{Duration, OffsetDateTime};

/// How far ahead of our own clock a publication timestamp may legitimately sit.
///
/// Derived from the two ways an honest feed can read "ahead" of us:
///
/// - **14 h — timezone.** A date-only feed (ECB publishes `2026-07-30`, no time)
///   is anchored at 00:00:00 UTC by its plugin. If the publisher's civil date is
///   ahead of UTC's, that anchor can land ahead of our clock; UTC+14 (Kiritimati)
///   is the widest civil offset in use.
/// - **12 h — host clock drift.** A generous allowance for a badly-synchronised
///   host, so a local NTP failure degrades into wrong-but-served rather than a
///   feed-wide outage.
///
/// 26 h is permissive enough that no correctly-published document is rejected,
/// while still catching a timestamp far enough ahead to outlive the ledger's
/// staleness window.
pub const MAX_FUTURE_SKEW_HOURS: i64 = 26;

/// Reject a document whose publication time runs more than
/// [`MAX_FUTURE_SKEW_HOURS`] ahead of `now`.
///
/// `now` is a parameter rather than an internal `OffsetDateTime::now_utc()` so
/// the bound is directly testable; callers on the fetch path pass
/// `OffsetDateTime::now_utc()`.
///
/// # Errors
/// [`RateProviderError::Internal`] naming both timestamps when `as_of` is beyond
/// the bound — matching every other parse-stage rejection in the source plugins,
/// and keeping a misbehaving feed visible on `fx_provider_fetch_errors_total`.
pub fn reject_future_publication_time(
    as_of: OffsetDateTime,
    now: OffsetDateTime,
    provider_id: &str,
) -> Result<(), RateProviderError> {
    let bound = now + Duration::hours(MAX_FUTURE_SKEW_HOURS);
    if as_of <= bound {
        return Ok(());
    }
    tracing::warn!(
        provider = provider_id,
        as_of = %as_of,
        bound = %bound,
        "rate-provider source: rejecting a document published beyond the accepted clock skew"
    );
    Err(RateProviderError::Internal(format!(
        "publication time {as_of} is more than {MAX_FUTURE_SKEW_HOURS}h ahead of {now}"
    )))
}

#[cfg(test)]
#[path = "publication_time_tests.rs"]
mod tests;
