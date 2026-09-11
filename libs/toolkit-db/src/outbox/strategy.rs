use std::collections::{HashMap, HashSet};

use super::batch::Batch;
use super::handler::{HandlerResult, LeasedHandler, OutboxMessage, TransactionalHandler};
use super::store::OutboxStore;
use super::types::{LeaseConfig, OutboxError};
use crate::Db;
use sea_orm::{ConnectionTrait, DatabaseExecutor, FromQueryResult, Statement, TransactionTrait};

/// Context for processing a single partition's batch.
pub struct ProcessContext<'a> {
    pub db: &'a Db,
    pub store: OutboxStore<'a>,
    pub partition_id: i64,
    /// Who this instance is, and who is waiting for a completion.
    pub mailbox: &'a super::subscription::Mailbox,
}

/// Sealed trait for compile-time processing mode dispatch.
///
/// Each implementation manages its own transaction scope. The processor
/// delegates the entire read→handle→ack cycle to the strategy.
pub trait ProcessingStrategy: Send + Sync {
    /// Process one batch for the given partition.
    ///
    /// `msg_batch_size` controls how many messages to fetch per cycle
    /// (from `WorkerTuning::batch_size`, possibly degraded by `PartitionMode`).
    ///
    /// Returns `Ok(Some(result))` if work was done, `Ok(None)` if the
    /// partition was empty or locked by another processor.
    fn process(
        &self,
        ctx: &ProcessContext<'_>,
        msg_batch_size: u32,
    ) -> impl std::future::Future<Output = Result<Option<ProcessResult>, OutboxError>> + Send;
}

/// Result of processing a batch.
pub struct ProcessResult {
    pub count: u32,
    pub handler_result: HandlerResult,
    /// Number of messages the handler successfully processed before the batch
    /// completed (or failed). `Some` for `PerMessageAdapter`-wrapped handlers,
    /// `None` for raw batch handlers. Used for partial-failure semantics.
    pub processed_count: Option<u32>,
}

// ---- SQL row types ----

#[derive(Debug, FromQueryResult)]
struct ProcessorRow {
    processed_seq: i64,
    attempts: i16,
}

#[derive(Debug, FromQueryResult)]
struct OutgoingRow {
    id: i64,
    body_id: i64,
    seq: i64,
}

#[derive(Debug, FromQueryResult)]
struct BodyRow {
    id: i64,
    payload: Vec<u8>,
    payload_type: String,
    created_at: chrono::DateTime<chrono::Utc>,
    trace_id: Option<i64>,
}

/// The trace each message belongs to, by `seq`.
///
/// Keyed by `seq` rather than paired positionally with the messages, so there
/// is no alignment to get wrong, and only traced messages appear - an untraced
/// batch leaves it empty and allocates nothing further.
type TraceIds = HashMap<i64, i64>;

// ---- Shared helpers ----

async fn read_messages(
    txn: &impl ConnectionTrait,
    store: &OutboxStore<'_>,
    partition_id: i64,
    proc_row: &ProcessorRow,
    msg_batch_size: u32,
) -> Result<(Vec<OutboxMessage>, TraceIds), OutboxError> {
    // Use seq > processed_seq (not seq >= processed_seq + 1) - the cursor
    // stores the last processed seq, so `>` is the natural predicate.
    let outgoing_rows = OutgoingRow::find_by_statement(Statement::from_sql_and_values(
        store.backend(),
        store.read_outgoing_batch(msg_batch_size),
        [partition_id.into(), proc_row.processed_seq.into()],
    ))
    .all(txn)
    .await?;

    if outgoing_rows.is_empty() {
        return Ok((Vec::new(), TraceIds::new()));
    }

    // Batch body read: single SELECT ... WHERE id IN (...) instead of N+1 queries
    let body_ids: Vec<i64> = outgoing_rows.iter().map(|r| r.body_id).collect();
    let body_sql = store.build_read_body_batch(body_ids.len());
    let body_values: Vec<sea_orm::Value> = body_ids.iter().map(|&id| id.into()).collect();
    let body_rows = BodyRow::find_by_statement(Statement::from_sql_and_values(
        store.backend(),
        &body_sql,
        body_values,
    ))
    .all(txn)
    .await?;

    let body_map: HashMap<i64, BodyRow> = body_rows.into_iter().map(|b| (b.id, b)).collect();

    let mut msgs = Vec::with_capacity(outgoing_rows.len());
    let mut trace_ids = TraceIds::new();
    for row in &outgoing_rows {
        let body = body_map.get(&row.body_id).ok_or_else(|| {
            OutboxError::Database(sea_orm::DbErr::Custom(format!(
                "body row {} not found for outgoing {}",
                row.body_id, row.id
            )))
        })?;

        if let Some(trace_id) = body.trace_id {
            trace_ids.insert(row.seq, trace_id);
        }

        msgs.push(OutboxMessage {
            partition_id,
            seq: row.seq,
            payload: body.payload.clone(),
            payload_type: body.payload_type.clone(),
            created_at: body.created_at,
            attempts: proc_row.attempts,
        });
    }

    Ok((msgs, trace_ids))
}

