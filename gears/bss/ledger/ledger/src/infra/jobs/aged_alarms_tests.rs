//! Tests for `AgedAlarmJob` — the aged-work `Warn` alarms (Group C).
//!
//! The load-bearing coverage is the pure [`aged_grains`] detector (the resolved
//! G-P5a age proxy: oldest contributing `UNALLOCATED` line's post time, gated on
//! a positive cached balance) plus the queue-age filter — both exercised in
//! plain unit tests. Docker-gated integration tests boot Postgres, seed an aged
//! `QUEUED` row + an aged parked unallocated grain, and assert the cross-tenant
//! scans find them and `run()` completes.
//!
//! Ignored Docker tests run with
//! `cargo test -p cf-gears-bss-ledger --lib 'infra::jobs::aged_alarms::tests' -- --ignored`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::doc_markdown,
    clippy::similar_names,
    clippy::inconsistent_struct_constructor
)]

use std::sync::Arc;

use chrono::NaiveDate;
use sea_orm::{ConnectionTrait, Database, DatabaseConnection, Statement, TransactionTrait};
use sea_orm_migration::MigratorTrait;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use time::Duration;
use toolkit_db::{ConnectOpts, DBProvider, DbError, connect_db};
use uuid::Uuid;

use super::{
    AgedAlarmJob, AgedUnallocatedGrain, NegativeTaxGrain, aged_grains, aged_refund_clearing_grains,
    is_beyond_filing_window, stage1_orphans,
};
use crate::domain::ports::metrics::NoopLedgerMetrics;
use crate::infra::events::publisher::LedgerEventPublisher;
use crate::infra::storage::entity::{
    account_balance, journal_entry, journal_line, refund, unallocated_balance,
};
use crate::infra::storage::migrations::Migrator;
use time::OffsetDateTime;

// ---------------------------------------------------------------------------
// Pure-function fixtures (no DB) — the age-proxy logic.
// ---------------------------------------------------------------------------

const TENANT: u128 = 0xA1;
const PAYER: u128 = 0xB1;
const ACCOUNT: u128 = 0xC1;

fn entry(entry_id: Uuid, posted_at: OffsetDateTime) -> journal_entry::Model {
    journal_entry::Model {
        entry_id,
        tenant_id: Uuid::from_u128(TENANT),
        legal_entity_id: Uuid::from_u128(TENANT),
        period_id: "202606".to_owned(),
        entry_currency: "USD".to_owned(),
        source_doc_type: "PAYMENT_SETTLE".to_owned(),
        source_business_id: "pay-1".to_owned(),
        reverses_entry_id: None,
        reverses_period_id: None,
        posted_at_utc: posted_at,
        effective_at: NaiveDate::from_ymd_opt(2026, 6, 1).unwrap(),
        origin: "SYSTEM".to_owned(),
        posted_by_actor_id: Uuid::from_u128(TENANT),
        correlation_id: Uuid::from_u128(TENANT),
        rounding_evidence: serde_json::Value::Null,
        created_seq: 1,
        row_hash: None,
        prev_hash: None,
        prev_entry_id: None,
        prev_period_id: None,
    }
}

