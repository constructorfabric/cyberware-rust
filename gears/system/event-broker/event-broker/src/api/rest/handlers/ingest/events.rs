//! `POST /v1/events`, `POST /v1/events:batch` (`DESIGN.md:584`,
//! `docs/schemas/gts.cf.core.events.event.v1~.schema.json`).

use axum::Extension;
use axum::extract::Query;
use toolkit::api::canonical_prelude::*;
use toolkit_security::SecurityContext;

use super::dto::{PublishBatchRequest, PublishEventRequest, SyncWaitQuery};
use crate::api::rest::state::HandlerState;
use crate::domain::ingest::PublishRequest;

// [todo]: I guess this is something that I missed, WTF is `Sync-Wait` when we have HTTP Prefer? let's plan the design update here, we don't want to invent the wheel
/// `true` if the caller opted into synchronous backend persistence via
/// either the `Sync-Wait: true` header or `?wait=persisted`
/// (`docs/openapi.yaml`'s `POST /v1/events`).
fn wants_sync_wait(headers: &axum::http::HeaderMap, query: &SyncWaitQuery) -> bool {
    let header_sync = headers
        .get("Sync-Wait")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("true"));
    let query_sync = query.wait.as_deref() == Some("persisted");
    header_sync || query_sync
}

/// # Errors
/// Returns the mapped `CanonicalError` for any `DomainError`
/// `IngestService::publish_event` produces (topic/event-type not found,
/// schema validation, sequence violation).
pub async fn publish_event(
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<HandlerState>,
    headers: axum::http::HeaderMap,
    Query(query): Query<SyncWaitQuery>,
    Json(req): Json<PublishEventRequest>,
) -> ApiResult<StatusCode> {
    state.ingest.publish_event(&ctx, req.try_into()?).await?;
    Ok(if wants_sync_wait(&headers, &query) {
        StatusCode::CREATED
    } else {
        StatusCode::ACCEPTED
    })
}

/// `docs/openapi.yaml` documents no response body for this endpoint (202/
/// 400/403/412/413, status only) - `BatchResult`'s per-event
/// accepted/failed detail isn't surfaced on success, matching that
/// documented contract exactly rather than inventing an undocumented body
/// shape.
///
/// # Errors
/// Returns the mapped `CanonicalError` for any `DomainError`
/// `IngestService::publish_batch` produces (mixed topics, batch too large,
/// or a mid-batch sequence violation).
pub async fn publish_batch(
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<HandlerState>,
    Json(req): Json<PublishBatchRequest>,
) -> ApiResult<StatusCode> {
    let requests = req
        .events
        .into_iter()
        .map(PublishRequest::try_from)
        .collect::<Result<Vec<_>, _>>()?;
    state.ingest.publish_batch(&ctx, requests).await?;
    Ok(StatusCode::ACCEPTED)
}
