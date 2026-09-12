//! The half-open interval arithmetic, the two derivations that hang off the
//! state token, and the `SQLite` half of the schema's own overlap refusal.
//!
//! Almost no database here, on purpose: [`intersects`] is the whole of §6's
//! non-overlap rule reduced to four instants, and the shapes that matter —
//! adjacency, containment, an open-ended end — are cheaper and clearer to state
//! as arithmetic than to seed as rows. `tests/sqlite_window_repo.rs` is where the
//! rule is proved *through the repository*, against the store that has to enforce
//! it.
//!
//! # The exception, and why it cannot live in `tests/`
//!
//! [`overlap_or`] reads a driver **message** — `pricing_price_window`'s constraint
//! name on Postgres, its `RAISE(ABORT)` text on `SQLite` — and answers
//! [`RepoError::WindowOverlap`] where `contention_or_db` would have answered
//! [`RepoError::Db`], which the door renders as a 500. Its Postgres half is
//! pinned behaviourally by `tests/postgres_window.rs`'s two race cases. Its
//! `SQLite` half was pinned only by `tests/sqlite_migrations.rs`'s name-and-digest
//! census, which protects the *migration's* literal and says nothing about the
//! recognizer's: change the string `overlap_or` matches on and every `SQLite`
//! overlap refusal degrades to a 500 with the whole suite green (review F7,
//! 2026-08-21).
//!
//! Closing that needs the two literals compared *through the engine* — a real
//! abort message handed to the real recognizer — and neither end of that is
//! reachable from an integration test. `overlap_or` is private, and its input is a
//! `toolkit_db::secure::ScopeError` no public door hands back: every door maps it
//! before it returns. So the two cases below take a migrated in-memory mirror,
//! write past [`super::schedule`]'s pre-check so that the **trigger** is what
//! answers, and feed the error it produced to the function under test. In-crate
//! database tests have precedent for exactly this reason — see
//! `idempotency_repo_tests`, whose compare-and-swap is likewise unreachable
//! through the public surface.
//!
//! A **race** is what `adjust_effective_to` actually loses on Postgres, and it is
//! deliberately not simulated here: `SQLite` serializes its writers, so a
//! concurrency case on this harness would prove nothing. What is proved here is
//! the pair the race depends on — that the trigger refuses, and that its wording
//! is one the recognizer knows.

use sea_orm::ActiveValue::Set;
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, EntityTrait};
use sea_orm_migration::MigratorTrait;
use toolkit_db::migration_runner::run_migrations_for_testing;
use toolkit_db::secure::{AccessScope, ScopeError, SecureInsertExt, SecureUpdateExt};
use toolkit_db::{ConnectOpts, DBProvider, DbError, connect_db};
use uuid::Uuid;

use super::{OCCUPYING_STATES, intersects, overlap_or, pick, render_interval};
use crate::domain::instant::utc_ymd_hms;
use crate::domain::scope_key::ScopeKey;
use crate::domain::window::WindowState;
use crate::infra::storage::RepoError;
use crate::infra::storage::entity::{price, price_window};
use crate::infra::storage::migrations::Migrator;
use crate::infra::storage::repo::price_repo;
use time::OffsetDateTime;

/// `2026-08-05T<hour>:00:00Z`. Every instant below is a whole hour of one day, so
/// a reader can see the shape of an interval pair at a glance.
fn t(hour: u32) -> OffsetDateTime {
    utc_ymd_hms(2026, 8, 5, hour, 0, 0)
}

/// **The rule §9 names by name.** `effectiveTo = next.effectiveFrom` is adjacency
/// and not an overlap, in both directions, because the interval is half-open: the
/// boundary instant belongs to the later window and to nothing else.
///
/// If this ever answers `true`, every coverage test of the slice is wrong with it
/// and so is every supersession and every cutover — both produce exactly this
/// shape.
#[test]
fn two_adjacent_intervals_do_not_intersect() {
    assert!(!intersects(t(1), Some(t(2)), t(2), Some(t(3))));
    assert!(!intersects(t(2), Some(t(3)), t(1), Some(t(2))));
}

/// The statement the guard-by-removal proof uses, as arithmetic: `[t1, t3)` and
/// `[t2, t4)` with `t1 < t2 < t3 < t4`. Fully covered from `t1` to `t4`, no
/// interior gap, and overlapping over `[t2, t3)`.
#[test]
fn a_staggered_pair_intersects_over_the_shared_stretch() {
    assert!(intersects(t(1), Some(t(3)), t(2), Some(t(4))));
    assert!(intersects(t(2), Some(t(4)), t(1), Some(t(3))));
}

