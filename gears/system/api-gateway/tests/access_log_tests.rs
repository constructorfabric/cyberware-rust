#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::{
    Router,
    body::Body,
    extract::ConnectInfo,
    http::{Request, StatusCode},
    middleware::from_fn,
    response::IntoResponse,
    routing::get,
};
use serde_json::json;
use toolkit::{
    Gear, api::OperationBuilder, config::ConfigProvider, context::GearCtx,
    contracts::ApiGatewayCapability,
};
use tower::util::ServiceExt;
use tower_http::request_id::{PropagateRequestIdLayer, SetRequestIdLayer};
use tracing_subscriber::layer::SubscriberExt;
use uuid::Uuid;

use api_gateway::middleware::request_id::{MakeReqId, header};

/// Captured access log event fields.
#[derive(Debug, Default, Clone)]
struct CapturedEvent {
    target: String,
    fields: std::collections::HashMap<String, String>,
}

/// A tracing layer that captures events with target `access_log`.
#[derive(Clone, Default)]
struct CapturingLayer {
    events: Arc<Mutex<Vec<CapturedEvent>>>,
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for CapturingLayer {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        if event.metadata().target() != "access_log" {
            return;
        }
        let mut captured = CapturedEvent {
            target: event.metadata().target().to_owned(),
            ..Default::default()
        };
        let mut visitor = FieldVisitor(&mut captured.fields);
        event.record(&mut visitor);
        self.events.lock().unwrap().push(captured);
    }
}

struct FieldVisitor<'a>(&'a mut std::collections::HashMap<String, String>);

impl tracing::field::Visit for FieldVisitor<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.0.insert(field.name().to_owned(), format!("{value:?}"));
    }
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.0.insert(field.name().to_owned(), value.to_owned());
    }
    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        self.0.insert(field.name().to_owned(), value.to_string());
    }
    fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
        self.0.insert(field.name().to_owned(), value.to_string());
    }
}

fn test_app() -> Router {
    let x_request_id = header();

    Router::new()
        .route("/test", get(handler_ok))
        .route("/error", get(handler_err))
        .layer(from_fn(
            api_gateway::middleware::access_log::access_log_middleware,
        ))
        .layer(from_fn(
            api_gateway::middleware::request_id::push_req_id_to_extensions,
        ))
        .layer(PropagateRequestIdLayer::new(x_request_id.clone()))
        .layer(SetRequestIdLayer::new(x_request_id, MakeReqId))
}

async fn handler_ok() -> impl IntoResponse {
    "ok"
}

async fn handler_err() -> impl IntoResponse {
    StatusCode::INTERNAL_SERVER_ERROR
}

/// Run a request against the test app with the capturing layer active.
///
/// The response body is fully consumed so that the counting-body wrapper
/// emits the access log before we return.
async fn run_with_capture(req: Request<Body>) -> (StatusCode, Vec<CapturedEvent>) {
    let layer = CapturingLayer::default();
    let events = layer.events.clone();

    let subscriber = tracing_subscriber::registry().with(layer);
    let _guard = tracing::subscriber::set_default(subscriber);

    let app = test_app();
    let response = app.oneshot(req).await.unwrap();
    let status = response.status();

    // Consume the body so the CountingBody emits the access log.
    let _ = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();

    let captured = events.lock().unwrap().clone();
    (status, captured)
}

/// Like `run_with_capture` but also returns the response body bytes and headers.
async fn run_with_capture_full(
    req: Request<Body>,
) -> (
    StatusCode,
    axum::http::HeaderMap,
    bytes::Bytes,
    Vec<CapturedEvent>,
) {
    let layer = CapturingLayer::default();
    let events = layer.events.clone();

    let subscriber = tracing_subscriber::registry().with(layer);
    let _guard = tracing::subscriber::set_default(subscriber);

    let app = test_app();
    let response = app.oneshot(req).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();

    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();

    let captured = events.lock().unwrap().clone();
    (status, headers, body, captured)
}

