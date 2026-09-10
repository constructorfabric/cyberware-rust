//! Postgres-only service-level tests for the chargeback (dispute) flow (Phase 4,
//! Group D): `ChargebackService::record_phase` drives the dispute state machine
//! (`opened → {won, lost}`) over the foundation engine, in both variants
//! (`CASH_HOLD` / `AR_RECLASS`). Ignored by default; run with
//! `cargo test -p cf-gears-bss-ledger --test postgres_chargebacks -- --ignored`.
//!
//! Mirrors `postgres_payment_returns.rs` (boot, `setup_seller`, a `settle`
//! helper) + `postgres_payments.rs` (`seed_ar_invoice`, the over-cap
//! fund-the-pool idiom). Each case asserts on the `ledger_dispute` row
//! (`variant` / `last_phase`), the chart `account_balance` /
//! `ar_invoice_balance.disputed_minor`, and `payment_settlement.clawed_back_minor`:
//!
//! 1. opened cash-hold (withheld): DISPUTE_HOLD = disputed, CASH_CLEARING net 0;
//!    dispute variant=CASH_HOLD, last_phase=OPENED.
//! 2. opened AR-reclass (not_moved): `balance_minor` unchanged, `disputed_minor`
//!    = disputed.
//! 3. won cash-hold: DISPUTE_HOLD 0, CASH_CLEARING restored; last_phase=WON.
//! 4. won AR-reclass: `disputed_minor` 0.
//! 5. lost cash-hold: DISPUTE_LOSS_EXPENSE = disputed, DISPUTE_HOLD 0,
//!    `clawed_back_minor` = disputed.
//! 6. lost AR-reclass (write-off, Model N): the lone `CR AR DISPUTED` writes the
//!    receivable off — `disputed_minor → 0` AND `balance_minor` dropped by
//!    disputed; DISPUTE_LOSS_EXPENSE = disputed; NO cash leg, so CASH_CLEARING is
//!    UNTOUCHED (the settle left it intact) and `clawed_back_minor` stays 0.
//! 7. lost AR-reclass write-off on an UNSETTLED payment: no settle ⇒
//!    CASH_CLEARING never funded; the write-off still books a REAL loss
//!    (DISPUTE_LOSS_EXPENSE = disputed, not netted to zero), posts no cash leg, and
//!    the unsettled payment has no settlement counter row (nothing clawed back).
//! 8. transition guard: opened on an already-OPENED dispute ⇒
//!    `InvalidDisputeTransition`.
//! 9. idempotent replay: the same opened twice ⇒ second is `replayed`, ledger
//!    effect applied once.
//! 10. cycle re-entrancy: opened → won → opened(cycle 2) succeeds.
//! 11. fee-bearing cash-hold won (Model N): settle gross 100 / fee 3 ⇒
//!     CASH_CLEARING net 97; opened parks 97 (CASH_CLEARING → 0, dropped by 97
//!     not 100); won restores CASH_CLEARING to 97.
//! 12. fee-bearing cash-hold lost (Model N): same net-97 open; lost ⇒
//!     DISPUTE_LOSS_EXPENSE = 97, CASH_CLEARING untouched, `clawed_back_minor` = 97
//!     (the net, not the gross 100).

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

use bss_ledger::domain::error::DomainError;
use bss_ledger::domain::instant::to_naive_date;
use bss_ledger::domain::model::{AccountRow, CurrencyScaleRow, NewEntry, NewLine};
use bss_ledger::domain::money::DEFAULT_PLAUSIBLE_MAX_MAJOR;
use bss_ledger::domain::payment::chargeback::{DisputePhase, FundsAtOpen};
use bss_ledger::domain::payment::settlement::SettlementInput;
use bss_ledger::domain::payment::settlement_return::SettlementReturnInput;
use bss_ledger::domain::ports::metrics::NoopLedgerMetrics;
use bss_ledger::infra::events::publisher::LedgerEventPublisher;
use bss_ledger::infra::payment::chargeback::{
    ChargebackOutcome, ChargebackRequest, ChargebackService,
};
use bss_ledger::infra::payment::settle::SettlementService;
use bss_ledger::infra::payment::settlement_return::SettlementReturnService;
use bss_ledger::infra::posting::service::PostingService;
use bss_ledger::infra::storage::migrations::Migrator;
use bss_ledger::infra::storage::repo::{PaymentRepo, ReferenceRepo};
use bss_ledger_sdk::{AccountClass, MappingStatus, Side, SourceDocType};
use chrono::NaiveDate;
use sea_orm::{ConnectionTrait, Database, Statement};
use sea_orm_migration::MigratorTrait;
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use time::OffsetDateTime;
use toolkit_db::secure::AccessScope;
use toolkit_db::{ConnectOpts, DBProvider, DbError, connect_db};
use toolkit_security::SecurityContext;
use uuid::Uuid;

fn pg(sql: impl Into<String>) -> Statement {
    Statement::from_string(sea_orm::DatabaseBackend::Postgres, sql.into())
}

/// Boot a container, migrate on a raw connection, and return a `bss`-search-path
/// `DBProvider` (the provisioning-test idiom).
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

