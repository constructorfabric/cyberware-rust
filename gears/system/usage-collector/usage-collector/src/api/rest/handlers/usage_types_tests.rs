//! Handler-level unit tests for the foundation `/usage-collector/v1/usage-types`
//! REST surface.
//!
//! Scope: pin handler-shaped concerns the SDK error-mapping and service
//! tests cannot reach. Specifically, [`super::handle_create_usage_type`]
//! must reject a bad-prefix `gts_id` at the `UsageTypeGtsId::new` boundary —
//! before ever dispatching to the catalog service. The DTO carries
//! `gts_id: String` (permissive) precisely so this short-circuit can run
//! and produce a canonical `InvalidArgument` `Problem` envelope rather
//! than axum's default `text/plain` 422.
//!
//! Out of scope here:
//!
//! * Wire-shape / DTO conversions — pinned in
//!   [`crate::api::rest::dto::tests`].
//! * Canonical-envelope body shape for the bad-`gts_id` failure
//!   (`status`, `context.field_violations[].field/.reason`) — pinned in
//!   [`crate::infra::sdk_error_mapping::sdk_error_mapping_tests`].
//!   This test pins the **handler-side composition**: that
//!   `handle_create_usage_type` short-circuits on the
//!   `UsageTypeGtsId::new` failure without reaching the catalog service.
//! * Service-layer register / list / read / delete CRUD — pinned in
//!   [`crate::domain::service::service_tests`].

use std::sync::Arc;

use axum::Json;
use axum::extract::Extension;
use axum::http::{StatusCode, Uri, header};
use toolkit::client_hub::ClientHub;
use toolkit_gts::gts_id;
use toolkit_security::SecurityContext;

use super::{
    handle_create_usage_type, handle_delete_usage_type, handle_get_usage_type,
    handle_list_usage_types,
};
use crate::api::rest::dto::CreateUsageTypeRequest;
use crate::domain::Service;
use crate::domain::test_support::{
    CountingUnreachableResolver, HappyPathPlugin, authenticated_ctx, enforcer_for,
    service_with_permit,
};

/// Wire a `Service` against a counting unreachable-PDP resolver and an
/// empty `ClientHub` (no plugin / no registry). Any handler path that
/// reaches the service surfaces 503 — but `CountingUnreachableResolver`
/// also records *that* it was reached, so the short-circuit tests below
/// can assert `resolver.calls() == 0` as direct evidence the service path
/// was not entered.
fn service_with_sentinel_pdp() -> (Arc<Service>, Arc<CountingUnreachableResolver>) {
    let hub = Arc::new(ClientHub::new());
    let resolver = CountingUnreachableResolver::new();
    let enforcer = enforcer_for(Arc::clone(&resolver) as _);
    let service = Arc::new(Service::new(hub, "constructorfabric".to_owned(), enforcer));
    (service, resolver)
}

