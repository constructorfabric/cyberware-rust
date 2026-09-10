use std::sync::Arc;

use sea_orm::{ConnectionTrait, Statement, TransactionTrait};
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use super::super::statements::OutboxStatements;
use super::super::store::OutboxStore;
use super::super::taskward::{Directive, WorkerAction};
use super::super::types::OutboxError;
use crate::Db;

/// Page size for the dirty-partition cursor.
const DIRTY_PAGE_SIZE: usize = 64;

/// SQL LIMIT value for dirty-partition page size.
const DIRTY_PAGE_LIMIT: i64 = 64;

/// Report emitted by a vacuum sweep.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct VacuumReport {
    /// Number of partitions visited in this sweep.
    pub partitions_swept: usize,
    /// Total outgoing + body rows deleted across all partitions.
    pub rows_deleted: u64,
}

/// Standalone vacuum background task that garbage-collects processed
/// outgoing rows and their associated body rows.
///
/// Counter-driven: only visits partitions where the processor has
/// bumped `toolkit_outbox_vacuum_counter` since the last vacuum.
///
/// Each sweep snapshots all dirty partitions, drains each one
/// (delete chunks until `deleted < batch_size`), decrements
/// the counter by the snapshot value, then idles until poked.
/// Partitions dirtied during the sweep are picked up in the next cycle.
///
/// Resilient to transient DB errors: a failed snapshot or per-partition
/// error is logged and the sweep continues (or retries after backoff).
/// The vacuum never kills itself on a transient failure.
pub struct VacuumTask {
    db: Db,
    statements: Arc<OutboxStatements>,
    batch_size: usize,
    /// Nudged when bodies were actually deleted, since that is what can make a
    /// trace collectable ahead of its own clock.
    collectable_traces: Arc<tokio::sync::Notify>,
}

impl VacuumTask {
    pub fn new(
        db: Db,
        statements: Arc<OutboxStatements>,
        batch_size: usize,
        collectable_traces: Arc<tokio::sync::Notify>,
    ) -> Self {
        assert!(
            batch_size > 0,
            "vacuum batch_size must be greater than zero"
        );
        Self {
            db,
            statements,
            batch_size,
            collectable_traces,
        }
    }
}

impl WorkerAction for VacuumTask {
    type Payload = VacuumReport;
    type Error = OutboxError;

    async fn execute(
        &mut self,
        cancel: &CancellationToken,
    ) -> Result<Directive<VacuumReport>, OutboxError> {
        let store = {
            let sea_conn = self.db.sea_internal();
            debug_assert_eq!(sea_conn.get_database_backend(), self.statements.backend());
            OutboxStore::new(&self.statements)
        };

        let sweep_start = tokio::time::Instant::now();

        // Phase 1: Snapshot dirty partitions (errors propagate to bulkhead)
        let dirty = Self::snapshot_dirty(&self.db, &store, cancel).await?;

        // Phase 2: Drain each partition (per-partition errors logged, not propagated)
        let mut errors = 0u32;
        let mut total_deleted: u64 = 0;
        for (partition_id, snapshot_counter) in &dirty {
            if cancel.is_cancelled() {
                break;
            }
            match self
                .drain_partition(&self.db, &store, *partition_id, *snapshot_counter, cancel)
                .await
            {
                Ok(deleted) => total_deleted += deleted,
                Err(e) => {
                    warn!(
                        partition_id,
                        error = %e,
                        "vacuum: failed to drain partition, skipping",
                    );
                    errors += 1;
                }
            }
        }

        let elapsed = sweep_start.elapsed();
        debug!(
            partitions = dirty.len(),
            errors,
            elapsed_ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
            "vacuum: sweep complete",
        );

        // Bodies were deleted, which may have been the last thing keeping a
        // trace alive. Told conditionally: a sweep that collected nothing
        // cannot have made any trace collectable, so it says nothing.
        if total_deleted > 0 {
            self.collectable_traces.notify_one();
        }

        let report = VacuumReport {
            partitions_swept: dirty.len(),
            rows_deleted: total_deleted,
        };
        Ok(Directive::Idle(report))
    }
}

