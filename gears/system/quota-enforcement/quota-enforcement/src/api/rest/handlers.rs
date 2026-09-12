//! Handlers of the Quota lifecycle endpoints. Each one converts the wire shape,
//! borrows the lifecycle component from the service, and lets the domain order
//! the steps; the domain error lifts to a `Problem`.

use std::sync::Arc;

use axum::Extension;
use axum::http::Uri;
use axum::response::IntoResponse;
use toolkit::api::response::created_json;
use toolkit::api::rest::extract::{Json as JsonBody, Path};
use toolkit_contract::query::QueryParamsExtractor;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::dto::{
    CreateQuotaDto, DeactivateOutcomeDto, ListQuotasQuery, QuotaPageDto, QuotaViewDto,
    UpdateQuotaDto,
};
use super::error::ApiResult;
use crate::domain::Service;
use crate::domain::quotas::{CreateQuotaRequest, ListQuotasRequest, UpdateQuotaRequest};
use quota_enforcement_sdk::QuotaId;

/// `POST /v1/quota-enforcement/quotas`: 201 with the view and a `Location`.
///
/// # Errors
///
/// The domain error of the failed step, as a `Problem`.
// @cpt-flow:cpt-cf-quota-enforcement-flow-quota-create:p1
pub async fn create_quota(
    uri: Uri,
    Extension(ctx): Extension<SecurityContext>,
    Extension(service): Extension<Arc<Service>>,
    JsonBody(body): JsonBody<CreateQuotaDto>,
) -> ApiResult<impl IntoResponse> {
    // @cpt-begin:cpt-cf-quota-enforcement-flow-quota-create:p1:inst-qcr-request
    let quotas = service.quotas()?;
    let request = CreateQuotaRequest::try_from(body)?;
    let view = quotas.create(&ctx, request).await?;
    // @cpt-end:cpt-cf-quota-enforcement-flow-quota-create:p1:inst-qcr-request
    // @cpt-begin:cpt-cf-quota-enforcement-flow-quota-create:p1:inst-qcr-return
    let id = view.quota.id.to_string();
    Ok(created_json(QuotaViewDto::from(view), &uri, &id).into_response())
    // @cpt-end:cpt-cf-quota-enforcement-flow-quota-create:p1:inst-qcr-return
}

/// `GET /v1/quota-enforcement/quotas/{id}`.
///
/// # Errors
///
/// The domain error of the failed step, as a `Problem`.
// @cpt-flow:cpt-cf-quota-enforcement-flow-quota-read:p1
pub async fn get_quota(
    Extension(ctx): Extension<SecurityContext>,
    Extension(service): Extension<Arc<Service>>,
    Path(id): Path<Uuid>,
) -> ApiResult<axum::Json<QuotaViewDto>> {
    // @cpt-begin:cpt-cf-quota-enforcement-flow-quota-read:p1:inst-qrd-request
    let quotas = service.quotas()?;
    let view = quotas.get(&ctx, QuotaId::new(id)).await?;
    // @cpt-end:cpt-cf-quota-enforcement-flow-quota-read:p1:inst-qrd-request
    // @cpt-begin:cpt-cf-quota-enforcement-flow-quota-read:p1:inst-qrd-return
    Ok(axum::Json(QuotaViewDto::from(view)))
    // @cpt-end:cpt-cf-quota-enforcement-flow-quota-read:p1:inst-qrd-return
}

/// `GET /v1/quota-enforcement/quotas`.
///
/// # Errors
///
/// The domain error of the failed step, as a `Problem`.
pub async fn list_quotas(
    Extension(ctx): Extension<SecurityContext>,
    Extension(service): Extension<Arc<Service>>,
    QueryParamsExtractor(query): QueryParamsExtractor<ListQuotasQuery>,
) -> ApiResult<axum::Json<QuotaPageDto>> {
    let quotas = service.quotas()?;
    let request = ListQuotasRequest::try_from(query)?;
    let page = quotas.list(&ctx, request).await?;
    Ok(axum::Json(QuotaPageDto::from(page)))
}

/// `PATCH /v1/quota-enforcement/quotas/{id}`.
///
/// # Errors
///
/// The domain error of the failed step, as a `Problem`.
// @cpt-flow:cpt-cf-quota-enforcement-flow-quota-update:p1
pub async fn update_quota(
    Extension(ctx): Extension<SecurityContext>,
    Extension(service): Extension<Arc<Service>>,
    Path(id): Path<Uuid>,
    JsonBody(body): JsonBody<UpdateQuotaDto>,
) -> ApiResult<axum::Json<QuotaViewDto>> {
    // @cpt-begin:cpt-cf-quota-enforcement-flow-quota-update:p1:inst-qup-request
    let quotas = service.quotas()?;
    let view = quotas
        .update(&ctx, QuotaId::new(id), UpdateQuotaRequest::from(body))
        .await?;
    // @cpt-end:cpt-cf-quota-enforcement-flow-quota-update:p1:inst-qup-request
    // @cpt-begin:cpt-cf-quota-enforcement-flow-quota-update:p1:inst-qup-return
    Ok(axum::Json(QuotaViewDto::from(view)))
    // @cpt-end:cpt-cf-quota-enforcement-flow-quota-update:p1:inst-qup-return
}

/// `POST /v1/quota-enforcement/quotas/{id}/deactivate`: 200 with the outcome.
///
/// # Errors
///
/// The domain error of the failed step, as a `Problem`.
pub async fn deactivate_quota(
    Extension(ctx): Extension<SecurityContext>,
    Extension(service): Extension<Arc<Service>>,
    Path(id): Path<Uuid>,
) -> ApiResult<axum::Json<DeactivateOutcomeDto>> {
    let quotas = service.quotas()?;
    let outcome = quotas.deactivate(&ctx, QuotaId::new(id)).await?;
    Ok(axum::Json(DeactivateOutcomeDto::from(outcome)))
}
