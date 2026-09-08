//! Role-assignment aggregate service — the canonical orchestration
//! entry point.
//!
//! Owns validate → enforce-PEP → write-to-repo for `create` /
//! `get` / `list` / `delete`. Role-assignments are create-and-delete
//! only in v1 — no PATCH path. The repo trait is a generic parameter;
//! `RoleDefinitionRepository` is also injected (for assignable-scope
//! lookup on create) and is generic too so tests can substitute fakes
//! for both.

use std::sync::Arc;

use rbac_sdk::models::{PrincipalType, Scope};
use toolkit_db::{DBProvider, DbError};
use toolkit_macros::domain_model;
use toolkit_odata::{ODataQuery, Page};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::actions;
use crate::domain::error::DomainError;
use crate::domain::etag::{Etag, etag_for};
use crate::domain::model::RoleAssignmentModel;
use crate::domain::policy_enforcer::{AuthorizationError, PolicyEnforcer};
use crate::domain::principal_type_resolver::principal_type_from_security_context;
use crate::domain::resource_types;
use crate::domain::rg_port::RbacRgRead;
use crate::domain::role_assignment::hydration::PrincipalNameHydrator;
use crate::domain::role_assignment_repo::{
    NewRoleAssignment, RoleAssignmentRepository, VisibilityFilter,
};
use crate::domain::role_definition_repo::{
    RoleDefinitionRepository, RoleDefinitionVisibility, derive_role_definition_visibility,
};
use crate::domain::scope_validator::ScopeValidator;

/// `create` input. Mirrors the SDK create-request shape but owned by
/// the domain layer — the REST boundary builds this from the wire DTO.
#[domain_model]
#[derive(Debug, Clone)]
pub struct CreateRoleAssignmentRequest {
    pub role_definition_id: Uuid,
    /// Opaque principal id (UUID for `Group`; opaque string for `User`
    /// / `ServicePrincipal`).
    pub principal_id: String,
    pub principal_type: PrincipalType,
    /// Hierarchical scope at which the role is being granted. Typed —
    /// the REST boundary already parses `body.scope: String` to
    /// [`Scope`] before constructing the request, so the domain no
    /// not re-parse.
    pub scope: Scope,
}

/// `list` input. The caller's `$filter` / `$orderby` / `cursor` /
/// `limit` lives inside the [`ODataQuery`]; [`Self::context_scope`]
/// carries the auth-derived caller scope used to compute readable
/// scopes (independent of the user filter).
#[domain_model]
#[derive(Debug, Clone)]
pub struct ListRoleAssignmentsRequest {
    /// Caller's scope context for the readable-scopes lookup. Root
    /// callers may pass `Scope::Root`.
    pub context_scope: Scope,
    /// Caller-supplied `OData` query (parsed by the `OData` extractor
    /// at the REST boundary).
    pub query: ODataQuery,
}

/// A row plus whatever display names were resolved for it.
///
/// Names deliberately do **not** live on [`RoleAssignmentModel`]: that
/// type is the projection of a `role_assignments` row, and the
/// repository layer must stay unaware of upstream identity readers.
/// Keeping them on a separate view type also leaves every existing
/// `RoleAssignmentModel` construction site untouched.
#[domain_model]
#[derive(Debug, Clone)]
pub struct HydratedRoleAssignment {
    pub model: RoleAssignmentModel,
    pub principal_name: Option<String>,
    pub created_by_name: Option<String>,
    /// Display name of the granted role definition. Unlike the two
    /// principal names this one is read from RBAC's own table, so it costs
    /// one local batched query per page and no upstream call — but it is
    /// still optional and still degrades, because a reader must not be
    /// able to turn a decoration into a failed read.
    pub role_definition_name: Option<String>,
}

impl HydratedRoleAssignment {
    /// Wrap a row with no names — the shape served when hydration is
    /// disabled, unwired, or fully degraded.
    #[must_use]
    pub fn bare(model: RoleAssignmentModel) -> Self {
        Self {
            model,
            principal_name: None,
            created_by_name: None,
            role_definition_name: None,
        }
    }
}

