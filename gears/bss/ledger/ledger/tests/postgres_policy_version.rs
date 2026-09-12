//! Postgres-only integration: the §4.6 (AC #15) `PolicyVersionGuard` — a
//! correction must REUSE the original posting's pinned evidence refs.
//!
//! Drives the REAL foundation engine (`PostingService` + `InvoicePostService`):
//! 1. posts an original entry carrying a `pricing_snapshot_ref` on a line;
//! 2. posts a REVERSAL of it via the real `build_reversal` + `post_reversal`
//!    flow — it succeeds, because the reversal's lines carry no pinned refs (all
//!    all-NULL tuples, which the guard ignores), so reuse is structural;
//! 3. crafts a correction whose `reverses_entry_id` is the original but whose
//!    line carries a DIFFERENT `pricing_snapshot_ref` — the guard rejects it
//!    with `PolicyVersionViolation`.
//!
//! Mirrors `postgres_chain.rs` / `postgres_invoice_post.rs`. Ignored by default;
//! run with `cargo test -p cf-gears-bss-ledger -- --ignored`.

#![allow(
    clippy::non_ascii_literal,
    clippy::let_underscore_must_use,
    clippy::needless_collect,
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::doc_markdown,
    clippy::panic,
    clippy::too_many_lines,
    clippy::similar_names
)]

use std::sync::Arc;

use bss_ledger::domain::error::DomainError;
use bss_ledger::domain::invoice::reversal::build_reversal;
use bss_ledger::domain::model::{AccountRow, CurrencyScaleRow, NewEntry, NewLine};
use bss_ledger::domain::money::DEFAULT_PLAUSIBLE_MAX_MAJOR;
use bss_ledger::infra::events::publisher::LedgerEventPublisher;
use bss_ledger::infra::invoice_post::InvoicePostService;
use bss_ledger::infra::metrics::test_harness::MetricsHarness;
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

fn naive(y: i32, m: u32, d: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, m, d).unwrap()
}

/// Provisioned seller ids (AR + CASH chart, one OPEN period, USD@2).
struct Fixture {
    tenant: Uuid,
    payer: Uuid,
    ar: Uuid,
    cash: Uuid,
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

/// Boot, migrate, seed USD@2 + an OPEN period + AR/CASH accounts. Returns the
/// migrate connection, the search_path-scoped provider, and the fixture ids.
async fn setup(
    url: &str,
) -> (
    DatabaseConnection,
    PostingService,
    InvoicePostService,
    Fixture,
) {
    let raw = Database::connect(url).await.unwrap();
    Migrator::up(&raw, None).await.unwrap();
    let repo_url = format!("{url}?options=-c%20search_path%3Dbss,public");
    let tdb = connect_db(&repo_url, ConnectOpts::default()).await.unwrap();
    let provider = DBProvider::<DbError>::new(tdb);

    let f = Fixture {
        tenant: Uuid::now_v7(),
        payer: Uuid::now_v7(),
        ar: Uuid::now_v7(),
        cash: Uuid::now_v7(),
        period_id: "202606".to_owned(),
    };

    let reference = ReferenceRepo::new(provider.clone());
    reference
        .upsert_currency_scale(CurrencyScaleRow {
            tenant_id: f.tenant,
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
        f.tenant, f.tenant, f.period_id
    )))
    .await
    .unwrap();
    reference
        .insert_account(account(f.tenant, f.ar, AccountClass::Ar, Side::Debit))
        .await
        .unwrap();
    reference
        .insert_account(account(
            f.tenant,
            f.cash,
            AccountClass::CashClearing,
            Side::Credit,
        ))
        .await
        .unwrap();

    let posting = PostingService::new(provider.clone(), Arc::new(LedgerEventPublisher::noop()));
    let harness = MetricsHarness::new();
    let invoice = InvoicePostService::new(
        provider.clone(),
        Arc::new(LedgerEventPublisher::noop()),
        Arc::new(harness.metrics()),
        bss_ledger::config::RecognitionConfig::default(),
        bss_ledger::config::FxConfig::default(),
    );
    (raw, posting, invoice, f)
}

