#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Integration tests for the transactional outbox subsystem.
//!
//! Organized as narrative chapters that trace complete lifecycle paths.
//! Uses `SQLite` in-memory databases for fast, hermetic testing.
//!
//! Chapter ordering mirrors the pipeline:
//!   1. Registration  →  2. Record  →  3. Sequencer
//!   4. Transactional Processing  →  5. Decoupled Processing
//!   6. Crash Detection & Recovery  →  7. Backoff & Adaptive Batching
//!   8. Vacuum  →  9. Dead Letters  →  10. Builder API
//!   11. End-to-End Lifecycle

use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sea_orm::{ConnectionTrait, DbBackend, FromQueryResult, Statement};
use sea_orm_migration::prelude::MigrationTrait;
use tokio_util::sync::CancellationToken;

use super::batch::Batch;
use super::dead_letter::{DeadLetterFilter, DeadLetterScope};
use super::handler::{
    HandlerResult, LeasedHandler, LeasedMessageHandler, MessageResult, OutboxMessage,
    PerMessageAdapter, TransactionalHandler, TransactionalMessageHandler,
};
use super::prioritizer::SharedPrioritizer;
use super::record::{Record, Records};
use super::store::OutboxStore;
use super::strategy::{LeasedStrategy, ProcessContext, ProcessingStrategy, TransactionalStrategy};
use super::tables::OutboxTables;
use super::taskward::{Directive, WorkerAction};
use super::types::{LeaseConfig, OutboxConfig, SequencerConfig, WorkerTuning};
use super::workers::sequencer::Sequencer;
use super::{Outbox, OutboxError, Partitions};
use crate::migration_runner::run_migrations_for_testing;
use crate::outbox::OutboxMessageId;
use crate::{ConnectOpts, Db, connect_db};

// ======================================================================
// Snapshot structs
// ======================================================================

struct TestOutbox {
    outbox: Arc<Outbox>,
    prioritizer: Arc<SharedPrioritizer>,
}

#[derive(Debug)]
struct ProcessorSnapshot {
    processed_seq: i64,
    attempts: i16,
    last_error: Option<String>,
    locked_by: Option<String>,
    locked_until: Option<String>,
}

#[derive(Debug)]
struct OutgoingSnapshot {
    id: i64,
    partition_id: i64,
    body_id: i64,
    seq: i64,
}

#[derive(Debug)]
struct DeadLetterSnapshot {
    id: i64,
    partition_id: i64,
    seq: i64,
    payload: Vec<u8>,
    payload_type: String,
    last_error: Option<String>,
    attempts: i16,
    status: String,
    completed_at: Option<String>,
    deadline: Option<String>,
}

// ======================================================================
// Layer A — Infrastructure (create resources)
// ======================================================================

async fn setup_db(name: &str) -> Db {
    setup_db_with_migrations(name, super::outbox_migrations()).await
}

async fn setup_db_with_migrations(name: &str, migrations: Vec<Box<dyn MigrationTrait>>) -> Db {
    let db = setup_empty_db(name).await;
    run_migrations_for_testing(&db, migrations)
        .await
        .expect("migrations");
    db
}

async fn setup_empty_db(name: &str) -> Db {
    setup_empty_db_with_pool(name, 1).await
}

/// A pool wider than one connection, so concurrent writers actually reach the
/// database in parallel rather than queueing in the pool.
async fn setup_empty_db_with_pool(name: &str, max_conns: u32) -> Db {
    let url = format!("sqlite:file:{name}?mode=memory&cache=shared");
    let opts = ConnectOpts {
        max_conns: Some(max_conns),
        ..Default::default()
    };
    connect_db(&url, opts).await.expect("connect")
}

// Must be async: `prioritizer` is a tokio::sync::RwLock and `blocking_write()`
// panics when called from within a tokio runtime (i.e. every `#[tokio::test]`).
async fn make_test_outbox(config: OutboxConfig) -> TestOutbox {
    let prioritizer = Arc::new(SharedPrioritizer::new());
    let outbox = Arc::new(Outbox::new(config));
    outbox
        .prioritizer
        .write()
        .await
        .replace(Arc::clone(&prioritizer));
    TestOutbox {
        outbox,
        prioritizer,
    }
}

async fn make_default_test_outbox() -> TestOutbox {
    make_test_outbox(OutboxConfig::default()).await
}

/// A trace sweeper with the sweep's three periods supplied by the caller, so
/// each rule can be exercised without waiting a day.
fn test_trace_sweeper(
    db: &Db,
    periods: super::workers::trace_sweeper::TraceSweep,
) -> super::workers::trace_sweeper::TraceSweeper {
    super::workers::trace_sweeper::TraceSweeper {
        db: db.clone(),
        statements: Arc::new(super::statements::OutboxStatements::new(
            db.sea_internal().get_database_backend(),
            &OutboxTables::default(),
        )),
        batch_size: 10_000,
        periods,
    }
}

/// The vacuum's downstream channel: the nudge that traces may be collectable.
fn test_collectable_traces() -> Arc<tokio::sync::Notify> {
    Arc::new(tokio::sync::Notify::new())
}

fn make_shared_prioritizer() -> Arc<SharedPrioritizer> {
    Arc::new(SharedPrioritizer::new())
}

fn make_sequencer(t: &TestOutbox, config: SequencerConfig, db: &Db) -> Sequencer {
    Sequencer::new(
        config,
        Arc::clone(&t.outbox),
        db.clone(),
        Arc::clone(&t.prioritizer),
    )
}

// ======================================================================
// Layer B — Actions (do things)
// ======================================================================

async fn enqueue_msgs(
    outbox: &Outbox,
    db: &Db,
    queue: &str,
    partition: u32,
    payloads: &[&str],
) -> Vec<OutboxMessageId> {
    let conn = db.conn().expect("conn");
    let mut ids = Vec::with_capacity(payloads.len());
    for payload in payloads {
        let id = outbox
            .enqueue(
                &conn,
                Record::to(queue, partition)
                    .payload(payload.as_bytes().to_vec(), "text/plain")
                    .build()
                    .unwrap(),
            )
            .await
            .expect("enqueue");
        ids.push(id);
    }
    ids
}

/// Run sequencer until truly idle (no work done).
async fn run_sequencer_until_idle(seq: &mut Sequencer) {
    let cancel = CancellationToken::new();
    while let Directive::Proceed(_) = seq.execute(&cancel).await.unwrap() {}
}

async fn run_sequencer_once(t: &TestOutbox, db: &Db) {
    let mut seq = make_sequencer(t, SequencerConfig::default(), db);
    run_sequencer_until_idle(&mut seq).await;
}

async fn enqueue_and_sequence(
    t: &TestOutbox,
    db: &Db,
    queue: &str,
    partition: u32,
    payloads: &[&str],
) -> Vec<OutboxMessageId> {
    let ids = enqueue_msgs(&t.outbox, db, queue, partition, payloads).await;
    run_sequencer_once(t, db).await;
    ids
}

async fn simulate_crash(db: &Db, partition_id: i64, lease_secs: i64) {
    simulate_crash_for_tables(db, &OutboxTables::default(), partition_id, lease_secs).await;
}

async fn simulate_crash_for_tables(
    db: &Db,
    tables: &OutboxTables,
    partition_id: i64,
    lease_secs: i64,
) {
    let conn = db.sea_internal();
    conn.execute_raw(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        format!(
            "UPDATE {} \
         SET locked_by = $1, \
             locked_until = datetime('now', '+' || $2 || ' seconds'), \
             attempts = attempts + 1 \
         WHERE partition_id = $3",
            tables.processor()
        ),
        ["crashed-pod".into(), lease_secs.into(), partition_id.into()],
    ))
    .await
    .expect("simulate_crash");
}

async fn expire_lease(db: &Db, partition_id: i64) {
    expire_lease_for_tables(db, &OutboxTables::default(), partition_id).await;
}

async fn expire_lease_for_tables(db: &Db, tables: &OutboxTables, partition_id: i64) {
    let conn = db.sea_internal();
    conn.execute_raw(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        format!(
            "UPDATE {} \
         SET locked_until = datetime('now', '-1 seconds') \
         WHERE partition_id = $1",
            tables.processor()
        ),
        [partition_id.into()],
    ))
    .await
    .expect("expire_lease");
}

// ======================================================================
// Layer C — Observations (read state only)
// ======================================================================

async fn count_rows(db: &Db, table: &str) -> i64 {
    #[derive(Debug, FromQueryResult)]
    struct Count {
        cnt: i64,
    }
    let conn = db.sea_internal();
    Count::find_by_statement(Statement::from_string(
        DbBackend::Sqlite,
        format!("SELECT COUNT(*) AS cnt FROM {table}"),
    ))
    .one(&conn)
    .await
    .expect("count query")
    .expect("count row")
    .cnt
}

async fn table_exists(db: &Db, table: &str) -> bool {
    #[derive(Debug, FromQueryResult)]
    struct Exists {
        cnt: i64,
    }
    let conn = db.sea_internal();
    Exists::find_by_statement(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        "SELECT COUNT(*) AS cnt FROM sqlite_master WHERE type = 'table' AND name = $1",
        [table.into()],
    ))
    .one(&conn)
    .await
    .expect("table-exists query")
    .expect("table-exists row")
    .cnt == 1
}

async fn assert_table_family_exists(db: &Db, tables: &OutboxTables) {
    for table in tables.table_names() {
        assert!(table_exists(db, table).await, "expected table {table}");
    }
}

async fn read_processor_state(db: &Db, partition_id: i64) -> ProcessorSnapshot {
    read_processor_state_for_tables(db, &OutboxTables::default(), partition_id).await
}

async fn read_processor_state_for_tables(
    db: &Db,
    tables: &OutboxTables,
    partition_id: i64,
) -> ProcessorSnapshot {
    #[derive(Debug, FromQueryResult)]
    struct Row {
        processed_seq: i64,
        attempts: i16,
        last_error: Option<String>,
        locked_by: Option<String>,
        locked_until: Option<String>,
    }
    let conn = db.sea_internal();
    let row = Row::find_by_statement(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        format!(
            "SELECT processed_seq, attempts, last_error, locked_by, \
         CAST(locked_until AS TEXT) AS locked_until \
         FROM {} WHERE partition_id = $1",
            tables.processor()
        ),
        [partition_id.into()],
    ))
    .one(&conn)
    .await
    .expect("query")
    .expect("processor row");
    ProcessorSnapshot {
        processed_seq: row.processed_seq,
        attempts: row.attempts,
        last_error: row.last_error,
        locked_by: row.locked_by,
        locked_until: row.locked_until,
    }
}

async fn read_outgoing(db: &Db, partition_id: i64) -> Vec<OutgoingSnapshot> {
    read_outgoing_for_tables(db, &OutboxTables::default(), partition_id).await
}

async fn read_outgoing_for_tables(
    db: &Db,
    tables: &OutboxTables,
    partition_id: i64,
) -> Vec<OutgoingSnapshot> {
    #[derive(Debug, FromQueryResult)]
    struct Row {
        id: i64,
        partition_id: i64,
        body_id: i64,
        seq: i64,
    }
    let conn = db.sea_internal();
    Row::find_by_statement(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        format!(
            "SELECT id, partition_id, body_id, seq \
         FROM {} WHERE partition_id = $1 ORDER BY seq",
            tables.outgoing()
        ),
        [partition_id.into()],
    ))
    .all(&conn)
    .await
    .expect("query")
    .into_iter()
    .map(|r| OutgoingSnapshot {
        id: r.id,
        partition_id: r.partition_id,
        body_id: r.body_id,
        seq: r.seq,
    })
    .collect()
}

async fn read_dead_letters(db: &Db) -> Vec<DeadLetterSnapshot> {
    read_dead_letters_for_tables(db, &OutboxTables::default()).await
}

async fn read_dead_letters_for_tables(db: &Db, tables: &OutboxTables) -> Vec<DeadLetterSnapshot> {
    #[derive(Debug, FromQueryResult)]
    struct Row {
        id: i64,
        partition_id: i64,
        seq: i64,
        payload: Vec<u8>,
        payload_type: String,
        last_error: Option<String>,
        attempts: i16,
        status: String,
        completed_at: Option<String>,
        deadline: Option<String>,
    }
    let conn = db.sea_internal();
    Row::find_by_statement(Statement::from_string(
        DbBackend::Sqlite,
        format!(
            "SELECT id, partition_id, seq, payload, payload_type, last_error, \
         attempts, status, CAST(completed_at AS TEXT) AS completed_at, \
         CAST(deadline AS TEXT) AS deadline \
         FROM {} ORDER BY seq",
            tables.dead_letters()
        ),
    ))
    .all(&conn)
    .await
    .expect("query")
    .into_iter()
    .map(|r| DeadLetterSnapshot {
        id: r.id,
        partition_id: r.partition_id,
        seq: r.seq,
        payload: r.payload,
        payload_type: r.payload_type,
        last_error: r.last_error,
        attempts: r.attempts,
        status: r.status,
        completed_at: r.completed_at,
        deadline: r.deadline,
    })
    .collect()
}

async fn read_partition_sequence(db: &Db, partition_id: i64) -> i64 {
    read_partition_sequence_for_tables(db, &OutboxTables::default(), partition_id).await
}

async fn read_partition_sequence_for_tables(
    db: &Db,
    tables: &OutboxTables,
    partition_id: i64,
) -> i64 {
    #[derive(Debug, FromQueryResult)]
    struct Row {
        sequence: i64,
    }
    let conn = db.sea_internal();
    Row::find_by_statement(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        format!("SELECT sequence FROM {} WHERE id = $1", tables.partitions()),
        [partition_id.into()],
    ))
    .one(&conn)
    .await
    .expect("query")
    .expect("partition row")
    .sequence
}

async fn poll_until<F, Fut>(f: F, timeout_ms: u64)
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        if f().await {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "poll_until timed out after {timeout_ms}ms"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

// ======================================================================
// Test handlers
// ======================================================================

struct CountingSuccessHandler {
    count: Arc<AtomicU32>,
}

#[async_trait::async_trait]
impl LeasedHandler for CountingSuccessHandler {
    async fn handle(&self, batch: &mut Batch<'_>) -> HandlerResult {
        while batch.next_msg().is_some() {
            self.count.fetch_add(1, Ordering::Relaxed);
            batch.ack();
        }
        HandlerResult::Success
    }
}

struct CountingMessageHandler {
    count: Arc<AtomicU32>,
}

#[async_trait::async_trait]
impl LeasedMessageHandler for CountingMessageHandler {
    async fn handle(&self, _msg: &OutboxMessage) -> MessageResult {
        self.count.fetch_add(1, Ordering::Relaxed);
        MessageResult::Ok
    }
}

struct AlwaysRetryHandler;

#[async_trait::async_trait]
impl LeasedMessageHandler for AlwaysRetryHandler {
    async fn handle(&self, _msg: &OutboxMessage) -> MessageResult {
        MessageResult::Retry
    }
}

struct AlwaysRejectHandler;

#[async_trait::async_trait]
impl LeasedMessageHandler for AlwaysRejectHandler {
    async fn handle(&self, _msg: &OutboxMessage) -> MessageResult {
        MessageResult::Reject("permanently bad".into())
    }
}

struct AttemptsRecorder {
    seen_attempts: Arc<Mutex<Vec<i16>>>,
}

#[async_trait::async_trait]
impl LeasedMessageHandler for AttemptsRecorder {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        self.seen_attempts.lock().unwrap().push(msg.attempts);
        MessageResult::Ok
    }
}

struct CountingTxHandler {
    count: Arc<AtomicU32>,
}

#[async_trait::async_trait]
impl TransactionalHandler for CountingTxHandler {
    async fn handle(
        &self,
        _txn: &sea_orm::DatabaseExecutor<'_>,
        msgs: &[OutboxMessage],
    ) -> HandlerResult {
        #[allow(clippy::cast_possible_truncation)]
        self.count.fetch_add(msgs.len() as u32, Ordering::Relaxed);
        HandlerResult::Success
    }
}

struct AlwaysRetryTxHandler;

#[async_trait::async_trait]
impl TransactionalHandler for AlwaysRetryTxHandler {
    async fn handle(
        &self,
        _txn: &sea_orm::DatabaseExecutor<'_>,
        _msgs: &[OutboxMessage],
    ) -> HandlerResult {
        HandlerResult::Retry {
            reason: "transient tx failure".into(),
        }
    }
}

struct AlwaysRejectTxHandler;

#[async_trait::async_trait]
impl TransactionalHandler for AlwaysRejectTxHandler {
    async fn handle(
        &self,
        _txn: &sea_orm::DatabaseExecutor<'_>,
        _msgs: &[OutboxMessage],
    ) -> HandlerResult {
        HandlerResult::Reject {
            reason: "permanently bad tx".into(),
        }
    }
}

/// Rejects a specific message (by seq number), succeeds on others.
struct PoisonMessageHandler {
    poison_seqs: Vec<i64>,
}

#[async_trait::async_trait]
impl LeasedMessageHandler for PoisonMessageHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        if self.poison_seqs.contains(&msg.seq) {
            MessageResult::Reject(format!("poison seq={}", msg.seq))
        } else {
            MessageResult::Ok
        }
    }
}

// ======================================================================
// Chapter 0: Migrations
// ======================================================================

#[tokio::test]
async fn migrations_create_default_table_family() {
    let db = setup_db("ch0_default_tables").await;

    assert_table_family_exists(&db, &OutboxTables::default()).await;
}

#[tokio::test]
async fn migrations_create_custom_table_family() {
    let tables = OutboxTables::new("mini_chat_outbox").unwrap();
    let db = setup_db_with_migrations(
        "ch0_custom_tables",
        super::outbox_migrations_with_prefix(tables.prefix()).unwrap(),
    )
    .await;

    assert_table_family_exists(&db, &tables).await;
}

#[tokio::test]
async fn migrations_create_default_and_custom_table_families_together() {
    let custom_tables = OutboxTables::new("mini_chat_outbox").unwrap();
    let mut migrations = super::outbox_migrations();
    migrations.extend(super::outbox_migrations_with_prefix(custom_tables.prefix()).unwrap());
    let db = setup_db_with_migrations("ch0_default_and_custom_tables", migrations).await;

    assert_table_family_exists(&db, &OutboxTables::default()).await;
    assert_table_family_exists(&db, &custom_tables).await;
}

// ======================================================================
// Chapter 1: Registration
// ======================================================================

#[tokio::test]
async fn registration_creates_partition_and_processor_rows() {
    let db = setup_db("ch1_creates_rows").await;
    let t = make_default_test_outbox().await;

    t.outbox.register_queue(&db, "orders", 4).await.unwrap();

    let part_count = count_rows(&db, "toolkit_outbox_partitions").await;
    assert_eq!(part_count, 4, "4 partition rows");

    let proc_count = count_rows(&db, "toolkit_outbox_processor").await;
    assert_eq!(proc_count, 4, "4 processor rows");

    // Each processor row starts at processed_seq=0, attempts=0
    let ids = t.outbox.all_partition_ids();
    for id in &ids {
        let snap = read_processor_state(&db, *id).await;
        assert_eq!(snap.processed_seq, 0);
        assert_eq!(snap.attempts, 0);
    }
}

#[tokio::test]
async fn registration_is_idempotent() {
    let db = setup_db("ch1_idempotent").await;
    let t = make_default_test_outbox().await;

    t.outbox.register_queue(&db, "orders", 4).await.unwrap();
    t.outbox.register_queue(&db, "orders", 4).await.unwrap();

    let part_count = count_rows(&db, "toolkit_outbox_partitions").await;
    assert_eq!(part_count, 4, "still exactly 4 - no duplicates");
}

#[tokio::test]
async fn registration_rejects_mismatched_partition_count() {
    let db = setup_db("ch1_mismatch").await;
    let t = make_default_test_outbox().await;

    t.outbox.register_queue(&db, "orders", 4).await.unwrap();
    let err = t.outbox.register_queue(&db, "orders", 2).await.unwrap_err();

    assert!(matches!(
        err,
        OutboxError::PartitionCountMismatch {
            expected: 2,
            found: 4,
            ..
        }
    ));
}

#[tokio::test]
async fn registration_multiple_queues_distinct_ids() {
    let db = setup_db("ch1_multi_queue").await;
    let t = make_default_test_outbox().await;

    t.outbox.register_queue(&db, "a", 2).await.unwrap();
    t.outbox.register_queue(&db, "b", 2).await.unwrap();

    let all_ids = t.outbox.all_partition_ids();
    assert_eq!(all_ids.len(), 4);
    // All distinct (sorted + deduped by all_partition_ids)
    let mut deduped = all_ids;
    deduped.dedup();
    assert_eq!(deduped.len(), 4);
}

#[tokio::test]
async fn registration_partition_to_queue_reverse_lookup() {
    let db = setup_db("ch1_reverse_lookup").await;
    let t = make_default_test_outbox().await;

    t.outbox.register_queue(&db, "orders", 2).await.unwrap();

    let ids = t.outbox.all_partition_ids();
    assert_eq!(ids.len(), 2);
    for id in &ids {
        assert_eq!(t.outbox.partition_to_queue(*id).as_deref(), Some("orders"));
    }
}

// ======================================================================
// Chapter 2: Record
// ======================================================================

#[tokio::test]
async fn enqueue_single_creates_body_and_incoming() {
    let db = setup_db("ch2_single").await;
    let t = make_default_test_outbox().await;
    t.outbox.register_queue(&db, "q", 1).await.unwrap();

    enqueue_msgs(&t.outbox, &db, "q", 0, &["hello"]).await;

    assert_eq!(count_rows(&db, "toolkit_outbox_body").await, 1);
    assert_eq!(count_rows(&db, "toolkit_outbox_incoming").await, 1);
}

#[tokio::test]
async fn enqueue_returns_correct_id() {
    let db = setup_db("ch2_correct_id").await;
    let t = make_default_test_outbox().await;
    t.outbox.register_queue(&db, "q", 1).await.unwrap();

    let ids = enqueue_msgs(&t.outbox, &db, "q", 0, &["msg"]).await;
    assert_eq!(ids.len(), 1);
    // The returned ID should be the incoming row ID (positive integer)
    assert!(ids[0].0 > 0);
}

#[tokio::test]
async fn enqueue_tx_rollback_leaves_no_rows() {
    let db = setup_db("ch2_rollback").await;
    let t = make_default_test_outbox().await;
    t.outbox.register_queue(&db, "q", 1).await.unwrap();

    // Use sea_orm transaction directly to simulate rollback
    let conn = db.sea_internal();
    let txn = sea_orm::TransactionTrait::begin(&conn).await.unwrap();
    txn.execute_raw(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        "INSERT INTO toolkit_outbox_body (payload, payload_type) VALUES ($1, $2)",
        [b"data".to_vec().into(), "text/plain".into()],
    ))
    .await
    .unwrap();
    txn.rollback().await.unwrap();

    assert_eq!(count_rows(&db, "toolkit_outbox_body").await, 0);
    assert_eq!(count_rows(&db, "toolkit_outbox_incoming").await, 0);
}

