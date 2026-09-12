//! API-level (router) tests for the provisioning endpoint
//! `POST /bss-ledger/v1/provisioning` (target tenant in the body) and
//! `GET /bss-ledger/v1/accounts` (seller via `$filter=tenant_id eq`, else caller).
//!
//! Drives the router via `tower::ServiceExt::oneshot` against a stub
//! `LedgerClientV1` (no DB) and an in-test `PolicyEnforcer` fake (no
//! PDP). Covers the happy path (200, allow + auth), the unauthenticated path
//! (401 problem+json — the enforcer is still layered so the 401 comes from
//! `require_authenticated`, not a missing-extension 500), a malformed body
//! (400 problem+json carrying the `json_syntax_error` reason), an invalid
//! granularity (400 problem+json — `into_request` fails before the client is
//! reached), a PDP deny (403 problem+json), and a cross-tenant target (403 —
//! the membership guard denies a target outside the caller's subtree).

#![allow(
    clippy::non_ascii_literal,
    clippy::let_underscore_must_use,
    clippy::needless_collect,
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::doc_markdown,
    clippy::panic
)]

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use authz_resolver_sdk::constraints::{Constraint, InPredicate, Predicate};
use authz_resolver_sdk::models::{
    EvaluationRequest, EvaluationResponse, EvaluationResponseContext,
};
use authz_resolver_sdk::{AuthZResolverApi, PolicyEnforcer};
use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use bss_ledger::api::rest::provisioning::{ApiState, router};
use bss_ledger_sdk::api::LedgerClientV1;
use bss_ledger_sdk::posting::{ODataQuery, Page, PostEntry, PostingRef};
use bss_ledger_sdk::{ProvisionOutcome, ProvisionRequest};
use toolkit::api::canonical_prelude::CanonicalError;
use toolkit_gts::gts_id;
use toolkit_odata::PageInfo;
use toolkit_security::PlatformSecurityContext;
use tower::ServiceExt;
use uuid::Uuid;

/// In-test data-access stub: the only method exercised is
/// `provision`, which returns a canned outcome.
#[derive(Default)]
struct StubClient {
    last_list_tenant: Mutex<Option<Uuid>>,
}

#[async_trait::async_trait]
impl LedgerClientV1 for StubClient {
    async fn return_payment(
        &self,
        _ctx: &toolkit_security::SecurityContext,
        _req: bss_ledger_sdk::ReturnPayment,
    ) -> Result<bss_ledger_sdk::PostingRef, CanonicalError> {
        unimplemented!("not exercised by these router tests")
    }

    async fn record_dispute_phase(
        &self,
        _ctx: &toolkit_security::SecurityContext,
        _req: bss_ledger_sdk::RecordDisputePhase,
    ) -> Result<bss_ledger_sdk::DisputeOutcome, CanonicalError> {
        unimplemented!("not exercised by these router tests")
    }

    async fn post_credit_application(
        &self,
        _ctx: &toolkit_security::SecurityContext,
        _req: bss_ledger_sdk::CreditApplication,
    ) -> Result<bss_ledger_sdk::CreditApplicationApplied, CanonicalError> {
        unimplemented!("not exercised by the provisioning router tests")
    }

    async fn post_balanced_entry(
        &self,
        _ctx: &toolkit_security::SecurityContext,
        _entry: PostEntry,
    ) -> Result<PostingRef, CanonicalError> {
        unimplemented!("not exercised by the provisioning router tests")
    }

    async fn read_account_balance(
        &self,
        _ctx: &toolkit_security::SecurityContext,
        _tenant_id: Uuid,
        _account_id: Uuid,
    ) -> Result<Option<i64>, CanonicalError> {
        unimplemented!("not exercised by the provisioning router tests")
    }

