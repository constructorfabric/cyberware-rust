//! `pricing_plan_addon_rule` against a real database — the key that D-105
//! restored, the constraints, the append-only triggers, and the repository that
//! normalizes conflict edges on the way in.
//!
//! Everything worth proving here is a property of the **schema**, of a
//! **statement**, or of what the repository leaves behind — never of a branch in
//! Rust. The three-column primary key is what lets a revision hold more than one
//! rule at all, the CHECKs are evaluated by the engine, and the three triggers
//! freeze a rule row when its revision publishes. A mock would assert that the
//! repository's own `if` fires and would keep asserting it after the predicate
//! that matters had been deleted.
//!
//! The suite carries **two** harnesses, as `sqlite_plan_phase.rs` does. The
//! constraint and trigger cases drive raw SQL through `common::migrated_db`,
//! because the states they need — a rule row under a published revision, a rule
//! row under an abandoned one — are states no typed path will ever produce and
//! the guards exist precisely for what reaches the table outside this gear. The
//! repository cases drive `PlanShapeRepo`, because what they prove is its:
//! which refusal a caller is told, and what the stored edges look like
//! afterwards.
//!
//! Postgres mirrors every rule here as one PL/pgSQL trigger function and the
//! same CHECKs, index, FK and PK, so no testcontainers case is added; see the
//! migration's module doc for the transform.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use bss_pricing::domain::concurrency::RowVersion;
use bss_pricing::domain::lifecycle::LifecycleState;
use bss_pricing::domain::plan_shape::AddonRule;
use bss_pricing::domain::scope_key::PlanId;
use bss_pricing::infra::storage::RepoError;
use bss_pricing::infra::storage::entity::plan;
use bss_pricing::infra::storage::migrations::Migrator;
use bss_pricing::infra::storage::repo::{NewPlanDraft, PlanRepo, PlanShapeRepo};

use bss_pricing::domain::instant::utc_ymd_hms;
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, DatabaseConnection, EntityTrait};
use sea_orm_migration::MigratorTrait;
use time::OffsetDateTime;
use toolkit_db::migration_runner::run_migrations_for_testing;
use toolkit_db::secure::{AccessScope, SecureUpdateExt};
use toolkit_db::{ConnectOpts, DBProvider, DbError, connect_db};
use uuid::Uuid;

mod common;

use common::{exec, migrated_db, must_succeed, scalar};

/// One value for a whole test binary: these suites drive a repository or a
/// service directly, where the value the HTTP edge would have established has
/// no producer. What each suite asserts *about* it is stated where it asserts
/// it.
const TEST_CORRELATION: uuid::Uuid = uuid::Uuid::from_u128(0x_c0_11_a7_10);

/// The stamp an audited repository call is made under: who acted, when, and the
/// request's correlation.
fn stamp_of(actor: uuid::Uuid, when: OffsetDateTime) -> bss_pricing::domain::audit::AuditStamp {
    bss_pricing::domain::audit::AuditStamp {
        actor_principal_id: actor,
        recorded_at: when,
        correlation_id: TEST_CORRELATION,
    }
}

const TENANT: &str = "11111111-1111-1111-1111-111111111111";
const PLAN: &str = "22222222-2222-2222-2222-222222222222";
const ACTOR: &str = "44444444-4444-4444-4444-444444444444";
const AUTHORED: &str = "2026-08-02 10:00:00 +00:00";

/// Reject, **and** for the stated reason.
///
/// The fragment is not decoration. This table carries three CHECKs, a
/// three-column primary key, a composite foreign key and three append-only
/// triggers, and every one of those refusals names `pricing_plan_addon_rule`. A
/// test that accepted any error naming the table would pass with the guard it
/// means to prove switched off, refused instead by a constraint it never
/// intended to trip.
async fn must_be_rejected(conn: &DatabaseConnection, sql: &str, because: &str) {
    let err = exec(conn, sql)
        .await
        .expect_err(&format!("this statement must be rejected: {sql}"));
    let message = err.to_string();
    assert!(
        message.contains("pricing_plan_addon_rule"),
        "the rejection must name the guard it came from, got: {message}"
    );
    assert!(
        message.contains(because),
        "the rejection must be the one under test (`{because}`), got: {message}"
    );
}

/// Insert one plan revision in `draft`.
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

/// One add-on rule row, spelled out so each case can vary exactly one thing.
struct Rule {
    sku: &'static str,
    revision: i64,
    required: i32,
    min_qty: Option<i32>,
    max_qty: Option<i32>,
    step_qty: Option<i32>,
}

const OPTIONAL: Rule = Rule {
    sku: "33333333-3333-3333-3333-333333333333",
    revision: 0,
    required: 0,
    min_qty: None,
    max_qty: None,
    step_qty: None,
};

