//! Pure helpers shared across the `SeaORM` repo split.
//!
//! Visibility: `pub(super)` — these helpers are private to the
//! `repo_impl` gear tree (siblings: `reads`, `lifecycle`,
//! `updates`, `retention`).

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use sea_orm::{ColumnTrait, Condition, DbErr};
use toolkit_db::DbError;
use toolkit_db::contention::is_retryable_contention;
use toolkit_db::secure::{DbTx, ScopeError, TxConfig};
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::metrics::{AM_SERIALIZABLE_RETRY, MetricKind, emit_metric};
use crate::domain::tenant::model::{TenantModel, TenantStatus};
use crate::infra::canonical_mapping::classify_db_err_to_domain;
use crate::infra::storage::entity::tenants;

use super::AmDbProvider;

/// Infra-internal error type used inside transactional bodies.
///
/// Domain code returns [`DomainError`] (pure, `#[domain_model]`-validated,
/// no `sea_orm` references). Transactional bodies need to carry a raw
/// [`DbErr`] until the retry helper has had its chance to inspect it for
/// retryable contention; that's what `TxError::Db` is for. Typed domain
/// failures (e.g. `Conflict`, `NotFound`) live in `TxError::Domain` so
/// they pass through retry untouched.
///
/// At retry exit:
/// * `TxError::Db(db_err)` is classified by [`classify_db_err_to_domain`].
/// * `TxError::Domain(d)` is returned verbatim.
#[derive(Debug)]
pub enum TxError {
    /// Raw `DbErr` carried through retry. After retry exhaustion the
    /// helper translates it to a typed [`DomainError`] via
    /// [`classify_db_err_to_domain`].
    Db(DbErr),
    /// Pre-classified domain error — already typed, will pass through
    /// retry without further inspection.
    Domain(DomainError),
}

impl TxError {
    /// Accessor used by [`toolkit_db::Db::transaction_with_retry_max`]
    /// to feed the wrapped `DbErr` into
    /// [`toolkit_db::contention::is_retryable_contention`] for the
    /// retry decision.
    pub(crate) fn db_err(&self) -> Option<&DbErr> {
        match self {
            Self::Db(e) => Some(e),
            Self::Domain(_) => None,
        }
    }
}

impl From<DomainError> for TxError {
    fn from(err: DomainError) -> Self {
        Self::Domain(err)
    }
}

impl From<DbError> for TxError {
    fn from(err: DbError) -> Self {
        match err {
            DbError::Sea(db) => Self::Db(db),
            other => Self::Domain(DomainError::from(other)),
        }
    }
}

/// Map a [`ScopeError`] surfaced from the secure-extension layer into a
/// transactional [`TxError`]. `ScopeError::Db(_)` carries the raw
/// `DbErr` through retry; the remaining variants are typed domain
/// failures by construction.
pub fn map_scope_to_tx(err: ScopeError) -> TxError {
    match err {
        ScopeError::Db(db) => TxError::Db(db),
        ScopeError::Invalid(msg) => TxError::Domain(DomainError::Internal {
            diagnostic: format!("scope invalid: {msg}"),
            cause: None,
        }),
        ScopeError::TenantNotInScope { .. } => {
            TxError::Domain(DomainError::CrossTenantDenied { cause: None })
        }
        ScopeError::Denied(msg) => TxError::Domain(DomainError::Internal {
            diagnostic: format!("unexpected access denied in AM repo: {msg}"),
            cause: None,
        }),
        // `ScopeError` is `#[non_exhaustive]`: variants this gear has no
        // specific answer for (today the graph-query refusals, which it can
        // never trigger) map to an internal error, like `Invalid`.
        other => TxError::Domain(DomainError::Internal {
            diagnostic: format!("scope invalid: {other}"),
            cause: None,
        }),
    }
}

/// Map a [`ScopeError`] surfaced outside a retry boundary into a typed
/// [`DomainError`]. Used by non-transactional code paths and by
/// `transaction_with_config` bodies that don't need retry — eager
/// classification is appropriate here because there is no retry helper
/// to consult the raw `DbErr`.
pub(super) fn map_scope_err(err: ScopeError) -> DomainError {
    match map_scope_to_tx(err) {
        TxError::Db(db) => classify_db_err_to_domain(db),
        TxError::Domain(d) => d,
    }
}

pub(super) fn entity_to_model(row: tenants::Model) -> Result<TenantModel, DomainError> {
    let status = TenantStatus::from_smallint(row.status).ok_or_else(|| DomainError::Internal {
        diagnostic: format!("tenants.status out-of-domain value: {}", row.status),
        cause: None,
    })?;
    let depth = u32::try_from(row.depth).map_err(|_| DomainError::Internal {
        diagnostic: format!("tenants.depth negative: {}", row.depth),
        cause: None,
    })?;
    Ok(TenantModel {
        id: row.id,
        parent_id: row.parent_id,
        name: row.name,
        status,
        self_managed: row.self_managed,
        tenant_type_uuid: row.tenant_type_uuid,
        depth,
        created_at: row.created_at,
        updated_at: row.updated_at,
        deleted_at: row.deleted_at,
    })
}

