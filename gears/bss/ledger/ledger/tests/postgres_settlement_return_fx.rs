//! Postgres-only integration (Slice 5 remediation): **functional carry-forward on
//! a cross-currency settlement-return**, driven through the REAL orchestrators
//! (`SettlementService` / `SettlementReturnService`) against the foundation engine.
//!
//! A settlement-return is the SYMMETRIC reverse of a cross-currency settle: it
//! claws the gross back out of the `UNALLOCATED` pool and relieves the cash + fee
//! legs at the pool's carried (settle-time) rate — a reversal, NOT a realized-FX
//! point, so the functional column nets to ZERO with no `FX_GAIN_LOSS` line (the
//! Slice-5 remediation fix: before it, the return posted transaction-only and left
//! the functional balances drifting). This test proves a full settle→return
//! round-trip drains every grain to `(0, 0)` in BOTH columns, including the fee leg
//! whose functional is the residual `dr_func − fee_func`.
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

use bss_ledger::config::FxConfig;
use bss_ledger::domain::model::{AccountRow, CurrencyScaleRow};
use bss_ledger::domain::money::DEFAULT_PLAUSIBLE_MAX_MAJOR;
use bss_ledger::domain::payment::settlement::SettlementInput;
use bss_ledger::domain::payment::settlement_return::SettlementReturnInput;
use bss_ledger::domain::ports::metrics::NoopLedgerMetrics;
use bss_ledger::infra::events::publisher::LedgerEventPublisher;
use bss_ledger::infra::fx::rate_locker::RateLocker;
use bss_ledger::infra::fx::rate_source::RateSource;
use bss_ledger::infra::payment::settle::SettlementService;
use bss_ledger::infra::payment::settlement_return::SettlementReturnService;
use bss_ledger::infra::storage::migrations::Migrator;
use bss_ledger::infra::storage::repo::{FxRepo, NewFxRate, ReferenceRepo};
use bss_ledger_sdk::AccountClass;
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

fn account(
    tenant: Uuid,
    id: Uuid,
    class: AccountClass,
    normal: &str,
    currency: &str,
) -> AccountRow {
    AccountRow {
        account_id: id,
        tenant_id: tenant,
        legal_entity_id: tenant,
        account_class: class.as_str().to_owned(),
        currency: currency.to_owned(),
        revenue_stream: None,
        normal_side: normal.to_owned(),
        may_go_negative: false,
        lifecycle_state: "OPEN".to_owned(),
    }
}

fn fx_config() -> FxConfig {
    FxConfig {
        provider_order: vec!["ecb".to_owned()],
        ..FxConfig::default()
    }
}

fn rate_locker(provider: &DBProvider<DbError>) -> RateLocker {
    RateLocker::new(
        RateSource::new(FxRepo::new(provider.clone()), fx_config()),
        FxRepo::new(provider.clone()),
    )
}

struct Chart {
    tenant: Uuid,
    payer: Uuid,
    unallocated: Uuid,
    cash: Uuid,
    psp_fee: Uuid,
    fx_gl: Uuid,
}

async fn acct(raw: &DatabaseConnection, tenant: Uuid, account: Uuid) -> (Option<i64>, Option<i64>) {
    let bal = scalar_i64(raw, &format!(
        "SELECT balance_minor FROM bss.ledger_account_balance WHERE tenant_id='{tenant}' AND account_id='{account}'"
    )).await;
    let func = scalar_i64(raw, &format!(
        "SELECT functional_balance_minor FROM bss.ledger_account_balance WHERE tenant_id='{tenant}' AND account_id='{account}'"
    )).await;
    (bal, func)
}

async fn unalloc(
    raw: &DatabaseConnection,
    tenant: Uuid,
    payer: Uuid,
) -> (Option<i64>, Option<i64>) {
    let bal = scalar_i64(raw, &format!(
        "SELECT balance_minor FROM bss.ledger_unallocated_balance WHERE tenant_id='{tenant}' AND payer_tenant_id='{payer}'"
    )).await;
    let func = scalar_i64(raw, &format!(
        "SELECT functional_balance_minor FROM bss.ledger_unallocated_balance WHERE tenant_id='{tenant}' AND payer_tenant_id='{payer}'"
    )).await;
    (bal, func)
}

async fn entry_functional_net(raw: &DatabaseConnection, tenant: Uuid, entry: Uuid) -> Option<i64> {
    scalar_i64(raw, &format!(
        "SELECT COALESCE(SUM(CASE WHEN side='DR' THEN functional_amount_minor ELSE -functional_amount_minor END),0)::bigint \
         FROM bss.ledger_journal_line WHERE tenant_id='{tenant}' AND entry_id='{entry}'"
    )).await
}

async fn fx_line_count(raw: &DatabaseConnection, tenant: Uuid, fx_gl: Uuid) -> Option<i64> {
    scalar_i64(raw, &format!(
        "SELECT count(*) FROM bss.ledger_journal_line WHERE tenant_id='{tenant}' AND account_id='{fx_gl}'"
    )).await
}