fn number(value: Option<i32>) -> String {
    value.map_or_else(|| "NULL".to_owned(), |v| v.to_string())
}

fn insert_rule(rule: &Rule) -> String {
    format!(
        "INSERT INTO pricing_plan_addon_rule (
            plan_id, plan_revision, addon_sku_id, tenant_id,
            required, min_qty, max_qty, step_qty)
         VALUES ('{PLAN}', {}, '{}', '{TENANT}', {}, {}, {}, {})",
        rule.revision,
        rule.sku,
        rule.required,
        number(rule.min_qty),
        number(rule.max_qty),
        number(rule.step_qty),
    )
}

async fn count_rules(conn: &DatabaseConnection, revision: i64) -> String {
    scalar(
        conn,
        &format!(
            "SELECT CAST(count(*) AS TEXT) AS v FROM pricing_plan_addon_rule \
             WHERE plan_id = '{PLAN}' AND plan_revision = {revision}"
        ),
    )
    .await
}

// ---------------------------------------------------------------------------
// The key (D-105) and the constraints.
// ---------------------------------------------------------------------------

/// The D-105 regression, and the reason this table's key has three columns.
///
/// Under the earlier `(plan_id, plan_revision)` key the second insert here
/// collides, so a revision holds **one** add-on rule — and the `depends_on`
/// cycle walk, the symmetric-conflict normalization and "two required
/// conflicting add-ons fail" are then rules over data no plan can ever hold.
/// None of them would have failed a test; they would simply have had nothing to
/// judge.
#[tokio::test]
async fn one_revision_holds_as_many_rules_as_it_has_add_ons_d105() {
    let conn = migrated_db().await;
    insert_revision(&conn, 0).await;

    for sku in [
        "33333333-3333-3333-3333-333333333333",
        "55555555-5555-5555-5555-555555555555",
        "66666666-6666-6666-6666-666666666666",
    ] {
        must_succeed(&conn, &insert_rule(&Rule { sku, ..OPTIONAL })).await;
    }
    assert_eq!(count_rules(&conn, 0).await, "3");

    // And the same add-on twice on one revision is still one rule: the third
    // key column discriminates add-ons, it does not multiply them.
    must_be_rejected(&conn, &insert_rule(&OPTIONAL), "UNIQUE constraint failed").await;
    assert_eq!(count_rules(&conn, 0).await, "3");
}

#[tokio::test]
async fn a_required_add_on_must_admit_at_least_one_of_itself() {
    let conn = migrated_db().await;
    insert_revision(&conn, 0).await;

    // Section 6's `max_qty >= 1 WHERE required`. Both failing shapes: no bound
    // at all, and a bound that admits nothing. A required add-on nobody can take
    // makes the whole plan sellable and unbuyable.
    must_be_rejected(
        &conn,
        &insert_rule(&Rule {
            required: 1,
            ..OPTIONAL
        }),
        "chk_pricing_plan_addon_rule_required_max_qty",
    )
    .await;
    must_be_rejected(
        &conn,
        &insert_rule(&Rule {
            required: 1,
            max_qty: Some(0),
            ..OPTIONAL
        }),
        "chk_pricing_plan_addon_rule_required_max_qty",
    )
    .await;

    // An **optional** add-on may carry no bound at all - the constraint is
    // conditional, and a blanket `max_qty >= 1` would forbid the ordinary case.
    must_succeed(&conn, &insert_rule(&OPTIONAL)).await;
    must_succeed(
        &conn,
        &insert_rule(&Rule {
            sku: "55555555-5555-5555-5555-555555555555",
            required: 1,
            max_qty: Some(1),
            ..OPTIONAL
        }),
    )
    .await;
    assert_eq!(count_rules(&conn, 0).await, "2");
}

/// The two constraints §6 does not name. Each is reported as an addition, and
/// each is here because §5 names no code for it — so the pipeline cannot report
/// the shape at all, and without the CHECK it publishes.
#[tokio::test]
async fn a_bound_pair_that_admits_no_quantity_is_refused() {
    let conn = migrated_db().await;
    insert_revision(&conn, 0).await;

    must_be_rejected(
        &conn,
        &insert_rule(&Rule {
            min_qty: Some(5),
            max_qty: Some(2),
            ..OPTIONAL
        }),
        "chk_pricing_plan_addon_rule_qty_range",
    )
    .await;
    must_be_rejected(
        &conn,
        &insert_rule(&Rule {
            step_qty: Some(0),
            ..OPTIONAL
        }),
        "chk_pricing_plan_addon_rule_step_qty",
    )
    .await;

    // A half-authored draft stays savable: one bound set and the other not is
    // the state an author passes through between two requests, and the NULL arms
    // are what keep it out of the refusal.
    must_succeed(
        &conn,
        &insert_rule(&Rule {
            min_qty: Some(5),
            ..OPTIONAL
        }),
    )
    .await;
    must_succeed(
        &conn,
        &insert_rule(&Rule {
            sku: "55555555-5555-5555-5555-555555555555",
            max_qty: Some(2),
            step_qty: Some(1),
            ..OPTIONAL
        }),
    )
    .await;
    assert_eq!(count_rules(&conn, 0).await, "2");
}

