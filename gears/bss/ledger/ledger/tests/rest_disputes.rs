//! API-level (router) tests for the ledger's chargeback (dispute) REST surface
//! (Group D): `POST /bss-ledger/v1/disputes/{dispute_id}/phases` (record a
//! dispute phase, tenant in body, dispute id in path).
//!
//! Mirrors `rest_payments.rs`: the router drives a REAL testcontainer Postgres
//! through a small in-test `LedgerClientV1` (`RealDisputeClient`) that delegates
//! `record_dispute_phase` to the SAME `pub` `ChargebackService` the in-process
//! `LedgerLocalClient` uses (parsing the wire phase / funds literals exactly as
//! `local_client.rs` does); every other trait method is `unimplemented!()`. An
//! always-allow `PolicyEnforcer` fake (no PDP) is layered as an `Extension`,
//! echoing the subject tenant as an `owner_tenant_id` constraint so the handler
//! write-gate's target-membership check passes when the body `tenant_id` equals
//! the subject tenant.
//!
//! Cases: an `opened` cash-hold (withheld) after a settle → 201; a cross-tenant
//! record (body `tenant_id` outside the caller's scope) → 403; an out-of-order
//! `won` before any `opened` → 202 with the `dispute-phase-queued` status token
//! (mirrors the allocate 202 `allocation-queued` assertion).

#![allow(
    clippy::non_ascii_literal,
    clippy::let_underscore_must_use,
    clippy::needless_collect,
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::doc_markdown,
    clippy::panic,
    clippy::too_many_lines
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
use bss_ledger::api::rest::disputes::{ApiState, router};
use bss_ledger::domain::model::{AccountRow, CurrencyScaleRow};
use bss_ledger::domain::money::DEFAULT_PLAUSIBLE_MAX_MAJOR;
use bss_ledger::domain::payment::settlement::SettlementInput;
use bss_ledger::domain::ports::metrics::NoopLedgerMetrics;
use bss_ledger::infra::events::publisher::LedgerEventPublisher;
use bss_ledger::infra::payment::chargeback::{
    ChargebackOutcome, ChargebackRequest, ChargebackService,
};
use bss_ledger::infra::payment::settle::SettlementService;
use bss_ledger::infra::storage::migrations::Migrator;
use bss_ledger::infra::storage::repo::{DisputeRepo, ReferenceRepo};
use bss_ledger_sdk::api::LedgerClientV1;
use bss_ledger_sdk::posting::{
    AllocateOutcome, AllocatePayment, ArInvoiceBalanceView, BalanceView, DisputeOutcome,
    DisputeQueued, DisputeRecorded, EntryView, LineView, ODataQuery, Page, PostEntry, PostingRef,
    RecordDisputePhase, SettlePayment, UnallocatedView,
};
use bss_ledger_sdk::{AccountClass, ProvisionOutcome, ProvisionRequest, Side};
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

// ── The in-test client: delegates record_dispute_phase to the real service ────

/// A `LedgerClientV1` over a real `DBProvider` that drives the SAME `pub`
/// `ChargebackService` the in-process `LedgerLocalClient` uses, so the router
/// runs end-to-end against the testcontainer DB. The PEP gate the production
/// client performs internally is exercised at the HANDLER layer here (the
/// allow-all enforcer extension), so the service is called with a plain
/// per-tenant `AccessScope`, mirroring `rest_payments.rs::RealPaymentClient`.
/// Only `record_dispute_phase` is implemented.
struct RealDisputeClient {
    provider: DBProvider<DbError>,
}

impl RealDisputeClient {
    fn chargeback_svc(&self) -> ChargebackService {
        ChargebackService::new(
            self.provider.clone(),
            Arc::new(LedgerEventPublisher::noop()),
            Arc::new(NoopLedgerMetrics),
        )
    }
}

