//! The three REST routes of the platform-plane API (T9), driven through the real
//! `register_routes` and `Router::oneshot`.
//!
//! Routing through `register_routes` rather than a hand-built `Router` is
//! deliberate: it is what puts the actual paths, the `OperationBuilder`
//! registration and the `Extension` wiring under test. A bare router would pass
//! while the route was registered at the wrong path or without its auth stage.
//!
//! Authentication is not exercised here — `.authenticated()` is enforced by
//! api-gateway's layers, which this harness does not build — so what these tests
//! cover is the contract: status codes, headers, problem documents and the
//! submit-then-poll shape.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use toolkit::api::{OpenApiRegistry, ResponseHeaderType};
use toolkit_gts::{gts_id, gts_uri};
use tower::ServiceExt;

use types_registry::api::rest::routes::{V1, V2};
use types_registry::config::TypesRegistryConfig;
use types_registry::domain::admission::{NullDispatch, OperationDispatch};
use types_registry::domain::policy::RegistrationPolicy;
use types_registry::domain::registry_service::{AdmissionMode, RegistryService};
use types_registry::domain::service::TypesRegistryService;
use types_registry::infra::InMemoryGtsRepository;

mod common;
use common::{stores, test_db};

const CF_TYPE: &str = gts_id!("cf.core.example.type.v1~");
/// An Instance of [`CF_TYPE`]: a full five-token last segment with no trailing `~`.
const CF_INSTANCE: &str = gts_id!("cf.core.example.type.v1~cf.core.example.first.v1");
const INVALID_ARGUMENT_TYPE: &str =
    gts_uri!("cf.core.errors.err.v1~cf.core.err.invalid_argument.v1~");
const HTTP_REQUEST_RESOURCE_TYPE: &str = gts_id!("cf.core.http.request.v1~");

/// One parameter as the generated document will carry it: name, location, and
/// whether it is required.
type DeclaredParam = (String, String, bool);
/// One response as the generated document will carry it: status and content type.
type DeclaredResponse = (u16, String);
/// One response header: status, name, and JSON Schema scalar type.
type DeclaredResponseHeader = (u16, String, ResponseHeaderType);

/// Records what `register_routes` declares, so the generated document is
/// assertable instead of eyeballed in `/cf/docs`.
#[derive(Default)]
struct TestOpenApi {
    /// `(method, path, operation_id)` in registration order.
    operations: std::sync::Mutex<Vec<(String, String, String)>>,
    /// `operation_id -> [(param name, location, required)]`.
    params: std::sync::Mutex<Vec<(String, Vec<DeclaredParam>)>>,
    /// `operation_id -> [(status, content type)]`.
    responses: std::sync::Mutex<Vec<(String, Vec<DeclaredResponse>)>>,
    /// `operation_id -> [(status, header name, scalar type)]`.
    response_headers: std::sync::Mutex<Vec<(String, Vec<DeclaredResponseHeader>)>>,
    /// `operation_id -> gateway visibility`.
    exposure: std::sync::Mutex<Vec<(String, bool)>>,
}

