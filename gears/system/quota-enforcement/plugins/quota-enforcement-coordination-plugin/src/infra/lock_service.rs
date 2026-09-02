//! [`DbCoordination`]: `CoordinationPluginV1` on the bound database. An
//! infrastructure adapter: it owns the SQL and the dialect clock expressions.
//!
//! Port of the account-management lease primitive. Acquisition runs in a
//! `SERIALIZABLE` retry transaction, so two workers cannot both observe a
//! free slot and both win: the loser sees a primary-key violation on the
//! insert path or zero affected rows on the steal path. Every time comparison
//! is a database-clock SQL expression. Clock drift between replicas can delay
//! an acquisition. It can never grant two live holders.

use std::time::Duration;

use async_trait::async_trait;
use quota_enforcement_sdk::{CoordinationError, CoordinationPluginV1, Lock, LockScope};
use sea_orm::sea_query::{Expr, SimpleExpr};
use sea_orm::{ActiveValue, ColumnTrait, EntityTrait, ExprTrait, QueryFilter};
use time::OffsetDateTime;
use toolkit_db::secure::{
    ScopeError, SecureEntityExt, SecureUpdateExt, TxConfig, is_unique_violation, secure_insert,
};
use toolkit_db::{Db, DbError};
use toolkit_security::AccessScope;
use uuid::Uuid;

use crate::infra::storage::entity::coordination_lock as locks;

const LOG_TARGET: &str = "qe.coordination";

/// Acquire-path outcome inside the retry transaction.
#[derive(Debug, thiserror::Error)]
enum AcquireError {
    /// A live holder owns the row.
    #[error("lock is held by another holder")]
    Held,
    /// Database failure. The retry helper classifies it.
    #[error(transparent)]
    Db(#[from] DbError),
}

impl AcquireError {
    fn db_err(&self) -> Option<&sea_orm::DbErr> {
        match self {
            Self::Db(DbError::Sea(e)) => Some(e),
            Self::Db(_) | Self::Held => None,
        }
    }
}

/// Database dialect for the clock expressions.
#[derive(Debug, Clone, Copy)]
enum Dialect {
    Postgres,
    Sqlite,
}

impl Dialect {
    fn now(self) -> SimpleExpr {
        match self {
            Self::Postgres => Expr::cust("NOW()"),
            Self::Sqlite => Expr::cust("datetime('now')"),
        }
    }

    fn now_plus_secs(self, secs: i64) -> SimpleExpr {
        match self {
            Self::Postgres => Expr::cust(format!("NOW() + INTERVAL '{secs} seconds'")),
            Self::Sqlite => Expr::cust(format!("datetime('now', '+{secs} seconds')")),
        }
    }

    fn epoch(self) -> SimpleExpr {
        match self {
            Self::Postgres => Expr::cust("TIMESTAMP 'epoch'"),
            Self::Sqlite => Expr::cust("'1970-01-01 00:00:00+00:00'"),
        }
    }
}

/// Database-backed [`CoordinationPluginV1`].
#[derive(Clone)]
pub struct DbCoordination {
    db: Db,
}

impl DbCoordination {
    /// Bind the service to the plugin's database.
    #[must_use]
    pub fn new(db: Db) -> Self {
        Self { db }
    }

