//! `CanonicalPath<T>` — drop-in wrapper around `axum::extract::Path<T>`
//! that converts `PathRejection` into a canonical `Problem+json` 400.
//!
//! Path half. Without this, axum's default `PathRejection`
//! emits a `text/plain; charset=utf-8` 400 (e.g. when a UUID path
//! parameter is malformed). The canonical-error middleware at
//! `toolkit::api::canonical_error_layer` does wrap such a body into a
//! minimal `Problem` now, but only the status survives: the message
//! naming the malformed parameter is logged and dropped from the
//! response. This wrapper is what keeps the diagnostic — see
//! [`super::canonical_json`] for the full story.
//!
//! Pattern mirrors [`super::canonical_json::CanonicalJson`].

use axum::extract::rejection::PathRejection;
use axum::extract::{FromRequestParts, Path};
use axum::http::request::Parts;
use rbac_sdk::error::{FieldError, RbacServiceError};
use serde::de::DeserializeOwned;
use toolkit::api::canonical_prelude::CanonicalError;

use crate::api::rest::error::rbac_service_error_to_canonical;

/// Drop-in replacement for `axum::extract::Path<T>` in handler
/// extractor position. Identical behaviour on success; on
/// rejection produces a canonical Problem-JSON 400 with
/// `field=path` and a reason code derived from the underlying
/// [`PathRejection`] variant.
#[derive(Debug, Clone)]
pub struct CanonicalPath<T>(pub T);

impl<T, S> FromRequestParts<S> for CanonicalPath<T>
where
    T: DeserializeOwned + Send,
    S: Send + Sync,
{
    type Rejection = CanonicalError;

    #[allow(clippy::manual_async_fn)]
    fn from_request_parts(
        parts: &mut Parts,
        state: &S,
    ) -> impl core::future::Future<Output = Result<Self, Self::Rejection>> + Send {
        async move {
            match Path::<T>::from_request_parts(parts, state).await {
                Ok(Path(value)) => Ok(CanonicalPath(value)),
                Err(rejection) => Err(path_rejection_to_canonical(&rejection)),
            }
        }
    }
}

fn path_rejection_to_canonical(rej: &PathRejection) -> CanonicalError {
    let (code, message) = classify_path_rejection(rej);
    rbac_service_error_to_canonical(RbacServiceError::validation_failed(vec![FieldError::new(
        "path", message, code,
    )]))
}

fn classify_path_rejection(rej: &PathRejection) -> (&'static str, String) {
    match rej {
        PathRejection::FailedToDeserializePathParams(_) => (
            "invalid_path_param",
            format!("path parameter is not valid: {rej}"),
        ),
        PathRejection::MissingPathParams(_) => (
            "missing_path_param",
            format!("path parameter is missing: {rej}"),
        ),
        // `PathRejection` is `#[non_exhaustive]`; any future variant
        // falls through to the generic invalid-path bucket.
        _ => (
            "invalid_path_param",
            format!("invalid path parameter: {rej}"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::body::Body;
    use axum::http::Request as HttpRequest;
    use axum::http::StatusCode;
    use axum::routing::get;
    use tower::ServiceExt;
    use uuid::Uuid;

    /// `Path<T>` is normally fed from the router's matched path
    /// segments. We exercise the full extractor by spinning a tiny
    /// router that mounts a `GET /thing/{id}` handler taking
    /// `CanonicalPath<Uuid>` — exactly the shape every RBAC
    /// handler uses.
    async fn handler(_id: CanonicalPath<Uuid>) -> &'static str {
        "ok"
    }

    fn router() -> Router {
        Router::new().route("/thing/{id}", get(handler))
    }

    #[tokio::test]
    async fn round_trips_valid_uuid() {
        let resp = router()
            .oneshot(
                HttpRequest::builder()
                    .uri(format!("/thing/{}", Uuid::nil()))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn malformed_uuid_returns_canonical_400() {
        let resp = router()
            .oneshot(
                HttpRequest::builder()
                    .uri("/thing/not-a-uuid")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let ct = resp
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .expect("Content-Type header")
            .to_str()
            .expect("utf8");
        assert!(
            ct.starts_with("application/problem+json"),
            "Content-Type MUST be application/problem+json, got {ct}",
        );
        let bytes = axum::body::to_bytes(resp.into_body(), 1_000_000)
            .await
            .expect("body");
        let body: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
        assert!(
            body["type"]
                .as_str()
                .unwrap_or("")
                .contains("invalid_argument"),
            "Problem `type` MUST be the invalid_argument URI, got {}",
            body["type"]
        );
        let violations = body["context"]["field_violations"]
            .as_array()
            .expect("field_violations MUST be present");
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0]["field"], "path");
        assert_eq!(violations[0]["reason"], "invalid_path_param");
    }
}