#[tokio::test]
async fn enqueue_with_standalone_connection() {
    let db = setup_db("ch2_standalone").await;
    let t = make_default_test_outbox().await;
    t.outbox.register_queue(&db, "q", 1).await.unwrap();

    enqueue_msgs(&t.outbox, &db, "q", 0, &["standalone"]).await;

    assert_eq!(count_rows(&db, "toolkit_outbox_body").await, 1);
    assert_eq!(count_rows(&db, "toolkit_outbox_incoming").await, 1);
}

#[tokio::test]
async fn enqueue_batch_creates_n_items() {
    let db = setup_db("ch2_batch_n").await;
    let t = make_default_test_outbox().await;
    t.outbox.register_queue(&db, "q", 1).await.unwrap();

    let batch = (0..50)
        .fold(Records::to("q").payload_type("text/plain"), |b, i| {
            b.push(0, format!("msg-{i}").into_bytes())
        })
        .build()
        .unwrap();
    let conn = db.conn().unwrap();
    let ids = t.outbox.enqueue_batch(&conn, batch).await.unwrap();

    assert_eq!(ids.len(), 50);
    assert_eq!(count_rows(&db, "toolkit_outbox_body").await, 50);
    assert_eq!(count_rows(&db, "toolkit_outbox_incoming").await, 50);
}

#[tokio::test]
async fn enqueue_batch_mixed_partitions() {
    let db = setup_db("ch2_batch_mixed").await;
    let t = make_default_test_outbox().await;
    t.outbox.register_queue(&db, "q", 2).await.unwrap();

    let batch = Records::to("q")
        .payload_type("text/plain")
        .push(0, b"a".to_vec())
        .push(1, b"b".to_vec())
        .push(0, b"c".to_vec())
        .push(1, b"d".to_vec())
        .build()
        .unwrap();
    let conn = db.conn().unwrap();
    let ids = t.outbox.enqueue_batch(&conn, batch).await.unwrap();
    assert_eq!(ids.len(), 4);
    assert_eq!(count_rows(&db, "toolkit_outbox_incoming").await, 4);
}

#[tokio::test]
async fn enqueue_batch_one_invalid_rejects_entire_batch() {
    let db = setup_db("ch2_batch_invalid").await;
    let t = make_default_test_outbox().await;
    t.outbox.register_queue(&db, "q", 1).await.unwrap();

    // An oversized entity is refused while the batch is built, before any
    // statement can run.
    let oversized = vec![0u8; 64 * 1024 + 1];
    let err = Records::to("q")
        .payload_type("text/plain")
        .push(0, b"ok".to_vec())
        .push(0, oversized)
        .build()
        .unwrap_err();
    assert!(matches!(err, OutboxError::PayloadTooLarge { .. }));
    assert_eq!(count_rows(&db, "toolkit_outbox_body").await, 0);

    // A rejection that needs the registry - an out-of-range partition - is
    // still all-or-nothing, and still writes nothing.
    let batch = Records::to("q")
        .payload_type("text/plain")
        .push(0, b"ok".to_vec())
        .push(7, b"out of range".to_vec())
        .build()
        .unwrap();
    let conn = db.conn().unwrap();
    let err = t.outbox.enqueue_batch(&conn, batch).await.unwrap_err();
    assert!(matches!(err, OutboxError::PartitionOutOfRange { .. }));
    assert_eq!(count_rows(&db, "toolkit_outbox_body").await, 0);
    assert_eq!(count_rows(&db, "toolkit_outbox_incoming").await, 0);
}

#[tokio::test]
async fn enqueue_empty_batch_returns_empty_vec() {
    let db = setup_db("ch2_batch_empty").await;
    let t = make_default_test_outbox().await;
    t.outbox.register_queue(&db, "q", 1).await.unwrap();

    let conn = db.conn().unwrap();
    let empty = Records::to("q").payload_type("text/plain").build().unwrap();
    let ids = t.outbox.enqueue_batch(&conn, empty).await.unwrap();
    assert!(ids.is_empty());
}

#[tokio::test]
async fn enqueue_batch_over_chunk_size_works() {
    let db = setup_db("ch2_batch_chunk").await;
    let t = make_default_test_outbox().await;
    t.outbox.register_queue(&db, "q", 1).await.unwrap();

    let batch = (0..150)
        .fold(Records::to("q").payload_type("text/plain"), |b, i| {
            b.push(0, format!("msg-{i}").into_bytes())
        })
        .build()
        .unwrap();
    let conn = db.conn().unwrap();
    let ids = t.outbox.enqueue_batch(&conn, batch).await.unwrap();

    assert_eq!(ids.len(), 150);
    assert_eq!(count_rows(&db, "toolkit_outbox_body").await, 150);
    assert_eq!(count_rows(&db, "toolkit_outbox_incoming").await, 150);
}

// ---------------------------------------------------------------------------
// Ch2b: a traced batch, from submission to completion
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_traced_batch_records_one_trace_row_its_bodies_point_at() {
    #[derive(Debug, FromQueryResult)]
    struct TraceRow {
        trace: String,
        owner_instance: String,
        queue: String,
        entities: i64,
        pending: i64,
        failures: i64,
        bodies: i64,
    }

    let db = setup_db("ch2b_trace_row").await;
    let t = make_default_test_outbox().await;
    t.outbox.register_queue(&db, "q", 1).await.unwrap();

    let conn = db.conn().unwrap();
    let batch = Records::to("q")
        .payload_type("text/plain")
        .trace("import-1")
        .push(0, b"a".to_vec())
        .push(0, b"b".to_vec())
        .push(0, b"c".to_vec())
        .build()
        .unwrap();
    t.outbox.enqueue_batch(&conn, batch).await.unwrap();

    let sea = db.sea_internal();
    let row = TraceRow::find_by_statement(Statement::from_string(
        DbBackend::Sqlite,
        "SELECT t.trace, t.owner_instance, t.queue, t.entities, t.pending, t.failures, \
                (SELECT COUNT(*) FROM toolkit_outbox_body b WHERE b.trace_id = t.id) AS bodies \
         FROM toolkit_outbox_trace t",
    ))
    .one(&sea)
    .await
    .expect("trace query")
    .expect("one trace row");

    assert_eq!(row.trace, "import-1");
    assert_eq!(row.owner_instance, t.outbox.instance_id());
    assert_eq!(row.queue, "q");
    assert_eq!(
        (row.entities, row.pending, row.failures),
        (3, 3, 0),
        "pending starts at the batch size and counts down from there"
    );
    assert_eq!(row.bodies, 3, "every body in the batch points at the trace");
}

#[tokio::test]
async fn an_untraced_batch_records_no_trace_at_all() {
    #[derive(Debug, FromQueryResult)]
    struct Count {
        cnt: i64,
    }

    let db = setup_db("ch2b_untraced").await;
    let t = make_default_test_outbox().await;
    t.outbox.register_queue(&db, "q", 1).await.unwrap();

    let conn = db.conn().unwrap();
    let batch = Records::to("q")
        .payload_type("text/plain")
        .push(0, b"a".to_vec())
        .push(0, b"b".to_vec())
        .build()
        .unwrap();
    t.outbox.enqueue_batch(&conn, batch).await.unwrap();

    assert_eq!(count_rows(&db, "toolkit_outbox_trace").await, 0);
    assert_eq!(count_rows(&db, "toolkit_outbox_body").await, 2);

    let sea = db.sea_internal();
    let traced = Count::find_by_statement(Statement::from_string(
        DbBackend::Sqlite,
        "SELECT COUNT(*) AS cnt FROM toolkit_outbox_body WHERE trace_id IS NOT NULL",
    ))
    .one(&sea)
    .await
    .expect("count")
    .expect("row")
    .cnt;
    assert_eq!(traced, 0, "an untraced batch leaves trace_id null");
}

#[tokio::test]
async fn a_traced_batch_counts_down_to_completion_as_it_is_processed() {
    let db = setup_db("ch2b_trace_completes").await;

    let counter = Arc::new(AtomicUsize::new(0));
    let notify = Arc::new(tokio::sync::Notify::new());
    let handler = CountingHandler {
        counter: Arc::clone(&counter),
        notify: Arc::clone(&notify),
    };

    let handle = Outbox::builder(db.clone())
        .processor_tuning(WorkerTuning::processor_default().idle_interval(Duration::from_mins(1)))
        .sequencer_tuning(WorkerTuning::sequencer_default().idle_interval(Duration::from_mins(1)))
        .processors(1)
        .maintenance(1, 1)
        .queue("test-q", Partitions::of(1))
        .leased(handler)
        .start()
        .await
        .unwrap();

    let outbox = handle.outbox();
    let conn = db.conn().unwrap();

    let batch = Records::to("test-q")
        .payload_type("test/msg")
        .trace("import-7")
        .push(0, b"one".to_vec())
        .push(0, b"two".to_vec())
        .build()
        .unwrap();
    outbox.enqueue_batch(&conn, batch).await.unwrap();

    // Before anything is processed the batch is wholly outstanding.
    let before = outbox
        .trace_status(&conn, "import-7")
        .await
        .unwrap()
        .expect("the trace exists as soon as it is enqueued");
    assert_eq!(
        (before.entities, before.pending, before.failures),
        (2, 2, 0)
    );
    assert!(!before.is_complete());
    assert!(!before.is_stalled());
    assert!(before.completed_at.is_none());

    outbox.flush();
    // The handler notifies once per batch, not once per message, so wait on
    // the message count rather than on a notification count.
    for _ in 0..250 {
        if counter.load(Ordering::Relaxed) >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(counter.load(Ordering::Relaxed), 2, "both messages handled");

    // The ack commits after the handler returns, so wait for the countdown.
    let mut status = outbox
        .trace_status(&conn, "import-7")
        .await
        .unwrap()
        .unwrap();
    for _ in 0..50 {
        if status.is_complete() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
        status = outbox
            .trace_status(&conn, "import-7")
            .await
            .unwrap()
            .unwrap();
    }

    assert_eq!(
        (status.entities, status.pending, status.failures),
        (2, 0, 0),
        "every entity reached a terminal state and none failed"
    );
    assert!(status.is_complete());
    assert!(
        status.completed_at.is_some(),
        "reaching zero stamps completion in the same statement"
    );
    assert_eq!(status.queue, "test-q");

    handle.stop().await;
}

#[tokio::test]
async fn a_rejected_entity_completes_its_trace_and_is_counted_as_a_failure() {
    #[derive(Debug, FromQueryResult)]
    struct Row {
        cnt: i64,
    }

    let db = setup_db("ch2b_trace_failure").await;

    let handle = Outbox::builder(db.clone())
        .processor_tuning(WorkerTuning::processor_default().idle_interval(Duration::from_mins(1)))
        .sequencer_tuning(WorkerTuning::sequencer_default().idle_interval(Duration::from_mins(1)))
        .processors(1)
        .maintenance(1, 1)
        .queue("reject-q", Partitions::of(1))
        .leased(AlwaysRejectHandler)
        .start()
        .await
        .unwrap();

    let outbox = handle.outbox();
    let conn = db.conn().unwrap();

    let batch = Records::to("reject-q")
        .payload_type("test/msg")
        .trace("doomed-1")
        .push(0, b"bad".to_vec())
        .build()
        .unwrap();
    outbox.enqueue_batch(&conn, batch).await.unwrap();
    outbox.flush();

    let mut status = outbox
        .trace_status(&conn, "doomed-1")
        .await
        .unwrap()
        .unwrap();
    for _ in 0..100 {
        if status.is_complete() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
        status = outbox
            .trace_status(&conn, "doomed-1")
            .await
            .unwrap()
            .unwrap();
    }

    assert_eq!(
        (status.entities, status.pending, status.failures),
        (1, 0, 1),
        "a dead letter is terminal, and counts as a failure of its trace"
    );
    assert!(status.completed_at.is_some());

    // And the dead letter carries the trace it belonged to, so a consumer can
    // list which entities failed without reading any payload.
    let sea = db.sea_internal();
    let attributed = Row::find_by_statement(Statement::from_string(
        DbBackend::Sqlite,
        "SELECT COUNT(*) AS cnt FROM toolkit_outbox_dead_letters d \
         JOIN toolkit_outbox_trace t ON t.id = d.trace_id \
         WHERE t.trace = 'doomed-1'",
    ))
    .one(&sea)
    .await
    .expect("query")
    .expect("row")
    .cnt;
    assert_eq!(attributed, 1);

    handle.stop().await;
}

async fn read_notified_at(db: &Db, trace: &str) -> Option<String> {
    #[derive(Debug, FromQueryResult)]
    struct Row {
        notified_at: Option<String>,
    }
    let conn = db.sea_internal();
    Row::find_by_statement(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        "SELECT notified_at FROM toolkit_outbox_trace WHERE trace = $1",
        [trace.into()],
    ))
    .one(&conn)
    .await
    .expect("notified_at query")
    .expect("trace row")
    .notified_at
}

struct AckAllHandler;

#[async_trait::async_trait]
impl LeasedMessageHandler for AckAllHandler {
    async fn handle(&self, _msg: &OutboxMessage) -> MessageResult {
        MessageResult::Ok
    }
}

#[tokio::test]
async fn a_completion_is_delivered_only_to_the_instance_that_submitted_it() {
    // Instance A submits and waits. Instance B does the processing. A must be
    // told and B must not, which is the whole point of `owner_instance`.
    let db = setup_db("ch2b_multi_instance").await;

    let instance_a = make_test_outbox(OutboxConfig {
        instance_id: super::types::InstanceId::new("instance-a"),
        ..OutboxConfig::default()
    })
    .await;
    assert_eq!(instance_a.outbox.instance_id(), "instance-a");

    // B runs the pipeline, under its own identity, against the same database.
    let handle_b = Outbox::builder(db.clone())
        .processor_tuning(WorkerTuning::processor_default().idle_interval(Duration::from_mins(1)))
        .sequencer_tuning(
            WorkerTuning::sequencer_default().idle_interval(Duration::from_millis(20)),
        )
        // A's enqueue marks the partition dirty in A's memory, not B's, so B
        // discovers the work through the cold reconciler - the cross-instance
        // path this test also happens to exercise.
        .reconciler_tuning(WorkerTuning::reconciler().idle_interval(Duration::from_millis(20)))
        .processors(1)
        .maintenance(1, 1)
        .queue("shared-q", Partitions::of(1))
        .leased(AckAllHandler)
        .start()
        .await
        .unwrap();
    // Nobody configured either identity: a running outbox generates its own,
    // and two of them are distinct, which is all the routing needs.
    assert_ne!(
        handle_b.outbox().instance_id(),
        instance_a.outbox.instance_id(),
        "two processes must not share an identity"
    );

    // A registers its interest before committing, then submits.
    instance_a
        .outbox
        .register_queue(&db, "shared-q", 1)
        .await
        .unwrap();
    let waiting = instance_a.outbox.subscribe("cross-instance-1");

    let conn = db.conn().unwrap();
    let batch = Records::to("shared-q")
        .payload_type("test/msg")
        .trace("cross-instance-1")
        .push(0, b"work".to_vec())
        .build()
        .unwrap();
    instance_a.outbox.enqueue_batch(&conn, batch).await.unwrap();

    // B processes it. Its ack tries to claim the completion and cannot,
    // because the trace belongs to A - so the mail is left where A will find
    // it rather than being consumed by whoever happened to do the work.
    for _ in 0..200 {
        let status = instance_a
            .outbox
            .trace_status(&conn, "cross-instance-1")
            .await
            .unwrap()
            .unwrap();
        if status.is_complete() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let completed = instance_a
        .outbox
        .trace_status(&conn, "cross-instance-1")
        .await
        .unwrap()
        .unwrap();
    // B marked the work done: it counted the trace down and stamped
    // completion, without owning it. Processing and delivery are two separate
    // marks - whoever acks stamps `completed_at`, only the owner stamps
    // `notified_at`.
    assert_eq!(
        (completed.entities, completed.pending, completed.failures),
        (1, 0, 0),
        "instance B must have counted the trace down"
    );
    assert!(
        completed.completed_at.is_some(),
        "instance B must have stamped completion, even though it does not own the trace"
    );
    assert_eq!(
        read_notified_at(&db, "cross-instance-1").await,
        None,
        "but B must not consume mail addressed to A"
    );

    // A collects its own mail.
    let mut notifier = super::workers::notifier::Notifier {
        outbox: Arc::clone(&instance_a.outbox),
        db: db.clone(),
        batch_size: 100,
        next_look: Duration::from_millis(100),
    };
    let cancel = tokio_util::sync::CancellationToken::new();
    notifier.execute(&cancel).await.unwrap();

    let outcome = tokio::time::timeout(Duration::from_secs(1), waiting)
        .await
        .expect("A is told")
        .expect("with an outcome");
    assert_eq!(outcome.trace, "cross-instance-1");
    assert_eq!(outcome.entities, 1);
    assert_eq!(outcome.failures, 0);
    assert!(outcome.is_clean());

    assert!(
        read_notified_at(&db, "cross-instance-1").await.is_some(),
        "the claim stamps delivery so it cannot happen twice"
    );

    // A second pass finds nothing: delivery is at most once.
    notifier.execute(&cancel).await.unwrap();

    handle_b.stop().await;
}

#[cfg(feature = "test-support")]
#[tokio::test]
async fn a_partial_countdown_does_not_try_to_claim() {
    // A batch spread over several partitions is acked once per partition, and
    // only the last of those reaches zero. The others must not issue the
    // claim: it is a guarded UPDATE against the one row every partition of the
    // batch contends for.
    let (db, recorder) = crate::test_support::connect_with_recorder(
        "sqlite:file:ch2b_partial_claim?mode=memory&cache=shared",
        ConnectOpts {
            max_conns: Some(1),
            ..Default::default()
        },
    )
    .await
    .expect("connect");
    run_migrations_for_testing(&db, super::outbox_migrations())
        .await
        .expect("migrations");
    let t = make_default_test_outbox().await;
    t.outbox.register_queue(&db, "q", 2).await.unwrap();
    let pids = t.outbox.all_partition_ids();

    let conn = db.conn().unwrap();
    t.outbox
        .enqueue_batch(
            &conn,
            Records::to("q")
                .payload_type("text/plain")
                .trace("split-1")
                .push(0, b"one".to_vec())
                .push(1, b"two".to_vec())
                .build()
                .unwrap(),
        )
        .await
        .unwrap();
    run_sequencer_once(&t, &db).await;

    // Ack the first partition: the trace still has one entity outstanding.
    recorder.clear();
    run_leased(
        &db,
        pids[0],
        CountingSuccessHandler {
            count: Arc::new(AtomicU32::new(0)),
        },
        Duration::from_secs(30),
        10,
    )
    .await;
    let after_first: Vec<String> = recorder.events().into_iter().map(|q| q.sql).collect();
    assert!(
        !after_first
            .iter()
            .any(|sql| sql.contains("SET notified_at")),
        "an advance that left work outstanding must not attempt the claim: {after_first:#?}"
    );

    // Ack the second: this one completes the batch, so the claim is right.
    recorder.clear();
    run_leased(
        &db,
        pids[1],
        CountingSuccessHandler {
            count: Arc::new(AtomicU32::new(0)),
        },
        Duration::from_secs(30),
        10,
    )
    .await;
    let after_second: Vec<String> = recorder.events().into_iter().map(|q| q.sql).collect();
    assert!(
        after_second
            .iter()
            .any(|sql| sql.contains("SET notified_at")),
        "the advance that reached zero must attempt it: {after_second:#?}"
    );
}

// ---------------------------------------------------------------------------
// Ch2d: what the sweep collects, and what it must not
// ---------------------------------------------------------------------------

/// Insert a trace row with controlled timestamps, so the sweep's rules can be
/// exercised without waiting a day.
async fn seed_trace(
    db: &Db,
    trace: &str,
    pending: i64,
    created: &str,
    completed: Option<&str>,
    notified: Option<&str>,
) -> i64 {
    #[derive(Debug, FromQueryResult)]
    struct Row {
        id: i64,
    }

    let conn = db.sea_internal();
    let completed = completed.map_or_else(|| "NULL".to_owned(), |e| format!("datetime({e})"));
    let notified = notified.map_or_else(|| "NULL".to_owned(), |e| format!("datetime({e})"));
    conn.execute_raw(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        format!(
            "INSERT INTO toolkit_outbox_trace \
               (trace, owner_instance, queue, entities, pending, created_at, completed_at, notified_at) \
             VALUES ($1, 'someone', 'q', 1, $2, datetime({created}), {completed}, {notified})"
        ),
        [trace.into(), pending.into()],
    ))
    .await
    .expect("seed trace");

    Row::find_by_statement(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        "SELECT id FROM toolkit_outbox_trace WHERE trace = $1",
        [trace.into()],
    ))
    .one(&conn)
    .await
    .expect("read back")
    .expect("row")
    .id
}

/// The same, with a failure count, since that is what decides how long a
/// delivered trace is kept.
async fn seed_trace_with_failures(
    db: &Db,
    trace: &str,
    failures: i64,
    created: &str,
    completed: Option<&str>,
    notified: Option<&str>,
) -> i64 {
    let id = seed_trace(db, trace, 0, created, completed, notified).await;
    let conn = db.sea_internal();
    conn.execute_raw(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        "UPDATE toolkit_outbox_trace SET failures = $1 WHERE id = $2",
        [failures.into(), id.into()],
    ))
    .await
    .expect("set failures");
    id
}

async fn surviving_traces(db: &Db) -> Vec<String> {
    #[derive(Debug, FromQueryResult)]
    struct Row {
        trace: String,
    }
    let conn = db.sea_internal();
    let mut names: Vec<String> = Row::find_by_statement(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        "SELECT trace FROM toolkit_outbox_trace",
        [],
    ))
    .all(&conn)
    .await
    .expect("surviving traces")
    .into_iter()
    .map(|row| row.trace)
    .collect();
    names.sort();
    names
}

