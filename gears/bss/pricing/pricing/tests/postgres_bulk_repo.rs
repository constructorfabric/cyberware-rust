//! `bulk_repo`'s own surface **on the engine that runs in production**
//! (`design/12-operator-efficiency.md` §4, §6; D-260, D-262, D-267, D-290,
//! D-294).
//!
//! `sqlite_bulk_repo` drives the same repository against the mirror, and until
//! this file that was the *only* place this code ran. The three bulk tables each
//! carry a schema suite on both engines, but the code over them was exercised on
//! `SQLite` alone — so every Postgres-only fact about it was unproven: the entity
//! mapping onto `jsonb`/`timestamptz`/`uuid` columns rather than the mirror's
//! `text`, the constraint names a refusal carries, and the statement-abort
//! semantics the next paragraph is entirely about.
//!
//! # The one rule `SQLite` structurally cannot test
//!
//! [`bulk_repo::take_locks`]'s doc requires an **autocommit connection, not a
//! transaction**, and gives the reason: Postgres aborts an enclosing transaction
//! on a failed statement, so the work that happens *after* the colliding insert —
//! the read that names the holder, and the partial release behind it — would fail
//! inside one, degrading the refusal from `RepoError::BulkRowLocked` to a bare
//! `RepoError::Db` and losing exactly the run `fr-concurrent-edit` requires it to
//! name. `SQLite` does not abort on a failed statement, so its cases pass whether
//! or not a caller honours the contract, and a caller that stopped honouring it
//! would keep every `SQLite` suite green while production reported a driver error
//! where an operator needs a run id.
//!
//! [`take_locks_inside_a_transaction_loses_the_holder`] is that case. It is a
//! characterisation rather than a wish: it pins what the shipping engine actually
//! does when the contract is broken, which is what makes the contract a rule
//! instead of a comment.
//!
//! Running it also **pins which statement dies**. `lock_holder` runs in front of
//! `release_locks`, and the other order costs the function its idempotence for its
//! own run: releasing first deletes every lock row of *this* `operation_id`, so a
//! run colliding with a lock it already holds finds nothing to read and answers
//! `RepoError::Db` having just unlocked the rows it still needed. Inside
//! an aborted transaction the read is therefore the first casualty and the
//! release never happens at all, and the case asserts the **read** by name so a
//! reordering cannot pass unnoticed.
//!
//! Run with:
//! `cargo test -p cf-gears-bss-pricing --test postgres_bulk_repo -- --ignored`.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod pg_support;

use std::sync::{Arc, Mutex};

use bss_pricing::domain::bulk::{BulkKind, BulkState};
use bss_pricing::domain::instant::utc_ymd_hms;
use bss_pricing::domain::money::{CurrencyCode, MinorAmount};
use bss_pricing::domain::price_record::PriceContent;
use bss_pricing::domain::price_row::{ModelKind, PriceRow};
use bss_pricing::domain::scope_key::{
    ChargeKind, Cohort, PhaseId, PlanId, PriceEligibility, Region, ScopeKey,
};
use bss_pricing::infra::storage::RepoError;
use bss_pricing::infra::storage::repo::{
    IdempotencyGate, NewBulkOperation, NewPriceDraft, PriceRepo, bulk_repo,
};
use time::OffsetDateTime;

use pg_support::Pg;
use toolkit_db::secure::{AccessScope, DbConn};
use toolkit_db::{DBProvider, DbError};
use uuid::Uuid;

const TENANT: Uuid = Uuid::from_u128(0x7e_41);
const ACTOR: Uuid = Uuid::from_u128(0xac_40);

fn at(hour: u32) -> OffsetDateTime {
    utc_ymd_hms(2026, 8, 9, hour, 0, 0)
}

fn scope() -> AccessScope {
    AccessScope::for_tenant(TENANT)
}

/// The run's own provider and a **second** one for the price repository.
///
/// Two pools on purpose. The harness caps a pool at two connections, and the
/// paths under test hold a connection for the run's statements while the price
/// repository opens a transaction of its own for each row it writes. One pool
/// would make the suite's own connection budget part of what it is testing,
/// which `pg_support`'s doc is emphatic about avoiding: a harness whose failures
/// are indistinguishable from the failures it exists to prove is worse than a
/// slow one.
struct Store {
    db: DBProvider<DbError>,
    prices: PriceRepo,
}

