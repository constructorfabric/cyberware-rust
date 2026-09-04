//! Fail-closed gear bootstrap (`features/foundation.md`, "Gear Bootstrap and
//! Readiness").
//!
//! Runs in the lifecycle entry before the ready signal. Every step that fails
//! records the failing dependency in [`Readiness`], so the health endpoint
//! names it, and returns an error so the runtime never marks the gear ready.
//! Later features extend [`Bootstrap::run`] with their own steps.

use std::sync::Arc;

use authz_resolver_sdk::AuthZResolverApi;
use quota_enforcement_sdk::{BootstrapBundle, QuotaEnforcementStoragePluginV1, StorageError};
use toolkit::client_hub::ClientHub;
use toolkit_macros::domain_model;

use super::error::{Dependency, DomainError};
use super::plugins::PluginBinding;
use super::ports::coordination::{CoordinatorBinding, SingletonCoordinator};
use super::readiness::Readiness;

const LOG_TARGET: &str = "qe.bootstrap";

/// Dependencies bound by a successful bootstrap.
#[domain_model]
#[derive(Clone)]
pub struct Bound {
    /// The active storage plugin.
    pub storage: Arc<dyn QuotaEnforcementStoragePluginV1>,
    /// The sweeper coordinator over the platform `cluster` gear.
    pub coordinator: Arc<dyn SingletonCoordinator>,
}

/// The bootstrap procedure.
#[domain_model]
pub struct Bootstrap {
    binding: PluginBinding,
    coordinator: Arc<dyn CoordinatorBinding>,
    hub: Arc<ClientHub>,
    readiness: Arc<Readiness>,
}

impl Bootstrap {
    /// Assemble the procedure.
    #[must_use]
    pub fn new(
        binding: PluginBinding,
        coordinator: Arc<dyn CoordinatorBinding>,
        hub: Arc<ClientHub>,
        readiness: Arc<Readiness>,
    ) -> Self {
        Self {
            binding,
            coordinator,
            hub,
            readiness,
        }
    }

    /// Run every step. On success the readiness cell is `Ready`; on failure
    /// it names the dependency and the error is returned.
    ///
    /// # Errors
    ///
    /// The first failing step's [`DomainError`].
    // @cpt-flow:cpt-cf-quota-enforcement-flow-gear-bootstrap:p1
    pub async fn run(&self) -> Result<Bound, DomainError> {
        match self.run_steps().await {
            Ok(bound) => {
                // @cpt-begin:cpt-cf-quota-enforcement-flow-gear-bootstrap:p1:inst-boot-ready
                self.readiness.mark_ready();
                tracing::info!(target: LOG_TARGET, "quota enforcement bootstrap complete");
                Ok(bound)
                // @cpt-end:cpt-cf-quota-enforcement-flow-gear-bootstrap:p1:inst-boot-ready
            }
            // @cpt-begin:cpt-cf-quota-enforcement-flow-gear-bootstrap:p1:inst-boot-probe-if
            Err((dependency, err)) => {
                // @cpt-begin:cpt-cf-quota-enforcement-flow-gear-bootstrap:p1:inst-boot-probe-abort
                self.readiness.mark_failed(dependency, err.to_string());
                tracing::error!(
                    target: LOG_TARGET,
                    dependency = %dependency,
                    error = %err,
                    "quota enforcement bootstrap failed; the gear serves nothing"
                );
                Err(err)
                // @cpt-end:cpt-cf-quota-enforcement-flow-gear-bootstrap:p1:inst-boot-probe-abort
            } // @cpt-end:cpt-cf-quota-enforcement-flow-gear-bootstrap:p1:inst-boot-probe-if
        }
    }

    async fn run_steps(&self) -> Result<Bound, (Dependency, DomainError)> {
        // @cpt-begin:cpt-cf-quota-enforcement-flow-gear-bootstrap:p1:inst-boot-start
        // Exactly one active storage plugin: the instance the configured vendor
        // selects.
        let storage = self
            .binding
            .resolve_storage()
            .await
            .map_err(|e| (Dependency::Storage, e))?;
        // @cpt-end:cpt-cf-quota-enforcement-flow-gear-bootstrap:p1:inst-boot-start

        // Schema check and default seeding are the plugin's steps.
        storage
            .bootstrap(&BootstrapBundle::foundation())
            .await
            .map_err(|e| (Dependency::Storage, lift_bootstrap_storage_error(e)))?;

        // @cpt-begin:cpt-cf-quota-enforcement-flow-gear-bootstrap:p1:inst-boot-cluster-resolve
        // The cluster resolver validates the operator's binding of the
        // `quota-enforcement` profile: an unbound profile or a backend without a
        // linearizable election fails here. There is no probe of our own.
        let coordinator = self
            .coordinator
            .resolve()
            .await
            .map_err(|e| (Dependency::Cluster, e))?;
        // @cpt-end:cpt-cf-quota-enforcement-flow-gear-bootstrap:p1:inst-boot-cluster-resolve

        // @cpt-begin:cpt-cf-quota-enforcement-flow-gear-bootstrap:p1:inst-boot-pdp-probe
        // Structural PDP probe: the admission boundary is unusable without the
        // `authz-resolver` client. Its liveness is reported by its own health
        // check, which api-gateway aggregates into `/readyz`.
        self.hub
            .get::<dyn AuthZResolverApi>()
            .map_err(|e| (Dependency::Pdp, DomainError::PdpUnavailable(e.to_string())))?;
        // @cpt-end:cpt-cf-quota-enforcement-flow-gear-bootstrap:p1:inst-boot-pdp-probe

        Ok(Bound {
            storage,
            coordinator,
        })
    }
}

/// At bootstrap a schema mismatch is a named, fatal condition rather than
/// the generic internal error the runtime lift produces.
fn lift_bootstrap_storage_error(err: StorageError) -> DomainError {
    match err {
        StorageError::SchemaVersionMismatch {
            installed,
            expected,
        } => DomainError::SchemaVersionMismatch {
            installed,
            expected,
        },
        other => DomainError::from(other),
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "bootstrap_tests.rs"]
mod bootstrap_tests;