/// Provisioned seller for the chargeback flow: the chart classes a dispute
/// touches — `CASH_CLEARING`, `UNALLOCATED` (the settle pool), `DISPUTE_HOLD`,
/// `DISPUTE_LOSS_EXPENSE`, `AR` — plus the `PSP_FEE_EXPENSE` the AR-invoice seed
/// credits. Mirrors `postgres_payments.rs::Seller` with the dispute classes
/// added.
struct Seller {
    tenant: Uuid,
    payer: Uuid,
    cash: Uuid,
    dispute_hold: Uuid,
    dispute_loss: Uuid,
    ar: Uuid,
    psp_fee: Uuid,
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

/// Provision the seller: USD@2 scale, an OPEN fiscal period for the current
/// month, and the chart accounts a dispute touches. `CASH_CLEARING` /
/// `DISPUTE_HOLD` / `DISPUTE_LOSS_EXPENSE` / `AR` are debit-normal; `UNALLOCATED`
/// is credit-normal (settle parks cash there); `PSP_FEE_EXPENSE` is the unguarded
/// counter the AR-invoice seed credits. settle/dispute derive `period_id` from
/// `OffsetDateTime::now_utc()` when no `effective_at` is supplied, matching this OPEN period.
async fn setup_seller(raw: &sea_orm::DatabaseConnection, provider: &DBProvider<DbError>) -> Seller {
    let now = OffsetDateTime::now_utc();
    let s = Seller {
        tenant: Uuid::now_v7(),
        payer: Uuid::now_v7(),
        cash: Uuid::now_v7(),
        dispute_hold: Uuid::now_v7(),
        dispute_loss: Uuid::now_v7(),
        ar: Uuid::now_v7(),
        psp_fee: Uuid::now_v7(),
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
        account(
            s.tenant,
            s.dispute_loss,
            AccountClass::DisputeLossExpense,
            Side::Debit,
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

fn settle_svc(provider: &DBProvider<DbError>) -> SettlementService {
    SettlementService::new(
        provider.clone(),
        Arc::new(LedgerEventPublisher::noop()),
        Arc::new(NoopLedgerMetrics),
    )
}

fn chargeback_svc(provider: &DBProvider<DbError>) -> ChargebackService {
    ChargebackService::new(
        provider.clone(),
        Arc::new(LedgerEventPublisher::noop()),
        Arc::new(NoopLedgerMetrics),
    )
}

/// Settle `gross` (fee 0) for `payment_id` — lands the cash in `CASH_CLEARING`
/// (DR net) + the unallocated pool (CR gross), and seeds the
/// `payment_settlement` counter row (`settled_minor = gross`) the clawback cap
/// nets against.
async fn settle(provider: &DBProvider<DbError>, s: &Seller, payment_id: &str, gross: i64) {
    settle_with_fee(provider, s, payment_id, gross, 0).await;
}

/// Settle `gross` with a PSP `fee` for `payment_id` (Model N): `settle` posts
/// `DR CASH_CLEARING (gross − fee) · DR PSP_FEE_EXPENSE (fee) · CR UNALLOCATED
/// (gross)`, so `CASH_CLEARING` only ever holds **net** = `gross − fee`. The
/// `payment_settlement` counter row is seeded `settled_minor = gross`,
/// `fee_minor = fee` — the orchestrator reads `net = settled − fee` pre-build to
/// size the CASH_HOLD dispute cash legs.
async fn settle_with_fee(
    provider: &DBProvider<DbError>,
    s: &Seller,
    payment_id: &str,
    gross: i64,
    fee: i64,
) {
    settle_svc(provider)
        .settle(
            &SecurityContext::anonymous(),
            &AccessScope::for_tenant(s.tenant),
            SettlementInput {
                tenant_id: s.tenant,
                payer_tenant_id: s.payer,
                payment_id: payment_id.to_owned(),
                gross_minor: gross,
                fee_minor: fee,
                currency: "USD".to_owned(),
                effective_at: None,
            },
        )
        .await
        .expect("settle must succeed");
}

fn settlement_return_svc(provider: &DBProvider<DbError>) -> SettlementReturnService {
    SettlementReturnService::new(
        provider.clone(),
        Arc::new(LedgerEventPublisher::noop()),
        Arc::new(NoopLedgerMetrics),
    )
}

/// Claw `amount` (fee 0) back out of `payment_id` through the real service —
/// decrements the payment's `settled_minor`/`net`, mirroring a PSP refund landing
/// AFTER a dispute has opened on the same payment.
async fn return_settlement(
    provider: &DBProvider<DbError>,
    s: &Seller,
    payment_id: &str,
    psp_return_id: &str,
    amount: i64,
) {
    settlement_return_svc(provider)
        .return_settlement(
            &SecurityContext::anonymous(),
            &AccessScope::for_tenant(s.tenant),
            SettlementReturnInput {
                tenant_id: s.tenant,
                payer_tenant_id: s.payer,
                payment_id: payment_id.to_owned(),
                psp_return_id: psp_return_id.to_owned(),
                amount_minor: amount,
                currency: "USD".to_owned(),
                effective_at: None,
            },
        )
        .await
        .expect("settlement return must succeed");
}

/// Record one dispute phase through the real service, returning the outcome.
#[allow(clippy::too_many_arguments)]
async fn record(
    provider: &DBProvider<DbError>,
    s: &Seller,
    dispute_id: &str,
    payment_id: &str,
    invoice_id: Option<&str>,
    cycle: i32,
    phase: DisputePhase,
    funds_at_open: FundsAtOpen,
    disputed: i64,
) -> Result<ChargebackOutcome, DomainError> {
    chargeback_svc(provider)
        .record_phase(
            &SecurityContext::anonymous(),
            &AccessScope::for_tenant(s.tenant),
            ChargebackRequest {
                tenant_id: s.tenant,
                payer_tenant_id: s.payer,
                payment_id: payment_id.to_owned(),
                dispute_id: dispute_id.to_owned(),
                invoice_id: invoice_id.map(ToOwned::to_owned),
                cycle,
                phase,
                funds_at_open,
                disputed_amount_minor: disputed,
                currency: "USD".to_owned(),
                effective_at: None,
            },
        )
        .await
}

/// Unwrap the inline-post arm. Every service-level case here either has its
/// `opened` first or IS the `opened`, so the outcome is `Recorded`; a `Queued`
/// would be a test-setup bug (it is the out-of-order REST path, exercised in
/// `rest_disputes.rs`), so panic.
fn recorded(outcome: ChargebackOutcome) -> bss_ledger_sdk::PostingRef {
    match outcome {
        ChargebackOutcome::Recorded(r) => r,
        ChargebackOutcome::Queued(q) => {
            panic!("expected an inline-posted phase, got Queued: {q:?}")
        }
    }
}

async fn account_balance(
    raw: &sea_orm::DatabaseConnection,
    s: &Seller,
    account: Uuid,
) -> Option<i64> {
    raw.query_one_raw(pg(format!(
        "SELECT balance_minor FROM bss.ledger_account_balance \
         WHERE tenant_id='{}' AND account_id='{}' AND currency='USD'",
        s.tenant, account
    )))
    .await
    .unwrap()
    .map(|r| r.try_get_by_index::<i64>(0).unwrap())
}

async fn ar_invoice_balance(
    raw: &sea_orm::DatabaseConnection,
    s: &Seller,
    invoice_id: &str,
) -> Option<i64> {
    raw.query_one_raw(pg(format!(
        "SELECT balance_minor FROM bss.ledger_ar_invoice_balance \
         WHERE tenant_id='{}' AND invoice_id='{}'",
        s.tenant, invoice_id
    )))
    .await
    .unwrap()
    .map(|r| r.try_get_by_index::<i64>(0).unwrap())
}

async fn ar_disputed_minor(
    raw: &sea_orm::DatabaseConnection,
    s: &Seller,
    invoice_id: &str,
) -> Option<i64> {
    raw.query_one_raw(pg(format!(
        "SELECT disputed_minor FROM bss.ledger_ar_invoice_balance \
         WHERE tenant_id='{}' AND invoice_id='{}'",
        s.tenant, invoice_id
    )))
    .await
    .unwrap()
    .map(|r| r.try_get_by_index::<i64>(0).unwrap())
}

/// Read the `ledger_dispute` current-state row's `(variant, last_phase, cycle)`.
async fn dispute_row(
    raw: &sea_orm::DatabaseConnection,
    s: &Seller,
    dispute_id: &str,
) -> Option<(String, String, i32)> {
    raw.query_one_raw(pg(format!(
        "SELECT variant, last_phase, cycle FROM bss.ledger_dispute \
         WHERE tenant_id='{}' AND dispute_id='{}'",
        s.tenant, dispute_id
    )))
    .await
    .unwrap()
    .map(|r| {
        (
            r.try_get_by_index::<String>(0).unwrap(),
            r.try_get_by_index::<String>(1).unwrap(),
            r.try_get_by_index::<i32>(2).unwrap(),
        )
    })
}

/// The work-state `status` of the CHARGEBACK queue row for one dispute phase
/// (`business_id = dispute_id:cycle:phase`), or `None` when no row was enqueued.
async fn queue_status(
    raw: &sea_orm::DatabaseConnection,
    s: &Seller,
    dispute_id: &str,
    cycle: i32,
    phase: DisputePhase,
) -> Option<String> {
    let business_id = format!("{dispute_id}:{cycle}:{}", phase.as_str());
    raw.query_one_raw(pg(format!(
        "SELECT status FROM bss.ledger_pending_event_queue \
         WHERE tenant_id='{}' AND flow='CHARGEBACK' AND business_id='{business_id}'",
        s.tenant
    )))
    .await
    .unwrap()
    .map(|r| r.try_get_by_index::<String>(0).unwrap())
}

/// `clawed_back_minor` on the payment's settlement counter (0 when never bumped).
async fn clawed_back(provider: &DBProvider<DbError>, s: &Seller, payment_id: &str) -> i64 {
    PaymentRepo::new(provider.clone())
        .read_settlement(&AccessScope::for_tenant(s.tenant), s.tenant, payment_id)
        .await
        .unwrap()
        .expect("settlement row present")
        .clawed_back_minor
}

/// Seed an OPEN AR invoice by posting `DR AR (invoice_id) / CR PSP_FEE_EXPENSE`
/// directly through the engine (mirrors `postgres_payments.rs::seed_ar_invoice`).
/// PSP_FEE_EXPENSE is unguarded, so this lands a clean `ar_invoice_balance` row
/// (`disputed_minor = 0`) the AR-reclass dispute then moves.
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

// ── 1. opened cash-hold (withheld) ───────────────────────────────────────────

/// `opened` with `funds_at_open = withheld` selects `CASH_HOLD`: settle 1000
/// (CASH_CLEARING = 1000), then `opened` moves the cash into the hold
/// (`DR DISPUTE_HOLD / CR CASH_CLEARING`, each 1000) ⇒ DISPUTE_HOLD = 1000,
/// CASH_CLEARING net 0; the dispute row records `variant = CASH_HOLD`,
/// `last_phase = OPENED`.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn opened_cash_hold_moves_cash_into_hold() {
    let (_c, raw, provider) = boot().await;
    let s = setup_seller(&raw, &provider).await;

    settle(&provider, &s, "PAY-CH-1", 1000).await;
    let posted = recorded(
        record(
            &provider,
            &s,
            "DSP-CH-1",
            "PAY-CH-1",
            None,
            1,
            DisputePhase::Opened,
            FundsAtOpen::Withheld,
            1000,
        )
        .await
        .expect("opened cash-hold must post"),
    );
    assert!(!posted.replayed, "first opened is fresh");

    // The cash parked in the hold; clearing is back to net zero.
    assert_eq!(
        account_balance(&raw, &s, s.dispute_hold).await,
        Some(1000),
        "DISPUTE_HOLD holds the disputed cash"
    );
    assert_eq!(
        account_balance(&raw, &s, s.cash).await,
        Some(0),
        "CASH_CLEARING net 0 (1000 in from settle, 1000 out to the hold)"
    );
    // The dispute current-state row records the chosen variant + phase.
    assert_eq!(
        dispute_row(&raw, &s, "DSP-CH-1").await,
        Some(("CASH_HOLD".to_owned(), "OPENED".to_owned(), 1)),
        "dispute row: variant=CASH_HOLD, last_phase=OPENED, cycle=1"
    );
}

// ── 2. opened AR-reclass (not_moved) ─────────────────────────────────────────

/// `opened` with `funds_at_open = not_moved` selects `AR_RECLASS`: seed an open
/// AR invoice (1000), then `opened` reclasses it `ACTIVE → DISPUTED`
/// (`DR AR DISPUTED + CR AR ACTIVE`, AR-class-neutral) ⇒ `balance_minor`
/// unchanged (1000), `disputed_minor` = 1000. No cash moves.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn opened_ar_reclass_moves_disputed_slice() {
    let (_c, raw, provider) = boot().await;
    let s = setup_seller(&raw, &provider).await;

    seed_ar_invoice(
        &provider,
        &s,
        "INV-AR-2",
        1000,
        OffsetDateTime::now_utc() - time::Duration::hours(1),
    )
    .await;
    recorded(
        record(
            &provider,
            &s,
            "DSP-AR-2",
            "PAY-AR-2",
            Some("INV-AR-2"),
            1,
            DisputePhase::Opened,
            FundsAtOpen::NotMoved,
            1000,
        )
        .await
        .expect("opened AR-reclass must post"),
    );

    // AR-class-neutral: the full open AR is unchanged; the disputed slice moved.
    assert_eq!(
        ar_invoice_balance(&raw, &s, "INV-AR-2").await,
        Some(1000),
        "balance_minor unchanged by an AR-class-neutral reclass"
    );
    assert_eq!(
        ar_disputed_minor(&raw, &s, "INV-AR-2").await,
        Some(1000),
        "disputed_minor = the disputed amount (+D)"
    );
    assert_eq!(
        dispute_row(&raw, &s, "DSP-AR-2").await,
        Some(("AR_RECLASS".to_owned(), "OPENED".to_owned(), 1)),
        "dispute row: variant=AR_RECLASS, last_phase=OPENED"
    );
}

// ── 3. won cash-hold ─────────────────────────────────────────────────────────

/// `won` on a `CASH_HOLD` dispute releases the hold back to clearing
/// (`DR CASH_CLEARING / CR DISPUTE_HOLD`): DISPUTE_HOLD 0, CASH_CLEARING restored
/// to its pre-dispute net (1000); dispute `last_phase = WON`. No clawback.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn won_cash_hold_releases_hold() {
    let (_c, raw, provider) = boot().await;
    let s = setup_seller(&raw, &provider).await;

    settle(&provider, &s, "PAY-CH-3", 1000).await;
    record(
        &provider,
        &s,
        "DSP-CH-3",
        "PAY-CH-3",
        None,
        1,
        DisputePhase::Opened,
        FundsAtOpen::Withheld,
        1000,
    )
    .await
    .expect("opened");
    recorded(
        record(
            &provider,
            &s,
            "DSP-CH-3",
            "PAY-CH-3",
            None,
            1,
            DisputePhase::Won,
            FundsAtOpen::Withheld,
            1000,
        )
        .await
        .expect("won cash-hold must post"),
    );

    assert_eq!(
        account_balance(&raw, &s, s.dispute_hold).await,
        Some(0),
        "the hold is released on a won"
    );
    assert_eq!(
        account_balance(&raw, &s, s.cash).await,
        Some(1000),
        "CASH_CLEARING restored (the withheld cash is the seller's again)"
    );
    assert_eq!(
        dispute_row(&raw, &s, "DSP-CH-3").await.map(|r| r.1),
        Some("WON".to_owned()),
        "last_phase=WON"
    );
    assert_eq!(
        clawed_back(&provider, &s, "PAY-CH-3").await,
        0,
        "a won claws nothing back"
    );
}

// ── 4. won AR-reclass ────────────────────────────────────────────────────────

/// `won` on an `AR_RECLASS` dispute reverses the reclass `DISPUTED → ACTIVE`
/// (`DR AR ACTIVE + CR AR DISPUTED`) ⇒ `disputed_minor` back to 0,
/// `balance_minor` still the full open AR; `last_phase = WON`.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn won_ar_reclass_clears_disputed_slice() {
    let (_c, raw, provider) = boot().await;
    let s = setup_seller(&raw, &provider).await;