#[tokio::test]
async fn the_sweep_collects_finished_traces_and_leaves_live_ones() {
    let db = setup_db("ch2d_trace_sweep").await;
    let t = make_default_test_outbox().await;
    t.outbox.register_queue(&db, "q", 1).await.unwrap();

    // 1. Delivered and past retention: ordinary housekeeping.
    seed_trace(
        &db,
        "delivered-old",
        0,
        "'now','-2 hours'",
        Some("'now','-2 hours'"),
        Some("'now','-2 hours'"),
    )
    .await;
    // 2. Completed but never collected - the owner crashed before hearing.
    seed_trace(
        &db,
        "uncollected-old",
        0,
        "'now','-2 hours'",
        Some("'now','-2 hours'"),
        None,
    )
    .await;
    // 3. Never completed and old: a crash between the trace insert and the
    //    body insert, rows deleted by hand, or a handler retrying for longer
    //    than the leftover window. Time is the only evidence a single-table
    //    sweep has, which is why that window is days rather than hours.
    seed_trace(&db, "leftover-old", 1, "'now','-2 days'", None, None).await;
    // 4. The same, but inside the window: still assumed to be in flight.
    seed_trace(&db, "in-flight-recent", 1, "'now','-2 hours'", None, None).await;
    // 5. Delivered, but only just: still inside retention.
    seed_trace(
        &db,
        "delivered-fresh",
        0,
        "'now'",
        Some("'now'"),
        Some("'now'"),
    )
    .await;

    let mut sweeper = test_trace_sweeper(
        &db,
        super::workers::trace_sweeper::TraceSweep {
            retention: Duration::from_hours(1),
            orphan_after: Duration::from_hours(1),
            leftover_after: Duration::from_hours(24),
        },
    );
    let cancel = CancellationToken::new();
    sweeper.execute(&cancel).await.unwrap();

    assert_eq!(
        surviving_traces(&db).await,
        vec!["delivered-fresh".to_owned(), "in-flight-recent".to_owned()],
        "collected: delivered past retention, uncollected past the orphan period, \
         and one never completed past the leftover window. Survived: a fresh \
         delivery, and one still inside the window"
    );
}

#[tokio::test]
async fn a_trace_that_produced_dead_letters_is_kept_longer() {
    // A dead letter outlives the delivery it failed, so the trace it belonged
    // to has to outlive it too - otherwise the dead letter cannot be
    // attributed to the batch it came from. The rule is the trace's own
    // `failures` count, not a lookup into the dead-letter table: a five-minute
    // background sweep must not touch three tables to decide.
    let db = setup_db("ch2d_trace_sweep_dl").await;
    let t = make_default_test_outbox().await;
    t.outbox.register_queue(&db, "q", 1).await.unwrap();

    // Both delivered an hour ago; one had a failure, one did not.
    seed_trace_with_failures(
        &db,
        "clean-hour-ago",
        0,
        "'now','-1 hours'",
        Some("'now','-1 hours'"),
        Some("'now','-1 hours'"),
    )
    .await;
    seed_trace_with_failures(
        &db,
        "failed-hour-ago",
        1,
        "'now','-1 hours'",
        Some("'now','-1 hours'"),
        Some("'now','-1 hours'"),
    )
    .await;

    let mut sweeper = test_trace_sweeper(
        &db,
        super::workers::trace_sweeper::TraceSweep {
            // Delivered traces go after a minute, but one with failures waits
            // for the leftover window.
            retention: Duration::from_mins(1),
            orphan_after: Duration::from_mins(1),
            leftover_after: Duration::from_hours(24 * 7),
        },
    );
    let cancel = CancellationToken::new();
    sweeper.execute(&cancel).await.unwrap();

    assert_eq!(
        surviving_traces(&db).await,
        vec!["failed-hour-ago".to_owned()],
        "the clean one is collected; the one with a dead letter is held"
    );

    // Past the leftover window it goes too.
    let mut later = test_trace_sweeper(
        &db,
        super::workers::trace_sweeper::TraceSweep {
            retention: Duration::from_mins(1),
            orphan_after: Duration::from_mins(1),
            leftover_after: Duration::from_mins(1),
        },
    );
    later.execute(&cancel).await.unwrap();
    assert!(surviving_traces(&db).await.is_empty());
}

#[tokio::test]
async fn a_stalled_trace_is_visible_before_it_completes() {
    // The batch is the unit of notification, so a consumer hears nothing until
    // every entity is terminal. That would leave "still working" and "wedged
    // for an hour" indistinguishable, which is what the stall fields answer.
    let db = setup_db("ch2b_trace_stalled").await;

    let handle = Outbox::builder(db.clone())
        .processor_tuning(
            WorkerTuning::processor_default()
                .idle_interval(Duration::from_millis(20))
                .retry_base(Duration::from_millis(5))
                .retry_max(Duration::from_millis(20)),
        )
        .sequencer_tuning(
            WorkerTuning::sequencer_default().idle_interval(Duration::from_millis(20)),
        )
        .processors(1)
        .maintenance(1, 1)
        .queue("stall-q", Partitions::of(1))
        .leased(AlwaysRetryHandler)
        .start()
        .await
        .unwrap();

    let outbox = handle.outbox();
    let conn = db.conn().unwrap();
    let batch = Records::to("stall-q")
        .payload_type("test/msg")
        .trace("stuck-1")
        .push(0, b"never succeeds".to_vec())
        .build()
        .unwrap();
    outbox.enqueue_batch(&conn, batch).await.unwrap();
    outbox.flush();

    // Wait for the handler to have been tried at least twice, so the stall is
    // a fact about the batch rather than a first attempt in progress.
    let mut status = outbox
        .trace_status(&conn, "stuck-1")
        .await
        .unwrap()
        .unwrap();
    for _ in 0..200 {
        if status.attempts >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
        status = outbox
            .trace_status(&conn, "stuck-1")
            .await
            .unwrap()
            .unwrap();
    }

    assert!(
        status.attempts >= 2,
        "the retry count must be visible, got {}",
        status.attempts
    );
    assert!(status.is_stalled(), "a retrying batch reports a stall");
    assert!(
        status.stalled_since.is_some(),
        "and says since when, so a consumer can act on the duration"
    );
    assert_eq!(status.pending, 1, "nothing reached a terminal state");
    assert_eq!(status.failures, 0, "and nothing failed permanently either");
    assert!(!status.is_complete(), "a retried batch is never complete");
    assert!(status.completed_at.is_none());

    // The first stall time is kept, not the latest attempt: what a consumer
    // needs is how long it has been stuck.
    let first_seen = status.stalled_since;
    let attempts_then = status.attempts;
    for _ in 0..100 {
        let later = outbox
            .trace_status(&conn, "stuck-1")
            .await
            .unwrap()
            .unwrap();
        if later.attempts > attempts_then {
            assert_eq!(
                later.stalled_since, first_seen,
                "stalled_since must not move with each new attempt"
            );
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    handle.stop().await;
}

#[tokio::test]
async fn a_trace_on_an_empty_batch_is_refused() {
    // Nothing acks an empty batch, so nothing counts its trace down, so
    // nothing stamps completion - a subscriber would wait for ever.
    let db = setup_db("ch2_empty_traced_batch").await;
    let t = make_default_test_outbox().await;
    t.outbox.register_queue(&db, "q", 1).await.unwrap();

    let err = Records::to("q")
        .payload_type("text/plain")
        .trace("nothing-to-do")
        .build()
        .unwrap_err();
    assert_eq!(
        err.to_string(),
        "a traced batch must carry at least one entity"
    );

    // Untraced, an empty batch remains legal: it asks for nothing.
    let conn = db.conn().unwrap();
    let ids = t
        .outbox
        .enqueue_batch(
            &conn,
            Records::to("q").payload_type("text/plain").build().unwrap(),
        )
        .await
        .unwrap();
    assert!(ids.is_empty());
    assert_eq!(count_rows(&db, "toolkit_outbox_trace").await, 0);
}

#[tokio::test]
async fn enqueue_oversized_payload_rejected() {
    let db = setup_db("ch2_oversized").await;
    let t = make_default_test_outbox().await;
    t.outbox.register_queue(&db, "q", 1).await.unwrap();

    let oversized = vec![0u8; 64 * 1024 + 1];
    let err = Record::to("q", 0)
        .payload(oversized, "bin")
        .build()
        .unwrap_err();
    assert!(matches!(err, OutboxError::PayloadTooLarge { .. }));
}

#[tokio::test]
async fn enqueue_unregistered_queue_rejected() {
    let db = setup_db("ch2_unreg").await;
    let t = make_default_test_outbox().await;
    // Don't register any queue

    let conn = db.conn().unwrap();
    let err = t
        .outbox
        .enqueue(
            &conn,
            Record::to("nonexistent", 0)
                .payload(b"x".to_vec(), "text/plain")
                .build()
                .unwrap(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, OutboxError::QueueNotRegistered(_)));
}

#[tokio::test]
async fn enqueue_out_of_range_partition_rejected() {
    let db = setup_db("ch2_oor").await;
    let t = make_default_test_outbox().await;
    t.outbox.register_queue(&db, "q", 4).await.unwrap();

    let conn = db.conn().unwrap();
    let err = t
        .outbox
        .enqueue(
            &conn,
            Record::to("q", 5)
                .payload(b"x".to_vec(), "text/plain")
                .build()
                .unwrap(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, OutboxError::PartitionOutOfRange { .. }));
}

#[tokio::test]
async fn enqueue_transaction_helper_auto_flushes() {
    let db = setup_db("ch2_tx_flush").await;
    let t = make_default_test_outbox().await;
    t.outbox.register_queue(&db, "q", 1).await.unwrap();

    // Set up a notified() listener before the transaction.
    // flush() now goes through the prioritizer, so listen on its notifier.
    let notified = t.prioritizer.notifier();
    let notified = notified.notified();

    let (_db, result) = t
        .outbox
        .transaction(db, |tx| {
            let outbox = Arc::clone(&t.outbox);
            Box::pin(async move {
                outbox
                    .enqueue(
                        tx,
                        Record::to("q", 0)
                            .payload(b"hello".to_vec(), "text/plain")
                            .build()
                            .unwrap(),
                    )
                    .await
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                Ok(())
            })
        })
        .await;
    result.unwrap();

    // Notify should fire within a short timeout
    tokio::time::timeout(Duration::from_millis(100), notified)
        .await
        .expect("sequencer should be notified on successful transaction");
}

#[tokio::test]
async fn enqueue_transaction_helper_no_flush_on_rollback() {
    let db = setup_db("ch2_tx_no_flush").await;
    let t = make_default_test_outbox().await;
    t.outbox.register_queue(&db, "q", 1).await.unwrap();

    let (_db, result) = t
        .outbox
        .transaction(db, |_tx| {
            Box::pin(async move { Err::<(), _>(anyhow::anyhow!("rollback")) })
        })
        .await;
    assert!(result.is_err());

    // Give a brief window — notify should NOT fire
    let notifier = t.prioritizer.notifier();
    let notified_fut = notifier.notified();
    let timed_out = tokio::time::timeout(Duration::from_millis(50), notified_fut)
        .await
        .is_err();
    assert!(timed_out, "sequencer should NOT be notified on rollback");
}

// ---------------------------------------------------------------------------
// Ch2e: stalls, pushed to whoever is watching
// ---------------------------------------------------------------------------

/// Retries a fixed number of times before letting the batch through, so a
/// stall can be observed and then observed to clear.
struct StubbornHandler {
    remaining_retries: Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait::async_trait]
impl LeasedMessageHandler for StubbornHandler {
    async fn handle(&self, _msg: &OutboxMessage) -> MessageResult {
        use std::sync::atomic::Ordering;
        if self
            .remaining_retries
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                if n > 0 { Some(n - 1) } else { None }
            })
            .is_ok()
        {
            return MessageResult::Retry;
        }
        MessageResult::Ok
    }
}

fn test_stall_reporter(
    outbox: &Arc<Outbox>,
    db: &Db,
) -> super::workers::stall_reporter::StallReporter {
    super::workers::stall_reporter::StallReporter {
        outbox: Arc::clone(outbox),
        db: db.clone(),
        batch_size: 100,
    }
}

