//! API-level (router) tests for the payment REST surface (Group E):
//! `POST /payments` (settle, tenant in body), `POST /payments/{id}/allocations`
//! (allocate), `GET /payments/{id}/allocations` (list), and
//! `GET /balances/unallocated` (read the payer's pool).
//!
//! Unlike `rest_journal_entries.rs` (pure stubs, no DB), these drive the router
//! against a REAL testcontainer Postgres so settle / allocate run end-to-end
//! through the foundation engine. `LedgerLocalClient::new` is `pub(crate)` and
//! unreachable from this out-of-crate test, so the router's `ApiState` is built
//! over a small in-test `LedgerClientV1` (`RealPaymentClient`) that delegates the
//! four payment methods to the SAME `pub` orchestrators the local client uses
//! (`SettlementService` / `AllocationService` / `PaymentRepo`) — the seam the
//! foundation already proved in `postgres_payments.rs`. The non-payment trait
//! methods are `unimplemented!()` (this router exercises only the payment
//! endpoints). An always-allow `PolicyEnforcer` fake (no PDP) is layered as an
//! `Extension`, mirroring the production `register_rest`.
//!
//! Cases: settle 1000/30 → 201, re-settle → 200 (replay); allocate after a
//! settle + an open AR → 201 with computed splits; an over-cap allocate → 400
//! `ALLOCATION_EXCEEDS_SETTLED`; a currency-mismatched allocate → 400
//! `ALLOCATION_CURRENCY_MISMATCH`; an over-open caller-computed split (Mode B)
//! → 400 `ALLOCATION_SPLIT_INVALID`; list allocations → the recorded shape;
//! read unallocated → the pool balance. (The >500-candidate
//! `ALLOCATION_TOO_LARGE` HTTP case is deferred — its wire code is the same 400
//! `field_violation` shape, proven at the service layer.)

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
use bss_ledger::api::rest::payments::{ApiState, router};
use bss_ledger::domain::instant::to_naive_date;
use bss_ledger::domain::model::{AccountRow, CurrencyScaleRow, NewEntry, NewLine};
use bss_ledger::domain::money::DEFAULT_PLAUSIBLE_MAX_MAJOR;
use bss_ledger::domain::payment::precedence::DEFAULT_PRECEDENCE_POLICY;
use bss_ledger::domain::payment::settlement::SettlementInput;
use bss_ledger::domain::ports::metrics::NoopLedgerMetrics;
use bss_ledger::infra::events::publisher::LedgerEventPublisher;
use bss_ledger::infra::payment::allocate::{AllocateRequest, AllocationService};
use bss_ledger::infra::payment::settle::SettlementService;
use bss_ledger::infra::payment::settlement_return::SettlementReturnService;
use bss_ledger::infra::posting::service::PostingService;
use bss_ledger::infra::storage::migrations::Migrator;
use bss_ledger::infra::storage::repo::{PaymentRepo, ReferenceRepo};
use bss_ledger_sdk::api::LedgerClientV1;
use bss_ledger_sdk::posting::{
    AllocateOutcome, AllocatePayment, AllocationApplied, AllocationView, ArInvoiceBalanceView,
    BalanceView, EntryView, LineView, ODataQuery, Page, PostEntry, PostingRef, SettlePayment,
    UnallocatedView,
};
use bss_ledger_sdk::{
    AccountClass, MappingStatus, ProvisionOutcome, ProvisionRequest, Side, SourceDocType,
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

// ── The in-test client: delegates the payment methods to the real orchestrators ─

/// A `LedgerClientV1` over a real `DBProvider` that drives the SAME `pub`
/// settle / allocate / read services the in-process `LedgerLocalClient` uses, so
/// the router runs end-to-end against the testcontainer DB. The PEP gate the
/// production client performs internally is exercised at the HANDLER layer here
/// (the allow-all enforcer extension), so the services are called with a plain
/// per-tenant `AccessScope` (the SQL-level BOLA filter), mirroring
/// `postgres_payments.rs`. Only the payment methods are implemented.
struct RealPaymentClient {
    provider: DBProvider<DbError>,
}

impl RealPaymentClient {
    fn settle_svc(&self) -> SettlementService {
        SettlementService::new(
            self.provider.clone(),
            Arc::new(LedgerEventPublisher::noop()),
            Arc::new(NoopLedgerMetrics),
        )
    }

    fn allocate_svc(&self) -> AllocationService {
        AllocationService::new(
            self.provider.clone(),
            Arc::new(LedgerEventPublisher::noop()),
            Arc::new(NoopLedgerMetrics),
        )
    }

    fn return_svc(&self) -> SettlementReturnService {
        SettlementReturnService::new(
            self.provider.clone(),
            Arc::new(LedgerEventPublisher::noop()),
            Arc::new(NoopLedgerMetrics),
        )
    }
}

#[async_trait::async_trait]
impl LedgerClientV1 for RealPaymentClient {
    async fn post_credit_application(
        &self,
        _ctx: &SecurityContext,
        _req: bss_ledger_sdk::CreditApplication,
    ) -> Result<bss_ledger_sdk::CreditApplicationApplied, CanonicalError> {
        unimplemented!("not exercised by the payment router tests")
    }

    async fn settle_payment(
        &self,
        ctx: &SecurityContext,
        req: SettlePayment,
    ) -> Result<PostingRef, CanonicalError> {
        let scope = AccessScope::for_tenant(req.tenant_id);
        let input = SettlementInput {
            tenant_id: req.tenant_id,
            payer_tenant_id: req.payer_tenant_id,
            payment_id: req.payment_id,
            gross_minor: req.gross_minor,
            fee_minor: req.fee_minor,
            currency: req.currency,
            effective_at: req.effective_at,
        };
        self.settle_svc()
            .settle(ctx, &scope, input)
            .await
            .map_err(CanonicalError::from)
    }

    async fn return_payment(
        &self,
        ctx: &SecurityContext,
        req: bss_ledger_sdk::ReturnPayment,
    ) -> Result<PostingRef, CanonicalError> {
        let scope = AccessScope::for_tenant(req.tenant_id);
        let input = bss_ledger::domain::payment::settlement_return::SettlementReturnInput {
            tenant_id: req.tenant_id,
            payer_tenant_id: req.payer_tenant_id,
            payment_id: req.payment_id,
            psp_return_id: req.psp_return_id,
            amount_minor: req.amount_minor,
            currency: req.currency,
            effective_at: req.effective_at,
        };
        self.return_svc()
            .return_settlement(ctx, &scope, input)
            .await
            .map_err(CanonicalError::from)
    }

    async fn record_dispute_phase(
        &self,
        _ctx: &SecurityContext,
        _req: bss_ledger_sdk::RecordDisputePhase,
    ) -> Result<bss_ledger_sdk::DisputeOutcome, CanonicalError> {
        unimplemented!("not exercised by the payment router tests")
    }

    async fn allocate_payment(
        &self,
        ctx: &SecurityContext,
        req: AllocatePayment,
    ) -> Result<AllocateOutcome, CanonicalError> {
        let scope = AccessScope::for_tenant(req.tenant_id);
        let currency = req.currency.clone();
        let caller_splits = req.splits.map(|splits| {
            splits
                .into_iter()
                .map(|s| bss_ledger::domain::payment::precedence::Allocated {
                    invoice_id: s.invoice_id,
                    amount_minor: s.amount_minor,
                })
                .collect()
        });
        let request = AllocateRequest {
            tenant_id: req.tenant_id,
            payer_tenant_id: req.payer_tenant_id,
            payment_id: req.payment_id,
            allocation_id: req.allocation_id,
            lump_minor: req.lump_minor,
            currency: req.currency,
            hint_invoice_id: req.hint_invoice_id,
            caller_splits,
        };
        // Map the service outcome onto the SDK outcome exactly as the production
        // client does: `Applied` synthesizes the per-invoice views (request
        // currency + apply instant + the ref the service stamped — a precedence
        // policy id, or `caller-split.v1` for a Mode B split); `Queued` carries
        // the queue key + `queued_at` (the 202 surface).
        match self
            .allocate_svc()
            .allocate(ctx, &scope, request)
            .await
            .map_err(CanonicalError::from)?
        {
            bss_ledger::infra::payment::allocate::AllocationOutcome::Applied(applied) => {
                let policy_ref = applied.policy_ref;
                let allocations = applied
                    .splits
                    .into_iter()
                    .map(|s| AllocationView {
                        invoice_id: s.invoice_id,
                        amount_minor: s.amount_minor,
                        currency: currency.clone(),
                        allocated_at_utc: OffsetDateTime::now_utc(),
                        precedence_policy_ref: policy_ref.clone(),
                    })
                    .collect();
                Ok(AllocateOutcome::Applied(AllocationApplied {
                    posting: applied.posting,
                    allocations,
                }))
            }
            bss_ledger::infra::payment::allocate::AllocationOutcome::Queued(queued) => {
                Ok(AllocateOutcome::Queued(bss_ledger_sdk::AllocationQueued {
                    flow: queued.flow,
                    business_id: queued.business_id,
                    queued_at: queued.queued_at,
                }))
            }
        }
    }

    async fn list_payment_allocations(
        &self,
        _ctx: &SecurityContext,
        tenant_id: Uuid,
        payment_id: String,
    ) -> Result<Vec<AllocationView>, CanonicalError> {
        let scope = AccessScope::for_tenant(tenant_id);
        let rows = PaymentRepo::new(self.provider.clone())
            .list_payment_allocations(&scope, tenant_id, &payment_id)
            .await
            .map_err(|e| CanonicalError::internal(format!("list allocations: {e}")).create())?;
        Ok(rows
            .into_iter()
            .map(|m| AllocationView {
                invoice_id: m.invoice_id,
                amount_minor: m.amount_minor,
                currency: m.currency,
                allocated_at_utc: m.allocated_at_utc,
                precedence_policy_ref: m.precedence_policy_ref,
            })
            .collect())
    }

    async fn read_unallocated(
        &self,
        _ctx: &SecurityContext,
        tenant_id: Uuid,
        payer_tenant_id: Uuid,
        currency: String,
    ) -> Result<UnallocatedView, CanonicalError> {
        let scope = AccessScope::for_tenant(tenant_id);
        let balance_minor = PaymentRepo::new(self.provider.clone())
            .read_unallocated(&scope, tenant_id, payer_tenant_id, &currency)
            .await
            .map_err(|e| CanonicalError::internal(format!("read unallocated: {e}")).create())?;
        Ok(UnallocatedView {
            payer_tenant_id,
            currency,
            balance_minor,
        })
    }

    // ── The non-payment surface is not exercised by these router tests. ──

    async fn post_balanced_entry(
        &self,
        _ctx: &SecurityContext,
        _entry: PostEntry,
    ) -> Result<PostingRef, CanonicalError> {
        unimplemented!("not exercised by the payment router tests")
    }

    async fn read_account_balance(
        &self,
        _ctx: &SecurityContext,
        _tenant_id: Uuid,
        _account_id: Uuid,
    ) -> Result<Option<i64>, CanonicalError> {
        unimplemented!("not exercised by the payment router tests")
    }

    async fn list_accounts(
        &self,
        _ctx: &SecurityContext,
        _tenant_id: Uuid,
        _query: &ODataQuery,
    ) -> Result<Page<bss_ledger_sdk::AccountInfo>, CanonicalError> {
        unimplemented!("not exercised by the payment router tests")
    }

    async fn get_entry(
        &self,
        _ctx: &SecurityContext,
        _tenant_id: Uuid,
        _entry_id: Uuid,
    ) -> Result<Option<EntryView>, CanonicalError> {
        unimplemented!("not exercised by the payment router tests")
    }

    async fn list_lines(
        &self,
        _ctx: &SecurityContext,
        _tenant_id: Uuid,
        _query: &ODataQuery,
    ) -> Result<Page<LineView>, CanonicalError> {
        unimplemented!("not exercised by the payment router tests")
    }

    async fn list_balances(
        &self,
        _ctx: &SecurityContext,
        _tenant_id: Uuid,
        _query: &ODataQuery,
    ) -> Result<Page<BalanceView>, CanonicalError> {
        unimplemented!("not exercised by the payment router tests")
    }

    async fn list_ar_invoice_balances(
        &self,
        _ctx: &SecurityContext,
        _tenant_id: Uuid,
        _payer_tenant_id: Option<Uuid>,
    ) -> Result<Vec<ArInvoiceBalanceView>, CanonicalError> {
        unimplemented!("not exercised by the payment router tests")
    }

    async fn provision(
        &self,
        _ctx: &SecurityContext,
        _req: ProvisionRequest,
    ) -> Result<ProvisionOutcome, CanonicalError> {
        unimplemented!("not exercised by the payment router tests")
    }

    async fn close_period(
        &self,
        _ctx: &SecurityContext,
        _tenant_id: Uuid,
        _period_id: String,
    ) -> Result<bss_ledger_sdk::CloseOutcome, CanonicalError> {
        unimplemented!("not exercised by the payment router tests")
    }

    async fn trigger_recognition_run(
        &self,
        _ctx: &SecurityContext,
        _req: bss_ledger_sdk::TriggerRecognitionRun,
    ) -> Result<bss_ledger_sdk::RecognitionRunOutcome, CanonicalError> {
        unimplemented!("not exercised by the payment router tests")
    }

    async fn list_revenue_disaggregation(
        &self,
        _ctx: &SecurityContext,
        _query: bss_ledger_sdk::RevenueDisaggregationQuery,
    ) -> Result<bss_ledger_sdk::RevenueDisaggregation, CanonicalError> {
        unimplemented!("not exercised by the payment router tests")
    }

    async fn change_recognition_schedule(
        &self,
        _ctx: &SecurityContext,
        _cmd: bss_ledger_sdk::ChangeRecognitionSchedule,
    ) -> Result<bss_ledger_sdk::ScheduleChangeRef, CanonicalError> {
        unimplemented!("not exercised by the payment router tests")
    }

    async fn get_recognition_schedule(
        &self,
        _ctx: &SecurityContext,
        _tenant_id: Uuid,
        _schedule_id: String,
    ) -> Result<Option<bss_ledger_sdk::RecognitionScheduleView>, CanonicalError> {
        unimplemented!("not exercised by the payment router tests")
    }

    async fn list_recognition_schedules(
        &self,
        _ctx: &SecurityContext,
        _tenant_id: Uuid,
        _invoice_id: Option<String>,
        _revenue_stream: Option<String>,
    ) -> Result<bss_ledger_sdk::RecognitionScheduleList, CanonicalError> {
        unimplemented!("not exercised by the payment router tests")
    }
}

// ── Authz fakes (mirror rest_journal_entries.rs) ─────────────────────────────

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

// ── DB + seller setup (mirror postgres_payments.rs) ──────────────────────────

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
/// for the current month, and the four payment-flow chart accounts. Mirrors
/// `postgres_payments.rs::setup_seller`.
async fn setup_seller(raw: &sea_orm::DatabaseConnection, provider: &DBProvider<DbError>) -> Seller {
    let now = OffsetDateTime::now_utc();
    let s = Seller {
        tenant: SUBJECT_TENANT,
        payer: Uuid::now_v7(),
        cash: Uuid::now_v7(),
        unallocated: Uuid::now_v7(),
        psp_fee: Uuid::now_v7(),
        ar: Uuid::now_v7(),
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
/// directly through the engine (mirrors `postgres_payments.rs::seed_ar_invoice`).
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

// ── Router / context helpers ─────────────────────────────────────────────────

/// Build the payment router over the `RealPaymentClient` (real DB) with the
/// always-allow enforcer layered as an `Extension`, as `register_rest` does.
fn router_with_db(provider: DBProvider<DbError>) -> Router {
    let state = Arc::new(ApiState {
        client: Arc::new(RealPaymentClient {
            provider: provider.clone(),
        }) as Arc<dyn LedgerClientV1>,
        payment_repo: PaymentRepo::new(provider),
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

fn settle_body(s: &Seller, payment_id: &str, gross: i64, fee: i64) -> serde_json::Value {
    serde_json::json!({
        "tenant_id": s.tenant,
        "payer_tenant_id": s.payer,
        "payment_id": payment_id,
        "gross_minor": gross,
        "fee_minor": fee,
        "currency": "USD",
        "scale": 2
    })
}

fn allocate_body(s: &Seller, lump: i64, currency: &str) -> serde_json::Value {
    serde_json::json!({
        "tenant_id": s.tenant,
        "payer_tenant_id": s.payer,
        "allocation_id": Uuid::now_v7(),
        "lump_minor": lump,
        "currency": currency,
        "scale": 2
    })
}

fn return_body(s: &Seller, psp_return_id: &str, amount: i64) -> serde_json::Value {
    serde_json::json!({
        "tenant_id": s.tenant,
        "payer_tenant_id": s.payer,
        "psp_return_id": psp_return_id,
        "amount_minor": amount,
        "currency": "USD",
        "scale": 2
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

/// `time::serde::rfc3339` keeps the offset a caller wrote and range-checks only
/// the local date-time. The UTC form of this `effective_at` is year 10000, which
/// `time` cannot hold, so before `domain::instant::rfc3339` the body parsed and
/// `overwrite_header` → `to_naive_date` panicked the posting handler on its first
/// line. The boundary answers the canonical JSON 400 instead.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_receipt_instant_with_no_utc_form_is_a_400_not_a_panic() {
    let (_c, raw, provider) = boot().await;
    let s = setup_seller(&raw, &provider).await;
    let mut body = settle_body(&s, "PAY-UTC-1", 1000, 30);
    body["effective_at"] = serde_json::json!("9999-12-31T23:00:00-05:00");

    let (status, problem) = send(
        router_with_db(provider).layer(axum::Extension(authed_context())),
        "POST",
        "/bss-ledger/v1/payments",
        Some(body),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{problem}");
    assert!(
        problem.to_string().contains("invalid_json_body")
            && problem.to_string().contains("no UTC representation"),
        "the canonical rejection carries the machine code and the reason: {problem}"
    );
}

/// `POST /payments` settles 1000/30 → 201 fresh; re-settling the SAME
/// `payment_id` replays → 200 with the prior posting reference.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn settle_payment_returns_201_then_200_on_replay() {
    let (_c, raw, provider) = boot().await;
    let s = setup_seller(&raw, &provider).await;
    let ctx = authed_context();

    let (status, fresh) = send(
        router_with_db(provider.clone()).layer(axum::Extension(ctx.clone())),
        "POST",
        "/bss-ledger/v1/payments",
        Some(settle_body(&s, "PAY-REST-1", 1000, 30)),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "fresh settle is 201: {fresh}");
    assert_eq!(fresh["replayed"], serde_json::json!(false));
    let entry_id = fresh["entry_id"].clone();

    // Re-settle the same payment ⇒ idempotent replay ⇒ 200 with the prior ref.
    let (status, replay) = send(
        router_with_db(provider.clone()).layer(axum::Extension(ctx)),
        "POST",
        "/bss-ledger/v1/payments",
        Some(settle_body(&s, "PAY-REST-1", 1000, 30)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "replay is 200: {replay}");
    assert_eq!(replay["replayed"], serde_json::json!(true));
    assert_eq!(replay["entry_id"], entry_id, "replay returns the prior id");

    // The pool holds the whole gross.
    let repo = PaymentRepo::new(provider.clone());
    assert_eq!(
        repo.read_unallocated(&AccessScope::for_tenant(s.tenant), s.tenant, s.payer, "USD")
            .await
            .unwrap(),
        1000
    );
}

/// `POST /payments/{id}/allocations` after a settle + two open AR invoices →
/// 201 with the oldest-first computed splits (INV-A fills, INV-B partial).
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn allocate_payment_returns_201_with_computed_splits() {
    let (_c, raw, provider) = boot().await;
    let s = setup_seller(&raw, &provider).await;
    let ctx = authed_context();

    // Settle 1000 into the pool, then seed INV-A (300, earlier) + INV-B (800, later).
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
            payment_id: "PAY-ALLOC-REST".to_owned(),
            gross_minor: 1000,
            fee_minor: 0,
            currency: "USD".to_owned(),
            effective_at: None,
        },
    )
    .await
    .expect("settle");
    seed_ar_invoice(
        &provider,
        &s,
        "INV-A",
        300,
        OffsetDateTime::now_utc() - time::Duration::hours(2),
    )
    .await;
    seed_ar_invoice(
        &provider,
        &s,
        "INV-B",
        800,
        OffsetDateTime::now_utc() - time::Duration::hours(1),
    )
    .await;

    // Allocate a lump of 500: INV-A fills (300), INV-B gets the remaining 200.
    let (status, applied) = send(
        router_with_db(provider.clone()).layer(axum::Extension(ctx)),
        "POST",
        "/bss-ledger/v1/payments/PAY-ALLOC-REST/allocations",
        Some(allocate_body(&s, 500, "USD")),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "allocate is 201: {applied}");
    assert_eq!(applied["replayed"], serde_json::json!(false));
    let allocs = applied["allocations"]
        .as_array()
        .expect("allocations array");
    assert_eq!(allocs.len(), 2, "two splits");
    assert_eq!(allocs[0]["invoice_id"], serde_json::json!("INV-A"));
    assert_eq!(allocs[0]["amount_minor"], serde_json::json!(300));
    assert_eq!(allocs[1]["invoice_id"], serde_json::json!("INV-B"));
    assert_eq!(allocs[1]["amount_minor"], serde_json::json!(200));
    assert_eq!(
        allocs[0]["precedence_policy_ref"],
        serde_json::json!(DEFAULT_PRECEDENCE_POLICY)
    );
}

/// An over-cap allocate → 409 `ALLOCATION_EXCEEDS_SETTLED`. Mirrors
/// `postgres_payments.rs::allocate_over_settled_cap_is_rejected`: settle PAY-CAP
/// at only 100, settle a second payment (500) to fund the shared pool so the
/// no-negative guard is NOT what trips, then allocate 200 against PAY-CAP — its
/// per-payment cap CHECK rejects it even though the pool is positive.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn allocate_over_cap_returns_409_exceeds_settled() {
    let (_c, raw, provider) = boot().await;
    let s = setup_seller(&raw, &provider).await;
    let ctx = authed_context();

    let settle = SettlementService::new(
        provider.clone(),
        Arc::new(LedgerEventPublisher::noop()),
        Arc::new(NoopLedgerMetrics),
    );
    let sys = SecurityContext::anonymous();
    let scope = AccessScope::for_tenant(s.tenant);
    let input = |pid: &str, gross: i64| SettlementInput {
        tenant_id: s.tenant,
        payer_tenant_id: s.payer,
        payment_id: pid.to_owned(),
        gross_minor: gross,
        fee_minor: 0,
        currency: "USD".to_owned(),
        effective_at: None,
    };
    settle
        .settle(&sys, &scope, input("PAY-CAP-1", 100))
        .await
        .expect("settle capped");
    settle
        .settle(&sys, &scope, input("PAY-OTHER", 500))
        .await
        .expect("fund the pool");
    seed_ar_invoice(
        &provider,
        &s,
        "INV-CAP",
        200,
        OffsetDateTime::now_utc() - time::Duration::hours(1),
    )
    .await;

    let (status, problem) = send(
        router_with_db(provider.clone()).layer(axum::Extension(ctx)),
        "POST",
        "/bss-ledger/v1/payments/PAY-CAP-1/allocations",
        Some(allocate_body(&s, 200, "USD")),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "over-cap is 409: {problem}");
    assert!(
        problem.to_string().contains("ALLOCATION_EXCEEDS_SETTLED"),
        "expected ALLOCATION_EXCEEDS_SETTLED, got {problem}"
    );
    // The rejected allocate rolled back: no allocations recorded.
    let repo = PaymentRepo::new(provider.clone());
    assert!(
        repo.list_payment_allocations(&scope, s.tenant, "PAY-CAP-1")
            .await
            .unwrap()
            .is_empty()
    );
}

/// A currency-mismatched allocate (settled USD, allocate EUR) → 400
/// `ALLOCATION_CURRENCY_MISMATCH`.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn allocate_currency_mismatch_returns_400() {
    let (_c, raw, provider) = boot().await;
    let s = setup_seller(&raw, &provider).await;
    let ctx = authed_context();

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
            payment_id: "PAY-CCY-1".to_owned(),
            gross_minor: 1000,
            fee_minor: 0,
            currency: "USD".to_owned(),
            effective_at: None,
        },
    )
    .await
    .expect("settle");

    let (status, problem) = send(
        router_with_db(provider.clone()).layer(axum::Extension(ctx)),
        "POST",
        "/bss-ledger/v1/payments/PAY-CCY-1/allocations",
        Some(allocate_body(&s, 500, "EUR")),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "mismatch is 400: {problem}"
    );
    assert!(
        problem.to_string().contains("ALLOCATION_CURRENCY_MISMATCH"),
        "expected ALLOCATION_CURRENCY_MISMATCH, got {problem}"
    );
}

/// Mode B over the wire: a caller-computed `splits` that over-allocates an
/// invoice past its open balance → 400 `ALLOCATION_SPLIT_INVALID` (the
/// `DomainError::AllocationSplitInvalid` → `field_violation` mapping, same 400
/// family as the other allocation rejections).
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn caller_split_over_open_returns_400_split_invalid() {
    let (_c, raw, provider) = boot().await;
    let s = setup_seller(&raw, &provider).await;
    let ctx = authed_context();

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
            payment_id: "PAY-CS-REST".to_owned(),
            gross_minor: 1000,
            fee_minor: 0,
            currency: "USD".to_owned(),
            effective_at: None,
        },
    )
    .await
    .expect("settle");
    seed_ar_invoice(
        &provider,
        &s,
        "INV-A",
        300,
        OffsetDateTime::now_utc() - time::Duration::hours(1),
    )
    .await;

    // 400 > INV-A's open 300 ⇒ rejected before any post.
    let body = serde_json::json!({
        "tenant_id": s.tenant,
        "payer_tenant_id": s.payer,
        "allocation_id": Uuid::now_v7(),
        "lump_minor": 1000,
        "currency": "USD",
        "scale": 2,
        "splits": [{ "invoice_id": "INV-A", "amount_minor": 400 }]
    });
    let (status, problem) = send(
        router_with_db(provider.clone()).layer(axum::Extension(ctx)),
        "POST",
        "/bss-ledger/v1/payments/PAY-CS-REST/allocations",
        Some(body),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "over-open caller split is 400: {problem}"
    );
    assert!(
        problem.to_string().contains("ALLOCATION_SPLIT_INVALID"),
        "expected ALLOCATION_SPLIT_INVALID, got {problem}"
    );
    // Rejected before the post: no rows recorded.
    let repo = PaymentRepo::new(provider.clone());
    assert!(
        repo.list_payment_allocations(&AccessScope::for_tenant(s.tenant), s.tenant, "PAY-CS-REST")
            .await
            .unwrap()
            .is_empty()
    );
}

/// `GET /payments/{id}/allocations` returns the recorded splits, and
/// `GET /balances/unallocated` returns the drained pool balance.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn list_allocations_and_read_unallocated() {
    let (_c, raw, provider) = boot().await;
    let s = setup_seller(&raw, &provider).await;
    let ctx = authed_context();

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
            payment_id: "PAY-LIST-1".to_owned(),
            gross_minor: 1000,
            fee_minor: 0,
            currency: "USD".to_owned(),
            effective_at: None,
        },
    )
    .await
    .expect("settle");
    seed_ar_invoice(
        &provider,
        &s,
        "INV-A",
        300,
        OffsetDateTime::now_utc() - time::Duration::hours(2),
    )
    .await;
    seed_ar_invoice(
        &provider,
        &s,
        "INV-B",
        800,
        OffsetDateTime::now_utc() - time::Duration::hours(1),
    )
    .await;

    // Allocate 500 via the router (so the rows exist), then list them back.
    let (status, _) = send(
        router_with_db(provider.clone()).layer(axum::Extension(ctx.clone())),
        "POST",
        "/bss-ledger/v1/payments/PAY-LIST-1/allocations",
        Some(allocate_body(&s, 500, "USD")),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, listed) = send(
        router_with_db(provider.clone()).layer(axum::Extension(ctx.clone())),
        "GET",
        "/bss-ledger/v1/payments/PAY-LIST-1/allocations",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "list is 200: {listed}");
    let allocs = listed["allocations"].as_array().expect("allocations array");
    assert_eq!(allocs.len(), 2, "two recorded splits");
    // Ordered by invoice_id (repo `order_by InvoiceId Asc`).
    assert_eq!(allocs[0]["invoice_id"], serde_json::json!("INV-A"));
    assert_eq!(allocs[0]["amount_minor"], serde_json::json!(300));
    assert_eq!(allocs[1]["invoice_id"], serde_json::json!("INV-B"));
    assert_eq!(allocs[1]["amount_minor"], serde_json::json!(200));

    // The pool drained by exactly the allocated total (1000 - 500 = 500 left).
    let (status, pool) = send(
        router_with_db(provider).layer(axum::Extension(ctx)),
        "GET",
        &format!(
            "/bss-ledger/v1/balances/unallocated?tenant_id={}&payer_tenant_id={}&currency=USD",
            s.tenant, s.payer
        ),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "unallocated read is 200: {pool}");
    assert_eq!(
        pool["payer_tenant_id"],
        serde_json::json!(s.payer.to_string())
    );
    assert_eq!(pool["currency"], serde_json::json!("USD"));
    assert_eq!(pool["balance_minor"], serde_json::json!(500));
}

