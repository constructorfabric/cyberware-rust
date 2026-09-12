//! Postgres-only integration (Slice 5, Phase 2 F1): **realized FX on a
//! cross-currency allocation close**, driven through the REAL payment
//! orchestrators (`InvoicePostService` + `SettlementService` + `AllocationService`)
//! against the foundation engine — the design's **worked example C** oracle.
//!
//! A USD-**functional** seller (functional currency on its `fiscal_calendar`,
//! S5-F3) bills in **EUR**:
//! 1. post an EUR invoice @ 1.10 → AR carried 120.00 EUR / **132.00 USD**;
//! 2. the rate moves to 1.08;
//! 3. settle 120.00 EUR @ 1.08 → the unallocated pool carries 120.00 EUR /
//!    **129.60 USD**;
//! 4. allocate the pool onto the invoice (a full close) — the DR UNALLOCATED leg
//!    relieves 129.60 USD, the CR AR leg 132.00 USD, so the functional column is
//!    short 2.40 on the debit side → a **DR FX_GAIN_LOSS 2.40 USD** realized
//!    **loss**, and BOTH grains close to functional zero.
//!
//! Asserts the net realized loss (2.40), the functional-only FX line shape
//! (`amount_minor = 0`, `functional_amount_minor = 240`, side DR), that the
//! allocate entry's functional column balances under the dual-column commit
//! trigger, and that both the AR invoice and the pool close to `(0, 0)` in BOTH
//! columns. No new rate is locked on the allocate entry (relief is at the carried
//! rate) → its lines carry no `rate_snapshot_ref`.
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
use bss_ledger::domain::payment::settlement::SettlementInput;
use bss_ledger::domain::ports::metrics::NoopLedgerMetrics;
use bss_ledger::infra::events::publisher::LedgerEventPublisher;
use bss_ledger::infra::fx::rate_locker::RateLocker;
use bss_ledger::infra::fx::rate_source::RateSource;
use bss_ledger::infra::invoice_post::InvoicePostService;
use bss_ledger::infra::payment::allocate::{AllocateRequest, AllocationOutcome, AllocationService};
use bss_ledger::infra::payment::settle::SettlementService;
use bss_ledger::infra::storage::migrations::Migrator;
use bss_ledger::infra::storage::repo::{FxRepo, NewFxRate, ReferenceRepo};
use bss_ledger_sdk::AccountClass;
use chrono::NaiveDate;
use sea_orm::{ConnectionTrait, Database, DatabaseConnection, Statement};
use sea_orm_migration::MigratorTrait;
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

/// A chart account in `currency` (EUR for the transaction grains, USD for the
/// functional FX_GAIN_LOSS account). `stream` is `Some` only for the per-stream
/// REVENUE class.
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

