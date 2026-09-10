//! Tests for the shared instrumented fetch path's metrics contract: error-kind
//! labels derived from the *mapped* error, an upstream-status count on every
//! response, and `set_last_success` recording the feed's own publication time
//! rather than wall-clock.
//!
//! Driven over a real in-process server: `fetch_and_parse` takes a live
//! `RequestBuilder`, so there is no transport seam to fake.

use std::sync::Mutex;
use std::time::Duration;

use axum::Router;
use axum::http::StatusCode;
use axum::routing::get;
use bss_ledger_sdk::{ProviderRate, RateProviderError};
use time::OffsetDateTime;
use toolkit_http::HttpClient;

use super::fetch_and_parse;
use crate::metrics::FetchMetrics;

const PROVIDER: &str = "ecb";

/// The publication timestamp a successful `parse` reports — deliberately far in
/// the past so it cannot be confused with a wall-clock reading.
const AS_OF_UNIX: i64 = 1_700_000_000;

/// One recorded metric call.
#[derive(Debug, PartialEq)]
enum Event {
    Fetch { provider: String },
    RatesReturned { provider: String, count: u64 },
    FetchError { provider: String, kind: String },
    LastSuccess { provider: String, unix_secs: i64 },
    UpstreamStatus { provider: String, status: u16 },
}

/// Records every call so a test can assert which instruments fired, with which
/// labels and values.
#[derive(Default)]
struct RecordingMetrics {
    events: Mutex<Vec<Event>>,
}

impl RecordingMetrics {
    fn events(&self) -> Vec<Event> {
        std::mem::take(&mut *self.events.lock().unwrap())
    }
}

impl FetchMetrics for RecordingMetrics {
    fn observe_fetch(&self, provider: &str, _seconds: f64) {
        self.events.lock().unwrap().push(Event::Fetch {
            provider: provider.to_owned(),
        });
    }
    fn observe_rates_returned(&self, provider: &str, count: u64) {
        self.events.lock().unwrap().push(Event::RatesReturned {
            provider: provider.to_owned(),
            count,
        });
    }
    fn incr_fetch_error(&self, provider: &str, kind: &str) {
        self.events.lock().unwrap().push(Event::FetchError {
            provider: provider.to_owned(),
            kind: kind.to_owned(),
        });
    }
    fn set_last_success(&self, provider: &str, unix_secs: i64) {
        self.events.lock().unwrap().push(Event::LastSuccess {
            provider: provider.to_owned(),
            unix_secs,
        });
    }
    fn incr_upstream_status(&self, provider: &str, status: u16) {
        self.events.lock().unwrap().push(Event::UpstreamStatus {
            provider: provider.to_owned(),
            status,
        });
    }
}

/// Serve `body` with `status` on `/feed`, returning its URL.
async fn spawn_server(status: u16, body: &'static str) -> (String, tokio::task::JoinHandle<()>) {
    let app = Router::new().route(
        "/feed",
        get(move || async move { (StatusCode::from_u16(status).unwrap(), body) }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}/feed"), handle)
}

fn client() -> HttpClient {
    HttpClient::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .unwrap()
}

/// What `fetch_and_parse`'s `parse` closure returns: the mapped rates plus the
/// document's publication time.
type ParseResult = Result<(Vec<ProviderRate>, i64), RateProviderError>;

/// A `parse` that succeeds with `n` rates and the fixed publication time.
fn parse_ok(n: usize) -> impl FnOnce(&[u8]) -> ParseResult {
    move |_bytes| {
        let rates = (0..n)
            .map(|i| ProviderRate {
                base: "EUR".to_owned(),
                quote: format!("Q{i}"),
                rate_micro: 1_000_000,
                as_of: OffsetDateTime::from_unix_timestamp(AS_OF_UNIX).unwrap(),
                provider: PROVIDER.to_owned(),
            })
            .collect();
        Ok((rates, AS_OF_UNIX))
    }
}

#[tokio::test]
async fn success_records_status_duration_count_and_the_feeds_publication_time() {
    let (url, _server) = spawn_server(200, "body").await;
    let metrics = RecordingMetrics::default();
    let rates = fetch_and_parse(client().get(&url), PROVIDER, &metrics, parse_ok(3))
        .await
        .unwrap();
    assert_eq!(rates.len(), 3);

    let events = metrics.events();
    assert!(events.contains(&Event::UpstreamStatus {
        provider: PROVIDER.to_owned(),
        status: 200
    }));
    assert!(events.contains(&Event::Fetch {
        provider: PROVIDER.to_owned()
    }));
    assert!(events.contains(&Event::RatesReturned {
        provider: PROVIDER.to_owned(),
        count: 3
    }));
    // The freshness gauge carries the DOCUMENT's publication time; wall-clock
    // would make a stale feed look perpetually fresh.
    assert!(events.contains(&Event::LastSuccess {
        provider: PROVIDER.to_owned(),
        unix_secs: AS_OF_UNIX
    }));
    assert!(
        !events.iter().any(|e| matches!(e, Event::FetchError { .. })),
        "a successful fetch must record no error: {events:?}"
    );
}

#[tokio::test]
async fn non_success_status_is_counted_and_labelled_upstream_status() {
    let (url, _server) = spawn_server(503, "nope").await;
    let metrics = RecordingMetrics::default();
    let err = fetch_and_parse(client().get(&url), PROVIDER, &metrics, parse_ok(1))
        .await
        .unwrap_err();
    assert!(matches!(err, RateProviderError::UpstreamStatus(503)));

    let events = metrics.events();
    assert!(events.contains(&Event::UpstreamStatus {
        provider: PROVIDER.to_owned(),
        status: 503
    }));
    assert!(events.contains(&Event::FetchError {
        provider: PROVIDER.to_owned(),
        kind: "upstream_status".to_owned()
    }));
    // A failed tick must not move the freshness gauge.
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, Event::LastSuccess { .. })),
        "a non-2xx response must not record a success: {events:?}"
    );
}