/// How much of each trace an ack is terminalizing: how many of its entities
/// reached a terminal state, and how many of those failed.
///
/// Only traces present in the batch appear, so an untraced batch produces
/// nothing and issues no statement.
fn trace_progress(
    msgs: &[OutboxMessage],
    trace_ids: &TraceIds,
    upto_seq: i64,
    failed: &HashSet<i64>,
) -> Vec<(i64, i64, i64)> {
    let mut per_trace: HashMap<i64, (i64, i64)> = HashMap::new();
    for msg in msgs.iter().filter(|m| m.seq <= upto_seq) {
        if let Some(&trace_id) = trace_ids.get(&msg.seq) {
            let entry = per_trace.entry(trace_id).or_insert((0, 0));
            entry.0 += 1;
            if failed.contains(&msg.seq) {
                entry.1 += 1;
            }
        }
    }
    // Sorted by trace id, and that is not cosmetic: an ack takes a row lock
    // on every trace it counts down, so two acks sharing two traces would
    // deadlock if they took them in opposite orders. A `HashMap`'s iteration
    // order is exactly that hazard.
    let mut progress: Vec<(i64, i64, i64)> = per_trace
        .into_iter()
        .map(|(trace_id, (terminal, failures))| (trace_id, terminal, failures))
        .collect();
    progress.sort_unstable_by_key(|&(trace_id, _, _)| trace_id);
    progress
}

/// Count each affected trace down, stamping completion on the one that reaches
/// zero, and claim any completion this instance owns.
///
/// Returns what to deliver. The delivery itself happens **after** the ack
/// commits: a completion handed to a caller inside the transaction would be a
/// lie if the transaction then rolled back, which it does when a lease has
/// expired.
async fn apply_trace_progress(
    conn: &DatabaseExecutor<'_>,
    store: &OutboxStore<'_>,
    mailbox: &super::subscription::Mailbox,
    progress: &[(i64, i64, i64)],
) -> Result<Vec<super::trace::TraceOutcome>, OutboxError> {
    let mut claimed = Vec::new();
    for &(trace_id, terminal, failures) in progress {
        let advanced = store
            .exec_trace_advance(conn, trace_id, terminal, failures, mailbox.instance_id())
            .await?;

        // Only an advance that reached zero can be claimed. Skipping the claim
        // otherwise spares the row every partition of the batch contends for
        // one guarded UPDATE per ack that did not finish it. `Unknown` is the
        // dialect that cannot say without another read, so it still tries.
        if matches!(advanced, super::trace::TraceAdvance::StillPending)
            || matches!(advanced, super::trace::TraceAdvance::NotAffected)
        {
            continue;
        }

        // Claims only a trace that has completed, is undelivered, and belongs
        // to this instance. Another instance's mail is left for its owner's
        // poller, which is what makes delivery the submitter's alone.
        if let Some(outcome) = store
            .exec_claim_trace_mail(conn, trace_id, mailbox.instance_id())
            .await?
        {
            claimed.push(outcome);
        }
    }
    Ok(claimed)
}

/// Hand claimed completions to whoever is waiting, once the ack has committed.
///
/// Mail with nobody waiting was stamped delivered by the claim and is
/// discarded here rather than retained: the guard was dropped, or this is not
/// the process that asked.
fn deliver_claimed(
    mailbox: &super::subscription::Mailbox,
    claimed: Vec<super::trace::TraceOutcome>,
) {
    for outcome in claimed {
        let trace = outcome.trace.clone();
        if !mailbox.subscriptions().deliver(outcome) {
            tracing::debug!(trace = %trace, "trace completed with nobody waiting on it");
        }
    }
}

