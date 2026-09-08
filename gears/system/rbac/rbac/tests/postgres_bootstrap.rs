//! Platform-admin bootstrap integration tests against an ephemeral
//! PostgreSQL — migrations + seeder + `BootstrapPlatformAdmin::run(...)`,
//! results verified via raw sqlx.
//!
//! ```bash
//! cargo test -p cf-gears-rbac -- --ignored postgres_bootstrap
//! ```

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
use rbac::infra::bootstrap::{
    BootstrapOutcome, BootstrapPlatformAdmin, OWNER_ROLE_ID, SYSTEM_BOOTSTRAP_CREATED_BY,
};
use rbac::infra::seeder::BuiltinRoleSeeder;
/// These tests assert against the full built-in roster, so they seed the
/// integration roles too (`Credstore Secret Operator`, `Usage Emitter`) —
/// the same choice a deployment running those gears makes in config.
const SEED_INTEGRATION_ROLES: bool = true;

/// Upper bound for the concurrent-bootstrap join. Two
/// `ON CONFLICT DO NOTHING` INSERTs usually complete in < 100 ms; the
/// generous budget lets a hung deadlock surface as a test failure rather
/// than CI timeout noise.
const BOOTSTRAP_TIMEOUT: Duration = Duration::from_secs(30);

/// Run seeder so the Owner FK target exists, then return a fresh bootstrap.
async fn seed_and_get_bootstrap(url: &str) -> (toolkit_db::Db, BootstrapPlatformAdmin) {
    let db = connect_db(url, ConnectOpts::default())
        .await
        .expect("connect_db must succeed");
    let conn = db.conn().expect("db.conn() must succeed");
    BuiltinRoleSeeder::new()
        .seed(
            &conn,
            SEED_INTEGRATION_ROLES,
            &BuiltinRoleTargets::default(),
        )
        .await
        .expect("seeder MUST succeed before bootstrap");
    (db, BootstrapPlatformAdmin::new())
}

/// Happy path: first boot inserts a `role_assignments` row matching
/// every normative field.
#[tokio::test]
#[ignore = "needs Docker; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn bootstrap_inserts_owner_at_root_on_first_run() -> Result<()> {
    let fixture = common::bring_up_migrated_postgres().await?;
    let (db, bootstrap) = seed_and_get_bootstrap(&fixture.url).await;
    let conn = db.conn()?;

    let subject = "user-test-b1";
    let outcome = bootstrap
        .run(&conn, subject)
        .await
        .expect("bootstrap MUST succeed on first run");

    assert_eq!(
        outcome,
        BootstrapOutcome::Created,
        "first run MUST return Created"
    );

    let row = sqlx::query(
        "SELECT role_definition_id, principal_id, principal_type, scope, \
         created_by, scope_depth FROM role_assignments \
         WHERE principal_id = $1",
    )
    .bind(subject)
    .fetch_one(&fixture.pool)
    .await
    .expect("role_assignments MUST contain the bootstrap row");

    let role_def_id: uuid::Uuid = row.get("role_definition_id");
    assert_eq!(
        role_def_id, OWNER_ROLE_ID,
        "role_definition_id MUST equal the canonical Owner UUID"
    );
    assert_eq!(
        row.get::<String, _>("principal_id"),
        subject,
        "principal_id MUST match the configured subject"
    );
    assert_eq!(
        row.get::<String, _>("principal_type"),
        "User",
        "principal_type MUST be 'User'"
    );
    assert_eq!(row.get::<String, _>("scope"), "/", "scope MUST be '/'");
    assert_eq!(
        row.get::<String, _>("created_by"),
        SYSTEM_BOOTSTRAP_CREATED_BY,
        "created_by MUST be 'system-bootstrap'"
    );
    // scope_depth = Scope::root().depth() = 1, set by the application.
    assert_eq!(
        row.get::<i32, _>("scope_depth"),
        1,
        "scope_depth MUST be 1 for scope='/' (Scope::root().depth())"
    );

    Ok(())
}

