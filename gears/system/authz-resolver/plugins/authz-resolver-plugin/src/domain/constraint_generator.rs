//! Pure transform from `Materialization` to AuthZEN-extended constraints.
//!
//! Every `Materialization` variant produces a real outcome:
//! - `TenantDirect` / `TenantSubtree` → tenant constraint on `owner_tenant_id`.
//! - `GroupSubtree` → group constraint: an `id` predicate AND-paired with an
//!   `owner_tenant_id` predicate in the SAME constraint (the two predicates
//!   are AND-combined). This enforces the RG model's "authorization always
//!   includes a tenant constraint alongside group predicates" invariant
//!   (`RESOURCE_GROUP_MODEL.md`) — a group constraint must never authorize a
//!   resource outside the group's owning tenant, even if a membership row
//!   crosses tenants.
//! - `Combined` → up to two OR-combined constraints (tenant side + group
//!   side); the group side is itself the AND-paired `id`+`owner_tenant_id`
//!   constraint described above.
//! - `Denied` → typed business deny (e.g. reserved scope variants).
//!
//! Two safety checks run inside `generate_constraints`:
//! - Expansion threshold (`max_expansion_ids`) — cheap len check on `In` lists.
//! - Supported properties — every emitted predicate's `property` must appear
//!   in `request.context.supported_properties`.
//!
//! The expansion threshold runs first; when both could fire,
//! `expansion_infeasible.v1` surfaces over `unsupported_property.v1`.

use authz_resolver_sdk::EvaluationRequest;
use authz_resolver_sdk::constraints::{
    Constraint, EqPredicate, InPredicate, InTenantSubtreePredicate, Predicate,
};
use authz_resolver_sdk::models::{BarrierMode, EvaluationResponse};
use tenant_resolver_sdk::TenantStatus;
use tracing::{debug, warn};
use uuid::Uuid;

use crate::config::AuthZResolverPluginConfig;
use crate::domain::deny::build_deny_response;
use crate::domain::deny::error_codes::{
    CONSTRAINTS_UNAVAILABLE_V1, EXPANSION_INFEASIBLE_V1, INSUFFICIENT_PERMISSIONS_V1,
    UNSUPPORTED_PROPERTY_V1,
};
use crate::domain::hierarchy_client::Materialization;
use toolkit_macros::domain_model;

/// Logical property name the plugin emits for tenant-scoped constraints.
pub(crate) const OWNER_TENANT_ID: &str = "owner_tenant_id";

/// Logical property name the plugin emits for group-scoped constraints.
pub(crate) const RESOURCE_ID: &str = "id";

/// Outcome of constraint generation for one materialization.
#[domain_model]
#[derive(Debug)]
pub(crate) enum ConstraintOutcome {
    /// Constraints ready to ship to the gateway. Production materializations
    /// yield at least one constraint; the orchestrator retains a defensive
    /// empty-vector check for future variants.
    Allow(Vec<Constraint>),
    /// Business deny — the constraint generator built the full
    /// `EvaluationResponse` (with `deny_reason` populated) so the caller can
    /// return it directly.
    Deny(EvaluationResponse),
}

