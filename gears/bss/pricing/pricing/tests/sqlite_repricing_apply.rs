//! The mass-repricing apply's own atomicity and idempotency —
//! `design/12-operator-efficiency.md` §3 `inst-mr-apply`, `inst-mr-validate-scope`
//! (D-134), driven at the repository seam rather than through HTTP.
//!
//! `tests/rest_repricing_runs.rs` proves the two REST surfaces and the states
//! `open_repricing_run`/the approve arm leave a run in; what it cannot reach
//! without an enormous fixture is D-134's one property that is easiest to lose
//! and hardest to notice — that a plan whose aggregate pass fails after its
//! rows have already been written **inside the same transaction** applies none
//! of them, while a sibling plan in the same run applies whole. This suite
//! drives [`apply_run_in`] directly, over a run and a journal built at the
//! repository seam, for exactly that reason.
//!
//! # How plan B's aggregate pass is made to fail
//!
//! Not via a defect the repricing adjustment itself introduces — a plain
//! markup/discount/fixed adjustment changes only a row's amount, and none of
//! the Foundation's aggregate-only rules (phase coverage, descriptor
//! completeness, region declaration, window coverage) are sensitive to an
//! amount's *value*, so an adjustment that starts from a clean plan cannot by
//! itself trip one. Plan B instead carries a **stray draft row** — a second,
//! unrelated key with no window scheduled on it, exactly the shape an
//! interactive author mid-edit would leave — which this suite's own module doc
//! (`infra::repricing`) names as the consequence of not taking the bulk lock:
//! `assemble_from`'s candidate set is every `published`/`draft` row of the
//! plan, so the stray draft rides along into the aggregate pass touching
//! nothing this run selected. `WINDOW_COVERAGE_MISSING` (`inst-wc-required`) is
//! genuinely an aggregate-only check — `domain::coverage::window_coverage_rules`
//! is registered in `run_publish_rules` and nowhere in
//! `domain::supersession::plan_supersession`'s row-local set — so plan B's own
//! selected row commits cleanly through its row-local checks and only the
//! aggregate pass, reading the transaction's own writes back, catches it.
//!
//! What this proves is the property D-134 states, not a claim that a plain
//! repricing run can organically produce this shape on a plan nothing else
//! touched: **once written, a row-local pass is not the last word — the
//! aggregate pass over the plan's real, post-commit state is**, and its
//! refusal takes every row of that plan's own selection down with it.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex};
use toolkit_canonical_errors::CanonicalError;

use async_trait::async_trait;
use bss_pricing::domain::audit::AuditStamp;
use bss_pricing::domain::audit::AuditSubjectKind;
use bss_pricing::domain::bulk::{BulkKind, BulkState, JournalState};
use bss_pricing::domain::concurrency::RowVersion;
use bss_pricing::domain::contracts::{BillingAnchorPolicy, ProrationBasis, ProrationContract};
use bss_pricing::domain::error::DomainError;
use bss_pricing::domain::instant::utc_ymd_hms;
use bss_pricing::domain::money::{CurrencyCode, MinorAmount, RateMinor};
use bss_pricing::domain::plan_shape::{
    BillingCycle, DescriptorSet, Frequency, PhaseKind, PlanPhase,
};
use bss_pricing::domain::price_record::PriceContent;
use bss_pricing::domain::price_row::{
    BillingGranularity, ModelKind, PriceRow, TierAggregationWindow, TierBand,
};
use bss_pricing::domain::scope_key::{
    ChargeKind, Cohort, PhaseId, PlanId, PriceEligibility, Region, ScopeKey,
};
use bss_pricing::infra::repricing::apply_run_in;
use bss_pricing::infra::storage::migrations::Migrator;
use bss_pricing::infra::storage::repo::approval_repo::NewApproval;
use bss_pricing::infra::storage::repo::repricing_journal_repo::NewJournalRow;
use bss_pricing::infra::storage::repo::{
    IdempotencyGate, NewBulkOperation, NewPlanDraft, NewPriceDraft, PlanRepo, PlanShapeRepo,
    PolicyObjectRepo, PriceRepo, approval_repo, audit_repo, bulk_repo, price_repo,
    repricing_journal_repo,
};
use bss_pricing_sdk::catalog_version_registry::{CatalogVersionRegistryV1, PendingVersionRef};
use time::OffsetDateTime;

use bss_pricing::domain::instant::format_rfc3339;
use sea_orm_migration::MigratorTrait;
use toolkit_db::secure::AccessScope;
use toolkit_db::{ConnectOpts, DBProvider, DbError, connect_db};
use toolkit_security::SecurityContext;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// The registry double — this suite's own, per `sqlite_publish_commit.rs`'s
// convention that no double is shared across binaries.
// ---------------------------------------------------------------------------

#[derive(Default)]
struct RegistryDouble {
    issued: Mutex<HashMap<String, String>>,
}

impl RegistryDouble {
    /// The request ids this double has been asked for, in no order.
    ///
    /// The map is keyed on the request id and `or_insert_with` is idempotent on
    /// it, so this counts **distinct** requests — which is what a claim about a
    /// stranded handle means: one act, one id, however many times it is retried.
    fn requested(&self) -> Vec<String> {
        self.issued
            .lock()
            .expect("no panics in the double")
            .keys()
            .cloned()
            .collect()
    }
}

#[async_trait]
impl CatalogVersionRegistryV1 for RegistryDouble {
    async fn request_version(
        &self,
        _ctx: &SecurityContext,
        request_id: &str,
    ) -> Result<PendingVersionRef, CanonicalError> {
        let mut issued = self.issued.lock().expect("no panics in the double");
        let next = issued.len();
        let pending = issued
            .entry(request_id.to_owned())
            .or_insert_with(|| format!("pend-{next}"))
            .clone();
        Ok(PendingVersionRef {
            request_id: request_id.to_owned(),
            pending_ref: pending,
        })
    }

    async fn committed_version(
        &self,
        _ctx: &SecurityContext,
        _pending_ref: &str,
    ) -> Result<Option<bss_pricing_sdk::catalog_version::CatalogVersion>, CanonicalError> {
        Ok(None)
    }
}

// ---------------------------------------------------------------------------
// The harness.
// ---------------------------------------------------------------------------

const TENANT: Uuid = Uuid::from_u128(0x7e_11);
const ACTOR: Uuid = Uuid::from_u128(0xac_10);
const CORRELATION: Uuid = Uuid::from_u128(0xc0_11);

fn ctx() -> SecurityContext {
    SecurityContext::builder()
        .subject_id(ACTOR)
        .subject_tenant_id(TENANT)
        .build()
        .expect("a subject and a tenant are all a context needs")
}

fn at(hour: u32) -> OffsetDateTime {
    utc_ymd_hms(2026, 8, 3, hour, 0, 0)
}

/// Far enough out that no wall clock reaches it, and clear of the batching
/// delay floor `ChangeoverMoment::Commit` holds the apply to — the fixtures'
/// standing rule (`tests/rest_repricing_runs.rs` carries the identical
/// constant for the identical reason).
fn changeover() -> OffsetDateTime {
    utc_ymd_hms(2099, 8, 20, 0, 0, 0)
}

struct Harness {
    provider: DBProvider<DbError>,
    plans: PlanRepo,
    shapes: PlanShapeRepo,
    prices: PriceRepo,
    policies: PolicyObjectRepo,
    registry: Arc<RegistryDouble>,
    scope: AccessScope,
}

async fn harness() -> Harness {
    let db = connect_db("sqlite::memory:", ConnectOpts::default())
        .await
        .expect("connect in-memory sqlite");
    toolkit_db::migration_runner::run_migrations_for_testing(&db, Migrator::migrations())
        .await
        .expect("run migrator");
    let provider = DBProvider::<DbError>::new(db);
    common::declare_fixture_regions(&provider, TENANT).await;
    Harness {
        // Field order matches the struct definition's, so every field here is
        // a `.clone()` of the cheap handle rather than the one bare move that
        // would otherwise have to come last.
        provider: provider.clone(),
        plans: PlanRepo::new(provider.clone()),
        shapes: PlanShapeRepo::new(provider.clone()),
        prices: PriceRepo::new(provider.clone()),
        policies: PolicyObjectRepo::new(
            provider.clone(),
            &bss_pricing::config::LimitsConfig::default(),
        ),
        registry: Arc::new(RegistryDouble::default()),
        scope: AccessScope::for_tenant(TENANT),
    }
}

/// A row the whole aggregate rule set passes: recurring, flat, tax-exclusive,
/// carrying the billing timing, proration contract, rounding policy **and tax
/// category** that `inst-pi-required`, `ROUNDING_POLICY_UNRESOLVED` and
/// `TAX_BASIS_INCOMPLETE` each demand of one.
///
/// `tax_category_ref` was `None` until 2026-08-20, and this suite published these
/// rows against `RegionTaxReadiness::empty()` — so every one of them froze
/// `resolved_tax_category` NULL, the state `pricing_price`'s migration header calls
/// impossible for a published row and `trg_pricing_price_append_only` makes unrepairable.
/// `price_repo::publish_rows` did not refuse it; H14 of the 2026-08-19 review moved
/// that refusal into the store, and these fixtures were among its violators. The
/// row now states its own category, exactly as it already stated its own rounding
/// policy for the identical reason one column over — nothing this suite proves
/// about repricing atomicity depends on either.
fn publishable_row(amount_minor: i64) -> PriceContent {
    let mut row = PriceRow::new(ChargeKind::Recurring, Some(ModelKind::Flat));
    row.amount_minor = Some(MinorAmount::new(amount_minor).expect("a non-negative amount"));
    PriceContent {
        row,
        tax_inclusive: false,
        tax_category_ref: Some("standard".to_owned()),
        billing_timing: Some("advance".to_owned()),
        proration_contract: Some(ProrationContract {
            billing_anchor_policy: BillingAnchorPolicy::CalendarMonth,
            proration_basis: ProrationBasis::CalendarDaysActual,
            credit_on_downgrade: false,
        }),
        rounding_policy_ref: Some("half_up".to_owned()),
        grandfather_until: None,
        supersedes_price_id: None,
    }
}

/// [`publishable_row`]'s graduated sibling: a two-band ladder, priced from
/// **rates** rather than from `amount_minor` (D-311).
///
/// Its bands are the pair `a_markup_that_overflows_one_band_of_a_ladder_is_out_of_range_for_the_row`
/// uses — one that survives a fat-fingered markup and one that does not — because
/// the defect only exists where the two disagree.
fn publishable_graduated_row() -> PriceContent {
    let mut row = PriceRow::new(ChargeKind::Usage, Some(ModelKind::Graduated));
    row.meter = Some("cloudlets".to_owned());
    // `EVAL_POLICY_MISSING`'s two operands for a tiered usage row: the unit the
    // bands are counted in, and the window the tier counter resets on.
    row.billing_granularity = Some(BillingGranularity::WholeUnit);
    row.tier_aggregation_window = Some(TierAggregationWindow::CalendarMonth);
    row.bands = vec![
        TierBand::closed(
            0,
            1_000,
            RateMinor::from_nano_minor(1_000_000_000).expect("a non-negative rate"),
        ),
        TierBand::open(
            1_000,
            RateMinor::from_nano_minor(100_000_000_000).expect("a non-negative rate"),
        ),
    ];
    PriceContent {
        row,
        tax_inclusive: false,
        tax_category_ref: Some("standard".to_owned()),
        billing_timing: Some("advance".to_owned()),
        proration_contract: Some(ProrationContract {
            billing_anchor_policy: BillingAnchorPolicy::CalendarMonth,
            proration_basis: ProrationBasis::CalendarDaysActual,
            credit_on_downgrade: false,
        }),
        rounding_policy_ref: Some("half_up".to_owned()),
        grandfather_until: None,
        supersedes_price_id: None,
    }
}

/// [`report`] under a **markup** of `value_bp`, which is the adjustment the
/// out-of-range refusal is about: `magnitude_out_of_range` bounds a discount at
/// 10 000 bp and leaves a markup unbounded above zero.
fn markup_report(value_bp: i64) -> serde_json::Value {
    serde_json::json!({
        "selector": serde_json::Value::Null,
        "adjustment": {
            "adjustment_kind": "markup",
            "magnitude_kind": "percent_bp",
            "adjustment_value": value_bp,
            "amounts": {},
        },
        "changeover": format_rfc3339(changeover()),
        "selected": 0,
    })
}

