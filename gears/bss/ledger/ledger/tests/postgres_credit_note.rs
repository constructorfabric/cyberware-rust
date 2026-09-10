//! Postgres-only integration: the Slice-3 `CreditNoteHandler` (Group C), driven
//! through the REAL foundation engine (`PostingService` + the in-txn
//! `CreditNotePostSidecar`) and a real recognized+deferred invoice posted by
//! `InvoicePostService` (the credited obligation). Asserts the design §4.2 / §11
//! durable effects:
//!
//! - a credit note posts DR `CONTRA_REVENUE`, DR `CONTRACT_LIABILITY` (deferred),
//!   and DR `TAX_PAYABLE` against CR `AR`, **never touching the posted invoice
//!   rows**, and in the SAME txn **reduces the owning schedule's
//!   `total_deferred_minor`** (so a later S6 run cannot re-recognize it);
//! - the `invoice_exposure` headroom CHECK **blocks an over-cap** credit note
//!   (`CreditNoteExceedsHeadroom` → `CREDIT_NOTE_EXCEEDS_HEADROOM`);
//! - a **goodwill** credit debits `GOODWILL` (not `CONTRA_REVENUE`) and touches no
//!   schedule;
//! - a credit note on a **paid** invoice credits the `REUSABLE_CREDIT` remainder
//!   and seeds the wallet sub-grain (K-2).
//!
//! `LedgerLocalClient::new` is `pub(crate)`, so this out-of-crate test drives the
//! `pub` `CreditNoteHandler` + `InvoicePostService` directly (mirrors
//! `postgres_recognition_build.rs`). Ignored by default; run with `-- --ignored`.

#![allow(
    clippy::non_ascii_literal,
    clippy::let_underscore_must_use,
    clippy::needless_collect,
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::doc_markdown,
    clippy::panic,
    clippy::too_many_lines,
    clippy::type_complexity
)]

use std::sync::Arc;

use bss_ledger::config::{FxConfig, RecognitionConfig};
use bss_ledger::domain::adjustment::credit_note::CreditNoteRequest;
use bss_ledger::domain::error::DomainError;
use bss_ledger::domain::invoice::builder::{InvoiceItem, PostedInvoice, TaxBreakdown};
use bss_ledger::domain::model::{AccountRow, CurrencyScaleRow};
use bss_ledger::domain::money::DEFAULT_PLAUSIBLE_MAX_MAJOR;
use bss_ledger::domain::recognition::input::{RecognitionInput, RecognitionTiming};
use bss_ledger::infra::adjustment::credit_note_service::CreditNoteHandler;
use bss_ledger::infra::events::publisher::LedgerEventPublisher;
use bss_ledger::infra::invoice_post::InvoicePostService;
use bss_ledger::infra::metrics::test_harness::MetricsHarness;
use bss_ledger::infra::storage::migrations::Migrator;
use bss_ledger::infra::storage::repo::ReferenceRepo;
use bss_ledger_sdk::{AccountClass, Side};
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

/// Provisioned seller ids (incl. the Slice-3 contra/goodwill/wallet accounts).
struct Seller {
    tenant: Uuid,
    payer: Uuid,
    ar: Uuid,
    revenue: Uuid,
    contract_liability: Uuid,
    contra_revenue: Uuid,
    goodwill: Uuid,
    reusable_credit: Uuid,
    tax: Uuid,
    suspense: Uuid,
    period_id: String,
}

fn account(
    tenant: Uuid,
    id: Uuid,
    class: AccountClass,
    normal: Side,
    stream: Option<&str>,
) -> AccountRow {
    AccountRow {
        account_id: id,
        tenant_id: tenant,
        legal_entity_id: tenant,
        account_class: class.as_str().to_owned(),
        currency: "USD".to_owned(),
        revenue_stream: stream.map(str::to_owned),
        normal_side: normal.as_str().to_owned(),
        may_go_negative: false,
        lifecycle_state: "OPEN".to_owned(),
    }
}

