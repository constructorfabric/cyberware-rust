//! Two sweeps over one due window, on real Postgres: one flip, one event.
//!
//! # Why this cannot be a `SQLite` suite, and why it cannot be two `run` calls
//!
//! `sqlite::memory:` **serializes writers**, so the interleaving under test can
//! be neither confirmed nor refuted there.
//! `tests/sqlite_window_activation.rs` proves that a *second* pass over the same
//! instant finds nothing due — which is the sequential half, and it is a
//! statement about the due read. What it cannot show is what happens when two
//! passes are **in flight at once**, both having read the same `scheduled` row
//! before either wrote: the case a multi-replica deployment produces every time
//! the coordination lease is lost or taken over, and the only case in which the
//! outbox dedup key is doing anything at all.
//!
//! Calling `run` twice would prove nothing about it. The second call's read
//! happens after the first call's commit, so it is the sequential test again with
//! extra steps.
//!
//! # The choreography, and why each step is there
//!
//! The idiom of `tests/postgres_approval_race.rs` and
//! `tests/postgres_audit_chain.rs`, driven by observable events only:
//!
//! 1. a hand-rolled transaction performs exactly what one sweep's flip does —
//!    the `transition` and the `enqueue` — and then **parks**, holding the
//!    window row's lock and the aggregate's `seq` uncommitted;
//! 2. a real [`WindowActivationJob::run`] starts. Its due read sees the still
//!    committed `scheduled` row, and its `UPDATE` blocks on that lock;
//! 3. a third connection **observes the block** in `pg_locks`, narrowed to
//!    `current_database()` because one server carries every test's database at
//!    once — which is what proves the sweep's read already happened;
//! 4. only then is the parked transaction released to commit, and the sweep's
//!    `UPDATE` re-evaluates `state = 'scheduled' AND effective_from <= at` under
//!    READ COMMITTED, matches nothing, and resolves into a no-op flip whose
//!    event the dedup key refuses.
//!
//! Step 3 is the load-bearing one. Without it the sweep could read the *flipped*
//! row, find nothing due, and report zero — green, and about nothing.
//!
//! # What "one flip and one event" rests on, precisely
//!
//! Not the lease: the lease is what makes this rare, not what makes it safe, and
//! a lease can be lost. Two things carry it, and the suite asserts both:
//!
//! - the **transition predicate** in the `UPDATE`'s `WHERE`, which makes the
//!   loser's flip match zero rows rather than re-stamping `activated_at`;
//! - the **dedup key** `PriceWindowActivated/<window_id>`, which makes the loser's
//!   event the same event and is refused by *either* of the two constraints it
//!   reaches — `uq_pricing_outbox_dedup_key` or the `outbox_id` primary key derived
//!   from the same pair. The suite asserts one row, not which index said no; the
//!   driver's error class cannot tell them apart (`contention_or_db` says so).
//!
//! **What this suite does not prove**, said here rather than left to the reader:
//! that the flip and the enqueue sharing one transaction is what keeps them
//! together. In this race the loser has nothing to roll back on the window side —
//! its `UPDATE` matched zero rows and `transition` answers `Ok` on the self-edge —
//! so what is demonstrated is *one flip, one event*, and the transaction is doing no
//! work in the demonstration. The property the transaction carries is a **committed
//! flip whose enqueue then fails**, and reaching it needs an injected enqueue
//! failure this suite has no way to produce; it holds by the shape of the code, one
//! `in_transaction` closure containing both writes.
//!
//! Ignored by default; they need Docker. Run with
//! `cargo test -p cf-gears-bss-pricing --test postgres_window_activation -- --ignored`.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod pg_support;

use std::sync::Arc;
use std::time::Duration;

use bss_pricing::config::JobsConfig;
use bss_pricing::domain::audit::AuditStamp;
use bss_pricing::domain::events::CatalogEvent;
use bss_pricing::domain::instant::utc_ymd_hms;
use bss_pricing::domain::scope_key::PlanId;
use bss_pricing::domain::window::WindowState;
use bss_pricing::infra::jobs::window_activation::WindowActivationJob;
use bss_pricing::infra::storage::RepoError;
use bss_pricing::infra::storage::entity::{outbox, price};
use bss_pricing::infra::storage::repo::window_repo::{self, NewWindow};
use bss_pricing::infra::storage::repo::{
    NewOutboxEvent, PriceWindowTransitionPayload, outbox_repo,
};
use time::OffsetDateTime;

