//! Evaluator-facing types: `SubjectRole` (subject's resolved roles in
//! context), `RolePolicy` (matcher input), `EffectivePermission` (grant
//! attribution), the `PermissionResult` outcome plus its supporting
//! payloads, and the `get_subject_roles` / `evaluate_permission`
//! request/response DTOs.

use std::cmp::Ordering;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::permission_rule::PermissionRule;
use crate::role_assignment::PrincipalType;
use crate::role_definition::RoleDefinition;
use crate::scope::Scope;

/// Matcher-input projection of a role's rule list.
///
/// Separates the editorial [`RoleDefinition`] from the matcher kernel (just
/// the rule list). `#[non_exhaustive]` — construct via [`RolePolicy::new`]
/// or the [`From<&RoleDefinition>`] / [`From<&SubjectRole>`] impls.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RolePolicy {
    /// Allow rules — first match becomes the contributing grant. Mirrors
    /// [`RoleDefinition::permissions`].
    pub permissions: Vec<PermissionRule>,
    /// Deny rules — a match here short-circuits role evaluation regardless
    /// of any allow match. Mirrors [`RoleDefinition::not_permissions`].
    pub not_permissions: Vec<PermissionRule>,
}

impl RolePolicy {
    /// Construct a [`RolePolicy`] from the two rule arrays.
    #[must_use]
    pub fn new(permissions: Vec<PermissionRule>, not_permissions: Vec<PermissionRule>) -> Self {
        Self {
            permissions,
            not_permissions,
        }
    }
}

impl From<&RoleDefinition> for RolePolicy {
    fn from(def: &RoleDefinition) -> Self {
        Self {
            permissions: def.permissions.clone(),
            not_permissions: def.not_permissions.clone(),
        }
    }
}

impl From<&SubjectRole> for RolePolicy {
    fn from(sr: &SubjectRole) -> Self {
        Self {
            permissions: sr.permissions.clone(),
            not_permissions: sr.not_permissions.clone(),
        }
    }
}

/// Outcome of a permission evaluation. Tagged with `type` on the wire.
///
/// `#[non_exhaustive]` — external `match` arms MUST end with a wildcard
/// `_ =>` arm.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
#[serde(tag = "type", rename_all = "PascalCase")]
pub enum PermissionResult {
    /// Permission granted. Carries every contributing grant plus the
    /// aggregated scope discriminator.
    Allowed(PermissionGranted),
    /// Permission denied. Carries the categorical reason.
    Denied(PermissionDenied),
}

/// Aggregated scope discriminator returned alongside `PermissionGranted`.
///
/// `#[non_exhaustive]` — consumers MUST match on known variants and treat
/// any unknown or *Reserved* variant as `Denied { NoMatchingPermission }`
/// (fail-closed).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
#[serde(tag = "type", rename_all = "PascalCase")]
pub enum PermissionScopeType {
    /// Global access (platform admin). **Active** in v1.
    Global,
    /// Access to the subtree rooted at a tenant. **Active** in v1.
    TenantSubtree {
        /// Root tenant of the subtree.
        root_tenant_id: Uuid,
    },
    /// Access to exactly one tenant, without subtree inheritance.
    /// **Reserved** — v1 producers do not emit this variant; v1 consumers
    /// MUST treat any incoming `TenantDirect` as
    /// `Denied { NoMatchingPermission }`.
    TenantDirect {
        /// Target tenant.
        tenant_id: Uuid,
    },
    /// Access to one or more resource-group subtrees, with inheritance to
    /// child groups. **Active** in v1.
    GroupSubtree {
        /// Root group IDs of the subtrees.
        root_group_ids: Vec<Uuid>,
    },
    /// Access to flat group membership only, without subtree expansion.
    /// **Reserved** — v1 producers do not emit this variant; v1 consumers
    /// MUST treat it as `Denied { NoMatchingPermission }`.
    ExplicitGroups {
        /// Group IDs the principal must be a direct member of.
        group_ids: Vec<Uuid>,
    },
    /// Multiple access paths (OR'd) — returned when a subject's roles span
    /// multiple scope types. **Active** in v1.
    Combined {
        /// Distinct contributing scopes.
        scopes: Vec<PermissionScopeType>,
    },
}

