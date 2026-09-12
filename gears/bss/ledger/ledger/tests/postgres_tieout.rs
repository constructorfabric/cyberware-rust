//! Postgres-only integration tests for `TieOutJob::tie_out_tenant` — the
//! per-tenant, all-time self-reconciliation. Boots a container, migrates, seeds
//! reference data + an OPEN period, posts ONE balanced entry through the real
//! `PostingService` (no-op publisher), then drives the four defect classes:
//!
//! * clean books tie out;
//! * an `account_balance` cache drift surfaces as one variance;
//! * a PENDING-mapped line is flagged;
//! * an imbalanced entry (inserted past the deferrable commit trigger) is
//!   caught by the entry-balance backstop.
//!
//! The seed/post harness is copied from `tests/postgres_posting.rs` (each
//! integration test is its own binary, so the helpers can't be shared).
//! Ignored by default; run with
//! `cargo test -p cf-gears-bss-ledger --test postgres_tieout -- --ignored`.

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

use bss_ledger::domain::instant::to_naive_date;
use bss_ledger::domain::model::{AccountRow, CurrencyScaleRow, NewEntry, NewLine};
use bss_ledger::domain::money::DEFAULT_PLAUSIBLE_MAX_MAJOR;
use bss_ledger::domain::payment::settlement::SettlementInput;
use bss_ledger::domain::ports::metrics::NoopLedgerMetrics;
use bss_ledger::infra::events::publisher::LedgerEventPublisher;
use bss_ledger::infra::jobs::tieout::TieOutJob;
use bss_ledger::infra::payment::allocate::{AllocateRequest, AllocationOutcome, AllocationService};
use bss_ledger::infra::payment::settle::SettlementService;
use bss_ledger::infra::posting::service::PostingService;
use bss_ledger::infra::storage::migrations::Migrator;
use bss_ledger::infra::storage::repo::{PaymentRepo, ReferenceRepo};
use bss_ledger_sdk::{AccountClass, MappingStatus, Side, SourceDocType};
use chrono::NaiveDate;
use sea_orm::{ConnectionTrait, Database, DatabaseConnection, Statement, TransactionTrait};
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

struct Fixture {
    tenant: Uuid,
    ar_account: Uuid,
    cash_account: Uuid,
    legal_entity: Uuid,
    period_id: String,
}

/// Boot, migrate, seed USD@2 + OPEN period + AR/CASH accounts; return the
/// migrate connection, the posting service over a search_path-scoped provider,
/// the provider itself, and the fixture ids. (Copied from
/// `tests/postgres_posting.rs::setup`.)
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

    let service = PostingService::new(provider.clone(), Arc::new(LedgerEventPublisher::noop()));
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
/// each `amount`. (Copied from `tests/postgres_posting.rs::balanced_entry`.)
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

/// Boot + seed + post ONE balanced entry; return the raw conn, the provider,
/// and the fixture. Shared by every test that needs clean populated books.
async fn setup_with_one_balanced_post(
    url: &str,
) -> (DatabaseConnection, DBProvider<DbError>, Fixture) {
    let (raw, service, provider, f) = setup(url).await;
    let scope = AccessScope::for_tenant(f.tenant);
    let ctx = SecurityContext::anonymous();
    let (entry, lines) = balanced_entry(&f, "biz-1", 1000);
    service
        .post(&ctx, &scope, entry, lines, None)
        .await
        .expect("balanced post must succeed");
    (raw, provider, f)
}