fn unalloc_line(entry_id: Uuid, account: u128) -> journal_line::Model {
    journal_line::Model {
        line_id: Uuid::now_v7(),
        entry_id,
        tenant_id: Uuid::from_u128(TENANT),
        period_id: "202606".to_owned(),
        payer_tenant_id: Uuid::from_u128(PAYER),
        seller_tenant_id: None,
        resource_tenant_id: None,
        account_id: Uuid::from_u128(account),
        account_class: "UNALLOCATED".to_owned(),
        gl_code: None,
        side: "CR".to_owned(),
        amount_minor: 1_000,
        currency: "USD".to_owned(),
        currency_scale: 2,
        invoice_id: None,
        due_date: None,
        revenue_stream: None,
        mapping_status: "RESOLVED".to_owned(),
        functional_amount_minor: None,
        functional_currency: None,
        rate_snapshot_ref: None,
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

fn unalloc_cache(account: u128, balance_minor: i64) -> unallocated_balance::Model {
    unallocated_balance::Model {
        tenant_id: Uuid::from_u128(TENANT),
        payer_tenant_id: Uuid::from_u128(PAYER),
        account_id: Uuid::from_u128(account),
        currency: "USD".to_owned(),
        balance_minor,
        functional_balance_minor: None,
        functional_currency: None,
        last_entry_seq: None,
        version: 0,
    }
}

#[test]
fn aged_grains_flags_old_grain_with_positive_balance() {
    let now = OffsetDateTime::now_utc();
    let cutoff = now - Duration::seconds(86_400);
    // One UNALLOCATED line posted 2 days ago — older than the 1-day cutoff.
    let e = Uuid::now_v7();
    let entries = vec![entry(e, now - Duration::days(2))];
    let lines = vec![unalloc_line(e, ACCOUNT)];
    let cache = vec![unalloc_cache(ACCOUNT, 1_000)];

    let aged = aged_grains(&entries, &lines, &cache, now, cutoff);
    assert_eq!(aged.len(), 1, "an old grain with parked cash is aged");
    assert_eq!(
        aged[0],
        AgedUnallocatedGrain {
            tenant_id: Uuid::from_u128(TENANT),
            payer_tenant_id: Uuid::from_u128(PAYER),
            account_id: Uuid::from_u128(ACCOUNT),
            currency: "USD".to_owned(),
            balance_minor: 1_000,
            age_secs: aged[0].age_secs,
        }
    );
    assert!(
        aged[0].age_secs >= 86_400,
        "age proxy is ~2 days in seconds"
    );
}

#[test]
fn aged_grains_uses_oldest_contributing_line_not_latest() {
    // Two lines for the SAME grain: one fresh (today), one old (3 days). The age
    // proxy is the OLDEST (3 days), NOT the latest — the resolved G-P5a decision
    // (NOT `last_entry_seq`, which would point at the fresh line).
    let now = OffsetDateTime::now_utc();
    let cutoff = now - Duration::seconds(86_400);
    let old = Uuid::now_v7();
    let fresh = Uuid::now_v7();
    let entries = vec![entry(old, now - Duration::days(3)), entry(fresh, now)];
    let lines = vec![unalloc_line(old, ACCOUNT), unalloc_line(fresh, ACCOUNT)];
    let cache = vec![unalloc_cache(ACCOUNT, 1_000)];

    let aged = aged_grains(&entries, &lines, &cache, now, cutoff);
    assert_eq!(aged.len(), 1, "grain is aged via its oldest line");
    assert!(
        aged[0].age_secs >= 3 * 86_400,
        "age must be measured from the OLDEST (3d) line, not the fresh one: {}",
        aged[0].age_secs
    );
}

#[test]
fn aged_grains_skips_fresh_grain() {
    // Oldest line is fresh (1h) — below the 1-day cutoff ⇒ not aged.
    let now = OffsetDateTime::now_utc();
    let cutoff = now - Duration::seconds(86_400);
    let e = Uuid::now_v7();
    let entries = vec![entry(e, now - Duration::hours(1))];
    let lines = vec![unalloc_line(e, ACCOUNT)];
    let cache = vec![unalloc_cache(ACCOUNT, 1_000)];

    assert!(
        aged_grains(&entries, &lines, &cache, now, cutoff).is_empty(),
        "a recently-funded pool is not aged"
    );
}

#[test]
fn aged_grains_skips_zero_balance_grain() {
    // Old line, but the cache balance is 0 (fully allocated) ⇒ not aged (the
    // `balance_minor > 0` gate). A drained pool needs no alarm even if its lines
    // are old.
    let now = OffsetDateTime::now_utc();
    let cutoff = now - Duration::seconds(86_400);
    let e = Uuid::now_v7();
    let entries = vec![entry(e, now - Duration::days(5))];
    let lines = vec![unalloc_line(e, ACCOUNT)];
    let cache = vec![unalloc_cache(ACCOUNT, 0)];

    assert!(
        aged_grains(&entries, &lines, &cache, now, cutoff).is_empty(),
        "a drained (zero-balance) pool is not aged"
    );
}

#[test]
fn aged_grains_keys_per_grain() {
    // Two distinct accounts: one old+parked (aged), one fresh+parked (not). Only
    // the old grain is flagged — confirms the (payer, account, currency) keying.
    let now = OffsetDateTime::now_utc();
    let cutoff = now - Duration::seconds(86_400);
    let e_old = Uuid::now_v7();
    let e_fresh = Uuid::now_v7();
    let other_account = 0xC2;
    let entries = vec![
        entry(e_old, now - Duration::days(2)),
        entry(e_fresh, now - Duration::minutes(5)),
    ];
    let lines = vec![
        unalloc_line(e_old, ACCOUNT),
        unalloc_line(e_fresh, other_account),
    ];
    let cache = vec![
        unalloc_cache(ACCOUNT, 1_000),
        unalloc_cache(other_account, 2_000),
    ];

    let aged = aged_grains(&entries, &lines, &cache, now, cutoff);
    assert_eq!(aged.len(), 1, "only the old grain is aged");
    assert_eq!(aged[0].account_id, Uuid::from_u128(ACCOUNT));
}

#[test]
fn aged_grains_empty_inputs() {
    let now = OffsetDateTime::now_utc();
    let cutoff = now - Duration::seconds(86_400);
    assert!(aged_grains(&[], &[], &[], now, cutoff).is_empty());
}

// ---------------------------------------------------------------------------
// Group F: refund-clearing aging + stage-1-orphan pure detectors (no DB).
// ---------------------------------------------------------------------------

const WARN_SECS: i64 = 7 * 24 * 60 * 60;
const PAGE_SECS: i64 = 14 * 24 * 60 * 60;

fn clearing_line(entry_id: Uuid, account: u128) -> journal_line::Model {
    journal_line::Model {
        account_class: "REFUND_CLEARING".to_owned(),
        ..unalloc_line(entry_id, account)
    }
}

fn clearing_cache(account: u128, balance_minor: i64) -> account_balance::Model {
    account_balance::Model {
        tenant_id: Uuid::from_u128(TENANT),
        account_id: Uuid::from_u128(account),
        currency: "USD".to_owned(),
        account_class: "REFUND_CLEARING".to_owned(),
        normal_side: "CR".to_owned(),
        balance_minor,
        functional_balance_minor: None,
        functional_currency: None,
        last_entry_seq: None,
        version: 0,
    }
}

fn refund_row(psp: &str, phase: &str, created_at: OffsetDateTime) -> refund::Model {
    refund::Model {
        tenant_id: Uuid::from_u128(TENANT),
        refund_id: format!("rf-{psp}-{phase}"),
        psp_refund_id: psp.to_owned(),
        phase: phase.to_owned(),
        pattern: "A_UNALLOCATED".to_owned(),
        payment_id: "pay-1".to_owned(),
        invoice_id: None,
        currency: "USD".to_owned(),
        amount_minor: 500,
        clearing_state: "PENDING".to_owned(),
        relates_to_refund_id: None,
        reverses_entry_id: None,
        created_at_utc: created_at,
        version: 0,
    }
}

#[test]
fn refund_clearing_aged_flags_open_grain_past_7d_warn() {
    let now = OffsetDateTime::now_utc();
    let warn = now - Duration::seconds(WARN_SECS);
    let page = now - Duration::seconds(PAGE_SECS);
    // Clearing line posted 8 days ago — past the 7d Warn, under the 14d Page.
    let e = Uuid::now_v7();
    let entries = vec![entry(e, now - Duration::days(8))];
    let lines = vec![clearing_line(e, ACCOUNT)];
    let cache = vec![clearing_cache(ACCOUNT, 500)];

    let aged = aged_refund_clearing_grains(
        Uuid::from_u128(TENANT),
        &entries,
        &lines,
        &cache,
        now,
        warn,
        page,
        &NoopLedgerMetrics,
    );
    assert_eq!(aged.len(), 1, "an 8-day-open clearing grain is aged (Warn)");
    assert_eq!(aged[0].account_id, Uuid::from_u128(ACCOUNT));
    assert_eq!(aged[0].balance_minor, 500);
    assert!(!aged[0].paged, "8 days is past Warn but not the 14d Page");
}

#[test]
fn refund_clearing_aged_marks_paged_past_14d() {
    let now = OffsetDateTime::now_utc();
    let warn = now - Duration::seconds(WARN_SECS);
    let page = now - Duration::seconds(PAGE_SECS);
    // 15 days open — past BOTH thresholds ⇒ paged (STUCK_REFUND_CLEARING).
    let e = Uuid::now_v7();
    let entries = vec![entry(e, now - Duration::days(15))];
    let lines = vec![clearing_line(e, ACCOUNT)];
    let cache = vec![clearing_cache(ACCOUNT, 500)];

    let aged = aged_refund_clearing_grains(
        Uuid::from_u128(TENANT),
        &entries,
        &lines,
        &cache,
        now,
        warn,
        page,
        &NoopLedgerMetrics,
    );
    assert_eq!(aged.len(), 1);
    assert!(
        aged[0].paged,
        "15 days is past the 14d Page → close-blocking"
    );
}

#[test]
fn refund_clearing_skips_fresh_and_drained_grains() {
    let now = OffsetDateTime::now_utc();
    let warn = now - Duration::seconds(WARN_SECS);
    let page = now - Duration::seconds(PAGE_SECS);
    // Fresh (2 days) open grain + an old (10 days) but DRAINED (0) grain — neither
    // is aged (the 7d cutoff + the `balance_minor > 0` gate).
    let fresh = Uuid::now_v7();
    let drained = Uuid::now_v7();
    let other = 0xC2;
    let entries = vec![
        entry(fresh, now - Duration::days(2)),
        entry(drained, now - Duration::days(10)),
    ];
    let lines = vec![clearing_line(fresh, ACCOUNT), clearing_line(drained, other)];
    let cache = vec![clearing_cache(ACCOUNT, 500), clearing_cache(other, 0)];

    assert!(
        aged_refund_clearing_grains(
            Uuid::from_u128(TENANT),
            &entries,
            &lines,
            &cache,
            now,
            warn,
            page,
            &NoopLedgerMetrics,
        )
        .is_empty(),
        "a fresh open grain and a drained old grain are both un-aged"
    );
}

#[test]
fn stage1_orphan_flags_unmatched_aged_stage1() {
    let now = OffsetDateTime::now_utc();
    let cutoff = now - Duration::seconds(WARN_SECS);
    // A stage-1 `initiated` 8 days old with NO terminal phase ⇒ orphan.
    let rows = vec![refund_row(
        "psp-orphan",
        "initiated",
        now - Duration::days(8),
    )];
    let orphans = stage1_orphans(Uuid::from_u128(TENANT), &rows, now, cutoff);
    assert_eq!(orphans.len(), 1, "an unmatched aged stage-1 is an orphan");
    assert_eq!(orphans[0].psp_refund_id, "psp-orphan");
    assert_eq!(orphans[0].amount_minor, 500);
    assert!(orphans[0].age_secs >= WARN_SECS);
}

#[test]
fn stage1_orphan_skips_advanced_and_fresh_stage1() {
    let now = OffsetDateTime::now_utc();
    let cutoff = now - Duration::seconds(WARN_SECS);
    let rows = vec![
        // Advanced: stage-1 + a matching `confirmed` (same psp) ⇒ NOT an orphan,
        // even though the stage-1 is old.
        refund_row("psp-done", "initiated", now - Duration::days(9)),
        refund_row("psp-done", "confirmed", now - Duration::days(8)),
        // A stage-1 reversal also counts as advanced.
        refund_row("psp-reversed", "initiated", now - Duration::days(9)),
        refund_row("psp-reversed", "rejected", now - Duration::days(8)),
        // A FRESH unmatched stage-1 (2 days) is under the threshold ⇒ not yet.
        refund_row("psp-fresh", "initiated", now - Duration::days(2)),
    ];
    assert!(
        stage1_orphans(Uuid::from_u128(TENANT), &rows, now, cutoff).is_empty(),
        "an advanced or fresh stage-1 is not an orphan"
    );
}

// ---------------------------------------------------------------------------
// Group 2 (Slice-3 Phase-3): NEGATIVE_TAX_SUBBALANCE beyond-filing-window filter.
// ---------------------------------------------------------------------------

#[test]
fn beyond_window_flags_prior_filing_period() {
    // A negative tax sub-balance in a CLOSED (prior) filing period is beyond its
    // window ⇒ flagged. (June 2026 current; May 2026 is closed.)
    assert!(
        is_beyond_filing_window("202605", "202606"),
        "a strictly-earlier filing period is beyond the window"
    );
    // Crossing a year boundary still orders chronologically (YYYYMM lexicographic).
    assert!(is_beyond_filing_window("202512", "202601"));
}

#[test]
fn beyond_window_skips_current_filing_period() {
    // An in-window (current-period) negative is a legitimate reversal ⇒ NOT flagged.
    assert!(
        !is_beyond_filing_window("202606", "202606"),
        "the current filing period is in-window (legitimate reversal)"
    );
}

#[test]
fn beyond_window_skips_future_filing_period() {
    // A future filing period (clock skew / pre-staged) is not a closed period ⇒
    // NOT flagged.
    assert!(
        !is_beyond_filing_window("202607", "202606"),
        "a future filing period is not a closed prior period"
    );
}

// ---------------------------------------------------------------------------
// Docker (testcontainers) integration — the cross-tenant scans + run().
// ---------------------------------------------------------------------------

fn pg(sql: impl Into<String>) -> Statement {
    Statement::from_string(sea_orm::DatabaseBackend::Postgres, sql.into())
}

/// Boot + migrate; return a raw conn (for SQL seeds) and a secure provider.
async fn setup(container_url: &str) -> (DatabaseConnection, DBProvider<DbError>) {
    let raw = Database::connect(container_url).await.unwrap();
    Migrator::up(&raw, None).await.unwrap();
    let repo_url = format!("{container_url}?options=-c%20search_path%3Dbss,public");
    let tdb = connect_db(&repo_url, ConnectOpts::default()).await.unwrap();
    (raw, DBProvider::<DbError>::new(tdb))
}

/// `run()` over an empty ledger completes with `Ok(())` (no aged work).
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn run_over_empty_ledger_completes() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let (_raw, provider) = setup(&url).await;

    let job = AgedAlarmJob::new(provider, Arc::new(LedgerEventPublisher::noop()));
    job.run()
        .await
        .expect("run must succeed over an empty ledger");
}

/// An aged `QUEUED` `PAYMENT_ALLOCATE` row is surfaced by the cross-tenant queue
/// scan, and `run()` over it completes `Ok`.
///
/// **The emission is not observed here, by any test in this file.** An alarm
/// reaches `LedgerEventPublisher::emit_invariant_alarm`, and the publisher these
/// cases hand the job is `noop()`, whose `metrics` is `None` — so the counter
/// mirror never fires and nothing records the alarm itself. A `run()` that
/// emitted nothing at all would pass every case in this file identically. What
/// the scan assertions above prove is the detector; observing the alarm needs a
/// publisher built with `LedgerEventPublisher::with_metrics` over a recording
/// port.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn aged_queue_row_is_detected_and_run_completes() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let (raw, provider) = setup(&url).await;

    let tenant = Uuid::now_v7();
    // Seed one QUEUED PAYMENT_ALLOCATE row queued 3 days ago (older than the 1-day
    // threshold) — directly via SQL (the durable work-state row the scan reads).
    raw.execute_raw(pg(format!(
        "INSERT INTO bss.ledger_pending_event_queue \
         (tenant_id, flow, business_id, payload, queued_at, apply_after, status, attempts) \
         VALUES ('{tenant}', 'PAYMENT_ALLOCATE', 'alloc-aged-1', '{{}}'::jsonb, \
         now() - interval '3 days', NULL, 'QUEUED', 0)"
    )))
    .await
    .unwrap();
    // A fresh row (queued now) must NOT be aged.
    raw.execute_raw(pg(format!(
        "INSERT INTO bss.ledger_pending_event_queue \
         (tenant_id, flow, business_id, payload, queued_at, apply_after, status, attempts) \
         VALUES ('{tenant}', 'PAYMENT_ALLOCATE', 'alloc-fresh-1', '{{}}'::jsonb, \
         now(), NULL, 'QUEUED', 0)"
    )))
    .await
    .unwrap();

    let job = AgedAlarmJob::new(provider, Arc::new(LedgerEventPublisher::noop()));

    // The internal scan finds exactly the aged row (not the fresh one).
    let aged = job
        .aged_queue_rows(
            "PAYMENT_ALLOCATE",
            OffsetDateTime::now_utc(),
            Duration::seconds(86_400),
        )
        .await
        .expect("aged_queue_rows must succeed");
    assert_eq!(aged.len(), 1, "only the 3-day-old row is aged");
    assert_eq!(aged[0].business_id, "alloc-aged-1");
    assert_eq!(aged[0].tenant_id, tenant);
    assert!(aged[0].age_secs >= 86_400);

    // run() over the seeded ledger completes Ok. The Warn alarm it emits is not
    // observed — see this case's doc.
    job.run()
        .await
        .expect("run must succeed with an aged queue row");
}

