//! Role-definition aggregate service — the canonical orchestration
//! entry point.
//!
//! Owns the validate → enforce-PEP → write-to-repo flow for every
//! `role_definition` operation. Repo trait is a generic parameter so
//! tests can substitute an in-memory fake without going through
//! `SeaORM`. Returns [`DomainError`] uniformly; SDK / REST boundaries
//! translate.

use std::sync::Arc;

use rbac_sdk::models::{PermissionRule, Scope};
use toolkit_db::{DBProvider, DbError};
use toolkit_macros::domain_model;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::actions;
use crate::domain::builtin_roles_catalog::CANONICAL_BUILTIN_ROLES;
use crate::domain::error::DomainError;
use crate::domain::etag::{Etag, etag_for};
use crate::domain::model::RoleDefinitionModel;
use crate::domain::permission_matcher::validate_permission_rule;
use crate::domain::policy_enforcer::{AuthorizationError, PolicyEnforcer, ReadableScopes};
use crate::domain::principal_type_resolver::principal_type_from_security_context;
use crate::domain::resource_types;
use crate::domain::role_assignment_repo::{RoleAssignmentRepository, VisibilityFilter};
use crate::domain::role_definition_repo::{
    NewRoleDefinition, RoleDefinitionPatch, RoleDefinitionRepository, RoleDefinitionVisibility,
    RoleTypeCounts,
};
use crate::domain::scope_validator::{MissingScopeEntity, ScopeError, ScopeValidator};
use crate::domain::target_type_validator::TargetTypeValidator;
use toolkit_odata::{ODataQuery, Page};

/// Default `limit` when the caller does not supply one to `list`.
pub const DEFAULT_LIMIT: u32 = 50;
/// Maximum `limit` accepted by `list`.
pub const MAX_LIMIT: u32 = 200;
/// Hard cap on the allowed-tenants list when `readable_scopes` returns
/// `Subtrees` — callers with `read` on more than this many tenants
/// should refine via `$filter=owner_tenant_id eq <uuid>`.
pub const ALLOWED_TENANTS_CAP: usize = 1024;

/// Caller's authentication scope, normalised at the REST extractor and
/// passed through to the service so it can resolve `owner_tenant_id`.
#[domain_model]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum CallerScope {
    /// Root callers (platform admins) MUST supply `owner_tenant_id`
    /// explicitly on create.
    #[default]
    Root,
    /// Tenant-scoped caller. Any caller-supplied `owner_tenant_id`
    /// (when present) MUST equal this tenant.
    Tenant(Uuid),
}

/// `create` input. Mirrors the SDK create-request shape but owned by
/// the domain layer — the REST boundary builds this from the wire DTO.
#[domain_model]
#[derive(Debug, Clone)]
pub struct CreateRoleDefinitionRequest {
    pub caller_scope: CallerScope,
    pub name: String,
    pub description: Option<String>,
    /// Allow rules.
    pub permissions: Vec<PermissionRule>,
    /// Deny rules.
    pub not_permissions: Vec<PermissionRule>,
    /// Assignable scopes, already parsed at the REST boundary (the
    /// wire DTO carries strings; the handler parses once and forwards
    /// typed [`Scope`]s, matching the PATCH path, so the service no
    /// longer re-parses). The service still validates each against the
    /// tenant hierarchy before writing.
    pub assignable_scopes: Vec<Scope>,
    /// Optional — root callers MUST supply; tenant-scoped callers MAY
    /// omit (the service infers their tenant).
    pub owner_tenant_id: Option<Uuid>,
}

/// `update` input. `patch` carries the typed RBAC-domain patch shape;
/// REST parses scope strings to `Scope` before reaching the service so
/// format errors surface as 422 at the wire boundary.
#[domain_model]
#[derive(Debug, Clone)]
pub struct UpdateRoleDefinitionRequest {
    pub id: Uuid,
    pub if_match: Option<Etag>,
    pub patch: RoleDefinitionPatch,
    /// Name of the first immutable field (`id`, `is_built_in`,
    /// `owner_tenant_id`, `created_at`, `created_by`) the client tried
    /// to send, or `None` if the request body was clean. Detected at
    /// the REST boundary because it requires the raw DTO shape; the
    /// service checks it only after authz so an unauthorized caller
    /// can't distinguish a body-shape rejection from
    /// `RoleDefinitionNotFound`.
    pub immutable_field_attempted: Option<&'static str>,
}

/// `list` input. `filter_*` arguments come from the OData-lite parser
/// at the REST boundary.
/// `list` input. The caller's `$filter` / `$orderby` / `cursor` /
/// `limit` lives inside the [`ODataQuery`]; [`Self::caller_scope`]
/// drives the readable-scopes lookup.
#[domain_model]
#[derive(Debug, Clone, Default)]
pub struct ListRoleDefinitionsRequest {
    pub caller_scope: CallerScope,
    /// Caller-supplied `OData` query (parsed at the REST boundary).
    pub query: ODataQuery,
}

/// A role definition plus the counts a read decorates it with.
///
/// The count deliberately does **not** live on [`RoleDefinitionModel`]:
/// that type is the projection of a `role_definitions` row, and the number
/// is not a column — it is an aggregate over a different table, taken under
/// the caller's own visibility. Keeping it on a separate view type also
/// leaves every existing `RoleDefinitionModel` construction site untouched.
#[domain_model]
#[derive(Debug, Clone)]
pub struct CountedRoleDefinition {
    pub model: RoleDefinitionModel,
    /// How many role assignments reference this definition, counted over
    /// the assignments the *caller* may read.
    ///
    /// `None` and `Some(0)` mean different things, and conflating them
    /// would be the one way to make this field actively misleading:
    ///
    /// * `None` — the caller has no read visibility on role assignments
    ///   anywhere, so no honest number exists. A zero here would be a fact
    ///   about the caller's permissions that a UI would render as "this
    ///   role is unused".
    /// * `Some(0)` — the caller can see assignments, and none of the ones
    ///   they can see use this role.
    ///
    /// Also `None` on write responses (`POST` / `PATCH`), which perform no
    /// count: the creator reads the number back on the next `GET`.
    pub assignment_count: Option<u64>,
}