async fn store() -> Store {
    let pg = Pg::applied().await;
    let db = DBProvider::<DbError>::new(pg.db().await);
    let prices = PriceRepo::new(DBProvider::<DbError>::new(pg.db().await));
    Store { db, prices }
}

fn new_run(kind: BulkKind, client_key: &str) -> NewBulkOperation {
    NewBulkOperation {
        operation_id: Uuid::now_v7(),
        tenant_id: TENANT,
        kind,
        client_key: client_key.to_owned(),
        request_hash: IdempotencyGate::payload_hash(client_key),
        report: serde_json::json!({ "rows": [] }),
        submitted_by: ACTOR,
        submitted_at: at(10),
    }
}

/// Assert that a refusal is **the one under test**, by the fact Postgres names.
///
/// Not `is_err()`: this engine puts the constraint or the trigger's own sentence
/// in the message, so a case here can be sharper than its mirror, where the same
/// refusal is frequently a bare `UNIQUE constraint failed`. A case asserting only
/// that something went wrong would pass with the guard it means to prove
/// switched off.
fn assert_refused_by(err: &RepoError, by: &str) {
    let message = err.to_string();
    assert!(
        message.contains(by),
        "the refusal must be the one under test (`{by}`), got: {message}"
    );
}

/// Author a real draft price row and answer its id.
///
/// **The lock's `price_id` is a foreign key**, so a lock cannot be taken on a row
/// that does not exist — which is the store saying `inst-bk-lock` is about *rows
/// an edit could target*, not about a set of ids a run declares.
async fn a_price_row(store: &Store, region: &str) -> Uuid {
    let price_id = Uuid::now_v7();
    let mut row = PriceRow::new(ChargeKind::Recurring, Some(ModelKind::Flat));
    row.amount_minor = Some(MinorAmount::new(9_900).expect("a non-negative amount"));
    store
        .prices
        .create_draft(
            &scope(),
            TENANT,
            NewPriceDraft {
                price_id,
                scope_key: ScopeKey::new(
                    PlanId::new(Uuid::from_u128(0x50_c5)),
                    CurrencyCode::new("EUR").expect("three letters"),
                    Region::new(region).expect("a non-blank region"),
                    PhaseId::new(Uuid::from_u128(0xfa_80)),
                    PriceEligibility::AllSubscriptions,
                    ChargeKind::Recurring,
                    Cohort::None,
                )
                .expect("the class pairs with the cohort"),
                content: PriceContent {
                    row,
                    tax_inclusive: false,
                    tax_category_ref: None,
                    billing_timing: Some("advance".to_owned()),
                    proration_contract: None,
                    rounding_policy_ref: Some("half_up".to_owned()),
                    grandfather_until: None,
                    supersedes_price_id: None,
                },
                created_by: ACTOR,
                created_at_utc: at(10),
                correlation_id: Uuid::from_u128(0xc0_41),
            },
        )
        .await
        .expect("author the row");
    price_id
}

/// Open a run and move it to `committing`, the only state its locks may be taken
/// in — the lock table's own trigger says so, and it is right: a lock belongs to
/// a run that is actually committing, not to one still validating.
async fn committing_run(conn: &DbConn<'_>, client_key: &str) -> Uuid {
    let run = bulk_repo::open(conn, &scope(), new_run(BulkKind::Import, client_key))
        .await
        .expect("open");
    bulk_repo::advance(
        conn,
        &scope(),
        TENANT,
        run.operation_id,
        BulkState::Validating,
        BulkState::Committing,
        serde_json::json!({}),
        at(11),
    )
    .await
    .expect("enter committing");
    run.operation_id
}

