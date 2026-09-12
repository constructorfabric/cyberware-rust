//! Route registration. Every operation is declared through `OperationBuilder`
//! so the `OpenAPI` document and the mounted route cannot drift.
//!
//! Errors are declared through `standard_errors`; the `501` of the reserved
//! `rate` type and the `503` of an unready gear are bookkept under `500` per
//! DESIGN section 3.3, the runtime status being the canonical category's.

use std::sync::Arc;

use axum::http::StatusCode;
use axum::{Extension, Router};
use toolkit::api::OpenApiRegistry;
use toolkit::api::operation_builder::{OperationBuilder, ResponseHeaderSpec, ResponseHeaderType};

use super::{dto, handlers};
use crate::domain::Service;

/// Path prefix of every operational route (DESIGN section 3.3).
pub const PATH_PREFIX: &str = "/v1/quota-enforcement";

const API_TAG: &str = "Quotas";

/// Mount the gear's routes and make the service reachable from handlers.
///
/// Owner projections are published by their owning gears directly to the
/// types registry; no QE route registers a contract.
// @cpt-begin:cpt-cf-quota-enforcement-flow-owner-projection-publication:p1:inst-pub-registry
pub fn register_routes(
    router: Router,
    openapi: &dyn OpenApiRegistry,
    service: Arc<Service>,
) -> Router {
    let router = register_quota_routes(router, openapi);
    router.layer(Extension(service))
}
// @cpt-end:cpt-cf-quota-enforcement-flow-owner-projection-publication:p1:inst-pub-registry

/// The five Quota lifecycle endpoints.
// @cpt-flow:cpt-cf-quota-enforcement-flow-quota-create:p1
// @cpt-flow:cpt-cf-quota-enforcement-flow-quota-update:p1
// @cpt-flow:cpt-cf-quota-enforcement-flow-quota-read:p1
// @cpt-dod:cpt-cf-quota-enforcement-dod-quota-crud:p1
fn register_quota_routes(mut router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    // @cpt-begin:cpt-cf-quota-enforcement-flow-quota-create:p1:inst-qcr-request
    router = OperationBuilder::post(format!("{PATH_PREFIX}/quotas"))
        .operation_id("quota_enforcement.create_quota")
        .summary("Create a Quota")
        .description(
            "Validate the draft, authorize its explicit target, resolve the metric owner's \
             constraint contract, and persist the Quota. The reserved `rate` type answers 501.",
        )
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .json_request::<dto::CreateQuotaDto>(openapi, "Quota draft")
        .handler(handlers::create_quota)
        .json_response_with_schema::<dto::QuotaViewDto>(
            openapi,
            StatusCode::CREATED,
            "The created Quota",
        )
        .response_header(ResponseHeaderSpec::new(
            "Location",
            "URI of the created Quota",
            ResponseHeaderType::String,
        ))
        .standard_errors(openapi)
        .register(router, openapi);
    // @cpt-end:cpt-cf-quota-enforcement-flow-quota-create:p1:inst-qcr-request

    // @cpt-begin:cpt-cf-quota-enforcement-flow-quota-read:p1:inst-qrd-request
    router = OperationBuilder::get(format!("{PATH_PREFIX}/quotas/{{id}}"))
        .operation_id("quota_enforcement.get_quota")
        .summary("Read a Quota")
        .description("One Quota within the caller's scope, deactivated ones included.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Quota UUID")
        .handler(handlers::get_quota)
        .json_response_with_schema::<dto::QuotaViewDto>(openapi, StatusCode::OK, "The Quota")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get(format!("{PATH_PREFIX}/quotas"))
        .operation_id("quota_enforcement.list_quotas")
        .summary("List Quotas")
        .description(
            "A filtered, cursor-paginated page of Quotas within the caller's scope, ordered by \
             identifier (creation order). `projection_type` and `subject_id` come together; \
             repeat `id` to name several Quotas.",
        )
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .query_params_from::<dto::ListQuotasQuery>()
        .handler(handlers::list_quotas)
        .json_response_with_schema::<dto::QuotaPageDto>(openapi, StatusCode::OK, "One page")
        .standard_errors(openapi)
        .register(router, openapi);
    // @cpt-end:cpt-cf-quota-enforcement-flow-quota-read:p1:inst-qrd-request

    // @cpt-begin:cpt-cf-quota-enforcement-flow-quota-update:p1:inst-qup-request
    router = OperationBuilder::patch(format!("{PATH_PREFIX}/quotas/{{id}}"))
        .operation_id("quota_enforcement.update_quota")
        .summary("Update a Quota")
        .description(
            "Apply a non-breaking patch. Metric, type, period, and subject are immutable: a \
             patch naming one answers 400 IMMUTABLE_FIELD, one naming the reserved `rate` type \
             answers 501. A cap below the consumed amount answers 400 CAP_BELOW_CONSUMED.",
        )
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Quota UUID")
        .json_request::<dto::UpdateQuotaDto>(openapi, "Mutable-fields patch")
        .handler(handlers::update_quota)
        .json_response_with_schema::<dto::QuotaViewDto>(
            openapi,
            StatusCode::OK,
            "The updated Quota",
        )
        .standard_errors(openapi)
        .register(router, openapi);
    // @cpt-end:cpt-cf-quota-enforcement-flow-quota-update:p1:inst-qup-request

    router = OperationBuilder::post(format!("{PATH_PREFIX}/quotas/{{id}}/deactivate"))
        .operation_id("quota_enforcement.deactivate_quota")
        .summary("Deactivate a Quota")
        .description(
            "Mark the Quota deactivated and resolve its active leases in the same transaction. \
             The record stays readable; a second deactivation answers 400 QUOTA_DEACTIVATED.",
        )
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Quota UUID")
        .handler(handlers::deactivate_quota)
        .json_response_with_schema::<dto::DeactivateOutcomeDto>(
            openapi,
            StatusCode::OK,
            "The resolved leases",
        )
        .standard_errors(openapi)
        .register(router, openapi);

    router
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "routes_tests.rs"]
mod routes_tests;