impl CountedRoleDefinition {
    /// Wrap a row with no count — the shape served by the write paths and
    /// by any read that could not establish the caller's assignment
    /// visibility.
    #[must_use]
    pub fn bare(model: RoleDefinitionModel) -> Self {
        Self {
            model,
            assignment_count: None,
        }
    }
}

/// Role-definition aggregate service. Generic over both repositories so
/// tests can substitute in-memory fakes.
///
/// `db` is the service's connection source: it owns the transaction
/// boundary and hands each repository call an executor, per
/// `docs/toolkit_unified_system/11_database_patterns.md`.
#[domain_model]
pub struct RoleDefinitionService<R: RoleDefinitionRepository, AR: RoleAssignmentRepository> {
    db: DBProvider<DbError>,
    repo: Arc<R>,
    /// Role-assignment store, read-only from here: role-definition reads
    /// carry a per-role assignment count, which is an aggregate over
    /// `role_assignments`.
    ///
    /// Generic rather than a trait object: the repositories take
    /// `<C: DBRunner>` method signatures, which are not dyn-compatible. That
    /// is why every `RoleDefinitionService<…>` annotation names both repos.
    assignment_repo: Arc<AR>,
    policy: Arc<dyn PolicyEnforcer>,
    scope_validator: Arc<ScopeValidator>,
    target_type_validator: Arc<dyn TargetTypeValidator>,
}

