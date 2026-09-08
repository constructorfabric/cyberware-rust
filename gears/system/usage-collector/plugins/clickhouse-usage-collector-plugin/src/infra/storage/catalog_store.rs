//! `ClickHouse`-backed [`CatalogStore`] over the `usage_type_catalog` table.
//!
//! Implements `create` / `get` / `list` against `ClickHouse` using the
//! `clickhouse` 0.15.x crate. Every read resolves
//! `ReplacingMergeTree(version)` explicitly in SQL — `ORDER BY version DESC`
//! plus `LIMIT 1 [BY gts_id]`, see [`query::dedup`] — so the highest-version
//! physical copy is what every read observes, without `FINAL`'s merge-on-read
//! cost.
//!
//! There is no coordination primitive: `create` is a plain version-resolved
//! pre-existence read followed by an `INSERT`. Two concurrent creates for the
//! same `gts_id` can both pass the read and both insert; `ReplacingMergeTree`
//! then converges the physical rows to the one with the highest `version`
//! (epoch microseconds at compose time), so the catalog is last-writer-wins
//! inside that window and neither caller observes `UsageTypeAlreadyExists`.
//! Outside the window the loser sees the winner's row and gets either a
//! silent absorb (identical payload) or `UsageTypeAlreadyExists`.
//!
//! `delete` emulates the reference plugin's `ON DELETE RESTRICT` without a
//! foreign key and without a mutual-exclusion primitive: existence read →
//! capped reference probe → `ALTER TABLE … DELETE` of the catalog row →
//! re-probe-gated sweep of any record that landed in between. The catalog row
//! is removed *before* the sweep on purpose — once it is gone, the record
//! store's insert-time existence check
//! ([`crate::infra::storage::record_store`]) rejects new records for the
//! `gts_id` on its own, which is what bounds the window to the span between
//! the probe and the delete. That window is **not** closed: an insert whose
//! own catalog check passed before the delete can still commit after the
//! sweep and orphan a row. See [`CatalogStore::delete`] for the full list of
//! accepted residuals.
//!
//! A single background refresh worker tracks the live catalog count via the
//! `uc_clickhouse_usage_type_catalog_size` gauge; because `delete` removes
//! rows, that gauge is not monotone.

use std::sync::Arc;
#[cfg(test)]
use std::sync::atomic::AtomicUsize;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use toolkit_odata::filter::{FilterField, convert_expr_to_filter_node};
use toolkit_odata::{ODataOrderBy, ODataQuery, OrderKey, Page as ODataPage, PageInfo, SortDir};
use usage_collector_sdk::{
    UsageCollectorPluginError, UsageType, UsageTypeFilterField, UsageTypeGtsId,
    is_keyset_safe_type_field,
};

use crate::domain::ports::CatalogStore;
use crate::infra::metrics::Metrics;
use crate::infra::storage::entity::{UsageTypeKindCode, UsageTypeRow};
use crate::infra::storage::error::{tracked_ch_err, with_deadline};
use crate::infra::storage::mapper::current_merge_version;
use crate::infra::storage::pool::configure_insert;
use crate::infra::storage::query::dedup::{LATEST_ONE, latest_by};
use crate::infra::storage::query::keyset::{
    encode_next_cursor, ensure_forward_cursor, keyset_predicate,
};
use crate::infra::storage::query::translate::{
    SqlCtx, bind_one, translate_usage_type_filter, usage_type_column,
};
use crate::infra::storage::query::{DEFAULT_PAGE_SIZE, effective_page_size};

// ── Static column list ────────────────────────────────────────────────────────

/// All columns in [`UsageTypeRow`] field order for `usage_type_catalog` SELECT.
///
/// A `'static` constant (never caller input), so `RowBinary` decoding is
/// positional without SQL injection risk.
const TYPE_COLUMNS: &str = "gts_id, kind, metadata_fields, version";

