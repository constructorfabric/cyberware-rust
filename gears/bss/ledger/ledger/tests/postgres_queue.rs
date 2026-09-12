//! Postgres-only integration tests for the §4.7 **allocation-before-settlement
//! queue** — the deferred-apply mechanism end-to-end: intake (a not-yet-settled
//! allocate durably enqueues + dedups `QUEUED` instead of posting), queued
//! replay (a second allocate of the same id returns the handle without a second
//! row), drain-on-settle (a `settle` applies the tenant's queue), the
//! cap-re-evaluation at apply (an over-cap queued allocation stays `QUEUED` +
//! bumps attempts), and the cross-tenant sweep job (`QueueApplierJob`) as the
//! restart/backstop path. Ignored by default; run with
//! `cargo test -p cf-gears-bss-ledger --test postgres_queue -- --ignored`.
//!
//! Run discipline (controller): these are testcontainer `#[ignore]` tests; run
//! the bin sequentially (each test boots its own container).
//!
//! Self-contained: re-declares the small `boot` / `setup_seller` / `settle_svc`
//! / `allocate_svc` / `seed_ar_invoice` / `pg` helpers it needs (the templates
//! duplicate per-file), plus raw-SQL readers for the queue + dedup tables
//! (`queue_status` / `dedup_status`) keyed on the `PAYMENT_ALLOCATE` flow.

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

/// Lift a component `RepoError` into a `DbError` so a repo write can be the
/// transaction's typed success value (`T`), surviving COMMIT (mirrors
/// `postgres_payments.rs`). Used by `sweep_applies_seeded_settlement` to seed a
/// settlement DIRECTLY in a `db.transaction`.
fn lift(e: RepoError) -> DbError {
    DbError::Sea(DbErr::Custom(e.to_string()))
}

/// Boot a container, run the chain on a raw connection, and return a
/// `bss`-search-path `DBProvider`. Mirrors `postgres_payments::boot`.
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

/// Provisioned seller ids (mirrors `postgres_payments::Seller`).
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
/// UNALLOCATED credit, PSP_FEE_EXPENSE debit, AR debit). Mirrors
/// `postgres_payments::setup_seller`.
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

/// An allocate request builder for the queue tests: lump in USD, no hint, no
/// caller split (the precedence/oldest-first path). `allocation_id` is supplied
/// explicitly so a test can replay the SAME id.
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

/// The work-state queue-row status for an allocation (its `business_id` is the
/// `allocation_id` string, its `flow` the `PAYMENT_ALLOCATE` literal). `None`
/// when no queue row exists. The drain flips this `QUEUED → APPLIED`.
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

/// Count the queue rows for one allocation (to prove a replay adds no second
/// row). Keyed identically to [`queue_status`].
async fn queue_row_count(
    raw: &sea_orm::DatabaseConnection,
    s: &Seller,
    allocation_id: Uuid,
) -> i64 {
    raw.query_one_raw(pg(format!(
        "SELECT COUNT(*) FROM bss.ledger_pending_event_queue \
         WHERE tenant_id='{}' AND flow='PAYMENT_ALLOCATE' AND business_id='{allocation_id}'",
        s.tenant
    )))
    .await
    .unwrap()
    .map_or(0, |r| r.try_get_by_index::<i64>(0).unwrap())
}

/// One queue row's `attempts` counter (bumped when a queued allocation is
/// `Blocked` at apply by a re-evaluated cap). `None` when no row exists.
async fn queue_attempts(
    raw: &sea_orm::DatabaseConnection,
    s: &Seller,
    allocation_id: Uuid,
) -> Option<i32> {
    raw.query_one_raw(pg(format!(
        "SELECT attempts FROM bss.ledger_pending_event_queue \
         WHERE tenant_id='{}' AND flow='PAYMENT_ALLOCATE' AND business_id='{allocation_id}'",
        s.tenant
    )))
    .await
    .unwrap()
    .map(|r| r.try_get_by_index::<i32>(0).unwrap())
}