impl PermissionScopeType {
    /// Classify one persisted role-assignment scope into the active v1
    /// permission-scope shape consumed by the authorization resolver.
    ///
    /// This is the canonical assignment-to-result mapping. Keeping it in the
    /// SDK lets both the RBAC producer and its consumer validate the same
    /// provenance contract instead of maintaining independent classifiers.
    #[must_use]
    pub fn from_assignment_scope(scope: &Scope) -> Self {
        match scope {
            Scope::Root => Self::Global,
            Scope::Tenant { tenant_id } => Self::TenantSubtree {
                root_tenant_id: *tenant_id,
            },
            Scope::ResourceGroup { group_id, .. } => Self::GroupSubtree {
                root_group_ids: vec![*group_id],
            },
        }
    }

    /// Canonically aggregate active v1 permission-scope shapes.
    ///
    /// Resource-group roots are merged into one sorted, deduplicated
    /// [`Self::GroupSubtree`]. Other identical shapes are deduplicated and the
    /// complete union is sorted into canonical variant/UUID order. A single
    /// remaining shape passes through; mixed shapes become [`Self::Combined`].
    ///
    /// Returns `None` for an empty input. Callers producing an `Allowed`
    /// result MUST treat that as a fail-closed invariant violation rather than
    /// inventing a default scope.
    #[must_use]
    pub fn aggregate(scopes: &[Self]) -> Option<Self> {
        if scopes.is_empty() {
            return None;
        }

        let mut merged = Vec::with_capacity(scopes.len());
        let mut merged_group_ids = Vec::new();
        let mut group_slot = None;

        for scope in scopes {
            match scope {
                Self::GroupSubtree { root_group_ids } => {
                    for group_id in root_group_ids {
                        if !merged_group_ids.contains(group_id) {
                            merged_group_ids.push(*group_id);
                        }
                    }
                    if group_slot.is_none() {
                        group_slot = Some(merged.len());
                        // Reserve the first-seen group position. The complete,
                        // deterministic ID set is installed after the scan.
                        merged.push(Self::GroupSubtree {
                            root_group_ids: Vec::new(),
                        });
                    }
                }
                other if !merged.contains(other) => merged.push(other.clone()),
                _ => {}
            }
        }

        if let Some(slot) = group_slot {
            merged_group_ids.sort_unstable();
            if let Some(entry) = merged.get_mut(slot) {
                *entry = Self::GroupSubtree {
                    root_group_ids: merged_group_ids,
                };
            }
        }

        // Repository ordering is an implementation detail and assignments at
        // the same depth may arrive in either order. Canonicalize the complete
        // union so equivalent grant sets produce byte-for-byte equal scope
        // values at the producer and every consumer boundary.
        merged.sort_by(compare_scope_types);

        if merged.len() == 1 {
            merged.into_iter().next()
        } else {
            Some(Self::Combined { scopes: merged })
        }
    }
}

/// Total ordering used only to canonicalize [`PermissionScopeType`] values.
///
/// Active variants are ordered from broad hierarchy roots to narrower roots,
/// then by UUID. Reserved and nested values also have stable ordering so this
/// public SDK helper remains deterministic for every currently known variant.
fn compare_scope_types(left: &PermissionScopeType, right: &PermissionScopeType) -> Ordering {
    fn rank(scope: &PermissionScopeType) -> u8 {
        match scope {
            PermissionScopeType::Global => 0,
            PermissionScopeType::TenantSubtree { .. } => 1,
            PermissionScopeType::GroupSubtree { .. } => 2,
            PermissionScopeType::TenantDirect { .. } => 3,
            PermissionScopeType::ExplicitGroups { .. } => 4,
            PermissionScopeType::Combined { .. } => 5,
        }
    }

    rank(left)
        .cmp(&rank(right))
        .then_with(|| match (left, right) {
            (
                PermissionScopeType::TenantSubtree {
                    root_tenant_id: left,
                },
                PermissionScopeType::TenantSubtree {
                    root_tenant_id: right,
                },
            )
            | (
                PermissionScopeType::TenantDirect { tenant_id: left },
                PermissionScopeType::TenantDirect { tenant_id: right },
            ) => left.cmp(right),
            (
                PermissionScopeType::GroupSubtree {
                    root_group_ids: left,
                },
                PermissionScopeType::GroupSubtree {
                    root_group_ids: right,
                },
            )
            | (
                PermissionScopeType::ExplicitGroups { group_ids: left },
                PermissionScopeType::ExplicitGroups { group_ids: right },
            ) => left.cmp(right),
            (
                PermissionScopeType::Combined { scopes: left },
                PermissionScopeType::Combined { scopes: right },
            ) => compare_scope_slices(left, right),
            // Different variants were already ordered by `rank` above.
            _ => Ordering::Equal,
        })
}