impl<R: RoleDefinitionRepository, AR: RoleAssignmentRepository> RoleDefinitionService<R, AR> {
    /// Borrow a connection for a single-statement read.
    fn conn(&self) -> Result<toolkit_db::secure::DbConn<'_>, DomainError> {
        self.db.conn().map_err(DomainError::from)
    }

    /// Construct a new service from its two repos + the three supporting
    /// ports. No I/O at construction time.
    ///
    /// The assignment repo is a required argument rather than an optional
    /// chainable extra so that `assignment_count == None` carries exactly
    /// one meaning — "the caller cannot see assignments" — instead of also
    /// meaning "this deployment forgot to wire the counter".
    #[must_use]
    pub fn new(
        db: DBProvider<DbError>,
        repo: Arc<R>,
        assignment_repo: Arc<AR>,
        policy: Arc<dyn PolicyEnforcer>,
        scope_validator: Arc<ScopeValidator>,
        target_type_validator: Arc<dyn TargetTypeValidator>,
    ) -> Self {
        Self {
            db,
            repo,
            assignment_repo,
            policy,
            scope_validator,
            target_type_validator,
        }
    }

    /// Create a custom role definition.
    ///
    /// Authz runs *before* any name-confusables check, target-type /
    /// catalog network lookup, or scope-existence check so an
    /// unauthorized caller cannot probe the platform's GTS / catalog /
    /// scope hierarchy through the differentiated 4xx responses.
    pub async fn create(
        &self,
        ctx: &SecurityContext,
        request: CreateRoleDefinitionRequest,
    ) -> Result<RoleDefinitionModel, DomainError> {
        let owner_tenant_id = resolve_owner_tenant(&request.caller_scope, request.owner_tenant_id)?;

        // Authz first. No network I/O before this point — the only
        // computation above is `resolve_owner_tenant`, which is pure.
        // Denial stays `AuthorizationDenied` (no existing resource whose
        // existence we'd leak).
        let subject = ctx.subject_id().to_string();
        let principal_type = principal_type_from_security_context(ctx)?;
        self.policy
            .enforce(
                ctx,
                &subject,
                principal_type,
                actions::WRITE,
                resource_types::ROLE_DEFINITION,
                &Scope::tenant(owner_tenant_id),
            )
            .await
            .map_err(authorization_error_for_role_definition)?;

        // Reject names containing `,` or `)` — the PG
        // `Key (col)=(val) already exists` detail string is parsed by
        // `extract_quoted_value` in `infra::canonical_mapping` to
        // recover the conflicting name from a uniqueness violation;
        // the parser splits on the first `,` or `)`, so a name
        // containing either truncates the recovered string and the
        // typed `RoleDefinitionNameTaken { name }` variant carries a
        // lie. Reject at the domain boundary so the parser's input
        // stays bounded.
        validate_name_charset(&request.name)?;

        // Confusables-aware fold so a caller cannot bypass the built-in
        // name reservation by substituting visually identical non-ASCII
        // characters (e.g. Greek capital Omicron for Latin O).
        if CANONICAL_BUILTIN_ROLES.iter().any(|b| {
            crate::domain::name_confusables::name_collides_with_builtin(&request.name, b.name)
        }) {
            return Err(DomainError::RoleDefinitionNameReservedByBuiltin { name: request.name });
        }

        // Shape + target-type validation runs first so malformed rules
        // short-circuit before the more expensive catalog/registry hits.
        // Two passes so error field paths reflect which array a bad
        // rule arrived in.
        // Shape validation stays per-rule so the error field path reflects
        // which array a bad rule arrived in.
        for (idx, rule) in request.permissions.iter().enumerate() {
            validate_permission_rule(rule).map_err(|e| DomainError::InvalidPermissionRule {
                detail: format!("permissions[{idx}]: {e}"),
            })?;
        }
        for (idx, rule) in request.not_permissions.iter().enumerate() {
            validate_permission_rule(rule).map_err(|e| DomainError::InvalidPermissionRule {
                detail: format!("not_permissions[{idx}]: {e}"),
            })?;
        }
        // One batched, deduped registry round-trip for every target
        // type across both arrays, instead of `ensure_exists` per rule.
        let target_types: Vec<&str> = request
            .permissions
            .iter()
            .chain(request.not_permissions.iter())
            .map(|r| r.target_type.as_str())
            .collect();
        self.target_type_validator
            .ensure_all_exist(&target_types)
            .await
            .map_err(DomainError::from)?;

        // The table enforces `jsonb_array_length(...) > 0`; catch the
        // empty array here so callers get a 4xx validation response
        // rather than a 500-style storage error.
        if request.assignable_scopes.is_empty() {
            return Err(DomainError::Validation {
                detail: "assignable_scopes must contain at least one scope".to_owned(),
            });
        }
        reject_oversized_assignable_scopes(request.assignable_scopes.len())?;
        self.validate_assignable_scopes(ctx, &request.assignable_scopes, owner_tenant_id)
            .await?;

        let new = NewRoleDefinition {
            id: Uuid::now_v7(),
            name: request.name,
            description: request.description,
            permissions: request.permissions,
            not_permissions: request.not_permissions,
            assignable_scopes: request.assignable_scopes,
            owner_tenant_id,
            created_by: subject,
        };
        self.repo.create(&self.conn()?, new).await
    }

    /// Validate every `assignable_scopes` entry for a create or patch:
    /// each one must resolve to an existing scope and sit inside the
    /// owner tenant's subtree.
    ///
    /// Every entry must pass, so there is nothing to short-circuit: the
    /// checks run concurrently and their tenant-resolver round-trips (up
    /// to two per entry) overlap. Results are read in list order, so the
    /// reported error is the one for the lowest failing index whichever
    /// resolver call finishes first.
    ///
    /// # Errors
    ///
    /// * `DomainError::ScopeNotFound` — an entry does not resolve.
    /// * `DomainError::Validation` — an entry exists but lies outside the
    ///   owner tenant's subtree (the detail names its index), or the owner
    ///   tenant itself does not resolve.
    /// * `DomainError::ServiceUnavailable` — the tenant resolver failed.
    async fn validate_assignable_scopes(
        &self,
        ctx: &SecurityContext,
        scopes: &[Scope],
        owner_tenant_id: Uuid,
    ) -> Result<(), DomainError> {
        let checks = scopes.iter().enumerate().map(|(idx, scope)| async move {
            // `validate_scope_exists` is string-based, so re-stringify.
            let scope_str = scope.path();
            self.scope_validator
                .validate_scope_exists(ctx, &scope_str)
                .await
                .map_err(DomainError::from)?;
            if !self
                .scope_within_owner_subtree(ctx, scope, owner_tenant_id)
                .await?
            {
                return Err(DomainError::Validation {
                    detail: format!(
                        "assignable_scopes[{idx}]: scope '{scope_str}' is not within owner tenant {owner_tenant_id} subtree"
                    ),
                });
            }
            Ok(())
        });
        // `join_all`, not `try_join_all`: the latter yields whichever
        // error lands first, which under real latency is not the lowest
        // index. The list is bounded (`MAX_ASSIGNABLE_SCOPES`), so waiting
        // for every check costs nothing measurable.
        for result in futures::future::join_all(checks).await {
            result?;
        }
        Ok(())
    }

    /// Whether `scope` sits inside the owner tenant's subtree — the
    /// containment rule the design states for `assignable_scopes`
    /// ("must remain within the immutable owner tenant subtree"), which
    /// admits the owner tenant itself, its resource groups, and any
    /// descendant tenant.
    ///
    /// Delegates to [`ScopeValidator::is_ancestor`], whose deliberate
    /// "a scope is its own ancestor" divergence exists for exactly this
    /// check. The same-tenant case is answered structurally first, so
    /// the common shape (the owner tenant, or an RG inside it) costs no
    /// tenant-resolver round-trip; only a genuinely different tenant
    /// reaches the hierarchy lookup.
    ///
    /// # Errors
    ///
    /// * `DomainError::Validation` — the owner tenant itself does not
    ///   resolve. That happens when a root-scoped caller supplies an
    ///   `owner_tenant_id` for a tenant that never existed, and on the
    ///   patch path when the row's owner tenant was deleted after the
    ///   role was created. Both are facts about the request, so they
    ///   must not surface as an opaque upstream failure.
    /// * `DomainError::ScopeNotFound` — the scope under test does not
    ///   resolve. `validate_scope_exists` normally catches this first;
    ///   reaching it here means the tenant was deleted in between.
    /// * `DomainError::ServiceUnavailable` — the tenant resolver failed.
    async fn scope_within_owner_subtree(
        &self,
        ctx: &SecurityContext,
        scope: &Scope,
        owner_tenant_id: Uuid,
    ) -> Result<bool, DomainError> {
        if scope.tenant_id() == Some(owner_tenant_id) {
            return Ok(true);
        }
        // The root scope is strictly above every tenant, so a custom role
        // can never be assignable platform-wide. `is_ancestor` answers
        // this too (`Scope::Root` as descendant returns `false`, pinned by
        // `is_ancestor_tenant_not_ancestor_of_root`); it is restated here
        // as defence-in-depth, being the one invariant standing between a
        // tenant-owned role and the whole platform — the same reason the
        // assignment side re-checks its own self case.
        if matches!(scope, Scope::Root) {
            return Ok(false);
        }
        match self
            .scope_validator
            .is_ancestor(ctx, &Scope::tenant(owner_tenant_id).path(), &scope.path())
            .await
        {
            Ok(within) => Ok(within),
            // Blame whichever endpoint actually went missing. Only the
            // owner is the caller's mistake to fix; a missing scope keeps
            // the 404 that `validate_scope_exists` would have produced.
            Err(ScopeError::ScopeNotFound {
                missing: MissingScopeEntity::Tenant { id },
                ..
            }) if id == owner_tenant_id => Err(DomainError::Validation {
                detail: format!("owner tenant {owner_tenant_id} does not exist"),
            }),
            Err(e) => Err(DomainError::from(e)),
        }
    }

    /// Fetch by id. Built-ins are visible to any
    /// authenticated caller; custom rows require `read` on the owner
    /// tenant subtree. Denial maps to `RoleDefinitionNotFound` (not
    /// `AuthorizationDenied`) to prevent id enumeration.
    ///
    /// Authz ordering: the row is read *before* `enforce`. This is
    /// structural — the custom-row policy check is scope-bound on the
    /// row's `owner_tenant_id`, which is only known after the read.
    /// `get`/`update`/`delete` all share this shape. The
    /// not-found-on-deny response hides existence; the residual is that
    /// an unauthorized caller still triggers one indexed `find_by_id` —
    /// that timing/load oracle is accepted here, since a scope-independent
    /// pre-check would over-deny or require storing scope outside the row.
    pub async fn get(
        &self,
        ctx: &SecurityContext,
        id: Uuid,
    ) -> Result<RoleDefinitionModel, DomainError> {
        let existing = self
            .repo
            .find_by_id(&self.conn()?, id)
            .await?
            .ok_or(DomainError::RoleDefinitionNotFound { id })?;

        // Built-ins are unconditionally visible.
        if existing.is_built_in {
            return Ok(existing);
        }

        // Custom roles require `read` on the owner tenant subtree.
        let owner = existing.owner_tenant_id.ok_or_else(|| {
            DomainError::internal(format!(
                "custom role definition {id} has no owner_tenant_id"
            ))
        })?;
        let subject = ctx.subject_id().to_string();
        let principal_type = principal_type_from_security_context(ctx)?;
        match self
            .policy
            .enforce(
                ctx,
                &subject,
                principal_type,
                actions::READ,
                resource_types::ROLE_DEFINITION,
                &Scope::tenant(owner),
            )
            .await
        {
            Ok(()) => Ok(existing),
            // 404, NOT 403: prevent id enumeration.
            Err(AuthorizationError::Denied) => Err(DomainError::RoleDefinitionNotFound { id }),
            Err(AuthorizationError::Internal(msg)) => Err(DomainError::internal(msg)),
        }
    }

    /// Cursor-paginated list. Built-ins surface first because the repo
    /// defaults `$orderby` to `is_built_in desc, id desc` when the
    /// caller didn't supply one. `readable_scopes` from the policy
    /// enforcer drives the [`RoleDefinitionVisibility`] narrowing
    /// applied on top of the user `$filter`.
    pub async fn list(
        &self,
        ctx: &SecurityContext,
        request: ListRoleDefinitionsRequest,
    ) -> Result<Page<RoleDefinitionModel>, DomainError> {
        let visibility = self
            .role_definition_visibility(ctx, &request.caller_scope)
            .await?;
        self.repo
            .list(&self.conn()?, visibility, &request.query)
            .await
    }

    /// [`Self::list`] plus the per-role assignment count.
    ///
    /// The page envelope (`items` order, `page_info` cursors) is the one
    /// [`Self::list`] produced; the count only decorates the rows, and it
    /// costs **one** batched query for the whole page plus one
    /// `readable_scopes` call — never one of either per row.
    ///
    /// # Errors
    ///
    /// Exactly the errors [`Self::list`] returns — nothing more. The count
    /// is a decoration and never changes this read's outcome: it is omitted
    /// (`assignment_count == None`), never raised, when it cannot be
    /// computed — because the caller has no assignment visibility, because
    /// their readable-scope set exceeds the shared projection cap, or
    /// because the count query or the scope-set query itself failed. A
    /// number taken over a truncated scope set would be worse than no
    /// number, and a decoration that can fail the read would turn a
    /// role-definition list into a role-assignment outage.
    pub async fn list_with_counts(
        &self,
        ctx: &SecurityContext,
        request: ListRoleDefinitionsRequest,
    ) -> Result<Page<CountedRoleDefinition>, DomainError> {
        let caller_scope = request.caller_scope.clone();
        let page = self.list(ctx, request).await?;
        let ids: Vec<Uuid> = dedup_ids(page.items.iter().map(|m| m.id));
        let counts = self.assignment_counts(ctx, &caller_scope, &ids).await;
        let items = page
            .items
            .into_iter()
            .map(|model| attach_count(model, counts.as_ref()))
            .collect();
        Ok(Page {
            items,
            page_info: page.page_info,
        })
    }

    /// [`Self::get`] plus the per-role assignment count.
    ///
    /// `caller_scope` is threaded in from the REST boundary rather than
    /// inferred here, because the count's visibility must be derived from
    /// exactly the same caller identity the list path uses — a root token
    /// holder counts across every tenant they can read, a tenant-scoped
    /// caller counts inside their own subtree.
    ///
    /// # Errors
    ///
    /// Exactly the errors [`Self::get`] returns. As on
    /// [`Self::list_with_counts`], a count that cannot be computed is
    /// omitted rather than raised, so the decoration can never turn a
    /// readable row into an error response.
    pub async fn get_with_counts(
        &self,
        ctx: &SecurityContext,
        id: Uuid,
        caller_scope: &CallerScope,
    ) -> Result<CountedRoleDefinition, DomainError> {
        let model = self.get(ctx, id).await?;
        let counts = self.assignment_counts(ctx, caller_scope, &[model.id]).await;
        Ok(attach_count(model, counts.as_ref()))
    }

    /// Built-in / custom counts over exactly the rows [`Self::list`] would
    /// page through for this caller.
    ///
    /// Goes through the same [`PolicyEnforcer`] gate and the same
    /// [`RoleDefinitionVisibility`] derivation as the list — not a second
    /// authorization path. A caller with no read anywhere still sees the
    /// built-in catalog counted, because built-ins are unconditionally
    /// visible to every authenticated caller and the list says so too.
    ///
    /// No `$filter`, no pagination: this is a summary of what the caller may
    /// see, not a facet over an arbitrary query.
    pub async fn summary(
        &self,
        ctx: &SecurityContext,
        caller_scope: &CallerScope,
    ) -> Result<RoleTypeCounts, DomainError> {
        let visibility = self.role_definition_visibility(ctx, caller_scope).await?;
        self.repo.count_by_type(&self.conn()?, visibility).await
    }

    /// Derive the caller's [`RoleDefinitionVisibility`] from the policy
    /// enforcer. Shared by [`Self::list`] and [`Self::summary`] so the two
    /// can never disagree about which rows the caller may see — the summary's
    /// whole job is to describe the list.
    async fn role_definition_visibility(
        &self,
        ctx: &SecurityContext,
        caller_scope: &CallerScope,
    ) -> Result<RoleDefinitionVisibility, DomainError> {
        let subject = ctx.subject_id().to_string();
        let principal_type = principal_type_from_security_context(ctx)?;

        // `caller_scope` drives the visibility query: root callers see
        // every tenant they have read in; tenant callers stay in their
        // home tenant. Mirrors the assignment-list path so root token
        // holders aren't silently downgraded to their bearer tenant.
        let context_scope = match caller_scope {
            CallerScope::Root => Scope::Root,
            CallerScope::Tenant(t) => Scope::tenant(*t),
        };

        let readable = self
            .policy
            .readable_scopes(
                ctx,
                &subject,
                principal_type,
                resource_types::ROLE_DEFINITION,
                &context_scope,
            )
            .await
            .map_err(|e| match e {
                AuthorizationError::Denied => {
                    DomainError::internal("readable_scopes returned Denied (unexpected)")
                }
                AuthorizationError::Internal(msg) => DomainError::internal(msg),
            })?;

        let visibility = match readable {
            // No read anywhere → still return built-ins (every
            // authenticated caller may read the built-in catalog).
            ReadableScopes::None => RoleDefinitionVisibility::BuiltinsOnly,
            ReadableScopes::Unrestricted => RoleDefinitionVisibility::All,
            ReadableScopes::Subtrees(prefixes) => {
                let tenants = tenant_ids_from_subtrees(&prefixes)?;
                if tenants.is_empty() {
                    RoleDefinitionVisibility::BuiltinsOnly
                } else if tenants.len() > ALLOWED_TENANTS_CAP {
                    return Err(DomainError::Validation {
                        detail: format!(
                            "list visibility set exceeds {ALLOWED_TENANTS_CAP}; \
                             refine via $filter=owner_tenant_id eq <uuid>"
                        ),
                    });
                } else {
                    // Built-ins are unconditionally visible to every
                    // authenticated caller, exactly as the
                    // `None`/`Unrestricted`/`get` paths grant them — so
                    // the tenant-scoped list returns built-ins UNION the
                    // caller's readable-tenant custom rows. Mapping to
                    // `CustomForTenants` here would hide built-ins from
                    // tenant admins.
                    RoleDefinitionVisibility::CustomForTenantsWithBuiltins(tenants)
                }
            }
        };

        Ok(visibility)
    }

    /// Per-role assignment counts for `ids`, bounded by the caller's own
    /// read visibility on **role assignments** — a different resource type
    /// from the one the surrounding read authorized, hence the second
    /// `readable_scopes` call this method costs on every role-definitions
    /// read.
    ///
    /// Returns `None` when the caller can read no assignments at all. That
    /// is the "no honest number exists" case documented on
    /// [`CountedRoleDefinition::assignment_count`], and it covers an empty
    /// `Subtrees` set as well as `ReadableScopes::None`: both admit zero
    /// rows, so both would otherwise report every role as unused.
    ///
    /// Also `None` whenever the number simply could not be produced — an
    /// unclassifiable caller principal type, an over-cap readable-scope set,
    /// a failed scope-set lookup, a failed aggregate query. There is no
    /// error channel at all: the return type is `Option`, not `Result`, so
    /// nothing on this path can fail the surrounding read even by accident.
    /// That is the decoration invariant: a name or a number must never
    /// change an HTTP status code, a row set, or a pagination cursor.
    ///
    /// `Some(map)` otherwise, with roles absent from the map having no
    /// visible assignments — the caller turns that into `Some(0)`.
    // Every branch here is a `None` the doc comment above enumerates: this is
    // a decoration path with no error channel, so each way of failing to
    // produce a number needs its own arm and they belong together.
    #[allow(clippy::cognitive_complexity)]
    async fn assignment_counts(
        &self,
        ctx: &SecurityContext,
        caller_scope: &CallerScope,
        ids: &[Uuid],
    ) -> Option<std::collections::HashMap<Uuid, u64>> {
        let subject = ctx.subject_id().to_string();
        // Not `?`: on the point-read path `get` returns early for built-ins
        // before ever resolving the principal type, so a token this binary
        // cannot classify would reach here first and turn a successful read
        // into a 422 — the decoration failing a read it only decorates. The
        // method returns `Option`, not `Result`, so that the invariant is
        // enforced by the type rather than by remembering at each `?`.
        let Ok(principal_type) = principal_type_from_security_context(ctx) else {
            tracing::debug!(
                target: "rbac.role_definition_counts",
                "caller principal type is unclassifiable; \
                 serving role definitions without assignment counts"
            );
            return None;
        };
        // Same context-scope derivation as the role-definition visibility
        // above: a root token holder counts across every tenant they can
        // read, a tenant-scoped caller inside their own subtree.
        let context_scope = match caller_scope {
            CallerScope::Root => Scope::Root,
            CallerScope::Tenant(t) => Scope::tenant(*t),
        };
        let readable = match self
            .policy
            .readable_scopes(
                ctx,
                &subject,
                principal_type,
                resource_types::ROLE_ASSIGNMENT,
                &context_scope,
            )
            .await
        {
            Ok(readable) => readable,
            Err(err) => {
                // Degrade exactly like the over-cap case below — omit the
                // count, serve the page. Neither failure shape is an answer
                // about the role definitions the caller asked for: `Denied`
                // from a scope-set query is an upstream anomaly rather than a
                // caller problem, and `Internal` means the enforcer could not
                // be reached at all. Raising either would let a decoration
                // turn `GET /rbac/v1/role-definitions` — a read that touches
                // no assignment data of its own — into a 500.
                //
                // Degrading here cannot weaken an access decision: the read
                // itself was already authorized, by `list`/`get`, before this
                // function ran. This second `readable_scopes` call is for a
                // *different* resource type (role assignments) and its only
                // effect is to narrow a number — a number that, on this
                // branch, is not produced at all.
                tracing::debug!(
                    target: "rbac.role_definition_counts",
                    error = %err,
                    "readable assignment-scope lookup failed; \
                     serving role definitions without assignment counts"
                );
                return None;
            }
        };
        // An empty `Subtrees` set is treated exactly like `None`, because it
        // admits exactly as many rows: reporting `Some(0)` for it would tell
        // the caller "no assignment anywhere uses this role" on the strength
        // of their own lack of permission.
        let no_visibility = match &readable {
            ReadableScopes::None => true,
            ReadableScopes::Subtrees(prefixes) => prefixes.is_empty(),
            ReadableScopes::Unrestricted => false,
        };
        if no_visibility {
            return None;
        }
        // One shared projection — the same one the assignment list uses, cap
        // included, so this count equals what the caller would get by paging
        // `GET /rbac/v1/role-assignments?$filter=role_definition_id eq <id>`.
        //
        // An over-cap scope set is the one case where the projection refuses.
        // On the assignment list that refusal is the answer (the caller asked
        // for those rows); here it must not be, because the count is a
        // decoration: a caller with very many readable scopes would otherwise
        // stop being able to list role definitions at all. So the count is
        // omitted, exactly as it is for a caller with no visibility, and the
        // page is served.
        let visibility = match VisibilityFilter::from_readable_scopes(readable) {
            Ok(visibility) => visibility,
            Err(err) => {
                tracing::debug!(
                    target: "rbac.role_definition_counts",
                    error = %err,
                    "readable assignment-scope set exceeds the projection cap; \
                     serving role definitions without assignment counts"
                );
                return None;
            }
        };
        // The last place the decoration could fail the read. A statement
        // timeout, a pool-acquire timeout or a lock wait on the
        // `GROUP BY role_definition_id` aggregate is a fact about the
        // assignments table, not about the role definitions this request
        // asked for — and before the count existed this read never touched
        // that table at all. So a failed aggregate omits the number and the
        // page is served, exactly as an over-cap scope set does.
        // A connection we cannot acquire is the pool-acquire case the
        // comment above already names, so it takes the same arm.
        let conn = match self.conn() {
            Ok(conn) => conn,
            Err(err) => {
                tracing::debug!(
                    target: "rbac.role_definition_counts",
                    error = %err,
                    "no connection for the assignment count; \
                     serving role definitions without assignment counts"
                );
                return None;
            }
        };
        match self
            .assignment_repo
            .count_by_role(&conn, visibility, ids)
            .await
        {
            Ok(counts) => Some(counts),
            Err(err) => {
                tracing::debug!(
                    target: "rbac.role_definition_counts",
                    error = %err,
                    "assignment count query failed; \
                     serving role definitions without assignment counts"
                );
                None
            }
        }
    }

    /// Apply a partial update. Built-ins are immutable. CAS via
    /// `If-Match`.
    ///
    /// Authz runs immediately after `find_by_id` and *before* the
    /// `is_built_in` / `ETag` / body-validation checks. Denial maps to
    /// `RoleDefinitionNotFound` (NOT `AuthorizationDenied`) so an
    /// unauthorized caller cannot distinguish "row exists but I'm
    /// denied" from "row doesn't exist" — closes the existence /
    /// stale-`ETag` / validation-error probing oracle.
    pub async fn update(
        &self,
        ctx: &SecurityContext,
        request: UpdateRoleDefinitionRequest,
    ) -> Result<RoleDefinitionModel, DomainError> {
        let if_match = request
            .if_match
            .ok_or(DomainError::OptimisticConcurrencyMissing)?;

        let existing = self
            .repo
            .find_by_id(&self.conn()?, request.id)
            .await?
            .ok_or(DomainError::RoleDefinitionNotFound { id: request.id })?;

        // Built-ins are immutable for every caller. Reject BEFORE
        // resolving `owner_tenant_id` — built-ins carry a NULL owner, so
        // the resolution below would raise an `internal` (500) error and
        // mask the immutability contract. The gate also runs ahead of the
        // authz check: a built-in's existence is already public via LIST,
        // so an early reject here leaks nothing the authz-first ordering
        // protects for custom roles, and the answer ("not modifiable") is
        // the same for every principal regardless of authorization.
        if existing.is_built_in {
            return Err(DomainError::BuiltInRoleNotModifiable {
                role_definition_id: request.id,
            });
        }

        let owner = existing.owner_tenant_id.ok_or_else(|| {
            DomainError::internal(format!(
                "role definition {} has no owner_tenant_id",
                existing.id
            ))
        })?;

        // Authz immediately after the row read, before any other
        // observable check. Denial → `RoleDefinitionNotFound` so the
        // unauthorized response is byte-identical to the missing-row
        // response. The "authorized but stale ETag / malformed patch"
        // case still surfaces correctly because those checks live below
        // this point, inside the authorized branch.
        let subject = ctx.subject_id().to_string();
        let principal_type = principal_type_from_security_context(ctx)?;
        match self
            .policy
            .enforce(
                ctx,
                &subject,
                principal_type,
                actions::WRITE,
                resource_types::ROLE_DEFINITION,
                &Scope::tenant(owner),
            )
            .await
        {
            Ok(()) => {}
            Err(AuthorizationError::Denied) => {
                return Err(DomainError::RoleDefinitionNotFound { id: request.id });
            }
            Err(AuthorizationError::Internal(msg)) => return Err(DomainError::internal(msg)),
        }

        // Immutable-field check runs AFTER authz so an unauthorized
        // caller's response is byte-identical to the missing-row
        // response (both `RoleDefinitionNotFound`). Detected at the
        // REST boundary in `first_immutable_field` because the typed
        // domain `RoleDefinitionPatch` doesn't carry the immutable
        // fields at all; only the wire DTO can express the rejection.
        if let Some(field) = request.immutable_field_attempted {
            return Err(DomainError::ImmutableFieldRejected {
                field: field.to_owned(),
            });
        }

        let current = etag_for(existing.updated_at, existing.id);
        if current != if_match {
            return Err(DomainError::StaleEtag {
                current_etag: current.into_string(),
            });
        }

        // A patch that renames the role must satisfy the same
        // charset guard as create so `extract_quoted_value` cannot be
        // fed `,` / `)` in any future uniqueness-violation recovery.
        if let Some(new_name) = request.patch.name.as_ref() {
            validate_name_charset(new_name)?;

            // Same built-in name reservation `create` applies. The DB
            // cannot be the backstop here: `uq_role_name_builtin` is
            // partial on `owner_tenant_id IS NULL`, so a tenant-owned
            // row is outside the index and a rename to `Owner` would
            // otherwise succeed — letting a custom role masquerade as
            // a built-in in the same result set.
            if CANONICAL_BUILTIN_ROLES.iter().any(|b| {
                crate::domain::name_confusables::name_collides_with_builtin(new_name, b.name)
            }) {
                return Err(DomainError::RoleDefinitionNameReservedByBuiltin {
                    name: new_name.clone(),
                });
            }
        }

        if let Some(rules) = &request.patch.permissions {
            self.validate_rules("permissions", rules).await?;
        }
        if let Some(rules) = &request.patch.not_permissions {
            self.validate_rules("not_permissions", rules).await?;
        }

        if let Some(scopes) = &request.patch.assignable_scopes {
            // PATCH with `assignable_scopes: []` hits the same
            // `jsonb_array_length(...) > 0` CHECK as create — surface
            // as 4xx validation rather than 500 storage error.
            if scopes.is_empty() {
                return Err(DomainError::Validation {
                    detail: "assignable_scopes must contain at least one scope".to_owned(),
                });
            }
            reject_oversized_assignable_scopes(scopes.len())?;
            self.validate_assignable_scopes(ctx, scopes, owner).await?;
        }

        self.repo
            .update(&self.conn()?, request.id, request.patch, &if_match)
            .await
    }

    /// Delete a custom role definition. Built-ins are rejected.
    ///
    /// Authz runs immediately after `find_by_id` (denial →
    /// `RoleDefinitionNotFound`), before the `is_built_in` and `ETag`
    /// checks. Same rationale as `update`: an unauthorized caller can't
    /// distinguish a denied-write from a missing-row.
    pub async fn delete(
        &self,
        ctx: &SecurityContext,
        id: Uuid,
        if_match: Option<Etag>,
    ) -> Result<(), DomainError> {
        let if_match = if_match.ok_or(DomainError::OptimisticConcurrencyMissing)?;

        let existing = self
            .repo
            .find_by_id(&self.conn()?, id)
            .await?
            .ok_or(DomainError::RoleDefinitionNotFound { id })?;

        // Built-ins are immutable for every caller. Reject BEFORE
        // resolving `owner_tenant_id` (NULL for built-ins) so the
        // immutability contract surfaces as BUILT_IN_ROLE_NOT_MODIFIABLE
        // (400) instead of the internal error the NULL-owner authz-scope
        // resolution would raise. Same rationale as `update`.
        if existing.is_built_in {
            return Err(DomainError::BuiltInRoleNotModifiable {
                role_definition_id: id,
            });
        }

        let owner = existing.owner_tenant_id.ok_or_else(|| {
            DomainError::internal(format!(
                "role definition {} has no owner_tenant_id; cannot enforce policy",
                existing.id
            ))
        })?;

        // Authz first; denial → `RoleDefinitionNotFound`.
        let subject = ctx.subject_id().to_string();
        let principal_type = principal_type_from_security_context(ctx)?;
        match self
            .policy
            .enforce(
                ctx,
                &subject,
                principal_type,
                actions::DELETE,
                resource_types::ROLE_DEFINITION,
                &Scope::tenant(owner),
            )
            .await
        {
            Ok(()) => {}
            Err(AuthorizationError::Denied) => {
                return Err(DomainError::RoleDefinitionNotFound { id });
            }
            Err(AuthorizationError::Internal(msg)) => return Err(DomainError::internal(msg)),
        }

        let current = etag_for(existing.updated_at, existing.id);
        if current != if_match {
            return Err(DomainError::StaleEtag {
                current_etag: current.into_string(),
            });
        }

        self.repo.delete(&self.conn()?, id, &if_match).await
    }

    /// Re-validate a slice of permission rules (shape + target type
    /// registration). Shared between `create` (per-array call from
    /// `validate_rules`/inline) and `update`.
    async fn validate_rules(
        &self,
        field_array: &str,
        rules: &[PermissionRule],
    ) -> Result<(), DomainError> {
        for (idx, rule) in rules.iter().enumerate() {
            validate_permission_rule(rule).map_err(|e| DomainError::InvalidPermissionRule {
                detail: format!("{field_array}[{idx}]: {e}"),
            })?;
        }
        // One batched, deduped registry round-trip for this array's
        // target types instead of `ensure_exists` per rule.
        let target_types: Vec<&str> = rules.iter().map(|r| r.target_type.as_str()).collect();
        self.target_type_validator
            .ensure_all_exist(&target_types)
            .await
            .map_err(DomainError::from)?;
        Ok(())
    }
}

