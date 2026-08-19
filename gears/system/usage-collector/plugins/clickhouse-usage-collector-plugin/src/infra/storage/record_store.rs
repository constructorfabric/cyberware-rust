//! `ClickHouse`-backed [`RecordStore`] over the `usage_records` table.
//!
//! All operations — `create` / `create_batch` / `get` / `list` / `aggregate` /
//! `deactivate` — are implemented against `ClickHouse` using the `clickhouse`
//! 0.15.x crate.
//!
//! ## Key design differences from the `TimescaleDB` reference plugin
//!
//! - **No `ON CONFLICT DO NOTHING`**: `ClickHouse` has no unique constraints.
//!   Dedup is performed explicitly: SELECT then INSERT.
//! - **No `FOR UPDATE`**: `ClickHouse` has no row-level locks. Coordination is
//!   provided by the cluster-backed
//!   [`LockManager`](crate::infra::coordination::lock_manager::LockManager)
//!   instead, reached through the [`CatalogLockPort`] seam.
//! - **No `UPDATE`**: deactivation uses versioned marker rows (INSERT with
//!   `status = 'inactive'` and a higher `version`) rather than `ALTER TABLE …
//!   UPDATE` (an async mutation unsuitable for the request path).
//! - **`FINAL` keyword**: every SELECT appends `FINAL` to the table name so
//!   `ReplacingMergeTree` version resolution is applied before the result is
//!   returned — un-qualified reads may return stale pre-deactivation or
//!   duplicate rows.
//! - **`?` placeholders**: `ClickHouse` uses positional `?` (not `$N`).
//! - **`metadata['key']`**: map subscript (not `metadata ->> key`).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use bigdecimal::BigDecimal;
use futures::future::join_all;
use tracing::instrument;
use uuid::Uuid;

use toolkit_odata::filter::{FilterField, convert_expr_to_filter_node};
use toolkit_odata::{ODataQuery, Page as ODataPage, PageInfo, SortDir};

use usage_collector_sdk::{
    AggregationBucket, AggregationDimension, AggregationResult, AggregationSpec, MetadataFilter,
    UsageCollectorPluginError, UsageRecord, UsageRecordFilterField, UsageTypeGtsId,
    is_keyset_safe_record_field,
};

use crate::domain::ports::RecordStore;
use crate::infra::coordination::lock_manager::LockGuardPort;
use crate::infra::metrics::{InsertMode, LockMode, Metrics, OpDurationGuard, QueryKind, TimedOp};
use crate::infra::storage::catalog_store::CatalogLockPort;
use crate::infra::storage::entity::{EpochMicros, UsageRecordRow, UsageRecordStatusCode};
use crate::infra::storage::error::tracked_ch_err;
use crate::infra::storage::mapper::{
    canonical_equal, current_merge_version, make_inactive_marker, record_row_key,
    version_higher_than,
};
use crate::infra::storage::query::aggregate::{
    agg_select_expr, aggregate_limit_clause, corrects_id_partition_clause, dimension_select_expr,
};
use crate::infra::storage::query::effective_page_size;
use crate::infra::storage::query::keyset::{
    encode_next_cursor, ensure_forward_cursor, keyset_predicate, render_order_by,
};
use crate::infra::storage::query::translate::{SqlBind, SqlCtx, bind_one, record_column};

/// Static column list for every `usage_records` SELECT, in [`UsageRecordRow`]
/// field order. A `'static` constant (never caller input), so decoding is
/// positional without SQL injection risk.
const RECORD_COLUMNS: &str = "id, tenant_id, gts_id, value, created_at, resource_id, \
     resource_type, subject_id, subject_type, idempotency_key, corrects_id, status, metadata, \
     ingested_at, version";

/// `ClickHouse`-backed implementation of [`RecordStore`] over `usage_records`.
#[derive(Clone)]
pub struct ChRecordStore {
    client: clickhouse::Client,
    lock_manager: Arc<dyn CatalogLockPort>,
    metrics: Arc<Metrics>,
}

impl ChRecordStore {
    /// Build a store from an existing `ClickHouse` client, exclusive-lock port,
    /// and metric inventory.
    ///
    /// The lock port is the erased [`CatalogLockPort`] the catalog store also
    /// depends on — both stores contend on the same exclusive per-`gts_id`
    /// cluster mutex — so create-path lock failures are exercisable offline
    /// with a stub implementation.
    #[must_use]
    pub fn new(
        client: clickhouse::Client,
        lock_manager: Arc<dyn CatalogLockPort>,
        metrics: Arc<Metrics>,
    ) -> Self {
        Self {
            client,
            lock_manager,
            metrics,
        }
    }

    /// Execute a `SELECT … FROM usage_type_catalog FINAL WHERE gts_id = ?`
    /// catalog existence check while the caller holds the exclusive create lock.
    ///
    /// Returns `Ok(())` if the usage type exists; `UsageTypeNotFound`
    /// otherwise. A deleted usage type is a real row removal (lightweight
    /// `DELETE FROM`, see `catalog_store::ChCatalogStore::delete`), so its
    /// absence from this query is immediate and unconditional — no tombstone
    /// flag is consulted here.
    ///
    /// # Errors
    ///
    /// Returns `Transient` on connectivity errors, `Internal` on protocol
    /// errors, `UsageTypeNotFound` when absent.
    ///
    /// (`ClickHouse` errors are mapped via [`tracked_ch_err`].)
    #[instrument(skip_all, fields(gts_id = %gts_id.as_ref()))]
    async fn check_catalog_existence(
        &self,
        gts_id: &UsageTypeGtsId,
    ) -> Result<(), UsageCollectorPluginError> {
        let sql = "SELECT gts_id FROM usage_type_catalog FINAL WHERE gts_id = ?";
        let found: Option<String> = self
            .client
            .query(sql)
            .bind(gts_id.as_ref())
            .fetch_optional::<String>()
            .await
            .map_err(|e| tracked_ch_err(&self.metrics, &e))?;
        if found.is_none() {
            return Err(UsageCollectorPluginError::UsageTypeNotFound {
                gts_id: gts_id.clone(),
            });
        }
        Ok(())
    }