/// Lexicographically compare nested scope arrays using the same canonical
/// ordering as top-level values.
fn compare_scope_slices(left: &[PermissionScopeType], right: &[PermissionScopeType]) -> Ordering {
    for (left_scope, right_scope) in left.iter().zip(right) {
        let ordering = compare_scope_types(left_scope, right_scope);
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
    left.len().cmp(&right.len())
}

/// Why an allowed permission result cannot prove that its aggregate scope came
/// from its contributing role assignments.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ScopeProvenanceError {
    /// Normal RBAC evaluation returned `Allowed` without a matching role
    /// assignment. No safe scope can be inferred from an empty grant set.
    #[error("allowed permission result has no contributing role assignments")]
    EmptyGrants,
    /// The supplied aggregate differs from the canonical aggregate of the
    /// contributing assignments and could therefore widen authorization.
    #[error("permission scope does not match contributing role assignments")]
    AggregateMismatch,
}

/// Categorical reason for a permission denial. Fail-closed default for any
/// unrecognised situation is `NoMatchingPermission`.
///
/// `#[non_exhaustive]` — external `match` arms MUST end with a wildcard
/// `_ =>` arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
#[serde(rename_all = "PascalCase")]
pub enum DenyReason {
    /// No role grants the requested operation/resource type, or no
    /// assignments were visible from `context_tenant_id`.
    NoMatchingPermission,
    /// Request matched a `not_permissions` rule in an otherwise matching
    /// role.
    NotPermissionExclusion,
}

/// Single role assignment expanded with the resolved role definition.
///
/// `#[non_exhaustive]` — construct via [`SubjectRole::new`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SubjectRole {
    /// Underlying `RoleAssignment.id`.
    pub assignment_id: Uuid,
    /// Underlying `RoleDefinition.id`.
    pub role_definition_id: Uuid,
    /// Resolved role name from the role definition.
    pub role_name: String,
    /// Allow rules copied from the role definition.
    pub permissions: Vec<PermissionRule>,
    /// Deny rules copied from the role definition.
    pub not_permissions: Vec<PermissionRule>,
    /// Assignment scope.
    pub scope: Scope,
    /// `true` when `scope` is an ancestor of the request's
    /// `context_tenant_id`; `false` when granted directly at that scope.
    pub is_inherited: bool,
    /// Underlying `RoleAssignment.principal_id`.
    pub principal_id: String,
    /// Underlying `RoleAssignment.principal_type`.
    pub principal_type: PrincipalType,
}

impl SubjectRole {
    /// Construct a [`SubjectRole`] from its currently-required fields.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        assignment_id: Uuid,
        role_definition_id: Uuid,
        role_name: impl Into<String>,
        permissions: Vec<PermissionRule>,
        not_permissions: Vec<PermissionRule>,
        scope: Scope,
        is_inherited: bool,
        principal_id: impl Into<String>,
        principal_type: PrincipalType,
    ) -> Self {
        Self {
            assignment_id,
            role_definition_id,
            role_name: role_name.into(),
            permissions,
            not_permissions,
            scope,
            is_inherited,
            principal_id: principal_id.into(),
            principal_type,
        }
    }
}

/// Single contributing grant returned inside `PermissionGranted::grants`.
///
/// `#[non_exhaustive]` — construct via [`EffectivePermission::new`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct EffectivePermission {
    /// The specific permission rule that matched the request.
    pub matched_permission: PermissionRule,
    /// Role definition that contributed the grant.
    pub role_definition_id: Uuid,
    /// Role assignment that grants this permission.
    pub role_assignment_id: Uuid,
    /// Resolved role definition name.
    pub role_name: String,
    /// Scope at which the granting role is assigned.
    pub assignment_scope: Scope,
    /// `true` when `assignment_scope` is an ancestor of the request's
    /// `context_tenant_id`; `false` when the grant was assigned directly at
    /// that scope.
    pub is_inherited: bool,
}

impl EffectivePermission {
    /// Construct an [`EffectivePermission`] from its currently-required
    /// fields.
    #[must_use]
    pub fn new(
        matched_permission: PermissionRule,
        role_definition_id: Uuid,
        role_assignment_id: Uuid,
        role_name: impl Into<String>,
        assignment_scope: Scope,
        is_inherited: bool,
    ) -> Self {
        Self {
            matched_permission,
            role_definition_id,
            role_assignment_id,
            role_name: role_name.into(),
            assignment_scope,
            is_inherited,
        }
    }
}

