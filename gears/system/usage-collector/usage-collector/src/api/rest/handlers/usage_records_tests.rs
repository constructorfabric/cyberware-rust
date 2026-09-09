//! Handler-level unit tests for the foundation
//! `/usage-collector/v1/records` create + deactivation surface.
//!
//! Scope: pin handler-shaped concerns the SDK error-mapping and service
//! tests cannot reach. Specifically, the create handler lifts per-record
//! `gts_id` validation failures into the canonical `InvalidArgument`
//! `Problem` envelope (`field_violations[0].reason="INVALID_BASE_GTS_ID"`)
//! WITHOUT failing the surrounding batch, and the deactivate handler
//! lifts a malformed `uuid` path segment into the canonical
//! `InvalidArgument` envelope before reaching the service.
//!
//! Out of scope here:
//!
//! * Wire-shape / DTO conversions — pinned in
//!   [`crate::api::rest::dto::tests`].
//! * Service-layer create / deactivation — pinned in
//!   [`crate::domain::service::service_tests`].

use std::sync::Arc;
use toolkit_gts::gts_id;

use axum::Json;
use axum::extract::{Extension, Path};
use axum::http::{StatusCode, header};
use axum::response::IntoResponse;
use time::OffsetDateTime;
use toolkit::client_hub::ClientHub;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::{handle_create_usage_records, handle_deactivate_usage_record, handle_get_usage_record};
use crate::api::rest::dto::{CreateUsageRecordRequest, CreateUsageRecordsRequest, ResourceRefDto};
use crate::domain::Service;
use crate::domain::test_support::{
    CountingUnreachableResolver, HappyPathPlugin, authenticated_ctx, enforcer_for,
    service_with_permit,
};

/// Wire a `Service` against a counting unreachable-PDP resolver and an
/// empty `ClientHub` (no plugin / no registry). Any handler path that
/// reaches the service surfaces 503 — but `CountingUnreachableResolver`
/// also records *that* it was reached, so short-circuit tests below can
/// assert `resolver.calls() == 0` as direct evidence the service path was
/// not entered.
fn service_with_sentinel_pdp() -> (Arc<Service>, Arc<CountingUnreachableResolver>) {
    let hub = Arc::new(ClientHub::new());
    let resolver = CountingUnreachableResolver::new();
    let enforcer = enforcer_for(Arc::clone(&resolver) as _);
    let service = Arc::new(Service::new(hub, "constructorfabric".to_owned(), enforcer));
    (service, resolver)
}

#[tokio::test]
async fn create_with_only_bad_gts_id_records_short_circuits_to_207_without_calling_service() {
    // Every record carries a bad-prefix `gts_id`, so every record is
    // rejected at the handler boundary BEFORE the service is invoked. We
    // pair the service with a `CountingUnreachableResolver` so the test
    // can pin the short-circuit two ways: the response status is 207
    // (not 503), AND the resolver was never invoked (`calls() == 0`).
    let (service, resolver) = service_with_sentinel_pdp();

    let req = CreateUsageRecordsRequest {
        records: vec![CreateUsageRecordRequest {
            gts_id: "not-a-valid-prefix".to_owned(),
            tenant_id: Uuid::new_v4(),
            resource_ref: ResourceRefDto {
                resource_id: "rsc-1".to_owned(),
                resource_type: "compute.vm".to_owned(),
            },
            subject_ref: None,
            metadata: std::collections::BTreeMap::new(),
            value: rust_decimal::Decimal::from(1),
            idempotency_key: "idem-bad-prefix-1".to_owned(),
            corrects_id: None,
            created_at: OffsetDateTime::UNIX_EPOCH,
        }],
    };

    let response = handle_create_usage_records(
        Extension(SecurityContext::anonymous()),
        Extension(service),
        Json(req),
    )
    .await
    .into_response();

    assert_eq!(
        response.status(),
        StatusCode::MULTI_STATUS,
        "all-rejected batch MUST surface as 207 Multi-Status",
    );
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    assert!(
        content_type.contains("json"),
        "207 envelope MUST be JSON (got `{content_type}`)",
    );

    let body_bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body collected");
    let body: serde_json::Value = serde_json::from_slice(&body_bytes).expect("body is JSON");
    let results = body
        .get("results")
        .and_then(serde_json::Value::as_array)
        .expect("response carries a `results` array");
    assert_eq!(results.len(), 1);
    let item = &results[0];
    assert_eq!(
        item.get("outcome").and_then(serde_json::Value::as_str),
        Some("rejected"),
        "bad-prefix record MUST surface as `outcome: rejected`",
    );
    let problem = item.get("error").expect("rejected item carries `error`");
    let violation = problem
        .get("context")
        .and_then(|c| c.get("field_violations"))
        .and_then(|fv| fv.as_array())
        .and_then(|arr| arr.first())
        .expect("rejected error carries field_violations[0]");
    assert_eq!(
        violation.get("field").and_then(serde_json::Value::as_str),
        Some("gts_id"),
        "per-record bad-prefix error MUST carry field_violations[0].field = gts_id",
    );
    assert_eq!(
        violation.get("reason").and_then(serde_json::Value::as_str),
        Some("INVALID_BASE_GTS_ID"),
        "per-record bad-prefix error MUST carry field_violations[0].reason = INVALID_BASE_GTS_ID",
    );
    assert_eq!(
        resolver.calls(),
        0,
        "handler MUST short-circuit before dispatching to the service \
         (resolver MUST NOT be touched on the all-rejected path)",
    );
}

#[tokio::test]
async fn deactivate_with_malformed_uuid_returns_400_before_reaching_service() {
    // A non-UUID path segment surfaces as a canonical InvalidArgument
    // problem without ever dispatching to the service. The counting
    // resolver lets the test pin the short-circuit directly: a service
    // path entry would have invoked it, so `calls() == 0` is the
    // sentinel.
    let (service, resolver) = service_with_sentinel_pdp();

    let raw_uuid = "not-a-uuid".to_owned();
    let response = handle_deactivate_usage_record(
        Extension(SecurityContext::anonymous()),
        Extension(service),
        Path(raw_uuid.clone()),
    )
    .await
    .into_response();

    assert_eq!(
        response.status(),
        StatusCode::BAD_REQUEST,
        "malformed UUID MUST lift to 400",
    );

    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    assert!(
        content_type.contains("problem+json"),
        "malformed-UUID response MUST be application/problem+json, not axum's \
         default text/plain (got `{content_type}`)",
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
        Some("id"),
        "field_violations[0].field MUST identify the malformed path segment",
    );
    assert_eq!(
        violation.get("reason").and_then(serde_json::Value::as_str),
        Some("VALIDATION"),
        "field_violations[0].reason MUST be VALIDATION for a non-UUID id",
    );
    let description = violation
        .get("description")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    assert!(
        description.contains(raw_uuid.as_str()),
        "field_violations[0].description MUST echo the rejected raw value \
         (got `{description}`)",
    );

    assert_eq!(
        resolver.calls(),
        0,
        "handler MUST reject the malformed UUID before reaching the service \
         (resolver MUST NOT be touched)",
    );
}

#[tokio::test]
async fn deactivate_without_plugin_surfaces_503() {
    // Wire a service against an empty `ClientHub` (no usage-collector
    // storage plugin registered). The `service_with_sentinel_pdp` helper
    // does NOT register a plugin, so the deactivate handler's first step
    // (`Service::get_plugin`) fails with the plugin-host `ServiceUnavailable`
    // and the handler lifts that to a canonical 503 `Problem` envelope.
    //
    // This is NOT a test of the PDP-unreachable branch — `resolver.calls()`
    // here is `0` because the handler never reaches the PDP step. The
    // PDP-unreachable branch is covered by
    // `deactivate_with_unreachable_pdp_surfaces_503` below (which wires a
    // real plugin so the PDP step IS reached and the unreachable resolver
    // surfaces the 503).
    let (service, resolver) = service_with_sentinel_pdp();

    let response = handle_deactivate_usage_record(
        Extension(SecurityContext::anonymous()),
        Extension(service),
        Path(Uuid::new_v4().to_string()),
    )
    .await
    .into_response();

    assert_eq!(
        response.status(),
        StatusCode::SERVICE_UNAVAILABLE,
        "missing storage plugin MUST surface 503",
    );
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    assert!(
        content_type.contains("problem+json"),
        "503 envelope MUST be application/problem+json (got `{content_type}`)",
    );
    assert_eq!(
        resolver.calls(),
        0,
        "missing-plugin path MUST short-circuit BEFORE reaching the PDP \
         (resolver MUST NOT be touched - its non-zero call count would \
         mean the handler walked past `get_plugin` unexpectedly)",
    );
}

#[tokio::test]
async fn deactivate_with_unreachable_pdp_surfaces_503() {
    // Wire a service against a REAL plugin (so `get_plugin` succeeds and the
    // prefetch returns a record) plus an unreachable PDP resolver. The
    // handler reaches the PDP step, the resolver fails with transport
    // `ServiceUnavailable`, and the handler lifts that to a canonical 503
    // `Problem` envelope. The counting resolver pins this as the real PDP
    // path: `calls() >= 1` is direct evidence the handler walked past
    // `get_plugin` and `get_usage_record` and invoked the PDP.
    let plugin = HappyPathPlugin::new();
    let target_uuid = Uuid::new_v4();
    let tenant_id = Uuid::from_u128(2);
    plugin.set_get_record(sample_persisted_record(target_uuid, tenant_id));

    let hub = crate::domain::test_support::hub_with_plugin(
        Arc::clone(&plugin) as Arc<dyn usage_collector_sdk::UsageCollectorPluginV1>,
        "test.handler.deactivate.unreachable_pdp.v1",
        "constructorfabric",
    );
    let resolver = CountingUnreachableResolver::new();
    let enforcer = enforcer_for(Arc::clone(&resolver) as _);
    let service = Arc::new(Service::new(hub, "constructorfabric".to_owned(), enforcer));

    let response = handle_deactivate_usage_record(
        Extension(authenticated_ctx()),
        Extension(service),
        Path(target_uuid.to_string()),
    )
    .await
    .into_response();

    assert_eq!(
        response.status(),
        StatusCode::SERVICE_UNAVAILABLE,
        "unreachable PDP MUST surface 503",
    );
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    assert!(
        content_type.contains("problem+json"),
        "503 envelope MUST be application/problem+json (got `{content_type}`)",
    );
    assert!(
        resolver.calls() >= 1,
        "handler MUST reach the PDP step before failing — a missing-plugin \
         or prefetch-failure path that bypasses the PDP would not catch a \
         real PDP-unreachable regression (resolver.calls() == {})",
        resolver.calls(),
    );
}

// ---------------------------------------------------------------------------
// Happy-path coverage.
//
// The short-circuit tests above pin the rejection wiring; these tests pin the
// success-side composition (request → service → DTO conversion → wire body).
// Specifically they guard against a regression that swaps the persisted
// record with the input record inside [`super::UsageRecordDto::from`]: the
// service-returned record carries a UUID DIFFERENT from the submitted one,
// and the test asserts the wire body echoes the service-returned UUID.
// ---------------------------------------------------------------------------

