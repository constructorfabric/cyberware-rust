//! Canonical GTS `resource_type` identifiers owned by the RBAC module.
//! Centralised here so a typo in any call site can't silently mismatch
//! the authorisation check.
//!
//! **Do NOT** put the URI form (`"gts://…"`) here; that lives in
//! [`crate::module`] as a separate concern.
//!
//! **Do NOT** add to this module without a matching update to
//! `api/rest/error.rs` — the
//! `#[resource_error]` proc-macro attribute needs the literal at the
//! attribute site and cannot consume a `const`.

use toolkit_gts::gts_id;

/// `resource_type` for role-definition operations.
///
/// Matched against `PermissionRule::target_type` by the policy enforcer.
pub const ROLE_DEFINITION: &str = gts_id!("cf.core.rbac.role_definition.v1~");

/// `resource_type` for role-assignment operations.
///
/// Matched against `PermissionRule::target_type` by the policy enforcer.
pub const ROLE_ASSIGNMENT: &str = gts_id!("cf.core.rbac.role_assignment.v1~");