/// A plan the whole aggregate rule set passes: one evergreen terminal phase, a
/// complete descriptor set, a tier and a frequency — [`seed_publishable_shape`]
/// in `tests/rest_support/mod.rs`'s own recipe, built here at the repository
/// seam instead of through the authoring routes.
async fn seed_plan(h: &Harness, plan_id: Uuid, phase_id: Uuid) {
    let plan = PlanId::new(plan_id);
    let created = h
        .plans
        .create_draft(
            &h.scope,
            NewPlanDraft {
                plan_name: None,
                plan_id: plan,
                tenant_id: TENANT,
                created_by: ACTOR,
                created_at_utc: at(10),
                sku_id: Some(Uuid::from_u128(0x5_c1)),
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
            },
        )
        .await
        .expect("create the draft plan");
    let after_phases = h
        .shapes
        .replace_phases(
            &h.scope,
            TENANT,
            plan,
            created.revision,
            created.row_version,
            vec![PlanPhase {
                phase_id: PhaseId::new(phase_id),
                kind: PhaseKind::Evergreen,
                display_name: None,
                ordinal: 0,
                converts_to_phase_id: None,
                phase_duration_days: None,
                display_trial_days: None,
            }],
            stamp(),
        )
        .await
        .expect("attach the phase chain");
    h.shapes
        .set_descriptor_set(
            &h.scope,
            TENANT,
            plan,
            created.revision,
            after_phases.row_version,
            DescriptorSet {
                invoice_line_template: Some("{plan}".to_owned()),
                gl_code: Some("4000".to_owned()),
                itemization_rule: Some("per_charge".to_owned()),
                additional: std::collections::BTreeMap::new(),
            },
            stamp(),
        )
        .await
        .expect("attach the descriptor set");
    publish_plan(h, plan, created.revision).await;
}

async fn publish_plan(h: &Harness, plan: PlanId, revision: u64) {
    let current = h
        .plans
        .find_revision(&h.scope, TENANT, plan, revision)
        .await
        .expect("read the revision")
        .expect("the revision exists");
    let scope = h.scope.clone();
    let (_, outcome) = h
        .provider
        .db()
        .in_transaction::<_, bss_pricing::infra::storage::RepoError, _>(move |txn| {
            Box::pin(async move {
                bss_pricing::infra::storage::repo::plan_repo::publish_revision(
                    txn,
                    &scope,
                    TENANT,
                    plan,
                    revision,
                    current.row_version,
                )
                .await
            })
        })
        .await;
    outcome.expect("publish the seeded plan revision");
}

fn scope_key(plan: PlanId, phase: Uuid, region: &str) -> ScopeKey {
    ScopeKey::new(
        plan,
        CurrencyCode::new("USD").expect("currency"),
        Region::new(region).expect("region"),
        PhaseId::new(phase),
        PriceEligibility::AllSubscriptions,
        ChargeKind::Recurring,
        Cohort::None,
    )
    .expect("scope key")
}

/// [`scope_key`]'s usage sibling: `graduated` and `volume` are usage-only kinds
/// (`MODEL_KIND_CHARGEKIND_MISMATCH`), so a ladder cannot ride a recurring key.
fn usage_scope_key(plan: PlanId, phase: Uuid, region: &str) -> ScopeKey {
    ScopeKey::new(
        plan,
        CurrencyCode::new("USD").expect("currency"),
        Region::new(region).expect("region"),
        PhaseId::new(phase),
        PriceEligibility::AllSubscriptions,
        ChargeKind::Usage,
        Cohort::None,
    )
    .expect("scope key")
}

/// Author and publish one row, with a scheduled coverage window
/// (`inst-wc-required`) — the shape every row this suite selects for
/// repricing needs.
async fn seed_published_row(
    h: &Harness,
    plan: PlanId,
    phase: Uuid,
    region: &str,
    amount_minor: i64,
) -> Uuid {
    seed_published_content(
        h,
        scope_key(plan, phase, region),
        publishable_row(amount_minor),
    )
    .await
}

/// [`seed_published_row`] over content the caller chooses, for the cases whose
/// subject is the row's **shape** rather than its amount.
async fn seed_published_content(h: &Harness, key: ScopeKey, content: PriceContent) -> Uuid {
    let price_id = Uuid::now_v7();
    let plan = key.plan_id();
    h.prices
        .create_draft(
            &h.scope,
            TENANT,
            NewPriceDraft {
                price_id,
                scope_key: key,
                content,
                created_by: ACTOR,
                created_at_utc: at(10),
                correlation_id: CORRELATION,
            },
        )
        .await
        .expect("author the price row");
    common::schedule_coverage_window(
        &h.provider.conn().expect("conn"),
        &h.scope,
        TENANT,
        price_id,
        stamp(),
    )
    .await;
    let scope = h.scope.clone();
    let (_, outcome) = h
        .provider
        .db()
        .in_transaction::<_, bss_pricing::infra::storage::RepoError, _>(move |txn| {
            Box::pin(async move {
                bss_pricing::infra::storage::repo::price_repo::publish_rows(
                    txn,
                    &scope,
                    TENANT,
                    plan,
                    &[(price_id, RowVersion::new(0))],
                    &bss_pricing::domain::tax_display::RegionTaxReadiness::empty(),
                    None,
                )
                .await
            })
        })
        .await;
    outcome.expect("publish the seeded price row");
    price_id
}

/// A real, currently-published `existing_grandfathered` generation on
/// `plan`/`phase`/`region` — [`seed_published_row`]'s own recipe (a scheduled
/// coverage window included, so the aggregate pass's `WINDOW_COVERAGE_MISSING`
/// never fires on it), over a key whose `priceEligibility` and `cohort` axes
/// mark it immutable in price (Foundation §4.3, `inst-mp-grandfathered`).
async fn seed_grandfathered_row(
    h: &Harness,
    plan: PlanId,
    phase: Uuid,
    region: &str,
    generation: OffsetDateTime,
    amount_minor: i64,
) -> Uuid {
    let price_id = Uuid::now_v7();
    let key = ScopeKey::new(
        plan,
        CurrencyCode::new("USD").expect("currency"),
        Region::new(region).expect("region"),
        PhaseId::new(phase),
        PriceEligibility::ExistingGrandfathered,
        ChargeKind::Recurring,
        Cohort::Generation(generation),
    )
    .expect("a grandfathered eligibility pairs with a non-none cohort");
    h.prices
        .create_draft(
            &h.scope,
            TENANT,
            NewPriceDraft {
                price_id,
                scope_key: key,
                content: publishable_row(amount_minor),
                created_by: ACTOR,
                created_at_utc: at(10),
                correlation_id: CORRELATION,
            },
        )
        .await
        .expect("author the grandfathered row");
    common::schedule_coverage_window(
        &h.provider.conn().expect("conn"),
        &h.scope,
        TENANT,
        price_id,
        stamp(),
    )
    .await;
    let scope = h.scope.clone();
    let (_, outcome) = h
        .provider
        .db()
        .in_transaction::<_, bss_pricing::infra::storage::RepoError, _>(move |txn| {
            Box::pin(async move {
                bss_pricing::infra::storage::repo::price_repo::publish_rows(
                    txn,
                    &scope,
                    TENANT,
                    plan,
                    &[(price_id, RowVersion::new(0))],
                    &bss_pricing::domain::tax_display::RegionTaxReadiness::empty(),
                    None,
                )
                .await
            })
        })
        .await;
    outcome.expect("publish the seeded grandfathered row");
    price_id
}

/// A **stray draft** row: authored, never published, no window scheduled —
/// exactly the shape this suite's module doc explains as the mechanism that
/// fails plan B's aggregate pass. On a different key from every published row
/// this suite seeds, so it collides with nothing.
///
/// The region is `us-east` and has to be **declared**. It was `apac` until
/// 2026-08-20, which `common::declare_fixture_regions` does not declare (its set
/// is `eu`/`EU`/`us`/`US`/`DE`/`us-east`), so plan B's aggregate pass failed on
/// `REGION_UNKNOWN` as well — `RegionsDeclared::evaluate` walks every candidate
/// row — and the module doc's load-bearing explanation was not what the fixture
/// isolated. A maintainer who "fixed" the stray draft by scheduling a window on it
/// would have found the plan still failing for a reason the doc never mentions.
async fn seed_stray_draft(h: &Harness, plan: PlanId, phase: Uuid) {
    h.prices
        .create_draft(
            &h.scope,
            TENANT,
            NewPriceDraft {
                price_id: Uuid::now_v7(),
                scope_key: scope_key(plan, phase, "us-east"),
                content: publishable_row(5_000),
                created_by: ACTOR,
                created_at_utc: at(10),
                correlation_id: CORRELATION,
            },
        )
        .await
        .expect("author the stray draft");
}

/// One draft row on `plan`, on a phase of its own so the key is unique, for the
/// only purpose of being lockable: the repricing journal's `price_id` carries a
/// foreign key into `pricing_price`, and
/// [`a_future_dropped_while_it_is_still_taking_its_locks_releases_them_too`]
/// needs many rows on one run's journal rather than many *priced* rows. Nothing
/// publishes these, and nothing reads their content.
async fn seed_lockable_draft(h: &Harness, plan: PlanId) -> Uuid {
    let price_id = Uuid::now_v7();
    h.prices
        .create_draft(
            &h.scope,
            TENANT,
            NewPriceDraft {
                price_id,
                scope_key: scope_key(plan, Uuid::now_v7(), "eu"),
                content: publishable_row(1_000),
                created_by: ACTOR,
                created_at_utc: at(10),
                correlation_id: CORRELATION,
            },
        )
        .await
        .expect("author a lockable draft");
    price_id
}

fn stamp() -> AuditStamp {
    AuditStamp {
        actor_principal_id: ACTOR,
        recorded_at: at(10),
        correlation_id: CORRELATION,
    }
}

fn apply_stamp() -> AuditStamp {
    AuditStamp {
        actor_principal_id: ACTOR,
        recorded_at: at(12),
        correlation_id: CORRELATION,
    }
}

/// [`api::rest::repricing_runs::frozen_report`]'s exact wire shape — this
/// suite's only way to hand [`apply_run_in`] an adjustment and a changeover,
/// since it takes neither directly and reads both off the run's own stored
/// report (`adjustment_of_report`/`changeover_of_report`, `pub(crate)` to that
/// module and unreachable from an integration test).
fn report() -> serde_json::Value {
    serde_json::json!({
        "selector": serde_json::Value::Null,
        "adjustment": {
            "adjustment_kind": "discount",
            "magnitude_kind": "percent_bp",
            "adjustment_value": 500,
            "amounts": {},
        },
        "changeover": format_rfc3339(changeover()),
        "selected": 0,
    })
}

/// Open a run already `committing`, with `price_ids` frozen into its journal
/// `pending` — [`open_repricing_run`]'s own two writes, built directly rather
/// than through HTTP: this suite's subject is the apply, not the freeze.
async fn open_committing_run(h: &Harness, price_ids: &[Uuid]) -> Uuid {
    open_committing_run_reporting(h, price_ids, report()).await
}

/// [`open_committing_run`] under an adjustment the caller chooses.
async fn open_committing_run_reporting(
    h: &Harness,
    price_ids: &[Uuid],
    report: serde_json::Value,
) -> Uuid {
    let operation_id = Uuid::now_v7();
    let conn = h.provider.conn().expect("conn");
    bulk_repo::open(
        &conn,
        &h.scope,
        NewBulkOperation {
            operation_id,
            tenant_id: TENANT,
            kind: BulkKind::Repricing,
            client_key: operation_id.to_string(),
            request_hash: IdempotencyGate::payload_hash(&operation_id.to_string()),
            report: report.clone(),
            submitted_by: ACTOR,
            submitted_at: at(11),
        },
    )
    .await
    .expect("open the run");
    let rows: Vec<NewJournalRow> = price_ids
        .iter()
        .map(|&price_id| NewJournalRow {
            run_id: operation_id,
            price_id,
            tenant_id: TENANT,
        })
        .collect();
    repricing_journal_repo::open_rows(&conn, &h.scope, &rows)
        .await
        .expect("freeze the journal");
    bulk_repo::advance(
        &conn,
        &h.scope,
        TENANT,
        operation_id,
        BulkState::Validating,
        BulkState::Committing,
        report,
        at(11),
    )
    .await
    .expect("enter committing");
    operation_id
}

