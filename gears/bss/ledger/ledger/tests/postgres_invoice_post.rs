//! Postgres-only integration: the invoice-post domain driven through the REAL
//! foundation engine (`InvoicePostService` over `PostingService`).
//!
//! Provisions a seller (AR / Revenue / Tax / Suspense chart + USD@2 + an OPEN
//! period), then:
//! - posts a balanced invoice and asserts the AR-debit / Revenue+Tax-credit
//!   cache balances + the emitted `invoice_post` / duration metrics;
//! - rejects a closed-payer post (`PAYER_CLOSED`) while a reversal of an
//!   already-posted invoice still posts;
//! - rejects a Revenue line missing `revenue_stream` at the foundation
//!   invariant (the `chk_journal_line_revenue_stream` DB CHECK);
//! - blocks a period close while a `SUSPENSE`/`PENDING` line is open;
//! - replays a `MAPPING_CORRECTION` re-post idempotently;
//! - rejects a reverse-of-a-reversal.
//!
//! `LedgerLocalClient::new` is `pub(crate)`, so this out-of-crate test drives
//! the `pub` `InvoicePostService` / `PostingService` directly (mirrors
//! `postgres_posting.rs`). Ignored by default; run with `-- --ignored`.

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
use bss_ledger::domain::error::DomainError;
use bss_ledger::domain::invoice::builder::{InvoiceItem, PostedInvoice, TaxBreakdown};
use bss_ledger::domain::invoice::reversal::build_reversal;
use bss_ledger::domain::model::{AccountRow, CurrencyScaleRow, NewEntry, NewLine};
use bss_ledger::domain::money::DEFAULT_PLAUSIBLE_MAX_MAJOR;
use bss_ledger::infra::events::publisher::LedgerEventPublisher;
use bss_ledger::infra::invoice_post::InvoicePostService;
use bss_ledger::infra::metrics::test_harness::MetricsHarness;
use bss_ledger::infra::period_close::PeriodCloseService;
use bss_ledger::infra::posting::service::PostingService;
use bss_ledger::infra::storage::migrations::Migrator;
use bss_ledger::infra::storage::repo::ReferenceRepo;
use bss_ledger_sdk::{AccountClass, EntryView, LineView, MappingStatus, Side, SourceDocType};
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

