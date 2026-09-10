#![allow(clippy::expect_used)]

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use axum::{Extension, Router};
use serde_json::{Value, json};
use toolkit::api::openapi_registry::OpenApiRegistryImpl;
use tower::ServiceExt as _;
use uuid::Uuid;

use super::{PATH_PREFIX, register_routes};
use crate::domain::Service;
use crate::test_support::{
    LLM_USER_PROJECTION, METRIC_TOKENS, PermitTenantsPdp, bound_service, ctx, tenant,
    unbound_service,
};

fn permitting() -> Arc<PermitTenantsPdp> {
    Arc::new(PermitTenantsPdp::new(vec![tenant().as_uuid()]))
}

fn app(service: Arc<Service>) -> Router {
    register_routes(Router::new(), &OpenApiRegistryImpl::new(), service).layer(Extension(ctx()))
}

async fn send(
    app: &Router,
    method: Method,
    path: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut request = Request::builder()
        .method(method)
        .uri(format!("{PATH_PREFIX}{path}"));
    let body = match body {
        Some(json) => {
            request = request.header(header::CONTENT_TYPE, "application/json");
            Body::from(json.to_string())
        }
        None => Body::empty(),
    };
    let response = app
        .clone()
        .oneshot(request.body(body).expect("request"))
        .await
        .expect("response");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()))
    };
    (status, value)
}

fn create_body() -> Value {
    json!({
        "tenant_id": tenant().as_uuid(),
        "subject": { "projection_type": LLM_USER_PROJECTION, "subject_id": "u1" },
        "metric": METRIC_TOKENS,
        "quota_type": "gts.cf.qe.quota.type.v1~cf.qe.quota.consumption.v1",
        "period": "gts.cf.qe.period.type.v1~cf.qe.period.month.v1",
        "enforcement_mode": "gts.cf.qe.enforcement.type.v1~cf.qe.enforcement.hard.v1",
        "cap": 100,
        "notification_thresholds": [50],
        "metadata": { "regions": ["eu"], "weight": 5 },
        "source": "gts.cf.qe.source.type.v1~cf.qe.source.operator.v1"
    })
}

async fn created(app: &Router) -> Uuid {
    let (status, body) = send(app, Method::POST, "/quotas", Some(create_body())).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    body["id"].as_str().expect("id").parse().expect("uuid")
}