#[tokio::test]
async fn a_retrying_batch_reports_its_stall_and_withdraws_it_on_progress() {
    let db = setup_db("ch2e_stall_reported").await;

    let remaining = Arc::new(std::sync::atomic::AtomicUsize::new(3));
    let handle = Outbox::builder(db.clone())
        .processor_tuning(
            WorkerTuning::processor_default()
                .idle_interval(Duration::from_millis(20))
                .retry_base(Duration::from_millis(10))
                .retry_max(Duration::from_millis(20)),
        )
        .sequencer_tuning(
            WorkerTuning::sequencer_default().idle_interval(Duration::from_millis(20)),
        )
        .processors(1)
        .maintenance(1, 1)
        .queue("stall-q", Partitions::of(1))
        .leased(StubbornHandler {
            remaining_retries: Arc::clone(&remaining),
        })
        .start()
        .await
        .unwrap();
    let outbox = Arc::clone(handle.outbox());

    // Interest, and the receiver that makes this instance look for stalls.
    let waiting = outbox.subscribe("stubborn-1");
    let mut stalls = waiting.progress();

    let conn = db.conn().unwrap();
    let batch = Records::to("stall-q")
        .payload_type("test/msg")
        .trace("stubborn-1")
        .push(0, b"work".to_vec())
        .push(0, b"more".to_vec())
        .build()
        .unwrap();
    outbox.enqueue_batch(&conn, batch).await.unwrap();

    // The reporter is driven by hand so the test does not race its timer.
    let mut reporter = test_stall_reporter(&outbox, &db);
    let cancel = tokio_util::sync::CancellationToken::new();

    let mut seen: Option<super::trace::TraceProgress> = None;
    for _ in 0..200 {
        reporter.execute(&cancel).await.unwrap();
        if let Some(progress) = stalls.current() {
            seen = Some(progress);
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let stalled = seen.expect("a batch being retried must be reported as stalled");
    assert_eq!(stalled.trace, "stubborn-1");
    assert_eq!(stalled.entities, 2, "the whole batch is what stalled");
    assert_eq!(
        stalled.pending, 2,
        "nothing reached a terminal state while the handler kept retrying"
    );
    assert_eq!(stalled.failures, 0, "a retry is not a failure");
    assert!(
        stalled.attempts > 0,
        "the report must say how hard the handler has tried"
    );
    assert!(
        stalled.stalled_for() >= chrono::TimeDelta::zero(),
        "the stall must be dated in the past"
    );

    // The handler relents. Once the batch has moved, the trace row no longer
    // matches the reporter's predicate and the stall is withdrawn - checked
    // before the completion consumes the subscription.
    let mut withdrawn = false;
    for _ in 0..200 {
        reporter.execute(&cancel).await.unwrap();
        if stalls.current().is_none() {
            withdrawn = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        withdrawn,
        "a batch that is moving again must stop being reported as stalled"
    );

    let outcome = tokio::time::timeout(Duration::from_secs(5), waiting)
        .await
        .expect("the batch finishes once the handler relents")
        .expect("with an outcome");
    assert!(outcome.is_clean(), "retries are not failures: {outcome:?}");
    assert_eq!(remaining.load(std::sync::atomic::Ordering::SeqCst), 0);

    handle.stop().await;
}

#[tokio::test]
async fn a_stalled_trace_nobody_watches_is_not_reported() {
    // The gate is `Registry::wants_progress`: a caller that only awaits
    // completion never takes a progress receiver, so the reporter returns
    // without a statement even though a matching row is sitting there.
    let db = setup_db("ch2e_stall_unwatched").await;
    let t = make_default_test_outbox().await;
    t.outbox.register_queue(&db, "q", 1).await.unwrap();

    insert_stalled_trace(&db, "unwatched-1", t.outbox.instance_id()).await;

    // Interest in the completion, but no interest in progress.
    let waiting = t.outbox.subscribe("unwatched-1");
    assert!(
        !t.outbox.mailbox().subscriptions().wants_progress(),
        "awaiting a completion must not arm the stall query"
    );

    let mut reporter = test_stall_reporter(&t.outbox, &db);
    let cancel = tokio_util::sync::CancellationToken::new();
    reporter.execute(&cancel).await.unwrap();

    assert!(
        waiting.progress().current().is_none(),
        "nothing was published while nobody was watching"
    );

    // Watching arms it, and the same row is then reported.
    let mut stalls = waiting.progress();
    assert!(t.outbox.mailbox().subscriptions().wants_progress());
    reporter.execute(&cancel).await.unwrap();
    let seen = stalls
        .current()
        .expect("the row was always there; only the watcher was missing");
    assert_eq!(seen.trace, "unwatched-1");
    assert_eq!(seen.attempts, 7);
    assert_eq!(seen.last_error.as_deref(), Some("upstream refused"));
}

/// Insert a trace that is incomplete and stalled, the state the reporter looks
/// for, without waiting for a handler to fail its way there.
async fn insert_stalled_trace(db: &Db, trace: &str, owner: &str) {
    let conn = db.sea_internal();
    conn.execute_raw(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        "INSERT INTO toolkit_outbox_trace \
           (trace, owner_instance, queue, entities, pending, failures, attempts, \
            last_error, stalled_since, created_at) \
         VALUES ($1, $2, 'q', 4, 3, 0, 7, 'upstream refused', \
                 datetime('now','-30 seconds'), datetime('now','-60 seconds'))",
        [trace.into(), owner.into()],
    ))
    .await
    .expect("insert stalled trace");
}

// ======================================================================
// Chapter 3: Sequencer
// ======================================================================

#[tokio::test]
async fn sequencer_moves_incoming_to_outgoing() {
    let db = setup_db("ch3_moves").await;
    let t = make_default_test_outbox().await;
    t.outbox.register_queue(&db, "q", 1).await.unwrap();

    enqueue_msgs(&t.outbox, &db, "q", 0, &["a", "b", "c"]).await;
    assert_eq!(count_rows(&db, "toolkit_outbox_incoming").await, 3);

    run_sequencer_once(&t, &db).await;

    assert_eq!(count_rows(&db, "toolkit_outbox_incoming").await, 0);
    assert_eq!(count_rows(&db, "toolkit_outbox_outgoing").await, 3);

    let pid = t.outbox.all_partition_ids()[0];
    let outgoing = read_outgoing(&db, pid).await;
    let seqs: Vec<i64> = outgoing.iter().map(|r| r.seq).collect();
    assert_eq!(seqs, vec![1, 2, 3]);

    // Verify structural fields: each row belongs to the queried partition,
    // has a positive id, and references a valid body row.
    for row in &outgoing {
        assert_eq!(row.partition_id, pid);
        assert!(row.id > 0);
        assert!(row.body_id > 0);
    }
    // IDs should be unique
    let ids: Vec<i64> = outgoing.iter().map(|r| r.id).collect();
    assert_eq!(ids.len(), 3);
    assert!(ids[0] != ids[1] && ids[1] != ids[2]);
}

/// Record many messages to one partition, sequence them, and verify the
/// outgoing sequence order matches the original enqueue (insertion) order.
/// This guards against non-deterministic row ordering in the claim step.
#[tokio::test]
async fn sequencer_preserves_enqueue_order_in_sequences() {
    let db = setup_db("ch3_fifo").await;
    let t = make_default_test_outbox().await;
    t.outbox.register_queue(&db, "q", 1).await.unwrap();

    // Enough messages to surface ordering issues.
    let payloads: Vec<String> = (0..8).map(|i| format!("msg-{i}")).collect();
    let payload_refs: Vec<&str> = payloads.iter().map(String::as_str).collect();
    let enqueue_ids = enqueue_msgs(&t.outbox, &db, "q", 0, &payload_refs).await;

    run_sequencer_once(&t, &db).await;

    let pid = t.outbox.all_partition_ids()[0];
    let outgoing = read_outgoing(&db, pid).await;

    // Sequences must be strictly monotonically increasing
    let seqs: Vec<i64> = outgoing.iter().map(|r| r.seq).collect();
    assert_eq!(seqs, vec![1, 2, 3, 4, 5, 6, 7, 8]);

    // body_ids must follow the same order as enqueue_ids (insertion order)
    let body_ids: Vec<i64> = outgoing.iter().map(|r| r.body_id).collect();
    for i in 1..body_ids.len() {
        assert!(
            body_ids[i] > body_ids[i - 1],
            "body_id[{i}]={} should be > body_id[{}]={}",
            body_ids[i],
            i - 1,
            body_ids[i - 1]
        );
    }

    // Verify count matches
    assert_eq!(enqueue_ids.len(), 8);
    assert_eq!(outgoing.len(), 8);
}

#[tokio::test]
async fn sequencer_updates_partition_sequence_counter() {
    let db = setup_db("ch3_seq_counter").await;
    let t = make_default_test_outbox().await;
    t.outbox.register_queue(&db, "q", 1).await.unwrap();

    enqueue_msgs(&t.outbox, &db, "q", 0, &["a", "b", "c"]).await;
    run_sequencer_once(&t, &db).await;

    let pid = t.outbox.all_partition_ids()[0];
    let seq = read_partition_sequence(&db, pid).await;
    assert_eq!(seq, 3);
}

#[tokio::test]
async fn sequencer_multi_partition_independent_sequences() {
    let db = setup_db("ch3_multi_part").await;
    let t = make_default_test_outbox().await;
    t.outbox.register_queue(&db, "q", 2).await.unwrap();

    enqueue_msgs(&t.outbox, &db, "q", 0, &["a0", "b0"]).await;
    enqueue_msgs(&t.outbox, &db, "q", 1, &["a1", "b1", "c1"]).await;
    run_sequencer_once(&t, &db).await;

    let ids = t.outbox.all_partition_ids();
    let out0 = read_outgoing(&db, ids[0]).await;
    let out1 = read_outgoing(&db, ids[1]).await;

    let seqs0: Vec<i64> = out0.iter().map(|r| r.seq).collect();
    let seqs1: Vec<i64> = out1.iter().map(|r| r.seq).collect();
    assert_eq!(seqs0, vec![1, 2]);
    assert_eq!(seqs1, vec![1, 2, 3]);
}

#[tokio::test]
async fn sequencer_empty_incoming_returns_zero() {
    let db = setup_db("ch3_empty").await;
    let t = make_default_test_outbox().await;
    t.outbox.register_queue(&db, "q", 1).await.unwrap();

    let mut seq = make_sequencer(&t, SequencerConfig::default(), &db);
    let cancel = CancellationToken::new();
    let result = seq.execute(&cancel).await.unwrap();
    assert!(matches!(result, Directive::Idle(_)));
}

#[tokio::test]
async fn sequencer_consecutive_batches_contiguous_sequences() {
    let db = setup_db("ch3_contiguous").await;
    let t = make_default_test_outbox().await;
    t.outbox.register_queue(&db, "q", 1).await.unwrap();

    enqueue_msgs(&t.outbox, &db, "q", 0, &["a", "b"]).await;
    run_sequencer_once(&t, &db).await;

    enqueue_msgs(&t.outbox, &db, "q", 0, &["c", "d"]).await;
    run_sequencer_once(&t, &db).await;

    let pid = t.outbox.all_partition_ids()[0];
    let outgoing = read_outgoing(&db, pid).await;
    let seqs: Vec<i64> = outgoing.iter().map(|r| r.seq).collect();
    assert_eq!(seqs, vec![1, 2, 3, 4]);
}

#[tokio::test]
async fn sequencer_batch_size_limit_enforced() {
    let db = setup_db("ch3_batch_limit").await;
    let t = make_default_test_outbox().await;
    t.outbox.register_queue(&db, "q", 1).await.unwrap();

    // 5 items, batch_size=2, max_inner_iterations=2 → processes 4, leaves 1.
    enqueue_msgs(&t.outbox, &db, "q", 0, &["a", "b", "c", "d", "e"]).await;

    let mut seq = make_sequencer(
        &t,
        SequencerConfig {
            batch_size: 2,
            max_inner_iterations: 2,
            ..Default::default()
        },
        &db,
    );
    let cancel = CancellationToken::new();
    // 2 iterations × 2 items = 4 processed. 1 remains → not drained → re-dirtied.
    let result = seq.execute(&cancel).await.unwrap();
    assert!(matches!(result, Directive::Proceed(_)));
    assert_eq!(result.payload().rows_claimed, 4);
    // Not drained (hit max_inner_iterations) → re-dirtied
    let guard = t
        .prioritizer
        .take()
        .expect("partition should be re-dirtied");
    guard.processed();

    // Remaining 1 item still in incoming
    assert_eq!(count_rows(&db, "toolkit_outbox_incoming").await, 1);
}

#[tokio::test]
async fn sequencer_saturated_partition_re_dirtied() {
    let db = setup_db("ch3_saturated").await;
    let t = make_default_test_outbox().await;
    t.outbox.register_queue(&db, "q", 1).await.unwrap();

    // With batch_size=2, max_inner_iterations=1: claims 2 of 3, can't drain
    // in one iteration → partition is still saturated → re-dirtied.
    enqueue_msgs(&t.outbox, &db, "q", 0, &["a", "b", "c"]).await;

    let mut seq = make_sequencer(
        &t,
        SequencerConfig {
            batch_size: 2,
            max_inner_iterations: 1,
            ..Default::default()
        },
        &db,
    );
    let cancel = CancellationToken::new();
    let result = seq.execute(&cancel).await.unwrap();
    assert!(matches!(result, Directive::Proceed(_)));
    // Only 1 inner iteration allowed, claimed 2 of 3 → not drained → re-dirtied
    let guard = t
        .prioritizer
        .take()
        .expect("partition should be re-dirtied");
    guard.processed();
}

#[tokio::test]
async fn sequencer_unsaturated_partition_not_re_dirtied() {
    let db = setup_db("ch3_unsaturated").await;
    let t = make_default_test_outbox().await;
    t.outbox.register_queue(&db, "q", 1).await.unwrap();

    enqueue_msgs(&t.outbox, &db, "q", 0, &["a"]).await;

    let mut seq = make_sequencer(
        &t,
        SequencerConfig {
            batch_size: 100,
            ..Default::default()
        },
        &db,
    );
    let cancel = CancellationToken::new();
    let result = seq.execute(&cancel).await.unwrap();
    assert!(matches!(result, Directive::Proceed(_)));
    // Not saturated → not re-dirtied (but Proceed because work was done)
    assert!(t.prioritizer.take().is_none());
}

#[tokio::test]
async fn sequencer_skips_empty_partitions() {
    let db = setup_db("ch3_skip_empty").await;
    let t = make_default_test_outbox().await;
    t.outbox.register_queue(&db, "q", 2).await.unwrap();

    // Only enqueue to partition 1, not partition 0
    enqueue_msgs(&t.outbox, &db, "q", 1, &["only-p1"]).await;
    run_sequencer_once(&t, &db).await;

    let ids = t.outbox.all_partition_ids();
    let out0 = read_outgoing(&db, ids[0]).await;
    let out1 = read_outgoing(&db, ids[1]).await;

    assert!(out0.is_empty(), "partition 0 should have no outgoing");
    assert_eq!(out1.len(), 1, "partition 1 should have 1 outgoing");
}

// ======================================================================
// Chapter 4: Transactional Processing
// ======================================================================

async fn run_transactional(
    db: &Db,
    partition_id: i64,
    handler: impl TransactionalHandler + 'static,
    msg_batch_size: u32,
) -> Option<super::strategy::ProcessResult> {
    let conn = db.sea_internal();
    let backend = conn.get_database_backend();
    drop(conn);

    let strategy = TransactionalStrategy::new(Box::new(handler));
    let tables = OutboxTables::default();
    let statements = super::statements::OutboxStatements::new(backend, &tables);
    let mailbox = super::subscription::Mailbox::new(
        "test-instance".to_owned(),
        super::subscription::Registry::new(),
    );
    let ctx = ProcessContext {
        db,
        store: OutboxStore::new(&statements),
        partition_id,
        mailbox: &mailbox,
    };
    strategy.process(&ctx, msg_batch_size).await.unwrap()
}

#[tokio::test]
async fn transactional_success_advances_cursor() {
    let db = setup_db("ch4_tx_success").await;
    let t = make_default_test_outbox().await;
    t.outbox.register_queue(&db, "q", 1).await.unwrap();
    let pid = t.outbox.all_partition_ids()[0];

    enqueue_and_sequence(&t, &db, "q", 0, &["a", "b", "c"]).await;

    let count = Arc::new(AtomicU32::new(0));
    run_transactional(
        &db,
        pid,
        CountingTxHandler {
            count: count.clone(),
        },
        3,
    )
    .await;

    assert_eq!(count.load(Ordering::Relaxed), 3);
    let snap = read_processor_state(&db, pid).await;
    assert_eq!(snap.processed_seq, 3);
    assert_eq!(snap.attempts, 0);
}

#[tokio::test]
async fn transactional_retry_increments_attempts() {
    let db = setup_db("ch4_tx_retry").await;
    let t = make_default_test_outbox().await;
    t.outbox.register_queue(&db, "q", 1).await.unwrap();
    let pid = t.outbox.all_partition_ids()[0];

    enqueue_and_sequence(&t, &db, "q", 0, &["msg"]).await;

    run_transactional(&db, pid, AlwaysRetryTxHandler, 10).await;

    let snap = read_processor_state(&db, pid).await;
    assert_eq!(snap.processed_seq, 0, "cursor not advanced");
    assert_eq!(snap.attempts, 1);
    assert_eq!(snap.last_error.as_deref(), Some("transient tx failure"));
}

#[tokio::test]
async fn transactional_reject_creates_dead_letter_and_advances() {
    let db = setup_db("ch4_tx_reject").await;
    let t = make_default_test_outbox().await;
    t.outbox.register_queue(&db, "q", 1).await.unwrap();
    let pid = t.outbox.all_partition_ids()[0];

    enqueue_and_sequence(&t, &db, "q", 0, &["poison"]).await;

    run_transactional(&db, pid, AlwaysRejectTxHandler, 10).await;

    let snap = read_processor_state(&db, pid).await;
    assert_eq!(snap.processed_seq, 1, "cursor advanced past rejected msg");

    let dls = read_dead_letters(&db).await;
    assert_eq!(dls.len(), 1);
    assert!(dls[0].id > 0);
    assert_eq!(dls[0].partition_id, pid);
    assert_eq!(dls[0].seq, 1);
    assert_eq!(dls[0].last_error.as_deref(), Some("permanently bad tx"));
    assert_eq!(dls[0].payload, b"poison");
    assert_eq!(dls[0].payload_type, "text/plain");
    assert_eq!(dls[0].attempts, 0);
    assert_eq!(dls[0].status, "pending");
}

#[tokio::test]
async fn transactional_batch_processes_multiple_in_single_tx() {
    let db = setup_db("ch4_tx_batch").await;
    let t = make_default_test_outbox().await;
    t.outbox.register_queue(&db, "q", 1).await.unwrap();
    let pid = t.outbox.all_partition_ids()[0];

    enqueue_and_sequence(&t, &db, "q", 0, &["a", "b", "c"]).await;

    let count = Arc::new(AtomicU32::new(0));
    // CountingTxHandler counts the number of messages per call
    run_transactional(
        &db,
        pid,
        CountingTxHandler {
            count: count.clone(),
        },
        3,
    )
    .await;

    // Handler called once with all 3 messages
    assert_eq!(count.load(Ordering::Relaxed), 3);
}

// ======================================================================
// Chapter 5: Decoupled Processing
// ======================================================================

async fn run_leased(
    db: &Db,
    partition_id: i64,
    handler: impl LeasedHandler + 'static,
    lease_duration: Duration,
    msg_batch_size: u32,
) -> Option<super::strategy::ProcessResult> {
    let conn = db.sea_internal();
    let backend = conn.get_database_backend();
    drop(conn);

    let strategy = LeasedStrategy::new(
        Arc::new(handler),
        "test-AAAAAA".to_owned(),
        LeaseConfig {
            duration: lease_duration,
            headroom: Duration::from_secs(2),
        },
    );
    let tables = OutboxTables::default();
    let statements = super::statements::OutboxStatements::new(backend, &tables);
    let mailbox = super::subscription::Mailbox::new(
        "test-instance".to_owned(),
        super::subscription::Registry::new(),
    );
    let ctx = ProcessContext {
        db,
        store: OutboxStore::new(&statements),
        partition_id,
        mailbox: &mailbox,
    };
    strategy.process(&ctx, msg_batch_size).await.unwrap()
}

/// Idle poll cycles must not accumulate `attempts` on the processor row.
/// `lease_acquire` increments attempts as a crash trace, but `lease_release`
/// resets it to 0 when no messages are found. After N idle cycles, the
/// processor row should have `attempts = 0`, not `N`.
#[tokio::test]
async fn idle_poll_does_not_ratchet_attempts() {
    let db = setup_db("ch5_idle_attempts").await;
    let t = make_default_test_outbox().await;
    t.outbox.register_queue(&db, "q", 1).await.unwrap();
    let pid = t.outbox.all_partition_ids()[0];

    // No messages enqueued - every cycle is an idle poll.
    let count = Arc::new(AtomicU32::new(0));
    for _ in 0..5 {
        run_leased(
            &db,
            pid,
            CountingSuccessHandler {
                count: count.clone(),
            },
            Duration::from_secs(30),
            10,
        )
        .await;
    }

    // Handler should never have been called (no messages).
    assert_eq!(count.load(Ordering::Relaxed), 0);

    // Attempts must be 0 after idle cycles, not 5.
    let snap = read_processor_state(&db, pid).await;
    assert_eq!(
        snap.attempts, 0,
        "idle poll cycles should not accumulate attempts (crash trace is reset by lease_release)"
    );
}

#[tokio::test]
async fn decoupled_success_advances_cursor_and_releases_lease() {
    let db = setup_db("ch5_dc_success").await;
    let t = make_default_test_outbox().await;
    t.outbox.register_queue(&db, "q", 1).await.unwrap();
    let pid = t.outbox.all_partition_ids()[0];

    enqueue_and_sequence(&t, &db, "q", 0, &["a", "b"]).await;

    let count = Arc::new(AtomicU32::new(0));
    run_leased(
        &db,
        pid,
        CountingSuccessHandler {
            count: count.clone(),
        },
        Duration::from_secs(30),
        2,
    )
    .await;

    assert_eq!(count.load(Ordering::Relaxed), 2);
    let snap = read_processor_state(&db, pid).await;
    assert_eq!(snap.processed_seq, 2);
    assert_eq!(snap.attempts, 0);
    assert!(snap.locked_by.is_none(), "lease released");
    assert!(snap.locked_until.is_none(), "lease released");
}

#[tokio::test]
async fn decoupled_retry_preserves_cursor_and_releases_lease() {
    let db = setup_db("ch5_dc_retry").await;
    let t = make_default_test_outbox().await;
    t.outbox.register_queue(&db, "q", 1).await.unwrap();
    let pid = t.outbox.all_partition_ids()[0];

    enqueue_and_sequence(&t, &db, "q", 0, &["msg"]).await;

    run_leased(&db, pid, AlwaysRetryHandler, Duration::from_secs(30), 10).await;

    let snap = read_processor_state(&db, pid).await;
    assert_eq!(snap.processed_seq, 0, "cursor unchanged");
    assert_eq!(snap.attempts, 1, "attempts incremented by lease_acquire");
    assert_eq!(
        snap.last_error.as_deref(),
        Some("message handler returned Retry")
    );
    assert!(snap.locked_by.is_none(), "lease released");
}

#[tokio::test]
async fn decoupled_reject_creates_dead_letter_and_releases_lease() {
    let db = setup_db("ch5_dc_reject").await;
    let t = make_default_test_outbox().await;
    t.outbox.register_queue(&db, "q", 1).await.unwrap();
    let pid = t.outbox.all_partition_ids()[0];

    enqueue_and_sequence(&t, &db, "q", 0, &["bad"]).await;

    run_leased(&db, pid, AlwaysRejectHandler, Duration::from_secs(30), 10).await;

    let snap = read_processor_state(&db, pid).await;
    assert_eq!(snap.processed_seq, 1, "cursor advanced past rejected");
    assert!(snap.locked_by.is_none(), "lease released");

    let dls = read_dead_letters(&db).await;
    assert_eq!(dls.len(), 1);
    assert_eq!(dls[0].last_error.as_deref(), Some("permanently bad"));
}

#[tokio::test]
async fn decoupled_empty_partition_releases_lease() {
    let db = setup_db("ch5_dc_empty").await;
    let t = make_default_test_outbox().await;
    t.outbox.register_queue(&db, "q", 1).await.unwrap();
    let pid = t.outbox.all_partition_ids()[0];

    // No messages enqueued
    let count = Arc::new(AtomicU32::new(0));
    let result = run_leased(
        &db,
        pid,
        CountingSuccessHandler {
            count: count.clone(),
        },
        Duration::from_secs(30),
        10,
    )
    .await;

    assert!(result.is_none(), "no work done");
    assert_eq!(count.load(Ordering::Relaxed), 0);
    let snap = read_processor_state(&db, pid).await;
    assert!(snap.locked_by.is_none(), "lease released after empty");
}

#[tokio::test]
async fn decoupled_empty_partition_does_not_accumulate_attempts() {
    let db = setup_db("ch5_dc_empty_attempts").await;
    let t = make_default_test_outbox().await;
    t.outbox.register_queue(&db, "q", 1).await.unwrap();
    let pid = t.outbox.all_partition_ids()[0];

    // Run 5 empty lease cycles: acquire → empty → release
    for _ in 0..5 {
        let count = Arc::new(AtomicU32::new(0));
        run_leased(
            &db,
            pid,
            CountingSuccessHandler {
                count: count.clone(),
            },
            Duration::from_secs(30),
            10,
        )
        .await;
        assert_eq!(count.load(Ordering::Relaxed), 0);
    }

    // After 5 empty cycles, attempts should be 0 (reset on each release)
    let snap = read_processor_state(&db, pid).await;
    assert_eq!(
        snap.attempts, 0,
        "attempts should be 0 after empty lease cycles, not accumulated"
    );
}

#[tokio::test]
async fn decoupled_each_message_adapter_processes_individually() {
    let db = setup_db("ch5_dc_each").await;
    let t = make_default_test_outbox().await;
    t.outbox.register_queue(&db, "q", 1).await.unwrap();
    let pid = t.outbox.all_partition_ids()[0];

    enqueue_and_sequence(&t, &db, "q", 0, &["a", "b", "c"]).await;

    let count = Arc::new(AtomicU32::new(0));
    let handler = CountingMessageHandler {
        count: count.clone(),
    };
    run_leased(&db, pid, handler, Duration::from_secs(30), 3).await;

    // LeasedMessageHandler blanket impl calls handler once per message
    assert_eq!(count.load(Ordering::Relaxed), 3);
}

// ======================================================================
// Chapter 6: Crash Detection & Recovery
// ======================================================================

#[tokio::test]
async fn crash_leaves_incremented_attempts_in_db() {
    let db = setup_db("ch6_crash_trace").await;
    let t = make_default_test_outbox().await;
    t.outbox.register_queue(&db, "q", 1).await.unwrap();
    let pid = t.outbox.all_partition_ids()[0];

    enqueue_and_sequence(&t, &db, "q", 0, &["msg"]).await;

    // Simulate: lease acquired (attempts incremented in DB), then pod dies
    simulate_crash(&db, pid, 300).await;

    let snap = read_processor_state(&db, pid).await;
    assert_eq!(snap.attempts, 1, "crash left incremented attempts");
    assert_eq!(snap.processed_seq, 0, "cursor unchanged");
    assert!(snap.locked_by.is_some(), "lease still held by crashed pod");
}

#[tokio::test]
async fn recovery_after_crash_sees_nonzero_attempts() {
    let db = setup_db("ch6_recovery").await;
    let t = make_default_test_outbox().await;
    t.outbox.register_queue(&db, "q", 1).await.unwrap();
    let pid = t.outbox.all_partition_ids()[0];

    enqueue_and_sequence(&t, &db, "q", 0, &["msg"]).await;

    // Crash + expire lease so a new processor can acquire it
    simulate_crash(&db, pid, 300).await;
    expire_lease(&db, pid).await;

    // Recovery processor should see attempts=1 (from the crash)
    let seen = Arc::new(Mutex::new(Vec::new()));
    let handler = AttemptsRecorder {
        seen_attempts: seen.clone(),
    };
    run_leased(&db, pid, handler, Duration::from_secs(30), 10).await;

    {
        let recorded = seen.lock().unwrap();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0], 1, "handler sees attempts=1 from the crash");
    }

    // After success, attempts reset to 0
    let snap = read_processor_state(&db, pid).await;
    assert_eq!(snap.attempts, 0);
}

#[tokio::test]
async fn multiple_crashes_accumulate_attempts() {
    let db = setup_db("ch6_multi_crash").await;
    let t = make_default_test_outbox().await;
    t.outbox.register_queue(&db, "q", 1).await.unwrap();
    let pid = t.outbox.all_partition_ids()[0];

    enqueue_and_sequence(&t, &db, "q", 0, &["msg"]).await;

    // Two crashes
    simulate_crash(&db, pid, 300).await;
    expire_lease(&db, pid).await;
    simulate_crash(&db, pid, 300).await;
    expire_lease(&db, pid).await;

    let snap = read_processor_state(&db, pid).await;
    assert_eq!(snap.attempts, 2, "two crashes accumulated");
}

#[tokio::test]
async fn retry_does_not_double_increment_attempts() {
    let db = setup_db("ch6_no_double").await;
    let t = make_default_test_outbox().await;
    t.outbox.register_queue(&db, "q", 1).await.unwrap();
    let pid = t.outbox.all_partition_ids()[0];

    enqueue_and_sequence(&t, &db, "q", 0, &["msg"]).await;

    // lease_acquire increments attempts 0→1 in DB;
    // handler returns Retry; lease_record_retry does NOT increment again
    run_leased(&db, pid, AlwaysRetryHandler, Duration::from_secs(30), 10).await;

    let snap = read_processor_state(&db, pid).await;
    assert_eq!(snap.attempts, 1, "not 2 - retry doesn't double-increment");
}

#[tokio::test]
async fn success_after_crash_resets_attempts() {
    let db = setup_db("ch6_reset").await;
    let t = make_default_test_outbox().await;
    t.outbox.register_queue(&db, "q", 1).await.unwrap();
    let pid = t.outbox.all_partition_ids()[0];

    enqueue_and_sequence(&t, &db, "q", 0, &["msg"]).await;

    // Crash
    simulate_crash(&db, pid, 300).await;
    expire_lease(&db, pid).await;

    let snap = read_processor_state(&db, pid).await;
    assert_eq!(snap.attempts, 1);

    // Recovery succeeds
    let count = Arc::new(AtomicU32::new(0));
    run_leased(
        &db,
        pid,
        CountingSuccessHandler {
            count: count.clone(),
        },
        Duration::from_secs(30),
        10,
    )
    .await;

    let snap = read_processor_state(&db, pid).await;
    assert_eq!(snap.attempts, 0, "success resets attempts to 0");
    assert_eq!(snap.processed_seq, 1);
}

// ======================================================================
// Chapter 7: Backoff & Adaptive Batching
// ======================================================================

#[tokio::test]
async fn adaptive_batch_isolates_poison_message() {
    let db = setup_db("ch7_poison").await;
    let t = make_default_test_outbox().await;
    t.outbox.register_queue(&db, "q", 1).await.unwrap();
    let pid = t.outbox.all_partition_ids()[0];

    // The message at seq=2 is the poison pill.
    enqueue_and_sequence(&t, &db, "q", 0, &["ok1", "poison", "ok3", "ok4"]).await;

    // Demonstrate the adaptive batch isolation mechanism step by step
    // with batch_size=1 (the degraded size):

    // Process msg 1 (ok1) — success
    let r = run_leased(
        &db,
        pid,
        PoisonMessageHandler {
            poison_seqs: vec![2],
        },
        Duration::from_secs(30),
        1,
    )
    .await;
    assert!(matches!(r.unwrap().handler_result, HandlerResult::Success));

    // Process msg 2 (poison) — rejected via batch.reject(), dead-lettered in ack phase
    let r = run_leased(
        &db,
        pid,
        PoisonMessageHandler {
            poison_seqs: vec![2],
        },
        Duration::from_secs(30),
        1,
    )
    .await;
    // Leased blanket impl returns Success with rejections tracked in the batch
    assert!(matches!(r.unwrap().handler_result, HandlerResult::Success));

    // Process msg 3 (ok3) — success
    let r = run_leased(
        &db,
        pid,
        PoisonMessageHandler {
            poison_seqs: vec![2],
        },
        Duration::from_secs(30),
        1,
    )
    .await;
    assert!(matches!(r.unwrap().handler_result, HandlerResult::Success));

    // Process msg 4 (ok4) — success
    let r = run_leased(
        &db,
        pid,
        PoisonMessageHandler {
            poison_seqs: vec![2],
        },
        Duration::from_secs(30),
        1,
    )
    .await;
    assert!(matches!(r.unwrap().handler_result, HandlerResult::Success));

    let snap = read_processor_state(&db, pid).await;
    assert_eq!(snap.processed_seq, 4, "all 4 messages processed");

    let dls = read_dead_letters(&db).await;
    assert_eq!(dls.len(), 1, "only the poison message was dead-lettered");
    assert_eq!(dls[0].seq, 2);
}

// ======================================================================
// Chapter 8: Vacuum
// ======================================================================

/// Run the vacuum for a single partition: read `processed_seq`, delete
/// outgoing + body rows in batches, then reset the vacuum counter.
async fn run_vacuum(db: &Db, partition_id: i64) {
    run_vacuum_for_tables(db, &OutboxTables::default(), partition_id).await;
}

