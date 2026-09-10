//! Postgres-only: the balance projector. A balanced DR AR / CR CASH entry
//! projects normal-side-positive deltas into `account_balance` and
//! `ar_payer_balance`; a follow-up CR AR beyond the AR balance trips the
//! no-negative guard (`ProjectError::NegativeBalance`). Ignored by default;
//! run with `cargo test -p cf-gears-bss-ledger -- --ignored`.

#![allow(
    clippy::non_ascii_literal,
    clippy::let_underscore_must_use,
    clippy::needless_collect,
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::doc_markdown
)]

use std::collections::HashMap;

use bss_ledger::domain::instant::parse_rfc3339;
use bss_ledger::domain::model::{AccountRow, NewEntry, NewLine};
use bss_ledger::infra::posting::projector::{BalanceProjector, ProjectError};
use bss_ledger::infra::storage::migrations::Migrator;
use bss_ledger::infra::storage::repo::ReferenceRepo;
use bss_ledger_sdk::{AccountClass, MappingStatus, Side, SourceDocType};
use chrono::NaiveDate;
use sea_orm::{ConnectionTrait, Database, Statement};
use sea_orm_migration::MigratorTrait;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use time::OffsetDateTime;
use toolkit_db::secure::AccessScope;
use toolkit_db::{ConnectOpts, DBProvider, DbError, connect_db};
use uuid::Uuid;

fn pg(sql: impl Into<String>) -> Statement {
    Statement::from_string(sea_orm::DatabaseBackend::Postgres, sql.into())
}

