//! Minimal Pingora-based HTTP/1 forwarding (design.md D7):
//! `pingora_core::connectors::http::v1::Connector`, a pooled outbound
//! client, called directly from an ordinary axum handler - not
//! `pingora-proxy`'s full session-owning engine (the dispatcher has no
//! auth/policy logic of its own to run first, unlike `oagw`). The request
//! body is buffered, bounded via [`crate::config::MAX_REQUEST_BODY_BYTES`]
//! (`http_body_util::Limited`, not `BatchConfig::max_payload_bytes` - see
//! that constant's doc comment); the response body streams back with an
//! idle-timeout on each read (design.md D9), so long-poll/SSE connections
//! stay open as long as bytes - heartbeat or data - keep arriving.

use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::Request;
use axum::http::{HeaderName, StatusCode};
use axum::response::Response;
use http_body_util::{BodyExt, Limited};
use pingora_core::connectors::http::v1::Connector;
use pingora_core::upstreams::peer::HttpPeer;
use pingora_http::RequestHeader;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use crate::config::MAX_REQUEST_BODY_BYTES;

/// The in-flight channel buffer for streamed response chunks.
const RESPONSE_CHANNEL_CAPACITY: usize = 4;

/// Forwarding to `addr` (`host:port`, no scheme) failed - the caller (design
/// wise `forward.rs`) logs the real cause and maps every variant but
/// [`ProxyError::BodyTooLarge`] to the "instance unreachable" `503`
/// (design.md D10, distinct from "no instance registered");
/// `BodyTooLarge` maps to `413` instead.
#[derive(Debug, thiserror::Error)]
pub enum ProxyError {
    /// Peer-address resolution (literal parse or `tokio::net::lookup_host`)
    /// failed for `addr`.
    #[error("failed to resolve forwarding address {addr}: {source}")]
    Resolve {
        addr: String,
        #[source]
        source: std::io::Error,
    },

    /// A Pingora HTTP/1 session operation (session acquisition, request
    /// write, or response read) failed.
    #[error("pingora session error: {0}")]
    Session(#[source] Box<pingora_core::Error>),

    /// The upstream response, or our own outbound request, could not be
    /// represented (header build/append/insert, missing status/response
    /// header, status-code parse, response-builder failure, or a
    /// non-size-related request-body read failure) - not independently
    /// actionable beyond "malformed", per design.md's Decisions.
    #[error("malformed upstream response or request construction failure")]
    MalformedResponse,

    /// The proxied request body exceeded [`MAX_REQUEST_BODY_BYTES`].
    #[error("proxied request body exceeded {limit} bytes")]
    BodyTooLarge { limit: usize },

    /// The resolved instance advertised an unsupported scheme (`https://`)
    /// - rejected rather than silently downgraded to plaintext.
    #[error("endpoint advertises unsupported scheme {scheme:?}")]
    UnsupportedScheme { scheme: String },
}

/// RFC 7230 §6.1 hop-by-hop headers, stripped in both directions - the same
/// set `oagw`'s bridge uses.
fn is_hop_by_hop(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
    )
}

/// Resolves `host_port` (no scheme) to a connectable [`std::net::SocketAddr`].
/// Tries the fast literal `SocketAddr::parse` first (no DNS lookup, no
/// behavior change for IP-literal-advertised instances); on failure, falls
/// back to asynchronous resolution via `tokio::net::lookup_host`, which
/// cannot block the runtime thread or panic on resolution failure (unlike
/// `HttpPeer::new`'s own bare `ToSocketAddrs::to_socket_addrs().unwrap()`).
/// The first resolved address is used. Hostname-advertised instances are not
/// a hypothetical: the real platform Directory service intentionally
/// accepts them (design.md's "Hostname resolution is not a latent edge
/// case").
async fn resolve_peer_addr(host_port: &str) -> Result<std::net::SocketAddr, ProxyError> {
    if let Ok(addr) = host_port.parse() {
        return Ok(addr);
    }

    let mut addrs =
        tokio::net::lookup_host(host_port)
            .await
            .map_err(|source| ProxyError::Resolve {
                addr: host_port.to_owned(),
                source,
            })?;

    addrs.next().ok_or_else(|| ProxyError::Resolve {
        addr: host_port.to_owned(),
        source: std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "hostname resolved to zero addresses",
        ),
    })
}

