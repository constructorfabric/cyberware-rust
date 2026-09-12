//! `ExceptionQueueRepo` — the durable close-blocking exception queue
//! (`bss.ledger_exception_queue`), keyed by `(tenant_id, exception_id)`. The
//! per-slice exception stubs + the reconciliation framework `open` rows here;
//! the close gate reads OPEN rows for a period (Slice 7, design §4.6).

use sea_orm::sea_query::Expr;
use sea_orm::{ActiveValue::Set, ColumnTrait, Condition, EntityTrait};
use serde_json::Value as JsonValue;
use toolkit_db::odata::sea_orm_filter::{LimitCfg, paginate_odata};
use toolkit_db::secure::{AccessScope, DbTx, SecureEntityExt, SecureInsertExt, SecureUpdateExt};
use toolkit_db::{DBProvider, DbError};
use toolkit_odata::{ODataQuery, Page, SortDir};
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::model::RepoError;
use crate::infra::storage::entity::exception_queue;
use crate::infra::storage::odata_mapping::ExceptionODataMapper;
use crate::infra::storage::repo::journal_repo::{
    OdataPageError, map_odata_err, query_with_default_order,
};
use crate::odata::ExceptionFilterField;
use time::OffsetDateTime;

/// SeaORM-backed exception-queue repository.
#[derive(Clone)]
pub struct ExceptionQueueRepo {
    db: DBProvider<DbError>,
}

impl ExceptionQueueRepo {
    #[must_use]
    pub fn new(db: DBProvider<DbError>) -> Self {
        Self { db }
    }

