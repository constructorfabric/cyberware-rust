//! The gear's domain service: admission plus the dependencies bootstrap binds.

use std::sync::{Arc, OnceLock};

use quota_enforcement_sdk::QuotaEnforcementStoragePluginV1;
use toolkit_macros::domain_model;

use super::admission::Admission;
use super::bootstrap::Bound;
use super::error::{Dependency, DomainError};
use super::ports::coordination::SingletonCoordinator;
use super::readiness::Readiness;

/// Composition root of the domain. Handlers and the in-process client reach
/// every dependency through it.
#[domain_model]
pub struct Service {
    admission: Admission,
    readiness: Arc<Readiness>,
    bound: OnceLock<Bound>,
}

impl Service {
    /// Assemble the service. Dependencies are bound later by bootstrap.
    #[must_use]
    pub fn new(admission: Admission, readiness: Arc<Readiness>) -> Self {
        Self {
            admission,
            readiness,
            bound: OnceLock::new(),
        }
    }

    /// The PEP boundary.
    #[must_use]
    pub fn admission(&self) -> &Admission {
        &self.admission
    }

    /// Readiness cell shared with the health check.
    #[must_use]
    pub fn readiness(&self) -> &Arc<Readiness> {
        &self.readiness
    }

    /// Publish the bootstrapped dependencies. Set once.
    ///
    /// # Errors
    ///
    /// Returns [`DomainError::Internal`] when dependencies were bound before.
    pub fn bind(&self, bound: Bound) -> Result<(), DomainError> {
        self.bound
            .set(bound)
            .map_err(|_| DomainError::Internal("dependencies were already bound".to_owned()))
    }

    /// The active storage plugin.
    ///
    /// # Errors
    ///
    /// Returns [`DomainError::NotReady`] before bootstrap completed.
    pub fn storage(&self) -> Result<Arc<dyn QuotaEnforcementStoragePluginV1>, DomainError> {
        self.bound
            .get()
            .map(|b| b.storage.clone())
            .ok_or(DomainError::NotReady {
                dependency: Dependency::Storage,
            })
    }

    /// The sweeper coordinator over the platform `cluster` gear.
    ///
    /// # Errors
    ///
    /// Returns [`DomainError::NotReady`] before bootstrap completed.
    pub fn coordinator(&self) -> Result<Arc<dyn SingletonCoordinator>, DomainError> {
        self.bound
            .get()
            .map(|b| b.coordinator.clone())
            .ok_or(DomainError::NotReady {
                dependency: Dependency::Cluster,
            })
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "service_tests.rs"]
mod service_tests;