/// A run is born `validating`, and **the record round-trips through Postgres's
/// own column types**.
///
/// The mirror stores `report` as `text` and `submitted_at` as `text`; here they
/// are `jsonb` and `timestamptz`, and the entity mapping between them is code
/// that had never run on this engine. A `jsonb` round trip that dropped or
/// reordered the report, or an instant that came back shifted, would be invisible
/// to every `SQLite` suite and to every schema suite alike -- the latter drive
/// raw SQL and never touch the entity.
#[tokio::test]
#[ignore = "requires Docker"]
async fn a_run_is_born_validating_and_reads_back_through_the_entity() {
    let store = store().await;
    let conn = store.db.conn().expect("conn");
    let run = bulk_repo::open(&conn, &scope(), new_run(BulkKind::Import, "k-1"))
        .await
        .expect("open the run");

    assert_eq!(run.state, BulkState::Validating);
    assert_eq!(run.kind, BulkKind::Import);
    assert!(run.completed_at.is_none(), "a run at birth is not over");
    assert_eq!(
        run.submitted_at,
        at(10),
        "the instant survives timestamptz unchanged"
    );
    assert_eq!(
        run.report,
        serde_json::json!({ "rows": [] }),
        "and the report survives jsonb"
    );
    assert_eq!(
        bulk_repo::read(&conn, &scope(), TENANT, run.operation_id)
            .await
            .expect("read it back")
            .expect("it exists"),
        run,
        "what `open` answers is what the store holds"
    );
}

/// O4's client key is unique per tenant, and the loser is told to retry against
/// the aggregate rather than handed a storage fault (D-159, Z2-2).
///
/// **This case asserted the index *name* until 2026-08-18** (review Z26-2), and
/// that assertion was pinning a defect. `open` mapped the insert failure to a bare
/// [`RepoError::Db`] carrying the driver's text, which is where the name came
/// from; the loser of a legitimate two-submit race therefore got a 500 for a
/// request whose contract promises either the winner's `202` or a refusal it can
/// act on. `contention_or_db` now classifies it, and the class is deliberately the
/// **same** for both unique indexes on this table — the client key and the
/// caller-minted `operation_id` — because both remedy the same way. So the name is
/// no longer observable at this boundary, by design.
///
/// What the boundary owes the caller is the class and the aggregate to retry
/// against, and that is what this asserts — as the whole error rather than as a
/// message substring, so a future rewording cannot quietly widen it.
///
/// **The index's identity is pinned where it is observable**, which is the residue
/// `storage::contention_or_db`'s doc recorded as owed until a Postgres suite
/// existed: by name in `postgres_migrations`' `EXPECTED_INDEXES` (and its `SQLite`
/// mirror), and by column set in that suite's
/// `the_client_key_index_spans_the_kind_as_well_as_the_tenant`. Two descriptions of
/// one index, neither of them a refusal message.
#[tokio::test]
#[ignore = "requires Docker"]
async fn one_client_key_opens_one_run() {
    let store = store().await;
    let conn = store.db.conn().expect("conn");
    bulk_repo::open(&conn, &scope(), new_run(BulkKind::Import, "k-2"))
        .await
        .expect("the first");

    let second = bulk_repo::open(&conn, &scope(), new_run(BulkKind::Import, "k-2"))
        .await
        .expect_err("the second is refused by the unique key");
    assert_eq!(
        second,
        RepoError::ConcurrentMutation {
            aggregate: "bulk operation under import client key k-2".to_owned(),
        },
        "a lost race is a contention naming what to retry, not a storage fault"
    );

    let found = bulk_repo::find_by_client_key(&conn, &scope(), TENANT, BulkKind::Import, "k-2")
        .await
        .expect("read by key")
        .expect("the first run");
    assert_eq!(found.client_key, "k-2");
}

/// The import happy path, walked through the repository rather than through raw
/// SQL: `validating -> committing -> completed`, with the report rewritten at
/// each step and the end instant stamped exactly on the terminal move.
#[tokio::test]
#[ignore = "requires Docker"]
async fn an_imports_happy_path_is_validating_then_committing_then_completed() {
    let store = store().await;
    let conn = store.db.conn().expect("conn");
    let run = bulk_repo::open(&conn, &scope(), new_run(BulkKind::Import, "k-3"))
        .await
        .expect("open");

    let committing = bulk_repo::advance(
        &conn,
        &scope(),
        TENANT,
        run.operation_id,
        BulkState::Validating,
        BulkState::Committing,
        serde_json::json!({ "phase": 2 }),
        at(11),
    )
    .await
    .expect("validating -> committing");
    assert_eq!(committing.state, BulkState::Committing);
    assert!(
        committing.completed_at.is_none(),
        "a run mid-commit is not over, and the CHECK agrees"
    );

    let done = bulk_repo::advance(
        &conn,
        &scope(),
        TENANT,
        run.operation_id,
        BulkState::Committing,
        BulkState::Completed,
        serde_json::json!({ "committed": 3 }),
        at(12),
    )
    .await
    .expect("committing -> completed");
    assert_eq!(done.state, BulkState::Completed);
    assert_eq!(
        done.completed_at,
        Some(at(12)),
        "a terminal state is stamped"
    );
    assert_eq!(done.report, serde_json::json!({ "committed": 3 }));
}

