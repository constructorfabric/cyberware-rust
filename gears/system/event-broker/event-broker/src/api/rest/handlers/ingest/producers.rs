//! `POST /v1/producers`, `GET .../cursors`, `POST .../{id}:reset`
//! (`DESIGN.md:585`).

use axum::Extension;
use axum::body::Bytes;
use axum::extract::Path;
use axum::http::Uri;
use toolkit::api::canonical_prelude::*;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::dto::{
    ProducerCursorsResponse, RegisterProducerRequest, RegisterProducerResponse,
    ResetProducerRequest,
};
use crate::api::rest::handlers::action_suffix::parse_action_suffixed_id;
use crate::api::rest::state::HandlerState;
use crate::domain::error::DomainError;
use crate::domain::ingest::ProducerRegistrationInput;

/// # Errors
/// Returns the mapped `CanonicalError` for any `DomainError`
/// `IngestService::register_producer` produces.
pub async fn register_producer(
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<HandlerState>,
    uri: Uri,
    Json(req): Json<RegisterProducerRequest>,
) -> ApiResult<impl IntoResponse> {
    // Validated before the registration is written: this `client_agent` is the
    // one that persists, into a column documented as printable ASCII within
    // 256 bytes.
    event_broker_sdk::validate::client_agent(&req.client_agent).map_err(DomainError::from)?;

    let registration = state
        .ingest
        .register_producer(
            &ctx,
            ProducerRegistrationInput {
                mode: req.mode.into(),
                client_agent: req.client_agent,
            },
        )
        .await?;
    Ok(created_json(
        RegisterProducerResponse {
            id: registration.id,
            mode: registration.mode.into(),
            client_agent: registration.client_agent,
        },
        // `uri` is this request's own collection-level path (`/v1/producers`,
        // no id) - `created_json` itself joins `new_id` onto it to build the
        // `Location` header, so it must NOT already contain the new id.
        &uri,
        &registration.id.to_string(),
    ))
}

/// # Errors
/// Returns the mapped `CanonicalError` for any `DomainError`
/// `IngestService::get_producer_cursors` produces (not found, forbidden).
pub async fn get_producer_cursors(
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<HandlerState>,
    Path(id): Path<Uuid>,
) -> ApiResult<JsonBody<ProducerCursorsResponse>> {
    let cursors = state.ingest.get_producer_cursors(&ctx, id).await?;
    Ok(Json(cursors.into()))
}

/// The `:reset` suffix can't be a normal axum route template - a bare
/// `{id}` is registered instead (matchit can't mix a path param with
/// literal text in one segment), so this handler receives the whole
/// `<uuid>:reset` segment and splits it itself via
/// `action_suffix::parse_action_suffixed_id` (shared with
/// `subscriptions::seek_subscription`). Same mechanism `producers.rs`'s
/// module doc comment already described for the dispatcher's forwarding
/// registration (`eb-dispatcher-routing`) - not a new workaround.
///
/// # Errors
/// Returns the mapped `CanonicalError` for any `DomainError`
/// `IngestService::reset_producer` produces (not found, forbidden), or a
/// `400` if the path segment isn't `<uuid>:reset` or the body isn't valid
/// JSON.
pub async fn reset_producer(
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<HandlerState>,
    Path(raw_id): Path<String>,
    body: Bytes,
) -> ApiResult<StatusCode> {
    let producer_id = parse_action_suffixed_id(&raw_id, "reset", "producer")?;

    let req: ResetProducerRequest = if body.is_empty() {
        ResetProducerRequest::default()
    } else {
        serde_json::from_slice(&body).map_err(|err| DomainError::Validation {
            code: "InvalidBody",
            message: format!("invalid JSON body: {err}"),
        })?
    };

    state
        .ingest
        .reset_producer(&ctx, producer_id, req.into())
        .await?;
    Ok(StatusCode::OK)
}
