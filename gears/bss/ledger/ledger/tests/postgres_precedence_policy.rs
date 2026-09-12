//! Postgres-only integration tests for Group B — effective-dated precedence
//! policy versioning. Drives the REAL `AllocationService` through the
//! foundation `PostingService` against a testcontainer Postgres, exercising
//! `PaymentRepo::read_effective_policy` + the `allocate_inner` wiring:
//!
//! - a seeded `HighestAmountFirst` policy row makes the split follow
//!   largest-open-first AND stamps the real `highest-amount-first.v1#<version>`
//!   ref onto every `payment_allocation` row;
//! - with NO policy row the allocator falls back to oldest-first and the ref
//!   stays `DEFAULT_PRECEDENCE_POLICY` (byte-stable with 2a);
//! - with two versions at different `effective_from`, the one in effect now
//!   (latest `effective_from <= now`) is the one chosen.
//!
//! Ignored by default (Docker/testcontainers); run with
//! `cargo test -p cf-gears-bss-ledger --test postgres_precedence_policy -- --ignored`.

#![allow(
    clippy::non_ascii_literal,
    clippy::let_underscore_must_use,
    clippy::needless_collect,
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::doc_markdown,
    clippy::panic
)]

use std::sync::Arc;

use bss_ledger::domain::instant::format_rfc3339;
use bss_ledger::domain::instant::to_naive_date;
use bss_ledger::domain::model::{AccountRow, CurrencyScaleRow, NewEntry, NewLine};
use bss_ledger::domain::money::DEFAULT_PLAUSIBLE_MAX_MAJOR;
use bss_ledger::domain::payment::precedence::DEFAULT_PRECEDENCE_POLICY;
use bss_ledger::domain::payment::settlement::SettlementInput;
use bss_ledger::domain::ports::metrics::NoopLedgerMetrics;
use bss_ledger::infra::events::publisher::LedgerEventPublisher;
use bss_ledger::infra::payment::allocate::{
    AllocateRequest, AllocationOutcome, AllocationService, AppliedAllocation,
};
use bss_ledger::infra::payment::settle::SettlementService;
use bss_ledger::infra::posting::service::PostingService;
use bss_ledger::infra::storage::migrations::Migrator;
use bss_ledger::infra::storage::repo::ReferenceRepo;
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

/// Boot a container, run the chain on a raw connection, and return a
/// `bss`-search-path `DBProvider` for the services (the payments-test idiom).
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

/// Provisioned seller ids for the service tests (mirrors `postgres_payments`).
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
/// month, and the four payment-flow chart accounts (CASH_CLEARING / UNALLOCATED
/// / PSP_FEE_EXPENSE / AR).
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

/// Unwrap the inline-post arm of an allocate outcome. Every test here settles
/// before allocating, so the outcome is always `Applied`; a `Queued` (the
/// not-yet-settled §4.7 path) is a test-setup bug, so panic.
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
        effective_at: None,
    }
}

