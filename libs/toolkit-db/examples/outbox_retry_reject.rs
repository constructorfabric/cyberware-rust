#![allow(clippy::unwrap_used, clippy::expect_used, clippy::use_debug)]

//! Retry with exponential backoff, then dead-letter on permanent failure.
//!
//! The handler retries twice (transient failure), then rejects on the 3rd attempt.
//!
//! Run:
//!   cargo run -p cf-gears-toolkit-db --example `outbox_retry_reject` --features sqlite

use std::time::{Duration, Instant};

use toolkit_db::outbox::{
    DeadLetterFilter, LeasedMessageHandler, MessageResult, Outbox, OutboxMessage, Partitions,
    Record, WorkerTuning, outbox_migrations,
};
use toolkit_db::{ConnectOpts, connect_db, migration_runner::run_migrations_for_testing};

struct RetryThenReject {
    start: Instant,
}

#[async_trait::async_trait]
impl LeasedMessageHandler for RetryThenReject {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let elapsed = self.start.elapsed();
        if msg.attempts < 2 {
            println!("  attempt={} at {elapsed:.0?} -> Retry", msg.attempts);
            MessageResult::Retry
        } else {
            println!(
                "  attempt={} at {elapsed:.0?} -> Reject (giving up)",
                msg.attempts
            );
            MessageResult::Reject(format!(
                "permanent failure after {} attempts",
                msg.attempts + 1
            ))
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let db = connect_db(
        "sqlite:file:outbox_retry?mode=memory&cache=shared",
        ConnectOpts {
            max_conns: Some(1),
            ..Default::default()
        },
    )
    .await?;
    run_migrations_for_testing(&db, outbox_migrations()).await?;

    let handle = Outbox::builder(db.clone())
        .processor_tuning(
            WorkerTuning::processor_default().idle_interval(Duration::from_millis(50)),
        )
        .sequencer_tuning(
            WorkerTuning::sequencer_default().idle_interval(Duration::from_millis(50)),
        )
        .queue("events", Partitions::of(1))
        .leased(RetryThenReject {
            start: Instant::now(),
        })
        .done()
        .start()
        .await?;

    let conn = db.conn()?;
    handle
        .outbox()
        .enqueue(
            &conn,
            Record::to("events", 0)
                .payload(
                    b"webhook-payload".to_vec(),
                    "application/octet-stream;webhooks.delivery.v1",
                )
                .build()?,
        )
        .await?;
    handle.outbox().flush();
    println!("Enqueued 1 message, watching retries:");

    // wait for retries + final reject
    tokio::time::sleep(Duration::from_secs(3)).await;

    let items = handle
        .outbox()
        .dead_letter_list(&db.conn()?, &DeadLetterFilter::default())
        .await?;
    for dl in &items {
        println!(
            "Dead letter: seq={} attempts={} reason={}",
            dl.seq,
            dl.attempts,
            dl.last_error.as_deref().unwrap_or("?")
        );
    }
    assert_eq!(items.len(), 1);

    handle.stop().await;
    println!("Done.");
    Ok(())
}
