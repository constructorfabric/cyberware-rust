//! Lock semantics against an in-memory `SQLite` database.
//!
//! Expiry is simulated by moving `locked_until` into the past with a direct
//! UPDATE, so no test waits on a clock.

#![allow(clippy::expect_used)]

use std::time::Duration;

use quota_enforcement_sdk::{CoordinationError, CoordinationPluginV1, LockScope};
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
use sea_orm_migration::MigratorTrait;
use toolkit_db::migration_runner::run_migrations_for_testing;
use toolkit_db::secure::{SecureEntityExt, SecureUpdateExt};
use toolkit_db::{ConnectOpts, Db, connect_db};
use toolkit_security::AccessScope;

use super::DbCoordination;
use crate::infra::storage::Migrator;
use crate::infra::storage::entity::coordination_lock as locks;

const TTL: Duration = Duration::from_mins(1);

/// One-connection in-memory `SQLite` with the lock table applied. A bare
/// `sqlite::memory:` gives every pooled connection its own database, so the
/// pool must hold exactly one connection.
async fn test_db() -> Db {
    let opts = ConnectOpts {
        max_conns: Some(1),
        min_conns: Some(1),
        ..ConnectOpts::default()
    };
    let db = connect_db("sqlite::memory:", opts)
        .await
        .expect("connect in-memory sqlite");
    run_migrations_for_testing(&db, Migrator::migrations())
        .await
        .expect("apply coordination migrations");
    db
}

async fn row(db: &Db, scope: LockScope) -> Option<locks::Model> {
    let conn = db.conn().expect("connection");
    locks::Entity::find_by_id(scope.key().to_owned())
        .secure()
        .scope_with(&AccessScope::allow_all())
        .one(&conn)
        .await
        .expect("read lock row")
}

/// Move the row's expiry into the past: the TTL has elapsed.
async fn expire(db: &Db, scope: LockScope) {
    let conn = db.conn().expect("connection");
    let result = locks::Entity::update_many()
        .col_expr(
            locks::Column::LockedUntil,
            Expr::cust("datetime('now', '-3600 seconds')"),
        )
        .filter(locks::Column::Key.eq(scope.key()))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .exec(&conn)
        .await
        .expect("expire row");
    assert_eq!(result.rows_affected, 1, "exactly one row is expired");
}

#[tokio::test]
async fn try_lock_grants_one_holder_and_a_contender_gets_conflict() {
    let db = test_db().await;
    let coord = DbCoordination::new(db.clone());

    let lock = coord
        .try_lock(LockScope::LeaseSweeper, TTL)
        .await
        .expect("first acquisition");
    assert_eq!(lock.scope(), LockScope::LeaseSweeper);
    assert_eq!(lock.ttl(), TTL);
    assert_eq!(
        lock.holder_id().get_version_num(),
        7,
        "holder ids are UUIDv7"
    );

    let stored = row(&db, LockScope::LeaseSweeper).await.expect("row exists");
    assert_eq!(stored.holder_id, Some(lock.holder_id()));
    assert_eq!(stored.attempts, 1);

    let contender = coord.try_lock(LockScope::LeaseSweeper, TTL).await;
    assert_eq!(
        contender,
        Err(CoordinationError::Conflict {
            scope: LockScope::LeaseSweeper,
        })
    );
    let after = row(&db, LockScope::LeaseSweeper).await.expect("row exists");
    assert_eq!(
        after.holder_id,
        Some(lock.holder_id()),
        "the loser changed nothing"
    );
    assert_eq!(after.attempts, 1);
}

#[tokio::test]
async fn scopes_are_independent_locks() {
    let db = test_db().await;
    let coord = DbCoordination::new(db.clone());
    let a = coord
        .try_lock(LockScope::LeaseSweeper, TTL)
        .await
        .expect("lease sweeper");
    let b = coord
        .try_lock(LockScope::RetentionSweeper, TTL)
        .await
        .expect("retention sweeper while the other scope is held");
    assert_ne!(a.holder_id(), b.holder_id());
    assert!(row(&db, LockScope::LeaseSweeper).await.is_some());
    assert!(row(&db, LockScope::RetentionSweeper).await.is_some());
}

