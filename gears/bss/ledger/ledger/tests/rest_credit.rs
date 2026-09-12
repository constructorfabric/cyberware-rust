//! API-level (router) tests for the credit-application REST surface (Group E):
//! `POST /bss-ledger/v1/credit-applications` (grant | apply, tenant in body).
//!
//! Like `rest_payments.rs`, these drive the router against a REAL testcontainer
//! Postgres so grant / apply run end-to-end through the foundation engine.
//! `LedgerLocalClient::new` is `pub(crate)` and unreachable from this out-of-crate
//! test, so the router's `ApiState` is built over a small in-test
//! `LedgerClientV1` (`RealCreditClient`) whose `post_credit_application` mirrors
//! the production local client verbatim: it matches the `CreditApplication` enum,
//! drives the SAME `pub` `CreditApplicationService` (grant_credit / apply_credit),
//! and maps the `CreditApplicationOutcome` back to the SDK `CreditApplicationApplied`.
//! The non-credit trait methods are `unimplemented!()` (the inverse of
//! `rest_payments.rs`). An always-allow `PolicyEnforcer` fake is layered as an
//! `Extension`, mirroring `register_rest`.
//!
//! Cases: a grant → 201; an apply (fund + open AR first) → 201 with the
//! per-sub-grain debits + per-invoice applications; an over-pool grant → 400
//! `GRANT_EXCEEDS_UNALLOCATED`; an over-open-AR apply → 400 `CREDIT_EXCEEDS_OPEN_AR`;
//! a credit-application whose body `tenant_id` is outside the caller's scope → 403.

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
use bss_ledger::api::rest::credit::{ApiState, router};
use bss_ledger::domain::instant::to_naive_date;
use bss_ledger::domain::model::{AccountRow, CurrencyScaleRow, NewEntry, NewLine};
use bss_ledger::domain::money::DEFAULT_PLAUSIBLE_MAX_MAJOR;
use bss_ledger::domain::payment::settlement::SettlementInput;
use bss_ledger::domain::ports::metrics::NoopLedgerMetrics;
use bss_ledger::infra::events::publisher::LedgerEventPublisher;
use bss_ledger::infra::payment::credit::{ApplyRequest, CreditApplicationService, GrantRequest};
use bss_ledger::infra::payment::settle::SettlementService;
use bss_ledger::infra::posting::service::PostingService;
use bss_ledger::infra::storage::migrations::Migrator;
use bss_ledger::infra::storage::repo::ReferenceRepo;
use bss_ledger_sdk::api::LedgerClientV1;
use bss_ledger_sdk::posting::{
    AllocateOutcome, AllocatePayment, AllocationView, ArInvoiceBalanceView, BalanceView,
    CreditApplication, CreditApplicationApplied, CreditDebitView, EntryView, LineView, ODataQuery,
    Page, PostEntry, PostingRef, SettlePayment, UnallocatedView,
};
use bss_ledger_sdk::{
    AccountClass, AllocationSplit, MappingStatus, ProvisionOutcome, ProvisionRequest, Side,
    SourceDocType,
};
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

// ── The in-test client: delegates the credit method to the real orchestrator ─

/// A `LedgerClientV1` over a real `DBProvider` that drives the SAME `pub`
/// `CreditApplicationService` the in-process `LedgerLocalClient` uses, so the
/// router runs end-to-end against the testcontainer DB. The PEP gate the
/// production client performs internally is exercised at the HANDLER layer here
/// (the allow-all enforcer extension), so the service is called with a plain
/// per-tenant `AccessScope`, mirroring `rest_payments.rs`. Only the credit method
/// is implemented.
struct RealCreditClient {
    provider: DBProvider<DbError>,
}

impl RealCreditClient {
    fn credit_svc(&self) -> CreditApplicationService {
        CreditApplicationService::new(
            self.provider.clone(),
            Arc::new(LedgerEventPublisher::noop()),
            Arc::new(NoopLedgerMetrics),
        )
    }
}

