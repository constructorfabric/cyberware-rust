//! Draft price rows and their tier bands against a real database.
//!
//! Almost nothing here is a property of a branch in Rust. The compare-and-swap
//! is a conjunction the database evaluates under the row lock; the band set's
//! order is an `ORDER BY` and not an in-memory sort of what the caller happened
//! to hand over; the foreign key decides in which order two DELETEs may run; and
//! the scope-key check has to see rows this process never wrote. A mock would
//! assert that the repository's own `if` fires and would keep asserting it after
//! the predicate that matters had been deleted.
//!
//! Four things get more room than their line count suggests.
//!
//! The **rollback** is driven by a `flat` row carrying bands, which is the only
//! shape that fails *between* the two writes: the row lands, and the band
//! table's kind trigger refuses the statement after it. Every other refusal in
//! this file happens before the first write or after the last, so the whole
//! suite passed with `in_transaction` replaced by sequential statements on a
//! plain connection — the repository's headline claim, "both tables or
//! neither", asserted by nothing.
//!
//! The **band order** is pinned from rows the database physically holds the
//! wrong way round, not from an out-of-order authoring: `create_draft`
//! normalizes before it writes, so a case driven through it would certify an
//! in-memory sort and leave the read side unpinned. `update_draft` does not
//! normalize, so it is what puts descending rows in the table, and the suite
//! reads the table with no `ORDER BY` to prove they are really there.
//!
//! The **occupied-key** cases are driven for a draft, a published and a
//! superseded occupant, because the repository's check answers all three while
//! the two partial `UNIQUE` indexes answer none of them the same way — and the
//! draft index gets a case of its own, driven through the entity rather than the
//! repository, because it is the half that decides a race the check can only
//! read for.
//!
//! The **content write** is exercised by an update that moves every column at
//! once and clears four of them. An update that submitted content identical to
//! what is stored except for the field under test would pass with any other
//! column silently dropped from the UPDATE.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use bss_pricing::domain::concurrency::RowVersion;
use bss_pricing::domain::contracts::{
    AnchorDay, BillingAnchorPolicy, ProrationBasis, ProrationContract,
};
use bss_pricing::domain::error::DomainError;
use bss_pricing::domain::instant::utc_ymd_hms;
use bss_pricing::domain::lifecycle::LifecycleState;
use bss_pricing::domain::money::{CurrencyCode, MinorAmount, RateMinor};
use bss_pricing::domain::price_record::{PriceContent, PriceRecord};
use bss_pricing::domain::price_row::{
    AggregationFunction, AggregationGranularity, BillingGranularity, IncludedAllowance,
    MinQtyUsageFallback, ModelKind, PriceRow, QuantitySource, ReservationFlavor, RolloverPolicy,
    TierAggregationWindow, TierBand, TierQualificationWindow,
};
use bss_pricing::domain::scope_key::{
    ChargeKind, Cohort, DimensionKey, Meter, PhaseId, PlanId, PriceEligibility, Region, ScopeKey,
};
use bss_pricing::domain::tax_display::{RegionReadiness, RegionTaxReadiness};
use bss_pricing::infra::storage::entity::{audit_log, price, price_tier_band, price_window};
use bss_pricing::infra::storage::migrations::Migrator;
use bss_pricing::infra::storage::repo::price_repo::aggregate_authoring_rows_for_plans;
use bss_pricing::infra::storage::repo::{NewPriceDraft, PriceRepo};
use bss_pricing::infra::storage::{RepoError, repo_failure};
use time::OffsetDateTime;

use sea_orm::ActiveValue::Set;
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, EntityTrait};
use sea_orm_migration::MigratorTrait;
use toolkit::api::canonical_prelude::CanonicalError;
use toolkit_db::migration_runner::run_migrations_for_testing;
use toolkit_db::secure::{AccessScope, SecureEntityExt, SecureInsertExt, SecureUpdateExt};
use toolkit_db::{ConnectOpts, DBProvider, DbError, connect_db};
use uuid::Uuid;

/// The readiness a fixture publishes against.
///
/// **A declared default on this suite's one market**, so a seeded publish resolves
/// a category without any case having to be about D-154.
///
/// This was `RegionTaxReadiness::empty()`, under a comment that first justified the
/// resulting NULL as "what a row stating no category in a region declaring no
/// default should carry" — corrected on 2026-08-18 to say the opposite, that
/// `TaxBasisComplete` refuses exactly such a row, `pricing_price`'s migration header calls
/// a NULL on a published row impossible and `trg_pricing_price_append_only` makes it
/// unrepairable, and that the seeder was "a shortcut past a rule this suite is not
/// about". H14 of the 2026-08-19 review closed that shortcut: `publish_rows` now
/// refuses the category half as it already refused the rounding half, so the
/// shortcut is gone and the seeder declares the default instead.
///
/// **`EU` is the whole of it, and that is not a guess**: every key builder this
/// suite publishes through — [`base_key`], [`new_subscriptions_key`],
/// [`grandfathered_key`] and [`usage_key`] on top of `base_key` — is on `EU`. The
/// `market_key("US")`/`("AP")`/`("LA")` rows of the gated-markets cases reach
/// `published` through [`flip_state`], never through `publish_rows`, so they are
/// outside this. A future published row on another market is refused by name here
/// rather than silently freezing a NULL.
///
/// The cases whose subject **is** the resolution build their own readiness with
/// [`readiness_for`] and are untouched — including
/// `publish_freezes_the_effective_tax_category_from_the_readiness_it_judged_with`,
/// whose premise is a row stating no category of its own. That is why the repair is
/// here and not on [`flat_content`]: a category on the shared content builder would
/// have left that case passing while proving nothing.
fn fixture_readiness() -> bss_pricing::domain::tax_display::RegionTaxReadiness {
    readiness_for("EU", Some("standard"))
}

/// One value for a whole test binary: these suites drive a repository or a
/// service directly, where the value the HTTP edge would have established has
/// no producer. What each suite asserts *about* it is stated where it asserts
/// it.
const TEST_CORRELATION: uuid::Uuid = uuid::Uuid::from_u128(0x_c0_11_a7_10);

/// The repository, plus the provider the seeding helper needs to put a row into
/// a state `price_repo::publish_rows` reaches. Both states it fabricates now have
/// real producers — `superseded` gained one with
/// `price_repo::commit_supersession_rows` (D-88's row half) — and it is kept
/// anyway: a case about one row should not have to compose a whole supersession to
/// reach the state it wants to assert against.
async fn harness() -> (PriceRepo, DBProvider<DbError>) {
    let db = connect_db("sqlite::memory:", ConnectOpts::default())
        .await
        .expect("connect in-memory sqlite");
    run_migrations_for_testing(&db, Migrator::migrations())
        .await
        .expect("run migrator");
    let provider = DBProvider::<DbError>::new(db);
    (PriceRepo::new(provider.clone()), provider)
}

fn tenant() -> Uuid {
    Uuid::from_u128(0x7e_11)
}

fn plan() -> PlanId {
    PlanId::new(Uuid::from_u128(0x9_1a4))
}

fn at(hour: u32) -> OffsetDateTime {
    utc_ymd_hms(2026, 8, 2, hour, 0, 0)
}

fn money(units: i64) -> MinorAmount {
    MinorAmount::new(units).expect("a non-negative amount")
}

/// A band rate, stated in whole minor units so these cases read as they always
/// did (D-311). The stored scale is 10^-9 of one.
///
/// Through `from_minor_units` and not a `* 1_000_000_000` literal: `NANO_PER_MINOR`
/// is derived from `RATE_SUB_DECIMALS` so "the scale has exactly one place it can
/// be changed", and a fixture that writes the factor out would build its rows at
/// the old scale while production asserted at the new one — green tests over a
/// 1000x disagreement (Z5-11).
fn rate(minor_units: i64) -> RateMinor {
    RateMinor::from_minor_units(minor_units).expect("a non-negative rate")
}

/// A rate stated in the **stored** 10^-9 scale, for the cases that need two
/// values a scale slip could not confuse.
///
/// [`rate`] above multiplies by 10^9, so every value it makes is a whole number
/// of minor units and divides evenly by the scale factor — which is exactly the
/// shape that nearly defeated D-311's own fix, because a reading at the wrong
/// scale still landed on a plausible number. The values passed here are not.
fn nano_rate(nano_minor: i64) -> RateMinor {
    RateMinor::from_nano_minor(nano_minor).expect("a non-negative rate")
}

/// The default key: `all_subscriptions`, `cohort = none`.
fn base_key(charge_kind: ChargeKind) -> ScopeKey {
    ScopeKey::new(
        plan(),
        CurrencyCode::new("USD").expect("three letters"),
        Region::new("EU").expect("a non-blank region"),
        PhaseId::new(Uuid::from_u128(0xfa_5e)),
        PriceEligibility::AllSubscriptions,
        charge_kind,
        Cohort::None,
    )
    .expect("all_subscriptions pairs with cohort none")
}

/// The third eligibility class's key. It carries `cohort = none` like the
/// default class does — the cohort axis discriminates *retained* generations,
/// and this one retains nobody.
fn new_subscriptions_key(charge_kind: ChargeKind) -> ScopeKey {
    ScopeKey::new(
        plan(),
        CurrencyCode::new("USD").expect("three letters"),
        Region::new("EU").expect("a non-blank region"),
        PhaseId::new(Uuid::from_u128(0xfa_5e)),
        PriceEligibility::NewSubscriptionsOnly,
        charge_kind,
        Cohort::None,
    )
    .expect("new_subscriptions_only pairs with cohort none")
}

/// A grandfathered generation's key.
///
/// The cohort axis cannot move on its own: `cohort != none` **if and only if**
/// `price_eligibility = existing_grandfathered`, enforced by the domain
/// constructor and again by `chk_pricing_price_cohort_eligibility`. So the two
/// axes move together here, which is what a real cutover does.
fn grandfathered_key(charge_kind: ChargeKind, cutover: OffsetDateTime) -> ScopeKey {
    ScopeKey::new(
        plan(),
        CurrencyCode::new("USD").expect("three letters"),
        Region::new("EU").expect("a non-blank region"),
        PhaseId::new(Uuid::from_u128(0xfa_5e)),
        PriceEligibility::ExistingGrandfathered,
        charge_kind,
        Cohort::Generation(cutover),
    )
    .expect("existing_grandfathered pairs with a generation")
}

/// The simplest publishable-looking shape: a flat recurring amount.
fn flat_content() -> PriceContent {
    let mut row = PriceRow::new(ChargeKind::Recurring, Some(ModelKind::Flat));
    row.amount_minor = Some(money(1_000));
    PriceContent {
        row,
        tax_inclusive: false,
        tax_category_ref: None,
        billing_timing: Some("advance".to_owned()),
        proration_contract: None,
        rounding_policy_ref: None,
        grandfather_until: None,
        supersedes_price_id: None,
    }
}

/// A tiered usage row carrying every Slice-3 evaluation-policy field the kind
/// admits, plus the D-45 allowance declaration and the grandfathering horizon —
/// so a mapping that dropped one column fails here rather than at publish.
fn graduated_content() -> PriceContent {
    let mut row = PriceRow::new(ChargeKind::Usage, Some(ModelKind::Graduated));
    row.bands = vec![
        TierBand::closed(0, 100, rate(0)),
        TierBand::closed(100, 1_000, rate(25)),
        TierBand::open(1_000, rate(10)),
    ];
    row.meter = Some("api_calls".to_owned());
    "region:eu".clone_into(&mut row.dimension_key);
    row.billing_granularity = Some(BillingGranularity::PerHour);
    row.tier_aggregation_window = Some(TierAggregationWindow::CalendarMonth);
    row.tier_qualification_window = Some(TierQualificationWindow::TrailingPeriod);
    row.aggregation_function = Some(AggregationFunction::TimeWeighted);
    row.aggregation_granularity = Some(AggregationGranularity::Hour);
    row.max_hold_granules = Some(6);
    row.included_allowance = Some(IncludedAllowance {
        quantity: 50,
        rollover_policy: RolloverPolicy::Carry,
    });
    // Slice 10's six columns. Authored here rather than in a case of their own
    // because this fixture's whole job is that **every** content column is
    // non-default, so a column the store drops is a changed value rather than a
    // hole staying a hole. Their absence was found by probe: dropping
    // `discount_ref` on the read path reddened nothing in the entire fast suite.
    row.reserved_rate = Some(rate(3));
    row.reservation_flavor = Some(ReservationFlavor::Capacity);
    row.min_qty_purchase = Some(7);
    row.min_qty_usage = Some(11);
    row.min_qty_usage_fallback = Some(MinQtyUsageFallback::Exception);
    row.discount_ref = Some("promo/spring".to_owned());
    PriceContent {
        row,
        tax_inclusive: true,
        tax_category_ref: None,
        billing_timing: Some("arrears".to_owned()),
        // Authored, for this fixture's own stated reason: it was `None` at every
        // one of its six sites in this file and `Some` at none, so the case whose
        // comment says "every content column this kind can carry moves at once"
        // never moved the proration contract and a store that dropped all three of
        // its members stayed green. `FixedDay` rather than `CalendarMonth` because
        // it is the one variant carrying a second fact -- the day -- and a mapping
        // that stored the token and lost the day is the defect that shape has.
        proration_contract: Some(ProrationContract {
            billing_anchor_policy: BillingAnchorPolicy::FixedDay(
                AnchorDay::new(14).expect("a day of the month"),
            ),
            proration_basis: ProrationBasis::CalendarDays30,
            credit_on_downgrade: true,
        }),
        rounding_policy_ref: Some("half_even".to_owned()),
        grandfather_until: Some(at(23)),
        supersedes_price_id: Some(Uuid::from_u128(0xb_0f)),
    }
}

fn draft(price_id: Uuid, scope_key: ScopeKey, content: PriceContent) -> NewPriceDraft {
    NewPriceDraft {
        price_id,
        scope_key,
        content,
        created_by: Uuid::from_u128(0xac_10),
        created_at_utc: at(10),
        correlation_id: TEST_CORRELATION,
    }
}

/// Move a row's `lifecycle_state` directly.
///
/// `draft -> published` is `price_repo::publish_rows`'s flip and
/// `published -> superseded` is `commit_supersession_rows`'s; both are fabricated
/// here so a suite about one row needs neither a publish unit nor a composed
/// supersession. The append-only trigger permits both: it fires only when the row
/// is already past `draft`, and `published -> superseded` is one of the two flips
/// it whitelists. (The reason stated here used to be "the trigger fires only when the
/// row is already past `draft`", which D-153 falsified: the trigger gained a draft
/// branch on both backends. It still *permits* both flips, so the fabrication is legal
/// — corrected 2026-08-05.)
async fn flip_state(
    provider: &DBProvider<DbError>,
    scope: &AccessScope,
    price_id: Uuid,
    state: LifecycleState,
) {
    let conn = provider.conn().expect("conn");
    let result = price::Entity::update_many()
        .secure()
        .scope_with(scope)
        .col_expr(price::Column::LifecycleState, Expr::value(state.as_str()))
        .filter(Condition::all().add(price::Column::PriceId.eq(price_id)))
        .exec(&conn)
        .await
        .expect("flip the lifecycle state");
    assert_eq!(result.rows_affected, 1, "the seed must have moved one row");
}

/// The band rows physically present under `price_id`, whatever the repository
/// would say about them.
async fn stored_bands(
    provider: &DBProvider<DbError>,
    scope: &AccessScope,
    price_id: Uuid,
) -> Vec<price_tier_band::Model> {
    let conn = provider.conn().expect("conn");
    price_tier_band::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(Condition::all().add(price_tier_band::Column::PriceId.eq(price_id)))
        .all(&conn)
        .await
        .expect("read the band table directly")
}

#[tokio::test]
async fn a_created_row_and_its_bands_read_back_whole() {
    let (repo, _provider) = harness().await;
    let scope = AccessScope::for_tenant(tenant());
    let price_id = Uuid::from_u128(0xb_10);
    let key = grandfathered_key(ChargeKind::Usage, at(9));

    let created = repo
        .create_draft(
            &scope,
            tenant(),
            draft(price_id, key.clone(), graduated_content()),
        )
        .await
        .expect("create the draft row");

    // Draft at version 0: the two facts every later assertion is measured
    // against.
    assert_eq!(created.lifecycle_state, LifecycleState::Draft);
    assert_eq!(created.row_version, RowVersion::new(0));

    let read = repo
        .find(&scope, tenant(), price_id)
        .await
        .expect("read")
        .expect("the row just created is there");

    // Field for field. A mapping that dropped a column would still round-trip
    // the identity, and the drop would only surface at publish — or, for
    // `included_allowance`, as a D-129 supersession guard reporting that
    // nothing had changed.
    assert_eq!(read, created);
    // **The key the row is filed under carries the line the content named**
    // (D-196 clause 3): `ScopeKeyRequest` has no meter member, so the row's own
    // fields are the author's only way to state the line and the door derives
    // the ninth and tenth axes from them. The key handed in here names no line;
    // the key that comes back does, and it is the same key the two scope-key
    // indexes hold the row under.
    assert_eq!(
        read.scope_key,
        key.clone()
            .with_usage_line(
                Some(Meter::new("api_calls").expect("a non-blank meter")),
                DimensionKey::new("region:eu"),
            )
            .expect("a usage key carries its line")
    );
    assert_eq!(read.row.model_kind, Some(ModelKind::Graduated));
    assert_eq!(read.row.charge_kind, ChargeKind::Usage);
    assert_eq!(read.row.meter.as_deref(), Some("api_calls"));
    assert_eq!(read.row.dimension_key, "region:eu");
    assert_eq!(
        read.row.billing_granularity,
        Some(BillingGranularity::PerHour)
    );
    assert_eq!(
        read.row.tier_aggregation_window,
        Some(TierAggregationWindow::CalendarMonth)
    );
    assert_eq!(
        read.row.tier_qualification_window,
        Some(TierQualificationWindow::TrailingPeriod)
    );
    assert_eq!(
        read.row.aggregation_function,
        Some(AggregationFunction::TimeWeighted)
    );
    assert_eq!(
        read.row.aggregation_granularity,
        Some(AggregationGranularity::Hour)
    );
    assert_eq!(read.row.max_hold_granules, Some(6));
    assert_eq!(
        read.row.included_allowance,
        Some(IncludedAllowance {
            quantity: 50,
            rollover_policy: RolloverPolicy::Carry,
        })
    );
    assert_eq!(read.row.reserved_rate, Some(rate(3)));
    assert_eq!(
        read.row.reservation_flavor,
        Some(ReservationFlavor::Capacity)
    );
    assert_eq!(read.row.min_qty_purchase, Some(7));
    assert_eq!(read.row.min_qty_usage, Some(11));
    assert_eq!(
        read.row.min_qty_usage_fallback,
        Some(MinQtyUsageFallback::Exception)
    );
    assert_eq!(read.row.discount_ref.as_deref(), Some("promo/spring"));
    assert_eq!(read.row.bands, graduated_content().row.bands);
    assert!(read.tax_inclusive);
    assert_eq!(read.billing_timing.as_deref(), Some("arrears"));
    assert_eq!(read.rounding_policy_ref.as_deref(), Some("half_even"));
    assert_eq!(read.grandfather_until, Some(at(23)));
    assert_eq!(read.supersedes_price_id, Some(Uuid::from_u128(0xb_0f)));
    assert_eq!(read.created_by, Uuid::from_u128(0xac_10));
    assert_eq!(read.created_at_utc, at(10));
}

#[tokio::test]
async fn the_band_set_comes_back_in_quantity_order_however_the_rows_were_written() {
    let (repo, provider) = harness().await;
    let scope = AccessScope::for_tenant(tenant());
    let price_id = Uuid::from_u128(0xb_11);

    let ascending = vec![
        TierBand::closed(0, 100, rate(0)),
        TierBand::closed(100, 1_000, rate(25)),
        TierBand::open(1_000, rate(10)),
    ];
    let descending = vec![
        TierBand::open(1_000, rate(10)),
        TierBand::closed(100, 1_000, rate(25)),
        TierBand::closed(0, 100, rate(0)),
    ];

    let created = repo
        .create_draft(
            &scope,
            tenant(),
            draft(
                price_id,
                grandfathered_key(ChargeKind::Usage, at(9)),
                graduated_content(),
            ),
        )
        .await
        .expect("create");
    assert_eq!(created.row.bands, ascending);

    // `update_draft` does **not** normalize: it deletes the band set and writes
    // the caller's, row by row, in the order given. So this update is what
    // actually puts descending rows into the table, and it is the only way this
    // suite can reach that state — `create_draft` sorts first, so a test driven
    // through it would prove an in-memory sort and leave the read side unpinned.
    let mut content = graduated_content();
    content.row.bands = descending.clone();
    repo.update_draft(
        &scope,
        tenant(),
        price_id,
        RowVersion::new(0),
        content,
        stamp(),
        /* on_behalf_of */ None,
    )
    .await
    .expect("replace the band set, descending");

    // The physical rows really are the wrong way round: this reads the table
    // with no ORDER BY at all, so it is measuring what the repository has to
    // correct rather than restating what it did.
    //
    // An unordered read has no contract, which is the point and also the risk:
    // the literal below is the order this engine happens to return, so an engine
    // that came back sorted would fail here loudly instead of leaving the case
    // below silently measuring nothing. No column would help — the key note
    // under the read says why.

    let physical: Vec<u64> = stored_bands(&provider, &scope, price_id)
        .await
        .iter()
        .map(|band| u64::try_from(band.from_qty).expect("a non-negative bound"))
        .collect();
    assert_eq!(
        physical,
        vec![1_000, 100, 0],
        "the update must have written the rows in the order it was given, \
         or this test is no longer measuring the read-side guarantee"
    );

    // And a read still answers ascending. The table carries **no ordinal**:
    // `uq_pricing_price_tier_band_lower_bound` is `UNIQUE (price_id, from_qty)`
    // and the `PRIMARY KEY` is `band_id`, which `price_repo::band_id` derives as
    // `Uuid::new_v5(price_id, from_qty)` — the same pair again, hashed, and so no
    // more a record of authoring order than the pair itself. Authoring order
    // therefore does not survive persistence; `TierBandValidator` judges geometry
    // over the set sorted by `from_qty` for that reason, and a repository that
    // answered in stored order would let a row pass the save-time pre-check and
    // fail the identical re-run inside the publish commit.

    let read = repo
        .find(&scope, tenant(), price_id)
        .await
        .expect("read")
        .expect("present");
    assert_eq!(read.row.bands, ascending);
}

#[tokio::test]
async fn the_per_kind_money_columns_round_trip_on_the_kinds_that_carry_them() {
    let (repo, _provider) = harness().await;
    let scope = AccessScope::for_tenant(tenant());

    // `package`: the money lives in the block columns, which
    // `chk_pricing_price_package_fields_kind` permits on this kind alone.
    let mut package = PriceRow::new(ChargeKind::Usage, Some(ModelKind::Package));
    package.package_size = Some(1_000);
    package.package_price_minor = Some(money(4_999));
    package.meter = Some("gb_egress".to_owned());
    package.billing_granularity = Some(BillingGranularity::WholeUnit);
    package.tier_aggregation_window = Some(TierAggregationWindow::InvoicePeriod);
    let package_id = Uuid::from_u128(0xb_20);
    repo.create_draft(
        &scope,
        tenant(),
        draft(
            package_id,
            base_key(ChargeKind::Usage),
            PriceContent {
                row: package,
                tax_inclusive: false,
                tax_category_ref: None,
                billing_timing: None,
                proration_contract: None,
                rounding_policy_ref: None,
                grandfather_until: None,
                supersedes_price_id: None,
            },
        ),
    )
    .await
    .expect("create the package row");

    // `per_unit` on a non-usage row: the unit **rate** on the row, and the
    // quantity the subscription cannot supply stated as a fixed one.
    //
    // The rate and not `amount_minor`. This fixture authored `amount_minor =
    // money(1_500)` with no `unit_rate` until 2026-08-20 — the pre-D-311 spelling
    // that `check_amount_placement` (`src/domain/rules/model_kind.rs`) refuses on
    // *both* counts: `amount_minor` must be NULL on a `per_unit` row and
    // `unitRateNanoMinor` must be present, because two priced columns are two
    // competing prices. The single test named for the per-kind money columns was
    // therefore certifying as canonical the one shape the publish rule rejects.
    // `an_update_reaches_the_per_kind_money_columns_too` was corrected on
    // 2026-08-14 and this one was not.
    let mut per_unit = PriceRow::new(ChargeKind::Recurring, Some(ModelKind::PerUnit));
    per_unit.unit_rate = Some(nano_rate(1_500_000_000));
    per_unit.quantity_source = Some(QuantitySource::Manual);
    per_unit.manual_quantity = Some(12);
    let per_unit_id = Uuid::from_u128(0xb_21);
    repo.create_draft(
        &scope,
        tenant(),
        draft(
            per_unit_id,
            base_key(ChargeKind::Recurring),
            PriceContent {
                row: per_unit,
                tax_inclusive: false,
                tax_category_ref: None,
                billing_timing: Some("advance".to_owned()),
                proration_contract: None,
                rounding_policy_ref: None,
                grandfather_until: None,
                supersedes_price_id: None,
            },
        ),
    )
    .await
    .expect("create the per-unit row");

    let read = repo
        .find(&scope, tenant(), package_id)
        .await
        .expect("read")
        .expect("present");
    assert_eq!(read.row.package_size, Some(1_000));
    assert_eq!(read.row.package_price_minor, Some(money(4_999)));
    assert_eq!(read.row.amount_minor, None);
    // Both rows were authored tax-exclusive. The rich round-trip above reads
    // `true`, so without this a mapping that answered a constant would survive
    // the suite.
    assert!(!read.tax_inclusive);

    let read = repo
        .find(&scope, tenant(), per_unit_id)
        .await
        .expect("read")
        .expect("present");
    assert_eq!(read.row.unit_rate, Some(nano_rate(1_500_000_000)));
    assert_eq!(
        read.row.amount_minor, None,
        "and the amount column stays empty: a `per_unit` row that carried both would be two \
         competing prices, which is what D-311 separated the columns to make unauthorable"
    );
    assert_eq!(read.row.quantity_source, Some(QuantitySource::Manual));
    assert_eq!(read.row.manual_quantity, Some(12));
    assert!(!read.tax_inclusive);
}

