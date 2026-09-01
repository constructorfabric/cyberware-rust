//! Materializes RBAC scope types into concrete tenant / resource ID lists.
//! The plugin's `evaluate()` calls `materialize_scope` after the policy
//! evaluator returns `Allowed`; constraint generation consumes the result.
//!
//! This module is the DECISION half. Hierarchy I/O — queries, cursors, page
//! draining, `IN`-list chunking, SDK error mapping — sits behind
//! [`HierarchyUpstream`] and is implemented in `crate::infra::hierarchy_upstream`.
//! Nothing here knows a resolver SDK type, and the module holds no infra
//! handle: it composes a port with a cache.
//!
//! Every upstream read goes through `HierarchyCache::get_or_fetch`, so repeated
//! evaluations inside the TTL window share one round-trip. The cache is owned
//! here rather than by the adapter, which keeps the adapter's round-trip metric
//! a true miss count.
//!
//! ## What stays on this side of the port
//!
//! The port reports upstream FACTS; the security decisions on them are here:
//!
//! * the `[Active]` clamp on a granted root (design §3.6) — which is why
//!   [`HierarchyUpstream::tenant_subtree`] hands back the root's status instead
//!   of folding the root into the id list;
//! * the fail-closed refusal to emit a group constraint whose resources have no
//!   owning tenant;
//! * the group-ownership comparison in `validate_group_tenant_ownership`.
//!
//! Moving any of them into the adapter would put an authz rule where a
//! transport concern belongs.

use std::sync::Arc;

use authz_resolver_sdk::models::{
    BarrierMode as SdkBarrierMode, Capability, EvaluationRequest, TenantMode,
};
use rbac_sdk::models::PermissionScopeType;
use tenant_resolver_sdk::models::TenantStatus;
use toolkit_macros::domain_model;
use tracing::warn;
use uuid::Uuid;

use crate::domain::error::PluginError;
use crate::domain::hierarchy_cache::{
    CacheKey, CacheValue, HierarchyCache, TenantMetadata, hash_ids, hash_status,
};
use crate::domain::hierarchy_upstream::HierarchyUpstream;

/// Concrete materialization of a `PermissionScopeType` ready for
/// constraint generation. Each variant produces a real outcome through
/// the constraint generator — there is no deferred or stubbed path.
#[domain_model]
#[derive(Debug, Clone)]
pub(crate) enum Materialization {
    /// `mode = RootOnly` path — no descendants resolved.
    TenantDirect { tenant_id: Uuid },
    /// `Global` or `TenantSubtree` with `mode = Subtree` (default) — list
    /// of every tenant in the subtree (including the root).
    TenantSubtree { tenant_ids: Vec<Uuid> },
    /// `Global` or `TenantSubtree` with `mode = Subtree` when the PEP
    /// advertises [`Capability::TenantHierarchy`] — emitted as a push-down
    /// `InTenantSubtree` predicate instead of an eagerly expanded ID list.
    /// Carries the grant root plus the barrier mode and the DESCENDANT status
    /// filter, so the PEP's `tenant_closure` subquery needs no resolver
    /// round-trip, no `max_expansion_ids` cap, and carries no cache staleness.
    ///
    /// `status` is the request filter verbatim: empty means "every status",
    /// matching the descendant semantics of `get_tenant_subtree_ids` — a
    /// caller must be able to read the lifecycle state of its own suspended or
    /// deleted descendants, and the owning gear gates each operation on status
    /// anyway.
    ///
    /// This is NOT fully equivalent to the eager path. `get_tenant_subtree_ids`
    /// clamps the granted ROOT to `[Active]` with a second, independent filter;
    /// the push-down predicate has a single status clause covering the whole
    /// closure including the root self-row, so a Suspended or Deleted granted
    /// root survives here and not there. Accepted and specified (see design
    /// section 3.7): the closure is wider by at most the granted root, and what
    /// it widens is visibility, not action. Narrowing it requires a second
    /// status clause in the push-down predicate contract, not a change here.
    TenantSubtreePushdown {
        root_tenant_id: Uuid,
        barrier_mode: SdkBarrierMode,
        status: Vec<TenantStatus>,
    },
    /// `GroupSubtree` — flat list of resource IDs that belong to any
    /// group in the subtree of the supplied roots, plus the distinct owning
    /// tenant(s) of those groups.
    ///
    /// `owner_tenant_ids` exists for the SECURITY invariant in
    /// `RESOURCE_GROUP_MODEL.md` ("authorization always includes a tenant
    /// constraint alongside group predicates"): the constraint generator
    /// AND-pairs it with the resource-id predicate so a group constraint can
    /// never authorize a resource outside the group's tenant. Cross-tenant
    /// groups are forbidden by the RG model, so this is normally one tenant;
    /// a multi-root grant spanning sibling tenants may yield several.
    GroupSubtree {
        resource_ids: Vec<Uuid>,
        owner_tenant_ids: Vec<Uuid>,
    },
    /// `Combined { scopes }` — union of every inner scope's outputs.
    /// `group_owner_tenant_ids` carries the owning tenant(s) of the group
    /// side (see `GroupSubtree`), AND-paired into the group constraint by the
    /// constraint generator; `tenant_ids` is the OR'd tenant side and is
    /// unrelated to the group side's owning tenants.
    Combined {
        tenant_ids: Vec<Uuid>,
        resource_ids: Vec<Uuid>,
        group_owner_tenant_ids: Vec<Uuid>,
    },
    /// Reserved-variant fail-closed deny carried through to the constraint
    /// generator. `materialize_scope` constructs this for
    /// `PermissionScopeType::TenantDirect` and `ExplicitGroups` (per design
    /// §3.6 — v1 RBAC never emits these; the plugin's defensive deny
    /// path treats them as `NoMatchingPermission`-equivalent denies).
    Denied {
        error_code: &'static str,
        details: Option<String>,
    },
}

