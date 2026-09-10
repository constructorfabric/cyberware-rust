//! `infra::currency_binding::addon_coverage` against a real database — the
//! storage half of `inst-cb-addon` (`design/04-currency-tax.md` §3, D-95).
//!
//! The domain rule is unit-tested in `domain::currency_binding`; what it judges
//! is a coverage map, and this is where that map is built. The two are worth
//! separating because the rule cannot see a resolution mistake: an add-on
//! resolved to the wrong markets produces a perfectly consistent verdict about
//! the wrong world.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use sea_orm::ActiveValue::Set;
use sea_orm::EntityTrait;
use sea_orm_migration::MigratorTrait;
use toolkit_db::migration_runner::run_migrations_for_testing;
use toolkit_db::secure::{AccessScope, SecureInsertExt};
use toolkit_db::{ConnectOpts, DBProvider, DbError, connect_db};
use uuid::Uuid;

use bss_pricing::domain::instant::utc_ymd_hms;
use bss_pricing::domain::plan_shape::{AddonRule, PlanShape};
use bss_pricing::domain::scope_key::PlanId;
use bss_pricing::infra::currency_binding::addon_coverage;
use bss_pricing::infra::storage::entity::{plan, price};
use bss_pricing::infra::storage::migrations::Migrator;
use time::OffsetDateTime;

const TENANT: Uuid = Uuid::from_u128(0x1111_1111);
const OTHER_TENANT: Uuid = Uuid::from_u128(0x9999_9999);
const SKU: Uuid = Uuid::from_u128(0x05c0_0001);

fn now() -> OffsetDateTime {
    utc_ymd_hms(2026, 8, 6, 12, 0, 0)
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

/// One plan of `tenant`, in `state`, selling `sku`.
async fn seed_plan(provider: &DBProvider<DbError>, tenant: Uuid, plan_id: u128, state: &str) {
    let conn = provider.conn().expect("conn");
    let row = plan::ActiveModel {
        plan_id: Set(Uuid::from_u128(plan_id)),
        revision: Set(1),
        tenant_id: Set(tenant),
        lifecycle_state: Set(state.to_owned()),
        sku_id: Set(Some(SKU)),
        created_by: Set(Uuid::from_u128(0x4444)),
        created_at_utc: Set(now()),
        ..Default::default()
    };
    plan::Entity::insert(row.clone())
        .secure()
        .scope_with_model(&AccessScope::allow_all(), &row)
        .expect("scope")
        .exec(&conn)
        .await
        .expect("seed the plan");
}

/// One row of `plan_id` on a market, in `state`, at `eligibility`.
struct RowSpec<'a> {
    tenant: Uuid,
    plan_id: u128,
    price_id: u128,
    currency: &'a str,
    region: &'a str,
    state: &'a str,
    eligibility: &'a str,
}

async fn seed_row(provider: &DBProvider<DbError>, spec: RowSpec<'_>) {
    let RowSpec {
        tenant,
        plan_id,
        price_id,
        currency,
        region,
        state,
        eligibility,
    } = spec;
    let conn = provider.conn().expect("conn");
    let grandfathered = eligibility == "existing_grandfathered";
    let row = price::ActiveModel {
        price_id: Set(Uuid::from_u128(price_id)),
        tenant_id: Set(tenant),
        plan_id: Set(Uuid::from_u128(plan_id)),
        currency: Set(currency.to_owned()),
        region: Set(region.to_owned()),
        price_overlay: Set("base".to_owned()),
        phase: Set(Uuid::from_u128(0xf1)),
        price_eligibility: Set(eligibility.to_owned()),
        charge_kind: Set("recurring".to_owned()),
        // The biconditional `chk_pricing_price_cohort_eligibility`: a cohort
        // other than `none` iff the row is grandfathered.
        cohort: Set(if grandfathered {
            "1893456000000".to_owned()
        } else {
            "none".to_owned()
        }),
        dimension_key: Set(String::new()),
        tax_inclusive: Set(false),
        lifecycle_state: Set(state.to_owned()),
        created_by: Set(Uuid::from_u128(0x4444)),
        created_at_utc: Set(now()),
        row_version: Set(0),
        ..Default::default()
    };
    price::Entity::insert(row.clone())
        .secure()
        .scope_with_model(&AccessScope::allow_all(), &row)
        .expect("scope")
        .exec(&conn)
        .await
        .expect("seed the price row");
}