/// An aged `CHARGEBACK` queue row is surfaced by the same scan under the
/// `CHARGEBACK` flow (the dispute-phase-queued family).
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn aged_chargeback_queue_row_is_detected() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let (raw, provider) = setup(&url).await;

    let tenant = Uuid::now_v7();
    raw.execute_raw(pg(format!(
        "INSERT INTO bss.ledger_pending_event_queue \
         (tenant_id, flow, business_id, payload, queued_at, apply_after, status, attempts) \
         VALUES ('{tenant}', 'CHARGEBACK', 'cb-aged-1', '{{}}'::jsonb, \
         now() - interval '2 days', NULL, 'QUEUED', 0)"
    )))
    .await
    .unwrap();

    let job = AgedAlarmJob::new(provider, Arc::new(LedgerEventPublisher::noop()));
    let aged = job
        .aged_queue_rows(
            "CHARGEBACK",
            OffsetDateTime::now_utc(),
            Duration::seconds(86_400),
        )
        .await
        .expect("aged_queue_rows(CHARGEBACK) must succeed");
    assert_eq!(aged.len(), 1, "the 2-day-old chargeback row is aged");
    assert_eq!(aged[0].business_id, "cb-aged-1");
}

/// Group F: an aged open `REFUND_CLEARING` balance is surfaced by the cross-tenant
/// refund-clearing scan (8 days → Warn), and `run()` over it completes `Ok`.
/// The alarm is not observed —
/// [`aged_queue_row_is_detected_and_run_completes`]'s doc says why — and neither
/// are the §9 gauges, for the separate reason that they run off the job's own
/// metrics sink, which `AgedAlarmJob::new` defaults to the no-op and only
/// `with_metrics` replaces.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn aged_refund_clearing_is_detected_and_run_completes() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let (raw, provider) = setup(&url).await;

    let tenant = Uuid::now_v7();
    let account = Uuid::now_v7();
    let source_account = Uuid::now_v7();
    let entry_id = Uuid::now_v7();
    // A REFUND_CLEARING journal entry posted 8 days ago (the age proxy source),
    // seeded with a BALANCED pair of lines in ONE transaction so the deferrable
    // `trg_journal_entry_balanced` constraint trigger fires exactly once — at the
    // txn COMMIT, against a complete, valid entry. (A per-statement autocommit
    // seed would fire the deferred trigger at the entry's own COMMIT, before any
    // line exists, raising LEDGER_ENTRY_EMPTY.)
    let txn = raw.begin().await.unwrap();
    txn.execute_raw(pg(format!(
        "INSERT INTO bss.ledger_journal_entry \
         (entry_id, tenant_id, legal_entity_id, period_id, entry_currency, source_doc_type, \
          source_business_id, posted_at_utc, effective_at, origin, posted_by_actor_id, \
          correlation_id, rounding_evidence, created_seq) \
         VALUES ('{entry_id}', '{tenant}', '{tenant}', '202606', 'USD', 'REFUND', \
          'psp-stuck:initiated', now() - interval '8 days', '2026-06-01', 'SYSTEM', '{tenant}', \
          '{entry_id}', '{{}}'::jsonb, 1)"
    )))
    .await
    .unwrap();
    // … its REFUND_CLEARING line (the grain the aged scan reads) …
    txn.execute_raw(pg(format!(
        "INSERT INTO bss.ledger_journal_line \
         (line_id, entry_id, tenant_id, period_id, payer_tenant_id, account_id, account_class, \
          side, amount_minor, currency, currency_scale, mapping_status) \
         VALUES ('{}', '{entry_id}', '{tenant}', '202606', '{tenant}', '{account}', \
          'REFUND_CLEARING', 'CR', 500, 'USD', 2, 'RESOLVED')",
        Uuid::now_v7()
    )))
    .await
    .unwrap();
    // … and its balancing DR leg (the money source a stage-1 refund reserves from),
    // making the entry zero-sum per (currency, currency_scale) so the deferred
    // balanced-trigger passes at COMMIT.
    txn.execute_raw(pg(format!(
        "INSERT INTO bss.ledger_journal_line \
         (line_id, entry_id, tenant_id, period_id, payer_tenant_id, account_id, account_class, \
          side, amount_minor, currency, currency_scale, mapping_status) \
         VALUES ('{}', '{entry_id}', '{tenant}', '202606', '{tenant}', '{source_account}', \
          'UNALLOCATED', 'DR', 500, 'USD', 2, 'RESOLVED')",
        Uuid::now_v7()
    )))
    .await
    .unwrap();
    txn.commit().await.unwrap();
    // … and the open cache grain (`balance_minor > 0`); `ledger_account_balance`
    // has no balanced-trigger, so a plain autocommit insert is fine here.
    raw.execute_raw(pg(format!(
        "INSERT INTO bss.ledger_account_balance \
         (tenant_id, account_id, currency, account_class, normal_side, balance_minor, version) \
         VALUES ('{tenant}', '{account}', 'USD', 'REFUND_CLEARING', 'CR', 500, 0)"
    )))
    .await
    .unwrap();

    let job = AgedAlarmJob::new(provider, Arc::new(LedgerEventPublisher::noop()));
    let aged = job
        .aged_refund_clearing_grains(OffsetDateTime::now_utc())
        .await
        .expect("aged_refund_clearing_grains must succeed");
    assert_eq!(aged.len(), 1, "the 8-day-open clearing grain is aged");
    assert_eq!(aged[0].tenant_id, tenant);
    assert_eq!(aged[0].balance_minor, 500);
    assert!(!aged[0].paged, "8 days is Warn, not the 14d Page");

    job.run()
        .await
        .expect("run must succeed with an aged clearing");
}