/// Collect ids preserving first-seen order, dropping repeats.
///
/// The count query is keyed by role id, so a page that shows the same role
/// twice (possible in principle across a `$filter`ed page, and free to
/// guard against) must not send that id twice into the `IN (...)`.
fn dedup_ids(ids: impl Iterator<Item = Uuid>) -> Vec<Uuid> {
    let mut seen: std::collections::HashSet<Uuid> = std::collections::HashSet::new();
    let mut out: Vec<Uuid> = Vec::new();
    for id in ids {
        if seen.insert(id) {
            out.push(id);
        }
    }
    out
}

/// Attach the count for one row.
///
/// The two-step lookup is where the `None` / `Some(0)` distinction is
/// actually made: `counts == None` means the caller has no assignment
/// visibility, so the field stays absent; a row missing from a present map
/// means the caller *can* see assignments and none use this role, so it
/// reports zero.
fn attach_count(
    model: RoleDefinitionModel,
    counts: Option<&std::collections::HashMap<Uuid, u64>>,
) -> CountedRoleDefinition {
    let assignment_count = counts.map(|map| map.get(&model.id).copied().unwrap_or(0));
    CountedRoleDefinition {
        model,
        assignment_count,
    }
}

/// Upper bound on `assignable_scopes` entries, mirrored by `maxItems` in
/// `schemas/role_definition.v1.schema.json`.
///
/// Every entry costs the writer two tenant-resolver round-trips at create
/// and patch time, and costs every later role-assignment create up to one
/// more while the envelope is searched. Without a bound that fan-out is
/// caller-controlled: duplicates are allowed and the gear installs no
/// `DefaultBodyLimit`, so axum's 2 MiB default would admit roughly 23,000
/// entries in a single request.
///
/// Ten is well above what the shape is for — a role is scoped to a tenant,
/// a handful of its sub-tenants, or a few resource groups — while keeping
/// the worst case per request in single digits.
pub(crate) const MAX_ASSIGNABLE_SCOPES: usize = 10;