/// Upper bound on the pre-delete reference probe (`sample_ref_count`).
///
/// The count is a coarse diagnostic on the `delete` refusal path, so the read
/// is capped rather than run unbounded over `usage_records`; the SPI declares
/// `sample_ref_count` a bounded, plugin-tunable value. Same value as the
/// reference plugin's `REF_COUNT_CAP`.
const REF_COUNT_CAP: u64 = 1000;

/// `mutations_sync` value used by both `ALTER TABLE … DELETE` statements.
///
/// `1` waits for the mutation to complete on the server that received it,
/// which is what makes `delete` observable to the caller's next read. `2`
/// (wait for all replicas) is deliberately not used: the shipped engine is
/// non-replicated `ReplacingMergeTree`, so there is no replica to wait for,
/// and on a replicated deployment the cross-node visibility lag is called out
/// as an accepted residual on [`CatalogStore::delete`] rather than paid for on
/// every request.
const MUTATIONS_SYNC: &str = "1";

// ── Helpers ───────────────────────────────────────────────────────────────────

/// The catalog's fixed sort order (`gts_id` ascending), used to encode the
/// next-page cursor. The catalog list ignores `query.order` by design.
fn gts_id_asc_order() -> ODataOrderBy {
    ODataOrderBy(vec![OrderKey {
        field: "gts_id".to_owned(),
        dir: SortDir::Asc,
    }])
}

// ── RefreshOutcome ────────────────────────────────────────────────────────────

/// Outcome of one background catalog-size refresh.
///
/// Surfaced so the worker can stop on cancel and unit tests can assert the
/// cancellation short-circuit without touching the `ClickHouse` client.
#[derive(Debug, PartialEq, Eq)]
enum RefreshOutcome {
    /// The cancellation token fired before the count completed; the previous
    /// count value is unchanged and no query is issued past cancellation.
    Cancelled,
    /// The `count()` ran to completion (success or a logged failure).
    Ran,
}

// ── ChCatalogStore ────────────────────────────────────────────────────────────

/// `ClickHouse`-backed implementation of [`CatalogStore`] over
/// `usage_type_catalog`.
#[derive(Clone)]
pub struct ChCatalogStore {
    client: clickhouse::Client,
    metrics: Arc<Metrics>,
    /// Client-side deadline applied to every individual `ClickHouse` await.
    request_timeout: Duration,
    /// Gear shutdown token; races the background catalog-size refresh so a
    /// shutdown drops the in-flight `count()` promptly.
    cancel: CancellationToken,
    /// Coalesces catalog-mutation refresh requests into at most one queued
    /// run: `notify_one` stores a single pending permit regardless of how
    /// many concurrent mutations fire it.
    refresh_signal: Arc<Notify>,
    /// Test-only counter: number of times the worker actually ran the count.
    /// Gates on `#[cfg(test)]` so it has zero footprint in production builds.
    #[cfg(test)]
    refresh_runs: Arc<AtomicUsize>,
}

impl std::fmt::Debug for ChCatalogStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // clickhouse::Client does not implement Debug; omit it.
        f.debug_struct("ChCatalogStore").finish_non_exhaustive()
    }
}

impl ChCatalogStore {
    /// Build a store from an existing `ClickHouse` client and metric
    /// inventory, then spawn the single background catalog-size refresh
    /// worker.
    ///
    /// `cancel` is the gear's cancellation token
    /// ([`toolkit::context::GearCtx::cancellation_token`]); the refresh worker
    /// races its `count()` against it so a shutdown drops the in-flight query,
    /// returns its connection promptly, and the worker exits.
    ///
    /// `request_timeout` bounds every individual `ClickHouse` await; production
    /// wiring passes `ClickHousePluginConfig::client_deadline()`. It is an
    /// explicit parameter rather than a defaulted builder step so a future
    /// wiring change cannot silently leave a store on a default that disagrees
    /// with the configured budget.
    ///
    /// Must be invoked within a Tokio runtime (the worker is spawned eagerly).
    #[must_use]
    pub fn new(
        client: clickhouse::Client,
        cancel: CancellationToken,
        metrics: Arc<Metrics>,
        request_timeout: Duration,
    ) -> Self {
        let store = Self {
            client,
            metrics,
            request_timeout,
            cancel,
            refresh_signal: Arc::new(Notify::new()),
            #[cfg(test)]
            refresh_runs: Arc::new(AtomicUsize::new(0)),
        };
        store.spawn_refresh_worker();
        store
    }

