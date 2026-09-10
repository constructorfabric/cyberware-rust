use sea_orm::{ConnectionTrait, DatabaseExecutor, DbBackend, DbErr, Statement, TransactionTrait};

use super::dialect::{AllocSql, ClaimSql, Dialect, VacuumSql};
use super::statements::{MySqlIdReservationStatements, OutboxStatements};
use super::tables::OutboxTables;
use super::trace::TraceAdvance;

/// Runtime SQL context for one outbox table family on one database backend.
///
pub(super) struct OutboxStore<'a> {
    statements: &'a OutboxStatements,
}

impl<'a> OutboxStore<'a> {
    pub(super) fn new(statements: &'a OutboxStatements) -> Self {
        Self { statements }
    }

    pub(super) fn backend(&self) -> DbBackend {
        self.statements.backend()
    }

    pub(super) fn dialect(&self) -> Dialect {
        self.statements.dialect()
    }

    pub(super) fn tables(&self) -> &OutboxTables {
        self.statements.tables()
    }

    pub(super) fn register_queue_select(&self) -> &str {
        self.statements.registration().register_queue_select()
    }

    pub(super) fn register_queue_insert(&self) -> &str {
        self.statements.registration().register_queue_insert()
    }

    pub(super) async fn exec_insert_body_batch(
        &self,
        conn: &DatabaseExecutor<'_>,
        payloads: &[(&[u8], &str, Option<i64>)],
    ) -> Result<Vec<i64>, DbErr> {
        if payloads.is_empty() {
            return Ok(Vec::new());
        }
        if self.backend() == DbBackend::MySql {
            let reservation = self
                .statements
                .enqueue()
                .body_id_reservation()
                .ok_or_else(|| DbErr::Custom("missing MySQL body ID reservation SQL".to_owned()))?;
            let ids = self
                .reserve_mysql_ids(conn, reservation, payloads.len(), "body")
                .await?;
            let sql = self.build_insert_body_batch_with_ids(payloads.len());
            let mut values: Vec<sea_orm::Value> = Vec::with_capacity(payloads.len() * 4);
            for (id, &(payload, payload_type, trace_id)) in ids.iter().zip(payloads) {
                values.push((*id).into());
                values.push(payload.to_vec().into());
                values.push(payload_type.into());
                values.push(trace_id.into());
            }
            conn.execute_raw(Statement::from_sql_and_values(self.backend(), &sql, values))
                .await?;
            return Ok(ids);
        }

        let sql = self
            .dialect()
            .build_insert_body_batch(self.tables(), payloads.len());
        let mut values: Vec<sea_orm::Value> = Vec::with_capacity(payloads.len() * 3);
        for &(payload, payload_type, trace_id) in payloads {
            values.push(payload.to_vec().into());
            values.push(payload_type.into());
            values.push(trace_id.into());
        }

        if self.dialect().supports_returning() {
            let rows = conn
                .query_all_raw(Statement::from_sql_and_values(self.backend(), &sql, values))
                .await?;
            rows.iter()
                .map(|r| {
                    r.try_get_by_index(0)
                        .map_err(|e| DbErr::Custom(format!("body id column: {e}")))
                })
                .collect()
        } else {
            conn.execute_raw(Statement::from_sql_and_values(self.backend(), &sql, values))
                .await?;
            let row = conn
                .query_one_raw(Statement::from_string(
                    self.backend(),
                    Dialect::last_insert_id(),
                ))
                .await?
                .ok_or_else(|| {
                    DbErr::Custom("LAST_INSERT_ID() returned no row for body batch".to_owned())
                })?;
            let first_id: i64 = row
                .try_get_by_index(0)
                .map_err(|e| DbErr::Custom(format!("body first_id column: {e}")))?;
            #[allow(clippy::cast_possible_wrap)]
            Ok((0..payloads.len() as i64).map(|i| first_id + i).collect())
        }
    }