/// Re-bootstrap is idempotent: second call returns `AlreadyAssigned`
/// and exactly one row exists.
#[tokio::test]
#[ignore = "needs Docker; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn re_bootstrap_is_idempotent() -> Result<()> {
    let fixture = common::bring_up_migrated_postgres().await?;
    let (db, bootstrap) = seed_and_get_bootstrap(&fixture.url).await;
    let conn = db.conn()?;

    let subject = "user-test-b2";
    let outcome1 = bootstrap.run(&conn, subject).await?;
    assert_eq!(
        outcome1,
        BootstrapOutcome::Created,
        "first run MUST return Created"
    );

    let outcome2 = bootstrap.run(&conn, subject).await?;
    assert_eq!(
        outcome2,
        BootstrapOutcome::AlreadyAssigned,
        "second run with same subject MUST return AlreadyAssigned"
    );

    let row = sqlx::query(
        "SELECT COUNT(*)::bigint AS n FROM role_assignments \
         WHERE principal_id = $1 AND role_definition_id = $2 AND scope = '/'",
    )
    .bind(subject)
    .bind(OWNER_ROLE_ID)
    .fetch_one(&fixture.pool)
    .await?;
    let n: i64 = row.get("n");
    assert_eq!(
        n, 1,
        "exactly one assignment row MUST exist after two bootstrap calls"
    );

    Ok(())
}

/// 1 — a pre-existing Group / ServicePrincipal row for the same
/// `(role_definition_id, principal_id, scope)` MUST NOT mask the User-row
/// bootstrap. Regression for a presence-check bug that left the platform
/// without an admin grant.
#[tokio::test]
#[ignore = "needs Docker; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn bootstrap_creates_user_row_when_group_row_with_same_principal_id_exists() -> Result<()> {
    let fixture = common::bring_up_migrated_postgres().await?;
    let (db, bootstrap) = seed_and_get_bootstrap(&fixture.url).await;
    let conn = db.conn()?;

    let subject = "user-test-b2-1";

    // Pre-seed a Group row sharing (role, principal_id, scope) — only
    // `principal_type` distinguishes them under `uq_assignment`.
    // `scope_depth` / `tenant_id` must be supplied explicitly, matching
    // what the repo writes for `Scope::root()`.
    sqlx::query(
        "INSERT INTO role_assignments (id, role_definition_id, principal_id, principal_type, \
         scope, scope_depth, tenant_id, created_by) \
         VALUES ($1, $2, $3, 'Group', '/', 1, NULL, 'tester')",
    )
    .bind(uuid::Uuid::now_v7())
    .bind(OWNER_ROLE_ID)
    .bind(subject)
    .execute(&fixture.pool)
    .await
    .expect("pre-seed Group row must succeed");

    let outcome = bootstrap
        .run(&conn, subject)
        .await
        .expect("bootstrap MUST succeed when a Group row already exists");
    assert_eq!(
        outcome,
        BootstrapOutcome::Created,
        "outcome MUST be Created (a sibling Group row must not mask the User bootstrap)"
    );

    let row = sqlx::query(
        "SELECT COUNT(*) FILTER (WHERE principal_type = 'User')  AS user_n, \
                COUNT(*) FILTER (WHERE principal_type = 'Group') AS group_n \
         FROM role_assignments \
         WHERE principal_id = $1 AND role_definition_id = $2 AND scope = '/'",
    )
    .bind(subject)
    .bind(OWNER_ROLE_ID)
    .fetch_one(&fixture.pool)
    .await?;
    let user_n: i64 = row.get("user_n");
    let group_n: i64 = row.get("group_n");
    assert_eq!(user_n, 1, "exactly one User row MUST exist post-bootstrap");
    assert_eq!(group_n, 1, "the pre-seeded Group row MUST remain");

    Ok(())
}

