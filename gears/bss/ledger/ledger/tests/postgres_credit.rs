//! Postgres-only **service-level** integration tests for the reusable-credit
//! (wallet) slice: they drive the REAL `CreditApplicationService` (grant / apply)
//! through the foundation `PostingService` against a testcontainer Postgres.
//! Ignored by default; run with
//! `cargo test -p cf-gears-bss-ledger --test postgres_credit -- --ignored`.
//!
//! The credit counterpart to `postgres_payments.rs`. `setup_seller` provisions
//! the chart (CASH_CLEARING / UNALLOCATED / PSP_FEE_EXPENSE / AR + a stream-less
//! REUSABLE_CREDIT credit account), USD@2, and an OPEN fiscal period for the
//! CURRENT month (grant/apply derive `period_id` from `OffsetDateTime::now_utc()` — they post
//! effective-now). The wallet itself is a projector grain
//! (`bss.ledger_reusable_credit_subbalance`), read here by raw SQL.
//!
//! Covers: (a) a grant moves `DR UNALLOCATED / CR REUSABLE_CREDIT` — the wallet
//! sub-grain rises and the unallocated pool drops by the same amount; (b) an
//! apply draws the wallet OLDEST-GRANT-FIRST across two sub-grains ("promo"
//! before the later "goodwill"), drains the named AR, and reports the per-grain
//! `debits`; (c) a grant past the live unallocated pool is rejected
//! `GrantExceedsUnallocated` before any post; (d) an apply whose targets exceed
//! open AR is rejected `CreditExceedsOpenAr`; (e) an apply past the available
//! wallet is rejected `CreditExceedsWallet`; (f) a replay of the same
//! `credit_application_id` returns the prior posting and moves nothing.

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
use bss_ledger::domain::model::{AccountRow, CurrencyScaleRow, NewEntry, NewLine};
use bss_ledger::domain::money::DEFAULT_PLAUSIBLE_MAX_MAJOR;
use bss_ledger::domain::payment::precedence::Allocated;
use bss_ledger::domain::payment::settlement::SettlementInput;
use bss_ledger::domain::ports::metrics::NoopLedgerMetrics;
use bss_ledger::infra::events::publisher::LedgerEventPublisher;
use bss_ledger::infra::payment::credit::{ApplyRequest, CreditApplicationService, GrantRequest};
use bss_ledger::infra::payment::settle::SettlementService;
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

/// Provisioned seller ids for the credit service tests (mirrors
/// `postgres_payments::Seller`, plus the stream-less REUSABLE_CREDIT account the
/// wallet posts hit).
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