use std::collections::BTreeMap;
use usage_collector_sdk::{
    IdempotencyKey, ResourceRef, UsageKind, UsageRecord, UsageRecordStatus, UsageType,
    UsageTypeGtsId, derive_usage_record_id,
};

const HAPPY_RECORD_GTS_ID: &str =
    gts_id!("cf.core.uc.usage_record.v1~cf.mini_chat._.tokens_consumed.v1");

fn happy_usage_type() -> UsageType {
    UsageType {
        gts_id: UsageTypeGtsId::new(HAPPY_RECORD_GTS_ID).expect("valid gts_id"),
        kind: UsageKind::Counter,
        metadata_fields: std::collections::BTreeSet::new(),
    }
}

fn sample_persisted_record(id: Uuid, tenant_id: Uuid) -> UsageRecord {
    sample_persisted_record_with_status(id, tenant_id, UsageRecordStatus::Active)
}

/// An [`ODataQuery`](toolkit_odata::ODataQuery) carrying a bounded
/// `created_at` window so a read request clears the gateway's
/// bounded-window guard and reaches the service / plugin.
fn bounded_window_query() -> toolkit_odata::ODataQuery {
    let expr = toolkit_odata::parse_filter_string(
        "created_at ge 2026-01-01T00:00:00Z and created_at lt 2026-02-01T00:00:00Z",
    )
    .expect("bounded window filter parses")
    .into_expr();
    toolkit_odata::ODataQuery::from(Some(expr))
}

fn sample_persisted_record_with_status(
    id: Uuid,
    tenant_id: Uuid,
    status: UsageRecordStatus,
) -> UsageRecord {
    UsageRecord {
        id,
        gts_id: UsageTypeGtsId::new(HAPPY_RECORD_GTS_ID).expect("valid gts_id"),
        tenant_id,
        resource_ref: ResourceRef::new("rsc-happy", "compute.vm").expect("valid resource ref"),
        subject_ref: None,
        metadata: BTreeMap::new(),
        value: rust_decimal::Decimal::from(1),
        idempotency_key: IdempotencyKey::new("idem-happy").expect("valid idempotency key"),
        corrects_id: None,
        status,
        created_at: OffsetDateTime::UNIX_EPOCH,
    }
}

#[tokio::test]
async fn create_records_happy_path_wire_body_reflects_service_returned_record() {
    // Wire the service against a permit-by-default PDP and a
    // `HappyPathPlugin` that:
    //   1. returns a `Counter` `UsageType` from `get_usage_type` so
    //      semantics validation passes, and
    //   2. returns one `Ok(persisted_record)` from `create_usage_records`
    //      where `persisted_record.id` is DIFFERENT from the gateway-derived
    //      dispatched record's id.
    // The handler then emits 200 OK; the wire body's `records[0].record.id`
    // MUST be the persisted id — proving the handler composes the
    // response from the SERVICE-RETURNED record, not from the dispatched one.
    let plugin = HappyPathPlugin::new();
    plugin.set_get_usage_type(happy_usage_type());

    let tenant_id = Uuid::from_u128(2);
    let gts_id = UsageTypeGtsId::new(HAPPY_RECORD_GTS_ID).expect("valid gts_id");
    let idempotency_key = IdempotencyKey::new("idem-happy").expect("valid idempotency key");
    let derived_id = derive_usage_record_id(
        tenant_id,
        &gts_id,
        &idempotency_key,
        OffsetDateTime::UNIX_EPOCH,
    );
    let persisted_uuid = Uuid::new_v4();
    assert_ne!(derived_id, persisted_uuid, "test premise");
    plugin.set_create_records(vec![Ok(sample_persisted_record(persisted_uuid, tenant_id))]);

    let service = service_with_permit(
        Arc::clone(&plugin) as Arc<dyn usage_collector_sdk::UsageCollectorPluginV1>,
        "test.handler.create_records.happy.v1",
    );

    let req = CreateUsageRecordsRequest {
        records: vec![CreateUsageRecordRequest {
            gts_id: HAPPY_RECORD_GTS_ID.to_owned(),
            tenant_id,
            resource_ref: ResourceRefDto {
                resource_id: "rsc-happy".to_owned(),
                resource_type: "compute.vm".to_owned(),
            },
            subject_ref: None,
            metadata: BTreeMap::new(),
            value: rust_decimal::Decimal::from(1),
            idempotency_key: "idem-happy".to_owned(),
            corrects_id: None,
            created_at: OffsetDateTime::UNIX_EPOCH,
        }],
    };

    let response = handle_create_usage_records(
        Extension(authenticated_ctx()),
        Extension(service),
        Json(req),
    )
    .await
    .into_response();

    assert_eq!(
        response.status(),
        StatusCode::OK,
        "all-accepted happy path MUST surface as 200 OK, not 207",
    );

    let body_bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body collected");
    let body: serde_json::Value = serde_json::from_slice(&body_bytes).expect("body is JSON");
    let results = body
        .get("results")
        .and_then(serde_json::Value::as_array)
        .expect("response carries a `results` array");
    assert_eq!(results.len(), 1);
    let item = &results[0];
    assert_eq!(
        item.get("outcome").and_then(serde_json::Value::as_str),
        Some("accepted"),
    );
    let record = item.get("record").expect("accepted item carries `record`");
    assert_eq!(
        record.get("id").and_then(serde_json::Value::as_str),
        Some(persisted_uuid.to_string().as_str()),
        "wire body MUST echo the service-returned (persisted) UUID, NOT the \
         gateway-derived dispatched UUID",
    );
    assert_eq!(
        record.get("status").and_then(serde_json::Value::as_str),
        Some("active"),
        "wire body MUST project `UsageRecordStatus::Active` to lowercase \
         string `active` (a regression that flipped this to e.g. `\"ACTIVE\"` \
         or the empty string would silently break OAS-typed clients)",
    );

    // Sanity: the plugin was actually invoked with the gateway-derived id.
    let forwarded = plugin
        .last_create_records_input()
        .expect("plugin received the eligible batch");
    assert_eq!(forwarded.len(), 1);
    assert_eq!(forwarded[0].id, derived_id);
}

#[tokio::test]
async fn create_stamps_derived_id() {
    // The gateway MUST derive the dispatched record's id from the dedup key
    // `(tenant_id, gts_id, idempotency_key, created_at)` rather than accept a
    // caller-chosen value — pin both that the dispatched id matches
    // `derive_usage_record_id` AND that a same-key resubmit derives the
    // identical id (determinism).
    let plugin = HappyPathPlugin::new();
    plugin.set_get_usage_type(happy_usage_type());

    let tenant_id = Uuid::from_u128(2);
    let gts_id = UsageTypeGtsId::new(HAPPY_RECORD_GTS_ID).expect("valid gts_id");
    let idempotency_key = IdempotencyKey::new("idem-derive-1").expect("valid idempotency key");
    let expected = derive_usage_record_id(
        tenant_id,
        &gts_id,
        &idempotency_key,
        OffsetDateTime::UNIX_EPOCH,
    );

    let service = service_with_permit(
        Arc::clone(&plugin) as Arc<dyn usage_collector_sdk::UsageCollectorPluginV1>,
        "test.handler.create_records.derive_id.v1",
    );

    let build_req = || CreateUsageRecordsRequest {
        records: vec![CreateUsageRecordRequest {
            gts_id: HAPPY_RECORD_GTS_ID.to_owned(),
            tenant_id,
            resource_ref: ResourceRefDto {
                resource_id: "rsc-happy".to_owned(),
                resource_type: "compute.vm".to_owned(),
            },
            subject_ref: None,
            metadata: BTreeMap::new(),
            value: rust_decimal::Decimal::from(1),
            idempotency_key: "idem-derive-1".to_owned(),
            corrects_id: None,
            created_at: OffsetDateTime::UNIX_EPOCH,
        }],
    };

    // First submission.
    plugin.set_create_records(vec![Ok(sample_persisted_record(Uuid::new_v4(), tenant_id))]);
    let response = handle_create_usage_records(
        Extension(authenticated_ctx()),
        Extension(Arc::clone(&service)),
        Json(build_req()),
    )
    .await
    .into_response();
    assert_eq!(response.status(), StatusCode::OK);

    let forwarded = plugin
        .last_create_records_input()
        .expect("plugin received the eligible batch");
    assert_eq!(forwarded.len(), 1);
    assert_eq!(
        forwarded[0].id, expected,
        "gateway MUST stamp the dispatched record's id with \
         derive_usage_record_id(tenant_id, gts_id, idempotency_key, created_at)",
    );

    // Same-key resubmit: the derived id MUST be identical.
    plugin.set_create_records(vec![Ok(sample_persisted_record(Uuid::new_v4(), tenant_id))]);
    let response = handle_create_usage_records(
        Extension(authenticated_ctx()),
        Extension(service),
        Json(build_req()),
    )
    .await
    .into_response();
    assert_eq!(response.status(), StatusCode::OK);

    let forwarded_again = plugin
        .last_create_records_input()
        .expect("plugin received the eligible batch");
    assert_eq!(forwarded_again.len(), 1);
    assert_eq!(
        forwarded_again[0].id, expected,
        "a same dedup-key resubmit MUST derive the identical id",
    );
}

#[tokio::test]
async fn create_same_key_different_created_at_derives_distinct_ids() {
    // ADR-0014: `created_at` is part of the identity. Two submissions sharing
    // `(tenant_id, gts_id, idempotency_key)` but carrying different `created_at`
    // values MUST be dispatched with DISTINCT ids (previously they collided on
    // one derived id).
    let plugin = HappyPathPlugin::new();
    plugin.set_get_usage_type(happy_usage_type());

    let tenant_id = Uuid::from_u128(2);
    let service = service_with_permit(
        Arc::clone(&plugin) as Arc<dyn usage_collector_sdk::UsageCollectorPluginV1>,
        "test.handler.create_records.distinct_created_at.v1",
    );

    let build_req = |created_at: OffsetDateTime| CreateUsageRecordsRequest {
        records: vec![CreateUsageRecordRequest {
            gts_id: HAPPY_RECORD_GTS_ID.to_owned(),
            tenant_id,
            resource_ref: ResourceRefDto {
                resource_id: "rsc-happy".to_owned(),
                resource_type: "compute.vm".to_owned(),
            },
            subject_ref: None,
            metadata: BTreeMap::new(),
            value: rust_decimal::Decimal::from(1),
            idempotency_key: "idem-distinct".to_owned(),
            corrects_id: None,
            created_at,
        }],
    };

    plugin.set_create_records(vec![Ok(sample_persisted_record(Uuid::new_v4(), tenant_id))]);
    let response = handle_create_usage_records(
        Extension(authenticated_ctx()),
        Extension(Arc::clone(&service)),
        Json(build_req(OffsetDateTime::UNIX_EPOCH)),
    )
    .await
    .into_response();
    assert_eq!(response.status(), StatusCode::OK);
    let id_a = plugin
        .last_create_records_input()
        .expect("first batch dispatched")[0]
        .id;

    plugin.set_create_records(vec![Ok(sample_persisted_record(Uuid::new_v4(), tenant_id))]);
    let response = handle_create_usage_records(
        Extension(authenticated_ctx()),
        Extension(service),
        Json(build_req(
            OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(1),
        )),
    )
    .await
    .into_response();
    assert_eq!(response.status(), StatusCode::OK);
    let id_b = plugin
        .last_create_records_input()
        .expect("second batch dispatched")[0]
        .id;

    assert_ne!(
        id_a, id_b,
        "same dedup key + different created_at MUST derive distinct ids",
    );
}