    seed_ar_invoice(
        &provider,
        &s,
        "INV-AR-4",
        1000,
        OffsetDateTime::now_utc() - time::Duration::hours(1),
    )
    .await;
    record(
        &provider,
        &s,
        "DSP-AR-4",
        "PAY-AR-4",
        Some("INV-AR-4"),
        1,
        DisputePhase::Opened,
        FundsAtOpen::NotMoved,
        1000,
    )
    .await
    .expect("opened");
    recorded(
        record(
            &provider,
            &s,
            "DSP-AR-4",
            "PAY-AR-4",
            Some("INV-AR-4"),
            1,
            DisputePhase::Won,
            FundsAtOpen::NotMoved,
            1000,
        )
        .await
        .expect("won AR-reclass must post"),
    );

    assert_eq!(
        ar_disputed_minor(&raw, &s, "INV-AR-4").await,
        Some(0),
        "disputed_minor cleared on a won (−D)"
    );
    assert_eq!(
        ar_invoice_balance(&raw, &s, "INV-AR-4").await,
        Some(1000),
        "balance_minor still the full open AR"
    );
    assert_eq!(
        dispute_row(&raw, &s, "DSP-AR-4").await.map(|r| r.1),
        Some("WON".to_owned()),
        "last_phase=WON"
    );
}

// ── 5. lost cash-hold ────────────────────────────────────────────────────────