use pg_support::Pg;
use sea_orm::ActiveValue::Set;
use sea_orm::{ColumnTrait, Condition, EntityTrait};
use tokio::sync::Notify;
use toolkit_db::secure::{AccessScope, SecureEntityExt, SecureInsertExt};
use toolkit_db::{DBProvider, DbError};
use uuid::Uuid;

const TENANT: Uuid = Uuid::from_u128(0x7e_11);
const ACTOR: Uuid = Uuid::from_u128(0xac_01);
const PLAN: Uuid = Uuid::from_u128(0x91_a1);
const PHASE: Uuid = Uuid::from_u128(0x40_a5);
const ROW: Uuid = Uuid::from_u128(0xa0_01);
const WINDOW: Uuid = Uuid::from_u128(0xa1_01);

/// One value for a whole test binary: this suite drives a job and a repository
/// directly, where the value the HTTP edge would have established has no
/// producer.
const TEST_CORRELATION: Uuid = Uuid::from_u128(0x_c0_11_a7_10);

/// Generous, because a cold container under load is slow — but **finite**: a
/// racer that never resolves is a refuted claim, not a slow one.
const RACE_TIMEOUT: Duration = Duration::from_secs(30);

/// `2099-09-<day>T00:00:00Z` — a fact rather than a fixture that ages, in
/// `tests/sqlite_window_repo.rs`'s sense.
fn t(day: u32) -> OffsetDateTime {
    utc_ymd_hms(2099, 9, day, 0, 0, 0)
}

fn scope() -> AccessScope {
    AccessScope::for_tenant(TENANT)
}

fn stamp() -> AuditStamp {
    AuditStamp {
        actor_principal_id: ACTOR,
        recorded_at: t(1),
        correlation_id: TEST_CORRELATION,
    }
}

/// A published price row and one `scheduled` window over `[t(10), t(20))`.
async fn seed(pg: &Pg) {
    let db = pg.db().await;
    let provider = DBProvider::<DbError>::new(db);
    let conn = provider.conn().expect("scoped connection");
    let row = price::ActiveModel {
        price_id: Set(ROW),
        tenant_id: Set(TENANT),
        plan_id: Set(PLAN),
        currency: Set("USD".to_owned()),
        region: Set("EU".to_owned()),
        phase: Set(PHASE),
        charge_kind: Set("recurring".to_owned()),
        amount_minor: Set(Some(1_000)),
        model_kind: Set(Some("flat".to_owned())),
        lifecycle_state: Set("published".to_owned()),
        created_by: Set(ACTOR),
        created_at_utc: Set(t(1)),
        ..price::ActiveModel::default()
    };
    price::Entity::insert(row.clone())
        .secure()
        .scope_with_model(&scope(), &row)
        .expect("scope the seeded price row")
        .exec(&conn)
        .await
        .expect("seed the price row");
    window_repo::schedule(
        &conn,
        &scope(),
        NewWindow {
            window_id: WINDOW,
            tenant_id: TENANT,
            price_id: ROW,
            effective_from: t(10),
            effective_to: Some(t(20)),
            reason_code: "priceIncrease".to_owned(),
        },
        stamp(),
    )
    .await
    .expect("schedule the window");
}

/// The window's state and the instant it took effect at.
async fn window_at_rest(pg: &Pg) -> (WindowState, Option<OffsetDateTime>) {
    let db = pg.db().await;
    let provider = DBProvider::<DbError>::new(db);
    let conn = provider.conn().expect("scoped connection");
    let record = window_repo::find(&conn, &scope(), TENANT, WINDOW)
        .await
        .expect("read the window")
        .expect("it is there");
    (record.state, record.activated_at)
}

/// Every outbox row of the tenant.
async fn events(pg: &Pg) -> Vec<outbox::Model> {
    let db = pg.db().await;
    let provider = DBProvider::<DbError>::new(db);
    let conn = provider.conn().expect("scoped connection");
    outbox::Entity::find()
        .secure()
        .scope_with(&scope())
        .filter(Condition::all().add(outbox::Column::TenantId.eq(TENANT)))
        .all(&conn)
        .await
        .expect("read the outbox")
}

fn activation_payload() -> PriceWindowTransitionPayload {
    PriceWindowTransitionPayload {
        window_id: WINDOW,
        plan_id: PlanId::new(PLAN),
        price_id: ROW,
        effective_from: t(10),
        effective_to: Some(t(20)),
        correlation_id: TEST_CORRELATION,
    }
}

