//! Domain-layer repository contract for `role_assignments`.
//!
//! Handlers depend on this trait. Unit tests substitute hand-rolled
//! trait stubs via the `stub_impl!` macro in `crate::test_support`; the
//! production SeaORM-backed implementation lives in `infra/storage`.

use std::collections::HashMap;

use async_trait::async_trait;
use rbac_sdk::models::PrincipalType;
use toolkit_db::secure::DBRunner;
use toolkit_macros::domain_model;
use toolkit_odata::{ODataQuery, Page};
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::model::RoleAssignmentModel;
use crate::domain::policy_enforcer::ReadableScopes;

/// Required inputs for [`RoleAssignmentRepository::create`].
///
/// The repository mints `id` (`UUIDv7`) and stamps `created_at` /
/// `updated_at` itself so all writers share the same clock-truncation
/// convention.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewRoleAssignment {
    pub role_definition_id: Uuid,
    pub principal_id: String,
    pub principal_type: PrincipalType,
    /// Typed scope. Serialised to its canonical path form at the
    /// storage boundary (`Scope::path()`).
    pub scope: rbac_sdk::models::Scope,
    pub created_by: String,
    /// Kind of the principal named by [`Self::created_by`]. The service
    /// stamps it from the caller's `SecurityContext`; `None` is reserved for
    /// writers that genuinely have no user identity to record (the platform
    /// bootstrap's root-scope row), and reads that column back as "no
    /// author name available".
    pub created_by_type: Option<PrincipalType>,
    /// Home tenant of the principal named by [`Self::created_by`] — the
    /// tenant a reader must ask to resolve that subject id to a name. `None`
    /// under the same conditions as [`Self::created_by_type`].
    pub created_by_tenant_id: Option<Uuid>,
}

/// Caller-visibility narrowing applied on top of the caller-supplied
/// `OData` `$filter`. Kept as a typed enum so the authorization-derived
/// scope set never gets conflated with the caller-supplied filter.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VisibilityFilter {
    /// Caller has read access at no scope — list yields empty.
    None,
    /// Caller has read access under each of these scope prefixes
    /// (descendant semantics: `scope == prefix` OR
    /// `scope.starts_with(prefix + "/")`).
    Subtrees(Vec<String>),
    /// Caller has read access everywhere (root-equivalent).
    Unrestricted,
}

/// Hard cap on the `Subtrees` prefix set handed to the repo's visibility
/// filter. `readable_scopes` accumulates one prefix per readable scope, and
/// the count is attacker-influenceable (a subject with many tenant / RG read
/// grants). Bound it so the downstream `WHERE tenant_id IN (…)` / prefix
/// list cannot grow without limit; mirrors
/// `role_definition::service::ALLOWED_TENANTS_CAP`.
pub const ALLOWED_SCOPE_PREFIXES_CAP: usize = 1024;

impl VisibilityFilter {
    /// Project the enforcer's [`ReadableScopes`] onto the row-visibility
    /// filter this repository understands, rejecting an over-sized prefix
    /// set.
    ///
    /// This lives here, next to the enum, because **two** endpoints now need
    /// the identical projection: the role-assignment list, and the
    /// per-role assignment count the role-definition reads carry. Copying
    /// the match — or worse, re-declaring the cap — would let the count
    /// drift away from the list it is supposed to agree with, and the whole
    /// point of that number is that a caller can reproduce it by paging the
    /// list themselves.
    ///
    /// # Errors
    ///
    /// [`DomainError::Validation`] when the prefix set exceeds
    /// [`ALLOWED_SCOPE_PREFIXES_CAP`]. The caller is told how to narrow the
    /// request rather than being served a silently truncated row set.
    pub fn from_readable_scopes(readable: ReadableScopes) -> Result<Self, DomainError> {
        match readable {
            ReadableScopes::None => Ok(Self::None),
            ReadableScopes::Unrestricted => Ok(Self::Unrestricted),
            ReadableScopes::Subtrees(prefixes) => {
                if prefixes.len() > ALLOWED_SCOPE_PREFIXES_CAP {
                    return Err(DomainError::Validation {
                        detail: format!(
                            "list visibility set exceeds {ALLOWED_SCOPE_PREFIXES_CAP}; \
                             refine via $filter (e.g. principal_id, scope)"
                        ),
                    });
                }
                Ok(Self::Subtrees(prefixes))
            }
        }
    }
}

