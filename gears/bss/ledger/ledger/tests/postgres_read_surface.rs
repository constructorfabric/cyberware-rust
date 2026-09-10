//! Postgres-only integration: the Slice-3 / Slice-4 **read-surface** repo list +
//! by-id methods, exercised across TWO tenants to lock the critical SQL-level
//! property — a list/by-id under tenant A's `AccessScope` NEVER returns tenant B's
//! rows (BOLA), the user `$filter` is **additive over** that scope (never replaces
//! it), and the cursor machinery bounds + advances a page.
//!
//! The methods under test (all `(&self, scope, tenant, &ODataQuery) -> Page<…>` for
//! lists, `(&self, scope, tenant, id) -> Option<…>` for by-id):
//! - `AdjustmentRepo`: `list_refunds` / `read_refund_out_of_txn`,
//!   `list_credit_notes` / `read_credit_note_out_of_txn`, `list_debit_notes` /
//!   `read_debit_note_out_of_txn`;
//! - `DisputeRepo`: `list_disputes` / `read_dispute`;
//! - `RecognitionRepo`: `list_runs` / `read_run_out_of_txn`;
//! - `JournalRepo`: `list_entries`.
//!
//! Rows are seeded by RAW SQL `INSERT` directly into `bss.ledger_*` (the simplest
//! reliable way to populate a read test — mirrors `postgres_refund_dispute_hold.rs`
//! / `postgres_credit_note.rs`), for two tenants A and B. The shared harness
//! (the `pg()` helper, `test_containers::postgres().start()`,
//! `Migrator::up`, the `search_path=bss,public` provider) is duplicated from
//! `postgres_refund.rs` per the established convention (no shared module across
//! these test files). Ignored by default; run with `-- --ignored` (needs Docker).

#![allow(
    clippy::non_ascii_literal,
    clippy::let_underscore_must_use,
    clippy::needless_collect,
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::doc_markdown,
    clippy::panic,
    clippy::too_many_lines,
    clippy::similar_names,
    clippy::too_many_arguments
)]

use bss_ledger::domain::approval::policy::effective_version;
use bss_ledger::infra::storage::migrations::Migrator;
use bss_ledger::infra::storage::repo::{
    AdjustmentRepo, ApprovalRepo, DisputeRepo, JournalRepo, RecognitionRepo,
};
use bss_ledger_sdk::ODataQuery;

use bss_ledger::domain::instant::parse_rfc3339;
use sea_orm::{ConnectionTrait, Database, DatabaseConnection, Statement, TransactionTrait};
use sea_orm_migration::MigratorTrait;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use toolkit_db::secure::AccessScope;
use toolkit_db::{ConnectOpts, DBProvider, DbError, connect_db};
use uuid::Uuid;

fn pg(sql: impl Into<String>) -> Statement {
    Statement::from_string(sea_orm::DatabaseBackend::Postgres, sql.into())
}

/// Build an `ODataQuery` carrying just a parsed `$filter` (the way the REST OData
/// extractor would); an empty query is `ODataQuery::default()`. Mirrors
/// `postgres_journal.rs::odata_filter`.
fn odata_filter(expr: &str) -> ODataQuery {
    let parsed = toolkit_odata::parse_filter_string(expr)
        .expect("test $filter must parse")
        .into_expr();
    ODataQuery::default().with_filter(parsed)
}

/// Boot Postgres, migrate, and return the migrate connection (for raw seed
/// INSERTs) + a `search_path=bss,public` provider (the gear's prod search path, so
/// the secured entity queries resolve into the `bss` schema). Mirrors
/// `postgres_refund.rs::setup` minus the chart (a read test needs no chart of
/// accounts — it seeds the record rows directly).
async fn boot(url: &str) -> (DatabaseConnection, DBProvider<DbError>) {
    let raw = Database::connect(url).await.unwrap();
    Migrator::up(&raw, None).await.unwrap();
    let repo_url = format!("{url}?options=-c%20search_path%3Dbss,public");
    let tdb = connect_db(&repo_url, ConnectOpts::default()).await.unwrap();
    let provider = DBProvider::<DbError>::new(tdb);
    (raw, provider)
}

