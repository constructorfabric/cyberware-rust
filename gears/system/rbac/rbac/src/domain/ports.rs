//! Outbound port traits — interfaces the domain calls. Adapters live under
//! `infra/`.
//!
//! The three `*_mock` modules are gated behind
//! `cfg(any(test, feature = "test-support"))` so they cannot leak into a
//! release artifact. A typo in module wiring that swapped the real
//! `PermissionEvaluator` for `MockPolicyEnforcer::allow_all()` would
//! silently disable RBAC; the gate makes such a typo a compile error
//! outside of test builds.

pub mod metrics;
pub mod permission_catalog;
#[cfg(any(test, feature = "test-support"))]
pub mod permission_catalog_mock;
pub mod policy_enforcer;
#[cfg(any(test, feature = "test-support"))]
pub mod policy_enforcer_mock;
pub mod principal_name_reader;
#[cfg(any(test, feature = "test-support"))]
pub mod principal_name_reader_mock;
pub(crate) mod principal_type_resolver;
pub mod rg_port;
pub mod target_type_validator;
#[cfg(any(test, feature = "test-support"))]
pub mod target_type_validator_mock;
