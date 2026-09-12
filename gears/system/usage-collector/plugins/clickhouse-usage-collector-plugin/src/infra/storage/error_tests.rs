use std::time::Duration;

use clickhouse::error::Error as ChError;
use usage_collector_sdk::UsageCollectorPluginError;

use super::{acquire_error_clears_readiness, map_ch_err, with_deadline};
use crate::infra::metrics::Metrics;

/// Assert `text` in a `BadResponse` body is classified as retryable.
#[track_caller]
fn assert_bad_response_is_transient(text: &str) {
    let err = ChError::BadResponse(text.to_owned());
    assert!(
        matches!(
            map_ch_err(&err),
            UsageCollectorPluginError::Transient { .. }
        ),
        "expected `{text}` to be classified retryable"
    );
}

/// Assert `text` in a `BadResponse` body is classified as non-retryable.
#[track_caller]
fn assert_bad_response_is_internal(text: &str) {
    let err = ChError::BadResponse(text.to_owned());
    assert!(
        matches!(map_ch_err(&err), UsageCollectorPluginError::Internal(_)),
        "expected `{text}` to be classified non-retryable"
    );
}

#[test]
fn network_error_maps_to_transient() {
    let err = ChError::Network(Box::new(std::io::Error::other("connection refused")));
    assert!(matches!(
        map_ch_err(&err),
        UsageCollectorPluginError::Transient { .. }
    ));
}

#[test]
fn timed_out_maps_to_transient() {
    let err = ChError::TimedOut;
    assert!(matches!(
        map_ch_err(&err),
        UsageCollectorPluginError::Transient { .. }
    ));
}

#[test]
fn row_not_found_maps_to_internal() {
    let err = ChError::RowNotFound;
    assert!(matches!(
        map_ch_err(&err),
        UsageCollectorPluginError::Internal(_)
    ));
}

#[test]
fn bad_response_maps_to_internal() {
    let err = ChError::BadResponse("unexpected response".to_owned());
    assert!(matches!(
        map_ch_err(&err),
        UsageCollectorPluginError::Internal(_)
    ));
}

/// Every code on the retryable allowlist, in the exception-body shape the
/// `clickhouse` crate produces when it can read the response.
#[test]
fn allowlisted_exception_codes_map_to_transient() {
    for (code, name) in [
        (159, "TIMEOUT_EXCEEDED"),
        (202, "TOO_MANY_SIMULTANEOUS_QUERIES"),
        (203, "NO_FREE_CONNECTION"),
        (209, "SOCKET_TIMEOUT"),
        (210, "NETWORK_ERROR"),
        (252, "TOO_MANY_PARTS"),
        (279, "ALL_CONNECTION_TRIES_FAILED"),
        (285, "TOO_FEW_LIVE_REPLICAS"),
        (999, "KEEPER_EXCEPTION"),
    ] {
        assert_bad_response_is_transient(&format!(
            "Code: {code}. DB::Exception: {name} (version 24.8.1.1 (official build))"
        ));
    }
}

/// The crate falls back to a bare `Code: <n>` built from the
/// `X-ClickHouse-Exception-Code` header when the body is empty or unreadable.
#[test]
fn bare_exception_code_header_form_maps_to_transient() {
    assert_bad_response_is_transient("Code: 279");
}

/// Codes off the allowlist stay `Internal` — including the two deliberate
/// exclusions, where retrying would either loop forever or need write-path
/// knowledge this classifier does not have.
#[test]
fn non_allowlisted_exception_codes_map_to_internal() {
    for (code, name) in [
        (241, "MEMORY_LIMIT_EXCEEDED"),
        (319, "UNKNOWN_STATUS_OF_INSERT"),
        (60, "UNKNOWN_TABLE"),
        (47, "UNKNOWN_IDENTIFIER"),
    ] {
        assert_bad_response_is_internal(&format!("Code: {code}. DB::Exception: {name}"));
    }
}

/// A permanent outer error must not be reclassified as retryable because a
/// nested exception from another node happens to quote a retryable code. Pins
/// the anchored parse against a substring search.
#[test]
fn a_nested_retryable_code_does_not_make_a_permanent_error_transient() {
    assert_bad_response_is_internal(
        "Code: 60. DB::Exception: Table default.nope does not exist. \
         (while receiving from shard: Code: 252. DB::Exception: Too many parts)",
    );
}

/// The `"<status> <reason>"` shape the crate emits when there is no readable
/// body at all — an intermediary returning a bare gateway error.
#[test]
fn retryable_http_status_fallback_maps_to_transient() {
    for text in [
        "502 Bad Gateway",
        "503 Service Unavailable",
        "504 Gateway Timeout",
    ] {
        assert_bad_response_is_transient(text);
    }
}

#[test]
fn non_retryable_http_status_fallback_maps_to_internal() {
    for text in ["400 Bad Request", "404 Not Found", "501 Not Implemented"] {
        assert_bad_response_is_internal(text);
    }
}

#[test]
fn network_error_clears_readiness() {
    let err = ChError::Network(Box::new(std::io::Error::other("refused")));
    assert!(acquire_error_clears_readiness(&err));
}

#[test]
fn timed_out_clears_readiness() {
    assert!(acquire_error_clears_readiness(&ChError::TimedOut));
}

#[test]
fn bad_response_does_not_clear_readiness() {
    assert!(!acquire_error_clears_readiness(&ChError::BadResponse(
        "oops".to_owned()
    )));
}

/// The two predicates answer different questions, and this is the case that
/// separates them: server-side backpressure is worth retrying, but the server
/// answered, so it is not an outage and must not clear the readiness gauge.
#[test]
fn a_retryable_server_code_is_transient_yet_not_an_outage() {
    let err = ChError::BadResponse("Code: 252. DB::Exception: Too many parts".to_owned());
    assert!(
        matches!(
            map_ch_err(&err),
            UsageCollectorPluginError::Transient { .. }
        ),
        "backpressure is retryable"
    );
    assert!(
        !acquire_error_clears_readiness(&err),
        "but the backend answered, so readiness must stay set"
    );
}

#[tokio::test]
async fn with_deadline_passes_through_a_completed_future() {
    let metrics = Metrics::new();
    let value = with_deadline(&metrics, Duration::from_secs(30), async { Ok(7_u32) })
        .await
        .expect("a future that resolves in time must pass through");
    assert_eq!(value, 7);
}

#[tokio::test]
async fn with_deadline_classifies_the_underlying_error() {
    let metrics = Metrics::new();
    let err = with_deadline(&metrics, Duration::from_secs(30), async {
        Err::<(), _>(ChError::RowNotFound)
    })
    .await
    .expect_err("the inner failure must surface");
    assert!(matches!(err, UsageCollectorPluginError::Internal(_)));
}

/// A future that never resolves — the shape of a connection that is accepted
/// and then never answered, which the server-side `send_timeout` /
/// `receive_timeout` settings cannot bound.
#[tokio::test]
async fn with_deadline_maps_an_expired_deadline_to_transient() {
    let metrics = Metrics::new();
    let err = with_deadline(&metrics, Duration::from_millis(20), async {
        std::future::pending::<Result<(), ChError>>().await
    })
    .await
    .expect_err("an unbounded future must be cut off at the deadline");
    assert!(
        matches!(err, UsageCollectorPluginError::Transient { .. }),
        "a timed-out request is retryable, got {err:?}"
    );
}
