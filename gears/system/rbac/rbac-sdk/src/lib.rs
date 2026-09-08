//! RBAC Service SDK
//!
//! Infrastructure-free contract crate for the `rbac` module. Consumers depend
//! on this SDK through `ClientHub` to read RBAC state without pulling in the
//! implementation crate's HTTP / ORM / migration baggage.
//!
//! Public surface:
//!
//! * [`RbacServiceClientV1`] — the two-method `ClientHub` trait.
//! * [`RbacServiceError`] — categorised error enum returned by every method.
//! * Per-resource model modules — [`scope`], [`permission_rule`],
//!   [`role_definition`], [`role_assignment`], [`subject_role`].
//! * [`models`] — flat re-export hub: every per-resource module's public
//!   items are also reachable via `rbac_sdk::models::*`.

pub mod api;
pub mod error;
pub mod permission_rule;
pub mod role_assignment;
pub mod role_definition;
pub mod scope;
pub mod subject_role;

/// Flat hub: every public item from the per-resource modules is re-exported
/// here, so `rbac_sdk::models::{Scope, RoleDefinition, ...}` resolves without
/// knowing the per-module path.
///
/// The error surface (`RbacServiceError`, `FieldError`, `FieldViolationField`,
/// `FieldViolationReason`, `MAX_FIELD_ERRORS`, `TRUNCATION_SENTINEL_CODE`) is
/// re-exported here too, even though it lives in [`crate::error`] rather than
/// in a per-resource module.
pub mod models {
    pub use crate::error::{
        FieldError, FieldViolationField, FieldViolationReason, MAX_FIELD_ERRORS, RbacServiceError,
        TRUNCATION_SENTINEL_CODE,
    };
    pub use crate::permission_rule::{Action, PermissionRule, UnknownAction};
    pub use crate::role_assignment::{PrincipalType, RoleAssignment, UnknownPrincipalType};
    pub use crate::role_definition::RoleDefinition;
    pub use crate::scope::{Scope, ScopeParseError};
    pub use crate::subject_role::{
        DenyReason, EffectivePermission, EvaluatePermissionRequest, EvaluatePermissionResponse,
        GetSubjectRolesRequest, GetSubjectRolesResponse, PermissionDenied, PermissionGranted,
        PermissionResult, PermissionScopeType, RolePolicy, ScopeProvenanceError, SubjectRole,
    };
}

pub use api::RbacServiceClientV1;
pub use error::RbacServiceError;