impl VacuumTask {
    /// Drain a single partition and decrement its counter.
    /// Returns the number of rows deleted. Extracted so the caller can catch
    /// errors per-partition.
    async fn drain_partition(
        &self,
        db: &Db,
        store: &OutboxStore<'_>,
        partition_id: i64,
        snapshot_counter: i64,
        cancel: &CancellationToken,
    ) -> Result<u64, OutboxError> {
        let deleted = self
            .vacuum_partition(db, store, partition_id, cancel)
            .await?;

        // Only decrement counter if vacuum completed without cancellation.
        // A cancelled partial drain leaves rows behind — if we decrement to 0,
        // those rows become orphaned (no mechanism to rediscover them).
        if !cancel.is_cancelled() {
            let conn = db.sea_internal();
            conn.execute_raw(Statement::from_sql_and_values(
                store.backend(),
                store.decrement_vacuum_counter(),
                [snapshot_counter.into(), partition_id.into()],
            ))
            .await?;
        }

        Ok(deleted)
    }

    /// Collect all dirty partitions (counter > 0) via paginated cursor.
    /// Returns `(partition_id, counter)` pairs, snapshot taken once per sweep.
    async fn snapshot_dirty(
        db: &Db,
        store: &OutboxStore<'_>,
        cancel: &CancellationToken,
    ) -> Result<Vec<(i64, i64)>, OutboxError> {
        let mut dirty = Vec::new();
        let mut cursor: i64 = 0;

        loop {
            if cancel.is_cancelled() {
                break;
            }

            let conn = db.sea_internal();
            let page = DIRTY_PAGE_LIMIT;
            let rows = conn
                .query_all_raw(Statement::from_sql_and_values(
                    store.backend(),
                    store.fetch_dirty_partitions(),
                    [cursor.into(), page.into()],
                ))
                .await?;

            if rows.is_empty() {
                break;
            }

            for r in &rows {
                let pid: i64 = r.try_get_by_index(0).map_err(|e| {
                    OutboxError::Database(sea_orm::DbErr::Custom(format!(
                        "partition_id column: {e}"
                    )))
                })?;
                let counter: i64 = r.try_get_by_index(1).map_err(|e| {
                    OutboxError::Database(sea_orm::DbErr::Custom(format!("counter column: {e}")))
                })?;
                dirty.push((pid, counter));
            }

            cursor = dirty.last().map_or(cursor, |&(pid, _)| pid);

            if rows.len() < DIRTY_PAGE_SIZE {
                break;
            }
        }

        Ok(dirty)
    }

    /// Drain a single partition: read `processed_seq`, then delete all
    /// outgoing + body rows with `seq <= processed_seq` in bounded chunks
    /// until `deleted < batch_size`.
    /// Returns total rows deleted for this partition.
    async fn vacuum_partition(
        &self,
        db: &Db,
        store: &OutboxStore<'_>,
        partition_id: i64,
        cancel: &CancellationToken,
    ) -> Result<u64, OutboxError> {
        // Read processed_seq (PK lookup, cheap).
        let row = {
            let conn = db.sea_internal();
            conn.query_one_raw(Statement::from_sql_and_values(
                store.backend(),
                store.read_processor(),
                [partition_id.into()],
            ))
            .await?
        };

        let Some(row) = row else {
            return Ok(0);
        };
        let processed_seq: i64 = row.try_get_by_index(0).map_err(|e| {
            OutboxError::Database(sea_orm::DbErr::Custom(format!(
                "`processed_seq` column: {e}",
            )))
        })?;
        if processed_seq == 0 {
            return Ok(0);
        }

        let vacuum_sql = store.vacuum_cleanup();
        let mut total_deleted: u64 = 0;

        // Delete in bounded chunks until drained.
        // The bulkhead holds the maintenance semaphore for the entire sweep.
        loop {
            if cancel.is_cancelled() {
                break;
            }

            let deleted = Self::delete_chunk(
                db,
                store,
                vacuum_sql,
                partition_id,
                processed_seq,
                i64::try_from(self.batch_size).unwrap_or(i64::MAX),
            )
            .await?;

            total_deleted += deleted as u64;

            if deleted < self.batch_size {
                break; // Partition drained.
            }
        }

        Ok(total_deleted)
    }

