//! Postgres-only integration tests for the journal truth tables.
//! Ignored by default; run with `cargo test -p cf-gears-bss-ledger -- --ignored`.
//!
//! Covers: (a) a balanced entry commits; (b) an unbalanced entry rolls
//! back at COMMIT with `LEDGER_ENTRY_UNBALANCED`; (c) a zero-line header
//! rolls back at COMMIT with `LEDGER_ENTRY_EMPTY`; (d) `UPDATE`/`DELETE`
//! on a committed line is rejected by the append-only trigger.

#![allow(
    clippy::non_ascii_literal,
    clippy::let_underscore_must_use,
    clippy::needless_collect,
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::doc_markdown,
    clippy::too_many_lines
)]

use sea_orm::{ConnectionTrait, Database, DatabaseConnection, Statement, TransactionTrait};
use sea_orm_migration::MigratorTrait;
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use uuid::Uuid;

use bss_ledger::domain::model::{AccountRow, CurrencyScaleRow, NewEntry, NewLine};
use bss_ledger::domain::money::DEFAULT_PLAUSIBLE_MAX_MAJOR;
use bss_ledger::infra::posting::service::PostingService;
use bss_ledger::infra::storage::migrations::Migrator;
use bss_ledger::infra::storage::repo::JournalRepo;
use bss_ledger::infra::storage::repo::ReferenceRepo;
use bss_ledger_sdk::{AccountClass, MappingStatus, ODataQuery, Side, SourceDocType};
use chrono::NaiveDate;
use time::OffsetDateTime;
use toolkit_db::secure::AccessScope;
use toolkit_db::{ConnectOpts, DBProvider, DbError, connect_db};
use toolkit_security::SecurityContext;

/// Build an `ODataQuery` carrying just a `$filter` expression (parsed from the
/// OData text), the way the REST `OData` extractor would. An empty query
/// (`ODataQuery::default()`) lists everything in scope.
fn odata_filter(expr: &str) -> ODataQuery {
    let parsed = toolkit_odata::parse_filter_string(expr)
        .expect("test $filter must parse")
        .into_expr();
    ODataQuery::default().with_filter(parsed)
}

async fn boot() -> (
    testcontainers_modules::testcontainers::ContainerAsync<Postgres>,
    DatabaseConnection,
) {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let db = Database::connect(&url).await.unwrap();
    Migrator::up(&db, None).await.unwrap();
    (container, db)
}

fn exec(sql: impl Into<String>) -> Statement {
    Statement::from_string(sea_orm::DatabaseBackend::Postgres, sql.into())
}

/// Insert a header row inside the open transaction. The balance trigger is
/// DEFERRED, so this succeeds regardless of line state until COMMIT.
async fn insert_entry(
    txn: &impl ConnectionTrait,
    entry_id: Uuid,
    tenant_id: Uuid,
    period_id: &str,
    currency: &str,
) {
    txn.execute_raw(exec(format!(
        "INSERT INTO bss.ledger_journal_entry
            (entry_id, tenant_id, legal_entity_id, period_id, entry_currency,
             source_doc_type, source_business_id, posted_at_utc, effective_at,
             origin, posted_by_actor_id, correlation_id)
         VALUES ('{entry_id}', '{tenant_id}', '{tenant_id}', '{period_id}', '{currency}',
                 'MANUAL_ADJUSTMENT', 'biz-1', now(), CURRENT_DATE,
                 'SYSTEM', '{tenant_id}', '{tenant_id}')"
    )))
    .await
    .unwrap();
}

#[allow(clippy::too_many_arguments)]
async fn insert_line(
    txn: &impl ConnectionTrait,
    line_id: Uuid,
    entry_id: Uuid,
    tenant_id: Uuid,
    period_id: &str,
    side: &str,
    amount: i64,
    currency: &str,
) {
    txn.execute_raw(exec(format!(
        "INSERT INTO bss.ledger_journal_line
            (line_id, entry_id, tenant_id, period_id, payer_tenant_id, account_id,
             account_class, side, amount_minor, currency, currency_scale, mapping_status)
         VALUES ('{line_id}', '{entry_id}', '{tenant_id}', '{period_id}', '{tenant_id}',
                 '{tenant_id}', 'AR', '{side}', {amount}, '{currency}', 2, 'RESOLVED')"
    )))
    .await
    .unwrap();
}