/// Enforce [`MAX_ASSIGNABLE_SCOPES`], reporting the limit and the length
/// received so the caller can trim the list without guessing.
fn reject_oversized_assignable_scopes(len: usize) -> Result<(), DomainError> {
    if len > MAX_ASSIGNABLE_SCOPES {
        return Err(DomainError::Validation {
            detail: format!(
                "assignable_scopes must contain at most {MAX_ASSIGNABLE_SCOPES} scopes, got {len}"
            ),
        });
    }
    Ok(())
}

/// Resolve `owner_tenant_id` for a create request.
fn resolve_owner_tenant(
    caller_scope: &CallerScope,
    body_value: Option<Uuid>,
) -> Result<Uuid, DomainError> {
    match (caller_scope, body_value) {
        (CallerScope::Root, Some(t)) => Ok(t),
        (CallerScope::Root, None) => Err(DomainError::OwnerTenantRequired),
        (CallerScope::Tenant(t), None) => Ok(*t),
        (CallerScope::Tenant(t), Some(body_t)) if t == &body_t => Ok(*t),
        (CallerScope::Tenant(_), Some(_)) => Err(DomainError::OwnerTenantMismatch),
    }
}

/// Project `ReadableScopes::Subtrees` prefixes into tenant UUIDs.
/// `/tenants/<uuid>` emits `<uuid>`; RG-scoped, root, and other
/// non-tenant variants are skipped (role definitions are tenant-scoped).
/// Malformed prefixes propagate as a typed `Internal` error rather than
/// being silently dropped — silent-deny would mask PEP / registry
/// corruption and surface to operators as "user can read only built-ins".
fn tenant_ids_from_subtrees(prefixes: &[String]) -> Result<Vec<Uuid>, DomainError> {
    let mut out: Vec<Uuid> = Vec::with_capacity(prefixes.len());
    // O(1) dedup via a HashSet instead of `Vec::contains` (O(n²) over
    // the prefix set), preserving first-seen order in `out`.
    let mut seen: std::collections::HashSet<Uuid> =
        std::collections::HashSet::with_capacity(prefixes.len());
    for prefix in prefixes {
        match Scope::parse(prefix) {
            Ok(Scope::Tenant { tenant_id }) => {
                if seen.insert(tenant_id) {
                    out.push(tenant_id);
                }
            }
            // RG-scoped grants don't make tenant-owned role-definition
            // rows visible. `Scope` is `#[non_exhaustive]`; fall
            // through for future variants.
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

/// Reject role-definition names containing characters that would
/// truncate the `extract_quoted_value` parser in
/// [`crate::infra::canonical_mapping`]. The parser splits on the first
/// `,` or `)` after `)=(` to recover the conflicting value from a PG
/// `Key (col)=(val) already exists.` detail string; a name carrying
/// either character would land truncated inside the typed
/// `RoleDefinitionNameTaken { name }` variant and lie about which name
/// conflicted. Reject at the domain boundary so the parser's input is
/// bounded.
fn validate_name_charset(name: &str) -> Result<(), DomainError> {
    if let Some(bad) = name.chars().find(|c| matches!(c, ',' | ')')) {
        return Err(DomainError::Validation {
            detail: format!(
                "role definition name MUST NOT contain the character {bad:?}; \
                 names with `,` or `)` would corrupt the conflict-detail \
                 parser in `canonical_mapping::extract_quoted_value`"
            ),
        });
    }
    Ok(())
}

/// PEP error → `DomainError`, with the role-definition-specific
/// `detail` text on `Denied`.
fn authorization_error_for_role_definition(err: AuthorizationError) -> DomainError {
    match err {
        AuthorizationError::Denied => DomainError::AuthorizationDenied {
            detail: "write denied on role_definition".to_owned(),
            cause: None,
        },
        AuthorizationError::Internal(msg) => DomainError::internal(msg),
    }
}

#[cfg(test)]
#[path = "service_tests.rs"]
mod service_tests;