    /// Dedup point-lookup using the full `ORDER BY` key prefix:
    /// `WHERE tenant_id = ? AND gts_id = ? AND created_at = ? AND id = ?`
    ///
    /// Returns the stored row if found, `None` if no row exists for this key.
    ///
    /// # Errors
    ///
    /// Returns `Transient` or `Internal` on `ClickHouse` errors.
    #[instrument(skip_all, fields(gts_id = %record.gts_id.as_ref()))]
    async fn dedup_point_lookup(
        &self,
        record: &UsageRecord,
    ) -> Result<Option<UsageRecordRow>, UsageCollectorPluginError> {
        let sql = format!(
            "SELECT {RECORD_COLUMNS} FROM usage_records FINAL \
             WHERE tenant_id = ? AND gts_id = ? \
             AND created_at = fromUnixTimestamp64Micro(?) AND id = ?"
        );
        let created_at_micros = EpochMicros::from(record.created_at).0;
        self.client
            .query(&sql)
            .bind(record.tenant_id.to_string())
            .bind(record.gts_id.as_ref())
            .bind(created_at_micros)
            .bind(record.id.to_string())
            .fetch_optional::<UsageRecordRow>()
            .await
            .map_err(|e| tracked_ch_err(&self.metrics, &e))
    }

    /// Insert a single row into `usage_records`.
    ///
    /// Tracks pool-acquire duration (time to get the `Insert` handle) via the
    /// metric inventory, and reports the single-row create latency measured
    /// from `op_start` — the caller's critical-section entry, so lock
    /// acquisition and the catalog check are inside the observed window.
    ///
    /// # Errors
    ///
    /// Returns `Transient` or `Internal` on `ClickHouse` errors.
    async fn insert_record(
        &self,
        row: &UsageRecordRow,
        op_start: Instant,
    ) -> Result<(), UsageCollectorPluginError> {
        let pool_start = Instant::now();
        let mut insert: clickhouse::insert::Insert<UsageRecordRow> = self
            .client
            .insert("usage_records")
            .await
            .map_err(|e| tracked_ch_err(&self.metrics, &e))?;
        self.metrics
            .record_pool_acquire(pool_start.elapsed().as_secs_f64());

        insert
            .write(row)
            .await
            .map_err(|e| tracked_ch_err(&self.metrics, &e))?;
        insert
            .end()
            .await
            .map_err(|e| tracked_ch_err(&self.metrics, &e))?;
        self.metrics
            .record_insert(InsertMode::Single, op_start.elapsed().as_secs_f64());
        Ok(())
    }

    /// Insert multiple rows into `usage_records` in a single INSERT statement.
    ///
    /// A single `ClickHouse` INSERT is applied as one atomic part write, so a
    /// `FINAL`-qualified reader either sees all rows or none.
    ///
    /// Tracks pool-acquire duration via the metric inventory and reports the
    /// batch-write latency measured from `op_start`, which the caller sets to
    /// the point its observed window should begin.
    ///
    /// # Errors
    ///
    /// Returns `Transient` or `Internal` on `ClickHouse` errors.
    async fn insert_records(
        &self,
        rows: &[UsageRecordRow],
        op_start: Instant,
    ) -> Result<(), UsageCollectorPluginError> {
        if rows.is_empty() {
            return Ok(());
        }
        let pool_start = Instant::now();
        let mut insert: clickhouse::insert::Insert<UsageRecordRow> = self
            .client
            .insert("usage_records")
            .await
            .map_err(|e| tracked_ch_err(&self.metrics, &e))?;
        self.metrics
            .record_pool_acquire(pool_start.elapsed().as_secs_f64());
        // Row counts up to the batch cap (≤1000) fit exactly in f64's 52-bit mantissa.
        #[allow(
            clippy::cast_precision_loss,
            reason = "batch size is bounded by REF_COUNT_CAP"
        )]
        let batch_len = rows.len() as f64;
        self.metrics.record_batch_rows(batch_len);

        for row in rows {
            insert
                .write(row)
                .await
                .map_err(|e| tracked_ch_err(&self.metrics, &e))?;
        }
        insert
            .end()
            .await
            .map_err(|e| tracked_ch_err(&self.metrics, &e))?;
        self.metrics
            .record_insert(InsertMode::Batch, op_start.elapsed().as_secs_f64());
        Ok(())
    }

    /// Batch dedup pre-check: SELECT all rows whose `(tenant_id, gts_id,
    /// created_at, id)` 4-tuple appears in the input list.
    ///
    /// Returns a map from the 4-tuple key to the stored row.
    ///
    /// # Errors
    ///
    /// Returns `Transient` or `Internal` on `ClickHouse` errors.
    #[instrument(skip_all, fields(record_count = records.len()))]
    async fn batch_dedup_lookup(
        &self,
        records: &[&UsageRecord],
    ) -> Result<HashMap<DedupKey, UsageRecordRow>, UsageCollectorPluginError> {
        if records.is_empty() {
            return Ok(HashMap::new());
        }
        // Build `(t, g, c, i) IN ((?, ?, fromUnixTimestamp64Micro(?), ?), ...)`.
        // A bare epoch-microsecond integer in a tuple comparison is coerced
        // through Decimal arithmetic by ClickHouse and can raise
        // DECIMAL_OVERFLOW before the query starts.
        let mut ctx = SqlCtx::new();
        let mut tuples = Vec::with_capacity(records.len());
        for r in records {
            ctx.push(SqlBind::Uuid(r.tenant_id));
            ctx.push(SqlBind::Str(r.gts_id.as_ref().to_owned()));
            ctx.push(SqlBind::DateTime64Micros(EpochMicros::from(r.created_at).0));
            ctx.push(SqlBind::Uuid(r.id));
            tuples.push("(?, ?, fromUnixTimestamp64Micro(?), ?)");
        }
        let in_clause = tuples.join(", ");
        let sql = format!(
            "SELECT {RECORD_COLUMNS} FROM usage_records FINAL \
             WHERE (tenant_id, gts_id, created_at, id) IN ({in_clause})"
        );
        let mut q = self.client.query(&sql);
        for b in &ctx.binds {
            q = bind_one(q, b);
        }
        let rows: Vec<UsageRecordRow> = q
            .fetch_all()
            .await
            .map_err(|e| tracked_ch_err(&self.metrics, &e))?;
        Ok(rows.into_iter().map(|r| (row_dedup_key(&r), r)).collect())
    }

    /// Append metadata side-channel filters as parameterised `WHERE` clauses.
    ///
    /// `metadata['?'] IN (?, ?)` — both key and each value are bound via `ctx`.
    fn push_metadata_filters(
        metadata_filter: &[MetadataFilter],
        ctx: &mut SqlCtx,
        clauses: &mut Vec<String>,
    ) {
        for mf in metadata_filter {
            if mf.values().is_empty() {
                clauses.push("FALSE".to_owned());
                continue;
            }
            ctx.push(SqlBind::Str(mf.key().as_str().to_owned()));
            let placeholders = mf
                .values()
                .iter()
                .map(|v| {
                    ctx.push(SqlBind::Str(v.clone()));
                    "?"
                })
                .collect::<Vec<_>>();
            clauses.push(format!("metadata[?] IN ({})", placeholders.join(", ")));
        }
    }
}

