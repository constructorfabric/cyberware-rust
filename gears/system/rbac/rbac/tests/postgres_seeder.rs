//! Built-in role seeder integration tests against an ephemeral
//! PostgreSQL. One test verifies the happy path; another tampers a row between two
//! seeder runs (flipping `is_built_in = false` while supplying a
//! non-NULL `owner_tenant_id` to satisfy the bi-conditional CHECK) and
//! asserts `verify_seeded_invariants` returns `Internal`.

// The tamper test needs to bypass SecureORM's built-in-invariant guard to set up the
// tamper-detection path, so raw sqlx is used and DE0706 silenced here.
#![cfg(test)]
#![allow(clippy::expect_used, clippy::doc_markdown)]
#![allow(unknown_lints, de0706_no_direct_sqlx)]

mod common;

use anyhow::Result;
use sqlx::Row;
use toolkit_db::{ConnectOpts, connect_db};
use uuid::Uuid;

use rbac::config::BuiltinRoleTargets;
use rbac::infra::seeder::BuiltinRoleSeeder;
use rbac_sdk::error::RbacServiceError;
/// These tests assert against the full built-in roster, so they seed the
/// integration roles too (`Credstore Secret Operator`, `Usage Emitter`) —
/// the same choice a deployment running those gears makes in config.
const SEED_INTEGRATION_ROLES: bool = true;

/// Happy path: the seeder produces one `is_built_in` row per canonical
/// built-in role, each with `owner_tenant_id IS NULL`. The expected count is
/// derived from the catalog, so adding a role does not require editing this
/// test.
#[tokio::test]
#[ignore = "needs Docker; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn seed_succeeds_and_seeds_the_whole_canonical_roster() -> Result<()> {
    let fixture = common::bring_up_migrated_postgres().await?;

    let db = connect_db(&fixture.url, ConnectOpts::default()).await?;
    let conn = db.conn()?;
    BuiltinRoleSeeder::new()
        .seed(
            &conn,
            SEED_INTEGRATION_ROLES,
            &BuiltinRoleTargets::default(),
        )
        .await
        .expect("seeder MUST succeed against a freshly-migrated database");

    let count_row =
        sqlx::query("SELECT count(*)::bigint AS n FROM role_definitions WHERE is_built_in")
            .fetch_one(&fixture.pool)
            .await?;
    let n: i64 = count_row.get("n");
    let expected = i64::try_from(BuiltinRoleSeeder::role_count(SEED_INTEGRATION_ROLES))
        .expect("roster size MUST fit in i64");
    assert_eq!(
        n, expected,
        "seeder MUST produce one built-in row per canonical role \
         ({expected}); got {n}",
    );

    // Independent raw-sqlx read-back confirms what landed in Postgres.
    let tampered_row = sqlx::query(
        "SELECT id FROM role_definitions WHERE is_built_in AND owner_tenant_id IS NOT NULL LIMIT 1",
    )
    .fetch_optional(&fixture.pool)
    .await?;
    assert!(
        tampered_row.is_none(),
        "every built-in row MUST have owner_tenant_id IS NULL after seeding \
         (DB CHECK constraint)",
    );

    Ok(())
}

/// After seeding, the `is_built_in = true` count equals the canonical roster
/// size — the roster is exhaustive and no extras leak in.
#[tokio::test]
#[ignore = "needs Docker; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn roster_count_matches_the_canonical_catalog() -> Result<()> {
    let fixture = common::bring_up_migrated_postgres().await?;

    let db = connect_db(&fixture.url, ConnectOpts::default()).await?;
    let conn = db.conn()?;
    BuiltinRoleSeeder::new()
        .seed(
            &conn,
            SEED_INTEGRATION_ROLES,
            &BuiltinRoleTargets::default(),
        )
        .await
        .expect("seeder MUST succeed against a freshly-migrated database");

    let row =
        sqlx::query("SELECT COUNT(*)::bigint AS n FROM role_definitions WHERE is_built_in = true")
            .fetch_one(&fixture.pool)
            .await?;
    let n: i64 = row.get("n");
    let expected = i64::try_from(BuiltinRoleSeeder::role_count(SEED_INTEGRATION_ROLES))
        .expect("roster size MUST fit in i64");
    assert_eq!(
        n, expected,
        "F1b: roster MUST be exhaustive — exactly {expected} built-in rows; got {n}",
    );

    Ok(())
}