#[domain_model]
pub(crate) struct HierarchyClient {
    upstream: Arc<dyn HierarchyUpstream>,
    cache: Arc<HierarchyCache>,
}

impl HierarchyClient {
    pub(crate) fn new(upstream: Arc<dyn HierarchyUpstream>, cache: Arc<HierarchyCache>) -> Self {
        Self { upstream, cache }
    }

    // ---------- Tenant operations ----------

    /// Resolve `{root_tenant_id} ∪ descendants`. DESCENDANTS default to ALL
    /// statuses (no filter) when `tenant_status` is `None` — descendant status
    /// is a business concern AM enforces itself, not an authz-scope clamp
    /// (clamping here hid suspended/deleted descendants from a caller's
    /// lifecycle reads, e.g. suspend re-read). The granted ROOT is always
    /// clamped to `[Active]` (§3.6) — a Suspended/Deleted granted root never
    /// enters the eager allow-set.
    pub(crate) async fn get_tenant_subtree_ids(
        &self,
        root_tenant_id: Uuid,
        barrier_mode: SdkBarrierMode,
        tenant_status: Option<Vec<TenantStatus>>,
    ) -> Result<Vec<Uuid>, PluginError> {
        // Descendants: no status filter by default (empty = all statuses).
        let descendant_status: Vec<TenantStatus> = tenant_status.unwrap_or_default();
        let key = CacheKey::TenantSubtree {
            id: root_tenant_id,
            barriers_ignored: matches!(barrier_mode, SdkBarrierMode::Ignore),
            status_hash: hash_status(&descendant_status),
        };
        let upstream = Arc::clone(&self.upstream);
        let value = self
            .cache
            .get_or_fetch(key, move || async move {
                let fetched = upstream
                    .tenant_subtree(root_tenant_id, barrier_mode, descendant_status)
                    .await?;
                let mut ids = Vec::with_capacity(fetched.descendant_ids.len() + 1);
                // SECURITY (§3.6): the resolver does NOT status-filter the
                // root (it reports the granted root regardless of status), so a
                // suspended/deleted granted root would otherwise land in the
                // materialized allow-set. Apply the `[Active]` root clamp here
                // — descendants are unfiltered, but a non-active granted root
                // is excluded. The clamp lives on this side of the port
                // deliberately: it is authz policy, not an upstream fact.
                if fetched.root_status == TenantStatus::Active {
                    ids.push(fetched.root_id);
                } else {
                    warn!(
                        tenant_id = %fetched.root_id,
                        status = ?fetched.root_status,
                        "excluding non-active granted root tenant from materialized subtree"
                    );
                }
                ids.extend(fetched.descendant_ids);
                Ok(CacheValue::TenantSubtree(ids))
            })
            .await?;
        match &*value {
            CacheValue::TenantSubtree(ids) => Ok(ids.clone()),
            // The cache stores a tagged union; the key→value variant pairing
            // is an invariant of this module. A mismatch means a cache bug,
            // not a request error — fail closed with a system error rather
            // than panicking (a PDP panic = process death = fail-open).
            other => Err(PluginError::internal(format!(
                "hierarchy cache returned a non-TenantSubtree value (got {})",
                other.variant_name()
            ))),
        }
    }