/// The 4-tuple dedup identity for `usage_records`: `(tenant_id, gts_id,
/// created_at_micros, id)`.
///
/// `created_at` is stored as `i64` epoch-microseconds, so the dedup key is
/// already µs-normalised; no truncation is needed.
type DedupKey = (Uuid, String, i64, Uuid);

fn record_dedup_key(r: &UsageRecord) -> DedupKey {
    (
        r.tenant_id,
        r.gts_id.as_ref().to_owned(),
        EpochMicros::from(r.created_at).0,
        r.id,
    )
}

fn row_dedup_key(r: &UsageRecordRow) -> DedupKey {
    (r.tenant_id, r.gts_id.clone(), r.created_at, r.id)
}

/// Build an `Internal` error noting a dedup invariant break and log it at
/// `error` level so it is observable without exposing identifiers to callers.
fn dedup_invariant_break(record: &UsageRecord, msg: &'static str) -> UsageCollectorPluginError {
    tracing::error!(
        tenant_id = %record.tenant_id,
        gts_id = %record.gts_id.as_ref(),
        idempotency_key = %record.idempotency_key.as_str(),
        "{msg}"
    );
    UsageCollectorPluginError::internal(msg)
}

/// Build a fresh, independently-owned copy of a partition-level failure for
/// embedding into every record's outcome slot in that `gts_id` partition.
///
/// [`UsageCollectorPluginError`] is intentionally not `Clone` (it is a
/// foundation-owned SPI contract type — `cpt-cf-usage-collector-dod-*
/// -plugin-contract-stability`), so [`ChRecordStore::create_batch`]
/// reconstructs an equivalent value per variant instead of cloning a shared
/// instance. Only [`CatalogLockPort::acquire_exclusive_for_create`],
/// [`ChRecordStore::check_catalog_existence`], [`ChRecordStore::batch_dedup_lookup`],
/// and [`ClusterLockGuard::ensure_still_held`](crate::infra::coordination::lock_manager::ClusterLockGuard::ensure_still_held)
/// can actually produce (`Transient` / `UsageTypeNotFound` / `Internal`) are
/// reachable here; the fallback arm exists only because the enum is
/// `#[non_exhaustive]`.
fn err_for_partition(err: &UsageCollectorPluginError) -> UsageCollectorPluginError {
    match err {
        UsageCollectorPluginError::Transient {
            detail,
            retry_after_seconds,
        } => UsageCollectorPluginError::Transient {
            detail: detail.clone(),
            retry_after_seconds: *retry_after_seconds,
        },
        UsageCollectorPluginError::UsageTypeNotFound { gts_id } => {
            UsageCollectorPluginError::UsageTypeNotFound {
                gts_id: gts_id.clone(),
            }
        }
        UsageCollectorPluginError::Internal(msg) => {
            UsageCollectorPluginError::Internal(msg.clone())
        }
        other => UsageCollectorPluginError::internal(other.to_string()),
    }
}

/// Fan a failed batch write out across the outcome slots that depended on it.
///
/// Rows absorbed from storage keep whatever the dedup read decided for them —
/// a write that never landed cannot invalidate a row that was already there —
/// so only the slots backed by a composed row are rewritten.
fn apply_insert_failure(
    err: &UsageCollectorPluginError,
    row_slots: &[Vec<usize>],
    outcomes: &mut [Option<Result<UsageRecord, UsageCollectorPluginError>>],
) {
    for slots in row_slots {
        for &idx in slots {
            outcomes[idx] = Some(Err(err_for_partition(err)));
        }
    }
}