/// A settle whose body `tenant_id` is OUTSIDE the caller's authorized scope (the
/// allow fake echoes only the subject tenant as `owner_tenant_id`) → the handler
/// `(payment, write)` gate's target-membership assertion denies it → 403, and no
/// settlement is recorded for the foreign tenant.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn settle_into_foreign_tenant_is_denied_403() {
    let (_c, raw, provider) = boot().await;
    let s = setup_seller(&raw, &provider).await;
    let ctx = authed_context();

    // Body targets a tenant the caller is NOT authorized for (≠ SUBJECT_TENANT).
    let foreign = Uuid::now_v7();
    let body = serde_json::json!({
        "tenant_id": foreign,
        "payer_tenant_id": s.payer,
        "payment_id": "PAY-FOREIGN",
        "gross_minor": 1000,
        "fee_minor": 0,
        "currency": "USD",
        "scale": 2
    });
    let (status, problem) = send(
        router_with_db(provider.clone()).layer(axum::Extension(ctx)),
        "POST",
        "/bss-ledger/v1/payments",
        Some(body),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "cross-tenant settle must be 403: {problem}"
    );

    // No settlement row was created for the foreign tenant.
    let repo = PaymentRepo::new(provider);
    assert!(
        repo.read_settlement(&AccessScope::for_tenant(foreign), foreign, "PAY-FOREIGN")
            .await
            .unwrap()
            .is_none(),
        "a denied settle must leave no settlement row"
    );
}