/// The repository names a state and writes it; the **trigger** decides whether
/// the move is an edge. So this asserts the trigger through the repository, and
/// on this engine it can assert the trigger's own sentence.
#[tokio::test]
#[ignore = "requires Docker"]
async fn a_move_that_is_not_an_edge_is_refused_by_the_store() {
    let store = store().await;
    let conn = store.db.conn().expect("conn");
    let run = bulk_repo::open(&conn, &scope(), new_run(BulkKind::Import, "k-4"))
        .await
        .expect("open");

    let skipped = bulk_repo::advance(
        &conn,
        &scope(),
        TENANT,
        run.operation_id,
        BulkState::Validating,
        BulkState::Completed,
        serde_json::json!({}),
        at(11),
    )
    .await
    .expect_err("validating -> completed skips the commit and is not an edge");
    assert_refused_by(&skipped, "not an edge");

    assert_eq!(
        bulk_repo::read(&conn, &scope(), TENANT, run.operation_id)
            .await
            .expect("read")
            .expect("exists")
            .state,
        BulkState::Validating,
        "and the refusal left the run where it was"
    );
}

/// D-137: a bulk import is never material, so the `CHECK` forbids the pair
/// outright — and Postgres names that `CHECK`, which is what tells this refusal
/// from the edge list's.
#[tokio::test]
#[ignore = "requires Docker"]
async fn an_import_cannot_wait_for_an_approval_it_never_needs() {
    let store = store().await;
    let conn = store.db.conn().expect("conn");
    let import = bulk_repo::open(&conn, &scope(), new_run(BulkKind::Import, "k-5"))
        .await
        .expect("open");
    let refused = bulk_repo::advance(
        &conn,
        &scope(),
        TENANT,
        import.operation_id,
        BulkState::Validating,
        BulkState::AwaitingApproval,
        serde_json::json!({}),
        at(11),
    )
    .await
    .expect_err("an import never awaits");
    assert_refused_by(&refused, "chk_pricing_bulk_operation_import_never_awaits");

    // The same move on a repricing run is legal — otherwise this case would pass
    // for a store that refused the state to everybody.
    let repricing = bulk_repo::open(&conn, &scope(), new_run(BulkKind::Repricing, "k-6"))
        .await
        .expect("open");
    let waiting = bulk_repo::advance(
        &conn,
        &scope(),
        TENANT,
        repricing.operation_id,
        BulkState::Validating,
        BulkState::AwaitingApproval,
        serde_json::json!({}),
        at(11),
    )
    .await
    .expect("a repricing run may wait");
    assert_eq!(waiting.state, BulkState::AwaitingApproval);
}