/// A shape composing the one add-on SKU under test.
fn shape_composing_the_addon() -> PlanShape {
    let mut shape = PlanShape::new(PlanId::new(Uuid::from_u128(0x91a4)), 1, now());
    shape.addon_rules = vec![AddonRule {
        addon_sku_id: SKU,
        required: true,
        min_qty: None,
        max_qty: None,
        step_qty: None,
        price_override_ref: None,
        depends_on: Vec::new(),
        conflicts_with: Vec::new(),
    }];
    shape
}

async fn markets(provider: &DBProvider<DbError>) -> Vec<String> {
    let conn = provider.conn().expect("conn");
    let coverage = addon_coverage(
        &conn,
        &AccessScope::allow_all(),
        TENANT,
        &shape_composing_the_addon(),
    )
    .await
    .expect("resolve the coverage");
    let mut rendered: Vec<String> = coverage
        .markets_of(SKU)
        .iter()
        .map(|(currency, region)| format!("{}/{region}", currency.as_str()))
        .collect();
    rendered.sort();
    rendered
}

// ---------------------------------------------------------------------------
// The ordinary resolution.
// ---------------------------------------------------------------------------

/// **And the control for the intersection case below.** A fold over one set is
/// that set: an implementation starting from an empty accumulator answers nothing
/// here, market for market, so the intersection's narrowing means narrowing rather
/// than a fold that never carried anything.
#[tokio::test]
async fn an_addon_resolves_to_the_markets_its_plan_publishes() {
    let provider = provider().await;
    seed_plan(&provider, TENANT, 0xa1, "published").await;
    seed_row(
        &provider,
        RowSpec {
            tenant: TENANT,
            plan_id: 0xa1,
            price_id: 0xb1,
            currency: "EUR",
            region: "eu",
            state: "published",
            eligibility: "all_subscriptions",
        },
    )
    .await;
    seed_row(
        &provider,
        RowSpec {
            tenant: TENANT,
            plan_id: 0xa1,
            price_id: 0xb2,
            currency: "USD",
            region: "us",
            state: "published",
            eligibility: "all_subscriptions",
        },
    )
    .await;

    assert_eq!(markets(&provider).await, ["EUR/eu", "USD/us"]);
}

/// **`published` and `retired` are both current** (D-31).
///
/// Reading `retired` as "not published" would make every add-on on a retired plan
/// look uncovered and block its base plan's remediation publish — the rule D-31
/// exists to forbid.
#[tokio::test]
async fn a_retired_plan_still_covers_what_it_covered() {
    let provider = provider().await;
    seed_plan(&provider, TENANT, 0xa1, "retired").await;
    seed_row(
        &provider,
        RowSpec {
            tenant: TENANT,
            plan_id: 0xa1,
            price_id: 0xb1,
            currency: "EUR",
            region: "eu",
            state: "published",
            eligibility: "all_subscriptions",
        },
    )
    .await;

    assert_eq!(markets(&provider).await, ["EUR/eu"]);
}

/// A draft row is not coverage: an order cannot resolve against it.
#[tokio::test]
async fn a_draft_row_is_not_coverage() {
    let provider = provider().await;
    seed_plan(&provider, TENANT, 0xa1, "published").await;
    seed_row(
        &provider,
        RowSpec {
            tenant: TENANT,
            plan_id: 0xa1,
            price_id: 0xb1,
            currency: "EUR",
            region: "eu",
            state: "draft",
            eligibility: "all_subscriptions",
        },
    )
    .await;

    assert_eq!(markets(&provider).await, Vec::<String>::new());
}

/// Grandfathered rows are excluded on the **covering** side, mirroring
/// `sold_markets`' exclusion on the selling side — so the two sides of the
/// comparison mean the same thing (ADR-0002).
#[tokio::test]
async fn a_grandfathered_row_is_not_coverage() {
    let provider = provider().await;
    seed_plan(&provider, TENANT, 0xa1, "published").await;
    seed_row(
        &provider,
        RowSpec {
            tenant: TENANT,
            plan_id: 0xa1,
            price_id: 0xb1,
            currency: "EUR",
            region: "eu",
            state: "published",
            eligibility: "existing_grandfathered",
        },
    )
    .await;

    assert_eq!(markets(&provider).await, Vec::<String>::new());
}

