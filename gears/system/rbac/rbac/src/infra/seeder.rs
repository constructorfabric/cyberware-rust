//! Idempotent seeder for the platform's built-in roles.
//!
//! [`BuiltinRoleSeeder::seed`] issues one
//! `INSERT … ON CONFLICT (id) DO UPDATE` per role, in **ascending `id`
//! order**, and the caller runs the whole roster inside one transaction, so a
//! crash mid-roster cannot leave the built-in catalogue half seeded. The
//! ascending order is the lock-ordering invariant that makes
//! that safe: it closes the disjoint-pair deadlock class `(A→B)` vs
//! `(B→A)` between two concurrent seeders.
//!
//! Idempotency invariants:
//!
//! * Conflict target is `id` (not `name`), so renaming a built-in role
//!   updates its row instead of orphaning it.
//! * `is_built_in` and `owner_tenant_id` are NEVER touched on conflict —
//!   only `name`, `description`, `permissions`, `not_permissions`,
//!   `assignable_scopes`, and `updated_at` are updated.
//! * `verify_seeded_invariants` reads each row back and rejects any with
//!   `is_built_in = false` or non-NULL `owner_tenant_id` (tamper
//!   detection).

use chrono::Utc;
use sea_orm::sea_query::OnConflict;
use sea_orm::{ActiveValue::Set, EntityTrait};
use serde_json::Value as JsonValue;
use toolkit_db::secure::{
    AccessScope, DBRunner, ScopeError, SecureEntityExt, SecureInsertExt, is_unique_violation,
};
use tracing::{debug, info};

use rbac_sdk::error::RbacServiceError;

use crate::config::BuiltinRoleTargets;
use crate::domain::builtin_roles_catalog as catalog;
use crate::domain::builtin_roles_catalog::{CanonicalBuiltinRole, SYSTEM_CREATED_BY};
use crate::infra::error_conv::redacted_scope_error;
use crate::infra::storage::entity::role_definition;

// Partial-unique index from `m20260521_000001_create_role_definitions_table`:
// `(name) WHERE owner_tenant_id IS NULL`. Used to disambiguate the
// concurrent-seeder race condition from genuine schema violations.
const UQ_ROLE_NAME_BUILTIN: &str = "uq_role_name_builtin";

/// Idempotent seeder for the platform's built-in roles.
///
/// `pub` so concurrency integration tests can invoke the production
/// seeder; downstream consumers SHOULD NOT depend on it directly — the
/// module's public contract is `dyn RbacServiceClientV1` in `ClientHub`.
#[derive(Debug, Default)]
pub struct BuiltinRoleSeeder;

impl BuiltinRoleSeeder {
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// Number of built-in roles this seeder upserts for the given
    /// integration-role setting.
    ///
    /// The three roster queries below are thin delegates to
    /// [`crate::domain::builtin_roles_catalog`], which owns them: the catalog is
    /// the domain fact, and `RbacServiceConfig::validate` has to read it without
    /// importing this module. They stay `pub` here so integration tests under
    /// `tests/` can assert against the roster without hardcoding a literal that
    /// silently rots the next time a role is added.
    #[must_use]
    pub fn role_count(include_integration: bool) -> usize {
        catalog::role_count(include_integration)
    }