/// Translate a `Materialization` into AuthZEN-extended constraints, applying
/// the expansion-threshold and supported-properties checks.
// Flat dispatch over the constraint shapes; splitting it would scatter
// the decision table across helpers.
#[allow(clippy::cognitive_complexity)]
pub(crate) fn generate_constraints(
    materialization: &Materialization,
    request: &EvaluationRequest,
    config: &AuthZResolverPluginConfig,
) -> ConstraintOutcome {
    // Step 1: Materialization::Denied is a typed business-deny channel —
    // dispatch immediately, before any predicate-building or validation.
    if let Materialization::Denied {
        error_code,
        details,
    } = materialization
    {
        debug!(error_code, "materialization carries reserved-variant deny");
        return ConstraintOutcome::Deny(build_deny_response(error_code, details.clone()));
    }

    // Step 2: expansion threshold (cheap len check). Runs before predicate
    // building so over-threshold materializations skip the construction work.
    if let Err(response) = enforce_expansion_threshold(
        materialization,
        config.capability_degradation.max_expansion_ids,
    ) {
        return ConstraintOutcome::Deny(response);
    }

    // Step 3: build predicates per variant.
    let constraints = match materialization {
        Materialization::TenantDirect { tenant_id } => {
            debug!(
                predicate = "eq",
                property = OWNER_TENANT_ID,
                "tenant constraints generated"
            );
            vec![Constraint {
                predicates: vec![tenant_predicate(&[*tenant_id])],
            }]
        }
        Materialization::TenantSubtree { tenant_ids } => {
            // Fail closed on an empty subtree: this happens when the granted
            // root tenant is non-active (excluded by the status filter, see
            // get_tenant_subtree_ids) AND it has no status-matching descendants,
            // so the grant resolves to zero accessible tenants. Emitting an
            // empty In(owner_tenant_id, []) would both violate tenant_predicate's
            // non-empty contract and lean on the PEP's empty-IN handling; a deny
            // is the honest, fail-closed outcome.
            if tenant_ids.is_empty() {
                warn!(
                    "tenant subtree resolved to zero tenants (non-active root, no matching descendants) - failing closed"
                );
                return ConstraintOutcome::Deny(build_deny_response(
                    INSUFFICIENT_PERMISSIONS_V1,
                    Some("tenant subtree resolved to no accessible tenants".to_owned()),
                ));
            }
            debug!(
                predicate = if tenant_ids.len() == 1 { "eq" } else { "in" },
                property = OWNER_TENANT_ID,
                values_count = tenant_ids.len(),
                "tenant constraints generated"
            );
            vec![Constraint {
                predicates: vec![tenant_predicate(tenant_ids)],
            }]
        }
        Materialization::TenantSubtreePushdown {
            root_tenant_id,
            barrier_mode,
            status,
        } => {
            let constraints = tenant_subtree_pushdown_constraints(
                *root_tenant_id,
                *barrier_mode,
                status,
                &request.context.supported_properties,
            );
            // The resolver was already skipped at materialization time, so an
            // empty set here (PEP advertised TenantHierarchy but declared no
            // tenant-shaped supported property) has no eager fallback. Fail
            // closed rather than emit an unconstrained allow.
            if constraints.is_empty() {
                warn!(
                    "tenant-subtree push-down: PEP advertised TenantHierarchy but supports no tenant-shaped property - failing closed"
                );
                return ConstraintOutcome::Deny(build_deny_response(
                    INSUFFICIENT_PERMISSIONS_V1,
                    Some(
                        "tenant subtree push-down has no PEP-supported tenant property".to_owned(),
                    ),
                ));
            }
            debug!(
                predicate = "in_tenant_subtree",
                root_tenant_id = %root_tenant_id,
                constraint_count = constraints.len(),
                "tenant subtree push-down constraints generated"
            );
            constraints
        }
        Materialization::GroupSubtree {
            resource_ids,
            owner_tenant_ids,
        } => {
            debug!(
                predicate = "in",
                property = RESOURCE_ID,
                values_count = resource_ids.len(),
                tenant_count = owner_tenant_ids.len(),
                "group constraints generated (tenant-paired)"
            );
            match group_constraint(resource_ids, owner_tenant_ids) {
                Ok(constraint) => vec![constraint],
                Err(response) => return ConstraintOutcome::Deny(response),
            }
        }
        Materialization::Combined {
            tenant_ids,
            resource_ids,
            group_owner_tenant_ids,
        } => match combined_constraints(tenant_ids, resource_ids, group_owner_tenant_ids) {
            Ok(constraints) => constraints,
            Err(response) => return ConstraintOutcome::Deny(response),
        },
        // Step 1 already returns on `Denied`, so this is logically
        // unreachable. Fail closed (return the deny) rather than panic, so a
        // future refactor that breaks the step-1 invariant can't take the PDP
        // process down. `debug_assert!` makes the regression noisy in debug
        // and test builds without weakening release-mode fail-closed safety.
        Materialization::Denied {
            error_code,
            details,
        } => {
            debug_assert!(
                false,
                "constraint_generator step 1 missed a Denied materialization (error_code={error_code})"
            );
            return ConstraintOutcome::Deny(build_deny_response(error_code, details.clone()));
        }
    };

    // Step 4: supported_properties validation. Runs after predicate building
    // so the validator can inspect the actual property strings.
    if let Err(response) =
        validate_supported_properties(&constraints, &request.context.supported_properties)
    {
        return ConstraintOutcome::Deny(response);
    }

    ConstraintOutcome::Allow(constraints)
}

/// Tenant predicate: `Eq` when exactly one tenant, `In` otherwise.
/// Caller guarantees the slice is non-empty.
fn tenant_predicate(tenant_ids: &[Uuid]) -> Predicate {
    if tenant_ids.len() == 1 {
        Predicate::Eq(EqPredicate::new(OWNER_TENANT_ID, tenant_ids[0]))
    } else {
        Predicate::In(InPredicate::new(
            OWNER_TENANT_ID,
            tenant_ids.iter().copied(),
        ))
    }
}

