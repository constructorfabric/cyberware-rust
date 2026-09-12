//! Pricing-local authoring list projection. The shared `OData` grammar is unchanged.
//!
//! The canonical revision subquery and outer read both carry the original plan
//! scope. Correlated child expressions are restricted to that exact tenant/plan;
//! they expose aggregate metadata, not separately protected approval content.
//! Filtering, ordering and seeking happen in SQL, before LIMIT.

use bss_pricing_sdk::odata::{PlanFilterField as Field, PlanOrderField as SortField};
use sea_orm::sea_query::{Alias, Expr, Func, LikeExpr, Query, SelectStatement};
use sea_orm::{
    ColumnTrait, Condition, EntityTrait, ExprTrait, FromQueryResult, Order, QueryOrder,
    QuerySelect, QueryTrait, Value,
};
use time::OffsetDateTime;
use toolkit_db::odata::sea_orm_filter::{encode_cursor_value, parse_cursor_value};
use toolkit_db::secure::{AccessScope, DBRunner, SecureEntityExt};
use toolkit_odata::filter::{
    FilterField, FilterNode, FilterOp, ODataValue, convert_expr_to_filter_node,
};
use toolkit_odata::{
    CursorV1, Error, ODataQuery, Page, PageInfo, SortDir, validate_cursor_against,
};
use uuid::Uuid;

use super::{PlanRevision, to_domain};
use crate::infra::storage::RepoError;
use crate::infra::storage::entity::{plan, price};
use crate::infra::storage::odata_mapping::{
    LIST_LIMIT_CFG, OdataPageError, lifecycle_token, query_with_default_order,
    query_with_unique_order,
};
use crate::infra::storage::repo::approval_repo;

/// One selected revision plus derived, non-persisted read metadata.
#[derive(Debug)]
pub struct PlanListEntry {
    /// Open draft, else the current published/retired revision.
    pub revision: PlanRevision,
    /// Timestamp of the earliest retained revision, never reset on revision open.
    pub created_at: OffsetDateTime,
}

/// Database projection; keep the canonical model decoder instead of copying it.
struct Row {
    model: plan::Model,
    created_at: OffsetDateTime,
    /// `None` unless the caller sorts on it — see the projection below for why
    /// the correlated count is not run on every page.
    price_row_count: Option<i64>,
}

impl FromQueryResult for Row {
    fn from_query_result(row: &sea_orm::QueryResult, prefix: &str) -> Result<Self, sea_orm::DbErr> {
        Ok(Self {
            model: plan::Model::from_query_result(row, prefix)?,
            created_at: row.try_get(prefix, "plan_created_at")?,
            price_row_count: row.try_get(prefix, "price_row_count").ok(),
        })
    }
}

/// Qualify a plan column in the outer query.
fn col(column: plan::Column) -> Expr {
    Expr::col((plan::Entity, column))
}

/// Timestamp of the first revision, including abandoned/superseded history.
fn created_at_expr() -> Expr {
    let origin = Alias::new("plan_origin");
    Query::select()
        .column((origin.clone(), plan::Column::CreatedAtUtc))
        .from_as(plan::Entity, origin.clone())
        .and_where(
            Expr::col((origin.clone(), plan::Column::TenantId)).eq(col(plan::Column::TenantId)),
        )
        .and_where(Expr::col((origin.clone(), plan::Column::PlanId)).eq(col(plan::Column::PlanId)))
        .order_by((origin, plan::Column::Revision), Order::Asc)
        .limit(1)
        .to_owned()
        .into()
}

/// Exact child set authorized by an outer plan row; also used by price aggregates.
fn price_query() -> SelectStatement {
    Query::select()
        .from(price::Entity)
        .and_where(
            Expr::col((price::Entity, price::Column::TenantId)).eq(col(plan::Column::TenantId)),
        )
        .and_where(Expr::col((price::Entity, price::Column::PlanId)).eq(col(plan::Column::PlanId)))
        .and_where(
            Expr::col((price::Entity, price::Column::LifecycleState)).is_in(["draft", "published"]),
        )
        .to_owned()
}

/// SQL count, usable in ORDER BY and the keyset predicate before LIMIT.
fn count_expr() -> Expr {
    price_query()
        .expr(Expr::col((price::Entity, price::Column::PriceId)).count())
        .to_owned()
        .into()
}

