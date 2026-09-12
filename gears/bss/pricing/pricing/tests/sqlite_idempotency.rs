//! The at-most-once gate against a real database.
//!
//! The gate is a `PRIMARY KEY` and an `INSERT ... ON CONFLICT DO NOTHING`, so
//! there is nothing here a mock could observe: every assertion below is about
//! what the *statement* did — which insert the key swallowed, which UPDATE
//! matched the row as it was read, which `CHECK` refused a half-recorded
//! answer. A test double would keep passing after the conflict clause had been
//! deleted.
//!
//! The suite drives the in-flight refusal directly rather than through
//! concurrency, on both paths that reach it. On `SQLite` a race cannot be staged
//! anyway — one writer, transactions serialized — so a racing test would prove
//! nothing while looking like it proved everything. What is worth pinning is
//! that an unanswered claim is *refused with its own code* rather than answered
//! with a response nobody was ever given. The takeover half of that, which needs
//! a caller holding a row the store has already moved past, is pinned beside the
//! module in `src/infra/storage/repo/idempotency_repo_tests.rs`, since no
//! sequence of public calls can produce a stale read.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use bss_pricing::config::LimitsConfig;
use bss_pricing::domain::instant::utc_ymd_hms;
use bss_pricing::infra::storage::entity::idempotency_dedup;
use bss_pricing::infra::storage::migrations::Migrator;
use bss_pricing::infra::storage::repo::{ClaimOutcome, IdempotencyGate};
use bss_pricing::infra::storage::{RepoError, repo_failure};
use sea_orm::ActiveValue::Set;
use sea_orm::{ColumnTrait, Condition, ConnectionTrait, DatabaseConnection, EntityTrait};
use sea_orm_migration::MigratorTrait;
use serde_json::json;
use time::OffsetDateTime;
use toolkit::api::canonical_prelude::CanonicalError;
use toolkit_db::migration_runner::run_migrations_for_testing;
use toolkit_db::secure::{AccessScope, SecureEntityExt, SecureInsertExt};
use toolkit_db::{ConnectOpts, DBProvider, DbError, connect_db};
use uuid::Uuid;

/// The retention window the gear actually ships with, taken from the config
/// defaults rather than restated as a literal. The suite measures every expiry
/// assertion against this, so a shipped window that moved would move the tests
/// with it; a hard-coded 24 would keep passing while asserting a bound the gear
/// no longer has.
fn ttl_hours() -> i64 {
    i64::try_from(LimitsConfig::default().idempotency_key_ttl_hours)
        .expect("the shipped TTL must be a representable number of hours")
}

/// The operation most of the suite claims under.
const CREATE_PLAN: &str = "create_plan";

/// The tenant that owns the claims, unless a test says otherwise.
fn owner() -> Uuid {
    Uuid::from_u128(0x7e_11)
}

/// One authoring request, as the gate sees it.
#[derive(Clone)]
struct Request {
    tenant: Uuid,
    operation: String,
    client_key: String,
    hash: Vec<u8>,
    now: OffsetDateTime,
}

impl Request {
    /// The suite's baseline request: the owning tenant, `create_plan`, one key,
    /// and a payload rendering digested the way the surface would digest it.
    fn new() -> Self {
        Self {
            tenant: owner(),
            operation: CREATE_PLAN.to_owned(),
            client_key: "ck-9f2a".to_owned(),
            hash: IdempotencyGate::payload_hash("create_plan|gold|monthly"),
            now: at(0),
        }
    }

    /// The same key carrying a different payload — the mismatch case.
    fn with_other_payload(&self) -> Self {
        Self {
            hash: IdempotencyGate::payload_hash("create_plan|platinum|monthly"),
            ..self.clone()
        }
    }

    /// The same request arriving `hours` later.
    fn at_hour(&self, hours: i64) -> Self {
        Self {
            now: at(hours),
            ..self.clone()
        }
    }
}

/// Hours after the suite's fixed origin instant.
fn at(hours: i64) -> OffsetDateTime {
    utc_ymd_hms(2026, 8, 2, 10, 0, 0) + time::Duration::hours(hours)
}