/// One DR AR / CR CASH line pair for `amount`; the AR line carries
/// `pricing_snapshot_ref = snapshot` (the original's pinned evidence). The
/// `reverses_*` header is supplied so the same builder serves the original
/// (None) and the crafted correction (Some).
#[allow(clippy::too_many_arguments)]
fn entry(
    f: &Fixture,
    entry_id: Uuid,
    business_id: &str,
    doc_type: SourceDocType,
    reverses_entry_id: Option<Uuid>,
    snapshot: Option<&str>,
) -> (NewEntry, Vec<NewLine>) {
    let header = NewEntry {
        entry_id,
        tenant_id: f.tenant,
        legal_entity_id: f.tenant,
        period_id: f.period_id.clone(),
        entry_currency: "USD".to_owned(),
        source_doc_type: doc_type,
        source_business_id: business_id.to_owned(),
        reverses_entry_id,
        reverses_period_id: reverses_entry_id.map(|_| f.period_id.clone()),
        posted_at_utc: OffsetDateTime::now_utc(),
        effective_at: naive(2026, 6, 1),
        origin: "SYSTEM".to_owned(),
        posted_by_actor_id: f.tenant,
        correlation_id: f.tenant,
        rounding_evidence: serde_json::Value::Null,
        rate_snapshot_ref: None,
    };
    let lines = vec![
        line(f, f.ar, AccountClass::Ar, Side::Debit, 1000, snapshot),
        line(
            f,
            f.cash,
            AccountClass::CashClearing,
            Side::Credit,
            1000,
            None,
        ),
    ];
    (header, lines)
}

fn line(
    f: &Fixture,
    account: Uuid,
    class: AccountClass,
    side: Side,
    amount: i64,
    pricing_snapshot_ref: Option<&str>,
) -> NewLine {
    NewLine {
        line_id: Uuid::now_v7(),
        ar_status: None,
        payer_tenant_id: f.payer,
        seller_tenant_id: None,
        resource_tenant_id: None,
        account_id: account,
        account_class: class,
        gl_code: None,
        side,
        amount_minor: amount,
        currency: "USD".to_owned(),
        currency_scale: 2,
        invoice_id: None,
        due_date: None,
        revenue_stream: None,
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
        pricing_snapshot_ref: pricing_snapshot_ref.map(str::to_owned),
        po_allocation_group: None,
        credit_grant_event_type: None,
    }
}

/// Synthesize the `EntryView` of the posted original so `build_reversal` can run
/// end-to-end without a private read-back mapper (mirrors
/// `postgres_invoice_post.rs::original_view`). The read-back `LineView` does NOT
/// carry pinned evidence-ref columns, so the reversal it builds has no pinned
/// refs — exactly the production behaviour the guard tolerates.
fn original_view(f: &Fixture, entry_id: Uuid) -> EntryView {
    let now: OffsetDateTime = OffsetDateTime::now_utc();
    EntryView {
        entry_id,
        tenant_id: f.tenant,
        period_id: f.period_id.clone(),
        entry_currency: "USD".to_owned(),
        source_doc_type: SourceDocType::ManualAdjustment,
        source_business_id: "orig".to_owned(),
        reverses_entry_id: None,
        reverses_period_id: None,
        posted_at_utc: now,
        effective_at: naive(2026, 6, 1),
        posted_by_actor_id: f.tenant,
        origin: "SYSTEM".to_owned(),
        correlation_id: f.tenant,
        created_seq: 1,
        lines: vec![
            view_line(f, entry_id, f.ar, AccountClass::Ar, Side::Debit, 1000),
            view_line(
                f,
                entry_id,
                f.cash,
                AccountClass::CashClearing,
                Side::Credit,
                1000,
            ),
        ],
    }
}