#[tokio::test]
async fn a_key_held_by_a_draft_or_a_published_row_takes_no_second_row() {
    let (repo, provider) = harness().await;
    let scope = AccessScope::for_tenant(tenant());
    let first = Uuid::from_u128(0xb_30);
    let second = Uuid::from_u128(0xb_31);
    let key = base_key(ChargeKind::Recurring);

    repo.create_draft(&scope, tenant(), draft(first, key.clone(), flat_content()))
        .await
        .expect("the first row takes the key");

    // The draft case is this repository's alone: `uq_pricing_price_scope_key_current`
    // is partial over `published` and cannot see it. Two drafts on one key is
    // the ambiguity publish would fail on, found a round trip earlier.
    let err = repo
        .create_draft(&scope, tenant(), draft(second, key.clone(), flat_content()))
        .await
        .expect_err("a second draft on one key must be refused");
    let RepoError::DuplicateScopeKey(detail) = err else {
        panic!("an occupied key must refuse with DUPLICATE_SCOPE_KEY");
    };
    assert!(detail.starts_with(&key.to_string()), "got: {detail}");
    assert!(detail.contains("draft"), "got: {detail}");
    assert!(detail.contains(&first.to_string()), "got: {detail}");

    // Publishing the occupant does not free the key either.
    flip_state(&provider, &scope, first, LifecycleState::Published).await;
    let err = repo
        .create_draft(&scope, tenant(), draft(second, key, flat_content()))
        .await
        .expect_err("a draft may not land on a published key");
    let RepoError::DuplicateScopeKey(detail) = err else {
        panic!("an occupied key must refuse with DUPLICATE_SCOPE_KEY");
    };
    assert!(detail.contains("published"), "got: {detail}");
}

#[tokio::test]
async fn a_superseded_row_no_longer_holds_its_key() {
    let (repo, provider) = harness().await;
    let scope = AccessScope::for_tenant(tenant());
    let predecessor = Uuid::from_u128(0xb_40);
    let successor = Uuid::from_u128(0xb_41);
    let key = base_key(ChargeKind::Recurring);

    repo.create_draft(
        &scope,
        tenant(),
        draft(predecessor, key.clone(), flat_content()),
    )
    .await
    .expect("create");
    flip_state(&provider, &scope, predecessor, LifecycleState::Published).await;
    flip_state(&provider, &scope, predecessor, LifecycleState::Superseded).await;

    // A superseded row is retained history and is not the **current** row on
    // its key (§4.3). Treating it as an occupant would make a key unusable
    // forever after its first reprice.
    repo.create_draft(&scope, tenant(), draft(successor, key, flat_content()))
        .await
        .expect("a superseded predecessor leaves the key free");
}

#[tokio::test]
async fn a_different_charge_kind_or_cohort_is_a_different_key() {
    let (repo, _provider) = harness().await;
    let scope = AccessScope::for_tenant(tenant());

    repo.create_draft(
        &scope,
        tenant(),
        draft(
            Uuid::from_u128(0xb_50),
            base_key(ChargeKind::Recurring),
            flat_content(),
        ),
    )
    .await
    .expect("the recurring component");

    // §4.1: a hybrid plan legitimately holds a `recurring` **and** a `usage`
    // row on one plan, currency, region and phase. Without `chargeKind` in the
    // key the second would be rejected as a duplicate of the first.
    let mut usage = flat_content();
    usage.row = PriceRow::new(ChargeKind::Usage, Some(ModelKind::PerUnit));
    usage.row.amount_minor = Some(money(5));
    usage.row.meter = Some("api_calls".to_owned());
    usage.row.billing_granularity = Some(BillingGranularity::WholeUnit);
    repo.create_draft(
        &scope,
        tenant(),
        draft(Uuid::from_u128(0xb_51), base_key(ChargeKind::Usage), usage),
    )
    .await
    .expect("the usage component is a different key");

    // And every cutover mints a **new** generation on its own key, so retained
    // generations never collide with each other or with the successor.
    let mut retained = flat_content();
    retained.grandfather_until = Some(at(23));
    repo.create_draft(
        &scope,
        tenant(),
        draft(
            Uuid::from_u128(0xb_52),
            grandfathered_key(ChargeKind::Recurring, at(9)),
            retained.clone(),
        ),
    )
    .await
    .expect("the first retained generation");
    repo.create_draft(
        &scope,
        tenant(),
        draft(
            Uuid::from_u128(0xb_53),
            grandfathered_key(ChargeKind::Recurring, at(12)),
            retained,
        ),
    )
    .await
    .expect("a second cutover is a second generation, not a duplicate");
}

#[tokio::test]
async fn a_new_subscriptions_only_row_round_trips_and_is_its_own_key() {
    let (repo, _provider) = harness().await;
    let scope = AccessScope::for_tenant(tenant());
    let new_only = Uuid::from_u128(0xb_58);
    let all = Uuid::from_u128(0xb_59);

    // The third normative class (PRD 1.4 glossary + 6.9, AC #59, S7 W3 /
    // `inst-el-fields`). It existed in the design set and in neither the enum,
    // the CHECK nor the repository's inverse list, so a row carrying it could
    // not be authored — and had one reached the table any read of it would have
    // answered `CorruptRow` forever.
    let created = repo
        .create_draft(
            &scope,
            tenant(),
            draft(
                new_only,
                new_subscriptions_key(ChargeKind::Recurring),
                flat_content(),
            ),
        )
        .await
        .expect("a new_subscriptions_only row is authorable");

    let read = repo
        .find(&scope, tenant(), new_only)
        .await
        .expect("read")
        .expect("present");
    assert_eq!(read, created);
    assert_eq!(
        read.scope_key.price_eligibility(),
        PriceEligibility::NewSubscriptionsOnly
    );
    assert_eq!(read.scope_key.cohort(), Cohort::None);

    // And it is a **different key** from the `all_subscriptions` row on every
    // other axis it shares, which is what lets both hold a current row at once —
    // the state W3's most-specific-wins rule exists to resolve. An eligibility
    // axis missing from the duplicate check would refuse this second row as a
    // duplicate and make the promo class unauthorable beside its base.
    repo.create_draft(
        &scope,
        tenant(),
        draft(all, base_key(ChargeKind::Recurring), flat_content()),
    )
    .await
    .expect("the all_subscriptions row on the same remaining axes is a second key");

    let both = repo
        .list_for_plan(&scope, tenant(), plan(), &[LifecycleState::Draft])
        .await
        .expect("list drafts");
    assert_eq!(both.len(), 2, "both classes hold a row of their own");
    assert_ne!(both[0].scope_key, both[1].scope_key);
}

#[tokio::test]
async fn a_grandfathering_horizon_off_its_class_is_the_callers_mistake_not_the_stores() {
    let (repo, _provider) = harness().await;
    let scope = AccessScope::for_tenant(tenant());
    let price_id = Uuid::from_u128(0xb_5a);

    // `grandfather_until` is ordinary draft content and no domain rule reads it,
    // while `chk_pricing_price_grandfather_until` pairs it with the
    // `existing_grandfathered` class. Without the repository's own refusal the
    // pairing is discovered by the driver: `RepoError::Db`, `DomainError::Internal`,
    // a 500 for a request whose author only has to clear one field.
    let mut content = flat_content();
    content.grandfather_until = Some(at(23));
    let err = repo
        .create_draft(
            &scope,
            tenant(),
            draft(price_id, base_key(ChargeKind::Recurring), content.clone()),
        )
        .await
        .expect_err("a horizon on an all_subscriptions key must be refused");
    assert_eq!(
        err,
        RepoError::GrandfatherHorizonOffClass {
            eligibility: "all_subscriptions".to_owned(),
        }
    );

    // D-147 made the pairing a rule, so the refusal reaches a consumer under a
    // code of its own instead of the generic bad-request answer it borrowed
    // while no document stated it. The code is what a client branches on: it
    // names the field to clear, which the shared answer never did.
    // **The code off the typed context, by equality.** A `contains` over the
    // rendered document is satisfied by the code with a character appended — the
    // weak form `sqlite_publish_commit.rs` records as having let `WINDOW_OVERLAPX`
    // pass as `WINDOW_OVERLAP` — and over `Debug` of the whole error it is looser
    // still, since any field that quotes the reason satisfies it.
    let mapped = CanonicalError::from(repo_failure(&err));
    assert_eq!(
        mapped.status_code(),
        400,
        "an architectural 422 reaches the wire as a 400 carrying its code"
    );
    assert_eq!(
        canonical_reason(&mapped),
        "GRANDFATHER_UNTIL_FORBIDDEN",
        "got: {mapped:?}"
    );
    assert!(
        format!("{mapped:?}").contains("grandfather_until"),
        "and it names the field to clear: {mapped:?}"
    );

    // The third class may not carry one either: the horizon expires a *retained
    // generation*, and this class retains nobody.
    let err = repo
        .create_draft(
            &scope,
            tenant(),
            draft(
                price_id,
                new_subscriptions_key(ChargeKind::Recurring),
                content.clone(),
            ),
        )
        .await
        .expect_err("a horizon on a new_subscriptions_only key must be refused");
    assert_eq!(
        err,
        RepoError::GrandfatherHorizonOffClass {
            eligibility: "new_subscriptions_only".to_owned(),
        }
    );

    // Refused before the transaction opens, so the key is still free.
    assert_eq!(
        repo.find(&scope, tenant(), price_id).await.expect("read"),
        None
    );
    let mut clean = flat_content();
    clean.grandfather_until = None;
    repo.create_draft(
        &scope,
        tenant(),
        draft(price_id, base_key(ChargeKind::Recurring), clean),
    )
    .await
    .expect("clearing the horizon is the whole remedy");

    // The same refusal on the update path, where the class comes from the
    // **stored** row rather than the submitted key — which is why the check
    // cannot be the create path's alone.
    let err = repo
        .update_draft(
            &scope,
            tenant(),
            price_id,
            RowVersion::new(0),
            content,
            stamp(),
            /* on_behalf_of */ None,
        )
        .await
        .expect_err("a horizon submitted onto a non-grandfathered row must be refused");
    assert_eq!(
        err,
        RepoError::GrandfatherHorizonOffClass {
            eligibility: "all_subscriptions".to_owned(),
        }
    );
    let read = repo
        .find(&scope, tenant(), price_id)
        .await
        .expect("read")
        .expect("present");
    assert_eq!(
        read.grandfather_until, None,
        "the refused edit left nothing"
    );
    assert_eq!(read.row_version, RowVersion::new(0), "and moved no tag");

    // And the class that may carry one still does.
    let grandfathered = Uuid::from_u128(0xb_5b);
    let mut retained = flat_content();
    retained.grandfather_until = Some(at(23));
    repo.create_draft(
        &scope,
        tenant(),
        draft(
            grandfathered,
            grandfathered_key(ChargeKind::Recurring, at(9)),
            retained,
        ),
    )
    .await
    .expect("a grandfathered generation carries its horizon");
}

#[tokio::test]
async fn an_authored_instant_finer_than_the_quantum_is_refused_on_both_write_paths() {
    let (repo, _provider) = harness().await;
    let scope = AccessScope::for_tenant(tenant());
    let price_id = Uuid::from_u128(0xb_5d);

    // The horizon is authored content and the column would take a finer value in
    // silence, which is exactly how a truncating producer and a non-truncating
    // consumer end up agreeing until the day they do not (D-144).
    let mut content = flat_content();
    content.grandfather_until = Some(at(23) + time::Duration::microseconds(1));
    let err = repo
        .create_draft(
            &scope,
            tenant(),
            draft(
                price_id,
                grandfathered_key(ChargeKind::Recurring, at(9)),
                content.clone(),
            ),
        )
        .await
        .expect_err("a sub-millisecond horizon must be refused");
    assert!(
        matches!(
            &err,
            RepoError::TimestampPrecisionExceeded { field, .. } if field == "grandfatherUntil"
        ),
        "got: {err:?}"
    );
    let mapped = CanonicalError::from(repo_failure(&err));
    assert_eq!(
        canonical_reason(&mapped),
        "TIMESTAMP_PRECISION_EXCEEDED",
        "got: {mapped:?}"
    );
    assert_eq!(mapped.status_code(), 400);

    // Refused before anything was written, and rounding to the quantum is the
    // whole remedy — the value is not moved on the author's behalf.
    assert_eq!(
        repo.find(&scope, tenant(), price_id).await.expect("read"),
        None
    );
    content.grandfather_until = Some(at(23));
    repo.create_draft(
        &scope,
        tenant(),
        draft(
            price_id,
            grandfathered_key(ChargeKind::Recurring, at(9)),
            content.clone(),
        ),
    )
    .await
    .expect("an instant on the quantum is storable");

    // **And the edit path refuses it too**, which is the half that matters most
    // on this plane: tightening `grandfather_until` on a draft row is the
    // sanctioned authoring move (`inst-gs-tighten`), so it is the likely way a
    // sub-millisecond horizon actually arrives. Guarding only creation would
    // leave the store one `PATCH` away from a column holding an instant finer
    // than the one the catalog compares at — and `timestamptz` takes it in
    // silence, so nothing downstream would ever report it.
    content.grandfather_until = Some(at(20) + time::Duration::microseconds(1));
    let err = repo
        .update_draft(
            &scope,
            tenant(),
            price_id,
            RowVersion::new(0),
            content,
            stamp(),
            /* on_behalf_of */ None,
        )
        .await
        .expect_err("a sub-millisecond horizon must be refused on the edit path too");
    assert!(
        matches!(
            &err,
            RepoError::TimestampPrecisionExceeded { field, .. } if field == "grandfatherUntil"
        ),
        "got: {err:?}"
    );

    // Refused ahead of the compare-and-swap, so the row keeps the horizon it
    // had and its tag never moved — an author who resubmits at the quantum is
    // still holding a current `ETag`.
    let read = repo
        .find(&scope, tenant(), price_id)
        .await
        .expect("read")
        .expect("present");
    assert_eq!(
        read.grandfather_until,
        Some(at(23)),
        "the refused edit left nothing"
    );
    assert_eq!(read.row_version, RowVersion::new(0), "and moved no tag");

    // The cohort axis is refused by the key itself, one layer earlier: it is
    // matched for equality against an instant another gear produced, so an
    // unquantized generation would build a key nobody can find.
    let err = ScopeKey::new(
        plan(),
        CurrencyCode::new("USD").expect("three letters"),
        Region::new("EU").expect("a non-blank region"),
        PhaseId::new(Uuid::from_u128(0xfa_5e)),
        PriceEligibility::ExistingGrandfathered,
        ChargeKind::Recurring,
        Cohort::Generation(at(9) + time::Duration::nanoseconds(1)),
    )
    .expect_err("a sub-millisecond cutover cannot become an axis value");
    assert!(matches!(err, DomainError::TimestampPrecisionExceeded(_)));
    assert_eq!(
        canonical_reason(&CanonicalError::from(err)),
        "TIMESTAMP_PRECISION_EXCEEDED"
    );
}

/// One [`CanonicalError`]'s declared reason code, off its typed context.
///
/// Each variant carries its own context type, so the arms cannot be folded; the two
/// here are the classes this plane's refusals map to. The code is read by equality
/// rather than by `contains` over a rendering — see the call sites for why the
/// difference matters.
fn canonical_reason(error: &CanonicalError) -> &str {
    match error {
        // `with_precondition_violation(subject, description, type_)` files the gear
        // code as the violation's `type_`; the §5 codes on this plane are all
        // architectural 422s rendered 400 through this variant.
        CanonicalError::FailedPrecondition { ctx, .. } => ctx
            .violations
            .first()
            .unwrap_or_else(|| panic!("a precondition failure with no violation: {error:?}"))
            .type_
            .as_str(),
        CanonicalError::Aborted { ctx, .. } => ctx.reason.as_str(),
        other => panic!(
            "the refusals on this plane reach the wire as a precondition failure or a conflict \
             carrying a reason code; got {other:?}"
        ),
    }
}

#[tokio::test]
async fn a_draft_may_shed_its_bands_and_its_tiered_kind_in_one_edit() {
    let (repo, provider) = harness().await;
    let scope = AccessScope::for_tenant(tenant());
    let price_id = Uuid::from_u128(0xb_5c);

    repo.create_draft(
        &scope,
        tenant(),
        draft(
            price_id,
            grandfathered_key(ChargeKind::Usage, at(9)),
            graduated_content(),
        ),
    )
    .await
    .expect("create the tiered row");

    // The edit the parent-side kind guard could have broken: `graduated` with
    // three bands becomes `flat` with none. The guard refuses a row that still
    // carries bands leaving the tiered kinds, so this only works because the
    // band set is replaced **before** the row moves. Written as an ordinary
    // authoring edit rather than as a schema case, because that is what it is.
    let mut content = flat_content();
    // The row was authored as a metered usage line, and the line is an axis
    // since D-196 clause (3): an edit that dropped it would be asking to move
    // the row to another key, which this door refuses. Carried forward
    // explicitly so this case stays about the band set and the kind.
    content.row.meter = Some("api_calls".to_owned());
    content.row.dimension_key = "region:eu".to_owned();
    content.grandfather_until = Some(at(23));
    repo.update_draft(
        &scope,
        tenant(),
        price_id,
        RowVersion::new(0),
        content,
        stamp(),
        /* on_behalf_of */ None,
    )
    .await
    .expect("a tiered draft may become a flat one in a single edit");

    let read = repo
        .find(&scope, tenant(), price_id)
        .await
        .expect("read")
        .expect("present");
    assert_eq!(read.row.model_kind, Some(ModelKind::Flat));
    assert!(read.row.bands.is_empty());
    assert!(
        stored_bands(&provider, &scope, price_id).await.is_empty(),
        "and the band rows really are gone"
    );

    // The submitted content said `recurring` and the key says `usage`, and the
    // key wins — silently, because the axis is stored once and an update may not
    // move the key it would have to move to honour the other answer. The create
    // path documents this; the update path did not, and this is what it does.
    assert_eq!(read.row.charge_kind, ChargeKind::Usage);
    assert_eq!(read.scope_key.charge_kind(), ChargeKind::Usage);
}

#[tokio::test]
async fn every_other_axis_that_can_move_is_a_different_key_too() {
    let (repo, _provider) = harness().await;
    let scope = AccessScope::for_tenant(tenant());

    repo.create_draft(
        &scope,
        tenant(),
        draft(
            Uuid::from_u128(0xb_c0),
            base_key(ChargeKind::Recurring),
            flat_content(),
        ),
    )
    .await
    .expect("the baseline row");

    // The four axes the test above leaves standing. An axis dropped from the
    // duplicate check's filter would compare fewer columns than the key has and
    // **over-refuse** — rejecting a row on a genuinely different key as a
    // duplicate — which no test that varies only `chargeKind` and `cohort` can
    // see. `priceOverlay` is the one axis with nothing to vary: every row this
    // gear authors carries `base`, and the column has a CHECK saying so.
    let other_plan = PlanId::new(Uuid::from_u128(0x9_1a5));
    let variants = [
        (
            Uuid::from_u128(0xb_c1),
            other_plan,
            "USD",
            "EU",
            0xfa_5e_u128,
        ),
        (Uuid::from_u128(0xb_c2), plan(), "EUR", "EU", 0xfa_5e),
        (Uuid::from_u128(0xb_c3), plan(), "USD", "US", 0xfa_5e),
        (Uuid::from_u128(0xb_c4), plan(), "USD", "EU", 0xfa_5f),
    ];
    for (price_id, plan_id, currency, region, phase) in variants {
        let key = ScopeKey::new(
            plan_id,
            CurrencyCode::new(currency).expect("three letters"),
            Region::new(region).expect("a non-blank region"),
            PhaseId::new(Uuid::from_u128(phase)),
            PriceEligibility::AllSubscriptions,
            ChargeKind::Recurring,
            Cohort::None,
        )
        .expect("all_subscriptions pairs with cohort none");
        repo.create_draft(&scope, tenant(), draft(price_id, key, flat_content()))
            .await
            .unwrap_or_else(|e| {
                panic!("{currency}/{region} on plan {plan_id} phase {phase:x} is its own key: {e}")
            });
    }
}

#[tokio::test]
async fn the_store_itself_refuses_the_second_draft_the_check_can_only_read_for() {
    let (repo, provider) = harness().await;
    let scope = AccessScope::for_tenant(tenant());
    let first = Uuid::from_u128(0xb_35);
    let key = base_key(ChargeKind::Recurring);

    repo.create_draft(&scope, tenant(), draft(first, key, flat_content()))
        .await
        .expect("the first row takes the key");

    // The repository's duplicate check is a read, so two concurrent creators
    // can both pass it; what decides the race is
    // `uq_pricing_price_scope_key_draft`. `uq_pricing_price_scope_key_current`
    // cannot do it — that index is partial over `published` and a draft is
    // invisible to it — so before this index existed both callers committed and
    // landed exactly the second draft the check exists to refuse.
    //
    // The insert goes through the entity rather than the repository, because
    // the repository would refuse it a statement earlier: what is under test is
    // the half that survives losing the race.
    let err = insert_bare_draft(&provider, &scope, Uuid::from_u128(0xb_36))
        .await
        .expect_err("a second draft on one canonical scope key must not land");

    // **This assertion changed shape with D-196 clause (2), and the change is a
    // measured property of `SQLite` rather than a weakening chosen here.** It
    // used to enumerate the nine indexed columns, because `SQLite` names the
    // colliding *columns* while Postgres names the index. Once the index carries
    // an **expression** — `COALESCE(meter, '')`, the sentinel that keeps a
    // nullable `meter` from dissolving the uniqueness of every non-usage key —
    // `SQLite` stops naming columns at all and names the index instead:
    //
    //     UNIQUE (a, b)                     -> "UNIQUE constraint failed: t.a, t.b"
    //     UNIQUE (a, b, COALESCE(meter,'')) -> "UNIQUE constraint failed: index 'ix'"
    //
    // measured directly on both forms, 2026-08-06. So the axis list is no longer
    // available to assert on this engine, and the index **name** is what both
    // engines now have in common. That is a real cost of the sentinel and it is
    // recorded on D-196 rather than absorbed silently: a reader who wants to know
    // which axes the guarantee covers reads the migration, not the error.
    //
    // The rest of the original note stands: this is also why the repository does
    // not turn the violation back into `DUPLICATE_SCOPE_KEY` itself — recognizing
    // it means knowing which backend is answering, a narrowing owed to the
    // surface layer (see `PriceRepo::create_draft`).
    let message = err.to_string();
    assert!(
        message.contains("UNIQUE constraint failed"),
        "the refusal must be a unique violation, got: {message}"
    );
    assert!(
        message.contains("uq_pricing_price_scope_key_draft"),
        "the violated guard must be the draft-plane scope-key index, got: {message}"
    );

    // And the winner is untouched.
    assert!(
        repo.find(&scope, tenant(), first)
            .await
            .expect("read")
            .is_some()
    );

    // The positive control, and it is what makes the index's `WHERE
    // lifecycle_state = 'draft'` load-bearing: once the occupant publishes, the
    // key takes a draft again. The two partial indexes are disjoint, which is
    // the state the D-88 supersession unit works in — a successor draft beside
    // the published predecessor it will replace. A non-partial UNIQUE here would
    // have made that state unreachable.
    flip_state(&provider, &scope, first, LifecycleState::Published).await;
    insert_bare_draft(&provider, &scope, Uuid::from_u128(0xb_37))
        .await
        .expect("a published occupant leaves the draft index free");
}

/// Insert a minimal draft row on the suite's base recurring key, straight
/// through the entity — the path a caller that had already passed the
/// repository's read would take.
async fn insert_bare_draft(
    provider: &DBProvider<DbError>,
    scope: &AccessScope,
    price_id: Uuid,
) -> Result<(), toolkit_db::secure::ScopeError> {
    let conn = provider.conn().expect("conn");
    let row = price::ActiveModel {
        price_id: Set(price_id),
        tenant_id: Set(tenant()),
        plan_id: Set(plan().get()),
        currency: Set("USD".to_owned()),
        region: Set("EU".to_owned()),
        phase: Set(Uuid::from_u128(0xfa_5e)),
        charge_kind: Set(ChargeKind::Recurring.as_str().to_owned()),
        amount_minor: Set(Some(1_000)),
        model_kind: Set(Some("flat".to_owned())),
        lifecycle_state: Set(LifecycleState::Draft.as_str().to_owned()),
        created_by: Set(Uuid::from_u128(0xac_10)),
        created_at_utc: Set(at(10)),
        ..price::ActiveModel::default()
    };
    price::Entity::insert(row.clone())
        .secure()
        .scope_with_model(scope, &row)?
        .exec(&conn)
        .await
        .map(|_| ())
}

