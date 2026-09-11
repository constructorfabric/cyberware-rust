//! Turning a `Frame` into bytes on each transport.
//!
//! Extracted from the handlers so the wire format is testable without an HTTP
//! round trip and without an open stream. Both transports carry the *same*
//! `FrameDto` body; they differ only in how a frame is delimited - a multipart
//! part with its own `Content-Type`, or an SSE event whose `event:` name is the
//! frame's own `kind` discriminant.

use axum::response::sse::Event as SseEvent;
use bytes::Bytes;

use crate::domain::delivery::Frame;

use super::dto::FrameDto;

/// The SSE event name for a frame, which is its `kind` discriminant.
///
/// Shared by both transports even though only SSE puts it on the wire: it is
/// the one place the mapping from frame variant to wire name lives, so the two
/// cannot drift.
#[must_use]
pub(crate) fn frame_kind(dto: &FrameDto) -> &'static str {
    match dto {
        FrameDto::Event { .. } => "event",
        FrameDto::Heartbeat { .. } => "heartbeat",
        FrameDto::Topology { .. } => "topology",
        FrameDto::Control { .. } => "control",
    }
}

/// One `multipart/mixed` part: the boundary, a JSON content type, the body, and
/// the trailing CRLF that separates it from the next part.
///
/// A frame that somehow fails to serialize yields an empty body rather than
/// breaking the multipart framing - a malformed part is recoverable for a
/// consumer, a truncated stream is not.
#[must_use]
pub fn to_multipart_part(boundary: &str, frame: Frame) -> Bytes {
    let dto = FrameDto::from(frame);
    let json = serde_json::to_vec(&dto).unwrap_or_default();
    Bytes::from(
        [
            format!("--{boundary}\r\nContent-Type: application/json\r\n\r\n").into_bytes(),
            json,
            b"\r\n".to_vec(),
        ]
        .concat(),
    )
}

/// One SSE event, named by the frame's kind.
///
/// Falls back to an empty `heartbeat` if serialization fails, which is
/// `json_data`'s only failure mode: a consumer that receives a heartbeat it did
/// not expect loses nothing, where a dropped event would look like a gap.
#[must_use]
pub fn to_sse_event(frame: Frame) -> SseEvent {
    let dto = FrameDto::from(frame);
    let kind = frame_kind(&dto);
    SseEvent::default()
        .event(kind)
        .json_data(dto)
        .unwrap_or_else(|_| SseEvent::default().event("heartbeat").data("{}"))
}
