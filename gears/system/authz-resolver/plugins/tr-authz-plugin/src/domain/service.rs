// Created: 2026-04-23 by Constructor Tech
// Updated: 2026-04-29 by Constructor Tech

//! Service implementation for the TR `AuthZ` resolver plugin.
//!
//! Implements the 8 access-check rules (R1–R8) from the tenant-based access
//! algorithm: each evaluation maps to exactly one rule, depending on three
//! axes read from the request:
//!
//! - **single-resource vs list** — decided by `resource.id`.
//!   `Some(_)` → single (GET/UPDATE/DELETE); `None` → list/create.
//! - **explicit target tenant** — `context.tenant_context.root_id`
//!   (`Some` / `None`).
//! - **scope mode** — `context.tenant_context.mode`
//!   (`RootOnly` / `Subtree`; default `Subtree`).
//!
//! See `docs/arch/authorization/AUTHZ_USAGE_SCENARIOS.md` for the full matrix
//! and per-rule HTTP examples.

use std::sync::Arc;

use authz_resolver_sdk::{
    BarrierMode as AuthzBarrierMode, Capability, Constraint, EvaluationRequest, EvaluationResponse,
    EvaluationResponseContext, InGroupPredicate, InGroupSubtreePredicate, InPredicate, Predicate,
    TenantMode,
};
use tenant_resolver_sdk::{
    BarrierMode, GetDescendantsOptions, IsAncestorOptions, TenantId, TenantResolverClient,
    TenantResolverError, TenantStatus,
};
use toolkit_security::{SecurityContext, pep_properties};
use tracing::{debug, info, warn};
use uuid::Uuid;

/// TR-based `AuthZ` resolver service.
///
/// Resolves tenant hierarchy via `TenantResolverClient`.
#[toolkit_macros::domain_model]
pub struct Service {
    tr: Arc<dyn TenantResolverClient>,
}

impl Service {
    pub fn new(tr: Arc<dyn TenantResolverClient>) -> Self {
        Self { tr }
    }

    /// Evaluate an authorization request.
    ///
    /// Branches the request into one of 8 rules (R1–R8) by
    /// `resource.id.is_some()` × `tenant_context.root_id.is_some()` × `mode`.
    /// On any failed access check, resolver call failure, or missing required
    /// field — returns `deny` (fail-closed).
    #[allow(clippy::cognitive_complexity)]
    pub async fn evaluate(&self, request: &EvaluationRequest) -> EvaluationResponse {
        info!(
            action = %request.action.name,
            resource_type = %request.resource.resource_type,
            "tr-authz: evaluate called"
        );

        // Subject tenant is required in every rule (R3/R4/R7/R8 use it directly;
        // R1/R2/R5/R6 use it inside `is_in_subtree`).
        let Some(subject_tid) = Self::read_uuid(&request.subject.properties, "tenant_id") else {
            warn!("tr-authz: subject tenant_id missing or unparseable -- deny");
            return Self::deny();
        };
        if subject_tid == Uuid::nil() {
            warn!("tr-authz: subject tenant_id is nil -- deny");
            return Self::deny();
        }

        let tc = request.context.tenant_context.as_ref();
        let root_id = tc.and_then(|t| t.root_id);
        let mode = tc.map(|t| t.mode.clone()).unwrap_or_default();
        let barrier_mode =
            Self::tr_barrier_mode(tc.map_or(AuthzBarrierMode::default(), |t| t.barrier_mode));

        // Parse caller-supplied tenant_status filter once, up-front. Any
        // unknown status string fails closed (deny) — silently dropping it
        // would widen the visible-subtree set in R6/R8.
        let tenant_statuses = match tc.and_then(|t| t.tenant_status.as_deref()) {
            None => Vec::new(),
            Some(strs) => match Self::parse_tenant_statuses(strs) {
                Ok(v) => v,
                Err(bad) => {
                    warn!(%bad, "tr-authz: unknown tenant_status value -- deny");
                    return Self::deny();
                }
            },
        };

        // tr-authz is a trusted plugin by design and MUST NOT propagate caller
        // scope into TR calls: the access-check rules (R1/R2/R5/R6) walk from
        // `subject` toward `root_id`, which routinely lies outside the caller's
        // visibility — a scope-limited view would hide legitimate ancestors and
        // yield wrong allow/deny decisions. The plugin must keep its full-tree
        // visibility either way.
        //
        // TODO(https://github.com/constructorfabric/gears-rust/issues/1597):
        // once the platform S2S authentication subsystem and the gRPC + mTLS
        // transport land, replace `SecurityContext::anonymous()` here with the
        // S2S-issued service context identifying this caller as
        // `tr-authz-plugin`. Anonymous is safe today (in-process trust boundary
        // between ToolKit gears) but unsafe over a network boundary — there
        // is no cryptographic identity on the wire.
        let ctx = SecurityContext::anonymous();

        let mut response = if request.resource.id.is_some() {
            // Single-resource: owner_tenant_id is mandatory (PEP must prefetch it).
            let Some(owner_tid) = Self::read_uuid(
                &request.resource.properties,
                pep_properties::OWNER_TENANT_ID,
            ) else {
                warn!(
                    "tr-authz: single-resource request missing owner_tenant_id in properties -- deny"
                );
                return Self::deny();
            };
            self.evaluate_single(&ctx, subject_tid, owner_tid, root_id, &mode, barrier_mode)
                .await
        } else {
            self.evaluate_list(
                &ctx,
                subject_tid,
                root_id,
                &mode,
                barrier_mode,
                &tenant_statuses,
            )
            .await
        };

        // Group predicates are orthogonal to tenant scoping — append only on
        // allow. If the group property is present but any of its UUIDs are
        // malformed, the group predicate cannot be compiled; fail-closed to
        // avoid silently widening scope to tenant-wide access.
        if response.decision
            && Self::append_group_predicates(
                &mut response,
                &request.resource.properties,
                &request.context.capabilities,
            )
            .is_err()
        {
            warn!("tr-authz: malformed group scoping properties -- deny");
            return Self::deny();
        }

        response
    }

