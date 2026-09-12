//! The at-most-once gate **on the engine that runs in production** (§9,
//! `inst-bk-idem`, D-143).
//!
//! # Why this file exists (Z12-7)
//!
//! `tests/sqlite_idempotency.rs` is 644 lines and drives the whole gate against
//! the mirror; until this file that was the *only* place the claim path ran. Every
//! `postgres_*` file naming the gate is a **schema** suite — the table, its
//! `CHECK`s, its key — so the behaviour over it was engine-untested, and the gate's
//! behaviour is more engine-dependent than most code in this crate:
//!
//! * the row is `bytea`/`jsonb`/`timestamptz` here and `blob`/`text`/`text` on the
//!   mirror, so the digest comparison, the replayed body and the expiry
//!   subtraction all run against different types than the ones the `SQLite`
//!   assertions cover;
//! * `claim`'s own doc names a `RepoError::Db` arm for *"the conflicting row being
//!   unreadable inside the transaction that just collided with it"* — an
//!   engine-specific hazard that had no engine-specific test at all;
//! * and the case `SQLite` **structurally cannot** run is the one the gate exists
//!   for. `sqlite_idempotency`'s own module doc says so: *"On `SQLite` a race
//!   cannot be staged anyway — one writer, transactions serialized — so a racing
//!   test would prove nothing while looking like it proved everything."* Two
//!   duplicates arriving at once is the situation the `PRIMARY KEY` and the
//!   `ON CONFLICT DO NOTHING` were chosen over a read-then-write for, and it is
//!   staged here.
//!
//! # This suite is the twin, not a second full pass
//!
//! The vocabulary cases — one key under another operation, another tenant's claim,
//! a status without a body — are facts about predicates that do not vary by
//! engine, and duplicating them here would double the slowest tier for nothing.
//! What is here is the set whose answer could differ: the round trip through the
//! real column types, the two refusals, the expiry boundary against a real
//! `timestamptz`, and the race.
//!
//! Run with:
//! `cargo test -p cf-gears-bss-pricing --test postgres_idempotency -- --ignored`.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod pg_support;

use std::sync::Arc;
use std::time::Duration;

use bss_pricing::config::LimitsConfig;
use bss_pricing::domain::instant::utc_ymd_hms;
use bss_pricing::infra::storage::RepoError;
use bss_pricing::infra::storage::entity::idempotency_dedup;
use bss_pricing::infra::storage::repo::{ClaimOutcome, IdempotencyGate};
use pg_support::Pg;
use sea_orm::{ColumnTrait, Condition, EntityTrait};
use serde_json::json;
use time::OffsetDateTime;
use tokio::sync::Notify;
use toolkit_db::secure::{AccessScope, SecureEntityExt};
use toolkit_db::{DBProvider, DbError};
use uuid::Uuid;

/// A staged race must not hang a run: every wait in this file is bounded.
const RACE_TIMEOUT: Duration = Duration::from_secs(30);

/// The operation this suite claims under.
const CREATE_PLAN: &str = "create_plan";

const CLIENT_KEY: &str = "ck-pg-9f2a";

fn owner() -> Uuid {
    Uuid::from_u128(0x7e_11)
}

fn scope() -> AccessScope {
    AccessScope::for_tenant(owner())
}

/// The shipped retention window, read off the config the gear boots with rather
/// than restated — `sqlite_idempotency`'s reason: a hard-coded number keeps
/// passing while asserting a bound the gear no longer has.
fn ttl_hours() -> i64 {
    i64::try_from(LimitsConfig::default().idempotency_key_ttl_hours)
        .expect("the shipped TTL must be a representable number of hours")
}

fn at(hours: i64) -> OffsetDateTime {
    utc_ymd_hms(2026, 8, 2, 10, 0, 0) + time::Duration::hours(hours)
}

fn gate() -> IdempotencyGate {
    IdempotencyGate::new(LimitsConfig::default().idempotency_key_ttl())
}

fn first_payload() -> Vec<u8> {
    IdempotencyGate::payload_hash("create_plan|gold|monthly")
}