impl ChRecordStore {
    /// Resolve a dedup-key hit against an already-materialised row.
    ///
    /// A stored row is absorbed only when it is still `active` and every
    /// canonical field matches. An `inactive` stored row means the dedup key
    /// was created and then deactivated, so re-creating it must not resurrect
    /// the deactivated row as a silent absorb — the key is already bound to a
    /// record the caller cannot have back, which is exactly
    /// [`UsageCollectorPluginError::IdempotencyConflict`].
    fn resolve_dedup_hit(
        &self,
        row: &UsageRecordRow,
        record: &UsageRecord,
    ) -> Result<UsageRecord, UsageCollectorPluginError> {
        if row.status == UsageRecordStatusCode::Inactive || !canonical_equal(row, record)? {
            self.metrics.inc_idempotency_conflict();
            return Err(UsageCollectorPluginError::IdempotencyConflict {
                idempotency_key: record.idempotency_key.as_str().to_owned(),
                existing_id: row.id,
            });
        }
        self.metrics.inc_dedup_absorbed();
        UsageRecord::try_from(row.clone())
    }

    /// Critical section of create while holding `guard`.
    ///
    /// `op_start` is the caller's critical-section entry instant, forwarded to
    /// the insert-latency histogram so lock and catalog contention are inside
    /// the observed window.
    async fn create_under_lock(
        &self,
        record: UsageRecord,
        guard: &dyn LockGuardPort,
        op_start: Instant,
    ) -> Result<UsageRecord, UsageCollectorPluginError> {
        self.check_catalog_existence(&record.gts_id).await?;

        let stored = self.dedup_point_lookup(&record).await?;

        if let Some(row) = stored {
            return self.resolve_dedup_hit(&row, &record);
        }

        if let Err(e) = guard.ensure_still_held().await {
            self.metrics.inc_lock_manager_unavailable(LockMode::Create);
            return Err(e);
        }

        if record.corrects_id.is_some() {
            self.metrics.inc_compensation();
        }

        let version = current_merge_version();
        let row = UsageRecordRow::from((&record, version));
        self.insert_record(&row, op_start).await?;
        UsageRecord::try_from(row)
    }

    /// Critical section of one `gts_id` partition of [`Self::create_batch`],
    /// run while its exclusive partition lock is held.
    ///
    /// Mirrors [`Self::create_under_lock`]'s read phase: catalog existence
    /// check, batch dedup pre-read, then a lease renewal so the caller knows
    /// the partition is still owned before any row is composed. Returns the
    /// stored rows keyed by their dedup identity.
    ///
    /// # Errors
    ///
    /// Returns `UsageTypeNotFound` when the partition's usage type is absent,
    /// and `Transient` / `Internal` on `ClickHouse` or lease failures. Every
    /// error is a whole-partition failure — the caller fans it out across that
    /// partition's outcome slots.
    async fn create_partition_under_lock(
        &self,
        records: &[UsageRecord],
        idxs: &[usize],
        guard: &dyn LockGuardPort,
    ) -> Result<HashMap<DedupKey, UsageRecordRow>, UsageCollectorPluginError> {
        let first = *idxs.first().ok_or_else(|| {
            UsageCollectorPluginError::internal("empty gts_id partition (invariant break)")
        })?;
        self.check_catalog_existence(&records[first].gts_id).await?;

        let record_refs: Vec<&UsageRecord> = idxs.iter().map(|&i| &records[i]).collect();
        let existing = self.batch_dedup_lookup(&record_refs).await?;

        if let Err(e) = guard.ensure_still_held().await {
            self.metrics.inc_lock_manager_unavailable(LockMode::Create);
            return Err(e);
        }

        Ok(existing)
    }
}

#[async_trait]
impl RecordStore for ChRecordStore {
    // @cpt-flow:cpt-cf-uc-ch-plugin-seq-ingest-dedup
    #[instrument(skip(self, record), fields(gts_id = %record.gts_id.as_ref()))]
    async fn create(&self, record: UsageRecord) -> Result<UsageRecord, UsageCollectorPluginError> {
        let op_start = Instant::now();
        let guard = self
            .lock_manager
            .acquire_exclusive_for_create(record.gts_id.as_ref())
            .await?;

        let result = self
            .create_under_lock(record, guard.as_ref(), op_start)
            .await;

        if let Err(e) = guard.release().await {
            tracing::warn!(error = %e, "failed to release create cluster lock");
            if result.is_ok() {
                return Err(e);
            }
        }

        result
    }

