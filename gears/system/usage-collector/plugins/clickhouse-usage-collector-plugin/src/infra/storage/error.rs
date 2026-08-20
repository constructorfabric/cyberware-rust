//! Error classification: maps `clickhouse::error::Error` to
//! [`UsageCollectorPluginError`].
//!
//! `ClickHouse` has no SQL-state codes, unique-constraint violation codes, or
//! FK codes to classify on the way `sqlx` does. Two signals are available
//! instead, and they answer different questions — hence two predicates:
//!
//! - `is_connectivity_error` matches on the [`clickhouse::error::Error`]
//!   *variant* (`Network`, `TimedOut`, `Compression`, `Decompression`). This is
//!   the "the backend is unreachable" signal, and it alone drives the
//!   readiness gauge (see [`acquire_error_clears_readiness`]).
//! - `is_retryable` additionally accepts a fixed allowlist of server-reported
//!   `ClickHouse` error codes carried in a `BadResponse` body (see
//!   `RETRYABLE_CH_CODES`). Those are overload, backpressure, and
//!   replication-degradation conditions: worth a retry, but *not* an outage —
//!   the server answered.
//!
//! Everything else maps to `Internal`: protocol, decode, schema, or
//! configuration errors that will not resolve on retry.

use std::time::Duration;

use clickhouse::error::Error as ChError;
use usage_collector_sdk::UsageCollectorPluginError;

use crate::infra::metrics::{ErrorClass, Metrics};

/// Server-reported `ClickHouse` error codes worth retrying.
///
/// Read out of a [`ChError::BadResponse`] body by [`retryable_server_code`].
/// Deliberately a closed allowlist rather than a denylist: an unrecognised code
/// stays `Internal`, so a genuinely permanent failure can never be retried
/// forever just because nobody classified it.
///
/// - `159` `TIMEOUT_EXCEEDED` — what the `send_timeout` / `receive_timeout`
///   session settings this plugin configures actually produce.
/// - `202` `TOO_MANY_SIMULTANEOUS_QUERIES`, `203` `NO_FREE_CONNECTION` —
///   server-side saturation.
/// - `209` `SOCKET_TIMEOUT`, `210` `NETWORK_ERROR`,
///   `279` `ALL_CONNECTION_TRIES_FAILED` — connectivity the *server* reports
///   (e.g. to a shard) rather than a local failure surfacing as
///   [`ChError::Network`].
/// - `252` `TOO_MANY_PARTS` — insert backpressure that clears as merges catch
///   up.
/// - `285` `TOO_FEW_LIVE_REPLICAS`, `999` `KEEPER_EXCEPTION` — replicated
///   deployment degradation.
///
/// Two notable exclusions. `241` `MEMORY_LIMIT_EXCEEDED` is left out because
/// for a batch that is simply too large it is effectively permanent, and
/// retrying it would loop forever. `319` `UNKNOWN_STATUS_OF_INSERT` is left out
/// because whether a retry is safe depends on the dedup guarantees of the
/// specific write path, which is not a judgment this variant-level classifier
/// can make.
const RETRYABLE_CH_CODES: [u32; 9] = [159, 202, 203, 209, 210, 252, 279, 285, 999];

/// HTTP statuses worth retrying when `ClickHouse` returned no readable body.
///
/// The `clickhouse` crate falls back to a `"<status> <reason>"` string when the
/// response body is empty or undecodable, which is what an intermediary (proxy,
/// load balancer) returning a bare 502/503/504 looks like from here.
const RETRYABLE_HTTP_STATUSES: [u32; 3] = [502, 503, 504];

/// True for errors that mean the backend could not be reached at all.
///
/// Matches on the error variant only. Used both as the connectivity half of
/// [`is_retryable`] and, on its own, as the outage signal for the readiness
/// gauge.
#[must_use]
fn is_connectivity_error(err: &ChError) -> bool {
    matches!(
        err,
        ChError::Network(_)
            | ChError::TimedOut
            | ChError::Compression(_)
            | ChError::Decompression(_)
    )
}

/// True when a `BadResponse` body reports a `ClickHouse` error code (or bare
/// HTTP status) on the retryable allowlist.
///
/// [`ChError::BadResponse`] carries an unstructured `String`, but the
/// `clickhouse` crate builds it in one of three predictable shapes: the
/// exception body (`Code: <n>. DB::Exception: …`), a bare `Code: <n>` derived
/// from the `X-ClickHouse-Exception-Code` header when the body is empty or
/// unreadable, or a `"<status> <reason>"` HTTP fallback.
///
/// The code is read from the **start** of the string, not by searching for
/// `Code:` anywhere in it. A distributed-query exception body can quote a
/// nested exception from another node, and the leading code is the one that
/// describes this failure — a permanent outer error must not be reclassified as
/// retryable because some inner message happened to mention a retryable code.
#[must_use]
fn retryable_server_code(err: &ChError) -> bool {
    let ChError::BadResponse(text) = err else {
        return false;
    };
    let text = text.trim_start();

    if let Some(after_code) = text.strip_prefix("Code:") {
        return leading_u32(after_code).is_some_and(|code| RETRYABLE_CH_CODES.contains(&code));
    }
    leading_u32(text).is_some_and(|status| RETRYABLE_HTTP_STATUSES.contains(&status))
}