/// Allowed-result payload for `PermissionResult::Allowed`. Carries every
/// contributing grant plus the aggregated scope-type discriminator.
///
/// `#[non_exhaustive]` — construct via [`PermissionGranted::new`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct PermissionGranted {
    /// Distinct contributing grants, ordered as the implementation produced
    /// them (typically deepest-scope-first).
    pub grants: Vec<EffectivePermission>,
    /// Aggregated scope discriminator across all contributing grants.
    pub scope_type: PermissionScopeType,
}

impl PermissionGranted {
    /// Construct a [`PermissionGranted`] payload from an explicit aggregate.
    ///
    /// Normal RBAC producers SHOULD use [`Self::from_grants`] so the aggregate
    /// cannot drift from the assignment scopes. This explicit constructor is
    /// retained for wire/test construction and the trusted in-process system
    /// actor, whose allow intentionally carries no persisted role assignment.
    #[must_use]
    pub fn new(grants: Vec<EffectivePermission>, scope_type: PermissionScopeType) -> Self {
        Self { grants, scope_type }
    }

    /// Construct a normal allowed result by deriving its aggregate scope only
    /// from the contributing role assignments.
    ///
    /// # Errors
    ///
    /// Returns [`ScopeProvenanceError::EmptyGrants`] when no assignment
    /// contributed. The caller MUST deny or return an internal error; it must
    /// never substitute `Global` or the subject's home tenant.
    pub fn from_grants(grants: Vec<EffectivePermission>) -> Result<Self, ScopeProvenanceError> {
        let scope_type = canonical_scope_from_grants(&grants)?;
        Ok(Self { grants, scope_type })
    }

    /// Verify that an externally supplied aggregate represents exactly the
    /// canonical aggregate of its contributing assignment scopes.
    ///
    /// Consumers MUST call this before hierarchy materialization when the
    /// payload came from outside the normal [`Self::from_grants`] producer.
    /// The supplied aggregate is canonicalized before comparison so equivalent
    /// ordering from an older producer remains valid during a rolling upgrade;
    /// different tenant/group roots still fail closed.
    ///
    /// # Errors
    ///
    /// Returns [`ScopeProvenanceError::EmptyGrants`] for an empty normal allow,
    /// or [`ScopeProvenanceError::AggregateMismatch`] when the supplied scope
    /// could broaden or otherwise misrepresent the assignments.
    pub fn validate_scope_provenance(&self) -> Result<(), ScopeProvenanceError> {
        let derived = canonical_scope_from_grants(&self.grants)?;
        let supplied = canonicalize_aggregate(&self.scope_type)
            .ok_or(ScopeProvenanceError::AggregateMismatch)?;
        if derived == supplied {
            Ok(())
        } else {
            Err(ScopeProvenanceError::AggregateMismatch)
        }
    }
}

/// Canonicalize one supplied aggregate without consulting assignment data.
///
/// This normalizes only representation details such as `Combined` leg order,
/// duplicate legs, and group-ID order. Provenance remains strict because the
/// normalized value is subsequently compared with the independently derived
/// aggregate from `grants[].assignment_scope`.
fn canonicalize_aggregate(scope: &PermissionScopeType) -> Option<PermissionScopeType> {
    match scope {
        PermissionScopeType::Combined { scopes } => PermissionScopeType::aggregate(scopes),
        other => PermissionScopeType::aggregate(std::slice::from_ref(other)),
    }
}

/// Derive the canonical aggregate without cloning the effective-permission
/// payloads. Validation runs on every allowed authorization request, so it must
/// inspect only the small typed scope values rather than duplicate role names
/// and permission strings on this hot path.
fn canonical_scope_from_grants(
    grants: &[EffectivePermission],
) -> Result<PermissionScopeType, ScopeProvenanceError> {
    let scopes: Vec<PermissionScopeType> = grants
        .iter()
        .map(|grant| PermissionScopeType::from_assignment_scope(&grant.assignment_scope))
        .collect();
    PermissionScopeType::aggregate(&scopes).ok_or(ScopeProvenanceError::EmptyGrants)
}

/// Denied-result payload for `PermissionResult::Denied`.
///
/// `#[non_exhaustive]` — construct via [`PermissionDenied::new`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct PermissionDenied {
    /// Categorical reason for the denial.
    pub reason: DenyReason,
}

