use std::fmt;
use uuid::Uuid;

/// A scalar value for scope filtering.
///
/// Used in [`ScopeFilter`] predicates to represent typed values.
/// JSON conversion happens at the PDP/PEP boundary (see the PEP compiler),
/// not inside the security model.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ScopeValue {
    /// UUID value (tenant IDs, resource IDs, etc.)
    Uuid(Uuid),
    /// String value (status, GTS type IDs, etc.)
    String(String),
    /// Integer value.
    Int(i64),
    /// Boolean value.
    Bool(bool),
}

impl ScopeValue {
    /// Try to extract a UUID from this value.
    ///
    /// Returns `Some` for `ScopeValue::Uuid` directly, and for
    /// `ScopeValue::String` if the string is a valid UUID.
    #[must_use]
    pub fn as_uuid(&self) -> Option<Uuid> {
        match self {
            Self::Uuid(u) => Some(*u),
            Self::String(s) => Uuid::parse_str(s).ok(),
            Self::Int(_) | Self::Bool(_) => None,
        }
    }
}

impl fmt::Display for ScopeValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Uuid(u) => write!(f, "{u}"),
            Self::String(s) => write!(f, "{s}"),
            Self::Int(n) => write!(f, "{n}"),
            Self::Bool(b) => write!(f, "{b}"),
        }
    }
}

impl From<Uuid> for ScopeValue {
    #[inline]
    fn from(u: Uuid) -> Self {
        Self::Uuid(u)
    }
}

impl From<&Uuid> for ScopeValue {
    #[inline]
    fn from(u: &Uuid) -> Self {
        Self::Uuid(*u)
    }
}

impl From<String> for ScopeValue {
    #[inline]
    fn from(s: String) -> Self {
        Self::String(s)
    }
}

impl From<&str> for ScopeValue {
    #[inline]
    fn from(s: &str) -> Self {
        Self::String(s.to_owned())
    }
}

impl From<i64> for ScopeValue {
    #[inline]
    fn from(n: i64) -> Self {
        Self::Int(n)
    }
}

impl From<bool> for ScopeValue {
    #[inline]
    fn from(b: bool) -> Self {
        Self::Bool(b)
    }
}

/// Well-known authorization property names.
///
/// These constants are shared between the PEP compiler and the ORM condition
/// builder (`ScopableEntity::resolve_property()`), ensuring a single source of
/// truth for property names.
pub mod pep_properties {
    /// Tenant-ownership property. Typically maps to the `tenant_id` column.
    pub const OWNER_TENANT_ID: &str = "owner_tenant_id";

    /// Resource identity property. Typically maps to the primary key column.
    pub const RESOURCE_ID: &str = "id";

    /// Owner (user) identity property. Typically maps to an `owner_id` column.
    pub const OWNER_ID: &str = "owner_id";
}

/// Well-known resource-group table and column names for subquery construction.
///
/// Used by the `SecureORM` condition builder to translate `InGroup`/`InGroupSubtree`
/// scope filters into SQL subqueries without depending on entity types.
///
/// **Note:** These tables are canonical to the RG gear's database.
/// `InGroup`/`InGroupSubtree` predicates are executable only where the RG tables
/// are co-located or projected into the querying database.
pub mod rg_tables {
    /// Membership table owned by the RG gear.
    pub const MEMBERSHIP_TABLE: &str = "resource_group_membership";
    /// Column in membership table: the resource's external ID.
    pub const MEMBERSHIP_RESOURCE_ID: &str = "resource_id";
    /// Column in membership table: the group the resource belongs to.
    pub const MEMBERSHIP_GROUP_ID: &str = "group_id";
    /// Column in membership table: the RG-local GTS type surrogate.
    pub const MEMBERSHIP_GTS_TYPE_ID: &str = "gts_type_id";

    /// RG-local GTS type registry table.
    pub const GTS_TYPE_TABLE: &str = "gts_type";
    /// Primary key in the RG-local GTS type registry.
    pub const GTS_TYPE_ID: &str = "id";
    /// External GTS schema identifier in the RG-local type registry.
    pub const GTS_TYPE_SCHEMA_ID: &str = "schema_id";

    /// Closure table for group hierarchy.
    pub const CLOSURE_TABLE: &str = "resource_group_closure";
    /// Column in closure table: the ancestor group.
    pub const CLOSURE_ANCESTOR_ID: &str = "ancestor_id";
    /// Column in closure table: the descendant group.
    pub const CLOSURE_DESCENDANT_ID: &str = "descendant_id";
}

/// Well-known tenant-closure table and column names for subquery construction.
///
/// Used by the `SecureORM` condition builder to translate `InTenantSubtree`
/// scope filters into SQL subqueries without depending on entity types.
///
/// **Note:** This table is canonical to the Account Management gear's
/// database. `InTenantSubtree` predicates are only executable in gears
/// that share the AM database (or replicate `tenant_closure` from it).
pub mod tenant_tables {
    /// Closure table for tenant hierarchy.
    pub const CLOSURE_TABLE: &str = "tenant_closure";
    /// Column in closure table: the ancestor tenant.
    pub const CLOSURE_ANCESTOR_ID: &str = "ancestor_id";
    /// Column in closure table: the descendant tenant.
    pub const CLOSURE_DESCENDANT_ID: &str = "descendant_id";
    /// Column in closure table: barrier flag.
    ///
    /// AM materializes `barrier = 1` on every closure row whose strict path
    /// `(ancestor, descendant]` crosses a self-managed tenant. Subtree
    /// queries that should stop at delegation boundaries clamp the
    /// subquery with `AND barrier = 0`.
    pub const CLOSURE_BARRIER: &str = "barrier";
    /// Column in closure table: status of the descendant tenant (SMALLINT,
    /// canonically `{1 = active, 2 = suspended, 3 = deleted}` — see
    /// `tenant_resolver_sdk::TenantStatus::as_smallint`).
    pub const CLOSURE_DESCENDANT_STATUS: &str = "descendant_status";
}