#[tokio::test]
async fn create_records_happy_path_wire_body_projects_inactive_status_as_lowercase() {
    // Sibling to `create_records_happy_path_wire_body_reflects_service_returned_record`:
    // pin the other side of the `UsageRecordStatus` projection. Plugins can
    // legitimately return an `Inactive` record from a re-emit (post-cascade)
    // scenario; the wire MUST project it as lowercase string `inactive`. A
    // regression that flipped the `From<UsageRecord>` mapping (e.g. capital
    // case, empty string, or the wrong variant) would not surface in any
    // existing test.
    let plugin = HappyPathPlugin::new();
    plugin.set_get_usage_type(happy_usage_type());

    let tenant_id = Uuid::from_u128(2);
    let persisted_uuid = Uuid::new_v4();
    plugin.set_create_records(vec![Ok(sample_persisted_record_with_status(
        persisted_uuid,
        tenant_id,
        UsageRecordStatus::Inactive,
    ))]);

    let service = service_with_permit(
        Arc::clone(&plugin) as Arc<dyn usage_collector_sdk::UsageCollectorPluginV1>,
        "test.handler.create_records.inactive_projection.v1",
    );

    let req = CreateUsageRecordsRequest {
        records: vec![CreateUsageRecordRequest {
            gts_id: HAPPY_RECORD_GTS_ID.to_owned(),
            tenant_id,
            resource_ref: ResourceRefDto {
                resource_id: "rsc-happy".to_owned(),
                resource_type: "compute.vm".to_owned(),
            },
            subject_ref: None,
            metadata: BTreeMap::new(),
            value: rust_decimal::Decimal::from(1),
            idempotency_key: "idem-happy".to_owned(),
            corrects_id: None,
            created_at: OffsetDateTime::UNIX_EPOCH,
        }],
    };

    let response = handle_create_usage_records(
        Extension(authenticated_ctx()),
        Extension(service),
        Json(req),
    )
    .await
    .into_response();

    assert_eq!(response.status(), StatusCode::OK);

    let body_bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body collected");
    let body: serde_json::Value = serde_json::from_slice(&body_bytes).expect("body is JSON");
    let record = body
        .get("results")
        .and_then(serde_json::Value::as_array)
        .and_then(|arr| arr.first())
        .and_then(|item| item.get("record"))
        .expect("accepted item carries `record`");
    assert_eq!(
        record.get("status").and_then(serde_json::Value::as_str),
        Some("inactive"),
        "wire body MUST project `UsageRecordStatus::Inactive` to lowercase \
         string `inactive`",
    );
}

#[tokio::test]
async fn create_records_mixed_batch_preserves_input_order_across_accept_and_reject() {
    // Submit a 3-record batch as [valid, bad-prefix, valid]. The bad-prefix
    // record is rejected at the handler boundary (never reaches the service);
    // the two valid records flow through the service and the plugin returns
    // Ok for both. The wire body's `results` array MUST come out ordered by
    // input index — [Accepted(0), Rejected(1), Accepted(2)] — to pin the
    // handler's sort-by-input-index step against a regression that would
    // append accepted entries after rejected ones.
    let plugin = HappyPathPlugin::new();
    plugin.set_get_usage_type(happy_usage_type());

    let tenant_id = Uuid::from_u128(2);
    let gts_id = UsageTypeGtsId::new(HAPPY_RECORD_GTS_ID).expect("valid gts_id");
    let derived_id_0 = derive_usage_record_id(
        tenant_id,
        &gts_id,
        &IdempotencyKey::new("idem-mixed-0").expect("valid idempotency key"),
        OffsetDateTime::UNIX_EPOCH,
    );
    let derived_id_2 = derive_usage_record_id(
        tenant_id,
        &gts_id,
        &IdempotencyKey::new("idem-mixed-2").expect("valid idempotency key"),
        OffsetDateTime::UNIX_EPOCH,
    );
    let persisted_uuid_0 = Uuid::new_v4();
    let persisted_uuid_2 = Uuid::new_v4();
    plugin.set_create_records(vec![
        Ok(sample_persisted_record(persisted_uuid_0, tenant_id)),
        Ok(sample_persisted_record(persisted_uuid_2, tenant_id)),
    ]);

    let service = service_with_permit(
        Arc::clone(&plugin) as Arc<dyn usage_collector_sdk::UsageCollectorPluginV1>,
        "test.handler.create_records.mixed.v1",
    );

    let valid_record = |idem: &str| CreateUsageRecordRequest {
        gts_id: HAPPY_RECORD_GTS_ID.to_owned(),
        tenant_id,
        resource_ref: ResourceRefDto {
            resource_id: "rsc-mixed".to_owned(),
            resource_type: "compute.vm".to_owned(),
        },
        subject_ref: None,
        metadata: BTreeMap::new(),
        value: rust_decimal::Decimal::from(1),
        idempotency_key: idem.to_owned(),
        corrects_id: None,
        created_at: OffsetDateTime::UNIX_EPOCH,
    };

    let req = CreateUsageRecordsRequest {
        records: vec![
            valid_record("idem-mixed-0"),
            CreateUsageRecordRequest {
                gts_id: "not-a-valid-prefix".to_owned(),
                tenant_id,
                resource_ref: ResourceRefDto {
                    resource_id: "rsc-mixed".to_owned(),
                    resource_type: "compute.vm".to_owned(),
                },
                subject_ref: None,
                metadata: BTreeMap::new(),
                value: rust_decimal::Decimal::from(1),
                idempotency_key: "idem-mixed-1".to_owned(),
                corrects_id: None,
                created_at: OffsetDateTime::UNIX_EPOCH,
            },
            valid_record("idem-mixed-2"),
        ],
    };

    let response = handle_create_usage_records(
        Extension(authenticated_ctx()),
        Extension(service),
        Json(req),
    )
    .await
    .into_response();

    assert_eq!(
        response.status(),
        StatusCode::MULTI_STATUS,
        "any rejection in the batch MUST surface as 207 Multi-Status",
    );

    let body_bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body collected");
    let body: serde_json::Value = serde_json::from_slice(&body_bytes).expect("body is JSON");
    let results = body
        .get("results")
        .and_then(serde_json::Value::as_array)
        .expect("response carries a `results` array");
    assert_eq!(results.len(), 3);

    let outcome = |i: usize| {
        results[i]
            .get("outcome")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    let index_field = |i: usize| results[i].get("index").and_then(serde_json::Value::as_u64);

    assert_eq!(outcome(0), "accepted", "input index 0 MUST be accepted");
    assert_eq!(index_field(0), Some(0));
    assert_eq!(
        results[0]
            .get("record")
            .and_then(|r| r.get("id"))
            .and_then(serde_json::Value::as_str),
        Some(persisted_uuid_0.to_string().as_str()),
    );

    assert_eq!(outcome(1), "rejected", "input index 1 MUST be rejected");
    assert_eq!(index_field(1), Some(1));

    assert_eq!(outcome(2), "accepted", "input index 2 MUST be accepted");
    assert_eq!(index_field(2), Some(2));
    assert_eq!(
        results[2]
            .get("record")
            .and_then(|r| r.get("id"))
            .and_then(serde_json::Value::as_str),
        Some(persisted_uuid_2.to_string().as_str()),
    );

    let forwarded = plugin
        .last_create_records_input()
        .expect("plugin received the eligible batch");
    assert_eq!(
        forwarded.len(),
        2,
        "plugin MUST receive only the eligible (handler-validated) records",
    );
    assert_eq!(forwarded[0].id, derived_id_0);
    assert_eq!(forwarded[1].id, derived_id_2);
}

#[tokio::test]
async fn deactivate_happy_path_returns_204_no_content() {
    // Wire a permit-PDP service whose plugin succeeds on both the
    // pre-PDP `get_usage_record` prefetch (so the gateway has an
    // attribution tuple to authorize against) and on the
    // `deactivate_usage_record` SPI dispatch. The handler MUST emit a
    // 204 No Content with no body.
    let plugin = HappyPathPlugin::new();
    let target_uuid = Uuid::new_v4();
    let tenant_id = Uuid::from_u128(2);
    plugin.set_get_record(sample_persisted_record(target_uuid, tenant_id));
    plugin.set_deactivate_ok();

    let service = service_with_permit(
        Arc::clone(&plugin) as Arc<dyn usage_collector_sdk::UsageCollectorPluginV1>,
        "test.handler.deactivate.happy.v1",
    );

    let response = handle_deactivate_usage_record(
        Extension(authenticated_ctx()),
        Extension(service),
        Path(target_uuid.to_string()),
    )
    .await
    .into_response();

    assert_eq!(
        response.status(),
        StatusCode::NO_CONTENT,
        "happy-path deactivate MUST surface 204 No Content",
    );
    let body_bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body collected");
    assert!(
        body_bytes.is_empty(),
        "204 No Content MUST carry an empty body (got {body_bytes:?})",
    );
    assert_eq!(
        plugin.last_deactivate_input(),
        Some(target_uuid),
        "service MUST forward the path-supplied UUID to the plugin",
    );
}

// ---------------------------------------------------------------------------
// `handle_get_usage_record`: GET /usage-collector/v1/records/{id}
//
// Handler-shaped concerns the service tests cannot reach:
//   - Malformed UUID path segment lifts to InvalidArgument (HTTP 400)
//     BEFORE the service is invoked.
//   - Missing storage plugin surfaces 503 BEFORE the PDP is reached.
//   - Unreachable PDP surfaces 503 AFTER the plugin prefetch succeeds.
//   - Happy path: 200 OK with the persisted record body, wire-projected
//     through `UsageRecordDto`.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn get_with_malformed_uuid_returns_400_before_reaching_service() {
    let (service, resolver) = service_with_sentinel_pdp();

    let raw_uuid = "not-a-uuid".to_owned();
    let response = handle_get_usage_record(
        Extension(SecurityContext::anonymous()),
        Extension(service),
        Path(raw_uuid.clone()),
    )
    .await
    .into_response();

    assert_eq!(
        response.status(),
        StatusCode::BAD_REQUEST,
        "malformed UUID MUST lift to 400",
    );
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    assert!(
        content_type.contains("problem+json"),
        "malformed-UUID response MUST be application/problem+json (got `{content_type}`)",
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
        Some("id"),
    );
    assert_eq!(
        violation.get("reason").and_then(serde_json::Value::as_str),
        Some("VALIDATION"),
    );
    assert_eq!(
        resolver.calls(),
        0,
        "handler MUST reject the malformed UUID before reaching the service",
    );
}