#[async_trait::async_trait]
impl LedgerClientV1 for RealDisputeClient {
    async fn record_dispute_phase(
        &self,
        ctx: &SecurityContext,
        req: RecordDisputePhase,
    ) -> Result<DisputeOutcome, CanonicalError> {
        let scope = AccessScope::for_tenant(req.tenant_id);
        // Parse the wire literals at the boundary exactly as `local_client.rs`
        // does (a bad literal is `InvalidRequest` ⇒ 400, not a deep fault).
        let phase = bss_ledger::domain::payment::chargeback::DisputePhase::parse(&req.phase)
            .ok_or_else(|| {
                CanonicalError::from(bss_ledger::domain::error::DomainError::InvalidRequest(
                    format!("unknown dispute phase {:?}", req.phase),
                ))
            })?;
        let funds_at_open =
            bss_ledger::domain::payment::chargeback::FundsAtOpen::parse(&req.funds_at_open)
                .ok_or_else(|| {
                    CanonicalError::from(bss_ledger::domain::error::DomainError::InvalidRequest(
                        format!("unknown funds_at_open {:?}", req.funds_at_open),
                    ))
                })?;
        let request = ChargebackRequest {
            tenant_id: req.tenant_id,
            payer_tenant_id: req.payer_tenant_id,
            payment_id: req.payment_id,
            dispute_id: req.dispute_id,
            invoice_id: req.invoice_id,
            cycle: req.cycle,
            phase,
            funds_at_open,
            disputed_amount_minor: req.disputed_amount_minor,
            currency: req.currency,
            effective_at: req.effective_at,
        };
        match self
            .chargeback_svc()
            .record_phase(ctx, &scope, request)
            .await
            .map_err(CanonicalError::from)?
        {
            ChargebackOutcome::Recorded(posting) => {
                Ok(DisputeOutcome::Recorded(DisputeRecorded { posting }))
            }
            ChargebackOutcome::Queued(queued) => Ok(DisputeOutcome::Queued(DisputeQueued {
                flow: queued.flow,
                business_id: queued.business_id,
                queued_at: queued.queued_at,
            })),
        }
    }

    // ── The non-dispute surface is not exercised by these router tests. ──

    async fn post_credit_application(
        &self,
        _ctx: &SecurityContext,
        _req: bss_ledger_sdk::CreditApplication,
    ) -> Result<bss_ledger_sdk::CreditApplicationApplied, CanonicalError> {
        unimplemented!("not exercised by the dispute router tests")
    }

    async fn settle_payment(
        &self,
        _ctx: &SecurityContext,
        _req: SettlePayment,
    ) -> Result<PostingRef, CanonicalError> {
        unimplemented!("not exercised by the dispute router tests")
    }

    async fn return_payment(
        &self,
        _ctx: &SecurityContext,
        _req: bss_ledger_sdk::ReturnPayment,
    ) -> Result<PostingRef, CanonicalError> {
        unimplemented!("not exercised by the dispute router tests")
    }

    async fn allocate_payment(
        &self,
        _ctx: &SecurityContext,
        _req: AllocatePayment,
    ) -> Result<AllocateOutcome, CanonicalError> {
        unimplemented!("not exercised by the dispute router tests")
    }

    async fn list_payment_allocations(
        &self,
        _ctx: &SecurityContext,
        _tenant_id: Uuid,
        _payment_id: String,
    ) -> Result<Vec<bss_ledger_sdk::posting::AllocationView>, CanonicalError> {
        unimplemented!("not exercised by the dispute router tests")
    }

    async fn read_unallocated(
        &self,
        _ctx: &SecurityContext,
        _tenant_id: Uuid,
        _payer_tenant_id: Uuid,
        _currency: String,
    ) -> Result<UnallocatedView, CanonicalError> {
        unimplemented!("not exercised by the dispute router tests")
    }

    async fn post_balanced_entry(
        &self,
        _ctx: &SecurityContext,
        _entry: PostEntry,
    ) -> Result<PostingRef, CanonicalError> {
        unimplemented!("not exercised by the dispute router tests")
    }

    async fn read_account_balance(
        &self,
        _ctx: &SecurityContext,
        _tenant_id: Uuid,
        _account_id: Uuid,
    ) -> Result<Option<i64>, CanonicalError> {
        unimplemented!("not exercised by the dispute router tests")
    }

    async fn list_accounts(
        &self,
        _ctx: &SecurityContext,
        _tenant_id: Uuid,
        _query: &ODataQuery,
    ) -> Result<Page<bss_ledger_sdk::AccountInfo>, CanonicalError> {
        unimplemented!("not exercised by the dispute router tests")
    }

    async fn get_entry(
        &self,
        _ctx: &SecurityContext,
        _tenant_id: Uuid,
        _entry_id: Uuid,
    ) -> Result<Option<EntryView>, CanonicalError> {
        unimplemented!("not exercised by the dispute router tests")
    }

    async fn list_lines(
        &self,
        _ctx: &SecurityContext,
        _tenant_id: Uuid,
        _query: &ODataQuery,
    ) -> Result<Page<LineView>, CanonicalError> {
        unimplemented!("not exercised by the dispute router tests")
    }