/// A single scope filter — a typed predicate on a named resource property.
///
/// The property name (e.g., `"owner_tenant_id"`, `"id"`) is an authorization
/// concept. Mapping to DB columns is done by `ScopableEntity::resolve_property()`.
///
/// Variants mirror the predicate types from the PDP response:
/// - [`ScopeFilter::Eq`] — equality (`property = value`)
/// - [`ScopeFilter::In`] — set membership (`property IN (values)`)
/// - [`ScopeFilter::InGroup`] — group membership subquery
/// - [`ScopeFilter::InGroupSubtree`] — group subtree subquery
/// - [`ScopeFilter::InTenantSubtree`] — tenant subtree subquery on `tenant_closure`
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScopeFilter {
    /// Equality: `property = value`.
    Eq(EqScopeFilter),
    /// Set membership: `property IN (values)`.
    In(InScopeFilter),
    /// Typed group membership. The membership subquery resolves
    /// `gts_type_id` through `gts_type.schema_id = membership_resource_type`
    /// and compares its opaque `resource_id` with `CAST(property AS text)`.
    InGroup(InGroupScopeFilter),
    /// Typed group subtree. Uses the same local `gts_type` resolution and text
    /// comparison as [`ScopeFilter::InGroup`], with matching group IDs selected
    /// from descendants in `resource_group_closure`.
    InGroupSubtree(InGroupSubtreeScopeFilter),
    /// Tenant subtree: `property IN (SELECT descendant_id FROM tenant_closure WHERE ancestor_id = root_tenant_id)`.
    InTenantSubtree(InTenantSubtreeScopeFilter),
}

/// Equality scope filter: `property = value`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct EqScopeFilter {
    /// Authorization property name (e.g., `pep_properties::OWNER_TENANT_ID`).
    property: String,
    /// The value to match.
    value: ScopeValue,
}

/// Set membership scope filter: `property IN (values)`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InScopeFilter {
    /// Authorization property name (e.g., `pep_properties::OWNER_TENANT_ID`).
    property: String,
    /// The set of values to match against.
    values: Vec<ScopeValue>,
}

impl EqScopeFilter {
    /// Create an equality scope filter.
    #[must_use]
    pub fn new(property: impl Into<String>, value: impl Into<ScopeValue>) -> Self {
        Self {
            property: property.into(),
            value: value.into(),
        }
    }

    /// The authorization property name.
    #[inline]
    #[must_use]
    pub fn property(&self) -> &str {
        &self.property
    }

    /// The filter value.
    #[inline]
    #[must_use]
    pub fn value(&self) -> &ScopeValue {
        &self.value
    }
}

impl InScopeFilter {
    /// Create a set membership scope filter.
    #[must_use]
    pub fn new(property: impl Into<String>, values: Vec<ScopeValue>) -> Self {
        Self {
            property: property.into(),
            values,
        }
    }

    /// Create from an iterator of convertible values.
    #[must_use]
    pub fn from_values<V: Into<ScopeValue>>(
        property: impl Into<String>,
        values: impl IntoIterator<Item = V>,
    ) -> Self {
        Self {
            property: property.into(),
            values: values.into_iter().map(Into::into).collect(),
        }
    }

    /// The authorization property name.
    #[inline]
    #[must_use]
    pub fn property(&self) -> &str {
        &self.property
    }

    /// The filter values.
    #[inline]
    #[must_use]
    pub fn values(&self) -> &[ScopeValue] {
        &self.values
    }
}

/// Group membership scope filter, qualified by the RG member-handle type.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InGroupScopeFilter {
    property: String,
    membership_resource_type: String,
    group_ids: Vec<ScopeValue>,
}

impl InGroupScopeFilter {
    /// Create an untyped group membership scope filter.
    ///
    /// This source-compatible constructor cannot safely qualify RG membership
    /// rows and therefore compiles to deny-all. New callers should use
    /// [`Self::new_typed`].
    #[deprecated(
        since = "0.9.1",
        note = "untyped group filters compile to deny-all; use `new_typed` with the RG member-handle type"
    )]
    #[must_use]
    pub fn new(property: impl Into<String>, group_ids: Vec<ScopeValue>) -> Self {
        Self {
            property: property.into(),
            membership_resource_type: String::new(),
            group_ids,
        }
    }

    /// Create a group membership scope filter qualified by the external GTS
    /// schema id stored for the member handle in RG's local type registry.
    ///
    /// The containing [`ScopeConstraint`] must also include an
    /// `owner_tenant_id` filter; SQL compilation rejects group-only constraints.
    #[must_use]
    pub fn new_typed(
        property: impl Into<String>,
        membership_resource_type: impl Into<String>,
        group_ids: Vec<ScopeValue>,
    ) -> Self {
        Self {
            property: property.into(),
            membership_resource_type: membership_resource_type.into(),
            group_ids,
        }
    }

    /// The authorization property name.
    #[inline]
    #[must_use]
    pub fn property(&self) -> &str {
        &self.property
    }

    /// The external GTS schema id qualifying membership rows.
    #[inline]
    #[must_use]
    pub fn membership_resource_type(&self) -> &str {
        &self.membership_resource_type
    }

    /// The group IDs.
    #[inline]
    #[must_use]
    pub fn group_ids(&self) -> &[ScopeValue] {
        &self.group_ids
    }
}

/// Group subtree scope filter, qualified by the RG member-handle type.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InGroupSubtreeScopeFilter {
    property: String,
    membership_resource_type: String,
    ancestor_ids: Vec<ScopeValue>,
}

impl InGroupSubtreeScopeFilter {
    /// Create an untyped group subtree scope filter.
    ///
    /// This source-compatible constructor cannot safely qualify RG membership
    /// rows and therefore compiles to deny-all. New callers should use
    /// [`Self::new_typed`].
    #[deprecated(
        since = "0.9.1",
        note = "untyped group filters compile to deny-all; use `new_typed` with the RG member-handle type"
    )]
    #[must_use]
    pub fn new(property: impl Into<String>, ancestor_ids: Vec<ScopeValue>) -> Self {
        Self {
            property: property.into(),
            membership_resource_type: String::new(),
            ancestor_ids,
        }
    }

    /// Create a group subtree scope filter qualified by the external GTS schema
    /// id stored for the member handle in RG's local type registry.
    ///
    /// The containing [`ScopeConstraint`] must also include an
    /// `owner_tenant_id` filter; SQL compilation rejects group-only constraints.
    #[must_use]
    pub fn new_typed(
        property: impl Into<String>,
        membership_resource_type: impl Into<String>,
        ancestor_ids: Vec<ScopeValue>,
    ) -> Self {
        Self {
            property: property.into(),
            membership_resource_type: membership_resource_type.into(),
            ancestor_ids,
        }
    }

    /// The authorization property name.
    #[inline]
    #[must_use]
    pub fn property(&self) -> &str {
        &self.property
    }

    /// The external GTS schema id qualifying membership rows.
    #[inline]
    #[must_use]
    pub fn membership_resource_type(&self) -> &str {
        &self.membership_resource_type
    }

    /// The ancestor group IDs.
    #[inline]
    #[must_use]
    pub fn ancestor_ids(&self) -> &[ScopeValue] {
        &self.ancestor_ids
    }
}

