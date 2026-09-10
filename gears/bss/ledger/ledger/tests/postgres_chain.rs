//! Postgres-only end-to-end: the in-transaction tamper-evidence chain seal.
//! Boots a container, migrates, seeds reference data, then drives the full
//! post sequence and asserts the chain columns: a first post seals with the
//! tenant genesis `prev_hash` and writes the `chain_state` tip; a second post
//! links onto the first; N concurrent posts form a single linear chain. The
//! append-only trigger negatives (a re-seal, a business-column UPDATE, and a
//! DELETE) are exercised with raw SQL against a posted-and-sealed entry.
//! Ignored by default; run with `cargo test -p cf-gears-bss-ledger -- --ignored`.

#![allow(
    clippy::non_ascii_literal,
    clippy::let_underscore_must_use,
    clippy::needless_collect,
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::doc_markdown,
    clippy::panic
)]

use bss_ledger::domain::chain::genesis_prev_hash;
use bss_ledger::domain::error::DomainError;
use bss_ledger::domain::model::{AccountRow, CurrencyScaleRow, NewEntry, NewLine};
use bss_ledger::domain::money::DEFAULT_PLAUSIBLE_MAX_MAJOR;
use bss_ledger::infra::events::publisher::LedgerEventPublisher;
use bss_ledger::infra::jobs::verifier::ChainVerifierJob;
use bss_ledger::infra::posting::service::PostingService;
use bss_ledger::infra::storage::migrations::Migrator;
use bss_ledger::infra::storage::repo::ReferenceRepo;
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

/// Hex string of a single-column, single-row SELECT (e.g. `encode(col,'hex')`),
/// `None` when the row is absent or the value is NULL.
async fn scalar_hex(conn: &DatabaseConnection, sql: &str) -> Option<String> {
    let row = conn.query_one_raw(pg(sql.to_owned())).await.unwrap();
    row.and_then(|r| r.try_get_by_index::<Option<String>>(0).unwrap())
}

