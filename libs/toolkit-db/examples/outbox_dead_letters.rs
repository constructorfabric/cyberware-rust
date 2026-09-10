#![allow(clippy::unwrap_used, clippy::expect_used, clippy::use_debug)]

//! Dead letter lifecycle: reject -> list -> replay -> resolve -> cleanup.
//!
//! Run:
//!   cargo run -p cf-gears-toolkit-db --example `outbox_dead_letters` --features sqlite

use std::time::Duration;

use toolkit_db::outbox::{
    DeadLetterFilter, DeadLetterScope, LeasedMessageHandler, MessageResult, Outbox, OutboxMessage,
    Partitions, Record, WorkerTuning, outbox_migrations,
};
use toolkit_db::{ConnectOpts, connect_db, migration_runner::run_migrations_for_testing};

struct RejectAll;

#[async_trait::async_trait]
impl LeasedMessageHandler for RejectAll {
    async fn handle(&self, _msg: &OutboxMessage) -> MessageResult {
        MessageResult::Reject("bad format".into())
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let db = connect_db(
        "sqlite:file:outbox_dl?mode=memory&cache=shared",
        ConnectOpts {
            max_conns: Some(1),
            ..Default::default()
        },
    )
    .await?;
    run_migrations_for_testing(&db, outbox_migrations()).await?;

    // Reject 3 messages to populate dead letters
    let h1 = Outbox::builder(db.clone())
        .processor_tuning(
            WorkerTuning::processor_default().idle_interval(Duration::from_millis(50)),
        )
        .sequencer_tuning(
            WorkerTuning::sequencer_default().idle_interval(Duration::from_millis(50)),
        )
        .queue("events", Partitions::of(1))
        .leased(RejectAll)
        .done()
        .start()
        .await?;
    let conn = db.conn()?;
    for i in 0..3 {
        h1.outbox()
            .enqueue(
                &conn,
                Record::to("events", 0)
                    .payload(
                        format!("evt-{i}").into_bytes(),
                        "text/plain;events.logged.v1",
                    )
                    .build()?,
            )
            .await?;
    }
    h1.outbox().flush();
    tokio::time::sleep(Duration::from_secs(1)).await;
    let outbox = std::sync::Arc::clone(h1.outbox());
    h1.stop().await;

    // List
    let items = outbox
        .dead_letter_list(&db.conn()?, &DeadLetterFilter::default())
        .await?;
    println!("Dead letters: {}", items.len());
    for dl in &items {
        println!(
            "  seq={} error={}",
            dl.seq,
            dl.last_error.as_deref().unwrap_or("?")
        );
    }

    // Replay 1 entry — claims it for reprocessing
    let replayed = outbox
        .dead_letter_replay(
            &db.conn()?,
            &DeadLetterScope::default().limit(1),
            Duration::from_mins(1),
        )
        .await?;
    println!("Replayed: {}", replayed.len());

    // Resolve the replayed messages (mark as successfully handled)
    let ids: Vec<i64> = replayed.iter().map(|m| m.id).collect();
    let resolved = outbox.dead_letter_resolve(&db.conn()?, &ids).await?;
    println!("Resolved: {resolved}");

    // Discard remaining pending dead letters
    let discarded = outbox
        .dead_letter_discard(&db.conn()?, &DeadLetterScope::default())
        .await?;
    println!("Discarded: {discarded}");

    // Cleanup terminal-state entries (resolved + discarded)
    let cleaned = outbox
        .dead_letter_cleanup(&db.conn()?, &DeadLetterScope::default())
        .await?;
    println!("Cleaned up: {cleaned}");

    let remaining = outbox
        .dead_letter_count(&db.conn()?, &DeadLetterFilter::default())
        .await?;
    println!("Remaining: {remaining}");
    assert_eq!(remaining, 0);

    println!("Done.");
    Ok(())
}
