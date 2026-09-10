//! Postgres-only end-to-end: the PII erasure + re-identification path
//! (`ErasureService`, Slice 6 Phase 3 Group 3A, architecture §4.5 / AC #22).
//! Boots a container, migrates, seeds reference data, posts ONE balanced entry
//! for a payer, seeds that payer's `payer_pii_map` row, then drives the erasure /
//! re-identify writes and asserts:
//!   * `erase` flips the map tombstone (`erased = true`) and writes ONE
//!     `erasure` secured-audit record, while the posted `journal_entry` /
//!     `journal_line` `row_hash` is byte-unchanged (the financial truth + its
//!     chain are untouched);
//!   * `reidentify` returns the `pii_ref` (even of the tombstoned payer) and
//!     writes ONE `re-identification` secured-audit record;
//!   * `reidentify` without a reason is rejected `MISSING_INVESTIGATION_REASON`,
//!     writing NO record.
//!
//! Ignored by default; run with `cargo test -p cf-gears-bss-ledger -- --ignored`.

#![allow(
    clippy::non_ascii_literal,
    clippy::let_underscore_must_use,
    clippy::needless_collect,
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::doc_markdown,
    clippy::panic,
    clippy::too_many_lines
)]

use bss_ledger::domain::error::DomainError;
use bss_ledger::domain::model::{AccountRow, CurrencyScaleRow, NewEntry, NewLine};
use bss_ledger::domain::money::DEFAULT_PLAUSIBLE_MAX_MAJOR;
use bss_ledger::infra::pii::ErasureService;
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

/// Read a boolean scalar (`erased`), `None` when absent.
async fn scalar_bool(conn: &DatabaseConnection, sql: &str) -> Option<bool> {
    let row = conn.query_one_raw(pg(sql.to_owned())).await.unwrap();
    row.map(|r| r.try_get_by_index::<bool>(0).unwrap())
}

struct Fixture {
    tenant: Uuid,
    ar_account: Uuid,
    cash_account: Uuid,
    legal_entity: Uuid,
    period_id: String,
}

/// Boot, migrate, seed USD@2 + OPEN period + AR/CASH accounts. (Mirrors
/// `postgres_metadata.rs::setup`.)
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

/// Build a balanced entry for `payer`: DR AR / CR CASH, each `amount`.
fn balanced_entry(
    f: &Fixture,
    payer: Uuid,
    business_id: &str,
    amount: i64,
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
    let lines = vec![
        line(
            f,
            payer,
            f.ar_account,
            AccountClass::Ar,
            Side::Debit,
            amount,
        ),
        line(
            f,
            payer,
            f.cash_account,
            AccountClass::CashClearing,
            Side::Credit,
            amount,
        ),
    ];
    (entry, lines)
}