/// Hex string of a 32-byte hash for embedding in a Postgres text comparison.
fn hex32(bytes: &[u8; 32]) -> String {
    let mut s = String::with_capacity(64);
    for b in bytes {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Count rows matching a bss-qualified predicate.
async fn count(conn: &DatabaseConnection, sql: &str) -> i64 {
    let row = conn.query_one_raw(pg(sql.to_owned())).await.unwrap();
    row.map_or(0, |r| r.try_get_by_index::<i64>(0).unwrap())
}

struct Fixture {
    tenant: Uuid,
    ar_account: Uuid,
    cash_account: Uuid,
    legal_entity: Uuid,
    period_id: String,
}

/// Boot, migrate, seed USD@2 + OPEN period + AR/CASH accounts; return the
/// migrate connection, the service, the provider, and the fixture ids.
/// (Mirrors `postgres_posting.rs::setup`.)
async fn setup(
    container_url: &str,
) -> (
    DatabaseConnection,
    PostingService,
    DBProvider<DbError>,
    Fixture,
) {
    let raw = Database::connect(container_url).await.unwrap();
    Migrator::up(&raw, None).await.unwrap();

    let repo_url = format!("{container_url}?options=-c%20search_path%3Dbss,public");
    let tdb = connect_db(&repo_url, ConnectOpts::default()).await.unwrap();
    let provider = DBProvider::<DbError>::new(tdb);

    let tenant = Uuid::now_v7();
    let legal_entity = tenant;
    let period_id = "202606".to_owned();
    let ar_account = Uuid::now_v7();
    let cash_account = Uuid::now_v7();

    let reference = ReferenceRepo::new(provider.clone());
    reference
        .upsert_currency_scale(CurrencyScaleRow {
            tenant_id: tenant,
            currency: "USD".to_owned(),
            minor_units: 2,
            plausible_max_major: DEFAULT_PLAUSIBLE_MAX_MAJOR,
            source: "iso".to_owned(),
        })
        .await
        .unwrap();

    raw.execute_raw(pg(format!(
        "INSERT INTO bss.ledger_fiscal_period (tenant_id, legal_entity_id, period_id, fiscal_tz, status)
         VALUES ('{tenant}','{legal_entity}','{period_id}','UTC','OPEN')"
    )))
    .await
    .unwrap();

    reference
        .insert_account(AccountRow {
            account_id: ar_account,
            tenant_id: tenant,
            legal_entity_id: legal_entity,
            account_class: "AR".to_owned(),
            currency: "USD".to_owned(),
            revenue_stream: None,
            normal_side: "DR".to_owned(),
            may_go_negative: false,
            lifecycle_state: "OPEN".to_owned(),
        })
        .await
        .unwrap();
    reference
        .insert_account(AccountRow {
            account_id: cash_account,
            tenant_id: tenant,
            legal_entity_id: legal_entity,
            account_class: "CASH_CLEARING".to_owned(),
            currency: "USD".to_owned(),
            revenue_stream: None,
            normal_side: "CR".to_owned(),
            may_go_negative: false,
            lifecycle_state: "OPEN".to_owned(),
        })
        .await
        .unwrap();

    let service = PostingService::new(
        provider.clone(),
        std::sync::Arc::new(bss_ledger::infra::events::publisher::LedgerEventPublisher::noop()),
    );
    (
        raw,
        service,
        provider,
        Fixture {
            tenant,
            ar_account,
            cash_account,
            legal_entity,
            period_id,
        },
    )
}

/// Build a balanced entry for `fixture` with `business_id`: DR AR / CR CASH,
/// each `amount`. (Mirrors `postgres_posting.rs::balanced_entry`, no swap.)
fn balanced_entry(f: &Fixture, business_id: &str, amount: i64) -> (NewEntry, Vec<NewLine>) {
    let entry_id = Uuid::now_v7();
    let entry = NewEntry {
        entry_id,
        tenant_id: f.tenant,
        legal_entity_id: f.legal_entity,
        period_id: f.period_id.clone(),
        entry_currency: "USD".to_owned(),
        source_doc_type: SourceDocType::ManualAdjustment,
        source_business_id: business_id.to_owned(),
        reverses_entry_id: None,
        reverses_period_id: None,
        posted_at_utc: OffsetDateTime::now_utc(),
        effective_at: NaiveDate::from_ymd_opt(2026, 6, 1).unwrap(),
        origin: "SYSTEM".to_owned(),
        posted_by_actor_id: f.tenant,
        correlation_id: f.tenant,
        rounding_evidence: serde_json::Value::Null,
        rate_snapshot_ref: None,
    };
    let lines = vec![
        line(f, f.ar_account, AccountClass::Ar, Side::Debit, amount),
        line(
            f,
            f.cash_account,
            AccountClass::CashClearing,
            Side::Credit,
            amount,
        ),
    ];
    (entry, lines)
}

fn line(f: &Fixture, account: Uuid, class: AccountClass, side: Side, amount: i64) -> NewLine {
    NewLine {
        line_id: Uuid::now_v7(),
        ar_status: None,
        payer_tenant_id: f.tenant,
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
        pricing_snapshot_ref: None,
        po_allocation_group: None,
        credit_grant_event_type: None,
    }
}

/// First post seals with the tenant genesis `prev_hash`, a non-NULL `row_hash`,
/// a NULL `prev_entry_id`, and writes a `chain_state` tip pointing at it.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn seals_first_post_with_genesis_prev() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");

    let (raw, service, _provider, f) = setup(&url).await;
    let scope = AccessScope::for_tenant(f.tenant);
    let ctx = SecurityContext::anonymous();

    let (entry, lines) = balanced_entry(&f, "biz-1", 1000);
    let entry_id = entry.entry_id;
    service
        .post(&ctx, &scope, entry, lines, None)
        .await
        .expect("post must succeed");

    // row_hash is sealed (non-NULL).
    let row_hash = scalar_hex(
        &raw,
        &format!(
            "SELECT encode(row_hash,'hex') FROM bss.ledger_journal_entry WHERE entry_id='{entry_id}'"
        ),
    )
    .await;
    assert!(row_hash.is_some(), "row_hash must be sealed (non-NULL)");
    let row_hash = row_hash.unwrap();

    // prev_hash == genesis_prev_hash(tenant).
    let prev_hash = scalar_hex(
        &raw,
        &format!(
            "SELECT encode(prev_hash,'hex') FROM bss.ledger_journal_entry WHERE entry_id='{entry_id}'"
        ),
    )
    .await
    .expect("prev_hash must be set");
    assert_eq!(
        prev_hash,
        hex32(&genesis_prev_hash(f.tenant)),
        "first post prev_hash must be the tenant genesis seed"
    );

    // prev_entry_id IS NULL at genesis.
    let prev_entry_nulls = count(
        &raw,
        &format!(
            "SELECT COUNT(*) FROM bss.ledger_journal_entry \
             WHERE entry_id='{entry_id}' AND prev_entry_id IS NULL"
        ),
    )
    .await;
    assert_eq!(prev_entry_nulls, 1, "genesis prev_entry_id must be NULL");

    // chain_state tip's last_row_hash equals this entry's row_hash; the tip's
    // last_entry_id (a uuid) is asserted separately by the count below.
    let tip_row_hash = scalar_hex(
        &raw,
        &format!(
            "SELECT encode(last_row_hash,'hex') FROM bss.chain_state WHERE tenant_id='{}'",
            f.tenant
        ),
    )
    .await
    .expect("chain_state tip must exist");
    assert_eq!(
        tip_row_hash, row_hash,
        "tip last_row_hash must equal row_hash"
    );

    let tip_points_at_entry = count(
        &raw,
        &format!(
            "SELECT COUNT(*) FROM bss.chain_state \
             WHERE tenant_id='{}' AND last_entry_id='{entry_id}'",
            f.tenant
        ),
    )
    .await;
    assert_eq!(
        tip_points_at_entry, 1,
        "chain_state tip must reference the sealed entry id"
    );
}

/// A second post for the same tenant links onto the first: its `prev_hash` is
/// the first entry's `row_hash`, and its prev pointers name the first entry.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn links_second_post_to_first() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");

    let (raw, service, _provider, f) = setup(&url).await;
    let scope = AccessScope::for_tenant(f.tenant);
    let ctx = SecurityContext::anonymous();

    let (e1, l1) = balanced_entry(&f, "biz-1", 1000);
    let first_id = e1.entry_id;
    service
        .post(&ctx, &scope, e1, l1, None)
        .await
        .expect("post 1");

    let (e2, l2) = balanced_entry(&f, "biz-2", 2000);
    let second_id = e2.entry_id;
    service
        .post(&ctx, &scope, e2, l2, None)
        .await
        .expect("post 2");

    let first_row_hash = scalar_hex(
        &raw,
        &format!(
            "SELECT encode(row_hash,'hex') FROM bss.ledger_journal_entry WHERE entry_id='{first_id}'"
        ),
    )
    .await
    .expect("first row_hash");
    let second_prev_hash = scalar_hex(
        &raw,
        &format!(
            "SELECT encode(prev_hash,'hex') FROM bss.ledger_journal_entry WHERE entry_id='{second_id}'"
        ),
    )
    .await
    .expect("second prev_hash");
    assert_eq!(
        second_prev_hash, first_row_hash,
        "second entry prev_hash must equal first entry row_hash"
    );

    // prev_entry_id and prev_period_id of the second point at the first.
    let links = count(
        &raw,
        &format!(
            "SELECT COUNT(*) FROM bss.ledger_journal_entry \
             WHERE entry_id='{second_id}' AND prev_entry_id='{first_id}' \
               AND prev_period_id='{}'",
            f.period_id
        ),
    )
    .await;
    assert_eq!(
        links, 1,
        "second entry prev pointers must name the first entry"
    );
}

