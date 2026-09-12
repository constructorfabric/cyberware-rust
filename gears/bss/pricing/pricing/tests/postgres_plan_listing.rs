//! `plan_repo::list_authoring_page` on Postgres: one row per plan, and the row
//! is the one an **author** is holding.
//!
//! Why this suite is on the Postgres tier rather than the `SQLite` one: what it
//! proves is an interaction between an `ORDER BY` over two columns, a `LIMIT`
//! and the two partial `UNIQUE` indexes that bound how many rows a plan can
//! contribute — `uq_pricing_plan_current` and `uq_pricing_plan_open_draft`. The
//! indexes are what make the collapse's over-fetch sufficient, and they exist
//! as partial indexes only on the engine that has them.
//!
//! The listing deliberately does **not** answer with `current_tokens()`.
//! `LifecycleState::is_current_revision` is `Published | Retired`, so a plan
//! whose only revision is the draft somebody is authoring right now has no
//! current revision at all — and that is exactly the plan an authoring UI most
//! needs to see. `plans.rs::authoring_revision` already decides the rule for one
//! plan; this is the same rule over a page.
//!
//! Ignored by default; they need Docker. Run with
//! `cargo test -p cf-gears-bss-pricing -- --ignored`.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;
mod pg_support;

#[path = "postgres_plan_listing/list_query.rs"]
mod list_query;

use bss_pricing::domain::audit::AuditStamp;
use bss_pricing::domain::lifecycle::LifecycleState;
use bss_pricing::domain::plan::PlanRevision;
use bss_pricing::domain::scope_key::PlanId;
use bss_pricing::infra::storage::entity::plan;
use bss_pricing::infra::storage::repo::plan_repo::count_plans_by_authoring_state;
use bss_pricing::infra::storage::repo::{NewPlanDraft, PlanRepo, plan_repo};

use bss_pricing::domain::instant::utc_ymd_hms;
use pg_support::Pg;
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, EntityTrait};
use time::OffsetDateTime;
use toolkit_db::secure::{AccessScope, DBRunner, SecureUpdateExt};
use toolkit_db::{DBProvider, DbError};
use uuid::Uuid;

const TENANT: Uuid = Uuid::from_u128(0x7e_51);
const ACTOR: Uuid = Uuid::from_u128(0xac_50);
const CORRELATION: Uuid = Uuid::from_u128(0xc0_51);

fn scope() -> AccessScope {
    AccessScope::for_tenant(TENANT)
}

fn at(hour: u32) -> OffsetDateTime {
    utc_ymd_hms(2026, 8, 11, hour, 0, 0)
}

fn stamp() -> AuditStamp {
    AuditStamp {
        actor_principal_id: ACTOR,
        recorded_at: at(11),
        correlation_id: CORRELATION,
    }
}

/// A plan whose revision `0` is `published`, by the direct route
/// `tests/common` documents: this suite is about the listing, and a fixture that
/// went through the publish pipeline would make a pipeline failure look like a
/// listing failure.
async fn seed_published_plan(provider: &DBProvider<DbError>, plan_id: PlanId) {
    let created = PlanRepo::new(provider.clone())
        .create_draft(
            &scope(),
            NewPlanDraft {
                plan_name: None,
                plan_id,
                tenant_id: TENANT,
                created_by: ACTOR,
                created_at_utc: at(10),
                sku_id: None,
                plan_tier: Some("gold".to_owned()),
                billing_cycle: None,
                frequency: None,
                plan_tier_override: false,
                purchase_min_qty: None,
                purchase_max_qty: None,
                invoice_grouping_key: None,
                available_from: None,
                available_to: None,
                cloned_from: None,
                correlation_id: CORRELATION,
            },
        )
        .await
        .expect("create the draft");
    common::publish_plan_directly(provider, &scope(), plan_id, created.revision).await;
}

