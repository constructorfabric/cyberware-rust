use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseExecutor, DbErr, Statement};
use sea_orm_migration::prelude::*;

use super::tables::OutboxTables;
use super::types::OutboxError;

/// Single migration that creates the entire outbox schema from scratch.
///
/// Tables are created in FK dependency order:
/// body -> partitions -> incoming -> outgoing -> dead-letters -> processor
///
/// # Idempotency
///
/// The whole migration is safe to re-run, e.g. after a partial failure. All
/// `CREATE TABLE` statements use `IF NOT EXISTS`, and every index goes through
/// [`create_index_if_absent`], which is idempotent on all three backends --
/// `IF NOT EXISTS` on Postgres/SQLite and an `information_schema` probe on
/// `MySQL`, which has no such syntax for indexes.
struct CreateOutboxSchema {
    tables: OutboxTables,
}

impl CreateOutboxSchema {
    fn new(tables: OutboxTables) -> Self {
        Self { tables }
    }
}

impl Default for CreateOutboxSchema {
    fn default() -> Self {
        Self::new(OutboxTables::default())
    }
}

impl MigrationName for CreateOutboxSchema {
    fn name(&self) -> &str {
        self.tables.migration_name()
    }
}

#[async_trait::async_trait]
impl MigrationTrait for CreateOutboxSchema {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let conn = manager.get_connection();
        let backend = conn.get_database_backend();
        let tables = &self.tables;

        create_trace(conn, backend, tables).await?;
        create_body(conn, backend, tables).await?;
        create_partitions(conn, backend, tables).await?;
        create_incoming(conn, backend, tables).await?;
        create_outgoing(conn, backend, tables).await?;
        create_dead_letters(conn, backend, tables).await?;
        create_processor(conn, backend, tables).await?;
        create_vacuum_counter(conn, backend, tables).await?;
        create_mysql_id_sequence_tables(conn, backend, tables).await?;

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let conn = manager.get_connection();
        let backend = conn.get_database_backend();

        for table in [
            self.tables.incoming_id_sequence(),
            self.tables.body_id_sequence(),
            self.tables.vacuum_counter(),
            self.tables.processor(),
            self.tables.dead_letters(),
            self.tables.outgoing(),
            self.tables.incoming(),
            self.tables.partitions(),
            self.tables.body(),
            self.tables.trace(),
        ] {
            conn.execute_raw(Statement::from_string(
                backend,
                format!("DROP TABLE IF EXISTS {table}"),
            ))
            .await?;
        }
        Ok(())
    }
}

/// Error for a database backend the outbox schema has no DDL for.
///
/// `DatabaseBackend` is `#[non_exhaustive]` as of `SeaORM` 2.0, so every DDL
/// helper below needs a fallback arm. Returning an error (rather than picking a
/// dialect) keeps us from creating outbox tables with SQL meant for a different
/// engine.
///
/// `coverage(off)`: the enum has exactly three constructible variants, so the
/// nine arms that call this are unreachable without transmuting an invalid
/// discriminant. Excluded from the denominator rather than faked with a test.
#[cfg_attr(coverage_nightly, coverage(off))]
fn unsupported_backend(backend: DatabaseBackend) -> DbErr {
    DbErr::Custom(format!(
        "outbox migrations support Postgres, SQLite and MySQL only; \
         got unsupported database backend {backend:?}"
    ))
}

/// One index to create, in a backend-agnostic form.
///
/// `columns` and `filter` are already backend-appropriate: the call sites that
/// differ per engine (see [`create_dead_letters`]) pick them before building the
/// spec.
struct IndexSpec<'a> {
    /// Emit `CREATE UNIQUE INDEX` instead of `CREATE INDEX`.
    unique: bool,
    name: &'a str,
    table: &'a str,
    /// Column list exactly as it appears inside the parentheses.
    columns: &'a str,
    /// Partial-index predicate without the leading `WHERE`. Postgres only.
    filter: Option<&'a str>,
}

/// Looks up one index by name in the current `MySQL` schema.
///
/// Parameterised rather than interpolated: the two `?` placeholders are compared
/// as *values* against `information_schema` columns, not spliced in as identifiers.
const MYSQL_INDEX_PROBE_SQL: &str = "SELECT 1 FROM information_schema.STATISTICS \
     WHERE table_schema = DATABASE() AND table_name = ? AND index_name = ? \
     LIMIT 1";

/// How to create one index idempotently on one backend.
///
/// Separating the plan from its execution keeps every per-backend decision --
/// which engines support `IF NOT EXISTS`, whether a partial filter survives,
/// whether the probe binds parameters -- assertable without a live database.
#[derive(Debug, PartialEq, Eq)]
enum IndexPlan {
    /// Backend supports `CREATE INDEX IF NOT EXISTS`: one statement, no pre-check.
    Guarded { create: String },
    /// Backend has no such syntax: probe for the index, create it only if absent.
    ProbeThenCreate {
        probe_sql: &'static str,
        /// `[table, index_name]`, bound to the probe's placeholders in order.
        probe_args: [String; 2],
        create: String,
    },
}