async fn journal_state(
    h: &Harness,
    run_id: Uuid,
    price_id: Uuid,
) -> (JournalState, Option<String>, Option<Uuid>) {
    let rows = repricing_journal_repo::list_for_run(
        &h.provider.conn().expect("conn"),
        &h.scope,
        TENANT,
        run_id,
    )
    .await
    .expect("read the journal");
    let row = rows
        .into_iter()
        .find(|row| row.price_id == price_id)
        .expect("the row is on this run's journal");
    (row.state, row.failure_reason, row.applied_price_id)
}

// ---------------------------------------------------------------------------
// Step 1's atomicity test.
// ---------------------------------------------------------------------------

/// The M-6 refusal on the **apply path**, which is the half nothing drove
/// (review F7, 2026-08-19).
///
/// `projection_out_of_range` has a unit test in `domain::repricing_tests` and the
/// `if` that calls it in `apply_rows_in` had none: deleting the refusal left the
/// whole suite green while the defect it exists for — a `graduated` ladder under a
/// markup that overflows one band and not another, committed `applied` with one
/// band moved and its sibling at its published rate — was live again. A unit test
/// of the callee cannot see that nobody calls it.
#[tokio::test]
async fn a_markup_that_overflows_one_band_fails_the_whole_plan_rather_than_moving_the_other() {
    let h = harness().await;

    let plan = Uuid::now_v7();
    let phase = Uuid::now_v7();
    seed_plan(&h, plan, phase).await;
    let ladder = seed_published_content(
        &h,
        usage_scope_key(PlanId::new(plan), phase, "eu"),
        publishable_graduated_row(),
    )
    .await;
    // A second, ordinary row on the same plan, **in the ladder's own market**:
    // D-134's unit is the plan, so the refusal has to take this one down with it,
    // and a plan selling usage in `eu` with no recurring base row there fails the
    // aggregate pass instead — which would make this case pass for the wrong
    // reason.
    let flat = seed_published_row(&h, PlanId::new(plan), phase, "eu", 12_000).await;

    // The fat-fingered extra six digits nothing in the authoring path refuses.
    let run_id =
        open_committing_run_reporting(&h, &[ladder, flat], markup_report(1_000_000_000_000)).await;

    let outcome = apply_run_in(
        &h.provider,
        &h.policies,
        &(Arc::clone(&h.registry) as Arc<dyn CatalogVersionRegistryV1>),
        &ctx(),
        &h.scope,
        TENANT,
        run_id,
        apply_stamp(),
        // No compensation lane in this suite: the guard's fallback is what a
        // cancellation here exercises, which is the shape a process with no
        // `bss-pricing` lifecycle running gets.
        None,
    )
    .await
    .expect("apply_run_in itself does not fail - only a plan's own rows do");

    assert_eq!(outcome.applied, 0, "nothing applied: {outcome:?}");
    assert_eq!(outcome.failed, 2, "the plan is the unit: {outcome:?}");

    let (ladder_state, ladder_reason, ladder_applied) = journal_state(&h, run_id, ladder).await;
    assert_eq!(ladder_state, JournalState::Failed);
    assert!(
        ladder_applied.is_none(),
        "no successor stands on the ladder's key - a written successor is the unauthored ladder \
         itself"
    );
    let reason = ladder_reason.expect("a failed row carries a reason");
    assert!(
        reason.contains("leaves the representable range"),
        "the refusal names why, so an operator can re-run with a smaller magnitude: {reason}"
    );

    let (flat_state, _, flat_applied) = journal_state(&h, run_id, flat).await;
    assert_eq!(
        flat_state,
        JournalState::Failed,
        "the plan's other row fails with it (D-134): a partial plan is the one outcome forbidden"
    );
    assert!(flat_applied.is_none());
}

/// The **positive control** for the case above: the same ladder under an ordinary
/// markup applies both bands.
///
/// Without it the refusal would pass against an apply that refused every
/// `graduated` row, which is the shape that makes a guard read as coverage.
#[tokio::test]
async fn the_same_ladder_under_an_ordinary_markup_applies() {
    let h = harness().await;

    let plan = Uuid::now_v7();
    let phase = Uuid::now_v7();
    seed_plan(&h, plan, phase).await;
    let ladder = seed_published_content(
        &h,
        usage_scope_key(PlanId::new(plan), phase, "eu"),
        publishable_graduated_row(),
    )
    .await;

    // The recurring base row the aggregate pass requires of a plan selling in this
    // market; not selected for repricing, so it changes nothing this case reads.
    seed_published_row(&h, PlanId::new(plan), phase, "eu", 12_000).await;

    let run_id = open_committing_run_reporting(&h, &[ladder], markup_report(500)).await;

    let outcome = apply_run_in(
        &h.provider,
        &h.policies,
        &(Arc::clone(&h.registry) as Arc<dyn CatalogVersionRegistryV1>),
        &ctx(),
        &h.scope,
        TENANT,
        run_id,
        apply_stamp(),
        // No compensation lane in this suite: the guard's fallback is what a
        // cancellation here exercises, which is the shape a process with no
        // `bss-pricing` lifecycle running gets.
        None,
    )
    .await
    .expect("apply_run_in itself does not fail");

    let (state, reason, applied) = journal_state(&h, run_id, ladder).await;
    assert_eq!(
        outcome.applied, 1,
        "an ordinary markup computes on both bands and must not be refused: {outcome:?}; the \
         journal says {state:?} / {reason:?}"
    );
    assert_eq!(state, JournalState::Applied);
    assert!(reason.is_none());
    let successor = applied.expect("a real successor stands on the key");

    // **The money, read off the successor.** This is the only case in the crate
    // that drives a `graduated` ladder through `apply_run_in`, and until 2026-08-20
    // it asserted the journal state and stopped: an apply that persisted the
    // predecessor's rates unchanged, or wrote the markup to `amount_minor` instead
    // of the bands (the D-311 rate-vs-amount split `domain::repricing::project_row`
    // handles), produces exactly this journal state — money silently not moving on
    // the one model kind whose money lives in bands.
    let stored = h
        .prices
        .find(&h.scope, TENANT, successor)
        .await
        .expect("read the successor")
        .expect("the successor the journal names exists");
    assert_eq!(
        stored
            .row
            .bands
            .iter()
            .map(|band| band.unit_price_rate.nano_minor())
            .collect::<Vec<_>>(),
        vec![1_050_000_000_i64, 105_000_000_000_i64],
        "a 500bp markup moves **both** bands' rates by 5%, band for band: {:?}",
        stored.row.bands
    );
    assert_eq!(
        stored.row.amount_minor, None,
        "and none of it lands in `amount_minor`: a tiered row's money is its bands, and a \
         second priced column would be two competing prices"
    );
}

#[tokio::test]
async fn a_plan_whose_aggregate_pass_fails_applies_none_of_that_plans_rows() {
    let h = harness().await;

    // Plan A: one clean row, nothing else on it. Applies whole.
    let plan_a = Uuid::now_v7();
    let phase_a = Uuid::now_v7();
    seed_plan(&h, plan_a, phase_a).await;
    let row_a = seed_published_row(&h, PlanId::new(plan_a), phase_a, "eu", 9_900).await;

    // Plan B: two clean, selected rows, plus a stray draft on a third key —
    // never selected, never touched, and the reason plan B's aggregate pass
    // fails (see the module doc).
    let plan_b = Uuid::now_v7();
    let phase_b = Uuid::now_v7();
    seed_plan(&h, plan_b, phase_b).await;
    let row_b1 = seed_published_row(&h, PlanId::new(plan_b), phase_b, "eu", 10_000).await;
    let row_b2 = seed_published_row(&h, PlanId::new(plan_b), phase_b, "us", 12_000).await;
    seed_stray_draft(&h, PlanId::new(plan_b), phase_b).await;

    let run_id = open_committing_run(&h, &[row_a, row_b1, row_b2]).await;

    let outcome = apply_run_in(
        &h.provider,
        &h.policies,
        &(Arc::clone(&h.registry) as Arc<dyn CatalogVersionRegistryV1>),
        &ctx(),
        &h.scope,
        TENANT,
        run_id,
        apply_stamp(),
        // No compensation lane in this suite: the guard's fallback is what a
        // cancellation here exercises, which is the shape a process with no
        // `bss-pricing` lifecycle running gets.
        None,
    )
    .await
    .expect("apply_run_in itself does not fail - only a plan's own rows do");

    assert_eq!(outcome.applied, 1, "plan A's one row: {outcome:?}");
    assert_eq!(
        outcome.failed, 2,
        "a partial plan is the one outcome D-134 forbids - plan B's whole selection fails: \
         {outcome:?}"
    );

    let (state_a, reason_a, applied_price_id_a) = journal_state(&h, run_id, row_a).await;
    assert_eq!(state_a, JournalState::Applied, "plan A applies whole");
    assert!(reason_a.is_none());
    assert!(
        applied_price_id_a.is_some(),
        "a real successor exists for row A"
    );

    let (state_b1, reason_b1, applied_b1) = journal_state(&h, run_id, row_b1).await;
    let (state_b2, reason_b2, applied_b2) = journal_state(&h, run_id, row_b2).await;
    assert_eq!(
        state_b1,
        JournalState::Failed,
        "plan B's aggregate pass refused it: no partial plan"
    );
    assert_eq!(state_b2, JournalState::Failed);
    assert!(
        applied_b1.is_none(),
        "plan B applied nothing, not even partially"
    );
    assert!(applied_b2.is_none());

    let reasons: BTreeSet<String> = [reason_b1, reason_b2]
        .into_iter()
        .map(|reason| reason.expect("a failed row carries a reason"))
        .collect();
    assert_eq!(
        reasons.len(),
        1,
        "every row of the plan fails with the shared reason: {reasons:?}"
    );
    let reason = reasons.into_iter().next().expect("exactly one");
    assert!(
        reason.contains("WINDOW_COVERAGE_MISSING"),
        "the aggregate-only rule the stray draft trips: {reason}"
    );
    assert!(
        !reason.contains("REGION_UNKNOWN"),
        "and **only** that rule: the stray draft's region was `apac` until 2026-08-20, which \
         `declare_fixture_regions` does not declare, so `RegionsDeclared` refused the plan \
         too and the module doc's explanation was not what this fixture isolated: {reason}"
    );

    // Plan A's row genuinely left the published plane and a real successor
    // stands on its key — the read that would catch a rollback silently
    // reaching plan A's writes too.
    let clean_plan_rows = bss_pricing::infra::storage::repo::price_repo::load_for_plan(
        &h.provider.conn().expect("conn"),
        &h.scope,
        TENANT,
        PlanId::new(plan_a),
        &[
            bss_pricing::domain::lifecycle::LifecycleState::Published,
            bss_pricing::domain::lifecycle::LifecycleState::Superseded,
        ],
    )
    .await
    .expect("read plan A's rows");
    assert_eq!(
        clean_plan_rows
            .iter()
            .filter(
                |r| r.lifecycle_state == bss_pricing::domain::lifecycle::LifecycleState::Published
            )
            .count(),
        1,
        "plan A holds exactly one published row - the successor: {clean_plan_rows:?}"
    );
    assert_eq!(
        clean_plan_rows
            .iter()
            .filter(
                |r| r.lifecycle_state == bss_pricing::domain::lifecycle::LifecycleState::Superseded
            )
            .count(),
        1,
        "and the predecessor, superseded rather than gone: {clean_plan_rows:?}"
    );

    // Plan B's two selected rows are untouched: still published, under their
    // original ids, nothing superseded — the rollback the plan's own
    // transaction performed.
    let broken_plan_rows = bss_pricing::infra::storage::repo::price_repo::load_for_plan(
        &h.provider.conn().expect("conn"),
        &h.scope,
        TENANT,
        PlanId::new(plan_b),
        &[bss_pricing::domain::lifecycle::LifecycleState::Published],
    )
    .await
    .expect("read plan B's published rows");
    let broken_plan_published: BTreeSet<Uuid> =
        broken_plan_rows.iter().map(|r| r.price_id).collect();
    assert!(
        broken_plan_published.contains(&row_b1) && broken_plan_published.contains(&row_b2),
        "plan B's rows still stand under their own ids - the transaction rolled back whole: \
         {broken_plan_rows:?}"
    );

    // ------------------------------------------------------------------
    // Step 5's idempotency test, over this exact run: applied and failed
    // rows both already decided.
    // ------------------------------------------------------------------
    let redrive = apply_run_in(
        &h.provider,
        &h.policies,
        &(Arc::clone(&h.registry) as Arc<dyn CatalogVersionRegistryV1>),
        &ctx(),
        &h.scope,
        TENANT,
        run_id,
        apply_stamp(),
        // No compensation lane in this suite: the guard's fallback is what a
        // cancellation here exercises, which is the shape a process with no
        // `bss-pricing` lifecycle running gets.
        None,
    )
    .await
    .expect("a redrive over an already-decided journal does not fail");

    assert_eq!(
        redrive, outcome,
        "a re-run over a journal containing applied rows applies nothing twice and answers \
         the same outcome"
    );
    let (_, _, applied_price_id_a_again) = journal_state(&h, run_id, row_a).await;
    assert_eq!(
        applied_price_id_a_again, applied_price_id_a,
        "the same successor, not a second one minted by a double-apply"
    );
    let clean_plan_rows_again = bss_pricing::infra::storage::repo::price_repo::load_for_plan(
        &h.provider.conn().expect("conn"),
        &h.scope,
        TENANT,
        PlanId::new(plan_a),
        &[bss_pricing::domain::lifecycle::LifecycleState::Published],
    )
    .await
    .expect("read plan A's rows again");
    assert_eq!(
        clean_plan_rows_again.len(),
        1,
        "still exactly one published row on plan A's key - a double-apply would mint a second: \
         {clean_plan_rows_again:?}"
    );

    let stored = bulk_repo::read(&h.provider.conn().expect("conn"), &h.scope, TENANT, run_id)
        .await
        .expect("read the run")
        .expect("the run exists");
    assert_eq!(
        stored.state,
        BulkState::CompletedWithConflicts,
        "one plan applied, one failed - a success with conflicts (`inst-bk-phase2`'s reading, \
         carried over from bulk import to repricing): {stored:?}"
    );
}