    /// Names of the built-in roles this seeder upserts, in catalog order
    /// (ascending `id` — the seeder's lock-ordering invariant).
    #[must_use]
    pub fn role_names(include_integration: bool) -> Vec<&'static str> {
        catalog::role_names(include_integration)
    }

    /// `id` of the built-in role named `name`, when this seeder would seed it.
    #[must_use]
    pub fn role_id_by_name(name: &str, include_integration: bool) -> Option<uuid::Uuid> {
        catalog::role_id_by_name(name, include_integration)
    }

    /// Seed every canonical built-in role into `role_definitions` and verify
    /// the post-upsert invariants. Idempotent.
    ///
    /// # Errors
    ///
    /// * [`RbacServiceError::Internal`] if a `SeaORM` query errors, or if any
    ///   seeded row reports `is_built_in = false` / non-NULL `owner_tenant_id`.
    pub async fn seed(
        &self,
        runner: &impl DBRunner,
        include_integration: bool,
        targets: &BuiltinRoleTargets,
    ) -> Result<(), RbacServiceError> {
        let now = Utc::now();
        // The caller wraps this loop in a transaction, so the ascending-id
        // ordering is load-bearing: two concurrent seeders take the same
        // row locks in the same order and cannot deadlock. The invariant is
        // enforced at compile time by
        // `_ASSERT_BUILTIN_ROLES_SORTED_BY_ID` in
        // `domain::service::builtin_roles_catalog`; a reorder there
        // fails `cargo build` rather than silently deadlocking in prod.
        let mut seeded = 0_usize;
        for role in catalog::roster(include_integration) {
            self.upsert_role(runner, role, now, targets).await?;
            self.verify_seeded_invariants(runner, role).await?;
            seeded += 1;
        }

        info!(
            seeded,
            include_integration, "rbac: seeded built-in roles (ascending-id order)"
        );
        Ok(())
    }

    /// Issue a single `INSERT … ON CONFLICT (id) DO UPDATE` for one role.
    ///
    /// `is_built_in` and `owner_tenant_id` are intentionally absent from the
    /// `update_columns` list so a tampered row is detected by the next
    /// verification pass instead of silently healed back to `true` / `NULL`.
    ///
    /// `scope_unchecked(&AccessScope::allow_all())` is the documented escape
    /// hatch for system-level writes; `on_conflict_raw` is required because
    /// the `unrestricted` profile has no tenant column for `SecureOnConflict`.
    async fn upsert_role(
        &self,
        runner: &impl DBRunner,
        role: &CanonicalBuiltinRole,
        now: chrono::DateTime<Utc>,
        targets: &BuiltinRoleTargets,
    ) -> Result<(), RbacServiceError> {
        let active = build_role_definition_active_model(role, now, targets);

        let on_conflict = OnConflict::column(role_definition::Column::Id)
            .update_columns([
                role_definition::Column::Name,
                role_definition::Column::Description,
                role_definition::Column::Permissions,
                role_definition::Column::NotPermissions,
                role_definition::Column::AssignableScopes,
                role_definition::Column::UpdatedAt,
            ])
            .to_owned();

        let result = role_definition::Entity::insert(active)
            .secure()
            .scope_unchecked(&AccessScope::allow_all())
            .map_err(|err| {
                RbacServiceError::internal(format!(
                    "rbac: built-in role seeder failed to apply allow-all scope for {} ({}): {}",
                    role.name,
                    role.id,
                    redacted_scope_error(&err)
                ))
            })?
            .on_conflict_raw(on_conflict)
            .exec(runner)
            .await;

        match result {
            Ok(_) => Ok(()),
            // A concurrent seeder may win the race on `uq_role_name_builtin`.
            // PG's `ON CONFLICT (id)` arbiter only catches PK conflicts;
            // violations on other unique indexes are raised. Both seeders
            // insert the same canonical (id, name), so this is idempotent
            // success — `verify_seeded_invariants` confirms.
            Err(ScopeError::Db(db_err))
                if is_unique_violation(&db_err)
                    && db_err.to_string().contains(UQ_ROLE_NAME_BUILTIN) =>
            {
                debug!(
                    role = role.name,
                    id = %role.id,
                    "rbac: built-in role already inserted by a concurrent seeder; skipping upsert",
                );
                Ok(())
            }
            Err(err) => Err(RbacServiceError::internal(format!(
                "rbac: built-in role seeder failed to upsert {} ({}): {}",
                role.name,
                role.id,
                redacted_scope_error(&err)
            ))),
        }
    }

    /// Read each upserted row back and confirm the built-in invariants.
    /// An external write that flips `is_built_in` or sets `owner_tenant_id`
    /// surfaces as a startup error here, not silently rewritten.
    async fn verify_seeded_invariants(
        &self,
        runner: &impl DBRunner,
        role: &CanonicalBuiltinRole,
    ) -> Result<(), RbacServiceError> {
        let row = role_definition::Entity::find_by_id(role.id)
            .secure()
            .scope_with(&AccessScope::allow_all())
            .one(runner)
            .await
            .map_err(|err| {
                RbacServiceError::internal(format!(
                    "rbac: failed to verify built-in role {} ({}) post-upsert: {}",
                    role.name,
                    role.id,
                    redacted_scope_error(&err)
                ))
            })?
            .ok_or_else(|| {
                RbacServiceError::internal(format!(
                    "rbac: built-in role {} ({}) is missing immediately \
                     after a successful upsert — concurrent delete or DB \
                     replication anomaly",
                    role.name, role.id
                ))
            })?;

        if !row.is_built_in {
            return Err(RbacServiceError::internal(format!(
                "rbac: built-in invariant violated — role {} ({}) has \
                 is_built_in = false after seeding (likely external tamper)",
                role.name, role.id
            )));
        }
        if row.owner_tenant_id.is_some() {
            return Err(RbacServiceError::internal(format!(
                "rbac: built-in invariant violated — role {} ({}) has \
                 a non-NULL owner_tenant_id ({:?}) after seeding (likely \
                 external tamper)",
                role.name, role.id, row.owner_tenant_id
            )));
        }
        Ok(())
    }
}