/// Decide how to create `spec` on `backend`.
///
/// Postgres and `SQLite` get `CREATE INDEX IF NOT EXISTS`. `MySQL` has no such
/// syntax for indexes, so it is probed through `information_schema.STATISTICS`
/// first. The probe is preferred over tolerating error 1061 (`ER_DUP_KEYNAME`)
/// because it needs no error-code matching, and it also covers `MariaDB` — which
/// `DatabaseBackend::MySql` does not distinguish, and whose `IF NOT EXISTS`
/// support differs. The check-then-create race is irrelevant here: the migration
/// runner does not run two migrations against one schema at once.
fn plan_create_index(backend: DatabaseBackend, spec: &IndexSpec<'_>) -> Result<IndexPlan, DbErr> {
    let unique = if spec.unique { "UNIQUE " } else { "" };
    // Appended verbatim on every backend. Only the Postgres call sites pass a
    // filter; a backend that cannot express one must fail loudly rather than
    // silently create a non-partial index under the requested name.
    let filter = spec
        .filter
        .map_or_else(String::new, |f| format!(" WHERE {f}"));
    let tail = format!("{} ON {} ({}){filter}", spec.name, spec.table, spec.columns);

    match backend {
        DatabaseBackend::Postgres | DatabaseBackend::Sqlite => Ok(IndexPlan::Guarded {
            create: format!("CREATE {unique}INDEX IF NOT EXISTS {tail}"),
        }),
        DatabaseBackend::MySql => Ok(IndexPlan::ProbeThenCreate {
            probe_sql: MYSQL_INDEX_PROBE_SQL,
            probe_args: [spec.table.to_owned(), spec.name.to_owned()],
            create: format!("CREATE {unique}INDEX {tail}"),
        }),
        _ => Err(unsupported_backend(backend)),
    }
}

/// Create an index, skipping it if one of that name already exists.
///
/// Thin executor over [`plan_create_index`], which owns the per-backend logic.
async fn create_index_if_absent(
    conn: &DatabaseExecutor<'_>,
    backend: DatabaseBackend,
    spec: &IndexSpec<'_>,
) -> Result<(), DbErr> {
    match plan_create_index(backend, spec)? {
        IndexPlan::Guarded { create } => {
            conn.execute_raw(Statement::from_string(backend, create))
                .await?;
        }
        IndexPlan::ProbeThenCreate {
            probe_sql,
            probe_args: [table, index_name],
            create,
        } => {
            let existing = conn
                .query_one_raw(Statement::from_sql_and_values(
                    backend,
                    probe_sql,
                    [
                        sea_orm::Value::from(table),
                        sea_orm::Value::from(index_name),
                    ],
                ))
                .await?;
            if existing.is_none() {
                conn.execute_raw(Statement::from_string(backend, create))
                    .await?;
            }
        }
    }
    Ok(())
}

/// One row per traced batch: what it was, who is waiting for it, and how much
/// of it is still outstanding.
///
/// `pending` counts down from `entities` as the ack carries each entity to a
/// terminal state; reaching zero stamps `completed_at`. `owner_instance` is the
/// instance that enqueued the batch, and is the only instance a completion is
/// delivered to.
async fn create_trace(
    conn: &DatabaseExecutor<'_>,
    backend: DatabaseBackend,
    tables: &OutboxTables,
) -> Result<(), DbErr> {
    let trace = tables.trace();
    conn.execute_raw(Statement::from_string(
        backend,
        match backend {
            DatabaseBackend::Postgres => {
                format!(
                    "CREATE TABLE IF NOT EXISTS {trace} (
                id             BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
                trace          VARCHAR(256) NOT NULL,
                owner_instance VARCHAR(256) NOT NULL,
                queue          VARCHAR(1024) NOT NULL,
                entities       BIGINT NOT NULL,
                pending        BIGINT NOT NULL,
                failures       BIGINT NOT NULL DEFAULT 0,
                attempts       BIGINT NOT NULL DEFAULT 0,
                last_error     TEXT,
                stalled_since  TIMESTAMPTZ,
                created_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
                completed_at   TIMESTAMPTZ,
                notified_at    TIMESTAMPTZ
            )"
                )
            }
            DatabaseBackend::Sqlite => {
                format!(
                    "CREATE TABLE IF NOT EXISTS {trace} (
                id             INTEGER PRIMARY KEY AUTOINCREMENT,
                trace          TEXT    NOT NULL,
                owner_instance TEXT    NOT NULL,
                queue          TEXT    NOT NULL,
                entities       INTEGER NOT NULL,
                pending        INTEGER NOT NULL,
                failures       INTEGER NOT NULL DEFAULT 0,
                attempts       INTEGER NOT NULL DEFAULT 0,
                last_error     TEXT,
                stalled_since  TEXT,
                created_at     TEXT    NOT NULL DEFAULT (datetime('now')),
                completed_at   TEXT,
                notified_at    TEXT
            )"
                )
            }
            DatabaseBackend::MySql => {
                format!(
                    "CREATE TABLE IF NOT EXISTS {trace} (
                id             BIGINT AUTO_INCREMENT PRIMARY KEY,
                trace          VARCHAR(256) CHARACTER SET ascii NOT NULL,
                owner_instance VARCHAR(256) CHARACTER SET ascii NOT NULL,
                queue          VARCHAR(1024) CHARACTER SET ascii NOT NULL,
                entities       BIGINT NOT NULL,
                pending        BIGINT NOT NULL,
                failures       BIGINT NOT NULL DEFAULT 0,
                attempts       BIGINT NOT NULL DEFAULT 0,
                last_error     TEXT,
                stalled_since  TIMESTAMP(6) NULL,
                created_at     TIMESTAMP(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
                completed_at   TIMESTAMP(6) NULL,
                notified_at    TIMESTAMP(6) NULL
            )"
                )
            }
            _ => return Err(unsupported_backend(backend)),
        },
    ))
    .await?;

    // Undelivered mail for one instance. Partial on Postgres, so the index
    // holds only what is actually waiting to be collected - normally nothing,
    // which is what makes the notifier's poll an empty index probe.
    let mail_filter = matches!(backend, DatabaseBackend::Postgres)
        .then_some("completed_at IS NOT NULL AND notified_at IS NULL");
    create_index_if_absent(
        conn,
        backend,
        &IndexSpec {
            unique: false,
            name: tables.idx_trace_mail(),
            table: tables.trace(),
            columns: "owner_instance",
            filter: mail_filter,
        },
    )
    .await?;

    // Traces of this instance that are stuck rather than merely slow. Partial
    // on Postgres for the same reason as the mail index: a healthy instance
    // has none, so the stall reporter's poll touches an empty index.
    let stalled_filter = matches!(backend, DatabaseBackend::Postgres)
        .then_some("completed_at IS NULL AND stalled_since IS NOT NULL");
    create_index_if_absent(
        conn,
        backend,
        &IndexSpec {
            unique: false,
            name: tables.idx_trace_stalled(),
            table: tables.trace(),
            columns: "owner_instance",
            filter: stalled_filter,
        },
    )
    .await?;

    // Lookup by the caller's own trace, for reconciliation after a restart has
    // lost the in-memory subscription.
    create_index_if_absent(
        conn,
        backend,
        &IndexSpec {
            unique: false,
            name: tables.idx_trace_key(),
            table: tables.trace(),
            columns: "trace",
            filter: None,
        },
    )
    .await?;
    Ok(())
}

