//! The in-process `QuotaManagerClientV1`: the SDK client trait over the domain
//! service, registered in `ClientHub` at `init`.
//!
//! It enters the domain exactly where REST does (`Service::quotas`), so both
//! transports share one admission boundary. Before bootstrap binds the
//! dependencies every call fails `NotReady`, the same 503 REST returns.

use std::sync::Arc;

use async_trait::async_trait;
use quota_enforcement_sdk::{
    DeactivateOutcome, PageRequest, PageResult, QuotaEnforcementError, QuotaFilter, QuotaId,
    QuotaManagerClientV1, QuotaPatch, QuotaSpec, QuotaView,
};
use toolkit_security::SecurityContext;

use crate::domain::Service;
use crate::domain::quotas::{CreateQuotaRequest, ListQuotasRequest, UpdateQuotaRequest};

/// The gear's own implementation of the manager client.
pub struct InProcessQuotaManager {
    service: Arc<Service>,
}

impl InProcessQuotaManager {
    /// Wrap the domain service.
    #[must_use]
    pub fn new(service: Arc<Service>) -> Self {
        Self { service }
    }
}

// @cpt-dod:cpt-cf-quota-enforcement-dod-quota-crud:p1
#[async_trait]
impl QuotaManagerClientV1 for InProcessQuotaManager {
    async fn create_quota(
        &self,
        ctx: &SecurityContext,
        spec: QuotaSpec,
    ) -> Result<QuotaId, QuotaEnforcementError> {
        let quotas = self.service.quotas()?;
        let request = CreateQuotaRequest::try_from(spec)?;
        Ok(quotas.create(ctx, request).await?.quota.id)
    }

    async fn update_quota(
        &self,
        ctx: &SecurityContext,
        quota_id: QuotaId,
        patch: QuotaPatch,
    ) -> Result<(), QuotaEnforcementError> {
        let quotas = self.service.quotas()?;
        let request = UpdateQuotaRequest::try_from(patch)?;
        quotas.update(ctx, quota_id, request).await?;
        Ok(())
    }

    async fn deactivate_quota(
        &self,
        ctx: &SecurityContext,
        quota_id: QuotaId,
    ) -> Result<DeactivateOutcome, QuotaEnforcementError> {
        let quotas = self.service.quotas()?;
        Ok(quotas.deactivate(ctx, quota_id).await?)
    }

    async fn read_quotas(
        &self,
        ctx: &SecurityContext,
        filter: QuotaFilter,
        page: PageRequest,
    ) -> Result<PageResult<QuotaView>, QuotaEnforcementError> {
        let quotas = self.service.quotas()?;
        Ok(quotas
            .list(ctx, ListQuotasRequest::from((filter, page)))
            .await?)
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "in_process_tests.rs"]
mod in_process_tests;