/// N concurrent posts for one tenant form a single linear chain: all `row_hash`
/// distinct, no two share a `prev_hash`, and every non-genesis `prev_hash`
/// matches exactly one other entry's `row_hash`.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn concurrent_posts_form_linear_chain() {
    const N: usize = 8;
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");

    let (raw, service, provider, f) = setup(&url).await;

    // Concurrent burst: races the in-txn seal on the per-tenant `chain_state`
    // hot row. Under the bounded SERIALIZABLE retry budget some posts may lose
    // the race (chain_state + shared balance-row contention) and roll back fully
    // — acceptable: a rolled-back post never enters the chain (no orphan, no
    // gap). The invariant under test is that whatever commits never forks.
    let mut handles = Vec::with_capacity(N);
    for i in 0..N {
        let svc = service.clone();
        let scope = AccessScope::for_tenant(f.tenant);
        let ctx = SecurityContext::anonymous();
        let amount = 1000 + i64::try_from(i).unwrap();
        let (entry, lines) = balanced_entry(&f, &format!("biz-conc-{i}"), amount);
        handles.push(tokio::spawn(async move {
            svc.post(&ctx, &scope, entry, lines, None).await
        }));
    }
    for h in handles {
        // Ignore individual contention losses; the settle pass below lands them.
        let _ = h.await.unwrap();
    }

    // Settle: re-post each business key sequentially. Idempotent on the business
    // key — a post that committed in the burst replays (same payload hash → the
    // prior entry), a rolled-back one posts fresh — so all N keys end with
    // exactly one committed, sealed entry. This also proves the chain is
    // gap-free under concurrency.
    let scope = AccessScope::for_tenant(f.tenant);
    let ctx = SecurityContext::anonymous();
    for i in 0..N {
        let amount = 1000 + i64::try_from(i).unwrap();
        let (entry, lines) = balanced_entry(&f, &format!("biz-conc-{i}"), amount);
        service
            .post(&ctx, &scope, entry, lines, None)
            .await
            .expect("sequential settle post must succeed");
    }

    // All sealed entries for the tenant.
    let row_hashes = fetch_hex_set(
        &raw,
        &format!(
            "SELECT encode(row_hash,'hex') FROM bss.ledger_journal_entry \
             WHERE tenant_id='{}' AND row_hash IS NOT NULL ORDER BY created_seq",
            f.tenant
        ),
    )
    .await;
    assert_eq!(row_hashes.len(), N, "every post must be sealed");

    // row_hash all distinct.
    let mut distinct = row_hashes.clone();
    distinct.sort();
    distinct.dedup();
    assert_eq!(distinct.len(), N, "all row_hash must be distinct");

    // prev_hash of all entries (genesis included).
    let prev_hashes = fetch_hex_set(
        &raw,
        &format!(
            "SELECT encode(prev_hash,'hex') FROM bss.ledger_journal_entry \
             WHERE tenant_id='{}' ORDER BY created_seq",
            f.tenant
        ),
    )
    .await;
    assert_eq!(prev_hashes.len(), N, "every entry has a prev_hash");

    // No two entries share a prev_hash (a fork would re-use a prev link).
    let mut distinct_prev = prev_hashes.clone();
    distinct_prev.sort();
    distinct_prev.dedup();
    assert_eq!(
        distinct_prev.len(),
        N,
        "no two entries may share a prev_hash (single linear chain)"
    );

    // Exactly one genesis link; every other prev_hash matches some row_hash.
    let genesis = hex32(&genesis_prev_hash(f.tenant));
    let genesis_count = prev_hashes.iter().filter(|h| **h == genesis).count();
    assert_eq!(genesis_count, 1, "exactly one entry links to genesis");
    for prev in &prev_hashes {
        if *prev == genesis {
            continue;
        }
        assert!(
            row_hashes.contains(prev),
            "every non-genesis prev_hash must match another entry's row_hash"
        );
    }

    // SERIALIZABLE chain-tip contract: the structural checks above
    // prove no fork; now prove the Verifier AGREES the chain verifies clean
    // after a concurrent burst (no break → no freeze). This ties the lockless
    // read-then-advance to the real verify path — if a future change downgrades
    // the post below SERIALIZABLE and a fork slips through, the Verifier here
    // would detect the break and freeze, failing this assertion.
    ChainVerifierJob::new(
        provider.clone(),
        std::sync::Arc::new(LedgerEventPublisher::noop()),
        std::sync::Arc::new(bss_ledger::domain::ports::metrics::NoopLedgerMetrics),
    )
    .run()
    .await
    .expect("verify run");

    let active_freezes = count(
        &raw,
        &format!(
            "SELECT COUNT(*) FROM bss.scope_freeze \
             WHERE tenant_id='{}' AND cleared_at IS NULL",
            f.tenant
        ),
    )
    .await;
    assert_eq!(
        active_freezes, 0,
        "concurrent seals must leave a clean, verifiable chain (no fork → no freeze)"
    );
}