/// Insert a precedence-policy version row directly (the write path is admin /
/// out of 2b's scope; the read + wiring is what Group B delivers).
async fn seed_policy(
    raw: &sea_orm::DatabaseConnection,
    s: &Seller,
    version: i64,
    strategy: &str,
    effective_from: OffsetDateTime,
) {
    raw.execute_raw(pg(format!(
        "INSERT INTO bss.ledger_tenant_precedence_policy
            (tenant_id, version, effective_from, strategy, created_at_utc)
         VALUES ('{}', {version}, '{}', '{strategy}', '{}')",
        s.tenant,
        format_rfc3339(effective_from),
        format_rfc3339(OffsetDateTime::now_utc())
    )))
    .await
    .expect("seed precedence policy row");
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

/// The distinct `precedence_policy_ref`s stamped on `(tenant, payment_id)`'s
/// allocation rows (deduplicated; one per ref).
async fn allocation_policy_refs(
    raw: &sea_orm::DatabaseConnection,
    s: &Seller,
    payment_id: &str,
) -> Vec<String> {
    let rows = raw
        .query_all_raw(pg(format!(
            "SELECT DISTINCT precedence_policy_ref FROM bss.ledger_payment_allocation \
             WHERE tenant_id='{}' AND payment_id='{}' ORDER BY precedence_policy_ref",
            s.tenant, payment_id
        )))
        .await
        .unwrap();
    rows.into_iter()
        .map(|r| r.try_get_by_index::<String>(0).unwrap())
        .collect()
}

/// Seed an OPEN AR invoice by posting `DR AR (invoice_id) / CR PSP_FEE_EXPENSE`
/// directly through the engine, with an explicit `posted_at` (the oldest-first
/// sort key). Mirrors `postgres_payments::seed_ar_invoice`.
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

/// Standard 2-invoice setup: settle 1000 into the pool, then seed INV-A (300,
/// posted EARLIER) and INV-B (800, posted LATER). Under oldest-first INV-A is
/// paid first; under highest-amount-first INV-B (the larger open) is paid first
/// — so the two policies produce visibly different splits of a lump of 500.
async fn settle_and_seed_two_invoices(
    provider: &DBProvider<DbError>,
    s: &Seller,
    ctx: &SecurityContext,
    scope: &AccessScope,
    payment_id: &str,
) {
    settle_svc(provider)
        .settle(ctx, scope, settlement_input(s, payment_id, 1000, 0))
        .await
        .expect("settle");
    seed_ar_invoice(
        provider,
        s,
        "INV-A",
        300,
        OffsetDateTime::now_utc() - time::Duration::hours(2),
    )
    .await;
    seed_ar_invoice(
        provider,
        s,
        "INV-B",
        800,
        OffsetDateTime::now_utc() - time::Duration::hours(1),
    )
    .await;
}

fn allocate_request(s: &Seller, payment_id: &str) -> AllocateRequest {
    AllocateRequest {
        tenant_id: s.tenant,
        payer_tenant_id: s.payer,
        payment_id: payment_id.to_owned(),
        allocation_id: Uuid::now_v7(),
        lump_minor: 500,
        currency: "USD".to_owned(),
        hint_invoice_id: None,
        caller_splits: None,
    }
}

/// A seeded `HighestAmountFirst` policy steers the split largest-open-first
/// (INV-B before INV-A) AND its real `highest-amount-first.v1#<version>` ref is
/// stamped on every allocation row.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn effective_policy_steers_split_and_stamps_real_ref() {
    let (_c, raw, provider) = boot().await;
    let s = setup_seller(&raw, &provider).await;
    let ctx = SecurityContext::anonymous();
    let scope = AccessScope::for_tenant(s.tenant);

    // Policy version 3, effective an hour ago ⇒ in effect now.
    seed_policy(
        &raw,
        &s,
        3,
        "highest-amount-first.v1",
        OffsetDateTime::now_utc() - time::Duration::hours(1),
    )
    .await;
    settle_and_seed_two_invoices(&provider, &s, &ctx, &scope, "PAY-HAF-1").await;

    let outcome = applied(
        allocate_svc(&provider)
            .allocate(&ctx, &scope, allocate_request(&s, "PAY-HAF-1"))
            .await
            .expect("allocate must succeed"),
    );

    // Highest-amount-first: INV-B (800 open) fills the whole 500; INV-A gets 0.
    let splits: Vec<(String, i64)> = outcome
        .splits
        .iter()
        .map(|a| (a.invoice_id.clone(), a.amount_minor))
        .collect();
    assert_eq!(
        splits,
        vec![("INV-B".to_owned(), 500)],
        "highest-amount-first pays the largest open balance (INV-B) first"
    );
    assert_eq!(ar_invoice_balance(&raw, &s, "INV-B").await, Some(300));
    assert_eq!(ar_invoice_balance(&raw, &s, "INV-A").await, Some(300));

    // The REAL effective ref (strategy#version) is stamped on the rows.
    assert_eq!(
        allocation_policy_refs(&raw, &s, "PAY-HAF-1").await,
        vec!["highest-amount-first.v1#3".to_owned()],
        "the stamped ref is the effective strategy + version"
    );
}