/// `lost` on a `CASH_HOLD` dispute forfeits the already-withheld hold funds
/// (`DR DISPUTE_LOSS_EXPENSE / CR DISPUTE_HOLD`): DISPUTE_LOSS_EXPENSE = 1000,
/// DISPUTE_HOLD 0, CASH_CLEARING untouched (the cash left clearing at open); the
/// orchestrator bumps `clawed_back_minor` by 1000.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn lost_cash_hold_forfeits_hold_and_claws_back() {
    let (_c, raw, provider) = boot().await;
    let s = setup_seller(&raw, &provider).await;

    settle(&provider, &s, "PAY-CH-5", 1000).await;
    record(
        &provider,
        &s,
        "DSP-CH-5",
        "PAY-CH-5",
        None,
        1,
        DisputePhase::Opened,
        FundsAtOpen::Withheld,
        1000,
    )
    .await
    .expect("opened");
    recorded(
        record(
            &provider,
            &s,
            "DSP-CH-5",
            "PAY-CH-5",
            None,
            1,
            DisputePhase::Lost,
            FundsAtOpen::Withheld,
            1000,
        )
        .await
        .expect("lost cash-hold must post"),
    );

    assert_eq!(
        account_balance(&raw, &s, s.dispute_loss).await,
        Some(1000),
        "the forfeiture booked into DISPUTE_LOSS_EXPENSE"
    );
    assert_eq!(
        account_balance(&raw, &s, s.dispute_hold).await,
        Some(0),
        "the hold is emptied on the loss"
    );
    assert_eq!(
        account_balance(&raw, &s, s.cash).await,
        Some(0),
        "CASH_CLEARING untouched by the cash-hold loss (cash left at open)"
    );
    assert_eq!(
        clawed_back(&provider, &s, "PAY-CH-5").await,
        1000,
        "clawed_back_minor bumped by the forfeited held funds"
    );
}

// ── 6. lost AR-reclass — write-off ───────────────────────────────────────────

/// `lost` on an `AR_RECLASS` dispute is a WRITE-OFF (Model N): the receivable was
/// never collected (funds `not_moved`), so the lone `CR AR (ar_status = DISPUTED)`
/// writes it off — `DR DISPUTE_LOSS_EXPENSE (disputed) / CR AR DISPUTED
/// (disputed)`. The single CR AR DISPUTED nets `−D` on BOTH `balance_minor` and
/// `disputed_minor`, so after `lost`: `disputed_minor → 0` AND `balance_minor`
/// dropped by `disputed`; `DISPUTE_LOSS_EXPENSE = disputed`. There is NO cash leg,
/// so `CASH_CLEARING` is UNTOUCHED (still whatever the settle left it) and the
/// payment's `clawed_back_minor` stays 0. A settle funds CASH_CLEARING here only
/// to prove the write-off leaves it alone (no clawback against the held cash).
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn lost_ar_reclass_writes_off_to_loss() {
    let (_c, raw, provider) = boot().await;
    let s = setup_seller(&raw, &provider).await;

    // Settle (CASH_CLEARING = 1000) + seed the settlement counter — present ONLY
    // so we can prove the write-off does NOT touch the cash. The disputed
    // receivable is a separate seeded AR invoice (balance_minor = 1000).
    settle(&provider, &s, "PAY-AR-6", 1000).await;
    seed_ar_invoice(
        &provider,
        &s,
        "INV-AR-6",
        1000,
        OffsetDateTime::now_utc() - time::Duration::hours(1),
    )
    .await;
    record(
        &provider,
        &s,
        "DSP-AR-6",
        "PAY-AR-6",
        Some("INV-AR-6"),
        1,
        DisputePhase::Opened,
        FundsAtOpen::NotMoved,
        1000,
    )
    .await
    .expect("opened");
    recorded(
        record(
            &provider,
            &s,
            "DSP-AR-6",
            "PAY-AR-6",
            Some("INV-AR-6"),
            1,
            DisputePhase::Lost,
            FundsAtOpen::NotMoved,
            1000,
        )
        .await
        .expect("lost AR-reclass (write-off) must post"),
    );

    // The lone CR AR DISPUTED clears the disputed slice AND writes the receivable
    // down: disputed_minor → 0 and balance_minor dropped by the full disputed.
    assert_eq!(
        ar_disputed_minor(&raw, &s, "INV-AR-6").await,
        Some(0),
        "disputed_minor → 0 (the disputed slice is written off)"
    );
    assert_eq!(
        ar_invoice_balance(&raw, &s, "INV-AR-6").await,
        Some(0),
        "balance_minor dropped by disputed (1000 → 0) via the lone CR AR DISPUTED"
    );
    assert_eq!(
        account_balance(&raw, &s, s.dispute_loss).await,
        Some(1000),
        "the receivable written off to DISPUTE_LOSS_EXPENSE (a real loss)"
    );
    // The write-off posts NO cash leg, so CASH_CLEARING is untouched by it (still
    // the 1000 the settle left).
    assert_eq!(
        account_balance(&raw, &s, s.cash).await,
        Some(1000),
        "CASH_CLEARING UNTOUCHED by the write-off (no cash leg)"
    );
    assert_eq!(
        clawed_back(&provider, &s, "PAY-AR-6").await,
        0,
        "a write-off claws nothing back (nothing was ever collected)"
    );
}

// ── 7. lost AR-reclass write-off on an unsettled payment ─────────────────────

/// The AR-reclass write-off is independent of any cash / settlement: an
/// invoice/ACH receivable can be disputed and lost with NO payment ever settled
/// (the funds were `not_moved`, so nothing ever hit `CASH_CLEARING`). The
/// write-off still books a REAL loss — `DR DISPUTE_LOSS_EXPENSE (disputed) /
/// CR AR DISPUTED (disputed)` — it is NOT netted to zero. After `lost`:
/// `disputed_minor → 0`, `balance_minor` dropped by `disputed`,
/// `DISPUTE_LOSS_EXPENSE = disputed`. CASH_CLEARING is never funded (no settle)
/// and the write-off posts no cash leg, so it stays at 0; the unsettled payment
/// has no `payment_settlement` counter row at all (nothing clawed back). The post
/// must SUCCEED and no guarded balance may be negative.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn lost_ar_reclass_write_off_without_settlement() {
    let (_c, raw, provider) = boot().await;
    let s = setup_seller(&raw, &provider).await;

    // No settle ⇒ CASH_CLEARING is never funded (stays 0). Seed the disputed AR
    // invoice (balance_minor = 1000). The write-off posts no cash leg, so it never
    // touches clearing and never bumps a (non-existent) settlement counter.
    seed_ar_invoice(
        &provider,
        &s,
        "INV-AR-7",
        1000,
        OffsetDateTime::now_utc() - time::Duration::hours(1),
    )
    .await;
    record(
        &provider,
        &s,
        "DSP-AR-7",
        "PAY-AR-7",
        Some("INV-AR-7"),
        1,
        DisputePhase::Opened,
        FundsAtOpen::NotMoved,
        1000,
    )
    .await
    .expect("opened");

    // The write-off posts (no cash leg ⇒ no negative-balance abort).
    let posted = recorded(
        record(
            &provider,
            &s,
            "DSP-AR-7",
            "PAY-AR-7",
            Some("INV-AR-7"),
            1,
            DisputePhase::Lost,
            FundsAtOpen::NotMoved,
            1000,
        )
        .await
        .expect("lost AR-reclass write-off must post"),
    );
    assert!(!posted.replayed, "the write-off lost is a fresh post");

    // The lone CR AR DISPUTED clears the disputed slice AND writes the receivable
    // down: disputed_minor → 0 and balance_minor dropped by the full disputed.
    assert_eq!(
        ar_disputed_minor(&raw, &s, "INV-AR-7").await,
        Some(0),
        "disputed_minor → 0 (the disputed slice is written off)"
    );
    assert_eq!(
        ar_invoice_balance(&raw, &s, "INV-AR-7").await,
        Some(0),
        "balance_minor dropped by disputed (1000 → 0) via the lone CR AR DISPUTED"
    );
    // A REAL loss is booked (the receivable written off) — NOT netted to zero.
    assert_eq!(
        account_balance(&raw, &s, s.dispute_loss).await,
        Some(1000),
        "DISPUTE_LOSS_EXPENSE = disputed (a real write-off loss, not zero)"
    );
    // CASH_CLEARING was never funded and the write-off posts no cash leg, so it
    // stays at 0 (and never went negative — the post would have aborted if it had).
    let cash = account_balance(&raw, &s, s.cash).await.unwrap_or(0);
    assert!(
        cash >= 0,
        "CASH_CLEARING must never be negative; got {cash}"
    );
    assert_eq!(
        cash, 0,
        "CASH_CLEARING never funded and untouched by the write-off (no cash leg)"
    );
    // The payment was never settled, so there is no counter row to claw back from.
    assert!(
        PaymentRepo::new(provider.clone())
            .read_settlement(&AccessScope::for_tenant(s.tenant), s.tenant, "PAY-AR-7")
            .await
            .unwrap()
            .is_none(),
        "no clawback occurred — the unsettled payment has no settlement counter row"
    );
}

