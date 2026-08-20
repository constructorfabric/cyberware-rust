//! `ClickHouse`-backed [`CatalogStore`] over the `usage_type_catalog` table.
//!
//! Implements `create` / `get` / `list` / `delete` against `ClickHouse` using
//! the `clickhouse` 0.15.x crate. All reads include `FINAL` so a row's
//! `ReplacingMergeTree(version)`-resolved state (the highest-version physical
//! copy) is what every read observes. Deletion is a real row removal via a
//! lightweight `DELETE FROM usage_type_catalog WHERE gts_id = ?` — never
//! `ALTER TABLE … DELETE`, which is an asynchronous background mutation
//! unsuitable for the request path. The statement sets
//! `lightweight_deletes_sync = 2` itself rather than inheriting the server
//! default, which is what makes a deleted row absent from every subsequent
//! query, with no tombstone flag or versioned marker required to represent
//! "deleted".
//!
//! `delete` acquires an exclusive per-`gts_id` coordination lock via
//! [`CatalogLockPort`], runs an authoritative bounded reference-count probe
//! under the lock, renews the lock lease immediately before the `DELETE`
//! (`ensure_still_held`), and issues the `DELETE` only when no references
//! exist. No rollback step exists; the exclusive lock closes the
//! referential-integrity race entirely (DESIGN.md §3.6).
//!
//! The referential-integrity guarantee is unconditional: no
//! `create_usage_record`/`create_usage_records` call can insert a row for
//! this `gts_id` while the exclusive lock is held, so the reference-count
//! probe observes every committed reference with no residual window
//! (DESIGN.md §3.6 step 6 "No residual race"). `ensure_still_held` is a
//! lock-lease renewal guard against TTL expiry during the critical section
//! — an operational safeguard, not a referential-integrity closure
//! mechanism; if it fails the `DELETE` is aborted with `Transient` so the
//! write is never issued past a detected lease expiry.
//!
//! A single background refresh worker tracks the live catalog count via the
//! `uc_clickhouse_usage_type_catalog_size` gauge.

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
use crate::infra::coordination::lock_manager::LockGuardPort;
use crate::infra::metrics::{LockMode, Metrics};
use crate::infra::storage::entity::{UsageTypeKindCode, UsageTypeRow};
use crate::infra::storage::error::{tracked_ch_err, with_deadline};
use crate::infra::storage::mapper::current_merge_version;
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

// ── Constants ─────────────────────────────────────────────────────────────────

/// Upper bound on the reference probe inside `delete`.
///
/// The SPI declares `sample_ref_count` a bounded, plugin-tunable value;
/// `REF_COUNT_CAP` caps the sub-query `LIMIT` so the probe never full-scans
/// `usage_records`.
const REF_COUNT_CAP: i64 = 1000;

// ── Helpers ───────────────────────────────────────────────────────────────────

/// The catalog's fixed sort order (`gts_id` ascending), used to encode the
/// next-page cursor. The catalog list ignores `query.order` by design.
fn gts_id_asc_order() -> ODataOrderBy {
    ODataOrderBy(vec![OrderKey {
        field: "gts_id".to_owned(),
        dir: SortDir::Asc,
    }])
}