#[tokio::test]
async fn an_add_on_rule_belongs_to_a_revision_that_exists() {
    let conn = migrated_db().await;

    // No revision at all: the row is refused. The **append-only trigger**
    // answers rather than the foreign key, and that is not an accident — its
    // predicate ("a draft revision with this key exists") strictly implies the
    // key's ("a revision with this key exists"), so on the insert path the
    // trigger can never be reached second.
    must_be_rejected(
        &conn,
        &insert_rule(&OPTIONAL),
        "under a non-draft plan revision is not permitted",
    )
    .await;

    // The foreign key is declared anyway, over **both** columns, and it says
    // something the trigger does not: that an add-on rule is filed under one
    // revision of one plan.
    let declared = scalar(
        &conn,
        "SELECT group_concat(\"from\" || '->' || \"to\", ',') AS v
           FROM pragma_foreign_key_list('pricing_plan_addon_rule')
          WHERE \"table\" = 'pricing_plan'",
    )
    .await;
    assert!(
        declared.contains("plan_id->plan_id") && declared.contains("plan_revision->revision"),
        "the composite FK must cover both key columns, got: {declared}"
    );
}

/// The edge columns are `NOT NULL DEFAULT '[]'`: an add-on rule always has both
/// sets, and an empty set is what "no edges" is. A nullable column would give
/// the empty set two spellings and make every reader choose one.
#[tokio::test]
async fn an_unauthored_edge_set_is_the_empty_array_not_null() {
    let conn = migrated_db().await;
    insert_revision(&conn, 0).await;
    must_succeed(&conn, &insert_rule(&OPTIONAL)).await;

    let stored = scalar(
        &conn,
        &format!(
            "SELECT depends_on_addon_sku_id || '/' || conflicts_with_addon_sku_id AS v
               FROM pricing_plan_addon_rule WHERE addon_sku_id = '{}'",
            OPTIONAL.sku
        ),
    )
    .await;
    assert_eq!(stored, "[]/[]");

    must_be_rejected(
        &conn,
        &format!(
            "UPDATE pricing_plan_addon_rule SET depends_on_addon_sku_id = NULL \
             WHERE addon_sku_id = '{}'",
            OPTIONAL.sku
        ),
        "NOT NULL constraint failed",
    )
    .await;
}

// ---------------------------------------------------------------------------
// The append-only triggers.
// ---------------------------------------------------------------------------

/// Revision 0 published with a rule, revision 2 abandoned with a rule, revision
/// 1 left open as the plan's one draft — in that order, because a plan admits
/// one draft at a time and each rule has to be written while its own revision is
/// still one.
async fn seeded_planes(conn: &DatabaseConnection) {
    insert_revision(conn, 0).await;
    must_succeed(conn, &insert_rule(&OPTIONAL)).await;
    flip(conn, 0, "published").await;

    insert_revision(conn, 2).await;
    must_succeed(
        conn,
        &insert_rule(&Rule {
            revision: 2,
            ..OPTIONAL
        }),
    )
    .await;
    flip(conn, 2, "abandoned").await;

    insert_revision(conn, 1).await;
    must_succeed(
        conn,
        &insert_rule(&Rule {
            revision: 1,
            ..OPTIONAL
        }),
    )
    .await;
}

#[tokio::test]
async fn a_frozen_revision_takes_no_new_add_on_rule_and_a_draft_one_does() {
    let conn = migrated_db().await;
    seeded_planes(&conn).await;

    // INSERT is guarded and not only UPDATE and DELETE, because it is the one
    // verb that *adds* an add-on to a frozen revision — the projector's warm
    // re-drive reads truth rows, so an appended rule would re-materialize a
    // frozen CatalogVersion at a composition nobody published.
    let appended = Rule {
        sku: "99999999-9999-9999-9999-999999999999",
        ..OPTIONAL
    };
    must_be_rejected(
        &conn,
        &insert_rule(&Rule {
            revision: 0,
            ..appended
        }),
        "INSERT of an add-on rule under a non-draft plan revision",
    )
    .await;
    // `abandoned` is not `draft`, which is exactly what makes the drop-then-flip
    // ordering in `PlanRepo::abandon_draft` mandatory rather than stylistic.
    must_be_rejected(
        &conn,
        &insert_rule(&Rule {
            revision: 2,
            ..appended
        }),
        "INSERT of an add-on rule under a non-draft plan revision",
    )
    .await;
    must_succeed(
        &conn,
        &insert_rule(&Rule {
            revision: 1,
            ..appended
        }),
    )
    .await;

    assert_eq!(count_rules(&conn, 0).await, "1");
    assert_eq!(count_rules(&conn, 2).await, "1");
    assert_eq!(count_rules(&conn, 1).await, "2");
}