/// The traces still represented past `upto_seq`, i.e. the ones a retry leaves
/// stuck. A trace entirely inside the acked prefix is progressing, not
/// stalling, and must not be marked.
fn traces_beyond(msgs: &[OutboxMessage], trace_ids: &TraceIds, upto_seq: i64) -> TraceIds {
    msgs.iter()
        .filter(|m| m.seq > upto_seq)
        .filter_map(|m| trace_ids.get(&m.seq).map(|&id| (m.seq, id)))
        .collect()
}

/// Record that every trace in this batch is being retried rather than
/// progressing, so a consumer can tell a stalled batch from a slow one.
async fn record_trace_retry(
    conn: &DatabaseExecutor<'_>,
    store: &OutboxStore<'_>,
    trace_ids: &TraceIds,
    reason: &str,
) -> Result<(), OutboxError> {
    // Sorted and deduplicated for the same reason the countdown is: these are
    // row locks, and two acks taking the same pair of traces in opposite
    // orders would deadlock.
    let mut traces: Vec<i64> = trace_ids.values().copied().collect();
    traces.sort_unstable();
    traces.dedup();
    for trace_id in traces {
        conn.execute_raw(Statement::from_sql_and_values(
            store.backend(),
            store.trace_retry(),
            [reason.into(), trace_id.into()],
        ))
        .await?;
    }
    Ok(())
}

/// The seqs a rejection list refers to.
fn rejected_seqs(msgs: &[OutboxMessage], rejections: &[super::batch::Rejection]) -> HashSet<i64> {
    rejections
        .iter()
        .filter_map(|rej| msgs.get(rej.index).map(|msg| msg.seq))
        .collect()
}

/// Append-only ack: only UPDATE `processed_seq`, no DELETEs.
/// Vacuum handles cleanup of processed outgoing + body rows.
async fn ack(
    conn: &DatabaseExecutor<'_>,
    store: &OutboxStore<'_>,
    partition_id: i64,
    msgs: &[OutboxMessage],
    trace_ids: &TraceIds,
    mailbox: &super::subscription::Mailbox,
    result: &HandlerResult,
) -> Result<Vec<super::trace::TraceOutcome>, OutboxError> {
    let last_seq = msgs.last().map_or(0, |m| m.seq);
    let mut claimed = Vec::new();

    match result {
        HandlerResult::Success => {
            conn.execute_raw(Statement::from_sql_and_values(
                store.backend(),
                store.advance_processed_seq(),
                [last_seq.into(), partition_id.into()],
            ))
            .await?;
            conn.execute_raw(Statement::from_sql_and_values(
                store.backend(),
                store.bump_vacuum_counter(),
                [partition_id.into()],
            ))
            .await?;

            let progress = trace_progress(msgs, trace_ids, last_seq, &HashSet::new());
            claimed = apply_trace_progress(conn, store, mailbox, &progress).await?;
        }
        HandlerResult::Retry { reason } => {
            conn.execute_raw(Statement::from_sql_and_values(
                store.backend(),
                store.record_retry(),
                [reason.as_str().into(), partition_id.into()],
            ))
            .await?;

            // Nothing reached a terminal state, so nothing is counted down -
            // the traces are recorded as stalled instead.
            record_trace_retry(conn, store, trace_ids, reason).await?;
        }
        HandlerResult::Reject { reason } => {
            for msg in msgs {
                conn.execute_raw(Statement::from_sql_and_values(
                    store.backend(),
                    store.insert_dead_letter(),
                    [
                        partition_id.into(),
                        msg.seq.into(),
                        msg.payload.clone().into(),
                        msg.payload_type.clone().into(),
                        msg.created_at.into(),
                        reason.as_str().into(),
                        msg.attempts.into(),
                        trace_ids.get(&msg.seq).copied().into(),
                    ],
                ))
                .await?;
            }

            conn.execute_raw(Statement::from_sql_and_values(
                store.backend(),
                store.advance_processed_seq(),
                [last_seq.into(), partition_id.into()],
            ))
            .await?;
            conn.execute_raw(Statement::from_sql_and_values(
                store.backend(),
                store.bump_vacuum_counter(),
                [partition_id.into()],
            ))
            .await?;

            // A dead letter has left the queue as surely as a delivered one,
            // and counts against its trace as a failure. Counted down last,
            // like the success path does it: a trace row is shared by every
            // partition the batch spread over, so the lock on it is held for
            // as little of the transaction as possible.
            let failed: HashSet<i64> = msgs.iter().map(|m| m.seq).collect();
            let progress = trace_progress(msgs, trace_ids, last_seq, &failed);
            claimed = apply_trace_progress(conn, store, mailbox, &progress).await?;
        }
    }

    Ok(claimed)
}

