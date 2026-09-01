//! `CanonicalJson<T>` — drop-in wrapper around `axum::Json<T>` that
//! converts `JsonRejection` into a canonical `Problem+json` 400.
//!
//! Without this, axum's default `JsonRejection` serialises as
//! `text/plain; charset=utf-8` — a body no handler shaped on purpose.
//!
//! The canonical-error middleware at `toolkit::api::canonical_error_layer`
//! rescues such a body rather than passing it through: `is_unstructured_error_body`
//! treats `text/plain` (and a missing `Content-Type`) as unshaped and wraps it in
//! a minimal RFC 9457 `Problem`. So the envelope is not the reason this
//! wrapper exists — **the diagnostic is**. The generic wrap carries the status
//! and the status's reason phrase as `detail`; axum's own message, which names
//! the offending field and offset, is handed to `log_foreign_body` at
//! `tracing::debug!` and never placed in the client-visible body. A deployment
//! logging at `info` therefore records it nowhere at all. This wrapper turns the
//! rejection into a typed `RbacServiceError` with `FieldError`s, which
//! is what the `.error_400(openapi)` declaration on each affected handler
//! promises and what a client needs to fix its request.
//!
//! `toolkit::api::rest::extract` (`Json`, `Query`, `Path`) is the toolkit's own
//! per-extractor answer to the same problem, added in the same change that
//! widened the middleware. The OAGW gear already uses it; moving these wrappers
//! onto it is a follow-up, not a no-op — it would swap this module's field-level
//! errors for the toolkit's shape.
//!
//! Pattern mirrors `toolkit::api::odata::OData`, which converts
//! its own rejections into `CanonicalError` the same way.

use axum::Json;
use axum::extract::{FromRequest, Request, rejection::JsonRejection};
use rbac_sdk::error::{FieldError, RbacServiceError};
use serde::de::DeserializeOwned;
use toolkit::api::canonical_prelude::CanonicalError;

use crate::api::rest::error::rbac_service_error_to_canonical;

/// Drop-in replacement for `axum::Json<T>` in handler extractor
/// position. Identical behaviour on success; on rejection produces
/// a canonical Problem-JSON 400 with `field=body` and a reason code
/// derived from the underlying [`JsonRejection`] variant.
#[derive(Debug, Clone)]
pub struct CanonicalJson<T>(pub T);

impl<T, S> FromRequest<S> for CanonicalJson<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = CanonicalError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        match Json::<T>::from_request(req, state).await {
            Ok(Json(body)) => Ok(CanonicalJson(body)),
            Err(rejection) => Err(json_rejection_to_canonical(&rejection)),
        }
    }
}

/// Map an axum `JsonRejection` to the canonical Problem-JSON 400.
/// The reason `code` is a `snake_case` machine identifier the
/// caller can branch on; the `message` is human-readable and may
/// embed the underlying axum diagnostic.
fn json_rejection_to_canonical(rej: &JsonRejection) -> CanonicalError {
    let (code, message) = classify_json_rejection(rej);
    rbac_service_error_to_canonical(RbacServiceError::validation_failed(vec![FieldError::new(
        "body", message, code,
    )]))
}

/// Classify a `JsonRejection` into a `(code, message)` pair. `code`
/// matches one of the well-known `snake_case` reasons the SDK
/// already documents for body failures so clients can dispatch
/// without parsing the human message.
fn classify_json_rejection(rej: &JsonRejection) -> (&'static str, String) {
    match rej {
        JsonRejection::JsonSyntaxError(_) => (
            "json_syntax_error",
            format!("request body is not valid JSON: {rej}"),
        ),
        JsonRejection::JsonDataError(_) => (
            "invalid_json_body",
            format!("request body could not be deserialized: {rej}"),
        ),
        JsonRejection::MissingJsonContentType(_) => (
            "missing_json_content_type",
            "expected request to have `Content-Type: application/json`".to_owned(),
        ),
        JsonRejection::BytesRejection(_) => (
            "json_body_read_error",
            format!("failed to read request body: {rej}"),
        ),
        // `JsonRejection` is `#[non_exhaustive]`; any future variant
        // falls through to the generic invalid-body bucket so
        // clients still get a Problem envelope.
        _ => ("invalid_json_body", format!("invalid request body: {rej}")),
    }
}

#[cfg(test)]
#[path = "canonical_json_tests.rs"]
mod tests;