/// **A plan appears once, and as the draft its author is editing.**
///
/// The two-row plan is the whole case: a published revision `0` and the draft
/// revision `1` opened over it. A listing keyed on `current_tokens()` would show
/// the published one, which is the body the author's next `PATCH` would not
/// match; a listing that did not collapse would show the plan twice.
#[tokio::test]
#[ignore = "requires Postgres; run with --ignored"]
async fn a_plan_with_a_draft_over_a_published_revision_lists_once_as_its_draft() {
    let pg = Pg::applied().await;
    let provider = DBProvider::<DbError>::new(pg.db().await);
    let plans = PlanRepo::new(provider.clone());

    let plan_id = PlanId::new(Uuid::from_u128(0x50_51));
    seed_published_plan(&provider, plan_id).await;
    let draft = plans
        .open_revision(&scope(), TENANT, plan_id, stamp())
        .await
        .expect("reopen the plan as a draft");
    assert_eq!(draft.revision, 1, "the successor is revision 1");

    let conn = provider.conn().expect("conn");
    let rows = plan_repo::list_authoring_page(&conn, &scope(), TENANT, &[], None, 100)
        .await
        .expect("list");

    let seen: Vec<_> = rows.iter().filter(|r| r.plan_id == plan_id).collect();
    assert_eq!(seen.len(), 1, "a plan appears once, not once per revision");
    assert_eq!(
        seen[0].revision, 1,
        "the page answers about the revision an author is holding"
    );
    assert_eq!(
        seen[0].lifecycle_state,
        LifecycleState::Draft,
        "the draft is the authoring revision, so it is what the listing shows"
    );
}

/// **The walk visits every plan exactly once.**
///
/// Five plans over pages of two. Each plan here holds exactly one revision, so
/// what this case exercises is the boundary arithmetic — a walk that mistook the
/// window (`2 × limit`) for the page would either repeat a plan or skip one at
/// every boundary. It does **not** reach the collapse or the redraw: with one
/// candidate row per plan the window is never simultaneously full and
/// unexhausted, which is
/// [`a_filtered_walk_returns_full_pages_when_one_plan_fills_the_window`]'s job.
#[tokio::test]
#[ignore = "requires Postgres; run with --ignored"]
async fn the_walk_pages_by_plan_and_never_repeats_one() {
    let pg = Pg::applied().await;
    let provider = DBProvider::<DbError>::new(pg.db().await);

    let mut expected = Vec::new();
    for n in 0..5_u128 {
        let plan_id = PlanId::new(Uuid::from_u128(0x50_60 + n));
        seed_published_plan(&provider, plan_id).await;
        expected.push(plan_id.get());
    }
    expected.sort();

    let conn = provider.conn().expect("conn");
    let walked: Vec<Uuid> = walk(&conn, &[], 2)
        .await
        .into_iter()
        .map(|row| row.plan_id.get())
        .collect();

    let mut deduped = walked.clone();
    deduped.dedup();
    assert_eq!(walked, deduped, "no plan is returned twice");
    let mut sorted = walked;
    sorted.sort();
    assert_eq!(sorted, expected, "every plan once, in plan_id order");
}

/// Page through the whole listing, and **terminate**.
///
/// The cap is an assertion, not tidiness. The walk's only exit is an empty page
/// and its cursor is re-derived from the page it was just handed, so a regression
/// that stopped `list_authoring_page` advancing past `after` — the
/// `plan_id.gt(cursor)` filter is the whole of that advance — would hand back the
/// same page forever. That non-advancing cursor is precisely the defect these
/// cases exist to catch, and without the cap it arrives as a hung CI job rather
/// than as a red one.
async fn walk(runner: &impl DBRunner, states: &[LifecycleState], limit: u64) -> Vec<PlanRevision> {
    /// Generous against every seed in this file (five plans over pages of two is
    /// four calls including the empty one), and finite.
    const CAP: usize = 20;

    let mut walked: Vec<PlanRevision> = Vec::new();
    let mut after = None;
    for _ in 0..CAP {
        let page = plan_repo::list_authoring_page(runner, &scope(), TENANT, states, after, limit)
            .await
            .expect("list");
        if page.is_empty() {
            return walked;
        }
        after = Some(page.last().expect("non-empty").plan_id.get());
        walked.extend(page);
    }
    panic!(
        "the walk did not terminate in {CAP} pages: the cursor is not advancing past `after`, \
         which a paging caller experiences as an endless listing"
    );
}