#[tokio::test]
async fn emits_access_log_with_expected_fields() {
    let req = Request::builder()
        .uri("/test?q=1")
        .header("x-request-id", "test-rid-42")
        .header("user-agent", "TestAgent/1.0")
        .header("content-length", "128")
        .body(Body::empty())
        .unwrap();

    let (status, events) = run_with_capture(req).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(events.len(), 1, "expected exactly one access_log event");

    let e = &events[0];
    assert_eq!(e.target, "access_log");
    assert_eq!(e.fields.get("method").unwrap(), "GET");
    assert_eq!(e.fields.get("uri").unwrap(), "/test?q=1");
    assert_eq!(e.fields.get("request_id").unwrap(), "test-rid-42");
    assert_eq!(e.fields.get("content_length").unwrap(), "128");
    assert_eq!(e.fields.get("user_agent").unwrap(), "TestAgent/1.0");
    assert_eq!(e.fields.get("status").unwrap(), "200");
    assert_eq!(e.fields.get("msg").unwrap(), "response completed");

    // duration fields are present and non-negative
    assert!(e.fields.contains_key("duration_ms"));
    assert!(e.fields.contains_key("duration"));
    assert!(e.fields.contains_key("pid"));
    assert!(e.fields.contains_key("bytes_sent"));
}

#[tokio::test]
async fn captures_error_status() {
    let req = Request::builder()
        .uri("/error")
        .header("x-request-id", "err-1")
        .body(Body::empty())
        .unwrap();

    let (status, events) = run_with_capture(req).await;

    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].fields.get("status").unwrap(), "500");
    assert_eq!(events[0].fields.get("request_id").unwrap(), "err-1");
}

#[tokio::test]
async fn generates_request_id_when_missing() {
    let req = Request::builder().uri("/test").body(Body::empty()).unwrap();

    let (_, events) = run_with_capture(req).await;

    assert_eq!(events.len(), 1);
    let rid = events[0].fields.get("request_id").unwrap();
    assert!(!rid.is_empty(), "request_id should be auto-generated");
}

#[tokio::test]
async fn extracts_trace_id_from_traceparent() {
    let req = Request::builder()
        .uri("/test")
        .header(
            "traceparent",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
        )
        .body(Body::empty())
        .unwrap();

    let (_, events) = run_with_capture(req).await;

    assert_eq!(events.len(), 1);
    assert_eq!(
        events[0].fields.get("trace_id").unwrap(),
        "4bf92f3577b34da6a3ce929d0e0e4736"
    );
}

#[tokio::test]
async fn does_not_alter_response() {
    let req = Request::builder()
        .uri("/test")
        .header("x-request-id", "passthrough")
        .body(Body::empty())
        .unwrap();

    let (status, headers, body, _events) = run_with_capture_full(req).await;

    assert_eq!(status, StatusCode::OK);
    // x-request-id is propagated by PropagateRequestIdLayer, not altered by access_log
    assert_eq!(
        headers.get("x-request-id").unwrap().to_str().unwrap(),
        "passthrough"
    );
    assert_eq!(&body[..], b"ok");
}

#[tokio::test]
async fn logs_uri_with_query_string_verbatim() {
    let req = Request::builder()
        .uri("/test?user=mike&token=s3cret&page=1")
        .header("x-request-id", "qs-test")
        .body(Body::empty())
        .unwrap();

    let (_, events) = run_with_capture(req).await;

    assert_eq!(events.len(), 1);
    let uri = events[0].fields.get("uri").unwrap();
    assert_eq!(uri, "/test?user=mike&token=s3cret&page=1");
}

#[tokio::test]
async fn logs_uri_without_query_string() {
    let req = Request::builder()
        .uri("/test")
        .header("x-request-id", "no-qs")
        .body(Body::empty())
        .unwrap();

    let (_, events) = run_with_capture(req).await;

    assert_eq!(events.len(), 1);
    assert_eq!(events[0].fields.get("uri").unwrap(), "/test");
}

#[tokio::test]
async fn defaults_for_missing_headers() {
    let req = Request::builder().uri("/test").body(Body::empty()).unwrap();
    // No x-request-id, no user-agent, no content-length, no traceparent

    let (_, events) = run_with_capture(req).await;

    assert_eq!(events.len(), 1);
    let e = &events[0];
    assert_eq!(e.fields.get("content_length").unwrap(), "0");
    assert_eq!(e.fields.get("user_agent").unwrap(), "");
    assert_eq!(e.fields.get("trace_id").unwrap(), "");
    // request_id is auto-generated by SetRequestIdLayer, so still non-empty
    assert!(!e.fields.get("request_id").unwrap().is_empty());
}

// ---------------------------------------------------------------------------
// E2E test: exercises the full ApiGateway::apply_middleware_stack() via
// rest_finalize(), sending a request through the production middleware wiring
// (SetRequestIdLayer, PropagateRequestIdLayer, TraceLayer, push_req_id,
// HttpMetrics, access_log, …) and asserting the access log contains the
// expected remote_addr / remote_addr_port fields.
// ---------------------------------------------------------------------------