/// Tenant subtree scope filter — clamps a property to descendants of a single
/// root tenant via the `tenant_closure` table.
///
/// Compiles to (with `respect_barriers = true`, the default, and an empty
/// `descendant_status`):
/// `property IN (SELECT descendant_id FROM tenant_closure
///   WHERE ancestor_id = root_tenant_id AND barrier = 0)`
///
/// With `respect_barriers = false`:
/// `property IN (SELECT descendant_id FROM tenant_closure
///   WHERE ancestor_id = root_tenant_id)`
///
/// With a non-empty `descendant_status` (each value is the canonical
/// SMALLINT for a tenant status — see
/// `tenant_resolver_sdk::TenantStatus::as_smallint`):
/// `... AND descendant_status IN (...)`
///
/// **Heads-up for `tenants`-style entities:** When a property resolves
/// to the `tenants` row's own primary key (via `pep_properties::RESOURCE_ID`),
/// the entity must declare the `id` column as a resolvable secured
/// property. Entities marked with `#[secure(no_resource, ...)]` will
/// fail-closed at scope resolution time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InTenantSubtreeScopeFilter {
    property: String,
    root_tenant_id: ScopeValue,
    respect_barriers: bool,
    descendant_status: Vec<ScopeValue>,
}

impl InTenantSubtreeScopeFilter {
    /// Create a tenant subtree scope filter that respects barriers with no
    /// status filter.
    ///
    /// Equivalent to
    /// `with_respect_barriers(property, root_tenant_id, true)`.
    #[must_use]
    pub fn new(property: impl Into<String>, root_tenant_id: impl Into<ScopeValue>) -> Self {
        Self::with_respect_barriers(property, root_tenant_id, true)
    }

    /// Create a tenant subtree scope filter with explicit barrier handling
    /// and no status filter.
    #[must_use]
    pub fn with_respect_barriers(
        property: impl Into<String>,
        root_tenant_id: impl Into<ScopeValue>,
        respect_barriers: bool,
    ) -> Self {
        Self::with_descendant_status(property, root_tenant_id, respect_barriers, Vec::new())
    }

    /// Create a tenant subtree scope filter with explicit barrier handling
    /// and a (possibly empty) status filter on the descendants. An empty
    /// list is equivalent to "no status filter".
    #[must_use]
    pub fn with_descendant_status(
        property: impl Into<String>,
        root_tenant_id: impl Into<ScopeValue>,
        respect_barriers: bool,
        descendant_status: Vec<ScopeValue>,
    ) -> Self {
        Self {
            property: property.into(),
            root_tenant_id: root_tenant_id.into(),
            respect_barriers,
            descendant_status,
        }
    }

    /// The authorization property name.
    #[inline]
    #[must_use]
    pub fn property(&self) -> &str {
        &self.property
    }

    /// The single root tenant ID at which the subtree is anchored.
    #[inline]
    #[must_use]
    pub fn root_tenant_id(&self) -> &ScopeValue {
        &self.root_tenant_id
    }

    /// Whether the SQL compilation should clamp the closure subquery
    /// with `AND barrier = 0` (i.e. stop at self-managed boundaries).
    #[inline]
    #[must_use]
    pub fn respect_barriers(&self) -> bool {
        self.respect_barriers
    }

    /// Status filter applied to the descendants reached via the closure.
    ///
    /// Empty slice means "no status filter"; otherwise the SQL adds
    /// `AND descendant_status IN (...)` to the closure subquery. Values
    /// are expected to be SMALLINT-encoded statuses
    /// (see `tenant_resolver_sdk::TenantStatus::as_smallint`).
    #[inline]
    #[must_use]
    pub fn descendant_status(&self) -> &[ScopeValue] {
        &self.descendant_status
    }
}

impl ScopeFilter {
    /// Create an equality filter (`property = value`).
    #[must_use]
    pub fn eq(property: impl Into<String>, value: impl Into<ScopeValue>) -> Self {
        Self::Eq(EqScopeFilter::new(property, value))
    }

    /// Create a set membership filter (`property IN (values)`).
    #[must_use]
    pub fn r#in(property: impl Into<String>, values: Vec<ScopeValue>) -> Self {
        Self::In(InScopeFilter::new(property, values))
    }

    /// Create a set membership filter from UUID values (convenience).
    #[must_use]
    pub fn in_uuids(property: impl Into<String>, uuids: Vec<Uuid>) -> Self {
        Self::In(InScopeFilter::new(
            property,
            uuids.into_iter().map(ScopeValue::Uuid).collect(),
        ))
    }

    /// Create an untyped group membership filter that compiles to deny-all.
    ///
    /// Retained for source compatibility. New callers should use
    /// [`Self::in_group_typed`] so membership rows can be safely qualified.
    #[deprecated(
        since = "0.9.1",
        note = "untyped group filters compile to deny-all; use `in_group_typed` with the RG member-handle type"
    )]
    #[must_use]
    pub fn in_group(property: impl Into<String>, group_ids: Vec<ScopeValue>) -> Self {
        #[allow(deprecated)]
        Self::InGroup(InGroupScopeFilter::new(property, group_ids))
    }

    /// Create a group membership filter qualified by its RG member-handle type.
    #[must_use]
    pub fn in_group_typed(
        property: impl Into<String>,
        membership_resource_type: impl Into<String>,
        group_ids: Vec<ScopeValue>,
    ) -> Self {
        Self::InGroup(InGroupScopeFilter::new_typed(
            property,
            membership_resource_type,
            group_ids,
        ))
    }

    /// Create an untyped group subtree filter that compiles to deny-all.
    ///
    /// Retained for source compatibility. New callers should use
    /// [`Self::in_group_subtree_typed`] so membership rows can be safely
    /// qualified.
    #[deprecated(
        since = "0.9.1",
        note = "untyped group filters compile to deny-all; use `in_group_subtree_typed` with the RG member-handle type"
    )]
    #[must_use]
    pub fn in_group_subtree(property: impl Into<String>, ancestor_ids: Vec<ScopeValue>) -> Self {
        #[allow(deprecated)]
        Self::InGroupSubtree(InGroupSubtreeScopeFilter::new(property, ancestor_ids))
    }

    /// Create a group subtree filter qualified by its RG member-handle type.
    #[must_use]
    pub fn in_group_subtree_typed(
        property: impl Into<String>,
        membership_resource_type: impl Into<String>,
        ancestor_ids: Vec<ScopeValue>,
    ) -> Self {
        Self::InGroupSubtree(InGroupSubtreeScopeFilter::new_typed(
            property,
            membership_resource_type,
            ancestor_ids,
        ))
    }

    /// Create a tenant subtree filter rooted at a single ancestor tenant.
    ///
    /// `descendant_status` is a (possibly empty) list of SMALLINT-encoded
    /// tenant statuses (see `tenant_resolver_sdk::TenantStatus::as_smallint`);
    /// when non-empty, the SQL adds `AND descendant_status IN (...)` to
    /// the closure subquery. Pass `Vec::new()` for "no status filter".
    #[must_use]
    pub fn in_tenant_subtree(
        property: impl Into<String>,
        root_tenant_id: impl Into<ScopeValue>,
        respect_barriers: bool,
        descendant_status: Vec<ScopeValue>,
    ) -> Self {
        Self::InTenantSubtree(InTenantSubtreeScopeFilter::with_descendant_status(
            property,
            root_tenant_id,
            respect_barriers,
            descendant_status,
        ))
    }

    /// The authorization property name.
    #[must_use]
    pub fn property(&self) -> &str {
        match self {
            Self::Eq(f) => f.property(),
            Self::In(f) => f.property(),
            Self::InGroup(f) => f.property(),
            Self::InGroupSubtree(f) => f.property(),
            Self::InTenantSubtree(f) => f.property(),
        }
    }

    /// Collect direct-match values as a slice-like view for iteration.
    ///
    /// For `Eq`, returns a single-element slice; for `In`, returns the values slice.
    /// For `InGroup`/`InGroupSubtree`/`InTenantSubtree`, returns empty — those
    /// are subquery parameters, not resource property values. The actual
    /// matching happens in SQL via [`secure::scope_to_condition`].
    ///
    /// **Write-path limitation:** Because `InTenantSubtree` returns an empty
    /// slice here, in-memory helpers such as [`AccessScope::contains_uuid`] and
    /// [`AccessScope::all_uuid_values_for`] always return negative/empty results
    /// for this filter variant. Secure-insert paths that validate scope membership
    /// via these helpers cannot use `InTenantSubtree` as a substitute for
    /// `allow_all()` without an additional DB-backed tenant-membership check.
    #[must_use]
    pub fn values(&self) -> ScopeFilterValues<'_> {
        match self {
            Self::Eq(f) => ScopeFilterValues::Single(&f.value),
            Self::In(f) => ScopeFilterValues::Multiple(&f.values),
            Self::InGroup(_) | Self::InGroupSubtree(_) | Self::InTenantSubtree(_) => {
                ScopeFilterValues::Multiple(&[])
            }
        }
    }

    /// Extract filter values as UUIDs, skipping non-UUID entries.
    ///
    /// Useful when the caller knows the property holds UUID values
    /// (e.g., `owner_tenant_id`, `id`).
    #[must_use]
    pub fn uuid_values(&self) -> Vec<Uuid> {
        self.values()
            .iter()
            .filter_map(ScopeValue::as_uuid)
            .collect()
    }
}

