//! Postgres-only **concurrency** tests for the payment slice (G2): they race
//! real `AllocationService` / `PostingService` orchestrators on a testcontainer
//! Postgres and pin the invariants the per-payment cap, the candidate ceiling,
//! and the payer-grain projector must preserve under contention. Ignored by
//! default; run with
//! `cargo test -p cf-gears-bss-ledger --test postgres_payment_concurrency -- --ignored`.
//!
//! Covers: (1) N concurrent `allocate`s of the SAME payment never push
//! `allocated_minor` past `settled_minor` — the per-payment cap CHECK is the
//! authority even though the shared unallocated pool is positive (every loser
//! is `MoneyOutCapExceeded`); (2) an `allocate` and a concurrent fresh
//! invoice-post for the SAME payer serialize without deadlock and BOTH effects
//! land; (3) an `allocate` whose candidate set exceeds
//! `MAX_INVOICES_PER_ALLOCATION` is rejected `AllocationTooLarge` BEFORE any
//! post (no allocation rows written).
//!
//! Self-contained: re-declares the small `boot` / `setup_seller` /
//! `seed_ar_invoice` helpers it needs (mirrors `postgres_posting.rs`), so it
//! does not depend on `postgres_payments.rs`.

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

use std::fmt::Write as _;
use std::sync::Arc;

