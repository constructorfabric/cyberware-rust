//! Permission evaluator — in-process engine behind
//! `RbacServiceClientV1::get_subject_roles` and `evaluate_permission`.
//!
//! Pieces:
//! * [`determine_scope_type`] — pure classifier `scope -> PermissionScopeType`.
//! * [`aggregate_scope_types`] — pure aggregator over contributing grants.
//! * [`PermissionEvaluator::get_subject_roles`] — orchestrates the
//!   two-phase scope query (ancestor `IN` + context-tenant RG `LIKE`).
//! * [`PermissionEvaluator::evaluate_permission`] — additive evaluator:
//!   union grants across roles, `not_permissions` subtraction stays
//!   inside the role that owns it, deny on empty grant set.

#![allow(unknown_lints, de0309_must_have_domain_model)]
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use rbac_sdk::error::RbacServiceError;
use rbac_sdk::models::{
    DenyReason, EffectivePermission, PermissionGranted, PermissionResult, PermissionScopeType,
    PrincipalType, SubjectRole,
};
use tenant_resolver_sdk::TenantResolverClient;
use tenant_resolver_sdk::error::TenantResolverError;
use tenant_resolver_sdk::models::{BarrierMode, GetAncestorsOptions, TenantId};
use toolkit_db::{DBProvider, DbError};
use toolkit_macros::domain_model;
use toolkit_odata::ODataQuery;
use toolkit_security::SecurityContext;
use tracing::warn;
use uuid::Uuid;

use crate::domain::actions;
use crate::domain::model::{RoleAssignmentModel, RoleDefinitionModel};
use crate::domain::permission_matcher::{
    PermissionMatchResult, is_permission_allowed, matches_operation, matches_target_type,
};

use crate::domain::policy_enforcer::{AuthorizationError, PolicyEnforcer, ReadableScopes};
use crate::domain::rg_port::RbacRgRead;
use crate::domain::role_assignment_repo::{RoleAssignmentRepository, SubjectAssignmentsQuery};
use crate::domain::role_definition_repo::RoleDefinitionRepository;
use crate::domain::scope_validator::{ScopeError, parse_scope};
use rbac_sdk::models::Scope;

use crate::domain::ports::metrics::{
    Dependency, DependencyOp, DependencyOutcome, EvalDenyReason, EvalErrorType, EvalResult,
    EvalScopeType, PermissionMetricsPort,
};

/// Rows requested per page of the group-membership walk. The
/// resource-group repositories resolve an absent `$top` to their
/// *default* page size (25), not their 200-row maximum, so a walk that
/// does not ask explicitly pays 8x the round trips on every
/// authorisation decision — and silently caps itself at
/// [`MEMBERSHIP_MAX_PAGES`] * 25, well under [`MEMBERSHIP_MAX_ITEMS`].
/// Asking for the documented maximum is what makes the item cap
/// reachable and the assert below true.
const MEMBERSHIP_PAGE_SIZE: usize = 200;

/// Upper bound on group memberships accumulated for one subject.
/// Bounds what a well-behaved upstream can return; the walk fails
/// CLOSED on breach rather than truncating, because a truncated
/// membership set silently narrows — and a duplicated one widens — the
/// allow-set derived from it.
const MEMBERSHIP_MAX_ITEMS: usize = 100_000;

/// Upper bound on pages drained per membership walk. [`MEMBERSHIP_MAX_ITEMS`]
/// bounds what a well-behaved upstream can return; this bounds what a
/// misbehaving one can cost. A page that carries a cursor but serves no
/// items never moves the item counter, so that cap alone cannot end the
/// walk — and this walk runs inside an authorisation decision, so a
/// non-terminating one hangs the request.
const MEMBERSHIP_MAX_PAGES: usize = 500;

/// Keeps the two caps from drifting: the page budget must still admit a
/// full [`MEMBERSHIP_MAX_ITEMS`] drain at the page size the walk
/// actually requests, so raising the item cap without the page cap — or
/// shrinking the page size — is a compile error rather than a silently
/// unreachable limit. Mirrors the identical assert in the PDP's
/// `hierarchy_client`.
const _: () = assert!(MEMBERSHIP_MAX_PAGES * MEMBERSHIP_PAGE_SIZE >= MEMBERSHIP_MAX_ITEMS);

/// Decide whether the membership walk advances, ends, or must fail
/// closed. Mirrors the PDP's `hierarchy_client::next_page_cursor`.
///
/// `Ok(None)` is the one normal ending. Both error cases fail closed:
/// this walk builds an allow-set, so a walk that cannot be trusted to
/// terminate must deny rather than return the rows it happened to
/// collect. A cursor that does not change is the cheapest such signal —
/// it is caught on the second page, before the page budget runs out,
/// and it is the only one the item cap cannot catch (a page carrying a
/// cursor but no items never moves the item counter).
///
/// `Err` carries the diagnostic tail; the caller prefixes the operation
/// and records the dependency sample.
fn next_membership_cursor(
    next_cursor: Option<String>,
    previous: &mut Option<String>,
) -> Result<Option<toolkit_odata::CursorV1>, String> {
    let Some(token) = next_cursor else {
        return Ok(None);
    };
    if previous.as_deref() == Some(token.as_str()) {
        warn!("resource group returned a non-advancing cursor; failing closed");
        return Err("non-advancing cursor from upstream".to_owned());
    }
    // Upstream emits `CursorV1::encode()`; decode round-trips its
    // keyset/sort/filter fields — a hand-built literal would fail the
    // DB-side `k.len() == order.len()` check.
    let cursor = toolkit_odata::CursorV1::decode(&token)
        .map_err(|e| format!("invalid next_cursor from upstream: {e}"))?;
    *previous = Some(token);
    Ok(Some(cursor))
}

/// Build one page's query. `ODataQuery` consumes the filter by value, so
/// the caller keeps the built `Expr` and clones it per page.
fn membership_page_query(
    subject_filter: &toolkit_odata::ast::Expr,
    cursor: Option<toolkit_odata::CursorV1>,
) -> ODataQuery {
    let mut query = ODataQuery::default()
        .with_filter(subject_filter.clone())
        .with_limit(MEMBERSHIP_PAGE_SIZE as u64);
    if let Some(c) = cursor {
        query = query.with_cursor(c);
    }
    query
}