/// Iterator adapter for [`ScopeFilter::values()`].
///
/// Provides a uniform way to iterate over filter values regardless of
/// whether the filter is `Eq` (single value) or `In` (multiple values).
#[derive(Clone, Debug)]
pub enum ScopeFilterValues<'a> {
    /// Single value from an `Eq` filter.
    Single(&'a ScopeValue),
    /// Multiple values from an `In` filter.
    Multiple(&'a [ScopeValue]),
}

impl<'a> ScopeFilterValues<'a> {
    /// Returns an iterator over the values.
    #[must_use]
    pub fn iter(&self) -> ScopeFilterValuesIter<'a> {
        match self {
            Self::Single(v) => ScopeFilterValuesIter::Single(Some(v)),
            Self::Multiple(vs) => ScopeFilterValuesIter::Multiple(vs.iter()),
        }
    }

    /// Returns `true` if any value matches the given predicate.
    #[must_use]
    pub fn contains(&self, value: &ScopeValue) -> bool {
        self.iter().any(|v| v == value)
    }
}

impl<'a> IntoIterator for ScopeFilterValues<'a> {
    type Item = &'a ScopeValue;
    type IntoIter = ScopeFilterValuesIter<'a>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl<'a> IntoIterator for &ScopeFilterValues<'a> {
    type Item = &'a ScopeValue;
    type IntoIter = ScopeFilterValuesIter<'a>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

/// Iterator over [`ScopeFilterValues`].
pub enum ScopeFilterValuesIter<'a> {
    /// Yields a single value.
    Single(Option<&'a ScopeValue>),
    /// Yields from a slice.
    Multiple(std::slice::Iter<'a, ScopeValue>),
}

impl<'a> Iterator for ScopeFilterValuesIter<'a> {
    type Item = &'a ScopeValue;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Single(v) => v.take(),
            Self::Multiple(iter) => iter.next(),
        }
    }
}

/// A conjunction (AND) of scope filters — one access path.
///
/// All filters within a constraint must match simultaneously for a row
/// to be accessible via this path.
#[derive(Clone, Debug, PartialEq)]
pub struct ScopeConstraint {
    filters: Vec<ScopeFilter>,
}

impl ScopeConstraint {
    /// Create a new scope constraint from a list of filters.
    #[must_use]
    pub fn new(filters: Vec<ScopeFilter>) -> Self {
        Self { filters }
    }

    /// The filters in this constraint (AND-ed together).
    #[inline]
    #[must_use]
    pub fn filters(&self) -> &[ScopeFilter] {
        &self.filters
    }

    /// Returns `true` if this constraint has no filters.
    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.filters.is_empty()
    }
}

/// A disjunction (OR) of scope constraints defining what data is accessible.
///
/// Each constraint is an independent access path (OR-ed). Filters within a
/// constraint are AND-ed. An unconstrained scope bypasses row-level filtering.
///
/// # Examples
///
/// ```
/// use toolkit_security::access_scope::{AccessScope, ScopeConstraint, ScopeFilter, pep_properties};
/// use uuid::Uuid;
///
/// // deny-all (default)
/// let scope = AccessScope::deny_all();
/// assert!(scope.is_deny_all());
///
/// // single tenant
/// let tid = Uuid::new_v4();
/// let scope = AccessScope::for_tenant(tid);
/// assert!(!scope.is_deny_all());
/// assert!(scope.contains_uuid(pep_properties::OWNER_TENANT_ID, tid));
/// ```
#[derive(Clone, Debug, PartialEq)]
pub struct AccessScope {
    constraints: Vec<ScopeConstraint>,
    unconstrained: bool,
}

impl Default for AccessScope {
    /// Default is deny-all: no constraints and not unconstrained.
    fn default() -> Self {
        Self::deny_all()
    }
}

impl AccessScope {
    // ── Constructors ────────────────────────────────────────────────

    /// Create an access scope from a list of constraints (OR-ed).
    #[must_use]
    pub fn from_constraints(constraints: Vec<ScopeConstraint>) -> Self {
        Self {
            constraints,
            unconstrained: false,
        }
    }

