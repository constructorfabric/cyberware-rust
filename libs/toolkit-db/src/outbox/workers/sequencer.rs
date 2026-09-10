use std::sync::Arc;

use sea_orm::{ConnectionTrait, FromQueryResult, Statement, TransactionTrait};
use tokio_util::sync::CancellationToken;
use tracing::warn;

use super::super::Outbox;
use super::super::dialect::AllocSql;
use super::super::prioritizer::SharedPrioritizer;
use super::super::store::OutboxStore;
use super::super::taskward::{Directive, WorkerAction};
use super::super::types::{OutboxError, SequencerConfig};
use crate::Db;
use crate::contention::is_retryable_contention;

/// Report emitted by a sequencer execution cycle.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct SequencerReport {
    /// The partition that was processed (`-1` when idle / no work).
    pub partition_id: i64,
    /// Number of incoming rows claimed and sequenced.
    pub rows_claimed: u32,
}

/// Background sequencer that consumes from incoming, assigns per-partition
/// sequence numbers, and writes to outgoing.
///
/// Processes only dirty partitions (those with pending incoming rows).
/// Each partition is processed in its own transaction.
/// Uses `FOR UPDATE SKIP LOCKED` on Postgres/MySQL to allow concurrent
/// sequencers without deadlocks.
pub struct Sequencer {
    config: SequencerConfig,
    outbox: Arc<Outbox>,
    db: Db,
    /// Shared prioritizer for parallel sequencer workers.
    shared_prioritizer: Arc<SharedPrioritizer>,
}

#[derive(Debug, FromQueryResult)]
struct ClaimedIncoming {
    id: i64,
    body_id: i64,
}

impl Sequencer {
    /// Create a new sequencer.
    #[must_use]
    pub fn new(
        config: SequencerConfig,
        outbox: Arc<Outbox>,
        db: Db,
        shared_prioritizer: Arc<SharedPrioritizer>,
    ) -> Self {
        Self {
            config,
            outbox,
            db,
            shared_prioritizer,
        }
    }

    /// Process a single partition with a bounded inner drain loop.
    /// Each iteration runs in its own transaction.
    async fn process_partition(
        &self,
        partition_id: i64,
    ) -> Result<PartitionProcessResult, PartitionError> {
        let conn = self.db.sea_internal();
        debug_assert_eq!(
            conn.get_database_backend(),
            self.outbox.statements().backend()
        );
        let store = OutboxStore::new(self.outbox.statements());

        let mut drained = true;
        let mut total_claimed: u32 = 0;

        for _iteration in 0..self.config.max_inner_iterations {
            let txn = conn.begin().await?;

            // Try to acquire row lock
            if let Some(lock_sql) = store.lock_partition() {
                let locked = self
                    .try_lock_partition(&txn, &store, partition_id, lock_sql)
                    .await?;
                if !locked {
                    // Rollback (drop txn) and signal skip
                    drop(txn);
                    return Err(PartitionError::Skipped);
                }
            }

            // Claim incoming for this partition
            let claimed = self
                .claim_incoming_for_partition(&txn, &store, partition_id)
                .await?;

            if claimed.is_empty() {
                // Nothing to process — partition is fully drained
                drained = true;
                drop(txn);
                break;
            }

            let item_count = i64::try_from(claimed.len()).unwrap_or(i64::MAX);

            #[allow(clippy::cast_possible_truncation)]
            let drained_this_iteration = (claimed.len() as u32) < self.config.batch_size;

            // Allocate sequences
            let start_seq = self
                .allocate_sequences(&txn, &store, partition_id, item_count)
                .await?;

            let outgoing_sql = store.build_insert_outgoing_batch(claimed.len());
            let mut values: Vec<sea_orm::Value> = Vec::with_capacity(claimed.len() * 3);
            for (i, item) in claimed.iter().enumerate() {
                #[allow(clippy::cast_possible_wrap)]
                let seq = start_seq + 1 + i as i64;
                values.push(partition_id.into());
                values.push(item.body_id.into());
                values.push(seq.into());
            }
            txn.execute_raw(Statement::from_sql_and_values(
                store.backend(),
                &outgoing_sql,
                values,
            ))
            .await?;

            txn.commit().await?;

            #[allow(clippy::cast_possible_truncation)]
            {
                total_claimed += claimed.len() as u32;
            }

            // Post-commit: notify the partition's processor
            self.outbox.notify_partition(partition_id);

            if drained_this_iteration {
                drained = true;
                break;
            }

            drained = false;
        }

        Ok(PartitionProcessResult {
            drained,
            rows_claimed: total_claimed,
        })
    }
}

/// Internal result for a single partition's processing.
struct PartitionProcessResult {
    drained: bool,
    rows_claimed: u32,
}

/// Internal error type distinguishing skipped vs DB errors.
enum PartitionError {
    Skipped,
    Db(OutboxError),
}

impl From<sea_orm::DbErr> for PartitionError {
    fn from(e: sea_orm::DbErr) -> Self {
        Self::Db(OutboxError::Database(e))
    }
}

impl From<OutboxError> for PartitionError {
    fn from(e: OutboxError) -> Self {
        Self::Db(e)
    }
}

impl WorkerAction for Sequencer {
    type Payload = SequencerReport;
    type Error = OutboxError;