/// Sort expression over the shown revision or its derived plan-level metadata.
fn sort_expr(field: SortField) -> Expr {
    match field {
        SortField::PlanId => col(plan::Column::PlanId),
        SortField::PlanName => col(plan::Column::PlanName),
        SortField::LifecycleState => col(plan::Column::LifecycleState),
        SortField::BillingCycle => col(plan::Column::BillingCycle),
        SortField::CreatedAt => created_at_expr(),
        SortField::PriceRowCount => count_expr(),
    }
}

/// Only these sort fields admit SQL NULL; nullable tokens use JSON option strings.
fn nullable(field: SortField) -> bool {
    matches!(field, SortField::PlanName | SortField::BillingCycle)
}

/// Malformed filter error at the same canonical boundary as toolkit conversions.
fn bad_filter(detail: impl Into<String>) -> OdataPageError {
    Error::InvalidFilter(detail.into()).into()
}

/// Values admitted by the Pricing schema (no new toolkit types or operators).
fn sql_value(value: &ODataValue) -> Result<Value, OdataPageError> {
    match value {
        ODataValue::String(s) => Ok(s.clone().into()),
        ODataValue::Uuid(id) => Ok((*id).into()),
        ODataValue::Bool(b) => Ok((*b).into()),
        ODataValue::DateTime(dt) => {
            // Match the entity's `time` storage codec on SQLite as well as PG.
            // Chrono's different text spelling otherwise breaks exact equality.
            let nanos = i128::from(dt.timestamp()) * 1_000_000_000
                + i128::from(dt.timestamp_subsec_nanos());
            let instant = OffsetDateTime::from_unix_timestamp_nanos(nanos)
                .map_err(|_| bad_filter("creation time is out of range"))?;
            Ok(Value::TimeDateTimeWithTimeZone(Some(instant)))
        }
        _ => Err(bad_filter("unsupported plan filter value")),
    }
}

/// Scalar operations, including literal (escaped) substring search.
fn scalar(expr: Expr, op: FilterOp, value: &ODataValue) -> Result<Condition, OdataPageError> {
    if matches!(value, ODataValue::Null) {
        return match op {
            FilterOp::Eq => Ok(expr.is_null().into()),
            FilterOp::Ne => Ok(expr.is_not_null().into()),
            _ => Err(bad_filter("null supports only eq/ne")),
        };
    }
    if matches!(
        op,
        FilterOp::Contains | FilterOp::StartsWith | FilterOp::EndsWith
    ) {
        let ODataValue::String(text) = value else {
            return Err(bad_filter("string function requires a string"));
        };
        let escaped = text
            .replace('!', "!!")
            .replace('%', "!%")
            .replace('_', "!_");
        let pattern = match op {
            FilterOp::StartsWith => format!("{escaped}%"),
            FilterOp::EndsWith => format!("%{escaped}"),
            _ => format!("%{escaped}%"),
        };
        return Ok(expr.like(LikeExpr::new(pattern).escape('!')).into());
    }
    let value = sql_value(value)?;
    Ok(match op {
        FilterOp::Eq => expr.eq(value),
        FilterOp::Ne => expr.ne(value),
        FilterOp::Gt => expr.gt(value),
        FilterOp::Ge => expr.gte(value),
        FilterOp::Lt => expr.lt(value),
        FilterOp::Le => expr.lte(value),
        _ => return Err(bad_filter("unsupported scalar plan filter operator")),
    }
    .into())
}