    /// Create an access scope with a single constraint.
    #[must_use]
    pub fn single(constraint: ScopeConstraint) -> Self {
        Self::from_constraints(vec![constraint])
    }

    /// Create an "allow all" (unconstrained) scope.
    ///
    /// This represents a legitimate PDP decision with no row-level filtering.
    /// Not a bypass — it's a valid authorization outcome.
    #[must_use]
    pub fn allow_all() -> Self {
        Self {
            constraints: Vec::new(),
            unconstrained: true,
        }
    }

    /// Create a "deny all" scope (no access).
    #[must_use]
    pub fn deny_all() -> Self {
        Self {
            constraints: Vec::new(),
            unconstrained: false,
        }
    }

    // ── Convenience constructors ────────────────────────────────────

    /// Create a scope for a set of tenant IDs.
    #[must_use]
    pub fn for_tenants(ids: Vec<Uuid>) -> Self {
        Self::single(ScopeConstraint::new(vec![ScopeFilter::in_uuids(
            pep_properties::OWNER_TENANT_ID,
            ids,
        )]))
    }

    /// Create a scope for a single tenant ID.
    #[must_use]
    pub fn for_tenant(id: Uuid) -> Self {
        Self::for_tenants(vec![id])
    }

    /// Create a scope for a set of resource IDs.
    #[must_use]
    pub fn for_resources(ids: Vec<Uuid>) -> Self {
        Self::single(ScopeConstraint::new(vec![ScopeFilter::in_uuids(
            pep_properties::RESOURCE_ID,
            ids,
        )]))
    }

    /// Create a scope for a single resource ID.
    #[must_use]
    pub fn for_resource(id: Uuid) -> Self {
        Self::for_resources(vec![id])
    }

    // ── Accessors ───────────────────────────────────────────────────

    /// The constraints in this scope (OR-ed).
    #[inline]
    #[must_use]
    pub fn constraints(&self) -> &[ScopeConstraint] {
        &self.constraints
    }

    /// Returns `true` if this scope is unconstrained (allow-all).
    #[inline]
    #[must_use]
    pub fn is_unconstrained(&self) -> bool {
        self.unconstrained
    }

    /// Returns `true` if this scope denies all access.
    ///
    /// A scope is deny-all when it is not unconstrained and has no constraints.
    #[must_use]
    pub fn is_deny_all(&self) -> bool {
        !self.unconstrained && self.constraints.is_empty()
    }

    /// Collect all values for a given property across all constraints.
    #[must_use]
    pub fn all_values_for(&self, property: &str) -> Vec<&ScopeValue> {
        let mut result = Vec::new();
        for constraint in &self.constraints {
            for filter in constraint.filters() {
                if filter.property() == property {
                    result.extend(filter.values());
                }
            }
        }
        result
    }

    /// Collect all UUID values for a given property across all constraints.
    ///
    /// Convenience wrapper — skips non-UUID values.
    #[must_use]
    pub fn all_uuid_values_for(&self, property: &str) -> Vec<Uuid> {
        let mut result = Vec::new();
        for constraint in &self.constraints {
            for filter in constraint.filters() {
                if filter.property() == property {
                    result.extend(filter.uuid_values());
                }
            }
        }
        result
    }

    /// Check if any constraint has a filter matching the given property and value.
    #[must_use]
    pub fn contains_value(&self, property: &str, value: &ScopeValue) -> bool {
        self.constraints.iter().any(|c| {
            c.filters()
                .iter()
                .any(|f| f.property() == property && f.values().contains(value))
        })
    }

    /// Check if any constraint has a filter matching the given property and UUID.
    ///
    /// Matches both `ScopeValue::Uuid` and `ScopeValue::String` variants so
    /// that UUID-as-string values are treated consistently with
    /// [`AccessScope::all_uuid_values_for`], which also parses strings via
    /// [`ScopeValue::as_uuid`].
    #[must_use]
    pub fn contains_uuid(&self, property: &str, id: Uuid) -> bool {
        self.constraints.iter().any(|c| {
            c.filters().iter().any(|f| {
                f.property() == property && f.values().iter().any(|v| v.as_uuid() == Some(id))
            })
        })
    }

    /// Check if any constraint references the given property.
    #[must_use]
    pub fn has_property(&self, property: &str) -> bool {
        self.constraints
            .iter()
            .any(|c| c.filters().iter().any(|f| f.property() == property))
    }

    /// Create a new scope retaining only `owner_tenant_id` filters.
    ///
    /// Useful for entities declared with `no_owner` (e.g., messages, reactions),
    /// where `owner_id` constraints cannot be resolved and would cause fail-closed
    /// deny-all behaviour.
    ///
    /// - Unconstrained scopes become deny-all (fail-closed).
    /// - Constraints that contain no `owner_tenant_id` filter are dropped entirely.
    /// - If all constraints are dropped, the result is deny-all.
    #[must_use]
    pub fn tenant_only(&self) -> Self {
        self.retain_properties(&[pep_properties::OWNER_TENANT_ID])
    }

    /// Create a new scope retaining only `owner_tenant_id` and `owner_id` filters.
    ///
    /// Useful for entities that have both tenant and owner columns but no
    /// resource-level constraints (e.g., reactions scoped to the acting user).
    ///
    /// - Unconstrained scopes become deny-all (fail-closed).
    /// - Constraints that contain none of the retained properties are dropped.
    /// - If all constraints are dropped, the result is deny-all.
    #[must_use]
    pub fn tenant_and_owner(&self) -> Self {
        self.retain_properties(&[pep_properties::OWNER_TENANT_ID, pep_properties::OWNER_ID])
    }

