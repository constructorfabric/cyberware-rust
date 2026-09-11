//! Collects finished trace rows, as its own task.
//!
//! It does one thing, on its own clock. Two events can make a trace
//! collectable: time passing, which no other task knows about, and the vacuum
//! deleting the bodies that were keeping it alive, which the vacuum signals.
//! Running inside the vacuum instead would have tied trace retention to
//! whatever pace the body collector happened to be set to.

use std::sync::Arc;

use sea_orm::{ConnectionTrait, Statement};
use tokio_util::sync::CancellationToken;
use tracing::debug;

use super::super::statements::OutboxStatements;
use super::super::store::OutboxStore;
use super::super::taskward::{Directive, WorkerAction};
use super::super::types::OutboxError;
use crate::Db;

/// How long each kind of finished trace row is kept.
#[derive(Debug, Clone, Copy)]
pub struct TraceSweep {
    /// A delivered trace.
    pub retention: std::time::Duration,
    /// A completed trace whose owner never collected it.
    pub orphan_after: std::time::Duration,
    /// A trace that never completed, or one whose delivery produced dead
    /// letters - those outlive the delivery they failed.
    pub leftover_after: std::time::Duration,
}

impl Default for TraceSweep {
    /// The periods the sweep keeps unless a caller says otherwise.
    ///
    /// They live here and not on `WorkerTuning`, which every worker shares:
    /// only this one reads them, and putting them there meant thirteen
    /// constructors each restating three durations that meant nothing to them.
    fn default() -> Self {
        Self {
            retention: std::time::Duration::from_hours(24),
            orphan_after: std::time::Duration::from_hours(24),
            leftover_after: std::time::Duration::from_hours(24 * 7),
        }
    }
}

/// Deletes trace rows that are finished with.
pub struct TraceSweeper {
    pub db: Db,
    pub statements: Arc<OutboxStatements>,
    pub batch_size: usize,
    pub periods: TraceSweep,
}

impl TraceSweeper {
    /// Collect one page.
    ///
    /// Four reasons to collect a trace: delivered and past `retention`;
    /// completed but never collected, past `orphan_after`, so its owner is
    /// evidently gone; delivered with failures, past `leftover_after`, because
    /// a dead letter outlives the delivery it failed and its trace has to
    /// outlive the dead letter; and never completed at all, past
    /// `leftover_after`.
    ///
    /// Liveness comes from the trace row's own fields, so the decision reads
    /// one table. A trace still working has `pending > 0` and is held by the
    /// last rule until it has made no progress for `leftover_after`; the
    /// messages themselves are never touched by this sweep.
    pub(super) async fn sweep(&self) -> Result<usize, OutboxError> {
        let store = OutboxStore::new(&self.statements);
        let conn = self.db.sea_internal();

        // Ages in seconds; the cutoff arithmetic happens in SQL so it compares
        // against the backend's own timestamp format.
        let secs = |after: std::time::Duration| i64::try_from(after.as_secs()).unwrap_or(i64::MAX);
        let limit = i64::try_from(self.batch_size).unwrap_or(i64::MAX);

        let rows = conn
            .query_all_raw(Statement::from_sql_and_values(
                store.backend(),
                store.select_collectable_traces(),
                // Statement order. `leftover_after` appears twice: once for a
                // delivered trace that produced dead letters, which outlive
                // the delivery they failed, and once for a trace that never
                // completed at all.
                [
                    secs(self.periods.retention).into(),
                    secs(self.periods.orphan_after).into(),
                    secs(self.periods.leftover_after).into(),
                    secs(self.periods.leftover_after).into(),
                    limit.into(),
                ],
            ))
            .await?;

        if rows.is_empty() {
            return Ok(0);
        }

        let mut ids: Vec<sea_orm::Value> = Vec::with_capacity(rows.len());
        for row in &rows {
            let id: i64 = row.try_get_by_index(0).map_err(|e| {
                OutboxError::Database(sea_orm::DbErr::Custom(format!("trace id column: {e}")))
            })?;
            ids.push(id.into());
        }

        let count = ids.len();
        let sql = store.build_delete_traces(count);
        conn.execute_raw(Statement::from_sql_and_values(store.backend(), &sql, ids))
            .await?;
        debug!(count, "collected finished traces");
        Ok(count)
    }
}

impl WorkerAction for TraceSweeper {
    type Payload = ();
    type Error = OutboxError;

    async fn execute(&mut self, _cancel: &CancellationToken) -> Result<Directive, Self::Error> {
        match self.sweep().await {
            // A full page suggests more is waiting, so keep going rather than
            // waiting out the interval.
            Ok(count) if count >= self.batch_size => Ok(Directive::proceed()),
            Ok(_) => Ok(Directive::idle()),
            // Surfaced rather than swallowed: the worker loop clears the
            // backoff on a success and escalates on a failure, so returning
            // `Ok` here would make an outage a warning every second forever.
            Err(e) => Err(e),
        }
    }
}
