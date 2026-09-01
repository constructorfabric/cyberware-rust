//! Scope Validator — pure-domain service that resolves and validates
//! RBAC scope paths against the live tenant hierarchy and resource-group
//! registry.
//!
//! ## Public methods (all `async`, accept `&SecurityContext`)
//!
//! * [`ScopeValidator::validate_scope_exists`] — confirm every entity
//!   in the scope path exists in the backend; RG scopes additionally
//!   require the RG to be owned by the claimed tenant.
//! * [`ScopeValidator::get_ancestor_scopes`] — root-to-leaf ordered
//!   ancestor chain including the scope itself. `get_group` is NEVER
//!   called — RG hierarchy is flat within a tenant.
//! * [`ScopeValidator::is_ancestor`] — whether `potential_ancestor` is
//!   an ancestor of (or equal to) `descendant`. Deliberately diverges
//!   from upstream `is_ancestor`:
//!   - **Self**: upstream returns `false`; this returns `true` (a scope
//!     is its own ancestor for `assignable_scopes` enforcement).
//!   - **Root**: `is_ancestor("/", anything) = true`; upstream has no
//!     root concept.
//!     These short-circuits are intentional. Do NOT "fix" them — doing so
//!     would break `assignable_scopes` enforcement.
//!
//! ## `BarrierMode::Ignore` discipline
//!
//! Every upstream tenant-resolver call passes `BarrierMode::Ignore`.
//! Using `BarrierMode::Respect` here would silently drop ancestors above
//! a `self_managed` barrier, hiding legitimate inherited grants. Spelled
//! out explicitly at every call site so a future change cannot silently
//! flip the behaviour.
//!
//! This is consistent with the rest of RBAC rather than a gap: the gear's
//! own decision point is [`PermissionEvaluator`], not the `AuthZ`
//! Resolver Plugin, and it resolves its ancestor chain with
//! `BarrierMode::Ignore` too. So a grant already inherits across a
//! barrier when RBAC evaluates it, and making containment stricter than
//! evaluation would reject assignments the evaluator would then honour.
//! That matters because [`ScopeValidator::is_ancestor`] decides
//! `assignable_scopes` containment, not just read-side chains — barriers
//! are enforced by the plugin that other gears use as their PEP, and by
//! the separate `write` check at the target scope.
//!
//! [`PermissionEvaluator`]: crate::domain::service::permission_evaluator::PermissionEvaluator
//!
//! ## Boundary against `rbac-sdk::RbacServiceError`
//!
//! [`ScopeError`] is `pub(crate)` and does NOT appear in `rbac-sdk`;
//! scope errors are mapped to SDK variants (`ScopeNotFound`,
//! `InvalidScope`) and HTTP 404 / 422 in `api/rest/error.rs`.

use std::sync::Arc;

use tenant_resolver_sdk::TenantResolverClient;
use tenant_resolver_sdk::error::TenantResolverError;
use tenant_resolver_sdk::models::{BarrierMode, GetAncestorsOptions, IsAncestorOptions, TenantId};
use thiserror::Error;
use toolkit_macros::domain_model;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::rg_port::RbacRgRead;

/// Canonical `Scope` value type lives in `rbac-sdk`; re-imported here
/// at the call sites that need it.
use rbac_sdk::models::Scope;

/// Error returned by every [`ScopeValidator`] method.
///
/// Variants map to HTTP statuses in `api/rest/error.rs`:
/// - [`ScopeError::InvalidScopeFormat`] → 422
/// - [`ScopeError::ScopeNotFound`] → 404
/// - [`ScopeError::Upstream`] → 503
#[domain_model]
#[derive(Debug, Error)]
pub enum ScopeError {
    /// The scope string does not match any of the three legal path
    /// forms. Returned BEFORE any external call.
    #[error("scope has invalid format: {scope}")]
    InvalidScopeFormat {
        /// The offending scope string, reproduced verbatim.
        scope: String,
    },

