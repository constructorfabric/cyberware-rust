//! Ingress attribution of a subject-based evaluation request
//! (`features/projection-contracts.md`, "Evaluation Request Ingress Validation"
//! and "PDP-Authorized Subject Mapping").
//!
//! Every debit, reserve, preview, and batch item enters here once those
//! operations exist. The order is fixed and each stage has its own error
//! class, so precedence is observable: the public shape is checked before the
//! PDP is called, the PDP authorizes the complete caller-supplied tuple, and
//! only then does the process-local catalogue map kinds to projections and
//! validate the metadata envelopes. No registry call happens on this path, and
//! no failure is ever a `Decision::Denied`.

use authz_resolver_sdk::pep::ResourceType;
use gts::GtsTypeId;
use quota_enforcement_sdk::{
    EvaluationAttribution, MetricId, ResourceProjection, SubjectRef, SubjectScope, TenantId,
};
use serde_json::{Map, Value, json};
use toolkit_macros::domain_model;
use toolkit_security::{AccessScope, SecurityContext};

use super::admission::{Admission, AdmissionTarget};
use super::catalog::{CatalogMiss, ProjectionContractCatalog, parse_metric_under_base};
use super::error::DomainError;
use super::pep::properties;
use super::ports::metrics::{DenialReason, QeMetrics, ValidationReason, ValidationSurface};
use super::tokens;

const LOG_TARGET: &str = "qe.attribution";

/// What a Policy expression may see: `{request, resource}`; `arbitration` is
/// added per Quota inside the evaluation transaction. Attribution and the
/// authenticated principal are kept out by construction.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyInput {
    /// The validated operation-level metadata object.
    pub request: Map<String, Value>,
    /// The validated optional resource projection.
    pub resource: Option<ResourceProjection>,
}

/// The authorized, catalogue-mapped attribution of one request.
#[domain_model]
#[derive(Debug, Clone, PartialEq)]
pub struct MappedAttribution {
    /// The PDP-authorized target tenant.
    pub tenant_id: TenantId,
    /// The complete `(projection_type, subject_id)` set: the tenant-scope
    /// subject materialized from `tenant_id` first, then the mapped claims.
    pub subjects: Vec<SubjectRef>,
    /// The `AccessScope` exactly as the PDP returned it.
    pub access_scope: AccessScope,
}

/// A request that passed ingress: what the evaluation pipeline consumes.
#[domain_model]
#[derive(Debug, Clone, PartialEq)]
pub struct AdmittedEvaluation {
    /// Who and what is being charged.
    pub attribution: MappedAttribution,
    /// The admitted metric.
    pub metric: MetricId,
    /// The Policy-visible input.
    pub input: PolicyInput,
}

/// The ingress step over the PEP boundary and the published catalogue.
// @cpt-dod:cpt-cf-quota-enforcement-dod-ingress-validation:p1
// @cpt-dod:cpt-cf-quota-enforcement-dod-subject-resolution:p1
pub struct Attribution<'a> {
    admission: &'a Admission,
    catalog: &'a ProjectionContractCatalog,
    metrics: &'a dyn QeMetrics,
}

/// The attribution after the shape stage: parsed, not yet authorized.
struct Shaped {
    tenant_id: TenantId,
    metric: MetricId,
    claims: Vec<(SubjectScope, String)>,
    metadata: Map<String, Value>,
    resource: Option<ShapedResource>,
}

struct ShapedResource {
    type_id: GtsTypeId,
    projection: ResourceProjection,
    metadata: Map<String, Value>,
}

impl<'a> Attribution<'a> {
    /// The step over `admission` and the published `catalog`.
    #[must_use]
    pub fn new(
        admission: &'a Admission,
        catalog: &'a ProjectionContractCatalog,
        metrics: &'a dyn QeMetrics,
    ) -> Self {
        Self {
            admission,
            catalog,
            metrics,
        }
    }