// ───────────────────────────── raw seed INSERTs ─────────────────────────────
//
// One helper per record table; the column lists + NOT-NULL / CHECK sets are taken
// from the `create_*` migrations (a wrong column name or an out-of-set enum value
// fails at runtime):
//   - refund:          phase∈{initiated,confirmed,rejected,voided,unknown_final},
//                      pattern∈{A_UNALLOCATED,B_RESTORE_AR},
//                      clearing_state∈{PENDING,SETTLED,REVERSED}, amount>=0;
//   - credit_note:     amount/recognized/deferred >= 0;
//   - debit_note:      amount/recognized/deferred >= 0;
//   - dispute:         variant∈{CASH_HOLD,AR_RECLASS},
//                      last_phase∈{OPENED,WON,LOST,PARTIAL}, cycle>=1,
//                      cash_hold_minor <= disputed_amount_minor;
//   - recognition_run: status∈{RUNNING,DONE,FAILED};
//   - journal_entry:   origin∈{SYSTEM,USER} + a balanced 2-line body (the deferred
//                      balance trigger rejects a zero-line / unbalanced header at
//                      COMMIT — so a header is seeded with two balancing lines).

/// Seed a `ledger_refund` row (surrogate PK `(tenant, refund_id)`; natural UNIQUE
/// `(tenant, psp_refund_id, phase)`). Pattern A (`invoice_id` NULL) by default.
async fn seed_refund(
    raw: &DatabaseConnection,
    tenant: Uuid,
    refund_id: &str,
    psp_refund_id: &str,
    payment_id: &str,
    amount: i64,
) {
    raw.execute_raw(pg(format!(
        "INSERT INTO bss.ledger_refund \
         (tenant_id, refund_id, psp_refund_id, phase, pattern, payment_id, currency, \
          amount_minor, clearing_state, created_at_utc) \
         VALUES ('{tenant}','{refund_id}','{psp_refund_id}','initiated','A_UNALLOCATED', \
                 '{payment_id}','USD',{amount},'PENDING', now())"
    )))
    .await
    .unwrap();
}

/// Seed a `ledger_credit_note` row (`(tenant, credit_note_id)` PK).
async fn seed_credit_note(
    raw: &DatabaseConnection,
    tenant: Uuid,
    credit_note_id: &str,
    origin_invoice_id: &str,
    amount: i64,
) {
    raw.execute_raw(pg(format!(
        "INSERT INTO bss.ledger_credit_note \
         (tenant_id, credit_note_id, origin_invoice_id, revenue_stream, currency, \
          amount_minor, reason_code, created_at_utc) \
         VALUES ('{tenant}','{credit_note_id}','{origin_invoice_id}','subscription','USD', \
                 {amount},'CUSTOMER_GOODWILL', now())"
    )))
    .await
    .unwrap();
}

/// Seed a `ledger_debit_note` row (`(tenant, debit_note_id)` PK).
async fn seed_debit_note(
    raw: &DatabaseConnection,
    tenant: Uuid,
    debit_note_id: &str,
    origin_invoice_id: &str,
    amount: i64,
) {
    raw.execute_raw(pg(format!(
        "INSERT INTO bss.ledger_debit_note \
         (tenant_id, debit_note_id, origin_invoice_id, currency, amount_minor, created_at_utc) \
         VALUES ('{tenant}','{debit_note_id}','{origin_invoice_id}','USD',{amount}, now())"
    )))
    .await
    .unwrap();
}

/// Seed an OPEN `ledger_dispute` row (`(tenant, dispute_id)` PK).
/// `cash_hold_minor <= disputed_amount_minor` satisfies the table CHECK. Mirrors
/// `postgres_refund_dispute_hold.rs::open_dispute`.
async fn seed_dispute(
    raw: &DatabaseConnection,
    tenant: Uuid,
    dispute_id: &str,
    payment_id: &str,
    disputed: i64,
) {
    raw.execute_raw(pg(format!(
        "INSERT INTO bss.ledger_dispute \
         (tenant_id, dispute_id, payment_id, currency, variant, last_phase, cycle, \
          disputed_amount_minor, cash_hold_minor, version) \
         VALUES ('{tenant}','{dispute_id}','{payment_id}','USD','CASH_HOLD','OPENED',1, \
                 {disputed},{disputed},0)"
    )))
    .await
    .unwrap();
}