async fn try_lock_and_read_state(
    txn: &impl ConnectionTrait,
    store: &OutboxStore<'_>,
    partition_id: i64,
) -> Result<Option<ProcessorRow>, OutboxError> {
    if let Some(lock_sql) = store.lock_processor() {
        let row = txn
            .query_one_raw(Statement::from_sql_and_values(
                store.backend(),
                lock_sql,
                [partition_id.into()],
            ))
            .await?;
        if row.is_none() {
            return Ok(None);
        }
    }

    let proc_row = ProcessorRow::find_by_statement(Statement::from_sql_and_values(
        store.backend(),
        store.read_processor(),
        [partition_id.into()],
    ))
    .one(txn)
    .await?;

    Ok(proc_row)
}

// ---- Transactional strategy ----

/// Processes messages inside the DB transaction holding the partition lock.
/// Handler can perform atomic DB writes alongside the ack.
pub struct TransactionalStrategy {
    handler: Box<dyn TransactionalHandler>,
}

impl TransactionalStrategy {
    pub fn new(handler: Box<dyn TransactionalHandler>) -> Self {
        Self { handler }
    }
}

impl ProcessingStrategy for TransactionalStrategy {
    async fn process(
        &self,
        ctx: &ProcessContext<'_>,
        msg_batch_size: u32,
    ) -> Result<Option<ProcessResult>, OutboxError> {
        let conn = ctx.db.sea_internal();
        let txn = conn.begin().await?;

        let Some(proc_row) = try_lock_and_read_state(&txn, &ctx.store, ctx.partition_id).await?
        else {
            txn.commit().await?;
            return Ok(None);
        };

        let (msgs, trace_ids) = read_messages(
            &txn,
            &ctx.store,
            ctx.partition_id,
            &proc_row,
            msg_batch_size,
        )
        .await?;
        if msgs.is_empty() {
            txn.commit().await?;
            return Ok(None);
        }

        #[allow(clippy::cast_possible_truncation)]
        let count = msgs.len() as u32;

        let result = self
            .handler
            .handle(&DatabaseExecutor::Transaction(&txn), &msgs)
            .await;
        #[allow(clippy::cast_possible_truncation)]
        let pc = self.handler.processed_count().map(|n| n as u32);

        // Transactional partial-failure semantics: on Reject/Retry the entire
        // transaction (including any handler side-effects) is committed with
        // the ack. Dead letters are created for all messages in the batch on
        // Reject - even those the handler processed successfully - because
        // the handler's successful work is atomic with the cursor advance.
        // The `processed_count` is still recorded in ProcessResult so the
        // PartitionMode state machine can degrade batch size intelligently.
        let claimed = ack(
            &DatabaseExecutor::Transaction(&txn),
            &ctx.store,
            ctx.partition_id,
            &msgs,
            &trace_ids,
            ctx.mailbox,
            &result,
        )
        .await?;

        txn.commit().await?;

        // Committed, so the completion is now true and may be handed over.
        deliver_claimed(ctx.mailbox, claimed);

        Ok(Some(ProcessResult {
            count,
            handler_result: result,
            processed_count: pc,
        }))
    }
}

// ---- Shared lease helpers ----

