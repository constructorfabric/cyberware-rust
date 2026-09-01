//! Strong-validator [`Etag`] for role-definition rows.
//!
//! Wire format: `"<updated_at_iso8601_micros>:<uuid>"` (no `W/` weak
//! prefix, no quoting — callers add HTTP framing in the REST layer).
//! Comparison is **byte-exact**. The `updated_at` value is truncated to
//! microsecond precision because `PostgreSQL`'s `timestamptz` stores
//! micros — truncation keeps the validator byte-stable across a DB
//! round-trip.

use std::fmt;
use std::str::FromStr;

use chrono::{DateTime, NaiveDateTime, SubsecRound, TimeZone, Utc};
use thiserror::Error;
use toolkit_macros::domain_model;
use uuid::Uuid;

/// Strong `ETag` carrying the row's `(updated_at_micros, id)` pair.
/// Constructed via [`etag_for`]; comparison is byte-exact.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Etag(String);

impl Etag {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    #[must_use]
    pub fn into_string(self) -> String {
        self.0
    }
}

impl fmt::Display for Etag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// ISO-8601 format with microsecond precision so that `PostgreSQL`'s
/// `timestamptz` representation round-trips identically.
const ETAG_TIME_FORMAT: &str = "%Y-%m-%dT%H:%M:%S%.6fZ";

/// Build an [`Etag`] from the row's `(updated_at, id)` pair. The
/// `updated_at` value is truncated to microsecond precision.
#[must_use]
pub fn etag_for(updated_at: DateTime<Utc>, id: Uuid) -> Etag {
    let micros = updated_at.trunc_subsecs(6);
    Etag(format!("{}:{}", micros.format(ETAG_TIME_FORMAT), id))
}

/// Failure surface for [`Etag::from_str`].
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum EtagParseError {
    /// The candidate string did not contain the `:` separator.
    #[error("etag missing ':' separator")]
    MissingSeparator,
    /// The timestamp segment did not parse as ISO-8601 UTC with
    /// microsecond precision.
    #[error("etag timestamp segment is not a valid microsecond-precision ISO-8601 UTC datetime")]
    InvalidTimestamp,
    /// The id segment did not parse as a UUID.
    #[error("etag id segment is not a valid UUID")]
    InvalidUuid,
}

impl FromStr for Etag {
    type Err = EtagParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        // The timestamp segment contains `:`-delimited hours/minutes/
        // seconds, so split on the LAST `:` to isolate the trailing UUID.
        let (ts_part, id_part) = s.rsplit_once(':').ok_or(EtagParseError::MissingSeparator)?;

        let naive = NaiveDateTime::parse_from_str(ts_part, ETAG_TIME_FORMAT)
            .map_err(|_| EtagParseError::InvalidTimestamp)?;
        let parsed_ts: DateTime<Utc> = Utc.from_utc_datetime(&naive);

        // Reject any nanosecond residue: canonical form is micros only.
        if parsed_ts.trunc_subsecs(6) != parsed_ts {
            return Err(EtagParseError::InvalidTimestamp);
        }

        // Structural check on the trailing UUID; we don't reuse the
        // parsed value because the original `s` is already canonical
        // (timestamp validated above, id segment is the verbatim suffix
        // after the last `:`).
        let _parsed_id: Uuid = id_part.parse().map_err(|_| EtagParseError::InvalidUuid)?;

        Ok(Self(s.to_owned()))
    }
}

#[cfg(test)]
#[path = "etag_tests.rs"]
mod etag_tests;
