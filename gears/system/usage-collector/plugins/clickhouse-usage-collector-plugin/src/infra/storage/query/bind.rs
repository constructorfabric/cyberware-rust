//! Value binding: convert a `toolkit_odata` AST value into a storage-typed
//! bind, and apply that bind to a `ClickHouse` [`Query`].
//!
//! Unlike the reference plugin (`sqlx`'s `$N` parameter style), `ClickHouse`
//! uses positional `?` placeholders.  [`SqlBind`] variants cover the column
//! types present in `usage_records` and `usage_type_catalog`. [`bind_one`]
//! applies a single bind to a consumed [`Query`], returning the updated query.
//!
//! ## `ClickHouse` crate API (verified against 0.15.1)
//!
//! - `Query::bind(value: impl Bind) -> Self` — consumes the query, appends the
//!   bind value, and returns a new query. Bind values are formatted as SQL
//!   literals (numbers as digits, strings as quoted `'…'`).
//! - [`uuid::Uuid`] has `features = ["serde"]` in this workspace; its
//!   `to_string()` produces a hyphenated UUID that `ClickHouse` accepts as a
//!   `UUID` literal.
//! - [`rust_decimal::Decimal`] has `features = ["serde"]`; its `to_string()`
//!   produces a decimal string (`"42.5"`) that `ClickHouse` accepts for
//!   `Decimal128(9)` columns.
//! - `DateTime64(6)` values are epoch-microseconds bound as `i64`, but their
//!   SQL placeholder must be wrapped in `fromUnixTimestamp64Micro(?)`.
//!   A bare microsecond integer in a tuple comparison is coerced through
//!   decimal arithmetic by `ClickHouse` and can fail with `DECIMAL_OVERFLOW`.
//!
//! [`Query`]: clickhouse::query::Query

use rust_decimal::Decimal;
use uuid::Uuid;

use toolkit_odata::filter::ODataValue;

/// A storage-typed value ready to be bound to a `ClickHouse` `?` placeholder.
///
/// Each variant maps to a column type in `usage_records` / `usage_type_catalog`.
#[derive(Debug, Clone)]
pub enum SqlBind {
    /// `UUID` column bind.
    Uuid(Uuid),
    /// `String` column bind.
    Str(String),
    /// `Decimal128(9)` column bind.
    Decimal(Decimal),
    /// `DateTime64(6)` column bind (epoch-microseconds as `i64`).
    DateTime64Micros(i64),
    /// `Boolean` column bind.
    Bool(bool),
    /// Signed 64-bit integer bind.
    I64(i64),
    /// Unsigned 64-bit integer bind.
    U64(u64),
}

impl SqlBind {
    /// Render the SQL placeholder required for this bind's storage type.
    ///
    /// Most values use a plain positional placeholder. `DateTime64(6)` needs
    /// an explicit epoch-microsecond conversion so `ClickHouse` does not
    /// interpret the large integer as seconds or coerce it through Decimal
    /// arithmetic in tuple comparisons.
    #[must_use]
    pub fn placeholder(&self) -> &'static str {
        match self {
            Self::DateTime64Micros(_) => "fromUnixTimestamp64Micro(?)",
            _ => "?",
        }
    }
}

/// Convert an `OData` AST value into a storage-typed [`SqlBind`].
///
/// - `DateTime` values are converted from `chrono::DateTime<Utc>` to epoch-
///   microseconds (`i64`) via [`timestamp_micros`].
/// - `Number` values are converted from `bigdecimal::BigDecimal` to
///   `rust_decimal::Decimal` via the string representation.
///
/// # Errors
///
/// Returns an error string on `Null` / `Date` / `Time` values (none of the
/// `usage_records` filter columns are date-only or time-only) or when a
/// numeric value is out of the `rust_decimal::Decimal` range.
///
/// [`timestamp_micros`]: chrono::DateTime::timestamp_micros
pub fn odata_value_to_bind(v: &ODataValue) -> Result<SqlBind, String> {
    match v {
        ODataValue::Uuid(u) => Ok(SqlBind::Uuid(*u)),
        ODataValue::String(s) => Ok(SqlBind::Str(s.clone())),
        ODataValue::Bool(b) => Ok(SqlBind::Bool(*b)),
        ODataValue::Number(n) => n
            .to_string()
            .parse::<Decimal>()
            .map(SqlBind::Decimal)
            .map_err(|e| format!("numeric out of range: {e}")),
        ODataValue::DateTime(dt) => Ok(SqlBind::DateTime64Micros(dt.timestamp_micros())),
        ODataValue::Null => Err("null filter value unsupported".to_owned()),
        ODataValue::Date(_) | ODataValue::Time(_) => {
            Err("date/time-only filter values unsupported".to_owned())
        }
    }
}

/// Apply a single [`SqlBind`] to a `ClickHouse` [`Query`], returning the query
/// with the bind appended.
///
/// `UUID` and `Decimal` values are bound as strings (hyphenated UUID / decimal
/// string literal) because `ClickHouse` parses those literals correctly for
/// `UUID` and `Decimal128(9)` column types. `DateTime64Micros` binds its inner
/// `i64`; callers must render the corresponding placeholder via
/// [`SqlBind::placeholder`] so the SQL applies `fromUnixTimestamp64Micro`.
///
/// [`Query`]: clickhouse::query::Query
pub fn bind_one(q: clickhouse::query::Query, v: &SqlBind) -> clickhouse::query::Query {
    match v {
        SqlBind::Uuid(u) => q.bind(u.to_string()),
        SqlBind::Str(s) => q.bind(s.as_str()),
        SqlBind::Decimal(d) => q.bind(d.to_string()),
        SqlBind::DateTime64Micros(n) | SqlBind::I64(n) => q.bind(*n),
        SqlBind::Bool(b) => q.bind(*b),
        SqlBind::U64(n) => q.bind(*n),
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "bind_tests.rs"]
mod bind_tests;