/// The `resource_id eq {subject_id}` filter every page must carry.
///
/// # Security
///
/// Omitting it would return every membership the `SecurityContext` can
/// see, folding another subject's groups into this subject's allow-set.
fn membership_subject_filter(subject_id: &str) -> toolkit_odata::ast::Expr {
    toolkit_odata::ast::Expr::Compare(
        Box::new(toolkit_odata::ast::Expr::Identifier(
            "resource_id".to_owned(),
        )),
        toolkit_odata::ast::CompareOperator::Eq,
        Box::new(toolkit_odata::ast::Expr::Value(
            toolkit_odata::ast::Value::String(subject_id.to_owned()),
        )),
    )
}

/// Diagnostic for the item cap, logged as it is built.
fn membership_item_cap_exceeded(accumulated: usize) -> String {
    warn!(
        accumulated,
        cap = MEMBERSHIP_MAX_ITEMS,
        "group memberships exceeded pagination safety cap; failing closed"
    );
    "membership set exceeded pagination safety cap".to_owned()
}

/// Diagnostic for the page budget, logged as it is built.
fn membership_page_cap_exceeded() -> String {
    warn!(
        cap = MEMBERSHIP_MAX_PAGES,
        "group memberships exceeded pagination page cap; failing closed"
    );
    "membership walk exceeded pagination page cap".to_owned()
}

/// Error returned by [`determine_scope_type`] when the stored scope
/// string is syntactically invalid. `ScopeValidator` rejects malformed
/// scopes on write, so this variant signals a defence-in-depth path
/// (manually edited row, migration bug, etc.).
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScopeClassifyError {
    /// The scope string does not match any of the three legal forms.
    InvalidScope {
        /// The offending scope reproduced verbatim.
        scope: String,
    },
}

impl fmt::Display for ScopeClassifyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidScope { scope } => {
                write!(f, "scope classification failed: invalid scope '{scope}'")
            }
        }
    }
}

impl std::error::Error for ScopeClassifyError {}

/// Pure classifier: turn a stored scope string into the corresponding
/// [`PermissionScopeType`] without any I/O.
///
/// | Input | Output |
/// |-------|--------|
/// | `"/"` | `PermissionScopeType::Global` |
/// | `"/tenants/{T}"` | `PermissionScopeType::TenantSubtree { root_tenant_id: T }` |
/// | `"/tenants/{T}/resourceGroups/{R}"` | `PermissionScopeType::GroupSubtree { root_group_ids: vec![R] }` |
/// | anything else | `Err(ScopeClassifyError::InvalidScope)` |
pub fn determine_scope_type(scope: &str) -> Result<PermissionScopeType, ScopeClassifyError> {
    match parse_scope(scope) {
        Ok(parsed) => Ok(PermissionScopeType::from_assignment_scope(&parsed)),
        Err(ScopeError::InvalidScopeFormat { scope }) => {
            Err(ScopeClassifyError::InvalidScope { scope })
        }
        // Defence in depth: a non-format `ScopeError` here is also a
        // corrupted-row signal.
        Err(other) => Err(ScopeClassifyError::InvalidScope {
            scope: format!("{other}"),
        }),
    }
}

/// Aggregate the contributing scope types of a single `Allowed`
/// evaluation into a single [`PermissionScopeType`].
///
/// 1. Merge every `GroupSubtree.root_group_ids` into one
///    deduplicated+sorted `GroupSubtree` (Phase 2 of `get_subject_roles`
///    admits only RG scopes under the context tenant, so merging is
///    sound).
/// 2. If the merged input collapses to a single distinct variant, return
///    it directly (no `Combined` wrapping).
/// 3. Otherwise return `Combined { scopes: distinct_variants }` in canonical
///    variant/UUID order, independent of repository result ordering.
///
/// Returns `None` only when called with an empty slice — a caller invariant
/// violation. The caller in `evaluate_permission` returns early for empty
/// grants (Phase-1 `IN` list returns no rows → `PermissionResult::Denied`), so
/// a non-empty slice is the contract. `None` is deliberately neither a
/// permissive default nor a panic: `PermissionScopeType::Global` would turn a
/// bug into `Allowed { scope_type: Global }`, and an `assert!` on the in-process
/// PEP would abort the calling task. The caller surfaces a 500 `Internal`
/// instead, with a `tracing::error!` for visibility.
#[must_use]
pub fn aggregate_scope_types(scopes: &[PermissionScopeType]) -> Option<PermissionScopeType> {
    let aggregated = PermissionScopeType::aggregate(scopes);
    if aggregated.is_none() {
        tracing::error!(
            target: "rbac.permission_evaluator",
            "aggregate_scope_types called with no contributing scopes; \
             caller invariant violated (see fn doc)"
        );
    }
    aggregated
}

/// In-process permission evaluation engine behind
/// `RbacServiceClientV1::get_subject_roles` and `evaluate_permission`.
///
/// Generic over the two repositories rather than holding them as trait
/// objects: their methods take the executor as `&C where C: DBRunner`,
/// which is not dyn-compatible. `db` is the evaluator's own connection
/// source — every read here is a single statement, so the evaluator takes
/// a connection per call and never opens a transaction.
pub struct PermissionEvaluator<AR: RoleAssignmentRepository, DR: RoleDefinitionRepository> {
    db: DBProvider<DbError>,
    role_assignment_repo: Arc<AR>,
    role_definition_repo: Arc<DR>,
    tenant_resolver: Arc<dyn TenantResolverClient>,
    rg: Arc<dyn RbacRgRead>,
    metrics: Arc<dyn PermissionMetricsPort>,
}

impl<AR: RoleAssignmentRepository, DR: RoleDefinitionRepository> PermissionEvaluator<AR, DR> {
    pub fn new(
        db: DBProvider<DbError>,
        role_assignment_repo: Arc<AR>,
        role_definition_repo: Arc<DR>,
        tenant_resolver: Arc<dyn TenantResolverClient>,
        rg: Arc<dyn RbacRgRead>,
        metrics: Arc<dyn PermissionMetricsPort>,
    ) -> Self {
        // Eagerly register the unknown-scope counter so monitoring
        // tooling can distinguish "RBAC has never seen an unknown
        // variant" (counter at 0) from "RBAC isn't running" (counter
        // absent). The `OnceLock` ensures idempotency across multiple
        // evaluator constructions in tests.
        register_unknown_scope_counter();
        Self {
            db,
            role_assignment_repo,
            role_definition_repo,
            tenant_resolver,
            rg,
            metrics,
        }
    }