    /// Scope syntactically valid but a named entity is missing, or
    /// (RG scopes) the RG is owned by a different tenant. HTTP 404
    /// regardless of the `missing` discriminant.
    #[error("scope not found: {scope}")]
    ScopeNotFound {
        /// The offending scope string, reproduced verbatim.
        scope: String,
        /// Which entity was missing or mismatched.
        missing: MissingScopeEntity,
    },

    /// A non-"not-found" upstream error. Only `TenantNotFound` /
    /// `ResourceGroupError::NotFound` translate to
    /// [`ScopeError::ScopeNotFound`]; every other error flows here
    /// unchanged.
    #[error("upstream error while resolving scope: {0}")]
    Upstream(UpstreamScopeError),
}

/// Identifies which entity was absent or mismatched when
/// [`ScopeError::ScopeNotFound`] is produced.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MissingScopeEntity {
    /// Tenant UUID not found in the hierarchy.
    Tenant { id: Uuid },

    /// Resource-group UUID not found.
    ResourceGroup { id: Uuid },

    /// RG row exists but its `owner_tenant_id` is not the tenant
    /// claimed in the scope path. HTTP-indistinguishable from a missing
    /// RG (still 404); the discriminant is preserved for diagnostics.
    ResourceGroupOwnerMismatch {
        rg_id: Uuid,
        claimed_tenant_id: Uuid,
        actual_tenant_id: Uuid,
    },
}

// Manual `PartialEq` because `UpstreamScopeError` wraps types without their
// own `PartialEq`. `Upstream` values compare by `Display`, which keeps the
// impl reflexive (`a == a` holds); treating them as never-equal would break
// assertions over `Result<_, ScopeError>`.
impl PartialEq for ScopeError {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::InvalidScopeFormat { scope: a }, Self::InvalidScopeFormat { scope: b }) => {
                a == b
            }
            (
                Self::ScopeNotFound {
                    scope: a,
                    missing: am,
                },
                Self::ScopeNotFound {
                    scope: b,
                    missing: bm,
                },
            ) => a == b && am == bm,
            (Self::Upstream(a), Self::Upstream(b)) => a.to_string() == b.to_string(),
            _ => false,
        }
    }
}

/// Lift a domain-internal [`ScopeError`] to the unified
/// [`DomainError`] surface. The richer diagnostic discriminator on
/// [`ScopeError::ScopeNotFound`] (which entity was missing) is
/// preserved only on the audit `tracing` event emitted upstream; the
/// public envelope sees only the offending scope path.
impl From<ScopeError> for DomainError {
    fn from(err: ScopeError) -> Self {
        match err {
            ScopeError::InvalidScopeFormat { scope } => DomainError::InvalidScopeFormat { scope },
            ScopeError::ScopeNotFound { scope, missing: _ } => DomainError::ScopeNotFound { scope },
            ScopeError::Upstream(upstream) => DomainError::ServiceUnavailable {
                detail: format!("scope resolution upstream failure: {upstream}"),
                retry_after: None,
                // Preserve the typed upstream in the `source()` chain
                // (`BoxError = Arc<...>` is cloneable for
                // exactly this) instead of flattening it into `detail`,
                // matching the role-assignment RG path. Keeps
                // `source()`-based triage working for scope-resolution
                // outages.
                cause: Some(Arc::new(upstream)),
            },
        }
    }
}

/// Wraps an upstream client error so the future REST mapper can route it to
/// 503 without losing the original diagnostic.
#[domain_model]
#[derive(Debug, Error)]
pub enum UpstreamScopeError {
    /// An error from [`TenantResolverClient`] that is not
    /// [`TenantResolverError::TenantNotFound`].
    #[error("tenant resolver: {0}")]
    Tenant(TenantResolverError),

