// Updated: 2026-04-14 by Constructor Tech
//! Constraint types for authorization decisions.
//!
//! Constraints represent row-level filtering conditions returned by the PDP.
//! They are compiled into `AccessScope` by the PEP compiler.
//!
//! ## Supported predicates
//!
//! - `Eq` / `In` - scalar value predicates (tenant scoping, resource ID)
//! - `InGroup` - group membership subquery: resource visible if member of any listed group
//! - `InGroupSubtree` - group subtree subquery: resource visible if member of any descendant of listed ancestors
//! - `InTenantSubtree` - tenant subtree subquery: resource visible if its tenant is a descendant of a single root tenant

use crate::models::BarrierMode;
use crate::pep::IntoPropertyValue;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tenant_resolver_sdk::TenantStatus;

/// A constraint on a specific resource property.
///
/// Multiple constraints within a response are `ORed`:
/// a resource matches if it satisfies ANY constraint.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema, utoipa::ToSchema)]
pub struct Constraint {
    /// The predicates within this constraint. All predicates are `ANDed`:
    /// a resource matches this constraint only if ALL predicates are satisfied.
    pub predicates: Vec<Predicate>,
}

/// A predicate comparing a resource property to a value or subquery.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema, utoipa::ToSchema)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Predicate {
    /// Equality: `resource_property = value`
    Eq(EqPredicate),
    /// Set membership: `resource_property IN (values)`
    In(InPredicate),
    /// Group membership: `resource_property IN (SELECT resource_id FROM membership WHERE group_id IN (group_ids))`
    InGroup(InGroupPredicate),
    /// Group subtree: `resource_property IN (SELECT resource_id FROM membership WHERE group_id IN (SELECT descendant_id FROM closure WHERE ancestor_id IN (ancestor_ids)))`
    InGroupSubtree(InGroupSubtreePredicate),
    /// Tenant subtree: `resource_property IN (SELECT descendant_id FROM tenant_closure WHERE ancestor_id = root_tenant_id)`
    InTenantSubtree(InTenantSubtreePredicate),
}

/// Equality predicate: `property = value`.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema, utoipa::ToSchema)]
pub struct EqPredicate {
    /// Resource property name (e.g., `pep_properties::OWNER_TENANT_ID`, `pep_properties::RESOURCE_ID`).
    pub property: String,
    /// The value to match (UUID string, plain string, number, bool, etc.).
    pub value: Value,
}

impl EqPredicate {
    /// Create an equality predicate with any convertible value.
    #[must_use]
    pub fn new(property: impl Into<String>, value: impl IntoPropertyValue) -> Self {
        Self {
            property: property.into(),
            value: value.into_filter_value(),
        }
    }
}

/// Set membership predicate: `property IN (values)`.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema, utoipa::ToSchema)]
pub struct InPredicate {
    /// Resource property name (e.g., `pep_properties::OWNER_TENANT_ID`, `pep_properties::RESOURCE_ID`).
    pub property: String,
    /// The set of values to match against.
    pub values: Vec<Value>,
}

impl InPredicate {
    /// Create an `IN` predicate from an iterator of convertible values.
    #[must_use]
    pub fn new<V: IntoPropertyValue>(
        property: impl Into<String>,
        values: impl IntoIterator<Item = V>,
    ) -> Self {
        Self {
            property: property.into(),
            values: values
                .into_iter()
                .map(IntoPropertyValue::into_filter_value)
                .collect(),
        }
    }
}

/// Group membership predicate: resource is visible if it belongs to any of the listed groups.
///
/// The PEP compiler combines this wire-level predicate with the current
/// [`crate::pep::ResourceType`]'s RG member-handle type. `SecureORM` compiles the
/// result to a type-qualified membership subquery and casts the entity property
/// to text to match RG's opaque `resource_id` storage. Compilation fails closed
/// when the resource descriptor has no membership mapping; a degraded PDP
/// response must use an explicit [`InPredicate`] instead.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema, utoipa::ToSchema)]
pub struct InGroupPredicate {
    /// Resource property to filter. Native compilation currently requires
    /// `pep_properties::RESOURCE_ID`; other properties need a future
    /// per-property RG member-type mapping.
    pub property: String,
    /// Group UUIDs - the resource must be a member of at least one.
    pub group_ids: Vec<Value>,
}

impl InGroupPredicate {
    /// Create an `InGroup` predicate.
    #[must_use]
    pub fn new<V: IntoPropertyValue>(
        property: impl Into<String>,
        group_ids: impl IntoIterator<Item = V>,
    ) -> Self {
        Self {
            property: property.into(),
            group_ids: group_ids
                .into_iter()
                .map(IntoPropertyValue::into_filter_value)
                .collect(),
        }
    }
}