    async fn list_accounts(
        &self,
        _ctx: &toolkit_security::SecurityContext,
        tenant_id: Uuid,
        _query: &ODataQuery,
    ) -> Result<Page<bss_ledger_sdk::AccountInfo>, CanonicalError> {
        *self.last_list_tenant.lock().unwrap() = Some(tenant_id);
        Ok(Page {
            items: vec![bss_ledger_sdk::AccountInfo {
                account_id: uuid::uuid!("99999999-9999-9999-9999-999999999999"),
                account_class: bss_ledger_sdk::AccountClass::Ar,
                currency: "USD".to_owned(),
                revenue_stream: None,
                lifecycle_state: "OPEN".to_owned(),
            }],
            page_info: PageInfo {
                next_cursor: None,
                prev_cursor: None,
                limit: 200,
            },
        })
    }

    async fn get_entry(
        &self,
        _ctx: &toolkit_security::SecurityContext,
        _tenant_id: Uuid,
        _entry_id: Uuid,
    ) -> Result<Option<bss_ledger_sdk::EntryView>, CanonicalError> {
        unimplemented!("not exercised by the provisioning router tests")
    }

    async fn list_lines(
        &self,
        _ctx: &toolkit_security::SecurityContext,
        _tenant_id: Uuid,
        _query: &ODataQuery,
    ) -> Result<Page<bss_ledger_sdk::LineView>, CanonicalError> {
        unimplemented!("not exercised by the provisioning router tests")
    }

    async fn list_balances(
        &self,
        _ctx: &toolkit_security::SecurityContext,
        _tenant_id: Uuid,
        _query: &ODataQuery,
    ) -> Result<Page<bss_ledger_sdk::BalanceView>, CanonicalError> {
        unimplemented!("not exercised by the provisioning router tests")
    }

    async fn list_ar_invoice_balances(
        &self,
        _ctx: &toolkit_security::SecurityContext,
        _tenant_id: Uuid,
        _payer_tenant_id: Option<Uuid>,
    ) -> Result<Vec<bss_ledger_sdk::ArInvoiceBalanceView>, CanonicalError> {
        unimplemented!("not exercised by the provisioning router tests")
    }

    async fn provision(
        &self,
        _ctx: &toolkit_security::SecurityContext,
        _req: ProvisionRequest,
    ) -> Result<ProvisionOutcome, CanonicalError> {
        Ok(ProvisionOutcome {
            accounts: vec![bss_ledger_sdk::AccountInfo {
                account_id: uuid::uuid!("99999999-9999-9999-9999-999999999999"),
                account_class: bss_ledger_sdk::AccountClass::Ar,
                currency: "USD".to_owned(),
                revenue_stream: None,
                lifecycle_state: "OPEN".to_owned(),
            }],
            accounts_created: 1,
            accounts_existing: 0,
            scales_created: 0,
            scales_existing: 0,
            calendar_created: true,
            period_id: "202606".to_owned(),
            period_created: true,
        })
    }

    async fn close_period(
        &self,
        _ctx: &toolkit_security::SecurityContext,
        _tenant_id: Uuid,
        _period_id: String,
    ) -> Result<bss_ledger_sdk::CloseOutcome, CanonicalError> {
        unimplemented!("not exercised by the provisioning router tests")
    }

    async fn settle_payment(
        &self,
        _ctx: &toolkit_security::SecurityContext,
        _req: bss_ledger_sdk::SettlePayment,
    ) -> Result<PostingRef, CanonicalError> {
        unimplemented!("not exercised by the provisioning router tests")
    }

    async fn allocate_payment(
        &self,
        _ctx: &toolkit_security::SecurityContext,
        _req: bss_ledger_sdk::AllocatePayment,
    ) -> Result<bss_ledger_sdk::AllocateOutcome, CanonicalError> {
        unimplemented!("not exercised by the provisioning router tests")
    }

    async fn list_payment_allocations(
        &self,
        _ctx: &toolkit_security::SecurityContext,
        _tenant_id: Uuid,
        _payment_id: String,
    ) -> Result<Vec<bss_ledger_sdk::AllocationView>, CanonicalError> {
        unimplemented!("not exercised by the provisioning router tests")
    }

