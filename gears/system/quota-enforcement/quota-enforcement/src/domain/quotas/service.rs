//! `QuotaManagement`: the Quota lifecycle component (DESIGN section 3.3,
//! `QuotaManagementService`; `features/quota-lifecycle.md`).
//!
//! Every operation runs in the same order: the request shape, the PDP, then
//! the catalogue and the registry, then storage. Nothing before the storage
//! call opens a transaction, and nothing after it touches the registry. REST
//! and the in-process client both enter here, so both transports share one
//! authorization boundary.

use std::collections::HashMap;

use quota_enforcement_sdk::{
    DeactivateOutcome, MetricId, MetricKind, PageRequest, PageResult, Quota, QuotaDraft,
    QuotaEnforcementStoragePluginV1, QuotaFilter, QuotaId, QuotaStatus, QuotaView, TenantId,
};
use time::OffsetDateTime;
use toolkit_security::{AccessScope, SecurityContext};

use super::events::{ChangeKind, quota_changed};
use super::metadata::validate_metadata;
use super::request::{CreateQuotaRequest, ListQuotasRequest, UpdateQuotaRequest};
use super::validation::{
    QuotaLimits, validate_create_shape, validate_list, validate_patched_shape,
    validate_subject_scope, validate_update_shape,
};
use super::window::{view, view_page};
use crate::domain::admission::{Admission, AdmissionTarget, Admitted};
use crate::domain::catalog::{ProjectionContractCatalog, check_projection_reference};
use crate::domain::error::{DomainError, ResourceKind};
use crate::domain::pep::{actions, resources};
use crate::domain::ports::contracts::ContractRegistry;
use crate::domain::ports::metric_registry::{Classified, MetricMode, MetricRegistry};
use crate::domain::ports::metrics::{DenialReason, QeMetrics, ValidationSurface};
use crate::domain::tokens;

const LOG_TARGET: &str = "qe.quotas";

/// The lifecycle component, borrowed from the bound service for one call.
// @cpt-dod:cpt-cf-quota-enforcement-dod-quota-crud:p1
pub struct QuotaManagement<'a> {
    admission: &'a Admission,
    catalog: &'a ProjectionContractCatalog,
    storage: &'a dyn QuotaEnforcementStoragePluginV1,
    registry: &'a dyn ContractRegistry,
    metric_registry: &'a dyn MetricRegistry,
    metrics: &'a dyn QeMetrics,
    limits: QuotaLimits,
}

impl<'a> QuotaManagement<'a> {
    /// Assemble the component over the bound dependencies.
    #[must_use]
    pub fn new(
        admission: &'a Admission,
        catalog: &'a ProjectionContractCatalog,
        storage: &'a dyn QuotaEnforcementStoragePluginV1,
        registry: &'a dyn ContractRegistry,
        metric_registry: &'a dyn MetricRegistry,
        metrics: &'a dyn QeMetrics,
        limits: QuotaLimits,
    ) -> Self {
        Self {
            admission,
            catalog,
            storage,
            registry,
            metric_registry,
            metrics,
            limits,
        }
    }

    /// The configured bounds.
    #[must_use]
    pub const fn limits(&self) -> QuotaLimits {
        self.limits
    }

