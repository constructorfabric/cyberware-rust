#![allow(dead_code)]

use sea_orm::DbBackend;

use super::dialect::{AllocSql, Dialect, VacuumSql};
use super::tables::OutboxTables;

pub(super) struct OutboxStatements {
    backend: DbBackend,
    dialect: Dialect,
    tables: OutboxTables,
    registration: RegistrationStatements,
    enqueue: EnqueueStatements,
    trace: TraceStatements,
    sequencer: SequencerStatements,
    processor: ProcessorStatements,
    vacuum: VacuumStatements,
    sweep: SweepStatements,
    dead_letters: DeadLetterStatements,
}

pub(super) struct RegistrationStatements {
    register_queue_select: String,
    register_queue_insert: String,
    insert_vacuum_counter_row: String,
}

pub(super) struct EnqueueStatements {
    insert_body_and_incoming_cte: Option<String>,
    insert_body: String,
    insert_incoming: String,
    insert_trace: String,
    body_id_reservation: Option<MySqlIdReservationStatements>,
    incoming_id_reservation: Option<MySqlIdReservationStatements>,
}

pub(super) struct MySqlIdReservationStatements {
    select_next_id_for_update: String,
    advance_next_id: String,
}
pub(super) struct TraceStatements {
    advance: String,
    retry: String,
    status: String,
    claim_mail: String,
    claim_mail_outcome: String,
    mail: String,
    stalled: String,
}

pub(super) struct SequencerStatements {
    allocate_sequences: AllocSql,
    lock_partition: Option<String>,
    discover_dirty_partitions: String,
}

pub(super) struct ProcessorStatements {
    insert_processor_row: String,
    lock_processor: Option<String>,
    advance_processed_seq: String,
    record_retry: String,
    insert_dead_letter: String,
    lease_acquire: String,
    lease_ack_advance: String,
    lease_record_retry: String,
    lease_release: String,
    read_processor: String,
}

/// Statements for collecting trace rows that have outlived their usefulness.
pub(super) struct SweepStatements {
    select_collectable_traces: String,
}

pub(super) struct VacuumStatements {
    cleanup: VacuumSql,
    bump_counter: String,
    fetch_dirty_partitions: String,
    decrement_counter: String,
    #[cfg(test)]
    reset_counter: String,
}

pub(super) struct DeadLetterStatements {
    select_columns: String,
    count_base: String,
    id_select_base: String,
}

impl OutboxStatements {
    pub(super) fn new(backend: DbBackend, tables: &OutboxTables) -> Self {
        let dialect = Dialect::from(backend);
        Self {
            backend,
            dialect,
            tables: tables.clone(),
            registration: RegistrationStatements::new(dialect, tables),
            enqueue: EnqueueStatements::new(dialect, tables),
            trace: TraceStatements::new(dialect, tables),
            sequencer: SequencerStatements::new(dialect, tables),
            processor: ProcessorStatements::new(dialect, tables),
            vacuum: VacuumStatements::new(dialect, tables),
            sweep: SweepStatements::new(dialect, tables),
            dead_letters: DeadLetterStatements::new(tables),
        }
    }

    pub(super) fn backend(&self) -> DbBackend {
        self.backend
    }

    pub(super) fn dialect(&self) -> Dialect {
        self.dialect
    }

    pub(super) fn tables(&self) -> &OutboxTables {
        &self.tables
    }

    pub(super) fn registration(&self) -> &RegistrationStatements {
        &self.registration
    }

    pub(super) fn enqueue(&self) -> &EnqueueStatements {
        &self.enqueue
    }

    pub(super) fn sequencer(&self) -> &SequencerStatements {
        &self.sequencer
    }

    pub(super) fn trace(&self) -> &TraceStatements {
        &self.trace
    }

    pub(super) fn sweep(&self) -> &SweepStatements {
        &self.sweep
    }

    pub(super) fn processor(&self) -> &ProcessorStatements {
        &self.processor
    }

    pub(super) fn vacuum(&self) -> &VacuumStatements {
        &self.vacuum
    }

    pub(super) fn dead_letters(&self) -> &DeadLetterStatements {
        &self.dead_letters
    }
}

impl RegistrationStatements {
    fn new(dialect: Dialect, tables: &OutboxTables) -> Self {
        Self {
            register_queue_select: register_queue_select(dialect, tables),
            register_queue_insert: register_queue_insert(dialect, tables),
            insert_vacuum_counter_row: insert_vacuum_counter_row(dialect, tables),
        }
    }

    pub(super) fn register_queue_select(&self) -> &str {
        &self.register_queue_select
    }

    pub(super) fn register_queue_insert(&self) -> &str {
        &self.register_queue_insert
    }

    pub(super) fn insert_vacuum_counter_row(&self) -> &str {
        &self.insert_vacuum_counter_row
    }
}

impl EnqueueStatements {
    fn new(dialect: Dialect, tables: &OutboxTables) -> Self {
        Self {
            insert_body_and_incoming_cte: insert_body_and_incoming_cte(dialect, tables),
            insert_body: insert_body(dialect, tables),
            insert_trace: insert_trace(dialect, tables),
            insert_incoming: insert_incoming(dialect, tables),
            body_id_reservation: mysql_id_reservation(dialect, tables.body_id_sequence()),
            incoming_id_reservation: mysql_id_reservation(dialect, tables.incoming_id_sequence()),
        }
    }

    pub(super) fn insert_body_and_incoming_cte(&self) -> Option<&str> {
        self.insert_body_and_incoming_cte.as_deref()
    }

    pub(super) fn insert_body(&self) -> &str {
        &self.insert_body
    }

    pub(super) fn insert_incoming(&self) -> &str {
        &self.insert_incoming
    }

    pub(super) fn insert_trace(&self) -> &str {
        &self.insert_trace
    }

    pub(super) fn body_id_reservation(&self) -> Option<&MySqlIdReservationStatements> {
        self.body_id_reservation.as_ref()
    }

    pub(super) fn incoming_id_reservation(&self) -> Option<&MySqlIdReservationStatements> {
        self.incoming_id_reservation.as_ref()
    }
}

