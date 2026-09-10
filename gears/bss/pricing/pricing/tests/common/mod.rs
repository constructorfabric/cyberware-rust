//! The database-under-test, the three verbs the schema suites drive it with, and
//! the one window every publishable fixture owes its rows.
//!
//! Everything here is shared because it has **no per-suite content at all**: a
//! migrated `SQLite` database, a statement that must land, a statement whose
//! result is read back, and the coverage window `inst-wc-required` demands of
//! any fixture that publishes. Each of the schema suites once carried its own
//! copy of the first three, and the copies had already stopped agreeing — not
//! about what the helpers do, but about the schema they were describing, so one
//! file said `pricing_price` carried nine `CHECK` constraints while the migration
//! that had grown six more sat two tasks away in the same branch. A helper each
//! suite re-types is a sentence each suite has to keep true on its own.
//!
//! [`schedule_coverage_window`] is here for the sharper version of that reason.
//! Four seeds across four suites publish a plan, and each of them now owes its
//! rows a window. Four copies of that decision would be four places to change the
//! day the rule moves, and four chances for one of them to schedule a window that
//! satisfies the letter of the rule while asserting about a world the system
//! would not hold.
//!
//! What is deliberately **not** here is `must_be_rejected`. Every suite asserts
//! that a refusal is *the one under test* — a raw "some error happened" would
//! pass with the guard it means to prove switched off — and the fragment that
//! makes that assertion sharp is different in each: the table name for the two
//! trigger suites, the constraint name for the CHECK suite. Hoisting them into
//! one helper would mean taking the weakest of the three, which is how a suite
//! ends up green against a schema that no longer holds.
//!
//! The chain is applied **sorted by migration name**, which is the order the
//! `Migrator` itself defines and which `tests/module_test.rs` pins: a table's
//! foreign key and every trigger that reads a parent depend on that parent
//! existing, so a suite that applied the chain in declaration order would fail
//! for a reason having nothing to do with what it is testing.

#![allow(
    dead_code,
    reason = "each test binary compiles the whole module and uses part of it"
)]
#![allow(clippy::expect_used, clippy::unwrap_used)]

use sea_orm::ActiveValue::Set;
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, EntityTrait};
use sea_orm::{ConnectionTrait, Database, DatabaseConnection, Statement};
use sea_orm_migration::{MigrationTrait, MigratorTrait, SchemaManager};
use toolkit_db::secure::{AccessScope, DBRunner, SecureInsertExt, SecureUpdateExt};
use toolkit_db::{DBProvider, DbError};
use uuid::Uuid;

use bss_pricing::domain::audit::AuditStamp;
use bss_pricing::domain::instant::utc_ymd_hms;
use bss_pricing::domain::lifecycle::LifecycleState;
use bss_pricing::infra::storage::entity::{plan, price, region_taxonomy};
use bss_pricing::infra::storage::migrations::Migrator;
use bss_pricing::infra::storage::repo::window_repo::{NewWindow, WindowRecord, schedule};
use time::OffsetDateTime;

/// An in-memory `SQLite` database carrying the whole migration chain.
pub async fn migrated_db() -> DatabaseConnection {
    let conn = Database::connect("sqlite::memory:")
        .await
        .expect("connect in-memory sqlite");
    let manager = SchemaManager::new(&conn);
    let mut chain: Vec<Box<dyn MigrationTrait>> = Migrator::migrations();
    chain.sort_by(|a, b| a.name().cmp(b.name()));
    for migration in &chain {
        migration
            .up(&manager)
            .await
            .unwrap_or_else(|e| panic!("up {} must succeed: {e}", migration.name()));
    }
    conn
}

/// Run one statement and hand back whatever the driver said about it.
///
/// # Errors
/// Whatever the driver refused with — which is the point: the schema suites are
/// about which rejections the database produces.
pub async fn exec(conn: &DatabaseConnection, sql: &str) -> Result<(), sea_orm::DbErr> {
    conn.execute_raw(Statement::from_string(
        sea_orm::DatabaseBackend::Sqlite,
        sql.to_owned(),
    ))
    .await
    .map(|_| ())
}

/// Run one statement that must land.
///
/// # Panics
/// When the statement is refused. Every suite here proves a *whitelist* rather
/// than a blanket ban, so the moves that are supposed to work are as
/// load-bearing as the ones that are not.
pub async fn must_succeed(conn: &DatabaseConnection, sql: &str) {
    exec(conn, sql)
        .await
        .unwrap_or_else(|e| panic!("statement must succeed: {sql}\n{e}"));
}

