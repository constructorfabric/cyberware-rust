//! Canonical error middleware (DESIGN.md §3.2 / §3.6 / §3.7).
//!
//! Post-processes responses with `Content-Type: application/problem+json`,
//! filling missing `trace_id` (W3C `traceparent` → `x-trace-id` →
//! `x-request-id` → span-id fallback) and `instance` (request URI path).
//! Logs at `warn!` for 4xx / `error!` for 5xx with structured fields.
//!
//! Any other error-status response whose body is genuinely unstructured (a
//! tower-layer short-circuit such as `RequestBodyLimitLayer`, an unmatched
//! route, or any other rejection never typed as `CanonicalError` - all of
//! which render as `text/plain` or omit `Content-Type` entirely, never a
//! real content type a handler chose on purpose) is wrapped into a minimal,
//! valid RFC 9457 `Problem` rather than passed through as-is - see
//! `wrap_foreign_response` and `is_unstructured_error_body`. A response with
//! any other `Content-Type` (`application/json`, `text/html`, ...) was
//! deliberately shaped by the handler that returned it and is left alone -
//! this fallback exists to rescue responses nothing ever shaped on purpose,
//! not to overwrite one that was.
//! A foreign 5xx is mapped to the real `internal` canonical category
//! (DESIGN.md §2.1's fail-safe fallback: "no error escapes the system
//! without a canonical category") since it is, by definition, this
//! platform's own fault, not the client's. A foreign 4xx uses RFC 9457
//! §4.2.1's `"about:blank"` convention instead - unlike a 5xx, a bare 4xx
//! genuinely doesn't determine one canonical category over another (e.g.
//! `invalid_argument` vs. `failed_precondition` vs. `out_of_range`), so
//! `about:blank` is the honest "no more specific type than the status code"
//! rather than a guess.
//! `crate::api::rest::extract` (`Json`, `Query`, `Path`) is the precise,
//! per-extractor counterpart to this generic fallback: prefer it wherever
//! the failure's shape is known ahead of time. See
//! `docs/arch/errors/ADR/0006-cpt-cf-adr-error-middleware-catchall.md` for
//! the decision record. Panics remain out of scope here - `CatchPanicLayer`
//! handles those before this middleware ever sees a response.
//!
//! **This middleware always renders JSON, gear-wide, with no content
//! negotiation, for the responses it actually touches.** Every wrapped
//! `Problem` (this fallback's, and every `CanonicalError`'s `IntoResponse`)
//! is `application/problem+json` unconditionally - there is no
//! `Accept`-header check anywhere in this pipeline. Since this middleware
//! wraps a gear's *entire* router (every gear, via `toolkit`'s `OoP`
//! bootstrap), it would be equally unconditional about *which* responses it
//! rewrites if it matched on status alone - which is exactly the bug
//! `is_unstructured_error_body` exists to close: a gear route that
//! deliberately returns a non-`CanonicalError`, non-JSON, or custom-shaped
//! JSON error body (server-rendered HTML, a legacy hand-rolled health
//! check, ...) keeps that body exactly as returned, on any status. Only a
//! response with no shape at all - because nothing ever gave it one - is
//! this fallback's business.

use std::time::Duration;

use axum::{
    body::{Body, to_bytes},
    extract::Request,
    http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header, response::Parts},
    middleware::Next,
    response::Response,
};
use toolkit_canonical_errors::{CanonicalError, ForeignPassthrough, Http, Problem};

const PROBLEM_JSON: &str = "application/problem+json";

/// Cap on how much of a foreign (non-`Problem`) error response body the
/// generic-wrap fallback will read for server-side diagnostic logging. Never
/// placed in the client-visible `detail` - see `wrap_as_generic_problem`.
/// Every rejection body seen in this codebase today (axum's own
/// `JsonRejection` messages, tower-http's body-limit text) is well under a
/// kilobyte; this bounds the worst case for a response this middleware does
/// not control the size of.
const MAX_FOREIGN_BODY_LOG_BYTES: usize = 8 * 1024;