/// A migrated in-memory database and a gate carrying the shipped window.
///
/// The gate is built from `LimitsConfig`'s own accessor, not from a duration
/// spelled out here: the thing under test is the window the gear boots with, and
/// a suite that constructed its own would be testing a number nobody ships.
async fn harness() -> (DBProvider<DbError>, IdempotencyGate) {
    let (provider, gate, _) = harness_with_raw().await;
    (provider, gate)
}

/// The same, plus a connection **past** the secure layer.
///
/// One case has to break the schema the gate reads, and `DBProvider::conn` hands
/// back a scoped runner with no way to issue DDL — which is that runner doing its
/// job rather than a gap.
async fn harness_with_raw() -> (DBProvider<DbError>, IdempotencyGate, DatabaseConnection) {
    // A **named** in-memory database with a shared cache, so a second handle
    // reaches the same one. The gate's own `Db` seals its `SeaORM` connection off
    // deliberately (`sea_internal` is `pub(crate)` to toolkit-db), which is the
    // secure layer doing its job — so the only way to break the schema the gate
    // reads is to arrive by a second connection.
    let url = format!(
        "sqlite:file:idem-{}?mode=memory&cache=shared",
        Uuid::now_v7().simple()
    );
    let raw = sea_orm::Database::connect(&url)
        .await
        .expect("a second handle on the shared in-memory database");
    let db = connect_db(&url, ConnectOpts::default())
        .await
        .expect("connect in-memory sqlite");
    run_migrations_for_testing(&db, Migrator::migrations())
        .await
        .expect("run migrator");
    let gate = IdempotencyGate::new(LimitsConfig::default().idempotency_key_ttl());
    (DBProvider::<DbError>::new(db), gate, raw)
}

/// One claim, in its own committed transaction.
///
/// The gear's contract puts the claim in the same transaction as the mutation it
/// guards; a test that only claims has no mutation, so one transaction per claim
/// is that contract with the guarded body empty.
async fn claim(
    provider: &DBProvider<DbError>,
    gate: &IdempotencyGate,
    scope: &AccessScope,
    req: &Request,
) -> Result<ClaimOutcome, RepoError> {
    let gate = gate.clone();
    let scope = scope.clone();
    let req = req.clone();
    provider
        .transaction(move |txn| {
            Box::pin(async move {
                Ok(gate
                    .claim(
                        txn,
                        &scope,
                        req.tenant,
                        &req.operation,
                        &req.client_key,
                        &req.hash,
                        req.now,
                    )
                    .await)
            })
        })
        .await
        .expect("the transaction itself must commit")
}

/// Two claims for the same request inside **one** transaction — the second one
/// meets a claim that has not answered yet.
async fn claim_twice(
    provider: &DBProvider<DbError>,
    gate: &IdempotencyGate,
    scope: &AccessScope,
    req: &Request,
) -> Result<ClaimOutcome, RepoError> {
    let gate = gate.clone();
    let scope = scope.clone();
    let req = req.clone();
    provider
        .transaction(move |txn| {
            Box::pin(async move {
                let first = gate
                    .claim(
                        txn,
                        &scope,
                        req.tenant,
                        &req.operation,
                        &req.client_key,
                        &req.hash,
                        req.now,
                    )
                    .await;
                assert_eq!(
                    first.as_ref().ok(),
                    Some(&ClaimOutcome::Claimed),
                    "the first claim must win the key"
                );
                Ok(gate
                    .claim(
                        txn,
                        &scope,
                        req.tenant,
                        &req.operation,
                        &req.client_key,
                        &req.hash,
                        req.now,
                    )
                    .await)
            })
        })
        .await
        .expect("the transaction itself must commit")
}

/// Record what the guarded operation answered, in its own committed transaction.
async fn answer(
    provider: &DBProvider<DbError>,
    scope: &AccessScope,
    req: &Request,
    status: i32,
    body: serde_json::Value,
) -> Result<(), RepoError> {
    let scope = scope.clone();
    let req = req.clone();
    provider
        .transaction(move |txn| {
            Box::pin(async move {
                Ok(IdempotencyGate::record_response(
                    txn,
                    &scope,
                    req.tenant,
                    &req.operation,
                    &req.client_key,
                    status,
                    body,
                )
                .await)
            })
        })
        .await
        .expect("the transaction itself must commit")
}

