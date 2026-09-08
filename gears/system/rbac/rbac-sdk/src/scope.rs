//! Hierarchical RBAC scope: `Scope` enum + parse-error surface.

use std::fmt;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Hierarchical scope at which a permission grant applies.
///
/// Wire format is the textual path (`/`, `/tenants/{id}`,
/// `/tenants/{id}/resourceGroups/{id}`); round-trips losslessly through
/// [`Scope::path`] / [`Scope::parse`].
///
/// `#[non_exhaustive]` — external `match` arms MUST end with `_ =>`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Scope {
    /// Synthetic root scope `/` — ancestor of every other scope.
    Root,
    /// Tenant scope `/tenants/{tenant_id}`.
    Tenant {
        /// UUID identifying the tenant within the tenant hierarchy.
        tenant_id: Uuid,
    },
    /// Resource-group scope `/tenants/{tenant_id}/resourceGroups/{group_id}`.
    ///
    /// `tenant_id` is the *claimed* owner expressed in the path string; an
    /// upstream validator verifies the group's actual owner matches.
    ResourceGroup {
        /// Claimed tenant UUID in the path.
        tenant_id: Uuid,
        /// Resource-group UUID.
        group_id: Uuid,
    },
}

impl Scope {
    /// Constructor for [`Scope::Root`].
    #[must_use]
    pub const fn root() -> Self {
        Scope::Root
    }

    /// Constructor for [`Scope::Tenant`].
    #[must_use]
    pub const fn tenant(tenant_id: Uuid) -> Self {
        Scope::Tenant { tenant_id }
    }

    /// Constructor for [`Scope::ResourceGroup`].
    #[must_use]
    pub const fn resource_group(tenant_id: Uuid, group_id: Uuid) -> Self {
        Scope::ResourceGroup {
            tenant_id,
            group_id,
        }
    }

    /// Parse a scope from its canonical path form.
    ///
    /// Accepts exactly the three shapes produced by [`Self::path`]:
    /// * `"/"` → [`Scope::Root`]
    /// * `"/tenants/{uuid}"` → [`Scope::Tenant`]
    /// * `"/tenants/{uuid}/resourceGroups/{uuid}"` → [`Scope::ResourceGroup`]
    ///
    /// # Errors
    ///
    /// Returns [`ScopeParseError`] when the input does not match one of
    /// the three canonical shapes — bad prefix, missing segment, or a
    /// UUID-shaped segment that fails [`Uuid::parse_str`].
    pub fn parse(input: &str) -> Result<Self, ScopeParseError> {
        if input == "/" {
            return Ok(Scope::Root);
        }
        let rest = input
            .strip_prefix("/tenants/")
            .ok_or_else(|| ScopeParseError::InvalidFormat(input.to_owned()))?;
        if let Some((tenant_part, after_tenant)) = rest.split_once('/') {
            let tenant_id = Uuid::parse_str(tenant_part)
                .map_err(|_| ScopeParseError::InvalidTenantUuid(tenant_part.to_owned()))?;
            let group_part = after_tenant
                .strip_prefix("resourceGroups/")
                .ok_or_else(|| ScopeParseError::InvalidFormat(input.to_owned()))?;
            if group_part.contains('/') {
                return Err(ScopeParseError::InvalidFormat(input.to_owned()));
            }
            let group_id = Uuid::parse_str(group_part)
                .map_err(|_| ScopeParseError::InvalidResourceGroupUuid(group_part.to_owned()))?;
            Ok(Scope::ResourceGroup {
                tenant_id,
                group_id,
            })
        } else {
            let tenant_id = Uuid::parse_str(rest)
                .map_err(|_| ScopeParseError::InvalidTenantUuid(rest.to_owned()))?;
            Ok(Scope::Tenant { tenant_id })
        }
    }