    // @cpt-flow:cpt-cf-uc-ch-plugin-seq-ingest-batch
    #[instrument(skip(self, records), fields(batch_size = records.len()))]
    async fn create_batch(
        &self,
        records: Vec<UsageRecord>,
    ) -> Result<Vec<Result<UsageRecord, UsageCollectorPluginError>>, UsageCollectorPluginError>
    {
        if records.is_empty() {
            tracing::warn!(
                "create_usage_records called with an empty batch (host-contract breach)"
            );
            return Err(UsageCollectorPluginError::internal(
                "create_usage_records called with an empty batch (host-contract breach)",
            ));
        }

        let op_start = Instant::now();

        let mut grouped: HashMap<&str, Vec<usize>> = HashMap::new();
        for (idx, record) in records.iter().enumerate() {
            grouped.entry(record.gts_id.as_ref()).or_default().push(idx);
        }

        // Acquiring per-`gts_id` locks in `HashMap` iteration order lets two
        // concurrent multi-type batches take the same pair of locks in opposite
        // orders and deadlock until both leases lapse. Sorting the partition
        // keys gives every caller one global acquisition order.
        let mut partitions: Vec<(&str, Vec<usize>)> = grouped.into_iter().collect();
        partitions.sort_unstable_by(|a, b| a.0.cmp(b.0));

        let mut outcomes: Vec<Option<Result<UsageRecord, UsageCollectorPluginError>>> =
            (0..records.len()).map(|_| None).collect();

        // Phase 1: take every partition lock up front, sequentially, in the
        // sorted order above.
        let mut locked: Vec<(usize, Box<dyn LockGuardPort>)> = Vec::with_capacity(partitions.len());
        for (p_idx, (partition_gts_id, idxs)) in partitions.iter().enumerate() {
            match self
                .lock_manager
                .acquire_exclusive_for_create(partition_gts_id)
                .await
            {
                Ok(guard) => locked.push((p_idx, guard)),
                Err(e) => {
                    for &idx in idxs {
                        outcomes[idx] = Some(Err(err_for_partition(&e)));
                    }
                }
            }
        }

        // Phase 2: every partition now holds its lock, so their read phases are
        // independent and run concurrently.
        let prepared = join_all(locked.iter().map(|(p_idx, guard)| {
            self.create_partition_under_lock(&records, &partitions[*p_idx].1, guard.as_ref())
        }))
        .await;

        // Phase 3: compose rows sequentially so version offsets and
        // within-batch dedup stay deterministic.
        let version_base = current_merge_version();
        let mut next_offset: u64 = 0;
        let mut to_insert: Vec<UsageRecordRow> = Vec::new();
        // Row position in `to_insert` rather than a clone of the row itself.
        let mut insert_map: HashMap<DedupKey, usize> = HashMap::new();
        // Parallel to `to_insert`: the `held` position that composed each row,
        // and the outcome slots whose success depends on that row landing.
        let mut row_partition: Vec<usize> = Vec::new();
        let mut row_slots: Vec<Vec<usize>> = Vec::new();
        let mut held: Vec<(usize, Box<dyn LockGuardPort>)> = Vec::with_capacity(locked.len());

        for ((p_idx, guard), partition_result) in locked.into_iter().zip(prepared) {
            let idxs = &partitions[p_idx].1;
            let existing = match partition_result {
                Ok(existing) => existing,
                Err(e) => {
                    for &idx in idxs {
                        outcomes[idx] = Some(Err(err_for_partition(&e)));
                    }
                    if let Err(rel) = guard.release().await {
                        tracing::warn!(error = %rel, "failed to release create-batch cluster lock");
                    }
                    continue;
                }
            };

            let held_idx = held.len();
            for &idx in idxs {
                let record = &records[idx];
                let key = record_dedup_key(record);
                let outcome = if let Some(stored_row) = existing.get(&key) {
                    self.resolve_dedup_hit(stored_row, record)
                } else if let Some(&row_idx) = insert_map.get(&key) {
                    // A second row for the same dedup key inside this batch is
                    // only absorbed when it is canonically identical to the one
                    // already composed; otherwise it is a conflict just as it
                    // would be against a stored row.
                    let resolved = self.resolve_dedup_hit(&to_insert[row_idx], record);
                    if resolved.is_ok() {
                        row_slots[row_idx].push(idx);
                    }
                    resolved
                } else {
                    let version = version_base.saturating_add(next_offset);
                    next_offset += 1;
                    let row = UsageRecordRow::from((record, version));
                    let row_idx = to_insert.len();
                    to_insert.push(row);
                    insert_map.insert(key, row_idx);
                    row_partition.push(held_idx);
                    row_slots.push(vec![idx]);
                    if record.corrects_id.is_some() {
                        self.metrics.inc_compensation();
                    }
                    UsageRecord::try_from(to_insert[row_idx].clone())
                };
                outcomes[idx] = Some(outcome);
            }

            held.push((p_idx, guard));
        }

        // Phase 4: renew every lease immediately before the combined write, so
        // a lease that expired while the other partitions were being prepared
        // cannot let a concurrent `delete_usage_type` orphan these rows.
        let mut expired: HashSet<usize> = HashSet::new();
        for (held_idx, (p_idx, guard)) in held.iter().enumerate() {
            if let Err(e) = guard.ensure_still_held().await {
                self.metrics.inc_lock_manager_unavailable(LockMode::Create);
                for &idx in &partitions[*p_idx].1 {
                    outcomes[idx] = Some(Err(err_for_partition(&e)));
                }
                expired.insert(held_idx);
            }
        }
        if !expired.is_empty() {
            let mut kept_rows = Vec::with_capacity(to_insert.len());
            let mut kept_slots = Vec::with_capacity(row_slots.len());
            for ((row, slots), partition) in
                to_insert.into_iter().zip(row_slots).zip(&row_partition)
            {
                if !expired.contains(partition) {
                    kept_rows.push(row);
                    kept_slots.push(slots);
                }
            }
            to_insert = kept_rows;
            row_slots = kept_slots;
        }

        let insert_result = self.insert_records(&to_insert, op_start).await;

        // The cluster guard's `Drop` is a no-op, so every guard is released
        // explicitly — including on the insert-failure path, which would
        // otherwise hold each `gts_id` until its lease lapsed.
        for (_, guard) in held {
            if let Err(e) = guard.release().await {
                tracing::warn!(error = %e, "failed to release create-batch cluster lock");
            }
        }

        // A failed write does not invalidate the outcomes already decided for
        // absorbed rows, so it is reported per slot rather than as a top-level
        // error that would discard the whole batch's per-record contract.
        if let Err(e) = insert_result {
            tracing::warn!(error = %e, "create-batch insert failed; reporting per-record outcomes");
            apply_insert_failure(&e, &row_slots, &mut outcomes);
        }

        let results = outcomes
            .into_iter()
            .enumerate()
            .map(|(idx, outcome)| {
                outcome.unwrap_or_else(|| {
                    Err(dedup_invariant_break(
                        &records[idx],
                        "batch index unresolved after partition processing (invariant break)",
                    ))
                })
            })
            .collect();

        Ok(results)
    }

