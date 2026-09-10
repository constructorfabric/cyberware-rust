//! The activation/expiry sweep against the executed `SQLite` mirror.
//!
//! `WindowActivationJob` is a statement about rows in two tables at once —
//! `pricing_price_window` and `pricing_outbox` — and about what one transaction
//! does to them together. A unit test over the job could assert that it calls
//! the store; it could not assert that a second pass over the same instant flips
//! nothing, that an open-ended window is never picked up, or that one plan's
//! events come back sequenced. So the sweep's behaviour lives here and
//! `src/infra/jobs/window_activation_tests.rs` keeps only what needs no
//! database, which is the split
//! `readmodel_warm.rs` / `tests/sqlite_read_model.rs` established.
//!
//! # Instants are the caller's, and that is what makes them facts
//!
//! [`WindowActivationJob::run`] takes `now`, so every "past" and "future" in
//! this file is a relation between two values this file supplies — not between a
//! fixture and the wall clock. The window instants are nevertheless in **2099**,
//! for `tests/sqlite_window_repo.rs`'s reason: the mirror's fifth trigger arm
//! compares an `effective_to` against `CURRENT_TIMESTAMP`, so a suite that ever
//! grows an `effectiveTo` adjustment inherits a fixture that would start failing
//! on a date rather than on a defect.
//!
//! # The one place the *text* of an instant matters
//!
//! [`a_boundary_earlier_the_same_day_is_due`] exists because the mirror stores
//! `timestamptz` as `text`: a due predicate rendered on one side and stored on
//! the other in different shapes compares lexicographically, and the two shapes
//! agree on the date and disagree at byte 11 — which is how a whole arm of this
//! chain once read every instant on today's UTC date as being in the future. A
//! comparison across a date boundary cannot see that; one inside a single day
//! can.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;

use bss_pricing::config::JobsConfig;
use bss_pricing::domain::audit::AuditStamp;
use bss_pricing::domain::events::CatalogEvent;
use bss_pricing::domain::lifecycle::LifecycleState;
use bss_pricing::domain::window::WindowState;
use bss_pricing::infra::jobs::window_activation::{ActivationReport, WindowActivationJob};
use bss_pricing::infra::metrics::test_harness::MetricsHarness;
use bss_pricing::infra::storage::entity::{outbox, price, price_window};
use bss_pricing::infra::storage::migrations::Migrator;
use bss_pricing::infra::storage::repo::window_repo::{self, NewWindow};

use bss_pricing::domain::instant::utc_ymd_hms;
use sea_orm::ActiveValue::Set;
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, EntityTrait, Order};
use sea_orm_migration::MigratorTrait;
use time::Duration;
use time::OffsetDateTime;
use toolkit_db::migration_runner::run_migrations_for_testing;
use toolkit_db::secure::{AccessScope, SecureEntityExt, SecureInsertExt, SecureUpdateExt};
use toolkit_db::{ConnectOpts, DBProvider, DbError, connect_db};
use uuid::Uuid;

const TENANT: Uuid = Uuid::from_u128(0x7e_11);
const OTHER_TENANT: Uuid = Uuid::from_u128(0x7e_22);
const ACTOR: Uuid = Uuid::from_u128(0xac_01);

/// The plan every window in this suite hangs off, unless a test names another.
const PLAN: Uuid = Uuid::from_u128(0x91_a1);
/// A second plan of the **same** tenant — the other half of "ordered per
/// `(tenant, plan)`": one plan's sequence must not be another's.
const PLAN_B: Uuid = Uuid::from_u128(0x91_b2);
/// A plan of another tenant, for the cross-tenant pass.
const PLAN_FOREIGN: Uuid = Uuid::from_u128(0x91_c3);
const PHASE: Uuid = Uuid::from_u128(0x40_a5);

/// `PLAN`'s published row.
const ROW: Uuid = Uuid::from_u128(0xa0_01);
/// A second row of `PLAN` on a **different** canonical scope key (a different
/// `charge_kind`), so two windows may hold one interval without overlapping.
const ROW_OTHER_KEY: Uuid = Uuid::from_u128(0xa0_02);
/// `PLAN_B`'s row.
const ROW_B: Uuid = Uuid::from_u128(0xa0_03);
/// `PLAN_FOREIGN`'s row, under [`OTHER_TENANT`].
const ROW_FOREIGN: Uuid = Uuid::from_u128(0xa0_04);
/// A **never-published draft** row of `PLAN`, on a key of its own.
///
/// Not a contrived fixture: a window has to be schedulable on a draft row,
/// because `inst-wc-required` fails a publish whose key has no covering window,
/// so every row is authored with its coverage *before* it publishes. The draft
/// state is therefore the state every window is scheduled in.
const ROW_DRAFT: Uuid = Uuid::from_u128(0xa0_05);

/// One value for the whole binary: this suite drives a job and a repository
/// directly, where the value the HTTP edge would have established has no
/// producer.
const TEST_CORRELATION: Uuid = Uuid::from_u128(0x_c0_11_a7_10);

/// `2099-09-01T00:00:00Z` plus `day` days.
///
/// Days rather than hours so every instant in a test is legibly ordered, and an
/// addition rather than a calendar literal so a test may reach past the end of
/// the month — `an_open_ended_window_never_expires` sweeps ten years out.
fn t(day: i64) -> OffsetDateTime {
    utc_ymd_hms(2099, 9, 1, 0, 0, 0) + Duration::days(day)
}