impl MySqlIdReservationStatements {
    fn new(table: &str) -> Self {
        Self {
            select_next_id_for_update: format!(
                "SELECT next_id FROM {table} WHERE slot = 1 FOR UPDATE"
            ),
            advance_next_id: format!("UPDATE {table} SET next_id = next_id + ? WHERE slot = 1"),
        }
    }

    pub(super) fn select_next_id_for_update(&self) -> &str {
        &self.select_next_id_for_update
    }

    pub(super) fn advance_next_id(&self) -> &str {
        &self.advance_next_id
    }
}

impl TraceStatements {
    fn new(dialect: Dialect, tables: &OutboxTables) -> Self {
        Self {
            advance: trace_advance(dialect, tables),
            retry: trace_retry(dialect, tables),
            status: trace_status(dialect, tables),
            claim_mail: trace_claim_mail(dialect, tables),
            claim_mail_outcome: trace_claim_mail_outcome(dialect, tables),
            mail: trace_mail(dialect, tables),
            stalled: trace_stalled(dialect, tables),
        }
    }

    pub(super) fn advance(&self) -> &str {
        &self.advance
    }

    pub(super) fn retry(&self) -> &str {
        &self.retry
    }

    pub(super) fn status(&self) -> &str {
        &self.status
    }

    pub(super) fn claim_mail(&self) -> &str {
        &self.claim_mail
    }

    pub(super) fn claim_mail_outcome(&self) -> &str {
        &self.claim_mail_outcome
    }

    pub(super) fn mail(&self) -> &str {
        &self.mail
    }

    pub(super) fn stalled(&self) -> &str {
        &self.stalled
    }
}

impl SequencerStatements {
    fn new(dialect: Dialect, tables: &OutboxTables) -> Self {
        Self {
            allocate_sequences: allocate_sequences(dialect, tables),
            lock_partition: lock_partition(dialect, tables),
            discover_dirty_partitions: discover_dirty_partitions(tables),
        }
    }

    pub(super) fn allocate_sequences(&self) -> &AllocSql {
        &self.allocate_sequences
    }

    pub(super) fn lock_partition(&self) -> Option<&str> {
        self.lock_partition.as_deref()
    }

    pub(super) fn discover_dirty_partitions(&self) -> &str {
        &self.discover_dirty_partitions
    }
}

impl ProcessorStatements {
    fn new(dialect: Dialect, tables: &OutboxTables) -> Self {
        Self {
            insert_processor_row: insert_processor_row(dialect, tables),
            lock_processor: lock_processor(dialect, tables),
            advance_processed_seq: advance_processed_seq(dialect, tables),
            record_retry: record_retry(dialect, tables),
            insert_dead_letter: insert_dead_letter(dialect, tables),
            lease_acquire: lease_acquire(dialect, tables),
            lease_ack_advance: lease_ack_advance(dialect, tables),
            lease_record_retry: lease_record_retry(dialect, tables),
            lease_release: lease_release(dialect, tables),
            read_processor: read_processor(dialect, tables),
        }
    }

    pub(super) fn insert_processor_row(&self) -> &str {
        &self.insert_processor_row
    }

    pub(super) fn lock_processor(&self) -> Option<&str> {
        self.lock_processor.as_deref()
    }

    pub(super) fn advance_processed_seq(&self) -> &str {
        &self.advance_processed_seq
    }

    pub(super) fn record_retry(&self) -> &str {
        &self.record_retry
    }

    pub(super) fn insert_dead_letter(&self) -> &str {
        &self.insert_dead_letter
    }

    pub(super) fn lease_acquire(&self) -> &str {
        &self.lease_acquire
    }

    pub(super) fn lease_ack_advance(&self) -> &str {
        &self.lease_ack_advance
    }

    pub(super) fn lease_record_retry(&self) -> &str {
        &self.lease_record_retry
    }

    pub(super) fn lease_release(&self) -> &str {
        &self.lease_release
    }

    pub(super) fn read_processor(&self) -> &str {
        &self.read_processor
    }
}

impl SweepStatements {
    fn new(dialect: Dialect, tables: &OutboxTables) -> Self {
        let (p1, p2, p3, p4, p5) = match dialect {
            Dialect::Postgres | Dialect::Sqlite => ("$1", "$2", "$3", "$4", "$5"),
            Dialect::MySql => ("?", "?", "?", "?", "?"),
        };
        Self {
            // One table, and deliberately so: an anti-join against the body
            // or the dead letters would make a background sweep touch the
            // hottest table in the outbox every few minutes, unindexed.
            //
            // Liveness comes from the trace row's own fields instead. A trace
            // still working has `pending > 0` and is held by the fourth rule;
            // one that produced dead letters has `failures > 0` and is held
            // for longer, because a dead letter outlives the delivery it
            // failed. Nothing else needs another table to decide.
            select_collectable_traces: format!(
                "SELECT t.id FROM {trace} t \
                 WHERE ( \
                       (t.notified_at IS NOT NULL AND t.failures = 0 \
                        AND t.notified_at < {before1}) \
                    OR (t.completed_at IS NOT NULL AND t.notified_at IS NULL \
                        AND t.completed_at < {before2}) \
                    OR (t.notified_at IS NOT NULL AND t.failures > 0 \
                        AND t.notified_at < {before3}) \
                    OR (t.completed_at IS NULL AND t.created_at < {before4}) \
                 ) \
                 ORDER BY t.id LIMIT {p5}",
                trace = tables.trace(),
                before1 = seconds_ago(dialect, p1),
                before2 = seconds_ago(dialect, p2),
                before3 = seconds_ago(dialect, p3),
                before4 = seconds_ago(dialect, p4),
            ),
        }
    }

    pub(super) fn select_collectable_traces(&self) -> &str {
        &self.select_collectable_traces
    }
}

impl VacuumStatements {
    fn new(dialect: Dialect, tables: &OutboxTables) -> Self {
        Self {
            cleanup: vacuum_cleanup(dialect, tables),
            bump_counter: bump_vacuum_counter(dialect, tables),
            fetch_dirty_partitions: fetch_vacuum_dirty_partitions(dialect, tables),
            decrement_counter: decrement_vacuum_counter(dialect, tables),
            #[cfg(test)]
            reset_counter: reset_vacuum_counter(dialect, tables),
        }
    }