#[test]
fn a_contained_interval_intersects_its_container() {
    assert!(intersects(t(1), Some(t(9)), t(3), Some(t(4))));
    assert!(intersects(t(3), Some(t(4)), t(1), Some(t(9))));
}

#[test]
fn an_interval_intersects_itself() {
    assert!(intersects(t(1), Some(t(2)), t(1), Some(t(2))));
    assert!(intersects(t(1), None, t(1), None));
}

/// An open-ended window reaches every later instant, so nothing may be scheduled
/// after it starts — which is why a key with one has no successor to schedule and
/// why `inst-ws-expire` says it never expires.
#[test]
fn an_open_ended_interval_intersects_everything_from_its_start_on() {
    assert!(intersects(t(1), None, t(5), Some(t(6))));
    assert!(intersects(t(5), Some(t(6)), t(1), None));
    assert!(intersects(t(1), None, t(1), Some(t(2))));
}

/// And nothing **before** its start: an open-ended end is not an open-ended
/// beginning. Without this the arithmetic would refuse the one shape a key that
/// has run open-ended forever must still admit — a historical window that closed
/// before it began.
#[test]
fn an_open_ended_interval_does_not_reach_back_before_its_start() {
    assert!(!intersects(t(5), None, t(1), Some(t(5))));
    assert!(!intersects(t(1), Some(t(5)), t(5), None));
}

/// A zero-width probe at a boundary. `chk_pricing_price_window_interval` makes
/// `effective_to = effective_from` unstorable, so this can only arrive from a
/// caller's *candidate* interval — and it must not be read as intersecting the
/// window it abuts.
#[test]
fn an_empty_candidate_interval_intersects_nothing_at_its_own_boundary() {
    assert!(!intersects(t(2), Some(t(2)), t(2), Some(t(3))));
}

/// The occupant set, stated as the property rather than as the constant's own
/// spelling: exactly the two non-terminal states compete for a key.
#[test]
fn only_the_non_terminal_states_occupy_a_key() {
    for state in WindowState::ALL {
        assert_eq!(
            OCCUPYING_STATES.contains(state),
            !state.is_terminal(),
            "{state}"
        );
    }
}

/// An expiry keeps the `activated_at` it was given when the price took effect —
/// `chk_pricing_price_window_activated_at` requires an `expired` window to carry
/// one, so a returned record that recomputed the timestamps from the new state
/// would disagree with the row it just wrote.
#[test]
fn a_flip_stamps_its_own_column_and_carries_the_others_forward() {
    let activated = t(1);
    let expired = t(9);
    assert_eq!(
        pick(
            WindowState::Expired,
            WindowState::Active,
            expired,
            Some(activated)
        ),
        Some(activated)
    );
    assert_eq!(
        pick(WindowState::Expired, WindowState::Expired, expired, None),
        Some(expired)
    );
    assert_eq!(
        pick(WindowState::Active, WindowState::Cancelled, activated, None),
        None
    );
}

/// A rendered interval keeps the half-open brackets and spells an absent end.
/// Both halves are what let an operator reading the refusal see why the window
/// abutting theirs was not the one that collided.
#[test]
fn a_rendered_interval_keeps_its_asymmetry_and_names_an_open_end() {
    let bounded = render_interval(t(1), Some(t(2)));
    assert!(bounded.starts_with('['), "{bounded}");
    assert!(bounded.ends_with(')'), "{bounded}");
    assert!(bounded.contains("2026-08-05T01:00:00"), "{bounded}");
    assert!(bounded.contains("2026-08-05T02:00:00"), "{bounded}");
    assert!(
        render_interval(t(1), None).contains("open-ended"),
        "an absent end is spelled, not dropped"
    );
}

// ---------------------------------------------------------------------------
// The schema's own refusal, and the recognizer that reads it back — `SQLite`
// ---------------------------------------------------------------------------

const TENANT: Uuid = Uuid::from_u128(0x_7e_11);
const ACTOR: Uuid = Uuid::from_u128(0x_ac_01);
const PLAN: Uuid = Uuid::from_u128(0x_91_a1);
const PHASE: Uuid = Uuid::from_u128(0x_40_a5);
/// The one price row every window below hangs off. Both triggers are scoped by
/// `(tenant_id, price_id)`, so one row is the whole world either of them can see.
const ROW: Uuid = Uuid::from_u128(0x_a0_01);

