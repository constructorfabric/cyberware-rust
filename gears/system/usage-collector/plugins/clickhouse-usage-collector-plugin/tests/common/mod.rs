#![cfg(feature = "clickhouse")]
// Shared across test binaries: not every binary uses every fixture, and these
// fixtures panic on invalid test input by design.
#![allow(dead_code, clippy::expect_used, clippy::unwrap_used)]
//! Shared `ClickHouse` + in-process cluster-lock test harness.
//!
//! Starts a `ClickHouse` container, applies the embedded schema migration, and
//! registers a linearizable `CasBasedDistributedLockBackend` for the
//! `usage-collector` profile. Requires Docker for `ClickHouse` only.

use std::sync::Arc;
use std::time::Duration;

use cluster::defaults::CasBasedDistributedLockBackend;
use cluster_sdk::lock::DistributedLockBackend;
use cluster_sdk::profile::ClusterProfile;
use cluster_sdk::register_lock_backend;
use rust_decimal::Decimal;
use standalone_cluster_plugin::StandaloneCache;
use testcontainers::core::WaitFor;
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, GenericImage, ImageExt};
use time::OffsetDateTime;
use tokio_util::sync::CancellationToken;
use toolkit::client_hub::ClientHub;
use uuid::Uuid;

use usage_collector_sdk::{
    IdempotencyKey, MetadataKey, ResourceRef, SubjectRef, UsageKind, UsageRecord, UsageType,
    UsageTypeGtsId,
};

use clickhouse_usage_collector_plugin::infra::coordination::lock_manager::{
    LockManager, UsageCollectorProfile,
};
use clickhouse_usage_collector_plugin::infra::metrics::Metrics;
use clickhouse_usage_collector_plugin::infra::storage::catalog_store::{
    CatalogLockPort, ChCatalogStore,
};
use clickhouse_usage_collector_plugin::infra::storage::pool::{
    apply_migrations, build_client, ensure_retention_ttl,
};
use clickhouse_usage_collector_plugin::infra::storage::record_store::ChRecordStore;

/// Live testcontainer harness holding a `ClickHouse` container and a shared
/// in-process cluster lock backend.
pub struct ChHarness {
    /// Configured `ClickHouse` HTTP client (pointing at the test container port).
    pub client: clickhouse::Client,
    /// Hub with the `usage-collector` lock backend registered.
    pub hub: Arc<ClientHub>,
    /// Cancellation token for background workers spawned from this harness.
    pub cancel: CancellationToken,
    /// Keep `ClickHouse` container alive.
    _ch_container: ContainerAsync<GenericImage>,
}

/// Password for the container's `default` user, exposed so tests can assert it
/// never leaks (e.g. through a `Debug` impl).
///
/// MUST be non-empty. The official image's entrypoint only provisions
/// `default` with `<networks><ip>::/0</ip></networks>` when `CLICKHOUSE_USER`
/// is non-default **or** `CLICKHOUSE_PASSWORD` is non-empty; otherwise it
/// writes a `users.d` override restricting `default` to `127.0.0.1`/`::1`,
/// which rejects every connection arriving through the mapped host port.
pub const CH_TEST_PASSWORD: &str = "ch_test_pw";