// ---------------------------------------------------------------------------
// `inst-mp-grandfathered` clause 2 — the apply's own per-row refusal of an
// explicitly-selected grandfathered row (task 6).
// ---------------------------------------------------------------------------

/// A selector that names the eligibility axis outright still expands over
/// `existing_grandfathered` rows and freezes them `pending`
/// (`RunSelector::admits_grandfathered`, `domain::repricing`'s own module doc) —
/// dropping them would be the silent skip the clause forbids. This suite drives
/// the apply directly at the repository seam, exactly as the atomicity test
/// above does, so the journal below is built by hand to carry exactly what such
/// a selector would have frozen: the grandfathered row, `pending`, beside an
/// ordinary row the same run also selected.
///
/// The ordinary row is the positive control the task-6 brief names in as many
/// words: without it, a handler that failed every row on the plan (the shape
/// the plan-wide `Err` used by `adjusts_rate`'s own refusal would produce, since
/// both rows share one plan and one transaction) would pass this test too.
#[tokio::test]
async fn a_grandfathered_row_the_selector_named_explicitly_is_refused_while_the_plans_other_row_applies()
 {
    let h = harness().await;

    let plan = Uuid::now_v7();
    let phase = Uuid::now_v7();
    seed_plan(&h, plan, phase).await;

    let ordinary_row = seed_published_row(&h, PlanId::new(plan), phase, "eu", 9_900).await;
    let grandfathered_row =
        seed_grandfathered_row(&h, PlanId::new(plan), phase, "us", at(1), 5_000).await;

    let run_id = open_committing_run(&h, &[ordinary_row, grandfathered_row]).await;

    let outcome = apply_run_in(
        &h.provider,
        &h.policies,
        &(Arc::clone(&h.registry) as Arc<dyn CatalogVersionRegistryV1>),
        &ctx(),
        &h.scope,
        TENANT,
        run_id,
        apply_stamp(),
        // No compensation lane in this suite: the guard's fallback is what a
        // cancellation here exercises, which is the shape a process with no
        // `bss-pricing` lifecycle running gets.
        None,
    )
    .await
    .expect("apply_run_in itself does not fail over a grandfathered row - only that row does");

    assert_eq!(
        outcome.applied, 1,
        "the run's other row still applies - the positive control: {outcome:?}"
    );
    assert_eq!(
        outcome.failed, 1,
        "the grandfathered row is refused outright, neither silently skipped nor applied: \
         {outcome:?}"
    );

    let (ord_state, ord_reason, ord_applied) = journal_state(&h, run_id, ordinary_row).await;
    assert_eq!(
        ord_state,
        JournalState::Applied,
        "one grandfathered row on the plan does not take the plan's other row down with it"
    );
    assert!(ord_reason.is_none());
    assert!(
        ord_applied.is_some(),
        "a real successor exists for the ordinary row"
    );

    let (gf_state, gf_reason, gf_applied) = journal_state(&h, run_id, grandfathered_row).await;
    assert_eq!(
        gf_state,
        JournalState::Failed,
        "inst-mp-grandfathered clause 2: an explicit attempt to include the class fails that row \
         with a per-row validation error"
    );
    assert!(gf_applied.is_none(), "never repriced");
    let reason = gf_reason.expect("a failed row carries a reason an operator reads");
    assert!(
        reason.to_lowercase().contains("grandfathered"),
        "the reason names why: {reason}"
    );

    // The row itself never moved: still published under its own id, and no
    // successor was authored on its key - proof this is a refusal and not a
    // reprice wearing a different journal state.
    let plan_rows = bss_pricing::infra::storage::repo::price_repo::load_for_plan(
        &h.provider.conn().expect("conn"),
        &h.scope,
        TENANT,
        PlanId::new(plan),
        &[
            bss_pricing::domain::lifecycle::LifecycleState::Published,
            bss_pricing::domain::lifecycle::LifecycleState::Superseded,
        ],
    )
    .await
    .expect("read the plan's rows");
    let gf_after = plan_rows
        .iter()
        .find(|r| r.price_id == grandfathered_row)
        .expect("the grandfathered row still exists");
    assert_eq!(
        gf_after.lifecycle_state,
        bss_pricing::domain::lifecycle::LifecycleState::Published,
        "still published, never superseded: {gf_after:?}"
    );
    assert!(
        !plan_rows
            .iter()
            .any(|r| r.scope_key == gf_after.scope_key && r.price_id != grandfathered_row),
        "no successor was authored on the grandfathered row's own key"
    );
}

/// `inst-mp-grandfathered` clause 1's own half, already built
/// (`RunSelector::admits_grandfathered`) — proven here rather than assumed, so
/// this task does not close clause 2 over an expansion that silently regressed
/// clause 1: a selector that does not name the eligibility axis excludes the
/// `existing_grandfathered` class from the expansion entirely, so no such row
/// is ever frozen into a journal for the apply to see.
#[tokio::test]
async fn a_selector_that_does_not_name_the_eligibility_axis_excludes_grandfathered_rows() {
    let h = harness().await;

    let plan = Uuid::now_v7();
    let phase = Uuid::now_v7();
    seed_plan(&h, plan, phase).await;

    let ordinary_row = seed_published_row(&h, PlanId::new(plan), phase, "eu", 9_900).await;
    let grandfathered_row =
        seed_grandfathered_row(&h, PlanId::new(plan), phase, "us", at(1), 5_000).await;

    let selector = bss_pricing::domain::repricing::RunSelector {
        plan_id: Some(PlanId::new(plan)),
        ..Default::default()
    };
    assert!(
        !selector.admits_grandfathered(),
        "an absent eligibility axis excludes the class - RunSelector's own rule"
    );

    let selected = bss_pricing::infra::storage::repo::price_repo::load_published_for_selector(
        &h.provider.conn().expect("conn"),
        &h.scope,
        TENANT,
        &selector,
    )
    .await
    .expect("expand the selector");

    assert!(
        selected.contains(&ordinary_row),
        "the ordinary row is in scope: {selected:?}"
    );
    assert!(
        !selected.contains(&grandfathered_row),
        "clause 1: an unnamed eligibility axis structurally excludes the grandfathered row - it \
         is never frozen into a journal for the apply to see: {selected:?}"
    );
}

/// The one interaction task 6 newly creates, found by review rather than by this
/// suite's first pass: a plan carrying an explicitly-selected grandfathered row
/// **and** an aggregate-pass failure, together.
///
/// `apply_rows_in`'s new check marks the grandfathered row `failed` **inside**
/// the plan's own transaction, before the aggregate pass runs — the first writer
/// to call `mark_failed` from in there rather than from `apply_run_in`'s
/// pre-existing, separate-transaction catch-all. When the aggregate pass then
/// fails (a stray draft, this suite's own mechanism — see the module doc), the
/// whole transaction rolls back, undoing that in-transaction mark along with
/// everything else: the row reads `pending` again the instant the transaction
/// returns, exactly as if the check had never run. It is the **catch-all**, in
/// its own separate transaction, that has to reach the row a second time. This
/// test pins that it actually does — the row ends `failed`, never left
/// `pending` — and, since the in-transaction mark's own reason was rolled back
/// with it, that the row carries the plan's **shared** aggregate-failure reason
/// rather than the `inst-mp-grandfathered` reason it was marked with the first
/// time. Pinning what actually happens rather than the tidier thing a reader
/// might assume.
#[tokio::test]
async fn a_grandfathered_row_whose_plan_also_fails_its_aggregate_pass_still_ends_failed_with_the_plans_shared_reason()
 {
    let h = harness().await;

    let plan = Uuid::now_v7();
    let phase = Uuid::now_v7();
    seed_plan(&h, plan, phase).await;

    let ordinary_row = seed_published_row(&h, PlanId::new(plan), phase, "eu", 9_900).await;
    let grandfathered_row =
        seed_grandfathered_row(&h, PlanId::new(plan), phase, "us", at(1), 5_000).await;
    // This suite's own mechanism (see the module doc) for tripping the
    // aggregate-only `WINDOW_COVERAGE_MISSING`: a stray draft on a third,
    // untouched key, never selected by this run.
    seed_stray_draft(&h, PlanId::new(plan), phase).await;

    let run_id = open_committing_run(&h, &[ordinary_row, grandfathered_row]).await;

    let outcome = apply_run_in(
        &h.provider,
        &h.policies,
        &(Arc::clone(&h.registry) as Arc<dyn CatalogVersionRegistryV1>),
        &ctx(),
        &h.scope,
        TENANT,
        run_id,
        apply_stamp(),
        // No compensation lane in this suite: the guard's fallback is what a
        // cancellation here exercises, which is the shape a process with no
        // `bss-pricing` lifecycle running gets.
        None,
    )
    .await
    .expect("apply_run_in itself does not fail - only the plan's own rows do");

    assert_eq!(
        outcome.applied, 0,
        "the plan's aggregate pass takes its whole selection down, the grandfathered row \
         included: {outcome:?}"
    );
    assert_eq!(
        outcome.failed, 2,
        "both of this plan's rows end failed: {outcome:?}"
    );

    let (ord_state, ord_reason, ord_applied) = journal_state(&h, run_id, ordinary_row).await;
    let (gf_state, gf_reason, gf_applied) = journal_state(&h, run_id, grandfathered_row).await;

    assert_eq!(ord_state, JournalState::Failed);
    assert!(ord_applied.is_none());

    assert_eq!(
        gf_state,
        JournalState::Failed,
        "the in-transaction mark rolled back with the rest of the plan - the catch-all is what \
         has to reach this row a second time, and it does: the row is never left pending"
    );
    assert!(gf_applied.is_none(), "never repriced");

    let ord_reason = ord_reason.expect("a failed row carries a reason");
    let gf_reason = gf_reason.expect("a failed row carries a reason");
    assert_eq!(
        ord_reason, gf_reason,
        "both of this plan's rows share the one reason the catch-all wrote in its own separate \
         transaction, over both price_ids alike"
    );
    assert!(
        gf_reason.contains("WINDOW_COVERAGE_MISSING"),
        "the plan's real aggregate-pass violation - the shared reason a `Err` propagating out of \
         `apply_plan_in` renders, not the row-local check's own text: {gf_reason}"
    );
    assert!(
        !gf_reason.contains("inst-mp-grandfathered"),
        "the row-local reason the in-transaction check wrote does not survive the rollback that \
         also undid the mark itself, so the row must not still be read as carrying it: {gf_reason}"
    );

    // The grandfathered row itself never moved: still published under its own
    // id, no successor authored on its key at all - proof this is a refusal
    // both times, not a reprice that happened to land on the losing side of a
    // rollback.
    let plan_rows = bss_pricing::infra::storage::repo::price_repo::load_for_plan(
        &h.provider.conn().expect("conn"),
        &h.scope,
        TENANT,
        PlanId::new(plan),
        &[
            bss_pricing::domain::lifecycle::LifecycleState::Published,
            bss_pricing::domain::lifecycle::LifecycleState::Superseded,
        ],
    )
    .await
    .expect("read the plan's rows");
    let gf_after = plan_rows
        .iter()
        .find(|r| r.price_id == grandfathered_row)
        .expect("the grandfathered row still exists");
    assert_eq!(
        gf_after.lifecycle_state,
        bss_pricing::domain::lifecycle::LifecycleState::Published,
        "still published, never superseded: {gf_after:?}"
    );
}

