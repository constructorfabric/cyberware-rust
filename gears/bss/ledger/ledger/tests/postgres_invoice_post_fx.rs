//! Postgres-only integration (Slice 5, D1): a **cross-currency** invoice driven
//! through the REAL foundation engine (`InvoicePostService`) with the S1 FX lock
//! live.
//!
//! A USD-**functional** seller (the functional currency seeded on its
//! `fiscal_calendar`, S5-F3) invoices in **EUR**. With an EUR→USD rate in the
//! local store, the S1 hook resolves + snapshots the rate, stamps the functional
//! translation on every line, and the dual-column commit trigger accepts the post
//! ONLY because the functional column balances. Asserts: every line carries
//! `functional_amount_minor` + `functional_currency = USD` + a non-null
//! `rate_snapshot_ref` (one snapshot per entry), the functional column nets to
//! zero, and the immutable `fx_rate_snapshot` row was frozen.
//!
//! Ignored by default; run with `-- --ignored`.

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

use bss_ledger::config::{FxConfig, RecognitionConfig};
use bss_ledger::domain::invoice::builder::{InvoiceItem, PostedInvoice, TaxBreakdown};
use bss_ledger::domain::model::{AccountRow, CurrencyScaleRow};
use bss_ledger::domain::money::DEFAULT_PLAUSIBLE_MAX_MAJOR;
use bss_ledger::infra::events::publisher::LedgerEventPublisher;
use bss_ledger::infra::invoice_post::InvoicePostService;
use bss_ledger::infra::metrics::test_harness::MetricsHarness;
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

