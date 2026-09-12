// Updated: 2026-04-14 by Constructor Tech
//! PEP constraint compiler.
//!
//! Compiles PDP evaluation responses into `AccessScope` for the secure ORM.
//!
//! ## Compilation Matrix (decision=true assumed)
//!
//! | `require_constraints` | constraints | Result |
//! |-------------------|-------------|--------|
//! | false             | empty       | `allow_all()` |
//! | false             | present     | Compile constraints → `AccessScope` |
//! | true              | empty       | `ConstraintsRequiredButAbsent` |
//! | true              | present     | Compile constraints → `AccessScope` |
//!
//! Unknown/unsupported properties fail that constraint (fail-closed).
//!
//! When `require_constraints=false`, empty constraints are treated as
//! `allow_all()` (legitimate PDP "yes, no row-level filtering"). When
//! `require_constraints=true`, empty constraints are an error (fail-closed).
//! If the PDP returns constraints regardless of the flag, they are compiled.
//!
//! ## Empty value lists (fail-closed)
//!
//! Set-membership predicates (`In`, `InGroup`, `InGroupSubtree`) with an
//! empty value list are rejected at compile time. An empty list means
//! "match nothing" which is semantically a deny — but passing it through
//! to the ORM would generate `WHERE col IN ()`, which is a SQL error on
//! some engines. Instead the compiler treats this as a PDP contract
//! violation and fails the constraint (fail-closed).
//!
//! `InTenantSubtree` carries a single `root_tenant_id` rather than a list,
//! so the empty-list case does not arise; a missing or non-UUID root id
//! is rejected the same way. Its optional `descendant_status` list is
//! converted element-wise (via `TenantStatus::as_smallint`) and bound to
//! the SQL `descendant_status` column; an empty list disables the filter.

use toolkit_security::{AccessScope, ScopeConstraint, ScopeFilter, ScopeValue, pep_properties};

use crate::constraints::{Constraint, Predicate};
use crate::models::{BarrierMode, Capability, EvaluationResponse};

/// Error during constraint compilation.
///
/// Marked `#[non_exhaustive]`: variants have been added before (most recently
/// `UnadvertisedCapabilities`), and downstream crates consume this enum
/// through a pinned toolkit, so exhaustive matches there must not break on a
/// cargo-compatible patch release.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ConstraintCompileError {
    /// Constraints were required but the PDP returned none.
    ///
    /// Per the design Decision Matrix, this is a deny: the PEP asked for
    /// row-level constraints but received an empty set. Fail-closed.
    #[error("constraints required but PDP returned none (fail-closed)")]
    ConstraintsRequiredButAbsent,

    /// All constraints contained unknown predicates (fail-closed).
    #[error("all constraints failed compilation (fail-closed): {reason}")]
    AllConstraintsFailed { reason: String },

    /// Every constraint carried a predicate whose native SQL capability was
    /// never advertised in the evaluation request (fail-closed).
    ///
    /// Per the capability negotiation contract a PDP must not emit
    /// `in_group`/`in_group_subtree`/`in_tenant_subtree` predicates unless the
    /// corresponding capability was advertised; a PDP that cannot expand a
    /// scope to explicit `in` predicates must deny instead. This typed variant
    /// lets enforcing services map that specific contract violation to a
    /// domain-level denial rather than a generic internal error, while other
    /// compile failures keep signalling infrastructure faults.
    #[error(
        "{predicate} predicate requires unadvertised capabilities: {} (fail-closed)",
        missing.join(", ")
    )]
    UnadvertisedCapabilities {
        /// Name of the offending predicate (for example `InTenantSubtree`).
        predicate: &'static str,
        /// Snake-case capability names missing from the negotiated set.
        missing: Vec<&'static str>,
    },
}

/// Per-constraint compilation failure.
///
/// Distinguishes capability-negotiation violations from structural failures
/// (unknown property, malformed value, missing membership type) so the
/// aggregated [`ConstraintCompileError`] can stay typed when every constraint
/// fails for the same negotiation reason.
enum ConstraintFailure {
    /// The predicate requires capabilities absent from the negotiated set.
    UnadvertisedCapabilities {
        predicate: &'static str,
        missing: Vec<&'static str>,
    },
    /// Any other fail-closed reason.
    Other(String),
}