/// Boot + provision a USD-functional / EUR-billing seller with the settle/return
/// chart (UNALLOCATED, CASH_CLEARING, PSP_FEE_EXPENSE in EUR; FX_GAIN_LOSS in USD),
/// the functional-currency source, an OPEN current-month period, and an EUR→USD @
/// 1.08 rate; then settle `gross` EUR (with `fee`) cross-currency so UNALLOCATED,
/// CASH_CLEARING and PSP_FEE_EXPENSE each carry a functional balance.
async fn setup_and_settle(
    gross: i64,
    fee: i64,
    payment_id: &str,
) -> (
    testcontainers_modules::testcontainers::ContainerAsync<Postgres>,
    DatabaseConnection,
    DBProvider<DbError>,
    Chart,
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
        unallocated: Uuid::now_v7(),
        cash: Uuid::now_v7(),
        psp_fee: Uuid::now_v7(),
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
            c.unallocated,
            AccountClass::Unallocated,
            "CR",
            "EUR",
        ),
        account(c.tenant, c.cash, AccountClass::CashClearing, "DR", "EUR"),
        account(
            c.tenant,
            c.psp_fee,
            AccountClass::PspFeeExpense,
            "DR",
            "EUR",
        ),
        account(c.tenant, c.fx_gl, AccountClass::FxGainLoss, "DR", "USD"),
    ] {
        reference.insert_account(row).await.unwrap();
    }
    FxRepo::new(provider.clone())
        .upsert_rate(&NewFxRate {
            tenant_id: c.tenant,
            base_currency: "EUR".to_owned(),
            quote_currency: "USD".to_owned(),
            provider: "ecb".to_owned(),
            rate_micro: 1_080_000,
            as_of: now,
            fallback_order: 0,
        })
        .await
        .unwrap();

    SettlementService::new(
        provider.clone(),
        Arc::new(LedgerEventPublisher::noop()),
        Arc::new(NoopLedgerMetrics),
    )
    .with_fx(rate_locker(&provider))
    .settle(
        &SecurityContext::anonymous(),
        &AccessScope::for_tenant(c.tenant),
        SettlementInput {
            tenant_id: c.tenant,
            payer_tenant_id: c.payer,
            payment_id: payment_id.to_owned(),
            gross_minor: gross,
            fee_minor: fee,
            currency: "EUR".to_owned(),
            effective_at: None,
        },
    )
    .await
    .expect("cross-currency settle must post");

    (container, raw, provider, c)
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn full_settlement_return_carries_functional_forward_no_fx() {
    // Settle 120.00 EUR gross / 20.00 EUR fee @ 1.08 → net cash 100.00 EUR.
    // UNALLOCATED carries (12000, 12960), CASH (10000, 10800), PSP_FEE (2000, 2160).
    let (_c, raw, provider, chart) = setup_and_settle(12_000, 2_000, "PAY-SR-1").await;
    let ctx = SecurityContext::anonymous();
    let scope = AccessScope::for_tenant(chart.tenant);

    assert_eq!(
        unalloc(&raw, chart.tenant, chart.payer).await,
        (Some(12_000), Some(12_960)),
        "settle stamped UNALLOCATED gross functional"
    );
    assert_eq!(
        acct(&raw, chart.tenant, chart.cash).await,
        (Some(10_000), Some(10_800)),
        "settle stamped CASH_CLEARING net functional"
    );
    assert_eq!(
        acct(&raw, chart.tenant, chart.psp_fee).await,
        (Some(2_000), Some(2_160)),
        "settle stamped PSP_FEE_EXPENSE fee functional"
    );

    // Full return: DR UNALLOCATED 12000 / CR CASH_CLEARING 10000 / CR PSP_FEE 2000.
    // The pool's carried 129.60 USD basis carries forward onto every leg (cash leg =
    // the exact residual), so the functional column nets to zero and each grain
    // drains to (0, 0) — no FX line.
    let reference = SettlementReturnService::new(
        provider.clone(),
        Arc::new(LedgerEventPublisher::noop()),
        Arc::new(NoopLedgerMetrics),
    )
    .return_settlement(
        &ctx,
        &scope,
        SettlementReturnInput {
            tenant_id: chart.tenant,
            payer_tenant_id: chart.payer,
            payment_id: "PAY-SR-1".to_owned(),
            psp_return_id: "PSP-SR-1".to_owned(),
            amount_minor: 12_000,
            currency: "EUR".to_owned(),
            effective_at: None,
        },
    )
    .await
    .expect("cross-currency settlement-return must post");

    assert_eq!(
        entry_functional_net(&raw, chart.tenant, reference.entry_id).await,
        Some(0),
        "return entry functional column balances (carry-forward, no FX)"
    );
    assert_eq!(
        unalloc(&raw, chart.tenant, chart.payer).await,
        (Some(0), Some(0)),
        "UNALLOCATED drained to (0, 0) — pool basis relieved"
    );
    assert_eq!(
        acct(&raw, chart.tenant, chart.cash).await,
        (Some(0), Some(0)),
        "CASH_CLEARING drained to (0, 0) — cash leg took the residual functional"
    );
    assert_eq!(
        acct(&raw, chart.tenant, chart.psp_fee).await,
        (Some(0), Some(0)),
        "PSP_FEE_EXPENSE drained to (0, 0) — fee leg relieved pro-rata"
    );
    assert_eq!(
        fx_line_count(&raw, chart.tenant, chart.fx_gl).await,
        Some(0),
        "a settlement-return posts NO FX_GAIN_LOSS line (carry-forward, not realized FX)"
    );
}