use bss_ledger::domain::error::DomainError;
use bss_ledger::domain::instant::to_naive_date;
use bss_ledger::domain::model::{AccountRow, CurrencyScaleRow, NewEntry, NewLine};
use bss_ledger::domain::money::DEFAULT_PLAUSIBLE_MAX_MAJOR;
use bss_ledger::domain::payment::settlement::SettlementInput;
use bss_ledger::domain::payment::settlement_return::SettlementReturnInput;
use bss_ledger::domain::ports::metrics::NoopLedgerMetrics;
use bss_ledger::infra::events::publisher::LedgerEventPublisher;
use bss_ledger::infra::payment::allocate::{
    AllocateRequest, AllocationService, MAX_INVOICES_PER_ALLOCATION,
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

/// Boot a container, migrate on a raw connection, and return a
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

/// Provisioned seller ids for the concurrency tests (mirrors
/// `postgres_payments::Seller`).
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

fn settlement_return_svc(provider: &DBProvider<DbError>) -> SettlementReturnService {
    SettlementReturnService::new(
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

/// Client-side retry of a service call on a projector-level serialization
/// conflict. Under `SERIALIZABLE`, two ops racing the same balance grain make
/// one abort with a 40001 that the projector stringifies into
/// `DomainError::Internal("…could not serialize…")` — decision O defers
/// in-service recompute-on-retry to the CALLER, so the test models what the
/// SDK/client does: re-run the WHOLE operation (re-reading open-AR, re-deciding
/// splits) until it commits or hits a real business rejection. A genuine
/// deadlock (40P01) is NOT a serialization conflict and propagates — failing the
/// test loudly, which is exactly the deadlock-freedom guarantee.
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

/// Seed an OPEN AR invoice by posting a balanced `DR AR (invoice_id) / CR
/// PSP_FEE_EXPENSE` directly through the engine (mirrors
/// `postgres_payments::seed_ar_invoice`). PSP_FEE_EXPENSE is unguarded, so this
/// lands a clean `ar_invoice_balance` row with `original_posted_at = posted_at`
/// (the oldest-first sort key).
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

/// Financial #G2: two CONCURRENT partial returns of the SAME payment
/// must BOTH commit — neither legal return may be falsely rejected. settle gross
/// 100 / fee 3 (settled=100, fee=3); two returns of 50 race, barrier-started for
/// maximum overlap. Pre-fix both read the SAME `(settled 100, fee 3)` snapshot
/// out-of-txn, both size `fee_share = 3×50/100 = 1`, and the second to commit
/// trips the `fee_minor <= settled_minor` CHECK (settled → 0 while fee still 1) —
/// a FALSE `SettlementReturnOverAllocated`. With the recompute-on-conflict loop
/// the loser re-reads `(settled 50, fee 2)`, re-sizes `fee_share = 2`, and
/// commits: the settlement drains fully (settled 0, fee 0), the fee reversed
/// exactly once each (1 + 2 = 3).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires Docker (testcontainers)"]
async fn concurrent_partial_returns_both_commit_and_drain_fee() {
    let (_c, raw, provider) = boot().await;
    let s = setup_seller(&raw, &provider).await;
    let ctx = SecurityContext::anonymous();
    let scope = AccessScope::for_tenant(s.tenant);

    // Settle gross 100 with a PSP fee 3: settled=100, fee=3 (net 97 in clearing).
    settle_svc(&provider)
        .settle(&ctx, &scope, settlement_input(&s, "PAY-CR", 100, 3))
        .await
        .expect("settle the payment to be returned");

    // Two partial returns of 50 each, distinct psp_return_id (genuine returns, not
    // idempotent replays), barrier-started so they overlap maximally.
    let returns = [("RET-A", 50i64), ("RET-B", 50i64)];
    let barrier = Arc::new(tokio::sync::Barrier::new(returns.len()));
    let mut handles = Vec::with_capacity(returns.len());
    for (psp_return_id, amount) in returns {
        let provider = provider.clone();
        let scope = scope.clone();
        let barrier = Arc::clone(&barrier);
        handles.push(tokio::spawn(async move {
            let svc = settlement_return_svc(&provider);
            let ctx = SecurityContext::anonymous();
            barrier.wait().await;
            // The service already recomputes fee_share on a cap-conflict; this
            // wraps the projector-level 40001 the engine surfaces, matching how a
            // client drives the op.
            retry_on_serialization(|| {
                svc.return_settlement(
                    &ctx,
                    &scope,
                    SettlementReturnInput {
                        tenant_id: s.tenant,
                        payer_tenant_id: s.payer,
                        payment_id: "PAY-CR".to_owned(),
                        psp_return_id: psp_return_id.to_owned(),
                        amount_minor: amount,
                        currency: "USD".to_owned(),
                        effective_at: None,
                    },
                )
            })
            .await
        }));
    }

    for handle in handles {
        handle
            .await
            .expect("return task must not panic")
            .expect("both concurrent partial returns must commit — neither falsely rejected");
    }

    // Both returns landed: the settlement drained fully — settled 0, fee 0 — so
    // `fee <= settled` held at every step and the fee was reversed exactly once
    // each (1 + 2 = 3), neither stranded nor double-counted.
    let row = PaymentRepo::new(provider.clone())
        .read_settlement(&scope, s.tenant, "PAY-CR")
        .await
        .unwrap()
        .expect("settlement row present");
    assert_eq!(row.settled_minor, 0, "both returns drained settled to 0");
    assert_eq!(row.fee_minor, 0, "the fee was fully reversed (1 + 2 = 3)");
}

/// Financial #G2-1: N concurrent allocates of the SAME payment racing its
/// per-payment money-out cap. The shared UNALLOCATED pool is funded ABOVE the
/// capped payment's settled amount by a SECOND payment (PAY-OTHER @ 500), so the
/// no-negative pool guard is NOT what trips — the per-payment cap CHECK
/// (`allocated_minor <= settled_minor`) is the sole authority. Four allocates of
/// lump=100 each race against PAY-CAP's settled=100: AT MOST one may commit, and
/// `allocated_minor` must NEVER exceed 100. Every loser is `MoneyOutCapExceeded`
/// — the client retries the projector-level serialization conflict (decision O
/// defers recompute-on-retry to the caller), so once the rows serialize a loser
/// re-reads `allocated_minor == 100` and surfaces the cap CHECK. Mirrors
/// `postgres_posting::concurrent_overdraw_of_guarded_account_stays_non_negative`,
/// scaled to a barrier-started N=4 fan-out.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires Docker (testcontainers)"]
async fn concurrent_allocate_respects_per_payment_cap() {
    let (_c, raw, provider) = boot().await;
    let s = setup_seller(&raw, &provider).await;
    let ctx = SecurityContext::anonymous();
    let scope = AccessScope::for_tenant(s.tenant);

    // Settle the capped payment at 100, and a second payment at 500 for the SAME
    // payer so the shared pool holds 600 — the pool stays positive throughout, so
    // any rejection is the per-payment cap, NOT the no-negative pool guard.
    let settle = settle_svc(&provider);
    settle
        .settle(&ctx, &scope, settlement_input(&s, "PAY-CAP", 100, 0))
        .await
        .expect("settle the capped payment");
    settle
        .settle(&ctx, &scope, settlement_input(&s, "PAY-OTHER", 500, 0))
        .await
        .expect("settle a second payment to fund the pool");

    // Seed open AR summing >= 400 (4 invoices @ 100), so each allocate of 100 has
    // candidates to drain and the cap — not an empty candidate set — is what
    // bounds the running total.
    for (i, invoice) in ["INV-1", "INV-2", "INV-3", "INV-4"].iter().enumerate() {
        seed_ar_invoice(
            &provider,
            &s,
            invoice,
            100,
            OffsetDateTime::now_utc() - time::Duration::hours(4 - i64::try_from(i).unwrap()),
        )
        .await;
    }

    // Barrier-start N=4 allocates of PAY-CAP @ lump=100, each a DISTINCT
    // allocation_id (so the dedup key never collides — every task is a genuine
    // fresh allocate racing the cap, not an idempotent replay). The barrier
    // releases all four at once onto the multi-thread runtime.
    let n = 4usize;
    let barrier = Arc::new(tokio::sync::Barrier::new(n));
    let mut handles = Vec::with_capacity(n);
    for _ in 0..n {
        let provider = provider.clone();
        let scope = scope.clone();
        let barrier = Arc::clone(&barrier);
        handles.push(tokio::spawn(async move {
            let svc = allocate_svc(&provider);
            let ctx = SecurityContext::anonymous();
            // One fixed allocation_id per task: a retry of a serialization-aborted
            // attempt is the SAME logical allocate, never a new one.
            let allocation_id = Uuid::now_v7();
            barrier.wait().await;
            retry_on_serialization(|| {
                svc.allocate(
                    &ctx,
                    &scope,
                    AllocateRequest {
                        tenant_id: s.tenant,
                        payer_tenant_id: s.payer,
                        payment_id: "PAY-CAP".to_owned(),
                        allocation_id,
                        lump_minor: 100,
                        currency: "USD".to_owned(),
                        hint_invoice_id: None,
                        caller_splits: None,
                    },
                )
            })
            .await
        }));
    }

    let mut oks = 0usize;
    for handle in handles {
        let result = handle.await.expect("allocate task must not panic");
        match result {
            Ok(_) => oks += 1,
            Err(DomainError::MoneyOutCapExceeded(_)) => {}
            Err(other) => {
                panic!("a losing concurrent allocate must be MoneyOutCapExceeded, got: {other:?}")
            }
        }
    }

    // INVARIANT: at least one allocate won, and the per-payment cap was never
    // exceeded — `allocated_minor` lands at EXACTLY the settled 100.
    assert!(oks >= 1, "at least one concurrent allocate must win");
    let repo = PaymentRepo::new(provider.clone());
    let row = repo
        .read_settlement(&scope, s.tenant, "PAY-CAP")
        .await
        .unwrap()
        .expect("settlement row present");
    assert_eq!(
        row.allocated_minor, 100,
        "the per-payment cap is never exceeded: allocated_minor == settled 100"
    );
    assert!(
        row.allocated_minor <= row.settled_minor,
        "allocated_minor ({}) must never exceed settled_minor ({})",
        row.allocated_minor,
        row.settled_minor
    );

    // Exactly one allocation's worth of rows persisted (the winner applied 100 to
    // a single 100-invoice → one payment_allocation row).
    assert_eq!(
        count_allocations(&raw, &s, "PAY-CAP").await,
        1,
        "only the winning allocate wrote its split row"
    );
}

/// G2-2: an allocate and a concurrent fresh invoice-post for the SAME payer must
/// serialize WITHOUT deadlock, and BOTH effects must land. The two ops touch the
/// shared payer-grain projection (`ar_payer_balance`) in overlapping order — a
/// reversed lock order would deadlock; the engine's SERIALIZABLE + retry must
/// instead serialize them. Asserts both complete (the `tokio::join!` returns
/// without a runtime timeout) and both effects are visible: INV-A drained to 0
/// by the allocate, INV-B posted at 500 by the concurrent invoice-post.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Docker (testcontainers)"]
async fn allocate_and_invoice_post_serialize_without_deadlock() {
    let (_c, raw, provider) = boot().await;
    let s = setup_seller(&raw, &provider).await;
    let ctx = SecurityContext::anonymous();
    let scope = AccessScope::for_tenant(s.tenant);

    // Settle 1000 into the pool and seed one open AR (INV-A @ 300).
    settle_svc(&provider)
        .settle(&ctx, &scope, settlement_input(&s, "PAY", 1000, 0))
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

    // Race (a) allocate(PAY, lump=300) draining INV-A to 0 against (b) a fresh
    // balanced invoice-post DR AR INV-B 500 / CR PSP_FEE 500 for the SAME payer.
    // Both hit the payer-grain projection; SERIALIZABLE + retry must serialize
    // them, never deadlock.
    let alloc_provider = provider.clone();
    let alloc_scope = scope.clone();
    let post_provider = provider.clone();
    let s_post = Seller {
        tenant: s.tenant,
        payer: s.payer,
        cash: s.cash,
        unallocated: s.unallocated,
        psp_fee: s.psp_fee,
        ar: s.ar,
        period_id: s.period_id.clone(),
    };

    let allocation_id = Uuid::now_v7();
    let allocate = tokio::spawn(async move {
        let svc = allocate_svc(&alloc_provider);
        let ctx = SecurityContext::anonymous();
        // Client retries the projector serialization conflict (decision O): the
        // allocate ultimately serializes after the concurrent post and commits.
        retry_on_serialization(|| {
            svc.allocate(
                &ctx,
                &alloc_scope,
                AllocateRequest {
                    tenant_id: s.tenant,
                    payer_tenant_id: s.payer,
                    payment_id: "PAY".to_owned(),
                    allocation_id,
                    lump_minor: 300,
                    currency: "USD".to_owned(),
                    hint_invoice_id: None,
                    caller_splits: None,
                },
            )
        })
        .await
    });
    let post = tokio::spawn(async move {
        let posting = PostingService::new(
            post_provider.clone(),
            Arc::new(LedgerEventPublisher::noop()),
        );
        let ctx = SecurityContext::anonymous();
        let scope = AccessScope::for_tenant(s_post.tenant);
        let posted_at = OffsetDateTime::now_utc() - time::Duration::minutes(30);
        // The concurrent invoice-post serializes against the allocate at the
        // shared payer/AR grain; the same client retry lands it.
        retry_on_serialization(|| {
            let entry = NewEntry {
                entry_id: Uuid::now_v7(),
                tenant_id: s_post.tenant,
                legal_entity_id: s_post.tenant,
                period_id: s_post.period_id.clone(),
                entry_currency: "USD".to_owned(),
                source_doc_type: SourceDocType::InvoicePost,
                source_business_id: "INV-B".to_owned(),
                reverses_entry_id: None,
                reverses_period_id: None,
                posted_at_utc: posted_at,
                effective_at: to_naive_date(posted_at),
                origin: "SYSTEM".to_owned(),
                posted_by_actor_id: s_post.tenant,
                correlation_id: Uuid::now_v7(),
                rounding_evidence: serde_json::Value::Null,
                rate_snapshot_ref: None,
            };
            let lines = vec![
                ar_line(&s_post, "INV-B", 500),
                psp_credit_line(&s_post, 500),
            ];
            posting.post(&ctx, &scope, entry, lines, None)
        })
        .await
    });

    let (alloc_res, post_res) = tokio::join!(allocate, post);
    alloc_res
        .expect("allocate task must not panic / deadlock")
        .expect("allocate must succeed");
    post_res
        .expect("invoice-post task must not panic / deadlock")
        .expect("invoice-post must succeed");

    // BOTH effects landed: the allocate drained INV-A to 0, and the concurrent
    // invoice-post left an INV-B AR row at 500.
    assert_eq!(
        ar_invoice_balance(&raw, &s, "INV-A").await,
        Some(0),
        "allocate drained INV-A to zero"
    );
    assert_eq!(
        ar_invoice_balance(&raw, &s, "INV-B").await,
        Some(500),
        "the concurrent invoice-post landed INV-B at 500"
    );
}

/// G2-3: an allocate whose open-AR candidate set exceeds
/// An allocation whose split **touches** more than `MAX_INVOICES_PER_ALLOCATION`
/// (500) invoices is rejected `AllocationTooLarge` BEFORE any post. 501 open
/// `ar_invoice_balance` rows are seeded by a DIRECT bulk SQL INSERT (NOT 501
/// engine posts — far too slow), each with balance 100, and a lump large enough
/// to reach EVERY one (501 × 100). The split therefore touches all 501, one over
/// the ceiling, so the guard fires before `build_allocation_entry`. Asserts the
/// error is `AllocationTooLarge` and that NO `payment_allocation` rows were
/// written (the guard precedes the post, so nothing is applied and
/// `allocated_minor` stays 0). Contrast `large_backlog_small_lump_allocates`,
/// which shows the same 501-invoice backlog allocates fine when the lump reaches
/// only a few of them — the bound is on invoices touched, not on the backlog.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn allocate_too_large_is_rejected() {
    let (_c, raw, provider) = boot().await;
    let s = setup_seller(&raw, &provider).await;
    let ctx = SecurityContext::anonymous();
    let scope = AccessScope::for_tenant(s.tenant);

    // Settle a pool large enough for the lump to reach all 501 invoices (501 ×
    // 100 = 50_100), so the split touches every candidate and trips the
    // touched-invoices ceiling.
    settle_svc(&provider)
        .settle(&ctx, &scope, settlement_input(&s, "PAY", 50_100, 0))
        .await
        .expect("settle");

    // Bulk-seed 501 open AR invoices for (tenant, payer, ar, USD) in ONE
    // multi-row INSERT. Each row carries a DISTINCT invoice_id (the 4th PK
    // column), a positive balance_minor (satisfies chk_ar_invoice_balance_no
    // _negative AND the `balance_minor > 0` candidate filter), and the seller's
    // AR account_id / payer / USD that `list_open_ar_invoices` filters on. Only
    // the NOT-NULL-without-default columns are supplied; original_posted_at /
    // due_date / last_entry_seq are nullable, version/ balance default-eligible
    // but balance is set explicitly.
    let mut sql = String::from(
        "INSERT INTO bss.ledger_ar_invoice_balance \
         (tenant_id, payer_tenant_id, account_id, invoice_id, currency, balance_minor) VALUES ",
    );
    // One over the ceiling — kept in lockstep with the source constant so the
    // test tracks any future change to the candidate cap.
    let count = MAX_INVOICES_PER_ALLOCATION + 1;
    for i in 0..count {
        if i > 0 {
            sql.push(',');
        }
        write!(
            sql,
            "('{}','{}','{}','INV-{:04}','USD',100)",
            s.tenant, s.payer, s.ar, i
        )
        .unwrap();
    }
    raw.execute_raw(pg(sql))
        .await
        .expect("bulk-seed 501 AR rows");

    // Sanity: exactly 501 open candidates exist for the payer.
    let seeded = raw
        .query_one_raw(pg(format!(
            "SELECT COUNT(*) FROM bss.ledger_ar_invoice_balance \
             WHERE tenant_id='{}' AND payer_tenant_id='{}' AND currency='USD' \
             AND balance_minor > 0",
            s.tenant, s.payer
        )))
        .await
        .unwrap()
        .map_or(0, |r| r.try_get_by_index::<i64>(0).unwrap());
    assert_eq!(
        seeded,
        i64::try_from(count).unwrap(),
        "501 open candidates seeded"
    );

    // The split touches all 501 invoices (the lump covers every one) — one over
    // the ceiling (500) → AllocationTooLarge, raised before any decision / post.
    let err = allocate_svc(&provider)
        .allocate(
            &ctx,
            &scope,
            AllocateRequest {
                tenant_id: s.tenant,
                payer_tenant_id: s.payer,
                payment_id: "PAY".to_owned(),
                allocation_id: Uuid::now_v7(),
                lump_minor: 50_100,
                currency: "USD".to_owned(),
                hint_invoice_id: None,
                caller_splits: None,
            },
        )
        .await
        .expect_err("an over-ceiling allocate must be rejected");
    assert!(
        matches!(err, DomainError::AllocationTooLarge(_)),
        "expected AllocationTooLarge, got {err:?}"
    );

    // The guard fired BEFORE the post: no allocation rows, allocated_minor still 0.
    assert_eq!(
        count_allocations(&raw, &s, "PAY").await,
        0,
        "the too-large guard precedes the post — no allocation rows written"
    );
    let repo = PaymentRepo::new(provider.clone());
    let row = repo
        .read_settlement(&scope, s.tenant, "PAY")
        .await
        .unwrap()
        .expect("settlement row present");
    assert_eq!(
        row.allocated_minor, 0,
        "nothing was applied (the post never ran)"
    );
}

/// The size bound is on invoices ACTUALLY touched, not on the open-invoice
/// backlog: a payer with 501 open invoices whose payment reaches only a few of
/// them allocates fine. Seeds the same 501-invoice backlog as
/// `allocate_too_large_is_rejected` but a small lump (300 over invoices of 100),
/// so the split touches 3 invoices and posts. Regression guard for the fix that
/// moved the cap off the candidate count onto the split.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn large_backlog_small_lump_allocates() {
    let (_c, raw, provider) = boot().await;
    let s = setup_seller(&raw, &provider).await;
    let ctx = SecurityContext::anonymous();
    let scope = AccessScope::for_tenant(s.tenant);

    settle_svc(&provider)
        .settle(&ctx, &scope, settlement_input(&s, "PAY", 1000, 0))
        .await
        .expect("settle");

    // Bulk-seed 501 open AR invoices (a large backlog), each balance 100.
    let mut sql = String::from(
        "INSERT INTO bss.ledger_ar_invoice_balance \
         (tenant_id, payer_tenant_id, account_id, invoice_id, currency, balance_minor) VALUES ",
    );
    let count = MAX_INVOICES_PER_ALLOCATION + 1;
    for i in 0..count {
        if i > 0 {
            sql.push(',');
        }
        write!(
            sql,
            "('{}','{}','{}','INV-{:04}','USD',100)",
            s.tenant, s.payer, s.ar, i
        )
        .unwrap();
    }
    raw.execute_raw(pg(sql))
        .await
        .expect("bulk-seed 501 AR rows");

    // The invoice-grain rows above are only part of the projection — a real
    // posting also maintains the AR account-level and per-payer aggregates, both
    // guarded no-negative. Seed them to the backlog total (501 × 100) so the
    // CR AR relief has headroom at every guarded grain, not just the invoice one.
    let ar_total = i64::try_from(count).unwrap() * 100;
    raw.execute_raw(pg(format!(
        "INSERT INTO bss.ledger_account_balance \
            (tenant_id, account_id, currency, account_class, normal_side, balance_minor) \
         VALUES ('{}','{}','USD','AR','DR',{ar_total})",
        s.tenant, s.ar
    )))
    .await
    .expect("seed AR account_balance aggregate");
    raw.execute_raw(pg(format!(
        "INSERT INTO bss.ledger_ar_payer_balance \
            (tenant_id, payer_tenant_id, account_id, currency, balance_minor) \
         VALUES ('{}','{}','{}','USD',{ar_total})",
        s.tenant, s.payer, s.ar
    )))
    .await
    .expect("seed AR payer_balance aggregate");

    // A small lump (300) reaches only the 3 oldest invoices (100 each), so the
    // split touches 3 — far under the ceiling — and the allocation posts.
    allocate_svc(&provider)
        .allocate(
            &ctx,
            &scope,
            AllocateRequest {
                tenant_id: s.tenant,
                payer_tenant_id: s.payer,
                payment_id: "PAY".to_owned(),
                allocation_id: Uuid::now_v7(),
                lump_minor: 300,
                currency: "USD".to_owned(),
                hint_invoice_id: None,
                caller_splits: None,
            },
        )
        .await
        .expect("a small lump against a large backlog must allocate");

    assert_eq!(
        count_allocations(&raw, &s, "PAY").await,
        3,
        "300 over invoices of 100 touches exactly 3",
    );
    let repo = PaymentRepo::new(provider.clone());
    let row = repo
        .read_settlement(&scope, s.tenant, "PAY")
        .await
        .unwrap()
        .expect("settlement row present");
    assert_eq!(row.allocated_minor, 300, "three invoices × 100 allocated");
}