#[async_trait::async_trait]
impl LedgerClientV1 for RealCreditClient {
    async fn return_payment(
        &self,
        _ctx: &toolkit_security::SecurityContext,
        _req: bss_ledger_sdk::ReturnPayment,
    ) -> Result<bss_ledger_sdk::PostingRef, CanonicalError> {
        unimplemented!("not exercised by the credit router tests")
    }

    async fn record_dispute_phase(
        &self,
        _ctx: &toolkit_security::SecurityContext,
        _req: bss_ledger_sdk::RecordDisputePhase,
    ) -> Result<bss_ledger_sdk::DisputeOutcome, CanonicalError> {
        unimplemented!("not exercised by the credit router tests")
    }

    async fn post_credit_application(
        &self,
        ctx: &SecurityContext,
        req: CreditApplication,
    ) -> Result<CreditApplicationApplied, CanonicalError> {
        // Scope: the handler-layer enforcer does the write gate, so (mirroring
        // `rest_payments.rs`) build a plain per-tenant scope for the SQL BOLA
        // filter from the operation's target tenant.
        let scope = AccessScope::for_tenant(req.tenant_id());
        // Dispatch grant vs apply on the enum arm — the SAME mapping the
        // production `local_client::post_credit_application` performs (`scale` is
        // advisory and dropped; the per-line scale resolver is authoritative).
        let outcome = match req {
            CreditApplication::Grant(g) => {
                self.credit_svc()
                    .grant_credit(
                        ctx,
                        &scope,
                        GrantRequest {
                            tenant_id: g.tenant_id,
                            payer_tenant_id: g.payer_tenant_id,
                            credit_application_id: g.credit_application_id,
                            currency: g.currency,
                            amount_minor: g.amount_minor,
                            credit_grant_event_type: g.credit_grant_event_type,
                        },
                    )
                    .await
            }
            CreditApplication::Apply(a) => {
                self.credit_svc()
                    .apply_credit(
                        ctx,
                        &scope,
                        ApplyRequest {
                            tenant_id: a.tenant_id,
                            payer_tenant_id: a.payer_tenant_id,
                            credit_application_id: a.credit_application_id,
                            currency: a.currency,
                            targets: a
                                .targets
                                .into_iter()
                                .map(|s| bss_ledger::domain::payment::precedence::Allocated {
                                    invoice_id: s.invoice_id,
                                    amount_minor: s.amount_minor,
                                })
                                .collect(),
                        },
                    )
                    .await
            }
        }
        .map_err(CanonicalError::from)?;
        // Map the domain outcome to the SDK shape: `debits` → per-sub-grain wallet
        // draw-downs, `applications` ← the validated per-invoice targets. A grant
        // leaves both empty.
        Ok(CreditApplicationApplied {
            posting: outcome.posting,
            debits: outcome
                .debits
                .into_iter()
                .map(|d| CreditDebitView {
                    credit_grant_event_type: d.credit_grant_event_type,
                    amount_minor: d.amount_minor,
                })
                .collect(),
            applications: outcome
                .targets
                .into_iter()
                .map(|t| AllocationSplit {
                    invoice_id: t.invoice_id,
                    amount_minor: t.amount_minor,
                })
                .collect(),
        })
    }

    // ── The non-credit surface is not exercised by these router tests. ──

    async fn settle_payment(
        &self,
        _ctx: &SecurityContext,
        _req: SettlePayment,
    ) -> Result<PostingRef, CanonicalError> {
        unimplemented!("not exercised by the credit router tests")
    }

    async fn allocate_payment(
        &self,
        _ctx: &SecurityContext,
        _req: AllocatePayment,
    ) -> Result<AllocateOutcome, CanonicalError> {
        unimplemented!("not exercised by the credit router tests")
    }