    pub(super) async fn exec_insert_incoming_batch(
        &self,
        conn: &DatabaseExecutor<'_>,
        entries: &[(i64, i64)],
    ) -> Result<Vec<i64>, DbErr> {
        if entries.is_empty() {
            return Ok(Vec::new());
        }
        if self.backend() == DbBackend::MySql {
            let reservation = self
                .statements
                .enqueue()
                .incoming_id_reservation()
                .ok_or_else(|| {
                    DbErr::Custom("missing MySQL incoming ID reservation SQL".to_owned())
                })?;
            let ids = self
                .reserve_mysql_ids(conn, reservation, entries.len(), "incoming")
                .await?;
            let sql = self.build_insert_incoming_batch_with_ids(entries.len());
            let mut values: Vec<sea_orm::Value> = Vec::with_capacity(entries.len() * 3);
            for (id, &(partition_id, body_id)) in ids.iter().zip(entries) {
                values.push((*id).into());
                values.push(partition_id.into());
                values.push(body_id.into());
            }
            conn.execute_raw(Statement::from_sql_and_values(self.backend(), &sql, values))
                .await?;
            return Ok(ids);
        }

        let sql = self
            .dialect()
            .build_insert_incoming_batch(self.tables(), entries.len());
        let mut values: Vec<sea_orm::Value> = Vec::with_capacity(entries.len() * 2);
        for &(partition_id, body_id) in entries {
            values.push(partition_id.into());
            values.push(body_id.into());
        }

        if self.dialect().supports_returning() {
            let rows = conn
                .query_all_raw(Statement::from_sql_and_values(self.backend(), &sql, values))
                .await?;
            rows.iter()
                .map(|r| {
                    r.try_get_by_index(0)
                        .map_err(|e| DbErr::Custom(format!("incoming id column: {e}")))
                })
                .collect()
        } else {
            conn.execute_raw(Statement::from_sql_and_values(self.backend(), &sql, values))
                .await?;
            let row = conn
                .query_one_raw(Statement::from_string(
                    self.backend(),
                    Dialect::last_insert_id(),
                ))
                .await?
                .ok_or_else(|| {
                    DbErr::Custom("LAST_INSERT_ID() returned no row for incoming batch".to_owned())
                })?;
            let first_id: i64 = row
                .try_get_by_index(0)
                .map_err(|e| DbErr::Custom(format!("incoming first_id column: {e}")))?;
            #[allow(clippy::cast_possible_wrap)]
            Ok((0..entries.len() as i64).map(|i| first_id + i).collect())
        }
    }

    /// Execute an INSERT and return the generated `id` column.
    ///
    /// Encapsulates RETURNING (Postgres/SQLite) vs `LAST_INSERT_ID` (`MySQL`).
    async fn exec_insert_returning_id(
        &self,
        conn: &DatabaseExecutor<'_>,
        sql: &str,
        params: Vec<sea_orm::Value>,
        context: &str,
    ) -> Result<i64, DbErr> {
        if self.dialect().supports_returning() {
            let row = conn
                .query_one_raw(Statement::from_sql_and_values(self.backend(), sql, params))
                .await?
                .ok_or_else(|| {
                    DbErr::Custom(format!("INSERT RETURNING returned no row for {context}"))
                })?;
            row.try_get_by_index(0)
                .map_err(|e| DbErr::Custom(format!("{context} id column: {e}")))
        } else {
            conn.execute_raw(Statement::from_sql_and_values(self.backend(), sql, params))
                .await?;
            let row = conn
                .query_one_raw(Statement::from_string(
                    self.backend(),
                    Dialect::last_insert_id(),
                ))
                .await?
                .ok_or_else(|| {
                    DbErr::Custom(format!("LAST_INSERT_ID() returned no row for {context}"))
                })?;
            row.try_get_by_index(0)
                .map_err(|e| DbErr::Custom(format!("{context} id column: {e}")))
        }
    }

    /// Execute a single body INSERT and return the generated ID.
    async fn exec_insert_body(
        &self,
        conn: &DatabaseExecutor<'_>,
        payload: Vec<u8>,
        payload_type: &str,
        trace_id: Option<i64>,
    ) -> Result<i64, DbErr> {
        if self.backend() == DbBackend::MySql {
            let payloads = [(payload.as_slice(), payload_type, trace_id)];
            let mut ids = self.exec_insert_body_batch(conn, &payloads).await?;
            return ids
                .pop()
                .ok_or_else(|| DbErr::Custom("body insert returned no id".to_owned()));
        }

        let sql = self.statements.enqueue().insert_body();
        self.exec_insert_returning_id(
            conn,
            sql,
            vec![payload.into(), payload_type.into(), trace_id.into()],
            "body",
        )
        .await
    }