/// Build a simple `Condition` that matches a tenant id. Used everywhere
/// to bridge the `SimpleExpr` returned by `Column::eq` with the
/// `Condition` parameter accepted by `SecureSelect::filter`.
pub(super) fn id_eq(id: Uuid) -> Condition {
    Condition::all().add(tenants::Column::Id.eq(id))
}

/// Maximum number of attempts for a SERIALIZABLE transaction before the
/// retry helper gives up and returns the underlying error to the caller.
pub(super) const MAX_SERIALIZABLE_ATTEMPTS: u32 = 5;

/// Bounds worst-case stuck row when `clear_retention_claim` fails —
/// `claimed_by` would otherwise stay set forever. `claimed_at` decoupled
/// from `updated_at` so future `updated_at` patches don't keep stale
/// claims alive.
pub(super) const RETENTION_CLAIM_TTL: Duration = Duration::from_mins(10);

/// Run a SERIALIZABLE transaction with bounded retry on retryable
/// contention. Thin wrapper over
/// [`toolkit_db::Db::transaction_with_retry_max`] that:
///
/// 1. Adapts AM's per-attempt closure-factory shape (each call returns
///    a fresh `Box<dyn FnOnce(&DbTx) -> _>`) to toolkit-db's
///    `FnMut(&DbTx) -> _` body slot.
/// 2. Pins the attempt budget at [`MAX_SERIALIZABLE_ATTEMPTS`] (the
///    workspace default of 3 is too tight for AM's hot tenant-write
///    paths on `PostgreSQL` under `SERIALIZABLE` isolation).
/// 3. Emits the `AM_SERIALIZABLE_RETRY` counter with
///    `outcome="exhausted"` when a retryable contention survives the
///    full retry budget — this is the operator-facing signal for
///    sustained DB contention required by
///    `dod-tenant-hierarchy-management-concurrency-serializability`.
///    The helper does not emit `outcome="recovered"` because
///    `transaction_with_retry_max` does not surface a per-attempt
///    callback; the `tracing::warn!` it logs on each retry is the
///    near-real-time signal for that case.
/// 4. Translates the surviving error into a typed [`DomainError`] —
///    `TxError::Db` goes through [`classify_db_err_to_domain`],
///    `TxError::Domain` passes through untouched. Domain code never
///    sees a `sea_orm::DbErr`.
///
/// The closure may be invoked multiple times — it must be idempotent.
/// All AM mutating transactions in this file are written so that
/// re-execution from a clean transaction state produces the same end
/// state (re-read row, re-check status, re-issue the same updates).
// @cpt-begin:cpt-cf-account-management-dod-tenant-hierarchy-management-concurrency-serializability:p1:inst-dod-concurrency-serializable-retry
pub(super) async fn with_serializable_retry<F, T>(
    db: &AmDbProvider,
    op: F,
) -> Result<T, DomainError>
where
    F: Fn() -> Box<
            dyn for<'a> FnOnce(
                    &'a DbTx<'a>,
                )
                    -> Pin<Box<dyn Future<Output = Result<T, TxError>> + Send + 'a>>
                + Send,
        > + Send
        + Sync,
    T: Send + 'static,
{
    let backend = db.db().backend();
    let result = db
        .db()
        .transaction_with_retry_max(
            TxConfig::serializable(),
            MAX_SERIALIZABLE_ATTEMPTS,
            TxError::db_err,
            |tx| {
                let inner = op();
                inner(tx)
            },
        )
        .await;
    match result {
        Ok(value) => Ok(value),
        Err(TxError::Db(db_err)) => {
            // Match the retry helper's own classifier
            // (`is_retryable_contention`) so every exhausted retryable
            // contention surfaces the operator signal — including
            // PostgreSQL deadlocks (`40P01`) and SQLite `BUSY` /
            // `BUSY_SNAPSHOT` retries, not just SERIALIZABLE
            // serialization failures (`40001`).
            if is_retryable_contention(backend, &db_err) {
                emit_metric(
                    AM_SERIALIZABLE_RETRY,
                    MetricKind::Counter,
                    &[("outcome", "exhausted")],
                );
            }
            Err(classify_db_err_to_domain(db_err))
        }
        Err(TxError::Domain(d)) => Err(d),
    }
}
// @cpt-end:cpt-cf-account-management-dod-tenant-hierarchy-management-concurrency-serializability:p1:inst-dod-concurrency-serializable-retry

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "helpers_tests.rs"]
mod helpers_tests;