fn account(tenant: Uuid, account_id: Uuid, class: AccountClass, normal: Side) -> AccountRow {
    AccountRow {
        account_id,
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

fn entry(tenant: Uuid) -> NewEntry {
    NewEntry {
        entry_id: Uuid::now_v7(),
        tenant_id: tenant,
        legal_entity_id: tenant,
        period_id: "202606".to_owned(),
        entry_currency: "USD".to_owned(),
        source_doc_type: SourceDocType::ManualAdjustment,
        source_business_id: "biz-1".to_owned(),
        reverses_entry_id: None,
        reverses_period_id: None,
        posted_at_utc: OffsetDateTime::now_utc(),
        effective_at: chrono::NaiveDate::from_ymd_opt(2026, 6, 1).unwrap(),
        origin: "SYSTEM".to_owned(),
        posted_by_actor_id: tenant,
        correlation_id: tenant,
        rounding_evidence: serde_json::Value::Null,
        rate_snapshot_ref: None,
    }
}

#[allow(clippy::too_many_arguments)]
fn line(account: Uuid, class: AccountClass, side: Side, amount: i64, payer: Uuid) -> NewLine {
    NewLine {
        line_id: Uuid::now_v7(),
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
        ar_status: None,
    }
}

async fn balance(raw: &sea_orm::DatabaseConnection, sql: &str) -> Option<i64> {
    raw.query_one_raw(pg(sql.to_owned()))
        .await
        .unwrap()
        .map(|row| row.try_get_by_index::<i64>(0).unwrap())
}

/// Read `(original_posted_at, due_date)` for an `ar_invoice_balance` row typed,
/// so the test compares the stamped post date / due date without text parsing.
async fn ar_invoice_stamps(
    raw: &sea_orm::DatabaseConnection,
    account: Uuid,
    invoice_id: &str,
) -> (Option<OffsetDateTime>, Option<NaiveDate>) {
    let row = raw
        .query_one_raw(pg(format!(
            "SELECT original_posted_at, due_date FROM bss.ledger_ar_invoice_balance \
             WHERE account_id='{account}' AND invoice_id='{invoice_id}'"
        )))
        .await
        .unwrap()
        .expect("ar_invoice_balance row must exist");
    (
        row.try_get_by_index::<Option<OffsetDateTime>>(0).unwrap(),
        row.try_get_by_index::<Option<NaiveDate>>(1).unwrap(),
    )
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn projects_deltas_and_enforces_no_negative() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");

    let raw = Database::connect(&url).await.unwrap();
    Migrator::up(&raw, None).await.unwrap();

    let repo_url = format!("{url}?options=-c%20search_path%3Dbss,public");
    let tdb = connect_db(&repo_url, ConnectOpts::default()).await.unwrap();
    let provider = DBProvider::<DbError>::new(tdb);

    let tenant = Uuid::now_v7();
    let payer = tenant;
    let ar = Uuid::now_v7();
    let cash = Uuid::now_v7();
    let scope = AccessScope::for_tenant(tenant);

    // Provision accounts: AR normal DR, CASH_CLEARING normal CR.
    let reference = ReferenceRepo::new(provider.clone());
    reference
        .insert_account(account(tenant, ar, AccountClass::Ar, Side::Debit))
        .await
        .unwrap();
    reference
        .insert_account(account(
            tenant,
            cash,
            AccountClass::CashClearing,
            Side::Credit,
        ))
        .await
        .unwrap();

    let mut normal_sides = HashMap::new();
    normal_sides.insert(ar, Side::Debit);
    normal_sides.insert(cash, Side::Credit);

    let projector = BalanceProjector::new();

    // Project DR AR 1000 / CR CASH 1000 → both normal-side-positive (+1000).
    let lines1 = vec![
        line(ar, AccountClass::Ar, Side::Debit, 1000, payer),
        line(cash, AccountClass::CashClearing, Side::Credit, 1000, payer),
    ];
    let e1 = entry(tenant);
    let r1 = run_project(
        &provider,
        &projector,
        &scope,
        &e1,
        &lines1,
        &normal_sides,
        1,
    )
    .await;
    assert!(r1.is_ok(), "first projection must succeed: {r1:?}");

    assert_eq!(
        balance(
            &raw,
            &format!(
                "SELECT balance_minor FROM bss.ledger_account_balance WHERE account_id='{ar}'"
            )
        )
        .await,
        Some(1000)
    );
    assert_eq!(
        balance(
            &raw,
            &format!(
                "SELECT balance_minor FROM bss.ledger_account_balance WHERE account_id='{cash}'"
            )
        )
        .await,
        Some(1000)
    );
    assert_eq!(
        balance(
            &raw,
            &format!(
                "SELECT balance_minor FROM bss.ledger_ar_payer_balance WHERE account_id='{ar}'"
            )
        )
        .await,
        Some(1000)
    );

    // Project CR AR 1500 (overpay) → AR account_balance would go -500 → guard.
    let lines2 = vec![
        line(ar, AccountClass::Ar, Side::Credit, 1500, payer),
        line(cash, AccountClass::CashClearing, Side::Debit, 1500, payer),
    ];
    let e2 = entry(tenant);
    let r2 = run_project(
        &provider,
        &projector,
        &scope,
        &e2,
        &lines2,
        &normal_sides,
        2,
    )
    .await;
    assert!(
        matches!(r2, Err(ProjectError::NegativeBalance { .. })),
        "overpay must trip the no-negative guard: {r2:?}"
    );
}

#[allow(clippy::too_many_arguments)]
async fn run_project(
    provider: &DBProvider<DbError>,
    projector: &BalanceProjector,
    scope: &AccessScope,
    entry: &NewEntry,
    lines: &[NewLine],
    normal_sides: &HashMap<Uuid, Side>,
    seq: i64,
) -> Result<(), ProjectError> {
    let projector = projector.clone();
    let scope = scope.clone();
    let entry = entry.clone();
    let lines = lines.to_vec();
    let normal_sides = normal_sides.clone();
    provider
        .transaction(move |txn| {
            Box::pin(async move {
                Ok::<_, DbError>(
                    projector
                        .project(txn, &scope, &entry, &lines, &normal_sides, seq)
                        .await,
                )
            })
        })
        .await
        .unwrap()
}

/// An AR line carrying an `invoice_id` fans out to the `ar_invoice_balance`
/// grain, and a `TAX_PAYABLE` line carrying both tax dims fans out to the
/// `tax_subbalance` grain — the two derived caches the first test does not
/// exercise. Both deltas are normal-side-positive, so no guard trips.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn projects_ar_invoice_and_tax_subgrains() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");

    let raw = Database::connect(&url).await.unwrap();
    Migrator::up(&raw, None).await.unwrap();

    let repo_url = format!("{url}?options=-c%20search_path%3Dbss,public");
    let tdb = connect_db(&repo_url, ConnectOpts::default()).await.unwrap();
    let provider = DBProvider::<DbError>::new(tdb);

    let tenant = Uuid::now_v7();
    let payer = tenant;
    let ar = Uuid::now_v7();
    let tax = Uuid::now_v7();
    let scope = AccessScope::for_tenant(tenant);

    // AR normal DR (guarded); TAX_PAYABLE normal CR (not guarded).
    let reference = ReferenceRepo::new(provider.clone());
    reference
        .insert_account(account(tenant, ar, AccountClass::Ar, Side::Debit))
        .await
        .unwrap();
    reference
        .insert_account(account(tenant, tax, AccountClass::TaxPayable, Side::Credit))
        .await
        .unwrap();

    let mut normal_sides = HashMap::new();
    normal_sides.insert(ar, Side::Debit);
    normal_sides.insert(tax, Side::Credit);

    // DR AR 1000 with an invoice → account + ar_payer + ar_invoice grains.
    let mut ar_line = line(ar, AccountClass::Ar, Side::Debit, 1000, payer);
    ar_line.invoice_id = Some("INV-1".to_owned());
    // CR TAX 200 with both tax dims → account + tax grains.
    let mut tax_line = line(tax, AccountClass::TaxPayable, Side::Credit, 200, payer);
    tax_line.tax_jurisdiction = Some("US-CA".to_owned());
    tax_line.tax_filing_period = Some("2026Q2".to_owned());

    let e = entry(tenant);
    let r = run_project(
        &provider,
        &BalanceProjector::new(),
        &scope,
        &e,
        &[ar_line, tax_line],
        &normal_sides,
        1,
    )
    .await;
    assert!(r.is_ok(), "projection must succeed: {r:?}");

    // ar_invoice_balance grain populated for (ar, INV-1).
    assert_eq!(
        balance(
            &raw,
            &format!(
                "SELECT balance_minor FROM bss.ledger_ar_invoice_balance WHERE account_id='{ar}' AND invoice_id='INV-1'"
            )
        )
        .await,
        Some(1000)
    );
    // tax_subbalance grain populated for (tax, US-CA, 2026Q2).
    assert_eq!(
        balance(
            &raw,
            &format!(
                "SELECT balance_minor FROM bss.ledger_tax_subbalance WHERE account_id='{tax}'"
            )
        )
        .await,
        Some(200)
    );
    // The TAX line also writes its account_balance grain (+200, CR on CR-normal).
    assert_eq!(
        balance(
            &raw,
            &format!(
                "SELECT balance_minor FROM bss.ledger_account_balance WHERE account_id='{tax}'"
            )
        )
        .await,
        Some(200)
    );
}

/// B2: a `CR UNALLOCATED` line projects into `unallocated_balance` (+gross); a
/// `DR UNALLOCATED` line nets it down; an over-draw beyond the balance trips
/// the no-negative guard (`ProjectError::NegativeBalance`).
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn projects_unallocated_balance_and_enforces_no_negative() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");

    let raw = Database::connect(&url).await.unwrap();
    Migrator::up(&raw, None).await.unwrap();

    let repo_url = format!("{url}?options=-c%20search_path%3Dbss,public");
    let tdb = connect_db(&repo_url, ConnectOpts::default()).await.unwrap();
    let provider = DBProvider::<DbError>::new(tdb);

    let tenant = Uuid::now_v7();
    let payer = tenant;
    let cash = Uuid::now_v7();
    let unalloc = Uuid::now_v7();
    let scope = AccessScope::for_tenant(tenant);

    // CASH_CLEARING normal DR (a settlement debits cash); UNALLOCATED normal CR
    // (the unapplied-cash credit), guarded.
    let reference = ReferenceRepo::new(provider.clone());
    reference
        .insert_account(account(
            tenant,
            cash,
            AccountClass::CashClearing,
            Side::Debit,
        ))
        .await
        .unwrap();
    reference
        .insert_account(account(
            tenant,
            unalloc,
            AccountClass::Unallocated,
            Side::Credit,
        ))
        .await
        .unwrap();

    let mut normal_sides = HashMap::new();
    normal_sides.insert(cash, Side::Debit);
    normal_sides.insert(unalloc, Side::Credit);

    let projector = BalanceProjector::new();

    // Settlement: DR CASH_CLEARING 1000 / CR UNALLOCATED 1000 → +1000 unapplied.
    let lines1 = vec![
        line(cash, AccountClass::CashClearing, Side::Debit, 1000, payer),
        line(
            unalloc,
            AccountClass::Unallocated,
            Side::Credit,
            1000,
            payer,
        ),
    ];
    let e1 = entry(tenant);
    let r1 = run_project(
        &provider,
        &projector,
        &scope,
        &e1,
        &lines1,
        &normal_sides,
        1,
    )
    .await;
    assert!(r1.is_ok(), "settlement projection must succeed: {r1:?}");
    assert_eq!(
        balance(
            &raw,
            &format!(
                "SELECT balance_minor FROM bss.ledger_unallocated_balance \
                 WHERE payer_tenant_id='{payer}' AND account_id='{unalloc}' AND currency='USD'"
            )
        )
        .await,
        Some(1000),
        "unallocated_balance rises by gross"
    );

    // Allocation: DR UNALLOCATED 400 (apply 400) → nets unapplied to 600.
    let lines2 = vec![
        line(unalloc, AccountClass::Unallocated, Side::Debit, 400, payer),
        line(cash, AccountClass::CashClearing, Side::Credit, 400, payer),
    ];
    let e2 = entry(tenant);
    let r2 = run_project(
        &provider,
        &projector,
        &scope,
        &e2,
        &lines2,
        &normal_sides,
        2,
    )
    .await;
    assert!(r2.is_ok(), "allocation net-down must succeed: {r2:?}");
    assert_eq!(
        balance(
            &raw,
            &format!(
                "SELECT balance_minor FROM bss.ledger_unallocated_balance \
                 WHERE payer_tenant_id='{payer}' AND account_id='{unalloc}' AND currency='USD'"
            )
        )
        .await,
        Some(600),
        "DR UNALLOCATED nets the unapplied-cash balance down"
    );

    // Over-draw: DR UNALLOCATED 1000 against a 600 balance → would go -400.
    let lines3 = vec![
        line(unalloc, AccountClass::Unallocated, Side::Debit, 1000, payer),
        line(cash, AccountClass::CashClearing, Side::Credit, 1000, payer),
    ];
    let e3 = entry(tenant);
    let r3 = run_project(
        &provider,
        &projector,
        &scope,
        &e3,
        &lines3,
        &normal_sides,
        3,
    )
    .await;
    assert!(
        matches!(r3, Err(ProjectError::NegativeBalance { .. })),
        "over-draw must trip the no-negative guard: {r3:?}"
    );
}