/// `POST /payments/{id}/allocations` for a payment that was NEVER settled → 202
/// ACCEPTED with the queued handle (§4.7 allocation-before-settlement): the body
/// `status` is the `allocation-queued` token and it carries the queue
/// `flow` (`PAYMENT_ALLOCATE`) + `business_id` (the request's `allocation_id`).
/// A second identical POST (SAME `allocation_id`) is still 202 — the queued
/// replay is idempotent (the dedup short-circuit returns the same handle).
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn allocate_unsettled_returns_202_queued() {
    let (_c, raw, provider) = boot().await;
    let s = setup_seller(&raw, &provider).await;
    let ctx = authed_context();

    // An open AR exists, but PAY-Q-REST is NEVER settled — so the allocate queues
    // instead of posting. A FIXED allocation_id lets the second POST replay it.
    seed_ar_invoice(
        &provider,
        &s,
        "INV-A",
        300,
        OffsetDateTime::now_utc() - time::Duration::hours(1),
    )
    .await;
    let allocation_id = Uuid::now_v7();
    let body = serde_json::json!({
        "tenant_id": s.tenant,
        "payer_tenant_id": s.payer,
        "allocation_id": allocation_id,
        "lump_minor": 300,
        "currency": "USD",
        "scale": 2
    });

    let (status, queued) = send(
        router_with_db(provider.clone()).layer(axum::Extension(ctx.clone())),
        "POST",
        "/bss-ledger/v1/payments/PAY-Q-REST/allocations",
        Some(body.clone()),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::ACCEPTED,
        "an unsettled allocate is 202: {queued}"
    );
    assert_eq!(
        queued["status"],
        serde_json::json!("allocation-queued"),
        "the 202 body status token: {queued}"
    );
    assert_eq!(
        queued["flow"],
        serde_json::json!("PAYMENT_ALLOCATE"),
        "the queued handle carries the PAYMENT_ALLOCATE flow"
    );
    assert_eq!(
        queued["business_id"],
        serde_json::json!(allocation_id.to_string()),
        "the queued handle's business_id is the allocation_id"
    );

    // The same allocation_id is still queued (not yet drained) → 202 replay.
    let (status, replay) = send(
        router_with_db(provider.clone()).layer(axum::Extension(ctx)),
        "POST",
        "/bss-ledger/v1/payments/PAY-Q-REST/allocations",
        Some(body),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::ACCEPTED,
        "a queued replay is still 202 (idempotent): {replay}"
    );
    assert_eq!(replay["status"], serde_json::json!("allocation-queued"));
    assert_eq!(
        replay["business_id"],
        serde_json::json!(allocation_id.to_string()),
        "the replay returns the same handle"
    );

    // Still queued, never posted: no allocation rows recorded for the payment.
    let repo = PaymentRepo::new(provider);
    assert!(
        repo.list_payment_allocations(&AccessScope::for_tenant(s.tenant), s.tenant, "PAY-Q-REST")
            .await
            .unwrap()
            .is_empty(),
        "a queued allocate records no allocation rows"
    );
}

