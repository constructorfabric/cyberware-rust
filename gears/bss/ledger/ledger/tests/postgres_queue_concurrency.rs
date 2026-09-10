//! Postgres-only **concurrency** tests for the deferred-apply queue (Slice 2b,
//! Phase 3, Group F): two appliers racing the SAME queued allocation must apply
//! it EXACTLY ONCE. Ignored by default; run with
//! `cargo test -p cf-gears-bss-ledger --test postgres_queue_concurrency -- --ignored`.
//!
//! Run discipline (controller): testcontainer `#[ignore]` tests; run the bin
//! sequentially (each test boots its own container).
//!
//! Covers: (1) two concurrent `AllocationService::drain`s of one tenant whose
//! queue holds a single due allocation apply it EXACTLY ONCE — one
//! `payment_allocation` row, `allocated_minor == 300` (not 600), queue
//! `APPLIED`, dedup `POSTED`. The `FOR UPDATE SKIP LOCKED` claim + the
//! `QueuedApply` POSTED-replay short-circuit are the two guards; a loser that
//! claims nothing (or replays the POSTED winner) never lands a second effect.
//! (2) A `settle` (which drains) racing a `QueueApplierJob::run()` on the same
//! tenant: both complete without deadlock and the allocation applies once.
//!
//! Self-contained: re-declares the small `boot` / `setup_seller` / `settle_svc`
//! / `allocate_svc` / `seed_ar_invoice` / `pg` / `retry_on_serialization`
//! helpers it needs (mirrors `postgres_payment_concurrency.rs`), plus the
//! queue/dedup raw-SQL readers.

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
use bss_ledger::domain::instant::to_naive_date;
use bss_ledger::domain::model::{AccountRow, CurrencyScaleRow, NewEntry, NewLine, RepoError};
use bss_ledger::domain::money::DEFAULT_PLAUSIBLE_MAX_MAJOR;
use bss_ledger::domain::payment::settlement::SettlementInput;
use bss_ledger::domain::ports::metrics::NoopLedgerMetrics;
use bss_ledger::infra::events::publisher::LedgerEventPublisher;
use bss_ledger::infra::jobs::queue_applier::QueueApplierJob;
use bss_ledger::infra::payment::allocate::{AllocateRequest, AllocationOutcome, AllocationService};
use bss_ledger::infra::payment::settle::SettlementService;
use bss_ledger::infra::posting::service::PostingService;
use bss_ledger::infra::storage::migrations::Migrator;
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

/// Lift a component `RepoError` into a `DbError` so an in-txn repo write can be
/// the transaction's typed success value (`T`), surviving COMMIT (mirrors
/// `postgres_payments.rs`). Used to seed a settlement DIRECTLY (no drain).
fn lift(e: RepoError) -> DbError {
    DbError::Sea(DbErr::Custom(e.to_string()))
}

/// Boot a container, migrate on a raw connection, and return a
/// `bss`-search-path `DBProvider`. Mirrors `postgres_payment_concurrency::boot`.
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

/// Provisioned seller ids for the concurrency tests (mirrors
/// `postgres_payment_concurrency::Seller`).
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
/// month, and the four payment-flow chart accounts. Mirrors
/// `postgres_payment_concurrency::setup_seller`.
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

/// An allocate request builder (lump in USD, no hint, no caller split — the
/// precedence/oldest-first path). `allocation_id` is supplied explicitly.
fn allocate_req(s: &Seller, payment_id: &str, allocation_id: Uuid, lump: i64) -> AllocateRequest {
    AllocateRequest {
        tenant_id: s.tenant,
        payer_tenant_id: s.payer,
        payment_id: payment_id.to_owned(),
        allocation_id,
        lump_minor: lump,
        currency: "USD".to_owned(),
        hint_invoice_id: None,
        caller_splits: None,
    }
}

