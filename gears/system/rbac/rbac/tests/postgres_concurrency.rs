//! Concurrent seeder integration test.
//!
//! Two seeders run against the same Postgres behind a `Barrier`; the
//! test asserts both succeed, exactly the canonical built-in IDs
//! are present, and `pg_stat_database.deadlocks` is unchanged. Verifies
//! the runtime consequence of the ascending-id lock-ordering invariant.

// Test probes raw `pg_stat_database` (no SecureORM equivalent), so the
// DE0706 lint is silenced at file scope with this documented rationale.
#![cfg(test)]
#![allow(clippy::expect_used, clippy::doc_markdown)]
#![allow(unknown_lints, de0706_no_direct_sqlx)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use sqlx::Row;
use tokio::sync::Barrier;
use tokio::time::timeout;
use toolkit_db::{ConnectOpts, connect_db};

use rbac::config::BuiltinRoleTargets;
use rbac::infra::seeder::BuiltinRoleSeeder;
/// These tests assert against the full built-in roster, so they seed the
/// integration roles too (`Credstore Secret Operator`, `Usage Emitter`) —
/// the same choice a deployment running those gears makes in config.
const SEED_INTEGRATION_ROLES: bool = true;

const SEEDER_TIMEOUT: Duration = Duration::from_secs(30);

#[tokio::test]
#[ignore = "needs Docker; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn c4_two_concurrent_seeders_converge_without_deadlock() -> Result<()> {
    let db = common::bring_up_migrated_postgres().await?;

    let deadlocks_before = read_deadlocks_for_current_db(&db.pool).await?;

    let url_a = db.url.clone();
    let url_b = db.url.clone();
    let barrier = Arc::new(Barrier::new(2));
    let barrier_a = Arc::clone(&barrier);
    let barrier_b = Arc::clone(&barrier);

    let task_a = tokio::spawn(async move { run_seeder_against(url_a, barrier_a).await });
    let task_b = tokio::spawn(async move { run_seeder_against(url_b, barrier_b).await });

    // `timeout` surfaces a hung deadlock as a test failure (30s budget is
    // generous — one UPSERT per canonical role usually runs in < 100 ms).
    let (result_a, result_b) = tokio::join!(
        timeout(SEEDER_TIMEOUT, task_a),
        timeout(SEEDER_TIMEOUT, task_b)
    );
    result_a
        .expect("seeder A timed out \u{2014} possible deadlock")?
        .expect("seeder A panicked")
        .expect("seeder A returned an unexpected RbacServiceError");
    result_b
        .expect("seeder B timed out \u{2014} possible deadlock")?
        .expect("seeder B panicked")
        .expect("seeder B returned an unexpected RbacServiceError");

    let count = sqlx::query("SELECT count(*)::bigint AS n FROM role_definitions WHERE is_built_in")
        .fetch_one(&db.pool)
        .await?;
    let n: i64 = count.get("n");
    let expected = i64::try_from(BuiltinRoleSeeder::role_count(SEED_INTEGRATION_ROLES))
        .expect("roster size MUST fit in i64");
    assert_eq!(
        n, expected,
        "after two concurrent seeders the final built-in count MUST equal \
         the canonical catalog size ({expected}); got {n}",
    );

    let deadlocks_after = read_deadlocks_for_current_db(&db.pool).await?;
    assert_eq!(
        deadlocks_after, deadlocks_before,
        "pg_stat_database.deadlocks MUST NOT increase during concurrent seeding \
         (lock-ordering invariant — CANONICAL_BUILTIN_ROLES sorted ascending \
         by id closes the disjoint-pair deadlock class). \
         before={deadlocks_before}, after={deadlocks_after}",
    );

    Ok(())
}

/// Open a fresh `Db` and run `BuiltinRoleSeeder::seed`. The `Barrier`
/// releases both tasks simultaneously so UPSERTs overlap maximally.
async fn run_seeder_against(
    url: String,
    barrier: Arc<Barrier>,
) -> Result<Result<(), rbac_sdk::error::RbacServiceError>> {
    let opts = ConnectOpts::default();
    let db = connect_db(&url, opts).await?;
    let conn = db.conn()?;
    barrier.wait().await;
    Ok(BuiltinRoleSeeder::new()
        .seed(
            &conn,
            SEED_INTEGRATION_ROLES,
            &BuiltinRoleTargets::default(),
        )
        .await)
}

/// Read `pg_stat_database.deadlocks` for the test's database via
/// `current_database()`.
async fn read_deadlocks_for_current_db(pool: &sqlx::PgPool) -> Result<i64> {
    let row = sqlx::query(
        "SELECT deadlocks::bigint AS n FROM pg_stat_database WHERE datname = current_database()",
    )
    .fetch_one(pool)
    .await?;
    Ok(row.get("n"))
}