fn other_payload() -> Vec<u8> {
    IdempotencyGate::payload_hash("create_plan|platinum|monthly")
}

/// One claim, in its own committed transaction — the gear's contract with the
/// guarded body empty.
async fn claim(pg: &Pg, hash: Vec<u8>, now: OffsetDateTime) -> Result<ClaimOutcome, RepoError> {
    let provider = DBProvider::<DbError>::new(pg.db().await);
    provider
        .transaction(move |txn| {
            Box::pin(async move {
                Ok(gate()
                    .claim(txn, &scope(), owner(), CREATE_PLAN, CLIENT_KEY, &hash, now)
                    .await)
            })
        })
        .await
        .expect("the transaction itself must commit")
}

/// Record what the guarded operation answered.
async fn answer(pg: &Pg, status: i32, body: serde_json::Value) -> Result<(), RepoError> {
    let provider = DBProvider::<DbError>::new(pg.db().await);
    provider
        .transaction(move |txn| {
            Box::pin(async move {
                Ok(IdempotencyGate::record_response(
                    txn,
                    &scope(),
                    owner(),
                    CREATE_PLAN,
                    CLIENT_KEY,
                    status,
                    body,
                )
                .await)
            })
        })
        .await
        .expect("the transaction itself must commit")
}

/// The stored row, read through the scope like every other reader.
async fn stored(pg: &Pg) -> Option<idempotency_dedup::Model> {
    let provider = DBProvider::<DbError>::new(pg.db().await);
    let conn = provider.conn().expect("scoped connection");
    idempotency_dedup::Entity::find()
        .secure()
        .scope_with(&scope())
        .filter(
            Condition::all()
                .add(idempotency_dedup::Column::TenantId.eq(owner()))
                .add(idempotency_dedup::Column::Operation.eq(CREATE_PLAN))
                .add(idempotency_dedup::Column::ClientKey.eq(CLIENT_KEY)),
        )
        .one(&conn)
        .await
        .expect("read the dedup row")
}

/// **The claim path, end to end, through the real column types.**
///
/// The replayed body is compared as a value rather than as a string, which is what
/// makes this a Postgres assertion and not a repeat of the mirror's: `jsonb` is
/// stored decomposed and re-rendered, so a body that survived `text` on `SQLite`
/// can come back with its members reordered here. The digest goes out and comes
/// back through `bytea`, and the instant through `timestamptz`.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_repeat_of_the_same_request_is_told_what_the_first_one_was_told() {
    let pg = Pg::applied().await;
    let body = json!({ "planId": "9f2a", "revision": 0, "tier": "gold" });

    assert_eq!(
        claim(&pg, first_payload(), at(0)).await.expect("claim"),
        ClaimOutcome::Claimed,
        "the first request holds the key"
    );
    answer(&pg, 201, body.clone())
        .await
        .expect("record what the guarded mutation answered");

    assert_eq!(
        claim(&pg, first_payload(), at(1)).await.expect("replay"),
        ClaimOutcome::Replay {
            status: 201,
            body: body.clone()
        },
        "the retry is answered from the store, verbatim, with no member reordered by jsonb"
    );

    let row = stored(&pg).await.expect("the claim is there");
    assert_eq!(
        row.request_hash,
        first_payload(),
        "the digest round-trips through bytea unchanged - 32 bytes, not a hex rendering"
    );
    assert_eq!(row.request_hash.len(), 32);
    assert_eq!(
        row.created_at_utc,
        at(0),
        "and the instant through timestamptz, at the offset it was written with"
    );
}