    async fn read_unallocated(
        &self,
        _ctx: &toolkit_security::SecurityContext,
        _tenant_id: Uuid,
        _payer_tenant_id: Uuid,
        _currency: String,
    ) -> Result<bss_ledger_sdk::UnallocatedView, CanonicalError> {
        unimplemented!("not exercised by the provisioning router tests")
    }

    async fn trigger_recognition_run(
        &self,
        _ctx: &toolkit_security::SecurityContext,
        _req: bss_ledger_sdk::TriggerRecognitionRun,
    ) -> Result<bss_ledger_sdk::RecognitionRunOutcome, CanonicalError> {
        unimplemented!("not exercised by the provisioning router tests")
    }

    async fn list_revenue_disaggregation(
        &self,
        _ctx: &toolkit_security::SecurityContext,
        _query: bss_ledger_sdk::RevenueDisaggregationQuery,
    ) -> Result<bss_ledger_sdk::RevenueDisaggregation, CanonicalError> {
        unimplemented!("not exercised by the provisioning router tests")
    }

    async fn change_recognition_schedule(
        &self,
        _ctx: &toolkit_security::SecurityContext,
        _cmd: bss_ledger_sdk::ChangeRecognitionSchedule,
    ) -> Result<bss_ledger_sdk::ScheduleChangeRef, CanonicalError> {
        unimplemented!("not exercised by the provisioning router tests")
    }

    async fn get_recognition_schedule(
        &self,
        _ctx: &toolkit_security::SecurityContext,
        _tenant_id: Uuid,
        _schedule_id: String,
    ) -> Result<Option<bss_ledger_sdk::RecognitionScheduleView>, CanonicalError> {
        unimplemented!("not exercised by the provisioning router tests")
    }

    async fn list_recognition_schedules(
        &self,
        _ctx: &toolkit_security::SecurityContext,
        _tenant_id: Uuid,
        _invoice_id: Option<String>,
        _revenue_stream: Option<String>,
    ) -> Result<bss_ledger_sdk::RecognitionScheduleList, CanonicalError> {
        unimplemented!("not exercised by the provisioning router tests")
    }
}

/// Subject tenant from the request (`subject.properties["tenant_id"]`), nil when
/// absent. Mirrors the rms fake's helper so the allow constraint echoes the
/// caller's own tenant, as a real PDP would for an own-tenant grant.
fn subject_tenant_id(request: &EvaluationRequest) -> Uuid {
    request
        .subject
        .properties
        .get("tenant_id")
        .and_then(|v| v.as_str())
        .and_then(|s| Uuid::parse_str(s).ok())
        .unwrap_or_else(Uuid::nil)
}

/// `owner_tenant_id IN [tenant_id]` — the allow fake's standard tenant-scoping
/// constraint. Non-empty so the `require_constraints = true` PEP path compiles a
/// scope rather than fail-closing.
fn tenant_in_constraint(tenant_id: Uuid) -> Constraint {
    Constraint {
        predicates: vec![Predicate::In(InPredicate::new(
            toolkit_security::pep_properties::OWNER_TENANT_ID,
            [tenant_id],
        ))],
    }
}

/// Always-allow fake that echoes the subject's tenant as an `owner_tenant_id`
/// `In` constraint — the in-process router has no PDP, so this stands in for one
/// granting the caller's own tenant.
struct AllowAuthZ;

#[async_trait]
impl AuthZResolverApi for AllowAuthZ {
    async fn evaluate(
        &self,
        _ctx: PlatformSecurityContext,
        request: EvaluationRequest,
    ) -> Result<EvaluationResponse, CanonicalError> {
        let tenant_id = subject_tenant_id(&request);
        Ok(EvaluationResponse {
            decision: true,
            context: EvaluationResponseContext {
                constraints: vec![tenant_in_constraint(tenant_id)],
                deny_reason: None,
            },
        })
    }
}