    /// Execute a single incoming INSERT and return the generated ID.
    async fn exec_insert_incoming(
        &self,
        conn: &DatabaseExecutor<'_>,
        partition_id: i64,
        body_id: i64,
    ) -> Result<i64, DbErr> {
        if self.backend() == DbBackend::MySql {
            let entries = [(partition_id, body_id)];
            let mut ids = self.exec_insert_incoming_batch(conn, &entries).await?;
            return ids
                .pop()
                .ok_or_else(|| DbErr::Custom("incoming insert returned no id".to_owned()));
        }

        let sql = self.statements.enqueue().insert_incoming();
        self.exec_insert_returning_id(
            conn,
            sql,
            vec![partition_id.into(), body_id.into()],
            "incoming",
        )
        .await
    }

    pub(super) async fn exec_insert_body_and_incoming(
        &self,
        conn: &DatabaseExecutor<'_>,
        partition_id: i64,
        payload: Vec<u8>,
        payload_type: &str,
        trace_id: Option<i64>,
    ) -> Result<i64, DbErr> {
        if let Some(cte) = self.statements.enqueue().insert_body_and_incoming_cte() {
            self.exec_insert_returning_id(
                conn,
                cte,
                vec![
                    payload.into(),
                    payload_type.into(),
                    trace_id.into(),
                    partition_id.into(),
                ],
                "incoming",
            )
            .await
        } else {
            // MySQL: two separate round-trips (no CTE INSERT support).
            let body_id = self
                .exec_insert_body(conn, payload, payload_type, trace_id)
                .await?;
            self.exec_insert_incoming(conn, partition_id, body_id).await
        }
    }

    /// Insert one trace row and return its id.
    ///
    /// `pending` starts equal to `entities`; the ack counts it down and zero
    /// is completion.
    pub(super) async fn exec_insert_trace(
        &self,
        conn: &DatabaseExecutor<'_>,
        trace: &str,
        owner_instance: &str,
        queue: &str,
        entities: i64,
    ) -> Result<i64, DbErr> {
        let sql = self.statements.enqueue().insert_trace();
        self.exec_insert_returning_id(
            conn,
            sql,
            vec![
                trace.into(),
                owner_instance.into(),
                queue.into(),
                entities.into(),
                entities.into(),
            ],
            "trace",
        )
        .await
    }

    pub(super) fn trace_retry(&self) -> &str {
        self.statements.trace().retry()
    }

    pub(super) fn trace_status(&self) -> &str {
        self.statements.trace().status()
    }

    pub(super) fn select_collectable_traces(&self) -> &str {
        self.statements.sweep().select_collectable_traces()
    }

    pub(super) fn build_delete_traces(&self, count: usize) -> String {
        self.dialect().build_delete_traces(self.tables(), count)
    }

    pub(super) fn trace_mail(&self) -> &str {
        self.statements.trace().mail()
    }

    pub(super) fn trace_stalled(&self) -> &str {
        self.statements.trace().stalled()
    }

    /// Claim one completed trace as delivered, returning what to deliver, or
    /// `None` when the trace is unfinished, already delivered, or owned by
    /// another instance.
    /// Count a trace down, and say whether that advance completed it.
    ///
    /// `None` means the guard did not match - `pending` was already zero, so
    /// somebody else's ack completed it. On a dialect with `RETURNING` this
    /// costs the same one statement it always did and saves the caller the
    /// claim on every advance that did not reach zero, which for a batch
    /// spread over N partitions is N-1 guarded UPDATEs against the one row
    /// they all contend for. Elsewhere it cannot be known without another
    /// read, so the caller keeps claiming unconditionally.
    pub(super) async fn exec_trace_advance(
        &self,
        conn: &DatabaseExecutor<'_>,
        trace_id: i64,
        terminal: i64,
        failures: i64,
        instance_id: &str,
    ) -> Result<TraceAdvance, DbErr> {
        let sql = self.statements.trace().advance();

        if !self.dialect().supports_returning() {
            // The claim rides the countdown here, so this is one statement
            // rather than a countdown plus a guarded claim that can only
            // match for one ack of the batch.
            let affected = conn
                .execute_raw(Statement::from_sql_and_values(
                    self.backend(),
                    sql,
                    [
                        terminal.into(),
                        failures.into(),
                        terminal.into(),
                        terminal.into(),
                        terminal.into(),
                        instance_id.into(),
                        trace_id.into(),
                    ],
                ))
                .await?
                .rows_affected();
            return Ok(if affected == 0 {
                TraceAdvance::NotAffected
            } else {
                TraceAdvance::Unknown
            });
        }

        let values = [
            terminal.into(),
            failures.into(),
            terminal.into(),
            terminal.into(),
            trace_id.into(),
        ];

        let row = conn
            .query_one_raw(Statement::from_sql_and_values(self.backend(), sql, values))
            .await?;
        let Some(row) = row else {
            return Ok(TraceAdvance::NotAffected);
        };
        let pending: i64 = row
            .try_get_by_index(0)
            .map_err(|e| DbErr::Custom(format!("pending column: {e}")))?;
        Ok(if pending <= 0 {
            TraceAdvance::Completed
        } else {
            TraceAdvance::StillPending
        })
    }

