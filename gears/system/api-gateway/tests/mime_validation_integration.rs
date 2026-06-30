#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Integration tests for MIME validation middleware
//!
//! Tests the middleware behavior through a real Axum router setup,
//! without testing private implementation details.

use axum::{
    Json, Router,
    body::Body,
    http::{Request, StatusCode},
    response::IntoResponse,
    routing::post,
};
use http::Method;
use serde_json::json;
use toolkit::api::OperationSpec;
use toolkit_canonical_errors::Problem;
use toolkit_gts::gts_uri;
use tower::ServiceExt; // for oneshot

use api_gateway::middleware::mime_validation::{
    build_mime_validation_map, mime_validation_middleware,
};
use toolkit::api::operation_builder::VendorExtensions;

const INVALID_ARGUMENT_TYPE: &str =
    gts_uri!("cf.core.errors.err.v1~cf.core.err.invalid_argument.v1~");
const PROBLEM_JSON: &str = "application/problem+json";

/// Helper to extract Problem from response
async fn extract_problem(response: axum::response::Response) -> Problem {
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("Failed to read body");
    serde_json::from_slice(&body).expect("Failed to parse Problem JSON")
}

fn content_type(response: &axum::response::Response) -> &str {
    response
        .headers()
        .get(http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
}

/// Test handler that just returns OK
async fn test_handler(Json(payload): Json<serde_json::Value>) -> impl IntoResponse {
    (StatusCode::OK, Json(json!({"received": payload})))
}

#[tokio::test]
async fn test_middleware_allows_configured_content_type() {
    // Setup: operation that only allows application/json
    let specs = vec![OperationSpec {
        method: Method::POST,
        path: "/api/data".to_owned(),
        operation_id: Some("test:create".to_owned()),
        summary: None,
        description: None,
        tags: vec![],
        params: vec![],
        request_body: None,
        responses: vec![],
        handler_id: "test".to_owned(),
        authenticated: false,
        exposed: false,
        license_requirement: None,
        throttling: None,
        allowed_request_content_types: Some(vec!["application/json"]),
        vendor_extensions: VendorExtensions::default(),
    }];

    let validation_map = build_mime_validation_map(&specs);

    let app =
        Router::new()
            .route("/api/data", post(test_handler))
            .layer(axum::middleware::from_fn(move |req, next| {
                mime_validation_middleware(validation_map.clone(), req, next)
            }));

    // Test: Send request with allowed content type
    let request = Request::builder()
        .method("POST")
        .uri("/api/data")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"test": "data"}"#))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();

    // Should pass through to handler
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_middleware_strips_content_type_parameters() {
    // Setup: operation that allows application/json
    let specs = vec![OperationSpec {
        method: Method::POST,
        path: "/api/data".to_owned(),
        operation_id: Some("test:create".to_owned()),
        summary: None,
        description: None,
        tags: vec![],
        params: vec![],
        request_body: None,
        responses: vec![],
        handler_id: "test".to_owned(),
        authenticated: false,
        exposed: false,
        license_requirement: None,
        throttling: None,
        allowed_request_content_types: Some(vec!["application/json"]),
        vendor_extensions: VendorExtensions::default(),
    }];

    let validation_map = build_mime_validation_map(&specs);

    let app =
        Router::new()
            .route("/api/data", post(test_handler))
            .layer(axum::middleware::from_fn(move |req, next| {
                mime_validation_middleware(validation_map.clone(), req, next)
            }));

    // Test: Send request with charset parameter
    let request = Request::builder()
        .method("POST")
        .uri("/api/data")
        .header("content-type", "application/json; charset=utf-8")
        .body(Body::from(r#"{"test": "data"}"#))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();

    // Should pass through (parameters stripped)
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_middleware_rejects_disallowed_content_type() {
    // Setup: operation that only allows application/json
    let specs = vec![OperationSpec {
        method: Method::POST,
        path: "/api/data".to_owned(),
        operation_id: Some("test:create".to_owned()),
        summary: None,
        description: None,
        tags: vec![],
        params: vec![],
        request_body: None,
        responses: vec![],
        handler_id: "test".to_owned(),
        authenticated: false,
        exposed: false,
        license_requirement: None,
        throttling: None,
        allowed_request_content_types: Some(vec!["application/json"]),
        vendor_extensions: VendorExtensions::default(),
    }];

    let validation_map = build_mime_validation_map(&specs);

    let app =
        Router::new()
            .route("/api/data", post(test_handler))
            .layer(axum::middleware::from_fn(move |req, next| {
                mime_validation_middleware(validation_map.clone(), req, next)
            }));

    // Test: Send request with disallowed content type
    let request = Request::builder()
        .method("POST")
        .uri("/api/data")
        .header("content-type", "text/plain")
        .body(Body::from("plain text"))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();

    // Should reject with 400 (canonical invalid_argument)
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(content_type(&response), PROBLEM_JSON);

    let problem = extract_problem(response).await;
    assert_eq!(problem.status, Some(400));
    assert_eq!(problem.problem_type, INVALID_ARGUMENT_TYPE);
    let violations = problem
        .context
        .get("field_violations")
        .and_then(|v| v.as_array())
        .expect("field_violations must be present");
    assert_eq!(violations.len(), 1);
    assert_eq!(violations[0]["field"], "content-type");
    assert_eq!(violations[0]["reason"], "UNSUPPORTED_MEDIA_TYPE");
    let description = violations[0]["description"].as_str().unwrap_or("");
    assert!(description.contains("text/plain"));
    assert!(description.contains("application/json"));
}

#[tokio::test]
async fn test_middleware_rejects_missing_content_type() {
    // Setup: operation that requires specific content type
    let specs = vec![OperationSpec {
        method: Method::POST,
        path: "/files/v1/upload".to_owned(),
        operation_id: Some("test:upload".to_owned()),
        summary: None,
        description: None,
        tags: vec![],
        params: vec![],
        request_body: None,
        responses: vec![],
        handler_id: "test".to_owned(),
        authenticated: false,
        exposed: false,
        license_requirement: None,
        throttling: None,
        allowed_request_content_types: Some(vec!["multipart/form-data"]),
        vendor_extensions: VendorExtensions::default(),
    }];

    let validation_map = build_mime_validation_map(&specs);

    let app = Router::new()
        .route("/files/v1/upload", post(test_handler))
        .layer(axum::middleware::from_fn(move |req, next| {
            mime_validation_middleware(validation_map.clone(), req, next)
        }));

    // Test: Send request without content-type header
    let request = Request::builder()
        .method("POST")
        .uri("/files/v1/upload")
        .body(Body::from("data"))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();

    // Should reject with 400 (canonical invalid_argument)
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(content_type(&response), PROBLEM_JSON);

    let problem = extract_problem(response).await;
    assert_eq!(problem.status, Some(400));
    assert_eq!(problem.problem_type, INVALID_ARGUMENT_TYPE);
    let violations = problem
        .context
        .get("field_violations")
        .and_then(|v| v.as_array())
        .expect("field_violations must be present");
    assert_eq!(violations.len(), 1);
    assert_eq!(violations[0]["field"], "content-type");
    assert_eq!(violations[0]["reason"], "MISSING_CONTENT_TYPE");
    assert!(
        violations[0]["description"]
            .as_str()
            .unwrap_or("")
            .contains("Missing Content-Type")
    );
}

#[tokio::test]
async fn test_middleware_passes_through_unconfigured_routes() {
    // Setup: No MIME validation configured for this route
    let specs = vec![]; // Empty specs, no validation

    let validation_map = build_mime_validation_map(&specs);

    // Apply middleware AFTER routing (like in real usage)
    let app = Router::new()
        .route("/tests/v1/public", post(test_handler))
        .layer(axum::middleware::from_fn(move |req, next| {
            mime_validation_middleware(validation_map.clone(), req, next)
        }));

    // Test: Send request with JSON body (even without content-type, should work if no validation)
    let request = Request::builder()
        .method("POST")
        .uri("/tests/v1/public")
        .header("content-type", "application/json") // Add content-type so handler doesn't fail
        .body(Body::from(r#"{"test": "data"}"#))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();

    // Should pass through (no validation configured)
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_middleware_allows_multiple_content_types() {
    // Setup: operation that allows multiple content types
    let specs = vec![OperationSpec {
        method: Method::POST,
        path: "/tests/v1/flexible".to_owned(),
        operation_id: Some("test:flexible".to_owned()),
        summary: None,
        description: None,
        tags: vec![],
        params: vec![],
        request_body: None,
        responses: vec![],
        handler_id: "test".to_owned(),
        authenticated: false,
        exposed: false,
        license_requirement: None,
        throttling: None,
        allowed_request_content_types: Some(vec![
            "application/json",
            "application/xml",
            "text/plain",
        ]),
        vendor_extensions: VendorExtensions::default(),
    }];

    let validation_map = build_mime_validation_map(&specs);

    let app = Router::new()
        .route("/tests/v1/flexible", post(test_handler))
        .layer(axum::middleware::from_fn(move |req, next| {
            mime_validation_middleware(validation_map.clone(), req, next)
        }));

    // Test: application/json should work
    let request = Request::builder()
        .method("POST")
        .uri("/tests/v1/flexible")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"test": "data"}"#))
        .unwrap();

    let response = ServiceExt::<Request<Body>>::oneshot(app.clone(), request)
        .await
        .unwrap();

    assert_eq!(
        response.status(),
        StatusCode::OK,
        "Content-Type application/json should be allowed"
    );

    // Test: Disallowed type should fail
    let request = Request::builder()
        .method("POST")
        .uri("/tests/v1/flexible")
        .header("content-type", "application/octet-stream")
        .body(Body::from("test data"))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(content_type(&response), PROBLEM_JSON);
    let problem = extract_problem(response).await;
    assert_eq!(problem.problem_type, INVALID_ARGUMENT_TYPE);
}
