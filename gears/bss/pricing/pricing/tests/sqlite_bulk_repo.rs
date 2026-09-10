//! The bulk run's store surface, executed
//! (`design/12-operator-efficiency.md` §4, §6; D-260, D-262, D-267, D-290).
//!
//! **The edges are asserted through the store, not against a table written here.**
//! The state machine lives in `pricing_bulk_operation`'s trigger on both engines,
//! and the repository deliberately does not restate it — so the only way to say
//! what the edges are is to try them. A fixture listing them would be the second
//! spelling the repository refuses to be, and D-283 is what a rule judged against
//! a private copy of the schema costs.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use bss_pricing::domain::bulk::{BulkKind, BulkState};
use bss_pricing::domain::instant::utc_ymd_hms;
use bss_pricing::domain::money::{CurrencyCode, MinorAmount};
use bss_pricing::domain::price_record::PriceContent;
use bss_pricing::domain::price_row::{ModelKind, PriceRow};
use bss_pricing::domain::scope_key::{
    ChargeKind, Cohort, PhaseId, PlanId, PriceEligibility, Region, ScopeKey,
};
use bss_pricing::infra::storage::RepoError;
use bss_pricing::infra::storage::migrations::Migrator;
use bss_pricing::infra::storage::repo::{
    IdempotencyGate, NewBulkOperation, NewPriceDraft, PriceRepo, bulk_repo,
};
use time::OffsetDateTime;

use sea_orm_migration::MigratorTrait;
use toolkit_db::migration_runner::run_migrations_for_testing;
use toolkit_db::secure::AccessScope;
use toolkit_db::{ConnectOpts, DBProvider, DbError, connect_db};
use uuid::Uuid;

const TENANT: Uuid = Uuid::from_u128(0x7e_41);
const ACTOR: Uuid = Uuid::from_u128(0xac_40);

fn at(hour: u32) -> OffsetDateTime {
    utc_ymd_hms(2026, 8, 9, hour, 0, 0)
}
fn scope() -> AccessScope {
    AccessScope::for_tenant(TENANT)
}

/// The refusal, **by the words the guard itself raises**.
///
/// `is_err()` is satisfied by every refusal this store can produce - a foreign
/// key, a unique index, a different arm of the same trigger, a
/// `ConcurrentMutation` - so a case pinned that way stays green when the rule it
/// names stops being the one that answers. Mirrors
/// `postgres_bulk_repo::assert_refused_by`, which the twin adopted first.
fn assert_refused_by(err: &RepoError, by: &str) {
    let message = err.to_string();
    assert!(
        message.contains(by),
        "the refusal must be the one under test (`{by}`), got: {message}"
    );
}

