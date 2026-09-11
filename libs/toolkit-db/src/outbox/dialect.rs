use std::fmt::Write as _;

use sea_orm::DbBackend;

use super::tables::OutboxTables;

/// Backend-specific SQL dialect for the outbox gear.
///
/// Centralizes all DML differences between `Postgres`, `SQLite`, and `MySQL`
/// so that `core.rs` and `sequencer.rs` contain zero `match backend` blocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialect {
    Postgres,
    /// `SQLite`: single-process only. No row-level locking — `lock_partition()`
    /// and `lock_processor()` return `None`. Do not run multiple outbox
    /// instances against the same `SQLite` database.
    Sqlite,
    /// `MySQL` 8.0+ required. Uses `FOR UPDATE SKIP LOCKED` for partition
    /// locking and sequencer claims, which is not available in `MySQL` 5.7
    /// or earlier.
    MySql,
}

impl From<DbBackend> for Dialect {
    /// # Panics
    ///
    /// Panics if `backend` is not one of `Postgres`, `SQLite`, or `MySQL`.
    /// `DbBackend` is `#[non_exhaustive]` as of `SeaORM` 2.0, so this arm exists
    /// for forward compatibility only — it is unreachable with the variants
    /// `SeaORM` defines today. This is the single point where a backend is
    /// narrowed to a dialect, so every SQL builder downstream can match
    /// `Dialect` exhaustively instead of guessing at an unknown backend and
    /// emitting SQL that silently targets the wrong engine.
    fn from(backend: DbBackend) -> Self {
        match backend {
            DbBackend::Postgres => Self::Postgres,
            DbBackend::Sqlite => Self::Sqlite,
            DbBackend::MySql => Self::MySql,
            other => unsupported_backend(other),
        }
    }
}

/// Diverging helper for the unreachable arm of [`Dialect::from`].
///
/// `coverage(off)` sits here rather than on `from` itself so the three real arms
/// stay in the coverage numerator (they are exercised by `dialect_from_dbbackend`)
/// while the unreachable panic body leaves the denominator. `DbBackend` has
/// exactly three constructible variants, so no test can reach this.
#[cfg_attr(coverage_nightly, coverage(off))]
fn unsupported_backend(backend: DbBackend) -> ! {
    panic!(
        "toolkit-db outbox supports Postgres, SQLite and MySQL only; \
         got unsupported database backend {backend:?}"
    )
}

/// SQL for the vacuum's bounded-chunk cleanup operation.
///
/// Strategy: SELECT a bounded chunk of (id, `body_id`) from outgoing, then
/// DELETE those outgoing rows by ID, then DELETE body rows by ID.
/// The caller loops while `deleted == batch_size` (more work likely).
pub struct VacuumSql {
    /// SELECT id, `body_id` with LIMIT for bounded chunk deletion.
    /// Parameters: `partition_id`, `processed_seq`, limit.
    pub select_outgoing_chunk: String,
}

/// SQL for the sequencer's claim-incoming operation.
///
/// All backends use SELECT-then-DELETE to guarantee FIFO ordering:
/// the SELECT returns rows ordered by `id`, and the sequencer assigns
/// sequences in that order before deleting.
pub struct ClaimSql {
    /// SELECT query that returns `id, body_id` ordered by `id`.
    /// Pg/MySQL append `FOR UPDATE`; `SQLite` omits it (no row locking).
    pub select: String,
}

/// SQL for the sequencer's sequence-allocation operation.
pub enum AllocSql {
    /// `Pg`/`SQLite`: single `UPDATE ... RETURNING` statement.
    UpdateReturning(String),
    /// `MySQL`: `UPDATE` then `SELECT` as two separate statements.
    UpdateThenSelect { update: String, select: String },
}

// -- Single-row insert queries --

impl Dialect {
    pub(super) fn supports_returning(self) -> bool {
        match self {
            Self::Postgres | Self::Sqlite => true,
            Self::MySql => false,
        }
    }

    /// Returns the `MySQL` query to retrieve the last auto-generated ID.
    pub(super) fn last_insert_id() -> &'static str {
        "SELECT CAST(LAST_INSERT_ID() AS SIGNED) AS id"
    }
}

// -- Batch insert builders --

impl Dialect {
    /// Build a multi-row INSERT for body rows.
    ///
    /// `MySQL` note: consecutive auto-increment IDs are guaranteed by `InnoDB`
    /// for a single multi-row INSERT when `innodb_autoinc_lock_mode` is 0 or 1.
    pub fn build_insert_body_batch(self, tables: &OutboxTables, count: usize) -> String {
        let mut sql = format!(
            "INSERT INTO {} (payload, payload_type, trace_id) VALUES ",
            tables.body()
        );
        self.append_value_tuples(&mut sql, count, 3);
        if self.supports_returning() {
            sql.push_str(" RETURNING id");
        }
        sql
    }

    pub fn build_insert_incoming_batch(self, tables: &OutboxTables, count: usize) -> String {
        let mut sql = format!(
            "INSERT INTO {} (partition_id, body_id) VALUES ",
            tables.incoming()
        );
        self.append_value_tuples(&mut sql, count, 2);
        if self.supports_returning() {
            sql.push_str(" RETURNING id");
        }
        sql
    }

    /// Build `SELECT id, payload, payload_type, created_at FROM toolkit_outbox_body WHERE id IN (...)`.
    pub fn build_read_body_batch(self, tables: &OutboxTables, count: usize) -> String {
        let mut sql = format!(
            "SELECT id, payload, payload_type, created_at, trace_id FROM {} WHERE id IN (",
            tables.body()
        );
        self.append_in_placeholders(&mut sql, count);
        sql.push(')');
        sql
    }