    /// An error from [`RbacRgRead`] that is not
    /// [`crate::domain::rg_port::RbacRgReadError::NotFound`]. Carries the
    /// RBAC-owned upstream
    /// projection so the domain layer never sees
    /// `resource_group_sdk::error::ResourceGroupError`
    /// directly.
    #[error("resource group: {0}")]
    ResourceGroup(crate::domain::rg_port::RbacRgReadError),
}

/// Domain service that validates and resolves RBAC scope paths.
#[domain_model]
pub struct ScopeValidator {
    /// Upstream tenant hierarchy client; every call passes
    /// `BarrierMode::Ignore` (see module docs).
    tenant_resolver: Arc<dyn TenantResolverClient>,

    /// Internal RBAC port for resource-group lookups.
    rg_read: Arc<dyn RbacRgRead>,
}

impl ScopeValidator {
    /// Construct a new `ScopeValidator` from the two resolved upstream
    /// clients. No I/O is performed at construction time.
    pub fn new(
        tenant_resolver: Arc<dyn TenantResolverClient>,
        rg_read: Arc<dyn RbacRgRead>,
    ) -> Self {
        Self {
            tenant_resolver,
            rg_read,
        }
    }

    /// Check that all entities named in a scope path exist in the
    /// backend. For RG scopes the tenant is verified first, so
    /// `get_group` is never called if the tenant is absent; the RG
    /// must additionally be owned by the claimed tenant.
    pub async fn validate_scope_exists(
        &self,
        ctx: &SecurityContext,
        scope: &str,
    ) -> Result<(), ScopeError> {
        match parse_scope(scope)? {
            Scope::Root => Ok(()),

            Scope::Tenant { tenant_id } => {
                match self
                    .tenant_resolver
                    .get_tenant(ctx, TenantId(tenant_id))
                    .await
                {
                    Ok(_) => Ok(()),
                    Err(TenantResolverError::TenantNotFound { .. }) => {
                        Err(ScopeError::ScopeNotFound {
                            scope: scope.to_owned(),
                            missing: MissingScopeEntity::Tenant { id: tenant_id },
                        })
                    }
                    Err(e) => Err(ScopeError::Upstream(UpstreamScopeError::Tenant(e))),
                }
            }

            Scope::ResourceGroup {
                tenant_id,
                group_id,
            } => {
                // Tenant check first — short-circuits before the RG
                // lookup if the tenant is absent.
                match self
                    .tenant_resolver
                    .get_tenant(ctx, TenantId(tenant_id))
                    .await
                {
                    Ok(_) => {}
                    Err(TenantResolverError::TenantNotFound { .. }) => {
                        return Err(ScopeError::ScopeNotFound {
                            scope: scope.to_owned(),
                            missing: MissingScopeEntity::Tenant { id: tenant_id },
                        });
                    }
                    Err(e) => return Err(ScopeError::Upstream(UpstreamScopeError::Tenant(e))),
                }

                let rg = match self.rg_read.get_group(ctx, group_id).await {
                    Ok(rg) => rg,
                    Err(crate::domain::rg_port::RbacRgReadError::NotFound) => {
                        return Err(ScopeError::ScopeNotFound {
                            scope: scope.to_owned(),
                            missing: MissingScopeEntity::ResourceGroup { id: group_id },
                        });
                    }
                    Err(e) => {
                        return Err(ScopeError::Upstream(UpstreamScopeError::ResourceGroup(e)));
                    }
                };

                // Ownership match — RG must be owned by the claimed tenant.
                if rg.tenant_id != tenant_id {
                    return Err(ScopeError::ScopeNotFound {
                        scope: scope.to_owned(),
                        missing: MissingScopeEntity::ResourceGroupOwnerMismatch {
                            rg_id: group_id,
                            claimed_tenant_id: tenant_id,
                            actual_tenant_id: rg.tenant_id,
                        },
                    });
                }

                Ok(())
            }
            // `Scope` is `#[non_exhaustive]` (lives in `rbac-sdk`).
            // Any future variant lands here and is treated as a
            // malformed scope until validation logic is extended to
            // honour it.
            _ => Err(ScopeError::InvalidScopeFormat {
                scope: scope.to_owned(),
            }),
        }
    }

