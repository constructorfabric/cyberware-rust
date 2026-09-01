//! Domain-internal projection of a `role_definitions` row.
//!
//! The `rbac_sdk::models::RoleDefinition` SDK aggregate is the public
//! wire shape; this module's [`RoleDefinitionModel`] is the in-memory
//! domain shape that flows between the repo and the service layer. The
//! two diverge in one place today (`assignable_scopes` is typed here,
//! string-formed in the SDK) and are free to diverge further without
//! breaking the SDK contract.
//!
//! Lowering to the SDK happens in a single place — see
//! [`crate::api::service::lowering`].

use chrono::{DateTime, Utc};
use rbac_sdk::models::{PermissionRule, Scope};
use toolkit_macros::domain_model;
use uuid::Uuid;

/// Domain representation of one `role_definitions` row.
///
/// Mirrors the SDK [`rbac_sdk::models::RoleDefinition`] aggregate except
/// `assignable_scopes` is parsed into typed [`Scope`] values at the
/// storage boundary — the same way the canonical tenant feature lifts
/// `i16` to `TenantStatus` inside `entity_to_model`.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleDefinitionModel {
    pub id: Uuid,
    pub name: String,
    pub description: Option<String>,
    pub is_built_in: bool,
    /// Allow rules.
    pub permissions: Vec<PermissionRule>,
    /// Deny rules.
    pub not_permissions: Vec<PermissionRule>,
    /// Scopes where the role can be assigned, parsed from the stored
    /// path strings.
    pub assignable_scopes: Vec<Scope>,
    /// Owner tenant for custom roles; `None` only for built-ins (DB
    /// CHECK enforces the bi-conditional).
    pub owner_tenant_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub created_by: String,
}
