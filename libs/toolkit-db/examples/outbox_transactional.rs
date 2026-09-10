#![allow(clippy::unwrap_used, clippy::expect_used, clippy::use_debug)]

//! Exactly-once message processing with a transactional handler.
//!
//! The handler runs inside the DB transaction that holds the partition lock,
//! so handler side-effects and cursor advance commit atomically.
//!
//! Run:
//!   cargo run -p cf-gears-toolkit-db --example `outbox_transactional` --features sqlite

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use sea_orm::DatabaseExecutor;
use toolkit_db::outbox::{
    HandlerResult, Outbox, OutboxMessage, Partitions, Record, TransactionalMessageHandler,
    WorkerTuning, outbox_migrations,
};
use toolkit_db::{ConnectOpts, connect_db, migration_runner::run_migrations_for_testing};

struct Processor {
    count: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl TransactionalMessageHandler for Processor {
    async fn handle(&self, _txn: &DatabaseExecutor<'_>, msg: &OutboxMessage) -> HandlerResult {
        let payload = String::from_utf8_lossy(&msg.payload);
        println!("  processed seq={} payload={payload}", msg.seq);
        self.count.fetch_add(1, Ordering::Relaxed);
        HandlerResult::Success
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // shared-cache so all pool connections see the same in-memory database;
    // max_conns=1 avoids SQLite locking contention in examples
    let db = connect_db(
        "sqlite:file:outbox_tx?mode=memory&cache=shared",
        ConnectOpts {
            max_conns: Some(1),
            ..Default::default()
        },
    )
    .await?;
    run_migrations_for_testing(&db, outbox_migrations()).await?;

    let count = Arc::new(AtomicUsize::new(0));

    let handle = Outbox::builder(db.clone())
        .processor_tuning(
            WorkerTuning::processor_default().idle_interval(Duration::from_millis(50)),
        )
        .sequencer_tuning(
            WorkerTuning::sequencer_default().idle_interval(Duration::from_millis(50)),
        )
        // 2 partitions — messages are spread across them for parallelism
        .queue("orders", Partitions::of(2))
        .transactional(Processor {
            count: count.clone(),
        })
        .start()
        .await?;

    // transaction() auto-flushes the sequencer on commit — no manual flush() needed
    let outbox = Arc::clone(handle.outbox());
    let (db, result) = outbox
        .transaction(db, |tx| {
            let outbox = Arc::clone(&outbox);
            Box::pin(async move {
                for i in 0..5u32 {
                    let payload = format!(r#"{{"order_id": {i}}}"#);
                    outbox
                        // payload_type is user-defined — convention: mime base + vendor domain type
                        .enqueue(
                            tx,
                            Record::to("orders", i % 2)
                                .payload(payload.into_bytes(), "application/json;orders.created.v1")
                                .build()?,
                        )
                        .await
                        .map_err(|e| anyhow::anyhow!("{e}"))?;
                }
                Ok(())
            })
        })
        .await;
    result?;
    println!("Enqueued 5 messages across 2 partitions");

    // Poll until all messages are processed (processor runs in background)
    for _ in 0..100 {
        if count.load(Ordering::Relaxed) >= 5 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let processed = count.load(Ordering::Relaxed);
    println!("Processed: {processed}/5");
    assert_eq!(processed, 5);

    handle.stop().await;
    println!("Done.");

    drop(db);
    Ok(())
}