async fn run_vacuum_for_tables(db: &Db, tables: &OutboxTables, partition_id: i64) {
    #[derive(Debug, FromQueryResult)]
    struct ProcRow {
        processed_seq: i64,
    }

    let conn = db.sea_internal();
    let backend = conn.get_database_backend();
    let statements = super::statements::OutboxStatements::new(backend, tables);
    let store = OutboxStore::new(&statements);

    let proc_row = ProcRow::find_by_statement(Statement::from_sql_and_values(
        store.backend(),
        format!(
            "SELECT processed_seq FROM {} WHERE partition_id = $1",
            tables.processor()
        ),
        [partition_id.into()],
    ))
    .one(&conn)
    .await
    .unwrap()
    .unwrap();

    if proc_row.processed_seq == 0 {
        return;
    }

    let vacuum_sql = store.vacuum_cleanup();

    // Fetch outgoing rows in bounded chunks.
    loop {
        let rows = conn
            .query_all_raw(Statement::from_sql_and_values(
                store.backend(),
                &vacuum_sql.select_outgoing_chunk,
                [
                    partition_id.into(),
                    proc_row.processed_seq.into(),
                    10_000i64.into(),
                ],
            ))
            .await
            .unwrap();
        if rows.is_empty() {
            break;
        }
        let outgoing_ids: Vec<i64> = rows
            .iter()
            .map(|r| r.try_get_by_index::<i64>(0))
            .collect::<Result<_, _>>()
            .expect("outgoing id column");
        let body_ids: Vec<i64> = rows
            .iter()
            .map(|r| r.try_get_by_index::<i64>(1))
            .collect::<Result<_, _>>()
            .expect("body id column");
        // Delete outgoing by ID
        let del_out = store.build_delete_outgoing_batch(outgoing_ids.len());
        let values: Vec<sea_orm::Value> = outgoing_ids.iter().map(|&id| id.into()).collect();
        conn.execute_raw(Statement::from_sql_and_values(
            store.backend(),
            &del_out,
            values,
        ))
        .await
        .unwrap();
        // Delete body by ID
        if !body_ids.is_empty() {
            let del_body = store.build_delete_body_batch(body_ids.len());
            let values: Vec<sea_orm::Value> = body_ids.iter().map(|&id| id.into()).collect();
            conn.execute_raw(Statement::from_sql_and_values(
                store.backend(),
                &del_body,
                values,
            ))
            .await
            .unwrap();
        }
    }

    // Reset vacuum counter after cleanup.
    conn.execute_raw(Statement::from_sql_and_values(
        store.backend(),
        store.reset_vacuum_counter(),
        [partition_id.into()],
    ))
    .await
    .unwrap();
}

#[tokio::test]
async fn vacuum_deletes_processed_outgoing_and_body_rows() {
    let db = setup_db("ch8_vacuum_deletes").await;
    let t = make_default_test_outbox().await;
    t.outbox.register_queue(&db, "q", 1).await.unwrap();
    let pid = t.outbox.all_partition_ids()[0];

    enqueue_and_sequence(&t, &db, "q", 0, &["a", "b", "c"]).await;

    // Process all 3
    let count = Arc::new(AtomicU32::new(0));
    run_leased(
        &db,
        pid,
        CountingSuccessHandler {
            count: count.clone(),
        },
        Duration::from_secs(30),
        3,
    )
    .await;

    // Reap
    run_vacuum(&db, pid).await;

    assert_eq!(count_rows(&db, "toolkit_outbox_outgoing").await, 0);
    assert_eq!(count_rows(&db, "toolkit_outbox_body").await, 0);

    let snap = read_processor_state(&db, pid).await;
    assert_eq!(snap.processed_seq, 3, "cursor preserved");
}

#[tokio::test]
async fn vacuum_skips_when_processed_seq_is_zero() {
    let db = setup_db("ch8_vacuum_skip").await;
    let t = make_default_test_outbox().await;
    t.outbox.register_queue(&db, "q", 1).await.unwrap();
    let pid = t.outbox.all_partition_ids()[0];

    enqueue_and_sequence(&t, &db, "q", 0, &["a"]).await;

    // Don't process — cursor at 0
    run_vacuum(&db, pid).await;

    assert_eq!(
        count_rows(&db, "toolkit_outbox_outgoing").await,
        1,
        "rows preserved"
    );
}

#[tokio::test]
async fn vacuum_preserves_unprocessed_rows() {
    let db = setup_db("ch8_vacuum_preserves").await;
    let t = make_default_test_outbox().await;
    t.outbox.register_queue(&db, "q", 1).await.unwrap();
    let pid = t.outbox.all_partition_ids()[0];

    enqueue_and_sequence(&t, &db, "q", 0, &["a", "b", "c", "d", "e"]).await;

    // Process only 3 of 5 (batch_size=3)
    let count = Arc::new(AtomicU32::new(0));
    run_leased(
        &db,
        pid,
        CountingSuccessHandler {
            count: count.clone(),
        },
        Duration::from_secs(30),
        3,
    )
    .await;

    let snap = read_processor_state(&db, pid).await;
    assert_eq!(snap.processed_seq, 3);

    // Reap — should only delete seqs 1-3
    run_vacuum(&db, pid).await;

    let remaining = read_outgoing(&db, pid).await;
    assert_eq!(remaining.len(), 2);
    let seqs: Vec<i64> = remaining.iter().map(|r| r.seq).collect();
    assert_eq!(seqs, vec![4, 5]);
    for row in &remaining {
        assert_eq!(row.partition_id, pid);
        assert!(row.id > 0);
        assert!(row.body_id > 0);
    }
}

/// Read the vacuum counter for a partition.
async fn read_vacuum_counter(db: &Db, partition_id: i64) -> i64 {
    read_vacuum_counter_for_tables(db, &OutboxTables::default(), partition_id).await
}

async fn read_vacuum_counter_for_tables(db: &Db, tables: &OutboxTables, partition_id: i64) -> i64 {
    #[derive(Debug, FromQueryResult)]
    struct Row {
        counter: i64,
    }
    let conn = db.sea_internal();
    Row::find_by_statement(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        format!(
            "SELECT counter FROM {} WHERE partition_id = $1",
            tables.vacuum_counter()
        ),
        [partition_id.into()],
    ))
    .one(&conn)
    .await
    .expect("query")
    .expect("vacuum counter row")
    .counter
}

/// Set the vacuum counter to an arbitrary value (test helper).
async fn set_vacuum_counter(db: &Db, partition_id: i64, value: i64) {
    set_vacuum_counter_for_tables(db, &OutboxTables::default(), partition_id, value).await;
}

async fn set_vacuum_counter_for_tables(
    db: &Db,
    tables: &OutboxTables,
    partition_id: i64,
    value: i64,
) {
    let conn = db.sea_internal();
    conn.execute_raw(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        format!(
            "UPDATE {} SET counter = $1 WHERE partition_id = $2",
            tables.vacuum_counter()
        ),
        [value.into(), partition_id.into()],
    ))
    .await
    .unwrap();
}

#[tokio::test]
async fn vacuum_counter_bumped_on_processed_seq_advance() {
    let db = setup_db("ch8_counter_bump").await;
    let t = make_default_test_outbox().await;
    t.outbox.register_queue(&db, "q", 1).await.unwrap();
    let pid = t.outbox.all_partition_ids()[0];

    // Counter starts at 0.
    assert_eq!(read_vacuum_counter(&db, pid).await, 0);

    enqueue_and_sequence(&t, &db, "q", 0, &["a", "b"]).await;

    // Process batch of 2 — counter should bump once (one ack).
    let count = Arc::new(AtomicU32::new(0));
    run_leased(
        &db,
        pid,
        CountingSuccessHandler {
            count: count.clone(),
        },
        Duration::from_secs(30),
        2,
    )
    .await;

    assert_eq!(read_vacuum_counter(&db, pid).await, 1);

    // Process again (no messages) — counter should not change.
    run_leased(
        &db,
        pid,
        CountingSuccessHandler {
            count: count.clone(),
        },
        Duration::from_secs(30),
        2,
    )
    .await;

    assert_eq!(read_vacuum_counter(&db, pid).await, 1);
}

#[tokio::test]
async fn vacuum_counter_preserves_concurrent_bumps() {
    let db = setup_db("ch8_counter_concurrent").await;
    let t = make_default_test_outbox().await;
    t.outbox.register_queue(&db, "q", 1).await.unwrap();
    let pid = t.outbox.all_partition_ids()[0];

    enqueue_and_sequence(&t, &db, "q", 0, &["a", "b", "c"]).await;

    // Process all 3 — counter = 1 (one ack of batch=3).
    let count = Arc::new(AtomicU32::new(0));
    run_leased(
        &db,
        pid,
        CountingSuccessHandler {
            count: count.clone(),
        },
        Duration::from_secs(30),
        3,
    )
    .await;

    assert_eq!(read_vacuum_counter(&db, pid).await, 1);

    // Simulate a concurrent processor bump (as if more messages were processed
    // while vacuum was running): manually set counter to 3.
    set_vacuum_counter(&db, pid, 3).await;

    // Vacuum with snapshot_counter=3 — deletes rows, decrements by 3.
    // After decrement: counter = GREATEST(3 - 3, 0) = 0.
    run_vacuum(&db, pid).await;

    assert_eq!(count_rows(&db, "toolkit_outbox_outgoing").await, 0);
    assert_eq!(read_vacuum_counter(&db, pid).await, 0);
}

#[tokio::test]
async fn vacuum_stale_counter_reset() {
    let db = setup_db("ch8_stale_counter").await;
    let t = make_default_test_outbox().await;
    t.outbox.register_queue(&db, "q", 1).await.unwrap();
    let pid = t.outbox.all_partition_ids()[0];

    // Create and process a message so processed_seq > 0.
    enqueue_and_sequence(&t, &db, "q", 0, &["a"]).await;
    let count = Arc::new(AtomicU32::new(0));
    run_leased(
        &db,
        pid,
        CountingSuccessHandler {
            count: count.clone(),
        },
        Duration::from_secs(30),
        10,
    )
    .await;

    // Vacuum to clean up — counter goes to 0.
    run_vacuum(&db, pid).await;
    assert_eq!(read_vacuum_counter(&db, pid).await, 0);
    assert_eq!(count_rows(&db, "toolkit_outbox_outgoing").await, 0);

    // Simulate stale counter (as if crash prevented decrement).
    set_vacuum_counter(&db, pid, 5).await;

    // Vacuum runs again: processed_seq > 0 but no outgoing rows → stale.
    // The run_vacuum helper resets counter after cleanup (0 rows deleted).
    run_vacuum(&db, pid).await;

    assert_eq!(read_vacuum_counter(&db, pid).await, 0);
}

#[tokio::test]
async fn vacuum_counter_row_created_on_register_queue() {
    let db = setup_db("ch8_counter_register").await;
    let t = make_default_test_outbox().await;
    t.outbox.register_queue(&db, "q", 2).await.unwrap();

    let pids = t.outbox.all_partition_ids();
    assert_eq!(pids.len(), 2);

    // Both partitions should have vacuum counter rows with counter = 0.
    for &pid in &pids {
        assert_eq!(read_vacuum_counter(&db, pid).await, 0);
    }

    // Re-register (idempotent) — should not fail.
    t.outbox.register_queue(&db, "q", 2).await.unwrap();

    // Counters still 0.
    for &pid in &pids {
        assert_eq!(read_vacuum_counter(&db, pid).await, 0);
    }
}

// ======================================================================
// Chapter 9: Dead Letters
// ======================================================================

/// Helper: enqueue, sequence, and reject N messages to create dead letters.
async fn create_dead_letters(
    t: &TestOutbox,
    db: &Db,
    queue: &str,
    partition: u32,
    payloads: &[&str],
) {
    enqueue_and_sequence(t, db, queue, partition, payloads).await;
    let ids = t.outbox.all_partition_ids();
    let pid = ids[partition as usize];
    run_leased(
        db,
        pid,
        AlwaysRejectHandler,
        Duration::from_secs(30),
        u32::try_from(payloads.len()).unwrap(),
    )
    .await;
}

#[tokio::test]
async fn dead_letter_list_returns_correct_fields() {
    let db = setup_db("ch9_dl_list").await;
    let t = make_default_test_outbox().await;
    t.outbox.register_queue(&db, "q", 1).await.unwrap();
    let pid = t.outbox.all_partition_ids()[0];

    create_dead_letters(&t, &db, "q", 0, &["a", "b", "c"]).await;

    let items = t
        .outbox
        .dead_letter_list(&db.conn().unwrap(), &DeadLetterFilter::default())
        .await
        .unwrap();
    assert_eq!(items.len(), 3);
    for item in &items {
        assert_eq!(item.partition_id, pid);
        assert_eq!(item.last_error.as_deref(), Some("permanently bad"));
        assert_eq!(item.status, super::dead_letter::DeadLetterStatus::Pending);
        assert!(item.completed_at.is_none());
    }
}

#[tokio::test]
async fn dead_letter_count_matches_list() {
    let db = setup_db("ch9_dl_count").await;
    let t = make_default_test_outbox().await;
    t.outbox.register_queue(&db, "q", 1).await.unwrap();

    create_dead_letters(&t, &db, "q", 0, &["a", "b", "c"]).await;

    let count = t
        .outbox
        .dead_letter_count(&db.conn().unwrap(), &DeadLetterFilter::default())
        .await
        .unwrap();
    assert_eq!(count, 3);
}

#[tokio::test]
async fn dead_letter_replay_claims_and_sets_reprocessing() {
    let db = setup_db("ch9_dl_replay").await;
    let t = make_default_test_outbox().await;
    t.outbox.register_queue(&db, "q", 1).await.unwrap();

    create_dead_letters(&t, &db, "q", 0, &["msg"]).await;

    let replayed = t
        .outbox
        .dead_letter_replay(
            &db.conn().unwrap(),
            &DeadLetterScope::default(),
            Duration::from_mins(1),
        )
        .await
        .unwrap();
    assert_eq!(replayed.len(), 1);

    // Dead letter now has status=reprocessing and a deadline
    let dls = read_dead_letters(&db).await;
    assert_eq!(dls.len(), 1);
    assert_eq!(dls[0].status, "reprocessing");
    assert!(dls[0].deadline.is_some());
}

#[tokio::test]
async fn dead_letter_full_replay_roundtrip() {
    let db = setup_db("ch9_dl_roundtrip").await;
    let t = make_default_test_outbox().await;
    t.outbox.register_queue(&db, "q", 1).await.unwrap();

    // Reject
    create_dead_letters(&t, &db, "q", 0, &["msg"]).await;

    // Replay (claim) → resolve
    let replayed = t
        .outbox
        .dead_letter_replay(
            &db.conn().unwrap(),
            &DeadLetterScope::default(),
            Duration::from_mins(1),
        )
        .await
        .unwrap();
    assert_eq!(replayed.len(), 1);

    let ids: Vec<i64> = replayed.iter().map(|m| m.id).collect();
    let resolved = t
        .outbox
        .dead_letter_resolve(&db.conn().unwrap(), &ids)
        .await
        .unwrap();
    assert_eq!(resolved, 1);

    // Dead letter is now resolved
    let dls = read_dead_letters(&db).await;
    assert_eq!(dls.len(), 1);
    assert_eq!(dls[0].status, "resolved");
    assert!(dls[0].completed_at.is_some());
}

#[tokio::test]
async fn dead_letter_cleanup_only_terminal() {
    let db = setup_db("ch9_dl_cleanup_soft").await;
    let t = make_default_test_outbox().await;
    t.outbox.register_queue(&db, "q", 1).await.unwrap();

    // Create 2 dead letters
    create_dead_letters(&t, &db, "q", 0, &["a", "b"]).await;

    // Replay only 1 (by limit), then resolve it
    let scope_one = DeadLetterScope::default().limit(1);
    let replayed = t
        .outbox
        .dead_letter_replay(&db.conn().unwrap(), &scope_one, Duration::from_mins(1))
        .await
        .unwrap();
    let ids: Vec<i64> = replayed.iter().map(|m| m.id).collect();
    t.outbox
        .dead_letter_resolve(&db.conn().unwrap(), &ids)
        .await
        .unwrap();

    // Cleanup — should only delete the resolved one
    let deleted = t
        .outbox
        .dead_letter_cleanup(&db.conn().unwrap(), &DeadLetterScope::default())
        .await
        .unwrap();
    assert_eq!(deleted, 1);

    // 1 pending dead letter remains
    let remaining = t
        .outbox
        .dead_letter_count(&db.conn().unwrap(), &DeadLetterFilter::default())
        .await
        .unwrap();
    assert_eq!(remaining, 1);
}

#[tokio::test]
async fn dead_letter_discard_then_cleanup_deletes_all() {
    let db = setup_db("ch9_dl_discard_cleanup").await;
    let t = make_default_test_outbox().await;
    t.outbox.register_queue(&db, "q", 1).await.unwrap();

    create_dead_letters(&t, &db, "q", 0, &["a", "b", "c"]).await;

    // Discard all pending
    let discarded = t
        .outbox
        .dead_letter_discard(&db.conn().unwrap(), &DeadLetterScope::default())
        .await
        .unwrap();
    assert_eq!(discarded, 3);

    // Cleanup terminal entries
    let cleaned = t
        .outbox
        .dead_letter_cleanup(&db.conn().unwrap(), &DeadLetterScope::default())
        .await
        .unwrap();
    assert_eq!(cleaned, 3);

    let remaining = t
        .outbox
        .dead_letter_count(
            &db.conn().unwrap(),
            &DeadLetterFilter::default().any_status(),
        )
        .await
        .unwrap();
    assert_eq!(remaining, 0);
}

#[tokio::test]
async fn dead_letter_filter_by_partition() {
    let db = setup_db("ch9_dl_filter_part").await;
    let t = make_default_test_outbox().await;
    t.outbox.register_queue(&db, "q", 2).await.unwrap();
    let ids = t.outbox.all_partition_ids();

    // Dead-letter messages on both partitions
    create_dead_letters(&t, &db, "q", 0, &["a0"]).await;
    create_dead_letters(&t, &db, "q", 1, &["b1", "b2"]).await;

    let filter_p0 = DeadLetterFilter::default().partition(ids[0]);
    let items = t
        .outbox
        .dead_letter_list(&db.conn().unwrap(), &filter_p0)
        .await
        .unwrap();
    assert_eq!(items.len(), 1);

    let filter_p1 = DeadLetterFilter::default().partition(ids[1]);
    let items = t
        .outbox
        .dead_letter_list(&db.conn().unwrap(), &filter_p1)
        .await
        .unwrap();
    assert_eq!(items.len(), 2);
}

#[tokio::test]
async fn dead_letter_filter_with_limit() {
    let db = setup_db("ch9_dl_filter_limit").await;
    let t = make_default_test_outbox().await;
    t.outbox.register_queue(&db, "q", 1).await.unwrap();

    create_dead_letters(&t, &db, "q", 0, &["a", "b", "c", "d", "e"]).await;

    let filter = DeadLetterFilter::default().limit(2);
    let items = t
        .outbox
        .dead_letter_list(&db.conn().unwrap(), &filter)
        .await
        .unwrap();
    assert_eq!(items.len(), 2);
}

// ======================================================================
// Chapter 10: Builder API
// ======================================================================

/// Graceful shutdown: handler is mid-batch when `stop()` is called.
/// The current batch completes (all messages processed and acked),
/// then the processor stops. No messages are lost or left unacked.
#[tokio::test]
async fn graceful_shutdown_completes_current_batch() {
    struct SlowHandler {
        processed: Arc<AtomicU32>,
        entered: Arc<tokio::sync::Notify>,
    }

    #[async_trait::async_trait]
    impl LeasedMessageHandler for SlowHandler {
        async fn handle(&self, _msg: &OutboxMessage) -> MessageResult {
            self.entered.notify_one();
            tokio::time::sleep(Duration::from_millis(50)).await;
            self.processed.fetch_add(1, Ordering::SeqCst);
            MessageResult::Ok
        }
    }

    let db = setup_db("ch10_graceful_shutdown").await;
    let processed = Arc::new(AtomicU32::new(0));
    let handler_entered = Arc::new(tokio::sync::Notify::new());

    let handle = Outbox::builder(db.clone())
        .processor_tuning(
            WorkerTuning::processor_default()
                .idle_interval(Duration::from_millis(50))
                .batch_size(5),
        )
        .sequencer_tuning(
            WorkerTuning::sequencer_default().idle_interval(Duration::from_millis(50)),
        )
        .queue("q", Partitions::of(1))
        .leased(SlowHandler {
            processed: processed.clone(),
            entered: handler_entered.clone(),
        })
        .start()
        .await
        .unwrap();

    let outbox = handle.outbox();

    let db2 = setup_db("ch10_graceful_shutdown").await;
    let conn = db2.conn().unwrap();
    for i in 0..3 {
        outbox
            .enqueue(
                &conn,
                Record::to("q", 0)
                    .payload(format!("msg-{i}").into_bytes(), "text/plain")
                    .build()
                    .unwrap(),
            )
            .await
            .unwrap();
    }
    outbox.flush();

    // Wait for at least 1 message to be processed (handler is active)
    tokio::time::timeout(Duration::from_secs(5), handler_entered.notified())
        .await
        .expect("handler should start processing within 5s");

    // Messages enqueued while the handler is active are picked up by the
    // current or the next batch cycle.
    for i in 3..6 {
        outbox
            .enqueue(
                &conn,
                Record::to("q", 0)
                    .payload(format!("msg-{i}").into_bytes(), "text/plain")
                    .build()
                    .unwrap(),
            )
            .await
            .unwrap();
    }
    outbox.flush();

    // Wait for all 6 messages to be processed
    poll_until(
        || {
            let c = processed.load(Ordering::SeqCst);
            async move { c >= 6 }
        },
        5000,
    )
    .await;

    // All 6 pre-stop messages processed.
    assert!(
        processed.load(Ordering::SeqCst) >= 6,
        "all pre-stop messages should be processed: got {}",
        processed.load(Ordering::SeqCst)
    );

    // Stop the pipeline. Must return without hanging.
    let stop_result = tokio::time::timeout(Duration::from_secs(5), handle.stop()).await;
    assert!(stop_result.is_ok(), "stop() should complete within 5s");

    // No further processing after stop.
    let after_stop = processed.load(Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        processed.load(Ordering::SeqCst),
        after_stop,
        "no messages should be processed after stop()"
    );
}