/// Translate one typed predicate. Collection predicates are independent EXISTS.
fn predicate(
    field: Field,
    op: FilterOp,
    value: &ODataValue,
    pending: &[Uuid],
) -> Result<Condition, OdataPageError> {
    if field == Field::HasPendingApprovals {
        let ODataValue::Bool(wanted) = value else {
            return Err(bad_filter("has_pending_approvals requires true or false"));
        };
        if !matches!(op, FilterOp::Eq | FilterOp::Ne) {
            return Err(bad_filter("pending flag supports eq/ne only"));
        }
        // The bind list is bounded by the tenant's **open units**, not by its
        // catalogue: `pending_plan_ids` reads the open-unit set first and
        // authorizes only those ids, so what lands here is at most one entry per
        // plan under review. It was the catalogue before that inversion, which is
        // the size at which an unchunked `is_in` stops being a slow query and
        // becomes a driver parameter-limit error the caller reads as a 500.
        let exists = Condition::all().add(col(plan::Column::PlanId).is_in(pending.iter().copied()));
        return Ok(if *wanted == (op == FilterOp::Eq) {
            exists
        } else {
            exists.not()
        });
    }
    if matches!(field, Field::ModelKind | Field::Currency) {
        if matches!(value, ODataValue::Null) {
            return Err(bad_filter("price membership requires a string, not null"));
        }
        let column = if field == Field::ModelKind {
            price::Column::ModelKind
        } else {
            price::Column::Currency
        };
        let comparison = scalar(
            Expr::col((price::Entity, column)),
            if op == FilterOp::Ne { FilterOp::Eq } else { op },
            value,
        )?;
        let exists = Condition::all().add(Expr::exists(
            price_query()
                .expr(Expr::val(1))
                .cond_where(comparison)
                .to_owned(),
        ));
        return Ok(if op == FilterOp::Ne {
            exists.not()
        } else {
            exists
        });
    }
    if field == Field::LifecycleState {
        lifecycle_token(value).map_err(bad_filter)?;
    }
    let expr = match field {
        Field::PlanId => col(plan::Column::PlanId),
        Field::PlanName => col(plan::Column::PlanName),
        Field::LifecycleState => col(plan::Column::LifecycleState),
        Field::SkuId => col(plan::Column::SkuId),
        Field::PlanTier => col(plan::Column::PlanTier),
        Field::BillingCycle => col(plan::Column::BillingCycle),
        Field::CreatedAt => created_at_expr(),
        Field::ModelKind | Field::Currency | Field::HasPendingApprovals => {
            return Err(bad_filter("invalid scalar field"));
        }
    };
    // Exact equality retains the authored spelling; UI substring search folds case.
    if field == Field::PlanName
        && matches!(
            op,
            FilterOp::Contains | FilterOp::StartsWith | FilterOp::EndsWith
        )
    {
        let ODataValue::String(text) = value else {
            return Err(bad_filter("name search requires a string"));
        };
        return scalar(
            Func::lower(expr).into(),
            op,
            &ODataValue::String(text.to_lowercase()),
        );
    }
    scalar(expr, op, value)
}

/// Preserve AND/OR/NOT grouping instead of flattening collection predicates.
fn filter(node: &FilterNode<Field>, pending: &[Uuid]) -> Result<Condition, OdataPageError> {
    match node {
        FilterNode::Binary { field, op, value } => predicate(*field, *op, value, pending),
        FilterNode::InList { field, values } => {
            if values.is_empty() {
                return Err(bad_filter("IN list must not be empty"));
            }
            values.iter().try_fold(Condition::any(), |out, value| {
                Ok(out.add(predicate(*field, FilterOp::Eq, value, pending)?))
            })
        }
        FilterNode::Not(inner) => Ok(filter(inner, pending)?.not()),
        FilterNode::Composite { op, children } => {
            let base = match op {
                FilterOp::And => Condition::all(),
                FilterOp::Or => Condition::any(),
                _ => return Err(bad_filter("invalid composite operator")),
            };
            children
                .iter()
                .try_fold(base, |out, child| Ok(out.add(filter(child, pending)?)))
        }
    }
}

/// Inspect typed fields so parser-supported case/property aliases cannot change
/// whether the pending join is loaded.
fn needs_pending(node: &FilterNode<Field>) -> bool {
    match node {
        FilterNode::Binary { field, .. } | FilterNode::InList { field, .. } => {
            *field == Field::HasPendingApprovals
        }
        FilterNode::Not(inner) => needs_pending(inner),
        FilterNode::Composite { children, .. } => children.iter().any(needs_pending),
    }
}