/// Build push-down tenant-subtree constraints: one `InTenantSubtree(prop, root)`
/// per PEP-supported tenant-shaped property (`OWNER_TENANT_ID`, `RESOURCE_ID`),
/// OR-combined at the response envelope. Unsupported properties are skipped
/// (never emitted) so `validate_supported_properties` cannot turn a capable
/// PEP's request into an `unsupported_property` deny — mirrors the static
/// plugin's `supports_property` gate. `barrier_mode` and `status` are forwarded
/// verbatim so the PEP's `tenant_closure` subquery matches the eager
/// `get_tenant_subtree_ids` semantics exactly (`status` already carries the
/// `[Active]` default).
fn tenant_subtree_pushdown_constraints(
    root_tenant_id: Uuid,
    barrier_mode: BarrierMode,
    status: &[TenantStatus],
    supported: &[String],
) -> Vec<Constraint> {
    [OWNER_TENANT_ID, RESOURCE_ID]
        .into_iter()
        .filter(|prop| supported.iter().any(|s| s == prop))
        .map(|prop| Constraint {
            predicates: vec![Predicate::InTenantSubtree(
                InTenantSubtreePredicate::with_barrier_mode(prop, root_tenant_id, barrier_mode)
                    .with_descendant_status(status.to_vec()),
            )],
        })
        .collect()
}

/// Group predicate: always `In` (groups don't have a "direct" mode — even
/// single-resource subtrees emit `In([res])`, a valid one-value shape).
fn group_predicate(resource_ids: &[Uuid]) -> Predicate {
    Predicate::In(InPredicate::new(RESOURCE_ID, resource_ids.iter().copied()))
}

/// Build a tenant-paired group constraint: an `id` predicate AND an
/// `owner_tenant_id` predicate in the SAME `Constraint` (predicates within a
/// constraint are AND-combined). Enforces the RG model's "tenant constraint
/// always applies alongside group predicates" invariant.
///
/// `resource_ids` MAY legitimately be empty — a group with zero member
/// resources (the grant resolves to no accessible resources). Emitting
/// `In(id, [])` would lean on the PEP compiler's empty-`In` handling, which
/// fails closed with an `Internal`/500 rather than a clean deny (the compiler
/// treats empty-`In` as a PDP contract violation). So we deny here, mirroring
/// the `TenantSubtree` empty-subtree path: an empty group subtree is a normal
/// runtime state, and the honest outcome is "access to zero resources" = deny.
///
/// `owner_tenant_ids` MUST be non-empty — `get_group_subtree_resource_ids`
/// guarantees this on the success path (it fails closed if resources resolve
/// with no owning tenant). An empty set here would mean a resolution invariant
/// broke; we fail closed with a deny rather than emit a tenant-less group
/// constraint (which would re-open the cross-tenant leak) or panic in
/// `tenant_predicate`.
fn group_constraint(
    resource_ids: &[Uuid],
    owner_tenant_ids: &[Uuid],
) -> Result<Constraint, EvaluationResponse> {
    if resource_ids.is_empty() {
        // Empty group subtree (a group with no member resources): fail closed
        // with a deny rather than build an `In(id, [])` predicate. The PEP
        // compiler rejects an empty-`In` value list as a contract violation
        // and returns `Internal`/500 (it cannot lower `id IN ()` portably), so
        // an empty member set must never reach it — the grant covers zero
        // resources, which is a clean deny.
        warn!(
            "group subtree resolved to zero member resources - failing closed (deny, not empty-In)"
        );
        return Err(build_deny_response(
            INSUFFICIENT_PERMISSIONS_V1,
            Some("group scope resolved to no member resources".to_owned()),
        ));
    }
    if owner_tenant_ids.is_empty() {
        // Should not happen on the success path (get_group_subtree_resource_ids
        // already fails closed when resources resolve with no owning tenant),
        // but if it does we DENY rather than panic: a tenant-less group
        // constraint would re-open the cross-tenant leak, and a panic on a PDP
        // request path is fail-OPEN (process death). Deny is the safe outcome.
        warn!(
            "group scope has no owning tenant - failing closed to avoid a tenant-less group constraint"
        );
        return Err(build_deny_response(
            INSUFFICIENT_PERMISSIONS_V1,
            Some("group scope resolved without an owning tenant".to_owned()),
        ));
    }
    Ok(Constraint {
        predicates: vec![
            group_predicate(resource_ids),
            tenant_predicate(owner_tenant_ids),
        ],
    })
}

