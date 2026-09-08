//! `SeaORM` entity for the `role_definitions` table.
//!
//! `#[secure(unrestricted)]` marks the entity global / system-level so
//! the seeder's `SecureInsertOne` chain becomes a passthrough; tenant
//! filtering for custom-row CRUD is enforced by
//! `RoleDefinitionRepository`, not by `toolkit-db`.

use chrono::{DateTime, Utc};
use sea_orm::entity::prelude::*;
use serde_json::Value as JsonValue;
use toolkit_db_macros::Scopable;
use uuid::Uuid;

// `Model` is `pub` because `DeriveEntityModel` generates `pub` items
// referencing it; effective visibility is `pub(crate)` via `infra/storage.rs`.
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
#[sea_orm(table_name = "role_definitions")]
#[secure(unrestricted)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: Uuid,
    pub name: String,
    pub description: Option<String>,
    pub is_built_in: bool,
    /// JSONB array of `{ operation, target_type }` rules.
    pub permissions: JsonValue,
    /// JSONB array of subtractive `{ operation, target_type }` rules.
    pub not_permissions: JsonValue,
    /// JSONB array of scope strings.
    pub assignable_scopes: JsonValue,
    pub owner_tenant_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub created_by: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}

// ---------------------------------------------------------------------------
// Domain mapping — kept next to the entity so it stays in lockstep with
// column-shape changes.
// ---------------------------------------------------------------------------

/// Errors that can surface during row → domain mapping. Callers MUST
/// map either variant to `RoleDefinitionRepoError::Internal` — both
/// indicate a corrupted-row condition (`ScopeValidator` rejects malformed
/// scopes on write, so reaching `InvalidAssignableScope` signals
/// defence-in-depth).
#[derive(Debug, thiserror::Error)]
pub enum RoleDefinitionMappingError {
    #[error("failed to decode JSONB column '{column}': {source}")]
    JsonbDecode {
        column: &'static str,
        #[source]
        source: serde_json::Error,
    },
    #[error("assignable_scopes: invalid stored path {raw:?}: {source}")]
    InvalidAssignableScope {
        raw: String,
        #[source]
        source: rbac_sdk::models::ScopeParseError,
    },
}

/// JSONB row shape inside the `permissions` / `not_permissions` columns.
/// The column itself encodes the rule's effect — no per-rule tag.
#[derive(serde::Deserialize)]
struct StoredRule {
    operation: String,
    target_type: String,
}

impl StoredRule {
    fn into_permission_rule(self) -> rbac_sdk::models::PermissionRule {
        rbac_sdk::models::PermissionRule::new(self.operation, self.target_type)
    }
}

/// Convert a `SeaORM` `Model` row into the domain
/// [`crate::domain::model::RoleDefinitionModel`]. Per-column JSONB order is
/// preserved; `assignable_scopes` is parsed into typed
/// [`rbac_sdk::models::Scope`] values at this
/// boundary (mirrors how the canonical tenant feature parses status &
/// depth in `entity_to_model`).
pub fn entity_to_model(
    mut row: Model,
) -> Result<crate::domain::model::RoleDefinitionModel, RoleDefinitionMappingError> {
    let allow_rows: Vec<StoredRule> = serde_json::from_value(std::mem::take(&mut row.permissions))
        .map_err(|source| RoleDefinitionMappingError::JsonbDecode {
            column: "permissions",
            source,
        })?;
    let deny_rows: Vec<StoredRule> =
        serde_json::from_value(std::mem::take(&mut row.not_permissions)).map_err(|source| {
            RoleDefinitionMappingError::JsonbDecode {
                column: "not_permissions",
                source,
            }
        })?;
    let scope_strings: Vec<String> =
        serde_json::from_value(std::mem::take(&mut row.assignable_scopes)).map_err(|source| {
            RoleDefinitionMappingError::JsonbDecode {
                column: "assignable_scopes",
                source,
            }
        })?;

    let permissions: Vec<rbac_sdk::models::PermissionRule> = allow_rows
        .into_iter()
        .map(StoredRule::into_permission_rule)
        .collect();
    let not_permissions: Vec<rbac_sdk::models::PermissionRule> = deny_rows
        .into_iter()
        .map(StoredRule::into_permission_rule)
        .collect();
    let assignable_scopes: Vec<rbac_sdk::models::Scope> = scope_strings
        .into_iter()
        .map(|raw| {
            rbac_sdk::models::Scope::parse(&raw).map_err(|source| {
                RoleDefinitionMappingError::InvalidAssignableScope { raw, source }
            })
        })
        .collect::<Result<_, _>>()?;

    Ok(crate::domain::model::RoleDefinitionModel {
        id: row.id,
        name: row.name,
        description: row.description,
        is_built_in: row.is_built_in,
        permissions,
        not_permissions,
        assignable_scopes,
        owner_tenant_id: row.owner_tenant_id,
        created_at: row.created_at,
        updated_at: row.updated_at,
        created_by: row.created_by,
    })
}
