//! Postgres-only integration: the Slice-3 `DebitNoteHandler` (Group D), driven
//! through the REAL foundation engine (`PostingService` + the in-txn
//! `DebitNotePostSidecar`) against a base invoice posted by `InvoicePostService`.
//! Asserts the design §4.3 / §11 durable effects of a debit note (an additional
//! charge, a DIRECT split mirroring S1 invoice-post):
//!
//! - a deferred debit note posts DR `AR` (incl. tax) + CR `REVENUE` (recognized
//!   now) + CR `CONTRACT_LIABILITY` (deferred) + CR `TAX_PAYABLE`, **never touching
//!   the posted invoice rows**, and in the SAME txn **builds a `recognition_schedule`
//!   (+ segments)** for the new deferred balance (D4) so a later S6 run can release
//!   it (no stuck liability);
//! - a debit note **raises** the invoice's headroom
//!   (`invoice_exposure.debit_note_total_minor += amount`), so a *credit note* that
//!   would have been over-cap before now fits;
//! - a **fully-recognized** debit note posts DR `AR` / CR `REVENUE` (+ tax) with NO
//!   `CONTRACT_LIABILITY` line and builds NO schedule.
//!
//! `LedgerLocalClient::new` is `pub(crate)`, so this out-of-crate test drives the
//! `pub` `DebitNoteHandler` + `InvoicePostService` + `CreditNoteHandler` directly
//! (mirrors `postgres_credit_note.rs`). Ignored by default; run with `-- --ignored`.

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
use bss_ledger::domain::adjustment::debit_note::DebitNoteRequest;
use bss_ledger::domain::invoice::builder::{InvoiceItem, PostedInvoice, TaxBreakdown};
use bss_ledger::domain::model::{AccountRow, CurrencyScaleRow};
use bss_ledger::domain::money::DEFAULT_PLAUSIBLE_MAX_MAJOR;
use bss_ledger::domain::recognition::input::{RecognitionInput, RecognitionTiming};
use bss_ledger::infra::adjustment::credit_note_service::CreditNoteHandler;
use bss_ledger::infra::adjustment::debit_note_service::DebitNoteHandler;
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

