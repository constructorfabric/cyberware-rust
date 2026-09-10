//! API-level (router) tests for the adjustment REST surface (Slice 3, Group E +
//! Group-6 / Phase-3 manual adjustments): `POST /bss-ledger/v1/credit-notes`,
//! `POST /bss-ledger/v1/debit-notes`, `POST /bss-ledger/v1/manual-adjustments`, and
//! `GET /bss-ledger/v1/invoices/{invoice_id}/exposure` (tenant in body for the
//! writes, in the query for the read).
//!
//! Like `rest_credit.rs` / `rest_payments.rs`, these drive the router against a
//! REAL testcontainer Postgres so a credit/debit note runs end-to-end through the
//! foundation engine (`PostingService` + the in-txn sidecars) and the headroom
//! CHECK. The adjustment handlers are CONCRETE orchestrators (not behind
//! `LedgerClientV1`), so the router's `ApiState` is built directly over real
//! `CreditNoteHandler` / `DebitNoteHandler` / `AdjustmentRepo` (no in-test client
//! wrapper). An always-allow `PolicyEnforcer` fake (echoing the subject tenant as
//! an `owner_tenant_id` `In` constraint) is layered as an `Extension`, mirroring
//! `register_rest`; a deny fake covers the 403.
//!
//! Cases:
//! - a deferred credit note → 201, an idempotent re-post (same `credit_note_id`)
//!   → 200 (`replayed = true`);
//! - an over-headroom credit note → 400 `CREDIT_NOTE_EXCEEDS_HEADROOM`;
//! - a credit note requesting a deferred part against a point-in-time invoice
//!   (no obligation to reduce) → 400 `CREDIT_NOTE_SPLIT_AMBIGUOUS`;
//! - a deferred debit note → 201, and it RAISES the invoice's headroom (a credit
//!   note that was over-cap before now fits → 201);
//! - `GET …/exposure` → 200 with the headroom counters + remaining AR;
//! - a credit note whose body `tenant_id` is outside the caller's scope → 403;
//! - a governed manual adjustment (DR SUSPENSE / CR CASH_CLEARING) → 201, an
//!   idempotent re-post (same `adjustment_id`) → 200 (`replayed = true`);
//! - a manual adjustment with a leg outside the action's allow-list (TAX_PAYABLE)
//!   → 400 `MANUAL_ADJUSTMENT_NOT_ALLOWED`;
//! - a manual adjustment shaped as a disguised write-off (DR CONTRA_REVENUE / CR AR)
//!   → 400 `MANUAL_ADJUSTMENT_NOT_ALLOWED`.
//!
//! The over-D2 dual-control gate (→ 409) + the manual-adjustment foreign-tenant 403
//! are not re-exercised here: the gate path needs the `ApprovalService` wired (it is
//! covered by the Group-5 service tests) and the cross-tenant write-gate is the SAME
//! `access_scope` path the credit-note 403 already covers.
//!
//! Ignored by default (needs Docker); run with `-- --ignored`.

#![allow(
    clippy::non_ascii_literal,
    clippy::let_underscore_must_use,
    clippy::needless_collect,
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::doc_markdown,
    clippy::panic,
    clippy::too_many_lines,
    clippy::needless_pass_by_value
)]

use std::sync::Arc;

use async_trait::async_trait;
use authz_resolver_sdk::constraints::{Constraint, InPredicate, Predicate};
use authz_resolver_sdk::models::{
    EvaluationRequest, EvaluationResponse, EvaluationResponseContext,
};
use authz_resolver_sdk::{AuthZResolverApi, PolicyEnforcer};
use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use bss_ledger::api::rest::adjustments::{ApiState, router};
use bss_ledger::config::{FxConfig, RecognitionConfig};
use bss_ledger::domain::invoice::builder::{InvoiceItem, PostedInvoice, TaxBreakdown};
use bss_ledger::domain::model::{AccountRow, CurrencyScaleRow};
use bss_ledger::domain::money::DEFAULT_PLAUSIBLE_MAX_MAJOR;
use bss_ledger::domain::recognition::input::{RecognitionInput, RecognitionTiming};
use bss_ledger::infra::adjustment::credit_note_service::CreditNoteHandler;
use bss_ledger::infra::adjustment::debit_note_service::DebitNoteHandler;
use bss_ledger::infra::adjustment::manual_adjustment_service::ManualAdjustmentHandler;
use bss_ledger::infra::events::publisher::LedgerEventPublisher;
use bss_ledger::infra::invoice_post::InvoicePostService;
use bss_ledger::infra::metrics::test_harness::MetricsHarness;
use bss_ledger::infra::storage::migrations::Migrator;
use bss_ledger::infra::storage::repo::{AdjustmentRepo, ReferenceRepo};
use bss_ledger_sdk::{AccountClass, Side};
use chrono::NaiveDate;
use sea_orm::{ConnectionTrait, Database, Statement};
use sea_orm_migration::MigratorTrait;
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use time::OffsetDateTime;
use toolkit::api::canonical_prelude::CanonicalError;
use toolkit_db::secure::AccessScope;
use toolkit_db::{ConnectOpts, DBProvider, DbError, connect_db};
use toolkit_gts::gts_id;
use toolkit_security::{PlatformSecurityContext, SecurityContext};
use tower::ServiceExt;
use uuid::Uuid;