    /// Root-to-leaf ordered ancestor chain for a scope, inclusive. The
    /// result starts with `"/"` and ends with the requested scope; RG
    /// scopes append the RG path to the parent-tenant chain.
    /// `get_group` is never called — RG hierarchy is flat per tenant.
    pub async fn get_ancestor_scopes(
        &self,
        ctx: &SecurityContext,
        scope: &str,
    ) -> Result<Vec<String>, ScopeError> {
        match parse_scope(scope)? {
            Scope::Root => Ok(vec!["/".to_owned()]),

            Scope::Tenant { tenant_id } => self.tenant_ancestor_chain(ctx, tenant_id, scope).await,

            Scope::ResourceGroup {
                tenant_id,
                group_id,
            } => {
                let mut chain = self.tenant_ancestor_chain(ctx, tenant_id, scope).await?;
                chain.push(format!("/tenants/{tenant_id}/resourceGroups/{group_id}"));
                Ok(chain)
            }
            _ => Err(ScopeError::InvalidScopeFormat {
                scope: scope.to_owned(),
            }),
        }
    }

    /// Root-to-leaf ancestor chain for `tenant_id`, inclusive. Calls
    /// `get_ancestors` with `BarrierMode::Ignore` and reverses the
    /// parent-to-root response.
    async fn tenant_ancestor_chain(
        &self,
        ctx: &SecurityContext,
        tenant_id: Uuid,
        scope: &str,
    ) -> Result<Vec<String>, ScopeError> {
        let response = match self
            .tenant_resolver
            .get_ancestors(
                ctx,
                TenantId(tenant_id),
                &GetAncestorsOptions {
                    barrier_mode: BarrierMode::Ignore,
                },
            )
            .await
        {
            Ok(r) => r,
            Err(TenantResolverError::TenantNotFound { .. }) => {
                return Err(ScopeError::ScopeNotFound {
                    scope: scope.to_owned(),
                    missing: MissingScopeEntity::Tenant { id: tenant_id },
                });
            }
            Err(e) => return Err(ScopeError::Upstream(UpstreamScopeError::Tenant(e))),
        };

        // Upstream `ancestors` is parent-to-root ordered; reverse and
        // sandwich with the synthetic root scope and the requested
        // tenant.
        let mut chain = Vec::with_capacity(response.ancestors.len() + 2);
        chain.push("/".to_owned());
        for ancestor in response.ancestors.iter().rev() {
            chain.push(format!("/tenants/{}", ancestor.id.0));
        }
        chain.push(format!("/tenants/{tenant_id}"));
        Ok(chain)
    }