/// Read one value back, from a query that aliases it `v`.
///
/// # Panics
/// When the query fails, returns no row, or the value is not text. Assertions
/// on what actually landed are how these suites tell "the guard refused" from
/// "the guard refused and the statement took effect anyway".
pub async fn scalar(conn: &DatabaseConnection, sql: &str) -> String {
    let row = conn
        .query_one_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Sqlite,
            sql.to_owned(),
        ))
        .await
        .expect("query")
        .expect("one row");
    row.try_get::<String>("", "v").expect("read value")
}

// ---------------------------------------------------------------------------
// The coverage every publishable fixture owes (`inst-wc-required`)
// ---------------------------------------------------------------------------

/// Inclusive start of the fixture coverage window: **2099-08-04T00:00:00Z**.
///
/// The window this schedules must be legitimately `scheduled` — created, not yet
/// in force — because a fixture in a state the system would immediately leave is a
/// fixture that asserts about a world the system does not have. **The year is what
/// makes that a fact instead of an expiry date.** It was `2026-08-04` and the
/// argument for it was that this is one day after the instant the publish suites
/// author their plans at (`2026-08-03`) — true inside the suites' own clock, and
/// false against the real one from the very afternoon it was written: the start had
/// already passed, and four weeks later the whole interval would have been in the
/// past, where a sweep against a real clock reads `expired`, `COVERING_STATES`
/// excludes it, and every publishable fixture in the crate fails
/// `inst-wc-required` on a Tuesday somebody has to bisect. `sqlite_window_repo.rs`
/// is the same lesson already learned: it was dated `2026-09` and would have begun
/// failing on `2026-09-15`, which is why its own scale says the year is
/// load-bearing.
///
/// Still a **fixed** date and not an offset from `now`, which was the right call
/// and is unchanged: a fixture computed from the clock asserts something different
/// every day it runs. What changes is that "the future" is now a fact rather than a
/// date that ages.
pub const COVERAGE_FROM_UTC: (i32, u32, u32) = (2099, 8, 4);

/// Exclusive end of the fixture coverage window: **2099-09-01T00:00:00Z**.
///
/// It stops exactly where the window suites' own instant scale starts —
/// `sqlite_read_model.rs`'s `window_at(0)` is `2099-09-01T00:00:00Z` — so **every**
/// window a suite builds on that scale is adjacent to this one or later, and never
/// overlaps it. `window_repo::schedule` refuses an overlap on the key
/// (`WINDOW_OVERLAP`), so an open-ended fixture window would have made the window
/// suites unable to schedule anything at all; adjacency is legal (§9, half-open
/// `[from, to)`) and is the shape a supersession and a cutover both produce.
///
/// **The residue, named rather than left to be hit.** Because this interval sits
/// *before* the window scale, a sweep run at any instant on that scale finds this
/// window due to activate too — so a test that counts activations sees one more
/// than its own case produced. Two tests in `sqlite_read_model.rs` §8/§9 cancel
/// this window for that reason and say so. Dating it *after* the scale instead
/// was rejected: it would then be the far end of every key's coverage and would
/// swamp the `coverageEnd` assertions that are the whole point of those tests.
pub const COVERAGE_TO_UTC: (i32, u32, u32) = (2099, 9, 1);

/// The fixture window's inclusive start, as an instant.
///
/// Public because a suite asserting what a **frozen version** says about this window
/// has to name an instant inside it, and rebuilding the date from
/// [`COVERAGE_FROM_UTC`] at the call site would be a second spelling of the one fact
/// this module exists to keep single.
#[must_use]
pub fn coverage_from() -> OffsetDateTime {
    midnight(COVERAGE_FROM_UTC)
}

/// The end of that coverage, as an instant.
///
/// [`coverage_from`]'s sibling, and it exists for the sharper half of that reason: a
/// suite naming an instant **outside** the fixture's coverage — a dormant key, a
/// changeover past the end — would otherwise rebuild the date from
/// [`COVERAGE_TO_UTC`] at the call site, which is a second spelling of the one fact
/// this module keeps single.
#[must_use]
pub fn coverage_to() -> OffsetDateTime {
    midnight(COVERAGE_TO_UTC)
}

/// The two instants above, as instants.
fn midnight(date: (i32, u32, u32)) -> OffsetDateTime {
    utc_ymd_hms(date.0, date.1, date.2, 0, 0, 0)
}

/// The window id this fixture gives `price_id`'s coverage.
///
/// Derived from the row rather than passed in, so a seed cannot mint two windows
/// on one row by accident and two seeds in one test cannot collide on an id. The
/// salt keeps it out of the `Uuid::from_u128(small)` space every fixture in this
/// crate hand-picks ids from.
#[must_use]
pub fn coverage_window_id(price_id: Uuid) -> Uuid {
    /// Arbitrary and fixed; only its distance from the hand-picked ids matters.
    const SALT: u128 = 0x_c0de_face_0000_0000_0000_0000_0000_0000;
    Uuid::from_u128(price_id.as_u128() ^ SALT)
}

