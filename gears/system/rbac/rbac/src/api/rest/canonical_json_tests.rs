//! Tests for [`super::canonical_json::CanonicalJson`]. Split out of the
//! main file because the inline `mod tests` block triggered DE1101
//! (`tests_in_separate_files`) — the lint caps inline tests at 100
//! lines per file.

use super::*;
use axum::body::Body;
use axum::http::{Request as HttpRequest, header};

/// Wrap a body string in an `axum::extract::Request` with the
/// supplied `Content-Type`. `Content-Type: application/json` is the
/// path the JSON extractor takes on the happy path; other types
/// route through the `MissingJsonContentType` rejection.
fn build_request(content_type: Option<&str>, body: &'static str) -> Request {
    let mut builder = HttpRequest::builder().method("POST").uri("/");
    if let Some(ct) = content_type {
        builder = builder.header(header::CONTENT_TYPE, ct);
    }
    builder
        .body(Body::from(body))
        .expect("test request must build")
}

#[derive(serde::Deserialize, Debug, PartialEq)]
struct Payload {
    name: String,
}

#[tokio::test]
async fn round_trips_valid_body() {
    let req = build_request(Some("application/json"), r#"{"name":"alice"}"#);
    let CanonicalJson(payload): CanonicalJson<Payload> = CanonicalJson::from_request(req, &())
        .await
        .expect("extract");
    assert_eq!(
        payload,
        Payload {
            name: "alice".to_owned()
        }
    );
}

#[tokio::test]
async fn malformed_json_returns_canonical_400() {
    let req = build_request(Some("application/json"), "{invalid");
    let err = CanonicalJson::<Payload>::from_request(req, &())
        .await
        .expect_err("malformed body must reject");
    // The canonical error MUST be an InvalidArgument (400) with a
    // `body` field-violation.
    let problem = toolkit_canonical_errors::Problem::from_error(&err)
        .expect("CanonicalError serializes to Problem");
    assert!(
        problem.problem_type.contains("invalid_argument"),
        "Problem `type` MUST be the invalid_argument URI, got {}",
        problem.problem_type,
    );
    let violations = problem
        .context
        .get("field_violations")
        .and_then(|v| v.as_array())
        .expect("Problem `context.field_violations` MUST be present");
    assert_eq!(violations.len(), 1);
    assert_eq!(violations[0]["field"], "body");
}

#[tokio::test]
async fn missing_content_type_returns_canonical_400() {
    let req = build_request(None, "{}");
    let err = CanonicalJson::<Payload>::from_request(req, &())
        .await
        .expect_err("missing content-type must reject");
    let problem = toolkit_canonical_errors::Problem::from_error(&err)
        .expect("CanonicalError serializes to Problem");
    assert!(
        problem.problem_type.contains("invalid_argument"),
        "Problem `type` MUST be the invalid_argument URI, got {}",
        problem.problem_type,
    );
    let violations = problem
        .context
        .get("field_violations")
        .and_then(|v| v.as_array())
        .expect("field_violations MUST be present on Problem");
    assert_eq!(violations[0]["field"], "body");
    assert_eq!(violations[0]["reason"], "missing_json_content_type");
}

#[tokio::test]
async fn semantic_type_mismatch_returns_canonical_400() {
    // Valid JSON, but `name` is a number — JsonDataError, not
    // JsonSyntaxError. Verifies the deserialize-error branch.
    let req = build_request(Some("application/json"), r#"{"name":42}"#);
    let err = CanonicalJson::<Payload>::from_request(req, &())
        .await
        .expect_err("type mismatch must reject");
    let problem = toolkit_canonical_errors::Problem::from_error(&err)
        .expect("CanonicalError serializes to Problem");
    let violations = problem
        .context
        .get("field_violations")
        .and_then(|v| v.as_array())
        .expect("field_violations");
    assert_eq!(violations[0]["field"], "body");
    assert_eq!(violations[0]["reason"], "invalid_json_body");
}