/// Client-side retry of a service call on a projector-level serialization
/// conflict (mirrors `postgres_payment_concurrency::retry_on_serialization`).
/// Under `SERIALIZABLE`, two appliers racing the same grain make one abort with
/// a 40001 the projector stringifies into `DomainError::Internal("…serialize…")`
/// — the caller re-runs the whole op. A genuine deadlock (40P01) is NOT a
/// serialization conflict and propagates, failing the test loudly (the
/// deadlock-freedom guarantee).
async fn retry_on_serialization<F, Fut, T>(mut op: F) -> Result<T, DomainError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, DomainError>>,
{
    for _ in 0..20 {
        match op().await {
            Err(DomainError::Internal(m)) if m.contains("serialize") => {}
            other => return other,
        }
    }
    op().await
}

// ── Queue + dedup raw-SQL readers (keyed on the PAYMENT_ALLOCATE flow) ────────

async fn queue_status(
    raw: &sea_orm::DatabaseConnection,
    s: &Seller,
    allocation_id: Uuid,
) -> Option<String> {
    raw.query_one_raw(pg(format!(
        "SELECT status FROM bss.ledger_pending_event_queue \
         WHERE tenant_id='{}' AND flow='PAYMENT_ALLOCATE' AND business_id='{allocation_id}'",
        s.tenant
    )))
    .await
    .unwrap()
    .map(|r| r.try_get_by_index::<String>(0).unwrap())
}