#[tokio::test]
async fn get_without_plugin_surfaces_503() {
    // No usage-collector storage plugin registered: the handler reaches
    // the service, whose first step (`Service::get_plugin`) fails closed.
    // The unreachable-PDP resolver is NOT touched — the failure is on
    // plugin resolution, before PDP — and `resolver.calls() == 0` pins
    // the short-circuit ordering.
    let (service, resolver) = service_with_sentinel_pdp();

    let response = handle_get_usage_record(
        Extension(SecurityContext::anonymous()),
        Extension(service),
        Path(Uuid::new_v4().to_string()),
    )
    .await
    .into_response();

    assert_eq!(
        response.status(),
        StatusCode::SERVICE_UNAVAILABLE,
        "missing storage plugin MUST surface 503",
    );
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    assert!(
        content_type.contains("problem+json"),
        "503 envelope MUST be application/problem+json (got `{content_type}`)",
    );
    assert_eq!(
        resolver.calls(),
        0,
        "missing-plugin path MUST short-circuit BEFORE reaching the PDP",
    );
}

#[tokio::test]
async fn get_with_unreachable_pdp_surfaces_503() {
    // Real plugin (so the prefetch succeeds and loads the attribution
    // tuple) + unreachable PDP resolver. The PDP step IS reached after
    // the prefetch, and the resolver fails with transport
    // `ServiceUnavailable` lifted to the canonical 503 `Problem`. The
    // counting resolver pins the real PDP path: `calls() >= 1` is
    // direct evidence the handler walked past `get_plugin` and the
    // prefetch SPI dispatch.
    let plugin = HappyPathPlugin::new();
    let target_uuid = Uuid::new_v4();
    let tenant_id = Uuid::from_u128(2);
    plugin.set_get_record(sample_persisted_record(target_uuid, tenant_id));

    let hub = crate::domain::test_support::hub_with_plugin(
        Arc::clone(&plugin) as Arc<dyn usage_collector_sdk::UsageCollectorPluginV1>,
        "test.handler.get_record.unreachable_pdp.v1",
        "constructorfabric",
    );
    let resolver = CountingUnreachableResolver::new();
    let enforcer = enforcer_for(Arc::clone(&resolver) as _);
    let service = Arc::new(Service::new(hub, "constructorfabric".to_owned(), enforcer));

    let response = handle_get_usage_record(
        Extension(authenticated_ctx()),
        Extension(service),
        Path(target_uuid.to_string()),
    )
    .await
    .into_response();

    assert_eq!(
        response.status(),
        StatusCode::SERVICE_UNAVAILABLE,
        "unreachable PDP MUST surface 503",
    );
    assert!(
        resolver.calls() >= 1,
        "handler MUST reach the PDP step before failing (resolver.calls() == {})",
        resolver.calls(),
    );
}

#[tokio::test]
async fn get_happy_path_returns_200_with_record_body() {
    let plugin = HappyPathPlugin::new();
    let target_uuid = Uuid::new_v4();
    let tenant_id = Uuid::from_u128(2);
    plugin.set_get_record(sample_persisted_record(target_uuid, tenant_id));

    let service = service_with_permit(
        Arc::clone(&plugin) as Arc<dyn usage_collector_sdk::UsageCollectorPluginV1>,
        "test.handler.get_record.happy.v1",
    );

    let response = handle_get_usage_record(
        Extension(authenticated_ctx()),
        Extension(service),
        Path(target_uuid.to_string()),
    )
    .await
    .into_response();

    assert_eq!(
        response.status(),
        StatusCode::OK,
        "happy-path get MUST surface 200 OK",
    );
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    assert!(
        content_type.contains("json"),
        "200 envelope MUST be JSON (got `{content_type}`)",
    );

    let body_bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body collected");
    let body: serde_json::Value = serde_json::from_slice(&body_bytes).expect("body is JSON");
    assert_eq!(
        body.get("id").and_then(serde_json::Value::as_str),
        Some(target_uuid.to_string().as_str()),
        "wire body MUST echo the loaded record's UUID",
    );
    assert_eq!(
        body.get("status").and_then(serde_json::Value::as_str),
        Some("active"),
    );
}

// ---------------------------------------------------------------------------
// prepare_list_query — $top cap, $orderby default, cursor validate
// ---------------------------------------------------------------------------

mod prepare_list_query_tests {
    use toolkit_canonical_errors::CanonicalError;
    use toolkit_canonical_errors::context::InvalidArgumentV1;
    use toolkit_odata::ast::{CompareOperator, Expr, Value};
    use toolkit_odata::{CursorV1, ODataOrderBy, ODataQuery, OrderKey, SortDir};

    use super::super::{MAX_PAGE_SIZE, prepare_list_query};

    fn assert_order_keys(order: &ODataOrderBy, expected: &[(&str, SortDir)]) {
        assert_eq!(order.0.len(), expected.len(), "order arity mismatch");
        for (i, (field, dir)) in expected.iter().enumerate() {
            assert_eq!(order.0[i].field, *field, "order key #{i} field");
            assert_eq!(order.0[i].dir, *dir, "order key #{i} dir");
        }
    }

    fn extract_first_field_violation_reason(err: &CanonicalError) -> Option<String> {
        let CanonicalError::InvalidArgument { ctx, .. } = err else {
            return None;
        };
        match ctx {
            InvalidArgumentV1::FieldViolations { field_violations } => {
                field_violations.first().map(|v| v.reason.clone())
            }
            _ => None,
        }
    }

    fn extract_first_field_violation_field(err: &CanonicalError) -> Option<String> {
        let CanonicalError::InvalidArgument { ctx, .. } = err else {
            return None;
        };
        match ctx {
            InvalidArgumentV1::FieldViolations { field_violations } => {
                field_violations.first().map(|v| v.field.clone())
            }
            _ => None,
        }
    }

    #[test]
    fn limit_above_max_page_size_is_rejected_as_invalid_argument() {
        // A silent clamp would hand the caller a partial page that
        // looks complete; reject so paginators surface the cap.
        let mut q = ODataQuery::new();
        q.limit = Some(5_000);
        let err = prepare_list_query(q).expect_err("limit > cap must be rejected");
        assert!(
            matches!(err, CanonicalError::InvalidArgument { .. }),
            "limit > MAX_PAGE_SIZE must surface as InvalidArgument, got {err:?}",
        );
        assert_eq!(
            extract_first_field_violation_reason(&err).as_deref(),
            Some("VALIDATION"),
        );
    }

    #[test]
    fn limit_above_max_page_size_names_top_as_the_violating_field() {
        // `$top` and `limit` both fold onto `ODataQuery.limit`, so the
        // parsed query cannot say which spelling arrived. The violation
        // names the canonical one and the detail names the alias —
        // matching `toolkit_odata`'s own `InvalidLimit` mapping, so a
        // client dispatching on `field` sees one spelling platform-wide.
        let mut q = ODataQuery::new();
        q.limit = Some(5_000);
        let err = prepare_list_query(q).expect_err("limit > cap must be rejected");
        assert_eq!(
            extract_first_field_violation_field(&err).as_deref(),
            Some("$top"),
        );
    }

    #[test]
    fn limit_equal_to_max_page_size_is_preserved() {
        let mut q = ODataQuery::new();
        q.limit = Some(MAX_PAGE_SIZE);
        let out = prepare_list_query(q).expect("limit == cap is the boundary");
        assert_eq!(out.limit, Some(MAX_PAGE_SIZE));
    }

    #[test]
    fn limit_below_cap_is_preserved() {
        let mut q = ODataQuery::new();
        q.limit = Some(42);
        let out = prepare_list_query(q).expect("ok");
        assert_eq!(out.limit, Some(42));
    }

    #[test]
    fn missing_limit_defaults_to_max_page_size() {
        let out = prepare_list_query(ODataQuery::new()).expect("ok");
        assert_eq!(
            out.limit,
            Some(MAX_PAGE_SIZE),
            "absent limit defaults to MAX_PAGE_SIZE so the plugin never sees an unbounded read",
        );
    }

    #[test]
    fn empty_orderby_and_no_cursor_defaults_to_canonical_keyset() {
        let out = prepare_list_query(ODataQuery::new()).expect("ok");
        assert_order_keys(
            &out.order,
            &[("created_at", SortDir::Asc), ("id", SortDir::Asc)],
        );
    }

    #[test]
    fn supplied_orderby_gets_unique_tiebreaker_appended() {
        // The caller's explicit `$orderby` is preserved as
        // the leading sort key, but the gateway MUST append the canonical
        // `(created_at, id)` suffix so the effective order ends in a
        // globally-unique key. Without it the plugin keys against a
        // non-unique boundary and silently drops the tied rows that did not
        // fit on the previous page.
        let mut q = ODataQuery::new();
        q.order = ODataOrderBy(vec![OrderKey {
            field: "resource_id".into(),
            dir: SortDir::Desc,
        }]);
        let out = prepare_list_query(q).expect("ok");
        // Direction-aware: the tiebreaker is appended in the order's existing
        // direction so the plugin (uniform-direction keyset only) never sees
        // a mixed-direction tuple.
        assert_order_keys(
            &out.order,
            &[
                ("resource_id", SortDir::Desc),
                ("created_at", SortDir::Desc),
                ("id", SortDir::Desc),
            ],
        );
    }

    #[test]
    fn explicit_orderby_created_at_gets_id_tiebreaker() {
        // The exact reproduction: `$orderby=created_at` (no unique
        // final key). The gateway must append `id` so a page boundary at a
        // tied `created_at` cannot drop rows. `created_at` is already the
        // leading key, so `ensure_tiebreaker("created_at", …)` is a no-op and
        // only `id` is appended.
        let mut q = ODataQuery::new();
        q.order = ODataOrderBy(vec![OrderKey {
            field: "created_at".into(),
            dir: SortDir::Asc,
        }]);
        let out = prepare_list_query(q).expect("ok");
        assert_order_keys(
            &out.order,
            &[("created_at", SortDir::Asc), ("id", SortDir::Asc)],
        );
    }

    #[test]
    fn descending_orderby_appends_tiebreaker_in_same_direction() {
        // Direction handling: this plugin's keyset only supports
        // uniform-direction tuples, so the appended tiebreaker must follow
        // the caller's direction. A `created_at desc` order must normalize to
        // `(created_at desc, id desc)` — never `(created_at desc, id
        // asc)`, which the plugin would reject as a mixed-direction keyset.
        let mut q = ODataQuery::new();
        q.order = ODataOrderBy(vec![OrderKey {
            field: "created_at".into(),
            dir: SortDir::Desc,
        }]);
        let out = prepare_list_query(q).expect("ok");
        assert_order_keys(
            &out.order,
            &[("created_at", SortDir::Desc), ("id", SortDir::Desc)],
        );
    }

