//! The retirement flip against a real database (`inst-re-references`, D-90,
//! D-128, D-145).
//!
//! `inst-rt-cancel`'s window unwind and `inst-rt-event`'s `PlanRetired` outbox row
//! are **not** here — this file touches neither plane. The unwind is asserted by
//! `sqlite_retirement_unit` and `sqlite_cutover_unwind`, which drive the
//! orchestrator that writes it. The `PlanRetired` row is asserted only by
//! `outbox_repo_tests`, over the **builder** rather than over a retirement that
//! ran: no suite reads that row back out of the outbox after a flip.
//!
//! Everything here is a property of a **statement** rather than of a branch in
//! Rust: the transition rides inside a compare-and-swap the database evaluates
//! under the row lock, and what makes "the plan's current revision" well defined
//! is the partial `uq_pricing_plan_current` index — which spans
//! `('published','retired')`, so a retired revision **stays** current. That last
//! fact is D-128's load-bearing one and no mock can see it: it is why the
//! projector still finds a retired plan's revision, and therefore why an arrears
//! charge for an in-flight subscriber still resolves after the plan stops
//! selling.
//!
//! `sqlite::memory:` serializes writers, so nothing here distinguishes isolation
//! levels. What it does prove is that a second retirement is refused by the
//! predicate rather than by a Rust `if` — delete the `lifecycle_state =
//! 'published'` clause from the swap and this suite reddens.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use bss_pricing::domain::instant::utc_ymd_hms;
use bss_pricing::domain::lifecycle::LifecycleState;
use bss_pricing::domain::money::{CurrencyCode, MinorAmount};
use bss_pricing::domain::plan_shape::{AddonRule, BillingCycle, Frequency};
use bss_pricing::domain::price_record::PriceContent;
use bss_pricing::domain::price_row::{ModelKind, PriceRow};
use bss_pricing::domain::retirement::BlockingReferenceKind;
use bss_pricing::domain::scope_key::{
    ChargeKind, Cohort, PhaseId, PlanId, PriceEligibility, Region, ScopeKey,
};
use bss_pricing::infra::storage::migrations::Migrator;
use bss_pricing::infra::storage::repo::{
    NewPlanDraft, PlanRepo, PlanShapeRepo, PriceRepo, plan_repo,
};
use bss_pricing::infra::storage::{RepoError, repo_failure};
use time::OffsetDateTime;

use sea_orm_migration::MigratorTrait;
use toolkit_db::secure::AccessScope;
use toolkit_db::{ConnectOpts, DBProvider, DbError, connect_db};
use uuid::Uuid;

const TENANT: Uuid = Uuid::from_u128(0x_7e_11_51);
const CORRELATION: Uuid = Uuid::from_u128(0x_c0_11_a7_11);

fn at(hour: u32) -> OffsetDateTime {
    utc_ymd_hms(2026, 8, 7, hour, 0, 0)
}

fn scope() -> AccessScope {
    AccessScope::for_tenant(TENANT)
}

async fn harness() -> (PlanRepo, DBProvider<DbError>) {
    let db = connect_db("sqlite::memory:", ConnectOpts::default())
        .await
        .expect("connect in-memory sqlite");
    run_migrations(&db).await;
    let provider = DBProvider::<DbError>::new(db);
    (PlanRepo::new(provider.clone()), provider)
}

async fn run_migrations(db: &toolkit_db::Db) {
    toolkit_db::migration_runner::run_migrations_for_testing(db, Migrator::migrations())
        .await
        .expect("run migrator");
}

fn new_draft(plan_id: PlanId) -> NewPlanDraft {
    NewPlanDraft {
        plan_name: None,
        plan_id,
        tenant_id: TENANT,
        created_by: Uuid::from_u128(0xac_11),
        created_at_utc: at(10),
        sku_id: Some(Uuid::from_u128(0x5_c1_11)),
        plan_tier: Some("gold".to_owned()),
        billing_cycle: Some(BillingCycle::Recurring),
        frequency: Some(Frequency::Monthly),
        plan_tier_override: false,
        purchase_min_qty: None,
        purchase_max_qty: None,
        invoice_grouping_key: None,
        available_from: None,
        available_to: None,
        cloned_from: None,
        correlation_id: CORRELATION,
    }
}