#[tokio::test]
async fn a_frozen_revisions_add_on_rule_may_not_be_edited_and_a_drafts_may() {
    let conn = migrated_db().await;
    seeded_planes(&conn).await;

    let edit = |revision: i64| {
        format!(
            "UPDATE pricing_plan_addon_rule SET min_qty = 9 \
             WHERE plan_revision = {revision} AND addon_sku_id = '{}'",
            OPTIONAL.sku
        )
    };
    must_be_rejected(
        &conn,
        &edit(0),
        "UPDATE of an add-on rule under a non-draft plan revision",
    )
    .await;
    must_be_rejected(
        &conn,
        &edit(2),
        "UPDATE of an add-on rule under a non-draft plan revision",
    )
    .await;
    must_succeed(&conn, &edit(1)).await;

    let frozen = scalar(
        &conn,
        &format!(
            "SELECT CAST(coalesce(min_qty, -1) AS TEXT) AS v FROM pricing_plan_addon_rule \
             WHERE plan_revision = 0 AND addon_sku_id = '{}'",
            OPTIONAL.sku
        ),
    )
    .await;
    assert_eq!(frozen, "-1", "no rejected UPDATE may have landed");
}

#[tokio::test]
async fn an_add_on_rule_may_not_be_re_pointed_at_a_frozen_revision() {
    let conn = migrated_db().await;
    seeded_planes(&conn).await;

    // Checking the NEW parent on UPDATE is not belt-and-braces: re-pointing a
    // child row's `plan_revision` is how one would otherwise append a rule to a
    // frozen revision without ever issuing an INSERT.
    must_be_rejected(
        &conn,
        "UPDATE pricing_plan_addon_rule SET plan_revision = 0 WHERE plan_revision = 1",
        "UPDATE of an add-on rule under a non-draft plan revision",
    )
    .await;
    // And the OLD parent, for the mirror-image reason: the row is being mutated
    // out from under a revision whose freeze governs it.
    must_be_rejected(
        &conn,
        "UPDATE pricing_plan_addon_rule SET plan_revision = 1 WHERE plan_revision = 0",
        "UPDATE of an add-on rule under a non-draft plan revision",
    )
    .await;

    assert_eq!(count_rules(&conn, 0).await, "1");
    assert_eq!(count_rules(&conn, 1).await, "1");
}

#[tokio::test]
async fn a_frozen_revisions_add_on_rule_may_not_be_deleted_and_a_drafts_may() {
    let conn = migrated_db().await;
    seeded_planes(&conn).await;

    for revision in [0, 2] {
        must_be_rejected(
            &conn,
            &format!("DELETE FROM pricing_plan_addon_rule WHERE plan_revision = {revision}"),
            "DELETE of an add-on rule under a non-draft plan revision",
        )
        .await;
    }
    must_succeed(
        &conn,
        "DELETE FROM pricing_plan_addon_rule WHERE plan_revision = 1",
    )
    .await;

    assert_eq!(count_rules(&conn, 0).await, "1");
    assert_eq!(count_rules(&conn, 2).await, "1");
    assert_eq!(count_rules(&conn, 1).await, "0");
}

// ---------------------------------------------------------------------------
// The repository.
// ---------------------------------------------------------------------------

/// The two repositories, plus the provider the seeding helper needs to put a
/// revision into a state `plan_repo::publish_revision` now reaches - fabricated
/// here rather than published, so this suite tests the child table and not the
/// publish unit.
async fn repo_harness() -> (PlanRepo, PlanShapeRepo, DBProvider<DbError>) {
    let db = connect_db("sqlite::memory:", ConnectOpts::default())
        .await
        .expect("connect in-memory sqlite");
    run_migrations_for_testing(&db, Migrator::migrations())
        .await
        .expect("run migrator");
    let provider = DBProvider::<DbError>::new(db);
    (
        PlanRepo::new(provider.clone()),
        PlanShapeRepo::new(provider.clone()),
        provider,
    )
}

/// Publish a revision straight at the column, which is the same flip
/// `plan_repo::publish_revision` performs; `pricing_plan`'s append-only trigger
/// whitelists it. Fabricated rather than published so this suite stays about
/// the child table.
async fn publish(provider: &DBProvider<DbError>, scope: &AccessScope, plan_id: PlanId) {
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
                .add(plan::Column::Revision.eq(0_i64)),
        )
        .exec(&conn)
        .await
        .expect("publish the revision");
    assert_eq!(result.rows_affected, 1, "the seed must have moved one row");
}