#[tokio::test]
async fn builder_start_stop_clean() {
    let db = setup_db("ch10_start_stop").await;

    let count = Arc::new(AtomicU32::new(0));
    let handler = CountingMessageHandler {
        count: count.clone(),
    };
    let handle = Outbox::builder(db)
        .processor_tuning(
            WorkerTuning::processor_default().idle_interval(Duration::from_millis(50)),
        )
        .sequencer_tuning(
            WorkerTuning::sequencer_default().idle_interval(Duration::from_millis(50)),
        )
        .queue("orders", Partitions::of(1))
        .leased(handler)
        .start()
        .await
        .unwrap();

    // Just verify it started and can stop cleanly
    handle.stop().await;
}

#[tokio::test]
async fn builder_partitions_of_all_valid_values() {
    for n in [1, 2, 4, 8, 16, 32, 64] {
        let p = Partitions::of(n);
        assert_eq!(p.count(), n);
    }
}

#[tokio::test]
async fn default_prefix_builder_enqueue_uses_default_tables() {
    let db = setup_db("ch10_default_prefix_builder").await;
    let t = make_default_test_outbox().await;
    t.outbox.register_queue(&db, "q", 1).await.unwrap();

    t.outbox
        .enqueue(
            &db.conn().unwrap(),
            Record::to("q", 0)
                .payload(b"default".to_vec(), "text/plain")
                .build()
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(count_rows(&db, "toolkit_outbox_body").await, 1);
    assert_eq!(count_rows(&db, "toolkit_outbox_incoming").await, 1);
}

#[tokio::test]
async fn custom_prefix_builder_registers_queue() {
    let tables = OutboxTables::new("mini_chat_outbox").unwrap();
    let db = setup_db_with_migrations(
        "ch10_custom_prefix_register",
        super::outbox_migrations_with_prefix(tables.prefix()).unwrap(),
    )
    .await;

    let handle = Outbox::builder(db.clone())
        .table_prefix(tables.prefix())
        .unwrap()
        .queue("q", Partitions::of(1))
        .leased(CountingMessageHandler {
            count: Arc::new(AtomicU32::new(0)),
        })
        .start()
        .await
        .unwrap();

    assert_eq!(count_rows(&db, tables.partitions()).await, 1);
    assert_eq!(count_rows(&db, tables.processor()).await, 1);
    assert!(!table_exists(&db, "toolkit_outbox_partitions").await);

    handle.stop().await;
}

#[tokio::test]
async fn custom_prefix_containing_default_body_token_registers_and_enqueues() {
    let tables = OutboxTables::new("toolkit_outbox_body").unwrap();
    let db = setup_db_with_migrations(
        "ch10_custom_prefix_body_token",
        super::outbox_migrations_with_prefix(tables.prefix()).unwrap(),
    )
    .await;

    let handle = Outbox::builder(db.clone())
        .table_prefix(tables.prefix())
        .unwrap()
        .queue("q", Partitions::of(1))
        .leased(CountingMessageHandler {
            count: Arc::new(AtomicU32::new(0)),
        })
        .start()
        .await
        .unwrap();

    let message_id = handle
        .outbox()
        .enqueue(
            &db.conn().unwrap(),
            Record::to("q", 0)
                .payload(b"nasty-prefix".to_vec(), "text/plain")
                .build()
                .unwrap(),
        )
        .await
        .unwrap();

    assert!(message_id.0 > 0);
    assert_eq!(count_rows(&db, tables.body()).await, 1);
    assert!(!table_exists(&db, "toolkit_outbox_body").await);
    assert!(!table_exists(&db, "toolkit_outbox_body_body_dead_letters").await);

    handle.stop().await;
}

#[tokio::test]
async fn custom_prefix_processes_message_without_default_tables() {
    let tables = OutboxTables::new("mini_chat_outbox").unwrap();
    let db = setup_db_with_migrations(
        "ch10_custom_prefix_process",
        super::outbox_migrations_with_prefix(tables.prefix()).unwrap(),
    )
    .await;
    let processed = Arc::new(AtomicU32::new(0));

    let handle = Outbox::builder(db.clone())
        .table_prefix(tables.prefix())
        .unwrap()
        .processor_tuning(
            WorkerTuning::processor_default().idle_interval(Duration::from_millis(20)),
        )
        .sequencer_tuning(
            WorkerTuning::sequencer_default().idle_interval(Duration::from_millis(20)),
        )
        .queue("q", Partitions::of(1))
        .leased(CountingMessageHandler {
            count: processed.clone(),
        })
        .start()
        .await
        .unwrap();

    let outbox = handle.outbox();
    outbox
        .enqueue(
            &db.conn().unwrap(),
            Record::to("q", 0)
                .payload(b"custom".to_vec(), "text/plain")
                .build()
                .unwrap(),
        )
        .await
        .unwrap();
    outbox.flush();

    poll_until(
        || {
            let processed = processed.load(Ordering::Relaxed);
            async move { processed >= 1 }
        },
        5000,
    )
    .await;

    assert_eq!(count_rows(&db, tables.body()).await, 1);
    assert_eq!(count_rows(&db, tables.outgoing()).await, 1);
    assert!(!table_exists(&db, "toolkit_outbox_body").await);

    handle.stop().await;
}

#[tokio::test]
async fn custom_prefix_dead_letter_uses_custom_table() {
    let tables = OutboxTables::new("mini_chat_outbox").unwrap();
    let db = setup_db_with_migrations(
        "ch10_custom_prefix_dead_letter",
        super::outbox_migrations_with_prefix(tables.prefix()).unwrap(),
    )
    .await;

    let handle = Outbox::builder(db.clone())
        .table_prefix(tables.prefix())
        .unwrap()
        .processor_tuning(
            WorkerTuning::processor_default().idle_interval(Duration::from_millis(20)),
        )
        .sequencer_tuning(
            WorkerTuning::sequencer_default().idle_interval(Duration::from_millis(20)),
        )
        .queue("q", Partitions::of(1))
        .leased(AlwaysRejectHandler)
        .start()
        .await
        .unwrap();

    let outbox = handle.outbox();
    outbox
        .enqueue(
            &db.conn().unwrap(),
            Record::to("q", 0)
                .payload(b"reject".to_vec(), "text/plain")
                .build()
                .unwrap(),
        )
        .await
        .unwrap();
    outbox.flush();

    poll_until(
        || {
            let db = db.clone();
            let table = tables.dead_letters().to_owned();
            async move { count_rows(&db, &table).await >= 1 }
        },
        5000,
    )
    .await;

    assert_eq!(count_rows(&db, tables.dead_letters()).await, 1);
    assert!(!table_exists(&db, "toolkit_outbox_dead_letters").await);

    handle.stop().await;
}

#[tokio::test]
async fn custom_prefix_vacuum_cleans_custom_tables() {
    use super::workers::vacuum::VacuumTask;

    let tables = OutboxTables::new("mini_chat_outbox").unwrap();
    let db = setup_db_with_migrations(
        "ch10_custom_prefix_vacuum",
        super::outbox_migrations_with_prefix(tables.prefix()).unwrap(),
    )
    .await;
    let processed = Arc::new(AtomicU32::new(0));

    let handle = Outbox::builder(db.clone())
        .table_prefix(tables.prefix())
        .unwrap()
        .processor_tuning(
            WorkerTuning::processor_default().idle_interval(Duration::from_millis(20)),
        )
        .sequencer_tuning(
            WorkerTuning::sequencer_default().idle_interval(Duration::from_millis(20)),
        )
        .queue("q", Partitions::of(1))
        .leased(CountingMessageHandler {
            count: processed.clone(),
        })
        .start()
        .await
        .unwrap();

    let outbox = handle.outbox();
    outbox
        .enqueue(
            &db.conn().unwrap(),
            Record::to("q", 0)
                .payload(b"vacuum".to_vec(), "text/plain")
                .build()
                .unwrap(),
        )
        .await
        .unwrap();
    outbox.flush();

    poll_until(
        || {
            let processed = processed.load(Ordering::Relaxed);
            async move { processed >= 1 }
        },
        5000,
    )
    .await;

    handle.stop().await;

    assert_eq!(count_rows(&db, tables.outgoing()).await, 1);
    assert_eq!(count_rows(&db, tables.body()).await, 1);

    let cancel = CancellationToken::new();
    let statements = Arc::new(super::statements::OutboxStatements::new(
        db.sea_internal().get_database_backend(),
        &tables,
    ));
    let mut vacuum = VacuumTask::new(db.clone(), statements, 10_000, test_collectable_traces());
    vacuum.execute(&cancel).await.unwrap();

    assert_eq!(count_rows(&db, tables.outgoing()).await, 0);
    assert_eq!(count_rows(&db, tables.body()).await, 0);
    assert!(!table_exists(&db, "toolkit_outbox_outgoing").await);
}

#[tokio::test]
async fn default_and_custom_prefix_instances_coexist() {
    let custom_tables = OutboxTables::new("mini_chat_outbox").unwrap();
    let mut migrations = super::outbox_migrations();
    migrations.extend(super::outbox_migrations_with_prefix(custom_tables.prefix()).unwrap());
    let db = setup_db_with_migrations("ch10_prefix_instances_coexist", migrations).await;

    let default_count = Arc::new(AtomicU32::new(0));
    let custom_count = Arc::new(AtomicU32::new(0));

    let default_handle = Outbox::builder(db.clone())
        .processor_tuning(
            WorkerTuning::processor_default().idle_interval(Duration::from_millis(20)),
        )
        .sequencer_tuning(
            WorkerTuning::sequencer_default().idle_interval(Duration::from_millis(20)),
        )
        .queue("q", Partitions::of(1))
        .leased(CountingMessageHandler {
            count: default_count.clone(),
        })
        .start()
        .await
        .unwrap();

    let custom_handle = Outbox::builder(db.clone())
        .table_prefix(custom_tables.prefix())
        .unwrap()
        .processor_tuning(
            WorkerTuning::processor_default().idle_interval(Duration::from_millis(20)),
        )
        .sequencer_tuning(
            WorkerTuning::sequencer_default().idle_interval(Duration::from_millis(20)),
        )
        .queue("q", Partitions::of(1))
        .leased(CountingMessageHandler {
            count: custom_count.clone(),
        })
        .start()
        .await
        .unwrap();

    default_handle
        .outbox()
        .enqueue(
            &db.conn().unwrap(),
            Record::to("q", 0)
                .payload(b"default".to_vec(), "text/plain")
                .build()
                .unwrap(),
        )
        .await
        .unwrap();
    custom_handle
        .outbox()
        .enqueue(
            &db.conn().unwrap(),
            Record::to("q", 0)
                .payload(b"custom".to_vec(), "text/plain")
                .build()
                .unwrap(),
        )
        .await
        .unwrap();
    default_handle.outbox().flush();
    custom_handle.outbox().flush();

    poll_until(
        || {
            let default_count = default_count.load(Ordering::Relaxed);
            let custom_count = custom_count.load(Ordering::Relaxed);
            async move { default_count >= 1 && custom_count >= 1 }
        },
        5000,
    )
    .await;

    assert_eq!(count_rows(&db, "toolkit_outbox_body").await, 1);
    assert_eq!(count_rows(&db, custom_tables.body()).await, 1);

    default_handle.stop().await;
    custom_handle.stop().await;
}

#[tokio::test]
async fn custom_prefix_without_matching_migration_fails_startup() {
    let db = setup_empty_db("ch10_custom_prefix_missing_migration").await;
    let result = Outbox::builder(db)
        .table_prefix("mini_chat_outbox")
        .unwrap()
        .queue("q", Partitions::of(1))
        .leased(CountingMessageHandler {
            count: Arc::new(AtomicU32::new(0)),
        })
        .start()
        .await;
    let err = match result {
        Ok(handle) => {
            handle.stop().await;
            panic!("expected startup to fail without matching migrations");
        }
        Err(err) => err,
    };

    assert!(matches!(err, OutboxError::Database(_)));
}

#[tokio::test]
async fn builder_multiple_queues() {
    let db = setup_db("ch10_multi_queue").await;

    let count_a = Arc::new(AtomicU32::new(0));
    let count_b = Arc::new(AtomicU32::new(0));

    let handle = Outbox::builder(db)
        .processor_tuning(
            WorkerTuning::processor_default().idle_interval(Duration::from_millis(50)),
        )
        .sequencer_tuning(
            WorkerTuning::sequencer_default().idle_interval(Duration::from_millis(50)),
        )
        .queue("a", Partitions::of(1))
        .leased(CountingMessageHandler {
            count: count_a.clone(),
        })
        .queue("b", Partitions::of(2))
        .leased(CountingMessageHandler {
            count: count_b.clone(),
        })
        .start()
        .await
        .unwrap();

    let outbox = handle.outbox();

    let db2 = setup_db("ch10_multi_queue").await;
    let conn = db2.conn().unwrap();
    outbox
        .enqueue(
            &conn,
            Record::to("a", 0)
                .payload(b"hello-a".to_vec(), "text/plain")
                .build()
                .unwrap(),
        )
        .await
        .unwrap();
    outbox
        .enqueue(
            &conn,
            Record::to("b", 0)
                .payload(b"hello-b".to_vec(), "text/plain")
                .build()
                .unwrap(),
        )
        .await
        .unwrap();
    outbox.flush();

    // Wait for processing
    poll_until(
        || {
            let ca = count_a.load(Ordering::Relaxed);
            let cb = count_b.load(Ordering::Relaxed);
            async move { ca >= 1 && cb >= 1 }
        },
        5000,
    )
    .await;

    assert!(count_a.load(Ordering::Relaxed) >= 1);
    assert!(count_b.load(Ordering::Relaxed) >= 1);

    handle.stop().await;
}

// ======================================================================
// Chapter 11: End-to-End Lifecycle
// ======================================================================

#[tokio::test]
async fn e2e_happy_path_enqueue_through_reap() {
    let db = setup_db("ch11_happy").await;
    let t = make_default_test_outbox().await;
    t.outbox.register_queue(&db, "q", 1).await.unwrap();
    let pid = t.outbox.all_partition_ids()[0];

    // Record → Sequence → Process (decoupled, success) → Reap
    enqueue_and_sequence(&t, &db, "q", 0, &["a", "b", "c"]).await;

    let count = Arc::new(AtomicU32::new(0));
    run_leased(
        &db,
        pid,
        CountingSuccessHandler {
            count: count.clone(),
        },
        Duration::from_secs(30),
        3,
    )
    .await;
    run_vacuum(&db, pid).await;

    assert_eq!(count_rows(&db, "toolkit_outbox_incoming").await, 0);
    assert_eq!(count_rows(&db, "toolkit_outbox_outgoing").await, 0);
    assert_eq!(count_rows(&db, "toolkit_outbox_body").await, 0);
    assert_eq!(count_rows(&db, "toolkit_outbox_dead_letters").await, 0);

    let snap = read_processor_state(&db, pid).await;
    assert_eq!(snap.processed_seq, 3);
    assert_eq!(snap.attempts, 0);
}

#[tokio::test]
async fn e2e_retry_then_recovery() {
    let db = setup_db("ch11_retry").await;
    let t = make_default_test_outbox().await;
    t.outbox.register_queue(&db, "q", 1).await.unwrap();
    let pid = t.outbox.all_partition_ids()[0];

    enqueue_and_sequence(&t, &db, "q", 0, &["msg"]).await;

    // Retry twice
    run_leased(&db, pid, AlwaysRetryHandler, Duration::from_secs(30), 10).await;
    expire_lease(&db, pid).await;
    run_leased(&db, pid, AlwaysRetryHandler, Duration::from_secs(30), 10).await;
    expire_lease(&db, pid).await;

    let snap = read_processor_state(&db, pid).await;
    assert_eq!(snap.processed_seq, 0);
    assert_eq!(snap.attempts, 2);

    // Then succeed
    let count = Arc::new(AtomicU32::new(0));
    run_leased(
        &db,
        pid,
        CountingSuccessHandler {
            count: count.clone(),
        },
        Duration::from_secs(30),
        10,
    )
    .await;

    let snap = read_processor_state(&db, pid).await;
    assert_eq!(snap.processed_seq, 1);
    assert_eq!(snap.attempts, 0, "attempts reset on success");
}

#[tokio::test]
async fn e2e_reject_replay_success() {
    let db = setup_db("ch11_reject_replay").await;
    let t = make_default_test_outbox().await;
    t.outbox.register_queue(&db, "q", 1).await.unwrap();

    // Reject
    create_dead_letters(&t, &db, "q", 0, &["msg"]).await;

    // Replay (claim) → resolve
    let replayed = t
        .outbox
        .dead_letter_replay(
            &db.conn().unwrap(),
            &DeadLetterScope::default(),
            Duration::from_mins(1),
        )
        .await
        .unwrap();
    assert_eq!(replayed.len(), 1);

    let ids: Vec<i64> = replayed.iter().map(|m| m.id).collect();
    t.outbox
        .dead_letter_resolve(&db.conn().unwrap(), &ids)
        .await
        .unwrap();

    // Dead letter has status=resolved and completed_at set
    let dls = read_dead_letters(&db).await;
    assert_eq!(dls.len(), 1);
    assert_eq!(dls[0].status, "resolved");
    assert!(dls[0].completed_at.is_some());
}

#[tokio::test]
async fn e2e_crash_then_recovery() {
    let db = setup_db("ch11_crash").await;
    let t = make_default_test_outbox().await;
    t.outbox.register_queue(&db, "q", 1).await.unwrap();
    let pid = t.outbox.all_partition_ids()[0];

    enqueue_and_sequence(&t, &db, "q", 0, &["msg"]).await;

    // Simulate crash
    simulate_crash(&db, pid, 300).await;
    expire_lease(&db, pid).await;

    // Recovery processor succeeds
    let seen = Arc::new(Mutex::new(Vec::new()));
    let handler = AttemptsRecorder {
        seen_attempts: seen.clone(),
    };
    run_leased(&db, pid, handler, Duration::from_secs(30), 10).await;

    {
        let recorded = seen.lock().unwrap();
        assert_eq!(recorded[0], 1, "handler saw attempts=1 from crash");
    }

    let snap = read_processor_state(&db, pid).await;
    assert_eq!(snap.processed_seq, 1);
    assert_eq!(snap.attempts, 0, "attempts reset after successful recovery");
}

// ======================================================================
// Chapter 12: Leased partial failure
// ======================================================================

/// Test helper: records which seqs the handler was called with,
/// rejects or retries at a configurable poison seq.
struct PartialFailureHandler {
    seen_seqs: Arc<Mutex<Vec<i64>>>,
    poison_seq: i64,
    reject: bool, // true = Reject, false = Retry
}

#[async_trait::async_trait]
impl LeasedMessageHandler for PartialFailureHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        self.seen_seqs.lock().unwrap().push(msg.seq);
        if msg.seq == self.poison_seq {
            if self.reject {
                return MessageResult::Reject(format!("poison seq={}", msg.seq));
            }
            return MessageResult::Retry;
        }
        MessageResult::Ok
    }
}

/// Transactional version of `PartialFailureHandler`.
struct TxPartialFailureHandler {
    seen_seqs: Arc<Mutex<Vec<i64>>>,
    poison_seq: i64,
    reject: bool,
}

#[async_trait::async_trait]
impl TransactionalMessageHandler for TxPartialFailureHandler {
    async fn handle(
        &self,
        _txn: &sea_orm::DatabaseExecutor<'_>,
        msg: &OutboxMessage,
    ) -> HandlerResult {
        self.seen_seqs.lock().unwrap().push(msg.seq);
        if msg.seq == self.poison_seq {
            if self.reject {
                return HandlerResult::Reject {
                    reason: format!("poison seq={}", msg.seq),
                };
            }
            return HandlerResult::Retry {
                reason: format!("transient seq={}", msg.seq),
            };
        }
        HandlerResult::Success
    }
}

/// Batch handler that always rejects — no `processed_count` side-channel.
struct BatchRejectHandler;

#[async_trait::async_trait]
impl LeasedHandler for BatchRejectHandler {
    async fn handle(&self, _batch: &mut Batch<'_>) -> HandlerResult {
        HandlerResult::Reject {
            reason: "batch reject".into(),
        }
    }
}

// ---- 14.3 Transactional strategy tests ----

#[tokio::test]
async fn tx_partial_reject_processed_count_in_result() {
    let db = setup_db("ch12_tx_partial_reject_pc").await;
    let t = make_default_test_outbox().await;
    t.outbox.register_queue(&db, "q", 1).await.unwrap();
    let pid = t.outbox.all_partition_ids()[0];

    enqueue_and_sequence(&t, &db, "q", 0, &["a", "b", "c", "d", "e"]).await;

    let seen = Arc::new(Mutex::new(Vec::new()));
    let handler = PerMessageAdapter::new(TxPartialFailureHandler {
        seen_seqs: seen.clone(),
        poison_seq: 3, // seqs are 1-based; poison at seq=3
        reject: true,
    });
    let result = run_transactional(&db, pid, handler, 5).await;

    let pr = result.expect("should have a result");
    assert!(matches!(pr.handler_result, HandlerResult::Reject { .. }));
    // PerMessageAdapter processed seqs 1, 2 successfully before poison at seq 3
    assert_eq!(pr.processed_count, Some(2));

    // Transactional mode: all messages dead-lettered (tx is atomic)
    let dls = read_dead_letters(&db).await;
    assert_eq!(dls.len(), 5, "all 5 messages dead-lettered in tx mode");

    // Cursor advances past all messages
    let snap = read_processor_state(&db, pid).await;
    assert_eq!(snap.processed_seq, 5);
}

#[tokio::test]
async fn tx_partial_retry_rolls_back_all() {
    let db = setup_db("ch12_tx_partial_retry").await;
    let t = make_default_test_outbox().await;
    t.outbox.register_queue(&db, "q", 1).await.unwrap();
    let pid = t.outbox.all_partition_ids()[0];

    enqueue_and_sequence(&t, &db, "q", 0, &["a", "b", "c"]).await;

    let seen = Arc::new(Mutex::new(Vec::new()));
    let handler = PerMessageAdapter::new(TxPartialFailureHandler {
        seen_seqs: seen.clone(),
        poison_seq: 2,
        reject: false, // retry
    });
    let result = run_transactional(&db, pid, handler, 3).await;

    let pr = result.expect("should have a result");
    assert!(matches!(pr.handler_result, HandlerResult::Retry { .. }));
    assert_eq!(pr.processed_count, Some(1)); // seq 1 succeeded before retry at seq 2

    // Cursor not advanced on retry
    let snap = read_processor_state(&db, pid).await;
    assert_eq!(snap.processed_seq, 0, "cursor unchanged on retry");

    // No dead letters on retry
    let dls = read_dead_letters(&db).await;
    assert!(dls.is_empty(), "no dead letters on retry");
}