/// Group subtree predicate: resource is visible if it belongs to any group
/// that is a descendant of the listed ancestor groups.
///
/// The PEP compiler combines this wire-level predicate with the current
/// [`crate::pep::ResourceType`]'s RG member-handle type. `SecureORM` compiles the
/// result to a type-qualified membership subquery whose group set comes from
/// `resource_group_closure`. Compilation fails closed when the resource
/// descriptor has no membership mapping; a degraded PDP response must use an
/// explicit [`InPredicate`] instead.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema, utoipa::ToSchema)]
pub struct InGroupSubtreePredicate {
    /// Resource property to filter. Native compilation currently requires
    /// `pep_properties::RESOURCE_ID`; other properties need a future
    /// per-property RG member-type mapping.
    pub property: String,
    /// Ancestor group UUIDs - the resource must be a member of any descendant.
    pub ancestor_ids: Vec<Value>,
}

impl InGroupSubtreePredicate {
    /// Create an `InGroupSubtree` predicate.
    #[must_use]
    pub fn new<V: IntoPropertyValue>(
        property: impl Into<String>,
        ancestor_ids: impl IntoIterator<Item = V>,
    ) -> Self {
        Self {
            property: property.into(),
            ancestor_ids: ancestor_ids
                .into_iter()
                .map(IntoPropertyValue::into_filter_value)
                .collect(),
        }
    }
}

/// Tenant subtree predicate: resource is visible if its tenant property is a
/// descendant of a single root tenant per the AM-owned `tenant_closure` table.
///
/// Compiles to (with `barrier_mode = Respect`, the default):
/// `property IN (SELECT descendant_id FROM tenant_closure
///   WHERE ancestor_id = root_tenant_id AND barrier = 0)`
///
/// With `barrier_mode = Ignore`:
/// `property IN (SELECT descendant_id FROM tenant_closure
///   WHERE ancestor_id = root_tenant_id)`
///
/// The `barrier = 0` clamp matches the AM closure-table contract:
/// `barrier` is set when any tenant on the strict path
/// `(ancestor, descendant]` is `self_managed`. Respecting the barrier
/// therefore yields the canonical "subtree minus self-managed branches"
/// semantics; ignoring it is reserved for cross-barrier operations such
/// as billing or tenant metadata reads.
///
/// Multiple-root semantics are expressed at the constraint envelope: emit
/// one `Constraint` per root and rely on the OR-of-constraints semantics.
///
/// **`descendant_status`:** When non-empty, the predicate compiles to
/// `AND descendant_status IN (...)` on the closure subquery, restricting
/// the subtree to tenants in the listed statuses. The name mirrors the
/// `tenant_closure.descendant_status` column so the binding is unambiguous
/// — the filter applies to the descendants reached via the closure, not
/// to the ancestor root.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema, utoipa::ToSchema)]
pub struct InTenantSubtreePredicate {
    /// Resource property to filter (e.g., `pep_properties::OWNER_TENANT_ID`,
    /// or `pep_properties::RESOURCE_ID` on the `tenants` entity itself).
    pub property: String,
    /// Root tenant UUID — the resource's tenant must be a descendant of this tenant.
    pub root_tenant_id: Value,
    /// Barrier enforcement mode. Defaults to [`BarrierMode::Respect`]
    /// which clamps the closure subquery with `AND barrier = 0`.
    #[serde(default)]
    pub barrier_mode: BarrierMode,
    /// Status filter applied to the descendants reached via the closure.
    ///
    /// Empty list means "no status filter"; a non-empty list compiles to
    /// `AND descendant_status IN (...)` on the closure subquery. The PEP
    /// maps each [`TenantStatus`] to the SMALLINT encoding canonically
    /// defined by [`TenantStatus::as_smallint`] (`Active = 1`,
    /// `Suspended = 2`, `Deleted = 3`).
    #[serde(default)]
    pub descendant_status: Vec<TenantStatus>,
}

impl InTenantSubtreePredicate {
    /// Create an `InTenantSubtree` predicate with the default barrier
    /// mode ([`BarrierMode::Respect`]) and no status filter.
    #[must_use]
    pub fn new<V: IntoPropertyValue>(property: impl Into<String>, root_tenant_id: V) -> Self {
        Self::with_barrier_mode(property, root_tenant_id, BarrierMode::Respect)
    }

    /// Create an `InTenantSubtree` predicate with an explicit barrier mode
    /// and no status filter.
    #[must_use]
    pub fn with_barrier_mode<V: IntoPropertyValue>(
        property: impl Into<String>,
        root_tenant_id: V,
        barrier_mode: BarrierMode,
    ) -> Self {
        Self {
            property: property.into(),
            root_tenant_id: root_tenant_id.into_filter_value(),
            barrier_mode,
            descendant_status: Vec::new(),
        }
    }

    /// Set the descendant-status filter on this predicate, replacing any
    /// previously set list. Empty list disables the filter.
    #[must_use]
    pub fn with_descendant_status(mut self, statuses: Vec<TenantStatus>) -> Self {
        self.descendant_status = statuses;
        self
    }
}

#[cfg(test)]
#[path = "constraints_tests.rs"]
mod constraints_tests;