/// One queue row's `apply_after` (the backoff deferral instant set when a queued
/// allocation is `Blocked` at apply). `None` when the column is NULL (immediately
/// eligible) or no row exists.
async fn queue_apply_after(
    raw: &sea_orm::DatabaseConnection,
    s: &Seller,
    allocation_id: Uuid,
) -> Option<OffsetDateTime> {
    raw.query_one_raw(pg(format!(
        "SELECT apply_after FROM bss.ledger_pending_event_queue \
         WHERE tenant_id='{}' AND flow='PAYMENT_ALLOCATE' AND business_id='{allocation_id}'",
        s.tenant
    )))
    .await
    .unwrap()
    .and_then(|r| r.try_get_by_index::<Option<OffsetDateTime>>(0).unwrap())
}

/// The dedup-row `(status, result_entry_id)` for an allocation (the
/// `PAYMENT_ALLOCATE` dedup key). A queued allocation is `("QUEUED", None)`; an
/// applied one is `("POSTED", Some(entry_id))`. `None` when no dedup row exists.
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

/// Seed an OPEN AR invoice by posting `DR AR (invoice_id) / CR PSP_FEE_EXPENSE`
/// directly through the engine (mirrors `postgres_payments::seed_ar_invoice`).
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

// ── Tests ────────────────────────────────────────────────────────────────────

/// §4.7 intake: an allocate of a payment that was NEVER settled does NOT post —
/// it durably enqueues. The outcome is `Queued`, a `QUEUED` queue row exists, the
/// dedup is `("QUEUED", None)` (no `result_entry_id` — nothing posted), and there
/// are NO `payment_allocation` rows and the AR is untouched.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn unsettled_allocate_queues() {
    let (_c, raw, provider) = boot().await;
    let s = setup_seller(&raw, &provider).await;
    let ctx = SecurityContext::anonymous();
    let scope = AccessScope::for_tenant(s.tenant);

    // Seed an open AR invoice, but DO NOT settle PAY-Q1 — the allocate must queue
    // (the §4.7 allocation-before-settlement path), not reject or post.
    seed_ar_invoice(
        &provider,
        &s,
        "INV-A",
        300,
        OffsetDateTime::now_utc() - time::Duration::hours(1),
    )
    .await;

    let allocation_id = Uuid::now_v7();
    let outcome = allocate_svc(&provider)
        .allocate(&ctx, &scope, allocate_req(&s, "PAY-Q1", allocation_id, 300))
        .await
        .expect("an unsettled allocate must queue (not error)");
    assert!(
        matches!(outcome, AllocationOutcome::Queued(_)),
        "an unsettled allocate must be Queued, got {outcome:?}"
    );

    // Durable QUEUED row + QUEUED dedup with NO result_entry_id (nothing posted).
    assert_eq!(
        queue_status(&raw, &s, allocation_id).await.as_deref(),
        Some("QUEUED")
    );
    assert_eq!(
        dedup_status(&raw, &s, allocation_id).await,
        Some(("QUEUED".to_owned(), None)),
        "dedup is QUEUED with no result entry id"
    );

    // Nothing posted: no allocation rows, AR untouched.
    assert_eq!(count_allocations(&raw, &s, "PAY-Q1").await, 0);
    assert_eq!(
        ar_invoice_balance(&raw, &s, "INV-A").await,
        Some(300),
        "AR untouched by a queued allocate"
    );
}

