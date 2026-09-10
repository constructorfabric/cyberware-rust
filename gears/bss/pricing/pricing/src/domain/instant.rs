//! The **millisecond quantum** every authored instant is expressed at (D-144).
//!
//! The gear's treatment of time fixed the zone and left the resolution open —
//! "all effective dating, window boundaries, `grandfatherUntil`,
//! `availableFrom`/`availableTo` and anchor math are UTC" — and an unquantized
//! axis is not a stylistic gap. `cohort` **is** a cutover instant and is matched
//! for **equality** across a gear boundary against a window bound another gear
//! produced (`design/07-pricewindow-linkage.md` §5, D-126): two instants denoting
//! the same moment at different resolutions are not equal, so the generation
//! becomes unfindable by exactly the subscribers grandfathering exists to
//! protect.
//!
//! A finer instant is therefore **refused, never truncated**. Truncation is what
//! an unstated quantum degenerates into, and it moves the instant a scope-key
//! axis, a window bound and an approval-time floor are all derived from — a
//! truncating producer and a non-truncating consumer agree until the day they do
//! not, with no failure in between.
//!
//! The rule is over instants the gear **authors, carries in a contract field,
//! publishes or compares**. Storage bookkeeping an operator never authors —
//! `created_at`, audit-chain and outbox timestamps — is outside it: none of it
//! enters a contract field and none of it is compared with anything.
//!
//! This is [`crate::domain::money`]'s temporal sibling and is shaped like it on
//! purpose: a predicate for callers that only have to decide, and a checked form
//! that names the code for callers that have to refuse.
//!
//! Every instant in the process is a **UTC** one: request and response fields
//! deserialize through [`rfc3339`], which converts at the boundary and refuses an
//! instant with no UTC form; Postgres returns UTC; `now_utc` and [`utc_ymd_hms`]
//! construct it. That is what lets the renderers below call
//! `OffsetDateTime::to_offset(UTC)` — a panicking conversion in `time` — as the
//! identity it is here.

use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::domain::error::DomainError;

/// Nanoseconds in the quantum. One millisecond.
const QUANTUM_NANOS: u32 = 1_000_000;

/// Is `at` expressible at the quantum — i.e. does it carry no precision below
/// one millisecond?
///
/// `time::OffsetDateTime` does not represent leap-second nanoseconds (the
/// nanosecond field is always in `0..1_000_000_000`), so the remainder is
/// taken over that field alone. A leap second authored at whole milliseconds
/// is therefore expressible the same way any other instant is.
#[must_use]
pub fn is_quantized(at: OffsetDateTime) -> bool {
    at.nanosecond().is_multiple_of(QUANTUM_NANOS)
}

/// The authoring-time form of [`is_quantized`].
///
/// `field` is the authored field the instant arrived on (`grandfatherUntil`,
/// `availableFrom`, `cohort`), so the author corrects one value instead of
/// resubmitting a request and guessing which of its instants was refused.
///
/// # Errors
///
/// [`DomainError::TimestampPrecisionExceeded`] when `at` carries precision finer
/// than one millisecond.
pub fn check_quantum(field: &str, at: OffsetDateTime) -> Result<(), DomainError> {
    if is_quantized(at) {
        return Ok(());
    }
    Err(DomainError::TimestampPrecisionExceeded(format!(
        "{field} {} is finer than the millisecond quantum",
        format_rfc3339(at)
    )))
}

/// RFC 3339 rendering: a `Z` designator and a fraction of exactly six digits.
///
/// **One renderer, deliberately.** Three lived here — a millisecond `.SSS`, and
/// two reproducing `chrono`'s `AutoSi` grouping with `Z` and with `+00:00` — and
/// every call site had to pick the one matching whatever `chrono` wrote for that
/// field on main. Nothing reads those bytes any more, so the only thing the
/// choice still bought was the chance of picking wrong, and it was picked wrong
/// on the response fields of four surfaces before this collapsed to one form.
///
/// Six digits, rather than three or `time`'s minimal-digit `Rfc3339`:
///
/// - **Lossless for every population the gear has.** Authored instants sit on
///   the millisecond quantum (D-144), but the audit chain is exempt from it by
///   design — its cursor binds `recorded_at` at the precision Postgres keeps —
///   and the ledger applies no quantum at all. A `.SSS` renderer drops a digit
///   neither of those can recover.
/// - **Exactly `timestamptz`'s resolution**, so an instant rendered before it is
///   stored and the same instant rendered after it is read back spell alike, and
///   a digest taken on either side reproduces. A nanosecond form would not: it
///   prints three digits Postgres never returns.
/// - **Fixed width**, so the rendering orders lexicographically as it orders
///   chronologically. These strings are keys — `subject_ref`, `request_id`, the
///   act labels on outbox events — not only display.
#[must_use]
// The fraction is whole microseconds, so the nanosecond division truncates by
// design: sub-microsecond precision has no representation downstream.
#[allow(clippy::integer_division)]
pub fn format_rfc3339(at: OffsetDateTime) -> String {
    let at = at.to_offset(time::UtcOffset::UTC);
    format!(
        "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{micros:06}Z",
        year = at.year(),
        month = u8::from(at.month()),
        day = at.day(),
        hour = at.hour(),
        minute = at.minute(),
        second = at.second(),
        micros = at.nanosecond() / 1_000
    )
}