fn pg(sql: impl Into<String>) -> Statement {
    Statement::from_string(sea_orm::DatabaseBackend::Postgres, sql.into())
}

fn naive(y: i32, m: u32, d: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, m, d).unwrap()
}

// ── PDP fakes (mirror rest_credit.rs / rest_recognition.rs) ──────────────────

/// The authenticated caller's tenant — also the provisioned seller and the only
/// tenant the allow fake authorizes. The body `tenant_id` MUST equal this so the
/// handler write-gate's `contains_uuid` membership check passes.
const SUBJECT_TENANT: Uuid = uuid::uuid!("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee");
const SUBJECT_ID: Uuid = uuid::uuid!("11111111-2222-3333-4444-555555555555");
/// A tenant the caller is NOT authorized for (the 403 target).
const FOREIGN_TENANT: Uuid = uuid::uuid!("ffffffff-ffff-ffff-ffff-ffffffffffff");

/// Always-allow fake emitting a flat `In([SUBJECT_TENANT])` over `owner_tenant_id`
/// — the degraded-mode shape this gear's PEP compiles. A `tenant_id` equal to
/// `SUBJECT_TENANT` passes the gate's write-membership assert; a foreign one fails
/// it (the gate denies a cross-tenant write).
struct AllowInResolver;

#[async_trait]
impl AuthZResolverApi for AllowInResolver {
    async fn evaluate(
        &self,
        _ctx: PlatformSecurityContext,
        _req: EvaluationRequest,
    ) -> Result<EvaluationResponse, CanonicalError> {
        Ok(EvaluationResponse {
            decision: true,
            context: EvaluationResponseContext {
                constraints: vec![Constraint {
                    predicates: vec![Predicate::In(InPredicate::new(
                        toolkit_security::pep_properties::OWNER_TENANT_ID,
                        vec![SUBJECT_TENANT],
                    ))],
                }],
                deny_reason: None,
            },
        })
    }
}

/// Always-deny fake — the PDP refuses, so the gate maps it to 403.
struct DenyResolver;

