//! `qe_quotas`: insert, locked and plain reads, compare-and-set writes, the
//! keyset page, and the two platform-plane aggregates.
//!
//! Writes are compare-and-set on `(id, status = 'active', record_version)`:
//! a row that moved under the caller affects zero rows, which the store
//! reports instead of overwriting. The row lock (`FOR UPDATE` on `PostgreSQL`,
//! omitted on `SQLite`, whose writers serialize) is taken on the plain select
//! before the secure layer scopes it.

use sea_orm::sea_query::{Condition, Expr, ExprTrait, LockType};
use sea_orm::{ColumnTrait, EntityTrait, FromQueryResult, Order, QueryFilter, QuerySelect};
use time::OffsetDateTime;
use toolkit_db::secure::{DBRunner, ScopeError, SecureEntityExt, SecureUpdateExt, secure_insert};
use toolkit_security::AccessScope;
use uuid::Uuid;

use crate::infra::storage::entity::quota::{self, Column, Entity};
use crate::infra::storage::quota_mapping::{QuotaUpdate, STATUS_ACTIVE, STATUS_DEACTIVATED};

/// The filter of one list page. Every `Some` narrows; `ids` empty means no
/// restriction; `after` is the exclusive lower bound of the keyset.
#[derive(Debug, Clone, Default)]
pub struct ListQuery<'a> {
    /// Owning tenant.
    pub tenant_id: Option<Uuid>,
    /// `(projection_type, subject_id)`.
    pub subject: Option<(&'a str, &'a str)>,
    /// Metric instance id.
    pub metric: Option<&'a str>,
    /// Stored status name.
    pub status: Option<&'a str>,
    /// Explicit ids.
    pub ids: &'a [Uuid],
    /// Return rows with `id > after` only.
    pub after: Option<Uuid>,
    /// Rows to return.
    pub limit: u64,
}

/// One `(metric, projection_type)` pair of the bindings aggregate.
#[derive(Debug, Clone, PartialEq, Eq, FromQueryResult)]
pub struct BindingRow {
    /// Metric instance id.
    pub metric: String,
    /// Subject projection type id.
    pub projection_type: String,
}

/// One row of the per-metric count aggregate.
#[derive(Debug, Clone, PartialEq, Eq, FromQueryResult)]
pub struct MetricCountRow {
    /// Metric instance id.
    pub metric: String,
    /// Active Quotas on it.
    pub total: i64,
}

/// Insert a new row inside `scope`.
///
/// # Errors
///
/// The scope or database error of the insert.
pub async fn insert(
    runner: &impl DBRunner,
    scope: &AccessScope,
    row: quota::ActiveModel,
) -> Result<quota::Model, ScopeError> {
    secure_insert::<Entity>(row, scope, runner).await
}

/// The row with `id` inside `scope`, locked for update when `lock` is set.
///
/// # Errors
///
/// The scope or database error of the read.
pub async fn find_by_id(
    runner: &impl DBRunner,
    scope: &AccessScope,
    id: Uuid,
    lock: bool,
) -> Result<Option<quota::Model>, ScopeError> {
    let mut select = Entity::find().filter(Column::Id.eq(id));
    if lock {
        select = select.lock(LockType::Update);
    }
    select.secure().scope_with(scope).one(runner).await
}

/// One keyset page ordered by id ascending.
///
/// # Errors
///
/// The scope or database error of the read.
pub async fn list_page(
    runner: &impl DBRunner,
    scope: &AccessScope,
    query: &ListQuery<'_>,
) -> Result<Vec<quota::Model>, ScopeError> {
    let mut condition = Condition::all();
    if let Some(tenant) = query.tenant_id {
        condition = condition.add(Column::TenantId.eq(tenant));
    }
    if let Some((projection_type, subject_id)) = query.subject {
        condition = condition
            .add(Column::ProjectionType.eq(projection_type))
            .add(Column::SubjectId.eq(subject_id));
    }
    if let Some(metric) = query.metric {
        condition = condition.add(Column::Metric.eq(metric));
    }
    if let Some(status) = query.status {
        condition = condition.add(Column::Status.eq(status));
    }
    if !query.ids.is_empty() {
        condition = condition.add(Column::Id.is_in(query.ids.iter().copied()));
    }
    if let Some(after) = query.after {
        condition = condition.add(Column::Id.gt(after));
    }
    Entity::find()
        .secure()
        .scope_with(scope)
        .filter(condition)
        .order_by(Column::Id, Order::Asc)
        .limit(query.limit)
        .all(runner)
        .await
}

