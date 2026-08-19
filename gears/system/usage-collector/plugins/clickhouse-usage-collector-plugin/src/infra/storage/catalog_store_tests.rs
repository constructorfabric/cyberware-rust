// Test modules using bare `panic!` opt in explicitly.
#![allow(clippy::panic)]
#![cfg_attr(coverage_nightly, coverage(off))]

//! Unit tests for [`ChCatalogStore`].
//!
//! Tests 1–4 are offline unit tests: no live `ClickHouse` server or cluster lock
//! instance is required. They exercise the refresh-worker cancellation /
//! coalescing behaviour and the delete lock-failure path.
//!
//! Test 5 exercises the `ensure_still_held` path via the [`LockGuardPort`]
//! seam: a stub guard returns `Transient`, and the test asserts that `delete`
//! propagates it without deleting the row.
//!
//! Tests 6–9 are gated behind `#[cfg(feature = "clickhouse")]` because they
//! require a live `ClickHouse` server to return meaningful query results.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;
use usage_collector_sdk::UsageCollectorPluginError;

use super::{CatalogLockPort, ChCatalogStore, RefreshOutcome};
use crate::infra::coordination::lock_manager::LockGuardPort;
use crate::infra::metrics::Metrics;

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Build a catalog store over an offline `ClickHouse` client with a
/// caller-chosen cancellation token.
///
/// The client is pointed at the default `ClickHouse` address
/// (`http://localhost:8123/`). Any query issued against it fails quickly
/// (connection refused) rather than blocking, keeping test duration low.
pub(super) fn offline_store(cancel: CancellationToken) -> ChCatalogStore {
    ChCatalogStore::new(
        clickhouse::Client::default(),
        Arc::new(AlwaysGrantLock),
        cancel,
        Arc::new(Metrics::new()),
    )
}

// ── Lock stubs ────────────────────────────────────────────────────────────────

/// Guard stub that always reports the session as still held.
pub(super) struct GrantedGuard;

#[async_trait]
impl LockGuardPort for GrantedGuard {
    async fn ensure_still_held(&self) -> Result<(), UsageCollectorPluginError> {
        Ok(())
    }

    async fn release(self: Box<Self>) -> Result<(), UsageCollectorPluginError> {
        Ok(())
    }
}

/// Lock stub that always grants the exclusive lock immediately.
///
/// The returned guard is a [`GrantedGuard`] — `ensure_still_held` returns
/// `Ok(())` so it never signals session loss.
pub(super) struct AlwaysGrantLock;

#[async_trait]
impl CatalogLockPort for AlwaysGrantLock {
    async fn acquire_exclusive_for_delete(
        &self,
        _gts_id: &str,
    ) -> Result<Box<dyn LockGuardPort>, UsageCollectorPluginError> {
        Ok(Box::new(GrantedGuard))
    }
}

/// Guard stub that always reports session loss from `ensure_still_held`.
///
/// Used to exercise the session-validity check path in
/// [`ChCatalogStore::delete`] without a live cluster lock.
pub(super) struct SessionLostGuard;

#[async_trait]
impl LockGuardPort for SessionLostGuard {
    async fn ensure_still_held(&self) -> Result<(), UsageCollectorPluginError> {
        Err(UsageCollectorPluginError::transient(
            "cluster lock lease lost (test stub)",
        ))
    }

    async fn release(self: Box<Self>) -> Result<(), UsageCollectorPluginError> {
        Ok(())
    }
}

/// Lock stub that grants the lock (returns `Ok`) but provides a guard whose
/// `ensure_still_held` always fails — simulating a session that expires
/// between lock grant and the critical write.
pub(super) struct GrantThenLoseSession;

#[async_trait]
impl CatalogLockPort for GrantThenLoseSession {
    async fn acquire_exclusive_for_delete(
        &self,
        _gts_id: &str,
    ) -> Result<Box<dyn LockGuardPort>, UsageCollectorPluginError> {
        Ok(Box::new(SessionLostGuard))
    }
}

/// Lock stub that always returns `Transient` (simulates cluster lock unavailability
/// at lock acquisition time — before `ensure_still_held` is reached).
pub(super) struct AlwaysTransientLock;

#[async_trait]
impl CatalogLockPort for AlwaysTransientLock {
    async fn acquire_exclusive_for_delete(
        &self,
        _gts_id: &str,
    ) -> Result<Box<dyn LockGuardPort>, UsageCollectorPluginError> {
        Err(UsageCollectorPluginError::transient(
            "cluster lock unavailable (test stub)",
        ))
    }
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

