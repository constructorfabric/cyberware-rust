//! The migration schedule against a real database (`inst-ms-api`,
//! `inst-mg-idem`, `inst-mst-start`, `inst-mst-complete`, `inst-mst-cancel`,
//! `inst-mst-cancel-inflight`, D-34, D-65).
//!
//! Everything here is a property of a **statement** rather than of a branch in
//! Rust, which is why it is a store suite and not a unit one:
//!
//! * **M2's idempotency is the primary key.** `insert_or_load` is an
//!   `INSERT ... ON CONFLICT DO NOTHING` plus a load, so two creates of one
//!   `migration_id` cannot both land however they interleave. A read-then-insert
//!   would pass a unit test and race in production.
//! * **Every flip is a compare-and-swap.** Each `UPDATE` carries the state it
//!   expects to leave in its `WHERE`; delete that conjunct and the "a completed
//!   run cannot be cancelled" case below stops proving anything, because the
//!   refusal would then come from a Rust `if` that a second writer can step past.
//! * **The trigger refuses what the domain refuses, independently.** §4's edge
//!   set is stated in `domain::migration`, in the table's `CHECK`s and in
//!   `trg_pricing_migration_flip_whitelist`. The cases that drive a raw `UPDATE`
//!   past the repository are what prove the third one exists.
//!
//! `sqlite::memory:` serializes writers, so nothing here distinguishes isolation
//! levels; what it proves is which layer produced each refusal.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use bss_pricing::domain::migration::MigrationState;
use bss_pricing::domain::scope_key::PlanId;
use bss_pricing::infra::storage::RepoError;
use bss_pricing::infra::storage::migrations::Migrator;
use bss_pricing::infra::storage::repo::migration_repo::{self, NewMigration};

use bss_pricing::domain::instant::utc_ymd_hms;
use sea_orm_migration::MigratorTrait;
use serde_json::json;
use time::Duration;
use time::OffsetDateTime;
use toolkit_db::secure::AccessScope;
use toolkit_db::{ConnectOpts, DBProvider, DbError, connect_db};
use uuid::Uuid;

mod common;

use common::{exec, migrated_db};

const TENANT: Uuid = Uuid::from_u128(0x_7e_11_51);
const ACTOR: Uuid = Uuid::from_u128(0x_ac_11);
const SOURCE: Uuid = Uuid::from_u128(0x_50_04_ce);
const TARGET: Uuid = Uuid::from_u128(0x_7a_46_e7);

fn at(hour: u32) -> OffsetDateTime {
    utc_ymd_hms(2026, 8, 7, hour, 0, 0)
}

fn scope() -> AccessScope {
    AccessScope::for_tenant(TENANT)
}

async fn harness() -> DBProvider<DbError> {
    let db = connect_db("sqlite::memory:", ConnectOpts::default())
        .await
        .expect("connect in-memory sqlite");
    toolkit_db::migration_runner::run_migrations_for_testing(&db, Migrator::migrations())
        .await
        .expect("run migrator");
    DBProvider::<DbError>::new(db)
}

fn new_migration(migration_id: Uuid) -> NewMigration {
    NewMigration {
        migration_id,
        tenant_id: TENANT,
        source_plan_id: PlanId::new(SOURCE),
        source_revision: 0,
        target_plan_id: PlanId::new(TARGET),
        effective_at: at(10) + Duration::days(90),
        announced_at: at(10),
        scope: json!({ "kind": "all" }),
        delta_report: json!({ "locked": [], "entitlement": [], "addon": [], "boundary": [] }),
        created_by: ACTOR,
        created_at: at(10),
    }
}

// ---------------------------------------------------------------------------
// M2 — the create is idempotent on the client-supplied key.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_schedule_is_created_once_and_read_back_whole() {
    let provider = harness().await;
    let conn = provider.conn().expect("conn");
    let id = Uuid::now_v7();

    let scheduled = migration_repo::insert_or_load(&conn, &scope(), new_migration(id))
        .await
        .expect("schedule");

    assert!(scheduled.created);
    assert_eq!(scheduled.record.migration_id, id);
    assert_eq!(scheduled.record.state, MigrationState::Scheduled);
    // D-65: nothing is persisted until a start declares itself.
    assert!(scheduled.record.exclusion_snapshot.is_none());
    assert!(scheduled.record.completion_record.is_none());
}

