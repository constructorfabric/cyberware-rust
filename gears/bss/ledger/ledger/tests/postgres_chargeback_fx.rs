//! Postgres-only integration (Slice 5, Phase 2 F3): **functional carry-forward on
//! a cross-currency chargeback close**, driven through the REAL orchestrators
//! (`SettlementService` / `InvoicePostService` / `ChargebackService`) against the
//! foundation engine.
//!
//! A chargeback reclassifies a position WITHOUT locking a new rate, so the
//! functional cost basis carries forward and the entry's functional column nets to
//! ZERO — there is **no realized FX_GAIN_LOSS line** (realized FX is recognised at
//! the cash in/out points: settle S2 / refund S3). These tests prove the
//! carry-forward keeps each closing grain's functional column in lockstep with
//! `balance_minor` for both dispute variants:
//!
//! - `cash_hold_dispute_carries_functional_forward_no_fx`: a USD-functional seller
//!   settles 120 EUR @ 1.08 (CASH_CLEARING carries 129.60 USD); a CASH_HOLD dispute
//!   moves it to DISPUTE_HOLD at `opened` (CASH→0, HOLD→129.60) and back at `won`
//!   (HOLD→0, CASH→129.60). The 129.60 USD cost basis round-trips; no FX line.
//! - `ar_reclass_lost_writes_off_at_carried_basis_no_fx`: an EUR invoice posts AR
//!   @ 1.10 (132.00 USD); an AR_RECLASS dispute reclasses it `opened`, then a `lost`
//!   write-off clears AR to (0, 0) and books DISPUTE_LOSS_EXPENSE at the 132.00 USD
//!   carried cost basis; no FX line.
//!
//! Ignored by default; run with `-- --ignored`.

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

use bss_ledger::config::{FxConfig, RecognitionConfig};
use bss_ledger::domain::instant::to_naive_date;
use bss_ledger::domain::invoice::builder::{InvoiceItem, PostedInvoice};
use bss_ledger::domain::model::{AccountRow, CurrencyScaleRow};
use bss_ledger::domain::money::DEFAULT_PLAUSIBLE_MAX_MAJOR;
use bss_ledger::domain::payment::chargeback::{DisputePhase, FundsAtOpen};
use bss_ledger::domain::payment::settlement::SettlementInput;
use bss_ledger::domain::ports::metrics::NoopLedgerMetrics;
use bss_ledger::infra::events::publisher::LedgerEventPublisher;
use bss_ledger::infra::fx::rate_locker::RateLocker;
use bss_ledger::infra::fx::rate_source::RateSource;
use bss_ledger::infra::invoice_post::InvoicePostService;
use bss_ledger::infra::payment::chargeback::{
    ChargebackOutcome, ChargebackRequest, ChargebackService,
};
use bss_ledger::infra::payment::settle::SettlementService;
use bss_ledger::infra::storage::migrations::Migrator;
use bss_ledger::infra::storage::repo::{FxRepo, NewFxRate, ReferenceRepo};
use bss_ledger_sdk::AccountClass;
use chrono::NaiveDate;
use sea_orm::{ConnectionTrait, Database, DatabaseConnection, Statement};
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

async fn scalar_i64(conn: &DatabaseConnection, sql: &str) -> Option<i64> {
    conn.query_one_raw(pg(sql.to_owned()))
        .await
        .unwrap()
        .map(|r| r.try_get_by_index::<i64>(0).unwrap())
}

fn naive(y: i32, m: u32, d: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, m, d).unwrap()
}

fn account(
    tenant: Uuid,
    id: Uuid,
    class: AccountClass,
    normal: &str,
    currency: &str,
    stream: Option<&str>,
) -> AccountRow {
    AccountRow {
        account_id: id,
        tenant_id: tenant,
        legal_entity_id: tenant,
        account_class: class.as_str().to_owned(),
        currency: currency.to_owned(),
        revenue_stream: stream.map(str::to_owned),
        normal_side: normal.to_owned(),
        may_go_negative: false,
        lifecycle_state: "OPEN".to_owned(),
    }
}

