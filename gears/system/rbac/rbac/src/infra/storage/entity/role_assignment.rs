//! `SeaORM` entity for the `role_assignments` table
//! (`m20260521_000002_create_role_assignments_table`, extended by
//! `m20260824_000003_add_role_assignment_author_identity`).
//!
//! `#[secure(unrestricted)]` marks this as a global / system-level entity:
//! the bootstrap writes a root-scope row and tenant filtering is enforced
//! by repository-side scopes, not by `toolkit-db`.
//!
//! `scope_depth` and `tenant_id` are populated by the application at
//! insert time from the parsed [`rbac_sdk::models::Scope`] via
//! [`rbac_sdk::models::Scope::depth`] and
//! [`rbac_sdk::models::Scope::tenant_id`]. `tenant_id` is `NULL` for the
//! root-scoped bootstrap row and the tenant UUID for every other shape,
//! and backs index-driven `WHERE tenant_id = $1` lookups on the read path.

use chrono::{DateTime, Utc};
use sea_orm::entity::prelude::*;
use toolkit_db_macros::Scopable;
use uuid::Uuid;

// `Model` is `pub` because `DeriveEntityModel` generates `pub` items
// referencing it; effective visibility is `pub(crate)` via `infra/storage.rs`.
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
#[sea_orm(table_name = "role_assignments")]
#[secure(unrestricted)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: Uuid,
    pub role_definition_id: Uuid,
    /// Opaque principal identifier from the upstream identity provider.
    pub principal_id: String,
    /// Closed-enum string: `"User"`, `"Group"`, or `"ServicePrincipal"`.
    /// Enforced at the application layer via the `PrincipalType` SDK enum;
    /// the column is `text NOT NULL` with no DB-level CHECK.
    pub principal_type: String,
    /// Hierarchical scope string (e.g. `"/"`, `"/tenants/abc"`, …).
    pub scope: String,
    /// Depth proxy for deepest-first ordering; equals
    /// [`rbac_sdk::models::Scope::depth`] of the parsed `scope`. Written by
    /// the application at insert time.
    #[sea_orm(column_name = "scope_depth")]
    pub scope_depth: i32,
    /// Tenant UUID extracted from `scope`; equals
    /// [`rbac_sdk::models::Scope::tenant_id`] of the parsed `scope`. `None`
    /// for the root-scoped bootstrap row. Written by the application at
    /// insert time.
    #[sea_orm(column_name = "tenant_id")]
    pub tenant_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub created_by: String,
    /// Kind of the principal named by `created_by`, as the same closed-enum
    /// string the `principal_type` column uses (`"User"`, `"Group"`,
    /// `"ServicePrincipal"`). `NULL` means "author identity not recorded":
    /// a row written before
    /// `m20260824_000003_add_role_assignment_author_identity`, or a machine
    /// author with no user identity to record. Stamped from the caller's
    /// `SecurityContext` at insert time and never updated — the author of a
    /// row does not change.
    #[sea_orm(column_name = "created_by_type")]
    pub created_by_type: Option<String>,
    /// Home tenant of the principal named by `created_by`, i.e. the tenant a
    /// reader must ask to resolve that subject id to a display name. `NULL`
    /// under exactly the conditions described for `created_by_type`.
    #[sea_orm(column_name = "created_by_tenant_id")]
    pub created_by_tenant_id: Option<Uuid>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    /// FK to `role_definitions(id) ON DELETE RESTRICT` (initial migration).
    #[sea_orm(
        belongs_to = "super::role_definition::Entity",
        from = "Column::RoleDefinitionId",
        to = "super::role_definition::Column::Id",
        on_update = "NoAction",
        on_delete = "Restrict"
    )]
    RoleDefinition,
}

impl Related<super::role_definition::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::RoleDefinition.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}

// ---------------------------------------------------------------------------
// Domain mapping — kept next to the entity so it stays in lockstep with
// column-shape changes.
// ---------------------------------------------------------------------------

/// Errors that can surface during row → domain mapping. All variants indicate
/// a corrupted-row condition — `ScopeValidator` rejects malformed scopes on
/// write, the `principal_type` column is constrained at the application layer,
/// and canonical scope paths must agree with their denormalized query columns.
#[derive(Debug, thiserror::Error)]
pub enum RoleAssignmentMappingError {
    #[error("principal_type: {0}")]
    InvalidPrincipalType(rbac_sdk::models::UnknownPrincipalType),
    #[error("scope: invalid stored path {raw:?}: {source}")]
    InvalidScope {
        raw: String,
        #[source]
        source: rbac_sdk::models::ScopeParseError,
    },
    /// The query-index columns disagree with the canonical scope string. This
    /// can make a root assignment look tenant-local to the candidate query, so
    /// it must be rejected before permission evaluation.
    #[error("scope path and denormalized query projection disagree")]
    InconsistentScopeProjection {
        raw: String,
        scope_depth: i32,
        expected_scope_depth: i32,
        tenant_id: Option<Uuid>,
        expected_tenant_id: Option<Uuid>,
    },
}

/// Convert a `SeaORM` `Model` row into the domain
/// [`crate::domain::model::RoleAssignmentModel`].
pub fn entity_to_model(
    row: Model,
) -> Result<crate::domain::model::RoleAssignmentModel, RoleAssignmentMappingError> {
    use std::str::FromStr as _;
    let principal_type = rbac_sdk::models::PrincipalType::from_str(&row.principal_type)
        .map_err(RoleAssignmentMappingError::InvalidPrincipalType)?;
    let scope = rbac_sdk::models::Scope::parse(&row.scope).map_err(|source| {
        RoleAssignmentMappingError::InvalidScope {
            raw: row.scope.clone(),
            source,
        }
    })?;
    let expected_scope_depth = scope.depth();
    let expected_tenant_id = scope.tenant_id();
    if row.scope_depth != expected_scope_depth || row.tenant_id != expected_tenant_id {
        return Err(RoleAssignmentMappingError::InconsistentScopeProjection {
            raw: row.scope,
            scope_depth: row.scope_depth,
            expected_scope_depth,
            tenant_id: row.tenant_id,
            expected_tenant_id,
        });
    }
    // The author's kind is parsed leniently, unlike `principal_type` above:
    // an unrecognised tag reads as "no author identity" instead of failing
    // the row. That keeps an older node able to serve rows a newer one
    // wrote with a principal kind it has never heard of — the author's
    // *name* is a display convenience, and losing it must never cost a
    // caller their list. `principal_type` cannot be lenient the same way:
    // it participates in authorization.
    let created_by_type = row
        .created_by_type
        .as_deref()
        .and_then(|raw| raw.parse::<rbac_sdk::models::PrincipalType>().ok());
    Ok(crate::domain::model::RoleAssignmentModel {
        id: row.id,
        role_definition_id: row.role_definition_id,
        principal_id: row.principal_id,
        principal_type,
        scope,
        created_at: row.created_at,
        updated_at: row.updated_at,
        created_by: row.created_by,
        created_by_type,
        created_by_tenant_id: row.created_by_tenant_id,
    })
}

#[cfg(test)]
#[path = "role_assignment_tests.rs"]
mod entity_test;
