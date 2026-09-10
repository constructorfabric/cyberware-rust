//! The window repository against the executed `SQLite` mirror.
//!
//! What makes this a database suite rather than a unit test is the half of each
//! operation a unit test cannot have: the row read back is the row the store
//! actually holds, the foreign scope is refused in SQL rather than by a `WHERE`
//! clause this file wrote, and the canonical scope key on every record is
//! resolved out of `pricing_price` rather than restated here.
//!
//! # This is where non-overlap is proved, and it is proved nowhere else
//!
//! §6 puts non-overlap per canonical scope key "inside every mutation" because no
//! constraint can reach it: the key is ten columns of `pricing_price` and none of
//! them is on the window row. So the rule has no schema object, `tests/postgres_window.rs`
//! cannot execute a statement that trips it, and this suite carries the whole
//! proof.
//!
//! **The statement only non-overlap can refuse** is
//! [`an_overlapping_window_is_refused`]'s: two windows on one key at `[t1, t3)` and
//! `[t2, t4)` with `t1 < t2 < t3 < t4`. That shape is chosen and not incidental —
//! see the test's own doc. Every refusal test here first puts the world in the
//! state where the object under test is what answers.
//!
//! # Why the price rows are seeded through their entity rather than authored
//!
//! `pricing_price` rows are written with `price::Entity::insert` rather than through
//! `PriceRepo::create_draft`, and deliberately: this suite needs a `superseded`
//! predecessor and a `published` successor **on one canonical scope key**, which is
//! the state a supersession leaves behind and which no authoring path in this gear
//! can currently produce. Composing it out of the writers that exist would mean
//! asserting the window rules against a world those writers can build rather than
//! against the world §6 is about. The insert still goes through the tenant gate, so
//! the seeding cannot fabricate a row the scope would not admit.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use bss_pricing::domain::audit::AuditStamp;
use bss_pricing::domain::scope_key::PlanId;
use bss_pricing::domain::window::WindowState;
use bss_pricing::infra::storage::RepoError;
use bss_pricing::infra::storage::entity::price;
use bss_pricing::infra::storage::migrations::Migrator;
use bss_pricing::infra::storage::repo::window_repo::{self, NewWindow, WindowRecord};

use bss_pricing::domain::instant::format_rfc3339;
use bss_pricing::domain::instant::{truncate_millis, utc_ymd_hms};
use sea_orm::ActiveValue::Set;
use sea_orm::EntityTrait;
use sea_orm_migration::MigratorTrait;
use time::OffsetDateTime;
use toolkit_db::migration_runner::run_migrations_for_testing;
use toolkit_db::secure::{AccessScope, SecureInsertExt};
use toolkit_db::{ConnectOpts, DBProvider, DbError, connect_db};
use uuid::Uuid;

const TENANT: Uuid = Uuid::from_u128(0x7e_11);
const OTHER_TENANT: Uuid = Uuid::from_u128(0x7e_22);
const ACTOR: Uuid = Uuid::from_u128(0xac_01);
const PLAN: Uuid = Uuid::from_u128(0x91_a1);
const PHASE: Uuid = Uuid::from_u128(0x40_a5);

/// The `published` row every test schedules against.
const ROW: Uuid = Uuid::from_u128(0xa0_01);
/// A `superseded` predecessor **on the same canonical scope key** as [`ROW`].
const PREDECESSOR: Uuid = Uuid::from_u128(0xa0_02);
/// A row on a **different** key — a different `charge_kind`, so the ten axes
/// differ in exactly one place.
const OTHER_KEY_ROW: Uuid = Uuid::from_u128(0xa0_03);

/// One value for the whole binary: this suite drives the repository directly,
/// where the value the HTTP edge would have established has no producer.
const TEST_CORRELATION: Uuid = Uuid::from_u128(0x_c0_11_a7_10);

fn stamp_at(when: OffsetDateTime) -> AuditStamp {
    AuditStamp {
        actor_principal_id: ACTOR,
        recorded_at: when,
        correlation_id: TEST_CORRELATION,
    }
}

/// `2099-09-<day>T00:00:00Z`. Days rather than hours so that every instant in a
/// test is legibly ordered.
///
/// **The year is 2099 and that is load-bearing**, which `tests/postgres_window.rs`
/// said out loud and this suite did not inherit: the mirror's fifth trigger arm and
/// [`window_repo::adjust_effective_to`]'s pre-check both compare against a clock, so
/// "future" here has to be a **fact rather than a fixture that ages**. With the
/// 2026-09 base these instants carried until 2026-08-04, this suite began failing
/// from 2026-09-15 — six weeks out — and every later group builds on this helper.
fn t(day: u32) -> OffsetDateTime {
    utc_ymd_hms(2099, 9, day, 0, 0, 0)
}

async fn harness() -> DBProvider<DbError> {
    let db = connect_db("sqlite::memory:", ConnectOpts::default())
        .await
        .expect("connect in-memory sqlite");
    run_migrations_for_testing(&db, Migrator::migrations())
        .await
        .expect("run migrator");
    let provider = DBProvider::<DbError>::new(db);
    seed_price_rows(&provider).await;
    provider
}

/// Three price rows: two on one canonical scope key, one on another.
///
/// The pair on one key is the shape the per-key half of non-overlap is about, and
/// it is legal: `uq_pricing_price_scope_key_current` forbids two **published** rows
/// on a key, and a `superseded` predecessor beside a `published` successor is
/// exactly what a supersession leaves.
async fn seed_price_rows(provider: &DBProvider<DbError>) {
    for (id, state, charge_kind) in [
        (ROW, "published", "recurring"),
        (PREDECESSOR, "superseded", "recurring"),
        (OTHER_KEY_ROW, "published", "one_time"),
    ] {
        seed_price_row(provider, TENANT, id, state, charge_kind).await;
    }
}

/// One price row, written through its entity under the tenant that owns it.
///
/// Every column the migration defaults is left `NotSet`, so the defaults are what
/// is exercised rather than values this file restates.
async fn seed_price_row(
    provider: &DBProvider<DbError>,
    tenant_id: Uuid,
    price_id: Uuid,
    lifecycle_state: &str,
    charge_kind: &str,
) {
    let conn = provider.conn().expect("scoped connection");
    let scope = AccessScope::for_tenant(tenant_id);
    let row = price::ActiveModel {
        price_id: Set(price_id),
        tenant_id: Set(tenant_id),
        plan_id: Set(PLAN),
        currency: Set("USD".to_owned()),
        region: Set("EU".to_owned()),
        phase: Set(PHASE),
        charge_kind: Set(charge_kind.to_owned()),
        amount_minor: Set(Some(1_000)),
        model_kind: Set(Some("flat".to_owned())),
        lifecycle_state: Set(lifecycle_state.to_owned()),
        created_by: Set(ACTOR),
        created_at_utc: Set(t(1)),
        ..price::ActiveModel::default()
    };
    price::Entity::insert(row.clone())
        .secure()
        .scope_with_model(&scope, &row)
        .expect("scope the seeded price row")
        .exec(&conn)
        .await
        .unwrap_or_else(|e| panic!("seed price row {price_id}: {e}"));
}

fn scope() -> AccessScope {
    AccessScope::for_tenant(TENANT)
}

/// A window on [`ROW`] over `[from, to)`.
fn window(id: u128, from: OffsetDateTime, to: Option<OffsetDateTime>) -> NewWindow {
    on_row(id, ROW, from, to)
}