/// The world every assertion below is read against: on Postgres, with real
/// `timestamptz` columns, one pass flips the due window and emits its event.
///
/// It is not a duplicate of the mirror suite's first test. That one proves the
/// due predicate against `text` instants; this one proves it against the typed
/// column the production backend has, which is the comparison a `text` mirror
/// can never exercise.
#[tokio::test]
#[ignore = "needs docker"]
async fn one_pass_flips_a_due_window_on_postgres() {
    let pg = Pg::applied().await;
    seed(&pg).await;

    let job = WindowActivationJob::new(
        DBProvider::<DbError>::new(pg.db().await),
        JobsConfig::default(),
    );
    let report = job.run(t(10)).await.expect("the pass runs");

    assert_eq!(report.windows_due, 1);
    assert_eq!(report.activated, 1);
    assert_eq!(report.failed, 0);
    assert_eq!(
        window_at_rest(&pg).await,
        (WindowState::Active, Some(t(10)))
    );
    let rows = events(&pg).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].event_name,
        CatalogEvent::PriceWindowActivated.as_str()
    );
}

/// Two sweeps in flight over one due window: **one** flip, **one** event.
///
/// The loser is the interesting side, and what it must not do is count. It
/// reports the window as due and the flip as failed, having performed neither —
/// which is what makes a lease takeover safe without the lease being the thing
/// that makes it safe.
#[tokio::test]
#[ignore = "needs docker"]
async fn two_sweeps_in_flight_flip_a_window_once_and_emit_one_event() {
    let pg = Pg::applied().await;
    seed(&pg).await;
    let observer = pg.raw().await;

    let written = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());

    // The winner: exactly the two statements one sweep's flip makes, then a
    // park. Hand-rolled rather than a `run` call because the pass has to stop
    // *between* its write and its commit, and no job exposes that seam - nor
    // should it.
    let winner = {
        let db = pg.db().await;
        let (written, release) = (Arc::clone(&written), Arc::clone(&release));
        tokio::spawn(async move {
            let (_db, out) = db
                .in_transaction::<(), RepoError, _>(move |txn| {
                    Box::pin(async move {
                        window_repo::transition(
                            txn,
                            &scope(),
                            TENANT,
                            WINDOW,
                            WindowState::Active,
                            t(10),
                            stamp(),
                        )
                        .await?;
                        outbox_repo::enqueue(
                            txn,
                            &scope(),
                            NewOutboxEvent::price_window_activated(
                                TENANT,
                                &activation_payload(),
                                t(10),
                            ),
                        )
                        .await?;
                        // The flip and its event are written and uncommitted:
                        // the row lock is held and the dedup key is taken.
                        written.notify_one();
                        release.notified().await;
                        Ok(())
                    })
                })
                .await;
            out
        })
    };

    written.notified().await;

    let loser = {
        let db = pg.db().await;
        tokio::spawn(async move {
            WindowActivationJob::new(DBProvider::<DbError>::new(db), JobsConfig::default())
                .run(t(10))
                .await
        })
    };

    // The sweep's due read has happened and its UPDATE is waiting on the lock.
    // Only now is the winner allowed to commit.
    pg_support::wait_until_a_backend_blocks(&observer).await;
    release.notify_one();

    tokio::time::timeout(RACE_TIMEOUT, winner)
        .await
        .expect("the winner must finish once released")
        .expect("its task must not panic")
        .expect("the winner is uncontended and must commit");

    let report = tokio::time::timeout(RACE_TIMEOUT, loser)
        .await
        .expect("the sweep must be released by the winner's commit")
        .expect("its task must not panic")
        .expect("a lost race is not a failed pass: the sweep returns a report");

    assert_eq!(
        report.windows_due, 1,
        "the loser did read the window as due - that is what makes this a race"
    );
    assert_eq!(
        report.activated, 0,
        "and it must not count a flip it did not perform"
    );
    assert_eq!(report.failed, 1);

    assert_eq!(
        window_at_rest(&pg).await,
        (WindowState::Active, Some(t(10))),
        "one flip: the instant the price took effect was written once"
    );
    let rows = events(&pg).await;
    assert_eq!(
        rows.len(),
        1,
        "one event: the dedup key is what refused the second, not the lease"
    );
    assert_eq!(
        rows[0].event_name,
        CatalogEvent::PriceWindowActivated.as_str()
    );
    assert_eq!(rows[0].seq, 0, "and it is the aggregate's first");
}