    /// Borrow a connection for one read. Reads here are single
    /// statements, so there is no transaction to own.
    fn conn(&self) -> Result<toolkit_db::secure::DbConn<'_>, RbacServiceError> {
        self.db
            .conn()
            .map_err(|e| RbacServiceError::internal(format!("db.conn: {e}")))
    }

    /// Resolve every role assignment applicable to `subject_id` in the
    /// tenant context `context_tenant_id`, executing the two-phase
    /// scope query in a single combined SQL statement.
    ///
    /// When `include_group_roles && principal_type == User`, group
    /// memberships are resolved via `RbacRgRead::list_memberships` and
    /// folded into the same query as a second principal predicate.
    ///
    /// # Trust boundary
    ///
    /// `subject_id` and `principal_type` are TRUSTED as the
    /// authenticated identity of the caller. Callers MUST pass values
    /// derived from a verified `SecurityContext`; there is no
    /// re-authentication here. The trust boundary lives at the
    /// transport edge / in-process adapter, NOT at this function.
    pub async fn get_subject_roles(
        &self,
        ctx: &SecurityContext,
        subject_id: &str,
        principal_type: PrincipalType,
        context_tenant_id: Uuid,
        include_group_roles: bool,
    ) -> Result<Vec<SubjectRole>, RbacServiceError> {
        let start = Instant::now();
        let out = self
            .get_subject_roles_inner(
                ctx,
                subject_id,
                principal_type,
                context_tenant_id,
                include_group_roles,
                None,
            )
            .await;
        self.metrics
            .subject_roles_duration(include_group_roles, start.elapsed().as_secs_f64());
        out
    }

    /// Variant of [`Self::get_subject_roles`] that reuses an
    /// already-resolved ancestor-tenant chain instead of re-fetching it
    /// from the tenant resolver. `evaluate_permission` resolves the chain
    /// once and threads it into both the candidate-set query (here) and
    /// the `assignment_applies` narrowing, so `get_ancestors` — the most
    /// expensive dependency call on the hot path — runs once per
    /// evaluation rather than twice.
    async fn get_subject_roles_with_ancestors(
        &self,
        ctx: &SecurityContext,
        subject_id: &str,
        principal_type: PrincipalType,
        context_tenant_id: Uuid,
        include_group_roles: bool,
        ancestor_tenant_ids: &[Uuid],
    ) -> Result<Vec<SubjectRole>, RbacServiceError> {
        let start = Instant::now();
        let out = self
            .get_subject_roles_inner(
                ctx,
                subject_id,
                principal_type,
                context_tenant_id,
                include_group_roles,
                Some(ancestor_tenant_ids),
            )
            .await;
        self.metrics
            .subject_roles_duration(include_group_roles, start.elapsed().as_secs_f64());
        out
    }

    async fn get_subject_roles_inner(
        &self,
        ctx: &SecurityContext,
        subject_id: &str,
        principal_type: PrincipalType,
        context_tenant_id: Uuid,
        include_group_roles: bool,
        precomputed_ancestors: Option<&[Uuid]>,
    ) -> Result<Vec<SubjectRole>, RbacServiceError> {
        // Hard precondition: an empty `subject_id` would match every
        // assignment row with `principal_id = ''` and silently grant
        // those role's permissions. The write path also rejects empty
        // `principal_id` (see `RoleAssignmentService::create`), but
        // this guard remains the authoritative read-side barrier so a
        // future code path that bypasses the service-layer check is
        // still safe.
        if subject_id.is_empty() {
            return Err(RbacServiceError::validation("subject_id must be non-empty"));
        }
        // Envelope-only debug event — never log grants/scope_type, and
        // never the `subject_id`: it is a principal identifier, so
        // emitting it on every authorization check would leak caller
        // identity into log sinks. Correlate via the request trace_id the
        // canonical middleware injects.
        tracing::debug!(
            target: "rbac::permission_evaluator",
            operation = "get_subject_roles",
            %context_tenant_id,
            ?principal_type,
            include_group_roles,
            "evaluator entry"
        );

        let ancestor_scopes = match precomputed_ancestors {
            // `evaluate_permission` already resolved the chain — reuse it,
            // skipping a redundant `get_ancestors` round-trip.
            Some(ids) => Self::ancestor_scopes_from_ids(ids, context_tenant_id),
            // Standalone callers (public `get_subject_roles`) resolve here.
            None => self.build_ancestor_scopes(ctx, context_tenant_id).await?,
        };
        let context_tenant_rg_prefix = format!("/tenants/{context_tenant_id}/resourceGroups/%");

        let group_principals = if include_group_roles && principal_type == PrincipalType::User {
            self.resolve_group_memberships(ctx, subject_id, context_tenant_id)
                .await?
        } else {
            Vec::new()
        };

        let query = SubjectAssignmentsQuery {
            user_principal: Some((principal_type, subject_id.to_owned())),
            group_principals,
            ancestor_scopes,
            context_tenant_rg_prefix,
            all_scopes: false,
        };

        let assignments = self
            .role_assignment_repo
            .get_subject_assignments(&self.conn()?, query)
            .await
            .map_err(|e| RbacServiceError::internal(format!("get_subject_assignments: {e}")))?;

        self.project_assignments_to_roles(assignments, context_tenant_id)
            .await
    }

    /// Resolve a subject's roles across EVERY tenant — no
    /// ancestor-scope narrowing — for the root-context list. Direct
    /// (user / service-principal) grants in any tenant are included, so a
    /// first-party-root caller's tenant-scoped read grants outside their
    /// home-tenant ancestor chain are not dropped.
    ///
    /// Group memberships are enumerated unscoped here:
    /// `resolve_group_memberships` filters only by `resource_id eq
    /// <subject_id>`, so the subject's groups in EVERY tenant surface, and
    /// this path applies no scope predicate — so group-derived grants in
    /// any tenant are included too, matching the cross-tenant intent above.
    /// (In the narrowed `get_subject_roles` path it is the ancestor-scope
    /// predicate, not this lookup, that clamps grants.)
    async fn get_subject_roles_all_scopes(
        &self,
        ctx: &SecurityContext,
        subject_id: &str,
        principal_type: PrincipalType,
        include_group_roles: bool,
    ) -> Result<Vec<SubjectRole>, RbacServiceError> {
        if subject_id.is_empty() {
            return Err(RbacServiceError::validation("subject_id must be non-empty"));
        }
        let home_tenant = ctx.subject_tenant_id();
        let group_principals = if include_group_roles && principal_type == PrincipalType::User {
            self.resolve_group_memberships(ctx, subject_id, home_tenant)
                .await?
        } else {
            Vec::new()
        };
        let query = SubjectAssignmentsQuery {
            user_principal: Some((principal_type, subject_id.to_owned())),
            group_principals,
            ancestor_scopes: Vec::new(),
            context_tenant_rg_prefix: String::new(),
            all_scopes: true,
        };
        let assignments = self
            .role_assignment_repo
            .get_subject_assignments(&self.conn()?, query)
            .await
            .map_err(|e| {
                RbacServiceError::internal(format!("get_subject_assignments (all scopes): {e}"))
            })?;
        self.project_assignments_to_roles(assignments, home_tenant)
            .await
    }

    /// Shared projection: dedupe the referenced role-definition ids,
    /// batch-fetch the definitions, and expand each assignment into a
    /// [`SubjectRole`]. `context_tenant_id` only drives the informational
    /// `is_inherited` flag, so callers that don't consume it (the root
    /// all-scopes path) may pass the caller's home tenant.
    async fn project_assignments_to_roles(
        &self,
        assignments: Vec<RoleAssignmentModel>,
        context_tenant_id: Uuid,
    ) -> Result<Vec<SubjectRole>, RbacServiceError> {
        // Dedupe the role-definition ids — multiple assignments can
        // reference the same role (e.g. the same role granted at
        // different scopes, or via group + user paths) — then fetch them in
        // one batch rather than per assignment.
        let role_def_ids: Vec<Uuid> = {
            let mut seen: HashSet<Uuid> = HashSet::with_capacity(assignments.len());
            assignments
                .iter()
                .filter_map(|a| {
                    seen.insert(a.role_definition_id)
                        .then_some(a.role_definition_id)
                })
                .collect()
        };
        let role_defs = self
            .role_definition_repo
            .find_by_ids(&self.conn()?, &role_def_ids)
            .await
            .map_err(|e| RbacServiceError::internal(format!("find_by_ids role_def: {e}")))?;
        let role_def_index: HashMap<Uuid, RoleDefinitionModel> =
            role_defs.into_iter().map(|r| (r.id, r)).collect();

        let mut subject_roles = Vec::with_capacity(assignments.len());
        for assignment in assignments {
            let role_def = role_def_index
                .get(&assignment.role_definition_id)
                .ok_or_else(|| {
                    // Defence-in-depth: an `assignment` row that
                    // FK-references a `role_definition` row absent
                    // from the batched fetch indicates either the
                    // FK-restrict race window fired (target was
                    // deleted) or row-level secure filtering hid
                    // the target. Surface as Internal — the same
                    // shape the pre-batch loop returned for an
                    // empty `find_by_id` result.
                    RbacServiceError::internal(format!(
                        "dangling role_definition_id {}",
                        assignment.role_definition_id
                    ))
                })?;

            let is_inherited = is_scope_inherited(&assignment.scope, context_tenant_id);
            subject_roles.push(SubjectRole::new(
                assignment.id,
                assignment.role_definition_id,
                role_def.name.clone(),
                role_def.permissions.clone(),
                role_def.not_permissions.clone(),
                assignment.scope,
                is_inherited,
                assignment.principal_id,
                assignment.principal_type,
            ));
        }
        Ok(subject_roles)
    }

    /// Additive permission evaluator.
    ///
    /// 1. Collect every candidate role via `get_subject_roles` (tenant-wide
    ///    set: root + ancestor-tenant chain + every RG in the context tenant).
    /// 2. Narrow the candidate set to assignments whose stored scope is
    ///    applicable to `context_scope` — see [`assignment_applies`]. This
    ///    is the per-RG narrowing that `get_subject_roles` does NOT do.
    /// 3. For each surviving role, run
    ///    `PermissionMatcher::is_permission_allowed`.
    /// 4. Union grants across roles into one `Vec<EffectivePermission>`
    ///    (one entry per granting role, not per matching rule).
    /// 5. Return `Allowed { grants, scope_type }` when the union is
    ///    non-empty, otherwise `Denied { reason }`. Reason is
    ///    `NotPermissionExclusion` if any role's `not_permissions`
    ///    matched the request and no role granted; otherwise
    ///    `NoMatchingPermission`.
    ///
    /// # Trust boundary
    ///
    /// Same contract as [`Self::get_subject_roles`]. The non-empty
    /// `subject_id` debug-assert below is a minimal tripwire, NOT a
    /// security boundary.
    ///
    /// # Security: scope narrowing
    ///
    /// `context_scope` MUST be the exact scope the caller is acting on. Taking
    /// only a `context_tenant_id` would collapse an RG-shaped request to its
    /// tenant, so an assignment on RG `A` would also satisfy a request on
    /// sibling RG `B` in the same tenant. The typed `Scope` plus the
    /// [`assignment_applies`] filter is what closes that bypass.
    pub async fn evaluate_permission(
        &self,
        ctx: &SecurityContext,
        subject_id: &str,
        principal_type: PrincipalType,
        operation: &str,
        resource_type: &str,
        context_scope: &Scope,
    ) -> Result<PermissionResult, RbacServiceError> {
        let start = Instant::now();
        let out = self
            .evaluate_permission_inner(
                ctx,
                subject_id,
                principal_type,
                operation,
                resource_type,
                context_scope,
            )
            .await;
        let secs = start.elapsed().as_secs_f64();
        match &out {
            Ok(PermissionResult::Allowed(granted)) => {
                self.metrics
                    .permission_eval_duration(EvalResult::Allow, secs);
                self.metrics
                    .permission_allow_scope_type(EvalScopeType::from(&granted.scope_type));
            }
            Ok(PermissionResult::Denied(denied)) => {
                self.metrics
                    .permission_eval_duration(EvalResult::Deny, secs);
                self.metrics
                    .permission_deny(EvalDenyReason::from(denied.reason));
            }
            // `PermissionResult` is `#[non_exhaustive]`; a future `Ok`
            // variant is recorded as a deny sample (fail-closed) so the
            // duration histogram stays complete without misattributing
            // it to allow.
            Ok(_) => {
                self.metrics
                    .permission_eval_duration(EvalResult::Deny, secs);
            }
            Err(e) => {
                self.metrics
                    .permission_eval_duration(EvalResult::Error, secs);
                self.metrics.permission_eval_error(EvalErrorType::from(e));
            }
        }
        out
    }

    async fn evaluate_permission_inner(
        &self,
        ctx: &SecurityContext,
        subject_id: &str,
        principal_type: PrincipalType,
        operation: &str,
        resource_type: &str,
        context_scope: &Scope,
    ) -> Result<PermissionResult, RbacServiceError> {
        // Hard precondition: a `debug_assert!` alone is a no-op in release,
        // so an empty `subject_id` is rejected unconditionally. See
        // `get_subject_roles` for the same guard at the lower level — either
        // may be the entry point depending on how the `PolicyEnforcer` trait
        // is reached.
        if subject_id.is_empty() {
            return Err(RbacServiceError::validation("subject_id must be non-empty"));
        }
        // Root-scope checks fall back to the caller's home tenant for
        // group-membership resolution; `get_subject_roles` still requires
        // a concrete tenant id.
        let context_tenant_id = context_scope
            .tenant_id()
            .unwrap_or_else(|| ctx.subject_tenant_id());
        // Envelope-only debug event — never log grants/scope_type, and
        // never the `subject_id`: it is a principal identifier, so
        // emitting it on every authorization check would leak caller
        // identity into log sinks. Correlate via the request trace_id the
        // canonical middleware injects.
        tracing::debug!(
            target: "rbac::permission_evaluator",
            operation = "evaluate_permission",
            %context_tenant_id,
            context_scope = %context_scope,
            ?principal_type,
            request_operation = %operation,
            resource_type = %resource_type,
            "evaluator entry"
        );

        // Ancestor chain for `context_tenant_id`, resolved ONCE per
        // request and threaded into BOTH the candidate-set query
        // (`get_subject_roles_with_ancestors`) and the per-grant
        // `assignment_applies` narrowing below. The two consumers need the
        // same chain, and `get_ancestors` is the most expensive dependency
        // call on this hot path — resolving it once (rather than once
        // inside `get_subject_roles` and again here) halves that cost.
        // Threading it into narrowing also keeps read and authz in
        // lock-step: a grant at a parent tenant authorises actions in any
        // descendant tenant (see `docs/DESIGN.md`),
        // so the SQL pre-filter and the per-grant narrowing agree on
        // inheritance instead of read saying `is_inherited = true` while
        // authz denies.
        let ancestor_tenants = self
            .fetch_ancestor_tenant_ids(ctx, context_tenant_id)
            .await?;

        let subject_roles = self
            .get_subject_roles_with_ancestors(
                ctx,
                subject_id,
                principal_type,
                context_tenant_id,
                true,
                &ancestor_tenants,
            )
            .await?;

        if subject_roles.is_empty() {
            return Ok(PermissionResult::Denied(
                rbac_sdk::models::PermissionDenied::new(DenyReason::NoMatchingPermission),
            ));
        }

        let mut grants: Vec<EffectivePermission> = Vec::new();
        let mut had_not_permission_exclusion = false;

        // The matcher operates on the rule list only; the
        // `From<&SubjectRole> for RolePolicy` impl in the SDK clones exactly
        // that.
        for sr in &subject_roles {
            // Per-scope narrowing — see `assignment_applies` for the rule.
            // The tenant-wide candidate set returned by `get_subject_roles`
            // includes every RG-scoped assignment in the context tenant;
            // counting them all would grant cross-RG access.
            if !assignment_applies(&sr.scope, context_scope, &ancestor_tenants) {
                continue;
            }
            let policy: rbac_sdk::models::RolePolicy = sr.into();
            match is_permission_allowed(&policy, operation, resource_type) {
                PermissionMatchResult::Allowed { matching_rule } => {
                    grants.push(EffectivePermission::new(
                        matching_rule,
                        sr.role_definition_id,
                        sr.assignment_id,
                        sr.role_name.clone(),
                        sr.scope.clone(),
                        sr.is_inherited,
                    ));
                }
                PermissionMatchResult::ExcludedByNotPermission { .. } => {
                    had_not_permission_exclusion = true;
                }
                PermissionMatchResult::NoMatch => {}
            }
        }

        if grants.is_empty() {
            let reason = if had_not_permission_exclusion {
                DenyReason::NotPermissionExclusion
            } else {
                DenyReason::NoMatchingPermission
            };
            return Ok(PermissionResult::Denied(
                rbac_sdk::models::PermissionDenied::new(reason),
            ));
        }

        // Build the aggregate from the typed assignment scopes at the single
        // canonical SDK construction point. This makes it impossible for the
        // normal RBAC producer to return `Global` unless a contributing grant
        // is actually root-scoped. Empty provenance fails closed as Internal
        // rather than inventing authority.
        let granted = PermissionGranted::from_grants(grants).map_err(|error| {
            tracing::error!(
                target: "rbac.permission_evaluator",
                %error,
                "failed to derive allowed scope from contributing assignments"
            );
            RbacServiceError::internal(
                "failed to derive allowed scope from contributing role assignments",
            )
        })?;

        Ok(PermissionResult::Allowed(granted))
    }

    /// Fetch the ancestor-tenant chain for `context_tenant_id` as raw
    /// UUIDs (root-to-direct-parent order, matching
    /// `TenantResolverClient::get_ancestors`). Used by both
    /// [`Self::build_ancestor_scopes`] (which formats them as the
    /// Phase-1 SQL `IN` list) and [`Self::evaluate_permission`] (which
    /// passes them to [`assignment_applies`] so a parent-tenant grant
    /// authorises a request at any descendant tenant per
    /// recorded in `docs/DESIGN.md`).
    async fn fetch_ancestor_tenant_ids(
        &self,
        ctx: &SecurityContext,
        context_tenant_id: Uuid,
    ) -> Result<Vec<Uuid>, RbacServiceError> {
        let options = GetAncestorsOptions {
            barrier_mode: BarrierMode::Ignore,
        };
        let dep_start = Instant::now();
        let resp = match self
            .tenant_resolver
            .get_ancestors(ctx, TenantId(context_tenant_id), &options)
            .await
        {
            Ok(resp) => {
                self.metrics.dependency_query(
                    Dependency::TenantResolver,
                    DependencyOp::GetAncestors,
                    DependencyOutcome::Success,
                    dep_start.elapsed().as_secs_f64(),
                );
                resp
            }
            // The scope's tenant does not exist: it has no resolvable
            // ancestor chain. Return an empty chain rather than surfacing
            // an `Internal` (500). Authz then decides purely on the
            // caller's root / exact-tenant grants — a missing tenant has no
            // parent grants, so this never widens permissions — and the
            // post-authz `validate_scope_exists` surfaces the clean
            // `ScopeNotFound` (404). Keeps authz-first intact: an
            // unauthorized caller still gets `Denied`, never a
            // tenant-existence oracle.
            Err(TenantResolverError::TenantNotFound { .. }) => {
                self.metrics.dependency_query(
                    Dependency::TenantResolver,
                    DependencyOp::GetAncestors,
                    DependencyOutcome::NotFound,
                    dep_start.elapsed().as_secs_f64(),
                );
                return Ok(Vec::new());
            }
            Err(e) => {
                self.metrics.dependency_query(
                    Dependency::TenantResolver,
                    DependencyOp::GetAncestors,
                    DependencyOutcome::Error,
                    dep_start.elapsed().as_secs_f64(),
                );
                return Err(RbacServiceError::internal(format!(
                    "tenant_resolver.get_ancestors: {e}"
                )));
            }
        };
        Ok(resp.ancestors.iter().map(|a| a.id.0).collect())
    }

    /// Build the Phase-1 `IN` list of ancestor scope paths for
    /// `context_tenant_id`.
    async fn build_ancestor_scopes(
        &self,
        ctx: &SecurityContext,
        context_tenant_id: Uuid,
    ) -> Result<Vec<String>, RbacServiceError> {
        let ancestor_ids = self
            .fetch_ancestor_tenant_ids(ctx, context_tenant_id)
            .await?;
        Ok(Self::ancestor_scopes_from_ids(
            &ancestor_ids,
            context_tenant_id,
        ))
    }

    /// Wrap raw ancestor tenant ids into the Phase-1 SQL `IN` list of
    /// scope paths: `["/", "/tenants/{ancestor}", …, "/tenants/{ctx}"]`.
    /// Pure (no I/O) so the resolved chain can be reused across the
    /// candidate-set query and the `assignment_applies` narrowing without
    /// a second `get_ancestors` round-trip.
    fn ancestor_scopes_from_ids(ancestor_ids: &[Uuid], context_tenant_id: Uuid) -> Vec<String> {
        let mut scopes = Vec::with_capacity(ancestor_ids.len() + 2);
        scopes.push("/".to_owned());
        for id in ancestor_ids {
            scopes.push(format!("/tenants/{id}"));
        }
        scopes.push(format!("/tenants/{context_tenant_id}"));
        scopes
    }

    /// Resolve the user's group memberships in the supplied tenant by
    /// paginating `RbacRgRead::list_memberships` until exhausted; returns
    /// the deduplicated set of group ids.
    ///
    /// # Security
    ///
    /// The `OData` filter `resource_id eq <subject_id>` MUST be applied on
    /// every page. Without it, `list_memberships` returns every
    /// membership the `SecurityContext` can see — folding in another
    /// subject's groups would grant the evaluated principal roles it
    /// does not have.
    // Resolves through the PEP-bypassing `ResourceGroupReadHierarchy` read
    // (unscoped — no re-entry into the PDP), so resolving group principals
    // cannot recurse RBAC→RG→PDP into a stack overflow.
    async fn resolve_group_memberships(
        &self,
        ctx: &SecurityContext,
        subject_id: &str,
        _context_tenant_id: Uuid,
    ) -> Result<Vec<String>, RbacServiceError> {
        // Time the whole walk and record exactly one dependency sample on
        // exit, whichever way it ends.
        let dep_start = Instant::now();
        let outcome = self.drain_group_memberships(ctx, subject_id).await;
        self.metrics.dependency_query(
            Dependency::ResourceGroup,
            DependencyOp::ListMemberships,
            if outcome.is_ok() {
                DependencyOutcome::Success
            } else {
                DependencyOutcome::Error
            },
            dep_start.elapsed().as_secs_f64(),
        );
        match outcome {
            // `HashSet::into_iter` order is undefined; that's fine — the
            // downstream consumer (`SubjectAssignmentsQuery::group_principals`
            // → SQL `IN (...)`) is set-membership and ignores order.
            Ok(group_ids) => Ok(group_ids.into_iter().collect()),
            Err(detail) => Err(RbacServiceError::internal(format!(
                "rg.list_memberships: {detail}"
            ))),
        }
    }

    /// The bounded membership walk behind [`Self::resolve_group_memberships`].
    ///
    /// Carries the same three fail-closed bounds as the PDP's
    /// `hierarchy_client` drains — an item cap, a page budget, and a
    /// non-advancing-cursor check. Each returns `Err` rather than the
    /// rows collected so far: this set widens the allow-set, so a walk
    /// that cannot be trusted to terminate must deny, not truncate.
    ///
    /// Returns the diagnostic tail on failure; the caller prefixes the
    /// operation name and records the dependency sample.
    async fn drain_group_memberships(
        &self,
        ctx: &SecurityContext,
        subject_id: &str,
    ) -> Result<HashSet<String>, String> {
        // `HashSet` dedup is O(n) overall (amortised); the prior
        // `Vec::contains` form was O(n²) per call and runs on every
        // authorisation decision for a user with group roles.
        let mut group_ids: HashSet<String> = HashSet::new();
        let mut next_cursor: Option<toolkit_odata::CursorV1> = None;
        let mut previous_cursor: Option<String> = None;
        let subject_filter = membership_subject_filter(subject_id);
        // Bounded: falling out of the `for` means the page budget ran out
        // with a cursor still outstanding, which is its own fail-closed
        // exit below. An unbounded `loop` here spun forever inside an
        // authorisation decision on a misbehaving upstream.
        for _ in 0..MEMBERSHIP_MAX_PAGES {
            let query = membership_page_query(&subject_filter, next_cursor.take());
            let page = self
                .rg
                .list_memberships(ctx, &query)
                .await
                .map_err(|e| e.to_string())?;
            group_ids.extend(page.items.into_iter().map(|m| m.group_id.to_string()));
            if group_ids.len() > MEMBERSHIP_MAX_ITEMS {
                return Err(membership_item_cap_exceeded(group_ids.len()));
            }
            match next_membership_cursor(page.page_info.next_cursor, &mut previous_cursor)? {
                Some(cursor) => next_cursor = Some(cursor),
                None => return Ok(group_ids),
            }
        }
        Err(membership_page_cap_exceeded())
    }
}

