//! The sellability surface over **stored** deltas: §4.4's resolution rule, and
//! the property that a frozen version never changes what it answers.
//!
//! None of this is provable without a database. `delta_at` is a statement about
//! which of several rows answers a pin — the greatest `catalog_version <= V` whose
//! `warm_completed` is set — and about a tenant boundary that only a compiled
//! scope evaluates. A unit test could assert the query was built; it could not
//! assert that a consumer pinned to yesterday's version still reads yesterday's
//! coverage, which is the whole of what a pinned read model is for.
//!
//! The payload reader itself is proved without a database, beside the reader, in
//! `src/infra/storage/repo/read_model_repo_tests.rs`: it is a pure function of one
//! row, and rendering a delta and reading it straight back is the sharper
//! statement.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use bss_pricing::domain::contracts::{EntitlementGrants, PlanChangeContract};
use bss_pricing::domain::instant::utc_ymd_hms;
use bss_pricing::domain::lifecycle::LifecycleState;
use bss_pricing::domain::money::CurrencyCode;
use bss_pricing::domain::plan_shape::Frequency;
use bss_pricing::domain::price_record::PriceRecord;
use bss_pricing::domain::price_row::{ModelKind, PriceRow};
use bss_pricing::domain::projection::PlanSubjectDelta;
use bss_pricing::domain::read_model::SubjectRef;
use bss_pricing::domain::scope_key::{
    ChargeKind, Cohort, PhaseId, PlanId, PriceEligibility, Region, ScopeKey,
};
use bss_pricing::domain::sellability::{
    PlanMarketVerdict, Predicate, PredicateAnswer, SellabilitySurface,
};
use bss_pricing::domain::window::{CoverageEnd, KeyWindows, WindowInterval, WindowState};
use bss_pricing::infra::storage::migrations::Migrator;
use bss_pricing::infra::storage::repo::read_model_repo::{self, NewDelta, StoredDelta};
use bss_pricing_sdk::CatalogVersion;
use sea_orm_migration::MigratorTrait;
use std::collections::BTreeMap;
use time::OffsetDateTime;
use toolkit_db::migration_runner::run_migrations_for_testing;
use toolkit_db::secure::AccessScope;
use toolkit_db::{ConnectOpts, DBProvider, DbError, connect_db};
use uuid::Uuid;

const TENANT: Uuid = Uuid::from_u128(0x7e_11);
const OTHER_TENANT: Uuid = Uuid::from_u128(0x7e_22);

async fn provider() -> DBProvider<DbError> {
    let db = connect_db("sqlite::memory:", ConnectOpts::default())
        .await
        .expect("connect in-memory sqlite");
    run_migrations_for_testing(&db, Migrator::migrations())
        .await
        .expect("run migrator");
    DBProvider::<DbError>::new(db)
}

/// `2099-01-01T00:00:00Z` plus `day` whole days. Fixed, and every comparison below
/// is against another instant from this function.
fn at(day: i64) -> OffsetDateTime {
    utc_ymd_hms(2099, 1, 1, 0, 0, 0) + time::Duration::days(day)
}

fn plan() -> PlanId {
    PlanId::new(Uuid::from_u128(0x5e_11))
}

fn recurring_key() -> ScopeKey {
    ScopeKey::new(
        plan(),
        CurrencyCode::new("EUR").expect("three letters"),
        Region::new("eu").expect("a non-blank region"),
        PhaseId::new(Uuid::from_u128(0xfa_5e)),
        PriceEligibility::AllSubscriptions,
        ChargeKind::Recurring,
        Cohort::None,
    )
    .expect("the class pairs with cohort none")
}

fn row() -> PriceRecord {
    let mut row = PriceRow::new(ChargeKind::Recurring, Some(ModelKind::Flat));
    row.amount_minor =
        Some(bss_pricing::domain::money::MinorAmount::new(1_200).expect("a non-negative amount"));
    PriceRecord {
        price_id: Uuid::from_u128(0xb_0001),
        scope_key: recurring_key(),
        row,
        tax_inclusive: false,
        tax_category_ref: None,
        billing_timing: None,
        proration_contract: None,
        rounding_policy_ref: None,
        grandfather_until: None,
        supersedes_price_id: None,
        lifecycle_state: LifecycleState::Published,
        created_by: Uuid::from_u128(0xac_10),
        created_at_utc: at(0),
        row_version: bss_pricing::domain::concurrency::RowVersion::new(0),
    }
}