/// A plan standing at a published revision `0`, the state retirement acts on.
async fn published_plan(repo: &PlanRepo, provider: &DBProvider<DbError>, plan_id: PlanId) {
    let created = repo
        .create_draft(&scope(), new_draft(plan_id))
        .await
        .expect("create the first revision");
    let (_, published) = provider
        .db()
        .in_transaction::<(), RepoError, _>(move |txn| {
            Box::pin(async move {
                plan_repo::publish_revision(
                    txn,
                    &scope(),
                    TENANT,
                    plan_id,
                    created.revision,
                    created.row_version,
                )
                .await
                .map(|_| ())
            })
        })
        .await;
    published.expect("publish the first revision");
}

/// Retire through the repository, returning whatever it answered.
async fn retire(
    provider: &DBProvider<DbError>,
    plan_id: PlanId,
    revision: u64,
) -> Result<LifecycleState, RepoError> {
    let (_, outcome) = provider
        .db()
        .in_transaction::<LifecycleState, RepoError, _>(move |txn| {
            Box::pin(async move {
                plan_repo::retire_revision(txn, &scope(), TENANT, plan_id, revision)
                    .await
                    .map(|row| row.lifecycle_state)
            })
        })
        .await;
    outcome.map_err(|e| e.into_domain(|infra| RepoError::Db(format!("retire: {infra}"))))
}

#[tokio::test]
async fn retirement_flips_the_current_published_revision_and_leaves_its_tag() {
    let (repo, provider) = harness().await;
    let plan_id = PlanId::new(Uuid::from_u128(0x9_1a5));
    published_plan(&repo, &provider, plan_id).await;

    let before =
        plan_repo::load_current(&provider.conn().expect("conn"), &scope(), TENANT, plan_id)
            .await
            .expect("read")
            .expect("a published plan has a current revision");
    assert_eq!(before.lifecycle_state, LifecycleState::Published);

    let state = retire(&provider, plan_id, before.revision)
        .await
        .expect("the published revision retires");
    assert_eq!(state, LifecycleState::Retired);

    let after = plan_repo::load_current(&provider.conn().expect("conn"), &scope(), TENANT, plan_id)
        .await
        .expect("read")
        .expect("a retired revision is still the plan's current one");
    assert_eq!(after.lifecycle_state, LifecycleState::Retired);
    // **The tag does not move**, and the store is what decides that: every
    // content column of a row past `draft` is frozen by
    // `trg_pricing_plan_frozen_columns`, `row_version` among them, so the only
    // update a published row admits is the bare flip. This assertion was
    // originally written the other way round - a bump alongside the flip - and
    // every case in this file reddened on `(code: 1811) revision is frozen`,
    // a driver refusal rather than an assertion. `supersede_current` had already
    // recorded the same rule on the same table.
    assert_eq!(after.row_version, before.row_version);
    // Same revision number: retirement mints nothing and consumes nothing.
    assert_eq!(after.revision, before.revision);
}

#[tokio::test]
async fn a_retired_revision_stays_the_plans_current_one() {
    // D-128's load-bearing consequence: `uq_pricing_plan_current` spans
    // `('published','retired')`, so the projector still sources the revision and
    // an in-flight subscriber's arrears charge still resolves after the plan
    // stops selling. A retirement that left no current revision would make the
    // plan unresolvable rather than unsellable.
    let (repo, provider) = harness().await;
    let plan_id = PlanId::new(Uuid::from_u128(0x9_1a6));
    published_plan(&repo, &provider, plan_id).await;

    retire(&provider, plan_id, 0).await.expect("retire");

    let current =
        plan_repo::load_current(&provider.conn().expect("conn"), &scope(), TENANT, plan_id)
            .await
            .expect("read");
    assert!(
        current.is_some(),
        "a retired plan must still answer with a current revision"
    );
    assert_eq!(
        current.expect("checked").lifecycle_state,
        LifecycleState::Retired
    );
}

#[tokio::test]
async fn a_second_retirement_is_refused_by_the_predicate() {
    // The `lifecycle_state = 'published'` clause is the guard. Delete it and the
    // second call succeeds, flipping `retired -> retired` and bumping the tag of
    // a row nothing changed about.
    let (repo, provider) = harness().await;
    let plan_id = PlanId::new(Uuid::from_u128(0x9_1a7));
    published_plan(&repo, &provider, plan_id).await;

    retire(&provider, plan_id, 0).await.expect("the first");
    let refusal = retire(&provider, plan_id, 0)
        .await
        .expect_err("the second finds no published row");

    assert!(
        matches!(refusal, RepoError::ConcurrentMutation { .. }),
        "the loser is a contention on the current-revision slot: {refusal:?}"
    );
    assert!(
        refusal.to_string().contains(&plan_id.to_string()),
        "and it names the aggregate: {refusal}"
    );
}