/// The stored row as `scope` can see it — `None` when it cannot see it at all.
async fn stored(
    provider: &DBProvider<DbError>,
    scope: &AccessScope,
    req: &Request,
) -> Option<idempotency_dedup::Model> {
    let conn = provider.conn().expect("scoped connection");
    idempotency_dedup::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(idempotency_dedup::Column::TenantId.eq(req.tenant))
                .add(idempotency_dedup::Column::Operation.eq(req.operation.clone()))
                .add(idempotency_dedup::Column::ClientKey.eq(req.client_key.clone())),
        )
        .one(&conn)
        .await
        .expect("read the dedup row")
}

/// What [`IdempotencyGate::recorded_response`] answers, in its own transaction.
///
/// The one caller is `customer_groups`' membership move, which asks before
/// deciding which act it is performing — so `Ok(None)` here means "go ahead and
/// perform it".
async fn recorded(
    provider: &DBProvider<DbError>,
    gate: &IdempotencyGate,
    scope: &AccessScope,
    req: &Request,
) -> Result<Option<(i32, serde_json::Value)>, RepoError> {
    let gate = gate.clone();
    let scope = scope.clone();
    let req = req.clone();
    provider
        .transaction(move |txn| {
            Box::pin(async move {
                Ok(gate
                    .recorded_response(
                        txn,
                        &scope,
                        req.tenant,
                        &req.operation,
                        &req.client_key,
                        &req.hash,
                        req.now,
                    )
                    .await)
            })
        })
        .await
        .expect("the transaction itself must commit")
}

/// Make the claim table unreadable, so the gate's read **faults** rather than
/// finding nothing.
///
/// A dropped table is the bluntest fault available and the right one here: the
/// distinction under test is between "the store could not answer" and "the store
/// answered that there is no row", and any fault at all separates them. A
/// narrower injection — a renamed column, a revoked grant — would prove the same
/// thing about a smaller class and needs more schema to arrange.
async fn make_the_claim_unreadable(raw: &DatabaseConnection) {
    raw.execute_unprepared("DROP TABLE pricing_idempotency_dedup")
        .await
        .expect("drop the claim table");
}

/// A read that could not run is not an answer that there is nothing recorded.
///
/// `recorded_response` discarded every error from its read and returned
/// `Ok(None)`. Its one caller exists to stop a wall-clock fork from outrunning the
/// guard — `move_membership` takes an immediate arm for `effective_from <= now()`
/// and opens no claim on it — so "nothing recorded" is the answer that says *go
/// ahead and perform the act*. A transient storage fault therefore produced the
/// duplicate membership move the guard was added to prevent.
#[tokio::test]
async fn a_faulting_read_is_not_answered_as_nothing_recorded() {
    let (provider, gate, raw) = harness_with_raw().await;
    let scope = AccessScope::for_tenant(owner());
    let req = Request::new();
    let body = json!({ "membershipId": "9f2a" });

    claim(&provider, &gate, &scope, &req).await.expect("claim");
    answer(&provider, &scope, &req, 202, body.clone())
        .await
        .expect("record what the guarded mutation answered");

    // The control: with the store answering, the recorded response is found. A
    // fault case alone would pass against a gate that answered `Err` to
    // everything.
    assert_eq!(
        recorded(&provider, &gate, &scope, &req)
            .await
            .expect("the store can answer"),
        Some((202, body))
    );

    make_the_claim_unreadable(&raw).await;

    let err = recorded(&provider, &gate, &scope, &req)
        .await
        .expect_err("a read that could not run must not answer `no recorded response`");
    assert!(matches!(err, RepoError::Db(_)), "unexpected: {err:?}");
}