fn rate_locker(provider: &DBProvider<DbError>, fx_config: &FxConfig) -> RateLocker {
    RateLocker::new(
        RateSource::new(FxRepo::new(provider.clone()), fx_config.clone()),
        FxRepo::new(provider.clone()),
    )
}

/// The chart account ids a cross-currency dispute seller needs.
struct Chart {
    tenant: Uuid,
    payer: Uuid,
    cash: Uuid,
    unallocated: Uuid,
    dispute_hold: Uuid,
    dispute_loss: Uuid,
    ar: Uuid,
    revenue: Uuid,
    fx_gl: Uuid,
}

/// Boot a container + provision a USD-functional / EUR-billing seller: USD+EUR
/// scales, the functional-currency source (S5-F3), an OPEN current-month period,
/// the EUR dispute/payment chart, and the USD FX_GAIN_LOSS account (provisioned so
/// the "no FX line" assertions are positive — the account EXISTS but stays
/// untouched, not merely absent). Seeds the EUR→USD rate at `rate_micro`.
async fn setup(
    rate_micro: i64,
) -> (
    testcontainers_modules::testcontainers::ContainerAsync<Postgres>,
    DatabaseConnection,
    DBProvider<DbError>,
    Chart,
    String,
) {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let raw = Database::connect(&url).await.unwrap();
    Migrator::up(&raw, None).await.unwrap();
    let repo_url = format!("{url}?options=-c%20search_path%3Dbss,public");
    let provider =
        DBProvider::<DbError>::new(connect_db(&repo_url, ConnectOpts::default()).await.unwrap());

    let c = Chart {
        tenant: Uuid::now_v7(),
        payer: Uuid::now_v7(),
        cash: Uuid::now_v7(),
        unallocated: Uuid::now_v7(),
        dispute_hold: Uuid::now_v7(),
        dispute_loss: Uuid::now_v7(),
        ar: Uuid::now_v7(),
        revenue: Uuid::now_v7(),
        fx_gl: Uuid::now_v7(),
    };
    let now = OffsetDateTime::now_utc();
    let period_id = bss_ledger::domain::instant::yyyymm(now);

    let reference = ReferenceRepo::new(provider.clone());
    for ccy in ["EUR", "USD"] {
        reference
            .upsert_currency_scale(CurrencyScaleRow {
                tenant_id: c.tenant,
                currency: ccy.to_owned(),
                minor_units: 2,
                plausible_max_major: DEFAULT_PLAUSIBLE_MAX_MAJOR,
                source: "iso".to_owned(),
            })
            .await
            .unwrap();
    }
    raw.execute_raw(pg(format!(
        "INSERT INTO bss.ledger_fiscal_calendar
           (tenant_id, legal_entity_id, fiscal_tz, granularity, fy_start_month, functional_currency)
         VALUES ('{}','{}','UTC','MONTH',1,'USD')",
        c.tenant, c.tenant
    )))
    .await
    .unwrap();
    raw.execute_raw(pg(format!(
        "INSERT INTO bss.ledger_fiscal_period (tenant_id, legal_entity_id, period_id, fiscal_tz, status)
         VALUES ('{}','{}','{period_id}','UTC','OPEN')",
        c.tenant, c.tenant
    )))
    .await
    .unwrap();
    for row in [
        account(
            c.tenant,
            c.cash,
            AccountClass::CashClearing,
            "DR",
            "EUR",
            None,
        ),
        account(
            c.tenant,
            c.unallocated,
            AccountClass::Unallocated,
            "CR",
            "EUR",
            None,
        ),
        account(
            c.tenant,
            c.dispute_hold,
            AccountClass::DisputeHold,
            "DR",
            "EUR",
            None,
        ),
        account(
            c.tenant,
            c.dispute_loss,
            AccountClass::DisputeLossExpense,
            "DR",
            "EUR",
            None,
        ),
        account(c.tenant, c.ar, AccountClass::Ar, "DR", "EUR", None),
        account(
            c.tenant,
            c.revenue,
            AccountClass::Revenue,
            "CR",
            "EUR",
            Some("subscription"),
        ),
        account(
            c.tenant,
            c.fx_gl,
            AccountClass::FxGainLoss,
            "DR",
            "USD",
            None,
        ),
    ] {
        reference.insert_account(row).await.unwrap();
    }
    FxRepo::new(provider.clone())
        .upsert_rate(&NewFxRate {
            tenant_id: c.tenant,
            base_currency: "EUR".to_owned(),
            quote_currency: "USD".to_owned(),
            provider: "ecb".to_owned(),
            rate_micro,
            as_of: now,
            fallback_order: 0,
        })
        .await
        .unwrap();

    (container, raw, provider, c, period_id)
}