// ---------------------------------------------------------------------------
// Task 7 - the bulk lock and its Drop guard.
// ---------------------------------------------------------------------------

/// Block until `price_id`'s bulk lock is visibly held by `run_id`.
///
/// A test that cancelled the apply *before* the lock was ever taken would prove
/// nothing about the drop guard, so this is the precondition both cancellation
/// tests below establish first.
///
/// # Why the bound counts polls and not seconds
///
/// What this waits for is another task's progress, which no amount of polling
/// makes synchronous - so a bound has to exist, and the question is what it
/// should measure. A wall clock is the wrong thing: it measures the box, not the
/// apply. A 5s clock here was measured failing on a box carrying 24 busy
/// processes while the apply had in fact taken its lock - the clock had simply
/// run out first, and the failure said "never took its bulk lock", which was
/// false. **A poll count measures what the assertion actually means.** Every
/// iteration below sleeps, which hands the runtime a chance to poll every other
/// runnable task, and reads the database, which requires the driver to have made
/// real progress; so [`POLL_BUDGET`] iterations mean the apply had that many
/// opportunities to advance and did not take one. A loaded box makes each
/// iteration slower in wall time and *more* forgiving, never less - which is the
/// direction a bound in a test may err in.
async fn await_lock_taken(h: &Harness, run_id: Uuid, price_id: Uuid) {
    for _ in 0..POLL_BUDGET {
        let held = bulk_repo::lock_holder(
            &h.provider.conn().expect("conn"),
            &h.scope,
            TENANT,
            price_id,
        )
        .await
        .expect("read the lock");
        if held == Some(run_id) {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }
    panic!(
        "the apply never took its bulk lock over {price_id} across {POLL_BUDGET} polls - each one \
         a chance for it to run and a read proving the driver was live, so this is the apply \
         wedged rather than a slow box; this test cannot prove anything about the drop guard \
         without first observing the lock taken"
    );
}

/// How many turns [`await_lock_taken`] hands the runtime before calling the apply
/// it is waiting for wedged.
///
/// **It bounds a wait on the subject under test, never on a compensation.** The
/// apply is a task this file spawned and can see the effects of; a poll that reads
/// the lock table proves the driver made progress, so a loaded box makes each
/// iteration slower in wall time and *more* forgiving. The releases the cancellation
/// cases assert are drained by hand through
/// `RunCompensationWorker::drain_pending` and wait on nothing — a bound over an
/// unowned task is a wall clock however it is spelled, and one here reported a real
/// finding as a slow box.
///
/// Sized for the diagnosis rather than for a duration: the lock appears within one
/// or two polls, and an apply that is wedged takes all of them.
const POLL_BUDGET: u32 = 2_000;

/// The run, and whichever of `locks` is still held, read together.
///
/// **Nothing here waits.** The release the two cancellation cases below assert is
/// driven to completion by `RunCompensationWorker::drain_pending`, which returns
/// only once it has released every request the lane holds - so by the time this
/// reads, the release either happened or was never enqueued, and both are values
/// rather than deadlines. A wait here would poll a **detached** task, one with no
/// handle to join, so its bound is a wall clock wearing a poll count's clothing: on
/// a saturated box a real finding - a lock row genuinely left held - is reported as
/// "the release never ran within 10s", which is a sentence about the box.
async fn read_release_state(
    h: &Harness,
    run_id: Uuid,
    locks: &[Uuid],
) -> (Option<bulk_repo::BulkOperationRecord>, Vec<(Uuid, Uuid)>) {
    let run = bulk_repo::read(&h.provider.conn().expect("conn"), &h.scope, TENANT, run_id)
        .await
        .expect("read the run");
    let mut held = Vec::new();
    for &price_id in locks {
        if let Some(holder) = bulk_repo::lock_holder(
            &h.provider.conn().expect("conn"),
            &h.scope,
            TENANT,
            price_id,
        )
        .await
        .expect("read the lock")
        {
            held.push((price_id, holder));
        }
    }
    (run, held)
}

/// Spend a `committing` run through the abort door and answer the terminal record.
///
/// The half that makes a `Drop` releasing only the lock a **remedy** rather than a
/// run stranded one state earlier: every caller below asserts the run is still
/// `committing`, and then this proves that state has a door out.
async fn abort_the_run(h: &Harness, run_id: Uuid) -> bulk_repo::BulkOperationRecord {
    bss_pricing::infra::repricing::abandon_committing_run(
        &h.provider,
        &h.scope,
        TENANT,
        run_id,
        bss_pricing::infra::repricing::ABORT_NOTE,
        at(13),
    )
    .await
    .expect("an operator aborts a run a cancellation abandoned")
}

/// **The RED this task is about.** `infra::bulk`'s own sibling releases the
/// bulk lock in its `Ok`/`Err` match arms alone (Z8-8/Z9-5) - a shape that
/// misses two of `apply_run_in`'s three abnormal exits: a panic unwinds past a
/// match arm exactly as it unwinds past everything else, and a dropped future
/// - a client disconnect, a shutdown signal, a losing `select!` arm - never
/// runs any of this crate's own code again, match arm or not. This test is
/// the third one: cancel the future genuinely mid-flight (`JoinHandle::abort`,
/// tokio's own mechanism for exactly that shape) after confirming it has
/// already taken its bulk lock, and prove the lock and the run's state both
/// recover anyway. A green test that dropped the future *before* the lock was
/// ever taken would prove nothing about the guard - `await_lock_taken` below
/// exists so this test cannot pass that way.
///
/// # It cancels where its own doc says it does, and only the whole lock set puts it there
///
/// The margin this case is about is the **per-plan loop**: two plans, so the run
/// has real work left between the lock becoming visible and the run being able to
/// finish. Waiting for `row_a`'s lock alone does not put the cancellation there -
/// `bulk_repo::take_locks` writes one independent statement per row, so `row_a`
/// visible means `row_b`'s insert may still be in flight, and the abort then lands
/// inside `take_locks`. That is the *other* case's window, and it leaves this one's
/// assertions unprovable: cancelling a future cancels the await, not a statement
/// the driver already holds, so `row_b`'s insert could land after the guard's own
/// `DELETE` and leave a lock row standing. Measured on a saturated box: a release
/// that deleted one row of two, and a row of that same run held afterwards.
///
/// So it waits for the **whole** lock set. With `take_locks` provably returned,
/// no insert is outstanding, the cancellation is in the loop this case is about,
/// and the release covers everything.
///
/// # The lane, not the detached fallback
///
/// `apply_run_in` is handed a real `RunCompensation` here and the worker is drained
/// by hand. That is what makes the release **observable**: `drain_pending` returns
/// once it has released every request the lane holds, so the assertions below read a
/// finished act rather than poll a deadline for an unowned task nothing can join.
#[tokio::test]
async fn a_future_dropped_mid_apply_releases_its_bulk_lock_and_leaves_the_run_committing() {
    let h = harness().await;

    // Two plans, so there is real work left in the per-plan loop between the
    // moment the locks become visible and the moment the whole run could
    // possibly finish - the margin this test cancels inside.
    let plan_a = Uuid::now_v7();
    let phase_a = Uuid::now_v7();
    seed_plan(&h, plan_a, phase_a).await;
    let row_a = seed_published_row(&h, PlanId::new(plan_a), phase_a, "eu", 9_900).await;

    let plan_b = Uuid::now_v7();
    let phase_b = Uuid::now_v7();
    seed_plan(&h, plan_b, phase_b).await;
    let row_b = seed_published_row(&h, PlanId::new(plan_b), phase_b, "eu", 10_000).await;

    let run_id = open_committing_run(&h, &[row_a, row_b]).await;

    let (lane, mut worker) =
        bss_pricing::infra::repricing::run_compensation_lane(h.provider.clone());

    let provider = h.provider.clone();
    let policies = h.policies.clone();
    let registry: Arc<dyn CatalogVersionRegistryV1> =
        Arc::clone(&h.registry) as Arc<dyn CatalogVersionRegistryV1>;
    let scope = h.scope.clone();

    let handle = tokio::spawn(async move {
        apply_run_in(
            &provider,
            &policies,
            &registry,
            &ctx(),
            &scope,
            TENANT,
            run_id,
            apply_stamp(),
            Some(&lane),
        )
        .await
    });

    // **Both**, so `take_locks` has provably returned and no insert is in flight.
    await_lock_taken(&h, run_id, row_a).await;
    await_lock_taken(&h, run_id, row_b).await;

    // Cancel it. `JoinHandle::abort` is `select!`'s own mechanism for a losing
    // arm - the future stops running at its next await point and every one of
    // its locals, the drop guard included, drops right there.
    handle.abort();
    let joined = handle.await;
    match joined {
        Err(ref e) if e.is_cancelled() => {}
        other => panic!(
            "the task must have been genuinely cancelled mid-flight for this test to prove \
             anything about the drop guard, not merely finished before the abort landed: \
             {other:?}"
        ),
    }

    // The guard's own send, drained to completion. The count is the first
    // assertion: a drop that enqueued nothing is a guard that never ran, which is a
    // different defect from a release that ran and failed.
    assert_eq!(
        worker.drain_pending().await,
        1,
        "the cancelled apply's guard enqueued exactly one lock release"
    );

    let (run, held) = read_release_state(&h, run_id, &[row_a, row_b]).await;
    assert!(
        held.is_empty(),
        "with `take_locks` returned before the cancellation, the release covers the run's whole \
         set: {held:?}"
    );

    // **And the run is still `committing`, deliberately.** A guard that forced the
    // run terminal here would fail every row the loop had not reached, on the
    // argument that a `pending` row under a run nobody can drive again is
    // unreachable. `POST …/repricing-runs/{runId}/abort` is that door, so what a
    // dropped future costs is the lock and nothing else.
    let run = run.expect("the run is there");
    assert_eq!(
        run.state,
        BulkState::Committing,
        "the drop releases the lock and lands nothing: {run:?}"
    );
    assert_eq!(
        journal_state(&h, run_id, row_b).await.0,
        JournalState::Pending,
        "and a row the loop never reached is still pending, which is what lets a second call \
         tell `never reached` from `decided`"
    );

    // The remedy, spent: `committing` is a state an operator can leave.
    let aborted = abort_the_run(&h, run_id).await;
    assert!(
        aborted.state.is_terminal(),
        "the abort door lands the run the cancellation abandoned: {aborted:?}"
    );
    assert!(
        aborted
            .report
            .get(bss_pricing::infra::bulk::ABORTED_MEMBER)
            .is_some(),
        "and stamps the note that tells this operation's replay from an ordinary finish: \
         {aborted:?}"
    );
}

/// **The window the test above deliberately steps over.** That case waits for the
/// run's whole lock set before cancelling, so `bulk_repo::take_locks` has returned
/// and the abort lands in the per-plan loop. This one cancels *inside*
/// `take_locks`, which is where the guard's arming point is the only thing standing
/// between a client disconnect and a lock nothing owns.
///
/// `take_locks` writes **one independent statement per row** on a
/// non-transactional runner — its own doc says so, and says why (a partial set
/// has to be releasable) — so the first lock row is durable while the loop is
/// still awaiting the second insert. Arming the guard after that call therefore
/// leaves a window in which a lock is committed and nothing owns it: a client
/// disconnect there is precisely the "run stuck `committing` until an operator
/// intervenes" outcome the guard was written to prevent.
///
/// This test makes that window wide enough to hit on purpose: forty rows on the
/// journal, and the cancellation fires the moment the **first** of their locks
/// appears, leaving thirty-nine inserts still to run. Forty is what made it
/// deterministic in measurement rather than what looked like enough — eleven runs
/// against a guard armed after `take_locks` failed eleven times, each of them with
/// the apply's cancellation landing inside `take_locks`, and six runs against one
/// armed before it passed six times.
///
/// # What it asserts, and the one thing it must not
///
/// **The guard enqueued a release.** That is the property, it is the whole of what
/// arming-before-the-first-insert buys, and `drain_pending`'s count is a value
/// rather than a deadline: a guard armed after `take_locks` never runs its `Drop`
/// here at all, so it enqueues nothing and the count is `0`.
///
/// **It must not assert that no lock row of the run remains.** Cancelling a future
/// cancels the await, not a statement the driver has already been handed, so one of
/// those thirty-nine inserts can land *after* the release's own `DELETE` — and
/// nothing in the process can order the two. Asserting an empty lock table here is
/// asserting that a race went one way; it was measured going the other way on a
/// saturated box, reported as "the release never ran", and it is a true finding
/// about `take_locks` rather than a defect in the guard. `RunLockGuard`'s own doc
/// carries what the residue costs and why no retry chases it.
///
/// So the invariant this case holds is the one that actually matters and is actually
/// guaranteed: **nothing stays frozen.** Whatever the race leaves, the run is
/// `committing`, and the abort door clears every lock the run holds inside its own
/// transaction, long after the cancelled future is gone. That assertion is read
/// after the abort and is exact.
#[tokio::test]
async fn a_future_dropped_while_it_is_still_taking_its_locks_releases_them_too() {
    let h = harness().await;

    // Draft rows, not published ones: the journal's own foreign key into
    // `pricing_price` is the only thing the ids owe (bare uuids are refused with
    // `code: 787`), and nothing this test reaches ever prices them — the abort
    // lands while `take_locks` is still running, long before `apply_by_plan`.
    // A distinct phase per row is what keeps each scope key unique; the count is
    // what matters, one INSERT each inside `take_locks`, and the abort has to
    // land among them rather than after the last.
    let plan = Uuid::now_v7();
    let phase = Uuid::now_v7();
    seed_plan(&h, plan, phase).await;
    let mut price_ids: Vec<Uuid> = Vec::new();
    for _ in 0..40 {
        price_ids.push(seed_lockable_draft(&h, PlanId::new(plan)).await);
    }
    price_ids.sort_unstable();
    let first_lock = price_ids[0];

    let run_id = open_committing_run(&h, &price_ids).await;

    let (lane, mut worker) =
        bss_pricing::infra::repricing::run_compensation_lane(h.provider.clone());

    let provider = h.provider.clone();
    let policies = h.policies.clone();
    let registry: Arc<dyn CatalogVersionRegistryV1> =
        Arc::clone(&h.registry) as Arc<dyn CatalogVersionRegistryV1>;
    let scope = h.scope.clone();
    let handle = tokio::spawn(async move {
        apply_run_in(
            &provider,
            &policies,
            &registry,
            &ctx(),
            &scope,
            TENANT,
            run_id,
            apply_stamp(),
            Some(&lane),
        )
        .await
    });

    // The *first* lock, so the other 39 inserts are still ahead of the abort.
    await_lock_taken(&h, run_id, first_lock).await;

    handle.abort();
    match handle.await {
        Err(ref e) if e.is_cancelled() => {}
        other => panic!(
            "the task must have been genuinely cancelled mid-flight for this test to prove \
             anything about the drop guard, not merely finished before the abort landed: \
             {other:?}"
        ),
    }

    // **The property.** A guard armed after `take_locks` would not exist yet at the
    // instant of the abort, so its `Drop` would never run and this count would be
    // zero. Nothing here waits on a clock.
    assert_eq!(
        worker.drain_pending().await,
        1,
        "a future cancelled inside `take_locks` still has a guard, and that guard enqueued its \
         release"
    );

    let (run, _) = read_release_state(&h, run_id, &price_ids).await;
    let run = run.expect("the run is there");
    assert_eq!(
        run.state,
        BulkState::Committing,
        "a future cancelled inside `take_locks` loses its locks and nothing else: {run:?}"
    );

    // Nothing stays frozen: whatever the insert-after-DELETE race left behind, the
    // abort clears every lock the run holds, in its own transaction, with the
    // cancelled future long gone. This is the exact assertion; the one above the
    // abort cannot be.
    let aborted = abort_the_run(&h, run_id).await;
    assert!(
        aborted.state.is_terminal(),
        "and the abort door is what ends it: {aborted:?}"
    );
    let (_, held) = read_release_state(&h, run_id, &price_ids).await;
    assert!(
        held.is_empty(),
        "no row of a cancelled-then-aborted run is left locked: {held:?}"
    );
}

// ---------------------------------------------------------------------------
// The abort door and the apply lane, in flight over one run at once.
// ---------------------------------------------------------------------------

/// **An operator's abort landing mid-apply stops the applier at the next plan.**
///
/// `committing` is where a run stands *while* `RunApplyLane` applies it, so
/// `abandon_committing_run` and `apply_run_in` can be in flight over one run at
/// once — and the abort releases D-134's lock rows and decides every row the loop
/// has not reached. An applier that carried on would be writing successors into
/// price rows it no longer holds the lock over, which a third run over the same rows
/// is free to take; nearer to hand, it meets the journal rows the abort froze
/// `failed`, and `mark_applied`'s swap refusal comes back out of `apply_run_in` as
/// an error over a run an operator had ended cleanly.
///
/// So the applier yields, and the two halves below are what that means: the apply
/// answers `Ok` — it stopped, rather than walking into the abort's writes — and
/// every clause the abort wrote still stands, the terminal state, its `completed_at`
/// and the rows it decided.
///
/// # Why ten plans, and what makes the landing observable rather than hoped for
///
/// The abort has to land in a **gap between** the loop's per-plan transactions,
/// which is the only place a competing write can land at all: `sqlite::memory:` is a
/// shared-cache database and a writer that meets an open write transaction waits for
/// it — the account beside `an_ordinary_err_leaves_the_run_committing_with_its_rows_pending`
/// measured that at 451ms. Two plans leave one gap, and a loop already past it is a
/// loop that finished; ten leave nine, and every plan after the landing is one the
/// applier must not touch.
///
/// The precondition is **asserted, never assumed**. A run the loop finished first is
/// terminal, so the abort refuses it with `LIFECYCLE_FORBIDDEN` and the `expect`
/// below fails loudly rather than the case passing on an apply nothing interrupted.
/// `applied < PLANS` is the other half of the same guard: it is the assertion that
/// the applier really did stop short.
#[tokio::test]
async fn an_abort_landing_mid_apply_stops_the_applier_at_the_next_plan() {
    const PLANS: usize = 10;

    let h = harness().await;
    let mut plans: Vec<PlanId> = Vec::new();
    let mut rows: Vec<Uuid> = Vec::new();
    for _ in 0..PLANS {
        let plan = Uuid::now_v7();
        let phase = Uuid::now_v7();
        seed_plan(&h, plan, phase).await;
        rows.push(seed_published_row(&h, PlanId::new(plan), phase, "eu", 9_900).await);
        plans.push(PlanId::new(plan));
    }
    let run_id = open_committing_run(&h, &rows).await;

    let provider = h.provider.clone();
    let policies = h.policies.clone();
    let registry: Arc<dyn CatalogVersionRegistryV1> =
        Arc::clone(&h.registry) as Arc<dyn CatalogVersionRegistryV1>;
    let scope = h.scope.clone();
    let handle = tokio::spawn(async move {
        apply_run_in(
            &provider,
            &policies,
            &registry,
            &ctx(),
            &scope,
            TENANT,
            run_id,
            apply_stamp(),
            None,
        )
        .await
    });

    // The **whole** lock set, so `take_locks` has provably returned and the abort
    // lands in the per-plan loop rather than among its inserts - the neighbouring
    // cancellation case's own reason for waiting on all of them.
    for &row in &rows {
        await_lock_taken(&h, run_id, row).await;
    }

    let aborted = bss_pricing::infra::repricing::abandon_committing_run(
        &h.provider,
        &h.scope,
        TENANT,
        run_id,
        bss_pricing::infra::repricing::ABORT_NOTE,
        at(13),
    )
    .await
    .expect(
        "the abort must land while the apply is still in its per-plan loop, or this case proves \
         nothing: a `LIFECYCLE_FORBIDDEN` here is the loop having finished all ten plans first",
    );
    assert!(
        aborted.state.is_terminal(),
        "the operator's own door lands the run: {aborted:?}"
    );

    let outcome = handle
        .await
        .expect("the apply task itself must not panic")
        .expect(
            "the applier must stop where the abort left it rather than fail: an applier that \
             walks on reaches a plan whose journal rows the abort has already frozen `failed`, \
             and `mark_applied`'s swap refusal leaves this an `Err` over a run an operator ended \
             cleanly",
        );

    let conn = h.provider.conn().expect("conn");

    // **Nothing the abort wrote was rewritten.** State and instant together: a run
    // that ended and a run that ended and was then re-landed by the apply that
    // arrived after it are the same state, and only `completed_at` tells them apart.
    let after = bulk_repo::read(&conn, &h.scope, TENANT, run_id)
        .await
        .expect("read the run")
        .expect("the run is there");
    assert_eq!(
        after.state, aborted.state,
        "the applier re-landed a run an operator had ended: {after:?}"
    );
    assert_eq!(
        after.completed_at, aborted.completed_at,
        "the instant the abort stamped stands: {after:?}"
    );
    assert!(
        after
            .report
            .get(bss_pricing::infra::bulk::ABORTED_MEMBER)
            .is_some(),
        "and the note that tells this operation's replay from an ordinary finish: {:?}",
        after.report
    );

    // The journal: every row decided, and the ones the loop never reached decided by
    // the **abort** rather than by a plan failure the applier invented on its way
    // out.
    let journal = repricing_journal_repo::list_for_run(&conn, &h.scope, TENANT, run_id)
        .await
        .expect("read the journal");
    assert_eq!(
        journal.len(),
        PLANS,
        "one journal row per plan: {journal:?}"
    );
    let applied = journal
        .iter()
        .filter(|row| row.state == JournalState::Applied)
        .count();
    assert!(
        applied < PLANS,
        "the abort landed while the loop still had plans to reach, so the applier must have \
         stopped short of all ten: {journal:?}"
    );
    for row in &journal {
        match row.state {
            JournalState::Applied => {}
            JournalState::Failed => assert!(
                row.failure_reason
                    .as_deref()
                    .unwrap_or_default()
                    .contains("an operator aborted this mass-repricing run"),
                "a row the applier never reached carries the abort's own reason, not a rule \
                 refusal the yield invented: {row:?}"
            ),
            JournalState::Pending => panic!(
                "no row may be left `pending` under a terminal run - nothing can ever reach it \
                 there: {row:?}"
            ),
        }
    }
    assert_eq!(
        outcome.applied, applied,
        "and the apply answers the journal as it now stands rather than its own count: {outcome:?}"
    );

    // The lock table, which is the exclusion a third run over these same rows would
    // take the moment the abort released it.
    for &row in &rows {
        assert_eq!(
            bulk_repo::lock_holder(&conn, &h.scope, TENANT, row)
                .await
                .expect("read the lock"),
            None,
            "no lock row of this run is left standing over {row}"
        );
    }

    // **The harm, read off the published plane.** A successor stands for exactly the
    // rows the journal says applied - so the applier published nothing under a run
    // an operator had already ended.
    let mut superseded = 0usize;
    for &plan in &plans {
        let plan_rows = bss_pricing::infra::storage::repo::price_repo::load_for_plan(
            &conn,
            &h.scope,
            TENANT,
            plan,
            &[
                bss_pricing::domain::lifecycle::LifecycleState::Published,
                bss_pricing::domain::lifecycle::LifecycleState::Superseded,
            ],
        )
        .await
        .expect("read the plan's rows");
        superseded += plan_rows
            .iter()
            .filter(|r| {
                r.lifecycle_state == bss_pricing::domain::lifecycle::LifecycleState::Superseded
            })
            .count();
    }
    assert_eq!(
        superseded, applied,
        "one superseded predecessor per applied journal row, and none for a row the abort decided"
    );
}

/// **An ordinary `Err` leaves the run `committing` and its rows `pending`**, and
/// the abort door is what spends that state.
///
/// The one genuine, non-corrupting `Err` this harness can force out of
/// `apply_run_in`: a lock row over one of the run's own targets already held by a
/// **different** run, which `bulk_repo::take_locks` refuses. That refusal sits
/// inside the same `inner` block as everything after it precisely so it takes this
/// exit rather than the drop guard's, and the property is the whole of D1's `Err`
/// arm - the run is not force-landed, the rows are not force-failed, and what the
/// exit preserves is now spendable.
///
/// **The other two shapes are not constructible here**, and the comment below this
/// case is the account: a mid-loop `Err` needs a storage failure this harness cannot
/// inject (a plan whose own rules refuse is absorbed into its journal rows and
/// returns `Ok`), and racing a direct write against the run's in-flight processing
/// loses reliably against `sqlite::memory:`'s shared-cache lock waits.
///
/// The positive control is the foreign lock itself: it is still held afterwards, so
/// the release that ran was this run's own and not a sweep of the table.
#[tokio::test]
async fn an_ordinary_err_leaves_the_run_committing_with_its_rows_pending() {
    let h = harness().await;
    let plan = Uuid::now_v7();
    let phase = Uuid::now_v7();
    seed_plan(&h, plan, phase).await;
    let row = seed_published_row(&h, PlanId::new(plan), phase, "eu", 10_000).await;
    let run_id = open_committing_run(&h, &[row]).await;

    // A second run holding the lock over this run's only target.
    let squatter = Uuid::now_v7();
    let conn = h.provider.conn().expect("conn");
    bulk_repo::open(
        &conn,
        &h.scope,
        NewBulkOperation {
            operation_id: squatter,
            tenant_id: TENANT,
            kind: BulkKind::Repricing,
            client_key: squatter.to_string(),
            request_hash: IdempotencyGate::payload_hash(&squatter.to_string()),
            report: report(),
            submitted_by: ACTOR,
            submitted_at: at(11),
        },
    )
    .await
    .expect("open the squatting run");
    bulk_repo::advance(
        &conn,
        &h.scope,
        TENANT,
        squatter,
        BulkState::Validating,
        BulkState::Committing,
        report(),
        at(11),
    )
    .await
    .expect("the squatter enters committing");
    bulk_repo::take_locks(&conn, &h.scope, TENANT, squatter, &[row], at(11))
        .await
        .expect("the squatter takes the lock");

    let outcome = apply_run_in(
        &h.provider,
        &h.policies,
        &(Arc::clone(&h.registry) as Arc<dyn CatalogVersionRegistryV1>),
        &ctx(),
        &h.scope,
        TENANT,
        run_id,
        apply_stamp(),
        None,
    )
    .await;
    // `RepoError::BulkRowLocked` maps onto `CONCURRENT_MUTATION` (`repo_failure`),
    // which is a 409 asking the caller to re-read and retry - the right answer for a
    // row a live run holds.
    let Err(DomainError::ConcurrentMutation(detail)) = &outcome else {
        panic!("a target row another run holds is refused `CONCURRENT_MUTATION`: {outcome:?}");
    };
    assert!(
        detail.contains(&squatter.to_string()),
        "and `fr-concurrent-edit` needs the holder named: {detail}"
    );

    let run = bulk_repo::read(&conn, &h.scope, TENANT, run_id)
        .await
        .expect("read the run")
        .expect("the run is there");
    assert_eq!(
        run.state,
        BulkState::Committing,
        "an ordinary `Err` does not force the run terminal - that would freeze every unreached \
         plan `failed` with no remedy: {run:?}"
    );
    assert_eq!(
        journal_state(&h, run_id, row).await.0,
        JournalState::Pending,
        "and it does not force its rows failed either"
    );
    assert_eq!(
        bulk_repo::lock_holder(&conn, &h.scope, TENANT, row)
            .await
            .expect("read the lock"),
        Some(squatter),
        "positive control: the release that ran was this run's own, so the other run's lock \
         still stands"
    );

    // And the state the exit preserved is spendable.
    let aborted = abort_the_run(&h, run_id).await;
    assert!(
        aborted.state.is_terminal(),
        "the abort door ends a run an ordinary `Err` left committing: {aborted:?}"
    );
    assert_eq!(
        journal_state(&h, run_id, row).await.0,
        JournalState::Failed,
        "and the abort is where a row the apply never reached is decided - by an operator's own \
         act, not inferred from a dropped future"
    );
}

// A test for the narrowed ordinary-`Err` behaviour lives beside
// `release_lock_after_ordinary_failure` itself, in `src/infra/repricing.rs`'s
// own `#[cfg(test)]` module (`step0_probe`'s own precedent in this file) —
// not here. Forcing a genuine, non-corrupting `Err` out of `apply_run_in`
// requires racing a direct write against the run's own in-flight processing,
// and this crate's `sqlite::memory:` harness serialises that race away: a
// competing write from a second task can only ever land in the narrow gaps
// *between* the run's transactions, never during one.
//
// **The reason is not a single-connection pool**, which is what this comment
// used to say and what a later reader would have reasoned from. `connect_db`'s
// SQLite arm applies `ConnectOpts::default()` as given — `max_conns: Some(10)`
// (`libs/toolkit-db/src/pool_opts.rs`), with no in-memory special case like the
// config-driven path's (`options.rs`, `max_connections(1)` for a memory DSN) —
// so this harness really does open a second connection, and it was measured
// doing so. What holds the property up instead is one layer down: `sqlx` rewrites
// `sqlite::memory:` into a *named, shared-cache* database, so those connections
// share one store, and a reader or writer that meets an open write transaction's
// table lock **waits** for it (`sqlite3_unlock_notify`) rather than erroring or
// proceeding. Measured, not argued: a read issued from a second connection while
// a transaction held a written table returned only when that transaction ended,
// 451ms later. An earlier version of
// this test tried exactly that race and lost it reliably — the raced row was
// already decided by the time the direct write ran, not merely flaky. The
// property is real and still worth pinning; it is pinned deterministically
// instead, against `release_lock_after_ordinary_failure` directly.

/// Retire the plan's current revision, the way a committed retirement does: the
/// `plan` row's `lifecycle_state` and nothing else.
///
/// **That narrowness is the whole of C3-2.** `retire_revision` does not touch
/// `pricing_price`, so every price row of a retired plan still reads `published`
/// and every reader that keys on the row's own state still finds it.
async fn retire_plan(h: &Harness, plan: PlanId) {
    let revision = bss_pricing::infra::storage::repo::plan_repo::load_current(
        &h.provider.conn().expect("conn"),
        &h.scope,
        TENANT,
        plan,
    )
    .await
    .expect("read the current revision")
    .expect("the plan has one")
    .revision;
    let scope = h.scope.clone();
    let (_, outcome) = h
        .provider
        .db()
        .in_transaction::<_, bss_pricing::infra::storage::RepoError, _>(move |txn| {
            Box::pin(async move {
                bss_pricing::infra::storage::repo::plan_repo::retire_revision(
                    txn, &scope, TENANT, plan, revision,
                )
                .await
            })
        })
        .await;
    outcome.expect("retire the seeded plan revision");
}

#[tokio::test]
async fn a_retired_plan_takes_no_repricing_successor_and_no_version_handle() {
    // **C3-2 on the fourth door.** `refuse_unpublishable_predecessor` was hoisted by
    // `publish::commit` and `supersede_in` and by neither the cutover nor this
    // aggregate. Both resolve the plan through `plan_repo::load_current`, which
    // answers `published` **or** `retired` without distinguishing them, and neither
    // asked `can_transition(Superseded)`.
    //
    // Nothing beneath refuses either: `commit_supersession_rows` runs
    // `refuse_mispaired` / `supersede_row` / `publish_rows` and none of the three
    // reads the plan's lifecycle, while `RepoError::NoSuccessorRevision` — the source
    // of `PLAN_RETIRED_NO_SUCCESSOR` — comes only from the revision-opening path a
    // repricing apply never takes. So the run superseded published rows on a plan
    // nobody may buy and took a `CatalogVersion` handle for them.
    //
    // The handle is the sharper half. The refusal is **permanent**, so a request made
    // past it stands pending forever and trips `pricing.catalogversion.commit_overdue`
    // for a publish that can never happen — which is why this is hoisted rather than
    // left to any later door.
    let h = harness().await;

    // The clean plan is the positive control: without it a probe asserting "the run
    // applied nothing" would pass against an apply that had simply broken.
    let clean = Uuid::now_v7();
    let clean_phase = Uuid::now_v7();
    seed_plan(&h, clean, clean_phase).await;
    let clean_row = seed_published_row(&h, PlanId::new(clean), clean_phase, "eu", 9_900).await;

    let retired = Uuid::now_v7();
    let retired_phase = Uuid::now_v7();
    seed_plan(&h, retired, retired_phase).await;
    let retired_row =
        seed_published_row(&h, PlanId::new(retired), retired_phase, "eu", 10_000).await;
    retire_plan(&h, PlanId::new(retired)).await;

    let run_id = open_committing_run(&h, &[clean_row, retired_row]).await;
    let handles_before = h.registry.requested().len();

    let outcome = apply_run_in(
        &h.provider,
        &h.policies,
        &(Arc::clone(&h.registry) as Arc<dyn CatalogVersionRegistryV1>),
        &ctx(),
        &h.scope,
        TENANT,
        run_id,
        apply_stamp(),
        // No compensation lane in this suite: the guard's fallback is what a
        // cancellation here exercises, which is the shape a process with no
        // `bss-pricing` lifecycle running gets.
        None,
    )
    .await
    .expect("apply_run_in itself does not fail - only a plan's own rows do");

    assert_eq!(outcome.applied, 1, "the clean plan's one row: {outcome:?}");
    assert_eq!(outcome.failed, 1, "the retired plan's: {outcome:?}");

    let (state, reason, applied) = journal_state(&h, run_id, retired_row).await;
    assert_eq!(state, JournalState::Failed);
    assert!(applied.is_none(), "no successor stands on a retired plan");
    let reason = reason.expect("a failed row carries a reason");
    // Matched on `refuse_unpublishable_predecessor`'s own sentence rather than on
    // `PLAN_RETIRED_NO_SUCCESSOR`: the journal records the `DomainError`'s
    // rendering, and only a `ValidationFailed` report renders rule codes into it
    // — which is why the sibling case above can match one and this cannot. The
    // clause is the refusal's own and no other refusal on this path produces it.
    assert!(
        reason.contains("can never be superseded"),
        "the hoisted refusal is what answered: {reason}"
    );

    // **One handle, not two.** Asserted as a count against the pre-call reading
    // rather than as "the registry was asked", because the clean plan asks for one
    // legitimately and a probe that only checked for absence would redden on it.
    assert_eq!(
        h.registry.requested().len() - handles_before,
        1,
        "the clean plan's handle and no second one stranded on the retired plan: {:?}",
        h.registry.requested()
    );
}

/// **A run whose report cannot be decoded stays where it was** (C4-5).
///
/// `apply_run_in` took the `awaiting_approval -> committing` edge and *then*
/// parsed the stored report, and both parses are `DomainError::Internal` on a
/// report it cannot decode. The edge is documented as single-spend — its premise
/// rides into the `UPDATE` so "two approvals landing at once cannot both spend
/// this edge" — so a run with an undecodable report was left in `committing`,
/// and by C4-1 `committing` has no door: the abort route is a bulk-import route,
/// and nothing re-drives a repricing run.
///
/// Neither parse reads the store, so moving them above the `advance` costs
/// nothing and turns an unrecoverable state into a failed call over an unchanged
/// run.
#[tokio::test]
async fn an_undecodable_report_does_not_spend_the_approval_edge() {
    let h = harness().await;
    let plan = Uuid::now_v7();
    let phase = Uuid::now_v7();
    seed_plan(&h, plan, phase).await;
    let row = seed_published_row(&h, PlanId::new(plan), phase, "eu", 10_000).await;

    // Opened and parked in `awaiting_approval` — `validating -> awaiting_approval`
    // is `inst-bs-approval`'s own edge, and the one a material run takes through
    // `advance_on_verdict` — carrying a report with no `adjustment` member, the
    // shape `adjustment_of_report` cannot decode.
    let run_id = Uuid::now_v7();
    let conn = h.provider.conn().expect("conn");
    bulk_repo::open(
        &conn,
        &h.scope,
        NewBulkOperation {
            operation_id: run_id,
            tenant_id: TENANT,
            kind: BulkKind::Repricing,
            client_key: run_id.to_string(),
            request_hash: IdempotencyGate::payload_hash(&run_id.to_string()),
            report: report(),
            submitted_by: ACTOR,
            submitted_at: at(11),
        },
    )
    .await
    .expect("open the run");
    repricing_journal_repo::open_rows(
        &conn,
        &h.scope,
        &[NewJournalRow {
            run_id,
            price_id: row,
            tenant_id: TENANT,
        }],
    )
    .await
    .expect("freeze the journal");
    bulk_repo::advance(
        &conn,
        &h.scope,
        TENANT,
        run_id,
        BulkState::Validating,
        BulkState::AwaitingApproval,
        serde_json::json!({ "selector": serde_json::Value::Null, "selected": 0 }),
        at(11),
    )
    .await
    .expect("park the run awaiting approval");

    let outcome = apply_run_in(
        &h.provider,
        &h.policies,
        &(Arc::clone(&h.registry) as Arc<dyn CatalogVersionRegistryV1>),
        &ctx(),
        &h.scope,
        TENANT,
        run_id,
        apply_stamp(),
        // No compensation lane in this suite: the guard's fallback is what a
        // cancellation here exercises, which is the shape a process with no
        // `bss-pricing` lifecycle running gets.
        None,
    )
    .await;
    // **The variant, not `is_err()`.** Every refusal this function can raise is an
    // `Err`, so `is_err()` is green for the two that would mean the opposite of
    // this case: a `NotFound` (the run was never opened) and a `ValidationFailed`
    // (the apply ran and a rule refused a row). `Internal` is what an undecodable
    // report renders as, and the detail names the member that could not be read.
    let Err(DomainError::Internal(detail)) = &outcome else {
        panic!("an undecodable report is an `Internal`, not any other refusal: {outcome:?}");
    };
    assert!(
        detail.contains("adjustment"),
        "and it names the member it could not decode: {detail}"
    );

    let run = bulk_repo::read(&conn, &h.scope, TENANT, run_id)
        .await
        .expect("read the run")
        .expect("the run is there");
    assert_eq!(
        run.state,
        BulkState::AwaitingApproval,
        "the edge is single-spend and must not be spent on a call that could not \
         have applied anything; `committing` has no door back out"
    );

    // And the journal is untouched, so nothing was half-applied under the
    // spent edge either.
    assert_eq!(
        journal_state(&h, run_id, row).await.0,
        JournalState::Pending
    );
}

/// **A key another unit holds refuses the apply, and the refusal does not spend
/// the approval edge** (C4-6).
///
/// `refuse_held_key` is called by `window`, `supersession`, `cutover`,
/// `grandfather` and five sites in `approval`; `infra::repricing` contained no
/// occurrence of it at all, so `inst-co-single-pending` was asked once at run
/// open, in the API layer, and never again.
///
/// **This is the only level the check is reachable from today, and that is the
/// finding's whole priority argument.** Through HTTP it cannot be: a material
/// run's own batch unit registers every selected key in `held_keys` while it
/// pends, so a competing unit's `approval_repo::open` is refused by
/// `uq_pricing_approval_key_pending` before it can create the situation —
/// measured, not assumed, by an earlier version of this case written at the REST
/// layer, which failed at the seed with `PendingKeyHeld`. A non-material run
/// applies in the same request as the open's own check. What is left is the
/// redrive door: a re-drive of a stalled run arrives arbitrarily later, and
/// building it without this check is the shape the five sibling paths treat as a
/// Critical.
///
/// The second assertion is what the check's *placement* is for. It sits above
/// `inst-bs-commit`'s edge, which is single-spend; below it, a refusal would
/// leave the run `committing`, which section 4 gives it no way out of.
#[tokio::test]
async fn a_key_a_pending_unit_holds_refuses_the_apply_without_spending_the_approval_edge() {
    let h = harness().await;
    let plan = Uuid::now_v7();
    let phase = Uuid::now_v7();
    seed_plan(&h, plan, phase).await;
    let row = seed_published_row(&h, PlanId::new(plan), phase, "eu", 10_000).await;

    let run_id = Uuid::now_v7();
    let conn = h.provider.conn().expect("conn");
    bulk_repo::open(
        &conn,
        &h.scope,
        NewBulkOperation {
            operation_id: run_id,
            tenant_id: TENANT,
            kind: BulkKind::Repricing,
            client_key: run_id.to_string(),
            request_hash: IdempotencyGate::payload_hash(&run_id.to_string()),
            report: report(),
            submitted_by: ACTOR,
            submitted_at: at(11),
        },
    )
    .await
    .expect("open the run");
    repricing_journal_repo::open_rows(
        &conn,
        &h.scope,
        &[NewJournalRow {
            run_id,
            price_id: row,
            tenant_id: TENANT,
        }],
    )
    .await
    .expect("freeze the journal");
    bulk_repo::advance(
        &conn,
        &h.scope,
        TENANT,
        run_id,
        BulkState::Validating,
        BulkState::AwaitingApproval,
        report(),
        at(11),
    )
    .await
    .expect("park the run awaiting approval");

    // The key was free when the run opened. An interactive unit takes it before
    // the apply arrives — the redrive window, in one statement.
    let keys = price_repo::load_scope_keys_for_ids(&conn, &h.scope, TENANT, &[row])
        .await
        .expect("the row's own key");
    let (_, key) = keys.into_iter().next().expect("one row, one key");
    approval_repo::open(
        &conn,
        &h.scope,
        NewApproval {
            approval_id: Uuid::now_v7(),
            tenant_id: TENANT,
            subject_ref: audit_repo::plan_revision_ref(PlanId::new(Uuid::now_v7()), 0),
            subject_kind: AuditSubjectKind::PlanRevision,
            content_hash: vec![0u8; 32],
            materiality: serde_json::json!({ "material": true, "reason": "an interactive unit" }),
            held_keys: std::iter::once(key.to_string()).collect(),
        },
        apply_stamp(),
    )
    .await
    .expect("seed a pending interactive unit holding the key");

    let outcome = apply_run_in(
        &h.provider,
        &h.policies,
        &(Arc::clone(&h.registry) as Arc<dyn CatalogVersionRegistryV1>),
        &ctx(),
        &h.scope,
        TENANT,
        run_id,
        apply_stamp(),
        // No compensation lane in this suite: the guard's fallback is what a
        // cancellation here exercises, which is the shape a process with no
        // `bss-pricing` lifecycle running gets.
        None,
    )
    .await;
    // The variant, for the sibling case's reason: `is_err()` cannot tell
    // `inst-co-single-pending`'s refusal from a run that was never opened.
    let Err(DomainError::PendingChangeUnitExists(detail)) = &outcome else {
        panic!(
            "a row whose key another unit holds is refused `PENDING_CHANGE_UNIT_EXISTS`: \
             {outcome:?}"
        );
    };
    assert!(
        detail.contains(&key.to_string()),
        "and the refusal names the key the other unit holds: {detail}"
    );

    let run = bulk_repo::read(&conn, &h.scope, TENANT, run_id)
        .await
        .expect("read the run")
        .expect("the run is there");
    assert_eq!(
        run.state,
        BulkState::AwaitingApproval,
        "and the single-spend edge was not spent on a call that refused: \
         `committing` has no door back out"
    );
    assert_eq!(
        journal_state(&h, run_id, row).await.0,
        JournalState::Pending,
        "nothing was applied under it either"
    );
}

/// **The ordinary-`Err` arm, which is the Critical's fix and had no coverage.**
///
/// `apply_run_in`'s `inner` block can fail after the lock guard is armed, and what
/// that arm does — release the lock, and touch neither the run's state nor its
/// journal — is what leaves the run redrivable. `finish_run`'s force-terminal
/// sweep, the other exit, would land the run terminal and freeze every unreached
/// row `failed`, which is unredrivable: a terminal run can never be handed to
/// `apply_run_in` again. The two assertions below are what tell those exits apart.
///
/// **How the failure is produced.** A second bulk operation holds the lock on the
/// run's only target row, so `take_locks` refuses with `RepoError::BulkRowLocked`
/// — inside `inner`, below the guard. Nothing above it intercepts: the pre-flight
/// `refuse_targets_on_a_held_key` reads *approval* key holds, not bulk locks, so a
/// row a sibling run has locked reaches the apply. This costs no race: the blocker
/// is committed before the call, and the collision is a single statement's answer
/// rather than a window two tasks have to interleave in.
#[tokio::test]
async fn an_ordinary_failure_leaves_the_run_committing_with_its_rows_pending() {
    let h = harness().await;
    let plan = Uuid::now_v7();
    let phase = Uuid::now_v7();
    seed_plan(&h, plan, phase).await;
    let row = seed_published_row(&h, PlanId::new(plan), phase, "eu", 10_000).await;

    let run_id = open_committing_run(&h, &[row]).await;

    let conn = h.provider.conn().expect("conn");
    let blocker = Uuid::now_v7();
    bulk_repo::open(
        &conn,
        &h.scope,
        NewBulkOperation {
            operation_id: blocker,
            tenant_id: TENANT,
            kind: BulkKind::Repricing,
            client_key: blocker.to_string(),
            request_hash: IdempotencyGate::payload_hash(&blocker.to_string()),
            report: report(),
            submitted_by: ACTOR,
            submitted_at: at(11),
        },
    )
    .await
    .expect("open the blocking run");
    // `trg_pricing_bulk_row_lock_custody` refuses a lock row under a run that is
    // not `committing`, so the blocker has to reach that state to hold one.
    bulk_repo::advance(
        &conn,
        &h.scope,
        TENANT,
        blocker,
        BulkState::Validating,
        BulkState::Committing,
        report(),
        at(11),
    )
    .await
    .expect("the blocker enters committing");
    bulk_repo::take_locks(&conn, &h.scope, TENANT, blocker, &[row], at(11))
        .await
        .expect("the blocker takes the lock first");

    let outcome = apply_run_in(
        &h.provider,
        &h.policies,
        &(Arc::clone(&h.registry) as Arc<dyn CatalogVersionRegistryV1>),
        &ctx(),
        &h.scope,
        TENANT,
        run_id,
        apply_stamp(),
        // No compensation lane: this case is about the arm that runs while
        // `apply_run_in`'s own code is still executing, which is exactly the exit
        // the lane's `Drop` fallback does not cover.
        None,
    )
    .await;
    assert!(
        outcome.is_err(),
        "a row another run holds is not a row this apply may take: {outcome:?}"
    );

    let run = bulk_repo::read(&conn, &h.scope, TENANT, run_id)
        .await
        .expect("read the run")
        .expect("the run is there");
    assert_eq!(
        run.state,
        BulkState::Committing,
        "the run stays where a redrive can pick it up; `finish_run` would have landed it terminal"
    );
    assert_eq!(
        journal_state(&h, run_id, row).await.0,
        JournalState::Pending,
        "and its unreached row stays pending; `finish_run`'s straggler sweep would have failed it"
    );
    assert_eq!(
        bulk_repo::lock_holder(&conn, &h.scope, TENANT, row)
            .await
            .expect("read the lock"),
        Some(blocker),
        "the release is scoped to the run that failed, so the blocker keeps its own lock"
    );
}