/// And the absent row still answers `None`, which is the whole point of keeping
/// the two apart.
///
/// The other half of the same fix: had the fault been separated by making every
/// outcome an `Err`, the caller would refuse every first-time request — the guard
/// would fail closed on the ordinary path instead of failing open on the rare one.
#[tokio::test]
async fn a_key_no_one_ever_claimed_has_no_recorded_response() {
    let (provider, gate) = harness().await;
    let scope = AccessScope::for_tenant(owner());

    assert_eq!(
        recorded(&provider, &gate, &scope, &Request::new())
            .await
            .expect("an unclaimed key is not a fault"),
        None
    );
}

/// A claim that has not answered yet is `None` too — in flight, not recorded.
#[tokio::test]
async fn a_claim_still_in_flight_has_no_recorded_response() {
    let (provider, gate) = harness().await;
    let scope = AccessScope::for_tenant(owner());
    let req = Request::new();

    claim(&provider, &gate, &scope, &req).await.expect("claim");

    assert_eq!(
        recorded(&provider, &gate, &scope, &req)
            .await
            .expect("a claim in flight is not a fault"),
        None
    );
}

#[tokio::test]
async fn a_repeat_of_the_same_request_is_told_what_the_first_one_was_told() {
    let (provider, gate) = harness().await;
    let scope = AccessScope::for_tenant(owner());
    let req = Request::new();
    let body = json!({ "planId": "9f2a", "revision": 0 });

    assert_eq!(
        claim(&provider, &gate, &scope, &req).await.expect("claim"),
        ClaimOutcome::Claimed,
        "the first request holds the key"
    );
    answer(&provider, &scope, &req, 201, body.clone())
        .await
        .expect("record what the guarded mutation answered");

    // The retry is answered from the store, not by running the mutation again.
    // Status and body come back exactly as they were given: a replay that
    // re-rendered the body would tell the caller something the first request
    // never was told.
    assert_eq!(
        claim(&provider, &gate, &scope, &req.at_hour(1))
            .await
            .expect("replay"),
        ClaimOutcome::Replay { status: 201, body }
    );
}

#[tokio::test]
async fn a_key_reused_for_a_different_request_is_refused_and_changes_nothing() {
    let (provider, gate) = harness().await;
    let scope = AccessScope::for_tenant(owner());
    let req = Request::new();
    let body = json!({ "planId": "9f2a", "revision": 0 });

    claim(&provider, &gate, &scope, &req).await.expect("claim");
    answer(&provider, &scope, &req, 201, body.clone())
        .await
        .expect("record the response");

    // Neither answer would be right: replaying tells the caller about a plan it
    // did not ask for, and re-executing breaks the promise the key is for.
    let err = claim(&provider, &gate, &scope, &req.with_other_payload())
        .await
        .expect_err("the same key with a different payload must be refused");
    assert_eq!(
        err,
        RepoError::IdempotencyPayloadMismatch {
            operation: CREATE_PLAN.to_owned(),
            client_key: "ck-9f2a".to_owned(),
        }
    );

    // "Never re-executed" is only half of it; "never overwritten" is the half a
    // refusal that had already touched the row would have broken.
    let row = stored(&provider, &scope, &req)
        .await
        .expect("the claim is still there");
    assert_eq!(row.response_status, Some(201));
    assert_eq!(row.response_body, Some(body));
    assert_eq!(
        row.request_hash, req.hash,
        "the first payload still owns it"
    );
    assert_eq!(row.created_at_utc, at(0));
}

#[tokio::test]
async fn a_duplicate_meeting_an_unanswered_claim_is_refused_not_answered() {
    let (provider, gate) = harness().await;
    let scope = AccessScope::for_tenant(owner());
    let req = Request::new();

    // Reached here only because both claims run in one transaction with no
    // mutation between them. The point is that a caller in that position is told
    // the truth instead of being handed a response that does not exist yet — and
    // since D-143 the truth has a code, so the refusal names the key the caller
    // should retry rather than leaving the surface to invent an answer.
    let err = claim_twice(&provider, &gate, &scope, &req)
        .await
        .expect_err("a duplicate meeting a live claim is refused");
    assert_eq!(
        err,
        RepoError::IdempotencyKeyInFlight {
            operation: req.operation.clone(),
            client_key: req.client_key.clone(),
        }
    );

    // And the wire code a client branches on is the one the design set names,
    // 409 rather than the 400 bucket: retrying is the remedy.
    let mapped = CanonicalError::from(repo_failure(&err));
    assert_eq!(mapped.status_code(), 409);
    assert!(format!("{mapped:?}").contains("IDEMPOTENCY_KEY_IN_FLIGHT"));

    let row = stored(&provider, &scope, &req)
        .await
        .expect("the claim exists");
    assert_eq!(row.response_status, None, "nobody has been told anything");
    assert_eq!(row.response_body, None);
}