#[tokio::test]
async fn parse_failure_label_comes_from_the_returned_error_not_a_hardcoded_kind() {
    // `parse` may return ANY `RateProviderError`, and the label must reflect the
    // real variant so `fx_provider_fetch_errors_total{kind}` stays truthful.
    let (url, _server) = spawn_server(200, "body").await;
    let metrics = RecordingMetrics::default();
    let err = fetch_and_parse(client().get(&url), PROVIDER, &metrics, |_bytes| {
        Err(RateProviderError::InvalidPair("bad pair".to_owned()))
    })
    .await
    .unwrap_err();
    assert!(matches!(err, RateProviderError::InvalidPair(_)));

    let events = metrics.events();
    assert!(
        events.contains(&Event::FetchError {
            provider: PROVIDER.to_owned(),
            kind: "invalid_pair".to_owned()
        }),
        "expected the mapped kind, got {events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, Event::LastSuccess { .. })),
        "a parse failure must not record a success: {events:?}"
    );
}

#[tokio::test]
async fn internal_parse_failure_is_labelled_internal() {
    let (url, _server) = spawn_server(200, "body").await;
    let metrics = RecordingMetrics::default();
    let _err = fetch_and_parse(client().get(&url), PROVIDER, &metrics, |_bytes| {
        Err(RateProviderError::Internal("bad xml".to_owned()))
    })
    .await
    .unwrap_err();

    assert!(metrics.events().contains(&Event::FetchError {
        provider: PROVIDER.to_owned(),
        kind: "internal".to_owned()
    }));
}

#[tokio::test]
async fn transport_failure_is_labelled_unreachable_and_records_no_status() {
    // A `.invalid` host (RFC 6761) can never resolve, so the request fails
    // before any response exists to count a status for — deterministically,
    // with no ephemeral port for another test's listener to race for.
    let metrics = RecordingMetrics::default();
    let err = fetch_and_parse(
        client().get("http://unreachable.invalid/feed"),
        PROVIDER,
        &metrics,
        parse_ok(1),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, RateProviderError::Unreachable(_)));

    let events = metrics.events();
    assert!(events.contains(&Event::FetchError {
        provider: PROVIDER.to_owned(),
        kind: "unreachable".to_owned()
    }));
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, Event::UpstreamStatus { .. })),
        "no response means no status to record: {events:?}"
    );
}

#[tokio::test]
async fn an_empty_but_valid_document_still_counts_as_a_success() {
    // Whether an empty table is acceptable is each source's call (both reject it);
    // the shared helper must not invent an error of its own.
    let (url, _server) = spawn_server(200, "body").await;
    let metrics = RecordingMetrics::default();
    let rates = fetch_and_parse(client().get(&url), PROVIDER, &metrics, parse_ok(0))
        .await
        .unwrap();
    assert!(rates.is_empty());

    assert!(metrics.events().contains(&Event::RatesReturned {
        provider: PROVIDER.to_owned(),
        count: 0
    }));
}

#[tokio::test]
async fn a_reply_slower_than_the_configured_timeout_is_unreachable_not_a_hang() {
    // The reachable-but-stalled upstream: a live listener that answers only
    // after 2 s, against a 200 ms client timeout. Connection refusal (above)
    // cannot represent this case — there the transport fails instantly, while
    // here the request has to be cut off by the configured timeout.
    // `HttpError::Timeout` maps to `Unreachable` (see `error::map_http_error`),
    // and a timed-out attempt must move only the error instrument.
    let app = Router::new().route(
        "/feed",
        get(|| async {
            tokio::time::sleep(Duration::from_secs(2)).await;
            "too late"
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let _server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let slow_client = HttpClient::builder()
        .timeout(Duration::from_millis(200))
        .build()
        .unwrap();
    let metrics = RecordingMetrics::default();
    let err = fetch_and_parse(
        slow_client.get(&format!("http://{addr}/feed")),
        PROVIDER,
        &metrics,
        parse_ok(1),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(err, RateProviderError::Unreachable(_)),
        "a timeout must map to Unreachable, got {err:?}"
    );

    let events = metrics.events();
    assert!(events.contains(&Event::FetchError {
        provider: PROVIDER.to_owned(),
        kind: "unreachable".to_owned()
    }));
    // No response ever arrived, so there is no status to count and no success
    // instrument may move — a timed-out feed must look failed, never fresh.
    assert!(
        !events.iter().any(|e| matches!(
            e,
            Event::UpstreamStatus { .. }
                | Event::Fetch { .. }
                | Event::RatesReturned { .. }
                | Event::LastSuccess { .. }
        )),
        "a timed-out fetch must record only the error: {events:?}"
    );
}