    async fn list_balances(
        &self,
        _ctx: &SecurityContext,
        _tenant_id: Uuid,
        _query: &ODataQuery,
    ) -> Result<Page<BalanceView>, CanonicalError> {
        unimplemented!("not exercised by the dispute router tests")
    }

    async fn list_ar_invoice_balances(
        &self,
        _ctx: &SecurityContext,
        _tenant_id: Uuid,
        _payer_tenant_id: Option<Uuid>,
    ) -> Result<Vec<ArInvoiceBalanceView>, CanonicalError> {
        unimplemented!("not exercised by the dispute router tests")
    }

    async fn provision(
        &self,
        _ctx: &SecurityContext,
        _req: ProvisionRequest,
    ) -> Result<ProvisionOutcome, CanonicalError> {
        unimplemented!("not exercised by the dispute router tests")
    }

    async fn close_period(
        &self,
        _ctx: &SecurityContext,
        _tenant_id: Uuid,
        _period_id: String,
    ) -> Result<bss_ledger_sdk::CloseOutcome, CanonicalError> {
        unimplemented!("not exercised by the dispute router tests")
    }

    async fn trigger_recognition_run(
        &self,
        _ctx: &SecurityContext,
        _req: bss_ledger_sdk::TriggerRecognitionRun,
    ) -> Result<bss_ledger_sdk::RecognitionRunOutcome, CanonicalError> {
        unimplemented!("not exercised by the dispute router tests")
    }

    async fn list_revenue_disaggregation(
        &self,
        _ctx: &SecurityContext,
        _query: bss_ledger_sdk::RevenueDisaggregationQuery,
    ) -> Result<bss_ledger_sdk::RevenueDisaggregation, CanonicalError> {
        unimplemented!("not exercised by the dispute router tests")
    }

    async fn change_recognition_schedule(
        &self,
        _ctx: &SecurityContext,
        _cmd: bss_ledger_sdk::ChangeRecognitionSchedule,
    ) -> Result<bss_ledger_sdk::ScheduleChangeRef, CanonicalError> {
        unimplemented!("not exercised by the dispute router tests")
    }

    async fn get_recognition_schedule(
        &self,
        _ctx: &SecurityContext,
        _tenant_id: Uuid,
        _schedule_id: String,
    ) -> Result<Option<bss_ledger_sdk::RecognitionScheduleView>, CanonicalError> {
        unimplemented!("not exercised by the dispute router tests")
    }

    async fn list_recognition_schedules(
        &self,
        _ctx: &SecurityContext,
        _tenant_id: Uuid,
        _invoice_id: Option<String>,
        _revenue_stream: Option<String>,
    ) -> Result<bss_ledger_sdk::RecognitionScheduleList, CanonicalError> {
        unimplemented!("not exercised by the dispute router tests")
    }
}

// ── Authz fakes (mirror rest_payments.rs) ─────────────────────────────────────

fn subject_tenant_id(request: &EvaluationRequest) -> Uuid {
    request
        .subject
        .properties
        .get("tenant_id")
        .and_then(|v| v.as_str())
        .and_then(|s| Uuid::parse_str(s).ok())
        .unwrap_or_else(Uuid::nil)
}

fn tenant_in_constraint(tenant_id: Uuid) -> Constraint {
    Constraint {
        predicates: vec![Predicate::In(InPredicate::new(
            toolkit_security::pep_properties::OWNER_TENANT_ID,
            [tenant_id],
        ))],
    }
}

/// Always-allow fake that echoes the subject's tenant as an `owner_tenant_id`
/// `In` constraint (so the handler write-gate's target-membership check passes
/// when the body `tenant_id` equals the subject tenant).
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

fn allow_enforcer() -> PolicyEnforcer {
    PolicyEnforcer::new(Arc::new(AllowAuthZ))
}

// ── DB + seller setup (mirror rest_payments.rs) ──────────────────────────────

/// The authenticated caller's tenant — also the provisioned seller and the only
/// tenant the allow fake authorizes. The body `tenant_id` MUST equal this so the
/// handler write-gate's `contains_uuid` membership check passes.
const SUBJECT_TENANT: Uuid = uuid::uuid!("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee");
const SUBJECT_ID: Uuid = uuid::uuid!("11111111-2222-3333-4444-555555555555");

