//! Target-type validator port.
//!
//! `permissions[i].target_type` and the matching `not_permissions`
//! field MUST be validated against the types-registry at create /
//! update time; unknown target types are rejected with 400.
//! Production: `infra::types_registry_target_type_validator`. Test
//! doubles: `AcceptAllTargetTypeValidator` / `DenyAllTargetTypeValidator`
//! (see `target_type_validator_mock`).

use async_trait::async_trait;
use thiserror::Error;
use toolkit_macros::domain_model;

/// Failure surface for [`TargetTypeValidator::ensure_exists`].
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum TargetTypeValidationError {
    /// No schema registered under `target_type`. Maps to 400 (REST:
    /// `InvalidPermissionRule`).
    #[error("target_type '{0}' is not registered in the types-registry")]
    NotRegistered(String),
    /// Upstream failure — maps to 500, NOT 400, since the validation is
    /// not authoritative.
    #[error("types-registry lookup failed: {0}")]
    Internal(String),
}

impl From<TargetTypeValidationError> for crate::domain::error::DomainError {
    fn from(err: TargetTypeValidationError) -> Self {
        match err {
            TargetTypeValidationError::NotRegistered(tt) => Self::InvalidPermissionRule {
                detail: format!("target_type '{tt}' is not registered in the types-registry"),
            },
            TargetTypeValidationError::Internal(msg) => {
                Self::internal(format!("types-registry lookup failed: {msg}"))
            }
        }
    }
}

/// Domain-layer port for validating `target_type` against the
/// types-registry.
#[async_trait]
pub trait TargetTypeValidator: Send + Sync {
    /// Confirm `target_type` resolves to a registered GTS type-schema.
    /// Implementations MUST distinguish [`TargetTypeValidationError::NotRegistered`]
    /// from [`TargetTypeValidationError::Internal`].
    async fn ensure_exists(&self, target_type: &str) -> Result<(), TargetTypeValidationError>;

    /// Confirm every `target_type` in `target_types` resolves to a
    /// registered schema. The default dedups and calls [`Self::ensure_exists`]
    /// per distinct type; the production implementation overrides this with
    /// a single batched registry round-trip: `create`/`update` call this
    /// once instead of `ensure_exists` per rule, which would be an N+1
    /// against the types-registry. Returns the first
    /// `NotRegistered`/`Internal` error in input order.
    async fn ensure_all_exist(
        &self,
        target_types: &[&str],
    ) -> Result<(), TargetTypeValidationError> {
        let mut seen = std::collections::HashSet::new();
        for &target_type in target_types {
            if seen.insert(target_type) {
                self.ensure_exists(target_type).await?;
            }
        }
        Ok(())
    }
}

// Test doubles live in [`target_type_validator_mock`]; re-exported so
// existing import paths stay stable. Gated behind the `test-support`
// feature so the test doubles never appear in a release artifact.
#[cfg(any(test, feature = "test-support"))]
pub use crate::domain::target_type_validator_mock::{
    AcceptAllTargetTypeValidator, DenyAllTargetTypeValidator, FailingTargetTypeValidator,
};
