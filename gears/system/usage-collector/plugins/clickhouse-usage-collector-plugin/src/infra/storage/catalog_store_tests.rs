// Test modules using bare `panic!` opt in explicitly.
#![allow(clippy::panic)]
#![cfg_attr(coverage_nightly, coverage(off))]

//! Unit tests for [`ChCatalogStore`].
//!
//! The offline tests need no live `ClickHouse` server: they exercise the
//! refresh-worker cancellation / coalescing behaviour, the client-side
//! deadline, the version scheme, and the unimplemented `delete`.
//!
//! The `live` module is gated behind `#[cfg(feature = "clickhouse")]` because
//! its tests require a live `ClickHouse` server to return meaningful query
//! results.

use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;
use usage_collector_sdk::UsageCollectorPluginError;

use super::{ChCatalogStore, DELETE_UNIMPLEMENTED_MSG, RefreshOutcome};
use crate::infra::metrics::Metrics;

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Endpoint for clients that must never reach a server.
///
/// Port 1 is reserved and never bound, so a query fails fast with connection
/// refused. The `clickhouse` crate's default address (`http://localhost:8123`)
/// is deliberately avoided: a developer running a real `ClickHouse` locally
/// would have these "offline" tests silently talk to it.
const UNREACHABLE_URL: &str = "http://127.0.0.1:1";

/// Client-side request deadline for stores built in this file.
///
/// Generous relative to every assertion here: these tests are about worker
/// behaviour, and their clients either fail fast (connection refused) or are
/// expected to hang, so the deadline must never be what a test observes —
/// except in the one test that sets its own short deadline deliberately.
const TEST_DEADLINE: Duration = Duration::from_secs(30);

/// Build a catalog store over an offline `ClickHouse` client with a
/// caller-chosen cancellation token.
///
/// The client is pointed at [`UNREACHABLE_URL`], so any query issued against it
/// fails quickly (connection refused) rather than blocking, keeping test
/// duration low.
pub(super) fn offline_store(cancel: CancellationToken) -> ChCatalogStore {
    ChCatalogStore::new(
        clickhouse::Client::default().with_url(UNREACHABLE_URL),
        cancel,
        Arc::new(Metrics::new()),
        TEST_DEADLINE,
    )
}

// ── Test 1: cancellation short-circuit ───────────────────────────────────────

#[tokio::test]
async fn refresh_short_circuits_when_token_already_cancelled() {
    let cancel = CancellationToken::new();
    cancel.cancel(); // pre-cancel before calling refresh

    let store = offline_store(cancel);

    // With a biased select and an already-cancelled token, the cancel arm wins
    // immediately — no count query is issued, no connection is checked out.
    let outcome = store.refresh_catalog_size_cancellable().await;
    assert_eq!(outcome, RefreshOutcome::Cancelled);
}

// ── Test 2: refresh runs when not cancelled ───────────────────────────────────

#[tokio::test]
async fn refresh_runs_to_completion_when_not_cancelled() {
    let cancel = CancellationToken::new(); // never cancelled
    let store = offline_store(cancel);

    // The count query attempts a connection to the unreachable endpoint.
    // Whether it succeeds or fails (connection refused against an offline
    // server), the outcome is always `Ran` — failure is logged at `warn`, not
    // propagated.
    let outcome = store.refresh_catalog_size_cancellable().await;
    assert_eq!(outcome, RefreshOutcome::Ran);
}

// ── Test 3: burst coalescing ──────────────────────────────────────────────────

#[tokio::test]
async fn burst_refresh_coalesces_into_at_most_five_runs() {
    use std::sync::atomic::Ordering;

    let cancel = CancellationToken::new();
    let store = offline_store(cancel);

    // Fire 32 mutation signals synchronously. `notify_one` collapses them: the
    // background worker holds at most one queued permit at any point, so the
    // total run count is bounded (at most one in-flight + one trailing) rather
    // than proportional to the signal count.
    for _ in 0..32 {
        store.request_catalog_size_refresh();
    }

    // Give the single worker ample wall-clock time to drain. Each run fails
    // fast (connection refused) so the worker cycles quickly; we wait long
    // enough to detect any spurious per-signal fan-out.
    tokio::time::sleep(Duration::from_millis(300)).await;

    let runs = store.refresh_runs.load(Ordering::SeqCst);
    assert!(
        runs <= 5,
        "32 burst signals must coalesce to ≤5 runs, got {runs} \
         (per-signal spawning would approach 32)"
    );
}