    // ── Single-resource branches (R1–R4) ──────────────────────────────────

    #[allow(clippy::cognitive_complexity)]
    async fn evaluate_single(
        &self,
        ctx: &SecurityContext,
        subject: Uuid,
        owner: Uuid,
        root_id: Option<Uuid>,
        mode: &TenantMode,
        barrier_mode: BarrierMode,
    ) -> EvaluationResponse {
        match (root_id, mode) {
            (Some(root), TenantMode::RootOnly) => {
                // R1: GET /tasks/{id}?tenant=t2&tenant_mode=root_only
                if owner != root {
                    warn!(%owner, %root, "R1: owner_tenant_id != root_id -- deny");
                    return Self::deny();
                }
                if !self.is_in_subtree(ctx, subject, root, barrier_mode).await {
                    warn!(%subject, %root, "R1: subject is not an ancestor of root_id -- deny");
                    return Self::deny();
                }
                debug!(rule = "R1", %owner, "tr-authz: allow");
                Self::allow_eq(owner)
            }
            (Some(root), TenantMode::Subtree) => {
                // R2: GET /tasks/{id}?tenant=t2
                if !self.is_in_subtree(ctx, root, owner, barrier_mode).await {
                    warn!(%owner, %root, "R2: owner is not in root_id subtree -- deny");
                    return Self::deny();
                }
                if !self.is_in_subtree(ctx, subject, root, barrier_mode).await {
                    warn!(%subject, %root, "R2: subject is not an ancestor of root_id -- deny");
                    return Self::deny();
                }
                debug!(rule = "R2", %owner, "tr-authz: allow");
                Self::allow_eq(owner)
            }
            (None, TenantMode::RootOnly) => {
                // R3: GET /tasks/{id}?tenant_mode=root_only
                if owner != subject {
                    warn!(%owner, %subject, "R3: owner_tenant_id != subject tenant -- deny");
                    return Self::deny();
                }
                debug!(rule = "R3", %owner, "tr-authz: allow");
                Self::allow_eq(owner)
            }
            (None, TenantMode::Subtree) => {
                // R4: GET /tasks/{id}
                if !self.is_in_subtree(ctx, subject, owner, barrier_mode).await {
                    warn!(%owner, %subject, "R4: owner is not in subject subtree -- deny");
                    return Self::deny();
                }
                debug!(rule = "R4", %owner, "tr-authz: allow");
                Self::allow_eq(owner)
            }
        }
    }

    // ── List / CREATE branches (R5–R8) ────────────────────────────────────