fn on_row(id: u128, price_id: Uuid, from: OffsetDateTime, to: Option<OffsetDateTime>) -> NewWindow {
    NewWindow {
        window_id: Uuid::from_u128(id),
        tenant_id: TENANT,
        price_id,
        effective_from: from,
        effective_to: to,
        reason_code: "priceIncrease".to_owned(),
    }
}

/// Schedule one window that must land.
async fn must_schedule(provider: &DBProvider<DbError>, new: NewWindow) -> WindowRecord {
    let conn = provider.conn().expect("scoped connection");
    let id = new.window_id;
    window_repo::schedule(&conn, &scope(), new, stamp_at(t(1)))
        .await
        .unwrap_or_else(|e| panic!("window {id} must be schedulable: {e}"))
}

// ---------------------------------------------------------------------------
// The world: what the store accepts
// ---------------------------------------------------------------------------

/// The world in which every refusal below is observable. Without it a suite of
/// refusals would pass against a repository that writes nothing at all.
///
/// It also pins the two things the record carries that no column does: the
/// canonical scope key, resolved out of `pricing_price`, and the stamp's actor and
/// instant landing in `created_by` / `created_at`.
#[tokio::test]
async fn a_scheduled_window_reads_back_as_it_was_written_carrying_its_rows_key() {
    let provider = harness().await;
    let written = must_schedule(&provider, window(0x01, t(10), Some(t(20)))).await;

    let conn = provider.conn().expect("scoped connection");
    let read = window_repo::find(&conn, &scope(), TENANT, Uuid::from_u128(0x01))
        .await
        .expect("read")
        .expect("the window is there");

    assert_eq!(read, written, "the read must be the row that was written");
    assert_eq!(
        read.state,
        WindowState::Scheduled,
        "a window is born scheduled"
    );
    assert_eq!(read.effective_from, t(10));
    assert_eq!(read.effective_to, Some(t(20)));
    assert_eq!(read.created_by, ACTOR, "the actor comes off the stamp");
    assert_eq!(read.created_at, t(1), "and so does the instant");
    assert_eq!(read.activated_at, None);
    assert_eq!(read.expired_at, None);
    assert_eq!(read.cancelled_at, None);
    // The key is the price row's ten axes, not anything this file spelled.
    assert_eq!(read.scope_key.plan_id(), PlanId::new(PLAN));
    assert_eq!(read.scope_key.currency().as_str(), "USD");
}

/// An open-ended window is a value and not a missing bound (`inst-ws-expire`).
///
/// Read back out of the store. `window_repo::schedule` builds its `WindowRecord`
/// from the `NewWindow` it was handed, so the returned value carries the `None`
/// this test passed in whatever the column holds — including a column with a
/// `NOT NULL` default that quietly filled it.
#[tokio::test]
async fn an_open_ended_window_is_storable() {
    let provider = harness().await;
    let written = must_schedule(&provider, window(0x02, t(10), None)).await;

    let conn = provider.conn().expect("scoped connection");
    let read = window_repo::find(&conn, &scope(), TENANT, written.window_id)
        .await
        .expect("read the window back")
        .expect("the window is there");
    assert_eq!(read.effective_to, None);
}

/// **§9's named false positive.** `effectiveTo = next.effectiveFrom` is adjacency,
/// so both windows land — and a coverage checker built on an off-by-one comparison
/// here would report a gap at the instant the key is in fact covered.
#[tokio::test]
async fn adjacent_windows_on_one_key_are_both_storable() {
    let provider = harness().await;
    must_schedule(&provider, window(0x03, t(10), Some(t(20)))).await;
    must_schedule(&provider, window(0x04, t(20), Some(t(30)))).await;

    let conn = provider.conn().expect("scoped connection");
    let all = window_repo::list_for_plan(&conn, &scope(), TENANT, PlanId::new(PLAN))
        .await
        .expect("list");
    assert_eq!(all.len(), 2, "both windows are on the key");
    assert_eq!(all[0].effective_from, t(10), "oldest interval first");
    assert_eq!(all[1].effective_from, t(20));
}

/// Two rows on **different** keys may hold the same interval — that is the whole
/// point of the rule being per key. A check that compared intervals per plan, or
/// per tenant, would refuse a plan from pricing two charge kinds over one period.
#[tokio::test]
async fn one_interval_may_be_held_on_two_different_keys_at_once() {
    let provider = harness().await;
    must_schedule(&provider, window(0x05, t(10), Some(t(20)))).await;
    must_schedule(&provider, on_row(0x06, OTHER_KEY_ROW, t(10), Some(t(20)))).await;
}

// ---------------------------------------------------------------------------
// Non-overlap per canonical scope key
// ---------------------------------------------------------------------------

/// **The statement only non-overlap can refuse**, and the shape is deliberate.
///
/// Two windows on one canonical scope key at `[t1, t3)` and `[t2, t4)` with
/// `t1 < t2 < t3 < t4`. Read as coverage, this key is **fully covered from `t1` to
/// `t4`**: there is no interior gap and no uncovered instant between the two
/// intervals. So the coverage rule, the future-gap rule and the trailing-void rule
/// — each of which will run inside this same mutation once the later groups land —
/// have nothing to object to, and the *only* thing that refuses the second
/// schedule is the overlap over `[t2, t3)`.
///
/// A later reader "simplifying" this into a disjoint pair, or into a pair that
/// leaves the key uncovered, would get a test that stays red for a reason having
/// nothing to do with the rule it names — and the proof would quietly stop proving.
#[tokio::test]
async fn an_overlapping_window_is_refused() {
    let provider = harness().await;
    // t1 = t(10), t2 = t(15), t3 = t(20), t4 = t(25).
    must_schedule(&provider, window(0x07, t(10), Some(t(20)))).await;

    let conn = provider.conn().expect("scoped connection");
    let refusal = window_repo::schedule(
        &conn,
        &scope(),
        window(0x08, t(15), Some(t(25))),
        stamp_at(t(1)),
    )
    .await
    .expect_err("the overlap over [t2, t3) must be refused");

    // All three fields, because all three are on the wire: the code alone tells an
    // operator that *something* collided, and only `key` says on which of the plan's
    // canonical scope keys, only `requested` echoes the interval they composed, and
    // only `conflicting` names the window they have to act on. Two of the three were
    // unasserted, so a rendering that dropped or transposed them stayed green.
    match &refusal {
        RepoError::WindowOverlap {
            key,
            requested,
            conflicting,
        } => {
            assert!(
                key.contains("USD") && key.contains("recurring"),
                "the refusal must name the canonical scope key the collision is on, \
                 not the price row; got {key}"
            );
            assert!(
                requested.contains(&format_rfc3339(t(15)))
                    && requested.contains(&format_rfc3339(t(25))),
                "the refusal must echo the interval that was asked for, both ends; \
                 got {requested}"
            );
            assert!(
                conflicting.contains(&Uuid::from_u128(0x07).to_string())
                    && conflicting.contains(&format_rfc3339(t(10))),
                "the refusal must name the window that was already there and the \
                 interval it holds, which is the pair an operator can act on; got \
                 {conflicting}"
            );
        }
        other => panic!("the refusal must be the overlap, got {other:?}"),
    }
    // And the losing window did not land: the check runs before the insert, so
    // nothing has to be undone on a table whose rows cannot be deleted.
    assert!(
        window_repo::find(&conn, &scope(), TENANT, Uuid::from_u128(0x08))
            .await
            .expect("read")
            .is_none()
    );
}

