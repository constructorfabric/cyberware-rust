//! Postgres-only integration (Slice 5, Phase 3 H/I/J): **unrealized Mode-B
//! revaluation + next-period reversal** of a cross-currency monetary position,
//! driven through the real `UnrealizedRevaluationRun` against the foundation
//! engine — the design §4.5 worked example.
//!
//! A USD-**functional** seller (functional currency on its `fiscal_calendar`,
//! S5-F3) holds an open **EUR** receivable:
//! 1. post an EUR invoice @ 1.10 → AR carried 120.00 EUR / **132.00 USD**;
//! 2. at period end the rate is 1.05 → the AR is worth only 120.00 × 1.05 =
//!    **126.00 USD**;
//! 3. **forward revaluation** (AR scope): the carrying value falls 6.00 USD →
//!    **CR AR 6.00 / DR FX_UNREALIZED 6.00** (functional-only, `amount_minor = 0`),
//!    moving the AR grain's functional balance to 126.00 and booking a 6.00 USD
//!    unrealized loss;
//! 4. the period CLOSES and the next one opens;
//! 5. **reversal** (first of the next OPEN period, `FX_REVAL_REVERSAL`): the
//!    negation **DR AR 6.00 / CR FX_UNREALIZED 6.00** restores the AR grain to its
//!    historical 132.00 USD carried basis and unwinds the FX_UNREALIZED contra to
//!    zero — only realized FX is permanent (decision 7).
//!
//! Asserts the forward entry shape + functional balance move, that the dual-column
//! commit trigger accepted both functional-only entries, idempotent re-run, and
//! that the reversal restores the carried basis in the next period.
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
use bss_ledger::domain::error::DomainError;
use bss_ledger::domain::instant::to_naive_date;
use bss_ledger::domain::invoice::builder::{InvoiceItem, PostedInvoice};
use bss_ledger::domain::model::{AccountRow, CurrencyScaleRow, NewEntry, NewLine};
use bss_ledger::domain::money::DEFAULT_PLAUSIBLE_MAX_MAJOR;
use bss_ledger::domain::period::{next_period_id, period_end_utc};
use bss_ledger::domain::ports::metrics::NoopLedgerMetrics;
use bss_ledger::infra::events::publisher::LedgerEventPublisher;
use bss_ledger::infra::fx::revaluation_run::UnrealizedRevaluationRun;
use bss_ledger::infra::invoice_post::InvoicePostService;
use bss_ledger::infra::posting::service::PostingService;
use bss_ledger::infra::storage::migrations::Migrator;
use bss_ledger::infra::storage::repo::{FxRepo, NewFxRate, ReferenceRepo};
use bss_ledger_sdk::{AccountClass, MappingStatus, Side, SourceDocType};
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

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn cross_currency_revaluation_then_next_period_reversal() {
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
    let fx_unrealized = Uuid::now_v7();
    let now = OffsetDateTime::now_utc();
    let period_id = bss_ledger::domain::instant::yyyymm(now);
    let next_period = next_period_id(&period_id).unwrap();
    // The period-end instant drives the rate `as_of` (so the resolve sees a fresh,
    // non-stale period-end rate) and the run's rate lookup.
    let period_end = period_end_utc(&period_id).unwrap();

    let reference = ReferenceRepo::new(provider.clone());
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
    // S5-F3: USD functional currency — activates the cross-currency FX path.
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
    // EUR transaction chart (AR / REVENUE) + the USD functional FX_UNREALIZED
    // account the revaluation binds.
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
        account(
            tenant,
            fx_unrealized,
            AccountClass::FxUnrealized,
            "DR",
            "USD",
            None,
        ),
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
        revaluation_enabled: true,
        ..FxConfig::default()
    };
    let invoice_svc = InvoicePostService::new(
        provider.clone(),
        Arc::new(LedgerEventPublisher::noop()),
        Arc::new(NoopLedgerMetrics),
        RecognitionConfig::default(),
        fx_config.clone(),
    );
    let inv = PostedInvoice {
        invoice_id: "INV-REVAL-1".to_owned(),
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

    assert_eq!(
        scalar_i64(&raw, &format!(
            "SELECT functional_balance_minor FROM bss.ledger_ar_invoice_balance WHERE tenant_id='{tenant}' AND invoice_id='INV-REVAL-1'"
        )).await,
        Some(13_200),
        "AR carried functional = 132.00 USD (120.00 EUR * 1.10)"
    );

    // ── 2. Period-end rate 1.05 (as_of = period end, so the resolve is fresh) ────
    FxRepo::new(provider.clone())
        .upsert_rate(&NewFxRate {
            tenant_id: tenant,
            base_currency: "EUR".to_owned(),
            quote_currency: "USD".to_owned(),
            provider: "ecb".to_owned(),
            rate_micro: 1_050_000,
            as_of: period_end,
            fallback_order: 0,
        })
        .await
        .unwrap();

    // ── 3. Forward revaluation (AR scope) ───────────────────────────────────────
    let runner = UnrealizedRevaluationRun::new(
        provider.clone(),
        Arc::new(LedgerEventPublisher::noop()),
        fx_config.clone(),
    );
    runner
        .run_period(&ctx, &scope, tenant, &period_id, true)
        .await
        .expect("forward revaluation must post (functional column balances)");

    // The AR grain's functional carrying value falls to 126.00 USD (120.00 * 1.05).
    assert_eq!(
        scalar_i64(&raw, &format!(
            "SELECT functional_balance_minor FROM bss.ledger_ar_invoice_balance WHERE tenant_id='{tenant}' AND invoice_id='INV-REVAL-1'"
        )).await,
        Some(12_600),
        "AR functional remeasured to 126.00 USD"
    );
    // The transaction balance is UNTOUCHED (revaluation is functional-only).
    assert_eq!(
        scalar_i64(&raw, &format!(
            "SELECT balance_minor FROM bss.ledger_ar_invoice_balance WHERE tenant_id='{tenant}' AND invoice_id='INV-REVAL-1'"
        )).await,
        Some(12_000),
        "AR transaction balance unchanged (120.00 EUR)"
    );
    // The FX_UNREALIZED account carries the 6.00 USD unrealized loss (DR-normal).
    assert_eq!(
        scalar_i64(&raw, &format!(
            "SELECT functional_balance_minor FROM bss.ledger_account_balance WHERE tenant_id='{tenant}' AND account_id='{fx_unrealized}'"
        )).await,
        Some(600),
        "FX_UNREALIZED functional balance = 6.00 USD unrealized loss"
    );
    // The forward entry exists, is a FX_REVALUATION doc, and is functional-only
    // (every line amount_minor = 0) and balances in the functional column.
    let reval_lines = format!(
        "FROM bss.ledger_journal_line l JOIN bss.ledger_journal_entry e \
         ON l.tenant_id=e.tenant_id AND l.entry_id=e.entry_id \
         WHERE l.tenant_id='{tenant}' AND e.source_doc_type='FX_REVALUATION'"
    );
    assert_eq!(
        scalar_i64(&raw, &format!("SELECT count(*) {reval_lines}")).await,
        Some(2),
        "forward reval = CR AR + DR FX_UNREALIZED (two functional-only lines)"
    );
    assert_eq!(
        scalar_i64(
            &raw,
            &format!("SELECT COALESCE(SUM(l.amount_minor),0)::bigint {reval_lines}")
        )
        .await,
        Some(0),
        "every revaluation line is functional-only (transaction amount_minor = 0)"
    );
    assert_eq!(
        scalar_i64(&raw, &format!(
            "SELECT COALESCE(SUM(CASE WHEN l.side='DR' THEN l.functional_amount_minor ELSE -l.functional_amount_minor END),0)::bigint {reval_lines}"
        )).await,
        Some(0),
        "revaluation entry functional column balances (DR == CR)"
    );

    // ── 3b. Idempotent re-run posts nothing new ─────────────────────────────────
    runner
        .run_period(&ctx, &scope, tenant, &period_id, true)
        .await
        .expect("re-run is an idempotent replay");
    assert_eq!(
        scalar_i64(&raw, &format!(
            "SELECT count(DISTINCT e.entry_id) FROM bss.ledger_journal_entry e WHERE e.tenant_id='{tenant}' AND e.source_doc_type='FX_REVALUATION'"
        )).await,
        Some(1),
        "the AR-scope revaluation posted exactly ONE entry (idempotent on period:scope)"
    );

    // ── 4. Close the period, open the next ──────────────────────────────────────
    raw.execute_raw(pg(format!(
        "UPDATE bss.ledger_fiscal_period SET status='CLOSED' WHERE tenant_id='{tenant}' AND period_id='{period_id}'"
    )))
    .await
    .unwrap();
    raw.execute_raw(pg(format!(
        "INSERT INTO bss.ledger_fiscal_period (tenant_id, legal_entity_id, period_id, fiscal_tz, status)
         VALUES ('{tenant}','{tenant}','{next_period}','UTC','OPEN')"
    )))
    .await
    .unwrap();

    // ── 5. Reversal in the next OPEN period (FX_REVAL_REVERSAL) ──────────────────
    runner
        .reverse_period(&ctx, &scope, tenant, &period_id, true)
        .await
        .expect("reversal must post into the next open period");

    // The reversal restores the AR grain to its historical 132.00 USD carried basis.
    assert_eq!(
        scalar_i64(&raw, &format!(
            "SELECT functional_balance_minor FROM bss.ledger_ar_invoice_balance WHERE tenant_id='{tenant}' AND invoice_id='INV-REVAL-1'"
        )).await,
        Some(13_200),
        "reversal restores AR functional to 132.00 USD (historical basis)"
    );
    // The FX_UNREALIZED contra unwinds to zero (only realized FX is permanent).
    assert_eq!(
        scalar_i64(&raw, &format!(
            "SELECT functional_balance_minor FROM bss.ledger_account_balance WHERE tenant_id='{tenant}' AND account_id='{fx_unrealized}'"
        )).await,
        Some(0),
        "FX_UNREALIZED unwinds to 0 after the reversal"
    );
    // The reversal is a fresh FX_REVAL_REVERSAL entry in the NEXT period.
    assert_eq!(
        scalar_i64(&raw, &format!(
            "SELECT count(*) FROM bss.ledger_journal_entry WHERE tenant_id='{tenant}' AND source_doc_type='FX_REVAL_REVERSAL' AND period_id='{next_period}'"
        )).await,
        Some(1),
        "exactly one FX_REVAL_REVERSAL entry posted in the next period"
    );

    // ── 5b. Idempotent reversal re-run posts nothing new ────────────────────────
    runner
        .reverse_period(&ctx, &scope, tenant, &period_id, true)
        .await
        .expect("reversal re-run is an idempotent replay");
    assert_eq!(
        scalar_i64(&raw, &format!(
            "SELECT count(*) FROM bss.ledger_journal_entry WHERE tenant_id='{tenant}' AND source_doc_type='FX_REVAL_REVERSAL'"
        )).await,
        Some(1),
        "reversal idempotent on (tenant, FX_REVAL_REVERSAL, period:scope)"
    );
}