    /// Admit one subject-based evaluation request.
    ///
    /// # Errors
    ///
    /// - [`DomainError::InvalidArgument`] with a closed reason token on a
    ///   malformed public shape (before the PDP), on an unknown or unadmitted
    ///   kind, or on a contract violation (after the PDP).
    /// - [`DomainError::PdpDenied`] / [`DomainError::PdpUnavailable`] from the
    ///   PDP, before any catalogue lookup.
    // @cpt-flow:cpt-cf-quota-enforcement-flow-ingress-validation:p1
    // @cpt-algo:cpt-cf-quota-enforcement-algo-subject-resolution:p1
    pub async fn admit_evaluation(
        &self,
        ctx: &SecurityContext,
        resource_type: &ResourceType,
        action: &str,
        attribution: EvaluationAttribution,
    ) -> Result<AdmittedEvaluation, DomainError> {
        // @cpt-begin:cpt-cf-quota-enforcement-flow-ingress-validation:p1:inst-ing-request
        let site = Site {
            ctx,
            resource: resource_type.name(),
            action,
        };
        // @cpt-end:cpt-cf-quota-enforcement-flow-ingress-validation:p1:inst-ing-request

        // @cpt-begin:cpt-cf-quota-enforcement-flow-ingress-validation:p1:inst-ing-shape
        // @cpt-begin:cpt-cf-quota-enforcement-algo-subject-resolution:p1:inst-res-shape
        let shaped = self.shape(&site, attribution)?;
        // @cpt-end:cpt-cf-quota-enforcement-algo-subject-resolution:p1:inst-res-shape
        // @cpt-end:cpt-cf-quota-enforcement-flow-ingress-validation:p1:inst-ing-shape

        // @cpt-begin:cpt-cf-quota-enforcement-flow-ingress-validation:p1:inst-ing-authz
        // @cpt-begin:cpt-cf-quota-enforcement-algo-subject-resolution:p1:inst-res-authz
        let admitted = self
            .admission
            .admit_with_properties(
                ctx,
                resource_type,
                action,
                AdmissionTarget::tenant(shaped.tenant_id),
                shaped.pdp_properties(),
            )
            .await?;
        // @cpt-end:cpt-cf-quota-enforcement-algo-subject-resolution:p1:inst-res-authz
        // @cpt-end:cpt-cf-quota-enforcement-flow-ingress-validation:p1:inst-ing-authz

        // @cpt-begin:cpt-cf-quota-enforcement-flow-ingress-validation:p1:inst-ing-lookup
        // @cpt-begin:cpt-cf-quota-enforcement-algo-subject-resolution:p1:inst-res-tenant
        // @cpt-begin:cpt-cf-quota-enforcement-algo-subject-resolution:p1:inst-res-each
        // @cpt-begin:cpt-cf-quota-enforcement-algo-subject-resolution:p1:inst-res-map
        // @cpt-begin:cpt-cf-quota-enforcement-algo-subject-resolution:p1:inst-res-append
        let mut subjects = Vec::with_capacity(shaped.claims.len() + 1);
        let tenant_projection = self.map_kind(
            &site,
            &shaped.metric,
            &SubjectScope::tenant(),
            "tenant_id",
            tokens::METRIC_NOT_ADMITTED,
        )?;
        subjects.push(SubjectRef {
            projection_type: tenant_projection,
            subject_id: shaped.tenant_id.to_string(),
        });
        for (scope, id) in &shaped.claims {
            let projection = self.map_kind(
                &site,
                &shaped.metric,
                scope,
                "subjects",
                tokens::SUBJECT_KIND_NOT_ADMITTED,
            )?;
            subjects.push(SubjectRef {
                projection_type: projection,
                subject_id: id.clone(),
            });
        }
        // @cpt-end:cpt-cf-quota-enforcement-algo-subject-resolution:p1:inst-res-append
        // @cpt-end:cpt-cf-quota-enforcement-algo-subject-resolution:p1:inst-res-map
        // @cpt-end:cpt-cf-quota-enforcement-algo-subject-resolution:p1:inst-res-each
        // @cpt-end:cpt-cf-quota-enforcement-algo-subject-resolution:p1:inst-res-tenant
        // @cpt-end:cpt-cf-quota-enforcement-flow-ingress-validation:p1:inst-ing-lookup

        // @cpt-begin:cpt-cf-quota-enforcement-flow-ingress-validation:p1:inst-ing-metadata
        let request_contract = self
            .catalog
            .request_contract(&shaped.metric)
            .ok_or_else(|| {
                // Unreachable once the tenant scope mapped: the catalogue holds a
                // request contract for every admitted metric by construction.
                self.metrics
                    .record_admitted_metric_violation(ValidationSurface::RequestSubject);
                DomainError::InvalidArgument {
                    field: "metric",
                    reason: tokens::METRIC_NOT_ADMITTED,
                }
            })?;
        let envelope = json!({
            "type": request_contract.type_id.as_ref(),
            "metadata": Value::Object(shaped.metadata.clone()),
        });
        self.validate_envelope(
            &site,
            ValidationSurface::RequestSubject,
            "metadata",
            &request_contract.contract,
            &envelope,
        )?;
        let resource = match shaped.resource {
            None => None,
            Some(shaped_resource) => Some(self.validate_resource(&site, shaped_resource)?),
        };
        // @cpt-end:cpt-cf-quota-enforcement-flow-ingress-validation:p1:inst-ing-metadata

        // @cpt-begin:cpt-cf-quota-enforcement-flow-ingress-validation:p1:inst-ing-map
        // @cpt-begin:cpt-cf-quota-enforcement-flow-ingress-validation:p1:inst-ing-forward
        // @cpt-begin:cpt-cf-quota-enforcement-algo-subject-resolution:p1:inst-res-return
        // @cpt-begin:cpt-cf-quota-enforcement-flow-owner-projection-publication:p1:inst-pub-return
        // Any authorized caller reached the owner's projections through the
        // catalogue; none of them chose a projection.
        Ok(AdmittedEvaluation {
            attribution: MappedAttribution {
                tenant_id: shaped.tenant_id,
                subjects,
                access_scope: admitted.access_scope,
            },
            metric: shaped.metric,
            input: PolicyInput {
                request: shaped.metadata,
                resource,
            },
        })
        // @cpt-end:cpt-cf-quota-enforcement-flow-owner-projection-publication:p1:inst-pub-return
        // @cpt-end:cpt-cf-quota-enforcement-algo-subject-resolution:p1:inst-res-return
        // @cpt-end:cpt-cf-quota-enforcement-flow-ingress-validation:p1:inst-ing-forward
        // @cpt-end:cpt-cf-quota-enforcement-flow-ingress-validation:p1:inst-ing-map
    }