/// The **per-key** half, which a per-row check would miss entirely.
///
/// The two windows are on two different price rows that share one canonical scope
/// key — a `superseded` predecessor and its `published` successor, which is the
/// state every supersession leaves behind. A non-overlap check keyed on `price_id`
/// would let their intervals overlap, and the key would then have two prices
/// covering one instant with nothing anywhere refusing it.
#[tokio::test]
async fn two_windows_on_one_key_but_different_rows_may_not_overlap() {
    let provider = harness().await;
    must_schedule(&provider, on_row(0x09, PREDECESSOR, t(10), Some(t(20)))).await;

    let conn = provider.conn().expect("scoped connection");
    let refusal = window_repo::schedule(
        &conn,
        &scope(),
        on_row(0x0a, ROW, t(15), Some(t(25))),
        stamp_at(t(1)),
    )
    .await
    .expect_err("one key, two rows, one overlapping instant");
    assert!(
        matches!(refusal, RepoError::WindowOverlap { .. }),
        "{refusal:?}"
    );
}

/// A cancelled window does not occupy its key. Without this a cancelled schedule
/// would block the corrected one forever — and the table forbids deleting it.
#[tokio::test]
async fn a_cancelled_window_no_longer_occupies_its_key() {
    let provider = harness().await;
    must_schedule(&provider, window(0x0b, t(10), Some(t(20)))).await;
    let conn = provider.conn().expect("scoped connection");
    window_repo::transition(
        &conn,
        &scope(),
        TENANT,
        Uuid::from_u128(0x0b),
        WindowState::Cancelled,
        t(2),
        stamp_at(t(2)),
    )
    .await
    .expect("a scheduled window cancels");

    // The same interval, again, on the same key.
    must_schedule(&provider, window(0x0c, t(10), Some(t(20)))).await;
}

/// The overlap check is scoped, and this is the half a `WHERE tenant_id = …` in
/// the wrong place would silently lose: another tenant's window on the same
/// interval must not refuse this tenant's schedule.
#[tokio::test]
async fn another_tenants_window_does_not_block_a_schedule() {
    let provider = harness().await;
    // The other tenant's own row and window, on identical axes.
    seed_price_row(
        &provider,
        OTHER_TENANT,
        Uuid::from_u128(0xb0_01),
        "published",
        "recurring",
    )
    .await;
    let conn = provider.conn().expect("scoped connection");
    window_repo::schedule(
        &conn,
        &AccessScope::for_tenant(OTHER_TENANT),
        NewWindow {
            window_id: Uuid::from_u128(0xb0_02),
            tenant_id: OTHER_TENANT,
            price_id: Uuid::from_u128(0xb0_01),
            effective_from: t(10),
            effective_to: Some(t(20)),
            reason_code: "priceIncrease".to_owned(),
        },
        stamp_at(t(1)),
    )
    .await
    .expect("the other tenant schedules its own window");

    // Identical interval, this tenant, same axes: nothing is in the way.
    must_schedule(&provider, window(0x0d, t(10), Some(t(20)))).await;
}

/// A foreign scope sees absence, not a forbidden window — the existence leak the
/// SQL-level scoping closes. What a window would otherwise confirm is that a
/// competitor has a price change scheduled.
#[tokio::test]
async fn a_foreign_scope_finds_nothing() {
    let provider = harness().await;
    must_schedule(&provider, window(0x0e, t(10), Some(t(20)))).await;
    let conn = provider.conn().expect("scoped connection");
    assert!(
        window_repo::find(
            &conn,
            &AccessScope::for_tenant(OTHER_TENANT),
            OTHER_TENANT,
            Uuid::from_u128(0x0e)
        )
        .await
        .expect("read")
        .is_none()
    );
}

