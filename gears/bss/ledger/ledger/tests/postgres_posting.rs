//! Postgres-only end-to-end: the `PostingService` ACID transaction. Boots
//! a container, migrates, seeds reference data via repos + raw SQL, then
//! drives the full post sequence: a balanced post updates the truth tables
//! and derived caches and stamps the dedup row; a re-post replays; a closed
//! period and a negative-balance post are rejected with the right codes.
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

use std::sync::Arc;

use bss_ledger::domain::error::DomainError;
use bss_ledger::domain::model::{AccountRow, CurrencyScaleRow, EntryKey, NewEntry, NewLine};
use bss_ledger::domain::money::DEFAULT_PLAUSIBLE_MAX_MAJOR;
use bss_ledger::infra::events::publisher::LedgerEventPublisher;
use bss_ledger::infra::jobs::tieout::TieOutJob;
use bss_ledger::infra::period_close::PeriodCloseService;
use bss_ledger::infra::posting::service::{PostSidecar, PostedFacts, PostingService};
use bss_ledger::infra::storage::entity::unallocated_balance;
use bss_ledger::infra::storage::migrations::Migrator;
use bss_ledger::infra::storage::repo::{JournalRepo, ReferenceRepo};
use bss_ledger_sdk::{AccountClass, MappingStatus, Side, SourceDocType};
use chrono::NaiveDate;
use sea_orm::sea_query::Expr;
use sea_orm::{
    ActiveValue::Set, ConnectionTrait, Database, DatabaseConnection, EntityTrait, Statement,
};
use sea_orm_migration::MigratorTrait;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use time::OffsetDateTime;
use toolkit_db::secure::{AccessScope, SecureInsertExt, SecureOnConflict};
use toolkit_db::{ConnectOpts, DBProvider, DbError, connect_db};
use toolkit_security::SecurityContext;
use uuid::Uuid;

fn pg(sql: impl Into<String>) -> Statement {
    Statement::from_string(sea_orm::DatabaseBackend::Postgres, sql.into())
}

/// Scalar i64 read of a single-column, single-row SELECT (bss-qualified).
async fn scalar_i64(conn: &DatabaseConnection, sql: &str) -> Option<i64> {
    let row = conn.query_one_raw(pg(sql.to_owned())).await.unwrap();
    row.map(|r| r.try_get_by_index::<i64>(0).unwrap())
}

/// Count rows matching a bss-qualified predicate.
async fn count(conn: &DatabaseConnection, sql: &str) -> i64 {
    scalar_i64(conn, sql).await.unwrap_or(0)
}

struct Fixture {
    tenant: Uuid,
    ar_account: Uuid,
    cash_account: Uuid,
    legal_entity: Uuid,
    period_id: String,
}

/// Boot, migrate, seed USD@2 + OPEN period + AR/CASH accounts; return the
/// migrate connection, the service over a search_path-scoped provider, and
/// the fixture ids.
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

    // OPEN fiscal period (raw, bss-qualified).
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
/// each `amount`. Pass `swap` to flip the sides (DR CASH / CR AR) so the
/// entry nets AR negative.
fn balanced_entry(
    f: &Fixture,
    business_id: &str,
    amount: i64,
    swap: bool,
) -> (NewEntry, Vec<NewLine>) {
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
    let (ar_side, cash_side) = if swap {
        (Side::Credit, Side::Debit)
    } else {
        (Side::Debit, Side::Credit)
    };
    let lines = vec![
        line(f, f.ar_account, AccountClass::Ar, ar_side, amount),
        line(
            f,
            f.cash_account,
            AccountClass::CashClearing,
            cash_side,
            amount,
        ),
    ];
    (entry, lines)
}