    #[allow(clippy::cognitive_complexity)]
    async fn evaluate_list(
        &self,
        ctx: &SecurityContext,
        subject: Uuid,
        root_id: Option<Uuid>,
        mode: &TenantMode,
        barrier_mode: BarrierMode,
        tenant_statuses: &[TenantStatus],
    ) -> EvaluationResponse {
        match (root_id, mode) {
            (Some(root), TenantMode::RootOnly) => {
                // R5: GET /tasks?tenant=t2&tenant_mode=root_only
                // Subject must be (reflexive) ancestor of root_id.
                if !self.is_in_subtree(ctx, subject, root, barrier_mode).await {
                    warn!(%subject, %root, "R5: subject is not an ancestor of root_id -- deny");
                    return Self::deny();
                }
                debug!(rule = "R5", %root, "tr-authz: allow");
                Self::allow_eq(root)
            }
            (Some(root), TenantMode::Subtree) => {
                // R6: GET /tasks?tenant=t2
                // Subject must be (reflexive) ancestor of root_id.
                if !self.is_in_subtree(ctx, subject, root, barrier_mode).await {
                    warn!(%subject, %root, "R6: subject is not an ancestor of root_id -- deny");
                    return Self::deny();
                }
                match self
                    .resolve_subtree(ctx, root, barrier_mode, tenant_statuses)
                    .await
                {
                    Ok(ids) if !ids.is_empty() => {
                        debug!(rule = "R6", %root, visible = ids.len(), "tr-authz: allow");
                        Self::allow_in(ids)
                    }
                    Ok(_) => {
                        warn!(%root, "R6: empty descendants -- deny");
                        Self::deny()
                    }
                    Err(e) => {
                        warn!(error = %e, %root, "R6: TR failure -- deny");
                        Self::deny()
                    }
                }
            }
            (None, TenantMode::RootOnly) => {
                // R7: GET /tasks?tenant_mode=root_only
                debug!(rule = "R7", %subject, "tr-authz: allow");
                Self::allow_eq(subject)
            }
            (None, TenantMode::Subtree) => {
                // R8: GET /tasks
                match self
                    .resolve_subtree(ctx, subject, barrier_mode, tenant_statuses)
                    .await
                {
                    Ok(ids) if !ids.is_empty() => {
                        debug!(rule = "R8", %subject, visible = ids.len(), "tr-authz: allow");
                        Self::allow_in(ids)
                    }
                    Ok(_) => {
                        warn!(%subject, "R8: empty descendants -- deny");
                        Self::deny()
                    }
                    Err(e) => {
                        warn!(error = %e, %subject, "R8: TR failure -- deny");
                        Self::deny()
                    }
                }
            }
        }
    }

    // ── TR helpers ────────────────────────────────────────────────────────