async fn create_body(
    conn: &DatabaseExecutor<'_>,
    backend: DatabaseBackend,
    tables: &OutboxTables,
) -> Result<(), DbErr> {
    let body_table = tables.body();
    conn.execute_raw(Statement::from_string(
        backend,
        match backend {
            DatabaseBackend::Postgres => {
                format!(
                    "CREATE TABLE IF NOT EXISTS {body_table} (
                id            BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
                payload       BYTEA  NOT NULL,
                payload_type  VARCHAR(1024) NOT NULL,
                created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
                trace_id      BIGINT NULL
            )"
                )
            }
            DatabaseBackend::Sqlite => {
                format!(
                    "CREATE TABLE IF NOT EXISTS {body_table} (
                id            INTEGER PRIMARY KEY AUTOINCREMENT,
                payload       BLOB   NOT NULL,
                payload_type  TEXT   NOT NULL,
                created_at    TEXT   NOT NULL DEFAULT (datetime('now')),
                trace_id      INTEGER NULL
            )"
                )
            }
            DatabaseBackend::MySql => {
                format!(
                    "CREATE TABLE IF NOT EXISTS {body_table} (
                id            BIGINT PRIMARY KEY,
                payload       LONGBLOB NOT NULL,
                payload_type  VARCHAR(1024) NOT NULL,
                created_at    TIMESTAMP(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
                trace_id      BIGINT NULL
            )"
                )
            }
            _ => return Err(unsupported_backend(backend)),
        },
    ))
    .await?;

    Ok(())
}

async fn create_partitions(
    conn: &DatabaseExecutor<'_>,
    backend: DatabaseBackend,
    tables: &OutboxTables,
) -> Result<(), DbErr> {
    let partitions = tables.partitions();
    conn.execute_raw(Statement::from_string(
        backend,
        match backend {
            DatabaseBackend::Postgres => {
                format!(
                    "CREATE TABLE IF NOT EXISTS {partitions} (
                id        BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
                queue     VARCHAR(1024) NOT NULL,
                partition SMALLINT NOT NULL,
                sequence  BIGINT   NOT NULL DEFAULT 0,
                UNIQUE (queue, partition)
            )"
                )
            }
            DatabaseBackend::Sqlite => {
                format!(
                    "CREATE TABLE IF NOT EXISTS {partitions} (
                id        INTEGER PRIMARY KEY AUTOINCREMENT,
                queue     TEXT    NOT NULL,
                partition INTEGER NOT NULL,
                sequence  INTEGER NOT NULL DEFAULT 0,
                UNIQUE (queue, partition)
            )"
                )
            }
            DatabaseBackend::MySql => {
                format!(
                    "CREATE TABLE IF NOT EXISTS {partitions} (
                id        BIGINT AUTO_INCREMENT PRIMARY KEY,
                queue     VARCHAR(1024) CHARACTER SET ascii NOT NULL,
                `partition` SMALLINT NOT NULL,
                sequence  BIGINT   NOT NULL DEFAULT 0,
                UNIQUE KEY (queue, `partition`)
            )"
                )
            }
            _ => return Err(unsupported_backend(backend)),
        },
    ))
    .await?;
    Ok(())
}