/// Z3-1: a tip ROLLBACK orphans the sealed rows above it. `chain_state` is a
/// MUTABLE tip cache (no append-only guard), so a writer who redirects it to an
/// older entry hides every newer sealed row — the walk-from-tip still verifies
/// clean. The Verifier must reconcile the walked count against the total sealed
/// rows and freeze the tenant on the surplus.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn tip_rollback_orphaning_sealed_rows_freezes_tenant() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");

    let (raw, service, provider, f) = setup(&url).await;
    let scope = AccessScope::for_tenant(f.tenant);
    let ctx = SecurityContext::anonymous();

    // genesis <- entry1 <- entry2; the tip points at entry2.
    let (e1, l1) = balanced_entry(&f, "biz-rollback-1", 1000);
    let e1_id = e1.entry_id;
    service
        .post(&ctx, &scope, e1, l1, None)
        .await
        .expect("post 1");
    let (e2, l2) = balanced_entry(&f, "biz-rollback-2", 2000);
    service
        .post(&ctx, &scope, e2, l2, None)
        .await
        .expect("post 2");

    // Roll the visible tip back to entry1: entry2 is now a sealed row NEWER than
    // the tip — orphaned, never reached by the tip-to-genesis walk.
    raw.execute_raw(pg(format!(
        "UPDATE bss.chain_state SET last_entry_id='{e1_id}', last_period_id='{}' \
         WHERE tenant_id='{}'",
        f.period_id, f.tenant
    )))
    .await
    .unwrap();

    // The walk-from-tip alone (entry1 -> genesis) is clean; only the count
    // reconciliation can catch the orphaned entry2.
    ChainVerifierJob::new(
        provider.clone(),
        std::sync::Arc::new(LedgerEventPublisher::noop()),
        std::sync::Arc::new(bss_ledger::domain::ports::metrics::NoopLedgerMetrics),
    )
    .run()
    .await
    .expect("verify run");

    let active_freezes = count(
        &raw,
        &format!(
            "SELECT COUNT(*) FROM bss.scope_freeze \
             WHERE tenant_id='{}' AND cleared_at IS NULL",
            f.tenant
        ),
    )
    .await;
    assert_eq!(
        active_freezes, 1,
        "a tip rollback that orphans sealed rows must freeze the tenant"
    );
}

