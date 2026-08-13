use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use toolkit::Gear;
use toolkit::client_hub::ClientScope;
use toolkit::context::GearCtx;
use toolkit::gts::PluginV1;
use tracing::info;
use types_registry_sdk::{RegisterResult, TypesRegistryClient};
use usage_collector_sdk::{UsageCollectorPluginSpecV1, UsageCollectorPluginV1};

use crate::config::ClickHousePluginConfig;
use crate::domain::adapter::StorageAdapter;
use crate::domain::ports::{CatalogStore, RecordStore};
use crate::infra::coordination::lock_manager::LockManager;
use crate::infra::metrics::Metrics;
use crate::infra::storage::catalog_store::{CatalogLockPort, ChCatalogStore};
use crate::infra::storage::pool::{apply_migrations, build_client};
use crate::infra::storage::record_store::ChRecordStore;

/// `ClickHouse` Usage Collector storage backend plugin module.
///
/// Conforms to the storage Plugin SPI: connects and migrates a `ClickHouse`
/// database, performs the full GTS registration handshake, then registers
/// the scoped `StorageAdapter` client so the plugin host resolves it on
/// first dispatch.
///
/// Depends on `cluster` so the `usage-collector` profile's distributed-lock
/// backend is initialized in topo order (backends themselves register during
/// cluster `start`; this plugin resolves them lazily on first lock acquire).
#[toolkit::gear(
    name = "clickhouse-usage-collector-plugin",
    deps = [types_registry, cluster]
)]
#[derive(Default)]
pub struct ClickHouseUsageCollectorPlugin;

#[async_trait]
impl Gear for ClickHouseUsageCollectorPlugin {
    // @cpt-flow:cpt-cf-usage-collector-flow-foundation-plugin-host-binding:p1
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let cfg: ClickHousePluginConfig = ctx.config_expanded_or_default()?;
        cfg.validate().map_err(|e| {
            anyhow::anyhow!("invalid clickhouse-usage-collector-plugin config: {e}")
        })?;

        // Build the metric inventory once; shared via Arc across all stores.
        let metrics = Arc::new(Metrics::new());

        // Publish readiness as 0 before any startup I/O so an init that never
        // completes is distinguishable from a gear that never started at all
        // (no series). Flipped to 1 only after the full sequence below.
        metrics.set_ready(false);

        // --- Four-step init sequence ---

        // Step A: Build the ClickHouse HTTP client and configure timeouts / pool.
        let client = build_client(&cfg);

        // Step B: Run the embedded idempotent schema migration (CREATE TABLE IF NOT EXISTS).
        apply_migrations(&client, cfg.retention_period_secs)
            .await
            .inspect_err(|_| metrics.inc_migration_failure())?;

        // Step C: Build the Coordination Lock Manager (cluster DistributedLockV1).
        // Resolves lazily on first acquire — cluster registers backends in
        // `start()`, which runs after this `init()`.
        let lock_manager = Arc::new(LockManager::new(
            ctx.client_hub(),
            Duration::from_secs(cfg.lock_ttl_secs),
            Duration::from_secs(cfg.lock_timeout_secs),
            Arc::clone(&metrics),
        ));

        // Step D: Build the domain stores and wire them into the StorageAdapter.
        //
        // Both stores share the same ClickHouse client (cheaply cloneable handle
        // to the shared HTTP pool) and LockManager (shared cluster lock facade).
        // The cancel token is threaded in so the catalog-size refresh worker
        // aborts on shutdown.
        let cancel = ctx.cancellation_token().clone();

        // Coerce Arc<LockManager> → Arc<dyn CatalogLockPort> at the binding site
        // so both stores receive the erased type their test interfaces expect.
        let lock_port: Arc<dyn CatalogLockPort> = lock_manager;

        let record_store: Arc<dyn RecordStore> = Arc::new(ChRecordStore::new(
            client.clone(),
            Arc::clone(&lock_port),
            Arc::clone(&metrics),
        ));

        let catalog_store: Arc<dyn CatalogStore> = Arc::new(ChCatalogStore::new(
            client,
            lock_port,
            cancel,
            Arc::clone(&metrics),
        ));

        // Construct the SPI adapter over the real stores.
        let service: Arc<dyn UsageCollectorPluginV1> =
            Arc::new(StorageAdapter::new(record_store, catalog_store));

        // --- Four-step GTS / types-registry / ClientHub handshake ---

        // Step 1: build registration payload for this plugin instance.
        let (instance_id, instance_json) =
            PluginV1::<UsageCollectorPluginSpecV1>::build_registration(
                "cf.core._.clickhouse_usage_collector.v1",
                cfg.vendor.clone(),
                cfg.priority,
            )?;

        // Step 2: publish to types-registry.
        let registry = ctx.client_hub().get::<dyn TypesRegistryClient>()?;
        let results = registry.register(vec![instance_json]).await?;

        // Step 3: ensure all registrations succeeded.
        RegisterResult::ensure_all_ok(&results)?;

        // Step 4: register the scoped backend client in ClientHub so the plugin
        // host resolves it on first dispatch.
        ctx.client_hub()
            .register_scoped::<dyn UsageCollectorPluginV1>(
                ClientScope::gts_id(&instance_id),
                service,
            );

        // Signal plugin-local readiness after a successful init; the background
        // catalog-size refresh worker MUST NOT re-arm this to 1. The Gear trait
        // exposes no shutdown hook, so the cancellation token is the only
        // shutdown signal: a detached watcher clears the gauge instead of
        // leaving it stuck at 1 for a drained replica. Spawned after
        // `set_ready(true)` so a cancellation that already fired is still
        // observed — `cancelled()` resolves immediately on a cancelled token.
        metrics.set_ready(true);
        let shutdown = ctx.cancellation_token().clone();
        let ready_metrics = Arc::clone(&metrics);
        tokio::spawn(async move {
            shutdown.cancelled().await;
            ready_metrics.set_ready(false);
        });

        info!(
            instance_id = %instance_id,
            vendor = %cfg.vendor,
            priority = cfg.priority,
            "Registered ClickHouse usage-collector plugin instance"
        );
        Ok(())
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "gear_tests.rs"]
mod gear_tests;