    /// Check whether `potential_ancestor` is an ancestor of (or equal
    /// to) `descendant`. See module docs for the deliberate divergences
    /// from upstream `is_ancestor` (self → `true`, root → `true`); do
    /// NOT "fix" them or `assignable_scopes` breaks.
    ///
    /// Inputs are parsed FIRST so malformed-but-byte-equal inputs
    /// produce `InvalidScopeFormat` rather than a byte-equality
    /// shortcut to `Ok(true)`. Same-tenant pairs short-circuit before
    /// delegating (a `Tenant` IS an ancestor of any RG in the same
    /// tenant; sibling RGs are NOT ancestors of each other).
    /// A `ResourceGroup` ancestor answers `false` for anything but
    /// itself, in any tenant — RG hierarchy is flat, and only a
    /// `Tenant` or `Root` ancestor can span a subtree.
    /// Cross-tenant tenant pairs delegate to upstream with
    /// `BarrierMode::Ignore`.
    ///
    /// # Errors
    ///
    /// * `InvalidScopeFormat` — either argument is not a legal scope path.
    /// * `ScopeNotFound` — the tenant hierarchy has no such tenant, for
    ///   either endpoint. Callers that treat a dangling scope as "no
    ///   match" should handle this variant rather than propagate it.
    /// * `Upstream` — the tenant resolver failed for any other reason.
    pub async fn is_ancestor(
        &self,
        ctx: &SecurityContext,
        potential_ancestor: &str,
        descendant: &str,
    ) -> Result<bool, ScopeError> {
        let anc = parse_scope(potential_ancestor)?;
        let desc = parse_scope(descendant)?;

        if anc == desc {
            return Ok(true);
        }

        let anc_tenant = match &anc {
            Scope::Root => return Ok(true),
            Scope::Tenant { tenant_id } => *tenant_id,
            // A resource group is an ancestor of nothing but itself. RG
            // hierarchy is flat within a tenant, so the only `true` case
            // is the equal-scope one already answered above.
            //
            // Answering here — rather than falling through — is what
            // keeps the cross-tenant delegation below from reducing an
            // RG to its bare tenant id. That reduction would make
            // `/tenants/P/resourceGroups/G` an "ancestor" of every scope
            // under every descendant of P, silently widening an
            // `assignable_scopes` entry from one resource group to a
            // whole subtree.
            Scope::ResourceGroup { .. } => return Ok(false),
            // `Scope` is `#[non_exhaustive]`; the wildcard is required for
            // forward compat even though it's currently unreachable.
            _ => {
                return Err(ScopeError::InvalidScopeFormat {
                    scope: potential_ancestor.to_owned(),
                });
            }
        };
        let desc_tenant = match &desc {
            // Asymmetric root: no non-root scope is an ancestor of root.
            Scope::Root => return Ok(false),
            Scope::Tenant { tenant_id } | Scope::ResourceGroup { tenant_id, .. } => *tenant_id,
            _ => {
                return Err(ScopeError::InvalidScopeFormat {
                    scope: descendant.to_owned(),
                });
            }
        };

        // Same-tenant short-circuit: `anc` is a Tenant scope by now (the
        // RG arm returned above), so it IS the ancestor of every RG in
        // that tenant. A Tenant/Tenant pair cannot reach here — equal
        // scopes returned `true` at the top.
        if anc_tenant == desc_tenant {
            return Ok(matches!(&desc, Scope::ResourceGroup { .. }));
        }

        self.tenant_resolver
            .is_ancestor(
                ctx,
                TenantId(anc_tenant),
                TenantId(desc_tenant),
                &IsAncestorOptions {
                    barrier_mode: BarrierMode::Ignore,
                },
            )
            .await
            .map_err(|e| match e {
                // Honour the `ScopeError` contract: a missing entity is
                // `ScopeNotFound`, and only genuine upstream failures are
                // `Upstream`. Without this, a tenant that was deleted
                // after a role recorded it reaches the caller as an
                // opaque 500 ("retry later") instead of a fact about the
                // scope. Mirrors the same mapping in `tenant_chain`.
                TenantResolverError::TenantNotFound { tenant_id } => ScopeError::ScopeNotFound {
                    scope: format!("/tenants/{}", tenant_id.0),
                    missing: MissingScopeEntity::Tenant { id: tenant_id.0 },
                },
                other => ScopeError::Upstream(UpstreamScopeError::Tenant(other)),
            })
    }
}

/// Parse an RBAC scope path string into a typed [`Scope`] value. Phase
/// 3 delegates to `rbac_sdk::models::Scope::parse`; this thin wrapper
/// preserves the local `ScopeError`-based contract.
pub fn parse_scope(input: &str) -> Result<Scope, ScopeError> {
    Scope::parse(input).map_err(|_| ScopeError::InvalidScopeFormat {
        scope: input.to_owned(),
    })
}

#[cfg(test)]
#[path = "scope_validator_tests.rs"]
mod scope_validator_tests;
