//! API-level (router) tests for the refund REST surface (Slice 3, Phase 2, Group
//! G): `POST /bss-ledger/v1/refunds`, `POST /bss-ledger/v1/refund-with-credit-note`,
//! and `GET /bss-ledger/v1/refunds/{refund_id}` (tenant in body for the writes, in
//! the query for the read).
//!
//! Like `rest_adjustments.rs`, these drive the router against a REAL testcontainer
//! Postgres so a refund runs end-to-end through the foundation engine
//! (`PostingService` + the in-txn `RefundPostSidecar`) + the per-payment money-out
//! caps + (for the composite) the credit-note's second entry in the SAME txn. The
//! refund handler is a CONCRETE orchestrator (not behind `LedgerClientV1`), so the
//! router's `ApiState` is built directly over a real GATED `RefundHandler` (+ the
//! composite `CreditNoteHandler`) + a real `ApprovalService` (so over-D2 → 409) +
//! the `AdjustmentRepo`. An always-allow `PolicyEnforcer` fake (echoing the subject
//! tenant as an `owner_tenant_id` `In` constraint) is layered as an `Extension`,
//! mirroring `register_rest`; a deny fake covers the 403.
//!
//! Cases:
//! - a Pattern-A single-step refund of a settled payment → 201, an idempotent
//!   re-post (same `psp_refund_id:phase`) → 200 (`replayed = true`);
//! - a refund whose cash crosses the default D2 threshold (≥ 100_000 minor) → 409
//!   `DUAL_CONTROL_REQUIRED`;
//! - a refund whose origin payment has no settlement (refund-before-payment) → 202
//!   `refund-quarantined` (a normal-body token, never posted);
//! - `POST /refund-with-credit-note` → 201 with BOTH entry ids (the composite
//!   commits the refund + the paired credit note atomically);
//! - `GET …/refunds/{id}` → 200 with the recorded refund + its clearing state;
//! - a refund whose body `tenant_id` is outside the caller's scope → 403; a PDP
//!   deny → 403.
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
use bss_ledger::api::rest::refunds::{ApiState, router};
use bss_ledger::config::{FxConfig, RecognitionConfig};
use bss_ledger::domain::approval::intent::ApprovalIntent;
use bss_ledger::domain::error::DomainError;
use bss_ledger::domain::model::{AccountRow, CurrencyScaleRow};
use bss_ledger::domain::money::DEFAULT_PLAUSIBLE_MAX_MAJOR;
use bss_ledger::domain::payment::settlement::SettlementInput;
use bss_ledger::domain::ports::metrics::{LedgerMetricsPort, NoopLedgerMetrics};
use bss_ledger::infra::adjustment::credit_note_service::CreditNoteHandler;
use bss_ledger::infra::adjustment::refund_service::RefundHandler;
use bss_ledger::infra::approval::service::{ApprovalExecutor, ApprovalService};
use bss_ledger::infra::events::publisher::LedgerEventPublisher;
use bss_ledger::infra::invoice_post::InvoicePostService;
use bss_ledger::infra::payment::settle::SettlementService;
use bss_ledger::infra::storage::migrations::Migrator;
use bss_ledger::infra::storage::repo::AdjustmentRepo;
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

// ── PDP fakes (mirror rest_adjustments.rs) ───────────────────────────────────

const SUBJECT_TENANT: Uuid = uuid::uuid!("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee");
const SUBJECT_ID: Uuid = uuid::uuid!("11111111-2222-3333-4444-555555555555");
const FOREIGN_TENANT: Uuid = uuid::uuid!("ffffffff-ffff-ffff-ffff-ffffffffffff");

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

/// An `ApprovalExecutor` stub: the over-D2 gate only CREATES a PENDING approval +
/// returns its id (→ 409); it never executes a held mutation in these tests.
#[derive(Clone, Default)]
struct NoopExecutor;