#[tokio::test]
async fn register_with_bad_gts_id_short_circuits_to_invalid_argument_problem() {
    // Drive the handler with a payload whose `gts_id` is well-formed JSON
    // (so the DTO deserializes) but does not derive from the reserved
    // `gts.cf.core.uc.usage_record.v1~` base (so `UsageTypeGtsId::new` rejects it). The
    // handler MUST detect this at the boundary and return the canonical
    // `InvalidArgument` `Problem` envelope without ever touching the
    // service. We pair the service with a `CountingUnreachableResolver`
    // so the test pins the short-circuit two ways: the 400 + canonical
    // InvalidArgument envelope (field_violations[0].reason =
    // INVALID_BASE_GTS_ID), AND `resolver.calls() == 0` (any path that
    // *did* reach the catalog would have invoked the resolver).
    let (service, resolver) = service_with_sentinel_pdp();
    let uri: Uri = "/usage-collector/v1/usage-types"
        .parse()
        .expect("static uri parses");

    let raw_gts_id = "not-a-valid-prefix".to_owned();
    let response = handle_create_usage_type(
        Extension(SecurityContext::anonymous()),
        Extension(service),
        uri,
        Json(CreateUsageTypeRequest {
            gts_id: raw_gts_id.clone(),
            kind: "counter".to_owned(),
            metadata_fields: vec![],
        }),
    )
    .await
    .into_response();

    assert_eq!(
        response.status(),
        StatusCode::BAD_REQUEST,
        "bad-prefix gts_id MUST surface as a 400 handler short-circuit, \
         not reach the service",
    );
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    assert!(
        content_type.contains("problem+json"),
        "bad-prefix response MUST be application/problem+json, not axum's \
         default text/plain 422 (got `{content_type}`)",
    );

    let body_bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("Problem body collected");
    let body: serde_json::Value =
        serde_json::from_slice(&body_bytes).expect("Problem body is JSON");
    let violation = body
        .get("context")
        .and_then(|c| c.get("field_violations"))
        .and_then(|fv| fv.as_array())
        .and_then(|arr| arr.first())
        .expect("InvalidArgument envelope carries field_violations[0]");
    assert_eq!(
        violation.get("field").and_then(serde_json::Value::as_str),
        Some("gts_id"),
        "field_violations[0].field MUST point at the offending payload field",
    );
    assert_eq!(
        violation.get("reason").and_then(serde_json::Value::as_str),
        Some("INVALID_BASE_GTS_ID"),
        "field_violations[0].reason MUST be INVALID_BASE_GTS_ID for a \
         bad-prefix gts_id",
    );
    let description = violation
        .get("description")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    assert!(
        description.contains(raw_gts_id.as_str()),
        "field_violations[0].description MUST echo the rejected raw value \
         (got `{description}`)",
    );
    assert_eq!(
        resolver.calls(),
        0,
        "handler MUST short-circuit before dispatching to the catalog \
         service (resolver MUST NOT be touched on the bad-prefix path)",
    );
}

fn assert_bad_gts_id_problem_body(body: &serde_json::Value, raw_gts_id: &str) {
    // Shared assertion helper for the bad-`gts_id` short-circuit envelope:
    // every catalog handler that calls `UsageTypeGtsId::new` at the boundary
    // MUST surface the canonical `InvalidArgument` `Problem` with
    // `field_violations[0].field == "gts_id"`,
    // `reason == "INVALID_BASE_GTS_ID"`, and a description echoing the
    // rejected raw value.
    let violation = body
        .get("context")
        .and_then(|c| c.get("field_violations"))
        .and_then(|fv| fv.as_array())
        .and_then(|arr| arr.first())
        .expect("InvalidArgument envelope carries field_violations[0]");
    assert_eq!(
        violation.get("field").and_then(serde_json::Value::as_str),
        Some("gts_id"),
        "field_violations[0].field MUST point at the offending payload field",
    );
    assert_eq!(
        violation.get("reason").and_then(serde_json::Value::as_str),
        Some("INVALID_BASE_GTS_ID"),
        "field_violations[0].reason MUST be INVALID_BASE_GTS_ID for a \
         bad-prefix gts_id",
    );
    let description = violation
        .get("description")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    assert!(
        description.contains(raw_gts_id),
        "field_violations[0].description MUST echo the rejected raw value \
         (got `{description}`)",
    );
}

#[tokio::test]
async fn get_with_bad_gts_id_short_circuits_to_invalid_argument_problem() {
    // Drive the GET handler with a `gts_id` path segment that does not derive
    // from the reserved `gts.cf.core.uc.usage_record.v1~` base. The handler
    // MUST detect this at the boundary (`UsageTypeGtsId::new`) and return the
    // canonical `InvalidArgument` `Problem` envelope without ever touching
    // the service. We pair the service with a `CountingUnreachableResolver`
    // so the test pins the short-circuit two ways: the 400 + canonical
    // InvalidArgument envelope (field_violations[0].reason =
    // INVALID_BASE_GTS_ID), AND `resolver.calls() == 0` (any path that
    // *did* reach the catalog would have invoked the resolver).
    let (service, resolver) = service_with_sentinel_pdp();

    let raw_gts_id = "not-a-valid-prefix".to_owned();
    let response = handle_get_usage_type(
        Extension(SecurityContext::anonymous()),
        Extension(service),
        axum::extract::Path(raw_gts_id.clone()),
    )
    .await
    .into_response();

    assert_eq!(
        response.status(),
        StatusCode::BAD_REQUEST,
        "bad-prefix gts_id MUST surface as a 400 handler short-circuit, \
         not reach the service",
    );
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    assert!(
        content_type.contains("problem+json"),
        "bad-prefix response MUST be application/problem+json, not axum's \
         default text/plain 422 (got `{content_type}`)",
    );

    let body_bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("Problem body collected");
    let body: serde_json::Value =
        serde_json::from_slice(&body_bytes).expect("Problem body is JSON");
    assert_bad_gts_id_problem_body(&body, &raw_gts_id);

    assert_eq!(
        resolver.calls(),
        0,
        "handler MUST short-circuit before dispatching to the catalog \
         service (resolver MUST NOT be touched on the bad-prefix path)",
    );
}