/// **A filtered walk still returns full pages when one plan fills the window.**
///
/// The case the redraw loop exists for, and the one nothing exercised: every
/// other test here passes an empty `states` slice, where a plan contributes at
/// most two candidate rows (`uq_pricing_plan_current` permits one current
/// revision, `uq_pricing_plan_open_draft` one open draft) and a single window of
/// `2 × limit` therefore always holds `limit` distinct plans. `superseded` is a
/// state a plan holds once **per revision**, so the first plan's four superseded
/// revisions fill the whole window on their own and collapse to a single row —
/// after which the page is short unless the window is drawn again.
///
/// A short page is what a paginating caller reads as the end of the results, so a
/// break here silently truncates an operator's plan listing with every other gate
/// green.
#[tokio::test]
#[ignore = "requires Postgres; run with --ignored"]
async fn a_filtered_walk_returns_full_pages_when_one_plan_fills_the_window() {
    let pg = Pg::applied().await;
    let provider = DBProvider::<DbError>::new(pg.db().await);

    // Ordered by `plan_id`, which is the order the listing answers in.
    let many = PlanId::new(Uuid::from_u128(0x50_70));
    let second = PlanId::new(Uuid::from_u128(0x50_71));
    let third = PlanId::new(Uuid::from_u128(0x50_72));
    seed_superseded_chain(&provider, many, 4).await;
    seed_superseded_chain(&provider, second, 1).await;
    seed_superseded_chain(&provider, third, 1).await;

    let conn = provider.conn().expect("conn");

    // The seed is what arms the case: with fewer than `2 × limit` superseded
    // revisions on `many` the first window would not be full and the redraw would
    // not be reached at all.
    for revision in 0..4 {
        assert_eq!(
            plan_repo::load_revision(&conn, &scope(), TENANT, many, revision)
                .await
                .expect("read")
                .expect("the revision is there")
                .lifecycle_state,
            LifecycleState::Superseded,
            "revision {revision} of the many-revisioned plan must be superseded"
        );
    }

    let states = &[LifecycleState::Superseded];
    let page = plan_repo::list_authoring_page(&conn, &scope(), TENANT, states, None, 2)
        .await
        .expect("list");
    assert_eq!(
        page.len(),
        2,
        "the first window holds only `many`'s four superseded revisions, which collapse to \
         one row; the page comes back full only because the window is drawn again"
    );
    assert_eq!(page[0].plan_id, many);
    assert_eq!(
        page[0].revision, 3,
        "and the row is the plan's highest revision in the filtered state"
    );
    assert_eq!(page[1].plan_id, second);

    let walked: Vec<(Uuid, u64)> = walk(&conn, states, 2)
        .await
        .into_iter()
        .map(|row| (row.plan_id.get(), row.revision))
        .collect();
    assert_eq!(
        walked,
        vec![(many.get(), 3), (second.get(), 0), (third.get(), 0)],
        "every plan once, in plan_id order, as its highest superseded revision"
    );
}

/// A plan whose revisions `0..superseded` are `superseded` and whose last
/// revision is `published`.
///
/// Built by the same direct route [`seed_published_plan`] uses and in the
/// production order — a successor is opened while its predecessor is still
/// current, the predecessor is superseded, and only then does the successor
/// publish — because `uq_pricing_plan_current` permits one current revision and
/// `uq_pricing_plan_open_draft` one open draft.
async fn seed_superseded_chain(provider: &DBProvider<DbError>, plan_id: PlanId, superseded: u64) {
    seed_published_plan(provider, plan_id).await;
    let plans = PlanRepo::new(provider.clone());
    for revision in 0..superseded {
        let next = plans
            .open_revision(&scope(), TENANT, plan_id, stamp())
            .await
            .expect("reopen the plan as a draft");
        assert_eq!(next.revision, revision + 1, "one successor per round");
        supersede_plan_directly(provider, plan_id, revision).await;
        common::publish_plan_directly(provider, &scope(), plan_id, next.revision).await;
    }
}