#[async_trait]
impl ApprovalExecutor for NoopExecutor {
    async fn execute(
        &self,
        _ctx: &SecurityContext,
        _scope: &AccessScope,
        _intent: &ApprovalIntent,
    ) -> Result<(), DomainError> {
        Ok(())
    }
}

// ── DB + seller setup ─────────────────────────────────────────────────────────

/// Provisioned seller: the refund chart (UNALLOCATED / AR / REFUND_CLEARING /
/// CASH_CLEARING / PSP_FEE_EXPENSE) plus the credit-note chart (REVENUE(sub) /
/// CONTRACT_LIABILITY(sub) / CONTRA_REVENUE / GOODWILL / REUSABLE_CREDIT / TAX) for
/// the composite. `tenant` is `SUBJECT_TENANT` so the handler authz gate passes;
/// `period_id` is the CURRENT month.
struct Seller {
    tenant: Uuid,
    payer: Uuid,
    cash: Uuid,
    unallocated: Uuid,
    refund_clearing: Uuid,
    ar: Uuid,
    psp_fee: Uuid,
    revenue: Uuid,
    contract_liability: Uuid,
    contra_revenue: Uuid,
    goodwill: Uuid,
    reusable_credit: Uuid,
    tax: Uuid,
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
        cash: Uuid::now_v7(),
        unallocated: Uuid::now_v7(),
        refund_clearing: Uuid::now_v7(),
        ar: Uuid::now_v7(),
        psp_fee: Uuid::now_v7(),
        revenue: Uuid::now_v7(),
        contract_liability: Uuid::now_v7(),
        contra_revenue: Uuid::now_v7(),
        goodwill: Uuid::now_v7(),
        reusable_credit: Uuid::now_v7(),
        tax: Uuid::now_v7(),
        period_id: bss_ledger::domain::instant::yyyymm(now),
    };

    let reference = bss_ledger::infra::storage::repo::ReferenceRepo::new(provider.clone());
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
        account(
            s.tenant,
            s.cash,
            AccountClass::CashClearing,
            Side::Debit,
            None,
        ),
        account(
            s.tenant,
            s.unallocated,
            AccountClass::Unallocated,
            Side::Credit,
            None,
        ),
        account(
            s.tenant,
            s.refund_clearing,
            AccountClass::RefundClearing,
            Side::Credit,
            None,
        ),
        account(s.tenant, s.ar, AccountClass::Ar, Side::Debit, None),
        account(
            s.tenant,
            s.psp_fee,
            AccountClass::PspFeeExpense,
            Side::Debit,
            None,
        ),
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
    ] {
        reference.insert_account(row).await.unwrap();
    }
    (container, raw, provider, s)
}

/// Settle `gross` (fee 0) for `payment_id` — seeds the `payment_settlement` row the
/// refund resolves as its origin (Pattern A draws its `UNALLOCATED` pool).
async fn settle(provider: &DBProvider<DbError>, s: &Seller, payment_id: &str, gross: i64) {
    SettlementService::new(
        provider.clone(),
        Arc::new(LedgerEventPublisher::noop()),
        Arc::new(NoopLedgerMetrics),
    )
    .settle(
        &SecurityContext::anonymous(),
        &AccessScope::for_tenant(s.tenant),
        SettlementInput {
            tenant_id: s.tenant,
            payer_tenant_id: s.payer,
            payment_id: payment_id.to_owned(),
            gross_minor: gross,
            fee_minor: 0,
            currency: "USD".to_owned(),
            effective_at: None,
        },
    )
    .await
    .expect("settle must succeed");
}