    /// Project the four documented fields of `TenantInfo` for caller
    /// validation. Cached under `CacheKey::TenantMeta { id }`.
    ///
    /// Reserved: no v1 evaluation path calls it — root validation is implicit
    /// in the RBAC scope.
    /// Kept rather than deleted so the contract stays visible.
    #[allow(dead_code)]
    pub(crate) async fn validate_tenant_root(
        &self,
        tenant_id: Uuid,
    ) -> Result<TenantMetadata, PluginError> {
        let key = CacheKey::TenantMeta { id: tenant_id };
        let upstream = Arc::clone(&self.upstream);
        let value = self
            .cache
            .get_or_fetch(key, move || async move {
                Ok(CacheValue::TenantMeta(
                    upstream.tenant_metadata(tenant_id).await?,
                ))
            })
            .await?;
        match &*value {
            CacheValue::TenantMeta(meta) => Ok(meta.clone()),
            // Invariant violation (see get_tenant_subtree_ids) — fail closed.
            other => Err(PluginError::internal(format!(
                "hierarchy cache returned a non-TenantMeta value (got {})",
                other.variant_name()
            ))),
        }
    }

    // ---------- Resource group operations ----------

    /// Resolve the union of resource IDs reachable through the subtrees
    /// rooted at the supplied groups, together with the distinct owning
    /// tenant(s) of every group in those subtrees. Order-independent cache key.
    ///
    /// Returns `(resource_ids, owner_tenant_ids)`. `owner_tenant_ids` is
    /// captured from the group hierarchy (`get_group_descendants` returns
    /// `depth >= 0`, so its page includes every root group itself, not just
    /// descendants — every group's `hierarchy.tenant_id` is therefore in
    /// reach without an extra `get_group` call). The constraint generator
    /// AND-pairs it with the resource-id predicate to enforce the RG model's
    /// "tenant constraint always applies alongside group predicates"
    /// invariant. On the success path it is non-empty whenever any group
    /// resolved; an empty result with non-empty resources is treated as a
    /// fail-closed integrity violation.
    pub(crate) async fn get_group_subtree_resource_ids(
        &self,
        root_group_ids: &[Uuid],
    ) -> Result<(Vec<Uuid>, Vec<Uuid>), PluginError> {
        let key = CacheKey::GroupSubtree {
            ids_hash: hash_ids(root_group_ids),
        };
        let roots = root_group_ids.to_vec();
        let upstream = Arc::clone(&self.upstream);
        let value = self
            .cache
            .get_or_fetch(key, move || async move {
                let fetched = upstream.group_subtree(&roots).await?;
                // SECURITY fail-closed: resources were materialized but no
                // owning tenant was resolved — the group hierarchy would have
                // to be inconsistent (a membership in a group not returned by
                // the descendant walk). Refuse to emit a tenant-less group
                // constraint rather than risk a cross-tenant leak. Checked on
                // this side of the port: an inconsistent hierarchy is a fact
                // upstream is entitled to report, and refusing to act on it is
                // the authz decision.
                if !fetched.resource_ids.is_empty() && fetched.owner_tenant_ids.is_empty() {
                    warn!(
                        resource_count = fetched.resource_ids.len(),
                        "group subtree resolved resources but no owning tenant"
                    );
                    return Err(PluginError::internal(
                        "group subtree resolved resources with no owning tenant",
                    ));
                }
                Ok(CacheValue::GroupSubtree {
                    resource_ids: fetched.resource_ids,
                    owner_tenant_ids: fetched.owner_tenant_ids,
                })
            })
            .await?;
        match &*value {
            CacheValue::GroupSubtree {
                resource_ids,
                owner_tenant_ids,
            } => Ok((resource_ids.clone(), owner_tenant_ids.clone())),
            // Invariant violation (see get_tenant_subtree_ids) — fail closed.
            other => Err(PluginError::internal(format!(
                "hierarchy cache returned a non-GroupSubtree value (got {})",
                other.variant_name()
            ))),
        }
    }

