//! `payload_hash`, and the takeover compare-and-swap.
//!
//! Most of this module is a property of a statement and is pinned in
//! `tests/sqlite_idempotency.rs`. Two things cannot live there.
//!
//! The first is `payload_hash`, which needs no database: the mismatch refusal
//! rests on the digest depending on the canonical rendering and on nothing else,
//! and on the smallest difference two renderings can have being enough to
//! separate them. A hasher that collapsed a one-byte edit would turn
//! `IDEMPOTENCY_PAYLOAD_MISMATCH` into a replay of the wrong answer.
//!
//! The second is [`take_over`]'s compare-and-swap, which needs a database *and*
//! a caller holding a row value the store has already moved past. The public
//! API cannot produce that: `claim` always reads the row immediately before it
//! acts on it, so no sequence of public calls hands `take_over` a stale `held`.
//! Reaching the private function directly is the only way to test the guard
//! **deterministically and single-threaded** — a concurrency test would prove
//! nothing on `SQLite`, whose single writer serializes the transactions the race
//! is made of. In-crate database tests have precedent
//! (`coord::lease::sqlite_tests`), and for the same reason: the assertion needs
//! something the public surface does not expose.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use sea_orm::ActiveValue::Set;
use sea_orm::{ColumnTrait, Condition, EntityTrait};
use sea_orm_migration::MigratorTrait;
use serde_json::json;
use toolkit_db::migration_runner::run_migrations_for_testing;
use toolkit_db::secure::{AccessScope, SecureEntityExt, SecureInsertExt};
use toolkit_db::{ConnectOpts, DBProvider, DbError, connect_db};
use uuid::Uuid;

use super::{ClaimOutcome, IdempotencyGate, take_over};
use crate::domain::instant::utc_ymd_hms;
use crate::infra::storage::RepoError;
use crate::infra::storage::entity::idempotency_dedup;
use crate::infra::storage::migrations::Migrator;
use time::OffsetDateTime;

/// The width of a SHA-256 digest, in bytes.
const SHA256_BYTES: usize = 32;

#[test]
fn one_rendering_always_digests_to_one_value() {
    // The retry arrives minutes later, on another node, from another process.
    // Nothing but the rendering may enter the digest, or an honest retry stops
    // matching its own claim.
    let canonical = "create_price\u{1f}plan-9f2a\u{1f}USD\u{1f}1200";

    assert_eq!(
        IdempotencyGate::payload_hash(canonical),
        IdempotencyGate::payload_hash(canonical)
    );
}

#[test]
fn a_single_byte_of_difference_makes_a_different_request() {
    // 1200 minor units and 1201 are different money. If the digest let that
    // through, the second request would be answered with the first one's
    // response and the price the caller asked for would never be written.
    let cheap = IdempotencyGate::payload_hash("create_price\u{1f}plan-9f2a\u{1f}USD\u{1f}1200");
    let dear = IdempotencyGate::payload_hash("create_price\u{1f}plan-9f2a\u{1f}USD\u{1f}1201");

    assert_ne!(cheap, dear);
}

#[test]
fn the_digest_is_a_whole_sha256() {
    // Stored as `bytea` and compared byte for byte, so a truncated digest would
    // silently widen what counts as "the same request".
    assert_eq!(
        IdempotencyGate::payload_hash("").len(),
        SHA256_BYTES,
        "the empty rendering still digests to a full SHA-256"
    );
    assert_eq!(
        IdempotencyGate::payload_hash("create_plan\u{1f}gold\u{1f}monthly").len(),
        SHA256_BYTES
    );
}

/// The tenant the takeover tests run as.
fn tenant() -> Uuid {
    Uuid::from_u128(0x7e_11)
}

/// Hours after a fixed origin instant.
fn at(hours: i64) -> OffsetDateTime {
    utc_ymd_hms(2026, 8, 2, 10, 0, 0) + time::Duration::hours(hours)
}