/// B3 (decision P): the projector stamps `ar_invoice_balance.original_posted_at`
/// + `due_date` from the entry's posted-at + the line's due date on the FIRST
/// write, and a later net-down (a payment-allocation CR AR) leaves both
/// UNCHANGED (first-write-wins; `on_conflict` omits the two columns).
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn stamps_ar_invoice_original_posted_at_and_due_date_first_write_wins() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");

    let raw = Database::connect(&url).await.unwrap();
    Migrator::up(&raw, None).await.unwrap();

    let repo_url = format!("{url}?options=-c%20search_path%3Dbss,public");
    let tdb = connect_db(&repo_url, ConnectOpts::default()).await.unwrap();
    let provider = DBProvider::<DbError>::new(tdb);

    let tenant = Uuid::now_v7();
    let payer = tenant;
    let ar = Uuid::now_v7();
    let scope = AccessScope::for_tenant(tenant);

    let reference = ReferenceRepo::new(provider.clone());
    reference
        .insert_account(account(tenant, ar, AccountClass::Ar, Side::Debit))
        .await
        .unwrap();
    let mut normal_sides = HashMap::new();
    normal_sides.insert(ar, Side::Debit);

    let projector = BalanceProjector::new();

    // First write: INVOICE_POST DR AR 1000 with due_date d at posted_at t1.
    let t1 = parse_rfc3339("2026-06-01T10:00:00Z").unwrap();
    let due = NaiveDate::from_ymd_opt(2026, 7, 1).unwrap();
    let mut e1 = entry(tenant);
    e1.source_doc_type = SourceDocType::InvoicePost;
    e1.posted_at_utc = t1;
    let mut ar_line = line(ar, AccountClass::Ar, Side::Debit, 1000, payer);
    ar_line.invoice_id = Some("INV-P".to_owned());
    ar_line.due_date = Some(due);
    let r1 = run_project(
        &provider,
        &projector,
        &scope,
        &e1,
        &[ar_line],
        &normal_sides,
        1,
    )
    .await;
    assert!(
        r1.is_ok(),
        "first AR-invoice projection must succeed: {r1:?}"
    );

    let (posted_at, due_date) = ar_invoice_stamps(&raw, ar, "INV-P").await;
    assert_eq!(
        posted_at,
        Some(t1),
        "original_posted_at stamped on first write"
    );
    assert_eq!(due_date, Some(due), "due_date stamped on first write");

    // Second write: a payment-allocation CR AR 400 net-down at t2 > t1.
    let t2 = parse_rfc3339("2026-06-15T10:00:00Z").unwrap();
    let mut e2 = entry(tenant);
    e2.source_doc_type = SourceDocType::PaymentAllocate;
    e2.posted_at_utc = t2;
    let mut net_line = line(ar, AccountClass::Ar, Side::Credit, 400, payer);
    net_line.invoice_id = Some("INV-P".to_owned());
    // A different (later) due_date on the net-down must NOT overwrite the stamp.
    net_line.due_date = Some(NaiveDate::from_ymd_opt(2026, 9, 1).unwrap());
    let r2 = run_project(
        &provider,
        &projector,
        &scope,
        &e2,
        &[net_line],
        &normal_sides,
        2,
    )
    .await;
    assert!(r2.is_ok(), "net-down projection must succeed: {r2:?}");

    let (posted_at2, due_date2) = ar_invoice_stamps(&raw, ar, "INV-P").await;
    assert_eq!(
        posted_at2,
        Some(t1),
        "original_posted_at UNCHANGED after net-down"
    );
    assert_eq!(due_date2, Some(due), "due_date UNCHANGED after net-down");
    // And the balance did net down (sanity).
    assert_eq!(
        balance(
            &raw,
            &format!("SELECT balance_minor FROM bss.ledger_ar_invoice_balance WHERE account_id='{ar}' AND invoice_id='INV-P'")
        )
        .await,
        Some(600),
        "AR invoice balance nets to 600"
    );
}