/// **Two tenants may schedule the same `migration_id`, and neither learns of the
/// other** (`inst-ms-api`'s key is a client's, and a client's key is theirs).
///
/// `migration_id` arrives straight off the request body and was the table's whole
/// `PRIMARY KEY` until 2026-08-11, so its namespace was the deployment rather than
/// the tenant. That is two defects in one column, and the second is the worse:
///
/// - **An existence oracle on a live route.** Tenant B posting an id tenant A holds
///   took the `DO NOTHING` branch, so `load` — which filters by `tenant_id` — found
///   nothing and B was answered `ConcurrentMutation` (409). An unused id answered
///   202. The difference is observable and is a fact about another tenant's rows.
/// - **A permanent denial with no remedy.** That refusal says *retry*, and a retry
///   collides identically, forever. Any tenant could reserve arbitrary migration
///   ids against every other tenant, and the victim has none: `DELETE` on this
///   table is refused by its own trigger.
///
/// Every sibling client-key store in this crate already scopes its conflict target
/// — `synthesis_repo` on `(tenant_id, subscription_ref)`, `idempotency_repo` on
/// `(tenant_id, operation, client_key)`, and `bulk_repo` on
/// `(tenant_id, kind, client_key)` since D-307. `pricing_migration` was the last
/// one global.
#[tokio::test]
async fn two_tenants_may_hold_one_migration_id_and_neither_can_deny_the_other() {
    const OTHER_TENANT: Uuid = Uuid::from_u128(0x_7e_11_52);

    let provider = harness().await;
    let conn = provider.conn().expect("conn");
    let shared = Uuid::now_v7();

    let mine = migration_repo::insert_or_load(&conn, &scope(), new_migration(shared))
        .await
        .expect("the first tenant schedules");
    assert!(mine.created);

    let mut theirs = new_migration(shared);
    theirs.tenant_id = OTHER_TENANT;
    let neighbour =
        migration_repo::insert_or_load(&conn, &AccessScope::for_tenant(OTHER_TENANT), theirs)
            .await
            .expect("a neighbour's identical id is not this tenant's business");

    assert!(
        neighbour.created,
        "the neighbour scheduled their own run; a `created = false` here is the \
         DO NOTHING branch reporting another tenant's row as this one's replay"
    );
    assert_eq!(neighbour.record.migration_id, shared);

    // And the first tenant's row is untouched by any of it.
    let reread = migration_repo::load(&conn, &scope(), TENANT, shared)
        .await
        .expect("read")
        .expect("the first tenant still holds its own schedule");
    assert_eq!(reread.migration_id, shared);
    assert_eq!(reread.state, MigrationState::Scheduled);
}

/// **D-144's quantum on the authored `effectiveAt`, and deliberately not on
/// `announcedAt`.**
///
/// `effectiveAt` arrives on the schedule request, is carried back in a contract
/// field and is *compared* — D-49's notice floor is `effective_at - announced_at`
/// — so it is inside the rule the whole gate exists for. `announcedAt` is the
/// scheduling commit instant the gear mints from `OffsetDateTime::now_utc()`, so applying the
/// quantum to it would refuse **every** schedule; `window_repo::transition`
/// measured exactly that on its flip timestamp and says so.
///
/// The offset is 137 microseconds — a prime number of them, so no implementation
/// that divided or truncated its way to an answer could land on the same instant
/// by accident, and nothing about the value is a round fraction of the quantum.
#[tokio::test]
async fn an_effective_instant_finer_than_the_quantum_is_refused_and_the_announcement_is_not() {
    let provider = harness().await;
    let conn = provider.conn().expect("conn");

    let mut unquantized = new_migration(Uuid::now_v7());
    unquantized.effective_at += Duration::microseconds(137);
    let refusal = migration_repo::insert_or_load(&conn, &scope(), unquantized)
        .await
        .expect_err("a sub-millisecond effectiveAt must be refused");
    assert!(
        matches!(
            &refusal,
            RepoError::TimestampPrecisionExceeded { field, .. } if field == "effectiveAt"
        ),
        "got {refusal:?}"
    );

    // The announcement instant is the commit's own and is **not** subject to the
    // rule: a schedule whose `announced_at` carries the microseconds `OffsetDateTime::now_utc()`
    // hands every caller still lands.
    let mut machine_stamped = new_migration(Uuid::now_v7());
    machine_stamped.announced_at += Duration::microseconds(137);
    machine_stamped.created_at += Duration::microseconds(137);
    let scheduled = migration_repo::insert_or_load(&conn, &scope(), machine_stamped)
        .await
        .expect("a machine-stamped announcement is not a precision fault");
    assert!(scheduled.created);
}