/// Hard cap on how many characters of a foreign body are ever placed in a
/// log line, after the byte cap above and UTF-8 lossy decoding.
const MAX_FOREIGN_BODY_LOG_CHARS: usize = 256;

/// Budget for reading a foreign response body before wrapping it. A stalled
/// or slow upstream body (e.g. one proxied by `toolkit-gateway::Forwarder`)
/// must not turn a fast error into a hung request - on elapse, wrapping
/// proceeds without the diagnostic log, using the status's reason phrase.
/// Reused (not just for the diagnostic-log read) as the budget for
/// `enrich_problem_response`'s own initial body read below - a response
/// merely labeled `application/problem+json` is exactly as untrusted as any
/// other foreign body until its content is actually validated.
const FOREIGN_BODY_READ_TIMEOUT: Duration = Duration::from_secs(2);

/// Cap on `enrich_problem_response`'s initial `application/problem+json`
/// body read. Distinct from `MAX_FOREIGN_BODY_LOG_BYTES`: that one only
/// bounds a truncated *diagnostic log excerpt*, while this bounds the real
/// body used to build the client response, so it needs headroom for a
/// legitimate `field_violations` list rather than a short log line. This
/// platform's own `CanonicalError`-produced bodies never come close to it;
/// it exists to bound a foreign peer's body before it's fully buffered.
const MAX_PROBLEM_BODY_BYTES: usize = 64 * 1024;

/// Tower middleware function that fills `trace_id` / `instance` on canonical
/// Problem responses and logs at `warn!` (4xx) / `error!` (5xx); wraps any
/// other error-status response into a minimal RFC 9457 `Problem`.
///
/// A response tagged `ForeignPassthrough` (by a reverse-proxy layer like
/// `toolkit-gateway::Forwarder` or `oagw`'s proxy data-plane) is returned
/// completely unchanged, regardless of status or `Content-Type` - it was
/// never constructed via `CanonicalError` and was never meant to be;
/// rewriting or even reading its body would destroy a genuine upstream's
/// own error identity and force full buffering of what may still be a
/// streamed body.
///
/// Success/redirect responses pass through unchanged. Malformed Problem
/// bodies are logged at `error!`; on an error status they are re-wrapped
/// through the same fallback as any other untyped rejection, and on a
/// non-error status they pass through unchanged.
pub async fn canonical_error_middleware(request: Request, next: Next) -> Response {
    let uri_path = request.uri().path().to_owned();
    let request_headers = request.headers().clone();

    let response = next.run(request).await;

    if response.extensions().get::<ForeignPassthrough>().is_some() {
        return response;
    }

    if is_problem_response(&response) {
        return enrich_problem_response(response, &uri_path, &request_headers).await;
    }

    let status = response.status();
    let is_error_status = status.is_client_error() || status.is_server_error();
    if is_error_status && is_unstructured_error_body(&response) {
        // A response reaching this branch is not `application/problem+json`
        // (see the `is_problem_response` check above), so it essentially
        // never carries a `CanonicalError` extension in practice - only
        // `CanonicalError::into_response()` sets one, and that impl always
        // sets `Content-Type: application/problem+json` in the same call.
        // Checked anyway for the same reason `enrich_problem_response`'s
        // malformed-body path checks it: defensively, in case some future
        // layer sets the extension without matching Content-Type.
        let recovered = response.extensions().get::<CanonicalError>().cloned();
        return wrap_foreign_response(response, &uri_path, &request_headers, recovered).await;
    }

    response
}