fn line(
    f: &Fixture,
    payer: Uuid,
    account: Uuid,
    class: AccountClass,
    side: Side,
    amount: i64,
) -> NewLine {
    let _ = f;
    NewLine {
        line_id: Uuid::now_v7(),
        ar_status: None,
        payer_tenant_id: payer,
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

/// The full Group 3A path against Postgres: erase tombstones the map + writes
/// one `erasure` record while the posted journal rows stay byte-unchanged;
/// reidentify returns the ref + writes one `re-identification` record; a
/// reason-less reidentify is rejected and writes nothing.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn erase_tombstones_and_leaves_journal_unchanged_then_reidentify() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");

    let (raw, service, provider, f) = setup(&url).await;
    let scope = AccessScope::for_tenant(f.tenant);
    let ctx = SecurityContext::anonymous();
    let payer = Uuid::now_v7();
    let pii_ref = "pii-store://payer/abc-123";

    // Post one balanced entry for the payer; capture the entry + AR-line row_hash
    // so we can prove they are byte-unchanged after the erasure.
    let (entry, lines) = balanced_entry(&f, payer, "biz-pii", 1000);
    let entry_id = entry.entry_id;
    let ar_line_id = lines[0].line_id;
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

    // Seed the payer_pii_map row (raw insert — the upsert path is unit-covered).
    raw.execute_raw(pg(format!(
        "INSERT INTO bss.payer_pii_map (tenant_id, payer_tenant_id, pii_ref, erased) \
         VALUES ('{}','{payer}','{pii_ref}', false)",
        f.tenant
    )))
    .await
    .unwrap();

    let svc = ErasureService::new();

    // S-1: the caller's correlation/trace id must reach the `erasure` record so
    // it can be cross-traced to the journal (which carries the same id).
    let corr = Uuid::now_v7();

    // (1) erase tombstones the map + writes ONE `erasure` record.
    svc.erase(
        &provider,
        &ctx,
        &scope,
        f.tenant,
        &scope,
        f.tenant,
        payer,
        "actor-dpo".to_owned(),
        "gdpr-erasure-request".to_owned(),
        Some(corr),
    )
    .await
    .expect("erase must succeed");

    let erased = scalar_bool(
        &raw,
        &format!(
            "SELECT erased FROM bss.payer_pii_map WHERE tenant_id='{}' AND payer_tenant_id='{payer}'",
            f.tenant
        ),
    )
    .await
    .expect("map row present");
    assert!(erased, "erase must flip the tombstone to true");

    let erasure_records = count(
        &raw,
        &format!(
            "SELECT COUNT(*) FROM bss.secured_audit_record \
             WHERE tenant_id='{}' AND event_type='erasure'",
            f.tenant
        ),
    )
    .await;
    assert_eq!(
        erasure_records, 1,
        "exactly one erasure secured-audit record"
    );

    // S-1: that record carries the caller's correlation id (not NULL) — the
    // cross-trace key back to the journal.
    let stored_corr = scalar_hex(
        &raw,
        &format!(
            "SELECT correlation_id::text FROM bss.secured_audit_record \
             WHERE tenant_id='{}' AND event_type='erasure'",
            f.tenant
        ),
    )
    .await;
    assert_eq!(
        stored_corr,
        Some(corr.to_string()),
        "the erasure record must carry the request correlation id (S-1)"
    );

    // (2) The posted journal_entry / journal_line row_hash is byte-unchanged.
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
        "journal_entry row_hash must be byte-unchanged by an erasure"
    );
    // journal_line carries no row_hash (the entry hash above already covers every
    // line field); assert the line row is intact and still carries the un-erased
    // internal payer_tenant_id — erasure tombstones only payer_pii_map.
    let line_intact = count(
        &raw,
        &format!(
            "SELECT COUNT(*) FROM bss.ledger_journal_line \
             WHERE line_id='{ar_line_id}' AND payer_tenant_id='{payer}'"
        ),
    )
    .await;
    assert_eq!(
        line_intact, 1,
        "the journal_line still carries the un-erased internal payer_tenant_id"
    );

    // (3) reidentify returns the pii_ref (even of the tombstoned payer) and
    //     writes ONE `re-identification` record.
    let got = svc
        .reidentify(
            &provider,
            &ctx,
            &scope,
            f.tenant,
            &scope,
            f.tenant,
            payer,
            "actor-investigator".to_owned(),
            "subpoena 2026-06".to_owned(),
            "LEGAL_HOLD".to_owned(),
            Some(corr),
        )
        .await
        .expect("reidentify must succeed");
    assert_eq!(got, pii_ref, "reidentify returns the stored pii_ref");

    let reid_records = count(
        &raw,
        &format!(
            "SELECT COUNT(*) FROM bss.secured_audit_record \
             WHERE tenant_id='{}' AND event_type='re-identification'",
            f.tenant
        ),
    )
    .await;
    assert_eq!(
        reid_records, 1,
        "exactly one re-identification secured-audit record"
    );

    // S-1: the re-identification record also carries the request correlation id.
    let stored_reid_corr = scalar_hex(
        &raw,
        &format!(
            "SELECT correlation_id::text FROM bss.secured_audit_record \
             WHERE tenant_id='{}' AND event_type='re-identification'",
            f.tenant
        ),
    )
    .await;
    assert_eq!(
        stored_reid_corr,
        Some(corr.to_string()),
        "the re-identification record must carry the request correlation id (S-1)"
    );

    // (4) reidentify WITHOUT a reason is rejected MISSING_INVESTIGATION_REASON,
    //     and writes NO additional record.
    let err = svc
        .reidentify(
            &provider,
            &ctx,
            &scope,
            f.tenant,
            &scope,
            f.tenant,
            payer,
            "actor-investigator".to_owned(),
            "   ".to_owned(),
            String::new(),
            None,
        )
        .await
        .expect_err("a reason-less reidentify must be rejected");
    assert!(
        matches!(err, DomainError::MissingInvestigationReason(_)),
        "reason-less reidentify must be MissingInvestigationReason, got: {err:?}"
    );
    let reid_after = count(
        &raw,
        &format!(
            "SELECT COUNT(*) FROM bss.secured_audit_record \
             WHERE tenant_id='{}' AND event_type='re-identification'",
            f.tenant
        ),
    )
    .await;
    assert_eq!(
        reid_after, 1,
        "a rejected reidentify must write NO additional re-identification record"
    );

    // (5) erase WITHOUT a reason is rejected MISSING_INVESTIGATION_REASON
    //     (matches reidentify), and writes NO erasure record.
    let erasure_before = count(
        &raw,
        &format!(
            "SELECT COUNT(*) FROM bss.secured_audit_record \
             WHERE tenant_id='{}' AND event_type='erasure'",
            f.tenant
        ),
    )
    .await;
    let err = svc
        .erase(
            &provider,
            &ctx,
            &scope,
            f.tenant,
            &scope,
            f.tenant,
            payer,
            "actor-dpo".to_owned(),
            "   ".to_owned(),
            None,
        )
        .await
        .expect_err("a reason-less erasure must be rejected");
    assert!(
        matches!(err, DomainError::MissingInvestigationReason(_)),
        "reason-less erase must be MissingInvestigationReason, got: {err:?}"
    );
    let erasure_after = count(
        &raw,
        &format!(
            "SELECT COUNT(*) FROM bss.secured_audit_record \
             WHERE tenant_id='{}' AND event_type='erasure'",
            f.tenant
        ),
    )
    .await;
    assert_eq!(
        erasure_before, erasure_after,
        "a rejected erasure must write NO additional erasure record"
    );
}