/// §4.7 queued replay: a second allocate of the SAME `allocation_id` while still
/// unsettled returns the same `Queued` handle — it does NOT enqueue a second row.
/// Exactly ONE queue row exists for the key, and the dedup stays `QUEUED`.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn queued_replay_returns_handle() {
    let (_c, raw, provider) = boot().await;
    let s = setup_seller(&raw, &provider).await;
    let ctx = SecurityContext::anonymous();
    let scope = AccessScope::for_tenant(s.tenant);

    seed_ar_invoice(
        &provider,
        &s,
        "INV-A",
        300,
        OffsetDateTime::now_utc() - time::Duration::hours(1),
    )
    .await;

    let svc = allocate_svc(&provider);
    let allocation_id = Uuid::now_v7();

    // First allocate enqueues.
    let first = svc
        .allocate(&ctx, &scope, allocate_req(&s, "PAY-Q2", allocation_id, 300))
        .await
        .expect("first unsettled allocate queues");
    assert!(matches!(first, AllocationOutcome::Queued(_)));

    // Second allocate of the SAME id (still unsettled) replays the handle.
    let second = svc
        .allocate(&ctx, &scope, allocate_req(&s, "PAY-Q2", allocation_id, 300))
        .await
        .expect("queued replay returns the handle");
    assert!(
        matches!(second, AllocationOutcome::Queued(_)),
        "a queued replay is still Queued, got {second:?}"
    );

    // Exactly one queue row + still-QUEUED dedup — no double-enqueue.
    assert_eq!(
        queue_row_count(&raw, &s, allocation_id).await,
        1,
        "the same allocation_id enqueues exactly one queue row"
    );
    assert_eq!(
        queue_status(&raw, &s, allocation_id).await.as_deref(),
        Some("QUEUED")
    );
    assert_eq!(
        dedup_status(&raw, &s, allocation_id).await,
        Some(("QUEUED".to_owned(), None))
    );
}

/// Drain-on-settle: a queued allocate (lump 300) is applied when the payment is
/// later settled — `SettlementService::settle` drains the tenant's queue after
/// committing. The queue row flips `→APPLIED`, the dedup flips `→POSTED` with a
/// `result_entry_id`, a `payment_allocation` row exists, and the AR drains to 0.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn settle_drains_queued_allocation() {
    let (_c, raw, provider) = boot().await;
    let s = setup_seller(&raw, &provider).await;
    let ctx = SecurityContext::anonymous();
    let scope = AccessScope::for_tenant(s.tenant);

    // Seed AR (inv=300) and queue an allocate of 300 on PAY-DRAIN BEFORE settling.
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
        .allocate(
            &ctx,
            &scope,
            allocate_req(&s, "PAY-DRAIN", allocation_id, 300),
        )
        .await
        .expect("unsettled allocate queues");
    assert!(matches!(queued, AllocationOutcome::Queued(_)));
    assert_eq!(
        queue_status(&raw, &s, allocation_id).await.as_deref(),
        Some("QUEUED")
    );

    // Settle PAY-DRAIN (gross 1000 funds the pool) — the drain-on-settle hook then
    // applies the queued allocation in a second txn.
    settle_svc(&provider)
        .settle(&ctx, &scope, settlement_input(&s, "PAY-DRAIN", 1000, 0))
        .await
        .expect("settle must succeed (and drain the queue)");

    // The queued allocation applied: queue → APPLIED, dedup → POSTED (+ entry id).
    assert_eq!(
        queue_status(&raw, &s, allocation_id).await.as_deref(),
        Some("APPLIED"),
        "the drain flipped the queue row to APPLIED"
    );
    let (status, entry_id) = dedup_status(&raw, &s, allocation_id)
        .await
        .expect("dedup row present after apply");
    assert_eq!(status, "POSTED", "dedup flipped to POSTED on apply");
    assert!(
        entry_id.is_some(),
        "a POSTED dedup carries the result entry id"
    );

    // The ledger effect landed: one allocation row, AR drained to 0.
    assert_eq!(count_allocations(&raw, &s, "PAY-DRAIN").await, 1);
    assert_eq!(ar_invoice_balance(&raw, &s, "INV-A").await, Some(0));
    let repo = PaymentRepo::new(provider.clone());
    let row = repo
        .read_settlement(&scope, s.tenant, "PAY-DRAIN")
        .await
        .unwrap()
        .expect("settlement row present");
    assert_eq!(
        row.allocated_minor, 300,
        "the drained allocation netted 300"
    );
}