fn noop_publisher() -> Arc<LedgerEventPublisher> {
    Arc::new(LedgerEventPublisher::noop())
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn clean_tenant_ties_out() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");

    let (_raw, provider, f) = setup_with_one_balanced_post(&url).await;

    let report = TieOutJob::new(provider, noop_publisher())
        .tie_out_tenant(f.tenant)
        .await
        .expect("tie-out must succeed");

    assert!(
        report.is_clean(),
        "clean books must tie out: {}",
        report.summary()
    );
    assert!(report.account_balance_variances.is_empty());
    assert!(report.imbalanced_entries.is_empty());
    assert!(report.negative_grains.is_empty());
    assert_eq!(report.pending_lines, 0);
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn account_balance_drift_is_detected() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");

    let (raw, provider, f) = setup_with_one_balanced_post(&url).await;

    // Corrupt exactly one grain's cached balance (the AR row). `account_balance`
    // carries no append-only trigger, so a plain UPDATE is fine; the no-negative
    // CHECK is satisfied by adding to the positive AR balance.
    raw.execute_raw(pg(format!(
        "UPDATE bss.ledger_account_balance SET balance_minor = balance_minor + 1 \
         WHERE tenant_id='{}' AND account_id='{}' AND currency='USD'",
        f.tenant, f.ar_account
    )))
    .await
    .unwrap();

    let report = TieOutJob::new(provider, noop_publisher())
        .tie_out_tenant(f.tenant)
        .await
        .expect("tie-out must succeed");

    assert!(!report.is_clean(), "drifted books must not tie out");
    assert_eq!(
        report.account_balance_variances.len(),
        1,
        "exactly one grain diverged: {:?}",
        report.account_balance_variances
    );
    let v = &report.account_balance_variances[0];
    assert_ne!(v.computed, v.cached, "computed must differ from cached");
    assert_eq!(v.account_id, f.ar_account, "the AR grain diverged");
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn ar_sub_grain_drift_is_detected() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");

    // The balanced post's AR (DR) line populates `ar_payer_balance`. Corrupt
    // that cache (no append-only trigger; balance stays positive) and confirm
    // the sub-grain recompute — not just `account_balance` — catches it.
    let (raw, provider, f) = setup_with_one_balanced_post(&url).await;
    raw.execute_raw(pg(format!(
        "UPDATE bss.ledger_ar_payer_balance SET balance_minor = balance_minor + 1 \
         WHERE tenant_id='{}' AND account_id='{}'",
        f.tenant, f.ar_account
    )))
    .await
    .unwrap();

    let report = TieOutJob::new(provider, noop_publisher())
        .tie_out_tenant(f.tenant)
        .await
        .expect("tie-out must succeed");

    assert!(!report.is_clean(), "an AR sub-grain drift must not tie out");
    assert!(
        report
            .sub_grain_variances
            .iter()
            .any(|v| v.grain == "ar_payer_balance" && v.computed != v.cached),
        "the ar_payer_balance drift must surface as a sub-grain variance: {:?}",
        report.sub_grain_variances
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn pending_mapping_is_flagged() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");

    let (raw, provider, f) = setup_with_one_balanced_post(&url).await;

    // `journal_line` carries the append-only reject-mutation trigger
    // (BEFORE UPDATE), so a direct UPDATE is rejected. `session_replication_role
    // = replica` skips regular + constraint triggers (superuser; testcontainers
    // runs as `postgres`). It MUST be `SET LOCAL` inside a transaction so the
    // setting and the UPDATE share one pinned connection — `Database::connect`
    // is a pool, so a plain `SET` lands on a different connection than the DML.
    let txn = raw.begin().await.unwrap();
    txn.execute_raw(pg("SET LOCAL session_replication_role = replica"))
        .await
        .unwrap();
    txn.execute_raw(pg(format!(
        "UPDATE bss.ledger_journal_line SET mapping_status='PENDING' WHERE tenant_id='{}'",
        f.tenant
    )))
    .await
    .unwrap();
    txn.commit().await.unwrap();

    let report = TieOutJob::new(provider, noop_publisher())
        .tie_out_tenant(f.tenant)
        .await
        .expect("tie-out must succeed");

    assert!(report.pending_lines > 0, "PENDING lines must be counted");
    assert!(!report.is_clean(), "PENDING lines block a clean report");
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn entry_balance_backstop_catches_imbalanced_entry() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");

    // Reuse the migrate + provider boot, but DON'T post anything — we hand-craft
    // a malformed entry for a FRESH tenant below.
    let (raw, _service, provider, base) = setup(&url).await;

    // A brand-new tenant whose only entry is a single, unbalanced DR line.
    let tenant = Uuid::now_v7();
    let legal_entity = tenant;
    let period_id = base.period_id.clone();
    let account = Uuid::now_v7();
    let entry_id = Uuid::now_v7();
    let line_id = Uuid::now_v7();

    // Seed the account so the line's `normal_side` resolves on read (AR is
    // DR-normal). Use the repo so the secure-insert path is exercised.
    ReferenceRepo::new(provider.clone())
        .insert_account(AccountRow {
            account_id: account,
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

    // BYPASS the P1 deferrable balanced-entry constraint trigger (a DEFERRABLE
    // CONSTRAINT TRIGGER AFTER INSERT, fired at COMMIT) so a deliberately
    // imbalanced entry can land: a single DR line with no offsetting CR, so
    // SUM(DR) - SUM(CR) = +500 (net != 0). `session_replication_role = replica`
    // skips regular + constraint triggers (superuser; testcontainers runs as
    // `postgres`). It MUST be `SET LOCAL` inside the SAME transaction as the
    // inserts so the setting holds through the deferred-trigger fire at COMMIT
    // and shares one pinned connection (`Database::connect` is a pool).
    let txn = raw.begin().await.unwrap();
    txn.execute_raw(pg("SET LOCAL session_replication_role = replica"))
        .await
        .unwrap();
    txn.execute_raw(pg(format!(
        "INSERT INTO bss.ledger_journal_entry \
            (entry_id, tenant_id, legal_entity_id, period_id, entry_currency, \
             source_doc_type, source_business_id, posted_at_utc, effective_at, \
             origin, posted_by_actor_id, correlation_id) \
         VALUES ('{entry_id}','{tenant}','{legal_entity}','{period_id}','USD', \
             'MANUAL_ADJUSTMENT','biz-bad', now(), DATE '2026-06-01', \
             'SYSTEM','{tenant}','{tenant}')"
    )))
    .await
    .unwrap();
    txn.execute_raw(pg(format!(
        "INSERT INTO bss.ledger_journal_line \
            (line_id, entry_id, tenant_id, period_id, payer_tenant_id, account_id, \
             account_class, side, amount_minor, currency, currency_scale, mapping_status) \
         VALUES ('{line_id}','{entry_id}','{tenant}','{period_id}','{tenant}','{account}', \
             'AR','DR',500,'USD',2,'RESOLVED')"
    )))
    .await
    .unwrap();
    txn.commit().await.unwrap();

    let report = TieOutJob::new(provider, noop_publisher())
        .tie_out_tenant(tenant)
        .await
        .expect("tie-out must succeed");

    assert!(!report.is_clean(), "an imbalanced entry must not tie out");
    assert!(
        !report.imbalanced_entries.is_empty(),
        "the entry-balance backstop must flag the malformed entry"
    );
    let imbalanced = report
        .imbalanced_entries
        .iter()
        .find(|e| e.entry_id == entry_id)
        .expect("the malformed entry must be among the imbalanced entries");
    assert_ne!(imbalanced.net_minor, 0, "net DR-CR must be non-zero");
}

/// The entry-balance backstop also catches a BALANCED entry whose lines span
/// more than one payer (`payer_count > 1`) — the app-level safety net for the
/// case the DB single-payer trigger is bypassed. The existing test exercises
/// only `net_minor != 0`; this pins the mixed-payer arm.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn entry_backstop_catches_mixed_payer_entry() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");

    let (raw, _service, provider, base) = setup(&url).await;

    let tenant = Uuid::now_v7();
    let legal_entity = tenant;
    let period_id = base.period_id.clone();
    let account = Uuid::now_v7();
    let entry_id = Uuid::now_v7();
    let payer_a = Uuid::now_v7();
    let payer_b = Uuid::now_v7();

    ReferenceRepo::new(provider.clone())
        .insert_account(AccountRow {
            account_id: account,
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

    // BYPASS the deferrable single-payer/balance trigger (replica role) so a
    // BALANCED but cross-payer entry can land: DR 500 (payer A) + CR 500
    // (payer B). net = 0 (NOT imbalanced), but two distinct payers.
    let txn = raw.begin().await.unwrap();
    txn.execute_raw(pg("SET LOCAL session_replication_role = replica"))
        .await
        .unwrap();
    txn.execute_raw(pg(format!(
        "INSERT INTO bss.ledger_journal_entry \
            (entry_id, tenant_id, legal_entity_id, period_id, entry_currency, \
             source_doc_type, source_business_id, posted_at_utc, effective_at, \
             origin, posted_by_actor_id, correlation_id) \
         VALUES ('{entry_id}','{tenant}','{legal_entity}','{period_id}','USD', \
             'MANUAL_ADJUSTMENT','biz-mixed', now(), DATE '2026-06-01', \
             'SYSTEM','{tenant}','{tenant}')"
    )))
    .await
    .unwrap();
    for (payer, side) in [(payer_a, "DR"), (payer_b, "CR")] {
        txn.execute_raw(pg(format!(
            "INSERT INTO bss.ledger_journal_line \
                (line_id, entry_id, tenant_id, period_id, payer_tenant_id, account_id, \
                 account_class, side, amount_minor, currency, currency_scale, mapping_status) \
             VALUES ('{}','{entry_id}','{tenant}','{period_id}','{payer}','{account}', \
                 'AR','{side}',500,'USD',2,'RESOLVED')",
            Uuid::now_v7()
        )))
        .await
        .unwrap();
    }
    txn.commit().await.unwrap();

    let report = TieOutJob::new(provider, noop_publisher())
        .tie_out_tenant(tenant)
        .await
        .expect("tie-out must succeed");

    assert!(!report.is_clean(), "a mixed-payer entry must not tie out");
    let flagged = report
        .imbalanced_entries
        .iter()
        .find(|e| e.entry_id == entry_id)
        .expect("the mixed-payer entry must be flagged by the backstop");
    assert_eq!(
        flagged.payer_count, 2,
        "two distinct payers must be reported"
    );
    assert_eq!(
        flagged.net_minor, 0,
        "the entry is balanced — flagged for mixed-payer, not imbalance"
    );
}

/// `run()` (the cross-tenant sweep) reaching its `EntryImbalance` AND
/// `NegativeBalanceViolation` emit arms — the two emit dispatches the existing
/// `tieout_tests::run_over_drifted_tenant_emits_alarm` does NOT exercise (it
/// drives only the `TieOutVariance` arm via an `account_balance` cache drift).
///
/// One tenant is seeded with BOTH defect classes at once, then swept by
/// `run()`: (a) a guarded `account_balance` grain driven NEGATIVE (the
/// no-negative backstop's target) and (b) a single-DR-line imbalanced entry (the
/// entry-balance backstop's target). Both require the `session_replication_role
/// = replica` superuser bypass to land — the SAME established technique the
/// `entry_balance_backstop_catches_imbalanced_entry` test uses, simulating the
/// real failure tie-out exists to catch (a bypassed/buggy projector or DB CHECK
/// letting a malformed state through). `run()` must complete `Ok` (it
/// logs/alarms, never errors) and `tie_out_tenant` independently confirms BOTH
/// defect classes are present — proving the `run()` loop reached both emit arms.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn run_emits_negative_grain_and_entry_imbalance_arms() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");

    // Migrate + provider boot; seed a FRESH tenant by hand (no clean post — this
    // tenant exists only to carry the two defects).
    let (raw, _service, provider, _base) = setup(&url).await;

    let tenant = Uuid::now_v7();
    let legal_entity = tenant;
    let period_id = "202606".to_owned();
    let guarded_account = Uuid::now_v7(); // AR — goes negative (a defect)
    let bad_entry_account = Uuid::now_v7(); // AR — carries the lone unbalanced DR line
    let bad_entry_id = Uuid::now_v7();

    // The OPEN period so the would-be post target exists (and the tenant is
    // enumerated). Seed the two AR accounts via the secure repo.
    raw.execute_raw(pg(format!(
        "INSERT INTO bss.ledger_fiscal_period (tenant_id, legal_entity_id, period_id, fiscal_tz, status)
         VALUES ('{tenant}','{legal_entity}','{period_id}','UTC','OPEN')"
    )))
    .await
    .unwrap();
    let reference = ReferenceRepo::new(provider.clone());
    // Two distinct AR probe accounts. The CoA uniqueness index is
    // (tenant, legal_entity, account_class, currency, COALESCE(revenue_stream,'-')),
    // so two AR/USD accounts must differ on `revenue_stream` to coexist. The stream
    // only differentiates the registration row; the injected cache/line carry their
    // own values, so tie-out detection (negative grain on `guarded_account`,
    // imbalanced entry on `bad_entry_account`) is unaffected.
    for (account_id, revenue_stream) in [
        (guarded_account, None),
        (bad_entry_account, Some("imbalance-probe".to_owned())),
    ] {
        reference
            .insert_account(AccountRow {
                account_id,
                tenant_id: tenant,
                legal_entity_id: legal_entity,
                account_class: "AR".to_owned(),
                currency: "USD".to_owned(),
                revenue_stream,
                normal_side: "DR".to_owned(),
                may_go_negative: false,
                lifecycle_state: "OPEN".to_owned(),
            })
            .await
            .unwrap();
    }

    // BOTH defects land under one replica-role txn: `session_replication_role =
    // replica` disables the deferrable balanced-entry constraint *trigger* for the
    // unbalanced line (b). It does NOT, however, disable CHECK constraints, so the
    // negative-grain injection (a) additionally drops the no-negative CHECK — this
    // simulates the cache-drift / bypassed-CHECK scenario the tie-out's independent
    // no-negative backstop is meant to catch (app-level detection is unaffected).
    let txn = raw.begin().await.unwrap();
    txn.execute_raw(pg("SET LOCAL session_replication_role = replica"))
        .await
        .unwrap();
    // (a) A NEGATIVE guarded (AR) account_balance grain — the no-negative
    // backstop's `NegativeBalanceViolation` defect. Drop the DB CHECK first
    // (replica role does not skip CHECKs); the container is throwaway.
    txn.execute_raw(pg("ALTER TABLE bss.ledger_account_balance \
         DROP CONSTRAINT IF EXISTS chk_account_balance_no_negative"))
        .await
        .unwrap();
    txn.execute_raw(pg(format!(
        "INSERT INTO bss.ledger_account_balance \
            (tenant_id, account_id, currency, account_class, normal_side, balance_minor) \
         VALUES ('{tenant}','{guarded_account}','USD','AR','DR',-500)"
    )))
    .await
    .unwrap();
    // (b) A single, unbalanced DR line (net +700) — the entry-balance backstop's
    // `EntryImbalance` defect.
    txn.execute_raw(pg(format!(
        "INSERT INTO bss.ledger_journal_entry \
            (entry_id, tenant_id, legal_entity_id, period_id, entry_currency, \
             source_doc_type, source_business_id, posted_at_utc, effective_at, \
             origin, posted_by_actor_id, correlation_id) \
         VALUES ('{bad_entry_id}','{tenant}','{legal_entity}','{period_id}','USD', \
             'MANUAL_ADJUSTMENT','biz-imbalance', now(), DATE '2026-06-01', \
             'SYSTEM','{tenant}','{tenant}')"
    )))
    .await
    .unwrap();
    txn.execute_raw(pg(format!(
        "INSERT INTO bss.ledger_journal_line \
            (line_id, entry_id, tenant_id, period_id, payer_tenant_id, account_id, \
             account_class, side, amount_minor, currency, currency_scale, mapping_status) \
         VALUES ('{}','{bad_entry_id}','{tenant}','{period_id}','{tenant}','{bad_entry_account}', \
             'AR','DR',700,'USD',2,'RESOLVED')",
        Uuid::now_v7()
    )))
    .await
    .unwrap();
    txn.commit().await.unwrap();

    // The cross-tenant sweep completes Ok even with a doubly-defective tenant —
    // it logs/alarms (reaching the EntryImbalance + NegativeBalanceViolation emit
    // arms), never errors.
    TieOutJob::new(provider.clone(), noop_publisher())
        .run()
        .await
        .expect("run must complete Ok even over a defective tenant");

    // Independently confirm BOTH defect classes are present — the report the
    // sweep folded into its two emit dispatches.
    let report = TieOutJob::new(provider, noop_publisher())
        .tie_out_tenant(tenant)
        .await
        .expect("tie-out must succeed");
    assert!(
        !report.is_clean(),
        "the doubly-defective tenant must not tie out"
    );
    assert!(
        report
            .negative_grains
            .iter()
            .any(|g| g.account_id == guarded_account && g.balance_minor == -500),
        "the negative guarded AR grain must be flagged: {:?}",
        report.negative_grains
    );
    let imbalanced = report
        .imbalanced_entries
        .iter()
        .find(|e| e.entry_id == bad_entry_id)
        .expect("the imbalanced entry must be flagged");
    assert_eq!(imbalanced.net_minor, 700, "net DR-CR is +700");
}

// ── Payment-counter reconcile through a REAL settle + allocate ───────────────
//
// Drives the real `SettlementService` + `AllocationService` so the
// `payment_settlement` / `payment_allocation` rows + the `PAYMENT_SETTLE`
// journal all materialise, then ties out the tenant through the FULL
// `tie_out_tenant` (which wires `recompute_payment_counter_variances` over those
// real rows). A `setup_seller` mirrors `postgres_payments.rs`: USD@2, an OPEN
// period for the CURRENT month (settle/allocate derive `period_id` from
// `OffsetDateTime::now_utc()`), and the four payment-flow chart accounts.

struct Seller {
    tenant: Uuid,
    payer: Uuid,
    cash: Uuid,
    unallocated: Uuid,
    psp_fee: Uuid,
    ar: Uuid,
    period_id: String,
}

fn seller_account(tenant: Uuid, id: Uuid, class: AccountClass, normal: Side) -> AccountRow {
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

async fn setup_seller(raw: &DatabaseConnection, provider: &DBProvider<DbError>) -> Seller {
    let now = OffsetDateTime::now_utc();
    let s = Seller {
        tenant: Uuid::now_v7(),
        payer: Uuid::now_v7(),
        cash: Uuid::now_v7(),
        unallocated: Uuid::now_v7(),
        psp_fee: Uuid::now_v7(),
        ar: Uuid::now_v7(),
        period_id: bss_ledger::domain::instant::yyyymm(now),
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
        seller_account(s.tenant, s.cash, AccountClass::CashClearing, Side::Debit),
        seller_account(
            s.tenant,
            s.unallocated,
            AccountClass::Unallocated,
            Side::Credit,
        ),
        seller_account(
            s.tenant,
            s.psp_fee,
            AccountClass::PspFeeExpense,
            Side::Debit,
        ),
        seller_account(s.tenant, s.ar, AccountClass::Ar, Side::Debit),
    ] {
        reference.insert_account(row).await.unwrap();
    }
    s
}

/// Seed an OPEN AR invoice by posting `DR AR (invoice_id) / CR PSP_FEE_EXPENSE`
/// directly (PSP_FEE is unguarded, so the CR from zero is allowed). Mirrors
/// `postgres_payments.rs::seed_ar_invoice`.
async fn seed_ar_invoice(
    provider: &DBProvider<DbError>,
    s: &Seller,
    invoice_id: &str,
    amount: i64,
) {
    let posting = PostingService::new(provider.clone(), noop_publisher());
    let ctx = SecurityContext::anonymous();
    let scope = AccessScope::for_tenant(s.tenant);
    let entry = NewEntry {
        entry_id: Uuid::now_v7(),
        tenant_id: s.tenant,
        legal_entity_id: s.tenant,
        period_id: s.period_id.clone(),
        entry_currency: "USD".to_owned(),
        source_doc_type: SourceDocType::InvoicePost,
        source_business_id: invoice_id.to_owned(),
        reverses_entry_id: None,
        reverses_period_id: None,
        posted_at_utc: OffsetDateTime::now_utc(),
        effective_at: to_naive_date(OffsetDateTime::now_utc()),
        origin: "SYSTEM".to_owned(),
        posted_by_actor_id: s.tenant,
        correlation_id: Uuid::now_v7(),
        rounding_evidence: serde_json::Value::Null,
        rate_snapshot_ref: None,
    };
    let ar_line = NewLine {
        line_id: Uuid::now_v7(),
        payer_tenant_id: s.payer,
        seller_tenant_id: Some(s.tenant),
        resource_tenant_id: None,
        account_id: s.ar,
        account_class: AccountClass::Ar,
        gl_code: None,
        side: Side::Debit,
        amount_minor: amount,
        currency: "USD".to_owned(),
        currency_scale: 2,
        invoice_id: Some(invoice_id.to_owned()),
        due_date: Some(NaiveDate::from_ymd_opt(2026, 12, 1).unwrap()),
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
    };
    let psp_credit = NewLine {
        account_id: s.psp_fee,
        account_class: AccountClass::PspFeeExpense,
        side: Side::Credit,
        invoice_id: None,
        due_date: None,
        line_id: Uuid::now_v7(),
        ..ar_line.clone()
    };
    posting
        .post(&ctx, &scope, entry, vec![ar_line, psp_credit], None)
        .await
        .expect("seed AR invoice post must succeed");
}

/// A clean ledger built from a REAL settle + allocate ties out (the payment-
/// counter reconcile runs over the real `payment_settlement` /
/// `payment_allocation` rows and the `PAYMENT_SETTLE` journal, and passes);
/// corrupting the cached `allocated_minor` then surfaces exactly one
/// `PaymentCounterVariance` — proving `recompute_payment_counter_variances` is
/// wired through the full `tie_out_tenant`, not just unit-tested in memory.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn payment_counter_reconcile_through_full_tie_out() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");

    let (raw, _service, provider, _base) = setup(&url).await;
    let s = setup_seller(&raw, &provider).await;
    let ctx = SecurityContext::anonymous();
    let scope = AccessScope::for_tenant(s.tenant);

    // Settle gross=1000 fee=30 (DR CASH 970 / DR PSP_FEE 30 / CR UNALLOCATED 1000),
    // seed an open AR invoice (300), then allocate 300 onto it.
    let settle = SettlementService::new(
        provider.clone(),
        noop_publisher(),
        Arc::new(NoopLedgerMetrics),
    );
    settle
        .settle(
            &ctx,
            &scope,
            SettlementInput {
                tenant_id: s.tenant,
                payer_tenant_id: s.payer,
                payment_id: "PAY-TIE-1".to_owned(),
                gross_minor: 1000,
                fee_minor: 30,
                currency: "USD".to_owned(),
                effective_at: None,
            },
        )
        .await
        .expect("settle must succeed");
    seed_ar_invoice(&provider, &s, "INV-TIE", 300).await;
    let allocate = AllocationService::new(
        provider.clone(),
        noop_publisher(),
        Arc::new(NoopLedgerMetrics),
    );
    let outcome = allocate
        .allocate(
            &ctx,
            &scope,
            AllocateRequest {
                tenant_id: s.tenant,
                payer_tenant_id: s.payer,
                payment_id: "PAY-TIE-1".to_owned(),
                allocation_id: Uuid::now_v7(),
                lump_minor: 300,
                currency: "USD".to_owned(),
                hint_invoice_id: None,
                caller_splits: None,
            },
        )
        .await
        .expect("allocate must succeed");
    assert!(
        matches!(outcome, AllocationOutcome::Applied(_)),
        "a settled payment allocates inline"
    );

    // Clean books tie out — the payment-counter reconcile ran and passed
    // (settled/fee/allocated all agree with the journal + allocation rows).
    let clean = TieOutJob::new(provider.clone(), noop_publisher())
        .tie_out_tenant(s.tenant)
        .await
        .expect("tie-out must succeed");
    assert!(
        clean.is_clean(),
        "a real settle+allocate must tie out clean: {}",
        clean.summary()
    );
    assert!(clean.payment_counter_variances.is_empty());

    // Corrupt the cached `allocated_minor` (the allocation rows are the truth, so
    // the recompute now disagrees). `payment_settlement` carries no append-only
    // trigger, so a plain UPDATE suffices.
    let repo = PaymentRepo::new(provider.clone());
    let before = repo
        .read_settlement(&scope, s.tenant, "PAY-TIE-1")
        .await
        .unwrap()
        .expect("settlement row present");
    assert_eq!(
        before.allocated_minor, 300,
        "allocated counter seeded to 300"
    );
    raw.execute_raw(pg(format!(
        "UPDATE bss.ledger_payment_settlement SET allocated_minor = 250 \
         WHERE tenant_id='{}' AND payment_id='PAY-TIE-1'",
        s.tenant
    )))
    .await
    .unwrap();

    let drifted = TieOutJob::new(provider, noop_publisher())
        .tie_out_tenant(s.tenant)
        .await
        .expect("tie-out must succeed");
    assert!(!drifted.is_clean(), "the counter drift must not tie out");
    let v = drifted
        .payment_counter_variances
        .iter()
        .find(|v| v.payment_id == "PAY-TIE-1" && v.counter == "allocated_minor")
        .expect("the allocated_minor counter must diverge");
    assert_eq!(
        (v.computed, v.cached),
        (300, 250),
        "computed (rows=300) vs corrupted cache (250)"
    );
}

/// Incremental tie-out over a REUSABLE_CREDIT grain across a CLOSED + OPEN period
/// split — exercises the wallet arms of `fold_grains` / `cache_grains` and
/// `IncrementalReport::into_tie_out_report`, which the existing incremental tests
/// (account / unallocated grains only) do not reach. Posts `DR CASH / CR
/// REUSABLE_CREDIT` in period 1, closes it + snapshots the baseline (what
/// period-close does), posts another wallet credit in period 2, then verifies the
/// incremental path (baseline[p1] + open-fold[p2]) reconciles clean, agrees with
/// the full fold, and adapts to a clean `TieOutReport`.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn incremental_tie_out_covers_reusable_credit_grain() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");

    let (raw, service, provider, f) = setup(&url).await;
    let ctx = SecurityContext::anonymous();
    let scope = AccessScope::for_tenant(f.tenant);

    // A CR-normal REUSABLE_CREDIT wallet account; the fixture's CASH (CR-normal)
    // would net negative on a DR, so add a DR-normal CASH-style account to offset
    // the wallet credit. Reuse the fixture's AR (DR-normal, guarded) as the debit
    // leg: DR AR raises it (non-negative), CR REUSABLE_CREDIT raises the wallet.
    let wallet = Uuid::now_v7();
    ReferenceRepo::new(provider.clone())
        .insert_account(AccountRow {
            account_id: wallet,
            tenant_id: f.tenant,
            legal_entity_id: f.legal_entity,
            account_class: "REUSABLE_CREDIT".to_owned(),
            currency: "USD".to_owned(),
            revenue_stream: None,
            normal_side: "CR".to_owned(),
            may_go_negative: false,
            lifecycle_state: "OPEN".to_owned(),
        })
        .await
        .unwrap();

    // A balanced DR AR / CR REUSABLE_CREDIT wallet credit for `period`, with the
    // wallet sub-grain bucket (`credit_grant_event_type`) the projector requires.
    let wallet_entry = |business_id: &str, period: &str, effective: NaiveDate, amount: i64| {
        let entry = NewEntry {
            entry_id: Uuid::now_v7(),
            tenant_id: f.tenant,
            legal_entity_id: f.legal_entity,
            period_id: period.to_owned(),
            entry_currency: "USD".to_owned(),
            source_doc_type: SourceDocType::ManualAdjustment,
            source_business_id: business_id.to_owned(),
            reverses_entry_id: None,
            reverses_period_id: None,
            posted_at_utc: OffsetDateTime::now_utc(),
            effective_at: effective,
            origin: "SYSTEM".to_owned(),
            posted_by_actor_id: f.tenant,
            correlation_id: f.tenant,
            rounding_evidence: serde_json::Value::Null,
            rate_snapshot_ref: None,
        };
        let ar_debit = line(&f, f.ar_account, AccountClass::Ar, Side::Debit, amount);
        let mut wallet_credit = line(
            &f,
            wallet,
            AccountClass::ReusableCredit,
            Side::Credit,
            amount,
        );
        wallet_credit.credit_grant_event_type = Some("promo".to_owned());
        (entry, vec![ar_debit, wallet_credit])
    };

    // Period 1 (the fixture's 202606): credit 1000 into the wallet, then close +
    // snapshot the baseline (mirrors period-close).
    let (e1, l1) = wallet_entry(
        "wallet-p1",
        &f.period_id,
        NaiveDate::from_ymd_opt(2026, 6, 1).unwrap(),
        1000,
    );
    service.post(&ctx, &scope, e1, l1, None).await.unwrap();
    raw.execute_raw(pg(format!(
        "UPDATE bss.ledger_fiscal_period SET status='CLOSED' \
         WHERE tenant_id='{}' AND period_id='{}'",
        f.tenant, f.period_id
    )))
    .await
    .unwrap();
    let conn = provider.conn().unwrap();
    let job = TieOutJob::new(provider.clone(), noop_publisher());
    job.snapshot_baseline(&conn, f.tenant, &f.period_id)
        .await
        .expect("snapshot baseline at close");

    // Period 2: open it, credit another 400 into the wallet (a second event-type
    // bucket would also work; the same bucket keeps the grain a single key).
    raw.execute_raw(pg(format!(
        "INSERT INTO bss.ledger_fiscal_period (tenant_id, legal_entity_id, period_id, fiscal_tz, status) \
         VALUES ('{}','{}','202607','UTC','OPEN')",
        f.tenant, f.legal_entity
    )))
    .await
    .unwrap();
    let (e2, l2) = wallet_entry(
        "wallet-p2",
        "202607",
        NaiveDate::from_ymd_opt(2026, 7, 1).unwrap(),
        400,
    );
    service.post(&ctx, &scope, e2, l2, None).await.unwrap();

    // Incremental reconciles clean: baseline[p1] + open-fold[p2] == cache[all],
    // INCLUDING the reusable_credit wallet grain (folded + projected through the
    // wallet arms of `fold_grains` / `cache_grains`).
    let inc = job
        .tie_out_incremental(&conn, f.tenant)
        .await
        .unwrap()
        .expect("a stored baseline ⇒ the incremental path runs");
    assert!(
        inc.is_clean(),
        "incremental clean on a consistent wallet ledger: {:?}",
        inc.sub_grain_variances
    );

    // The full all-time fold agrees (and the wallet sub-grain ties out there too).
    let full = job.tie_out_on(&conn, f.tenant).await.unwrap();
    assert!(
        full.is_clean(),
        "the full fold is clean too: {}",
        full.summary()
    );

    // The wallet cache carries the all-time total (1000 + 400 = 1400) — confirms
    // both credits projected onto the reusable_credit sub-grain.
    let wallet_balance = raw
        .query_one_raw(pg(format!(
            "SELECT balance_minor FROM bss.ledger_reusable_credit_subbalance \
             WHERE tenant_id='{}' AND account_id='{wallet}' AND credit_grant_event_type='promo'",
            f.tenant
        )))
        .await
        .unwrap()
        .map(|r| r.try_get_by_index::<i64>(0).unwrap());
    assert_eq!(wallet_balance, Some(1400), "wallet sub-grain = 1000 + 400");

    // `into_tie_out_report` adapts the clean incremental result to a clean
    // `TieOutReport` (open-line count carried; full-only defect classes empty).
    let report = inc.into_tie_out_report(f.tenant);
    assert!(report.is_clean(), "the adapted report is clean");
    assert_eq!(report.tenant_id, f.tenant);
    assert!(
        report.posted_line_count >= 2,
        "the open-period fold carried period-2's lines: {}",
        report.posted_line_count
    );
}