    /// Canonical path-form representation. Round-trips with [`Self::parse`].
    #[must_use]
    pub fn path(&self) -> String {
        match self {
            Scope::Root => "/".to_owned(),
            Scope::Tenant { tenant_id } => format!("/tenants/{tenant_id}"),
            Scope::ResourceGroup {
                tenant_id,
                group_id,
            } => format!("/tenants/{tenant_id}/resourceGroups/{group_id}"),
        }
    }

    /// Tenant UUID associated with this scope, if any.
    /// [`Scope::Root`] has no tenant.
    #[must_use]
    pub fn tenant_id(&self) -> Option<Uuid> {
        match self {
            Scope::Root => None,
            Scope::Tenant { tenant_id } | Scope::ResourceGroup { tenant_id, .. } => {
                Some(*tenant_id)
            }
        }
    }

    /// Depth proxy: count of `/` separators in the canonical path. Used as
    /// the sort key for deepest-first ordering in the assignments evaluator;
    /// values are monotonically increasing with real hierarchy depth and
    /// independent of segment-id width.
    ///
    /// * [`Scope::Root`] → `1` (just `/`).
    /// * [`Scope::Tenant`] → `2` (`/tenants/{id}`).
    /// * [`Scope::ResourceGroup`] → `4` (`/tenants/{id}/resourceGroups/{id}`).
    #[must_use]
    pub fn depth(&self) -> i32 {
        match self {
            Scope::Root => 1,
            Scope::Tenant { .. } => 2,
            Scope::ResourceGroup { .. } => 4,
        }
    }

    /// `true` when `self` is an ancestor of (or equal to) `descendant`
    /// in the scope hierarchy.
    ///
    /// Examples (with `T1`, `T2` distinct UUIDs):
    /// * `Root.is_ancestor_of(_)` → `true` for every scope.
    /// * `Tenant{T1}.is_ancestor_of(Tenant{T1})` → `true`.
    /// * `Tenant{T1}.is_ancestor_of(ResourceGroup{T1, _})` → `true`.
    /// * `Tenant{T1}.is_ancestor_of(Tenant{T2})` → `false`.
    /// * `ResourceGroup{T1, G}.is_ancestor_of(Tenant{T1})` → `false`.
    #[must_use]
    pub fn is_ancestor_of(&self, descendant: &Scope) -> bool {
        match (self, descendant) {
            (Scope::Root, _) => true,
            (
                Scope::Tenant { tenant_id: a },
                Scope::Tenant { tenant_id: b } | Scope::ResourceGroup { tenant_id: b, .. },
            ) => a == b,
            (
                Scope::ResourceGroup {
                    tenant_id: a_t,
                    group_id: a_g,
                },
                Scope::ResourceGroup {
                    tenant_id: b_t,
                    group_id: b_g,
                },
            ) => a_t == b_t && a_g == b_g,
            _ => false,
        }
    }
}

impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.path())
    }
}

impl Serialize for Scope {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.path())
    }
}

impl<'de> Deserialize<'de> for Scope {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = <std::borrow::Cow<'_, str>>::deserialize(deserializer)?;
        Scope::parse(&raw).map_err(serde::de::Error::custom)
    }
}

impl std::str::FromStr for Scope {
    type Err = ScopeParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Scope::parse(s)
    }
}

/// Failure surface for [`Scope::parse`].
///
/// `#[non_exhaustive]` — external `match` arms MUST end with `_ =>`. This
/// was the only public enum in the crate without it, which made adding a
/// scope shape a silent breaking change for consumers.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[allow(clippy::enum_variant_names)] // All variants describe "invalid X" by design.
#[non_exhaustive]
pub enum ScopeParseError {
    /// The string did not match any of the canonical scope shapes.
    #[error("scope has invalid format: {0}")]
    InvalidFormat(String),
    /// The tenant segment was not a valid UUID.
    #[error("scope tenant id is not a valid UUID: {0}")]
    InvalidTenantUuid(String),
    /// The resource-group segment was not a valid UUID.
    #[error("scope resource-group id is not a valid UUID: {0}")]
    InvalidResourceGroupUuid(String),
}

#[cfg(test)]
#[path = "scope_tests.rs"]
mod scope_tests;