/// Seed a `ledger_recognition_run` row (3-col PK `(tenant, period_id, run_id)`).
async fn seed_run(raw: &DatabaseConnection, tenant: Uuid, run_id: Uuid, period_id: &str) {
    raw.execute_raw(pg(format!(
        "INSERT INTO bss.ledger_recognition_run \
         (tenant_id, run_id, period_id, started_at_utc, status) \
         VALUES ('{tenant}','{run_id}','{period_id}', now(),'DONE')"
    )))
    .await
    .unwrap();
}

/// Seed a `ledger_dual_control_policy` version (`(tenant, version)` PK). `d2`/`a6`/
/// `ttl` must satisfy the migration range CHECKs (`d2 ∈ [10000, 100000000]`,
/// `a6 ∈ [1, 30]`, `ttl > 0`). `effective_from` is an RFC-3339 instant.
async fn seed_policy(
    raw: &DatabaseConnection,
    tenant: Uuid,
    version: i64,
    effective_from: &str,
    d2: i64,
    a6: i32,
    ttl: i64,
) {
    raw.execute_raw(pg(format!(
        "INSERT INTO bss.ledger_dual_control_policy \
         (tenant_id, version, effective_from, d2_threshold_minor, \
          a6_backdating_biz_days, pending_ttl_seconds, created_at_utc) \
         VALUES ('{tenant}',{version},'{effective_from}',{d2},{a6},{ttl}, now())"
    )))
    .await
    .unwrap();
}

/// Seed one balanced `ledger_journal_entry` (header + a DR/CR pair of `journal_line`
/// rows) inside a transaction — the deferred balance trigger rejects a zero-line /
/// unbalanced header at COMMIT, so the body is two balancing USD lines. Mirrors
/// `postgres_journal.rs::insert_entry` + `insert_line`. Returns the `entry_id`.
async fn seed_journal_entry(
    raw: &DatabaseConnection,
    tenant: Uuid,
    period_id: &str,
    source_business_id: &str,
) -> Uuid {
    let entry_id = Uuid::now_v7();
    let txn = raw.begin().await.unwrap();
    txn.execute_raw(pg(format!(
        "INSERT INTO bss.ledger_journal_entry \
            (entry_id, tenant_id, legal_entity_id, period_id, entry_currency, \
             source_doc_type, source_business_id, posted_at_utc, effective_at, \
             origin, posted_by_actor_id, correlation_id) \
         VALUES ('{entry_id}','{tenant}','{tenant}','{period_id}','USD', \
                 'MANUAL_ADJUSTMENT','{source_business_id}', now(), CURRENT_DATE, \
                 'SYSTEM','{tenant}','{tenant}')"
    )))
    .await
    .unwrap();
    for (side, amount) in [("DR", 1000_i64), ("CR", 1000_i64)] {
        txn.execute_raw(pg(format!(
            "INSERT INTO bss.ledger_journal_line \
                (line_id, entry_id, tenant_id, period_id, payer_tenant_id, account_id, \
                 account_class, side, amount_minor, currency, currency_scale, mapping_status) \
             VALUES ('{}','{entry_id}','{tenant}','{period_id}','{tenant}','{tenant}', \
                     'AR','{side}',{amount},'USD',2,'RESOLVED')",
            Uuid::now_v7()
        )))
        .await
        .unwrap();
    }
    txn.commit().await.expect("balanced entry must commit");
    entry_id
}

// ───────────────────────────── refund: list + by-id ─────────────────────────