#[tokio::test]
async fn a_revision_beyond_the_old_columns_range_round_trips() {
    // Z6-7, and the measurement `pricing_snapshot_provenance`'s module doc leans on rather than
    // asserting: `source_revision` was `integer` with an `i32::try_from` in front of
    // it, so a source plan standing at a revision above 2^31-1 was refused
    // `CorruptRow` — a fail-closed refusal, but a refusal of a revision the plan's own
    // `bigint` column can hold and the domain's `u64` can name.
    //
    // The value is `i32::MAX as u64 + 1` and not something round, because what is
    // being proved is exactly the old boundary: one less than this passed before the
    // widening too, so a probe at any smaller value would have been green against the
    // defect.
    //
    // It round-trips rather than merely inserting: the read is what proves the width
    // survives storage on the engine whose declared type this migration deliberately
    // did not change.
    let provider = harness().await;
    let conn = provider.conn().expect("conn");
    let id = Uuid::now_v7();
    let beyond = u64::try_from(i32::MAX).expect("i32::MAX is non-negative") + 1;

    let mut wide = new_migration(id);
    wide.source_revision = beyond;
    let scheduled = migration_repo::insert_or_load(&conn, &scope(), wide)
        .await
        .expect("a revision above the old integer column's range must be storable");

    assert_eq!(scheduled.record.source_revision, beyond);

    let read = migration_repo::load(&conn, &scope(), TENANT, id)
        .await
        .expect("read back")
        .expect("the schedule is there");
    assert_eq!(
        read.source_revision, beyond,
        "the revision must survive the round trip, not merely the insert"
    );
}

#[tokio::test]
async fn a_retry_of_one_migration_id_returns_the_original_schedule_and_never_a_second() {
    // `inst-ms-api`, verbatim: "a timed-out client retry returns the original
    // schedule, never a second one".
    let provider = harness().await;
    let conn = provider.conn().expect("conn");
    let id = Uuid::now_v7();

    let first = migration_repo::insert_or_load(&conn, &scope(), new_migration(id))
        .await
        .expect("first");

    // A retry whose body differs: the stored schedule is what answers, because
    // the key is the identity and the second body never lands.
    let mut retry = new_migration(id);
    retry.effective_at = at(10) + Duration::days(365);
    let second = migration_repo::insert_or_load(&conn, &scope(), retry)
        .await
        .expect("retry");

    assert!(first.created);
    assert!(!second.created, "the retry must not report a fresh create");
    assert_eq!(second.record, first.record);
    assert_eq!(second.record.effective_at, at(10) + Duration::days(90));

    assert_eq!(
        count_rows(&conn, id).await,
        1,
        "one migration_id must never hold two rows"
    );
}

// ---------------------------------------------------------------------------
// §4's edges, through the repository.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn start_persists_the_exclusion_set_and_a_repeat_start_replays_it_verbatim() {
    // D-65, sharpened 2026-07-31: the set is computed once, persisted, and repeat
    // calls return **that stored snapshot** without re-transitioning, because a
    // recompute could differ from the set the executor already honoured.
    let provider = harness().await;
    let conn = provider.conn().expect("conn");
    let id = Uuid::now_v7();
    migration_repo::insert_or_load(&conn, &scope(), new_migration(id))
        .await
        .expect("schedule");

    let first = migration_repo::start(
        &conn,
        &scope(),
        TENANT,
        id,
        json!({ "locked": ["sub-1"] }),
        at(11),
    )
    .await
    .expect("start")
    .expect("present");

    assert!(first.transitioned);
    assert_eq!(first.record.state, MigrationState::InProgress);

    // The replay carries a *different* set. It must be ignored.
    let replay = migration_repo::start(
        &conn,
        &scope(),
        TENANT,
        id,
        json!({ "locked": ["sub-1", "sub-2"] }),
        at(12),
    )
    .await
    .expect("replay")
    .expect("present");

    assert!(!replay.transitioned, "a replay must not re-transition");
    assert_eq!(
        replay.record.exclusion_snapshot,
        Some(json!({ "locked": ["sub-1"] })),
        "the replay must return the set the executor was already handed"
    );
}

