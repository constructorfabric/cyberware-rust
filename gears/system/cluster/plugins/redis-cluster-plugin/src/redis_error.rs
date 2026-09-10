//! `fred::error::Error` → [`ClusterError`] mapping (DESIGN.md §10), shared by
//! the cache, the lock, and the startup preflight.
//!
//! Two things about `fred`'s error model shape this module, and both are worth
//! stating because neither is guessable from the mapping table alone.
//!
//! **A Redis error reply is not a distinct `ErrorKind`.** `fred` parses the
//! wire reply in `protocol::utils::pretty_error`, which switches on only the
//! first token and recognizes just `ERR`, `WRONGTYPE`, `NOAUTH`, `WRONGPASS`,
//! `MOVED`, `ASK`, and `CLUSTERDOWN`; everything else — `OOM`, `READONLY`,
//! `LOADING`, `MASTERDOWN`, `NOSCRIPT`, `CROSSSLOT` — arrives as
//! [`ErrorKind::Unknown`] with the full reply text in `details()`. So the
//! server-reply rows of DESIGN.md §10 can only be recognized by reading that
//! first token back out, which is what [`server_error_code`] does, and that
//! check has to run *before* the `ErrorKind` match or every one of those rows
//! would collapse into `Other`.
//!
//! **A malformed URL is a config fault, not a backend fault.** `ErrorKind::Url`
//! and `ErrorKind::Config` map to [`ClusterError::InvalidConfig`] rather than
//! being wrapped as [`ClusterError::Provider`], so an operator reading the error
//! is pointed at their YAML instead of at their server (`RD-LIFE-004`).

use cluster_sdk::{ClusterError, ProviderErrorKind};
use fred::error::{Error, ErrorKind};

/// The `NOSCRIPT` error code, as Redis spells it on the wire.
///
/// Named because two places have to agree on it: this module, which classifies
/// it, and `scripts.rs`, which acts on the classification by reloading the
/// catalog exactly once (DESIGN.md §6).
const NOSCRIPT: &str = "NOSCRIPT";

/// Returns the leading error code of a Redis error reply — the `OOM` of
/// `OOM command not allowed when used memory > 'maxmemory'`.
///
/// Redis's error-reply grammar puts an uppercase code first, so the first
/// whitespace-delimited token is the code whenever the error came from the
/// server at all. For a client-side `fred` error (an IO failure, a timeout)
/// the first token is just the first word of a prose message and matches
/// nothing in the table below, which is why this is safe to run over every
/// error unconditionally.
fn server_error_code(details: &str) -> &str {
    details.split_whitespace().next().unwrap_or("")
}

/// Returns `true` when `err` is the server reporting that the script hash it
/// was handed is not in its script cache.
///
/// The one `fred` error this plugin recovers from rather than surfaces: the
/// server restarted or its cache was flushed, so the catalog is reloaded and
/// the `EVALSHA` retried exactly once (DESIGN.md §6, `scripts::eval`). A
/// second `NOSCRIPT` is not recovered from — it falls through this module's
/// normal mapping and becomes `Provider { Other }`.
#[must_use]
pub fn is_noscript(err: &Error) -> bool {
    server_error_code(err.details()) == NOSCRIPT
}