/// Z7-4: backfill the thin pii branch coverage at the integration layer —
/// erase is idempotent + non-leaking (a repeat and an unmapped payer both
/// succeed and still record the event, never a 404 that would reveal mapping
/// state), and reidentify of an unmapped payer is `PayerPiiNotFound` (404).
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn erase_is_idempotent_and_unmapped_noleak_and_reidentify_404() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");

    let (raw, _service, provider, f) = setup(&url).await;
    let scope = AccessScope::for_tenant(f.tenant);
    let ctx = SecurityContext::anonymous();
    let svc = ErasureService::new();

    // (1) Idempotency: a mapped payer erased TWICE — both succeed and the
    //     tombstone stays set (a repeat erase is not an error).
    let mapped = Uuid::now_v7();
    raw.execute_raw(pg(format!(
        "INSERT INTO bss.payer_pii_map (tenant_id, payer_tenant_id, pii_ref, erased) \
         VALUES ('{}','{mapped}','pii://m', false)",
        f.tenant
    )))
    .await
    .unwrap();
    for _ in 0..2 {
        svc.erase(
            &provider,
            &ctx,
            &scope,
            f.tenant,
            &scope,
            f.tenant,
            mapped,
            "actor-dpo".to_owned(),
            "gdpr".to_owned(),
            None,
        )
        .await
        .expect("erase must be idempotent (a repeat still succeeds)");
    }
    let tomb = scalar_bool(
        &raw,
        &format!(
            "SELECT erased FROM bss.payer_pii_map WHERE tenant_id='{}' AND payer_tenant_id='{mapped}'",
            f.tenant
        ),
    )
    .await
    .expect("map row present");
    assert!(tomb, "the tombstone stays set after a repeat erase");

    // (2) No-leak: erasing a NEVER-mapped payer still SUCCEEDS (not 404) and
    //     records the event — so it cannot reveal whether the payer was mapped.
    let unmapped = Uuid::now_v7();
    let erasures_before = count(
        &raw,
        &format!(
            "SELECT COUNT(*) FROM bss.secured_audit_record \
             WHERE tenant_id='{}' AND event_type='erasure'",
            f.tenant
        ),
    )
    .await;
    svc.erase(
        &provider,
        &ctx,
        &scope,
        f.tenant,
        &scope,
        f.tenant,
        unmapped,
        "actor-dpo".to_owned(),
        "gdpr".to_owned(),
        None,
    )
    .await
    .expect("erase of an unmapped payer must succeed (no 404 leak)");
    let erasures_after = count(
        &raw,
        &format!(
            "SELECT COUNT(*) FROM bss.secured_audit_record \
             WHERE tenant_id='{}' AND event_type='erasure'",
            f.tenant
        ),
    )
    .await;
    assert_eq!(
        erasures_after,
        erasures_before + 1,
        "erasing an unmapped payer still records exactly one erasure event"
    );

    // (3) reidentify of a payer with NO map row → PayerPiiNotFound (404).
    let err = svc
        .reidentify(
            &provider,
            &ctx,
            &scope,
            f.tenant,
            &scope,
            f.tenant,
            Uuid::now_v7(),
            "actor-inv".to_owned(),
            "subpoena".to_owned(),
            "LEGAL_HOLD".to_owned(),
            None,
        )
        .await
        .expect_err("reidentify of an unmapped payer must 404");
    assert!(
        matches!(err, DomainError::PayerPiiNotFound(_)),
        "unmapped reidentify must be PayerPiiNotFound, got: {err:?}"
    );
}