/// Microseconds since the Unix epoch. Same integer [`chrono`] produced via
/// `timestamp_micros`, so the audit-chain preimage stays byte-stable.
// Truncating to whole microseconds IS the conversion; a float would introduce
// the imprecision this integer path exists to avoid.
#[allow(clippy::integer_division)]
#[must_use]
pub fn timestamp_micros(at: OffsetDateTime) -> i64 {
    at.unix_timestamp()
        .saturating_mul(1_000_000)
        .saturating_add(i64::from(at.nanosecond() / 1_000))
}

/// Construct a UTC instant from a civil date-time.
///
/// # Panics
///
/// On a civil date-time the calendar does not have (`2026-02-30`, month 13,
/// hour 24). Every in-tree caller passes a literal (or `max_utc`'s), so the panic is a
/// programmer error surfacing at the fixture that wrote it — the same contract
/// `chrono`'s `with_ymd_and_hms(..).unwrap()` had on main. The Unix-epoch
/// fallback this replaced let a mistyped fixture pass vacuously; a value that has
/// not been checked goes through [`try_utc_ymd_hms`].
#[must_use]
#[track_caller]
pub fn utc_ymd_hms(
    year: i32,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
    second: u32,
) -> OffsetDateTime {
    match try_utc_ymd_hms(year, month, day, hour, minute, second) {
        Some(at) => at,
        None => panic!(
            "utc_ymd_hms({year}, {month}, {day}, {hour}, {minute}, {second}) is not a valid \
             civil date-time"
        ),
    }
}

/// [`utc_ymd_hms`] for a value the caller has not range-checked: `None` where
/// the calendar has no such date-time.
#[must_use]
pub fn try_utc_ymd_hms(
    year: i32,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
    second: u32,
) -> Option<OffsetDateTime> {
    use time::{Date, Month, PrimitiveDateTime, Time};

    let month = Month::try_from(u8::try_from(month).ok()?).ok()?;
    let date = Date::from_calendar_date(year, month, u8::try_from(day).ok()?).ok()?;
    let time = Time::from_hms(
        u8::try_from(hour).ok()?,
        u8::try_from(minute).ok()?,
        u8::try_from(second).ok()?,
    )
    .ok()?;
    Some(PrimitiveDateTime::new(date, time).assume_utc())
}

/// Unix timestamp plus nanoseconds, or `None` when the pair is not a real
/// instant. Replaces `chrono::DateTime::from_timestamp`.
#[must_use]
pub fn from_unix(seconds: i64, nanos: u32) -> Option<OffsetDateTime> {
    OffsetDateTime::from_unix_timestamp(seconds)
        .ok()?
        .replace_nanosecond(nanos)
        .ok()
}

/// Unix timestamp in milliseconds, or `None` when the value is not a real
/// instant. Replaces `chrono::DateTime::from_timestamp_millis`.
#[must_use]
pub fn from_unix_millis(millis: i64) -> Option<OffsetDateTime> {
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(millis) * 1_000_000).ok()
}

/// Milliseconds since the Unix epoch. Replaces `chrono`'s `timestamp_millis`.
// As in `timestamp_micros`: whole milliseconds are the unit, not a rounding of
// one.
#[allow(clippy::integer_division)]
#[must_use]
pub fn timestamp_millis(at: OffsetDateTime) -> i64 {
    at.unix_timestamp()
        .saturating_mul(1_000)
        .saturating_add(i64::from(at.nanosecond() / 1_000_000))
}

/// Drop sub-millisecond nanos so the instant sits on the D-144 quantum.
// Flooring onto the D-144 quantum is the whole point of this function.
#[allow(clippy::integer_division)]
#[must_use]
pub fn truncate_millis(at: OffsetDateTime) -> OffsetDateTime {
    let nanos = at.nanosecond() / QUANTUM_NANOS * QUANTUM_NANOS;
    at.replace_nanosecond(nanos).unwrap_or(at)
}

