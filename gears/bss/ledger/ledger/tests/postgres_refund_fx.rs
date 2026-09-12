//! Postgres-only integration (Slice 5, Phase 2 F2): **functional carry-forward on
//! a cross-currency refund close**, driven through the REAL orchestrators
//! (`SettlementService` / `RefundHandler`) against the foundation engine.
//!
//! A refund unwinds a position through the two-stage `REFUND_CLEARING` entirely in
//! the transaction currency (no in-ledger conversion), so the functional cost
//! basis carries forward and each stage's functional column nets to ZERO — there
//! is **no realized FX_GAIN_LOSS line** (true EUR→USD realization is Slice-7
//! reconciliation, owner decision 2026-06-28). These tests prove the carry-forward
//! keeps each relieved grain's functional column in lockstep with `balance_minor`:
//!
//! - `pattern_a_two_stage_refund_carries_functional_forward_no_fx`: settle 120 EUR
//!   @ 1.08 (UNALLOCATED + CASH_CLEARING each carry 129.60 USD); a Pattern-A refund
//!   drains UNALLOCATED → REFUND_CLEARING at `initiated` (UNALLOCATED → (0,0),
//!   REFUND_CLEARING → 129.60) then REFUND_CLEARING → CASH_CLEARING at `confirmed`
//!   (both → (0,0)). The 129.60 USD basis flows out; no FX line.
//! - `pattern_a_single_step_refund_carries_functional_forward_no_fx`: a single-step
//!   refund posts `DR UNALLOCATED · CR CASH_CLEARING` in one move; both close to
//!   (0,0); no FX line.
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
use bss_ledger::domain::adjustment::refund::{RefundDirection, RefundRequest};
use bss_ledger::domain::adjustment::refund::{RefundPattern, RefundPhase};
use bss_ledger::domain::error::DomainError;
use bss_ledger::domain::model::{AccountRow, CurrencyScaleRow};
use bss_ledger::domain::money::DEFAULT_PLAUSIBLE_MAX_MAJOR;
use bss_ledger::domain::payment::settlement::SettlementInput;
use bss_ledger::domain::ports::metrics::NoopLedgerMetrics;
use bss_ledger::infra::adjustment::refund_service::RefundHandler;
use bss_ledger::infra::events::publisher::LedgerEventPublisher;
use bss_ledger::infra::fx::rate_locker::RateLocker;
use bss_ledger::infra::fx::rate_source::RateSource;
use bss_ledger::infra::payment::settle::SettlementService;
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

fn rate_locker(provider: &DBProvider<DbError>, fx_config: &FxConfig) -> RateLocker {
    RateLocker::new(
        RateSource::new(FxRepo::new(provider.clone()), fx_config.clone()),
        FxRepo::new(provider.clone()),
    )
}

fn fx_config() -> FxConfig {
    FxConfig {
        provider_order: vec!["ecb".to_owned()],
        ..FxConfig::default()
    }
}

struct Chart {
    tenant: Uuid,
    payer: Uuid,
    unallocated: Uuid,
    cash: Uuid,
    refund_clearing: Uuid,
    fx_gl: Uuid,
}