/// Parse the leading run of ASCII digits (after any leading whitespace).
fn leading_u32(text: &str) -> Option<u32> {
    let text = text.trim_start();
    let digits_end = text
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(text.len());
    if digits_end == 0 {
        return None;
    }
    text[..digits_end].parse().ok()
}

/// Classify a `ClickHouse` error as retryable (transient) or not.
///
/// Returns `true` for connectivity-class errors and for the server-reported
/// overload/backpressure codes on [`RETRYABLE_CH_CODES`]; `false` for protocol,
/// decode, or schema errors.
#[must_use]
fn is_retryable(err: &ChError) -> bool {
    is_connectivity_error(err) || retryable_server_code(err)
}

/// Map a `ClickHouse` error to a [`UsageCollectorPluginError`].
///
/// Retryable errors (`is_retryable`) become
/// [`UsageCollectorPluginError::Transient`] and are logged at `warn` level;
/// everything else becomes [`UsageCollectorPluginError::Internal`] and is
/// logged at `error` level. Either way the raw error reaches operator logs
/// only — it is never included in the returned detail (which may surface to
/// callers).
#[must_use]
pub fn map_ch_err(err: &ChError) -> UsageCollectorPluginError {
    if is_retryable(err) {
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
    let class = if is_retryable(err) {
        ErrorClass::Transient
    } else {
        ErrorClass::Internal
    };
    metrics.inc_backend_error(class);
    map_ch_err(err)
}

/// Await a `ClickHouse` future under the client-side deadline, mapping both
/// outcomes to a [`UsageCollectorPluginError`] and counting the error.
///
/// The `send_timeout` / `receive_timeout` settings this plugin configures are
/// *server* settings: they bound how long `ClickHouse` itself waits on its own
/// sockets, and do nothing when a connection is accepted and then never
/// answered, or when an intermediary holds it open. Without this wrapper such a
/// call has no bound at all. See `ClickHousePluginConfig::client_deadline` for
/// how the budget relates to the server-side one.
///
/// Applied per await point rather than around a whole store operation on
/// purpose: this plugin holds a cluster lock across `ClickHouse` I/O and
/// releases it explicitly on every exit path (cluster `LockGuard` drop is a
/// no-op). A deadline wrapped around the whole critical section would drop the
/// future mid-flight, skipping that release and leaking the lock until its
/// lease expired. An `Err` returned from here instead flows through the normal
/// error path, where the release already happens.
///
/// An expired deadline is counted as [`ErrorClass::Transient`] — the same class
/// [`ChError::TimedOut`] gets — so the backend-error counter cannot disagree
/// with the returned error's class.
///
/// # Errors
///
/// Returns `Transient` when the deadline expires, otherwise whatever
/// [`tracked_ch_err`] makes of the underlying failure.
pub async fn with_deadline<T>(
    metrics: &Metrics,
    deadline: Duration,
    fut: impl Future<Output = Result<T, ChError>>,
) -> Result<T, UsageCollectorPluginError> {
    match tokio::time::timeout(deadline, fut).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(err)) => Err(tracked_ch_err(metrics, &err)),
        Err(_elapsed) => {
            metrics.inc_backend_error(ErrorClass::Transient);
            tracing::warn!(
                deadline_secs = deadline.as_secs(),
                "ClickHouse request exceeded the client-side deadline"
            );
            Err(UsageCollectorPluginError::transient(
                "ClickHouse request timed out",
            ))
        }
    }
}

/// Whether a `ClickHouse` client error on the acquire path should clear the
/// readiness gauge.
///
/// Mirrors the reference plugin's `acquire_error_clears_readiness`: only
/// connectivity-class errors represent a genuine outage; protocol or decode
/// errors on the happy path are non-outage Internal errors.
///
/// Deliberately consults `is_connectivity_error` and **not** `is_retryable`.
/// A server-reported overload code (`TOO_MANY_PARTS`, say) is retryable but
/// proves the opposite of an outage — the backend answered. Clearing readiness
/// on it would report the backend as down every time it pushed back.
#[must_use]
pub fn acquire_error_clears_readiness(err: &ChError) -> bool {
    is_connectivity_error(err)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "error_tests.rs"]
mod error_tests;
