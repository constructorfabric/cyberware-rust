//! Postgres-only **concurrency** test for the chargeback slice (Group D): two
//! concurrent `ChargebackService::record_phase` posts on the SAME dispute
//! serialize under `SERIALIZABLE` (caller-retry, decision O). Ignored by default;
//! run with
//! `cargo test -p cf-gears-bss-ledger --test postgres_chargeback_concurrency -- --ignored`.
//!
//! Covers: a racing pair recording the SAME `lost` phase (same
//! `dispute_id:cycle:phase`) on one opened `CASH_HOLD` dispute must land EXACTLY
//! ONE ledger effect — the `(tenant, CHARGEBACK, business_id)` dedup admits one
//! winner; the loser replays the winner's finalized entry. The forfeiture posts
//! once and `clawed_back_minor` is NEVER double-counted (no double clawback).
//! Mirrors `postgres_payment_concurrency.rs`'s `retry_on_serialization` +
//! `tokio::join!` shape (and `postgres_payments::allocate_replay_makes_no_
//! duplicate_rows`, the same-key racing-claim pattern).
//!
//! Self-contained: re-declares the small `boot` / `setup_seller` / `settle`
//! helpers it needs (mirrors `postgres_payment_concurrency.rs`), so it does not
//! depend on `postgres_chargebacks.rs`.

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
use bss_ledger::domain::payment::chargeback::{DisputePhase, FundsAtOpen};
use bss_ledger::domain::payment::settlement::SettlementInput;
use bss_ledger::domain::ports::metrics::NoopLedgerMetrics;
use bss_ledger::infra::events::publisher::LedgerEventPublisher;
use bss_ledger::infra::payment::chargeback::{
    ChargebackOutcome, ChargebackRequest, ChargebackService,
};
use bss_ledger::infra::payment::settle::SettlementService;
use bss_ledger::infra::storage::migrations::Migrator;
use bss_ledger::infra::storage::repo::PaymentRepo;
use bss_ledger::infra::storage::repo::ReferenceRepo;
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
/// `DBProvider`. Mirrors `postgres_payment_concurrency::boot`.
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

