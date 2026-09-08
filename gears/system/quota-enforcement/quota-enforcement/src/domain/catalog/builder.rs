//! The bootstrap consistency set that produces the catalogue
//! (`features/projection-contracts.md`, "Catalogue Bootstrap and Consistency
//! Set"), and the compatibility check against the Quotas storage holds.
//!
//! Every rejection is a [`DomainError::CatalogInvalid`] naming the failing
//! check and its subject, recorded on `contract_validation_failures_total`
//! with the `bootstrap` surface before it is returned. The registry is read only
//! here; the published catalogue answers every later question.

use std::collections::{HashMap, HashSet};

use gts::GtsTypeId;
use quota_enforcement_sdk::{
    CONSTRAINT_BASE, ContractRef, METRIC_BASE_TYPE, MetricId, ProjectionBinding, REQUEST_BASE,
    RESOURCE_BASE, SCOPE_TYPE, SUBJECT_BASE, SubjectScope,
};
use serde_json::Value;
use toolkit_macros::domain_model;

use super::model::{
    CompiledContract, ConstraintContract, MetricRequestContract, ProjectionContractCatalog,
    ResourceProjectionContract, SubjectProjectionContract, parse_metric_under_base,
};
use crate::domain::error::DomainError;
use crate::domain::ports::contracts::{ContractRegistry, RegisteredType};
use crate::domain::ports::metrics::{QeMetrics, ValidationReason, ValidationSurface};

const LOG_TARGET: &str = "qe.bootstrap";

/// The projections the operator configured for evaluation. Request and
/// constraint contracts are discovered from the registry, not configured.
#[domain_model]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CatalogConfig {
    /// Concrete subject projections derived from `gts.cf.core.qe.subj.v1~`.
    pub subject_projections: Vec<GtsTypeId>,
    /// Concrete resource projections derived from `gts.cf.core.qe.res.v1~`.
    pub resource_projections: Vec<GtsTypeId>,
}

/// Runs the consistency set over a registry and produces the catalogue.
pub struct CatalogBuilder<'a> {
    registry: &'a dyn ContractRegistry,
    metrics: &'a dyn QeMetrics,
}

impl<'a> CatalogBuilder<'a> {
    /// A builder reading `registry` and recording rejections on `metrics`.
    #[must_use]
    pub fn new(registry: &'a dyn ContractRegistry, metrics: &'a dyn QeMetrics) -> Self {
        Self { registry, metrics }
    }