#[tokio::test]
async fn an_edit_advances_the_tag_and_the_previous_tag_stops_working() {
    let (repo, _provider) = harness().await;
    let scope = AccessScope::for_tenant(tenant());
    let price_id = Uuid::from_u128(0xb_60);

    repo.create_draft(
        &scope,
        tenant(),
        draft(price_id, base_key(ChargeKind::Recurring), flat_content()),
    )
    .await
    .expect("create");

    let mut content = flat_content();
    content.row.amount_minor = Some(money(2_000));
    let edited = repo
        .update_draft(
            &scope,
            tenant(),
            price_id,
            RowVersion::new(0),
            content,
            stamp(),
            /* on_behalf_of */ None,
        )
        .await
        .expect("the first edit holds the current version");
    assert_eq!(edited.row.amount_minor, Some(money(2_000)));
    assert_eq!(edited.row_version, RowVersion::new(1));

    // The second writer is the bulk import that read before the interactive
    // edit landed. It is refused, and the refusal names both versions.
    let mut stale = flat_content();
    stale.row.amount_minor = Some(money(3_000));
    let err = repo
        .update_draft(
            &scope,
            tenant(),
            price_id,
            RowVersion::new(0),
            stale,
            stamp(),
            /* on_behalf_of */ None,
        )
        .await
        .expect_err("a submit against a superseded tag must be refused");
    assert_eq!(
        err,
        RepoError::StaleRowVersion {
            subject: "price row".to_owned(),
            id: price_id.to_string(),
            current: 1,
            submitted: 0,
        }
    );

    // And nothing of the refused edit landed.
    let read = repo
        .find(&scope, tenant(), price_id)
        .await
        .expect("read")
        .expect("present");
    assert_eq!(read.row.amount_minor, Some(money(2_000)));
    assert_eq!(read.row_version, RowVersion::new(1));
}

#[tokio::test]
async fn a_frozen_row_refuses_by_name_and_an_absent_one_is_not_found() {
    let (repo, provider) = harness().await;
    let scope = AccessScope::for_tenant(tenant());
    let price_id = Uuid::from_u128(0xb_70);

    // A **banded** row on purpose. The delete path has to remove the children
    // before the parent — the foreign key neither cascades nor nulls — so a row
    // with no bands
    // would let `delete_bands` touch nothing, the band trigger never fire, and
    // the row DELETE's own draft conjunct produce the same `NotDraft`. Only a
    // row that actually has bands can tell the typed refusal apart from the
    // trigger's raw one.
    repo.create_draft(
        &scope,
        tenant(),
        draft(
            price_id,
            grandfathered_key(ChargeKind::Usage, at(9)),
            graduated_content(),
        ),
    )
    .await
    .expect("create");
    flip_state(&provider, &scope, price_id, LifecycleState::Published).await;

    // The submitted version is the row's real one, so only the draft-only
    // conjunct can have failed. The typed refusal is the whole point: the table
    // trigger would answer with a database error carrying no state, and the
    // caller would be told the store is broken.
    let err = repo
        .update_draft(
            &scope,
            tenant(),
            price_id,
            RowVersion::new(0),
            graduated_content(),
            stamp(),
            /* on_behalf_of */ None,
        )
        .await
        .expect_err("a published row is frozen in content");
    assert_eq!(
        err,
        RepoError::NotDraft {
            subject: "price row".to_owned(),
            id: price_id.to_string(),
            state: "published".to_owned(),
        }
    );

    // Deleting it is refused the same way, and — this is the part the band set
    // makes observable — *before* the band table is touched. Without the read
    // that refuses first, `delete_bands` would run against a published parent
    // and the band trigger would answer `RepoError::Db`, so the caller would be
    // told the store is broken rather than that the row is published.
    let err = repo
        .delete_draft(
            &scope,
            tenant(),
            price_id,
            RowVersion::new(0),
            stamp(),
            None,
        )
        .await
        .expect_err("only a never-published draft is deletable");
    assert_eq!(
        err,
        RepoError::NotDraft {
            subject: "price row".to_owned(),
            id: price_id.to_string(),
            state: "published".to_owned(),
        }
    );
    assert_eq!(
        stored_bands(&provider, &scope, price_id).await.len(),
        3,
        "a refused delete removes no band"
    );

    let absent = Uuid::from_u128(0xb_71);
    let err = repo
        .update_draft(
            &scope,
            tenant(),
            absent,
            RowVersion::new(0),
            flat_content(),
            stamp(),
            /* on_behalf_of */ None,
        )
        .await
        .expect_err("that row was never authored");
    assert_eq!(
        err,
        RepoError::NotFound {
            subject: "price row".to_owned(),
            id: absent.to_string(),
        }
    );
}

#[tokio::test]
async fn an_update_replaces_the_whole_band_set() {
    let (repo, provider) = harness().await;
    let scope = AccessScope::for_tenant(tenant());
    let price_id = Uuid::from_u128(0xb_80);
    let key = grandfathered_key(ChargeKind::Usage, at(9));

    repo.create_draft(&scope, tenant(), draft(price_id, key, graduated_content()))
        .await
        .expect("create");
    assert_eq!(stored_bands(&provider, &scope, price_id).await.len(), 3);

    // A new set, not a merge: two bands where there were three, a different
    // lower bound in the middle, and a different top price. A merge would leave
    // the old middle band behind and the geometry rules would see an overlap
    // nobody authored.
    let mut content = graduated_content();
    content.row.bands = vec![
        TierBand::closed(0, 500, rate(30)),
        TierBand::open(500, rate(20)),
    ];
    let edited = repo
        .update_draft(
            &scope,
            tenant(),
            price_id,
            RowVersion::new(0),
            content,
            stamp(),
            /* on_behalf_of */ None,
        )
        .await
        .expect("replace the band set");

    assert_eq!(
        edited.row.bands,
        vec![
            TierBand::closed(0, 500, rate(30)),
            TierBand::open(500, rate(20)),
        ]
    );
    assert_eq!(edited.row_version, RowVersion::new(1));

    // The band edit moved the **parent's** tag. Bands carry none of their own,
    // so without that two authors editing different bands of one draft would
    // both satisfy `If-Match` and silently interleave.
    let stored = stored_bands(&provider, &scope, price_id).await;
    assert_eq!(stored.len(), 2, "exactly the new bands are there");
    assert!(
        stored.iter().all(|band| band.tenant_id == tenant()),
        "a band's tenant comes from its parent row"
    );
}

#[tokio::test]
async fn an_update_rewrites_every_content_column_and_can_clear_one() {
    let (repo, _provider) = harness().await;
    let scope = AccessScope::for_tenant(tenant());
    let price_id = Uuid::from_u128(0xb_81);
    let key = grandfathered_key(ChargeKind::Usage, at(9));

    repo.create_draft(&scope, tenant(), draft(price_id, key, graduated_content()))
        .await
        .expect("create");

    // Every content column this kind can carry moves at once, and four
    // previously-set optionals are **cleared**. Both halves matter. An update
    // that submitted content byte-identical to what is stored except for the one
    // field under test would pass with any other column silently dropped from
    // the UPDATE — the write-side twin of a dropped read mapping. And clearing
    // is the whole justification for `update_draft` taking whole content instead
    // of a patch: a per-field `Some`/`None` encoding cannot say "set this back
    // to NULL" at all.
    let mut content = graduated_content();
    content.row.model_kind = Some(ModelKind::Volume);
    content.row.bands = vec![TierBand::open(0, rate(7))];
    // **The line stays put, and that is D-196 clause (3) narrowing this case by
    // exactly two columns.** `meter` and `dimensionKey` are axes of the canonical
    // scope key now, not content, so an update may not move them — the case
    // below asserts the refusal. Everything else here still moves.
    content.row.meter = Some("api_calls".to_owned());
    content.row.dimension_key = "region:eu".to_owned();
    content.row.billing_granularity = Some(BillingGranularity::PerDay);
    content.row.tier_aggregation_window = Some(TierAggregationWindow::InvoicePeriod);
    content.row.aggregation_function = Some(AggregationFunction::Peak);
    content.row.aggregation_granularity = Some(AggregationGranularity::Day);
    content.row.max_hold_granules = Some(12);
    content.row.tier_qualification_window = None;
    content.row.included_allowance = None;
    // **Slice 10's six, moved rather than resubmitted.** They were re-sent at
    // exactly their created values until 2026-08-20 and re-asserted nowhere after
    // the update, so dropping any of them from `content_assignments`
    // (`price_repo.rs`) left this suite green — the identical silent-revert
    // regression that function's own doc records for `tax_category_ref` and
    // `unit_rate_nano`. No other test in the crate moves these six on an update
    // path, so a `PATCH` that reverted a `reserved_rate` (money per covered
    // granule, D-139) answered 200 undetected.
    content.row.reserved_rate = Some(rate(9));
    content.row.reservation_flavor = Some(ReservationFlavor::Consumption);
    content.row.min_qty_purchase = Some(21);
    content.row.min_qty_usage = Some(33);
    // `MinQtyUsageFallback` has one variant, so the only move available to it is
    // **out**: cleared here, which is what proves the column is assigned on the
    // update path rather than left at its created value.
    content.row.min_qty_usage_fallback = None;
    content.row.discount_ref = Some("promo/autumn".to_owned());
    content.tax_inclusive = false;
    // **The column this case was named for and did not carry.** It was absent from
    // `content_assignments` until 2026-08-11, so a `PATCH` that set or cleared it
    // answered 200 and reverted the field — while this test, whose own comment says
    // "every content column this kind can carry moves at once", stayed green
    // because `graduated_content()` leaves it `None` and nothing here ever moved it.
    // Set here (the row was created without one) and cleared below.
    content.tax_category_ref = Some("digital_services".to_owned());
    content.billing_timing = Some("advance".to_owned());
    // **Slice 6's contract, all three members at once.** It was `None` in the
    // fixture and moved nowhere in this file, so an assignment list that named none
    // of `billing_anchor_policy`, `anchor_day`, `proration_basis` or
    // `credit_on_downgrade` left this case green -- and the anchor decides where a
    // cycle boundary falls, which is what a customer is invoiced on. All three move
    // together here because a case moving one would pass against a list that
    // carried that one and dropped the rest; the anchor moves to another `FixedDay`
    // so the *day* moves without the token, which is the member a store that keyed
    // on the token alone would lose.
    content.proration_contract = Some(ProrationContract {
        billing_anchor_policy: BillingAnchorPolicy::FixedDay(
            AnchorDay::new(28).expect("a day of the month"),
        ),
        proration_basis: ProrationBasis::BySecond,
        credit_on_downgrade: false,
    });
    content.rounding_policy_ref = None;
    content.grandfather_until = Some(at(20));
    content.supersedes_price_id = None;

    repo.update_draft(
        &scope,
        tenant(),
        price_id,
        RowVersion::new(0),
        content,
        stamp(),
        /* on_behalf_of */ None,
    )
    .await
    .expect("replace the whole content");

    let read = repo
        .find(&scope, tenant(), price_id)
        .await
        .expect("read")
        .expect("present");

    assert_eq!(read.row.model_kind, Some(ModelKind::Volume));
    assert_eq!(read.row.bands, vec![TierBand::open(0, rate(7))]);
    // The line is unchanged because an update may not move it (D-196 clause 3);
    // what this case is about is every column that still moves.
    assert_eq!(read.row.meter.as_deref(), Some("api_calls"));
    // The empty string is the empty-tuple sentinel, not an absent value: the
    // column is NOT NULL DEFAULT '' so the Slice-2 injectivity index collides
    // undimensioned rows instead of treating them as distinct NULLs. It is now
    // also the tenth axis's sentinel, which is why `''` had to stay unauthorable
    // as a *meter* — see `Meter::new`.
    assert_eq!(read.row.dimension_key, "region:eu");
    assert_eq!(
        read.row.billing_granularity,
        Some(BillingGranularity::PerDay)
    );
    assert_eq!(
        read.row.tier_aggregation_window,
        Some(TierAggregationWindow::InvoicePeriod)
    );
    assert_eq!(
        read.row.aggregation_function,
        Some(AggregationFunction::Peak)
    );
    assert_eq!(
        read.row.aggregation_granularity,
        Some(AggregationGranularity::Day)
    );
    assert_eq!(read.row.max_hold_granules, Some(12));
    // Slice 10's six, each read back at its moved value.
    assert_eq!(
        read.row.reserved_rate,
        Some(rate(9)),
        "D-139's money per covered granule is content and moves on an edit"
    );
    assert_eq!(
        read.row.reservation_flavor,
        Some(ReservationFlavor::Consumption)
    );
    assert_eq!(read.row.min_qty_purchase, Some(21));
    assert_eq!(read.row.min_qty_usage, Some(33));
    assert_eq!(
        read.row.min_qty_usage_fallback, None,
        "`inst-ft-fallback`'s marker is cleared by the edit, not carried forward"
    );
    assert_eq!(read.row.discount_ref.as_deref(), Some("promo/autumn"));
    assert!(!read.tax_inclusive);
    assert_eq!(
        read.tax_category_ref.as_deref(),
        Some("digital_services"),
        "D-110: the row's category is the source of truth, and a draft edit is the \
         only place it can be corrected before D-154 freezes it for seven years"
    );
    assert_eq!(read.billing_timing.as_deref(), Some("advance"));
    assert_eq!(
        read.proration_contract,
        Some(ProrationContract {
            billing_anchor_policy: BillingAnchorPolicy::FixedDay(
                AnchorDay::new(28).expect("a day of the month")
            ),
            proration_basis: ProrationBasis::BySecond,
            credit_on_downgrade: false,
        }),
        "all three members of Slice 6's contract move on an edit, and the day moves with the \
         anchor rather than being re-derived from the token"
    );
    assert_eq!(read.grandfather_until, Some(at(20)));

    // The four that were set and are now NULL.
    assert_eq!(read.row.tier_qualification_window, None);
    assert_eq!(read.row.included_allowance, None);
    assert_eq!(read.rounding_policy_ref, None);
    assert_eq!(read.supersedes_price_id, None);

    // And nothing the update may not move has moved — including the usage line,
    // which is an axis of the key rather than an editable content field since
    // D-196 clause (3), so the update carries it forward rather than re-deriving
    // it.
    assert_eq!(
        read.scope_key,
        grandfathered_key(ChargeKind::Usage, at(9))
            .with_usage_line(
                Some(Meter::new("api_calls").expect("a non-blank meter")),
                DimensionKey::new("region:eu"),
            )
            .expect("a usage key carries its line")
    );
    assert_eq!(read.created_by, Uuid::from_u128(0xac_10));
    assert_eq!(read.created_at_utc, at(10));
    assert_eq!(read.row_version, RowVersion::new(1));

    // **The other direction.** A column that can be set and not cleared is still
    // half-broken, and clearing is the whole justification for `update_draft`
    // taking whole content rather than a patch. D-110 makes `None` mean *the row
    // states none* — which D-154 then resolves against the region taxonomy's
    // default — rather than "no category", so this is a meaningful edit and not
    // just the absence of one.
    let mut cleared = graduated_content();
    cleared.row.meter = Some("api_calls".to_owned());
    cleared.row.dimension_key = "region:eu".to_owned();
    cleared.tax_category_ref = None;
    // The contract's clearing pass, the shape the four optionals above already
    // have. `None` here means *this row states no proration contract*, which the
    // plan-level default then answers for, so it is a meaningful edit and the one a
    // column assigned only when `Some` cannot express.
    cleared.proration_contract = None;
    repo.update_draft(
        &scope,
        tenant(),
        price_id,
        RowVersion::new(1),
        cleared,
        stamp(),
        /* on_behalf_of */ None,
    )
    .await
    .expect("clear the category");

    let recleared = repo
        .find(&scope, tenant(), price_id)
        .await
        .expect("read")
        .expect("present");
    assert_eq!(
        recleared.tax_category_ref, None,
        "a category that can be set and not cleared is still an unexpressible correction"
    );
    assert_eq!(
        recleared.proration_contract, None,
        "and neither is a contract that can be set and not cleared"
    );
}

#[tokio::test]
async fn an_update_reaches_the_per_kind_money_columns_too() {
    let (repo, _provider) = harness().await;
    let scope = AccessScope::for_tenant(tenant());

    // The five content columns the tiered row above cannot legally carry:
    // `package_size` / `package_price_minor` live only on a `package` row
    // (`chk_pricing_price_package_fields_kind`), and `unit_rate_nano` /
    // `quantity_source` / `manual_quantity` belong to the untiered kinds. They
    // reach the UPDATE through the same assignment list, so they need the same
    // proof that the list actually names them.
    let package_id = Uuid::from_u128(0xb_82);
    let mut package = PriceRow::new(ChargeKind::Usage, Some(ModelKind::Package));
    package.package_size = Some(1_000);
    package.package_price_minor = Some(money(4_999));
    package.billing_granularity = Some(BillingGranularity::WholeUnit);
    package.tier_aggregation_window = Some(TierAggregationWindow::InvoicePeriod);
    let package_content = PriceContent {
        row: package,
        tax_inclusive: false,
        tax_category_ref: None,
        billing_timing: None,
        proration_contract: None,
        rounding_policy_ref: None,
        grandfather_until: None,
        supersedes_price_id: None,
    };
    repo.create_draft(
        &scope,
        tenant(),
        draft(
            package_id,
            base_key(ChargeKind::Usage),
            package_content.clone(),
        ),
    )
    .await
    .expect("create the package row");

    let mut resized = package_content;
    resized.row.package_size = Some(500);
    resized.row.package_price_minor = Some(money(2_999));
    repo.update_draft(
        &scope,
        tenant(),
        package_id,
        RowVersion::new(0),
        resized,
        stamp(),
        /* on_behalf_of */ None,
    )
    .await
    .expect("re-block the package row");

    let read = repo
        .find(&scope, tenant(), package_id)
        .await
        .expect("read")
        .expect("present");
    assert_eq!(read.row.package_size, Some(500));
    assert_eq!(read.row.package_price_minor, Some(money(2_999)));

    let per_unit_id = Uuid::from_u128(0xb_83);
    let mut per_unit = PriceRow::new(ChargeKind::Recurring, Some(ModelKind::PerUnit));
    // **A `per_unit` row's money is its rate, and `amount_minor` is NULL on it**
    // (D-311). This row carried `amount_minor` and no rate until 2026-08-14, which
    // is both the pre-D-311 shape and the reason the gap below survived: the case
    // named for "the per-kind money columns" never touched the one column that had
    // just become a `per_unit` row's price. `amount_minor`'s own assignment is
    // proved by the flat-row edit in
    // `an_edit_advances_the_tag_and_the_previous_tag_stops_working`, so correcting
    // the shape here costs no coverage.
    per_unit.unit_rate = Some(nano_rate(1_234_567_891));
    per_unit.quantity_source = Some(QuantitySource::Manual);
    per_unit.manual_quantity = Some(12);
    let per_unit_content = PriceContent {
        row: per_unit,
        tax_inclusive: false,
        tax_category_ref: None,
        billing_timing: Some("advance".to_owned()),
        proration_contract: None,
        rounding_policy_ref: None,
        grandfather_until: None,
        supersedes_price_id: None,
    };
    repo.create_draft(
        &scope,
        tenant(),
        draft(
            per_unit_id,
            base_key(ChargeKind::Recurring),
            per_unit_content.clone(),
        ),
    )
    .await
    .expect("create the per-unit row");

    // Moving to the seat count clears the fixed quantity in the same write —
    // the two fields answer one question, and a row that kept both would give
    // rating two answers to "how many". And the **rate moves with it**: re-rating
    // a metered row is the commonest draft edit there is.
    //
    // Neither value is a whole number of minor units and neither is the other
    // scaled by a power of ten, so a read or a write at the wrong scale lands on
    // a number this assertion refuses rather than on a plausible one.
    let mut seated = per_unit_content;
    seated.row.unit_rate = Some(nano_rate(987_654_321));
    seated.row.quantity_source = Some(QuantitySource::SubscriptionSeatCount);
    seated.row.manual_quantity = None;
    repo.update_draft(
        &scope,
        tenant(),
        per_unit_id,
        RowVersion::new(0),
        seated,
        stamp(),
        /* on_behalf_of */ None,
    )
    .await
    .expect("re-price the per-unit row");

    let read = repo
        .find(&scope, tenant(), per_unit_id)
        .await
        .expect("read")
        .expect("present");
    // **The stored value, read back through `find` — not the one submitted.** An
    // assertion on the content handed to `update_draft`, or on a response body
    // rendered from the request, passes against a column the UPDATE never names.
    assert_eq!(
        read.row.unit_rate,
        Some(nano_rate(987_654_321)),
        "D-311 gave the per_unit rate a column of its own, and a draft edit that \
         cannot move it loses a commercial change while answering success"
    );
    assert_eq!(
        read.row.amount_minor, None,
        "a per_unit row's money is its rate; two priced columns are two competing prices"
    );
    assert_eq!(
        read.row.quantity_source,
        Some(QuantitySource::SubscriptionSeatCount)
    );
    assert_eq!(read.row.manual_quantity, None);
}

#[tokio::test]
async fn deleting_a_draft_takes_its_bands_with_it() {
    let (repo, provider) = harness().await;
    let scope = AccessScope::for_tenant(tenant());
    let price_id = Uuid::from_u128(0xb_90);
    let key = grandfathered_key(ChargeKind::Usage, at(9));

    repo.create_draft(&scope, tenant(), draft(price_id, key, graduated_content()))
        .await
        .expect("create");

    // Abandoning a draft is a write like any other: a caller working from a
    // read it did not refresh would otherwise discard an edit it never saw.
    let err = repo
        .delete_draft(
            &scope,
            tenant(),
            price_id,
            RowVersion::new(4),
            stamp(),
            None,
        )
        .await
        .expect_err("a stale tag must not delete");
    assert_eq!(
        err,
        RepoError::StaleRowVersion {
            subject: "price row".to_owned(),
            id: price_id.to_string(),
            current: 0,
            submitted: 4,
        }
    );
    assert_eq!(
        stored_bands(&provider, &scope, price_id).await.len(),
        3,
        "a refused delete removes no band"
    );

    // The bands go first, inside the transaction: the foreign key declares the
    // default NO ACTION on both backends, so the row cannot leave while its
    // children point at it.
    repo.delete_draft(
        &scope,
        tenant(),
        price_id,
        RowVersion::new(0),
        stamp(),
        None,
    )
    .await
    .expect("the current tag deletes");
    assert_eq!(
        repo.find(&scope, tenant(), price_id).await.expect("read"),
        None
    );
    assert!(
        stored_bands(&provider, &scope, price_id).await.is_empty(),
        "the band set went with its parent"
    );
}

/// A draft a **window** stands on is refused by name, and nothing is deleted on
/// the way to the refusal.
///
/// `fk_pricing_price_window_price` references `pricing_price (price_id)` with no
/// cascade on both backends, and sqlx turns `foreign_keys` ON for `SQLite`, so past
/// the guard the DELETE meets the key and comes back as `RepoError::Db` — a 500
/// telling the operator the store is broken about a row they can see. This is
/// `refuse_if_locked_elsewhere`'s claim one table over, on that guard's own
/// standard: a rule that lives on one authoring path is not a rule.
///
/// The bands are the second half. The window check runs **before** the band delete,
/// so a refusal cannot land with the band set already gone and the row still there
/// — the half-applied state the whole transaction exists to make impossible.
///
/// The positive control is a **second draft with no window**, deleted in the same
/// world by the same call: without it a green here is satisfied by a delete that
/// refuses everything.
#[tokio::test]
async fn deleting_a_draft_a_window_stands_on_is_refused_naming_the_window() {
    let (repo, provider) = harness().await;
    let scope = AccessScope::for_tenant(tenant());
    let bound = Uuid::from_u128(0xb_93);
    let unbound = Uuid::from_u128(0xb_94);

    // Two keys, because two drafts on one canonical scope key is a refusal of its
    // own and would answer this case for the wrong reason.
    repo.create_draft(
        &scope,
        tenant(),
        draft(
            bound,
            grandfathered_key(ChargeKind::Usage, at(9)),
            graduated_content(),
        ),
    )
    .await
    .expect("create the draft a window will stand on");
    // The horizon expires a retained generation, so it is legal only on the
    // grandfathered class and the control's key is not on it.
    let mut control = graduated_content();
    control.grandfather_until = None;
    repo.create_draft(
        &scope,
        tenant(),
        draft(unbound, base_key(ChargeKind::Usage), control),
    )
    .await
    .expect("create the control draft");

    let conn = provider.conn().expect("conn");
    let window = common::schedule_coverage_window(&conn, &scope, tenant(), bound, stamp()).await;

    let err = repo
        .delete_draft(&scope, tenant(), bound, RowVersion::new(0), stamp(), None)
        .await
        .expect_err("a draft a window stands on does not delete");
    assert_eq!(
        err,
        RepoError::PriceWindowScheduled {
            price_id: bound.to_string(),
            window_id: window.window_id.to_string(),
        },
        "the refusal names the window, which is the object the operator can act on"
    );

    assert!(
        repo.find(&scope, tenant(), bound)
            .await
            .expect("read")
            .is_some(),
        "the refused delete removed no row"
    );
    assert_eq!(
        stored_bands(&provider, &scope, bound).await.len(),
        3,
        "and no band: the guard runs before the band delete"
    );
    assert!(
        window_rows(&provider, &scope, bound)
            .await
            .iter()
            .any(|row| row.window_id == window.window_id),
        "and the window is still there: the refusal cancels nothing on the operator's behalf"
    );

    // The positive control, in this same world and through this same call.
    repo.delete_draft(&scope, tenant(), unbound, RowVersion::new(0), stamp(), None)
        .await
        .expect("positive control: a draft no window stands on deletes");
    assert_eq!(
        repo.find(&scope, tenant(), unbound).await.expect("read"),
        None
    );
}