/// Maps a `fred` error to a [`ClusterError`] per DESIGN.md §10.
///
/// | `fred` error / server reply | Result |
/// |---|---|
/// | `ErrorKind::Config`, `ErrorKind::Url` | `InvalidConfig` |
/// | `ErrorKind::IO` | `Provider { ConnectionLost }` |
/// | `ErrorKind::Timeout` | `Provider { Timeout }` |
/// | `ErrorKind::Auth` (which is where `fred` puts `NOAUTH`/`WRONGPASS`) | `Provider { AuthFailure }` |
/// | `ErrorKind::Backpressure` | `Provider { ResourceExhausted }` |
/// | `OOM …` | `Provider { ResourceExhausted }` |
/// | `READONLY …` | `Provider { ConnectionLost }` |
/// | `LOADING …`, `MASTERDOWN …`, `CLUSTERDOWN …` | `Provider { ResourceExhausted }` |
/// | anything else, including `NOSCRIPT` and `CROSSSLOT` | `Provider { Other }` |
///
/// Takes `err` by value so call sites can write `.map_err(map_redis_error)`
/// without a wrapping closure — the dominant call pattern once the cache and
/// lock land — rather than for its own sake.
#[allow(clippy::needless_pass_by_value)]
#[must_use]
pub fn map_redis_error(err: Error) -> ClusterError {
    // A bad DSN, an unparseable URL, or a client configuration `fred` refuses
    // outright is the operator's YAML, not the server. `InvalidConfig` rather
    // than `Provider` so it does not read as a backend fault and, in
    // particular, so it is never classified retryable (DESIGN.md §10).
    if matches!(err.kind(), ErrorKind::Config | ErrorKind::Url) {
        return ClusterError::InvalidConfig {
            reason: err.to_string(),
        };
    }

    let kind = server_reply_kind(err.details()).unwrap_or_else(|| client_error_kind(err.kind()));

    ClusterError::Provider {
        kind,
        message: err.to_string(),
    }
}

/// Classifies the Redis error replies DESIGN.md §10 enumerates but `fred` does
/// not give a distinct `ErrorKind`, returning `None` when the error did not
/// come from the server or carries a code this table does not name.
///
/// `CROSSSLOT` and `NOSCRIPT` are deliberately absent: both fall through to
/// `Other`. `CROSSSLOT` is unreachable by construction — every script in the
/// catalog declares exactly one key (DESIGN.md §6) — so if it ever appears it
/// is a plugin bug, and `Other` is right because no amount of retrying fixes
/// one. `NOSCRIPT` is handled a layer up by [`is_noscript`] and only reaches
/// here on the second occurrence, which DESIGN.md §6 likewise makes `Other`
/// rather than an unbounded retry.
fn server_reply_kind(details: &str) -> Option<ProviderErrorKind> {
    match server_error_code(details) {
        // The routing landed on a node that is now a replica, mid-failover.
        // `ConnectionLost` rather than `Other` because that is exactly its
        // retry semantics: `fred` re-resolves the topology and the next attempt
        // reaches the new primary.
        "READONLY" => Some(ProviderErrorKind::ConnectionLost),

        // Four transient refusals that clear on their own, so all four are
        // retryable-with-backoff and share one arm. `OOM` is the instance being
        // over `maxmemory` — DESIGN.md §3.7 is the operator's pointer for why
        // it should not be in that state at all; `LOADING` is a server still
        // reading its dataset after a restart; `MASTERDOWN` and `CLUSTERDOWN`
        // are a Sentinel or cluster with no reachable primary for the slot
        // right now, which a failover in progress is already resolving.
        "OOM" | "LOADING" | "MASTERDOWN" | "CLUSTERDOWN" => {
            Some(ProviderErrorKind::ResourceExhausted)
        }

        // `NOAUTH`/`WRONGPASS` are not listed here on purpose: `fred`'s own
        // reply parser already classifies both as `ErrorKind::Auth`, so the
        // client-side arm below covers them and a second copy of the rule here
        // could only drift from it.
        _ => None,
    }
}

/// Classifies a client-side `fred` error by its [`ErrorKind`].
///
/// `ErrorKind::Cluster` (`MOVED`/`ASK`) is intentionally left to the `Other`
/// catch-all: `fred` follows redirections and re-resolves the slot map itself,
/// so one surfacing to a caller means the redirection could not be followed,
/// which is not something a retry at this layer improves.
fn client_error_kind(kind: &ErrorKind) -> ProviderErrorKind {
    match kind {
        ErrorKind::IO => ProviderErrorKind::ConnectionLost,
        ErrorKind::Timeout => ProviderErrorKind::Timeout,
        ErrorKind::Auth => ProviderErrorKind::AuthFailure,
        ErrorKind::Backpressure => ProviderErrorKind::ResourceExhausted,
        _ => ProviderErrorKind::Other,
    }
}

// Layer-1 unit tests (TESTING.md §2, `redis_error.rs` row): the full DESIGN.md
// §10 table row by row. Out-of-line per DE1101.
#[cfg(test)]
#[path = "redis_error_tests.rs"]
mod tests;