    /// Signal the background worker to run a catalog-size refresh off the
    /// request path.
    ///
    /// `notify_one` coalesces a burst of concurrent `create` calls into at
    /// most one queued refresh — never one `count()` per mutation.
    fn request_catalog_size_refresh(&self) {
        self.refresh_signal.notify_one();
    }

    /// Spawn the single long-lived worker that drains refresh requests.
    ///
    /// Parks on [`Notify::notified`] until a mutation signals it, runs one
    /// cancellable refresh, then loops. A `notify_one` permit stored while the
    /// worker was busy is consumed on the next iteration — the trailing run
    /// that reflects the post-burst catalog size. Exits on cancellation.
    fn spawn_refresh_worker(&self) {
        let store = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    () = store.cancel.cancelled() => return,
                    () = store.refresh_signal.notified() => {}
                }
                // A shutdown racing the signal still short-circuits the count.
                if store.refresh_catalog_size_cancellable().await == RefreshOutcome::Cancelled {
                    return;
                }
            }
        });
    }

    /// Race [`Self::refresh_catalog_size`] against the cancellation token.
    ///
    /// On cancel the `count()` future is dropped and `Cancelled` is returned
    /// so the worker exits promptly. Returns `Ran` when the count completes
    /// (success or a logged failure) regardless of whether the query succeeded.
    async fn refresh_catalog_size_cancellable(&self) -> RefreshOutcome {
        tokio::select! {
            biased;
            () = self.cancel.cancelled() => RefreshOutcome::Cancelled,
            () = self.refresh_catalog_size() => RefreshOutcome::Ran,
        }
    }

    /// Count the distinct usage types and report the result to the
    /// `uc_clickhouse_usage_type_catalog_size` gauge.
    ///
    /// `uniqExact(gts_id)` rather than `count()`: `gts_id` *is* the table's
    /// whole sort key, so counting rows would count each unmerged duplicate
    /// copy of a type separately and overstate the gauge. `uniqExact` is a
    /// scan of one column with no merge-on-read, and this runs off the request
    /// path on a small table.
    async fn refresh_catalog_size(&self) {
        #[cfg(test)]
        self.refresh_runs
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);

        let sql = "SELECT uniqExact(gts_id) FROM usage_type_catalog";
        // Bounded with a bare `timeout` rather than `with_deadline`: a stalled
        // gauge refresh is not a request-path backend error, so it stays out of
        // the backend-error counter and is only logged.
        match tokio::time::timeout(
            self.request_timeout,
            self.client.query(sql).fetch_one::<u64>(),
        )
        .await
        {
            Ok(Ok(n)) => {
                self.metrics.set_catalog_size(n);
            }
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "failed to refresh usage_type_catalog size");
            }
            Err(_elapsed) => {
                tracing::warn!(
                    deadline_secs = self.request_timeout.as_secs(),
                    "usage_type_catalog size refresh exceeded the client-side deadline"
                );
            }
        }
    }

    /// INSERT a single `UsageTypeRow` into `usage_type_catalog`.
    async fn insert_type_row(&self, row: &UsageTypeRow) -> Result<(), UsageCollectorPluginError> {
        let pool_start = std::time::Instant::now();
        let mut insert = configure_insert(
            with_deadline(
                &self.metrics,
                self.request_timeout,
                self.client.insert::<UsageTypeRow>("usage_type_catalog"),
            )
            .await?,
            self.request_timeout,
            // Deliberately synchronous, independent of the record store's
            // `async_insert` setting, and therefore a literal rather than a
            // config-threaded field. `usage_type_catalog` is a control-plane
            // table: writes come only from `create_usage_type` and
            // `delete_usage_type`, and the table is unpartitioned and tiny —
            // so there is no
            // concurrent insert stream for the server-side buffer to coalesce
            // and no part count to reduce. Enabling it would add the
            // buffer-flush wait to a request whose latency is directly
            // observed, for nothing.
            false,
            // No insert dedup token: the catalog has no dedup window, and the
            // create-type race it would guard is already resolved by the
            // `ReplacingMergeTree` version on this table (see the module docs).
            None,
        );
        self.metrics
            .record_pool_acquire(pool_start.elapsed().as_secs_f64());
        insert
            .write(row)
            .await
            .map_err(|e| tracked_ch_err(&self.metrics, &e))?;
        insert
            .end()
            .await
            .map_err(|e| tracked_ch_err(&self.metrics, &e))
    }

    /// Whether any row carries this `gts_id`.
    ///
    /// No version resolution: `gts_id` is the whole sort key and no physical
    /// copy is a tombstone, so existence is invariant across versions and
    /// `LIMIT 1` on the first match is both correct and the cheapest form.
    /// (Same reasoning as the record store's insert-time check.)
    async fn type_exists(
        &self,
        gts_id: &UsageTypeGtsId,
    ) -> Result<bool, UsageCollectorPluginError> {
        let sql = "SELECT gts_id FROM usage_type_catalog WHERE gts_id = ? LIMIT 1";
        let found: Option<String> = with_deadline(
            &self.metrics,
            self.request_timeout,
            self.client
                .query(sql)
                .bind(gts_id.as_ref())
                .fetch_optional::<String>(),
        )
        .await?;
        Ok(found.is_some())
    }

    /// Count referencing `usage_records` rows, capped at [`REF_COUNT_CAP`].
    ///
    /// `gts_id` leads the `usage_records` sorting key, so this is a
    /// primary-key range read rather than a scan, and the inner `LIMIT` stops
    /// it early on a heavily referenced type.
    ///
    /// Counts rows of **every** `status`, `inactive` deactivation markers
    /// included: that is what the reference plugin's FK counts, and a type
    /// whose only records are deactivated still has rows that would be
    /// orphaned by removing it.
    async fn count_references(
        &self,
        gts_id: &UsageTypeGtsId,
    ) -> Result<u64, UsageCollectorPluginError> {
        let sql = format!(
            "SELECT count() FROM \
             (SELECT 1 FROM usage_records WHERE gts_id = ? LIMIT {REF_COUNT_CAP})"
        );
        with_deadline(
            &self.metrics,
            self.request_timeout,
            self.client
                .query(&sql)
                .bind(gts_id.as_ref())
                .fetch_one::<u64>(),
        )
        .await
    }

    /// Remove records that referenced `gts_id` after its catalog row went away.
    ///
    /// Called only once the catalog row is gone, so a row seen here landed
    /// inside the probe→delete window and is by definition an orphan. The
    /// re-probe gates the mutation: on the overwhelmingly common path nothing
    /// landed and no write is issued against `usage_records` at all.
    ///
    /// Infallible by design — the type is already deleted, so neither a failed
    /// probe nor a failed sweep can be reported as a failed delete without
    /// misstating the outcome. Both are logged at `error` instead. Backend
    /// errors still reach `uc_clickhouse_backend_errors_total` through
    /// [`with_deadline`].
    async fn sweep_orphaned_records(&self, gts_id: &UsageTypeGtsId) {
        let orphans = self.count_orphans_after_delete(gts_id).await;
        if orphans == 0 {
            return;
        }

        self.metrics.inc_orphaned_reference_detected();
        tracing::warn!(
            gts_id = %gts_id.as_ref(),
            orphan_sample_count = orphans,
            "delete_usage_type: records landed while the type was being deleted; sweeping them"
        );

        if let Err(e) = self.delete_where_gts_id("usage_records", gts_id).await {
            tracing::error!(
                gts_id = %gts_id.as_ref(),
                error = %e,
                "delete_usage_type: the orphan sweep failed; the usage type is \
                 deleted but records referencing it survive"
            );
        }
    }

    /// The post-delete orphan probe, reporting `0` when it could not run.
    ///
    /// A failed probe is indistinguishable from "no orphans" to the caller on
    /// purpose: both leave the sweep unrun, and the type is deleted either way.
    /// The distinction that matters to an operator — that the plugin does not
    /// *know* whether orphans survive — is in the log line.
    async fn count_orphans_after_delete(&self, gts_id: &UsageTypeGtsId) -> u64 {
        match self.count_references(gts_id).await {
            Ok(n) => n,
            Err(e) => {
                tracing::error!(
                    gts_id = %gts_id.as_ref(),
                    error = %e,
                    "delete_usage_type: the post-delete orphan probe failed; \
                     records inserted during the delete window may survive"
                );
                0
            }
        }
    }

    /// Run one `ALTER TABLE … DELETE WHERE gts_id = ?` mutation synchronously.
    ///
    /// `with_setting` rather than the crate's `with_option`: the latter is
    /// `#[deprecated(since = "0.14.3")]` and the workspace lints deprecation
    /// (the same reason [`configure_insert`] gives).
    ///
    /// A mutation is heavier than the `request_timeout` budget assumes — it
    /// rewrites every part matching the predicate. On timeout the client await
    /// is abandoned but the server keeps applying the mutation, so the
    /// operation is left half-applied; that is one of the accepted residuals
    /// on [`CatalogStore::delete`].
    async fn delete_where_gts_id(
        &self,
        table: &str,
        gts_id: &UsageTypeGtsId,
    ) -> Result<(), UsageCollectorPluginError> {
        // `table` is a `'static` caller literal, never caller input; `gts_id`
        // is bound.
        let sql = format!("ALTER TABLE {table} DELETE WHERE gts_id = ?");
        with_deadline(
            &self.metrics,
            self.request_timeout,
            self.client
                .query(&sql)
                .with_setting("mutations_sync", MUTATIONS_SYNC)
                .bind(gts_id.as_ref())
                .execute(),
        )
        .await
    }
}