/// Build the `ActiveModel` for a single canonical built-in role.
///
/// Every field MUST stay aligned with the migrations + built-in invariants
/// (`is_built_in = true`, `owner_tenant_id = NULL`, `not_permissions = []`,
/// `created_by = "system"`).
pub(crate) fn build_role_definition_active_model(
    role: &CanonicalBuiltinRole,
    now: chrono::DateTime<Utc>,
    targets: &BuiltinRoleTargets,
) -> role_definition::ActiveModel {
    role_definition::ActiveModel {
        id: Set(role.id),
        name: Set(role.name.to_owned()),
        description: Set(Some(role.description.to_owned())),
        is_built_in: Set(true),
        permissions: Set(role_permissions_json(role, targets)),
        not_permissions: Set(JsonValue::Array(Vec::new())),
        assignable_scopes: Set(role_assignable_scopes_json(role)),
        owner_tenant_id: Set(None),
        created_at: Set(now),
        updated_at: Set(now),
        created_by: Set(SYSTEM_CREATED_BY.to_owned()),
    }
}

/// Serialise the role's `permission_rules` slice as
/// `[{"operation": "...", "target_type": "..."}, …]`.
///
/// A rule over a configurable slot expands into one JSON rule per configured
/// target, so `platform: ["gts.cf.*", "gts.vendor.*"]` gives `Owner` two rules
/// rather than one. Rule order follows the catalog, then the config list.
pub(crate) fn role_permissions_json(
    role: &CanonicalBuiltinRole,
    targets: &BuiltinRoleTargets,
) -> JsonValue {
    JsonValue::Array(
        role.permission_rules
            .iter()
            .flat_map(|rule| {
                targets
                    .resolve(rule.target)
                    .into_iter()
                    .map(|target| {
                        serde_json::json!({
                            "operation": rule.operation,
                            "target_type": target,
                        })
                    })
                    .collect::<Vec<_>>()
            })
            .collect(),
    )
}

/// Serialise the role's `assignable_scopes` slice as a JSONB string array.
pub(crate) fn role_assignable_scopes_json(role: &CanonicalBuiltinRole) -> JsonValue {
    JsonValue::Array(
        role.assignable_scopes
            .iter()
            .map(|scope| JsonValue::String((*scope).to_owned()))
            .collect(),
    )
}

#[cfg(test)]
#[path = "seeder_tests.rs"]
mod seeder_tests;