/// Cap re-evaluation at apply (§4.7): the per-payment money-out cap is re-checked
/// when the queued allocation is applied, NOT trusted from intake. Queue an
/// allocate of lump=1000 on PAY-CAP with AR inv=1000 (so the AR is NOT the
/// binding constraint), then settle PAY-CAP at only gross=500. At apply the split
/// re-derives to 1000 against the still-open 1000 AR, but bumping `allocated_minor`
/// to 1000 > settled 500 trips the per-payment cap ⇒ the row is `Blocked`: it
/// stays `QUEUED` with `attempts >= 1`, the dedup stays `QUEUED`, the AR is
/// unchanged, and NO `payment_allocation` row is written (the apply rolled back).
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn cap_reevaluated_at_apply_blocks() {
    let (_c, raw, provider) = boot().await;
    let s = setup_seller(&raw, &provider).await;
    let ctx = SecurityContext::anonymous();
    let scope = AccessScope::for_tenant(s.tenant);

    // AR inv=1000 (>= lump), so the per-payment cap — not the AR open balance — is
    // the binding constraint at apply (the prompt's AR >= lump > settled shape).
    seed_ar_invoice(
        &provider,
        &s,
        "INV-A",
        1000,
        OffsetDateTime::now_utc() - time::Duration::hours(1),
    )
    .await;

    // Queue an allocate of the WHOLE 1000 on PAY-CAP while unsettled.
    let allocation_id = Uuid::now_v7();
    let queued = allocate_svc(&provider)
        .allocate(
            &ctx,
            &scope,
            allocate_req(&s, "PAY-CAP", allocation_id, 1000),
        )
        .await
        .expect("unsettled allocate queues");
    assert!(matches!(queued, AllocationOutcome::Queued(_)));

    // Settle PAY-CAP at only 500 ⇒ settled_minor=500 < the queued lump 1000. The
    // drain-on-settle applies the queue; at apply the cap (allocated 1000 > settled
    // 500) blocks it. The settle itself still succeeds (the drain swallows errors).
    settle_svc(&provider)
        .settle(&ctx, &scope, settlement_input(&s, "PAY-CAP", 500, 0))
        .await
        .expect("settle succeeds even though the queued allocation is blocked");

    // The over-cap apply was blocked: the row stays QUEUED with attempts bumped.
    assert_eq!(
        queue_status(&raw, &s, allocation_id).await.as_deref(),
        Some("QUEUED"),
        "a cap-blocked queued allocation stays QUEUED"
    );
    assert!(
        queue_attempts(&raw, &s, allocation_id).await.unwrap_or(0) >= 1,
        "a blocked apply bumps attempts"
    );
    assert!(
        queue_apply_after(&raw, &s, allocation_id).await.is_some(),
        "a blocked apply defers the row via apply_after (backoff), so a poison \
         row is not re-claimed on every drain pass"
    );
    assert_eq!(
        dedup_status(&raw, &s, allocation_id).await,
        Some(("QUEUED".to_owned(), None)),
        "the dedup stays QUEUED (nothing posted)"
    );

    // Nothing applied: AR unchanged, no allocation row, allocated_minor still 0.
    assert_eq!(ar_invoice_balance(&raw, &s, "INV-A").await, Some(1000));
    assert_eq!(count_allocations(&raw, &s, "PAY-CAP").await, 0);
    let repo = PaymentRepo::new(provider.clone());
    let row = repo
        .read_settlement(&scope, s.tenant, "PAY-CAP")
        .await
        .unwrap()
        .expect("settlement row present");
    assert_eq!(
        row.allocated_minor, 0,
        "the blocked apply left allocated at 0"
    );
}