    /// Claim mail with no transaction of the caller's own, atomically on every
    /// backend.
    ///
    /// The ack calls the primitive below inside its own transaction, so it is
    /// already atomic there. The collector has none, and on a backend without
    /// `RETURNING` the claim is an `UPDATE` followed by a `SELECT` - if the
    /// second fails, the row is stamped delivered with nobody told, no record
    /// and no retry, and the caller waits for ever. So that backend gets a
    /// transaction; the others are one statement and need none.
    pub(super) async fn claim_trace_mail_alone(
        &self,
        conn: &sea_orm::DatabaseConnection,
        trace_id: i64,
        instance_id: &str,
    ) -> Result<Option<super::trace::TraceOutcome>, DbErr> {
        if self.dialect().supports_returning() {
            let runner = crate::secure::SeaOrmRunner::Conn(conn);
            return self
                .exec_claim_trace_mail(&runner.executor(), trace_id, instance_id)
                .await;
        }

        let txn = conn.begin().await?;
        let claimed = self
            .exec_claim_trace_mail(&DatabaseExecutor::Transaction(&txn), trace_id, instance_id)
            .await?;
        txn.commit().await?;
        Ok(claimed)
    }

    pub(super) async fn exec_claim_trace_mail(
        &self,
        conn: &DatabaseExecutor<'_>,
        trace_id: i64,
        instance_id: &str,
    ) -> Result<Option<super::trace::TraceOutcome>, DbErr> {
        let claim = self.statements.trace().claim_mail();

        if self.dialect().supports_returning() {
            let row = conn
                .query_one_raw(Statement::from_sql_and_values(
                    self.backend(),
                    claim,
                    [trace_id.into(), instance_id.into()],
                ))
                .await?;
            return row.as_ref().map(Self::outcome_from_row).transpose();
        }

        // Nothing to claim: `exec_trace_advance` already stamped delivery in
        // the same statement that counted down. This read is how we learn
        // whether it did, and its guard is what makes that answer exact -
        // only the ack that drove `pending` to zero can see a row, because
        // every later advance is refused by `pending > 0`.
        let row = conn
            .query_one_raw(Statement::from_sql_and_values(
                self.backend(),
                self.statements.trace().claim_mail_outcome(),
                [trace_id.into(), instance_id.into()],
            ))
            .await?;
        row.as_ref().map(Self::outcome_from_row).transpose()
    }

    fn outcome_from_row(row: &sea_orm::QueryResult) -> Result<super::trace::TraceOutcome, DbErr> {
        Ok(super::trace::TraceOutcome {
            trace: row
                .try_get_by_index(0)
                .map_err(|e| DbErr::Custom(format!("trace column: {e}")))?,
            entities: row
                .try_get_by_index(1)
                .map_err(|e| DbErr::Custom(format!("entities column: {e}")))?,
            failures: row
                .try_get_by_index(2)
                .map_err(|e| DbErr::Custom(format!("failures column: {e}")))?,
            attempts: row
                .try_get_by_index(3)
                .map_err(|e| DbErr::Custom(format!("attempts column: {e}")))?,
            completed_at: row
                .try_get_by_index(4)
                .map_err(|e| DbErr::Custom(format!("completed_at column: {e}")))?,
        })
    }

    pub(super) fn lock_partition(&self) -> Option<&str> {
        self.statements.sequencer().lock_partition()
    }

    pub(super) fn discover_dirty_partitions(&self) -> &str {
        self.statements.sequencer().discover_dirty_partitions()
    }

    pub(super) fn insert_processor_row(&self) -> &str {
        self.statements.processor().insert_processor_row()
    }