#[tokio::test]
async fn a_started_run_completes_with_its_record() {
    let provider = harness().await;
    let conn = provider.conn().expect("conn");
    let id = Uuid::now_v7();
    migration_repo::insert_or_load(&conn, &scope(), new_migration(id))
        .await
        .expect("schedule");
    migration_repo::start(&conn, &scope(), TENANT, id, json!({}), at(11))
        .await
        .expect("start");

    let completed = migration_repo::complete(
        &conn,
        &scope(),
        TENANT,
        id,
        json!({ "processed": 12, "excluded": ["sub-1"], "failed": [] }),
        at(13),
    )
    .await
    .expect("complete");

    assert_eq!(completed.state, MigrationState::Completed);
    assert_eq!(
        completed.completion_record,
        Some(json!({ "processed": 12, "excluded": ["sub-1"], "failed": [] }))
    );
}

#[tokio::test]
async fn a_scheduled_run_cancels_with_nothing_attempted() {
    let provider = harness().await;
    let conn = provider.conn().expect("conn");
    let id = Uuid::now_v7();
    migration_repo::insert_or_load(&conn, &scope(), new_migration(id))
        .await
        .expect("schedule");

    let cancelled = migration_repo::cancel(
        &conn,
        &scope(),
        TENANT,
        id,
        MigrationState::Scheduled,
        None,
        at(11),
    )
    .await
    .expect("cancel");

    assert_eq!(cancelled.state, MigrationState::Cancelled);
    // Nothing ran, so there is no partial set to list.
    assert!(cancelled.completion_record.is_none());
    assert!(cancelled.exclusion_snapshot.is_none());
}

#[tokio::test]
async fn an_in_flight_run_cancels_and_lists_its_partial_sets() {
    // D-34, the stop-the-bleeding control. Already-migrated subscriptions are
    // unaffected **by construction**: the catalog never held their state.
    let provider = harness().await;
    let conn = provider.conn().expect("conn");
    let id = Uuid::now_v7();
    migration_repo::insert_or_load(&conn, &scope(), new_migration(id))
        .await
        .expect("schedule");
    migration_repo::start(&conn, &scope(), TENANT, id, json!({}), at(11))
        .await
        .expect("start");

    let cancelled = migration_repo::cancel(
        &conn,
        &scope(),
        TENANT,
        id,
        MigrationState::InProgress,
        Some(json!({ "migrated": ["sub-1"], "not_attempted": ["sub-2", "sub-3"] })),
        at(12),
    )
    .await
    .expect("cancel in flight");

    assert_eq!(cancelled.state, MigrationState::Cancelled);
    assert_eq!(
        cancelled.completion_record,
        Some(json!({ "migrated": ["sub-1"], "not_attempted": ["sub-2", "sub-3"] }))
    );
}

#[tokio::test]
async fn a_flip_the_state_forbids_names_the_state_rather_than_advising_a_retry() {
    // The compare-and-swap's whole point: `complete` expects to leave
    // `in_progress`, and this row is still `scheduled`.
    //
    // **And nothing here races.** The row never moved, so the predicate can never
    // match and a retry is the one thing that cannot help — which is what an
    // unconditional `ConcurrentMutation` on zero rows advises. So the refusal
    // re-reads and names the state the row actually holds, as the two sibling
    // compare-and-swaps in this slice do.
    let provider = harness().await;
    let conn = provider.conn().expect("conn");
    let id = Uuid::now_v7();
    migration_repo::insert_or_load(&conn, &scope(), new_migration(id))
        .await
        .expect("schedule");

    let err = migration_repo::complete(&conn, &scope(), TENANT, id, json!({}), at(13))
        .await
        .expect_err("a scheduled run has not started");

    match &err {
        RepoError::MigrationStateForbidden {
            state, attempted, ..
        } => {
            assert_eq!(
                state, "scheduled",
                "it names the state the row holds: {err:?}"
            );
            assert!(
                attempted.contains("in_progress") && attempted.contains("completed"),
                "and the edge that was refused: {err:?}"
            );
        }
        other => panic!("expected a state refusal naming `scheduled`, got {other:?}"),
    }

    let still = migration_repo::load(&conn, &scope(), TENANT, id)
        .await
        .expect("load")
        .expect("present");
    assert_eq!(still.state, MigrationState::Scheduled);
}