    /// Resolve the configured projections and their contracts into a
    /// catalogue, or reject the configuration.
    ///
    /// # Errors
    ///
    /// - [`DomainError::CatalogInvalid`] on the first failing consistency check.
    /// - [`DomainError::TypesRegistryUnavailable`] when the registry cannot
    ///   answer or a registered contract does not resolve.
    // @cpt-algo:cpt-cf-quota-enforcement-algo-catalog-bootstrap:p1
    pub async fn build(
        &self,
        config: &CatalogConfig,
    ) -> Result<ProjectionContractCatalog, DomainError> {
        // @cpt-begin:cpt-cf-quota-enforcement-algo-catalog-bootstrap:p1:inst-cat-resolve
        // @cpt-begin:cpt-cf-quota-enforcement-algo-catalog-bootstrap:p1:inst-cat-concrete
        // @cpt-begin:cpt-cf-quota-enforcement-algo-catalog-bootstrap:p1:inst-cat-metric
        // @cpt-begin:cpt-cf-quota-enforcement-algo-catalog-bootstrap:p1:inst-cat-unique
        let mut subjects = Vec::with_capacity(config.subject_projections.len());
        let mut pairs: HashMap<(MetricId, SubjectScope), GtsTypeId> = HashMap::new();
        let mut admitted: HashSet<MetricId> = HashSet::new();
        for id in &config.subject_projections {
            let registered = self.resolve_concrete(id, SUBJECT_BASE).await?;
            let scope = self.scope_of(&registered).await?;
            let metrics = self.admitted_metrics_of(&registered).await?;
            for metric in &metrics {
                if let Some(other) = pairs.insert((metric.clone(), scope.clone()), id.clone()) {
                    self.metrics
                        .record_admitted_metric_violation(ValidationSurface::Bootstrap);
                    return Err(self.reject(
                        ValidationReason::DuplicatePair,
                        format!("{metric} at {scope} is admitted by {other} and {id}"),
                    ));
                }
            }
            admitted.extend(metrics.iter().cloned());
            subjects.push(SubjectProjectionContract {
                type_id: id.clone(),
                scope,
                admitted_metrics: metrics,
            });
        }
        // @cpt-end:cpt-cf-quota-enforcement-algo-catalog-bootstrap:p1:inst-cat-unique
        // @cpt-end:cpt-cf-quota-enforcement-algo-catalog-bootstrap:p1:inst-cat-metric

        let mut resources = Vec::with_capacity(config.resource_projections.len());
        for id in &config.resource_projections {
            let registered = self.resolve_concrete(id, RESOURCE_BASE).await?;
            let contract = self.compile(&registered)?;
            resources.push(ResourceProjectionContract {
                type_id: id.clone(),
                contract,
            });
        }
        // @cpt-end:cpt-cf-quota-enforcement-algo-catalog-bootstrap:p1:inst-cat-concrete
        // @cpt-end:cpt-cf-quota-enforcement-algo-catalog-bootstrap:p1:inst-cat-resolve

        // @cpt-begin:cpt-cf-quota-enforcement-algo-catalog-bootstrap:p1:inst-cat-contract-pair
        let requests = self.request_contracts(&admitted).await?;
        // @cpt-end:cpt-cf-quota-enforcement-algo-catalog-bootstrap:p1:inst-cat-contract-pair

        // @cpt-begin:cpt-cf-quota-enforcement-algo-catalog-bootstrap:p1:inst-cat-index
        // @cpt-begin:cpt-cf-quota-enforcement-algo-catalog-bootstrap:p1:inst-cat-publish
        let catalog = ProjectionContractCatalog::assemble(subjects, resources, requests);
        tracing::info!(
            target: LOG_TARGET,
            subject_projections = config.subject_projections.len(),
            resource_projections = config.resource_projections.len(),
            admitted_metrics = admitted.len(),
            "projection contract catalogue built"
        );
        Ok(catalog)
        // @cpt-end:cpt-cf-quota-enforcement-algo-catalog-bootstrap:p1:inst-cat-publish
        // @cpt-end:cpt-cf-quota-enforcement-algo-catalog-bootstrap:p1:inst-cat-index
    }

    /// The catalogue must still admit every `(metric, projection)` an active
    /// Quota binds; otherwise the configuration would strand those Quotas.
    ///
    /// # Errors
    ///
    /// Returns [`DomainError::CatalogInvalid`] with `IncompatibleState`.
    ///
    /// The Quota half of catalogue compatibility, over the storage contract's
    /// `read_active_projection_bindings`. Policy compatibility is tracked by
    /// the resolution-policy-engine feature: a Policy version carries no
    /// contract reference yet.
    // @cpt-begin:cpt-cf-quota-enforcement-algo-catalog-bootstrap:p1:inst-cat-compat-if
    pub fn check_compatibility(
        &self,
        catalog: &ProjectionContractCatalog,
        bindings: &HashSet<ProjectionBinding>,
    ) -> Result<(), DomainError> {
        let mut ordered: Vec<&ProjectionBinding> = bindings.iter().collect();
        ordered.sort_by_key(|b| (b.metric.to_string(), b.projection_type.to_string()));
        for binding in ordered {
            if !catalog.admits(&binding.projection_type, &binding.metric) {
                // @cpt-begin:cpt-cf-quota-enforcement-algo-catalog-bootstrap:p1:inst-cat-compat
                return Err(self.reject(
                    ValidationReason::IncompatibleState,
                    format!(
                        "active Quotas bind {} for {}",
                        binding.projection_type, binding.metric
                    ),
                ));
                // @cpt-end:cpt-cf-quota-enforcement-algo-catalog-bootstrap:p1:inst-cat-compat
            }
        }
        Ok(())
    }
    // @cpt-end:cpt-cf-quota-enforcement-algo-catalog-bootstrap:p1:inst-cat-compat-if