#[tokio::test]
async fn retiring_a_plan_that_never_published_moves_nothing() {
    // Revision `0` is a draft here. The swap names `published`, so it matches no
    // row - and the caller is told so rather than the draft being retired, which
    // is the flip `domain::lifecycle` and the store's trigger both refuse.
    let (repo, provider) = harness().await;
    let plan_id = PlanId::new(Uuid::from_u128(0x9_1a8));
    repo.create_draft(&scope(), new_draft(plan_id))
        .await
        .expect("create the draft");

    let refusal = retire(&provider, plan_id, 0)
        .await
        .expect_err("a draft is not retirable");
    assert!(
        matches!(refusal, RepoError::ConcurrentMutation { .. }),
        "{refusal:?}"
    );

    let draft = repo
        .find_revision(&scope(), TENANT, plan_id, 0)
        .await
        .expect("read")
        .expect("the draft is still there");
    assert_eq!(
        draft.lifecycle_state,
        LifecycleState::Draft,
        "the refusal left the draft alone"
    );
}

#[tokio::test]
async fn the_refusal_reaches_a_caller_as_a_lifecycle_answer_and_not_a_storage_fault() {
    // `repo_failure` is the single ladder every surface maps through; a
    // contention that arrived as `Internal` would tell an operator to open a
    // ticket for a retry they could make themselves.
    let (repo, provider) = harness().await;
    let plan_id = PlanId::new(Uuid::from_u128(0x9_1a9));
    published_plan(&repo, &provider, plan_id).await;
    retire(&provider, plan_id, 0).await.expect("the first");

    let refusal = retire(&provider, plan_id, 0).await.expect_err("the second");
    let domain = repo_failure(&refusal);
    assert!(
        matches!(
            domain,
            bss_pricing::domain::error::DomainError::ConcurrentMutation(_)
        ),
        "{domain:?}"
    );
}

// ---------------------------------------------------------------------------
// `inst-re-references` — what refers to the plan, read from the store.
// ---------------------------------------------------------------------------

/// A bundle plan carrying `component` in its **current published** revision.
///
/// Composition is authored on the draft and then published, because that is the
/// only state `infra::retirement::references` counts (D-212's narrowing): a
/// draft revision's component set is nobody's truth.
async fn published_bundle_on(
    plans: &PlanRepo,
    bundles: &bss_pricing::infra::storage::repo::BundleRepo,
    bundle_plan: PlanId,
    component: PlanId,
) -> Uuid {
    let created = plans
        .create_draft(&scope(), new_draft(bundle_plan))
        .await
        .expect("create the bundle plan draft");
    let bundle_id = bundles
        .create(
            &scope(),
            bss_pricing::infra::storage::repo::NewBundle {
                bundle_id: Uuid::now_v7(),
                tenant_id: TENANT,
                plan_id: bundle_plan,
                price_basis: bss_pricing::domain::bundle::PriceBasis::SumOfParts,
                invoice_itemization: bss_pricing::domain::bundle::InvoiceItemization::Aggregate,
            },
            stamp(),
        )
        .await
        .expect("create the bundle");
    bundles
        .replace_composition(
            &scope(),
            TENANT,
            bundle_plan,
            created.revision,
            created.row_version,
            bss_pricing::infra::storage::repo::CompositionDraft {
                components: vec![bss_pricing::infra::storage::repo::BundleComponentDraft {
                    component_plan_id: component.get(),
                    included_sku_id: Uuid::from_u128(0x5_c2),
                    min_qty: None,
                    max_qty: None,
                }],
                rev_share_groups: Vec::new(),
            },
            stamp(),
        )
        .await
        .expect("author the composition");
    bundle_id
}