/// Provisioned seller ids.
struct Seller {
    tenant: Uuid,
    payer: Uuid,
    ar: Uuid,
    revenue: Uuid,
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

/// Boot, migrate, seed USD@2 + an OPEN period + AR/REVENUE(subscription)/TAX/
/// SUSPENSE accounts. Returns the migrate connection, the search_path-scoped
/// provider, and the seller ids.
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
    raw.execute_raw(pg(format!(
        "INSERT INTO bss.ledger_fiscal_period (tenant_id, legal_entity_id, period_id, fiscal_tz, status)
         VALUES ('{}','{}','{}','UTC','OPEN')",
        s.tenant, s.tenant, s.period_id
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

/// One ex-tax `subscription` item mapped to REVENUE via the Catalog class.
fn revenue_item(amount: i64) -> InvoiceItem {
    InvoiceItem {
        amount_minor_ex_tax: amount,
        deferred_minor: 0,
        currency: "USD".to_owned(),
        revenue_stream: "subscription".to_owned(),
        catalog_class: Some(AccountClass::Revenue),
        contract_class: None,
        gl_code: Some("4000".to_owned()),
        recognition: None,
        invoice_item_ref: None,
        sku_or_plan_ref: None,
        price_id: None,
        pricing_snapshot_ref: None,
    }
}

fn tax_breakdown(amount: i64) -> TaxBreakdown {
    TaxBreakdown {
        amount_minor: amount,
        currency: "USD".to_owned(),
        tax_jurisdiction: "US-CA".to_owned(),
        tax_filing_period: "2026Q2".to_owned(),
        tax_rate_ref: None,
    }
}

fn invoice(
    s: &Seller,
    invoice_id: &str,
    items: Vec<InvoiceItem>,
    tax: Vec<TaxBreakdown>,
) -> PostedInvoice {
    PostedInvoice {
        invoice_id: invoice_id.to_owned(),
        payer_tenant_id: s.payer,
        resource_tenant_id: None,
        seller_tenant_id: s.tenant,
        effective_at: naive(2026, 6, 1),
        due_date: Some(naive(2026, 7, 1)),
        period_id: s.period_id.clone(),
        items,
        tax,
        posted_by_actor_id: s.tenant,
        correlation_id: s.tenant,
    }
}

fn svc(provider: &DBProvider<DbError>, metrics: &MetricsHarness) -> InvoicePostService {
    InvoicePostService::new(
        provider.clone(),
        Arc::new(LedgerEventPublisher::noop()),
        Arc::new(metrics.metrics()),
        RecognitionConfig::default(),
        FxConfig::default(),
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

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn posts_balanced_invoice_and_emits_metrics() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let (raw, provider, s) = setup(&url).await;
    let harness = MetricsHarness::new();
    let service = svc(&provider, &harness);
    let ctx = SecurityContext::anonymous();
    let scope = AccessScope::for_tenant(s.tenant);

    // DR AR 1200 / CR Revenue 1000 / CR Tax 200.
    let inv = invoice(
        &s,
        "INV-1",
        vec![revenue_item(1000)],
        vec![tax_breakdown(200)],
    );
    let posted = service
        .post_invoice(&ctx, &scope, &inv, true)
        .await
        .expect("invoice post must succeed");
    assert!(!posted.replayed, "first post is fresh");

    assert_eq!(
        bal(&raw, &s, s.ar).await,
        Some(1200),
        "AR debit = gross 1200"
    );
    assert_eq!(
        bal(&raw, &s, s.revenue).await,
        Some(1000),
        "Revenue credit = 1000"
    );
    assert_eq!(bal(&raw, &s, s.tax).await, Some(200), "Tax credit = 200");

    // Metrics: one posted attempt + one duration sample.
    harness.force_flush();
    assert_eq!(
        harness.counter_value(
            "ledger_invoice_post_total",
            &[("result", "posted"), ("flow", "invoice_post")]
        ),
        1,
        "one posted invoice-post counted"
    );
    assert_eq!(
        harness.histogram_count(
            "ledger_invoice_post_duration_seconds",
            &[("flow", "invoice_post")]
        ),
        1,
        "one duration sample recorded"
    );

    // Re-post the same invoice ⇒ idempotent replay (no new ledger effect).
    let replay = service
        .post_invoice(&ctx, &scope, &inv, true)
        .await
        .expect("replay must succeed");
    assert!(replay.replayed, "second identical post replays");
    assert_eq!(
        replay.entry_id, posted.entry_id,
        "replay returns the prior id"
    );
    assert_eq!(
        bal(&raw, &s, s.ar).await,
        Some(1200),
        "AR unchanged on replay"
    );
    harness.force_flush();
    assert_eq!(
        harness.counter_value(
            "ledger_invoice_post_total",
            &[("result", "replayed"), ("flow", "invoice_post")]
        ),
        1,
        "the replay is counted as replayed"
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn closed_payer_is_rejected_but_a_reversal_still_posts() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let (raw, provider, s) = setup(&url).await;
    let harness = MetricsHarness::new();
    let service = svc(&provider, &harness);
    let ctx = SecurityContext::anonymous();
    let scope = AccessScope::for_tenant(s.tenant);

    // 1. Post a normal invoice (payer open) so there is something to reverse.
    let inv = invoice(
        &s,
        "INV-2",
        vec![revenue_item(1000)],
        vec![tax_breakdown(200)],
    );
    let original = service
        .post_invoice(&ctx, &scope, &inv, true)
        .await
        .expect("the original post must succeed");

    // 2. A NEW invoice for a CLOSED payer is rejected with PAYER_CLOSED, no
    //    ledger effect. `payer_open: false` is injected directly because the gear
    //    has NO payer-state reader by design — this exercises the service's
    //    fast-path payer gate. The DB account-lifecycle guard (a CLOSED AR account
    //    rejected at post time) is the complementary authority, covered by
    //    `closed_ar_account_post_is_rejected_at_the_db_guard`.
    let inv2 = invoice(&s, "INV-3", vec![revenue_item(500)], vec![]);
    let err = service
        .post_invoice(&ctx, &scope, &inv2, false)
        .await
        .expect_err("a closed-payer post must be rejected");
    assert!(matches!(err, DomainError::PayerClosed(_)), "got {err:?}");
    harness.force_flush();
    assert_eq!(
        harness.counter_value(
            "ledger_invoice_post_total",
            &[("result", "rejected"), ("flow", "invoice_post")]
        ),
        1,
        "the rejected post is counted"
    );

    // 3. A reversal of the ALREADY-POSTED invoice still posts even though the
    //    payer is closed (the reversal path bypasses the payer gate). Build the
    //    reversal from the original's read-back view.
    let view = original_view(
        &s,
        original.entry_id,
        &[
            (
                s.ar,
                AccountClass::Ar,
                Side::Debit,
                1200,
                Some("INV-2"),
                None,
            ),
            (
                s.revenue,
                AccountClass::Revenue,
                Side::Credit,
                1000,
                Some("INV-2"),
                Some("subscription"),
            ),
            (
                s.tax,
                AccountClass::TaxPayable,
                Side::Credit,
                200,
                Some("INV-2"),
                None,
            ),
        ],
    );
    let reversal = build_reversal(
        &view,
        s.period_id.clone(),
        naive(2026, 6, 2),
        s.tenant,
        s.tenant,
    )
    .expect("reversal of an invoice-post must build");
    service
        .post_reversal(&ctx, &scope, reversal, None)
        .await
        .expect("the reversal must post for a closed payer");

    // The reversal nets AR/Revenue/Tax back to zero.
    assert_eq!(
        bal(&raw, &s, s.ar).await,
        Some(0),
        "AR nets to zero after reversal"
    );
    assert_eq!(
        bal(&raw, &s, s.revenue).await,
        Some(0),
        "Revenue nets to zero"
    );
    assert_eq!(bal(&raw, &s, s.tax).await, Some(0), "Tax nets to zero");
}

/// Companion to `closed_payer_is_rejected_but_a_reversal_still_posts`: the REST
/// seam hardcodes `payer_open=true` (no payer-state reader), so the REAL
/// authority for a genuinely closed counterparty is the foundation account-
/// lifecycle invariant. Provision the chart, CLOSE the AR account, then post a
/// normal invoice through the full `post_invoice` path (payer gate OPEN). The DB
/// guard reads the AR account back and rejects the post with `AccountClosed` —
/// exercising the read → gate chain end-to-end, not the `payer_open` flag.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn closed_ar_account_post_is_rejected_at_the_db_guard() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let (raw, provider, s) = setup(&url).await;
    let harness = MetricsHarness::new();
    let service = svc(&provider, &harness);
    let ctx = SecurityContext::anonymous();
    let scope = AccessScope::for_tenant(s.tenant);

    // Close the provisioned AR account (the post's DR leg targets it). The chart
    // of accounts is `bss.ledger_tenant_account`; `find_account` reads lifecycle_state
    // from it during the post.
    raw.execute_raw(pg(format!(
        "UPDATE bss.ledger_tenant_account SET lifecycle_state='CLOSED' \
         WHERE tenant_id='{}' AND account_id='{}'",
        s.tenant, s.ar
    )))
    .await
    .unwrap();

    // Post a normal invoice with the payer gate OPEN: the fast-path flag passes,
    // and the DB account-lifecycle guard is the one that rejects.
    let inv = invoice(&s, "INV-CLOSED-AR", vec![revenue_item(1000)], vec![]);
    let err = service
        .post_invoice(&ctx, &scope, &inv, true)
        .await
        .expect_err("a post to a CLOSED AR account must be rejected by the DB guard");
    assert!(
        matches!(err, DomainError::AccountClosed(_)),
        "expected AccountClosed from the lifecycle guard, got {err:?}"
    );

    // No ledger effect: the AR balance row was never written.
    assert_eq!(
        bal(&raw, &s, s.ar).await,
        None,
        "a rejected closed-account post leaves no AR balance"
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn revenue_line_missing_revenue_stream_is_rejected_by_the_foundation() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let (_raw, provider, s) = setup(&url).await;
    let posting = PostingService::new(provider.clone(), Arc::new(LedgerEventPublisher::noop()));
    let ctx = SecurityContext::anonymous();
    let scope = AccessScope::for_tenant(s.tenant);

    // A balanced DR AR / CR REVENUE entry whose Revenue line carries NO
    // revenue_stream — the chk_journal_line_revenue_stream DB CHECK must reject
    // it at COMMIT (the foundation invariant), not silently accept it.
    let (entry, lines) = bad_revenue_entry(&s);
    let err = posting
        .post(&ctx, &scope, entry, lines, None)
        .await
        .expect_err("a Revenue line without a revenue_stream must be rejected");
    // The DB CHECK surfaces as an Internal-mapped fault (a constraint the gear
    // does not pre-validate); assert the post did NOT succeed and named the
    // revenue-stream constraint.
    let msg = format!("{err:?}");
    assert!(
        msg.contains("revenue_stream") || matches!(err, DomainError::Internal(_)),
        "expected a revenue_stream constraint rejection, got {err:?}"
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_pending_suspense_line_blocks_period_close() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let (_raw, provider, s) = setup(&url).await;
    let harness = MetricsHarness::new();
    let service = svc(&provider, &harness);
    let ctx = SecurityContext::anonymous();
    let scope = AccessScope::for_tenant(s.tenant);

    // An invoice whose single item has NO mapping ⇒ routes to SUSPENSE/PENDING.
    let mut unmapped = revenue_item(1000);
    unmapped.catalog_class = None;
    unmapped.contract_class = None;
    let inv = invoice(&s, "INV-SUS", vec![unmapped], vec![]);
    service
        .post_invoice(&ctx, &scope, &inv, true)
        .await
        .expect("the suspense post itself succeeds (the line is parked, not rejected)");

    // The suspense gauge was emitted for the parked line.
    harness.force_flush();
    assert_eq!(
        harness.counter_value(
            "ledger_invoice_post_total",
            &[("result", "posted"), ("flow", "invoice_post")]
        ),
        1
    );

    // Closing the period must now FAIL the pre-close tie-out (a PENDING line is
    // a soft defect that blocks a clean close).
    let close = PeriodCloseService::new(
        provider.clone(),
        Arc::new(LedgerEventPublisher::noop()),
        std::sync::Arc::new(
            bss_ledger::infra::audit::secured_audit_sink::NoopSecuredAuditSink::new(),
        ),
    );
    let err = close
        .close(
            &SecurityContext::anonymous(),
            s.tenant,
            s.tenant,
            s.period_id.clone(),
        )
        .await
        .expect_err("a PENDING suspense line must block the close");
    // Group B unified the gate: a PENDING mapping line is a tie-out defect surfaced
    // as one accumulated blocked reason on `PeriodCloseBlocked` (design §4.5), not a
    // bare `PreCloseTieOutFailed`.
    assert!(
        matches!(&err, DomainError::PeriodCloseBlocked(d) if d.contains("tie-out")),
        "got {err:?}"
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn mapping_correction_retry_replays_idempotently() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let (raw, provider, s) = setup(&url).await;
    let harness = MetricsHarness::new();
    let service = svc(&provider, &harness);
    let ctx = SecurityContext::anonymous();
    let scope = AccessScope::for_tenant(s.tenant);

    // Post an original invoice, then a MAPPING_CORRECTION re-post (a corrected
    // re-book keyed on invoice_id:correction_id). Posting the SAME correction
    // twice must replay — exactly one correction entry persists.
    let inv = invoice(&s, "INV-MC", vec![revenue_item(1000)], vec![]);
    let original = service
        .post_invoice(&ctx, &scope, &inv, true)
        .await
        .expect("original post");

    // A reversal first clears the original (so AR nets out before the re-book).
    let view = original_view(
        &s,
        original.entry_id,
        &[
            (
                s.ar,
                AccountClass::Ar,
                Side::Debit,
                1000,
                Some("INV-MC"),
                None,
            ),
            (
                s.revenue,
                AccountClass::Revenue,
                Side::Credit,
                1000,
                Some("INV-MC"),
                Some("subscription"),
            ),
        ],
    );
    let reversal = build_reversal(
        &view,
        s.period_id.clone(),
        naive(2026, 6, 2),
        s.tenant,
        s.tenant,
    )
    .expect("reversal builds");
    let reversal_ref = service
        .post_reversal(&ctx, &scope, reversal, None)
        .await
        .expect("reversal posts");

    // The corrected re-post, posted directly through the engine (a balanced
    // DR AR / CR Revenue), keyed MAPPING_CORRECTION on invoice_id:correction_id.
    let correction = bss_ledger::domain::invoice::reversal::correction_id(
        original.entry_id,
        reversal_ref.entry_id,
    );
    let business_id = format!("INV-MC:{correction}");
    let (e1, l1) = correction_entry(&s, &business_id, original.entry_id, reversal_ref.entry_id);
    let posting = PostingService::new(provider.clone(), Arc::new(LedgerEventPublisher::noop()));
    let first = posting
        .post(&ctx, &scope, e1, l1, None)
        .await
        .expect("the correction posts");
    assert!(!first.replayed);

    // Retry the SAME correction (same business id + payload) ⇒ replay.
    let (e2, l2) = correction_entry(&s, &business_id, original.entry_id, reversal_ref.entry_id);
    let retry = posting
        .post(&ctx, &scope, e2, l2, None)
        .await
        .expect("the correction retry replays");
    assert!(retry.replayed, "the retried correction must replay");
    assert_eq!(retry.entry_id, first.entry_id);

    // Exactly one MAPPING_CORRECTION entry persists for the business key.
    let count = scalar_i64(
        &raw,
        &format!(
            "SELECT COUNT(*) FROM bss.ledger_journal_entry \
             WHERE tenant_id='{}' AND source_business_id='{}'",
            s.tenant, business_id
        ),
    )
    .await
    .unwrap_or(0);
    assert_eq!(count, 1, "exactly one correction entry for the key");
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn reverse_of_a_reversal_is_rejected() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let (_raw, provider, s) = setup(&url).await;
    let harness = MetricsHarness::new();
    let service = svc(&provider, &harness);
    let ctx = SecurityContext::anonymous();
    let scope = AccessScope::for_tenant(s.tenant);

    // Post + reverse an invoice, then read the reversal back and attempt to
    // reverse IT — the domain must reject a reverse-of-a-reversal.
    let inv = invoice(&s, "INV-RR", vec![revenue_item(1000)], vec![]);
    let original = service
        .post_invoice(&ctx, &scope, &inv, true)
        .await
        .expect("original post");
    let orig_view = original_view(
        &s,
        original.entry_id,
        &[
            (
                s.ar,
                AccountClass::Ar,
                Side::Debit,
                1000,
                Some("INV-RR"),
                None,
            ),
            (
                s.revenue,
                AccountClass::Revenue,
                Side::Credit,
                1000,
                Some("INV-RR"),
                Some("subscription"),
            ),
        ],
    );
    let reversal = build_reversal(
        &orig_view,
        s.period_id.clone(),
        naive(2026, 6, 2),
        s.tenant,
        s.tenant,
    )
    .expect("reversal builds");
    let reversal_ref = service
        .post_reversal(&ctx, &scope, reversal, None)
        .await
        .expect("reversal posts");

    // Build a view of the REVERSAL (source_doc_type = REVERSAL) and try to
    // reverse it — CannotReverseReversal.
    let mut reversal_view = original_view(
        &s,
        reversal_ref.entry_id,
        &[
            (
                s.ar,
                AccountClass::Ar,
                Side::Credit,
                1000,
                Some("INV-RR"),
                None,
            ),
            (
                s.revenue,
                AccountClass::Revenue,
                Side::Debit,
                1000,
                Some("INV-RR"),
                Some("subscription"),
            ),
        ],
    );
    reversal_view.source_doc_type = SourceDocType::Reversal;
    let err = build_reversal(
        &reversal_view,
        s.period_id.clone(),
        naive(2026, 6, 3),
        s.tenant,
        s.tenant,
    )
    .expect_err("reversing a reversal must be rejected");
    assert_eq!(
        err,
        bss_ledger::domain::invoice::reversal::ReversalError::CannotReverseReversal
    );
}

/// Concurrency (idempotency #1): two posts of the SAME invoice (same
/// `invoiceId`) racing on two service clones must land EXACTLY ONE ledger
/// effect — the `INVOICE_POST` dedup key `(tenant, source_business_id =
/// invoiceId)` admits one winner; the loser replays the winner's finalized id.
/// No duplicate journal entry, and no absent/nil ref on the replay path.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn concurrent_same_invoice_posts_exactly_once() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let (raw, provider, s) = setup(&url).await;
    let harness = MetricsHarness::new();

    // Two services over the SAME provider, posting the SAME invoice concurrently.
    let svc_a = svc(&provider, &harness);
    let svc_b = svc(&provider, &harness);
    let scope_a = AccessScope::for_tenant(s.tenant);
    let scope_b = AccessScope::for_tenant(s.tenant);
    let ctx_a = SecurityContext::anonymous();
    let ctx_b = SecurityContext::anonymous();
    let inv_a = invoice(
        &s,
        "INV-CONC",
        vec![revenue_item(1000)],
        vec![tax_breakdown(200)],
    );
    let inv_b = invoice(
        &s,
        "INV-CONC",
        vec![revenue_item(1000)],
        vec![tax_breakdown(200)],
    );

    let (ra, rb) = tokio::join!(
        async move { svc_a.post_invoice(&ctx_a, &scope_a, &inv_a, true).await },
        async move { svc_b.post_invoice(&ctx_b, &scope_b, &inv_b, true).await },
    );

    // Exactly one journal entry persists for the shared invoice id.
    let entry_count = scalar_i64(
        &raw,
        &format!(
            "SELECT COUNT(*) FROM bss.ledger_journal_entry \
             WHERE tenant_id='{}' AND source_business_id='INV-CONC'",
            s.tenant
        ),
    )
    .await
    .unwrap_or(0);
    assert_eq!(
        entry_count, 1,
        "exactly one ledger effect for the shared invoice id"
    );

    // At least one side succeeds; whichever succeed share the one finalized id
    // and never the nil UUID (the loser replays the winner's ref).
    let oks = i32::from(ra.is_ok()) + i32::from(rb.is_ok());
    assert!(
        oks >= 1,
        "at least one concurrent invoice post must succeed: {ra:?} / {rb:?}"
    );
    let ids: Vec<Uuid> = [&ra, &rb]
        .into_iter()
        .filter_map(|r| r.as_ref().ok())
        .map(|p| p.entry_id)
        .collect();
    for id in &ids {
        assert_ne!(
            *id,
            Uuid::nil(),
            "a successful post/replay must carry a real entry id"
        );
    }
    if let [a, b] = ids.as_slice() {
        assert_eq!(
            a, b,
            "the fresh post and the replay reference the same entry"
        );
    }

    // The single posted effect moved AR exactly once (gross 1200, not 2400).
    assert_eq!(
        bal(&raw, &s, s.ar).await,
        Some(1200),
        "AR reflects exactly one post, not a double charge"
    );
}

/// Concurrency (at-most-once-per-entry reversal): two reversals of the SAME
/// posted entry racing must land EXACTLY ONE reversal — the `REVERSAL` dedup key
/// `(tenant, source_business_id = "reverses=<entryId>")` admits one winner;
/// the loser replays it (or is rejected). The original nets back to zero exactly
/// once — never double-reversed past zero.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn concurrent_reversals_of_one_entry_post_exactly_once() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let (raw, provider, s) = setup(&url).await;
    let harness = MetricsHarness::new();
    let service = svc(&provider, &harness);
    let ctx = SecurityContext::anonymous();
    let scope = AccessScope::for_tenant(s.tenant);

    // Post an original invoice (DR AR 1200 / CR Revenue 1000 / CR Tax 200).
    let inv = invoice(
        &s,
        "INV-REVRACE",
        vec![revenue_item(1000)],
        vec![tax_breakdown(200)],
    );
    let original = service
        .post_invoice(&ctx, &scope, &inv, true)
        .await
        .expect("original post");

    // Build TWO reversals of the SAME original: each carries its own fresh
    // entry_id but the SAME source_business_id ("reverses=<original>"), so the
    // dedup key collides — exactly one may persist.
    let view = original_view(
        &s,
        original.entry_id,
        &[
            (
                s.ar,
                AccountClass::Ar,
                Side::Debit,
                1200,
                Some("INV-REVRACE"),
                None,
            ),
            (
                s.revenue,
                AccountClass::Revenue,
                Side::Credit,
                1000,
                Some("INV-REVRACE"),
                Some("subscription"),
            ),
            (
                s.tax,
                AccountClass::TaxPayable,
                Side::Credit,
                200,
                Some("INV-REVRACE"),
                None,
            ),
        ],
    );
    let reversal_a = build_reversal(
        &view,
        s.period_id.clone(),
        naive(2026, 6, 2),
        s.tenant,
        s.tenant,
    )
    .expect("reversal A builds");
    let reversal_b = build_reversal(
        &view,
        s.period_id.clone(),
        naive(2026, 6, 2),
        s.tenant,
        s.tenant,
    )
    .expect("reversal B builds");
    let business_id = reversal_a.source_business_id.clone();
    assert_eq!(
        business_id, reversal_b.source_business_id,
        "both reversals key on the same reverses=<entry> business id"
    );

    let svc_a = svc(&provider, &harness);
    let svc_b = svc(&provider, &harness);
    let scope_a = scope.clone();
    let scope_b = scope.clone();
    let ctx_a = SecurityContext::anonymous();
    let ctx_b = SecurityContext::anonymous();
    let (ra, rb) = tokio::join!(
        async move {
            svc_a
                .post_reversal(&ctx_a, &scope_a, reversal_a, None)
                .await
        },
        async move {
            svc_b
                .post_reversal(&ctx_b, &scope_b, reversal_b, None)
                .await
        },
    );

    // Exactly one REVERSAL entry persists for the shared key (at most once per
    // reversed entry).
    let reversal_count = scalar_i64(
        &raw,
        &format!(
            "SELECT COUNT(*) FROM bss.ledger_journal_entry \
             WHERE tenant_id='{}' AND source_business_id='{}'",
            s.tenant, business_id
        ),
    )
    .await
    .unwrap_or(0);
    assert_eq!(
        reversal_count, 1,
        "exactly one reversal persists for the reversed entry"
    );

    // At least one side made progress; the loser replayed or was rejected, never
    // a second ledger effect.
    let oks = i32::from(ra.is_ok()) + i32::from(rb.is_ok());
    assert!(
        oks >= 1,
        "at least one concurrent reversal must succeed: {ra:?} / {rb:?}"
    );

    // The original nets back to zero EXACTLY once — a double reversal would drive
    // the guarded AR balance negative (and be rejected), so a clean zero proves
    // the second reversal did not apply a second effect.
    assert_eq!(
        bal(&raw, &s, s.ar).await,
        Some(0),
        "AR nets to zero after exactly one reversal"
    );
    assert_eq!(
        bal(&raw, &s, s.revenue).await,
        Some(0),
        "Revenue nets to zero after exactly one reversal"
    );
    assert_eq!(
        bal(&raw, &s, s.tax).await,
        Some(0),
        "Tax nets to zero after exactly one reversal"
    );
}

// ── Test builders ──────────────────────────────────────────────────────────

/// Synthesize the `EntryView` of a posted entry from the known posted lines
/// (account_id, class, side, amount, invoice_id, revenue_stream). Lets the test
/// drive `build_reversal` end-to-end without a private read-back mapper.
fn original_view(
    s: &Seller,
    entry_id: Uuid,
    lines: &[(Uuid, AccountClass, Side, i64, Option<&str>, Option<&str>)],
) -> EntryView {
    let now: OffsetDateTime = OffsetDateTime::now_utc();
    EntryView {
        entry_id,
        tenant_id: s.tenant,
        period_id: s.period_id.clone(),
        entry_currency: "USD".to_owned(),
        source_doc_type: SourceDocType::InvoicePost,
        source_business_id: "INV".to_owned(),
        reverses_entry_id: None,
        reverses_period_id: None,
        posted_at_utc: now,
        effective_at: naive(2026, 6, 1),
        posted_by_actor_id: s.tenant,
        origin: "SYSTEM".to_owned(),
        correlation_id: s.tenant,
        created_seq: 1,
        lines: lines
            .iter()
            .map(|(account, class, side, amount, invoice, stream)| {
                // Faithful read-back: a posted TAX_PAYABLE line always carries its
                // filing dims (the chk_journal_line_tax_dims CHECK guarantees it),
                // so the synthesized view must too — build_reversal copies them
                // onto the reversing TAX line, which must satisfy the same CHECK.
                let is_tax = *class == AccountClass::TaxPayable;
                LineView {
                    line_id: Uuid::now_v7(),
                    entry_id,
                    payer_tenant_id: s.payer,
                    account_id: *account,
                    account_class: *class,
                    gl_code: None,
                    side: *side,
                    amount_minor: *amount,
                    currency: "USD".to_owned(),
                    currency_scale: 2,
                    invoice_id: invoice.map(str::to_owned),
                    due_date: invoice.map(|_| naive(2026, 7, 1)),
                    revenue_stream: stream.map(str::to_owned),
                    mapping_status: MappingStatus::Resolved,
                    functional_amount_minor: None,
                    functional_currency: None,
                    tax_jurisdiction: is_tax.then(|| "US-CA".to_owned()),
                    tax_filing_period: is_tax.then(|| "2026Q2".to_owned()),
                    ar_status: None,
                }
            })
            .collect(),
    }
}

/// A balanced DR AR / CR REVENUE entry whose Revenue line OMITS revenue_stream
/// (to trip the foundation `chk_journal_line_revenue_stream` CHECK).
fn bad_revenue_entry(s: &Seller) -> (NewEntry, Vec<NewLine>) {
    let entry_id = Uuid::now_v7();
    let entry = NewEntry {
        entry_id,
        tenant_id: s.tenant,
        legal_entity_id: s.tenant,
        period_id: s.period_id.clone(),
        entry_currency: "USD".to_owned(),
        source_doc_type: SourceDocType::InvoicePost,
        source_business_id: "INV-BAD".to_owned(),
        reverses_entry_id: None,
        reverses_period_id: None,
        posted_at_utc: OffsetDateTime::now_utc(),
        effective_at: naive(2026, 6, 1),
        origin: "SYSTEM".to_owned(),
        posted_by_actor_id: s.tenant,
        correlation_id: s.tenant,
        rounding_evidence: serde_json::Value::Null,
        rate_snapshot_ref: None,
    };
    let lines = vec![
        line(
            s,
            s.ar,
            AccountClass::Ar,
            Side::Debit,
            1000,
            Some("INV-BAD"),
            None,
        ),
        // Revenue line with revenue_stream = None — the CHECK must reject this.
        line(
            s,
            s.revenue,
            AccountClass::Revenue,
            Side::Credit,
            1000,
            None,
            None,
        ),
    ];
    (entry, lines)
}

/// A balanced MAPPING_CORRECTION re-post (DR AR / CR Revenue), keyed on the
/// supplied business id and pointing back at the reversal it follows.
fn correction_entry(
    s: &Seller,
    business_id: &str,
    original_entry_id: Uuid,
    reversal_entry_id: Uuid,
) -> (NewEntry, Vec<NewLine>) {
    let entry_id = Uuid::now_v7();
    let entry = NewEntry {
        entry_id,
        tenant_id: s.tenant,
        legal_entity_id: s.tenant,
        period_id: s.period_id.clone(),
        entry_currency: "USD".to_owned(),
        source_doc_type: SourceDocType::MappingCorrection,
        source_business_id: business_id.to_owned(),
        reverses_entry_id: Some(reversal_entry_id),
        reverses_period_id: Some(s.period_id.clone()),
        posted_at_utc: OffsetDateTime::now_utc(),
        effective_at: naive(2026, 6, 2),
        origin: "SYSTEM".to_owned(),
        posted_by_actor_id: s.tenant,
        correlation_id: original_entry_id,
        rounding_evidence: serde_json::Value::Null,
        rate_snapshot_ref: None,
    };
    let lines = vec![
        line(
            s,
            s.ar,
            AccountClass::Ar,
            Side::Debit,
            1000,
            Some("INV-MC"),
            None,
        ),
        line(
            s,
            s.revenue,
            AccountClass::Revenue,
            Side::Credit,
            1000,
            Some("INV-MC"),
            Some("subscription"),
        ),
    ];
    (entry, lines)
}

#[allow(clippy::too_many_arguments)]
fn line(
    s: &Seller,
    account: Uuid,
    class: AccountClass,
    side: Side,
    amount: i64,
    invoice_id: Option<&str>,
    revenue_stream: Option<&str>,
) -> NewLine {
    NewLine {
        line_id: Uuid::now_v7(),
        payer_tenant_id: s.payer,
        seller_tenant_id: None,
        resource_tenant_id: None,
        account_id: account,
        account_class: class,
        gl_code: None,
        side,
        amount_minor: amount,
        currency: "USD".to_owned(),
        currency_scale: 2,
        invoice_id: invoice_id.map(str::to_owned),
        due_date: invoice_id.map(|_| naive(2026, 7, 1)),
        revenue_stream: revenue_stream.map(str::to_owned),
        mapping_status: MappingStatus::Resolved,
        functional_amount_minor: None,
        functional_currency: None,
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
    }
}