    // The count query attempts a connection to localhost:8123. Whether it
    // succeeds or fails (connection refused against an offline server), the
    // outcome is always `Ran` — failure is logged at `warn`, not propagated.
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
        Arc::new(AlwaysGrantLock),
        cancel.clone(),
        Arc::new(Metrics::new()),
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

// ── Test 4: delete returns Transient when lock manager is unavailable ─────────

#[tokio::test]
async fn delete_returns_transient_on_lock_manager_unavailable() {
    use usage_collector_sdk::{UsageCollectorPluginError, UsageTypeGtsId};

    use crate::domain::ports::CatalogStore;

    let cancel = CancellationToken::new();
    let store = ChCatalogStore::new(
        clickhouse::Client::default(),
        Arc::new(AlwaysTransientLock), // always denies the lock at acquisition
        cancel,
        Arc::new(Metrics::new()),
    );

    let gts_id =
        UsageTypeGtsId::new("gts.cf.core.uc.usage_record.v1~cf.compute._.lock_fail_test.v1")
            .expect("valid gts_id");

    // The lock stub returns Transient before any SQL is issued.
    // MUST NOT proceed without the lock (DESIGN.md §3.6 step 7, fail-closed).
    let err = store
        .delete(gts_id)
        .await
        .expect_err("delete must fail when the lock manager is unavailable");

    match err {
        UsageCollectorPluginError::Transient { .. } => {} // expected
        other => panic!("expected Transient, got {other:?}"),
    }
}

// ── Test 5: delete fails closed when the lease cannot be confirmed ────────────

/// `delete` never reports success while holding a guard that cannot confirm
/// its lease.
///
/// Scope note: offline, the SQL steps that precede `ensure_still_held` (the
/// existence check and the reference probe) fail first against the unreachable
/// `localhost:8123`, so this asserts only the fail-closed outcome — that
/// `delete` returns an error and therefore issues no `DELETE`. The
/// `ensure_still_held` branch itself is reached only with a live server, which
/// `live::delete_aborts_when_the_lease_cannot_be_renewed` covers under
/// `feature = "clickhouse"`.
#[tokio::test]
async fn delete_fails_closed_when_the_lease_cannot_be_confirmed() {
    use usage_collector_sdk::{UsageCollectorPluginError, UsageTypeGtsId};

    use crate::domain::ports::CatalogStore;

    let cancel = CancellationToken::new();
    let store = ChCatalogStore::new(
        clickhouse::Client::default(),
        Arc::new(GrantThenLoseSession), // grants lock but guard.ensure_still_held() → Err
        cancel,
        Arc::new(Metrics::new()),
    );

    let gts_id =
        UsageTypeGtsId::new("gts.cf.core.uc.usage_record.v1~cf.compute._.session_lost_test.v1")
            .expect("valid gts_id");

    let err = store
        .delete(gts_id)
        .await
        .expect_err("delete must fail rather than proceed without a confirmed lease");

    match err {
        UsageCollectorPluginError::Transient { .. }
        | UsageCollectorPluginError::Internal { .. } => {}
        other => panic!("expected Transient or Internal, got {other:?}"),
    }
}

// ── Test 5b: create fails closed when the lock manager is unavailable ─────────

/// `create` takes the same exclusive per-`gts_id` lock as `delete`, so an
/// unavailable lock manager must stop it before any SQL is issued rather than
/// letting two concurrent creates race (DESIGN.md §3.6 step 7, fail-closed).
#[tokio::test]
async fn create_returns_transient_on_lock_manager_unavailable() {
    use std::collections::BTreeSet;

    use usage_collector_sdk::{UsageCollectorPluginError, UsageKind, UsageType, UsageTypeGtsId};

    use crate::domain::ports::CatalogStore;

    let cancel = CancellationToken::new();
    let store = ChCatalogStore::new(
        clickhouse::Client::default(),
        Arc::new(AlwaysTransientLock),
        cancel,
        Arc::new(Metrics::new()),
    );

    let usage_type = UsageType {
        gts_id: UsageTypeGtsId::new(
            "gts.cf.core.uc.usage_record.v1~cf.compute._.create_lock_fail_test.v1",
        )
        .expect("valid gts_id"),
        kind: UsageKind::Counter,
        metadata_fields: BTreeSet::new(),
    };

    let err = store
        .create(usage_type)
        .await
        .expect_err("create must fail when the lock manager is unavailable");

    match err {
        UsageCollectorPluginError::Transient { .. } => {}
        other => panic!("expected Transient, got {other:?}"),
    }
}

/// Regression guard for `create`'s version assignment.
///
/// `usage_type_catalog` is a `ReplacingMergeTree(version)` ordered by `(gts_id)`,
/// so a `FINAL` read resolves to whichever physical row carries the highest
/// `version`. Two concurrent `create` calls for the same `gts_id` can both
/// pass the pre-existence check and race to `INSERT`; distinct, monotonically
/// increasing versions are what let `FINAL` resolution deterministically pick
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

// ── Tests 6-9: `ClickHouse`-backed paths (require live server) ────────────────
//
// These tests exercise SQL paths that need a live `ClickHouse` server to return
// meaningful results (empty row / live row / count > 0). Without a server,
// `fetch_optional` returns a network error (mapped to `Transient`) which is
// indistinguishable from a connectivity issue, so the assertions cannot pass.
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

