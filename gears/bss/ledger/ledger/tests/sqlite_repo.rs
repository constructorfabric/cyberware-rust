//! Fast SQLite round-trip for the foundation repos. Opens an in-memory
//! database, runs the migrator, inserts a balanced 2-line entry through
//! `JournalRepo::insert_entry_with_lines` inside a transaction, then reads
//! it back via `find_entry`. SQLite has no triggers, so the entry is
//! already balanced (balance validation lands in P3).

#![allow(
    clippy::non_ascii_literal,
    clippy::let_underscore_must_use,
    clippy::needless_collect,
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::doc_markdown,
    clippy::similar_names
)]

use bss_ledger::domain::model::{EntryKey, NewEntry, NewLine};
use bss_ledger::infra::storage::migrations::Migrator;
use bss_ledger::infra::storage::repo::JournalRepo;
use bss_ledger_sdk::{AccountClass, MappingStatus, Side, SourceDocType};
use chrono::NaiveDate;
use sea_orm_migration::MigratorTrait;
use serde_json::json;
use time::OffsetDateTime;
use toolkit_db::migration_runner::run_migrations_for_testing;
use toolkit_db::secure::AccessScope;
use toolkit_db::{ConnectOpts, DBProvider, DbError, connect_db};
use uuid::Uuid;

#[tokio::test]
async fn balanced_entry_round_trips_on_sqlite() {
    let db = connect_db("sqlite::memory:", ConnectOpts::default())
        .await
        .expect("connect in-memory sqlite");
    run_migrations_for_testing(&db, Migrator::migrations())
        .await
        .expect("run migrator");
    let provider = DBProvider::<DbError>::new(db);
    let repo = JournalRepo::new(provider.clone());

    let tenant_id = Uuid::now_v7();
    let entry_id = Uuid::now_v7();
    let account_id = Uuid::now_v7();
    let period_id = "202606".to_owned();

    let entry = NewEntry {
        entry_id,
        tenant_id,
        legal_entity_id: Uuid::now_v7(),
        period_id: period_id.clone(),
        entry_currency: "USD".to_owned(),
        source_doc_type: SourceDocType::InvoicePost,
        source_business_id: "inv-1".to_owned(),
        reverses_entry_id: None,
        reverses_period_id: None,
        posted_at_utc: OffsetDateTime::now_utc(),
        effective_at: NaiveDate::from_ymd_opt(2026, 6, 18).unwrap(),
        origin: "SYSTEM".to_owned(),
        posted_by_actor_id: Uuid::now_v7(),
        correlation_id: Uuid::now_v7(),
        rounding_evidence: json!({}),
        rate_snapshot_ref: None,
    };

    let mk_line = |account_class: AccountClass, side: Side, amount: i64| NewLine {
        line_id: Uuid::now_v7(),
        payer_tenant_id: tenant_id,
        seller_tenant_id: None,
        resource_tenant_id: None,
        account_id,
        account_class,
        gl_code: None,
        side,
        amount_minor: amount,
        currency: "USD".to_owned(),
        currency_scale: 2,
        invoice_id: Some("inv-1".to_owned()),
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
    };

    // Balanced: 1000 DR (AR) against 1000 CR (CASH_CLEARING), same
    // currency/scale. Neither class requires revenue_stream/tax dims, so the
    // entry exercises the round-trip without tripping the column CHECKs.
    let lines = vec![
        mk_line(AccountClass::Ar, Side::Debit, 1000),
        mk_line(AccountClass::CashClearing, Side::Credit, 1000),
    ];

    let entry_ref = provider
        .transaction(move |tx| {
            Box::pin(async move {
                repo.insert_entry_with_lines(tx, entry, lines)
                    .await
                    .map_err(|e| DbError::Other(anyhow::Error::msg(e.to_string())))
            })
        })
        .await
        .expect("insert entry inside transaction");

    assert_eq!(entry_ref.entry_id, entry_id);
    assert!(
        entry_ref.created_seq > 0,
        "created_seq must be DB-populated, got {}",
        entry_ref.created_seq
    );

    let read_repo = JournalRepo::new(provider);
    let scope = AccessScope::for_tenant(tenant_id);
    let record = read_repo
        .find_entry(
            &scope,
            EntryKey {
                tenant_id,
                period_id,
                entry_id,
            },
        )
        .await
        .expect("find_entry must succeed")
        .expect("entry MUST be present after insert");

    assert_eq!(record.entry_id, entry_id);
    assert_eq!(record.created_seq, entry_ref.created_seq);
    assert_eq!(record.entry_currency, "USD");
    assert_eq!(record.lines.len(), 2);

    let total_dr: i64 = record
        .lines
        .iter()
        .filter(|l| l.side == "DR")
        .map(|l| l.amount_minor)
        .sum();
    let total_cr: i64 = record
        .lines
        .iter()
        .filter(|l| l.side == "CR")
        .map(|l| l.amount_minor)
        .sum();
    assert_eq!(total_dr, total_cr, "lines must round-trip balanced");
    assert!(record.lines.iter().all(|l| l.currency == "USD"));
}