    async fn list_payment_allocations(
        &self,
        _ctx: &SecurityContext,
        _tenant_id: Uuid,
        _payment_id: String,
    ) -> Result<Vec<AllocationView>, CanonicalError> {
        unimplemented!("not exercised by the credit router tests")
    }

    async fn read_unallocated(
        &self,
        _ctx: &SecurityContext,
        _tenant_id: Uuid,
        _payer_tenant_id: Uuid,
        _currency: String,
    ) -> Result<UnallocatedView, CanonicalError> {
        unimplemented!("not exercised by the credit router tests")
    }

    async fn post_balanced_entry(
        &self,
        _ctx: &SecurityContext,
        _entry: PostEntry,
    ) -> Result<PostingRef, CanonicalError> {
        unimplemented!("not exercised by the credit router tests")
    }

    async fn read_account_balance(
        &self,
        _ctx: &SecurityContext,
        _tenant_id: Uuid,
        _account_id: Uuid,
    ) -> Result<Option<i64>, CanonicalError> {
        unimplemented!("not exercised by the credit router tests")
    }

    async fn list_accounts(
        &self,
        _ctx: &SecurityContext,
        _tenant_id: Uuid,
        _query: &ODataQuery,
    ) -> Result<Page<bss_ledger_sdk::AccountInfo>, CanonicalError> {
        unimplemented!("not exercised by the credit router tests")
    }

    async fn get_entry(
        &self,
        _ctx: &SecurityContext,
        _tenant_id: Uuid,
        _entry_id: Uuid,
    ) -> Result<Option<EntryView>, CanonicalError> {
        unimplemented!("not exercised by the credit router tests")
    }

    async fn list_lines(
        &self,
        _ctx: &SecurityContext,
        _tenant_id: Uuid,
        _query: &ODataQuery,
    ) -> Result<Page<LineView>, CanonicalError> {
        unimplemented!("not exercised by the credit router tests")
    }

    async fn list_balances(
        &self,
        _ctx: &SecurityContext,
        _tenant_id: Uuid,
        _query: &ODataQuery,
    ) -> Result<Page<BalanceView>, CanonicalError> {
        unimplemented!("not exercised by the credit router tests")
    }

    async fn list_ar_invoice_balances(
        &self,
        _ctx: &SecurityContext,
        _tenant_id: Uuid,
        _payer_tenant_id: Option<Uuid>,
    ) -> Result<Vec<ArInvoiceBalanceView>, CanonicalError> {
        unimplemented!("not exercised by the credit router tests")
    }

    async fn provision(
        &self,
        _ctx: &SecurityContext,
        _req: ProvisionRequest,
    ) -> Result<ProvisionOutcome, CanonicalError> {
        unimplemented!("not exercised by the credit router tests")
    }

    async fn close_period(
        &self,
        _ctx: &SecurityContext,
        _tenant_id: Uuid,
        _period_id: String,
    ) -> Result<bss_ledger_sdk::CloseOutcome, CanonicalError> {
        unimplemented!("not exercised by the credit router tests")
    }

    async fn trigger_recognition_run(
        &self,
        _ctx: &SecurityContext,
        _req: bss_ledger_sdk::TriggerRecognitionRun,
    ) -> Result<bss_ledger_sdk::RecognitionRunOutcome, CanonicalError> {
        unimplemented!("not exercised by the credit router tests")
    }

    async fn list_revenue_disaggregation(
        &self,
        _ctx: &SecurityContext,
        _query: bss_ledger_sdk::RevenueDisaggregationQuery,
    ) -> Result<bss_ledger_sdk::RevenueDisaggregation, CanonicalError> {
        unimplemented!("not exercised by the credit router tests")
    }

    async fn change_recognition_schedule(
        &self,
        _ctx: &SecurityContext,
        _cmd: bss_ledger_sdk::ChangeRecognitionSchedule,
    ) -> Result<bss_ledger_sdk::ScheduleChangeRef, CanonicalError> {
        unimplemented!("not exercised by the credit router tests")
    }