fn line(f: &Fixture, account: Uuid, class: AccountClass, side: Side, amount: i64) -> NewLine {
    NewLine {
        line_id: Uuid::now_v7(),
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
        ar_status: None,
    }
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn post_balanced_replay_period_and_negative() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");

    let (raw, service, _provider, f) = setup(&url).await;
    let scope = AccessScope::for_tenant(f.tenant);
    let ctx = SecurityContext::anonymous();

    // --- 1. Balanced post: DR AR 1000 / CR CASH 1000 ---
    let (entry, lines) = balanced_entry(&f, "biz-1", 1000, false);
    let first_entry_id = entry.entry_id;
    let posted = service
        .post(&ctx, &scope, entry, lines, None)
        .await
        .expect("post must succeed");
    assert!(!posted.replayed, "first post is not a replay");
    assert!(posted.created_seq > 0, "created_seq must be positive");
    assert_eq!(posted.entry_id, first_entry_id);

    // account_balance: AR=1000 (DR-normal, DR delta), CASH=1000 (CR-normal, CR delta).
    let ar_bal = scalar_i64(
        &raw,
        &format!(
            "SELECT balance_minor FROM bss.ledger_account_balance \
             WHERE tenant_id='{}' AND account_id='{}' AND currency='USD'",
            f.tenant, f.ar_account
        ),
    )
    .await;
    assert_eq!(ar_bal, Some(1000), "AR balance");
    let cash_bal = scalar_i64(
        &raw,
        &format!(
            "SELECT balance_minor FROM bss.ledger_account_balance \
             WHERE tenant_id='{}' AND account_id='{}' AND currency='USD'",
            f.tenant, f.cash_account
        ),
    )
    .await;
    assert_eq!(cash_bal, Some(1000), "CASH balance");

    // ar_payer_balance = 1000.
    let payer_bal = scalar_i64(
        &raw,
        &format!(
            "SELECT balance_minor FROM bss.ledger_ar_payer_balance \
             WHERE tenant_id='{}' AND payer_tenant_id='{}' AND account_id='{}' AND currency='USD'",
            f.tenant, f.tenant, f.ar_account
        ),
    )
    .await;
    assert_eq!(payer_bal, Some(1000), "ar_payer_balance");

    // idempotency_dedup.result_entry_id populated.
    let dedup_rows = count(
        &raw,
        &format!(
            "SELECT COUNT(*) FROM bss.ledger_idempotency_dedup \
             WHERE tenant_id='{}' AND business_id='biz-1' AND result_entry_id IS NOT NULL",
            f.tenant
        ),
    )
    .await;
    assert_eq!(dedup_rows, 1, "dedup row finalized");

    // 2 journal_line rows for the entry.
    let line_count = count(
        &raw,
        &format!("SELECT COUNT(*) FROM bss.ledger_journal_line WHERE entry_id='{first_entry_id}'"),
    )
    .await;
    assert_eq!(line_count, 2, "two journal lines");

    // --- 2. Replay: same business key + payload ---
    let (entry2, lines2) = balanced_entry(&f, "biz-1", 1000, false);
    let replay = service
        .post(&ctx, &scope, entry2, lines2, None)
        .await
        .expect("replay must succeed");
    assert!(replay.replayed, "second post is a replay");
    assert_eq!(
        replay.entry_id, first_entry_id,
        "replay returns the prior entry id"
    );

    // Still exactly one journal_entry + two lines for biz-1 (no duplicate).
    let entry_count = count(
        &raw,
        &format!(
            "SELECT COUNT(*) FROM bss.ledger_journal_entry \
             WHERE tenant_id='{}' AND source_business_id='biz-1'",
            f.tenant
        ),
    )
    .await;
    assert_eq!(entry_count, 1, "no duplicate journal entry on replay");
    let line_count2 = count(
        &raw,
        &format!("SELECT COUNT(*) FROM bss.ledger_journal_line WHERE entry_id='{first_entry_id}'"),
    )
    .await;
    assert_eq!(line_count2, 2, "no duplicate journal lines on replay");

    // --- 3. Closed period ---
    raw.execute_raw(pg(format!(
        "UPDATE bss.ledger_fiscal_period SET status='CLOSED' \
         WHERE tenant_id='{}' AND legal_entity_id='{}' AND period_id='{}'",
        f.tenant, f.legal_entity, f.period_id
    )))
    .await
    .unwrap();
    let (entry3, lines3) = balanced_entry(&f, "biz-closed", 500, false);
    let closed_err = service
        .post(&ctx, &scope, entry3, lines3, None)
        .await
        .expect_err("closed period rejects");
    assert!(
        matches!(closed_err, DomainError::PeriodClosed(_)),
        "got {closed_err:?}"
    );

    // Re-open for the next case.
    raw.execute_raw(pg(format!(
        "UPDATE bss.ledger_fiscal_period SET status='OPEN' \
         WHERE tenant_id='{}' AND legal_entity_id='{}' AND period_id='{}'",
        f.tenant, f.legal_entity, f.period_id
    )))
    .await
    .unwrap();

    // --- 4. Negative balance: DR CASH 1500 / CR AR 1500 drives AR to -500 ---
    let (entry4, lines4) = balanced_entry(&f, "biz-neg", 1500, true);
    let neg_err = service
        .post(&ctx, &scope, entry4, lines4, None)
        .await
        .expect_err("negative balance rejects");
    assert!(
        matches!(neg_err, DomainError::NegativeBalance(_)),
        "got {neg_err:?}"
    );

    // AR balance unchanged (the negative post rolled back).
    let ar_after = scalar_i64(
        &raw,
        &format!(
            "SELECT balance_minor FROM bss.ledger_account_balance \
             WHERE tenant_id='{}' AND account_id='{}' AND currency='USD'",
            f.tenant, f.ar_account
        ),
    )
    .await;
    assert_eq!(ar_after, Some(1000), "AR balance unchanged after rollback");
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn concurrent_same_key_posts_once() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");

    let (raw, service, _provider, f) = setup(&url).await;
    let scope = AccessScope::for_tenant(f.tenant);

    // Two posts of the SAME business key, run concurrently on two service
    // clones sharing the provider.
    let svc_a = service.clone();
    let svc_b = service.clone();
    let scope_a = scope.clone();
    let scope_b = scope.clone();
    let ctx_a = SecurityContext::anonymous();
    let ctx_b = SecurityContext::anonymous();
    let (e_a, l_a) = balanced_entry(&f, "biz-conc", 1000, false);
    let (e_b, l_b) = balanced_entry(&f, "biz-conc", 1000, false);

    let (ra, rb) = tokio::join!(
        async move { svc_a.post(&ctx_a, &scope_a, e_a, l_a, None).await },
        async move { svc_b.post(&ctx_b, &scope_b, e_b, l_b, None).await },
    );

    // Both succeed (one fresh, one replay) OR one succeeds + one is a
    // conflict; in every case at most one journal_entry must persist.
    let entry_count = count(
        &raw,
        &format!(
            "SELECT COUNT(*) FROM bss.ledger_journal_entry \
             WHERE tenant_id='{}' AND source_business_id='biz-conc'",
            f.tenant
        ),
    )
    .await;
    assert_eq!(
        entry_count, 1,
        "exactly one entry persisted for the shared key"
    );

    let oks = i32::from(ra.is_ok()) + i32::from(rb.is_ok());
    assert!(
        oks >= 1,
        "at least one concurrent post must succeed: {ra:?} / {rb:?}"
    );

    // Every successful result (fresh OR replay) must carry a real, shared entry
    // id: a concurrent replay returns the winner's finalized id, never the nil
    // UUID (regression for the replay-nil-id path).
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
        assert_eq!(a, b, "fresh post and replay must reference the same entry");
    }
}