    /// Stage 1: the public request shape, before any PDP call.
    fn shape(
        &self,
        site: &Site<'_>,
        attribution: EvaluationAttribution,
    ) -> Result<Shaped, DomainError> {
        if attribution.tenant_id.as_uuid().is_nil() {
            return Err(self.reject_shape(site, "tenant_id", tokens::TENANT_ID_REQUIRED, ""));
        }
        let metric = parse_metric_under_base(&attribution.metric).ok_or_else(|| {
            self.reject_shape(site, "metric", tokens::METRIC_INVALID, &attribution.metric)
        })?;

        let mut claims = Vec::with_capacity(attribution.subjects.len());
        for claim in &attribution.subjects {
            if claim.id.trim().is_empty() {
                return Err(self.reject_shape(
                    site,
                    "subjects",
                    tokens::SUBJECT_ID_REQUIRED,
                    &claim.kind,
                ));
            }
            let scope = SubjectScope::parse(&claim.kind).map_err(|_| {
                self.reject_shape(site, "subjects", tokens::SUBJECT_KIND_INVALID, &claim.kind)
            })?;
            if scope.is_tenant() {
                return Err(self.reject_shape(
                    site,
                    "subjects",
                    tokens::TENANT_SCOPE_REPEATED,
                    &claim.kind,
                ));
            }
            if claims.iter().any(|(seen, _)| *seen == scope) {
                return Err(self.reject_shape(
                    site,
                    "subjects",
                    tokens::SUBJECT_KIND_DUPLICATE,
                    &claim.kind,
                ));
            }
            claims.push((scope, claim.id.clone()));
        }

        // Required on the wire, `{}` included; never defaulted.
        let Some(metadata) = attribution.metadata else {
            return Err(self.reject_missing(site, "metadata", tokens::METADATA_REQUIRED));
        };

        let resource = match attribution.resource {
            None => None,
            Some(projection) => {
                let type_id = GtsTypeId::try_new(&projection.r#type).map_err(|_| {
                    self.reject_shape(
                        site,
                        "resource.type",
                        tokens::RESOURCE_TYPE_INVALID,
                        &projection.r#type,
                    )
                })?;
                let Some(metadata) = projection.metadata.clone() else {
                    return Err(self.reject_missing(
                        site,
                        "resource.metadata",
                        tokens::RESOURCE_METADATA_REQUIRED,
                    ));
                };
                Some(ShapedResource {
                    type_id,
                    projection,
                    metadata,
                })
            }
        };