    /// Resolve direct (flat) memberships only. Reserved for the future
    /// `ExplicitGroups` scope; v1 callers don't reach this path through
    /// `materialize_scope`.
    #[allow(dead_code)]
    pub(crate) async fn get_group_member_resource_ids(
        &self,
        group_ids: &[Uuid],
    ) -> Result<Vec<Uuid>, PluginError> {
        let key = CacheKey::GroupMembers {
            ids_hash: hash_ids(group_ids),
        };
        let groups = group_ids.to_vec();
        let upstream = Arc::clone(&self.upstream);
        let value = self
            .cache
            .get_or_fetch(key, move || async move {
                Ok(CacheValue::GroupMembers(
                    upstream.group_member_resource_ids(&groups).await?,
                ))
            })
            .await?;
        match &*value {
            CacheValue::GroupMembers(ids) => Ok(ids.clone()),
            // Invariant violation (see get_tenant_subtree_ids) — fail closed.
            other => Err(PluginError::internal(format!(
                "hierarchy cache returned a non-GroupMembers value (got {})",
                other.variant_name()
            ))),
        }
    }

    /// Security check: ensure the group's `hierarchy.tenant_id` matches the
    /// supplied tenant. Not cached — caching a wrong answer would leak
    /// access after an admin re-assigns a group.
    ///
    /// Reserved: v1 constraint generation does not call it. Kept rather than
    /// deleted so the contract and
    /// its fail-closed semantics stay visible.
    #[allow(dead_code)]
    pub(crate) async fn validate_group_tenant_ownership(
        &self,
        group_id: Uuid,
        expected_tenant_id: Uuid,
    ) -> Result<(), PluginError> {
        let actual_tenant_id = self.upstream.group_owner_tenant_id(group_id).await?;
        if actual_tenant_id == expected_tenant_id {
            Ok(())
        } else {
            Err(PluginError::internal(format!(
                "group ownership mismatch: group {group_id} owned by tenant {actual_tenant_id}, expected {expected_tenant_id}"
            )))
        }
    }

    // ---------- materialize_scope ----------

    /// Map an RBAC `PermissionScopeType` to the concrete `Materialization`
    /// that constraint generation inlines into `AuthZEN`
    /// `In`/`Eq` predicates.
    pub(crate) async fn materialize_scope(
        &self,
        scope_type: &PermissionScopeType,
        request: &EvaluationRequest,
    ) -> Result<Materialization, PluginError> {
        // Top-level entry: push-down is permitted when the PEP advertises
        // `Capability::TenantHierarchy`. Inside a `Combined` aggregation the
        // recursion forces eager expansion (see `materialize_scope_impl`) so the
        // ID lists can be unioned — a push-down predicate carries no IDs to fold.
        let allow_pushdown = advertises_tenant_hierarchy(request);
        self.materialize_scope_impl(scope_type, request, allow_pushdown)
            .await
    }