struct TestConfigProvider {
    config: serde_json::Value,
}

impl ConfigProvider for TestConfigProvider {
    fn get_gear_config(&self, gear: &str) -> Option<&serde_json::Value> {
        self.config.get(gear)
    }
}

async fn e2e_handler() -> impl IntoResponse {
    "e2e-ok"
}

#[tokio::test]
async fn e2e_full_middleware_stack_logs_remote_addr() -> anyhow::Result<()> {
    let cfg = json!({
        "api-gateway": {
            "config": {
                "bind_addr": "127.0.0.1:0",
                "cors_enabled": false,
                "auth_disabled": true,
            }
        }
    });

    let hub = Arc::new(toolkit::ClientHub::new());
    let ctx = GearCtx::new(
        "api-gateway",
        Uuid::new_v4(),
        Arc::new(TestConfigProvider { config: cfg }),
        hub,
        tokio_util::sync::CancellationToken::new(),
    );

    let api = api_gateway::ApiGateway::default();
    api.init(&ctx).await?;

    let router = OperationBuilder::get("/tests/v1/access-log-e2e")
        .operation_id("test:access-log-e2e")
        .summary("E2E access log test")
        .anonymous()
        .json_response(StatusCode::OK, "OK")
        .handler(get(e2e_handler))
        .register(Router::new(), &api);

    let app = api.rest_finalize(
        &ctx,
        router,
        Arc::new(toolkit::RestHealthcheckRegistry::new()),
    )?;

    // Set up capturing layer for the access log.
    let layer = CapturingLayer::default();
    let events = layer.events.clone();
    let subscriber = tracing_subscriber::registry().with(layer);
    let _guard = tracing::subscriber::set_default(subscriber);

    // Build a request with ConnectInfo<SocketAddr> injected to simulate
    // a real TCP connection (axum normally inserts this via
    // `into_make_service_with_connect_info`).
    let fake_addr: SocketAddr = "192.168.1.42:54321".parse().unwrap();

    let mut req = Request::builder()
        .method("GET")
        .uri("/tests/v1/access-log-e2e?q=hello")
        .header("x-request-id", "e2e-rid-99")
        .header("user-agent", "E2EAgent/2.0")
        .body(Body::empty())?;
    req.extensions_mut().insert(ConnectInfo(fake_addr));

    let response = app.oneshot(req).await?;

    assert_eq!(response.status(), StatusCode::OK);

    // x-request-id is propagated by the real PropagateRequestIdLayer.
    assert_eq!(
        response
            .headers()
            .get("x-request-id")
            .and_then(|v| v.to_str().ok()),
        Some("e2e-rid-99"),
    );

    // Consume body to trigger CountingBody log emission.
    let body_bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await?;
    assert_eq!(&body_bytes[..], b"e2e-ok");

    let captured = events.lock().unwrap().clone();
    assert_eq!(captured.len(), 1, "expected exactly one access_log event");

    let e = &captured[0];
    assert_eq!(e.target, "access_log");
    assert_eq!(e.fields.get("method").unwrap(), "GET");
    assert_eq!(
        e.fields.get("uri").unwrap(),
        "/tests/v1/access-log-e2e?q=hello"
    );
    assert_eq!(e.fields.get("request_id").unwrap(), "e2e-rid-99");
    assert_eq!(e.fields.get("user_agent").unwrap(), "E2EAgent/2.0");
    assert_eq!(e.fields.get("status").unwrap(), "200");

    // remote_addr fields populated from ConnectInfo<SocketAddr>
    assert_eq!(
        e.fields.get("remote_addr").unwrap(),
        "192.168.1.42:54321",
        "remote_addr must reflect ConnectInfo"
    );
    assert_eq!(
        e.fields.get("remote_addr_ip").unwrap(),
        "192.168.1.42",
        "remote_addr_ip must reflect ConnectInfo"
    );
    assert_eq!(
        e.fields.get("remote_addr_port").unwrap(),
        "54321",
        "remote_addr_port must reflect ConnectInfo"
    );

    // bytes_sent should reflect actual body size
    let bytes_sent: u64 = e.fields.get("bytes_sent").unwrap().parse().unwrap();
    assert!(
        bytes_sent > 0,
        "bytes_sent should be non-zero for a response with a body"
    );

    assert!(e.fields.contains_key("duration_ms"));
    assert!(e.fields.contains_key("duration"));
    assert!(e.fields.contains_key("pid"));

    Ok(())
}