fn fx_config() -> FxConfig {
    FxConfig {
        provider_order: vec!["ecb".to_owned()],
        ..FxConfig::default()
    }
}

/// Functional column net (DR − CR) over one entry — must be 0 for a carry-forward.
async fn entry_functional_net(raw: &DatabaseConnection, tenant: Uuid, entry: Uuid) -> Option<i64> {
    scalar_i64(
        raw,
        &format!(
            "SELECT COALESCE(SUM(CASE WHEN side='DR' THEN functional_amount_minor \
             ELSE -functional_amount_minor END),0)::bigint \
             FROM bss.ledger_journal_line WHERE tenant_id='{tenant}' AND entry_id='{entry}'"
        ),
    )
    .await
}

async fn acct_balance(
    raw: &DatabaseConnection,
    tenant: Uuid,
    account: Uuid,
) -> (Option<i64>, Option<i64>) {
    let bal = scalar_i64(raw, &format!(
        "SELECT balance_minor FROM bss.ledger_account_balance WHERE tenant_id='{tenant}' AND account_id='{account}'"
    )).await;
    let func = scalar_i64(raw, &format!(
        "SELECT functional_balance_minor FROM bss.ledger_account_balance WHERE tenant_id='{tenant}' AND account_id='{account}'"
    )).await;
    (bal, func)
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn cash_hold_dispute_carries_functional_forward_no_fx() {
    // EUR→USD @ 1.08 at settle.
    let (_c, raw, provider, chart, _period) = setup(1_080_000).await;
    let ctx = SecurityContext::anonymous();
    let scope = AccessScope::for_tenant(chart.tenant);
    let cfg = fx_config();

    // Settle 120.00 EUR @ 1.08 ⇒ CASH_CLEARING carries 120.00 EUR / 129.60 USD.
    SettlementService::new(
        provider.clone(),
        Arc::new(LedgerEventPublisher::noop()),
        Arc::new(NoopLedgerMetrics),
    )
    .with_fx(rate_locker(&provider, &cfg))
    .settle(
        &ctx,
        &scope,
        SettlementInput {
            tenant_id: chart.tenant,
            payer_tenant_id: chart.payer,
            payment_id: "PAY-CB-1".to_owned(),
            gross_minor: 12_000,
            fee_minor: 0,
            currency: "EUR".to_owned(),
            effective_at: None,
        },
    )
    .await
    .expect("cross-currency settle must post");
    assert_eq!(
        acct_balance(&raw, chart.tenant, chart.cash).await,
        (Some(12_000), Some(12_960)),
        "CASH_CLEARING carries 120.00 EUR / 129.60 USD after settle"
    );

    let cb = ChargebackService::new(
        provider.clone(),
        Arc::new(LedgerEventPublisher::noop()),
        Arc::new(NoopLedgerMetrics),
    );
    let request = |phase: DisputePhase| ChargebackRequest {
        tenant_id: chart.tenant,
        payer_tenant_id: chart.payer,
        payment_id: "PAY-CB-1".to_owned(),
        dispute_id: "D-1".to_owned(),
        invoice_id: None,
        cycle: 1,
        phase,
        funds_at_open: FundsAtOpen::Withheld, // ⇒ CASH_HOLD
        disputed_amount_minor: 12_000,
        currency: "EUR".to_owned(),
        effective_at: None,
    };

    // opened: DR DISPUTE_HOLD 12000 / CR CASH_CLEARING 12000 — the 129.60 USD cost
    // basis carries forward from CASH_CLEARING (→ 0) into DISPUTE_HOLD.
    let opened = match cb
        .record_phase(&ctx, &scope, request(DisputePhase::Opened))
        .await
    {
        Ok(ChargebackOutcome::Recorded(p)) => p.entry_id,
        other => panic!("opened must post inline: {other:?}"),
    };
    assert_eq!(
        entry_functional_net(&raw, chart.tenant, opened).await,
        Some(0),
        "opened entry functional column balances (carry-forward, no FX)"
    );
    assert_eq!(
        acct_balance(&raw, chart.tenant, chart.cash).await,
        (Some(0), Some(0)),
        "CASH_CLEARING closes to (0, 0) — the held cash AND its cost basis left"
    );
    assert_eq!(
        acct_balance(&raw, chart.tenant, chart.dispute_hold).await,
        (Some(12_000), Some(12_960)),
        "DISPUTE_HOLD carries the 120.00 EUR / 129.60 USD cost basis forward"
    );

    // won: DR CASH_CLEARING 12000 / CR DISPUTE_HOLD 12000 — the cost basis returns.
    let won = match cb
        .record_phase(&ctx, &scope, request(DisputePhase::Won))
        .await
    {
        Ok(ChargebackOutcome::Recorded(p)) => p.entry_id,
        other => panic!("won must post inline: {other:?}"),
    };
    assert_eq!(
        entry_functional_net(&raw, chart.tenant, won).await,
        Some(0),
        "won entry functional column balances (carry-forward, no FX)"
    );
    assert_eq!(
        acct_balance(&raw, chart.tenant, chart.dispute_hold).await,
        (Some(0), Some(0)),
        "DISPUTE_HOLD closes to (0, 0) on won"
    );
    assert_eq!(
        acct_balance(&raw, chart.tenant, chart.cash).await,
        (Some(12_000), Some(12_960)),
        "CASH_CLEARING regains 120.00 EUR / 129.60 USD — cost basis round-tripped"
    );

    // NO realized FX line was ever posted (the FX_GAIN_LOSS account exists but is
    // untouched) — a chargeback realizes no FX (carry-forward).
    assert_eq!(
        scalar_i64(&raw, &format!(
            "SELECT count(*) FROM bss.ledger_journal_line WHERE tenant_id='{}' AND account_id='{}'",
            chart.tenant, chart.fx_gl
        )).await,
        Some(0),
        "a chargeback posts NO FX_GAIN_LOSS line (carry-forward, not realized FX)"
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn ar_reclass_lost_writes_off_at_carried_basis_no_fx() {
    // EUR→USD @ 1.10 at invoice post.
    let (_c, raw, provider, chart, period_id) = setup(1_100_000).await;
    let ctx = SecurityContext::anonymous();
    let scope = AccessScope::for_tenant(chart.tenant);
    let cfg = fx_config();

    // Post an EUR invoice @ 1.10 ⇒ AR carries 120.00 EUR / 132.00 USD.
    InvoicePostService::new(
        provider.clone(),
        Arc::new(LedgerEventPublisher::noop()),
        Arc::new(NoopLedgerMetrics),
        RecognitionConfig::default(),
        cfg.clone(),
    )
    .post_invoice(
        &ctx,
        &scope,
        &PostedInvoice {
            invoice_id: "INV-CB-1".to_owned(),
            payer_tenant_id: chart.payer,
            resource_tenant_id: None,
            seller_tenant_id: chart.tenant,
            effective_at: to_naive_date(OffsetDateTime::now_utc()),
            due_date: Some(naive(2026, 12, 1)),
            period_id,
            items: vec![InvoiceItem {
                amount_minor_ex_tax: 12_000,
                deferred_minor: 0,
                currency: "EUR".to_owned(),
                revenue_stream: "subscription".to_owned(),
                catalog_class: Some(AccountClass::Revenue),
                contract_class: None,
                gl_code: Some("4000".to_owned()),
                recognition: None,
                invoice_item_ref: None,
                sku_or_plan_ref: None,
                price_id: None,
                pricing_snapshot_ref: None,
            }],
            tax: vec![],
            posted_by_actor_id: chart.tenant,
            correlation_id: chart.tenant,
        },
        true,
    )
    .await
    .expect("cross-currency invoice must post");

    let ar_carried = || async {
        let bal = scalar_i64(&raw, &format!(
            "SELECT balance_minor FROM bss.ledger_ar_invoice_balance WHERE tenant_id='{}' AND invoice_id='INV-CB-1'",
            chart.tenant
        )).await;
        let func = scalar_i64(&raw, &format!(
            "SELECT functional_balance_minor FROM bss.ledger_ar_invoice_balance WHERE tenant_id='{}' AND invoice_id='INV-CB-1'",
            chart.tenant
        )).await;
        (bal, func)
    };
    assert_eq!(
        ar_carried().await,
        (Some(12_000), Some(13_200)),
        "AR carries 120.00 EUR / 132.00 USD"
    );

    let cb = ChargebackService::new(
        provider.clone(),
        Arc::new(LedgerEventPublisher::noop()),
        Arc::new(NoopLedgerMetrics),
    );
    let request = |phase: DisputePhase| ChargebackRequest {
        tenant_id: chart.tenant,
        payer_tenant_id: chart.payer,
        payment_id: "PAY-CB-2".to_owned(),
        dispute_id: "D-2".to_owned(),
        invoice_id: Some("INV-CB-1".to_owned()),
        cycle: 1,
        phase,
        funds_at_open: FundsAtOpen::NotMoved, // ⇒ AR_RECLASS
        disputed_amount_minor: 12_000,
        currency: "EUR".to_owned(),
        effective_at: None,
    };

    // opened: DR AR DISPUTED / CR AR ACTIVE — same grain, nets ZERO on both columns
    // (reclass); AR carried (12000, 13200) unchanged, disputed sub-balance → 12000.
    let opened = match cb
        .record_phase(&ctx, &scope, request(DisputePhase::Opened))
        .await
    {
        Ok(ChargebackOutcome::Recorded(p)) => p.entry_id,
        other => panic!("opened must post inline: {other:?}"),
    };
    assert_eq!(
        entry_functional_net(&raw, chart.tenant, opened).await,
        Some(0),
        "opened reclass nets functional to 0"
    );
    assert_eq!(
        ar_carried().await,
        (Some(12_000), Some(13_200)),
        "AR carried unchanged by a reclass"
    );

    // lost: DR DISPUTE_LOSS_EXPENSE / CR AR DISPUTED — write off AR to (0, 0); the
    // loss is booked at the 132.00 USD carried cost basis (carry-forward, no FX).
    let lost = match cb
        .record_phase(&ctx, &scope, request(DisputePhase::Lost))
        .await
    {
        Ok(ChargebackOutcome::Recorded(p)) => p.entry_id,
        other => panic!("lost must post inline: {other:?}"),
    };
    assert_eq!(
        entry_functional_net(&raw, chart.tenant, lost).await,
        Some(0),
        "lost write-off functional balances"
    );
    assert_eq!(
        ar_carried().await,
        (Some(0), Some(0)),
        "AR written off to (0, 0) in both columns"
    );
    assert_eq!(
        acct_balance(&raw, chart.tenant, chart.dispute_loss).await,
        (Some(12_000), Some(13_200)),
        "DISPUTE_LOSS_EXPENSE booked at the 132.00 USD carried cost basis"
    );
    assert_eq!(
        scalar_i64(&raw, &format!(
            "SELECT count(*) FROM bss.ledger_journal_line WHERE tenant_id='{}' AND account_id='{}'",
            chart.tenant, chart.fx_gl
        )).await,
        Some(0),
        "a write-off realizes no FX (carry-forward at the carried basis)"
    );
}