    pub(super) fn cleanup(&self) -> &VacuumSql {
        &self.cleanup
    }

    pub(super) fn bump_counter(&self) -> &str {
        &self.bump_counter
    }

    pub(super) fn fetch_dirty_partitions(&self) -> &str {
        &self.fetch_dirty_partitions
    }

    pub(super) fn decrement_counter(&self) -> &str {
        &self.decrement_counter
    }

    #[cfg(test)]
    pub(super) fn reset_counter(&self) -> &str {
        &self.reset_counter
    }
}

impl DeadLetterStatements {
    fn new(tables: &OutboxTables) -> Self {
        Self {
            select_columns: format!(
                "SELECT d.id, d.partition_id, d.seq, d.payload, d.payload_type, \
                 d.created_at, d.failed_at, d.last_error, d.attempts, d.status, \
                 d.completed_at, d.deadline FROM {} d",
                tables.dead_letters()
            ),
            count_base: format!("SELECT COUNT(*) AS cnt FROM {} d", tables.dead_letters()),
            id_select_base: format!("SELECT d.id FROM {} d", tables.dead_letters()),
        }
    }

    pub(super) fn select_columns(&self) -> &str {
        &self.select_columns
    }

    pub(super) fn count_base(&self) -> &str {
        &self.count_base
    }

    pub(super) fn id_select_base(&self) -> &str {
        &self.id_select_base
    }
}

fn register_queue_select(dialect: Dialect, tables: &OutboxTables) -> String {
    match dialect {
        Dialect::Postgres | Dialect::Sqlite => format!(
            "SELECT id FROM {} WHERE queue = $1 ORDER BY partition ASC",
            tables.partitions()
        ),
        Dialect::MySql => format!(
            "SELECT id FROM {} WHERE queue = ? ORDER BY `partition` ASC",
            tables.partitions()
        ),
    }
}

fn register_queue_insert(dialect: Dialect, tables: &OutboxTables) -> String {
    match dialect {
        Dialect::Postgres => format!(
            "INSERT INTO {} (queue, partition) \
             VALUES ($1, $2) ON CONFLICT (queue, partition) DO NOTHING",
            tables.partitions()
        ),
        Dialect::Sqlite => format!(
            "INSERT OR IGNORE INTO {} (queue, partition) VALUES ($1, $2)",
            tables.partitions()
        ),
        Dialect::MySql => format!(
            "INSERT IGNORE INTO {} (queue, `partition`) VALUES (?, ?)",
            tables.partitions()
        ),
    }
}

fn insert_body_and_incoming_cte(dialect: Dialect, tables: &OutboxTables) -> Option<String> {
    match dialect {
        Dialect::Postgres => Some(format!(
            "WITH b AS (\
               INSERT INTO {} (payload, payload_type, trace_id) \
               VALUES ($1, $2, $3) RETURNING id\
             ) \
             INSERT INTO {} (partition_id, body_id) \
             SELECT $4, id FROM b RETURNING id",
            tables.body(),
            tables.incoming()
        )),
        Dialect::Sqlite | Dialect::MySql => None,
    }
}

fn insert_body(dialect: Dialect, tables: &OutboxTables) -> String {
    match dialect {
        Dialect::Postgres | Dialect::Sqlite => format!(
            "INSERT INTO {} (payload, payload_type, trace_id) \
             VALUES ($1, $2, $3) RETURNING id",
            tables.body()
        ),
        Dialect::MySql => format!(
            "INSERT INTO {} (payload, payload_type, trace_id) VALUES (?, ?, ?)",
            tables.body()
        ),
    }
}

/// One row per traced batch. `pending` starts at `entities` and the ack counts
/// it down; reaching zero is what completion means.
fn insert_trace(dialect: Dialect, tables: &OutboxTables) -> String {
    match dialect {
        Dialect::Postgres | Dialect::Sqlite => format!(
            "INSERT INTO {} (trace, owner_instance, queue, entities, pending) \
             VALUES ($1, $2, $3, $4, $5) RETURNING id",
            tables.trace()
        ),
        Dialect::MySql => format!(
            "INSERT INTO {} (trace, owner_instance, queue, entities, pending) \
             VALUES (?, ?, ?, ?, ?)",
            tables.trace()
        ),
    }
}

fn insert_incoming(dialect: Dialect, tables: &OutboxTables) -> String {
    match dialect {
        Dialect::Postgres | Dialect::Sqlite => format!(
            "INSERT INTO {} (partition_id, body_id) VALUES ($1, $2) RETURNING id",
            tables.incoming()
        ),
        Dialect::MySql => format!(
            "INSERT INTO {} (partition_id, body_id) VALUES (?, ?)",
            tables.incoming()
        ),
    }
}

fn mysql_id_reservation(
    dialect: Dialect,
    sequence_table: &str,
) -> Option<MySqlIdReservationStatements> {
    match dialect {
        Dialect::MySql => Some(MySqlIdReservationStatements::new(sequence_table)),
        Dialect::Postgres | Dialect::Sqlite => None,
    }
}

fn allocate_sequences(dialect: Dialect, tables: &OutboxTables) -> AllocSql {
    match dialect {
        Dialect::Postgres | Dialect::Sqlite => AllocSql::UpdateReturning(format!(
            "UPDATE {} \
             SET sequence = sequence + $1 \
             WHERE id = $2 \
             RETURNING sequence - $1 AS start_seq",
            tables.partitions()
        )),
        Dialect::MySql => AllocSql::UpdateThenSelect {
            update: format!(
                "UPDATE {} SET sequence = sequence + ? WHERE id = ?",
                tables.partitions()
            ),
            select: format!(
                "SELECT sequence - ? AS start_seq FROM {} WHERE id = ?",
                tables.partitions()
            ),
        },
    }
}