fn stamp_at(when: OffsetDateTime) -> AuditStamp {
    AuditStamp {
        actor_principal_id: ACTOR,
        recorded_at: when,
        correlation_id: TEST_CORRELATION,
    }
}

fn scope_of(tenant: Uuid) -> AccessScope {
    AccessScope::for_tenant(tenant)
}

async fn harness() -> DBProvider<DbError> {
    let db = connect_db("sqlite::memory:", ConnectOpts::default())
        .await
        .expect("connect in-memory sqlite");
    run_migrations_for_testing(&db, Migrator::migrations())
        .await
        .expect("run migrator");
    let provider = DBProvider::<DbError>::new(db);
    for (tenant, plan, row, charge_kind, lifecycle_state) in [
        (TENANT, PLAN, ROW, "recurring", "published"),
        (TENANT, PLAN, ROW_OTHER_KEY, "one_time", "published"),
        (TENANT, PLAN_B, ROW_B, "recurring", "published"),
        (
            OTHER_TENANT,
            PLAN_FOREIGN,
            ROW_FOREIGN,
            "recurring",
            "published",
        ),
        (TENANT, PLAN, ROW_DRAFT, "usage", "draft"),
    ] {
        seed_price_row(&provider, tenant, plan, row, charge_kind, lifecycle_state).await;
    }
    provider
}

/// One price row in the lifecycle state the caller names, written through its
/// entity under the tenant that owns it.
///
/// Through the entity rather than `PriceRepo::create_draft` for
/// `tests/sqlite_window_repo.rs`'s reason: what this suite needs is a row a
/// window can hang off, and composing it out of the authoring path would put
/// that path's rules between the fixture and the sweep under test. The insert
/// still goes through the tenant gate.
///
/// The **state is a parameter** because the sweep's row set is a lifecycle
/// question: `chk_pricing_price_lifecycle_state` admits `draft`, `published` and
/// `superseded`, and which of the three a window's row stands in decides whether
/// the sweep may announce that window took effect.
async fn seed_price_row(
    provider: &DBProvider<DbError>,
    tenant_id: Uuid,
    plan_id: Uuid,
    price_id: Uuid,
    charge_kind: &str,
    lifecycle_state: &str,
) {
    let conn = provider.conn().expect("scoped connection");
    let row = price::ActiveModel {
        price_id: Set(price_id),
        tenant_id: Set(tenant_id),
        plan_id: Set(plan_id),
        currency: Set("USD".to_owned()),
        region: Set("EU".to_owned()),
        phase: Set(PHASE),
        charge_kind: Set(charge_kind.to_owned()),
        amount_minor: Set(Some(1_000)),
        model_kind: Set(Some("flat".to_owned())),
        lifecycle_state: Set(lifecycle_state.to_owned()),
        created_by: Set(ACTOR),
        created_at_utc: Set(t(0)),
        ..price::ActiveModel::default()
    };
    price::Entity::insert(row.clone())
        .secure()
        .scope_with_model(&scope_of(tenant_id), &row)
        .expect("scope the seeded price row")
        .exec(&conn)
        .await
        .unwrap_or_else(|e| panic!("seed price row {price_id}: {e}"));
}

/// Move one seeded row along `pricing_price`'s own lifecycle.
///
/// `tests/sqlite_price_repo.rs`'s `flip_state`, for its reason: the two edges
/// this suite needs — `draft → published` and `published → superseded` — are the
/// two the table's trigger sanctions, and there is no repository call that does
/// only this. The `rows_affected` assertion is what stops a test from claiming a
/// world the store never entered.
async fn flip_lifecycle(
    provider: &DBProvider<DbError>,
    tenant: Uuid,
    price_id: Uuid,
    state: LifecycleState,
) {
    let conn = provider.conn().expect("scoped connection");
    let result = price::Entity::update_many()
        .secure()
        .scope_with(&scope_of(tenant))
        .col_expr(price::Column::LifecycleState, Expr::value(state.as_str()))
        .filter(Condition::all().add(price::Column::PriceId.eq(price_id)))
        .exec(&conn)
        .await
        .unwrap_or_else(|e| panic!("flip price row {price_id} to {state}: {e}"));
    assert_eq!(result.rows_affected, 1, "the seed must have moved one row");
}

