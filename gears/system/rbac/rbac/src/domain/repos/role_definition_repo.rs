//! Domain-layer repository contract for `role_definitions`.
//!
//! Handlers depend on this trait. Unit tests substitute hand-rolled
//! trait stubs via the `stub_impl!` macro in `crate::test_support`; the
//! production SeaORM-backed implementation lives in `infra/storage`.

use async_trait::async_trait;
use rbac_sdk::models::{PermissionRule, Scope};
use toolkit_db::secure::DBRunner;
use toolkit_macros::domain_model;
use toolkit_odata::{ODataQuery, Page};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::etag::Etag;
use crate::domain::model::RoleDefinitionModel;
use crate::domain::policy_enforcer::{AuthorizationError, PolicyEnforcer, ReadableScopes};
use crate::domain::principal_type_resolver::principal_type_from_security_context;
use crate::domain::resource_types;

/// Required inputs for [`RoleDefinitionRepository::create`].
///
/// The caller supplies the `UUIDv7` — the repository is agnostic to the
/// id-generation policy. Timestamps are stamped by the repository so
/// all writers share the same clock-truncation convention.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewRoleDefinition {
    pub id: Uuid,
    pub name: String,
    pub description: Option<String>,
    /// Allow rules — written verbatim to the `permissions` JSONB column.
    pub permissions: Vec<PermissionRule>,
    /// Deny rules — written verbatim to the `not_permissions` JSONB column.
    pub not_permissions: Vec<PermissionRule>,
    /// Typed scopes; serialised to canonical path form
    /// ([`Scope::path`]) at the storage boundary.
    pub assignable_scopes: Vec<Scope>,
    /// Custom roles MUST carry an owner; built-ins are seeded through
    /// the dedicated seeder path, not through this repository.
    pub owner_tenant_id: Uuid,
    pub created_by: String,
}

/// Partial-update payload for [`RoleDefinitionRepository::update`].
///
/// Each `Option` field: `None` = unchanged, `Some(v)` = set. The
/// `description` field uses a double-Option because the column is
/// nullable: `None` = unchanged, `Some(None)` = clear, `Some(Some(s))`
/// = set.
#[domain_model]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RoleDefinitionPatch {
    pub name: Option<String>,
    /// Nullable column: `None` = unchanged, `Some(None)` = clear,
    /// `Some(Some(s))` = set. The double `Option` is intentional.
    #[allow(clippy::option_option)]
    pub description: Option<Option<String>>,
    /// Allow rules. `None` = unchanged; `Some(vec)` = replace the
    /// `permissions` array wholesale (empty vec clears it).
    pub permissions: Option<Vec<PermissionRule>>,
    /// Deny rules. `None` = unchanged; `Some(vec)` = replace the
    /// `not_permissions` array wholesale.
    pub not_permissions: Option<Vec<PermissionRule>>,
    /// Typed replacement set. `None` = unchanged; `Some(vec)` = replace
    /// wholesale (serialised to canonical path strings on write).
    pub assignable_scopes: Option<Vec<Scope>>,
}

impl RoleDefinitionPatch {
    /// `true` when every field is `None` — used by the update path to
    /// detect a no-op patch (`updated_at` MUST still advance even on a
    /// no-op patch).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.name.is_none()
            && self.description.is_none()
            && self.permissions.is_none()
            && self.not_permissions.is_none()
            && self.assignable_scopes.is_none()
    }
}

/// Auth-derived visibility narrowing applied on top of the caller's
/// `$filter`. Distinct from the user-supplied `$filter` so the
/// authorization-derived row set never gets conflated with the wire
/// payload.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoleDefinitionVisibility {
    /// Built-in rows only (`is_built_in = true`).
    BuiltinsOnly,
    /// Custom rows owned by any of the listed tenants. An empty
    /// `Vec` means the caller can read no custom rows; the repo
    /// returns an empty page. Does NOT include built-ins — used where
    /// the caller explicitly wants only custom rows.
    CustomForTenants(Vec<Uuid>),
    /// Built-in rows (always visible to any authenticated caller)
    /// PLUS custom rows owned by any of the listed tenants. This is the
    /// tenant-admin list view: a tenant-scoped caller sees the built-in
    /// catalog and their own tenant's custom roles. An empty `Vec`
    /// degrades to built-ins only.
    CustomForTenantsWithBuiltins(Vec<Uuid>),
    /// All rows (built-in + custom) — used when the caller has
    /// unrestricted read on the resource type. Built-ins surface
    /// first because the repo defaults `$orderby` to
    /// `is_built_in desc, id desc` when none is supplied.
    All,
}