/// Forwards `req` to `addr` (a resolved `ServiceInstance::address`, e.g.
/// `"http://host:port"` - design.md D5) using the shared, pooled
/// `connector`, and returns the relayed response, streaming the response
/// body back with an idle-timeout on each read: `idle_timeout` resets on
/// every byte received (heartbeat or data), not tied to total connection
/// duration (design.md D9) - the caller (`forward()`) supplies the
/// hardcoded production constant; tests supply a short one so the
/// close-on-idle behavior doesn't require a real wait.
///
/// `addr` is rejected up front with [`ProxyError::UnsupportedScheme`] if it
/// advertises `https://` - real upstream TLS support is out of scope for
/// this change (design.md's Decisions); an `http://`-advertised or
/// bare `host:port` address proceeds exactly as before.
pub async fn proxy(
    addr: &str,
    connector: &Connector,
    req: Request,
    idle_timeout: Duration,
) -> Result<Response, ProxyError> {
    if addr.starts_with("https://") {
        return Err(ProxyError::UnsupportedScheme {
            scheme: "https".to_owned(),
        });
    }
    let host_port = addr.strip_prefix("http://").unwrap_or(addr);

    let peer_addr = resolve_peer_addr(host_port).await?;
    let peer = HttpPeer::new(peer_addr, false, String::new());

    let (parts, body) = req.into_parts();
    let limited_body = Limited::new(body, MAX_REQUEST_BODY_BYTES);
    let body_bytes = limited_body
        .collect()
        .await
        .map_err(|err| {
            if err
                .downcast_ref::<http_body_util::LengthLimitError>()
                .is_some()
            {
                ProxyError::BodyTooLarge {
                    limit: MAX_REQUEST_BODY_BYTES,
                }
            } else {
                ProxyError::MalformedResponse
            }
        })?
        .to_bytes();

    let (mut session, _reused) = connector
        .get_http_session(&peer)
        .await
        .map_err(ProxyError::Session)?;

    let path_and_query = parts.uri.path_and_query().map_or("/", |pq| pq.as_str());
    let mut header = RequestHeader::build(parts.method.as_str(), path_and_query.as_bytes(), None)
        .map_err(|_| ProxyError::MalformedResponse)?;
    for (name, value) in &parts.headers {
        if is_hop_by_hop(name) || name == axum::http::header::HOST {
            continue;
        }
        header
            .append_header(name, value.as_bytes())
            .map_err(|_| ProxyError::MalformedResponse)?;
    }
    // `host_port` (the original advertised text, not the resolved
    // `SocketAddr`) is what's sent as the `Host` header regardless of
    // whether resolution took the literal or `lookup_host` path.
    header
        .insert_header("host", host_port)
        .map_err(|_| ProxyError::MalformedResponse)?;
    header
        .insert_header("content-length", body_bytes.len().to_string())
        .map_err(|_| ProxyError::MalformedResponse)?;

    session
        .write_request_header(Box::new(header))
        .await
        .map_err(ProxyError::Session)?;
    if !body_bytes.is_empty() {
        session
            .write_body(&body_bytes)
            .await
            .map_err(ProxyError::Session)?;
    }
    session.finish_body().await.map_err(ProxyError::Session)?;

    session.read_response().await.map_err(ProxyError::Session)?;
    let status = session.get_status().ok_or(ProxyError::MalformedResponse)?;
    let resp_headers = session
        .resp_header()
        .ok_or(ProxyError::MalformedResponse)?
        .headers
        .clone();

    let mut builder = Response::builder()
        .status(StatusCode::from_u16(status.as_u16()).map_err(|_| ProxyError::MalformedResponse)?);
    for (name, value) in &resp_headers {
        if is_hop_by_hop(name) {
            continue;
        }
        builder = builder.header(name, value);
    }

    let (tx, rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(RESPONSE_CHANNEL_CAPACITY);
    tokio::spawn(async move {
        loop {
            let read = tokio::time::timeout(idle_timeout, session.read_body_bytes()).await;
            match read {
                Ok(Ok(Some(chunk))) => {
                    if tx.send(Ok(chunk)).await.is_err() {
                        break;
                    }
                }
                Ok(Ok(None)) => break,
                Ok(Err(_)) | Err(_) => {
                    tx.send(Err(std::io::Error::other(
                        "upstream response read failed or idle-timed out",
                    )))
                    .await
                    .ok();
                    break;
                }
            }
        }
    });
    let body = Body::from_stream(ReceiverStream::new(rx));

    builder
        .body(body)
        .map_err(|_| ProxyError::MalformedResponse)
}