async fn create_incoming(
    conn: &DatabaseExecutor<'_>,
    backend: DatabaseBackend,
    tables: &OutboxTables,
) -> Result<(), DbErr> {
    let incoming = tables.incoming();
    let partitions = tables.partitions();
    let body_table = tables.body();
    conn.execute_raw(Statement::from_string(
        backend,
        match backend {
            DatabaseBackend::Postgres => {
                format!(
                    "CREATE TABLE IF NOT EXISTS {incoming} (
                id           BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
                partition_id BIGINT   NOT NULL REFERENCES {partitions}(id),
                body_id      BIGINT   NOT NULL REFERENCES {body_table}(id)
            )"
                )
            }
            DatabaseBackend::Sqlite => {
                format!(
                    "CREATE TABLE IF NOT EXISTS {incoming} (
                id           INTEGER PRIMARY KEY AUTOINCREMENT,
                partition_id INTEGER NOT NULL REFERENCES {partitions}(id),
                body_id      INTEGER NOT NULL REFERENCES {body_table}(id)
            )"
                )
            }
            DatabaseBackend::MySql => {
                format!(
                    "CREATE TABLE IF NOT EXISTS {incoming} (
                id           BIGINT PRIMARY KEY,
                partition_id BIGINT NOT NULL,
                body_id      BIGINT NOT NULL,
                FOREIGN KEY (partition_id) REFERENCES {partitions}(id),
                FOREIGN KEY (body_id) REFERENCES {body_table}(id)
            )"
                )
            }
            _ => return Err(unsupported_backend(backend)),
        },
    ))
    .await?;

    create_index_if_absent(
        conn,
        backend,
        &IndexSpec {
            unique: false,
            name: tables.idx_incoming_partition(),
            table: tables.incoming(),
            columns: "partition_id, id",
            filter: None,
        },
    )
    .await?;

    // Index on body_id to accelerate FK constraint checks during
    // DELETE FROM outbox body WHERE id IN (...).
    create_index_if_absent(
        conn,
        backend,
        &IndexSpec {
            unique: false,
            name: tables.idx_incoming_body_id(),
            table: tables.incoming(),
            columns: "body_id",
            filter: None,
        },
    )
    .await?;
    Ok(())
}

async fn create_outgoing(
    conn: &DatabaseExecutor<'_>,
    backend: DatabaseBackend,
    tables: &OutboxTables,
) -> Result<(), DbErr> {
    let outgoing = tables.outgoing();
    let partitions = tables.partitions();
    let body_table = tables.body();
    conn.execute_raw(Statement::from_string(
        backend,
        match backend {
            DatabaseBackend::Postgres => {
                format!(
                    "CREATE TABLE IF NOT EXISTS {outgoing} (
                id           BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
                partition_id BIGINT NOT NULL REFERENCES {partitions}(id),
                body_id      BIGINT NOT NULL REFERENCES {body_table}(id),
                seq          BIGINT NOT NULL,
                sequenced_at TIMESTAMPTZ NOT NULL DEFAULT now()
            )"
                )
            }
            DatabaseBackend::Sqlite => {
                format!(
                    "CREATE TABLE IF NOT EXISTS {outgoing} (
                id           INTEGER PRIMARY KEY AUTOINCREMENT,
                partition_id INTEGER NOT NULL REFERENCES {partitions}(id),
                body_id      INTEGER NOT NULL REFERENCES {body_table}(id),
                seq          INTEGER NOT NULL,
                sequenced_at TEXT    NOT NULL DEFAULT (datetime('now'))
            )"
                )
            }
            DatabaseBackend::MySql => {
                format!(
                    "CREATE TABLE IF NOT EXISTS {outgoing} (
                id           BIGINT AUTO_INCREMENT PRIMARY KEY,
                partition_id BIGINT NOT NULL,
                body_id      BIGINT NOT NULL,
                seq          BIGINT NOT NULL,
                sequenced_at TIMESTAMP(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
                FOREIGN KEY (partition_id) REFERENCES {partitions}(id),
                FOREIGN KEY (body_id) REFERENCES {body_table}(id)
            )"
                )
            }
            _ => return Err(unsupported_backend(backend)),
        },
    ))
    .await?;

    create_index_if_absent(
        conn,
        backend,
        &IndexSpec {
            unique: true,
            name: tables.idx_outgoing_partition_seq(),
            table: tables.outgoing(),
            columns: "partition_id, seq",
            filter: None,
        },
    )
    .await?;

    // Index on body_id to accelerate FK constraint checks during
    // DELETE FROM outbox body WHERE id IN (...).
    create_index_if_absent(
        conn,
        backend,
        &IndexSpec {
            unique: false,
            name: tables.idx_outgoing_body_id(),
            table: tables.outgoing(),
            columns: "body_id",
            filter: None,
        },
    )
    .await?;
    Ok(())
}