/// Like [`insert_line`] but with a caller-chosen `payer_tenant_id` (for the
/// single-payer trigger arm).
#[allow(clippy::too_many_arguments)]
async fn insert_line_payer(
    txn: &impl ConnectionTrait,
    entry_id: Uuid,
    tenant_id: Uuid,
    period_id: &str,
    payer_tenant_id: Uuid,
    side: &str,
    amount: i64,
    currency: &str,
) {
    txn.execute_raw(exec(format!(
        "INSERT INTO bss.ledger_journal_line
            (line_id, entry_id, tenant_id, period_id, payer_tenant_id, account_id,
             account_class, side, amount_minor, currency, currency_scale, mapping_status)
         VALUES ('{}', '{entry_id}', '{tenant_id}', '{period_id}', '{payer_tenant_id}',
                 '{tenant_id}', 'AR', '{side}', {amount}, '{currency}', 2, 'RESOLVED')",
        Uuid::new_v4()
    )))
    .await
    .unwrap();
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn balanced_entry_commits() {
    let (_c, db) = boot().await;
    let entry_id = Uuid::new_v4();
    let tenant_id = Uuid::new_v4();

    let txn = db.begin().await.unwrap();
    insert_entry(&txn, entry_id, tenant_id, "202606", "USD").await;
    insert_line(
        &txn,
        Uuid::new_v4(),
        entry_id,
        tenant_id,
        "202606",
        "DR",
        1000,
        "USD",
    )
    .await;
    insert_line(
        &txn,
        Uuid::new_v4(),
        entry_id,
        tenant_id,
        "202606",
        "CR",
        1000,
        "USD",
    )
    .await;
    txn.commit().await.expect("balanced entry must commit");

    let row = db
        .query_one_raw(exec(format!(
            "SELECT created_seq FROM bss.ledger_journal_entry WHERE entry_id = '{entry_id}'"
        )))
        .await
        .unwrap()
        .expect("committed entry must be visible");
    let created_seq: i64 = row.try_get("", "created_seq").expect("created_seq column");
    assert!(
        created_seq > 0,
        "created_seq must be a positive DB-generated sequence, got {created_seq}"
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn unbalanced_entry_rolls_back_at_commit() {
    let (_c, db) = boot().await;
    let entry_id = Uuid::new_v4();
    let tenant_id = Uuid::new_v4();

    let txn = db.begin().await.unwrap();
    insert_entry(&txn, entry_id, tenant_id, "202606", "USD").await;
    insert_line(
        &txn,
        Uuid::new_v4(),
        entry_id,
        tenant_id,
        "202606",
        "DR",
        1000,
        "USD",
    )
    .await;
    insert_line(
        &txn,
        Uuid::new_v4(),
        entry_id,
        tenant_id,
        "202606",
        "CR",
        700,
        "USD",
    )
    .await;
    let err = txn
        .commit()
        .await
        .expect_err("unbalanced entry must fail at COMMIT");
    assert!(
        err.to_string().contains("LEDGER_ENTRY_UNBALANCED"),
        "unexpected error: {err}"
    );

    let row = db
        .query_one_raw(exec(format!(
            "SELECT entry_id FROM bss.ledger_journal_entry WHERE entry_id = '{entry_id}'"
        )))
        .await
        .unwrap();
    assert!(row.is_none(), "rolled-back entry must not persist");
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn zero_line_entry_rolls_back_at_commit() {
    let (_c, db) = boot().await;
    let entry_id = Uuid::new_v4();
    let tenant_id = Uuid::new_v4();

    let txn = db.begin().await.unwrap();
    insert_entry(&txn, entry_id, tenant_id, "202606", "USD").await;
    let err = txn
        .commit()
        .await
        .expect_err("zero-line entry must fail at COMMIT");
    assert!(
        err.to_string().contains("LEDGER_ENTRY_EMPTY"),
        "unexpected error: {err}"
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn append_only_rejects_update_and_delete() {
    let (_c, db) = boot().await;
    let entry_id = Uuid::new_v4();
    let tenant_id = Uuid::new_v4();
    let line_id = Uuid::new_v4();

    let txn = db.begin().await.unwrap();
    insert_entry(&txn, entry_id, tenant_id, "202606", "USD").await;
    insert_line(
        &txn, line_id, entry_id, tenant_id, "202606", "DR", 1000, "USD",
    )
    .await;
    insert_line(
        &txn,
        Uuid::new_v4(),
        entry_id,
        tenant_id,
        "202606",
        "CR",
        1000,
        "USD",
    )
    .await;
    txn.commit().await.unwrap();

    let upd = db
        .execute_raw(exec(format!(
            "UPDATE bss.ledger_journal_line SET amount_minor = 5 WHERE line_id = '{line_id}'"
        )))
        .await
        .expect_err("UPDATE on append-only line must be rejected");
    assert!(upd.to_string().contains("append-only"), "unexpected: {upd}");

    let del = db
        .execute_raw(exec(format!(
            "DELETE FROM bss.ledger_journal_line WHERE line_id = '{line_id}'"
        )))
        .await
        .expect_err("DELETE on append-only line must be rejected");
    assert!(del.to_string().contains("append-only"), "unexpected: {del}");
}

/// Two balanced USD lines carrying DIFFERENT `payer_tenant_id` values trip the
/// single-payer arm of the balance trigger (`MIXED_PAYER_TENANT`) at COMMIT —
/// a cross-payer entry must never persist (cost-attribution / BOLA isolation).
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn mixed_payer_entry_rolls_back_at_commit() {
    let (_c, db) = boot().await;
    let entry_id = Uuid::new_v4();
    let tenant_id = Uuid::new_v4();
    let payer_b = Uuid::new_v4();

    let txn = db.begin().await.unwrap();
    insert_entry(&txn, entry_id, tenant_id, "202606", "USD").await;
    insert_line_payer(
        &txn, entry_id, tenant_id, "202606", tenant_id, "DR", 1000, "USD",
    )
    .await;
    insert_line_payer(
        &txn, entry_id, tenant_id, "202606", payer_b, "CR", 1000, "USD",
    )
    .await;
    let err = txn
        .commit()
        .await
        .expect_err("mixed-payer entry must fail at COMMIT");
    assert!(
        err.to_string().contains("MIXED_PAYER_TENANT"),
        "unexpected error: {err}"
    );

    let row = db
        .query_one_raw(exec(format!(
            "SELECT entry_id FROM bss.ledger_journal_entry WHERE entry_id = '{entry_id}'"
        )))
        .await
        .unwrap();
    assert!(
        row.is_none(),
        "rolled-back mixed-payer entry must not persist"
    );
}

/// A line whose currency differs from the entry currency (and is not a
/// zero-amount functional-only line) trips the currency arm of the balance
/// trigger (`LEDGER_ENTRY_CURRENCY_MISMATCH`) at COMMIT, ahead of the
/// unbalanced check.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn currency_mismatch_line_rolls_back_at_commit() {
    let (_c, db) = boot().await;
    let entry_id = Uuid::new_v4();
    let tenant_id = Uuid::new_v4();

    let txn = db.begin().await.unwrap();
    insert_entry(&txn, entry_id, tenant_id, "202606", "USD").await;
    // Same payer (no mixed-payer); one USD and one EUR line (EUR != entry USD).
    insert_line(
        &txn,
        Uuid::new_v4(),
        entry_id,
        tenant_id,
        "202606",
        "DR",
        1000,
        "USD",
    )
    .await;
    insert_line(
        &txn,
        Uuid::new_v4(),
        entry_id,
        tenant_id,
        "202606",
        "CR",
        1000,
        "EUR",
    )
    .await;
    let err = txn
        .commit()
        .await
        .expect_err("currency-mismatch entry must fail at COMMIT");
    assert!(
        err.to_string().contains("LEDGER_ENTRY_CURRENCY_MISMATCH"),
        "unexpected error: {err}"
    );
}

// --- A3: PDP-scoped ledger read seam (journal repo) ------------------------
//
// Provisions a seller, posts ONE balanced invoice entry through the real
// engine, then drives the new scoped repo reads (`find_entry_with_lines`,
// `list_lines`, `list_balances`, `list_ar_invoice_balances`) used by the
// `LedgerClientV1` read methods, plus a cross-tenant BOLA case mirroring
// `postgres_bola.rs`.

/// Seeded ledger ids for the read tests.
struct ReadFixture {
    tenant: Uuid,
    payer: Uuid,
    ar_account: Uuid,
    revenue_account: Uuid,
    tax_account: Uuid,
    invoice_id: String,
    due_date: NaiveDate,
}

fn read_account(
    tenant: Uuid,
    account_id: Uuid,
    class: AccountClass,
    normal: Side,
    revenue_stream: Option<&str>,
) -> AccountRow {
    AccountRow {
        account_id,
        tenant_id: tenant,
        legal_entity_id: tenant,
        account_class: class.as_str().to_owned(),
        currency: "USD".to_owned(),
        revenue_stream: revenue_stream.map(str::to_owned),
        normal_side: normal.as_str().to_owned(),
        may_go_negative: false,
        lifecycle_state: "OPEN".to_owned(),
    }
}

#[allow(clippy::too_many_arguments)]
fn read_line(
    f: &ReadFixture,
    account: Uuid,
    class: AccountClass,
    side: Side,
    amount: i64,
    invoice_id: Option<&str>,
    revenue_stream: Option<&str>,
    tax: Option<(&str, &str)>,
) -> NewLine {
    NewLine {
        line_id: Uuid::now_v7(),
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
        invoice_id: invoice_id.map(str::to_owned),
        due_date: invoice_id.map(|_| f.due_date),
        revenue_stream: revenue_stream.map(str::to_owned),
        mapping_status: MappingStatus::Resolved,
        functional_amount_minor: None,
        functional_currency: None,
        tax_jurisdiction: tax.map(|(j, _)| j.to_owned()),
        tax_filing_period: tax.map(|(_, p)| p.to_owned()),
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

/// Boot, migrate, seed AR/REVENUE/TAX accounts + USD@2 + an OPEN period, and
/// post ONE balanced invoice entry: DR AR 1200 (gross) / CR REVENUE 1000 /
/// CR TAX 200, AR line carrying `invoice_id` + `due_date`, TAX line carrying
/// the tax dims. Returns the migrate connection, a search_path-scoped provider,
/// and the fixture.
async fn setup_posted_invoice(url: &str) -> (DatabaseConnection, DBProvider<DbError>, ReadFixture) {
    let raw = Database::connect(url).await.unwrap();
    Migrator::up(&raw, None).await.unwrap();
    let repo_url = format!("{url}?options=-c%20search_path%3Dbss,public");
    let tdb = connect_db(&repo_url, ConnectOpts::default()).await.unwrap();
    let provider = DBProvider::<DbError>::new(tdb);

    let tenant = Uuid::now_v7();
    let payer = Uuid::now_v7();
    let period_id = "202606".to_owned();
    let f = ReadFixture {
        tenant,
        payer,
        ar_account: Uuid::now_v7(),
        revenue_account: Uuid::now_v7(),
        tax_account: Uuid::now_v7(),
        invoice_id: "INV-1".to_owned(),
        due_date: NaiveDate::from_ymd_opt(2026, 7, 1).unwrap(),
    };

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
    raw.execute_raw(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        format!(
            "INSERT INTO bss.ledger_fiscal_period (tenant_id, legal_entity_id, period_id, fiscal_tz, status)
             VALUES ('{tenant}','{tenant}','{period_id}','UTC','OPEN')"
        ),
    ))
    .await
    .unwrap();
    reference
        .insert_account(read_account(
            tenant,
            f.ar_account,
            AccountClass::Ar,
            Side::Debit,
            None,
        ))
        .await
        .unwrap();
    reference
        .insert_account(read_account(
            tenant,
            f.revenue_account,
            AccountClass::Revenue,
            Side::Credit,
            Some("subscription"),
        ))
        .await
        .unwrap();
    reference
        .insert_account(read_account(
            tenant,
            f.tax_account,
            AccountClass::TaxPayable,
            Side::Credit,
            None,
        ))
        .await
        .unwrap();

    // Balanced invoice: DR AR 1200 / CR REVENUE 1000 / CR TAX 200.
    let entry = NewEntry {
        entry_id: Uuid::now_v7(),
        tenant_id: tenant,
        legal_entity_id: tenant,
        period_id: period_id.clone(),
        entry_currency: "USD".to_owned(),
        source_doc_type: SourceDocType::InvoicePost,
        source_business_id: f.invoice_id.clone(),
        reverses_entry_id: None,
        reverses_period_id: None,
        posted_at_utc: OffsetDateTime::now_utc(),
        effective_at: NaiveDate::from_ymd_opt(2026, 6, 1).unwrap(),
        origin: "SYSTEM".to_owned(),
        posted_by_actor_id: tenant,
        correlation_id: tenant,
        rounding_evidence: serde_json::Value::Null,
        rate_snapshot_ref: None,
    };
    let lines = vec![
        read_line(
            &f,
            f.ar_account,
            AccountClass::Ar,
            Side::Debit,
            1200,
            Some(&f.invoice_id),
            None,
            None,
        ),
        read_line(
            &f,
            f.revenue_account,
            AccountClass::Revenue,
            Side::Credit,
            1000,
            None,
            Some("subscription"),
            None,
        ),
        read_line(
            &f,
            f.tax_account,
            AccountClass::TaxPayable,
            Side::Credit,
            200,
            None,
            None,
            Some(("US-CA", "2026Q2")),
        ),
    ];
    let service = PostingService::new(
        provider.clone(),
        std::sync::Arc::new(bss_ledger::infra::events::publisher::LedgerEventPublisher::noop()),
    );
    service
        .post(
            &SecurityContext::anonymous(),
            &AccessScope::for_tenant(tenant),
            entry,
            lines,
            None,
        )
        .await
        .expect("invoice post must succeed");

    (raw, provider, f)
}

/// `ledger_account_balance` is keyed `(tenant_id, account_id, currency)` and
/// `list_balances` breaks ties on `account_id` alone, so an account carried in two
/// currencies has two rows sharing the tiebreaker. Under a caller's `$orderby`
/// the pair `[field, account_id]` collides in the keyset predicate and the second
/// currency falls out at the page boundary. `query_with_unique_order` appends
/// `currency`; this walks that boundary one row a page.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_one_row_walk_under_account_class_visits_both_currencies_of_an_account() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let (_raw, provider, f) = setup_posted_invoice(&url).await;

    // A second, EUR invoice over the same AR and REVENUE accounts.
    ReferenceRepo::new(provider.clone())
        .upsert_currency_scale(CurrencyScaleRow {
            tenant_id: f.tenant,
            currency: "EUR".to_owned(),
            minor_units: 2,
            plausible_max_major: DEFAULT_PLAUSIBLE_MAX_MAJOR,
            source: "iso".to_owned(),
        })
        .await
        .unwrap();
    let in_eur = |mut line: NewLine| {
        line.currency = "EUR".to_owned();
        line
    };
    let entry = NewEntry {
        entry_id: Uuid::now_v7(),
        tenant_id: f.tenant,
        legal_entity_id: f.tenant,
        period_id: "202606".to_owned(),
        entry_currency: "EUR".to_owned(),
        source_doc_type: SourceDocType::InvoicePost,
        source_business_id: "INV-EUR-1".to_owned(),
        reverses_entry_id: None,
        reverses_period_id: None,
        posted_at_utc: OffsetDateTime::now_utc(),
        effective_at: NaiveDate::from_ymd_opt(2026, 6, 2).unwrap(),
        origin: "SYSTEM".to_owned(),
        posted_by_actor_id: f.tenant,
        correlation_id: f.tenant,
        rounding_evidence: serde_json::Value::Null,
        rate_snapshot_ref: None,
    };
    let lines = vec![
        in_eur(read_line(
            &f,
            f.ar_account,
            AccountClass::Ar,
            Side::Debit,
            500,
            Some("INV-EUR-1"),
            None,
            None,
        )),
        in_eur(read_line(
            &f,
            f.revenue_account,
            AccountClass::Revenue,
            Side::Credit,
            500,
            None,
            Some("subscription"),
            None,
        )),
    ];
    PostingService::new(
        provider.clone(),
        std::sync::Arc::new(bss_ledger::infra::events::publisher::LedgerEventPublisher::noop()),
    )
    .post(
        &SecurityContext::anonymous(),
        &AccessScope::for_tenant(f.tenant),
        entry,
        lines,
        None,
    )
    .await
    .expect("EUR invoice post must succeed");

    let repo = JournalRepo::new(provider);
    let scope = AccessScope::for_tenant(f.tenant);
    let by_class = ODataQuery::default().with_order(toolkit_odata::ODataOrderBy(vec![
        toolkit_odata::OrderKey {
            field: "account_class".to_owned(),
            dir: toolkit_odata::SortDir::Asc,
        },
    ]));

    // The referent: one page, same order. AR×2, REVENUE×2, TAX×1.
    let whole = repo
        .list_balances(&scope, f.tenant, &by_class.clone().with_limit(100))
        .await
        .expect("the whole list");
    let key = |b: &bss_ledger::infra::storage::entity::account_balance::Model| {
        (b.account_class.clone(), b.currency.clone(), b.account_id)
    };
    let expected: Vec<_> = whole.items.iter().map(key).collect();
    assert_eq!(expected.len(), 5, "{expected:?}");
    assert_eq!(
        expected
            .iter()
            .filter(|(_, _, account)| *account == f.ar_account)
            .count(),
        2,
        "the AR account is carried in two currencies: {expected:?}"
    );

    let mut walked = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..=expected.len() {
        let query = match cursor.take() {
            None => by_class.clone().with_limit(1),
            Some(token) => ODataQuery::default()
                .with_limit(1)
                .with_cursor(toolkit_odata::CursorV1::decode(&token).expect("cursor decodes")),
        };
        let page = repo
            .list_balances(&scope, f.tenant, &query)
            .await
            .expect("a page");
        walked.extend(page.items.iter().map(key));
        match page.page_info.next_cursor {
            None => break,
            Some(token) => cursor = Some(token),
        }
    }

    assert_eq!(
        walked, expected,
        "the walk must visit every balance row exactly once, in the referent's order"
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn reads_return_entry_lines_balances_and_ar_invoice() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let (_raw, provider, f) = setup_posted_invoice(&url).await;

    let repo = JournalRepo::new(provider);
    let scope = AccessScope::for_tenant(f.tenant);

    // get_entry: header + all three lines.
    let entry_id = repo
        .entry_ids_for_business_id(&scope, f.tenant, &f.invoice_id)
        .await
        .expect("resolve entry id")
        .first()
        .copied()
        .expect("one entry for the invoice business id");
    let record = repo
        .find_entry_with_lines(&scope, f.tenant, entry_id)
        .await
        .expect("get entry")
        .expect("entry must exist");
    assert_eq!(record.source_doc_type, "INVOICE_POST");
    assert_eq!(record.lines.len(), 3, "AR + Revenue + Tax lines");

    // list_lines filtered by payer (OData `$filter`): all three lines share it.
    let by_payer = repo
        .list_lines(
            &scope,
            f.tenant,
            &odata_filter(&format!("payer_tenant_id eq {}", f.payer)),
        )
        .await
        .expect("list lines by payer");
    assert_eq!(by_payer.items.len(), 3, "all three lines share the payer");

    // list_lines filtered by account_class = REVENUE: exactly the revenue line,
    // carrying its revenue_stream.
    let revenue_lines = repo
        .list_lines(
            &scope,
            f.tenant,
            &odata_filter("account_class eq 'REVENUE'"),
        )
        .await
        .expect("list revenue lines");
    assert_eq!(revenue_lines.items.len(), 1, "one revenue line");
    assert_eq!(
        revenue_lines.items[0].revenue_stream.as_deref(),
        Some("subscription"),
        "revenue line carries its stream"
    );

    // list_balances (bare query): AR + Revenue + Tax account-balance grains.
    let balances = repo
        .list_balances(&scope, f.tenant, &ODataQuery::default())
        .await
        .expect("list balances");
    assert_eq!(balances.items.len(), 3, "AR + Revenue + Tax balances");
    let ar = balances
        .items
        .iter()
        .find(|b| b.account_id == f.ar_account)
        .expect("AR balance present");
    assert_eq!(ar.balance_minor, 1200, "AR balance = gross 1200");

    // list_balances filtered by class = REVENUE (OData `$filter`).
    let rev_only = repo
        .list_balances(
            &scope,
            f.tenant,
            &odata_filter("account_class eq 'REVENUE'"),
        )
        .await
        .expect("list revenue balance");
    assert_eq!(rev_only.items.len(), 1, "only the revenue balance");
    assert_eq!(rev_only.items[0].balance_minor, 1000);

    // list_ar_invoice_balances: the AR-invoice grain with its due_date.
    let ar_invoices = repo
        .list_ar_invoice_balances(&scope, f.tenant, Some(f.payer))
        .await
        .expect("list ar invoice balances");
    assert_eq!(ar_invoices.len(), 1, "one AR-invoice row");
    assert_eq!(ar_invoices[0].invoice_id, f.invoice_id);
    assert_eq!(ar_invoices[0].balance_minor, 1200);
    // Decision P: the projector now threads the AR LINE's due_date onto the
    // ar_invoice_balance cache row (first-write-wins), so the cache read
    // surfaces the same date `build_invoice_entry` stamped — closing the latent
    // AR-aging gap where the cache used to hardcode NULL.
    assert_eq!(
        ar_invoices[0].due_date,
        Some(NaiveDate::from_ymd_opt(2026, 7, 1).unwrap()),
        "the projector threads the line's due_date onto the cache row (decision P)"
    );
}

/// BOLA (authz #3): a tenant-B scope must see NONE of tenant A's journal data —
/// `get_entry` → `None`, `list_lines` / `list_balances` /
/// `list_ar_invoice_balances` → empty — even though A genuinely has rows. The
/// SecureORM scope predicate overrides the caller-supplied `tenant_id` filter at
/// the SQL layer (mirrors `postgres_bola.rs`).
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn cross_tenant_reads_are_sql_scoped_to_empty() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let (_raw, provider, f) = setup_posted_invoice(&url).await;

    let repo = JournalRepo::new(provider);
    let scope_a = AccessScope::for_tenant(f.tenant);
    let scope_b = AccessScope::for_tenant(Uuid::now_v7());

    // Positive control: A sees its own entry id.
    let a_entry = repo
        .entry_ids_for_business_id(&scope_a, f.tenant, &f.invoice_id)
        .await
        .expect("resolve A entry")
        .first()
        .copied()
        .expect("A has an entry");

    // BOLA: B's scope, querying A's exact tenant + entry id, gets None / empty.
    assert!(
        repo.find_entry_with_lines(&scope_b, f.tenant, a_entry)
            .await
            .expect("query ok")
            .is_none(),
        "B must not read A's entry by id"
    );
    assert!(
        repo.list_lines(&scope_b, f.tenant, &ODataQuery::default())
            .await
            .expect("query ok")
            .items
            .is_empty(),
        "B must not list A's lines"
    );
    assert!(
        repo.list_balances(&scope_b, f.tenant, &ODataQuery::default())
            .await
            .expect("query ok")
            .items
            .is_empty(),
        "B must not list A's balances"
    );
    assert!(
        repo.list_ar_invoice_balances(&scope_b, f.tenant, None)
            .await
            .expect("query ok")
            .is_empty(),
        "B must not list A's AR-invoice rows"
    );

    // Sanity: A's own scope still sees its rows (so empty above = scoped out).
    assert!(
        repo.list_balances(&scope_a, f.tenant, &ODataQuery::default())
            .await
            .expect("query ok")
            .items
            .len()
            == 3,
        "A sees its own three balances"
    );
}