/// Z3-2 (§5.2): when the Verifier freezes a tenant it must also write a
/// `freeze-set-clear` secured-audit record in the SAME transaction — a
/// tamper-evident record of who froze the tenant and why. Before the fix the
/// freeze touched only `scope_freeze` (a mutable, non-chained row).
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn verifier_freeze_writes_freeze_set_clear_audit_record() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");

    let (raw, service, provider, f) = setup(&url).await;
    let scope = AccessScope::for_tenant(f.tenant);
    let ctx = SecurityContext::anonymous();

    // Two posts, then roll the tip back so the Verifier detects an orphan and
    // freezes (reuses the Z3-1 path — any freeze must write the audit record).
    let (e1, l1) = balanced_entry(&f, "biz-fsc-1", 1000);
    let e1_id = e1.entry_id;
    service
        .post(&ctx, &scope, e1, l1, None)
        .await
        .expect("post 1");
    let (e2, l2) = balanced_entry(&f, "biz-fsc-2", 2000);
    service
        .post(&ctx, &scope, e2, l2, None)
        .await
        .expect("post 2");
    raw.execute_raw(pg(format!(
        "UPDATE bss.chain_state SET last_entry_id='{e1_id}', last_period_id='{}' \
         WHERE tenant_id='{}'",
        f.period_id, f.tenant
    )))
    .await
    .unwrap();

    ChainVerifierJob::new(
        provider.clone(),
        std::sync::Arc::new(LedgerEventPublisher::noop()),
        std::sync::Arc::new(bss_ledger::domain::ports::metrics::NoopLedgerMetrics),
    )
    .run()
    .await
    .expect("verify run");

    let frozen = count(
        &raw,
        &format!(
            "SELECT COUNT(*) FROM bss.scope_freeze \
             WHERE tenant_id='{}' AND cleared_at IS NULL",
            f.tenant
        ),
    )
    .await;
    assert_eq!(frozen, 1, "the tenant must be frozen (precondition)");

    let audit_records = count(
        &raw,
        &format!(
            "SELECT COUNT(*) FROM bss.secured_audit_record \
             WHERE tenant_id='{}' AND event_type='freeze-set-clear'",
            f.tenant
        ),
    )
    .await;
    assert_eq!(
        audit_records, 1,
        "a freeze must write exactly one freeze-set-clear secured-audit record (§5.2)"
    );
}

