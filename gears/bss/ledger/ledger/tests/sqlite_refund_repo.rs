//! Fast SQLite integration tests for the Slice-3 Phase-2 refund repo guarantees
//! (Group B3), exercised at the repo layer — the cheap half of the Phase-2
//! integration matrix that does not need Docker/testcontainers. The full handler
//! end-to-end (chart provisioning + projector + post engine + the two-stage
//! REFUND_CLEARING drain) is a Postgres-only test (`postgres_refund.rs`,
//! `#[ignore]`).
//!
//! Covered:
//! - **`insert_refund` round-trips** (B3): a `refund` row persists with all fields
//!   (Pattern A with NULL invoice_id; Pattern B with an invoice_id).
//! - **the surrogate PK collides** (B3): a duplicate `(tenant, refund_id)` is a
//!   `Db` error (the engine's idempotency claim normally short-circuits this).
//! - **the natural UNIQUE `(tenant, psp_refund_id, phase)` collides** (B3 / design
//!   §7): two DIFFERENT `refund_id`s carrying the SAME `(psp_refund_id, phase)`
//!   collide on `uq_ledger_refund_psp_phase` — the idempotency grain is enforced at
//!   the row even past the surrogate PK.
//! - **distinct phases of one PSP refund coexist** (B3): `(psp, initiated)` +
//!   `(psp, confirmed)` both persist (one PSP refund advances through phase rows).

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
use bss_ledger::infra::storage::repo::adjustment_repo::NewRefund;
use sea_orm_migration::MigratorTrait;
use time::OffsetDateTime;
use toolkit_db::migration_runner::run_migrations_for_testing;
use toolkit_db::secure::AccessScope;
use toolkit_db::{ConnectOpts, DBProvider, DbError, connect_db};
use uuid::Uuid;

/// Connect an in-memory SQLite + run the migrator (the same harness as the
/// note repo tests).
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
/// must return; the `RepoError` `Debug` is stamped into `DbError::Other`.
fn repo_to_db(e: RepoError) -> DbError {
    DbError::Other(anyhow::Error::msg(format!("{e:?}")))
}

/// A Pattern-A (`A_UNALLOCATED`) stage-1 refund row: NULL invoice_id, PENDING
/// clearing.
fn refund_a(tenant: Uuid, refund_id: &str, psp_refund_id: &str, phase: &str) -> NewRefund {
    NewRefund {
        tenant_id: tenant,
        refund_id: refund_id.to_owned(),
        psp_refund_id: psp_refund_id.to_owned(),
        phase: phase.to_owned(),
        pattern: "A_UNALLOCATED".to_owned(),
        payment_id: "pay-1".to_owned(),
        invoice_id: None,
        currency: "USD".to_owned(),
        amount_minor: 500,
        clearing_state: "PENDING".to_owned(),
        relates_to_refund_id: None,
        reverses_entry_id: None,
        created_at_utc: OffsetDateTime::now_utc(),
    }
}

/// Persist one refund row in a fresh txn, returning the txn result.
async fn insert(
    provider: &DBProvider<DbError>,
    scope: &AccessScope,
    row: NewRefund,
) -> Result<(), DbError> {
    let scope = scope.clone();
    provider
        .transaction(move |tx| {
            Box::pin(async move {
                AdjustmentRepo::insert_refund(tx, &scope, &row)
                    .await
                    .map_err(repo_to_db)
            })
        })
        .await
}

#[tokio::test]
async fn refund_row_round_trips_pattern_a_and_b() {
    let provider = provider().await;
    let tenant = Uuid::now_v7();
    let scope = AccessScope::for_tenant(tenant);

    // Pattern A: NULL invoice_id.
    insert(
        &provider,
        &scope,
        refund_a(tenant, "rf-a", "psp-a", "initiated"),
    )
    .await
    .expect("insert Pattern A refund");

    // Pattern B: an invoice_id, SETTLED single-step.
    let row_b = NewRefund {
        tenant_id: tenant,
        refund_id: "rf-b".to_owned(),
        psp_refund_id: "psp-b".to_owned(),
        phase: "initiated".to_owned(),
        pattern: "B_RESTORE_AR".to_owned(),
        payment_id: "pay-2".to_owned(),
        invoice_id: Some("inv-9".to_owned()),
        currency: "USD".to_owned(),
        amount_minor: 800,
        clearing_state: "SETTLED".to_owned(),
        relates_to_refund_id: None,
        reverses_entry_id: None,
        created_at_utc: OffsetDateTime::now_utc(),
    };
    insert(&provider, &scope, row_b)
        .await
        .expect("insert Pattern B refund");
}

#[tokio::test]
async fn duplicate_surrogate_pk_collides() {
    let provider = provider().await;
    let tenant = Uuid::now_v7();
    let scope = AccessScope::for_tenant(tenant);

    insert(
        &provider,
        &scope,
        refund_a(tenant, "rf-dup", "psp-1", "initiated"),
    )
    .await
    .expect("first insert");
    // Same (tenant, refund_id) — a different psp/phase cannot rescue the surrogate PK.
    let dup = insert(
        &provider,
        &scope,
        refund_a(tenant, "rf-dup", "psp-2", "confirmed"),
    )
    .await;
    assert!(
        dup.is_err(),
        "duplicate (tenant, refund_id) must collide on PK"
    );
}

#[tokio::test]
async fn duplicate_natural_psp_phase_collides() {
    let provider = provider().await;
    let tenant = Uuid::now_v7();
    let scope = AccessScope::for_tenant(tenant);

    // First row claims (psp-x, initiated) under refund_id rf-1.
    insert(
        &provider,
        &scope,
        refund_a(tenant, "rf-1", "psp-x", "initiated"),
    )
    .await
    .expect("first insert");
    // A DIFFERENT surrogate refund_id (rf-2) but the SAME (psp_refund_id, phase) must
    // collide on the natural UNIQUE index `uq_ledger_refund_psp_phase` (the
    // idempotency grain, design §7) — past the surrogate PK.
    let dup = insert(
        &provider,
        &scope,
        refund_a(tenant, "rf-2", "psp-x", "initiated"),
    )
    .await;
    assert!(
        dup.is_err(),
        "duplicate (tenant, psp_refund_id, phase) must collide on the natural UNIQUE index"
    );
}

#[tokio::test]
async fn distinct_phases_of_one_psp_refund_coexist() {
    let provider = provider().await;
    let tenant = Uuid::now_v7();
    let scope = AccessScope::for_tenant(tenant);

    // One PSP refund advances initiated → confirmed: two rows, same psp_refund_id,
    // distinct phase + distinct surrogate refund_id. Both persist.
    insert(
        &provider,
        &scope,
        refund_a(tenant, "rf-s1", "psp-multi", "initiated"),
    )
    .await
    .expect("stage-1 row");
    insert(
        &provider,
        &scope,
        refund_a(tenant, "rf-s2", "psp-multi", "confirmed"),
    )
    .await
    .expect("stage-2 row coexists (distinct phase)");
}
