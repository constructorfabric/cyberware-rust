//! Fail-closed gear bootstrap (`features/foundation.md`, "Gear Bootstrap and
//! Readiness").
//!
//! Runs in the lifecycle entry before the ready signal. Every step that fails
//! records the failing dependency in [`Readiness`], so the health endpoint
//! names it, and returns an error so the runtime never marks the gear ready.
//! The projection-contracts feature adds the QE-owned GTS registration, the
//! catalogue consistency set, and the compatibility check against active
//! Quotas (`features/projection-contracts.md`). The quota-lifecycle feature
//! adds the removed-metric scan over the bound metrics and binds the registries
//! the write path needs. Later features extend [`Bootstrap::run`] with their
//! own steps.

use std::sync::Arc;

use quota_enforcement_sdk::{
    BootstrapBundle, QuotaEnforcementStoragePluginV1, StorageError, owned_definitions,
};
use toolkit_macros::domain_model;

use super::catalog::{CatalogBuilder, CatalogConfig, ProjectionContractCatalog};
use super::error::{Dependency, DomainError};
use super::plugins::PluginBinding;
use super::ports::contracts::ContractRegistry;
use super::ports::coordination::{CoordinatorBinding, SingletonCoordinator};
use super::ports::metric_registry::MetricRegistry;
use super::ports::metrics::QeMetrics;
use super::ports::pdp::PdpProbe;
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
    /// The published projection contract catalogue. Immutable for the process.
    pub catalog: Arc<ProjectionContractCatalog>,
    /// The contract registry the write path snapshots projections from.
    pub registry: Arc<dyn ContractRegistry>,
    /// The metric identity and classification registry.
    pub metric_registry: Arc<dyn MetricRegistry>,
}

/// The registry the catalogue is built from and the projections to build it
/// for.
#[domain_model]
pub struct CatalogBinding {
    /// The contract registry (platform `types-registry`).
    pub registry: Arc<dyn ContractRegistry>,
    /// The configured projections.
    pub config: CatalogConfig,
}

/// The bootstrap procedure.
#[domain_model]
pub struct Bootstrap {
    binding: PluginBinding,
    coordinator: Arc<dyn CoordinatorBinding>,
    pdp: Arc<dyn PdpProbe>,
    catalog: CatalogBinding,
    metric_registry: Arc<dyn MetricRegistry>,
    metrics: Arc<dyn QeMetrics>,
    readiness: Arc<Readiness>,
}

impl Bootstrap {
    /// Assemble the procedure.
    #[must_use]
    pub fn new(
        binding: PluginBinding,
        coordinator: Arc<dyn CoordinatorBinding>,
        pdp: Arc<dyn PdpProbe>,
        catalog: CatalogBinding,
        metric_registry: Arc<dyn MetricRegistry>,
        metrics: Arc<dyn QeMetrics>,
        readiness: Arc<Readiness>,
    ) -> Self {
        Self {
            binding,
            coordinator,
            pdp,
            catalog,
            metric_registry,
            metrics,
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

        // @cpt-begin:cpt-cf-quota-enforcement-flow-owner-projection-publication:p1:inst-pub-boot
        // @cpt-begin:cpt-cf-quota-enforcement-algo-catalog-bootstrap:p1:inst-cat-bases
        // The QE-owned GTS definitions: the four abstract bases, the scope
        // type, and its two well-known instances. Idempotent; a byte-identical
        // definition already present is a success, a different one a conflict.
        let definitions = owned_definitions().map_err(|e| {
            (
                Dependency::Catalog,
                DomainError::Internal(format!("embedded GTS definition does not parse: {e}")),
            )
        })?;
        self.catalog
            .registry
            .ensure_registered(&definitions)
            .await
            .map_err(|e| (catalog_dependency(&e), e))?;
        // @cpt-end:cpt-cf-quota-enforcement-algo-catalog-bootstrap:p1:inst-cat-bases

        // The consistency set over the configured projections, then the
        // compatibility check against the Quotas storage holds. The catalogue
        // is published only after both pass; a failure serves nothing.
        let builder = CatalogBuilder::new(self.catalog.registry.as_ref(), self.metrics.as_ref());
        let catalog = builder
            .build(&self.catalog.config)
            .await
            .map_err(|e| (catalog_dependency(&e), e))?;
        let bindings = storage
            .read_active_projection_bindings()
            .await
            .map_err(|e| (Dependency::Storage, DomainError::from(e)))?;
        builder
            .check_compatibility(&catalog, &bindings)
            .map_err(|e| (Dependency::Catalog, e))?;
        // @cpt-end:cpt-cf-quota-enforcement-flow-owner-projection-publication:p1:inst-pub-boot

        // A persisted Quota whose metric was later removed from the registry is
        // flagged, never deactivated: every distinct bound metric is looked up
        // once, a registry that does not answer fails readiness.
        let mut seen = std::collections::HashSet::new();
        for metric in bindings.iter().map(|b| &b.metric) {
            if !seen.insert(metric.clone()) {
                continue;
            }
            let described = self
                .metric_registry
                .describe(metric)
                .await
                .map_err(|e| (Dependency::TypesRegistry, e))?;
            if described.is_none() {
                tracing::warn!(
                    target: LOG_TARGET,
                    metric = %metric,
                    "active Quotas reference a metric the types registry no longer knows"
                );
            }
        }

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
        // One round trip to the PDP. `init` already proved the client is
        // registered; a registered client over an unreachable PDP would still
        // deny every request, so the gear must not report ready behind it.
        self.pdp.probe().await.map_err(|e| (Dependency::Pdp, e))?;
        // @cpt-end:cpt-cf-quota-enforcement-flow-gear-bootstrap:p1:inst-boot-pdp-probe

        Ok(Bound {
            storage,
            coordinator,
            catalog: Arc::new(catalog),
            registry: self.catalog.registry.clone(),
            metric_registry: self.metric_registry.clone(),
        })
    }
}

/// A catalogue failure is the catalogue's fault when the registry answered
/// and a consistency check failed, and the registry's when it did not answer.
fn catalog_dependency(err: &DomainError) -> Dependency {
    match err {
        DomainError::CatalogInvalid { .. } => Dependency::Catalog,
        _ => Dependency::TypesRegistry,
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