/// Idempotency-key reuse with a DIFFERENT payload is a conflict, not a silent
/// replay (Codex P3). Settle PAY-FP (gross 1000) with AR inv=1000, allocate
/// `alloc_id` lump=600 (posts inline), then re-send the SAME `alloc_id`: an
/// IDENTICAL request replays cleanly, but a different lump is rejected as
/// `IdempotencyConflict` — the replay short-circuit compares the request-based
/// dedup hash instead of blindly returning the prior result. The original 600
/// allocation is left untouched.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn allocate_idempotency_key_reuse_with_different_payload_conflicts() {
    let (_c, raw, provider) = boot().await;
    let s = setup_seller(&raw, &provider).await;
    let ctx = SecurityContext::anonymous();
    let scope = AccessScope::for_tenant(s.tenant);

    seed_ar_invoice(
        &provider,
        &s,
        "INV-FP",
        1000,
        OffsetDateTime::now_utc() - time::Duration::hours(1),
    )
    .await;
    settle_svc(&provider)
        .settle(&ctx, &scope, settlement_input(&s, "PAY-FP", 1000, 0))
        .await
        .expect("settle");

    let alloc_id = Uuid::now_v7();
    let applied = allocate_svc(&provider)
        .allocate(&ctx, &scope, allocate_req(&s, "PAY-FP", alloc_id, 600))
        .await
        .expect("first allocate posts");
    assert!(matches!(applied, AllocationOutcome::Applied(_)));

    // Same id, IDENTICAL payload ⇒ a clean replay (no conflict).
    let replay = allocate_svc(&provider)
        .allocate(&ctx, &scope, allocate_req(&s, "PAY-FP", alloc_id, 600))
        .await
        .expect("identical replay is accepted");
    assert!(
        matches!(replay, AllocationOutcome::Applied(ref a) if a.posting.replayed),
        "an identical re-send replays the prior posting"
    );

    // Same id, DIFFERENT lump ⇒ idempotency conflict, not a silent replay.
    let err = allocate_svc(&provider)
        .allocate(&ctx, &scope, allocate_req(&s, "PAY-FP", alloc_id, 400))
        .await
        .expect_err("reused id with a different payload is rejected");
    assert!(
        matches!(err, DomainError::IdempotencyConflict(_)),
        "expected IdempotencyConflict, got {err:?}"
    );

    // The original 600 allocation is untouched (still exactly one allocation row).
    assert_eq!(count_allocations(&raw, &s, "PAY-FP").await, 1);
}

/// QUEUED-path idempotency conflict: an allocate-before-settlement queues under
/// `allocation_id`; re-submitting the SAME id with a DIFFERENT lump (still
/// unsettled) is rejected as `IdempotencyConflict`, not silently re-queued — the
/// request-hash compare in `replay_short_circuit` (the deterministic sibling of
/// the in-txn intake-race guard). An identical re-submit still returns the queued
/// handle with no duplicate row.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn queued_allocate_reuse_with_different_payload_conflicts() {
    let (_c, raw, provider) = boot().await;
    let s = setup_seller(&raw, &provider).await;
    let ctx = SecurityContext::anonymous();
    let scope = AccessScope::for_tenant(s.tenant);

    // Queue an allocate of an unsettled payment.
    let alloc_id = Uuid::now_v7();
    let queued = allocate_svc(&provider)
        .allocate(&ctx, &scope, allocate_req(&s, "PAY-QC", alloc_id, 600))
        .await
        .expect("unsettled allocate queues");
    assert!(matches!(queued, AllocationOutcome::Queued(_)));

    // Identical re-submit ⇒ same queued handle (idempotent, no second row).
    let again = allocate_svc(&provider)
        .allocate(&ctx, &scope, allocate_req(&s, "PAY-QC", alloc_id, 600))
        .await
        .expect("identical re-submit returns the queued handle");
    assert!(matches!(again, AllocationOutcome::Queued(_)));
    assert_eq!(
        queue_row_count(&raw, &s, alloc_id).await,
        1,
        "an identical re-submit adds no duplicate queue row"
    );

    // Same id, DIFFERENT lump ⇒ idempotency conflict.
    let err = allocate_svc(&provider)
        .allocate(&ctx, &scope, allocate_req(&s, "PAY-QC", alloc_id, 400))
        .await
        .expect_err("reused id with a different payload is rejected");
    assert!(
        matches!(err, DomainError::IdempotencyConflict(_)),
        "expected IdempotencyConflict, got {err:?}"
    );
}