    // @cpt-flow:cpt-cf-uc-ch-plugin-seq-get
    #[instrument(skip_all, fields(record_id = %id))]
    async fn get(&self, id: Uuid) -> Result<UsageRecord, UsageCollectorPluginError> {
        let sql = format!("SELECT {RECORD_COLUMNS} FROM usage_records FINAL WHERE id = ?");
        let row: Option<UsageRecordRow> = self
            .client
            .query(&sql)
            .bind(id.to_string())
            .fetch_optional()
            .await
            .map_err(|e| tracked_ch_err(&self.metrics, &e))?;

        match row {
            Some(row) => UsageRecord::try_from(row),
            None => Err(UsageCollectorPluginError::UsageRecordNotFound { id }),
        }
    }

    // @cpt-flow:cpt-cf-uc-ch-plugin-seq-list-keyset
    #[instrument(skip_all, fields(gts_id = %gts_id.as_ref()))]
    async fn list(
        &self,
        gts_id: UsageTypeGtsId,
        query: &ODataQuery,
        metadata_filter: &[MetadataFilter],
    ) -> Result<ODataPage<UsageRecord>, UsageCollectorPluginError> {
        let _timer =
            OpDurationGuard::start(Arc::clone(&self.metrics), TimedOp::Query(QueryKind::Raw));
        self.metrics.inc_query_request(QueryKind::Raw);

        let limit =
            effective_page_size(query.limit, crate::infra::storage::query::DEFAULT_PAGE_SIZE);

        // `gts_id = ?` is the first bind; all subsequent binds follow.
        let mut ctx = SqlCtx::new();
        let mut clauses: Vec<String> = vec!["gts_id = ?".to_owned()];

        // `$filter` (validated AST → typed node → parameterised fragment).
        if let Some(expr) = query.filter() {
            let node = convert_expr_to_filter_node::<UsageRecordFilterField>(expr)
                .map_err(|e| UsageCollectorPluginError::internal(format!("invalid filter: {e}")))?;
            let fragment =
                crate::infra::storage::query::translate::translate_record_filter(&node, &mut ctx)
                    .map_err(UsageCollectorPluginError::internal)?;
            clauses.push(fragment);
        }

        // Metadata side-channel.
        Self::push_metadata_filters(metadata_filter, &mut ctx, &mut clauses);

        // Keyset continuation (forward only).
        if let Some(cursor) = query.cursor.as_ref() {
            ensure_forward_cursor(cursor).map_err(UsageCollectorPluginError::internal)?;
            if cursor.f.as_deref() != query.filter_hash.as_deref() {
                return Err(UsageCollectorPluginError::internal(
                    "cursor filter hash mismatch",
                ));
            }
            if !query.order.equals_signed_tokens(&cursor.s) {
                return Err(UsageCollectorPluginError::internal(
                    "cursor sort order mismatch",
                ));
            }
            let order_pairs: Vec<(&str, bool)> = query
                .order
                .0
                .iter()
                .map(|key| (key.field.as_str(), matches!(key.dir, SortDir::Asc)))
                .collect();
            let predicate = keyset_predicate(
                &order_pairs,
                &cursor.k,
                record_column,
                |name| UsageRecordFilterField::from_name(name).map(|f| f.kind()),
                is_keyset_safe_record_field,
                &mut ctx,
            )
            .map_err(UsageCollectorPluginError::internal)?;
            clauses.push(predicate);
        }

        let order_sql = render_order_by(&query.order, record_column)
            .map_err(UsageCollectorPluginError::internal)?;

        let sql = format!(
            "SELECT {RECORD_COLUMNS} FROM usage_records FINAL \
             WHERE {} ORDER BY {order_sql} LIMIT {}",
            clauses.join(" AND "),
            limit.saturating_add(1),
        );

        let mut q = self.client.query(&sql).bind(gts_id.as_ref());
        for b in &ctx.binds {
            q = bind_one(q, b);
        }
        let mut rows: Vec<UsageRecordRow> = q
            .fetch_all()
            .await
            .map_err(|e| tracked_ch_err(&self.metrics, &e))?;

        // Look-ahead row present → a next page exists.
        let has_next = rows.len() > usize::try_from(limit).unwrap_or(usize::MAX);
        if has_next {
            rows.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
        }

        let next_cursor = if has_next {
            let last = rows.last().ok_or_else(|| {
                UsageCollectorPluginError::internal("non-empty page lost its tail")
            })?;
            let keys = query
                .order
                .0
                .iter()
                .map(|key| {
                    record_row_key(last, &key.field).ok_or_else(|| {
                        UsageCollectorPluginError::internal(format!(
                            "order field `{}` has no cursor key on the row",
                            key.field
                        ))
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            let token = encode_next_cursor(&query.order, &keys, query.filter_hash.as_deref())
                .map_err(UsageCollectorPluginError::internal)?;
            Some(token)
        } else {
            None
        };

        let items = rows
            .into_iter()
            .map(UsageRecord::try_from)
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

    // @cpt-flow:cpt-cf-uc-ch-plugin-seq-query-aggregated
    #[instrument(skip_all, fields(gts_id = %gts_id.as_ref()))]
    async fn aggregate(
        &self,
        gts_id: UsageTypeGtsId,
        query: &ODataQuery,
        metadata_filter: &[MetadataFilter],
        spec: AggregationSpec,
    ) -> Result<AggregationResult, UsageCollectorPluginError> {
        let _timer = OpDurationGuard::start(
            Arc::clone(&self.metrics),
            TimedOp::Query(QueryKind::Aggregated),
        );
        self.metrics.inc_query_request(QueryKind::Aggregated);

        let mut ctx = SqlCtx::new();
        let mut clauses: Vec<String> =
            vec!["gts_id = ?".to_owned(), "status = 'active'".to_owned()];

        // `corrects_id` partition (plugin-spi.md §Method 3).
        if let Some(clause) = corrects_id_partition_clause(spec.op) {
            clauses.push(clause.to_owned());
        }

        // `$filter`.
        if let Some(expr) = query.filter() {
            let node = convert_expr_to_filter_node::<UsageRecordFilterField>(expr)
                .map_err(|e| UsageCollectorPluginError::internal(format!("invalid filter: {e}")))?;
            let fragment =
                crate::infra::storage::query::translate::translate_record_filter(&node, &mut ctx)
                    .map_err(UsageCollectorPluginError::internal)?;
            clauses.push(fragment);
        }

        // Metadata side-channel.
        Self::push_metadata_filters(metadata_filter, &mut ctx, &mut clauses);

        // Dimension SELECT exprs + subject-not-null guards.
        // SELECT-list binds (metadata keys) are collected separately from the
        // WHERE `ctx`: they appear first in the SQL text and must be applied
        // before `gts_id` and every WHERE `?`.
        let mut select_dims: Vec<String> = Vec::with_capacity(spec.group_by.len());
        let mut select_binds: Vec<SqlBind> = Vec::new();
        for dim in &spec.group_by {
            match dim {
                AggregationDimension::SubjectId => {
                    clauses.push("subject_id IS NOT NULL".to_owned());
                }
                AggregationDimension::SubjectType => {
                    clauses.push("subject_type IS NOT NULL".to_owned());
                }
                _ => {}
            }
            let (expr, bind) = dimension_select_expr(dim);
            select_dims.push(expr);
            if let Some(b) = bind {
                select_binds.push(b);
            }
        }

        let dim_count = select_dims.len();
        let mut select_parts = select_dims;
        select_parts.push(agg_select_expr(spec.op).to_owned());
        let group_by = if dim_count == 0 {
            String::new()
        } else {
            let ordinals = (1..=dim_count)
                .map(|n| n.to_string())
                .collect::<Vec<_>>()
                .join(", ");
            format!(" GROUP BY {ordinals}")
        };

        let limit_clause = aggregate_limit_clause(dim_count);

        // `JSONEachRow` lets us decode a result whose column count varies per call
        // without a fixed `Row` struct. Every column is aliased (`d0`…`dN`, `agg`)
        // so the JSON keys stay predictable whatever the dimension exprs are.
        let aliased_select = select_parts
            .iter()
            .enumerate()
            .map(|(i, expr)| {
                if i == dim_count {
                    format!("{expr} AS agg")
                } else {
                    format!("{expr} AS d{i}")
                }
            })
            .collect::<Vec<_>>()
            .join(", ");

        let sql = format!(
            "SELECT {aliased_select} FROM usage_records FINAL \
             WHERE {}{group_by}{limit_clause}",
            clauses.join(" AND "),
        );

        // Bind order matches left-to-right `?` in `sql`: SELECT metadata keys,
        // then `gts_id`, then filter / metadata-filter WHERE binds.
        let mut q = self.client.query(&sql);
        for b in &select_binds {
            q = bind_one(q, b);
        }
        q = q.bind(gts_id.as_ref());
        for b in &ctx.binds {
            q = bind_one(q, b);
        }

        // Column names for the JSON key lookup below, matching the aliases above.
        let dim_names: Vec<String> = (0..dim_count).map(|i| format!("d{i}")).collect();

        // Stream-parse `JSONEachRow` as chunks arrive. The server-side `LIMIT`
        // from `aggregate_limit_clause` still caps row count; streaming avoids
        // holding the full encoded body in memory in addition to the buckets.
        let mut parser = AggregateNdjsonParser::new(dim_names);
        let mut cursor = q
            .fetch_bytes("JSONEachRow")
            .map_err(|e| tracked_ch_err(&self.metrics, &e))?;
        while let Some(chunk) = cursor
            .next()
            .await
            .map_err(|e| tracked_ch_err(&self.metrics, &e))?
        {
            parser.push_chunk(&chunk)?;
        }

        Ok(AggregationResult {
            buckets: parser.finish()?,
        })
    }

    // @cpt-flow:cpt-cf-uc-ch-plugin-seq-deactivate-cascade
    #[instrument(skip_all, fields(record_id = %id))]
    async fn deactivate(&self, id: Uuid) -> Result<(), UsageCollectorPluginError> {
        let _timer = OpDurationGuard::start(Arc::clone(&self.metrics), TimedOp::Deactivate);

        // No coordination lock required for deactivation (DESIGN.md §3.6): the host
        // prevents a concurrent compensation from reaching create_usage_record while
        // a deactivation is in flight (plugin-spi.md Method 5 caller-side rule).

        // Step 1: Read the target + active depth-1 compensations.
        // Enable skip indexes under FINAL so `idx_records_id` /
        // `idx_records_corrects_id` can prune granules; keep exact-mode on so
        // FINAL resolution stays correct when a skip index drops a granule.
        let sql = format!(
            "SELECT {RECORD_COLUMNS} FROM usage_records FINAL \
             WHERE id = ? OR (corrects_id = ? AND status = 'active')"
        );
        let rows: Vec<UsageRecordRow> = self
            .client
            .query(&sql)
            .with_setting("use_skip_indexes_if_final", "1")
            .with_setting("use_skip_indexes_if_final_exact_mode", "1")
            .bind(id.to_string())
            .bind(id.to_string())
            .fetch_all()
            .await
            .map_err(|e| tracked_ch_err(&self.metrics, &e))?;

        // Step 2: Identify target and compensation rows.
        let target_row = rows.iter().find(|r| r.id == id);
        match target_row {
            None => return Err(UsageCollectorPluginError::UsageRecordNotFound { id }),
            Some(r) if r.status == UsageRecordStatusCode::Inactive => {
                return Err(UsageCollectorPluginError::UsageRecordAlreadyInactive { id });
            }
            Some(_) => {}
        }

        // Step 3: Compose one versioned marker row per affected id (target +
        // active compensations). `version_higher_than` mints each marker's
        // version off the row it supersedes, so no batch-wide base version is
        // needed — the per-row offset only spaces markers whose source rows
        // already share a version.
        let markers: Vec<UsageRecordRow> = rows
            .iter()
            .enumerate()
            .map(|(i, r)| make_inactive_marker(r, version_higher_than(r.version, i as u64), 0))
            .collect();

        // Step 4: One multi-row INSERT for all marker rows.
        // ATOMICITY NOTE (DESIGN.md §3.6): A single ClickHouse INSERT is applied as
        // one atomic part write. A FINAL-qualified reader either sees the pre-cascade
        // state or the fully-flipped state — never a partially-flipped cascade.
        self.insert_records(&markers, Instant::now()).await?;

        Ok(())
    }
}

/// Incremental `JSONEachRow` decoder for aggregate responses.
///
/// Keeps only an incomplete trailing line between chunks so peak memory is
/// dominated by the already-parsed [`AggregationBucket`]s (bounded by the
/// server-side bucket-cap `LIMIT`), not a second full copy of the body.
struct AggregateNdjsonParser {
    leftover: Vec<u8>,
    dim_names: Vec<String>,
    buckets: Vec<AggregationBucket>,
}

impl AggregateNdjsonParser {
    fn new(dim_names: Vec<String>) -> Self {
        Self {
            leftover: Vec::new(),
            dim_names,
            buckets: Vec::new(),
        }
    }

    fn push_chunk(&mut self, chunk: &[u8]) -> Result<(), UsageCollectorPluginError> {
        self.leftover.extend_from_slice(chunk);
        self.consume_complete_lines()
    }

    fn finish(mut self) -> Result<Vec<AggregationBucket>, UsageCollectorPluginError> {
        self.consume_complete_lines()?;
        if !self.leftover.is_empty() {
            let line = std::str::from_utf8(&self.leftover).map_err(|e| {
                UsageCollectorPluginError::internal(format!("aggregate response utf-8 error: {e}"))
            })?;
            if let Some(bucket) = parse_aggregate_line(line, &self.dim_names)? {
                self.buckets.push(bucket);
            }
            self.leftover.clear();
        }
        Ok(self.buckets)
    }

    fn consume_complete_lines(&mut self) -> Result<(), UsageCollectorPluginError> {
        let mut start = 0;
        while let Some(rel) = self.leftover[start..].iter().position(|&b| b == b'\n') {
            let end = start + rel;
            let line = std::str::from_utf8(&self.leftover[start..end]).map_err(|e| {
                UsageCollectorPluginError::internal(format!("aggregate response utf-8 error: {e}"))
            })?;
            if let Some(bucket) = parse_aggregate_line(line, &self.dim_names)? {
                self.buckets.push(bucket);
            }
            start = end + 1;
        }
        if start > 0 {
            self.leftover.drain(..start);
        }
        Ok(())
    }
}

/// Decode a complete `JSONEachRow` aggregate response body into
/// [`AggregationBucket`]s.
///
/// Test helper over [`AggregateNdjsonParser`]; production aggregate path
/// pushes chunks into the parser directly.
///
/// `dim_names` are the `d0`…`dN` column aliases the SELECT emitted, in
/// `group_by` order; a missing or non-string dimension decodes as an empty
/// key component. The `agg` column is accepted as a JSON string or number and
/// parsed into a [`BigDecimal`]; a JSON `null` (an empty `MIN`/`MAX`/`AVG`
/// group) becomes `None`.
///
/// # Errors
///
/// Returns [`UsageCollectorPluginError::Internal`] when a line is not valid
/// JSON, carries an `agg` value of an unexpected JSON type, or holds an
/// unparseable decimal.
#[cfg(test)]
fn parse_aggregate_response(
    bytes: &[u8],
    dim_names: &[String],
) -> Result<Vec<AggregationBucket>, UsageCollectorPluginError> {
    let mut parser = AggregateNdjsonParser::new(dim_names.to_vec());
    parser.push_chunk(bytes)?;
    parser.finish()
}

/// Parse one NDJSON line into an optional bucket (`None` for blank lines).
fn parse_aggregate_line(
    line: &str,
    dim_names: &[String],
) -> Result<Option<AggregationBucket>, UsageCollectorPluginError> {
    let line = line.trim();
    if line.is_empty() {
        return Ok(None);
    }
    let obj: serde_json::Value = serde_json::from_str(line).map_err(|e| {
        UsageCollectorPluginError::internal(format!("aggregate JSON parse error: {e}"))
    })?;
    let key = dim_names
        .iter()
        .map(|dim_name| {
            obj.get(dim_name)
                .and_then(|v| v.as_str())
                .map(str::to_owned)
                .unwrap_or_default()
        })
        .collect();
    let value = match obj.get("agg") {
        None | Some(serde_json::Value::Null) => None,
        Some(v) => {
            let s = match v {
                serde_json::Value::String(s) => s.clone(),
                serde_json::Value::Number(n) => n.to_string(),
                other => {
                    return Err(UsageCollectorPluginError::internal(format!(
                        "unexpected aggregate value type: {other}"
                    )));
                }
            };
            Some(s.parse::<BigDecimal>().map_err(|e| {
                UsageCollectorPluginError::internal(format!("aggregate value parse error: {e}"))
            })?)
        }
    };
    Ok(Some(AggregationBucket { key, value }))
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "record_store_tests.rs"]
mod record_store_tests;