    /// Reflexive "candidate is in the closed subtree rooted at `anchor`".
    /// Returns `false` on any TR error (fail-closed).
    async fn is_in_subtree(
        &self,
        ctx: &SecurityContext,
        anchor: Uuid,
        candidate: Uuid,
        barrier_mode: BarrierMode,
    ) -> bool {
        if anchor == candidate {
            return true;
        }
        match self
            .tr
            .is_ancestor(
                ctx,
                TenantId(anchor),
                TenantId(candidate),
                &IsAncestorOptions { barrier_mode },
            )
            .await
        {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %e, %anchor, %candidate, "is_ancestor failed -- treat as false");
                false
            }
        }
    }

    /// Resolve the closed subtree (root + descendants) as UUIDs.
    ///
    /// `tenant_statuses` filters descendants by status (empty = all). Per the
    /// TR SDK contract (`GetDescendantsOptions::status`), the filter does not
    /// apply to the starting tenant itself.
    async fn resolve_subtree(
        &self,
        ctx: &SecurityContext,
        tenant_id: Uuid,
        barrier_mode: BarrierMode,
        tenant_statuses: &[TenantStatus],
    ) -> Result<Vec<Uuid>, String> {
        let response = self
            .tr
            .get_descendants(
                ctx,
                TenantId(tenant_id),
                &GetDescendantsOptions {
                    status: tenant_statuses.to_vec(),
                    barrier_mode,
                    max_depth: None,
                },
            )
            .await
            .map_err(|e| match e {
                TenantResolverError::TenantNotFound { .. } => {
                    format!("Tenant {tenant_id} not found")
                }
                other => format!("TR error: {other}"),
            })?;

        let mut visible = Vec::with_capacity(response.descendants.len() + 1);
        visible.push(response.tenant.id.0);
        visible.extend(response.descendants.iter().map(|t| t.id.0));
        Ok(visible)
    }

    // ── Response builders ────────────────────────────────────────────────

    fn allow_eq(tenant_id: Uuid) -> EvaluationResponse {
        Self::allow(vec![Predicate::In(InPredicate::new(
            pep_properties::OWNER_TENANT_ID,
            [tenant_id],
        ))])
    }

    fn allow_in(tenant_ids: Vec<Uuid>) -> EvaluationResponse {
        Self::allow(vec![Predicate::In(InPredicate::new(
            pep_properties::OWNER_TENANT_ID,
            tenant_ids,
        ))])
    }

    fn allow(predicates: Vec<Predicate>) -> EvaluationResponse {
        EvaluationResponse {
            decision: true,
            context: EvaluationResponseContext {
                constraints: vec![Constraint { predicates }],
                ..Default::default()
            },
        }
    }

    fn deny() -> EvaluationResponse {
        EvaluationResponse {
            decision: false,
            context: EvaluationResponseContext::default(),
        }
    }

    // ── Group predicates (orthogonal to tenant) ──────────────────────────

    /// Returns `Err(())` when a group scoping property is present but cannot
    /// be parsed as a full `Vec<Uuid>` (e.g. not an array, or contains a
    /// non-UUID string). Caller maps that to `deny` (fail-closed). Missing
    /// properties are `Ok(())`, but a present empty array is a deny: its set
    /// semantics are "match nothing", not "remove group filtering". A native
    /// predicate that the PEP did not advertise is also rejected rather than
    /// silently widening the response to tenant-only access.
    fn append_group_predicates(
        response: &mut EvaluationResponse,
        props: &std::collections::HashMap<String, serde_json::Value>,
        capabilities: &[Capability],
    ) -> Result<(), ()> {
        let Some(Constraint { predicates }) = response.context.constraints.get_mut(0) else {
            return Ok(());
        };
        if let Some(group_ids) = props.get("group_ids") {
            let ids = Self::parse_uuid_array(group_ids).ok_or(())?;
            if ids.is_empty() || !capabilities.contains(&Capability::GroupMembership) {
                return Err(());
            }
            predicates.push(Predicate::InGroup(InGroupPredicate::new("id", ids)));
        }
        if let Some(ancestor_ids) = props.get("ancestor_group_ids") {
            let ids = Self::parse_uuid_array(ancestor_ids).ok_or(())?;
            if ids.is_empty()
                || !capabilities.contains(&Capability::GroupMembership)
                || !capabilities.contains(&Capability::GroupHierarchy)
            {
                return Err(());
            }
            predicates.push(Predicate::InGroupSubtree(InGroupSubtreePredicate::new(
                "id", ids,
            )));
        }
        Ok(())
    }

    // ── Parsing helpers ──────────────────────────────────────────────────

    fn read_uuid(
        props: &std::collections::HashMap<String, serde_json::Value>,
        key: &str,
    ) -> Option<Uuid> {
        props
            .get(key)
            .and_then(|v| v.as_str())
            .and_then(|s| Uuid::parse_str(s).ok())
    }

    /// Strict array-of-UUID parse: returns `None` when the JSON value is not
    /// an array, OR when any element is not a valid UUID string. Callers treat
    /// `None` as a hard error (fail-closed) rather than silently dropping the
    /// bad entries, which would widen the resulting access scope.
    fn parse_uuid_array(value: &serde_json::Value) -> Option<Vec<Uuid>> {
        let arr = value.as_array()?;
        arr.iter()
            .map(|v| v.as_str().and_then(|s| Uuid::parse_str(s).ok()))
            .collect()
    }

    /// Map caller-supplied `tenant_status` strings to TR SDK `TenantStatus`.
    /// Returns the first unrecognized value on failure so the caller can
    /// fail-closed with a diagnostic — silently dropping unknowns would widen
    /// the status filter to "all statuses" and leak suspended/deleted tenants.
    ///
    /// Accepted values match the SDK's `#[serde(rename_all = "snake_case")]`
    /// representation of `TenantStatus`: `active`, `suspended`, `deleted`.
    fn parse_tenant_statuses(statuses: &[String]) -> Result<Vec<TenantStatus>, String> {
        statuses
            .iter()
            .map(|s| match s.as_str() {
                "active" => Ok(TenantStatus::Active),
                "suspended" => Ok(TenantStatus::Suspended),
                "deleted" => Ok(TenantStatus::Deleted),
                other => Err(other.to_owned()),
            })
            .collect()
    }

    fn tr_barrier_mode(mode: AuthzBarrierMode) -> BarrierMode {
        match mode {
            AuthzBarrierMode::Respect => BarrierMode::Respect,
            AuthzBarrierMode::Ignore => BarrierMode::Ignore,
        }
    }
}

#[cfg(test)]
#[path = "service_tests.rs"]
mod service_tests;