    fn dialect(&self) -> Result<Dialect, CoordinationError> {
        match self.db.db_engine() {
            "postgres" => Ok(Dialect::Postgres),
            "sqlite" => Ok(Dialect::Sqlite),
            other => Err(CoordinationError::Internal(format!(
                "unsupported database engine for coordination locks: {other}"
            ))),
        }
    }
}

fn ttl_secs(ttl: Duration) -> i64 {
    i64::try_from(ttl.as_secs()).unwrap_or(i64::MAX)
}

fn map_scope_err(err: ScopeError) -> DbError {
    match err {
        ScopeError::Db(db) => DbError::Sea(db),
        other => DbError::Sea(sea_orm::DbErr::Custom(format!(
            "unexpected scope error on the unscoped lock table: {other:?}"
        ))),
    }
}

fn backend_unavailable(operation: &'static str, err: &DbError) -> CoordinationError {
    tracing::warn!(
        target: LOG_TARGET,
        operation,
        error = %err,
        "coordination backend call failed"
    );
    CoordinationError::BackendUnavailable(format!("database call failed during {operation}"))
}

// @cpt-dod:cpt-cf-quota-enforcement-dod-coordination-default:p1
// @cpt-algo:cpt-cf-quota-enforcement-algo-coordination-lock:p1
// @cpt-state:cpt-cf-quota-enforcement-state-coordination-lock:p1
#[async_trait]
impl CoordinationPluginV1 for DbCoordination {
    async fn try_lock(&self, scope: LockScope, ttl: Duration) -> Result<Lock, CoordinationError> {
        let dialect = self.dialect()?;
        let holder_id = Uuid::now_v7();
        let secs = ttl_secs(ttl);
        let key = scope.key();

        // @cpt-begin:cpt-cf-quota-enforcement-algo-coordination-lock:p1:inst-lock-try
        let outcome = self
            .db
            .transaction_with_retry::<(), AcquireError, _, _>(
                TxConfig::serializable(),
                AcquireError::db_err,
                move |tx| {
                    Box::pin(async move {
                        let existing = locks::Entity::find()
                            .filter(locks::Column::Key.eq(key))
                            .secure()
                            .scope_with(&AccessScope::allow_all())
                            .one(tx)
                            .await
                            .map_err(map_scope_err)?;

                        match existing {
                            None => {
                                // @cpt-begin:cpt-cf-quota-enforcement-state-coordination-lock:p1:inst-lockst-acquire
                                let row = locks::ActiveModel {
                                    key: ActiveValue::Set(key.to_owned()),
                                    holder_id: ActiveValue::Set(Some(holder_id)),
                                    locked_until: ActiveValue::Set(OffsetDateTime::now_utc() + ttl),
                                    attempts: ActiveValue::Set(1),
                                };
                                match secure_insert::<locks::Entity>(
                                    row,
                                    &AccessScope::allow_all(),
                                    tx,
                                )
                                .await
                                {
                                    Ok(_) => Ok(()),
                                    // A peer inserted between our SELECT and INSERT.
                                    Err(ScopeError::Db(db)) if is_unique_violation(&db) => {
                                        Err(AcquireError::Held)
                                    }
                                    Err(err) => Err(map_scope_err(err).into()),
                                }
                                // @cpt-end:cpt-cf-quota-enforcement-state-coordination-lock:p1:inst-lockst-acquire
                            }
                            Some(row) if row.locked_until <= OffsetDateTime::now_utc() => {
                                // Read side says expired or released. The UPDATE
                                // re-checks on the database clock, so drift can
                                // only make us lose, never steal a live hold.
                                // @cpt-begin:cpt-cf-quota-enforcement-algo-coordination-lock:p1:inst-lock-ttl
                                // @cpt-begin:cpt-cf-quota-enforcement-state-coordination-lock:p1:inst-lockst-reacquire
                                // @cpt-begin:cpt-cf-quota-enforcement-state-coordination-lock:p1:inst-lockst-handoff
                                let result = locks::Entity::update_many()
                                    .col_expr(locks::Column::HolderId, Expr::value(holder_id))
                                    .col_expr(
                                        locks::Column::LockedUntil,
                                        dialect.now_plus_secs(secs),
                                    )
                                    .col_expr(
                                        locks::Column::Attempts,
                                        Expr::col(locks::Column::Attempts).add(1),
                                    )
                                    .filter(locks::Column::Key.eq(key).and(
                                        Expr::col(locks::Column::LockedUntil).lt(dialect.now()),
                                    ))
                                    .secure()
                                    .scope_with(&AccessScope::allow_all())
                                    .exec(tx)
                                    .await
                                    .map_err(map_scope_err)?;
                                if result.rows_affected == 0 {
                                    return Err(AcquireError::Held);
                                }
                                Ok(())
                                // @cpt-end:cpt-cf-quota-enforcement-state-coordination-lock:p1:inst-lockst-handoff
                                // @cpt-end:cpt-cf-quota-enforcement-state-coordination-lock:p1:inst-lockst-reacquire
                                // @cpt-end:cpt-cf-quota-enforcement-algo-coordination-lock:p1:inst-lock-ttl
                            }
                            Some(_) => Err(AcquireError::Held),
                        }
                    })
                },
            )
            .await;
        // @cpt-end:cpt-cf-quota-enforcement-algo-coordination-lock:p1:inst-lock-try

        match outcome {
            // @cpt-begin:cpt-cf-quota-enforcement-algo-coordination-lock:p1:inst-lock-return
            Ok(()) => Ok(Lock::new(scope, holder_id, ttl, OffsetDateTime::now_utc())),
            // @cpt-end:cpt-cf-quota-enforcement-algo-coordination-lock:p1:inst-lock-return
            Err(AcquireError::Held) => Err(CoordinationError::Conflict { scope }),
            Err(AcquireError::Db(err)) => Err(backend_unavailable("try_lock", &err)),
        }
    }