fn view_line(
    f: &Fixture,
    entry_id: Uuid,
    account: Uuid,
    class: AccountClass,
    side: Side,
    amount: i64,
) -> LineView {
    LineView {
        line_id: Uuid::now_v7(),
        ar_status: None,
        entry_id,
        payer_tenant_id: f.payer,
        account_id: account,
        account_class: class,
        gl_code: None,
        side,
        amount_minor: amount,
        currency: "USD".to_owned(),
        currency_scale: 2,
        invoice_id: None,
        due_date: None,
        revenue_stream: None,
        mapping_status: MappingStatus::Resolved,
        functional_amount_minor: None,
        functional_currency: None,
        tax_jurisdiction: None,
        tax_filing_period: None,
    }
}

/// The full AC #15 flow end-to-end: original (pinned ref) → real reversal
/// (passes, no pinned refs) → crafted correction with a DIFFERENT pinned ref
/// (rejected `PolicyVersionViolation`).
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn correction_must_reuse_original_evidence_refs() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let (_raw, posting, invoice, f) = setup(&url).await;
    let scope = AccessScope::for_tenant(f.tenant);
    let ctx = SecurityContext::anonymous();

    // 1. Post the ORIGINAL — its AR line carries a pinned `pricing_snapshot_ref`.
    let original_id = Uuid::now_v7();
    let (e0, l0) = entry(
        &f,
        original_id,
        "orig",
        SourceDocType::ManualAdjustment,
        None,
        Some("snap-ORIGINAL"),
    );
    posting
        .post(&ctx, &scope, e0, l0, None)
        .await
        .expect("original post must succeed");

    // 2. Post a REVERSAL via the REAL reversal flow. `build_reversal` copies the
    //    original's read-back lines (which carry NO pinned refs), so the reversal
    //    has all-NULL evidence tuples — the guard ignores them and the reversal
    //    posts. Reuse is structural here; the guard never fires on the real flow.
    let view = original_view(&f, original_id);
    let reversal = build_reversal(
        &view,
        f.period_id.clone(),
        naive(2026, 6, 1),
        f.tenant,
        f.tenant,
    )
    .expect("reversal must build");
    invoice
        .post_reversal(&ctx, &scope, reversal, None)
        .await
        .expect("the reversal must post (it reuses no pinned refs)");

    // 3. Craft a buggy correction: it points back at the original
    //    (`reverses_entry_id`) but its AR line carries a DIFFERENT pinned
    //    `pricing_snapshot_ref` the original never had. The guard must reject it.
    let (e_bad, l_bad) = entry(
        &f,
        Uuid::now_v7(),
        "correction-bad",
        SourceDocType::MappingCorrection,
        Some(original_id),
        Some("snap-DIFFERENT"),
    );
    let err = posting
        .post(&ctx, &scope, e_bad, l_bad, None)
        .await
        .expect_err("a correction inventing a new pinned ref must be rejected");
    assert!(
        matches!(err, DomainError::PolicyVersionViolation(_)),
        "expected PolicyVersionViolation, got: {err:?}"
    );

    // A SECOND original (un-reversed): uq_journal_entry_reversal allows only ONE
    // entry to reverse a given original, and step 2 already reversed the first —
    // so the good correction below needs a fresh target to actually post.
    let original2_id = Uuid::now_v7();
    let (e2, l2) = entry(
        &f,
        original2_id,
        "orig-2",
        SourceDocType::ManualAdjustment,
        None,
        Some("snap-ORIGINAL"),
    );
    posting
        .post(&ctx, &scope, e2, l2, None)
        .await
        .expect("second original post must succeed");

    // 4. A correction that REUSES the original's pinned ref passes the guard.
    //    (It points back at the un-reversed second original; the AR line carries
    //    the SAME `snap-ORIGINAL`.)
    let (e_ok, l_ok) = entry(
        &f,
        Uuid::now_v7(),
        "correction-ok",
        SourceDocType::MappingCorrection,
        Some(original2_id),
        Some("snap-ORIGINAL"),
    );
    posting
        .post(&ctx, &scope, e_ok, l_ok, None)
        .await
        .expect("a correction reusing the original's pinned ref must post");
}