/// The window rows physically bound to `price_id`, whatever state they are in.
///
/// [`stored_bands`]'s shape and its reason: the claim is about what the table
/// holds, and a `cancelled` window still holds the foreign key.
async fn window_rows(
    provider: &DBProvider<DbError>,
    scope: &AccessScope,
    price_id: Uuid,
) -> Vec<price_window::Model> {
    let conn = provider.conn().expect("conn");
    price_window::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(Condition::all().add(price_window::Column::PriceId.eq(price_id)))
        .all(&conn)
        .await
        .expect("read the window plane")
}

#[tokio::test]
async fn list_for_plan_filters_by_state_and_orders_stably() {
    let (repo, provider) = harness().await;
    let scope = AccessScope::for_tenant(tenant());
    let recurring = Uuid::from_u128(0xb_a1);
    let one_time = Uuid::from_u128(0xb_a2);
    let setup = Uuid::from_u128(0xb_a3);

    for (price_id, charge_kind) in [
        (setup, ChargeKind::OneTimeSetup),
        (one_time, ChargeKind::OneTime),
        (recurring, ChargeKind::Recurring),
    ] {
        let mut content = flat_content();
        content.row = PriceRow::new(charge_kind, Some(ModelKind::Flat));
        content.row.amount_minor = Some(money(1_000));
        repo.create_draft(
            &scope,
            tenant(),
            draft(price_id, base_key(charge_kind), content),
        )
        .await
        .expect("create");
    }
    flip_state(&provider, &scope, one_time, LifecycleState::Published).await;

    // Ascending by `price_id`, whatever order the rows were written in. The
    // list surface's cursor (D-125, G7) needs a total order that does not
    // depend on the plan index's physical layout.
    let drafts = repo
        .list_for_plan(&scope, tenant(), plan(), &[LifecycleState::Draft])
        .await
        .expect("list drafts");
    assert_eq!(
        drafts.iter().map(|row| row.price_id).collect::<Vec<_>>(),
        vec![recurring, setup]
    );

    let published = repo
        .list_for_plan(&scope, tenant(), plan(), &[LifecycleState::Published])
        .await
        .expect("list published");
    assert_eq!(
        published.iter().map(|row| row.price_id).collect::<Vec<_>>(),
        vec![one_time]
    );

    // An empty state set selects nothing. Reading it as "every state" would
    // hand a caller whose filter computed to nothing the whole catalog.
    assert!(
        repo.list_for_plan(&scope, tenant(), plan(), &[])
            .await
            .expect("list nothing")
            .is_empty()
    );
}

#[tokio::test]
async fn another_tenants_price_row_is_invisible_and_unwritable() {
    let (repo, _provider) = harness().await;
    let mine = Uuid::from_u128(0x7e_22);
    let price_id = Uuid::from_u128(0xb_b0);
    let key = grandfathered_key(ChargeKind::Usage, at(9));

    repo.create_draft(
        &AccessScope::for_tenant(tenant()),
        tenant(),
        draft(price_id, key, graduated_content()),
    )
    .await
    .expect("the other tenant authors its row");

    // SQL-level BOLA, the same shape `PinFrontierRepo::read` documents: my
    // scope resolves their row to nothing, whichever tenant id I name. The
    // catalog is commercially sensitive, so the reads fail to `None` and the
    // writes fail to "not found" — never to "forbidden", which would confirm
    // the row exists.
    let scope = AccessScope::for_tenant(mine);
    assert_eq!(
        repo.find(&scope, tenant(), price_id).await.expect("read"),
        None
    );
    assert!(
        repo.list_for_plan(&scope, tenant(), plan(), &[LifecycleState::Draft])
            .await
            .expect("list")
            .is_empty()
    );

    let err = repo
        .update_draft(
            &scope,
            tenant(),
            price_id,
            RowVersion::new(0),
            flat_content(),
            stamp(),
            /* on_behalf_of */ None,
        )
        .await
        .expect_err("a foreign draft is not writable");
    assert!(matches!(err, RepoError::NotFound { .. }));

    let err = repo
        .delete_draft(
            &scope,
            tenant(),
            price_id,
            RowVersion::new(0),
            stamp(),
            None,
        )
        .await
        .expect_err("a foreign draft is not deletable");
    assert!(matches!(err, RepoError::NotFound { .. }));

    // And the row is untouched for its owner, band set included.
    let owner = AccessScope::for_tenant(tenant());
    let read = repo
        .find(&owner, tenant(), price_id)
        .await
        .expect("read")
        .expect("their row is still there");
    assert_eq!(read.row_version, RowVersion::new(0));
    assert_eq!(read.row.bands.len(), 3);
}

#[tokio::test]
async fn a_price_row_may_not_be_created_into_another_tenant() {
    let (repo, _provider) = harness().await;
    let mine = Uuid::from_u128(0x7e_22);
    let price_id = Uuid::from_u128(0xb_b1);

    // The insert side of the BOLA the test above closes for reads, updates and
    // deletes, and the only side no `WHERE` clause can close: a scoped read
    // filters rows that exist, while a scoped insert has to refuse a row before
    // it does. `scope_with_model` is what refuses it — the `ActiveModel`'s own
    // `tenant_id` is checked against the caller's scope — and without that
    // check a caller could plant a price row inside another tenant's catalog,
    // priced however it liked, and then be unable to see or unwrite it.
    let err = repo
        .create_draft(
            &AccessScope::for_tenant(mine),
            tenant(),
            draft(price_id, base_key(ChargeKind::Recurring), flat_content()),
        )
        .await
        .expect_err("a row may not be created into a tenant the caller is not scoped to");
    let RepoError::Db(detail) = err else {
        panic!("a refused insert scope is a storage failure, not a typed refusal");
    };
    assert!(detail.contains("pricing_price scope"), "got: {detail}");

    // And nothing landed under either tenant — least of all the victim's.
    assert_eq!(
        repo.find(&AccessScope::for_tenant(tenant()), tenant(), price_id)
            .await
            .expect("read"),
        None
    );
    assert_eq!(
        repo.find(&AccessScope::for_tenant(mine), mine, price_id)
            .await
            .expect("read"),
        None
    );
}

#[tokio::test]
async fn a_create_the_band_table_refuses_leaves_no_row_behind() {
    let (repo, provider) = harness().await;
    let scope = AccessScope::for_tenant(tenant());
    let price_id = Uuid::from_u128(0xb_c8);

    // A `flat` row carrying a band set. It fails in the one place that makes
    // the transaction observable: the row INSERT succeeds — `flat` is a legal
    // kind and the row satisfies every CHECK on its own table — and the *next*
    // statement is refused, by the band table's structural-exclusivity trigger
    // reading the parent this call has just written.
    let mut content = flat_content();
    content.row.bands = vec![TierBand::closed(0, 100, rate(50))];
    let err = repo
        .create_draft(
            &scope,
            tenant(),
            draft(price_id, base_key(ChargeKind::Recurring), content),
        )
        .await
        .expect_err("bands are forbidden on a flat row");
    let RepoError::Db(detail) = err else {
        panic!("the band table's refusal reaches the caller as a storage failure");
    };
    assert!(
        detail.contains("pricing_price_tier_band"),
        "the refusal must be the band table's, got: {detail}"
    );

    // The claim this file's repository makes is "both tables or neither", and
    // this is the only case that can tell the difference: with the two writes
    // run as plain statements the row above is durable, so the caller is handed
    // a failure while the store holds a `flat` row that a later authoring call
    // would find occupying its scope key, that `list_for_plan` would return,
    // and that publish would judge — a row nobody successfully created.
    assert_eq!(
        repo.find(&scope, tenant(), price_id).await.expect("read"),
        None,
        "a refused create must leave nothing behind"
    );
    assert!(
        stored_bands(&provider, &scope, price_id).await.is_empty(),
        "and no band either"
    );

    // The key is free, which is the operational half of the same fact: an
    // author whose first attempt was refused may fix the row and try again.
    repo.create_draft(
        &scope,
        tenant(),
        draft(price_id, base_key(ChargeKind::Recurring), flat_content()),
    )
    .await
    .expect("the refused attempt left the scope key unoccupied");
}

#[tokio::test]
async fn a_granule_bound_no_column_can_hold_is_refused_before_anything_is_written() {
    let (repo, _provider) = harness().await;
    let scope = AccessScope::for_tenant(tenant());
    let price_id = Uuid::from_u128(0xb_c9);

    // Every count on the row is a `u64` in the domain over a **signed**
    // `bigint`, so the top half of the domain range has no storage at all and
    // the refusal is reachable through any of them; `max_hold_granules` is the
    // one this case drives. Checked rather than cast: a cast would turn an
    // impossible bound into a plausible one — `i64::MAX + 1` renders as
    // `i64::MIN` — and hold a sampling gap nobody authored, on a column whose
    // own CHECK demands `>= 1`.
    let mut content = graduated_content();
    let unstorable = u64::try_from(i64::MAX).expect("i64::MAX is a u64") + 1;
    content.row.max_hold_granules = Some(unstorable);

    let err = repo
        .create_draft(
            &scope,
            tenant(),
            draft(
                price_id,
                grandfathered_key(ChargeKind::Usage, at(9)),
                content,
            ),
        )
        .await
        .expect_err("a bound past the signed bigint column is not storable");

    // Named to the field, and **not** `CorruptRow`: the number arrived on a
    // request and the author can change it, which is the line between a bad
    // request and an internal fault.
    assert_eq!(
        err,
        RepoError::ValueOutOfRange {
            field: "max_hold_granules".to_owned(),
            value: unstorable.to_string(),
        }
    );

    // Rendered before the transaction opens, so the refusal costs no write at
    // all — the key is still free.
    assert_eq!(
        repo.find(&scope, tenant(), price_id).await.expect("read"),
        None
    );
    repo.create_draft(
        &scope,
        tenant(),
        draft(
            price_id,
            grandfathered_key(ChargeKind::Usage, at(9)),
            graduated_content(),
        ),
    )
    .await
    .expect("a storable bound is authored on the same key");
}

/// `publish_rows` through a real transaction and over an explicit validated
/// set, which is now what its signature demands.
///
/// The set is `(price_id, row_version)` for every row the publish claims to
/// have judged: the repository publishes exactly those rows at exactly those
/// versions and re-derives nothing, so a row whose content moved between
/// validation and the flip is refused naming the row.
/// [`publish_rows`] with a tenant default, for the one case whose subject is the
/// default itself.
async fn publish_rows_with_default(
    provider: &DBProvider<DbError>,
    scope: &AccessScope,
    tenant_id: Uuid,
    plan_id: PlanId,
    validated: Vec<(Uuid, RowVersion)>,
    readiness: &RegionTaxReadiness,
    default_rounding_policy: &str,
) -> Result<Vec<Uuid>, RepoError> {
    let scope = scope.clone();
    let readiness = readiness.clone();
    let default_rounding_policy = default_rounding_policy.to_owned();
    let (_, outcome) = provider
        .db()
        .in_transaction::<Vec<Uuid>, RepoError, _>(move |txn| {
            Box::pin(async move {
                bss_pricing::infra::storage::repo::price_repo::publish_rows(
                    txn,
                    &scope,
                    tenant_id,
                    plan_id,
                    &validated,
                    &readiness,
                    Some(default_rounding_policy.as_str()),
                )
                .await
            })
        })
        .await;
    outcome.map_err(|err| {
        err.into_domain(|infra| RepoError::Db(format!("publish transaction: {infra}")))
    })
}

async fn publish_rows(
    provider: &DBProvider<DbError>,
    scope: &AccessScope,
    tenant_id: Uuid,
    plan_id: PlanId,
    validated: Vec<(Uuid, RowVersion)>,
    readiness: &RegionTaxReadiness,
) -> Result<Vec<Uuid>, RepoError> {
    let scope = scope.clone();
    let readiness = readiness.clone();
    let (_, outcome) = provider
        .db()
        .in_transaction::<Vec<Uuid>, RepoError, _>(move |txn| {
            Box::pin(async move {
                bss_pricing::infra::storage::repo::price_repo::publish_rows(
                    txn,
                    &scope,
                    tenant_id,
                    plan_id,
                    &validated,
                    &readiness,
                    // **A tenant default, because a published row must resolve a
                    // rounding policy at all** — `publish_rows` refuses a set that
                    // resolves none (review F1, 2026-08-19), so a helper passing
                    // `None` would refuse every case here on a ground none of them
                    // is about. This comment used to say the cases author a
                    // row-level policy; they do not, and the helper passed `None`,
                    // which is how five cases whose subject is the tax category
                    // came to publish rows with no rounding resolution at all.
                    // The cases whose subject *is* the resolution use
                    // `publish_rows_with_default` and
                    // `publish_rows_resolving_nothing`.
                    Some("half_up/2"),
                )
                .await
            })
        })
        .await;
    outcome.map_err(|err| {
        err.into_domain(|infra| RepoError::Db(format!("publish transaction: {infra}")))
    })
}

/// [`publish_rows`] with **no** tenant default, for the cases whose subject is a
/// set that resolves no rounding policy at all.
async fn publish_rows_resolving_nothing(
    provider: &DBProvider<DbError>,
    scope: &AccessScope,
    tenant_id: Uuid,
    plan_id: PlanId,
    validated: Vec<(Uuid, RowVersion)>,
    readiness: &RegionTaxReadiness,
) -> Result<Vec<Uuid>, RepoError> {
    let scope = scope.clone();
    let readiness = readiness.clone();
    let (_, outcome) = provider
        .db()
        .in_transaction::<Vec<Uuid>, RepoError, _>(move |txn| {
            Box::pin(async move {
                bss_pricing::infra::storage::repo::price_repo::publish_rows(
                    txn, &scope, tenant_id, plan_id, &validated, &readiness, None,
                )
                .await
            })
        })
        .await;
    outcome.map_err(|err| {
        err.into_domain(|infra| RepoError::Db(format!("publish transaction: {infra}")))
    })
}

/// Every `draft` row of the plan, paired with the version it stands at — the
/// shape a publish subject hands the repository after the rule set has passed.
async fn validated_drafts(
    repo: &PriceRepo,
    scope: &AccessScope,
    tenant_id: Uuid,
    plan_id: PlanId,
) -> Vec<(Uuid, RowVersion)> {
    repo.list_for_plan(scope, tenant_id, plan_id, &[LifecycleState::Draft])
        .await
        .expect("read the plan's draft rows")
        .into_iter()
        .map(|record| (record.price_id, record.row_version))
        .collect()
}

// ---------------------------------------------------------------------------
// The publish unit's price-row flip.
// ---------------------------------------------------------------------------

/// The stored row, whatever the repository would say about it.
async fn stored_row(
    provider: &DBProvider<DbError>,
    scope: &AccessScope,
    price_id: Uuid,
) -> price::Model {
    let conn = provider.conn().expect("conn");
    price::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(Condition::all().add(price::Column::PriceId.eq(price_id)))
        .one(&conn)
        .await
        .expect("read the stored row")
        .expect("the row is there")
}

/// A readiness declaring one region's default category.
fn readiness_for(region: &str, category: Option<&str>) -> RegionTaxReadiness {
    RegionTaxReadiness::new(
        [(
            region.to_owned(),
            RegionReadiness {
                tax_category: category.map(ToOwned::to_owned),
                tax_rate_present: true,
            },
        )]
        .into_iter()
        .collect(),
    )
}

/// **D-154: publish resolves the effective category and freezes the result.**
///
/// The row states no category of its own, so the value comes from the region
/// default — and it is written **by the publish statement**, against the
/// readiness the rule set judged the row with.
///
/// This is `T-13`. It was first built resolving in the projector instead, which
/// runs up to D-47's five-minute batching maximum later, against a region
/// taxonomy anyone holding `config × write` may have re-declared in between: a
/// version could freeze a category no rule ever judged, or lose one that was
/// present when publish passed. Freezing here is what makes the value a property
/// of the publish rather than of whenever the sweep happened to run.
#[tokio::test]
async fn publish_freezes_the_effective_tax_category_from_the_readiness_it_judged_with() {
    let (repo, provider) = harness().await;
    let scope = AccessScope::for_tenant(tenant());
    let price_id = Uuid::from_u128(0xb_00c1);
    repo.create_draft(
        &scope,
        tenant(),
        draft(price_id, base_key(ChargeKind::Recurring), flat_content()),
    )
    .await
    .expect("author the row");

    publish_rows(
        &provider,
        &scope,
        tenant(),
        plan(),
        vec![(price_id, RowVersion::new(0))],
        &readiness_for(
            base_key(ChargeKind::Recurring).region().as_str(),
            Some("standard"),
        ),
    )
    .await
    .expect("publish");

    let conn = provider.conn().expect("conn");
    let frozen = bss_pricing::infra::storage::repo::price_repo::frozen_resolutions(
        &conn,
        &scope,
        tenant(),
        plan(),
    )
    .await
    .expect("read the frozen categories");

    assert_eq!(
        frozen
            .get(&price_id)
            .and_then(|r| r.tax_category.as_deref()),
        Some("standard"),
        "the region default resolved at publish is what the row carries"
    );
}

/// The row's **own** category wins, and is what is frozen.
#[tokio::test]
async fn publish_freezes_the_rows_own_category_over_the_region_default() {
    let (repo, provider) = harness().await;
    let scope = AccessScope::for_tenant(tenant());
    let price_id = Uuid::from_u128(0xb_00c2);
    let mut content = flat_content();
    content.tax_category_ref = Some("reduced".to_owned());
    repo.create_draft(
        &scope,
        tenant(),
        draft(price_id, base_key(ChargeKind::Recurring), content),
    )
    .await
    .expect("author the row");

    publish_rows(
        &provider,
        &scope,
        tenant(),
        plan(),
        vec![(price_id, RowVersion::new(0))],
        &readiness_for(
            base_key(ChargeKind::Recurring).region().as_str(),
            Some("standard"),
        ),
    )
    .await
    .expect("publish");

    let conn = provider.conn().expect("conn");
    let frozen = bss_pricing::infra::storage::repo::price_repo::frozen_resolutions(
        &conn,
        &scope,
        tenant(),
        plan(),
    )
    .await
    .expect("read");

    assert_eq!(
        frozen
            .get(&price_id)
            .and_then(|r| r.tax_category.as_deref()),
        Some("reduced"),
        "D-110 makes the row the source of truth; the default is only a fallback"
    );
}

/// **The map's key set is "has published", and a never-published draft is not in
/// it** (Z7-6).
///
/// `frozen_resolutions`' own doc reads its two absences apart: *"`None` in
/// the map is a row that has one and it is null; a row absent from the map has
/// not published."* The read carried no lifecycle filter, so every draft row of
/// the plan was in the map with `None` — the one value the doc says means
/// "published, with no category". The live caller looks the map up only at ids it
/// drew from `load_for_plan(… PROJECTED_ROW_STATES)`, so nothing mispriced; what
/// the filter buys is that the key set means what the sentence says, and that a
/// draft's NULL cannot be carried into a projection.
///
/// **Armed at both edges, deliberately.** The `superseded` row is here because
/// `PROJECTED_ROW_STATES` is `published` **and** `superseded` — a filter written
/// `eq(Published)` would be armed narrower than the claim and would drop a row
/// that has published, and this case fails on it. `chk_pricing_price_lifecycle_state`
/// admits exactly those three tokens as the chain now stands (`pricing_price`,
/// unwidened since), so the three rows are the whole vocabulary.
#[tokio::test]
async fn the_frozen_category_map_holds_every_published_row_and_no_draft() {
    let (repo, provider) = harness().await;
    let scope = AccessScope::for_tenant(tenant());
    let published = Uuid::from_u128(0xb_00d1);
    let superseded = Uuid::from_u128(0xb_00d2);
    let never_published = Uuid::from_u128(0xb_00d3);
    let readiness = readiness_for(base_key(ChargeKind::Recurring).region().as_str(), None);

    for (price_id, key) in [
        (published, base_key(ChargeKind::Recurring)),
        (superseded, new_subscriptions_key(ChargeKind::Recurring)),
        (
            never_published,
            grandfathered_key(ChargeKind::Recurring, at(9)),
        ),
    ] {
        let mut content = flat_content();
        content.tax_category_ref = Some("standard".to_owned());
        repo.create_draft(&scope, tenant(), draft(price_id, key, content))
            .await
            .expect("author the row");
    }
    // Two of the three publish; one of those two then supersedes, which is a row
    // that HAS published and whose frozen category a consumer still resolves
    // against (D-121's reason for keeping `superseded` in the projected set).
    publish_rows(
        &provider,
        &scope,
        tenant(),
        plan(),
        vec![
            (published, RowVersion::new(0)),
            (superseded, RowVersion::new(0)),
        ],
        &readiness,
    )
    .await
    .expect("publish the two");
    flip_state(&provider, &scope, superseded, LifecycleState::Superseded).await;

    let conn = provider.conn().expect("conn");
    let frozen = bss_pricing::infra::storage::repo::price_repo::frozen_resolutions(
        &conn,
        &scope,
        tenant(),
        plan(),
    )
    .await
    .expect("read the frozen categories");

    assert!(
        !frozen.contains_key(&never_published),
        "a never-published draft is absent from the map, because absence is what the doc reads \
         as 'has not published': {frozen:?}"
    );
    // The positive control, and it is what stops the filter being written
    // narrower than the claim: both rows that published are still in the map,
    // carrying the category the publish froze.
    assert_eq!(
        frozen
            .get(&published)
            .and_then(|r| r.tax_category.as_deref()),
        Some("standard"),
        "the published row is in the map with what publish froze"
    );
    assert_eq!(
        frozen
            .get(&superseded)
            .and_then(|r| r.tax_category.as_deref()),
        Some("standard"),
        "and so is the superseded one: it has published, and rating resolves past instants \
         against it"
    );
    assert_eq!(
        frozen.len(),
        2,
        "the map is exactly the rows that have published: {frozen:?}"
    );
}

#[tokio::test]
async fn draft_rows_flip_and_published_rows_are_left_alone() {
    let (repo, provider) = harness().await;
    let scope = AccessScope::for_tenant(tenant());
    let already = Uuid::from_u128(0xb_0001);
    let pending = Uuid::from_u128(0xb_0002);
    repo.create_draft(
        &scope,
        tenant(),
        draft(already, base_key(ChargeKind::Recurring), flat_content()),
    )
    .await
    .expect("author the first row");
    flip_state(&provider, &scope, already, LifecycleState::Published).await;
    repo.create_draft(
        &scope,
        tenant(),
        draft(
            pending,
            new_subscriptions_key(ChargeKind::Recurring),
            flat_content(),
        ),
    )
    .await
    .expect("author the second row");

    let validated = validated_drafts(&repo, &scope, tenant(), plan()).await;
    let moved = publish_rows(
        &provider,
        &scope,
        tenant(),
        plan(),
        validated,
        &fixture_readiness(),
    )
    .await
    .expect("publish the plan's draft rows");

    assert_eq!(moved, vec![pending], "only the draft row moved");
    assert_eq!(
        stored_row(&provider, &scope, pending).await.lifecycle_state,
        LifecycleState::Published.as_str()
    );
    let untouched = stored_row(&provider, &scope, already).await;
    assert_eq!(
        untouched.lifecycle_state,
        LifecycleState::Published.as_str()
    );
    assert_eq!(
        untouched.row_version, 0,
        "an already-published row is not re-published and its tag does not move"
    );
}

#[tokio::test]
async fn the_published_rows_entity_tag_freezes_with_the_content_it_names() {
    // D-141 / §3.7: the tag joins the frozen whitelist, and this flip changes no
    // content. A bump here would move an entity tag under a representation no
    // caller can write to.
    let (repo, provider) = harness().await;
    let scope = AccessScope::for_tenant(tenant());
    let price_id = Uuid::from_u128(0xb_0001);
    let authored = repo
        .create_draft(
            &scope,
            tenant(),
            draft(price_id, base_key(ChargeKind::Recurring), flat_content()),
        )
        .await
        .expect("author the row");

    let validated = validated_drafts(&repo, &scope, tenant(), plan()).await;
    publish_rows(
        &provider,
        &scope,
        tenant(),
        plan(),
        validated,
        &fixture_readiness(),
    )
    .await
    .expect("publish");

    let published = repo
        .find(&scope, tenant(), price_id)
        .await
        .expect("read it back")
        .expect("it is there");
    assert_eq!(published.lifecycle_state, LifecycleState::Published);
    assert_eq!(published.row_version, authored.row_version);
}

#[tokio::test]
async fn the_key_moves_from_the_draft_plane_index_to_the_published_plane() {
    let (repo, provider) = harness().await;
    let scope = AccessScope::for_tenant(tenant());
    let first = Uuid::from_u128(0xb_0001);
    repo.create_draft(
        &scope,
        tenant(),
        draft(first, base_key(ChargeKind::Recurring), flat_content()),
    )
    .await
    .expect("author the row");

    let validated = validated_drafts(&repo, &scope, tenant(), plan()).await;
    publish_rows(
        &provider,
        &scope,
        tenant(),
        plan(),
        validated,
        &fixture_readiness(),
    )
    .await
    .expect("publish");

    // The draft-plane index (D-148) released the key as the row left `draft`,
    // and the published-plane index claimed it as the row arrived — so a second
    // draft on the same key is now refused by `create_draft`'s occupancy check
    // rather than by either index.
    let refusal = repo
        .create_draft(
            &scope,
            tenant(),
            draft(
                Uuid::from_u128(0xb_0002),
                base_key(ChargeKind::Recurring),
                flat_content(),
            ),
        )
        .await
        .expect_err("the key is occupied by a published row");
    assert!(
        matches!(refusal, RepoError::DuplicateScopeKey(_)),
        "got {refusal:?}"
    );
}