/// **The exclusion is the predicate's, not an empty read's** — Z13-1's fail-open
/// site.
///
/// `a_grandfathered_row_is_not_coverage` above cannot tell "the grandfathered row
/// was excluded" from "no row was read at all", because both answer the empty
/// set. The eligibility predicate is the crate's only `.ne()` over this
/// vocabulary, so it is the one member of the hand-spelled-token class that fails
/// **open**: a spelling that stops matching the stored token makes it match every
/// row, and a grandfathered row then counts as add-on coverage and lets an
/// `inst-cb-addon` publish through that D-95 requires refused.
///
/// So the refusal gets its positive control in the same fixture: one market
/// covered by an eligible row, one market covered only by a grandfathered one.
/// The market that must not appear is named beside a market that must, which is
/// what makes a drifted token show up as a wrong answer rather than as a
/// coincidentally equal one.
#[tokio::test]
async fn a_grandfathered_rows_market_is_excluded_beside_a_sibling_that_covers() {
    let provider = provider().await;
    seed_plan(&provider, TENANT, 0xa1, "published").await;
    seed_row(
        &provider,
        RowSpec {
            tenant: TENANT,
            plan_id: 0xa1,
            price_id: 0xb1,
            currency: "EUR",
            region: "eu",
            state: "published",
            eligibility: "all_subscriptions",
        },
    )
    .await;
    seed_row(
        &provider,
        RowSpec {
            tenant: TENANT,
            plan_id: 0xa1,
            price_id: 0xb2,
            currency: "USD",
            region: "us",
            state: "published",
            eligibility: "existing_grandfathered",
        },
    )
    .await;

    assert_eq!(
        markets(&provider).await,
        ["EUR/eu"],
        "USD/us is covered only by a grandfathered row, which is not coverage"
    );
}

/// Another tenant's plan selling the same SKU contributes nothing.
#[tokio::test]
async fn a_foreign_tenants_plan_does_not_cover_this_tenants_addon() {
    let provider = provider().await;
    seed_plan(&provider, OTHER_TENANT, 0xa2, "published").await;
    seed_row(
        &provider,
        RowSpec {
            tenant: OTHER_TENANT,
            plan_id: 0xa2,
            price_id: 0xb9,
            currency: "EUR",
            region: "eu",
            state: "published",
            eligibility: "all_subscriptions",
        },
    )
    .await;

    assert_eq!(markets(&provider).await, Vec::<String>::new());
}

// ---------------------------------------------------------------------------
// Two plans, one SKU — the operator that decides whether the guard holds.
// ---------------------------------------------------------------------------

/// **Coverage across plans sharing a SKU is the INTERSECTION, not the union.**
///
/// Nothing in the schema makes a SKU's plan unique, and an order resolves the
/// add-on to **one** plan revision — not to the set. So the honest answer to
/// "where can this add-on be bought" for the purpose of a fail-closed guard is
/// the markets *every* candidate covers.
///
/// Under the union: SKU `S` sold by plan A (EUR/eu only) and plan B (USD/us
/// only) covers both, a base plan selling both publishes clean, and a
/// subscription bound to EUR/eu that resolves `S` through plan B dies at order
/// assembly with an unresolvable line. That is the D-95 failure `inst-cb-addon`
/// exists to prevent, reached through the SKU-resolution door. Register `T-15`.
#[tokio::test]
async fn two_plans_selling_one_sku_cover_only_what_both_cover() {
    let provider = provider().await;
    seed_plan(&provider, TENANT, 0xa1, "published").await;
    seed_plan(&provider, TENANT, 0xa2, "published").await;
    // Both cover EUR/eu; only one covers each of the others.
    seed_row(
        &provider,
        RowSpec {
            tenant: TENANT,
            plan_id: 0xa1,
            price_id: 0xb1,
            currency: "EUR",
            region: "eu",
            state: "published",
            eligibility: "all_subscriptions",
        },
    )
    .await;
    seed_row(
        &provider,
        RowSpec {
            tenant: TENANT,
            plan_id: 0xa1,
            price_id: 0xb2,
            currency: "USD",
            region: "us",
            state: "published",
            eligibility: "all_subscriptions",
        },
    )
    .await;
    seed_row(
        &provider,
        RowSpec {
            tenant: TENANT,
            plan_id: 0xa2,
            price_id: 0xb3,
            currency: "EUR",
            region: "eu",
            state: "published",
            eligibility: "all_subscriptions",
        },
    )
    .await;
    seed_row(
        &provider,
        RowSpec {
            tenant: TENANT,
            plan_id: 0xa2,
            price_id: 0xb4,
            currency: "JPY",
            region: "jp",
            state: "published",
            eligibility: "all_subscriptions",
        },
    )
    .await;

    assert_eq!(
        markets(&provider).await,
        ["EUR/eu"],
        "only the market both candidate plans cover is coverage an order can rely on"
    );
}