/// Phase 1: Acquire a time-based lease and read messages.
/// Returns `None` if another processor holds the lease or no messages are available.
async fn acquire_lease_and_read(
    ctx: &ProcessContext<'_>,
    lease_id: &str,
    lease_secs: i64,
    msg_batch_size: u32,
) -> Result<Option<(Vec<OutboxMessage>, TraceIds)>, OutboxError> {
    let sea_conn = ctx.db.sea_internal();
    let txn = sea_conn.begin().await?;

    let proc_row = ctx
        .store
        .exec_lease_acquire(
            &DatabaseExecutor::Transaction(&txn),
            lease_id,
            lease_secs,
            ctx.partition_id,
        )
        .await?
        .map(|(processed_seq, attempts)| ProcessorRow {
            processed_seq,
            // lease_acquire increments attempts in the DB so a crash leaves
            // a trace. Subtract 1 so the handler sees the pre-increment
            // value (0 = first attempt, 1 = one previous attempt, etc.).
            attempts: attempts.saturating_sub(1),
        });

    let Some(proc_row) = proc_row else {
        txn.commit().await?;
        return Ok(None);
    };

    let (msgs, trace_ids) = read_messages(
        &txn,
        &ctx.store,
        ctx.partition_id,
        &proc_row,
        msg_batch_size,
    )
    .await?;

    txn.commit().await?;

    if msgs.is_empty() {
        // Release the lease AND reset `attempts` back to 0.
        // The increment from `lease_acquire` is a crash-detection trace only:
        // if the process crashes between acquire and ack, the next processor
        // sees a non-zero attempt count. On idle polls (no messages),
        // `lease_release` resets attempts so they do not accumulate across
        // empty cycles.
        let conn = ctx.db.sea_internal();
        conn.execute_raw(Statement::from_sql_and_values(
            ctx.store.backend(),
            ctx.store.lease_release(),
            [ctx.partition_id.into(), lease_id.into()],
        ))
        .await?;
        return Ok(None);
    }

    Ok(Some((msgs, trace_ids)))
}

// ---- Lease-guarded ack helpers ----

/// Persist per-message rejections from the blanket impl as dead-letter rows.
async fn persist_rejections(
    txn: &impl ConnectionTrait,
    ctx: &ProcessContext<'_>,
    msgs: &[OutboxMessage],
    rejections: &[super::batch::Rejection],
    trace_ids: &TraceIds,
) -> Result<(), OutboxError> {
    for rej in rejections {
        let msg = &msgs[rej.index];
        insert_dead_letter(txn, ctx, msg, &rej.reason, trace_ids.get(&msg.seq).copied()).await?;
    }
    Ok(())
}

/// Advance the cursor and bump the vacuum counter. Returns `false` if the
/// lease expired (caller must rollback). Releases the lease if `seq == 0`.
async fn advance_cursor(
    txn: &impl ConnectionTrait,
    ctx: &ProcessContext<'_>,
    seq: i64,
    lease_id: &str,
) -> Result<bool, OutboxError> {
    if seq == 0 {
        txn.execute_raw(Statement::from_sql_and_values(
            ctx.store.backend(),
            ctx.store.lease_release(),
            [ctx.partition_id.into(), lease_id.into()],
        ))
        .await?;
        return Ok(true);
    }

    let res = txn
        .execute_raw(Statement::from_sql_and_values(
            ctx.store.backend(),
            ctx.store.lease_ack_advance(),
            [seq.into(), ctx.partition_id.into(), lease_id.into()],
        ))
        .await?;
    if res.rows_affected() == 0 {
        return Ok(false);
    }

    txn.execute_raw(Statement::from_sql_and_values(
        ctx.store.backend(),
        ctx.store.bump_vacuum_counter(),
        [ctx.partition_id.into()],
    ))
    .await?;
    Ok(true)
}

/// Record a retry without advancing the cursor. Returns `false` if the
/// lease expired.
async fn record_retry(
    txn: &impl ConnectionTrait,
    ctx: &ProcessContext<'_>,
    reason: &str,
    lease_id: &str,
) -> Result<bool, OutboxError> {
    let res = txn
        .execute_raw(Statement::from_sql_and_values(
            ctx.store.backend(),
            ctx.store.lease_record_retry(),
            [reason.into(), ctx.partition_id.into(), lease_id.into()],
        ))
        .await?;
    Ok(res.rows_affected() > 0)
}

/// Sequence number of the last processed message, or 0 if nothing processed.
fn processed_advance_seq(msgs: &[OutboxMessage], processed: u32) -> i64 {
    if processed > 0 && (processed as usize) <= msgs.len() {
        msgs[processed as usize - 1].seq
    } else {
        0
    }
}