/// Role-assignment aggregate service. Generic over both repository
/// traits so tests can substitute in-memory fakes for the assignment
/// repo and the role-definition repo independently.
#[domain_model]
pub struct RoleAssignmentService<R: RoleAssignmentRepository, RDR: RoleDefinitionRepository> {
    /// The service's connection source. It owns the transaction boundary
    /// and hands each repository call an executor, per
    /// `docs/toolkit_unified_system/11_database_patterns.md`.
    db: DBProvider<DbError>,
    repo: Arc<R>,
    role_repo: Arc<RDR>,
    policy: Arc<dyn PolicyEnforcer>,
    scope_validator: Arc<ScopeValidator>,
    rg: Arc<dyn RbacRgRead>,
    /// Optional display-name hydrator. `None` in unit tests and in
    /// deployments where display-name resolution is switched off or has
    /// no upstream to consult; reads then serve rows with ids and no
    /// names, which is a supported response shape rather than a
    /// degraded one.
    names: Option<Arc<PrincipalNameHydrator<RDR>>>,
}

impl<R, RDR> RoleAssignmentService<R, RDR>
where
    R: RoleAssignmentRepository,
    RDR: RoleDefinitionRepository,
{
    /// Borrow a connection for a single-statement read.
    fn conn(&self) -> Result<toolkit_db::secure::DbConn<'_>, DomainError> {
        self.db.conn().map_err(DomainError::from)
    }

    /// Construct a new service. No I/O at construction time.
    #[must_use]
    pub fn new(
        db: DBProvider<DbError>,
        repo: Arc<R>,
        role_repo: Arc<RDR>,
        policy: Arc<dyn PolicyEnforcer>,
        scope_validator: Arc<ScopeValidator>,
        rg: Arc<dyn RbacRgRead>,
    ) -> Self {
        Self {
            db,
            repo,
            role_repo,
            policy,
            scope_validator,
            rg,
            names: None,
        }
    }

    /// Attach the display-name hydrator.
    ///
    /// Chainable rather than a `new` parameter on purpose: `new` has
    /// eight call sites across the crate and its tests, and none of them
    /// cares about display names. Threading an extra argument through all
    /// of them would trade real churn for no expressiveness.
    #[must_use]
    pub fn with_hydrator(mut self, hydrator: Arc<PrincipalNameHydrator<RDR>>) -> Self {
        self.names = Some(hydrator);
        self
    }

    /// Create an assignment. Authz runs *before* any role-definition or
    /// scope-existence read so a caller without `write` cannot use the
    /// differentiated 4xx responses to enumerate role IDs, tenant / RG
    /// scopes, or assignable-scope membership.
    pub async fn create(
        &self,
        ctx: &SecurityContext,
        request: CreateRoleAssignmentRequest,
    ) -> Result<RoleAssignmentModel, DomainError> {
        // Reject empty `principal_id` for every principal type
        // (User / Group / ServicePrincipal). The read path also
        // rejects empty `subject_id`; both guards together prevent a
        // poisoned row from silently matching a malformed
        // `SecurityContext`. The Group/UUID check below
        // stays — it's a stricter shape check on top of non-empty.
        if request.principal_id.is_empty() {
            return Err(DomainError::Validation {
                detail: "principal_id must be non-empty".to_owned(),
            });
        }
        // For Group, `principal_id` MUST be a UUID. User /
        // ServicePrincipal store opaque ids.
        if request.principal_type == PrincipalType::Group
            && Uuid::parse_str(&request.principal_id).is_err()
        {
            return Err(DomainError::Validation {
                detail: "principal_id must be a UUID when principal_type=Group".to_owned(),
            });
        }

        // `request.scope` is already typed; the REST handler at
        // `post_role_assignment` parsed it once at the wire boundary.
        let parsed_scope = request.scope.clone();

        // Authorize *before* any read that could surface differentiated
        // 4xx errors. Authz-first collapses every pre-authz outcome to
        // `AuthorizationDenied`.
        let subject = ctx.subject_id().to_string();
        let caller_principal_type = principal_type_from_security_context(ctx)?;
        self.policy
            .enforce(
                ctx,
                &subject,
                caller_principal_type,
                actions::WRITE,
                resource_types::ROLE_ASSIGNMENT,
                &parsed_scope,
            )
            .await
            .map_err(authorization_error_for_role_assignment_write)?;

        let role = self
            .role_repo
            .find_by_id(&self.conn()?, request.role_definition_id)
            .await?
            .ok_or(DomainError::RoleDefinitionNotFound {
                id: request.role_definition_id,
            })?;

        self.scope_validator
            .validate_scope_exists(ctx, &request.scope.path())
            .await
            .map_err(DomainError::from)?;

        // Descendant rule: `/` matches anything.
        if !assignable_scopes_admit(
            &self.scope_validator,
            ctx,
            &role.assignable_scopes,
            &parsed_scope,
        )
        .await?
        {
            let assignable_paths: Vec<String> =
                role.assignable_scopes.iter().map(Scope::path).collect();
            return Err(DomainError::ScopeNotWithinAssignableScopes {
                scope: request.scope.path(),
                assignable_scopes: assignable_paths,
            });
        }

        if request.principal_type == PrincipalType::Group {
            self.validate_group_principal(ctx, &request, &parsed_scope)
                .await?;
        }

        let new = NewRoleAssignment {
            role_definition_id: request.role_definition_id,
            principal_id: request.principal_id,
            principal_type: request.principal_type,
            scope: parsed_scope,
            created_by: subject,
            // Capture the two facts about the author that cannot be
            // recovered from `created_by` later: which kind of principal it
            // is, and which tenant can resolve it to a name. Both come from
            // the caller's own `SecurityContext`, so this adds no upstream
            // call to the write path — a role grant must not start failing
            // because the identity provider is slow. The kind is the one
            // already computed for the authz call above, deliberately not
            // recomputed: the row must record exactly the identity that was
            // authorized.
            created_by_type: Some(caller_principal_type),
            // A nil tenant is stored as "not recorded" rather than as a
            // tenant: it resolves to nothing, and recording it would cost
            // one pointless upstream lookup per page for every row the
            // caller ever creates. `subject_tenant_id()` is infallible in
            // the type system, so the nil case has to be filtered here.
            created_by_tenant_id: Some(ctx.subject_tenant_id()).filter(|t| !t.is_nil()),
        };
        match self.repo.create(&self.conn()?, new).await {
            Ok(model) => Ok(model),
            // Context-aware: on `POST /role-assignments`, an FK miss on
            // `role_definition_id` is the caller-visible
            // `RoleDefinitionNotFound`, not the generic `Conflict` that
            // the central classifier returns.
            Err(DomainError::RoleDefinitionMissing { role_definition_id }) => {
                Err(DomainError::RoleDefinitionNotFound {
                    id: role_definition_id,
                })
            }
            Err(other) => Err(other),
        }
    }

    /// Fetch by id. Visibility: a denied caller sees
    /// `RoleAssignmentNotFound` (NOT `AuthorizationDenied`) to prevent
    /// id enumeration.
    ///
    /// Authz ordering: the row is read *before* `enforce`. This is
    /// structural, not an oversight — the policy check is scope-bound and
    /// the scope lives on the row (`existing.scope` below), so the
    /// resource must be read to know what scope to authorize against.
    /// `get`/`update`/`delete` all share this shape. A scope-independent
    /// pre-check would either over-deny callers who *do* hold the row's
    /// real scope, or require duplicating the scope outside the row. The
    /// not-found-on-deny response already hides existence; the residual
    /// is that an unauthorized caller still triggers one indexed
    /// `find_by_id` — that timing/load oracle is accepted here.
    pub async fn get(
        &self,
        ctx: &SecurityContext,
        id: Uuid,
    ) -> Result<RoleAssignmentModel, DomainError> {
        let existing = self
            .repo
            .find_by_id(&self.conn()?, id)
            .await?
            .ok_or(DomainError::RoleAssignmentNotFound { id })?;

        let subject = ctx.subject_id().to_string();
        let principal_type = principal_type_from_security_context(ctx)?;
        match self
            .policy
            .enforce(
                ctx,
                &subject,
                principal_type,
                actions::READ,
                resource_types::ROLE_ASSIGNMENT,
                &existing.scope,
            )
            .await
        {
            Ok(()) => Ok(existing),
            Err(AuthorizationError::Denied) => Err(DomainError::RoleAssignmentNotFound { id }),
            Err(AuthorizationError::Internal(msg)) => Err(DomainError::internal(msg)),
        }
    }

    /// Cursor-paginated list. Visibility through
    /// `readable_scopes`; rows ordered by `(created_at DESC, id DESC)`.
    pub async fn list(
        &self,
        ctx: &SecurityContext,
        request: ListRoleAssignmentsRequest,
    ) -> Result<Page<RoleAssignmentModel>, DomainError> {
        let subject = ctx.subject_id().to_string();
        let principal_type = principal_type_from_security_context(ctx)?;
        let readable = self
            .policy
            .readable_scopes(
                ctx,
                &subject,
                principal_type,
                resource_types::ROLE_ASSIGNMENT,
                &request.context_scope,
            )
            .await
            .map_err(|err| match err {
                // The list endpoint MUST NOT return 403. A
                // `Denied` here would be surprising — surface as
                // Internal so operators see the upstream failure.
                AuthorizationError::Denied => {
                    DomainError::internal("readable_scopes returned Denied (unexpected)")
                }
                AuthorizationError::Internal(msg) => DomainError::internal(msg),
            })?;
        // The prefix-set cap and the projection onto the repo's
        // visibility filter live on `VisibilityFilter` itself, because the
        // role-definition read path needs the identical mapping to make its
        // `assignment_count` agree with this list.
        let visibility = VisibilityFilter::from_readable_scopes(readable)?;

        self.repo
            .list(&self.conn()?, visibility, &request.query)
            .await
    }

    /// [`Self::get`] plus display-name hydration.
    ///
    /// Hydration is best-effort and additive: it can never change the
    /// status code or the row that [`Self::get`] would have returned, so
    /// a deployment with no name resolver simply serves the bare row.
    ///
    /// # Errors
    ///
    /// Exactly the errors [`Self::get`] returns — a naming failure is
    /// never one of them.
    pub async fn get_with_names(
        &self,
        ctx: &SecurityContext,
        id: Uuid,
    ) -> Result<HydratedRoleAssignment, DomainError> {
        let model = self.get(ctx, id).await?;
        let Some(hydrator) = &self.names else {
            return Ok(HydratedRoleAssignment::bare(model));
        };
        // The row's own scope is the context the caller was just
        // authorized in, so it is the honest context to ask "which role
        // definitions may you see" in. A root-scope grant still projects
        // to `Unrestricted`, so a platform admin is not narrowed by
        // reading a tenant-scoped row.
        let role_visibility = self.role_visibility_for_names(ctx, &model.scope).await;
        let mut hydrated = hydrator
            .hydrate(ctx, vec![model.clone()], role_visibility)
            .await;
        // `hydrate` maps 1:1 over its input, so a one-row input always
        // yields one row. The fallback keeps the function total without
        // an `unwrap`, and returns the same row either way.
        Ok(hydrated
            .pop()
            .unwrap_or_else(|| HydratedRoleAssignment::bare(model)))
    }

    /// [`Self::list`] plus display-name hydration, batched across the
    /// whole page rather than per row.
    ///
    /// The page envelope (`items` order, `page_info` cursors) is the one
    /// [`Self::list`] produced; hydration only decorates the rows.
    ///
    /// # Errors
    ///
    /// Exactly the errors [`Self::list`] returns.
    pub async fn list_with_names(
        &self,
        ctx: &SecurityContext,
        request: ListRoleAssignmentsRequest,
    ) -> Result<Page<HydratedRoleAssignment>, DomainError> {
        // Kept before `list` consumes the request: the role-name
        // visibility must be derived in the same scope the row visibility
        // was, or a root caller would be silently narrowed to their
        // bearer tenant for names while paging across the hierarchy.
        let context_scope = request.context_scope.clone();
        let page = self.list(ctx, request).await?;
        let items = match &self.names {
            None => page
                .items
                .into_iter()
                .map(HydratedRoleAssignment::bare)
                .collect(),
            // One hydration pass for the whole page: the hydrator batches
            // per lookup tenant, so this is where the "no per-row
            // lookups" guarantee is actually realised.
            Some(hydrator) => {
                let role_visibility = self.role_visibility_for_names(ctx, &context_scope).await;
                hydrator.hydrate(ctx, page.items, role_visibility).await
            }
        };
        Ok(Page {
            items,
            page_info: page.page_info,
        })
    }

    /// The caller's own role-definition visibility, for use as a *name*
    /// filter — never as a gate on the assignment rows themselves.
    ///
    /// Two things make this method worth its own name. It exists at all
    /// because the role-definition catalog hides another tenant's custom
    /// roles behind a `404`, while an assignment row granting such a role
    /// at a descendant scope is legitimately readable: resolving the
    /// name through an unnarrowed read would hand over exactly the string
    /// the `404` withholds.
    ///
    /// And it returns `Option`, not `Result`, because the consequence of
    /// a failure here must be bounded to the decoration. `None` means
    /// "resolve no role names"; it can never mean "fail the read". A
    /// caller whose readable-scope set is oversized, whose subject type
    /// this binary cannot classify, or whose enforcer is having a bad
    /// day, still gets every row of their page — with role ids in place
    /// of role names, which is the same shape a deleted definition
    /// produces.
    async fn role_visibility_for_names(
        &self,
        ctx: &SecurityContext,
        context_scope: &Scope,
    ) -> Option<RoleDefinitionVisibility> {
        match derive_role_definition_visibility(self.policy.as_ref(), ctx, context_scope).await {
            Ok(visibility) => Some(visibility),
            Err(err) => {
                tracing::debug!(
                    target: "rbac.principal_names",
                    error = %err,
                    "could not derive role-definition visibility for display names; \
                     rows keep their role ids"
                );
                None
            }
        }
    }

    /// Delete a role assignment.
    ///
    /// Authz runs *before* the `ETag` compare so an unauthorized caller
    /// cannot distinguish a stale-`ETag` response from a missing-row
    /// response. Denial maps to `RoleAssignmentNotFound` (mirroring the
    /// `get` path's id-enumeration defense). The authorized-but-stale
    /// case still surfaces as `StaleEtag` because the `ETag` check lives
    /// inside the authorized branch.
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
            .ok_or(DomainError::RoleAssignmentNotFound { id })?;

        // Authz immediately after the row read, before the ETag check.
        // Denial → `RoleAssignmentNotFound`.
        let subject = ctx.subject_id().to_string();
        let principal_type = principal_type_from_security_context(ctx)?;
        match self
            .policy
            .enforce(
                ctx,
                &subject,
                principal_type,
                actions::DELETE,
                resource_types::ROLE_ASSIGNMENT,
                &existing.scope,
            )
            .await
        {
            Ok(()) => {}
            Err(AuthorizationError::Denied) => {
                return Err(DomainError::RoleAssignmentNotFound { id });
            }
            Err(AuthorizationError::Internal(msg)) => return Err(DomainError::internal(msg)),
        }

        let current = etag_for(existing.updated_at, existing.id);
        if current != if_match {
            return Err(DomainError::StaleEtag {
                current_etag: current.into_string(),
            });
        }

        if self.repo.delete(&self.conn()?, id).await? {
            Ok(())
        } else {
            // Race: the row vanished between the pre-fetch and the
            // DELETE. Surface as 412 (`StaleEtag` with empty current
            // etag) rather than 404 — a 404 here would let a caller
            // without `delete` learn the row had existed because a
            // concurrent admin deleted it first.
            Err(DomainError::StaleEtag {
                current_etag: String::new(),
            })
        }
    }

    /// Group-principal sub-flow: root-scope rejection, RG
    /// existence, tenant match.
    async fn validate_group_principal(
        &self,
        ctx: &SecurityContext,
        request: &CreateRoleAssignmentRequest,
        parsed_scope: &Scope,
    ) -> Result<(), DomainError> {
        if matches!(parsed_scope, Scope::Root) {
            return Err(DomainError::GroupPrincipalRootScopeForbidden);
        }

        // Defensive: already validated upstream.
        let principal_uuid =
            Uuid::parse_str(&request.principal_id).map_err(|_| DomainError::Validation {
                detail: "principal_id must be a UUID when principal_type=Group".to_owned(),
            })?;

        let group = self
            .rg
            .get_group(ctx, principal_uuid)
            .await
            .map_err(|err| match err {
                crate::domain::rg_port::RbacRgReadError::NotFound => {
                    DomainError::GroupPrincipalNotFound {
                        principal_id: principal_uuid,
                    }
                }
                crate::domain::rg_port::RbacRgReadError::Upstream(source) => {
                    DomainError::ServiceUnavailable {
                        detail: format!("resource-group: {source}"),
                        retry_after: None,
                        // `Box<dyn Error>` → `Arc<dyn Error>`; satisfies
                        // the new `BoxError = Arc<...>` typedef.
                        cause: Some(std::sync::Arc::from(source)),
                    }
                }
            })?;

        let scope_tenant_id = match parsed_scope {
            Scope::Root => unreachable!("root rejected above"),
            Scope::Tenant { tenant_id } | Scope::ResourceGroup { tenant_id, .. } => *tenant_id,
            // `Scope` is `#[non_exhaustive]`; an unmapped future variant
            // should surface loud rather than be silently classified as
            // a `Validation` failure. Log + return `Internal` so a
            // dependency upgrade landing a new variant fails closed
            // *and* is operator-visible (mirror of
            // `record_unknown_scope_variant` in the permission
            // evaluator).
            unknown => {
                tracing::error!(
                    target: "rbac.role_assignment",
                    request_scope = %parsed_scope,
                    "unknown Scope variant in group-principal validation; \
                     denying request (Scope is #[non_exhaustive]) \u{2014} variant: {unknown:?}"
                );
                return Err(DomainError::internal(format!(
                    "unknown Scope variant in group-principal validation: {unknown:?}"
                )));
            }
        };
        if group.tenant_id != scope_tenant_id {
            // Collapse "exists in different tenant" and "does not exist
            // anywhere" into the same shape so an authorised but
            // curious caller cannot enumerate the platform-wide group
            // catalog by tenant.
            return Err(DomainError::GroupPrincipalNotFound {
                principal_id: principal_uuid,
            });
        }
        Ok(())
    }
}