#[tokio::test]
async fn tx_reject_at_first_msg_processed_count_zero() {
    let db = setup_db("ch12_tx_reject_first").await;
    let t = make_default_test_outbox().await;
    t.outbox.register_queue(&db, "q", 1).await.unwrap();
    let pid = t.outbox.all_partition_ids()[0];

    enqueue_and_sequence(&t, &db, "q", 0, &["a", "b"]).await;

    let seen = Arc::new(Mutex::new(Vec::new()));
    let handler = PerMessageAdapter::new(TxPartialFailureHandler {
        seen_seqs: seen.clone(),
        poison_seq: 1, // first message
        reject: true,
    });
    let result = run_transactional(&db, pid, handler, 2).await;

    let pr = result.expect("should have a result");
    assert_eq!(pr.processed_count, Some(0));
}

#[tokio::test]
async fn tx_batch_handler_reject_deadletters_all() {
    let db = setup_db("ch12_tx_batch_reject").await;
    let t = make_default_test_outbox().await;
    t.outbox.register_queue(&db, "q", 1).await.unwrap();
    let pid = t.outbox.all_partition_ids()[0];

    enqueue_and_sequence(&t, &db, "q", 0, &["a", "b", "c"]).await;

    let result = run_transactional(&db, pid, AlwaysRejectTxHandler, 3).await;

    let pr = result.expect("should have a result");
    assert_eq!(pr.processed_count, None, "batch handler returns None");

    let dls = read_dead_letters(&db).await;
    assert_eq!(dls.len(), 3, "all dead-lettered");
}

// ---- 14.4 Decoupled strategy tests ----

#[tokio::test]
async fn decoupled_partial_reject_deadletters_only_remaining() {
    let db = setup_db("ch12_dc_partial_reject").await;
    let t = make_default_test_outbox().await;
    t.outbox.register_queue(&db, "q", 1).await.unwrap();
    let pid = t.outbox.all_partition_ids()[0];

    enqueue_and_sequence(&t, &db, "q", 0, &["a", "b", "c", "d", "e"]).await;

    let seen = Arc::new(Mutex::new(Vec::new()));
    let handler = PartialFailureHandler {
        seen_seqs: seen.clone(),
        poison_seq: 3,
        reject: true,
    };
    let result = run_leased(&db, pid, handler, Duration::from_secs(30), 5).await;

    let pr = result.expect("should have a result");
    // Leased blanket impl: reject calls batch.reject() and continues, returns Success
    assert!(matches!(pr.handler_result, HandlerResult::Success));

    // Only the poison message (seq=3) is dead-lettered; others processed normally
    let dls = read_dead_letters(&db).await;
    assert_eq!(dls.len(), 1, "only poison message dead-lettered");
    assert_eq!(dls[0].seq, 3);

    // Cursor advances past all messages
    let snap = read_processor_state(&db, pid).await;
    assert_eq!(snap.processed_seq, 5);
}

#[tokio::test]
async fn leased_reject_at_first_deadletters_only_poison() {
    let db = setup_db("ch12_dc_reject_first").await;
    let t = make_default_test_outbox().await;
    t.outbox.register_queue(&db, "q", 1).await.unwrap();
    let pid = t.outbox.all_partition_ids()[0];

    enqueue_and_sequence(&t, &db, "q", 0, &["a", "b", "c"]).await;

    let seen = Arc::new(Mutex::new(Vec::new()));
    let handler = PartialFailureHandler {
        seen_seqs: seen.clone(),
        poison_seq: 1, // first message
        reject: true,
    };
    let result = run_leased(&db, pid, handler, Duration::from_secs(30), 3).await;

    let pr = result.expect("should have a result");
    // Leased blanket impl: reject at first, continue with rest, return Success
    assert!(matches!(pr.handler_result, HandlerResult::Success));

    // Only the poison message (seq=1) is dead-lettered; seqs 2,3 processed normally
    let dls = read_dead_letters(&db).await;
    assert_eq!(dls.len(), 1, "only poison message dead-lettered");
    assert_eq!(dls[0].seq, 1);

    // Cursor advances past all messages
    let snap = read_processor_state(&db, pid).await;
    assert_eq!(snap.processed_seq, 3);
}

#[tokio::test]
async fn decoupled_retry_does_not_advance_cursor() {
    let db = setup_db("ch12_dc_retry_no_advance").await;
    let t = make_default_test_outbox().await;
    t.outbox.register_queue(&db, "q", 1).await.unwrap();
    let pid = t.outbox.all_partition_ids()[0];

    enqueue_and_sequence(&t, &db, "q", 0, &["a", "b", "c"]).await;

    let seen = Arc::new(Mutex::new(Vec::new()));
    let handler = PartialFailureHandler {
        seen_seqs: seen.clone(),
        poison_seq: 2,
        reject: false,
    };
    let result = run_leased(&db, pid, handler, Duration::from_secs(30), 3).await;

    let pr = result.expect("should have a result");
    assert!(matches!(pr.handler_result, HandlerResult::Retry { .. }));

    let snap = read_processor_state(&db, pid).await;
    // With the leased blanket impl, msg 1 (seq 1) was acked before msg 2
    // triggered Retry. The cursor advances past the processed prefix so
    // only the failing tail retries - msg 1 is not redelivered.
    assert_eq!(
        snap.processed_seq, 1,
        "cursor advances past processed prefix on retry"
    );

    let dls = read_dead_letters(&db).await;
    assert!(dls.is_empty(), "no dead letters on retry");
}

#[tokio::test]
async fn decoupled_batch_handler_reject_deadletters_all() {
    let db = setup_db("ch12_dc_batch_reject").await;
    let t = make_default_test_outbox().await;
    t.outbox.register_queue(&db, "q", 1).await.unwrap();
    let pid = t.outbox.all_partition_ids()[0];

    enqueue_and_sequence(&t, &db, "q", 0, &["a", "b", "c"]).await;

    let result = run_leased(&db, pid, BatchRejectHandler, Duration::from_secs(30), 3).await;

    let pr = result.expect("should have a result");
    assert_eq!(
        pr.processed_count,
        Some(0),
        "batch handler processed nothing"
    );

    // processed_count=0 → all messages dead-lettered
    let dls = read_dead_letters(&db).await;
    assert_eq!(dls.len(), 3, "all dead-lettered for batch handler");
}

// ---- 14.5 Multi-cycle degradation tests ----

#[tokio::test]
async fn degradation_with_processed_count() {
    use super::workers::processor::{PartitionMode, PartitionModeState};

    // Simulate: batch_size=8, poison at position 3 (0-indexed)
    // → processed_count = 3, degrade to max(3, 1) = 3
    let mut mode = PartitionMode::new();
    mode.transition(
        &HandlerResult::Reject {
            reason: "poison".into(),
        },
        8,
        Some(3),
        1, // degrade immediately
    );
    assert_eq!(mode.effective_batch_size(8), 3);

    // Next success: 3 → 6
    mode.transition(&HandlerResult::Success, 8, None, 1);
    assert_eq!(mode.effective_batch_size(8), 6);

    // Next success: 6 → Normal(8)
    mode.transition(&HandlerResult::Success, 8, None, 1);
    assert!(matches!(mode.state, PartitionModeState::Normal));
}

#[tokio::test]
async fn degradation_batch_handler_falls_back_to_one() {
    use super::workers::processor::PartitionMode;

    let mut mode = PartitionMode::new();
    // Batch handler: None processed_count → degrade to 1
    mode.transition(
        &HandlerResult::Reject {
            reason: "bad".into(),
        },
        8,
        None,
        1, // degrade immediately
    );
    assert_eq!(mode.effective_batch_size(8), 1);
}

#[tokio::test]
async fn degradation_processed_count_zero_degrades_to_one() {
    use super::workers::processor::PartitionMode;

    let mut mode = PartitionMode::new();
    // processed_count=0 → max(0, 1) = 1
    mode.transition(
        &HandlerResult::Retry {
            reason: "fail".into(),
        },
        8,
        Some(0),
        1, // degrade immediately
    );
    assert_eq!(mode.effective_batch_size(8), 1);
}

// ---- 14.6 Edge case tests ----

#[tokio::test]
async fn batch_size_one_partial_failure_is_noop() {
    // With batch_size=1, the leased blanket impl processes exactly 1 message.
    // If it rejects, batch.reject() is called and the single message is dead-lettered.
    let db = setup_db("ch12_batch_one_noop").await;
    let t = make_default_test_outbox().await;
    t.outbox.register_queue(&db, "q", 1).await.unwrap();
    let pid = t.outbox.all_partition_ids()[0];

    enqueue_and_sequence(&t, &db, "q", 0, &["a"]).await;

    let seen = Arc::new(Mutex::new(Vec::new()));
    let handler = PartialFailureHandler {
        seen_seqs: seen.clone(),
        poison_seq: 1,
        reject: true,
    };
    let result = run_leased(&db, pid, handler, Duration::from_secs(30), 1).await;

    let pr = result.expect("should have a result");
    // Leased blanket impl: reject calls batch.reject(), returns Success
    assert!(matches!(pr.handler_result, HandlerResult::Success));

    let dls = read_dead_letters(&db).await;
    assert_eq!(dls.len(), 1, "single message dead-lettered");
}

#[tokio::test]
async fn processed_count_exceeds_batch_is_clamped() {
    use super::workers::processor::PartitionMode;

    // Even if processed_count somehow exceeds batch count, clamping prevents
    // invalid state. The processor clamps pc to count before passing to transition.
    let mut mode = PartitionMode::new();
    // Simulated: count=3, processed_count=5 → clamped to 3 by processor
    let clamped = Some(3u32);
    mode.transition(&HandlerResult::Reject { reason: "x".into() }, 8, clamped, 1);
    assert_eq!(mode.effective_batch_size(8), 3);
}

// ======================================================================
// Chapter 13: Dirty-set-driven Sequencer & Cold Reconciler
// ======================================================================

/// Helper: insert raw incoming rows bypassing enqueue (no dirty flag set).
async fn insert_raw_incoming(db: &Db, partition_id: i64, count: usize) {
    insert_raw_incoming_for_tables(db, &OutboxTables::default(), partition_id, count).await;
}

async fn insert_raw_incoming_for_tables(
    db: &Db,
    tables: &OutboxTables,
    partition_id: i64,
    count: usize,
) {
    let conn = db.sea_internal();
    for _ in 0..count {
        // Insert a body row first
        let body_id = conn
            .query_one_raw(Statement::from_string(
                DbBackend::Sqlite,
                format!(
                    "INSERT INTO {} (payload, payload_type) VALUES (X'AA', 'raw') RETURNING id",
                    tables.body()
                ),
            ))
            .await
            .expect("insert body")
            .expect("body row")
            .try_get_by_index::<i64>(0)
            .expect("body_id");

        conn.execute_raw(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            format!(
                "INSERT INTO {} (partition_id, body_id) VALUES ($1, $2)",
                tables.incoming()
            ),
            [partition_id.into(), body_id.into()],
        ))
        .await
        .expect("insert incoming");
    }
}

#[tokio::test]
async fn dirty_set_populated_after_enqueue() {
    let db = setup_db("ch13_dirty_enqueue").await;
    let t = make_default_test_outbox().await;
    t.outbox.register_queue(&db, "q", 4).await.unwrap();

    enqueue_msgs(&t.outbox, &db, "q", 0, &["a"]).await;
    enqueue_msgs(&t.outbox, &db, "q", 2, &["b"]).await;

    // Prioritizer should have exactly those 2 partition IDs
    let ids = t.outbox.all_partition_ids();
    let g1 = t
        .prioritizer
        .take()
        .expect("should have first dirty partition");
    let g2 = t
        .prioritizer
        .take()
        .expect("should have second dirty partition");
    let mut dirty = vec![g1.partition_id(), g2.partition_id()];
    dirty.sort_unstable();
    g1.processed();
    g2.processed();
    assert_eq!(dirty, vec![ids[0], ids[2]]);
}

#[tokio::test]
async fn sequencer_processes_only_dirty_partitions() {
    let db = setup_db("ch13_only_dirty").await;
    let t = make_default_test_outbox().await;
    t.outbox.register_queue(&db, "q", 4).await.unwrap();

    let ids = t.outbox.all_partition_ids();
    enqueue_msgs(&t.outbox, &db, "q", 1, &["x", "y"]).await;

    run_sequencer_once(&t, &db).await;

    // Only partition 1 should have outgoing
    assert!(read_outgoing(&db, ids[0]).await.is_empty());
    assert_eq!(read_outgoing(&db, ids[1]).await.len(), 2);
    assert!(read_outgoing(&db, ids[2]).await.is_empty());
    assert!(read_outgoing(&db, ids[3]).await.is_empty());
}

#[tokio::test]
async fn poker_discovers_pending_from_incoming_table() {
    let db = setup_db("ch13_poker_discover").await;
    let t = make_default_test_outbox().await;
    t.outbox.register_queue(&db, "q", 2).await.unwrap();

    let ids = t.outbox.all_partition_ids();

    // Insert raw incoming (bypassing enqueue — no dirty flag)
    insert_raw_incoming(&db, ids[0], 2).await;
    insert_raw_incoming(&db, ids[1], 1).await;

    // Prioritizer should be empty (we bypassed enqueue)
    assert!(t.prioritizer.take().is_none());

    // Run cold reconciler
    super::workers::reconciler::reconcile_dirty(&t.outbox, &db, &t.prioritizer).await;

    // Prioritizer should now contain both partitions
    let g1 = t
        .prioritizer
        .take()
        .expect("should have first dirty partition");
    let g2 = t
        .prioritizer
        .take()
        .expect("should have second dirty partition");
    let mut dirty = vec![g1.partition_id(), g2.partition_id()];
    dirty.sort_unstable();
    g1.processed();
    g2.processed();
    assert_eq!(dirty, vec![ids[0], ids[1]]);
}

#[tokio::test]
async fn startup_reconciliation_finds_preexisting_incoming() {
    let db = setup_db("ch13_startup_recon").await;
    let t = make_default_test_outbox().await;
    t.outbox.register_queue(&db, "q", 1).await.unwrap();

    let pid = t.outbox.all_partition_ids()[0];
    insert_raw_incoming(&db, pid, 3).await;

    // Simulate startup reconciliation
    super::workers::reconciler::reconcile_dirty(&t.outbox, &db, &t.prioritizer).await;

    // Now sequencer should pick them up
    run_sequencer_once(&t, &db).await;

    assert_eq!(count_rows(&db, "toolkit_outbox_incoming").await, 0);
    assert_eq!(read_outgoing(&db, pid).await.len(), 3);
}

#[tokio::test]
async fn max_inner_iterations_cap_yields_after_limit() {
    let db = setup_db("ch13_max_iter").await;
    let t = make_default_test_outbox().await;
    t.outbox.register_queue(&db, "q", 1).await.unwrap();

    // Record many messages: batch_size=2, max_inner_iterations=3
    // → can process at most 2*3=6 rows per cycle
    enqueue_msgs(
        &t.outbox,
        &db,
        "q",
        0,
        &["a", "b", "c", "d", "e", "f", "g", "h"],
    )
    .await;

    let config = SequencerConfig {
        batch_size: 2,
        max_inner_iterations: 3,
        ..SequencerConfig::default()
    };
    let mut seq = make_sequencer(&t, config, &db);
    let cancel = CancellationToken::new();
    let result = seq.execute(&cancel).await.unwrap();
    assert!(matches!(result, Directive::Proceed(_)));

    // Should have processed 6 (2 per iteration × 3 iterations) and re-inserted
    let pid = t.outbox.all_partition_ids()[0];
    let outgoing = read_outgoing(&db, pid).await;
    assert_eq!(outgoing.len(), 6);

    // Remaining 2 should still be in incoming
    assert_eq!(count_rows(&db, "toolkit_outbox_incoming").await, 2);

    // Partition should have been re-dirtied (saturated)
    let guard = t
        .prioritizer
        .take()
        .expect("saturated partition should be re-dirtied");
    assert_eq!(guard.partition_id(), pid);
    guard.processed();
}

#[tokio::test]
async fn execute_processes_one_partition_per_call() {
    let db = setup_db("ch13_one_per_call").await;
    let t = make_default_test_outbox().await;
    t.outbox.register_queue(&db, "q", 4).await.unwrap();

    let ids = t.outbox.all_partition_ids();
    for i in 0..4 {
        enqueue_msgs(&t.outbox, &db, "q", i, &["msg"]).await;
    }

    let mut seq = make_sequencer(&t, SequencerConfig::default(), &db);
    let cancel = CancellationToken::new();

    // First execute() processes exactly one partition (unsaturated but did work → Proceed)
    let result = seq.execute(&cancel).await.unwrap();
    assert!(matches!(result, Directive::Proceed(_)));

    let mut processed = 0;
    for &id in &ids {
        if !read_outgoing(&db, id).await.is_empty() {
            processed += 1;
        }
    }
    assert_eq!(processed, 1);

    // Run until idle — all 4 partitions processed
    run_sequencer_until_idle(&mut seq).await;

    processed = 0;
    for &id in &ids {
        if !read_outgoing(&db, id).await.is_empty() {
            processed += 1;
        }
    }
    assert_eq!(processed, 4);
}

#[tokio::test]
async fn prioritizer_lru_fairness_across_cycles() {
    let db = setup_db("ch13_lru_fair").await;
    let t = make_default_test_outbox().await;
    t.outbox.register_queue(&db, "q", 4).await.unwrap();

    let ids = t.outbox.all_partition_ids();

    for i in 0..4 {
        enqueue_msgs(&t.outbox, &db, "q", i, &["r1"]).await;
    }

    // Run until idle — all partitions processed via LRU ordering
    let mut seq = make_sequencer(&t, SequencerConfig::default(), &db);
    run_sequencer_until_idle(&mut seq).await;

    let mut total_outgoing = 0;
    for &id in &ids {
        total_outgoing += read_outgoing(&db, id).await.len();
    }
    assert_eq!(total_outgoing, 4, "all 4 partitions should be processed");
}

// ======================================================================
// Chapter 14: Parallel Sequencer Workers
// ======================================================================

/// Helper: create a sequencer with a specific shared prioritizer (for multi-worker tests).
fn make_sequencer_with_shared(
    t: &TestOutbox,
    config: SequencerConfig,
    db: &Db,
    shared: Arc<SharedPrioritizer>,
) -> Sequencer {
    Sequencer::new(config, Arc::clone(&t.outbox), db.clone(), shared)
}

#[tokio::test]
async fn parallel_sequencers_process_distinct_partitions() {
    let db = setup_db("ch14_parallel_distinct").await;
    let t = make_default_test_outbox().await;
    t.outbox.register_queue(&db, "q", 4).await.unwrap();

    for i in 0..4 {
        enqueue_msgs(&t.outbox, &db, "q", i, &["msg"]).await;
    }

    // Two sequencers sharing the same prioritizer
    let shared = make_shared_prioritizer();
    let ids = t.outbox.all_partition_ids();
    for &id in &ids {
        shared.push_dirty(id);
    }
    let config = SequencerConfig::default();
    let mut seq_a = make_sequencer_with_shared(&t, config.clone(), &db, Arc::clone(&shared));
    let mut seq_b = make_sequencer_with_shared(&t, config, &db, Arc::clone(&shared));
    let cancel = CancellationToken::new();

    // Each sequencer takes one partition at a time from the shared prioritizer
    let r1 = seq_a.execute(&cancel).await.unwrap();
    let r2 = seq_b.execute(&cancel).await.unwrap();
    assert!(matches!(r1, Directive::Proceed(_)));
    assert!(matches!(r2, Directive::Proceed(_)));

    // After two executes, exactly 2 partitions should be processed
    let mut processed = 0;
    for &id in &ids {
        if !read_outgoing(&db, id).await.is_empty() {
            processed += 1;
        }
    }
    assert_eq!(processed, 2);

    // Two more executes drain the remaining 2 partitions
    let r3 = seq_a.execute(&cancel).await.unwrap();
    let r4 = seq_b.execute(&cancel).await.unwrap();
    assert!(matches!(r3, Directive::Proceed(_)));
    assert!(matches!(r4, Directive::Proceed(_)));

    processed = 0;
    for &id in &ids {
        if !read_outgoing(&db, id).await.is_empty() {
            processed += 1;
        }
    }
    assert_eq!(processed, 4);

    // Both should now be idle
    let r5 = seq_a.execute(&cancel).await.unwrap();
    assert!(matches!(r5, Directive::Idle(_)));
}

#[tokio::test]
async fn parallel_sequencers_no_duplicate_sequences() {
    let db = setup_db("ch14_no_dups").await;
    let t = make_default_test_outbox().await;
    t.outbox.register_queue(&db, "q", 4).await.unwrap();

    for i in 0..4 {
        enqueue_msgs(&t.outbox, &db, "q", i, &["a", "b", "c"]).await;
    }

    // Two sequencers sharing the same prioritizer
    let shared = make_shared_prioritizer();
    let ids = t.outbox.all_partition_ids();
    for &id in &ids {
        shared.push_dirty(id);
    }
    let config = SequencerConfig::default();
    let mut seq_a = make_sequencer_with_shared(&t, config.clone(), &db, Arc::clone(&shared));
    let mut seq_b = make_sequencer_with_shared(&t, config, &db, Arc::clone(&shared));

    // Run both until idle (alternating to simulate concurrency)
    let cancel = CancellationToken::new();
    loop {
        let a = seq_a.execute(&cancel).await.unwrap();
        let b = seq_b.execute(&cancel).await.unwrap();
        if matches!(a, Directive::Idle(_)) && matches!(b, Directive::Idle(_)) {
            break;
        }
    }

    // Verify: each partition has exactly 3 outgoing rows with contiguous sequences 1,2,3
    let ids = t.outbox.all_partition_ids();
    for &pid in &ids {
        let outgoing = read_outgoing(&db, pid).await;
        assert_eq!(outgoing.len(), 3, "partition {pid} should have 3 rows");
        let seqs: Vec<i64> = outgoing.iter().map(|r| r.seq).collect();
        assert_eq!(
            seqs,
            vec![1, 2, 3],
            "partition {pid} should have seqs 1,2,3"
        );
    }

    // No rows left in incoming
    assert_eq!(count_rows(&db, "toolkit_outbox_incoming").await, 0);
}

