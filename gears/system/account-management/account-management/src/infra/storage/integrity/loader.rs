//! Snapshot loader for the Rust-side integrity check.
//!
//! Two `SecureSelect` calls — `tenants` and `tenant_closure` — issued
//! against the same `DbTx<'_>` so both reads are observed against a
//! single MVCC snapshot. The transaction is opened by the caller
//! (`integrity::run_integrity_check`) with the strongest repeatable
//! isolation the underlying backend supports (`REPEATABLE READ` on
//! `Postgres`; `Serializable` on `SQLite`, which is the only level
//! `SQLite` exposes — see `toolkit_db::secure::tx_config`). The
//! `integrity_check_runs` single-flight gate INSERT/DELETE happen in
//! **separate** committed transactions wrapping this snapshot tx
//! (see `integrity::lock`), so the snapshot tx itself contains no
//! writes and runs as a clean read-only window over both tables.
//!
//! Both SELECTs scope via [`AccessScope::allow_all`] unconditionally —
//! `tenant_closure` rows are `no_tenant`/`no_resource` (per the
//! entity's `#[secure(...)]` declaration), so any narrower
//! `AccessScope` collapses to `WHERE false` and hides every row, and
//! the integrity check needs to see every tenant + closure row to
//! detect cross-cutting corruption (orphans, cycles, strict-ancestor
//! gaps).
//!
//! `tenants.status` is filtered to exclude only `Provisioning` (per
//! ADR-0007 — classifiers treat provisioning inputs as a programmer
//! error). Rows with unexpected/corrupt status values are deliberately
//! loaded so `tenant_row_to_snap()` can fail fast rather than silently
//! hiding corruption that the repair planner might misinterpret.

use sea_orm::{ColumnTrait, Condition, EntityTrait};
use toolkit_db::secure::{DbTx, SecureEntityExt};
use toolkit_security::AccessScope;

use crate::domain::error::DomainError;
use crate::domain::tenant::model::TenantStatus;
use crate::infra::storage::entity::{tenant_closure, tenants};

use super::snapshot::{ClosureSnap, Snapshot, TenantSnap};

/// Load a `(tenants, tenant_closure)` snapshot from the supplied
/// transaction.
///
/// Both SELECTs run inside the caller's `DbTx<'_>`, so the snapshot is
/// guaranteed to be MVCC-consistent. Provisioning tenants are filtered
/// out at the SQL level.
///
/// # Errors
///
/// Any `DbErr` from either SELECT, mapped through the canonical
/// `From<DbError> for DomainError` ladder via `map_scope_err`.
pub async fn load_snapshot(tx: &DbTx<'_>, scope: &AccessScope) -> Result<Snapshot, DomainError> {
    // Production callers pass `AccessScope::allow_all` (the integrity
    // check is run by the service layer with allow_all per the
    // Phase-2 call-site rewrite). Defensively reject narrower scopes
    // here so a future caller cannot silently downgrade the audit by
    // hiding ancestor rows that orphan / cycle / strict-ancestor
    // classifiers need to see.
    if !scope.is_unconstrained() {
        return Err(DomainError::internal(
            "integrity-check loader requires AccessScope::allow_all; \
             a narrower scope would hide ancestor rows and \
             produce false-negative orphan/cycle reports",
        ));
    }
    let allow_all = AccessScope::allow_all();

    let tenant_rows = tenants::Entity::find()
        .secure()
        .scope_with(&allow_all)
        .filter(non_provisioning_filter())
        .all(tx)
        .await
        .map_err(map_scope_err)?;

    let mut tenant_snaps: Vec<TenantSnap> = Vec::with_capacity(tenant_rows.len());
    for row in &tenant_rows {
        tenant_snaps.push(tenant_row_to_snap(row)?);
    }

    let closure_rows = tenant_closure::Entity::find()
        .secure()
        .scope_with(&allow_all)
        .all(tx)
        .await
        .map_err(map_scope_err)?;

    let mut closure_snaps: Vec<ClosureSnap> = Vec::with_capacity(closure_rows.len());
    for row in &closure_rows {
        closure_snaps.push(closure_row_to_snap(row)?);
    }

    Ok(Snapshot::new(tenant_snaps, closure_snaps))
}