// `PolicyEnforcer` is implemented directly on `PermissionEvaluator` so
// the evaluator is the single owner of permission decisions. The trait
// itself is kept so handler tests can use `MockPolicyEnforcer` without
// building a full role-assignment graph.
#[async_trait]
impl<AR: RoleAssignmentRepository, DR: RoleDefinitionRepository> PolicyEnforcer
    for PermissionEvaluator<AR, DR>
{
    async fn enforce(
        &self,
        ctx: &SecurityContext,
        subject_id: &str,
        principal_type: PrincipalType,
        operation: &str,
        target_type: &str,
        context_scope: &Scope,
    ) -> Result<(), AuthorizationError> {
        // Pass the typed `Scope` straight through — the evaluator owns
        // the per-RG narrowing. See `evaluate_permission` § Security.
        let result = self
            .evaluate_permission(
                ctx,
                subject_id,
                principal_type,
                operation,
                target_type,
                context_scope,
            )
            .await
            .map_err(|e| AuthorizationError::Internal(format!("evaluate_permission: {e}")))?;
        if matches!(result, PermissionResult::Allowed(_)) {
            Ok(())
        } else {
            Err(AuthorizationError::Denied)
        }
    }

    async fn readable_scopes(
        &self,
        ctx: &SecurityContext,
        subject_id: &str,
        principal_type: PrincipalType,
        target_type: &str,
        context_scope: &Scope,
    ) -> Result<ReadableScopes, AuthorizationError> {
        // Include group-granted reads so list visibility tracks the
        // same effective permission set a single-scope
        // `evaluate_permission` would grant.
        //
        // A root-scope context means a first-party-root caller listing
        // across the whole hierarchy. Collapsing `Scope::Root` to the
        // home tenant only surfaced grants in that tenant's ancestor
        // chain, silently dropping a root caller's read grants in other
        // tenants. Aggregate across every tenant instead.
        let roles = if matches!(context_scope, Scope::Root) {
            self.get_subject_roles_all_scopes(ctx, subject_id, principal_type, true)
                .await
        } else {
            let tenant_uuid = context_scope
                .tenant_id()
                .unwrap_or_else(|| ctx.subject_tenant_id());
            self.get_subject_roles(ctx, subject_id, principal_type, tenant_uuid, true)
                .await
        }
        .map_err(|e| AuthorizationError::Internal(format!("get_subject_roles: {e}")))?;
        Ok(project_readable_scopes(&roles, target_type))
    }
}