/// Post an invoice (deferred over 12) so the composite's credit note has a posted
/// invoice + schedule to act against.
async fn post_invoice(provider: &DBProvider<DbError>, s: &Seller, invoice_id: &str, amount: i64) {
    use bss_ledger::domain::invoice::builder::{InvoiceItem, PostedInvoice, TaxBreakdown};
    use bss_ledger::domain::recognition::input::{RecognitionInput, RecognitionTiming};
    let item = InvoiceItem {
        amount_minor_ex_tax: amount,
        deferred_minor: 0,
        currency: "USD".to_owned(),
        revenue_stream: "subscription".to_owned(),
        catalog_class: Some(AccountClass::Revenue),
        contract_class: None,
        gl_code: Some("4000".to_owned()),
        recognition: Some(RecognitionInput {
            policy_ref: "policy.sl.v1".to_owned(),
            timing: RecognitionTiming::StraightLine {
                periods: 12,
                first_period_id: None,
            },
            po_allocation_group: Some("grp-1".to_owned()),
            multi_po: false,
            ssp_snapshot_ref: None,
            subscription_ref: Some("sub-1".to_owned()),
            vc_estimate_ref: None,
            vc_method_ref: None,
            immaterial_one_shot_sku: false,
        }),
        invoice_item_ref: Some("item-1".to_owned()),
        sku_or_plan_ref: None,
        price_id: None,
        pricing_snapshot_ref: None,
    };
    let inv = PostedInvoice {
        invoice_id: invoice_id.to_owned(),
        payer_tenant_id: s.payer,
        resource_tenant_id: None,
        seller_tenant_id: s.tenant,
        effective_at: naive(2026, 6, 1),
        due_date: Some(naive(2026, 7, 1)),
        period_id: s.period_id.clone(),
        items: vec![item],
        tax: Vec::<TaxBreakdown>::new(),
        posted_by_actor_id: s.tenant,
        correlation_id: s.tenant,
    };
    InvoicePostService::new(
        provider.clone(),
        Arc::new(LedgerEventPublisher::noop()),
        Arc::new(NoopLedgerMetrics),
        RecognitionConfig::default(),
        FxConfig::default(),
    )
    .post_invoice(
        &SecurityContext::anonymous(),
        &AccessScope::for_tenant(s.tenant),
        &inv,
        true,
    )
    .await
    .expect("seed invoice post must succeed");
}

// ── Router / context / request helpers ────────────────────────────────────────

