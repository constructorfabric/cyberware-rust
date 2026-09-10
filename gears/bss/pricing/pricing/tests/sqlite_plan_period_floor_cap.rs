//! `pricing_plan_period_floor_cap` against a real database (**D-319**) — the
//! market key, the five `CHECK`s that make a bound usable, the append-only
//! triggers, and the repository statements that write the set.
//!
//! Everything worth proving here is a property of the **schema**, of a
//! **statement**, or of what the repository leaves behind — never of a branch in
//! Rust. A mock would assert that the repository's own `if` fires and would keep
//! asserting it after the constraint that matters had been deleted.
//!
//! The suite carries **two** harnesses, as its three sibling suites do. The
//! constraint and trigger cases drive raw SQL through `common::migrated_db`,
//! because a bound row under a published revision is a state no typed path will
//! ever produce and the guards exist precisely for what reaches the table
//! outside this gear. The repository cases drive `PlanShapeRepo`.
//!
//! **Why the five `CHECK`s are here and the publish rules are not.** Each of
//! them has a `PERIOD_FLOOR_CAP_AMOUNT_INVALID` rule that explains it in a
//! report line (S2 `inst-pfc-amount`, D-148's arrangement), and those rules are
//! unit-tested where they live. What this file proves is the half a report line
//! cannot: that the row is refused **by the store**, so a bound that admits no
//! bill cannot exist even if every rule above it were deleted. The market rule
//! (`inst-pfc-market`) has no counterpart here on purpose — "the plan sells this
//! market" is a fact about the plan's price rows, which no constraint on this
//! table can see.
//!
//! Postgres mirrors every rule here as one PL/pgSQL trigger function and the
//! same `CHECK`s, index, FK and PK — `tests/postgres_migrations.rs` asserts all
//! of them by name on that engine — so no testcontainers case is added; see the
//! migration's module doc for the transform.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use bss_pricing::domain::concurrency::RowVersion;
use bss_pricing::domain::money::{CurrencyCode, MinorAmount};
use bss_pricing::domain::plan_shape::PeriodFloorCap;
use bss_pricing::domain::scope_key::{PlanId, Region};
use bss_pricing::infra::storage::migrations::Migrator;
use bss_pricing::infra::storage::repo::{NewPlanDraft, PlanRepo, PlanShapeRepo};

use bss_pricing::domain::instant::utc_ymd_hms;
use sea_orm::DatabaseConnection;
use sea_orm_migration::MigratorTrait;
use time::OffsetDateTime;
use toolkit_db::migration_runner::run_migrations_for_testing;
use toolkit_db::secure::AccessScope;
use toolkit_db::{ConnectOpts, DBProvider, DbError, connect_db};
use uuid::Uuid;

mod common;

use common::{exec, migrated_db, must_succeed, scalar};

const TEST_CORRELATION: Uuid = Uuid::from_u128(0x_c0_11_a7_11);

const TENANT: &str = "11111111-1111-1111-1111-111111111111";
const PLAN: &str = "22222222-2222-2222-2222-222222222222";
const ACTOR: &str = "44444444-4444-4444-4444-444444444444";
const AUTHORED: &str = "2026-08-15 10:00:00 +00:00";

/// Reject, **and** for the stated reason.
///
/// The fragment is not decoration: this table carries a four-column primary key,
/// a composite foreign key, five `CHECK`s and three append-only triggers, so a
/// test that accepted any error naming the table would pass with the guard it
/// means to prove switched off — refused instead by a constraint it never
/// intended to trip. That is the failure mode the whole suite exists to avoid,
/// and with five `CHECK`s on one row it is not hypothetical: a `0` floor and an
/// inverted pair are one edit apart.
async fn must_be_rejected(conn: &DatabaseConnection, sql: &str, because: &str) {
    let err = exec(conn, sql)
        .await
        .expect_err(&format!("this statement must be rejected: {sql}"));
    let message = err.to_string();
    assert!(
        message.contains("pricing_plan_period_floor_cap"),
        "the rejection must name the guard it came from, got: {message}"
    );
    assert!(
        message.contains(because),
        "the rejection must be the one under test (`{because}`), got: {message}"
    );
}