fn at(hour: u32) -> OffsetDateTime {
    utc_ymd_hms(2026, 8, 2, hour, 0, 0)
}

fn draft_of(plan_id: PlanId, tenant_id: Uuid) -> NewPlanDraft {
    NewPlanDraft {
        plan_name: None,
        plan_id,
        tenant_id,
        created_by: Uuid::from_u128(0xac_10),
        created_at_utc: at(10),
        sku_id: None,
        plan_tier: None,
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
    }
}

const ANALYTICS: Uuid = Uuid::from_u128(0xadd01);
const SUPPORT: Uuid = Uuid::from_u128(0xadd02);
const SEATS: Uuid = Uuid::from_u128(0xadd03);
/// An add-on of some *other* plan: an edge naming it points out of this plan's
/// set, which is `ADDON_INCOMPATIBLE`'s finding rather than the repository's.
const FOREIGN_ADDON: Uuid = Uuid::from_u128(0xadd99);

fn rule(addon_sku_id: Uuid) -> AddonRule {
    AddonRule {
        addon_sku_id,
        required: false,
        min_qty: None,
        max_qty: None,
        step_qty: None,
        price_override_ref: None,
        depends_on: Vec::new(),
        conflicts_with: Vec::new(),
    }
}

/// Three rules, one conflict edge authored on **one** side only, and one
/// dependency edge — the set §9's D-105 acceptance case names.
fn three_rules() -> Vec<AddonRule> {
    vec![
        AddonRule {
            required: true,
            max_qty: Some(3),
            min_qty: Some(1),
            step_qty: Some(1),
            conflicts_with: vec![SUPPORT],
            ..rule(ANALYTICS)
        },
        rule(SUPPORT),
        AddonRule {
            depends_on: vec![ANALYTICS],
            price_override_ref: Some(Uuid::from_u128(0x0_ff1)),
            ..rule(SEATS)
        },
    ]
}

#[tokio::test]
async fn a_conflict_authored_on_one_side_is_stored_on_both() {
    let (repo, shapes, provider) = repo_harness().await;

    let tenant = Uuid::from_u128(0x7e_11);
    let scope = AccessScope::for_tenant(tenant);
    let plan_id = PlanId::new(Uuid::from_u128(0x9_1a4));

    repo.create_draft(&scope, draft_of(plan_id, tenant))
        .await
        .expect("create");
    shapes
        .replace_addon_rules(
            &scope,
            tenant,
            plan_id,
            0,
            RowVersion::new(0),
            three_rules(),
            stamp(),
        )
        .await
        .expect("author the add-on set");

    let stored = shapes
        .list_addon_rules(&scope, tenant, plan_id, 0)
        .await
        .expect("read");
    assert_eq!(
        stored.len(),
        3,
        "all three rules persist under the revision"
    );

    // The back-edge. Without it the conflict exists when read from the
    // analytics row and not from the support row, so `AddonConflictBothRequired`
    // reaches two verdicts on one plan depending on which row it started from.
    let support = stored
        .iter()
        .find(|row| row.addon_sku_id == SUPPORT)
        .expect("the support rule is there");
    assert_eq!(
        support.conflicts_with,
        vec![ANALYTICS],
        "a conflict authored on one side must be stored on both"
    );
    let analytics = stored
        .iter()
        .find(|row| row.addon_sku_id == ANALYTICS)
        .expect("the analytics rule is there");
    assert_eq!(analytics.conflicts_with, vec![SUPPORT]);

    // Everything else round-trips as authored, the directed edge included: a
    // symmetrized `depends_on` would make every dependency its own two-cycle and
    // `ADDON_CYCLE` would then fail every plan that has one.
    let seats = stored
        .iter()
        .find(|row| row.addon_sku_id == SEATS)
        .expect("the seats rule is there");
    assert_eq!(seats.depends_on, vec![ANALYTICS]);
    assert!(seats.conflicts_with.is_empty());
    assert_eq!(seats.price_override_ref, Some(Uuid::from_u128(0x0_ff1)));
    assert!(analytics.depends_on.is_empty());
    assert!(analytics.required);
    assert_eq!(analytics.min_qty, Some(1));
    assert_eq!(analytics.max_qty, Some(3));
    assert_eq!(analytics.step_qty, Some(1));

    // And the write recorded, which is what `stamp()` is for and what nothing in
    // this file read. All three of its fields, because a stamp assembled from a
    // default actor and a wall clock satisfies a row-exists check while recording
    // an act nobody can attribute — and the table is append-only, so a wrong row
    // is the row forever.
    let records = bss_pricing::infra::storage::repo::audit_repo::for_subject(
        &provider.conn().expect("conn"),
        &scope,
        tenant,
        bss_pricing::domain::audit::AuditSubjectKind::PlanRevision,
        &bss_pricing::infra::storage::repo::audit_repo::plan_revision_ref(plan_id, 0),
    )
    .await
    .expect("read the revision's audit trail");
    // By action, not by actor: `create_draft` above records under the same
    // principal, at `at(10)`, so an actor filter finds the seed's row and the
    // instant assertion below would be about the fixture rather than the write.
    let updates: Vec<_> = records
        .iter()
        .filter(|row| row.action == "update")
        .collect();
    // Exactly one, not merely one to be found: the table is append-only, so a
    // second `update` row is a write nobody made and a `find` would step over it.
    let [recorded] = updates.as_slice() else {
        panic!("the add-on write records one update: {records:?}");
    };
    assert_eq!(recorded.actor_principal_id, uuid::Uuid::from_u128(0xac_10));

    assert_eq!(
        recorded.recorded_at,
        at(12),
        "the stamp's instant, not a clock"
    );
    assert_eq!(recorded.correlation_id, Some(TEST_CORRELATION));
}

