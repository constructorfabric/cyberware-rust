//! Postgres-only service-level tests for the settlement-return flow (Phase 4,
//! Group A + Group D Model N): `SettlementReturnService` posts the symmetric
//! reverse of settle — `DR UNALLOCATED amount / CR CASH_CLEARING (amount −
//! fee_share) / CR PSP_FEE_EXPENSE fee_share` — and decrements BOTH the original
//! payment's `settled_minor` and `fee_minor` in the same txn. Ignored by default;
//! run with
//! `cargo test -p cf-gears-bss-ledger --test postgres_payment_returns -- --ignored`.
//!
//! Covers: (a) a return after a settle decrements `settled_minor` and drains the
//! pool by the returned amount; (b) a re-posted return (same `psp_return_id`)
//! replays idempotently — `settled_minor` decrements exactly once; (c) a return
//! exceeding the still-returnable settled amount trips the per-payment cap CHECK
//! and surfaces as `SettlementReturnOverAllocated`, leaving the row untouched;
//! (d) a fee-bearing FULL return reverses CASH_CLEARING by the NET and
//! PSP_FEE_EXPENSE by the fee (Model N), zeroing both `settled_minor` and
//! `fee_minor`.

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

use bss_ledger::domain::error::DomainError;
use bss_ledger::domain::model::{AccountRow, CurrencyScaleRow};
use bss_ledger::domain::money::DEFAULT_PLAUSIBLE_MAX_MAJOR;
use bss_ledger::domain::payment::settlement::SettlementInput;
use bss_ledger::domain::payment::settlement_return::SettlementReturnInput;
use bss_ledger::domain::ports::metrics::NoopLedgerMetrics;
use bss_ledger::infra::events::publisher::LedgerEventPublisher;
use bss_ledger::infra::payment::settle::SettlementService;
use bss_ledger::infra::payment::settlement_return::SettlementReturnService;
use bss_ledger::infra::storage::migrations::Migrator;
use bss_ledger::infra::storage::repo::{PaymentRepo, ReferenceRepo};
use bss_ledger_sdk::{AccountClass, Side};
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

/// Provisioned seller for the return flow: the classes a settle + return touch
/// (`CASH_CLEARING`, `UNALLOCATED`, and — for the fee-bearing Model-N reverse —
/// `PSP_FEE_EXPENSE`). `cash` / `psp_fee` ids are retained so a test can read
/// their `account_balance`.
struct Seller {
    tenant: Uuid,
    payer: Uuid,
    cash: Uuid,
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
/// month, and the `CASH_CLEARING` / `UNALLOCATED` / `PSP_FEE_EXPENSE` chart
/// accounts.
async fn setup_seller(raw: &sea_orm::DatabaseConnection, provider: &DBProvider<DbError>) -> Seller {
    let now = OffsetDateTime::now_utc();
    let s = Seller {
        tenant: Uuid::now_v7(),
        payer: Uuid::now_v7(),
        cash: Uuid::now_v7(),
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
        // PSP_FEE_EXPENSE is debit-normal (the fee is expensed at settle); the
        // Model-N symmetric reverse credits it back on a fee-bearing return.
        account(
            s.tenant,
            s.psp_fee,
            AccountClass::PspFeeExpense,
            Side::Debit,
        ),
    ] {
        reference.insert_account(row).await.unwrap();
    }
    s
}

/// Read an account's cached `balance_minor` (USD), or `None` when no row exists.
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

fn settle_svc(provider: &DBProvider<DbError>) -> SettlementService {
    SettlementService::new(
        provider.clone(),
        Arc::new(LedgerEventPublisher::noop()),
        Arc::new(NoopLedgerMetrics),
    )
}

fn return_svc(provider: &DBProvider<DbError>) -> SettlementReturnService {
    SettlementReturnService::new(
        provider.clone(),
        Arc::new(LedgerEventPublisher::noop()),
        Arc::new(NoopLedgerMetrics),
    )
}

/// Settle `gross` (fee 0) into the pool for `payment_id`.
async fn settle(provider: &DBProvider<DbError>, s: &Seller, payment_id: &str, gross: i64) {
    settle_with_fee(provider, s, payment_id, gross, 0).await;
}

/// Settle `gross` with a PSP `fee` (Model N): `settle` posts `DR CASH_CLEARING
/// (gross − fee) · DR PSP_FEE_EXPENSE (fee) · CR UNALLOCATED (gross)`, so
/// `CASH_CLEARING` holds only **net**; the counter row is seeded
/// `settled_minor = gross`, `fee_minor = fee`.
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

fn return_input(
    s: &Seller,
    payment_id: &str,
    psp_return_id: &str,
    amount: i64,
) -> SettlementReturnInput {
    SettlementReturnInput {
        tenant_id: s.tenant,
        payer_tenant_id: s.payer,
        payment_id: payment_id.to_owned(),
        psp_return_id: psp_return_id.to_owned(),
        amount_minor: amount,
        currency: "USD".to_owned(),
        effective_at: None,
    }
}

/// A return after a settle decrements `settled_minor` and drains the pool by the
/// returned amount (settle 1000, return 400 ⇒ settled 600, pool 600).
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn return_decrements_settled_and_pool() {
    let (_c, raw, provider) = boot().await;
    let s = setup_seller(&raw, &provider).await;
    let scope = AccessScope::for_tenant(s.tenant);

    settle(&provider, &s, "PAY-1", 1000).await;
    return_svc(&provider)
        .return_settlement(
            &SecurityContext::anonymous(),
            &scope,
            return_input(&s, "PAY-1", "RET-1", 400),
        )
        .await
        .expect("return must post");

    let repo = PaymentRepo::new(provider.clone());
    let row = repo
        .read_settlement(&scope, s.tenant, "PAY-1")
        .await
        .unwrap()
        .expect("settlement row present");
    assert_eq!(row.settled_minor, 600, "settled decremented by the return");
    assert_eq!(
        repo.read_unallocated(&scope, s.tenant, s.payer, "USD")
            .await
            .unwrap(),
        600,
        "pool drained by the returned amount"
    );
}

/// A re-posted return (same `psp_return_id`) replays idempotently — the second
/// call returns `replayed = true` and `settled_minor` decrements exactly once.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn return_replay_is_idempotent() {
    let (_c, raw, provider) = boot().await;
    let s = setup_seller(&raw, &provider).await;
    let scope = AccessScope::for_tenant(s.tenant);