/// Row counts split by role kind, over whatever row set a
/// [`RoleDefinitionVisibility`] admits.
///
/// Two buckets and no `total` field: the total is `built_in + custom` by
/// construction, so storing it would be a second source of truth that a
/// future edit could leave disagreeing with its own parts. Callers derive it
/// through [`Self::total`].
#[domain_model]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RoleTypeCounts {
    /// Platform-seeded built-in roles (`is_built_in = true`).
    pub built_in: u64,
    /// Tenant-owned custom roles (`is_built_in = false`).
    pub custom: u64,
}

impl RoleTypeCounts {
    /// Both buckets summed. `saturating_add` rather than `+` because a
    /// counts endpoint must not be the thing that panics a read path, and
    /// there is no meaningful answer above `u64::MAX` anyway.
    #[must_use]
    pub fn total(&self) -> u64 {
        self.built_in.saturating_add(self.custom)
    }
}

/// Domain-layer repository contract for `role_definitions`.
///
/// Errors flow through [`DomainError`] so callers never see
/// `sea_orm::DbErr` or storage-specific enums.
///
/// Every method takes the executor as `db: &C where C: DBRunner`, so the
/// same method body runs on a plain connection or inside a transaction —
/// `DbConn` and `DbTx` both implement [`DBRunner`]. The caller owns the
/// transaction boundary; the repository never acquires a connection of its
/// own. This is the repository shape
/// `docs/toolkit_unified_system/11_database_patterns.md` mandates.
#[async_trait]
pub trait RoleDefinitionRepository: Send + Sync + 'static {
    async fn create<C: DBRunner>(
        &self,
        db: &C,
        new: NewRoleDefinition,
    ) -> Result<RoleDefinitionModel, DomainError>;

    async fn find_by_id<C: DBRunner>(
        &self,
        db: &C,
        id: Uuid,
    ) -> Result<Option<RoleDefinitionModel>, DomainError>;

    /// Batched lookup. Returns every row whose `id` is in `ids`, in any
    /// order — the caller indexes by `id`. Missing rows are silently
    /// absent (e.g. an `assignment.role_definition_id` whose target was
    /// deleted under the FK-restrict race window — the caller surfaces
    /// this as `Internal`).
    ///
    /// Empty `ids` MUST return `Ok(Vec::new())` without touching the
    /// DB: `WHERE id IN ()` is a syntax error on every dialect.
    async fn find_by_ids<C: DBRunner>(
        &self,
        db: &C,
        ids: &[Uuid],
    ) -> Result<Vec<RoleDefinitionModel>, DomainError>;

    /// [`Self::find_by_ids`] narrowed by the caller's own
    /// [`RoleDefinitionVisibility`].
    ///
    /// Exists because `find_by_ids` deliberately reads with
    /// `AccessScope::allow_all()` and applies no visibility at all: it
    /// backs the create path, where the row must be found before the
    /// caller is told anything about it. Reusing it to *display* a role
    /// name would hand a descendant-tenant admin the name of an
    /// ancestor-owned custom role that `GET /rbac/v1/role-definitions/{id}`
    /// answers `404` for — an ancestor admin may grant such a role at a
    /// descendant scope, and the descendant admin may then read the
    /// assignment row. Same rows, same chunking, one extra predicate.
    ///
    /// Built-ins stay visible to every authenticated caller, so a
    /// tenant-scoped caller keeps seeing built-in role names.
    ///
    /// The default implementation filters
    /// [`Self::find_by_ids`] in memory through [`visibility_admits`],
    /// which is correct for any implementation and cheap at the sizes
    /// this is called with (the distinct role ids on one page). A storage
    /// implementation SHOULD override it with the same SQL predicate
    /// [`Self::list`] uses, so the narrowing happens in the database.
    ///
    /// # Errors
    ///
    /// Whatever [`Self::find_by_ids`] returns.
    async fn find_by_ids_visible<C: DBRunner>(
        &self,
        db: &C,
        visibility: RoleDefinitionVisibility,
        ids: &[Uuid],
    ) -> Result<Vec<RoleDefinitionModel>, DomainError> {
        let rows = self.find_by_ids(db, ids).await?;
        // `move`: the predicate owns the visibility for the length of the
        // filter, so the parameter is consumed rather than borrowed.
        Ok(rows
            .into_iter()
            .filter(move |row| visibility_admits(&visibility, row))
            .collect())
    }

    /// Cursor-paginated list driven by a caller-supplied `OData` query
    /// (`$filter`, `$orderby`, `limit`, `cursor`), narrowed by the
    /// caller-derived `visibility` predicate.
    async fn list<C: DBRunner>(
        &self,
        db: &C,
        visibility: RoleDefinitionVisibility,
        query: &ODataQuery,
    ) -> Result<Page<RoleDefinitionModel>, DomainError>;

    async fn update<C: DBRunner>(
        &self,
        db: &C,
        id: Uuid,
        patch: RoleDefinitionPatch,
        expected_etag: &Etag,
    ) -> Result<RoleDefinitionModel, DomainError>;

    async fn delete<C: DBRunner>(
        &self,
        db: &C,
        id: Uuid,
        expected_etag: &Etag,
    ) -> Result<(), DomainError>;

    async fn count_assignments_for_role<C: DBRunner>(
        &self,
        db: &C,
        id: Uuid,
    ) -> Result<u64, DomainError>;

    /// Built-in / custom row counts over the same `visibility` predicate the
    /// list endpoint narrows with — one `GROUP BY is_built_in` round trip.
    ///
    /// Sharing the predicate with [`Self::list`] is the whole contract: the
    /// catalog UI renders these numbers next to rows the caller can actually
    /// page to, so a count that admitted a row the list hides would be a
    /// disclosure, and one that hid a row the list shows would be a bug the
    /// user reports as "the tab badge is wrong".
    ///
    /// Takes no `$filter`: this is a plain summary of what the caller may
    /// see, not a facet over an arbitrary query.
    async fn count_by_type<C: DBRunner>(
        &self,
        db: &C,
        visibility: RoleDefinitionVisibility,
    ) -> Result<RoleTypeCounts, DomainError>;
}