#[tokio::test]
async fn a_key_past_its_window_is_taken_over_and_the_old_answer_goes_with_it() {
    let (provider, gate) = harness().await;
    let scope = AccessScope::for_tenant(owner());
    let req = Request::new();

    claim(&provider, &gate, &scope, &req).await.expect("claim");
    answer(&provider, &scope, &req, 201, json!({ "planId": "9f2a" }))
        .await
        .expect("record the response");

    // Past the window the key is forgotten, so the request that carries it is
    // simply a new one. No reaper has run; expiry is decided here, at claim
    // time, which is what makes the configured 24h an actual bound rather than
    // a bound plus however long a sweeper happened to be down.
    let later = req.at_hour(ttl_hours() + 1);
    assert_eq!(
        claim(&provider, &gate, &scope, &later)
            .await
            .expect("an expired key claims afresh"),
        ClaimOutcome::Claimed
    );

    // And it claims afresh in full: carrying the stale response forward would
    // let the next replay answer this request with the previous one's outcome.
    let row = stored(&provider, &scope, &req)
        .await
        .expect("the row was taken over, not deleted");
    assert_eq!(row.response_status, None);
    assert_eq!(row.response_body, None);
    assert_eq!(row.created_at_utc, at(ttl_hours() + 1));
}

#[tokio::test]
async fn an_expired_key_is_free_even_to_a_different_payload() {
    let (provider, gate) = harness().await;
    let scope = AccessScope::for_tenant(owner());
    let req = Request::new();

    claim(&provider, &gate, &scope, &req).await.expect("claim");
    answer(&provider, &scope, &req, 201, json!({ "planId": "9f2a" }))
        .await
        .expect("record the response");

    // Expiry is asked before the digest is compared, and that order is the
    // difference between a key that can be reused and one poisoned for good:
    // nothing in this gear deletes a dedup row, so a mismatch raised against an
    // expired claim would be raised again for every later request carrying that
    // key, forever. "Forgotten" has to mean forgotten.
    let later = req.with_other_payload().at_hour(ttl_hours() + 1);
    assert_eq!(
        claim(&provider, &gate, &scope, &later)
            .await
            .expect("an expired key contradicts nothing"),
        ClaimOutcome::Claimed
    );

    let row = stored(&provider, &scope, &req)
        .await
        .expect("the row was taken over");
    assert_eq!(row.request_hash, later.hash, "the new payload owns it now");
    assert_eq!(row.response_status, None);
}

#[tokio::test]
async fn a_key_inside_its_window_is_not_taken_over() {
    let (provider, gate) = harness().await;
    let scope = AccessScope::for_tenant(owner());
    let req = Request::new();
    let body = json!({ "planId": "9f2a" });

    claim(&provider, &gate, &scope, &req).await.expect("claim");
    answer(&provider, &scope, &req, 201, body.clone())
        .await
        .expect("record the response");

    // One hour short of the window. An off-by-one in the expiry comparison
    // would show up here as a second execution of a mutation the caller
    // believed had happened once.
    assert_eq!(
        claim(&provider, &gate, &scope, &req.at_hour(ttl_hours() - 1))
            .await
            .expect("still inside the window"),
        ClaimOutcome::Replay { status: 201, body }
    );

    let row = stored(&provider, &scope, &req)
        .await
        .expect("the claim is still there");
    assert_eq!(
        row.created_at_utc,
        at(0),
        "a replay must not restart the window, or a busy key would never expire"
    );
}