#[tokio::test]
async fn an_edge_pointing_out_of_the_set_is_left_for_the_pipeline_to_report() {
    let (repo, shapes, _provider) = repo_harness().await;
    let tenant = Uuid::from_u128(0x7e_11);
    let scope = AccessScope::for_tenant(tenant);
    let plan_id = PlanId::new(Uuid::from_u128(0x9_1a4));

    repo.create_draft(&scope, draft_of(plan_id, tenant))
        .await
        .expect("create");
    shapes
        .replace_addon_rules(
            &scope,
            tenant,
            plan_id,
            0,
            RowVersion::new(0),
            vec![AddonRule {
                conflicts_with: vec![FOREIGN_ADDON],
                ..rule(ANALYTICS)
            }],
            stamp(),
        )
        .await
        .expect("a set with a dangling edge is storable");

    // Kept, and kept one-sided. There is no row for the back-edge to land on,
    // and inventing one would turn `ADDON_INCOMPATIBLE`'s finding — an edge
    // pointing outside the plan's own add-on set — into a silent repair the
    // author never sees.
    let stored = shapes
        .list_addon_rules(&scope, tenant, plan_id, 0)
        .await
        .expect("read");
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].conflicts_with, vec![FOREIGN_ADDON]);
}

/// An edge set is a **set**: authored twice, in either order, it stores once.
#[tokio::test]
async fn an_edge_set_is_stored_deduplicated_and_in_one_fixed_order() {
    let (repo, shapes, _provider) = repo_harness().await;
    let tenant = Uuid::from_u128(0x7e_11);
    let scope = AccessScope::for_tenant(tenant);
    let plan_id = PlanId::new(Uuid::from_u128(0x9_1a4));

    repo.create_draft(&scope, draft_of(plan_id, tenant))
        .await
        .expect("create");
    shapes
        .replace_addon_rules(
            &scope,
            tenant,
            plan_id,
            0,
            RowVersion::new(0),
            // Authored in the opposite order to the one the read must produce,
            // so a suite that dropped the `ORDER BY` cannot pass on insertion
            // order alone.
            vec![
                rule(SEATS),
                rule(SUPPORT),
                AddonRule {
                    // Reverse order, one of them twice.
                    depends_on: vec![SEATS, SUPPORT, SEATS],
                    ..rule(ANALYTICS)
                },
            ],
            stamp(),
        )
        .await
        .expect("author the set");

    let stored = shapes
        .list_addon_rules(&scope, tenant, plan_id, 0)
        .await
        .expect("read");
    let analytics = stored
        .iter()
        .find(|row| row.addon_sku_id == ANALYTICS)
        .expect("present");
    assert_eq!(
        analytics.depends_on,
        vec![SUPPORT, SEATS],
        "the stored edge set must be deduplicated and ordered by id"
    );
    // The rows themselves come back in one fixed order too, so a cycle report
    // names its members the same way on every run.
    assert_eq!(
        stored
            .iter()
            .map(|row| row.addon_sku_id)
            .collect::<Vec<Uuid>>(),
        vec![ANALYTICS, SUPPORT, SEATS],
    );
}