/// A published, monthly plan whose one recurring key is covered over
/// `[at(0), coverage_to)` — or open-ended when `coverage_to` is `None`.
fn delta_covering(coverage_to: Option<OffsetDateTime>) -> PlanSubjectDelta {
    delta_covering_for(plan(), coverage_to)
}

/// [`delta_covering`] for a named plan.
///
/// The plan id is a parameter because it is *in the payload*: a fixture that
/// hardcoded it made two subjects' stored documents carry the same `planId`, and
/// an assertion reading it back could not tell them apart. It also made the
/// fixture a state production cannot produce — a payload whose `planId` disagrees
/// with its `subject_ref`.
fn delta_covering_for(plan_id: PlanId, coverage_to: Option<OffsetDateTime>) -> PlanSubjectDelta {
    PlanSubjectDelta {
        plan_id,
        revision: 1,
        // Empty: no predicate this fixture drives reads a derived meter, and a
        // populated set here would be a seed no assertion reads back.
        composites: Vec::new(),
        lifecycle_state: LifecycleState::Published,
        sku_id: None,
        plan_tier: Some("gold".to_owned()),
        plan_tier_override: false,
        billing_cycle: None,
        frequency: Some(Frequency::Monthly),
        available_from: Some(at(0)),
        available_to: None,
        purchase_min_qty: None,
        purchase_max_qty: None,
        invoice_grouping_key: None,
        phases: Vec::new(),
        addon_rules: Vec::new(),
        descriptor_set: None,
        period_floor_caps: Vec::new(),
        entitlement_grants: EntitlementGrants::default(),
        change_contract: PlanChangeContract::default(),
        prices: vec![row()],
        tax_projection: BTreeMap::new(),
        windows: vec![KeyWindows {
            scope_key: recurring_key(),
            intervals: vec![WindowInterval::new(at(0), coverage_to, WindowState::Active)],
        }],
    }
}

/// Project one plan-subject delta at `version`, warm.
async fn project(
    provider: &DBProvider<DbError>,
    tenant: Uuid,
    version: u64,
    projected_at: OffsetDateTime,
    delta: &PlanSubjectDelta,
) {
    project_subject_ref(
        provider,
        tenant,
        version,
        projected_at,
        SubjectRef::Plan(plan().get()),
        delta,
    )
    .await;
}

/// The same, under a subject the caller names — the form the sibling-plan case
/// needs.
async fn project_subject_ref(
    provider: &DBProvider<DbError>,
    tenant: Uuid,
    version: u64,
    projected_at: OffsetDateTime,
    subject: SubjectRef,
    delta: &PlanSubjectDelta,
) {
    let conn = provider.conn().expect("conn");
    read_model_repo::project_subject(
        &conn,
        &AccessScope::for_tenant(tenant),
        NewDelta {
            tenant_id: tenant,
            catalog_version: CatalogVersion::new(version),
            subject,
            payload: delta.to_value(),
            projected_at,
        },
    )
    .await
    .expect("project the subject");
}

async fn resolve(
    provider: &DBProvider<DbError>,
    tenant: Uuid,
    at_version: u64,
) -> Option<StoredDelta> {
    let conn = provider.conn().expect("conn");
    read_model_repo::delta_at(
        &conn,
        &AccessScope::for_tenant(tenant),
        tenant,
        &SubjectRef::Plan(plan().get()),
        CatalogVersion::new(at_version),
    )
    .await
    .expect("resolve the subject")
}

/// §4.4's rule, exactly: the **greatest** `catalog_version <= V`, not the newest
/// row and not an exact match.
///
/// An exact-version read would answer `None` for nearly every pin: a version is a
/// tenant-wide batch and a plan re-projects only when a publish unit touches it, so
/// the row that answers a pin at `V` is almost always stamped with some earlier
/// version.
#[tokio::test]
async fn a_pin_resolves_the_greatest_version_at_or_below_it() {
    let provider = provider().await;
    project(&provider, TENANT, 3, at(3), &delta_covering(Some(at(30)))).await;
    project(&provider, TENANT, 7, at(7), &delta_covering(Some(at(70)))).await;

    // Between the two: the earlier row answers, and the later one is invisible.
    let mid = resolve(&provider, TENANT, 5)
        .await
        .expect("version 3 is at or below 5");
    assert_eq!(mid.catalog_version, CatalogVersion::new(3));
    assert_eq!(mid.projected_at, at(3));

    // At and above the later one: the later row answers.
    for pin in [7, 9] {
        let found = resolve(&provider, TENANT, pin)
            .await
            .expect("version 7 is at or below both");
        assert_eq!(found.catalog_version, CatalogVersion::new(7));
        assert_eq!(found.projected_at, at(7));
    }

    // Below every projection: nothing is addressable, which the surface answers
    // as predicate (2) failing rather than as a plan with no rows.
    assert_eq!(resolve(&provider, TENANT, 2).await, None);
}