/// `list_refunds` under A's scope+tenant returns ONLY A's rows (count + ids),
/// never B's — the BOLA property.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn refund_list_is_tenant_scoped() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let (raw, provider) = boot(&url).await;
    let (a, b) = (Uuid::now_v7(), Uuid::now_v7());

    // A: 3 refunds; B: 2 refunds (a populated outsider — so an A-scoped empty of B
    // means "scoped out", not "the store is empty").
    seed_refund(&raw, a, "A-RF-1", "A-PSP-1", "A-PAY-1", 100).await;
    seed_refund(&raw, a, "A-RF-2", "A-PSP-2", "A-PAY-2", 200).await;
    seed_refund(&raw, a, "A-RF-3", "A-PSP-3", "A-PAY-3", 300).await;
    seed_refund(&raw, b, "B-RF-1", "B-PSP-1", "B-PAY-1", 400).await;
    seed_refund(&raw, b, "B-RF-2", "B-PSP-2", "B-PAY-2", 500).await;

    let repo = AdjustmentRepo::new(provider.clone());
    let page = repo
        .list_refunds(&AccessScope::for_tenant(a), a, &ODataQuery::default())
        .await
        .expect("list A refunds");
    assert_eq!(page.items.len(), 3, "A sees exactly its own 3 refunds");
    let mut ids: Vec<&str> = page.items.iter().map(|m| m.refund_id.as_str()).collect();
    ids.sort_unstable();
    assert_eq!(ids, ["A-RF-1", "A-RF-2", "A-RF-3"]);
    assert!(
        page.items.iter().all(|m| m.tenant_id == a),
        "no B-owned refund leaks into A's list"
    );

    // BOLA: A's scope asking for B's tenant yields ZERO (the scope predicate
    // overrides the caller-supplied `tenant = B`, mirroring postgres_bola.rs).
    let cross = repo
        .list_refunds(&AccessScope::for_tenant(a), b, &ODataQuery::default())
        .await
        .expect("A-scope, B-tenant list");
    assert!(
        cross.items.is_empty(),
        "A's scope must NOT list B's refunds (SQL-level BOLA); got {}",
        cross.items.len()
    );
    // Sanity: B's own scope sees B's 2 rows.
    let b_page = repo
        .list_refunds(&AccessScope::for_tenant(b), b, &ODataQuery::default())
        .await
        .expect("list B refunds");
    assert_eq!(b_page.items.len(), 2, "B sees its own 2 refunds");
}

/// `read_refund_out_of_txn` with A's scope+tenant but B's id → `None` (no existence
/// leak); with A's own id → `Some`.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn refund_by_id_foreign_tenant_is_none() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let (raw, provider) = boot(&url).await;
    let (a, b) = (Uuid::now_v7(), Uuid::now_v7());
    seed_refund(&raw, a, "A-RF-1", "A-PSP-1", "A-PAY-1", 100).await;
    seed_refund(&raw, b, "B-RF-1", "B-PSP-1", "B-PAY-1", 400).await;

    let repo = AdjustmentRepo::new(provider.clone());
    let scope_a = AccessScope::for_tenant(a);

    // A reads its own id → Some.
    assert!(
        repo.read_refund_out_of_txn(&scope_a, a, "A-RF-1")
            .await
            .expect("read own")
            .is_some(),
        "A resolves its own refund by id"
    );
    // A reads B's id (under A's tenant) → None: no existence leak.
    assert!(
        repo.read_refund_out_of_txn(&scope_a, a, "B-RF-1")
            .await
            .expect("read foreign-id query ok")
            .is_none(),
        "A must not resolve B's refund id (no existence leak)"
    );
    // Even handing A's scope B's tenant + B's id → None (scope overrides).
    assert!(
        repo.read_refund_out_of_txn(&scope_a, b, "B-RF-1")
            .await
            .expect("read cross query ok")
            .is_none(),
        "A's scope must not resolve a B-owned refund even with B's tenant"
    );
}