/// Phase 3: Lease-guarded ack. Used by `LeasedStrategy`.
async fn lease_guarded_ack(
    ctx: &ProcessContext<'_>,
    msgs: &[OutboxMessage],
    trace_ids: &TraceIds,
    lease_id: &str,
    result: HandlerResult,
    processed: u32,
    rejections: &[super::batch::Rejection],
) -> Result<Option<ProcessResult>, OutboxError> {
    let ack_conn = ctx.db.sea_internal();
    let ack_txn = ack_conn.begin().await?;
    let count = u32::try_from(msgs.len()).unwrap_or(u32::MAX);

    // All three branches persist rejections first, then differ in cursor behavior.
    let ack_exec = DatabaseExecutor::Transaction(&ack_txn);
    let mut claimed = Vec::new();
    persist_rejections(&ack_txn, ctx, msgs, rejections, trace_ids).await?;
    let rejected = rejected_seqs(msgs, rejections);

    let lease_ok = match &result {
        HandlerResult::Success => {
            let seq = processed_advance_seq(msgs, processed);
            // Cursor first, traces last. A trace row is shared by every
            // partition its batch spread over, so its lock is held for as
            // little of the transaction as possible - and a lost lease skips
            // the trace statements altogether, since the rollback would undo
            // them anyway.
            let ok = advance_cursor(&ack_txn, ctx, seq, lease_id).await?;
            if ok {
                let progress = trace_progress(msgs, trace_ids, seq, &rejected);
                claimed.extend(
                    apply_trace_progress(&ack_exec, &ctx.store, ctx.mailbox, &progress).await?,
                );
            }
            ok
        }
        HandlerResult::Retry { reason } => {
            let advance_seq = processed_advance_seq(msgs, processed);
            if advance_seq > 0 {
                // Partial progress: advance past processed prefix, retry the tail.
                let ok = advance_cursor(&ack_txn, ctx, advance_seq, lease_id).await?;
                if ok {
                    let progress = trace_progress(msgs, trace_ids, advance_seq, &rejected);
                    claimed.extend(
                        apply_trace_progress(&ack_exec, &ctx.store, ctx.mailbox, &progress).await?,
                    );
                    // Only the traces still represented in the retried tail.
                    // A trace whose entities were all in the acked prefix is
                    // making progress, and marking it stalled here would undo
                    // the clearing the line above just did.
                    let stalled = traces_beyond(msgs, trace_ids, advance_seq);
                    record_trace_retry(&ack_exec, &ctx.store, &stalled, reason).await?;
                }
                ok
            } else {
                // Nothing processed: record retry, no cursor advance.
                record_trace_retry(&ack_exec, &ctx.store, trace_ids, reason).await?;
                record_retry(&ack_txn, ctx, reason, lease_id).await?
            }
        }
        HandlerResult::Reject { reason } => {
            // Dead-letter the unprocessed tail (rejections already persisted above).
            let skip = (processed as usize).min(msgs.len());
            let mut failed = rejected.clone();
            for msg in &msgs[skip..] {
                insert_dead_letter(&ack_txn, ctx, msg, reason, trace_ids.get(&msg.seq).copied())
                    .await?;
                failed.insert(msg.seq);
            }
            // Advance past the entire batch (all messages handled or dead-lettered).
            let last_seq = msgs.last().map_or(0, |m| m.seq);
            let ok = advance_cursor(&ack_txn, ctx, last_seq, lease_id).await?;
            if ok {
                let progress = trace_progress(msgs, trace_ids, last_seq, &failed);
                claimed.extend(
                    apply_trace_progress(&ack_exec, &ctx.store, ctx.mailbox, &progress).await?,
                );
            }
            ok
        }
    };

    if !lease_ok {
        tracing::error!(
            partition_id = ctx.partition_id,
            "lease expired before ack, another processor may have taken over"
        );
        ack_txn.rollback().await?;
        // The claims are rolled back with everything else, and are
        // deliberately not delivered: a caller told its batch finished must
        // not learn it from a transaction that did not commit.
        return Ok(None);
    }

    ack_txn.commit().await?;
    deliver_claimed(ctx.mailbox, claimed);

    Ok(Some(ProcessResult {
        count,
        handler_result: result,
        processed_count: Some(processed),
    }))
}

