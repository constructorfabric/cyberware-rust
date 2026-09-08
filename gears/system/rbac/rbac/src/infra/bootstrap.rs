//! Platform-admin bootstrap — idempotent Owner-at-`/` assignment.
//!
//! Writes a single `role_assignments` row granting the configured
//! platform-admin subject the Owner role at scope `"/"`. The bootstrap
//! writes directly to the table (bypassing the REST handler path, which
//! cannot authorize the very first admin), and is idempotent: a
//! pre-existing row surfaces as [`BootstrapOutcome::AlreadyAssigned`] via
//! `INSERT … ON CONFLICT (uq_assignment columns) DO NOTHING`. The clause
//! collapses the SELECT-then-INSERT race entirely and is fail-loud — if
//! `uq_assignment` is ever dropped, Postgres rejects the INSERT at
//! runtime instead of silently producing duplicate admin grants.
//!
//! Bootstrap rows carry `created_by = "system-bootstrap"`, distinct from
//! the seeder's `"system"`, so the two write paths are unambiguous in the
//! audit trail.

use chrono::Utc;
use sea_orm::sea_query::OnConflict;
use sea_orm::{ActiveValue::Set, DbErr, EntityTrait};
use toolkit_db::secure::{AccessScope, DbConn, ScopeError, SecureInsertExt};
use uuid::Uuid;

use rbac_sdk::error::RbacServiceError;
use rbac_sdk::models::PrincipalType;

use crate::infra::error_conv::redacted_scope_error;
use crate::infra::storage::entity::role_assignment;

/// Attribution stamped on `created_by` for all bootstrap-written rows.
/// Distinct from the seeder's `SYSTEM_CREATED_BY` (`"system"`).
pub const SYSTEM_BOOTSTRAP_CREATED_BY: &str = "system-bootstrap";

/// Fixed UUID for the canonical Owner built-in role.
pub const OWNER_ROLE_ID: Uuid = {
    // Verified by `owner_role_id_constant_matches_catalog` test.
    Uuid::from_u128(0x0195_f2b6_0001_7000_8000_0000_0000_0001_u128)
};

/// Fixed UUID for the canonical Credstore Secret Operator built-in role.
pub const CREDSTORE_SECRET_OPERATOR_ROLE_ID: Uuid = {
    // Verified by `credstore_operator_role_id_constant_matches_catalog` test.
    Uuid::from_u128(0x0195_f2b6_0005_7000_8000_0000_0000_0005_u128)
};

/// Outcome returned by [`BootstrapPlatformAdmin::run`].
#[derive(Debug, PartialEq, Eq)]
pub enum BootstrapOutcome {
    /// A new Owner-at-`/` assignment was inserted.
    Created,
    /// An Owner-at-`/` assignment already existed; the bootstrap was a no-op.
    AlreadyAssigned,
}

/// Decision produced by [`evaluate_bootstrap_decision`] before any I/O.
#[derive(Debug)]
pub enum BootstrapDecision {
    /// Proceed and run the bootstrap with the given subject identifier.
    Run(String),
    /// Skip the bootstrap because no subject identifier was configured.
    Skip,
}

/// Decide whether the bootstrap should run, given the optional subject id.
#[must_use]
pub fn evaluate_bootstrap_decision(platform_admin_subject_id: Option<&str>) -> BootstrapDecision {
    match platform_admin_subject_id {
        Some(id) => BootstrapDecision::Run(id.to_owned()),
        None => BootstrapDecision::Skip,
    }
}

/// Idempotent bootstrap for the platform administrator's Owner-at-`/` grant.
#[derive(Debug, Default)]
pub struct BootstrapPlatformAdmin;