/// Group F: an orphaned stage-1 refund (an `initiated` row 8 days old with no
/// matching terminal phase) is surfaced by the cross-tenant stage-1-orphan scan.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn stage1_orphan_refund_is_detected() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let (raw, provider) = setup(&url).await;

    let tenant = Uuid::now_v7();
    // An `initiated` stage-1 refund 8 days old, NO terminal phase ⇒ orphan.
    raw.execute_raw(pg(format!(
        "INSERT INTO bss.ledger_refund \
         (tenant_id, refund_id, psp_refund_id, phase, pattern, payment_id, currency, \
          amount_minor, clearing_state, created_at_utc, version) \
         VALUES ('{tenant}', 'rf-orphan', 'psp-orphan', 'initiated', 'A_UNALLOCATED', 'pay-1', \
          'USD', 500, 'PENDING', now() - interval '8 days', 0)"
    )))
    .await
    .unwrap();
    // A fully-advanced refund (initiated + confirmed) must NOT be flagged.
    raw.execute_raw(pg(format!(
        "INSERT INTO bss.ledger_refund \
         (tenant_id, refund_id, psp_refund_id, phase, pattern, payment_id, currency, \
          amount_minor, clearing_state, created_at_utc, version) \
         VALUES ('{tenant}', 'rf-done-1', 'psp-done', 'initiated', 'A_UNALLOCATED', 'pay-2', \
          'USD', 500, 'PENDING', now() - interval '9 days', 0)"
    )))
    .await
    .unwrap();
    raw.execute_raw(pg(format!(
        "INSERT INTO bss.ledger_refund \
         (tenant_id, refund_id, psp_refund_id, phase, pattern, payment_id, currency, \
          amount_minor, clearing_state, created_at_utc, version) \
         VALUES ('{tenant}', 'rf-done-2', 'psp-done', 'confirmed', 'A_UNALLOCATED', 'pay-2', \
          'USD', 500, 'SETTLED', now() - interval '8 days', 0)"
    )))
    .await
    .unwrap();

    let job = AgedAlarmJob::new(provider, Arc::new(LedgerEventPublisher::noop()));
    let orphans = job
        .stage1_orphan_refunds(OffsetDateTime::now_utc())
        .await
        .expect("stage1_orphan_refunds must succeed");
    assert_eq!(orphans.len(), 1, "only the unmatched stage-1 is an orphan");
    assert_eq!(orphans[0].psp_refund_id, "psp-orphan");
    assert_eq!(orphans[0].tenant_id, tenant);
}