/// An EUR chart account (the invoice's transaction currency).
fn eur_account(
    tenant: Uuid,
    id: Uuid,
    class: AccountClass,
    normal: &str,
    stream: Option<&str>,
) -> AccountRow {
    AccountRow {
        account_id: id,
        tenant_id: tenant,
        legal_entity_id: tenant,
        account_class: class.as_str().to_owned(),
        currency: "EUR".to_owned(),
        revenue_stream: stream.map(str::to_owned),
        normal_side: normal.to_owned(),
        may_go_negative: false,
        lifecycle_state: "OPEN".to_owned(),
    }
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn cross_currency_invoice_stamps_functional_and_balances_both_columns() {
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
    let tax = Uuid::now_v7();
    let period_id = "202606";

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
    // The S5-F3 functional-currency source: a fiscal_calendar row carrying USD.
    // This is what ACTIVATES the S1 FX lock for this tenant (absent → single-ccy).
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
    for row in [
        eur_account(tenant, ar, AccountClass::Ar, "DR", None),
        eur_account(
            tenant,
            revenue,
            AccountClass::Revenue,
            "CR",
            Some("subscription"),
        ),
        eur_account(tenant, tax, AccountClass::TaxPayable, "CR", None),
    ] {
        reference.insert_account(row).await.unwrap();
    }

    // Seed the EUR→USD rate (1.10) in the local store — what the RateSyncJob /
    // ingest endpoint would refresh; the lock-time RateSource reads it locally.
    FxRepo::new(provider.clone())
        .upsert_rate(&NewFxRate {
            tenant_id: tenant,
            base_currency: "EUR".to_owned(),
            quote_currency: "USD".to_owned(),
            provider: "ecb".to_owned(),
            rate_micro: 1_100_000,
            as_of: OffsetDateTime::now_utc(),
            fallback_order: 0,
        })
        .await
        .unwrap();

    // FxConfig with "ecb" in the provider order so RateSource resolves it.
    let fx_config = FxConfig {
        provider_order: vec!["ecb".to_owned()],
        ..FxConfig::default()
    };
    let harness = MetricsHarness::new();
    let service = InvoicePostService::new(
        provider.clone(),
        Arc::new(LedgerEventPublisher::noop()),
        Arc::new(harness.metrics()),
        RecognitionConfig::default(),
        fx_config,
    );
    let ctx = SecurityContext::anonymous();
    let scope = AccessScope::for_tenant(tenant);

    // An EUR invoice: 10.00 ex-tax (1000 minor) + 2.00 tax (200) = 12.00 gross AR.
    let inv = PostedInvoice {
        invoice_id: "INV-FX-1".to_owned(),
        payer_tenant_id: payer,
        resource_tenant_id: None,
        seller_tenant_id: tenant,
        effective_at: naive(2026, 6, 1),
        due_date: Some(naive(2026, 7, 1)),
        period_id: period_id.to_owned(),
        items: vec![InvoiceItem {
            amount_minor_ex_tax: 1000,
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
        tax: vec![TaxBreakdown {
            amount_minor: 200,
            currency: "EUR".to_owned(),
            tax_jurisdiction: "US-CA".to_owned(),
            tax_filing_period: "2026Q2".to_owned(),
            tax_rate_ref: None,
        }],
        posted_by_actor_id: tenant,
        correlation_id: tenant,
    };

    // The post SUCCEEDS only because the functional column balances under the
    // dual-column commit trigger — that is itself a core assertion.
    let posted = service
        .post_invoice(&ctx, &scope, &inv, true)
        .await
        .expect("cross-currency invoice must post (both columns balanced)");
    assert!(!posted.replayed, "first post is fresh");

    let where_t = format!("WHERE tenant_id='{tenant}'");

    // Transaction column unchanged: AR 1200 DR = Revenue 1000 + Tax 200 CR.
    assert_eq!(
        scalar_i64(&raw, &format!(
            "SELECT functional_amount_minor FROM bss.ledger_journal_line {where_t} AND account_id='{ar}'"
        )).await,
        Some(1_320),
        "AR functional = 1200 EUR * 1.10 = 13.20 USD (1320 minor)"
    );
    assert_eq!(
        scalar_i64(&raw, &format!(
            "SELECT functional_amount_minor FROM bss.ledger_journal_line {where_t} AND account_id='{revenue}'"
        )).await,
        Some(1_100),
        "Revenue functional = 1000 EUR * 1.10 = 11.00 USD"
    );
    assert_eq!(
        scalar_i64(&raw, &format!(
            "SELECT functional_amount_minor FROM bss.ledger_journal_line {where_t} AND account_id='{tax}'"
        )).await,
        Some(220),
        "Tax functional = 200 EUR * 1.10 = 2.20 USD"
    );

    // Every line stamped functional USD; none left NULL.
    assert_eq!(
        scalar_i64(&raw, &format!(
            "SELECT count(*) FROM bss.ledger_journal_line {where_t} AND functional_currency='USD'"
        )).await,
        Some(3),
        "all three lines carry functional_currency = USD"
    );
    assert_eq!(
        scalar_i64(&raw, &format!(
            "SELECT count(*) FROM bss.ledger_journal_line {where_t} AND functional_amount_minor IS NULL"
        )).await,
        Some(0),
        "no line is left functional-NULL on a cross-currency entry (all-or-nothing)"
    );

    // The functional column nets to zero (DR == CR).
    assert_eq!(
        scalar_i64(&raw, &format!(
            "SELECT COALESCE(SUM(CASE WHEN side='DR' THEN functional_amount_minor ELSE -functional_amount_minor END), 0)::bigint \
             FROM bss.ledger_journal_line {where_t}"
        )).await,
        Some(0),
        "functional column balances (DR == CR)"
    );

    // One rate snapshot per entry, stamped on every line (rate_snapshot_ref).
    assert_eq!(
        scalar_i64(&raw, &format!(
            "SELECT count(*) FROM bss.ledger_journal_line {where_t} AND rate_snapshot_ref IS NOT NULL"
        )).await,
        Some(3),
        "every line carries the entry's rate_snapshot_ref"
    );
    assert_eq!(
        scalar_i64(
            &raw,
            &format!(
                "SELECT count(DISTINCT rate_snapshot_ref) FROM bss.ledger_journal_line {where_t}"
            )
        )
        .await,
        Some(1),
        "one rate per entry (§4.3) — all lines share the snapshot"
    );

    // The immutable snapshot was frozen with the locked rate + the EUR→USD pair.
    assert_eq!(
        scalar_i64(
            &raw,
            &format!(
                "SELECT count(*) FROM bss.ledger_fx_rate_snapshot {where_t} \
             AND base_currency='EUR' AND quote_currency='USD' AND rate_micro=1100000"
            )
        )
        .await,
        Some(1),
        "one immutable EUR→USD snapshot frozen at the locked rate"
    );

    // The same EUR invoice re-posts as an idempotent replay (no second snapshot
    // is required for correctness; the post dedups on its business key).
    let replay = service
        .post_invoice(&ctx, &scope, &inv, true)
        .await
        .expect("replay must succeed");
    assert!(replay.replayed, "re-post is an idempotent replay");
}