#[async_trait]
impl AuthZResolverApi for DenyResolver {
    async fn evaluate(
        &self,
        _ctx: PlatformSecurityContext,
        _req: EvaluationRequest,
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

fn allow_enforcer() -> PolicyEnforcer {
    PolicyEnforcer::new(Arc::new(AllowInResolver))
}

fn deny_enforcer() -> PolicyEnforcer {
    PolicyEnforcer::new(Arc::new(DenyResolver))
}

// ── DB + seller setup ────────────────────────────────────────────────────────

/// Provisioned seller ids (the full Slice-3 chart: AR / REVENUE(sub) /
/// CONTRACT_LIABILITY(sub) / CONTRA_REVENUE / GOODWILL / REUSABLE_CREDIT / TAX /
/// SUSPENSE / CASH_CLEARING). `tenant` is fixed to `SUBJECT_TENANT` so the handler
/// authz gate passes. `period_id` is the CURRENT month (the notes / adjustments
/// post into it).
struct Seller {
    tenant: Uuid,
    payer: Uuid,
    ar: Uuid,
    revenue: Uuid,
    contract_liability: Uuid,
    contra_revenue: Uuid,
    goodwill: Uuid,
    reusable_credit: Uuid,
    tax: Uuid,
    suspense: Uuid,
    cash_clearing: Uuid,
    period_id: String,
}

fn account(
    tenant: Uuid,
    id: Uuid,
    class: AccountClass,
    normal: Side,
    stream: Option<&str>,
) -> AccountRow {
    AccountRow {
        account_id: id,
        tenant_id: tenant,
        legal_entity_id: tenant,
        account_class: class.as_str().to_owned(),
        currency: "USD".to_owned(),
        revenue_stream: stream.map(str::to_owned),
        normal_side: normal.as_str().to_owned(),
        may_go_negative: false,
        lifecycle_state: "OPEN".to_owned(),
    }
}

/// Boot a container, migrate, seed USD@2 + a CURRENT-month OPEN period + the full
/// chart, and return a `bss`-search-path `DBProvider`. Mirrors
/// `postgres_credit_note.rs::setup` but with the current-month period (the notes
/// derive their post period from `Utc::now`).
async fn boot() -> (
    testcontainers_modules::testcontainers::ContainerAsync<Postgres>,
    sea_orm::DatabaseConnection,
    DBProvider<DbError>,
    Seller,
) {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let raw = Database::connect(&url).await.unwrap();
    Migrator::up(&raw, None).await.unwrap();
    let repo_url = format!("{url}?options=-c%20search_path%3Dbss,public");
    let tdb = connect_db(&repo_url, ConnectOpts::default()).await.unwrap();
    let provider = DBProvider::<DbError>::new(tdb);

    let now = OffsetDateTime::now_utc();
    let s = Seller {
        tenant: SUBJECT_TENANT,
        payer: Uuid::now_v7(),
        ar: Uuid::now_v7(),
        revenue: Uuid::now_v7(),
        contract_liability: Uuid::now_v7(),
        contra_revenue: Uuid::now_v7(),
        goodwill: Uuid::now_v7(),
        reusable_credit: Uuid::now_v7(),
        tax: Uuid::now_v7(),
        suspense: Uuid::now_v7(),
        cash_clearing: Uuid::now_v7(),
        period_id: bss_ledger::domain::instant::yyyymm(now),
    };

    let reference = ReferenceRepo::new(provider.clone());
    reference
        .upsert_currency_scale(CurrencyScaleRow {
            tenant_id: s.tenant,
            currency: "USD".to_owned(),
            minor_units: 2,
            plausible_max_major: DEFAULT_PLAUSIBLE_MAX_MAJOR,
            source: "iso".to_owned(),
        })
        .await
        .unwrap();
    raw.execute_raw(pg(format!(
        "INSERT INTO bss.ledger_fiscal_period (tenant_id, legal_entity_id, period_id, fiscal_tz, status)
         VALUES ('{}','{}','{}','UTC','OPEN')",
        s.tenant, s.tenant, s.period_id
    )))
    .await
    .unwrap();

    for row in [
        account(s.tenant, s.ar, AccountClass::Ar, Side::Debit, None),
        account(
            s.tenant,
            s.revenue,
            AccountClass::Revenue,
            Side::Credit,
            Some("subscription"),
        ),
        account(
            s.tenant,
            s.contract_liability,
            AccountClass::ContractLiability,
            Side::Credit,
            Some("subscription"),
        ),
        account(
            s.tenant,
            s.contra_revenue,
            AccountClass::ContraRevenue,
            Side::Debit,
            None,
        ),
        account(
            s.tenant,
            s.goodwill,
            AccountClass::Goodwill,
            Side::Debit,
            None,
        ),
        account(
            s.tenant,
            s.reusable_credit,
            AccountClass::ReusableCredit,
            Side::Credit,
            None,
        ),
        account(
            s.tenant,
            s.tax,
            AccountClass::TaxPayable,
            Side::Credit,
            None,
        ),
        account(
            s.tenant,
            s.suspense,
            AccountClass::Suspense,
            Side::Credit,
            None,
        ),
        account(
            s.tenant,
            s.cash_clearing,
            AccountClass::CashClearing,
            Side::Debit,
            None,
        ),
    ] {
        reference.insert_account(row).await.unwrap();
    }
    (container, raw, provider, s)
}

// ── Invoice / handler helpers (mirror postgres_credit_note.rs) ───────────────

/// A `subscription` item, straight-line deferred over `periods` (the credited
/// obligation), or fully point-in-time when `periods == 0`.
fn item(amount: i64, periods: u32, item_ref: &str) -> InvoiceItem {
    InvoiceItem {
        amount_minor_ex_tax: amount,
        deferred_minor: 0,
        currency: "USD".to_owned(),
        revenue_stream: "subscription".to_owned(),
        catalog_class: Some(AccountClass::Revenue),
        contract_class: None,
        gl_code: Some("4000".to_owned()),
        recognition: Some(RecognitionInput {
            policy_ref: "policy.sl.v1".to_owned(),
            timing: if periods == 0 {
                RecognitionTiming::PointInTime
            } else {
                RecognitionTiming::StraightLine {
                    periods,
                    first_period_id: None,
                }
            },
            po_allocation_group: Some("grp-1".to_owned()),
            multi_po: false,
            ssp_snapshot_ref: None,
            subscription_ref: Some("sub-1".to_owned()),
            vc_estimate_ref: None,
            vc_method_ref: None,
            immaterial_one_shot_sku: false,
        }),
        invoice_item_ref: Some(item_ref.to_owned()),
        sku_or_plan_ref: None,
        price_id: None,
        pricing_snapshot_ref: None,
    }
}

fn invoice(s: &Seller, invoice_id: &str, items: Vec<InvoiceItem>) -> PostedInvoice {
    PostedInvoice {
        invoice_id: invoice_id.to_owned(),
        payer_tenant_id: s.payer,
        resource_tenant_id: None,
        seller_tenant_id: s.tenant,
        effective_at: naive(2026, 6, 1),
        due_date: Some(naive(2026, 7, 1)),
        period_id: s.period_id.clone(),
        items,
        tax: Vec::<TaxBreakdown>::new(),
        posted_by_actor_id: s.tenant,
        correlation_id: s.tenant,
    }
}

/// Post `inv` through the real `InvoicePostService` (the credited / charged
/// obligation), so the credit/debit notes have a posted invoice + schedule to act
/// against.
async fn post_invoice(provider: &DBProvider<DbError>, s: &Seller, inv: &PostedInvoice) {
    let harness = MetricsHarness::new();
    let svc = InvoicePostService::new(
        provider.clone(),
        Arc::new(LedgerEventPublisher::noop()),
        Arc::new(harness.metrics()),
        RecognitionConfig::default(),
        FxConfig::default(),
    );
    svc.post_invoice(
        &SecurityContext::anonymous(),
        &AccessScope::for_tenant(s.tenant),
        inv,
        true,
    )
    .await
    .expect("seed invoice post must succeed");
}

// ── Router / context / request helpers (mirror rest_credit.rs) ───────────────

/// Build the adjustment router over REAL handlers (no client wrapper — the
/// handlers are concrete) with the given enforcer layered as an `Extension`, as
/// `register_rest` does.
fn router_with(provider: DBProvider<DbError>, enforcer: PolicyEnforcer) -> Router {
    let state = Arc::new(ApiState {
        credit: Arc::new(CreditNoteHandler::new(
            provider.clone(),
            Arc::new(LedgerEventPublisher::noop()),
            Arc::new(bss_ledger::domain::ports::metrics::NoopLedgerMetrics),
        )),
        debit: Arc::new(DebitNoteHandler::new(
            provider.clone(),
            Arc::new(LedgerEventPublisher::noop()),
            Arc::new(bss_ledger::domain::ports::metrics::NoopLedgerMetrics),
            RecognitionConfig::default(),
        )),
        // The governed manual-adjustment handler. Built WITHOUT `.with_approval` (no
        // dual-control engine): these cases exercise the pure `govern` gate
        // (allow-list / write-off → 400) + the happy post / idempotent replay, none of
        // which depend on the D2 threshold. The over-D2 → 409 gate path needs the
        // ApprovalService wired and is covered by the Group-5 service tests.
        manual: Arc::new(ManualAdjustmentHandler::new(
            provider.clone(),
            Arc::new(LedgerEventPublisher::noop()),
            Arc::new(bss_ledger::infra::audit::secured_audit_sink::NoopSecuredAuditSink::new()),
        )),
        exposure_repo: AdjustmentRepo::new(provider),
    });
    let openapi = toolkit::api::OpenApiRegistryImpl::new();
    router(state, &openapi).layer(axum::Extension(enforcer))
}

fn authed_context() -> SecurityContext {
    SecurityContext::builder()
        .subject_id(SUBJECT_ID)
        .subject_tenant_id(SUBJECT_TENANT)
        .subject_type(gts_id!("cf.core.security.subject_user.v1~"))
        .token_scopes(vec!["*".to_owned()])
        .build()
        .expect("authed SecurityContext must build")
}

fn credit_body(
    s: &Seller,
    credit_note_id: &str,
    invoice_id: &str,
    amount_minor: i64,
    requested_deferred_minor: i64,
) -> serde_json::Value {
    serde_json::json!({
        "tenant_id": s.tenant,
        "payer_tenant_id": s.payer,
        "credit_note_id": credit_note_id,
        "origin_invoice_id": invoice_id,
        "origin_invoice_item_ref": "item-1",
        "po_allocation_group": "grp-1",
        "revenue_stream": "subscription",
        "currency": "USD",
        "scale": 2,
        "amount_minor": amount_minor,
        "tax_minor": 0,
        "tax": [],
        "requested_deferred_minor": requested_deferred_minor,
        "reason_code": "CUSTOMER_GOODWILL"
    })
}

fn debit_body(
    s: &Seller,
    debit_note_id: &str,
    invoice_id: &str,
    amount_minor: i64,
    deferred_minor: i64,
    periods: u32,
) -> serde_json::Value {
    let recognition = if deferred_minor > 0 {
        serde_json::json!({
            "policy_ref": "policy.sl.v1",
            "timing": "straight_line",
            "periods": periods,
            "po_allocation_group": "grp-1",
            "subscription_ref": "sub-1"
        })
    } else {
        serde_json::Value::Null
    };
    serde_json::json!({
        "tenant_id": s.tenant,
        "payer_tenant_id": s.payer,
        "debit_note_id": debit_note_id,
        "origin_invoice_id": invoice_id,
        "origin_invoice_item_ref": "item-1",
        "revenue_stream": "subscription",
        "currency": "USD",
        "scale": 2,
        "amount_minor": amount_minor,
        "tax_minor": 0,
        "tax": [],
        "deferred_minor": deferred_minor,
        "reason_code": "ADDITIONAL_USAGE",
        "recognition": recognition
    })
}

/// A governed manual-adjustment body: `action` + a two-leg balanced set
/// (`legs[i] = (account_class, side, amount_minor)`), no payer (the parking /
/// clearing classes are payer-less), mandatory `reason_code`, no tax. The preparer
/// actor is NOT in the body — it is the authenticated subject the handler stamps.
fn manual_body(
    s: &Seller,
    adjustment_id: &str,
    action: &str,
    legs: &[(AccountClass, Side, i64)],
) -> serde_json::Value {
    let legs: Vec<serde_json::Value> = legs
        .iter()
        .map(|(class, side, amount)| {
            serde_json::json!({
                "account_class": class.as_str(),
                "side": side.as_str(),
                "amount_minor": amount,
                "revenue_stream": serde_json::Value::Null,
            })
        })
        .collect();
    serde_json::json!({
        "tenant_id": s.tenant,
        "adjustment_id": adjustment_id,
        "action": action,
        "currency": "USD",
        "legs": legs,
        "reason_code": "ROUNDING_RESIDUE",
        "tax": []
    })
}

async fn send(
    router: Router,
    method: &str,
    uri: &str,
    body: Option<serde_json::Value>,
) -> (StatusCode, serde_json::Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    let req = if let Some(b) = body {
        builder = builder.header(header::CONTENT_TYPE, "application/json");
        builder.body(Body::from(b.to_string())).expect("build req")
    } else {
        builder.body(Body::empty()).expect("build req")
    };
    let response = router.oneshot(req).await.expect("send");
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1_000_000).await.unwrap();
    let value = if bytes.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&bytes).expect("body must be JSON")
    };
    (status, value)
}