async fn insert_revision(conn: &DatabaseConnection, revision: i64) {
    must_succeed(
        conn,
        &format!(
            "INSERT INTO pricing_plan (
                plan_id, revision, tenant_id, lifecycle_state, created_by, created_at_utc)
             VALUES ('{PLAN}', {revision}, '{TENANT}', 'draft', '{ACTOR}', '{AUTHORED}')"
        ),
    )
    .await;
}

/// Move a revision out of `draft`. Both flips this suite uses —
/// `draft -> published` and `draft -> abandoned` — are whitelisted by
/// `pricing_plan`'s own append-only trigger.
async fn flip(conn: &DatabaseConnection, revision: i64, state: &str) {
    must_succeed(
        conn,
        &format!(
            "UPDATE pricing_plan SET lifecycle_state = '{state}' \
             WHERE plan_id = '{PLAN}' AND revision = {revision}"
        ),
    )
    .await;
}

/// A bound row's INSERT, with both amounts written literally so a case can put
/// any pair on the table.
fn insert_bound(revision: i64, currency: &str, region: &str, floor: &str, cap: &str) -> String {
    format!(
        "INSERT INTO pricing_plan_period_floor_cap (
            plan_id, plan_revision, currency, region, tenant_id, floor_minor, cap_minor)
         VALUES ('{PLAN}', {revision}, '{currency}', '{region}', '{TENANT}', {floor}, {cap})"
    )
}

async fn count_bounds(conn: &DatabaseConnection, revision: i64) -> String {
    scalar(
        conn,
        &format!(
            "SELECT CAST(count(*) AS TEXT) AS v FROM pricing_plan_period_floor_cap \
             WHERE plan_id = '{PLAN}' AND plan_revision = {revision}"
        ),
    )
    .await
}

// ---------------------------------------------------------------------------
// The key.
// ---------------------------------------------------------------------------

/// The market pair is the key, so a revision holds at most one bound per market
/// and any number of markets.
///
/// Both halves matter and they pull against each other: without the market in
/// the key a plan could carry one bound in total, and without the *whole* market
/// in the key two currencies in one region would collide. The second insert
/// below differs from the first only in `region` and the third only in
/// `currency`.
#[tokio::test]
async fn a_revision_holds_one_bound_per_market_and_any_number_of_markets() {
    let conn = migrated_db().await;
    insert_revision(&conn, 0).await;

    must_succeed(&conn, &insert_bound(0, "USD", "us", "50000", "NULL")).await;
    must_succeed(&conn, &insert_bound(0, "USD", "ca", "40000", "NULL")).await;
    must_succeed(&conn, &insert_bound(0, "EUR", "us", "45000", "NULL")).await;
    assert_eq!(count_bounds(&conn, 0).await, "3");

    // And the same market twice is one bound with two amounts, which is an
    // ambiguity about what the plan's minimum is rather than a second minimum.
    must_be_rejected(
        &conn,
        &insert_bound(0, "USD", "us", "60000", "NULL"),
        "UNIQUE",
    )
    .await;
}

/// A bound cannot hang off a revision that does not exist — the revision number
/// included, not just the plan.
///
/// **The append-only trigger answers, not the foreign key**, and that is a fact
/// about this engine rather than about the rule: the trigger's predicate ("a
/// *draft* revision with this key exists") strictly implies the key's, so on the
/// insert path the trigger can never be reached second. The key is therefore
/// asserted where it can be seen — as a declaration — and its refusal is
/// exercised on Postgres, where `postgres_schema_plan_shape.rs` names
/// `fk_pricing_plan_period_floor_cap_revision` directly. This is
/// `sqlite_plan_descriptor_set.rs`'s arrangement one table over, and the same
/// asymmetry.
#[tokio::test]
async fn a_bound_cannot_name_a_revision_that_does_not_exist() {
    let conn = migrated_db().await;
    insert_revision(&conn, 0).await;

    must_be_rejected(
        &conn,
        &insert_bound(7, "USD", "us", "50000", "NULL"),
        "under a non-draft plan revision is not permitted",
    )
    .await;

    let declared = scalar(
        &conn,
        "SELECT group_concat(\"from\" || '->' || \"to\", ',') AS v
           FROM pragma_foreign_key_list('pricing_plan_period_floor_cap')
          WHERE \"table\" = 'pricing_plan'",
    )
    .await;
    assert!(
        declared.contains("plan_id->plan_id") && declared.contains("plan_revision->revision"),
        "the composite FK must cover both key columns, got: {declared}"
    );
}