/// Cursor: seed N > page rows for A, list with a small `limit`; the page is bounded
/// to `limit`, `page_info` carries a `next_cursor`, and following it returns the
/// remainder with NO overlap. (Refunds only — the cursor machinery is shared by
/// every `paginate_odata` list, so one proof suffices.)
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn refund_list_cursor_paginates() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let (raw, provider) = boot(&url).await;
    let a = Uuid::now_v7();

    // 5 refunds, ids zero-padded so the default `refund_id ASC` keyset is a stable
    // lexical order (the cursor walks this keyset).
    for i in 1..=5 {
        let id = format!("A-RF-{i:02}");
        seed_refund(
            &raw,
            a,
            &id,
            &format!("A-PSP-{i:02}"),
            &format!("A-PAY-{i:02}"),
            100 * i,
        )
        .await;
    }

    let repo = AdjustmentRepo::new(provider.clone());
    let scope_a = AccessScope::for_tenant(a);

    // Page 1: limit 2 ⇒ bounded to 2, a next cursor is present.
    let page1 = repo
        .list_refunds(&scope_a, a, &ODataQuery::default().with_limit(2))
        .await
        .expect("page 1");
    assert_eq!(page1.items.len(), 2, "the page is bounded to the limit");
    assert_eq!(
        page1.page_info.limit, 2,
        "page_info echoes the effective limit"
    );
    let cursor = page1
        .page_info
        .next_cursor
        .clone()
        .expect("a bounded first page carries a next cursor");
    let page1_ids: Vec<String> = page1.items.iter().map(|m| m.refund_id.clone()).collect();
    assert_eq!(
        page1_ids,
        ["A-RF-01", "A-RF-02"],
        "page 1 is the first keyset slice"
    );

    // Page 2: follow the cursor (parsed back into the query, the way the REST layer
    // re-hydrates a `?cursor=` param) — the next slice, no overlap with page 1.
    let cursor_v1 = toolkit_odata::CursorV1::decode(&cursor).expect("cursor decodes");
    let page2 = repo
        .list_refunds(
            &scope_a,
            a,
            &ODataQuery::default().with_limit(2).with_cursor(cursor_v1),
        )
        .await
        .expect("page 2");
    assert_eq!(page2.items.len(), 2, "page 2 is also bounded to the limit");
    let page2_ids: Vec<String> = page2.items.iter().map(|m| m.refund_id.clone()).collect();
    assert_eq!(
        page2_ids,
        ["A-RF-03", "A-RF-04"],
        "page 2 is the next keyset slice"
    );
    assert!(
        page1_ids.iter().all(|id| !page2_ids.contains(id)),
        "the cursor advanced — no row repeats across pages"
    );
}

/// `$filter`: a `payment_id eq …` filter narrows the list to the matching rows,
/// still ANDed under the tenant scope (a foreign-tenant row with the same
/// `payment_id` would not leak — but here all rows are A's, so this asserts the
/// filter selectivity).
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn refund_list_filter_narrows() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let (raw, provider) = boot(&url).await;
    let (a, b) = (Uuid::now_v7(), Uuid::now_v7());

    // Two A refunds share PAY-X; one A refund is on PAY-Y; B has a refund ALSO on
    // PAY-X (the cross-tenant collision the scope must still exclude).
    seed_refund(&raw, a, "A-RF-1", "A-PSP-1", "PAY-X", 100).await;
    seed_refund(&raw, a, "A-RF-2", "A-PSP-2", "PAY-X", 200).await;
    seed_refund(&raw, a, "A-RF-3", "A-PSP-3", "PAY-Y", 300).await;
    seed_refund(&raw, b, "B-RF-1", "B-PSP-1", "PAY-X", 400).await;

    let repo = AdjustmentRepo::new(provider.clone());
    let page = repo
        .list_refunds(
            &AccessScope::for_tenant(a),
            a,
            &odata_filter("payment_id eq 'PAY-X'"),
        )
        .await
        .expect("filter by payment_id");
    assert_eq!(
        page.items.len(),
        2,
        "exactly A's two PAY-X refunds (B's PAY-X row excluded by the scope)"
    );
    let mut ids: Vec<&str> = page.items.iter().map(|m| m.refund_id.as_str()).collect();
    ids.sort_unstable();
    assert_eq!(ids, ["A-RF-1", "A-RF-2"]);
    assert!(
        page.items
            .iter()
            .all(|m| m.payment_id == "PAY-X" && m.tenant_id == a),
        "every matched row is A's and on PAY-X"
    );
}