/// One plan's pin resolves **that plan's** subject, and a sibling plan projected
/// into the same version is not it.
///
/// The subject is the row's identity, so a resolution that matched on the version
/// alone would hand a consumer another plan's coverage under the plan id it asked
/// about — a wrong answer rather than a missing one, and the worst shape a read
/// model has.
#[tokio::test]
async fn a_pin_resolves_the_plan_it_names_and_not_a_sibling_in_the_same_version() {
    let provider = provider().await;
    let sibling = PlanId::new(Uuid::from_u128(0x5e_99));
    project(&provider, TENANT, 3, at(3), &delta_covering(Some(at(30)))).await;
    project_subject_ref(
        &provider,
        TENANT,
        3,
        at(3),
        SubjectRef::Plan(sibling.get()),
        // **The sibling's own plan id in its own payload.** Built with `plan()`
        // until 2026-08-20, under which both stored payloads carried the same
        // `planId` and the assertion labelled "the payload is the plan's own, not
        // the sibling's" was satisfied by either row — only the `coverageEnd`
        // assertion below discriminated them.
        &delta_covering_for(sibling, None),
    )
    .await;

    let found = resolve(&provider, TENANT, 3)
        .await
        .expect("this plan's subject is projected at 3");
    assert_eq!(
        found.payload.get("planId"),
        Some(&serde_json::json!(plan().get())),
        "the payload is the plan's own"
    );
    assert_ne!(
        found.payload.get("planId"),
        Some(&serde_json::json!(sibling.get())),
        "and not the sibling's, which is projected into the same version"
    );
    assert_eq!(
        found.payload["windows"][0]["coverageEnd"]["kind"],
        serde_json::json!("ends"),
        "and it carries this plan's bounded coverage rather than the sibling's open-ended one"
    );
}

/// A subject nobody projected resolves to nothing — and so does one projected in
/// **another tenant**, which is SQL-level BOLA rather than a filter this reader
/// wrote.
#[tokio::test]
async fn an_unprojected_subject_and_a_foreign_tenants_subject_read_alike() {
    let provider = provider().await;
    project(&provider, OTHER_TENANT, 3, at(3), &delta_covering(None)).await;

    assert_eq!(
        resolve(&provider, TENANT, 9).await,
        None,
        "this tenant has projected nothing"
    );
    assert!(
        resolve(&provider, OTHER_TENANT, 9).await.is_some(),
        "and the row that exists is readable under its own scope, so the case above is the \
         boundary rather than an empty table"
    );
}

/// The `warm_completed` filter is not decoration: a row that is present and not
/// warm is a subject that is **not resolvable**, and handing its payload to a
/// consumer would advertise content the projector had not finished.
///
/// The row is written warm by this gear, so the state has to be staged directly —
/// which is the honest form of the test, since what is being asserted is the
/// query's predicate rather than a path through the writer.
#[tokio::test]
async fn a_row_that_is_not_warm_does_not_answer_a_pin() {
    use sea_orm::{ColumnTrait, Condition, EntityTrait};
    use toolkit_db::secure::SecureUpdateExt;

    let provider = provider().await;
    project(&provider, TENANT, 3, at(3), &delta_covering(Some(at(30)))).await;
    project(&provider, TENANT, 7, at(7), &delta_covering(Some(at(70)))).await;

    let conn = provider.conn().expect("conn");
    let affected = bss_pricing::infra::storage::entity::read_model::Entity::update_many()
        .secure()
        .scope_with(&AccessScope::for_tenant(TENANT))
        .col_expr(
            bss_pricing::infra::storage::entity::read_model::Column::WarmCompleted,
            sea_orm::sea_query::Expr::value(false),
        )
        .col_expr(
            bss_pricing::infra::storage::entity::read_model::Column::WarmCompletedAt,
            sea_orm::sea_query::Expr::value(Option::<OffsetDateTime>::None),
        )
        .filter(
            Condition::all()
                .add(bss_pricing::infra::storage::entity::read_model::Column::TenantId.eq(TENANT))
                .add(bss_pricing::infra::storage::entity::read_model::Column::CatalogVersion.eq(7)),
        )
        .exec(&conn)
        .await
        .expect("unwarm the later row");
    assert_eq!(
        affected.rows_affected, 1,
        "the seed must have moved one row"
    );

    let found = resolve(&provider, TENANT, 9)
        .await
        .expect("the warm earlier row still answers");
    assert_eq!(
        found.catalog_version,
        CatalogVersion::new(3),
        "an unwarm row is skipped rather than answering with a half-projected payload"
    );
}