#[tokio::test]
async fn a_key_exactly_at_its_window_still_belongs_to_the_first_request() {
    let (provider, gate) = harness().await;
    let scope = AccessScope::for_tenant(owner());
    let req = Request::new();
    let body = json!({ "planId": "9f2a" });

    claim(&provider, &gate, &scope, &req).await.expect("claim");
    answer(&provider, &scope, &req, 201, body.clone())
        .await
        .expect("record the response");

    // The boundary itself, which `TTL-1` and `TTL+1` leave undefined. The
    // comparison is strict, so a claim landing exactly on the window is still
    // inside it: a 24h retention that expired *at* 24h would mean the last
    // instant of the documented window is the one where a retry silently
    // executes a second time. Whichever way this is decided it must be decided
    // by a test, not by whichever of `>` and `>=` was typed.
    assert_eq!(
        claim(&provider, &gate, &scope, &req.at_hour(ttl_hours()))
            .await
            .expect("the boundary instant is inside the window"),
        ClaimOutcome::Replay { status: 201, body }
    );

    let row = stored(&provider, &scope, &req)
        .await
        .expect("the claim is still there");
    assert_eq!(row.created_at_utc, at(0), "nothing was taken over");
}

#[tokio::test]
async fn an_answer_is_written_once_and_a_second_one_is_refused() {
    let (provider, gate) = harness().await;
    let scope = AccessScope::for_tenant(owner());
    let req = Request::new();
    let first = json!({ "planId": "9f2a", "revision": 0 });

    claim(&provider, &gate, &scope, &req).await.expect("claim");
    answer(&provider, &scope, &req, 201, first.clone())
        .await
        .expect("record what the guarded mutation answered");

    // The stored response is what the original caller was actually told, so it
    // is not a cell to be updated. A handler that answered twice, or a caller
    // recording after being told `Replay`, would otherwise leave every later
    // replay serving a response that contradicts the one that was sent.
    let err = answer(&provider, &scope, &req, 500, json!({ "error": "later" }))
        .await
        .expect_err("an answered claim takes no second answer");
    assert_eq!(
        err,
        RepoError::NotFound {
            subject: "idempotency claim".to_owned(),
            id: format!("{CREATE_PLAN}/ck-9f2a"),
        },
        "no unanswered claim is visible, which is what the refusal means"
    );

    // The first answer stands, and so does the replay built on it.
    let row = stored(&provider, &scope, &req)
        .await
        .expect("the claim is still there");
    assert_eq!(row.response_status, Some(201));
    assert_eq!(row.response_body, Some(first.clone()));
    assert_eq!(
        claim(&provider, &gate, &scope, &req.at_hour(1))
            .await
            .expect("replay"),
        ClaimOutcome::Replay {
            status: 201,
            body: first
        }
    );
}

#[tokio::test]
async fn one_key_under_another_operation_or_another_tenant_is_another_claim() {
    let (provider, gate) = harness().await;
    let scope = AccessScope::for_tenant(owner());
    let req = Request::new();

    claim(&provider, &gate, &scope, &req).await.expect("claim");

    // The key is the whole triple. A gate keyed on the client key alone would
    // let one tenant's retry suppress another tenant's first attempt, and would
    // make a client that reuses one key across operations able to perform only
    // the first of them.
    let other_operation = Request {
        operation: "create_price".to_owned(),
        ..req.clone()
    };
    assert_eq!(
        claim(&provider, &gate, &scope, &other_operation)
            .await
            .expect("another operation is another claim"),
        ClaimOutcome::Claimed
    );

    let neighbour = Uuid::from_u128(0x7e_22);
    let other_tenant = Request {
        tenant: neighbour,
        ..req.clone()
    };
    assert_eq!(
        claim(
            &provider,
            &gate,
            &AccessScope::for_tenant(neighbour),
            &other_tenant
        )
        .await
        .expect("another tenant is another claim"),
        ClaimOutcome::Claimed
    );
}