/// SecureORM SQL-level isolation (authz #3): an entry posted under tenant A is
/// invisible to a tenant-B scope, even though the key triple names A's row —
/// the tenant predicate is enforced in the query, not just at the PEP gate.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn secure_orm_isolates_cross_tenant_entry_reads() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");

    let (_raw, service, provider, f) = setup(&url).await;
    let ctx = SecurityContext::anonymous();
    let scope_a = AccessScope::for_tenant(f.tenant);

    let (entry, lines) = balanced_entry(&f, "biz-iso", 1000, false);
    let entry_id = entry.entry_id;
    let period_id = entry.period_id.clone();
    service
        .post(&ctx, &scope_a, entry, lines, None)
        .await
        .expect("post under A");

    let journal = JournalRepo::new(provider.clone());
    let key = |tenant| EntryKey {
        entry_id,
        tenant_id: tenant,
        period_id: period_id.clone(),
    };

    // A foreign tenant B (its own scope) reading A's exact key gets nothing.
    let tenant_b = Uuid::now_v7();
    let found_b = journal
        .find_entry(&AccessScope::for_tenant(tenant_b), key(f.tenant))
        .await
        .expect("query ok");
    assert!(
        found_b.is_none(),
        "tenant B must not read tenant A's entry through SecureORM"
    );

    // Sanity: A sees its own entry under its own scope.
    let found_a = journal
        .find_entry(&scope_a, key(f.tenant))
        .await
        .expect("query ok");
    assert!(found_a.is_some(), "tenant A must see its own entry");
}