    #[test]
    fn mixed_direction_orderby_is_rejected_as_invalid_argument() {
        // The storage plugin's keyset supports only uniform-direction
        // tuples. A caller order that mixes ascending and descending keys
        // (e.g. `$orderby=created_at asc, value desc`) can only ever compose
        // into a mixed-direction keyset the plugin rejects downstream with a
        // late, non-specific error. Reject it up front with a typed 400 that
        // names the real cause (mixed sort directions) instead of leaking a
        // plugin-internal keyset error to the caller.
        let mut q = ODataQuery::new();
        q.order = ODataOrderBy(vec![
            OrderKey {
                field: "created_at".into(),
                dir: SortDir::Asc,
            },
            OrderKey {
                field: "value".into(),
                dir: SortDir::Desc,
            },
        ]);
        let err =
            prepare_list_query(q).expect_err("mixed-direction $orderby must be rejected up front");
        assert!(
            matches!(err, CanonicalError::InvalidArgument { .. }),
            "mixed-direction $orderby must surface as InvalidArgument, got {err:?}",
        );
        assert_eq!(
            extract_first_field_violation_reason(&err).as_deref(),
            Some("VALIDATION"),
        );
    }

    #[test]
    fn orderby_on_a_nullable_field_is_rejected_as_invalid_argument() {
        // The storage plugin's keyset continuation is a row-value tuple
        // comparison that is only sound over NOT NULL columns. `subject_id`,
        // `subject_type`, and `corrects_id` are domain-optional (nullable), so
        // a `$orderby` leading on one of them would silently drop NULL-keyed
        // rows from the page (and 500 on a page ending at a NULL row). Reject
        // it up front with a typed 400 that names the real cause instead of
        // leaking a plugin-internal keyset error — or, worse, an incomplete
        // page — to the caller.
        for field in ["subject_id", "subject_type", "corrects_id"] {
            let mut q = ODataQuery::new();
            q.order = ODataOrderBy(vec![OrderKey {
                field: field.into(),
                dir: SortDir::Asc,
            }]);
            let err =
                prepare_list_query(q).expect_err("$orderby on a nullable field must be rejected");
            assert!(
                matches!(err, CanonicalError::InvalidArgument { .. }),
                "$orderby on nullable `{field}` must surface as InvalidArgument, got {err:?}",
            );
            assert_eq!(
                extract_first_field_violation_reason(&err).as_deref(),
                Some("VALIDATION"),
            );
        }
    }

    #[test]
    fn orderby_on_a_mandatory_field_other_than_the_tiebreaker_is_accepted() {
        // Guard against over-rejection: `tenant_id` / `status` are mandatory
        // (NOT NULL) columns, so ordering by them is a valid keyset and must
        // still gain the canonical `(created_at, id)` suffix.
        for field in ["tenant_id", "status"] {
            let mut q = ODataQuery::new();
            q.order = ODataOrderBy(vec![OrderKey {
                field: field.into(),
                dir: SortDir::Asc,
            }]);
            let out =
                prepare_list_query(q).expect("ordering by a mandatory field is a valid keyset");
            assert_order_keys(
                &out.order,
                &[
                    (field, SortDir::Asc),
                    ("created_at", SortDir::Asc),
                    ("id", SortDir::Asc),
                ],
            );
        }
    }

    #[test]
    fn uniform_multi_key_orderby_is_accepted_and_tiebroken() {
        // A uniform-direction multi-key order (all `desc` here) is valid: it
        // is preserved and gains the canonical `id` suffix in the same
        // direction. Guards the mixed-direction rejection against
        // over-rejecting legitimate multi-key orders.
        let mut q = ODataQuery::new();
        q.order = ODataOrderBy(vec![
            OrderKey {
                field: "resource_id".into(),
                dir: SortDir::Desc,
            },
            OrderKey {
                field: "created_at".into(),
                dir: SortDir::Desc,
            },
        ]);
        let out = prepare_list_query(q).expect("uniform multi-key order is valid");
        assert_order_keys(
            &out.order,
            &[
                ("resource_id", SortDir::Desc),
                ("created_at", SortDir::Desc),
                ("id", SortDir::Desc),
            ],
        );
    }

    #[test]
    fn cursor_present_materializes_keyset_order_from_signed_tokens() {
        // Regression (cursor-continuation 500): the toolkit OData extractor
        // leaves `order` empty on a cursor request — the effective keyset
        // order lives in the cursor's signed-token payload (`cursor.s`).
        // The storage plugin reads `query.order` *directly* to build both
        // the `ORDER BY` and the keyset continuation predicate; it has no
        // access to the cursor's token derivation. So `prepare_list_query`
        // MUST materialize the cursor-derived order back into `query.order`.
        // Before the fix it was derived "for comparison purposes only" and
        // the empty order propagated to the plugin, which 500'd with
        // "keyset order must not be empty" on every cursor follow-up.
        let mut q = ODataQuery::new();
        q.cursor = Some(CursorV1 {
            k: vec!["2026-06-12T00:00:00Z".into(), uuid::Uuid::nil().to_string()],
            o: SortDir::Asc,
            s: "+created_at,+id".to_owned(),
            f: None,
            d: "fwd".to_owned(),
        });
        // Note: q.order intentionally empty (mirrors the toolkit extractor).
        let out = prepare_list_query(q).expect("cursor-driven request validates");
        assert_order_keys(
            &out.order,
            &[("created_at", SortDir::Asc), ("id", SortDir::Asc)],
        );
    }

    // `validate_cursor_against` derives the effective order from
    // `cursor.s` itself in our wrapper, so an `OrderMismatch` between
    // a caller-supplied `$orderby` and the cursor's bound order can
    // never originate inside `prepare_list_query` directly — the
    // toolkit's OData extractor already rejects "cursor + $orderby"
    // combinations as `Error::OrderWithCursor` upstream. The
    // architectural decision is documented in
    // `prepare_list_query`'s docstring; no dedicated regression test
    // is needed for an unreachable branch.

    #[test]
    fn cursor_filter_hash_mismatch_surfaces_filter_mismatch_reason() {
        let mut q = ODataQuery::new();
        // Caller filter hashed to "hash_current".
        q.filter = Some(Box::new(Expr::Compare(
            Box::new(Expr::Identifier("status".into())),
            CompareOperator::Eq,
            Box::new(Expr::Value(Value::String("active".into()))),
        )));
        q.filter_hash = Some("hash_current".into());
        q.cursor = Some(CursorV1 {
            k: vec!["x".into()],
            o: SortDir::Asc,
            s: "+created_at,+id".to_owned(),
            f: Some("hash_DIFFERENT".into()),
            d: "fwd".to_owned(),
        });
        let err = prepare_list_query(q).expect_err("filter hash divergence rejects");
        let reason = extract_first_field_violation_reason(&err)
            .expect("canonical envelope carries a field violation");
        assert_eq!(reason, "FILTER_MISMATCH");
    }

    #[test]
    fn cursor_with_malformed_signed_tokens_surfaces_invalid_orderby_field() {
        // `from_signed_tokens` rejects an empty `s` as InvalidOrderByField,
        // which lifts to canonical InvalidArgument carrying reason
        // "INVALID_ORDERBY_FIELD" — NOT one of the cursor categories,
        // but the same fail-closed posture.
        let mut q = ODataQuery::new();
        q.cursor = Some(CursorV1 {
            k: vec!["x".into()],
            o: SortDir::Asc,
            s: String::new(), // malformed
            f: None,
            d: "fwd".to_owned(),
        });
        let err = prepare_list_query(q).expect_err("empty signed tokens reject");
        assert!(
            matches!(err, CanonicalError::InvalidArgument { .. }),
            "malformed cursor signed tokens MUST lift to canonical InvalidArgument",
        );
    }

    #[test]
    fn happy_path_cursor_with_matching_hash_and_order_passes() {
        let mut q = ODataQuery::new();
        q.filter_hash = Some("h0".into());
        q.cursor = Some(CursorV1 {
            k: vec!["k".into(), "u".into()],
            o: SortDir::Asc,
            s: "+created_at,+id".to_owned(),
            f: Some("h0".into()),
            d: "fwd".to_owned(),
        });
        let out = prepare_list_query(q).expect("matching cursor passes through");
        assert!(out.cursor.is_some());
        // The cursor-derived keyset order is materialized into `query.order`
        // so the storage plugin can build the ORDER BY / keyset predicate.
        assert_order_keys(
            &out.order,
            &[("created_at", SortDir::Asc), ("id", SortDir::Asc)],
        );
    }
}

// ---------------------------------------------------------------------------
// parse_metadata_filters — repeated query-param grouping
// ---------------------------------------------------------------------------

mod parse_metadata_filters_tests {
    use toolkit_canonical_errors::CanonicalError;

    use super::super::parse_metadata_filters;

    fn p(key: &str, value: &str) -> (String, String) {
        (key.to_owned(), value.to_owned())
    }

    #[test]
    fn no_metadata_entries_yields_empty_vec() {
        let out = parse_metadata_filters(&[p("gts_id", "x")]).expect("ok");
        assert!(out.is_empty());
    }

