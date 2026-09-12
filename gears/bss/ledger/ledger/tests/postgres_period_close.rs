//! Postgres-only integration tests for `PeriodCloseService::close` — the
//! minimal `OPEN→CLOSED` transition gated by a synchronous pre-close tie-out.
//! Boots a container, migrates, seeds reference data + an OPEN period, posts
//! ONE balanced entry through the real `PostingService` (no-op publisher), then:
//!
//! * a cache drift blocks the close (`PeriodCloseBlocked`, tie-out reason) and the
//!   period stays `OPEN`;
//! * a clean close succeeds (`already_closed = false`) and is idempotent on a
//!   re-close (`already_closed = true`), with the row left `CLOSED`;
//! * closing an unknown period is `PeriodNotFound`.
//!
//! The seed/post harness is copied from `tests/postgres_posting.rs` (each
//! integration test is its own binary, so the helpers can't be shared).
//! Ignored by default; run with
//! `cargo test -p cf-gears-bss-ledger --test postgres_period_close -- --ignored`.

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
use bss_ledger::domain::model::{AccountRow, CurrencyScaleRow, NewEntry, NewLine};
use bss_ledger::domain::money::DEFAULT_PLAUSIBLE_MAX_MAJOR;
use bss_ledger::infra::events::publisher::LedgerEventPublisher;
use bss_ledger::infra::period_close::PeriodCloseService;
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

/// Read the `status` of a `(tenant, le, period)` fiscal-period row.
async fn period_status(
    db: &DatabaseConnection,
    tenant: Uuid,
    legal_entity: Uuid,
    period_id: &str,
) -> String {
    let row = db
        .query_one_raw(pg(format!(
            "SELECT status FROM bss.ledger_fiscal_period \
             WHERE tenant_id='{tenant}' AND legal_entity_id='{legal_entity}' \
               AND period_id='{period_id}'"
        )))
        .await
        .unwrap()
        .expect("fiscal_period row must exist");
    row.try_get::<String>("", "status").unwrap()
}

struct Fixture {
    tenant: Uuid,
    ar_account: Uuid,
    cash_account: Uuid,
    legal_entity: Uuid,
    period_id: String,
}

/// Boot, migrate, seed USD@2 + OPEN period + AR/CASH accounts; return the
/// migrate connection, the posting service, the provider, and the fixture ids.
/// (Copied from `tests/postgres_posting.rs::setup`.)
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