    pub(super) fn lock_processor(&self) -> Option<&str> {
        self.statements.processor().lock_processor()
    }

    pub(super) fn read_outgoing_batch(&self, batch_size: u32) -> String {
        self.dialect()
            .read_outgoing_batch(self.tables(), batch_size)
    }

    pub(super) fn build_read_body_batch(&self, count: usize) -> String {
        self.dialect().build_read_body_batch(self.tables(), count)
    }

    pub(super) fn advance_processed_seq(&self) -> &str {
        self.statements.processor().advance_processed_seq()
    }

    pub(super) fn record_retry(&self) -> &str {
        self.statements.processor().record_retry()
    }

    pub(super) fn insert_dead_letter(&self) -> &str {
        self.statements.processor().insert_dead_letter()
    }

    pub(super) fn lease_ack_advance(&self) -> &str {
        self.statements.processor().lease_ack_advance()
    }

    pub(super) fn lease_record_retry(&self) -> &str {
        self.statements.processor().lease_record_retry()
    }

    pub(super) fn lease_release(&self) -> &str {
        self.statements.processor().lease_release()
    }

    pub(super) async fn exec_lease_acquire(
        &self,
        conn: &DatabaseExecutor<'_>,
        lease_id: &str,
        lease_secs: i64,
        partition_id: i64,
    ) -> Result<Option<(i64, i16)>, DbErr> {
        let lease_acquire = self.statements.processor().lease_acquire();
        if self.dialect().supports_returning() {
            let row = conn
                .query_one_raw(Statement::from_sql_and_values(
                    self.backend(),
                    lease_acquire,
                    [lease_id.into(), lease_secs.into(), partition_id.into()],
                ))
                .await?;
            match row {
                Some(r) => {
                    let processed_seq: i64 = r
                        .try_get_by_index(0)
                        .map_err(|e| DbErr::Custom(format!("processed_seq column: {e}")))?;
                    let attempts: i16 = r
                        .try_get_by_index(1)
                        .map_err(|e| DbErr::Custom(format!("attempts column: {e}")))?;
                    Ok(Some((processed_seq, attempts)))
                }
                None => Ok(None),
            }
        } else {
            let result = conn
                .execute_raw(Statement::from_sql_and_values(
                    self.backend(),
                    lease_acquire,
                    [lease_id.into(), lease_secs.into(), partition_id.into()],
                ))
                .await?;
            if result.rows_affected() == 0 {
                return Ok(None);
            }
            let read_processor = self.read_processor();
            let row = conn
                .query_one_raw(Statement::from_sql_and_values(
                    self.backend(),
                    read_processor,
                    [partition_id.into()],
                ))
                .await?;
            match row {
                Some(r) => {
                    let processed_seq: i64 = r
                        .try_get_by_index(0)
                        .map_err(|e| DbErr::Custom(format!("processed_seq column: {e}")))?;
                    let attempts: i16 = r
                        .try_get_by_index(1)
                        .map_err(|e| DbErr::Custom(format!("attempts column: {e}")))?;
                    Ok(Some((processed_seq, attempts)))
                }
                None => Ok(None),
            }
        }
    }

    pub(super) fn claim_incoming(&self, batch_size: u32) -> ClaimSql {
        self.dialect().claim_incoming(self.tables(), batch_size)
    }

    pub(super) fn delete_incoming_batch(&self, count: usize) -> String {
        self.dialect().delete_incoming_batch(self.tables(), count)
    }

    pub(super) fn allocate_sequences(&self) -> &AllocSql {
        self.statements.sequencer().allocate_sequences()
    }

    pub(super) fn build_insert_outgoing_batch(&self, count: usize) -> String {
        self.dialect()
            .build_insert_outgoing_batch(self.tables(), count)
    }

    pub(super) fn vacuum_cleanup(&self) -> &VacuumSql {
        self.statements.vacuum().cleanup()
    }

    pub(super) fn build_delete_outgoing_batch(&self, count: usize) -> String {
        self.dialect()
            .build_delete_outgoing_batch(self.tables(), count)
    }

    pub(super) fn build_delete_body_batch(&self, count: usize) -> String {
        self.dialect().build_delete_body_batch(self.tables(), count)
    }

    pub(super) fn read_processor(&self) -> &str {
        self.statements.processor().read_processor()
    }

    pub(super) fn bump_vacuum_counter(&self) -> &str {
        self.statements.vacuum().bump_counter()
    }

