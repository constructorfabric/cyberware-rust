//! End-to-end dispatcher tests (`eb-dispatcher-routing` §2/§3) driven
//! through the real `register_dispatcher_routes` router against genuine
//! mock HTTP instances registered via the `test_support` fake
//! `DirectoryClient` (`test_support::{standalone_event_broker_cluster,
//! test_directory_client, mock_instance}`) - not hand-rolled fakes.
//!
//! `classifies_every_endpoint_to_the_correct_role` doubles as the "no
//! instance registered" `503` case (task 3.4, spec "Forwarding error
//! semantics") since nothing is registered against a fresh `ClientHub` -
//! the `503` detail (which names the resolved role's service name) is what
//! makes classification observable through real `forward()` behavior.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::Extension;
use axum::Router;
use axum::body::{Body, Bytes};
use axum::http::{Method, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use http_body_util::BodyExt;
use tokio_stream::StreamExt;
use toolkit::api::OpenApiRegistry;
use toolkit::api::operation_builder::OperationSpec;
use toolkit::directory::{
    DirectoryClient, LabelSelector, RegisterInstanceInfo, ServiceEndpoint, ServiceInstanceInfo,
};
use tower::ServiceExt;
use uuid::Uuid;

use super::register_dispatcher_routes;
use crate::infra::dispatcher::DispatcherState;

struct NoopOpenApiRegistry;

impl OpenApiRegistry for NoopOpenApiRegistry {
    fn register_operation(&self, _spec: &OperationSpec) {}

    fn ensure_schema_raw(
        &self,
        name: &str,
        _schemas: Vec<(
            String,
            utoipa::openapi::RefOr<utoipa::openapi::schema::Schema>,
        )>,
    ) -> String {
        name.to_owned()
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

fn build_router(state: Arc<DispatcherState>) -> Router {
    register_dispatcher_routes(Router::new(), &NoopOpenApiRegistry).layer(Extension(state))
}

/// Drives `method path` through `router` and returns the response body as
/// text (for asserting on the `Problem` `detail`).
async fn response_text(router: Router, method: Method, path: &str) -> (StatusCode, String) {
    let request = Request::builder()
        .method(method)
        .uri(path)
        .body(Body::empty())
        .expect("request must build");
    let response = router
        .oneshot(request)
        .await
        .expect("the service itself must not error");
    let status = response.status();
    let body = response
        .into_body()
        .collect()
        .await
        .expect("body must collect")
        .to_bytes();
    (status, String::from_utf8_lossy(&body).into_owned())
}

#[tokio::test]
async fn classifies_every_endpoint_to_the_correct_role() {
    // DESIGN.md:1354-1392's routing table: 5 Ingest + 11 Delivery + 3 Shared
    // (Shared forwards to Ingest - design.md D2). Expected role is the
    // `DirectoryService` gear name `forward()` resolves (design.md D4),
    // minus the `"event-broker-"` prefix `forward()` adds internally.
    let cases: &[(Method, &str, &str)] = &[
        (Method::POST, "/event-broker/v1/events", "ingest"),
        (Method::POST, "/event-broker/v1/events:batch", "ingest"),
        (Method::POST, "/event-broker/v1/producers", "ingest"),
        (
            Method::GET,
            "/event-broker/v1/producers/p1/cursors",
            "ingest",
        ),
        (
            Method::POST,
            "/event-broker/v1/producers/p1:reset",
            "ingest",
        ),
        (Method::GET, "/event-broker/v1/events:stream", "delivery"),
        (Method::GET, "/event-broker/v1/events:sse", "delivery"),
        (Method::POST, "/event-broker/v1/consumer-groups", "delivery"),
        (Method::GET, "/event-broker/v1/consumer-groups", "delivery"),
        (
            Method::GET,
            "/event-broker/v1/consumer-groups/g1",
            "delivery",
        ),
        (
            Method::DELETE,
            "/event-broker/v1/consumer-groups/g1",
            "delivery",
        ),
        (Method::POST, "/event-broker/v1/subscriptions", "delivery"),
        (Method::GET, "/event-broker/v1/subscriptions", "delivery"),
        (Method::GET, "/event-broker/v1/subscriptions/s1", "delivery"),
        (
            Method::DELETE,
            "/event-broker/v1/subscriptions/s1",
            "delivery",
        ),
        (
            Method::POST,
            "/event-broker/v1/subscriptions/s1:seek",
            "delivery",
        ),
        (Method::GET, "/event-broker/v1/topics", "ingest"),
        (Method::GET, "/event-broker/v1/topics/segments", "ingest"),
        (Method::GET, "/event-broker/v1/event-types", "ingest"),
    ];
    assert_eq!(
        cases.len(),
        19,
        "DESIGN.md:1354-1392 lists 19 endpoints - update this table deliberately if that changes"
    );

    let (hub, _cluster) = crate::test_support::standalone_event_broker_cluster().await;
    let directory = crate::test_support::test_directory_client(&hub);
    let state = Arc::new(DispatcherState::new(directory));

    for (method, path, expected_role) in cases.iter().cloned() {
        let router = build_router(Arc::clone(&state));
        let (status, body) = response_text(router, method.clone(), path).await;
        assert_eq!(
            status,
            StatusCode::SERVICE_UNAVAILABLE,
            "{method} {path} - nothing is registered, expected 503; body: {body}"
        );
        let expected_detail = format!("no event-broker-{expected_role} instance registered");
        assert!(
            body.contains(&expected_detail),
            "{method} {path} expected detail to contain {expected_detail:?}, body was: {body}"
        );
    }
}

#[tokio::test]
async fn forwards_to_the_registered_instance() {
    let (hub, _cluster) = crate::test_support::standalone_event_broker_cluster().await;
    let directory = crate::test_support::test_directory_client(&hub);
    let mock_router = Router::new().route(
        "/event-broker/v1/topics",
        get(|| async { "mock topics response" }),
    );
    let mock =
        crate::test_support::mock_instance(&directory, "event-broker-ingest", mock_router).await;

    let state = Arc::new(DispatcherState::new(directory));
    let router = build_router(state);
    let (status, body) = response_text(router, Method::GET, "/event-broker/v1/topics").await;

    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body, "mock topics response");
    drop(mock);
}

#[tokio::test]
async fn random_selection_reaches_both_registered_instances() {
    let (hub, _cluster) = crate::test_support::standalone_event_broker_cluster().await;
    let directory = crate::test_support::test_directory_client(&hub);
    let router_a = Router::new().route("/event-broker/v1/subscriptions", get(|| async { "A" }));
    let router_b = Router::new().route("/event-broker/v1/subscriptions", get(|| async { "B" }));
    let mock_a =
        crate::test_support::mock_instance(&directory, "event-broker-delivery", router_a).await;
    let mock_b =
        crate::test_support::mock_instance(&directory, "event-broker-delivery", router_b).await;

    let state = Arc::new(DispatcherState::new(directory));
    let (mut seen_a, mut seen_b) = (false, false);
    for _ in 0..40 {
        let router = build_router(Arc::clone(&state));
        let (status, body) =
            response_text(router, Method::GET, "/event-broker/v1/subscriptions").await;
        assert_eq!(status, StatusCode::OK, "body: {body}");
        match body.as_str() {
            "A" => seen_a = true,
            "B" => seen_b = true,
            other => panic!("unexpected response body: {other}"),
        }
        if seen_a && seen_b {
            break;
        }
    }

    assert!(
        seen_a && seen_b,
        "expected both mock instances to receive at least one request across 40 tries \
         (seen_a={seen_a}, seen_b={seen_b})"
    );
    drop((mock_a, mock_b));
}

#[tokio::test]
#[tracing_test::traced_test]
async fn returns_503_with_distinct_detail_when_resolved_instance_is_unreachable() {
    let (hub, _cluster) = crate::test_support::standalone_event_broker_cluster().await;
    let directory = crate::test_support::test_directory_client(&hub);

    // Bind then immediately drop: a concrete, closed target that refuses
    // connections - unlike a merely-unassigned port, whose OS behavior
    // varies by platform.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("binding an ephemeral local port must not fail");
    let addr = listener
        .local_addr()
        .expect("bound listener has an address");
    drop(listener);

    directory
        .register_instance(
            RegisterInstanceInfo::new("event-broker-ingest".to_owned(), Uuid::new_v4().to_string())
                .with_rest_endpoint(ServiceEndpoint::new(format!("http://{addr}"))),
        )
        .await
        .expect("registering an instance address must not fail");

    let state = Arc::new(DispatcherState::new(directory));
    let router = build_router(state);
    let (status, body) = response_text(router, Method::GET, "/event-broker/v1/topics").await;

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "body: {body}");
    assert!(
        body.contains("resolved event-broker-ingest instance unreachable"),
        "body: {body}"
    );
    assert!(
        !body.contains("no event-broker-ingest instance registered"),
        "must be the distinct unreachable detail, not the no-instance-registered one; body: {body}"
    );
    assert!(
        logs_contain("forwarding to resolved instance failed"),
        "the connection failure must be logged before the 503 is returned"
    );
}

/// Emits three heartbeat bytes a little apart, then goes silent (holding the
/// connection open) rather than ever completing the response - simulating a
/// long-poll/SSE upstream (`DESIGN.md:1825`'s heartbeat-cadence framing).
async fn heartbeat_then_silence() -> Response {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(4);
    tokio::spawn(async move {
        for _ in 0..3 {
            tokio::time::sleep(Duration::from_millis(30)).await;
            if tx.send(Ok(Bytes::from_static(b"."))).await.is_err() {
                return;
            }
        }
        tokio::time::sleep(Duration::from_hours(1)).await;
    });
    let body = Body::from_stream(tokio_stream::wrappers::ReceiverStream::new(rx));
    (StatusCode::OK, body).into_response()
}

#[tokio::test]
async fn idle_timeout_closes_connection_after_silence_but_not_between_heartbeats() {
    let (hub, _cluster) = crate::test_support::standalone_event_broker_cluster().await;
    let directory = crate::test_support::test_directory_client(&hub);
    let mock_router = Router::new().route(
        "/event-broker/v1/events:stream",
        get(heartbeat_then_silence),
    );
    let mock =
        crate::test_support::mock_instance(&directory, "event-broker-delivery", mock_router).await;

    // Heartbeats arrive every 30ms; a 150ms idle window must survive each
    // one but close shortly after they stop (spec "Idle-timeout on proxied
    // streaming connections").
    let state =
        Arc::new(DispatcherState::new(directory).with_idle_timeout(Duration::from_millis(150)));
    let router = build_router(state);
    let request = Request::builder()
        .method(Method::GET)
        .uri("/event-broker/v1/events:stream")
        .body(Body::empty())
        .expect("request must build");
    let response = router
        .oneshot(request)
        .await
        .expect("the service itself must not error");
    assert_eq!(response.status(), StatusCode::OK);

    let mut stream = response.into_body().into_data_stream();
    let mut heartbeat_bytes = 0usize;
    let drained = tokio::time::timeout(Duration::from_secs(2), async {
        while let Some(frame) = stream.next().await {
            match frame {
                Ok(chunk) => heartbeat_bytes += chunk.len(),
                Err(_) => break,
            }
        }
    })
    .await;

    assert!(
        drained.is_ok(),
        "the idle-timeout must close the connection well within 2s of the last heartbeat, \
         not hang forever"
    );
    assert!(
        heartbeat_bytes >= 1,
        "expected at least one heartbeat byte to arrive before the connection closed"
    );
    drop(mock);
}

/// A running mock ingest/delivery instance whose directory registration is
/// a hostname (`localhost:<port>`), not an IP literal - so resolving it
/// exercises `proxy_client`'s `tokio::net::lookup_host` fallback path
/// instead of the fast literal `SocketAddr::parse`, mirroring exactly what
/// the real Directory service accepts from a hostname-configured
/// `advertise_addr` (design.md's "Hostname resolution is not a latent edge
/// case").
async fn mock_instance_with_hostname_endpoint(
    directory: &Arc<dyn DirectoryClient>,
    gear_name: &str,
    router: Router,
) -> (
    tokio::task::JoinHandle<()>,
    tokio_util::sync::CancellationToken,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("binding an ephemeral local port must not fail");
    let port = listener
        .local_addr()
        .expect("a just-bound listener has a local address")
        .port();

    let shutdown = tokio_util::sync::CancellationToken::new();
    let shutdown_signal = shutdown.clone();
    let server = tokio::spawn(async move {
        axum::serve(listener, router)
            .with_graceful_shutdown(async move { shutdown_signal.cancelled().await })
            .await
            .expect("mock instance server must not error");
    });

    directory
        .register_instance(
            RegisterInstanceInfo::new(gear_name.to_owned(), Uuid::new_v4().to_string())
                .with_rest_endpoint(ServiceEndpoint::new(format!("http://localhost:{port}"))),
        )
        .await
        .expect("registering the hostname-advertised mock instance must not fail");

    (server, shutdown)
}

#[tokio::test]
async fn hostname_advertised_endpoint_resolves_and_forwards() {
    let (hub, _cluster) = crate::test_support::standalone_event_broker_cluster().await;
    let directory = crate::test_support::test_directory_client(&hub);
    let mock_router = Router::new().route(
        "/event-broker/v1/topics",
        get(|| async { "hostname mock response" }),
    );
    let (server, shutdown) =
        mock_instance_with_hostname_endpoint(&directory, "event-broker-ingest", mock_router).await;

    let state = Arc::new(DispatcherState::new(directory));
    let router = build_router(state);
    let (status, body) = response_text(router, Method::GET, "/event-broker/v1/topics").await;

    assert_eq!(
        status,
        StatusCode::OK,
        "a hostname-advertised instance must resolve via tokio::net::lookup_host and forward \
         successfully instead of 503ing; body: {body}"
    );
    assert_eq!(body, "hostname mock response");

    shutdown.cancel();
    server.abort();
}

#[tokio::test]
#[tracing_test::traced_test]
async fn unresolvable_hostname_returns_503_without_panicking_and_logs_the_failure() {
    let (hub, _cluster) = crate::test_support::standalone_event_broker_cluster().await;
    let directory = crate::test_support::test_directory_client(&hub);

    // `.invalid` is reserved (RFC 2606) to never resolve - a deterministic
    // NXDOMAIN, not a flaky real-network dependency.
    directory
        .register_instance(
            RegisterInstanceInfo::new("event-broker-ingest".to_owned(), Uuid::new_v4().to_string())
                .with_rest_endpoint(ServiceEndpoint::new(
                    "http://this-host-should-never-resolve.invalid:9999".to_owned(),
                )),
        )
        .await
        .expect("registering an unresolvable-hostname instance must not fail");

    let state = Arc::new(DispatcherState::new(directory));
    let router = build_router(state);
    let (status, body) = response_text(router, Method::GET, "/event-broker/v1/topics").await;

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "body: {body}");
    assert!(
        body.contains("resolved event-broker-ingest instance unreachable"),
        "body: {body}"
    );
    assert!(
        logs_contain("forwarding to resolved instance failed"),
        "the resolution failure must be logged, not silently discarded"
    );
}