/// Assert the RFC 9457 `problem+json` body carries the expected machine code
/// (`field_violation.reason` / `aborted.reason`). Matched as a substring of the
/// serialized body, the `rest_credit.rs` idiom (the exact field placement varies
/// by canonical category, but the wire code is always present in the body).
fn assert_problem_code(body: &serde_json::Value, code: &str) {
    let rendered = body.to_string();
    assert!(
        rendered.contains(code),
        "expected {code} in problem body, got {rendered}"
    );
}

// ── Tests ────────────────────────────────────────────────────────────────────

/// A deferred credit note → 201; an identical re-post (same `credit_note_id`) →
/// 200 (`replayed = true`), no second entry.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn credit_note_posts_201_then_replays_200() {
    let (_c, _raw, provider, s) = boot().await;
    // 1200 ex-tax subscription, straight-line over 12 ⇒ the whole 1200 defers;
    // AR = 1200; headroom original = 1200.
    post_invoice(
        &provider,
        &s,
        &invoice(&s, "INV-CN", vec![item(1200, 12, "item-1")]),
    )
    .await;

    let app =
        router_with(provider.clone(), allow_enforcer()).layer(axum::Extension(authed_context()));
    // Credit 300 entirely against the deferred balance.
    let (status, body) = send(
        app,
        "POST",
        "/bss-ledger/v1/credit-notes",
        Some(credit_body(&s, "CN-1", "INV-CN", 300, 300)),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "fresh credit note 201: {body}");
    assert_eq!(body["replayed"], serde_json::json!(false));

    // Idempotent replay (same credit_note_id) → 200 replayed.
    let app2 = router_with(provider, allow_enforcer()).layer(axum::Extension(authed_context()));
    let (status, body) = send(
        app2,
        "POST",
        "/bss-ledger/v1/credit-notes",
        Some(credit_body(&s, "CN-1", "INV-CN", 300, 300)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "replay 200: {body}");
    assert_eq!(body["replayed"], serde_json::json!(true));
}