/// The cross-tenant sweep job (`QueueApplierJob`) is the restart/backstop path:
/// it applies a queued allocation whose settlement landed through some path that
/// did NOT drain. Queue an allocate (lump 300) on PAY-SWEEP with AR inv=300, then
/// seed the settlement row DIRECTLY via `PaymentRepo::seed_settlement` in a
/// `db.transaction` (so NO drain-on-settle ran — the queue row is still `QUEUED`).
/// `QueueApplierJob::run()` then finds + applies it: the row flips `→APPLIED`, the
/// dedup `→POSTED`, and the AR drains to 0.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn sweep_applies_seeded_settlement() {
    let (_c, raw, provider) = boot().await;
    let s = setup_seller(&raw, &provider).await;
    let ctx = SecurityContext::anonymous();
    let scope = AccessScope::for_tenant(s.tenant);

    // Seed AR (inv=300) and queue an allocate of 300 on PAY-SWEEP while unsettled.
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
        .allocate(
            &ctx,
            &scope,
            allocate_req(&s, "PAY-SWEEP", allocation_id, 300),
        )
        .await
        .expect("unsettled allocate queues");
    assert!(matches!(queued, AllocationOutcome::Queued(_)));
    assert_eq!(
        queue_status(&raw, &s, allocation_id).await.as_deref(),
        Some("QUEUED")
    );

    // Seed the settlement DIRECTLY (bypassing SettlementService → no drain runs),
    // so the queue row is still QUEUED when the sweep starts (the restart path).
    let scope_seed = scope.clone();
    provider
        .transaction(move |txn| {
            let scope = scope_seed.clone();
            Box::pin(async move {
                PaymentRepo::seed_settlement(txn, &scope, s.tenant, "PAY-SWEEP", "USD", 1000, 0)
                    .await
                    .map_err(lift)
            })
        })
        .await
        .expect("seed settlement directly");
    // A real settle CRs the UNALLOCATED pool (the cash the allocation draws). The
    // direct counter seed bypasses that, so seed the post-settle projector state
    // for UNALLOCATED — BOTH guarded grains the apply's DR UNALLOCATED touches:
    // the aggregate `account_balance` (per account) AND the per-payer
    // `unallocated_balance` pool. Without `account_balance` the projector's
    // guarded no-negative pre-check on the aggregate drives it to -amount and the
    // row is (correctly) blocked instead of applied.
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
    // Sanity: the seed did NOT trigger a drain — the row is still QUEUED.
    assert_eq!(
        queue_status(&raw, &s, allocation_id).await.as_deref(),
        Some("QUEUED"),
        "seeding the settlement directly must not apply the queued allocation"
    );

    // The sweep job (cross-tenant) discovers the due QUEUED row and applies it.
    let report = QueueApplierJob::new(
        provider.clone(),
        Arc::new(LedgerEventPublisher::noop()),
        Arc::new(NoopLedgerMetrics),
    )
    .run()
    .await
    .expect("sweep must run");
    assert!(
        report.applied >= 1,
        "the sweep applied at least one row: {report:?}"
    );

    // Applied by the sweep: queue → APPLIED, dedup → POSTED (+ entry id), AR → 0.
    assert_eq!(
        queue_status(&raw, &s, allocation_id).await.as_deref(),
        Some("APPLIED"),
        "the sweep flipped the queue row to APPLIED"
    );
    let (status, entry_id) = dedup_status(&raw, &s, allocation_id)
        .await
        .expect("dedup row present after sweep apply");
    assert_eq!(status, "POSTED");
    assert!(entry_id.is_some());
    assert_eq!(count_allocations(&raw, &s, "PAY-SWEEP").await, 1);
    assert_eq!(ar_invoice_balance(&raw, &s, "INV-A").await, Some(0));
}