/// Always-deny fake (`decision: false`) — models the PDP refusing the action so
/// the gate maps it to 403.
struct DenyAuthZ;

#[async_trait]
impl AuthZResolverApi for DenyAuthZ {
    async fn evaluate(
        &self,
        _ctx: PlatformSecurityContext,
        _request: EvaluationRequest,
    ) -> Result<EvaluationResponse, CanonicalError> {
        Ok(EvaluationResponse {
            decision: false,
            context: EvaluationResponseContext {
                constraints: vec![],
                deny_reason: None,
            },
        })
    }
}

/// A permissive `PolicyEnforcer` for the router tests (PDP allows + echoes the
/// caller's tenant scope).
fn allow_enforcer() -> PolicyEnforcer {
    PolicyEnforcer::new(Arc::new(AllowAuthZ))
}

/// A `PolicyEnforcer` whose PDP denies every request — proves the gate maps a
/// deny to 403.
fn deny_enforcer() -> PolicyEnforcer {
    PolicyEnforcer::new(Arc::new(DenyAuthZ))
}

/// Build the provisioning router with the stub client and the supplied
/// enforcer. The enforcer is layered as an `Extension` (the `provision` handler
/// extracts `Extension<PolicyEnforcer>`); without it the handler 500s on a
/// missing extension before the auth/authz checks run.
fn router_with_enforcer(enforcer: PolicyEnforcer) -> Router {
    let state = Arc::new(ApiState {
        client: Arc::new(StubClient::default()) as Arc<dyn LedgerClientV1>,
    });
    let openapi = toolkit::api::OpenApiRegistryImpl::new();
    router(state, &openapi).layer(axum::Extension(enforcer))
}

/// Build the provisioning router with the stub client and the always-allow
/// enforcer (the default for the auth/body tests).
fn base_router() -> Router {
    router_with_enforcer(allow_enforcer())
}

/// The authenticated caller's tenant — also the only tenant the allow fake
/// authorizes (its `In` constraint echoes it), so provisioning THIS tenant is
/// in-scope while any other target is a cross-tenant write.
const SUBJECT_TENANT: Uuid = uuid::uuid!("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee");

/// An authenticated `SecurityContext` (non-nil subject + tenant + a positive
/// `subject_type`) so `require_authenticated` passes. Mirrors the rbac
/// router-test fixture (`common::test_security_context`).
fn authed_context() -> toolkit_security::SecurityContext {
    toolkit_security::SecurityContext::builder()
        .subject_id(uuid::uuid!("11111111-2222-3333-4444-555555555555"))
        .subject_tenant_id(SUBJECT_TENANT)
        .subject_type(gts_id!("cf.core.security.subject_user.v1~"))
        .token_scopes(vec!["*".to_owned()])
        .build()
        .expect("authed SecurityContext must build")
}

/// A valid snake_case provisioning request body (target tenant = `SUBJECT_TENANT`).
fn valid_body() -> serde_json::Value {
    serde_json::json!({
        "tenant_id": SUBJECT_TENANT,
        "accounts": [
            {
                "account_class": "AR",
                "currency": "USD",
                "normal_side": "DR"
            }
        ],
        "currency_scales": [
            { "currency": "XBT", "minor_units": 8, "source": "TENANT" }
        ],
        "fiscal_calendar": {
            "timezone": "UTC",
            "granularity": "MONTH",
            "fy_start": 1
        }
    })
}

fn provisioning_uri() -> String {
    "/bss-ledger/v1/provisioning".to_owned()
}