#[tokio::test]
async fn a_new_revision_carries_every_add_on_rule_and_its_symmetry_d83() {
    let (repo, shapes, provider) = repo_harness().await;
    let tenant = Uuid::from_u128(0x7e_11);
    let scope = AccessScope::for_tenant(tenant);
    let plan_id = PlanId::new(Uuid::from_u128(0x9_1a4));

    repo.create_draft(&scope, draft_of(plan_id, tenant))
        .await
        .expect("create");
    shapes
        .replace_addon_rules(
            &scope,
            tenant,
            plan_id,
            0,
            RowVersion::new(0),
            three_rules(),
            stamp(),
        )
        .await
        .expect("author the add-on set");
    publish(&provider, &scope, plan_id).await;

    let opened = repo
        .open_revision(
            &scope,
            tenant,
            plan_id,
            stamp_of(Uuid::from_u128(0xac_11), at(12)),
        )
        .await
        .expect("open the successor revision");
    assert_eq!(opened.revision, 1);

    // All three, not a sample: a copy that dropped one is a successor revision
    // composing with fewer add-ons than its predecessor, and its buyers lose an
    // entitlement nobody removed.
    let carried = shapes
        .list_addon_rules(&scope, tenant, plan_id, 1)
        .await
        .expect("read the new revision's rules");
    let frozen = shapes
        .list_addon_rules(&scope, tenant, plan_id, 0)
        .await
        .expect("read the published revision's rules");
    assert_eq!(carried.len(), 3);
    assert_eq!(
        carried, frozen,
        "the successor must carry the set as stored, symmetry and all"
    );
    // Spelled out rather than left to the comparison, which would hold just as
    // well if the copy had lost the closure on both sides at once.
    let support = carried
        .iter()
        .find(|row| row.addon_sku_id == SUPPORT)
        .expect("present");
    assert_eq!(support.conflicts_with, vec![ANALYTICS]);
}

#[tokio::test]
async fn a_published_revisions_add_on_set_is_refused_by_name_not_by_trigger() {
    let (repo, shapes, provider) = repo_harness().await;
    let tenant = Uuid::from_u128(0x7e_11);
    let scope = AccessScope::for_tenant(tenant);
    let plan_id = PlanId::new(Uuid::from_u128(0x9_1a4));

    repo.create_draft(&scope, draft_of(plan_id, tenant))
        .await
        .expect("create");
    let authored = shapes
        .replace_addon_rules(
            &scope,
            tenant,
            plan_id,
            0,
            RowVersion::new(0),
            three_rules(),
            stamp(),
        )
        .await
        .expect("author the add-on set");
    // The child set has no entity tag of its own: the plan revision's covers it.
    assert_eq!(authored.row_version, RowVersion::new(1));
    publish(&provider, &scope, plan_id).await;

    // The table's own trigger would answer this with a raw driver error carrying
    // no state and no subject, which reaches a caller as an internal fault. The
    // read that precedes the delete is what turns it into a refusal a surface
    // can render.
    let err = shapes
        .replace_addon_rules(
            &scope,
            tenant,
            plan_id,
            0,
            authored.row_version,
            Vec::new(),
            stamp(),
        )
        .await
        .expect_err("a frozen revision's composition is not editable");
    assert!(
        matches!(err, RepoError::NotDraft { ref state, .. } if state == "published"),
        "got: {err:?}"
    );
    assert_eq!(
        shapes
            .list_addon_rules(&scope, tenant, plan_id, 0)
            .await
            .expect("read")
            .len(),
        3,
        "the frozen set must be untouched"
    );
}

#[tokio::test]
async fn a_stale_version_replaces_no_add_on_rule() {
    let (repo, shapes, _provider) = repo_harness().await;
    let tenant = Uuid::from_u128(0x7e_11);
    let scope = AccessScope::for_tenant(tenant);
    let plan_id = PlanId::new(Uuid::from_u128(0x9_1a4));

    repo.create_draft(&scope, draft_of(plan_id, tenant))
        .await
        .expect("create");
    shapes
        .replace_addon_rules(
            &scope,
            tenant,
            plan_id,
            0,
            RowVersion::new(0),
            three_rules(),
            stamp(),
        )
        .await
        .expect("author the add-on set");

    let err = shapes
        .replace_addon_rules(
            &scope,
            tenant,
            plan_id,
            0,
            RowVersion::new(0),
            vec![rule(SEATS)],
            stamp(),
        )
        .await
        .expect_err("a shape edit under a superseded tag must be refused");
    assert!(
        matches!(
            err,
            RepoError::StaleRowVersion {
                current: 1,
                submitted: 0,
                ..
            }
        ),
        "got: {err:?}"
    );

    // The delete runs before the swap answers, so the rollback is what keeps
    // this true — not an ordering that avoided touching the table.
    assert_eq!(
        shapes
            .list_addon_rules(&scope, tenant, plan_id, 0)
            .await
            .expect("read")
            .len(),
        3,
        "a refused replacement must leave the previous set intact"
    );
}

