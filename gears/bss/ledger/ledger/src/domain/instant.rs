//! Instant helpers shared by the ledger's `time::OffsetDateTime` surface.
//!
//! Calendar dates (`due_date`, posting `effective_at`) stay `chrono::NaiveDate`.
//! Instants — posted-at, queued-at, policy `effective_from` — are UTC
//! [`time::OffsetDateTime`], matching AM and pricing.
//!
//! Every instant in the process is a **UTC** one: request and response fields
//! deserialize through [`rfc3339`], which converts at the boundary and refuses an
//! instant with no UTC form; Postgres returns UTC; `now_utc` and [`utc_ymd_hms`]
//! construct it. That is what lets [`to_naive_date`], [`yyyymm`] and
//! [`format_rfc3339`] call `OffsetDateTime::to_offset(UTC)` — a panicking
//! conversion in `time` — as the identity it is here.

use chrono::NaiveDate;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

/// RFC 3339 rendering: a `Z` designator and a fraction of exactly six digits.
///
/// The gear's **one** rendering, and byte-identical to the pricing gear's, so an
/// instant that crosses between them spells the same on both sides. It replaced
/// a renderer that reproduced `chrono`'s `AutoSi` grouping with a `+00:00`
/// designator; nothing reads those bytes any more.
///
/// Six digits because this gear applies no quantum: its instants come from
/// Postgres at `timestamptz` resolution, so a `.SSS` form would discard a digit
/// the reader cannot recover, and a nanosecond form would print three digits
/// Postgres never returns. Fixed width so the rendering orders lexicographically
/// as it orders chronologically — the inquiry CSV export and the dual-control
/// policy snapshot are read as data, and sort on this column.
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

/// Microseconds since the Unix epoch. Same integer chrono produced via
/// `timestamp_micros`.
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
/// hour 24). Every in-tree caller passes a literal, so the panic is a
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
/// instant.
#[must_use]
pub fn from_unix(seconds: i64, nanos: u32) -> Option<OffsetDateTime> {
    OffsetDateTime::from_unix_timestamp(seconds)
        .ok()?
        .replace_nanosecond(nanos)
        .ok()
}

/// Unix timestamp in milliseconds, or `None` when the value is not a real
/// instant.
#[must_use]
pub fn from_unix_millis(millis: i64) -> Option<OffsetDateTime> {
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(millis) * 1_000_000).ok()
}

/// Milliseconds since the Unix epoch.
#[must_use]
pub fn timestamp_millis(at: OffsetDateTime) -> i64 {
    at.unix_timestamp()
        .saturating_mul(1_000)
        .saturating_add(i64::from(at.nanosecond() / 1_000_000))
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

/// The civil UTC date of `at`, as the `NaiveDate` posting dates still use.
#[must_use]
pub fn to_naive_date(at: OffsetDateTime) -> NaiveDate {
    let at = at.to_offset(time::UtcOffset::UTC);
    NaiveDate::from_ymd_opt(
        at.year(),
        u32::from(u8::from(at.month())),
        u32::from(at.day()),
    )
    .unwrap_or(NaiveDate::MIN)
}

/// `YYYYMM` period id for a UTC instant. Replaces chrono `format("%Y%m")`.
#[must_use]
pub fn yyyymm(at: OffsetDateTime) -> String {
    let at = at.to_offset(time::UtcOffset::UTC);
    format!("{:04}{:02}", at.year(), u8::from(at.month()))
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

/// Parse an RFC 3339 instant.
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
