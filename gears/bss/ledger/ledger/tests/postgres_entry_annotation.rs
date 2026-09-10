//! Postgres-only end-to-end: the typed controlled non-financial annotation path
//! (`AnnotationService::set`, Slice 6 Phase 2 Group 2B, Variant C remodel).
//! Boots a container, migrates, seeds reference data, posts one balanced entry,
//! then drives the annotation writes and asserts:
//!   * a PII-bearing description is rejected `PII_IN_METADATA_VALUE` pre-write
//!     (no `entry_annotation` row);
//!   * a dangling target is rejected `INVALID_REQUEST` pre-write (no row);
//!   * an allow-listed description writes ONE `entry_annotation` row + ONE
//!     `metadata-change` `secured_audit_record`, and the posted
//!     `journal_entry` `row_hash` is byte-unchanged;
//!   * a second set UPSERTs in place (one row) with the description updated.
//!
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

use bss_ledger::domain::error::DomainError;
use bss_ledger::domain::model::{AccountRow, CurrencyScaleRow, NewEntry, NewLine};
use bss_ledger::domain::money::DEFAULT_PLAUSIBLE_MAX_MAJOR;
use bss_ledger::infra::annotation::{AnnotationService, AnnotationTarget};
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

/// Hex string of a single-column, single-row SELECT, `None` when absent/NULL.
async fn scalar_hex(conn: &DatabaseConnection, sql: &str) -> Option<String> {
    let row = conn.query_one_raw(pg(sql.to_owned())).await.unwrap();
    row.and_then(|r| r.try_get_by_index::<Option<String>>(0).unwrap())
}

/// Count rows matching a bss-qualified predicate.
async fn count(conn: &DatabaseConnection, sql: &str) -> i64 {
    let row = conn.query_one_raw(pg(sql.to_owned())).await.unwrap();
    row.map_or(0, |r| r.try_get_by_index::<i64>(0).unwrap())
}

/// A single text column over one row, `None` when absent/NULL.
async fn scalar_text(conn: &DatabaseConnection, sql: &str) -> Option<String> {
    let row = conn.query_one_raw(pg(sql.to_owned())).await.unwrap();
    row.and_then(|r| r.try_get_by_index::<Option<String>>(0).unwrap())
}

struct Fixture {
    tenant: Uuid,
    ar_account: Uuid,
    cash_account: Uuid,
    legal_entity: Uuid,
    period_id: String,
}

/// Boot, migrate, seed USD@2 + OPEN period + AR/CASH accounts. (Mirrors
/// `postgres_chain.rs::setup`.)
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

/// Build a balanced entry: DR AR / CR CASH, each `amount`.
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