/// Fills `instance` / `trace_id` on a response already carrying
/// `Content-Type: application/problem+json` and logs it.
async fn enrich_problem_response(
    response: Response,
    uri_path: &str,
    request_headers: &HeaderMap,
) -> Response {
    let (parts, body) = response.into_parts();

    // The `IntoResponse` impl for `CanonicalError` stashes the original error
    // into the response extensions. Recover it so the diagnostic on
    // `Internal` / `Unknown` (carried by `#[serde(skip)]` fields and thus
    // absent from the wire body) can be logged server-side per DESIGN §3.6.
    let canonical_err = parts.extensions.get::<CanonicalError>().cloned();

    let bytes = match tokio::time::timeout(
        FOREIGN_BODY_READ_TIMEOUT,
        to_bytes(body, MAX_PROBLEM_BODY_BYTES),
    )
    .await
    {
        Ok(Ok(b)) => b,
        Ok(Err(e)) => {
            tracing::error!(error = %e, "canonical error middleware: failed to read response body");
            return finish_unreadable_problem_body(parts, uri_path, request_headers, canonical_err)
                .await;
        }
        Err(_) => {
            tracing::error!(
                instance = uri_path,
                "canonical error middleware: timed out reading problem+json response body"
            );
            return finish_unreadable_problem_body(parts, uri_path, request_headers, canonical_err)
                .await;
        }
    };

    let mut problem: Problem = match serde_json::from_slice(&bytes) {
        Ok(p) => p,
        Err(e) => {
            tracing::error!(error = %e, "canonical error middleware: failed to deserialize problem+json body");
            let status = parts.status;
            if !(status.is_client_error() || status.is_server_error()) {
                // A non-error response (2xx/3xx) that merely mislabeled its
                // Content-Type as `application/problem+json` is not this
                // middleware's business to rewrite into an error - the
                // "every error response is a valid Problem" guarantee only
                // applies to actual error statuses. Pass the original bytes
                // through unchanged rather than manufacturing a Problem with
                // a success status.
                tracing::warn!(
                    status = status.as_u16(),
                    instance = uri_path,
                    "canonical error middleware: non-error response claimed application/problem+json but its body is not valid Problem JSON; passing through unchanged"
                );
                return Response::from_parts(parts, Body::from(bytes));
            }
            // A response claiming `application/problem+json` that isn't
            // actually valid `Problem` JSON still needs to become one - the
            // platform-wide guarantee is "every error response is a valid
            // RFC 9457 Problem", not "unless it lied about its own
            // Content-Type". Route it through the same safe fallback as any
            // other untyped rejection, from the bytes already read (no
            // second body read).
            let foreign = Response::from_parts(parts, Body::from(bytes));
            return wrap_foreign_response(foreign, uri_path, request_headers, canonical_err).await;
        }
    };

    if problem.instance.is_none() {
        problem.instance = Some(uri_path.to_owned());
    }
    if problem.trace_id.is_none() {
        problem.trace_id = extract_trace_id(request_headers);
    }
    // RFC 9457 §3.1 makes `status` advisory; a peer that omitted it is
    // still fully spec-compliant, and the real response status is right here.
    problem.status.get_or_insert(parts.status.as_u16());

    log_problem(&problem, canonical_err.as_ref());

    let new_bytes = match serde_json::to_vec(&problem) {
        Ok(b) => b,
        Err(e) => {
            tracing::error!(
                error = %e,
                "canonical error middleware: failed to re-serialize problem+json body"
            );
            return Response::from_parts(parts, Body::from(bytes));
        }
    };

    let len = new_bytes.len();
    let mut response = Response::from_parts(parts, Body::from(new_bytes));
    response
        .headers_mut()
        .insert(header::CONTENT_LENGTH, HeaderValue::from(len));
    response
}

/// Shared fallback for `enrich_problem_response` when its initial body read
/// failed outright (I/O error, size limit, or timeout) rather than merely
/// deserializing to something other than `Problem` - there are no bytes to
/// build a `Problem` from or to pass through unchanged, so this mirrors the
/// same 2xx/3xx-vs-error-status split the deserialize-failure path applies:
/// a non-error response is left as an empty body (no valid bytes exist to
/// preserve either way), an error response is routed through the same safe
/// fallback as any other untyped rejection.
async fn finish_unreadable_problem_body(
    parts: Parts,
    uri_path: &str,
    request_headers: &HeaderMap,
    canonical_err: Option<CanonicalError>,
) -> Response {
    let status = parts.status;
    if !(status.is_client_error() || status.is_server_error()) {
        return Response::from_parts(parts, Body::empty());
    }
    let foreign = Response::from_parts(parts, Body::empty());
    wrap_foreign_response(foreign, uri_path, request_headers, canonical_err).await
}