impl OpenApiRegistry for TestOpenApi {
    fn register_operation(&self, spec: &toolkit::api::OperationSpec) {
        self.operations.lock().expect("operations lock").push((
            spec.method.to_string(),
            spec.path.clone(),
            spec.operation_id.clone().unwrap_or_default(),
        ));
        self.params.lock().expect("params lock").push((
            spec.operation_id.clone().unwrap_or_default(),
            spec.params
                .iter()
                .map(|p| (p.name.clone(), format!("{:?}", p.location), p.required))
                .collect(),
        ));
        self.responses.lock().expect("responses lock").push((
            spec.operation_id.clone().unwrap_or_default(),
            spec.responses
                .iter()
                .map(|r| (r.status, r.content_type.to_owned()))
                .collect(),
        ));
        self.response_headers
            .lock()
            .expect("response headers lock")
            .push((
                spec.operation_id.clone().unwrap_or_default(),
                spec.responses
                    .iter()
                    .flat_map(|response| {
                        response.headers.iter().map(move |header| {
                            (response.status, header.name.clone(), header.header_type)
                        })
                    })
                    .collect(),
            ));
        self.exposure
            .lock()
            .expect("exposure lock")
            .push((spec.operation_id.clone().unwrap_or_default(), spec.exposed));
    }
    fn ensure_schema_raw(
        &self,
        root_name: &str,
        _schemas: Vec<(
            String,
            utoipa::openapi::RefOr<utoipa::openapi::schema::Schema>,
        )>,
    ) -> String {
        root_name.to_owned()
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// A router with both services wired, as `register_rest` builds it.
async fn router_with_db() -> Router {
    router_with(false).await
}

/// The same router with the legacy service ready, so v1 answers instead of refusing
/// on `is_ready()`.
async fn router_with_v1_ready() -> Router {
    router_with(true).await
}

async fn router_with(v1_ready: bool) -> Router {
    let db = test_db().await;
    let openapi = TestOpenApi::default();
    let config = TypesRegistryConfig::default();
    let legacy = Arc::new(TypesRegistryService::new(
        Arc::new(InMemoryGtsRepository::new(config.to_gts_config())),
        config.clone(),
    ));
    if v1_ready {
        legacy.switch_to_ready().expect("switch legacy to ready");
    }
    let dispatch: Arc<dyn OperationDispatch> = Arc::new(NullDispatch);
    let registry = Arc::new(RegistryService::new(
        db.db(),
        stores(),
        RegistrationPolicy::default(),
        config,
        dispatch,
        // Admission inline, as `init()` wires it until T21.
        AdmissionMode::Inline,
        common::metrics(),
    ));
    types_registry::api::rest::routes::register_routes(
        Router::new(),
        &openapi,
        legacy,
        Some(registry),
    )
}

/// The same routes with no database bound — `no-db.yaml` and `--mock`. Ready,
/// because the point of the case is that v1 still serves.
fn router_without_db() -> Router {
    let openapi = TestOpenApi::default();
    let config = TypesRegistryConfig::default();
    let legacy = Arc::new(TypesRegistryService::new(
        Arc::new(InMemoryGtsRepository::new(config.to_gts_config())),
        config,
    ));
    legacy.switch_to_ready().expect("switch legacy to ready");
    types_registry::api::rest::routes::register_routes(Router::new(), &openapi, legacy, None)
}

fn schema(gts_id: &str) -> Value {
    json!({
        "$id": format!("gts://{gts_id}"),
        "$schema": "http://json-schema.org/draft-07/schema#",
        "type": "object",
        "properties": { "name": { "type": "string" } },
    })
}

struct Response {
    status: StatusCode,
    content_type: Option<String>,
    location: Option<String>,
    retry_after: Option<String>,
    idempotency_replayed: Option<String>,
    body: Value,
}

async fn call(router: &Router, req: Request<Body>) -> Response {
    let resp = router.clone().oneshot(req).await.expect("router dispatch");
    let status = resp.status();
    let content_type = resp
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let location = resp
        .headers()
        .get(axum::http::header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let retry_after = resp
        .headers()
        .get(axum::http::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let idempotency_replayed = resp
        .headers()
        .get("idempotency-replayed")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("read body");
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes)
            .unwrap_or(Value::String(String::from_utf8_lossy(&bytes).into_owned()))
    };
    Response {
        status,
        content_type,
        location,
        retry_after,
        idempotency_replayed,
        body,
    }
}

fn assert_invalid_argument_rejection(
    response: &Response,
    expected_status: StatusCode,
    expected_field: &str,
    expected_reason: &str,
) {
    assert_eq!(response.status, expected_status, "got: {:?}", response.body);
    assert_eq!(
        response.content_type.as_deref(),
        Some("application/problem+json"),
    );
    assert_eq!(response.body["type"], json!(INVALID_ARGUMENT_TYPE));
    assert_eq!(response.body["title"], json!("Invalid Argument"));
    assert_eq!(response.body["status"], json!(expected_status.as_u16()));
    assert_eq!(response.body["detail"], json!("Request validation failed"));
    assert_eq!(
        response.body["context"]["resource_type"],
        json!(HTTP_REQUEST_RESOURCE_TYPE),
    );

    let violations = response.body["context"]["field_violations"]
        .as_array()
        .expect("field_violations is an array");
    assert_eq!(violations.len(), 1, "got: {:?}", response.body);
    assert_eq!(violations[0]["field"], json!(expected_field));
    assert_eq!(violations[0]["reason"], json!(expected_reason));
    assert!(
        violations[0]["description"]
            .as_str()
            .is_some_and(|description| !description.is_empty()),
        "the rejection carries a public description: {:?}",
        response.body,
    );
    for forbidden in ["stack", "trace", "backtrace"] {
        assert!(
            response.body.get(forbidden).is_none(),
            "the Problem must not expose `{forbidden}`: {:?}",
            response.body,
        );
    }
}

fn submit(key: Option<&str>, body: &Value) -> Request<Body> {
    submit_to(&format!("{V2}/entities"), key, body)
}

fn submit_to(uri: &str, key: Option<&str>, body: &Value) -> Request<Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json");
    if let Some(key) = key {
        builder = builder.header("idempotency-key", key);
    }
    builder
        .body(Body::from(serde_json::to_vec(body).expect("serialize")))
        .expect("request")
}

fn get(uri: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(uri)
        .body(Body::empty())
        .expect("request")
}

fn one_candidate(gts_id: &str) -> Value {
    json!({ "items": [{ "gts_id": gts_id, "content": schema(gts_id) }] })
}

// ---------------------------------------------------------------------------
// Canonical extractor rejections
// ---------------------------------------------------------------------------

#[tokio::test]
async fn malformed_submission_json_is_a_canonical_invalid_argument() {
    let router = router_with_db().await;
    let request = Request::post(format!("{V2}/entities"))
        .header("content-type", "application/json")
        .header("idempotency-key", "malformed-json")
        .body(Body::from("{not-json}"))
        .expect("request");

    let response = call(&router, request).await;

    assert_invalid_argument_rejection(
        &response,
        StatusCode::BAD_REQUEST,
        "body",
        "json_syntax_error",
    );
}

#[tokio::test]
async fn invalid_list_query_is_a_canonical_invalid_argument() {
    let router = router_with_v1_ready().await;

    let response = call(&router, get(&format!("{V1}/entities?is_schema=not-a-bool"))).await;

    assert_invalid_argument_rejection(
        &response,
        StatusCode::BAD_REQUEST,
        "query",
        "invalid_query_string",
    );
}

#[tokio::test]
async fn invalid_operation_uuid_is_a_canonical_invalid_argument() {
    let router = router_with_db().await;

    let response = call(&router, get(&format!("{V2}/operations/not-a-uuid"))).await;

    assert_invalid_argument_rejection(
        &response,
        StatusCode::BAD_REQUEST,
        "path",
        "invalid_path_params",
    );
}

// ---------------------------------------------------------------------------
// The submit-then-poll contract
// ---------------------------------------------------------------------------

/// `202` with the operation's `Location` and an advisory `Retry-After`, then the
/// operation and the entity are both readable. This is Checkpoint 1's first item
/// as a single test.
#[tokio::test]
async fn a_registration_is_accepted_polled_and_read_back() {
    let router = router_with_db().await;

    let accepted = call(&router, submit(Some("key-1"), &one_candidate(CF_TYPE))).await;
    assert_eq!(accepted.status, StatusCode::ACCEPTED);
    let operation_id = accepted.body["operation_id"]
        .as_str()
        .expect("operation_id");
    let location = accepted
        .location
        .as_deref()
        .expect("a 202 carries Location");
    assert!(
        location.ends_with(&format!("{V2}/operations/{operation_id}")),
        "the receipt must point at the operation, prefix and all: {location}",
    );
    assert_eq!(
        accepted.retry_after.as_deref(),
        Some("1"),
        "advisory only, but present on 202",
    );
    assert_eq!(accepted.body["replayed"], json!(false));

    let operation = call(&router, get(&format!("{V2}/operations/{operation_id}"))).await;
    assert_eq!(operation.status, StatusCode::OK);
    assert_eq!(operation.body["status"], json!("completed"));
    assert_eq!(operation.body["kind"], json!("registration"));
    assert_eq!(operation.body["dry_run"], json!(false));
    let item = &operation.body["items"][0];
    assert_eq!(item["gts_id"], json!(CF_TYPE));
    assert_eq!(item["status"], json!("succeeded"));
    assert_eq!(item["resource_version"], json!(1));
    assert!(
        item.get("expected_resource_version").is_none(),
        "the operation outcome must not echo the request precondition",
    );
    assert!(
        item.get("revision_no").is_none(),
        "the operation outcome must expose resource_version, not an internal revision number",
    );

    let entity = call(&router, get(&format!("{V2}/entities/{CF_TYPE}"))).await;
    assert_eq!(entity.status, StatusCode::OK);
    assert_eq!(entity.body["gts_id"], json!(CF_TYPE));
    assert_eq!(entity.body["kind"], json!("type_schema"));
    assert_eq!(entity.body["lifecycle_status"], json!("active"));
    assert_eq!(entity.body["resource_version"], json!(1));
    // D3: the artifacts are materialized, so a read recomputes nothing.
    assert!(entity.body["resolved_schema"].is_object());
    assert!(entity.body["effective_traits"].is_object());
    assert!(entity.body["content"].is_object());
}

/// An Instance reads back with its authored value, exactly as a Type Schema reads
/// back with its document.
///
/// Regression: the read path once asked the Type Schema store alone, so an admitted
/// Instance answered `200` with `content: null` while its operation said `succeeded`.
///
/// The three `effective_*` artifacts stay absent, and that is the contract rather
/// than the same gap: an Instance has no derived state (T10 — its value is authored
/// and its schema revision immutable), so there is nothing to materialize.
#[tokio::test]
async fn an_instance_reads_back_with_its_authored_value() {
    let router = router_with_db().await;
    call(&router, submit(Some("key-type"), &one_candidate(CF_TYPE))).await;

    let value = json!({ "name": "first" });
    let accepted = call(
        &router,
        submit(
            Some("key-instance"),
            &json!({ "items": [{ "gts_id": CF_INSTANCE, "content": value }] }),
        ),
    )
    .await;
    assert_eq!(accepted.status, StatusCode::ACCEPTED);

    // Polled before the read: a refused candidate would otherwise be indistinguishable
    // from a value the read failed to reach, which is the very confusion this covers.
    let operation_id = accepted.body["operation_id"]
        .as_str()
        .expect("operation_id");
    let operation = call(&router, get(&format!("{V2}/operations/{operation_id}"))).await;
    assert_eq!(operation.body["items"][0]["status"], json!("succeeded"));
    assert!(operation.body["items"][0].get("revision_no").is_none());

    let entity = call(&router, get(&format!("{V2}/entities/{CF_INSTANCE}"))).await;
    assert_eq!(entity.status, StatusCode::OK);
    assert_eq!(entity.body["gts_id"], json!(CF_INSTANCE));
    assert_eq!(entity.body["kind"], json!("instance"));
    assert_eq!(entity.body["lifecycle_status"], json!("active"));
    assert_eq!(entity.body["resource_version"], json!(1));
    assert_eq!(
        entity.body["content"], value,
        "the authored value, byte for byte what was submitted",
    );
    assert!(
        entity.body.get("revision_no").is_none(),
        "the immutable content revision remains internal; writes use resource_version",
    );
    assert!(
        entity.body["resolved_schema"].is_null()
            && entity.body["effective_traits"].is_null()
            && entity.body["effective_traits_schema"].is_null(),
        "an Instance has no derived artifacts; absent is the answer, not a gap: {:?}",
        entity.body,
    );
}

/// The same Instance by its Registry Reference. The key classifier is kind-agnostic,
/// and this pins that the *value* survives the UUID path too — the branch this read
/// now takes is chosen by the row's kind, after the lookup, not by how it was found.
#[tokio::test]
async fn an_instance_is_readable_by_registry_reference() {
    let router = router_with_db().await;
    call(&router, submit(Some("key-type"), &one_candidate(CF_TYPE))).await;
    call(
        &router,
        submit(
            Some("key-instance"),
            &json!({ "items": [{ "gts_id": CF_INSTANCE, "content": { "name": "first" } }] }),
        ),
    )
    .await;

    let uuid = gts::GtsId::try_new(CF_INSTANCE)
        .expect("identifier")
        .to_uuid();
    let by_uuid = call(&router, get(&format!("{V2}/entities/{uuid}"))).await;
    assert_eq!(by_uuid.status, StatusCode::OK);
    assert_eq!(by_uuid.body["gts_id"], json!(CF_INSTANCE));
    assert_eq!(by_uuid.body["content"], json!({ "name": "first" }));
}

/// The same entity by its Registry Reference. Both keys name one row, which is why
/// the route takes `{entity_key}` rather than `{gts_id}`.
#[tokio::test]
async fn an_entity_is_readable_by_registry_reference() {
    let router = router_with_db().await;
    call(&router, submit(Some("key-1"), &one_candidate(CF_TYPE))).await;

    let uuid = gts::GtsId::try_new(CF_TYPE).expect("identifier").to_uuid();
    let by_uuid = call(&router, get(&format!("{V2}/entities/{uuid}"))).await;
    assert_eq!(by_uuid.status, StatusCode::OK);
    assert_eq!(by_uuid.body["gts_id"], json!(CF_TYPE));
    assert_eq!(by_uuid.body["gts_uuid"], json!(uuid.to_string()));
}

/// A replay of a terminal operation is `200`, not `202`: there is nothing left to
/// wait for, and `202` would tell the caller otherwise.
#[tokio::test]
async fn a_terminal_replay_answers_200() {
    let router = router_with_db().await;
    let first = call(&router, submit(Some("key-1"), &one_candidate(CF_TYPE))).await;
    assert_eq!(first.status, StatusCode::ACCEPTED);
    assert!(
        first.idempotency_replayed.is_none(),
        "a fresh submission must not be marked as replayed",
    );

    let replay = call(&router, submit(Some("key-1"), &one_candidate(CF_TYPE))).await;
    assert_eq!(replay.status, StatusCode::OK);
    assert_eq!(replay.body["replayed"], json!(true));
    assert_eq!(replay.body["operation_id"], first.body["operation_id"]);
    assert!(
        replay.retry_after.is_none(),
        "there is nothing to retry after",
    );
    assert_eq!(
        replay.idempotency_replayed.as_deref(),
        Some("true"),
        "an idempotent replay must carry the standard response signal",
    );
}

/// A different body under one key is a conflict, as an RFC-9457 problem document.
#[tokio::test]
async fn a_different_request_under_one_key_is_a_conflict_problem() {
    let router = router_with_db().await;
    call(&router, submit(Some("key-1"), &one_candidate(CF_TYPE))).await;

    let mut different = schema(CF_TYPE);
    different["title"] = json!("something else");
    let conflict = call(
        &router,
        submit(
            Some("key-1"),
            &json!({ "items": [{ "gts_id": CF_TYPE, "content": different }] }),
        ),
    )
    .await;
    assert_eq!(conflict.status, StatusCode::CONFLICT);
    assert!(
        conflict.body["type"].is_string() && conflict.body["title"].is_string(),
        "errors are RFC-9457 problem details, not raw status tuples: {:?}",
        conflict.body,
    );
}

/// A `Location` exists to be followed, so this follows it — under a prefix, which is
/// how every gear is actually mounted (`api-gateway` nests the router under
/// `prefix_path`, `/cf` in `quickstart.yaml`).
///
/// A gear-relative constant would be a `404` for any client that took the receipt at
/// its word: RFC 9110 §10.2.2 resolves `Location` against the effective request URI,
/// and an absolute-path reference discards the prefix. `nest` also rewrites the URI
/// the handler sees, which is why the path comes from `OriginalUri` — with `Uri` this
/// test fails exactly as the hardcoded string does.
#[tokio::test]
async fn the_receipt_is_followable_under_a_gateway_prefix() {
    let prefixed = Router::new().nest("/cf", router_with_db().await);

    let accepted = call(
        &prefixed,
        submit_to(
            &format!("/cf{V2}/entities"),
            Some("key-1"),
            &one_candidate(CF_TYPE),
        ),
    )
    .await;
    assert_eq!(accepted.status, StatusCode::ACCEPTED);
    let location = accepted
        .location
        .as_deref()
        .expect("a 202 carries Location");
    assert_eq!(
        location,
        format!(
            "/cf{V2}/operations/{}",
            accepted.body["operation_id"]
                .as_str()
                .expect("operation_id")
        ),
    );

    // The claim, made the only way that means anything: the receipt is followed
    // verbatim, against the same prefixed router the client would be talking to.
    let followed = call(&prefixed, get(location)).await;
    assert_eq!(
        followed.status,
        StatusCode::OK,
        "a client that follows the receipt must reach the operation",
    );
    assert_eq!(followed.body["status"], json!("completed"));

    // And the unprefixed path — the value the header used to carry — is a 404 on this
    // router, so the assertion above is not passing by accident.
    let unprefixed = call(
        &prefixed,
        get(&format!(
            "{V2}/operations/{}",
            accepted.body["operation_id"]
                .as_str()
                .expect("operation_id")
        )),
    )
    .await;
    assert_eq!(unprefixed.status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// Refusals, all synchronous and all problem documents
// ---------------------------------------------------------------------------

/// The `Idempotency-Key` is required, and its absence is refused **before** any
/// operation exists — a generated key would turn every retry into a new operation.
#[tokio::test]
async fn a_missing_idempotency_key_is_a_synchronous_refusal() {
    let router = router_with_db().await;
    let refused = call(&router, submit(None, &one_candidate(CF_TYPE))).await;
    assert_eq!(refused.status, StatusCode::BAD_REQUEST);
    assert!(refused.body["type"].is_string(), "{:?}", refused.body);

    // Nothing was accepted, so nothing is readable.
    let entity = call(&router, get(&format!("{V2}/entities/{CF_TYPE}"))).await;
    assert_eq!(entity.status, StatusCode::NOT_FOUND);
}

/// Dry Run is part of the final P0 contract, but its rollback-only worker path
/// lands at T20. Until then it must fail synchronously rather than leave a
/// non-terminal operation behind.
#[tokio::test]
async fn a_dry_run_is_a_synchronous_refusal_until_t20() {
    let router = router_with_db().await;
    let mut body = one_candidate(CF_TYPE);
    body["dry_run"] = json!(true);

    let refused = call(&router, submit(Some("dry-run-key"), &body)).await;
    assert_eq!(refused.status, StatusCode::BAD_REQUEST);
    let text = serde_json::to_string(&refused.body).expect("serialize");
    assert!(
        text.contains("dry_run"),
        "the problem must name the unsupported field: {text}",
    );
    assert!(
        refused.body.get("operation_id").is_none(),
        "a synchronous refusal must not return an operation receipt",
    );

    let entity = call(&router, get(&format!("{V2}/entities/{CF_TYPE}"))).await;
    assert_eq!(entity.status, StatusCode::NOT_FOUND);
}

/// A header that was sent but cannot be decoded is told apart from one that was not
/// sent at all: "required" would send the caller looking for a bug it does not have.
#[tokio::test]
async fn a_non_utf8_idempotency_key_is_not_reported_as_a_missing_one() {
    let router = router_with_db().await;
    let request = Request::builder()
        .method("POST")
        .uri(format!("{V2}/entities"))
        .header("content-type", "application/json")
        .header(
            "idempotency-key",
            axum::http::HeaderValue::from_bytes(&[0xff, 0xfe]).expect("a valid header value"),
        )
        .body(Body::from(
            serde_json::to_vec(&one_candidate(CF_TYPE)).expect("serialize"),
        ))
        .expect("request");

    let refused = call(&router, request).await;
    assert_eq!(refused.status, StatusCode::BAD_REQUEST);
    let text = serde_json::to_string(&refused.body).expect("serialize");
    assert!(
        text.contains("not valid UTF-8"),
        "the detail must name what is wrong with the header: {text}",
    );
    assert!(
        !text.contains("required"),
        "a key that was sent must not be reported as missing: {text}",
    );
}

/// A closed region refuses a declared creation, and the problem document carries
/// the region and the parameter — the two things an operator has to edit.
#[tokio::test]
async fn a_closed_region_is_refused_with_the_region_named() {
    let router = router_with_db().await;
    let acme = gts_id!("acme.crm.customer.type.v1~");
    let refused = call(&router, submit(Some("key-1"), &one_candidate(acme))).await;
    // `failed_precondition` maps to 400 in this toolkit's canonical ladder, which
    // is the gRPC-to-HTTP convention. The status is not what carries the meaning
    // here — the precondition violation naming the region and the parameter is.
    assert_eq!(refused.status, StatusCode::BAD_REQUEST);
    let text = serde_json::to_string(&refused.body).expect("serialize");
    assert!(
        text.contains("allowed_vendors"),
        "the parameter must be named: {text}",
    );
}

/// A literal `0` precondition is refused; omitting the field is how must-not-exist
/// is spelled.
#[tokio::test]
async fn a_zero_precondition_is_refused() {
    let router = router_with_db().await;
    let body = json!({
        "items": [{
            "gts_id": CF_TYPE,
            "content": schema(CF_TYPE),
            "expected_resource_version": 0,
        }]
    });
    let refused = call(&router, submit(Some("key-1"), &body)).await;
    assert_eq!(refused.status, StatusCode::BAD_REQUEST);
}

/// The review's probe, as a standing test: naming `expected_resource_version` must
/// not get a candidate into a **closed** region.
///
/// A revision bypasses SPEC §8.1's policy gate at acceptance — otherwise closing a
/// region would freeze the entities already in it — so this is accepted and then
/// refused *terminally by the worker*, which requires the identifier to exist at the
/// named version. What this pins is that it is never committed as an ordinary
/// creation inside the closed region.
#[tokio::test]
async fn naming_a_version_does_not_get_a_candidate_past_a_closed_region() {
    let router = router_with_db().await;
    let acme = gts_id!("acme.crm.customer.type.v1~");
    let body = json!({
        "items": [{
            "gts_id": acme,
            "content": schema(acme),
            "expected_resource_version": 7,
        }]
    });

    let accepted = call(&router, submit(Some("key-1"), &body)).await;
    assert_eq!(accepted.status, StatusCode::ACCEPTED);
    let operation_id = accepted.body["operation_id"]
        .as_str()
        .expect("operation_id");

    let operation = call(&router, get(&format!("{V2}/operations/{operation_id}"))).await;
    let item = &operation.body["items"][0];
    assert_eq!(item["status"], json!("failed"));
    let text = serde_json::to_string(&item["error"]).expect("serialize");
    assert!(
        text.contains("precondition_failed"),
        "the item must name the precondition it lost: {text}",
    );

    // The claim that matters: no entity exists in the closed region.
    let entity = call(&router, get(&format!("{V2}/entities/{acme}"))).await;
    assert_eq!(entity.status, StatusCode::NOT_FOUND);
}

/// The standing read criterion for anything that extends the write path: whatever
/// becomes storable is readable through the public route.
#[tokio::test]
async fn a_revision_is_readable_through_the_entity_route() {
    let router = router_with_db().await;
    call(&router, submit(Some("key-1"), &one_candidate(CF_TYPE))).await;

    let mut revised = schema(CF_TYPE);
    revised["title"] = json!("revised");
    let body = json!({
        "items": [{
            "gts_id": CF_TYPE,
            "content": revised,
            "expected_resource_version": 1,
        }]
    });
    let accepted = call(&router, submit(Some("key-2"), &body)).await;
    assert_eq!(accepted.status, StatusCode::ACCEPTED);
    let operation_id = accepted.body["operation_id"]
        .as_str()
        .expect("operation_id");
    let operation = call(&router, get(&format!("{V2}/operations/{operation_id}"))).await;
    assert_eq!(operation.body["items"][0]["status"], json!("succeeded"));
    assert_eq!(operation.body["items"][0]["resource_version"], json!(2));

    let entity = call(&router, get(&format!("{V2}/entities/{CF_TYPE}"))).await;
    assert_eq!(entity.status, StatusCode::OK);
    assert_eq!(entity.body["resource_version"], json!(2));
    assert_eq!(entity.body["content"]["title"], json!("revised"));
    assert_eq!(
        entity.body["resolved_schema"]["title"],
        json!("revised"),
        "the artifacts were re-materialized with the pointer, not left on revision 1",
    );
}

/// Resubmitting the current content is terminal and successful, and says so with
/// its own status rather than as a second `succeeded`.
#[tokio::test]
async fn unchanged_content_reports_unchanged_on_the_operation() {
    let router = router_with_db().await;
    call(&router, submit(Some("key-1"), &one_candidate(CF_TYPE))).await;

    let body = json!({
        "items": [{
            "gts_id": CF_TYPE,
            "content": schema(CF_TYPE),
            "expected_resource_version": 1,
        }]
    });
    let accepted = call(&router, submit(Some("key-2"), &body)).await;
    let operation_id = accepted.body["operation_id"]
        .as_str()
        .expect("operation_id");
    let operation = call(&router, get(&format!("{V2}/operations/{operation_id}"))).await;
    let item = &operation.body["items"][0];
    assert_eq!(item["status"], json!("unchanged"));
    assert_eq!(item["resource_version"], json!(1));

    let entity = call(&router, get(&format!("{V2}/entities/{CF_TYPE}"))).await;
    assert_eq!(entity.body["resource_version"], json!(1));
}

/// An absent operation and an absent entity are both `404` problem documents
/// rather than empty `200`s.
#[tokio::test]
async fn absent_resources_are_not_found_problems() {
    let router = router_with_db().await;

    let operation = call(
        &router,
        get(&format!(
            "{V2}/operations/00000000-0000-0000-0000-000000000001"
        )),
    )
    .await;
    assert_eq!(operation.status, StatusCode::NOT_FOUND);
    assert!(operation.body["type"].is_string());

    let entity = call(
        &router,
        get(&format!(
            "{V2}/entities/{}",
            gts_id!("cf.core.absent.type.v1~")
        )),
    )
    .await;
    assert_eq!(entity.status, StatusCode::NOT_FOUND);
    assert!(entity.body["type"].is_string());
}

/// A candidate that fails admission on its merits is **not** a failed request: the
/// submission is accepted, and the refusal is the item's outcome.
#[tokio::test]
async fn a_candidate_refused_by_admission_surfaces_through_the_operation() {
    let router = router_with_db().await;
    let dangling = json!({
        "$id": format!("gts://{CF_TYPE}"),
        "$schema": "http://json-schema.org/draft-07/schema#",
        "allOf": [{ "$ref": format!("gts://{}", gts_id!("cf.core.absent.type.v1~")) }],
    });
    let accepted = call(
        &router,
        submit(
            Some("key-1"),
            &json!({ "items": [{ "gts_id": CF_TYPE, "content": dangling }] }),
        ),
    )
    .await;
    assert_eq!(accepted.status, StatusCode::ACCEPTED);

    let operation_id = accepted.body["operation_id"]
        .as_str()
        .expect("operation_id");
    let operation = call(&router, get(&format!("{V2}/operations/{operation_id}"))).await;
    assert_eq!(operation.body["status"], json!("completed"));
    let item = &operation.body["items"][0];
    assert_eq!(item["status"], json!("failed"));
    assert_eq!(
        item["error"]["reason"],
        json!("invalid_schema"),
        "the reason travels as a field, not as prose: {:?}",
        item["error"],
    );
}

// ---------------------------------------------------------------------------
// No database bound
// ---------------------------------------------------------------------------

/// With no database bound the routes still exist and answer `503` — a problem
/// document naming the cause beats a `404` suggesting the API changed.
#[tokio::test]
async fn without_a_database_the_routes_report_service_unavailable() {
    let router = router_without_db();

    for req in [
        submit(Some("key-1"), &one_candidate(CF_TYPE)),
        get(&format!(
            "{V2}/operations/00000000-0000-0000-0000-000000000001"
        )),
        get(&format!(
            "{V2}/entities/{}",
            gts_id!("cf.core.example.type.v1~")
        )),
    ] {
        let resp = call(&router, req).await;
        assert_eq!(
            resp.status,
            StatusCode::SERVICE_UNAVAILABLE,
            "got {:?}",
            resp.body
        );
    }
}

/// **The two stores do not see each other**, and neither route falls back to the
/// other on a miss. A fallback would report an entity as registered when the
/// admission meant to persist it never ran — the accident P6 exists to prevent.
#[tokio::test]
async fn a_v1_registration_is_absent_from_v2_and_the_reverse() {
    let router = router_with_v1_ready().await;

    // v1 registers into the in-memory repository, on `main`'s request shape.
    let v1_id = gts_id!("cf.core.example.v1only.v1~");
    let registered = call(
        &router,
        Request::post(format!("{V1}/entities"))
            .header("content-type", "application/json")
            .body(Body::from(
                json!({ "entities": [schema(v1_id)] }).to_string(),
            ))
            .expect("v1 request"),
    )
    .await;
    assert_eq!(
        registered.status,
        StatusCode::OK,
        "got {:?}",
        registered.body
    );
    assert_eq!(
        registered.body["summary"]["succeeded"], 1,
        "got {:?}",
        registered.body
    );

    // It is readable on v1 and absent on v2.
    assert_eq!(
        call(&router, get(&format!("{V1}/entities/{v1_id}")))
            .await
            .status,
        StatusCode::OK,
    );
    assert_eq!(
        call(&router, get(&format!("{V2}/entities/{v1_id}")))
            .await
            .status,
        StatusCode::NOT_FOUND,
        "a v1 registration must not be visible on the database surface",
    );

    // v2 admits into the database.
    let v2_id = gts_id!("cf.core.example.v2only.v1~");
    let accepted = call(&router, submit(Some("v2-only"), &one_candidate(v2_id))).await;
    assert_eq!(
        accepted.status,
        StatusCode::ACCEPTED,
        "got {:?}",
        accepted.body
    );

    // It is readable on v2 and absent on v1.
    assert_eq!(
        call(&router, get(&format!("{V2}/entities/{v2_id}")))
            .await
            .status,
        StatusCode::OK,
    );
    assert_eq!(
        call(&router, get(&format!("{V1}/entities/{v2_id}")))
            .await
            .status,
        StatusCode::NOT_FOUND,
        "a v2 admission must not be visible on the in-memory surface",
    );
}

/// With no database bound v2 degrades alone: a `--mock` or `no-db.yaml` deployment
/// keeps the contract it had before this branch.
#[tokio::test]
async fn without_a_database_the_v1_routes_still_serve() {
    let router = router_without_db();
    let id = gts_id!("cf.core.example.nodb.v1~");

    let registered = call(
        &router,
        Request::post(format!("{V1}/entities"))
            .header("content-type", "application/json")
            .body(Body::from(json!({ "entities": [schema(id)] }).to_string()))
            .expect("v1 request"),
    )
    .await;
    assert_eq!(
        registered.status,
        StatusCode::OK,
        "got {:?}",
        registered.body
    );
    assert_eq!(
        registered.body["summary"]["succeeded"], 1,
        "got {:?}",
        registered.body
    );

    assert_eq!(
        call(&router, get(&format!("{V1}/entities/{id}")))
            .await
            .status,
        StatusCode::OK,
    );
    assert_eq!(
        call(&router, get(&format!("{V1}/entities"))).await.status,
        StatusCode::OK,
    );
}

/// The `/cf/docs` check as a test. `OperationBuilder` registers two routes under one
/// `operation_id` without complaint and the second replaces the first in the
/// generated document — invisible in the router, which keys on method and path.
#[test]
fn both_versions_are_declared_with_distinct_operation_ids() {
    let openapi = TestOpenApi::default();
    let config = TypesRegistryConfig::default();
    let legacy = Arc::new(TypesRegistryService::new(
        Arc::new(InMemoryGtsRepository::new(config.to_gts_config())),
        config,
    ));
    let _router =
        types_registry::api::rest::routes::register_routes(Router::new(), &openapi, legacy, None);

    let declared = openapi.operations.lock().expect("operations lock").clone();

    let mut ids: Vec<&str> = declared.iter().map(|(_, _, id)| id.as_str()).collect();
    ids.sort_unstable();
    let total = ids.len();
    ids.dedup();
    assert_eq!(
        ids.len(),
        total,
        "duplicate operation id among {declared:?}"
    );

    let mut actual: Vec<(&str, &str, &str)> = declared
        .iter()
        .map(|(m, p, id)| (m.as_str(), p.as_str(), id.as_str()))
        .collect();
    actual.sort_unstable();

    let mut expected = vec![
        (
            "POST",
            "/types-registry/v1/entities",
            "types_registry.register",
        ),
        ("GET", "/types-registry/v1/entities", "types_registry.list"),
        (
            "GET",
            "/types-registry/v1/entities/{gts_id}",
            "types_registry.get",
        ),
        (
            "POST",
            "/types-registry/v2/entities",
            "types_registry.submit_entities",
        ),
        (
            "GET",
            "/types-registry/v2/operations/{operation_id}",
            "types_registry.get_operation",
        ),
        (
            "GET",
            "/types-registry/v2/entities/{entity_key}",
            "types_registry.get_entity",
        ),
    ];
    expected.sort_unstable();

    assert_eq!(actual, expected);
}

/// Ceiling C8's fail-closed fallback is executable: until a platform listener
/// authenticates and authorizes mutations, neither registration spelling may
/// be published through api-gateway.
#[test]
fn mutation_routes_are_internal_only() {
    let openapi = TestOpenApi::default();
    let config = TypesRegistryConfig::default();
    let legacy = Arc::new(TypesRegistryService::new(
        Arc::new(InMemoryGtsRepository::new(config.to_gts_config())),
        config,
    ));
    let _router =
        types_registry::api::rest::routes::register_routes(Router::new(), &openapi, legacy, None);

    let exposure = openapi.exposure.lock().expect("exposure lock");
    for operation_id in ["types_registry.register", "types_registry.submit_entities"] {
        let exposed = exposure
            .iter()
            .find(|(id, _)| id == operation_id)
            .map(|(_, exposed)| *exposed)
            .expect("mutation operation is registered");
        assert!(
            !exposed,
            "{operation_id} must remain internal-only while ceiling C8 is open",
        );
    }
}

/// `Idempotency-Key` is declared in the generated document, not only enforced.
///
/// The route once asserted the opposite — that `OperationBuilder` could not declare a
/// header. It can; only the `header_param` convenience is missing (upstream #4614).
/// A required header absent from the document is what a generated client omits, so
/// the declaration is pinned here rather than left to prose.
#[test]
fn the_idempotency_key_header_is_declared_as_a_required_parameter() {
    let openapi = TestOpenApi::default();
    let config = TypesRegistryConfig::default();
    let legacy = Arc::new(TypesRegistryService::new(
        Arc::new(InMemoryGtsRepository::new(config.to_gts_config())),
        config,
    ));
    let _router =
        types_registry::api::rest::routes::register_routes(Router::new(), &openapi, legacy, None);

    let params = openapi.params.lock().expect("params lock").clone();
    let submit = params
        .iter()
        .find(|(id, _)| id == "types_registry.submit_entities")
        .map(|(_, p)| p.clone())
        .expect("the submit operation is registered");

    assert!(
        submit.contains(&("Idempotency-Key".to_owned(), "Header".to_owned(), true)),
        "submit must declare a required Idempotency-Key header, got: {submit:?}",
    );
}

#[test]
fn submission_response_headers_are_declared() {
    let openapi = TestOpenApi::default();
    let config = TypesRegistryConfig::default();
    let legacy = Arc::new(TypesRegistryService::new(
        Arc::new(InMemoryGtsRepository::new(config.to_gts_config())),
        config,
    ));
    let _router =
        types_registry::api::rest::routes::register_routes(Router::new(), &openapi, legacy, None);

    let headers = openapi
        .response_headers
        .lock()
        .expect("response headers lock");
    let submit = headers
        .iter()
        .find(|(id, _)| id == "types_registry.submit_entities")
        .map(|(_, headers)| headers)
        .expect("the submit operation is registered");

    for expected in [
        (202, "Location", ResponseHeaderType::String),
        (202, "Retry-After", ResponseHeaderType::Integer),
        (202, "Idempotency-Replayed", ResponseHeaderType::Boolean),
        (200, "Location", ResponseHeaderType::String),
        (200, "Idempotency-Replayed", ResponseHeaderType::Boolean),
    ] {
        assert!(
            submit.iter().any(|actual| {
                actual.0 == expected.0 && actual.1 == expected.1 && actual.2 == expected.2
            }),
            "missing response header {expected:?}: {submit:?}",
        );
    }
    assert!(
        !submit
            .iter()
            .any(|(status, name, _)| *status == 200 && name == "Retry-After"),
        "terminal replay must not advertise Retry-After: {submit:?}",
    );
}

/// `extract::Json<T>` can reject a request before its handler with three statuses
/// that `standard_errors` intentionally does not add. Keep the generated contract
/// aligned with both JSON request extractors.
#[test]
fn json_extractor_error_statuses_are_declared_for_both_post_operations() {
    let openapi = TestOpenApi::default();
    let config = TypesRegistryConfig::default();
    let legacy = Arc::new(TypesRegistryService::new(
        Arc::new(InMemoryGtsRepository::new(config.to_gts_config())),
        config,
    ));
    let _router =
        types_registry::api::rest::routes::register_routes(Router::new(), &openapi, legacy, None);

    let responses = openapi.responses.lock().expect("responses lock");
    for operation_id in ["types_registry.register", "types_registry.submit_entities"] {
        let declared = responses
            .iter()
            .find(|(id, _)| id == operation_id)
            .map(|(_, responses)| responses)
            .expect("JSON operation is registered");
        for status in [413, 415, 422] {
            assert!(
                declared.contains(&(status, "application/problem+json".to_owned())),
                "{operation_id} must declare {status} as a Problem response: {declared:?}",
            );
        }
    }
}