/// Publish one revision of a plan through the sanctioned path.
async fn publish_revision_of(
    repo: &PlanRepo,
    provider: &DBProvider<DbError>,
    plan_id: PlanId,
    revision: u64,
) {
    let draft = repo
        .find_revision(&scope(), TENANT, plan_id, revision)
        .await
        .expect("read")
        .expect("the revision to publish");
    let (_, published) = provider
        .db()
        .in_transaction::<(), RepoError, _>(move |txn| {
            Box::pin(async move {
                plan_repo::publish_revision(
                    txn,
                    &scope(),
                    TENANT,
                    plan_id,
                    revision,
                    draft.row_version,
                )
                .await
                .map(|_| ())
            })
        })
        .await;
    published.expect("publish the revision");
}

fn stamp() -> bss_pricing::domain::audit::AuditStamp {
    bss_pricing::domain::audit::AuditStamp {
        actor_principal_id: Uuid::from_u128(0xac_11),
        recorded_at: at(12),
        correlation_id: CORRELATION,
    }
}

#[tokio::test]
async fn a_plan_nothing_references_reports_an_empty_report() {
    let (repo, provider) = harness().await;
    let plan_id = PlanId::new(Uuid::from_u128(0x9_1b0));
    published_plan(&repo, &provider, plan_id).await;

    let report = bss_pricing::infra::retirement::references(
        &provider.conn().expect("conn"),
        &scope(),
        TENANT,
        plan_id,
    )
    .await
    .expect("read the references");

    assert!(report.blocking.is_empty());
    assert!(report.ensure_retirable(plan_id.get()).is_ok());
}

#[tokio::test]
async fn a_component_of_a_published_bundle_blocks_the_retirement() {
    let (repo, provider) = harness().await;
    let bundles = bss_pricing::infra::storage::repo::BundleRepo::new(provider.clone());
    let component = PlanId::new(Uuid::from_u128(0x9_1b1));
    let bundle_plan = PlanId::new(Uuid::from_u128(0x9_1b2));
    published_plan(&repo, &provider, component).await;

    let bundle_id = published_bundle_on(&repo, &bundles, bundle_plan, component).await;
    // Published, so the composition becomes the one consumers resolve against.
    publish_revision_of(&repo, &provider, bundle_plan, 0).await;

    let report = bss_pricing::infra::retirement::references(
        &provider.conn().expect("conn"),
        &scope(),
        TENANT,
        component,
    )
    .await
    .expect("read the references");

    assert_eq!(report.blocking.len(), 1, "{report:?}");
    assert_eq!(report.blocking[0].1.referrer_id, bundle_id);
    let err = report
        .ensure_retirable(component.get())
        .expect_err("a bundle component may not retire");
    assert!(err.to_string().contains("bundle component"), "{err}");
}

#[tokio::test]
async fn a_component_only_an_unpublished_bundle_plan_names_does_not_block() {
    // The bundle plan never published at all, so it has no current revision and
    // nothing resolves its composition.
    let (repo, provider) = harness().await;
    let bundles = bss_pricing::infra::storage::repo::BundleRepo::new(provider.clone());
    let component = PlanId::new(Uuid::from_u128(0x9_1b3));
    let bundle_plan = PlanId::new(Uuid::from_u128(0x9_1b4));
    published_plan(&repo, &provider, component).await;

    // Authored, and deliberately **not** published.
    published_bundle_on(&repo, &bundles, bundle_plan, component).await;

    let report = bss_pricing::infra::retirement::references(
        &provider.conn().expect("conn"),
        &scope(),
        TENANT,
        component,
    )
    .await
    .expect("read the references");

    assert!(
        report.blocking.is_empty(),
        "a composition nobody resolves is not a reference: {report:?}"
    );
}