/// Builds a minimal, valid `Problem` for a response that was never typed as
/// `CanonicalError` - a tower-layer short-circuit, an unmatched route, or any
/// other untyped rejection. If `recovered` is `Some` (a `CanonicalError` was
/// recovered from the original response's extensions - see
/// `enrich_problem_response`'s malformed-body path), that real category is
/// used directly: it isn't a guess from a bare status code, so there's no
/// reason to discard it in favor of a class-based fallback. Only when
/// `recovered` is `None` does the class-based split apply: a foreign 5xx
/// maps to the real `internal` canonical category (`wrap_as_internal_problem`);
/// a foreign 4xx uses RFC 9457 §4.2.1's `"about:blank"` convention
/// (`wrap_as_generic_problem`) - see this module's doc comment for why the
/// two classes are treated differently in that case.
async fn wrap_foreign_response(
    response: Response,
    uri_path: &str,
    request_headers: &HeaderMap,
    recovered: Option<CanonicalError>,
) -> Response {
    if let Some(canonical) = recovered {
        return wrap_recovered_canonical_error(response, uri_path, request_headers, canonical)
            .await;
    }
    if response.status().is_server_error() {
        wrap_as_internal_problem(response, uri_path, request_headers).await
    } else {
        wrap_as_generic_problem(response, uri_path, request_headers).await
    }
}

/// Builds a `Problem` directly from a `CanonicalError` recovered from the
/// original response's extensions, for a response whose body could not be
/// trusted (failed to read, or failed to deserialize as `Problem`) but whose
/// extensions still carried the real error the handler produced. Skips the
/// class-based `about:blank`/`internal` guess entirely - the real category,
/// status, and diagnostic are already known.
async fn wrap_recovered_canonical_error(
    response: Response,
    uri_path: &str,
    request_headers: &HeaderMap,
    canonical: CanonicalError,
) -> Response {
    let status = response.status();
    let (parts, body) = response.into_parts();

    log_foreign_body(body, status, uri_path).await;

    let mut problem: Problem = canonical.clone().into();
    problem.instance = Some(uri_path.to_owned());
    problem.trace_id = extract_trace_id(request_headers);

    log_problem(&problem, Some(&canonical));
    finish_wrapped_response(parts, &problem)
}

/// Reads a foreign response body for server-side diagnostic logging only -
/// never surfaced in any client-visible field. This response was never
/// typed as `CanonicalError`, so nothing here has vetted it for the "no
/// internal details on the wire" guarantee the rest of this error system
/// enforces (`Internal`/`Unknown`'s `description` is `#[serde(skip)]` for
/// the same reason); a foreign 5xx in particular could carry a stack trace,
/// an internal hostname, or a credential. The log line itself is bounded
/// and escaped: it reaches a less-restricted access boundary than the HTTP
/// response, and the raw text is client-controlled, so it's escaped to
/// block log-line injection (embedded newlines/ANSI) and hard-truncated in
/// addition to the byte cap already applied to the read. Bounded by
/// `FOREIGN_BODY_READ_TIMEOUT` so a stalled or slow body (e.g. one proxied
/// by `toolkit-gateway::Forwarder`) cannot turn a fast error into a hung
/// request.
async fn log_foreign_body(body: Body, status: StatusCode, uri_path: &str) {
    tracing::warn!(
        status = status.as_u16(),
        instance = uri_path,
        "canonical error middleware: wrapping a foreign error response"
    );
    match tokio::time::timeout(
        FOREIGN_BODY_READ_TIMEOUT,
        to_bytes(body, MAX_FOREIGN_BODY_LOG_BYTES),
    )
    .await
    {
        Ok(Ok(bytes)) => {
            let text = String::from_utf8_lossy(&bytes);
            let text = text.trim();
            if !text.is_empty() {
                let truncated: String = text.chars().take(MAX_FOREIGN_BODY_LOG_CHARS).collect();
                tracing::debug!(
                    status = status.as_u16(),
                    instance = uri_path,
                    body = %truncated.escape_debug(),
                    "canonical error middleware: foreign response body (diagnostic only, never sent to client)"
                );
            }
        }
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "canonical error middleware: failed to read foreign response body while wrapping");
        }
        Err(_) => {
            tracing::warn!(
                status = status.as_u16(),
                instance = uri_path,
                "canonical error middleware: timed out reading foreign response body while wrapping"
            );
        }
    }
}

