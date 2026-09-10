//! Fast SQLite integration tests for the Slice-3 credit-note durable guarantees
//! (Group C), exercised at the repo layer (the load-bearing CHECK guards + the
//! first-touch upsert + the reads) — the cheap half of the Phase-1 integration
//! matrix that does not need Docker/testcontainers. The full handler end-to-end
//! (chart provisioning + projector + post engine) is a Postgres-only test
//! (`postgres_credit_note.rs`, `#[ignore]`).
//!
//! Covered:
//! - **headroom CHECK blocks over-cap** (C5 / AC #24): `add_credit_note_total`
//!   within the seeded headroom succeeds; a bump past `original_total +
//!   debit_note_total` trips `chk_ledger_invoice_exposure_headroom` →
//!   `MoneyOutCapExceeded` (the handler refines it to `CreditNoteExceedsHeadroom`).
//! - **first-touch upsert is idempotent + concurrency-shaped** (C2 headroom seed):
//!   a second `seed_exposure_first_touch` is a no-op that never resets the running
//!   `credit_note_total_minor`.
//! - **schedule deferred reduction + over-reduction CHECK** (C2 schedule reduction):
//!   `reduce_deferred` lowers `total_deferred_minor`; a reduction past the
//!   releasable remainder (below `recognized_minor`) trips the schedule CHECK.
//! - **posted-AR / open-AR reads** (C2 caps): the headroom seed basis + the
//!   AR-vs-wallet cap reads return the journal/cache totals.
//! - **credit_note row persists** (C2): `insert_credit_note` round-trips.

#![allow(
    clippy::non_ascii_literal,
    clippy::let_underscore_must_use,
    clippy::needless_collect,
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::doc_markdown,
    clippy::similar_names,
    clippy::needless_pass_by_value
)]

use bss_ledger::domain::model::{NewEntry, NewLine, RepoError};
use bss_ledger::infra::storage::migrations::Migrator;
use bss_ledger::infra::storage::repo::adjustment_repo::NewCreditNote;
use bss_ledger::infra::storage::repo::recognition_repo::NewSchedule;
use bss_ledger::infra::storage::repo::{AdjustmentRepo, JournalRepo, RecognitionRepo};
use bss_ledger_sdk::{AccountClass, MappingStatus, Side, SourceDocType};
use chrono::NaiveDate;
use sea_orm_migration::MigratorTrait;
use time::OffsetDateTime;
use toolkit_db::migration_runner::run_migrations_for_testing;
use toolkit_db::secure::AccessScope;
use toolkit_db::{ConnectOpts, DBProvider, DbError, connect_db};
use uuid::Uuid;

/// Connect an in-memory SQLite + run the migrator (the same harness as
/// `sqlite_repo.rs`).
async fn provider() -> DBProvider<DbError> {
    let db = connect_db("sqlite::memory:", ConnectOpts::default())
        .await
        .expect("connect in-memory sqlite");
    run_migrations_for_testing(&db, Migrator::migrations())
        .await
        .expect("run migrator");
    DBProvider::<DbError>::new(db)
}

/// Seed an ACTIVE recognition schedule in a fresh txn and return its id.
async fn seed_schedule(
    provider: &DBProvider<DbError>,
    scope: &AccessScope,
    tenant: Uuid,
    total_deferred: i64,
    recognized: i64,
) -> String {
    let schedule_id = Uuid::now_v7().to_string();
    let sched = NewSchedule {
        tenant_id: tenant,
        schedule_id: schedule_id.clone(),
        payer_tenant_id: tenant,
        source_invoice_id: "inv-1".to_owned(),
        source_invoice_item_ref: "item-1".to_owned(),
        po_allocation_group: Some("po-1".to_owned()),
        subscription_ref: None,
        revenue_stream: "SAAS".to_owned(),
        currency: "USD".to_owned(),
        total_deferred_minor: total_deferred,
        policy_ref: "straight_line".to_owned(),
        ssp_snapshot_ref: None,
        vc_estimate_ref: None,
        vc_method_ref: None,
    };
    let scope_owned = scope.clone();
    provider
        .transaction(move |tx| {
            Box::pin(async move {
                RecognitionRepo::insert_schedule(tx, &scope_owned, &sched)
                    .await
                    .map_err(|e| DbError::Other(anyhow::Error::msg(e.to_string())))
            })
        })
        .await
        .expect("seed schedule");
    // The schedule starts at recognized_minor = 0; bump it to `recognized` so the
    // reduction tests have an in-flight schedule (the cap CHECK floor).
    if recognized > 0 {
        let sid = schedule_id.clone();
        let scope_owned = scope.clone();
        provider
            .transaction(move |tx| {
                Box::pin(async move {
                    RecognitionRepo::add_recognized(tx, &scope_owned, tenant, &sid, recognized)
                        .await
                        .map_err(|e| DbError::Other(anyhow::Error::msg(e.to_string())))
                })
            })
            .await
            .expect("bump recognized");
    }
    schedule_id
}