    // Scope-shape dispatch with per-shape fail-closed paths.
    #[allow(clippy::cognitive_complexity)]
    async fn materialize_scope_impl(
        &self,
        scope_type: &PermissionScopeType,
        request: &EvaluationRequest,
        allow_pushdown: bool,
    ) -> Result<Materialization, PluginError> {
        let (barrier_mode, status, mode) = resolve_tenant_context(request);

        match scope_type {
            PermissionScopeType::Global => {
                // Per design §3.6: Global → TenantSubtree(root_tenant).
                // RootOnly short-circuits the descendants call — the root id
                // alone is enough.
                let root_id = self.upstream.root_tenant_id().await?;
                match mode {
                    TenantMode::RootOnly => {
                        Ok(Materialization::TenantDirect { tenant_id: root_id })
                    }
                    TenantMode::Subtree if allow_pushdown => {
                        Ok(Materialization::TenantSubtreePushdown {
                            root_tenant_id: root_id,
                            barrier_mode,
                            // No status filter by default (empty = all)
                            // so AM's lifecycle reads see suspended/deleted
                            // tenants (the just-suspended tenant must survive the
                            // re-read). AM enforces status per-op. NB the single
                            // closure-status clause also governs the root self-row,
                            // so unlike the eager path this does not status-exclude a
                            // suspended granted root — acceptable: AM gates ops.
                            status: status.unwrap_or_default(),
                        })
                    }
                    TenantMode::Subtree => {
                        let tenant_ids = self
                            .get_tenant_subtree_ids(root_id, barrier_mode, status)
                            .await?;
                        Ok(Materialization::TenantSubtree { tenant_ids })
                    }
                }
            }
            PermissionScopeType::TenantSubtree { root_tenant_id } => match mode {
                TenantMode::RootOnly => Ok(Materialization::TenantDirect {
                    tenant_id: *root_tenant_id,
                }),
                TenantMode::Subtree if allow_pushdown => {
                    Ok(Materialization::TenantSubtreePushdown {
                        root_tenant_id: *root_tenant_id,
                        barrier_mode,
                        // No status filter by default (empty = all) so
                        // AM's lifecycle reads see suspended/deleted tenants; AM
                        // enforces status per-op. (Push-down's single closure-
                        // status clause governs the root self-row too, so unlike
                        // the eager path it does not status-exclude a suspended
                        // granted root — acceptable: AM gates ops.)
                        status: status.unwrap_or_default(),
                    })
                }
                TenantMode::Subtree => {
                    let tenant_ids = self
                        .get_tenant_subtree_ids(*root_tenant_id, barrier_mode, status)
                        .await?;
                    Ok(Materialization::TenantSubtree { tenant_ids })
                }
            },
            PermissionScopeType::GroupSubtree { root_group_ids } => {
                let (resource_ids, owner_tenant_ids) =
                    self.get_group_subtree_resource_ids(root_group_ids).await?;
                Ok(Materialization::GroupSubtree {
                    resource_ids,
                    owner_tenant_ids,
                })
            }
            PermissionScopeType::Combined { scopes } => {
                // Fail-closed pre-scan: if ANY inner scope (recursively) is
                // a reserved variant, deny the whole Combined up front — before
                // doing any tenant/group resolution work for the legitimate
                // sub-scopes. This avoids wasted resolver calls on a request that
                // will be denied anyway, and never emits partial constraints.
                if let Some(variant) = scopes.iter().find_map(first_reserved_variant) {
                    return Ok(Materialization::Denied {
                        error_code: crate::domain::deny::error_codes::INSUFFICIENT_PERMISSIONS_V1,
                        details: Some(format!("rbac returned reserved scope variant: {variant}")),
                    });
                }
                let mut all_tenants: Vec<Uuid> = Vec::new();
                let mut all_resources: Vec<Uuid> = Vec::new();
                // Owning tenants of the GROUP side only — kept separate from
                // `all_tenants` (the OR'd tenant-scope side) so the constraint
                // generator can AND-pair them with the group predicate.
                let mut all_group_tenants: Vec<Uuid> = Vec::new();
                for inner in scopes {
                    // Recurse via Box::pin to satisfy the async-fn-recursion rules.
                    // `allow_pushdown = false`: a Combined aggregates concrete ID
                    // lists, so inner tenant scopes must expand eagerly here.
                    let inner_result =
                        Box::pin(self.materialize_scope_impl(inner, request, false)).await?;
                    match inner_result {
                        // Inner Denied short-circuits the whole Combined
                        // (fail-closed: never emit partial constraints
                        // when one sub-scope is a reserved-variant deny).
                        Materialization::Denied {
                            error_code,
                            details,
                        } => {
                            return Ok(Materialization::Denied {
                                error_code,
                                details,
                            });
                        }
                        Materialization::TenantDirect { tenant_id } => all_tenants.push(tenant_id),
                        Materialization::TenantSubtree { mut tenant_ids } => {
                            all_tenants.append(&mut tenant_ids);
                        }
                        // `allow_pushdown = false` on the recursion above, so a
                        // push-down can never surface here. Fail closed rather
                        // than silently drop the scope if that invariant breaks.
                        Materialization::TenantSubtreePushdown { .. } => {
                            debug_assert!(
                                false,
                                "tenant-subtree push-down surfaced inside Combined (allow_pushdown=false)"
                            );
                            return Ok(Materialization::Denied {
                                error_code:
                                    crate::domain::deny::error_codes::INSUFFICIENT_PERMISSIONS_V1,
                                details: Some(
                                    "internal: tenant-subtree push-down inside Combined".to_owned(),
                                ),
                            });
                        }
                        Materialization::GroupSubtree {
                            mut resource_ids,
                            mut owner_tenant_ids,
                        } => {
                            all_resources.append(&mut resource_ids);
                            all_group_tenants.append(&mut owner_tenant_ids);
                        }
                        Materialization::Combined {
                            mut tenant_ids,
                            mut resource_ids,
                            mut group_owner_tenant_ids,
                        } => {
                            all_tenants.append(&mut tenant_ids);
                            all_resources.append(&mut resource_ids);
                            all_group_tenants.append(&mut group_owner_tenant_ids);
                        }
                    }
                }
                all_tenants.sort();
                all_tenants.dedup();
                all_resources.sort();
                all_resources.dedup();
                all_group_tenants.sort();
                all_group_tenants.dedup();
                Ok(Materialization::Combined {
                    tenant_ids: all_tenants,
                    resource_ids: all_resources,
                    group_owner_tenant_ids: all_group_tenants,
                })
            }
            // Reserved variants — per design §3.6, v1 RBAC does not emit
            // these. The plugin's defensive deny path surfaces them as a
            // typed `Materialization::Denied` (the constraint generator
            // turns it into an `Ok(decision=false, insufficient_permissions.v1)`
            // business deny — NOT an infrastructure error).
            other => Ok(Materialization::Denied {
                error_code: crate::domain::deny::error_codes::INSUFFICIENT_PERMISSIONS_V1,
                // Name the variant only — the full `{other:?}` would put the
                // RBAC scope's tenant/group IDs into a client-facing deny detail.
                details: Some(format!(
                    "rbac returned reserved scope variant: {}",
                    match other {
                        PermissionScopeType::TenantDirect { .. } => "TenantDirect",
                        PermissionScopeType::ExplicitGroups { .. } => "ExplicitGroups",
                        _ => "unrecognized",
                    }
                )),
            }),
        }
    }
}

