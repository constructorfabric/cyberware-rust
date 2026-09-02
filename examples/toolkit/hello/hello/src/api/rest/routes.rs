//! Route registration for the hello gear.

use axum::Router;
use http::StatusCode;

use toolkit::api::{OpenApiRegistry, OperationBuilder};

use super::dto::PingResponse;
use super::handlers;

/// Register all REST routes for the hello gear.
pub fn register_routes(router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    // GET /hello/v1/ping
    //   * `.exposed()`   -> registered as a public route at the api-gateway edge
    //                       (proxied to this OoP pod).
    //   * `.anonymous()` -> no bearer token required.
    OperationBuilder::get("/hello/v1/ping")
        .operation_id("hello.ping")
        .summary("Liveness ping")
        .description("Returns `pong` and the id of the serving process.")
        .tag("Hello")
        .exposed()
        .anonymous()
        .handler(handlers::handle_ping)
        .json_response_with_schema::<PingResponse>(openapi, StatusCode::OK, "Pong response")
        .register(router, openapi)
}