/// Plans the caller may read that hold an open approval unit.
///
/// **The open units are read first and the catalogue is never read whole.** This
/// used to resolve every draft, published and retired plan id the tenant owns and
/// hand the lot to `pending_for_plans`, so a `$filter=has_pending_approvals eq
/// true` materialized the entire catalogue to return a page capped at
/// [`LIST_LIMIT_CFG`]. Inverted, the first read is bounded by how many reviews
/// are open and the authorization read carries those ids as its predicate — the
/// same intersection, in the order where neither side is catalogue-sized.
///
/// Only IDs are loaded, never full plans or an in-memory sorted page.
async fn pending_plan_ids(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
) -> Result<Vec<Uuid>, OdataPageError> {
    #[derive(FromQueryResult)]
    struct Identity {
        plan_id: Uuid,
    }
    let open = approval_repo::plan_ids_with_open_units(runner, tenant_id).await?;
    if open.is_empty() {
        return Ok(Vec::new());
    }
    let ids: Vec<Uuid> = open.iter().map(|plan_id| plan_id.get()).collect();
    let authorized = plan::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(plan::Column::TenantId.eq(tenant_id))
                .add(plan::Column::PlanId.is_in(ids))
                .add(plan::Column::LifecycleState.is_in(["draft", "published", "retired"])),
        )
        .project_all(runner, |q| {
            q.select_only()
                .column(plan::Column::PlanId)
                .distinct()
                .into_model::<Identity>()
        })
        .await
        .map_err(|e| OdataPageError::Db(format!("authorize pending plan join: {e}")))?;
    Ok(authorized.into_iter().map(|row| row.plan_id).collect())
}

/// NULLS LAST lexicographic seek, including mixed directions and nullable ties.
fn seek(cursor: &CursorV1, fields: &[(SortField, SortDir)]) -> Result<Condition, OdataPageError> {
    if cursor.d != "fwd" || cursor.k.len() != fields.len() {
        return Err(Error::CursorInvalidKeys.into());
    }
    let mut equal = Condition::all();
    let mut after = Condition::any();
    for ((field, dir), token) in fields.iter().zip(&cursor.k) {
        let expr = sort_expr(*field);
        let value = if nullable(*field) {
            serde_json::from_str::<Option<String>>(token)
                .map_err(|_| Error::CursorInvalidKeys)?
                .map(Value::from)
        } else {
            Some(parse_cursor_value(field.kind(), token).map_err(|_| Error::CursorInvalidKeys)?)
        };
        if let Some(value) = value {
            let comparison = match dir {
                SortDir::Asc => expr.clone().gt(value.clone()),
                SortDir::Desc => expr.clone().lt(value.clone()),
            };
            let mut step = Condition::any().add(comparison);
            if nullable(*field) {
                step = step.add(expr.clone().is_null());
            }
            after = after.add(equal.clone().add(step));
            equal = equal.add(expr.eq(value));
        } else {
            // A null is already last; only subsequent sort keys can advance.
            equal = equal.add(expr.is_null());
        }
    }
    Ok(after)
}

/// Encode the exact values SQL ordered by; JSON distinguishes NULL from "null".
fn cursor_keys(row: &Row, fields: &[(SortField, SortDir)]) -> Result<Vec<String>, OdataPageError> {
    fields
        .iter()
        .map(|(field, _)| {
            if nullable(*field) {
                let text = if *field == SortField::PlanName {
                    &row.model.plan_name
                } else {
                    &row.model.billing_cycle
                };
                return serde_json::to_string(text)
                    .map_err(|e| OdataPageError::Db(format!("encode nullable plan cursor: {e}")));
            }
            let value = match field {
                SortField::PlanId => row.model.plan_id.into(),
                SortField::LifecycleState => row.model.lifecycle_state.clone().into(),
                SortField::CreatedAt => Value::TimeDateTimeWithTimeZone(Some(row.created_at)),
                SortField::PriceRowCount => row
                    .price_row_count
                    .ok_or_else(|| {
                        OdataPageError::Db(
                            "plan cursor sorts on price_row_count but the column was not \
                             projected"
                                .to_owned(),
                        )
                    })?
                    .into(),
                SortField::PlanName | SortField::BillingCycle => {
                    return Err(Error::CursorInvalidKeys.into());
                }
            };
            encode_cursor_value(&value, field.kind()).map_err(OdataPageError::Db)
        })
        .collect()
}