async fn create_dead_letters(
    conn: &DatabaseExecutor<'_>,
    backend: DatabaseBackend,
    tables: &OutboxTables,
) -> Result<(), DbErr> {
    let dead_letters = tables.dead_letters();
    let partitions = tables.partitions();
    conn.execute_raw(Statement::from_string(
        backend,
        match backend {
            DatabaseBackend::Postgres => {
                format!(
                    "CREATE TABLE IF NOT EXISTS {dead_letters} (
                id           BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
                partition_id BIGINT NOT NULL REFERENCES {partitions}(id),
                seq          BIGINT NOT NULL,
                payload      BYTEA  NOT NULL,
                payload_type VARCHAR(1024) NOT NULL,
                created_at   TIMESTAMPTZ NOT NULL,
                failed_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
                last_error   TEXT,
                attempts     SMALLINT NOT NULL,
                status       VARCHAR(32) NOT NULL DEFAULT 'pending',
                completed_at TIMESTAMPTZ,
                deadline     TIMESTAMPTZ,
                trace_id     BIGINT NULL
            )"
                )
            }
            DatabaseBackend::Sqlite => {
                format!(
                    "CREATE TABLE IF NOT EXISTS {dead_letters} (
                id           INTEGER PRIMARY KEY AUTOINCREMENT,
                partition_id INTEGER NOT NULL REFERENCES {partitions}(id),
                seq          INTEGER NOT NULL,
                payload      BLOB   NOT NULL,
                payload_type TEXT   NOT NULL,
                created_at   TEXT    NOT NULL,
                failed_at    TEXT    NOT NULL DEFAULT (datetime('now')),
                last_error   TEXT,
                attempts     INTEGER NOT NULL,
                status       TEXT    NOT NULL DEFAULT 'pending',
                completed_at TEXT,
                deadline     TEXT,
                trace_id     INTEGER NULL
            )"
                )
            }
            DatabaseBackend::MySql => {
                format!(
                    "CREATE TABLE IF NOT EXISTS {dead_letters} (
                id           BIGINT AUTO_INCREMENT PRIMARY KEY,
                partition_id BIGINT NOT NULL,
                seq          BIGINT NOT NULL,
                payload      LONGBLOB NOT NULL,
                payload_type VARCHAR(1024) CHARACTER SET ascii NOT NULL,
                created_at   TIMESTAMP(6) NOT NULL,
                failed_at    TIMESTAMP(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
                last_error   TEXT,
                attempts     SMALLINT NOT NULL,
                status       VARCHAR(32) NOT NULL DEFAULT 'pending',
                completed_at TIMESTAMP(6) NULL,
                deadline     TIMESTAMP(6) NULL,
                trace_id     BIGINT NULL,
                FOREIGN KEY (partition_id) REFERENCES {partitions}(id)
            )"
                )
            }
            _ => return Err(unsupported_backend(backend)),
        },
    ))
    .await?;

    // Index for replay query (status = 'pending' OR (status = 'reprocessing' AND deadline < now()))
    //
    // Postgres gets a *partial* index, under its own name. MySQL has no partial
    // indexes at all; SQLite has them (3.8.0+) but has never used one here, so
    // both get a plain index under a different name. Preserved as-is: switching
    // SQLite over would rename an index on existing databases.
    let replay_index = match backend {
        DatabaseBackend::Postgres => IndexSpec {
            unique: false,
            name: tables.idx_dl_replayable(),
            table: tables.dead_letters(),
            columns: "status, deadline",
            filter: Some("status IN ('pending', 'reprocessing')"),
        },
        DatabaseBackend::Sqlite | DatabaseBackend::MySql => IndexSpec {
            unique: false,
            name: tables.idx_dl_status_deadline(),
            table: tables.dead_letters(),
            columns: "status, deadline",
            filter: None,
        },
        _ => return Err(unsupported_backend(backend)),
    };
    create_index_if_absent(conn, backend, &replay_index).await?;

    // Index for list queries with status filter + ORDER BY failed_at DESC.
    // Only Postgres records the DESC direction.
    let status_failed_index = IndexSpec {
        unique: false,
        name: tables.idx_dl_status_failed(),
        table: tables.dead_letters(),
        columns: match backend {
            DatabaseBackend::Postgres => "status, failed_at DESC",
            DatabaseBackend::Sqlite | DatabaseBackend::MySql => "status, failed_at",
            _ => return Err(unsupported_backend(backend)),
        },
        filter: None,
    };
    create_index_if_absent(conn, backend, &status_failed_index).await?;

    Ok(())
}

async fn create_processor(
    conn: &DatabaseExecutor<'_>,
    backend: DatabaseBackend,
    tables: &OutboxTables,
) -> Result<(), DbErr> {
    let processor = tables.processor();
    let partitions = tables.partitions();
    conn.execute_raw(Statement::from_string(
        backend,
        match backend {
            DatabaseBackend::Postgres => {
                format!(
                    "CREATE TABLE IF NOT EXISTS {processor} (
                partition_id  BIGINT PRIMARY KEY REFERENCES {partitions}(id),
                processed_seq BIGINT   NOT NULL DEFAULT 0,
                attempts      SMALLINT NOT NULL DEFAULT 0,
                last_error    TEXT,
                locked_by     VARCHAR(1024),
                locked_until  TIMESTAMPTZ
            )"
                )
            }
            DatabaseBackend::Sqlite => {
                format!(
                    "CREATE TABLE IF NOT EXISTS {processor} (
                partition_id  INTEGER PRIMARY KEY REFERENCES {partitions}(id),
                processed_seq INTEGER NOT NULL DEFAULT 0,
                attempts      INTEGER NOT NULL DEFAULT 0,
                last_error    TEXT,
                locked_by     TEXT,
                locked_until  TEXT
            )"
                )
            }
            DatabaseBackend::MySql => {
                format!(
                    "CREATE TABLE IF NOT EXISTS {processor} (
                partition_id  BIGINT PRIMARY KEY,
                processed_seq BIGINT   NOT NULL DEFAULT 0,
                attempts      SMALLINT NOT NULL DEFAULT 0,
                last_error    TEXT,
                locked_by     VARCHAR(1024) CHARACTER SET ascii,
                locked_until  TIMESTAMP(6) NULL,
                FOREIGN KEY (partition_id) REFERENCES {partitions}(id)
            )"
                )
            }
            _ => return Err(unsupported_backend(backend)),
        },
    ))
    .await?;
    Ok(())
}

async fn create_vacuum_counter(
    conn: &DatabaseExecutor<'_>,
    backend: DatabaseBackend,
    tables: &OutboxTables,
) -> Result<(), DbErr> {
    let vacuum_counter = tables.vacuum_counter();
    let partitions = tables.partitions();
    conn.execute_raw(Statement::from_string(
        backend,
        match backend {
            DatabaseBackend::Postgres => {
                format!(
                    "CREATE TABLE IF NOT EXISTS {vacuum_counter} (
                partition_id BIGINT PRIMARY KEY
                    REFERENCES {partitions}(id),
                counter      BIGINT NOT NULL DEFAULT 0
            )"
                )
            }
            DatabaseBackend::Sqlite => {
                format!(
                    "CREATE TABLE IF NOT EXISTS {vacuum_counter} (
                partition_id INTEGER PRIMARY KEY
                    REFERENCES {partitions}(id),
                counter      INTEGER NOT NULL DEFAULT 0
            )"
                )
            }
            DatabaseBackend::MySql => {
                format!(
                    "CREATE TABLE IF NOT EXISTS {vacuum_counter} (
                partition_id BIGINT PRIMARY KEY,
                counter      BIGINT NOT NULL DEFAULT 0,
                FOREIGN KEY (partition_id) REFERENCES {partitions}(id)
            )"
                )
            }
            _ => return Err(unsupported_backend(backend)),
        },
    ))
    .await?;
    Ok(())
}