/// Count a trace down by what an ack just terminalized.
///
/// The subtraction reads the value before this statement, so the completion
/// stamp lands in the same UPDATE that reaches zero. Guarded on `pending > 0`
/// so a stray advance cannot drive it negative, and clears the stall fields
/// because progress is the answer to a stall.
///
/// `attempts` is cleared by progress for the same reason, *except* on the
/// advance that completes the batch: the completion is read back from this row
/// and a caller being told its batch finished wants to know what it cost, so
/// zeroing it there would deliver a nought every time.
fn trace_advance(dialect: Dialect, tables: &OutboxTables) -> String {
    let now = now_expr(dialect);
    match dialect {
        Dialect::Postgres | Dialect::Sqlite => format!(
            "UPDATE {} \
             SET pending = pending - $1, \
                 failures = failures + $2, \
                 attempts = CASE WHEN pending - $3 <= 0 THEN attempts ELSE 0 END, \
                 stalled_since = NULL, \
                 last_error = NULL, \
                 completed_at = CASE WHEN pending - $4 <= 0 THEN {now} ELSE completed_at END \
             WHERE id = $5 AND pending > 0 \
             RETURNING pending",
            tables.trace()
        ),
        // No RETURNING here, so the claim rides the countdown instead: this
        // one statement both counts down and stamps delivery when the advance
        // reaches zero and this instance owns the trace. Otherwise every ack
        // would issue a separate guarded claim - and for a batch spread over N
        // partitions, N-1 of them can never match, on the row all N contend
        // for. The follow-up read is then paid per delivery, not per ack.
        Dialect::MySql => format!(
            "UPDATE {} \
             SET pending = pending - ?, \
                 failures = failures + ?, \
                 attempts = CASE WHEN pending - ? <= 0 THEN attempts ELSE 0 END, \
                 stalled_since = NULL, \
                 last_error = NULL, \
                 completed_at = CASE WHEN pending - ? <= 0 THEN {now} ELSE completed_at END, \
                 notified_at = CASE WHEN pending - ? <= 0 \
                                     AND notified_at IS NULL \
                                     AND owner_instance = ? \
                                THEN {now} ELSE notified_at END \
             WHERE id = ? AND pending > 0",
            tables.trace()
        ),
    }
}

/// Record that a trace is being retried rather than progressing.
///
/// `stalled_since` keeps the *first* stall time, so a consumer sees how long
/// the trace has been stuck rather than when it was last attempted.
fn trace_retry(dialect: Dialect, tables: &OutboxTables) -> String {
    let now = now_expr(dialect);
    match dialect {
        Dialect::Postgres | Dialect::Sqlite => format!(
            "UPDATE {} \
             SET attempts = attempts + 1, \
                 last_error = $1, \
                 stalled_since = COALESCE(stalled_since, {now}) \
             WHERE id = $2 AND completed_at IS NULL",
            tables.trace()
        ),
        Dialect::MySql => format!(
            "UPDATE {} \
             SET attempts = attempts + 1, \
                 last_error = ?, \
                 stalled_since = COALESCE(stalled_since, {now}) \
             WHERE id = ? AND completed_at IS NULL",
            tables.trace()
        ),
    }
}

/// The most recent trace recorded under a given trace string.
///
/// Nothing requires a caller to make its traces unique, so the newest is the
/// one reported.
fn trace_status(dialect: Dialect, tables: &OutboxTables) -> String {
    let placeholder = match dialect {
        Dialect::Postgres | Dialect::Sqlite => "$1",
        Dialect::MySql => "?",
    };
    format!(
        "SELECT trace, queue, entities, pending, failures, attempts, last_error, \
                stalled_since, created_at, completed_at \
         FROM {} \
         WHERE trace = {placeholder} \
         ORDER BY created_at DESC, id DESC \
         LIMIT 1",
        tables.trace()
    )
}

/// Claim one completed trace as delivered, and return what to deliver.
///
/// One statement decides everything: it stamps `notified_at` only if the trace
/// has completed, has not been delivered, and belongs to *this* instance. So a
/// row it affects is mail this instance may deliver, and a row it does not
/// affect is either unfinished or someone else's - no read is needed to tell
/// the cases apart, and two instances cannot both deliver.
fn trace_claim_mail(dialect: Dialect, tables: &OutboxTables) -> String {
    let now = now_expr(dialect);
    match dialect {
        Dialect::Postgres | Dialect::Sqlite => format!(
            "UPDATE {} SET notified_at = {now} \
             WHERE id = $1 \
               AND pending <= 0 \
               AND notified_at IS NULL \
               AND owner_instance = $2 \
             RETURNING trace, entities, failures, attempts, completed_at",
            tables.trace()
        ),
        // No RETURNING: the caller reads the row back only when the UPDATE
        // affected one, so the extra round trip is paid per delivery rather
        // than per attempt.
        Dialect::MySql => format!(
            "UPDATE {} SET notified_at = {now} \
             WHERE id = ? \
               AND pending <= 0 \
               AND notified_at IS NULL \
               AND owner_instance = ?",
            tables.trace()
        ),
    }
}

/// Read back what a trace this instance just claimed should deliver.
///
/// Guarded rather than a bare lookup by id, because on the dialect without
/// `RETURNING` the claim rides the countdown and this read is how the caller
/// learns whether it claimed anything. Exactly one ack can drive `pending` to
/// zero - every later advance is refused by `pending > 0` - so a row coming
/// back here means this ack is that one, and the owner check keeps another
/// instance's mail out of it.
fn trace_claim_mail_outcome(dialect: Dialect, tables: &OutboxTables) -> String {
    let (p1, p2) = match dialect {
        Dialect::Postgres | Dialect::Sqlite => ("$1", "$2"),
        Dialect::MySql => ("?", "?"),
    };
    format!(
        "SELECT trace, entities, failures, attempts, completed_at \
         FROM {} \
         WHERE id = {p1} \
           AND pending <= 0 \
           AND notified_at IS NOT NULL \
           AND owner_instance = {p2}",
        tables.trace()
    )
}

