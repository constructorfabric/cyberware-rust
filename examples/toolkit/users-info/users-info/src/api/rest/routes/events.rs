use super::{License, dto, handlers};
use axum::Router;
use std::time::Duration;
use toolkit::api::OpenApiRegistry;
use toolkit::api::operation_builder::OperationBuilder;
use tower_http::timeout::TimeoutLayer;

const API_TAG: &str = "Users Info Events";

pub(super) fn register_sse_route<S>(
    router: Router<S>,
    openapi: &dyn OpenApiRegistry,
    sse: toolkit::SseBroadcaster<dto::UserEvent>,
) -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    // Register the SSE route
    let router = OperationBuilder::get("/users-info/v1/users/events")
        .operation_id("users_info.events")
        .authenticated()
        .exposed()
        .require_license_features::<License>([])
        .summary("User events stream (SSE)")
        .description("Real-time stream of user events as Server-Sent Events")
        .tag(API_TAG)
        .handler(handlers::users_events)
        .sse_json::<dto::UserEvent>(openapi, "SSE stream of UserEvent")
        .register(router, openapi);

    // Apply layers for the specific route
    router
        .layer(axum::Extension(sse))
        .layer(TimeoutLayer::with_status_code(
            axum::http::StatusCode::GATEWAY_TIMEOUT,
            Duration::from_hours(1),
        ))
}