// ---------------------------------------------------------------------------
// The five CHECKs — four are a bound that admits no bill, the fifth is the
// currency's width.
// ---------------------------------------------------------------------------

/// `0` is refused, and the positive control is that `1` is not.
///
/// The control is the point of the case rather than politeness: a `CHECK`
/// written `floor_minor > 0` and one written `floor_minor > 1000` both refuse
/// the zero, and only one of them is the rule. **Absence** is the third arm, and
/// it is what a plan with no minimum says — asserted here because the whole
/// argument for refusing `0` is that the state it was reaching for is
/// expressible another way.
#[tokio::test]
async fn a_zero_floor_is_refused_while_one_minor_unit_and_absence_are_not() {
    let conn = migrated_db().await;
    insert_revision(&conn, 0).await;

    must_be_rejected(
        &conn,
        &insert_bound(0, "USD", "us", "0", "NULL"),
        "chk_pricing_plan_period_floor_cap_floor_positive",
    )
    .await;

    must_succeed(&conn, &insert_bound(0, "USD", "us", "1", "NULL")).await;
    must_succeed(&conn, &insert_bound(0, "USD", "ca", "NULL", "1")).await;
    assert_eq!(count_bounds(&conn, 0).await, "2");
}

/// A negative floor is refused by the same `CHECK`, and it is worth its own case
/// because the domain type would have refused it one layer up: this asserts the
/// store refuses it even when nothing above the store does.
#[tokio::test]
async fn a_negative_bound_is_refused_on_both_columns() {
    let conn = migrated_db().await;
    insert_revision(&conn, 0).await;

    must_be_rejected(
        &conn,
        &insert_bound(0, "USD", "us", "-1", "NULL"),
        "chk_pricing_plan_period_floor_cap_floor_positive",
    )
    .await;
    must_be_rejected(
        &conn,
        &insert_bound(0, "USD", "us", "NULL", "-1"),
        "chk_pricing_plan_period_floor_cap_cap_positive",
    )
    .await;
}

#[tokio::test]
async fn a_zero_cap_is_refused_while_a_positive_one_is_not() {
    let conn = migrated_db().await;
    insert_revision(&conn, 0).await;

    must_be_rejected(
        &conn,
        &insert_bound(0, "USD", "us", "NULL", "0"),
        "chk_pricing_plan_period_floor_cap_cap_positive",
    )
    .await;
    must_succeed(&conn, &insert_bound(0, "USD", "us", "NULL", "500000")).await;
}

/// The ordering `CHECK`, pinned on both sides of its boundary.
///
/// Equality passes and one minor unit over fails: a plan that bills exactly X
/// per period is a fixed-fee plan, not a contradiction, and a `CHECK` written
/// `<` instead of `<=` would refuse it while passing every other case here.
#[tokio::test]
async fn a_floor_above_its_cap_is_refused_and_equality_is_not() {
    let conn = migrated_db().await;
    insert_revision(&conn, 0).await;

    must_be_rejected(
        &conn,
        &insert_bound(0, "USD", "us", "50001", "50000"),
        "chk_pricing_plan_period_floor_cap_ordered",
    )
    .await;
    must_succeed(&conn, &insert_bound(0, "USD", "us", "50000", "50000")).await;
}

/// **The ordering `CHECK` names both NULL arms, and this is why.**
///
/// Written as the bare `floor_minor <= cap_minor`, SQL's NULL propagation makes
/// the comparison NULL — which both engines count as **satisfied** — so a
/// floor-only bound and a cap-only bound would pass a constraint that had
/// stopped being evaluated at all. That is the exact shape
/// `chk_pricing_plan_phase_display_trial_days` fell into (D-151), and the only
/// way to tell the two spellings apart is to assert that the one-sided rows are
/// accepted *and* that the two-sided contradiction is still refused. Both are
/// above; what this case adds is the pair on one revision, so the constraint is
/// exercised with each column NULL in turn.
/// A currency that is not three characters is refused, and the positive control
/// is that a three-letter code is not.
///
/// The control is the case rather than politeness, for the reason its four
/// siblings state: a `CHECK` written `length(currency) = 3` and one written
/// `length(currency) > 0` both refuse the two-letter code, and only one of them
/// is the rule. Both the short and the long side are asserted because
/// `length(...) = 3` is an equality and a `>` would pass one of them.
#[tokio::test]
async fn a_currency_that_is_not_three_characters_is_refused_on_either_side() {
    let conn = migrated_db().await;
    insert_revision(&conn, 0).await;

    must_be_rejected(
        &conn,
        &insert_bound(0, "US", "us", "1", "NULL"),
        "chk_pricing_plan_period_floor_cap_currency",
    )
    .await;
    must_be_rejected(
        &conn,
        &insert_bound(0, "USDD", "us", "1", "NULL"),
        "chk_pricing_plan_period_floor_cap_currency",
    )
    .await;

    must_succeed(&conn, &insert_bound(0, "USD", "us", "1", "NULL")).await;
    assert_eq!(count_bounds(&conn, 0).await, "1");
}