#[tokio::test]
async fn another_tenants_claim_is_invisible_and_unanswerable() {
    let (provider, gate) = harness().await;
    let theirs = AccessScope::for_tenant(owner());
    let req = Request::new();
    let body = json!({ "planId": "9f2a" });

    claim(&provider, &gate, &theirs, &req).await.expect("claim");
    answer(&provider, &theirs, &req, 201, body.clone())
        .await
        .expect("record the response");

    // SQL-level BOLA, the shape the whole gear uses: my scope resolves their row
    // to nothing. The stored response is a commercially meaningful document, so
    // the one thing that must never happen is a foreign scope being handed it.
    let intruder = AccessScope::for_tenant(Uuid::from_u128(0x7e_33));
    assert_eq!(stored(&provider, &intruder, &req).await, None);

    // Claiming their key under my scope is refused by the scope validation
    // before any statement runs. What matters is the negative: whatever the
    // refusal is, it is never their stored response handed back to me.
    let outcome = claim(&provider, &gate, &intruder, &req).await;
    assert!(
        !matches!(outcome, Ok(ClaimOutcome::Replay { .. })),
        "a foreign scope must never be replayed another tenant's response"
    );
    assert!(
        matches!(outcome, Err(RepoError::Db(_))),
        "a foreign claim is refused by the scope, never executed"
    );

    let err = answer(&provider, &intruder, &req, 500, json!({ "error": "nope" }))
        .await
        .expect_err("a foreign scope holds no claim to answer");
    assert!(matches!(err, RepoError::NotFound { .. }));

    // And the owner's row is exactly as it was.
    let row = stored(&provider, &theirs, &req)
        .await
        .expect("their claim is still there");
    assert_eq!(row.response_status, Some(201));
    assert_eq!(row.response_body, Some(body));
}

#[tokio::test]
async fn half_a_response_is_not_a_storable_answer_in_either_direction() {
    let (provider, _gate) = harness().await;
    let scope = AccessScope::for_tenant(owner());
    let conn = provider.conn().expect("scoped connection");

    // The response columns are nullable so a claim can exist before its answer
    // does. The pairing CHECK is what keeps that from also permitting half an
    // answer, which no reader could interpret: a status with nothing to send,
    // or a body nobody assigned a status to.
    let am = idempotency_dedup::ActiveModel {
        tenant_id: Set(owner()),
        operation: Set(CREATE_PLAN.to_owned()),
        client_key: Set("ck-half".to_owned()),
        request_hash: Set(IdempotencyGate::payload_hash("half")),
        response_status: Set(Some(201)),
        response_body: Set(None),
        created_at_utc: Set(at(0)),
    };
    let refused = idempotency_dedup::Entity::insert(am.clone())
        .secure()
        .scope_with_model(&scope, &am)
        .expect("the scope permits the write")
        .exec(&conn)
        .await;

    // Named, not merely "an error": the status-range CHECK and the primary key
    // guard the same statement, and a test satisfied by any refusal would keep
    // passing after the pairing constraint had been dropped.
    let Err(refusal) = refused else {
        panic!("half a response must be refused");
    };
    let refusal = refusal.to_string();
    assert!(
        refusal.contains("chk_pricing_idempotency_dedup_answered"),
        "the pairing CHECK must be the one that refused; got: {refusal}"
    );

    // And the other direction the comment above names. A pairing constraint has
    // two halves and one of them can be dropped on its own, so a case that drove
    // only the first would report the CHECK intact while a body nobody assigned a
    // status to had become storable — and a replay would then answer it under
    // whatever status the reader defaulted to.
    let am = idempotency_dedup::ActiveModel {
        tenant_id: Set(owner()),
        operation: Set(CREATE_PLAN.to_owned()),
        client_key: Set("ck-other-half".to_owned()),
        request_hash: Set(IdempotencyGate::payload_hash("other-half")),
        response_status: Set(None),
        response_body: Set(Some(serde_json::json!({ "plan_id": "whatever" }))),
        created_at_utc: Set(at(0)),
    };
    let refused = idempotency_dedup::Entity::insert(am.clone())
        .secure()
        .scope_with_model(&scope, &am)
        .expect("the scope permits the write")
        .exec(&conn)
        .await;

    let Err(refusal) = refused else {
        panic!("a body nobody assigned a status to must be refused");
    };
    let refusal = refusal.to_string();
    assert!(
        refusal.contains("chk_pricing_idempotency_dedup_answered"),
        "the pairing CHECK must be the one that refused; got: {refusal}"
    );
}
