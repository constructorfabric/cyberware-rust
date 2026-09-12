// Updated: 2026-04-14 by Constructor Tech
//! Service implementation for the static `AuthZ` resolver plugin.

use authz_resolver_sdk::{
    Capability, Constraint, EvaluationRequest, EvaluationResponse, EvaluationResponseContext,
    InPredicate, InTenantSubtreePredicate, Predicate,
};
use toolkit_macros::domain_model;
use toolkit_security::pep_properties;
use uuid::Uuid;

/// Static `AuthZ` resolver service.
///
/// - Returns `decision: true` with an `in` predicate on `pep_properties::OWNER_TENANT_ID`
///   scoped to the context tenant from the request (for all operations including CREATE),
///   when the PEP declares that property in `context.supported_properties`.
/// - Additionally emits parallel `InTenantSubtree(<prop>, tid)` constraints (one per
///   tenant-shaped supported property) when the PEP advertises
///   [`Capability::TenantHierarchy`]. This lets entities whose `Scopable` declaration
///   is `no_tenant, resource_col = "..."` (e.g. AM's `tenants`) bind via
///   `InTenantSubtree(RESOURCE_ID, tid)`, and entities declared `tenant_col = "..."`
///   that opt-in to subtree access (e.g. AM's `tenant_metadata` / `conversion_requests`)
///   bind via `InTenantSubtree(OWNER_TENANT_ID, tid)` -- without that addition the
///   `In(OWNER_TENANT_ID)` clamp restricts visibility to the caller's own tenant row,
///   hiding direct-child writes the test fixtures exercise.
/// - Constraints are OR-ed, and the `SecureORM` compiler drops any predicate whose
///   property doesn't resolve to a column on the entity being queried -- so the
///   addition is invisible to PEPs that don't advertise the capability and to entities
///   that don't expose the property.
/// - Emits an empty constraint set when the PEP declares none of the properties above.
///   The PEP compiler turns that into `allow_all()`, or a deny under
///   `require_constraints` -- this service does not repeat that matrix. Such a permit
///   means "no row filter", never "the caller owns this row".
/// - Denies access (`decision: false`) when no valid tenant can be resolved, before any
///   constraint is built.
#[domain_model]
#[derive(Default)]
pub struct Service;

impl Service {
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// Evaluate an authorization request.
    #[must_use]
    #[allow(clippy::unused_self)] // &self reserved for future config/state
    pub fn evaluate(&self, request: &EvaluationRequest) -> EvaluationResponse {
        // Always scope to context tenant (all CRUD operations get constraints)
        let tenant_id = request
            .context
            .tenant_context
            .as_ref()
            .and_then(|t| t.root_id)
            .or_else(|| {
                // Fallback: extract tenant_id from subject properties
                request
                    .subject
                    .properties
                    .get("tenant_id")
                    .and_then(|v| v.as_str())
                    .and_then(|s| Uuid::parse_str(s).ok())
            });

        let Some(tid) = tenant_id else {
            // No tenant resolvable from context or subject - deny access.
            return EvaluationResponse {
                decision: false,
                context: EvaluationResponseContext::default(),
            };
        };

        if tid == Uuid::default() {
            // Nil UUID tenant - deny rather than grant unrestricted access.
            return EvaluationResponse {
                decision: false,
                context: EvaluationResponseContext::default(),
            };
        }

        // This static PDP has no RG client and therefore cannot resolve group
        // selectors to explicit resource IDs. Ignoring a selector would widen
        // it to tenant-wide access, so deny whenever group scoping is requested.
        if request.resource.properties.contains_key("group_ids")
            || request
                .resource
                .properties
                .contains_key("ancestor_group_ids")
        {
            return EvaluationResponse {
                decision: false,
                context: EvaluationResponseContext::default(),
            };
        }

        // Baseline OWNER_TENANT_ID clamp -- the universal shape a PEP binds when its
        // entity declares `tenant_col`. Emitted only when the PEP declares that property:
        // the compiler fails any constraint naming something outside
        // `context.supported_properties` (`authz-resolver-sdk/src/pep/compiler.rs`), so an
        // undeclared clamp is dead on arrival -- and as the only constraint it failed the
        // whole set (`AllConstraintsFailed`, a deny), losing every request for a
        // platform-global resource that declares no property at all.
        let mut constraints = Vec::new();
        if supports_property(request, pep_properties::OWNER_TENANT_ID) {
            constraints.push(Constraint {
                predicates: vec![Predicate::In(InPredicate::new(
                    pep_properties::OWNER_TENANT_ID,
                    [tid],
                ))],
            });
        }

        // Closes the gap from `gears-rust#1813` (plugin half) for the dev stack:
        // PEPs that advertise `Capability::TenantHierarchy` get an
        // `InTenantSubtree(<prop>, tid)` constraint for each
        // tenant-shaped property they declare as supported
        // (`OWNER_TENANT_ID` and `RESOURCE_ID`). The two predicates target
        // different entity shapes:
        //
        // * `InTenantSubtree(OWNER_TENANT_ID, tid)` binds against entities
        //   declared with `tenant_col` (e.g. AM's `tenant_metadata` /
        //   `conversion_requests`) and clamps to the caller's subtree --
        //   the contract the test fixtures exercise when a caller in the
        //   root tenant writes metadata on a direct child.
        // * `InTenantSubtree(RESOURCE_ID, tid)` binds against entities
        //   declared with `resource_col` (e.g. AM's `tenants` itself,
        //   `no_tenant`) and clamps via the resource id.
        //
        // The constraints are OR-ed, and the SecureORM compiler drops any
        // predicate whose property doesn't resolve to a column on the
        // entity being queried -- so the addition is invisible to PEPs
        // that don't advertise the capability and to entities that don't
        // expose the property. Gears that do not opt-in to
        // `Capability::TenantHierarchy` see the baseline shape unchanged.
        if advertises_tenant_hierarchy(request) {
            for prop in [pep_properties::OWNER_TENANT_ID, pep_properties::RESOURCE_ID] {
                if supports_property(request, prop) {
                    constraints.push(Constraint {
                        predicates: vec![Predicate::InTenantSubtree(
                            InTenantSubtreePredicate::new(prop, tid),
                        )],
                    });
                }
            }
        }

        // No constraint means the PEP declared none of the properties above. The compiler
        // owns what that means -- `allow_all()`, or a deny under `require_constraints`
        // (`compiler.rs` Step 1, pinned by the composition tests) -- so there is no deny of
        // our own here. That Step 1 returns without logging, hence the warn.
        //
        // Do not narrow the required case with `resource.properties[OWNER_TENANT_ID]`
        // instead: this plugin has no tenant tree, so it cannot tell an ancestor from a
        // stranger, and an equality check denies the ancestor-tenant requests the AM e2e
        // suites exercise.
        if constraints.is_empty() && request.context.require_constraints {
            tracing::warn!(
                resource_type = %request.resource.resource_type,
                "static-authz: PEP requires constraints but declares none of the properties \
                 this plugin constrains -- the PEP compiler will fail this closed",
            );
        }

        EvaluationResponse {
            decision: true,
            context: EvaluationResponseContext {
                constraints,
                ..Default::default()
            },
        }
    }
}

fn advertises_tenant_hierarchy(request: &EvaluationRequest) -> bool {
    request
        .context
        .capabilities
        .iter()
        .any(|c| matches!(c, Capability::TenantHierarchy))
}

fn supports_property(request: &EvaluationRequest, property: &str) -> bool {
    request
        .context
        .supported_properties
        .iter()
        .any(|p| p == property)
}

#[cfg(test)]
#[path = "service_tests.rs"]
mod service_tests;