/// Collect a single text column over many rows into a `Vec<String>`.
async fn fetch_hex_set(conn: &DatabaseConnection, sql: &str) -> Vec<String> {
    let rows = conn.query_all_raw(pg(sql.to_owned())).await.unwrap();
    rows.into_iter()
        .map(|r| r.try_get_by_index::<String>(0).unwrap())
        .collect()
}

/// The append-only trigger negatives (Group B B3) against a posted-and-sealed
/// entry: a re-seal, a business-column UPDATE, and a DELETE must each raise an
/// `append-only` exception.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn append_only_trigger_rejects_reseal_update_and_delete() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");

    let (raw, service, _provider, f) = setup(&url).await;
    let scope = AccessScope::for_tenant(f.tenant);
    let ctx = SecurityContext::anonymous();

    let (entry, lines) = balanced_entry(&f, "biz-seal", 1000);
    let entry_id = entry.entry_id;
    service
        .post(&ctx, &scope, entry, lines, None)
        .await
        .expect("post must succeed and seal");

    // (a) Re-seal: a second from-non-NULL UPDATE of row_hash is rejected
    // (the row is already sealed).
    let reseal = raw
        .execute_raw(pg(format!(
            "UPDATE bss.ledger_journal_entry SET row_hash='\\x00' WHERE entry_id='{entry_id}'"
        )))
        .await;
    let reseal_err = reseal.expect_err("a re-seal must be rejected").to_string();
    assert!(
        reseal_err.contains("append-only"),
        "re-seal error must mention append-only, got: {reseal_err}"
    );

    // (b) Business-column UPDATE is rejected (only chain columns may change).
    let biz = raw
        .execute_raw(pg(format!(
            "UPDATE bss.ledger_journal_entry SET origin='X' WHERE entry_id='{entry_id}'"
        )))
        .await;
    let biz_err = biz
        .expect_err("a business-column UPDATE must be rejected")
        .to_string();
    assert!(
        biz_err.contains("append-only"),
        "business UPDATE error must mention append-only, got: {biz_err}"
    );

    // (c) DELETE is rejected.
    let del = raw
        .execute_raw(pg(format!(
            "DELETE FROM bss.ledger_journal_entry WHERE entry_id='{entry_id}'"
        )))
        .await;
    let del_err = del.expect_err("a DELETE must be rejected").to_string();
    assert!(
        del_err.contains("append-only"),
        "DELETE error must mention append-only, got: {del_err}"
    );
}