async fn create_mysql_id_sequence_tables(
    conn: &DatabaseExecutor<'_>,
    backend: DatabaseBackend,
    tables: &OutboxTables,
) -> Result<(), DbErr> {
    if backend != DatabaseBackend::MySql {
        return Ok(());
    }

    for table in [tables.body_id_sequence(), tables.incoming_id_sequence()] {
        conn.execute_raw(Statement::from_string(
            backend,
            mysql_create_id_sequence_sql(table),
        ))
        .await?;
        conn.execute_raw(Statement::from_string(
            backend,
            mysql_seed_id_sequence_sql(table),
        ))
        .await?;
    }

    Ok(())
}

fn mysql_create_id_sequence_sql(table: &str) -> String {
    format!(
        "CREATE TABLE IF NOT EXISTS {table} (
                slot    TINYINT PRIMARY KEY,
                next_id BIGINT NOT NULL
            )"
    )
}

fn mysql_seed_id_sequence_sql(table: &str) -> String {
    format!("INSERT IGNORE INTO {table} (slot, next_id) VALUES (1, 1)")
}

/// Returns all outbox migrations in dependency order.
#[must_use]
pub fn outbox_migrations() -> Vec<Box<dyn MigrationTrait>> {
    vec![Box::new(CreateOutboxSchema::default())]
}