// ── Test 3b: worker exits on a cancel that lands mid-count ───────────────────

/// Cancelling while a `count()` is in flight drops that query and stops the
/// worker, instead of waiting for a response that may never arrive.
///
/// The store points at a socket that accepts the connection and then never
/// answers, so the count is deterministically still in flight when the token
/// fires — the same shape as a shutdown racing an unresponsive server.
#[tokio::test]
async fn worker_exits_when_cancelled_during_an_in_flight_count() {
    use std::sync::atomic::Ordering;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind a local socket");
    let addr = listener.local_addr().expect("local addr");
    // Accept and hold connections open without ever writing a response.
    tokio::spawn(async move {
        let mut accepted = Vec::new();
        while let Ok((stream, _)) = listener.accept().await {
            accepted.push(stream);
        }
    });

    let cancel = CancellationToken::new();
    let store = ChCatalogStore::new(
        clickhouse::Client::default().with_url(format!("http://{addr}")),
        cancel.clone(),
        Arc::new(Metrics::new()),
        TEST_DEADLINE,
    );

    store.request_catalog_size_refresh();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        store.refresh_runs.load(Ordering::SeqCst),
        1,
        "the worker must have started exactly one count, which is now hanging"
    );

    cancel.cancel();
    tokio::time::sleep(Duration::from_millis(100)).await;

    // The worker is gone: a further signal is never drained.
    store.request_catalog_size_refresh();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        store.refresh_runs.load(Ordering::SeqCst),
        1,
        "a cancelled worker must not pick up further refresh signals"
    );
}

// ── Test 3c: a black-holed socket is cut off by the client-side deadline ──────

/// A connection that is accepted and then never answered is bounded by the
/// client-side deadline, and reported as `Transient`.
///
/// This is the failure the `send_timeout` / `receive_timeout` settings
/// `build_client` configures cannot catch: they are *server* settings, and a
/// black-holed socket never reaches a server that could apply them. Without a
/// client-side deadline this call hangs for as long as the caller waits.
#[tokio::test]
async fn a_black_holed_socket_is_cut_off_by_the_client_side_deadline() {
    use usage_collector_sdk::UsageTypeGtsId;

    use crate::domain::ports::CatalogStore;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind a local socket");
    let addr = listener.local_addr().expect("local addr");
    tokio::spawn(async move {
        let mut accepted = Vec::new();
        while let Ok((stream, _)) = listener.accept().await {
            accepted.push(stream);
        }
    });

    let deadline = Duration::from_millis(300);
    let store = ChCatalogStore::new(
        clickhouse::Client::default().with_url(format!("http://{addr}")),
        CancellationToken::new(),
        Arc::new(Metrics::new()),
        deadline,
    );

    let gts_id =
        UsageTypeGtsId::new("gts.cf.core.uc.usage_record.v1~cf.compute._.black_holed_test.v1")
            .expect("valid gts_id");

    let started = std::time::Instant::now();
    let err = store
        .get(gts_id)
        .await
        .expect_err("a request to a socket that never answers must not succeed");
    let elapsed = started.elapsed();

    match err {
        UsageCollectorPluginError::Transient { .. } => {}
        other => panic!("expected Transient, got {other:?}"),
    }
    assert!(
        elapsed < Duration::from_secs(5),
        "the call must return at roughly the deadline rather than hang, took {elapsed:?}"
    );
}

// ── Test 4: delete is unimplemented and touches no backend ────────────────────

