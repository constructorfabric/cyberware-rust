use std::sync::Arc;

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
use crate::infra::metrics::Metrics;
use crate::infra::storage::catalog_store::ChCatalogStore;
use crate::infra::storage::pool::{
    apply_migrations, build_client, ensure_insert_dedup_window, ensure_retention_ttl,
};
use crate::infra::storage::record_store::ChRecordStore;

/// `ClickHouse` Usage Collector storage backend plugin module.
///
/// Conforms to the storage Plugin SPI: connects and migrates a `ClickHouse`
/// database, performs the full GTS registration handshake, then registers
/// the scoped `StorageAdapter` client so the plugin host resolves it on
/// first dispatch.
///
/// Its only gear dependency is `types_registry` (for the registration
/// handshake); no cluster or coordination backend is required.
#[toolkit::gear(name = "clickhouse-usage-collector-plugin", deps = [types_registry])]
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

        // --- Three-step init sequence ---

        // Step A: Build the ClickHouse HTTP client and configure timeouts / pool.
        let client = build_client(&cfg);

        // Step B: Run the embedded idempotent schema migration, then reconcile
        // usage_records TTL with the configured retention window. Both are
        // bounded by the same client-side deadline the request path uses: a
        // hung init is worse than a failed one, since it never surfaces.
        let client_deadline = cfg.client_deadline();
        apply_migrations(&client, client_deadline)
            .await
            .inspect_err(|_| metrics.inc_migration_failure())?;
        ensure_retention_ttl(&client, cfg.retention_period_secs, client_deadline)
            .await
            .inspect_err(|_| metrics.inc_migration_failure())?;
        ensure_insert_dedup_window(&client, client_deadline)
            .await
            .inspect_err(|_| metrics.inc_migration_failure())?;

        // Step C: Build the domain stores and wire them into the StorageAdapter.
        //
        // Both stores share the same ClickHouse client (cheaply cloneable handle
        // to the shared HTTP pool). The cancel token is threaded in so the
        // catalog-size refresh worker aborts on shutdown.
        let cancel = ctx.cancellation_token().clone();

        let record_store: Arc<dyn RecordStore> = Arc::new(ChRecordStore::new(
            client.clone(),
            Arc::clone(&metrics),
            client_deadline,
            cfg.async_insert,
        ));

        let catalog_store: Arc<dyn CatalogStore> = Arc::new(ChCatalogStore::new(
            client,
            cancel,
            Arc::clone(&metrics),
            client_deadline,
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
