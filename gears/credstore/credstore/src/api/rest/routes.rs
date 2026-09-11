//! REST route registration for the credstore module (ADR-0004: the
//! credential surface).

use std::sync::Arc;

use axum::Router;
use axum::http::StatusCode;
use toolkit::api::operation_builder::OperationBuilderODataExt;
use toolkit::api::{OpenApiRegistry, OperationBuilder, ParamLocation, ParamSpec};

use super::dto::{
    CredentialDto, CredentialListItemDto, CredentialPatchDto, PutCredentialRequestDto, SecretDto,
};
use super::handlers::{self, ConcreteService};
use crate::domain::secret::list_filter::CredentialFilterField;

const TAG: &str = "Credential Store";

fn if_match_param(required: bool, description: &str) -> ParamSpec {
    ParamSpec {
        name: "If-Match".to_owned(),
        location: ParamLocation::Header,
        required,
        description: Some(description.to_owned()),
        param_type: "string".to_owned(),
        array: false,
    }
}

fn if_none_match_param(description: &str) -> ParamSpec {
    ParamSpec {
        name: "If-None-Match".to_owned(),
        location: ParamLocation::Header,
        required: false,
        description: Some(description.to_owned()),
        param_type: "string".to_owned(),
        array: false,
    }
}

/// Register all REST routes for the credstore module.
pub fn register_routes(
    router: Router,
    openapi: &dyn OpenApiRegistry,
    svc: Arc<ConcreteService>,
) -> Router {
    let router = OperationBuilder::get("/credstore/v1/credentials")
        .operation_id("credstore.list_credentials")
        .summary("List credentials")
        .description(
            "Upward-rooted collection read (ADR-0005): one reduced item per reference, from \
             the caller's tenant and its ancestor chain only. `$filter` accepts `reference`/ \
             `type` (SQL-clamped, eq/in) and `sharing`/`fallback`/`expires_at` (applied after \
             reduction); `$orderby` accepts only `reference`. Selecting `secret` in `$select` \
             switches to value mode (ADR-0004): no `limit`/`cursor`/`$orderby`, a `reference` or \
             `type` selector only, capped and unpaginated.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .query_param_typed("limit", false, "Page size (metadata mode only)", "integer")
        .query_param(
            "cursor",
            false,
            "Opaque continuation token from a previous page",
        )
        .with_odata_filter::<CredentialFilterField>()
        .with_odata_orderby::<CredentialFilterField>()
        .with_odata_select()
        .handler(handlers::list_credentials)
        .json_response_with_schema::<toolkit_odata::Page<CredentialListItemDto>>(
            openapi,
            StatusCode::OK,
            "A page of reduced credential items (value-mode items additionally carry `secret`)",
        )
        .standard_errors(openapi)
        .error_503(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get("/credstore/v1/credentials/{ref}")
        .operation_id("credstore.get_credential")
        .summary("Get a credential record by reference")
        .description(
            "Retrieve the credential record for the authenticated tenant, with walk-up \
             resolution. Never carries the value -- see `.../secret`.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param(
            "ref",
            "Credential reference (`[a-zA-Z0-9_-]+`, maximum length 255 characters)",
        )
        .handler(handlers::get_credential)
        .json_response_with_schema::<CredentialDto>(
            openapi,
            StatusCode::OK,
            "The resolved credential record",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get("/credstore/v1/credentials/{ref}/secret")
        .operation_id("credstore.get_secret")
        .summary("Get a credential's secret value by reference")
        .description(
            "Retrieve the resolved secret value for the authenticated tenant, with walk-up \
             resolution. A winning record with no value (declared, or suppressed) is the \
             canonical 404.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param(
            "ref",
            "Credential reference (`[a-zA-Z0-9_-]+`, maximum length 255 characters)",
        )
        .handler(handlers::get_secret)
        .json_response_with_schema::<SecretDto>(
            openapi,
            StatusCode::OK,
            "The resolved secret value",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    let router = OperationBuilder::put("/credstore/v1/credentials/{ref}")
        .operation_id("credstore.put_credential")
        .summary("Create or replace a credential by reference")
        .description(
            "Whole-credential replace: record and value together, in one call. Exactly one of \
             `If-None-Match: *` (create-only) or `If-Match` (guarded/unconditional replace) is \
             required. `value` is required in the body -- a PUT always carries a value.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param(
            "ref",
            "Credential reference (`[a-zA-Z0-9_-]+`, maximum length 255 characters)",
        )
        .param(if_none_match_param(
            "`*` -- create-only: fails if the caller's own tenant already holds a record \
             under the reference. Mutually exclusive with `If-Match`.",
        ))
        .param(if_match_param(
            false,
            "`*` (replace, last-writer-wins) or a quoted `\"<id>.<version>\"` ETag (guarded \
             replace). Mutually exclusive with `If-None-Match`.",
        ))
        .json_request::<PutCredentialRequestDto>(
            openapi,
            "Credential type, sharing, fallback, expiry, and value",
        )
        .handler(handlers::put_credential)
        .no_content_response(
            StatusCode::CREATED,
            "Credential created (see Location/ETag headers)",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    let router = OperationBuilder::patch("/credstore/v1/credentials/{ref}")
        .operation_id("credstore.patch_credential")
        .summary("Partially update a credential by reference")
        .description(
            "RFC 7396 JSON Merge Patch over the record, the value, or both. Requires \
             `Content-Type: application/merge-patch+json` (415 otherwise) and a mandatory \
             `If-Match`. Never creates. A body carrying no `value` key whose metadata already \
             matches the current record is a no-op (204, unchanged ETag).",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param(
            "ref",
            "Credential reference (`[a-zA-Z0-9_-]+`, maximum length 255 characters)",
        )
        .param(if_match_param(
            true,
            "Mandatory. `*` (unconditional) or a quoted `\"<id>.<version>\"` ETag (guarded).",
        ))
        // Registered for OpenAPI schema purposes only: the toolkit's
        // `OperationBuilder`/`Json<T>` extractor has no non-JSON
        // content-type registration, so this documents the body shape as
        // `application/json` (a stated deviation) while the handler takes
        // the raw body and enforces the real
        // `application/merge-patch+json` requirement itself — see
        // `handlers::patch_credential`'s doc comment.
        .json_request::<CredentialPatchDto>(
            openapi,
            "Merge-patch body (Content-Type: application/merge-patch+json)",
        )
        .handler(handlers::patch_credential)
        .no_content_response(StatusCode::NO_CONTENT, "Credential updated")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_415(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    let router = OperationBuilder::delete("/credstore/v1/credentials/{ref}")
        .operation_id("credstore.delete_credential")
        .summary("Delete a credential by reference")
        .description("Delete the record and its value, releasing the reference at once.")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param(
            "ref",
            "Credential reference (`[a-zA-Z0-9_-]+`, maximum length 255 characters)",
        )
        .param(if_match_param(
            true,
            "Mandatory. `*` or a quoted `\"<id>.<version>\"` ETag.",
        ))
        .handler(handlers::delete_credential)
        .no_content_response(StatusCode::NO_CONTENT, "Credential deleted")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    router.layer(axum::Extension(svc))
}

#[cfg(test)]
#[path = "routes_tests.rs"]
mod tests;