/// `delete` is not implemented by this backend: it returns `Internal` carrying
/// the fixed not-implemented message and issues no SQL.
///
/// The store points at a socket that accepts and never answers, with a long
/// deadline: had `delete` issued any statement, the call would block for the
/// whole deadline. Returning well under it proves no round-trip was attempted.
#[tokio::test]
async fn delete_is_unimplemented_and_issues_no_sql() {
    use usage_collector_sdk::UsageTypeGtsId;

    use crate::domain::ports::CatalogStore;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind a local socket");
    let addr = listener.local_addr().expect("local addr");
    tokio::spawn(async move {
        let mut accepted = Vec::new();
        while let Ok((stream, _)) = listener.accept().await {
            accepted.push(stream);
        }
    });

    let store = ChCatalogStore::new(
        clickhouse::Client::default().with_url(format!("http://{addr}")),
        CancellationToken::new(),
        Arc::new(Metrics::new()),
        TEST_DEADLINE,
    );

    let gts_id =
        UsageTypeGtsId::new("gts.cf.core.uc.usage_record.v1~cf.compute._.delete_unimpl_test.v1")
            .expect("valid gts_id");

    let started = std::time::Instant::now();
    let err = store
        .delete(gts_id)
        .await
        .expect_err("delete is not implemented and must fail");
    let elapsed = started.elapsed();

    match err {
        UsageCollectorPluginError::Internal(msg) => {
            assert_eq!(msg, DELETE_UNIMPLEMENTED_MSG);
            assert!(
                msg.contains("not implemented"),
                "message must say why: {msg}"
            );
        }
        other => panic!("expected Internal, got {other:?}"),
    }
    assert!(
        elapsed < Duration::from_secs(1),
        "delete must return without any round-trip (the socket never answers), took {elapsed:?}"
    );
}

/// Regression guard for `create`'s version assignment.
///
/// `usage_type_catalog` is a `ReplacingMergeTree(version)` ordered by `(gts_id)`,
/// so every read resolves to whichever physical row carries the highest
/// `version`. Two concurrent `create` calls for the same `gts_id` can both
/// pass the pre-existence check and race to `INSERT`; distinct, monotonically
/// increasing versions are what let that resolution deterministically pick
/// a winner instead of an undefined tie. An earlier draft of this phase
/// hardcoded `version = 1` on create, which would make every racing insert
/// for the same `gts_id` tie on version. This test fails if that regresses.
#[test]
fn create_version_is_monotonic_not_hardcoded() {
    use crate::infra::storage::mapper::current_merge_version;

    let first = current_merge_version();
    std::thread::sleep(Duration::from_millis(2));
    let second = current_merge_version();

    assert!(first > 1, "create must not emit the hardcoded version 1");
    assert!(
        first < second,
        "current_merge_version() must be monotonically increasing ({first} < {second})"
    );
}

// ── Live tests: `ClickHouse`-backed paths (require live server) ───────────────
//
// These tests exercise SQL paths that need a live `ClickHouse` server to return
// meaningful results (empty row / live row). Without a server, `fetch_optional`
// returns a network error (mapped to `Transient`) which is indistinguishable
// from a connectivity issue, so the assertions cannot pass.
//
// Run with: `cargo test -p cf-gears-clickhouse-usage-collector-plugin --features clickhouse`

#[cfg(feature = "clickhouse")]
mod live {
    use std::sync::Arc;
    use std::time::Duration;

    use testcontainers::core::WaitFor;
    use testcontainers::runners::AsyncRunner;
    use testcontainers::{ContainerAsync, GenericImage, ImageExt};
    use tokio_util::sync::CancellationToken;
    use usage_collector_sdk::{UsageCollectorPluginError, UsageKind, UsageType, UsageTypeGtsId};

    use crate::domain::ports::CatalogStore;
    use crate::infra::metrics::Metrics;
    use crate::infra::storage::catalog_store::ChCatalogStore;
    use crate::infra::storage::pool::{apply_migrations, ensure_retention_ttl};

    const CH_PASSWORD: &str = "live_test_pw";