/// Read one SQL-paginated page of canonical authoring revisions.
///
/// # Errors
/// Invalid filters/cursors are client errors; scoped storage/corruption errors fail closed.
pub(super) async fn list(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    query: &ODataQuery,
) -> Result<Page<PlanListEntry>, OdataPageError> {
    let query = query_with_unique_order(
        &query_with_default_order(query, &[SortField::PlanId]),
        &[SortField::PlanId],
    );
    let fields = query
        .order
        .0
        .iter()
        .map(|key| {
            SortField::from_name(&key.field)
                .map(|field| (field, key.dir))
                .ok_or_else(|| Error::InvalidOrderByField(key.field.clone()))
        })
        .collect::<Result<Vec<_>, _>>()?;
    if let Some(cursor) = &query.cursor {
        if cursor.f.is_some() != query.filter_hash.is_some() {
            return Err(Error::FilterMismatch.into());
        }
        validate_cursor_against(cursor, &query.order, query.filter_hash.as_deref())?;
    }
    let node = query
        .filter
        .as_deref()
        .map(convert_expr_to_filter_node::<Field>)
        .transpose()
        .map_err(|e| bad_filter(e.to_string()))?;
    let pending = if node.as_ref().is_some_and(needs_pending) {
        pending_plan_ids(runner, scope, tenant_id).await?
    } else {
        Vec::new()
    };

    let canonical = plan::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(plan::Column::TenantId.eq(tenant_id))
                .add(plan::Column::LifecycleState.is_in(["draft", "published", "retired"])),
        )
        .into_inner()
        .select_only()
        .column(plan::Column::PlanId)
        .column_as(plan::Column::Revision.max(), "revision")
        .group_by(plan::Column::PlanId)
        .into_query();
    let mut condition = Condition::all()
        .add(plan::Column::TenantId.eq(tenant_id))
        .add(
            Expr::tuple([col(plan::Column::PlanId), col(plan::Column::Revision)])
                .in_subquery(canonical),
        );
    if let Some(node) = node {
        condition = condition.add(filter(&node, &pending)?);
    }
    if let Some(cursor) = &query.cursor {
        condition = condition.add(seek(cursor, &fields)?);
    }
    let limit = Ord::min(
        query.limit.unwrap_or(LIST_LIMIT_CFG.default),
        LIST_LIMIT_CFG.max,
    );
    let mut rows = plan::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(condition)
        .limit(limit.saturating_add(1))
        .project_all(runner, |q| {
            // **The correlated count is projected only when a sort key reads it.**
            //
            // `count_expr()` is a COUNT over `pricing_price` evaluated per returned
            // row — up to `limit + 1` of them — and the value never reached the
            // response: `PlanListEntry` does not carry it, and the handler fills
            // `PlanSummaryView::price_row_count` from D-360's one grouped read
            // instead. The only reader left is `cursor_keys`, and only on the one
            // sort field whose keyset predicate is built from `sort_expr`, which
            // composes the same expression itself.
            let mut q = q.column_as(created_at_expr(), "plan_created_at");
            if fields
                .iter()
                .any(|(field, _)| *field == SortField::PriceRowCount)
            {
                q = q.column_as(count_expr(), "price_row_count");
            }
            for (field, dir) in &fields {
                let expr = sort_expr(*field);
                if nullable(*field) {
                    q = q.order_by(expr.clone().is_null(), Order::Asc);
                }
                q = q.order_by(
                    expr,
                    match dir {
                        SortDir::Asc => Order::Asc,
                        SortDir::Desc => Order::Desc,
                    },
                );
            }
            q.into_model::<Row>()
        })
        .await
        .map_err(|e| OdataPageError::Db(format!("list authoring plans: {e}")))?;
    let has_more = u64::try_from(rows.len()).unwrap_or(u64::MAX) > limit;
    if has_more {
        rows.pop();
    }
    let next_cursor = if has_more {
        rows.last()
            .map(|row| {
                CursorV1 {
                    k: cursor_keys(row, &fields)?,
                    o: fields.first().map_or(SortDir::Asc, |(_, dir)| *dir),
                    s: query.order.to_signed_tokens(),
                    f: query.filter_hash.clone(),
                    d: "fwd".to_owned(),
                }
                .encode()
                .map_err(|e| OdataPageError::Db(format!("encode plan cursor: {e}")))
            })
            .transpose()?
    } else {
        None
    };
    Ok(Page {
        items: rows
            .into_iter()
            .map(|row| {
                if row.price_row_count.is_some_and(|count| count < 0) {
                    return Err(RepoError::CorruptRow("negative price count".to_owned()));
                }
                Ok(PlanListEntry {
                    revision: to_domain(row.model)?,
                    created_at: row.created_at,
                })
            })
            .collect::<Result<_, RepoError>>()?,
        page_info: PageInfo {
            next_cursor,
            prev_cursor: None,
            limit,
        },
    })
}