/// Schedule the window `inst-wc-required` demands of a billable row's key.
///
/// **Why every publishable fixture in this crate calls this.** `inst-wc-required`
/// (S7 §3) makes publish fail for a billable row whose canonical scope key holds
/// no active or scheduled `PriceWindow`, and before 2026-08-04 every fixture in
/// every suite published with **zero** windows. Seventy-four tests reddened the
/// day the rule registered. The fix is the fixtures: a published row lives in
/// time, so a fixture that publishes one has to say when it is effective.
///
/// **There is deliberately no way to opt out.** No flag, no "skip coverage"
/// parameter, no test-only bypass — a rule a fixture can opt out of is a rule the
/// next fixture opts out of, and the refusal is the whole content of the definition of done this
/// slice is built for. A suite for which windows are noise calls this and moves
/// on; it does not get an exemption.
///
/// **`window_repo::schedule` and not `POST …/prices/{priceId}/windows`, for
/// isolation and speed.** The route is mounted and works — `rest_windows.rs` drives
/// it, including the sequence that authors a plan's first window through it — so
/// this is a choice rather than a workaround, and it is stated as one because a
/// fixture whose recorded reason is false is worse than one with none. Two grounds:
/// a mutation through the service resolves the plan's *current* revision, and this
/// window has to exist **before** the plan publishes; and every publishing fixture
/// in the crate goes through here, so routing them through the schedule path would
/// make one defect in it redden four suites at once, none of them naming the rule
/// that broke.
///
/// The window is `scheduled` — the only state `window_repo::schedule` creates,
/// §4's initial one — over `[COVERAGE_FROM_UTC, COVERAGE_TO_UTC)`. Both instants
/// are **fixed dates and not offsets from `now`**: a fixture computed from the
/// clock is one that changes what it asserts every day it runs, and a fixture
/// dated far enough ahead to look safe is one that starts failing on a Tuesday
/// somebody has to bisect.
///
/// # Panics
/// When the schedule is refused — an overlap on the key, a row that is not there,
/// or a scope that cannot see it. Each of those is the fixture being wrong about
/// the world rather than the store misbehaving.
pub async fn schedule_coverage_window(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    price_id: Uuid,
    stamp: AuditStamp,
) -> WindowRecord {
    schedule(
        runner,
        scope,
        NewWindow {
            window_id: coverage_window_id(price_id),
            tenant_id,
            price_id,
            effective_from: midnight(COVERAGE_FROM_UTC),
            effective_to: Some(midnight(COVERAGE_TO_UTC)),
            reason_code: "fixtureCoverage".to_owned(),
        },
        stamp,
    )
    .await
    .unwrap_or_else(|e| panic!("schedule the coverage window of price row {price_id}: {e}"))
}

/// Declare the region universe every publishing fixture's rows sell in.
///
/// **Required since `inst-tx-region` was registered in the Foundation rule set.**
/// C2 is fail-closed — a tenant whose region taxonomy declares nothing publishes
/// nothing — so a fixture that publishes a plan has to declare the region its
/// rows carry, exactly as a real operator would through
/// `PUT /config/taxonomies/region`.
///
/// Before the rule was registered these fixtures published rows in regions no
/// tenant had ever declared, which is a world the system is not supposed to be
/// able to reach. That they were green is not evidence the rule was satisfied;
/// it is evidence nothing was asking.
///
/// The set is the union of the spellings the suites use — `region` is
/// case-sensitive and `eu` and `EU` are two different values on the axis.
pub async fn declare_fixture_regions(provider: &DBProvider<DbError>, tenant_id: Uuid) {
    let conn = provider.conn().expect("conn");
    for value in ["eu", "EU", "us", "US", "DE", "us-east"] {
        let row = region_taxonomy::ActiveModel {
            tenant_id: Set(tenant_id),
            value: Set(value.to_owned()),
            display_name: Set(format!("fixture region {value}")),
            state: Set("active".to_owned()),
            // **Both D-01 markers declared**, and not for tidiness:
            // `inst-td-policy` is registered in the Foundation set, its category
            // arm is unconditional (D-154) and C4's rate arm is fail-closed, so a
            // fixture region declaring neither would fail every publish in the
            // crate on a rule none of those suites is about. A real operator
            // declares them in the same `PUT` that declares the region.
            tax_category: Set(Some("standard".to_owned())),
            tax_rate_present: Set(true),
        };
        region_taxonomy::Entity::insert(row.clone())
            .secure()
            .scope_with_model(&AccessScope::allow_all(), &row)
            .expect("scope")
            .exec(&conn)
            .await
            .expect("declare a fixture region");
    }
}