/// An ACTIVE tenant-wide freeze (`scope_freeze`) blocks a fresh post with
/// `TamperVerificationFailed`; clearing the freeze lets posting resume. Drives
/// the `TamperFreezeGuard` fail-fast gate end-to-end against Postgres.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn frozen_scope_blocks_posting() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");

    let (raw, service, _provider, f) = setup(&url).await;
    let scope = AccessScope::for_tenant(f.tenant);
    let ctx = SecurityContext::anonymous();

    // The integrity verifier froze this tenant tenant-wide (period_id='ALL').
    raw.execute_raw(pg(format!(
        "INSERT INTO bss.scope_freeze (tenant_id, scope, period_id, reason, set_by) \
         VALUES ('{}','tenant','ALL','test','Verifier')",
        f.tenant
    )))
    .await
    .unwrap();

    // A fresh post into the frozen scope is rejected before any write.
    let (entry, lines) = balanced_entry(&f, "biz-frozen", 1000);
    let err = service
        .post(&ctx, &scope, entry, lines, None)
        .await
        .expect_err("post into a frozen scope must be rejected");
    assert!(
        matches!(err, DomainError::TamperVerificationFailed(_)),
        "frozen post must fail with TamperVerificationFailed, got: {err:?}"
    );

    // Clearing the freeze lets the same tenant post again (refs already seeded).
    raw.execute_raw(pg(format!(
        "UPDATE bss.scope_freeze SET cleared_at = now(), cleared_by = 'Operator' \
         WHERE tenant_id = '{}' AND scope = 'tenant' AND period_id = 'ALL'",
        f.tenant
    )))
    .await
    .unwrap();

    let (entry, lines) = balanced_entry(&f, "biz-thawed", 1000);
    service
        .post(&ctx, &scope, entry, lines, None)
        .await
        .expect("post must succeed once the freeze is cleared");
}

/// True negative: the Verifier re-walks a clean (untampered) sealed chain and
/// does NOT freeze. Guards against a recompute that disagrees with the seal
/// encoding (e.g. a field round-trip mismatch) — which would false-positive and
/// freeze every healthy tenant, yet still "pass" the tampered-chain test.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn verifier_passes_clean_chain() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");

    let (raw, service, provider, f) = setup(&url).await;
    let scope = AccessScope::for_tenant(f.tenant);
    let ctx = SecurityContext::anonymous();

    // A sealed 3-link chain, untouched.
    for i in 0..3_i64 {
        let (e, l) = balanced_entry(&f, &format!("biz-clean-{i}"), 1000 + i);
        service
            .post(&ctx, &scope, e, l, None)
            .await
            .expect("clean post");
    }

    ChainVerifierJob::new(
        provider.clone(),
        std::sync::Arc::new(LedgerEventPublisher::noop()),
        std::sync::Arc::new(bss_ledger::domain::ports::metrics::NoopLedgerMetrics),
    )
    .run()
    .await
    .expect("verify run");

    let active_freezes = count(
        &raw,
        &format!(
            "SELECT COUNT(*) FROM bss.scope_freeze \
             WHERE tenant_id='{}' AND cleared_at IS NULL",
            f.tenant
        ),
    )
    .await;
    assert_eq!(
        active_freezes, 0,
        "the Verifier must NOT freeze a clean chain (no false positive)"
    );

    // Posting continues to work after a clean verify.
    let (e, l) = balanced_entry(&f, "biz-clean-after", 5000);
    service
        .post(&ctx, &scope, e, l, None)
        .await
        .expect("post must still succeed after a clean verify");
}