#[tokio::test]
async fn create_answers_201_with_the_view_and_a_location_header() {
    let app = app(bound_service(permitting()).await);
    let request = Request::builder()
        .method(Method::POST)
        .uri(format!("{PATH_PREFIX}/quotas"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(create_body().to_string()))
        .expect("request");
    let response = app.clone().oneshot(request).await.expect("response");
    assert_eq!(response.status(), StatusCode::CREATED);
    let location = response
        .headers()
        .get(header::LOCATION)
        .expect("location")
        .to_str()
        .expect("ascii")
        .to_owned();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    let body: Value = serde_json::from_slice(&bytes).expect("json");
    assert_eq!(
        location,
        format!("{PATH_PREFIX}/quotas/{}", body["id"].as_str().expect("id"))
    );
    assert_eq!(body["status"], json!("active"));
    assert_eq!(body["record_version"], json!(1));
    assert_eq!(body["currently_within_window"], json!(true));
    assert_eq!(body["metric_kind"], json!("counter"));
    assert_eq!(body["metadata"]["weight"], json!(5));
    assert_eq!(
        body["constraint_contract"]["type_id"],
        json!("gts.cf.core.qe.constraint.v1~cf.genai.llm_gateway.token_constraint.v1~")
    );
}

#[tokio::test]
async fn shape_rejections_carry_their_tokens_and_statuses() {
    let app = app(bound_service(permitting()).await);
    let cases: Vec<(Value, StatusCode, &str)> = vec![
        (
            {
                let mut b = create_body();
                b["quota_type"] = json!("gts.cf.qe.quota.type.v1~cf.qe.quota.rate.v1");
                b.as_object_mut().expect("object").remove("period");
                b
            },
            StatusCode::NOT_IMPLEMENTED,
            "NOT_YET_IMPLEMENTED",
        ),
        (
            {
                let mut b = create_body();
                b["quota_type"] = json!("gts.cf.qe.quota.type.v1~cf.qe.quota.allocation.v1");
                b["period"] = Value::Null;
                b
            },
            StatusCode::BAD_REQUEST,
            "PERIOD_NOT_ALLOWED",
        ),
        (
            {
                let mut b = create_body();
                b.as_object_mut().expect("object").remove("period");
                b
            },
            StatusCode::BAD_REQUEST,
            "PERIOD_REQUIRED",
        ),
        (
            {
                let mut b = create_body();
                b["cap"] = json!(-1);
                b
            },
            StatusCode::BAD_REQUEST,
            "CAP_MUST_BE_NON_NEGATIVE",
        ),
        (
            {
                let mut b = create_body();
                b["metadata"] = json!({ "weight": "heavy" });
                b
            },
            StatusCode::BAD_REQUEST,
            "CONSTRAINT_CONTRACT_MISMATCH",
        ),
        (
            {
                let mut b = create_body();
                b["bogus"] = json!(1);
                b
            },
            // The toolkit's JSON extractor rejects a body it cannot decode
            // with 422, the platform convention `standard_errors` declares.
            StatusCode::UNPROCESSABLE_ENTITY,
            "unknown field `bogus`",
        ),
    ];
    for (body, expected, token) in cases {
        let (status, answer) = send(&app, Method::POST, "/quotas", Some(body.clone())).await;
        assert_eq!(status, expected, "{body} -> {answer}");
        assert!(answer.to_string().contains(token), "{token}: {answer}");
    }
}

#[tokio::test]
async fn update_gates_rate_before_the_immutable_fields_and_applies_a_patch() {
    let app = app(bound_service(permitting()).await);
    let id = created(&app).await;
    let (status, body) = send(
        &app,
        Method::PATCH,
        &format!("/quotas/{id}"),
        Some(
            json!({ "quota_type": "gts.cf.qe.quota.type.v1~cf.qe.quota.rate.v1", "metric": null }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_IMPLEMENTED, "{body}");
    assert!(
        body["detail"]
            .as_str()
            .expect("detail")
            .starts_with("NOT_YET_IMPLEMENTED")
    );

    let (status, body) = send(
        &app,
        Method::PATCH,
        &format!("/quotas/{id}"),
        Some(json!({ "metric": null })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.to_string().contains("IMMUTABLE_FIELD"), "{body}");

    let (status, body) = send(
        &app,
        Method::PATCH,
        &format!("/quotas/{id}"),
        Some(json!({ "bogus": 1 })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "unknown keys fail at the extractor: {body}"
    );

    let (status, body) = send(
        &app,
        Method::PATCH,
        &format!("/quotas/{id}"),
        Some(json!({ "cap": 250, "fail_open_hint": true })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["cap"], json!(250));
    assert_eq!(body["fail_open_hint"], json!(true));
    assert_eq!(body["record_version"], json!(2));
}

#[tokio::test]
async fn reads_list_with_repeated_ids_and_reject_bad_parameters() {
    let app = app(bound_service(permitting()).await);
    let first = created(&app).await;
    let second = created(&app).await;
    let (status, body) = send(&app, Method::GET, &format!("/quotas/{first}"), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["id"], json!(first.to_string()));

    let (status, body) = send(
        &app,
        Method::GET,
        &format!("/quotas?id={first}&id={second}&limit=1"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["items"].as_array().expect("items").len(), 1);
    assert!(body["next_cursor"].is_string(), "more rows follow: {body}");

    let (status, body) = send(&app, Method::GET, "/quotas", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["items"].as_array().expect("items").len(), 2);
    assert!(body["next_cursor"].is_null());

    for (query, token) in [
        (
            format!("?projection_type={LLM_USER_PROJECTION}"),
            "LIST_SUBJECT_INCOMPLETE",
        ),
        ("?limit=0".to_owned(), "LIST_LIMIT_OUT_OF_RANGE"),
        ("?status=paused".to_owned(), "STATUS_INVALID"),
        ("?id=nope".to_owned(), "QUOTA_ID_INVALID"),
        // Cursors are opaque to the gear; storage refuses one it did not issue
        // and the refusal is caller input, never a 500.
        ("?cursor=not-a-cursor".to_owned(), "CURSOR_INVALID"),
    ] {
        let (status, body) = send(&app, Method::GET, &format!("/quotas{query}"), None).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{query}: {body}");
        assert!(body.to_string().contains(token), "{query}: {body}");
    }

    let (status, body) = send(
        &app,
        Method::GET,
        &format!("/quotas/{}", Uuid::from_u128(404)),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert!(body.to_string().contains("cf.qe.resource.quota"), "{body}");
}

#[tokio::test]
async fn deactivate_answers_200_with_the_outcome_and_is_terminal() {
    let app = app(bound_service(permitting()).await);
    let id = created(&app).await;
    let (status, body) = send(
        &app,
        Method::POST,
        &format!("/quotas/{id}/deactivate"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!({ "resolved_leases": [] }));
    let (status, body) = send(
        &app,
        Method::POST,
        &format!("/quotas/{id}/deactivate"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.to_string().contains("QUOTA_DEACTIVATED"), "{body}");
    let (status, body) = send(&app, Method::GET, &format!("/quotas/{id}"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], json!("deactivated"));
}

#[tokio::test]
async fn an_unbound_service_answers_503_not_ready() {
    let app = app(unbound_service(permitting()));
    let (status, body) = send(&app, Method::POST, "/quotas", Some(create_body())).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert!(body.to_string().contains("NOT_READY"), "{body}");
}

#[tokio::test]
async fn the_five_operations_are_registered_in_the_openapi_document() {
    let registry = OpenApiRegistryImpl::new();
    let _router = register_routes(Router::new(), &registry, unbound_service(permitting()));
    let spec = registry
        .build_openapi(&toolkit::api::OpenApiInfo::default())
        .expect("spec");
    let rendered = serde_json::to_string(&spec).expect("spec json");
    for operation in [
        "quota_enforcement.create_quota",
        "quota_enforcement.get_quota",
        "quota_enforcement.list_quotas",
        "quota_enforcement.update_quota",
        "quota_enforcement.deactivate_quota",
    ] {
        assert!(rendered.contains(operation), "{operation} missing");
    }
}