/// Provisioned seller ids (AR / REVENUE(sub) / CONTRACT_LIABILITY(sub) /
/// CONTRA_REVENUE / TAX / SUSPENSE) — the chart a debit note + a follow-up credit
/// note resolve against.
struct Seller {
    tenant: Uuid,
    payer: Uuid,
    ar: Uuid,
    revenue: Uuid,
    contract_liability: Uuid,
    contra_revenue: Uuid,
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

/// Boot, migrate, seed USD@2 + an OPEN period + the chart.
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
        account(
            s.tenant,
            s.contra_revenue,
            AccountClass::ContraRevenue,
            Side::Debit,
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

/// A fully-recognized `subscription` invoice item — PointInTime, so the whole
/// ex-tax amount recognizes now (deferred = 0, no schedule), carrying the
/// `invoice_item_ref` it books under.
fn recognized_item(amount: i64, item_ref: &str) -> InvoiceItem {
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
            // Recognize-now ⇒ PointInTime (whole ex-tax amount recognizes at
            // invoice, deferred = 0, no schedule). Modeling this as
            // StraightLine { periods: 1 } was wrong: by the domain contract a
            // single-period straight line still DEFERS the whole amount to
            // CONTRACT_LIABILITY and recognizes it in that one segment
            // (is_deferred() == true), minting a schedule and booking 0 to
            // REVENUE. PointInTime is the "recognizes now" the docstring intends.
            timing: RecognitionTiming::PointInTime,
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

/// A `subscription` invoice item deferred straight-line over `periods` — builds a
/// live ACTIVE recognition schedule (total_deferred = amount) that the debit-note
/// EXTEND path then folds into. (`recognized_item` is point-in-time; this defers.)
fn deferred_item(amount: i64, periods: u32, item_ref: &str) -> InvoiceItem {
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

fn debit_handler(provider: &DBProvider<DbError>) -> DebitNoteHandler {
    DebitNoteHandler::new(
        provider.clone(),
        Arc::new(LedgerEventPublisher::noop()),
        Arc::new(bss_ledger::domain::ports::metrics::NoopLedgerMetrics),
        RecognitionConfig::default(),
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

/// The number of `recognition_schedule` rows built against `invoice_id` (the debit
/// note's `source_invoice_id`) for `stream` — the D4 schedule-build assertion.
async fn schedule_count(raw: &DatabaseConnection, s: &Seller, invoice_id: &str) -> Option<i64> {
    scalar_i64(
        raw,
        &format!(
            "SELECT count(*) FROM bss.ledger_recognition_schedule \
             WHERE tenant_id='{}' AND source_invoice_id='{invoice_id}' \
               AND revenue_stream='subscription'",
            s.tenant
        ),
    )
    .await
}

async fn schedule_total_deferred(
    raw: &DatabaseConnection,
    s: &Seller,
    invoice_id: &str,
) -> Option<i64> {
    scalar_i64(
        raw,
        &format!(
            "SELECT total_deferred_minor FROM bss.ledger_recognition_schedule \
             WHERE tenant_id='{}' AND source_invoice_id='{invoice_id}' \
               AND revenue_stream='subscription'",
            s.tenant
        ),
    )
    .await
}

/// Σ of all recognition_segment amounts for the invoice's `subscription` schedule —
/// must equal `total_deferred` after an EXTEND merge (else the S6 run cannot drain
/// it exactly — stranded or over-recognized revenue).
async fn segment_sum(raw: &DatabaseConnection, s: &Seller, invoice_id: &str) -> Option<i64> {
    scalar_i64(
        raw,
        &format!(
            "SELECT COALESCE(SUM(seg.amount_minor), 0)::bigint \
             FROM bss.ledger_recognition_segment seg \
             JOIN bss.ledger_recognition_schedule sch \
               ON seg.tenant_id = sch.tenant_id AND seg.schedule_id = sch.schedule_id \
             WHERE sch.tenant_id = '{}' AND sch.source_invoice_id = '{invoice_id}' \
               AND sch.revenue_stream = 'subscription'",
            s.tenant
        ),
    )
    .await
}

async fn debit_note_total(raw: &DatabaseConnection, s: &Seller, invoice_id: &str) -> Option<i64> {
    scalar_i64(
        raw,
        &format!(
            "SELECT debit_note_total_minor FROM bss.ledger_invoice_exposure \
             WHERE tenant_id='{}' AND invoice_id='{invoice_id}'",
            s.tenant
        ),
    )
    .await
}

/// A debit-note request against `inv`'s `item-1` / `subscription` stream. Carries a
/// straight-line spec (the schedule build needs it) when `deferred_minor > 0`.
fn debit_req(
    s: &Seller,
    debit_note_id: &str,
    invoice_id: &str,
    amount_minor: i64,
    tax_minor: i64,
    deferred_minor: i64,
    periods: u32,
) -> DebitNoteRequest {
    DebitNoteRequest {
        tenant_id: s.tenant,
        payer_tenant_id: s.payer,
        debit_note_id: debit_note_id.to_owned(),
        origin_invoice_id: invoice_id.to_owned(),
        origin_invoice_item_ref: Some("item-1".to_owned()),
        revenue_stream: "subscription".to_owned(),
        currency: "USD".to_owned(),
        amount_minor,
        tax_minor,
        // Tax booking requires a dimensioned breakdown — chk_journal_line_tax_dims
        // rejects a TAX_PAYABLE line without (jurisdiction, filing_period). The
        // legacy bare-`tax_minor` path in `build_debit_note_legs` emits a
        // dimensionless TAX_PAYABLE leg that the schema rejects at insert, so a
        // taxed note must carry a breakdown (mirrors the S1 invoice-post tests).
        tax: if tax_minor > 0 {
            vec![TaxBreakdown {
                amount_minor: tax_minor,
                currency: "USD".to_owned(),
                tax_jurisdiction: "US-CA".to_owned(),
                tax_filing_period: "2026Q2".to_owned(),
                tax_rate_ref: None,
            }]
        } else {
            Vec::new()
        },
        deferred_minor,
        reason_code: "ADDITIONAL_USAGE".to_owned(),
        recognition: if deferred_minor > 0 {
            Some(RecognitionInput {
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
            })
        } else {
            None
        },
    }
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn debit_note_against_unposted_invoice_is_note_invoice_not_found() {
    // F4 (design §4.3 / §5): a debit note MUST link an originating posted invoice.
    // No `INVOICE_POST` entry for the referenced invoice ⇒ `NOTE_INVOICE_NOT_FOUND`
    // (404), BEFORE any ledger effect — no orphan charge entry, no exposure row.
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let (raw, provider, s) = setup(&url).await;
    let ctx = SecurityContext::anonymous();
    let scope = AccessScope::for_tenant(s.tenant);

    // Note the chart is provisioned but NO invoice was ever posted for INV-NONE.
    let err = debit_handler(&provider)
        .post_debit_note(
            &ctx,
            &scope,
            debit_req(&s, "DN-NF", "INV-NONE", 1000, 0, 0, 0),
            true,
        )
        .await
        .expect_err("a debit note against an unposted invoice must be rejected");
    assert!(
        matches!(
            err,
            bss_ledger::domain::error::DomainError::NoteInvoiceNotFound(_)
        ),
        "expected NoteInvoiceNotFound, got {err:?}"
    );

    // No books / counter effect: neither a debit_note row nor an exposure row exists.
    assert_eq!(
        scalar_i64(
            &raw,
            &format!(
                "SELECT count(*) FROM bss.ledger_debit_note \
                 WHERE tenant_id='{}' AND debit_note_id='DN-NF'",
                s.tenant
            ),
        )
        .await,
        Some(0),
        "no debit_note row was persisted"
    );
    assert_eq!(
        scalar_i64(
            &raw,
            &format!(
                "SELECT count(*) FROM bss.ledger_invoice_exposure \
                 WHERE tenant_id='{}' AND invoice_id='INV-NONE'",
                s.tenant
            ),
        )
        .await,
        Some(0),
        "no invoice_exposure row was seeded"
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn deferred_debit_note_books_ar_revenue_cl_and_builds_schedule() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let (raw, provider, s) = setup(&url).await;
    let harness = MetricsHarness::new();
    let ctx = SecurityContext::anonymous();
    let scope = AccessScope::for_tenant(s.tenant);

    // Base invoice: 1000 fully-recognized ⇒ AR 1000 / REVENUE 1000. Posted AR 1000.
    let inv = invoice(&s, "INV-DN", vec![recognized_item(1000, "item-1")]);
    invoice_svc(&provider, &harness)
        .post_invoice(&ctx, &scope, &inv, true)
        .await
        .expect("base invoice posts");
    assert_eq!(bal(&raw, &s, s.ar).await, Some(1000));
    assert_eq!(bal(&raw, &s, s.revenue).await, Some(1000));
    // The base invoice mints NO schedule (recognized over 1 period).
    assert_eq!(schedule_count(&raw, &s, "INV-DN").await, Some(0));

    // A 1000 ex-tax debit note, 600 deferred over 6 periods ⇒ DR AR 1000 / CR
    // REVENUE 400 / CR CONTRACT_LIABILITY 600 (+ D4 schedule for the 600).
    debit_handler(&provider)
        .post_debit_note(
            &ctx,
            &scope,
            debit_req(&s, "DN-1", "INV-DN", 1000, 0, 600, 6),
            true,
        )
        .await
        .expect("deferred debit note posts");

    // AR up by the full incl-tax 1000 ⇒ 2000; REVENUE up by the recognized 400 ⇒
    // 1400; CONTRACT_LIABILITY up by the deferred 600.
    assert_eq!(
        bal(&raw, &s, s.ar).await,
        Some(2000),
        "AR up by the additional charge"
    );
    assert_eq!(
        bal(&raw, &s, s.revenue).await,
        Some(1400),
        "REVENUE up by the recognized-now part"
    );
    assert_eq!(
        bal(&raw, &s, s.contract_liability).await,
        Some(600),
        "CONTRACT_LIABILITY up by the deferred part"
    );

    // D4: a recognition_schedule was built for the deferred 600 (so a later S6 run
    // can release it — no stuck liability).
    assert_eq!(
        schedule_count(&raw, &s, "INV-DN").await,
        Some(1),
        "D4 — a schedule was built for the deferred CL credit"
    );
    assert_eq!(
        schedule_total_deferred(&raw, &s, "INV-DN").await,
        Some(600),
        "schedule total_deferred = the debit note's deferred part"
    );

    // The headroom row is seeded (= posted AR 1000) and the debit note RAISED it.
    let original = scalar_i64(
        &raw,
        &format!(
            "SELECT original_total_minor FROM bss.ledger_invoice_exposure \
             WHERE tenant_id='{}' AND invoice_id='INV-DN'",
            s.tenant
        ),
    )
    .await;
    assert_eq!(original, Some(1000), "headroom seeded = posted AR");
    assert_eq!(
        debit_note_total(&raw, &s, "INV-DN").await,
        Some(1000),
        "debit_note_total_minor raised by the incl-tax note amount"
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn debit_note_raises_headroom_for_a_later_credit_note() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let (raw, provider, s) = setup(&url).await;
    let harness = MetricsHarness::new();
    let ctx = SecurityContext::anonymous();
    let scope = AccessScope::for_tenant(s.tenant);

    // Posted AR 1000 ⇒ headroom 1000 for credit notes.
    let inv = invoice(&s, "INV-HR", vec![recognized_item(1000, "item-1")]);
    invoice_svc(&provider, &harness)
        .post_invoice(&ctx, &scope, &inv, true)
        .await
        .expect("invoice posts");

    // A 500 fully-recognized debit note raises the headroom to 1500 (DR AR 500 / CR
    // REVENUE 500, no CL line, no schedule).
    debit_handler(&provider)
        .post_debit_note(
            &ctx,
            &scope,
            debit_req(&s, "DN-HR", "INV-HR", 500, 0, 0, 0),
            true,
        )
        .await
        .expect("fully-recognized debit note posts");
    assert_eq!(
        bal(&raw, &s, s.ar).await,
        Some(1500),
        "AR up by the additional charge"
    );
    assert_eq!(
        schedule_count(&raw, &s, "INV-HR").await,
        Some(0),
        "fully-recognized debit note builds no schedule"
    );
    assert_eq!(
        debit_note_total(&raw, &s, "INV-HR").await,
        Some(500),
        "headroom raised"
    );

    // A 1300 credit note now fits the raised headroom (1000 + 500 = 1500); it would
    // have been over-cap (1300 > 1000) before the debit note.
    let credit = CreditNoteRequest {
        tenant_id: s.tenant,
        payer_tenant_id: s.payer,
        credit_note_id: "CN-HR".to_owned(),
        origin_invoice_id: "INV-HR".to_owned(),
        origin_invoice_item_ref: Some("item-1".to_owned()),
        po_allocation_group: Some("grp-1".to_owned()),
        revenue_stream: "subscription".to_owned(),
        currency: "USD".to_owned(),
        amount_minor: 1300,
        tax_minor: 0,
        tax: Vec::new(),
        requested_deferred_minor: 0,
        reason_code: "CUSTOMER_GOODWILL".to_owned(),
        goodwill: false,
    };
    credit_handler(&provider)
        .post_credit_note(&ctx, &scope, credit)
        .await
        .expect("credit note fits the debit-note-raised headroom");

    let credit_total = scalar_i64(
        &raw,
        &format!(
            "SELECT credit_note_total_minor FROM bss.ledger_invoice_exposure \
             WHERE tenant_id='{}' AND invoice_id='INV-HR'",
            s.tenant
        ),
    )
    .await;
    assert_eq!(
        credit_total,
        Some(1300),
        "credit note recorded against the raised headroom"
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn fully_recognized_debit_note_books_no_contract_liability() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let (raw, provider, s) = setup(&url).await;
    let harness = MetricsHarness::new();
    let ctx = SecurityContext::anonymous();
    let scope = AccessScope::for_tenant(s.tenant);

    let inv = invoice(&s, "INV-FR", vec![recognized_item(1000, "item-1")]);
    invoice_svc(&provider, &harness)
        .post_invoice(&ctx, &scope, &inv, true)
        .await
        .expect("invoice posts");

    // A 1100 incl 100 tax fully-recognized debit note ⇒ DR AR 1100 / CR REVENUE
    // 1000 / CR TAX 100; NO CONTRACT_LIABILITY line, NO schedule.
    debit_handler(&provider)
        .post_debit_note(
            &ctx,
            &scope,
            debit_req(&s, "DN-FR", "INV-FR", 1100, 100, 0, 0),
            true,
        )
        .await
        .expect("fully-recognized debit note posts");

    assert_eq!(
        bal(&raw, &s, s.ar).await,
        Some(2100),
        "AR 1000 base + 1100 note"
    );
    assert_eq!(
        bal(&raw, &s, s.revenue).await,
        Some(2000),
        "REVENUE 1000 base + 1000 note"
    );
    assert_eq!(
        bal(&raw, &s, s.tax).await,
        Some(100),
        "TAX_PAYABLE credited the posted tax"
    );
    assert!(
        matches!(bal(&raw, &s, s.contract_liability).await, None | Some(0)),
        "fully-recognized debit note books no CONTRACT_LIABILITY"
    );
    assert_eq!(
        schedule_count(&raw, &s, "INV-FR").await,
        Some(0),
        "fully-recognized debit note builds no schedule"
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn deferred_debit_note_extends_the_live_schedule_not_a_second() {
    // Z3-1: a deferred debit note on an item that ALREADY has a live ACTIVE
    // recognition schedule EXTENDS it (one ACTIVE per key, the partial UNIQUE) —
    // total_deferred grows, overlapping-period segments fold in, and there is still
    // exactly ONE schedule_id. Without the fix the note's SCHEDULE_BUILD claim
    // collided with the base build → replay → skip, and its 600 deferred was lost.
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let (raw, provider, s) = setup(&url).await;
    let harness = MetricsHarness::new();
    let ctx = SecurityContext::anonymous();
    let scope = AccessScope::for_tenant(s.tenant);

    // Base invoice: a 1200 ex-tax item DEFERRED straight-line over 12 periods → one
    // live ACTIVE schedule (total_deferred 1200, 12 PENDING segments summing to 1200).
    let inv = invoice(&s, "INV-EXT", vec![deferred_item(1200, 12, "item-1")]);
    invoice_svc(&provider, &harness)
        .post_invoice(&ctx, &scope, &inv, true)
        .await
        .expect("base deferred invoice posts");
    assert_eq!(
        schedule_count(&raw, &s, "INV-EXT").await,
        Some(1),
        "base deferred item built exactly one schedule"
    );
    assert_eq!(
        schedule_total_deferred(&raw, &s, "INV-EXT").await,
        Some(1200)
    );
    assert_eq!(
        segment_sum(&raw, &s, "INV-EXT").await,
        Some(1200),
        "base Σ(segments) == total_deferred"
    );

    // A deferred debit note: 600 deferred over 6 periods on the SAME item-1 →
    // EXTENDS the live schedule (folds into periods 1-6), NOT a second schedule.
    debit_handler(&provider)
        .post_debit_note(
            &ctx,
            &scope,
            debit_req(&s, "DN-EXT", "INV-EXT", 600, 0, 600, 6),
            true,
        )
        .await
        .expect("deferred debit note extends the live schedule");

    // STILL exactly one ACTIVE schedule for the key (the partial UNIQUE allows one),
    // and total_deferred grew by the note's deferred part (1200 + 600).
    assert_eq!(
        schedule_count(&raw, &s, "INV-EXT").await,
        Some(1),
        "the debit note EXTENDED the live schedule — still ONE schedule, not a second"
    );
    assert_eq!(
        schedule_total_deferred(&raw, &s, "INV-EXT").await,
        Some(1800),
        "total_deferred grew by the note's deferred part (1200 + 600)"
    );
    // The merged segments still sum to total_deferred — the S6 run drains exactly,
    // no stranded or over-recognized revenue.
    assert_eq!(
        segment_sum(&raw, &s, "INV-EXT").await,
        Some(1800),
        "Σ(segment amounts) == total_deferred after the EXTEND merge"
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn debit_note_for_closed_payer_is_rejected() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let (raw, provider, s) = setup(&url).await;
    let harness = MetricsHarness::new();
    let ctx = SecurityContext::anonymous();
    let scope = AccessScope::for_tenant(s.tenant);

    let inv = invoice(&s, "INV-PC", vec![recognized_item(1000, "item-1")]);
    invoice_svc(&provider, &harness)
        .post_invoice(&ctx, &scope, &inv, true)
        .await
        .expect("invoice posts");

    // A debit note with payer_open = false is rejected before any ledger effect
    // (A-2 payer-close gate); AR stays at the base 1000 (no charge posted).
    let err = debit_handler(&provider)
        .post_debit_note(
            &ctx,
            &scope,
            debit_req(&s, "DN-PC", "INV-PC", 500, 0, 0, 0),
            false,
        )
        .await
        .expect_err("closed-payer debit note must reject");
    assert!(
        matches!(err, bss_ledger::domain::error::DomainError::PayerClosed(_)),
        "expected PayerClosed, got {err:?}"
    );
    assert_eq!(
        bal(&raw, &s, s.ar).await,
        Some(1000),
        "no charge posted for a closed payer"
    );
}