/// A migrated in-memory database holding one **answered** claim stamped `at(0)`,
/// plus that row as it reads back — the value a racing takeover would be holding.
async fn one_answered_claim() -> (DBProvider<DbError>, idempotency_dedup::Model) {
    let db = connect_db("sqlite::memory:", ConnectOpts::default())
        .await
        .expect("connect in-memory sqlite");
    run_migrations_for_testing(&db, Migrator::migrations())
        .await
        .expect("run migrator");
    let provider = DBProvider::<DbError>::new(db);
    let scope = AccessScope::for_tenant(tenant());

    let am = idempotency_dedup::ActiveModel {
        tenant_id: Set(tenant()),
        operation: Set("create_plan".to_owned()),
        client_key: Set("ck-race".to_owned()),
        request_hash: Set(IdempotencyGate::payload_hash("original")),
        response_status: Set(Some(201)),
        response_body: Set(Some(json!({ "planId": "9f2a" }))),
        created_at_utc: Set(at(0)),
    };
    let conn = provider.conn().expect("scoped connection");
    idempotency_dedup::Entity::insert(am.clone())
        .secure()
        .scope_with_model(&scope, &am)
        .expect("the scope permits the seed")
        .exec(&conn)
        .await
        .expect("seed the claim about to expire");

    let held = idempotency_dedup::Entity::find()
        .secure()
        .scope_with(&scope)
        .filter(Condition::all().add(idempotency_dedup::Column::TenantId.eq(tenant())))
        .one(&conn)
        .await
        .expect("read the seeded claim")
        .expect("the seed is there");
    (provider, held)
}

#[tokio::test]
async fn a_takeover_matches_the_row_it_read_and_only_that_row() {
    let (provider, held) = one_answered_claim().await;
    let scope = AccessScope::for_tenant(tenant());
    let winner = IdempotencyGate::payload_hash("winner");
    let loser = IdempotencyGate::payload_hash("loser");

    // Two takeovers driven from ONE `held`, which is exactly the state two
    // racing transactions are in: both cleared the conflict check, both read the
    // same expired row, and neither knows the other exists. Sequential here on
    // purpose — a concurrency test would prove nothing on `SQLite`, whose single
    // writer serializes the two transactions the race is made of, while this
    // reproduces the only thing that actually decides the race: whether the
    // second UPDATE still matches the row its `held` describes.
    let (winner_outcome, loser_outcome) = {
        let scope = scope.clone();
        let held = held.clone();
        let winner = winner.clone();
        let loser = loser.clone();
        provider
            .transaction(move |txn| {
                Box::pin(async move {
                    let first = take_over(txn, &scope, &held, &winner, at(25)).await;
                    let second = take_over(txn, &scope, &held, &loser, at(26)).await;
                    Ok((first, second))
                })
            })
            .await
            .expect("the transaction itself must commit")
    };

    assert_eq!(
        winner_outcome.expect("the first takeover matches the row it read"),
        ClaimOutcome::Claimed
    );
    // Without `created_at_utc = <the value that was read>` in the predicate the
    // second UPDATE would match on the primary key alone, both callers would be
    // told they claimed the key, and the guarded mutation would run twice under
    // one idempotency key.
    //
    // The loser is refused rather than handed an outcome it has nothing to do
    // with: the key it lost is claimed and unanswered, and D-143 gives that
    // state a code the surface can answer with. The refusal names the key the
    // winner now holds, which is the key the loser must retry.
    assert_eq!(
        loser_outcome.expect_err("the second takeover is refused, not claimed"),
        RepoError::IdempotencyKeyInFlight {
            operation: "create_plan".to_owned(),
            client_key: "ck-race".to_owned(),
        },
        "the row has moved past the value this caller read, so nothing matches"
    );

    // The winner owns the key outright, and the loser left no trace: not its
    // digest, not its timestamp.
    let conn = provider.conn().expect("scoped connection");
    let row = idempotency_dedup::Entity::find()
        .secure()
        .scope_with(&scope)
        .filter(Condition::all().add(idempotency_dedup::Column::TenantId.eq(tenant())))
        .one(&conn)
        .await
        .expect("read the row back")
        .expect("the row is there");
    assert_eq!(
        row.request_hash, winner,
        "the winner's payload owns the key"
    );
    assert_eq!(row.created_at_utc, at(25));
    assert_eq!(row.response_status, None, "the stale answer went with it");
    assert_eq!(row.response_body, None);
}