/// Boot a container, migrate on a raw connection, and return a `bss`-search-path
/// `DBProvider` for the service (the provisioning-test idiom).
async fn boot() -> (
    testcontainers_modules::testcontainers::ContainerAsync<Postgres>,
    sea_orm::DatabaseConnection,
    DBProvider<DbError>,
) {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let raw = Database::connect(&url).await.unwrap();
    Migrator::up(&raw, None).await.unwrap();

    let repo_url = format!("{url}?options=-c%20search_path%3Dbss,public");
    let tdb = connect_db(&repo_url, ConnectOpts::default()).await.unwrap();
    let provider = DBProvider::<DbError>::new(tdb);
    (container, raw, provider)
}

/// Provisioned seller ids. `tenant` is fixed to `SUBJECT_TENANT` so the handler
/// authz gate (body `tenant_id` ∈ the caller's scope) passes.
struct Seller {
    tenant: Uuid,
    payer: Uuid,
    cash: Uuid,
    dispute_hold: Uuid,
    period_id: String,
}

fn account(tenant: Uuid, id: Uuid, class: AccountClass, normal: Side) -> AccountRow {
    AccountRow {
        account_id: id,
        tenant_id: tenant,
        legal_entity_id: tenant,
        account_class: class.as_str().to_owned(),
        currency: "USD".to_owned(),
        revenue_stream: None,
        normal_side: normal.as_str().to_owned(),
        may_go_negative: false,
        lifecycle_state: "OPEN".to_owned(),
    }
}

/// Provision the seller (= `SUBJECT_TENANT`): USD@2 scale, an OPEN fiscal period
/// for the current month, and the cash-hold dispute chart accounts (CASH_CLEARING
/// debit, UNALLOCATED credit, DISPUTE_HOLD debit). Mirrors
/// `rest_payments.rs::setup_seller`.
async fn setup_seller(raw: &sea_orm::DatabaseConnection, provider: &DBProvider<DbError>) -> Seller {
    let now = OffsetDateTime::now_utc();
    let s = Seller {
        tenant: SUBJECT_TENANT,
        payer: Uuid::now_v7(),
        cash: Uuid::now_v7(),
        dispute_hold: Uuid::now_v7(),
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
        account(s.tenant, s.cash, AccountClass::CashClearing, Side::Debit),
        account(
            s.tenant,
            Uuid::now_v7(),
            AccountClass::Unallocated,
            Side::Credit,
        ),
        account(
            s.tenant,
            s.dispute_hold,
            AccountClass::DisputeHold,
            Side::Debit,
        ),
    ] {
        reference.insert_account(row).await.unwrap();
    }
    s
}

/// Settle `gross` (fee 0) for `payment_id` directly through the service (so an
/// `opened` cash-hold has CASH_CLEARING funds to move into the hold).
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

// ── Router / context helpers ─────────────────────────────────────────────────

