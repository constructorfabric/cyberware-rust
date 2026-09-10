//! Postgres-only repo-level tests for `PaymentRepo` (the payment counter
//! tables + the allocation candidate / view reads). Ignored by default; run
//! with `cargo test -p cf-gears-bss-ledger --test postgres_payments -- --ignored`.
//!
//! Covers: (a) `seed_settlement` then `read_settlement` round-trips
//! `settled_minor`; (b) two `add_allocated` calls net; (c) `add_allocated`
//! past `settled_minor` trips the cap CHECK → `MoneyOutCapExceeded`;
//! (d) `insert_allocation_rows` then `list_payment_allocations` returns N
//! rows; (e) `list_open_ar_invoices` filters `balance_minor > 0` and orders
//! `original_posted_at, invoice_id`, under the tenant's own scope; (f)
//! `bump_allocation_refund` nets and its refund-vs-allocated CHECK is enforced.
//!
//! Every read a foreign tenant must not see is in
//! `payment_grains_are_invisible_to_a_foreign_tenant_scope`, paired with a
//! positive control: a negative arm over an empty seed proves nothing.

#![allow(
    clippy::non_ascii_literal,
    clippy::let_underscore_must_use,
    clippy::needless_collect,
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::doc_markdown,
    clippy::panic,
    clippy::needless_pass_by_value
)]

use std::sync::Arc;

use bss_ledger::domain::error::DomainError;
use bss_ledger::domain::instant::format_rfc3339;
use bss_ledger::domain::instant::to_naive_date;
use bss_ledger::domain::model::{AccountRow, CurrencyScaleRow, NewEntry, NewLine, RepoError};
use bss_ledger::domain::money::DEFAULT_PLAUSIBLE_MAX_MAJOR;
use bss_ledger::domain::payment::precedence::Allocated;
use bss_ledger::domain::payment::settlement::SettlementInput;
use bss_ledger::domain::ports::metrics::NoopLedgerMetrics;
use bss_ledger::infra::events::publisher::LedgerEventPublisher;
use bss_ledger::infra::payment::allocate::{
    AllocateRequest, AllocationOutcome, AllocationService, AppliedAllocation,
};
use bss_ledger::infra::payment::settle::SettlementService;
use bss_ledger::infra::posting::service::PostingService;
use bss_ledger::infra::storage::migrations::Migrator;
use bss_ledger::infra::storage::repo::payment_repo::NewAllocationRow;
use bss_ledger::infra::storage::repo::{PaymentRepo, ReferenceRepo};
use bss_ledger_sdk::{AccountClass, MappingStatus, Side, SourceDocType};
use chrono::NaiveDate;
use sea_orm::{ConnectionTrait, Database, DbErr, Statement};
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

/// Lift a component `RepoError` into a `DbError` so the repo write can be the
/// transaction's typed success value (`T`), surviving COMMIT (mirrors
/// `postgres_idempotency.rs`).
fn lift(e: RepoError) -> DbError {
    DbError::Sea(DbErr::Custom(e.to_string()))
}