/// Build the refund router over a REAL gated `RefundHandler` (with the composite
/// credit-note handler + a real `ApprovalService` so over-D2 → 409) + the
/// `AdjustmentRepo`, with the given enforcer layered as an `Extension`.
fn router_with(provider: DBProvider<DbError>, enforcer: PolicyEnforcer) -> Router {
    let publisher = Arc::new(LedgerEventPublisher::noop());
    let metrics: Arc<dyn LedgerMetricsPort> = Arc::new(NoopLedgerMetrics);
    let credit_note = Arc::new(CreditNoteHandler::new(
        provider.clone(),
        Arc::clone(&publisher),
        Arc::clone(&metrics),
    ));
    let approval = Arc::new(ApprovalService::new(
        provider.clone(),
        Arc::new(NoopExecutor),
        Arc::clone(&metrics),
        bss_ledger::config::FxConfig::default(),
    ));
    let refunds = Arc::new(
        RefundHandler::new(provider.clone(), Arc::clone(&publisher))
            .with_approval(approval)
            .with_credit_note_handler(credit_note)
            .with_metrics(metrics),
    );
    let state = Arc::new(ApiState {
        refunds,
        refund_repo: AdjustmentRepo::new(provider),
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

/// A Pattern-A single-step refund body (no `invoice_id`, `two_stage = false` ⇒ one
/// `initiated` entry straight to cash).
fn refund_body(
    s: &Seller,
    refund_id: &str,
    psp_refund_id: &str,
    payment_id: &str,
    amount_minor: i64,
) -> serde_json::Value {
    serde_json::json!({
        "tenant_id": s.tenant,
        "payer_tenant_id": s.payer,
        "refund_id": refund_id,
        "psp_refund_id": psp_refund_id,
        "phase": "initiated",
        "pattern": "A_UNALLOCATED",
        "payment_id": payment_id,
        "currency": "USD",
        "amount_minor": amount_minor,
        "scale": 2,
        "two_stage": false
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

fn assert_problem_code(body: &serde_json::Value, code: &str) {
    let rendered = body.to_string();
    assert!(
        rendered.contains(code),
        "expected {code} in problem body, got {rendered}"
    );
}

// ── Tests ──────────────────────────────────────────────────────────────────────

/// A Pattern-A single-step refund of a settled payment → 201; an identical re-post
/// (same `psp_refund_id:phase`) → 200 (`replayed = true`), no second entry.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn refund_posts_201_then_replays_200() {
    let (_c, _raw, provider, s) = boot().await;
    settle(&provider, &s, "PAY-1", 5_000).await;

    let app =
        router_with(provider.clone(), allow_enforcer()).layer(axum::Extension(authed_context()));
    let (status, body) = send(
        app,
        "POST",
        "/bss-ledger/v1/refunds",
        Some(refund_body(&s, "RF-1", "PSP-1", "PAY-1", 300)),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "fresh refund 201: {body}");
    assert_eq!(body["replayed"], serde_json::json!(false));

    let app2 = router_with(provider, allow_enforcer()).layer(axum::Extension(authed_context()));
    let (status, body) = send(
        app2,
        "POST",
        "/bss-ledger/v1/refunds",
        Some(refund_body(&s, "RF-1", "PSP-1", "PAY-1", 300)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "replay 200: {body}");
    assert_eq!(body["replayed"], serde_json::json!(true));
}

/// A refund whose returned cash crosses the default D2 threshold (≥ 100_000 minor)
/// → 409 `DUAL_CONTROL_REQUIRED` (the gate opens a PENDING approval; nothing posts).
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn refund_over_d2_threshold_requires_dual_control_409() {
    let (_c, _raw, provider, s) = boot().await;
    settle(&provider, &s, "PAY-BIG", 200_000).await;

    let app = router_with(provider, allow_enforcer()).layer(axum::Extension(authed_context()));
    // 150_000 minor (> the 100_000 default D2) ⇒ dual-control.
    let (status, body) = send(
        app,
        "POST",
        "/bss-ledger/v1/refunds",
        Some(refund_body(&s, "RF-BIG", "PSP-BIG", "PAY-BIG", 150_000)),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "over-D2 409: {body}");
    assert_problem_code(&body, "DUAL_CONTROL_REQUIRED");
}

/// A refund whose origin payment has no settlement (refund-before-payment) → 202
/// `refund-quarantined` (a normal-body kebab token, NOT an error; nothing posted).
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn refund_before_payment_quarantined_202() {
    let (_c, _raw, provider, s) = boot().await;
    // No settle for PAY-MISSING ⇒ no origin settlement ⇒ quarantine.
    let app = router_with(provider, allow_enforcer()).layer(axum::Extension(authed_context()));
    let (status, body) = send(
        app,
        "POST",
        "/bss-ledger/v1/refunds",
        Some(refund_body(&s, "RF-Q", "PSP-Q", "PAY-MISSING", 400)),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "quarantine 202: {body}");
    assert_eq!(
        body["status"],
        serde_json::json!("refund-quarantined"),
        "body: {body}"
    );
    assert_eq!(body["flow"], serde_json::json!("REFUND_QUARANTINE"));
}

/// `POST /refund-with-credit-note` → 201 with BOTH entry ids (the refund + the
/// paired credit note, committed atomically in one txn).
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn refund_with_credit_note_posts_both_201() {
    let (_c, _raw, provider, s) = boot().await;
    settle(&provider, &s, "PAY-CN", 5_000).await;
    post_invoice(&provider, &s, "INV-CN", 1_200).await;

    let app = router_with(provider, allow_enforcer()).layer(axum::Extension(authed_context()));
    let body = serde_json::json!({
        "refund": refund_body(&s, "RF-CN", "PSP-CN", "PAY-CN", 300),
        "credit_note": {
            "tenant_id": s.tenant,
            "payer_tenant_id": s.payer,
            "credit_note_id": "CN-PAIR",
            "origin_invoice_id": "INV-CN",
            "origin_invoice_item_ref": "item-1",
            "po_allocation_group": "grp-1",
            "revenue_stream": "subscription",
            "currency": "USD",
            "scale": 2,
            "amount_minor": 300,
            "tax_minor": 0,
            "tax": [],
            "requested_deferred_minor": 300,
            "reason_code": "CUSTOMER_GOODWILL"
        }
    });
    let (status, resp) = send(
        app,
        "POST",
        "/bss-ledger/v1/refund-with-credit-note",
        Some(body),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "composite 201: {resp}");
    assert!(
        resp["refund_entry_id"].is_string(),
        "refund entry id present: {resp}"
    );
    assert!(
        resp["credit_note_entry_id"].is_string(),
        "credit-note entry id present: {resp}"
    );
    assert_eq!(resp["replayed"], serde_json::json!(false));
}

/// `GET …/refunds/{id}` after a stage-1 two-stage refund → 200 with the recorded
/// refund + `clearing_state = PENDING` (the REFUND_CLEARING balance is open).
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn get_refund_returns_record_and_clearing_state() {
    let (_c, _raw, provider, s) = boot().await;
    settle(&provider, &s, "PAY-G", 5_000).await;

    // A two-stage stage-1 ⇒ clearing_state = PENDING.
    let app =
        router_with(provider.clone(), allow_enforcer()).layer(axum::Extension(authed_context()));
    let mut body = refund_body(&s, "RF-G", "PSP-G", "PAY-G", 250);
    body["two_stage"] = serde_json::json!(true);
    let (status, _) = send(app, "POST", "/bss-ledger/v1/refunds", Some(body)).await;
    assert_eq!(status, StatusCode::CREATED);

    let app2 = router_with(provider, allow_enforcer()).layer(axum::Extension(authed_context()));
    let (status, resp) = send(
        app2,
        "GET",
        &format!("/bss-ledger/v1/refunds/RF-G?tenant_id={}", s.tenant),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "get refund 200: {resp}");
    assert_eq!(resp["refund_id"], serde_json::json!("RF-G"));
    assert_eq!(resp["psp_refund_id"], serde_json::json!("PSP-G"));
    assert_eq!(resp["pattern"], serde_json::json!("A_UNALLOCATED"));
    assert_eq!(resp["clearing_state"], serde_json::json!("PENDING"));
    assert_eq!(resp["amount_minor"], serde_json::json!(250));
}

/// `GET …/refunds/{id}` for an unknown refund → 404 (no existence leak).
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn get_refund_absent_returns_404() {
    let (_c, _raw, provider, s) = boot().await;
    let app = router_with(provider, allow_enforcer()).layer(axum::Extension(authed_context()));
    let (status, _) = send(
        app,
        "GET",
        &format!("/bss-ledger/v1/refunds/RF-NONE?tenant_id={}", s.tenant),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// A refund whose body `tenant_id` is outside the caller's authorized scope → 403
/// (the write-gate's cross-tenant membership assert denies it).
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn refund_foreign_tenant_denied_403() {
    let (_c, _raw, provider, s) = boot().await;
    let app = router_with(provider, allow_enforcer()).layer(axum::Extension(authed_context()));
    let mut body = refund_body(&s, "RF-FOREIGN", "PSP-F", "PAY-F", 100);
    body["tenant_id"] = serde_json::json!(FOREIGN_TENANT);
    let (status, _b) = send(app, "POST", "/bss-ledger/v1/refunds", Some(body)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

/// A PDP deny → 403 (the gate maps the refusal).
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn refund_pdp_deny_403() {
    let (_c, _raw, provider, s) = boot().await;
    let app = router_with(provider, deny_enforcer()).layer(axum::Extension(authed_context()));
    let (status, _b) = send(
        app,
        "POST",
        "/bss-ledger/v1/refunds",
        Some(refund_body(&s, "RF-DENY", "PSP-D", "PAY-D", 100)),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}