    /// Create a Quota.
    ///
    /// # Errors
    ///
    /// In order: the shape violation, the PDP outcome,
    /// [`DomainError::MetricNotRegistered`] or the registry's availability,
    /// the catalogue membership error, the subject-scope violation, the
    /// metadata violation, then the storage error.
    // @cpt-flow:cpt-cf-quota-enforcement-flow-quota-create:p1
    pub async fn create(
        &self,
        ctx: &SecurityContext,
        request: CreateQuotaRequest,
    ) -> Result<QuotaView, DomainError> {
        // @cpt-begin:cpt-cf-quota-enforcement-flow-quota-create:p1:inst-qcr-request
        // The request names its target `(projection_type, subject_id)` and
        // tenant explicitly; the PDP authorizes that target below.
        // @cpt-begin:cpt-cf-quota-enforcement-flow-quota-create:p1:inst-qcr-validate
        let draft = validate_create_shape(request).map_err(|e| self.shape_rejected(e))?;
        // @cpt-end:cpt-cf-quota-enforcement-flow-quota-create:p1:inst-qcr-validate
        let admitted = self
            .admission
            .admit(
                ctx,
                &resources::QUOTA,
                actions::CREATE,
                AdmissionTarget::tenant(draft.tenant_id),
            )
            .await?;
        // @cpt-end:cpt-cf-quota-enforcement-flow-quota-create:p1:inst-qcr-request

        // @cpt-begin:cpt-cf-quota-enforcement-flow-quota-create:p1:inst-qcr-metric
        let classified = self.describe_metric(&draft.metric).await?;
        // @cpt-end:cpt-cf-quota-enforcement-flow-quota-create:p1:inst-qcr-metric

        // @cpt-begin:cpt-cf-quota-enforcement-flow-quota-create:p1:inst-qcr-membership
        // The registry snapshot tells an unregistered projection from a
        // registered one outside the catalogue; the catalogue alone cannot.
        let snapshot = self
            .registry
            .type_schema(&draft.subject.projection_type)
            .await?;
        check_projection_reference(
            self.catalog,
            self.metrics,
            ValidationSurface::Arbitration,
            snapshot.as_ref(),
            &draft.subject.projection_type,
            Some(&draft.metric),
        )?;
        // @cpt-end:cpt-cf-quota-enforcement-flow-quota-create:p1:inst-qcr-membership

        // @cpt-begin:cpt-cf-quota-enforcement-flow-quota-create:p1:inst-qcr-subject-scope
        let projection = self
            .catalog
            .subject_projection(&draft.subject.projection_type)
            .ok_or_else(|| {
                DomainError::Internal("membership passed for an unconfigured projection".to_owned())
            })?;
        validate_subject_scope(
            &projection.scope,
            draft.tenant_id,
            &draft.subject.subject_id,
        )
        .map_err(|e| self.shape_rejected(e))?;
        // @cpt-end:cpt-cf-quota-enforcement-flow-quota-create:p1:inst-qcr-subject-scope

        // @cpt-begin:cpt-cf-quota-enforcement-flow-quota-create:p1:inst-qcr-metadata
        let request_contract = self
            .catalog
            .request_contract(&draft.metric)
            .ok_or_else(|| {
                DomainError::Internal("admitted metric without a request contract".to_owned())
            })?;
        let constraint_contract = validate_metadata(
            &draft.metadata,
            &request_contract.constraint,
            self.limits.metadata_max_bytes,
            self.metrics,
        )?;
        // @cpt-end:cpt-cf-quota-enforcement-flow-quota-create:p1:inst-qcr-metadata

        // @cpt-begin:cpt-cf-quota-enforcement-flow-quota-create:p1:inst-qcr-outside-tx
        // Everything above ran without a storage call; the transaction below
        // holds its locks for the insert alone.
        // @cpt-end:cpt-cf-quota-enforcement-flow-quota-create:p1:inst-qcr-outside-tx

        // @cpt-begin:cpt-cf-quota-enforcement-flow-quota-create:p1:inst-qcr-persist
        let now = OffsetDateTime::now_utc();
        let tenant_id = draft.tenant_id;
        let subject = draft.subject.clone();
        let events = [quota_changed(
            tenant_id,
            None,
            Some(subject),
            ChangeKind::Created,
            now,
        )];
        let storage_draft = QuotaDraft {
            tenant_id: draft.tenant_id,
            subject: draft.subject,
            metric: draft.metric,
            quota_type: draft.quota_type,
            period: draft.period,
            enforcement_mode: draft.enforcement_mode,
            cap: draft.cap,
            notification_thresholds: draft.notification_thresholds,
            validity_window: draft.validity_window,
            fail_open_hint: draft.fail_open_hint,
            metadata: draft.metadata,
            source: draft.source,
            constraint_contract,
        };
        let id = self
            .storage
            .create_quota(ctx, &admitted.access_scope, storage_draft, &events)
            .await?;
        // @cpt-end:cpt-cf-quota-enforcement-flow-quota-create:p1:inst-qcr-persist

        // @cpt-begin:cpt-cf-quota-enforcement-flow-quota-create:p1:inst-qcr-direct-if
        if classified.descriptor.mode == MetricMode::Direct {
            // @cpt-begin:cpt-cf-quota-enforcement-flow-quota-create:p1:inst-qcr-direct
            // Accepted: a metric's mode can flip. The Quota is inert until then
            // and counted by the `quota_for_direct_metric_total` gauge.
            tracing::info!(
                target: LOG_TARGET,
                quota_id = %id,
                "quota created on a metric currently classified Direct; inert until the mode flips"
            );
            // @cpt-end:cpt-cf-quota-enforcement-flow-quota-create:p1:inst-qcr-direct
        }
        // @cpt-end:cpt-cf-quota-enforcement-flow-quota-create:p1:inst-qcr-direct-if

        // @cpt-begin:cpt-cf-quota-enforcement-flow-quota-create:p1:inst-qcr-return
        let quota = self
            .read_one(ctx, &admitted.access_scope, id)
            .await?
            .ok_or_else(|| DomainError::Internal("created quota is not readable".to_owned()))?;
        Ok(view(
            quota,
            Some(classified.descriptor.kind),
            OffsetDateTime::now_utc(),
        ))
        // @cpt-end:cpt-cf-quota-enforcement-flow-quota-create:p1:inst-qcr-return
    }