/// Boot + provision a USD-functional / EUR-billing seller with the refund chart
/// (UNALLOCATED, CASH_CLEARING, REFUND_CLEARING in EUR; FX_GAIN_LOSS in USD), the
/// functional-currency source, an OPEN current-month period, and an EUR→USD @ 1.08
/// rate; then settle `gross` EUR cross-currency so UNALLOCATED + CASH_CLEARING each
/// carry a functional balance.
async fn setup_and_settle(
    gross: i64,
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
        refund_clearing: Uuid::now_v7(),
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
            c.refund_clearing,
            AccountClass::RefundClearing,
            "CR",
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

    // Settle the receipt cross-currency @ 1.08 → UNALLOCATED + CASH_CLEARING carry.
    SettlementService::new(
        provider.clone(),
        Arc::new(LedgerEventPublisher::noop()),
        Arc::new(NoopLedgerMetrics),
    )
    .with_fx(rate_locker(&provider, &fx_config()))
    .settle(
        &SecurityContext::anonymous(),
        &AccessScope::for_tenant(c.tenant),
        SettlementInput {
            tenant_id: c.tenant,
            payer_tenant_id: c.payer,
            payment_id: payment_id.to_owned(),
            gross_minor: gross,
            fee_minor: 0,
            currency: "EUR".to_owned(),
            effective_at: None,
        },
    )
    .await
    .expect("cross-currency settle must post");

    (container, raw, provider, c)
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

fn refund_req(
    c: &Chart,
    payment_id: &str,
    psp_refund_id: &str,
    phase: RefundPhase,
    amount: i64,
    two_stage: bool,
) -> RefundRequest {
    RefundRequest {
        tenant_id: c.tenant,
        payer_tenant_id: c.payer,
        // The `refund` row PK is (tenant, refund_id) — one row per phase; the
        // idempotency grain is (psp_refund_id, phase). So each phase carries its own
        // refund_id while sharing the psp_refund_id.
        refund_id: format!("R-{psp_refund_id}-{}", phase.as_str()),
        psp_refund_id: psp_refund_id.to_owned(),
        phase,
        pattern: RefundPattern::AUnallocated,
        payment_id: payment_id.to_owned(),
        invoice_id: None,
        currency: "EUR".to_owned(),
        amount_minor: amount,
        two_stage,
        relates_to_refund_id: None,
        direction: RefundDirection::Outbound,
    }
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn pattern_a_two_stage_refund_carries_functional_forward_no_fx() {
    let (_c, raw, provider, chart) = setup_and_settle(12_000, "PAY-RF-1").await;
    let ctx = SecurityContext::anonymous();
    let scope = AccessScope::for_tenant(chart.tenant);
    let handler = RefundHandler::new(provider.clone(), Arc::new(LedgerEventPublisher::noop()));

    // Settle landed UNALLOCATED + CASH_CLEARING at 120.00 EUR / 129.60 USD each.
    assert_eq!(
        unalloc(&raw, chart.tenant, chart.payer).await,
        (Some(12_000), Some(12_960))
    );
    assert_eq!(
        acct(&raw, chart.tenant, chart.cash).await,
        (Some(12_000), Some(12_960))
    );

    // initiated: DR UNALLOCATED 12000 / CR REFUND_CLEARING 12000 — the 129.60 USD
    // basis carries forward UNALLOCATED → REFUND_CLEARING.
    let initiated = handler
        .post_refund(
            &ctx,
            &scope,
            refund_req(
                &chart,
                "PAY-RF-1",
                "PSP-RF-1",
                RefundPhase::Initiated,
                12_000,
                true,
            ),
        )
        .await
        .expect("initiated refund must post");
    assert_eq!(
        entry_functional_net(&raw, chart.tenant, initiated.entry_id).await,
        Some(0),
        "initiated entry functional balances (carry-forward, no FX)"
    );
    assert_eq!(
        unalloc(&raw, chart.tenant, chart.payer).await,
        (Some(0), Some(0)),
        "UNALLOCATED drained to (0, 0) — basis left"
    );
    assert_eq!(
        acct(&raw, chart.tenant, chart.refund_clearing).await,
        (Some(12_000), Some(12_960)),
        "REFUND_CLEARING carries the 129.60 USD basis forward"
    );

    // confirmed: DR REFUND_CLEARING 12000 / CR CASH_CLEARING 12000 — basis leaves.
    let confirmed = handler
        .post_refund(
            &ctx,
            &scope,
            refund_req(
                &chart,
                "PAY-RF-1",
                "PSP-RF-1",
                RefundPhase::Confirmed,
                12_000,
                true,
            ),
        )
        .await
        .expect("confirmed refund must post");
    assert_eq!(
        entry_functional_net(&raw, chart.tenant, confirmed.entry_id).await,
        Some(0),
        "confirmed entry functional balances (carry-forward, no FX)"
    );
    assert_eq!(
        acct(&raw, chart.tenant, chart.refund_clearing).await,
        (Some(0), Some(0)),
        "REFUND_CLEARING drained to (0, 0)"
    );
    assert_eq!(
        acct(&raw, chart.tenant, chart.cash).await,
        (Some(0), Some(0)),
        "CASH_CLEARING drained to (0, 0) — the full settle→refund round-trip nets to zero in both columns"
    );

    // No realized FX line anywhere (the FX_GAIN_LOSS account exists but is untouched).
    assert_eq!(
        fx_line_count(&raw, chart.tenant, chart.fx_gl).await,
        Some(0),
        "a refund posts NO FX_GAIN_LOSS line (carry-forward, not realized FX)"
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn pattern_a_single_step_refund_carries_functional_forward_no_fx() {
    let (_c, raw, provider, chart) = setup_and_settle(12_000, "PAY-RF-2").await;
    let ctx = SecurityContext::anonymous();
    let scope = AccessScope::for_tenant(chart.tenant);
    let handler = RefundHandler::new(provider.clone(), Arc::new(LedgerEventPublisher::noop()));

    // Single-step: DR UNALLOCATED 12000 / CR CASH_CLEARING 12000 in one `initiated`
    // move (two_stage = false) — carry-forward at UNALLOCATED's carried WAC.
    let posted = handler
        .post_refund(
            &ctx,
            &scope,
            refund_req(
                &chart,
                "PAY-RF-2",
                "PSP-RF-2",
                RefundPhase::Initiated,
                12_000,
                false,
            ),
        )
        .await
        .expect("single-step refund must post");
    assert_eq!(
        entry_functional_net(&raw, chart.tenant, posted.entry_id).await,
        Some(0),
        "single-step entry functional balances (carry-forward, no FX)"
    );
    assert_eq!(
        unalloc(&raw, chart.tenant, chart.payer).await,
        (Some(0), Some(0)),
        "UNALLOCATED drained to (0, 0)"
    );
    assert_eq!(
        acct(&raw, chart.tenant, chart.cash).await,
        (Some(0), Some(0)),
        "CASH_CLEARING drained to (0, 0) in both columns"
    );
    assert_eq!(
        fx_line_count(&raw, chart.tenant, chart.fx_gl).await,
        Some(0),
        "single-step refund posts NO FX_GAIN_LOSS line"
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn cross_currency_clawback_is_rejected_not_silently_drifted() {
    // A refund-of-refund CLAW-BACK restores a drawn-down position at the PRIOR
    // refund's rate, which the WAC carry-forward cannot source (Slice 7). Until then
    // a CROSS-CURRENCY claw-back must be REJECTED up front (`FxOperationUnsupported`)
    // — never silently posted functional-NULL (the drift this remediation closes).
    // A cross-currency pool is in play here (settle landed UNALLOCATED with a
    // functional balance), so the stamp's cross-currency detect fires and rejects.
    let (_c, raw, provider, chart) = setup_and_settle(12_000, "PAY-RF-CB").await;
    let ctx = SecurityContext::anonymous();
    let scope = AccessScope::for_tenant(chart.tenant);
    let handler = RefundHandler::new(provider.clone(), Arc::new(LedgerEventPublisher::noop()));

    let clawback = RefundRequest {
        tenant_id: chart.tenant,
        payer_tenant_id: chart.payer,
        refund_id: "R-CB-1-initiated".to_owned(),
        psp_refund_id: "PSP-CB-1".to_owned(),
        phase: RefundPhase::Initiated,
        pattern: RefundPattern::AUnallocated,
        payment_id: "PAY-RF-CB".to_owned(),
        invoice_id: None,
        currency: "EUR".to_owned(),
        amount_minor: 12_000,
        two_stage: false,
        // A claw-back references a prior refund (refund-of-refund); the link + the
        // Clawback direction make `is_clawback()` true.
        relates_to_refund_id: Some("R-PRIOR".to_owned()),
        direction: RefundDirection::Clawback,
    };
    let err = handler
        .post_refund(&ctx, &scope, clawback)
        .await
        .expect_err("a cross-currency claw-back must be rejected, not posted");
    assert!(
        matches!(err, DomainError::FxOperationUnsupported(_)),
        "expected FxOperationUnsupported, got {err:?}"
    );

    // And nothing posted: UNALLOCATED still carries the full settled basis (the
    // reject fired BEFORE any ledger effect — no silent drift).
    assert_eq!(
        unalloc(&raw, chart.tenant, chart.payer).await,
        (Some(12_000), Some(12_960)),
        "UNALLOCATED untouched — the claw-back was rejected before posting"
    );
}