/// Headers preserved from the original foreign response onto the wrapped
/// `Problem` response. The body is fully replaced, so the correct default is
/// an allowlist, not a denylist: nothing that described or applied to the
/// discarded original body (e.g. `Content-Encoding`, `ETag`, `Server`, an
/// internal `X-*` header) is assumed to still be true of the new one.
/// `Content-Type`/`Content-Length` are set separately, fresh, for the new
/// body. What's kept: `WWW-Authenticate`/`Retry-After` carry client-actionable
/// semantics independent of body content (auth challenge, backoff); the
/// CORS headers and `Vary` are needed for a wrapped error to still pass a
/// browser's CORS check.
const PRESERVED_FOREIGN_HEADERS: &[axum::http::HeaderName] = &[
    header::RETRY_AFTER,
    header::ACCESS_CONTROL_ALLOW_ORIGIN,
    header::ACCESS_CONTROL_ALLOW_CREDENTIALS,
    header::ACCESS_CONTROL_EXPOSE_HEADERS,
    header::VARY,
];

/// Rate-limit/quota headers set alongside a `resource_exhausted`/
/// `service_unavailable` `CanonicalError` response by
/// `gears/system/api-gateway/src/middleware/throttling.rs` (`RateLimit-Policy`,
/// `RateLimit-Limit`, `X-RateLimit-Limit`) and `gears/system/oagw`'s
/// `error_response` (`x-ratelimit-limit`, `x-ratelimit-remaining`,
/// `x-ratelimit-reset`) - both attach these to the same kind of response
/// (`CanonicalError`/`Problem`, extension present) that can reach
/// `wrap_recovered_canonical_error` if its body is ever corrupted
/// downstream; without this list, `Retry-After` alone would survive that
/// path while its sibling quota numbers silently vanished.
const PRESERVED_RATE_LIMIT_HEADERS: &[&str] = &[
    "ratelimit-policy",
    "ratelimit-limit",
    "x-ratelimit-limit",
    "x-ratelimit-remaining",
    "x-ratelimit-reset",
];

/// Preserved via `get_all`/`append`, not `get`/`insert` like
/// `PRESERVED_FOREIGN_HEADERS` - both are repeatable headers.
/// `Set-Cookie` (RFC 6265 §4.1.1 forbids folding multiple cookies into one
/// line) - a response clearing a session or setting several cookies on an
/// error path would silently lose all but one if treated as single-valued.
/// `WWW-Authenticate` can likewise carry multiple challenges (RFC 9110
/// §11.6.1) - no call site in this codebase appends more than one today
/// (checked), but treating it as single-valued here would be a latent
/// version of the exact bug `Set-Cookie` just had.
const PRESERVED_MULTI_VALUE_FOREIGN_HEADERS: &[axum::http::HeaderName] =
    &[header::SET_COOKIE, header::WWW_AUTHENTICATE];

/// Serializes `problem` and swaps it in as the response body, keeping only
/// `PRESERVED_FOREIGN_HEADERS`/`PRESERVED_RATE_LIMIT_HEADERS`/
/// `PRESERVED_MULTI_VALUE_FOREIGN_HEADERS` from the original response.
fn finish_wrapped_response(parts: axum::http::response::Parts, problem: &Problem) -> Response {
    let bytes = match serde_json::to_vec(problem) {
        Ok(b) => b,
        Err(e) => {
            tracing::error!(
                error = %e,
                "canonical error middleware: failed to serialize generic-wrap problem body"
            );
            return Response::from_parts(parts, Body::empty());
        }
    };

    let len = bytes.len();
    let mut headers = HeaderMap::new();
    for name in PRESERVED_FOREIGN_HEADERS {
        if let Some(value) = parts.headers.get(name) {
            headers.insert(name.clone(), value.clone());
        }
    }
    for name in PRESERVED_RATE_LIMIT_HEADERS {
        // Static, pre-lowercased literals - safe to unwrap.
        let name = HeaderName::from_static(name);
        if let Some(value) = parts.headers.get(&name) {
            headers.insert(name, value.clone());
        }
    }
    for name in PRESERVED_MULTI_VALUE_FOREIGN_HEADERS {
        for value in parts.headers.get_all(name) {
            headers.append(name.clone(), value.clone());
        }
    }
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(PROBLEM_JSON));
    headers.insert(header::CONTENT_LENGTH, HeaderValue::from(len));

    let mut response = Response::from_parts(parts, Body::from(bytes));
    *response.headers_mut() = headers;
    response
}

