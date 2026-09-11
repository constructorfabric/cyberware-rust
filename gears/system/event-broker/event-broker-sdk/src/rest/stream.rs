//! Delivery-stream decoding for both wire framings.
//!
//! `multipart/mixed` (cloud) and `text/event-stream` (browser) carry the same
//! `FrameWire` bodies; only the delimiting differs. Each framing has a decoder
//! that is fed raw bytes incrementally and drains complete frames, so the
//! `WireFrame` sequence a caller observes is identical either way. The open
//! function reconnects on a dropped connection until a terminal control frame
//! arrives or the retry budget is spent.

use std::time::Duration;

use http_body_util::BodyExt;

use toolkit_security::SecurityContext;

use crate::api::{ControlCode, FrameStream, WireFrame};
use crate::error::EventBrokerError;
use crate::ids::SubscriptionId;
use crate::rest::client::{RestBroker, StreamTransport};
use crate::rest::wire::FrameWire;

const MAX_RECONNECTS: u32 = 5;
const RECONNECT_BACKOFF: Duration = Duration::from_millis(250);

/// Incremental decoder for one framing. Bytes arrive in arbitrary chunks; each
/// `drain` returns the frames that have become complete since the last call.
enum Decoder {
    /// SSE: events are separated by a blank line; the frame body is the
    /// concatenation of the event's `data:` lines. The `event:` name is
    /// ignored - the frame kind is the JSON body's own `kind` tag.
    Sse { buf: Vec<u8> },
    /// multipart/mixed: parts are separated by `--<boundary>`; the frame body is
    /// the bytes after the part's header block (a blank line).
    Multipart { boundary: String, buf: Vec<u8> },
}

impl Decoder {
    fn sse() -> Self {
        Decoder::Sse { buf: Vec::new() }
    }

    fn multipart(boundary: String) -> Self {
        Decoder::Multipart {
            boundary,
            buf: Vec::new(),
        }
    }

    fn push(&mut self, chunk: &[u8]) {
        match self {
            Decoder::Sse { buf } | Decoder::Multipart { buf, .. } => buf.extend_from_slice(chunk),
        }
    }

    /// Pulls every frame that is now complete, leaving any partial tail buffered.
    fn drain(&mut self) -> Result<Vec<FrameWire>, EventBrokerError> {
        match self {
            Decoder::Sse { buf } => drain_sse(buf),
            Decoder::Multipart { boundary, buf } => drain_multipart(boundary, buf),
        }
    }
}

fn parse_frame(body: &[u8]) -> Result<FrameWire, EventBrokerError> {
    serde_json::from_slice::<FrameWire>(body)
        .map_err(|err| EventBrokerError::Transport(format!("undecodable stream frame: {err}")))
}

fn drain_sse(buf: &mut Vec<u8>) -> Result<Vec<FrameWire>, EventBrokerError> {
    let text = String::from_utf8(std::mem::take(buf)).map_err(|err| {
        EventBrokerError::Transport(format!("stream carried non-UTF-8 SSE bytes: {err}"))
    })?;
    let mut frames = Vec::new();
    let mut rest = text.as_str();
    // Events are terminated by a blank line. Keep the trailing partial event
    // (if any) buffered for the next chunk.
    while let Some(end) = rest.find("\n\n") {
        let (event, tail) = rest.split_at(end);
        rest = &tail[2..];
        let mut data = String::new();
        for line in event.lines() {
            if let Some(payload) = line.strip_prefix("data:") {
                data.push_str(payload.trim_start());
            }
            // `event:`, `id:` and comment (`:`) lines carry no frame body.
        }
        if !data.is_empty() {
            frames.push(parse_frame(data.as_bytes())?);
        }
    }
    buf.extend_from_slice(rest.as_bytes());
    Ok(frames)
}