/// Provisioned seller for the chargeback concurrency test (the cash-hold dispute
/// classes + the settle pool).
struct Seller {
    tenant: Uuid,
    payer: Uuid,
    cash: Uuid,
    dispute_hold: Uuid,
    dispute_loss: Uuid,
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
/// and the cash-hold dispute chart accounts (CASH_CLEARING debit, UNALLOCATED
/// credit, DISPUTE_HOLD debit, DISPUTE_LOSS_EXPENSE debit). Mirrors
/// `postgres_payment_concurrency::setup_seller`.
async fn setup_seller(raw: &sea_orm::DatabaseConnection, provider: &DBProvider<DbError>) -> Seller {
    let now = OffsetDateTime::now_utc();
    let s = Seller {
        tenant: Uuid::now_v7(),
        payer: Uuid::now_v7(),
        cash: Uuid::now_v7(),
        dispute_hold: Uuid::now_v7(),
        dispute_loss: Uuid::now_v7(),
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
    ] {
        reference.insert_account(row).await.unwrap();
    }
    s
}

fn chargeback_svc(provider: &DBProvider<DbError>) -> ChargebackService {
    ChargebackService::new(
        provider.clone(),
        Arc::new(LedgerEventPublisher::noop()),
        Arc::new(NoopLedgerMetrics),
    )
}

/// Settle `gross` (fee 0) for `payment_id` — funds CASH_CLEARING + seeds the
/// settlement counter (`settled_minor = gross`) the clawback cap nets against.
async fn settle(provider: &DBProvider<DbError>, s: &Seller, payment_id: &str, gross: i64) {
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
    .expect("settle must succeed");
}

/// Client-side retry of a service call on a projector-level serialization
/// conflict (40001 stringified into `DomainError::Internal("…serialize…")`).
/// Decision O defers recompute-on-retry to the CALLER, so the test models what
/// the SDK/client does: re-run the WHOLE operation until it commits or hits a
/// real business rejection. A genuine deadlock (40P01) is NOT a serialization
/// conflict and propagates — failing the test loudly (the deadlock-freedom
/// guarantee). Copied verbatim from `postgres_payment_concurrency.rs`.
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

/// Read a chart account's cached `balance_minor` (or `None` if never posted to).
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

/// Chargeback #D-1: two concurrent records of the SAME `lost` phase on ONE
/// opened `CASH_HOLD` dispute (same `dispute_id:cycle:phase`, so the dedup key
/// COLLIDES) must land EXACTLY ONE ledger effect. Under `SERIALIZABLE` the two
/// posts serialize at the dispute row + the clawback counter; the
/// `(tenant, CHARGEBACK, business_id)` dedup admits one winner and the loser
/// replays the winner's finalized entry (the client retries a projector-level
/// 40001 via `retry_on_serialization`). The forfeiture posts once
/// (DISPUTE_LOSS_EXPENSE = 1000, DISPUTE_HOLD = 0) and `clawed_back_minor` is
/// NEVER double-counted (it lands at EXACTLY the disputed 1000 — no double
/// clawback). Mirrors `postgres_payments::allocate_replay_makes_no_duplicate_
/// rows`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Docker (testcontainers)"]
async fn concurrent_lost_on_one_dispute_claws_back_once() {
    let (_c, raw, provider) = boot().await;
    let s = setup_seller(&raw, &provider).await;
    let ctx = SecurityContext::anonymous();
    let scope = AccessScope::for_tenant(s.tenant);

    // Settle 1000 (funds CASH_CLEARING + the settlement counter) and open a
    // cash-hold dispute (moves the 1000 into DISPUTE_HOLD).
    settle(&provider, &s, "PAY-CC-1", 1000).await;
    chargeback_svc(&provider)
        .record_phase(
            &ctx,
            &scope,
            ChargebackRequest {
                tenant_id: s.tenant,
                payer_tenant_id: s.payer,
                payment_id: "PAY-CC-1".to_owned(),
                dispute_id: "DSP-CC-1".to_owned(),
                invoice_id: None,
                cycle: 1,
                phase: DisputePhase::Opened,
                funds_at_open: FundsAtOpen::Withheld,
                disputed_amount_minor: 1000,
                currency: "USD".to_owned(),
                effective_at: None,
            },
        )
        .await
        .expect("opened cash-hold");

    // The SAME `lost` phase on both racing requests ⇒ the dedup key collides; at
    // most one may post the forfeiture, the other replays it.
    let request = || ChargebackRequest {
        tenant_id: s.tenant,
        payer_tenant_id: s.payer,
        payment_id: "PAY-CC-1".to_owned(),
        dispute_id: "DSP-CC-1".to_owned(),
        invoice_id: None,
        cycle: 1,
        phase: DisputePhase::Lost,
        funds_at_open: FundsAtOpen::Withheld,
        disputed_amount_minor: 1000,
        currency: "USD".to_owned(),
        effective_at: None,
    };

    let svc_a = chargeback_svc(&provider);
    let svc_b = chargeback_svc(&provider);
    let scope_a = scope.clone();
    let scope_b = scope.clone();
    let ctx_a = SecurityContext::anonymous();
    let ctx_b = SecurityContext::anonymous();
    let (ra, rb) = tokio::join!(
        async move { retry_on_serialization(|| svc_a.record_phase(&ctx_a, &scope_a, request())).await },
        async move { retry_on_serialization(|| svc_b.record_phase(&ctx_b, &scope_b, request())).await },
    );

    // Both sides resolved cleanly (a deadlock would have propagated out of
    // `retry_on_serialization`); at least one made progress. A `lost` is an inline
    // post, so each Ok is a `Recorded` (one fresh, the other a replay).
    for r in [&ra, &rb] {
        match r {
            Ok(ChargebackOutcome::Recorded(_)) => {}
            Ok(ChargebackOutcome::Queued(q)) => {
                panic!("a lost on an opened dispute posts inline, got Queued: {q:?}")
            }
            Err(e) => panic!("a racing lost must succeed (post or replay), got: {e:?}"),
        }
    }
    let replays = i32::from(matches!(&ra, Ok(ChargebackOutcome::Recorded(r)) if r.replayed))
        + i32::from(matches!(&rb, Ok(ChargebackOutcome::Recorded(r)) if r.replayed));
    assert!(
        replays >= 1,
        "exactly one side wins; the other replays the finalized entry: {ra:?} / {rb:?}"
    );

    // INVARIANT: the forfeiture posted EXACTLY once — DISPUTE_LOSS_EXPENSE = 1000,
    // DISPUTE_HOLD emptied — and `clawed_back_minor` was not double-counted.
    assert_eq!(
        account_balance(&raw, &s, s.dispute_loss).await,
        Some(1000),
        "the forfeiture booked exactly once into DISPUTE_LOSS_EXPENSE"
    );
    assert_eq!(
        account_balance(&raw, &s, s.dispute_hold).await,
        Some(0),
        "the hold is emptied exactly once"
    );

    let row = PaymentRepo::new(provider.clone())
        .read_settlement(&scope, s.tenant, "PAY-CC-1")
        .await
        .unwrap()
        .expect("settlement row present");
    assert_eq!(
        row.clawed_back_minor, 1000,
        "clawed_back_minor reflects exactly one clawback (1000), never doubled"
    );
    assert!(
        row.refunded_minor + row.clawed_back_minor <= row.settled_minor,
        "the total money-out cap held: refunded ({}) + clawed ({}) <= settled ({})",
        row.refunded_minor,
        row.clawed_back_minor,
        row.settled_minor
    );

    // The dispute resolved to LOST (advanced once).
    let phase = raw
        .query_one_raw(pg(format!(
            "SELECT last_phase FROM bss.ledger_dispute \
             WHERE tenant_id='{}' AND dispute_id='DSP-CC-1'",
            s.tenant
        )))
        .await
        .unwrap()
        .map(|r| r.try_get_by_index::<String>(0).unwrap());
    assert_eq!(
        phase,
        Some("LOST".to_owned()),
        "the dispute advanced to LOST"
    );
}

/// Chargeback #D-2 (the won/lost outcome race — regression for the missing
/// `(last_phase, cycle)` predicate on `dispute_advance`): a `won` and a `lost` on
/// the SAME opened cycle carry DISTINCT dedup keys (`…:won` vs `…:lost`), so BOTH
/// clear the dedup gate AND the out-of-txn transition guard and reach the sidecar.
/// Exactly ONE may resolve the dispute; the loser MUST be rejected as a clean
/// `InvalidDisputeTransition` — never commit a second outcome entry over the
/// first, never surface as a 500. Under `SERIALIZABLE` one commits; the other
/// finds the row no longer `OPENED` at this cycle (its in-txn UPDATE matches 0
/// rows, or, after a caller retry on a projector 40001, the out-of-txn guard sees
/// the resolved row) and is rejected.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Docker (testcontainers)"]
async fn concurrent_won_and_lost_resolves_one_outcome() {
    let (_c, raw, provider) = boot().await;
    let s = setup_seller(&raw, &provider).await;
    let ctx = SecurityContext::anonymous();
    let scope = AccessScope::for_tenant(s.tenant);

    settle(&provider, &s, "PAY-CC-2", 1000).await;
    chargeback_svc(&provider)
        .record_phase(
            &ctx,
            &scope,
            ChargebackRequest {
                tenant_id: s.tenant,
                payer_tenant_id: s.payer,
                payment_id: "PAY-CC-2".to_owned(),
                dispute_id: "DSP-CC-2".to_owned(),
                invoice_id: None,
                cycle: 1,
                phase: DisputePhase::Opened,
                funds_at_open: FundsAtOpen::Withheld,
                disputed_amount_minor: 1000,
                currency: "USD".to_owned(),
                effective_at: None,
            },
        )
        .await
        .expect("opened cash-hold");

    // Distinct dedup keys (won vs lost) — neither is deduped against the other.
    let won_req = || ChargebackRequest {
        tenant_id: s.tenant,
        payer_tenant_id: s.payer,
        payment_id: "PAY-CC-2".to_owned(),
        dispute_id: "DSP-CC-2".to_owned(),
        invoice_id: None,
        cycle: 1,
        phase: DisputePhase::Won,
        funds_at_open: FundsAtOpen::Withheld,
        disputed_amount_minor: 1000,
        currency: "USD".to_owned(),
        effective_at: None,
    };
    let lost_req = || ChargebackRequest {
        tenant_id: s.tenant,
        payer_tenant_id: s.payer,
        payment_id: "PAY-CC-2".to_owned(),
        dispute_id: "DSP-CC-2".to_owned(),
        invoice_id: None,
        cycle: 1,
        phase: DisputePhase::Lost,
        funds_at_open: FundsAtOpen::Withheld,
        disputed_amount_minor: 1000,
        currency: "USD".to_owned(),
        effective_at: None,
    };

    let svc_a = chargeback_svc(&provider);
    let svc_b = chargeback_svc(&provider);
    let scope_a = scope.clone();
    let scope_b = scope.clone();
    let ctx_a = SecurityContext::anonymous();
    let ctx_b = SecurityContext::anonymous();
    let (won, lost) = tokio::join!(
        async move { retry_on_serialization(|| svc_a.record_phase(&ctx_a, &scope_a, won_req())).await },
        async move { retry_on_serialization(|| svc_b.record_phase(&ctx_b, &scope_b, lost_req())).await },
    );

    // EXACTLY ONE outcome resolved; the other is a clean invalid-transition
    // rejection (NOT a second commit, NOT an Internal/500).
    let oks = i32::from(won.is_ok()) + i32::from(lost.is_ok());
    assert_eq!(
        oks, 1,
        "exactly one outcome may win: won={won:?} lost={lost:?}"
    );
    for r in [&won, &lost] {
        if let Err(e) = r {
            assert!(
                matches!(e, DomainError::InvalidDisputeTransition(_)),
                "the losing outcome must be a clean InvalidDisputeTransition, got {e:?}"
            );
        }
    }

    // The dispute sits at EXACTLY ONE terminal phase, matching the winner; the
    // hold is emptied once either way (won releases it, lost forfeits it).
    let winner_phase = if won.is_ok() { "WON" } else { "LOST" };
    let phase = raw
        .query_one_raw(pg(format!(
            "SELECT last_phase FROM bss.ledger_dispute \
             WHERE tenant_id='{}' AND dispute_id='DSP-CC-2'",
            s.tenant
        )))
        .await
        .unwrap()
        .map(|r| r.try_get_by_index::<String>(0).unwrap());
    assert_eq!(
        phase.as_deref(),
        Some(winner_phase),
        "the dispute resolved to the winning outcome exactly once"
    );
    assert_eq!(
        account_balance(&raw, &s, s.dispute_hold).await,
        Some(0),
        "DISPUTE_HOLD drained by the single applied outcome"
    );
    // The clawback counter reflects the winner: 0 on a won, the full 1000 on a lost.
    let want_clawed = if won.is_ok() { 0 } else { 1000 };
    let row = PaymentRepo::new(provider.clone())
        .read_settlement(&scope, s.tenant, "PAY-CC-2")
        .await
        .unwrap()
        .expect("settlement row present");
    assert_eq!(
        row.clawed_back_minor, want_clawed,
        "clawed_back reflects the single winning outcome (won=>0, lost=>1000)"
    );
}