// ── 8. transition guard ──────────────────────────────────────────────────────

/// An `opened` on a dispute whose `last_phase` is still `OPENED` is an illegal
/// re-open ⇒ `InvalidDisputeTransition`; the dispute row is unchanged.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn opened_on_open_dispute_is_rejected() {
    let (_c, raw, provider) = boot().await;
    let s = setup_seller(&raw, &provider).await;

    settle(&provider, &s, "PAY-G-8", 1000).await;
    record(
        &provider,
        &s,
        "DSP-G-8",
        "PAY-G-8",
        None,
        1,
        DisputePhase::Opened,
        FundsAtOpen::Withheld,
        1000,
    )
    .await
    .expect("first opened");

    // A SECOND opened (same dispute, same cycle) on a still-OPENED dispute: a
    // distinct cycle would change the business id, so re-open the same cycle to
    // reach the transition guard (not the dedup short-circuit).
    let err = record(
        &provider,
        &s,
        "DSP-G-8",
        "PAY-G-8",
        None,
        2,
        DisputePhase::Opened,
        FundsAtOpen::Withheld,
        1000,
    )
    .await
    .expect_err("an opened on a still-OPENED dispute must be rejected");
    assert!(
        matches!(err, DomainError::InvalidDisputeTransition(_)),
        "expected InvalidDisputeTransition, got {err:?}"
    );

    // The rejected re-open left the row at its first-cycle OPENED state.
    assert_eq!(
        dispute_row(&raw, &s, "DSP-G-8").await,
        Some(("CASH_HOLD".to_owned(), "OPENED".to_owned(), 1)),
        "the illegal re-open did not advance the dispute row"
    );
}

// ── 9. idempotent replay ─────────────────────────────────────────────────────

/// Re-recording the SAME `opened` (same `dispute_id:cycle:phase`) replays: the
/// second call returns `replayed = true` with the prior entry id, and the ledger
/// effect (DISPUTE_HOLD = 1000) is applied exactly once.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn opened_replay_is_idempotent() {
    let (_c, raw, provider) = boot().await;
    let s = setup_seller(&raw, &provider).await;

    settle(&provider, &s, "PAY-R-9", 1000).await;
    let fresh = recorded(
        record(
            &provider,
            &s,
            "DSP-R-9",
            "PAY-R-9",
            None,
            1,
            DisputePhase::Opened,
            FundsAtOpen::Withheld,
            1000,
        )
        .await
        .expect("fresh opened"),
    );
    assert!(!fresh.replayed, "first opened is fresh");

    let replay = recorded(
        record(
            &provider,
            &s,
            "DSP-R-9",
            "PAY-R-9",
            None,
            1,
            DisputePhase::Opened,
            FundsAtOpen::Withheld,
            1000,
        )
        .await
        .expect("replayed opened"),
    );
    assert!(replay.replayed, "same dispute:cycle:phase replays");
    assert_eq!(
        replay.entry_id, fresh.entry_id,
        "replay returns the prior entry id"
    );

    // The hold move applied exactly once (1000, not 2000).
    assert_eq!(
        account_balance(&raw, &s, s.dispute_hold).await,
        Some(1000),
        "the ledger effect applied once, not twice"
    );
    assert_eq!(
        account_balance(&raw, &s, s.cash).await,
        Some(0),
        "CASH_CLEARING net 0 (the replay moved no further cash)"
    );
}

// ── 10. cycle re-entrancy ────────────────────────────────────────────────────

/// `opened → won → opened(cycle 2)` succeeds: once the first cycle ends (WON), a
/// fresh `opened` on a NEW cycle is a legal transition. The row advances to the
/// new cycle's OPENED state and the hold is taken again.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn reopen_after_won_starts_fresh_cycle() {
    let (_c, raw, provider) = boot().await;
    let s = setup_seller(&raw, &provider).await;

    // Cycle 1: settle enough for two successive holds (1000 each), open + win.
    settle(&provider, &s, "PAY-RE-10", 2000).await;
    record(
        &provider,
        &s,
        "DSP-RE-10",
        "PAY-RE-10",
        None,
        1,
        DisputePhase::Opened,
        FundsAtOpen::Withheld,
        1000,
    )
    .await
    .expect("cycle 1 opened");
    record(
        &provider,
        &s,
        "DSP-RE-10",
        "PAY-RE-10",
        None,
        1,
        DisputePhase::Won,
        FundsAtOpen::Withheld,
        1000,
    )
    .await
    .expect("cycle 1 won");

    // Cycle 2: a fresh opened on the now-resolved dispute is legal.
    recorded(
        record(
            &provider,
            &s,
            "DSP-RE-10",
            "PAY-RE-10",
            None,
            2,
            DisputePhase::Opened,
            FundsAtOpen::Withheld,
            1000,
        )
        .await
        .expect("cycle 2 opened must succeed on a resolved dispute"),
    );

    // The row advanced to cycle 2's OPENED state.
    assert_eq!(
        dispute_row(&raw, &s, "DSP-RE-10").await,
        Some(("CASH_HOLD".to_owned(), "OPENED".to_owned(), 2)),
        "the dispute re-opened on a fresh cycle (cycle=2, last_phase=OPENED)"
    );
    // Cycle 1 won released the hold (→0), cycle 2 opened took it again (→1000).
    assert_eq!(
        account_balance(&raw, &s, s.dispute_hold).await,
        Some(1000),
        "the new cycle's hold is taken"
    );
}

// ── 11. fee-bearing CASH_HOLD — won (net-sized cash legs, Model N) ────────────