/// One window already `active` with its end in the past, written straight through
/// the entity.
///
/// The backlog fixture, and the only place this suite does not walk the machine.
/// Reaching this state through the sweep would take one pass per window and the
/// point of the fixture is a page **saturated** with them, so the rows are seeded
/// where they would stand: `active`, `activated_at` at the start they were the
/// arrival of, and an `effective_to` that has passed. Nothing is bypassed that
/// judges this state — the migration's own note records that there is deliberately
/// no born-in-a-state INSERT guard, and the three biconditional CHECKs plus
/// `chk_pricing_price_window_activation_order` all still judge the row.
///
/// It is a **reachable** world, not a contrived one: an `active` window whose end
/// has passed is exactly what a stalled sweep leaves behind, which is the state
/// `pricing.window.activation_overdue` exists to report and the state a saturated
/// page is drawn from.
async fn seed_active_window(
    provider: &DBProvider<DbError>,
    tenant: Uuid,
    price_id: Uuid,
    id: u128,
    from: OffsetDateTime,
    to: OffsetDateTime,
) -> Uuid {
    let conn = provider.conn().expect("scoped connection");
    let window_id = Uuid::from_u128(id);
    let row = price_window::ActiveModel {
        window_id: Set(window_id),
        tenant_id: Set(tenant),
        price_id: Set(price_id),
        effective_from: Set(from),
        effective_to: Set(Some(to)),
        state: Set("active".to_owned()),
        reason_code: Set("priceIncrease".to_owned()),
        created_by: Set(ACTOR),
        created_at: Set(t(0)),
        activated_at: Set(Some(from)),
        expired_at: Set(None),
        cancelled_at: Set(None),
        // Born mid-life, so no operator has acted on it beyond its own schedule.
        mutation_seq: Set(0),
    };
    price_window::Entity::insert(row.clone())
        .secure()
        .scope_with_model(&scope_of(tenant), &row)
        .expect("scope the seeded window")
        .exec(&conn)
        .await
        .unwrap_or_else(|e| panic!("seed active window {id}: {e}"));
    window_id
}

/// Schedule one window that must land.
async fn schedule(
    provider: &DBProvider<DbError>,
    tenant: Uuid,
    price_id: Uuid,
    id: u128,
    from: OffsetDateTime,
    to: Option<OffsetDateTime>,
) -> Uuid {
    let conn = provider.conn().expect("scoped connection");
    let window_id = Uuid::from_u128(id);
    window_repo::schedule(
        &conn,
        &scope_of(tenant),
        NewWindow {
            window_id,
            tenant_id: tenant,
            price_id,
            effective_from: from,
            effective_to: to,
            reason_code: "priceIncrease".to_owned(),
        },
        stamp_at(t(0)),
    )
    .await
    .unwrap_or_else(|e| panic!("window {id} must be schedulable: {e}"));
    window_id
}

fn job(provider: &DBProvider<DbError>) -> WindowActivationJob {
    WindowActivationJob::new(provider.clone(), JobsConfig::default())
}

/// One pass at `now`.
async fn sweep(provider: &DBProvider<DbError>, now: OffsetDateTime) -> ActivationReport {
    job(provider)
        .run(now)
        .await
        .expect("the sweep pass must run")
}

/// One pass at `now`, with the alarm plane attached so it can be counted.
///
/// Separate from [`sweep`] rather than replacing it: every other case in this file
/// is about the report, and a harness on all of them would make the alarm look
/// like something they assert.
async fn sweep_watched(
    provider: &DBProvider<DbError>,
    now: OffsetDateTime,
) -> (ActivationReport, MetricsHarness) {
    let harness = MetricsHarness::new();
    let report = WindowActivationJob::new(provider.clone(), JobsConfig::default())
        .with_metrics(Arc::new(harness.metrics()))
        .run(now)
        .await
        .expect("the sweep pass must run");
    harness.force_flush();
    (report, harness)
}

/// One window as the store holds it: its state and the three flip instants.
async fn window_at_rest(
    provider: &DBProvider<DbError>,
    tenant: Uuid,
    window_id: Uuid,
) -> (
    WindowState,
    Option<OffsetDateTime>,
    Option<OffsetDateTime>,
    Option<OffsetDateTime>,
) {
    let conn = provider.conn().expect("scoped connection");
    let record = window_repo::find(&conn, &scope_of(tenant), tenant, window_id)
        .await
        .expect("read the window")
        .expect("the window is there");
    (
        record.state,
        record.activated_at,
        record.expired_at,
        record.cancelled_at,
    )
}

/// Every outbox row of one tenant, in the order the store sequenced them.
async fn events(provider: &DBProvider<DbError>, tenant: Uuid) -> Vec<outbox::Model> {
    let conn = provider.conn().expect("scoped connection");
    outbox::Entity::find()
        .secure()
        .scope_with(&scope_of(tenant))
        .filter(Condition::all().add(outbox::Column::TenantId.eq(tenant)))
        .order_by(outbox::Column::AggregateId, Order::Asc)
        .order_by(outbox::Column::Seq, Order::Asc)
        .all(&conn)
        .await
        .expect("read the outbox")
}

/// The `(event name, seq)` pairs of one aggregate — the sequence a consumer sees.
async fn sequence_of(
    provider: &DBProvider<DbError>,
    tenant: Uuid,
    aggregate: Uuid,
) -> Vec<(String, i64)> {
    events(provider, tenant)
        .await
        .into_iter()
        .filter(|row| row.aggregate_id == aggregate)
        .map(|row| (row.event_name, row.seq))
        .collect()
}

// ---------------------------------------------------------------------------
// The two time-driven edges
// ---------------------------------------------------------------------------