impl BootstrapPlatformAdmin {
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// Create the Owner-at-`/` assignment for `subject_id`, or confirm one
    /// already exists.
    ///
    /// Issues a single `INSERT … ON CONFLICT (role_definition_id,
    /// principal_type, principal_id, scope) DO NOTHING`. The conflict
    /// arbiter matches the `uq_assignment` unique index, so a pre-existing
    /// row — including the race-loser case under concurrent multi-replica
    /// bootstrap — collapses into [`BootstrapOutcome::AlreadyAssigned`]
    /// atomically. There is no SELECT-then-INSERT TOCTOU window.
    ///
    /// `principal_type` is part of the conflict target, so a pre-existing
    /// Group or `ServicePrincipal` row at the same `(role, principal_id,
    /// scope)` is a *different* uq-key and cannot mask the User-row
    /// bootstrap (covered in `postgres_bootstrap` by
    /// `bootstrap_creates_user_row_when_group_row_with_same_principal_id_exists`).
    ///
    /// # Errors
    ///
    /// * [`RbacServiceError::Internal`] if the INSERT fails for any reason
    ///   other than the `DO NOTHING` no-op (e.g. FK violation, connection
    ///   error). The `DO NOTHING` no-op surfaces as
    ///   [`BootstrapOutcome::AlreadyAssigned`], not an error.
    pub async fn run(
        &self,
        conn: &DbConn<'_>,
        subject_id: &str,
    ) -> Result<BootstrapOutcome, RbacServiceError> {
        let now = Utc::now();
        let active = build_role_assignment_active_model(subject_id, now);
        insert_assignment_idempotent(conn, active, &format!("subject {subject_id}")).await
    }
}

/// Idempotently insert a `role_assignments` row, mapping the
/// `ON CONFLICT … DO NOTHING` no-op to [`BootstrapOutcome::AlreadyAssigned`].
///
/// `attribution` is interpolated into error messages to identify the grant
/// (e.g. `"subject <id>"` or `"vp-idp-plugin credstore grant"`).
///
/// Arbiter columns mirror the `uq_assignment` unique index exactly. If that
/// index is ever renamed or dropped, Postgres errors out with `42P10
/// invalid_column_reference` — duplicates can never ship silently.
async fn insert_assignment_idempotent(
    conn: &DbConn<'_>,
    active: role_assignment::ActiveModel,
    attribution: &str,
) -> Result<BootstrapOutcome, RbacServiceError> {
    let on_conflict = OnConflict::columns([
        role_assignment::Column::RoleDefinitionId,
        role_assignment::Column::PrincipalType,
        role_assignment::Column::PrincipalId,
        role_assignment::Column::Scope,
    ])
    .do_nothing()
    .to_owned();

    let result = role_assignment::Entity::insert(active)
        .secure()
        .scope_unchecked(&AccessScope::allow_all())
        .map_err(|err| {
            RbacServiceError::internal(format!(
                "rbac bootstrap: failed to apply allow-all scope for {attribution}: {}",
                redacted_scope_error(&err)
            ))
        })?
        .on_conflict_raw(on_conflict)
        .exec(conn)
        .await;

    match result {
        Ok(_) => Ok(BootstrapOutcome::Created),
        // SeaORM surfaces `ON CONFLICT … DO NOTHING` with no row inserted as
        // `DbErr::RecordNotInserted`. The goal (assignment exists) is already
        // satisfied by the conflicting row, so this is success, not failure.
        Err(ScopeError::Db(DbErr::RecordNotInserted)) => Ok(BootstrapOutcome::AlreadyAssigned),
        Err(err) => Err(RbacServiceError::internal(format!(
            "rbac bootstrap: INSERT failed for {attribution}: {}",
            redacted_scope_error(&err)
        ))),
    }
}

/// Idempotently grant a configured principal a built-in role at root scope.
///
/// Covers both configured grant lists. In-process system actors (an `IdP`
/// plugin writing per-realm admin secrets, for instance) run under their own
/// `SecurityContext`; a grant here is what lets the authz-resolver authorize
/// their writes through ordinary RBAC instead of a PEP bypass. Human operators
/// need one so a fresh deployment has somebody able to administer it before any
/// assignment API call is possible. Which principals exist, under which subject
/// ids, and as which `principal_type`, belongs to the deployment — so every
/// argument comes from `RbacServiceConfig` and nothing is granted unless
/// configured.
///
/// # Errors
///
/// [`RbacServiceError::Internal`] if the INSERT fails for any reason other
/// than the `DO NOTHING` no-op (which surfaces as
/// [`BootstrapOutcome::AlreadyAssigned`]).
pub async fn seed_configured_grant(
    conn: &DbConn<'_>,
    role_id: Uuid,
    principal_id: &str,
    principal_type: PrincipalType,
) -> Result<BootstrapOutcome, RbacServiceError> {
    let now = Utc::now();
    let active = build_configured_grant_active_model(role_id, principal_id, principal_type, now);
    insert_assignment_idempotent(conn, active, "configured grant").await
}