/// Invariant-violation tamper: a row flipped to `is_built_in = false`
/// between seeder runs MUST surface as `Err(Internal { .. })` from the
/// second run's `verify_seeded_invariants` pass.
#[tokio::test]
#[ignore = "needs Docker; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn seed_aborts_when_built_in_invariant_is_violated() -> Result<()> {
    let fixture = common::bring_up_migrated_postgres().await?;

    // Step 1: first seed — every canonical row lands.
    {
        let db = connect_db(&fixture.url, ConnectOpts::default()).await?;
        let conn = db.conn()?;
        BuiltinRoleSeeder::new()
            .seed(
                &conn,
                SEED_INTEGRATION_ROLES,
                &BuiltinRoleTargets::default(),
            )
            .await
            .expect("first seed MUST succeed against a freshly-migrated database");
    }

    // Step 2: tamper the 'Owner' row. Pair `is_built_in = false` with a
    // synthetic `owner_tenant_id` to satisfy the bi-conditional CHECK.
    let synthetic_tenant_id = Uuid::new_v4();
    let rows_updated = sqlx::query(
        "UPDATE role_definitions \
         SET is_built_in = false, owner_tenant_id = $1 \
         WHERE name = 'Owner'",
    )
    .bind(synthetic_tenant_id)
    .execute(&fixture.pool)
    .await?
    .rows_affected();
    assert_eq!(
        rows_updated, 1,
        "tamper UPDATE MUST affect exactly one row (the 'Owner' built-in); got {rows_updated}",
    );

    // Step 3: second seed against the tampered DB.
    let db2 = connect_db(&fixture.url, ConnectOpts::default()).await?;
    let conn2 = db2.conn()?;
    let result = BuiltinRoleSeeder::new()
        .seed(
            &conn2,
            SEED_INTEGRATION_ROLES,
            &BuiltinRoleTargets::default(),
        )
        .await;

    assert!(
        matches!(result, Err(RbacServiceError::Internal { .. })),
        "seeder MUST return RbacServiceError::Internal when the \
         is_built_in invariant is violated; got: {result:?}",
    );

    Ok(())
}

/// The seeder's writes MUST participate in the caller's transaction.
///
/// `init()` runs the whole roster inside one transaction so a crash
/// mid-roster cannot leave the built-in catalogue half seeded. That
/// property lives entirely in the *caller*, so it needs its own guard:
/// without one, someone could hand the seeder a plain connection again and
/// every existing seeder test would still pass.
///
/// The test seeds a fresh database inside a transaction that then fails.
/// If the writes were auto-committed per role, the rows would survive.
#[tokio::test]
#[ignore = "needs Docker; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn seeded_roster_rolls_back_with_the_callers_transaction() -> Result<()> {
    let fixture = common::bring_up_migrated_postgres().await?;
    let db = connect_db(&fixture.url, ConnectOpts::default()).await?;
    let provider: toolkit_db::DBProvider<toolkit_db::DbError> = toolkit_db::DBProvider::new(db);

    let outcome: Result<(), toolkit_db::DbError> = provider
        .transaction(|tx| {
            Box::pin(async move {
                BuiltinRoleSeeder::new()
                    .seed(tx, SEED_INTEGRATION_ROLES, &BuiltinRoleTargets::default())
                    .await
                    .map_err(|err| {
                        toolkit_db::DbError::Other(anyhow::anyhow!("seed failed: {err}"))
                    })?;
                // Fail *after* the roster is written, standing in for the
                // crash the transaction exists to survive.
                Err(toolkit_db::DbError::Other(anyhow::anyhow!(
                    "deliberate post-seed failure"
                )))
            })
        })
        .await;

    assert!(
        outcome.is_err(),
        "the deliberate failure must surface, not be swallowed"
    );

    let remaining: i64 = sqlx::query("SELECT count(*) AS c FROM role_definitions")
        .fetch_one(&fixture.pool)
        .await?
        .get("c");
    assert_eq!(
        remaining, 0,
        "a failed seeding transaction MUST leave no built-in rows behind; \
         found {remaining} — the seeder's writes are not joining the \
         caller's transaction"
    );
    Ok(())
}