/// This instance's undelivered mail.
///
/// Only the id, because the claim returns the rest. On Postgres the supporting
/// index is partial on exactly this predicate, so the query is an index probe
/// that normally finds nothing.
fn trace_mail(dialect: Dialect, tables: &OutboxTables) -> String {
    match dialect {
        Dialect::Postgres | Dialect::Sqlite => format!(
            "SELECT id FROM {} \
             WHERE owner_instance = $1 \
               AND completed_at IS NOT NULL \
               AND notified_at IS NULL \
             ORDER BY completed_at LIMIT $2",
            tables.trace()
        ),
        Dialect::MySql => format!(
            "SELECT id FROM {} \
             WHERE owner_instance = ? \
               AND completed_at IS NOT NULL \
               AND notified_at IS NULL \
             ORDER BY completed_at LIMIT ?",
            tables.trace()
        ),
    }
}

/// This instance's traces that are stuck rather than merely slow.
///
/// A trace appears here from the moment a handler first retried one of its
/// entities until the batch makes progress again, which clears `stalled_since`
/// in the same UPDATE that counts the batch down. On Postgres the supporting
/// index is partial on exactly this predicate, so a healthy instance probes an
/// empty index.
fn trace_stalled(dialect: Dialect, tables: &OutboxTables) -> String {
    let (owner, limit) = match dialect {
        Dialect::Postgres | Dialect::Sqlite => ("$1", "$2"),
        Dialect::MySql => ("?", "?"),
    };
    format!(
        "SELECT trace, entities, pending, failures, attempts, last_error, stalled_since \
         FROM {} \
         WHERE owner_instance = {owner} \
           AND completed_at IS NULL \
           AND stalled_since IS NOT NULL \
         ORDER BY stalled_since LIMIT {limit}",
        tables.trace()
    )
}

/// The backend's expression for "`n` seconds before now", with `n` bound.
///
/// The arithmetic belongs in SQL, not in Rust: a bound timestamp is
/// serialised in a format that does not compare correctly against the text
/// `SQLite` stores for `datetime('now')` - the space separator sorts before
/// `T`, so every row would look older than any bound value. The lease
/// statements already compute their deadlines this way.
fn seconds_ago(dialect: Dialect, placeholder: &str) -> String {
    match dialect {
        Dialect::Postgres => format!("now() - ({placeholder} * INTERVAL '1 second')"),
        Dialect::Sqlite => format!("datetime('now', '-' || {placeholder} || ' seconds')"),
        Dialect::MySql => format!("DATE_SUB(NOW(6), INTERVAL {placeholder} SECOND)"),
    }
}

/// The backend's expression for "now".
const fn now_expr(dialect: Dialect) -> &'static str {
    match dialect {
        Dialect::Postgres => "now()",
        Dialect::Sqlite => "datetime('now')",
        Dialect::MySql => "NOW(6)",
    }
}

fn lock_partition(dialect: Dialect, tables: &OutboxTables) -> Option<String> {
    match dialect {
        Dialect::Postgres => Some(format!(
            "SELECT id FROM {} WHERE id = $1 FOR UPDATE SKIP LOCKED",
            tables.partitions()
        )),
        Dialect::MySql => Some(format!(
            "SELECT id FROM {} WHERE id = ? FOR UPDATE SKIP LOCKED",
            tables.partitions()
        )),
        Dialect::Sqlite => None,
    }
}

fn discover_dirty_partitions(tables: &OutboxTables) -> String {
    format!("SELECT DISTINCT partition_id FROM {}", tables.incoming())
}

fn insert_processor_row(dialect: Dialect, tables: &OutboxTables) -> String {
    match dialect {
        Dialect::Postgres => format!(
            "INSERT INTO {} (partition_id) \
             VALUES ($1) ON CONFLICT (partition_id) DO NOTHING",
            tables.processor()
        ),
        Dialect::Sqlite => format!(
            "INSERT OR IGNORE INTO {} (partition_id) VALUES ($1)",
            tables.processor()
        ),
        Dialect::MySql => format!(
            "INSERT IGNORE INTO {} (partition_id) VALUES (?)",
            tables.processor()
        ),
    }
}

fn lock_processor(dialect: Dialect, tables: &OutboxTables) -> Option<String> {
    match dialect {
        Dialect::Postgres => Some(format!(
            "SELECT partition_id, processed_seq, attempts \
             FROM {} WHERE partition_id = $1 FOR UPDATE SKIP LOCKED",
            tables.processor()
        )),
        Dialect::MySql => Some(format!(
            "SELECT partition_id, processed_seq, attempts \
             FROM {} WHERE partition_id = ? FOR UPDATE SKIP LOCKED",
            tables.processor()
        )),
        Dialect::Sqlite => None,
    }
}

fn advance_processed_seq(dialect: Dialect, tables: &OutboxTables) -> String {
    match dialect {
        Dialect::Postgres | Dialect::Sqlite => format!(
            "UPDATE {} \
             SET processed_seq = $1, attempts = 0, last_error = NULL \
             WHERE partition_id = $2",
            tables.processor()
        ),
        Dialect::MySql => format!(
            "UPDATE {} \
             SET processed_seq = ?, attempts = 0, last_error = NULL \
             WHERE partition_id = ?",
            tables.processor()
        ),
    }
}

fn record_retry(dialect: Dialect, tables: &OutboxTables) -> String {
    match dialect {
        Dialect::Postgres | Dialect::Sqlite => format!(
            "UPDATE {} \
             SET attempts = attempts + 1, last_error = $1 \
             WHERE partition_id = $2",
            tables.processor()
        ),
        Dialect::MySql => format!(
            "UPDATE {} \
             SET attempts = attempts + 1, last_error = ? \
             WHERE partition_id = ?",
            tables.processor()
        ),
    }
}

fn insert_dead_letter(dialect: Dialect, tables: &OutboxTables) -> String {
    match dialect {
        Dialect::Postgres | Dialect::Sqlite => format!(
            "INSERT INTO {} \
             (partition_id, seq, payload, payload_type, created_at, last_error, attempts, trace_id) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
            tables.dead_letters()
        ),
        Dialect::MySql => format!(
            "INSERT INTO {} \
             (partition_id, seq, payload, payload_type, created_at, last_error, attempts, trace_id) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            tables.dead_letters()
        ),
    }
}

