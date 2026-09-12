#![allow(clippy::unwrap_used, clippy::expect_used)]

//! End-to-end tests for the `GET /gear-orchestrator/v1/gears` REST endpoint.
//!
//! These tests build a real axum `Router` with the gear orchestrator's routes
//! registered via `OperationBuilder`, then send HTTP requests using `tower::ServiceExt::oneshot`.

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use gear_orchestrator::api::rest;
use std::sync::Arc;
use toolkit::registry::RegistryBuilder;
use toolkit::runtime::{Endpoint, GearInstance, GearManager};
use tower::ServiceExt;
use uuid::Uuid;

use gear_orchestrator::domain::service::GearsService;

// ---- Test helpers ----

#[derive(Default)]
struct DummyCore;
#[async_trait::async_trait]
impl toolkit::Gear for DummyCore {
    async fn init(&self, _ctx: &toolkit::context::GearCtx) -> anyhow::Result<()> {
        Ok(())
    }
}

#[derive(Default, Clone)]
struct DummyRest;
impl toolkit::contracts::RestApiCapability for DummyRest {
    fn register_rest(
        &self,
        _ctx: &toolkit::context::GearCtx,
        _router: axum::Router,
        _openapi: &dyn toolkit::api::OpenApiRegistry,
    ) -> anyhow::Result<axum::Router> {
        Ok(axum::Router::new())
    }
}

#[derive(Default)]
struct DummySystem;
#[async_trait::async_trait]
impl toolkit::contracts::SystemCapability for DummySystem {}

// (name, deps, has_rest, has_system)
type GearSpec = (&'static str, &'static [&'static str], bool, bool);

fn build_router_with(gears: &[GearSpec], manager: Arc<GearManager>) -> Router {
    let mut b = RegistryBuilder::default();
    for &(name, deps, has_rest, has_system) in gears {
        b.register_core_with_meta(name, deps, Arc::new(DummyCore));
        if has_rest {
            b.register_rest_with_meta(name, Arc::new(DummyRest));
        }
        if has_system {
            b.register_system_with_meta(name, Arc::new(DummySystem));
        }
    }
    let registry = b.build_topo_sorted().unwrap();

    let svc = Arc::new(GearsService::new(&registry, manager));
    let openapi = api_gateway::ApiGateway::default();
    rest::routes::register_routes(Router::new(), &openapi, svc)
}

async fn get_gears(router: Router) -> (StatusCode, serde_json::Value) {
    let response = router
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/gear-orchestrator/v1/gears")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    (status, json)
}

#[tokio::test]
async fn returns_200_with_empty_catalog() {
    let router = build_router_with(&[], Arc::new(GearManager::new()));

    let (status, json) = get_gears(router).await;

    assert_eq!(status, StatusCode::OK);
    assert!(json.as_array().unwrap().is_empty());
}

#[tokio::test]
async fn returns_compiled_in_gears_with_capabilities() {
    let router = build_router_with(
        &[
            ("api_gateway", &[], true, true),
            ("grpc_hub", &[], false, false),
        ],
        Arc::new(GearManager::new()),
    );

    let (status, json) = get_gears(router).await;

    assert_eq!(status, StatusCode::OK);
    let gears = json.as_array().unwrap();
    assert_eq!(gears.len(), 2);

    // Sorted by name
    assert_eq!(gears[0]["name"], "api_gateway");
    assert_eq!(gears[0]["deployment_mode"], "compiled_in");
    assert!(
        gears[0]["capabilities"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("rest"))
    );
    assert!(
        gears[0]["capabilities"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("system"))
    );

    assert_eq!(gears[1]["name"], "grpc_hub");
    assert_eq!(gears[1]["deployment_mode"], "compiled_in");
}

#[tokio::test]
async fn dynamic_instances_without_catalog_entry_appear_as_out_of_process() {
    let manager = Arc::new(GearManager::new());
    let instance = Arc::new(GearInstance::new("dynamic_svc", Uuid::new_v4()).with_version("0.5.0"));
    manager.register_instance(instance).unwrap();

    let router = build_router_with(&[], manager);

    let (status, json) = get_gears(router).await;

    assert_eq!(status, StatusCode::OK);
    let gear = &json.as_array().unwrap()[0];
    assert_eq!(gear["name"], "dynamic_svc");
    assert_eq!(gear["deployment_mode"], "out_of_process");
    assert_eq!(gear["version"], "0.5.0");
    assert!(gear["capabilities"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn includes_running_instances_with_grpc_services() {
    let manager = Arc::new(GearManager::new());
    let instance_id = Uuid::new_v4();
    let instance = Arc::new(
        GearInstance::new("my_gear", instance_id)
            .with_version("1.2.3")
            .with_grpc_service("my.Service", Endpoint::http("127.0.0.1", 9000)),
    );
    manager.register_instance(instance).unwrap();

    let router = build_router_with(&[("my_gear", &[], false, false)], manager);

    let (status, json) = get_gears(router).await;

    assert_eq!(status, StatusCode::OK);
    let gear = &json.as_array().unwrap()[0];
    assert_eq!(gear["name"], "my_gear");
    // Gear-level version derived from first instance
    assert_eq!(gear["version"], "1.2.3");

    let instances = gear["instances"].as_array().unwrap();
    assert_eq!(instances.len(), 1);
    assert_eq!(instances[0]["instance_id"], instance_id.to_string());
    assert_eq!(instances[0]["version"], "1.2.3");
    assert_eq!(instances[0]["state"], "registered");
    assert!(
        instances[0]["grpc_services"]["my.Service"]
            .as_str()
            .unwrap()
            .contains("127.0.0.1")
    );
}

#[tokio::test]
async fn plugins_field_omitted_when_empty() {
    let router = build_router_with(&[("test", &[], false, false)], Arc::new(GearManager::new()));

    let (status, json) = get_gears(router).await;

    assert_eq!(status, StatusCode::OK);
    let gear = &json.as_array().unwrap()[0];
    // plugins field should be absent (skip_serializing_if = Vec::is_empty)
    assert!(gear.get("plugins").is_none());
}

#[tokio::test]
async fn version_omitted_when_no_instances() {
    let router = build_router_with(
        &[("no_instances", &[], false, false)],
        Arc::new(GearManager::new()),
    );

    let (status, json) = get_gears(router).await;

    assert_eq!(status, StatusCode::OK);
    let gear = &json.as_array().unwrap()[0];
    // version should be absent when no instances report one
    assert!(gear.get("version").is_none());
}

#[tokio::test]
async fn gears_are_sorted_alphabetically() {
    let router = build_router_with(
        &[
            ("zebra", &[], false, false),
            ("alpha", &[], false, false),
            ("middle", &[], false, false),
        ],
        Arc::new(GearManager::new()),
    );

    let (status, json) = get_gears(router).await;

    assert_eq!(status, StatusCode::OK);
    let names: Vec<&str> = json
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["alpha", "middle", "zebra"]);
}
