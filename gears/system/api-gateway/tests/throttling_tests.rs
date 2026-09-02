#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Integration tests for zone-based throttling (rate limit + in-flight concurrency).
//!
//! These exercise the gateway's throttling middleware end-to-end: routes bind to
//! named zones via `with_throttling(...)`, and the gateway enforces the limits
//! defined in the `rate_limit_zones` / `in_flight_limit_zones` config sections.

use anyhow::Result;
use async_trait::async_trait;
use axum::{
    Router,
    body::Body,
    extract::Json,
    http::{Request, StatusCode, header},
    routing::get,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::Notify;
use toolkit::{
    Gear, GearCtx, RestApiCapability,
    api::{OperationBuilder, ThrottlingSpec},
    config::ConfigProvider,
    contracts::{ApiGatewayCapability, OpenApiRegistry},
};
use toolkit_canonical_errors::Problem;
use tower::ServiceExt;
use utoipa::ToSchema;
use uuid::Uuid;

const RESOURCE_EXHAUSTED_TYPE: &str =
    "gts://gts.cf.core.errors.err.v1~cf.core.err.resource_exhausted.v1~";
const PROBLEM_JSON: &str = "application/problem+json";

struct TestConfigProvider {
    config: serde_json::Value,
}

impl ConfigProvider for TestConfigProvider {
    fn get_gear_config(&self, gear: &str) -> Option<&serde_json::Value> {
        if gear == "api-gateway" {
            Some(&self.config)
        } else {
            None
        }
    }
}

fn wrap_config(config: &serde_json::Value) -> serde_json::Value {
    serde_json::json!({ "config": config })
}

fn create_test_gear_ctx_with_config(config: &serde_json::Value) -> GearCtx {
    let wrapped_config = wrap_config(config);
    let hub = Arc::new(toolkit::ClientHub::new());

    GearCtx::new(
        "api-gateway",
        Uuid::new_v4(),
        Arc::new(TestConfigProvider {
            config: wrapped_config,
        }),
        hub,
        tokio_util::sync::CancellationToken::new(),
    )
}

#[derive(Serialize, Deserialize, ToSchema, Debug, Clone)]
struct TestResponse {
    message: String,
}

/// Test gear with throttled routes bound to named zones.
///
/// The slow route is coordinated with the test via two [`Notify`] signals so the
/// in-flight test is deterministic (no timing sleeps): `slow_entered` fires once
/// the handler is reached (i.e. the in-flight permit is confirmed held), and the
/// handler blocks on `slow_release` until the test lets it complete.
pub struct ThrottledGear {
    slow_entered: Arc<Notify>,
    slow_release: Arc<Notify>,
}

impl Default for ThrottledGear {
    fn default() -> Self {
        Self {
            slow_entered: Arc::new(Notify::new()),
            slow_release: Arc::new(Notify::new()),
        }
    }
}

#[async_trait]
impl Gear for ThrottledGear {
    async fn init(&self, _ctx: &toolkit::GearCtx) -> Result<()> {
        Ok(())
    }
}

fn ip_throttling(rate_zone: &str, inflight_zone: &str) -> ThrottlingSpec {
    // Empty string means "no zone in this category".
    let zone = |name: &str| (!name.is_empty()).then(|| name.to_owned());
    ThrottlingSpec {
        rate_limit_zone: zone(rate_zone),
        in_flight_limit_zone: zone(inflight_zone),
        require_security_context: false,
        dry_run: false,
    }
}

impl RestApiCapability for ThrottledGear {
    fn register_rest(
        &self,
        _ctx: &toolkit::GearCtx,
        router: axum::Router,
        openapi: &dyn OpenApiRegistry,
    ) -> Result<axum::Router> {
        // Rate-limited route: 1 rps / burst 1, keyed per client IP.
        let router = OperationBuilder::get("/tests/v1/limited")
            .operation_id("test:limited")
            .summary("Rate-limited endpoint")
            .anonymous()
            .with_throttling(ip_throttling("rl_limited", ""))
            .json_response(http::StatusCode::OK, "Success")
            .handler(get(limited_handler))
            .register(router, openapi);

        // In-flight-limited route: 1 concurrent request, no backlog. The handler
        // signals entry (permit held) and blocks until the test releases it, so
        // the second request is issued and rejected while the permit is held.
        let entered = Arc::clone(&self.slow_entered);
        let release = Arc::clone(&self.slow_release);
        let slow = get(move || {
            let entered = Arc::clone(&entered);
            let release = Arc::clone(&release);
            async move {
                entered.notify_one();
                release.notified().await;
                Json(TestResponse {
                    message: "slow".to_owned(),
                })
            }
        });
        let router = OperationBuilder::get("/tests/v1/slow")
            .operation_id("test:slow")
            .summary("Slow endpoint with low in-flight limit")
            .anonymous()
            .with_throttling(ip_throttling("", "ifl_slow"))
            .json_response(http::StatusCode::OK, "Success")
            .handler(slow)
            .register(router, openapi);

        Ok(router)
    }
}

async fn limited_handler() -> Json<TestResponse> {
    Json(TestResponse {
        message: "limited".to_owned(),
    })
}

/// Config with a strict rate-limit zone and a single-slot in-flight zone.
fn throttling_config() -> serde_json::Value {
    serde_json::json!({
        "bind_addr": "127.0.0.1:0",
        "cors_enabled": false,
        "auth_disabled": true,
        "rate_limit_zones": {
            "rl_limited": {
                "rate_limit": "1/s",
                "burst_limit": 1,
                "key": { "type": "ip" },
                "max_keys": 1000
            }
        },
        "in_flight_limit_zones": {
            "ifl_slow": {
                "in_flight_limit": 1,
                "backlog_limit": 0,
                "backlog_timeout": "0s",
                "key": { "type": "ip" },
                "max_keys": 1000
            }
        }
    })
}

async fn finalize_app_with_gear(config: &serde_json::Value, gear: &ThrottledGear) -> Router {
    let api_gateway = api_gateway::ApiGateway::default();
    let ctx = create_test_gear_ctx_with_config(config);
    api_gateway.init(&ctx).await.expect("Failed to init");

    let router = gear
        .register_rest(&ctx, Router::new(), &api_gateway)
        .expect("Failed to register routes");
    api_gateway
        .rest_finalize(
            &ctx,
            router,
            Arc::new(toolkit::RestHealthcheckRegistry::new()),
        )
        .expect("Failed to finalize router")
}

async fn finalize_app(config: &serde_json::Value) -> Router {
    finalize_app_with_gear(config, &ThrottledGear::default()).await
}

#[tokio::test]
async fn test_rate_limit_returns_canonical_problem_with_headers() {
    let app = finalize_app(&throttling_config()).await;

    // First request consumes the only token.
    let res1 = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/tests/v1/limited")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("Request failed");
    assert_eq!(res1.status(), StatusCode::OK);

    // Second request should be rejected with a canonical resource_exhausted Problem.
    let res2 = app
        .oneshot(
            Request::builder()
                .uri("/tests/v1/limited")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("Request failed");

    assert_eq!(res2.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        res2.headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or(""),
        PROBLEM_JSON
    );

    // Rate-limit metadata headers + Retry-After must be present on the 429.
    assert!(
        res2.headers().get("RateLimit-Policy").is_some(),
        "RateLimit-Policy header must be present on 429"
    );
    assert!(
        res2.headers().get("RateLimit-Limit").is_some(),
        "RateLimit-Limit header must be present on 429"
    );
    assert!(
        res2.headers().get("X-RateLimit-Limit").is_some(),
        "X-RateLimit-Limit header must be present on 429"
    );
    assert!(
        res2.headers().get(header::RETRY_AFTER).is_some(),
        "Retry-After header must be present on 429"
    );

    let body = axum::body::to_bytes(res2.into_body(), usize::MAX)
        .await
        .expect("read body");
    let problem: Problem = serde_json::from_slice(&body).expect("parse Problem JSON");
    assert_eq!(problem.problem_type, RESOURCE_EXHAUSTED_TYPE);
    let violations = problem
        .context
        .get("violations")
        .and_then(|v| v.as_array())
        .expect("violations must be present");
    assert_eq!(violations.len(), 1);
    assert_eq!(violations[0]["subject"], "throttling");
}

#[tokio::test]
async fn test_in_flight_limit_rejects_second_concurrent_request() {
    let gear = ThrottledGear::default();
    let slow_entered = Arc::clone(&gear.slow_entered);
    let slow_release = Arc::clone(&gear.slow_release);
    let app = finalize_app_with_gear(&throttling_config(), &gear).await;

    // Start one slow request that holds the only in-flight permit.
    let app_clone = app.clone();
    let first = tokio::spawn(async move {
        app_clone
            .oneshot(
                Request::builder()
                    .uri("/tests/v1/slow")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .expect("first request failed")
    });

    // Deterministic handshake: wait until the slow handler is entered, which
    // only happens after the middleware has acquired the sole in-flight permit.
    slow_entered.notified().await;

    let res2 = app
        .oneshot(
            Request::builder()
                .uri("/tests/v1/slow")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("second request failed");

    // The permit is held throughout the second request; now let the first finish.
    slow_release.notify_one();
    let first_res = first.await.expect("first task panicked");
    assert_eq!(first_res.status(), StatusCode::OK);

    // Second concurrent request is rejected with a canonical resource_exhausted Problem.
    assert_eq!(res2.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        res2.headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or(""),
        PROBLEM_JSON
    );
    assert!(
        res2.headers().get(header::RETRY_AFTER).is_some(),
        "Retry-After header must be present on in-flight rejection"
    );

    let body = axum::body::to_bytes(res2.into_body(), usize::MAX)
        .await
        .expect("read body");
    let problem: Problem = serde_json::from_slice(&body).expect("parse Problem JSON");
    assert_eq!(problem.problem_type, RESOURCE_EXHAUSTED_TYPE);
}