/// Concurrent bootstraps converge to exactly one row.
///
/// A `tokio::sync::Barrier` makes the race deterministic: both tasks
/// complete `connect_db` first, then `barrier.wait().await` immediately
/// before `bootstrap.run(...)` so both hit Postgres at the same instant.
/// With `INSERT … ON CONFLICT … DO NOTHING` only one INSERT can win the
/// `uq_assignment` arbiter, so the outcomes MUST be exactly one
/// `Created` and one `AlreadyAssigned` — the stronger invariant the
/// pre-`ON CONFLICT` SELECT-then-INSERT path could not guarantee.
///
/// `tokio::time::timeout` surfaces a hung deadlock as a test failure
/// rather than a CI timeout (matches the seeder pattern in
/// `postgres_concurrency.rs`).
#[tokio::test]
#[ignore = "needs Docker; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn concurrent_bootstrap_converges() -> Result<()> {
    let fixture = common::bring_up_migrated_postgres().await?;
    let (db, _) = seed_and_get_bootstrap(&fixture.url).await;
    drop(db);

    let url = fixture.url.clone();
    let subject = "user-test-b3";

    let barrier = Arc::new(Barrier::new(2));

    let url1 = url.clone();
    let subject1 = subject.to_owned();
    let barrier1 = Arc::clone(&barrier);
    let task1 = tokio::spawn(async move {
        let db = connect_db(&url1, ConnectOpts::default())
            .await
            .expect("task1: connect");
        let conn = db.conn().expect("task1: conn");
        // Synchronise: both tasks hold here until both have connected,
        // so the bootstrap INSERTs race for the same `uq_assignment` key.
        barrier1.wait().await;
        BootstrapPlatformAdmin::new().run(&conn, &subject1).await
    });

    let url2 = url.clone();
    let subject2 = subject.to_owned();
    let barrier2 = Arc::clone(&barrier);
    let task2 = tokio::spawn(async move {
        let db = connect_db(&url2, ConnectOpts::default())
            .await
            .expect("task2: connect");
        let conn = db.conn().expect("task2: conn");
        barrier2.wait().await;
        BootstrapPlatformAdmin::new().run(&conn, &subject2).await
    });

    let (r1, r2) = tokio::join!(
        timeout(BOOTSTRAP_TIMEOUT, task1),
        timeout(BOOTSTRAP_TIMEOUT, task2),
    );
    let o1 = r1
        .expect("task1 timed out \u{2014} possible deadlock")
        .expect("task1 panicked")
        .expect("task1 MUST succeed");
    let o2 = r2
        .expect("task2 timed out \u{2014} possible deadlock")
        .expect("task2 panicked")
        .expect("task2 MUST succeed");

    // With `ON CONFLICT DO NOTHING` against a shared `uq_assignment`
    // arbiter, exactly one INSERT can return `Created`; the race-loser
    // MUST observe `AlreadyAssigned`. Both `Created` is impossible
    // (would mean two rows landed); both `AlreadyAssigned` is impossible
    // (no row was present at start of test).
    let created_count = [&o1, &o2]
        .iter()
        .filter(|o| matches!(***o, BootstrapOutcome::Created))
        .count();
    let already_assigned_count = [&o1, &o2]
        .iter()
        .filter(|o| matches!(***o, BootstrapOutcome::AlreadyAssigned))
        .count();
    assert_eq!(
        created_count, 1,
        "exactly one task MUST observe Created, got o1={o1:?} o2={o2:?}"
    );
    assert_eq!(
        already_assigned_count, 1,
        "exactly one task MUST observe AlreadyAssigned (race-loser via ON CONFLICT), \
         got o1={o1:?} o2={o2:?}"
    );

    let row = sqlx::query(
        "SELECT COUNT(*)::bigint AS n FROM role_assignments \
         WHERE principal_id = $1 AND role_definition_id = $2 AND scope = '/'",
    )
    .bind(subject)
    .bind(OWNER_ROLE_ID)
    .fetch_one(&fixture.pool)
    .await?;
    let n: i64 = row.get("n");
    assert_eq!(
        n, 1,
        "concurrent bootstraps MUST converge to exactly one row"
    );

    Ok(())
}

/// Bootstrap failure propagates: deleting the Owner row triggers
/// an INSERT-time FK violation that surfaces as `Err(_)`.
#[tokio::test]
#[ignore = "needs Docker; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn bootstrap_failure_propagates() -> Result<()> {
    let fixture = common::bring_up_migrated_postgres().await?;
    let (db, bootstrap) = seed_and_get_bootstrap(&fixture.url).await;

    sqlx::query("DELETE FROM role_definitions WHERE id = $1")
        .bind(OWNER_ROLE_ID)
        .execute(&fixture.pool)
        .await
        .expect("DELETE of Owner role MUST succeed (seeder just created it)");

    let conn = db.conn()?;
    let result = bootstrap.run(&conn, "user-test-b4").await;

    assert!(
        result.is_err(),
        "bootstrap MUST return Err(_) when the Owner role FK target is missing; \
         got: {result:?}"
    );

    let row = sqlx::query(
        "SELECT COUNT(*)::bigint AS n FROM role_assignments WHERE principal_id = 'user-test-b4'",
    )
    .fetch_one(&fixture.pool)
    .await?;
    let n: i64 = row.get("n");
    assert_eq!(
        n, 0,
        "no role_assignments row MUST remain after a failed bootstrap"
    );

    Ok(())
}