/// `inst-ws-activate`: at `effectiveFrom` the window flips and the event emits.
///
/// The second window is what makes this a test of the boundary rather than of
/// the sweep's willingness to flip things: it starts twenty days later, and a
/// pass that took every `scheduled` row would move it too. `at` is the
/// **boundary** instant, not the sweep's, so `window_repo::transition`'s own
/// `effective_from <= at` predicate is satisfied by construction for any row the
/// sweep hands it — which means the due read is the only thing standing between
/// this pass and every scheduled window in the store.
#[tokio::test]
async fn a_scheduled_window_activates_at_its_effective_from() {
    let provider = harness().await;
    let due = schedule(&provider, TENANT, ROW, 0x01, t(10), Some(t(20))).await;
    let later = schedule(&provider, TENANT, ROW, 0x02, t(30), Some(t(40))).await;

    let report = sweep(&provider, t(10)).await;

    assert_eq!(
        report,
        ActivationReport {
            windows_due: 1,
            plans_seen: 1,
            activated: 1,
            ..ActivationReport::default()
        },
        "one window was due and it activated; nothing else was even attempted"
    );

    let (state, activated_at, expired_at, cancelled_at) =
        window_at_rest(&provider, TENANT, due).await;
    assert_eq!(state, WindowState::Active);
    assert_eq!(
        activated_at,
        Some(t(10)),
        "the flip is stamped where section 4 puts it - at effectiveFrom, not at the sweep instant"
    );
    assert_eq!(expired_at, None);
    assert_eq!(cancelled_at, None);

    let (later_state, _, _, _) = window_at_rest(&provider, TENANT, later).await;
    assert_eq!(
        later_state,
        WindowState::Scheduled,
        "a window whose start has not arrived is not touched"
    );

    let rows = events(&provider, TENANT).await;
    assert_eq!(rows.len(), 1, "one flip, one event");
    assert_eq!(
        rows[0].event_name,
        CatalogEvent::PriceWindowActivated.as_str()
    );
    assert_eq!(
        rows[0].aggregate_id, PLAN,
        "window events are ordered per (tenant, plan), so the plan is the aggregate"
    );
    // Which window the row is about, and no more of the body than that: the
    // payload's whole key set is asserted against a literal by
    // `outbox_repo_tests::both_window_transition_events_carry_the_same_whole_body_under_different_names`,
    // and the correlation id is minted per flip so this side cannot assert the
    // value by equality anyway.
    assert_eq!(
        rows[0].payload.get("windowId").and_then(|v| v.as_str()),
        Some(due.to_string().as_str())
    );
}

/// `inst-ws-expire`: at `effectiveTo` the window flips.
///
/// Reached by walking the machine — the first pass activates, the second expires
/// — because `scheduled -> expired` is not an edge and a fixture that wrote the
/// token would be asserting against a row no sanctioned path can produce.
///
/// It is also where the dedup key's **transition** half is proved: both events
/// name one window, so a key that covered only the window id would let the
/// activation refuse the expiry.
#[tokio::test]
async fn an_active_window_expires_at_its_effective_to() {
    let provider = harness().await;
    let window = schedule(&provider, TENANT, ROW, 0x01, t(10), Some(t(20))).await;

    sweep(&provider, t(10)).await;
    let report = sweep(&provider, t(20)).await;

    assert_eq!(
        report,
        ActivationReport {
            windows_due: 1,
            plans_seen: 1,
            expired: 1,
            ..ActivationReport::default()
        }
    );

    let (state, activated_at, expired_at, _) = window_at_rest(&provider, TENANT, window).await;
    assert_eq!(state, WindowState::Expired);
    assert_eq!(
        activated_at,
        Some(t(10)),
        "an expired window keeps the instant it took effect at"
    );
    assert_eq!(expired_at, Some(t(20)));

    assert_eq!(
        sequence_of(&provider, TENANT, PLAN).await,
        [
            (CatalogEvent::PriceWindowActivated.as_str().to_owned(), 0),
            (CatalogEvent::PriceWindowExpired.as_str().to_owned(), 1),
        ],
        "two transitions of one window are two events, in the order they happened"
    );
}

/// An open-ended window never expires - no fallback pricing exists, and a key
/// without a successor fails closed downstream rather than falling back.
#[tokio::test]
async fn an_open_ended_window_never_expires() {
    let provider = harness().await;
    let window = schedule(&provider, TENANT, ROW, 0x01, t(10), None).await;

    sweep(&provider, t(10)).await;
    // Ten years past the start. Nothing in the row bounds it, so if anything
    // could ever make an open-ended window due this instant would.
    let report = sweep(&provider, t(3_660)).await;

    assert_eq!(
        report,
        ActivationReport::default(),
        "an open-ended window is never due for anything"
    );
    let (state, _, expired_at, _) = window_at_rest(&provider, TENANT, window).await;
    assert_eq!(state, WindowState::Active);
    assert_eq!(expired_at, None);
    assert_eq!(
        events(&provider, TENANT).await.len(),
        1,
        "only the activation was ever emitted"
    );
}

/// Idempotent: a second pass over the same instant flips nothing and emits
/// nothing. This is what a lease takeover looks like from the store's side.
#[tokio::test]
async fn a_second_pass_over_the_same_instant_is_a_no_op() {
    let provider = harness().await;
    let window = schedule(&provider, TENANT, ROW, 0x01, t(10), Some(t(20))).await;

    let first = sweep(&provider, t(10)).await;
    assert_eq!(first.activated, 1);

    let second = sweep(&provider, t(10)).await;
    assert_eq!(
        second,
        ActivationReport::default(),
        "the second pass finds nothing due: the state predicate is the idempotence, not a marker \
         column"
    );

    let (state, activated_at, _, _) = window_at_rest(&provider, TENANT, window).await;
    assert_eq!(state, WindowState::Active);
    assert_eq!(
        activated_at,
        Some(t(10)),
        "the instant a price took effect is written once"
    );
    assert_eq!(
        events(&provider, TENANT).await.len(),
        1,
        "and one event, not two"
    );
}