/// Build the dispute router over the `RealDisputeClient` (real DB) with the
/// always-allow enforcer layered as an `Extension`, as `register_rest` does.
fn router_with_db(provider: DBProvider<DbError>) -> Router {
    let state = Arc::new(ApiState {
        client: Arc::new(RealDisputeClient {
            provider: provider.clone(),
        }) as Arc<dyn LedgerClientV1>,
        approval: None,
        dispute_repo: DisputeRepo::new(provider),
    });
    let openapi = toolkit::api::OpenApiRegistryImpl::new();
    router(state, &openapi).layer(axum::Extension(allow_enforcer()))
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

// ── Tests ────────────────────────────────────────────────────────────────────

/// `POST /disputes/{id}/phases` recording an `opened` cash-hold (withheld) after
/// a settle → 201 fresh (the LEDGER chose CASH_HOLD from `funds_at_open` and
/// posted the hold move).
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn record_opened_cash_hold_returns_201() {
    let (_c, raw, provider) = boot().await;
    let s = setup_seller(&raw, &provider).await;
    let ctx = authed_context();

    // Fund CASH_CLEARING so the hold move (CR CASH_CLEARING) does not underflow.
    settle(&provider, &s, "PAY-DSP-REST-1", 1000).await;

    let body = serde_json::json!({
        "tenant_id": s.tenant,
        "payer_tenant_id": s.payer,
        "payment_id": "PAY-DSP-REST-1",
        "phase": "OPENED",
        "funds_at_open": "withheld",
        "disputed_amount_minor": 1000,
        "currency": "USD",
        "scale": 2
    });
    let (status, fresh) = send(
        router_with_db(provider.clone()).layer(axum::Extension(ctx)),
        "POST",
        "/bss-ledger/v1/disputes/DSP-REST-1/phases",
        Some(body),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "fresh opened cash-hold is 201: {fresh}"
    );
    assert_eq!(fresh["replayed"], serde_json::json!(false));

    // The dispute row was seeded with the chosen variant.
    let variant = raw
        .query_one_raw(pg(format!(
            "SELECT variant FROM bss.ledger_dispute \
             WHERE tenant_id='{}' AND dispute_id='DSP-REST-1'",
            s.tenant
        )))
        .await
        .unwrap()
        .map(|r| r.try_get_by_index::<String>(0).unwrap());
    assert_eq!(
        variant,
        Some("CASH_HOLD".to_owned()),
        "the LEDGER recorded variant=CASH_HOLD from funds_at_open=withheld"
    );
}

/// A record whose body `tenant_id` is OUTSIDE the caller's authorized scope (the
/// allow fake echoes only the subject tenant as `owner_tenant_id`) → the handler
/// `(dispute, write)` gate's target-membership assertion denies it → 403, and no
/// dispute is recorded for the foreign tenant.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn record_into_foreign_tenant_is_denied_403() {
    let (_c, raw, provider) = boot().await;
    let s = setup_seller(&raw, &provider).await;
    let ctx = authed_context();

    // Body targets a tenant the caller is NOT authorized for (≠ SUBJECT_TENANT).
    let foreign = Uuid::now_v7();
    let body = serde_json::json!({
        "tenant_id": foreign,
        "payer_tenant_id": s.payer,
        "payment_id": "PAY-FOREIGN",
        "phase": "OPENED",
        "funds_at_open": "withheld",
        "disputed_amount_minor": 1000,
        "currency": "USD",
        "scale": 2
    });
    let (status, problem) = send(
        router_with_db(provider.clone()).layer(axum::Extension(ctx)),
        "POST",
        "/bss-ledger/v1/disputes/DSP-FOREIGN/phases",
        Some(body),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "cross-tenant record must be 403: {problem}"
    );

    // No dispute row was created for the foreign tenant.
    let count = raw
        .query_one_raw(pg(
            "SELECT COUNT(*) FROM bss.ledger_dispute WHERE dispute_id='DSP-FOREIGN'".to_owned(),
        ))
        .await
        .unwrap()
        .map_or(0, |r| r.try_get_by_index::<i64>(0).unwrap());
    assert_eq!(count, 0, "a denied record must leave no dispute row");
}

/// An out-of-order `won` (no `opened` has landed for the dispute) → 202 ACCEPTED
/// with the `dispute-phase-queued` status token (§4.7): the request is durably
/// queued until its `opened` arrives, never rejected. The 202 body carries the
/// `CHARGEBACK` flow + the `dispute_id:cycle:phase` business id (mirrors the
/// allocate 202 `allocation-queued` assertion in `rest_payments.rs`).
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn out_of_order_won_returns_202_queued() {
    let (_c, raw, provider) = boot().await;
    let s = setup_seller(&raw, &provider).await;
    let ctx = authed_context();

    // No `opened` was ever recorded for DSP-Q-REST — so a `won` queues.
    let body = serde_json::json!({
        "tenant_id": s.tenant,
        "payer_tenant_id": s.payer,
        "payment_id": "PAY-Q-REST",
        "phase": "WON",
        "funds_at_open": "withheld",
        "disputed_amount_minor": 1000,
        "currency": "USD",
        "scale": 2
    });
    let (status, queued) = send(
        router_with_db(provider.clone()).layer(axum::Extension(ctx)),
        "POST",
        "/bss-ledger/v1/disputes/DSP-Q-REST/phases",
        Some(body),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::ACCEPTED,
        "an out-of-order won is 202: {queued}"
    );
    assert_eq!(
        queued["status"],
        serde_json::json!("dispute-phase-queued"),
        "the 202 body status token: {queued}"
    );
    assert_eq!(
        queued["flow"],
        serde_json::json!("CHARGEBACK"),
        "the queued handle carries the CHARGEBACK flow"
    );
    assert_eq!(
        queued["business_id"],
        serde_json::json!("DSP-Q-REST:1:WON"),
        "the queued handle's business_id is dispute_id:cycle:phase"
    );

    // Still queued, never posted: no dispute current-state row exists yet.
    let count = raw
        .query_one_raw(pg(format!(
            "SELECT COUNT(*) FROM bss.ledger_dispute \
             WHERE tenant_id='{}' AND dispute_id='DSP-Q-REST'",
            s.tenant
        )))
        .await
        .unwrap()
        .map_or(0, |r| r.try_get_by_index::<i64>(0).unwrap());
    assert_eq!(
        count, 0,
        "a queued won posts nothing — no dispute row until its opened lands"
    );
}