// ─────────────────────────── credit note: list + by-id ──────────────────────

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn credit_note_list_is_tenant_scoped() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let (raw, provider) = boot(&url).await;
    let (a, b) = (Uuid::now_v7(), Uuid::now_v7());
    seed_credit_note(&raw, a, "A-CN-1", "A-INV-1", 100).await;
    seed_credit_note(&raw, a, "A-CN-2", "A-INV-2", 200).await;
    seed_credit_note(&raw, b, "B-CN-1", "B-INV-1", 300).await;

    let repo = AdjustmentRepo::new(provider.clone());
    let page = repo
        .list_credit_notes(&AccessScope::for_tenant(a), a, &ODataQuery::default())
        .await
        .expect("list A credit notes");
    assert_eq!(page.items.len(), 2, "A sees exactly its own 2 credit notes");
    assert!(page.items.iter().all(|m| m.tenant_id == a));
    let cross = repo
        .list_credit_notes(&AccessScope::for_tenant(a), b, &ODataQuery::default())
        .await
        .expect("A-scope, B-tenant");
    assert!(cross.items.is_empty(), "A must not list B's credit notes");
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn credit_note_by_id_foreign_tenant_is_none() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let (raw, provider) = boot(&url).await;
    let (a, b) = (Uuid::now_v7(), Uuid::now_v7());
    seed_credit_note(&raw, a, "A-CN-1", "A-INV-1", 100).await;
    seed_credit_note(&raw, b, "B-CN-1", "B-INV-1", 300).await;

    let repo = AdjustmentRepo::new(provider.clone());
    let scope_a = AccessScope::for_tenant(a);
    assert!(
        repo.read_credit_note_out_of_txn(&scope_a, a, "A-CN-1")
            .await
            .expect("read own")
            .is_some(),
        "A resolves its own credit note"
    );
    assert!(
        repo.read_credit_note_out_of_txn(&scope_a, a, "B-CN-1")
            .await
            .expect("query ok")
            .is_none(),
        "A must not resolve B's credit note id"
    );
}

// ─────────────────────────── debit note: list + by-id ───────────────────────

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn debit_note_list_is_tenant_scoped() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let (raw, provider) = boot(&url).await;
    let (a, b) = (Uuid::now_v7(), Uuid::now_v7());
    seed_debit_note(&raw, a, "A-DN-1", "A-INV-1", 100).await;
    seed_debit_note(&raw, a, "A-DN-2", "A-INV-2", 200).await;
    seed_debit_note(&raw, b, "B-DN-1", "B-INV-1", 300).await;

    let repo = AdjustmentRepo::new(provider.clone());
    let page = repo
        .list_debit_notes(&AccessScope::for_tenant(a), a, &ODataQuery::default())
        .await
        .expect("list A debit notes");
    assert_eq!(page.items.len(), 2, "A sees exactly its own 2 debit notes");
    assert!(page.items.iter().all(|m| m.tenant_id == a));
    let cross = repo
        .list_debit_notes(&AccessScope::for_tenant(a), b, &ODataQuery::default())
        .await
        .expect("A-scope, B-tenant");
    assert!(cross.items.is_empty(), "A must not list B's debit notes");
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn debit_note_by_id_foreign_tenant_is_none() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let (raw, provider) = boot(&url).await;
    let (a, b) = (Uuid::now_v7(), Uuid::now_v7());
    seed_debit_note(&raw, a, "A-DN-1", "A-INV-1", 100).await;
    seed_debit_note(&raw, b, "B-DN-1", "B-INV-1", 300).await;

    let repo = AdjustmentRepo::new(provider.clone());
    let scope_a = AccessScope::for_tenant(a);
    assert!(
        repo.read_debit_note_out_of_txn(&scope_a, a, "A-DN-1")
            .await
            .expect("read own")
            .is_some(),
        "A resolves its own debit note"
    );
    assert!(
        repo.read_debit_note_out_of_txn(&scope_a, a, "B-DN-1")
            .await
            .expect("query ok")
            .is_none(),
        "A must not resolve B's debit note id"
    );
}