    async fn renew(&self, lock: &Lock) -> Result<(), CoordinationError> {
        let dialect = self.dialect()?;
        let scope = lock.scope();
        let conn = self
            .db
            .conn()
            .map_err(|e| backend_unavailable("renew", &e))?;

        // @cpt-begin:cpt-cf-quota-enforcement-algo-coordination-lock:p1:inst-lock-renew
        // @cpt-begin:cpt-cf-quota-enforcement-state-coordination-lock:p1:inst-lockst-renew
        // The filter requires the row to be live on the database clock. An
        // expired row is logically lost, so renewal must not revive it.
        let result = locks::Entity::update_many()
            .col_expr(
                locks::Column::LockedUntil,
                dialect.now_plus_secs(ttl_secs(lock.ttl())),
            )
            .filter(
                locks::Column::Key
                    .eq(scope.key())
                    .and(locks::Column::HolderId.eq(lock.holder_id()))
                    .and(Expr::col(locks::Column::LockedUntil).gt(dialect.now())),
            )
            .secure()
            .scope_with(&AccessScope::allow_all())
            .exec(&conn)
            .await
            .map_err(|e| backend_unavailable("renew", &map_scope_err(e)))?;
        // @cpt-end:cpt-cf-quota-enforcement-state-coordination-lock:p1:inst-lockst-renew

        // @cpt-begin:cpt-cf-quota-enforcement-state-coordination-lock:p1:inst-lockst-expire
        if result.rows_affected == 0 {
            return Err(CoordinationError::LockExpired { scope });
        }
        // @cpt-end:cpt-cf-quota-enforcement-state-coordination-lock:p1:inst-lockst-expire
        Ok(())
        // @cpt-end:cpt-cf-quota-enforcement-algo-coordination-lock:p1:inst-lock-renew
    }

    async fn release(&self, lock: Lock) -> Result<(), CoordinationError> {
        let dialect = self.dialect()?;
        let scope = lock.scope();
        let conn = self
            .db
            .conn()
            .map_err(|e| backend_unavailable("release", &e))?;

        // @cpt-begin:cpt-cf-quota-enforcement-algo-coordination-lock:p1:inst-lock-release
        // @cpt-begin:cpt-cf-quota-enforcement-state-coordination-lock:p1:inst-lockst-release
        let result = locks::Entity::update_many()
            .col_expr(locks::Column::HolderId, Expr::value(None::<Uuid>))
            .col_expr(locks::Column::LockedUntil, dialect.epoch())
            .col_expr(locks::Column::Attempts, Expr::value(0_i32))
            .filter(
                locks::Column::Key
                    .eq(scope.key())
                    .and(locks::Column::HolderId.eq(lock.holder_id())),
            )
            .secure()
            .scope_with(&AccessScope::allow_all())
            .exec(&conn)
            .await
            .map_err(|e| backend_unavailable("release", &map_scope_err(e)))?;

        if result.rows_affected == 0 {
            // A peer already took the scope over. TTL expiry remains the
            // authoritative cleanup; the hint simply had nothing to do.
            tracing::debug!(
                target: LOG_TARGET,
                scope = %scope,
                holder_id = %lock.holder_id(),
                "release matched no row; the hold was already taken over"
            );
        }
        Ok(())
        // @cpt-end:cpt-cf-quota-enforcement-state-coordination-lock:p1:inst-lockst-release
        // @cpt-end:cpt-cf-quota-enforcement-algo-coordination-lock:p1:inst-lock-release
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "lock_service_tests.rs"]
mod lock_service_tests;