// ---------------------------------------------------------------------------
// The retirement guard's input.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn only_live_schedules_are_reported_as_targeting_a_plan() {
    // A completed or cancelled run moves nobody, so it must not refuse a
    // retirement. The narrowing is the store's, not the caller's.
    let provider = harness().await;
    let conn = provider.conn().expect("conn");
    let target = PlanId::new(Uuid::from_u128(0x_7a_46_e7));

    let live = Uuid::now_v7();
    migration_repo::insert_or_load(&conn, &scope(), new_migration(live))
        .await
        .expect("live");

    let done = Uuid::now_v7();
    migration_repo::insert_or_load(&conn, &scope(), new_migration(done))
        .await
        .expect("done");
    migration_repo::start(&conn, &scope(), TENANT, done, json!({}), at(11))
        .await
        .expect("start");
    migration_repo::complete(&conn, &scope(), TENANT, done, json!({}), at(13))
        .await
        .expect("complete");

    let targeting = migration_repo::live_targeting(&conn, &scope(), TENANT, target)
        .await
        .expect("live targeting");

    assert_eq!(targeting.len(), 1);
    assert_eq!(targeting[0].migration_id, live);
}

// ---------------------------------------------------------------------------
// The layer below the repository — the guards a raw statement meets.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_store_refuses_a_delete_because_cancel_is_a_state() {
    // An executor re-reading the schedule must be able to tell a cancelled run
    // from an absent one, so absence and cancellation must not be one answer.
    let conn = migrated_db().await;
    let id = Uuid::now_v7();
    seeded_schedule(&conn, id, "scheduled").await;

    let err = exec(
        &conn,
        &format!("DELETE FROM pricing_migration WHERE migration_id = '{id}'"),
    )
    .await
    .expect_err("DELETE must be refused");

    assert!(
        err.to_string()
            .contains("cancel is a state, not a deletion"),
        "{err}"
    );
}

#[tokio::test]
async fn the_store_refuses_an_unsanctioned_edge_the_repository_never_offers() {
    // §4 has no `scheduled -> completed`: a run cannot be reported processed by
    // an executor that never declared it started, because the D-36 re-validation
    // and D-65's exclusion set both hang off that declaration.
    //
    // **Two guards can answer this move, and this case isolates the trigger**
    // (D-301). It used to assert `contains("not a sanctioned one") ||
    // contains("started_required")` over a statement that violated both, so
    // *either* guard could be deleted with it still green — and it was the only
    // reference to `chk_pricing_migration_started_required` anywhere outside the
    // migration census, which made the one Slice-11 constraint that looked
    // covered the one covered by an assertion surviving its own removal. The
    // statement below carries `started_at`, so the CHECK is satisfied and only the
    // edge list can refuse it.
    let conn = migrated_db().await;
    let id = Uuid::now_v7();
    seeded_schedule(&conn, id, "scheduled").await;

    let err = exec(
        &conn,
        &format!(
            "UPDATE pricing_migration SET state = 'completed', \
             started_at = '2026-08-07T12:00:00+00:00', \
             completed_at = '2026-08-07T13:00:00+00:00' WHERE migration_id = '{id}'"
        ),
    )
    .await
    .expect_err("scheduled -> completed must be refused");

    let message = err.to_string();
    assert!(
        message.contains("not a sanctioned one"),
        "the edge list is what answers a move no edge sanctions: {err}"
    );
    // **The isolation is asserted, not assumed.** If the CHECK answered this
    // statement the case would be back to covering two guards with one sentence,
    // which is the defect it was split out of.
    assert!(
        !message.contains("started_required"),
        "and the CHECK did not answer first: {err}"
    );
}

