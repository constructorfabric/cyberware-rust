//! Fast SQLite integration tests for the Slice-3 debit-note repo guarantees
//! (Group D3), exercised at the repo layer — the cheap half of the Phase-1
//! integration matrix that does not need Docker/testcontainers. The full handler
//! end-to-end (chart provisioning + projector + post engine + schedule build) is a
//! Postgres-only test (`postgres_debit_note.rs`, `#[ignore]`).
//!
//! Covered:
//! - **`add_debit_note_total` raises the headroom** (D3 / AC #24): a debit note
//!   bumps `debit_note_total_minor`, which is the RHS of the headroom CHECK
//!   (`credit_note_total_minor <= original_total_minor + debit_note_total_minor`),
//!   so the cap for *later credit notes* grows — a credit note that would have been
//!   over-cap before the debit note now fits, and the one-unit-over is still
//!   rejected (proving the raise is exactly the debit-note amount).
//! - **`add_debit_note_total` requires a seeded row** (invariant): a bump before
//!   the first-touch seed is a `Db` error.
//! - **`insert_debit_note` round-trips** (D3): a `debit_note` row persists and a
//!   duplicate `(tenant, debit_note_id)` collides on the PK.

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

use bss_ledger::domain::model::RepoError;
use bss_ledger::infra::storage::migrations::Migrator;
use bss_ledger::infra::storage::repo::AdjustmentRepo;
use bss_ledger::infra::storage::repo::adjustment_repo::NewDebitNote;
use sea_orm_migration::MigratorTrait;
use time::OffsetDateTime;
use toolkit_db::migration_runner::run_migrations_for_testing;
use toolkit_db::secure::AccessScope;
use toolkit_db::{ConnectOpts, DBProvider, DbError, connect_db};
use uuid::Uuid;

/// Connect an in-memory SQLite + run the migrator (the same harness as the
/// credit-note repo test).
async fn provider() -> DBProvider<DbError> {
    let db = connect_db("sqlite::memory:", ConnectOpts::default())
        .await
        .expect("connect in-memory sqlite");
    run_migrations_for_testing(&db, Migrator::migrations())
        .await
        .expect("run migrator");
    DBProvider::<DbError>::new(db)
}

/// Map an in-txn `RepoError` into the `DbError` the `provider.transaction` closure
/// must return (it fixes the closure error to `DbError` and rolls back on `Err`);
/// the `RepoError` `Debug` is stamped into `DbError::Other` so a caller can assert
/// the variant by a contains-check.
fn repo_to_db(e: RepoError) -> DbError {
    DbError::Other(anyhow::Error::msg(format!("{e:?}")))
}

/// Assert a rolled-back `provider.transaction` result IS the headroom cap CHECK
/// (`RepoError::MoneyOutCapExceeded`).
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

#[tokio::test]
async fn debit_note_total_raises_headroom_for_credit_notes() {
    let provider = provider().await;
    let tenant = Uuid::now_v7();
    let scope = AccessScope::for_tenant(tenant);
    let invoice = "inv-dn-headroom";

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

    // Raise the headroom by a 500 debit note ⇒ headroom = 1000 + 500 = 1500.
    let scope_b = scope.clone();
    provider
        .transaction(move |tx| {
            Box::pin(async move {
                AdjustmentRepo::add_debit_note_total(tx, &scope_b, tenant, invoice, 500)
                    .await
                    .map_err(|e| DbError::Other(anyhow::Error::msg(e.to_string())))
            })
        })
        .await
        .expect("debit note raises headroom");

    // A 1500 credit note now fits exactly to the raised cap (1000 original + 500
    // debit) — it WOULD have been over-cap (1500 > 1000) before the debit note.
    let scope_c = scope.clone();
    provider
        .transaction(move |tx| {
            Box::pin(async move {
                AdjustmentRepo::add_credit_note_total(tx, &scope_c, tenant, invoice, 1500)
                    .await
                    .map_err(|e| DbError::Other(anyhow::Error::msg(e.to_string())))
            })
        })
        .await
        .expect("credit note fits the debit-note-raised headroom");

    // One unit over the raised cap (running credit 1501 > 1500) is rejected —
    // confirms the headroom was raised by exactly the debit-note amount.
    let scope_d = scope.clone();
    let res = provider
        .transaction(move |tx| {
            Box::pin(async move {
                AdjustmentRepo::add_credit_note_total(tx, &scope_d, tenant, invoice, 1)
                    .await
                    .map_err(repo_to_db)
            })
        })
        .await;
    assert_cap_exceeded(&res);
}

#[tokio::test]
async fn add_debit_note_total_requires_a_seeded_row() {
    let provider = provider().await;
    let tenant = Uuid::now_v7();
    let scope = AccessScope::for_tenant(tenant);

    // No seed first ⇒ the bump matches no row and is a Db invariant error.
    let scope_a = scope.clone();
    let res = provider
        .transaction(move |tx| {
            Box::pin(async move {
                AdjustmentRepo::add_debit_note_total(tx, &scope_a, tenant, "inv-unseeded", 100)
                    .await
                    .map_err(repo_to_db)
            })
        })
        .await;
    let err = res
        .as_ref()
        .expect_err("expected a not-seeded error")
        .to_string();
    assert!(
        err.contains("not seeded"),
        "expected not-seeded Db error, got: {err}"
    );
}

#[tokio::test]
async fn debit_note_row_round_trips() {
    let provider = provider().await;
    let tenant = Uuid::now_v7();
    let scope = AccessScope::for_tenant(tenant);

    let note = NewDebitNote {
        tenant_id: tenant,
        debit_note_id: "dn-rt".to_owned(),
        origin_invoice_id: "inv-1".to_owned(),
        currency: "USD".to_owned(),
        amount_minor: 1100,
        recognized_part_minor: 600,
        deferred_part_minor: 400,
        created_at_utc: OffsetDateTime::now_utc(),
    };
    let scope_a = scope.clone();
    provider
        .transaction(move |tx| {
            Box::pin(async move {
                AdjustmentRepo::insert_debit_note(tx, &scope_a, &note)
                    .await
                    .map_err(|e| DbError::Other(anyhow::Error::msg(e.to_string())))
            })
        })
        .await
        .expect("insert debit_note");

    // The insert committed without error — the round-trip success. Field-level
    // persistence (incl-tax amount + ex-tax split parts) is asserted by the engine
    // end-to-end PG test (`postgres_debit_note`); here the composite-PK guard is
    // asserted by the duplicate-insert collision below (mirrors the credit_note
    // round-trip in `sqlite_adjustment_repo`).

    // A duplicate PK insert collides (the engine's idempotency claim normally
    // short-circuits this before the sidecar; here we assert the PK guard holds).
    let note2 = NewDebitNote {
        tenant_id: tenant,
        debit_note_id: "dn-rt".to_owned(),
        origin_invoice_id: "inv-1".to_owned(),
        currency: "USD".to_owned(),
        amount_minor: 1,
        recognized_part_minor: 1,
        deferred_part_minor: 0,
        created_at_utc: OffsetDateTime::now_utc(),
    };
    let scope_b = scope.clone();
    let dup = provider
        .transaction(move |tx| {
            Box::pin(async move {
                AdjustmentRepo::insert_debit_note(tx, &scope_b, &note2)
                    .await
                    .map_err(repo_to_db)
            })
        })
        .await;
    assert!(
        dup.is_err(),
        "duplicate debit_note_id must collide on the PK"
    );
}