/// Ordered per (tenant, plan): the outbox sequence of one plan's window events
/// is monotonic.
///
/// The world is a changeover on one key plus an unrelated window on a second
/// plan, all due at one instant: the predecessor expires at `t(20)` and the
/// successor activates at `t(20)`. A pass that emitted in the order its two
/// reads happened to return would put the activation first, which is a consumer
/// being told the successor took effect while the predecessor still holds the
/// key.
#[tokio::test]
async fn one_plans_window_events_are_sequenced() {
    let provider = harness().await;
    let predecessor = schedule(&provider, TENANT, ROW, 0x01, t(10), Some(t(20))).await;
    schedule(&provider, TENANT, ROW, 0x02, t(20), Some(t(30))).await;
    // A second plan, so "the sequence is per aggregate" is falsifiable: a global
    // counter would give this row seq 2.
    schedule(&provider, TENANT, ROW_B, 0x03, t(20), Some(t(30))).await;

    sweep(&provider, t(10)).await;
    let (state, _, _, _) = window_at_rest(&provider, TENANT, predecessor).await;
    assert_eq!(state, WindowState::Active, "the predecessor is in force");

    let report = sweep(&provider, t(20)).await;
    assert_eq!(
        report,
        ActivationReport {
            windows_due: 3,
            plans_seen: 2,
            activated: 2,
            expired: 1,
            ..ActivationReport::default()
        }
    );

    assert_eq!(
        sequence_of(&provider, TENANT, PLAN).await,
        [
            (CatalogEvent::PriceWindowActivated.as_str().to_owned(), 0),
            (CatalogEvent::PriceWindowExpired.as_str().to_owned(), 1),
            (CatalogEvent::PriceWindowActivated.as_str().to_owned(), 2),
        ],
        "the plan's three transitions in boundary order, the changeover's expiry before its \
         successor's activation"
    );
    assert_eq!(
        sequence_of(&provider, TENANT, PLAN_B).await,
        [(CatalogEvent::PriceWindowActivated.as_str().to_owned(), 0)],
        "a second plan's sequence starts at zero: the aggregate is the plan, never the deployment"
    );
}

// ---------------------------------------------------------------------------
// The pass's own properties: the date-boundary trap, tenants, the alarm
// ---------------------------------------------------------------------------

/// A boundary earlier the **same day** as the sweep is due.
///
/// The mirror stores instants as `text`, so this comparison is the one a
/// mismatch between the rendered and the stored shape breaks: the two agree on
/// the date and diverge at byte 11, which put every instant on the sweep's own
/// UTC date in the future for a whole arm of this chain. A test whose two
/// instants are on different dates passes either way.
#[tokio::test]
async fn a_boundary_earlier_the_same_day_is_due() {
    let provider = harness().await;
    let morning = t(10) + Duration::hours(6);
    let evening = t(10) + Duration::hours(18);
    let due = schedule(&provider, TENANT, ROW, 0x01, morning, Some(t(20))).await;
    let not_due = schedule(&provider, TENANT, ROW_OTHER_KEY, 0x02, evening, Some(t(20))).await;

    let report = sweep(&provider, t(10) + Duration::hours(12)).await;

    assert_eq!(report.activated, 1, "the morning window is due at noon");
    let (due_state, _, _, _) = window_at_rest(&provider, TENANT, due).await;
    assert_eq!(due_state, WindowState::Active);
    let (later_state, _, _, _) = window_at_rest(&provider, TENANT, not_due).await;
    assert_eq!(
        later_state,
        WindowState::Scheduled,
        "and the evening window on the same date is not"
    );
}

/// One pass sweeps every tenant, because the due read is under the system scope.
///
/// Two tenants, two plans, one pass. The read has to be under
/// `AccessScope::allow_all` or the foreign tenant's window is invisible and its
/// flip never happens — which is what the assertions below are about.
///
/// **What this does not assert is the narrowing**, and saying so is the point:
/// `crate::infra::jobs`'s rule is that a cross-tenant job narrows to
/// `AccessScope::for_tenant` before any per-tenant write, and the system scope
/// admits every write — so a pass that wrote under `allow_all` would produce
/// byte-identical rows and removing the narrowing reddens **nothing**, here or
/// anywhere. It is a discipline no suite in this crate can see, and its
/// guard-by-removal answer is zero. The observable half, which is asserted, is
/// that each tenant's event landed under its own tenant id.
#[tokio::test]
async fn one_pass_sweeps_every_tenant() {
    let provider = harness().await;
    let ours = schedule(&provider, TENANT, ROW, 0x01, t(10), Some(t(20))).await;
    let theirs = schedule(
        &provider,
        OTHER_TENANT,
        ROW_FOREIGN,
        0x02,
        t(10),
        Some(t(20)),
    )
    .await;

    let report = sweep(&provider, t(10)).await;

    assert_eq!(report.activated, 2);
    assert_eq!(
        report.plans_seen, 2,
        "two (tenant, plan) pairs, which is the unit the ordering is per"
    );
    let (ours_state, _, _, _) = window_at_rest(&provider, TENANT, ours).await;
    let (theirs_state, _, _, _) = window_at_rest(&provider, OTHER_TENANT, theirs).await;
    assert_eq!(ours_state, WindowState::Active);
    assert_eq!(theirs_state, WindowState::Active);

    assert_eq!(
        events(&provider, TENANT).await.len(),
        1,
        "each tenant's event is written under its own scope"
    );
    assert_eq!(events(&provider, OTHER_TENANT).await.len(), 1);
}