/// Take the `published -> superseded` flip on one revision, directly.
async fn supersede_plan_directly(provider: &DBProvider<DbError>, plan_id: PlanId, revision: u64) {
    let conn = provider.conn().expect("conn");
    let result = plan::Entity::update_many()
        .secure()
        .scope_with(&scope())
        .col_expr(
            plan::Column::LifecycleState,
            Expr::value(LifecycleState::Superseded.as_str()),
        )
        .filter(
            Condition::all()
                .add(plan::Column::PlanId.eq(plan_id.get()))
                .add(plan::Column::Revision.eq(i64::try_from(revision).expect("a small revision")))
                .add(plan::Column::LifecycleState.eq(LifecycleState::Published.as_str())),
        )
        .exec(&conn)
        .await
        .expect("supersede the seeded plan revision");
    assert_eq!(result.rows_affected, 1, "the seed must have moved one row");
}

/// **The catalogue counts are computed by the engine, and the draft shadow is a
/// subtraction there.**
///
/// On this tier and not the `SQLite` one for this suite's own stated reason. The
/// counts read answers four integers from two grouped statements and takes
/// `COUNT(*)` per state as the count of *plans* in that state — which is only
/// sound because `uq_pricing_plan_open_draft` and `uq_pricing_plan_current` bound
/// a plan to one draft row and one current row, and those are partial indexes
/// that exist on the engine that has them. The second statement subtracts, per
/// current state, the plans that also hold a draft; an `IN (subquery)` and a
/// `GROUP BY` are the two pieces of SQL that the in-memory tier cannot vouch
/// for.
///
/// Three plans, one per bucket, and the middle one is the case the subtraction
/// exists for: published with a draft opened over it, which must count as
/// `draft` and **not** as `published`.
#[tokio::test]
#[ignore = "requires Postgres; run with --ignored"]
async fn the_authoring_counts_group_in_the_engine_and_subtract_the_draft_shadow() {
    let pg = Pg::applied().await;
    let provider = DBProvider::<DbError>::new(pg.db().await);
    let plans = PlanRepo::new(provider.clone());

    // Published, no draft.
    let published = PlanId::new(Uuid::from_u128(0x50_61));
    seed_published_plan(&provider, published).await;

    // Published *and* holding a draft — the shadow.
    let shadowed = PlanId::new(Uuid::from_u128(0x50_62));
    seed_published_plan(&provider, shadowed).await;
    plans
        .open_revision(&scope(), TENANT, shadowed, stamp())
        .await
        .expect("reopen the shadowed plan as a draft");

    // A plain draft, never published.
    let drafted = PlanId::new(Uuid::from_u128(0x50_63));
    plans
        .create_draft(
            &scope(),
            NewPlanDraft {
                plan_name: None,
                plan_id: drafted,
                tenant_id: TENANT,
                created_by: ACTOR,
                created_at_utc: at(10),
                sku_id: None,
                plan_tier: Some("gold".to_owned()),
                billing_cycle: None,
                frequency: None,
                plan_tier_override: false,
                purchase_min_qty: None,
                purchase_max_qty: None,
                invoice_grouping_key: None,
                available_from: None,
                available_to: None,
                cloned_from: None,
                correlation_id: CORRELATION,
            },
        )
        .await
        .expect("create a draft-only plan");

    let conn = provider.conn().expect("conn");
    let counts = count_plans_by_authoring_state(&conn, &scope(), TENANT)
        .await
        .expect("count the catalogue");

    assert_eq!(
        counts.draft, 2,
        "the draft-only plan and the shadowed one: {counts:?}"
    );
    assert_eq!(
        counts.published, 1,
        "only the plan with no draft over it stays published: {counts:?}"
    );
    assert_eq!(counts.retired, 0, "{counts:?}");
    // **Three, not the sum of the buckets.** `count_plans_by_authoring_state`
    // computes `total` as exactly that sum, so comparing against it would assert
    // that a value equals itself; the seeded plan count is what can actually
    // catch a shadowed plan counted twice.
    assert_eq!(
        counts.total, 3,
        "three plans seeded, each in exactly one bucket: {counts:?}"
    );
}