// ---------------------------------------------------------------------------
// The supersession door (D-195), and the ordering its commit owes.
// ---------------------------------------------------------------------------

/// `insert_successor_draft_on` through a real transaction, the way the D-88
/// composer will hold it.
async fn supersede(
    provider: &DBProvider<DbError>,
    scope: &AccessScope,
    tenant_id: Uuid,
    draft: NewPriceDraft,
) -> Result<(PriceRecord, Uuid), RepoError> {
    let scope = scope.clone();
    let (_, outcome) = provider
        .db()
        .in_transaction::<(PriceRecord, Uuid), RepoError, _>(move |txn| {
            Box::pin(async move {
                Box::pin(
                    bss_pricing::infra::storage::repo::price_repo::insert_successor_draft_on(
                        txn, &scope, tenant_id, draft,
                    ),
                )
                .await
            })
        })
        .await;
    outcome.map_err(|err| {
        err.into_domain(|infra| RepoError::Db(format!("supersession transaction: {infra}")))
    })
}

/// Put a published row on the base key and answer its id — the predecessor every
/// case below supersedes.
async fn published_predecessor(
    repo: &PriceRepo,
    provider: &DBProvider<DbError>,
    scope: &AccessScope,
) -> Uuid {
    let predecessor = Uuid::from_u128(0xb_5001);
    repo.create_draft(
        &scope.clone(),
        tenant(),
        draft(predecessor, base_key(ChargeKind::Recurring), flat_content()),
    )
    .await
    .expect("author the predecessor");
    flip_state(provider, scope, predecessor, LifecycleState::Published).await;
    predecessor
}

#[tokio::test]
async fn the_supersession_door_puts_a_successor_draft_on_the_key_its_predecessor_holds() {
    // The shape §3.7's two disjoint partial `UNIQUE`s permit and the authoring
    // door refuses: a draft beside the published row it will supersede, on one
    // canonical scope key. D-195 clause (2) — the occupancy precondition is the
    // authoring door's, inverted.
    let (repo, provider) = harness().await;
    let scope = AccessScope::for_tenant(tenant());
    let predecessor = published_predecessor(&repo, &provider, &scope).await;
    let successor = Uuid::from_u128(0xb_5002);

    let (record, superseded) = supersede(
        &provider,
        &scope,
        tenant(),
        draft(successor, base_key(ChargeKind::Recurring), flat_content()),
    )
    .await
    .expect("a published occupant is this door's precondition, not its refusal");

    assert_eq!(superseded, predecessor, "the door names what it superseded");
    assert_eq!(record.price_id, successor);
    assert_eq!(record.lifecycle_state, LifecycleState::Draft);
    // Both rows stand on the key, which is the whole point.
    assert_eq!(
        stored_row(&provider, &scope, predecessor)
            .await
            .lifecycle_state,
        LifecycleState::Published.as_str(),
        "the predecessor is untouched by the compose - the flip is the commit's"
    );
    assert_eq!(
        stored_row(&provider, &scope, successor)
            .await
            .lifecycle_state,
        LifecycleState::Draft.as_str()
    );
}

#[tokio::test]
async fn the_supersession_door_stamps_the_predecessor_it_found() {
    // D-127: the successor carries `supersedes_price_id`. The door stamps it from
    // the same read that validated the key, so a link disagreeing with the key's
    // actual current row is not expressible — whatever the caller sent.
    let (repo, provider) = harness().await;
    let scope = AccessScope::for_tenant(tenant());
    let predecessor = published_predecessor(&repo, &provider, &scope).await;
    let successor = Uuid::from_u128(0xb_5002);

    let mut content = flat_content();
    content.supersedes_price_id = Some(Uuid::from_u128(0xdead_beef));
    let (record, _) = supersede(
        &provider,
        &scope,
        tenant(),
        draft(successor, base_key(ChargeKind::Recurring), content),
    )
    .await
    .expect("compose");

    assert_eq!(
        record.supersedes_price_id,
        Some(predecessor),
        "the door's own read decides the link, not the payload"
    );
    assert_eq!(
        stored_row(&provider, &scope, successor)
            .await
            .supersedes_price_id,
        Some(predecessor),
        "and it is what the table holds"
    );
}

#[tokio::test]
async fn the_supersession_door_refuses_a_key_no_published_row_holds() {
    // `inst-su-compose` presupposes current coverage and fails compose on a
    // dormant key. This is that presupposition read off the row plane: a key with
    // no current row has nothing to supersede, and the caller named a target that
    // is not there.
    let (_repo, provider) = harness().await;
    let scope = AccessScope::for_tenant(tenant());

    let err = supersede(
        &provider,
        &scope,
        tenant(),
        draft(
            Uuid::from_u128(0xb_5002),
            base_key(ChargeKind::Recurring),
            flat_content(),
        ),
    )
    .await
    .expect_err("an empty key is not supersedable");

    let RepoError::NotFound { subject, id } = err else {
        panic!("a key with no current row must answer NotFound, got: {err:?}");
    };
    assert!(subject.contains("current price"), "got: {subject}");
    assert!(
        id.starts_with(&base_key(ChargeKind::Recurring).to_string()),
        "the refusal names the key that has no occupant, got: {id}"
    );
}

#[tokio::test]
async fn the_supersession_door_refuses_a_key_a_draft_already_stands_on() {
    // One draft per key stays the most the two doors admit between them (§3.7's
    // D-148 argument, under D-195's two-door reading). A draft here is a
    // composition that already staged one; `inst-co-single-pending` refuses the
    // second *unit* a layer up, and this is the floor under it.
    let (repo, provider) = harness().await;
    let scope = AccessScope::for_tenant(tenant());
    published_predecessor(&repo, &provider, &scope).await;
    let first = Uuid::from_u128(0xb_5002);
    let second = Uuid::from_u128(0xb_5003);

    supersede(
        &provider,
        &scope,
        tenant(),
        draft(first, base_key(ChargeKind::Recurring), flat_content()),
    )
    .await
    .expect("the first composition stages its successor");

    let err = supersede(
        &provider,
        &scope,
        tenant(),
        draft(second, base_key(ChargeKind::Recurring), flat_content()),
    )
    .await
    .expect_err("a second successor on one key must be refused");

    let RepoError::DuplicateScopeKey(detail) = err else {
        panic!("a draft occupant must answer DUPLICATE_SCOPE_KEY, got: {err:?}");
    };
    assert!(detail.contains("draft"), "got: {detail}");
    assert!(
        detail.contains(&first.to_string()),
        "the refusal names the draft holding the key, got: {detail}"
    );
}

#[tokio::test]
async fn a_successor_publishes_only_after_its_predecessor_leaves_the_published_plane() {
    // D-195 clause (3), measured rather than reasoned. §3.7 admits one published
    // row per key, so the order of the commit's two row moves is not free: the
    // failing order is a raw driver error - a 500 - and not a refusal, which is
    // why the rule is written down at `inst-su-commit` rather than left to be
    // rediscovered. With the flip first, `publish_rows` needs no change at all.
    let (repo, provider) = harness().await;
    let scope = AccessScope::for_tenant(tenant());
    let predecessor = published_predecessor(&repo, &provider, &scope).await;
    let successor = Uuid::from_u128(0xb_5002);
    supersede(
        &provider,
        &scope,
        tenant(),
        draft(successor, base_key(ChargeKind::Recurring), flat_content()),
    )
    .await
    .expect("stage the shape through the door that permits it");

    let validated = vec![(successor, RowVersion::new(0))];
    let refused = publish_rows(
        &provider,
        &scope,
        tenant(),
        plan(),
        validated.clone(),
        &fixture_readiness(),
    )
    .await
    .expect_err("publishing beside a live predecessor collides on the key");
    let RepoError::Db(detail) = &refused else {
        panic!("the collision arrives as a storage fault, got: {refused:?}");
    };
    assert!(
        detail.contains("UNIQUE"),
        "and it is the published-plane index that produced it, got: {detail}"
    );

    // The ordering `inst-su-commit` now states: the predecessor leaves first.
    flip_state(&provider, &scope, predecessor, LifecycleState::Superseded).await;
    let moved = publish_rows(
        &provider,
        &scope,
        tenant(),
        plan(),
        validated,
        &fixture_readiness(),
    )
    .await
    .expect("with the key free on the published plane, the flip is ordinary");

    assert_eq!(moved, vec![successor]);
    assert_eq!(
        stored_row(&provider, &scope, successor)
            .await
            .lifecycle_state,
        LifecycleState::Published.as_str()
    );
}

/// `commit_supersession_rows` through a real transaction — the row half of
/// `inst-su-commit`, whose whole point is that the caller does not order the two
/// moves.
async fn commit_supersession_rows(
    provider: &DBProvider<DbError>,
    scope: &AccessScope,
    tenant_id: Uuid,
    plan_id: PlanId,
    predecessor: Uuid,
    successor: (Uuid, RowVersion),
) -> Result<(), RepoError> {
    let scope = scope.clone();
    let (_, outcome) = provider
        .db()
        .in_transaction::<(), RepoError, _>(move |txn| {
            Box::pin(async move {
                bss_pricing::infra::storage::repo::price_repo::commit_supersession_rows(
                    txn,
                    &scope,
                    tenant_id,
                    plan_id,
                    predecessor,
                    successor,
                    &fixture_readiness(),
                    // The tenant default the predecessor published under. A
                    // successor cloned from a row that carries no policy of its
                    // own resolves nothing without it, and `publish_rows` refuses
                    // that set (review F1) — which is the subject of
                    // `a_supersession_whose_tenant_lost_its_default_is_refused_at_the_commit`
                    // and of nothing else on this plane.
                    Some("half_up/2"),
                )
                .await
            })
        })
        .await;
    outcome.map_err(|err| {
        err.into_domain(|infra| RepoError::Db(format!("supersession commit transaction: {infra}")))
    })
}

/// [`commit_supersession_rows`] for a tenant with **no** default rounding policy
/// — the world F1 describes, where the successor resolves nothing.
async fn commit_supersession_rows_resolving_nothing(
    provider: &DBProvider<DbError>,
    scope: &AccessScope,
    tenant_id: Uuid,
    plan_id: PlanId,
    predecessor: Uuid,
    successor: (Uuid, RowVersion),
) -> Result<(), RepoError> {
    let scope = scope.clone();
    let (_, outcome) = provider
        .db()
        .in_transaction::<(), RepoError, _>(move |txn| {
            Box::pin(async move {
                bss_pricing::infra::storage::repo::price_repo::commit_supersession_rows(
                    txn,
                    &scope,
                    tenant_id,
                    plan_id,
                    predecessor,
                    successor,
                    &fixture_readiness(),
                    None,
                )
                .await
            })
        })
        .await;
    outcome.map_err(|err| {
        err.into_domain(|infra| RepoError::Db(format!("supersession commit transaction: {infra}")))
    })
}

/// A key carrying its published predecessor and the staged successor draft — the
/// world `inst-su-commit` runs against.
async fn composed_supersession(
    repo: &PriceRepo,
    provider: &DBProvider<DbError>,
    scope: &AccessScope,
) -> (Uuid, Uuid) {
    let predecessor = published_predecessor(repo, provider, scope).await;
    let successor = Uuid::from_u128(0xb_5002);
    supersede(
        provider,
        scope,
        tenant(),
        draft(successor, base_key(ChargeKind::Recurring), flat_content()),
    )
    .await
    .expect("compose");
    (predecessor, successor)
}

#[tokio::test]
async fn the_supersession_commit_flips_the_predecessor_and_publishes_the_successor() {
    // `inst-su-commit`'s two row moves, in the one order that works, from one
    // call — so that no caller is in a position to order them wrongly. D-195
    // clause (3) in code rather than in prose.
    let (repo, provider) = harness().await;
    let scope = AccessScope::for_tenant(tenant());
    let (predecessor, successor) = composed_supersession(&repo, &provider, &scope).await;

    commit_supersession_rows(
        &provider,
        &scope,
        tenant(),
        plan(),
        predecessor,
        (successor, RowVersion::new(0)),
    )
    .await
    .expect("the pair commits");

    assert_eq!(
        stored_row(&provider, &scope, predecessor)
            .await
            .lifecycle_state,
        LifecycleState::Superseded.as_str(),
        "the predecessor left the published plane"
    );
    assert_eq!(
        stored_row(&provider, &scope, successor)
            .await
            .lifecycle_state,
        LifecycleState::Published.as_str(),
        "and the successor arrived on it"
    );
}

#[tokio::test]
async fn the_predecessors_flip_leaves_its_frozen_entity_tag_alone() {
    // D-141 / §3.7: the row version freezes with the published row's content and
    // neither sanctioned in-place mutation moves it — not the `lifecycle_state`
    // flips, not the monotonic `grandfatherUntil` tightening. A tag that moved
    // under a representation no caller can write to would report a stale cache
    // that is not stale.
    let (repo, provider) = harness().await;
    let scope = AccessScope::for_tenant(tenant());
    let (predecessor, successor) = composed_supersession(&repo, &provider, &scope).await;
    let before = stored_row(&provider, &scope, predecessor).await.row_version;

    commit_supersession_rows(
        &provider,
        &scope,
        tenant(),
        plan(),
        predecessor,
        (successor, RowVersion::new(0)),
    )
    .await
    .expect("the pair commits");

    assert_eq!(
        stored_row(&provider, &scope, predecessor).await.row_version,
        before,
        "the flip is not a content mutation and does not move the tag"
    );
}

#[tokio::test]
async fn the_supersession_commit_refuses_a_predecessor_that_is_no_longer_published() {
    // The replay case, and the reason the flip is a compare-and-swap rather than
    // an UPDATE the state machine is trusted to have gated: a committed unit
    // leaves the key's former current row `superseded`, and a second commit of
    // the same unit must not silently move a row that has already moved.
    let (repo, provider) = harness().await;
    let scope = AccessScope::for_tenant(tenant());
    let (predecessor, successor) = composed_supersession(&repo, &provider, &scope).await;
    flip_state(&provider, &scope, predecessor, LifecycleState::Superseded).await;

    let err = commit_supersession_rows(
        &provider,
        &scope,
        tenant(),
        plan(),
        predecessor,
        (successor, RowVersion::new(0)),
    )
    .await
    .expect_err("a row that has already left `published` is not supersedable again");

    // Not `NotDraft`: that variant's sentence names as the remedy an operation
    // this caller is not attempting. The remedy here is to recompose against the
    // key's new current row.
    let RepoError::NotSupersedable { id, state, .. } = err else {
        panic!("the refusal names the state it found, got: {err:?}");
    };
    assert_eq!(id, predecessor.to_string());
    assert_eq!(state, LifecycleState::Superseded.as_str());
}

#[tokio::test]
async fn the_supersession_commit_leaves_the_predecessor_standing_when_the_successor_will_not_publish()
 {
    // `inst-su-commit`: "or everything rolls back". The predecessor's flip is the
    // first move, so a successor refused by the *second* is exactly the case that
    // would leave a key with no current row at all — a sales outage produced by a
    // half-applied unit. The successor is refused on a version that moved.
    let (repo, provider) = harness().await;
    let scope = AccessScope::for_tenant(tenant());
    let (predecessor, successor) = composed_supersession(&repo, &provider, &scope).await;

    let err = commit_supersession_rows(
        &provider,
        &scope,
        tenant(),
        plan(),
        predecessor,
        (successor, RowVersion::new(7)),
    )
    .await
    .expect_err("a successor whose content moved since validation is refused");
    assert!(
        matches!(err, RepoError::StaleRowVersion { .. }),
        "the successor's own precondition still decides, got: {err:?}"
    );

    assert_eq!(
        stored_row(&provider, &scope, predecessor)
            .await
            .lifecycle_state,
        LifecycleState::Published.as_str(),
        "the flip rolled back with the publish it was paired with"
    );
    assert_eq!(
        stored_row(&provider, &scope, successor)
            .await
            .lifecycle_state,
        LifecycleState::Draft.as_str()
    );
}

#[tokio::test]
async fn a_plan_with_no_draft_rows_publishes_nothing_and_says_so() {
    let (repo, provider) = harness().await;
    let scope = AccessScope::for_tenant(tenant());

    let validated = validated_drafts(&repo, &scope, tenant(), plan()).await;
    let moved = publish_rows(
        &provider,
        &scope,
        tenant(),
        plan(),
        validated,
        &fixture_readiness(),
    )
    .await
    .expect("an empty plan is not an error");

    assert!(moved.is_empty());
}

#[tokio::test]
async fn another_tenants_draft_rows_are_invisible_to_a_publish() {
    let (repo, provider) = harness().await;
    let mine = AccessScope::for_tenant(tenant());
    let theirs_tenant = Uuid::from_u128(0x7e_22);
    let theirs = AccessScope::for_tenant(theirs_tenant);
    let my_row = Uuid::from_u128(0xb_0001);
    let their_row = Uuid::from_u128(0xb_0002);
    repo.create_draft(
        &mine,
        tenant(),
        draft(my_row, base_key(ChargeKind::Recurring), flat_content()),
    )
    .await
    .expect("author mine");
    repo.create_draft(
        &theirs,
        theirs_tenant,
        draft(
            their_row,
            new_subscriptions_key(ChargeKind::Recurring),
            flat_content(),
        ),
    )
    .await
    .expect("author theirs");

    let validated = validated_drafts(&repo, &mine, tenant(), plan()).await;
    let moved = publish_rows(
        &provider,
        &mine,
        tenant(),
        plan(),
        validated,
        &fixture_readiness(),
    )
    .await
    .expect("publish my rows");

    assert_eq!(moved, vec![my_row]);
    assert_eq!(
        stored_row(&provider, &theirs, their_row)
            .await
            .lifecycle_state,
        LifecycleState::Draft.as_str(),
        "SecureORM keeps another tenant's rows out of this plan's publish"
    );
}

// ---------------------------------------------------------------------------
// The validated set is the set, and nothing else publishes.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_row_whose_content_moved_since_validation_is_refused_by_its_own_tag() {
    // The defect this closes: the publish commit validates its subject, makes a
    // network round-trip to the `CatalogVersion` registry, and only then flips.
    // `in_transaction` opens the engine default - READ COMMITTED on Postgres -
    // so every statement takes a fresh snapshot and a concurrent `update_draft`
    // committing inside that window changes the content of a row the rule set
    // already passed. A re-derived draft set would publish the mutation. The
    // row's own entity tag is what refuses it (D-141), naming the row.
    let (repo, provider) = harness().await;
    let scope = AccessScope::for_tenant(tenant());
    let price_id = Uuid::from_u128(0xb_0001);
    repo.create_draft(
        &scope,
        tenant(),
        draft(price_id, base_key(ChargeKind::Recurring), flat_content()),
    )
    .await
    .expect("author the row");

    let validated = validated_drafts(&repo, &scope, tenant(), plan()).await;
    assert_eq!(validated, vec![(price_id, RowVersion::new(0))]);

    // The world moves: the row's content changes and its tag advances with it.
    let mut edited = flat_content();
    edited.row.amount_minor = Some(money(4_242));
    repo.update_draft(
        &scope,
        tenant(),
        price_id,
        RowVersion::new(0),
        edited,
        stamp(),
        /* on_behalf_of */ None,
    )
    .await
    .expect("edit the draft");

    let refusal = publish_rows(
        &provider,
        &scope,
        tenant(),
        plan(),
        validated,
        &fixture_readiness(),
    )
    .await
    .expect_err("a row that moved since validation must not publish");

    assert!(
        matches!(refusal, RepoError::StaleRowVersion { current, submitted, .. }
            if current == 1 && submitted == 0),
        "got {refusal:?}"
    );
    assert_eq!(
        stored_row(&provider, &scope, price_id)
            .await
            .lifecycle_state,
        LifecycleState::Draft.as_str(),
        "nothing published"
    );
}

#[tokio::test]
async fn a_row_authored_after_validation_is_not_published_by_this_commit() {
    // The other half of the same window: a `create_draft` committing between the
    // subject's assembly and the flip inserts a row the rule set never saw. It
    // is simply not in the set, so it stays `draft` and publishes with the next
    // revision - correct by construction, because nothing validated it.
    let (repo, provider) = harness().await;
    let scope = AccessScope::for_tenant(tenant());
    let judged = Uuid::from_u128(0xb_0001);
    let unjudged = Uuid::from_u128(0xb_0002);
    repo.create_draft(
        &scope,
        tenant(),
        draft(judged, base_key(ChargeKind::Recurring), flat_content()),
    )
    .await
    .expect("author the judged row");

    let validated = validated_drafts(&repo, &scope, tenant(), plan()).await;

    repo.create_draft(
        &scope,
        tenant(),
        draft(
            unjudged,
            new_subscriptions_key(ChargeKind::Recurring),
            flat_content(),
        ),
    )
    .await
    .expect("author the row nobody judged");

    let moved = publish_rows(
        &provider,
        &scope,
        tenant(),
        plan(),
        validated,
        &fixture_readiness(),
    )
    .await
    .expect("the validated row publishes");

    assert_eq!(moved, vec![judged]);
    assert_eq!(
        stored_row(&provider, &scope, unjudged)
            .await
            .lifecycle_state,
        LifecycleState::Draft.as_str(),
        "a row the rule set never saw must not become consumer-visible"
    );
}

#[tokio::test]
async fn a_row_deleted_since_validation_refuses_the_whole_publish() {
    let (repo, provider) = harness().await;
    let scope = AccessScope::for_tenant(tenant());
    let kept = Uuid::from_u128(0xb_0001);
    let dropped = Uuid::from_u128(0xb_0002);
    for (price_id, key) in [
        (kept, base_key(ChargeKind::Recurring)),
        (dropped, new_subscriptions_key(ChargeKind::Recurring)),
    ] {
        repo.create_draft(&scope, tenant(), draft(price_id, key, flat_content()))
            .await
            .expect("author");
    }
    let validated = validated_drafts(&repo, &scope, tenant(), plan()).await;
    repo.delete_draft(&scope, tenant(), dropped, RowVersion::new(0), stamp(), None)
        .await
        .expect("discard one of them");

    let refusal = publish_rows(
        &provider,
        &scope,
        tenant(),
        plan(),
        validated,
        &fixture_readiness(),
    )
    .await
    .expect_err("a validated row that is gone must refuse the publish");

    assert!(
        matches!(refusal, RepoError::NotFound { .. }),
        "got {refusal:?}"
    );
    assert_eq!(
        stored_row(&provider, &scope, kept).await.lifecycle_state,
        LifecycleState::Draft.as_str(),
        "the transaction took the other row's flip back with it"
    );
}

#[tokio::test]
async fn a_validated_row_of_another_plan_is_caught_by_the_count() {
    // The count assertion's one reachable arm without concurrency, and the only
    // thing that tests it at all. The pre-read is scoped by tenant and identity
    // — it mirrors `mutable_draft`, which does not filter `plan_id` — so a
    // validated entry naming a draft row of another plan of the same tenant
    // passes it. The UPDATE's own plan filter then excludes that row, and the
    // number that moved is one short of the number validated. Without the count
    // the publish would report success having published fewer rows than it was
    // handed.
    let (repo, provider) = harness().await;
    let scope = AccessScope::for_tenant(tenant());
    let mine = Uuid::from_u128(0xb_0001);
    let elsewhere = Uuid::from_u128(0xb_0002);
    let other_plan = PlanId::new(Uuid::from_u128(0x9_1a5));

    repo.create_draft(
        &scope,
        tenant(),
        draft(mine, base_key(ChargeKind::Recurring), flat_content()),
    )
    .await
    .expect("author a row of the plan under publish");

    let foreign_key = ScopeKey::new(
        other_plan,
        CurrencyCode::new("USD").expect("three letters"),
        Region::new("EU").expect("a non-blank region"),
        PhaseId::new(Uuid::from_u128(0xfa_5e)),
        PriceEligibility::AllSubscriptions,
        ChargeKind::Recurring,
        Cohort::None,
    )
    .expect("all_subscriptions pairs with cohort none");
    repo.create_draft(
        &scope,
        tenant(),
        draft(elsewhere, foreign_key, flat_content()),
    )
    .await
    .expect("author a row of a different plan of the same tenant");

    // Both rows are `draft` at version 0, so both clear the pre-read; only one
    // of them belongs to the plan being published.
    let validated = vec![(mine, RowVersion::new(0)), (elsewhere, RowVersion::new(0))];
    let refusal = publish_rows(
        &provider,
        &scope,
        tenant(),
        plan(),
        validated,
        &fixture_readiness(),
    )
    .await
    .expect_err("a validated set naming a foreign plan's row must not report success");

    assert!(
        refusal
            .to_string()
            .contains("validated 2 price rows and 1 moved"),
        "the refusal must name the shortfall, got {refusal:?}"
    );
    assert_eq!(
        stored_row(&provider, &scope, mine).await.lifecycle_state,
        LifecycleState::Draft.as_str(),
        "and the row that did move rolled back with the transaction"
    );
}