    #[test]
    fn single_key_with_one_value_makes_one_filter() {
        let out = parse_metadata_filters(&[p("metadata.user_id", "u1")]).expect("ok");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].key().as_str(), "user_id");
        assert_eq!(out[0].values(), &["u1".to_owned()]);
    }

    #[test]
    fn multiple_values_for_same_key_collapse_into_one_filter() {
        let out = parse_metadata_filters(&[
            p("metadata.user_id", "u1"),
            p("metadata.user_id", "u2"),
            p("metadata.user_id", "u3"),
        ])
        .expect("ok");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].key().as_str(), "user_id");
        assert_eq!(
            out[0].values(),
            &["u1".to_owned(), "u2".to_owned(), "u3".to_owned()],
        );
    }

    #[test]
    fn distinct_keys_make_distinct_filters_in_sorted_order() {
        let out = parse_metadata_filters(&[
            p("metadata.region", "eu"),
            p("metadata.user_id", "u1"),
            p("metadata.account", "acme"),
        ])
        .expect("ok");
        // BTreeMap inside the helper gives deterministic key order.
        let keys: Vec<_> = out.iter().map(|f| f.key().as_str().to_owned()).collect();
        assert_eq!(keys, vec!["account", "region", "user_id"]);
    }

    #[test]
    fn empty_key_is_rejected_as_invalid_argument() {
        let err = parse_metadata_filters(&[p("metadata.", "x")])
            .expect_err("metadata. with no key MUST fail");
        assert!(matches!(err, CanonicalError::InvalidArgument { .. }));
    }

    #[test]
    fn non_metadata_params_are_ignored() {
        let out = parse_metadata_filters(&[
            p("gts_id", "g"),
            p("from", "f"),
            p("metadata.k", "v"),
            p("$filter", "x"),
        ])
        .expect("ok");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].key().as_str(), "k");
    }

    #[test]
    fn distinct_key_count_at_cap_is_admitted_but_one_over_is_rejected() {
        // Boundary check on `MAX_METADATA_FILTERS`. At the cap the helper
        // returns Ok; one over the cap MUST surface as `InvalidArgument`
        // with the violation field pinned to `metadata` (NOT to a
        // specific key) so the caller learns it is the cardinality, not
        // any single key, that breached the cap.
        use super::super::MAX_METADATA_FILTERS;

        let at_cap: Vec<(String, String)> = (0..MAX_METADATA_FILTERS)
            .map(|i| p(&format!("metadata.k{i}"), "v"))
            .collect();
        let out = parse_metadata_filters(&at_cap).expect("at-cap distinct keys must be admitted");
        assert_eq!(out.len(), MAX_METADATA_FILTERS);

        let over_cap: Vec<(String, String)> = (0..=MAX_METADATA_FILTERS)
            .map(|i| p(&format!("metadata.k{i}"), "v"))
            .collect();
        let err = parse_metadata_filters(&over_cap)
            .expect_err("MAX_METADATA_FILTERS + 1 distinct keys MUST reject");
        let field = match err {
            CanonicalError::InvalidArgument { ctx, .. } => match ctx {
                toolkit_canonical_errors::context::InvalidArgumentV1::FieldViolations {
                    field_violations,
                } => field_violations
                    .first()
                    .map(|v| v.field.clone())
                    .unwrap_or_default(),
                _ => String::new(),
            },
            _ => panic!("over-cap distinct keys MUST surface as InvalidArgument"),
        };
        assert_eq!(
            field, "metadata",
            "over-cap distinct-key rejection MUST blame the bag (`metadata`), \
             not any specific `metadata.<key>`",
        );
    }

    #[test]
    fn per_key_value_count_at_cap_is_admitted_but_one_over_is_rejected() {
        // Boundary check on `MAX_METADATA_FILTER_VALUES` — the per-key
        // value-list cap that bounds the OR-within-key expansion at the
        // plugin. The over-cap violation MUST scope its `field` to the
        // offending `metadata.<key>` so the caller can correct the
        // specific repeated query parameter.
        use super::super::MAX_METADATA_FILTER_VALUES;

        let at_cap: Vec<(String, String)> = (0..MAX_METADATA_FILTER_VALUES)
            .map(|i| p("metadata.k", &format!("v{i}")))
            .collect();
        let out = parse_metadata_filters(&at_cap).expect("at-cap per-key values must be admitted");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].values().len(), MAX_METADATA_FILTER_VALUES);

        let over_cap: Vec<(String, String)> = (0..=MAX_METADATA_FILTER_VALUES)
            .map(|i| p("metadata.k", &format!("v{i}")))
            .collect();
        let err = parse_metadata_filters(&over_cap)
            .expect_err("MAX_METADATA_FILTER_VALUES + 1 values on one key MUST reject");
        let field = match err {
            CanonicalError::InvalidArgument { ctx, .. } => match ctx {
                toolkit_canonical_errors::context::InvalidArgumentV1::FieldViolations {
                    field_violations,
                } => field_violations
                    .first()
                    .map(|v| v.field.clone())
                    .unwrap_or_default(),
                _ => String::new(),
            },
            _ => panic!("over-cap per-key values MUST surface as InvalidArgument"),
        };
        assert_eq!(
            field, "metadata.k",
            "over-cap per-key-value rejection MUST blame the specific \
             `metadata.<key>` whose value list breached the cap",
        );
    }
}

// ---------------------------------------------------------------------------
// parse_required_gts_id — typed mandatory query parameter
// ---------------------------------------------------------------------------
//
// `parse_required_gts_id` is the only place the gateway lifts `gts_id`
// query-string presence + validity into the typed `UsageTypeGtsId`. The
// service tests cannot reach this surface (they construct the typed
// value directly), and the wire-level `handle_list_usage_records` tests
// would conflate three failure modes into the same 400. Pin each
// failure mode separately here so a regression in one path doesn't get
// masked by another.

mod parse_required_gts_id_tests {
    use toolkit_canonical_errors::CanonicalError;
    use toolkit_canonical_errors::context::InvalidArgumentV1;

    use super::super::parse_required_gts_id;
    use super::HAPPY_RECORD_GTS_ID;

    fn p(key: &str, value: &str) -> (String, String) {
        (key.to_owned(), value.to_owned())
    }

    fn first_violation(err: &CanonicalError) -> (String, String, String) {
        match err {
            CanonicalError::InvalidArgument {
                ctx: InvalidArgumentV1::FieldViolations { field_violations },
                ..
            } => {
                let v = field_violations.first().expect("at least one violation");
                (v.field.clone(), v.reason.clone(), v.description.clone())
            }
            _ => panic!("expected InvalidArgument with field_violations"),
        }
    }

    #[test]
    fn missing_gts_id_param_rejects_as_missing_required() {
        let err =
            parse_required_gts_id(&[p("$filter", "x")]).expect_err("missing gts_id MUST reject");
        let (field, reason, description) = first_violation(&err);
        assert_eq!(field, "gts_id");
        assert_eq!(
            reason, "VALIDATION",
            "host-private wire-shape rejection uses the gateway-side \
             `VALIDATION` reason, NOT the SDK's `INVALID_BASE_GTS_ID`",
        );
        assert!(
            description.contains("missing required query parameter"),
            "description MUST cite the missing-required idiom (got `{description}`)",
        );
    }

    #[test]
    fn duplicate_gts_id_param_rejects_instead_of_silently_last_winning() {
        // Two `gts_id=…` entries with DIFFERENT values: last-wins
        // would silently mask the caller bug. The helper MUST reject
        // outright. Pin BOTH the field/reason and that the rejection
        // happens regardless of whether either value is well-formed.
        let valid_gts = HAPPY_RECORD_GTS_ID.to_owned();
        let err =
            parse_required_gts_id(&[p("gts_id", &valid_gts), p("gts_id", "even.something.else")])
                .expect_err("duplicate gts_id MUST reject");
        let (field, reason, description) = first_violation(&err);
        assert_eq!(field, "gts_id");
        assert_eq!(reason, "VALIDATION");
        assert!(
            description.contains("at most once"),
            "duplicate-rejection description MUST cite the at-most-once \
             contract (got `{description}`)",
        );
    }

    #[test]
    fn malformed_gts_id_lifts_through_sdk_invalid_base_gts_id() {
        // A single but malformed `gts_id` must surface the SDK-side
        // `INVALID_BASE_GTS_ID` reason — NOT the host's `VALIDATION`
        // bucket — so caller-facing diagnostics distinguish "wrong
        // shape" from "missing / duplicate".
        let err = parse_required_gts_id(&[p("gts_id", "not-a-valid-prefix")])
            .expect_err("malformed gts_id MUST reject");
        let (field, reason, _) = first_violation(&err);
        assert_eq!(field, "gts_id");
        assert_eq!(reason, "INVALID_BASE_GTS_ID");
    }

    #[test]
    fn well_formed_gts_id_round_trips_through_the_typed_newtype() {
        let raw = HAPPY_RECORD_GTS_ID;
        let parsed = parse_required_gts_id(&[p("gts_id", raw)]).expect("well-formed gts_id passes");
        assert_eq!(AsRef::<str>::as_ref(&parsed), raw);
    }
}

// ---------------------------------------------------------------------------
// reject_unknown_list_params — the list allowlist admits both page-size
// spellings and nothing beyond the declared set.
//
// `toolkit::api::odata::ODataParams` declares `limit` with
// `#[serde(alias = "$top")]`, so `$top` and `limit` fold onto the same
// `ODataQuery.limit` slot. An allowlist entry with no binding behind it
// would be worse than a rejection — it would accept the parameter,
// ignore it, and hand back a full page the caller reads as their
// requested page — so each admitted spelling must be one the extractor
// actually binds.
// ---------------------------------------------------------------------------

mod reject_unknown_list_params_tests {
    use toolkit_canonical_errors::CanonicalError;
    use toolkit_canonical_errors::context::InvalidArgumentV1;

    use super::super::reject_unknown_list_params;

    fn p(key: &str, value: &str) -> (String, String) {
        (key.to_owned(), value.to_owned())
    }

    fn violating_field(err: &CanonicalError) -> Option<String> {
        let CanonicalError::InvalidArgument { ctx, .. } = err else {
            return None;
        };
        match ctx {
            InvalidArgumentV1::FieldViolations { field_violations } => {
                field_violations.first().map(|v| v.field.clone())
            }
            _ => None,
        }
    }

    #[test]
    fn allowed_params_pass() {
        reject_unknown_list_params(&[
            p("$filter", "created_at ge 1 and created_at lt 2"),
            p("$orderby", "created_at asc"),
            p("limit", "10"),
            p("cursor", "opaque"),
            p("gts_id", "g"),
            p("metadata.user_id", "u1"),
        ])
        .expect("the list allowlist admits every documented parameter");
    }

    #[test]
    fn top_is_admitted_as_the_canonical_page_size_spelling() {
        // `$top` is canonical OData (OASIS OData 4.01 Part 2 §5.1.6) and
        // the toolkit extractor binds it as an alias of `limit`. Rejecting
        // it here would refuse a page size the platform honours.
        reject_unknown_list_params(&[p("gts_id", "g"), p("$top", "5")])
            .expect("`$top` MUST be admitted: the extractor binds it onto `ODataQuery.limit`");
    }

    #[test]
    fn select_is_rejected_because_no_projection_is_applied() {
        // The toolkit extractor parses `$select` into `ODataQuery.select`,
        // but this gear never reads that field: the handler returns whole
        // DTOs and the plugin selects a fixed column list. Admitting it
        // would answer `200` with every field to a caller who asked for
        // one, which reads as a satisfied projection rather than an
        // unsupported parameter.
        let err = reject_unknown_list_params(&[p("gts_id", "g"), p("$select", "id")])
            .expect_err("`$select` MUST be rejected: no code path applies the projection");
        assert_eq!(
            violating_field(&err).as_deref(),
            Some("$select"),
            "the violation MUST name `$select` so the caller learns which parameter is unsupported",
        );
    }

    #[test]
    fn unknown_parameter_is_rejected_with_field_naming_the_offender() {
        let err = reject_unknown_list_params(&[p("unknown_param", "x")])
            .expect_err("unknown param MUST reject");
        assert_eq!(violating_field(&err).as_deref(), Some("unknown_param"));
    }
}

// ---------------------------------------------------------------------------
// reject_unknown_aggregate_params — aggregate allowlist is STRICTER than list
//
// The list path admits `$top`, `cursor`, `$orderby`, and `limit`; the
// aggregate path intentionally rejects them (the aggregation result is
// not paginated). Verify the asymmetry directly — a regression that
// copy-pasted the list allowlist into the aggregate validator would not
// be caught by any other test.
// ---------------------------------------------------------------------------