// ───────────────────────────── dispute: list + by-id ────────────────────────

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn dispute_list_is_tenant_scoped() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let (raw, provider) = boot(&url).await;
    let (a, b) = (Uuid::now_v7(), Uuid::now_v7());
    seed_dispute(&raw, a, "A-DISP-1", "A-PAY-1", 1000).await;
    seed_dispute(&raw, a, "A-DISP-2", "A-PAY-2", 2000).await;
    seed_dispute(&raw, b, "B-DISP-1", "B-PAY-1", 3000).await;

    let repo = DisputeRepo::new(provider.clone());
    let page = repo
        .list_disputes(&AccessScope::for_tenant(a), a, &ODataQuery::default())
        .await
        .expect("list A disputes");
    assert_eq!(page.items.len(), 2, "A sees exactly its own 2 disputes");
    assert!(page.items.iter().all(|m| m.tenant_id == a));
    let cross = repo
        .list_disputes(&AccessScope::for_tenant(a), b, &ODataQuery::default())
        .await
        .expect("A-scope, B-tenant");
    assert!(cross.items.is_empty(), "A must not list B's disputes");
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn dispute_by_id_foreign_tenant_is_none() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let (raw, provider) = boot(&url).await;
    let (a, b) = (Uuid::now_v7(), Uuid::now_v7());
    seed_dispute(&raw, a, "A-DISP-1", "A-PAY-1", 1000).await;
    seed_dispute(&raw, b, "B-DISP-1", "B-PAY-1", 3000).await;

    let repo = DisputeRepo::new(provider.clone());
    let scope_a = AccessScope::for_tenant(a);
    assert!(
        repo.read_dispute(&scope_a, a, "A-DISP-1")
            .await
            .expect("read own")
            .is_some(),
        "A resolves its own dispute"
    );
    assert!(
        repo.read_dispute(&scope_a, a, "B-DISP-1")
            .await
            .expect("query ok")
            .is_none(),
        "A must not resolve B's dispute id"
    );
}

// ─────────────────────── recognition run: list + by-id ──────────────────────

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn recognition_run_list_is_tenant_scoped() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let (raw, provider) = boot(&url).await;
    let (a, b) = (Uuid::now_v7(), Uuid::now_v7());
    let (a_run_1, a_run_2, b_run) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    seed_run(&raw, a, a_run_1, "202606").await;
    seed_run(&raw, a, a_run_2, "202607").await;
    seed_run(&raw, b, b_run, "202606").await;

    let repo = RecognitionRepo::new(provider.clone());
    let page = repo
        .list_runs(&AccessScope::for_tenant(a), a, &ODataQuery::default())
        .await
        .expect("list A runs");
    assert_eq!(page.items.len(), 2, "A sees exactly its own 2 runs");
    assert!(page.items.iter().all(|m| m.tenant_id == a));
    let cross = repo
        .list_runs(&AccessScope::for_tenant(a), b, &ODataQuery::default())
        .await
        .expect("A-scope, B-tenant");
    assert!(cross.items.is_empty(), "A must not list B's runs");
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn recognition_run_by_id_foreign_tenant_is_none() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let (raw, provider) = boot(&url).await;
    let (a, b) = (Uuid::now_v7(), Uuid::now_v7());
    let (a_run, b_run) = (Uuid::now_v7(), Uuid::now_v7());
    seed_run(&raw, a, a_run, "202606").await;
    seed_run(&raw, b, b_run, "202606").await;

    let repo = RecognitionRepo::new(provider.clone());
    let scope_a = AccessScope::for_tenant(a);
    assert!(
        repo.read_run_out_of_txn(&scope_a, a, a_run)
            .await
            .expect("read own")
            .is_some(),
        "A resolves its own run by run_id"
    );
    assert!(
        repo.read_run_out_of_txn(&scope_a, a, b_run)
            .await
            .expect("query ok")
            .is_none(),
        "A must not resolve B's run id (no existence leak)"
    );
}

// ─────────────────────────── journal entries: list ──────────────────────────