impl std::fmt::Display for ConstraintFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // Render through the public variant so the warn-log text and the
            // surfaced error text come from one format string and cannot
            // drift.
            Self::UnadvertisedCapabilities { predicate, missing } => {
                ConstraintCompileError::UnadvertisedCapabilities {
                    predicate,
                    missing: missing.clone(),
                }
                .fmt(f)
            }
            Self::Other(reason) => f.write_str(reason),
        }
    }
}

// Lets the string-producing conversion helpers keep using `?` inside
// `compile_constraint` without wrapping every call site.
impl From<String> for ConstraintFailure {
    fn from(reason: String) -> Self {
        Self::Other(reason)
    }
}

/// Compile constraints from an evaluation response into an `AccessScope`.
///
/// **Precondition:** the caller has already verified `response.decision == true`.
/// This function only handles constraint compilation:
/// - `require_constraints=false, constraints=[]` → `Ok(allow_all())`
/// - `require_constraints=false, constraints=[..]` → compile predicates
/// - `require_constraints=true, constraints=[]` → `Err(ConstraintsRequiredButAbsent)`
/// - `require_constraints=true, constraints=[..]` → compile predicates
///
/// Each PDP constraint compiles to a `ScopeConstraint` (AND of filters).
/// Multiple constraints become `AccessScope::from_constraints` (OR-ed).
///
/// The compiler validates predicates against the provided
/// `supported_properties` list and converts them structurally. Unknown
/// properties fail that constraint (fail-closed). Native group predicates are
/// additionally restricted to `id` and must share an AND constraint with an
/// `owner_tenant_id` predicate. Native group
/// predicates also fail closed through this entry point because their RG
/// member-handle type is absent; use
/// [`compile_to_access_scope_with_group_membership_type`] for those predicates.
/// If ALL constraints fail compilation, returns `AllConstraintsFailed`.
///
/// # Errors
///
/// - `ConstraintsRequiredButAbsent` if constraints were required but empty
/// - `AllConstraintsFailed` if all constraints have unsupported predicates
pub fn compile_to_access_scope(
    response: &EvaluationResponse,
    require_constraints: bool,
    supported_properties: &[&str],
) -> Result<AccessScope, ConstraintCompileError> {
    compile_to_access_scope_with_group_membership_type(
        response,
        require_constraints,
        supported_properties,
        None,
    )
}

/// Compile constraints with the RG member-handle type required by native group
/// predicates.
///
/// Kept separate from [`compile_to_access_scope`] so existing low-level callers
/// remain source-compatible. A native `InGroup`/`InGroupSubtree` predicate sent
/// through the untyped entry point fails closed instead of querying membership
/// rows across unrelated resource types.
///
/// # Errors
///
/// Returns the same errors as [`compile_to_access_scope`]. Native group
/// predicates additionally fail compilation when `group_membership_type` is
/// absent or empty, when UUID hierarchy keys are malformed, when the predicate
/// targets a property other than `id`, or when the same constraint lacks an
/// `owner_tenant_id` predicate.
pub fn compile_to_access_scope_with_group_membership_type(
    response: &EvaluationResponse,
    require_constraints: bool,
    supported_properties: &[&str],
    group_membership_type: Option<&str>,
) -> Result<AccessScope, ConstraintCompileError> {
    compile_to_access_scope_impl(
        response,
        require_constraints,
        supported_properties,
        group_membership_type,
        None,
    )
}

/// Compile constraints while enforcing the exact capabilities advertised in
/// the evaluation request.
///
/// The high-level enforcer uses this path to reject unsolicited native group
/// predicates from a PDP. Low-level callers that explicitly provide a group
/// membership type remain responsible for negotiating their own capabilities.
pub(crate) fn compile_to_access_scope_with_negotiated_capabilities(
    response: &EvaluationResponse,
    require_constraints: bool,
    supported_properties: &[&str],
    group_membership_type: Option<&str>,
    negotiated_capabilities: &[Capability],
) -> Result<AccessScope, ConstraintCompileError> {
    compile_to_access_scope_impl(
        response,
        require_constraints,
        supported_properties,
        group_membership_type,
        Some(negotiated_capabilities),
    )
}