/// Exclude only `Provisioning` rows (per ADR-0007) so that rows with
/// unexpected/corrupt status values still enter the snapshot and fail
/// fast in `tenant_row_to_snap()` rather than silently disappearing
/// (which would cause the repair planner to treat their closure rows
/// as orphaned extras and delete them).
fn non_provisioning_filter() -> Condition {
    Condition::all().add(tenants::Column::Status.ne(TenantStatus::Provisioning.as_smallint()))
}

fn tenant_row_to_snap(row: &tenants::Model) -> Result<TenantSnap, DomainError> {
    let status = TenantStatus::from_smallint(row.status).ok_or_else(|| {
        DomainError::internal(format!(
            "tenants.status out-of-domain value: {}",
            row.status
        ))
    })?;
    // Defence-in-depth: `non_provisioning_filter` excludes
    // Provisioning at SELECT time, but reject the row here too so a
    // future relaxation of that filter cannot silently leak
    // Provisioning into the snapshot per ADR-0007. The classifiers
    // treat Provisioning inputs as a programmer error.
    if !status.is_sdk_visible() {
        return Err(DomainError::internal(format!(
            "tenants.status must be SDK-visible in integrity snapshot per ADR-0007: {}",
            row.status
        )));
    }
    Ok(TenantSnap {
        id: row.id,
        parent_id: row.parent_id,
        status,
        depth: row.depth,
        self_managed: row.self_managed,
    })
}

fn closure_row_to_snap(row: &tenant_closure::Model) -> Result<ClosureSnap, DomainError> {
    let descendant_status =
        TenantStatus::from_smallint(row.descendant_status).ok_or_else(|| {
            DomainError::internal(format!(
                "tenant_closure.descendant_status out-of-domain value: {}",
                row.descendant_status
            ))
        })?;
    // Defence-in-depth: unlike `tenants`, `tenant_closure` has NO
    // SQL-level filter on `descendant_status` (closure rows only
    // exist for activated tenants per the activation contract, so
    // historically there was nothing to filter). A stale or corrupt
    // Provisioning value here would still be handed to the
    // classifiers as a valid input. Reject it explicitly so
    // ADR-0007 holds without relying on the activation contract
    // staying honoured forever.
    if !descendant_status.is_sdk_visible() {
        return Err(DomainError::internal(format!(
            "tenant_closure.descendant_status must be SDK-visible in integrity snapshot per ADR-0007: {}",
            row.descendant_status
        )));
    }
    Ok(ClosureSnap {
        ancestor_id: row.ancestor_id,
        descendant_id: row.descendant_id,
        barrier: row.barrier,
        descendant_status,
    })
}

/// Mirror of `repo_impl::map_scope_err` — kept private to the
/// integrity-check gear so loader/lock errors funnel through the
/// same canonical `From<DbError>` ladder used by the rest of the AM
/// repo.
fn map_scope_err(err: toolkit_db::secure::ScopeError) -> DomainError {
    use toolkit_db::secure::ScopeError;
    match err {
        ScopeError::Db(db) => DomainError::from(toolkit_db::DbError::Sea(db)),
        ScopeError::Invalid(msg) => DomainError::internal(format!("scope invalid: {msg}")),
        ScopeError::TenantNotInScope { .. } => DomainError::CrossTenantDenied { cause: None },
        ScopeError::Denied(msg) => DomainError::internal(format!(
            "unexpected access denied in AM integrity-check loader: {msg}"
        )),
        // `ScopeError` is `#[non_exhaustive]`: variants this gear has no
        // specific answer for (today the graph-query refusals, which it can
        // never trigger) map to an internal error, like `Invalid`.
        other => DomainError::internal(format!("scope invalid: {other}")),
    }
}