    /// Apply a non-breaking patch.
    ///
    /// # Errors
    ///
    /// In order: the shape violation (`rate` first, then the immutable
    /// fields), the PDP outcome, [`DomainError::NotFound`],
    /// [`DomainError::QuotaDeactivated`], the patched-shape violation, the
    /// registry's answer, the metadata violation, then the storage error
    /// ([`DomainError::CapBelowConsumed`] and
    /// [`DomainError::ThresholdsRequireBoundedCap`] decided under the row lock).
    // @cpt-flow:cpt-cf-quota-enforcement-flow-quota-update:p1
    pub async fn update(
        &self,
        ctx: &SecurityContext,
        quota_id: QuotaId,
        request: UpdateQuotaRequest,
    ) -> Result<QuotaView, DomainError> {
        // @cpt-begin:cpt-cf-quota-enforcement-flow-quota-update:p1:inst-qup-request
        let mut patch = validate_update_shape(request).map_err(|e| self.shape_rejected(e))?;
        let admitted = self.admit_resource(ctx, actions::UPDATE, quota_id).await?;
        // @cpt-end:cpt-cf-quota-enforcement-flow-quota-update:p1:inst-qup-request

        // @cpt-begin:cpt-cf-quota-enforcement-flow-quota-update:p1:inst-qup-validate
        let current = self
            .read_one(ctx, &admitted.access_scope, quota_id)
            .await?
            .ok_or_else(|| not_found(quota_id))?;
        if current.status == QuotaStatus::Deactivated {
            return Err(DomainError::QuotaDeactivated {
                id: quota_id.to_string(),
            });
        }
        validate_patched_shape(&current, &patch).map_err(|e| self.shape_rejected(e))?;
        // Metric identity is revalidated at update time.
        let classified = self.describe_metric(&current.metric).await?;
        // @cpt-end:cpt-cf-quota-enforcement-flow-quota-update:p1:inst-qup-validate

        // @cpt-begin:cpt-cf-quota-enforcement-flow-quota-update:p1:inst-qup-meta-if
        if let Some(metadata) = &patch.metadata {
            // @cpt-begin:cpt-cf-quota-enforcement-flow-quota-update:p1:inst-qup-meta
            let request_contract =
                self.catalog
                    .request_contract(&current.metric)
                    .ok_or_else(|| {
                        self.metrics
                            .record_admitted_metric_violation(ValidationSurface::Arbitration);
                        DomainError::InvalidArgument {
                            field: "metric",
                            reason: tokens::METRIC_NOT_ADMITTED,
                        }
                    })?;
            // The new object is validated against the contract this process's
            // catalogue holds, and the accepted reference travels with it so
            // storage writes both in one row update: a catalogue that moved
            // since creation moves the stored reference too.
            let accepted = validate_metadata(
                metadata,
                &request_contract.constraint,
                self.limits.metadata_max_bytes,
                self.metrics,
            )?;
            patch.constraint_contract = Some(accepted);
            // @cpt-end:cpt-cf-quota-enforcement-flow-quota-update:p1:inst-qup-meta
        }
        // @cpt-end:cpt-cf-quota-enforcement-flow-quota-update:p1:inst-qup-meta-if

        // @cpt-begin:cpt-cf-quota-enforcement-flow-quota-update:p1:inst-qup-persist
        // @cpt-begin:cpt-cf-quota-enforcement-flow-quota-update:p1:inst-qup-event
        let now = OffsetDateTime::now_utc();
        let events = [quota_changed(
            current.tenant_id,
            Some(quota_id),
            Some(current.subject.clone()),
            ChangeKind::Updated,
            now,
        )];
        // @cpt-end:cpt-cf-quota-enforcement-flow-quota-update:p1:inst-qup-event
        // @cpt-begin:cpt-cf-quota-enforcement-flow-quota-update:p1:inst-qup-guard-if
        // The cap-versus-consumed guard (I6) and the thresholds-versus-cap rule
        // (I14) are decided by storage on the merged row under its lock, never
        // on the row read above.
        let updated = self
            .storage
            .update_quota(ctx, &admitted.access_scope, quota_id, patch, &events)
            .await
            // @cpt-begin:cpt-cf-quota-enforcement-flow-quota-update:p1:inst-qup-guard
            .map_err(DomainError::from)?;
        // @cpt-end:cpt-cf-quota-enforcement-flow-quota-update:p1:inst-qup-guard
        // @cpt-end:cpt-cf-quota-enforcement-flow-quota-update:p1:inst-qup-guard-if
        // @cpt-end:cpt-cf-quota-enforcement-flow-quota-update:p1:inst-qup-persist

        // @cpt-begin:cpt-cf-quota-enforcement-flow-quota-update:p1:inst-qup-return
        Ok(view(
            updated,
            Some(classified.descriptor.kind),
            OffsetDateTime::now_utc(),
        ))
        // @cpt-end:cpt-cf-quota-enforcement-flow-quota-update:p1:inst-qup-return
    }