/// D-267's whole content, through the repository on the shipping engine: before
/// it, a refused batch approval stranded the run in `awaiting_approval` with no
/// exit.
///
/// The stamping half matters here specifically. [`bulk_repo::advance`] derives
/// `completed_at` from [`BulkState::is_terminal`], and `rejected` joined that set
/// four migrations after the table — a `is_terminal` that had not been widened
/// with it would write `NULL` and be refused by
/// `chk_pricing_bulk_operation_completed_at`, which is a disagreement between the
/// Rust enumeration and the `CHECK` that only an engine naming its constraints
/// reports legibly.
#[tokio::test]
#[ignore = "requires Docker"]
async fn a_rejected_run_leaves_awaiting_approval_and_is_over() {
    let store = store().await;
    let conn = store.db.conn().expect("conn");
    let run = bulk_repo::open(&conn, &scope(), new_run(BulkKind::Repricing, "k-7"))
        .await
        .expect("open");
    bulk_repo::advance(
        &conn,
        &scope(),
        TENANT,
        run.operation_id,
        BulkState::Validating,
        BulkState::AwaitingApproval,
        serde_json::json!({}),
        at(11),
    )
    .await
    .expect("waiting");

    let rejected = bulk_repo::advance(
        &conn,
        &scope(),
        TENANT,
        run.operation_id,
        BulkState::AwaitingApproval,
        BulkState::Rejected,
        serde_json::json!({ "reason": "refused" }),
        at(12),
    )
    .await
    .expect("awaiting_approval -> rejected");
    assert_eq!(rejected.state, BulkState::Rejected);
    assert_eq!(
        rejected.completed_at,
        Some(at(12)),
        "rejected is terminal and the CHECK stamps it"
    );

    let onward = bulk_repo::advance(
        &conn,
        &scope(),
        TENANT,
        run.operation_id,
        BulkState::Rejected,
        BulkState::Committing,
        serde_json::json!({}),
        at(13),
    )
    .await
    .expect_err("and nothing leaves it");
    assert_refused_by(&onward, "not an edge");
}

/// `inst-bk-lock` and `fr-concurrent-edit`: the mutual exclusion is the key, and
/// the point of reading the holder back is that the conflict can **name** it.
///
/// This is the autocommit half of the pair. Its transactional twin below is what
/// says the arrangement is load-bearing rather than incidental.
#[tokio::test]
#[ignore = "requires Docker"]
async fn two_runs_cannot_hold_one_row_and_the_refusal_names_the_holder() {
    let store = store().await;
    let conn = store.db.conn().expect("conn");
    let first = committing_run(&conn, "k-8").await;
    let second = committing_run(&conn, "k-9").await;
    let row = a_price_row(&store, "eu").await;

    bulk_repo::take_locks(&conn, &scope(), TENANT, first, &[row], at(11))
        .await
        .expect("the first run takes it");

    let clash = bulk_repo::take_locks(&conn, &scope(), TENANT, second, &[row], at(11))
        .await
        .expect_err("the second cannot");
    match clash {
        RepoError::BulkRowLocked {
            price_id,
            bulk_operation_id,
        } => {
            assert_eq!(price_id, row.to_string());
            assert_eq!(
                bulk_operation_id,
                first.to_string(),
                "the refusal names the run that holds it, not merely that one does"
            );
        }
        other => panic!("expected a bulk-row-lock refusal, got {other:?}"),
    }
}