fn compile_to_access_scope_impl(
    response: &EvaluationResponse,
    require_constraints: bool,
    supported_properties: &[&str],
    group_membership_type: Option<&str>,
    negotiated_capabilities: Option<&[Capability]>,
) -> Result<AccessScope, ConstraintCompileError> {
    // Step 1: Handle empty constraints based on require_constraints flag.
    if response.context.constraints.is_empty() {
        if require_constraints {
            return Err(ConstraintCompileError::ConstraintsRequiredButAbsent);
        }
        return Ok(AccessScope::allow_all());
    }

    // Step 2: Compile each constraint
    let mut constraints = Vec::new();
    let mut failures: Vec<ConstraintFailure> = Vec::new();

    for constraint in &response.context.constraints {
        match compile_constraint(
            constraint,
            supported_properties,
            group_membership_type,
            negotiated_capabilities,
        ) {
            Ok(sc) => constraints.push(sc),
            Err(failure) => {
                tracing::warn!(
                    reason = %failure,
                    "constraint compilation failed (fail-closed), possible PDP contract violation",
                );
                failures.push(failure);
            }
        }
    }

    // If no constraint compiled successfully, fail-closed. When every failure
    // is a capability-negotiation violation, surface the typed variant so
    // callers can map it to a domain-level denial; any structural failure in
    // the mix keeps the aggregate as a generic compile error. Several
    // constraints can fail with different unadvertised predicates — surfacing
    // only the first is intentional, the per-constraint warn above records
    // the rest.
    if constraints.is_empty() {
        let all_unadvertised = failures
            .iter()
            .all(|f| matches!(f, ConstraintFailure::UnadvertisedCapabilities { .. }));
        let reason = failures
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("; ");
        return Err(match failures.into_iter().next() {
            Some(ConstraintFailure::UnadvertisedCapabilities { predicate, missing })
                if all_unadvertised =>
            {
                ConstraintCompileError::UnadvertisedCapabilities { predicate, missing }
            }
            _ => ConstraintCompileError::AllConstraintsFailed { reason },
        });
    }

    // Every successfully compiled constraint carries at least one filter:
    // `compile_constraint` rejects empty-predicates constraints as malformed
    // (DESIGN.md, PEP requirement #7), and each surviving predicate lowers to
    // exactly one filter. Folding empty constraints into `allow_all()` here
    // would turn a degenerate PDP permit into unrestricted access.
    Ok(AccessScope::from_constraints(constraints))
}