/// **The mismatch refusal, and the row is not touched.**
///
/// The comparison is `bytea` against `&[u8]` here where the mirror compares blobs,
/// and this is the one class `SQLite`'s affinity rules never convert — so a
/// comparison that worked on one engine is no evidence at all about the other.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_key_reused_for_a_different_request_is_refused_and_changes_nothing() {
    let pg = Pg::applied().await;
    let body = json!({ "planId": "9f2a", "revision": 0 });

    claim(&pg, first_payload(), at(0)).await.expect("claim");
    answer(&pg, 201, body.clone())
        .await
        .expect("record the response");

    let err = claim(&pg, other_payload(), at(1))
        .await
        .expect_err("the same key with a different payload must be refused");
    assert_eq!(
        err,
        RepoError::IdempotencyPayloadMismatch {
            operation: CREATE_PLAN.to_owned(),
            client_key: CLIENT_KEY.to_owned(),
        }
    );

    // Never re-executed is half of it; never overwritten is the half a refusal
    // that had already touched the row would have broken.
    let row = stored(&pg).await.expect("the claim is still there");
    assert_eq!(row.response_status, Some(201));
    assert_eq!(row.response_body, Some(body));
    assert_eq!(
        row.request_hash,
        first_payload(),
        "the first payload still owns it"
    );

    // The positive control: the *original* payload is still replayed, so the
    // refusal above is a fact about the digest and not about the key having been
    // poisoned by the attempt.
    assert!(
        matches!(
            claim(&pg, first_payload(), at(2)).await,
            Ok(ClaimOutcome::Replay { status: 201, .. })
        ),
        "a refused mismatch must leave the honest retry replayable"
    );
}

/// **Two duplicates in flight at once, which is the case `SQLite` cannot stage.**
///
/// This is what the `PRIMARY KEY` and the `INSERT ... ON CONFLICT DO NOTHING` were
/// chosen over a read-then-write for, and it had no test on any engine: the mirror
/// serializes writers, so its suite pins the in-flight refusal by calling `claim`
/// twice inside **one** transaction — a stand-in whose own doc says a race "would
/// prove nothing while looking like it proved everything".
///
/// Driven by observable events rather than by hope, in `postgres_approval_race`'s
/// idiom:
///
/// 1. the first duplicate claims and **parks**, its row uncommitted;
/// 2. the second duplicate's `INSERT ... ON CONFLICT DO NOTHING` meets that
///    uncommitted row and blocks on it — which is the arm `claim`'s doc calls "the
///    conflicting row being unreadable inside the transaction that just collided
///    with it", and the reason it is a `Db` arm at all;
/// 3. a third connection **observes the block**, which is what proves the second
///    insert really contended rather than arriving after the first committed;
/// 4. the first is released, and the second resolves.
///
/// Step 3 is load-bearing. Without it the second claim could run entirely after
/// the first commit and be answered `IdempotencyKeyInFlight` by the ordinary
/// uncontended path — green, and about nothing.
///
/// **Exactly one claims.** The loser is refused rather than told `Claimed`, which
/// is the whole of at-most-once: two `Claimed` answers would run the guarded
/// mutation twice under one key.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn two_duplicates_in_flight_leave_exactly_one_holding_the_key() {
    let pg = Pg::applied().await;
    let observer = pg.raw().await;
    let claimed = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());

    let first = {
        let provider = DBProvider::<DbError>::new(pg.db().await);
        let (claimed, release) = (Arc::clone(&claimed), Arc::clone(&release));
        tokio::spawn(async move {
            provider
                .transaction(move |txn| {
                    Box::pin(async move {
                        let outcome = gate()
                            .claim(
                                txn,
                                &scope(),
                                owner(),
                                CREATE_PLAN,
                                CLIENT_KEY,
                                &first_payload(),
                                at(0),
                            )
                            .await;
                        // The row is written and uncommitted: the key's primary
                        // key entry is held against every other inserter.
                        claimed.notify_one();
                        release.notified().await;
                        Ok(outcome)
                    })
                })
                .await
        })
    };

    claimed.notified().await;

    let second = {
        // The same database, a **separate** connection: the harness handle is
        // `{port, database}` and holds no pool, so a clone is a second client and
        // not a second server.
        let pg_second = pg.clone();
        tokio::spawn(async move { claim(&pg_second, first_payload(), at(0)).await })
    };

    // The second insert is waiting on the first transaction's uncommitted row.
    pg_support::wait_until_a_backend_blocks(&observer).await;
    release.notify_one();

    let winner = tokio::time::timeout(RACE_TIMEOUT, first)
        .await
        .expect("the first claim must finish once released")
        .expect("its task must not panic")
        .expect("the transaction itself must commit")
        .expect("the first claim is uncontended and must win the key");
    assert_eq!(
        winner,
        ClaimOutcome::Claimed,
        "the request that got there first holds the key"
    );

    let loser = tokio::time::timeout(RACE_TIMEOUT, second)
        .await
        .expect("the second claim must be released by the first's commit")
        .expect("its task must not panic");
    assert_eq!(
        loser,
        Err(RepoError::IdempotencyKeyInFlight {
            operation: CREATE_PLAN.to_owned(),
            client_key: CLIENT_KEY.to_owned(),
        }),
        "the loser is told the truth - the key is held by a claim nobody has answered - and \
         not `Claimed`, which would run the guarded mutation a second time under one key \
         (D-143)"
    );

    // One row, one holder: the conflict swallowed the second insert rather than
    // the second overwriting the first.
    let row = stored(&pg).await.expect("the claim exists");
    assert_eq!(row.created_at_utc, at(0));
    assert_eq!(row.response_status, None, "nobody has been told anything");
    assert_eq!(row.response_body, None);
}