        Ok(Shaped {
            tenant_id: attribution.tenant_id,
            metric,
            claims,
            metadata,
            resource,
        })
    }

    /// Stage 3a: `(metric, kind)` through the catalogue's unique index.
    fn map_kind(
        &self,
        site: &Site<'_>,
        metric: &MetricId,
        scope: &SubjectScope,
        field: &'static str,
        reason: &'static str,
    ) -> Result<GtsTypeId, DomainError> {
        match self.catalog.map_subject(metric, scope) {
            Ok(projection) => Ok(projection.clone()),
            // @cpt-begin:cpt-cf-quota-enforcement-flow-ingress-validation:p1:inst-ing-metric-if
            // @cpt-begin:cpt-cf-quota-enforcement-flow-ingress-validation:p1:inst-ing-metric
            // @cpt-begin:cpt-cf-quota-enforcement-algo-subject-resolution:p1:inst-res-invalid-if
            // @cpt-begin:cpt-cf-quota-enforcement-algo-subject-resolution:p1:inst-res-invalid
            Err(miss) => {
                self.metrics
                    .record_admitted_metric_violation(ValidationSurface::RequestSubject);
                let reason = match miss {
                    CatalogMiss::MetricNotAdmitted => tokens::METRIC_NOT_ADMITTED,
                    CatalogMiss::KindUnknown | CatalogMiss::KindNotAdmitted => reason,
                };
                let field = if miss == CatalogMiss::MetricNotAdmitted {
                    "metric"
                } else {
                    field
                };
                tracing::warn!(
                    target: LOG_TARGET,
                    subject_id = %site.ctx.subject_id(),
                    resource = site.resource,
                    action = site.action,
                    metric = %metric,
                    scope = %scope,
                    ?miss,
                    "no configured projection admits the metric at this scope"
                );
                Err(DomainError::InvalidArgument { field, reason })
            } // @cpt-end:cpt-cf-quota-enforcement-algo-subject-resolution:p1:inst-res-invalid
              // @cpt-end:cpt-cf-quota-enforcement-algo-subject-resolution:p1:inst-res-invalid-if
              // @cpt-end:cpt-cf-quota-enforcement-flow-ingress-validation:p1:inst-ing-metric
              // @cpt-end:cpt-cf-quota-enforcement-flow-ingress-validation:p1:inst-ing-metric-if
        }
    }

    /// Stage 3b: the resource projection against its configured contract.
    fn validate_resource(
        &self,
        site: &Site<'_>,
        resource: ShapedResource,
    ) -> Result<ResourceProjection, DomainError> {
        let Some(contract) = self.catalog.resource_projection(&resource.type_id) else {
            self.metrics.record_contract_validation_failure(
                ValidationSurface::RequestResource,
                ValidationReason::ProjectionNotResolvable,
            );
            tracing::warn!(
                target: LOG_TARGET,
                subject_id = %site.ctx.subject_id(),
                resource_type = %resource.type_id,
                "the resource projection is not in the configured catalogue"
            );
            return Err(DomainError::InvalidArgument {
                field: "resource.type",
                reason: tokens::RESOURCE_TYPE_UNKNOWN,
            });
        };
        // The complete `{type, id?, metadata}` document, `id` present only when
        // the caller sent one: the resource base allows an omitted id and
        // requires a string when present.
        let mut document = Map::new();
        document.insert(
            "type".to_owned(),
            Value::String(resource.type_id.as_ref().to_owned()),
        );
        if let Some(id) = &resource.projection.id {
            document.insert("id".to_owned(), Value::String(id.clone()));
        }
        document.insert("metadata".to_owned(), Value::Object(resource.metadata));
        self.validate_envelope(
            site,
            ValidationSurface::RequestResource,
            "resource",
            &contract.contract,
            &Value::Object(document),
        )?;
        Ok(resource.projection)
    }

    fn validate_envelope(
        &self,
        site: &Site<'_>,
        surface: ValidationSurface,
        field: &'static str,
        contract: &super::catalog::CompiledContract,
        document: &Value,
    ) -> Result<(), DomainError> {
        contract.validate(document).map_err(|violations| {
            self.metrics
                .record_contract_validation_failure(surface, ValidationReason::SchemaViolation);
            tracing::warn!(
                target: LOG_TARGET,
                subject_id = %site.ctx.subject_id(),
                resource = site.resource,
                action = site.action,
                contract = %contract.type_id(),
                field,
                violations = ?violations,
                "contract violation at ingress"
            );
            DomainError::InvalidArgument {
                field,
                reason: tokens::CONTRACT_VIOLATION,
            }
        })
    }

    /// A malformed public shape: a denial before the PDP, recorded on both
    /// the denial counter and the caller-attribution validation surface.
    fn reject_shape(
        &self,
        site: &Site<'_>,
        field: &'static str,
        reason: &'static str,
        offending: &str,
    ) -> DomainError {
        self.record_shape(
            site,
            field,
            reason,
            offending,
            ValidationReason::ShapeInvalid,
        )
    }

    fn reject_missing(
        &self,
        site: &Site<'_>,
        field: &'static str,
        reason: &'static str,
    ) -> DomainError {
        self.record_shape(site, field, reason, "", ValidationReason::MetadataMissing)
    }

    fn record_shape(
        &self,
        site: &Site<'_>,
        field: &'static str,
        reason: &'static str,
        offending: &str,
        validation: ValidationReason,
    ) -> DomainError {
        self.metrics.record_denial(DenialReason::InvalidArgument);
        self.metrics
            .record_contract_validation_failure(ValidationSurface::CallerAttribution, validation);
        tracing::warn!(
            target: LOG_TARGET,
            subject_id = %site.ctx.subject_id(),
            resource = site.resource,
            action = site.action,
            field,
            reason,
            offending,
            "malformed attribution rejected before the PDP"
        );
        DomainError::InvalidArgument { field, reason }
    }
}