#[tokio::test]
async fn provision_happy_path_returns_200() {
    let router = base_router().layer(axum::Extension(authed_context()));
    let response = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(provisioning_uri())
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(valid_body().to_string()))
                .expect("build req"),
        )
        .await
        .expect("send");

    assert_eq!(response.status(), StatusCode::OK);
    let bytes = to_bytes(response.into_body(), 1_000_000).await.unwrap();
    let value: serde_json::Value = serde_json::from_slice(&bytes).expect("body must be JSON");
    assert_eq!(value["accounts_created"], serde_json::json!(1));
    assert_eq!(
        value["accounts"][0]["account_id"],
        serde_json::json!("99999999-9999-9999-9999-999999999999")
    );
}

#[tokio::test]
async fn list_accounts_happy_path_returns_200() {
    let router = base_router().layer(axum::Extension(authed_context()));
    let response = router
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/bss-ledger/v1/accounts")
                .body(Body::empty())
                .expect("build req"),
        )
        .await
        .expect("send");

    assert_eq!(response.status(), StatusCode::OK);
    let bytes = to_bytes(response.into_body(), 1_000_000).await.unwrap();
    let value: serde_json::Value = serde_json::from_slice(&bytes).expect("body must be JSON");
    // Canonical OData `Page` envelope: accounts under `items`, cursor metadata
    // under `page_info` (no more bespoke `{ accounts: [...] }`).
    assert_eq!(
        value["items"][0]["account_id"],
        serde_json::json!("99999999-9999-9999-9999-999999999999")
    );
    assert_eq!(
        value["items"][0]["lifecycle_state"],
        serde_json::json!("OPEN")
    );
    assert!(
        value["page_info"].is_object(),
        "the Page envelope must carry page_info, got {value}"
    );
}

#[tokio::test]
async fn list_accounts_named_tenant_id_returns_400() {
    let router = base_router().layer(axum::Extension(authed_context()));
    let response = router
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!(
                    "/bss-ledger/v1/accounts?tenant_id={SUBJECT_TENANT}"
                ))
                .body(Body::empty())
                .expect("build req"),
        )
        .await
        .expect("send");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let bytes = to_bytes(response.into_body(), 1_000_000).await.unwrap();
    let value: serde_json::Value = serde_json::from_slice(&bytes).expect("body must be JSON");
    let text = value.to_string();
    assert!(text.contains("tenant_id"), "detail names the key: {value}");
    assert!(text.contains("$filter"), "detail hints $filter: {value}");
}

#[tokio::test]
async fn list_accounts_plain_status_returns_400() {
    let router = base_router().layer(axum::Extension(authed_context()));
    let response = router
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/bss-ledger/v1/accounts?status=OPEN")
                .body(Body::empty())
                .expect("build req"),
        )
        .await
        .expect("send");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let bytes = to_bytes(response.into_body(), 1_000_000).await.unwrap();
    let value: serde_json::Value = serde_json::from_slice(&bytes).expect("body must be JSON");
    assert!(
        value.to_string().contains("status"),
        "detail names the key: {value}"
    );
}

#[tokio::test]
async fn list_accounts_filter_tenant_id_eq_returns_200() {
    let stub = Arc::new(StubClient::default());
    let state = Arc::new(ApiState {
        client: stub.clone() as Arc<dyn LedgerClientV1>,
    });
    let openapi = toolkit::api::OpenApiRegistryImpl::new();
    let router = router(state, &openapi)
        .layer(axum::Extension(allow_enforcer()))
        .layer(axum::Extension(authed_context()));
    let response = router
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!(
                    "/bss-ledger/v1/accounts?$filter=tenant_id%20eq%20{SUBJECT_TENANT}"
                ))
                .body(Body::empty())
                .expect("build req"),
        )
        .await
        .expect("send");

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        *stub.last_list_tenant.lock().unwrap(),
        Some(SUBJECT_TENANT),
        "$filter=tenant_id eq must reach the client as that seller"
    );
}