#[tokio::test]
async fn a_component_the_bundles_current_revision_dropped_does_not_block() {
    // **D-212's narrowing, and the only case that reaches it.** The membership
    // row for revision 0 survives - `pricing_bundle_component` is revision-scoped
    // and revision 0 is frozen, not deleted - while the revision consumers
    // actually resolve, 1, no longer lists the component. A read that probed the
    // index and stopped would still block the retirement here.
    //
    // Its sibling above cannot reach this branch: a bundle plan with no current
    // revision at all is filtered one step earlier, by `current_revision`
    // answering `None`. Written down because the first version of this file
    // asserted the narrowing through that sibling, and deleting the narrowing
    // reddened nothing.
    let (repo, provider) = harness().await;
    let bundles = bss_pricing::infra::storage::repo::BundleRepo::new(provider.clone());
    let component = PlanId::new(Uuid::from_u128(0x9_1b6));
    let bundle_plan = PlanId::new(Uuid::from_u128(0x9_1b7));
    published_plan(&repo, &provider, component).await;
    published_bundle_on(&repo, &bundles, bundle_plan, component).await;
    publish_revision_of(&repo, &provider, bundle_plan, 0).await;

    // Revision 0 is current and names the component: it blocks.
    let before = bss_pricing::infra::retirement::references(
        &provider.conn().expect("conn"),
        &scope(),
        TENANT,
        component,
    )
    .await
    .expect("read");
    assert_eq!(before.blocking.len(), 1, "the seed must reach the state");

    // Revision 1 drops it.
    let successor = repo
        .open_revision(&scope(), TENANT, bundle_plan, stamp())
        .await
        .expect("open the successor revision");
    bundles
        .replace_composition(
            &scope(),
            TENANT,
            bundle_plan,
            successor.revision,
            successor.row_version,
            bss_pricing::infra::storage::repo::CompositionDraft::default(),
            stamp(),
        )
        .await
        .expect("drop the component");
    publish_revision_of(&repo, &provider, bundle_plan, successor.revision).await;

    let after = bss_pricing::infra::retirement::references(
        &provider.conn().expect("conn"),
        &scope(),
        TENANT,
        component,
    )
    .await
    .expect("read");
    assert!(
        after.blocking.is_empty(),
        "revision 0's frozen membership row is not the composition consumers resolve: {after:?}"
    );
}

/// **A rule on the retiring plan's *own* row is not a referrer** — the third of
/// `referencing_addon_overrides`' filters, and the one nothing measured.
///
/// This replaces `a_plan_with_no_price_rows_has_no_add_on_override_referrers`,
/// which was a strict duplicate of
/// [`a_plan_nothing_references_reports_an_empty_report`]: same seed, same call, a
/// subset of the same assertions, and it reached only the empty-`price_ids`
/// short-circuit — which its own comment named as the subject while asserting
/// nothing about probe count.
///
/// The two revision filters are driven by
/// [`an_override_the_referrers_current_revision_holds_blocks_past_an_older_row`]
/// and [`an_override_only_a_superseded_revision_holds_does_not_block`]. The
/// self-reference exclusion is the residue: retiring a plan whose own add-on rule
/// overrides one of its own price rows must refuse nothing, and without the
/// `*referrer != plan_id` filter that plan could never be retired at all.
///
/// **Both directions, in one world**, because the absence proves nothing on its
/// own: the second half points a *second* plan's rule at the same row and gets the
/// block, so the fixture is demonstrably capable of producing one.
#[tokio::test]
async fn a_rule_on_the_retiring_plans_own_row_is_not_a_referrer() {
    let (repo, provider) = harness().await;
    let shapes = PlanShapeRepo::new(provider.clone());
    let prices = PriceRepo::new(provider.clone());
    let retiring = PlanId::new(Uuid::from_u128(0x9_1b5));
    let neighbour = PlanId::new(Uuid::from_u128(0x9_1b6));

    // The retiring plan prices a row and then overrides it from its own add-on
    // rule set, on the revision consumers resolve.
    let created = repo
        .create_draft(&scope(), new_draft(retiring))
        .await
        .expect("create the retiring plan");
    let price_id = price_row_on(&prices, retiring).await;
    shapes
        .replace_addon_rules(
            &scope(),
            TENANT,
            retiring,
            created.revision,
            created.row_version,
            vec![override_rule(price_id)],
            stamp(),
        )
        .await
        .expect("the plan overrides its own row");
    publish_revision_of(&repo, &provider, retiring, created.revision).await;

    let report = bss_pricing::infra::retirement::references(
        &provider.conn().expect("conn"),
        &scope(),
        TENANT,
        retiring,
    )
    .await
    .expect("read the references");
    assert!(
        report.blocking.is_empty(),
        "a plan is not a referrer of itself, and without that filter it could never retire: \
         {report:?}"
    );
    assert!(report.ensure_retirable(retiring.get()).is_ok());

    // The positive control, same row, same query: a **second** plan's rule on it
    // does block. So the absence above is the self-reference filter rather than a
    // probe that finds nothing.
    let others = repo
        .create_draft(&scope(), new_draft(neighbour))
        .await
        .expect("create the neighbour");
    shapes
        .replace_addon_rules(
            &scope(),
            TENANT,
            neighbour,
            others.revision,
            others.row_version,
            vec![override_rule(price_id)],
            stamp(),
        )
        .await
        .expect("the neighbour overrides the same row");
    publish_revision_of(&repo, &provider, neighbour, others.revision).await;

    let report = bss_pricing::infra::retirement::references(
        &provider.conn().expect("conn"),
        &scope(),
        TENANT,
        retiring,
    )
    .await
    .expect("read the references");
    assert_eq!(
        report.blocking.len(),
        1,
        "exactly the neighbour, and not the retiring plan's own rule beside it: {report:?}"
    );
    assert_eq!(
        report.blocking[0].0,
        BlockingReferenceKind::AddOnPriceOverrideTarget
    );
    assert_eq!(report.blocking[0].1.referrer_id, neighbour.get());
}