/// A boundary older than the job SLO raises `pricing.window.activation_overdue`.
///
/// The condition is evaluated on the due set **as the pass finds it**, which is
/// what "not yet transitioned" is a statement about: a window whose start passed
/// an hour ago and is only being flipped now was not transitioned for an hour,
/// whether or not this pass then succeeds.
#[tokio::test]
async fn a_boundary_older_than_the_job_slo_is_overdue() {
    let provider = harness().await;
    schedule(&provider, TENANT, ROW, 0x01, t(10), Some(t(20))).await;

    let report = sweep(&provider, t(10) + Duration::hours(1)).await;

    assert_eq!(
        report.activated, 1,
        "it still flips - the alarm is not a refusal"
    );
    assert_eq!(
        report.overdue, 1,
        "and the flip being an hour late is what the Warn alarm names"
    );
}

/// **One alarm per pass, whatever the backlog.**
///
/// The alarm is the operator's signal that the singleton is stalled, and the fact
/// it carries is "boundaries are standing uncrossed" — one fact per pass. Raised
/// inside the loop it fires once per overdue row, so a backlog of a thousand sends
/// a thousand identical alarms on one tick and the rule that pages someone is
/// drowned by the condition it exists to report. The count is the assertion: the
/// report's `overdue` counter cannot see how many times the alarm was raised, and
/// this crate has no tracing capture, so nothing else here can either.
#[tokio::test]
async fn a_backlog_raises_one_alarm_for_the_pass_and_not_one_per_window() {
    let provider = harness().await;
    // Adjacent hourly windows on one row, every end already passed — the backlog
    // shape one screen down, at three instead of a thousand. Seeded rather than
    // scheduled: `schedule` refuses an overlap, and adjacency is what keeps three
    // windows on one scope key legal.
    for hour in 0..3_i64 {
        seed_active_window(
            &provider,
            TENANT,
            ROW,
            0x01 + u128::try_from(hour).expect("a positive hour"),
            t(1) + Duration::hours(hour),
            t(1) + Duration::hours(hour + 1),
        )
        .await;
    }

    let (report, metrics) = sweep_watched(&provider, t(10) + Duration::hours(1)).await;

    assert_eq!(
        report.overdue, 3,
        "all three boundaries are late: {report:?}"
    );
    assert_eq!(
        metrics.counter_value(
            "pricing_alarm_total",
            &[
                ("alarm", "pricing.window.activation_overdue"),
                ("severity", "warn"),
            ]
        ),
        1,
        "three overdue windows are one stalled singleton, so one alarm"
    );
}

/// A boundary that has only just arrived is **not** overdue.
///
/// Without the age threshold every window whose instant merely arrived between
/// two ticks would alarm on the tick that flips it — which is every window, once
/// each, on the one pass that behaved perfectly.
#[tokio::test]
async fn a_boundary_that_has_only_just_arrived_is_not_overdue() {
    let provider = harness().await;
    schedule(&provider, TENANT, ROW, 0x01, t(10), Some(t(20))).await;

    let report = sweep(&provider, t(10) + Duration::seconds(1)).await;

    assert_eq!(report.activated, 1);
    assert_eq!(
        report.overdue, 0,
        "one second past the boundary is the sweep working, not the singleton stalling"
    );
}

/// A cancelled window is due for nothing, and neither is a terminal one.
///
/// The due read filters on the state as well as the boundary, and the two
/// filters answer different questions: this one is why a pass does not re-attempt
/// history. Without it a cancelled window whose start has passed would be handed
/// to `transition` as an activation and refused there — the pass would report a
/// failure per cancelled window per tick, forever.
#[tokio::test]
async fn a_cancelled_window_is_due_for_nothing() {
    let provider = harness().await;
    let cancelled = schedule(&provider, TENANT, ROW, 0x01, t(10), Some(t(20))).await;
    {
        // Scoped so the connection is back in the pool before the sweep opens
        // its own transaction on it.
        let conn = provider.conn().expect("scoped connection");
        window_repo::transition(
            &conn,
            &scope_of(TENANT),
            TENANT,
            cancelled,
            WindowState::Cancelled,
            t(1),
            stamp_at(t(1)),
        )
        .await
        .expect("a scheduled window cancels");
    }

    let report = sweep(&provider, t(10)).await;

    assert_eq!(
        report,
        ActivationReport::default(),
        "a cancelled window is not a schedule waiting to happen"
    );
    let (state, _, _, cancelled_at) = window_at_rest(&provider, TENANT, cancelled).await;
    assert_eq!(state, WindowState::Cancelled);
    assert_eq!(cancelled_at, Some(t(1)));
    assert!(events(&provider, TENANT).await.is_empty());
}

// ---------------------------------------------------------------------------
// The page, and the one thing a saturated one must never do
// ---------------------------------------------------------------------------

