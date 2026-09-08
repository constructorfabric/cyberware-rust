#![allow(unknown_lints, de0309_must_have_domain_model)]

//! Accept-all and deny-all test doubles for [`TargetTypeValidator`].
//! Both names remain reachable via
//! `crate::domain::target_type_validator::*` through a re-export.

use async_trait::async_trait;

use super::target_type_validator::{TargetTypeValidationError, TargetTypeValidator};

/// Refuse to construct any of the test doubles in this module when
/// compiled with `debug_assertions = false` (release profile). The
/// module is gated `#[cfg(any(test, feature = "test-support"))]`, but
/// cargo feature unification means a workspace member depending on
/// `rbac/test-support` would re-export these constructors into a
/// release binary. The runtime guard makes any such misuse fail loud on
/// first call instead of silently accepting every target type.
///
/// The guard is only reachable because every double below carries a
/// private field: a `pub` unit struct is itself a constructor
/// expression, so `Arc::new(AcceptAllTargetTypeValidator)` would
/// otherwise bypass both this check and any `Default` impl.
// Intentional fail-loud guard (see fn doc). `manual_assert` is suppressed
// too: converting to `assert!(cfg!(debug_assertions), …)` would trip
// `assertions_on_constants` since the condition is a compile-time const.
#[allow(clippy::panic, clippy::manual_assert)]
fn release_build_guard() {
    if !cfg!(debug_assertions) {
        panic!(
            "rbac::domain::ports::target_type_validator_mock test double constructed in a \
             release-profile build \u{2014} this type is a test fixture and must never reach \
             production. Use the real `TypesRegistryTargetTypeValidator` (see \
             infra::types_registry_target_type_validator) instead."
        );
    }
}

/// Accept-all validator for tests that don't care about registry
/// lookups.
#[derive(Debug)]
pub struct AcceptAllTargetTypeValidator {
    /// Forces construction through [`Self::new`] so
    /// [`release_build_guard`] cannot be bypassed by a unit-struct
    /// literal.
    _guarded: (),
}

impl AcceptAllTargetTypeValidator {
    /// Construct the accept-all double. Panics in a release-profile
    /// build — see [`release_build_guard`].
    #[must_use]
    pub fn new() -> Self {
        release_build_guard();
        Self { _guarded: () }
    }
}

impl Default for AcceptAllTargetTypeValidator {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl TargetTypeValidator for AcceptAllTargetTypeValidator {
    async fn ensure_exists(&self, _target_type: &str) -> Result<(), TargetTypeValidationError> {
        Ok(())
    }
}

/// Deny-all validator — exercises the `InvalidPermissionRule` mapping.
#[derive(Debug)]
pub struct DenyAllTargetTypeValidator {
    /// See [`AcceptAllTargetTypeValidator::_guarded`].
    _guarded: (),
}

impl DenyAllTargetTypeValidator {
    /// Construct the deny-all double. Panics in a release-profile build
    /// — see [`release_build_guard`].
    #[must_use]
    pub fn new() -> Self {
        release_build_guard();
        Self { _guarded: () }
    }
}

impl Default for DenyAllTargetTypeValidator {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl TargetTypeValidator for DenyAllTargetTypeValidator {
    async fn ensure_exists(&self, target_type: &str) -> Result<(), TargetTypeValidationError> {
        Err(TargetTypeValidationError::NotRegistered(
            target_type.to_owned(),
        ))
    }
}

/// Validator that fails with [`TargetTypeValidationError::Internal`] — the
/// types-registry-outage path.
///
/// `Internal` must surface as a 500, not the 400 that
/// [`DenyAllTargetTypeValidator`]'s `NotRegistered` produces: an
/// unreachable registry is not evidence that the target type is invalid.
/// Neither existing double could produce `Internal`, so that arm of
/// `From<TargetTypeValidationError>` was unreachable from any test and a
/// regression collapsing an outage into a 400 would have gone unnoticed.
#[derive(Debug)]
pub struct FailingTargetTypeValidator {
    /// Detail carried on the `Internal` error.
    detail: String,
}

impl FailingTargetTypeValidator {
    /// Construct the failing double. Panics in a release-profile build —
    /// see [`release_build_guard`].
    #[must_use]
    pub fn new(detail: impl Into<String>) -> Self {
        release_build_guard();
        Self {
            detail: detail.into(),
        }
    }
}

impl Default for FailingTargetTypeValidator {
    fn default() -> Self {
        Self::new("types registry unavailable")
    }
}

#[async_trait]
impl TargetTypeValidator for FailingTargetTypeValidator {
    async fn ensure_exists(&self, _target_type: &str) -> Result<(), TargetTypeValidationError> {
        Err(TargetTypeValidationError::Internal(self.detail.clone()))
    }
}