// ── CatalogStore impl ─────────────────────────────────────────────────────────

#[async_trait]
impl CatalogStore for ChCatalogStore {
    // @cpt-flow:cpt-cf-uc-ch-plugin-seq-create-type
    /// Create a usage type: version-resolved pre-existence read, then `INSERT`.
    ///
    /// The two statements are not one atomic section. Two concurrent creates
    /// for the same `gts_id` can both pass the read and both insert; the
    /// higher `version` (epoch microseconds) wins on both merge and read-time
    /// resolution, so the
    /// catalog converges to the later writer's payload and both callers get
    /// `Ok`. Once the winner's row is visible, a later create sees it and
    /// returns either a silent absorb (same payload) or
    /// [`UsageCollectorPluginError::UsageTypeAlreadyExists`].
    async fn create(&self, usage_type: UsageType) -> Result<UsageType, UsageCollectorPluginError> {
        let gts_id_raw = usage_type.gts_id.as_ref().to_owned();

        // 1. Pre-existence check: version-resolved read WHERE gts_id = ?.
        // `gts_id` is the entire sort key, so the `WHERE` pins one logical row
        // and a bare `LIMIT 1` over `version DESC` is the resolved copy.
        let sql =
            format!("SELECT {TYPE_COLUMNS} FROM usage_type_catalog WHERE gts_id = ?{LATEST_ONE}");
        let existing: Option<UsageTypeRow> = {
            with_deadline(
                &self.metrics,
                self.request_timeout,
                self.client
                    .query(&sql)
                    .bind(gts_id_raw.as_str())
                    .fetch_optional::<UsageTypeRow>(),
            )
            .await?
        };

        if let Some(row) = existing {
            // 2-3. Compare kind and metadata_fields for idempotency absorb vs conflict.
            let same_kind = row.kind == UsageTypeKindCode::from(usage_type.kind);
            // BTreeSet<MetadataKey> is already sorted; compare against sorted stored Vec.
            let mut stored_sorted = row.metadata_fields.clone();
            stored_sorted.sort_unstable();
            let incoming_sorted: Vec<String> = usage_type
                .metadata_fields
                .iter()
                .map(|k| k.as_str().to_owned())
                .collect();
            return if same_kind && stored_sorted == incoming_sorted {
                // Same payload already stored — silent absorb (SPI idempotency rule).
                UsageType::try_from(row)
            } else {
                Err(UsageCollectorPluginError::UsageTypeAlreadyExists {
                    gts_id: usage_type.gts_id,
                })
            };
        }

        // 4. Absent → INSERT.
        //
        // Version scheme: epoch microseconds from `current_merge_version()` (the
        // same helper Record Store uses for usage_records). Nothing serialises
        // creates for one `gts_id`, so the version is what orders two racing
        // inserts: read-time resolution keeps the higher one. A re-create after
        // a `delete` has no earlier row to outrank either: `delete` removes the
        // physical rows with `ALTER TABLE … DELETE` rather than leaving a
        // tombstone, so this INSERT is the only copy (pinned by
        // `live::a_deleted_type_can_be_recreated`).
        let row = UsageTypeRow {
            gts_id: gts_id_raw,
            kind: usage_type.kind.into(),
            metadata_fields: usage_type
                .metadata_fields
                .iter()
                .map(|k| k.as_str().to_owned())
                .collect(),
            version: current_merge_version(),
        };
        self.insert_type_row(&row).await?;

        // 5. Signal catalog-size refresh off the request path.
        self.request_catalog_size_refresh();
        Ok(usage_type)
    }