#[tokio::test]
async fn https_advertised_endpoint_is_rejected_without_connecting() {
    let (hub, _cluster) = crate::test_support::standalone_event_broker_cluster().await;
    let directory = crate::test_support::test_directory_client(&hub);

    // A real, live, plaintext-listening server - if the dispatcher silently
    // downgraded the https-advertised endpoint to a plaintext connection
    // instead of rejecting it, this would actually answer and `hits` would
    // be nonzero.
    let hits = Arc::new(AtomicUsize::new(0));
    let hits_for_handler = Arc::clone(&hits);
    let mock_router = Router::new().route(
        "/event-broker/v1/topics",
        get(move || {
            let hits = Arc::clone(&hits_for_handler);
            async move {
                hits.fetch_add(1, Ordering::SeqCst);
                "should never be reached"
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("binding an ephemeral local port must not fail");
    let addr = listener
        .local_addr()
        .expect("a just-bound listener has a local address");
    let shutdown = tokio_util::sync::CancellationToken::new();
    let shutdown_signal = shutdown.clone();
    let server = tokio::spawn(async move {
        axum::serve(listener, mock_router)
            .with_graceful_shutdown(async move { shutdown_signal.cancelled().await })
            .await
            .expect("mock server must not error");
    });

    directory
        .register_instance(
            RegisterInstanceInfo::new("event-broker-ingest".to_owned(), Uuid::new_v4().to_string())
                .with_rest_endpoint(ServiceEndpoint::new(format!("https://{addr}"))),
        )
        .await
        .expect("registering an https-advertised instance must not fail");

    let state = Arc::new(DispatcherState::new(directory));
    let router = build_router(state);
    let (status, body) = response_text(router, Method::GET, "/event-broker/v1/topics").await;

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "body: {body}");
    assert!(
        body.contains("resolved event-broker-ingest instance unreachable"),
        "body: {body}"
    );
    assert_eq!(
        hits.load(Ordering::SeqCst),
        0,
        "the dispatcher must never have connected to the https-advertised endpoint"
    );

    shutdown.cancel();
    server.abort();
}

#[tokio::test]
async fn oversized_proxied_body_is_rejected_and_a_body_at_the_limit_forwards() {
    let (hub, _cluster) = crate::test_support::standalone_event_broker_cluster().await;
    let directory = crate::test_support::test_directory_client(&hub);
    let mock_router =
        Router::new().route("/event-broker/v1/producers", post(|| async { "accepted" }));
    let mock =
        crate::test_support::mock_instance(&directory, "event-broker-ingest", mock_router).await;
    let state = Arc::new(DispatcherState::new(directory));

    let oversized = vec![0u8; crate::config::MAX_REQUEST_BODY_BYTES + 1];
    let router = build_router(Arc::clone(&state));
    let request = Request::builder()
        .method(Method::POST)
        .uri("/event-broker/v1/producers")
        .body(Body::from(oversized))
        .expect("request must build");
    let response = router
        .oneshot(request)
        .await
        .expect("the service itself must not error");
    assert_eq!(
        response.status(),
        StatusCode::PAYLOAD_TOO_LARGE,
        "a body exceeding MAX_REQUEST_BODY_BYTES must be rejected with 413, not forwarded"
    );

    let at_limit = vec![0u8; crate::config::MAX_REQUEST_BODY_BYTES];
    let router = build_router(state);
    let request = Request::builder()
        .method(Method::POST)
        .uri("/event-broker/v1/producers")
        .body(Body::from(at_limit))
        .expect("request must build");
    let response = router
        .oneshot(request)
        .await
        .expect("the service itself must not error");
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "a body exactly at MAX_REQUEST_BODY_BYTES must forward unchanged"
    );

    drop(mock);
}

/// A `DirectoryClient` double whose `resolve_rest_service` always fails with
/// a generic (non-`DirectoryNotFound`) error - `LocalDirectoryClient` itself
/// has no failure mode other than "not found" (it's a pure in-memory read),
/// so exercising the "directory backend genuinely broken" branch of
/// `forward()`'s error mapping needs a dedicated double, not a second
/// general-purpose `DirectoryClient` fake.
struct AlwaysErroringDirectory;

#[async_trait::async_trait]
impl DirectoryClient for AlwaysErroringDirectory {
    async fn resolve_grpc_service(&self, _service_name: &str) -> anyhow::Result<ServiceEndpoint> {
        anyhow::bail!("directory backend unavailable")
    }

    async fn resolve_rest_service(&self, _gear_name: &str) -> anyhow::Result<ServiceEndpoint> {
        anyhow::bail!("directory backend timed out")
    }

    async fn get_openapi_spec(&self, _gear_name: &str) -> anyhow::Result<String> {
        anyhow::bail!("directory backend unavailable")
    }

    async fn list_instances(&self, _gear: &str) -> anyhow::Result<Vec<ServiceInstanceInfo>> {
        anyhow::bail!("directory backend unavailable")
    }

    async fn resolve_by_labels(
        &self,
        _gear: &str,
        _selector: &LabelSelector,
    ) -> anyhow::Result<Vec<ServiceInstanceInfo>> {
        anyhow::bail!("directory backend unavailable")
    }

    async fn list_all_instances(&self) -> anyhow::Result<Vec<ServiceInstanceInfo>> {
        anyhow::bail!("directory backend unavailable")
    }

    async fn register_instance(&self, _info: RegisterInstanceInfo) -> anyhow::Result<()> {
        Ok(())
    }

    async fn deregister_instance(&self, _gear: &str, _instance_id: &str) -> anyhow::Result<()> {
        Ok(())
    }

    async fn send_heartbeat(&self, _gear: &str, _instance_id: &str) -> anyhow::Result<()> {
        Ok(())
    }
}

#[tokio::test]
#[tracing_test::traced_test]
async fn directory_backend_failure_is_logged_and_differs_from_not_found() {
    let state = Arc::new(DispatcherState::new(
        Arc::new(AlwaysErroringDirectory) as Arc<dyn DirectoryClient>
    ));
    let router = build_router(state);
    let (status, body) = response_text(router, Method::GET, "/event-broker/v1/topics").await;

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "body: {body}");
    assert!(
        body.contains("service discovery unavailable for event-broker-ingest instances"),
        "a generic directory failure must not be conflated with the \"not found\" detail; \
         body: {body}"
    );
    assert!(
        !body.contains("no event-broker-ingest instance registered"),
        "must be the distinct \"discovery unavailable\" detail, not the no-instance-registered \
         one; body: {body}"
    );
    assert!(
        logs_contain("directory backend timed out"),
        "the real underlying directory error must be logged, not just its DirectoryNotFound-ness"
    );
}