    /// Deactivate a Quota. The record stays readable; storage resolves the
    /// active leases in the same transaction and lists them in the outcome.
    ///
    /// # Errors
    ///
    /// The PDP outcome, [`DomainError::NotFound`],
    /// [`DomainError::QuotaDeactivated`] on a second deactivation, or the
    /// storage error.
    pub async fn deactivate(
        &self,
        ctx: &SecurityContext,
        quota_id: QuotaId,
    ) -> Result<DeactivateOutcome, DomainError> {
        let admitted = self
            .admit_resource(ctx, actions::DEACTIVATE, quota_id)
            .await?;
        let current = self
            .read_one(ctx, &admitted.access_scope, quota_id)
            .await?
            .ok_or_else(|| not_found(quota_id))?;
        let now = OffsetDateTime::now_utc();
        let events = [quota_changed(
            current.tenant_id,
            Some(quota_id),
            Some(current.subject),
            ChangeKind::Deactivated,
            now,
        )];
        Ok(self
            .storage
            .deactivate_quota(ctx, &admitted.access_scope, quota_id, &events)
            .await?)
    }

    /// Read one Quota.
    ///
    /// # Errors
    ///
    /// The PDP outcome, [`DomainError::NotFound`] for an unknown or
    /// out-of-scope id, or the registry's availability on a cold cache.
    // @cpt-flow:cpt-cf-quota-enforcement-flow-quota-read:p1
    pub async fn get(
        &self,
        ctx: &SecurityContext,
        quota_id: QuotaId,
    ) -> Result<QuotaView, DomainError> {
        // @cpt-begin:cpt-cf-quota-enforcement-flow-quota-read:p1:inst-qrd-request
        let admitted = self.admit_resource(ctx, actions::GET, quota_id).await?;
        // @cpt-end:cpt-cf-quota-enforcement-flow-quota-read:p1:inst-qrd-request
        // @cpt-begin:cpt-cf-quota-enforcement-flow-quota-read:p1:inst-qrd-read
        // @cpt-begin:cpt-cf-quota-enforcement-flow-quota-read:p1:inst-qrd-deactivated
        // No status filter: a deactivated Quota is read like any other.
        let quota = self
            .read_one(ctx, &admitted.access_scope, quota_id)
            .await?
            .ok_or_else(|| not_found(quota_id))?;
        // @cpt-end:cpt-cf-quota-enforcement-flow-quota-read:p1:inst-qrd-deactivated
        // @cpt-end:cpt-cf-quota-enforcement-flow-quota-read:p1:inst-qrd-read
        // @cpt-begin:cpt-cf-quota-enforcement-flow-quota-read:p1:inst-qrd-window
        let kinds = self.kinds_of(std::slice::from_ref(&quota.metric)).await?;
        let kind = kinds.get(&quota.metric).copied();
        // @cpt-end:cpt-cf-quota-enforcement-flow-quota-read:p1:inst-qrd-window
        // @cpt-begin:cpt-cf-quota-enforcement-flow-quota-read:p1:inst-qrd-return
        Ok(view(quota, kind, OffsetDateTime::now_utc()))
        // @cpt-end:cpt-cf-quota-enforcement-flow-quota-read:p1:inst-qrd-return
    }