/// Return the name of the first reserved scope variant found in `scope`
/// (recursing into nested `Combined`), or `None` if every variant is one v1
/// RBAC may legitimately emit (`Global` / `TenantSubtree` / `GroupSubtree`).
///
/// Reserved = `TenantDirect`, `ExplicitGroups`, or any future/unrecognized
/// variant — all treated as fail-closed denies (per design §3.6). Used by the
/// `Combined` arm to short-circuit before doing any hierarchy resolution.
fn first_reserved_variant(scope: &PermissionScopeType) -> Option<&'static str> {
    match scope {
        PermissionScopeType::Global
        | PermissionScopeType::TenantSubtree { .. }
        | PermissionScopeType::GroupSubtree { .. } => None,
        PermissionScopeType::TenantDirect { .. } => Some("TenantDirect"),
        PermissionScopeType::ExplicitGroups { .. } => Some("ExplicitGroups"),
        PermissionScopeType::Combined { scopes } => scopes.iter().find_map(first_reserved_variant),
        // Any variant not recognized above is reserved/unknown — fail closed.
        _ => Some("unrecognized"),
    }
}

/// True when the PEP advertises [`Capability::TenantHierarchy`], i.e. it can
/// compile an `InTenantSubtree` push-down predicate against the co-located
/// `tenant_closure` table. Gates the push-down vs eager-materialization choice.
fn advertises_tenant_hierarchy(request: &EvaluationRequest) -> bool {
    request
        .context
        .capabilities
        .iter()
        .any(|c| matches!(c, Capability::TenantHierarchy))
}

fn resolve_tenant_context(
    request: &EvaluationRequest,
) -> (SdkBarrierMode, Option<Vec<TenantStatus>>, TenantMode) {
    let tc = request.context.tenant_context.as_ref();
    let barrier_mode = tc.map_or(SdkBarrierMode::Respect, |tc| tc.barrier_mode);
    let status = tc
        .and_then(|tc| tc.tenant_status.as_ref())
        .and_then(|statuses| {
            let parsed: Vec<TenantStatus> = statuses
                .iter()
                .filter_map(|s| match s.as_str() {
                    "active" => Some(TenantStatus::Active),
                    "suspended" => Some(TenantStatus::Suspended),
                    "deleted" => Some(TenantStatus::Deleted),
                    other => {
                        warn!(
                            tenant_status = other,
                            "ignoring unrecognized tenant_status value from request"
                        );
                        None
                    }
                })
                .collect();
            // If every supplied value was unrecognized (or the list was empty),
            // return `None` so `get_tenant_subtree_ids` applies the documented
            // `[Active]` default — never an empty filter (whose resolver-side
            // meaning, "all statuses", would wrongly include suspended/deleted).
            if parsed.is_empty() {
                None
            } else {
                Some(parsed)
            }
        });
    let mode = tc.map_or(TenantMode::Subtree, |tc| tc.mode.clone());
    (barrier_mode, status, mode)
}

#[cfg(test)]
#[path = "hierarchy_client_tests.rs"]
mod tests;