/// The abort text `excl_pricing_price_window_no_overlap`'s two `SQLite` triggers raise, as the
/// **migration** spells it.
///
/// Stated here as well as in [`overlap_or`] on purpose, and it is not a third
/// copy of a literal: the assertions below take the message off the engine and
/// ask each side about it separately, so a drift between the migration and the
/// recognizer reddens whichever of the two moved. A single shared constant would
/// make both sides agree by construction and prove nothing.
/// The window ids the two cases below write. `FIRST` and `ABUTTING` are the world
/// each case is judged against; `COLLIDING` is the insert that must be refused and
/// `SUBJECT` is the row whose end the update extends into `ABUTTING`.
const FIRST: u128 = 0x_01;
const ABUTTING: u128 = 0x_02;
const COLLIDING: u128 = 0x_03;
const SUBJECT: u128 = 0x_04;

const MIRROR_ABORT: &str = "interval overlaps an occupying window on this price row";

/// `2099-09-<day>T00:00:00Z`.
///
/// **The year is load-bearing.** `pricing_price_window`'s fifth `SQLite` arm refuses a
/// move of an `effective_to` that is not in the future, comparing against
/// `CURRENT_TIMESTAMP` — so the update case below would be answered by *that* arm
/// rather than by the overlap trigger it is written for, on any instant that has
/// passed. [`t`] above is 2026 and deliberately stays there: it feeds arithmetic,
/// which has no clock.
fn future(day: u32) -> OffsetDateTime {
    utc_ymd_hms(2099, 9, day, 0, 0, 0)
}

/// A migrated in-memory mirror holding one price row, with that row's canonical
/// scope key resolved out of it rather than restated here.
async fn mirror() -> (DBProvider<DbError>, ScopeKey) {
    let db = connect_db("sqlite::memory:", ConnectOpts::default())
        .await
        .expect("connect in-memory sqlite");
    run_migrations_for_testing(&db, Migrator::migrations())
        .await
        .expect("run migrator");
    let provider = DBProvider::<DbError>::new(db);
    let scope = AccessScope::for_tenant(TENANT);
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
        created_at_utc: Set(future(1)),
        ..price::ActiveModel::default()
    };
    price::Entity::insert(row.clone())
        .secure()
        .scope_with_model(&scope, &row)
        .expect("the scope permits the seeded price row")
        .exec(&conn)
        .await
        .expect("seed the price row the windows hang off");

    let key = price_repo::load_scope_key(&conn, &scope, TENANT, ROW)
        .await
        .expect("resolve the seeded row's key")
        .expect("the row is there");
    (provider, key)
}

/// Insert one `scheduled` — and therefore **occupying** — window on [`ROW`],
/// straight through its entity, keeping the driver's own error.
///
/// Past [`super::schedule`] on purpose, and that is the whole reason these two
/// cases exist: that door's `refuse_overlap` walk answers first on a single-writer
/// store, so a case going through it would prove the **pre-check** and never reach
/// the trigger under test. The scope gate is still crossed — the write cannot
/// fabricate a row the tenant would not be given — and the error is returned
/// unmapped, because [`overlap_or`]'s input is a `ScopeError` and mapping it here
/// would throw away the thing being tested.
async fn insert_window(
    provider: &DBProvider<DbError>,
    id: u128,
    from: OffsetDateTime,
    to: OffsetDateTime,
) -> Result<(), ScopeError> {
    let am = price_window::ActiveModel {
        window_id: Set(Uuid::from_u128(id)),
        tenant_id: Set(TENANT),
        price_id: Set(ROW),
        effective_from: Set(from),
        effective_to: Set(Some(to)),
        state: Set(WindowState::Scheduled.as_str().to_owned()),
        reason_code: Set("priceIncrease".to_owned()),
        created_by: Set(ACTOR),
        created_at: Set(future(1)),
        activated_at: Set(None),
        expired_at: Set(None),
        cancelled_at: Set(None),
        mutation_seq: Set(0),
    };
    let scope = AccessScope::for_tenant(TENANT);
    let conn = provider.conn().expect("scoped connection");
    price_window::Entity::insert(am.clone())
        .secure()
        .scope_with_model(&scope, &am)
        .expect("the scope permits the window")
        .exec(&conn)
        .await
        .map(|_| ())
}