fn drain_multipart(boundary: &str, buf: &mut Vec<u8>) -> Result<Vec<FrameWire>, EventBrokerError> {
    let delimiter = format!("--{boundary}");
    let text = String::from_utf8(std::mem::take(buf)).map_err(|err| {
        EventBrokerError::Transport(format!("stream carried non-UTF-8 multipart bytes: {err}"))
    })?;
    let mut frames = Vec::new();
    let mut remainder = String::new();
    // A part is only certainly complete once the next boundary arrives, so a
    // part's body is emitted as soon as it parses as whole JSON; a part whose
    // JSON is not yet complete is re-buffered for the next chunk. The segment
    // before the first boundary is the (usually empty) preamble.
    for (i, part) in text.split(delimiter.as_str()).enumerate() {
        if i == 0 {
            continue;
        }
        let trimmed = part.trim_start_matches(['\r', '\n']);
        // The closing marker is `--` right after the last boundary.
        if trimmed.starts_with("--") {
            continue;
        }
        let body = match trimmed.find("\r\n\r\n").or_else(|| trimmed.find("\n\n")) {
            Some(idx) => trimmed[idx..].trim_start_matches(['\r', '\n']).trim_end(),
            None => "",
        };
        if body.is_empty() {
            remainder = format!("{delimiter}{part}");
            continue;
        }
        match parse_frame(body.as_bytes()) {
            Ok(frame) => frames.push(frame),
            // Incomplete JSON tail: hold the whole segment for the next chunk.
            Err(_) => remainder = format!("{delimiter}{part}"),
        }
    }
    buf.extend_from_slice(remainder.as_bytes());
    Ok(frames)
}

/// Extracts the `boundary=` token from a `multipart/mixed` content type.
fn boundary_of(content_type: &str) -> Option<String> {
    content_type
        .split(';')
        .filter_map(|p| p.trim().strip_prefix("boundary="))
        .map(|b| b.trim_matches('"').to_owned())
        .next()
}

/// Opens the delivery stream for `id` and returns a frame stream. Reconnects on
/// a dropped connection until a terminal control frame or the retry budget.
pub(crate) async fn open(
    client: &RestBroker,
    ctx: &SecurityContext,
    id: SubscriptionId,
) -> Result<FrameStream, EventBrokerError> {
    // Clone what the owned stream needs; the returned stream outlives the call.
    let http = client.http().clone();
    let framing = client.framing();
    let ctx = ctx.clone();
    let (path, accept) = match framing {
        StreamTransport::Multipart => ("/event-broker/v1/events:stream", "multipart/mixed"),
        StreamTransport::Sse => ("/event-broker/v1/events:sse", "text/event-stream"),
    };
    let url = client.url(&format!("{path}?subscription_id={id}"));

    let stream = async_stream::try_stream! {
        let mut reconnects = 0u32;
        loop {
            let rb = match ctx.bearer_token() {
                Some(token) => http.get(&url).bearer_auth(secrecy::ExposeSecret::expose_secret(token)),
                None => http.get(&url),
            }
            .header("Accept", accept);

            let resp = rb.send().await.map_err(|e| EventBrokerError::Transport(e.to_string()))?;
            let resp = if resp.status().is_success() {
                resp
            } else {
                let err = crate::rest::client::error_from_response(resp).await;
                Err(err)?;
                unreachable!("Err(..)? returns from the stream above");
            };

            let mut decoder = match framing {
                StreamTransport::Sse => Decoder::sse(),
                StreamTransport::Multipart => {
                    let boundary = resp
                        .headers()
                        .get("content-type")
                        .and_then(|v| v.to_str().ok())
                        .and_then(boundary_of)
                        .ok_or_else(|| EventBrokerError::Transport(
                            "multipart stream response carried no boundary".to_owned(),
                        ))?;
                    Decoder::multipart(boundary)
                }
            };

            let mut body = resp.into_body();
            let mut terminated = false;
            while let Some(frame) = body.frame().await {
                let frame = frame.map_err(|e| EventBrokerError::Transport(e.to_string()))?;
                if let Some(data) = frame.data_ref() {
                    decoder.push(data);
                    for wire_frame in decoder.drain()? {
                        let wf = wire_frame.into_frame()?;
                        let is_terminal = matches!(
                            &wf,
                            WireFrame::Control { code: ControlCode::Terminal, .. }
                        );
                        yield wf;
                        if is_terminal {
                            terminated = true;
                            break;
                        }
                    }
                }
                if terminated {
                    break;
                }
            }

            if terminated {
                break;
            }
            // The connection dropped without a terminal frame; reconnect. The
            // broker resumes from the group's committed cursor, so no re-seek is
            // needed here.
            reconnects += 1;
            if reconnects > MAX_RECONNECTS {
                break;
            }
            tokio::time::sleep(RECONNECT_BACKOFF).await;
        }
    };
    Ok(Box::pin(stream))
}

#[cfg(test)]
mod tests;