/// **The expiry boundary, decided by a real `timestamptz` subtraction.**
///
/// Both sides in one case, because the boundary is what either arm could be wrong
/// about: exactly at the window the key still belongs to the first request, and one
/// hour past it the key is free — free even to a payload that would have been
/// refused a moment earlier, which is `claim`'s stated order (expiry before hash,
/// so one payload cannot poison its key forever in a gear with no reaper).
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn the_window_is_measured_against_the_stored_instant_and_not_a_rounded_one() {
    let pg = Pg::applied().await;
    let body = json!({ "planId": "9f2a" });

    claim(&pg, first_payload(), at(0)).await.expect("claim");
    answer(&pg, 201, body.clone())
        .await
        .expect("record the response");

    assert_eq!(
        claim(&pg, first_payload(), at(ttl_hours()))
            .await
            .expect("a key at its window is still held"),
        ClaimOutcome::Replay { status: 201, body },
        "exactly at the window the claim still stands: the comparison is strictly greater"
    );

    assert_eq!(
        claim(&pg, other_payload(), at(ttl_hours() + 1))
            .await
            .expect("past the window the key is free"),
        ClaimOutcome::Claimed,
        "and it is free even to a different payload, expiry being asked before the digest"
    );

    let row = stored(&pg).await.expect("the taken-over claim");
    assert_eq!(
        row.request_hash,
        other_payload(),
        "the takeover rewrites the digest to the arriving request's"
    );
    assert_eq!(
        row.response_status, None,
        "and drops the old answer with it, so the new claim is unanswered"
    );
    assert_eq!(row.response_body, None);
    assert_eq!(row.created_at_utc, at(ttl_hours() + 1));
}

/// **An answer is written once**, and the `jsonb` column is not a cell to update.
///
/// The refusal is `NotFound` because no *unanswered* claim is visible, which is the
/// predicate doing the work rather than a comparison in the repository.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn an_answer_is_written_once_and_a_second_one_is_refused() {
    let pg = Pg::applied().await;
    let first = json!({ "planId": "9f2a", "revision": 0 });

    claim(&pg, first_payload(), at(0)).await.expect("claim");
    answer(&pg, 201, first.clone())
        .await
        .expect("record what the guarded mutation answered");

    let err = answer(&pg, 500, json!({ "error": "later" }))
        .await
        .expect_err("an answered claim takes no second answer");
    assert_eq!(
        err,
        RepoError::NotFound {
            subject: "idempotency claim".to_owned(),
            id: format!("{CREATE_PLAN}/{CLIENT_KEY}"),
        },
        "no unanswered claim is visible, which is what the refusal means"
    );

    let row = stored(&pg).await.expect("the claim is still there");
    assert_eq!(row.response_status, Some(201));
    assert_eq!(row.response_body, Some(first.clone()));
    assert_eq!(
        claim(&pg, first_payload(), at(1)).await.expect("replay"),
        ClaimOutcome::Replay {
            status: 201,
            body: first
        },
        "the first answer stands, and so does the replay built on it"
    );
}