#[tokio::test]
async fn a_one_sided_bound_is_accepted_on_either_side() {
    let conn = migrated_db().await;
    insert_revision(&conn, 0).await;

    must_succeed(&conn, &insert_bound(0, "USD", "us", "50000", "NULL")).await;
    must_succeed(&conn, &insert_bound(0, "USD", "ca", "NULL", "50000")).await;
    assert_eq!(count_bounds(&conn, 0).await, "2");
}

/// A row authoring neither bound says nothing, and the store refuses it.
#[tokio::test]
async fn a_bound_authoring_neither_amount_is_refused() {
    let conn = migrated_db().await;
    insert_revision(&conn, 0).await;

    must_be_rejected(
        &conn,
        &insert_bound(0, "USD", "us", "NULL", "NULL"),
        "chk_pricing_plan_period_floor_cap_present",
    )
    .await;
}

// ---------------------------------------------------------------------------
// Append-only with the revision.
// ---------------------------------------------------------------------------

/// Every verb is guarded once the revision leaves `draft`, INSERT included.
///
/// INSERT is the one that is easy to leave out and the one that matters most:
/// it is how a bound would be **added** to a revision a publish has already
/// frozen — a minimum nobody approved, on a plan whose `pricing_plan` row never
/// moved.
#[tokio::test]
async fn a_published_revisions_bounds_take_no_insert_no_update_and_no_delete() {
    let conn = migrated_db().await;
    insert_revision(&conn, 0).await;
    must_succeed(&conn, &insert_bound(0, "USD", "us", "50000", "NULL")).await;
    flip(&conn, 0, "published").await;

    must_be_rejected(
        &conn,
        &insert_bound(0, "EUR", "de", "40000", "NULL"),
        "INSERT",
    )
    .await;
    must_be_rejected(
        &conn,
        &format!(
            "UPDATE pricing_plan_period_floor_cap SET floor_minor = 999999 \
             WHERE plan_id = '{PLAN}' AND plan_revision = 0"
        ),
        "UPDATE",
    )
    .await;
    must_be_rejected(
        &conn,
        &format!(
            "DELETE FROM pricing_plan_period_floor_cap \
             WHERE plan_id = '{PLAN}' AND plan_revision = 0"
        ),
        "DELETE",
    )
    .await;

    assert_eq!(count_bounds(&conn, 0).await, "1");
}

/// **Re-pointing `plan_revision` is how one appends to a frozen revision without
/// issuing an INSERT**, so the UPDATE arm checks the row's destination as well
/// as its origin.
///
/// The `OLD` parent here is a legal `draft`, so an UPDATE arm that consulted
/// only the row's current revision would accept this and move a bound onto a
/// published one.
///
/// The seeding order is forced by `uq_pricing_plan_open_draft`, which admits one
/// draft per plan: revision 0 is written, given its bound and **published**
/// first, and only then can revision 1 be opened as the plan's one draft.
#[tokio::test]
async fn a_bound_may_not_be_moved_onto_a_frozen_revision() {
    let conn = migrated_db().await;
    insert_revision(&conn, 0).await;
    flip(&conn, 0, "published").await;
    insert_revision(&conn, 1).await;
    must_succeed(&conn, &insert_bound(1, "USD", "us", "50000", "NULL")).await;

    must_be_rejected(
        &conn,
        &format!(
            "UPDATE pricing_plan_period_floor_cap SET plan_revision = 0 \
             WHERE plan_id = '{PLAN}' AND plan_revision = 1"
        ),
        "UPDATE",
    )
    .await;
}