// ---------------------------------------------------------------------------
// The frozen-version property, through the surface
// ---------------------------------------------------------------------------

/// **A consumer pinned to the pre-cancel version still reports the old coverage,
/// and the newest version reports it gone** (§9's own D-99 criterion).
///
/// Asserted **at the store** and not through the route, because the route has no
/// `version=` parameter: §5's signature is `?at=&currency=&region=` and the surface
/// reads the tenant's pin-eligible frontier, so it only ever answers at one version.
/// Adding a parameter to make this assertable through HTTP would mint a surface the
/// design set does not declare. `api::rest::windows::get_plan_sellability` says so
/// where the parameter would have been.
///
/// The two rows differ in **`catalog_version` and `projected_at`**, asserted, so the
/// case cannot pass against one row read twice — which is exactly how a resolution
/// query that ignored the version would look.
#[tokio::test]
async fn a_consumer_pinned_before_a_cancel_still_reads_the_old_coverage() {
    let provider = provider().await;
    // V3: the key is covered to at(40), which clears the monthly horizon from
    // at(5) - the instant every predicate below is evaluated at - by four days.
    // V7: the successor was cancelled, so the projected window set is empty and the
    // key is uncovered.
    project(&provider, TENANT, 3, at(3), &delta_covering(Some(at(40)))).await;
    project(&provider, TENANT, 7, at(7), &delta_uncovered()).await;

    let old = resolve(&provider, TENANT, 3)
        .await
        .expect("the pre-cancel version");
    let new = resolve(&provider, TENANT, 7)
        .await
        .expect("the post-cancel version");

    // Two rows, not one read twice.
    assert_ne!(old.catalog_version, new.catalog_version);
    assert_ne!(old.projected_at, new.projected_at);

    let surface_of = |delta: &StoredDelta| {
        SellabilitySurface::of_delta(
            &read_model_repo::sellability_facts(delta).expect("a payload this gear wrote"),
            at(5),
            &CurrencyCode::new("EUR").expect("three letters"),
            &Region::new("eu").expect("a non-blank region"),
        )
    };

    let before = surface_of(&old);
    assert_eq!(
        before.keys[0].coverage_end,
        CoverageEnd::Ends(at(40)),
        "the frozen version still advertises the coverage the cancel removed"
    );
    assert_eq!(
        key_answer(&before, Predicate::ActiveWindowWithHorizon),
        &PredicateAnswer::Satisfied,
        "and it is covered at the instant asked about, with the monthly horizon met"
    );

    let after = surface_of(&new);
    assert_eq!(
        after.keys[0].coverage_end,
        CoverageEnd::Uncovered,
        "the newest pin-eligible version reports it gone"
    );
    assert!(matches!(
        key_answer(&after, Predicate::ActiveWindowWithHorizon),
        PredicateAnswer::Failed { .. }
    ));
    assert_eq!(after.plan_market_verdict(), PlanMarketVerdict::NotSellable);
}

/// The same plan with its window plane emptied — a cancelled successor leaves the
/// projected key present with no interval, which is the declared spelling of
/// "uncovered" rather than an absent group (D-167 clause 1).
fn delta_uncovered() -> PlanSubjectDelta {
    PlanSubjectDelta {
        windows: vec![KeyWindows {
            scope_key: recurring_key(),
            intervals: Vec::new(),
        }],
        ..delta_covering(None)
    }
}

/// The one key's answer to a per-key predicate.
fn key_answer(surface: &SellabilitySurface, predicate: Predicate) -> &PredicateAnswer {
    surface.keys[0]
        .answers
        .iter()
        .find(|outcome| outcome.predicate == predicate)
        .map(|outcome| &outcome.answer)
        .expect("the key answers every per-key predicate")
}