/// A1 seam: a `REVERSAL` post carrying `reverses_entry_id` / `reverses_period_id`
/// persists those header columns. Posts an original `INVOICE_POST`-style entry,
/// then a balanced reversal (flipped sides) whose header points back at the
/// original via the `reverses_*` fields, and reads the reversal row back through
/// the repo to assert the columns round-trip. (The SDK `PostEntry.reverses_*` →
/// gear `NewEntry.reverses_*` copy is threaded in `LedgerLocalClient`; this
/// proves the gear-internal + storage half of that seam.)
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn reverses_fields_persist_on_a_reversal_post() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");

    let (raw, service, provider, f) = setup(&url).await;
    let ctx = SecurityContext::anonymous();
    let scope = AccessScope::for_tenant(f.tenant);

    // Original entry: DR AR 2000 / CR CASH 2000. This is an A1 SEAM test — it
    // asserts the `reverses_*` header columns round-trip, NOT balance mechanics.
    // The reversal below is a full (2000) flip that nets both guarded grains back
    // to exactly zero — the zero-boundary upsert path a guarded net-down must
    // survive (Postgres checks the no-negative CHECK against the INSERT arbiter
    // tuple, so the projector seeds the projected post-state, not the bare delta).
    let (orig, orig_lines) = balanced_entry(&f, "inv-rev-1", 2000, false);
    let original_entry_id = orig.entry_id;
    service
        .post(&ctx, &scope, orig, orig_lines, None)
        .await
        .expect("original post must succeed");

    // Reversal: flipped sides (DR CASH / CR AR 2000), header points back at the
    // original via reverses_entry_id / reverses_period_id; nets AR/CASH to 0.
    let (mut reversal, reversal_lines) = balanced_entry(&f, "reverses=inv-rev-1", 2000, true);
    reversal.source_doc_type = SourceDocType::Reversal;
    reversal.reverses_entry_id = Some(original_entry_id);
    reversal.reverses_period_id = Some(f.period_id.clone());
    let reversal_entry_id = reversal.entry_id;
    service
        .post(&ctx, &scope, reversal, reversal_lines, None)
        .await
        .expect("reversal post must succeed");

    // The guarded balances net back to exactly zero (the zero-boundary the
    // arbiter-tuple seed must permit).
    let ar_bal = scalar_i64(
        &raw,
        &format!(
            "SELECT balance_minor FROM bss.ledger_account_balance \
             WHERE tenant_id='{}' AND account_id='{}' AND currency='USD'",
            f.tenant, f.ar_account
        ),
    )
    .await;
    assert_eq!(ar_bal, Some(0), "AR nets to zero after the full reversal");
    let cash_bal = scalar_i64(
        &raw,
        &format!(
            "SELECT balance_minor FROM bss.ledger_account_balance \
             WHERE tenant_id='{}' AND account_id='{}' AND currency='USD'",
            f.tenant, f.cash_account
        ),
    )
    .await;
    assert_eq!(
        cash_bal,
        Some(0),
        "CASH nets to zero after the full reversal"
    );

    // Read the reversal entry back and assert the reverses_* columns.
    let record = JournalRepo::new(provider)
        .find_entry(
            &scope,
            EntryKey {
                entry_id: reversal_entry_id,
                tenant_id: f.tenant,
                period_id: f.period_id.clone(),
            },
        )
        .await
        .expect("read reversal entry")
        .expect("reversal entry must exist");
    assert_eq!(
        record.reverses_entry_id,
        Some(original_entry_id),
        "reverses_entry_id must persist the original entry id"
    );
    assert_eq!(
        record.reverses_period_id,
        Some(f.period_id.clone()),
        "reverses_period_id must persist the original period"
    );
    assert_eq!(record.source_doc_type, "REVERSAL");
}

/// Concurrent overdraw (financial #3): two posts racing the same guarded
/// (`may_go_negative = false`) AR account that together would drive it
/// negative. At most one may commit; the DB no-negative CHECK must abort the
/// other so the balance never goes below zero — proving the app-level guard's
/// lockless read is backstopped at the database.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn concurrent_overdraw_of_guarded_account_stays_non_negative() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");

    let (raw, service, _provider, f) = setup(&url).await;
    let ctx = SecurityContext::anonymous();
    let scope = AccessScope::for_tenant(f.tenant);

    // Seed a positive AR balance: DR AR 1000.
    let (e0, l0) = balanced_entry(&f, "seed", 1000, false);
    service
        .post(&ctx, &scope, e0, l0, None)
        .await
        .expect("seed post");

    // Two concurrent posts, each CR AR 600 (swap) — together -1200 would take
    // AR to -200.
    let svc_a = service.clone();
    let svc_b = service.clone();
    let sa = scope.clone();
    let sb = scope.clone();
    let ca = SecurityContext::anonymous();
    let cb = SecurityContext::anonymous();
    let (ea, la) = balanced_entry(&f, "ov-a", 600, true);
    let (eb, lb) = balanced_entry(&f, "ov-b", 600, true);
    let (ra, rb) = tokio::join!(
        async move { svc_a.post(&ca, &sa, ea, la, None).await },
        async move { svc_b.post(&cb, &sb, eb, lb, None).await },
    );

    let oks = i32::from(ra.is_ok()) + i32::from(rb.is_ok());
    assert!(
        oks <= 1,
        "at most one overdraw may commit against the guarded account: {ra:?} / {rb:?}"
    );
    let ar = scalar_i64(
        &raw,
        &format!(
            "SELECT balance_minor FROM bss.ledger_account_balance \
             WHERE tenant_id='{}' AND account_id='{}'",
            f.tenant, f.ar_account
        ),
    )
    .await;
    assert!(
        ar.unwrap_or(0) >= 0,
        "guarded AR balance must never be negative, got {ar:?}"
    );
}