#[tokio::test]
async fn another_tenants_add_on_set_is_invisible() {
    let (repo, shapes, _provider) = repo_harness().await;
    let owner = Uuid::from_u128(0x7e_11);
    let owner_scope = AccessScope::for_tenant(owner);
    let plan_id = PlanId::new(Uuid::from_u128(0x9_1a4));

    repo.create_draft(&owner_scope, draft_of(plan_id, owner))
        .await
        .expect("create");
    shapes
        .replace_addon_rules(
            &owner_scope,
            owner,
            plan_id,
            0,
            RowVersion::new(0),
            three_rules(),
            stamp(),
        )
        .await
        .expect("author the add-on set");

    // SQL-level BOLA: the catalog is commercially sensitive, so a foreign scope
    // sees absence rather than a refusal that would confirm the plan exists.
    let intruder = Uuid::from_u128(0x7e_22);
    let intruder_scope = AccessScope::for_tenant(intruder);
    assert!(
        shapes
            .list_addon_rules(&intruder_scope, intruder, plan_id, 0)
            .await
            .expect("read")
            .is_empty(),
        "a foreign tenant must see no add-on rules"
    );
    // Not even by naming the owner's tenant id: the scope decides, not the
    // argument.
    assert!(
        shapes
            .list_addon_rules(&intruder_scope, owner, plan_id, 0)
            .await
            .expect("read")
            .is_empty(),
        "naming the owning tenant must not widen a foreign scope"
    );
    assert_eq!(
        shapes
            .list_addon_rules(&owner_scope, owner, plan_id, 0)
            .await
            .expect("read")
            .len(),
        3,
        "the owner still sees the whole set"
    );
}

/// The actor and instant every mutating repository call records (D-135 - the
/// audit row commits inside the mutation's own transaction).
///
/// Fixed like every other instant in this suite. A wall clock here would put the
/// one column an assertion about the stamp has to read out of the test's reach,
/// and the row it lands in is append-only, so there is no second chance to pin it.
fn stamp() -> bss_pricing::domain::audit::AuditStamp {
    bss_pricing::domain::audit::AuditStamp {
        actor_principal_id: uuid::Uuid::from_u128(0xac_10),
        recorded_at: at(12),
        correlation_id: TEST_CORRELATION,
    }
}

// ---------------------------------------------------------------------------
// The parent revision's tenancy.
// ---------------------------------------------------------------------------

/// A rule row belongs to the tenant its own revision belongs to.
///
/// `fk_pricing_plan_addon_rule_revision` covers `(plan_id, plan_revision)` alone
/// while the table is scoped by `tenant_id`, so without this arm the key admits a
/// row naming somebody else's tenant — invisible to every scoped reader and frozen
/// with the revision it was written under.
///
/// The arm was present on both engines and **executed nowhere**: it was carried by
/// the trigger-name roster, the trigger-body digest roster and both schema goldens,
/// all of which pin the guard's *text*, which is the half an author regenerates in
/// the same edit that breaks it. A guard that is present, correct-looking and
/// semantically inert reads as coverage to all three.
///
/// Pinned by the guard's **own message fragment** and not by the table name:
/// `must_be_rejected` already requires the table name, and every CHECK and every
/// other trigger arm on this table carries it too, so the table name alone passes
/// for whichever guard happened to answer.
#[tokio::test]
async fn a_rule_may_not_claim_a_tenant_its_revision_does_not_belong_to() {
    const TENANT_TWO: &str = "11111111-1111-1111-1111-1111111111b2";
    let conn = migrated_db().await;
    insert_revision(&conn, 0).await;

    let foreign = format!(
        "INSERT INTO pricing_plan_addon_rule (
            plan_id, plan_revision, addon_sku_id, tenant_id, required)
         VALUES ('{PLAN}', 0, '{}', '{TENANT_TWO}', 0)",
        OPTIONAL.sku
    );
    must_be_rejected(
        &conn,
        &foreign,
        "belongs to another tenant and may not hold this row",
    )
    .await;
    assert_eq!(
        count_rules(&conn, 0).await,
        "0",
        "the foreign-tenant row must not have landed"
    );

    // The same row under the revision's own tenant lands, so the refusal above is
    // about the tenant and about nothing else the row carries.
    must_succeed(&conn, &insert_rule(&OPTIONAL)).await;

    // And the UPDATE direction, because the arm sits on the function's `NEW` path
    // and a draft revision's rules are mutable — the frozen-column whitelist has
    // nothing to say about `tenant_id`.
    must_be_rejected(
        &conn,
        &format!(
            "UPDATE pricing_plan_addon_rule SET tenant_id = '{TENANT_TWO}' \
             WHERE plan_id = '{PLAN}' AND plan_revision = 0 AND addon_sku_id = '{}'",
            OPTIONAL.sku
        ),
        "belongs to another tenant and may not hold this row",
    )
    .await;
}