impl PermissionDenied {
    /// Construct a [`PermissionDenied`] payload.
    #[must_use]
    pub fn new(reason: DenyReason) -> Self {
        Self { reason }
    }
}

/// Input for `RbacServiceClientV1::get_subject_roles`.
///
/// `#[non_exhaustive]` — construct via [`GetSubjectRolesRequest::new`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct GetSubjectRolesRequest {
    /// Principal ID.
    pub subject_id: String,
    /// Principal kind.
    pub principal_type: PrincipalType,
    /// Scope context the request is anchored to. Root-scope callers fall
    /// back to their own home tenant inside the evaluator.
    pub context_scope: Scope,
    /// When `true`, fold the subject's group memberships into the query.
    /// Only honoured for `PrincipalType::User`; ignored otherwise.
    pub include_group_roles: bool,
}

impl GetSubjectRolesRequest {
    /// Construct a [`GetSubjectRolesRequest`].
    #[must_use]
    pub fn new(
        subject_id: impl Into<String>,
        principal_type: PrincipalType,
        context_scope: Scope,
        include_group_roles: bool,
    ) -> Self {
        Self {
            subject_id: subject_id.into(),
            principal_type,
            context_scope,
            include_group_roles,
        }
    }
}

/// Output for `RbacServiceClientV1::get_subject_roles`.
///
/// `#[non_exhaustive]` — construct via [`GetSubjectRolesResponse::new`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct GetSubjectRolesResponse {
    /// All role assignments resolved for the subject in the current context,
    /// expanded with the role definition name and permission rules.
    pub roles: Vec<SubjectRole>,
}

impl GetSubjectRolesResponse {
    /// Construct a [`GetSubjectRolesResponse`].
    #[must_use]
    pub fn new(roles: Vec<SubjectRole>) -> Self {
        Self { roles }
    }
}

/// Input for `RbacServiceClientV1::evaluate_permission`.
///
/// `#[non_exhaustive]` — construct via [`EvaluatePermissionRequest::new`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct EvaluatePermissionRequest {
    /// Principal ID being evaluated.
    pub subject_id: String,
    /// Principal kind.
    pub principal_type: PrincipalType,
    /// Short operation verb (e.g. `read`).
    pub operation: String,
    /// Scope context the request is anchored to. `Scope::Root` resolves to
    /// the caller's home tenant inside the evaluator.
    pub context_scope: Scope,
    /// Concrete GTS resource type (e.g. `gts.cf.resources.compute.vm.v1`).
    pub resource_type: String,
}

impl EvaluatePermissionRequest {
    /// Construct an [`EvaluatePermissionRequest`].
    #[must_use]
    pub fn new(
        subject_id: impl Into<String>,
        principal_type: PrincipalType,
        operation: impl Into<String>,
        context_scope: Scope,
        resource_type: impl Into<String>,
    ) -> Self {
        Self {
            subject_id: subject_id.into(),
            principal_type,
            operation: operation.into(),
            context_scope,
            resource_type: resource_type.into(),
        }
    }
}

/// Output for `RbacServiceClientV1::evaluate_permission`.
///
/// `result` is the ONLY carrier of the decision. There is deliberately no
/// stored `allowed` boolean beside it: as a public field it could be reassigned
/// (`#[non_exhaustive]` blocks a struct literal, not a field write), and a
/// derived `Deserialize` would happily reconstruct `allowed: true` next to
/// `PermissionResult::Denied`. Either one hands a caller that trusts the bool a
/// deny it reads as an allow. Ask [`Self::allowed`] instead — it is derived from
/// the discriminant every time, so the contradiction is unrepresentable rather
/// than merely documented.
///
/// `#[non_exhaustive]` — construct via [`EvaluatePermissionResponse::from_result`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct EvaluatePermissionResponse {
    /// Tagged outcome with the granting payload or the deny reason.
    pub result: PermissionResult,
}

impl EvaluatePermissionResponse {
    /// Construct an [`EvaluatePermissionResponse`] from a [`PermissionResult`].
    #[must_use]
    pub fn from_result(result: PermissionResult) -> Self {
        Self { result }
    }

    /// `true` iff the decision is an allow.
    ///
    /// Derived from `result` on every call, so it cannot disagree with it.
    #[must_use]
    pub fn allowed(&self) -> bool {
        matches!(self.result, PermissionResult::Allowed(_))
    }
}

#[cfg(test)]
#[path = "subject_role_tests.rs"]
mod subject_role_tests;