/// Compile a single PDP constraint into a `ScopeConstraint`.
///
/// Each predicate becomes a `ScopeFilter`. If any predicate's property
/// is not in `supported_properties`, the entire constraint fails (fail-closed).
fn compile_constraint(
    constraint: &Constraint,
    supported_properties: &[&str],
    group_membership_type: Option<&str>,
    negotiated_capabilities: Option<&[Capability]>,
) -> Result<ScopeConstraint, ConstraintFailure> {
    // DESIGN.md, PEP requirement #7: a constraint MUST carry at least one
    // predicate. An empty `predicates: []` is a malformed PDP constraint and
    // fails closed — letting it compile would produce a filterless
    // `ScopeConstraint`, which downstream semantics treat as "no restriction"
    // (a degenerate permit escalating to unrestricted access).
    if constraint.predicates.is_empty() {
        return Err(ConstraintFailure::Other(
            "constraint has empty predicates: at least one predicate is required (fail-closed)"
                .to_owned(),
        ));
    }

    // Capability negotiation is checked for every predicate BEFORE any shape
    // validation, so a predicate that is both unadvertised and malformed
    // still classifies as the typed negotiation violation — the highest-
    // priority failure class, which enforcing services map to a denial.
    for predicate in &constraint.predicates {
        match predicate {
            Predicate::InGroup(_) => require_negotiated_capabilities(
                negotiated_capabilities,
                &[Capability::GroupMembership],
                "InGroup",
            )?,
            Predicate::InGroupSubtree(_) => require_negotiated_capabilities(
                negotiated_capabilities,
                &[Capability::GroupMembership, Capability::GroupHierarchy],
                "InGroupSubtree",
            )?,
            Predicate::InTenantSubtree(_) => require_negotiated_capabilities(
                negotiated_capabilities,
                &[Capability::TenantHierarchy],
                "InTenantSubtree",
            )?,
            Predicate::Eq(_) | Predicate::In(_) => {}
        }
    }

    let has_group_predicate = constraint.predicates.iter().any(|predicate| {
        matches!(
            predicate,
            Predicate::InGroup(_) | Predicate::InGroupSubtree(_)
        )
    });
    if has_group_predicate && !has_tenant_scope_predicate(constraint) {
        return Err(ConstraintFailure::Other(
            "native group predicates require an owner_tenant_id predicate in the same constraint (fail-closed)"
                .to_owned(),
        ));
    }

    let mut filters = Vec::new();

    for predicate in &constraint.predicates {
        let (property, filter) = match predicate {
            Predicate::Eq(eq) => {
                let value = if eq.property == pep_properties::OWNER_TENANT_ID {
                    json_to_uuid_scope_value(&eq.value, pep_properties::OWNER_TENANT_ID)?
                } else {
                    json_to_scope_value(&eq.value)?
                };
                (eq.property.as_str(), ScopeFilter::eq(&eq.property, value))
            }
            Predicate::In(p) => {
                let values: Vec<ScopeValue> = if p.property == pep_properties::OWNER_TENANT_ID {
                    json_values_to_uuid_scope_values(&p.values, pep_properties::OWNER_TENANT_ID)?
                } else {
                    p.values
                        .iter()
                        .map(json_to_scope_value)
                        .collect::<Result<_, _>>()?
                };
                if values.is_empty() {
                    return Err(format!(
                        "In predicate on '{}' has empty value list (fail-closed)",
                        p.property
                    )
                    .into());
                }
                (p.property.as_str(), ScopeFilter::r#in(&p.property, values))
            }
            Predicate::InGroup(p) => {
                require_resource_id_group_property("InGroup", &p.property)?;
                let group_ids = json_values_to_uuid_scope_values(&p.group_ids, "group_ids")?;
                if group_ids.is_empty() {
                    return Err(format!(
                        "InGroup predicate on '{}' has empty group_ids (fail-closed)",
                        p.property
                    )
                    .into());
                }
                let membership_type =
                    required_group_membership_type(group_membership_type, "InGroup", &p.property)?;
                (
                    p.property.as_str(),
                    ScopeFilter::in_group_typed(&p.property, membership_type, group_ids),
                )
            }
            Predicate::InGroupSubtree(p) => {
                require_resource_id_group_property("InGroupSubtree", &p.property)?;
                let ancestor_ids =
                    json_values_to_uuid_scope_values(&p.ancestor_ids, "ancestor_ids")?;
                if ancestor_ids.is_empty() {
                    return Err(format!(
                        "InGroupSubtree predicate on '{}' has empty ancestor_ids (fail-closed)",
                        p.property
                    )
                    .into());
                }
                let membership_type = required_group_membership_type(
                    group_membership_type,
                    "InGroupSubtree",
                    &p.property,
                )?;
                (
                    p.property.as_str(),
                    ScopeFilter::in_group_subtree_typed(&p.property, membership_type, ancestor_ids),
                )
            }
            Predicate::InTenantSubtree(p) => {
                let root_tenant_id = json_to_uuid_scope_value(&p.root_tenant_id, "root_tenant_id")
                    .map_err(|e| {
                        format!(
                            "InTenantSubtree predicate on '{}' has invalid root_tenant_id: {e}",
                            p.property
                        )
                    })?;
                // Map authz-sdk barrier mode onto toolkit-security's bool flag.
                // `Respect` (default) clamps the closure subquery with
                // `AND barrier = 0`; `Ignore` is reserved for cross-barrier
                // operations such as billing or tenant metadata reads.
                let respect_barriers = matches!(p.barrier_mode, BarrierMode::Respect);
                // Each `TenantStatus` lowers to its canonical SMALLINT
                // encoding (Active=1, Suspended=2, Deleted=3) so the SQL
                // bind matches the `tenant_closure.descendant_status`
                // column domain. The `Provisioning` AM-internal status is
                // not part of `TenantStatus` and therefore cannot be
                // expressed here — that matches the closure invariant.
                let descendant_status: Vec<ScopeValue> = p
                    .descendant_status
                    .iter()
                    .map(|s| ScopeValue::Int(i64::from(s.as_smallint())))
                    .collect();
                (
                    p.property.as_str(),
                    ScopeFilter::in_tenant_subtree(
                        &p.property,
                        root_tenant_id,
                        respect_barriers,
                        descendant_status,
                    ),
                )
            }
        };

        if !supported_properties.contains(&property) {
            return Err(format!("unsupported property: {property}").into());
        }

        filters.push(filter);
    }

    Ok(ScopeConstraint::new(filters))
}

/// Whether a group-bearing constraint also carries the mandatory tenant scope
/// in the same AND envelope.
fn has_tenant_scope_predicate(constraint: &Constraint) -> bool {
    constraint
        .predicates
        .iter()
        .any(|predicate| match predicate {
            Predicate::Eq(predicate) => predicate.property == pep_properties::OWNER_TENANT_ID,
            Predicate::In(predicate) => predicate.property == pep_properties::OWNER_TENANT_ID,
            Predicate::InTenantSubtree(predicate) => {
                predicate.property == pep_properties::OWNER_TENANT_ID
            }
            Predicate::InGroup(_) | Predicate::InGroupSubtree(_) => false,
        })
}

/// Native group membership currently describes the resource itself. Applying
/// one resource type mapping to another property (for example `owner_id`) can
/// select unrelated membership rows when identifiers collide.
fn require_resource_id_group_property(predicate: &str, property: &str) -> Result<(), String> {
    if property == pep_properties::RESOURCE_ID {
        Ok(())
    } else {
        Err(format!(
            "{predicate} predicate must target '{}' until per-property membership types are supported, got '{property}' (fail-closed)",
            pep_properties::RESOURCE_ID,
        ))
    }
}

/// Require every native SQL capability needed by a predicate when the
/// high-level enforcer supplies the negotiated request capabilities.
fn require_negotiated_capabilities(
    negotiated_capabilities: Option<&[Capability]>,
    required: &[Capability],
    predicate: &'static str,
) -> Result<(), ConstraintFailure> {
    let Some(negotiated_capabilities) = negotiated_capabilities else {
        return Ok(());
    };
    let missing: Vec<&'static str> = required
        .iter()
        .filter(|capability| !negotiated_capabilities.contains(capability))
        .map(|capability| match capability {
            Capability::TenantHierarchy => "tenant_hierarchy",
            Capability::GroupMembership => "group_membership",
            Capability::GroupHierarchy => "group_hierarchy",
        })
        .collect();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(ConstraintFailure::UnadvertisedCapabilities { predicate, missing })
    }
}