    settle(&provider, &s, "PAY-2", 1000).await;
    let svc = return_svc(&provider);
    let fresh = svc
        .return_settlement(
            &SecurityContext::anonymous(),
            &scope,
            return_input(&s, "PAY-2", "RET-2", 400),
        )
        .await
        .expect("fresh return");
    assert!(!fresh.replayed, "first return is fresh");

    let replay = svc
        .return_settlement(
            &SecurityContext::anonymous(),
            &scope,
            return_input(&s, "PAY-2", "RET-2", 400),
        )
        .await
        .expect("replayed return");
    assert!(replay.replayed, "same psp_return_id replays");
    assert_eq!(
        replay.entry_id, fresh.entry_id,
        "replay returns the prior id"
    );

    // The decrement applied exactly once (settled 1000 - 400 = 600, not 200).
    let row = PaymentRepo::new(provider.clone())
        .read_settlement(&scope, s.tenant, "PAY-2")
        .await
        .unwrap()
        .expect("settlement row present");
    assert_eq!(row.settled_minor, 600, "decrement applied once, not twice");
}

/// A return exceeding a payment's OWN settled amount trips the per-payment cap
/// CHECK and surfaces as `SettlementReturnOverAllocated`. A second settlement
/// funds the SHARED unallocated pool so the 1500 return of PAY-3 does not
/// underflow it (which would raise `NegativeBalance` first) — isolating PAY-3's
/// `settled_minor` cap (you can't claw back more than this payment settled).
/// Mirrors `rest_payments::allocate_over_cap`. The row is left untouched (the
/// whole post rolled back).
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn return_exceeding_settled_is_rejected() {
    let (_c, raw, provider) = boot().await;
    let s = setup_seller(&raw, &provider).await;
    let scope = AccessScope::for_tenant(s.tenant);

    settle(&provider, &s, "PAY-3", 1000).await;
    // Fund the shared pool (pool = 2000) so the 1500 return clears the pool
    // no-negative guard and reaches PAY-3's own settled-cap CHECK.
    settle(&provider, &s, "PAY-OTHER", 1000).await;
    let err = return_svc(&provider)
        .return_settlement(
            &SecurityContext::anonymous(),
            &scope,
            return_input(&s, "PAY-3", "RET-3", 1500),
        )
        .await
        .expect_err("an over-claw must be rejected");
    assert!(
        matches!(err, DomainError::SettlementReturnOverAllocated(_)),
        "expected SettlementReturnOverAllocated, got {err:?}"
    );

    // The rejected return rolled back: settled_minor is still the full 1000.
    let row = PaymentRepo::new(provider.clone())
        .read_settlement(&scope, s.tenant, "PAY-3")
        .await
        .unwrap()
        .expect("settlement row present");
    assert_eq!(
        row.settled_minor, 1000,
        "rejected return left the row untouched"
    );
}