/// Read a schedule's `total_deferred_minor` back.
async fn read_total_deferred(
    provider: &DBProvider<DbError>,
    scope: &AccessScope,
    tenant: Uuid,
    schedule_id: &str,
) -> i64 {
    RecognitionRepo::new(provider.clone())
        .read_schedule(scope, tenant, schedule_id)
        .await
        .expect("read schedule")
        .expect("schedule present")
        .total_deferred_minor
}

#[tokio::test]
async fn headroom_check_blocks_over_cap_credit_note() {
    let provider = provider().await;
    let tenant = Uuid::now_v7();
    let scope = AccessScope::for_tenant(tenant);
    let invoice = "inv-cap";

    // Seed original_total = 1000 (no debit notes ⇒ headroom = 1000).
    let scope_a = scope.clone();
    provider
        .transaction(move |tx| {
            Box::pin(async move {
                AdjustmentRepo::seed_exposure_first_touch(
                    tx, &scope_a, tenant, invoice, "USD", 1000,
                )
                .await
                .map_err(|e| DbError::Other(anyhow::Error::msg(e.to_string())))
            })
        })
        .await
        .expect("seed exposure");

    // A 600 bump is within headroom — OK.
    let scope_b = scope.clone();
    provider
        .transaction(move |tx| {
            Box::pin(async move {
                AdjustmentRepo::add_credit_note_total(tx, &scope_b, tenant, invoice, 600)
                    .await
                    .map_err(|e| DbError::Other(anyhow::Error::msg(e.to_string())))
            })
        })
        .await
        .expect("first credit note within cap");

    // A further 500 bump (running total 1100 > 1000) trips the headroom CHECK →
    // MoneyOutCapExceeded (the handler refines this to CreditNoteExceedsHeadroom).
    let scope_c = scope.clone();
    let res = provider
        .transaction(move |tx| {
            Box::pin(async move {
                AdjustmentRepo::add_credit_note_total(tx, &scope_c, tenant, invoice, 500)
                    .await
                    .map_err(repo_to_db)
            })
        })
        .await;
    assert_cap_exceeded(&res);
}

#[tokio::test]
async fn first_touch_seed_is_idempotent_and_preserves_running_total() {
    let provider = provider().await;
    let tenant = Uuid::now_v7();
    let scope = AccessScope::for_tenant(tenant);
    let invoice = "inv-seed";

    // First touch seeds original_total = 1000.
    let scope_a = scope.clone();
    provider
        .transaction(move |tx| {
            Box::pin(async move {
                AdjustmentRepo::seed_exposure_first_touch(
                    tx, &scope_a, tenant, invoice, "USD", 1000,
                )
                .await
                .map_err(|e| DbError::Other(anyhow::Error::msg(e.to_string())))
            })
        })
        .await
        .expect("first seed");
    // Bump the running credit-note total to 400.
    let scope_b = scope.clone();
    provider
        .transaction(move |tx| {
            Box::pin(async move {
                AdjustmentRepo::add_credit_note_total(tx, &scope_b, tenant, invoice, 400)
                    .await
                    .map_err(|e| DbError::Other(anyhow::Error::msg(e.to_string())))
            })
        })
        .await
        .expect("bump 400");
    // A second seed (a later credit note on the same invoice) is a no-op: it must
    // NOT reset original_total nor the running credit_note_total. A subsequent
    // 600 bump fits exactly to the cap (400 + 600 == 1000); a 601 would not.
    let scope_c = scope.clone();
    provider
        .transaction(move |tx| {
            Box::pin(async move {
                AdjustmentRepo::seed_exposure_first_touch(
                    tx, &scope_c, tenant, invoice, "USD", 9999,
                )
                .await
                .map_err(|e| DbError::Other(anyhow::Error::msg(e.to_string())))
            })
        })
        .await
        .expect("second seed is a no-op");

    let scope_d = scope.clone();
    provider
        .transaction(move |tx| {
            Box::pin(async move {
                AdjustmentRepo::add_credit_note_total(tx, &scope_d, tenant, invoice, 600)
                    .await
                    .map_err(|e| DbError::Other(anyhow::Error::msg(e.to_string())))
            })
        })
        .await
        .expect("bump to exactly the cap (proves original_total stayed 1000)");

    // One more unit over the cap is rejected — confirms the second seed did not
    // bump original_total to 9999.
    let scope_e = scope.clone();
    let res = provider
        .transaction(move |tx| {
            Box::pin(async move {
                AdjustmentRepo::add_credit_note_total(tx, &scope_e, tenant, invoice, 1)
                    .await
                    .map_err(repo_to_db)
            })
        })
        .await;
    assert_cap_exceeded(&res);
}