/// Build the `ActiveModel` for the Owner-at-`/` bootstrap row.
///
/// `scope_depth` and `tenant_id` are derived from the scope via
/// [`rbac_sdk::models::Scope::depth`] and
/// [`rbac_sdk::models::Scope::tenant_id`]. For this bootstrap row the scope
/// is root, so `tenant_id` resolves to `None` and `scope_depth` to `1`.
#[must_use]
pub fn build_role_assignment_active_model(
    subject_id: &str,
    now: chrono::DateTime<Utc>,
) -> role_assignment::ActiveModel {
    let scope = rbac_sdk::models::Scope::root();
    role_assignment::ActiveModel {
        id: Set(Uuid::now_v7()),
        role_definition_id: Set(OWNER_ROLE_ID),
        principal_id: Set(subject_id.to_owned()),
        principal_type: Set(PrincipalType::User.as_str().to_owned()),
        scope: Set(scope.path()),
        scope_depth: Set(scope.depth()),
        tenant_id: Set(scope.tenant_id()),
        created_at: Set(now),
        updated_at: Set(now),
        created_by: Set(SYSTEM_BOOTSTRAP_CREATED_BY.to_owned()),
        // The bootstrap has no caller and therefore no author identity to
        // record: `SYSTEM_BOOTSTRAP_CREATED_BY` is a marker, not a subject
        // any identity provider can resolve. NULL is the honest value, and
        // the read path renders it as "no author name" — inventing a kind
        // and tenant here would make the row claim a person granted it.
        created_by_type: Set(None),
        created_by_tenant_id: Set(None),
    }
}

/// Build the `ActiveModel` for a configured grant at root scope. Root scope ⇒
/// `tenant_id = None`, `scope_depth = 1`; a secret filed under the actor's own
/// tenant is covered by root scope, so an own-tenant gate on the target gear
/// still passes.
///
/// Root is the only scope this path writes. A tenant-scoped grant would name a
/// tenant that need not exist when RBAC starts, and this path writes straight
/// to the table — it never reaches the scope-existence validation the REST
/// handler performs.
#[must_use]
pub fn build_configured_grant_active_model(
    role_id: Uuid,
    principal_id: &str,
    principal_type: PrincipalType,
    now: chrono::DateTime<Utc>,
) -> role_assignment::ActiveModel {
    let scope = rbac_sdk::models::Scope::root();
    role_assignment::ActiveModel {
        id: Set(Uuid::now_v7()),
        role_definition_id: Set(role_id),
        principal_id: Set(principal_id.to_owned()),
        principal_type: Set(principal_type.as_str().to_owned()),
        scope: Set(scope.path()),
        scope_depth: Set(scope.depth()),
        tenant_id: Set(scope.tenant_id()),
        created_at: Set(now),
        updated_at: Set(now),
        created_by: Set(SYSTEM_BOOTSTRAP_CREATED_BY.to_owned()),
        // The bootstrap has no caller and therefore no author identity to
        // record: `SYSTEM_BOOTSTRAP_CREATED_BY` is a marker, not a subject
        // any identity provider can resolve. NULL is the honest value, and
        // the read path renders it as "no author name" — inventing a kind
        // and tenant here would make the row claim a person granted it.
        created_by_type: Set(None),
        created_by_tenant_id: Set(None),
    }
}

#[cfg(test)]
#[path = "bootstrap_tests.rs"]
mod bootstrap_tests;
