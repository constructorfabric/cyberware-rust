//! CRUD for consumer groups (`DESIGN.md:590`).

use axum::Extension;
use axum::body::Bytes;
use axum::extract::Path;
use axum::http::Uri;
use toolkit::api::canonical_prelude::*;
use toolkit_gts::GtsInstanceId;
use toolkit_odata::ast::Value as ODataValue;
use toolkit_security::SecurityContext;

use super::dto::{ConsumerGroupDto, CreateConsumerGroupRequest};
use crate::api::rest::pagination::{eval_filter, paginate_by_key};
use crate::api::rest::state::HandlerState;
use crate::domain::error::DomainError;
use crate::domain::model::{ConsumerGroupCreateInput, ConsumerGroupKind};

/// # Errors
/// Returns the mapped `CanonicalError` for any `DomainError`
/// `DeliveryService::create_consumer_group` produces, or a `400` if a
/// non-empty body isn't valid JSON matching `CreateConsumerGroupRequest`.
pub async fn create_consumer_group(
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<HandlerState>,
    uri: Uri,
    body: Bytes,
) -> ApiResult<impl IntoResponse> {
    // [question]: don't we have named groups? How does the user specify named group? check in DESIGN.md; report

    let input: ConsumerGroupCreateInput = if body.is_empty() {
        ConsumerGroupCreateInput::default()
    } else {
        serde_json::from_slice::<CreateConsumerGroupRequest>(&body)
            .map_err(|err| DomainError::Validation {
                code: "InvalidBody",
                message: format!("invalid JSON body: {err}"),
            })?
            .into()
    };

    if let Some(client_agent) = input.client_agent.as_deref() {
        event_broker_sdk::validate::client_agent(client_agent).map_err(DomainError::from)?;
    }
    if let Some(description) = input.description.as_deref() {
        event_broker_sdk::validate::description(description).map_err(DomainError::from)?;
    }

    let group = state.delivery.create_consumer_group(&ctx, input).await?;
    let id = group.id.clone();
    Ok(created_json(
        ConsumerGroupDto::from(group),
        &uri,
        id.as_ref(),
    ))
}

/// # Errors
/// Returns `400` for an invalid `$filter` expression or pagination cursor.
pub async fn list_consumer_groups(
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<HandlerState>,
    OData(query): OData,
) -> ApiResult<JsonPage<ConsumerGroupDto>> {
    // Fetch-all-then-paginate-in-memory: see
    // `ConsumerGroupRepo::list_consumer_groups`'s `#[temporary]` doc
    // (`gears-rust#4347`).
    // [todo]: do we have kind filter stated in our openapi.yaml or DESIGN.md? report;
    let mut groups = state.delivery.list_consumer_groups(&ctx).await?;
    if let Some(filter) = query.filter() {
        groups.retain(|g| {
            eval_filter(filter, &|field| match field {
                "id" => Some(ODataValue::String(g.id.to_string())),
                "kind" => Some(ODataValue::String(
                    match g.kind {
                        ConsumerGroupKind::Anonymous => "anonymous",
                        ConsumerGroupKind::Named => "named",
                    }
                    .to_owned(),
                )),
                _ => None,
            })
        });
    }
    let page = paginate_by_key(groups, &query, "id", |g| g.id.to_string())?;
    Ok(Json(page.map_items(ConsumerGroupDto::from)))
}

/// # Errors
/// Returns the mapped `CanonicalError` for `DomainError::NotFound` if the
/// group isn't registered.
pub async fn get_consumer_group(
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<HandlerState>,
    Path(id): Path<String>,
) -> ApiResult<JsonBody<ConsumerGroupDto>> {
    let id = GtsInstanceId::try_new(&id).map_err(|err| DomainError::Validation {
        code: "InvalidPath",
        message: format!("'{id}' is not a valid GTS instance id: {err}"),
    })?;
    let group = state.delivery.get_consumer_group(&ctx, &id).await?;
    Ok(Json(group.into()))
}

/// # Errors
/// Returns the mapped `CanonicalError` for `DomainError::NotFound` if the
/// group isn't registered, or `DomainError::Conflict` if it still has
/// active members.
pub async fn delete_consumer_group(
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<HandlerState>,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    let id = GtsInstanceId::try_new(&id).map_err(|err| DomainError::Validation {
        code: "InvalidPath",
        message: format!("'{id}' is not a valid GTS instance id: {err}"),
    })?;
    state.delivery.delete_consumer_group(&ctx, &id).await?;
    Ok(StatusCode::NO_CONTENT)
}