#[tokio::test]
async fn a_run_cannot_enter_in_progress_without_declaring_when_it_started() {
    // The other half of the split above (D-301), isolating
    // `chk_pricing_migration_started_required`. `scheduled -> in_progress` **is**
    // a sanctioned edge, so the trigger passes it through and only the CHECK can
    // refuse — which is what makes this the one statement that proves the CHECK
    // still refuses anything.
    //
    // The constraint is two implications rather than a biconditional because
    // `cancelled` is reachable from `scheduled` as well as from `in_progress`, so
    // a cancelled run may legitimately carry no start.
    let conn = migrated_db().await;
    let id = Uuid::now_v7();
    seeded_schedule(&conn, id, "scheduled").await;

    let err = exec(
        &conn,
        &format!("UPDATE pricing_migration SET state = 'in_progress' WHERE migration_id = '{id}'"),
    )
    .await
    .expect_err("in_progress without a start instant must be refused");

    let message = err.to_string();
    assert!(
        message.contains("started_required"),
        "and the CHECK is what answers it, by name: {err}"
    );
    assert!(
        !message.contains("not a sanctioned one"),
        "on a sanctioned edge the trigger has nothing to say: {err}"
    );
}

#[tokio::test]
async fn the_store_freezes_the_schedule_time_delta_report() {
    // §6 calls it the deltas "at schedule time": it is the evidence of what the
    // operator confirmed against, so D-36's execution-time re-resolution lands in
    // `exclusion_snapshot` instead and cannot overwrite it.
    let conn = migrated_db().await;
    let id = Uuid::now_v7();
    seeded_schedule(&conn, id, "scheduled").await;

    let err = exec(
        &conn,
        &format!(
            "UPDATE pricing_migration SET delta_report = '{{\"locked\":[\"sub-9\"]}}' \
             WHERE migration_id = '{id}'"
        ),
    )
    .await
    .expect_err("the schedule-time report is frozen");

    assert!(
        err.to_string()
            .contains("bound to its source, target, effective date, scope"),
        "{err}"
    );
}

#[tokio::test]
async fn the_store_refuses_a_second_exclusion_set_on_a_started_run() {
    // D-65's replay half, guarded below the repository: a recompute could differ
    // from the set the executor already honoured.
    let conn = migrated_db().await;
    let id = Uuid::now_v7();
    seeded_schedule(&conn, id, "in_progress").await;

    let err = exec(
        &conn,
        &format!(
            "UPDATE pricing_migration SET exclusion_snapshot = '{{\"locked\":[\"sub-9\"]}}' \
             WHERE migration_id = '{id}'"
        ),
    )
    .await
    .expect_err("the exclusion set is replayed, never recomputed");

    assert!(
        err.to_string()
            .contains("computed once and replayed verbatim"),
        "{err}"
    );
}

#[tokio::test]
async fn a_terminal_run_is_immutable_history() {
    let conn = migrated_db().await;
    let id = Uuid::now_v7();
    seeded_schedule(&conn, id, "cancelled").await;

    let err = exec(
        &conn,
        &format!(
            "UPDATE pricing_migration SET completion_record = '{{}}' WHERE migration_id = '{id}'"
        ),
    )
    .await
    .expect_err("a cancelled run is history");

    assert!(
        err.to_string()
            .contains("completed or cancelled run is immutable history"),
        "{err}"
    );
}

// ---------------------------------------------------------------------------
// Helpers.
// ---------------------------------------------------------------------------

/// One schedule seeded through raw SQL, for the cases that drive statements
/// **past** the repository — the layer that cannot see a guard stop refusing.
async fn seeded_schedule(conn: &sea_orm::DatabaseConnection, id: Uuid, state: &str) {
    let started = if state == "scheduled" {
        ("NULL".to_owned(), "NULL".to_owned())
    } else {
        (
            "'2026-08-07T11:00:00+00:00'".to_owned(),
            "'{\"locked\":[]}'".to_owned(),
        )
    };
    let cancelled = if state == "cancelled" {
        "'2026-08-07T12:00:00+00:00'"
    } else {
        "NULL"
    };
    let completed = if state == "completed" {
        "'2026-08-07T13:00:00+00:00'"
    } else {
        "NULL"
    };
    common::must_succeed(
        conn,
        &format!(
            "INSERT INTO pricing_migration (
                 migration_id, tenant_id, source_plan_id, source_revision, target_plan_id,
                 effective_at, announced_at, scope, state, delta_report,
                 exclusion_snapshot, completion_record, created_by, created_at,
                 started_at, completed_at, cancelled_at)
             VALUES ('{id}', '{TENANT}', '{SOURCE}', 0, '{TARGET}',
                 '2026-11-05T10:00:00+00:00', '2026-08-07T10:00:00+00:00',
                 '{{\"kind\":\"all\"}}', '{state}', '{{\"locked\":[]}}',
                 {}, NULL, '{ACTOR}', '2026-08-07T10:00:00+00:00',
                 {}, {completed}, {cancelled})",
            started.1, started.0
        ),
    )
    .await;
}