/// #4(A): a post and a period close racing the SAME period must not leave a
/// CLOSED period that an entry slipped into unseen. Both run SERIALIZABLE+retry,
/// so Postgres SSI aborts the loser; whatever the interleaving, a CLOSED period
/// must tie out clean (close's pre-close tie-out saw every committed entry).
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn concurrent_close_never_certifies_a_period_a_post_landed_in() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");

    let (raw, service, provider, f) = setup(&url).await;
    let publisher = Arc::new(LedgerEventPublisher::noop());
    let close_svc = PeriodCloseService::new(
        provider.clone(),
        Arc::clone(&publisher),
        std::sync::Arc::new(
            bss_ledger::infra::audit::secured_audit_sink::NoopSecuredAuditSink::new(),
        ),
    );

    // Race: post a balanced entry INTO the period, and close the period.
    let svc = service.clone();
    let scope = AccessScope::for_tenant(f.tenant);
    let ctx = SecurityContext::anonymous();
    let (entry, lines) = balanced_entry(&f, "race", 1000, false);
    let (tenant, legal_entity, period) = (f.tenant, f.legal_entity, f.period_id.clone());

    let (post_res, close_res) = tokio::join!(
        async move { svc.post(&ctx, &scope, entry, lines, None).await },
        async move {
            close_svc
                .close(&SecurityContext::anonymous(), tenant, legal_entity, period)
                .await
        },
    );

    // At least one side must make progress.
    assert!(
        post_res.is_ok() || close_res.is_ok(),
        "post={post_res:?} / close={close_res:?}"
    );

    let status = raw
        .query_one_raw(pg(format!(
            "SELECT status FROM bss.ledger_fiscal_period \
             WHERE tenant_id='{}' AND period_id='{}'",
            f.tenant, f.period_id
        )))
        .await
        .unwrap()
        .expect("period row exists");
    let status: String = status.try_get("", "status").unwrap();

    // THE INVARIANT: a CLOSED period must tie out clean. If the post committed
    // into it, close's SERIALIZABLE pre-close tie-out must have seen the entry
    // (else the race certified a period with an unverified entry).
    if status == "CLOSED" {
        let report = TieOutJob::new(provider.clone(), Arc::clone(&publisher))
            .tie_out_tenant(f.tenant)
            .await
            .unwrap();
        assert!(
            report.is_clean(),
            "a CLOSED period must tie out clean (post={post_res:?} / close={close_res:?}); \
             report={}",
            report.summary()
        );
    }
}

/// B1 sidecar — writes a marker row through SecureORM inside the post txn,
/// stamping the posted sequence into an `unallocated_balance` counter row keyed
/// by the posted tenant/payer/account. Proves the sidecar's SecureORM write
/// commits atomically with the journal entry.
struct MarkerSidecar {
    tenant: Uuid,
    payer: Uuid,
    account: Uuid,
}

#[async_trait::async_trait]
impl PostSidecar for MarkerSidecar {
    async fn run(
        &self,
        txn: &toolkit_db::secure::DbTx<'_>,
        scope: &AccessScope,
        posted: &PostedFacts,
    ) -> Result<(), DomainError> {
        // Stamp the posted sequence into a counter row (balance_minor) so the
        // test can read it back and confirm atomic commit with the entry.
        let am = unallocated_balance::ActiveModel {
            tenant_id: Set(self.tenant),
            payer_tenant_id: Set(self.payer),
            account_id: Set(self.account),
            currency: Set("USD".to_owned()),
            balance_minor: Set(posted.created_seq),
            functional_balance_minor: Set(None),
            functional_currency: Set(None),
            last_entry_seq: Set(Some(posted.created_seq)),
            version: Set(0),
        };
        // ON CONFLICT must carry an action (a bare conflict target emits
        // invalid SQL before RETURNING); a retry re-stamps the counter row.
        let on_conflict = SecureOnConflict::<unallocated_balance::Entity>::columns([
            // `ledger_unallocated_balance` PK = (tenant_id, payer_tenant_id,
            // currency); account_id is NOT a key column (the unallocated pool is
            // per payer+currency), so it must not be in the ON CONFLICT target.
            unallocated_balance::Column::TenantId,
            unallocated_balance::Column::PayerTenantId,
            unallocated_balance::Column::Currency,
        ])
        .value(
            unallocated_balance::Column::BalanceMinor,
            Expr::value(posted.created_seq),
        )
        .and_then(|oc| {
            oc.value(
                unallocated_balance::Column::LastEntrySeq,
                Expr::value(Some(posted.created_seq)),
            )
        })
        .map_err(|e| DomainError::Internal(format!("marker on_conflict: {e}")))?;
        unallocated_balance::Entity::insert(am.clone())
            .secure()
            .scope_with_model(scope, &am)
            .map_err(|e| DomainError::Internal(format!("marker scope: {e}")))?
            .on_conflict(on_conflict)
            .exec_with_returning(txn)
            .await
            .map_err(|e| DomainError::Internal(format!("marker insert: {e}")))?;
        Ok(())
    }
}

/// B1 sidecar — always rejects, so the whole post (entry + lines + projection)
/// must roll back and the error surfaces to the caller.
struct RejectingSidecar;

#[async_trait::async_trait]
impl PostSidecar for RejectingSidecar {
    async fn run(
        &self,
        _txn: &toolkit_db::secure::DbTx<'_>,
        _scope: &AccessScope,
        _posted: &PostedFacts,
    ) -> Result<(), DomainError> {
        Err(DomainError::InvalidRequest("sidecar rejected".to_owned()))
    }
}