/// A window is bound to a row that exists, and the refusal is the scope gate's
/// rather than the foreign key's: an unknown `price_id` and another tenant's row
/// are the same answer, so no caller can probe for rows by scheduling against them.
#[tokio::test]
async fn a_window_on_a_row_that_is_not_there_is_refused_before_the_foreign_key() {
    let provider = harness().await;
    let conn = provider.conn().expect("scoped connection");
    let refusal = window_repo::schedule(
        &conn,
        &scope(),
        on_row(0x0f, Uuid::from_u128(0xdead), t(10), Some(t(20))),
        stamp_at(t(1)),
    )
    .await
    .expect_err("no such price row in scope");
    match refusal {
        RepoError::NotFound { subject, .. } => assert_eq!(subject, "price"),
        other => panic!("expected the scope gate's absence, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Transitions
// ---------------------------------------------------------------------------

/// Two of the three edges §4 sanctions, each landing its own timestamp — and the
/// expiry keeping the `activated_at` it was given, which
/// `chk_pricing_price_window_activated_at` requires of it. The third,
/// `scheduled -> cancelled`, stamps `cancelled_at` under
/// [`a_second_cancellation_is_refused_rather_than_answered_ok`], which walks the
/// edge before refusing the second attempt.
#[tokio::test]
async fn the_sanctioned_flips_land_and_stamp_their_own_column() {
    let provider = harness().await;
    must_schedule(&provider, window(0x10, t(10), Some(t(20)))).await;
    let conn = provider.conn().expect("scoped connection");
    let id = Uuid::from_u128(0x10);

    let active = window_repo::transition(
        &conn,
        &scope(),
        TENANT,
        id,
        WindowState::Active,
        t(10),
        stamp_at(t(10)),
    )
    .await
    .expect("scheduled -> active");
    assert_eq!(active.state, WindowState::Active);
    assert_eq!(active.activated_at, Some(t(10)));

    let expired = window_repo::transition(
        &conn,
        &scope(),
        TENANT,
        id,
        WindowState::Expired,
        t(20),
        stamp_at(t(20)),
    )
    .await
    .expect("active -> expired");
    assert_eq!(expired.state, WindowState::Expired);
    assert_eq!(expired.expired_at, Some(t(20)));
    assert_eq!(
        expired.activated_at,
        Some(t(10)),
        "an expired window keeps the instant it took effect"
    );

    // And the store holds what was returned, rather than the caller's arithmetic.
    let read = window_repo::find(&conn, &scope(), TENANT, id)
        .await
        .expect("read")
        .expect("there");
    assert_eq!(read, expired);
}

/// A **second cancellation** is refused rather than answered `Ok`.
///
/// `domain::window::check_cancellation` refuses a cancelled window "and not as an
/// idempotent no-op … a second cancellation is a second publish unit (D-99) …
/// answering `Ok` would let that unit run over a window that never changes", and
/// until 2026-08-04 this store answered `Ok` to exactly that. G4's
/// `DELETE …/price-windows/{windowId}` reaching `transition(.., Cancelled, ..)` on
/// an already-cancelled window would have answered **202** and run a publish unit,
/// with its own `CatalogVersion` request, over a window that cannot change.
///
/// # The world, and the statement only this narrowing can refuse
///
/// The window is genuinely `cancelled` — walked there through §4's own edge, not
/// written — so `scheduled -> cancelled` has already succeeded once and the request
/// is a repeat rather than a mistake. No neighbour refuses it: the interval is
/// intact, the instant is the same one the first cancellation used, and the
/// cancellation edge carries no §4 condition at all (`edge_condition` renders
/// nothing for it), so the *only* thing left to answer is whether arriving at a
/// state the row already holds is a no-op. It is not, for `cancelled`, because no
/// clock decides a cancellation.
///
/// The refusal is `WindowStateForbidden` and not a window code, which is the store's
/// own line: the wire code an operator sees on the DELETE path is
/// `WINDOW_NOT_CANCELLABLE`, and it stays the domain check's to raise.
#[tokio::test]
async fn a_second_cancellation_is_refused_rather_than_answered_ok() {
    let provider = harness().await;
    must_schedule(&provider, window(0x1c, t(10), Some(t(20)))).await;
    let conn = provider.conn().expect("scoped connection");
    let id = Uuid::from_u128(0x1c);

    let cancelled = window_repo::transition(
        &conn,
        &scope(),
        TENANT,
        id,
        WindowState::Cancelled,
        t(2),
        stamp_at(t(2)),
    )
    .await
    .expect("section 4's third edge");
    assert_eq!(cancelled.state, WindowState::Cancelled);

    let refusal = window_repo::transition(
        &conn,
        &scope(),
        TENANT,
        id,
        WindowState::Cancelled,
        t(2),
        stamp_at(t(2)),
    )
    .await
    .expect_err("a second cancellation is a second publish unit, not a no-op");
    match refusal {
        RepoError::WindowStateForbidden {
            state, attempted, ..
        } => {
            assert_eq!(state, "cancelled");
            assert!(attempted.contains("cancelled"), "got: {attempted}");
        }
        other => panic!("expected the refused self-edge, got {other:?}"),
    }

    // And nothing moved: the instant the window was unscheduled is written once.
    let read = window_repo::find(&conn, &scope(), TENANT, id)
        .await
        .expect("read")
        .expect("there");
    assert_eq!(read.cancelled_at, Some(t(2)));
}

/// A transition to the state the window is already in is a **no-op that succeeds**,
/// because the activation job re-drives batches it may have already flipped under a
/// lease it lost. It is not a widening of the machine: the original
/// `activated_at` is not re-stamped, so the instant the price took effect is
/// written once and a re-driven sweep cannot move it.
#[tokio::test]
async fn a_flip_to_the_state_the_window_already_holds_is_idempotent() {
    let provider = harness().await;
    must_schedule(&provider, window(0x11, t(10), Some(t(20)))).await;
    let conn = provider.conn().expect("scoped connection");
    let id = Uuid::from_u128(0x11);
    for at in [t(10), t(11)] {
        let flipped = window_repo::transition(
            &conn,
            &scope(),
            TENANT,
            id,
            WindowState::Active,
            at,
            stamp_at(at),
        )
        .await
        .expect("the re-drive must not fail");
        assert_eq!(flipped.state, WindowState::Active);
        assert_eq!(
            flipped.activated_at,
            Some(t(10)),
            "the second sweep must not re-stamp when the price took effect"
        );
    }
}

/// An unsanctioned edge is refused **by the repository**, naming the state the
/// window is actually in — not by the trigger, which would reach a caller as a 500.
///
/// The world matters: the window is `scheduled`, so the terminal-history arm has
/// nothing to say and the edge itself is what refuses.
#[tokio::test]
async fn an_unsanctioned_edge_is_refused_with_the_windows_actual_state() {
    let provider = harness().await;
    must_schedule(&provider, window(0x12, t(10), Some(t(20)))).await;
    let conn = provider.conn().expect("scoped connection");
    let refusal = window_repo::transition(
        &conn,
        &scope(),
        TENANT,
        Uuid::from_u128(0x12),
        WindowState::Expired,
        t(20),
        stamp_at(t(20)),
    )
    .await
    .expect_err("a scheduled window does not expire");
    match refusal {
        RepoError::WindowStateForbidden { state, .. } => assert_eq!(state, "scheduled"),
        other => panic!("expected the state refusal, got {other:?}"),
    }
}

/// **`inst-ws-activate`'s condition, through the sanctioned writer.** A window does
/// not activate before its own `effectiveFrom`, and the refusal names the boundary.
///
/// The world is what makes this the condition's test and not the edge set's: the
/// window is `scheduled` and `scheduled -> active` **is** one of §4's edges, so
/// `may_move_to` has nothing to object to and the only thing wrong is that `at` has
/// not reached `effectiveFrom`. Until 2026-08-04 this returned
/// `Ok((Active, effective_from = t(10), activated_at = t(2)))` — the "activation that
/// never happened" the migration's own INSERT-guard note cites, reachable through the
/// writer the routes and the activation job both use.
#[tokio::test]
async fn a_window_does_not_activate_before_its_effective_from() {
    let provider = harness().await;
    must_schedule(&provider, window(0x21, t(10), Some(t(20)))).await;
    let conn = provider.conn().expect("scoped connection");
    let id = Uuid::from_u128(0x21);

    let refusal = window_repo::transition(
        &conn,
        &scope(),
        TENANT,
        id,
        WindowState::Active,
        t(2),
        stamp_at(t(2)),
    )
    .await
    .expect_err("a window whose start has not arrived does not activate");
    match refusal {
        RepoError::WindowStateForbidden {
            state, attempted, ..
        } => {
            assert_eq!(state, "scheduled");
            assert!(
                attempted.contains(&format_rfc3339(t(10))),
                "the refusal must name the start that has not arrived, because the \
                 remedy is to wait until it has: {attempted}"
            );
        }
        other => panic!("expected the state refusal, got {other:?}"),
    }
    // And nothing moved — the condition is in the statement's `WHERE` as well, so
    // there is no window in which a row was flipped and then reported as refused.
    let read = window_repo::find(&conn, &scope(), TENANT, id)
        .await
        .expect("read")
        .expect("there");
    assert_eq!(read.state, WindowState::Scheduled);
    assert_eq!(read.activated_at, None);
}

/// **`inst-ws-expire`'s condition**, the same rule one edge over: an `active` window
/// does not expire before its own `effectiveTo`.
///
/// Staged on an `active` window so the terminal arm is silent and `active -> expired`
/// is a sanctioned edge, which leaves the instant as the only fault.
#[tokio::test]
async fn an_active_window_does_not_expire_before_its_effective_to() {
    let provider = harness().await;
    must_schedule(&provider, window(0x22, t(10), Some(t(20)))).await;
    let conn = provider.conn().expect("scoped connection");
    let id = Uuid::from_u128(0x22);
    window_repo::transition(
        &conn,
        &scope(),
        TENANT,
        id,
        WindowState::Active,
        t(10),
        stamp_at(t(10)),
    )
    .await
    .expect("scheduled -> active");

    let refusal = window_repo::transition(
        &conn,
        &scope(),
        TENANT,
        id,
        WindowState::Expired,
        t(15),
        stamp_at(t(15)),
    )
    .await
    .expect_err("the end has not arrived");
    match refusal {
        RepoError::WindowStateForbidden {
            state, attempted, ..
        } => {
            assert_eq!(state, "active");
            assert!(
                attempted.contains(&format_rfc3339(t(20))),
                "the refusal must name the end that has not arrived: {attempted}"
            );
        }
        other => panic!("expected the state refusal, got {other:?}"),
    }
}

/// **`inst-ws-expire`, verbatim: "an open-ended window never expires."**
///
/// There is no instant at which this flip becomes legal, which is what makes the
/// refusal's own sentence different from the two above — "wait" is the remedy there
/// and there is no remedy here. Until 2026-08-04 this returned
/// `Ok((Expired, effective_to = None, expired_at = t(30)))`: a key whose coverage the
/// store said had stopped at an instant the row does not carry, with no fallback
/// pricing anywhere downstream to stop at.
#[tokio::test]
async fn an_open_ended_window_never_expires() {
    let provider = harness().await;
    must_schedule(&provider, window(0x23, t(10), None)).await;
    let conn = provider.conn().expect("scoped connection");
    let id = Uuid::from_u128(0x23);
    window_repo::transition(
        &conn,
        &scope(),
        TENANT,
        id,
        WindowState::Active,
        t(10),
        stamp_at(t(10)),
    )
    .await
    .expect("scheduled -> active");

    let refusal = window_repo::transition(
        &conn,
        &scope(),
        TENANT,
        id,
        WindowState::Expired,
        t(30),
        stamp_at(t(30)),
    )
    .await
    .expect_err("an open-ended window has no end to have arrived");
    match refusal {
        RepoError::WindowStateForbidden { attempted, .. } => assert!(
            attempted.contains("open-ended"),
            "the refusal must say the window has no end at all, because 'wait' is \
             not the remedy here and there is no remedy: {attempted}"
        ),
        other => panic!("expected the state refusal, got {other:?}"),
    }
}

/// **A flip instant carries no D-144 quantum**, because it is not an authored one.
///
/// §5 scopes the millisecond quantum to `effectiveFrom`/`effectiveTo`, the cutover
/// instant, the D-88 changeover instant and `grandfatherUntil` — every one of them a
/// value an operator wrote on a request. A transition's `at` is machine-generated:
/// the activation sweep's `OffsetDateTime::now_utc()`, which carries sub-millisecond precision on
/// every platform this runs on. Until 2026-08-04 [`window_repo::transition`] ran the
/// authoring check over it, so **every flip an activation sweep would ever attempt**
/// failed with `TIMESTAMP_PRECISION_EXCEEDED`, and no test in 1118 saw it.
///
/// The instant here is one microsecond past the window's start, which is both finer
/// than the quantum and past `effectiveFrom`, so the flip's own condition holds and
/// the quantum is the only thing that could refuse it.
#[tokio::test]
async fn a_flip_instant_finer_than_the_quantum_is_not_refused() {
    let provider = harness().await;
    must_schedule(&provider, window(0x24, t(10), Some(t(20)))).await;
    let conn = provider.conn().expect("scoped connection");
    let id = Uuid::from_u128(0x24);
    let sweep_instant = t(10) + time::Duration::microseconds(1);

    let flipped = window_repo::transition(
        &conn,
        &scope(),
        TENANT,
        id,
        WindowState::Active,
        sweep_instant,
        stamp_at(sweep_instant),
    )
    .await
    .expect("a machine flip instant is not an authored one");
    assert_eq!(flipped.state, WindowState::Active);
    assert_eq!(
        flipped.activated_at,
        Some(truncate_millis(sweep_instant)),
        "the flip lands; the stamp is persisted at the millisecond quantum"
    );
    // **Read back from the store.** SeaORM's `time` TEXT form writes a
    // millisecond bound as `…00.000Z`. A later machine instant with extra
    // fractional digits (`…00.000001Z`) then compares *smaller* as SQLite TEXT,
    // so `chk_pricing_price_window_activation_order` (`activated_at >=
    // effective_from`) would refuse a same-millisecond sweep. The stamp is
    // therefore stored at the bound's quantum. The flip itself is what this
    // case is named for: D-144 does not apply to a machine instant.
    let stored = window_repo::find(&conn, &scope(), TENANT, flipped.window_id)
        .await
        .expect("read the flipped window")
        .expect("it exists");
    assert_eq!(stored.state, WindowState::Active);
    assert_eq!(
        stored.activated_at,
        Some(truncate_millis(sweep_instant)),
        "the stored stamp is the quantum the activation-order CHECK can compare"
    );
    // And `schedule` still refuses the same precision on an authored instant, which
    // is what says the quantum was scoped rather than removed.
    let refusal = window_repo::schedule(
        &conn,
        &scope(),
        window(0x25, sweep_instant, Some(t(30))),
        stamp_at(t(1)),
    )
    .await
    .expect_err("an authored start below the quantum is still refused");
    assert!(
        matches!(refusal, RepoError::TimestampPrecisionExceeded { .. }),
        "{refusal:?}"
    );
}

/// `inst-ws-cancel`: an **active** window is never cancelled. Staged against an
/// active window on purpose — against a scheduled one this edge is legal, and
/// against a cancelled one the terminal arm would answer first.
#[tokio::test]
async fn cancelling_an_active_window_is_refused_by_the_machine() {
    let provider = harness().await;
    must_schedule(&provider, window(0x13, t(10), Some(t(20)))).await;
    let conn = provider.conn().expect("scoped connection");
    let id = Uuid::from_u128(0x13);
    window_repo::transition(
        &conn,
        &scope(),
        TENANT,
        id,
        WindowState::Active,
        t(10),
        stamp_at(t(10)),
    )
    .await
    .expect("scheduled -> active");

    let refusal = window_repo::transition(
        &conn,
        &scope(),
        TENANT,
        id,
        WindowState::Cancelled,
        t(11),
        stamp_at(t(11)),
    )
    .await
    .expect_err("an active window is never cancelled");
    match refusal {
        RepoError::WindowStateForbidden { state, .. } => assert_eq!(state, "active"),
        other => panic!("expected the state refusal, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Moving the end
// ---------------------------------------------------------------------------

/// A shorten and an extend of a **future** end, both legal (`inst-ws-immutable`),
/// and an extension to open-ended with them.
#[tokio::test]
async fn a_future_end_may_be_shortened_extended_and_opened() {
    let provider = harness().await;
    must_schedule(&provider, window(0x14, t(10), Some(t(20)))).await;
    let conn = provider.conn().expect("scoped connection");
    let id = Uuid::from_u128(0x14);

    // Three acts in sequence, so each asserts the number the one before it left
    // (D-190): the act sequence is the window's, not the request's.
    for (act, target) in [Some(t(15)), Some(t(25)), None].into_iter().enumerate() {
        let moved = window_repo::adjust_effective_to(
            &conn,
            &scope(),
            TENANT,
            id,
            target,
            act as u64,
            stamp_at(t(2)),
        )
        .await
        .unwrap_or_else(|e| panic!("moving the end to {target:?} must be legal: {e}"));
        assert_eq!(moved.effective_to, target);
        let read = window_repo::find(&conn, &scope(), TENANT, id)
            .await
            .expect("read")
            .expect("there");
        assert_eq!(read.effective_to, target, "and the store holds it");
    }
}

/// **An extension is a new claim on the key**, so the overlap check runs again.
/// Without it a shorten-then-extend would walk one window straight through its
/// successor, and the key would carry two prices over the overlap with nothing
/// refusing it.
#[tokio::test]
async fn extending_a_window_into_its_successor_is_refused() {
    let provider = harness().await;
    must_schedule(&provider, window(0x15, t(10), Some(t(20)))).await;
    must_schedule(&provider, window(0x16, t(20), Some(t(30)))).await;
    let conn = provider.conn().expect("scoped connection");

    let refusal = window_repo::adjust_effective_to(
        &conn,
        &scope(),
        TENANT,
        Uuid::from_u128(0x15),
        Some(t(25)),
        0,
        stamp_at(t(2)),
    )
    .await
    .expect_err("the extension reaches into the successor's interval");
    assert!(
        matches!(refusal, RepoError::WindowOverlap { .. }),
        "{refusal:?}"
    );
}

/// And the adjustment does **not** collide with the window it is replacing. If the
/// overlap check did not exclude the subject, every single adjustment would be
/// refused — which is a guard that refuses its own legal case.
#[tokio::test]
async fn an_adjustment_does_not_collide_with_the_window_being_adjusted() {
    let provider = harness().await;
    must_schedule(&provider, window(0x17, t(10), Some(t(20)))).await;
    let conn = provider.conn().expect("scoped connection");
    window_repo::adjust_effective_to(
        &conn,
        &scope(),
        TENANT,
        Uuid::from_u128(0x17),
        Some(t(19)),
        0,
        stamp_at(t(2)),
    )
    .await
    .expect("shortening must not collide with itself");
}

/// `inst-ws-immutable`: an expired or cancelled window is immutable history, and
/// the repository says so rather than letting the trigger answer with a 500.
///
/// **The refusal is `WindowHistorical` and not `WindowStateForbidden`**, which it
/// was until 2026-08-04. §5 gives this ground its own code —
/// `WINDOW_HISTORICAL_IMMUTABLE` (409) — and the variant it used to carry renders
/// `LIFECYCLE_FORBIDDEN` (400), D-146's "no alternative action". There is an
/// alternative here and the refusal now names it: schedule a new window.
#[tokio::test]
async fn a_terminal_windows_end_cannot_be_moved() {
    let provider = harness().await;
    must_schedule(&provider, window(0x18, t(10), Some(t(20)))).await;
    let conn = provider.conn().expect("scoped connection");
    let id = Uuid::from_u128(0x18);
    window_repo::transition(
        &conn,
        &scope(),
        TENANT,
        id,
        WindowState::Cancelled,
        t(2),
        stamp_at(t(2)),
    )
    .await
    .expect("scheduled -> cancelled");

    let refusal = window_repo::adjust_effective_to(
        &conn,
        &scope(),
        TENANT,
        id,
        Some(t(30)),
        1,
        stamp_at(t(3)),
    )
    .await
    .expect_err("a cancelled window is history");
    match refusal {
        RepoError::WindowHistorical { frozen, .. } => assert!(
            frozen.contains("cancelled"),
            "the refusal must say WHICH history it is, because 'the window was \
             cancelled' and 'the window already expired' send an operator to \
             different places; got {frozen}"
        ),
        other => panic!("expected the historical-immutability refusal, got {other:?}"),
    }
}

/// `inst-ws-immutable`'s second ground: an end that has **already passed** cannot
/// be moved, and not even forward.
///
/// **This is the statement only this pre-check can refuse**, and staging it is the
/// whole test. The window is `active` — so the terminal arm has nothing to say — its
/// start is behind the stamp's clock and its end is too, which is the state a
/// window sits in whenever the activation job is behind its SLO. The move is
/// *forward*, to an instant comfortably in the future, so a check that only asked
/// "is the target future?" would admit it. What makes it illegal is that the end
/// being moved is the instant a price stopped applying: moving it re-opens a window
/// that already closed and rewrites what was chargeable.
///
/// Before this pre-check the statement was refused by
/// `trg_pricing_price_window_append_only`'s fifth arm and reached the caller as a
/// **500**.
#[tokio::test]
async fn an_end_that_has_already_passed_cannot_be_moved_forward() {
    let provider = harness().await;
    must_schedule(&provider, window(0x1c, t(10), Some(t(20)))).await;
    let conn = provider.conn().expect("scoped connection");
    let id = Uuid::from_u128(0x1c);
    window_repo::transition(
        &conn,
        &scope(),
        TENANT,
        id,
        WindowState::Active,
        t(10),
        stamp_at(t(10)),
    )
    .await
    .expect("scheduled -> active");

    // The world: `active`, started, and its end passed five days ago by the
    // stamp's clock.
    let refusal = window_repo::adjust_effective_to(
        &conn,
        &scope(),
        TENANT,
        id,
        Some(t(28)),
        0,
        stamp_at(t(25)),
    )
    .await
    .expect_err("a passed end is history whichever way it is moved");
    match refusal {
        RepoError::WindowHistorical { frozen, .. } => assert!(
            frozen.contains(&format_rfc3339(t(20))),
            "the refusal must name the end that had already passed: {frozen}"
        ),
        other => panic!("expected the historical-immutability refusal, got {other:?}"),
    }
    // And nothing moved.
    let read = window_repo::find(&conn, &scope(), TENANT, id)
        .await
        .expect("read")
        .expect("there");
    assert_eq!(read.effective_to, Some(t(20)));
}

/// `inst-ws-immutable`'s third ground: an end may only be moved **to** a future
/// instant.
///
/// Staged against a window whose stored end is still ahead — so the second ground
/// has nothing to say — leaving the target instant as the only thing that can
/// refuse the move. That is the shorten a cutover composes, one day too late.
#[tokio::test]
async fn an_end_may_not_be_moved_to_an_instant_that_has_passed() {
    let provider = harness().await;
    must_schedule(&provider, window(0x1d, t(10), Some(t(30)))).await;
    let conn = provider.conn().expect("scoped connection");
    let id = Uuid::from_u128(0x1d);

    let refusal = window_repo::adjust_effective_to(
        &conn,
        &scope(),
        TENANT,
        id,
        Some(t(19)),
        0,
        stamp_at(t(20)),
    )
    .await
    .expect_err("a shorten into the past is a retroactive reprice");
    match refusal {
        RepoError::WindowHistorical { frozen, .. } => assert!(
            frozen.contains(&format_rfc3339(t(19))),
            "the refusal must name the target an author has to correct: {frozen}"
        ),
        other => panic!("expected the historical-immutability refusal, got {other:?}"),
    }
    // The same shorten, one instant the other side of the clock, is legal — which
    // is what says the refusal above is about the clock rather than about
    // shortening.
    window_repo::adjust_effective_to(&conn, &scope(), TENANT, id, Some(t(21)), 0, stamp_at(t(20)))
        .await
        .expect("a shorten to a future instant is the permitted mutation");
}

/// An interval whose end is not strictly after its start is refused by the
/// repository, naming the interval — not by `chk_pricing_price_window_interval`,
/// which reached the caller as a 500.
///
/// Both shapes, because they are one rule: `effective_to = effective_from` is not
/// a zero-length window, and an inverted pair is not a window read backwards.
#[tokio::test]
async fn an_empty_or_inverted_interval_is_refused_before_the_check_constraint() {
    let provider = harness().await;
    let conn = provider.conn().expect("scoped connection");

    for (id, from, to) in [(0x1e, t(10), t(10)), (0x1f, t(10), t(9))] {
        let refusal =
            window_repo::schedule(&conn, &scope(), window(id, from, Some(to)), stamp_at(t(1)))
                .await
                .expect_err("an interval that is not an interval");
        match refusal {
            RepoError::WindowIntervalEmpty { requested } => assert!(
                requested.contains(&format_rfc3339(to)),
                "the refusal must render the interval so an author can see which \
                 end to move: {requested}"
            ),
            other => panic!("expected the empty-interval refusal, got {other:?}"),
        }
    }
}

/// The same rule on the adjustment path: a shorten cannot walk the end past the
/// start it is measured from.
///
/// Staged with a **future** target that is nonetheless before the window's start,
/// so neither of `inst-ws-immutable`'s clock rules has anything to say and the
/// emptiness of the resulting interval is the only thing left to refuse it.
#[tokio::test]
async fn an_adjustment_may_not_leave_an_empty_interval() {
    let provider = harness().await;
    must_schedule(&provider, window(0x20, t(20), Some(t(30)))).await;
    let conn = provider.conn().expect("scoped connection");

    let refusal = window_repo::adjust_effective_to(
        &conn,
        &scope(),
        TENANT,
        Uuid::from_u128(0x20),
        Some(t(15)),
        0,
        stamp_at(t(10)),
    )
    .await
    .expect_err("an end before the start is not an interval");
    assert!(
        matches!(refusal, RepoError::WindowIntervalEmpty { .. }),
        "{refusal:?}"
    );
}

/// D-144's quantum, at the boundary the store writes through. `timestamptz` holds
/// microseconds and `SQLite` holds whatever text it is handed, so a finer instant
/// persists in silence and is then compared for **equality** against a `cohort`
/// another gear produced.
#[tokio::test]
async fn an_instant_finer_than_the_quantum_is_refused_at_the_boundary() {
    let provider = harness().await;
    let conn = provider.conn().expect("scoped connection");
    let sub_millisecond = t(10) + time::Duration::microseconds(1);
    let refusal = window_repo::schedule(
        &conn,
        &scope(),
        window(0x19, sub_millisecond, Some(t(20))),
        stamp_at(t(1)),
    )
    .await
    .expect_err("a sub-millisecond start is refused");
    match refusal {
        RepoError::TimestampPrecisionExceeded { field, .. } => assert_eq!(field, "effectiveFrom"),
        other => panic!("expected the precision refusal, got {other:?}"),
    }
}

/// The plan-wide read includes the terminal windows, because it is the store's read
/// and not the projection: the operator's coverage report has to be able to say
/// that a key lost its successor to a cancellation.
#[tokio::test]
async fn the_plan_read_carries_the_cancelled_windows_too() {
    let provider = harness().await;
    must_schedule(&provider, window(0x1a, t(10), Some(t(20)))).await;
    must_schedule(&provider, on_row(0x1b, OTHER_KEY_ROW, t(10), Some(t(20)))).await;
    let conn = provider.conn().expect("scoped connection");
    window_repo::transition(
        &conn,
        &scope(),
        TENANT,
        Uuid::from_u128(0x1a),
        WindowState::Cancelled,
        t(2),
        stamp_at(t(2)),
    )
    .await
    .expect("cancel");

    let all = window_repo::list_for_plan(&conn, &scope(), TENANT, PlanId::new(PLAN))
        .await
        .expect("list");
    assert_eq!(all.len(), 2);
    assert!(all.iter().any(|w| w.state == WindowState::Cancelled));
    // Each record carries its own row's key, and the two rows are on different
    // keys — so the read is not resolving one key for the whole plan.
    assert_ne!(all[0].scope_key, all[1].scope_key);
}

/// A plan nobody scheduled a window on reads empty rather than failing, and a plan
/// with no price rows at all reads empty too — the second is the shape that would
/// otherwise send an `IN ()` to the driver.
#[tokio::test]
async fn a_plan_with_no_windows_and_a_plan_with_no_rows_both_read_empty() {
    let provider = harness().await;
    let conn = provider.conn().expect("scoped connection");
    assert!(
        window_repo::list_for_plan(&conn, &scope(), TENANT, PlanId::new(PLAN))
            .await
            .expect("list")
            .is_empty()
    );
    assert!(
        window_repo::list_for_plan(
            &conn,
            &scope(),
            TENANT,
            PlanId::new(Uuid::from_u128(0xffff))
        )
        .await
        .expect("list")
        .is_empty()
    );
}

// ---------------------------------------------------------------------------
// The act sequence (D-190) and the precondition it makes comparable (D-191)
// ---------------------------------------------------------------------------

/// **The counter D-190 asks for, and the two halves of what it is for.**
///
/// A window is born at act `0` and every operator act on it advances the number by
/// exactly one. That is what makes an act nameable: the act token `<op>/<seq>/…` is
/// distinct across two acts *and* stable across a genuine retry of one, because a
/// retry of an act that has not committed reads the same sequence the first attempt
/// read.
#[tokio::test]
async fn a_window_is_born_at_act_zero_and_an_adjustment_advances_it() {
    let provider = harness().await;
    let written = must_schedule(&provider, window(0x_5e_01, t(10), Some(t(20)))).await;
    assert_eq!(
        written.mutation_seq, 0,
        "a window has been acted on once - its own schedule - and that is act zero"
    );

    let conn = provider.conn().expect("scoped connection");
    let adjusted = window_repo::adjust_effective_to(
        &conn,
        &scope(),
        TENANT,
        Uuid::from_u128(0x_5e_01),
        Some(t(25)),
        /* expected_seq */ 0,
        stamp_at(t(1)),
    )
    .await
    .expect("a lengthening against the current sequence lands");
    assert_eq!(adjusted.mutation_seq, 1, "the act advanced the sequence");

    let read = window_repo::find(&conn, &scope(), TENANT, Uuid::from_u128(0x_5e_01))
        .await
        .expect("read")
        .expect("there");
    assert_eq!(
        read.mutation_seq, 1,
        "and the store holds it, rather than the caller's arithmetic"
    );
}

/// **The sweep does not advance it, and this is the test that keeps D-184 closed.**
///
/// The sequence counts *acts*, not row writes. If the activation sweep advanced it,
/// a window that activated between a refused act and its approved retry would make
/// the retry render a different act token — so the retry would name a subject no
/// unit was opened under, find nothing, and open a second unit. That is exactly the
/// approval loop with no exit that D-184 closed, reopened through the clock instead
/// of through the window id.
///
/// So the two clock-driven edges of §4 leave the number alone and the one
/// operator-driven edge advances it, which is `inst-ws-cancel` against
/// `inst-ws-activate` / `inst-ws-expire`.
#[tokio::test]
async fn the_activation_sweep_does_not_advance_the_act_sequence() {
    let provider = harness().await;
    must_schedule(&provider, window(0x_5e_02, t(10), Some(t(20)))).await;
    let conn = provider.conn().expect("scoped connection");
    let id = Uuid::from_u128(0x_5e_02);

    let active = window_repo::transition(
        &conn,
        &scope(),
        TENANT,
        id,
        WindowState::Active,
        t(10),
        stamp_at(t(10)),
    )
    .await
    .expect("scheduled -> active");
    assert_eq!(
        active.mutation_seq, 0,
        "an activation is the clock's, not an operator's act"
    );

    let expired = window_repo::transition(
        &conn,
        &scope(),
        TENANT,
        id,
        WindowState::Expired,
        t(20),
        stamp_at(t(20)),
    )
    .await
    .expect("active -> expired");
    assert_eq!(expired.mutation_seq, 0, "and neither is an expiry");
}

/// A cancellation **is** an operator act, so it advances the sequence — the other
/// side of [`the_activation_sweep_does_not_advance_the_act_sequence`].
#[tokio::test]
async fn a_cancellation_advances_the_act_sequence() {
    let provider = harness().await;
    must_schedule(&provider, window(0x_5e_03, t(10), Some(t(20)))).await;
    let conn = provider.conn().expect("scoped connection");

    let cancelled = window_repo::transition(
        &conn,
        &scope(),
        TENANT,
        Uuid::from_u128(0x_5e_03),
        WindowState::Cancelled,
        t(2),
        stamp_at(t(2)),
    )
    .await
    .expect("scheduled -> cancelled");
    assert_eq!(cancelled.mutation_seq, 1);
}

/// **The precondition is compared by the `UPDATE`, not by a read before it.**
///
/// The sequence rides in the statement's own `WHERE`, which is what
/// `api::rest::preconditions`' doc means by *"the repository's compare-and-swap is
/// the authority"*: a tag compared after a read and before a write is a decision
/// handed to a statement that races it. So an act asserting a sequence the row has
/// left is refused as a stale row version — the same refusal, and the same wire code
/// (`STALE_VERSION`), that every other precondition in this gear answers with.
#[tokio::test]
async fn an_adjustment_against_a_stale_act_sequence_is_refused() {
    let provider = harness().await;
    must_schedule(&provider, window(0x_5e_04, t(10), Some(t(20)))).await;
    let conn = provider.conn().expect("scoped connection");
    let id = Uuid::from_u128(0x_5e_04);

    window_repo::adjust_effective_to(&conn, &scope(), TENANT, id, Some(t(25)), 0, stamp_at(t(1)))
        .await
        .expect("the first act lands and takes the sequence to 1");

    let refused = window_repo::adjust_effective_to(
        &conn,
        &scope(),
        TENANT,
        id,
        Some(t(30)),
        /* the sequence the first act consumed */ 0,
        stamp_at(t(1)),
    )
    .await
    .expect_err("an act against a consumed sequence is refused");
    assert!(
        matches!(refused, RepoError::StaleRowVersion { .. }),
        "a moved premise is a stale row version, not a storage failure: {refused:?}"
    );

    let read = window_repo::find(&conn, &scope(), TENANT, id)
        .await
        .expect("read")
        .expect("there");
    assert_eq!(
        read.effective_to,
        Some(t(25)),
        "and the refused act wrote nothing"
    );
    assert_eq!(read.mutation_seq, 1, "nor did it advance the sequence");
}

/// **A tag no row could ever carry is the caller's mistake, not a corrupt row**
/// (review M54).
///
/// `RowVersion::from_etag` accepts any `u64`, and the window door hands what it
/// parsed straight down as `expected_seq`. Until 2026-08-20 that number was
/// converted by `advanced_seq` -- the helper whose subject is a sequence **the
/// row holds** -- so an `If-Match` above `i64::MAX` came back
/// `RepoError::CorruptRow`, which `repo_failure` renders 500 Internal blaming
/// `pricing_price_window.mutation_seq of window {id}` for a value the column
/// never held. An operator was sent to inspect a row that was fine.
///
/// `storable_seq` is the split: a tag no row can carry cannot match the row, so
/// the answer is the ordinary stale-version refusal the price-row door already
/// gives for the same input. Both numbers are asserted, because "stale" without
/// the pair is a refusal an operator cannot act on -- and `current` is what says
/// the sequence they should have sent.
#[tokio::test]
async fn an_act_sequence_past_what_the_column_can_hold_is_refused_as_stale() {
    let provider = harness().await;
    must_schedule(&provider, window(0x_5e_05, t(10), Some(t(20)))).await;
    let conn = provider.conn().expect("scoped connection");
    let id = Uuid::from_u128(0x_5e_05);

    // One past `i64::MAX`: a legal `u64`, and a number `mutation_seq` cannot hold.
    let unstorable = 9_223_372_036_854_775_808_u64;
    let refused = window_repo::adjust_effective_to(
        &conn,
        &scope(),
        TENANT,
        id,
        Some(t(25)),
        unstorable,
        stamp_at(t(1)),
    )
    .await
    .expect_err("a sequence no row can hold cannot match one");
    match refused {
        RepoError::StaleRowVersion {
            current, submitted, ..
        } => {
            assert_eq!(
                current, 0,
                "the window stands at the sequence its schedule gave it"
            );
            assert_eq!(
                submitted, unstorable,
                "and the refusal hands back what the caller sent, which is what makes the \
                 refusal actionable"
            );
        }
        other => panic!("an out-of-range tag is the caller's number, not the row's: {other:?}"),
    }

    let read = window_repo::find(&conn, &scope(), TENANT, id)
        .await
        .expect("read")
        .expect("there");
    assert_eq!(
        read.effective_to,
        Some(t(20)),
        "and the refused act wrote nothing"
    );
    assert_eq!(read.mutation_seq, 0, "nor did it advance the sequence");

    // The control: the same act under the sequence the window actually stands at
    // lands. Without it "the adjustment was refused" would be satisfied by a
    // window this suite had left unadjustable for some other reason.
    window_repo::adjust_effective_to(&conn, &scope(), TENANT, id, Some(t(25)), 0, stamp_at(t(1)))
        .await
        .expect("the in-range tag is the one the window stands at");
}

// ---------------------------------------------------------------------------
// The collection read: `GET /bss-pricing/v1/price-windows`
// ---------------------------------------------------------------------------

/// The page is a keyset walk on `window_id`, and the walk visits every window
/// once.
///
/// The two assertions are separate facts. That the page is **bounded** is what
/// `limit` means; that resuming after the last id continues rather than restarts
/// is what makes the token a cursor rather than an offset. A `list_page` that
/// ignored `after` would satisfy the first and fail the second.
#[tokio::test]
async fn the_window_page_walks_by_window_id_and_never_repeats_one() {
    let provider = harness().await;
    must_schedule(&provider, window(0x_11_01, t(10), Some(t(20)))).await;
    must_schedule(&provider, window(0x_11_02, t(20), Some(t(30)))).await;
    must_schedule(
        &provider,
        on_row(0x_11_03, OTHER_KEY_ROW, t(10), Some(t(20))),
    )
    .await;
    let conn = provider.conn().expect("scoped connection");

    let mut walked: Vec<Uuid> = Vec::new();
    let mut after = None;
    loop {
        let page = window_repo::list_page(&conn, &scope(), TENANT, None, after, 2)
            .await
            .expect("list");
        assert!(page.len() <= 2, "the page is bounded by the limit it asked");
        let Some(last) = page.last() else { break };
        after = Some(last.window_id);
        walked.extend(page.iter().map(|record| record.window_id));
    }

    assert_eq!(
        walked,
        vec![
            Uuid::from_u128(0x_11_01),
            Uuid::from_u128(0x_11_02),
            Uuid::from_u128(0x_11_03),
        ],
        "every window once, in window_id order"
    );
}

/// `price_id` narrows the page to one price row's windows.
///
/// **`plan_id` is not the filter and cannot be**: `pricing_price_window` carries
/// `price_id` and no plan reference at all (the entity's module doc says why —
/// the plan lives on `pricing_price` so the two cannot disagree). The filter is
/// asserted against [`OTHER_KEY_ROW`] rather than [`ROW`] so that a query which
/// dropped the condition would return the two windows of `ROW` as well and fail.
#[tokio::test]
async fn the_window_page_narrows_to_one_price_row() {
    let provider = harness().await;
    must_schedule(&provider, window(0x_12_01, t(10), Some(t(20)))).await;
    must_schedule(&provider, window(0x_12_02, t(20), Some(t(30)))).await;
    must_schedule(
        &provider,
        on_row(0x_12_03, OTHER_KEY_ROW, t(10), Some(t(20))),
    )
    .await;
    let conn = provider.conn().expect("scoped connection");

    let page = window_repo::list_page(&conn, &scope(), TENANT, Some(OTHER_KEY_ROW), None, 100)
        .await
        .expect("list");

    assert_eq!(
        page.iter()
            .map(|record| record.window_id)
            .collect::<Vec<_>>(),
        vec![Uuid::from_u128(0x_12_03)],
        "only the named row's window"
    );
    assert_eq!(
        page[0].scope_key.plan_id(),
        PlanId::new(PLAN),
        "and the page resolves each window's key out of pricing_price, as `find` does"
    );
}