    /// Create a new scope that guarantees an `owner_id` equality filter
    /// matching exactly the supplied `owner_id` is present in every constraint.
    ///
    /// **Intersection semantics**: if a constraint already contains an
    /// `owner_id` filter, the supplied value must be among its values —
    /// otherwise the constraint is dropped. When it matches, the filter is
    /// narrowed to exactly that single value.
    ///
    /// - **Unconstrained** → single constraint with only the `owner_id` filter.
    /// - **Deny-all** → stays deny-all.
    /// - **No existing owner filter** → `owner_id` is injected.
    /// - **Existing owner filter containing `owner_id`** → narrowed to `Eq`.
    /// - **Existing owner filter NOT containing `owner_id`** → constraint dropped
    ///   (constraints use OR semantics, so dropping one narrows access; dropping
    ///   all yields deny-all).
    ///
    /// Use this as a defence-in-depth measure for user-owned resources when
    /// the PDP may not always return `owner_id` constraints or may return a
    /// broader set than the current subject.
    #[must_use]
    pub fn ensure_owner(&self, owner_id: Uuid) -> Self {
        if self.is_deny_all() {
            return Self::deny_all();
        }

        let owner_filter = ScopeFilter::eq(pep_properties::OWNER_ID, owner_id);

        if self.unconstrained {
            return Self::single(ScopeConstraint::new(vec![owner_filter]));
        }

        let constraints = self
            .constraints
            .iter()
            .filter_map(|c| {
                let owner_filters: Vec<&ScopeFilter> = c
                    .filters()
                    .iter()
                    .filter(|f| f.property() == pep_properties::OWNER_ID)
                    .collect();

                if owner_filters.is_empty() {
                    let mut filters = c.filters().to_vec();
                    filters.push(owner_filter.clone());
                    return Some(ScopeConstraint::new(filters));
                }

                // Intersection semantics: ALL owner_id predicates must contain
                // the supplied owner_id, otherwise the constraint is dropped.
                let all_match = owner_filters
                    .iter()
                    .all(|f| f.values().iter().any(|v| v.as_uuid() == Some(owner_id)));
                if !all_match {
                    return None;
                }

                // Fast path: single Eq already matches → constraint unchanged.
                if owner_filters.len() == 1 && matches!(owner_filters[0], ScopeFilter::Eq(_)) {
                    return Some(c.clone());
                }

                // Replace all owner_id filters with a single Eq.
                let mut filters: Vec<ScopeFilter> = c
                    .filters()
                    .iter()
                    .filter(|f| f.property() != pep_properties::OWNER_ID)
                    .cloned()
                    .collect();
                filters.push(owner_filter.clone());
                Some(ScopeConstraint::new(filters))
            })
            .collect();

        Self::from_constraints(constraints)
    }