#[tokio::test]
async fn delete_with_bad_gts_id_short_circuits_to_invalid_argument_problem() {
    // Mirror of the GET test above: DELETE also calls `UsageTypeGtsId::new`
    // at the boundary, so the same short-circuit contract applies. Without
    // this test, a refactor that moved the parse past the service call
    // would not fail any test (handler tests only covered POST).
    let (service, resolver) = service_with_sentinel_pdp();

    let raw_gts_id = "not-a-valid-prefix".to_owned();
    let response = handle_delete_usage_type(
        Extension(SecurityContext::anonymous()),
        Extension(service),
        axum::extract::Path(raw_gts_id.clone()),
    )
    .await
    .into_response();

    assert_eq!(
        response.status(),
        StatusCode::BAD_REQUEST,
        "bad-prefix gts_id MUST surface as a 400 handler short-circuit, \
         not reach the service",
    );
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    assert!(
        content_type.contains("problem+json"),
        "bad-prefix response MUST be application/problem+json, not axum's \
         default text/plain 422 (got `{content_type}`)",
    );

    let body_bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("Problem body collected");
    let body: serde_json::Value =
        serde_json::from_slice(&body_bytes).expect("Problem body is JSON");
    assert_bad_gts_id_problem_body(&body, &raw_gts_id);

    assert_eq!(
        resolver.calls(),
        0,
        "handler MUST short-circuit before dispatching to the catalog \
         service (resolver MUST NOT be touched on the bad-prefix path)",
    );
}

#[tokio::test]
async fn register_with_unknown_kind_returns_validation_problem() {
    // The wire surface accepts `kind` as a lowercase string projection of
    // the closed `UsageKind` enum (`counter` / `gauge`). Anything else is
    // rejected by the handler's `FromStr` parse, surfacing the standard
    // `Validation` `Problem` envelope (HTTP 400). The `gts_id` here is valid
    // so that we reach the `kind` parse step; the counting resolver lets
    // the test prove the unknown-`kind` path also short-circuits before
    // reaching the catalog service (`calls() == 0`).
    let (service, resolver) = service_with_sentinel_pdp();
    let uri: Uri = "/usage-collector/v1/usage-types"
        .parse()
        .expect("static uri parses");

    let raw_gts_id =
        gts_id!("cf.core.uc.usage_record.v1~tenant.example._.unknown_kind.v1").to_owned();
    let response = handle_create_usage_type(
        Extension(SecurityContext::anonymous()),
        Extension(service),
        uri,
        Json(CreateUsageTypeRequest {
            gts_id: raw_gts_id,
            kind: "histogram".to_owned(),
            metadata_fields: vec![],
        }),
    )
    .await
    .into_response();

    assert_eq!(
        response.status(),
        StatusCode::BAD_REQUEST,
        "unknown kind MUST surface as a 400 handler short-circuit, \
         not reach the service",
    );
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    assert!(
        content_type.contains("problem+json"),
        "unknown-kind response MUST be application/problem+json \
         (got `{content_type}`)",
    );
    let body_bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("Problem body collected");
    let body: serde_json::Value =
        serde_json::from_slice(&body_bytes).expect("Problem body is JSON");
    let violation = body
        .get("context")
        .and_then(|c| c.get("field_violations"))
        .and_then(|fv| fv.as_array())
        .and_then(|arr| arr.first())
        .expect("InvalidArgument envelope carries field_violations[0]");
    assert_eq!(
        violation.get("field").and_then(serde_json::Value::as_str),
        Some("kind"),
        "field_violations[0].field MUST point at the offending payload field",
    );
    assert_eq!(
        violation.get("reason").and_then(serde_json::Value::as_str),
        Some("VALIDATION"),
        "field_violations[0].reason MUST be VALIDATION for an unknown `kind`",
    );
    assert_eq!(
        resolver.calls(),
        0,
        "handler MUST short-circuit on the unknown-`kind` parse before \
         dispatching to the catalog service (resolver MUST NOT be touched)",
    );
}