/// Build a balanced entry for `fixture`: DR AR / CR CASH, each `amount`.
/// (Copied from `tests/postgres_posting.rs::balanced_entry`.)
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
/// and the fixture.
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
async fn close_blocked_by_tieout_variance() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");

    let (raw, provider, f) = setup_with_one_balanced_post(&url).await;

    // Drift one cached grain so the pre-close tie-out fails.
    raw.execute_raw(pg(format!(
        "UPDATE bss.ledger_account_balance SET balance_minor = balance_minor + 1 \
         WHERE tenant_id='{}' AND account_id='{}' AND currency='USD'",
        f.tenant, f.ar_account
    )))
    .await
    .unwrap();

    let err = PeriodCloseService::new(
        provider,
        noop_publisher(),
        std::sync::Arc::new(
            bss_ledger::infra::audit::secured_audit_sink::NoopSecuredAuditSink::new(),
        ),
    )
    .close(
        &SecurityContext::anonymous(),
        f.tenant,
        f.legal_entity,
        f.period_id.clone(),
    )
    .await
    .expect_err("a drifted tenant must block the close");
    // Group B unified the gate: a tie-out defect is one accumulated blocked reason
    // on `PeriodCloseBlocked` (design §4.5 blocked_reasons), not a bare
    // `PreCloseTieOutFailed`.
    assert!(
        matches!(&err, DomainError::PeriodCloseBlocked(d) if d.contains("tie-out")),
        "got {err:?}"
    );

    // The period must remain OPEN (the flip never ran).
    let status = period_status(&raw, f.tenant, f.legal_entity, &f.period_id).await;
    assert_eq!(status, "OPEN", "blocked close leaves the period OPEN");
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn clean_close_succeeds_and_is_idempotent() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");

    let (raw, provider, f) = setup_with_one_balanced_post(&url).await;
    let service = PeriodCloseService::new(
        provider,
        noop_publisher(),
        std::sync::Arc::new(
            bss_ledger::infra::audit::secured_audit_sink::NoopSecuredAuditSink::new(),
        ),
    );

    // --- First close: clean books -> OPEN→CLOSED. ---
    let outcome = service
        .close(
            &SecurityContext::anonymous(),
            f.tenant,
            f.legal_entity,
            f.period_id.clone(),
        )
        .await
        .expect("a clean close must succeed");
    assert!(!outcome.already_closed, "first close is a fresh close");
    assert_eq!(outcome.period_id, f.period_id);
    let status = period_status(&raw, f.tenant, f.legal_entity, &f.period_id).await;
    assert_eq!(status, "CLOSED", "the period is now CLOSED");

    // --- Re-close: idempotent no-op on the already-CLOSED period. ---
    let outcome2 = service
        .close(
            &SecurityContext::anonymous(),
            f.tenant,
            f.legal_entity,
            f.period_id.clone(),
        )
        .await
        .expect("a re-close must succeed (idempotent)");
    assert!(outcome2.already_closed, "re-close reports already_closed");
    let status2 = period_status(&raw, f.tenant, f.legal_entity, &f.period_id).await;
    assert_eq!(status2, "CLOSED", "the period stays CLOSED");
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn close_unknown_period_is_not_found() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");

    let (_raw, provider, f) = setup_with_one_balanced_post(&url).await;

    let err = PeriodCloseService::new(
        provider,
        noop_publisher(),
        std::sync::Arc::new(
            bss_ledger::infra::audit::secured_audit_sink::NoopSecuredAuditSink::new(),
        ),
    )
    .close(
        &SecurityContext::anonymous(),
        f.tenant,
        f.legal_entity,
        "209901".to_owned(),
    )
    .await
    .expect_err("an unknown period must not be found");
    assert!(matches!(err, DomainError::PeriodNotFound(_)), "got {err:?}");
}

/// Group D: an OPEN close-blocking `exception_queue` row for the period blocks the
/// close (the books are clean, so the exception is the ONLY blocker — the gate must
/// surface it as `PeriodCloseBlocked` and leave the period OPEN).
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn close_blocked_by_open_exception() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");

    let (raw, provider, f) = setup_with_one_balanced_post(&url).await;

    // Seed one OPEN close-blocking exception bound to the period.
    raw.execute_raw(pg(format!(
        "INSERT INTO bss.ledger_exception_queue \
         (tenant_id, exception_id, exception_type, business_ref, status, period_id, opened_at) \
         VALUES ('{}','{}','RECON_MISMATCH','inv-x','OPEN','{}', now())",
        f.tenant,
        Uuid::now_v7(),
        f.period_id
    )))
    .await
    .unwrap();

    let err = PeriodCloseService::new(
        provider,
        noop_publisher(),
        std::sync::Arc::new(
            bss_ledger::infra::audit::secured_audit_sink::NoopSecuredAuditSink::new(),
        ),
    )
    .close(
        &SecurityContext::anonymous(),
        f.tenant,
        f.legal_entity,
        f.period_id.clone(),
    )
    .await
    .expect_err("an OPEN exception must block the close");
    assert!(
        matches!(err, DomainError::PeriodCloseBlocked(_)),
        "got {err:?}"
    );

    let status = period_status(&raw, f.tenant, f.legal_entity, &f.period_id).await;
    assert_eq!(status, "OPEN", "a blocked close leaves the period OPEN");

    // The blocked close records `period_close = CLOSING` + the reasons (dashboard).
    let close_status: String = raw
        .query_one_raw(pg(format!(
            "SELECT status FROM bss.ledger_period_close \
             WHERE tenant_id='{}' AND legal_entity_id='{}' AND period_id='{}'",
            f.tenant, f.legal_entity, f.period_id
        )))
        .await
        .unwrap()
        .expect("period_close row written on a blocked close")
        .try_get::<String>("", "status")
        .unwrap();
    assert_eq!(
        close_status, "CLOSING",
        "a blocked close parks period_close=CLOSING"
    );
}