/// `list_entries` (the entry-HEADER list, R5) under A's scope returns ONLY A's
/// entries, never B's. (No by-id variant in scope here — `JournalRepo` exposes
/// `find_entry_with_lines`, covered by `postgres_journal.rs`.)
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn journal_entries_list_is_tenant_scoped() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let (raw, provider) = boot(&url).await;
    let (a, b) = (Uuid::now_v7(), Uuid::now_v7());
    let a1 = seed_journal_entry(&raw, a, "202606", "A-BIZ-1").await;
    let a2 = seed_journal_entry(&raw, a, "202606", "A-BIZ-2").await;
    let _b1 = seed_journal_entry(&raw, b, "202606", "B-BIZ-1").await;

    let repo = JournalRepo::new(provider.clone());
    let page = repo
        .list_entries(&AccessScope::for_tenant(a), a, &ODataQuery::default())
        .await
        .expect("list A entries");
    assert_eq!(page.items.len(), 2, "A sees exactly its own 2 entries");
    let mut ids: Vec<Uuid> = page.items.iter().map(|m| m.entry_id).collect();
    ids.sort_unstable();
    let mut want = [a1, a2];
    want.sort_unstable();
    assert_eq!(ids, want, "the two entries are A's");
    assert!(page.items.iter().all(|m| m.tenant_id == a));

    // BOLA: A's scope, B's tenant → empty (even though B genuinely has an entry).
    let cross = repo
        .list_entries(&AccessScope::for_tenant(a), b, &ODataQuery::default())
        .await
        .expect("A-scope, B-tenant");
    assert!(
        cross.items.is_empty(),
        "A must not list B's journal entries"
    );
    // Sanity: B's own scope sees its one entry.
    let b_page = repo
        .list_entries(&AccessScope::for_tenant(b), b, &ODataQuery::default())
        .await
        .expect("list B entries");
    assert_eq!(b_page.items.len(), 1, "B sees its own entry");
}

// ──────────────────── dual-control policy: effective read + BOLA ─────────────

/// `read_policy_versions` is tenant-scoped (SQL-level BOLA) and `effective_version`
/// resolves the version in force (R6): A's two versions resolve to the latest
/// `effective_from <= now`; A's scope reading B's tenant returns no rows.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn dual_control_policy_effective_read_resolves_and_is_scoped() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let (raw, provider) = boot(&url).await;
    let (a, b) = (Uuid::now_v7(), Uuid::now_v7());
    // A: v2 (06-20) supersedes v1 (06-01); B: one version (must stay invisible to A).
    seed_policy(&raw, a, 1, "2026-06-01T00:00:00Z", 50_000, 5, 604_800).await;
    seed_policy(&raw, a, 2, "2026-06-20T00:00:00Z", 200_000, 7, 3_600).await;
    seed_policy(&raw, b, 1, "2026-06-01T00:00:00Z", 80_000, 5, 604_800).await;

    let repo = ApprovalRepo::new(provider.clone());
    let now = parse_rfc3339("2026-06-25T00:00:00Z").expect("ts");

    let versions = repo
        .read_policy_versions(&AccessScope::for_tenant(a), a)
        .await
        .expect("read A policy versions");
    assert_eq!(versions.len(), 2, "A sees exactly its own 2 versions");
    let effective = effective_version(&versions, now).expect("a version is in force");
    assert_eq!(effective.version, 2, "the latest effective_from wins");
    assert_eq!(effective.policy.d2_threshold_minor, 200_000);
    assert_eq!(effective.policy.a6_backdating_biz_days, 7);
    assert_eq!(effective.policy.pending_ttl_seconds, 3_600);

    // BOLA: A's scope reading B's tenant resolves to no rows (no value/existence leak).
    let cross = repo
        .read_policy_versions(&AccessScope::for_tenant(a), b)
        .await
        .expect("A-scope, B-tenant");
    assert!(cross.is_empty(), "A must not read B's policy versions");
    assert!(
        effective_version(&cross, now).is_none(),
        "no row ⇒ no effective version ⇒ handler renders the platform defaults"
    );
}

/// A tenant with no policy row reads as no versions ⇒ `effective_version` is `None`
/// (the handler then renders the ratified platform defaults, `is_default = true`).
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn dual_control_policy_absent_row_yields_no_effective_version() {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let (_raw, provider) = boot(&url).await;
    let a = Uuid::now_v7();

    let repo = ApprovalRepo::new(provider.clone());
    let now = parse_rfc3339("2026-06-25T00:00:00Z").expect("ts");
    let versions = repo
        .read_policy_versions(&AccessScope::for_tenant(a), a)
        .await
        .expect("read policy versions");
    assert!(versions.is_empty(), "no rows seeded for this tenant");
    assert!(
        effective_version(&versions, now).is_none(),
        "absent policy ⇒ None ⇒ platform defaults"
    );
}