/// **All or none, with the contended row second so a partial set genuinely
/// exists** (D-294).
///
/// Put the contended row *first* and the very first insert collides, nothing
/// partial is ever created, and the release under test is a no-op the case cannot
/// tell from a release that works — the mis-arrangement a recent fix wave had to
/// correct one layer up. Here the run takes `free_row`, collides on `held_row`,
/// and the lock it did take must be gone: a lock left behind is held by an
/// operation that is about to end, and freezes an interactive editor on that row
/// with no operator remedy, which is the freeze `inst-bs-done`'s "lock released
/// either way" and D-37's release path both exist to prevent.
///
/// Deleting the `release_locks` call from `take_locks`' failure arm reddens the
/// `prior_row` and `free_row` assertions below and nothing else.
///
/// # Why a lock taken in an *earlier* call is part of the fixture
///
/// The end state of "took `free_row`, then released it" and "never took
/// `free_row`" is the same: nobody holds it. So an assertion on `free_row` alone
/// cannot tell a working release from a release that never ran — it is green
/// either way, which is the shape of defect this program keeps finding.
///
/// `prior_row` closes that. It is taken in a **separate, successful** call and
/// asserted held *before* the failing one, so it is a lock that provably existed;
/// observing it gone afterwards is a direct observation that the release
/// executed. It also pins what the release actually is: `release_locks` is
/// bounded by the **run**, not by the rows of the call that failed, so a run that
/// was already holding rows loses those too.
#[tokio::test]
#[ignore = "requires Docker"]
async fn a_run_that_cannot_take_every_lock_releases_the_ones_it_took() {
    let store = store().await;
    let conn = store.db.conn().expect("conn");
    let neighbour = committing_run(&conn, "k-14").await;
    let run = committing_run(&conn, "k-15").await;
    let prior_row = a_price_row(&store, "fr").await;
    let free_row = a_price_row(&store, "us").await;
    let held_row = a_price_row(&store, "eu").await;

    bulk_repo::take_locks(&conn, &scope(), TENANT, neighbour, &[held_row], at(11))
        .await
        .expect("the neighbour holds the second row");
    bulk_repo::take_locks(&conn, &scope(), TENANT, run, &[prior_row], at(11))
        .await
        .expect("and this run is already holding a row of its own");
    assert_eq!(
        bulk_repo::lock_holder(&conn, &scope(), TENANT, prior_row)
            .await
            .expect("read the lock"),
        Some(run),
        "the fixture's premise: this lock exists before the failing call, so its \
         absence afterwards is an observation and not an assumption"
    );

    let clash = bulk_repo::take_locks(
        &conn,
        &scope(),
        TENANT,
        run,
        // Free first, contended second: the run takes one lock before it fails.
        &[free_row, held_row],
        at(12),
    )
    .await
    .expect_err("the batch cannot be taken whole");
    match clash {
        RepoError::BulkRowLocked {
            price_id,
            bulk_operation_id,
        } => {
            assert_eq!(
                price_id,
                held_row.to_string(),
                "the refusal names the row that was contended, not the batch"
            );
            assert_eq!(bulk_operation_id, neighbour.to_string());
        }
        other => panic!("expected a bulk-row-lock refusal, got {other:?}"),
    }

    assert_eq!(
        bulk_repo::lock_holder(&conn, &scope(), TENANT, prior_row)
            .await
            .expect("read the lock"),
        None,
        "the lock that provably existed before the call is gone, so the release ran"
    );
    assert_eq!(
        bulk_repo::lock_holder(&conn, &scope(), TENANT, free_row)
            .await
            .expect("read the lock"),
        None,
        "the lock this run did take must be released, not left held by a run that is over"
    );
    assert_eq!(
        bulk_repo::lock_holder(&conn, &scope(), TENANT, held_row)
            .await
            .expect("read the lock"),
        Some(neighbour),
        "and the release took only this run's, leaving the neighbour holding its own"
    );
}