fn lease_acquire(dialect: Dialect, tables: &OutboxTables) -> String {
    match dialect {
        Dialect::Postgres => format!(
            "UPDATE {} \
             SET locked_by = $1, locked_until = NOW() + $2 * INTERVAL '1 second', \
                 attempts = attempts + 1 \
             WHERE partition_id = $3 \
               AND (locked_by IS NULL OR locked_until < NOW()) \
             RETURNING processed_seq, attempts",
            tables.processor()
        ),
        Dialect::Sqlite => format!(
            "UPDATE {} \
             SET locked_by = $1, locked_until = datetime('now', '+' || $2 || ' seconds'), \
                 attempts = attempts + 1 \
             WHERE partition_id = $3 \
               AND (locked_by IS NULL OR locked_until < datetime('now')) \
             RETURNING processed_seq, attempts",
            tables.processor()
        ),
        Dialect::MySql => format!(
            "UPDATE {} \
             SET locked_by = ?, locked_until = DATE_ADD(NOW(6), INTERVAL ? SECOND), \
                 attempts = attempts + 1 \
             WHERE partition_id = ? \
               AND (locked_by IS NULL OR locked_until < NOW(6))",
            tables.processor()
        ),
    }
}

fn lease_ack_advance(dialect: Dialect, tables: &OutboxTables) -> String {
    match dialect {
        Dialect::Postgres | Dialect::Sqlite => format!(
            "UPDATE {} \
             SET processed_seq = $1, attempts = 0, last_error = NULL, \
                 locked_by = NULL, locked_until = NULL \
             WHERE partition_id = $2 AND locked_by = $3",
            tables.processor()
        ),
        Dialect::MySql => format!(
            "UPDATE {} \
             SET processed_seq = ?, attempts = 0, last_error = NULL, \
                 locked_by = NULL, locked_until = NULL \
             WHERE partition_id = ? AND locked_by = ?",
            tables.processor()
        ),
    }
}

fn lease_record_retry(dialect: Dialect, tables: &OutboxTables) -> String {
    match dialect {
        Dialect::Postgres | Dialect::Sqlite => format!(
            "UPDATE {} \
             SET last_error = $1, locked_by = NULL, locked_until = NULL \
             WHERE partition_id = $2 AND locked_by = $3",
            tables.processor()
        ),
        Dialect::MySql => format!(
            "UPDATE {} \
             SET last_error = ?, locked_by = NULL, locked_until = NULL \
             WHERE partition_id = ? AND locked_by = ?",
            tables.processor()
        ),
    }
}

fn lease_release(dialect: Dialect, tables: &OutboxTables) -> String {
    match dialect {
        Dialect::Postgres | Dialect::Sqlite => format!(
            "UPDATE {} \
             SET attempts = 0, locked_by = NULL, locked_until = NULL \
             WHERE partition_id = $1 AND locked_by = $2",
            tables.processor()
        ),
        Dialect::MySql => format!(
            "UPDATE {} \
             SET attempts = 0, locked_by = NULL, locked_until = NULL \
             WHERE partition_id = ? AND locked_by = ?",
            tables.processor()
        ),
    }
}

fn read_processor(dialect: Dialect, tables: &OutboxTables) -> String {
    match dialect {
        Dialect::Postgres | Dialect::Sqlite => format!(
            "SELECT processed_seq, attempts FROM {} WHERE partition_id = $1",
            tables.processor()
        ),
        Dialect::MySql => format!(
            "SELECT processed_seq, attempts FROM {} WHERE partition_id = ?",
            tables.processor()
        ),
    }
}

fn vacuum_cleanup(dialect: Dialect, tables: &OutboxTables) -> VacuumSql {
    match dialect {
        Dialect::Postgres | Dialect::Sqlite => VacuumSql {
            select_outgoing_chunk: format!(
                "SELECT id, body_id FROM {} \
                 WHERE partition_id = $1 AND seq <= $2 \
                 ORDER BY seq LIMIT $3",
                tables.outgoing()
            ),
        },
        Dialect::MySql => VacuumSql {
            select_outgoing_chunk: format!(
                "SELECT id, body_id FROM {} \
                 WHERE partition_id = ? AND seq <= ? \
                 ORDER BY seq LIMIT ?",
                tables.outgoing()
            ),
        },
    }
}

fn bump_vacuum_counter(dialect: Dialect, tables: &OutboxTables) -> String {
    match dialect {
        Dialect::Postgres | Dialect::Sqlite => format!(
            "UPDATE {} SET counter = counter + 1 WHERE partition_id = $1",
            tables.vacuum_counter()
        ),
        Dialect::MySql => format!(
            "UPDATE {} SET counter = counter + 1 WHERE partition_id = ?",
            tables.vacuum_counter()
        ),
    }
}

fn fetch_vacuum_dirty_partitions(dialect: Dialect, tables: &OutboxTables) -> String {
    match dialect {
        Dialect::Postgres | Dialect::Sqlite => format!(
            "SELECT partition_id, counter \
             FROM {} \
             WHERE counter > 0 AND partition_id > $1 \
             ORDER BY partition_id LIMIT $2",
            tables.vacuum_counter()
        ),
        Dialect::MySql => format!(
            "SELECT partition_id, counter \
             FROM {} \
             WHERE counter > 0 AND partition_id > ? \
             ORDER BY partition_id LIMIT ?",
            tables.vacuum_counter()
        ),
    }
}

fn decrement_vacuum_counter(dialect: Dialect, tables: &OutboxTables) -> String {
    match dialect {
        Dialect::Postgres => format!(
            "UPDATE {} \
             SET counter = GREATEST(counter - $1, 0) \
             WHERE partition_id = $2",
            tables.vacuum_counter()
        ),
        Dialect::Sqlite => format!(
            "UPDATE {} \
             SET counter = MAX(counter - $1, 0) \
             WHERE partition_id = $2",
            tables.vacuum_counter()
        ),
        Dialect::MySql => format!(
            "UPDATE {} \
             SET counter = GREATEST(counter - ?, 0) \
             WHERE partition_id = ?",
            tables.vacuum_counter()
        ),
    }
}

#[cfg(test)]
fn reset_vacuum_counter(dialect: Dialect, tables: &OutboxTables) -> String {
    match dialect {
        Dialect::Postgres | Dialect::Sqlite => format!(
            "UPDATE {} SET counter = 0 WHERE partition_id = $1",
            tables.vacuum_counter()
        ),
        Dialect::MySql => format!(
            "UPDATE {} SET counter = 0 WHERE partition_id = ?",
            tables.vacuum_counter()
        ),
    }
}