/// `true` iff an assignment stored at `grant_scope` applies to a
/// request at `request_scope`, given the runtime tenant ancestry of the
/// request's tenant in `ancestor_tenants`.
///
/// Per-variant matching matrix. Defence in depth: `get_subject_roles` has
/// already narrowed the candidate set to ancestors of the request scope, but
/// that invariant lives in the SQL query, so it is re-checked here — which
/// closes the cross-tenant escalation and root-fallback chains at the
/// per-grant level whatever candidate set the query returns.
///
/// * `Root` → applies to anything (root is ancestor of every scope).
/// * `Tenant { tenant_id: g_t }` → applies iff the request is rooted
///   at the same tenant (`Tenant{g_t}` itself or any
///   `ResourceGroup{g_t, _}` underneath) **or** `g_t` is in
///   `ancestor_tenants` (the request's tenant chain resolved by the
///   caller via `tenant_resolver.get_ancestors`). This is the
///   unconditional downward scope inheritance recorded in
///   `docs/DESIGN.md`: a grant at a parent tenant propagates to every
///   descendant tenant. Crucially **does not** apply to a `Root`
///   request — a tenant grant is narrower than root, even when listed
///   as an ancestor.
/// * `ResourceGroup { g_t, g_g }` → applies iff the request is the
///   exact same RG. RG grants do NOT propagate across tenants — they
///   are the leaf of the hierarchy; the per-RG narrowing the SQL
///   query intentionally skips.
///
/// Callers in `evaluate_permission` resolve `ancestor_tenants` once
/// per request via `fetch_ancestor_tenant_ids`. Unit tests of the
/// strict structural arms pass an empty slice, which reproduces the
/// no-inheritance semantics for tests where the two tenants are not
/// in an ancestor relationship.
///
/// `Scope` is `#[non_exhaustive]`; unknown future variants deny *and*
/// surface through [`record_unknown_scope_variant`] so a dependency
/// upgrade that adds a new variant cannot silently take RBAC down.
pub(crate) fn assignment_applies(
    grant_scope: &Scope,
    request_scope: &Scope,
    ancestor_tenants: &[Uuid],
) -> bool {
    match grant_scope {
        Scope::Root => true,
        Scope::Tenant { tenant_id: g_t } => match request_scope {
            Scope::Tenant { tenant_id: r_t } | Scope::ResourceGroup { tenant_id: r_t, .. } => {
                g_t == r_t || ancestor_tenants.contains(g_t)
            }
            // A tenant grant is narrower than root; root requests
            // require a root-scoped grant. Prevents the caller-tenant
            // fallback from accidentally granting root authority.
            // Listing the grant tenant as an ancestor does NOT relax
            // this — only a `Root` grant authorises a `Root` request.
            Scope::Root => false,
            unknown => {
                record_unknown_scope_variant(unknown, request_scope);
                false
            }
        },
        Scope::ResourceGroup {
            tenant_id: g_t,
            group_id: g_g,
        } => match request_scope {
            // Exact same-group request (unchanged).
            Scope::ResourceGroup {
                tenant_id: r_t,
                group_id: r_g,
            } => g_t == r_t && g_g == r_g,
            // A tenant-scope request (e.g. a hint-less
            // collection read evaluates at the caller's tenant) from the
            // group's OWN tenant. The grant contributes its permission, and
            // its scope classifies to `GroupSubtree` (`determine_scope_type`),
            // so a constraints-requiring caller is narrowed to the group's
            // members — this does NOT widen to tenant-wide visibility.
            // Strictly same-tenant: no ancestor inheritance for RG grants.
            Scope::Tenant { tenant_id: r_t } => g_t == r_t,
            // An RG grant is narrower than root.
            Scope::Root => false,
            unknown => {
                record_unknown_scope_variant(unknown, request_scope);
                false
            }
        },
        unknown => {
            record_unknown_scope_variant(unknown, request_scope);
            false
        }
    }
}