impl Shaped {
    /// The rest of the attribution tuple as PDP resource properties. The
    /// tenant is the admission target and is written by `Admission` itself.
    fn pdp_properties(&self) -> Map<String, Value> {
        let mut properties = Map::new();
        properties.insert(
            properties::METRIC.to_owned(),
            Value::String(self.metric.as_gts().as_ref().to_owned()),
        );
        properties.insert(
            properties::SUBJECTS.to_owned(),
            Value::Array(
                self.claims
                    .iter()
                    .map(|(scope, id)| json!({ "kind": scope.as_str(), "id": id }))
                    .collect(),
            ),
        );
        if let Some(resource) = &self.resource {
            let mut document = Map::new();
            document.insert(
                "type".to_owned(),
                Value::String(resource.type_id.as_ref().to_owned()),
            );
            if let Some(id) = &resource.projection.id {
                document.insert("id".to_owned(), Value::String(id.clone()));
            }
            document.insert(
                "metadata".to_owned(),
                Value::Object(resource.metadata.clone()),
            );
            properties.insert(properties::RESOURCE.to_owned(), Value::Object(document));
        }
        properties
    }
}

/// Log context of one ingress call.
struct Site<'c> {
    ctx: &'c SecurityContext,
    resource: &'c str,
    action: &'c str,
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "attribution_tests.rs"]
mod attribution_tests;