/// Return the configured RG member-handle type or a fail-closed compilation
/// reason suitable for the enclosing constraint.
fn required_group_membership_type<'a>(
    group_membership_type: Option<&'a str>,
    predicate: &str,
    property: &str,
) -> Result<&'a str, String> {
    group_membership_type.filter(|value| !value.is_empty()).ok_or_else(|| {
        format!(
            "{predicate} predicate on '{property}' requires a configured RG membership resource type (fail-closed)"
        )
    })
}

/// Convert a UUID-valued JSON list to scope values without permitting scalar
/// types that would fail against `PostgreSQL`'s UUID hierarchy columns.
fn json_values_to_uuid_scope_values(
    values: &[serde_json::Value],
    field: &str,
) -> Result<Vec<ScopeValue>, String> {
    values
        .iter()
        .map(|value| json_to_uuid_scope_value(value, field))
        .collect()
}

/// Convert a `serde_json::Value` to a UUID `ScopeValue`.
///
/// Only valid UUID strings are accepted; anything else (non-UUID string,
/// number, bool, null, array, object) is rejected for UUID-backed hierarchy
/// columns.
fn json_to_uuid_scope_value(v: &serde_json::Value, field: &str) -> Result<ScopeValue, String> {
    match v {
        serde_json::Value::String(s) => uuid::Uuid::parse_str(s)
            .map(ScopeValue::Uuid)
            .map_err(|_| format!("{field} must contain UUID strings, got: {s:?} (fail-closed)")),
        serde_json::Value::Number(_) => Err(format!(
            "{field} must contain UUID strings, got number (fail-closed)"
        )),
        serde_json::Value::Bool(_) => Err(format!(
            "{field} must contain UUID strings, got bool (fail-closed)"
        )),
        other => Err(format!(
            "{field} must contain UUID strings, got: {other} (fail-closed)"
        )),
    }
}

/// Convert a `serde_json::Value` to a `ScopeValue`.
///
/// UUID strings are detected and stored as `ScopeValue::Uuid`;
/// other strings become `ScopeValue::String`.
fn json_to_scope_value(v: &serde_json::Value) -> Result<ScopeValue, String> {
    match v {
        serde_json::Value::String(s) => {
            if let Ok(uuid) = uuid::Uuid::parse_str(s) {
                Ok(ScopeValue::Uuid(uuid))
            } else {
                Ok(ScopeValue::String(s.clone()))
            }
        }
        serde_json::Value::Number(n) => n.as_i64().map(ScopeValue::Int).ok_or_else(|| {
            format!("only integer JSON numbers are supported for scope filters, got: {n}")
        }),
        serde_json::Value::Bool(b) => Ok(ScopeValue::Bool(*b)),
        other => Err(format!(
            "unsupported JSON value type for scope filter: {other}"
        )),
    }
}

#[cfg(test)]
#[path = "compiler_tests.rs"]
mod compiler_tests;