/// The real Model N coverage: a CASH_HOLD dispute over a payment settled with a
/// PSP fee. Settle gross 100 with fee 3 ⇒ `DR CASH_CLEARING 97 · DR
/// PSP_FEE_EXPENSE 3 · CR UNALLOCATED 100`, so CASH_CLEARING only holds the
/// **net** 97. `opened` then sizes its cash legs at net: `DR DISPUTE_HOLD 97 / CR
/// CASH_CLEARING 97` ⇒ DISPUTE_HOLD = 97 and CASH_CLEARING dropped by exactly 97
/// (97 → 0), NOT by the gross 100 (which would underflow the guarded clearing).
/// `won` releases the hold: `DR CASH_CLEARING 97 / CR DISPUTE_HOLD 97` ⇒
/// CASH_CLEARING restored to 97; nothing clawed back. `disputed` is the GROSS
/// claim (100); the orchestrator reads `net = settled − fee` to size the legs.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn fee_bearing_cash_hold_won_uses_net_legs() {
    let (_c, raw, provider) = boot().await;
    let s = setup_seller(&raw, &provider).await;

    // gross 100, fee 3 ⇒ CASH_CLEARING = net 97 (the fee is in PSP_FEE_EXPENSE).
    settle_with_fee(&provider, &s, "PAY-FEE-W", 100, 3).await;
    assert_eq!(
        account_balance(&raw, &s, s.cash).await,
        Some(97),
        "settle lands NET 97 in CASH_CLEARING (the 3 fee went to PSP_FEE_EXPENSE)"
    );

    // opened CASH_HOLD: disputed is the GROSS claim (100), but the cash legs are
    // sized at net (97) — DISPUTE_HOLD = 97, CASH_CLEARING dropped by 97 (→ 0).
    record(
        &provider,
        &s,
        "DSP-FEE-W",
        "PAY-FEE-W",
        None,
        1,
        DisputePhase::Opened,
        FundsAtOpen::Withheld,
        100,
    )
    .await
    .expect("opened cash-hold (fee-bearing)");
    assert_eq!(
        account_balance(&raw, &s, s.dispute_hold).await,
        Some(97),
        "DISPUTE_HOLD holds the NET 97, not the gross 100"
    );
    assert_eq!(
        account_balance(&raw, &s, s.cash).await,
        Some(0),
        "CASH_CLEARING dropped by exactly 97 (97 → 0), not by the gross 100"
    );

    // won: release the net hold back to clearing (CASH_CLEARING restored to 97).
    recorded(
        record(
            &provider,
            &s,
            "DSP-FEE-W",
            "PAY-FEE-W",
            None,
            1,
            DisputePhase::Won,
            FundsAtOpen::Withheld,
            100,
        )
        .await
        .expect("won cash-hold (fee-bearing)"),
    );
    assert_eq!(
        account_balance(&raw, &s, s.dispute_hold).await,
        Some(0),
        "the hold is released on the won"
    );
    assert_eq!(
        account_balance(&raw, &s, s.cash).await,
        Some(97),
        "CASH_CLEARING restored by the net 97 (the 3 fee stays expensed)"
    );
    assert_eq!(
        clawed_back(&provider, &s, "PAY-FEE-W").await,
        0,
        "a won claws nothing back"
    );
}

// ── 12. fee-bearing CASH_HOLD — lost (net-sized loss + clawback, Model N) ─────

/// The lost counterpart of the fee-bearing CASH_HOLD path. Settle gross 100 with
/// fee 3 (CASH_CLEARING = net 97); `opened` parks the net 97 in DISPUTE_HOLD
/// (CASH_CLEARING → 0). `lost` forfeits the held net out of the hold: `DR
/// DISPUTE_LOSS_EXPENSE 97 / CR DISPUTE_HOLD 97` ⇒ DISPUTE_LOSS_EXPENSE = 97,
/// DISPUTE_HOLD = 0, CASH_CLEARING untouched (the cash left at open). The
/// orchestrator bumps `clawed_back_minor` by the NET 97 (not the gross 100); the
/// total loss is net 97 + the 3 fee already expensed at settle = gross 100.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn fee_bearing_cash_hold_lost_uses_net_legs() {
    let (_c, raw, provider) = boot().await;
    let s = setup_seller(&raw, &provider).await;

    // gross 100, fee 3 ⇒ CASH_CLEARING = net 97.
    settle_with_fee(&provider, &s, "PAY-FEE-L", 100, 3).await;

    // opened CASH_HOLD: net 97 into the hold, CASH_CLEARING dropped by 97 (→ 0).
    record(
        &provider,
        &s,
        "DSP-FEE-L",
        "PAY-FEE-L",
        None,
        1,
        DisputePhase::Opened,
        FundsAtOpen::Withheld,
        100,
    )
    .await
    .expect("opened cash-hold (fee-bearing)");
    assert_eq!(
        account_balance(&raw, &s, s.dispute_hold).await,
        Some(97),
        "DISPUTE_HOLD holds the NET 97"
    );
    assert_eq!(
        account_balance(&raw, &s, s.cash).await,
        Some(0),
        "CASH_CLEARING dropped by exactly 97 (→ 0)"
    );

    // lost: forfeit the net hold to loss; CASH_CLEARING untouched (cash left at open).
    recorded(
        record(
            &provider,
            &s,
            "DSP-FEE-L",
            "PAY-FEE-L",
            None,
            1,
            DisputePhase::Lost,
            FundsAtOpen::Withheld,
            100,
        )
        .await
        .expect("lost cash-hold (fee-bearing)"),
    );
    assert_eq!(
        account_balance(&raw, &s, s.dispute_loss).await,
        Some(97),
        "DISPUTE_LOSS_EXPENSE = the NET 97 (the 3 fee was already expensed at settle)"
    );
    assert_eq!(
        account_balance(&raw, &s, s.dispute_hold).await,
        Some(0),
        "the hold is emptied on the loss"
    );
    assert_eq!(
        account_balance(&raw, &s, s.cash).await,
        Some(0),
        "CASH_CLEARING untouched by the loss (the cash left clearing at open)"
    );
    assert_eq!(
        clawed_back(&provider, &s, "PAY-FEE-L").await,
        97,
        "clawed_back_minor bumped by the NET 97, not the gross 100"
    );
}

// ── drain-on-opened ──────────────────────────────────────────────────────────

/// Drain-on-`opened` (mirrors `postgres_queue.rs::settle_drains_queued_allocation`):
/// an out-of-order `won` (no `opened` landed yet) is durably QUEUED at intake
/// (§4.7); recording the `opened` then drains the CHARGEBACK queue INLINE — the
/// queued `won` applies in a second txn without waiting for the periodic sweep.
/// After the `opened`: the queue row flips `QUEUED → APPLIED`, the dispute advances
/// to `WON`, and the won's ledger effect lands (the hold is released back to
/// CASH_CLEARING).
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn opened_drains_queued_outcome() {
    let (_c, raw, provider) = boot().await;
    let s = setup_seller(&raw, &provider).await;

    // Fund the cash leg so the cash-hold dispute has net to move.
    settle(&provider, &s, "PAY-CH-DR", 1000).await;

    // Record a `won` BEFORE any `opened` — out-of-order, so it durably QUEUES (202).
    let queued = record(
        &provider,
        &s,
        "DSP-CH-DR",
        "PAY-CH-DR",
        None,
        1,
        DisputePhase::Won,
        FundsAtOpen::Withheld,
        1000,
    )
    .await
    .expect("out-of-order won queues");
    assert!(
        matches!(queued, ChargebackOutcome::Queued(_)),
        "a won with no prior opened is enqueued, not posted"
    );
    assert_eq!(
        queue_status(&raw, &s, "DSP-CH-DR", 1, DisputePhase::Won)
            .await
            .as_deref(),
        Some("QUEUED"),
        "the out-of-order won is durably QUEUED"
    );

    // Record the `opened` — the drain-on-opened hook then applies the queued won
    // inline (a fresh, non-replayed opened is the only outcome that drains).
    recorded(
        record(
            &provider,
            &s,
            "DSP-CH-DR",
            "PAY-CH-DR",
            None,
            1,
            DisputePhase::Opened,
            FundsAtOpen::Withheld,
            1000,
        )
        .await
        .expect("opened must post (and drain the queue)"),
    );

    // The queued won applied inline: queue row → APPLIED, dispute advanced to WON.
    assert_eq!(
        queue_status(&raw, &s, "DSP-CH-DR", 1, DisputePhase::Won)
            .await
            .as_deref(),
        Some("APPLIED"),
        "the drain-on-opened flipped the queued won to APPLIED"
    );
    assert_eq!(
        dispute_row(&raw, &s, "DSP-CH-DR").await.map(|r| r.1),
        Some("WON".to_owned()),
        "the queued won advanced the dispute to WON"
    );
    // The won's ledger effect landed: the hold is released back to clearing.
    assert_eq!(
        account_balance(&raw, &s, s.dispute_hold).await,
        Some(0),
        "the hold is released by the drained won"
    );
    assert_eq!(
        account_balance(&raw, &s, s.cash).await,
        Some(1000),
        "CASH_CLEARING restored to its pre-dispute net by the drained won"
    );
}

