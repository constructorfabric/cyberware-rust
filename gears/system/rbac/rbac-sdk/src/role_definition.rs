//! `RoleDefinition` — RBAC role: name, allow / deny rules, and the
//! scopes at which it can be assigned.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::permission_rule::PermissionRule;
use crate::scope::Scope;

/// RBAC role definition: ties a set of `PermissionRule`s to a name and the
/// scopes where the role can be assigned.
///
/// Built-in roles use fixed UUIDs, carry `is_built_in = true`, and have
/// `owner_tenant_id = None`.
///
/// Permission rules are split into `permissions` (Allow) and
/// `not_permissions` (Deny). The matcher applies `Deny > Allow` precedence:
/// any match in `not_permissions` short-circuits the role; otherwise the
/// first match in `permissions` records the grant.
///
/// `#[non_exhaustive]` — construct via [`RoleDefinition::new`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RoleDefinition {
    /// Stable identifier (`UUIDv7`).
    pub id: Uuid,
    /// Human-readable role name. Capped at 256 chars by JSON Schema / DB column.
    pub name: String,
    /// Optional description, capped at 4096 chars.
    pub description: Option<String>,
    /// `true` for platform-seeded built-in roles, which are immutable.
    pub is_built_in: bool,
    /// Allow rules contributed by this role.
    pub permissions: Vec<PermissionRule>,
    /// Deny rules. A match here short-circuits the role and surfaces as
    /// [`crate::subject_role::DenyReason::NotPermissionExclusion`] when no other role granted the
    /// request.
    pub not_permissions: Vec<PermissionRule>,
    /// Scopes where the role can be assigned. Non-empty by DB CHECK and
    /// application invariant.
    ///
    /// Typed rather than `Vec<String>`: the legal forms are the [`Scope`]
    /// variants, so an unparseable value is not representable here and no
    /// consumer has to re-parse. The wire form is unchanged — [`Scope`]
    /// serializes as its canonical path string (`/`, `/tenants/{id}`,
    /// `/tenants/{id}/resourceGroups/{id}`) and parses back from one.
    pub assignable_scopes: Vec<Scope>,
    /// Owning tenant for custom roles; `None` is reserved for built-ins (DB
    /// CHECK enforces the bi-conditional).
    pub owner_tenant_id: Option<Uuid>,
    /// Creation timestamp, serialised as UTC.
    pub created_at: DateTime<Utc>,
    /// Last modification timestamp, serialised as UTC.
    pub updated_at: DateTime<Utc>,
    /// Creator subject ID (used for audit). Built-ins use `"system"`.
    pub created_by: String,
    /// How many role assignments reference this definition, counted over
    /// **only the assignments the caller may read** — never platform-wide.
    /// The number therefore equals what the caller would get by paging
    /// `GET /rbac/v1/role-assignments?$filter=role_definition_id eq <id>`
    /// themselves.
    ///
    /// `None` and `Some(0)` are different answers. `None` means no count
    /// exists for this caller: they have no read visibility on role
    /// assignments anywhere (a zero would then be a fact about their
    /// permissions, which a UI would render as "this role is unused"), or the
    /// response came from a write path, which performs no count. `Some(0)`
    /// means the caller can see assignments and none of them use this role.
    ///
    /// Display-only and never persisted: not filterable, not orderable, and
    /// rejected in request bodies. It is also not transactionally consistent
    /// with the row it accompanies — assignments can appear or vanish between
    /// the two queries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assignment_count: Option<u64>,
}

impl RoleDefinition {
    /// Construct a [`RoleDefinition`] from its currently-required fields.
    /// Stable across `#[non_exhaustive]` field additions.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: Uuid,
        name: impl Into<String>,
        description: Option<String>,
        is_built_in: bool,
        permissions: Vec<PermissionRule>,
        not_permissions: Vec<PermissionRule>,
        assignable_scopes: Vec<Scope>,
        owner_tenant_id: Option<Uuid>,
        created_at: DateTime<Utc>,
        updated_at: DateTime<Utc>,
        created_by: impl Into<String>,
    ) -> Self {
        Self {
            id,
            name: name.into(),
            description,
            is_built_in,
            permissions,
            not_permissions,
            assignable_scopes,
            owner_tenant_id,
            created_at,
            updated_at,
            created_by: created_by.into(),
            // The count is a read-path projection over a different table,
            // never part of the row: construction can only leave it unset,
            // and the read path attaches it through the setter below.
            assignment_count: None,
        }
    }

    /// Attach the caller-visibility-bounded assignment count. Chainable;
    /// `None` clears it (and means "no count for this caller", never zero).
    #[must_use]
    pub fn with_assignment_count(mut self, count: Option<u64>) -> Self {
        self.assignment_count = count;
        self
    }
}

#[cfg(test)]
#[path = "role_definition_tests.rs"]
mod role_definition_tests;