async fn dedup_status(
    raw: &sea_orm::DatabaseConnection,
    s: &Seller,
    allocation_id: Uuid,
) -> Option<(String, Option<Uuid>)> {
    raw.query_one_raw(pg(format!(
        "SELECT status, result_entry_id FROM bss.ledger_idempotency_dedup \
         WHERE tenant_id='{}' AND flow='PAYMENT_ALLOCATE' AND business_id='{allocation_id}'",
        s.tenant
    )))
    .await
    .unwrap()
    .map(|r| {
        (
            r.try_get_by_index::<String>(0).unwrap(),
            r.try_get_by_index::<Option<Uuid>>(1).unwrap(),
        )
    })
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

/// Seed an OPEN AR invoice by posting `DR AR (invoice_id) / CR PSP_FEE_EXPENSE`
/// directly through the engine (mirrors
/// `postgres_payment_concurrency::seed_ar_invoice`).
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

/// Queue an allocate (lump 300) on `payment_id`, seed an open AR (inv 300), then
/// seed the settlement row DIRECTLY in a `db.transaction` so NO drain ran — the
/// queue row is `QUEUED` and now "due". Returns the `allocation_id`. Shared
/// setup for the race tests (the racing appliers then compete to apply it).
async fn queue_with_seeded_settlement(
    raw: &sea_orm::DatabaseConnection,
    provider: &DBProvider<DbError>,
    s: &Seller,
    payment_id: &str,
) -> Uuid {
    let ctx = SecurityContext::anonymous();
    let scope = AccessScope::for_tenant(s.tenant);

    seed_ar_invoice(
        provider,
        s,
        "INV-A",
        300,
        OffsetDateTime::now_utc() - time::Duration::hours(1),
    )
    .await;

    let allocation_id = Uuid::now_v7();
    let queued = allocate_svc(provider)
        .allocate(
            &ctx,
            &scope,
            allocate_req(s, payment_id, allocation_id, 300),
        )
        .await
        .expect("unsettled allocate queues");
    assert!(
        matches!(queued, AllocationOutcome::Queued(_)),
        "queued first"
    );

    // Seed the settlement DIRECTLY (bypassing SettlementService → no drain), so
    // both racing appliers find a still-QUEUED, now-due row to compete over.
    let tenant = s.tenant;
    let payment = payment_id.to_owned();
    provider
        .transaction(move |txn| {
            let scope = scope.clone();
            let payment = payment.clone();
            Box::pin(async move {
                PaymentRepo::seed_settlement(txn, &scope, tenant, &payment, "USD", 1000, 0)
                    .await
                    .map_err(lift)
            })
        })
        .await
        .expect("seed settlement directly");

    // A real settle CRs the UNALLOCATED pool (the cash the allocation draws). The
    // direct counter seed above bypasses that, so seed the post-settle projector
    // state for UNALLOCATED — BOTH guarded grains the apply's DR UNALLOCATED
    // touches: the aggregate `account_balance` (per account) AND the per-payer
    // `unallocated_balance` pool. Without `account_balance` the projector's
    // guarded no-negative pre-check on the aggregate drives it to -amount and the
    // allocation is (correctly) blocked.
    raw.execute_raw(pg(format!(
        "INSERT INTO bss.ledger_account_balance \
            (tenant_id, account_id, currency, account_class, normal_side, balance_minor) \
         VALUES ('{}','{}','USD','UNALLOCATED','CR',1000)",
        s.tenant, s.unallocated
    )))
    .await
    .expect("seed unallocated account_balance");
    raw.execute_raw(pg(format!(
        "INSERT INTO bss.ledger_unallocated_balance \
            (tenant_id, payer_tenant_id, account_id, currency, balance_minor) \
         VALUES ('{}','{}','{}','USD',1000)",
        s.tenant, s.payer, s.unallocated
    )))
    .await
    .expect("seed unallocated pool");
    allocation_id
}

// ── Tests ────────────────────────────────────────────────────────────────────

/// Exactly-once under concurrent drains: with a single due queued allocation and
/// its settlement seeded directly (so neither drain triggered it), two
/// `AllocationService::drain`s race on the SAME tenant. The `FOR UPDATE SKIP
/// LOCKED` claim hands the row to at most one applier; if the other still
/// observes it (or replays a finalized winner via the `QueuedApply` POSTED
/// short-circuit), it lands NO second effect. INVARIANT: exactly ONE
/// `payment_allocation` row, `allocated_minor == 300` (never 600), queue
/// `APPLIED`, dedup `POSTED`, AR drained to 0.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Docker (testcontainers)"]
async fn two_drains_apply_a_queued_allocation_exactly_once() {
    let (_c, raw, provider) = boot().await;
    let s = setup_seller(&raw, &provider).await;

    let allocation_id = queue_with_seeded_settlement(&raw, &provider, &s, "PAY-RACE").await;
    // Precondition: the directly-seeded settlement did not apply it.
    assert_eq!(
        queue_status(&raw, &s, allocation_id).await.as_deref(),
        Some("QUEUED"),
        "the seeded settlement left the queue row QUEUED (no drain ran)"
    );

    // Two concurrent drains of the SAME tenant, each retrying serialization
    // conflicts. SKIP LOCKED + the POSTED-replay guarantee apply-exactly-once.
    let p_a = provider.clone();
    let p_b = provider.clone();
    let tenant = s.tenant;
    let drain_a = tokio::spawn(async move {
        let svc = allocate_svc(&p_a);
        let ctx = SecurityContext::anonymous();
        let scope = AccessScope::for_tenant(tenant);
        retry_on_serialization(|| svc.drain(&ctx, &scope, tenant, 16)).await
    });
    let drain_b = tokio::spawn(async move {
        let svc = allocate_svc(&p_b);
        let ctx = SecurityContext::anonymous();
        let scope = AccessScope::for_tenant(tenant);
        retry_on_serialization(|| svc.drain(&ctx, &scope, tenant, 16)).await
    });

    let (ra, rb) = tokio::join!(drain_a, drain_b);
    let report_a = ra
        .expect("drain A task must not panic / deadlock")
        .expect("drain A ok");
    let report_b = rb
        .expect("drain B task must not panic / deadlock")
        .expect("drain B ok");

    // The row was applied EXACTLY ONCE across the two drains (the other saw an
    // empty claim or replayed the POSTED winner — never a second apply).
    assert_eq!(
        report_a.applied + report_b.applied,
        1,
        "the queued allocation is applied exactly once across the racing drains: {report_a:?} / {report_b:?}"
    );

    // INVARIANT: one allocation's worth of effect, never doubled.
    assert_eq!(
        count_allocations(&raw, &s, "PAY-RACE").await,
        1,
        "exactly one payment_allocation row (the race applied it once)"
    );
    let repo = PaymentRepo::new(provider.clone());
    let row = repo
        .read_settlement(&AccessScope::for_tenant(s.tenant), s.tenant, "PAY-RACE")
        .await
        .unwrap()
        .expect("settlement row present");
    assert_eq!(
        row.allocated_minor, 300,
        "allocated_minor reflects exactly one apply (300), not a doubled 600"
    );
    assert_eq!(
        queue_status(&raw, &s, allocation_id).await.as_deref(),
        Some("APPLIED")
    );
    let (status, entry_id) = dedup_status(&raw, &s, allocation_id)
        .await
        .expect("dedup row present after apply");
    assert_eq!(status, "POSTED");
    assert!(
        entry_id.is_some(),
        "the applied dedup carries the result entry id"
    );
    assert_eq!(ar_invoice_balance(&raw, &s, "INV-A").await, Some(0));
}

/// A `settle` (whose drain-on-settle hook applies the queue) racing a
/// `QueueApplierJob::run()` sweep on the SAME tenant: both complete without
/// deadlock and the queued allocation applies EXACTLY ONCE. Here the settlement
/// arrives via the real `SettlementService` (so the settle's own drain competes
/// with the concurrent sweep over the same SKIP-LOCKED claim). Asserts both
/// futures return (no deadlock) and the allocation is applied once (one
/// `payment_allocation` row, `allocated_minor == 300`, queue `APPLIED`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Docker (testcontainers)"]
async fn drain_and_sweep_race_without_deadlock() {
    let (_c, raw, provider) = boot().await;
    let s = setup_seller(&raw, &provider).await;
    let ctx = SecurityContext::anonymous();
    let scope = AccessScope::for_tenant(s.tenant);

    // Seed AR (inv 300) and queue an allocate of 300 on PAY-RS while unsettled.
    seed_ar_invoice(
        &provider,
        &s,
        "INV-A",
        300,
        OffsetDateTime::now_utc() - time::Duration::hours(1),
    )
    .await;
    let allocation_id = Uuid::now_v7();
    let queued = allocate_svc(&provider)
        .allocate(&ctx, &scope, allocate_req(&s, "PAY-RS", allocation_id, 300))
        .await
        .expect("unsettled allocate queues");
    assert!(matches!(queued, AllocationOutcome::Queued(_)));

    // Race the settle (which drains after committing) against a sweep pass. Both
    // contend for the same SKIP-LOCKED claim; one applies the row, the other sees
    // an empty claim or replays the POSTED winner — neither deadlocks.
    let settle_provider = provider.clone();
    let settle_scope = scope.clone();
    let s_settle = settlement_input(&s, "PAY-RS", 1000, 0);
    let settle = tokio::spawn(async move {
        let ctx = SecurityContext::anonymous();
        settle_svc(&settle_provider)
            .settle(&ctx, &settle_scope, s_settle)
            .await
    });
    let sweep_provider = provider.clone();
    let sweep = tokio::spawn(async move {
        QueueApplierJob::new(
            sweep_provider,
            Arc::new(LedgerEventPublisher::noop()),
            Arc::new(NoopLedgerMetrics),
        )
        .run()
        .await
    });

    let (settle_res, sweep_res) = tokio::join!(settle, sweep);
    settle_res
        .expect("settle task must not panic / deadlock")
        .expect("settle must succeed");
    sweep_res
        .expect("sweep task must not panic / deadlock")
        .expect("sweep must run");

    // Applied EXACTLY ONCE regardless of which side won the claim.
    assert_eq!(
        count_allocations(&raw, &s, "PAY-RS").await,
        1,
        "the queued allocation is applied exactly once across the settle/sweep race"
    );
    let repo = PaymentRepo::new(provider.clone());
    let row = repo
        .read_settlement(&scope, s.tenant, "PAY-RS")
        .await
        .unwrap()
        .expect("settlement row present");
    assert_eq!(
        row.allocated_minor, 300,
        "allocated_minor is 300, not doubled"
    );
    assert_eq!(
        queue_status(&raw, &s, allocation_id).await.as_deref(),
        Some("APPLIED")
    );
    assert_eq!(ar_invoice_balance(&raw, &s, "INV-A").await, Some(0));
}