/// A credit note whose incl-tax amount exceeds the invoice's headroom (no prior
/// debit note raised it) → 400 `CREDIT_NOTE_EXCEEDS_HEADROOM`.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn credit_note_over_headroom_rejected() {
    let (_c, _raw, provider, s) = boot().await;
    // original headroom = 700 (posted AR).
    post_invoice(
        &provider,
        &s,
        &invoice(&s, "INV-CAP", vec![item(700, 12, "item-1")]),
    )
    .await;

    let app = router_with(provider, allow_enforcer()).layer(axum::Extension(authed_context()));
    // Credit 800 against the deferred balance — over the 700 headroom.
    let (status, body) = send(
        app,
        "POST",
        "/bss-ledger/v1/credit-notes",
        Some(credit_body(&s, "CN-OVER", "INV-CAP", 800, 800)),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "over-headroom 400: {body}");
    assert_problem_code(&body, "CREDIT_NOTE_EXCEEDS_HEADROOM");
}

/// A credit note requesting a deferred part against a POINT-IN-TIME invoice (no
/// deferred schedule, so no obligation to reduce) → 400
/// `CREDIT_NOTE_SPLIT_AMBIGUOUS` (block-on-ambiguous, never a silent pro-rata).
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn credit_note_split_ambiguous_rejected() {
    let (_c, _raw, provider, s) = boot().await;
    // A fully point-in-time invoice ⇒ NO deferred schedule on `subscription`.
    post_invoice(
        &provider,
        &s,
        &invoice(&s, "INV-PIT", vec![item(500, 0, "item-1")]),
    )
    .await;

    let app = router_with(provider, allow_enforcer()).layer(axum::Extension(authed_context()));
    // Request a deferred part (200) with no obligation to reduce ⇒ ambiguous.
    let (status, body) = send(
        app,
        "POST",
        "/bss-ledger/v1/credit-notes",
        Some(credit_body(&s, "CN-AMB", "INV-PIT", 200, 200)),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "ambiguous 400: {body}");
    assert_problem_code(&body, "CREDIT_NOTE_SPLIT_AMBIGUOUS");
}