/// **The `BEFORE INSERT` arm refuses an overlap, and [`overlap_or`] reads its
/// refusal back as the domain's own.**
///
/// Two claims in one case, because neither is worth much alone. If the trigger
/// stopped refusing, the `expect_err` is what says so. If [`overlap_or`]'s
/// `SQLite` literal drifted from the migration's, the error falls through to
/// `contention_or_db` and arrives as [`RepoError::Db`] — a 500 telling the caller
/// to retry a write that can never succeed — and the destructure is what says so.
/// `tests/sqlite_migrations.rs`'s name-and-digest census sees neither: it pins the
/// migration's string against itself.
///
/// The abutting window is the positive control. Without it a trigger that refused
/// every second insert on a price row would pass this case, and `[t, u)` beside
/// `[u, v)` is exactly the shape §9 requires the rule to admit — it is what a
/// supersession and a cutover both leave behind.
#[tokio::test]
async fn the_mirrors_insert_trigger_refuses_an_overlap_and_the_recognizer_names_it() {
    let (provider, key) = mirror().await;

    insert_window(&provider, FIRST, future(10), future(20))
        .await
        .expect("the first window on an empty key must land");
    insert_window(&provider, ABUTTING, future(20), future(30))
        .await
        .expect("a window abutting it must land too: adjacency is not an overlap");

    let refused = insert_window(&provider, COLLIDING, future(15), future(25))
        .await
        .expect_err("the trigger must refuse a window overlapping an occupying one");
    let raised = refused.to_string();
    assert!(
        raised.contains(MIRROR_ABORT),
        "the refusal must be the overlap trigger's own and not a neighbour arm's: {raised}"
    );

    let recognized = overlap_or(
        &refused,
        &key,
        future(15),
        Some(future(25)),
        &format!("window {}", Uuid::from_u128(COLLIDING)),
        "insert pricing_price_window",
    );
    let RepoError::WindowOverlap {
        key: named,
        requested,
        ..
    } = &recognized
    else {
        panic!("the mirror's abort must be recognized, not rendered a 500: {recognized:?}");
    };
    assert_eq!(named, &key.to_string(), "the key the collision happened on");
    assert_eq!(
        requested,
        &render_interval(future(15), Some(future(25))),
        "and the interval the caller asked for"
    );
}

/// **The `BEFORE UPDATE` arm refuses an overlap too, and so does the recognizer.**
///
/// The statement here is [`super::adjust_effective_to`]'s: `effective_to` moved
/// forward and the act sequence advanced by one, on a `scheduled` row. It matters
/// more than the insert arm, because extending an end is the mutation that makes a
/// genuinely **new** claim on the key — and because that door mapped this very
/// error to [`RepoError::Db`] until review F1, 2026-08-21.
///
/// It reaches the overlap trigger and no other arm, which on this table takes
/// saying: the state does not move (the flip whitelist is silent), no frozen column
/// moves, both ends are in 2099 (the future-end arm is silent), the row is
/// `scheduled` rather than terminal (the immutable-history arm is silent), and the
/// sequence moves by exactly one (`pricing_price_window`'s arm is silent). The message
/// assertion is what holds that claim rather than a reader's arithmetic.
#[tokio::test]
async fn the_mirrors_update_trigger_refuses_an_overlap_and_the_recognizer_names_it() {
    let (provider, key) = mirror().await;
    let subject = Uuid::from_u128(SUBJECT);

    insert_window(&provider, SUBJECT, future(10), future(20))
        .await
        .expect("the subject window must land");
    insert_window(&provider, ABUTTING, future(20), future(30))
        .await
        .expect("its abutting neighbour must land");

    let scope = AccessScope::for_tenant(TENANT);
    let conn = provider.conn().expect("scoped connection");
    let refused = price_window::Entity::update_many()
        .secure()
        .scope_with(&scope)
        .col_expr(
            price_window::Column::EffectiveTo,
            Expr::value(Some(future(25))),
        )
        .col_expr(price_window::Column::MutationSeq, Expr::value(1_i64))
        .filter(
            Condition::all()
                .add(price_window::Column::TenantId.eq(TENANT))
                .add(price_window::Column::WindowId.eq(subject)),
        )
        .exec(&conn)
        .await
        .expect_err("the trigger must refuse an end extended across its neighbour");

    let raised = refused.to_string();
    assert!(
        raised.contains(MIRROR_ABORT),
        "the refusal must be the overlap trigger's own and not the frozen-column, \
         future-end or act-sequence arm's: {raised}"
    );

    let recognized = overlap_or(
        &refused,
        &key,
        future(10),
        Some(future(25)),
        &format!("window {subject}"),
        &format!("adjust pricing_price_window {subject}"),
    );
    let RepoError::WindowOverlap {
        key: named,
        requested,
        ..
    } = &recognized
    else {
        panic!("the mirror's abort must be recognized, not rendered a 500: {recognized:?}");
    };
    assert_eq!(named, &key.to_string(), "the key the collision happened on");
    assert_eq!(
        requested,
        &render_interval(future(10), Some(future(25))),
        "and the interval the extension asked for"
    );
}