    /// Internal helper: build a new scope keeping only filters whose property
    /// is in the given whitelist.
    fn retain_properties(&self, properties: &[&str]) -> Self {
        if self.unconstrained {
            return Self::deny_all();
        }

        let constraints = self
            .constraints
            .iter()
            .filter_map(|c| {
                let kept: Vec<ScopeFilter> = c
                    .filters()
                    .iter()
                    .filter(|f| properties.contains(&f.property()))
                    .cloned()
                    .collect();

                if kept.is_empty() {
                    None
                } else {
                    Some(ScopeConstraint::new(kept))
                }
            })
            .collect();

        Self::from_constraints(constraints)
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use uuid::Uuid;

    const T1: &str = "11111111-1111-1111-1111-111111111111";
    const T2: &str = "22222222-2222-2222-2222-222222222222";

    fn uid(s: &str) -> Uuid {
        Uuid::parse_str(s).unwrap()
    }

    // --- ScopeFilter::Eq ---

    #[test]
    fn scope_filter_eq_constructor() {
        let f = ScopeFilter::eq(pep_properties::OWNER_TENANT_ID, uid(T1));
        assert_eq!(f.property(), pep_properties::OWNER_TENANT_ID);
        assert!(matches!(f, ScopeFilter::Eq(_)));
        assert!(f.values().contains(&ScopeValue::Uuid(uid(T1))));
    }

    #[test]
    fn all_values_for_works_with_eq() {
        let scope = AccessScope::single(ScopeConstraint::new(vec![ScopeFilter::eq(
            pep_properties::OWNER_TENANT_ID,
            uid(T1),
        )]));
        assert_eq!(
            scope.all_uuid_values_for(pep_properties::OWNER_TENANT_ID),
            &[uid(T1)]
        );
    }

    #[test]
    fn all_values_for_works_with_mixed_eq_and_in() {
        let scope = AccessScope::from_constraints(vec![
            ScopeConstraint::new(vec![ScopeFilter::eq(
                pep_properties::OWNER_TENANT_ID,
                uid(T1),
            )]),
            ScopeConstraint::new(vec![ScopeFilter::in_uuids(
                pep_properties::OWNER_TENANT_ID,
                vec![uid(T2)],
            )]),
        ]);
        let values = scope.all_uuid_values_for(pep_properties::OWNER_TENANT_ID);
        assert_eq!(values, &[uid(T1), uid(T2)]);
    }

    #[test]
    fn contains_value_works_with_eq() {
        let scope = AccessScope::single(ScopeConstraint::new(vec![ScopeFilter::eq(
            pep_properties::OWNER_TENANT_ID,
            uid(T1),
        )]));
        assert!(scope.contains_uuid(pep_properties::OWNER_TENANT_ID, uid(T1)));
        assert!(!scope.contains_uuid(pep_properties::OWNER_TENANT_ID, uid(T2)));
    }

    // --- tenant_only ---

    #[test]
    fn tenant_only_strips_owner_id() {
        let scope = AccessScope::single(ScopeConstraint::new(vec![
            ScopeFilter::eq(pep_properties::OWNER_TENANT_ID, uid(T1)),
            ScopeFilter::eq(pep_properties::OWNER_ID, uid(T2)),
        ]));

        let tenant_scope = scope.tenant_only();
        assert!(tenant_scope.contains_uuid(pep_properties::OWNER_TENANT_ID, uid(T1)));
        assert!(!tenant_scope.has_property(pep_properties::OWNER_ID));
    }

    #[test]
    fn tenant_only_unconstrained_becomes_deny_all() {
        let scope = AccessScope::allow_all();
        let tenant_scope = scope.tenant_only();
        assert!(tenant_scope.is_deny_all());
    }

    #[test]
    fn tenant_only_deny_all_when_no_tenant_filters() {
        let scope = AccessScope::single(ScopeConstraint::new(vec![ScopeFilter::eq(
            pep_properties::OWNER_ID,
            uid(T1),
        )]));

        let tenant_scope = scope.tenant_only();
        assert!(tenant_scope.is_deny_all());
    }

    #[test]
    fn tenant_only_on_deny_all_stays_deny_all() {
        let scope = AccessScope::deny_all();
        let tenant_scope = scope.tenant_only();
        assert!(tenant_scope.is_deny_all());
    }

    // --- tenant_and_owner ---

    #[test]
    fn tenant_and_owner_keeps_both_properties() {
        let scope = AccessScope::single(ScopeConstraint::new(vec![
            ScopeFilter::eq(pep_properties::OWNER_TENANT_ID, uid(T1)),
            ScopeFilter::eq(pep_properties::OWNER_ID, uid(T2)),
            ScopeFilter::eq(pep_properties::RESOURCE_ID, uid(T1)),
        ]));

        let narrowed = scope.tenant_and_owner();
        assert!(narrowed.contains_uuid(pep_properties::OWNER_TENANT_ID, uid(T1)));
        assert!(narrowed.contains_uuid(pep_properties::OWNER_ID, uid(T2)));
        assert!(!narrowed.has_property(pep_properties::RESOURCE_ID));
    }

    #[test]
    fn tenant_and_owner_unconstrained_becomes_deny_all() {
        let scope = AccessScope::allow_all();
        assert!(scope.tenant_and_owner().is_deny_all());
    }

    #[test]
    fn tenant_and_owner_deny_all_when_no_matching_filters() {
        let scope = AccessScope::single(ScopeConstraint::new(vec![ScopeFilter::eq(
            pep_properties::RESOURCE_ID,
            uid(T1),
        )]));
        assert!(scope.tenant_and_owner().is_deny_all());
    }

    // --- ensure_owner ---

    #[test]
    fn ensure_owner_adds_owner_when_missing() {
        let scope = AccessScope::for_tenant(uid(T1));
        let owner_id = uid(T2);

        let scoped = scope.ensure_owner(owner_id);
        assert!(scoped.contains_uuid(pep_properties::OWNER_TENANT_ID, uid(T1)));
        assert!(scoped.contains_uuid(pep_properties::OWNER_ID, owner_id));
    }

    #[test]
    fn ensure_owner_keeps_existing_owner() {
        let existing_owner = uid(T2);
        let scope = AccessScope::single(ScopeConstraint::new(vec![
            ScopeFilter::eq(pep_properties::OWNER_TENANT_ID, uid(T1)),
            ScopeFilter::eq(pep_properties::OWNER_ID, existing_owner),
        ]));

        let scoped = scope.ensure_owner(existing_owner);
        assert_eq!(
            scoped.all_uuid_values_for(pep_properties::OWNER_ID),
            &[existing_owner]
        );
    }

    #[test]
    fn ensure_owner_on_unconstrained_creates_owner_scope() {
        let scope = AccessScope::allow_all();
        let owner_id = uid(T1);

        let scoped = scope.ensure_owner(owner_id);
        assert!(!scoped.is_unconstrained());
        assert!(scoped.contains_uuid(pep_properties::OWNER_ID, owner_id));
    }

    #[test]
    fn ensure_owner_on_deny_all_stays_deny_all() {
        let scope = AccessScope::deny_all();
        let scoped = scope.ensure_owner(uid(T1));
        assert!(scoped.is_deny_all());
    }

    #[test]
    fn ensure_owner_narrows_existing_owner_to_subject() {
        let user_a = uid(T1);
        let user_b = uid(T2);
        let scope = AccessScope::single(ScopeConstraint::new(vec![
            ScopeFilter::eq(pep_properties::OWNER_TENANT_ID, uid(T1)),
            ScopeFilter::in_uuids(pep_properties::OWNER_ID, vec![user_a, user_b]),
        ]));

        let scoped = scope.ensure_owner(user_a);
        assert_eq!(
            scoped.all_uuid_values_for(pep_properties::OWNER_ID),
            &[user_a],
            "Must narrow to exactly the subject's owner_id"
        );
        assert!(scoped.contains_uuid(pep_properties::OWNER_TENANT_ID, uid(T1)));
    }

    #[test]
    fn ensure_owner_drops_constraint_when_subject_not_in_pdp() {
        let user_x = uid(T1);
        let user_y = uid(T2);
        let scope = AccessScope::single(ScopeConstraint::new(vec![
            ScopeFilter::eq(pep_properties::OWNER_TENANT_ID, uid(T1)),
            ScopeFilter::eq(pep_properties::OWNER_ID, user_x),
        ]));

        let scoped = scope.ensure_owner(user_y);
        assert!(
            scoped.is_deny_all(),
            "Must be deny-all when subject not in PDP's owner set"
        );
    }

    #[test]
    fn ensure_owner_checks_all_owner_filters_in_constraint() {
        let alice = uid(T1);
        let bob = uid(T2);
        // Contrived: two owner_id filters in one constraint.
        // alice is in the first but not the second → must be dropped.
        let scope = AccessScope::single(ScopeConstraint::new(vec![
            ScopeFilter::in_uuids(pep_properties::OWNER_ID, vec![alice, bob]),
            ScopeFilter::in_uuids(pep_properties::OWNER_ID, vec![bob]),
        ]));

        let scoped = scope.ensure_owner(alice);
        assert!(
            scoped.is_deny_all(),
            "Must deny when subject is missing from any owner_id filter"
        );

        // bob is in both → should pass and narrow to Eq.
        let scoped = scope.ensure_owner(bob);
        assert!(!scoped.is_deny_all());
        assert_eq!(
            scoped.all_uuid_values_for(pep_properties::OWNER_ID),
            &[bob],
            "Must narrow to single Eq for the matching owner"
        );
    }

    #[test]
    fn ensure_owner_multi_constraint_keeps_only_matching() {
        let alice = uid(T1);
        let bob = uid(T2);
        let tenant = uid(T1);

        // Constraint 1: tenant + alice → matches alice
        let c1 = ScopeConstraint::new(vec![
            ScopeFilter::eq(pep_properties::OWNER_TENANT_ID, tenant),
            ScopeFilter::eq(pep_properties::OWNER_ID, alice),
        ]);
        // Constraint 2: tenant + bob → does NOT match alice
        let c2 = ScopeConstraint::new(vec![
            ScopeFilter::eq(pep_properties::OWNER_TENANT_ID, tenant),
            ScopeFilter::eq(pep_properties::OWNER_ID, bob),
        ]);

        let scope = AccessScope::from_constraints(vec![c1, c2]);
        let scoped = scope.ensure_owner(alice);

        assert!(
            !scoped.is_deny_all(),
            "Must not be deny-all - one constraint matches"
        );
        assert_eq!(
            scoped.all_uuid_values_for(pep_properties::OWNER_ID),
            &[alice],
            "Must keep only the constraint matching alice"
        );
        assert!(
            scoped.contains_uuid(pep_properties::OWNER_TENANT_ID, tenant),
            "Tenant filter must be preserved"
        );
    }

    // --- ScopeFilter::InGroup ---

    const MEMBER_TYPE: &str = "gts.cf.core.rg.type.v1~example.core.rg.member.v1~";

    #[test]
    fn scope_filter_in_group_constructor() {
        let f = ScopeFilter::in_group_typed(
            pep_properties::OWNER_TENANT_ID,
            MEMBER_TYPE,
            vec![ScopeValue::Uuid(uid(T1))],
        );
        assert_eq!(f.property(), pep_properties::OWNER_TENANT_ID);
        assert!(matches!(f, ScopeFilter::InGroup(_)));
        assert_eq!(f.values().iter().count(), 0);
        let ScopeFilter::InGroup(filter) = &f else {
            unreachable!("variant asserted above")
        };
        assert_eq!(filter.membership_resource_type(), MEMBER_TYPE);
    }

    #[test]
    #[allow(deprecated)]
    fn legacy_in_group_constructor_is_untyped() {
        let f = ScopeFilter::in_group(
            pep_properties::OWNER_TENANT_ID,
            vec![ScopeValue::Uuid(uid(T1))],
        );
        let ScopeFilter::InGroup(filter) = f else {
            unreachable!("constructor always returns InGroup")
        };
        assert!(filter.membership_resource_type().is_empty());
    }

    // --- ScopeFilter::InGroupSubtree ---

    #[test]
    fn scope_filter_in_group_subtree_constructor() {
        let f = ScopeFilter::in_group_subtree_typed(
            pep_properties::OWNER_TENANT_ID,
            MEMBER_TYPE,
            vec![ScopeValue::Uuid(uid(T1))],
        );
        assert_eq!(f.property(), pep_properties::OWNER_TENANT_ID);
        assert!(matches!(f, ScopeFilter::InGroupSubtree(_)));
        assert_eq!(f.values().iter().count(), 0);
        let ScopeFilter::InGroupSubtree(filter) = &f else {
            unreachable!("variant asserted above")
        };
        assert_eq!(filter.membership_resource_type(), MEMBER_TYPE);
    }

    #[test]
    #[allow(deprecated)]
    fn legacy_in_group_subtree_constructor_is_untyped() {
        let f = ScopeFilter::in_group_subtree(
            pep_properties::OWNER_TENANT_ID,
            vec![ScopeValue::Uuid(uid(T1))],
        );
        let ScopeFilter::InGroupSubtree(filter) = f else {
            unreachable!("constructor always returns InGroupSubtree")
        };
        assert!(filter.membership_resource_type().is_empty());
    }

    #[test]
    fn in_group_scope_contains_uuid_returns_false() {
        let scope = AccessScope::single(ScopeConstraint::new(vec![ScopeFilter::in_group_typed(
            pep_properties::OWNER_TENANT_ID,
            MEMBER_TYPE,
            vec![ScopeValue::Uuid(uid(T1))],
        )]));
        assert!(!scope.contains_uuid(pep_properties::OWNER_TENANT_ID, uid(T1)));
    }

    #[test]
    fn in_group_subtree_scope_contains_uuid_returns_false() {
        let scope = AccessScope::single(ScopeConstraint::new(vec![
            ScopeFilter::in_group_subtree_typed(
                pep_properties::OWNER_TENANT_ID,
                MEMBER_TYPE,
                vec![ScopeValue::Uuid(uid(T1))],
            ),
        ]));
        assert!(!scope.contains_uuid(pep_properties::OWNER_TENANT_ID, uid(T1)));
    }

    // --- ScopeFilter::InTenantSubtree ---

    #[test]
    fn scope_filter_in_tenant_subtree_constructor_defaults_to_respect() {
        let f = ScopeFilter::in_tenant_subtree(
            pep_properties::RESOURCE_ID,
            ScopeValue::Uuid(uid(T1)),
            true,
            Vec::new(),
        );
        assert_eq!(f.property(), pep_properties::RESOURCE_ID);
        assert!(matches!(f, ScopeFilter::InTenantSubtree(_)));
        assert_eq!(f.values().iter().count(), 0);
        match &f {
            ScopeFilter::InTenantSubtree(sf) => {
                assert!(sf.respect_barriers());
                assert!(sf.descendant_status().is_empty());
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn in_tenant_subtree_scope_contains_uuid_returns_false() {
        let scope =
            AccessScope::single(ScopeConstraint::new(vec![ScopeFilter::in_tenant_subtree(
                pep_properties::RESOURCE_ID,
                ScopeValue::Uuid(uid(T1)),
                true,
                Vec::new(),
            )]));
        assert!(!scope.contains_uuid(pep_properties::RESOURCE_ID, uid(T1)));
    }

    #[test]
    fn in_tenant_subtree_scope_filter_carries_descendant_status() {
        let f = InTenantSubtreeScopeFilter::with_descendant_status(
            pep_properties::OWNER_TENANT_ID,
            ScopeValue::Uuid(uid(T1)),
            true,
            vec![ScopeValue::Int(1), ScopeValue::Int(2)],
        );
        assert_eq!(f.descendant_status().len(), 2);
        assert_eq!(f.descendant_status()[0], ScopeValue::Int(1));
        assert_eq!(f.descendant_status()[1], ScopeValue::Int(2));
    }

    #[test]
    fn in_tenant_subtree_scope_filter_exposes_property_and_root() {
        let f =
            InTenantSubtreeScopeFilter::new(pep_properties::RESOURCE_ID, ScopeValue::Uuid(uid(T1)));
        assert_eq!(f.property(), pep_properties::RESOURCE_ID);
        assert_eq!(f.root_tenant_id(), &ScopeValue::Uuid(uid(T1)));
        assert!(f.respect_barriers());
    }

    #[test]
    fn in_tenant_subtree_scope_filter_ignore_barriers_constructor() {
        let f = InTenantSubtreeScopeFilter::with_respect_barriers(
            pep_properties::OWNER_TENANT_ID,
            ScopeValue::Uuid(uid(T1)),
            false,
        );
        assert!(!f.respect_barriers());
    }

    #[test]
    fn tenant_tables_constants_are_stable() {
        assert_eq!(tenant_tables::CLOSURE_TABLE, "tenant_closure");
        assert_eq!(tenant_tables::CLOSURE_ANCESTOR_ID, "ancestor_id");
        assert_eq!(tenant_tables::CLOSURE_DESCENDANT_ID, "descendant_id");
        assert_eq!(tenant_tables::CLOSURE_BARRIER, "barrier");
    }

    // --- contains_uuid string matching ---

    #[test]
    fn contains_uuid_matches_string_variant() {
        let scope = AccessScope::single(ScopeConstraint::new(vec![ScopeFilter::eq(
            pep_properties::OWNER_TENANT_ID,
            ScopeValue::String(T1.to_owned()),
        )]));
        assert!(scope.contains_uuid(pep_properties::OWNER_TENANT_ID, uid(T1)));
        assert!(!scope.contains_uuid(pep_properties::OWNER_TENANT_ID, uid(T2)));
    }

    #[test]
    fn contains_uuid_does_not_match_invalid_string() {
        let scope = AccessScope::single(ScopeConstraint::new(vec![ScopeFilter::eq(
            pep_properties::OWNER_TENANT_ID,
            ScopeValue::String("not-a-uuid".to_owned()),
        )]));
        assert!(!scope.contains_uuid(pep_properties::OWNER_TENANT_ID, uid(T1)));
    }
}