    /// A registered, concrete type derived from `base`.
    async fn resolve_concrete(
        &self,
        id: &GtsTypeId,
        base: &'static str,
    ) -> Result<RegisteredType, DomainError> {
        let registered = self
            .registry
            .type_schema(id)
            .await?
            .ok_or_else(|| self.reject(ValidationReason::Unregistered, id.to_string()))?;
        if registered.is_abstract {
            return Err(self.reject(ValidationReason::Abstract, id.to_string()));
        }
        if !registered.derives_from(base) {
            return Err(self.reject(
                ValidationReason::NotDerived,
                format!("{id} is not derived from {base}"),
            ));
        }
        Ok(registered)
    }

    /// The projection's effective `scope` trait, as a registered scope instance.
    async fn scope_of(&self, projection: &RegisteredType) -> Result<SubjectScope, DomainError> {
        let raw = projection
            .effective_traits
            .get("scope")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                self.reject(
                    ValidationReason::SchemaInvalid,
                    format!("{} declares no scope trait", projection.id),
                )
            })?;
        let scope = SubjectScope::parse(raw).map_err(|e| {
            self.reject(
                ValidationReason::ScopeInvalid,
                format!("{}: {e}", projection.id),
            )
        })?;
        match self.registry.instance_type(scope.as_gts()).await? {
            Some(declaring) if declaring.as_ref() == SCOPE_TYPE => Ok(scope),
            _ => Err(self.reject(
                ValidationReason::ScopeInvalid,
                format!(
                    "{}: {scope} is not a registered scope instance",
                    projection.id
                ),
            )),
        }
    }

    /// The projection's effective `admitted_metrics` trait, every entry a
    /// registered instance of the metric base. `x-gts-ref` proved only the
    /// prefix; registration and the instance-of relation are checked here.
    async fn admitted_metrics_of(
        &self,
        projection: &RegisteredType,
    ) -> Result<HashSet<MetricId>, DomainError> {
        let entries = projection
            .effective_traits
            .get("admitted_metrics")
            .and_then(Value::as_array)
            .filter(|a| !a.is_empty())
            .ok_or_else(|| {
                self.reject(
                    ValidationReason::SchemaInvalid,
                    format!("{} declares no admitted metrics", projection.id),
                )
            })?;
        let mut metrics = HashSet::with_capacity(entries.len());
        for entry in entries {
            let text = entry.as_str().ok_or_else(|| {
                self.reject(
                    ValidationReason::SchemaInvalid,
                    format!("{}: admitted metric is not a string", projection.id),
                )
            })?;
            let metric = parse_metric_under_base(text)
                .ok_or_else(|| self.reject(ValidationReason::MetricNotInstance, text))?;
            match self.registry.instance_type(metric.as_gts()).await? {
                None => return Err(self.reject(ValidationReason::MetricUnregistered, text)),
                Some(declaring) if declaring.as_ref() != METRIC_BASE_TYPE => {
                    return Err(self.reject(
                        ValidationReason::MetricNotInstance,
                        format!("{text} is an instance of {declaring}"),
                    ));
                }
                Some(_) => {}
            }
            metrics.insert(metric);
        }
        Ok(metrics)
    }

    /// Exactly one concrete request contract per admitted metric, discovered
    /// from the registry listing. Contracts for other metrics are ignored, so
    /// an unrelated owner cannot break this deployment (decision 1).
    async fn request_contracts(
        &self,
        admitted: &HashSet<MetricId>,
    ) -> Result<Vec<MetricRequestContract>, DomainError> {
        if admitted.is_empty() {
            return Ok(Vec::new());
        }
        let discovered = self
            .registry
            .derived_types(&GtsTypeId::new(REQUEST_BASE))
            .await?;
        let mut candidates: HashMap<&MetricId, Vec<GtsTypeId>> = HashMap::new();
        for contract in discovered {
            if contract.is_abstract {
                continue;
            }
            let Some(metric) = contract
                .declared_traits
                .get("metric")
                .and_then(Value::as_str)
                .and_then(parse_metric_under_base)
            else {
                continue;
            };
            if let Some(admitted_metric) = admitted.get(&metric) {
                candidates
                    .entry(admitted_metric)
                    .or_default()
                    .push(contract.id);
            }
        }

        let mut metrics: Vec<&MetricId> = admitted.iter().collect();
        metrics.sort_by_key(ToString::to_string);
        let mut requests = Vec::with_capacity(metrics.len());
        for metric in metrics {
            match candidates.get(metric).map_or(&[][..], Vec::as_slice) {
                [] => {
                    return Err(
                        self.reject(ValidationReason::RequestContractMissing, metric.to_string())
                    );
                }
                [single] => requests.push(self.request_contract(metric, single).await?),
                many => {
                    let ids: Vec<String> = many.iter().map(ToString::to_string).collect();
                    return Err(self.reject(
                        ValidationReason::RequestContractAmbiguous,
                        format!("{metric} has request contracts {}", ids.join(", ")),
                    ));
                }
            }
        }
        Ok(requests)
    }

    /// One request contract with its attached constraint contract, both
    /// resolved and compiled.
    async fn request_contract(
        &self,
        metric: &MetricId,
        id: &GtsTypeId,
    ) -> Result<MetricRequestContract, DomainError> {
        let registered = self.resolve_concrete(id, REQUEST_BASE).await?;
        let declared = registered
            .effective_traits
            .get("metric")
            .and_then(Value::as_str);
        if declared != Some(metric.as_gts().as_ref()) {
            return Err(self.reject(
                ValidationReason::SchemaInvalid,
                format!("{id} was listed for {metric} but resolved with metric {declared:?}"),
            ));
        }
        let constraint_id = registered
            .effective_traits
            .get("constraint_contract")
            .and_then(Value::as_str)
            .and_then(|s| GtsTypeId::try_new(s).ok())
            .ok_or_else(|| {
                self.reject(
                    ValidationReason::ConstraintInvalid,
                    format!("{id} declares no constraint contract"),
                )
            })?;
        let constraint = self
            .registry
            .type_schema(&constraint_id)
            .await?
            .ok_or_else(|| {
                self.reject(
                    ValidationReason::ConstraintInvalid,
                    format!("{constraint_id} is not registered"),
                )
            })?;
        if constraint.is_abstract || !constraint.derives_from(CONSTRAINT_BASE) {
            return Err(self.reject(
                ValidationReason::ConstraintInvalid,
                format!("{constraint_id} is abstract or not derived from {CONSTRAINT_BASE}"),
            ));
        }
        let reference = ContractRef::for_type(&constraint_id).ok_or_else(|| {
            self.reject(
                ValidationReason::ConstraintInvalid,
                format!("{constraint_id} carries no version"),
            )
        })?;
        Ok(MetricRequestContract {
            type_id: id.clone(),
            metric: metric.clone(),
            constraint: ConstraintContract {
                reference,
                contract: self.compile(&constraint)?,
            },
            contract: self.compile(&registered)?,
        })
    }

    fn compile(&self, registered: &RegisteredType) -> Result<CompiledContract, DomainError> {
        CompiledContract::compile(registered.id.clone(), registered.schema.clone()).map_err(|e| {
            self.reject(
                ValidationReason::SchemaInvalid,
                format!("{}: {e}", registered.id),
            )
        })
    }

    /// Record and build one rejection. The catalogue is never published after
    /// it; bootstrap fails and the gear serves nothing.
    // @cpt-begin:cpt-cf-quota-enforcement-algo-catalog-bootstrap:p1:inst-cat-fail-if
    // @cpt-begin:cpt-cf-quota-enforcement-algo-catalog-bootstrap:p1:inst-cat-fail
    fn reject(&self, reason: ValidationReason, subject: impl Into<String>) -> DomainError {
        let subject = subject.into();
        self.metrics
            .record_contract_validation_failure(ValidationSurface::Bootstrap, reason);
        tracing::error!(
            target: LOG_TARGET,
            %reason,
            subject = %subject,
            "projection contract catalogue rejected"
        );
        DomainError::CatalogInvalid { reason, subject }
    }
    // @cpt-end:cpt-cf-quota-enforcement-algo-catalog-bootstrap:p1:inst-cat-fail
    // @cpt-end:cpt-cf-quota-enforcement-algo-catalog-bootstrap:p1:inst-cat-fail-if
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "builder_tests.rs"]
mod builder_tests;