/// Counter for the silent-deny fallthrough in [`assignment_applies`].
///
/// Lazy `OnceLock` so init runs at the first fallthrough — meaning the
/// crate works even before the process-global `MeterProvider` is
/// installed (the no-op provider returns a no-op meter and `.add` is
/// a no-op). On a real Prometheus / OTLP pipeline the counter shows up
/// as `rbac_unknown_scope_variants_total`; a non-zero sample is the
/// alert that a dependency bump landed a `Scope` variant the
/// evaluator doesn't handle.
static UNKNOWN_SCOPE_VARIANTS_COUNTER: std::sync::OnceLock<opentelemetry::metrics::Counter<u64>> =
    std::sync::OnceLock::new();

fn init_unknown_scope_counter() -> opentelemetry::metrics::Counter<u64> {
    let scope = opentelemetry::InstrumentationScope::builder("rbac").build();
    opentelemetry::global::meter_with_scope(scope)
        .u64_counter("rbac_unknown_scope_variants_total")
        .with_description(
            "Count of `assignment_applies` calls that hit the wildcard arm for an \
             unknown `rbac_sdk::models::Scope` variant. A non-zero sample means a \
             dependency upgrade introduced a `Scope` variant the RBAC evaluator does \
             not yet handle; every such request is denied (fail-closed). Alert on the \
             first non-zero sample.",
        )
        .build()
}