/// Build a `RateLocker` over the local FX store for the S1/S2 lock points.
fn rate_locker(provider: &DBProvider<DbError>, fx_config: &FxConfig) -> RateLocker {
    RateLocker::new(
        RateSource::new(FxRepo::new(provider.clone()), fx_config.clone()),
        FxRepo::new(provider.clone()),
    )
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn cross_currency_allocate_realizes_fx_and_closes_both_grains() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");

    let raw = Database::connect(&url).await.unwrap();
    Migrator::up(&raw, None).await.unwrap();
    let repo_url = format!("{url}?options=-c%20search_path%3Dbss,public");
    let provider =
        DBProvider::<DbError>::new(connect_db(&repo_url, ConnectOpts::default()).await.unwrap());

    let tenant = Uuid::now_v7();
    let payer = Uuid::now_v7();
    let ar = Uuid::now_v7();
    let revenue = Uuid::now_v7();
    let cash = Uuid::now_v7();
    let unallocated = Uuid::now_v7();
    let fx_gl = Uuid::now_v7();
    let now = OffsetDateTime::now_utc();
    let period_id = bss_ledger::domain::instant::yyyymm(now);

    let reference = ReferenceRepo::new(provider.clone());
    // Both the transaction (EUR) and functional (USD) scales must be registered.
    for ccy in ["EUR", "USD"] {
        reference
            .upsert_currency_scale(CurrencyScaleRow {
                tenant_id: tenant,
                currency: ccy.to_owned(),
                minor_units: 2,
                plausible_max_major: DEFAULT_PLAUSIBLE_MAX_MAJOR,
                source: "iso".to_owned(),
            })
            .await
            .unwrap();
    }
    // S5-F3: the functional-currency source (USD) — what ACTIVATES the FX locks.
    raw.execute_raw(pg(format!(
        "INSERT INTO bss.ledger_fiscal_calendar
           (tenant_id, legal_entity_id, fiscal_tz, granularity, fy_start_month, functional_currency)
         VALUES ('{tenant}','{tenant}','UTC','MONTH',1,'USD')"
    )))
    .await
    .unwrap();
    raw.execute_raw(pg(format!(
        "INSERT INTO bss.ledger_fiscal_period (tenant_id, legal_entity_id, period_id, fiscal_tz, status)
         VALUES ('{tenant}','{tenant}','{period_id}','UTC','OPEN')"
    )))
    .await
    .unwrap();
    // The EUR transaction chart (AR / REVENUE / CASH_CLEARING / UNALLOCATED) + the
    // USD functional FX_GAIN_LOSS account the realized-FX poster binds (F1: it MUST
    // be provisioned in the functional currency).
    for row in [
        account(tenant, ar, AccountClass::Ar, "DR", "EUR", None),
        account(
            tenant,
            revenue,
            AccountClass::Revenue,
            "CR",
            "EUR",
            Some("subscription"),
        ),
        account(tenant, cash, AccountClass::CashClearing, "DR", "EUR", None),
        account(
            tenant,
            unallocated,
            AccountClass::Unallocated,
            "CR",
            "EUR",
            None,
        ),
        account(tenant, fx_gl, AccountClass::FxGainLoss, "DR", "USD", None),
    ] {
        reference.insert_account(row).await.unwrap();
    }

    let ctx = SecurityContext::anonymous();
    let scope = AccessScope::for_tenant(tenant);

    // ── 1. EUR invoice @ 1.10 → AR carried 120.00 EUR / 132.00 USD ──────────────
    FxRepo::new(provider.clone())
        .upsert_rate(&NewFxRate {
            tenant_id: tenant,
            base_currency: "EUR".to_owned(),
            quote_currency: "USD".to_owned(),
            provider: "ecb".to_owned(),
            rate_micro: 1_100_000,
            as_of: now,
            fallback_order: 0,
        })
        .await
        .unwrap();

    let fx_config = FxConfig {
        provider_order: vec!["ecb".to_owned()],
        ..FxConfig::default()
    };
    let invoice_svc = InvoicePostService::new(
        provider.clone(),
        Arc::new(LedgerEventPublisher::noop()),
        Arc::new(NoopLedgerMetrics),
        RecognitionConfig::default(),
        fx_config.clone(),
    );
    // 120.00 EUR ex-tax, no tax ⇒ DR AR 12000 / CR REVENUE 12000 (functional @1.10
    // = 13200 each).
    let inv = PostedInvoice {
        invoice_id: "INV-FXC-1".to_owned(),
        payer_tenant_id: payer,
        resource_tenant_id: None,
        seller_tenant_id: tenant,
        effective_at: to_naive_date(now),
        due_date: Some(naive(2026, 12, 1)),
        period_id: period_id.clone(),
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
        posted_by_actor_id: tenant,
        correlation_id: tenant,
    };
    invoice_svc
        .post_invoice(&ctx, &scope, &inv, true)
        .await
        .expect("cross-currency invoice must post");

    // AR grain carried both columns: 120.00 EUR / 132.00 USD.
    assert_eq!(
        scalar_i64(&raw, &format!(
            "SELECT balance_minor FROM bss.ledger_ar_invoice_balance WHERE tenant_id='{tenant}' AND invoice_id='INV-FXC-1'"
        )).await,
        Some(12_000),
        "AR carried transaction = 120.00 EUR"
    );
    assert_eq!(
        scalar_i64(&raw, &format!(
            "SELECT functional_balance_minor FROM bss.ledger_ar_invoice_balance WHERE tenant_id='{tenant}' AND invoice_id='INV-FXC-1'"
        )).await,
        Some(13_200),
        "AR carried functional = 132.00 USD (120.00 EUR * 1.10)"
    );

    // ── 2. The rate moves to 1.08 (overwrites the local store) ──────────────────
    FxRepo::new(provider.clone())
        .upsert_rate(&NewFxRate {
            tenant_id: tenant,
            base_currency: "EUR".to_owned(),
            quote_currency: "USD".to_owned(),
            provider: "ecb".to_owned(),
            rate_micro: 1_080_000,
            as_of: now,
            fallback_order: 0,
        })
        .await
        .unwrap();

    // ── 3. Settle 120.00 EUR @ 1.08 → pool carries 120.00 EUR / 129.60 USD ──────
    let settle_svc = SettlementService::new(
        provider.clone(),
        Arc::new(LedgerEventPublisher::noop()),
        Arc::new(NoopLedgerMetrics),
    )
    .with_fx(rate_locker(&provider, &fx_config));
    settle_svc
        .settle(
            &ctx,
            &scope,
            SettlementInput {
                tenant_id: tenant,
                payer_tenant_id: payer,
                payment_id: "PAY-FXC-1".to_owned(),
                gross_minor: 12_000,
                fee_minor: 0,
                currency: "EUR".to_owned(),
                effective_at: None,
            },
        )
        .await
        .expect("cross-currency settle must post");

    assert_eq!(
        scalar_i64(&raw, &format!(
            "SELECT balance_minor FROM bss.ledger_unallocated_balance WHERE tenant_id='{tenant}' AND payer_tenant_id='{payer}'"
        )).await,
        Some(12_000),
        "pool carried transaction = 120.00 EUR"
    );
    assert_eq!(
        scalar_i64(&raw, &format!(
            "SELECT functional_balance_minor FROM bss.ledger_unallocated_balance WHERE tenant_id='{tenant}' AND payer_tenant_id='{payer}'"
        )).await,
        Some(12_960),
        "pool carried functional = 129.60 USD (120.00 EUR * 1.08)"
    );

    // ── 4. Allocate the pool onto the invoice — the cross-currency close ─────────
    // AllocationService takes NO `.with_fx()`: realized FX is carried-driven (it
    // reads the grains' carried functional), not rate-locked.
    let allocate_svc = AllocationService::new(
        provider.clone(),
        Arc::new(LedgerEventPublisher::noop()),
        Arc::new(NoopLedgerMetrics),
    );
    let outcome = allocate_svc
        .allocate(
            &ctx,
            &scope,
            AllocateRequest {
                tenant_id: tenant,
                payer_tenant_id: payer,
                payment_id: "PAY-FXC-1".to_owned(),
                allocation_id: Uuid::now_v7(),
                lump_minor: 12_000,
                currency: "EUR".to_owned(),
                hint_invoice_id: None,
                caller_splits: None,
            },
        )
        .await
        .expect("cross-currency allocate must post (functional column balances)");
    let alloc_entry = match outcome {
        AllocationOutcome::Applied(a) => {
            assert!(!a.posting.replayed, "first allocate is fresh");
            a.posting.entry_id
        }
        AllocationOutcome::Queued(q) => panic!("expected an inline allocate, got Queued: {q:?}"),
    };
    let alloc_lines = format!(
        "FROM bss.ledger_journal_line WHERE tenant_id='{tenant}' AND entry_id='{alloc_entry}'"
    );

    // The allocate entry has THREE lines: DR UNALLOCATED + CR AR + the FX line.
    assert_eq!(
        scalar_i64(&raw, &format!("SELECT count(*) {alloc_lines}")).await,
        Some(3),
        "allocate entry = DR UNALLOCATED + CR AR + the appended FX_GAIN_LOSS line"
    );

    // The net realized FX line: a functional-only DR FX_GAIN_LOSS of 2.40 USD loss
    // (amount_minor = 0, functional_amount_minor = 240, currency = USD).
    assert_eq!(
        scalar_i64(
            &raw,
            &format!("SELECT functional_amount_minor {alloc_lines} AND account_id='{fx_gl}'")
        )
        .await,
        Some(240),
        "DR FX_GAIN_LOSS = 2.40 USD realized loss (132.00 - 129.60)"
    );
    assert_eq!(
        scalar_i64(
            &raw,
            &format!("SELECT amount_minor {alloc_lines} AND account_id='{fx_gl}'")
        )
        .await,
        Some(0),
        "the FX line is functional-only (transaction amount_minor = 0)"
    );
    assert_eq!(
        scalar_i64(&raw, &format!(
            "SELECT count(*) {alloc_lines} AND account_id='{fx_gl}' AND side='DR' AND functional_currency='USD'"
        )).await,
        Some(1),
        "a realized LOSS is a DEBIT to FX_GAIN_LOSS in the functional currency"
    );

    // The allocate entry's functional column balances (DR == CR) — the dual-column
    // commit trigger accepted the post ONLY because of this.
    assert_eq!(
        scalar_i64(&raw, &format!(
            "SELECT COALESCE(SUM(CASE WHEN side='DR' THEN functional_amount_minor ELSE -functional_amount_minor END),0)::bigint {alloc_lines}"
        )).await,
        Some(0),
        "allocate entry functional column balances (DR == CR)"
    );
    // No new base rate is locked on an allocation close (relief at the carried
    // rate, §4.3) — the allocate lines carry no rate_snapshot_ref.
    assert_eq!(
        scalar_i64(
            &raw,
            &format!("SELECT count(*) {alloc_lines} AND rate_snapshot_ref IS NOT NULL")
        )
        .await,
        Some(0),
        "an allocation close locks no new rate (carried-rate relief)"
    );

    // ── 5. Both grains close to ZERO in BOTH columns ────────────────────────────
    assert_eq!(
        scalar_i64(&raw, &format!(
            "SELECT balance_minor FROM bss.ledger_ar_invoice_balance WHERE tenant_id='{tenant}' AND invoice_id='INV-FXC-1'"
        )).await,
        Some(0),
        "AR transaction balance closed to 0"
    );
    assert_eq!(
        scalar_i64(&raw, &format!(
            "SELECT functional_balance_minor FROM bss.ledger_ar_invoice_balance WHERE tenant_id='{tenant}' AND invoice_id='INV-FXC-1'"
        )).await,
        Some(0),
        "AR functional balance closed to 0"
    );
    assert_eq!(
        scalar_i64(&raw, &format!(
            "SELECT balance_minor FROM bss.ledger_unallocated_balance WHERE tenant_id='{tenant}' AND payer_tenant_id='{payer}'"
        )).await,
        Some(0),
        "pool transaction balance closed to 0"
    );
    assert_eq!(
        scalar_i64(&raw, &format!(
            "SELECT functional_balance_minor FROM bss.ledger_unallocated_balance WHERE tenant_id='{tenant}' AND payer_tenant_id='{payer}'"
        )).await,
        Some(0),
        "pool functional balance closed to 0"
    );

    // The FX_GAIN_LOSS account carries the realized loss in the functional column
    // (a P&L grain: transaction balance 0, functional = the 2.40 USD loss).
    assert_eq!(
        scalar_i64(&raw, &format!(
            "SELECT functional_balance_minor FROM bss.ledger_account_balance WHERE tenant_id='{tenant}' AND account_id='{fx_gl}'"
        )).await,
        Some(240),
        "FX_GAIN_LOSS functional balance = the 2.40 USD realized loss"
    );
}