#[tokio::test]
async fn the_keyset_page_walks_the_same_total_order_the_list_declares() {
    // The caller `list_for_plan_filters_by_state_and_orders_stably` was written
    // for. D-125 forbids offset pagination over an append-only store, so the
    // page has to be a keyset walk on the `price_id ASC` order that test pins —
    // and this is where the two are shown to be the same order rather than two
    // orders that happen to agree today.
    let (repo, _provider) = harness().await;
    let scope = AccessScope::for_tenant(tenant());
    let ids = [
        Uuid::from_u128(0xbb01),
        Uuid::from_u128(0xbb02),
        Uuid::from_u128(0xbb03),
        Uuid::from_u128(0xbb04),
    ];

    for (price_id, charge_kind) in [
        (ids[3], ChargeKind::OneTimeSetup),
        (ids[1], ChargeKind::OneTime),
        (ids[0], ChargeKind::Recurring),
    ] {
        let mut content = flat_content();
        content.row = PriceRow::new(charge_kind, Some(ModelKind::Flat));
        content.row.amount_minor = Some(money(1_000));
        repo.create_draft(
            &scope,
            tenant(),
            draft(price_id, base_key(charge_kind), content),
        )
        .await
        .expect("create");
    }

    // **One `graduated` row, because the band claim below needs an operand.**
    // Every row this case seeded was `flat` with no bands at all until 2026-08-20,
    // so the assertion that "the paged read carries each row's band set" checked
    // `charge_kind` — a value copied straight out of the seed's own key — and
    // `hydrate_bands`, the second and independent band join `list_for_plan`,
    // `list_for_plan_page` and `list_history_page` use (it is *not* the
    // `load_bands` that `find` uses), could have answered an empty set for every
    // row with the only case naming that guarantee still green.
    let mut banded_content = flat_content();
    banded_content.row = PriceRow::new(ChargeKind::Usage, Some(ModelKind::Graduated));
    banded_content.row.bands = graduated_content().row.bands;
    banded_content.row.meter = Some("api_calls".to_owned());
    "region:eu".clone_into(&mut banded_content.row.dimension_key);
    repo.create_draft(
        &scope,
        tenant(),
        draft(
            ids[2],
            usage_key(Some("api_calls"), "region:eu"),
            banded_content,
        ),
    )
    .await
    .expect("create the banded row");

    // The whole result, in one page, is exactly what the unbounded list gives.
    let whole = repo
        .list_for_plan(&scope, tenant(), plan(), &[LifecycleState::Draft])
        .await
        .expect("list");
    let paged = repo
        .list_for_plan_page(
            &scope,
            tenant(),
            plan(),
            &[LifecycleState::Draft],
            None,
            100,
        )
        .await
        .expect("page");
    assert_eq!(
        paged.iter().map(|row| row.price_id).collect::<Vec<_>>(),
        whole.iter().map(|row| row.price_id).collect::<Vec<_>>(),
        "the page and the list must not be two orders"
    );

    // `limit` bounds the page; `after` resumes STRICTLY after the key, so the
    // row the cursor names is never handed out twice.
    let first = repo
        .list_for_plan_page(&scope, tenant(), plan(), &[LifecycleState::Draft], None, 2)
        .await
        .expect("first page");
    assert_eq!(
        first.iter().map(|row| row.price_id).collect::<Vec<_>>(),
        vec![ids[0], ids[1]]
    );

    let second = repo
        .list_for_plan_page(
            &scope,
            tenant(),
            plan(),
            &[LifecycleState::Draft],
            Some(ids[1]),
            2,
        )
        .await
        .expect("second page");
    assert_eq!(
        second.iter().map(|row| row.price_id).collect::<Vec<_>>(),
        vec![ids[2], ids[3]],
        "`after` is exclusive, or a walk repeats one row on every page boundary"
    );

    // Past the end there is nothing, and an empty state set still selects
    // nothing - the page inherits both of `list_for_plan`'s contracts.
    assert!(
        repo.list_for_plan_page(
            &scope,
            tenant(),
            plan(),
            &[LifecycleState::Draft],
            Some(ids[3]),
            2,
        )
        .await
        .expect("past the end")
        .is_empty()
    );
    assert!(
        repo.list_for_plan_page(&scope, tenant(), plan(), &[], None, 2)
            .await
            .expect("no states")
            .is_empty()
    );

    // The bands travel with the row on the paged path too: a page that dropped
    // them would answer a geometry no rule could evaluate.
    let banded = paged
        .iter()
        .find(|row| row.price_id == ids[2])
        .expect("the graduated row is in the page");
    assert_eq!(
        banded.row.bands,
        graduated_content().row.bands,
        "`hydrate_bands` carries the whole ladder onto the paged row, in ascending `from_qty` \
         order: a page that answered an empty set would price every tier at nothing"
    );
    assert!(
        banded
            .row
            .bands
            .windows(2)
            .all(|pair| pair[0].from_qty < pair[1].from_qty),
        "and the order is ascending rather than whatever the join returned: {:?}",
        banded.row.bands
    );
    // The flat rows keep an empty ladder, so the assertion above is about the join
    // rather than about a constant.
    assert!(
        paged
            .iter()
            .filter(|row| row.price_id != ids[2])
            .all(|row| row.row.bands.is_empty()),
        "a flat row carries no bands"
    );
}

/// The actor and instant every mutating repository call now records (D-135 - the
/// audit row commits inside the mutation's own transaction).
fn stamp() -> bss_pricing::domain::audit::AuditStamp {
    bss_pricing::domain::audit::AuditStamp {
        actor_principal_id: uuid::Uuid::from_u128(0xac_10),
        recorded_at: OffsetDateTime::now_utc(),
        correlation_id: TEST_CORRELATION,
    }
}

/// The same stamp under a named correlation.
fn stamp_correlated(correlation_id: Uuid) -> bss_pricing::domain::audit::AuditStamp {
    bss_pricing::domain::audit::AuditStamp {
        correlation_id,
        ..stamp()
    }
}

/// **Every price-plane record carries the correlation its caller supplied**
/// (D-178).
///
/// The whole authoring plane's correlation was unconstrained: replacing both
/// `draft.correlation_id` and `stamp.correlation_id` with a fresh
/// `Uuid::now_v7()` at all four sites in `price_repo` left the suite green.
/// Clause (1) - never NULL - is type-enforced by `NewPriceDraft` and
/// `AuditStamp` taking a bare `Uuid`, so it survives any mint; clause (2) has no
/// equality to break on this plane, because no price route writes two records in
/// one call. `rest_plans.rs::two_records_of_one_patch_carry_one_correlation_id`
/// covers the plan plane exactly that way and cannot be repeated here.
///
/// So the binding is taken where the correlation is an **input** rather than an
/// edge-established value: the repository is what answers, and the three
/// mutations are driven under three **distinct** correlations. A per-record mint
/// fails on all three; a record that borrowed a neighbouring call's value fails
/// on the pair it confused. Blanking to `None` fails at the type level and never
/// reaches here.
///
/// The bulk-import arm (D-118 / D-177) is the reason this matters beyond
/// tidiness: it is one call authoring many rows, and it is the first place a
/// per-record mint becomes an untraceable provenance instead of a redundancy.
#[tokio::test]
async fn every_price_record_carries_the_correlation_its_caller_supplied() {
    const CREATED_BY_CALL: Uuid = Uuid::from_u128(0x_c0_11_00_01);
    const EDITED_BY_CALL: Uuid = Uuid::from_u128(0x_c0_11_00_02);
    const DELETED_BY_CALL: Uuid = Uuid::from_u128(0x_c0_11_00_03);

    let (repo, provider) = harness().await;
    let scope = AccessScope::for_tenant(tenant());
    let price_id = Uuid::from_u128(0xb_c0);

    let mut created = draft(price_id, base_key(ChargeKind::Recurring), flat_content());
    created.correlation_id = CREATED_BY_CALL;
    repo.create_draft(&scope, tenant(), created)
        .await
        .expect("create the draft row");

    repo.update_draft(
        &scope,
        tenant(),
        price_id,
        RowVersion::new(0),
        flat_content(),
        stamp_correlated(EDITED_BY_CALL),
        /* on_behalf_of */ None,
    )
    .await
    .expect("edit it");

    repo.delete_draft(
        &scope,
        tenant(),
        price_id,
        RowVersion::new(1),
        stamp_correlated(DELETED_BY_CALL),
        None,
    )
    .await
    .expect("take it away");

    let conn = provider.conn().expect("conn");
    let written: Vec<(String, Option<Uuid>)> = audit_log::Entity::find()
        .secure()
        .scope_with(&scope)
        .order_by(audit_log::Column::Seq, sea_orm::Order::Asc)
        .all(&conn)
        .await
        .expect("read the trail")
        .into_iter()
        .map(|row| (row.action, row.correlation_id))
        .collect();

    assert_eq!(
        written,
        vec![
            ("create".to_owned(), Some(CREATED_BY_CALL)),
            ("update".to_owned(), Some(EDITED_BY_CALL)),
            ("delete".to_owned(), Some(DELETED_BY_CALL)),
        ],
        "three calls, three records, and each names the call that wrote it"
    );
}

#[tokio::test]
async fn a_grandfathered_generation_may_not_be_superseded() {
    // Foundation §4.3 is normative: "An `existing_grandfathered` row is **immutable in
    // price** and MUST NOT be superseded", restated in S7 §1.7's UC table ("Attempt to
    // supersede or reprice an `existing_grandfathered` row → rejected; only tightening
    // `grandfatherUntil` is allowed"). Found by review 2026-08-05: **nothing enforced
    // it.** The unit guard compares `PriceRow` fields and a `PriceRow` carries no
    // eligibility class; compose reads only intervals; this door checked occupancy only.
    //
    // It is money rather than tidiness, and the two halves compound. The successor
    // rewrites the retained cohort's price — the exact thing the class exists to
    // prevent. And compose would hand `adjust_effective_to` a shorten of that
    // generation's window to the changeover, while that function does **not** enforce
    // D-04's `inst-co-bounds` (coverage through `grandfatherUntil` plus the longest
    // billing cycle) — a gap recorded in its own doc — so a bound subscriber is
    // stranded mid-cycle with no guard on either side.
    //
    // The door is the cheapest correct home: it already holds the key.
    let (repo, provider) = harness().await;
    let scope = AccessScope::for_tenant(tenant());
    let generation = grandfathered_key(ChargeKind::Recurring, at(12));
    let retained = Uuid::from_u128(0xb_5101);
    repo.create_draft(
        &scope,
        tenant(),
        draft(retained, generation.clone(), flat_content()),
    )
    .await
    .expect("author the grandfathered copy");
    flip_state(&provider, &scope, retained, LifecycleState::Published).await;

    let err = supersede(
        &provider,
        &scope,
        tenant(),
        draft(Uuid::from_u128(0xb_5102), generation, flat_content()),
    )
    .await
    .expect_err("a retained generation is immutable in price");

    let RepoError::NotSupersedable { state, .. } = err else {
        panic!("the class refusal names why, got: {err:?}");
    };
    assert!(
        state.contains("existing_grandfathered"),
        "the refusal names the class, got: {state}"
    );
}

// ---------------------------------------------------------------------------
// D-196 clause (3): the repository carries the usage line into and out of keys
// ---------------------------------------------------------------------------

/// The line a key names, as the authoring door receives it.
fn usage_key(meter: Option<&str>, dimension: &str) -> ScopeKey {
    base_key(ChargeKind::Usage)
        .with_usage_line(
            meter.map(|m| Meter::new(m).expect("a non-blank meter")),
            DimensionKey::new(dimension),
        )
        .expect("a usage key carries its line")
}

/// A usage row's content, carrying the same line its key does.
fn usage_line_content(meter: Option<&str>, dimension: &str) -> PriceContent {
    let mut content = flat_content();
    // Its own rounding policy, because these rows publish through **real** doors —
    // `infra::cutover::commit_cutover` resolves the tenant default itself and
    // `publish_rows` refuses a set that resolves none at all (review F1,
    // 2026-08-19). `flat_content` carries none, and this fixture's subject is the
    // usage line's key, never its rounding.
    content.rounding_policy_ref = Some("half_up/2".to_owned());
    // And its own tax category, for the identical reason one frozen column over:
    // `publish_rows` refuses a publish that resolves none either (H14, review
    // 2026-08-19), and the **real** door reads the readiness from this tenant's
    // taxonomy rather than from `fixture_readiness`, which declares no region here.
    content.tax_category_ref = Some("standard".to_owned());
    content.row = PriceRow::new(ChargeKind::Usage, Some(ModelKind::PerUnit));
    // The **rate** column, not `amount_minor`: `check_amount_placement` refuses a
    // `per_unit` row that keeps its money in the amount column, and these rows
    // publish through real doors.
    content.row.unit_rate = Some(nano_rate(1_000_000_000));
    content.row.meter = meter.map(std::borrow::ToOwned::to_owned);
    dimension.clone_into(&mut content.row.dimension_key);
    content
}

#[tokio::test]
async fn two_usage_lines_of_one_market_both_author() {
    // D-103's confirmed example, through the door that refused it: the second
    // line used to answer `DUPLICATE_SCOPE_KEY` because both rendered one key.
    let (repo, _provider) = harness().await;
    let scope = AccessScope::for_tenant(tenant());

    repo.create_draft(
        &scope,
        tenant(),
        draft(
            Uuid::from_u128(0xd1_96_01),
            usage_key(Some("cloudlets"), ""),
            usage_line_content(Some("cloudlets"), ""),
        ),
    )
    .await
    .expect("the first meter takes its key");

    repo.create_draft(
        &scope,
        tenant(),
        draft(
            Uuid::from_u128(0xd1_96_02),
            usage_key(Some("egress_gb"), ""),
            usage_line_content(Some("egress_gb"), ""),
        ),
    )
    .await
    .expect("a second meter is a second key, which is the whole of D-196");
}

#[tokio::test]
async fn the_occupancy_read_finds_a_meterless_occupant() {
    // **The filter carries the same NULL trap the index did, one layer up.**
    // A key with no meter renders `meter IS NULL`, and `Column::Meter.eq(None)`
    // is `meter = NULL`, which matches nothing — so the occupancy read would
    // answer "free" over an occupied key and the duplicate would be caught by
    // the index as a driver error rather than by the door as a refusal.
    let (repo, _provider) = harness().await;
    let scope = AccessScope::for_tenant(tenant());

    repo.create_draft(
        &scope,
        tenant(),
        draft(
            Uuid::from_u128(0xd1_96_03),
            usage_key(None, ""),
            usage_line_content(None, ""),
        ),
    )
    .await
    .expect("the meterless line takes its key");

    let err = repo
        .create_draft(
            &scope,
            tenant(),
            draft(
                Uuid::from_u128(0xd1_96_04),
                usage_key(None, ""),
                usage_line_content(None, ""),
            ),
        )
        .await
        .expect_err("a second meterless line on one key must be refused");

    assert!(
        matches!(err, RepoError::DuplicateScopeKey(_)),
        "the door refuses it by name, not the index by driver error: {err:?}"
    );
}

#[tokio::test]
async fn a_metered_line_does_not_occupy_the_meterless_key() {
    // The other direction of the same filter: the two are different keys, so
    // neither read may find the other.
    let (repo, _provider) = harness().await;
    let scope = AccessScope::for_tenant(tenant());

    repo.create_draft(
        &scope,
        tenant(),
        draft(
            Uuid::from_u128(0xd1_96_05),
            usage_key(Some("cloudlets"), ""),
            usage_line_content(Some("cloudlets"), ""),
        ),
    )
    .await
    .expect("the metered line takes its own key");

    repo.create_draft(
        &scope,
        tenant(),
        draft(
            Uuid::from_u128(0xd1_96_06),
            usage_key(None, ""),
            usage_line_content(None, ""),
        ),
    )
    .await
    .expect("the meterless key is free");
}

#[tokio::test]
async fn a_loaded_key_carries_the_line_it_was_filed_under() {
    // `to_scope_key` rebuilds from the columns, so a round trip has to return
    // the ninth and tenth axes or every consumer of a loaded key — the window
    // plane, the approval register, the supersession door — compares keys that
    // are equal on eight axes and different rows.
    let (repo, _provider) = harness().await;
    let scope = AccessScope::for_tenant(tenant());
    let price_id = Uuid::from_u128(0xd1_96_07);

    repo.create_draft(
        &scope,
        tenant(),
        draft(
            price_id,
            usage_key(Some("cloudlets"), "region=eu"),
            usage_line_content(Some("cloudlets"), "region=eu"),
        ),
    )
    .await
    .expect("author the dimensioned line");

    let loaded = repo
        .find(&scope, tenant(), price_id)
        .await
        .expect("read")
        .expect("the row is there");

    assert_eq!(
        loaded.scope_key.meter().map(Meter::as_str),
        Some("cloudlets")
    );
    assert_eq!(loaded.scope_key.dimension_key().as_str(), "region=eu");
    assert_eq!(loaded.scope_key, usage_key(Some("cloudlets"), "region=eu"));
}

#[tokio::test]
async fn a_content_naming_a_line_its_key_does_not_is_refused() {
    // **A refusal, deliberately, where `charge_kind` gets a rewrite.**
    // `authored_content` rewrites `charge_kind` from the key because the wire
    // cannot express it — a placeholder is forced. The wire *can* express a
    // meter, so a disagreement is a caller's mistake worth naming rather than
    // one to paper over. And a silent rewrite here would be worse than untidy:
    // it would make the D-82 unit guard's `meter` and `dimensionKey` clauses
    // unreachable, which is exactly how `charge_kind`'s placeholder cost three
    // Criticals on 2026-08-06.
    let (repo, _provider) = harness().await;
    let scope = AccessScope::for_tenant(tenant());

    let err = repo
        .create_draft(
            &scope,
            tenant(),
            draft(
                Uuid::from_u128(0xd1_96_08),
                usage_key(Some("cloudlets"), ""),
                usage_line_content(Some("egress_gb"), ""),
            ),
        )
        .await
        .expect_err("a row whose meter is not its key's must not be stored under either");

    let message = format!("{err:?}");
    assert!(
        message.contains("cloudlets") && message.contains("egress_gb"),
        "the refusal names both lines so the author can see which is wrong: {message}"
    );
}

#[tokio::test]
async fn an_update_may_not_move_the_row_to_another_line() {
    // **The defect D-196 clause (3) exposed, pinned.** `update_draft` rewrote
    // `meter` and `dimension_key` as ordinary content columns. Once the pair
    // became key axes, that meant a `PATCH` could move a draft onto a *different*
    // canonical scope key with no occupancy check anywhere on the path — the key
    // another row might already hold — and the only thing that would notice is
    // the partial `UNIQUE`, arriving as a driver error rather than as a refusal.
    //
    // The remedy this door names is the one its own doc already named for every
    // other axis: delete the draft and author another one.
    let (repo, _provider) = harness().await;
    let scope = AccessScope::for_tenant(tenant());
    let price_id = Uuid::from_u128(0xd1_96_09);

    let created = repo
        .create_draft(
            &scope,
            tenant(),
            draft(
                price_id,
                usage_key(Some("cloudlets"), ""),
                usage_line_content(Some("cloudlets"), ""),
            ),
        )
        .await
        .expect("author the metered line");

    let err = repo
        .update_draft(
            &scope,
            tenant(),
            price_id,
            created.row_version,
            usage_line_content(Some("egress_gb"), ""),
            stamp(),
            /* on_behalf_of */ None,
        )
        .await
        .expect_err("an update may not move the row's line");

    let message = format!("{err:?}");
    assert!(
        message.contains("cloudlets") && message.contains("egress_gb"),
        "the refusal names the stored line and the submitted one: {message}"
    );

    // And the row is untouched — the refusal is before the write.
    let read = repo
        .find(&scope, tenant(), price_id)
        .await
        .expect("read")
        .expect("the row survives a refused update");
    assert_eq!(read.scope_key.meter().map(Meter::as_str), Some("cloudlets"));
    assert_eq!(read.row_version, created.row_version);
}

// ---------------------------------------------------------------------------
// D-196 clause (3), the other half: the two axis **columns** hold the axis, not
// the caller's spelling of it
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_meter_with_stray_whitespace_does_not_mint_a_second_key() {
    // **The axis and the column that stores it disagreed by whitespace.**
    // `Meter::new` and `DimensionKey::new` both `trim()`, so the canonical scope
    // key a row is filed under carries the trimmed value — while `content_model`
    // persisted `row.meter` and `row.dimension_key` as the caller's raw strings.
    //
    // Every gate over the key then compared the trimmed value against the
    // untrimmed column: `scope_key_filter` renders `meter = 'api_calls'`, so
    // `find_key_occupant` read "this key is free" over an occupied one, and both
    // partial UNIQUE indexes key over `COALESCE(meter, '')` and `dimension_key`
    // *as stored*, so neither of them noticed either.
    //
    // The consequence is the one §3.7 exists to forbid: authoring `"api_calls "`
    // and then `"api_calls"` left **two draft rows on one canonical scope key** —
    // and then two *published* ones, because no publish rule judges duplicate
    // keys (`domain::import`'s in-batch check is the only DUPLICATE_SCOPE_KEY
    // producer over a set) so `publish_rows` flips both in one UPDATE.
    let (repo, provider) = harness().await;
    let scope = AccessScope::for_tenant(tenant());
    let padded = Uuid::from_u128(0xd1_96_0a);
    let plain = Uuid::from_u128(0xd1_96_0b);

    let created = repo
        .create_draft(
            &scope,
            tenant(),
            draft(
                padded,
                usage_key(Some("api_calls "), " region=eu "),
                usage_line_content(Some("api_calls "), " region=eu "),
            ),
        )
        .await
        .expect("the padded line authors; whitespace is not a different meter");

    // The same canonical key, spelled without the space.
    let err = repo
        .create_draft(
            &scope,
            tenant(),
            draft(
                plain,
                usage_key(Some("api_calls"), "region=eu"),
                usage_line_content(Some("api_calls"), "region=eu"),
            ),
        )
        .await
        .expect_err("one canonical scope key takes one current row, however it is spelled");

    assert!(
        matches!(err, RepoError::DuplicateScopeKey(_)),
        "the door refuses it by name rather than admitting a second row on one key: {err:?}"
    );
    assert!(
        repo.find(&scope, tenant(), plain)
            .await
            .expect("read")
            .is_none(),
        "and nothing was written for the refused second row"
    );

    // The mechanism, read back: the stored columns are the key's axes, so a
    // loaded key equals the key the row was filed under and the occupancy read
    // above had something to match against.
    let stored = stored_row(&provider, &scope, padded).await;
    assert_eq!(
        stored.meter.as_deref(),
        Some("api_calls"),
        "the column holds the axis, not the spelling the caller sent"
    );
    assert_eq!(stored.dimension_key, "region=eu");
    assert_eq!(
        created.scope_key.meter().map(Meter::as_str),
        stored.meter.as_deref(),
        "the key the row is filed under and the column that stores that axis must be one value"
    );
    assert_eq!(
        created.scope_key.dimension_key().as_str(),
        stored.dimension_key,
        "and the same for the tenth axis"
    );
}

#[tokio::test]
async fn the_reverse_authoring_order_is_refused_too() {
    // **The positive control, and the asymmetry is why it is needed.** Trimmed
    // first and padded second was *already* refused before the fix above — the
    // occupancy filter's trimmed literal matches a stored trimmed value — so a
    // probe written in this order would have passed against the defect and
    // proved nothing. Both orders now answer the same way, which is what "one
    // canonical key" means.
    let (repo, _provider) = harness().await;
    let scope = AccessScope::for_tenant(tenant());

    repo.create_draft(
        &scope,
        tenant(),
        draft(
            Uuid::from_u128(0xd1_96_0c),
            usage_key(Some("api_calls"), ""),
            usage_line_content(Some("api_calls"), ""),
        ),
    )
    .await
    .expect("the trimmed line takes the key");

    let err = repo
        .create_draft(
            &scope,
            tenant(),
            draft(
                Uuid::from_u128(0xd1_96_0d),
                usage_key(Some(" api_calls"), ""),
                usage_line_content(Some(" api_calls"), ""),
            ),
        )
        .await
        .expect_err("the key is held");

    assert!(
        matches!(err, RepoError::DuplicateScopeKey(_)),
        "the same refusal in the other order: {err:?}"
    );
}

#[tokio::test]
async fn an_update_may_respell_the_stored_line_with_stray_whitespace() {
    // `check_update_keeps_the_line` compares the stored pair against the
    // submitted one, and both sides used to be raw — so a `PATCH` resubmitting
    // the row's own line with a stray space was answered `USAGE_LINE_AXIS_MISMATCH`
    // and the row could not even be *normalised* in place. Both sides are now the
    // canonical line, so what the guard refuses is a line **move** and nothing
    // else, which is the rule D-196 actually states.
    let (repo, provider) = harness().await;
    let scope = AccessScope::for_tenant(tenant());
    let price_id = Uuid::from_u128(0xd1_96_0e);

    let created = repo
        .create_draft(
            &scope,
            tenant(),
            draft(
                price_id,
                usage_key(Some("cloudlets"), "region=eu"),
                usage_line_content(Some("cloudlets"), "region=eu"),
            ),
        )
        .await
        .expect("author the dimensioned line");

    let updated = repo
        .update_draft(
            &scope,
            tenant(),
            price_id,
            created.row_version,
            usage_line_content(Some(" cloudlets "), " region=eu "),
            stamp(),
            /* on_behalf_of */ None,
        )
        .await
        .expect("whitespace around an axis value is not a move to another line");

    assert_eq!(
        updated.scope_key.meter().map(Meter::as_str),
        Some("cloudlets")
    );
    let stored = stored_row(&provider, &scope, price_id).await;
    assert_eq!(stored.meter.as_deref(), Some("cloudlets"));
    assert_eq!(stored.dimension_key, "region=eu");
}

// ---------------------------------------------------------------------------
// `PriceCreated` — the producer S3 puts on this door
// ---------------------------------------------------------------------------