    async fn get_recognition_schedule(
        &self,
        _ctx: &SecurityContext,
        _tenant_id: Uuid,
        _schedule_id: String,
    ) -> Result<Option<bss_ledger_sdk::RecognitionScheduleView>, CanonicalError> {
        unimplemented!("not exercised by the credit router tests")
    }

    async fn list_recognition_schedules(
        &self,
        _ctx: &SecurityContext,
        _tenant_id: Uuid,
        _invoice_id: Option<String>,
        _revenue_stream: Option<String>,
    ) -> Result<bss_ledger_sdk::RecognitionScheduleList, CanonicalError> {
        unimplemented!("not exercised by the credit router tests")
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
/// `DBProvider` for the services (the provisioning-test idiom).
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
    unallocated: Uuid,
    psp_fee: Uuid,
    ar: Uuid,
    reusable_credit: Uuid,
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
/// for the current month, the four payment-flow chart accounts, and a stream-less
/// REUSABLE_CREDIT credit account. Mirrors `rest_payments.rs::setup_seller`.
async fn setup_seller(raw: &sea_orm::DatabaseConnection, provider: &DBProvider<DbError>) -> Seller {
    let now = OffsetDateTime::now_utc();
    let s = Seller {
        tenant: SUBJECT_TENANT,
        payer: Uuid::now_v7(),
        cash: Uuid::now_v7(),
        unallocated: Uuid::now_v7(),
        psp_fee: Uuid::now_v7(),
        ar: Uuid::now_v7(),
        reusable_credit: Uuid::now_v7(),
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
            s.unallocated,
            AccountClass::Unallocated,
            Side::Credit,
        ),
        account(
            s.tenant,
            s.psp_fee,
            AccountClass::PspFeeExpense,
            Side::Debit,
        ),
        account(s.tenant, s.ar, AccountClass::Ar, Side::Debit),
        account(
            s.tenant,
            s.reusable_credit,
            AccountClass::ReusableCredit,
            Side::Credit,
        ),
    ] {
        reference.insert_account(row).await.unwrap();
    }
    s
}

fn ar_line(s: &Seller, invoice_id: &str, amount: i64) -> NewLine {
    NewLine {
        line_id: Uuid::now_v7(),
        payer_tenant_id: s.payer,
        seller_tenant_id: Some(s.tenant),
        resource_tenant_id: None,
        account_id: s.ar,
        account_class: AccountClass::Ar,
        gl_code: None,
        side: Side::Debit,
        amount_minor: amount,
        currency: "USD".to_owned(),
        currency_scale: 2,
        invoice_id: Some(invoice_id.to_owned()),
        due_date: Some(NaiveDate::from_ymd_opt(2026, 12, 1).unwrap()),
        revenue_stream: None,
        mapping_status: MappingStatus::Resolved,
        functional_amount_minor: None,
        functional_currency: None,
        tax_jurisdiction: None,
        tax_filing_period: None,
        tax_rate_ref: None,
        legal_entity_id: None,
        invoice_item_ref: None,
        sku_or_plan_ref: None,
        price_id: None,
        pricing_snapshot_ref: None,
        po_allocation_group: None,
        credit_grant_event_type: None,
        ar_status: None,
    }
}

fn psp_credit_line(s: &Seller, amount: i64) -> NewLine {
    NewLine {
        line_id: Uuid::now_v7(),
        payer_tenant_id: s.payer,
        seller_tenant_id: Some(s.tenant),
        resource_tenant_id: None,
        account_id: s.psp_fee,
        account_class: AccountClass::PspFeeExpense,
        gl_code: None,
        side: Side::Credit,
        amount_minor: amount,
        currency: "USD".to_owned(),
        currency_scale: 2,
        invoice_id: None,
        due_date: None,
        revenue_stream: None,
        mapping_status: MappingStatus::Resolved,
        functional_amount_minor: None,
        functional_currency: None,
        tax_jurisdiction: None,
        tax_filing_period: None,
        tax_rate_ref: None,
        legal_entity_id: None,
        invoice_item_ref: None,
        sku_or_plan_ref: None,
        price_id: None,
        pricing_snapshot_ref: None,
        po_allocation_group: None,
        credit_grant_event_type: None,
        ar_status: None,
    }
}

/// Seed an OPEN AR invoice by posting `DR AR (invoice_id) / CR PSP_FEE_EXPENSE`
/// directly through the engine (mirrors `rest_payments.rs::seed_ar_invoice`).
async fn seed_ar_invoice(
    provider: &DBProvider<DbError>,
    s: &Seller,
    invoice_id: &str,
    amount: i64,
    posted_at: OffsetDateTime,
) {
    let posting = PostingService::new(provider.clone(), Arc::new(LedgerEventPublisher::noop()));
    let ctx = SecurityContext::anonymous();
    let scope = AccessScope::for_tenant(s.tenant);
    let entry = NewEntry {
        entry_id: Uuid::now_v7(),
        tenant_id: s.tenant,
        legal_entity_id: s.tenant,
        period_id: s.period_id.clone(),
        entry_currency: "USD".to_owned(),
        source_doc_type: SourceDocType::InvoicePost,
        source_business_id: invoice_id.to_owned(),
        reverses_entry_id: None,
        reverses_period_id: None,
        posted_at_utc: posted_at,
        effective_at: to_naive_date(posted_at),
        origin: "SYSTEM".to_owned(),
        posted_by_actor_id: s.tenant,
        correlation_id: Uuid::now_v7(),
        rounding_evidence: serde_json::Value::Null,
        rate_snapshot_ref: None,
    };
    let lines = vec![ar_line(s, invoice_id, amount), psp_credit_line(s, amount)];
    posting
        .post(&ctx, &scope, entry, lines, None)
        .await
        .expect("seed AR invoice post must succeed");
}

/// Settle a fee-less payment of `gross` to fund the payer's unallocated pool
/// (the grant cap basis / wallet funding source).
async fn fund_pool(provider: &DBProvider<DbError>, s: &Seller, payment_id: &str, gross: i64) {
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
    .expect("settle to fund the pool");
}

/// Grant `amount` into the named wallet sub-grain directly through the service
/// (fund a wallet for the apply tests).
async fn grant_wallet(
    provider: &DBProvider<DbError>,
    s: &Seller,
    credit_application_id: &str,
    event_type: &str,
    amount: i64,
) {
    CreditApplicationService::new(
        provider.clone(),
        Arc::new(LedgerEventPublisher::noop()),
        Arc::new(NoopLedgerMetrics),
    )
    .grant_credit(
        &SecurityContext::anonymous(),
        &AccessScope::for_tenant(s.tenant),
        GrantRequest {
            tenant_id: s.tenant,
            payer_tenant_id: s.payer,
            credit_application_id: credit_application_id.to_owned(),
            currency: "USD".to_owned(),
            amount_minor: amount,
            credit_grant_event_type: event_type.to_owned(),
        },
    )
    .await
    .expect("grant to fund the wallet");
}

// ── Router / context helpers ─────────────────────────────────────────────────

/// Build the credit router over the `RealCreditClient` (real DB) with the
/// always-allow enforcer layered as an `Extension`, as `register_rest` does.
fn router_with_db(provider: DBProvider<DbError>) -> Router {
    let state = Arc::new(ApiState {
        client: Arc::new(RealCreditClient { provider }) as Arc<dyn LedgerClientV1>,
        approval: None,
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

fn grant_body(
    s: &Seller,
    credit_application_id: &str,
    event_type: &str,
    amount: i64,
) -> serde_json::Value {
    serde_json::json!({
        "kind": "grant",
        "tenant_id": s.tenant,
        "payer_tenant_id": s.payer,
        "credit_application_id": credit_application_id,
        "currency": "USD",
        "scale": 2,
        "amount_minor": amount,
        "credit_grant_event_type": event_type
    })
}

fn apply_body(
    s: &Seller,
    credit_application_id: &str,
    targets: serde_json::Value,
) -> serde_json::Value {
    serde_json::json!({
        "kind": "apply",
        "tenant_id": s.tenant,
        "payer_tenant_id": s.payer,
        "credit_application_id": credit_application_id,
        "currency": "USD",
        "scale": 2,
        "targets": targets
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

// ── Tests ────────────────────────────────────────────────────────────────────

/// `POST /credit-applications` with `kind=grant` after a settle → 201, the
/// `replayed=false` posting and (for a grant) empty debits/applications.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn grant_returns_201() {
    let (_c, raw, provider) = boot().await;
    let s = setup_seller(&raw, &provider).await;
    let ctx = authed_context();

    fund_pool(&provider, &s, "PAY-G-201", 1000).await;

    let (status, body) = send(
        router_with_db(provider.clone()).layer(axum::Extension(ctx)),
        "POST",
        "/bss-ledger/v1/credit-applications",
        Some(grant_body(&s, "CR-G-201", "promo", 600)),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "grant is 201: {body}");
    assert_eq!(body["replayed"], serde_json::json!(false));
    assert_eq!(
        body["debits"].as_array().map(Vec::len),
        Some(0),
        "a grant moves no wallet debits"
    );
    assert_eq!(
        body["applications"].as_array().map(Vec::len),
        Some(0),
        "a grant pays no receivables"
    );
}

/// `POST /credit-applications` with `kind=apply` after funding a wallet + an open
/// AR → 201 with the per-sub-grain debits and the per-invoice applications.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn apply_returns_201() {
    let (_c, raw, provider) = boot().await;
    let s = setup_seller(&raw, &provider).await;
    let ctx = authed_context();

    // Fund a 500 "promo" wallet and seed an open AR invoice of 500.
    fund_pool(&provider, &s, "PAY-A-201", 1000).await;
    grant_wallet(&provider, &s, "CR-A-201-GRANT", "promo", 500).await;
    seed_ar_invoice(
        &provider,
        &s,
        "inv-1",
        500,
        OffsetDateTime::now_utc() - time::Duration::hours(1),
    )
    .await;

    let (status, body) = send(
        router_with_db(provider.clone()).layer(axum::Extension(ctx)),
        "POST",
        "/bss-ledger/v1/credit-applications",
        Some(apply_body(
            &s,
            "CR-A-201",
            serde_json::json!([{ "invoice_id": "inv-1", "amount_minor": 500 }]),
        )),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "apply is 201: {body}");
    assert_eq!(body["replayed"], serde_json::json!(false));

    // One debit (promo 500) and one application (inv-1 500).
    let debits = body["debits"].as_array().expect("debits array");
    assert_eq!(debits.len(), 1, "one sub-grain drawn");
    assert_eq!(
        debits[0]["credit_grant_event_type"],
        serde_json::json!("promo")
    );
    assert_eq!(debits[0]["amount_minor"], serde_json::json!(500));
    let applications = body["applications"].as_array().expect("applications array");
    assert_eq!(applications.len(), 1, "one receivable paid");
    assert_eq!(applications[0]["invoice_id"], serde_json::json!("inv-1"));
    assert_eq!(applications[0]["amount_minor"], serde_json::json!(500));
}

/// A grant whose amount exceeds the payer's live unallocated pool → 409
/// `GRANT_EXCEEDS_UNALLOCATED` (the `DomainError::GrantExceedsUnallocated` →
/// `aborted` mapping — an exceeded cap is a conflict, not a bad request).
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn grant_over_unallocated_returns_409_with_code() {
    let (_c, raw, provider) = boot().await;
    let s = setup_seller(&raw, &provider).await;
    let ctx = authed_context();

    // Pool holds only 100; granting 500 exceeds it.
    fund_pool(&provider, &s, "PAY-G-400", 100).await;

    let (status, problem) = send(
        router_with_db(provider.clone()).layer(axum::Extension(ctx)),
        "POST",
        "/bss-ledger/v1/credit-applications",
        Some(grant_body(&s, "CR-G-400", "promo", 500)),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "over-pool grant is 409: {problem}"
    );
    assert!(
        problem.to_string().contains("GRANT_EXCEEDS_UNALLOCATED"),
        "expected GRANT_EXCEEDS_UNALLOCATED, got {problem}"
    );
}

/// An apply whose targets exceed open AR → 409 `CREDIT_EXCEEDS_OPEN_AR` (the
/// `DomainError::CreditExceedsOpenAr` → `aborted` mapping, same 409 family
/// as the grant rejection).
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn apply_over_open_ar_returns_409_with_code() {
    let (_c, raw, provider) = boot().await;
    let s = setup_seller(&raw, &provider).await;
    let ctx = authed_context();

    // Ample wallet (1000), but the open AR invoice is only 300.
    fund_pool(&provider, &s, "PAY-A-400", 1000).await;
    grant_wallet(&provider, &s, "CR-A-400-GRANT", "promo", 1000).await;
    seed_ar_invoice(
        &provider,
        &s,
        "inv-1",
        300,
        OffsetDateTime::now_utc() - time::Duration::hours(1),
    )
    .await;

    let (status, problem) = send(
        router_with_db(provider.clone()).layer(axum::Extension(ctx)),
        "POST",
        "/bss-ledger/v1/credit-applications",
        Some(apply_body(
            &s,
            "CR-A-400",
            serde_json::json!([{ "invoice_id": "inv-1", "amount_minor": 500 }]),
        )),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "over-open-AR apply is 409: {problem}"
    );
    assert!(
        problem.to_string().contains("CREDIT_EXCEEDS_OPEN_AR"),
        "expected CREDIT_EXCEEDS_OPEN_AR, got {problem}"
    );
}

/// A credit-application whose body `tenant_id` is OUTSIDE the caller's authorized
/// scope (the allow fake echoes only the subject tenant as `owner_tenant_id`) →
/// the handler `(credit_application, write)` gate's target-membership assertion
/// denies it → 403. Mirrors `rest_payments.rs::settle_into_foreign_tenant_is
/// _denied_403`.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn cross_tenant_target_is_forbidden_403() {
    let (_c, raw, provider) = boot().await;
    let s = setup_seller(&raw, &provider).await;
    let ctx = authed_context();

    // Body targets a tenant the caller is NOT authorized for (≠ SUBJECT_TENANT).
    let foreign = Uuid::now_v7();
    let body = serde_json::json!({
        "kind": "grant",
        "tenant_id": foreign,
        "payer_tenant_id": s.payer,
        "credit_application_id": "CR-FOREIGN",
        "currency": "USD",
        "scale": 2,
        "amount_minor": 100,
        "credit_grant_event_type": "promo"
    });
    let (status, problem) = send(
        router_with_db(provider.clone()).layer(axum::Extension(ctx)),
        "POST",
        "/bss-ledger/v1/credit-applications",
        Some(body),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "cross-tenant credit-application must be 403: {problem}"
    );

    // No wallet sub-grain was created for the foreign tenant.
    let foreign_subgrain = raw
        .query_one_raw(pg(format!(
            "SELECT balance_minor FROM bss.ledger_reusable_credit_subbalance \
             WHERE tenant_id='{foreign}'"
        )))
        .await
        .unwrap();
    assert!(
        foreign_subgrain.is_none(),
        "a denied credit-application must leave no wallet sub-grain"
    );
}