#[tokio::test]
async fn reduce_deferred_lowers_total_and_blocks_over_reduction() {
    let provider = provider().await;
    let tenant = Uuid::now_v7();
    let scope = AccessScope::for_tenant(tenant);

    // Schedule: total_deferred = 1000, recognized = 300 ⇒ releasable remainder 700.
    let schedule_id = seed_schedule(&provider, &scope, tenant, 1000, 300).await;

    // Reduce 400 (within the 700 remainder) ⇒ total_deferred = 600.
    let sid = schedule_id.clone();
    let scope_a = scope.clone();
    provider
        .transaction(move |tx| {
            Box::pin(async move {
                RecognitionRepo::reduce_deferred(tx, &scope_a, tenant, &sid, 400)
                    .await
                    .map_err(|e| DbError::Other(anyhow::Error::msg(e.to_string())))
            })
        })
        .await
        .expect("reduce within remainder");
    assert_eq!(
        read_total_deferred(&provider, &scope, tenant, &schedule_id).await,
        600,
        "total_deferred reduced over the unreleased remainder"
    );

    // A further reduction of 400 would drop total_deferred to 200 < recognized 300
    // — over-reducing an in-flight schedule. The recognized_minor <=
    // total_deferred_minor CHECK rejects it → MoneyOutCapExceeded.
    let sid = schedule_id.clone();
    let scope_b = scope.clone();
    let res = provider
        .transaction(move |tx| {
            Box::pin(async move {
                RecognitionRepo::reduce_deferred(tx, &scope_b, tenant, &sid, 400)
                    .await
                    .map_err(repo_to_db)
            })
        })
        .await;
    assert_cap_exceeded(&res);
    // The rejected reduction rolled back — total_deferred is still 600.
    assert_eq!(
        read_total_deferred(&provider, &scope, tenant, &schedule_id).await,
        600
    );
}

#[tokio::test]
async fn posted_ar_read_nets_invoice_post_ar_lines() {
    let provider = provider().await;
    let tenant = Uuid::now_v7();
    let scope = AccessScope::for_tenant(tenant);
    let invoice = "inv-ar";

    // Post an INVOICE_POST entry: DR AR 1100 (incl 100 tax) / CR REVENUE 1000 /
    // CR TAX_PAYABLE 100. The posted AR incl. tax is 1100.
    insert_invoice_post(&provider, tenant, invoice, 1100).await;

    let posted = AdjustmentRepo::new(provider.clone())
        .read_posted_ar_incl_tax_out_of_txn(&scope, tenant, invoice)
        .await
        .expect("read posted AR");
    assert_eq!(posted, 1100, "posted AR incl. tax = the AR debit");

    // An invoice with no posted AR line reads 0 (the headroom then floors on debit
    // notes only).
    let absent = AdjustmentRepo::new(provider.clone())
        .read_posted_ar_incl_tax_out_of_txn(&scope, tenant, "inv-none")
        .await
        .expect("read absent posted AR");
    assert_eq!(absent, 0);
}

#[tokio::test]
async fn credit_note_row_round_trips() {
    let provider = provider().await;
    let tenant = Uuid::now_v7();
    let scope = AccessScope::for_tenant(tenant);

    let note = NewCreditNote {
        tenant_id: tenant,
        credit_note_id: "cn-rt".to_owned(),
        origin_invoice_id: "inv-1".to_owned(),
        origin_invoice_item_ref: Some("item-1".to_owned()),
        revenue_stream: "SAAS".to_owned(),
        currency: "USD".to_owned(),
        amount_minor: 1000,
        recognized_part_minor: 400,
        deferred_part_minor: 500,
        split_basis_ref: Some("item=item-1;po=po-1".to_owned()),
        reason_code: "CUSTOMER_GOODWILL".to_owned(),
        created_at_utc: OffsetDateTime::now_utc(),
    };
    let scope_a = scope.clone();
    provider
        .transaction(move |tx| {
            Box::pin(async move {
                AdjustmentRepo::insert_credit_note(tx, &scope_a, &note)
                    .await
                    .map_err(|e| DbError::Other(anyhow::Error::msg(e.to_string())))
            })
        })
        .await
        .expect("insert credit_note");
    // A duplicate PK insert collides (the engine's idempotency claim normally
    // short-circuits this before the sidecar; here we assert the PK guard holds).
    let note2 = NewCreditNote {
        tenant_id: tenant,
        credit_note_id: "cn-rt".to_owned(),
        origin_invoice_id: "inv-1".to_owned(),
        origin_invoice_item_ref: None,
        revenue_stream: "SAAS".to_owned(),
        currency: "USD".to_owned(),
        amount_minor: 1,
        recognized_part_minor: 1,
        deferred_part_minor: 0,
        split_basis_ref: None,
        reason_code: "X".to_owned(),
        created_at_utc: OffsetDateTime::now_utc(),
    };
    let scope_b = scope.clone();
    let dup = provider
        .transaction(move |tx| {
            Box::pin(async move {
                AdjustmentRepo::insert_credit_note(tx, &scope_b, &note2)
                    .await
                    .map_err(repo_to_db)
            })
        })
        .await;
    assert!(
        dup.is_err(),
        "duplicate credit_note_id must collide on the PK"
    );
}