    use super::{AlwaysGrantLock, GrantThenLoseSession};

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

        apply_migrations(&client)
            .await
            .expect("schema migrations must succeed on the live test container");
        ensure_retention_ttl(&client, 365 * 24 * 3600)
            .await
            .expect("retention TTL reconcile must succeed on the live test container");

        let store = ChCatalogStore::new(
            client.clone(),
            Arc::new(AlwaysGrantLock),
            CancellationToken::new(),
            Arc::new(Metrics::new()),
        );
        (store, client, container)
    }

    /// Insert one `usage_records` row referencing `gts_id` so the delete
    /// reference probe has something to find.
    async fn seed_reference(client: &clickhouse::Client, gts_id: &UsageTypeGtsId) {
        let sql = "INSERT INTO usage_records (id, tenant_id, gts_id, value, created_at, \
                   resource_id, resource_type, subject_id, subject_type, idempotency_key, \
                   corrects_id, status, metadata, ingested_at, version) VALUES \
                   (generateUUIDv4(), generateUUIDv4(), ?, 1, now64(6), 'res-1', 'vm', NULL, \
                   NULL, 'idem-ref', NULL, 'active', map(), now64(6), 1)";
        client
            .query(sql)
            .bind(gts_id.as_ref())
            .execute()
            .await
            .expect("seeding a referencing usage record must succeed");
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

    // Test 6
    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn delete_returns_usage_type_not_found_when_absent() {
        let (store, _client, _container) = start().await;
        let gts_id = counter_gts_id("absent_delete_test");

        let err = store
            .delete(gts_id.clone())
            .await
            .expect_err("delete of absent type must fail");

        match err {
            UsageCollectorPluginError::UsageTypeNotFound { gts_id: g } => {
                assert_eq!(g, gts_id);
            }
            other => panic!("expected UsageTypeNotFound, got {other:?}"),
        }
    }

    // Test 7
    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn delete_returns_usage_type_referenced_when_ref_count_positive() {
        let (store, client, _container) = start().await;
        let ut = counter_usage_type("delete_referenced_test");
        let gts_id = ut.gts_id.clone();

        store.create(ut).await.expect("create must succeed");
        seed_reference(&client, &gts_id).await;

        let err = store
            .delete(gts_id.clone())
            .await
            .expect_err("a referenced usage type must not be deletable");

        match err {
            UsageCollectorPluginError::UsageTypeReferenced {
                gts_id: g,
                sample_ref_count,
            } => {
                assert_eq!(g, gts_id);
                assert!(
                    sample_ref_count >= 1,
                    "the probe must report at least the seeded reference, got {sample_ref_count}"
                );
            }
            other => panic!("expected UsageTypeReferenced, got {other:?}"),
        }

        // The type must still be there — a rejected delete removes nothing.
        store
            .get(gts_id)
            .await
            .expect("a rejected delete must leave the usage type in place");
    }

    // Test 7b: the `ensure_still_held` branch of `delete`, isolated.
    //
    // Offline the SQL steps ahead of the lease renewal fail first, so this is
    // the only place the renewal branch itself is exercised: the reads succeed
    // against the live server, the stub guard then reports the lease lost, and
    // the DELETE must never be issued.
    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn delete_aborts_when_the_lease_cannot_be_renewed() {
        let (seed_store, client, _container) = start().await;
        let ut = counter_usage_type("lease_lost_delete_test");
        let gts_id = ut.gts_id.clone();
        seed_store.create(ut).await.expect("create must succeed");

        let store = ChCatalogStore::new(
            client,
            Arc::new(GrantThenLoseSession),
            CancellationToken::new(),
            Arc::new(Metrics::new()),
        );

        let err = store
            .delete(gts_id.clone())
            .await
            .expect_err("a lost lease must abort the delete");

        match err {
            UsageCollectorPluginError::Transient { .. } => {}
            other => panic!("expected Transient from the lease renewal, got {other:?}"),
        }

        seed_store
            .get(gts_id.clone())
            .await
            .expect("the row must survive a delete aborted at the lease check");

        seed_store.delete(gts_id).await.ok();
    }

    // Test 8
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

        store.delete(ut.gts_id).await.ok();
    }

    // Test 9
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

        store.delete(gts_id).await.ok();
    }
}