    /// Open a new exception row (caller mints the synthetic `exception_id`).
    ///
    /// # Errors
    /// Returns [`RepoError::Db`] if scope validation or insertion fails.
    #[allow(
        clippy::too_many_arguments,
        reason = "an exception row carries its full identity at open time"
    )]
    pub async fn open(
        txn: &DbTx<'_>,
        scope: &AccessScope,
        tenant: Uuid,
        exception_id: Uuid,
        exception_type: &str,
        business_ref: &str,
        period_id: Option<&str>,
        detail: Option<JsonValue>,
    ) -> Result<(), RepoError> {
        let am = exception_queue::ActiveModel {
            tenant_id: Set(tenant),
            exception_id: Set(exception_id),
            exception_type: Set(exception_type.to_owned()),
            business_ref: Set(business_ref.to_owned()),
            status: Set("OPEN".to_owned()),
            period_id: Set(period_id.map(ToOwned::to_owned)),
            detail: Set(detail),
            opened_at: Set(OffsetDateTime::now_utc()),
            resolved_at: Set(None),
            resolved_by: Set(None),
        };
        exception_queue::Entity::insert(am.clone())
            .secure()
            .scope_with_model(scope, &am)
            .map_err(|e| RepoError::Db(format!("ledger_exception_queue scope: {e}")))?
            .exec_with_returning(txn)
            .await
            .map_err(|e| RepoError::Db(format!("insert ledger_exception_queue: {e}")))?;
        Ok(())
    }

    /// List OPEN close-blocking exceptions for a period (in-txn — the close gate
    /// input). `APPROVED_EXCEPTION` rows are not OPEN, so they are excluded.
    ///
    /// # Errors
    /// Returns [`RepoError::Db`] if the scoped query fails.
    pub async fn list_open_in_txn(
        txn: &DbTx<'_>,
        scope: &AccessScope,
        tenant: Uuid,
        period_id: &str,
    ) -> Result<Vec<exception_queue::Model>, RepoError> {
        let rows = exception_queue::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(
                Condition::all()
                    .add(exception_queue::Column::TenantId.eq(tenant))
                    .add(exception_queue::Column::PeriodId.eq(period_id))
                    .add(exception_queue::Column::Status.eq("OPEN")),
            )
            .all(txn)
            .await
            .map_err(|e| RepoError::Db(format!("list open ledger_exception_queue: {e}")))?;
        Ok(rows)
    }

    /// Whether an `OPEN` row already exists for `(tenant, type, business_ref)` — the
    /// routing dedup. A periodically-ticking stub (e.g. the aged-refund-clearing job)
    /// re-detects the same condition each scan, and an inline re-try repeats the same
    /// business key; without this they would pile up duplicate OPEN rows. In-txn so
    /// the check and the subsequent open share one snapshot.
    ///
    /// # Errors
    /// Returns [`RepoError::Db`] if the scoped existence query fails.
    pub async fn exists_open_for_ref(
        txn: &DbTx<'_>,
        scope: &AccessScope,
        tenant: Uuid,
        exception_type: &str,
        business_ref: &str,
    ) -> Result<bool, RepoError> {
        let row = exception_queue::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(
                Condition::all()
                    .add(exception_queue::Column::TenantId.eq(tenant))
                    .add(exception_queue::Column::ExceptionType.eq(exception_type))
                    .add(exception_queue::Column::BusinessRef.eq(business_ref))
                    .add(exception_queue::Column::Status.eq("OPEN")),
            )
            .one(txn)
            .await
            .map_err(|e| RepoError::Db(format!("exists open exception_queue: {e}")))?;
        Ok(row.is_some())
    }

    /// List exceptions for the dashboard (out-of-txn), tenant-scoped, optionally
    /// filtered by status.
    ///
    /// # Errors
    /// Returns [`DomainError::Internal`] if acquiring a connection or querying
    /// the exception queue fails.
    pub async fn list(
        &self,
        scope: &AccessScope,
        tenant: Uuid,
        status: Option<&str>,
    ) -> Result<Vec<exception_queue::Model>, DomainError> {
        let conn = self
            .db
            .conn()
            .map_err(|e| DomainError::Internal(format!("conn: {e}")))?;
        let mut predicate = Condition::all().add(exception_queue::Column::TenantId.eq(tenant));
        if let Some(s) = status {
            predicate = predicate.add(exception_queue::Column::Status.eq(s));
        }
        let rows = exception_queue::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(predicate)
            .order_by(exception_queue::Column::OpenedAt, sea_orm::Order::Desc)
            .all(&conn)
            .await
            .map_err(|e| DomainError::Internal(format!("list ledger_exception_queue: {e}")))?;
        Ok(rows)
    }

    /// List the exception-queue rows for `tenant` under `scope`, cursor-paginated
    /// via the canonical `query` (`$filter` over `type` / `status` / `business_ref`
    /// / `period_id`, `$orderby` / `limit` / `cursor`). The tenant predicate is
    /// pre-applied to the secured select; the user `$filter` is additive over it
    /// (SQL-level BOLA — a foreign value still ANDs the scope, so a cross-tenant
    /// exception never leaks). A bare list defaults to `exception_id ASC`. The
    /// `GET /exceptions` dashboard source; out-of-txn on a fresh scoped connection.
    /// Mirrors `AdjustmentRepo::list_refunds` / `DisputeRepo::list_disputes`.
    ///
    /// # Errors
    /// [`OdataPageError::Db`] on a storage / connection failure;
    /// [`OdataPageError::Odata`] on a malformed `$filter` / `$orderby` / cursor
    /// (the caller projects it to a canonical 400).
    pub async fn list_page(
        &self,
        scope: &AccessScope,
        tenant: Uuid,
        query: &ODataQuery,
    ) -> Result<Page<exception_queue::Model>, OdataPageError> {
        let conn = self
            .db
            .conn()
            .map_err(|e| OdataPageError::Db(format!("conn: {e}")))?;
        // Pre-apply the tenant predicate to the secured select; the user `$filter`
        // is applied additively by `paginate_odata` (it never replaces this scope —
        // BOLA preserved).
        let base_select = exception_queue::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(Condition::all().add(exception_queue::Column::TenantId.eq(tenant)));
        let query = query_with_default_order(query, "exception_id");
        paginate_odata::<
            ExceptionFilterField,
            ExceptionODataMapper,
            exception_queue::Entity,
            exception_queue::Model,
            _,
            _,
        >(
            base_select,
            &conn,
            &query,
            ("exception_id", SortDir::Asc),
            LimitCfg {
                default: 25,
                max: 200,
            },
            |m| m,
        )
        .await
        .map_err(map_odata_err)
    }

    /// Transition an exception to a terminal / ack status (in-txn).
    ///
    /// # Errors
    /// Returns [`RepoError::Db`] if the scoped update fails.
    pub async fn resolve(
        txn: &DbTx<'_>,
        scope: &AccessScope,
        tenant: Uuid,
        exception_id: Uuid,
        status: &str,
        resolved_by: &str,
    ) -> Result<(), RepoError> {
        exception_queue::Entity::update_many()
            .secure()
            .scope_with(scope)
            .col_expr(exception_queue::Column::Status, Expr::value(status))
            .col_expr(
                exception_queue::Column::ResolvedBy,
                Expr::value(resolved_by),
            )
            .col_expr(
                exception_queue::Column::ResolvedAt,
                Expr::value(OffsetDateTime::now_utc()),
            )
            .filter(
                Condition::all()
                    .add(exception_queue::Column::TenantId.eq(tenant))
                    .add(exception_queue::Column::ExceptionId.eq(exception_id)),
            )
            .exec(txn)
            .await
            .map_err(|e| RepoError::Db(format!("resolve ledger_exception_queue: {e}")))?;
        Ok(())
    }

    /// Read one exception by id (out-of-txn, tenant-scoped) — the resolution
    /// endpoint's pre-read (validate the transition against the row's type/status).
    /// A foreign-owned id resolves to `None` (SQL-level BOLA, no existence leak).
    ///
    /// # Errors
    /// Returns [`DomainError::Internal`] if acquiring a connection or querying
    /// the exception fails.
    pub async fn read(
        &self,
        scope: &AccessScope,
        tenant: Uuid,
        exception_id: Uuid,
    ) -> Result<Option<exception_queue::Model>, DomainError> {
        let conn = self
            .db
            .conn()
            .map_err(|e| DomainError::Internal(format!("conn: {e}")))?;
        let row = exception_queue::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(
                Condition::all()
                    .add(exception_queue::Column::TenantId.eq(tenant))
                    .add(exception_queue::Column::ExceptionId.eq(exception_id)),
            )
            .one(&conn)
            .await
            .map_err(|e| DomainError::Internal(format!("read ledger_exception_queue: {e}")))?;
        Ok(row)
    }

    /// Apply a resolution transition in its own transaction (the REST resolution
    /// endpoint's apply step). Thin wrapper over [`Self::resolve`] so the handler
    /// need not own a transaction.
    ///
    /// # Errors
    /// Returns [`DomainError::Internal`] if the transaction or scoped update
    /// fails.
    pub async fn resolve_one(
        &self,
        scope: &AccessScope,
        tenant: Uuid,
        exception_id: Uuid,
        status: &str,
        resolved_by: &str,
    ) -> Result<(), DomainError> {
        let scope = scope.clone();
        let status = status.to_owned();
        let resolved_by = resolved_by.to_owned();
        self.db
            .transaction(move |txn| {
                let scope = scope.clone();
                let status = status.clone();
                let resolved_by = resolved_by.clone();
                Box::pin(async move {
                    Self::resolve(txn, &scope, tenant, exception_id, &status, &resolved_by)
                        .await
                        .map_err(|e| DbError::Other(anyhow::anyhow!("resolve exception: {e}")))
                })
            })
            .await
            .map_err(|e| DomainError::Internal(format!("resolve exception txn: {e}")))
    }
}