/// Inputs for [`RoleAssignmentRepository::get_subject_assignments`] —
/// the evaluator-facing read path.
///
/// * `user_principal` — `(principal_type, principal_id)` for the
///   calling subject. `None` skips the user-principal branch.
/// * `group_principals` — group ids resolved upstream; empty vector
///   skips the group branch (the SQL MUST NOT emit
///   `principal_id = ANY('{}')`).
/// * `ancestor_scopes` — Phase 1 `IN` list: `/`, ancestor tenant
///   scopes, and the context tenant scope.
/// * `context_tenant_rg_prefix` — Phase 2 `LIKE` pattern, typically
///   `/tenants/{context_tenant_id}/resourceGroups/%`.
/// * `all_scopes` — when `true`, match by principal only and IGNORE
///   `ancestor_scopes` / `context_tenant_rg_prefix`. Used for the
///   root-context list, which aggregates a subject's read grants across
///   *every* tenant rather than just the home-tenant ancestor chain.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubjectAssignmentsQuery {
    pub user_principal: Option<(PrincipalType, String)>,
    pub group_principals: Vec<String>,
    pub ancestor_scopes: Vec<String>,
    pub context_tenant_rg_prefix: String,
    pub all_scopes: bool,
}

/// Domain-layer repository contract for `role_assignments`.
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
pub trait RoleAssignmentRepository: Send + Sync + 'static {
    async fn create<C: DBRunner>(
        &self,
        db: &C,
        new: NewRoleAssignment,
    ) -> Result<RoleAssignmentModel, DomainError>;

    async fn find_by_id<C: DBRunner>(
        &self,
        db: &C,
        id: Uuid,
    ) -> Result<Option<RoleAssignmentModel>, DomainError>;

    /// Cursor-paginated list driven by a caller-supplied `OData` query
    /// (`$filter`, `$orderby`, `limit`, `cursor`), narrowed by the
    /// caller-derived `visibility` predicate. Returns an empty page
    /// when `visibility` is `None`.
    async fn list<C: DBRunner>(
        &self,
        db: &C,
        visibility: VisibilityFilter,
        query: &ODataQuery,
    ) -> Result<Page<RoleAssignmentModel>, DomainError>;

    /// Evaluator-facing read: every assignment matching the two-phase
    /// scope predicate, ordered `(scope_depth DESC, id DESC)`. No
    /// pagination or visibility narrowing — the evaluator unions grants
    /// across every applicable assignment.
    async fn get_subject_assignments<C: DBRunner>(
        &self,
        db: &C,
        query: SubjectAssignmentsQuery,
    ) -> Result<Vec<RoleAssignmentModel>, DomainError>;

    /// Hard delete by id. Returns `true` when a row was removed,
    /// `false` when no row matched (race with another delete).
    async fn delete<C: DBRunner>(&self, db: &C, id: Uuid) -> Result<bool, DomainError>;

    /// How many assignments reference each of `ids`, counted over exactly
    /// the rows `visibility` admits — one `GROUP BY role_definition_id`
    /// query per id chunk, never one query per id.
    ///
    /// The `visibility` argument is not decoration. A count taken over every
    /// row in the table would tell a tenant admin how many grants of a
    /// built-in role exist platform-wide, which is a fact about other
    /// tenants' size and activity. Counting under the caller's own readable
    /// scopes makes the number equal what the caller would get by paging
    /// [`Self::list`] with `role_definition_id eq <id>` themselves.
    ///
    /// A role with no visible assignments is **absent** from the returned
    /// map rather than present with `0`: the repository reports what the
    /// `GROUP BY` produced, and it is the caller that decides whether an
    /// absent role means "zero" (it does, when the caller has visibility at
    /// all) or "unknown".
    ///
    /// An empty `ids` slice MUST return an empty map without touching the
    /// DB — `WHERE role_definition_id IN ()` is a syntax error on every
    /// dialect we ship — and so MUST `VisibilityFilter::None`, which admits
    /// no rows at all.
    ///
    /// Consistency with a page of rows read alongside it is the **caller's**
    /// choice: pass the same `DbTx` to both and the count agrees with the
    /// page; pass a plain connection and assignments can be created or
    /// deleted between the two statements. The read paths deliberately take
    /// the second option — a display count does not justify holding a
    /// transaction open across a page read.
    async fn count_by_role<C: DBRunner>(
        &self,
        db: &C,
        visibility: VisibilityFilter,
        ids: &[Uuid],
    ) -> Result<HashMap<Uuid, u64>, DomainError>;
}
