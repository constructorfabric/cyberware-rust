//! Custom table-prefix outbox example.
//!
//! Run:
//!   cargo run -p cf-gears-toolkit-db --example `outbox_custom_prefix` --features sqlite

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use toolkit_db::outbox::{
    LeasedMessageHandler, MessageResult, Outbox, OutboxMessage, Partitions, Record, WorkerTuning,
    outbox_migrations_with_prefix,
};
use toolkit_db::{ConnectOpts, connect_db, migration_runner::run_migrations_for_testing};

struct PrintHandler {
    processed: Arc<AtomicU32>,
}

#[async_trait::async_trait]
impl LeasedMessageHandler for PrintHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        println!(
            "processed partition={} seq={} payload={}",
            msg.partition_id,
            msg.seq,
            String::from_utf8_lossy(&msg.payload)
        );
        self.processed.fetch_add(1, Ordering::Relaxed);
        MessageResult::Ok
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let prefix = "mini_chat_outbox";
    let db = connect_db(
        "sqlite:file:outbox_custom_prefix?mode=memory&cache=shared",
        ConnectOpts {
            max_conns: Some(1),
            ..Default::default()
        },
    )
    .await?;

    run_migrations_for_testing(&db, outbox_migrations_with_prefix(prefix)?).await?;

    let processed = Arc::new(AtomicU32::new(0));
    let handle = Outbox::builder(db.clone())
        .table_prefix(prefix)?
        .processor_tuning(
            WorkerTuning::processor_default().idle_interval(Duration::from_millis(50)),
        )
        .sequencer_tuning(
            WorkerTuning::sequencer_default().idle_interval(Duration::from_millis(50)),
        )
        .queue("messages", Partitions::of(1))
        .leased(PrintHandler {
            processed: Arc::clone(&processed),
        })
        .start()
        .await?;

    handle
        .outbox()
        .enqueue(
            &db.conn()?,
            Record::to("messages", 0)
                .payload(b"hello from a prefixed outbox".to_vec(), "text/plain")
                .build()?,
        )
        .await?;
    handle.outbox().flush();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while processed.load(Ordering::Relaxed) == 0 {
        if tokio::time::Instant::now() >= deadline {
            anyhow::bail!("message was not processed before timeout");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    handle.stop().await;
    Ok(())
}