/// Group D: a recognition segment due `<=` the closing period and not `DONE`
/// blocks the close (design §4.5 — the period-N gate waits for the recognition
/// run). Clean books, so the due segment is the only blocker.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn close_blocked_by_due_recognition_segment() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");

    let (raw, provider, f) = setup_with_one_balanced_post(&url).await;

    // Seed one due-but-not-DONE recognition segment in the closing period.
    raw.execute_raw(pg(format!(
        "INSERT INTO bss.ledger_recognition_segment \
         (tenant_id, schedule_id, segment_no, period_id, amount_minor, status) \
         VALUES ('{}','SCH-D',1,'{}',1000,'PENDING')",
        f.tenant, f.period_id
    )))
    .await
    .unwrap();

    let err = PeriodCloseService::new(
        provider,
        noop_publisher(),
        std::sync::Arc::new(
            bss_ledger::infra::audit::secured_audit_sink::NoopSecuredAuditSink::new(),
        ),
    )
    .close(
        &SecurityContext::anonymous(),
        f.tenant,
        f.legal_entity,
        f.period_id.clone(),
    )
    .await
    .expect_err("a due-not-DONE recognition segment must block the close");
    assert!(
        matches!(err, DomainError::PeriodCloseBlocked(_)),
        "got {err:?}"
    );

    let status = period_status(&raw, f.tenant, f.legal_entity, &f.period_id).await;
    assert_eq!(status, "OPEN", "a blocked close leaves the period OPEN");
}

/// Group D: the dual-control reopen replay (what the executor calls on an approved
/// `PeriodReopen`) flips a CLOSED period back to OPEN, lands `period_close` =
/// REOPENED, and is idempotent on an already-OPEN period. Reopen is ALWAYS
/// dual-control (no inline path), so this models the post-approval executor replay.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn reopen_after_close_flips_to_open_and_records_reopened() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");

    let (raw, provider, f) = setup_with_one_balanced_post(&url).await;
    let service = PeriodCloseService::new(
        provider,
        noop_publisher(),
        std::sync::Arc::new(
            bss_ledger::infra::audit::secured_audit_sink::NoopSecuredAuditSink::new(),
        ),
    );

    // Clean close → CLOSED.
    service
        .close(
            &SecurityContext::anonymous(),
            f.tenant,
            f.legal_entity,
            f.period_id.clone(),
        )
        .await
        .expect("a clean close must succeed");
    assert_eq!(
        period_status(&raw, f.tenant, f.legal_entity, &f.period_id).await,
        "CLOSED"
    );

    // Reopen (the executor replays this on an approved PeriodReopen; `approver` is
    // the distinct second actor the dual-control flow enforced upstream).
    let approver = Uuid::now_v7();
    service
        .reopen(f.tenant, f.legal_entity, &f.period_id, approver)
        .await
        .expect("reopen flips CLOSED→OPEN");

    assert_eq!(
        period_status(&raw, f.tenant, f.legal_entity, &f.period_id).await,
        "OPEN",
        "reopen flips the fiscal period back to OPEN"
    );
    let close_status: String = raw
        .query_one_raw(pg(format!(
            "SELECT status FROM bss.ledger_period_close \
             WHERE tenant_id='{}' AND legal_entity_id='{}' AND period_id='{}'",
            f.tenant, f.legal_entity, f.period_id
        )))
        .await
        .unwrap()
        .expect("period_close row present after reopen")
        .try_get::<String>("", "status")
        .unwrap();
    assert_eq!(close_status, "REOPENED", "period_close lands REOPENED");

    // Idempotent: a second reopen on the already-OPEN period is a no-op success.
    service
        .reopen(f.tenant, f.legal_entity, &f.period_id, approver)
        .await
        .expect("reopen is idempotent on an already-OPEN period");
    assert_eq!(
        period_status(&raw, f.tenant, f.legal_entity, &f.period_id).await,
        "OPEN",
        "a re-reopen leaves the period OPEN"
    );
}