/// Retire one declared fixture region, past the repository's guard.
///
/// Direct because the guard is not what these cases are about: they need a value
/// that *is* `retired`, and reaching it through `PUT /config/taxonomies/region`
/// would make an unrelated refusal there look like the failure under test.
pub async fn retire_fixture_region(provider: &DBProvider<DbError>, tenant_id: Uuid, value: &str) {
    let conn = provider.conn().expect("conn");
    let affected = region_taxonomy::Entity::update_many()
        .secure()
        .scope_with(&AccessScope::allow_all())
        .col_expr(region_taxonomy::Column::State, Expr::value("retired"))
        .filter(
            Condition::all()
                .add(region_taxonomy::Column::TenantId.eq(tenant_id))
                .add(region_taxonomy::Column::Value.eq(value)),
        )
        .exec(&conn)
        .await
        .expect("retire the fixture region");
    assert_eq!(affected.rows_affected, 1, "the fixture region must exist");
}

/// Move a price row to `published` directly, past every door.
///
/// **Fabricated on purpose, and the reason is `sqlite_price_repo::flip_state`'s.**
/// A suite whose subject is something *else* must not depend on the publish
/// engine's four preconditions to put a row in the state its fixture needs — the
/// engine has its own suites, and borrowing it here would make an unrelated
/// failure read as this file's. The flip is legal because the append-only trigger
/// **whitelists** `draft -> published`, not because the trigger is inert on a
/// draft: it has a draft branch on both backends (D-153).
pub async fn publish_row_directly(
    provider: &DBProvider<DbError>,
    scope: &AccessScope,
    price_id: Uuid,
) {
    let conn = provider.conn().expect("conn");
    let result = price::Entity::update_many()
        .secure()
        .scope_with(scope)
        .col_expr(
            price::Column::LifecycleState,
            Expr::value(LifecycleState::Published.as_str()),
        )
        .filter(Condition::all().add(price::Column::PriceId.eq(price_id)))
        .exec(&conn)
        .await
        .expect("publish the seeded row");
    assert_eq!(result.rows_affected, 1, "the seed must have moved one row");
}

/// Move a plan revision to `superseded` directly, past the engine.
///
/// [`publish_plan_directly`] is a blunt UPDATE and does not demote the revision it
/// replaces, so a fixture that wants a plan sitting **past** revision 0 has to
/// demote 0 itself: `uq_pricing_plan_current` is partial over
/// `('published','retired')` and admits one row per plan.
pub async fn supersede_plan_directly(
    provider: &DBProvider<DbError>,
    scope: &AccessScope,
    plan_id: bss_pricing::domain::scope_key::PlanId,
    revision: u64,
) {
    let conn = provider.conn().expect("conn");
    let result = plan::Entity::update_many()
        .secure()
        .scope_with(scope)
        .col_expr(
            plan::Column::LifecycleState,
            Expr::value(LifecycleState::Superseded.as_str()),
        )
        .filter(
            Condition::all()
                .add(plan::Column::PlanId.eq(plan_id.get()))
                .add(plan::Column::Revision.eq(i64::try_from(revision).expect("a small revision"))),
        )
        .exec(&conn)
        .await
        .expect("supersede the seeded plan revision");
    assert_eq!(result.rows_affected, 1, "the seed must have moved one row");
}

/// Move a plan revision to `published` directly, past the engine.
///
/// [`publish_row_directly`]'s reason exactly. It exists so a fixture can make
/// `plan_repo::load_current` answer with a revision — which is what D-212's
/// resolver reads to decide *which* revision of a referencing bundle counts.
pub async fn publish_plan_directly(
    provider: &DBProvider<DbError>,
    scope: &AccessScope,
    plan_id: bss_pricing::domain::scope_key::PlanId,
    revision: u64,
) {
    let conn = provider.conn().expect("conn");
    let result = plan::Entity::update_many()
        .secure()
        .scope_with(scope)
        .col_expr(
            plan::Column::LifecycleState,
            Expr::value(LifecycleState::Published.as_str()),
        )
        .filter(
            Condition::all()
                .add(plan::Column::PlanId.eq(plan_id.get()))
                .add(plan::Column::Revision.eq(i64::try_from(revision).expect("a small revision"))),
        )
        .exec(&conn)
        .await
        .expect("publish the seeded plan revision");
    assert_eq!(result.rows_affected, 1, "the seed must have moved one row");
}