/// Provision a seller: USD@2 scale, an OPEN fiscal period for the current month,
/// the four payment-flow chart accounts (CASH_CLEARING debit, UNALLOCATED credit,
/// PSP_FEE_EXPENSE debit, AR debit), and a stream-less REUSABLE_CREDIT credit
/// account (the wallet). Reuses the file's `boot()` for the container/provider.
async fn setup_seller(raw: &sea_orm::DatabaseConnection, provider: &DBProvider<DbError>) -> Seller {
    let now = OffsetDateTime::now_utc();
    let s = Seller {
        tenant: Uuid::now_v7(),
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
        // The wallet: REUSABLE_CREDIT is credit-normal and stream-less (resolves
        // on `stream = None`, like UNALLOCATED).
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

fn settle_svc(provider: &DBProvider<DbError>) -> SettlementService {
    SettlementService::new(
        provider.clone(),
        Arc::new(LedgerEventPublisher::noop()),
        Arc::new(NoopLedgerMetrics),
    )
}

fn credit_svc(provider: &DBProvider<DbError>) -> CreditApplicationService {
    CreditApplicationService::new(
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
        // None ⇒ the orchestrator stamps a current-month effective date / period
        // (matching the OPEN period `setup_seller` provisions).
        effective_at: None,
    }
}

/// Fund the payer's unallocated pool by settling a fee-less payment of `gross`.
async fn fund_pool(provider: &DBProvider<DbError>, s: &Seller, payment_id: &str, gross: i64) {
    settle_svc(provider)
        .settle(
            &SecurityContext::anonymous(),
            &AccessScope::for_tenant(s.tenant),
            settlement_input(s, payment_id, gross, 0),
        )
        .await
        .expect("settle to fund the pool");
}

/// Read the live unallocated pool for the payer (the grant cap basis).
async fn unallocated(provider: &DBProvider<DbError>, s: &Seller) -> i64 {
    PaymentRepo::new(provider.clone())
        .read_unallocated(&AccessScope::for_tenant(s.tenant), s.tenant, s.payer, "USD")
        .await
        .unwrap()
}

/// Read one wallet sub-grain's `balance_minor` from the projector cache. The
/// table is keyed by `(tenant, payer, account_id, currency, event_type)`; the
/// read here matches on `(tenant, payer, currency, event_type)` (the values that
/// uniquely identify the bucket for this seller's single REUSABLE_CREDIT account)
/// — mirrors the tax sub-balance raw-read idiom in `postgres_projector.rs`.
async fn wallet_subgrain(
    raw: &sea_orm::DatabaseConnection,
    s: &Seller,
    event_type: &str,
) -> Option<i64> {
    raw.query_one_raw(pg(format!(
        "SELECT balance_minor FROM bss.ledger_reusable_credit_subbalance \
         WHERE tenant_id='{}' AND payer_tenant_id='{}' AND currency='USD' \
         AND credit_grant_event_type='{}'",
        s.tenant, s.payer, event_type
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

/// Seed an OPEN AR invoice by posting a balanced `DR AR (invoice_id) / CR
/// PSP_FEE_EXPENSE` directly through the engine (mirrors
/// `postgres_payments::seed_ar_invoice`). PSP_FEE_EXPENSE is unguarded, so this
/// lands a clean `ar_invoice_balance` row with `original_posted_at = posted_at`
/// (the oldest-first sort key); `posted_at` is supplied explicitly so the test
/// controls ordering deterministically.
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

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn grant_raises_wallet_and_lowers_unallocated() {
    let (_c, raw, provider) = boot().await;
    let s = setup_seller(&raw, &provider).await;
    let ctx = SecurityContext::anonymous();
    let scope = AccessScope::for_tenant(s.tenant);

    // Settle 1000 into the pool, then grant 600 to the "promo" wallet bucket:
    // DR UNALLOCATED 600 / CR REUSABLE_CREDIT 600.
    fund_pool(&provider, &s, "PAY-GRANT-1", 1000).await;
    assert_eq!(
        unallocated(&provider, &s).await,
        1000,
        "pool funded to 1000"
    );

    let outcome = credit_svc(&provider)
        .grant_credit(
            &ctx,
            &scope,
            GrantRequest {
                tenant_id: s.tenant,
                payer_tenant_id: s.payer,
                credit_application_id: "CR-GRANT-1".to_owned(),
                currency: "USD".to_owned(),
                amount_minor: 600,
                credit_grant_event_type: "promo".to_owned(),
            },
        )
        .await
        .expect("grant must succeed");
    assert!(!outcome.posting.replayed, "first grant is fresh");
    // A grant moves no wallet/AR splits — both vectors are empty.
    assert!(outcome.debits.is_empty(), "a grant emits no debits");
    assert!(outcome.targets.is_empty(), "a grant emits no targets");

    // The wallet sub-grain rose to 600, and the pool dropped by exactly 600.
    assert_eq!(
        wallet_subgrain(&raw, &s, "promo").await,
        Some(600),
        "the promo wallet sub-grain holds the grant"
    );
    assert_eq!(
        unallocated(&provider, &s).await,
        400,
        "the unallocated pool dropped by the grant amount (1000 - 600)"
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn apply_draws_oldest_grant_first_across_subgrains() {
    let (_c, raw, provider) = boot().await;
    let s = setup_seller(&raw, &provider).await;
    let ctx = SecurityContext::anonymous();
    let scope = AccessScope::for_tenant(s.tenant);
    let svc = credit_svc(&provider);

    // Fund the pool, then grant 400 "promo" FIRST and 300 "goodwill" SECOND. The
    // wallet sub-balance projector stamps `first_granted_at` from each grant's
    // post instant (first-write-wins, per bucket), so promo's stamp is strictly
    // earlier than goodwill's — apply draws promo first (oldest-grant-first). The
    // brief sleep makes that ordering immune to a same-instant clock collision
    // (which would otherwise fall back to the `credit_grant_event_type` ASC
    // tiebreak, i.e. "goodwill" before "promo").
    fund_pool(&provider, &s, "PAY-APPLY-1", 1000).await;
    svc.grant_credit(
        &ctx,
        &scope,
        GrantRequest {
            tenant_id: s.tenant,
            payer_tenant_id: s.payer,
            credit_application_id: "CR-PROMO".to_owned(),
            currency: "USD".to_owned(),
            amount_minor: 400,
            credit_grant_event_type: "promo".to_owned(),
        },
    )
    .await
    .expect("grant promo");
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    svc.grant_credit(
        &ctx,
        &scope,
        GrantRequest {
            tenant_id: s.tenant,
            payer_tenant_id: s.payer,
            credit_application_id: "CR-GOODWILL".to_owned(),
            currency: "USD".to_owned(),
            amount_minor: 300,
            credit_grant_event_type: "goodwill".to_owned(),
        },
    )
    .await
    .expect("grant goodwill");
    assert_eq!(wallet_subgrain(&raw, &s, "promo").await, Some(400));
    assert_eq!(wallet_subgrain(&raw, &s, "goodwill").await, Some(300));

    // One open AR invoice of 500, paid entirely by the wallet.
    seed_ar_invoice(
        &provider,
        &s,
        "inv-1",
        500,
        OffsetDateTime::now_utc() - time::Duration::hours(1),
    )
    .await;

    // Apply 500 against inv-1: oldest-grant-first drains promo (400) then takes
    // the remaining 100 from goodwill.
    let outcome = svc
        .apply_credit(
            &ctx,
            &scope,
            ApplyRequest {
                tenant_id: s.tenant,
                payer_tenant_id: s.payer,
                credit_application_id: "CR-APPLY-1".to_owned(),
                currency: "USD".to_owned(),
                targets: vec![Allocated {
                    invoice_id: "inv-1".to_owned(),
                    amount_minor: 500,
                }],
            },
        )
        .await
        .expect("apply must succeed");
    assert!(!outcome.posting.replayed, "first apply is fresh");

    // The debits are the per-sub-grain draw-downs in fill order: promo 400 then
    // goodwill 100.
    let debits: Vec<(String, i64)> = outcome
        .debits
        .iter()
        .map(|d| (d.credit_grant_event_type.clone(), d.amount_minor))
        .collect();
    assert_eq!(
        debits,
        vec![("promo".to_owned(), 400), ("goodwill".to_owned(), 100)],
        "oldest-grant-first draws promo (400) then goodwill (100)"
    );
    // The targets echo the validated receivable shares.
    assert_eq!(outcome.targets.len(), 1);
    assert_eq!(outcome.targets[0].invoice_id, "inv-1");
    assert_eq!(outcome.targets[0].amount_minor, 500);

    // AR fully paid; promo drained to 0, goodwill down to 200.
    assert_eq!(ar_invoice_balance(&raw, &s, "inv-1").await, Some(0));
    assert_eq!(wallet_subgrain(&raw, &s, "promo").await, Some(0));
    assert_eq!(wallet_subgrain(&raw, &s, "goodwill").await, Some(200));
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn grant_exceeding_unallocated_is_rejected() {
    let (_c, raw, provider) = boot().await;
    let s = setup_seller(&raw, &provider).await;
    let ctx = SecurityContext::anonymous();
    let scope = AccessScope::for_tenant(s.tenant);

    // Pool holds only 100; a grant of 500 exceeds it ⇒ rejected before any post.
    fund_pool(&provider, &s, "PAY-GRANT-OVR", 100).await;

    let err = credit_svc(&provider)
        .grant_credit(
            &ctx,
            &scope,
            GrantRequest {
                tenant_id: s.tenant,
                payer_tenant_id: s.payer,
                credit_application_id: "CR-GRANT-OVR".to_owned(),
                currency: "USD".to_owned(),
                amount_minor: 500,
                credit_grant_event_type: "promo".to_owned(),
            },
        )
        .await
        .expect_err("an over-pool grant must be rejected");
    assert!(
        matches!(err, DomainError::GrantExceedsUnallocated(_)),
        "expected GrantExceedsUnallocated, got {err:?}"
    );

    // Rejected before the post: no wallet sub-grain row, pool unchanged at 100.
    assert_eq!(
        wallet_subgrain(&raw, &s, "promo").await,
        None,
        "the rejected grant created no wallet sub-grain"
    );
    assert_eq!(
        unallocated(&provider, &s).await,
        100,
        "the unallocated pool is unchanged by a rejected grant"
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn apply_exceeding_open_ar_is_rejected() {
    let (_c, raw, provider) = boot().await;
    let s = setup_seller(&raw, &provider).await;
    let ctx = SecurityContext::anonymous();
    let scope = AccessScope::for_tenant(s.tenant);
    let svc = credit_svc(&provider);

    // Fund a wallet of 1000 (ample), but seed an open AR invoice of only 300.
    fund_pool(&provider, &s, "PAY-AR-OVR", 1000).await;
    svc.grant_credit(
        &ctx,
        &scope,
        GrantRequest {
            tenant_id: s.tenant,
            payer_tenant_id: s.payer,
            credit_application_id: "CR-AR-OVR-GRANT".to_owned(),
            currency: "USD".to_owned(),
            amount_minor: 1000,
            credit_grant_event_type: "promo".to_owned(),
        },
    )
    .await
    .expect("grant to fund the wallet");
    seed_ar_invoice(
        &provider,
        &s,
        "inv-1",
        300,
        OffsetDateTime::now_utc() - time::Duration::hours(1),
    )
    .await;

    // 500 > inv-1's open 300 ⇒ CreditExceedsOpenAr (the per-invoice open cap; the
    // wallet (1000) is ample, so the wallet cap is NOT what trips).
    let err = svc
        .apply_credit(
            &ctx,
            &scope,
            ApplyRequest {
                tenant_id: s.tenant,
                payer_tenant_id: s.payer,
                credit_application_id: "CR-AR-OVR".to_owned(),
                currency: "USD".to_owned(),
                targets: vec![Allocated {
                    invoice_id: "inv-1".to_owned(),
                    amount_minor: 500,
                }],
            },
        )
        .await
        .expect_err("an over-open-AR apply must be rejected");
    assert!(
        matches!(err, DomainError::CreditExceedsOpenAr(_)),
        "expected CreditExceedsOpenAr, got {err:?}"
    );

    // Rejected before the post: AR untouched, wallet untouched.
    assert_eq!(ar_invoice_balance(&raw, &s, "inv-1").await, Some(300));
    assert_eq!(wallet_subgrain(&raw, &s, "promo").await, Some(1000));
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn apply_exceeding_wallet_is_rejected() {
    let (_c, raw, provider) = boot().await;
    let s = setup_seller(&raw, &provider).await;
    let ctx = SecurityContext::anonymous();
    let scope = AccessScope::for_tenant(s.tenant);
    let svc = credit_svc(&provider);

    // Wallet holds only 200, but the open AR invoice is 500. The targets sit
    // within open AR (500 <= 500), so the receivable cap passes and the WALLET
    // cap is what trips.
    fund_pool(&provider, &s, "PAY-WAL-OVR", 1000).await;
    svc.grant_credit(
        &ctx,
        &scope,
        GrantRequest {
            tenant_id: s.tenant,
            payer_tenant_id: s.payer,
            credit_application_id: "CR-WAL-OVR-GRANT".to_owned(),
            currency: "USD".to_owned(),
            amount_minor: 200,
            credit_grant_event_type: "promo".to_owned(),
        },
    )
    .await
    .expect("grant a small wallet");
    seed_ar_invoice(
        &provider,
        &s,
        "inv-1",
        500,
        OffsetDateTime::now_utc() - time::Duration::hours(1),
    )
    .await;

    let err = svc
        .apply_credit(
            &ctx,
            &scope,
            ApplyRequest {
                tenant_id: s.tenant,
                payer_tenant_id: s.payer,
                credit_application_id: "CR-WAL-OVR".to_owned(),
                currency: "USD".to_owned(),
                targets: vec![Allocated {
                    invoice_id: "inv-1".to_owned(),
                    amount_minor: 500,
                }],
            },
        )
        .await
        .expect_err("an over-wallet apply must be rejected");
    assert!(
        matches!(err, DomainError::CreditExceedsWallet(_)),
        "expected CreditExceedsWallet, got {err:?}"
    );

    // Rejected before the post: AR untouched, wallet still 200.
    assert_eq!(ar_invoice_balance(&raw, &s, "inv-1").await, Some(500));
    assert_eq!(wallet_subgrain(&raw, &s, "promo").await, Some(200));
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn apply_replays_idempotently() {
    let (_c, raw, provider) = boot().await;
    let s = setup_seller(&raw, &provider).await;
    let ctx = SecurityContext::anonymous();
    let scope = AccessScope::for_tenant(s.tenant);
    let svc = credit_svc(&provider);

    // Fund a 500 wallet and an open AR of 500.
    fund_pool(&provider, &s, "PAY-RPL", 1000).await;
    svc.grant_credit(
        &ctx,
        &scope,
        GrantRequest {
            tenant_id: s.tenant,
            payer_tenant_id: s.payer,
            credit_application_id: "CR-RPL-GRANT".to_owned(),
            currency: "USD".to_owned(),
            amount_minor: 500,
            credit_grant_event_type: "promo".to_owned(),
        },
    )
    .await
    .expect("grant the wallet");
    seed_ar_invoice(
        &provider,
        &s,
        "inv-1",
        500,
        OffsetDateTime::now_utc() - time::Duration::hours(1),
    )
    .await;

    let apply = || ApplyRequest {
        tenant_id: s.tenant,
        payer_tenant_id: s.payer,
        credit_application_id: "CR-RPL-APPLY".to_owned(),
        currency: "USD".to_owned(),
        targets: vec![Allocated {
            invoice_id: "inv-1".to_owned(),
            amount_minor: 300,
        }],
    };

    let first = svc
        .apply_credit(&ctx, &scope, apply())
        .await
        .expect("first");
    assert!(!first.posting.replayed, "first apply is fresh");
    // One application's effect landed: AR 500→200, wallet 500→200.
    assert_eq!(ar_invoice_balance(&raw, &s, "inv-1").await, Some(200));
    assert_eq!(wallet_subgrain(&raw, &s, "promo").await, Some(200));

    // A second apply with the SAME credit_application_id replays the prior
    // posting and moves nothing further (the AR has drained below the target, so
    // a *fresh* apply would be rejected — only a true replay leaves the prior
    // entry and the balances untouched).
    let second = svc
        .apply_credit(&ctx, &scope, apply())
        .await
        .expect("replay");
    assert!(
        second.posting.replayed,
        "second apply is an idempotent replay"
    );
    assert_eq!(
        first.posting.entry_id, second.posting.entry_id,
        "replay returns the prior entry"
    );
    assert_eq!(
        ar_invoice_balance(&raw, &s, "inv-1").await,
        Some(200),
        "AR unchanged on replay"
    );
    assert_eq!(
        wallet_subgrain(&raw, &s, "promo").await,
        Some(200),
        "wallet unchanged on replay"
    );
}

/// Idempotency-key reuse with a DIFFERENT payload is a conflict, not a silent
/// replay (Codex P2). Grant 600 under `CR-FP`, then re-send the SAME id: an
/// IDENTICAL grant replays cleanly, but a different amount is rejected as
/// `IdempotencyConflict` — the credit short-circuit compares the request-based
/// dedup hash instead of blindly returning the prior posting. The wallet keeps
/// exactly the original 600.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn credit_idempotency_key_reuse_with_different_payload_conflicts() {
    let (_c, raw, provider) = boot().await;
    let s = setup_seller(&raw, &provider).await;
    let ctx = SecurityContext::anonymous();
    let scope = AccessScope::for_tenant(s.tenant);
    let svc = credit_svc(&provider);

    fund_pool(&provider, &s, "PAY-CR-FP", 1000).await;
    let grant = |amount: i64| GrantRequest {
        tenant_id: s.tenant,
        payer_tenant_id: s.payer,
        credit_application_id: "CR-FP".to_owned(),
        currency: "USD".to_owned(),
        amount_minor: amount,
        credit_grant_event_type: "promo".to_owned(),
    };

    let first = svc
        .grant_credit(&ctx, &scope, grant(600))
        .await
        .expect("first grant");
    assert!(!first.posting.replayed, "first grant is fresh");

    // Same id, IDENTICAL payload ⇒ a clean replay.
    let replay = svc
        .grant_credit(&ctx, &scope, grant(600))
        .await
        .expect("identical replay is accepted");
    assert!(replay.posting.replayed, "an identical re-send replays");
    assert_eq!(
        first.posting.entry_id, replay.posting.entry_id,
        "replay returns the prior entry"
    );

    // Same id, DIFFERENT amount ⇒ idempotency conflict, not a silent replay.
    let err = svc
        .grant_credit(&ctx, &scope, grant(400))
        .await
        .expect_err("reused id with a different payload is rejected");
    assert!(
        matches!(err, DomainError::IdempotencyConflict(_)),
        "expected IdempotencyConflict, got {err:?}"
    );

    // The wallet still holds exactly the original 600 (the conflict moved nothing).
    assert_eq!(wallet_subgrain(&raw, &s, "promo").await, Some(600));
}