/// How many rows the table holds under one `migration_id`.
///
/// Narrowed by the primary key `(tenant_id, migration_id)` and by nothing else,
/// which is what makes it a count of the row rather than of a view over it. The
/// obvious spelling — `live_targeting(..).filter(id).count()` — narrows further,
/// to the live states and to one target plan, so the retry landing a second row
/// in a terminal state or under another plan would read as absent.
///
/// Two is unreachable while that primary key stands, so the only value the
/// assertion below can take other than 1 is **0** — and that is what it buys:
/// the retry did not consume the row it was supposed to find. Written as a count
/// rather than a presence check so the day the key is widened it still reads.
async fn count_rows(conn: &impl toolkit_db::secure::DBRunner, id: Uuid) -> usize {
    use bss_pricing::infra::storage::entity::migration;
    use sea_orm::{ColumnTrait, Condition, EntityTrait};
    use toolkit_db::secure::SecureEntityExt;

    migration::Entity::find()
        .secure()
        .scope_with(&scope())
        .filter(
            Condition::all()
                .add(migration::Column::TenantId.eq(TENANT))
                .add(migration::Column::MigrationId.eq(id)),
        )
        .all(conn)
        .await
        .expect("read the migration rows")
        .len()
}

// ---------------------------------------------------------------------------
// The collection read: `GET /bss-pricing/v1/migrations`
// ---------------------------------------------------------------------------

/// The page is a keyset walk on `migration_id` and visits every schedule once.
///
/// The ids are minted with [`Uuid::from_u128`] rather than `now_v7` so the order
/// the walk must produce is a fact this test knows rather than one it reads back
/// off the store it is testing.
#[tokio::test]
async fn the_migration_page_walks_by_migration_id_and_never_repeats_one() {
    let provider = harness().await;
    let conn = provider.conn().expect("conn");
    for n in 0..3_u128 {
        migration_repo::insert_or_load(
            &conn,
            &scope(),
            new_migration(Uuid::from_u128(0x_31_00 + n)),
        )
        .await
        .expect("schedule");
    }

    let mut walked: Vec<Uuid> = Vec::new();
    let mut after = None;
    // **Bounded, not `loop`.** The defect this case is written against is a
    // `list_page` that ignores `after`, and such a page is never empty — so an
    // unbounded walk spins forever and grows `walked` without limit instead of
    // failing. Measured 2026-08-20: pinning `after` to `None` here left this test
    // running past 600 seconds until the runner killed it, which is a CI timeout
    // with no assertion message rather than a regression report. Three rows at two
    // per page is two pages and then the empty one; eight is slack.
    let mut drained = false;
    for _ in 0..8 {
        let page = migration_repo::list_page(&conn, &scope(), TENANT, &[], after, 2)
            .await
            .expect("list");
        assert!(page.len() <= 2, "the page is bounded by the limit it asked");
        let Some(last) = page.last() else {
            drained = true;
            break;
        };
        after = Some(last.migration_id);
        walked.extend(page.iter().map(|record| record.migration_id));
    }
    assert!(
        drained,
        "the page never advanced: eight pages of two and the walk still had rows, which is what \
         a keyset read that ignored `after` looks like ({walked:?})"
    );

    assert_eq!(
        walked,
        vec![
            Uuid::from_u128(0x_31_00),
            Uuid::from_u128(0x_31_01),
            Uuid::from_u128(0x_31_02),
        ],
        "every schedule once, in migration_id order"
    );
}