#[tokio::test]
async fn list_accounts_openapi_declares_orderby_not_tenant_id() {
    let state = Arc::new(ApiState {
        client: Arc::new(StubClient::default()) as Arc<dyn LedgerClientV1>,
    });
    let openapi = toolkit::api::OpenApiRegistryImpl::new();
    let _router = router(state, &openapi);
    let spec = openapi
        .build_openapi(&toolkit::api::OpenApiInfo::default())
        .expect("openapi must build");
    let json = serde_json::to_value(spec).expect("serialize spec");
    let params = json["paths"]["/bss-ledger/v1/accounts"]["get"]["parameters"]
        .as_array()
        .expect("accounts GET must declare parameters");
    let names: Vec<&str> = params.iter().filter_map(|p| p["name"].as_str()).collect();
    assert!(
        names.contains(&"$orderby"),
        "$orderby must be advertised: {names:?}"
    );
    assert!(
        names.contains(&"$filter"),
        "$filter must be advertised: {names:?}"
    );
    assert!(
        !names.contains(&"tenant_id"),
        "named tenant_id must leave the list: {names:?}"
    );
}

#[tokio::test]
async fn provision_cross_tenant_target_returns_403() {
    // The allow fake authorizes only the caller's own tenant (its `In`
    // constraint echoes SUBJECT_TENANT). Targeting a DIFFERENT tenant — outside
    // the caller's authorized subtree — must be denied at the PEP boundary even
    // though the PDP itself allows: the cross-tenant-write guard (BOLA).
    let other_tenant = uuid::uuid!("dddddddd-eeee-ffff-0000-111111111111");
    // Target tenant is now the body's `tenant_id` (not the path).
    let mut body = valid_body();
    body["tenant_id"] = serde_json::json!(other_tenant);
    let router = base_router().layer(axum::Extension(authed_context()));
    let response = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(provisioning_uri())
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_string()))
                .expect("build req"),
        )
        .await
        .expect("send");

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn provision_without_auth_returns_401() {
    // No Extension(ctx) layer => require_authenticated fails with 401. The
    // enforcer IS layered so the 401 comes from require_authenticated, not a
    // missing-extension 500.
    let router = base_router();
    let response = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(provisioning_uri())
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(valid_body().to_string()))
                .expect("build req"),
        )
        .await
        .expect("send");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let ct = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    assert!(
        ct.contains("application/problem+json"),
        "expected problem+json, got '{ct}'"
    );
}

#[tokio::test]
async fn provision_malformed_body_returns_400() {
    let router = base_router().layer(axum::Extension(authed_context()));
    let response = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(provisioning_uri())
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from("{"))
                .expect("build req"),
        )
        .await
        .expect("send");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let ct = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    assert!(
        ct.contains("application/problem+json"),
        "expected problem+json, got '{ct}'"
    );
    let bytes = to_bytes(response.into_body(), 1_000_000).await.unwrap();
    let value: serde_json::Value = serde_json::from_slice(&bytes).expect("body must be JSON");
    assert!(
        value.to_string().contains("json_syntax_error"),
        "expected the json_syntax_error reason code in the body; got {value}"
    );
}

#[tokio::test]
async fn provision_invalid_granularity_returns_400() {
    let mut body = valid_body();
    body["fiscal_calendar"]["granularity"] = serde_json::json!("WEEK");

    let router = base_router().layer(axum::Extension(authed_context()));
    let response = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(provisioning_uri())
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_string()))
                .expect("build req"),
        )
        .await
        .expect("send");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let ct = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    assert!(
        ct.contains("application/problem+json"),
        "expected problem+json, got '{ct}'"
    );
}

#[tokio::test]
async fn provision_denied_returns_403() {
    // Authenticated caller, valid body, but the PDP denies (admin, provision):
    // the billing-setup gate maps the deny to 403 problem+json before the
    // client is reached.
    let router = router_with_enforcer(deny_enforcer()).layer(axum::Extension(authed_context()));
    let response = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(provisioning_uri())
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(valid_body().to_string()))
                .expect("build req"),
        )
        .await
        .expect("send");

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let ct = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    assert!(
        ct.contains("application/problem+json"),
        "expected problem+json, got '{ct}'"
    );
}