/// Build the constraint list for a `Combined` materialization. One or two
/// constraints are emitted depending on which side is populated. If both sides
/// are empty, fail closed with `constraints_unavailable.v1`; an empty vector
/// must never represent an unconstrained allow. The group side is tenant-paired
/// via [`group_constraint`].
fn combined_constraints(
    tenant_ids: &[Uuid],
    resource_ids: &[Uuid],
    group_owner_tenant_ids: &[Uuid],
) -> Result<Vec<Constraint>, EvaluationResponse> {
    Ok(match (tenant_ids.is_empty(), resource_ids.is_empty()) {
        // Both empty — the aggregate resolved to no accessible IDs. This is
        // normally rejected by the orchestrator before its decision-only
        // branch; retain the same deny here as defense in depth for direct
        // callers and future orchestration changes.
        (true, true) => {
            return Err(build_deny_response(
                CONSTRAINTS_UNAVAILABLE_V1,
                Some("combined scope materialized to no accessible IDs".to_owned()),
            ));
        }
        // Only tenants — single tenant constraint.
        (false, true) => vec![Constraint {
            predicates: vec![tenant_predicate(tenant_ids)],
        }],
        // Only resources — single tenant-paired group constraint.
        (true, false) => vec![group_constraint(resource_ids, group_owner_tenant_ids)?],
        // Both populated — two constraints, OR'd at response level: the OR'd
        // tenant-scope constraint first, the tenant-paired group constraint
        // second.
        (false, false) => vec![
            Constraint {
                predicates: vec![tenant_predicate(tenant_ids)],
            },
            group_constraint(resource_ids, group_owner_tenant_ids)?,
        ],
    })
}

/// Extract the `property` field from any SDK `Predicate` variant. Exhaustive
/// match so a new SDK variant becomes a compile error here.
fn predicate_property(predicate: &Predicate) -> &str {
    match predicate {
        Predicate::Eq(p) => &p.property,
        Predicate::In(p) => &p.property,
        Predicate::InGroup(p) => &p.property,
        Predicate::InGroupSubtree(p) => &p.property,
        Predicate::InTenantSubtree(p) => &p.property,
    }
}

/// Verify every predicate's `property` appears in the PEP's declared
/// `supported_properties`. On miss, return a `Deny(EvaluationResponse)`
/// carrying `unsupported_property.v1`.
fn validate_supported_properties(
    constraints: &[Constraint],
    supported: &[String],
) -> Result<(), EvaluationResponse> {
    for constraint in constraints {
        for predicate in &constraint.predicates {
            let property = predicate_property(predicate);
            if !supported.iter().any(|s| s == property) {
                warn!(
                    property = %property,
                    supported = ?supported,
                    "unsupported property in generated predicate"
                );
                return Err(build_deny_response(
                    UNSUPPORTED_PROPERTY_V1,
                    Some(format!(
                        "unsupported property '{property}'; PEP supports: {supported:?}"
                    )),
                ));
            }
        }
    }
    Ok(())
}

/// Enforce `max_expansion_ids` on the materialization's `In`-list lengths.
/// Strict `>` comparison — at-threshold passes. Applies to variants that
/// carry a `values: Vec<...>` (`TenantSubtree`, `GroupSubtree`, Combined);
/// `TenantDirect` carries one value (Eq predicate) and is exempt. Denied is
/// handled before this function runs.
fn enforce_expansion_threshold(
    materialization: &Materialization,
    max_expansion_ids: usize,
) -> Result<(), EvaluationResponse> {
    let check = |count: usize| -> Result<(), EvaluationResponse> {
        if count > max_expansion_ids {
            warn!(
                materialized_count = count,
                max_expansion_ids, "expansion infeasible"
            );
            Err(build_deny_response(
                EXPANSION_INFEASIBLE_V1,
                Some(format!(
                    "materialized {count} IDs, exceeds max_expansion_ids ({max_expansion_ids})"
                )),
            ))
        } else {
            Ok(())
        }
    };
    // `TenantDirect` and `Denied` both return `Ok(())` but are kept as
    // separate arms: each documents a distinct reason there is nothing to
    // bound, so they intentionally are not merged.
    #[allow(clippy::match_same_arms)]
    match materialization {
        Materialization::TenantDirect { .. } => Ok(()),
        // Push-down emits a subquery predicate, not an ID list — nothing to bound.
        Materialization::TenantSubtreePushdown { .. } => Ok(()),
        Materialization::TenantSubtree { tenant_ids } => check(tenant_ids.len()),
        // owner_tenant_ids is bounded by the number of tenants a group set
        // spans (normally 1) — not subject to the expansion threshold.
        Materialization::GroupSubtree { resource_ids, .. } => check(resource_ids.len()),
        Materialization::Combined {
            tenant_ids,
            resource_ids,
            ..
        } => {
            check(tenant_ids.len())?;
            check(resource_ids.len())
        }
        // Denied is handled before this function — defensive Ok keeps the
        // match exhaustive without a panic path.
        Materialization::Denied { .. } => Ok(()),
    }
}

#[cfg(test)]
#[path = "constraint_generator_tests.rs"]
mod tests;