/// Does `visibility` admit this row?
///
/// The in-memory twin of the SQL predicate the storage layer builds for
/// [`RoleDefinitionRepository::list`] / `count_by_type`. Kept here, next
/// to the enum, so the two stay readable side by side: any divergence
/// between them is a row a caller can see in one place and not the other.
#[must_use]
pub fn visibility_admits(visibility: &RoleDefinitionVisibility, row: &RoleDefinitionModel) -> bool {
    match visibility {
        RoleDefinitionVisibility::BuiltinsOnly => row.is_built_in,
        RoleDefinitionVisibility::CustomForTenants(tenants) => {
            !row.is_built_in && row.owner_tenant_id.is_some_and(|t| tenants.contains(&t))
        }
        // Built-ins are unconditionally visible to every authenticated
        // caller; custom rows only for the readable tenants.
        RoleDefinitionVisibility::CustomForTenantsWithBuiltins(tenants) => {
            row.is_built_in || row.owner_tenant_id.is_some_and(|t| tenants.contains(&t))
        }
        RoleDefinitionVisibility::All => true,
    }
}

/// Derive the caller's [`RoleDefinitionVisibility`] from the policy
/// enforcer.
///
/// Takes the enforcer and a `context_scope` rather than a whole service,
/// so both the role-definition read path and the role-assignment read
/// path (which resolves `role_definition_name` and must not disclose a
/// name the catalog would hide) can share one derivation.
///
/// **Duplicated logic, deliberately visible:**
/// `crate::domain::role_definition::service::RoleDefinitionService::role_definition_visibility`
/// computes the same thing from a `CallerScope`. The two MUST agree —
/// they answer the same question, "which role definitions may this caller
/// see" — and converging them onto this function is the intended end
/// state; the copy exists only because the two landed in parallel. If you
/// change the mapping here, change it there.
///
/// # Errors
///
/// [`DomainError::Validation`] when the caller's readable-tenant set is
/// too large to push into a `WHERE` clause, and [`DomainError::internal`]
/// for an enforcer failure or an unparseable scope prefix. Every caller
/// on a *display* path MUST treat an error as "resolve no names", never
/// as a failed read.
pub async fn derive_role_definition_visibility(
    policy: &dyn PolicyEnforcer,
    ctx: &SecurityContext,
    context_scope: &Scope,
) -> Result<RoleDefinitionVisibility, DomainError> {
    let subject = ctx.subject_id().to_string();
    let principal_type = principal_type_from_security_context(ctx)?;

    let readable = policy
        .readable_scopes(
            ctx,
            &subject,
            principal_type,
            resource_types::ROLE_DEFINITION,
            context_scope,
        )
        .await
        .map_err(|e| match e {
            AuthorizationError::Denied => {
                DomainError::internal("readable_scopes returned Denied (unexpected)")
            }
            AuthorizationError::Internal(msg) => DomainError::internal(msg),
        })?;

    Ok(match readable {
        // No read anywhere → still the built-in catalog: every
        // authenticated caller may read built-in role definitions.
        ReadableScopes::None => RoleDefinitionVisibility::BuiltinsOnly,
        ReadableScopes::Unrestricted => RoleDefinitionVisibility::All,
        ReadableScopes::Subtrees(prefixes) => {
            let tenants = visibility_tenants_from_subtrees(&prefixes)?;
            if tenants.is_empty() {
                RoleDefinitionVisibility::BuiltinsOnly
            } else if tenants.len() > ROLE_DEFINITION_VISIBILITY_TENANTS_CAP {
                return Err(DomainError::Validation {
                    detail: format!(
                        "role-definition visibility set exceeds \
                         {ROLE_DEFINITION_VISIBILITY_TENANTS_CAP} tenants"
                    ),
                });
            } else {
                RoleDefinitionVisibility::CustomForTenantsWithBuiltins(tenants)
            }
        }
    })
}

