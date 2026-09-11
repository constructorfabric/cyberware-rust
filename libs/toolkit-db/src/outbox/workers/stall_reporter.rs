//! Tells subscribers which of their batches are stuck.
//!
//! Separate from the notifier because it answers a different question with a
//! different query: the notifier delivers a finished batch's outcome once,
//! this reports an unfinished batch's situation for as long as it lasts. A
//! failure of either must not stop the other, and a completion should not wait
//! behind a stall report.
//!
//! Gated on somebody actually watching. A caller that only awaits completion
//! never takes a progress receiver, so an instance whose callers all do that
//! issues no query here at all.

use std::collections::HashSet;
use std::sync::Arc;

use sea_orm::{FromQueryResult, Statement};
use tokio_util::sync::CancellationToken;
use tracing::warn;

use super::super::core::Outbox;
use super::super::store::OutboxStore;
use super::super::taskward::{Directive, WorkerAction};
use super::super::trace::TraceProgress;
use super::super::types::OutboxError;
use crate::Db;

#[derive(Debug, FromQueryResult)]
struct StallRow {
    trace: String,
    entities: i64,
    pending: i64,
    failures: i64,
    attempts: i64,
    last_error: Option<String>,
    stalled_since: chrono::DateTime<chrono::Utc>,
}

impl From<StallRow> for TraceProgress {
    fn from(row: StallRow) -> Self {
        Self {
            trace: row.trace,
            entities: row.entities,
            pending: row.pending,
            failures: row.failures,
            attempts: row.attempts,
            last_error: row.last_error,
            stalled_since: row.stalled_since,
        }
    }
}

/// Pushes the stalls of this instance's traces to whoever is watching them.
pub struct StallReporter {
    pub outbox: Arc<Outbox>,
    pub db: Db,
    pub batch_size: u32,
}

impl WorkerAction for StallReporter {
    type Payload = ();
    type Error = OutboxError;

    async fn execute(&mut self, _cancel: &CancellationToken) -> Result<Directive, Self::Error> {
        let mailbox = self.outbox.mailbox();
        if !mailbox.subscriptions().wants_progress() {
            // Nobody is watching, so there is nothing a query could tell us.
            return Ok(Directive::idle());
        }

        let store = OutboxStore::new(self.outbox.statements());
        let conn = self.db.sea_internal();
        let limit = i64::from(self.batch_size);
        let rows = match StallRow::find_by_statement(Statement::from_sql_and_values(
            store.backend(),
            store.trace_stalled(),
            [mailbox.instance_id().into(), limit.into()],
        ))
        .all(&conn)
        .await
        {
            Ok(rows) => rows,
            Err(e) => {
                warn!(error = %e, "stall reporter: failed to read this instance's stalled traces");
                return Err(e.into());
            }
        };

        // A full page proves nothing about the traces it did not reach, so a
        // stall is only withdrawn when the page held every stalled trace there
        // was. Reporting a stall a little too long is harmless; clearing one
        // that is still stuck would be a lie.
        let complete_picture = rows.len() < usize::try_from(limit).unwrap_or(usize::MAX);
        let mut stalled = HashSet::with_capacity(rows.len());
        for row in rows {
            stalled.insert(row.trace.clone());
            mailbox.subscriptions().publish_progress(row.into());
        }
        if complete_picture {
            mailbox.subscriptions().withdraw_progress_except(&stalled);
        }

        // A stall is a state, not a backlog: there is never "more" of it to
        // fetch, so the next look belongs to the timer.
        Ok(Directive::idle())
    }
}
