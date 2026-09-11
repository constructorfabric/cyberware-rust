#![allow(clippy::unwrap_used, clippy::expect_used, clippy::use_debug)]

//! Batch processing with multiple queues in a single pipeline.
//!
//! "orders" uses `leased` with a batch handler (handler receives messages in batches).
//! "notifications" uses leased with a single-message handler.
//! Both queues process independently within the same `OutboxBuilder`.
//!
//! Run:
//!   cargo run -p cf-gears-toolkit-db --example `outbox_batch_multi_queue` --features sqlite

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use toolkit_db::outbox::{
    Batch, HandlerResult, LeasedHandler, LeasedMessageHandler, MessageResult, Outbox,
    OutboxMessage, Partitions, Record, WorkerTuning, outbox_migrations,
};
use toolkit_db::{ConnectOpts, connect_db, migration_runner::run_migrations_for_testing};

struct OrderBatchHandler {
    count: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl LeasedHandler for OrderBatchHandler {
    async fn handle(&self, batch: &mut Batch<'_>) -> HandlerResult {
        // batch handler receives multiple messages per call
        let mut n = 0usize;
        while let Some(_msg) = batch.next_msg() {
            self.count.fetch_add(1, Ordering::Relaxed);
            batch.ack();
            n += 1;
        }
        println!("  orders: batch of {n} messages");
        HandlerResult::Success
    }
}

struct NotificationHandler {
    count: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl LeasedMessageHandler for NotificationHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let payload = String::from_utf8_lossy(&msg.payload);
        println!("  notifs: seq={} payload={payload}", msg.seq);
        self.count.fetch_add(1, Ordering::Relaxed);
        MessageResult::Ok
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let db = connect_db(
        "sqlite:file:outbox_batch?mode=memory&cache=shared",
        ConnectOpts {
            max_conns: Some(1),
            ..Default::default()
        },
    )
    .await?;
    run_migrations_for_testing(&db, outbox_migrations()).await?;

    let order_count = Arc::new(AtomicUsize::new(0));
    let notif_count = Arc::new(AtomicUsize::new(0));

    let handle = Outbox::builder(db.clone())
        .processor_tuning(
            WorkerTuning::processor_default().idle_interval(Duration::from_millis(50)),
        )
        .sequencer_tuning(
            WorkerTuning::sequencer_default().idle_interval(Duration::from_millis(50)),
        )
        // orders: 2 partitions for parallelism, batch handler processes messages in batches
        .queue("orders", Partitions::of(2))
        .leased(OrderBatchHandler {
            count: order_count.clone(),
        })
        .done()
        // notifications: single partition, single-message handler
        .queue("notifications", Partitions::of(1))
        .leased(NotificationHandler {
            count: notif_count.clone(),
        })
        .done()
        .start()
        .await?;

    let conn = db.conn()?;
    for i in 0..8u32 {
        let payload = format!(r#"{{"order_id": {i}}}"#);
        handle
            .outbox()
            .enqueue(
                &conn,
                Record::to("orders", i % 2)
                    .payload(payload.into_bytes(), "application/json;orders.placed.v1")
                    .build()?,
            )
            .await?;
    }
    for i in 0..3u32 {
        let payload = format!("user_{i}_welcome");
        handle
            .outbox()
            .enqueue(
                &conn,
                Record::to("notifications", 0)
                    .payload(payload.into_bytes(), "text/plain;notifications.welcome.v1")
                    .build()?,
            )
            .await?;
    }
    handle.outbox().flush();
    println!("Enqueued 8 orders + 3 notifications");

    for _ in 0..100 {
        let done = order_count.load(Ordering::Relaxed) + notif_count.load(Ordering::Relaxed);
        if done >= 11 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let orders = order_count.load(Ordering::Relaxed);
    let notifs = notif_count.load(Ordering::Relaxed);
    println!("Orders processed: {orders}/8");
    println!("Notifications processed: {notifs}/3");
    assert_eq!(orders, 8);
    assert_eq!(notifs, 3);

    handle.stop().await;
    println!("Done.");
    Ok(())
}
