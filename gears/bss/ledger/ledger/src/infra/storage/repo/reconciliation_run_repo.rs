//! `ReconciliationRunRepo` — the reconciliation-run table
//! (`bss.ledger_reconciliation_run`), keyed by `(tenant_id, run_id)`. The
//! framework `start`s a RUNNING row then `finalize`s it with the variance;
//! an out-of-tolerance run feeds an `exception_queue` row + the close gate
//! (Slice 7, design §4.3).

use sea_orm::sea_query::Expr;
use sea_orm::{ActiveValue::Set, ColumnTrait, Condition, EntityTrait};
use serde_json::Value as JsonValue;
use toolkit_db::secure::{AccessScope, DbTx, SecureEntityExt, SecureInsertExt, SecureUpdateExt};
use toolkit_db::{DBProvider, DbError};
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::model::RepoError;
use crate::infra::storage::entity::reconciliation_run;
use time::OffsetDateTime;

/// SeaORM-backed reconciliation-run repository.
#[derive(Clone)]
pub struct ReconciliationRunRepo {
    db: DBProvider<DbError>,
}

impl ReconciliationRunRepo {
    #[must_use]
    pub fn new(db: DBProvider<DbError>) -> Self {
        Self { db }
    }

    /// Create a RUNNING reconciliation-run row.
    ///
    /// # Errors
    /// Returns [`RepoError::Db`] if scope validation or insertion fails.
    pub async fn start(
        txn: &DbTx<'_>,
        scope: &AccessScope,
        tenant: Uuid,
        run_id: Uuid,
        period_id: &str,
        check_type: &str,
    ) -> Result<(), RepoError> {
        let am = reconciliation_run::ActiveModel {
            tenant_id: Set(tenant),
            run_id: Set(run_id),
            period_id: Set(period_id.to_owned()),
            check_type: Set(check_type.to_owned()),
            variance_minor: Set(0),
            within_tolerance: Set(true),
            status: Set("RUNNING".to_owned()),
            watermark: Set(None),
            detail: Set(None),
            at_utc: Set(OffsetDateTime::now_utc()),
        };
        reconciliation_run::Entity::insert(am.clone())
            .secure()
            .scope_with_model(scope, &am)
            .map_err(|e| RepoError::Db(format!("ledger_reconciliation_run scope: {e}")))?
            .exec_with_returning(txn)
            .await
            .map_err(|e| RepoError::Db(format!("insert ledger_reconciliation_run: {e}")))?;
        Ok(())
    }

    /// Finalize a run with its variance result.
    ///
    /// # Errors
    /// Returns [`RepoError::Db`] if the scoped update fails.
    #[allow(
        clippy::too_many_arguments,
        reason = "a finalized run records its full variance result in one write"
    )]
    pub async fn finalize(
        txn: &DbTx<'_>,
        scope: &AccessScope,
        tenant: Uuid,
        run_id: Uuid,
        status: &str,
        variance_minor: i64,
        within_tolerance: bool,
        watermark: Option<i64>,
        detail: Option<JsonValue>,
    ) -> Result<(), RepoError> {
        reconciliation_run::Entity::update_many()
            .secure()
            .scope_with(scope)
            .col_expr(reconciliation_run::Column::Status, Expr::value(status))
            .col_expr(
                reconciliation_run::Column::VarianceMinor,
                Expr::value(variance_minor),
            )
            .col_expr(
                reconciliation_run::Column::WithinTolerance,
                Expr::value(within_tolerance),
            )
            .col_expr(
                reconciliation_run::Column::Watermark,
                Expr::value(watermark),
            )
            .col_expr(reconciliation_run::Column::Detail, Expr::value(detail))
            .filter(
                Condition::all()
                    .add(reconciliation_run::Column::TenantId.eq(tenant))
                    .add(reconciliation_run::Column::RunId.eq(run_id)),
            )
            .exec(txn)
            .await
            .map_err(|e| RepoError::Db(format!("finalize ledger_reconciliation_run: {e}")))?;
        Ok(())
    }

    /// Read a run (out-of-txn). SQL-level BOLA: a foreign tenant yields no row.
    ///
    /// # Errors
    /// Returns [`DomainError::Internal`] if acquiring a connection or reading
    /// the run fails.
    pub async fn read(
        &self,
        scope: &AccessScope,
        tenant: Uuid,
        run_id: Uuid,
    ) -> Result<Option<reconciliation_run::Model>, DomainError> {
        let conn = self
            .db
            .conn()
            .map_err(|e| DomainError::Internal(format!("conn: {e}")))?;
        let row = reconciliation_run::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(
                Condition::all()
                    .add(reconciliation_run::Column::TenantId.eq(tenant))
                    .add(reconciliation_run::Column::RunId.eq(run_id)),
            )
            .one(&conn)
            .await
            .map_err(|e| DomainError::Internal(format!("read ledger_reconciliation_run: {e}")))?;
        Ok(row)
    }
}