    pub(super) fn fetch_dirty_partitions(&self) -> &str {
        self.statements.vacuum().fetch_dirty_partitions()
    }

    pub(super) fn decrement_vacuum_counter(&self) -> &str {
        self.statements.vacuum().decrement_counter()
    }

    /// Only the `SQLite` integration suite resets the counter, so the gate
    /// matches that module's (`#[cfg(test)] #[cfg(feature = "sqlite")]`) —
    /// otherwise this is dead code in a pg- or mysql-only build.
    #[cfg(all(test, feature = "sqlite"))]
    pub(super) fn reset_vacuum_counter(&self) -> &str {
        self.statements.vacuum().reset_counter()
    }

    pub(super) fn insert_vacuum_counter_row(&self) -> &str {
        self.statements.registration().insert_vacuum_counter_row()
    }

    pub(super) fn dead_letter_select_columns(&self) -> &str {
        self.statements.dead_letters().select_columns()
    }

    pub(super) fn dead_letter_count_base(&self) -> &str {
        self.statements.dead_letters().count_base()
    }

    pub(super) fn dead_letter_id_select_base(&self) -> &str {
        self.statements.dead_letters().id_select_base()
    }

    async fn reserve_mysql_ids(
        &self,
        conn: &DatabaseExecutor<'_>,
        reservation: &MySqlIdReservationStatements,
        count: usize,
        context: &str,
    ) -> Result<Vec<i64>, DbErr> {
        let count = i64::try_from(count)
            .map_err(|e| DbErr::Custom(format!("{context} batch too large: {e}")))?;
        let row = conn
            .query_one_raw(Statement::from_string(
                self.backend(),
                reservation.select_next_id_for_update(),
            ))
            .await?
            .ok_or_else(|| {
                DbErr::Custom(format!(
                    "MySQL {context} ID sequence table returned no singleton row"
                ))
            })?;
        let first_id: i64 = row
            .try_get_by_index(0)
            .map_err(|e| DbErr::Custom(format!("{context} next_id column: {e}")))?;

        conn.execute_raw(Statement::from_sql_and_values(
            self.backend(),
            reservation.advance_next_id(),
            [count.into()],
        ))
        .await?;

        Ok((0..count).map(|offset| first_id + offset).collect())
    }

    fn build_insert_body_batch_with_ids(&self, count: usize) -> String {
        let mut sql = format!(
            "INSERT INTO {} (id, payload, payload_type, trace_id) VALUES ",
            self.tables().body()
        );
        append_mysql_value_tuples(&mut sql, count, 4);
        sql
    }

    fn build_insert_incoming_batch_with_ids(&self, count: usize) -> String {
        let mut sql = format!(
            "INSERT INTO {} (id, partition_id, body_id) VALUES ",
            self.tables().incoming()
        );
        append_mysql_value_tuples(&mut sql, count, 3);
        sql
    }
}

fn append_mysql_value_tuples(sql: &mut String, row_count: usize, cols: usize) {
    for row in 0..row_count {
        if row > 0 {
            sql.push_str(", ");
        }
        sql.push('(');
        for col in 0..cols {
            if col > 0 {
                sql.push_str(", ");
            }
            sql.push('?');
        }
        sql.push(')');
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use sea_orm::DbBackend;

    use super::*;
    use crate::outbox::tables::OutboxTables;

    fn mysql_store() -> OutboxStore<'static> {
        let tables = OutboxTables::default();
        let statements = OutboxStatements::new(DbBackend::MySql, &tables);
        let leaked = Box::leak(Box::new(statements));
        OutboxStore::new(leaked)
    }

    #[test]
    fn mysql_explicit_body_insert_includes_ids() {
        let store = mysql_store();
        let sql = store.build_insert_body_batch_with_ids(2);

        assert_eq!(
            sql,
            "INSERT INTO toolkit_outbox_body (id, payload, payload_type, trace_id) VALUES (?, ?, ?, ?), (?, ?, ?, ?)"
        );
    }

    #[test]
    fn mysql_explicit_incoming_insert_includes_ids() {
        let store = mysql_store();
        let sql = store.build_insert_incoming_batch_with_ids(2);

        assert_eq!(
            sql,
            "INSERT INTO toolkit_outbox_incoming (id, partition_id, body_id) VALUES (?, ?, ?), (?, ?, ?)"
        );
    }
}