/// Seed a balanced `DR <source> / CR UNALLOCATED` journal entry whose
/// `posted_at_utc` is `age_days` in the past, and the matching open
/// `unallocated_balance` cache grain (`balance_minor`). Mirrors the
/// refund-clearing integration seed: the deferrable `trg_journal_entry_balanced`
/// trigger needs a complete, zero-sum entry at COMMIT, so the entry + both legs
/// land in ONE transaction. The CR UNALLOCATED line (the grain the unallocated
/// age scan reads) is balanced by a DR CASH_CLEARING leg so the entry is zero-sum
/// per `(currency, currency_scale)`. The `ledger_unallocated_balance` cache has
/// no balanced-trigger, so a plain autocommit insert is fine.
async fn seed_unallocated_grain(
    raw: &DatabaseConnection,
    tenant: Uuid,
    payer: Uuid,
    unallocated_account: Uuid,
    age_days: i64,
    balance_minor: i64,
) {
    let entry_id = Uuid::now_v7();
    let cash_account = Uuid::now_v7();
    let txn = raw.begin().await.unwrap();
    txn.execute_raw(pg(format!(
        "INSERT INTO bss.ledger_journal_entry \
         (entry_id, tenant_id, legal_entity_id, period_id, entry_currency, source_doc_type, \
          source_business_id, posted_at_utc, effective_at, origin, posted_by_actor_id, \
          correlation_id, rounding_evidence, created_seq) \
         VALUES ('{entry_id}', '{tenant}', '{tenant}', '202606', 'USD', 'PAYMENT_SETTLE', \
          'pay-{entry_id}', now() - interval '{age_days} days', '2026-06-01', 'SYSTEM', '{tenant}', \
          '{entry_id}', '{{}}'::jsonb, 1)"
    )))
    .await
    .unwrap();
    // The CR UNALLOCATED leg (the grain the aged-unallocated scan reads): its
    // owning entry's `posted_at_utc` is the age proxy (the resolved G-P5a oldest
    // contributing line's post time).
    txn.execute_raw(pg(format!(
        "INSERT INTO bss.ledger_journal_line \
         (line_id, entry_id, tenant_id, period_id, payer_tenant_id, account_id, account_class, \
          side, amount_minor, currency, currency_scale, mapping_status) \
         VALUES ('{}', '{entry_id}', '{tenant}', '202606', '{payer}', '{unallocated_account}', \
          'UNALLOCATED', 'CR', 1000, 'USD', 2, 'RESOLVED')",
        Uuid::now_v7()
    )))
    .await
    .unwrap();
    // … and its balancing DR CASH_CLEARING leg, so the entry is zero-sum at COMMIT.
    txn.execute_raw(pg(format!(
        "INSERT INTO bss.ledger_journal_line \
         (line_id, entry_id, tenant_id, period_id, payer_tenant_id, account_id, account_class, \
          side, amount_minor, currency, currency_scale, mapping_status) \
         VALUES ('{}', '{entry_id}', '{tenant}', '202606', '{payer}', '{cash_account}', \
          'CASH_CLEARING', 'DR', 1000, 'USD', 2, 'RESOLVED')",
        Uuid::now_v7()
    )))
    .await
    .unwrap();
    txn.commit().await.unwrap();
    // The open cache grain (`balance_minor`), keyed (tenant, payer, currency).
    raw.execute_raw(pg(format!(
        "INSERT INTO bss.ledger_unallocated_balance \
         (tenant_id, payer_tenant_id, account_id, currency, balance_minor, version) \
         VALUES ('{tenant}', '{payer}', '{unallocated_account}', 'USD', {balance_minor}, 0)"
    )))
    .await
    .unwrap();
}