/// Apply `update` to the active row at `expected_version`, bumping the
/// version and `updated_at`. `false` when no such row exists any more.
///
/// # Errors
///
/// The scope or database error of the write.
pub async fn apply_update(
    runner: &impl DBRunner,
    scope: &AccessScope,
    id: Uuid,
    expected_version: i32,
    update: &QuotaUpdate,
    now: OffsetDateTime,
) -> Result<bool, ScopeError> {
    let mut statement = Entity::update_many()
        .col_expr(Column::RecordVersion, Expr::value(expected_version + 1))
        .col_expr(Column::UpdatedAt, Expr::value(now));
    if let Some(cap) = update.cap {
        statement = statement.col_expr(Column::Cap, Expr::value(cap));
    }
    if let Some(thresholds) = &update.notification_thresholds {
        statement = statement.col_expr(
            Column::NotificationThresholds,
            Expr::value(thresholds.clone()),
        );
    }
    if let Some((start, end)) = update.validity {
        statement = statement
            .col_expr(Column::ValidityStart, Expr::value(start))
            .col_expr(Column::ValidityEnd, Expr::value(end));
    }
    if let Some(metadata) = &update.metadata {
        statement = statement.col_expr(Column::Metadata, Expr::value(metadata.clone()));
    }
    if let Some((contract_type, contract_version)) = &update.constraint_contract {
        statement = statement
            .col_expr(
                Column::ConstraintContractType,
                Expr::value(contract_type.clone()),
            )
            .col_expr(
                Column::ConstraintContractVersion,
                Expr::value(*contract_version),
            );
    }
    if let Some(mode) = &update.enforcement_mode {
        statement = statement.col_expr(Column::EnforcementMode, Expr::value(mode.clone()));
    }
    if let Some(hint) = update.fail_open_hint {
        statement = statement.col_expr(Column::FailOpenHint, Expr::value(hint));
    }
    let result = statement
        .filter(cas_predicate(id, expected_version))
        .secure()
        .scope_with(scope)
        .exec(runner)
        .await?;
    Ok(result.rows_affected == 1)
}

/// Flip the active row at `expected_version` to `deactivated`. `false` when
/// no such row exists any more.
///
/// # Errors
///
/// The scope or database error of the write.
pub async fn mark_deactivated(
    runner: &impl DBRunner,
    scope: &AccessScope,
    id: Uuid,
    expected_version: i32,
    now: OffsetDateTime,
) -> Result<bool, ScopeError> {
    let result = Entity::update_many()
        .col_expr(Column::Status, Expr::value(STATUS_DEACTIVATED))
        .col_expr(Column::RecordVersion, Expr::value(expected_version + 1))
        .col_expr(Column::UpdatedAt, Expr::value(now))
        .filter(cas_predicate(id, expected_version))
        .secure()
        .scope_with(scope)
        .exec(runner)
        .await?;
    Ok(result.rows_affected == 1)
}

fn cas_predicate(id: Uuid, expected_version: i32) -> Condition {
    Condition::all()
        .add(Column::Id.eq(id))
        .add(Column::Status.eq(STATUS_ACTIVE))
        .add(Column::RecordVersion.eq(expected_version))
}

/// Distinct `(metric, projection_type)` pairs of active rows, platform-wide.
///
/// # Errors
///
/// The database error of the read.
pub async fn active_bindings(runner: &impl DBRunner) -> Result<Vec<BindingRow>, ScopeError> {
    Entity::find()
        .secure()
        .scope_with(&AccessScope::allow_all())
        .filter(Condition::all().add(Column::Status.eq(STATUS_ACTIVE)))
        .project_all(runner, |q| {
            q.select_only()
                .column(Column::Metric)
                .column(Column::ProjectionType)
                .distinct()
                .into_model::<BindingRow>()
        })
        .await
}

/// Active rows per metric, platform-wide.
///
/// # Errors
///
/// The database error of the read.
pub async fn active_counts_by_metric(
    runner: &impl DBRunner,
) -> Result<Vec<MetricCountRow>, ScopeError> {
    Entity::find()
        .secure()
        .scope_with(&AccessScope::allow_all())
        .filter(Condition::all().add(Column::Status.eq(STATUS_ACTIVE)))
        .project_all(runner, |q| {
            q.select_only()
                .column(Column::Metric)
                .column_as(Expr::col(Column::Id).count(), "total")
                .group_by(Column::Metric)
                .into_model::<MetricCountRow>()
        })
        .await
}

/// Active rows with `cap = 0`, platform-wide.
///
/// # Errors
///
/// The database error of the read.
pub async fn count_active_cap_zero(runner: &impl DBRunner) -> Result<u64, ScopeError> {
    Entity::find()
        .secure()
        .scope_with(&AccessScope::allow_all())
        .filter(
            Condition::all()
                .add(Column::Status.eq(STATUS_ACTIVE))
                .add(Column::Cap.eq(0_i64)),
        )
        .count(runner)
        .await
}

/// Active rows with an unbounded cap, platform-wide.
///
/// # Errors
///
/// The database error of the read.
pub async fn count_active_cap_unbounded(runner: &impl DBRunner) -> Result<u64, ScopeError> {
    Entity::find()
        .secure()
        .scope_with(&AccessScope::allow_all())
        .filter(
            Condition::all()
                .add(Column::Status.eq(STATUS_ACTIVE))
                .add(Column::Cap.is_null()),
        )
        .count(runner)
        .await
}