mod reject_unknown_aggregate_params_tests {
    use super::super::reject_unknown_aggregate_params;

    fn p(key: &str, value: &str) -> (String, String) {
        (key.to_owned(), value.to_owned())
    }

    #[test]
    fn allowed_params_pass() {
        // `$filter`, typed `gts_id`, and `metadata.<key>` entries are
        // explicitly admitted on the aggregate path.
        reject_unknown_aggregate_params(&[
            p("$filter", "x"),
            p("gts_id", "g"),
            p("metadata.user_id", "u1"),
        ])
        .expect("aggregate allowlist admits $filter, gts_id, metadata.<key>");
    }

    #[test]
    fn list_only_params_are_rejected_on_aggregate_path() {
        // Each of these is on `OUR_ODATA_PARAMS` for the LIST path but
        // NOT on `AGGREGATE_ODATA_PARAMS`. The aggregate validator MUST
        // reject them — silent admission would let a caller paginate an
        // unpaginated endpoint and ship an inconsistent wire contract.
        //
        // `$select` is deliberately absent: it is rejected on BOTH paths,
        // so it demonstrates no asymmetry. Its list-path rejection is
        // pinned by `reject_unknown_list_params_tests`.
        for forbidden in ["$top", "cursor", "limit", "$orderby"] {
            assert!(
                reject_unknown_aggregate_params(&[p(forbidden, "v")]).is_err(),
                "`{forbidden}` MUST be rejected on the aggregate path - \
                 list-only OData parameters cannot leak through here",
            );
        }
    }

    #[test]
    fn unknown_parameter_is_rejected_with_field_naming_the_offender() {
        // The violation MUST identify which parameter was unrecognised
        // so the caller can fix THAT parameter, not guess.
        let err = reject_unknown_aggregate_params(&[p("unknown_param", "x")])
            .expect_err("unknown param MUST reject");
        let field = match err {
            toolkit_canonical_errors::CanonicalError::InvalidArgument { ctx, .. } => match ctx {
                toolkit_canonical_errors::context::InvalidArgumentV1::FieldViolations {
                    field_violations,
                } => field_violations
                    .first()
                    .map(|v| v.field.clone())
                    .unwrap_or_default(),
                _ => String::new(),
            },
            _ => panic!("unknown-param rejection MUST be InvalidArgument"),
        };
        assert_eq!(field, "unknown_param");
    }
}

// ---------------------------------------------------------------------------
// handle_create_usage_records — batch-size guard.
//
// The handler mirrors the service's `1..=MAX_BATCH_RECORDS` gate. The
// short-circuit MUST refuse the request BEFORE the per-record loop
// allocates, so a regression that drops the gate would let the service
// see an oversized batch and a denial-of-service vector reopens.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn create_with_empty_batch_rejects_with_invalid_batch_size_before_service() {
    let (service, resolver) = service_with_sentinel_pdp();

    let response = handle_create_usage_records(
        Extension(SecurityContext::anonymous()),
        Extension(service),
        Json(CreateUsageRecordsRequest {
            records: Vec::new(),
        }),
    )
    .await
    .into_response();

    assert_eq!(
        response.status(),
        StatusCode::BAD_REQUEST,
        "empty batch MUST lift to 400 (NOT 200 with an empty results array)",
    );
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    assert!(
        content_type.contains("problem+json"),
        "InvalidBatchSize MUST surface as application/problem+json (got `{content_type}`)",
    );
    assert_eq!(
        resolver.calls(),
        0,
        "batch-size gate MUST short-circuit BEFORE reaching the PDP",
    );
}

#[tokio::test]
async fn create_with_batch_above_cap_rejects_without_iterating_records() {
    use crate::domain::service::MAX_BATCH_RECORDS;

    let (service, resolver) = service_with_sentinel_pdp();

    // One over the cap: gate MUST refuse the request as a whole, NOT
    // walk the per-record loop and emit MAX_BATCH_RECORDS + 1 `Rejected`
    // entries.
    let oversize = MAX_BATCH_RECORDS + 1;
    let records: Vec<_> = (0..oversize)
        .map(|i| CreateUsageRecordRequest {
            gts_id: HAPPY_RECORD_GTS_ID.to_owned(),
            tenant_id: Uuid::from_u128(2),
            resource_ref: ResourceRefDto {
                resource_id: format!("rsc-{i}"),
                resource_type: "compute.vm".to_owned(),
            },
            subject_ref: None,
            metadata: BTreeMap::new(),
            value: rust_decimal::Decimal::from(1),
            idempotency_key: format!("idem-oversize-{i}"),
            corrects_id: None,
            created_at: OffsetDateTime::UNIX_EPOCH,
        })
        .collect();

    let response = handle_create_usage_records(
        Extension(SecurityContext::anonymous()),
        Extension(service),
        Json(CreateUsageRecordsRequest { records }),
    )
    .await
    .into_response();

    assert_eq!(
        response.status(),
        StatusCode::BAD_REQUEST,
        "MAX_BATCH_RECORDS + 1 records MUST lift to 400 InvalidBatchSize",
    );

    // The wire payload MUST be a single canonical Problem (NOT a 207
    // envelope with per-record rejections) so the contract surface is
    // unambiguous about whether the batch was even considered.
    let body_bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body collected");
    let body: serde_json::Value =
        serde_json::from_slice(&body_bytes).expect("Problem body is JSON");
    assert!(
        body.get("results").is_none(),
        "oversize-batch rejection MUST NOT surface a 207-shape `results` envelope \
         (got body: {body})",
    );
    assert!(
        body.get("status").and_then(serde_json::Value::as_u64) == Some(400),
        "Problem body MUST carry status 400",
    );

    assert_eq!(
        resolver.calls(),
        0,
        "oversize-batch gate MUST short-circuit BEFORE reaching the PDP",
    );
}

// ---------------------------------------------------------------------------
// handle_list_usage_records — wire-boundary validation + round-trip
//
// The list handler is otherwise untested at the wire boundary. The
// service-layer tests cover authorize / compose / dispatch; these tests
// pin two handler-only concerns:
//
//   1. Pre-service validation rejections (missing / malformed `gts_id`,
//      unknown query parameter, cursor / filter-hash mismatch) surface
//      as a `400` canonical envelope before the service runs.
//   2. A successful list lifts the plugin's `Page<UsageRecord>` to the
//      wire as `Page<UsageRecordDto>` — items projected via
//      `UsageRecordDto::from`, `page_info` carried through verbatim.
// ---------------------------------------------------------------------------

mod handle_list_usage_records_tests {
    use std::sync::Arc;

    use axum::extract::{Extension, Query};
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use toolkit::api::canonical_prelude::OData;
    use toolkit::client_hub::ClientHub;
    use toolkit_odata::{CursorV1, ODataQuery, Page as ODataPage, PageInfo, SortDir};
    use toolkit_security::{SecurityContext, pep_properties};
    use uuid::Uuid;

    use super::super::handle_list_usage_records;
    use super::{HAPPY_RECORD_GTS_ID, sample_persisted_record};
    use crate::domain::Service;
    use crate::domain::test_support::{
        CountingPermitResolver, CountingUnreachableResolver, HappyPathPlugin, authenticated_ctx,
        enforcer_for, hub_with_plugin,
    };

    fn service_no_plugin() -> Arc<Service> {
        let hub = Arc::new(ClientHub::new());
        let resolver = CountingUnreachableResolver::new();
        let enforcer = enforcer_for(Arc::clone(&resolver) as _);
        Arc::new(Service::new(hub, "constructorfabric".to_owned(), enforcer))
    }

    fn service_with_permit_plugin(plugin: &Arc<HappyPathPlugin>, suffix: &str) -> Arc<Service> {
        let hub = hub_with_plugin(
            Arc::clone(plugin) as Arc<dyn usage_collector_sdk::UsageCollectorPluginV1>,
            suffix,
            "constructorfabric",
        );
        let resolver = CountingPermitResolver::new(
            pep_properties::OWNER_TENANT_ID,
            Uuid::from_u128(2).to_string(),
        );
        let enforcer = enforcer_for(Arc::clone(&resolver) as _);
        Arc::new(Service::new(hub, "constructorfabric".to_owned(), enforcer))
    }