/// Group C: an aged parked `unallocated_balance` grain (oldest contributing
/// `UNALLOCATED` line posted 3 days ago, cache still holding cash) is surfaced by
/// the per-tenant DB scan `aged_unallocated_grains` — the read path that
/// enumerates tenants from the cache, reads each tenant's journal entries +
/// `UNALLOCATED` lines + cache, builds the `entry_id -> posted_at_utc` age map,
/// and folds it through [`aged_grains`]. `run()` then completes `Ok`; the
/// `AGED_UNALLOCATED` Warn alarm it emits is not observed —
/// [`aged_queue_row_is_detected_and_run_completes`]'s doc says why.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn aged_unallocated_grain_is_detected_and_run_completes() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let (raw, provider) = setup(&url).await;

    let tenant = Uuid::now_v7();
    let payer = Uuid::now_v7();
    let account = Uuid::now_v7();
    // A pool funded 3 days ago (older than the 1-day cutoff) still holding 700.
    seed_unallocated_grain(&raw, tenant, payer, account, 3, 700).await;

    let job = AgedAlarmJob::new(provider, Arc::new(LedgerEventPublisher::noop()));

    // The per-tenant DB scan finds exactly the aged grain, with the age proxy read
    // from the contributing entry's posted_at (≥ the 1-day threshold) and the
    // parked balance carried through.
    let aged = job
        .aged_unallocated_grains(OffsetDateTime::now_utc(), Duration::seconds(86_400))
        .await
        .expect("aged_unallocated_grains must succeed");
    assert_eq!(aged.len(), 1, "the 3-day-old parked pool is aged");
    assert_eq!(aged[0].tenant_id, tenant);
    assert_eq!(aged[0].payer_tenant_id, payer);
    assert_eq!(aged[0].account_id, account);
    assert_eq!(aged[0].balance_minor, 700, "the parked balance is reported");
    assert_eq!(aged[0].currency, "USD");
    assert!(
        aged[0].age_secs >= 86_400,
        "age proxy is the oldest contributing line's post time (~3 days): {}",
        aged[0].age_secs
    );

    // run() over the seeded ledger completes Ok. The AGED_UNALLOCATED Warn it
    // emits is not observed — see this case's doc.
    job.run()
        .await
        .expect("run must succeed with an aged unallocated grain");
}