// ---------------------------------------------------------------------------
// Happy-path coverage.
//
// The short-circuit tests above pin the rejection wiring; these tests pin
// the success-side composition (request → service → DTO conversion → wire
// body) for each of the four catalog handlers. They guard against
// regressions in `UsageTypeDto::from`, the 201 `Location` header
// composition on register, the OData page-envelope projection on list,
// and the 204 No Content shape on delete.
// ---------------------------------------------------------------------------

use axum::extract::Path;
use axum::response::IntoResponse;
use toolkit::api::canonical_prelude::OData;
use toolkit_odata::{ODataQuery, Page as ODataPage, page::PageInfo};
use usage_collector_sdk::{UsageKind, UsageType, UsageTypeGtsId};

const HAPPY_USAGE_TYPE_GTS_ID: &str =
    gts_id!("cf.core.uc.usage_record.v1~cf.handler_tests._.happy_counter.v1");

fn happy_usage_type() -> UsageType {
    UsageType {
        gts_id: UsageTypeGtsId::new(HAPPY_USAGE_TYPE_GTS_ID).expect("valid gts_id"),
        kind: UsageKind::Counter,
        metadata_fields: std::collections::BTreeSet::new(),
    }
}

#[tokio::test]
async fn register_happy_path_returns_201_with_location_and_wire_body() {
    // Wire the catalog handler against a permit PDP and a plugin whose
    // `create_usage_type` returns the supplied `UsageType` verbatim. The
    // handler MUST emit 201 Created, set a `Location` header pointing at
    // the canonical GET path for the new resource, and serialize the
    // wire body from the SERVICE-RETURNED record.
    let plugin = HappyPathPlugin::new();
    let persisted = happy_usage_type();
    plugin.set_create_usage_type(persisted.clone());

    let service = service_with_permit(
        Arc::clone(&plugin) as Arc<dyn usage_collector_sdk::UsageCollectorPluginV1>,
        "test.handler.register.happy.v1",
    );

    let uri: Uri = "/usage-collector/v1/usage-types"
        .parse()
        .expect("static uri parses");

    let response = handle_create_usage_type(
        Extension(authenticated_ctx()),
        Extension(service),
        uri,
        Json(CreateUsageTypeRequest {
            gts_id: HAPPY_USAGE_TYPE_GTS_ID.to_owned(),
            kind: "counter".to_owned(),
            metadata_fields: vec![],
        }),
    )
    .await
    .into_response();

    assert_eq!(
        response.status(),
        StatusCode::CREATED,
        "happy-path register MUST surface 201 Created",
    );
    let location = response
        .headers()
        .get(header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
        .expect("201 Created MUST carry a `Location` header");
    assert!(
        location.ends_with(HAPPY_USAGE_TYPE_GTS_ID),
        "Location header MUST point at the new resource's gts_id \
         (got `{location}`)",
    );

    let body_bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body collected");
    let body: serde_json::Value = serde_json::from_slice(&body_bytes).expect("body is JSON");
    assert_eq!(
        body.get("gts_id").and_then(serde_json::Value::as_str),
        Some(HAPPY_USAGE_TYPE_GTS_ID),
    );
    assert_eq!(
        body.get("kind").and_then(serde_json::Value::as_str),
        Some("counter"),
    );

    let forwarded = plugin
        .last_create_usage_type_input()
        .expect("plugin received the input usage type");
    assert_eq!(forwarded.gts_id.as_ref(), HAPPY_USAGE_TYPE_GTS_ID);
}

#[tokio::test]
async fn get_usage_type_happy_path_returns_200_with_wire_body() {
    let plugin = HappyPathPlugin::new();
    plugin.set_get_usage_type(happy_usage_type());

    let service = service_with_permit(
        Arc::clone(&plugin) as Arc<dyn usage_collector_sdk::UsageCollectorPluginV1>,
        "test.handler.get.happy.v1",
    );

    let response = handle_get_usage_type(
        Extension(authenticated_ctx()),
        Extension(service),
        Path(HAPPY_USAGE_TYPE_GTS_ID.to_owned()),
    )
    .await
    .into_response();

    assert_eq!(
        response.status(),
        StatusCode::OK,
        "happy-path get MUST surface 200 OK",
    );

    let body_bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body collected");
    let body: serde_json::Value = serde_json::from_slice(&body_bytes).expect("body is JSON");
    assert_eq!(
        body.get("gts_id").and_then(serde_json::Value::as_str),
        Some(HAPPY_USAGE_TYPE_GTS_ID),
    );
    assert_eq!(
        body.get("kind").and_then(serde_json::Value::as_str),
        Some("counter"),
    );
    assert_eq!(
        body.get("metadata_fields"),
        Some(&serde_json::json!([])),
        "get-by-id response MUST carry the closed `metadata_fields` array \
         from the catalog read (empty here, but the field must be present)",
    );
}

#[tokio::test]
async fn list_usage_types_happy_path_returns_200_with_page_envelope() {
    // Pass a non-default `ODataQuery` (custom `limit`, custom `select`) so
    // the test can pin two things at once:
    //   1. The 200 OK page envelope is composed from the plugin's response.
    //   2. The handler forwards the caller-supplied query unchanged through
    //      the service and into the SPI. A regression that swapped `&query`
    //      for `&ODataQuery::default()` (or dropped the parameter entirely)
    //      would silently break paginated catalog listings.
    let plugin = HappyPathPlugin::new();
    plugin.set_list_usage_types(ODataPage {
        items: vec![happy_usage_type()],
        page_info: PageInfo {
            next_cursor: None,
            prev_cursor: None,
            limit: 50,
        },
    });

    let service = service_with_permit(
        Arc::clone(&plugin) as Arc<dyn usage_collector_sdk::UsageCollectorPluginV1>,
        "test.handler.list.happy.v1",
    );

    let caller_query = ODataQuery::default()
        .with_limit(7)
        .with_select(vec!["gts_id".to_owned()]);

    let response = handle_list_usage_types(
        Extension(authenticated_ctx()),
        Extension(service),
        OData(caller_query),
    )
    .await
    .into_response();

    assert_eq!(
        response.status(),
        StatusCode::OK,
        "happy-path list MUST surface 200 OK",
    );

    let body_bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body collected");
    let body: serde_json::Value = serde_json::from_slice(&body_bytes).expect("body is JSON");
    let items = body
        .get("items")
        .and_then(serde_json::Value::as_array)
        .expect("page envelope carries an `items` array");
    assert_eq!(items.len(), 1);
    assert_eq!(
        items[0].get("gts_id").and_then(serde_json::Value::as_str),
        Some(HAPPY_USAGE_TYPE_GTS_ID),
    );
    assert!(
        body.get("page_info").is_some(),
        "page envelope MUST carry a `page_info` object",
    );

    let forwarded = plugin
        .last_list_usage_types_input()
        .expect("plugin received the list query");
    assert_eq!(
        forwarded.limit,
        Some(7),
        "handler MUST forward the caller's `limit` to the service / SPI \
         unchanged (a regression that dropped `&query` would surface here)",
    );
    assert_eq!(
        forwarded.selected_fields(),
        Some(["gts_id".to_owned()].as_slice()),
        "handler MUST forward the caller's `select` to the service / SPI \
         unchanged",
    );
}

#[tokio::test]
async fn delete_usage_type_happy_path_returns_204_no_content() {
    let plugin = HappyPathPlugin::new();
    plugin.set_delete_usage_type_ok();

    let service = service_with_permit(
        Arc::clone(&plugin) as Arc<dyn usage_collector_sdk::UsageCollectorPluginV1>,
        "test.handler.delete.happy.v1",
    );

    let response = handle_delete_usage_type(
        Extension(authenticated_ctx()),
        Extension(service),
        Path(HAPPY_USAGE_TYPE_GTS_ID.to_owned()),
    )
    .await
    .into_response();

    assert_eq!(
        response.status(),
        StatusCode::NO_CONTENT,
        "happy-path delete MUST surface 204 No Content",
    );
    let body_bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body collected");
    assert!(
        body_bytes.is_empty(),
        "204 No Content MUST carry an empty body (got {body_bytes:?})",
    );
    let forwarded = plugin
        .last_delete_usage_type_input()
        .expect("plugin received the target gts_id");
    assert_eq!(forwarded.as_ref(), HAPPY_USAGE_TYPE_GTS_ID);
}