/// `POST /payments/{id}/returns` after a settle → 201 fresh; re-posting the SAME
/// `psp_return_id` replays → 200, and the pool drains by exactly the returned
/// amount (1000 settled − 400 returned = 600 left).
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn return_payment_returns_201_then_200_on_replay() {
    let (_c, raw, provider) = boot().await;
    let s = setup_seller(&raw, &provider).await;
    let ctx = authed_context();

    // Settle 1000 into the pool.
    let (status, _) = send(
        router_with_db(provider.clone()).layer(axum::Extension(ctx.clone())),
        "POST",
        "/bss-ledger/v1/payments",
        Some(settle_body(&s, "PAY-RET-REST", 1000, 0)),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "settle is 201");

    // Return 400 → 201 fresh.
    let (status, fresh) = send(
        router_with_db(provider.clone()).layer(axum::Extension(ctx.clone())),
        "POST",
        "/bss-ledger/v1/payments/PAY-RET-REST/returns",
        Some(return_body(&s, "RET-REST-1", 400)),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "fresh return is 201: {fresh}");
    assert_eq!(fresh["replayed"], serde_json::json!(false));
    let entry_id = fresh["entry_id"].clone();

    // Re-post the same psp_return_id ⇒ idempotent replay ⇒ 200 with the prior ref.
    let (status, replay) = send(
        router_with_db(provider.clone()).layer(axum::Extension(ctx)),
        "POST",
        "/bss-ledger/v1/payments/PAY-RET-REST/returns",
        Some(return_body(&s, "RET-REST-1", 400)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "replay is 200: {replay}");
    assert_eq!(replay["replayed"], serde_json::json!(true));
    assert_eq!(replay["entry_id"], entry_id, "replay returns the prior id");

    // The pool drained by exactly the returned amount (1000 - 400 = 600).
    let repo = PaymentRepo::new(provider);
    assert_eq!(
        repo.read_unallocated(&AccessScope::for_tenant(s.tenant), s.tenant, s.payer, "USD")
            .await
            .unwrap(),
        600
    );
}