// ---------------------------------------------------------------------------
// `inst-re-references`, the add-on override half — the referring plan carries a
// rule row at every revision it has published (D-83's copy-forward).
// ---------------------------------------------------------------------------

/// One `draft` price row on `plan_id`: the thing an add-on override points at.
///
/// Draft on purpose. `price_repo::load_scope_keys_for_plan` applies no lifecycle
/// filter — its own doc says why — so the reference is a reference whatever state
/// the row is in, and a draft is the cheapest state to reach.
async fn price_row_on(prices: &PriceRepo, plan_id: PlanId) -> Uuid {
    let price_id = Uuid::now_v7();
    let mut row = PriceRow::new(ChargeKind::Recurring, Some(ModelKind::Flat));
    row.amount_minor = Some(MinorAmount::new(9_900).expect("a non-negative amount"));
    prices
        .create_draft(
            &scope(),
            TENANT,
            bss_pricing::infra::storage::repo::NewPriceDraft {
                price_id,
                scope_key: ScopeKey::new(
                    plan_id,
                    CurrencyCode::new("EUR").expect("three letters"),
                    Region::new("eu").expect("a non-blank region"),
                    PhaseId::new(Uuid::from_u128(0xfa_11)),
                    PriceEligibility::AllSubscriptions,
                    ChargeKind::Recurring,
                    Cohort::None,
                )
                .expect("the class pairs with the cohort"),
                content: PriceContent {
                    row,
                    tax_inclusive: false,
                    tax_category_ref: None,
                    billing_timing: Some("advance".to_owned()),
                    proration_contract: None,
                    rounding_policy_ref: Some("half_up".to_owned()),
                    grandfather_until: None,
                    supersedes_price_id: None,
                },
                created_by: Uuid::from_u128(0xac_11),
                created_at_utc: at(10),
                correlation_id: CORRELATION,
            },
        )
        .await
        .expect("author the price row the override points at");
    price_id
}

/// One add-on rule overriding `price_override_ref`.
fn override_rule(price_override_ref: Uuid) -> AddonRule {
    AddonRule {
        addon_sku_id: Uuid::from_u128(0x5_a11),
        required: false,
        min_qty: None,
        max_qty: None,
        step_qty: None,
        price_override_ref: Some(price_override_ref),
        depends_on: Vec::new(),
        conflicts_with: Vec::new(),
    }
}

/// A plan that has published **two** revisions, each carrying an add-on rule
/// overriding `price_id`.
///
/// The second row is not authored: `open_revision` copies every rule row forward
/// with `price_override_ref` unchanged (D-83), which is exactly why a referring
/// plan holds one row per revision it has ever published — and why the row the
/// unordered probe query returns first is the **oldest** one.
async fn referrer_with_two_published_revisions(
    plans: &PlanRepo,
    shapes: &PlanShapeRepo,
    provider: &DBProvider<DbError>,
    referrer: PlanId,
    price_id: Uuid,
) {
    let created = plans
        .create_draft(&scope(), new_draft(referrer))
        .await
        .expect("create the referring plan");
    shapes
        .replace_addon_rules(
            &scope(),
            TENANT,
            referrer,
            created.revision,
            created.row_version,
            vec![override_rule(price_id)],
            stamp(),
        )
        .await
        .expect("author the override on revision 0");
    publish_revision_of(plans, provider, referrer, created.revision).await;

    let successor = plans
        .open_revision(&scope(), TENANT, referrer, stamp())
        .await
        .expect("open the successor revision");
    publish_revision_of(plans, provider, referrer, successor.revision).await;
    assert_eq!(
        successor.revision, 1,
        "the fixture's whole point is a second revision carrying a copy of the rule"
    );
}