/// A deferred debit note → 201, and it RAISES the invoice's headroom: a credit
/// note that would have been over-cap against the original AR now fits (201).
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn debit_note_posts_201_and_raises_headroom() {
    let (_c, _raw, provider, s) = boot().await;
    // original headroom = 500 (posted AR).
    post_invoice(
        &provider,
        &s,
        &invoice(&s, "INV-DN", vec![item(500, 12, "item-1")]),
    )
    .await;

    // Debit note +600 (deferred over 6) ⇒ headroom now 500 + 600 = 1100.
    let app =
        router_with(provider.clone(), allow_enforcer()).layer(axum::Extension(authed_context()));
    let (status, body) = send(
        app,
        "POST",
        "/bss-ledger/v1/debit-notes",
        Some(debit_body(&s, "DN-1", "INV-DN", 600, 600, 6)),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "fresh debit note 201: {body}");
    assert_eq!(body["replayed"], serde_json::json!(false));

    // A credit note of 900 (> original 500, <= raised 1100) now fits.
    let app2 = router_with(provider, allow_enforcer()).layer(axum::Extension(authed_context()));
    let (status, body) = send(
        app2,
        "POST",
        "/bss-ledger/v1/credit-notes",
        Some(credit_body(&s, "CN-AFTER-DN", "INV-DN", 900, 900)),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "credit note within raised headroom 201: {body}"
    );
}