/// B1: a post carrying an `Ok` sidecar commits the sidecar's marker row
/// atomically with the journal entry; a post carrying an `Err` sidecar rolls
/// the entire entry back and surfaces the rejection.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn post_sidecar_commits_with_entry_and_rolls_back_on_err() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");

    let (raw, service, _provider, f) = setup(&url).await;
    let scope = AccessScope::for_tenant(f.tenant);
    let ctx = SecurityContext::anonymous();

    // The marker sidecar stamps a counter row into unallocated_balance keyed by
    // a fresh account id (cache rows carry no FK to the chart).
    let marker_account = Uuid::now_v7();
    let marker = || {
        Arc::new(MarkerSidecar {
            tenant: f.tenant,
            payer: f.tenant,
            account: marker_account,
        })
    };

    // --- 1. Ok sidecar: the marker row commits with the entry ---
    let (entry, lines) = balanced_entry(&f, "sc-ok", 1000, false);
    let posted = service
        .post(&ctx, &scope, entry, lines, Some(marker()))
        .await
        .expect("post with ok sidecar must succeed");

    let marker_seq = scalar_i64(
        &raw,
        &format!(
            "SELECT balance_minor FROM bss.ledger_unallocated_balance \
             WHERE tenant_id='{}' AND payer_tenant_id='{}' AND account_id='{marker_account}' \
             AND currency='USD'",
            f.tenant, f.tenant
        ),
    )
    .await;
    assert_eq!(
        marker_seq,
        Some(posted.created_seq),
        "sidecar marker row visible after commit, stamping the posted seq"
    );

    // --- 2. Err sidecar: the entry rolls back and the error surfaces ---
    let (entry2, lines2) = balanced_entry(&f, "sc-err", 500, false);
    let entry2_id = entry2.entry_id;
    let err = service
        .post(
            &ctx,
            &scope,
            entry2,
            lines2,
            Some(Arc::new(RejectingSidecar)),
        )
        .await
        .expect_err("rejecting sidecar must roll the post back");
    assert!(
        matches!(err, DomainError::InvalidRequest(_)),
        "sidecar rejection surfaces as its DomainError: {err:?}"
    );

    // The journal entry did NOT persist (rolled back with the sidecar Err).
    let entry_rows = count(
        &raw,
        &format!("SELECT COUNT(*) FROM bss.ledger_journal_entry WHERE entry_id='{entry2_id}'"),
    )
    .await;
    assert_eq!(entry_rows, 0, "rejected post must not persist its entry");
}

/// The same DR AR / CR CASH balanced entry as `balanced_entry`, but with a
/// functional amount on BOTH lines (a cross-currency post): it balances in the
/// transaction column (`amount`) AND the functional column (`functional`).
fn cross_currency_entry(
    f: &Fixture,
    business_id: &str,
    amount: i64,
    functional: i64,
) -> (NewEntry, Vec<NewLine>) {
    let (entry, mut lines) = balanced_entry(f, business_id, amount, false);
    for l in &mut lines {
        l.functional_amount_minor = Some(functional);
        l.functional_currency = Some("USD".to_owned());
    }
    (entry, lines)
}

/// Slice 5 B1: a cross-currency post (a functional amount on every line) projects
/// `functional_balance_minor` / `functional_currency` onto the balance caches. The
/// entry balances in BOTH columns; the projector mirrors the transaction sign onto
/// the functional column (AR is DR-normal, CASH is CR-normal, both +functional).
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn cross_currency_post_populates_functional_balance() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");

    let (raw, service, _provider, f) = setup(&url).await;
    let scope = AccessScope::for_tenant(f.tenant);
    let ctx = SecurityContext::anonymous();

    // DR AR 1000 (func 1100) / CR CASH 1000 (func 1100): balances in both columns.
    let (entry, lines) = cross_currency_entry(&f, "fx-1", 1000, 1100);
    service
        .post(&ctx, &scope, entry, lines, None)
        .await
        .expect("cross-currency post must succeed (balances in both columns)");

    for (label, account) in [("AR", f.ar_account), ("CASH", f.cash_account)] {
        let func = scalar_i64(
            &raw,
            &format!(
                "SELECT functional_balance_minor FROM bss.ledger_account_balance \
                 WHERE tenant_id='{}' AND account_id='{}' AND currency='USD'",
                f.tenant, account
            ),
        )
        .await;
        assert_eq!(
            func,
            Some(1100),
            "{label} functional_balance_minor populated"
        );
        let ccy = count(
            &raw,
            &format!(
                "SELECT COUNT(*) FROM bss.ledger_account_balance \
                 WHERE tenant_id='{}' AND account_id='{}' AND functional_currency='USD'",
                f.tenant, account
            ),
        )
        .await;
        assert_eq!(ccy, 1, "{label} functional_currency stamped");
    }

    // ar_payer_balance carries the functional value too.
    let payer_func = scalar_i64(
        &raw,
        &format!(
            "SELECT functional_balance_minor FROM bss.ledger_ar_payer_balance \
             WHERE tenant_id='{}' AND payer_tenant_id='{}' AND account_id='{}' AND currency='USD'",
            f.tenant, f.tenant, f.ar_account
        ),
    )
    .await;
    assert_eq!(
        payer_func,
        Some(1100),
        "ar_payer functional_balance_minor populated"
    );
}