    /// Append `$1, $2, ...` or `?, ?, ...` placeholders for an IN clause.
    fn append_in_placeholders(self, sql: &mut String, count: usize) {
        for i in 0..count {
            if i > 0 {
                sql.push_str(", ");
            }
            match self {
                Self::Postgres | Self::Sqlite => {
                    #[allow(clippy::let_underscore_must_use)]
                    let _ = write!(sql, "${}", i + 1);
                }
                Self::MySql => {
                    sql.push('?');
                }
            }
        }
    }

    /// Append `(p1, p2), (p3, p4), ...` with correct placeholder style.
    fn append_value_tuples(self, sql: &mut String, row_count: usize, cols: usize) {
        for i in 0..row_count {
            if i > 0 {
                sql.push_str(", ");
            }
            sql.push('(');
            for c in 0..cols {
                if c > 0 {
                    sql.push_str(", ");
                }
                match self {
                    Self::Postgres | Self::Sqlite => {
                        let idx = i * cols + c + 1;
                        // Writing to a String is infallible.
                        #[allow(clippy::let_underscore_must_use)]
                        let _ = write!(sql, "${idx}");
                    }
                    Self::MySql => {
                        sql.push('?');
                    }
                }
            }
            sql.push(')');
        }
    }
}

// -- Sequencer queries --

impl Dialect {
    pub fn claim_incoming(self, tables: &OutboxTables, batch_size: u32) -> ClaimSql {
        match self {
            Self::Postgres => ClaimSql {
                select: format!(
                    "SELECT id, body_id \
                     FROM {} \
                     WHERE partition_id = $1 \
                     ORDER BY id \
                     LIMIT {batch_size} \
                     FOR UPDATE SKIP LOCKED",
                    tables.incoming()
                ),
            },
            Self::Sqlite => ClaimSql {
                select: format!(
                    "SELECT id, body_id \
                     FROM {} \
                     WHERE partition_id = $1 \
                     ORDER BY id \
                     LIMIT {batch_size}",
                    tables.incoming()
                ),
            },
            // SKIP LOCKED prevents InnoDB gap-lock deadlocks when
            // multiple sequencers claim from adjacent partitions.
            Self::MySql => ClaimSql {
                select: format!(
                    "SELECT id, body_id \
                     FROM {} \
                     WHERE partition_id = ? \
                     ORDER BY id \
                     LIMIT {batch_size} \
                     FOR UPDATE SKIP LOCKED",
                    tables.incoming()
                ),
            },
        }
    }

    /// Build `DELETE FROM toolkit_outbox_incoming WHERE id IN ($1, $2, ...)`.
    pub fn delete_incoming_batch(self, tables: &OutboxTables, count: usize) -> String {
        let mut sql = format!("DELETE FROM {} WHERE id IN (", tables.incoming());
        for i in 0..count {
            if i > 0 {
                sql.push_str(", ");
            }
            match self {
                Self::Postgres | Self::Sqlite => {
                    // Writing to a String is infallible.
                    #[allow(clippy::let_underscore_must_use)]
                    let _ = write!(sql, "${}", i + 1);
                }
                Self::MySql => {
                    sql.push('?');
                }
            }
        }
        sql.push(')');
        sql
    }

    pub fn build_insert_outgoing_batch(self, tables: &OutboxTables, count: usize) -> String {
        let mut sql = format!(
            "INSERT INTO {} (partition_id, body_id, seq) VALUES ",
            tables.outgoing()
        );
        self.append_value_tuples(&mut sql, count, 3);
        sql
    }
}

// -- Processor queries --

impl Dialect {
    pub fn read_outgoing_batch(self, tables: &OutboxTables, batch_size: u32) -> String {
        match self {
            Self::Postgres | Self::Sqlite => format!(
                "SELECT id, body_id, seq \
                 FROM {} \
                 WHERE partition_id = $1 AND seq > $2 \
                 ORDER BY seq \
                 LIMIT {batch_size}",
                tables.outgoing()
            ),
            Self::MySql => format!(
                "SELECT id, body_id, seq \
                 FROM {} \
                 WHERE partition_id = ? AND seq > ? \
                 ORDER BY seq \
                 LIMIT {batch_size}",
                tables.outgoing()
            ),
        }
    }

    /// Build `DELETE FROM <prefix>_trace WHERE id IN ($1, $2, ...)`.
    ///
    /// Selected then deleted by id, rather than one statement with a
    /// subquery, because `MySQL` will not delete from a table its own
    /// subquery reads.
    pub fn build_delete_traces(self, tables: &OutboxTables, count: usize) -> String {
        let mut sql = format!("DELETE FROM {} WHERE id IN (", tables.trace());
        self.append_in_placeholders(&mut sql, count);
        sql.push(')');
        sql
    }

    /// Build `DELETE FROM toolkit_outbox_outgoing WHERE id IN ($1, $2, ...)`.
    pub fn build_delete_outgoing_batch(self, tables: &OutboxTables, count: usize) -> String {
        let mut sql = format!("DELETE FROM {} WHERE id IN (", tables.outgoing());
        self.append_in_placeholders(&mut sql, count);
        sql.push(')');
        sql
    }

    /// Build `DELETE FROM toolkit_outbox_body WHERE id IN (...)`.
    pub fn build_delete_body_batch(self, tables: &OutboxTables, count: usize) -> String {
        let mut sql = format!("DELETE FROM {} WHERE id IN (", tables.body());
        self.append_in_placeholders(&mut sql, count);
        sql.push(')');
        sql
    }
}