#[tokio::test]
async fn release_frees_the_row_and_a_peer_reacquires_without_a_ttl_wait() {
    let db = test_db().await;
    let coord = DbCoordination::new(db.clone());
    let first = coord
        .try_lock(LockScope::RetentionSweeper, TTL)
        .await
        .expect("acquire");
    let first_holder = first.holder_id();

    coord.release(first).await.expect("release");
    let freed = row(&db, LockScope::RetentionSweeper)
        .await
        .expect("row kept");
    assert_eq!(freed.holder_id, None, "release clears the holder");
    assert_eq!(
        freed.attempts, 0,
        "a clean release resets the steal counter"
    );

    let second = coord
        .try_lock(LockScope::RetentionSweeper, TTL)
        .await
        .expect("handoff without waiting for the TTL");
    assert_ne!(second.holder_id(), first_holder);
    let taken = row(&db, LockScope::RetentionSweeper).await.expect("row");
    assert_eq!(taken.holder_id, Some(second.holder_id()));
    assert_eq!(
        taken.attempts, 1,
        "re-acquisition of a free row counts one attempt"
    );
}

#[tokio::test]
async fn expired_hold_is_stealable_and_the_old_holder_cannot_renew_or_evict() {
    let db = test_db().await;
    let coord = DbCoordination::new(db.clone());
    let old = coord
        .try_lock(LockScope::LeaseSweeper, TTL)
        .await
        .expect("acquire");

    expire(&db, LockScope::LeaseSweeper).await;

    let renew = coord.renew(&old).await;
    assert_eq!(
        renew,
        Err(CoordinationError::LockExpired {
            scope: LockScope::LeaseSweeper,
        }),
        "renewal must not revive an expired hold"
    );

    let survivor = coord
        .try_lock(LockScope::LeaseSweeper, TTL)
        .await
        .expect("a survivor acquires within one TTL");
    let stolen = row(&db, LockScope::LeaseSweeper).await.expect("row");
    assert_eq!(stolen.holder_id, Some(survivor.holder_id()));
    assert_eq!(stolen.attempts, 2, "steal increments the forensic counter");

    coord
        .release(old)
        .await
        .expect("a stale release is best-effort and never an error");
    let still = row(&db, LockScope::LeaseSweeper).await.expect("row");
    assert_eq!(
        still.holder_id,
        Some(survivor.holder_id()),
        "the stale holder must not evict the survivor"
    );

    let renewed = coord.renew(&survivor).await;
    assert_eq!(renewed, Ok(()), "the live holder renews");
}

#[tokio::test]
async fn renew_extends_a_live_hold_on_the_database_clock() {
    let db = test_db().await;
    let coord = DbCoordination::new(db.clone());
    let lock = coord
        .try_lock(LockScope::RetentionSweeper, Duration::from_secs(5))
        .await
        .expect("acquire");
    let before = row(&db, LockScope::RetentionSweeper)
        .await
        .expect("row")
        .locked_until;

    let long = quota_enforcement_sdk::Lock::new(
        lock.scope(),
        lock.holder_id(),
        Duration::from_hours(1),
        lock.acquired_at(),
    );
    coord.renew(&long).await.expect("renew with a longer ttl");
    let after = row(&db, LockScope::RetentionSweeper)
        .await
        .expect("row")
        .locked_until;
    assert!(
        after > before,
        "renew must push locked_until forward: {before} -> {after}"
    );
    assert!(
        after - before > time::Duration::minutes(50),
        "the extension follows the lock's TTL"
    );
}

#[tokio::test]
async fn bootstrap_probe_pattern_succeeds_repeatedly_for_every_scope() {
    let db = test_db().await;
    let coord = DbCoordination::new(db);
    for _round in 0..2 {
        for scope in LockScope::ALL {
            let lock = coord
                .try_lock(scope, Duration::from_secs(1))
                .await
                .expect("probe acquisition");
            coord.release(lock).await.expect("probe release");
        }
    }
}