/// A saturated **expiry** page does not let a changeover's activation through
/// without its expiry.
///
/// The world: a backlog of `DUE_WINDOWS_PER_PASS` expiries on one plan, every one
/// of them older than a changeover on another plan, plus that changeover — the
/// predecessor ending and the successor beginning at one instant, `t(50)`.
///
/// The page is drawn **per pass**, expiries first, and that is what this asserts.
/// Drawn per boundary — two independent capped reads, merged and sorted — the
/// backlog fills the expiry page and the predecessor's expiry, the 1001st oldest,
/// falls outside it, while the activation page holds the successor and is nowhere
/// near its own cap. The pass then emits `PriceWindowActivated` for the successor
/// and **no** `PriceWindowExpired` for the predecessor, and a consumer reads a key
/// held by two `active` windows at once. Sorting cannot save it: the losing row is
/// not in the vector to be sorted.
///
/// So the pass takes **neither** here, and that is the whole difference between a
/// late flip and a lost one: the second pass, at the very same instant, flips both
/// in changeover order. `boundary_rank` orders the pair once they are both read;
/// this is what guarantees they are.
#[tokio::test]
async fn a_saturated_page_defers_a_changeover_rather_than_splitting_it() {
    let provider = harness().await;
    // The changeover, on PLAN: predecessor in force, successor waiting, both
    // meeting at t(50).
    let predecessor = schedule(&provider, TENANT, ROW, 0x01, t(10), Some(t(50))).await;
    let successor = schedule(&provider, TENANT, ROW, 0x02, t(50), Some(t(60))).await;
    sweep(&provider, t(10)).await;

    // The backlog, on PLAN_B: one page's worth of adjacent hourly windows, all
    // `active` with ends already passed — a sweep that has not run for six weeks.
    // Every one of their boundaries is older than t(50), so they win the
    // boundary-ordered page.
    for hour in 0..1_000_i64 {
        seed_active_window(
            &provider,
            TENANT,
            ROW_B,
            0x1_0000 + u128::try_from(hour).expect("a positive hour"),
            t(1) + Duration::hours(hour),
            t(1) + Duration::hours(hour + 1),
        )
        .await;
    }

    let first = sweep(&provider, t(50)).await;

    assert_eq!(
        sequence_of(&provider, TENANT, PLAN).await,
        [(CatalogEvent::PriceWindowActivated.as_str().to_owned(), 0)],
        "the changeover is untouched: the successor was NOT announced while the \
         predecessor still held the key"
    );
    // The counts, and the oldest by presence rather than by identity: this case is
    // about what one saturated page does, and pinning which of a thousand backlog
    // windows happens to be oldest would make it about the alarm instead.
    assert!(
        first.oldest_overdue.is_some(),
        "a pass with overdue windows names one for the operator"
    );
    assert_eq!(
        ActivationReport {
            oldest_overdue: None,
            ..first
        },
        ActivationReport {
            windows_due: 1_000,
            plans_seen: 1,
            expired: 1_000,
            overdue: 1_000,
            ..ActivationReport::default()
        },
        "one page, spent entirely on the backlog: no activation was read, so none \
         could be emitted out of order"
    );
    let (predecessor_state, _, _, _) = window_at_rest(&provider, TENANT, predecessor).await;
    let (successor_state, _, _, _) = window_at_rest(&provider, TENANT, successor).await;
    assert_eq!(predecessor_state, WindowState::Active);
    assert_eq!(
        successor_state,
        WindowState::Scheduled,
        "and no key is held by two active windows"
    );

    // The same instant again. The backlog is gone, so the page has room and the
    // changeover crosses whole, in order.
    let second = sweep(&provider, t(50)).await;

    assert_eq!(
        second,
        ActivationReport {
            windows_due: 2,
            plans_seen: 1,
            activated: 1,
            expired: 1,
            ..ActivationReport::default()
        },
        "deferred by one pass, not dropped"
    );
    assert_eq!(
        sequence_of(&provider, TENANT, PLAN).await,
        [
            (CatalogEvent::PriceWindowActivated.as_str().to_owned(), 0),
            (CatalogEvent::PriceWindowExpired.as_str().to_owned(), 1),
            (CatalogEvent::PriceWindowActivated.as_str().to_owned(), 2),
        ],
        "the predecessor's end reaches the consumer before the successor's start"
    );
}

// ---------------------------------------------------------------------------
// The row the window hangs off: the sweep is on the published side
// ---------------------------------------------------------------------------

/// A window on a **never-published draft** row is not flipped, and nothing is
/// announced.
///
/// `PriceWindowActivated` states that a price took effect. A draft row is
/// addressable at no `CatalogVersion` and resolvable by no pin, so there is no
/// consumer for whom the statement could be true — and the sweep's only reader,
/// the plan-subject projection, excludes exactly these rows (D-121,
/// `PROJECTED_ROW_STATES`). This is therefore not a seam between two consumers
/// but a producer agreeing with its single reader.
///
/// The **outbox count** is asserted beside the row state on purpose: an
/// `activated == 0` on its own is satisfied by any filter that happens to drop
/// the row, including one that drops it for the wrong reason after writing the
/// event.
#[tokio::test]
async fn a_window_on_a_never_published_row_is_not_flipped() {
    let provider = harness().await;
    let window = schedule(&provider, TENANT, ROW_DRAFT, 0x01, t(10), Some(t(20))).await;

    let report = sweep(&provider, t(10)).await;

    assert_eq!(
        report,
        ActivationReport::default(),
        "a draft row's window is not even due: it is absent from the read, so it raises no overdue \
         alarm either"
    );
    let (state, activated_at, _, _) = window_at_rest(&provider, TENANT, window).await;
    assert_eq!(state, WindowState::Scheduled);
    assert_eq!(
        activated_at, None,
        "nothing took effect, so nothing is stamped"
    );
    assert!(
        events(&provider, TENANT).await.is_empty(),
        "no event announces an interval no published row and no CatalogVersion backs"
    );
}