#[tokio::test]
async fn an_override_the_referrers_current_revision_holds_blocks_past_an_older_row() {
    // **The per-plan collapse must not run ahead of the revision test** (review
    // H10, 2026-08-20). A referring plan holds one rule row per revision it has
    // published, the probe query is unfiltered by revision and unordered, and an
    // unordered scan tends to answer insertion order — the oldest revision first.
    // With the de-duplication in front of `rule.plan_revision != revision`, that
    // first row consumed the plan's one slot and was then dropped as stale, so the
    // row on the revision consumers actually resolve was never looked at and the
    // retirement went through.
    //
    // To redden this: move the `!seen.insert(rule.plan_id)` clause back into the
    // `continue` guard at the top of the loop.
    let (repo, provider) = harness().await;
    let shapes = PlanShapeRepo::new(provider.clone());
    let prices = PriceRepo::new(provider.clone());
    let retiring = PlanId::new(Uuid::from_u128(0x9_1b8));
    let referrer = PlanId::new(Uuid::from_u128(0x9_1b9));
    published_plan(&repo, &provider, retiring).await;
    let price_id = price_row_on(&prices, retiring).await;
    referrer_with_two_published_revisions(&repo, &shapes, &provider, referrer, price_id).await;

    let report = bss_pricing::infra::retirement::references(
        &provider.conn().expect("conn"),
        &scope(),
        TENANT,
        retiring,
    )
    .await
    .expect("read the references");

    assert_eq!(report.blocking.len(), 1, "{report:?}");
    assert_eq!(
        report.blocking[0].0,
        BlockingReferenceKind::AddOnPriceOverrideTarget
    );
    assert_eq!(report.blocking[0].1.referrer_id, referrer.get());
    let err = report
        .ensure_retirable(retiring.get())
        .expect_err("a plan another plan's current revision overrides may not retire");
    assert!(
        err.to_string().contains("add-on price-override target"),
        "{err}"
    );
}

#[tokio::test]
async fn an_override_only_a_superseded_revision_holds_does_not_block() {
    // The narrowing itself, and the control the case above needs: the rule row
    // survives on revision 0 because a published revision is frozen, and revision
    // 1 — the one consumers resolve — dropped it. A probe that reported every rule
    // row whatever its revision would block here, which is the fault in the
    // opposite direction from H10's.
    let (repo, provider) = harness().await;
    let shapes = PlanShapeRepo::new(provider.clone());
    let prices = PriceRepo::new(provider.clone());
    let retiring = PlanId::new(Uuid::from_u128(0x9_1ba));
    let referrer = PlanId::new(Uuid::from_u128(0x9_1bb));
    published_plan(&repo, &provider, retiring).await;
    let price_id = price_row_on(&prices, retiring).await;

    let created = repo
        .create_draft(&scope(), new_draft(referrer))
        .await
        .expect("create the referring plan");
    shapes
        .replace_addon_rules(
            &scope(),
            TENANT,
            referrer,
            created.revision,
            created.row_version,
            vec![override_rule(price_id)],
            stamp(),
        )
        .await
        .expect("author the override on revision 0");
    publish_revision_of(&repo, &provider, referrer, created.revision).await;

    // Revision 1 carries the copy forward and then drops it.
    let successor = repo
        .open_revision(&scope(), TENANT, referrer, stamp())
        .await
        .expect("open the successor revision");
    shapes
        .replace_addon_rules(
            &scope(),
            TENANT,
            referrer,
            successor.revision,
            successor.row_version,
            Vec::new(),
            stamp(),
        )
        .await
        .expect("drop the override on revision 1");
    publish_revision_of(&repo, &provider, referrer, successor.revision).await;

    let report = bss_pricing::infra::retirement::references(
        &provider.conn().expect("conn"),
        &scope(),
        TENANT,
        retiring,
    )
    .await
    .expect("read the references");
    assert!(
        report.blocking.is_empty(),
        "revision 0's frozen rule row is not the override consumers resolve: {report:?}"
    );
    assert!(report.ensure_retirable(retiring.get()).is_ok());
}