/// **`abandoned` is not `draft`**, which is what forces `abandon_draft` to drop
/// these rows *before* it flips the revision.
#[tokio::test]
async fn an_abandoned_revisions_bounds_can_no_longer_be_dropped() {
    let conn = migrated_db().await;
    insert_revision(&conn, 0).await;
    must_succeed(&conn, &insert_bound(0, "USD", "us", "50000", "NULL")).await;
    flip(&conn, 0, "abandoned").await;

    must_be_rejected(
        &conn,
        &format!(
            "DELETE FROM pricing_plan_period_floor_cap \
             WHERE plan_id = '{PLAN}' AND plan_revision = 0"
        ),
        "DELETE",
    )
    .await;
}

// ---------------------------------------------------------------------------
// The repository.
// ---------------------------------------------------------------------------

fn at(day: u32) -> OffsetDateTime {
    utc_ymd_hms(2026, 8, day, 10, 0, 0)
}

fn stamp() -> bss_pricing::domain::audit::AuditStamp {
    bss_pricing::domain::audit::AuditStamp {
        actor_principal_id: Uuid::from_u128(0xac_10),
        recorded_at: at(15),
        correlation_id: TEST_CORRELATION,
    }
}

fn bound(currency: &str, region: &str, floor: Option<i64>, cap: Option<i64>) -> PeriodFloorCap {
    PeriodFloorCap {
        currency: CurrencyCode::new(currency).expect("three letters"),
        region: Region::new(region).expect("non-blank"),
        floor_minor: floor.map(|v| MinorAmount::new(v).expect("non-negative")),
        cap_minor: cap.map(|v| MinorAmount::new(v).expect("non-negative")),
    }
}

async fn harness() -> (PlanRepo, PlanShapeRepo) {
    let db = connect_db("sqlite::memory:", ConnectOpts::default())
        .await
        .expect("connect in-memory sqlite");
    run_migrations_for_testing(&db, Migrator::migrations())
        .await
        .expect("run migrator");
    let provider = DBProvider::<DbError>::new(db);
    (
        PlanRepo::new(provider.clone()),
        PlanShapeRepo::new(provider),
    )
}

/// The set round-trips, in `(currency, region)` order, and a replace is
/// wholesale.
///
/// The read order is asserted rather than sorted at the call site because
/// `PERIOD_FLOOR_CAP_MARKET_UNSOLD`'s report enumerates the offending bounds,
/// and a set that came back differently on each read would make that report
/// unreproducible.
#[tokio::test]
async fn the_bound_set_round_trips_in_market_order_and_replaces_wholesale() {
    let (plans, shapes) = harness().await;
    let tenant = Uuid::from_u128(0x7e_11);
    let scope = AccessScope::for_tenant(tenant);
    let plan_id = PlanId::new(Uuid::from_u128(0x9_1a4));
    plans
        .create_draft(
            &scope,
            NewPlanDraft {
                plan_id,
                tenant_id: tenant,
                created_by: Uuid::from_u128(0xac_10),
                created_at_utc: at(15),
                sku_id: None,
                plan_tier: None,
                plan_name: None,
                billing_cycle: None,
                frequency: None,
                plan_tier_override: false,
                purchase_min_qty: None,
                purchase_max_qty: None,
                invoice_grouping_key: None,
                available_from: None,
                available_to: None,
                cloned_from: None,
                correlation_id: TEST_CORRELATION,
            },
        )
        .await
        .expect("create the draft");

    let written = shapes
        .replace_period_floor_caps(
            &scope,
            tenant,
            plan_id,
            0,
            RowVersion::new(0),
            vec![
                bound("USD", "us", Some(50_000), Some(500_000)),
                bound("EUR", "de", Some(40_000), None),
            ],
            stamp(),
        )
        .await
        .expect("author the bound set");

    assert_eq!(
        shapes
            .list_period_floor_caps(&scope, tenant, plan_id, 0)
            .await
            .expect("read the bound set"),
        vec![
            bound("EUR", "de", Some(40_000), None),
            bound("USD", "us", Some(50_000), Some(500_000)),
        ],
        "the set comes back in (currency, region) order, both amounts intact"
    );

    // Wholesale: re-sending one market leaves one, which is also the only way an
    // author withdraws a bound.
    shapes
        .replace_period_floor_caps(
            &scope,
            tenant,
            plan_id,
            0,
            written.row_version,
            vec![bound("USD", "us", Some(60_000), None)],
            stamp(),
        )
        .await
        .expect("replace the bound set");

    assert_eq!(
        shapes
            .list_period_floor_caps(&scope, tenant, plan_id, 0)
            .await
            .expect("read the bound set"),
        vec![bound("USD", "us", Some(60_000), None)],
        "a replace is wholesale, so the EUR bound is gone and the USD one moved"
    );
}

