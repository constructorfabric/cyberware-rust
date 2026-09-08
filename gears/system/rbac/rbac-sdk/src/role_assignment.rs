//! `RoleAssignment` — `(principal, role, scope)` triplet binding +
//! the closed `PrincipalType` enum.

use std::fmt;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::scope::Scope;

/// Triplet binding a `RoleDefinition` to a principal at a scope.
///
/// `#[non_exhaustive]` — construct via [`RoleAssignment::new`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RoleAssignment {
    /// Stable identifier (`UUIDv7`).
    pub id: Uuid,
    /// FK → `role_definitions.id`. DB enforces `ON DELETE RESTRICT`.
    pub role_definition_id: Uuid,
    /// Principal receiving the role.
    pub principal_id: String,
    /// Discriminates user / group / service-principal semantics.
    pub principal_type: PrincipalType,
    /// Scope at which the role is granted. Permissions inherit to child scopes.
    pub scope: Scope,
    /// Creation timestamp, serialised as UTC.
    pub created_at: DateTime<Utc>,
    /// Last modification timestamp. `role_assignments` is create-and-delete
    /// only in v1, so this equals `created_at` for any persisted row.
    pub updated_at: DateTime<Utc>,
    /// Creator subject ID (used for audit).
    pub created_by: String,
    /// Display name of the principal, resolved on the read path by the
    /// RBAC gear (users via account management, groups via the
    /// resource-group module). `None` means "not resolved" — a deleted
    /// principal, a `ServicePrincipal` (no reverse lookup exists), a
    /// principal outside the lookup tenant, a caller without user-read,
    /// or an unavailable upstream. Never persisted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub principal_name: Option<String>,
    /// Display name of the author. Same resolution and same `None`
    /// semantics as [`Self::principal_name`]; additionally `None` for a
    /// machine author and for rows created before the author's kind was
    /// stored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_by_name: Option<String>,
    /// Display name of the granted role definition, resolved on the read
    /// path from RBAC's own `role_definitions` table. `None` means "not
    /// resolved": the referenced definition was deleted inside the
    /// FK-restrict race window, or the local read failed. Cheaper than the
    /// two principal names — no upstream gear is involved — but reported
    /// with the same semantics so a consumer has one rule for all three.
    /// Never persisted on the assignment row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role_definition_name: Option<String>,
}

impl RoleAssignment {
    /// Construct a [`RoleAssignment`] from its currently-required fields.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: Uuid,
        role_definition_id: Uuid,
        principal_id: impl Into<String>,
        principal_type: PrincipalType,
        scope: Scope,
        created_at: DateTime<Utc>,
        updated_at: DateTime<Utc>,
        created_by: impl Into<String>,
    ) -> Self {
        Self {
            id,
            role_definition_id,
            principal_id: principal_id.into(),
            principal_type,
            scope,
            created_at,
            updated_at,
            created_by: created_by.into(),
            // Display names are a read-path projection, never part of
            // the row: construction can only ever leave them unset, and
            // the read path attaches them through the setters below.
            principal_name: None,
            created_by_name: None,
            role_definition_name: None,
        }
    }

    /// Attach the resolved principal display name. Chainable; `None`
    /// clears it.
    #[must_use]
    pub fn with_principal_name(mut self, name: Option<String>) -> Self {
        self.principal_name = name;
        self
    }

    /// Attach the resolved author display name. Chainable; `None` clears
    /// it.
    #[must_use]
    pub fn with_created_by_name(mut self, name: Option<String>) -> Self {
        self.created_by_name = name;
        self
    }

    /// Attach the resolved role-definition display name. Chainable;
    /// `None` clears it.
    #[must_use]
    pub fn with_role_definition_name(mut self, name: Option<String>) -> Self {
        self.role_definition_name = name;
        self
    }
}

/// Principal kinds that can hold a role assignment.
///
/// Stored as the short tag (`User` / `Group` / `ServicePrincipal`) on the
/// wire and in the DB. `#[non_exhaustive]` — external `match` arms MUST end
/// with a wildcard `_ =>` arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub enum PrincipalType {
    /// Human user identity.
    User,
    /// User group managed by the resource-group module (tenant-scoped).
    /// RG-backed existence is required for assignments.
    Group,
    /// Machine / service identity.
    ServicePrincipal,
}

impl PrincipalType {
    /// Canonical wire / DB string tag for this variant.
    ///
    /// The match is intentionally exhaustive — adding a new variant fails
    /// the SDK build here, forcing a deliberate decision about the wire tag.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            PrincipalType::User => "User",
            PrincipalType::Group => "Group",
            PrincipalType::ServicePrincipal => "ServicePrincipal",
        }
    }
}

impl fmt::Display for PrincipalType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str((*self).as_str())
    }
}

/// Error returned by `PrincipalType::from_str` when the input does not
/// match a known variant. Carries the offending value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownPrincipalType(pub String);

impl fmt::Display for UnknownPrincipalType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unknown principal type: '{}'", self.0.escape_debug())
    }
}

impl std::error::Error for UnknownPrincipalType {}

impl std::str::FromStr for PrincipalType {
    type Err = UnknownPrincipalType;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "User" => Ok(Self::User),
            "Group" => Ok(Self::Group),
            "ServicePrincipal" => Ok(Self::ServicePrincipal),
            other => Err(UnknownPrincipalType(other.to_owned())),
        }
    }
}

#[cfg(test)]
#[path = "role_assignment_tests.rs"]
mod role_assignment_tests;