/// Builds a minimal `about:blank` `Problem` for a foreign **4xx** response -
/// a bare client-error status alone doesn't determine one canonical
/// category over another (e.g. 400 could be `invalid_argument`,
/// `failed_precondition`, or `out_of_range`), so this is the honest "no
/// more specific type than the status code" per RFC 9457 §4.2.1, not a
/// guess.
async fn wrap_as_generic_problem(
    response: Response,
    uri_path: &str,
    request_headers: &HeaderMap,
) -> Response {
    let status = response.status();
    let (parts, body) = response.into_parts();
    let reason = status.canonical_reason().unwrap_or("Error");

    log_foreign_body(body, status, uri_path).await;

    let problem = Problem {
        problem_type: "about:blank".to_owned(),
        title: reason.to_owned(),
        status: Some(status.as_u16()),
        detail: reason.to_owned(),
        instance: Some(uri_path.to_owned()),
        trace_id: extract_trace_id(request_headers),
        context: serde_json::Value::Object(serde_json::Map::new()),
        error_code: None,
        error_domain: None,
    };

    log_problem(&problem, None);
    finish_wrapped_response(parts, &problem)
}

/// Builds a real `internal` canonical-category `Problem` for a foreign
/// **5xx** response - unlike a 4xx, a 5xx is unambiguous: it's this
/// platform's own fault, not the client's, so DESIGN.md §2.1's fail-safe
/// fallback ("no error escapes the system without a canonical category")
/// applies directly. The original status is preserved via `.with_override`
/// when it isn't already 500 (e.g. a proxied 502/503/504) - `internal`'s
/// default status is already 500, and every override here stays within the
/// 5xx class, so the builder's same-status-class invariant always holds.
async fn wrap_as_internal_problem(
    response: Response,
    uri_path: &str,
    request_headers: &HeaderMap,
) -> Response {
    let status = response.status();
    let (parts, body) = response.into_parts();
    let reason = status.canonical_reason().unwrap_or("Error");

    log_foreign_body(body, status, uri_path).await;

    let canonical = if status == StatusCode::INTERNAL_SERVER_ERROR {
        CanonicalError::internal(reason).create()
    } else {
        CanonicalError::internal(reason)
            .with_override(Http::status_code(status.as_u16()))
            .create()
    };
    let mut problem: Problem = canonical.clone().into();
    problem.instance = Some(uri_path.to_owned());
    problem.trace_id = extract_trace_id(request_headers);

    log_problem(&problem, Some(&canonical));
    finish_wrapped_response(parts, &problem)
}

/// The response's `Content-Type` media type, ignoring parameters (e.g.
/// `; charset=...`) - per RFC 9110, those never affect the type itself, and
/// a naive full-header comparison would both reject a validly-cased-but-
/// parameterized value and wrongly distinguish two responses of the same
/// underlying type.
fn content_type_media(response: &Response) -> Option<&str> {
    response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|ct| {
            ct.split_once(';')
                .map_or(ct, |(media_type, _)| media_type)
                .trim()
        })
}

fn is_problem_response(response: &Response) -> bool {
    // Comparison is ASCII-case-insensitive per RFC 9110 - a naive
    // `starts_with` would both reject a validly-cased
    // `Application/Problem+Json` and wrongly accept an unrelated type like
    // `application/problem+json-seq` that merely shares this prefix.
    content_type_media(response).is_some_and(|mt| mt.eq_ignore_ascii_case(PROBLEM_JSON))
}