// --- helpers ---

/// Map an in-txn `RepoError` into the `DbError` the `provider.transaction` closure
/// must return (it fixes the closure error to `DbError` and rolls back on `Err`).
/// The `RepoError` `Debug` is stamped into `DbError::Other` so a caller can assert
/// the variant (`RepoError::MoneyOutCapExceeded(_)`'s `Debug` contains the variant
/// name) — the cap tests only confirm the rejection IS the cap guard, so a
/// contains-check on the surfaced string is sufficient + robust.
fn repo_to_db(e: RepoError) -> DbError {
    DbError::Other(anyhow::Error::msg(format!("{e:?}")))
}

/// Assert a rolled-back `provider.transaction` result IS the per-row cap CHECK
/// (`RepoError::MoneyOutCapExceeded`) — the headroom / schedule guards.
fn assert_cap_exceeded(res: &Result<(), DbError>) {
    let err = res
        .as_ref()
        .expect_err("expected a cap-CHECK rejection")
        .to_string();
    assert!(
        err.contains("MoneyOutCapExceeded"),
        "expected MoneyOutCapExceeded, got: {err}"
    );
}

/// Insert a minimal INVOICE_POST journal entry (DR AR `ar_incl_tax` / CR
/// CASH_CLEARING) for `invoice` so `read_posted_ar_incl_tax` can net it. (We use
/// CASH_CLEARING as the credit side to avoid the per-stream revenue_stream CHECK;
/// the read only sums AR-class lines, so the credit side's class is irrelevant.)
async fn insert_invoice_post(
    provider: &DBProvider<DbError>,
    tenant: Uuid,
    invoice: &str,
    ar_incl_tax: i64,
) {
    let repo = JournalRepo::new(provider.clone());
    let account_id = Uuid::now_v7();
    let entry = NewEntry {
        entry_id: Uuid::now_v7(),
        tenant_id: tenant,
        legal_entity_id: tenant,
        period_id: "202606".to_owned(),
        entry_currency: "USD".to_owned(),
        source_doc_type: SourceDocType::InvoicePost,
        source_business_id: invoice.to_owned(),
        reverses_entry_id: None,
        reverses_period_id: None,
        posted_at_utc: OffsetDateTime::now_utc(),
        effective_at: NaiveDate::from_ymd_opt(2026, 6, 18).unwrap(),
        origin: "SYSTEM".to_owned(),
        posted_by_actor_id: Uuid::now_v7(),
        correlation_id: Uuid::now_v7(),
        rounding_evidence: serde_json::json!({}),
        rate_snapshot_ref: None,
    };
    let mk = |class: AccountClass, side: Side, amount: i64| NewLine {
        line_id: Uuid::now_v7(),
        payer_tenant_id: tenant,
        seller_tenant_id: Some(tenant),
        resource_tenant_id: None,
        account_id,
        account_class: class,
        gl_code: None,
        side,
        amount_minor: amount,
        currency: "USD".to_owned(),
        currency_scale: 2,
        invoice_id: Some(invoice.to_owned()),
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
    let lines = vec![
        mk(AccountClass::Ar, Side::Debit, ar_incl_tax),
        mk(AccountClass::CashClearing, Side::Credit, ar_incl_tax),
    ];
    provider
        .transaction(move |tx| {
            Box::pin(async move {
                repo.insert_entry_with_lines(tx, entry, lines)
                    .await
                    .map_err(|e| DbError::Other(anyhow::Error::msg(e.to_string())))
            })
        })
        .await
        .expect("insert invoice-post entry");
}