async fn provider() -> DBProvider<DbError> {
    let db = connect_db("sqlite::memory:", ConnectOpts::default())
        .await
        .expect("connect in-memory sqlite");
    run_migrations_for_testing(&db, Migrator::migrations())
        .await
        .expect("run migrator");
    DBProvider::<DbError>::new(db)
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

/// Author a real draft price row and answer its id.
///
/// **The lock's `price_id` is a foreign key**, so a lock cannot be taken on a row
/// that does not exist — which is the store saying that `inst-bk-lock` is about
/// *rows an edit could target*, not about a set of ids a run declares.
async fn a_price_row(p: &DBProvider<DbError>, region: &str) -> Uuid {
    let price_id = Uuid::now_v7();
    let mut row = PriceRow::new(ChargeKind::Recurring, Some(ModelKind::Flat));
    row.amount_minor = Some(MinorAmount::new(9_900).expect("a non-negative amount"));
    PriceRepo::new(p.clone())
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

/// Open a run and move it to `committing`, which is the only state its locks may
/// be taken in — the lock table's own trigger says so ("the bulk lock takes
/// effect only on entry to committing"), and it is right: a lock belongs to a run
/// that is actually committing, not to one still validating.
async fn committing_run(conn: &toolkit_db::secure::DbConn<'_>, client_key: &str) -> Uuid {
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

#[tokio::test]
async fn a_run_is_born_validating_whatever_the_caller_wanted() {
    // `NewBulkOperation` carries no state, and this is why: the insert trigger
    // refuses any birth state but `validating`, so a field would offer a choice
    // the store takes back.
    let p = provider().await;
    let conn = p.conn().expect("conn");
    let run = bulk_repo::open(&conn, &scope(), new_run(BulkKind::Import, "k-1"))
        .await
        .expect("open the run");

    assert_eq!(run.state, BulkState::Validating);
    assert_eq!(run.kind, BulkKind::Import);
    assert!(run.completed_at.is_none(), "a run at birth is not over");
    assert_eq!(
        bulk_repo::read(&conn, &scope(), TENANT, run.operation_id)
            .await
            .expect("read it back")
            .expect("it exists"),
        run,
        "what `open` answers is what the store holds"
    );
}

#[tokio::test]
async fn one_client_key_opens_one_run() {
    // O4's key is unique per tenant, which is what makes the replay possible at
    // all: a second run under one key would leave two reports to answer with.
    let p = provider().await;
    let conn = p.conn().expect("conn");
    bulk_repo::open(&conn, &scope(), new_run(BulkKind::Import, "k-2"))
        .await
        .expect("the first");

    // **And the class is the contract's** (Z2-2). Both callers replay by reading
    // `find_by_client_key` first and opening second, in two different
    // transactions, so two concurrent submits under one `Idempotency-Key` both see
    // the key free and both reach `open`. The loser is refused by
    // `uq_pricing_bulk_operation_client_key`, and as a bare `RepoError::Db` that
    // is a 500 for a request whose contract promises either the winner's 202 or a
    // refusal the caller can act on. `ConcurrentMutation` is a 409 whose retry
    // does succeed: the second attempt finds the winner's run and replays it,
    // which is the one thing D-159's "a retry is expected to succeed" asks of the
    // class.
    let second = bulk_repo::open(&conn, &scope(), new_run(BulkKind::Import, "k-2")).await;
    let Err(RepoError::ConcurrentMutation { aggregate }) = second else {
        panic!("the second is refused by the unique key, as a contention; got {second:?}");
    };
    assert!(
        aggregate.contains("k-2"),
        "and D-159 requires the refusal to name what the caller retries against: {aggregate}"
    );

    let found = bulk_repo::find_by_client_key(&conn, &scope(), TENANT, BulkKind::Import, "k-2")
        .await
        .expect("read by key")
        .expect("the first run");
    assert_eq!(found.client_key, "k-2");
}

#[tokio::test]
async fn an_imports_happy_path_is_validating_then_committing_then_completed() {
    let p = provider().await;
    let conn = p.conn().expect("conn");
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

#[tokio::test]
async fn a_move_that_is_not_an_edge_is_refused_by_the_store() {
    // The repository names a state and writes it; the trigger decides. So this
    // asserts the trigger, which is the only place the edges live.
    let p = provider().await;
    let conn = p.conn().expect("conn");
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
    .await;
    assert_refused_by(
        skipped
            .as_ref()
            .expect_err("validating -> completed skips the commit and is not an edge"),
        "not an edge of the bulk state machine",
    );
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

#[tokio::test]
async fn an_import_cannot_wait_for_an_approval_it_never_needs() {
    // D-137: a bulk import is never material, so the `CHECK` forbids the pair
    // outright. The state exists for mass repricing, whose rows are published.
    let p = provider().await;
    let conn = p.conn().expect("conn");
    let import = bulk_repo::open(&conn, &scope(), new_run(BulkKind::Import, "k-5"))
        .await
        .expect("open");
    assert_refused_by(
        &bulk_repo::advance(
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
        .expect_err("an import may never park awaiting an approval"),
        "chk_pricing_bulk_operation_import_never_awaits",
    );

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

#[tokio::test]
async fn a_rejected_run_leaves_awaiting_approval_and_is_over() {
    // D-267's whole content: before it, a refused batch approval stranded the run
    // in `awaiting_approval` with no exit.
    let p = provider().await;
    let conn = p.conn().expect("conn");
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

    assert_refused_by(
        &bulk_repo::advance(
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
        .expect_err("nothing leaves rejected"),
        "not an edge of the bulk state machine",
    );
}

#[tokio::test]
async fn two_runs_cannot_hold_one_row_and_the_refusal_names_the_holder() {
    // `inst-bk-lock` and `fr-concurrent-edit`: the mutual exclusion is the key,
    // and the point of reading the holder back is that the conflict can name it.
    let p = provider().await;
    let conn = p.conn().expect("conn");
    let first = committing_run(&conn, "k-8").await;
    let second = committing_run(&conn, "k-9").await;
    let row = a_price_row(&p, "eu").await;

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

#[tokio::test]
async fn releasing_frees_the_row_for_the_next_run() {
    let p = provider().await;
    let conn = p.conn().expect("conn");
    let first = committing_run(&conn, "k-10").await;
    let second = committing_run(&conn, "k-11").await;
    let rows = [a_price_row(&p, "eu").await, a_price_row(&p, "us").await];

    bulk_repo::take_locks(&conn, &scope(), TENANT, first, &rows, at(11))
        .await
        .expect("take both");
    assert_eq!(
        bulk_repo::lock_holder(&conn, &scope(), TENANT, rows[0])
            .await
            .expect("read"),
        Some(first)
    );

    let freed = bulk_repo::release_locks(&conn, &scope(), TENANT, first)
        .await
        .expect("release");
    assert_eq!(freed, 2, "both, and only this run's");
    assert_eq!(
        bulk_repo::lock_holder(&conn, &scope(), TENANT, rows[0])
            .await
            .expect("read"),
        None
    );

    bulk_repo::take_locks(&conn, &scope(), TENANT, second, &rows, at(12))
        .await
        .expect("the next run may take them");
}

#[tokio::test]
async fn a_release_takes_only_its_own_runs_locks() {
    // Without this the release sweep is a `DELETE` nobody has bounded, and one
    // run finishing would free every row every other run is holding.
    let p = provider().await;
    let conn = p.conn().expect("conn");
    let mine = committing_run(&conn, "k-12").await;
    let theirs = committing_run(&conn, "k-13").await;
    let my_row = a_price_row(&p, "eu").await;
    let their_row = a_price_row(&p, "us").await;

    bulk_repo::take_locks(&conn, &scope(), TENANT, mine, &[my_row], at(11))
        .await
        .expect("mine");
    bulk_repo::take_locks(&conn, &scope(), TENANT, theirs, &[their_row], at(11))
        .await
        .expect("theirs");

    assert_eq!(
        bulk_repo::release_locks(&conn, &scope(), TENANT, mine)
            .await
            .expect("release"),
        1
    );
    assert_eq!(
        bulk_repo::lock_holder(&conn, &scope(), TENANT, their_row)
            .await
            .expect("read"),
        Some(theirs),
        "the other run still holds its own row"
    );
}

/// **A run retaking a lock it already holds keeps it** (review M44).
///
/// `take_locks` used to release before it read: the insert was refused, this
/// run's whole partial set was deleted, and only then was `lock_holder` asked --
/// which by that point found nothing, because the row it would have named had
/// just been deleted along with the rest. So a run colliding with **itself**
/// answered `RepoError::Db`, a 500, having unlocked every row it still needed.
///
/// No production path reaches it today: there is no redrive route. It is armed
/// anyway because `infra::repricing` justifies leaving a wedged run `committing`
/// on the stated ground that a second call "simply retakes the lock and carries
/// on" -- which was false, and D-37's redrive was going to be built on it.
///
/// The second call names the held row **and one more**, so the assertion
/// separates "the collision was tolerated" from "the call did nothing": a
/// `continue` that skipped the rest of the set would leave `second` unlocked.
#[tokio::test]
async fn a_run_retaking_a_lock_it_already_holds_keeps_every_row() {
    let p = provider().await;
    let conn = p.conn().expect("conn");
    let run = committing_run(&conn, "k-14").await;
    let held = a_price_row(&p, "eu").await;
    let fresh = a_price_row(&p, "us").await;

    bulk_repo::take_locks(&conn, &scope(), TENANT, run, &[held], at(11))
        .await
        .expect("the run takes its first row");

    // The redrive, arriving in exactly the state a wedged `committing` run is in.
    bulk_repo::take_locks(&conn, &scope(), TENANT, run, &[held, fresh], at(12))
        .await
        .expect("a run's own lock is not a collision with anybody");

    for (label, row) in [("the row it already held", held), ("the new one", fresh)] {
        assert_eq!(
            bulk_repo::lock_holder(&conn, &scope(), TENANT, row)
                .await
                .expect("read"),
            Some(run),
            "{label} must still be this run's after the retake"
        );
    }
}

/// The control on the case above: a lock held by **somebody else** is still a
/// refusal, and the partial set this run had already taken is still released.
///
/// Without it the `continue` would be indistinguishable from a `take_locks` that
/// had stopped excluding anybody -- `inst-bk-lock` is the mutual exclusion, and a
/// self-collision is the one exception to it. The release half is asserted for the
/// same reason it exists: the inserts are independent statements, so a run refused
/// partway would otherwise end holding rows nobody will free.
#[tokio::test]
async fn a_lock_another_run_holds_is_still_refused_and_frees_the_partial_set() {
    let p = provider().await;
    let conn = p.conn().expect("conn");
    let mine = committing_run(&conn, "k-15").await;
    let theirs = committing_run(&conn, "k-16").await;
    let free = a_price_row(&p, "eu").await;
    let contested = a_price_row(&p, "us").await;

    bulk_repo::take_locks(&conn, &scope(), TENANT, theirs, &[contested], at(11))
        .await
        .expect("the other run takes the contested row first");

    // `free` is taken, then `contested` collides -- so there is a partial set to
    // release at the moment the refusal is decided.
    let clash = bulk_repo::take_locks(&conn, &scope(), TENANT, mine, &[free, contested], at(12))
        .await
        .expect_err("a row another run holds is not takeable");
    match clash {
        RepoError::BulkRowLocked {
            price_id,
            bulk_operation_id,
        } => {
            assert_eq!(price_id, contested.to_string());
            assert_eq!(
                bulk_operation_id,
                theirs.to_string(),
                "the holder is read after the release and must still be named"
            );
        }
        other => panic!("expected a bulk-row-lock refusal, got {other:?}"),
    }

    assert_eq!(
        bulk_repo::lock_holder(&conn, &scope(), TENANT, free)
            .await
            .expect("read"),
        None,
        "all or none: the row this run had already taken is released"
    );
    assert_eq!(
        bulk_repo::lock_holder(&conn, &scope(), TENANT, contested)
            .await
            .expect("read"),
        Some(theirs),
        "and the other run keeps what it held"
    );
}

// ---------------------------------------------------------------------------
// Z8-7 — the compare-and-set premise, and the self-edge the trigger admits.
// ---------------------------------------------------------------------------

/// **The RED this case is about.** `pricing_bulk_operation`'s transition trigger
/// returns early when `NEW.state = OLD.state` — a self-edge is not judged at all
/// — so a second [`bulk_repo::advance`] to the state a run already holds lands
/// silently. And `advance` does not merely restate the state: it rewrites
/// `report` and `completed_at` wholesale, so the repeat **clobbers the run's
/// stored answer** with whatever the late caller happened to carry. That answer
/// is the operator-facing record of money that moved.
///
/// The premise has to be *in the statement*, which is `window_repo`'s own rule —
/// "a tag read, compared and then handed to a statement is a decision racing the
/// write it authorizes" — and every look-alike in this crate obeys it
/// (`approval_repo::swap`, `overlay_repo`, `window_repo`, `pin_frontier_repo`,
/// `policy_repo`, `idempotency_repo`).
///
/// Armed at the **repeat**, not at the happy path: the first two moves are here
/// only as the fixture that makes the third one a repeat.
#[tokio::test]
async fn a_repeat_move_to_the_state_a_run_already_holds_is_refused_and_leaves_its_report_alone() {
    let p = provider().await;
    let conn = p.conn().expect("conn");
    let run = bulk_repo::open(&conn, &scope(), new_run(BulkKind::Import, "k-cas-1"))
        .await
        .expect("open");

    bulk_repo::advance(
        &conn,
        &scope(),
        TENANT,
        run.operation_id,
        BulkState::Validating,
        BulkState::Committing,
        serde_json::json!({ "phase": "committing", "rows": 2 }),
        at(11),
    )
    .await
    .expect("validating -> committing");

    let landed = bulk_repo::advance(
        &conn,
        &scope(),
        TENANT,
        run.operation_id,
        BulkState::Committing,
        BulkState::CompletedWithConflicts,
        serde_json::json!({ "committed": [{ "row": 0 }], "conflicted": [] }),
        at(12),
    )
    .await
    .expect("committing -> completed_with_conflicts");

    // The repeat. A caller that read `committing` a moment ago and is only now
    // writing its own terminal move — an abort racing the commit's own tail,
    // which is the shape `bulk_imports`' route guard stands in front of and
    // cannot close, being a read.
    let repeat = bulk_repo::advance(
        &conn,
        &scope(),
        TENANT,
        run.operation_id,
        BulkState::Committing,
        BulkState::CompletedWithConflicts,
        serde_json::json!({ "committed": [], "conflicted": [], "aborted": "clobbered" }),
        at(13),
    )
    .await;

    let err = repeat.expect_err(
        "the move's premise, the run still being `committing`, is gone, and the trigger cannot \
         say so because a self-edge returns early on both engines",
    );
    assert!(
        matches!(err, RepoError::ConcurrentMutation { .. }),
        "the refusal is a lost-update refusal naming the state the run now reads, not a \
         not-found: {err:?}"
    );

    let after = bulk_repo::read(&conn, &scope(), TENANT, run.operation_id)
        .await
        .expect("read")
        .expect("exists");
    assert_eq!(
        after.report, landed.report,
        "and the run's stored report is the one the commit wrote, not the repeat's"
    );
    assert_eq!(
        after.completed_at,
        Some(at(12)),
        "nor was the end instant re-stamped"
    );
}

/// The **positive control** for the case above: a legitimate edge whose named
/// prior state is the run's own still moves, and still returns the moved record.
/// Without this, a store that refused every `advance` outright would satisfy the
/// refusal case.
#[tokio::test]
async fn the_named_prior_state_admits_the_edge_it_belongs_to() {
    let p = provider().await;
    let conn = p.conn().expect("conn");
    let run = bulk_repo::open(&conn, &scope(), new_run(BulkKind::Import, "k-cas-2"))
        .await
        .expect("open");

    let committing = bulk_repo::advance(
        &conn,
        &scope(),
        TENANT,
        run.operation_id,
        BulkState::Validating,
        BulkState::Committing,
        serde_json::json!({ "phase": "committing" }),
        at(11),
    )
    .await
    .expect("validating -> committing, whose premise is the run's own state");
    assert_eq!(committing.state, BulkState::Committing);

    let done = bulk_repo::advance(
        &conn,
        &scope(),
        TENANT,
        run.operation_id,
        BulkState::Committing,
        BulkState::Completed,
        serde_json::json!({ "committed": [] }),
        at(12),
    )
    .await
    .expect("committing -> completed");
    assert_eq!(done.state, BulkState::Completed);
    assert_eq!(done.completed_at, Some(at(12)));
}

/// A stale premise the trigger **cannot** catch, because the move it names is a
/// legal edge from where the row now stands.
///
/// `validating -> committing` and `awaiting_approval -> committing` are both
/// edges. So a caller that read a repricing run `validating`, decided the change
/// was immaterial and only then wrote `committing` would commit a run a
/// concurrent evaluation had meanwhile parked in `awaiting_approval` — spending
/// `inst-bs-commit`'s edge on a run whose approval nobody has given. The
/// trigger's edge list admits that write; only the caller's own premise refuses
/// it, which is why the premise has to travel into the statement.
#[tokio::test]
async fn a_move_whose_premise_has_moved_is_refused_even_when_the_edge_is_legal() {
    let p = provider().await;
    let conn = p.conn().expect("conn");
    let run = bulk_repo::open(&conn, &scope(), new_run(BulkKind::Repricing, "k-cas-3"))
        .await
        .expect("open");

    // Somebody else's evaluation found the run material and parked it.
    bulk_repo::advance(
        &conn,
        &scope(),
        TENANT,
        run.operation_id,
        BulkState::Validating,
        BulkState::AwaitingApproval,
        serde_json::json!({ "materiality": "always" }),
        at(11),
    )
    .await
    .expect("validating -> awaiting_approval");

    // Ours still believes it read `validating`.
    let raced = bulk_repo::advance(
        &conn,
        &scope(),
        TENANT,
        run.operation_id,
        BulkState::Validating,
        BulkState::Committing,
        serde_json::json!({ "phase": "committing" }),
        at(12),
    )
    .await;

    // The variant, not `is_err()`: a stale premise is a `ConcurrentMutation`, and
    // every other refusal this call can raise would mean something else entirely.
    assert!(
        matches!(raced, Err(RepoError::ConcurrentMutation { .. })),
        "the premise the caller acted on is gone, and the store is where that is \
         judged: {raced:?}"
    );
    assert_eq!(
        bulk_repo::read(&conn, &scope(), TENANT, run.operation_id)
            .await
            .expect("read")
            .expect("exists")
            .state,
        BulkState::AwaitingApproval,
        "the run is still waiting for the approval nobody has given"
    );
}