/// Whether a response's body is genuinely unshaped rather than something a
/// handler deliberately built. Every case this fallback is actually meant
/// to rescue - an axum extractor rejection not yet migrated to
/// `crate::api::rest::extract`, a tower-layer short-circuit like
/// `RequestBodyLimitLayer`, an unmatched route - renders as `text/plain` or
/// omits `Content-Type` entirely; none of them is a real content type any
/// handler chose on purpose. A response with any other `Content-Type`
/// (`application/json`, `text/html`, ...) was shaped by the code that
/// returned it, even if that shape isn't `CanonicalError`/`Problem` - e.g. a
/// legacy hand-rolled JSON error body predating this platform's RFC 9457
/// adoption. Overwriting that body would silently destroy real,
/// client-relied-on fields with no way for the handler to opt out short of
/// switching its own `Content-Type` - this happened for real to
/// `api-gateway`'s `/health` endpoint (its `components` array), which is
/// exactly why this check exists rather than matching on status alone.
fn is_unstructured_error_body(response: &Response) -> bool {
    match content_type_media(response) {
        None => true,
        Some(mt) => mt.eq_ignore_ascii_case("text/plain"),
    }
}

/// W3C `traceparent` → `x-trace-id` → `x-request-id` → span-id fallback.
///
/// For `traceparent`, returns the 32-hex trace-id segment only — matching
/// `toolkit_http::otel::parse_trace_id` and the access log / `OTel` span
/// recording in this codebase, so the wire `trace_id` is grep-equal to the
/// trace-id surfaced in logs and traces. A malformed traceparent falls
/// through to `x-trace-id` / `x-request-id` (preserves the function's
/// graceful-failure semantics).
fn extract_trace_id(headers: &HeaderMap) -> Option<String> {
    if let Some(tp) = headers.get("traceparent").and_then(|v| v.to_str().ok())
        && let Some(trace_id) = parse_w3c_trace_id(tp)
    {
        return Some(trace_id);
    }
    for name in ["x-trace-id", "x-request-id"] {
        if let Some(v) = headers.get(name).and_then(|v| v.to_str().ok()) {
            return Some(v.to_owned());
        }
    }
    tracing::Span::current()
        .id()
        .map(|id| id.into_u64().to_string())
}

/// Mirror of `toolkit_http::otel::parse_trace_id`. Duplicated rather than
/// taking a new dep edge from `toolkit` onto `toolkit-http` for seven lines
/// of parsing. Keep behaviour in lock-step with the source.
fn parse_w3c_trace_id(traceparent: &str) -> Option<String> {
    let parts: Vec<&str> = traceparent.split('-').collect();
    if parts.len() >= 4 && parts[0] == "00" {
        Some(parts[1].to_owned())
    } else {
        None
    }
}

fn log_problem(problem: &Problem, canonical: Option<&CanonicalError>) {
    // Every Problem this middleware logs was either just normalized from a
    // real HTTP status (`enrich_problem_response`) or built via
    // `From<CanonicalError>`, so `status` is always `Some` in practice; `0`
    // is a safe fallback that simply logs nothing (it matches neither range
    // below) rather than a guessed status.
    let status = problem.status.unwrap_or(0);
    let problem_type = problem.problem_type.as_str();
    let instance = problem.instance.as_deref().unwrap_or("");
    let trace_id = problem.trace_id.as_deref().unwrap_or("");
    // `diagnostic()` returns Some only for `Internal` / `Unknown` (5xx-only
    // categories). Surface it server-side so operators can correlate
    // `trace_id` → root cause without exposing it on the wire.
    let description = canonical.and_then(CanonicalError::diagnostic).unwrap_or("");

    if (400..500).contains(&status) {
        tracing::warn!(
            status,
            problem_type,
            instance,
            trace_id,
            "canonical error response (client)"
        );
    } else if (500..600).contains(&status) {
        tracing::error!(
            status,
            problem_type,
            instance,
            trace_id,
            description,
            "canonical error response (server)"
        );
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "canonical_error_layer_tests.rs"]
mod tests;