/// The empty set is a valid request, and it is how every bound is withdrawn.
///
/// Asserted because the repository could reasonably have refused it — an empty
/// phase set is refused at *publish* by `inst-ph-graph`, and a reader who
/// transplanted that reflex here would make "this plan has no minimum" an
/// unreachable state on a plan that once had one.
#[tokio::test]
async fn an_empty_bound_set_is_how_every_bound_is_withdrawn() {
    let (plans, shapes) = harness().await;
    let tenant = Uuid::from_u128(0x7e_11);
    let scope = AccessScope::for_tenant(tenant);
    let plan_id = PlanId::new(Uuid::from_u128(0x9_1a4));
    plans
        .create_draft(
            &scope,
            NewPlanDraft {
                plan_id,
                tenant_id: tenant,
                created_by: Uuid::from_u128(0xac_10),
                created_at_utc: at(15),
                sku_id: None,
                plan_tier: None,
                plan_name: None,
                billing_cycle: None,
                frequency: None,
                plan_tier_override: false,
                purchase_min_qty: None,
                purchase_max_qty: None,
                invoice_grouping_key: None,
                available_from: None,
                available_to: None,
                cloned_from: None,
                correlation_id: TEST_CORRELATION,
            },
        )
        .await
        .expect("create the draft");

    let written = shapes
        .replace_period_floor_caps(
            &scope,
            tenant,
            plan_id,
            0,
            RowVersion::new(0),
            vec![bound("USD", "us", Some(50_000), None)],
            stamp(),
        )
        .await
        .expect("author the bound");

    shapes
        .replace_period_floor_caps(
            &scope,
            tenant,
            plan_id,
            0,
            written.row_version,
            Vec::new(),
            stamp(),
        )
        .await
        .expect("an empty set is a valid request");

    assert!(
        shapes
            .list_period_floor_caps(&scope, tenant, plan_id, 0)
            .await
            .expect("read the bound set")
            .is_empty(),
        "the plan is back to having no minimum, which is the ordinary plan"
    );
}

// ---------------------------------------------------------------------------
// The parent revision's tenancy.
// ---------------------------------------------------------------------------

/// A period bound belongs to the tenant its own revision belongs to.
///
/// The third of the four parent-tenancy arms, and the one this case executes. Same
/// shape as its two siblings: the composite foreign key covers
/// `(plan_id, plan_revision)` alone while the table is scoped by `tenant_id`, and
/// the two trigger rosters plus the two schema goldens pin the arm's text without
/// saying whether it refuses.
///
/// Pinned by the guard's own message fragment, which on a table carrying five
/// `CHECK`s is not a nicety: the table name alone is shared by all of them.
#[tokio::test]
async fn a_bound_may_not_claim_a_tenant_its_revision_does_not_belong_to() {
    const TENANT_TWO: &str = "11111111-1111-1111-1111-1111111111b2";
    let conn = migrated_db().await;
    insert_revision(&conn, 0).await;

    must_be_rejected(
        &conn,
        &format!(
            "INSERT INTO pricing_plan_period_floor_cap (
                plan_id, plan_revision, currency, region, tenant_id, floor_minor, cap_minor)
             VALUES ('{PLAN}', 0, 'USD', 'us', '{TENANT_TWO}', 50000, 500000)"
        ),
        "belongs to another tenant and may not hold this row",
    )
    .await;
    assert_eq!(
        count_bounds(&conn, 0).await,
        "0",
        "the foreign-tenant row must not have landed"
    );

    // The revision's own tenant lands: a legal pair, so the refusal above is about
    // the tenant and not about the amounts.
    must_succeed(&conn, &insert_bound(0, "USD", "us", "50000", "500000")).await;

    must_be_rejected(
        &conn,
        &format!(
            "UPDATE pricing_plan_period_floor_cap SET tenant_id = '{TENANT_TWO}' \
             WHERE plan_id = '{PLAN}' AND plan_revision = 0"
        ),
        "belongs to another tenant and may not hold this row",
    )
    .await;
}