async fn outbox_events(provider: &DBProvider<DbError>, scope: &AccessScope) -> Vec<String> {
    let conn = provider.conn().expect("conn");
    bss_pricing::infra::storage::entity::outbox::Entity::find()
        .secure()
        .scope_with(scope)
        .all(&conn)
        .await
        .expect("read the outbox")
        .into_iter()
        .map(|row| row.event_name)
        .collect()
}

#[tokio::test]
async fn authoring_a_draft_row_emits_price_created() {
    // **S3 puts the producer here in as many words**: "a draft price row is
    // authored on the canonical scope key ... `PriceCreated` emits per row", and
    // "`PriceCreated` on row authoring". The event has been declared and
    // producerless since the gear was created — every consumer counting row
    // creations has been counting zero.
    let (repo, provider) = harness().await;
    let scope = AccessScope::for_tenant(tenant());

    repo.create_draft(
        &scope,
        tenant(),
        draft(
            Uuid::from_u128(0x9c_01),
            base_key(ChargeKind::Recurring),
            flat_content(),
        ),
    )
    .await
    .expect("author the row");

    assert_eq!(outbox_events(&provider, &scope).await, vec!["PriceCreated"]);
}

#[tokio::test]
async fn editing_and_deleting_a_draft_emit_nothing_further() {
    // The event is `PriceCreated`, not `PriceTouched`: it fires once, on the act
    // that brought the row into existence. The audit chain is what carries the
    // edits, and it already does.
    let (repo, provider) = harness().await;
    let scope = AccessScope::for_tenant(tenant());
    let price_id = Uuid::from_u128(0x9c_02);

    let created = repo
        .create_draft(
            &scope,
            tenant(),
            draft(price_id, base_key(ChargeKind::Recurring), flat_content()),
        )
        .await
        .expect("author the row");
    let mut edited = flat_content();
    edited.row.amount_minor = Some(money(2_000));
    repo.update_draft(
        &scope,
        tenant(),
        price_id,
        created.row_version,
        edited,
        stamp(),
        /* on_behalf_of */ None,
    )
    .await
    .expect("edit it");

    assert_eq!(
        outbox_events(&provider, &scope).await,
        vec!["PriceCreated"],
        "one creation, one event"
    );

    // **And the delete**, which this case is half named for and never called until
    // 2026-08-20: a `delete_draft` that enqueued an event would have left the suite
    // green while the one case claiming "it fires once" read as covering it.
    repo.delete_draft(
        &scope,
        tenant(),
        price_id,
        RowVersion::new(1),
        stamp(),
        /* on_behalf_of */ None,
    )
    .await
    .expect("discard the draft");
    assert_eq!(
        outbox_events(&provider, &scope).await,
        vec!["PriceCreated"],
        "and the discard enqueues nothing either: the row's whole outbox history is the one \
         event its creation filed"
    );
}

// ---------------------------------------------------------------------------
// The cutover's row plane (`inst-co-supersede`, D-100)
// ---------------------------------------------------------------------------

async fn cutover_rows(
    provider: &DBProvider<DbError>,
    scope: &AccessScope,
    predecessor: Uuid,
    successor: (Uuid, RowVersion),
    copy: (Uuid, RowVersion),
    cutover_at: OffsetDateTime,
) -> Result<(), RepoError> {
    let scope = scope.clone();
    let (_, outcome) = provider
        .db()
        .in_transaction::<(), RepoError, _>(move |txn| {
            Box::pin(async move {
                bss_pricing::infra::storage::repo::price_repo::commit_cutover_rows(
                    txn,
                    &scope,
                    tenant(),
                    plan(),
                    predecessor,
                    successor,
                    copy,
                    cutover_at,
                    &fixture_readiness(),
                    // The tenant default the predecessor published under. A
                    // successor and a copy cloned from a row that carries no
                    // policy of its own resolve nothing without it, and
                    // `publish_rows` refuses that set (review F1, 2026-08-19).
                    Some("half_up/2"),
                )
                .await
            })
        })
        .await;
    outcome.map_err(|err| err.into_domain(|infra| RepoError::Db(format!("cutover rows: {infra}"))))
}

/// A published predecessor, a successor drafted on its own key, and a copy drafted
/// on the generation `cutover_at` mints.
///
/// **The instant is a parameter because the copy's key is built from it.** It was a
/// constant until 2026-08-06, which left the cross-plane case seeding a copy on the
/// 2026 generation while committing at a 2099 one — an incoherence no assertion
/// could see until `refuse_ungenerational` began comparing the cohort against the
/// act's own instant, and then it reddened that case immediately.
async fn seeded_cutover(
    repo: &PriceRepo,
    provider: &DBProvider<DbError>,
    scope: &AccessScope,
    meter: Option<&str>,
    cutover_at: OffsetDateTime,
) -> (Uuid, (Uuid, RowVersion), (Uuid, RowVersion)) {
    let key = usage_key(meter, "");
    let predecessor = Uuid::from_u128(0xc0_01);
    repo.create_draft(
        scope,
        tenant(),
        draft(predecessor, key.clone(), usage_line_content(meter, "")),
    )
    .await
    .expect("author the predecessor");
    flip_state(provider, scope, predecessor, LifecycleState::Published).await;

    let successor = Uuid::from_u128(0xc0_02);
    let mut content = usage_line_content(meter, "");
    content.supersedes_price_id = Some(predecessor);
    // On a transaction, because the entry point takes one: it writes the successor
    // and stamps the predecessor's link, so a failure part way through a bare
    // connection commits half of it.
    let staged_scope = scope.clone();
    let staged_draft = draft(successor, key.clone(), content);
    let (_, staged) = provider
        .db()
        .in_transaction::<_, bss_pricing::infra::storage::RepoError, _>(move |txn| {
            Box::pin(async move {
                // Boxed for the reason the call was boxed before it moved onto a
                // transaction: the future is over 16 KiB and `clippy::large_futures`
                // denies it unboxed.
                Box::pin(
                    bss_pricing::infra::storage::repo::price_repo::insert_successor_draft_on(
                        txn,
                        &staged_scope,
                        tenant(),
                        staged_draft,
                    ),
                )
                .await
            })
        })
        .await;
    let (authored, _) = staged.expect("stage the successor on the predecessor's key");

    let copy_id = Uuid::from_u128(0xc0_03);
    let copy_key = bss_pricing::domain::cutover::grandfathered_copy_key(&key, cutover_at, &[])
        .expect("a fresh generation");
    let copied = repo
        .create_draft(
            scope,
            tenant(),
            draft(copy_id, copy_key, usage_line_content(meter, "")),
        )
        .await
        .expect("author the grandfathered copy");

    (
        predecessor,
        (successor, authored.row_version),
        (copy_id, copied.row_version),
    )
}

#[tokio::test]
async fn the_cutover_flips_the_predecessor_and_publishes_both_new_rows() {
    let (repo, provider) = harness().await;
    let scope = AccessScope::for_tenant(tenant());
    let (predecessor, successor, copy) = Box::pin(seeded_cutover(
        &repo,
        &provider,
        &scope,
        Some("cloudlets"),
        at(20),
    ))
    .await;

    cutover_rows(&provider, &scope, predecessor, successor, copy, at(20))
        .await
        .expect("the cutover's four row moves commit together");

    let state = async |id: Uuid| {
        repo.find(&scope, tenant(), id)
            .await
            .expect("read")
            .expect("present")
            .lifecycle_state
    };
    assert_eq!(state(predecessor).await, LifecycleState::Superseded);
    assert_eq!(state(successor.0).await, LifecycleState::Published);
    assert_eq!(
        state(copy.0).await,
        LifecycleState::Published,
        "the copy publishes in the same transaction, on its own generation key"
    );
}

/// **The middle arm**: a copy on the predecessor's market whose eligibility is
/// not `existing_grandfathered`.
///
/// `refuse_ungenerational` has three arms and this suite covered the outer two —
/// the market comparison and the generation comparison — and not this one, whose
/// message string appeared nowhere in the crate's tests. Deleting it would let a
/// non-grandfathered row publish onto the predecessor's key and collide with the
/// successor on the published-plane UNIQUE, degrading a typed refusal into a
/// driver 500.
///
/// The instrument is the **successor itself passed as the copy**, which is what a
/// caller that transposed the two arguments produces: it is on the predecessor's
/// market by construction (that is what makes it a successor), so the market arm
/// passes and the eligibility arm is the only one that can answer.
#[tokio::test]
async fn a_copy_that_is_not_the_grandfathered_class_is_refused() {
    let (repo, provider) = harness().await;
    let scope = AccessScope::for_tenant(tenant());
    let (predecessor, successor, _) = Box::pin(seeded_cutover(
        &repo,
        &provider,
        &scope,
        Some("cloudlets"),
        at(20),
    ))
    .await;

    let err = cutover_rows(&provider, &scope, predecessor, successor, successor, at(20))
        .await
        .expect_err("an all_subscriptions draft is not a grandfathered generation");
    let RepoError::NotSupersedable { id, state, .. } = err else {
        panic!("the refusal must be the row plane's own NotSupersedable, got: {err:?}");
    };
    assert_eq!(
        id,
        successor.0.to_string(),
        "and it names the offending row"
    );
    assert!(
        state.contains("all_subscriptions") && state.contains("existing_grandfathered"),
        "the state must name the eligibility class it found and the one it wanted, so an \
         operator can see which of the three arms answered: {state}"
    );
    // The predecessor is untouched: the refusal is ahead of `supersede_row`.
    assert_eq!(
        repo.find(&scope, tenant(), predecessor)
            .await
            .expect("read")
            .expect("present")
            .lifecycle_state,
        LifecycleState::Published,
        "nothing was flipped before the refusal"
    );
}

#[tokio::test]
async fn a_copy_that_is_not_a_generation_of_the_predecessors_market_is_refused() {
    // The looser of the two pairings, and the one that has to be written down:
    // the copy is compared modulo `priceEligibility` and `cohort`, so every other
    // axis — the usage line included, since D-196 — has to be the predecessor's.
    let (repo, provider) = harness().await;
    let scope = AccessScope::for_tenant(tenant());
    let (predecessor, successor, _) = Box::pin(seeded_cutover(
        &repo,
        &provider,
        &scope,
        Some("cloudlets"),
        at(20),
    ))
    .await;

    let stranger = Uuid::from_u128(0xc0_04);
    let other_line = bss_pricing::domain::cutover::grandfathered_copy_key(
        &usage_key(Some("egress_gb"), ""),
        at(20),
        &[],
    )
    .expect("a generation of another line");
    let authored = repo
        .create_draft(
            &scope,
            tenant(),
            draft(
                stranger,
                other_line,
                usage_line_content(Some("egress_gb"), ""),
            ),
        )
        .await
        .expect("author a generation of a different meter");

    let err = cutover_rows(
        &provider,
        &scope,
        predecessor,
        successor,
        (stranger, authored.row_version),
        at(20),
    )
    .await
    .expect_err("a generation of another line is not this cutover's copy");

    let message = format!("{err:?}");
    assert!(
        message.contains("not a grandfathered generation"),
        "the refusal names what the copy failed to be: {message}"
    );
}

#[tokio::test]
async fn a_successor_on_another_line_of_one_market_is_refused() {
    // **This is the case that arms `refuse_mispaired`'s widening from eight key
    // columns to ten.** A case that varies only the *copy* leaves the guard driven
    // by successors on the predecessor's own key, so reverting `scope_key_columns`
    // to eight reddens nothing — which is the whole failure mode a fix's test exists
    // to rule out. This case is the one that bites: a successor on a **different meter of
    // the same market**, naming the predecessor, so the key comparison is the only
    // thing left that can refuse it.
    let (repo, provider) = harness().await;
    let scope = AccessScope::for_tenant(tenant());
    let (predecessor, _, copy) = Box::pin(seeded_cutover(
        &repo,
        &provider,
        &scope,
        Some("cloudlets"),
        at(20),
    ))
    .await;

    let other_line = Uuid::from_u128(0xc0_05);
    let mut content = usage_line_content(Some("egress_gb"), "");
    content.supersedes_price_id = Some(predecessor);
    let authored = repo
        .create_draft(
            &scope,
            tenant(),
            draft(other_line, usage_key(Some("egress_gb"), ""), content),
        )
        .await
        .expect("author a successor on another line of the same market");

    let err = cutover_rows(
        &provider,
        &scope,
        predecessor,
        (other_line, authored.row_version),
        copy,
        at(20),
    )
    .await
    .expect_err("a row on another line is not this key's successor");

    let message = format!("{err:?}");
    assert!(
        message.contains("on a different canonical scope key"),
        "the ten-column comparison is what refuses, not the supersedes link: {message}"
    );
}

#[tokio::test]
async fn a_copy_on_an_earlier_generation_of_the_same_market_is_refused() {
    // The copy is a generation of the right market and carries the right class, and
    // it is still not **this** cutover's copy: its cohort names another instant, so
    // it is a previous cutover's immutable retained row. Publishing it would republish
    // a generation nobody composed, and `inst-co-copy` mints exactly one generation
    // per act — keyed by the act's own instant.
    let (repo, provider) = harness().await;
    let scope = AccessScope::for_tenant(tenant());
    let (predecessor, successor, _) = Box::pin(seeded_cutover(
        &repo,
        &provider,
        &scope,
        Some("cloudlets"),
        at(20),
    ))
    .await;

    let earlier = Uuid::from_u128(0xc0_06);
    let earlier_key = bss_pricing::domain::cutover::grandfathered_copy_key(
        &usage_key(Some("cloudlets"), ""),
        at(19),
        &[],
    )
    .expect("a generation of the same market at another instant");
    let authored = repo
        .create_draft(
            &scope,
            tenant(),
            draft(
                earlier,
                earlier_key,
                usage_line_content(Some("cloudlets"), ""),
            ),
        )
        .await
        .expect("author an earlier generation of the same market");

    let err = cutover_rows(
        &provider,
        &scope,
        predecessor,
        successor,
        (earlier, authored.row_version),
        at(20),
    )
    .await
    .expect_err("a generation on another instant is another cutover's copy");

    let message = format!("{err:?}");
    assert!(
        message.contains("generation"),
        "the refusal names the axis that is wrong: {message}"
    );
}

#[tokio::test]
async fn the_cross_plane_commit_moves_three_windows_and_three_rows_together() {
    // `inst-gc-commit`: the predecessor's window shortens to the cutover, the
    // successor's and the copy's open there, and the three row moves ride the same
    // transaction. Every instant comes from the one `ComposedCutover`, so the
    // handover is gap-free by construction rather than by a later check.
    let (repo, provider) = harness().await;
    let scope = AccessScope::for_tenant(tenant());
    // Inside the fixture coverage window `[2099-08-04, 2099-09-01)`, because this
    // case commits against a real window plane. The copy is seeded on **this**
    // instant's generation: the row-plane cases can use any instant, but here the
    // window the shorten moves and the cohort the copy carries are two halves of
    // one act and cannot be built from two different clocks.
    let cutover_at = common::coverage_from() + time::Duration::days(3);
    let (predecessor, successor, copy) = Box::pin(seeded_cutover(
        &repo,
        &provider,
        &scope,
        Some("cloudlets"),
        cutover_at,
    ))
    .await;

    let conn = provider.conn().expect("conn");
    let covering =
        common::schedule_coverage_window(&conn, &scope, tenant(), predecessor, stamp()).await;
    let plane = vec![bss_pricing::domain::supersession::NamedWindow {
        window_id: covering.window_id,
        interval: bss_pricing::domain::window::WindowInterval::new(
            covering.effective_from,
            covering.effective_to,
            covering.state,
        ),
    }];
    let composed = bss_pricing::domain::cutover::compose_cutover_windows(&plane, cutover_at)
        .expect("a live key composes");

    let plan = bss_pricing::infra::cutover::CutoverCommit::of_composition(
        composed,
        plan(),
        predecessor,
        covering.mutation_seq,
        successor,
        Uuid::from_u128(0xc0_10),
        copy,
        Uuid::from_u128(0xc0_11),
        "grandfatheringCutover".to_owned(),
    );

    let scope_for_txn = scope.clone();
    let (_, outcome) = provider
        .db()
        .in_transaction::<_, RepoError, _>(move |txn| {
            Box::pin(async move {
                bss_pricing::infra::cutover::commit_cutover(
                    txn,
                    &scope_for_txn,
                    tenant(),
                    plan,
                    stamp(),
                )
                .await
            })
        })
        .await;
    let written = outcome
        .map_err(|err| err.into_domain(|infra| RepoError::Db(format!("cutover: {infra}"))))
        .expect("the cutover commits across both planes");

    assert_eq!(written.shortened.effective_to, Some(cutover_at));
    assert_eq!(written.successor_window.effective_from, cutover_at);
    assert_eq!(
        written.copy_window.effective_from, cutover_at,
        "both arrivals open at the instant the predecessor's coverage ends"
    );
    assert_eq!(
        written.copy_window.effective_to, None,
        "the copy's window is open-ended, which is what makes the D-04 bound hold"
    );

    let state = async |id: Uuid| {
        repo.find(&scope, tenant(), id)
            .await
            .expect("read")
            .expect("present")
            .lifecycle_state
    };
    assert_eq!(state(predecessor).await, LifecycleState::Superseded);
    assert_eq!(state(successor.0).await, LifecycleState::Published);
    assert_eq!(state(copy.0).await, LifecycleState::Published);
}

// ---------------------------------------------------------------------------
// D-246 — the catalog-wide GA backlog.
// ---------------------------------------------------------------------------

/// A publishable market key on one region, everything else held constant.
fn market_key(region: &str) -> ScopeKey {
    market_key_in("USD", region)
}

/// The same, on a stated currency.
///
/// `gated_markets` dedups on `(tenant_id, currency, region)` and every fixture in
/// this file hard-codes `USD`, so the currency axis of that key was never varied:
/// dropping it from the tuple collapsed two currencies of one region into one
/// market with the suite green.
fn market_key_in(currency: &str, region: &str) -> ScopeKey {
    ScopeKey::new(
        plan(),
        CurrencyCode::new(currency).expect("three letters"),
        Region::new(region).expect("a non-blank region"),
        PhaseId::new(Uuid::from_u128(0xfa_5e)),
        PriceEligibility::AllSubscriptions,
        ChargeKind::Recurring,
        Cohort::None,
    )
    .expect("all_subscriptions pairs with cohort none")
}

/// A grandfathered generation's key **on a market of its own**.
///
/// The market matters and the first version of the case got it wrong: seeding
/// the frozen generation on `EU`, which two live rows already gate, made the
/// exclusion unobservable — removing it from the query changed no count, so the
/// clause was asserted by a fixture that could not reach the state it claimed to
/// cover. Found by a probe that reddened **nothing**.
fn grandfathered_market_key(region: &str, cutover: OffsetDateTime) -> ScopeKey {
    ScopeKey::new(
        plan(),
        CurrencyCode::new("USD").expect("three letters"),
        Region::new(region).expect("a non-blank region"),
        PhaseId::new(Uuid::from_u128(0xfa_5e)),
        PriceEligibility::ExistingGrandfathered,
        ChargeKind::Recurring,
        Cohort::Generation(cutover),
    )
    .expect("existing_grandfathered pairs with a generation")
}

/// The flat recurring shape, priced **tax-inclusive** — the gated one.
fn tax_inclusive_flat() -> PriceContent {
    let mut content = flat_content();
    content.tax_inclusive = true;
    content
}

/// **`included_allowance` is the one content column with no shape constraint, so
/// a malformed document is a state the store genuinely admits.**
///
/// Every token column's `CorruptRow` arm is justified by a `CHECK` that
/// `sqlite_price_checks` pins; this column is declared bare (`included_allowance
/// jsonb` / `text`), so nothing at the schema refuses a document
/// `read_allowance` cannot read. Its three exits -- a non-object, a missing or
/// non-integer `quantity`, a missing `rolloverPolicy` -- had no case anywhere,
/// and this file carried no `CorruptRow` assertion at all: every read of such a
/// row answers `Internal` forever with nothing naming why.
#[tokio::test]
async fn a_malformed_included_allowance_is_a_corrupt_row_and_not_an_internal_error() {
    let (repo, provider) = harness().await;
    let scope = AccessScope::for_tenant(tenant());
    let price_id = Uuid::from_u128(0xd3_01);

    let mut content = flat_content();
    content.row.included_allowance = Some(IncludedAllowance {
        quantity: 50,
        rollover_policy: RolloverPolicy::Carry,
    });
    repo.create_draft(&scope, tenant(), draft(price_id, market_key("EU"), content))
        .await
        .expect("create");

    // **The positive control first**: the well-formed document reads back, so an
    // absence below is the malformation and not a read that was broken all along.
    assert!(
        repo.find(&scope, tenant(), price_id)
            .await
            .expect("a well-formed allowance reads")
            .is_some()
    );

    // Each exit, one at a time, written straight at the column so nothing in the
    // repository's own write path can normalise it away.
    for poison in [
        serde_json::json!("carry"),
        serde_json::json!({ "rolloverPolicy": "carry" }),
        serde_json::json!({ "quantity": -1, "rolloverPolicy": "carry" }),
        serde_json::json!({ "quantity": 50 }),
    ] {
        let conn = provider.conn().expect("conn");
        price::Entity::update_many()
            .secure()
            .scope_with(&scope)
            .col_expr(
                price::Column::IncludedAllowance,
                Expr::value(poison.clone()),
            )
            .filter(Condition::all().add(price::Column::PriceId.eq(price_id)))
            .exec(&conn)
            .await
            .expect("write the malformed document");

        let err = repo
            .find(&scope, tenant(), price_id)
            .await
            .expect_err("a document `read_allowance` cannot read is a corrupt row");
        let RepoError::CorruptRow(detail) = &err else {
            panic!("a malformed allowance must be CorruptRow, not {err:?}: {poison}");
        };
        assert!(
            detail.contains("pricing_price.included_allowance"),
            "the refusal must name the column an operator has to repair: {detail}"
        );
    }
}

/// The backlog counts **markets**, deduplicated, and excludes everything a
/// re-publish could never clear (D-246).
///
/// D-246/D-250: the refresher publishes the catalog-wide count to the gauge.
///
/// It lives here rather than beside the job because the seeding does: every row is
/// written through `create_draft` and flipped, so the catalog the job reads is one
/// the gear could actually produce. The job's own module tests what it owns without
/// a catalog — that a failed read publishes nothing, and that the tick is the
/// configured one — and deliberately builds no `ActiveModel` by hand.
#[tokio::test]
async fn the_refresher_publishes_the_catalog_wide_gated_market_count() {
    use bss_pricing::config::JobsConfig;
    use bss_pricing::domain::ports::metrics::{
        CurrencyBindingCase, PreviewFailClosed, PricingAlarm, PricingMetricsPort,
    };
    use bss_pricing::infra::jobs::gated_markets::GatedMarketsJob;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicI64, Ordering};

    #[derive(Default)]
    struct Recording(AtomicI64);
    impl PricingMetricsPort for Recording {
        fn preview_failclosed(&self, _reason: PreviewFailClosed) {}
        fn currency_binding_block(&self, _case: CurrencyBindingCase) {}
        fn tax_not_sellable_ga(&self, count: i64) {
            self.0.store(count, Ordering::Relaxed);
        }
        fn alarm(&self, _alarm: PricingAlarm) {}
    }

    let (repo, provider) = harness().await;
    let scope = AccessScope::for_tenant(tenant());
    let metrics = Arc::new(Recording::default());
    let job = GatedMarketsJob::new(
        provider.clone(),
        Arc::clone(&metrics) as Arc<dyn PricingMetricsPort>,
        JobsConfig::default(),
    );

    // Two published tax-inclusive rows on one market, and a third on another: the
    // count dedups, so the gauge must read 2 rather than 3.
    for (n, key) in [
        (0xd3_01_u128, market_key("EU")),
        (0xd3_02, new_subscriptions_key(ChargeKind::Recurring)),
        (0xd3_03, market_key("US")),
    ] {
        let price_id = Uuid::from_u128(n);
        repo.create_draft(&scope, tenant(), draft(price_id, key, tax_inclusive_flat()))
            .await
            .expect("create");
        flip_state(&provider, &scope, price_id, LifecycleState::Published).await;
    }

    let report = job.run_once().await.expect("the pass reads");

    assert_eq!(report.gated_markets, 2, "three rows, two markets");
    assert_eq!(
        metrics.0.load(Ordering::Relaxed),
        2,
        "and the gauge carries what the pass read, not a per-plan contribution"
    );
}