/// Group C complement: a freshly-funded pool (oldest contributing line posted
/// just now) is NOT aged, and a long-parked-but-DRAINED grain (`balance_minor =
/// 0`) is NOT aged either — the `balance_minor > 0` gate. The cross-tenant
/// per-tenant scan returns nothing in both cases, and `run()` still completes
/// `Ok` (no alarm to emit).
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn fresh_or_drained_unallocated_grain_is_not_aged() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let (raw, provider) = setup(&url).await;

    // Tenant A: a pool funded JUST NOW (0 days), still holding cash ⇒ not aged
    // (its oldest line is under the 1-day cutoff).
    let tenant_fresh = Uuid::now_v7();
    seed_unallocated_grain(&raw, tenant_fresh, Uuid::now_v7(), Uuid::now_v7(), 0, 700).await;
    // Tenant B: a pool funded 5 days ago but fully DRAINED to 0 ⇒ not aged (the
    // `balance_minor > 0` gate — a drained pool needs no alarm even when old).
    let tenant_drained = Uuid::now_v7();
    seed_unallocated_grain(&raw, tenant_drained, Uuid::now_v7(), Uuid::now_v7(), 5, 0).await;

    let job = AgedAlarmJob::new(provider, Arc::new(LedgerEventPublisher::noop()));
    let aged = job
        .aged_unallocated_grains(OffsetDateTime::now_utc(), Duration::seconds(86_400))
        .await
        .expect("aged_unallocated_grains must succeed");
    assert!(
        aged.is_empty(),
        "neither a fresh pool nor a drained old pool is aged: {aged:?}"
    );

    job.run()
        .await
        .expect("run must succeed with no aged unallocated grain");
}

