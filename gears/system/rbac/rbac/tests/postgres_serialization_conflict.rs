//! End-to-end Postgres SQLSTATE-40001 mapping integration test.
//!
//! Two concurrent SERIALIZABLE transactions perform a classic write-skew
//! (each reads the row the other is about to write). PostgreSQL's SSI
//! detector rolls back one of them with SQLSTATE 40001. The test then
//! passes the failing `sqlx::Error` through the same `DbErr`-wrapping
//! that SeaORM would produce in production, and asserts the rbac error
//! classifier (`classify_db_err_to_domain`) maps it to
//! `DomainError::Aborted { reason: "SERIALIZATION_CONFLICT", .. }`.
//!
//! Closes the gap the canonical-mapping unit test cannot catch: the unit
//! test feeds the classifier a hand-crafted `"...SQLSTATE 40001"` string,
//! so a future change to sqlx's error Display (or to
//! `toolkit_db::contention::is_retryable_contention`'s substring scan)
//! could let real PG conflicts misclassify as `Internal` while the unit
//! test stays green.
//!
//! # Known failure
//!
//! On the current revision of `toolkit_db::contention::is_retryable_contention`,
//! this test **fails** because the detector does a substring-scan for
//! `"40001"` on the error's `Display` output, but real
//! `sqlx::Error::Database` only renders as
//! `"error returned from database: could not serialize access due to
//! read/write dependencies among transactions"` — the SQLSTATE never
//! appears in the string. The unit test in
//! `src/infra/canonical_mapping_tests.rs` hand-crafts a string that
//! does contain `"40001"`, so it passes; the integration test against
//! a real Postgres surfaces the gap. The fix needs a structured
//! SQLSTATE probe via `sqlx::DatabaseError::code()` in
//! `toolkit_db::contention::is_retryable_contention` (or in rbac's
//! adapter `is_serialization_failure`), tracked as a separate ticket.
//! Until that lands, the test stays `#[ignore]`d with a reason that
//! names the underlying bug — running `cargo test --ignored` will
//! surface the failure loudly, which is the intended alarm.

// Probes raw `sqlx::Error::into_database_error()` to confirm the real
// SQLSTATE, mirroring `postgres_constraints.rs`. DE0706 silenced with
// the same rationale.
#![cfg(test)]
#![allow(clippy::expect_used, clippy::panic, clippy::doc_markdown)]
#![allow(unknown_lints, de0706_no_direct_sqlx)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use rbac::domain::DomainError;
use rbac::infra::classify_db_err_to_domain;
use sea_orm::{DbErr, RuntimeErr};
use sqlx::{Connection, Executor, PgConnection, Row};
use tokio::sync::Barrier;
use tokio::time::timeout;
use uuid::Uuid;

/// Hard upper bound for both transactions; SSI detection completes in
/// milliseconds against a local container.
const TXN_TIMEOUT: Duration = Duration::from_secs(15);

const SQLSTATE_SERIALIZATION_FAILURE: &str = "40001";

/// A real PostgreSQL SSI rollback must reach `DomainError::Aborted`.
///
/// This was long carried as a `#[should_panic]` known-failure guard: the
/// underlying `toolkit_db::contention::is_retryable_contention` classified by
/// scanning the error's `Display` for SQLSTATE `40001`, which sqlx's `Display`
/// never emits. That is fixed upstream — `is_pg_contention` now also matches
/// the server message (`"could not serialize access"`), which the `Display`
/// does carry — so the guard is a plain passing test again, exactly as its
/// former note instructed once the fix shipped.
///
/// The message match is `lc_messages`-dependent by nature. A server running
/// under a non-English locale would emit different text and fall back to the
/// numeric-code checks, which the same `Display` gap defeats — so if this test
/// ever starts failing on a differently-configured container, the fix is in
/// `toolkit_db` (classify via `sqlx::DatabaseError::code()`), not here.
///
/// `inner()` carries the `Result<()>` scaffolding so the assertions can use
/// `?`; the test body only unwraps it.
#[tokio::test]
#[ignore = "Needs Docker; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn pg_sqlstate_40001_round_trips_to_domain_aborted() {
    inner()
        .await
        .expect("test scaffolding failed before the classifier was even invoked");
}

