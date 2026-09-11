//! `GET /v1/events:stream`, `GET /v1/events:sse` (`DESIGN.md:589`,
//! `event-broker-consumption-frames`, baseline subset only - see
//! `domain/delivery.rs`'s module doc comment).

use axum::Extension;
use axum::body::{Body, Bytes};
use axum::extract::Query;
use axum::http::HeaderMap;
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use std::convert::Infallible;
use tokio_stream::StreamExt;
use toolkit::api::canonical_prelude::*;
use toolkit_canonical_errors::Http;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::dto::StreamQuery;
use super::encode;
use crate::api::rest::error::EventBrokerResourceError;
use crate::api::rest::state::HandlerState;

fn check_accept_multipart(headers: &HeaderMap) -> Result<(), CanonicalError> {
    let accept = headers
        .get(axum::http::header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("*/*");
    if accept.contains("*/*") || accept.contains("multipart/mixed") {
        return Ok(());
    }
    let detail = if accept.contains("text/event-stream") {
        "SSE is served only at /v1/events:sse; this endpoint serves multipart/mixed"
    } else {
        "this endpoint serves multipart/mixed only"
    };
    Err(EventBrokerResourceError::invalid_argument()
        .with_format(detail)
        .with_override(Http::status_code(406))
        .create())
}

// Symmetric guard for /v1/events:sse — rejects anything that is not
// text/event-stream or */*.
fn check_accept_sse(headers: &HeaderMap) -> Result<(), CanonicalError> {
    let accept = headers
        .get(axum::http::header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("*/*");
    if accept.contains("*/*") || accept.contains("text/event-stream") {
        return Ok(());
    }
    let detail = if accept.contains("multipart/mixed") {
        "multipart/mixed is served only at /v1/events:stream; this endpoint serves text/event-stream"
    } else {
        "this endpoint serves text/event-stream only"
    };
    Err(EventBrokerResourceError::invalid_argument()
        .with_format(detail)
        .with_override(Http::status_code(406))
        .create())
}

/// Maps one domain `Frame` to a multipart/mixed part (`stream_events`'s wire
/// shape) - a `--boundary`/`Content-Type`/JSON-body/`\r\n` part, matching
/// `event-broker-consumption-frames`.
// `Body::from_stream` requires `Stream<Item = Result<_, E>>` - the `Result`
// wrapper is a call-site contract, not a spurious `Ok`.
#[allow(clippy::unnecessary_wraps)]
/// # Errors
/// Returns `406 Not Acceptable` when the `Accept` header excludes
/// `multipart/mixed` (scenarios 1.07, 1.08). Returns the mapped
/// `CanonicalError` for any `DomainError` `DeliveryService::stream`
/// produces (subscription not found, `PositionsNotSet`, `StreamingInProgress`).
pub async fn stream_events(
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<HandlerState>,
    headers: HeaderMap,
    Query(query): Query<StreamQuery>,
) -> ApiResult<impl IntoResponse> {
    check_accept_multipart(&headers)?;
    let boundary = format!("eb-frame-{}", Uuid::new_v4().simple());
    let content_type = format!("multipart/mixed; boundary={boundary}");
    let frames = state.delivery.stream(&ctx, query.subscription_id).await?;
    let stream = frames
        .map(move |frame| encode::to_multipart_part(&boundary, frame))
        .map(Ok::<Bytes, Infallible>);
    Ok((
        [(axum::http::header::CONTENT_TYPE, content_type)],
        Body::from_stream(stream),
    ))
}

/// # Errors
/// Returns `406 Not Acceptable` when the `Accept` header excludes
/// `text/event-stream`. Returns the mapped `CanonicalError` for any
/// `DomainError` `DeliveryService::stream` produces (subscription not found,
/// `PositionsNotSet`, `StreamingInProgress`).
pub async fn sse_events(
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<HandlerState>,
    headers: HeaderMap,
    Query(query): Query<StreamQuery>,
) -> ApiResult<Sse<impl tokio_stream::Stream<Item = Result<SseEvent, Infallible>>>> {
    check_accept_sse(&headers)?;
    let frames = state.delivery.stream(&ctx, query.subscription_id).await?;
    let stream = frames.map(|frame| Ok::<SseEvent, Infallible>(encode::to_sse_event(frame)));
    // Heartbeats are already real domain frames on the documented cadence -
    // no need for axum's own comment-based keepalive on top.
    Ok(Sse::new(stream).keep_alive(KeepAlive::default().text("")))
}