/// Seed one `tax_subbalance` cache grain directly (a self-contained grain — no
/// journal age computation, unlike the unallocated / refund-clearing scans, so a
/// plain autocommit insert suffices). `balance_minor < 0` makes it a negative
/// grain; `filing_period` (`YYYYMM`) places it in a closed-prior or in-window
/// period. Mirrors the `seed_unallocated_grain` direct-seed idiom.
async fn seed_tax_subbalance(
    raw: &DatabaseConnection,
    tenant: Uuid,
    account: Uuid,
    jurisdiction: &str,
    filing_period: &str,
    balance_minor: i64,
) {
    raw.execute_raw(pg(format!(
        "INSERT INTO bss.ledger_tax_subbalance \
         (tenant_id, account_id, tax_jurisdiction, tax_filing_period, balance_minor, version) \
         VALUES ('{tenant}', '{account}', '{jurisdiction}', '{filing_period}', {balance_minor}, 0)"
    )))
    .await
    .unwrap();
}

/// Group 2 (Slice-3 Phase-3): the `NEGATIVE_TAX_SUBBALANCE` cross-tenant DB scan
/// `negative_tax_subbalances` — the read path that currently has only pure-function
/// coverage of its `is_beyond_filing_window` filter. A `tax_subbalance` that went
/// negative in a CLOSED (prior) filing period is beyond its window and is flagged;
/// an in-window (current-period) negative is a legitimate reversal and is NOT
/// flagged; a non-negative prior-period grain is not flagged either. `run()` then
/// completes `Ok`; the `Critical` `NEGATIVE_TAX_SUBBALANCE` alarm it emits per
/// flagged grain is not observed —
/// [`aged_queue_row_is_detected_and_run_completes`]'s doc says why.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn negative_tax_subbalance_beyond_window_is_detected_and_run_completes() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let (raw, provider) = setup(&url).await;

    let now = OffsetDateTime::now_utc();
    let current_period = crate::domain::instant::yyyymm(now);
    let tenant = Uuid::now_v7();
    let account = Uuid::now_v7();

    // (a) A negative grain in a long-closed prior period (200001) ⇒ beyond the
    //     filing window ⇒ flagged (Revenue Assurance must reconcile).
    seed_tax_subbalance(&raw, tenant, account, "US-CA", "200001", -250).await;
    // (b) A negative grain in the CURRENT filing period ⇒ in-window legitimate
    //     reversal ⇒ NOT flagged.
    seed_tax_subbalance(&raw, tenant, account, "US-NY", &current_period, -100).await;
    // (c) A non-negative grain in a closed prior period ⇒ not negative ⇒ NOT flagged.
    seed_tax_subbalance(&raw, tenant, account, "US-TX", "200001", 500).await;

    let job = AgedAlarmJob::new(provider, Arc::new(LedgerEventPublisher::noop()));

    let grains: Vec<NegativeTaxGrain> = job
        .negative_tax_subbalances(now)
        .await
        .expect("negative_tax_subbalances must succeed");
    assert_eq!(
        grains.len(),
        1,
        "only the negative-beyond-window grain is flagged (in-window + non-negative skipped): {grains:?}"
    );
    let g = &grains[0];
    assert_eq!(g.tenant_id, tenant);
    assert_eq!(g.account_id, account);
    assert_eq!(g.tax_jurisdiction, "US-CA");
    assert_eq!(g.tax_filing_period, "200001");
    assert_eq!(g.balance_minor, -250);

    // run() over the seeded ledger completes Ok. The Critical alarm it emits is
    // not observed — see this case's doc.
    job.run()
        .await
        .expect("run must succeed with a negative-beyond-window tax sub-balance");
}