/// `post_with_request_hash` is the orchestrator entry point that keys the dedup
/// claim on an externally-computed REQUEST hash (stable across the
/// state-dependent entry rebuild) instead of the entry-derived payload hash. The
/// FIRST post under `(flow, business_id)` + request-hash `A` finalizes; a SECOND
/// post that REUSES the same `(flow, business_id)` but supplies a DIFFERENT
/// request-hash `B` must be rejected `IdempotencyConflict` — the
/// same-key/different-payload guard a payment orchestrator's replay short-circuit
/// relies on (the in-txn `row.payload_hash != payload_hash` arm). A genuine
/// reuse-with-changed-payload is a real client error, not a replay.
///
/// The two posts carry DIFFERENT entries (distinct `entry_id`s and amounts), so
/// the entry-derived hash would differ too — but it is the REQUEST hash that is
/// authoritative here, so the conflict is driven purely by `A != B` under the
/// shared business key, and the rejected second post leaves no second entry.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn post_with_request_hash_rejects_same_key_different_payload() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");

    let (raw, service, _provider, f) = setup(&url).await;
    let scope = AccessScope::for_tenant(f.tenant);
    let ctx = SecurityContext::anonymous();

    // First post under business key "biz-rh" keyed on request-hash A.
    let (entry_a, lines_a) = balanced_entry(&f, "biz-rh", 1000, false);
    let first_entry_id = entry_a.entry_id;
    let posted = service
        .post_with_request_hash(&ctx, &scope, entry_a, lines_a, None, "A".repeat(64))
        .await
        .expect("first request-hash post must succeed");
    assert!(!posted.replayed, "first post is fresh");

    // Second post REUSES the same (flow, business_id) "biz-rh" but a DIFFERENT
    // request-hash B ⇒ IdempotencyConflict (same key, different payload).
    let (entry_b, lines_b) = balanced_entry(&f, "biz-rh", 500, false);
    let second_entry_id = entry_b.entry_id;
    let err = service
        .post_with_request_hash(&ctx, &scope, entry_b, lines_b, None, "B".repeat(64))
        .await
        .expect_err("same key + different request hash must conflict");
    assert!(
        matches!(err, DomainError::IdempotencyConflict(_)),
        "expected IdempotencyConflict, got {err:?}"
    );

    // Exactly one journal_entry for the shared key — the conflicting post never
    // persisted its (distinct) entry.
    let entry_count = count(
        &raw,
        &format!(
            "SELECT COUNT(*) FROM bss.ledger_journal_entry \
             WHERE tenant_id='{}' AND source_business_id='biz-rh'",
            f.tenant
        ),
    )
    .await;
    assert_eq!(entry_count, 1, "the conflicting post added no second entry");
    let first_rows = count(
        &raw,
        &format!("SELECT COUNT(*) FROM bss.ledger_journal_entry WHERE entry_id='{first_entry_id}'"),
    )
    .await;
    assert_eq!(first_rows, 1, "the original entry is intact");
    let second_rows = count(
        &raw,
        &format!(
            "SELECT COUNT(*) FROM bss.ledger_journal_entry WHERE entry_id='{second_entry_id}'"
        ),
    )
    .await;
    assert_eq!(second_rows, 0, "the rejected post's entry never landed");
}

/// A re-post via `post_with_request_hash` that reuses the same `(flow,
/// business_id)` AND the same request-hash is an idempotent REPLAY (not a
/// conflict): it returns the prior finalized entry id and writes no second
/// entry. This pins the matching-hash replay arm of the request-hash path (the
/// sibling of the conflict arm above) — the orchestrator's safe retry.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn post_with_request_hash_same_hash_replays() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");

    let (raw, service, _provider, f) = setup(&url).await;
    let scope = AccessScope::for_tenant(f.tenant);
    let ctx = SecurityContext::anonymous();

    let hash = "c".repeat(64);
    let (entry, lines) = balanced_entry(&f, "biz-rh-replay", 1000, false);
    let first_entry_id = entry.entry_id;
    let first = service
        .post_with_request_hash(&ctx, &scope, entry, lines, None, hash.clone())
        .await
        .expect("first request-hash post must succeed");
    assert!(!first.replayed, "first is fresh");

    // Re-issue with the SAME key and SAME request-hash (the entry is rebuilt with
    // a fresh entry_id, exactly as an orchestrator retry would) ⇒ replay.
    let (entry2, lines2) = balanced_entry(&f, "biz-rh-replay", 1000, false);
    let replay = service
        .post_with_request_hash(&ctx, &scope, entry2, lines2, None, hash)
        .await
        .expect("same key + same request hash replays");
    assert!(replay.replayed, "second is an idempotent replay");
    assert_eq!(
        replay.entry_id, first_entry_id,
        "replay returns the prior finalized entry id"
    );

    let entry_count = count(
        &raw,
        &format!(
            "SELECT COUNT(*) FROM bss.ledger_journal_entry \
             WHERE tenant_id='{}' AND source_business_id='biz-rh-replay'",
            f.tenant
        ),
    )
    .await;
    assert_eq!(entry_count, 1, "replay adds no second entry");
}

