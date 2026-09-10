//! Authorization primitives for Chat Engine.
//!
//! Exposes the `PolicyEnforcer`, `ResourceType` descriptors, and `AccessRequest`
//! from the authz-resolver SDK, plus the gear-local resource-type constants.
//
// @cpt-cf-chat-engine-component-policy-enforcer

pub mod bypass;
pub mod owner_guard;
pub mod resource_types;

/// Canonical PEP action names, passed as the `action` argument to
/// `access_scope` / `access_scope_with`. String constants (the SDK takes
/// `&str`), so downstream phases may use these consts or bare literals
/// interchangeably.
pub mod actions {
    pub const LIST: &str = "list";
    pub const CREATE: &str = "create";
    // Aligned with the authorization-engine action vocabulary
    // ([list, get, create, update, delete]); the gear previously emitted
    // "read", which is outside that enum and cannot back a read/get ACE.
    pub const READ: &str = "get";
    pub const UPDATE: &str = "update";
    pub const DELETE: &str = "delete";
}

pub use authz_resolver_sdk::pep::{AccessRequest, EnforcerError, PolicyEnforcer};