/// `state` narrows the page, and an empty filter is every state rather than none.
///
/// The asserted state is `cancelled`, which is **not** the state a schedule is
/// born in: a query that dropped the condition would return all three and a query
/// that hardcoded the default would return the wrong one, so neither passes by
/// coincidence.
#[tokio::test]
async fn the_migration_page_narrows_to_the_named_state() {
    let provider = harness().await;
    let conn = provider.conn().expect("conn");
    for n in 0..3_u128 {
        migration_repo::insert_or_load(
            &conn,
            &scope(),
            new_migration(Uuid::from_u128(0x_32_00 + n)),
        )
        .await
        .expect("schedule");
    }
    migration_repo::cancel(
        &conn,
        &scope(),
        TENANT,
        Uuid::from_u128(0x_32_01),
        MigrationState::Scheduled,
        None,
        at(11),
    )
    .await
    .expect("cancel the middle one");

    let cancelled = migration_repo::list_page(
        &conn,
        &scope(),
        TENANT,
        &[MigrationState::Cancelled],
        None,
        100,
    )
    .await
    .expect("list");
    assert_eq!(
        cancelled
            .iter()
            .map(|record| record.migration_id)
            .collect::<Vec<_>>(),
        vec![Uuid::from_u128(0x_32_01)],
        "only the cancelled schedule"
    );

    let everything = migration_repo::list_page(&conn, &scope(), TENANT, &[], None, 100)
        .await
        .expect("list");
    assert_eq!(
        everything.len(),
        3,
        "an empty filter is every state, not an empty page"
    );
}

/// **The column default writes the format the ordering CHECKs compare against.**
///
/// `created_at` is `text` on this engine and the three ordering constraints compare
/// it lexicographically against `started_at`, `completed_at` and `cancelled_at` —
/// values every writer renders as RFC 3339. `CURRENT_TIMESTAMP` renders
/// `YYYY-MM-DD HH:MM:SS`, and a space sorts below `T`, so a defaulted `created_at`
/// is below every same-day RFC 3339 instant whatever the clock said: the guard stops
/// refusing rather than starts. Postgres has neither problem, its column being
/// `timestamptz`.
///
/// Latent today — every writer supplies the column — which is exactly why it needs a
/// case rather than a reader noticing.
#[tokio::test]
async fn a_defaulted_created_at_still_binds_the_ordering_checks() {
    let conn = migrated_db().await;
    let id = Uuid::now_v7();

    // No `created_at`: the default is what lands.
    common::must_succeed(
        &conn,
        &format!(
            "INSERT INTO pricing_migration (
                 migration_id, tenant_id, source_plan_id, source_revision, target_plan_id,
                 effective_at, announced_at, scope, state, delta_report, created_by)
             VALUES ('{id}', '{TENANT}', '{SOURCE}', 0, '{TARGET}',
                 '2099-11-05T10:00:00+00:00', '2099-08-07T10:00:00+00:00',
                 '{{\"kind\":\"all\"}}', 'scheduled', '{{\"locked\":[]}}', '{ACTOR}')"
        ),
    )
    .await;

    // A start earlier the **same day**, which is the whole of the window: across
    // different dates the leading `YYYY-MM-DD` decides the comparison and the two
    // renderings agree. On one date position ten is a space against a `T`, the space
    // is the lower byte, and so every RFC 3339 instant of that day sorts above a
    // defaulted `created_at` — including the ones that precede it.
    let err = exec(
        &conn,
        &format!(
            "UPDATE pricing_migration
                SET started_at = date('now') || 'T00:00:00+00:00',
                    exclusion_snapshot = '{{\"locked\":[]}}',
                    state = 'in_progress'
              WHERE migration_id = '{id}'"
        ),
    )
    .await
    .expect_err("a start before creation is what chk_pricing_migration_started_order refuses");
    assert!(
        err.to_string().contains("started_order"),
        "the ordering constraint has to be what refuses it: {err}"
    );

    // And the stored rendering is the writers', not merely *a* sortable one: the
    // comparison is lexicographic, so two renderings of the same instant that differ
    // in any byte order the column wrongly.
    let stored = common::scalar(
        &conn,
        &format!("SELECT created_at AS v FROM pricing_migration WHERE migration_id = '{id}'"),
    )
    .await;
    assert!(
        stored.ends_with("+00:00") && stored.as_bytes()[10] == b'T',
        "the default has to render what every writer renders: {stored}"
    );
}