    async fn get(&self, gts_id: UsageTypeGtsId) -> Result<UsageType, UsageCollectorPluginError> {
        let sql =
            format!("SELECT {TYPE_COLUMNS} FROM usage_type_catalog WHERE gts_id = ?{LATEST_ONE}");
        let row: Option<UsageTypeRow> = with_deadline(
            &self.metrics,
            self.request_timeout,
            self.client
                .query(&sql)
                .bind(gts_id.as_ref())
                .fetch_optional::<UsageTypeRow>(),
        )
        .await?;

        match row {
            Some(row) => UsageType::try_from(row),
            None => Err(UsageCollectorPluginError::UsageTypeNotFound { gts_id }),
        }
    }

    /// Keyset-paginated `usage_type_catalog` list, fixed-ordered by `gts_id`
    /// ascending. `query.order` is ignored (the catalog has one stable order).
    ///
    /// Version resolution happens in an inner subquery and every caller
    /// predicate is applied *outside* it. `kind` is both filterable and
    /// keyset-safe, and two racing creates for one `gts_id` can store
    /// different `kind`s under different `version`s, so a `kind` predicate
    /// evaluated before resolution could retain a superseded row and hide the
    /// winning one. Filtering after resolution avoids having to prove which
    /// half of an opaque translated fragment is version-invariant, and keeps
    /// the bind order identical to the emitted `?` order. The inner scan is a
    /// whole-table dedup, which is affordable here precisely because the
    /// catalog is small — the `uc_clickhouse_usage_type_catalog_size` gauge
    /// tracks that assumption.
    ///
    /// The extra look-ahead row detects a following page without a separate
    /// `count(*)`.
    async fn list(
        &self,
        query: &ODataQuery,
    ) -> Result<ODataPage<UsageType>, UsageCollectorPluginError> {
        let limit = effective_page_size(query.limit, DEFAULT_PAGE_SIZE);

        // No leading scope bind — catalog is not tenant/gts-scoped.
        let mut ctx = SqlCtx::new();
        let mut clauses: Vec<String> = Vec::new();

        // Optional `$filter` (currently ignored by the SPI gateway for catalog,
        // but the allowlist + translate layer is wired for completeness and
        // future use; the allowed fields are `gts_id` and `kind`).
        if let Some(expr) = query.filter() {
            let node = convert_expr_to_filter_node::<UsageTypeFilterField>(expr)
                .map_err(|e| UsageCollectorPluginError::internal(format!("invalid filter: {e}")))?;
            let fragment = translate_usage_type_filter(&node, &mut ctx)
                .map_err(UsageCollectorPluginError::internal)?;
            clauses.push(fragment);
        }

        // Keyset continuation (forward only).
        if let Some(cursor) = query.cursor.as_ref() {
            ensure_forward_cursor(cursor).map_err(UsageCollectorPluginError::internal)?;
            if cursor.f.as_deref() != query.filter_hash.as_deref() {
                return Err(UsageCollectorPluginError::internal(
                    "cursor filter hash mismatch",
                ));
            }
            // The catalog list has one fixed order, so the cursor is checked
            // against that order rather than the ignored `query.order` — a
            // token minted under any other order cannot be walked forward here.
            if !gts_id_asc_order().equals_signed_tokens(&cursor.s) {
                return Err(UsageCollectorPluginError::internal(
                    "cursor sort order mismatch",
                ));
            }
            let predicate = keyset_predicate(
                &[("gts_id", true)], // fixed ASC order
                &cursor.k,
                usage_type_column,
                |name| UsageTypeFilterField::from_name(name).map(|f| f.kind()),
                is_keyset_safe_type_field,
                &mut ctx,
            )
            .map_err(UsageCollectorPluginError::internal)?;
            clauses.push(predicate);
        }

        // No filter and no keyset cursor leaves `clauses` empty on the first
        // page of an unfiltered list — omit `WHERE` entirely rather than
        // emitting `WHERE ` with nothing to its right.
        let where_clause = if clauses.is_empty() {
            String::new()
        } else {
            format!("WHERE {} ", clauses.join(" AND "))
        };
        // The inner query carries no caller predicate, so every `?` in the
        // emitted text still comes from `where_clause` in `ctx` push order.
        let resolve_latest = latest_by(&["gts_id"]);
        let sql = format!(
            "SELECT {TYPE_COLUMNS} FROM \
             (SELECT {TYPE_COLUMNS} FROM usage_type_catalog{resolve_latest}) \
             {where_clause}ORDER BY gts_id ASC LIMIT {}",
            limit.saturating_add(1),
        );

        let mut q = self.client.query(&sql);
        for b in &ctx.binds {
            q = bind_one(q, b);
        }
        let mut rows: Vec<UsageTypeRow> =
            with_deadline(&self.metrics, self.request_timeout, q.fetch_all()).await?;

        // Look-ahead row present → a next page exists; drop it before mapping.
        let has_next = rows.len() > usize::try_from(limit).unwrap_or(usize::MAX);
        if has_next {
            rows.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
        }

        let next_cursor = if has_next {
            let last = rows.last().ok_or_else(|| {
                UsageCollectorPluginError::internal("non-empty page lost its tail")
            })?;
            let order = gts_id_asc_order();
            let token = encode_next_cursor(
                &order,
                std::slice::from_ref(&last.gts_id),
                query.filter_hash.as_deref(),
            )
            .map_err(UsageCollectorPluginError::internal)?;
            Some(token)
        } else {
            None
        };

        let items = rows
            .into_iter()
            .map(UsageType::try_from)
            .collect::<Result<Vec<_>, _>>()?;

        Ok(ODataPage::new(
            items,
            PageInfo {
                next_cursor,
                prev_cursor: None,
                limit,
            },
        ))
    }