/// `GET …/exposure` after a credit note → 200 with the headroom counters
/// (`original`, `credit_note_total`, `remaining_headroom = original − credit`) +
/// the remaining open AR.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn get_exposure_returns_headroom_and_open_ar() {
    let (_c, _raw, provider, s) = boot().await;
    post_invoice(
        &provider,
        &s,
        &invoice(&s, "INV-EXP", vec![item(1000, 12, "item-1")]),
    )
    .await;

    // Post a 300 credit note so the exposure row is seeded + bumped.
    let app =
        router_with(provider.clone(), allow_enforcer()).layer(axum::Extension(authed_context()));
    let (status, _) = send(
        app,
        "POST",
        "/bss-ledger/v1/credit-notes",
        Some(credit_body(&s, "CN-EXP", "INV-EXP", 300, 300)),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let app2 = router_with(provider, allow_enforcer()).layer(axum::Extension(authed_context()));
    let (status, body) = send(
        app2,
        "GET",
        &format!(
            "/bss-ledger/v1/invoices/INV-EXP/exposure?tenant_id={}",
            s.tenant
        ),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "exposure 200: {body}");
    assert_eq!(body["original_total_minor"], serde_json::json!(1000));
    assert_eq!(body["credit_note_total_minor"], serde_json::json!(300));
    assert_eq!(body["debit_note_total_minor"], serde_json::json!(0));
    assert_eq!(
        body["remaining_headroom_minor"],
        serde_json::json!(700),
        "1000 − 300"
    );
    // Open AR net down by the 300 credit (1000 − 300 = 700).
    assert_eq!(
        body["open_ar_minor"],
        serde_json::json!(700),
        "body: {body}"
    );
}