/// Fee-bearing FULL return (Model N, D1 — the mirror of settle): settle gross 100
/// with fee 3 ⇒ `DR CASH_CLEARING 97 · DR PSP_FEE_EXPENSE 3 · CR UNALLOCATED
/// 100`, so CASH_CLEARING holds only the net 97. A full return of 100 reverses it
/// symmetrically: `DR UNALLOCATED 100 · CR CASH_CLEARING 97 · CR PSP_FEE_EXPENSE
/// 3` ⇒ CASH_CLEARING drained by the NET 97 (97 → 0, NOT by the gross 100, which
/// would underflow the guarded clearing), PSP_FEE_EXPENSE reversed by 3 (3 → 0),
/// the pool emptied, and BOTH `settled_minor` and `fee_minor` zeroed.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn fee_bearing_full_return_reverses_net_and_fee() {
    let (_c, raw, provider) = boot().await;
    let s = setup_seller(&raw, &provider).await;
    let scope = AccessScope::for_tenant(s.tenant);

    // Settle gross 100 / fee 3 ⇒ CASH_CLEARING = net 97, PSP_FEE_EXPENSE = 3.
    settle_with_fee(&provider, &s, "PAY-FEE", 100, 3).await;
    assert_eq!(
        account_balance(&raw, &s, s.cash).await,
        Some(97),
        "settle lands NET 97 in CASH_CLEARING"
    );
    assert_eq!(
        account_balance(&raw, &s, s.psp_fee).await,
        Some(3),
        "the 3 fee is expensed to PSP_FEE_EXPENSE"
    );

    // Full return of the gross 100: symmetric reverse of settle.
    return_svc(&provider)
        .return_settlement(
            &SecurityContext::anonymous(),
            &scope,
            return_input(&s, "PAY-FEE", "RET-FEE", 100),
        )
        .await
        .expect("fee-bearing full return must post");

    // CASH_CLEARING reversed by the NET 97 (→ 0), not the gross 100.
    assert_eq!(
        account_balance(&raw, &s, s.cash).await,
        Some(0),
        "CASH_CLEARING reversed by the net 97 (97 → 0)"
    );
    // PSP_FEE_EXPENSE reversed by the full fee 3 (→ 0).
    assert_eq!(
        account_balance(&raw, &s, s.psp_fee).await,
        Some(0),
        "PSP_FEE_EXPENSE reversed by the fee 3 (3 → 0)"
    );
    // The pool is emptied (gross 100 in at settle, 100 out at return).
    assert_eq!(
        PaymentRepo::new(provider.clone())
            .read_unallocated(&scope, s.tenant, s.payer, "USD")
            .await
            .unwrap(),
        0,
        "the unallocated pool is emptied by the full return"
    );
    // BOTH counters zeroed (settled 100 → 0, fee 3 → 0).
    let row = PaymentRepo::new(provider.clone())
        .read_settlement(&scope, s.tenant, "PAY-FEE")
        .await
        .unwrap()
        .expect("settlement row present");
    assert_eq!(row.settled_minor, 0, "settled_minor zeroed by the return");
    assert_eq!(
        row.fee_minor, 0,
        "fee_minor zeroed by the proportional fee reverse"
    );
}

/// Regression (cross-currency return, H2): a settlement return sizes the
/// proportional fee slice off the SETTLED payment's counters and posts in the
/// request currency, so a return whose currency differs from the settlement (a
/// mistyped or malicious EUR return on a USD payment) must be rejected as
/// `CurrencyMismatch` before the fee share is sized or any leg posts — leaving
/// the settlement counters untouched.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn return_in_wrong_currency_is_rejected() {
    let (_c, raw, provider) = boot().await;
    let s = setup_seller(&raw, &provider).await;
    let scope = AccessScope::for_tenant(s.tenant);

    settle(&provider, &s, "PAY-XC", 1000).await; // settled in USD
    let err = return_svc(&provider)
        .return_settlement(
            &SecurityContext::anonymous(),
            &scope,
            SettlementReturnInput {
                tenant_id: s.tenant,
                payer_tenant_id: s.payer,
                payment_id: "PAY-XC".to_owned(),
                psp_return_id: "RET-XC".to_owned(),
                amount_minor: 400,
                currency: "EUR".to_owned(), // != settled USD
                effective_at: None,
            },
        )
        .await
        .expect_err("a cross-currency return must be rejected");
    assert!(
        matches!(err, DomainError::CurrencyMismatch(_)),
        "expected CurrencyMismatch, got {err:?}"
    );

    // The settlement counters are untouched (nothing decremented).
    let row = PaymentRepo::new(provider.clone())
        .read_settlement(&scope, s.tenant, "PAY-XC")
        .await
        .unwrap()
        .expect("settlement row present");
    assert_eq!(
        row.settled_minor, 1000,
        "a rejected return decrements nothing"
    );
}