/// `true` iff `scope` is admitted by at least one assignable scope
/// under the descendant rule: a scope is admissible when it equals an
/// assignable scope or lies anywhere below it, which for tenants means
/// the live tenant hierarchy — `/tenants/{parent}` admits
/// `/tenants/{child}`.
///
/// The structural pass runs first: `Scope::is_ancestor_of` answers the
/// same-tenant shapes (identity, and a tenant over its own resource
/// groups) with no round-trip, and it compares typed scopes rather than
/// strings, so `/tenants/T1` cannot match `/tenants/T10` by prefix.
/// Anything it cannot answer costs exactly one call to
/// [`ScopeValidator::get_ancestor_scopes`], whatever the length of
/// `assignable_scopes` — see the comment on the lookup below.
///
/// The self case (`a == scope`) is checked locally as defence-in-depth:
/// `is_ancestor_of` already returns `true` for equal scopes per the
/// SDK contract, but encoding the invariant at the call site means a
/// future upstream change to a strict-ancestor variant cannot silently
/// break the "exact scope is admissible" guarantee.
///
/// A dangling entry — one whose tenant has been deleted since the role
/// recorded it — admits nothing, and does not affect the other entries.
/// Nothing prunes `assignable_scopes` when a tenant goes away, so
/// letting one stale entry decide the outcome would block assignments
/// the surviving entries still legitimately allow, and would make the
/// result depend on the order the list happens to be stored in.
///
/// # Errors
///
/// * `DomainError::ScopeNotFound` — the *requested* scope does not
///   resolve. `create` calls `validate_scope_exists` on it first, so
///   reaching this means it was deleted in between.
/// * `DomainError::ServiceUnavailable` — the tenant resolver failed.
///
/// A dangling *assignable* scope is not an error — it is a non-match.
async fn assignable_scopes_admit(
    validator: &ScopeValidator,
    ctx: &SecurityContext,
    assignable_scopes: &[Scope],
    scope: &Scope,
) -> Result<bool, DomainError> {
    if assignable_scopes
        .iter()
        .any(|a| a == scope || a.is_ancestor_of(scope))
    {
        return Ok(true);
    }

    // One round-trip, not one per entry. `get_ancestor_scopes` returns
    // the whole root-to-leaf chain above `scope` — `["/", "/tenants/…",
    // …, scope]` — and an assignable scope admits `scope` exactly when
    // it appears in that chain. Asking the resolver once and testing
    // membership is the same relation `is_ancestor` computes pairwise,
    // so the cost stops depending on how long `assignable_scopes` is.
    //
    // The chain also gives the RG and dangling-entry rules for free. It
    // carries the scope's own resource group only as the final element
    // and never a sibling's, so an RG entry still matches nothing but
    // itself. And an entry whose tenant has been deleted simply is not
    // in the chain, so it is a non-match without needing to be
    // distinguished from a resolver failure.
    let chain = validator
        .get_ancestor_scopes(ctx, &scope.path())
        .await
        .map_err(DomainError::from)?;

    let admitted_by = assignable_scopes
        .iter()
        .find(|a| chain.iter().any(|ancestor| *ancestor == a.path()));

    if let Some(assignable) = admitted_by {
        tracing::debug!(
            target: "rbac.role_assignment",
            request_scope = %scope.path(),
            assignable_scope = %assignable.path(),
            "assignment scope admitted through the live tenant hierarchy"
        );
        return Ok(true);
    }
    Ok(false)
}

fn authorization_error_for_role_assignment_write(err: AuthorizationError) -> DomainError {
    match err {
        AuthorizationError::Denied => DomainError::AuthorizationDenied {
            detail: "write denied on role_assignment".to_owned(),
            cause: None,
        },
        AuthorizationError::Internal(msg) => DomainError::internal(msg),
    }
}

#[cfg(test)]
#[path = "service_tests.rs"]
mod service_tests;
