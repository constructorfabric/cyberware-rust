//! Error classification: maps `clickhouse::error::Error` to
//! [`UsageCollectorPluginError`].
//!
//! `ClickHouse` does not have the same transient/internal breakdown as `sqlx`:
//! there are no SQL-state codes, unique-constraint violation codes, or FK codes.
//! Classification is therefore purely based on the variant of
//! [`clickhouse::error::Error`]:
//!
//! - **Transient**: `Network`, `TimedOut`, `Compression`, `Decompression` —
//!   connectivity-class errors safe to retry.
//! - **Internal**: all other variants — protocol, decode, schema, or
//!   configuration errors that are unlikely to resolve on retry.

use clickhouse::error::Error as ChError;
use usage_collector_sdk::UsageCollectorPluginError;

use crate::infra::metrics::{ErrorClass, Metrics};

/// Classify a `ClickHouse` error as transient (retryable) or internal.
///
/// Returns `true` for connectivity-class errors that the caller may retry;
/// `false` for protocol, decode, or schema errors.
#[must_use]
fn is_transient(err: &ChError) -> bool {
    matches!(
        err,
        ChError::Network(_)
            | ChError::TimedOut
            | ChError::Compression(_)
            | ChError::Decompression(_)
    )
}

/// Map a `ClickHouse` error to a [`UsageCollectorPluginError`].
///
/// Transient (connectivity-class) errors become
/// [`UsageCollectorPluginError::Transient`] and are logged at `warn` level;
/// everything else becomes [`UsageCollectorPluginError::Internal`] and is
/// logged at `error` level. Either way the raw error reaches operator logs
/// only — it is never included in the returned detail (which may surface to
/// callers).
#[must_use]
pub fn map_ch_err(err: &ChError) -> UsageCollectorPluginError {
    if is_transient(err) {
        tracing::warn!(error = %err, "transient ClickHouse error mapped to Transient");
        return UsageCollectorPluginError::transient("ClickHouse unavailable");
    }
    tracing::error!(error = %err, "unclassified ClickHouse error mapped to Internal");
    UsageCollectorPluginError::internal("ClickHouse error")
}

/// Map a `ClickHouse` error to a plugin error and increment the
/// `uc_clickhouse_backend_errors_total` counter under the matching
/// [`ErrorClass`].
///
/// Single definition shared by every store so the transient/internal split
/// used for the metric label can never drift from the one [`map_ch_err`]
/// applies to the returned error.
#[must_use]
pub fn tracked_ch_err(metrics: &Metrics, err: &ChError) -> UsageCollectorPluginError {
    let class = if is_transient(err) {
        ErrorClass::Transient
    } else {
        ErrorClass::Internal
    };
    metrics.inc_backend_error(class);
    map_ch_err(err)
}

/// Whether a `ClickHouse` client error on the acquire path should clear the
/// readiness gauge.
///
/// Mirrors the reference plugin's `acquire_error_clears_readiness`: only
/// connectivity-class (transient) errors represent a genuine outage; protocol or
/// decode errors on the happy path are non-outage Internal errors.
#[must_use]
pub fn acquire_error_clears_readiness(err: &ChError) -> bool {
    is_transient(err)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "error_tests.rs"]
mod error_tests;