/// Hard cap on the readable-tenant list derived from
/// `ReadableScopes::Subtrees`. Mirrors
/// `crate::domain::role_definition::service::ALLOWED_TENANTS_CAP` — the
/// same set feeds the same `IN (...)`, so the two must not drift.
pub const ROLE_DEFINITION_VISIBILITY_TENANTS_CAP: usize =
    crate::domain::role_definition::service::ALLOWED_TENANTS_CAP;

/// Project `ReadableScopes::Subtrees` prefixes onto tenant UUIDs,
/// preserving first-seen order and deduplicating.
///
/// Role definitions are tenant-owned, so RG-scoped and root prefixes
/// contribute no tenant. A malformed prefix is an `Internal` error rather
/// than a silent skip: silently dropping it would present as "this admin
/// can only see built-ins" and hide enforcer or registry corruption.
///
/// Same duplication note as [`derive_role_definition_visibility`]: the
/// twin is `tenant_ids_from_subtrees` in
/// `crate::domain::role_definition::service`.
fn visibility_tenants_from_subtrees(prefixes: &[String]) -> Result<Vec<Uuid>, DomainError> {
    let mut out: Vec<Uuid> = Vec::with_capacity(prefixes.len());
    let mut seen: std::collections::HashSet<Uuid> =
        std::collections::HashSet::with_capacity(prefixes.len());
    for prefix in prefixes {
        match Scope::parse(prefix) {
            Ok(Scope::Tenant { tenant_id }) => {
                if seen.insert(tenant_id) {
                    out.push(tenant_id);
                }
            }
            Ok(_) => {}
            Err(err) => {
                return Err(DomainError::internal(format!(
                    "readable_scopes returned unparseable prefix '{prefix}': {err}"
                )));
            }
        }
    }
    Ok(out)
}