/// Every exclusion here is a separate seeded row rather than a clause in a
/// comment, because the count is a single number and a wrong one is
/// indistinguishable from a right one without saying which rows it is made of.
#[tokio::test]
async fn the_gated_market_count_dedups_markets_and_excludes_what_cannot_be_cleared() {
    use bss_pricing::infra::storage::repo::price_repo;

    let (repo, provider) = harness().await;
    let scope = AccessScope::for_tenant(tenant());
    let all = AccessScope::allow_all();
    let conn = provider.conn().expect("conn");

    // A real zero before anything is published — the control without which every
    // assertion below would also pass against a count that always answered 0.
    assert_eq!(
        price_repo::gated_markets(&conn, &all, false)
            .await
            .expect("count an empty catalog"),
        0
    );

    // Two published tax-inclusive rows on **one** market: `all_subscriptions`
    // and `new_subscriptions_only` are different keys and the same market.
    for (n, key) in [
        (0xd2_01_u128, market_key("EU")),
        (0xd2_02, new_subscriptions_key(ChargeKind::Recurring)),
    ] {
        let price_id = Uuid::from_u128(n);
        repo.create_draft(&scope, tenant(), draft(price_id, key, tax_inclusive_flat()))
            .await
            .expect("create");
        flip_state(&provider, &scope, price_id, LifecycleState::Published).await;
    }
    assert_eq!(
        price_repo::gated_markets(&conn, &all, false)
            .await
            .expect("count"),
        1,
        "two rows on one market are one market"
    );

    // A second market.
    let second = Uuid::from_u128(0xd2_03);
    repo.create_draft(
        &scope,
        tenant(),
        draft(second, market_key("US"), tax_inclusive_flat()),
    )
    .await
    .expect("create");
    flip_state(&provider, &scope, second, LifecycleState::Published).await;
    assert_eq!(
        price_repo::gated_markets(&conn, &all, false)
            .await
            .expect("count"),
        2
    );

    // Three rows that must **not** count, seeded one at a time so a regression
    // names which exclusion broke.
    //
    // A grandfathered generation: immutable, MUST NOT be superseded, so a market
    // reached only through one can never be un-gated by re-publishing (ADR-0002).
    let frozen = Uuid::from_u128(0xd2_04);
    repo.create_draft(
        &scope,
        tenant(),
        draft(
            frozen,
            grandfathered_market_key("GF", at(9)),
            tax_inclusive_flat(),
        ),
    )
    .await
    .expect("create");
    flip_state(&provider, &scope, frozen, LifecycleState::Published).await;
    assert_eq!(
        price_repo::gated_markets(&conn, &all, false)
            .await
            .expect("count"),
        2,
        "a grandfathered generation is in no backlog any action can clear"
    );

    // Tax-**exclusive**: sellable today, so not gated at all.
    let exclusive = Uuid::from_u128(0xd2_05);
    repo.create_draft(
        &scope,
        tenant(),
        draft(exclusive, market_key("AP"), flat_content()),
    )
    .await
    .expect("create");
    flip_state(&provider, &scope, exclusive, LifecycleState::Published).await;
    assert_eq!(
        price_repo::gated_markets(&conn, &all, false)
            .await
            .expect("count"),
        2,
        "the gate is on the tax basis, and this row does not carry it"
    );

    // A draft: nothing is published on that market, so nothing is gated on it.
    let unpublished = Uuid::from_u128(0xd2_06);
    repo.create_draft(
        &scope,
        tenant(),
        draft(unpublished, market_key("LA"), tax_inclusive_flat()),
    )
    .await
    .expect("create");
    assert_eq!(
        price_repo::gated_markets(&conn, &all, false)
            .await
            .expect("count"),
        2,
        "a draft has published nothing and gates nothing"
    );

    // **The two axes of the dedup key nothing in the crate varied.** `gated_markets`
    // folds into a `BTreeSet<(tenant_id, currency, region)>`; every row above is one
    // tenant on `USD`, so dropping either of those two members from the tuple left
    // this case, `the_refresher_publishes_the_catalog_wide_gated_market_count` and
    // `sqlite_gated_markets_shape` all green while two tenants gated on the same
    // market counted once between them.
    //
    // A second tenant, on the market `EU` is already counted for the first.
    let other_tenant = Uuid::from_u128(0x7e_12);
    let other_scope = AccessScope::for_tenant(other_tenant);
    let elsewhere = Uuid::from_u128(0xd2_07);
    repo.create_draft(
        &other_scope,
        other_tenant,
        draft(elsewhere, market_key("EU"), tax_inclusive_flat()),
    )
    .await
    .expect("create");
    flip_state(
        &provider,
        &other_scope,
        elsewhere,
        LifecycleState::Published,
    )
    .await;
    assert_eq!(
        price_repo::gated_markets(&conn, &all, false)
            .await
            .expect("count"),
        3,
        "two tenants gated on one market are two gated markets, not one"
    );

    // And a second currency inside a region the first tenant already gates.
    let other_currency = Uuid::from_u128(0xd2_08);
    repo.create_draft(
        &scope,
        tenant(),
        draft(
            other_currency,
            market_key_in("EUR", "EU"),
            tax_inclusive_flat(),
        ),
    )
    .await
    .expect("create");
    flip_state(&provider, &scope, other_currency, LifecycleState::Published).await;
    assert_eq!(
        price_repo::gated_markets(&conn, &all, false)
            .await
            .expect("count"),
        4,
        "one region priced in two currencies is two markets"
    );
}
/// **The gauge answers zero once the tax engine is GA** — the site a
/// `TAX_ENGINE_GA` grep did not reach.
///
/// `is_not_sellable_ga(row, ga)` is `row.tax_inclusive && !ga`, so on the day the
/// constant flips a gated market stops existing. `metrics.rs`'s alarm reads the
/// constant and would correctly go quiet; this read *reasoned about* it in prose
/// and hard-coded the predicate, so it would have kept publishing the full count
/// of published tax-inclusive markets forever — §7's backlog series pinned at a
/// number no action can clear.
///
/// Driven by flipping the parameter rather than the constant, because a constant
/// cannot be flipped from a test: that is exactly why it had to become one.
#[tokio::test]
async fn the_gated_market_gauge_is_empty_once_the_tax_engine_is_ga() {
    use bss_pricing::infra::storage::repo::price_repo;

    let (repo, provider) = harness().await;
    let scope = AccessScope::for_tenant(tenant());
    let all = AccessScope::allow_all();
    let conn = provider.conn().expect("conn");

    let price_id = Uuid::from_u128(0xd2_9a);
    repo.create_draft(
        &scope,
        tenant(),
        draft(price_id, market_key("EU"), tax_inclusive_flat()),
    )
    .await
    .expect("create");
    flip_state(&provider, &scope, price_id, LifecycleState::Published).await;

    // The control: before GA this market is gated, so the case is not asserting
    // an emptiness the fixture would have produced anyway.
    assert_eq!(
        price_repo::gated_markets(&conn, &all, false)
            .await
            .expect("count"),
        1
    );

    assert_eq!(
        price_repo::gated_markets(&conn, &all, true)
            .await
            .expect("count"),
        0,
        "a gated market is a market whose rows are not sellable *because the engine \
         is absent*; once it ships there is nothing to report"
    );
}

/// A reserved rate below one minor unit survives the round trip (D-311, Z5-3).
///
/// `reservedRate` is a **rate**: PRD §2674 calls it a committed *unit price* and
/// the `capacity` flavor accrues it **per covered granule**, which is D-311's own
/// definition of one. It was left behind when that decision moved
/// `TierBand::unit_price_rate` and `PriceRow::unit_rate` to `RateMinor`, because
/// D-311's census enumerated references to `unit_price_minor` and this field is
/// not one of them.
///
/// Typed as whole minor units the smallest expressible non-zero value is one
/// cent, so a reserved capacity billed per second — `max_hold_granules`' own doc
/// names `per_second` as the granularity that motivated widening that column —
/// cannot be authored at all: `$0.0000166667` per GB-second is `0.00166667`
/// minor units, and the author must submit `0` or `1`, the latter being 600x the
/// intended rate. Not truncated: unrepresentable.
///
/// The value below is deliberately **not** a whole number of minor units, for
/// [`nano_rate`]'s stated reason — a value that divides evenly by the scale
/// factor still reads plausibly at the wrong scale, which is the shape that
/// nearly defeated D-311's own fix.
#[tokio::test]
async fn a_reserved_rate_below_one_minor_unit_reads_back_exactly() {
    let (repo, _provider) = harness().await;
    let scope = AccessScope::for_tenant(tenant());
    let price_id = Uuid::from_u128(0xb_11);
    let key = grandfathered_key(ChargeKind::Usage, at(9));

    let authored = nano_rate(1_666_670);
    let mut content = graduated_content();
    content.row.reserved_rate = Some(authored);
    content.row.reservation_flavor = Some(ReservationFlavor::Capacity);

    repo.create_draft(&scope, tenant(), draft(price_id, key.clone(), content))
        .await
        .expect("create the draft row");

    let read = repo
        .find(&scope, tenant(), price_id)
        .await
        .expect("read")
        .expect("the row just created is there");

    assert_eq!(
        read.row.reserved_rate,
        Some(authored),
        "a reserved rate is a rate and survives at the stored 10^-9 scale"
    );
}

/// **Publish freezes the tenant's rounding default beside a row that carries
/// none** (review M-2, corrected by review C-3).
///
/// `RoundingPolicyResolved` returns at its first line the moment a tenant default
/// exists, and its doc says *"the resolved id then freezes into the read model and
/// the snapshot"* — which nothing performed until 2026-08-19. Both renderers read
/// the raw nullable column, so a tenant with a default and rows carrying none
/// froze `"roundingPolicyRef": null` on every one of them.
///
/// **The second assertion is the reason freezing exists at all.** Answering "the
/// consumer can look the default up" is what makes the first cost survivable and
/// the second one not: with the value unfrozen, a tenant that later changes the
/// default silently re-rounds every already-frozen version. So this pins that a
/// *subsequent* publish under a different default leaves the first row alone —
/// a probe asserting only that the column is non-null would pass against a
/// look-up-at-read implementation, which is the defect.
///
/// **This case asserted the wrong column until 2026-08-19.** The freeze landed on
/// `rounding_policy_ref` — the column the *author* sets — so a row that
/// deliberately carried none came back naming a value, the tenant default stopped
/// reaching it, and `authored_content` carried the resolution into every
/// successor draft as if a person had typed it. The freeze is right and its
/// target was not; `resolved_rounding_policy` is where it goes, beside
/// `resolved_tax_category`, which never had this shape.
#[tokio::test]
async fn publish_freezes_the_tenant_rounding_default_onto_a_row_that_carries_none() {
    let (repo, provider) = harness().await;
    let scope = AccessScope::for_tenant(tenant());
    let first = Uuid::from_u128(0xb_0d01);

    let mut content = flat_content();
    content.rounding_policy_ref = None;
    repo.create_draft(
        &scope,
        tenant(),
        draft(first, base_key(ChargeKind::Recurring), content),
    )
    .await
    .expect("author a row with no rounding policy of its own");

    publish_rows_with_default(
        &provider,
        &scope,
        tenant(),
        plan(),
        vec![(first, RowVersion::new(0))],
        &readiness_for(
            base_key(ChargeKind::Recurring).region().as_str(),
            Some("standard"),
        ),
        "half_up/2",
    )
    .await
    .expect("publish");

    let conn = provider.conn().expect("conn");
    let frozen = bss_pricing::infra::storage::repo::price_repo::frozen_resolutions(
        &conn,
        &scope,
        tenant(),
        plan(),
    )
    .await
    .expect("read the frozen resolutions");
    assert_eq!(
        frozen
            .get(&first)
            .and_then(|r| r.rounding_policy.as_deref()),
        Some("half_up/2"),
        "the resolved policy has to be frozen onto the row, not left for a consumer to look up"
    );

    let stored = repo
        .find(&scope, tenant(), first)
        .await
        .expect("the read succeeds")
        .expect("the row is there");
    assert_eq!(
        stored.rounding_policy_ref, None,
        "and the authored column stays as the author left it: this row carries no policy of \
         its own, and a successor draft cloned from it must inherit that absence, not the \
         resolution"
    );

    // The tenant changes its default. The already-published row must not move.
    let second = Uuid::from_u128(0xb_0d02);
    let mut other = flat_content();
    other.rounding_policy_ref = None;
    repo.create_draft(
        &scope,
        tenant(),
        draft(second, base_key(ChargeKind::OneTime), other),
    )
    .await
    .expect("author a second row");
    publish_rows_with_default(
        &provider,
        &scope,
        tenant(),
        plan(),
        vec![(second, RowVersion::new(0))],
        &readiness_for(
            base_key(ChargeKind::OneTime).region().as_str(),
            Some("standard"),
        ),
        "half_even/2",
    )
    .await
    .expect("publish under the new default");

    let conn = provider.conn().expect("conn");
    let after = bss_pricing::infra::storage::repo::price_repo::frozen_resolutions(
        &conn,
        &scope,
        tenant(),
        plan(),
    )
    .await
    .expect("read the frozen resolutions");
    assert_eq!(
        after.get(&first).and_then(|r| r.rounding_policy.as_deref()),
        Some("half_up/2"),
        "a frozen version does not re-round when the tenant default changes; that is what \
         freezing is for"
    );
    assert_eq!(
        after
            .get(&second)
            .and_then(|r| r.rounding_policy.as_deref()),
        Some("half_even/2"),
        "and the row published under the new default carries the new one"
    );
}

/// A set that resolves **no** rounding policy is refused at the freeze, not
/// frozen as `NULL` (review F1, 2026-08-19).
///
/// `foundation.rounding_policy_resolved` says the same thing and runs on one of
/// `publish_rows`' four callers. `pricing_price`'s migration header states of this
/// column that `NULL` on a published row "cannot happen ... the publish rule
/// refuses the publish", and `trg_pricing_price_append_only` then makes the row immutable — so
/// the claim has to hold on every door that writes it or there is no repair
/// afterwards. This is the guarantee stated where it is actually enforceable.
#[tokio::test]
async fn a_publish_that_resolves_no_rounding_policy_is_refused_rather_than_frozen_as_null() {
    let (repo, provider) = harness().await;
    let scope = AccessScope::for_tenant(tenant());
    let price_id = Uuid::from_u128(0xb_0d11);

    let mut content = flat_content();
    content.rounding_policy_ref = None;
    repo.create_draft(
        &scope,
        tenant(),
        draft(price_id, base_key(ChargeKind::Recurring), content),
    )
    .await
    .expect("author a row with no rounding policy of its own");

    let err = publish_rows_resolving_nothing(
        &provider,
        &scope,
        tenant(),
        plan(),
        vec![(price_id, RowVersion::new(0))],
        &readiness_for(
            base_key(ChargeKind::Recurring).region().as_str(),
            Some("standard"),
        ),
    )
    .await
    .expect_err("a row resolving no rounding policy cannot publish");
    assert!(
        matches!(
            &err,
            RepoError::RoundingPolicyUnresolved { price_id: named }
                if named == &price_id.to_string()
        ),
        "the refusal names the row whose resolution is absent, which is the edit: {err}"
    );

    assert_eq!(
        stored_row(&provider, &scope, price_id)
            .await
            .lifecycle_state,
        LifecycleState::Draft.as_str(),
        "and nothing flipped: the refusal lands before any group's statement executes, or a \
         plan with two resolutions publishes half of itself and then refuses"
    );
}

/// The **positive control** for the case above, on the door F1 was actually
/// about: the same set publishes once a resolution exists.
///
/// Without this row the refusal above would pass against an implementation that
/// refused every publish.
#[tokio::test]
async fn the_same_set_publishes_once_the_tenant_default_resolves_it() {
    let (repo, provider) = harness().await;
    let scope = AccessScope::for_tenant(tenant());
    let price_id = Uuid::from_u128(0xb_0d12);

    let mut content = flat_content();
    content.rounding_policy_ref = None;
    repo.create_draft(
        &scope,
        tenant(),
        draft(price_id, base_key(ChargeKind::Recurring), content),
    )
    .await
    .expect("author a row with no rounding policy of its own");

    publish_rows_with_default(
        &provider,
        &scope,
        tenant(),
        plan(),
        vec![(price_id, RowVersion::new(0))],
        &readiness_for(
            base_key(ChargeKind::Recurring).region().as_str(),
            Some("standard"),
        ),
        "half_up/2",
    )
    .await
    .expect("the identical set publishes when the tenant has a default");
}

/// **`RoundingPolicyUnresolved`'s twin, on the guard ordered before it.**
///
/// The two sit adjacent in `publish_rows` and are refused by the same shape of
/// `find`. Between them they had 19 assertions and 0: `TaxCategoryUnresolved`
/// appeared nowhere in `pricing/tests/` and nowhere in `src/**/*_tests.rs`, and it
/// is the one that runs **first** — so a plan whose region declares no default
/// category reached the untested arm before it could reach the tested one.
///
/// Same stakes as its twin, for the same structural reason: `taxCategory` is a
/// pinned D-48 v1 descriptor element, `pricing_price.resolved_tax_category` is
/// written by the publish statement, and `trg_pricing_price_append_only` then makes
/// the row immutable. A `NULL` frozen there is unrepairable.
///
/// **The rounding default is supplied**, which is what makes this case about the
/// category: with both resolutions absent the tax arm fires anyway, and the case
/// would pass equally against a store that had lost the ordering. Here the only
/// thing wrong with the set is the category.
#[tokio::test]
async fn a_publish_that_resolves_no_tax_category_is_refused_rather_than_frozen_as_null() {
    let (repo, provider) = harness().await;
    let scope = AccessScope::for_tenant(tenant());
    let price_id = Uuid::from_u128(0xb_0d13);

    // `flat_content` states no `tax_category_ref` of its own, so the region's
    // default is the only resolution available.
    repo.create_draft(
        &scope,
        tenant(),
        draft(price_id, base_key(ChargeKind::Recurring), flat_content()),
    )
    .await
    .expect("author a row with no tax category of its own");

    let err = publish_rows_with_default(
        &provider,
        &scope,
        tenant(),
        plan(),
        vec![(price_id, RowVersion::new(0))],
        // The region declares none, which is the fault.
        &readiness_for(base_key(ChargeKind::Recurring).region().as_str(), None),
        "half_up/2",
    )
    .await
    .expect_err("a row resolving no tax category cannot publish");
    assert!(
        matches!(
            &err,
            RepoError::TaxCategoryUnresolved { price_id: named }
                if named == &price_id.to_string()
        ),
        "the refusal names the row whose resolution is absent, and is the category's rather \
         than the rounding policy's: {err}"
    );

    assert_eq!(
        stored_row(&provider, &scope, price_id)
            .await
            .lifecycle_state,
        LifecycleState::Draft.as_str(),
        "and nothing flipped: the refusal lands before any group's statement executes"
    );
}

/// The **positive control** for the case above: the same set, the same absent
/// `tax_category_ref`, and a region that declares a default.
///
/// Without it the refusal above would pass against a `publish_rows` that refused
/// every publish, and against one whose `find` predicate read the wrong element of
/// the key.
#[tokio::test]
async fn the_same_set_publishes_once_the_region_declares_a_category() {
    let (repo, provider) = harness().await;
    let scope = AccessScope::for_tenant(tenant());
    let price_id = Uuid::from_u128(0xb_0d14);

    repo.create_draft(
        &scope,
        tenant(),
        draft(price_id, base_key(ChargeKind::Recurring), flat_content()),
    )
    .await
    .expect("author a row with no tax category of its own");

    publish_rows_with_default(
        &provider,
        &scope,
        tenant(),
        plan(),
        vec![(price_id, RowVersion::new(0))],
        &readiness_for(
            base_key(ChargeKind::Recurring).region().as_str(),
            Some("standard"),
        ),
        "half_up/2",
    )
    .await
    .expect("the identical set publishes when the region declares a category");

    // And the resolution the publish statement froze is the region's, which is the
    // value the refusal above exists to stop being `NULL`.
    assert_eq!(
        stored_row(&provider, &scope, price_id)
            .await
            .resolved_tax_category
            .as_deref(),
        Some("standard"),
        "the publish freezes the resolution it judged the row with"
    );
}

/// The supersession door, which is where F1 actually bites: a price change on a
/// key whose rows lean on a tenant default that has since been cleared.
///
/// `infra::supersession` reads the default itself and passes it here, and no rule
/// on that path judges it — `plan_supersession` runs `price_row_rules()` and
/// `supersession_rules()`, neither of which holds a rounding rule. Before the
/// refusal below, this froze `NULL` onto a **published** successor and
/// `trg_pricing_price_append_only` makes it immutable.
#[tokio::test]
async fn a_supersession_whose_tenant_lost_its_default_is_refused_at_the_commit() {
    let (repo, provider) = harness().await;
    let scope = AccessScope::for_tenant(tenant());
    let (predecessor, successor) = composed_supersession(&repo, &provider, &scope).await;

    let err = commit_supersession_rows_resolving_nothing(
        &provider,
        &scope,
        tenant(),
        plan(),
        predecessor,
        (successor, RowVersion::new(0)),
    )
    .await
    .expect_err("the successor resolves no rounding policy, so the pair does not commit");
    assert!(
        matches!(&err, RepoError::RoundingPolicyUnresolved { .. }),
        "the supersession door reports the same fault the publish rule reports: {err}"
    );

    assert_eq!(
        stored_row(&provider, &scope, predecessor)
            .await
            .lifecycle_state,
        LifecycleState::Published.as_str(),
        "and the predecessor is still the key's current row - a refusal after the flip would \
         leave the key with no published row at all"
    );
}

// ---------------------------------------------------------------------------
// D-360: the list page's row facts, one grouped read for the whole page.
// ---------------------------------------------------------------------------

/// A key on `plan` in `currency`, on its own phase so several rows of one plan
/// and one currency do not collide on the scope key.
fn key_on(plan: PlanId, currency: &str, phase: u128, charge_kind: ChargeKind) -> ScopeKey {
    ScopeKey::new(
        plan,
        CurrencyCode::new(currency).expect("three letters"),
        Region::new("EU").expect("a non-blank region"),
        PhaseId::new(Uuid::from_u128(phase)),
        PriceEligibility::AllSubscriptions,
        charge_kind,
        Cohort::None,
    )
    .expect("all_subscriptions pairs with cohort none")
}

/// A recurring row that names **no** model kind - the "tiered (unspecified)"
/// row the Studio counts and does not list as a model.
fn kindless_content() -> PriceContent {
    let mut row = PriceRow::new(ChargeKind::Recurring, None);
    row.amount_minor = Some(money(500));
    PriceContent {
        row,
        ..flat_content()
    }
}

/// The three facts the list page carries per plan (D-360): the count is over
/// `draft` + `published` rows only, the two vocabularies are distinct and
/// sorted, a `NULL` model kind counts but names no kind, and a plan with no
/// authoring row has no entry at all.
///
/// One grouped read answers for every plan asked about, which is the point: the
/// Studio derived these by reading every plan's rows, one request each.
#[tokio::test]
async fn the_page_aggregate_counts_authoring_rows_and_lists_kinds_and_currencies_sorted() {
    let (repo, provider) = harness().await;
    let scope = AccessScope::for_tenant(tenant());
    let a = PlanId::new(Uuid::from_u128(0xa1));
    let b = PlanId::new(Uuid::from_u128(0xb1));
    let c = PlanId::new(Uuid::from_u128(0xc1));

    // Plan A: draft EUR flat, published USD graduated, published EUR flat,
    // superseded EUR flat.
    let a_rows = [
        (
            0xa_01_u128,
            "EUR",
            0xf_01_u128,
            flat_content(),
            LifecycleState::Draft,
        ),
        (
            0xa_02,
            "USD",
            0xf_02,
            graduated_content(),
            LifecycleState::Published,
        ),
        (
            0xa_03,
            "EUR",
            0xf_03,
            flat_content(),
            LifecycleState::Published,
        ),
        (
            0xa_04,
            "EUR",
            0xf_04,
            flat_content(),
            LifecycleState::Superseded,
        ),
    ];
    for (price, currency, phase, mut content, state) in a_rows {
        // The graduated fixture carries a grandfathering horizon, which an
        // `all_subscriptions` key refuses (`GrandfatherHorizonOffClass`); the
        // horizon is not what this case is about.
        content.grandfather_until = None;
        let charge_kind = content.row.charge_kind;
        let price_id = Uuid::from_u128(price);
        repo.create_draft(
            &scope,
            tenant(),
            draft(price_id, key_on(a, currency, phase, charge_kind), content),
        )
        .await
        .expect("author plan A's row");
        // `published` first: the append-only trigger sanctions `draft -> published`
        // and `published -> superseded`, never the jump.
        if state != LifecycleState::Draft {
            flip_state(&provider, &scope, price_id, LifecycleState::Published).await;
        }
        if state == LifecycleState::Superseded {
            flip_state(&provider, &scope, price_id, state).await;
        }
    }
    // Plan B: one published GBP row with no model kind.
    let b_price = Uuid::from_u128(0xb_01);
    repo.create_draft(
        &scope,
        tenant(),
        draft(
            b_price,
            key_on(b, "GBP", 0xf_11, ChargeKind::Recurring),
            kindless_content(),
        ),
    )
    .await
    .expect("author plan B's row");
    flip_state(&provider, &scope, b_price, LifecycleState::Published).await;
    // Plan C: nothing.

    let conn = provider.conn().expect("conn");
    let out = aggregate_authoring_rows_for_plans(&conn, &scope, tenant(), &[a, b, c])
        .await
        .expect("aggregate");

    let a_agg = &out[&a];
    assert_eq!(
        a_agg.price_row_count, 3,
        "superseded is not an authoring row: {a_agg:?}"
    );
    assert_eq!(a_agg.model_kinds, ["flat", "graduated"], "sorted, distinct");
    assert_eq!(a_agg.currencies, ["EUR", "USD"], "sorted, distinct");
    let b_agg = &out[&b];
    assert_eq!(b_agg.price_row_count, 1);
    assert!(
        b_agg.model_kinds.is_empty(),
        "a NULL model_kind is the absence of one, not a kind: {b_agg:?}"
    );
    assert_eq!(b_agg.currencies, ["GBP"]);
    assert!(
        !out.contains_key(&c),
        "a plan with no rows has no entry; the caller renders zero and empty"
    );
    assert!(
        aggregate_authoring_rows_for_plans(&conn, &scope, tenant(), &[])
            .await
            .expect("aggregate nothing")
            .is_empty(),
        "an empty page asks the store nothing"
    );
}