#[tokio::test]
async fn saturated_partition_fully_drained_across_cycles() {
    let db = setup_db("ch14_saturated_cycles").await;
    let t = make_default_test_outbox().await;
    t.outbox.register_queue(&db, "q", 1).await.unwrap();
    let pid = t.outbox.all_partition_ids()[0];

    // Record 20 messages. batch_size=3, max_inner_iterations=2 → 6 per execute.
    // Needs 4 execute() cycles to drain 20 (6+6+6+2).
    let payloads: Vec<&str> = (0..20).map(|_| "x").collect();
    enqueue_msgs(&t.outbox, &db, "q", 0, &payloads).await;

    let config = SequencerConfig {
        batch_size: 3,
        max_inner_iterations: 2,
        ..Default::default()
    };
    let mut seq = make_sequencer(&t, config, &db);
    let cancel = CancellationToken::new();

    // Cycle 1: drains 6 (3×2), saturated → re-dirtied
    let r = seq.execute(&cancel).await.unwrap();
    assert!(matches!(r, Directive::Proceed(_)));
    assert_eq!(read_outgoing(&db, pid).await.len(), 6);
    assert_eq!(count_rows(&db, "toolkit_outbox_incoming").await, 14);

    // Run until idle — remaining 14 drained across more cycles
    run_sequencer_until_idle(&mut seq).await;

    assert_eq!(read_outgoing(&db, pid).await.len(), 20);
    assert_eq!(count_rows(&db, "toolkit_outbox_incoming").await, 0);

    // Sequences are contiguous 1..=20
    let outgoing = read_outgoing(&db, pid).await;
    let seqs: Vec<i64> = outgoing.iter().map(|r| r.seq).collect();
    assert_eq!(seqs, (1..=20).collect::<Vec<_>>());
}

// ======================================================================
// Chapter 15: Processor Semaphore
// ======================================================================

#[tokio::test]
async fn processor_semaphore_limits_concurrency() {
    use super::taskward::{BackoffConfig, Bulkhead, BulkheadConfig, ConcurrencyLimit};
    use tokio::sync::Semaphore;

    // Create a semaphore with 2 permits
    let sem = Arc::new(Semaphore::new(2));

    // Acquire 2 permits manually (simulating 2 active processors)
    let _p1 = sem.clone().acquire_owned().await.unwrap();
    let _p2 = sem.clone().acquire_owned().await.unwrap();

    // A third acquire should not complete immediately
    let cancel = CancellationToken::new();
    let bulkhead = Bulkhead::new(
        "test",
        BulkheadConfig {
            semaphore: ConcurrencyLimit::Fixed(Arc::clone(&sem)),
            backoff: BackoffConfig::new(Duration::from_millis(100), Duration::from_secs(30), 2.0),
        },
    );

    // Cancel immediately to avoid blocking — acquire should return None
    cancel.cancel();
    let result = bulkhead.acquire(&cancel).await;
    assert!(
        result.is_none(),
        "should not acquire when all permits taken and cancelled"
    );
}

// ======================================================================
// Chapter 16: Vacuum Parallelism
// ======================================================================

#[tokio::test]
async fn vacuum_counter_decrement_is_idempotent() {
    let db = setup_db("ch16_vac_idempotent").await;
    let t = make_default_test_outbox().await;
    t.outbox.register_queue(&db, "q", 1).await.unwrap();
    let pid = t.outbox.all_partition_ids()[0];

    // Set vacuum counter to 5 (simulating 5 processed messages)
    set_vacuum_counter(&db, pid, 5).await;
    assert_eq!(read_vacuum_counter(&db, pid).await, 5);

    // Two "vacuum workers" both snapshot counter=5, both decrement by 5
    // First decrement: 5 - 5 = 0
    // Against the statement the vacuum actually issues, so a second unused
    // copy of it cannot mask a wrong one.
    let conn = db.sea_internal();
    let statements = super::statements::OutboxStatements::new(
        conn.get_database_backend(),
        &OutboxTables::default(),
    );
    let store = OutboxStore::new(&statements);
    conn.execute_raw(Statement::from_sql_and_values(
        conn.get_database_backend(),
        store.decrement_vacuum_counter(),
        // Statement order: the delta, then the row.
        [5i64.into(), pid.into()],
    ))
    .await
    .unwrap();
    assert_eq!(read_vacuum_counter(&db, pid).await, 0);

    // Second decrement (stale snapshot): GREATEST(0 - 5, 0) = 0
    conn.execute_raw(Statement::from_sql_and_values(
        conn.get_database_backend(),
        store.decrement_vacuum_counter(),
        [5i64.into(), pid.into()],
    ))
    .await
    .unwrap();
    // Counter should floor at 0, never go negative
    assert_eq!(read_vacuum_counter(&db, pid).await, 0);
}

#[tokio::test]
async fn vacuum_concurrent_workers_safe() {
    use super::workers::vacuum::VacuumTask;

    let db = setup_db("ch16_vac_concurrent").await;
    let t = make_default_test_outbox().await;
    t.outbox.register_queue(&db, "q", 1).await.unwrap();
    let pid = t.outbox.all_partition_ids()[0];

    enqueue_and_sequence(&t, &db, "q", 0, &["a", "b", "c"]).await;

    // Advance processed_seq to 3 (simulating processor progress)
    let conn = db.sea_internal();
    conn.execute_raw(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        "UPDATE toolkit_outbox_processor SET processed_seq = 3 WHERE partition_id = $1",
        [pid.into()],
    ))
    .await
    .unwrap();

    // Bump vacuum counter so vacuum picks up the partition
    set_vacuum_counter(&db, pid, 3).await;

    // Run two vacuum workers sequentially (SQLite single connection)
    let cancel = CancellationToken::new();
    let tables = OutboxTables::default();
    let statements = Arc::new(super::statements::OutboxStatements::new(
        db.sea_internal().get_database_backend(),
        &tables,
    ));
    let collectable = test_collectable_traces();
    let mut vac1 = VacuumTask::new(
        db.clone(),
        Arc::clone(&statements),
        10_000,
        Arc::clone(&collectable),
    );
    let mut vac2 = VacuumTask::new(db.clone(), statements, 10_000, collectable);

    vac1.execute(&cancel).await.unwrap();
    vac2.execute(&cancel).await.unwrap();

    // All outgoing and body rows should be cleaned up
    assert_eq!(count_rows(&db, "toolkit_outbox_outgoing").await, 0);
    assert_eq!(count_rows(&db, "toolkit_outbox_body").await, 0);

    // Counter should be at 0
    assert_eq!(read_vacuum_counter(&db, pid).await, 0);
}

// ======================================================================
// Chapter 17: Priority Semaphore
// ======================================================================

#[tokio::test]
async fn priority_bulkhead_prefers_shared_when_available() {
    use super::taskward::{BackoffConfig, Bulkhead, BulkheadConfig, ConcurrencyLimit};
    use tokio::sync::Semaphore;

    let guaranteed = Arc::new(Semaphore::new(4));
    let shared = Arc::new(Semaphore::new(2));
    let cancel = CancellationToken::new();

    let bulkhead = Bulkhead::new(
        "seq-0",
        BulkheadConfig {
            semaphore: ConcurrencyLimit::Tiered {
                guaranteed: Arc::clone(&guaranteed),
                shared: Arc::clone(&shared),
            },
            backoff: BackoffConfig::new(Duration::from_millis(100), Duration::from_secs(30), 2.0),
        },
    );

    // When both available, biased select prefers shared
    let permit = bulkhead.acquire(&cancel).await;
    assert!(permit.is_some(), "should acquire a permit");

    // shared should have 1 available (started with 2, acquired 1)
    assert_eq!(shared.available_permits(), 1);
    // guaranteed should still have all 4
    assert_eq!(guaranteed.available_permits(), 4);
}

#[tokio::test]
async fn priority_bulkhead_falls_back_to_guaranteed_when_shared_exhausted() {
    use super::taskward::{BackoffConfig, Bulkhead, BulkheadConfig, ConcurrencyLimit};
    use tokio::sync::Semaphore;

    let guaranteed = Arc::new(Semaphore::new(4));
    let shared = Arc::new(Semaphore::new(2));

    // Exhaust shared permits (simulating vacuum holding them)
    let _hold1 = shared.clone().acquire_owned().await.unwrap();
    let _hold2 = shared.clone().acquire_owned().await.unwrap();
    assert_eq!(shared.available_permits(), 0);

    let cancel = CancellationToken::new();
    let bulkhead = Bulkhead::new(
        "seq-0",
        BulkheadConfig {
            semaphore: ConcurrencyLimit::Tiered {
                guaranteed: Arc::clone(&guaranteed),
                shared: Arc::clone(&shared),
            },
            backoff: BackoffConfig::new(Duration::from_millis(100), Duration::from_secs(30), 2.0),
        },
    );

    // Should fall back to guaranteed since shared is exhausted
    let permit = bulkhead.acquire(&cancel).await;
    assert!(permit.is_some(), "should acquire guaranteed permit");

    // guaranteed should have 3 remaining (started with 4, acquired 1)
    assert_eq!(guaranteed.available_permits(), 3);
}

// ======================================================================
// Chapter 18: Partition Guard Panic Recovery
// ======================================================================

#[tokio::test]
async fn partition_guard_drop_preserves_dirty_signal() {
    let t = make_default_test_outbox().await;

    // Mark a partition dirty via prioritizer
    t.prioritizer.push_dirty(42);

    // Take a guard
    let guard = t.prioritizer.take().expect("should get guard");
    assert_eq!(guard.partition_id(), 42);

    // Drop without ack (simulating panic)
    drop(guard);

    // The partition should still be available for retry
    let guard2 = t.prioritizer.take().expect("should get guard after drop");
    assert_eq!(guard2.partition_id(), 42);
    guard2.processed(); // clean up

    // Now it's consumed
    assert!(t.prioritizer.take().is_none());
}

#[tokio::test]
async fn sequencer_processes_across_enqueue_cycles() {
    let db = setup_db("ch18_guard_error_retry").await;
    let t = make_default_test_outbox().await;
    t.outbox.register_queue(&db, "q", 1).await.unwrap();
    let pid = t.outbox.all_partition_ids()[0];

    enqueue_msgs(&t.outbox, &db, "q", 0, &["a"]).await;

    let shared = make_shared_prioritizer();
    shared.push_dirty(pid);
    // Wire outbox to use shared prioritizer for subsequent enqueues
    t.outbox
        .prioritizer
        .write()
        .await
        .replace(Arc::clone(&shared));
    let config = SequencerConfig::default();
    let mut seq = make_sequencer_with_shared(&t, config, &db, Arc::clone(&shared));
    let cancel = CancellationToken::new();

    // First execute processes the partition successfully (unsaturated but did work → Proceed)
    let r = seq.execute(&cancel).await.unwrap();
    assert!(matches!(r, Directive::Proceed(_)));
    assert_eq!(read_outgoing(&db, pid).await.len(), 1);

    enqueue_msgs(&t.outbox, &db, "q", 0, &["b"]).await;

    // This time the sequencer should process it in another cycle
    let r = seq.execute(&cancel).await.unwrap();
    assert!(matches!(r, Directive::Proceed(_)));
    assert_eq!(read_outgoing(&db, pid).await.len(), 2);

    // Sequences should be contiguous
    let outgoing = read_outgoing(&db, pid).await;
    let seqs: Vec<i64> = outgoing.iter().map(|r| r.seq).collect();
    assert_eq!(seqs, vec![1, 2]);
}

// ======================================================================
// Chapter 19: Full-Pipeline E2E (builder → handler delivery)
// ======================================================================

/// Counting handler for full-pipeline tests.
struct CountingHandler {
    counter: Arc<AtomicUsize>,
    notify: Arc<tokio::sync::Notify>,
}

#[async_trait::async_trait]
impl LeasedHandler for CountingHandler {
    async fn handle(&self, batch: &mut Batch<'_>) -> super::handler::HandlerResult {
        while batch.next_msg().is_some() {
            self.counter.fetch_add(1, Ordering::Relaxed);
            batch.ack();
        }
        self.notify.notify_one();
        super::handler::HandlerResult::Success
    }
}

/// Enqueue a single message through the full builder pipeline, wait for
/// the handler to receive it, then verify exactly one delivery (no
/// duplicates, no loss).
#[tokio::test]
async fn pipeline_single_enqueue_one_delivery() {
    let db = setup_db("ch19_pipeline_single").await;

    let counter = Arc::new(AtomicUsize::new(0));
    let notify = Arc::new(tokio::sync::Notify::new());

    let handler = CountingHandler {
        counter: Arc::clone(&counter),
        notify: Arc::clone(&notify),
    };

    let handle = Outbox::builder(db.clone())
        .processor_tuning(WorkerTuning::processor_default().idle_interval(Duration::from_mins(1)))
        .sequencer_tuning(WorkerTuning::sequencer_default().idle_interval(Duration::from_mins(1)))
        .processors(1)
        .maintenance(1, 1)
        .queue("test-q", Partitions::of(1))
        .leased(handler)
        .start()
        .await
        .unwrap();

    let outbox = handle.outbox();

    let (db, result) = outbox
        .transaction(db, |tx| {
            let o = Arc::clone(outbox);
            Box::pin(async move {
                o.enqueue(
                    tx,
                    Record::to("test-q", 0)
                        .payload(b"hello".to_vec(), "test/msg")
                        .build()
                        .unwrap(),
                )
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
                Ok(())
            })
        })
        .await;
    result.unwrap();

    // Wait for the message to be consumed
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if counter.load(Ordering::Acquire) >= 1 {
            break;
        }
        let remaining = deadline
            .checked_duration_since(tokio::time::Instant::now())
            .unwrap_or(Duration::ZERO);
        assert!(
            !remaining.is_zero(),
            "timed out waiting for consumption (consumed: {})",
            counter.load(Ordering::Relaxed)
        );
        tokio::time::timeout(remaining, notify.notified())
            .await
            .ok();
    }

    // Verify no duplicate delivery arrives. Wait 200ms and check the counter
    // hasn't incremented beyond 1. We check the counter directly instead of
    // notify.notified() because a stale permit from the first delivery's
    // notify_one() would cause notified().await to resolve immediately.
    let baseline = counter.load(Ordering::Relaxed);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        counter.load(Ordering::Relaxed),
        baseline,
        "no duplicate delivery should occur (counter changed during wait)"
    );
    assert_eq!(
        baseline, 1,
        "single enqueue must produce exactly one delivery"
    );

    drop(db);
    handle.stop().await;
}

// ======================================================================
// Chapter 20: P0 Coverage — Concurrency & Boundary Tests
// ======================================================================

/// Spawn a background producer that enqueues while the sequencer is running.
/// Verifies that all messages appear in outgoing with correct sequences.
#[tokio::test]
async fn concurrent_enqueue_during_sequencer_preserves_order() {
    let db = setup_db("ch20_concurrent_enqueue").await;
    let t = make_default_test_outbox().await;
    t.outbox.register_queue(&db, "q", 2).await.unwrap();

    // Enqueue an initial batch so the sequencer has work
    enqueue_msgs(&t.outbox, &db, "q", 0, &["a", "b"]).await;
    enqueue_msgs(&t.outbox, &db, "q", 1, &["c", "d"]).await;

    let ids = t.outbox.all_partition_ids();
    for &id in &ids {
        t.prioritizer.push_dirty(id);
    }

    // Spawn a background producer that enqueues more while sequencer runs
    let outbox_clone = Arc::clone(&t.outbox);
    let db_clone = db.clone();
    let producer = tokio::spawn(async move {
        for i in 0..5 {
            let payload = format!("bg-{i}");
            let conn = db_clone.conn().expect("conn");
            outbox_clone
                .enqueue(
                    &conn,
                    Record::to("q", 0)
                        .payload(payload.into_bytes(), "text/plain")
                        .build()
                        .unwrap(),
                )
                .await
                .expect("bg enqueue");
        }
    });

    // Run sequencer concurrently
    let mut seq = make_sequencer(&t, SequencerConfig::default(), &db);
    run_sequencer_until_idle(&mut seq).await;

    // Wait for producer to finish
    producer.await.unwrap();

    // The background messages may have dirtied partitions — drain again
    for &id in &ids {
        t.prioritizer.push_dirty(id);
    }
    run_sequencer_until_idle(&mut seq).await;

    // Verify: all messages in outgoing, sequences contiguous per partition
    for &pid in &ids {
        let outgoing = read_outgoing(&db, pid).await;
        if outgoing.is_empty() {
            continue;
        }
        let seqs: Vec<i64> = outgoing.iter().map(|r| r.seq).collect();
        #[allow(clippy::cast_possible_wrap)]
        let expected: Vec<i64> = (1..=seqs.len() as i64).collect();
        assert_eq!(
            seqs, expected,
            "partition {pid} sequences must be contiguous"
        );
    }

    // Total messages: 4 initial + 5 background = 9
    let mut total = 0;
    for &pid in &ids {
        total += read_outgoing(&db, pid).await.len();
    }
    assert_eq!(total, 9, "all 9 messages should be in outgoing");
}

/// Verify `Partitions::of(0)` panics.
#[test]
#[should_panic(expected = "partition count must be a power of 2")]
fn registration_zero_partitions_rejected() {
    #[allow(clippy::let_underscore_must_use)]
    let _ = Partitions::of(0);
}

/// Verify that the sequencer returns `Directive::Idle` with zero rows
/// when processing an empty (already-drained) partition.
#[tokio::test]
async fn sequencer_empty_partition_returns_idle_zero() {
    let db = setup_db("ch20_idle_zero").await;
    let t = make_default_test_outbox().await;
    t.outbox.register_queue(&db, "q", 1).await.unwrap();
    let pid = t.outbox.all_partition_ids()[0];

    // Push dirty but don't enqueue — partition is empty
    t.prioritizer.push_dirty(pid);

    let mut seq = make_sequencer(&t, SequencerConfig::default(), &db);
    let cancel = CancellationToken::new();
    let result = seq.execute(&cancel).await.unwrap();
    assert!(
        matches!(result, Directive::Idle(_)),
        "empty partition should return Idle"
    );
    assert_eq!(result.payload().rows_claimed, 0);
}

// ======================================================================
// Chapter 21: P1 Coverage
// ======================================================================

/// Builder with no queues starts successfully but enqueue fails.
#[tokio::test]
async fn builder_no_queues_starts_but_enqueue_fails() {
    let db = setup_db("ch21_no_queues").await;

    let handle = Outbox::builder(db.clone())
        .processor_tuning(WorkerTuning::processor_default().idle_interval(Duration::from_mins(1)))
        .sequencer_tuning(WorkerTuning::sequencer_default().idle_interval(Duration::from_mins(1)))
        .maintenance(1, 1)
        .start()
        .await
        .expect("start with no queues should succeed");

    let outbox = handle.outbox();
    let conn = db.conn().expect("conn");
    let err = outbox
        .enqueue(
            &conn,
            Record::to("nonexistent", 0)
                .payload(b"hello".to_vec(), "text/plain")
                .build()
                .unwrap(),
        )
        .await;
    assert!(err.is_err(), "enqueue to unregistered queue should fail");

    handle.stop().await;
}

// reconciler_is_idempotent — covered by Ch 13 tests:
// `poker_discovers_pending_from_incoming_table` and
// `startup_reconciliation_finds_preexisting_incoming`.
// Direct reconcile_dirty call hangs on SQLite single-connection pool.

// ======================================================================
// Chapter 22: Bug Reproduction Tests
// ======================================================================

// -- Bug 1: batch_transactional() forces batch_size=1 --
//
// TransactionalProcessorFactory::spawn() unconditionally sets
// ctx.tuning.batch_size = 1 (builder.rs:214). This means
// batch_transactional() handlers never see more than 1 message per call,
// even though they are batch handlers and the user configured batch_size > 1.

/// Batch transactional handler that records the max batch size it receives.
struct MaxBatchSizeHandler {
    max_batch_seen: Arc<AtomicU32>,
    total_processed: Arc<AtomicUsize>,
    notify: Arc<tokio::sync::Notify>,
}

#[async_trait::async_trait]
impl TransactionalHandler for MaxBatchSizeHandler {
    async fn handle(
        &self,
        _txn: &sea_orm::DatabaseExecutor<'_>,
        msgs: &[OutboxMessage],
    ) -> HandlerResult {
        #[allow(clippy::cast_possible_truncation)]
        let batch_len = msgs.len() as u32;
        self.max_batch_seen.fetch_max(batch_len, Ordering::Relaxed);
        self.total_processed
            .fetch_add(msgs.len(), Ordering::Relaxed);
        self.notify.notify_one();
        HandlerResult::Success
    }
}

/// `batch_transactional()` should respect the configured `batch_size`, not force it to 1.
///
/// Regression test: `TransactionalProcessorFactory::spawn()` must not
/// unconditionally override `batch_size` to 1.
#[tokio::test]
async fn batch_transactional_respects_configured_batch_size() {
    let db = setup_db("ch22_batch_txn_batch_size").await;

    let max_batch_seen = Arc::new(AtomicU32::new(0));
    let total_processed = Arc::new(AtomicUsize::new(0));
    let notify = Arc::new(tokio::sync::Notify::new());

    let handler = MaxBatchSizeHandler {
        max_batch_seen: Arc::clone(&max_batch_seen),
        total_processed: Arc::clone(&total_processed),
        notify: Arc::clone(&notify),
    };

    // Configure processor tuning with batch_size=5
    let handle = Outbox::builder(db.clone())
        .processor_tuning(
            WorkerTuning::processor_default()
                .batch_size(5)
                .idle_interval(Duration::from_mins(1)),
        )
        .sequencer_tuning(WorkerTuning::sequencer_default().idle_interval(Duration::from_mins(1)))
        .processors(1)
        .maintenance(1, 1)
        .queue("batch-q", Partitions::of(1))
        .batch_transactional(handler)
        .start()
        .await
        .unwrap();

    let outbox = handle.outbox();

    let (db, result) = outbox
        .transaction(db, |tx| {
            let o = Arc::clone(outbox);
            Box::pin(async move {
                for i in 0..5u8 {
                    o.enqueue(
                        tx,
                        Record::to("batch-q", 0)
                            .payload(vec![i], "test/msg")
                            .build()
                            .unwrap(),
                    )
                    .await
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                }
                Ok(())
            })
        })
        .await;
    result.unwrap();

    // Wait for all 5 messages to be consumed
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if total_processed.load(Ordering::Acquire) >= 5 {
            break;
        }
        let remaining = deadline
            .checked_duration_since(tokio::time::Instant::now())
            .unwrap_or(Duration::ZERO);
        assert!(
            !remaining.is_zero(),
            "timed out waiting for consumption (processed: {})",
            total_processed.load(Ordering::Relaxed)
        );
        tokio::time::timeout(remaining, notify.notified())
            .await
            .ok();
    }

    // The handler should have seen at least one batch with >1 message.
    // With batch_size=5 and 5 messages on the same partition, the handler
    // should ideally receive all 5 in one call.
    // BUG: Today the max is always 1 because the factory forces batch_size=1.
    let max = max_batch_seen.load(Ordering::Relaxed);
    assert!(
        max > 1,
        "batch_transactional handler should receive batches > 1, but max batch size seen was {max}. \
         This proves TransactionalProcessorFactory forces batch_size=1 even for batch handlers."
    );

    drop(db);
    handle.stop().await;
}