    /// Read a filtered page of Quotas within the caller's scope.
    ///
    /// # Errors
    ///
    /// The list-shape violation, the PDP outcome, the storage error, or the
    /// registry's availability on a cold cache.
    pub async fn list(
        &self,
        ctx: &SecurityContext,
        request: ListQuotasRequest,
    ) -> Result<PageResult<QuotaView>, DomainError> {
        let (filter, page) =
            validate_list(request, &self.limits).map_err(|e| self.shape_rejected(e))?;
        let target = filter
            .tenant_id
            .unwrap_or_else(|| TenantId::new(ctx.subject_tenant_id()));
        let admitted = self
            .admission
            .admit(
                ctx,
                &resources::QUOTA,
                actions::LIST,
                AdmissionTarget::tenant(target),
            )
            .await?;
        // Rows outside the scope are absent, not errors; the cursor carries
        // position only and the scope is re-applied on every page.
        let page = self
            .storage
            .read_quotas(ctx, &admitted.access_scope, filter, page)
            .await?;
        let metrics: Vec<MetricId> = page.items.iter().map(|q| q.metric.clone()).collect();
        let kinds = self.kinds_of(&metrics).await?;
        Ok(view_page(page, &kinds, OffsetDateTime::now_utc()))
    }

    /// The PDP target of an id-addressed operation: the caller's home tenant
    /// and the Quota id. The returned scope decides whether a Quota of another
    /// tenant is visible at all.
    async fn admit_resource(
        &self,
        ctx: &SecurityContext,
        action: &str,
        quota_id: QuotaId,
    ) -> Result<Admitted, DomainError> {
        self.admission
            .admit(
                ctx,
                &resources::QUOTA,
                action,
                AdmissionTarget::resource(
                    TenantId::new(ctx.subject_tenant_id()),
                    quota_id.as_uuid(),
                ),
            )
            .await
    }

    /// The metric must be a registered, classified instance of the metric
    /// base; a stale classification is good enough to admit a write.
    async fn describe_metric(&self, metric: &MetricId) -> Result<Classified, DomainError> {
        self.metric_registry.describe(metric).await?.ok_or_else(|| {
            DomainError::MetricNotRegistered {
                metric: metric.as_str().to_owned(),
            }
        })
    }

    /// The kinds of the distinct metrics of a read. A metric the registry no
    /// longer knows is logged and left out; the Quota stays readable.
    async fn kinds_of(
        &self,
        metrics: &[MetricId],
    ) -> Result<HashMap<MetricId, MetricKind>, DomainError> {
        let mut kinds = HashMap::new();
        for metric in metrics {
            if kinds.contains_key(metric) {
                continue;
            }
            if let Some(classified) = self.metric_registry.describe(metric).await? {
                kinds.insert(metric.clone(), classified.descriptor.kind);
            } else {
                tracing::warn!(
                    target: LOG_TARGET,
                    metric = %metric,
                    "quota references a metric the types registry no longer knows"
                );
            }
        }
        Ok(kinds)
    }

    async fn read_one(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        quota_id: QuotaId,
    ) -> Result<Option<Quota>, DomainError> {
        let filter = QuotaFilter {
            ids: vec![quota_id],
            ..QuotaFilter::default()
        };
        let page = self
            .storage
            .read_quotas(ctx, scope, filter, PageRequest::first(1))
            .await?;
        Ok(page.items.into_iter().next())
    }

    /// A request rejected before the PDP is an admission denial by invalid
    /// argument; a reserved capability is not.
    fn shape_rejected(&self, err: DomainError) -> DomainError {
        if !matches!(err, DomainError::NotYetImplemented { .. }) {
            self.metrics.record_denial(DenialReason::InvalidArgument);
        }
        err
    }
}

fn not_found(quota_id: QuotaId) -> DomainError {
    DomainError::NotFound {
        kind: ResourceKind::Quota,
        id: quota_id.to_string(),
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "service_tests.rs"]
mod service_tests;