/// One row per partition, created at registration and never deleted.
fn insert_vacuum_counter_row(dialect: Dialect, tables: &OutboxTables) -> String {
    let table = tables.vacuum_counter();
    match dialect {
        Dialect::Postgres => format!(
            "INSERT INTO {table} (partition_id) \
             VALUES ($1) ON CONFLICT (partition_id) DO NOTHING"
        ),
        Dialect::Sqlite => format!("INSERT OR IGNORE INTO {table} (partition_id) VALUES ($1)"),
        Dialect::MySql => format!("INSERT IGNORE INTO {table} (partition_id) VALUES (?)"),
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    fn statements(backend: DbBackend) -> OutboxStatements {
        OutboxStatements::new(backend, &OutboxTables::default())
    }

    #[test]
    fn postgres_uses_dollar_placeholders() {
        let statements = statements(DbBackend::Postgres);

        assert!(statements.enqueue().insert_body().contains("$1"));
        assert!(statements.enqueue().insert_body().contains("$2"));
        assert!(statements.enqueue().insert_body().contains("RETURNING id"));
        assert!(
            statements
                .registration()
                .register_queue_insert()
                .contains("ON CONFLICT")
        );
        assert!(statements.processor().lease_acquire().contains("$3"));
    }

    #[test]
    fn sqlite_omits_lock_statements() {
        let statements = statements(DbBackend::Sqlite);

        assert!(statements.sequencer().lock_partition().is_none());
        assert!(statements.processor().lock_processor().is_none());
        assert!(statements.enqueue().insert_body().contains("RETURNING id"));
    }

    #[test]
    fn mysql_uses_question_placeholders_and_lock_statements() {
        let statements = statements(DbBackend::MySql);

        assert!(statements.enqueue().insert_body().contains('?'));
        assert!(!statements.enqueue().insert_body().contains('$'));
        assert!(
            statements
                .enqueue()
                .insert_body_and_incoming_cte()
                .is_none()
        );
        assert!(
            statements
                .registration()
                .register_queue_select()
                .contains("`partition`")
        );
        assert!(
            statements
                .sequencer()
                .lock_partition()
                .expect("mysql lock partition")
                .contains("FOR UPDATE SKIP LOCKED")
        );
        assert!(
            statements
                .processor()
                .lock_processor()
                .expect("mysql lock processor")
                .contains("FOR UPDATE SKIP LOCKED")
        );
        assert!(
            statements
                .enqueue()
                .body_id_reservation()
                .expect("body id reservation")
                .select_next_id_for_update()
                .contains("toolkit_outbox_body_id_sequence")
        );
        assert!(
            statements
                .enqueue()
                .incoming_id_reservation()
                .expect("incoming id reservation")
                .advance_next_id()
                .contains("toolkit_outbox_incoming_id_sequence")
        );
    }

    #[test]
    fn statements_use_custom_prefix_without_replacing_inside_names() {
        let tables = OutboxTables::new("toolkit_outbox_body").expect("valid prefix");
        let statements = OutboxStatements::new(DbBackend::Postgres, &tables);

        assert!(
            statements
                .enqueue()
                .insert_body()
                .contains("toolkit_outbox_body_body")
        );
        assert!(
            statements
                .enqueue()
                .insert_incoming()
                .contains("toolkit_outbox_body_incoming")
        );
        assert!(
            statements
                .processor()
                .insert_dead_letter()
                .contains("toolkit_outbox_body_dead_letters")
        );
        assert!(
            !statements
                .processor()
                .insert_dead_letter()
                .contains("toolkit_outbox_body_body_dead_letters")
        );
        assert!(
            statements
                .dead_letters()
                .count_base()
                .contains("toolkit_outbox_body_dead_letters")
        );
    }

    /// Every value's position must be the same under both placeholder schemes.
    ///
    /// `Postgres` and `SQLite` take numbered `$n`, `MySQL` takes positional `?`, and
    /// one values vector serves both. So a `$n` sequence that does not ascend
    /// textually binds correctly on one backend and to the wrong columns on
    /// the other - silently, because the types usually still fit.
    /// The dialects that number their parameters tolerate a placeholder
    /// repeated for the same value; the one that uses bare positional markers
    /// silently expects one value per marker. A statement written with a
    /// repeated placeholder therefore works on two backends and takes the
    /// wrong number of values on the third, which is a defect no amount of
    /// `SQLite` testing can see. So every statement must want the same number of
    /// values on every backend.
    #[test]
    fn every_statement_wants_the_same_parameter_count_on_every_backend() {
        fn highest_numbered(sql: &str) -> usize {
            let bytes = sql.as_bytes();
            let mut highest = 0;
            for (i, b) in bytes.iter().enumerate() {
                if *b != b'$' {
                    continue;
                }
                if let Some(d) = bytes.get(i + 1).and_then(|c| char::from(*c).to_digit(10)) {
                    highest = highest.max(d as usize);
                }
            }
            highest
        }
        fn positional(sql: &str) -> usize {
            sql.bytes().filter(|b| *b == b'?').count()
        }

        let pg = statements(DbBackend::Postgres);
        let my = statements(DbBackend::MySql);
        let pairs: Vec<(&str, &str, &str)> = vec![
            (
                "insert_trace",
                pg.enqueue().insert_trace(),
                my.enqueue().insert_trace(),
            ),
            ("trace_retry", pg.trace().retry(), my.trace().retry()),
            ("trace_status", pg.trace().status(), my.trace().status()),
            (
                "trace_claim_mail",
                pg.trace().claim_mail(),
                my.trace().claim_mail(),
            ),
            ("trace_mail", pg.trace().mail(), my.trace().mail()),
            ("trace_stalled", pg.trace().stalled(), my.trace().stalled()),
            (
                "select_collectable_traces",
                pg.sweep().select_collectable_traces(),
                my.sweep().select_collectable_traces(),
            ),
            (
                "advance_processed_seq",
                pg.processor().advance_processed_seq(),
                my.processor().advance_processed_seq(),
            ),
            (
                "record_retry",
                pg.processor().record_retry(),
                my.processor().record_retry(),
            ),
            (
                "lease_ack_advance",
                pg.processor().lease_ack_advance(),
                my.processor().lease_ack_advance(),
            ),
            (
                "lease_record_retry",
                pg.processor().lease_record_retry(),
                my.processor().lease_record_retry(),
            ),
            (
                "lease_release",
                pg.processor().lease_release(),
                my.processor().lease_release(),
            ),
            (
                "read_processor",
                pg.processor().read_processor(),
                my.processor().read_processor(),
            ),
            (
                "decrement_vacuum_counter",
                pg.vacuum().decrement_counter(),
                my.vacuum().decrement_counter(),
            ),
            (
                "bump_vacuum_counter",
                pg.vacuum().bump_counter(),
                my.vacuum().bump_counter(),
            ),
            (
                "fetch_dirty_partitions",
                pg.vacuum().fetch_dirty_partitions(),
                my.vacuum().fetch_dirty_partitions(),
            ),
        ];

        // One statement deliberately differs, and a deliberate difference has
        // to be declared rather than quietly skipped: on the dialect without
        // `RETURNING`, the countdown also stamps delivery, so it takes the
        // completion test and the owner as well. Pinned here so a change to
        // either side still has to come past this test.
        assert_eq!(
            (
                highest_numbered(pg.trace().advance()),
                positional(my.trace().advance())
            ),
            (5, 7),
            "trace_advance: numbered dialects count down; the positional one \
             counts down and claims in the same statement"
        );

        let mut checked = 0;
        for (name, pg_sql, my_sql) in pairs {
            assert_eq!(
                highest_numbered(pg_sql),
                positional(my_sql),
                "{name} wants a different number of values per backend\n  numbered: {pg_sql}\n  positional: {my_sql}"
            );
            checked += 1;
        }
        assert!(checked >= 16, "only {checked} statements checked");
    }

    #[test]
    fn every_statement_binds_its_values_in_textual_order() {
        fn placeholder_order(sql: &str) -> Vec<u32> {
            let bytes = sql.as_bytes();
            let mut seen: Vec<u32> = Vec::new();
            for (i, b) in bytes.iter().enumerate() {
                if *b != b'$' {
                    continue;
                }
                if let Some(d) = bytes.get(i + 1).and_then(|c| char::from(*c).to_digit(10))
                    && !seen.contains(&d)
                {
                    seen.push(d);
                }
            }
            seen
        }

        let pg = statements(DbBackend::Postgres);
        let mut checked = 0;
        let mut statements_to_check: Vec<String> = vec![
            pg.enqueue().insert_body().to_owned(),
            pg.enqueue().insert_incoming().to_owned(),
            pg.enqueue().insert_trace().to_owned(),
            pg.trace().advance().to_owned(),
            pg.trace().retry().to_owned(),
            pg.trace().status().to_owned(),
            pg.trace().claim_mail().to_owned(),
            pg.trace().mail().to_owned(),
            pg.trace().stalled().to_owned(),
            pg.processor().advance_processed_seq().to_owned(),
            pg.processor().record_retry().to_owned(),
            pg.processor().lease_ack_advance().to_owned(),
            pg.processor().lease_record_retry().to_owned(),
            pg.processor().lease_release().to_owned(),
            pg.processor().read_processor().to_owned(),
            pg.vacuum().decrement_counter().to_owned(),
            pg.vacuum().bump_counter().to_owned(),
            pg.vacuum().fetch_dirty_partitions().to_owned(),
            pg.vacuum().cleanup().select_outgoing_chunk.clone(),
            pg.sweep().select_collectable_traces().to_owned(),
        ];
        if let AllocSql::UpdateReturning(sql) = pg.sequencer().allocate_sequences() {
            statements_to_check.push(sql.clone());
        }

        for sql in statements_to_check {
            let order = placeholder_order(&sql);
            let ascending: Vec<u32> = (1..=u32::try_from(order.len()).unwrap_or(0)).collect();
            assert_eq!(order, ascending, "placeholders out of textual order: {sql}");
            checked += 1;
        }
        assert!(checked >= 21, "only {checked} statements checked");
    }

    #[test]
    fn allocation_and_vacuum_groups_keep_backend_syntax() {
        let pg = statements(DbBackend::Postgres);
        match pg.sequencer().allocate_sequences() {
            AllocSql::UpdateReturning(sql) => {
                assert!(sql.contains("$1"));
                assert!(sql.contains("$2"));
                assert!(sql.contains("RETURNING sequence - $1"));
            }
            AllocSql::UpdateThenSelect { .. } => panic!("postgres should use returning"),
        }
        assert!(
            pg.vacuum()
                .cleanup()
                .select_outgoing_chunk
                .contains("LIMIT $3")
        );

        let mysql = statements(DbBackend::MySql);
        match mysql.sequencer().allocate_sequences() {
            AllocSql::UpdateReturning(_) => panic!("mysql should use update then select"),
            AllocSql::UpdateThenSelect { update, select } => {
                assert!(update.contains('?'));
                assert!(select.contains('?'));
                assert!(!update.contains('$'));
                assert!(!select.contains('$'));
            }
        }
        assert!(
            mysql
                .vacuum()
                .cleanup()
                .select_outgoing_chunk
                .contains("LIMIT ?")
        );
    }

    #[test]
    fn registration_processor_vacuum_and_dead_letter_groups_are_present() {
        let statements = statements(DbBackend::Postgres);

        assert!(
            statements
                .registration()
                .register_queue_select()
                .contains("toolkit_outbox_partitions")
        );
        assert!(
            statements
                .registration()
                .insert_vacuum_counter_row()
                .contains("toolkit_outbox_vacuum_counter")
        );
        assert!(
            statements
                .processor()
                .read_processor()
                .contains("toolkit_outbox_processor")
        );
        assert!(
            statements
                .vacuum()
                .fetch_dirty_partitions()
                .contains("toolkit_outbox_vacuum_counter")
        );
        assert!(
            statements
                .dead_letters()
                .select_columns()
                .contains("toolkit_outbox_dead_letters")
        );
    }
}