async fn inner() -> Result<()> {
    let db = common::bring_up_migrated_postgres().await?;

    // Seed two rows so each transaction can read one and update the
    // other — the canonical SSI write-skew shape.
    let row_a = Uuid::new_v4();
    let row_b = Uuid::new_v4();
    common::insert_canonical_built_in_role(&db.pool, row_a, "serialization-conflict-A").await?;
    common::insert_canonical_built_in_role(&db.pool, row_b, "serialization-conflict-B").await?;

    // Two raw connections so the transactions can interleave.
    let mut conn_a = PgConnection::connect(&db.url)
        .await
        .context("conn_a: open raw sqlx connection")?;
    let mut conn_b = PgConnection::connect(&db.url)
        .await
        .context("conn_b: open raw sqlx connection")?;

    let barrier = Arc::new(Barrier::new(2));
    let barrier_a = Arc::clone(&barrier);
    let barrier_b = Arc::clone(&barrier);

    let task_a =
        tokio::spawn(async move { run_write_skew_txn(&mut conn_a, row_a, row_b, barrier_a).await });
    let task_b =
        tokio::spawn(async move { run_write_skew_txn(&mut conn_b, row_b, row_a, barrier_b).await });

    let (a_res, b_res) = tokio::join!(timeout(TXN_TIMEOUT, task_a), timeout(TXN_TIMEOUT, task_b),);
    let a_outcome = a_res
        .expect("task A timed out \u{2014} SSI detector should resolve in ms")
        .expect("task A panicked");
    let b_outcome = b_res
        .expect("task B timed out \u{2014} SSI detector should resolve in ms")
        .expect("task B panicked");

    // Exactly one MUST succeed and one MUST fail with SQLSTATE 40001.
    let failing = match (a_outcome, b_outcome) {
        (Ok(()), Err(e)) | (Err(e), Ok(())) => e,
        (Ok(()), Ok(())) => panic!(
            "both serializable transactions committed; PG's SSI detector \
             should have rolled one back. Schema / index changes may have \
             broken the read-set/write-set anomaly this test relies on."
        ),
        (Err(a), Err(b)) => panic!(
            "both serializable transactions failed; expected exactly one \
             SSI rollback. a={a:?} b={b:?}"
        ),
    };

    // Confirm the failing error really carries SQLSTATE 40001 — guards
    // against the test mis-detecting some unrelated sqlx failure.
    let db_diag = failing
        .as_database_error()
        .expect("failing error MUST be a database error carrying a SQLSTATE");
    let state = db_diag
        .code()
        .expect("PG database errors MUST expose a SQLSTATE code");
    assert_eq!(
        state.as_ref(),
        SQLSTATE_SERIALIZATION_FAILURE,
        "expected SQLSTATE {SQLSTATE_SERIALIZATION_FAILURE} (serialization_failure); \
         got {state}. Test setup may have triggered a different anomaly class."
    );

    // Wrap the sqlx error the same way SeaORM does in production
    // (`DbErr::Exec(RuntimeErr::SqlxError(_))`), then run it through
    // the rbac classifier and assert the typed Domain mapping.
    // SeaORM 2 carries the sqlx error behind an `Arc` in `RuntimeErr::SqlxError`.
    let db_err = DbErr::Exec(RuntimeErr::SqlxError(Arc::new(failing)));
    match classify_db_err_to_domain(db_err) {
        DomainError::Aborted { reason, detail } => {
            assert_eq!(
                reason, "SERIALIZATION_CONFLICT",
                "Aborted.reason MUST be the canonical SERIALIZATION_CONFLICT tag; got {reason}"
            );
            assert!(
                detail.contains("serialization conflict"),
                "Aborted.detail should name the conflict class for operator triage, \
                 got: {detail}"
            );
        }
        other => panic!(
            "real PG SQLSTATE-40001 misclassified by classify_db_err_to_domain: {other:?}. \
             A change in sqlx's error Display or in toolkit_db::is_retryable_contention's \
             substring scan likely broke the production classifier without the \
             unit test in canonical_mapping_tests.rs noticing."
        ),
    }

    Ok(())
}

/// Run one half of the write-skew pair: read `read_row`, wait at the
/// barrier (so both transactions have completed their reads before
/// either writes), then update `write_row`. Returns the commit result.
async fn run_write_skew_txn(
    conn: &mut PgConnection,
    read_row: Uuid,
    write_row: Uuid,
    barrier: Arc<Barrier>,
) -> Result<(), sqlx::Error> {
    let mut txn = conn.begin().await?;
    txn.execute("SET TRANSACTION ISOLATION LEVEL SERIALIZABLE")
        .await?;

    // Read-set: SIReadLock placed on the row this transaction observes.
    let _seen: String = sqlx::query("SELECT created_by FROM role_definitions WHERE id = $1")
        .bind(read_row)
        .fetch_one(&mut *txn)
        .await?
        .get("created_by");

    // Synchronise so both txns finish their reads before either writes —
    // makes the write-skew anomaly visible to PG's SSI detector.
    barrier.wait().await;

    // Write-set: update the row the peer just read. Touching `updated_at`
    // and `created_by` is enough to mark the row dirty.
    sqlx::query(
        "UPDATE role_definitions \
         SET created_by = $1, updated_at = now() \
         WHERE id = $2",
    )
    .bind(format!("r18-{read_row}"))
    .bind(write_row)
    .execute(&mut *txn)
    .await?;

    txn.commit().await
}
