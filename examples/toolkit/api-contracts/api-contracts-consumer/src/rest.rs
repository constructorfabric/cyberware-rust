//! Manual REST route for the consumer's charge-proxy endpoint.
//!
//! Mirrors the hand-written `OperationBuilder` pattern used in the provider
//! crate (`api-contracts/src/api/rest/routes.rs`). The handler resolves the
//! consumed `PaymentApi` (via [`ChargeProxyService`]) and forwards the charge.

use std::sync::Arc;

use axum::http::StatusCode;
use axum::{Extension, Json, Router};
use toolkit::api::OpenApiRegistry;
use toolkit::api::operation_builder::OperationBuilder;
use toolkit_security::SecurityContext;

use api_contracts_sdk::models::{ChargeRequest, ChargeResponse};

use crate::domain::ChargeProxyService;

const API_TAG: &str = "API Contracts \u{2014} Consumer";

/// Register the consumer's single proxy route on the given router.
///
/// `POST /api-contracts-consumer/v1/charge` forwards to the consumed
/// `PaymentApi`. The domain service is layered by value into the handler closure
/// (per request it resolves `Arc<dyn PaymentApi>` from the hub).
#[allow(clippy::needless_pass_by_value)]
pub fn register_routes(
    router: Router,
    openapi: &dyn OpenApiRegistry,
    service: Arc<ChargeProxyService>,
) -> Router {
    OperationBuilder::post("/api-contracts-consumer/v1/charge")
        .operation_id("api_contracts_consumer.charge")
        .summary("Charge via the consumed PaymentApi")
        .description(
            "Forwards a charge to the `PaymentApi` contract resolved from the \
             ClientHub. The backing provider may be in-process (local-wins) or a \
             remote REST service; the consumer code is identical either way.",
        )
        .tag(API_TAG)
        .authenticated()
        .exposed()
        .no_license_required()
        .json_request::<ChargeRequest>(openapi, "")
        .handler({
            let service = Arc::clone(&service);
            move |Extension(ctx): Extension<SecurityContext>, Json(req): Json<ChargeRequest>| {
                let service = Arc::clone(&service);
                async move { service.place_charge(ctx, req).await.map(Json) }
            }
        })
        .json_response_with_schema::<ChargeResponse>(openapi, StatusCode::OK, "")
        .standard_errors(openapi)
        .register(router, openapi)
}