/// Regression for the `(last_phase, cycle)` predicate on `dispute_advance`: an
/// outcome targeting a STALE cycle must NOT resolve the CURRENT open cycle. Open
/// cycle 1 (CASH_HOLD), win it, re-open as cycle 2, then submit a `lost` for the
/// already-closed cycle 1. Its dedup key (`DSP:1:lost`) never posted (cycle 1 was
/// WON, not lost), so it clears the dedup gate AND the out-of-txn transition guard
/// (which sees the cycle-2 row as OPENED and does not check the cycle). Only the
/// in-txn `cycle = 1` predicate stops it: the cycle-2 row matches 0 rows, so the
/// stale outcome is rejected as `InvalidDisputeTransition` instead of silently
/// resolving cycle 2 (and committing a second outcome entry) with cycle 1's data.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn stale_cycle_outcome_does_not_resolve_current_cycle() {
    let (_c, raw, provider) = boot().await;
    let s = setup_seller(&raw, &provider).await;

    settle(&provider, &s, "PAY-SC-1", 1000).await;
    // Cycle 1: open then WIN (releases the hold back to CASH_CLEARING).
    record(
        &provider,
        &s,
        "DSP-SC-1",
        "PAY-SC-1",
        None,
        1,
        DisputePhase::Opened,
        FundsAtOpen::Withheld,
        1000,
    )
    .await
    .expect("opened cycle 1");
    record(
        &provider,
        &s,
        "DSP-SC-1",
        "PAY-SC-1",
        None,
        1,
        DisputePhase::Won,
        FundsAtOpen::Withheld,
        1000,
    )
    .await
    .expect("won cycle 1");
    // Re-open as cycle 2 (a fresh OPENED on the same dispute).
    record(
        &provider,
        &s,
        "DSP-SC-1",
        "PAY-SC-1",
        None,
        2,
        DisputePhase::Opened,
        FundsAtOpen::Withheld,
        1000,
    )
    .await
    .expect("re-opened cycle 2");
    assert_eq!(
        dispute_row(&raw, &s, "DSP-SC-1").await,
        Some(("CASH_HOLD".to_owned(), "OPENED".to_owned(), 2)),
        "the dispute is OPENED at cycle 2 before the stale outcome"
    );

    // The stale cycle-1 `lost` clears dedup (DSP:1:lost never posted) and the
    // out-of-txn guard (the row is OPENED) — only the cycle predicate rejects it.
    let err = record(
        &provider,
        &s,
        "DSP-SC-1",
        "PAY-SC-1",
        None,
        1,
        DisputePhase::Lost,
        FundsAtOpen::Withheld,
        1000,
    )
    .await
    .expect_err("a stale-cycle outcome must be rejected");
    assert!(
        matches!(err, DomainError::InvalidDisputeTransition(_)),
        "stale-cycle outcome must be InvalidDisputeTransition, got {err:?}"
    );

    // The cycle-2 row is UNCHANGED — still OPENED at cycle 2, not rewritten to
    // LOST cycle 1 by the stale outcome.
    assert_eq!(
        dispute_row(&raw, &s, "DSP-SC-1").await,
        Some(("CASH_HOLD".to_owned(), "OPENED".to_owned(), 2)),
        "the stale outcome left the current open cycle untouched"
    );
}

/// Regression (cross-currency dispute, H2): a CASH_HOLD chargeback sizes its net
/// cash leg off the SETTLED payment's counters and posts in the request currency,
/// so a request whose currency differs from the settlement (a mistyped or
/// malicious EUR dispute on a USD payment) must be rejected as `CurrencyMismatch`
/// before any leg is sized or posted — mirrors the allocation currency gate.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn cash_hold_chargeback_in_wrong_currency_is_rejected() {
    let (_c, raw, provider) = boot().await;
    let s = setup_seller(&raw, &provider).await;

    settle(&provider, &s, "PAY-XC-1", 1000).await; // settled in USD
    let err = chargeback_svc(&provider)
        .record_phase(
            &SecurityContext::anonymous(),
            &AccessScope::for_tenant(s.tenant),
            ChargebackRequest {
                tenant_id: s.tenant,
                payer_tenant_id: s.payer,
                payment_id: "PAY-XC-1".to_owned(),
                dispute_id: "DSP-XC-1".to_owned(),
                invoice_id: None,
                cycle: 1,
                phase: DisputePhase::Opened,
                funds_at_open: FundsAtOpen::Withheld,
                disputed_amount_minor: 1000,
                currency: "EUR".to_owned(), // != settled USD
                effective_at: None,
            },
        )
        .await
        .expect_err("a cross-currency chargeback must be rejected");
    assert!(
        matches!(err, DomainError::CurrencyMismatch(_)),
        "expected CurrencyMismatch, got {err:?}"
    );
    // Nothing persisted: no dispute row was seeded.
    assert_eq!(
        dispute_row(&raw, &s, "DSP-XC-1").await,
        None,
        "a rejected cross-currency dispute seeds no row"
    );
}

/// Regression — cross-feature stranded hold: a settlement-return that lowers
/// a payment's settled total AFTER a CASH_HOLD dispute opened must NOT change what
/// the outcome releases. The held cash is a fact fixed at open
/// (`cash_hold_minor`), so the `won` outcome releases the FULL amount held — not a
/// re-read `settled − fee`. Pre-fix the outcome re-read the now-lower net and
/// released too little, stranding cash in DISPUTE_HOLD.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn won_cash_hold_after_settlement_return_releases_full_held_amount() {
    let (_c, raw, provider) = boot().await;
    let s = setup_seller(&raw, &provider).await;

    // Two settled receipts fund the shared CASH_CLEARING pool so the return on
    // PAY-A can credit clearing back (PAY-A's own net is parked in the hold).
    settle(&provider, &s, "PAY-A", 1000).await;
    settle(&provider, &s, "PAY-B", 1000).await;

    // Open a CASH_HOLD dispute on PAY-A: the full net (1000, fee 0) parks in the
    // hold (CASH_CLEARING 2000 → 1000, DISPUTE_HOLD → 1000).
    recorded(
        record(
            &provider,
            &s,
            "DISP-A",
            "PAY-A",
            None,
            1,
            DisputePhase::Opened,
            FundsAtOpen::Withheld,
            1000,
        )
        .await
        .unwrap(),
    );
    assert_eq!(
        account_balance(&raw, &s, s.dispute_hold).await,
        Some(1000),
        "open parks the full net in the hold"
    );

    // Partial settlement-return on PAY-A AFTER the dispute opened: its settled/net
    // drops 1000 → 500 (CASH_CLEARING 1000 → 500). Pre-fix the won outcome would
    // then re-read net=500 and release only that.
    return_settlement(&provider, &s, "PAY-A", "RET-A", 500).await;
    assert_eq!(
        account_balance(&raw, &s, s.cash).await,
        Some(500),
        "the partial return credits clearing back down to 500"
    );

    // Win the dispute: the hold releases the amount HELD at open (1000), sized off
    // the stored `cash_hold_minor` — not the now-lower net.
    recorded(
        record(
            &provider,
            &s,
            "DISP-A",
            "PAY-A",
            None,
            1,
            DisputePhase::Won,
            FundsAtOpen::Withheld,
            1000,
        )
        .await
        .unwrap(),
    );

    // The hold is fully released (pre-fix: 500 stranded), and clearing is restored
    // to the consistent 1500 = 2000 settled − 500 returned.
    assert_eq!(
        account_balance(&raw, &s, s.dispute_hold).await,
        Some(0),
        "won releases the full held amount; DISPUTE_HOLD is not stranded"
    );
    assert_eq!(
        account_balance(&raw, &s, s.cash).await,
        Some(1500),
        "clearing restored by the full held net; books consistent (2000 − 500)"
    );
}