/// Boot a container, run the chain on a raw connection, and return a
/// `bss`-search-path `DBProvider` for the repo (the provisioning-test idiom).
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

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn seed_then_read_settlement() {
    let (_c, _raw, provider) = boot().await;
    let tenant = Uuid::now_v7();
    let payment_id = "psp-1";
    let scope = AccessScope::allow_all();

    provider
        .transaction(|txn| {
            let scope = scope.clone();
            Box::pin(async move {
                PaymentRepo::seed_settlement(txn, &scope, tenant, payment_id, "USD", 1000, 0)
                    .await
                    .map_err(lift)
            })
        })
        .await
        .expect("seed settlement");

    let repo = PaymentRepo::new(provider.clone());
    let row = repo
        .read_settlement(&scope, tenant, payment_id)
        .await
        .expect("read settlement")
        .expect("settlement row present");
    assert_eq!(row.settled_minor, 1000);
    assert_eq!(row.allocated_minor, 0);
    assert_eq!(row.currency, "USD");
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn add_allocated_nets_and_caps() {
    let (_c, _raw, provider) = boot().await;
    let tenant = Uuid::now_v7();
    let payment_id = "psp-2";
    let scope = AccessScope::allow_all();

    // Seed settled=1000.
    provider
        .transaction(|txn| {
            let scope = scope.clone();
            Box::pin(async move {
                PaymentRepo::seed_settlement(txn, &scope, tenant, payment_id, "USD", 1000, 0)
                    .await
                    .map_err(lift)
            })
        })
        .await
        .expect("seed");

    // Two increments net to 500.
    for delta in [300_i64, 200_i64] {
        provider
            .transaction(|txn| {
                let scope = scope.clone();
                Box::pin(async move {
                    PaymentRepo::add_allocated(txn, &scope, tenant, payment_id, delta)
                        .await
                        .map_err(lift)
                })
            })
            .await
            .expect("add allocated");
    }

    let repo = PaymentRepo::new(provider.clone());
    let row = repo
        .read_settlement(&scope, tenant, payment_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.allocated_minor, 500, "300 + 200 nets to 500");

    // A third increment of 600 would push allocated to 1100 > settled 1000 →
    // the cap CHECK rejects it; the repo maps it to MoneyOutCapExceeded.
    let err = provider
        .transaction(|txn| {
            let scope = scope.clone();
            Box::pin(async move {
                PaymentRepo::add_allocated(txn, &scope, tenant, payment_id, 600)
                    .await
                    .map_err(lift)
            })
        })
        .await
        .expect_err("over-cap allocate must be rejected");
    assert!(
        err.to_string().contains("money-out cap exceeded"),
        "expected MoneyOutCapExceeded, got: {err}"
    );

    // The rejected increment rolled back: allocated stays 500.
    let row = repo
        .read_settlement(&scope, tenant, payment_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.allocated_minor, 500, "over-cap increment rolled back");
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn insert_then_list_allocations() {
    let (_c, _raw, provider) = boot().await;
    let tenant = Uuid::now_v7();
    let payer = Uuid::now_v7();
    let payment_id = "psp-3";
    let allocation_id = Uuid::now_v7();
    let scope = AccessScope::allow_all();

    let rows = vec![
        NewAllocationRow {
            tenant_id: tenant,
            allocation_id,
            payer_tenant_id: payer,
            payment_id: payment_id.to_owned(),
            invoice_id: "inv-a".to_owned(),
            amount_minor: 300,
            currency: "USD".to_owned(),
            precedence_policy_ref: "oldest-first.v1".to_owned(),
            allocated_at_utc: OffsetDateTime::now_utc(),
        },
        NewAllocationRow {
            tenant_id: tenant,
            allocation_id,
            payer_tenant_id: payer,
            payment_id: payment_id.to_owned(),
            invoice_id: "inv-b".to_owned(),
            amount_minor: 200,
            currency: "USD".to_owned(),
            precedence_policy_ref: "oldest-first.v1".to_owned(),
            allocated_at_utc: OffsetDateTime::now_utc(),
        },
    ];

    provider
        .transaction(|txn| {
            let scope = scope.clone();
            let rows = rows;
            Box::pin(async move {
                PaymentRepo::insert_allocation_rows(txn, &scope, &rows)
                    .await
                    .map_err(lift)
            })
        })
        .await
        .expect("insert allocation rows");

    let repo = PaymentRepo::new(provider.clone());
    let listed = repo
        .list_payment_allocations(&scope, tenant, payment_id)
        .await
        .expect("list allocations");
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0].invoice_id, "inv-a", "ordered by invoice_id");
    assert_eq!(listed[0].amount_minor, 300);
    assert_eq!(listed[1].invoice_id, "inv-b");
    assert_eq!(listed[1].amount_minor, 200);
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn list_open_ar_invoices_filters_and_orders() {
    let (_c, raw, provider) = boot().await;
    let tenant = Uuid::now_v7();
    let payer = Uuid::now_v7();
    let account = Uuid::now_v7();
    // The tenant's own scope, not `allow_all()`: a wildcard scope makes the
    // SecureORM tenant predicate a no-op, so this — the only test of the AR
    // candidate read that drives every payment allocation — would have stayed
    // green with `.secure().scope_with(scope)` dropped from the reader. The
    // cross-tenant negative is in
    // `payment_grains_are_invisible_to_a_foreign_tenant_scope`.
    let scope = AccessScope::for_tenant(tenant);

    // Seed three AR-invoice cache rows: two open (different posted dates) and
    // one fully paid (balance 0, must be filtered out). inv-late posts AFTER
    // inv-early, so oldest-first puts inv-early first.
    let insert = |invoice: &str, balance: i64, posted: &str| {
        pg(format!(
            "INSERT INTO bss.ledger_ar_invoice_balance
                (tenant_id, payer_tenant_id, account_id, invoice_id, currency,
                 balance_minor, original_posted_at)
             VALUES ('{tenant}','{payer}','{account}','{invoice}','USD',
                     {balance}, '{posted}')"
        ))
    };
    raw.execute_raw(insert("inv-late", 800, "2026-02-01T00:00:00Z"))
        .await
        .unwrap();
    raw.execute_raw(insert("inv-early", 300, "2026-01-01T00:00:00Z"))
        .await
        .unwrap();
    raw.execute_raw(insert("inv-paid", 0, "2026-01-15T00:00:00Z"))
        .await
        .unwrap();

    let repo = PaymentRepo::new(provider.clone());
    let open = repo
        .list_open_ar_invoices(&scope, tenant, payer, "USD")
        .await
        .expect("list open ar invoices");
    assert_eq!(
        open.len(),
        2,
        "the paid (0-balance) invoice is filtered out"
    );
    assert_eq!(open[0].invoice_id, "inv-early", "oldest posted first");
    assert_eq!(open[0].balance_minor, 300);
    assert_eq!(open[1].invoice_id, "inv-late");
    assert_eq!(open[1].balance_minor, 800);
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn bump_allocation_refund_nets_and_caps() {
    let (_c, raw, provider) = boot().await;
    let tenant = Uuid::now_v7();
    let payment_id = "psp-4";
    let scope = AccessScope::allow_all();

    // Two bumps net the allocated counter to 500.
    for delta in [300_i64, 200_i64] {
        provider
            .transaction(|txn| {
                let scope = scope.clone();
                Box::pin(async move {
                    PaymentRepo::bump_allocation_refund(
                        txn, &scope, tenant, payment_id, "inv-a", delta,
                    )
                    .await
                    .map_err(lift)
                })
            })
            .await
            .expect("bump allocation refund");
    }

    let allocated: i64 = {
        let row = raw
            .query_one_raw(pg(format!(
                "SELECT allocated_minor FROM bss.ledger_payment_allocation_refund
                 WHERE tenant_id='{tenant}' AND payment_id='{payment_id}' AND invoice_id='inv-a'"
            )))
            .await
            .unwrap()
            .expect("refund row present");
        row.try_get("", "allocated_minor").unwrap()
    };
    assert_eq!(allocated, 500, "300 + 200 nets to 500");

    // Drive refunded_minor up to allocated (500) directly, then a further
    // refund would break refunded_minor <= allocated_minor — verifying the
    // CHECK exists on the table (the refund increment is Slice 3's path).
    raw.execute_raw(pg(format!(
        "UPDATE bss.ledger_payment_allocation_refund SET refunded_minor = 500
         WHERE tenant_id='{tenant}' AND payment_id='{payment_id}' AND invoice_id='inv-a'"
    )))
    .await
    .expect("refunded_minor = allocated_minor is allowed");
    let err = raw
        .execute_raw(pg(format!(
            "UPDATE bss.ledger_payment_allocation_refund SET refunded_minor = 600
             WHERE tenant_id='{tenant}' AND payment_id='{payment_id}' AND invoice_id='inv-a'"
        )))
        .await
        .expect_err("refunded_minor > allocated_minor must be rejected by CHECK");
    assert!(
        err.to_string().contains("chk_par_refunded_le_allocated"),
        "unexpected error: {err}"
    );
}

// ── D2/D3 service-level integration: SettlementService + AllocationService ───
//
// These drive the REAL payment orchestrators (`infra::payment::settle` /
// `::allocate`) through the foundation `PostingService` against the
// testcontainer Postgres: settle lands cash in UNALLOCATED (no AR move),
// allocate drains the pool into AR oldest-first under the per-payment cap, and
// both replay idempotently. `setup_seller` provisions the chart
// (CASH_CLEARING / UNALLOCATED / PSP_FEE_EXPENSE / AR), USD@2, and an OPEN
// fiscal period for the CURRENT month (settle/allocate derive `period_id` from
// `OffsetDateTime::now_utc()` when no `effective_at` is supplied).

/// Provisioned seller ids for the service tests.
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

/// Provision a seller: USD@2 scale, an OPEN fiscal period for the current
/// month, and the four payment-flow chart accounts (CASH_CLEARING debit,
/// UNALLOCATED credit, PSP_FEE_EXPENSE debit, AR debit). Reuses the file's
/// `boot()` for the container/provider.
async fn setup_seller(raw: &sea_orm::DatabaseConnection, provider: &DBProvider<DbError>) -> Seller {
    let now = OffsetDateTime::now_utc();
    let s = Seller {
        tenant: Uuid::now_v7(),
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

fn settle_svc(provider: &DBProvider<DbError>) -> SettlementService {
    SettlementService::new(
        provider.clone(),
        Arc::new(LedgerEventPublisher::noop()),
        Arc::new(NoopLedgerMetrics),
    )
}

fn allocate_svc(provider: &DBProvider<DbError>) -> AllocationService {
    AllocationService::new(
        provider.clone(),
        Arc::new(LedgerEventPublisher::noop()),
        Arc::new(NoopLedgerMetrics),
    )
}

/// Unwrap the inline-post arm of an allocate outcome. Every service-level test
/// here settles BEFORE allocating, so the outcome is always `Applied` (the
/// `Queued` arm is the not-yet-settled §4.7 path, exercised separately); a
/// `Queued` here is a test-setup bug, so panic.
fn applied(outcome: AllocationOutcome) -> AppliedAllocation {
    match outcome {
        AllocationOutcome::Applied(a) => a,
        AllocationOutcome::Queued(q) => {
            panic!("expected an inline-posted allocation, got Queued: {q:?}")
        }
    }
}

fn settlement_input(s: &Seller, payment_id: &str, gross: i64, fee: i64) -> SettlementInput {
    SettlementInput {
        tenant_id: s.tenant,
        payer_tenant_id: s.payer,
        payment_id: payment_id.to_owned(),
        gross_minor: gross,
        fee_minor: fee,
        currency: "USD".to_owned(),
        // None ⇒ the orchestrator stamps a current-month effective date /
        // period (matching the OPEN period `setup_seller` provisions).
        effective_at: None,
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

async fn count_allocations(raw: &sea_orm::DatabaseConnection, s: &Seller, payment_id: &str) -> i64 {
    raw.query_one_raw(pg(format!(
        "SELECT COUNT(*) FROM bss.ledger_payment_allocation \
         WHERE tenant_id='{}' AND payment_id='{}'",
        s.tenant, payment_id
    )))
    .await
    .unwrap()
    .map_or(0, |r| r.try_get_by_index::<i64>(0).unwrap())
}

async fn allocation_refund(
    raw: &sea_orm::DatabaseConnection,
    s: &Seller,
    payment_id: &str,
    invoice_id: &str,
) -> Option<i64> {
    raw.query_one_raw(pg(format!(
        "SELECT allocated_minor FROM bss.ledger_payment_allocation_refund \
         WHERE tenant_id='{}' AND payment_id='{}' AND invoice_id='{}'",
        s.tenant, payment_id, invoice_id
    )))
    .await
    .unwrap()
    .map(|r| r.try_get_by_index::<i64>(0).unwrap())
}

/// Seed an OPEN AR invoice by posting a balanced `DR AR (invoice_id) / CR
/// PSP_FEE_EXPENSE` directly through the engine. PSP_FEE_EXPENSE is unguarded
/// (a CR from zero is allowed) and carries no per-line CHECK, so this lands a
/// clean `ar_invoice_balance` row with `original_posted_at = posted_at` (the
/// oldest-first sort key). `posted_at` is supplied explicitly so the test
/// controls the ordering deterministically.
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
    let entry_id = Uuid::now_v7();
    let entry = NewEntry {
        entry_id,
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

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn settle_lands_cash_in_unallocated_and_replays() {
    let (_c, raw, provider) = boot().await;
    let s = setup_seller(&raw, &provider).await;
    let service = settle_svc(&provider);
    let ctx = SecurityContext::anonymous();
    let scope = AccessScope::for_tenant(s.tenant);

    // Settle gross=1000, fee=30 ⇒ DR CASH 970 / DR PSP_FEE 30 / CR UNALLOCATED 1000.
    let posted = service
        .settle(&ctx, &scope, settlement_input(&s, "PAY-SET-1", 1000, 30))
        .await
        .expect("settle must succeed");
    assert!(!posted.replayed, "first settle is fresh");

    // The payment_settlement counter row is seeded: settled=1000, allocated=0.
    let repo = PaymentRepo::new(provider.clone());
    let row = repo
        .read_settlement(&scope, s.tenant, "PAY-SET-1")
        .await
        .unwrap()
        .expect("settlement row present");
    assert_eq!(row.settled_minor, 1000);
    assert_eq!(row.allocated_minor, 0);
    assert_eq!(row.currency, "USD");

    // The whole gross parks in the payer's unallocated pool.
    assert_eq!(
        repo.read_unallocated(&scope, s.tenant, s.payer, "USD")
            .await
            .unwrap(),
        1000,
        "gross lands in UNALLOCATED"
    );
    // Net cash hit clearing; AR was untouched (a receipt does not move AR).
    assert_eq!(
        account_balance(&raw, &s, s.cash).await,
        Some(970),
        "CASH_CLEARING = net (gross - fee)"
    );
    assert_eq!(
        account_balance(&raw, &s, s.ar).await,
        None,
        "AR untouched by a settlement"
    );

    // Re-settle the SAME payment_id ⇒ idempotent replay, balances unchanged.
    let replay = service
        .settle(&ctx, &scope, settlement_input(&s, "PAY-SET-1", 1000, 30))
        .await
        .expect("re-settle replays");
    assert!(replay.replayed, "second settle of the same payment replays");
    assert_eq!(replay.entry_id, posted.entry_id, "replay returns prior id");
    assert_eq!(
        repo.read_unallocated(&scope, s.tenant, s.payer, "USD")
            .await
            .unwrap(),
        1000,
        "UNALLOCATED unchanged on replay"
    );
    assert_eq!(
        account_balance(&raw, &s, s.cash).await,
        Some(970),
        "CASH_CLEARING unchanged on replay"
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn allocate_oldest_first_drains_unallocated_into_ar() {
    let (_c, raw, provider) = boot().await;
    let s = setup_seller(&raw, &provider).await;
    let ctx = SecurityContext::anonymous();
    let scope = AccessScope::for_tenant(s.tenant);

    // Settle 1000 into the pool.
    settle_svc(&provider)
        .settle(&ctx, &scope, settlement_input(&s, "PAY-ALLOC-1", 1000, 0))
        .await
        .expect("settle");

    // Two open AR invoices: INV-A (300) posted earlier, INV-B (800) later. The
    // explicit posted_at + lexical id both put INV-A first under oldest-first.
    let earlier = OffsetDateTime::now_utc() - time::Duration::hours(2);
    let later = OffsetDateTime::now_utc() - time::Duration::hours(1);
    seed_ar_invoice(&provider, &s, "INV-A", 300, earlier).await;
    seed_ar_invoice(&provider, &s, "INV-B", 800, later).await;

    // Allocate a lump of 500: INV-A fills (300), INV-B gets the remaining 200.
    let outcome = allocate_svc(&provider)
        .allocate(
            &ctx,
            &scope,
            AllocateRequest {
                tenant_id: s.tenant,
                payer_tenant_id: s.payer,
                payment_id: "PAY-ALLOC-1".to_owned(),
                allocation_id: Uuid::now_v7(),
                lump_minor: 500,
                currency: "USD".to_owned(),
                hint_invoice_id: None,
                caller_splits: None,
            },
        )
        .await
        .expect("allocate must succeed");
    let outcome = applied(outcome);
    assert!(!outcome.posting.replayed, "first allocate is fresh");
    let splits: Vec<(String, i64)> = outcome
        .splits
        .iter()
        .map(|a| (a.invoice_id.clone(), a.amount_minor))
        .collect();
    assert_eq!(
        splits,
        vec![("INV-A".to_owned(), 300), ("INV-B".to_owned(), 200)],
        "oldest-first fills INV-A then INV-B"
    );

    // AR drained: INV-A fully paid (0), INV-B down to 600.
    assert_eq!(ar_invoice_balance(&raw, &s, "INV-A").await, Some(0));
    assert_eq!(ar_invoice_balance(&raw, &s, "INV-B").await, Some(600));

    // allocated_minor netted to 500; two payment_allocation rows; refunds 300/200.
    let repo = PaymentRepo::new(provider.clone());
    let row = repo
        .read_settlement(&scope, s.tenant, "PAY-ALLOC-1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        row.allocated_minor, 500,
        "allocated nets to the applied total"
    );
    assert_eq!(count_allocations(&raw, &s, "PAY-ALLOC-1").await, 2);
    assert_eq!(
        allocation_refund(&raw, &s, "PAY-ALLOC-1", "INV-A").await,
        Some(300)
    );
    assert_eq!(
        allocation_refund(&raw, &s, "PAY-ALLOC-1", "INV-B").await,
        Some(200)
    );

    // The pool drained by exactly the applied total (1000 - 500 = 500 left).
    assert_eq!(
        repo.read_unallocated(&scope, s.tenant, s.payer, "USD")
            .await
            .unwrap(),
        500,
        "UNALLOCATED drained by the allocated total"
    );
}

/// Mode B (§4.4 F-5): a valid caller-computed split SKIPS precedence and posts
/// the EXACT caller amounts (not the oldest-first decision). INV-A is older but
/// the caller deliberately pays INV-B more — the split is applied verbatim, the
/// named invoices' AR drops by those amounts, and the rows record the
/// `caller-split.v1` policy ref (not `oldest-first.v1`).
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn caller_split_posts_exact_amounts_and_reduces_ar() {
    let (_c, raw, provider) = boot().await;
    let s = setup_seller(&raw, &provider).await;
    let ctx = SecurityContext::anonymous();
    let scope = AccessScope::for_tenant(s.tenant);

    settle_svc(&provider)
        .settle(&ctx, &scope, settlement_input(&s, "PAY-CS-1", 1000, 0))
        .await
        .expect("settle");
    let earlier = OffsetDateTime::now_utc() - time::Duration::hours(2);
    let later = OffsetDateTime::now_utc() - time::Duration::hours(1);
    seed_ar_invoice(&provider, &s, "INV-A", 300, earlier).await;
    seed_ar_invoice(&provider, &s, "INV-B", 800, later).await;

    // Oldest-first would fill INV-A (300) then INV-B (200). The caller instead
    // pays INV-B 400 and INV-A 100 — a different, deliberate split.
    let outcome = allocate_svc(&provider)
        .allocate(
            &ctx,
            &scope,
            AllocateRequest {
                tenant_id: s.tenant,
                payer_tenant_id: s.payer,
                payment_id: "PAY-CS-1".to_owned(),
                allocation_id: Uuid::now_v7(),
                lump_minor: 500,
                currency: "USD".to_owned(),
                hint_invoice_id: None,
                caller_splits: Some(vec![
                    Allocated {
                        invoice_id: "INV-B".to_owned(),
                        amount_minor: 400,
                    },
                    Allocated {
                        invoice_id: "INV-A".to_owned(),
                        amount_minor: 100,
                    },
                ]),
            },
        )
        .await
        .expect("caller split must succeed");
    let outcome = applied(outcome);
    assert!(!outcome.posting.replayed, "first allocate is fresh");
    // The split is the caller's, in the caller's order — not the oldest-first
    // decision.
    let splits: Vec<(String, i64)> = outcome
        .splits
        .iter()
        .map(|a| (a.invoice_id.clone(), a.amount_minor))
        .collect();
    assert_eq!(
        splits,
        vec![("INV-B".to_owned(), 400), ("INV-A".to_owned(), 100)],
        "caller split applied verbatim"
    );
    assert_eq!(
        outcome.policy_ref, "caller-split.v1",
        "Mode B stamps the caller-split audit ref"
    );

    // AR dropped by exactly the caller amounts: INV-A 300→200, INV-B 800→400.
    assert_eq!(ar_invoice_balance(&raw, &s, "INV-A").await, Some(200));
    assert_eq!(ar_invoice_balance(&raw, &s, "INV-B").await, Some(400));

    // Two rows, both stamped caller-split.v1; allocated nets to 500.
    assert_eq!(count_allocations(&raw, &s, "PAY-CS-1").await, 2);
    let repo = PaymentRepo::new(provider.clone());
    let rows = repo
        .list_payment_allocations(&scope, s.tenant, "PAY-CS-1")
        .await
        .unwrap();
    assert!(
        rows.iter()
            .all(|r| r.precedence_policy_ref == "caller-split.v1"),
        "persisted rows carry the caller-split ref: {rows:?}"
    );
    let settlement = repo
        .read_settlement(&scope, s.tenant, "PAY-CS-1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(settlement.allocated_minor, 500);
}

/// Mode B: a caller split that over-allocates an invoice past its open balance
/// is rejected `AllocationSplitInvalid` BEFORE any post — no AR change, no rows.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn caller_split_over_open_is_rejected() {
    let (_c, raw, provider) = boot().await;
    let s = setup_seller(&raw, &provider).await;
    let ctx = SecurityContext::anonymous();
    let scope = AccessScope::for_tenant(s.tenant);

    settle_svc(&provider)
        .settle(&ctx, &scope, settlement_input(&s, "PAY-CS-OVR", 1000, 0))
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

    // 400 > INV-A's open 300 ⇒ AllocationSplitInvalid (the lump (1000) is ample,
    // so the per-invoice open cap is what trips).
    let err = allocate_svc(&provider)
        .allocate(
            &ctx,
            &scope,
            AllocateRequest {
                tenant_id: s.tenant,
                payer_tenant_id: s.payer,
                payment_id: "PAY-CS-OVR".to_owned(),
                allocation_id: Uuid::now_v7(),
                lump_minor: 1000,
                currency: "USD".to_owned(),
                hint_invoice_id: None,
                caller_splits: Some(vec![Allocated {
                    invoice_id: "INV-A".to_owned(),
                    amount_minor: 400,
                }]),
            },
        )
        .await
        .expect_err("an over-open caller split must be rejected");
    assert!(
        matches!(err, DomainError::AllocationSplitInvalid(_)),
        "expected AllocationSplitInvalid, got {err:?}"
    );

    // Rejected before the post: INV-A untouched, no rows, allocated stays 0.
    assert_eq!(ar_invoice_balance(&raw, &s, "INV-A").await, Some(300));
    assert_eq!(count_allocations(&raw, &s, "PAY-CS-OVR").await, 0);
    let row = PaymentRepo::new(provider.clone())
        .read_settlement(&scope, s.tenant, "PAY-CS-OVR")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        row.allocated_minor, 0,
        "rejected caller split left no effect"
    );
}

/// Mode B is idempotent on `allocation_id` just like the precedence path: a
/// replay of the same id returns the prior posting and writes no duplicate rows
/// / no double-counted `allocated_minor`.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn caller_split_replay_makes_no_duplicate_rows() {
    let (_c, raw, provider) = boot().await;
    let s = setup_seller(&raw, &provider).await;
    let ctx = SecurityContext::anonymous();
    let scope = AccessScope::for_tenant(s.tenant);

    settle_svc(&provider)
        .settle(&ctx, &scope, settlement_input(&s, "PAY-CS-RPL", 1000, 0))
        .await
        .expect("settle");
    // Seed open balance well above 2× the split (250): after the first
    // allocation drains it to 750 (>= 250), the replay's re-validation against
    // the now-drained candidate still admits the same split, so the request
    // reaches the engine and replays cleanly (vs a drained-below replay, which
    // is correctly rejected — that path is the over-open test's concern).
    seed_ar_invoice(
        &provider,
        &s,
        "INV-A",
        1000,
        OffsetDateTime::now_utc() - time::Duration::hours(1),
    )
    .await;

    let allocation_id = Uuid::now_v7();
    let request = || AllocateRequest {
        tenant_id: s.tenant,
        payer_tenant_id: s.payer,
        payment_id: "PAY-CS-RPL".to_owned(),
        allocation_id,
        lump_minor: 1000,
        currency: "USD".to_owned(),
        hint_invoice_id: None,
        caller_splits: Some(vec![Allocated {
            invoice_id: "INV-A".to_owned(),
            amount_minor: 250,
        }]),
    };

    let svc = allocate_svc(&provider);
    let first = applied(svc.allocate(&ctx, &scope, request()).await.expect("first"));
    assert!(!first.posting.replayed, "first is fresh");
    let second = applied(svc.allocate(&ctx, &scope, request()).await.expect("replay"));
    assert!(second.posting.replayed, "second is an idempotent replay");
    assert_eq!(
        first.posting.entry_id, second.posting.entry_id,
        "replay returns the prior entry"
    );

    // Exactly one allocation's worth of effect: one row, AR 1000→750, allocated 250.
    assert_eq!(count_allocations(&raw, &s, "PAY-CS-RPL").await, 1);
    assert_eq!(ar_invoice_balance(&raw, &s, "INV-A").await, Some(750));
    let row = PaymentRepo::new(provider.clone())
        .read_settlement(&scope, s.tenant, "PAY-CS-RPL")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        row.allocated_minor, 250,
        "allocated not double-counted on replay"
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn allocate_over_settled_cap_is_rejected() {
    let (_c, raw, provider) = boot().await;
    let s = setup_seller(&raw, &provider).await;
    let ctx = SecurityContext::anonymous();
    let scope = AccessScope::for_tenant(s.tenant);

    // Settle PAY-CAP-1 at only 100. Settle a SECOND payment (500) for the same
    // payer so the shared UNALLOCATED pool holds 600 — the no-negative guard on
    // UNALLOCATED is therefore NOT what trips. An allocate of 200 against
    // PAY-CAP-1 pushes ITS allocated_minor (200) past ITS settled_minor (100):
    // the per-payment cap CHECK in the sidecar is the authority that rejects it
    // with MoneyOutCapExceeded, even though the pool is positive (the design's
    // "blocks Σ-allocations > settled even when pooled unallocated is positive
    // from another payment" case). The whole post rolls back.
    let settle = settle_svc(&provider);
    settle
        .settle(&ctx, &scope, settlement_input(&s, "PAY-CAP-1", 100, 0))
        .await
        .expect("settle the capped payment");
    settle
        .settle(&ctx, &scope, settlement_input(&s, "PAY-OTHER", 500, 0))
        .await
        .expect("settle a second payment to fund the pool");
    seed_ar_invoice(
        &provider,
        &s,
        "INV-CAP",
        200,
        OffsetDateTime::now_utc() - time::Duration::hours(1),
    )
    .await;

    let err = allocate_svc(&provider)
        .allocate(
            &ctx,
            &scope,
            AllocateRequest {
                tenant_id: s.tenant,
                payer_tenant_id: s.payer,
                payment_id: "PAY-CAP-1".to_owned(),
                allocation_id: Uuid::now_v7(),
                lump_minor: 200,
                currency: "USD".to_owned(),
                hint_invoice_id: None,
                caller_splits: None,
            },
        )
        .await
        .expect_err("an over-cap allocate must be rejected");
    assert!(
        matches!(err, DomainError::MoneyOutCapExceeded(_)),
        "expected MoneyOutCapExceeded, got {err:?}"
    );

    // The rolled-back post left no effect: allocated stays 0, INV-CAP untouched.
    let repo = PaymentRepo::new(provider.clone());
    let row = repo
        .read_settlement(&scope, s.tenant, "PAY-CAP-1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.allocated_minor, 0, "over-cap allocate rolled back");
    assert_eq!(ar_invoice_balance(&raw, &s, "INV-CAP").await, Some(200));
    assert_eq!(count_allocations(&raw, &s, "PAY-CAP-1").await, 0);
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn allocate_currency_mismatch_is_rejected() {
    let (_c, raw, provider) = boot().await;
    let s = setup_seller(&raw, &provider).await;
    let ctx = SecurityContext::anonymous();
    let scope = AccessScope::for_tenant(s.tenant);

    // Settle in USD, then allocate in EUR ⇒ AllocationCurrencyMismatch, before
    // any candidate read or post.
    settle_svc(&provider)
        .settle(&ctx, &scope, settlement_input(&s, "PAY-CCY-1", 1000, 0))
        .await
        .expect("settle");

    let err = allocate_svc(&provider)
        .allocate(
            &ctx,
            &scope,
            AllocateRequest {
                tenant_id: s.tenant,
                payer_tenant_id: s.payer,
                payment_id: "PAY-CCY-1".to_owned(),
                allocation_id: Uuid::now_v7(),
                lump_minor: 500,
                currency: "EUR".to_owned(),
                hint_invoice_id: None,
                caller_splits: None,
            },
        )
        .await
        .expect_err("a currency-mismatched allocate must be rejected");
    assert!(
        matches!(err, DomainError::AllocationCurrencyMismatch(_)),
        "expected AllocationCurrencyMismatch, got {err:?}"
    );
}

/// Idempotency: two allocates of the SAME `allocation_id` (same payment, same
/// lump) racing on two service clones must land EXACTLY ONE ledger effect — the
/// `PAYMENT_ALLOCATE` dedup key `(tenant, allocation_id)` admits one winner; the
/// loser replays the winner's finalized entry. Exactly the original N
/// `payment_allocation` rows persist and `allocated_minor` is not double-counted.
///
/// Why concurrent (not sequential): this exercises the racing-claim / SSI path —
/// a concurrent pair both observe the same pre-allocation AR, freeze identical
/// entries (split derivation is outside the engine's retry loop), and the loser
/// replays the winner — the design's true at-most-once-per-`allocation_id`
/// guarantee (mirrors
/// `postgres_invoice_post::concurrent_same_invoice_posts_exactly_once`). A
/// *sequential* re-issue of the same request would ALSO be safe — the dedup hash
/// is request-based (split-independent), so `replay_short_circuit` returns the
/// prior POSTED entry cleanly — it just wouldn't test the contention.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn allocate_replay_makes_no_duplicate_rows() {
    let (_c, raw, provider) = boot().await;
    let s = setup_seller(&raw, &provider).await;
    let ctx = SecurityContext::anonymous();
    let scope = AccessScope::for_tenant(s.tenant);

    settle_svc(&provider)
        .settle(&ctx, &scope, settlement_input(&s, "PAY-RPL-1", 1000, 0))
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

    // The SAME allocation_id on both racing requests ⇒ the dedup key collides.
    let allocation_id = Uuid::now_v7();
    let request = || AllocateRequest {
        tenant_id: s.tenant,
        payer_tenant_id: s.payer,
        payment_id: "PAY-RPL-1".to_owned(),
        allocation_id,
        lump_minor: 500,
        currency: "USD".to_owned(),
        hint_invoice_id: None,
        caller_splits: None,
    };

    let svc_a = allocate_svc(&provider);
    let svc_b = allocate_svc(&provider);
    let scope_a = scope.clone();
    let scope_b = scope.clone();
    let ctx_a = SecurityContext::anonymous();
    let ctx_b = SecurityContext::anonymous();
    let (ra, rb) = tokio::join!(
        async move { svc_a.allocate(&ctx_a, &scope_a, request()).await },
        async move { svc_b.allocate(&ctx_b, &scope_b, request()).await },
    );

    // At least one side made progress; the loser replayed (or was rejected),
    // never a second ledger effect.
    let oks = i32::from(ra.is_ok()) + i32::from(rb.is_ok());
    assert!(
        oks >= 1,
        "at least one concurrent allocate must succeed: {ra:?} / {rb:?}"
    );

    // Exactly the original two payment_allocation rows persist for the key.
    assert_eq!(
        count_allocations(&raw, &s, "PAY-RPL-1").await,
        2,
        "the same allocation_id adds no duplicate allocation rows"
    );

    // allocated_minor reflects exactly one allocation (500), not 1000.
    let repo = PaymentRepo::new(provider.clone());
    let row = repo
        .read_settlement(&scope, s.tenant, "PAY-RPL-1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        row.allocated_minor, 500,
        "allocated_minor not double-counted across the racing pair"
    );

    // AR drained by exactly one allocation: INV-A 0, INV-B 600.
    assert_eq!(ar_invoice_balance(&raw, &s, "INV-A").await, Some(0));
    assert_eq!(ar_invoice_balance(&raw, &s, "INV-B").await, Some(600));
}

/// SQL-level BOLA on the new payment grains: rows seeded for tenant A are
/// invisible to a read carried out under a tenant-B `AccessScope`, EVEN when the
/// caller passes tenant A's id in the filter — the `#[secure(tenant_col)]`
/// predicate ANDs `tenant_id ∈ {B}`, so a foreign scope yields nothing. This is
/// the cross-tenant negative READ test the payment-grain coverage was missing; it
/// locks the isolation wiring against a future `#[secure]` regression.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn payment_grains_are_invisible_to_a_foreign_tenant_scope() {
    let (_c, raw, provider) = boot().await;
    let tenant_a = Uuid::now_v7();
    let tenant_b = Uuid::now_v7();
    let payer = Uuid::now_v7();
    let payment_id = "PAY-BOLA";
    let own = AccessScope::for_tenant(tenant_a);
    let foreign = AccessScope::for_tenant(tenant_b);

    // Seed a settlement + one allocation row for tenant A.
    let alloc_id = Uuid::now_v7();
    provider
        .transaction(|txn| {
            let own = own.clone();
            Box::pin(async move {
                PaymentRepo::seed_settlement(txn, &own, tenant_a, payment_id, "USD", 1000, 0)
                    .await
                    .map_err(lift)?;
                PaymentRepo::insert_allocation_rows(
                    txn,
                    &own,
                    &[NewAllocationRow {
                        tenant_id: tenant_a,
                        allocation_id: alloc_id,
                        payer_tenant_id: payer,
                        payment_id: payment_id.to_owned(),
                        invoice_id: "INV-A".to_owned(),
                        amount_minor: 1000,
                        currency: "USD".to_owned(),
                        precedence_policy_ref: "oldest-first.v1".to_owned(),
                        allocated_at_utc: OffsetDateTime::now_utc(),
                    }],
                )
                .await
                .map_err(lift)
            })
        })
        .await
        .expect("seed tenant A");

    // Seed the remaining scoped payment grains for tenant A via raw SQL (these
    // are projector caches / a policy table with no repo seed helper). They are
    // exactly the grains the first negative test missed; the wallet
    // (`reusable_credit_subbalance`) is the most sensitive, and the AR candidate
    // cache is the one every allocation starts from.
    let acct = Uuid::now_v7();
    let ts = format_rfc3339(OffsetDateTime::now_utc());
    raw.execute_raw(pg(format!(
        "INSERT INTO bss.ledger_reusable_credit_subbalance \
         (tenant_id, payer_tenant_id, account_id, currency, credit_grant_event_type, \
          first_granted_at, balance_minor, version) \
         VALUES ('{tenant_a}','{payer}','{acct}','USD','promo','{ts}',500,0)"
    )))
    .await
    .expect("seed wallet sub-grain");
    raw.execute_raw(pg(format!(
        "INSERT INTO bss.ledger_unallocated_balance \
         (tenant_id, payer_tenant_id, account_id, currency, balance_minor, version) \
         VALUES ('{tenant_a}','{payer}','{acct}','USD',700,0)"
    )))
    .await
    .expect("seed unallocated pool");
    raw.execute_raw(pg(format!(
        "INSERT INTO bss.ledger_tenant_precedence_policy \
         (tenant_id, version, effective_from, strategy, created_at_utc) \
         VALUES ('{tenant_a}',1,'{ts}','oldest-first.v1','{ts}')"
    )))
    .await
    .expect("seed precedence policy");
    // The AR candidate cache: `list_open_ar_invoices` is the reader every payment
    // allocation starts from, and until this row existed it was the one payment
    // entry point with no cross-tenant negative anywhere in the crate — its only
    // other test runs under a scope that makes the tenant predicate a no-op.
    raw.execute_raw(pg(format!(
        "INSERT INTO bss.ledger_ar_invoice_balance \
         (tenant_id, payer_tenant_id, account_id, invoice_id, currency, \
          balance_minor, original_posted_at) \
         VALUES ('{tenant_a}','{payer}','{acct}','INV-BOLA','USD',400,'{ts}')"
    )))
    .await
    .expect("seed ar candidate");

    let repo = PaymentRepo::new(provider.clone());

    // Tenant A sees its own rows.
    assert!(
        repo.read_settlement(&own, tenant_a, payment_id)
            .await
            .unwrap()
            .is_some(),
        "tenant A reads its own settlement"
    );
    assert_eq!(
        repo.list_payment_allocations(&own, tenant_a, payment_id)
            .await
            .unwrap()
            .len(),
        1,
        "tenant A reads its own allocation"
    );

    // A tenant-B scope sees NOTHING — even passing tenant A's id in the filter.
    assert!(
        repo.read_settlement(&foreign, tenant_a, payment_id)
            .await
            .unwrap()
            .is_none(),
        "a foreign scope cannot read tenant A's settlement (SQL-level BOLA)"
    );
    assert!(
        repo.list_payment_allocations(&foreign, tenant_a, payment_id)
            .await
            .unwrap()
            .is_empty(),
        "a foreign scope cannot read tenant A's allocations (SQL-level BOLA)"
    );

    // The remaining grains: own scope sees them; a foreign scope sees nothing.
    assert_eq!(
        repo.list_credit_subgrains(&own, tenant_a, payer, "USD")
            .await
            .unwrap()
            .len(),
        1,
        "tenant A reads its own reusable-credit wallet sub-grain"
    );
    assert!(
        repo.list_credit_subgrains(&foreign, tenant_a, payer, "USD")
            .await
            .unwrap()
            .is_empty(),
        "a foreign scope cannot read tenant A's wallet (SQL-level BOLA)"
    );
    assert_eq!(
        repo.read_unallocated(&own, tenant_a, payer, "USD")
            .await
            .unwrap(),
        700,
        "tenant A reads its own unallocated pool"
    );
    assert_eq!(
        repo.read_unallocated(&foreign, tenant_a, payer, "USD")
            .await
            .unwrap(),
        0,
        "a foreign scope reads zero unallocated for tenant A (SQL-level BOLA)"
    );
    assert!(
        repo.read_effective_policy(&own, tenant_a, OffsetDateTime::now_utc())
            .await
            .unwrap()
            .is_some(),
        "tenant A reads its own precedence policy"
    );
    assert!(
        repo.read_effective_policy(&foreign, tenant_a, OffsetDateTime::now_utc())
            .await
            .unwrap()
            .is_none(),
        "a foreign scope cannot read tenant A's precedence policy (SQL-level BOLA)"
    );
    assert_eq!(
        repo.list_open_ar_invoices(&own, tenant_a, payer, "USD")
            .await
            .unwrap()
            .len(),
        1,
        "tenant A reads its own open AR candidate"
    );
    assert!(
        repo.list_open_ar_invoices(&foreign, tenant_a, payer, "USD")
            .await
            .unwrap()
            .is_empty(),
        "a foreign scope cannot read tenant A's AR candidates (SQL-level BOLA)"
    );
}