/// Slice 5 B1: a single-currency post leaves `functional_balance_minor` NULL —
/// the plain `+ functional_delta` on conflict keeps NULL = NULL (no COALESCE), so
/// existing single-currency grains never spuriously gain a functional value.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn single_currency_post_leaves_functional_null() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");

    let (raw, service, _provider, f) = setup(&url).await;
    let scope = AccessScope::for_tenant(f.tenant);
    let ctx = SecurityContext::anonymous();

    let (entry, lines) = balanced_entry(&f, "sc-only", 1000, false);
    service
        .post(&ctx, &scope, entry, lines, None)
        .await
        .expect("single-currency post must succeed");

    // The AR account_balance row exists (balance 1000) but its functional column
    // is NULL — a row with a non-NULL functional would NOT match this predicate.
    let ar_func_null = count(
        &raw,
        &format!(
            "SELECT COUNT(*) FROM bss.ledger_account_balance \
             WHERE tenant_id='{}' AND account_id='{}' AND functional_balance_minor IS NULL",
            f.tenant, f.ar_account
        ),
    )
    .await;
    assert_eq!(ar_func_null, 1, "single-currency AR functional stays NULL");
}

/// A held `tenant_posting_lock` (design §3.2 pre-transaction gate) refuses every
/// post for the tenant with `TenantPostingLocked`, before any write.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn tenant_posting_lock_blocks_posting() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");

    let (raw, service, _provider, f) = setup(&url).await;
    let scope = AccessScope::for_tenant(f.tenant);
    let ctx = SecurityContext::anonymous();

    // Set the kill switch for the tenant.
    raw.execute_raw(pg(format!(
        "INSERT INTO bss.ledger_tenant_posting_lock (tenant_id, locked, reason_code, set_at) \
         VALUES ('{}', true, 'TENANT_TERMINATED', now())",
        f.tenant
    )))
    .await
    .unwrap();

    // A balanced post that would otherwise succeed is refused.
    let (entry, lines) = balanced_entry(&f, "biz-locked", 1000, false);
    let err = service
        .post(&ctx, &scope, entry, lines, None)
        .await
        .expect_err("a locked tenant must be refused");
    assert!(
        matches!(err, DomainError::TenantPostingLocked(_)),
        "got {err:?}"
    );

    // No entry was written (the gate is pre-transaction).
    let entries = count(
        &raw,
        &format!(
            "SELECT COUNT(*) FROM bss.ledger_journal_entry WHERE tenant_id='{}'",
            f.tenant
        ),
    )
    .await;
    assert_eq!(entries, 0, "a refused post writes nothing");

    // Clearing the lock lets the same tenant post again.
    raw.execute_raw(pg(format!(
        "UPDATE bss.ledger_tenant_posting_lock SET locked = false, cleared_at = now() \
         WHERE tenant_id = '{}'",
        f.tenant
    )))
    .await
    .unwrap();
    let (entry, lines) = balanced_entry(&f, "biz-unlocked", 1000, false);
    service
        .post(&ctx, &scope, entry, lines, None)
        .await
        .expect("post must succeed once the lock is cleared");
}

/// A post whose `posted_at_utc` is skewed beyond ±24 h from the server clock is
/// quarantined with `ClockSkewQuarantine` (design §3.2), before any write.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn clock_skew_beyond_24h_is_quarantined() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");

    let (raw, service, _provider, f) = setup(&url).await;
    let scope = AccessScope::for_tenant(f.tenant);
    let ctx = SecurityContext::anonymous();

    let (mut entry, lines) = balanced_entry(&f, "biz-skewed", 1000, false);
    entry.posted_at_utc = OffsetDateTime::now_utc() - time::Duration::hours(48);
    let err = service
        .post(&ctx, &scope, entry, lines, None)
        .await
        .expect_err("a >±24h skewed post must be quarantined");
    assert!(
        matches!(err, DomainError::ClockSkewQuarantine(_)),
        "got {err:?}"
    );

    let entries = count(
        &raw,
        &format!(
            "SELECT COUNT(*) FROM bss.ledger_journal_entry WHERE tenant_id='{}'",
            f.tenant
        ),
    )
    .await;
    assert_eq!(entries, 0, "a quarantined post writes nothing");
}