    #[tokio::test]
    async fn missing_gts_id_returns_400() {
        let service = service_no_plugin();

        let response = handle_list_usage_records(
            Extension(SecurityContext::anonymous()),
            Extension(service),
            Query(Vec::new()),
            OData(ODataQuery::new()),
        )
        .await
        .into_response();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn malformed_gts_id_returns_400() {
        // A single but shape-invalid `gts_id` must surface through the
        // SDK's `InvalidUsageTypeGtsId` mapping as a 400 — covering
        // gts_id-shape rejections, not just missing / duplicate /
        // unknown-param.
        let service = service_no_plugin();

        let response = handle_list_usage_records(
            Extension(SecurityContext::anonymous()),
            Extension(service),
            Query(vec![("gts_id".to_owned(), "not-a-valid-prefix".to_owned())]),
            OData(ODataQuery::new()),
        )
        .await
        .into_response();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn unknown_query_parameter_returns_400() {
        // A parameter that is neither an OData token, a typed
        // (`gts_id`), nor a `metadata.<key>` entry MUST be refused
        // rather than silently dropped — silent drop is a documented
        // contract-drift surface.
        let service = service_no_plugin();

        let response = handle_list_usage_records(
            Extension(SecurityContext::anonymous()),
            Extension(service),
            Query(vec![
                ("gts_id".to_owned(), HAPPY_RECORD_GTS_ID.to_owned()),
                ("totally_unknown".to_owned(), "x".to_owned()),
            ]),
            OData(ODataQuery::new()),
        )
        .await
        .into_response();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn cursor_filter_hash_mismatch_returns_400() {
        // A continuation cursor whose embedded filter-hash no longer
        // matches the request's `filter_hash` MUST be refused with a 400
        // rather than silently resumed against a different filter.
        let service = service_no_plugin();

        let mut q = ODataQuery::new();
        q.filter_hash = Some("hash_current".into());
        q.cursor = Some(CursorV1 {
            k: vec!["k".into()],
            o: SortDir::Asc,
            s: "+created_at,+id".to_owned(),
            f: Some("hash_DIFFERENT".into()),
            d: "fwd".to_owned(),
        });

        let response = handle_list_usage_records(
            Extension(SecurityContext::anonymous()),
            Extension(service),
            Query(vec![("gts_id".to_owned(), HAPPY_RECORD_GTS_ID.to_owned())]),
            OData(q),
        )
        .await
        .into_response();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn happy_path_maps_page_items_through_dto_and_preserves_page_info() {
        // Plugin returns a 2-item `Page<UsageRecord>` with a non-default
        // `PageInfo`. The handler MUST:
        //   1. project each item via `UsageRecordDto::from` (id +
        //      lowercase `status` are the cheapest, regression-prone
        //      witnesses), and
        //   2. carry `page_info` verbatim (`next_cursor`, `prev_cursor`,
        //      `limit`).
        let plugin = HappyPathPlugin::new();
        let item_a = sample_persisted_record(Uuid::new_v4(), Uuid::from_u128(2));
        let item_b = sample_persisted_record(Uuid::new_v4(), Uuid::from_u128(2));
        let expected_uuids = [item_a.id, item_b.id];
        plugin.set_list_usage_records_response(ODataPage::new(
            vec![item_a, item_b],
            PageInfo {
                next_cursor: Some("next-cursor-blob".to_owned()),
                prev_cursor: Some("prev-cursor-blob".to_owned()),
                limit: 137,
            },
        ));

        let service = service_with_permit_plugin(&plugin, "test.handler.list_records.happy.v1");

        let response = handle_list_usage_records(
            Extension(authenticated_ctx()),
            Extension(service),
            Query(vec![("gts_id".to_owned(), HAPPY_RECORD_GTS_ID.to_owned())]),
            OData(super::bounded_window_query()),
        )
        .await
        .into_response();

        assert_eq!(response.status(), StatusCode::OK);
        let body_bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body collected");
        let body: serde_json::Value = serde_json::from_slice(&body_bytes).expect("body is JSON");

        let items = body
            .get("items")
            .and_then(serde_json::Value::as_array)
            .expect("wire body MUST carry `items`");
        assert_eq!(items.len(), 2);
        let actual_uuids: Vec<_> = items
            .iter()
            .map(|i| {
                i.get("id")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_owned()
            })
            .collect();
        assert_eq!(
            actual_uuids,
            expected_uuids.map(|u| u.to_string()).to_vec(),
            "items[i].id MUST echo plugin order (preserves keyset \
             pagination contract)",
        );
        for (i, item) in items.iter().enumerate() {
            assert_eq!(
                item.get("status").and_then(serde_json::Value::as_str),
                Some("active"),
                "items[{i}].status MUST be lowercase `active` (projection \
                 through UsageRecordDto::from)",
            );
        }

        let page_info = body
            .get("page_info")
            .expect("wire body MUST carry `page_info`");
        assert_eq!(
            page_info
                .get("next_cursor")
                .and_then(serde_json::Value::as_str),
            Some("next-cursor-blob"),
        );
        assert_eq!(
            page_info
                .get("prev_cursor")
                .and_then(serde_json::Value::as_str),
            Some("prev-cursor-blob"),
        );
        assert_eq!(
            page_info.get("limit").and_then(serde_json::Value::as_u64),
            Some(137),
            "page_info.limit MUST carry the plugin-reported limit \
             verbatim (NOT the gateway MAX_PAGE_SIZE cap)",
        );
    }
}

// ---------------------------------------------------------------------------
// handle_query_aggregated_usage_records — wire validation + body projection
//
// The aggregate handler shares the metadata + gts_id surface with list,
// but its allowlist is stricter (no `$top`, no `cursor`) and it ships
// the `AggregationSpec` in the body. The tests here pin only what list
// tests can't: the stricter allowlist, the body lift through
// `TryFrom<QueryAggregatedUsageRecordsRequest>`, and the result
// projection through `AggregationResultDto`.
// ---------------------------------------------------------------------------

mod handle_query_aggregated_usage_records_tests {
    use std::collections::BTreeSet;
    use std::sync::Arc;
    use toolkit_gts::gts_id;

    use axum::Json;
    use axum::extract::{Extension, Query};
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use bigdecimal::BigDecimal;
    use toolkit::api::canonical_prelude::OData;
    use toolkit::client_hub::ClientHub;
    use toolkit_odata::ODataQuery;
    use toolkit_security::{SecurityContext, pep_properties};
    use usage_collector_sdk::{
        AggregationBucket, AggregationResult, UsageKind, UsageType, UsageTypeGtsId,
    };
    use uuid::Uuid;

    use super::super::handle_query_aggregated_usage_records;
    use crate::api::rest::dto::{
        AggregationDimensionDto, AggregationOpDto, QueryAggregatedUsageRecordsRequest,
    };
    use crate::domain::Service;
    use crate::domain::test_support::{
        CountingPermitResolver, CountingUnreachableResolver, HappyPathPlugin, authenticated_ctx,
        enforcer_for, hub_with_plugin,
    };

    const VALID_GTS_ID: &str =
        gts_id!("cf.core.uc.usage_record.v1~cf.mini_chat._.tokens_consumed.v1");

    fn service_no_plugin() -> Arc<Service> {
        let hub = Arc::new(ClientHub::new());
        let resolver = CountingUnreachableResolver::new();
        let enforcer = enforcer_for(Arc::clone(&resolver) as _);
        Arc::new(Service::new(hub, "constructorfabric".to_owned(), enforcer))
    }

    fn service_with_permit_plugin(plugin: &Arc<HappyPathPlugin>, suffix: &str) -> Arc<Service> {
        let hub = hub_with_plugin(
            Arc::clone(plugin) as Arc<dyn usage_collector_sdk::UsageCollectorPluginV1>,
            suffix,
            "constructorfabric",
        );
        let resolver = CountingPermitResolver::new(
            pep_properties::OWNER_TENANT_ID,
            Uuid::from_u128(2).to_string(),
        );
        let enforcer = enforcer_for(Arc::clone(&resolver) as _);
        Arc::new(Service::new(hub, "constructorfabric".to_owned(), enforcer))
    }

    fn sum_no_group() -> QueryAggregatedUsageRecordsRequest {
        QueryAggregatedUsageRecordsRequest {
            op: AggregationOpDto::Sum,
            group_by: Vec::new(),
        }
    }

    #[tokio::test]
    async fn missing_gts_id_on_aggregate_path_returns_400() {
        // The aggregate path uses the same `parse_required_gts_id`
        // helper as list; a missing `gts_id` MUST be refused with a 400
        // here too.
        let service = service_no_plugin();

        let response = handle_query_aggregated_usage_records(
            Extension(SecurityContext::anonymous()),
            Extension(service),
            Query(Vec::new()),
            OData(ODataQuery::new()),
            Json(sum_no_group()),
        )
        .await
        .into_response();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn malformed_gts_id_on_aggregate_path_returns_400() {
        // Parallel to the list-side `malformed_gts_id_returns_400` test:
        // a shape-invalid `gts_id` on the aggregate path MUST surface
        // through `parse_required_gts_id` as a 400.
        let service = service_no_plugin();

        let response = handle_query_aggregated_usage_records(
            Extension(SecurityContext::anonymous()),
            Extension(service),
            Query(vec![("gts_id".to_owned(), "not-a-valid-prefix".to_owned())]),
            OData(ODataQuery::new()),
            Json(sum_no_group()),
        )
        .await
        .into_response();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn cursor_parameter_is_rejected_on_aggregate_path() {
        // `cursor` is allowed by the LIST allowlist but NOT by the
        // aggregate allowlist — aggregation is not paginated. The
        // handler MUST refuse it with a 400.
        let service = service_no_plugin();

        let response = handle_query_aggregated_usage_records(
            Extension(SecurityContext::anonymous()),
            Extension(service),
            Query(vec![
                ("gts_id".to_owned(), VALID_GTS_ID.to_owned()),
                ("cursor".to_owned(), "any-blob".to_owned()),
            ]),
            OData(ODataQuery::new()),
            Json(sum_no_group()),
        )
        .await
        .into_response();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn aggregation_body_with_invalid_metadata_key_lifts_through_tryfrom() {
        // `AggregationDimension::Metadata` carries a typed
        // `MetadataKey` — an empty / oversized key in the body MUST
        // surface through `AggregationSpec::try_from`'s host-side
        // canonical lift, NOT bypass the typed boundary. Pinned with
        // the empty-string key, which the SDK rejects on
        // `MetadataKey::new`.
        let service = service_no_plugin();

        let response = handle_query_aggregated_usage_records(
            Extension(SecurityContext::anonymous()),
            Extension(service),
            Query(vec![("gts_id".to_owned(), VALID_GTS_ID.to_owned())]),
            OData(ODataQuery::new()),
            Json(QueryAggregatedUsageRecordsRequest {
                op: AggregationOpDto::Sum,
                group_by: vec![AggregationDimensionDto::Metadata(String::new())],
            }),
        )
        .await
        .into_response();

        assert_eq!(
            response.status(),
            StatusCode::BAD_REQUEST,
            "empty metadata-dimension key MUST refuse at the handler boundary",
        );
    }

    #[tokio::test]
    async fn happy_path_projects_aggregation_result_through_dto() {
        // Plugin returns a 2-bucket `AggregationResult`; the handler
        // MUST surface a 200 OK body whose `buckets` array projects
        // each bucket through `AggregationBucketDto` — `key` carried
        // verbatim, `value` serialised as a decimal string per the
        // `bigdecimal_str_option` contract.
        let plugin = HappyPathPlugin::new();
        plugin.set_get_usage_type(UsageType {
            gts_id: UsageTypeGtsId::new(VALID_GTS_ID).expect("valid gts_id"),
            kind: UsageKind::Counter,
            metadata_fields: BTreeSet::new(),
        });
        plugin.set_query_aggregated_usage_records_response(AggregationResult {
            buckets: vec![
                AggregationBucket {
                    key: vec!["eu".to_owned()],
                    value: Some(BigDecimal::from(42)),
                },
                AggregationBucket {
                    key: vec!["us".to_owned()],
                    value: None,
                },
            ],
        });

        let service = service_with_permit_plugin(&plugin, "test.handler.aggregate.happy.v1");

        let response = handle_query_aggregated_usage_records(
            Extension(authenticated_ctx()),
            Extension(service),
            Query(vec![("gts_id".to_owned(), VALID_GTS_ID.to_owned())]),
            OData(super::bounded_window_query()),
            Json(QueryAggregatedUsageRecordsRequest {
                op: AggregationOpDto::Sum,
                group_by: vec![AggregationDimensionDto::ResourceType],
            }),
        )
        .await
        .into_response();

        assert_eq!(response.status(), StatusCode::OK);
        let body_bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body collected");
        let body: serde_json::Value = serde_json::from_slice(&body_bytes).expect("body is JSON");
        let buckets = body
            .get("buckets")
            .and_then(serde_json::Value::as_array)
            .expect("wire body MUST carry `buckets`");
        assert_eq!(buckets.len(), 2);

        assert_eq!(
            buckets[0]
                .get("key")
                .and_then(serde_json::Value::as_array)
                .and_then(|a| a.first())
                .and_then(serde_json::Value::as_str),
            Some("eu"),
        );
        assert_eq!(
            buckets[0].get("value").and_then(serde_json::Value::as_str),
            Some("42"),
            "non-empty bucket value MUST serialise as the decimal string \
             form, NOT a JSON number (the float-round-trip safety \
             requires the bigdecimal_str_option codec)",
        );
        assert!(
            buckets[1]
                .get("value")
                .is_none_or(serde_json::Value::is_null),
            "empty-set bucket value MUST serialise as null per the \
             SDK contract for `MIN over an empty set` etc.",
        );
    }
}