/// `GET …/exposure` for an invoice with no note posted yet (no exposure row) →
/// 404 (no existence leak).
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn get_exposure_absent_returns_404() {
    let (_c, _raw, provider, s) = boot().await;
    post_invoice(
        &provider,
        &s,
        &invoice(&s, "INV-NONE", vec![item(1000, 12, "item-1")]),
    )
    .await;
    // No credit/debit note posted ⇒ no invoice_exposure row.
    let app = router_with(provider, allow_enforcer()).layer(axum::Extension(authed_context()));
    let (status, _body) = send(
        app,
        "GET",
        &format!(
            "/bss-ledger/v1/invoices/INV-NONE/exposure?tenant_id={}",
            s.tenant
        ),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// A credit note whose body `tenant_id` is outside the caller's authorized scope
/// → 403 (the write-gate's cross-tenant membership assert denies it). Uses the
/// allow fake (which authorizes ONLY `SUBJECT_TENANT`) with a FOREIGN body tenant,
/// matching how `rest_credit.rs` tests the cross-tenant deny.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn credit_note_foreign_tenant_denied_403() {
    let (_c, _raw, provider, s) = boot().await;
    let app = router_with(provider, allow_enforcer()).layer(axum::Extension(authed_context()));
    let mut body = credit_body(&s, "CN-FOREIGN", "INV-X", 100, 0);
    body["tenant_id"] = serde_json::json!(FOREIGN_TENANT);
    let (status, _b) = send(app, "POST", "/bss-ledger/v1/credit-notes", Some(body)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

/// A PDP deny → 403 (the gate maps the refusal). Mirrors
/// `rest_recognition.rs::trigger_recognition_run_pdp_deny_returns_403`.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn credit_note_pdp_deny_403() {
    let (_c, _raw, provider, s) = boot().await;
    let app = router_with(provider, deny_enforcer()).layer(axum::Extension(authed_context()));
    let (status, _b) = send(
        app,
        "POST",
        "/bss-ledger/v1/credit-notes",
        Some(credit_body(&s, "CN-DENY", "INV-X", 100, 0)),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

// ── Manual adjustments (Group 6 / Phase 3) ───────────────────────────────────

/// A governed rounding correction (DR CASH_CLEARING 1 / CR SUSPENSE 1, both in the
/// `ROUNDING_CORRECTION` allow-list, payer-less) → 201; an identical re-post (same
/// `adjustment_id`) → 200 (`replayed = true`), no second entry. The guarded
/// `CASH_CLEARING` (debit-normal in `boot`) is DEBITED so its projected balance
/// stays non-negative; `SUSPENSE` (credit-normal, un-guarded) takes the credit.
/// Mirrors `postgres_manual_adjustment::rounding_correction_posts_then_replays_idempotently`
/// driven through the REST surface.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn manual_adjustment_posts_201_then_replays_200() {
    let (_c, _raw, provider, s) = boot().await;
    let app =
        router_with(provider.clone(), allow_enforcer()).layer(axum::Extension(authed_context()));
    let legs = [
        (AccountClass::CashClearing, Side::Debit, 1),
        (AccountClass::Suspense, Side::Credit, 1),
    ];
    let (status, body) = send(
        app,
        "POST",
        "/bss-ledger/v1/manual-adjustments",
        Some(manual_body(&s, "ADJ-RC-1", "ROUNDING_CORRECTION", &legs)),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "fresh manual adjustment 201: {body}"
    );
    assert_eq!(body["replayed"], serde_json::json!(false));

    // Idempotent replay (same adjustment_id) → 200 replayed.
    let app2 = router_with(provider, allow_enforcer()).layer(axum::Extension(authed_context()));
    let (status, body) = send(
        app2,
        "POST",
        "/bss-ledger/v1/manual-adjustments",
        Some(manual_body(&s, "ADJ-RC-1", "ROUNDING_CORRECTION", &legs)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "replay 200: {body}");
    assert_eq!(body["replayed"], serde_json::json!(true));
}

/// A balanced adjustment with a leg OUTSIDE the action's allow-list (DR SUSPENSE 5 /
/// CR TAX_PAYABLE 5 — `TAX_PAYABLE` is in no allow-list) → 400
/// `MANUAL_ADJUSTMENT_NOT_ALLOWED`, no books effect (the pure `govern` gate rejects
/// before the post). Mirrors `postgres_manual_adjustment::class_outside_allow_list_is_not_allowed`.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn manual_adjustment_class_outside_allow_list_rejected() {
    let (_c, _raw, provider, s) = boot().await;
    let app = router_with(provider, allow_enforcer()).layer(axum::Extension(authed_context()));
    let legs = [
        (AccountClass::Suspense, Side::Debit, 5),
        (AccountClass::TaxPayable, Side::Credit, 5),
    ];
    let (status, body) = send(
        app,
        "POST",
        "/bss-ledger/v1/manual-adjustments",
        Some(manual_body(
            &s,
            "ADJ-NOTALLOWED",
            "ROUNDING_CORRECTION",
            &legs,
        )),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "not-allowed 400: {body}");
    assert_problem_code(&body, "MANUAL_ADJUSTMENT_NOT_ALLOWED");
}

/// A disguised write-off shape (DR CONTRA_REVENUE 5 / CR AR 5 — an unpaired
/// CONTRA_REVENUE leg with no same-stream recognized-REVENUE reduction) → 400
/// `MANUAL_ADJUSTMENT_NOT_ALLOWED` (the `govern` write-off structural guard fires;
/// the handler additionally captures + pages out-of-band, but the wire result is the
/// same canonical 400). `payer_tenant_id` is supplied because the AR leg is
/// payer-scoped — but the write-off guard rejects it before the payer gate matters.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn manual_adjustment_write_off_rejected() {
    let (_c, _raw, provider, s) = boot().await;
    let app = router_with(provider, allow_enforcer()).layer(axum::Extension(authed_context()));
    let legs = [
        (AccountClass::ContraRevenue, Side::Debit, 5),
        (AccountClass::Ar, Side::Credit, 5),
    ];
    let mut body = manual_body(&s, "ADJ-WRITEOFF", "SUSPENSE_CLEAR", &legs);
    // The AR leg is payer-scoped; supply the payer so the write-off guard (not the
    // payer gate) is the rejecting check.
    body["payer_tenant_id"] = serde_json::json!(s.payer);
    let (status, body) = send(app, "POST", "/bss-ledger/v1/manual-adjustments", Some(body)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "write-off 400: {body}");
    assert_problem_code(&body, "MANUAL_ADJUSTMENT_NOT_ALLOWED");
}
