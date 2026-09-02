//! Bootstrap readiness, shared between the lifecycle entry and the health
//! check.

use std::sync::RwLock;

use toolkit_macros::domain_model;

use super::error::Dependency;

/// Where bootstrap stands.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadinessState {
    /// Bootstrap has not completed yet.
    Starting,
    /// Bootstrap failed on a dependency. The gear serves nothing.
    Failed {
        /// The failing dependency.
        dependency: Dependency,
        /// Operator-facing detail.
        reason: String,
    },
    /// Bootstrap completed.
    Ready,
}

/// Thread-safe readiness cell.
#[domain_model]
#[derive(Debug)]
pub struct Readiness {
    state: RwLock<ReadinessState>,
}

impl Default for Readiness {
    fn default() -> Self {
        Self::new()
    }
}

impl Readiness {
    /// A cell in the `Starting` state.
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: RwLock::new(ReadinessState::Starting),
        }
    }

    /// Current state. A poisoned lock still yields the last written state.
    #[must_use]
    pub fn snapshot(&self) -> ReadinessState {
        self.state
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// True in the `Ready` state.
    #[must_use]
    pub fn is_ready(&self) -> bool {
        matches!(self.snapshot(), ReadinessState::Ready)
    }

    /// Record a bootstrap failure.
    pub fn mark_failed(&self, dependency: Dependency, reason: impl Into<String>) {
        *self
            .state
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = ReadinessState::Failed {
            dependency,
            reason: reason.into(),
        };
    }

    /// Record bootstrap completion.
    pub fn mark_ready(&self) {
        *self
            .state
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = ReadinessState::Ready;
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "readiness_tests.rs"]
mod readiness_tests;
