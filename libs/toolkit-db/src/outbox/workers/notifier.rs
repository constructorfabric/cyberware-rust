//! Collects this instance's completion mail.
//!
//! Only ever needed when the instance that submitted a traced batch is not the
//! instance that finished it - when they are the same, the ack delivers the
//! completion itself and stamps it, so this worker finds nothing.
//!
//! It is gated on the subscription registry: an instance with nothing
//! outstanding has provably no mail worth looking for, so no query runs at
//! all - it sleeps until a subscription is taken.
//!
//! While somebody *is* waiting it schedules itself rather than taking a fixed
//! pace, because the two things it must not do pull in opposite directions: a
//! caller whose batch finishes should not wait a second to hear, and a caller
//! whose batch takes ten minutes should not be polled for at 100ms throughout.
//! So it starts tight and widens, and snaps back to tight the moment anything
//! is delivered.

use std::sync::Arc;
use std::time::Duration;

use sea_orm::{FromQueryResult, Statement};
use tokio_util::sync::CancellationToken;
use tracing::warn;

use super::super::core::Outbox;
use super::super::store::OutboxStore;
use super::super::taskward::{Directive, WorkerAction};
use super::super::types::OutboxError;
use crate::Db;

#[derive(Debug, FromQueryResult)]
struct MailRow {
    id: i64,
}

/// The tightest interval, used while a wait is fresh or has just produced
/// something.
const FIRST_LOOK: Duration = Duration::from_millis(100);

/// The widest. A batch that has not finished in this long will not be helped
/// by asking more often.
const WIDEST_LOOK: Duration = Duration::from_secs(2);

/// Delivers completions this instance owns but did not finish itself.
pub struct Notifier {
    pub outbox: Arc<Outbox>,
    pub db: Db,
    pub batch_size: u32,
    /// How long to wait before looking again while a caller is still waiting.
    pub next_look: Duration,
}

impl Notifier {
    /// Collect one page of mail. Returns how many completions were delivered.
    /// Returns how many completions were delivered, or the failure that
    /// stopped it. A failure must reach the worker loop: swallowing it would
    /// clear the backoff, so a database outage would be a failing query and a
    /// warning every 100ms with no escalation at all.
    async fn collect(&self) -> Result<u64, OutboxError> {
        let mailbox = self.outbox.mailbox();
        let store = OutboxStore::new(self.outbox.statements());
        let conn = self.db.sea_internal();
        let limit = i64::from(self.batch_size);
        let rows = match MailRow::find_by_statement(Statement::from_sql_and_values(
            store.backend(),
            store.trace_mail(),
            [mailbox.instance_id().into(), limit.into()],
        ))
        .all(&conn)
        .await
        {
            Ok(rows) => rows,
            Err(e) => {
                warn!(error = %e, "notifier: failed to read this instance's mail");
                return Err(e.into());
            }
        };

        let mut delivered = 0;
        for row in rows {
            // Claiming is what makes delivery at most once: the same guarded
            // UPDATE stamps the row and hands back what to deliver, so two
            // passes cannot both deliver the same completion. It commits
            // before anything is handed over, so a completion is never
            // announced from a claim that did not stick.
            match store
                .claim_trace_mail_alone(&conn, row.id, mailbox.instance_id())
                .await
            {
                Ok(Some(outcome)) => {
                    let trace = outcome.trace.clone();
                    if mailbox.subscriptions().deliver(outcome) {
                        delivered += 1;
                    } else {
                        // Nobody is waiting any more - the guard was dropped,
                        // or this process restarted since it subscribed. The
                        // claim already marked it delivered, so it is
                        // discarded rather than retained.
                        tracing::debug!(
                            trace = %trace,
                            "notifier: mail with nobody waiting, discarded"
                        );
                    }
                }
                // Someone else claimed it between the read and the claim.
                Ok(None) => {}
                Err(e) => {
                    warn!(error = %e, "notifier: failed to claim mail");
                    return Err(e.into());
                }
            }
        }
        Ok(delivered)
    }
}

impl WorkerAction for Notifier {
    type Payload = ();
    type Error = OutboxError;

    async fn execute(&mut self, _cancel: &CancellationToken) -> Result<Directive, Self::Error> {
        if self.outbox.mailbox().subscriptions().is_idle() {
            // Nothing outstanding: no query, no round trip, nothing. Sleep
            // until a subscription is taken, and be tight again when it is.
            self.next_look = FIRST_LOOK;
            return Ok(Directive::idle());
        }

        if self.collect().await? > 0 {
            // Mail was waiting, so more may be: look again at once, and stay
            // tight afterwards.
            self.next_look = FIRST_LOOK;
            return Ok(Directive::proceed());
        }

        // Somebody is still waiting and there was nothing yet. Widen, so a
        // long-running batch is not polled for at the arrival pace, and note
        // that `Sleep` wakes early if a new subscription arrives.
        let wait = self.next_look;
        self.next_look = self.next_look.saturating_mul(2).min(WIDEST_LOOK);
        Ok(Directive::sleep(wait))
    }
}