/// Boot, migrate, seed USD@2 + an OPEN period + the chart (AR / REVENUE(sub) /
/// CONTRACT_LIABILITY(sub) / CONTRA_REVENUE(sub) / GOODWILL / REUSABLE_CREDIT /
/// TAX / SUSPENSE).
async fn setup(url: &str) -> (DatabaseConnection, DBProvider<DbError>, Seller) {
    let raw = Database::connect(url).await.unwrap();
    Migrator::up(&raw, None).await.unwrap();
    let repo_url = format!("{url}?options=-c%20search_path%3Dbss,public");
    let tdb = connect_db(&repo_url, ConnectOpts::default()).await.unwrap();
    let provider = DBProvider::<DbError>::new(tdb);

    let s = Seller {
        tenant: Uuid::now_v7(),
        payer: Uuid::now_v7(),
        ar: Uuid::now_v7(),
        revenue: Uuid::now_v7(),
        contract_liability: Uuid::now_v7(),
        contra_revenue: Uuid::now_v7(),
        goodwill: Uuid::now_v7(),
        reusable_credit: Uuid::now_v7(),
        tax: Uuid::now_v7(),
        suspense: Uuid::now_v7(),
        period_id: "202606".to_owned(),
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
    // Seed BOTH the invoice's period (`s.period_id`) and the CURRENT period: the
    // adjustment handlers post into `OffsetDateTime::now_utc()`'s period (credit/debit-note
    // `eff_date = OffsetDateTime::now_utc()`), so a fixed historical period alone makes the test
    // date-dependent (green only in that calendar month). ON CONFLICT dedups when
    // `now` already equals `s.period_id`.
    raw.execute_raw(pg(format!(
        "INSERT INTO bss.ledger_fiscal_period (tenant_id, legal_entity_id, period_id, fiscal_tz, status)
         VALUES ('{t}','{t}','{p}','UTC','OPEN'), ('{t}','{t}','{cur}','UTC','OPEN')
         ON CONFLICT DO NOTHING",
        t = s.tenant,
        p = s.period_id,
        cur = bss_ledger::domain::instant::yyyymm(OffsetDateTime::now_utc())
    )))
    .await
    .unwrap();

    for row in [
        account(s.tenant, s.ar, AccountClass::Ar, Side::Debit, None),
        account(
            s.tenant,
            s.revenue,
            AccountClass::Revenue,
            Side::Credit,
            Some("subscription"),
        ),
        account(
            s.tenant,
            s.contract_liability,
            AccountClass::ContractLiability,
            Side::Credit,
            Some("subscription"),
        ),
        // CONTRA_REVENUE is NOT a per-stream class (SDK `PER_STREAM` = REVENUE +
        // CONTRACT_LIABILITY only), so `ChartIndex::resolve` keys it stream-less —
        // provision it stream-less. The credit-note line may still CARRY the stream
        // tag (the journal CHECK allows a stream on non-REVENUE/CL classes; it is
        // forward-compat for the Phase-3 contra-paired-with-revenue disaggregation),
        // but the chart row it resolves to is the single stream-less account.
        account(
            s.tenant,
            s.contra_revenue,
            AccountClass::ContraRevenue,
            Side::Debit,
            None,
        ),
        account(
            s.tenant,
            s.goodwill,
            AccountClass::Goodwill,
            Side::Debit,
            None,
        ),
        account(
            s.tenant,
            s.reusable_credit,
            AccountClass::ReusableCredit,
            Side::Credit,
            None,
        ),
        account(
            s.tenant,
            s.tax,
            AccountClass::TaxPayable,
            Side::Credit,
            None,
        ),
        account(
            s.tenant,
            s.suspense,
            AccountClass::Suspense,
            Side::Credit,
            None,
        ),
    ] {
        reference.insert_account(row).await.unwrap();
    }
    (raw, provider, s)
}

/// A `subscription` item, straight-line deferred over `periods`, with the
/// `invoice_item_ref` a deferred line requires.
fn recognized_item(amount: i64, periods: u32, item_ref: &str) -> InvoiceItem {
    InvoiceItem {
        amount_minor_ex_tax: amount,
        deferred_minor: 0,
        currency: "USD".to_owned(),
        revenue_stream: "subscription".to_owned(),
        catalog_class: Some(AccountClass::Revenue),
        contract_class: None,
        gl_code: Some("4000".to_owned()),
        recognition: Some(RecognitionInput {
            policy_ref: "policy.sl.v1".to_owned(),
            timing: RecognitionTiming::StraightLine {
                periods,
                first_period_id: None,
            },
            po_allocation_group: Some("grp-1".to_owned()),
            multi_po: false,
            ssp_snapshot_ref: None,
            subscription_ref: Some("sub-1".to_owned()),
            vc_estimate_ref: None,
            vc_method_ref: None,
            immaterial_one_shot_sku: false,
        }),
        invoice_item_ref: Some(item_ref.to_owned()),
        sku_or_plan_ref: None,
        price_id: None,
        pricing_snapshot_ref: None,
    }
}

fn invoice(s: &Seller, invoice_id: &str, items: Vec<InvoiceItem>) -> PostedInvoice {
    PostedInvoice {
        invoice_id: invoice_id.to_owned(),
        payer_tenant_id: s.payer,
        resource_tenant_id: None,
        seller_tenant_id: s.tenant,
        effective_at: naive(2026, 6, 1),
        due_date: Some(naive(2026, 7, 1)),
        period_id: s.period_id.clone(),
        items,
        tax: Vec::<TaxBreakdown>::new(),
        posted_by_actor_id: s.tenant,
        correlation_id: s.tenant,
    }
}

fn invoice_svc(provider: &DBProvider<DbError>, metrics: &MetricsHarness) -> InvoicePostService {
    InvoicePostService::new(
        provider.clone(),
        Arc::new(LedgerEventPublisher::noop()),
        Arc::new(metrics.metrics()),
        RecognitionConfig::default(),
        FxConfig::default(),
    )
}

fn credit_handler(provider: &DBProvider<DbError>) -> CreditNoteHandler {
    CreditNoteHandler::new(
        provider.clone(),
        Arc::new(LedgerEventPublisher::noop()),
        Arc::new(bss_ledger::domain::ports::metrics::NoopLedgerMetrics),
    )
}

async fn bal(raw: &DatabaseConnection, s: &Seller, account: Uuid) -> Option<i64> {
    scalar_i64(
        raw,
        &format!(
            "SELECT balance_minor FROM bss.ledger_account_balance \
             WHERE tenant_id='{}' AND account_id='{}' AND currency='USD'",
            s.tenant, account
        ),
    )
    .await
}

async fn total_deferred(raw: &DatabaseConnection, s: &Seller, invoice_id: &str) -> Option<i64> {
    scalar_i64(
        raw,
        &format!(
            "SELECT total_deferred_minor FROM bss.ledger_recognition_schedule \
             WHERE tenant_id='{}' AND source_invoice_id='{invoice_id}'",
            s.tenant
        ),
    )
    .await
}

// TODO(VHP-1856 Slice 3 Phase 3): an `#[ignore]` integration test that posts a
// credit note carrying a multi-component `tax` breakdown and asserts the projected
// `tax_subbalance` is disaggregated per `(jurisdiction, filing)` (and that the
// `NegativeTaxSubbalance` alarm added in Group 2 is now reachable for notes). The
// pure per-component leg routing is covered by the `credit_note_tests` unit suite
// (`tax_breakdown_emits_per_component_tax_legs`); the projector-grain assertion
// needs the `tax_subbalance` read path wired into this harness first.

/// A baseline non-goodwill credit-note request against `inv`'s `item-1` /
/// `subscription` stream.
fn credit_req(
    s: &Seller,
    credit_note_id: &str,
    invoice_id: &str,
    amount_minor: i64,
    tax_minor: i64,
    requested_deferred_minor: i64,
) -> CreditNoteRequest {
    CreditNoteRequest {
        tenant_id: s.tenant,
        payer_tenant_id: s.payer,
        credit_note_id: credit_note_id.to_owned(),
        origin_invoice_id: invoice_id.to_owned(),
        origin_invoice_item_ref: Some("item-1".to_owned()),
        po_allocation_group: Some("grp-1".to_owned()),
        revenue_stream: "subscription".to_owned(),
        currency: "USD".to_owned(),
        amount_minor,
        tax_minor,
        tax: Vec::new(),
        requested_deferred_minor,
        reason_code: "CUSTOMER_GOODWILL".to_owned(),
        goodwill: false,
    }
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn credit_note_against_unposted_invoice_is_note_invoice_not_found() {
    // F4 (design §4.2 / §5): a credit note MUST link an originating posted invoice.
    // No `INVOICE_POST` entry for the referenced invoice ⇒ `NOTE_INVOICE_NOT_FOUND`
    // (404), BEFORE any read/split/post — no orphan compensating entry.
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let (raw, provider, s) = setup(&url).await;
    let ctx = SecurityContext::anonymous();
    let scope = AccessScope::for_tenant(s.tenant);

    // Chart provisioned but NO invoice was ever posted for INV-NONE.
    let err = credit_handler(&provider)
        .post_credit_note(&ctx, &scope, credit_req(&s, "CN-NF", "INV-NONE", 300, 0, 0))
        .await
        .expect_err("a credit note against an unposted invoice must be rejected");
    assert!(
        matches!(err, DomainError::NoteInvoiceNotFound(_)),
        "expected NoteInvoiceNotFound, got {err:?}"
    );

    // No books / record effect: no credit_note row was persisted.
    assert_eq!(
        scalar_i64(
            &raw,
            &format!(
                "SELECT count(*) FROM bss.ledger_credit_note \
                 WHERE tenant_id='{}' AND credit_note_id='CN-NF'",
                s.tenant
            ),
        )
        .await,
        Some(0),
        "no credit_note row was persisted"
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn deferred_credit_note_reduces_cl_ar_and_schedule_total() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let (raw, provider, s) = setup(&url).await;
    let harness = MetricsHarness::new();
    let ctx = SecurityContext::anonymous();
    let scope = AccessScope::for_tenant(s.tenant);

    // A 1200 ex-tax subscription, straight-line over 12 ⇒ the whole 1200 defers to
    // CONTRACT_LIABILITY; AR = 1200; schedule total_deferred = 1200.
    let inv = invoice(&s, "INV-DEF", vec![recognized_item(1200, 12, "item-1")]);
    invoice_svc(&provider, &harness)
        .post_invoice(&ctx, &scope, &inv, true)
        .await
        .expect("deferred invoice posts");
    assert_eq!(bal(&raw, &s, s.ar).await, Some(1200));
    assert_eq!(bal(&raw, &s, s.contract_liability).await, Some(1200));
    assert_eq!(total_deferred(&raw, &s, "INV-DEF").await, Some(1200));

    // Credit 300 (ex-tax, no tax) entirely against the deferred balance.
    let req = credit_req(&s, "CN-1", "INV-DEF", 300, 0, 300);
    credit_handler(&provider)
        .post_credit_note(&ctx, &scope, req)
        .await
        .expect("deferred credit note posts");

    // CONTRACT_LIABILITY net down by 300 (1200 − 300); AR net down by 300; the
    // posted invoice's schedule total_deferred reduced to 900 (a later run cannot
    // re-recognize the 300). CONTRA_REVENUE untouched (no recognized part).
    assert_eq!(
        bal(&raw, &s, s.contract_liability).await,
        Some(900),
        "CONTRACT_LIABILITY reduced by the deferred credit"
    );
    assert_eq!(bal(&raw, &s, s.ar).await, Some(900), "AR reduced incl. tax");
    assert_eq!(
        total_deferred(&raw, &s, "INV-DEF").await,
        Some(900),
        "schedule total_deferred reduced — S6 cannot re-recognize the credited-back 300"
    );
    assert!(
        matches!(bal(&raw, &s, s.contra_revenue).await, None | Some(0)),
        "no recognized part ⇒ no CONTRA_REVENUE"
    );

    // The headroom row is seeded (= posted AR 1200) with the running credit total.
    let original = scalar_i64(
        &raw,
        &format!(
            "SELECT original_total_minor FROM bss.ledger_invoice_exposure \
             WHERE tenant_id='{}' AND invoice_id='INV-DEF'",
            s.tenant
        ),
    )
    .await;
    assert_eq!(original, Some(1200), "headroom seeded = posted AR");
    let credit_total = scalar_i64(
        &raw,
        &format!(
            "SELECT credit_note_total_minor FROM bss.ledger_invoice_exposure \
             WHERE tenant_id='{}' AND invoice_id='INV-DEF'",
            s.tenant
        ),
    )
    .await;
    assert_eq!(credit_total, Some(300), "running credit-note total bumped");
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn headroom_check_blocks_over_cap_credit_note() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let (raw, provider, s) = setup(&url).await;
    let harness = MetricsHarness::new();
    let ctx = SecurityContext::anonymous();
    let scope = AccessScope::for_tenant(s.tenant);

    // Posted AR 1000 (a fully-recognized 1000 invoice — recognition over 1 period
    // recognizes now, so AR 1000 / Revenue 1000, no deferred).
    let inv = invoice(&s, "INV-CAP", vec![recognized_item(1000, 1, "item-1")]);
    invoice_svc(&provider, &harness)
        .post_invoice(&ctx, &scope, &inv, true)
        .await
        .expect("invoice posts");

    // First credit note 700 (recognized) — within headroom 1000.
    credit_handler(&provider)
        .post_credit_note(&ctx, &scope, credit_req(&s, "CN-A", "INV-CAP", 700, 0, 0))
        .await
        .expect("first credit within headroom");

    // Second credit note 500 ⇒ running 1200 > 1000 headroom ⇒ blocked by the
    // invoice_exposure CHECK, surfaced as CreditNoteExceedsHeadroom; the whole post
    // rolls back (no partial CONTRA/AR effect).
    let err = credit_handler(&provider)
        .post_credit_note(&ctx, &scope, credit_req(&s, "CN-B", "INV-CAP", 500, 0, 0))
        .await
        .expect_err("over-cap credit note must block");
    assert!(
        matches!(err, DomainError::CreditNoteExceedsHeadroom(_)),
        "expected CreditNoteExceedsHeadroom, got {err:?}"
    );
    // The running credit total stayed at 700 (the over-cap note rolled back).
    let credit_total = scalar_i64(
        &raw,
        &format!(
            "SELECT credit_note_total_minor FROM bss.ledger_invoice_exposure \
             WHERE tenant_id='{}' AND invoice_id='INV-CAP'",
            s.tenant
        ),
    )
    .await;
    assert_eq!(credit_total, Some(700), "over-cap note rolled back");
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn goodwill_credit_uses_goodwill_class_not_contra() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let (raw, provider, s) = setup(&url).await;
    let harness = MetricsHarness::new();
    let ctx = SecurityContext::anonymous();
    let scope = AccessScope::for_tenant(s.tenant);

    // Posted AR 1000 (fully recognized).
    let inv = invoice(&s, "INV-GW", vec![recognized_item(1000, 1, "item-1")]);
    invoice_svc(&provider, &harness)
        .post_invoice(&ctx, &scope, &inv, true)
        .await
        .expect("invoice posts");

    // A 200 goodwill credit (no tax, no deferred): DR GOODWILL 200 / CR AR 200.
    let mut req = credit_req(&s, "CN-GW", "INV-GW", 200, 0, 0);
    req.goodwill = true;
    credit_handler(&provider)
        .post_credit_note(&ctx, &scope, req)
        .await
        .expect("goodwill credit posts");

    assert_eq!(
        bal(&raw, &s, s.goodwill).await,
        Some(200),
        "GOODWILL debited"
    );
    assert!(
        matches!(bal(&raw, &s, s.contra_revenue).await, None | Some(0)),
        "goodwill never uses CONTRA_REVENUE"
    );
    assert_eq!(
        bal(&raw, &s, s.ar).await,
        Some(800),
        "AR reduced by goodwill"
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn paid_invoice_credit_seeds_reusable_credit_wallet() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let (raw, provider, s) = setup(&url).await;
    let harness = MetricsHarness::new();
    let ctx = SecurityContext::anonymous();
    let scope = AccessScope::for_tenant(s.tenant);

    // Post a 500 fully-recognized invoice, then drain its open AR to 0 by a manual
    // AR-credit (simulating a payment allocation: DR SUSPENSE / CR AR) so the
    // invoice is "paid" — the credit-note remainder must then route to the wallet.
    let inv = invoice(&s, "INV-PAID", vec![recognized_item(500, 1, "item-1")]);
    invoice_svc(&provider, &harness)
        .post_invoice(&ctx, &scope, &inv, true)
        .await
        .expect("invoice posts");
    // Drain open AR to 0 directly in the cache (test shortcut — the open-AR read is
    // what gates the AR-vs-wallet split; a real payment would net it the same).
    raw.execute_raw(pg(format!(
        "UPDATE bss.ledger_ar_invoice_balance SET balance_minor = 0 \
         WHERE tenant_id='{}' AND invoice_id='INV-PAID'",
        s.tenant
    )))
    .await
    .unwrap();

    // A 300 credit on the now-paid invoice ⇒ open AR 0 ⇒ the whole 300 seeds the
    // REUSABLE_CREDIT wallet (K-2), no AR credit.
    credit_handler(&provider)
        .post_credit_note(
            &ctx,
            &scope,
            credit_req(&s, "CN-PAID", "INV-PAID", 300, 0, 0),
        )
        .await
        .expect("paid-invoice credit posts");

    assert_eq!(
        bal(&raw, &s, s.reusable_credit).await,
        Some(300),
        "remainder beyond open AR seeds REUSABLE_CREDIT"
    );
    // The wallet sub-grain is seeded under credit_grant_event_type = CREDIT_NOTE.
    let wallet = scalar_i64(
        &raw,
        &format!(
            "SELECT balance_minor FROM bss.ledger_reusable_credit_subbalance \
             WHERE tenant_id='{}' AND payer_tenant_id='{}' AND currency='USD' \
               AND credit_grant_event_type='CREDIT_NOTE'",
            s.tenant, s.payer
        ),
    )
    .await;
    assert_eq!(wallet, Some(300), "reusable_credit_subbalance seeded (K-2)");
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn mixed_credit_note_books_both_contra_and_cl() {
    // The MIXED split (the central §4.2 case the all-deferred / all-goodwill tests
    // never exercise): a note whose ex-tax amount is SPLIT into a recognized part
    // (→ DR CONTRA_REVENUE) AND a deferred part (→ DR CONTRACT_LIABILITY), both
    // against CR AR. The recognized-leg path + the per-stream CL reduction fire
    // together in one balanced post.
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let (raw, provider, s) = setup(&url).await;
    let harness = MetricsHarness::new();
    let ctx = SecurityContext::anonymous();
    let scope = AccessScope::for_tenant(s.tenant);

    // A 1200 ex-tax subscription, straight-line over 12 ⇒ the whole 1200 defers to
    // CONTRACT_LIABILITY (releasable remainder = 1200); AR = 1200.
    let inv = invoice(&s, "INV-MIX", vec![recognized_item(1200, 12, "item-1")]);
    invoice_svc(&provider, &harness)
        .post_invoice(&ctx, &scope, &inv, true)
        .await
        .expect("deferred invoice posts");
    assert_eq!(bal(&raw, &s, s.ar).await, Some(1200));
    assert_eq!(bal(&raw, &s, s.contract_liability).await, Some(1200));
    assert_eq!(total_deferred(&raw, &s, "INV-MIX").await, Some(1200));

    // Credit 300 ex-tax, of which only 100 is requested deferred ⇒ recognized part
    // = 300 − 100 = 200 (DR CONTRA_REVENUE 200), deferred part = 100 (DR
    // CONTRACT_LIABILITY 100, within the 1200 releasable). CR AR = 300.
    credit_handler(&provider)
        .post_credit_note(
            &ctx,
            &scope,
            credit_req(&s, "CN-MIX", "INV-MIX", 300, 0, 100),
        )
        .await
        .expect("mixed credit note posts");

    // BOTH debit legs booked: CONTRA_REVENUE = 200 (the recognized part) and
    // CONTRACT_LIABILITY net down by 100 (1200 − 100). AR net down by the full 300.
    assert_eq!(
        bal(&raw, &s, s.contra_revenue).await,
        Some(200),
        "the recognized part debits CONTRA_REVENUE"
    );
    assert_eq!(
        bal(&raw, &s, s.contract_liability).await,
        Some(1100),
        "the deferred part reduces CONTRACT_LIABILITY (1200 − 100)"
    );
    assert_eq!(
        bal(&raw, &s, s.ar).await,
        Some(900),
        "AR reduced by the full credited 300 (200 recognized + 100 deferred)"
    );
    // Only the deferred 100 reduces the schedule (the recognized 200 was never
    // deferred, so it does not touch total_deferred — S6 can't re-recognize the 100).
    assert_eq!(
        total_deferred(&raw, &s, "INV-MIX").await,
        Some(1100),
        "schedule total_deferred reduced by the deferred part only (1200 − 100)"
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn goodwill_credit_over_open_ar_is_rejected() {
    // Over-application guard (design §4.2, K-2): a GOODWILL credit is AR-only — it
    // may relieve open AR but must NEVER mint spendable REUSABLE_CREDIT. A goodwill
    // amount that exceeds the invoice's open AR has a wallet remainder, so it is
    // rejected (InvalidRequest) rather than converting a goodwill gesture into a
    // cash-equivalent grant. (A non-goodwill paid-invoice credit DOES seed the
    // wallet — that is the `paid_invoice_credit_seeds_reusable_credit_wallet` path;
    // this proves goodwill is held to the stricter AR-only rule.)
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let (raw, provider, s) = setup(&url).await;
    let harness = MetricsHarness::new();
    let ctx = SecurityContext::anonymous();
    let scope = AccessScope::for_tenant(s.tenant);

    // Posted AR 1000 (fully recognized), then drain its open AR to 200 (a partial
    // payment) so a 300 goodwill credit has only 200 of AR to relieve — a 100
    // wallet remainder the goodwill rule forbids.
    let inv = invoice(&s, "INV-GWO", vec![recognized_item(1000, 1, "item-1")]);
    invoice_svc(&provider, &harness)
        .post_invoice(&ctx, &scope, &inv, true)
        .await
        .expect("invoice posts");
    raw.execute_raw(pg(format!(
        "UPDATE bss.ledger_ar_invoice_balance SET balance_minor = 200 \
         WHERE tenant_id='{}' AND invoice_id='INV-GWO'",
        s.tenant
    )))
    .await
    .unwrap();
    // A real partial payment reduces BOTH the per-invoice AR grain (above) AND the
    // per-account AR grain that `bal(s.ar)` reads (`ledger_account_balance`). The
    // raw shortcut must touch both, else the post-rejection assertion `bal(s.ar) ==
    // 200` sees the untouched 1000 from the invoice post.
    raw.execute_raw(pg(format!(
        "UPDATE bss.ledger_account_balance SET balance_minor = 200 \
         WHERE tenant_id='{}' AND account_id='{}' AND currency='USD'",
        s.tenant, s.ar
    )))
    .await
    .unwrap();

    // A 300 goodwill credit > open AR 200 ⇒ 100 wallet remainder ⇒ InvalidRequest
    // (goodwill is AR-only). 300 is within the 1000 headroom, so it is the goodwill
    // wallet-mint rule — not the headroom CHECK — that rejects it.
    let mut req = credit_req(&s, "CN-GWO", "INV-GWO", 300, 0, 0);
    req.goodwill = true;
    let err = credit_handler(&provider)
        .post_credit_note(&ctx, &scope, req)
        .await
        .expect_err("a goodwill credit exceeding open AR must be rejected");
    assert!(
        matches!(err, DomainError::InvalidRequest(_)),
        "expected InvalidRequest (goodwill is AR-only, cannot mint reusable credit), got {err:?}"
    );

    // Rejected with NO books / record effect: AR untouched at 200, GOODWILL never
    // debited, no wallet sub-grain seeded, no credit_note row persisted.
    assert_eq!(
        bal(&raw, &s, s.ar).await,
        Some(200),
        "AR untouched by the rejected goodwill credit"
    );
    assert!(
        matches!(bal(&raw, &s, s.goodwill).await, None | Some(0)),
        "GOODWILL never debited"
    );
    assert_eq!(
        scalar_i64(
            &raw,
            &format!(
                "SELECT count(*) FROM bss.ledger_reusable_credit_subbalance \
                 WHERE tenant_id='{}' AND payer_tenant_id='{}'",
                s.tenant, s.payer
            ),
        )
        .await,
        Some(0),
        "no REUSABLE_CREDIT wallet sub-grain was seeded"
    );
    assert_eq!(
        scalar_i64(
            &raw,
            &format!(
                "SELECT count(*) FROM bss.ledger_credit_note \
                 WHERE tenant_id='{}' AND credit_note_id='CN-GWO'",
                s.tenant
            ),
        )
        .await,
        Some(0),
        "no credit_note row persisted (rejected before the post)"
    );
}