// Re-export so catalog unit tests can keep `use super::CatalogLockPort`.
pub use crate::infra::coordination::lock_manager::CatalogLockPort;

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
    lock_manager: Arc<dyn CatalogLockPort>,
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
    /// Build a store from an existing `ClickHouse` client, lock port, and
    /// metric inventory, then spawn the single background catalog-size refresh
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
        lock_manager: Arc<dyn CatalogLockPort>,
        cancel: CancellationToken,
        metrics: Arc<Metrics>,
        request_timeout: Duration,
    ) -> Self {
        let store = Self {
            client,
            lock_manager,
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
    /// `notify_one` coalesces a burst of concurrent `create` / `delete` calls
    /// into at most one queued refresh — never one `count()` per mutation.
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

    /// Run a `SELECT count() FROM usage_type_catalog FINAL` and report the
    /// result to the `uc_clickhouse_usage_type_catalog_size` gauge.
    async fn refresh_catalog_size(&self) {
        #[cfg(test)]
        self.refresh_runs
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);

        let sql = "SELECT count() FROM usage_type_catalog FINAL";
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
        // `with_timeouts` bounds the `write` / `end` awaits below natively.
        let mut insert = with_deadline(
            &self.metrics,
            self.request_timeout,
            self.client.insert::<UsageTypeRow>("usage_type_catalog"),
        )
        .await?
        .with_timeouts(Some(self.request_timeout), Some(self.request_timeout));
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
}

// ── CatalogStore impl ─────────────────────────────────────────────────────────

#[async_trait]
impl CatalogStore for ChCatalogStore {
    // @cpt-flow:cpt-cf-uc-ch-plugin-seq-create-type
    /// Create a usage type under the exclusive per-`gts_id` coordination lock.
    ///
    /// The lock is the same exclusive mutex `delete` and record ingest take,
    /// so the pre-existence check and the `INSERT` are one atomic critical
    /// section: two concurrent creates for the same `gts_id` serialize and the
    /// loser observes the winner's row, returning either a silent absorb (same
    /// payload) or [`UsageCollectorPluginError::UsageTypeAlreadyExists`].
    async fn create(&self, usage_type: UsageType) -> Result<UsageType, UsageCollectorPluginError> {
        let exclusive_guard = self
            .lock_manager
            .acquire_exclusive_for_create(usage_type.gts_id.as_ref())
            .await?;

        let result = self
            .create_under_lock(usage_type, exclusive_guard.as_ref())
            .await;

        if let Err(e) = exclusive_guard.release().await {
            tracing::warn!(error = %e, "failed to release catalog-create cluster lock");
            if result.is_ok() {
                return Err(e);
            }
        }

        result
    }

    async fn get(&self, gts_id: UsageTypeGtsId) -> Result<UsageType, UsageCollectorPluginError> {
        let sql = format!("SELECT {TYPE_COLUMNS} FROM usage_type_catalog FINAL WHERE gts_id = ?");
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
    /// All reads are `FINAL`-qualified. The extra look-ahead row detects a
    /// following page without a separate `count(*)`.
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
        let sql = format!(
            "SELECT {TYPE_COLUMNS} FROM usage_type_catalog FINAL \
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
    // @cpt-constraint:cpt-cf-uc-ch-plugin-constraint-gts-lock-required
    /// Delete a usage type by a real row removal under an exclusive lock.
    ///
    /// Follows DESIGN.md §3.6 "Delete Usage Type — Lock-Protected Verify".
    /// The exclusive lock makes the reference-count probe authoritative.
    ///
    /// **Lease renew**: `ensure_still_held` (cluster `renew`) runs after the
    /// reference probe and before the `DELETE`. Cluster lock drop is a no-op —
    /// the lock is always released explicitly after the critical section.
    async fn delete(&self, gts_id: UsageTypeGtsId) -> Result<(), UsageCollectorPluginError> {
        let exclusive_guard = self
            .lock_manager
            .acquire_exclusive_for_delete(gts_id.as_ref())
            .await?;

        let result = self
            .delete_under_lock(&gts_id, exclusive_guard.as_ref())
            .await;

        if let Err(e) = exclusive_guard.release().await {
            tracing::warn!(error = %e, "failed to release catalog-delete cluster lock");
            if result.is_ok() {
                return Err(e);
            }
        }

        result
    }
}

impl ChCatalogStore {
    /// Critical section of create while holding `exclusive_guard`.
    async fn create_under_lock(
        &self,
        usage_type: UsageType,
        exclusive_guard: &dyn LockGuardPort,
    ) -> Result<UsageType, UsageCollectorPluginError> {
        let gts_id_raw = usage_type.gts_id.as_ref().to_owned();

        // 1. Pre-existence check: SELECT FINAL WHERE gts_id = ?.
        let sql = format!("SELECT {TYPE_COLUMNS} FROM usage_type_catalog FINAL WHERE gts_id = ?");
        let existing: Option<UsageTypeRow> = with_deadline(
            &self.metrics,
            self.request_timeout,
            self.client
                .query(&sql)
                .bind(gts_id_raw.as_str())
                .fetch_optional::<UsageTypeRow>(),
        )
        .await?;

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

        if let Err(e) = exclusive_guard.ensure_still_held().await {
            self.metrics.inc_lock_manager_unavailable(LockMode::Create);
            return Err(e);
        }

        // 4. Absent → INSERT.
        //
        // Version scheme: epoch microseconds from `current_merge_version()` (the
        // same helper Record Store uses for usage_records). The exclusive lock
        // serialises creates for one `gts_id`, so the version only has to order
        // an insert against a *previous* row; `ReplacingMergeTree(version)`
        // FINAL resolution keeps the higher one. Deletion is a real row removal
        // (lightweight `DELETE FROM`, see [`Self::delete`]), so a re-create
        // after a delete never has to outrank a leftover tombstone row.
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

    /// Critical section of delete while holding `exclusive_guard`.
    async fn delete_under_lock(
        &self,
        gts_id: &UsageTypeGtsId,
        exclusive_guard: &dyn LockGuardPort,
    ) -> Result<(), UsageCollectorPluginError> {
        let sql = format!("SELECT {TYPE_COLUMNS} FROM usage_type_catalog FINAL WHERE gts_id = ?");
        let existing: Option<UsageTypeRow> = with_deadline(
            &self.metrics,
            self.request_timeout,
            self.client
                .query(&sql)
                .bind(gts_id.as_ref())
                .fetch_optional::<UsageTypeRow>(),
        )
        .await?;

        if existing.is_none() {
            return Err(UsageCollectorPluginError::UsageTypeNotFound {
                gts_id: gts_id.clone(),
            });
        }

        let ref_sql = "SELECT count() FROM (SELECT 1 FROM usage_records FINAL WHERE gts_id = ? LIMIT ?) \
             AS sub_ref";
        let ref_count: u64 = with_deadline(
            &self.metrics,
            self.request_timeout,
            self.client
                .query(ref_sql)
                .bind(gts_id.as_ref())
                .bind(REF_COUNT_CAP)
                .fetch_one::<u64>(),
        )
        .await?;

        if ref_count > 0 {
            self.metrics.inc_usage_type_referenced();
            return Err(UsageCollectorPluginError::UsageTypeReferenced {
                gts_id: gts_id.clone(),
                sample_ref_count: ref_count.max(1),
            });
        }

        if let Err(e) = exclusive_guard.ensure_still_held().await {
            self.metrics.inc_lock_manager_unavailable(LockMode::Delete);
            return Err(e);
        }

        // `lightweight_deletes_sync = 2` is stated explicitly rather than
        // inherited: the removal must be visible to the very next read, because
        // a re-`create` for this `gts_id` decides absorb-vs-`UsageTypeAlreadyExists`
        // from a `FINAL` pre-existence check (see `create_under_lock`), and
        // `create_usage_record` decides `UsageTypeNotFound` the same way. The
        // server-side default is not ours to rely on — ClickHouse Cloud ships
        // `1` and a settings profile can ship `0`, which returns from `DELETE`
        // before the row stops being visible. A query-level setting overrides
        // both, at the cost of waiting for the mutation on every replica.
        let delete_sql = "DELETE FROM usage_type_catalog WHERE gts_id = ?";
        with_deadline(
            &self.metrics,
            self.request_timeout,
            self.client
                .query(delete_sql)
                .with_setting("lightweight_deletes_sync", "2")
                .bind(gts_id.as_ref())
                .execute(),
        )
        .await?;

        self.request_catalog_size_refresh();
        Ok(())
    }
}

// ── Test module ───────────────────────────────────────────────────────────────

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "catalog_store_tests.rs"]
mod catalog_store_tests;