    /// Start a `ClickHouse` container and apply migrations. Panics if Docker is unavailable.
    async fn start() -> (
        ChCatalogStore,
        clickhouse::Client,
        ContainerAsync<GenericImage>,
    ) {
        let image = GenericImage::new("clickhouse/clickhouse-server", "25.6")
            .with_wait_for(WaitFor::Nothing)
            .with_env_var("CLICKHOUSE_USER", "default")
            .with_env_var("CLICKHOUSE_PASSWORD", CH_PASSWORD)
            .with_env_var("CLICKHOUSE_DB", "default");

        let container = image
            .start()
            .await
            .expect("ClickHouse container must start");

        let port = container
            .get_host_port_ipv4(8123)
            .await
            .expect("container port 8123 must be mapped");
        let client = clickhouse::Client::default()
            .with_url(format!("http://127.0.0.1:{port}/"))
            .with_user("default")
            .with_password(CH_PASSWORD)
            .with_database("default");

        for _ in 0..120u8 {
            if client.query("SELECT 1").fetch_one::<u8>().await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }

        apply_migrations(&client, super::TEST_DEADLINE)
            .await
            .expect("schema migrations must succeed on the live test container");
        ensure_retention_ttl(&client, 365 * 24 * 3600, super::TEST_DEADLINE)
            .await
            .expect("retention TTL reconcile must succeed on the live test container");

        let store = ChCatalogStore::new(
            client.clone(),
            CancellationToken::new(),
            Arc::new(Metrics::new()),
            super::TEST_DEADLINE,
        );
        (store, client, container)
    }

    fn counter_gts_id(suffix: &str) -> UsageTypeGtsId {
        UsageTypeGtsId::new(format!(
            "gts.cf.core.uc.usage_record.v1~cf.compute._.{suffix}.v1"
        ))
        .expect("valid gts_id")
    }

    fn counter_usage_type(suffix: &str) -> UsageType {
        UsageType {
            gts_id: counter_gts_id(suffix),
            kind: UsageKind::Counter,
            metadata_fields: std::collections::BTreeSet::new(),
        }
    }

    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn create_silent_absorb_on_identical_resubmission() {
        let (store, _client, _container) = start().await;
        let ut = counter_usage_type("create_absorb_test");

        let first = store
            .create(ut.clone())
            .await
            .expect("first create must succeed");

        let second = store
            .create(ut.clone())
            .await
            .expect("second create (identical) must be absorbed");

        assert_eq!(first.gts_id, second.gts_id);
        assert_eq!(first.kind, second.kind);
        assert_eq!(first.metadata_fields, second.metadata_fields);
    }

    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn create_returns_already_exists_on_differing_payload() {
        use std::collections::BTreeSet;
        use usage_collector_sdk::MetadataKey;

        let (store, _client, _container) = start().await;
        let gts_id = counter_gts_id("create_conflict_test");

        let ut_counter = counter_usage_type("create_conflict_test");
        store
            .create(ut_counter)
            .await
            .expect("first create must succeed");

        let mut fields = BTreeSet::new();
        fields.insert(MetadataKey::new("region".to_owned()).expect("valid key"));
        let ut_gauge = UsageType {
            gts_id: gts_id.clone(),
            kind: UsageKind::Gauge,
            metadata_fields: fields,
        };

        let err = store
            .create(ut_gauge)
            .await
            .expect_err("differing payload must return AlreadyExists");

        match err {
            UsageCollectorPluginError::UsageTypeAlreadyExists { gts_id: g } => {
                assert_eq!(g, gts_id);
            }
            other => panic!("expected UsageTypeAlreadyExists, got {other:?}"),
        }
    }

    /// Against a live server the unimplemented `delete` still refuses and the
    /// row stays exactly where it was.
    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn delete_is_unimplemented_and_leaves_the_row() {
        let (store, _client, _container) = start().await;
        let ut = counter_usage_type("delete_unimplemented_test");
        let gts_id = ut.gts_id.clone();
        store.create(ut).await.expect("create must succeed");

        let err = store
            .delete(gts_id.clone())
            .await
            .expect_err("delete is not implemented");
        assert!(
            matches!(err, UsageCollectorPluginError::Internal(_)),
            "expected Internal, got {err:?}"
        );

        store
            .get(gts_id)
            .await
            .expect("the usage type must still exist after the refused delete");
    }
}