/// Start `ClickHouse`, apply migrations, register the cluster lock backend.
pub async fn bring_up() -> anyhow::Result<ChHarness> {
    // No log-based wait strategy: this image sends the server log (including
    // "Ready for connections") to files under /var/log/clickhouse-server
    // inside the container, so it never appears on stdout/stderr and a
    // `message_on_stdout` wait can only ever time out. Readiness is polled
    // over HTTP below instead.
    let ch_image = GenericImage::new("clickhouse/clickhouse-server", "25.6")
        .with_wait_for(WaitFor::Nothing)
        .with_env_var("CLICKHOUSE_USER", "default")
        .with_env_var("CLICKHOUSE_PASSWORD", CH_TEST_PASSWORD)
        .with_env_var("CLICKHOUSE_DB", "default");
    let ch_container = ch_image.start().await?;
    let ch_port = ch_container.get_host_port_ipv4(8123).await?;

    let cfg: clickhouse_usage_collector_plugin::config::ClickHousePluginConfig =
        serde_json::from_str(&format!(
            r#"{{ "database_url": "http://default:{CH_TEST_PASSWORD}@127.0.0.1:{ch_port}/default",
                  "allow_insecure_http": true,
                  "lock_ttl_secs": 30,
                  "lock_timeout_secs": 5 }}"#
        ))
        .expect("valid test config json");

    let client = build_client(&cfg);

    wait_until_ready(&client).await?;

    let mut last_err = None;
    for _ in 0..20u8 {
        match apply_migrations(&client).await {
            Ok(()) => {
                last_err = None;
                break;
            }
            Err(e) => {
                last_err = Some(e);
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
    }
    if let Some(e) = last_err {
        return Err(e);
    }

    ensure_retention_ttl(&client, cfg.retention_period_secs).await?;

    let hub = Arc::new(ClientHub::default());
    let cache = StandaloneCache::new();
    let backend = CasBasedDistributedLockBackend::new(cache)?;
    register_lock_backend(&hub, UsageCollectorProfile::NAME, Arc::new(backend))?;

    let cancel = CancellationToken::new();
    Ok(ChHarness {
        client,
        hub,
        cancel,
        _ch_container: ch_container,
    })
}

/// Poll `SELECT 1` until the server answers, or give up after ~60s.
async fn wait_until_ready(client: &clickhouse::Client) -> anyhow::Result<()> {
    let mut last_err = None;
    for _ in 0..120u8 {
        match client.query("SELECT 1").fetch_one::<u8>().await {
            Ok(_) => return Ok(()),
            Err(e) => {
                last_err = Some(e);
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
    }
    Err(anyhow::anyhow!(
        "ClickHouse container never became ready: {}",
        last_err.map_or_else(|| "no error recorded".to_owned(), |e| e.to_string())
    ))
}

/// Bring up the harness, or print a Docker-unavailable notice and return
/// `None` for the caller to skip its own test body.
///
/// Skipping is silent to the test harness (the test still reports `ok`), which
/// makes a Docker-less run indistinguishable from a real one — including in a
/// coverage report, where every gated line stays red while the suite claims
/// success. Set `CH_REQUIRE_DOCKER=1` to turn a failed bring-up into a panic
/// instead; coverage runs MUST set it.
pub async fn bring_up_or_skip() -> Option<ChHarness> {
    match bring_up().await {
        Ok(h) => Some(h),
        Err(e) => {
            assert!(
                !std::env::var("CH_REQUIRE_DOCKER").is_ok_and(|v| v == "1"),
                "CH_REQUIRE_DOCKER=1 but the ClickHouse test harness failed to start: {e}"
            );
            eprintln!(
                "DOCKER UNAVAILABLE — skipping test (bring_up failed): {e}\n\
                 Run `cargo test -p cf-gears-clickhouse-usage-collector-plugin \
                 --features clickhouse` with Docker available to execute these tests."
            );
            None
        }
    }
}

/// Build a fresh metric inventory (recording is a no-op without an exporter).
#[must_use]
pub fn metrics() -> Arc<Metrics> {
    Arc::new(Metrics::new())
}

/// Build a [`LockManager`] against the harness hub.
#[must_use]
pub fn lock_manager(hub: &Arc<ClientHub>) -> Arc<LockManager> {
    lock_manager_with_timeout(hub, Duration::from_secs(5))
}

/// Same as [`lock_manager`], but with a caller-supplied acquire timeout.
#[must_use]
pub fn lock_manager_with_timeout(hub: &Arc<ClientHub>, timeout: Duration) -> Arc<LockManager> {
    Arc::new(LockManager::new(
        Arc::clone(hub),
        Duration::from_secs(30),
        timeout,
        metrics(),
    ))
}

/// Build a [`ChRecordStore`] with its own [`LockManager`] and metric handle.
#[must_use]
pub fn record_store(h: &ChHarness) -> ChRecordStore {
    record_store_over(h, h.client.clone())
}

/// Same as [`record_store`], but over a caller-supplied `ClickHouse` client
/// (e.g. [`unreachable_client`]) while keeping the harness's live lock backend,
/// so a call reaches its SQL instead of failing at lock acquisition.
#[must_use]
pub fn record_store_over(h: &ChHarness, client: clickhouse::Client) -> ChRecordStore {
    ChRecordStore::new(client, lock_manager(&h.hub), metrics())
}

/// Same as [`record_store`], but with a caller-supplied cluster lock backend
/// registered for the `usage-collector` profile on its own hub, for driving the
/// guard failure paths inside `create`'s critical section against a live
/// `ClickHouse` client.
///
/// `ChRecordStore` owns a concrete [`LockManager`], so the injection point is
/// the cluster backend the manager resolves rather than a store-level port.
#[must_use]
pub fn record_store_with_lock_backend(
    h: &ChHarness,
    backend: Arc<dyn DistributedLockBackend>,
) -> ChRecordStore {
    let hub = Arc::new(ClientHub::default());
    register_lock_backend(&hub, UsageCollectorProfile::NAME, backend)
        .expect("register the caller-supplied lock backend");
    ChRecordStore::new(h.client.clone(), lock_manager(&hub), metrics())
}

/// Build a [`ChCatalogStore`] with its own [`LockManager`] and metric handle.
#[must_use]
pub fn catalog_store(h: &ChHarness) -> ChCatalogStore {
    catalog_store_over(h, h.client.clone())
}

/// Same as [`catalog_store`], but over a caller-supplied `ClickHouse` client
/// (e.g. [`unreachable_client`]) while keeping the harness's live lock backend.
#[must_use]
pub fn catalog_store_over(h: &ChHarness, client: clickhouse::Client) -> ChCatalogStore {
    let lock_port: Arc<dyn CatalogLockPort> = lock_manager(&h.hub);
    ChCatalogStore::new(client, lock_port, h.cancel.clone(), metrics())
}

/// Same as [`catalog_store`], but with a caller-supplied lock port, for driving
/// the guard failure paths inside `delete`'s critical section against a live
/// `ClickHouse` client.
#[must_use]
pub fn catalog_store_with_lock(
    h: &ChHarness,
    lock_port: Arc<dyn CatalogLockPort>,
) -> ChCatalogStore {
    ChCatalogStore::new(h.client.clone(), lock_port, h.cancel.clone(), metrics())
}

/// A client pointed at a port with nothing listening, so every statement fails
/// fast with a connection error instead of hanging.
#[must_use]
pub fn unreachable_client() -> clickhouse::Client {
    // Port 1 is reserved and never bound by the test harness.
    clickhouse::Client::default().with_url("http://127.0.0.1:1")
}

/// Process-wide base instant (unix seconds) for fixture `created_at` values.
///
/// Anchored to the current clock, **not** a hardcoded epoch: `usage_records`
/// carries `TTL created_at + INTERVAL retention_period_secs SECOND DELETE`
/// (365 days by default), so a fixture timestamp older than the retention
/// window makes every inserted row immediately TTL-expired — a background
/// merge (or a `FINAL` read that triggers one) then drops it mid-test, and
/// any reference/aggregation assertion fails depending on timing. A
/// hardcoded epoch works until it ages past the window and then rots the
/// whole suite.
///
/// Resolved once per process so it stays deterministic within a run: the dedup
/// tests build two fixtures independently and rely on them sharing the same
/// `(tenant_id, gts_id, created_at, id)` key. Offset well into the past so
/// callers adding per-record offsets (`base + i`) stay in the past too.
#[must_use]
pub fn fixture_base_ts() -> i64 {
    static BASE: std::sync::OnceLock<i64> = std::sync::OnceLock::new();
    *BASE.get_or_init(|| {
        OffsetDateTime::now_utc()
            .unix_timestamp()
            .saturating_sub(48 * 60 * 60)
    })
}

/// Build a valid [`UsageTypeGtsId`] from a raw string.
#[must_use]
pub fn fixture_gts_id(gts: &str) -> UsageTypeGtsId {
    UsageTypeGtsId::new(gts).expect("fixture gts_id must be a valid usage-type GTS instance id")
}

/// Build a [`UsageType`] fixture from raw parts.
#[must_use]
pub fn fixture_usage_type(gts: &str, kind: &str, fields: &[&str]) -> UsageType {
    let kind: UsageKind = kind.parse().expect("fixture kind must be counter/gauge");
    let metadata_fields = fields
        .iter()
        .map(|f| MetadataKey::new(*f).expect("fixture metadata field must be valid"))
        .collect();
    UsageType {
        gts_id: fixture_gts_id(gts),
        kind,
        metadata_fields,
    }
}

/// Build a minimal [`UsageRecord`] fixture referencing `gts_id`.
#[must_use]
pub fn fixture_usage_record(
    gts: &str,
    tenant_id: Uuid,
    idem: &str,
    value: Decimal,
    seq: u128,
) -> UsageRecord {
    UsageRecord {
        id: Uuid::from_u128(seq),
        gts_id: fixture_gts_id(gts),
        tenant_id,
        resource_ref: ResourceRef::new("res-1", "compute.vm")
            .expect("fixture resource_ref must be valid"),
        subject_ref: None,
        metadata: std::collections::BTreeMap::new(),
        value,
        idempotency_key: IdempotencyKey::new(idem).expect("fixture idempotency_key must be valid"),
        corrects_id: None,
        status: usage_collector_sdk::UsageRecordStatus::Active,
        created_at: OffsetDateTime::from_unix_timestamp(fixture_base_ts())
            .expect("fixture created_at must be a valid unix timestamp"),
    }
}

/// Build a [`UsageRecord`] fixture with a caller-chosen `resource_id`.
#[must_use]
pub fn fixture_usage_record_with_resource(
    gts: &str,
    tenant_id: Uuid,
    idem: &str,
    value: Decimal,
    seq: u128,
    resource_id: &str,
) -> UsageRecord {
    let mut rec = fixture_usage_record(gts, tenant_id, idem, value, seq);
    rec.resource_ref =
        ResourceRef::new(resource_id, "compute.vm").expect("fixture resource_ref must be valid");
    rec
}

/// Build a [`UsageRecord`] fixture carrying a `subject_ref`.
#[must_use]
pub fn fixture_usage_record_with_subject(
    gts: &str,
    tenant_id: Uuid,
    idem: &str,
    value: Decimal,
    seq: u128,
    subject_id: &str,
    subject_type: Option<&str>,
) -> UsageRecord {
    let mut rec = fixture_usage_record(gts, tenant_id, idem, value, seq);
    rec.subject_ref =
        Some(SubjectRef::new(subject_id, subject_type).expect("fixture subject_ref must be valid"));
    rec
}
