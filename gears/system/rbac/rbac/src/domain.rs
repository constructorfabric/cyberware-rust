//! Domain layer: pure business logic with no I/O, no SeaORM, no upstream
//! ClientHub coupling.

pub mod error;
pub mod model;
pub mod ports;
pub mod repos;
pub mod role_assignment;
pub mod role_definition;
pub mod service;

pub use error::DomainError;

// The `model / ports / repos / service` split is an internal organization
// detail: every submodule is re-exported flat at `domain::*`, which is the
// public surface of `rbac::domain`.

// `pub` rather than `pub(crate)` so the evaluator tests under
// `tests/postgres_*.rs` can construct a real `PermissionEvaluator` against a
// SeaORM repo. `#[doc(hidden)]` keeps it out of the rendered rustdoc.
#[cfg(test)]
pub(crate) use model::scope_fakes;
pub(crate) use model::{actions, resource_types};

pub(crate) use ports::principal_type_resolver;
pub use ports::{
    metrics, permission_catalog, policy_enforcer, principal_name_reader, rg_port,
    target_type_validator,
};

// `*_mock` modules carry test doubles only; gated behind the
// `test-support` feature so the symbols cannot leak into a release
// artifact. See `domain/ports.rs` for the matching module-decl gate.
#[cfg(any(test, feature = "test-support"))]
pub use ports::{
    permission_catalog_mock, policy_enforcer_mock, principal_name_reader_mock,
    target_type_validator_mock,
};

pub use repos::{role_assignment_repo, role_definition_repo};

// Repo-level test double, gated exactly like the `ports::*_mock` modules.
#[cfg(any(test, feature = "test-support"))]
pub use repos::role_assignment_repo_mock;

pub(crate) use service::{builtin_roles_catalog, name_confusables, permission_matcher};
pub use service::{caller_scope, etag, permission_evaluator, scope_validator};