// ── partial cash-hold (disputed < net) ───────────────────────────────────────

/// A buyer disputes only PART of a settled receipt: settle 1000 (CASH_CLEARING =
/// 1000, fee 0 ⇒ net = 1000), then `opened` a CASH_HOLD dispute for only 600. The
/// orchestrator sizes the hold at `cash_hold_minor = min(disputed, net) =
/// min(600, 1000) = 600` (the `min` branch the all-or-nothing 1000-disputed cases
/// never exercise), so only 600 moves CASH_CLEARING → DISPUTE_HOLD and 400 of
/// collected cash stays in clearing. `won` then releases exactly the 600 held back
/// (CASH_CLEARING → 1000); a partial dispute leaves the un-disputed remainder
/// untouched throughout.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn opened_partial_cash_hold_parks_only_disputed_slice() {
    let (_c, raw, provider) = boot().await;
    let s = setup_seller(&raw, &provider).await;

    // Settle 1000: CASH_CLEARING holds the full net 1000.
    settle(&provider, &s, "PAY-PCH-1", 1000).await;
    assert_eq!(
        account_balance(&raw, &s, s.cash).await,
        Some(1000),
        "settle lands the full net in CASH_CLEARING"
    );

    // Dispute only 600 of the 1000: the hold is sized at min(600, 1000) = 600.
    recorded(
        record(
            &provider,
            &s,
            "DSP-PCH-1",
            "PAY-PCH-1",
            None,
            1,
            DisputePhase::Opened,
            FundsAtOpen::Withheld,
            600,
        )
        .await
        .expect("partial opened cash-hold must post"),
    );
    assert_eq!(
        account_balance(&raw, &s, s.dispute_hold).await,
        Some(600),
        "DISPUTE_HOLD holds only the 600 disputed slice (min(disputed, net))"
    );
    assert_eq!(
        account_balance(&raw, &s, s.cash).await,
        Some(400),
        "CASH_CLEARING keeps the 400 un-disputed remainder (1000 − 600)"
    );
    assert_eq!(
        dispute_row(&raw, &s, "DSP-PCH-1").await,
        Some(("CASH_HOLD".to_owned(), "OPENED".to_owned(), 1)),
        "dispute row: variant=CASH_HOLD, last_phase=OPENED"
    );

    // Win the partial dispute: the 600 held is released back, leaving CASH_CLEARING
    // restored to the full 1000 (the 400 was never disputed).
    recorded(
        record(
            &provider,
            &s,
            "DSP-PCH-1",
            "PAY-PCH-1",
            None,
            1,
            DisputePhase::Won,
            FundsAtOpen::Withheld,
            600,
        )
        .await
        .expect("partial won cash-hold must post"),
    );
    assert_eq!(
        account_balance(&raw, &s, s.dispute_hold).await,
        Some(0),
        "the 600 hold is fully released on won"
    );
    assert_eq!(
        account_balance(&raw, &s, s.cash).await,
        Some(1000),
        "CASH_CLEARING restored to the full collected 1000 (only the slice round-tripped)"
    );
    // A won claws nothing back.
    assert_eq!(
        clawed_back(&provider, &s, "PAY-PCH-1").await,
        0,
        "a won claws nothing back"
    );
}

// ── lost cash-hold on an already-refunded payment → CHARGEBACK_ON_REFUNDED ────

/// `guard_not_on_refunded` real-path (chargeback.rs L1138-1199): a `lost`
/// cash-hold whose clawback cannot fit under the total money-out cap BECAUSE the
/// payment was already (partially) refunded routes to the minimal exception stub —
/// logged + `ChargebackOnRefunded` — instead of a generic `ChargebackExceedsSettled`.
///
/// Scenario (a real PSP sequence): a 1000 receipt is settled, then a 600 refund
/// already landed against it (`refunded_minor = 600`, seeded directly on the
/// settlement counter — the refund path's own write — exactly as
/// `postgres_payments.rs::bump_allocation_refund_nets_and_caps` drives a counter to
/// a target state). A CASH_HOLD dispute then opens for the full 1000 (hold = 1000)
/// and is LOST: the clawback (1000) would push `refunded(600) + clawed(0) +
/// clawback(1000) = 1600 > settled(1000)` over the cap, AND `refunded_minor > 0`,
/// so the pre-check raises `ChargebackOnRefunded`. The post never reaches the
/// engine: the dispute stays OPENED, DISPUTE_HOLD keeps the held 1000 (the loss did
/// not post), and `clawed_back_minor` stays 0.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn lost_cash_hold_on_refunded_payment_routes_to_exception() {
    let (_c, raw, provider) = boot().await;
    let s = setup_seller(&raw, &provider).await;

    // Settle 1000, open a CASH_HOLD dispute for the full 1000 (hold = 1000).
    settle(&provider, &s, "PAY-CBR-1", 1000).await;
    recorded(
        record(
            &provider,
            &s,
            "DSP-CBR-1",
            "PAY-CBR-1",
            None,
            1,
            DisputePhase::Opened,
            FundsAtOpen::Withheld,
            1000,
        )
        .await
        .expect("opened cash-hold must post"),
    );
    assert_eq!(
        account_balance(&raw, &s, s.dispute_hold).await,
        Some(1000),
        "the full net is held at open"
    );

    // A 600 refund already landed on this payment: bump `refunded_minor` to 600 on
    // the settlement counter (the refund path's own write; seeded directly so this
    // test owns the dispute lifecycle, mirroring the counter-seed idiom). 600 + 0
    // <= 1000 still satisfies the money-out cap, so the seed itself is admissible.
    raw.execute_raw(pg(format!(
        "UPDATE bss.ledger_payment_settlement SET refunded_minor = 600 \
         WHERE tenant_id='{}' AND payment_id='PAY-CBR-1'",
        s.tenant
    )))
    .await
    .expect("seeding a partial refund (within the cap) must be admissible");

    // Lose the dispute: clawback 1000 cannot fit (600 + 1000 > 1000) AND the
    // payment was refunded ⇒ ChargebackOnRefunded (the specific already-refunded
    // signal), not the generic ChargebackExceedsSettled.
    let err = record(
        &provider,
        &s,
        "DSP-CBR-1",
        "PAY-CBR-1",
        None,
        1,
        DisputePhase::Lost,
        FundsAtOpen::Withheld,
        1000,
    )
    .await
    .expect_err("a lost on an already-refunded payment whose clawback can't fit must be rejected");
    assert!(
        matches!(err, DomainError::ChargebackOnRefunded(_)),
        "expected ChargebackOnRefunded (the already-refunded route), got {err:?}"
    );

    // The pre-check rejected BEFORE the post: the loss never booked. The dispute is
    // still OPENED (not advanced to LOST), DISPUTE_HOLD still holds the 1000, no
    // DISPUTE_LOSS_EXPENSE, and clawed_back_minor stays 0.
    assert_eq!(
        dispute_row(&raw, &s, "DSP-CBR-1").await.map(|r| r.1),
        Some("OPENED".to_owned()),
        "the rejected lost left the dispute OPENED (the outcome never advanced it)"
    );
    assert_eq!(
        account_balance(&raw, &s, s.dispute_hold).await,
        Some(1000),
        "the held cash is untouched (the forfeit never posted)"
    );
    assert!(
        matches!(
            account_balance(&raw, &s, s.dispute_loss).await,
            None | Some(0)
        ),
        "no DISPUTE_LOSS_EXPENSE was booked"
    );
    assert_eq!(
        clawed_back(&provider, &s, "PAY-CBR-1").await,
        0,
        "nothing was clawed back (the cap pre-check rejected first)"
    );
}