/// Eagerly register the unknown-scope counter so monitoring tooling
/// can distinguish "RBAC has never seen an unknown variant" (counter
/// at 0) from "RBAC isn't running" (counter absent). Called from
/// `PermissionEvaluator::new` once per evaluator construction; the
/// `OnceLock` keeps it idempotent across multiple evaluators in tests.
pub(crate) fn register_unknown_scope_counter() {
    UNKNOWN_SCOPE_VARIANTS_COUNTER
        .get_or_init(init_unknown_scope_counter)
        .add(0, &[]);
}

/// Side channel for the exhaustiveness fixture. Increments once per
/// fallthrough and is read by
/// `permission_evaluator_tests::assignment_applies_handles_every_known_scope_variant_without_fallthrough`.
pub(crate) static UNKNOWN_SCOPE_VARIANTS_TEST_COUNT: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Emit a `tracing::error!` event and increment the unknown-scope
/// counter. Called only from the silent-deny fallthrough in
/// [`assignment_applies`].
fn record_unknown_scope_variant(grant_scope: &Scope, request_scope: &Scope) {
    tracing::error!(
        target: "rbac.permission_evaluator",
        grant_scope_path = %grant_scope.path(),
        request_scope_path = %request_scope.path(),
        "unknown Scope variant in assignment_applies; denying request (Scope is #[non_exhaustive])"
    );
    UNKNOWN_SCOPE_VARIANTS_COUNTER
        .get_or_init(init_unknown_scope_counter)
        .add(1, &[]);
    UNKNOWN_SCOPE_VARIANTS_TEST_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

#[cfg(test)]
#[path = "permission_evaluator_tests.rs"]
mod permission_evaluator_tests;

/// Project a `SubjectRole[]` into a [`ReadableScopes`] for `target_type`.
///
/// A role grants `read` on `target_type` when:
/// 1. some entry in `permissions` matches `(operation=read, target_type)`,
///    AND
/// 2. no entry in `not_permissions` matches the same pair.
pub(crate) fn project_readable_scopes(roles: &[SubjectRole], target_type: &str) -> ReadableScopes {
    let mut prefixes: Vec<String> = Vec::new();
    // Dedup via a HashSet (O(1) membership) rather than
    // `Vec::contains` (O(n) per role → O(n²) over the grant set) — this
    // runs on every list authorization.
    let mut seen: HashSet<String> = HashSet::new();
    for role in roles {
        // Deny pass first — same precedence as
        // `permission_matcher::is_permission_allowed`.
        let deny_matched = role.not_permissions.iter().any(|rule| {
            matches_operation(&rule.operation, actions::READ)
                && matches_target_type(&rule.target_type, target_type)
        });
        if deny_matched {
            continue;
        }
        let allow_matched = role.permissions.iter().any(|rule| {
            matches_operation(&rule.operation, actions::READ)
                && matches_target_type(&rule.target_type, target_type)
        });
        if !allow_matched {
            continue;
        }
        // Root scope short-circuits to Unrestricted.
        if matches!(role.scope, Scope::Root) {
            return ReadableScopes::Unrestricted;
        }
        let path = role.scope.path();
        if seen.insert(path.clone()) {
            prefixes.push(path);
        }
    }

    if prefixes.is_empty() {
        ReadableScopes::None
    } else {
        ReadableScopes::Subtrees(prefixes)
    }
}

/// Compute `is_inherited` for a `SubjectRole`: an assignment is
/// inherited iff its scope is `/` or `/tenants/{X}` for some ancestor
/// `X != context_tenant_id`. RG scopes under the context tenant and the
/// context-tenant scope itself are NOT inherited.
//
// `pub` so the evaluator tests under `tests/postgres_*.rs` can probe the
// helper directly. `#[doc(hidden)]` keeps it out of rustdoc.
#[doc(hidden)]
#[must_use]
pub fn is_scope_inherited(scope: &Scope, context_tenant_id: Uuid) -> bool {
    // Decide by the scope's typed tenant identity, not by string
    // prefix matching. `Scope::tenant_id()` is `None` only for `Root`;
    // every tenant- or RG-rooted scope yields its owning tenant. This
    // sidesteps the `/tenants/T1` vs `/tenants/T10` sibling-prefix bug
    // class that string `starts_with` checks are prone to.
    match scope.tenant_id() {
        // Root is an ancestor of every tenant: a root-scoped grant is
        // always inherited from above the context tenant.
        None => true,
        // A grant under the context tenant (or an RG within it) is
        // direct; a grant under any other tenant is an ancestor grant
        // and therefore inherited.
        Some(tenant_id) => tenant_id != context_tenant_id,
    }
}