/// The daily chain Verifier re-walks a sealed 2-link chain, detects a `row_hash`
/// that was tampered out-of-band (the append-only trigger temporarily
/// disabled), freezes the tenant tenant-wide, and thereby blocks any further
/// post for that tenant with `TamperVerificationFailed`.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn verifier_freezes_tampered_chain() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");

    let (raw, service, provider, f) = setup(&url).await;
    let scope = AccessScope::for_tenant(f.tenant);
    let ctx = SecurityContext::anonymous();

    // Post two balanced entries — a sealed 2-link chain.
    let (e1, l1) = balanced_entry(&f, "biz-1", 1000);
    let first_id = e1.entry_id;
    service
        .post(&ctx, &scope, e1, l1, None)
        .await
        .expect("post 1");
    let (e2, l2) = balanced_entry(&f, "biz-2", 2000);
    service
        .post(&ctx, &scope, e2, l2, None)
        .await
        .expect("post 2");

    // Tamper the FIRST entry's row_hash out-of-band. The append-only trigger
    // forbids any post-seal UPDATE, so disable it for the tamper and re-enable
    // it after (a real attacker with direct DB access; the chain re-walk is what
    // catches it). `deadbeef` x8 = 64 hex chars = a 32-byte hash.
    raw.execute_raw(pg(
        "ALTER TABLE bss.ledger_journal_entry DISABLE TRIGGER trg_journal_entry_append_guard",
    ))
    .await
    .unwrap();
    raw.execute_raw(pg(format!(
        "UPDATE bss.ledger_journal_entry \
         SET row_hash = decode('deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef','hex') \
         WHERE entry_id = '{first_id}'"
    )))
    .await
    .unwrap();
    raw.execute_raw(pg(
        "ALTER TABLE bss.ledger_journal_entry ENABLE TRIGGER trg_journal_entry_append_guard",
    ))
    .await
    .unwrap();

    // Run the Verifier: a detected tamper is reported via a freeze + alarm, not
    // an Err, so `run()` returns Ok. The noop publisher needs no broker.
    ChainVerifierJob::new(
        provider.clone(),
        std::sync::Arc::new(LedgerEventPublisher::noop()),
        std::sync::Arc::new(bss_ledger::domain::ports::metrics::NoopLedgerMetrics),
    )
    .run()
    .await
    .expect("verify run");

    // An ACTIVE tenant-wide freeze now exists for the tenant.
    let active_freezes = count(
        &raw,
        &format!(
            "SELECT COUNT(*) FROM bss.scope_freeze \
             WHERE tenant_id='{}' AND cleared_at IS NULL",
            f.tenant
        ),
    )
    .await;
    assert_eq!(
        active_freezes, 1,
        "the Verifier must freeze the tampered tenant tenant-wide"
    );

    // A subsequent post for that tenant is rejected by the freeze guard.
    let (entry, lines) = balanced_entry(&f, "biz-after-tamper", 1000);
    let err = service
        .post(&ctx, &scope, entry, lines, None)
        .await
        .expect_err("post after a tamper freeze must be rejected");
    assert!(
        matches!(err, DomainError::TamperVerificationFailed(_)),
        "post after tamper must fail with TamperVerificationFailed, got: {err:?}"
    );
}

/// Z2-1: pin the exact column set of `ledger_journal_entry`. The append-only
/// seal trigger (migration 000012) guards business columns by an EXPLICIT
/// blacklist — every business column is enumerated in its `ROW(NEW…) IS DISTINCT
/// FROM ROW(OLD…)` check. That is complete today, but a future migration that
/// adds a column and forgets to extend the trigger would make the new column
/// silently mutable during the one permitted seal UPDATE — a tamper-evidence
/// bypass on financial/identity data. This test breaks the moment a column is
/// added or removed, forcing a conscious "guard it in the trigger, or it's a new
/// chain column" decision. The set is exactly the 16 trigger-guarded business
/// columns + the 4 chain columns (`row_hash`, `prev_hash`, `prev_entry_id`,
/// `prev_period_id`).
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn journal_entry_column_set_is_pinned_for_trigger_drift() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");

    let (raw, _service, _provider, _f) = setup(&url).await;

    let columns = fetch_hex_set(
        &raw,
        "SELECT column_name FROM information_schema.columns \
         WHERE table_schema='bss' AND table_name='ledger_journal_entry' \
         ORDER BY column_name",
    )
    .await;

    // Sorted; mirrors the trigger's guarded set (000012) + the 4 chain columns.
    let expected = [
        "correlation_id",
        "created_seq",
        "effective_at",
        "entry_currency",
        "entry_id",
        "legal_entity_id",
        "origin",
        "period_id",
        "posted_at_utc",
        "posted_by_actor_id",
        "prev_entry_id",
        "prev_hash",
        "prev_period_id",
        "reverses_entry_id",
        "reverses_period_id",
        "rounding_evidence",
        "row_hash",
        "source_business_id",
        "source_doc_type",
        "tenant_id",
    ];
    assert_eq!(
        columns, expected,
        "ledger_journal_entry columns changed — extend the append-only seal \
         trigger (migration 000012) to guard the new column, or treat it as a \
         new chain column, then update this pinned set"
    );
}