/// The full Group 2B annotation path against Postgres: a dangling target and a
/// PII-bearing note are rejected pre-write (no row); an allow-listed note writes
/// one `entry_annotation` row + one `metadata-change` secured-audit record and
/// leaves the posted journal rows byte-unchanged; a second set UPSERTs in place
/// (one row) with before = the prior value.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
#[allow(
    clippy::too_many_lines,
    reason = "one container boot amortizes the full pre-write rejection matrix + happy-path upsert + journal-unchanged proof"
)]
async fn annotation_set_upserts_and_leaves_journal_unchanged() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");

    let (raw, service, provider, f) = setup(&url).await;
    let scope = AccessScope::for_tenant(f.tenant);
    let ctx = SecurityContext::anonymous();

    let (entry, lines) = balanced_entry(&f, "biz-annot", 1000);
    let entry_id = entry.entry_id;
    service
        .post(&ctx, &scope, entry, lines, None)
        .await
        .expect("post must succeed");

    let entry_hash_before = scalar_hex(
        &raw,
        &format!(
            "SELECT encode(row_hash,'hex') FROM bss.ledger_journal_entry WHERE entry_id='{entry_id}'"
        ),
    )
    .await
    .expect("entry row_hash before");

    let svc = AnnotationService::new();

    // (1) A PII-bearing description is rejected PII_IN_METADATA_VALUE, pre-write.
    let err = svc
        .set(
            &provider,
            &ctx,
            &scope,
            f.tenant,
            entry_id,
            f.period_id.clone(),
            AnnotationTarget::Entry,
            Some("refund for jane.doe@example.com".to_owned()),
            "actor-1".to_owned(),
            None,
            None,
        )
        .await
        .expect_err("a PII-bearing annotation must be rejected");
    assert!(
        matches!(err, DomainError::PiiInMetadataValue(_)),
        "got: {err:?}"
    );

    // (2) A dangling target is rejected pre-write.
    let err = svc
        .set(
            &provider,
            &ctx,
            &scope,
            f.tenant,
            uuid::Uuid::now_v7(),
            f.period_id.clone(),
            AnnotationTarget::Entry,
            Some("note for a ghost".to_owned()),
            "actor-1".to_owned(),
            None,
            None,
        )
        .await
        .expect_err("a dangling-target annotation must be rejected");
    assert!(
        matches!(err, DomainError::InvalidRequest(_)),
        "got: {err:?}"
    );

    let rows_after_rejections = count(
        &raw,
        &format!(
            "SELECT COUNT(*) FROM bss.entry_annotation WHERE tenant_id='{}'",
            f.tenant
        ),
    )
    .await;
    assert_eq!(
        rows_after_rejections, 0,
        "a pre-write rejection must write NO entry_annotation row"
    );

    // (3) First set: writes one current-state row + one audit record.
    svc.set(
        &provider,
        &ctx,
        &scope,
        f.tenant,
        entry_id,
        f.period_id.clone(),
        AnnotationTarget::Entry,
        Some("first note".to_owned()),
        "actor-1".to_owned(),
        Some("ops note".to_owned()),
        // S-1 cross-trace (Variant B): the handler anchors the audit record on the
        // annotated entry's OWN correlation_id; the fixture posts it as `f.tenant`.
        Some(f.tenant),
    )
    .await
    .expect("an annotation set must succeed");

    let rows = count(
        &raw,
        &format!("SELECT COUNT(*) FROM bss.entry_annotation WHERE tenant_id='{}' AND target_id='{entry_id}'", f.tenant),
    )
    .await;
    assert_eq!(rows, 1, "exactly one entry_annotation row");

    let desc = scalar_text(
        &raw,
        &format!("SELECT description FROM bss.entry_annotation WHERE tenant_id='{}' AND target_id='{entry_id}' AND target_kind='ENTRY'", f.tenant),
    )
    .await;
    assert_eq!(desc.as_deref(), Some("first note"));

    let audit_rows = count(
        &raw,
        &format!("SELECT COUNT(*) FROM bss.secured_audit_record WHERE tenant_id='{}' AND event_type='metadata-change'", f.tenant),
    )
    .await;
    assert_eq!(
        audit_rows, 1,
        "exactly one metadata-change secured-audit record"
    );

    // S-1 cross-trace (Variant B): the audit record carries the annotated entry's
    // own correlation_id, so it joins back to `journal_entry` by construction.
    let audit_correlation = scalar_text(
        &raw,
        &format!(
            "SELECT correlation_id::text FROM bss.secured_audit_record \
             WHERE tenant_id='{}' AND event_type='metadata-change'",
            f.tenant
        ),
    )
    .await;
    assert_eq!(
        audit_correlation.as_deref(),
        Some(f.tenant.to_string().as_str()),
        "the metadata-change record must carry the entry's correlation_id"
    );

    // (4) Second set: UPSERTs in place (still one row), description updated.
    svc.set(
        &provider,
        &ctx,
        &scope,
        f.tenant,
        entry_id,
        f.period_id.clone(),
        AnnotationTarget::Entry,
        Some("second note".to_owned()),
        "actor-2".to_owned(),
        Some("revised".to_owned()),
        None,
    )
    .await
    .expect("the second annotation set must succeed");

    let rows = count(
        &raw,
        &format!("SELECT COUNT(*) FROM bss.entry_annotation WHERE tenant_id='{}' AND target_id='{entry_id}'", f.tenant),
    )
    .await;
    assert_eq!(rows, 1, "the second set UPSERTs in place — still one row");

    let desc = scalar_text(
        &raw,
        &format!("SELECT description FROM bss.entry_annotation WHERE tenant_id='{}' AND target_id='{entry_id}' AND target_kind='ENTRY'", f.tenant),
    )
    .await;
    assert_eq!(
        desc.as_deref(),
        Some("second note"),
        "current-state reflects the latest set"
    );

    let audit_rows = count(
        &raw,
        &format!("SELECT COUNT(*) FROM bss.secured_audit_record WHERE tenant_id='{}' AND event_type='metadata-change'", f.tenant),
    )
    .await;
    assert_eq!(
        audit_rows, 2,
        "two changes ⇒ two audit records (history lives in the chain)"
    );

    // (5) The latest secured-audit record must carry before="first note" / after="second note".
    //     `before_after` is the ONLY place change history is kept (design decision D2).
    let ba_before = scalar_text(
        &raw,
        &format!(
            "SELECT before_after->>'before' FROM bss.secured_audit_record \
             WHERE tenant_id='{0}' AND event_type='metadata-change' \
             ORDER BY at_utc DESC LIMIT 1",
            f.tenant
        ),
    )
    .await;
    assert_eq!(
        ba_before.as_deref(),
        Some("first note"),
        "audit chain must record the prior description as 'before'"
    );

    let ba_after = scalar_text(
        &raw,
        &format!(
            "SELECT before_after->>'after' FROM bss.secured_audit_record \
             WHERE tenant_id='{0}' AND event_type='metadata-change' \
             ORDER BY at_utc DESC LIMIT 1",
            f.tenant
        ),
    )
    .await;
    assert_eq!(
        ba_after.as_deref(),
        Some("second note"),
        "audit chain must record the new description as 'after'"
    );

    // The posted journal row_hash is byte-unchanged by either annotation.
    let entry_hash_after = scalar_hex(
        &raw,
        &format!(
            "SELECT encode(row_hash,'hex') FROM bss.ledger_journal_entry WHERE entry_id='{entry_id}'"
        ),
    )
    .await
    .expect("entry row_hash after");
    assert_eq!(
        entry_hash_before, entry_hash_after,
        "annotation must NOT touch the journal"
    );
}