/// **The autocommit contract, on the only engine where it is a contract.**
///
/// [`bulk_repo::take_locks`] documents that its runner must be an autocommit
/// connection and not a transaction. This case drives it inside one and pins what
/// actually happens: Postgres aborts the enclosing transaction the moment the
/// colliding insert fails, so the statements that follow it — the read that names
/// the holder, and the partial release behind it — cannot run, and the refusal comes
/// back as `RepoError::BulkLocksHeld` instead of a `RepoError::BulkRowLocked`
/// naming the holding run. That variant rather than a bare `RepoError::Db` because
/// a `take_locks` whose release did not run cannot know whether it holds anything
/// — here it does not, since the very first insert collided — and the safe half of
/// that ignorance is the one that keeps the caller's run recoverable.
///
/// It is asserted against the *same* contention the case above resolves cleanly
/// on an autocommit connection, which is what makes it a statement about the
/// runner rather than about the fixture. Together the two are the arming for
/// `postgres_bulk_commit`'s holder-naming case: a `commit_batch` that took its
/// locks inside a transaction would degrade exactly this way, every `SQLite`
/// suite would stay green, and an operator would get a driver error where
/// `fr-concurrent-edit` requires a run id.
#[tokio::test]
#[ignore = "requires Docker"]
async fn take_locks_inside_a_transaction_loses_the_holder() {
    let store = store().await;
    let conn = store.db.conn().expect("conn");
    let holder = committing_run(&conn, "k-16").await;
    let taker = committing_run(&conn, "k-17").await;
    let row = a_price_row(&store, "eu").await;

    bulk_repo::take_locks(&conn, &scope(), TENANT, holder, &[row], at(11))
        .await
        .expect("the holder takes it on an autocommit connection");

    // The same contention, the same call, on a transaction. The refusal is
    // captured out of band because the transaction's own result is not what is
    // under test.
    let sink: Arc<Mutex<Option<RepoError>>> = Arc::new(Mutex::new(None));
    let captured = Arc::clone(&sink);
    let committed: Result<(), DbError> = store
        .db
        .transaction(move |tx| {
            Box::pin(async move {
                let refusal =
                    bulk_repo::take_locks(tx, &scope(), TENANT, taker, &[row], at(12)).await;
                *captured.lock().expect("the sink is not poisoned") = refusal.err();
                Ok(())
            })
        })
        .await;
    // Deliberately not asserted: what the aborted transaction answers its own
    // COMMIT is not the subject, and pinning it here would tie this case to a
    // driver detail beside the one it is about.
    drop(committed);

    let degraded = sink
        .lock()
        .expect("the sink is not poisoned")
        .clone()
        .expect("the take must have been refused inside the transaction too");

    assert!(
        !matches!(degraded, RepoError::BulkRowLocked { .. }),
        "if this ever answers BulkRowLocked inside a transaction, the autocommit \
         requirement in `take_locks` has stopped being a rule and this case is the \
         place to say so: {degraded:?}"
    );
    match degraded {
        RepoError::BulkLocksHeld { cause: message, .. } => {
            assert!(
                message.contains("current transaction is aborted"),
                "the degradation must be Postgres refusing to run anything further in \
                 the aborted transaction, which is the reason the doc gives; got: {message}"
            );
            // **Which statement dies**, and worth pinning: the holder read runs
            // before the partial release, so it is the first casualty and the
            // release never happens at all.
            //
            // **The order is the whole reason this is a pin**: `lock_holder` runs
            // in front of `release_locks`, because releasing first
            // deleted every lock row of *this* run and made the subsequent read
            // find nothing -- so a run colliding with its own lock answered
            // `RepoError::Db` having just unlocked the rows it still needed. The
            // reorder is deliberate and this case is where its observable
            // consequence is recorded; the assertion below is what noticed the two
            // had swapped.
            assert!(
                message.contains("read pricing_bulk_row_lock"),
                "the holder read is what fails first, ahead of the partial release; \
                 if this ever reports the release instead, the order of the two inside \
                 `take_locks` has changed: {message}"
            );
        }
        other => panic!("expected the degraded storage refusal, got {other:?}"),
    }
}

/// Releasing frees the row for the next run, and takes **only** its own run's
/// locks.
///
/// Without the second half the release sweep is a `DELETE` nobody has bounded,
/// and one run finishing would free every row every other run is holding.
#[tokio::test]
#[ignore = "requires Docker"]
async fn releasing_frees_the_row_and_takes_only_its_own_runs_locks() {
    let store = store().await;
    let conn = store.db.conn().expect("conn");
    let mine = committing_run(&conn, "k-10").await;
    let theirs = committing_run(&conn, "k-11").await;
    let next = committing_run(&conn, "k-12").await;
    let my_rows = [
        a_price_row(&store, "eu").await,
        a_price_row(&store, "de").await,
    ];
    let their_row = a_price_row(&store, "us").await;

    bulk_repo::take_locks(&conn, &scope(), TENANT, mine, &my_rows, at(11))
        .await
        .expect("take both");
    bulk_repo::take_locks(&conn, &scope(), TENANT, theirs, &[their_row], at(11))
        .await
        .expect("theirs");
    assert_eq!(
        bulk_repo::lock_holder(&conn, &scope(), TENANT, my_rows[0])
            .await
            .expect("read"),
        Some(mine)
    );

    let freed = bulk_repo::release_locks(&conn, &scope(), TENANT, mine)
        .await
        .expect("release");
    assert_eq!(freed, 2, "both, and only this run's");
    assert_eq!(
        bulk_repo::lock_holder(&conn, &scope(), TENANT, my_rows[0])
            .await
            .expect("read"),
        None
    );
    assert_eq!(
        bulk_repo::lock_holder(&conn, &scope(), TENANT, their_row)
            .await
            .expect("read"),
        Some(theirs),
        "the other run still holds its own row"
    );

    bulk_repo::take_locks(&conn, &scope(), TENANT, next, &my_rows, at(12))
        .await
        .expect("the next run may take what was released");
}