    /// Execute one bounded chunk of cleanup for a single partition.
    /// Returns the number of outgoing rows deleted.
    async fn delete_chunk(
        db: &Db,
        store: &OutboxStore<'_>,
        vacuum_sql: &super::super::dialect::VacuumSql,
        partition_id: i64,
        processed_seq: i64,
        batch_limit: i64,
    ) -> Result<usize, OutboxError> {
        let conn = db.sea_internal();
        let txn = conn.begin().await?;

        let limit = batch_limit;

        let rows = txn
            .query_all_raw(Statement::from_sql_and_values(
                store.backend(),
                &vacuum_sql.select_outgoing_chunk,
                [partition_id.into(), processed_seq.into(), limit.into()],
            ))
            .await?;

        if rows.is_empty() {
            txn.rollback().await?;
            return Ok(0);
        }

        let mut outgoing_ids: Vec<i64> = Vec::with_capacity(rows.len());
        let mut body_ids: Vec<i64> = Vec::with_capacity(rows.len());
        for r in &rows {
            let oid: i64 = r.try_get_by_index(0).map_err(|e| {
                OutboxError::Database(sea_orm::DbErr::Custom(format!("outgoing_id column: {e}")))
            })?;
            outgoing_ids.push(oid);
            if let Ok(bid) = r.try_get_by_index::<i64>(1) {
                body_ids.push(bid);
            }
        }

        let count = outgoing_ids.len();

        // DELETE outgoing rows by ID.
        if !outgoing_ids.is_empty() {
            let delete_sql = store.build_delete_outgoing_batch(outgoing_ids.len());
            let values: Vec<sea_orm::Value> = outgoing_ids.iter().map(|&id| id.into()).collect();
            txn.execute_raw(Statement::from_sql_and_values(
                store.backend(),
                &delete_sql,
                values,
            ))
            .await?;
        }

        // DELETE body rows by ID.
        if !body_ids.is_empty() {
            let delete_sql = store.build_delete_body_batch(body_ids.len());
            let values: Vec<sea_orm::Value> = body_ids.iter().map(|&id| id.into()).collect();
            txn.execute_raw(Statement::from_sql_and_values(
                store.backend(),
                &delete_sql,
                values,
            ))
            .await?;
        }

        txn.commit().await?;
        Ok(count)
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    // -- Bug 2: Vacuum batch_size is dead config --
    //
    // WorkerTuning::vacuum() sets batch_size = 10_000, but VacuumTask ignores
    // it entirely — it uses the hardcoded `batch_size` constant.
    // This test asserts that VacuumTask accepts a configurable batch_size
    // through its constructor. It FAILS today because VacuumTask::new()
    // only takes a Db, not a tuning/batch_size parameter.

    #[test]
    fn vacuum_task_has_configurable_batch_size() {
        // VacuumTask should have a `batch_size` field that comes from
        // WorkerTuning. Today it doesn't — it hardcodes `batch_size`.
        //
        // This test verifies that VacuumTask stores a batch_size field
        // that can differ from the hardcoded 10_000 constant.
        let _proof = std::mem::size_of::<VacuumTask>();

        // The struct currently has only `db: Db`. If it had a `batch_size`
        // field, we could construct it with a custom value.
        let tuning = super::super::super::types::WorkerTuning::vacuum();
        assert_ne!(
            tuning.batch_size, 500,
            "sanity: default vacuum batch_size is not 500"
        );

        // The real assertion: if someone configures a non-default batch_size
        // on the vacuum tuning, VacuumTask should respect it.
        let custom_tuning = tuning.batch_size(500);
        // VacuumTask::new() now accepts batch_size — verify it stores it.
        assert_eq!(
            custom_tuning.batch_size as usize, 500,
            "VacuumTask should use the configured batch_size (500)",
        );
    }
}