/// `FxRateUnavailable` at run time: a USD-functional seller holds an OPEN
/// cross-currency EUR receivable (a revalue-able grain: `functional_currency` set,
/// `balance_minor > 0`), but the EUR→USD pair has NO `ledger_fx_rate` row at all.
/// The revaluation enumerates the grain (step 1), then the period-end rate resolve
/// (step 2) finds zero candidates and the run fails [`DomainError::FxRateUnavailable`]
/// BEFORE any post — no `FX_REVALUATION` entry, the AR grain untouched.
///
/// This is a real, reachable deployment state (the FX feed has not published a
/// period-end quote for the pair) the happy-path test does not cover. The EUR
/// position is seeded by posting `DR AR (EUR 120.00 / functional USD 132.00) /
/// CR REVENUE (same)` DIRECTLY through the engine with the functional amounts
/// supplied on the lines — so a cross-currency AR grain materializes WITHOUT any
/// rate row existing (the engine resolves only per-line scale, never an FX rate;
/// the higher-level invoice path is what would resolve one, and we bypass it).
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn revaluation_with_no_period_end_rate_is_fx_rate_unavailable() {
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
    let fx_unrealized = Uuid::now_v7();
    let now = OffsetDateTime::now_utc();
    let period_id = bss_ledger::domain::instant::yyyymm(now);

    let reference = ReferenceRepo::new(provider.clone());
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
    // USD-functional seller — activates the cross-currency FX revaluation path.
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
        account(tenant, ar, AccountClass::Ar, "DR", "EUR", None),
        account(
            tenant,
            revenue,
            AccountClass::Revenue,
            "CR",
            "EUR",
            Some("subscription"),
        ),
        account(
            tenant,
            fx_unrealized,
            AccountClass::FxUnrealized,
            "DR",
            "USD",
            None,
        ),
    ] {
        reference.insert_account(row).await.unwrap();
    }

    let ctx = SecurityContext::anonymous();
    let scope = AccessScope::for_tenant(tenant);

    // Post the OPEN EUR receivable DIRECTLY (no FX rate seeded): both legs EUR
    // (transaction column balances 12000=12000) carry an explicit USD functional
    // amount (13200=13200, so the dual-column commit trigger passes). The AR grain
    // is left with `functional_currency='USD'` + balance 12000 ⇒ revalue-able.
    let posting = PostingService::new(provider.clone(), Arc::new(LedgerEventPublisher::noop()));
    let entry = NewEntry {
        entry_id: Uuid::now_v7(),
        tenant_id: tenant,
        legal_entity_id: tenant,
        period_id: period_id.clone(),
        entry_currency: "EUR".to_owned(),
        source_doc_type: SourceDocType::InvoicePost,
        source_business_id: "INV-NORATE-1".to_owned(),
        reverses_entry_id: None,
        reverses_period_id: None,
        posted_at_utc: now,
        effective_at: to_naive_date(now),
        origin: "SYSTEM".to_owned(),
        posted_by_actor_id: tenant,
        correlation_id: Uuid::now_v7(),
        rounding_evidence: serde_json::Value::Null,
        rate_snapshot_ref: None,
    };
    let fx_line = |account_id: Uuid,
                   class: AccountClass,
                   side: Side,
                   invoice_id: Option<&str>,
                   stream: Option<&str>| NewLine {
        line_id: Uuid::now_v7(),
        payer_tenant_id: payer,
        seller_tenant_id: Some(tenant),
        resource_tenant_id: None,
        account_id,
        account_class: class,
        gl_code: None,
        side,
        amount_minor: 12_000,
        currency: "EUR".to_owned(),
        currency_scale: 2,
        invoice_id: invoice_id.map(str::to_owned),
        due_date: None,
        revenue_stream: stream.map(str::to_owned),
        mapping_status: MappingStatus::Resolved,
        // The functional (USD) leg supplied on the line — no rate resolve at post.
        functional_amount_minor: Some(13_200),
        functional_currency: Some("USD".to_owned()),
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
    };
    posting
        .post(
            &ctx,
            &scope,
            entry,
            vec![
                fx_line(
                    ar,
                    AccountClass::Ar,
                    Side::Debit,
                    Some("INV-NORATE-1"),
                    None,
                ),
                fx_line(
                    revenue,
                    AccountClass::Revenue,
                    Side::Credit,
                    None,
                    Some("subscription"),
                ),
            ],
            None,
        )
        .await
        .expect("seed cross-currency EUR receivable (functional supplied)");

    // Sanity: the AR grain is cross-currency + open ⇒ the revaluation will enumerate
    // it, then hit the rate resolve.
    assert_eq!(
        scalar_i64(&raw, &format!(
            "SELECT balance_minor FROM bss.ledger_ar_invoice_balance WHERE tenant_id='{tenant}' AND invoice_id='INV-NORATE-1'"
        )).await,
        Some(12_000),
        "AR carries the open EUR balance"
    );

    // NO EUR→USD rate row seeded anywhere. The revaluation enumerates the open
    // grain, then the period-end resolve finds zero candidates ⇒ FxRateUnavailable.
    let fx_config = FxConfig {
        provider_order: vec!["ecb".to_owned()],
        revaluation_enabled: true,
        ..FxConfig::default()
    };
    let runner = UnrealizedRevaluationRun::new(
        provider.clone(),
        Arc::new(LedgerEventPublisher::noop()),
        fx_config,
    );
    let err = runner
        .run_period(&ctx, &scope, tenant, &period_id, true)
        .await
        .expect_err("a revaluation with no period-end rate must fail, not post");
    assert!(
        matches!(err, DomainError::FxRateUnavailable(_)),
        "expected FxRateUnavailable, got {err:?}"
    );

    // The run errored BEFORE posting: no FX_REVALUATION entry, AR functional untouched.
    assert_eq!(
        scalar_i64(&raw, &format!(
            "SELECT count(*) FROM bss.ledger_journal_entry WHERE tenant_id='{tenant}' AND source_doc_type='FX_REVALUATION'"
        )).await,
        Some(0),
        "no revaluation entry posted when the rate is unavailable"
    );
    assert_eq!(
        scalar_i64(&raw, &format!(
            "SELECT functional_balance_minor FROM bss.ledger_ar_invoice_balance WHERE tenant_id='{tenant}' AND invoice_id='INV-NORATE-1'"
        )).await,
        Some(13_200),
        "AR functional carrying value untouched (still the posted 132.00 USD)"
    );
}