    async fn execute(
        &mut self,
        _cancel: &CancellationToken,
    ) -> Result<Directive<SequencerReport>, OutboxError> {
        let Some(guard) = self.shared_prioritizer.take() else {
            return Ok(Directive::Idle(SequencerReport {
                partition_id: -1,
                rows_claimed: 0,
            }));
        };

        let pid = guard.partition_id();
        match self.process_partition(pid).await {
            Ok(result) => {
                let report = SequencerReport {
                    partition_id: pid,
                    rows_claimed: result.rows_claimed,
                };
                guard.processed();

                // Re-dirty only when saturated (partition still has rows).
                // Drained partitions don't go back — new enqueues re-dirty them.
                if !result.drained {
                    self.shared_prioritizer.push_dirty(pid);
                }

                // Idle only when zero work was done (partition already stolen
                // or empty). Any work → Proceed, because other dirty partitions
                // may be queued in the SharedPrioritizer.
                if result.rows_claimed == 0 {
                    Ok(Directive::Idle(report))
                } else {
                    Ok(Directive::Proceed(report))
                }
            }
            Err(PartitionError::Skipped) => {
                guard.skipped();
                Ok(Directive::Idle(SequencerReport {
                    partition_id: pid,
                    rows_claimed: 0,
                }))
            }
            Err(PartitionError::Db(e)) => {
                let backend = self.db.sea_internal().get_database_backend();
                if matches!(&e, OutboxError::Database(db_err) if is_retryable_contention(backend, db_err))
                {
                    // Transient lock contention — safe to retry immediately.
                    tracing::debug!(
                        partition_id = pid,
                        "sequencer: lock contention, retrying partition"
                    );
                    guard.skipped();
                    self.shared_prioritizer.push_dirty(pid);
                    return Ok(Directive::Proceed(SequencerReport {
                        partition_id: pid,
                        rows_claimed: 0,
                    }));
                }
                warn!(partition_id = pid, error = %e, "sequencer partition error");
                guard.error();
                Ok(Directive::Proceed(SequencerReport {
                    partition_id: pid,
                    rows_claimed: 0,
                }))
            }
        }
    }
}

impl Sequencer {
    /// Try to acquire a row-level lock on the partition row.
    /// Returns `true` if the lock was acquired, `false` if skipped.
    async fn try_lock_partition(
        &self,
        txn: &impl ConnectionTrait,
        store: &OutboxStore<'_>,
        partition_id: i64,
        sql: &str,
    ) -> Result<bool, OutboxError> {
        let row = txn
            .query_one_raw(Statement::from_sql_and_values(
                store.backend(),
                sql,
                [partition_id.into()],
            ))
            .await?;
        Ok(row.is_some())
    }

    /// Claim incoming items for a single partition.
    ///
    /// Uses SELECT-then-DELETE on all backends to guarantee FIFO order:
    /// the SELECT returns rows ordered by `id`, and the caller assigns
    /// sequences in that iteration order.
    async fn claim_incoming_for_partition(
        &self,
        txn: &impl ConnectionTrait,
        store: &OutboxStore<'_>,
        partition_id: i64,
    ) -> Result<Vec<ClaimedIncoming>, OutboxError> {
        let claim = store.claim_incoming(self.config.batch_size);

        // SELECT id, body_id ... ORDER BY id
        let rows = ClaimedIncoming::find_by_statement(Statement::from_sql_and_values(
            store.backend(),
            &claim.select,
            [partition_id.into()],
        ))
        .all(txn)
        .await?;

        if rows.is_empty() {
            return Ok(rows);
        }

        // DELETE the selected rows by id
        let delete_sql = store.delete_incoming_batch(rows.len());
        let values: Vec<sea_orm::Value> = rows.iter().map(|r| r.id.into()).collect();
        txn.execute_raw(Statement::from_sql_and_values(
            store.backend(),
            &delete_sql,
            values,
        ))
        .await?;

        Ok(rows)
    }

    /// Atomically allocate sequence numbers for a partition.
    ///
    /// Returns the `start_seq` (items get `start_seq` + 1, `start_seq` + 2, etc.).
    async fn allocate_sequences(
        &self,
        txn: &impl ConnectionTrait,
        store: &OutboxStore<'_>,
        partition_id: i64,
        count: i64,
    ) -> Result<i64, OutboxError> {
        match store.allocate_sequences() {
            AllocSql::UpdateReturning(sql) => {
                // Pg/SQLite: one UPDATE ... RETURNING.
                let row = txn
                    .query_one_raw(Statement::from_sql_and_values(
                        store.backend(),
                        sql,
                        // Statement order: the delta, then the row.
                        [count.into(), partition_id.into()],
                    ))
                    .await?
                    .ok_or_else(|| {
                        OutboxError::Database(sea_orm::DbErr::Custom(
                            "UPDATE RETURNING returned no row for sequence allocation".to_owned(),
                        ))
                    })?;
                let start_seq: i64 = row.try_get_by_index(0).map_err(|e| {
                    OutboxError::Database(sea_orm::DbErr::Custom(format!("start_seq column: {e}")))
                })?;
                Ok(start_seq)
            }
            AllocSql::UpdateThenSelect { update, select } => {
                // MySQL: UPDATE, then SELECT the start of the range.
                txn.execute_raw(Statement::from_sql_and_values(
                    store.backend(),
                    update,
                    [count.into(), partition_id.into()],
                ))
                .await?;
                let row = txn
                    .query_one_raw(Statement::from_sql_and_values(
                        store.backend(),
                        select,
                        [count.into(), partition_id.into()],
                    ))
                    .await?
                    .ok_or_else(|| {
                        OutboxError::Database(sea_orm::DbErr::Custom(
                            "SELECT returned no row for sequence allocation".to_owned(),
                        ))
                    })?;
                let start_seq: i64 = row.try_get_by_index(0).map_err(|e| {
                    OutboxError::Database(sea_orm::DbErr::Custom(format!("start_seq column: {e}")))
                })?;
                Ok(start_seq)
            }
        }
    }
}