/// A window whose interval passed **in full** while its row was a draft is still
/// `scheduled`, and therefore still flips once the row publishes.
///
/// This is the case that makes refusing to flip correct rather than lossy, and it
/// is the one the counter-argument rests on. `expired` is terminal —
/// `WindowState::may_move_to` has no edge back to `scheduled` or `active` — so a
/// sweep that had flipped this window while its row was a draft would have driven
/// it to terminal `expired` before the row ever published, and the window would
/// have **permanently** lost the ability to take effect. Refusing instead costs
/// one tick of lateness, and the lateness is visible: the pass that does flip it
/// counts it `overdue`, which is the Warn alarm §7 names.
#[tokio::test]
async fn a_window_whose_interval_passed_as_a_draft_still_flips_once_the_row_publishes() {
    let provider = harness().await;
    let window = schedule(&provider, TENANT, ROW_DRAFT, 0x01, t(10), Some(t(20))).await;

    // Twenty days after the window's own end, with the row still a draft.
    let unpublished = sweep(&provider, t(40)).await;
    assert_eq!(
        unpublished,
        ActivationReport::default(),
        "the whole interval passed and the sweep did nothing with it"
    );
    let (state, _, _, _) = window_at_rest(&provider, TENANT, window).await;
    assert_eq!(
        state,
        WindowState::Scheduled,
        "and it is still scheduled - nothing terminal happened to it"
    );

    flip_lifecycle(&provider, TENANT, ROW_DRAFT, LifecycleState::Published).await;

    let activation = sweep(&provider, t(40)).await;
    assert_eq!(
        activation,
        ActivationReport {
            windows_due: 1,
            plans_seen: 1,
            activated: 1,
            overdue: 1,
            // Named because the alarm is raised once for the pass and this is the
            // window an operator is pointed at.
            oldest_overdue: Some((TENANT, t(10), window)),
            ..ActivationReport::default()
        },
        "the very next tick flips it, and reports it as late while doing so"
    );
    let (state, activated_at, _, _) = window_at_rest(&provider, TENANT, window).await;
    assert_eq!(state, WindowState::Active);
    assert_eq!(
        activated_at,
        Some(t(10)),
        "stamped where section 4 puts it, at the boundary it was the arrival of"
    );

    let expiry = sweep(&provider, t(40)).await;
    assert_eq!(
        expiry,
        ActivationReport {
            windows_due: 1,
            plans_seen: 1,
            expired: 1,
            overdue: 1,
            oldest_overdue: Some((TENANT, t(20), window)),
            ..ActivationReport::default()
        }
    );
    let (state, _, expired_at, _) = window_at_rest(&provider, TENANT, window).await;
    assert_eq!(state, WindowState::Expired);
    assert_eq!(expired_at, Some(t(20)));
    assert_eq!(
        sequence_of(&provider, TENANT, PLAN).await,
        [
            (CatalogEvent::PriceWindowActivated.as_str().to_owned(), 0),
            (CatalogEvent::PriceWindowExpired.as_str().to_owned(), 1),
        ],
        "both events reach the consumer, late and in order, rather than neither ever"
    );
}

/// A **superseded** row's active window still expires.
///
/// The other half of `PROJECTED_ROW_STATES`, and the assertion that stops the
/// fix from silently narrowing to `published` alone. `superseded` is the
/// load-bearing member: rating pins *current* versions and rates *past*
/// instants, so a changeover's predecessor — superseded, its window expiring —
/// is exactly the row a legitimately covered arrears period resolves against. A
/// sweep that stopped expiring its windows would leave the predecessor `active`
/// forever and hand a consumer a key held by two windows at once.
#[tokio::test]
async fn a_superseded_rows_active_window_still_expires() {
    let provider = harness().await;
    let window = schedule(&provider, TENANT, ROW, 0x01, t(10), Some(t(20))).await;

    sweep(&provider, t(10)).await;
    flip_lifecycle(&provider, TENANT, ROW, LifecycleState::Superseded).await;

    let report = sweep(&provider, t(20)).await;

    assert_eq!(
        report,
        ActivationReport {
            windows_due: 1,
            plans_seen: 1,
            expired: 1,
            ..ActivationReport::default()
        }
    );
    let (state, _, expired_at, _) = window_at_rest(&provider, TENANT, window).await;
    assert_eq!(state, WindowState::Expired);
    assert_eq!(expired_at, Some(t(20)));
    assert_eq!(
        sequence_of(&provider, TENANT, PLAN).await,
        [
            (CatalogEvent::PriceWindowActivated.as_str().to_owned(), 0),
            (CatalogEvent::PriceWindowExpired.as_str().to_owned(), 1),
        ],
        "the predecessor's end reaches the consumer that has to stop resolving it"
    );
}