/// Returns all outbox migrations for a custom table prefix in dependency order.
///
/// # Errors
///
/// Returns [`OutboxError::InvalidTablePrefix`] if the prefix cannot be used as
/// a portable SQL identifier.
pub fn outbox_migrations_with_prefix(
    prefix: impl Into<String>,
) -> Result<Vec<Box<dyn MigrationTrait>>, OutboxError> {
    let tables = OutboxTables::new(prefix)?;
    Ok(vec![Box::new(CreateOutboxSchema::new(tables))])
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::outbox::tables::DEFAULT_OUTBOX_MIGRATION_NAME;

    #[test]
    fn default_migration_name_is_preserved() {
        let migrations = outbox_migrations();

        assert_eq!(migrations[0].name(), DEFAULT_OUTBOX_MIGRATION_NAME);
    }

    #[test]
    fn prefixed_migration_name_is_deterministic_and_distinct() {
        let first = outbox_migrations_with_prefix("mini_chat_outbox").unwrap();
        let second = outbox_migrations_with_prefix("mini_chat_outbox").unwrap();

        assert_eq!(first[0].name(), second[0].name());
        assert_ne!(first[0].name(), DEFAULT_OUTBOX_MIGRATION_NAME);
        assert_eq!(
            first[0].name(),
            "m001_create_toolkit_outbox_schema__mini_chat_outbox"
        );
    }

    #[test]
    fn invalid_prefix_is_rejected() {
        let Err(err) = outbox_migrations_with_prefix("public.outbox") else {
            panic!("expected invalid prefix error")
        };

        assert!(
            matches!(
                err,
                OutboxError::InvalidTablePrefix { reason }
                    if reason == "must contain only ASCII alphanumerics and '_'"
            ),
            "the rejection states the rule, not the prefix"
        );
    }

    #[test]
    fn default_and_prefixed_migration_names_do_not_collide() {
        let default = outbox_migrations();
        let prefixed = outbox_migrations_with_prefix("mini_chat_outbox").unwrap();

        assert_ne!(default[0].name(), prefixed[0].name());
    }

    #[test]
    fn mysql_sequence_table_sql_uses_custom_prefix() {
        let tables = OutboxTables::new("mini_chat_outbox").unwrap();

        let body_create = mysql_create_id_sequence_sql(tables.body_id_sequence());
        let incoming_create = mysql_create_id_sequence_sql(tables.incoming_id_sequence());
        let body_seed = mysql_seed_id_sequence_sql(tables.body_id_sequence());
        let incoming_seed = mysql_seed_id_sequence_sql(tables.incoming_id_sequence());

        assert!(body_create.contains("mini_chat_outbox_body_id_sequence"));
        assert!(incoming_create.contains("mini_chat_outbox_incoming_id_sequence"));
        assert!(body_seed.contains("mini_chat_outbox_body_id_sequence"));
        assert!(incoming_seed.contains("mini_chat_outbox_incoming_id_sequence"));
        assert!(!body_create.contains("toolkit_outbox_body_id_sequence"));
        assert!(!incoming_create.contains("toolkit_outbox_incoming_id_sequence"));
    }

    /// Every backend the outbox supports DDL for.
    const BACKENDS: [DatabaseBackend; 3] = [
        DatabaseBackend::Postgres,
        DatabaseBackend::MySql,
        DatabaseBackend::Sqlite,
    ];

    fn plan(backend: DatabaseBackend, spec: &IndexSpec<'_>) -> IndexPlan {
        plan_create_index(backend, spec).unwrap()
    }

    /// The `create` statement of a plan, whichever variant it is.
    fn create_sql(plan: &IndexPlan) -> &str {
        match plan {
            IndexPlan::Guarded { create } | IndexPlan::ProbeThenCreate { create, .. } => create,
        }
    }

    fn simple_spec<'a>(name: &'a str, table: &'a str) -> IndexSpec<'a> {
        IndexSpec {
            unique: false,
            name,
            table,
            columns: "status, deadline",
            filter: None,
        }
    }

    #[test]
    fn plan_create_index_is_idempotent_on_every_backend() {
        // The point of the helper: re-running the migration must not fail on a
        // pre-existing index, on any backend. Postgres/SQLite express that with
        // IF NOT EXISTS; MySQL has no such syntax and must probe instead.
        for backend in BACKENDS {
            let spec = simple_spec("idx_x", "tbl");
            match plan(backend, &spec) {
                IndexPlan::Guarded { create } => {
                    assert!(
                        matches!(backend, DatabaseBackend::Postgres | DatabaseBackend::Sqlite),
                        "{backend:?} should not use the guarded path"
                    );
                    assert!(
                        create.contains("IF NOT EXISTS"),
                        "{backend:?} guarded DDL must be conditional, got: {create}"
                    );
                }
                IndexPlan::ProbeThenCreate {
                    probe_sql,
                    probe_args,
                    create,
                } => {
                    assert_eq!(
                        backend,
                        DatabaseBackend::MySql,
                        "only MySQL lacks CREATE INDEX IF NOT EXISTS"
                    );
                    // MySQL would reject IF NOT EXISTS on an index, so the
                    // guard must come from the probe, not the DDL.
                    assert!(
                        !create.contains("IF NOT EXISTS"),
                        "MySQL DDL must not use IF NOT EXISTS, got: {create}"
                    );
                    assert!(probe_sql.contains("information_schema.STATISTICS"));
                    assert_eq!(probe_args, ["tbl".to_owned(), "idx_x".to_owned()]);
                }
            }
        }
    }

    #[test]
    fn mysql_index_probe_binds_identifiers_instead_of_interpolating_them() {
        // A regression here would splice table/index names into the probe SQL.
        // They are compared as values against information_schema, so they must
        // stay bound to the two placeholders.
        let spec = simple_spec("idx_dl_status_deadline", "toolkit_outbox_dead_letters");
        let IndexPlan::ProbeThenCreate {
            probe_sql,
            probe_args,
            ..
        } = plan(DatabaseBackend::MySql, &spec)
        else {
            panic!("MySQL must use the probe path")
        };

        assert_eq!(probe_sql.matches('?').count(), 2, "probe: {probe_sql}");
        assert!(
            !probe_sql.contains("toolkit_outbox_dead_letters")
                && !probe_sql.contains("idx_dl_status_deadline"),
            "names must not appear in the SQL text, got: {probe_sql}"
        );
        assert_eq!(
            probe_args,
            [
                "toolkit_outbox_dead_letters".to_owned(),
                "idx_dl_status_deadline".to_owned()
            ],
            "table then index name, matching placeholder order"
        );
    }

    #[test]
    fn plan_create_index_preserves_partial_index_filter() {
        // The Postgres replay index is partial. Routing it through the shared
        // helper must not drop the predicate -- that would silently widen the
        // index and change the replay query's access path.
        let spec = IndexSpec {
            unique: false,
            name: "idx_dl_replayable",
            table: "toolkit_outbox_dead_letters",
            columns: "status, deadline",
            filter: Some("status IN ('pending', 'reprocessing')"),
        };

        assert_eq!(
            create_sql(&plan(DatabaseBackend::Postgres, &spec)),
            "CREATE INDEX IF NOT EXISTS idx_dl_replayable ON toolkit_outbox_dead_letters \
             (status, deadline) WHERE status IN ('pending', 'reprocessing')"
        );
    }

    #[test]
    fn plan_create_index_omits_where_clause_when_unfiltered() {
        // Negative space: an unfiltered spec must not grow a WHERE clause.
        for backend in BACKENDS {
            let spec = simple_spec("idx_x", "tbl");
            let plan = plan(backend, &spec);
            assert!(
                !create_sql(&plan).contains("WHERE"),
                "{backend:?} emitted a WHERE for an unfiltered index: {}",
                create_sql(&plan)
            );
        }
    }

    #[test]
    fn plan_create_index_keeps_unique_on_every_backend() {
        // The outgoing (partition_id, seq) index is UNIQUE and that uniqueness
        // is a correctness invariant, not an optimisation.
        for backend in BACKENDS {
            let spec = IndexSpec {
                unique: true,
                name: "idx_outgoing_partition_seq",
                table: "toolkit_outbox_outgoing",
                columns: "partition_id, seq",
                filter: None,
            };
            let plan = plan(backend, &spec);
            assert!(
                create_sql(&plan).starts_with("CREATE UNIQUE INDEX "),
                "{backend:?} dropped UNIQUE: {}",
                create_sql(&plan)
            );
        }
    }

    #[test]
    fn mysql_sequence_table_sql_has_singleton_primary_key_and_seed() {
        let tables = OutboxTables::default();

        let create = mysql_create_id_sequence_sql(tables.body_id_sequence());
        let seed = mysql_seed_id_sequence_sql(tables.body_id_sequence());

        assert!(create.contains("slot    TINYINT PRIMARY KEY"));
        assert!(create.contains("next_id BIGINT NOT NULL"));
        assert_eq!(
            seed,
            "INSERT IGNORE INTO toolkit_outbox_body_id_sequence (slot, next_id) VALUES (1, 1)"
        );
    }

    /// Live-database checks. The unit tests above pin the *emitted SQL*; these
    /// pin what the database actually ends up with.
    #[cfg(feature = "sqlite")]
    mod sqlite_tests {
        use super::*;
        use crate::{ConnectOpts, Db, connect_db};
        use sea_orm::DbBackend;

        /// Single pooled connection over a shared-cache in-memory database, so
        /// two sequential `up()` calls hit the *same* schema. With a larger pool
        /// the second call could land on a fresh empty database and the test
        /// would pass without proving anything.
        async fn setup_shared_db(name: &str) -> Db {
            let url = format!("sqlite:file:{name}?mode=memory&cache=shared");
            let opts = ConnectOpts {
                max_conns: Some(1),
                min_conns: Some(1),
                ..Default::default()
            };
            connect_db(&url, opts).await.expect("connect")
        }

        async fn outbox_index_names(conn: &sea_orm::DatabaseConnection) -> Vec<String> {
            let rows = conn
                .query_all_raw(Statement::from_string(
                    DbBackend::Sqlite,
                    "SELECT name FROM sqlite_master \
                     WHERE type = 'index' AND name LIKE 'idx_toolkit_outbox%' \
                     ORDER BY name",
                ))
                .await
                .expect("list indexes");
            rows.iter()
                .map(|row| row.try_get::<String>("", "name").expect("index name"))
                .collect()
        }

        async fn columns_of(conn: &sea_orm::DatabaseConnection, table: &str) -> Vec<String> {
            let rows = conn
                .query_all_raw(Statement::from_string(
                    DbBackend::Sqlite,
                    format!("SELECT name FROM pragma_table_info('{table}') ORDER BY name"),
                ))
                .await
                .expect("list columns");
            rows.iter()
                .map(|row| row.try_get::<String>("", "name").expect("column name"))
                .collect()
        }

        #[tokio::test]
        async fn schema_carries_the_trace_table_and_its_identity_columns() {
            let db = setup_shared_db("outbox_schema_shape").await;
            let conn = db.sea_internal();
            let manager = SchemaManager::new(&conn);
            CreateOutboxSchema::default()
                .up(&manager)
                .await
                .expect("up");
            let tables = OutboxTables::default();

            // Trace identity reaches both the body and its dead letter.
            for table in [tables.body(), tables.dead_letters()] {
                assert!(
                    columns_of(&conn, table)
                        .await
                        .iter()
                        .any(|c| c == "trace_id"),
                    "{table} is missing trace_id"
                );
            }

            let trace_columns = columns_of(&conn, tables.trace()).await;
            assert!(!trace_columns.is_empty(), "the trace table was not created");
            for column in [
                "attempts",
                "completed_at",
                "created_at",
                "entities",
                "failures",
                "id",
                "last_error",
                "notified_at",
                "owner_instance",
                "pending",
                "queue",
                "stalled_since",
                "trace",
            ] {
                assert!(
                    trace_columns.iter().any(|c| c == column),
                    "the trace table is missing {column}"
                );
            }

            let indexes = outbox_index_names(&conn).await;
            for index in [tables.idx_trace_mail(), tables.idx_trace_key()] {
                assert!(
                    indexes.iter().any(|i| i == index),
                    "missing index {index}: have {indexes:?}"
                );
            }
        }

        #[tokio::test]
        async fn migration_up_is_rerunnable_over_a_fully_created_schema() {
            // This is the scenario the schema DDL has to survive: the runner
            // applied the migration, crashed before recording it, and retries.
            // Before `create_index_if_absent`, the second run died on the first
            // pre-existing index.
            let db = setup_shared_db("outbox_migration_rerun").await;
            let conn = db.sea_internal();
            let manager = SchemaManager::new(&conn);
            let migration = CreateOutboxSchema::default();

            migration.up(&manager).await.expect("first up");
            let after_first = outbox_index_names(&conn).await;

            migration
                .up(&manager)
                .await
                .expect("second up must succeed over an existing schema");
            let after_second = outbox_index_names(&conn).await;

            // Non-empty guards against a vacuous pass: if the first run had
            // created nothing, re-running would trivially succeed.
            assert!(
                !after_first.is_empty(),
                "first up() created no outbox indexes"
            );
            assert_eq!(
                after_first, after_second,
                "re-running the migration changed the index set"
            );
        }

        #[tokio::test]
        async fn migration_up_completes_after_indexes_were_created_but_tables_were_not_recorded() {
            // Narrower version of the above: drop one index between runs and
            // confirm the retry re-creates exactly that one rather than either
            // failing on the survivors or leaving the dropped one missing.
            let db = setup_shared_db("outbox_migration_partial").await;
            let conn = db.sea_internal();
            let manager = SchemaManager::new(&conn);
            let migration = CreateOutboxSchema::default();

            migration.up(&manager).await.expect("first up");
            let complete = outbox_index_names(&conn).await;

            let tables = OutboxTables::default();
            conn.execute_raw(Statement::from_string(
                DbBackend::Sqlite,
                format!("DROP INDEX {}", tables.idx_incoming_body_id()),
            ))
            .await
            .expect("drop one index");
            assert_ne!(
                outbox_index_names(&conn).await,
                complete,
                "the DROP INDEX did not take effect"
            );

            migration
                .up(&manager)
                .await
                .expect("retry after partial loss");

            assert_eq!(
                outbox_index_names(&conn).await,
                complete,
                "retry did not restore the dropped index"
            );
        }
    }
}