/// With NO policy row the allocator falls back to oldest-first and stamps the
/// unchanged `DEFAULT_PRECEDENCE_POLICY` ref (byte-stable with 2a behaviour).
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn no_policy_row_defaults_to_oldest_first_and_default_ref() {
    let (_c, raw, provider) = boot().await;
    let s = setup_seller(&raw, &provider).await;
    let ctx = SecurityContext::anonymous();
    let scope = AccessScope::for_tenant(s.tenant);

    // No seed_policy call ⇒ read_effective_policy returns None.
    settle_and_seed_two_invoices(&provider, &s, &ctx, &scope, "PAY-DEF-1").await;

    let outcome = applied(
        allocate_svc(&provider)
            .allocate(&ctx, &scope, allocate_request(&s, "PAY-DEF-1"))
            .await
            .expect("allocate must succeed"),
    );

    // Oldest-first: INV-A (300) fills, INV-B takes the remaining 200.
    let splits: Vec<(String, i64)> = outcome
        .splits
        .iter()
        .map(|a| (a.invoice_id.clone(), a.amount_minor))
        .collect();
    assert_eq!(
        splits,
        vec![("INV-A".to_owned(), 300), ("INV-B".to_owned(), 200)],
        "no policy ⇒ oldest-first fills INV-A then INV-B"
    );
    assert_eq!(ar_invoice_balance(&raw, &s, "INV-A").await, Some(0));
    assert_eq!(ar_invoice_balance(&raw, &s, "INV-B").await, Some(600));

    // The default ref is stamped verbatim (NOT a strategy#version form).
    assert_eq!(
        allocation_policy_refs(&raw, &s, "PAY-DEF-1").await,
        vec![DEFAULT_PRECEDENCE_POLICY.to_owned()],
        "the fallback stamps DEFAULT_PRECEDENCE_POLICY unchanged"
    );
}

/// Two versions at different `effective_from`: the LATEST one whose
/// `effective_from <= now` wins. An older highest-amount-first (effective 2h
/// ago) is superseded by a newer oldest-first (effective 1h ago); a third
/// future row (effective tomorrow) is NOT yet in effect.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn latest_effective_version_in_effect_is_chosen() {
    let (_c, raw, provider) = boot().await;
    let s = setup_seller(&raw, &provider).await;
    let ctx = SecurityContext::anonymous();
    let scope = AccessScope::for_tenant(s.tenant);

    // v1: highest-amount-first, effective 2h ago (superseded).
    seed_policy(
        &raw,
        &s,
        1,
        "highest-amount-first.v1",
        OffsetDateTime::now_utc() - time::Duration::hours(2),
    )
    .await;
    // v2: oldest-first, effective 1h ago (the one in effect now).
    seed_policy(
        &raw,
        &s,
        2,
        "oldest-first.v1",
        OffsetDateTime::now_utc() - time::Duration::hours(1),
    )
    .await;
    // v3: highest-amount-first, effective TOMORROW (not yet in effect).
    seed_policy(
        &raw,
        &s,
        3,
        "highest-amount-first.v1",
        OffsetDateTime::now_utc() + time::Duration::days(1),
    )
    .await;
    settle_and_seed_two_invoices(&provider, &s, &ctx, &scope, "PAY-VER-1").await;

    let outcome = applied(
        allocate_svc(&provider)
            .allocate(&ctx, &scope, allocate_request(&s, "PAY-VER-1"))
            .await
            .expect("allocate must succeed"),
    );

    // v2 (oldest-first) is in effect ⇒ INV-A then INV-B; v3's future
    // highest-amount-first must NOT apply.
    let splits: Vec<(String, i64)> = outcome
        .splits
        .iter()
        .map(|a| (a.invoice_id.clone(), a.amount_minor))
        .collect();
    assert_eq!(
        splits,
        vec![("INV-A".to_owned(), 300), ("INV-B".to_owned(), 200)],
        "the latest effective-now version (v2 oldest-first) decides the split"
    );
    assert_eq!(
        allocation_policy_refs(&raw, &s, "PAY-VER-1").await,
        vec!["oldest-first.v1#2".to_owned()],
        "the stamped ref names the chosen version (v2)"
    );
}