/// Insert a single dead-letter row.
async fn insert_dead_letter(
    txn: &impl ConnectionTrait,
    ctx: &ProcessContext<'_>,
    msg: &OutboxMessage,
    reason: &str,
    trace_id: Option<i64>,
) -> Result<(), OutboxError> {
    txn.execute_raw(Statement::from_sql_and_values(
        ctx.store.backend(),
        ctx.store.insert_dead_letter(),
        [
            ctx.partition_id.into(),
            msg.seq.into(),
            msg.payload.clone().into(),
            msg.payload_type.clone().into(),
            msg.created_at.into(),
            reason.into(),
            msg.attempts.into(),
            trace_id.into(),
        ],
    ))
    .await?;
    Ok(())
}

// ---- Leased strategy ----

use std::sync::Arc;

/// Processes messages under a time-limited lease using `LeasedHandler`.
///
/// Three-phase pipeline: acquire lease + read, call handler with `timeout_at`,
/// lease-guarded ack. Cancellation is graceful: `batch.remaining()` signals
/// the handler to stop between messages; `timeout_at` is the hard backstop.
pub struct LeasedStrategy {
    handler: Arc<dyn LeasedHandler>,
    worker_id: String,
    lease_config: LeaseConfig,
}

impl LeasedStrategy {
    pub fn new(
        handler: Arc<dyn LeasedHandler>,
        worker_id: String,
        lease_config: LeaseConfig,
    ) -> Self {
        Self {
            handler,
            worker_id,
            lease_config,
        }
    }
}

impl ProcessingStrategy for LeasedStrategy {
    async fn process(
        &self,
        ctx: &ProcessContext<'_>,
        msg_batch_size: u32,
    ) -> Result<Option<ProcessResult>, OutboxError> {
        let lease_secs = i64::try_from(self.lease_config.duration.as_secs()).unwrap_or(i64::MAX);

        // Capture the clock before Phase 1 so that acquire+read time is
        // deducted from the handler budget. The DB lease starts at SQL
        // NOW(), so our Rust deadline must track the same origin.
        let lease_start = tokio::time::Instant::now();

        let Some((msgs, trace_ids)) =
            acquire_lease_and_read(ctx, &self.worker_id, lease_secs, msg_batch_size).await?
        else {
            return Ok(None);
        };

        // Phase 2: call handler with graceful two-phase cancellation.
        //
        // Soft signal: batch.remaining() returns Duration::ZERO after the
        // deadline. The blanket impl checks this between messages and stops
        // starting new work.
        //
        // Hard drop: timeout_at drops the handler future if it didn't return
        // before the deadline (catches handlers that ignore remaining()).
        let deadline = lease_start + self.lease_config.handler_budget();
        let mut batch = Batch::new(&msgs, deadline);

        let result = tokio::time::timeout_at(deadline, self.handler.handle(&mut batch))
            .await
            .unwrap_or_else(|_| HandlerResult::Retry {
                reason: "lease expired".into(),
            });

        // Phase 3: lease-guarded ack
        lease_guarded_ack(
            ctx,
            &msgs,
            &trace_ids,
            &self.worker_id,
            result,
            batch.processed(),
            batch.rejections(),
        )
        .await
    }
}

/// Generate a worker ID in the format `"{name}-{XXXXXX}"` where XXXXXX
/// is 6 random alphanumeric characters (A-Z, 0-9).
pub fn generate_worker_id(queue_name: &str) -> String {
    const CHARSET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos();
    // Simple PRNG seeded from nanosecond clock - sufficient for worker ID uniqueness
    let mut seed = u64::from(nanos) ^ u64::from(std::process::id());
    let mut suffix = String::with_capacity(6);
    for _ in 0..6 {
        seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
        let idx = ((seed >> 33) as usize) % CHARSET.len();
        suffix.push(CHARSET[idx] as char);
    }
    format!("{queue_name}-{suffix}")
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn worker_id_format() {
        let id = generate_worker_id("orders");
        assert!(id.starts_with("orders-"), "expected orders- prefix: {id}");
        let suffix = &id["orders-".len()..];
        assert_eq!(suffix.len(), 6, "suffix should be 6 chars: {suffix}");
        assert!(
            suffix
                .chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit()),
            "suffix should be A-Z0-9: {suffix}"
        );
    }

    #[test]
    fn worker_ids_differ() {
        let id1 = generate_worker_id("q");
        std::thread::sleep(std::time::Duration::from_millis(1));
        let id2 = generate_worker_id("q");
        assert_ne!(id1, id2, "worker IDs should differ: {id1} vs {id2}");
    }
}