/// `chrono::Duration::try_days` — `None` when `days` does not fit a
/// `time::Duration`.
#[must_use]
pub fn try_days(days: i64) -> Option<time::Duration> {
    days.checked_mul(24)?
        .checked_mul(60)?
        .checked_mul(60)
        .map(time::Duration::seconds)
}

/// The latest civil instant `time` can represent. Stands in for chrono's
/// `DateTime::<Utc>::MAX_UTC` in overflow cases.
#[must_use]
pub fn max_utc() -> OffsetDateTime {
    utc_ymd_hms(9999, 12, 31, 23, 59, 59)
}

/// RFC 3339 `serde` for request and response instants: [`format_rfc3339`] out,
/// `time`'s parser in, with the UTC normalisation `chrono`'s `DateTime<Utc>` did
/// at the boundary.
///
/// `time` keeps the offset a caller wrote and range-checks only the *local*
/// date-time, so `9999-12-31T23:00:00-05:00` deserializes and then panics in
/// [`OffsetDateTime::to_offset`] (`local datetime out of valid range`) at the
/// first render, comparison key or database bind — a 500 where `chrono` answered
/// 400. Deserializing here converts with [`OffsetDateTime::checked_to_offset`]
/// and refuses the value when it has no UTC representation, so every instant in
/// the process is a UTC one and no downstream `to_offset(UTC)` can fail.
/// Serializing goes through [`format_rfc3339`], not `time`'s own `Rfc3339`: the
/// wire form is the gear's one rendering, so a field that is serialized here and
/// the same instant rendered into a key or a digest elsewhere spell alike.
pub mod rfc3339 {
    use serde::{Deserializer, Serializer};
    use time::{OffsetDateTime, UtcOffset};

    /// The refusal a caller reads back in the 400 detail.
    const NO_UTC_FORM: &str =
        "instant has no UTC representation: the year leaves 0000..=9999 once its offset is applied";

    /// Convert to UTC or refuse.
    fn to_utc<E: serde::de::Error>(at: OffsetDateTime) -> Result<OffsetDateTime, E> {
        at.checked_to_offset(UtcOffset::UTC)
            .ok_or_else(|| E::custom(NO_UTC_FORM))
    }

    /// Serialize through [`format_rfc3339`].
    ///
    /// # Errors
    ///
    /// Whatever the serializer reports.
    pub fn serialize<S: Serializer>(at: &OffsetDateTime, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&super::format_rfc3339(*at))
    }

    /// Deserialize an RFC 3339 instant into its UTC form.
    ///
    /// # Errors
    ///
    /// When the text is not RFC 3339, or the instant has no UTC representation.
    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<OffsetDateTime, D::Error> {
        time::serde::rfc3339::deserialize(deserializer).and_then(to_utc)
    }

    /// The `Option<OffsetDateTime>` form, for
    /// `#[serde(default, with = "rfc3339::option")]`.
    pub mod option {
        use serde::{Deserializer, Serializer};
        use time::OffsetDateTime;

        /// Serialize through [`format_rfc3339`].
        ///
        /// # Errors
        ///
        /// Whatever the serializer reports.
        #[allow(
            clippy::ref_option,
            reason = "serde's `with` contract is `serialize(&T, S)` with `T = Option<_>`"
        )]
        pub fn serialize<S: Serializer>(
            at: &Option<OffsetDateTime>,
            serializer: S,
        ) -> Result<S::Ok, S::Error> {
            match at {
                Some(at) => serializer.serialize_some(&super::super::format_rfc3339(*at)),
                None => serializer.serialize_none(),
            }
        }

        /// Deserialize an optional RFC 3339 instant into its UTC form.
        ///
        /// # Errors
        ///
        /// When the text is not RFC 3339, or the instant has no UTC representation.
        pub fn deserialize<'de, D: Deserializer<'de>>(
            deserializer: D,
        ) -> Result<Option<OffsetDateTime>, D::Error> {
            time::serde::rfc3339::option::deserialize(deserializer)?
                .map(super::to_utc)
                .transpose()
        }
    }
}

/// Parse an RFC 3339 instant. Replaces `DateTime::parse_from_rfc3339`.
///
/// # Errors
///
/// When `raw` is not RFC 3339.
pub fn parse_rfc3339(raw: &str) -> Result<OffsetDateTime, time::error::Parse> {
    OffsetDateTime::parse(raw, &Rfc3339)
}

#[cfg(test)]
#[path = "instant_tests.rs"]
mod instant_tests;