    // @cpt-flow:cpt-cf-uc-ch-plugin-seq-delete-type
    /// Delete a usage type: probe for references, remove the catalog row, then
    /// sweep any record that landed in between.
    ///
    /// `ClickHouse` has no foreign key to enforce `ON DELETE RESTRICT` and
    /// this plugin has no mutual-exclusion primitive, so the FK is emulated in
    /// four steps:
    ///
    /// 1. Existence read — absent → [`UsageCollectorPluginError::UsageTypeNotFound`],
    ///    so the gateway can tell "already gone" from "deleted now".
    /// 2. Capped reference probe — any referencing row →
    ///    [`UsageCollectorPluginError::UsageTypeReferenced`] and the catalog
    ///    row is left untouched.
    /// 3. `ALTER TABLE usage_type_catalog DELETE` under `mutations_sync`.
    ///    Ordered **before** the sweep deliberately: from this point the
    ///    record store's insert-time existence check refuses new records for
    ///    the `gts_id` on its own, which is what bounds the race window to the
    ///    span between steps 2 and 3.
    /// 4. Re-probe, and only if it is still non-zero, `ALTER TABLE
    ///    usage_records DELETE` for the rows that landed inside that window.
    ///    The gate keeps the common case (nothing landed) from issuing a
    ///    mutation against the large table at all.
    ///
    /// # Accepted residuals
    ///
    /// This does **not** satisfy `plugin-spi.md` Method 9's "MUST NOT admit a
    /// window" clause — that requires a serializable read-before-delete this
    /// backend cannot express. The known gaps:
    ///
    /// - An insert whose own catalog check passed before step 3 can commit
    ///   after step 4 and orphan a row.
    /// - With `async_insert` on (the plugin default) a record can sit in a
    ///   server-side buffer past the sweep, widening that window from
    ///   microseconds to the flush interval.
    /// - Two concurrent deletes for one `gts_id` both pass step 1 and both
    ///   return `Ok(())`; neither sees `UsageTypeNotFound`.
    /// - `mutations_sync = 1` waits only for the receiving server, so on a
    ///   replicated deployment another process can briefly still see the type
    ///   and accept a record for it.
    /// - A step-3 or step-4 timeout abandons the client await while the server
    ///   keeps applying the mutation, leaving the operation half-applied.
    ///
    /// A step-4 failure is logged at `error` and **not** propagated: the type
    /// itself is deleted, so reporting failure would misstate the outcome and
    /// a retry could only ever answer `UsageTypeNotFound`.
    async fn delete(&self, gts_id: UsageTypeGtsId) -> Result<(), UsageCollectorPluginError> {
        // 1. Existence read — absence is an error, not a silent success.
        if !self.type_exists(&gts_id).await? {
            return Err(UsageCollectorPluginError::UsageTypeNotFound { gts_id });
        }

        // 2. Reference probe. Non-zero refuses the delete and touches nothing.
        let refs = self.count_references(&gts_id).await?;
        if refs > 0 {
            self.metrics.inc_usage_type_referenced();
            tracing::info!(
                gts_id = %gts_id.as_ref(),
                sample_ref_count = refs,
                "delete_usage_type refused: the type is still referenced"
            );
            return Err(UsageCollectorPluginError::UsageTypeReferenced {
                gts_id,
                // The probe ran under `refs > 0`, so the SPI's "sample count
                // >= 1" holds without a clamp.
                sample_ref_count: refs,
            });
        }

        // 3. Remove the catalog row. From here new records for this `gts_id`
        // are refused by the record store's own existence check.
        self.delete_where_gts_id("usage_type_catalog", &gts_id)
            .await?;

        // 4. Sweep whatever landed between steps 2 and 3.
        self.sweep_orphaned_records(&gts_id).await;

        // 5. The catalog shrank; refresh the gauge off the request path.
        self.request_catalog_size_refresh();
        Ok(())
    }
}

// ── Test module ───────────────────────────────────────────────────────────────

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "catalog_store_tests.rs"]
mod catalog_store_tests;
