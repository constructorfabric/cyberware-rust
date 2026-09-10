//! Publish -> sweep -> pinnable, against a real database.
//!
//! This is the whole of G6's claim, and none of it is provable without a
//! database: every property here is a statement about rows in four tables at
//! once — `pricing_catalog_version_ref`, `pricing_read_model`,
//! `pricing_pin_frontier` and `pricing_outbox` — and about what a transaction
//! does to them together. A unit test over the projector could assert the calls
//! happen; it could not assert that a version reads complete, that a frontier
//! refuses to move, or that a second pass writes nothing.
//!
//! The harness is `sqlite_publish_commit.rs`'s, extended in the one way this
//! group needs: the registry double can be **told what to answer**. G5's double
//! answered `committed_version` with `Ok(None)` under the comment "G6's call,
//! and nothing in this suite makes it"; this one makes it.
//!
//! The G5 seam is not re-asserted here. `sqlite_publish_commit.rs` asserts it
//! on every success case and passes **unedited**, which is a stronger statement
//! than a copy of it would be.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use bss_pricing_sdk::catalog_version_registry::registry_unreachable;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use toolkit_canonical_errors::CanonicalError;

use async_trait::async_trait;
use bss_pricing::config::{JobsConfig, LimitsConfig};
use bss_pricing::domain::concurrency::RowVersion;
use bss_pricing::domain::contracts::{BillingAnchorPolicy, ProrationBasis, ProrationContract};
use bss_pricing::domain::error::DomainError;
use bss_pricing::domain::instant::{format_rfc3339, parse_rfc3339, utc_ymd_hms};
use bss_pricing::domain::lifecycle::LifecycleState;
use bss_pricing::domain::money::{CurrencyCode, MinorAmount};
use bss_pricing::domain::overlay::{
    Adjustment, Disclosure, LineKey, Magnitude, OverlayInterval, OverlayLine, ScopeClass,
    ScopeSelector, ScopeValue, TargetRef, TaxBasis,
};
use bss_pricing::domain::plan::PlanShapePatch;
use bss_pricing::domain::plan_shape::{
    BillingCycle, DescriptorSet, Frequency, PhaseKind, PlanPhase,
};
use bss_pricing::domain::price_record::PriceContent;
use bss_pricing::domain::price_row::{ModelKind, PriceRow};
use bss_pricing::domain::publish::{OverlayPublishUnit, PlanPublishUnit, PublishAuthorization};
use bss_pricing::domain::read_model::{
    OverlayIndexShard, OverlayScopeClass, SubjectKind, SubjectRef,
};
use bss_pricing::domain::scope_key::{
    ChargeKind, Cohort, PhaseId, PlanId, PriceEligibility, Region, ScopeKey,
};
use bss_pricing::domain::window::{WindowInterval, WindowState};
use bss_pricing::infra::fixture_gate::FixtureGate;
use bss_pricing::infra::jobs::readmodel_warm::{ReadModelWarmJob, SweepReport};
use bss_pricing::infra::jobs::window_activation::WindowActivationJob;
use bss_pricing::infra::membership_publish;
use bss_pricing::infra::overlay_publish::OverlayPublishService;
use bss_pricing::infra::publish::PublishService;
use bss_pricing::infra::storage::RepoError;
use bss_pricing::infra::storage::entity::{
    catalog_version_ref, customer_group_taxonomy, outbox, partner_taxonomy, plan, price, read_model,
};
use bss_pricing::infra::storage::migrations::Migrator;
use bss_pricing::infra::storage::repo::catalog_version_ref_repo::RefIdentity;
use bss_pricing::infra::storage::repo::group_membership_repo;
use bss_pricing::infra::storage::repo::overlay_repo::{NewOverlay, OverlayRepo};
use bss_pricing::infra::storage::repo::window_repo::{self, NewWindow};
use bss_pricing::infra::storage::repo::{
    NewMembership, NewPlanDraft, NewPriceDraft, PendingVersionRow, PinFrontierRepo, PlanRepo,
    PlanShapeRepo, PriceRepo, catalog_version_ref_repo, plan_repo,
};
use bss_pricing_sdk::CatalogVersion;
use bss_pricing_sdk::catalog_version_registry::{CatalogVersionRegistryV1, PendingVersionRef};
use time::OffsetDateTime;

use sea_orm::ActiveValue::Set;
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, EntityTrait, Order};
use sea_orm_migration::MigratorTrait;
use std::path::{Path, PathBuf};
use toolkit_db::migration_runner::run_migrations_for_testing;
use toolkit_db::secure::{AccessScope, SecureEntityExt, SecureInsertExt, SecureUpdateExt};
use toolkit_db::{ConnectOpts, DBProvider, DbError, connect_db};
use toolkit_security::SecurityContext;
use uuid::Uuid;

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

// ---------------------------------------------------------------------------
// The registry double, now with an answer for `committed_version`.
// ---------------------------------------------------------------------------

/// A registry that hands out one pending handle per `request_id` and can be
/// told what each handle later commits to.
///
/// `commits` maps a handle to the answers it gives, **in order**; the last
/// answer repeats once the script runs out. That is what lets a case say "not
/// yet, then version 5" and, for the invariant-breach case, "version 5, then
/// version 6" — a registry contradicting itself, which is the one answer the
/// finalize refuses.
#[derive(Default)]
struct RegistryDouble {
    issued: Mutex<HashMap<String, String>>,
    commits: Mutex<HashMap<String, Vec<Option<CatalogVersion>>>>,
    calls: Mutex<HashMap<String, usize>>,
    /// An outage: every `committed_version` call fails with this, which is
    /// **not** the same as `Unconfigured` and must not make the pass inert.
    outage: Mutex<Option<String>>,
    /// A **per-ref** outage — the condition D-163 clause (2) names, where one
    /// ref of a version cannot be resolved while its siblings can.
    unresolvable: Mutex<Vec<String>>,
}

impl RegistryDouble {
    /// Answer `pending_ref` with `version` from now on.
    fn commit(&self, pending_ref: &str, version: u64) {
        self.commits
            .lock()
            .expect("no panics in the double")
            .insert(
                pending_ref.to_owned(),
                vec![Some(CatalogVersion::new(version))],
            );
    }

    /// Fail every lookup — a configured registry that cannot be reached.
    fn fail_with(&self, error: &CanonicalError) {
        *self.outage.lock().expect("no panics in the double") = Some(error.to_string());
    }

    /// Fail lookups of **one** handle, leaving its siblings answerable.
    fn fail_handle(&self, pending_ref: &str) {
        self.unresolvable
            .lock()
            .expect("no panics in the double")
            .push(pending_ref.to_owned());
    }

    /// Let a previously unresolvable handle be answered again.
    fn clear_handle_failures(&self) {
        self.unresolvable
            .lock()
            .expect("no panics in the double")
            .clear();
    }

    /// How many handles have been **requested** at all.
    ///
    /// Distinct from [`Self::calls_for`], which counts resolutions of a handle
    /// that exists. This counts the act that creates one — and it is the only
    /// way to see a `request_version` that a rolled-back transaction did not
    /// undo, because the registry is outside the transaction and a refused
    /// commit's *database* writes vanish either way. A test asserting only that
    /// no ref row survives cannot tell an ordering mistake from a correct
    /// refusal; that exact assertion was measured vacuous by a probe on
    /// 2026-08-08.
    fn issued_count(&self) -> usize {
        self.issued.lock().expect("no panics in the double").len()
    }

    /// How many times this handle has been asked for its committed version.
    fn calls_for(&self, pending_ref: &str) -> usize {
        self.calls
            .lock()
            .expect("no panics in the double")
            .get(pending_ref)
            .copied()
            .unwrap_or(0)
    }

    /// Answer `pending_ref` with a scripted sequence, the last entry repeating.
    fn script(&self, pending_ref: &str, answers: Vec<Option<CatalogVersion>>) {
        self.commits
            .lock()
            .expect("no panics in the double")
            .insert(pending_ref.to_owned(), answers);
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
        pending_ref: &str,
    ) -> Result<Option<CatalogVersion>, CanonicalError> {
        if let Some(reason) = self.outage.lock().expect("no panics in the double").clone() {
            return Err(registry_unreachable(reason));
        }
        if self
            .unresolvable
            .lock()
            .expect("no panics in the double")
            .iter()
            .any(|handle| handle == pending_ref)
        {
            return Err(registry_unreachable(format!(
                "{pending_ref} is unresolvable"
            )));
        }
        let mut calls = self.calls.lock().expect("no panics in the double");
        let seen = calls.entry(pending_ref.to_owned()).or_insert(0);
        let index = *seen;
        *seen += 1;
        let commits = self.commits.lock().expect("no panics in the double");
        let Some(answers) = commits.get(pending_ref) else {
            return Ok(None);
        };
        Ok(answers
            .get(index)
            .copied()
            .unwrap_or_else(|| answers.last().copied().flatten()))
    }
}

// ---------------------------------------------------------------------------
// The harness.
// ---------------------------------------------------------------------------

const TENANT: Uuid = Uuid::from_u128(0x7e_11);
/// A second tenant, for the one property a per-tenant bound has and a
/// cross-tenant one cannot: that one tenant's saturated scan does not defer
/// another tenant's completions (D-163 clause 2).
///
/// **Above** [`TENANT`], because the tenant sweep order is ascending tenant id —
/// so this tenant is swept *after* the saturated one, which is the direction that
/// would fail under a cross-tenant page.
const OTHER_TENANT: Uuid = Uuid::from_u128(0x7e_22);
const ACTOR: Uuid = Uuid::from_u128(0xac_10);
const CORRELATION: Uuid = Uuid::from_u128(0xc0_11);

fn at(hour: u32) -> OffsetDateTime {
    at_min(hour, 0)
}

/// Minute resolution, because two publishes of one pass must be tellable apart.
fn at_min(hour: u32, minute: u32) -> OffsetDateTime {
    utc_ymd_hms(2026, 8, 3, hour, minute, 0)
}

fn ctx_of(tenant: Uuid) -> SecurityContext {
    SecurityContext::builder()
        .subject_id(ACTOR)
        .subject_tenant_id(tenant)
        .build()
        .expect("a subject and a tenant are all a context needs")
}

fn committed_registry_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/corpus/registry.toml")
}

struct Harness {
    plans: PlanRepo,
    shapes: PlanShapeRepo,
    prices: PriceRepo,
    publish: PublishService,
    frontier: PinFrontierRepo,
    job: ReadModelWarmJob,
    registry: Arc<RegistryDouble>,
    scope: AccessScope,
    provider: DBProvider<DbError>,
    /// The in-memory exporter the sweep's alarms are read back through (D-238).
    metrics: bss_pricing::infra::metrics::test_harness::MetricsHarness,
}

async fn harness() -> Harness {
    harness_with(JobsConfig::default()).await
}

/// The harness over the whole migration chain.
///
/// It carried a `stop_before` parameter that withheld a migration and everything
/// after it, so a test could write the rows an older schema's writer produced and
/// let the migration under test meet them. **No caller ever passed one** - the
/// only call site passed `None` - and the doc named a test
/// (`a_membership_ref_written_before_the_pin_existed_sweeps_after_the_backfill`)
/// that exists nowhere in the crate. The partial-chain scenario therefore has no
/// coverage in this suite; the parameter is gone rather than standing as a claim
/// that it does.
async fn harness_with(jobs: JobsConfig) -> Harness {
    let db = connect_db("sqlite::memory:", ConnectOpts::default())
        .await
        .expect("connect in-memory sqlite");
    run_migrations_for_testing(&db, Migrator::migrations())
        .await
        .expect("run migrator");
    let provider = DBProvider::<DbError>::new(db);
    // The tenant declares the regions its rows sell in. `inst-tx-region` is
    // registered in the Foundation rule set and C2 is fail-closed, so a publish
    // by a tenant with an empty region taxonomy is refused — which is correct,
    // and which every fixture here would otherwise trip on a rule none of them
    // is about.
    common::declare_fixture_regions(&provider, TENANT).await;
    common::declare_fixture_regions(&provider, OTHER_TENANT).await;
    let registry = Arc::new(RegistryDouble::default());
    let metrics_harness = bss_pricing::infra::metrics::test_harness::MetricsHarness::new();
    let publish = PublishService::new(
        provider.clone(),
        &LimitsConfig::default(),
        FixtureGate::load(&committed_registry_path()),
        Arc::clone(&registry) as Arc<dyn CatalogVersionRegistryV1>,
    );
    Harness {
        plans: PlanRepo::new(provider.clone()),
        shapes: PlanShapeRepo::new(provider.clone()),
        prices: PriceRepo::new(provider.clone()),
        publish,
        frontier: PinFrontierRepo::new(provider.clone()),
        job: ReadModelWarmJob::new(
            provider.clone(),
            Arc::clone(&registry) as Arc<dyn CatalogVersionRegistryV1>,
            jobs,
        )
        .with_metrics(Arc::new(metrics_harness.metrics())),
        registry,
        scope: AccessScope::for_tenant(TENANT),
        provider,
        metrics: metrics_harness,
    }
}

fn plan_draft_of(tenant: Uuid, plan_id: PlanId, tier: &str) -> NewPlanDraft {
    NewPlanDraft {
        plan_name: None,
        plan_id,
        tenant_id: tenant,
        created_by: ACTOR,
        created_at_utc: at(10),
        sku_id: Some(Uuid::from_u128(0x5_c1)),
        plan_tier: Some(tier.to_owned()),
        billing_cycle: Some(BillingCycle::Recurring),
        frequency: Some(Frequency::Monthly),
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

fn flat_row() -> PriceContent {
    let mut row = PriceRow::new(ChargeKind::Recurring, Some(ModelKind::Flat));
    row.amount_minor = Some(MinorAmount::new(9_900).expect("a non-negative amount"));
    PriceContent {
        row,
        tax_inclusive: false,
        tax_category_ref: None,
        billing_timing: Some("advance".to_owned()),
        // Stated, because this is a **recurring** row and Slice 6's
        // `inst-pi-required` makes the three proration inputs mandatory on one.
        // A fixture that asserts a clean publish needs a row publishable in every
        // respect but the one under judgement, and a row with no proration
        // contract is not.
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

fn scope_key(plan_id: PlanId, phase: PhaseId) -> ScopeKey {
    ScopeKey::new(
        plan_id,
        CurrencyCode::new("EUR").expect("three letters"),
        Region::new("eu").expect("a non-blank region"),
        phase,
        PriceEligibility::AllSubscriptions,
        ChargeKind::Recurring,
        Cohort::None,
    )
    .expect("the class pairs with cohort none")
}

/// A plan the whole rule set passes, plus one flat recurring row.
async fn seed_publishable(h: &Harness, plan_id: PlanId, tier: &str) -> (u64, RowVersion) {
    seed_publishable_of(h, TENANT, plan_id, tier).await
}

/// A grant set carrying a value in every field this plan shape may author.
///
/// `plan_tier_ref` and `plan_level` off their defaults, because a reader that
/// never touched the column emits exactly the empty shell. **`per_phase` stays
/// empty deliberately**: these plans are non-phased - their schedule is only D-19's
/// implicit terminal phase - and `inst-gs-perphase` refuses a per-phase set on one,
/// so an empty map here is the rule's answer rather than an omission.
fn seeded_grants() -> bss_pricing::domain::contracts::EntitlementGrants {
    use bss_pricing::domain::contracts::{EntitlementGrants, GrantSet};
    use std::collections::BTreeMap;
    EntitlementGrants {
        plan_tier_ref: Some("gold".to_owned()),
        plan_level: GrantSet {
            feature_flags: BTreeMap::from([("bss.pricing/api".to_owned(), true)]),
            quotas: BTreeMap::from([("projects".to_owned(), 5)]),
        },
        per_phase: BTreeMap::new(),
    }
}

/// A change contract carrying a rank and D-113's flag off its default.
///
/// **`allowed_change_targets` stays unset**, and that is `inst-pc-targets`: a
/// target is an explicit *published* `planId`, so naming one would need a second
/// published plan seeded per case for a column no case here reads.
fn seeded_change_contract() -> bss_pricing::domain::contracts::PlanChangeContract {
    bss_pricing::domain::contracts::PlanChangeContract {
        allowed_change_targets: None,
        comparability_rank: Some(10),
        usage_counter_on_plan_change:
            bss_pricing::domain::contracts::UsageCounterOnPlanChange::Carry,
    }
}

async fn seed_publishable_of(
    h: &Harness,
    tenant: Uuid,
    plan_id: PlanId,
    tier: &str,
) -> (u64, RowVersion) {
    let scope = AccessScope::for_tenant(tenant);
    let phase = PhaseId::new(Uuid::new_v4());
    let created = h
        .plans
        .create_draft(&scope, plan_draft_of(tenant, plan_id, tier))
        .await
        .expect("create the draft");
    let after_phases = h
        .shapes
        .replace_phases(
            &scope,
            tenant,
            plan_id,
            created.revision,
            created.row_version,
            vec![PlanPhase {
                phase_id: phase,
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
    let after_descriptors = h
        .shapes
        .set_descriptor_set(
            &scope,
            tenant,
            plan_id,
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

    // **The plan-level contract columns, non-default.** `create_draft` cannot carry
    // them, and left unset they project as `null`, `{}` and an empty grant shell -
    // the same three values a projector that never read the table emits, so every
    // assertion about them is satisfied by their absence.
    let after_contract = h
        .plans
        .update_draft(
            &scope,
            tenant,
            plan_id,
            created.revision,
            after_descriptors.row_version,
            bss_pricing::domain::plan::PlanShapePatch {
                entitlement_grants: Some(seeded_grants()),
                change_contract: Some(seeded_change_contract()),
                ..bss_pricing::domain::plan::PlanShapePatch::default()
            },
            stamp(),
        )
        .await
        .expect("author the columns the create form cannot carry");

    let price_id = Uuid::new_v4();
    h.prices
        .create_draft(
            &scope,
            tenant,
            NewPriceDraft {
                price_id,
                scope_key: scope_key(plan_id, phase),
                content: flat_row(),
                created_by: ACTOR,
                created_at_utc: at(10),
                correlation_id: TEST_CORRELATION,
            },
        )
        .await
        .expect("author the price row");

    // `inst-wc-required`: the row does not publish until its key holds an active
    // or scheduled window. Thirty tests in this file reddened when the rule
    // registered.
    //
    // It is `common::schedule_coverage_window`'s interval and not this file's, and
    // that interval ends exactly where [`window_at`]'s scale begins — so §8's
    // window suites build their own chains **adjacent** to it rather than
    // overlapping it, which `window_repo::schedule` would refuse. The consequence
    // is visible and deliberate: every §8 delta carries this leading `scheduled`
    // interval as well as the ones its own case authored, and each of those tests
    // says so.
    common::schedule_coverage_window(
        &h.provider.conn().expect("conn"),
        &scope,
        tenant,
        price_id,
        stamp(),
    )
    .await;

    (created.revision, after_contract.row_version)
}

/// Publish a seeded plan at `now` and hand back the pending handle its commit
/// recorded.
///
/// **The instant is a parameter, and that is not decoration.** An earlier
/// version of this helper committed every publish at one literal instant, so
/// every `requested_at` in the store was identical — and a guard that compared
/// request instants therefore matched every row, which made a whole class of
/// frontier defect unexpressible by this suite. In production the instant is
/// the commit's own `now`. A fixture that cannot tell two publishes apart is a
/// fixture carrying the property the code is supposed to carry.
async fn publish(
    h: &Harness,
    plan_id: PlanId,
    revision: u64,
    version: RowVersion,
    now: OffsetDateTime,
) -> String {
    publish_of(h, TENANT, plan_id, revision, version, now).await
}

async fn publish_of(
    h: &Harness,
    tenant: Uuid,
    plan_id: PlanId,
    revision: u64,
    version: RowVersion,
    now: OffsetDateTime,
) -> String {
    let receipt = h
        .publish
        .commit(
            &ctx_of(tenant),
            &AccessScope::for_tenant(tenant),
            tenant,
            PlanPublishUnit::plan_content(plan_id, revision),
            version,
            PublishAuthorization::auto_publishable(),
            ACTOR,
            CORRELATION,
            now,
        )
        .await
        .expect("the publish commits");
    receipt
        .version_ref()
        .pending_ref()
        .expect("a commit stamps a pending handle, never a version")
        .to_owned()
}

/// Seed a plan, publish it at `now`, and return `(plan_id, pending_ref)`.
async fn seed_and_publish_at(h: &Harness, tier: &str, now: OffsetDateTime) -> (PlanId, String) {
    seed_and_publish_for(h, TENANT, tier, now).await
}

async fn seed_and_publish_for(
    h: &Harness,
    tenant: Uuid,
    tier: &str,
    now: OffsetDateTime,
) -> (PlanId, String) {
    let plan_id = PlanId::new(Uuid::new_v4());
    let (revision, version) = seed_publishable_of(h, tenant, plan_id, tier).await;
    let pending = publish_of(h, tenant, plan_id, revision, version, now).await;
    (plan_id, pending)
}

/// Flip a plan's current revision `published -> retired`.
///
/// By statement rather than through a repository, because **retirement has no
/// writer in this gear**: it is a publish unit of its own (D-128) and the group
/// that lands it is not this one. The flip is one the `pricing_plan` trigger
/// whitelist explicitly sanctions, so this is the state a retired plan is in
/// and not a state reached around the guards.
async fn retire(h: &Harness, plan_id: PlanId) {
    let conn = h.provider.conn().expect("conn");
    let moved = plan::Entity::update_many()
        .secure()
        .scope_with(&h.scope)
        .col_expr(
            plan::Column::LifecycleState,
            sea_orm::sea_query::Expr::value("retired"),
        )
        .filter(
            Condition::all()
                .add(plan::Column::TenantId.eq(TENANT))
                .add(plan::Column::PlanId.eq(plan_id.get()))
                .add(plan::Column::LifecycleState.eq("published")),
        )
        .exec(&conn)
        .await
        .expect("the trigger whitelist sanctions published -> retired");
    assert_eq!(moved.rows_affected, 1, "one current revision was retired");
}

async fn sweep(h: &Harness, now: OffsetDateTime) -> SweepReport {
    h.job.run(now).await.expect("the sweep pass runs")
}

// ---------------------------------------------------------------------------
// Reading the tables back.
// ---------------------------------------------------------------------------

async fn deltas(h: &Harness) -> Vec<read_model::Model> {
    let conn = h.provider.conn().expect("conn");
    read_model::Entity::find()
        .secure()
        .scope_with(&h.scope)
        .order_by(read_model::Column::CatalogVersion, Order::Asc)
        .all(&conn)
        .await
        .expect("read the read model")
}

async fn refs(h: &Harness) -> Vec<catalog_version_ref::Model> {
    let conn = h.provider.conn().expect("conn");
    catalog_version_ref::Entity::find()
        .secure()
        .scope_with(&h.scope)
        .order_by(catalog_version_ref::Column::PendingRef, Order::Asc)
        .all(&conn)
        .await
        .expect("read the version refs")
}

async fn degraded_events(h: &Harness) -> Vec<outbox::Model> {
    let conn = h.provider.conn().expect("conn");
    outbox::Entity::find()
        .secure()
        .scope_with(&h.scope)
        .filter(Condition::all().add(outbox::Column::EventName.eq("PlanPublishDegraded")))
        .all(&conn)
        .await
        .expect("read the outbox")
}

/// D-248: an overlay publish announces itself, and the payload names the shard.
///
/// Until 2026-08-07 this act moved **two** projected subjects — the overlay and its
/// `overlay_index` shard (D-112) — and announced neither, so a consumer polling the
/// outbox learned that plans, prices, bundles and windows had moved and never that
/// the overlay layer applied on top of them had. The name is the fourteenth in
/// `fr-event-contract`'s set and arrived as a decision rather than as a literal in
/// `infra::overlay_publish`, which is the discipline that module was observing when
/// it declined to mint one for itself.
#[tokio::test]
async fn an_overlay_publish_announces_itself_and_names_the_shard_it_moved() {
    let h = harness().await;
    let overlays = OverlayRepo::new(h.provider.clone());
    let price_overlay_id = seed_draft_overlay(&h, 40).await;
    let record = overlays
        .load(&h.scope, TENANT, price_overlay_id, 0)
        .await
        .expect("load")
        .expect("the draft");
    let pin = bss_pricing::domain::approval::content_pin::overlay_content_hash(&record.content());

    let receipt = overlay_publish(&h)
        .commit(
            &ctx_of(TENANT),
            &h.scope,
            TENANT,
            OverlayPublishUnit::new(price_overlay_id, 0),
            PublishAuthorization::approved(Uuid::new_v4(), ACTOR, Uuid::from_u128(0xbb), pin),
            stamp_of(ACTOR, at_min(12, 0)),
        )
        .await
        .expect("an approved overlay publishes");

    let events = events_named(&h, "PriceOverlayPublished").await;
    assert_eq!(
        events.len(),
        1,
        "exactly one announcement per published revision"
    );
    let event = &events[0];

    assert_eq!(
        event.aggregate_id, price_overlay_id,
        "the aggregate is the overlay, not a plan. An overlay is tenant-scoped and may target no \
         plan at all, so there is no plan stream to order it within"
    );
    assert_eq!(
        event.dedup_key,
        bss_pricing::infra::storage::repo::outbox_repo::price_overlay_published_dedup_key(
            price_overlay_id,
            0
        ),
        "the key comes from the module that owns it (Z8-4) rather than being spelled a second \
         time here, and it is revision-scoped because the commit's compare-and-swap refuses the \
         second publish of one revision before the enqueue"
    );
    assert_eq!(
        event.dedup_key,
        format!("PriceOverlayPublished/{price_overlay_id}/0"),
        "rendered from the enum and separated as its ten siblings are. `:` was this event's own \
         spelling, and D-248's prose still quotes it"
    );

    let payload = &event.payload;
    assert_eq!(
        payload["priceOverlayId"]
            .as_str()
            .and_then(|id| Uuid::parse_str(id).ok()),
        Some(price_overlay_id)
    );
    assert_eq!(payload["revision"].as_u64(), Some(0));
    assert_eq!(
        payload["scopeClass"].as_str(),
        Some("partner"),
        "the shard key, so a consumer knows which index document to re-read (D-112)"
    );
    assert_eq!(payload["scopeValue"].as_str(), Some("acme"));
    assert_eq!(payload["precedence"].as_i64(), Some(40));
    assert_eq!(
        payload["pendingVersionRef"].as_str(),
        Some(receipt.pending_ref.as_str()),
        "pending, because the version does not exist yet and will not until the registry batches \
         (D-47)"
    );
}

/// Every outbox row of one event name, for this harness's tenant.
async fn events_named(h: &Harness, name: &str) -> Vec<outbox::Model> {
    let conn = h.provider.conn().expect("conn");
    outbox::Entity::find()
        .secure()
        .scope_with(&h.scope)
        .filter(Condition::all().add(outbox::Column::EventName.eq(name)))
        .all(&conn)
        .await
        .expect("read the outbox")
}

async fn frontier_version(h: &Harness) -> Option<u64> {
    frontier_version_of(h, TENANT).await
}

async fn frontier_version_of(h: &Harness, tenant: Uuid) -> Option<u64> {
    h.frontier
        .read(&AccessScope::for_tenant(tenant), tenant)
        .await
        .expect("read the frontier")
        .map(|frontier| frontier.catalog_version.get())
}

// ---------------------------------------------------------------------------
// 1. The path, end to end.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_committed_publish_becomes_pinnable_after_one_sweep() {
    // The seam closes. Before this group a successful commit left a pending
    // handle and nothing resolvable at all; the assertion at the end is the one
    // G7's e2e inherits, and the only read-side end-to-end statement it can
    // make.
    let h = harness().await;
    let (plan_id, pending) = seed_and_publish_at(&h, "gold", at_min(12, 0)).await;
    h.registry.commit(&pending, 1);

    let report = sweep(&h, at(13)).await;
    assert!(!report.inert);
    assert_eq!(report.versions_projected, 1);
    assert_eq!(report.subjects_projected, 1);
    assert_eq!(report.frontiers_advanced, 1);

    let rows = deltas(&h).await;
    assert_eq!(rows.len(), 1, "one subject, one delta");
    assert_eq!(rows[0].catalog_version, 1);
    assert_eq!(rows[0].subject_kind, "plan");
    assert_eq!(rows[0].subject_ref, plan_id.to_string());
    assert!(rows[0].warm_completed, "the row is written warm");
    assert_eq!(
        rows[0].warm_completed_at,
        Some(at(13)),
        "the marker and its instant move together"
    );
    assert_eq!(
        rows[0].payload.get("planId"),
        Some(&serde_json::json!(plan_id.get()))
    );

    let stored = refs(&h).await;
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].catalog_version, Some(1));
    assert_eq!(
        stored[0].committed_at,
        Some(at(13)),
        "the commit CHECK ties the version to its instant"
    );

    assert_eq!(
        frontier_version(&h).await,
        Some(1),
        "PinFrontierRepo::read now answers, which is what G7's e2e asserts"
    );
}

/// **`inst-cm-frozen`, end to end: what a version freezes, a consumer can read.**
///
/// The instruction has two clauses - the catalog persists and freezes the
/// definition, and Rating computes from the snapshot - and only the first had a
/// mechanism. `content_pin` hashed the composite set from v11 and `approvals.rs`
/// showed it to a reviewer, so a formula was covered by a signature and reached no
/// artifact any consumer reads; `domain::projection` contained the word
/// "composite" zero times. Rating resolves a plan through `pricingSnapshotRef`
/// into one of these delta rows, so a formula outside this payload is one Rating
/// cannot evaluate however carefully it was approved.
///
/// Against the **store** rather than against the renderer: `projection_tests`
/// pins the wire keys off a hand-built delta, which would stay green if the
/// projector never read `pricing_composite_meter` at all. What this asserts is that
/// the definition a publish froze came out of the table the publish rules judged.
///
/// The formula is compared whole, nested object included. It is opaque to this
/// gear (A4), so there is nothing to compare but the document the author wrote.
#[tokio::test]
async fn a_resolved_plan_subjects_delta_freezes_the_composite_definitions() {
    let h = harness().await;
    let plan_id = PlanId::new(Uuid::new_v4());
    let (revision, version) = seed_publishable(&h, plan_id, "gold").await;

    let formula = serde_json::json!({ "op": "weighted_sum", "weights": { "vcpu": 2, "ram": 1 } });
    let composite_id = Uuid::new_v4();
    let attached = h
        .shapes
        .replace_composites(
            &h.scope,
            TENANT,
            plan_id,
            revision,
            version,
            vec![bss_pricing::domain::plan_shape::CompositeMeter {
                composite_id,
                output_unit: "vm".to_owned(),
                constituent_units: vec!["vcpu".to_owned(), "ram".to_owned()],
                formula: formula.clone(),
            }],
            stamp(),
        )
        .await
        .expect("attach the composite set to the open draft");

    let pending = publish_of(
        &h,
        TENANT,
        plan_id,
        revision,
        attached.row_version,
        at_min(12, 0),
    )
    .await;
    h.registry.commit(&pending, 1);
    sweep(&h, at(13)).await;

    let rows = deltas(&h).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].payload.get("composites"),
        Some(&serde_json::json!([{
            "compositeId": composite_id,
            "outputUnit": "vm",
            "constituentUnits": ["vcpu", "ram"],
            "formula": formula,
        }])),
        "{:?}",
        rows[0].payload
    );
}

#[tokio::test]
async fn a_resolved_plan_subjects_delta_stamps_the_cross_boundary_marker() {
    // D-169 clause (1), against the store rather than against the renderer. The
    // marker is a launch-constant tenant-wide value on every resolved `plan`
    // subject row, and the field that used to sit beside it left the contract -
    // so what a version freezes here is machine-readable and derivable by nobody
    // but this gear, and the sentence a human is shown is PRD AC #66's.
    let h = harness().await;
    let (_, pending) = seed_and_publish_at(&h, "gold", at_min(12, 0)).await;
    h.registry.commit(&pending, 1);

    sweep(&h, at(13)).await;

    let rows = deltas(&h).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].payload.get("crossBoundaryChangePolicy"),
        Some(&serde_json::json!("cancel_plus_new")),
        "{:?}",
        rows[0].payload
    );
    assert!(
        !rows[0]
            .payload
            .to_string()
            .to_ascii_lowercase()
            .contains("warningtext"),
        "and no warning text, under any spelling: {:?}",
        rows[0].payload
    );
}

#[tokio::test]
async fn an_overlay_ref_naming_an_overlay_that_is_not_there_is_an_internal_fault() {
    // **This case's reason changed with D-234 and its name and comment changed
    // with it.** It used to read "the other three kinds have no store in this
    // gear - so the projector refuses them by name", and it passed on that
    // ground. The overlay plane has had a store since Slice 9 and a projector
    // since D-234, so what fails here now is not the kind: it is a ref pinning a
    // revision that is not in the table.
    //
    // Left failing rather than deleted, because the outcome it asserts is the
    // one D-128 requires - an empty document is worse than a loud fault, and a
    // consumer resolving an overlay at a pin must never receive a shell.
    let h = harness().await;
    let conn = h.provider.conn().expect("conn");
    catalog_version_ref_repo::record_pending(
        &conn,
        &h.scope,
        PendingVersionRow::for_subject(
            TENANT,
            "pend-overlay".to_owned(),
            &SubjectRef::PriceOverlay(Uuid::new_v4()),
            Some(0),
            Some(LifecycleState::Published),
            at_min(12, 0),
        ),
    )
    .await
    .expect("record an overlay subject's ref");
    h.registry.commit("pend-overlay", 4);

    let report = sweep(&h, at(13)).await;

    assert_eq!(
        report.subjects_failed, 1,
        "a pinned revision that is not there is a fault, not an empty delta"
    );
    assert!(
        deltas(&h).await.is_empty(),
        "and nothing was written for it"
    );
}

#[tokio::test]
async fn an_enrolled_membership_reaches_the_read_model_as_a_published_subject() {
    // Task 5's own claim, proved through the real repository rather than by
    // constructing `MembershipRow`s by hand. **This replaces
    // `a_subject_kind_with_no_store_in_this_gear_is_still_refused_by_name`**,
    // which asserted the opposite of what is now true: `group_membership` was
    // the last kind `subject_of` refused by name (`overlay_index` had already
    // left the case when its own arm landed), and it leaves with this task.
    //
    // Proved by the projection's **content**, not by the hard error's absence -
    // "no error occurred" would pass just as well against a projection that
    // silently wrote nothing.
    //
    // `enroll` itself did not open a publish unit until Task 6's
    // `infra::membership_publish`; the route-level suite
    // (`tests/rest_customer_groups.rs::a_route_level_enrollment_is_a_real_publish_unit`)
    // is what proves that producer end to end. This test stays a lower-level
    // proof of the projector alone: it records the ref directly the way
    // `a_published_overlay_subject_projects_its_document` does, so the
    // projector's own correctness is provable without going through the route
    // and the idempotency gate.
    let h = harness().await;
    let conn = h.provider.conn().expect("conn");
    let payer_tenant_id = Uuid::new_v4();
    let membership_id = Uuid::new_v4();
    let enrolled = group_membership_repo::enroll(
        &conn,
        &h.scope,
        TENANT,
        NewMembership {
            membership_id,
            tenant_id: TENANT,
            payer_tenant_id,
            group_value: "gold".to_owned(),
            effective_from: at(1),
            effective_to: Some(at(9)),
        },
        stamp_of(ACTOR, at(1)),
    )
    .await
    .expect("enroll the payer");

    // Pinned the way `infra::membership_publish` pins it — the row version this
    // mutation produced and the interval end it judged. `for_subject`'s
    // `None, None` was what this seed used until the pin existed, and the
    // projector refuses it now: a membership subject with no pinned version is
    // one whose `effective_to` cannot be told from an unpinned `NULL`.
    catalog_version_ref_repo::record_pending(
        &conn,
        &h.scope,
        PendingVersionRow::for_membership(
            TENANT,
            "pend-membership".to_owned(),
            membership_id,
            enrolled.row_version,
            enrolled.effective_to,
            at_min(12, 0),
        ),
    )
    .await
    .expect("record the membership subject's ref");
    h.registry.commit("pend-membership", 4);

    let report = sweep(&h, at(13)).await;

    assert_eq!(report.subjects_failed, 0);
    assert_eq!(report.subjects_projected, 1);
    let rows = deltas(&h).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].subject_kind, "group_membership");
    assert_eq!(rows[0].subject_ref, membership_id.to_string());
    assert_eq!(rows[0].catalog_version, 4);
    assert_eq!(
        rows[0].payload.get("payerTenantId"),
        Some(&serde_json::json!(payer_tenant_id)),
        "{:?}",
        rows[0].payload
    );
    assert_eq!(
        rows[0].payload.get("groupValue"),
        Some(&serde_json::json!("gold"))
    );
    assert_eq!(
        rows[0].payload.get("effectiveFrom"),
        Some(&serde_json::json!(format_rfc3339(enrolled.effective_from)))
    );
    assert_eq!(
        rows[0].payload.get("effectiveTo"),
        Some(&serde_json::json!(
            enrolled.effective_to.map(format_rfc3339)
        ))
    );
}

/// Two publish units over **one** membership row, and each freezes the state its
/// own commit judged.
///
/// # The interleaving, not a helper in isolation
///
/// `MembershipSubjectDelta`'s doc named this exact sequence as the premise the
/// membership plane was safe under while nothing minted a second publish unit
/// per row — the enroll route and the PATCH route mint one each, so it is minted
/// now. The order below is the whole test: **enroll**, its version committed,
/// **then** the end, its version committed, and only **then** the sweep. Reading
/// the row live at that point answers "ended" for both versions, so the
/// enrollment's version — pinned, INSERT-only, on a ≥ 7-year horizon in a store
/// whose contract is that a completed version never changes — would say the
/// payer's membership had already ended at an instant when it had not, to
/// Tariffs, which resolves the group at `t` against exactly this surface.
///
/// Driven through `infra::membership_publish`, which is what the two routes
/// call, rather than by recording refs by hand: the pin is the *producer's*
/// half, so a test that wrote the ref rows itself would prove the projector
/// against a pin no route ever writes.
///
/// The instants are ordered so that only the freeze can distinguish the two
/// deltas: the enrollment is open-ended (`effectiveTo: null`) and the end
/// narrows it to a real instant.
#[tokio::test]
async fn each_of_two_publish_units_over_one_membership_freezes_the_state_it_judged() {
    let h = harness().await;
    let conn = h.provider.conn().expect("conn");
    let ctx = ctx_of(TENANT);
    let payer_tenant_id = Uuid::new_v4();
    let membership_id = Uuid::new_v4();
    // `membership_publish` refuses a group the taxonomy does not declare
    // (`GROUP_UNKNOWN`), so the fixture reaches a world a real route could have
    // produced rather than one only the repository would accept.
    declare_customer_group(&h, "gold").await;

    let enrolled = membership_publish::enroll_in(
        &conn,
        h.registry.as_ref(),
        &ctx,
        &h.scope,
        TENANT,
        NewMembership {
            membership_id,
            tenant_id: TENANT,
            payer_tenant_id,
            group_value: "gold".to_owned(),
            effective_from: at(1),
            effective_to: None,
        },
        stamp_of(ACTOR, at(1)),
    )
    .await
    .expect("the enrollment is its own publish unit");
    assert_eq!(
        enrolled.membership.effective_to, None,
        "the enrollment is open-ended, which is the state its version must freeze"
    );
    h.registry.commit(&enrolled.pending_ref, 4);

    // The second mutation lands between the first unit's commit and the sweep —
    // the window D-47 makes up to five minutes wide.
    let ended = membership_publish::end_in(
        &conn,
        h.registry.as_ref(),
        &ctx,
        &h.scope,
        TENANT,
        membership_id,
        at(9),
        enrolled.membership.row_version,
        stamp_of(ACTOR, at(2)),
    )
    .await
    .expect("the end is its own publish unit");
    assert_eq!(ended.membership.effective_to, Some(at(9)));
    h.registry.commit(&ended.pending_ref, 5);

    let report = sweep(&h, at(13)).await;
    assert_eq!(report.subjects_failed, 0, "{report:?}");
    assert_eq!(report.subjects_projected, 2, "{report:?}");

    let rows = deltas(&h).await;
    assert_eq!(rows.len(), 2, "one delta per publish unit: {rows:?}");
    assert_eq!(rows[0].catalog_version, 4);
    assert_eq!(rows[1].catalog_version, 5);
    assert_eq!(
        rows[0].payload.get("effectiveTo"),
        Some(&serde_json::json!(None::<OffsetDateTime>)),
        "version 4 froze the enrollment, which was open-ended when its publish judged it; \
         reading the row live at sweep time is what freezes the *later* state under the \
         earlier commit's version, permanently, on an INSERT-only row"
    );
    assert_eq!(
        rows[1].payload.get("effectiveTo"),
        Some(&serde_json::json!(format_rfc3339(at(9)))),
        "and version 5 froze the end, which is the state *its* publish judged"
    );

    // The facts no mutation moves are the same in both, so the assertion above is
    // about the interval and not about two unrelated documents.
    for row in &rows {
        assert_eq!(
            row.payload.get("membershipId"),
            Some(&serde_json::json!(membership_id))
        );
        assert_eq!(
            row.payload.get("payerTenantId"),
            Some(&serde_json::json!(payer_tenant_id))
        );
        assert_eq!(
            row.payload.get("groupValue"),
            Some(&serde_json::json!("gold"))
        );
        assert_eq!(
            row.payload.get("effectiveFrom"),
            Some(&serde_json::json!(format_rfc3339(at(1))))
        );
    }
}

/// Declare one customer-group value, the way `declare_partner` below declares
/// the partner one and for the same reason: `membership_publish` validates the
/// group against `inst-cg-taxonomy` before it writes anything.
async fn declare_customer_group(h: &Harness, value: &str) {
    let conn = h.provider.conn().expect("a connection");
    let row = customer_group_taxonomy::ActiveModel {
        tenant_id: Set(TENANT),
        value: Set(value.to_owned()),
        display_name: Set(value.to_owned()),
        state: Set("active".to_owned()),
    };
    customer_group_taxonomy::Entity::insert(row.clone())
        .secure()
        .scope_with_model(&h.scope, &row)
        .expect("the scope admits the row")
        .exec(&conn)
        .await
        .expect("the customer-group value is declared");
}

/// Declare `partner/acme`, the value both overlay seeds scope themselves to.
///
/// **Added 2026-08-08 with §4.2's second validation run, and it is a fixture fix
/// rather than an accommodation.** Both seeds below scope their overlay to
/// `partner/acme` and neither declared it, so the commits they drove published an
/// overlay whose scope value existed in no taxonomy — a state the submit route
/// refuses outright with `SCOPE_VALUE_UNKNOWN` (D-120). They passed only because
/// the commit ran no world check; the second run found them the first time it
/// ran. Declaring the value is what makes the fixture reach a world a real submit
/// could have produced.
///
/// **Idempotent**, because a case that seeds two overlays calls it twice and the
/// taxonomy is `UNIQUE (tenant_id, value)`.
async fn declare_partner(h: &Harness) {
    let conn = h.provider.conn().expect("a connection");
    if partner_taxonomy::Entity::find()
        .secure()
        .scope_with(&h.scope)
        .filter(
            Condition::all()
                .add(partner_taxonomy::Column::TenantId.eq(TENANT))
                .add(partner_taxonomy::Column::Value.eq("acme")),
        )
        .one(&conn)
        .await
        .expect("read the partner taxonomy")
        .is_some()
    {
        return;
    }
    let row = partner_taxonomy::ActiveModel {
        tenant_id: Set(TENANT),
        value: Set("acme".to_owned()),
        display_name: Set("Acme".to_owned()),
        state: Set("active".to_owned()),
    };
    partner_taxonomy::Entity::insert(row.clone())
        .secure()
        .scope_with_model(&h.scope, &row)
        .expect("the scope admits the row")
        .exec(&conn)
        .await
        .expect("the partner value is declared");
}

/// The overlay publish service over this harness's provider and registry.
fn overlay_publish(h: &Harness) -> OverlayPublishService {
    OverlayPublishService::new(
        h.provider.clone(),
        Arc::clone(&h.registry) as Arc<dyn CatalogVersionRegistryV1>,
    )
}

/// A **draft** overlay of one tenant, scoped `partner/acme`, with one line.
async fn seed_draft_overlay(h: &Harness, precedence: i32) -> Uuid {
    declare_partner(h).await;
    let overlays = OverlayRepo::new(h.provider.clone());
    let price_overlay_id = Uuid::new_v4();
    overlays
        .create(
            &h.scope,
            NewOverlay {
                price_overlay_id,
                tenant_id: TENANT,
                scope: ScopeSelector::scoped(
                    ScopeClass::Partner,
                    ScopeValue::new("acme").expect("a value"),
                )
                .expect("a valued class"),
                precedence,
                interval: OverlayInterval {
                    from: Some(at(9)),
                    to: None,
                },
                tax_basis: TaxBasis::Exclusive,
                disclosure: Disclosure::Restricted,
                target_ref: TargetRef::default(),
            },
            vec![OverlayLine {
                line_id: Uuid::new_v4(),
                key: LineKey::list_default(),
                adjustment: Adjustment::Discount(Magnitude::PercentBp(1500)),
            }],
            stamp_of(ACTOR, at_min(11, 0)),
        )
        .await
        .expect("author the draft");
    price_overlay_id
}

/// A published overlay of one tenant, scoped `partner/acme`, with one line.
///
/// Written through the repository rather than by SQL so the row set is the one a
/// real publish leaves — three tables, the revision flip and its audit record.
async fn seed_published_overlay(h: &Harness, precedence: i32) -> Uuid {
    declare_partner(h).await;
    let overlays = OverlayRepo::new(h.provider.clone());
    let price_overlay_id = Uuid::new_v4();
    overlays
        .create(
            &h.scope,
            NewOverlay {
                price_overlay_id,
                tenant_id: TENANT,
                scope: ScopeSelector::scoped(
                    ScopeClass::Partner,
                    ScopeValue::new("acme").expect("a value"),
                )
                .expect("a valued class"),
                precedence,
                interval: OverlayInterval {
                    from: Some(at(9)),
                    to: None,
                },
                tax_basis: TaxBasis::Exclusive,
                disclosure: Disclosure::Restricted,
                target_ref: TargetRef::default(),
            },
            vec![OverlayLine {
                line_id: Uuid::new_v4(),
                key: LineKey::list_default(),
                adjustment: Adjustment::Discount(Magnitude::PercentBp(1500)),
            }],
            stamp_of(ACTOR, at_min(11, 0)),
        )
        .await
        .expect("author the draft");
    overlays
        .publish_revision(
            &h.scope,
            TENANT,
            price_overlay_id,
            0,
            stamp_of(ACTOR, at_min(11, 1)),
        )
        .await
        .expect("publish revision 0");
    price_overlay_id
}

#[tokio::test]
async fn a_published_overlay_subject_projects_its_document() {
    // D-234's projection half for the document subject. Until it landed, the
    // projector refused every non-plan kind by name and an approved overlay was
    // approved and unpublished.
    //
    // The delta is read from the revision the ref **pinned**, not from whatever
    // revision is current when the sweep arrives - the plan subject's own D-128
    // rule, and reachable here for the same reason: the registry batches at up to
    // five minutes and a second overlay revision inside that window would
    // otherwise freeze content this version's publish never judged.
    let h = harness().await;
    let price_overlay_id = seed_published_overlay(&h, 40).await;
    let conn = h.provider.conn().expect("conn");
    catalog_version_ref_repo::record_pending(
        &conn,
        &h.scope,
        PendingVersionRow::for_subject(
            TENANT,
            "pend-overlay-doc".to_owned(),
            &SubjectRef::PriceOverlay(price_overlay_id),
            Some(0),
            Some(LifecycleState::Published),
            at_min(12, 0),
        ),
    )
    .await
    .expect("record the document subject's ref");
    h.registry.commit("pend-overlay-doc", 4);

    let report = sweep(&h, at(13)).await;

    assert_eq!(report.subjects_failed, 0);
    assert_eq!(report.subjects_projected, 1);
    let rows = deltas(&h).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].subject_kind, "price_overlay");
    assert_eq!(rows[0].subject_ref, price_overlay_id.to_string());
    assert_eq!(rows[0].catalog_version, 4);
    assert_eq!(
        rows[0].payload.get("scopeClass"),
        Some(&serde_json::json!("partner")),
        "{:?}",
        rows[0].payload
    );
    assert_eq!(
        rows[0].payload.get("precedence"),
        Some(&serde_json::json!(40))
    );
    assert_eq!(
        rows[0]
            .payload
            .get("lines")
            .and_then(serde_json::Value::as_array)
            .map(Vec::len),
        Some(1),
        "the document carries its lines - it is what a consumer resolves"
    );
}

/// Record both subjects of one overlay publish against one handle.
async fn record_overlay_unit(h: &Harness, handle: &str, price_overlay_id: Uuid, revision: u64) {
    let conn = h.provider.conn().expect("conn");
    for subject in [
        SubjectRef::PriceOverlay(price_overlay_id),
        SubjectRef::OverlayIndex(
            OverlayIndexShard::scoped(OverlayScopeClass::Partner, "acme").expect("a valued shard"),
        ),
    ] {
        let revisioned = matches!(subject, SubjectRef::PriceOverlay(_));
        catalog_version_ref_repo::record_pending(
            &conn,
            &h.scope,
            PendingVersionRow::for_subject(
                TENANT,
                handle.to_owned(),
                &subject,
                revisioned.then_some(revision),
                revisioned.then_some(LifecycleState::Published),
                at_min(12, 0),
            ),
        )
        .await
        .expect("both subjects of the one act");
    }
}

#[tokio::test]
async fn an_overlay_publish_projects_its_shard_listing_the_overlay_it_published() {
    // D-112's enumeration access path, and the case the whole multi-subject unit
    // exists for: per-subject resolution answers "overlay X at pin V", evaluation
    // needs the *set*, and S9 sec 7 requires that set to be every scope-matching
    // overlay **live at V**.
    //
    // The shard must list the overlay **this same act published**, and that is
    // the ordering hazard: the two subjects are projected in two transactions, so
    // a shard derived from committed refs alone would omit its own sibling
    // whenever the shard happened to be projected first. The derivation therefore
    // counts the refs of *this version* the pass is holding, exactly as
    // `version_is_complete` does.
    let h = harness().await;
    let price_overlay_id = seed_published_overlay(&h, 40).await;
    record_overlay_unit(&h, "pend-unit", price_overlay_id, 0).await;
    h.registry.commit("pend-unit", 4);

    let report = sweep(&h, at(13)).await;

    assert_eq!(report.subjects_failed, 0);
    assert_eq!(report.subjects_projected, 2, "the document and its shard");
    let rows = deltas(&h).await;
    let shard = rows
        .iter()
        .find(|row| row.subject_kind == "overlay_index")
        .expect("the shard delta");
    assert_eq!(shard.subject_ref, "partner/acme");
    let listed = shard
        .payload
        .get("overlays")
        .and_then(serde_json::Value::as_array)
        .expect("overlays");
    assert_eq!(listed.len(), 1, "{:?}", shard.payload);
    assert_eq!(
        listed[0].get("priceOverlayId"),
        Some(&serde_json::json!(price_overlay_id))
    );
    assert_eq!(listed[0].get("precedence"), Some(&serde_json::json!(40)));
}

#[tokio::test]
async fn a_shard_projected_before_its_own_document_still_lists_it() {
    // **The ordering-independence, and it needs forcing to be tested at all.**
    // The two subjects of one act are projected in two transactions, and
    // `list_pending_for_tenant` orders by `requested_at` then `pending_ref` -
    // which for one act ties on both, because the commit writes both refs at one
    // instant. So the order is arbitrary in production and merely *happened* to
    // put the document first in the case above; that case therefore proves
    // nothing about this property, which is what a probe showed by not reddening.
    //
    // Here the shard is requested first, so it is projected first. Derived from
    // committed refs alone it would omit its own sibling - the overlay this very
    // act published - and the omission would be permanent, because the shard is
    // warm and only the document gets re-driven.
    let h = harness().await;
    let price_overlay_id = seed_published_overlay(&h, 40).await;
    let conn = h.provider.conn().expect("conn");
    for (subject, requested_at) in [
        (
            SubjectRef::OverlayIndex(
                OverlayIndexShard::scoped(OverlayScopeClass::Partner, "acme")
                    .expect("a valued shard"),
            ),
            at_min(12, 0),
        ),
        (SubjectRef::PriceOverlay(price_overlay_id), at_min(12, 1)),
    ] {
        let revisioned = matches!(subject, SubjectRef::PriceOverlay(_));
        catalog_version_ref_repo::record_pending(
            &conn,
            &h.scope,
            PendingVersionRow::for_subject(
                TENANT,
                "pend-shard-first".to_owned(),
                &subject,
                revisioned.then_some(0),
                revisioned.then_some(LifecycleState::Published),
                requested_at,
            ),
        )
        .await
        .expect("both subjects, the shard requested first");
    }
    h.registry.commit("pend-shard-first", 4);

    let report = sweep(&h, at(13)).await;

    assert_eq!(report.subjects_failed, 0);
    let rows = deltas(&h).await;
    let shard = rows
        .iter()
        .find(|row| row.subject_kind == "overlay_index")
        .expect("the shard delta");
    assert_eq!(
        shard
            .payload
            .get("overlays")
            .and_then(serde_json::Value::as_array)
            .map(Vec::len),
        Some(1),
        "the shard must list the overlay its own act published: {:?}",
        shard.payload
    );
}

#[tokio::test]
async fn a_shard_does_not_list_an_overlay_published_into_a_later_version() {
    // The half that makes the shard a statement about **V** rather than about the
    // moment the projector arrived. The registry batches at up to five minutes
    // (D-47), so a second overlay publishing inside that window is ordinary - and
    // read from truth at projection time it would land in the *earlier* version's
    // frozen shard, which is INSERT-only on the seven-year horizon.
    //
    // A consumer pinned at V4 would then enumerate an overlay whose document has
    // no delta at or below V4, and the per-subject document read S9 sec 7
    // prescribes in the same sentence would find nothing.
    //
    // **What this case does NOT prove**, found by probing it: removing the `<= V`
    // bound from the query leaves it green. When V4 is projected, V5's refs are
    // still pending and the `IS NOT NULL` filter excludes them on its own. So this
    // asserts the outcome and not the mechanism - see
    // `overlay_revisions_at_or_below`, where the premise is written down.
    let h = harness().await;
    let first = seed_published_overlay(&h, 40).await;
    let second = seed_published_overlay(&h, 50).await;
    record_overlay_unit(&h, "pend-v4", first, 0).await;
    record_overlay_unit(&h, "pend-v5", second, 0).await;
    h.registry.commit("pend-v4", 4);
    h.registry.commit("pend-v5", 5);

    sweep(&h, at(13)).await;

    let rows = deltas(&h).await;
    let shard_at = |version: i64| {
        rows.iter()
            .find(|row| row.subject_kind == "overlay_index" && row.catalog_version == version)
            .unwrap_or_else(|| panic!("the shard delta at V{version}"))
            .payload
            .get("overlays")
            .and_then(serde_json::Value::as_array)
            .expect("overlays")
            .len()
    };

    assert_eq!(
        shard_at(4),
        1,
        "V4 knows only the overlay published into it"
    );
    assert_eq!(shard_at(5), 2, "V5 knows both");
}

#[tokio::test]
async fn one_version_holding_two_publishes_of_one_overlay_resolves_the_greater_revision() {
    // Z7-3. `overlay_revisions_at_or_below` folds "later row wins" over an order
    // that was total on `catalog_version` alone, and D-47's five-minute batching is
    // precisely the mechanism that puts two publishes of ONE overlay into one
    // version: two handles, two refs, one version, revisions 0 and 1. At that
    // point the two rows tie, SQL leaves their relative order undefined, and the
    // fold freezes an arbitrary revision into the frozen `overlay_index` shard -
    // permanently, because a delta is INSERT-only over the seven-year horizon and
    // no later write exists to correct it.
    //
    // The function's own doc states the rule the fold must keep: **the greatest
    // ref, not any ref**. So the claim is about *which* revision wins a tie, and a
    // probe that resolves one overlay per version proves nothing about it. Both
    // overlays here carry the tie and differ only in the order their two refs were
    // written and in the alphabetical order of their handles - which are the two
    // things the store is free to order tied rows by, the second being this
    // table's primary key. Neither arrangement may change the answer.
    //
    // Asked of the repository directly, because the ref rows are the only input
    // this function reads, and the sweep cannot be made to write two refs of one
    // overlay into one version without a registry double answering two handles the
    // same number - which is `two_publishes_of_one_tenant_commit_into_one_version`
    // one plane over, and would assert the projector rather than the fold.
    let h = harness().await;
    let conn = h.provider.conn().expect("conn");
    let greater_written_first = Uuid::new_v4();
    let greater_written_last = Uuid::new_v4();
    for (price_overlay_id, handle, revision) in [
        (greater_written_first, "pend-a-tie-rev1", 1_u64),
        (greater_written_first, "pend-b-tie-rev0", 0),
        (greater_written_last, "pend-c-tie-rev0", 0),
        (greater_written_last, "pend-d-tie-rev1", 1),
    ] {
        let row = PendingVersionRow::for_subject(
            TENANT,
            handle.to_owned(),
            &SubjectRef::PriceOverlay(price_overlay_id),
            Some(revision),
            Some(LifecycleState::Published),
            at_min(12, 0),
        );
        catalog_version_ref_repo::record_pending(&conn, &h.scope, row.clone())
            .await
            .expect("one publish's overlay document ref");
        catalog_version_ref_repo::finalize(
            &conn,
            &h.scope,
            RefIdentity::of(&row),
            CatalogVersion::new(4),
            at(13),
        )
        .await
        .expect("both publishes of the overlay batch into V4");
    }

    // **A third overlay one version up**, so the query's `.lte(target)` bound has
    // an operand. Every ref above finalizes at V4 and the query asks at V4, so
    // without this the bound was never exercised — deleting `.lte(target)` from
    // `overlay_revisions_at_or_below` reddened nothing in the whole tree, and the
    // sweep-level case `a_shard_does_not_list_an_overlay_published_into_a_later_version`
    // says in its own comment that it stays green too. The consequence is a frozen,
    // INSERT-only `overlay_index` shard listing an overlay from a later version: a
    // consumer pinned at V enumerates an overlay whose document has no delta at or
    // below V.
    let later = Uuid::new_v4();
    let later_row = PendingVersionRow::for_subject(
        TENANT,
        "pend-e-v5".to_owned(),
        &SubjectRef::PriceOverlay(later),
        Some(0),
        Some(LifecycleState::Published),
        at_min(12, 0),
    );
    catalog_version_ref_repo::record_pending(&conn, &h.scope, later_row.clone())
        .await
        .expect("a third overlay's document ref");
    catalog_version_ref_repo::finalize(
        &conn,
        &h.scope,
        RefIdentity::of(&later_row),
        CatalogVersion::new(5),
        at(13),
    )
    .await
    .expect("it commits into the next version");

    let resolved = catalog_version_ref_repo::overlay_revisions_at_or_below(
        &conn,
        &h.scope,
        TENANT,
        CatalogVersion::new(4),
    )
    .await
    .expect("the overlays live at V4");

    assert_eq!(
        resolved.get(&greater_written_first),
        Some(&1),
        "the greatest ref wins the tie when it was written first: {resolved:?}"
    );
    assert_eq!(
        resolved.get(&greater_written_last),
        Some(&1),
        "...and when it was written last: {resolved:?}"
    );
    assert_eq!(
        resolved.get(&later),
        None,
        "and an overlay finalized into V5 is not resolved at V4: the fold is bounded at or below the target, not over every finalized ref of the tenant: {resolved:?}"
    );

    // The positive control for that absence: the V5 ref really is in the table, so
    // the line above is about the bound rather than about a seed that never landed.
    let at_five = catalog_version_ref_repo::overlay_revisions_at_or_below(
        &conn,
        &h.scope,
        TENANT,
        CatalogVersion::new(5),
    )
    .await
    .expect("the fold one version up");
    assert_eq!(
        at_five.get(&later),
        Some(&0),
        "the V5 ref is resolvable at V5: {at_five:?}"
    );
}

// ---------------------------------------------------------------------------
// 2. Batching — the finding this group's migration amendment exists for.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn two_publishes_of_one_tenant_commit_into_one_version() {
    // D-47's normal case, and the one `uq_pricing_catalog_version_ref_version`
    // made physically impossible: several of a tenant's pending refs committing
    // into ONE version. Under that index the second finalize failed outright.
    let h = harness().await;
    let (plan_a, pending_a) = seed_and_publish_at(&h, "gold", at_min(12, 0)).await;
    let (plan_b, pending_b) = seed_and_publish_at(&h, "silver", at_min(12, 1)).await;
    h.registry.commit(&pending_a, 4);
    h.registry.commit(&pending_b, 4);

    let report = sweep(&h, at(13)).await;
    assert_eq!(report.versions_projected, 1, "one version, two subjects");
    assert_eq!(report.subjects_projected, 2);
    assert_eq!(report.frontiers_advanced, 1, "one advance, not two");

    let rows = deltas(&h).await;
    assert_eq!(rows.len(), 2);
    let mut subjects: Vec<&str> = rows.iter().map(|row| row.subject_ref.as_str()).collect();
    subjects.sort_unstable();
    let mut expected = vec![plan_a.to_string(), plan_b.to_string()];
    expected.sort();
    assert_eq!(subjects, expected);
    assert!(rows.iter().all(|row| row.catalog_version == 4));

    assert!(
        refs(&h)
            .await
            .iter()
            .all(|row| row.catalog_version == Some(4)),
        "both refs finalized at the same version"
    );
    assert_eq!(frontier_version(&h).await, Some(4));
}

// ---------------------------------------------------------------------------
// 3 + 4. The prefix, and the forward walk that keeps it from stranding.
// ---------------------------------------------------------------------------

/// A `V5` whose second subject cannot project yet, and a complete `V6`.
///
/// The incompleteness is a **per-subject projection fault**, which is the shape
/// incompleteness actually takes once batch atomicity is assumed: the registry
/// commits a batch as a unit, so a version above the frontier is short of
/// pin-eligibility because one of its subjects would not project, not because
/// one of its refs is unknown. Here the second subject names a plan whose
/// revision row is not there when the sweep arrives — and `seed_late_subject`
/// is what makes it repairable, so the walk case can continue from this one.
async fn seed_incomplete_five_and_complete_six(h: &Harness) -> PlanId {
    let (_, pending_a) = seed_and_publish_at(h, "gold", at_min(12, 0)).await;
    let late = PlanId::new(Uuid::new_v4());
    let pending_late = "pend-late".to_owned();
    let conn = h.provider.conn().expect("conn");
    catalog_version_ref_repo::record_pending(
        &conn,
        &h.scope,
        PendingVersionRow::for_subject(
            TENANT,
            pending_late.clone(),
            &SubjectRef::Plan(late.get()),
            Some(0),
            Some(LifecycleState::Published),
            at_min(12, 1),
        ),
    )
    .await
    .expect("record the second subject of V5");
    let (_, pending_c) = seed_and_publish_at(h, "bronze", at_min(12, 2)).await;

    h.registry.commit(&pending_a, 5);
    h.registry.commit(&pending_late, 5);
    h.registry.commit(&pending_c, 6);
    late
}

/// Give the late subject the published revision it was waiting for, without
/// minting a second ref for it.
async fn seed_late_subject(h: &Harness, plan_id: PlanId) {
    let (revision, version) = seed_publishable(h, plan_id, "silver").await;
    let (_, outcome) = h
        .provider
        .db()
        .in_transaction::<(), bss_pricing::infra::storage::RepoError, _>(move |txn| {
            let scope = AccessScope::for_tenant(TENANT);
            Box::pin(async move {
                plan_repo::publish_revision(txn, &scope, TENANT, plan_id, revision, version)
                    .await
                    .map(|_| ())
            })
        })
        .await;
    outcome.expect("the late subject's revision publishes");
}

#[tokio::test]
async fn a_complete_later_version_does_not_move_the_frontier_over_an_incomplete_earlier_one() {
    // D-114's prefix, which is the whole reason pin-eligibility is version-level
    // AND prefix-closed: without it a pin of V6 resolved the late plan at V4
    // and then, once its warm landed, at V5 - one pin, two contents.
    let h = harness().await;
    let _ = seed_incomplete_five_and_complete_six(&h).await;

    let report = sweep(&h, at(13)).await;
    assert_eq!(report.versions_projected, 2, "V5 and V6 both saw a pass");
    assert_eq!(
        report.subjects_failed, 1,
        "V5's second subject could not project"
    );
    assert_eq!(
        report.frontiers_advanced, 0,
        "V5 is incomplete, and V6 is not the frontier's next in order"
    );
    assert_eq!(
        frontier_version(&h).await,
        None,
        "a tenant whose first version is incomplete has nothing it may pin"
    );

    // V6 really is fully warm, which is what makes the refusal a prefix
    // decision rather than an incompleteness one.
    let rows = deltas(&h).await;
    assert!(rows.iter().any(|row| row.catalog_version == 6));
}

#[tokio::test]
async fn completing_the_earlier_version_walks_the_frontier_through_to_the_later_one() {
    // The D-136 divergence, pinned as behaviour. Read literally, the frontier
    // advances only in the transaction completing its NEXT version in order -
    // so when V5 finally completes the frontier reaches V5, and V6, already
    // complete, would never see another completion to advance on. The
    // implementation walks forward instead.
    let h = harness().await;
    let late = seed_incomplete_five_and_complete_six(&h).await;
    sweep(&h, at(13)).await;
    assert_eq!(frontier_version(&h).await, None);

    seed_late_subject(&h, late).await;
    let report = sweep(&h, at(14)).await;

    assert_eq!(report.subjects_projected, 1, "only the late one was left");
    assert_eq!(report.subjects_failed, 0);
    assert_eq!(
        frontier_version(&h).await,
        Some(6),
        "one pass carried the frontier through V5 to the already-complete V6"
    );
}

#[tokio::test]
async fn a_straggler_into_a_version_the_frontier_has_passed_is_refused_loudly() {
    // The enforcement that replaces a premise. Completeness is judged from a
    // version's committed refs plus the ones the pass is finalizing, so it is
    // only as good as batch atomicity - one batch, one version, one event. When
    // that fails, a ref of V5 arrives after V5 is already pinnable, and adding
    // a subject to a pinnable version is one pin resolving two contents over
    // time. It is refused at the moment it would corrupt rather than predicted.
    //
    // It also pins the reading of D-163 clause (2) that decides which faults get
    // charged clause (3)'s price. `Ok(None)` is a SUCCESSFUL answer - "not
    // committed yet" - and under clause (1) a ref the registry has not committed
    // cannot belong to a version that already has one, so it does NOT stop the
    // pass judging V5 complete. What the registry then does below is contradict
    // itself, and that is the contract violation this refusal is for. A per-ref
    // registry ERROR is the other thing and defers instead:
    // `a_pass_with_an_unresolvable_sibling_ref_decides_no_completion`.
    let h = harness().await;
    let (_, pending_a) = seed_and_publish_at(&h, "gold", at_min(12, 0)).await;
    let (plan_b, pending_b) = seed_and_publish_at(&h, "silver", at_min(12, 1)).await;
    h.registry.commit(&pending_a, 5);
    h.registry.script(&pending_b, vec![None]);

    let first = sweep(&h, at(13)).await;
    assert_eq!(first.frontiers_advanced, 1);
    assert_eq!(frontier_version(&h).await, Some(5));

    // Now B turns out to have belonged to V5 all along.
    h.registry.commit(&pending_b, 5);
    let second = sweep(&h, at(14)).await;

    assert_eq!(
        second.subjects_failed, 1,
        "the straggler is refused, not quietly projected"
    );
    assert_eq!(second.subjects_projected, 0);

    let rows = deltas(&h).await;
    assert_eq!(rows.len(), 1, "no second subject joined a pinnable version");
    assert!(
        !rows.iter().any(|row| row.subject_ref == plan_b.to_string()),
        "and it is not B's"
    );
    assert_eq!(
        refs(&h)
            .await
            .iter()
            .filter(|row| row.pending_ref == pending_b)
            .filter(|row| row.catalog_version.is_none())
            .count(),
        1,
        "B's ref stays pending, so its age alarms rather than its content landing"
    );
    assert_eq!(frontier_version(&h).await, Some(5), "and nothing moved");
}

// ---------------------------------------------------------------------------
// 4b. The completeness bound: a pass that could not have seen the whole subject
//     set decides no completion (D-163 clause 2).
// ---------------------------------------------------------------------------

/// A version with two subjects of one tenant, both projectable.
///
/// Returns `(the two handles, the version they commit into)`.
async fn seed_two_subject_version(h: &Harness, version: u64) -> (String, String) {
    let (_, first) = seed_and_publish_at(h, "gold", at_min(12, 0)).await;
    let (_, second) = seed_and_publish_at(h, "silver", at_min(12, 1)).await;
    h.registry.commit(&first, version);
    h.registry.commit(&second, version);
    (first, second)
}

#[tokio::test]
async fn one_pending_handle_carries_every_subject_its_unit_projects() {
    // D-234's store half. A plan publish unit projects one subject and the table
    // was keyed for exactly that — `PRIMARY KEY (tenant_id, pending_ref)`. An
    // overlay publish unit projects **two** (D-112: the overlay document and the
    // `overlay_index` shard), and three when a revision moves the scope value
    // (D-133), so the second `record_pending` of one act was refused by the key.
    //
    // Two handles is not the way out and the reason is a pin rather than a
    // preference: the index is the enumeration access path, so a pin landing
    // between two handles resolves the overlay document live while the shard at
    // or below it still says the overlay is not — enumeration and resolution
    // disagreeing at one pin.
    //
    // `pricing_catalog_version_ref`'s migration doc named this as owed, in these terms.
    let h = harness().await;
    let conn = h.provider.conn().expect("conn");
    let overlay_id = Uuid::new_v4();

    catalog_version_ref_repo::record_pending(
        &conn,
        &h.scope,
        PendingVersionRow::for_subject(
            TENANT,
            "pend-overlay-unit".to_owned(),
            &SubjectRef::PriceOverlay(overlay_id),
            Some(1),
            Some(LifecycleState::Published),
            at_min(12, 0),
        ),
    )
    .await
    .expect("the overlay document subject");

    catalog_version_ref_repo::record_pending(
        &conn,
        &h.scope,
        PendingVersionRow::for_subject(
            TENANT,
            "pend-overlay-unit".to_owned(),
            &SubjectRef::OverlayIndex(OverlayIndexShard::Global),
            None,
            None,
            at_min(12, 0),
        ),
    )
    .await
    .expect("the same handle's index shard subject");

    let rows = refs(&h).await;
    assert_eq!(rows.len(), 2, "one handle, two subject rows: {rows:?}");
    assert!(
        rows.iter()
            .all(|row| row.pending_ref == "pend-overlay-unit"),
        "both rows name the one assignment the act requested: {rows:?}"
    );
    let mut kinds: Vec<&str> = rows.iter().map(|row| row.subject_kind.as_str()).collect();
    kinds.sort_unstable();
    assert_eq!(kinds, vec!["overlay_index", "price_overlay"]);
}

#[tokio::test]
async fn one_handle_still_refuses_a_second_record_of_the_same_subject() {
    // The protection the old key gave, at the granularity the widened one keeps
    // it: the registry is idempotent on `request_id`, so one handle arriving
    // twice for one **subject** means two publish transactions believe they own
    // that assignment, and an upsert would hand one publish's subject to the
    // other's version. Widening the key must not widen that hole.
    let h = harness().await;
    let conn = h.provider.conn().expect("conn");
    let overlay_id = Uuid::new_v4();
    let row = |at: OffsetDateTime| {
        PendingVersionRow::for_subject(
            TENANT,
            "pend-twice".to_owned(),
            &SubjectRef::PriceOverlay(overlay_id),
            Some(1),
            Some(LifecycleState::Published),
            at,
        )
    };

    catalog_version_ref_repo::record_pending(&conn, &h.scope, row(at_min(12, 0)))
        .await
        .expect("the first record");
    let refusal = catalog_version_ref_repo::record_pending(&conn, &h.scope, row(at_min(12, 1)))
        .await
        .expect_err("the same handle and the same subject, twice");

    assert!(
        matches!(refusal, RepoError::Db(_)),
        "the primary key's own refusal, unchanged in kind: {refusal:?}"
    );
    assert_eq!(refs(&h).await.len(), 1, "and nothing was overwritten");
}

#[tokio::test]
async fn finalizing_one_subject_of_a_handle_leaves_its_siblings_pending() {
    // The finalize is per **subject**, not per handle, and the reason is what
    // keeps a multi-subject unit self-healing.
    //
    // The projector finalizes a ref inside the transaction that projects that
    // subject. Finalized handle-wide, subject A's transaction would commit
    // subject B's ref too — and if B's own projection then fails, B is committed
    // and unwarm, so it is no longer in `list_pending_for_tenant` and **no later
    // pass ever offers its version to the projector again**. The version stays
    // incomplete, the frontier stays stuck, and the two signals that would say so
    // both read off pending refs. Per subject, B stays pending until B is
    // projected, which is what the re-drive is made of.
    let h = harness().await;
    let conn = h.provider.conn().expect("conn");
    let overlay_id = Uuid::new_v4();
    for subject in [
        SubjectRef::PriceOverlay(overlay_id),
        SubjectRef::OverlayIndex(OverlayIndexShard::Global),
    ] {
        catalog_version_ref_repo::record_pending(
            &conn,
            &h.scope,
            PendingVersionRow::for_subject(
                TENANT,
                "pend-partial".to_owned(),
                &subject,
                None,
                None,
                at_min(12, 0),
            ),
        )
        .await
        .expect("both subjects of the one act");
    }

    catalog_version_ref_repo::finalize(
        &conn,
        &h.scope,
        RefIdentity {
            tenant_id: TENANT,
            pending_ref: "pend-partial",
            subject_kind: SubjectKind::PriceOverlay,
            subject_ref: &overlay_id.to_string(),
        },
        CatalogVersion::new(5),
        at(13),
    )
    .await
    .expect("finalize the document subject alone");

    let rows = refs(&h).await;
    let document = rows
        .iter()
        .find(|row| row.subject_kind == "price_overlay")
        .expect("the document row");
    let shard = rows
        .iter()
        .find(|row| row.subject_kind == "overlay_index")
        .expect("the shard row");
    assert_eq!(document.catalog_version, Some(5));
    assert_eq!(
        shard.catalog_version, None,
        "the sibling is still pending, so the sweep keeps offering its version"
    );
    assert_eq!(shard.committed_at, None, "and the pair moves together");
}

#[tokio::test]
async fn a_multi_subject_handle_is_asked_of_the_registry_once_per_pass() {
    // The registry is a cross-gear network call and the handle is what it
    // answers about: `committed_version` takes a `pending_ref` and nothing else,
    // so every subject of one handle is asking one question. Once a unit records
    // two or three subjects against one handle (D-112, D-133), a per-row loop
    // multiplies the sweep's registry traffic by the subject count for an answer
    // it already holds.
    //
    // It also keeps the scripted double honest: `script` indexes its answers by
    // call count, so a per-row ask would consume a handle's script two entries at
    // a time and a test scripting "not yet, then V5" would see V5 on the first
    // pass.
    let h = harness().await;
    let conn = h.provider.conn().expect("conn");
    for subject in [
        SubjectRef::PriceOverlay(Uuid::new_v4()),
        SubjectRef::OverlayIndex(OverlayIndexShard::Global),
    ] {
        catalog_version_ref_repo::record_pending(
            &conn,
            &h.scope,
            PendingVersionRow::for_subject(
                TENANT,
                "pend-two-subjects".to_owned(),
                &subject,
                None,
                None,
                at_min(12, 0),
            ),
        )
        .await
        .expect("both subjects of the one act");
    }
    h.registry.commit("pend-two-subjects", 5);

    let report = sweep(&h, at(13)).await;

    assert_eq!(
        report.pending_seen, 2,
        "the pass still sees both rows - the dedup is the registry ask, not the subject set"
    );
    assert_eq!(
        h.registry.calls_for("pend-two-subjects"),
        1,
        "one handle, one question"
    );
}

#[tokio::test]
async fn a_version_whose_subjects_all_warm_in_one_pass_is_complete_and_advances() {
    // The unchanged base case, asserted on the new report field so the bound is
    // visibly not blocking the ordinary path. A pass that saw both refs and got
    // an answer for both is entitled to count them.
    let h = harness().await;
    let _ = seed_two_subject_version(&h, 5).await;

    let report = sweep(&h, at(13)).await;

    assert_eq!(report.tenants_seen, 1);
    assert_eq!(report.pending_seen, 2);
    assert_eq!(report.subjects_projected, 2);
    assert_eq!(report.versions_complete, 1, "the pass saw the whole set");
    assert_eq!(report.frontiers_advanced, 1);
    assert_eq!(frontier_version(&h).await, Some(5));
}

#[tokio::test]
async fn a_pass_with_an_unresolvable_sibling_ref_decides_no_completion() {
    // D-163 clause (2), the per-ref-outage arm. Before the bound the version read
    // complete from the warm set alone - the unresolvable ref is not in the
    // version's subject set at all, because a pending ref carries no version - the
    // frontier advanced, and the sibling then arrived BELOW it and was refused
    // loudly, at clause (3)'s price: unresolvable at any version, nothing
    // self-healing.
    //
    // Delete `coverage.may_decide_completion()` from either place in
    // `read_model.rs` and this test fails.
    let h = harness().await;
    let (first, second) = seed_two_subject_version(&h, 5).await;
    h.registry.fail_handle(&second);

    let report = sweep(&h, at(13)).await;

    assert_eq!(report.subjects_projected, 1, "the answerable one warmed");
    assert_eq!(
        report.versions_complete, 0,
        "and the pass declined to judge a set it could not have seen whole"
    );
    assert_eq!(report.frontiers_advanced, 0);
    assert_eq!(
        frontier_version(&h).await,
        None,
        "the frontier lags, which is the safe direction"
    );
    // The warmed subject really is warm: the bound gates the COMPLETENESS
    // decision and nothing else about the pass.
    assert_eq!(deltas(&h).await.len(), 1);
    assert!(
        refs(&h)
            .await
            .iter()
            .any(|row| row.pending_ref == first && row.catalog_version == Some(5)),
        "the resolvable ref finalized as usual"
    );
}

#[tokio::test]
async fn the_next_pass_with_the_outage_cleared_completes_the_version_and_advances() {
    // The lag recovers on its own, which is what makes deferring safe rather than
    // merely cautious. Nothing out of band is needed - the sweep arriving again
    // IS the recovery, exactly as it is for the warm re-drive.
    let h = harness().await;
    let (_, second) = seed_two_subject_version(&h, 5).await;
    h.registry.fail_handle(&second);
    sweep(&h, at(13)).await;
    assert_eq!(frontier_version(&h).await, None);

    h.registry.clear_handle_failures();
    let report = sweep(&h, at(14)).await;

    assert_eq!(
        report.subjects_projected, 1,
        "only the deferred one was left"
    );
    assert_eq!(report.versions_complete, 1);
    assert_eq!(report.frontiers_advanced, 1);
    assert_eq!(frontier_version(&h).await, Some(5));
    assert_eq!(deltas(&h).await.len(), 2);
}

#[tokio::test]
async fn a_deferred_completion_is_not_silent() {
    // D-166 clause (5) is what makes clause (2)'s lag reportable rather than
    // invisible: a tenant with no frontier and a ref of its own past the batching
    // SLO is short of pin-eligibility, and that is precisely the deferred state.
    let h = harness().await;
    let (_, second) = seed_two_subject_version(&h, 5).await;
    h.registry.fail_handle(&second);

    let report = sweep(&h, at(13)).await;

    assert_eq!(report.versions_complete, 0);
    assert_eq!(
        report.pin_eligibility_overdue, 1,
        "the deferral is reported, not merely survived"
    );

    // **And it reaches the alarm plane** (D-238). The report count is a return
    // value only this test reads; the counter is what an alerting rule sees, and
    // for months this was the gear's only Critical alarm that no rule could.
    h.metrics.force_flush();
    assert_eq!(
        h.metrics.counter_value(
            "pricing_alarm_total",
            &[
                ("alarm", "pricing.readmodel.pin_eligibility_overdue"),
                ("severity", "critical"),
            ]
        ),
        1
    );
}

#[tokio::test]
async fn a_saturated_scan_defers_its_own_tenant_and_no_one_elses() {
    // The fairness half, and the one a cross-tenant bound cannot pass. Under the
    // old cross-tenant page ordered by request instant, the tenant holding the
    // oldest refs filled the budget and every OTHER tenant's completions were
    // deferred with it - their frontiers standing still because someone else's
    // backlog was stuck.
    //
    // `pending_refs_per_tenant = 2` and the first tenant holds exactly two, so
    // its page fills; the second holds one and does not. The second tenant's id
    // sorts ABOVE the first's, so it is swept second - the direction that would
    // fail under a shared budget.
    let h = harness_with(JobsConfig {
        pending_refs_per_tenant: 2,
        ..JobsConfig::default()
    })
    .await;
    let _ = seed_two_subject_version(&h, 5).await;
    let (_, other) = seed_and_publish_for(&h, OTHER_TENANT, "gold", at_min(12, 2)).await;
    h.registry.commit(&other, 6);

    let report = sweep(&h, at(13)).await;

    assert_eq!(
        report.tenants_seen, 2,
        "both tenants were found - the counter is the discovery read"
    );
    assert_eq!(
        report.subjects_projected, 3,
        "and all three subjects warmed"
    );
    assert_eq!(
        report.versions_complete, 1,
        "one completion: the saturated tenant's version was not judged"
    );
    assert_eq!(
        frontier_version(&h).await,
        None,
        "the saturated tenant's frontier waits for a pass that could see its whole set"
    );
    assert_eq!(
        frontier_version_of(&h, OTHER_TENANT).await,
        Some(6),
        "and the other tenant's advanced in the same pass"
    );
}

#[tokio::test]
async fn a_straggler_below_the_frontier_is_still_refused_once_the_bound_is_in_place() {
    // The bound narrowed `refuse_projection_below_frontier`'s reachability
    // without disarming it. What can no longer reach it is an ordinary per-ref
    // outage; what still does is the registry contradicting D-163 clause (1) -
    // a batch that did not commit atomically into one version.
    //
    // Constructed directly: the frontier is advanced by a complete V5, and then a
    // ref of the SAME tenant is offered at V5 afterwards. No outage is involved,
    // which is the point.
    let h = harness().await;
    let (_, first) = seed_and_publish_at(&h, "gold", at_min(12, 0)).await;
    h.registry.commit(&first, 5);
    sweep(&h, at(13)).await;
    assert_eq!(frontier_version(&h).await, Some(5));

    // A second publish of the tenant that the registry now says belongs to V5 -
    // a version whose subject set clause (1) says closed at the first commit.
    let (plan_b, second) = seed_and_publish_at(&h, "silver", at_min(13, 30)).await;
    h.registry.commit(&second, 5);

    let report = sweep(&h, at(14)).await;

    assert_eq!(
        report.subjects_failed, 1,
        "the straggler is refused, not quietly projected"
    );
    assert_eq!(report.subjects_projected, 0);
    assert_eq!(
        deltas(&h).await.len(),
        1,
        "no subject joined a pinnable version"
    );
    assert!(
        !deltas(&h)
            .await
            .iter()
            .any(|row| row.subject_ref == plan_b.to_string())
    );
    assert_eq!(frontier_version(&h).await, Some(5), "and nothing moved");
}

// ---------------------------------------------------------------------------
// 5. The re-drive is idempotent.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_second_pass_over_a_complete_version_writes_nothing_and_refuses_nothing() {
    // The projector's subtraction IS the re-drive, so a version with nothing
    // outstanding is a pass that does nothing: no second delta row (the primary
    // key would refuse one), no re-finalize, and no attempt to re-advance - a
    // FrontierRegression escaping as a failed pass would be the ordering bug
    // that refusal exists to surface, raised by a mechanism that is working.
    let h = harness().await;
    let (_, pending) = seed_and_publish_at(&h, "gold", at_min(12, 0)).await;
    h.registry.commit(&pending, 1);

    sweep(&h, at(13)).await;
    let after_first = deltas(&h).await;
    let committed_at = refs(&h).await[0].committed_at;

    let report = sweep(&h, at(14)).await;
    assert_eq!(report.subjects_projected, 0, "nothing was outstanding");
    assert_eq!(report.frontiers_advanced, 0);

    assert_eq!(deltas(&h).await, after_first, "not one byte moved");
    assert_eq!(
        refs(&h).await[0].committed_at,
        committed_at,
        "the finalize was not repeated"
    );
    assert_eq!(frontier_version(&h).await, Some(1));
}

// ---------------------------------------------------------------------------
// 6. A registry with no answer leaves the seam exactly as the commit left it.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_registry_that_has_not_committed_the_batch_leaves_the_seam_untouched() {
    // The seam, one tick later. `None` is the registry batching, which D-47
    // budgets at up to five minutes - not an error and not an alarm at this
    // age.
    let h = harness().await;
    let (_, pending) = seed_and_publish_at(&h, "gold", at_min(12, 0)).await;
    h.registry.script(&pending, vec![None]);

    // One minute after the request, which is inside the ratified five-minute
    // max batching delay: the wait is budgeted, so nothing alarms.
    let report = sweep(&h, at(12) + time::Duration::minutes(1)).await;
    assert_eq!(report.pending_seen, 1);
    assert_eq!(report.versions_projected, 0);
    assert_eq!(report.commit_overdue, 0);
    assert_eq!(
        report.degraded_emitted, 0,
        "a publish inside the batching SLO is not degraded"
    );
    assert_eq!(
        report.pin_eligibility_overdue, 0,
        "nor is it short of pin-eligibility: the wait is budgeted, and a tenant that has          simply not published for a while is not a stuck one"
    );

    assert!(deltas(&h).await.is_empty());
    assert_eq!(frontier_version(&h).await, None);
    assert_eq!(
        refs(&h).await[0].catalog_version,
        None,
        "the ref is exactly as the commit wrote it"
    );
}

// ---------------------------------------------------------------------------
// 7. A registry that contradicts itself.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_registry_answering_one_handle_two_versions_is_refused() {
    // Re-pointing a committed handle re-points a pin that posted periods
    // resolve through, so the finalize refuses it as an invariant breach rather
    // than accepting the newer answer - and answers the SAME version with `Ok`,
    // because that is the idempotent replay a re-drive is entitled to make.
    //
    // Asked of the repository directly, because through the sweep the case is
    // unreachable: a committed ref is not in `list_pending`, so the sweep never
    // offers the registry a second chance to contradict itself. That is a
    // property worth stating rather than a gap - it is why the guard is a
    // predicate on a statement and not a check in the pass.
    let h = harness().await;
    let (plan_id, pending) = seed_and_publish_at(&h, "gold", at_min(12, 0)).await;
    h.registry.commit(&pending, 5);

    let first = sweep(&h, at(13)).await;
    assert_eq!(first.subjects_projected, 1);
    assert_eq!(frontier_version(&h).await, Some(5));

    let conn = h.provider.conn().expect("conn");
    catalog_version_ref_repo::finalize(
        &conn,
        &h.scope,
        RefIdentity {
            tenant_id: TENANT,
            pending_ref: &pending,
            subject_kind: SubjectKind::Plan,
            subject_ref: &plan_id.get().to_string(),
        },
        CatalogVersion::new(5),
        at(14),
    )
    .await
    .expect("the same version again is the idempotent replay");

    let refusal = catalog_version_ref_repo::finalize(
        &conn,
        &h.scope,
        RefIdentity {
            tenant_id: TENANT,
            pending_ref: &pending,
            subject_kind: SubjectKind::Plan,
            subject_ref: &plan_id.get().to_string(),
        },
        CatalogVersion::new(6),
        at(14),
    )
    .await
    .expect_err("one handle, two versions, is an invariant breach");
    assert!(
        matches!(refusal, RepoError::CorruptRow(_)),
        "no new variant and no wire code - this path has no client: {refusal:?}"
    );

    let rows = deltas(&h).await;
    assert_eq!(rows.len(), 1, "the subject was projected once, at V5");
    assert_eq!(rows[0].catalog_version, 5);
    assert_eq!(
        refs(&h).await[0].catalog_version,
        Some(5),
        "the refusal moved nothing"
    );
    assert_eq!(frontier_version(&h).await, Some(5));
}

// ---------------------------------------------------------------------------
// 8 + 9. The projection source.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn retirement_carries_its_own_version_rather_than_leaking_into_an_older_one() {
    // D-128 against a database, and it is a statement about **two** versions.
    // Retirement is a publish unit of its own precisely because a retired plan
    // can never publish again, so nothing later would re-project it - and being
    // a unit, it requests its own `CatalogVersion` and pins its own state.
    //
    // So V5, published before the retirement, keeps saying `published`: at V5
    // the plan really was sellable, and a delta that said otherwise would be a
    // frozen version changing its mind. V6, the retirement's own version, says
    // `retired`, which is what sellability predicate (4) reads at the pin.
    let h = harness().await;
    let (plan_id, pending_v5) = seed_and_publish_at(&h, "gold", at_min(12, 0)).await;

    // The retirement unit: the flip, and the ref it would request. Written by
    // statement because retirement has no writer in this gear at all - the
    // group that lands it is not this one - and the flip is one the
    // `pricing_plan` trigger whitelist explicitly sanctions.
    retire(&h, plan_id).await;
    let conn = h.provider.conn().expect("conn");
    catalog_version_ref_repo::record_pending(
        &conn,
        &h.scope,
        PendingVersionRow::for_subject(
            TENANT,
            "pend-retire".to_owned(),
            &SubjectRef::Plan(plan_id.get()),
            Some(0),
            Some(LifecycleState::Retired),
            at_min(12, 1),
        ),
    )
    .await
    .expect("record the retirement unit's ref");

    h.registry.commit(&pending_v5, 5);
    h.registry.commit("pend-retire", 6);
    sweep(&h, at(13)).await;

    let rows = deltas(&h).await;
    assert_eq!(rows.len(), 2, "one delta per version, same subject");
    let v5 = rows
        .iter()
        .find(|row| row.catalog_version == 5)
        .expect("V5's delta");
    let v6 = rows
        .iter()
        .find(|row| row.catalog_version == 6)
        .expect("V6's delta");

    assert_eq!(
        v5.payload.get("lifecycleState"),
        Some(&serde_json::json!("published")),
        "the retirement did not leak backwards into a version that froze before it"
    );
    assert_eq!(
        v6.payload.get("lifecycleState"),
        Some(&serde_json::json!("retired")),
        "and it arrives carrying its own version"
    );
    assert_eq!(frontier_version(&h).await, Some(6));
}

#[tokio::test]
async fn a_delta_never_freezes_a_superseded_lifecycle_state() {
    // The residue pinning the revision alone left, closed. Read live, the
    // pinned revision's state is `superseded` the moment its successor commits
    // - a third value D-128 does not contemplate for a projected subject, and
    // one `load_current` could never return. Frozen into an INSERT-only delta
    // on the seven-year horizon it makes the version read unsellable to a
    // consumer coding sellability predicate (4) as "is published", which is how
    // D-90 names that predicate's input.
    let h = harness().await;
    let plan_id = PlanId::new(Uuid::new_v4());
    let (rev0, version0) = seed_publishable(&h, plan_id, "gold").await;
    let pending_v5 = publish(&h, plan_id, rev0, version0, at_min(12, 0)).await;

    // The successor publishes before V5 warms, so revision 0 is `superseded` in
    // the truth table by the time the sweep arrives.
    let opened = h
        .plans
        .open_revision(&h.scope, TENANT, plan_id, stamp_of(ACTOR, at_min(12, 1)))
        .await
        .expect("open a successor");
    let pending_v6 = publish(
        &h,
        plan_id,
        opened.revision,
        opened.row_version,
        at_min(12, 2),
    )
    .await;
    assert_eq!(
        h.plans
            .find_revision(&h.scope, TENANT, plan_id, rev0)
            .await
            .expect("read revision 0")
            .expect("it is there")
            .lifecycle_state,
        LifecycleState::Superseded,
        "the truth row really is superseded, which is what makes this a real case"
    );

    h.registry.commit(&pending_v5, 5);
    h.registry.commit(&pending_v6, 6);
    sweep(&h, at(13)).await;

    let rows = deltas(&h).await;
    for row in &rows {
        assert_ne!(
            row.payload.get("lifecycleState"),
            Some(&serde_json::json!("superseded")),
            "no delta may carry a state D-128 does not contemplate: {row:?}"
        );
    }
    assert_eq!(
        rows.iter()
            .find(|row| row.catalog_version == 5)
            .expect("V5's delta")
            .payload
            .get("lifecycleState"),
        Some(&serde_json::json!("published")),
        "V5 froze the state its own publish judged"
    );
}

#[tokio::test]
async fn the_open_draft_revision_is_never_the_projection_source() {
    // Sec 4.4's 2026-07-30 review fix, as behaviour: a degraded re-drive must
    // not leak draft edits into a frozen version. It is a property of calling
    // load_current and never load_open_draft, so the assertion is that a draft
    // carrying different content changes nothing about the delta.
    let h = harness().await;
    let (plan_id, pending) = seed_and_publish_at(&h, "gold", at_min(12, 0)).await;

    // A successor revision, open and edited, with a tier the published one
    // does not have.
    let opened = h
        .plans
        .open_revision(&h.scope, TENANT, plan_id, stamp_of(ACTOR, at(12)))
        .await
        .expect("open a successor draft");
    h.plans
        .update_draft(
            &h.scope,
            TENANT,
            plan_id,
            opened.revision,
            opened.row_version,
            PlanShapePatch {
                plan_tier: Some("platinum".to_owned()),
                ..PlanShapePatch::default()
            },
            stamp(),
        )
        .await
        .expect("edit the draft");

    h.registry.commit(&pending, 1);
    sweep(&h, at(13)).await;

    let rows = deltas(&h).await;
    assert_eq!(
        rows[0].payload.get("planTier"),
        Some(&serde_json::json!("gold")),
        "the current revision's content, never the open draft's"
    );
    assert_eq!(
        rows[0].payload.get("revision"),
        Some(&serde_json::json!(0)),
        "and the current revision's number with it"
    );
}

#[tokio::test]
async fn a_version_freezes_the_revision_its_own_publish_judged() {
    // The reviewer's reproduction, and the reason the ref row carries the
    // revision. sec 4.4 sources the projection from the plan's CURRENT revision,
    // and the sweep arrives up to the D-47 maximum batching delay - five
    // minutes - after the commit. A second publish of the same plan inside that
    // window makes its own revision current, so "current" freezes content the
    // earlier version's publish never judged. Permanently: a delta is
    // INSERT-only on the seven-year horizon, in a store whose whole contract is
    // that a completed version never changes.
    let h = harness().await;
    let plan_id = PlanId::new(Uuid::new_v4());
    let (rev0, version0) = seed_publishable(&h, plan_id, "gold").await;
    let pending_v5 = publish(&h, plan_id, rev0, version0, at_min(12, 0)).await;

    // A successor revision with different content, published before the first
    // version's warm.
    let opened = h
        .plans
        .open_revision(&h.scope, TENANT, plan_id, stamp_of(ACTOR, at_min(12, 1)))
        .await
        .expect("open a successor");
    let edited = h
        .plans
        .update_draft(
            &h.scope,
            TENANT,
            plan_id,
            opened.revision,
            opened.row_version,
            PlanShapePatch {
                plan_tier: Some("platinum".to_owned()),
                ..PlanShapePatch::default()
            },
            stamp(),
        )
        .await
        .expect("edit the successor");
    let pending_v6 = publish(
        &h,
        plan_id,
        opened.revision,
        edited.row_version,
        at_min(12, 2),
    )
    .await;

    // Both batches commit, and one sweep resolves both.
    h.registry.commit(&pending_v5, 5);
    h.registry.commit(&pending_v6, 6);
    sweep(&h, at(13)).await;

    let rows = deltas(&h).await;
    assert_eq!(rows.len(), 2, "one delta per version");
    let v5 = rows
        .iter()
        .find(|row| row.catalog_version == 5)
        .expect("V5's delta");
    let v6 = rows
        .iter()
        .find(|row| row.catalog_version == 6)
        .expect("V6's delta");

    assert_eq!(
        v5.payload.get("revision"),
        Some(&serde_json::json!(rev0)),
        "V5 froze the revision its own publish judged"
    );
    assert_eq!(
        v5.payload.get("planTier"),
        Some(&serde_json::json!("gold")),
        "and that revision's content, not the one that overtook it"
    );
    assert_eq!(
        v6.payload.get("revision"),
        Some(&serde_json::json!(opened.revision))
    );
    assert_eq!(
        v6.payload.get("planTier"),
        Some(&serde_json::json!("platinum"))
    );
}

// ---------------------------------------------------------------------------
// 10. The degraded observation, and the instant that separates it from the
//     overdue one (D-166).
// ---------------------------------------------------------------------------

/// A ref whose commit the registry answers and whose subject can never project,
/// so the pass observes the commit and the warm keeps failing — the one state
/// `fr-publish-fanout-atomicity` names and the merged predicate could not.
///
/// Returns the handle.
async fn seed_observed_but_unwarm(
    h: &Harness,
    version: u64,
    requested_at: OffsetDateTime,
) -> String {
    let conn = h.provider.conn().expect("conn");
    catalog_version_ref_repo::record_pending(
        &conn,
        &h.scope,
        PendingVersionRow::for_subject(
            TENANT,
            "pend-stuck".to_owned(),
            &SubjectRef::Plan(Uuid::new_v4()),
            Some(0),
            Some(LifecycleState::Published),
            requested_at,
        ),
    )
    .await
    .expect("record the stuck subject");
    h.registry.commit("pend-stuck", version);
    "pend-stuck".to_owned()
}

/// The same stuck state on an **overlay** subject (D-234's plane).
async fn seed_observed_but_unwarm_overlay(
    h: &Harness,
    version: u64,
    requested_at: OffsetDateTime,
) -> String {
    let conn = h.provider.conn().expect("conn");
    catalog_version_ref_repo::record_pending(
        &conn,
        &h.scope,
        PendingVersionRow::for_subject(
            TENANT,
            "pend-overlay".to_owned(),
            &SubjectRef::PriceOverlay(Uuid::new_v4()),
            Some(0),
            Some(LifecycleState::Published),
            requested_at,
        ),
    )
    .await
    .expect("record the stuck overlay subject");
    h.registry.commit("pend-overlay", version);
    "pend-overlay".to_owned()
}

#[tokio::test]
async fn an_overlay_subjects_stuck_warm_raises_the_frontier_alarm_and_no_degraded_event() {
    // **D-237's argument, as a case rather than as a paragraph.** `mark_degraded`
    // refuses every non-plan subject, and since D-234 the projector genuinely
    // dispatches overlay subjects — so the refusal is a real silence on a real
    // state, and the question is whether that silence is a hole.
    //
    // It is not, and this is what says so: `PlanPublishDegraded` carries a
    // `planId` and is addressed to consumers of that plan; the subject-generic
    // hazard — *this tenant's pins are not advancing* — is carried by
    // `pin_eligibility_overdue`, whose predicate reads tenant and age and never
    // subject kind. A version is incomplete exactly while it holds a pending ref,
    // so the overlay row holds the frontier for the whole tenant.
    //
    // Both halves are asserted, because either alone would pass against the bug:
    // the silence alone would also pass if nothing alarmed at all.
    let h = harness().await;
    let _pending = seed_observed_but_unwarm_overlay(&h, 5, at_min(12, 1)).await;

    sweep(&h, at(13)).await;
    let outside = sweep(&h, at(13) + time::Duration::seconds(5)).await;

    assert_eq!(
        outside.degraded_emitted, 0,
        "the plan-shaped event stays silent for a subject that has no plan"
    );
    assert!(
        degraded_events(&h).await.is_empty(),
        "and nothing reached the outbox under a payload it could not fill"
    );
    assert_eq!(
        outside.pin_eligibility_overdue, 1,
        "while the alarm that does cover it speaks - this is the half that makes \
         the silence acceptable rather than a hole"
    );
    assert_eq!(
        frontier_version(&h).await,
        None,
        "and the frontier it holds has not advanced, so no consumer can pin a \
         version whose overlay is unwarmed"
    );
}

/// A ref's recorded commit observation.
async fn observed_at(h: &Harness, pending_ref: &str) -> Option<OffsetDateTime> {
    refs(h)
        .await
        .into_iter()
        .find(|row| row.pending_ref == pending_ref)
        .expect("the ref is there")
        .commit_observed_at
}

#[tokio::test]
async fn a_pending_ref_the_registry_has_not_answered_is_overdue_and_not_degraded() {
    // D-166 clause (4). Before the observation instant existed BOTH signals
    // fired here, off one clock, so an operator could not tell "the registry has
    // not answered" from "the registry answered and the warm is failing" - the
    // one distinction `fr-publish-fanout-atomicity` exists to draw.
    let h = harness().await;
    let (_, pending) = seed_and_publish_at(&h, "gold", at_min(12, 0)).await;
    h.registry.script(&pending, vec![None]);

    // Requested at 12:00; the ratified max batching delay is five minutes, so by
    // 13:00 the registry is overdue - and by 14:00 it still is.
    let first = sweep(&h, at(13)).await;
    assert_eq!(first.commit_overdue, 1);
    assert_eq!(
        first.degraded_emitted, 0,
        "there is nothing degraded about a publish whose version does not exist yet"
    );
    let second = sweep(&h, at(14)).await;
    assert_eq!(second.commit_overdue, 1, "still unanswered, still alarming");
    assert_eq!(second.degraded_emitted, 0);

    assert!(
        degraded_events(&h).await.is_empty(),
        "and no event was enqueued for it"
    );
    assert_eq!(
        observed_at(&h, &pending).await,
        None,
        "nothing was observed, so nothing is stamped"
    );
}

#[tokio::test]
async fn a_registry_that_errors_still_trips_the_commit_overdue_alarm() {
    // sec 3.6 conditions `commit_overdue` on the ref's age and on whether the
    // commit has been observed - never on what the registry said THIS pass. An
    // earlier version raised it only when the registry answered "not yet", so a
    // registry that ERRORED left the ref ageing in silence however long it stood,
    // which is the outage the alarm most needs to name.
    let h = harness().await;
    let (_, pending) = seed_and_publish_at(&h, "gold", at_min(12, 0)).await;
    // No answer is scripted, and the double answers `Ok(None)` for an unknown
    // handle; script an outage instead.
    h.registry
        .fail_with(&registry_unreachable("down".to_owned()));

    let report = sweep(&h, at(13)).await;

    assert_eq!(report.versions_projected, 0);
    assert_eq!(
        report.commit_overdue, 1,
        "the ref aged past the SLO with no observation, whatever the registry said"
    );
    assert_eq!(
        report.degraded_emitted, 0,
        "an unreachable registry is not a warm that is failing"
    );
    assert_eq!(observed_at(&h, &pending).await, None);
}

#[tokio::test]
async fn an_observed_commit_is_stamped_even_though_its_projection_fails() {
    // The placement is the mechanism. The projector finalizes a ref and writes
    // its delta warm in ONE transaction, so an observation stamped inside it
    // would roll back on exactly the path the degraded signal exists for.
    //
    // Delete `observe_commit`'s call in `resolve` and this test fails, along
    // with every degraded assertion below.
    let h = harness().await;
    let pending = seed_observed_but_unwarm(&h, 5, at_min(12, 1)).await;

    let report = sweep(&h, at(13)).await;

    assert_eq!(report.subjects_failed, 1, "the subject cannot project");
    assert_eq!(
        observed_at(&h, &pending).await,
        Some(at(13)),
        "and the observation is recorded anyway"
    );
    assert_eq!(
        refs(&h)
            .await
            .into_iter()
            .find(|row| row.pending_ref == pending)
            .expect("the ref is there")
            .catalog_version,
        None,
        "while the ref itself is still unresolved - the state that is unreachable \
         without the column"
    );
}

#[tokio::test]
async fn the_observation_instant_is_the_first_sighting_and_does_not_move() {
    // Write-once, and it is the whole of whether the signal ever raises: a stamp
    // that advanced every pass would hold the degraded age at zero forever.
    let h = harness().await;
    let pending = seed_observed_but_unwarm(&h, 5, at_min(12, 1)).await;

    sweep(&h, at_min(13, 0)).await;
    sweep(&h, at_min(13, 30)).await;

    assert_eq!(
        observed_at(&h, &pending).await,
        Some(at_min(13, 0)),
        "the second pass re-observed the same commit and left the instant alone"
    );
}

#[tokio::test]
async fn an_observed_commit_is_degraded_only_once_the_propagation_slo_has_passed() {
    // D-166 clause (2), both directions. sec 1.2's SLO is 5s, and the pass that
    // first observes the commit observes it at its own `now` - so the pass that
    // stamps can never be the pass that alarms, which is the correct shape: the
    // warm was attempted in that same pass.
    let h = harness().await;
    let pending = seed_observed_but_unwarm(&h, 5, at_min(12, 1)).await;

    let stamping = sweep(&h, at(13)).await;
    assert_eq!(
        stamping.degraded_emitted, 0,
        "an observation zero seconds old is inside the SLO"
    );

    let inside = sweep(&h, at(13) + time::Duration::seconds(4)).await;
    assert_eq!(
        inside.degraded_emitted, 0,
        "four seconds after the observation is still inside it"
    );

    let outside = sweep(&h, at(13) + time::Duration::seconds(5)).await;
    assert_eq!(
        outside.degraded_emitted, 1,
        "five seconds is the SLO, and the warm has not landed"
    );
    assert_eq!(
        outside.commit_overdue, 0,
        "and it is NOT overdue - the registry answered"
    );

    let events = degraded_events(&h).await;
    assert_eq!(events.len(), 1, "one degradation, one event");
    assert_eq!(
        events[0].payload.get("catalogVersion"),
        Some(&serde_json::json!(5)),
        "the event names the publish by its version, which D-166 makes knowable"
    );
    assert_eq!(
        events[0].payload.get("commitObservedAt"),
        Some(&serde_json::json!(format_rfc3339(at(13)))),
        "and measures the wait from the observation, not from the request"
    );
    assert_eq!(
        events[0].payload.get("pendingVersionRef"),
        Some(&serde_json::json!(pending)),
        "the handle rides as lineage"
    );
    assert_eq!(
        events[0].published_at, None,
        "the relay drains, not the sweep"
    );

    // The dedup index makes a repeat of one degradation one event.
    let again = sweep(&h, at(14)).await;
    assert_eq!(
        again.degraded_emitted, 0,
        "the dedup index refused the repeat"
    );
    assert_eq!(degraded_events(&h).await.len(), 1);
}

#[tokio::test]
async fn an_observed_commit_is_never_overdue_however_long_it_stays_unresolved() {
    // D-166 clause (4)'s other side, and the reason the two signals are now
    // disjoint over one clock rather than merely differently thresholded: the
    // ref below is HOURS past the max batching delay and the registry answered
    // it, so `commit_overdue` must stay silent whatever its age.
    let h = harness().await;
    let _ = seed_observed_but_unwarm(&h, 5, at_min(12, 1)).await;

    sweep(&h, at(13)).await;
    let much_later = sweep(&h, at(23)).await;
    let later_still = sweep(&h, at(23) + time::Duration::hours(5)).await;

    assert_eq!(
        much_later.commit_overdue, 0,
        "the registry answered; ageing past its budget says nothing about it now"
    );
    assert_eq!(
        later_still.commit_overdue, 0,
        "and it stays silent however long the warm keeps failing"
    );
    assert_eq!(
        degraded_events(&h).await.len(),
        1,
        "the degradation is reported once and by the other signal"
    );
}

#[tokio::test]
async fn every_pending_state_lands_on_exactly_one_signal_or_none() {
    // The table D-166 clause (4) closes with: "nothing goes silent". Four
    // combinations of (observed?, age past its own threshold?), each asserted to
    // produce exactly one of the two signals or neither - which is what a merged
    // predicate could not express, both of its arms being true at once.
    //
    // The tenant has no frontier, so `pin_eligibility_overdue` is a THIRD signal
    // riding along on the observed cases; it is asserted where it belongs
    // (`a_stale_frontier_...`) and read here only to show it is not what carries
    // these two.
    for (unanswered, earlier_pass, now, overdue, degraded, label) in [
        (
            true,
            None,
            at_min(12, 1),
            0,
            0,
            "unanswered and inside the batching SLO: neither",
        ),
        (
            true,
            None,
            at(13),
            1,
            0,
            "unanswered and past the batching SLO: overdue only",
        ),
        (
            false,
            None,
            at_min(12, 1),
            0,
            0,
            "answered, warm attempted this pass, observation zero seconds old: neither",
        ),
        (
            false,
            Some(at_min(12, 1)),
            at(13),
            0,
            1,
            "answered and unwarm past the propagation SLO: degraded only",
        ),
    ] {
        let h = harness().await;
        let pending = seed_observed_but_unwarm(&h, 5, at_min(12, 0)).await;
        if unanswered {
            h.registry.script(&pending, vec![None]);
        }
        if let Some(stamping) = earlier_pass {
            // Stamp the observation on an earlier pass, so the age measured at
            // `now` is the observation's and not zero.
            sweep(&h, stamping).await;
        }

        let report = sweep(&h, now).await;

        // **Asserted over the report, because over the loop's own literals it was
        // decided at authoring time.** `overdue == 0 || degraded == 0` reads two
        // constants destructured from the table below and can never fail for any
        // behaviour of `sweep` -- so the invariant it names was asserted nowhere
        // against the thing that could violate it (2026-08-21 review, finding 9).
        // Over `report.*` it is a claim about this scenario's actual sweep, and it
        // stands ahead of the two `assert_eq!`s rather than behind them so a sweep
        // emitting both signals is named as that fault rather than as a count
        // mismatch.
        //
        // The table's own completeness is a separate matter and stays a comment: the
        // four rows are the whole product of (answered?, past the SLO?), so there is
        // no fifth combination for a wider loop to range over, and no row expects
        // both signals.
        assert!(
            report.commit_overdue == 0 || report.degraded_emitted == 0,
            "the two signals may never both fire on one ref, and this sweep emitted \
             {} overdue beside {} degraded: {label}",
            report.commit_overdue,
            report.degraded_emitted
        );
        assert_eq!(report.commit_overdue, overdue, "{label}");
        assert_eq!(report.degraded_emitted, degraded, "{label}");
    }
}

#[tokio::test]
async fn a_committed_version_whose_projection_keeps_failing_holds_the_frontier_and_says_so() {
    // sec 4.4's own words: "a stuck version now holds the frontier, which is
    // exactly what that alarm signals". Under D-166 this path is DEGRADED rather
    // than overdue - the registry answered - and `pin_eligibility_overdue` is
    // what says the frontier is held.
    let h = harness().await;
    let (_, pending_a) = seed_and_publish_at(&h, "gold", at_min(12, 0)).await;
    let _ = seed_observed_but_unwarm(&h, 5, at_min(12, 1)).await;
    h.registry.commit(&pending_a, 5);

    sweep(&h, at(13)).await;
    let report = sweep(&h, at(14)).await;

    assert_eq!(report.subjects_failed, 1, "the stuck subject refuses");
    assert_eq!(
        frontier_version(&h).await,
        None,
        "so V5 never becomes pinnable"
    );
    assert_eq!(
        report.commit_overdue, 0,
        "the registry answered, so this is not its fault"
    );
    assert_eq!(
        report.degraded_emitted, 1,
        "the version committed and the warm has not landed"
    );
    assert_eq!(
        report.pin_eligibility_overdue, 1,
        "and a committed version stands short of pin-eligibility with no frontier at all"
    );
}

#[tokio::test]
async fn an_ordinary_publish_behind_an_old_frontier_does_not_alarm() {
    // The second half of the same regression. A tenant that published an hour
    // ago has a stale frontier by any measure, and a fresh publish of it is
    // simply waiting out D-47's budget - so staleness conjoined with "has a
    // pending ref" would raise Critical on a healthy catalog every tick. The
    // ref has to be overdue itself.
    let h = harness().await;
    let (_, first) = seed_and_publish_at(&h, "gold", at_min(12, 0)).await;
    h.registry.commit(&first, 5);
    sweep(&h, at_min(12, 1)).await;
    assert_eq!(frontier_version(&h).await, Some(5));

    // An hour later, a new publish that the registry has not committed yet.
    let (_, second) = seed_and_publish_at(&h, "silver", at_min(13, 0)).await;
    h.registry.script(&second, vec![None]);

    let report = sweep(&h, at_min(13, 1)).await;

    assert_eq!(
        report.pin_eligibility_overdue, 0,
        "the frontier is an hour old and the publish is one minute old; nothing is stuck"
    );
    assert_eq!(report.commit_overdue, 0);
}

#[tokio::test]
async fn a_stale_frontier_with_a_version_waiting_behind_it_alarms_on_its_own_age() {
    // D-136 and PinFrontier::advanced_at both name the frontier's age as this
    // alarm's referent. The tenant below has a frontier, so nothing about a
    // pending ref's age is what makes it overdue - the frontier itself has not
    // moved within the SLO while a committed version waits.
    let h = harness().await;
    let (_, pending_a) = seed_and_publish_at(&h, "gold", at_min(12, 0)).await;
    h.registry.commit(&pending_a, 5);
    sweep(&h, at_min(12, 1)).await;
    assert_eq!(frontier_version(&h).await, Some(5));

    // A later version whose subject cannot project, so V6 stands committed and
    // short of pin-eligibility while the frontier ages at V5.
    let conn = h.provider.conn().expect("conn");
    catalog_version_ref_repo::record_pending(
        &conn,
        &h.scope,
        PendingVersionRow::for_subject(
            TENANT,
            "pend-stuck".to_owned(),
            &SubjectRef::Plan(Uuid::new_v4()),
            Some(0),
            Some(LifecycleState::Published),
            at_min(12, 2),
        ),
    )
    .await
    .expect("record the stuck subject");
    h.registry.commit("pend-stuck", 6);

    let report = sweep(&h, at(14)).await;

    assert_eq!(
        report.pin_eligibility_overdue, 1,
        "the frontier last advanced at 12:01 and a committed V6 is waiting"
    );
}

// ---------------------------------------------------------------------------
// The inert pass G7's e2e depends on.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_sweep_with_no_registry_configured_is_inert() {
    // G7's e2e boots without a registry. A pass that logged an error every five
    // seconds would make that boot unreadable, so the unconfigured answer
    // returns the pass at debug: no alarm, no error, nothing written.
    let db = connect_db("sqlite::memory:", ConnectOpts::default())
        .await
        .expect("connect in-memory sqlite");
    run_migrations_for_testing(&db, Migrator::migrations())
        .await
        .expect("run migrator");
    let provider = DBProvider::<DbError>::new(db);
    // The tenant declares the regions its rows sell in. `inst-tx-region` is
    // registered in the Foundation rule set and C2 is fail-closed, so a publish
    // by a tenant with an empty region taxonomy is refused — which is correct,
    // and which every fixture here would otherwise trip on a rule none of them
    // is about.
    common::declare_fixture_regions(&provider, TENANT).await;
    common::declare_fixture_regions(&provider, OTHER_TENANT).await;
    let registry = Arc::new(RegistryDouble::default());
    let h = Harness {
        plans: PlanRepo::new(provider.clone()),
        shapes: PlanShapeRepo::new(provider.clone()),
        prices: PriceRepo::new(provider.clone()),
        publish: PublishService::new(
            provider.clone(),
            &LimitsConfig::default(),
            FixtureGate::load(&committed_registry_path()),
            Arc::clone(&registry) as Arc<dyn CatalogVersionRegistryV1>,
        ),
        frontier: PinFrontierRepo::new(provider.clone()),
        job: ReadModelWarmJob::new(
            provider.clone(),
            Arc::new(
                bss_pricing_sdk::catalog_version_registry::UnconfiguredCatalogVersionRegistryV1,
            ),
            JobsConfig::default(),
        ),
        registry,
        scope: AccessScope::for_tenant(TENANT),
        provider,
        metrics: bss_pricing::infra::metrics::test_harness::MetricsHarness::new(),
    };
    let (_, _) = seed_and_publish_at(&h, "gold", at_min(12, 0)).await;

    let report = sweep(&h, at(13)).await;

    assert!(report.inert, "no registry means no pass");
    assert_eq!(report.versions_projected, 0);
    assert_eq!(
        report.commit_overdue, 0,
        "an absent registry is not an alarm"
    );
    assert!(deltas(&h).await.is_empty());
    assert_eq!(frontier_version(&h).await, None);
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

// ---------------------------------------------------------------------------
// 8. The window facts the projector freezes (D-99, D-121).
// ---------------------------------------------------------------------------

/// `2099-09-01T00:00:00Z` plus `day` days. Deliberately not [`at`]: window
/// instants have to be orderable over a span longer than one day and legibly
/// distinct from the publish instants around them.
///
/// **The year moves with `common::COVERAGE_TO_UTC` and has to.** `window_at(0)` is
/// where the shared fixture window ends, which is what makes every window this
/// suite schedules adjacent to it or later instead of overlapping it — and the pair
/// is dated 2099 so that no real clock is ever inside it. Dating this scale in 2026
/// while the fixture sat in 2099 would put the fixture *after* every window here,
/// making it the far end of every key's coverage and swamping the `coverageEnd`
/// assertions this section exists for.
fn window_at(day: i64) -> OffsetDateTime {
    utc_ymd_hms(2099, 9, 1, 0, 0, 0) + time::Duration::days(day)
}

/// The inclusive start of the coverage window `seed_publishable_of` schedules.
///
/// Read off `common`'s constants rather than restated, so a fixture assertion
/// cannot disagree with the fixture.
fn coverage_window_from() -> OffsetDateTime {
    let (y, m, d) = common::COVERAGE_FROM_UTC;
    utc_ymd_hms(y, m, d, 0, 0, 0)
}

/// Cancel the coverage window `seed_publishable_of` scheduled on `price_id`.
///
/// For the one case whose property is a key with **no** surviving window: the
/// seed's window is content like any other and has to be removed the way the
/// system removes one, through §4's `scheduled -> cancelled` edge.
async fn cancel_coverage_window(h: &Harness, price_id: Uuid) {
    let conn = h.provider.conn().expect("conn");
    window_repo::transition(
        &conn,
        &h.scope,
        TENANT,
        common::coverage_window_id(price_id),
        WindowState::Cancelled,
        coverage_window_from(),
        stamp(),
    )
    .await
    .expect("cancel the seed's coverage window");
}

/// The one `published` price row a seeded plan has, as its stored row.
///
/// The whole row rather than its id, because the suites below copy its eight
/// canonical axes to build a sibling on the same key, and read its `phase` to
/// build one on a different key.
async fn published_price(h: &Harness, plan_id: PlanId) -> price::Model {
    let conn = h.provider.conn().expect("conn");
    let rows = price::Entity::find()
        .secure()
        .scope_with(&h.scope)
        .filter(
            Condition::all()
                .add(price::Column::TenantId.eq(TENANT))
                .add(price::Column::PlanId.eq(plan_id.get()))
                .add(price::Column::LifecycleState.eq("published")),
        )
        .all(&conn)
        .await
        .expect("read the plan's price rows");
    assert_eq!(rows.len(), 1, "the fixture publishes exactly one row");
    rows[0].clone()
}

/// The one `published` price row a seeded plan has.
async fn published_price_id(h: &Harness, plan_id: PlanId) -> Uuid {
    published_price(h, plan_id).await.price_id
}

/// A never-published `draft` price row with **every canonical axis copied off
/// `source`**, so the two rows share one canonical scope key.
///
/// Written through the entity rather than through `PriceRepo::create_draft`, for
/// `tests/sqlite_window_repo.rs`'s reason one table over: `find_key_occupant`
/// refuses a new draft on a key a `published` row already holds, so the **authoring**
/// door cannot build this world — and it is nonetheless a **legal** world and a
/// produced one. Foundation §3.7's two partial `UNIQUE` predicates are disjoint
/// (only two *published* rows on a key collide), and the door that does produce it
/// is `price_repo::insert_successor_draft_on` (D-195, D-88's row half). The seeding
/// is deliberately **not** moved onto it: this suite is about what the read model
/// projects from a pair of rows, and it should not acquire the supersession door's
/// preconditions — a predecessor with current coverage, no draft already standing —
/// as incidental setup for an assertion about projection.
/// The insert still goes through the tenant gate, so the seeding cannot fabricate
/// a row the scope would not admit.
async fn draft_row_on_the_same_key(h: &Harness, source: &price::Model, draft_id: Uuid) {
    let conn = h.provider.conn().expect("conn");
    let row = price::ActiveModel {
        price_id: Set(draft_id),
        tenant_id: Set(source.tenant_id),
        // The ten axes, copied rather than restated: a literal here that drifted
        // from the fixture's key would put the draft on a key of its own and the
        // test would pass against the defect.
        //
        // **`meter` and `dimension_key` were left to the column defaults until
        // D-296**, which is that drift by another route: eight axes copied, two
        // taken from the table. Correct only for as long as the seeded source
        // stayed `recurring` — a usage source would have put the "same key" draft
        // on a different key, silently.
        plan_id: Set(source.plan_id),
        currency: Set(source.currency.clone()),
        region: Set(source.region.clone()),
        price_overlay: Set(source.price_overlay.clone()),
        phase: Set(source.phase),
        price_eligibility: Set(source.price_eligibility.clone()),
        charge_kind: Set(source.charge_kind.clone()),
        cohort: Set(source.cohort.clone()),
        meter: Set(source.meter.clone()),
        dimension_key: Set(source.dimension_key.clone()),
        amount_minor: Set(source.amount_minor),
        model_kind: Set(source.model_kind.clone()),
        lifecycle_state: Set(LifecycleState::Draft.as_str().to_owned()),
        created_by: Set(source.created_by),
        created_at_utc: Set(source.created_at_utc),
        ..price::ActiveModel::default()
    };
    price::Entity::insert(row.clone())
        .secure()
        .scope_with_model(&h.scope, &row)
        .expect("scope the seeded draft row")
        .exec(&conn)
        .await
        .unwrap_or_else(|e| panic!("seed draft price row {draft_id}: {e}"));
}

/// The `windows` groups of one delta.
fn window_groups(delta: &read_model::Model) -> Vec<serde_json::Value> {
    delta
        .payload
        .get("windows")
        .expect("the delta carries the window facts")
        .as_array()
        .expect("array")
        .clone()
}

/// The canonical keys `prices` enumerates and the ones `windows` does, rendered
/// by the same axis-by-axis renderer and therefore directly comparable.
fn key_sets(delta: &read_model::Model) -> (Vec<serde_json::Value>, Vec<serde_json::Value>) {
    let keys = |field: &str| -> Vec<serde_json::Value> {
        delta
            .payload
            .get(field)
            .and_then(|v| v.as_array())
            .unwrap_or_else(|| panic!("the delta carries {field}"))
            .iter()
            .map(|entry| entry.get("scopeKey").expect("a scope key").clone())
            .collect()
    };
    (keys("prices"), keys("windows"))
}

/// Schedule a window on `price_id` and walk it to `state`.
async fn window(
    h: &Harness,
    price_id: Uuid,
    id: u128,
    from: OffsetDateTime,
    to: Option<OffsetDateTime>,
    state: WindowState,
) {
    let conn = h.provider.conn().expect("conn");
    let window_id = Uuid::from_u128(id);
    window_repo::schedule(
        &conn,
        &h.scope,
        NewWindow {
            window_id,
            tenant_id: TENANT,
            price_id,
            effective_from: from,
            effective_to: to,
            reason_code: "priceIncrease".to_owned(),
        },
        stamp(),
    )
    .await
    .unwrap_or_else(|e| panic!("schedule window {id}: {e}"));

    // Walk §4's edges rather than writing the state: `scheduled -> expired` is
    // not an edge, so an `expired` window in this fixture is one that genuinely
    // activated first — which is the state a predecessor is in after a
    // changeover, and the state this test is about.
    //
    // **Each flip is stamped at the instant §4 puts it at** — an activation at
    // `effectiveFrom`, an expiry at `effectiveTo` — because the edges are
    // conditional (`inst-ws-activate` WHEN `now >= effectiveFrom`,
    // `inst-ws-expire` WHEN `now >= effectiveTo`) and the store carries those
    // conditions into the statement. This fixture used to expire the window one
    // second after activating it, ten days before its end: a window that stopped
    // applying before the interval it was projected over had run, which is
    // exactly what the read model here is asked to project.
    let path: Vec<(WindowState, OffsetDateTime)> = match state {
        WindowState::Scheduled => vec![],
        WindowState::Active => vec![(WindowState::Active, from)],
        WindowState::Expired => vec![
            (WindowState::Active, from),
            (
                WindowState::Expired,
                to.expect("an expired window is one whose end arrived, so it has one"),
            ),
        ],
        WindowState::Cancelled => vec![(WindowState::Cancelled, from)],
    };
    for (next, flip_at) in path {
        window_repo::transition(&conn, &h.scope, TENANT, window_id, next, flip_at, stamp())
            .await
            .unwrap_or_else(|e| panic!("window {id} -> {next}: {e}"));
    }
}

/// D-121's projected window states, through a real store: a **cancelled** window
/// is not projected and an **expired** one is.
///
/// The world is what makes this the filter's test. All four states are present on
/// one canonical scope key, so a projector that projected everything and a
/// projector that projected only what is currently in force would both be
/// distinguishable from the right one — and the `expired` window is reached by
/// walking §4's edges rather than by writing the token, so it is genuinely a
/// window that took effect and ended, which is the state a superseded
/// predecessor's interval is in when an arrears period at a past instant resolves
/// against it.
#[tokio::test]
async fn a_cancelled_window_is_not_projected_and_an_expired_one_is() {
    let h = harness().await;
    let (plan_id, pending) = seed_and_publish_at(&h, "gold", at(12)).await;
    let price_id = published_price_id(&h, plan_id).await;

    window(
        &h,
        price_id,
        0x_a1,
        window_at(10),
        Some(window_at(20)),
        WindowState::Expired,
    )
    .await;
    window(
        &h,
        price_id,
        0x_a2,
        window_at(20),
        Some(window_at(30)),
        WindowState::Active,
    )
    .await;
    window(
        &h,
        price_id,
        0x_a3,
        window_at(30),
        Some(window_at(40)),
        WindowState::Scheduled,
    )
    .await;
    window(
        &h,
        price_id,
        0x_a4,
        window_at(40),
        Some(window_at(50)),
        WindowState::Cancelled,
    )
    .await;

    h.registry.commit(&pending, 1);
    sweep(&h, at(13)).await;

    let deltas = deltas(&h).await;
    assert_eq!(deltas.len(), 1, "one subject, one delta");
    let groups = deltas[0]
        .payload
        .get("windows")
        .expect("the delta carries the window facts")
        .as_array()
        .expect("array")
        .clone();
    assert_eq!(groups.len(), 1, "one canonical scope key, one group");

    let states: Vec<&str> = groups[0]
        .get("intervals")
        .expect("intervals")
        .as_array()
        .expect("array")
        .iter()
        .map(|i| i.get("state").and_then(|s| s.as_str()).expect("state"))
        .collect();
    assert_eq!(
        states,
        // The seed's coverage window leads: `inst-wc-required` means the row
        // could not have published without one, and it is projected content like
        // any other. The three this case authored follow it in time order, and
        // the cancelled one is absent.
        ["scheduled", "expired", "active", "scheduled"],
        "the D-121 states, in time order, and the cancelled window absent"
    );

    // The derived coverage end is the far end of what is covered, and the
    // cancelled window's interval — which runs five days past it — contributes
    // nothing. Were it counted, the D-80 horizon would read this key as covered
    // through an interval nothing was ever effective on.
    assert_eq!(
        groups[0].get("coverageEnd"),
        Some(&serde_json::json!({ "kind": "ends", "at": format_rfc3339(window_at(40)) }))
    );
}

/// D-121's projected **row** set applies to the window facts too: a
/// never-published draft's window is not projected, and it does not move the
/// key's frozen coverage end.
///
/// # The world, and why nothing but the row filter can answer here
///
/// The draft carries every axis of the published row, so the two are on **one**
/// canonical scope key — which is legal (Foundation §3.7's two partial `UNIQUE`
/// predicates are disjoint) and is what a supersession authors. Its window is
/// `scheduled`, so [`PROJECTED_WINDOW_STATES`](bss_pricing::domain::projection)
/// admits it; it is on the same key, so the per-key grouping puts it in the
/// published row's group; and it runs forty days past the published row's own
/// coverage. The only fact that keeps it out is the lifecycle state of the **row**
/// it hangs off, which is why the assertion is on the frozen `coverageEnd`
/// **value** rather than on a count: a delta whose `prices` carries one row and
/// whose `coverageEnd` reads day 60 is exactly the defect, and a count assertion
/// passes against it.
///
/// What it costs when it is wrong: the pinned read model advertises coverage no
/// published row backs, and sellability predicate (1) reads the key as sellable
/// into an interval nothing published is effective over — the D-99 exposure,
/// arriving from the projection side.
#[tokio::test]
async fn a_draft_rows_window_does_not_move_the_published_keys_coverage_end() {
    let h = harness().await;
    let (plan_id, pending) = seed_and_publish_at(&h, "gold", at(12)).await;
    let published = published_price(&h, plan_id).await;

    // The published row's own coverage, and the whole of it.
    window(
        &h,
        published.price_id,
        0x_c1,
        window_at(10),
        Some(window_at(20)),
        WindowState::Active,
    )
    .await;

    // A never-published draft on the same key, carrying a window that runs forty
    // days past it.
    let draft_id = Uuid::from_u128(0x_d1);
    draft_row_on_the_same_key(&h, &published, draft_id).await;
    window(
        &h,
        draft_id,
        0x_c2,
        window_at(50),
        Some(window_at(60)),
        WindowState::Scheduled,
    )
    .await;

    h.registry.commit(&pending, 1);
    sweep(&h, at(13)).await;

    let deltas = deltas(&h).await;
    assert_eq!(deltas.len(), 1, "one subject, one delta");
    let groups = window_groups(&deltas[0]);
    assert_eq!(groups.len(), 1, "one projected key, one group");

    // THE assertion. Day 20 is where the published row's coverage stops; day 60 is
    // where the draft's window ends.
    assert_eq!(
        groups[0].get("coverageEnd"),
        Some(&serde_json::json!({ "kind": "ends", "at": format_rfc3339(window_at(20)) })),
        "the key's coverage end is the published row's own"
    );

    let starts: Vec<serde_json::Value> = groups[0]
        .get("intervals")
        .and_then(|i| i.as_array())
        .expect("intervals")
        .iter()
        .map(|i| i.get("effectiveFrom").expect("a start").clone())
        .collect();
    assert_eq!(
        starts,
        vec![
            // The seed's coverage window, without which the row could not have
            // published at all.
            serde_json::json!(format_rfc3339(coverage_window_from())),
            serde_json::json!(format_rfc3339(window_at(10))),
        ],
        "the draft's interval is absent, not merely uncounted"
    );

    let (price_keys, window_keys) = key_sets(&deltas[0]);
    assert_eq!(
        price_keys.len(),
        1,
        "the draft row is not in `prices` either"
    );
    assert_eq!(
        price_keys, window_keys,
        "`windows` enumerates the keys `prices` does"
    );
}

/// A projected key whose only window was **cancelled** renders
/// `coverageEnd.kind == "uncovered"`.
///
/// This is the token's only writer. `group_by_key` creates a group only for a key
/// that has at least one pair, so before the projected key set seeded the
/// grouping, `CoverageEnd::Uncovered` and its `"uncovered"` token could not be
/// reached from the projector at all: "this key is uncovered" was spelled as the
/// *absence* of a group, and D-167 clause 1 requires one declared spelling per
/// payload fact. A token with no writer is not declared; a writer with no token is
/// a defect.
///
/// It is also the answer that must not round the other way: `Uncovered` fails the
/// D-80 horizon on every predicate there is, and a missing group leaves a consumer
/// to guess.
#[tokio::test]
async fn a_projected_key_whose_only_window_was_cancelled_reads_uncovered() {
    let h = harness().await;
    let (plan_id, pending) = seed_and_publish_at(&h, "gold", at(12)).await;
    let price_id = published_price_id(&h, plan_id).await;

    window(
        &h,
        price_id,
        0x_c3,
        window_at(10),
        Some(window_at(20)),
        WindowState::Cancelled,
    )
    .await;
    // **And the seed's coverage window too**, because the property under test is
    // that a key with *no surviving window* reads `uncovered` — and the row could
    // not have published without one (`inst-wc-required`). Cancelling only the
    // case's own window would leave the key covered by the seed's and this test
    // would be asserting about a different world than its name claims.
    cancel_coverage_window(&h, price_id).await;

    h.registry.commit(&pending, 1);
    sweep(&h, at(13)).await;

    let deltas = deltas(&h).await;
    let groups = window_groups(&deltas[0]);
    assert_eq!(
        groups.len(),
        1,
        "the key `prices` carries has a group even with no surviving window"
    );
    assert_eq!(
        groups[0]
            .get("intervals")
            .and_then(|i| i.as_array())
            .map(Vec::len),
        Some(0),
        "the cancelled window is a schedule that never happened"
    );
    assert_eq!(
        groups[0].get("coverageEnd"),
        Some(&serde_json::json!({ "kind": "uncovered", "at": null })),
        "and the key says so in the one spelling the payload declares"
    );
}

/// A draft row on a key of its **own** produces no group at all — `windows`
/// enumerates exactly the keys `prices` does.
///
/// The question G5 would otherwise have to invent an answer to. Before the fix
/// this delta carried a `windows` group for a key that appears nowhere in
/// `prices`, so a consumer walking the groups would resolve a key no projected row
/// backs.
///
/// The draft here is authored through the **real** door: no published row holds
/// its key, so `PriceRepo::create_draft` admits it, and the world is one an
/// ordinary author can build today.
#[tokio::test]
async fn a_draft_row_on_a_new_key_gets_no_group_at_all() {
    let h = harness().await;
    let (plan_id, pending) = seed_and_publish_at(&h, "gold", at(12)).await;
    let published = published_price(&h, plan_id).await;
    window(
        &h,
        published.price_id,
        0x_c4,
        window_at(10),
        Some(window_at(20)),
        WindowState::Active,
    )
    .await;

    // One axis apart — a different eligibility class, so a different canonical
    // key, and `cohort` stays `none` as the class requires.
    let draft_id = Uuid::from_u128(0x_d2);
    h.prices
        .create_draft(
            &h.scope,
            TENANT,
            NewPriceDraft {
                price_id: draft_id,
                scope_key: ScopeKey::new(
                    plan_id,
                    CurrencyCode::new(&published.currency).expect("three letters"),
                    Region::new(&published.region).expect("a non-blank region"),
                    PhaseId::new(published.phase),
                    PriceEligibility::NewSubscriptionsOnly,
                    ChargeKind::Recurring,
                    Cohort::None,
                )
                .expect("the class pairs with cohort none"),
                content: flat_row(),
                created_by: ACTOR,
                created_at_utc: at(10),
                correlation_id: TEST_CORRELATION,
            },
        )
        .await
        .expect("a draft on an unheld key is ordinary authoring");
    window(
        &h,
        draft_id,
        0x_c5,
        window_at(30),
        Some(window_at(40)),
        WindowState::Scheduled,
    )
    .await;

    h.registry.commit(&pending, 1);
    sweep(&h, at(13)).await;

    let deltas = deltas(&h).await;
    let (price_keys, window_keys) = key_sets(&deltas[0]);
    assert_eq!(price_keys.len(), 1, "one published row, one projected key");
    assert_eq!(
        price_keys, window_keys,
        "`windows` enumerates the keys `prices` does, and no others"
    );
}

// ---------------------------------------------------------------------------
// 9. Activation is NOT a publish unit (`inst-ws-publishunit`'s negative half).
// ---------------------------------------------------------------------------

/// The frozen window interval **starting at `from`**, read back the way a
/// consumer reads it — off the payload, through the domain's own type.
///
/// Through [`WindowInterval`] rather than by comparing JSON numbers because the
/// claim under test is a *consumer's*: the delta carries intervals, and
/// "in force at `t`" is [`WindowInterval::covers`] applied to one. A test that
/// re-implemented the containment here would be asserting its own arithmetic.
///
/// **The start is a parameter and not "the only one".** The key also carries the
/// seed's coverage window, without which its row could not have published, so a
/// helper that took "the one interval" would be asserting about whichever of the
/// two the payload happened to list first. The payload carries no `window_id` — by
/// `WindowInterval`'s own decision, a consumer resolving a price at `t` has no call
/// to make with one — so the start is what names an interval here.
fn frozen_interval(delta: &read_model::Model, from: OffsetDateTime) -> WindowInterval {
    let groups = delta
        .payload
        .get("windows")
        .and_then(|w| w.as_array())
        .expect("the delta carries the window facts");
    assert_eq!(groups.len(), 1, "one canonical scope key, one group");
    let intervals = groups[0]
        .get("intervals")
        .and_then(|i| i.as_array())
        .expect("intervals");
    let matching: Vec<&serde_json::Value> = intervals
        .iter()
        .filter(|i| i.get("effectiveFrom") == Some(&serde_json::json!(format_rfc3339(from))))
        .collect();
    assert_eq!(
        matching.len(),
        1,
        "exactly one interval starts at {from}; got {intervals:?}"
    );
    let interval = matching[0];
    let instant = |key: &str| {
        interval
            .get(key)
            .filter(|v| !v.is_null())
            .map(|v| parse_rfc3339(v.as_str().expect("an RFC3339 instant")).expect("an instant"))
    };
    WindowInterval::new(
        instant("effectiveFrom").expect("a window has a start"),
        instant("effectiveTo"),
        interval
            .get("state")
            .and_then(|s| s.as_str())
            .and_then(WindowState::from_token)
            .expect("a projected state token"),
    )
}

/// A window ACTIVATION requests no `CatalogVersion`, writes no
/// `pricing_read_model` delta and re-projects nothing: the delta carries
/// intervals, so "active at `t`" is derived at read time and a frozen version
/// never needed to change. The same frozen delta reports the key active once
/// `t` passes `effectiveFrom`.
///
/// # The world state is the whole of this test
///
/// The flip has to **genuinely happen** — the job must report one activation and
/// the truth row must read `active` afterwards — or the invariance below is the
/// invariance of a store nothing touched, which is what a job with a typo would
/// also produce.
///
/// And the delta is compared as the **whole row**, not as a payload: a projector
/// that re-projected and happened to produce equal content would pass a payload
/// comparison and fail this one, because `projected_at` and `catalog_version`
/// are what say *which* projection this row is. That is why the assertion is
/// `read_model::Model` equality rather than `payload` equality.
#[tokio::test]
async fn an_activation_re_projects_nothing_and_the_frozen_delta_answers_anyway() {
    let h = harness().await;
    let (plan_id, pending) = seed_and_publish_at(&h, "gold", at(12)).await;
    let price_id = published_price_id(&h, plan_id).await;
    let window_id = Uuid::from_u128(0x_b1);
    window(
        &h,
        price_id,
        0x_b1,
        window_at(10),
        Some(window_at(20)),
        WindowState::Scheduled,
    )
    .await;
    // The seed's coverage window is cancelled, because this test counts
    // activations and its whole claim is that **one** window flipped. The seed's
    // interval ends before the sweep instant below, so a sweep at `window_at(10)`
    // finds it due to activate as well and answers `activated == 2` — a number
    // that says nothing about the flip under test. Cancelled through §4's real
    // edge, which is what removing a window means; the row published with its
    // coverage first, as `inst-wc-required` requires.
    cancel_coverage_window(&h, price_id).await;

    h.registry.commit(&pending, 1);
    sweep(&h, at(13)).await;

    let before = deltas(&h).await;
    assert_eq!(before.len(), 1, "one subject, one delta");
    let refs_before = refs(&h).await;

    // What the frozen delta answers, before anything flips. The two answers are
    // the same row read at two instants, which is the whole of D-99's paired
    // clarification: a point-in-time boolean would have had to be re-projected
    // between them.
    let frozen = frozen_interval(&before[0], window_at(10));
    assert_eq!(
        frozen.state,
        WindowState::Scheduled,
        "the version froze the window as it stood when it was projected"
    );
    assert!(
        !frozen.covers(window_at(9)),
        "before its start the key is not in force at t"
    );
    assert!(
        frozen.covers(window_at(11)),
        "and after it, it is - derived from the interval, not read off a flag"
    );

    // The flip, for real.
    let report = WindowActivationJob::new(h.provider.clone(), JobsConfig::default())
        .run(window_at(10))
        .await
        .expect("the activation sweep runs");
    assert_eq!(report.activated, 1, "the window genuinely activated");

    let truth = {
        let conn = h.provider.conn().expect("conn");
        window_repo::find(&conn, &h.scope, TENANT, window_id)
            .await
            .expect("read the window")
            .expect("it is there")
    };
    assert_eq!(truth.state, WindowState::Active);
    assert_eq!(truth.activated_at, Some(window_at(10)));

    // Nothing on the read side moved: not the delta, not its instant, not its
    // version - and no second `CatalogVersion` was ever asked for.
    assert_eq!(
        deltas(&h).await,
        before,
        "the frozen delta is the same row byte for byte across the flip"
    );
    assert_eq!(
        refs(&h).await,
        refs_before,
        "an activation requests no CatalogVersion: it is not a publish unit"
    );

    // And it still answers, unchanged, including the state token it froze.
    let after = frozen_interval(&deltas(&h).await[0], window_at(10));
    assert_eq!(after, frozen);
    assert_eq!(
        after.state,
        WindowState::Scheduled,
        "the frozen token still says scheduled - a completed version never mutates, and the \
         consumer's answer never depended on it"
    );
    assert!(after.covers(window_at(11)));
}

// ---------------------------------------------------------------------------
// D-234's commit: the surface that writes an overlay's refs.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_approved_overlay_commit_records_both_subjects_and_the_sweep_projects_them() {
    // The whole pipeline end to end, and the first case in which nothing is
    // recorded by hand: the commit requests the handle, flips the revision and
    // writes **both** ref rows, and the sweep then warms both subjects.
    //
    // Until this existed, every projection case in this file staged its own refs,
    // so the projector was proved and the thing that feeds it was not.
    let h = harness().await;
    let overlays = OverlayRepo::new(h.provider.clone());
    let price_overlay_id = seed_draft_overlay(&h, 40).await;
    let record = overlays
        .load(&h.scope, TENANT, price_overlay_id, 0)
        .await
        .expect("load")
        .expect("the draft");
    let pin = bss_pricing::domain::approval::content_pin::overlay_content_hash(&record.content());

    let receipt = overlay_publish(&h)
        .commit(
            &ctx_of(TENANT),
            &h.scope,
            TENANT,
            OverlayPublishUnit::new(price_overlay_id, 0),
            PublishAuthorization::approved(Uuid::new_v4(), ACTOR, Uuid::from_u128(0xbb), pin),
            stamp_of(ACTOR, at_min(12, 0)),
        )
        .await
        .expect("an approved overlay publishes");

    // The revision really is published, and both subjects have a ref on one
    // handle - one act, one registry assignment.
    assert_eq!(
        overlays
            .load(&h.scope, TENANT, price_overlay_id, 0)
            .await
            .expect("load")
            .expect("the revision")
            .lifecycle_state,
        bss_pricing::domain::overlay::OverlayLifecycle::Published
    );
    let refs = refs(&h).await;
    assert_eq!(refs.len(), 2, "{refs:?}");
    assert!(
        refs.iter()
            .all(|row| row.pending_ref == receipt.pending_ref)
    );

    h.registry.commit(&receipt.pending_ref, 4);
    let report = sweep(&h, at(13)).await;

    assert_eq!(report.subjects_failed, 0);
    assert_eq!(report.subjects_projected, 2);
    let rows = deltas(&h).await;
    assert!(rows.iter().any(|row| row.subject_kind == "price_overlay"));
    let shard = rows
        .iter()
        .find(|row| row.subject_kind == "overlay_index")
        .expect("the shard delta");
    assert_eq!(shard.subject_ref, "partner/acme");
    assert_eq!(
        shard
            .payload
            .get("overlays")
            .and_then(serde_json::Value::as_array)
            .map(Vec::len),
        Some(1)
    );
}

#[tokio::test]
async fn a_commit_whose_content_moved_since_the_approval_is_refused() {
    // `inst-ap-pin` scopes the TOCTOU void to a `submitted` record, so an
    // approved unit survives a mutation of its own subject and the
    // approve-to-commit window is real. The pin is therefore re-derived **inside
    // the commit's transaction**, exactly as the plan plane does it: a check made
    // before the transaction opened is a check the world can move behind.
    //
    // Without it, a second person's approval of one overlay would authorize
    // publishing a different one.
    let h = harness().await;
    let price_overlay_id = seed_draft_overlay(&h, 40).await;
    let overlays = OverlayRepo::new(h.provider.clone());

    // **The content has to actually move.** A pin of `[0_u8; 32]` costs the case
    // its subject: nothing hashes to it, so the refusal is for a pin that never
    // matched rather than one that stopped matching, and a `commit` that re-derived
    // nothing at all satisfies it equally.
    let approved = overlays
        .load(&h.scope, TENANT, price_overlay_id, 0)
        .await
        .expect("read the draft")
        .expect("revision 0 exists");
    let pin = bss_pricing::domain::approval::content_pin::overlay_content_hash(&approved.content());

    // ...and now it moves: the one line's discount is re-authored from 15% to 5%
    // after the approval. One line and not two, because
    // `uq_pricing_price_overlay_line_key` admits a key once per revision.
    overlays
        .replace_lines(
            &h.scope,
            TENANT,
            price_overlay_id,
            0,
            approved.row_version,
            vec![OverlayLine {
                line_id: Uuid::new_v4(),
                key: LineKey::list_default(),
                adjustment: Adjustment::Discount(Magnitude::PercentBp(500)),
            }],
            stamp_of(ACTOR, at_min(11, 30)),
        )
        .await
        .expect("the world moves behind the approval");

    let refusal = overlay_publish(&h)
        .commit(
            &ctx_of(TENANT),
            &h.scope,
            TENANT,
            OverlayPublishUnit::new(price_overlay_id, 0),
            PublishAuthorization::approved(Uuid::new_v4(), ACTOR, Uuid::from_u128(0xbb), pin),
            stamp_of(ACTOR, at_min(12, 0)),
        )
        .await
        .expect_err("content that is not what was approved");

    assert!(
        matches!(
            refusal,
            bss_pricing::domain::error::DomainError::ApprovalContentMismatch(_)
        ),
        "{refusal:?}"
    );
    assert!(refs(&h).await.is_empty(), "a refused commit records no ref");
    // **And no registry handle was orphaned** — which the line above cannot say.
    // `refs` reads database rows, and a refused `commit` rolls those back either
    // way; the registry double lives outside the transaction, so a handle requested
    // before the refusal survives it and nothing ever commits it, tripping
    // `pricing.catalogversion.commit_overdue` forever. Moving the
    // `ApprovalContentMismatch` check below step 3's `registry.request_version`
    // would have left this case green without this assertion — the same vacuity
    // `a_scope_value_withdrawn_between_submit_and_commit_refuses_and_writes_nothing`
    // records as measured on 2026-08-08.
    assert_eq!(
        h.registry.issued_count(),
        0,
        "a refused commit must request no CatalogVersion at all: the refusal has to precede the \
         handle, not merely roll back beside it"
    );
}

// ---------------------------------------------------------------------------
// `inst-bt-frozen` (Slice 6) — Billing's sole deferral input survives the
// publish, byte for byte.
// ---------------------------------------------------------------------------

/// The authored `billingTiming` reaches a consumer unchanged.
///
/// `inst-bt-frozen` says Billing derives its deferral policy from the frozen
/// value "never from heuristics", which is a statement about the whole path —
/// draft column, publish, snapshot, projection, delta row — and no unit test
/// over the renderer can make it: the renderer is handed a `PriceRecord` some
/// other layer built. This walks the real one.
///
/// Appended at the end of this file rather than filed beside the other payload
/// cases: a second strand is in this tree, and an append is the one edit that
/// cannot collide with a regrouping.
#[tokio::test]
async fn a_recurring_rows_billing_timing_is_frozen_into_the_delta_as_authored() {
    let h = harness().await;
    let (_, pending) = seed_and_publish_at(&h, "gold", at_min(12, 0)).await;
    h.registry.commit(&pending, 1);

    sweep(&h, at(13)).await;

    let rows = deltas(&h).await;
    assert_eq!(rows.len(), 1);
    let prices = rows[0]
        .payload
        .get("prices")
        .and_then(serde_json::Value::as_array)
        .expect("the delta carries the version's price rows");
    assert_eq!(prices.len(), 1);
    assert_eq!(
        prices[0].get("billingTiming"),
        Some(&serde_json::json!("advance")),
        "the value `flat_row` authored, unnormalised: {:?}",
        prices[0]
    );
}

// ---------------------------------------------------------------------------
// `inst-cr-resolve` / `inst-cr-return` (Slice 6) — resolving the contract set
// through the pin, and getting the same answer twice.
// ---------------------------------------------------------------------------

/// A consumer that pins a committed `CatalogVersion` reads the contract fields
/// exactly as published, and reads the same thing on a second pass.
///
/// This is §2's flow end to end and the half no unit test reaches: `inst-cr-resolve`
/// is about resolving *through* `pricingSnapshotRef` — the frontier, the ref row
/// and the delta together — and `inst-cr-return` is about the answer being stable
/// for that pin. A renderer test can assert determinism of one value; only the
/// store can say that the version a consumer pins is the version they get back.
///
/// The second sweep is the point of the second read: a re-run of the projector
/// over an already-warm version must not move a byte, or "stable for the pinned
/// version" would hold only until the next sweep.
#[tokio::test]
async fn a_pinned_version_resolves_the_same_contract_set_on_every_read() {
    let h = harness().await;
    let (_, pending) = seed_and_publish_at(&h, "gold", at_min(12, 0)).await;
    h.registry.commit(&pending, 1);

    sweep(&h, at(13)).await;
    let first = deltas(&h).await;
    assert_eq!(first.len(), 1);

    // The version a consumer would pin.
    assert_eq!(frontier_version(&h).await, Some(1));

    // A second pass over an already-warm version.
    sweep(&h, at(14)).await;
    let second = deltas(&h).await;

    assert_eq!(
        second.len(),
        1,
        "no second delta was written for one version"
    );
    assert_eq!(
        first[0].payload, second[0].payload,
        "the frozen contract set is stable for the pinned version"
    );

    // And the contract fields are actually in it, rather than the assertion above
    // holding vacuously over a payload that carries none of them.
    let payload = &second[0].payload;
    // **Present is not enough.** `get(key).is_some()` is satisfied by a JSON
    // `null`, which is what a projector that names a field and never resolves it
    // writes - so each key is pinned twice: that the key **exists** (a dropped
    // field reads `null` through indexing, so `get` is what sees the difference)
    // and that it carries the value the fixture authored.
    //
    // Two of the six carry no operand and each for a stated rule rather than by
    // omission: `allowedChangeTargets` because `inst-pc-targets` admits only
    // published `planId`s, and `phaseGrantMap` because `inst-gs-perphase` refuses a
    // per-phase set on a non-phased plan, which these are.
    for (key, expected) in [
        (
            "crossBoundaryChangePolicy",
            serde_json::json!("cancel_plus_new"),
        ),
        ("usageCounterOnPlanChange", serde_json::json!("carry")),
        ("comparabilityRank", serde_json::json!(10)),
        ("allowedChangeTargets", serde_json::Value::Null),
        ("phaseGrantMap", serde_json::json!({})),
        (
            "entitlementGrants",
            serde_json::json!({
                "featureFlags": { "bss.pricing/api": true },
                "perPhase": {},
                "planTierRef": "gold",
                "quotas": { "projects": 5 }
            }),
        ),
    ] {
        assert!(
            payload.get(key).is_some(),
            "the resolved version carries the key {key}: {payload:?}"
        );
        assert_eq!(
            payload[key], expected,
            "and its authored value: {payload:?}"
        );
    }

    let prices = payload["prices"].as_array().expect("the version's rows");
    assert!(!prices.is_empty(), "the version carries rows: {payload:?}");
    for row in prices {
        for key in ["billingTiming", "prorationBasis", "billingAnchorPolicy"] {
            assert!(
                row.get(key).is_some_and(|v| !v.is_null()),
                "a row carries {key} with a value: {row}"
            );
        }
        assert_eq!(
            row["prorationBasis"], "calendar_days_actual",
            "and it is the basis the fixture authored, not a placeholder: {row}"
        );
        assert_eq!(
            row["billingAnchorPolicy"], "calendar_month",
            "and the anchor policy likewise: {row}"
        );
        assert_eq!(row["billingTiming"], "advance", "{row}");
    }
}

/// §4.2's **second validation run**: a world that moved between the submit and
/// the commit is refused, and nothing is written on the way to saying no.
///
/// This is D-234's residue (1), and the case is what the residue was about. The
/// approval content pin proves the commit freezes what a reviewer approved; it
/// says nothing about whether the world still admits it. Here the overlay is
/// approved while `partner/acme` is declared, the value is then **retired** from
/// the taxonomy — D-120's own withdrawal — and the commit must refuse rather than
/// freeze an overlay whose scope selects a partner universe that no longer
/// contains it.
///
/// **The three negative assertions are the point.** A refusal that had already
/// requested a `CatalogVersion` would leave a pending handle nothing ever
/// commits, tripping `commit_overdue` for a publish that never happened; one that
/// had already flipped the revision would be a publish it then denied. So the
/// revision must still be a draft, no ref may exist, and no announcement may have
/// been enqueued.
#[tokio::test]
async fn a_scope_value_withdrawn_between_submit_and_commit_refuses_and_writes_nothing() {
    let h = harness().await;
    let overlays = OverlayRepo::new(h.provider.clone());
    let price_overlay_id = seed_draft_overlay(&h, 40).await;
    let record = overlays
        .load(&h.scope, TENANT, price_overlay_id, 0)
        .await
        .expect("load")
        .expect("the draft");
    let pin = bss_pricing::domain::approval::content_pin::overlay_content_hash(&record.content());

    // The world moves: the partner value this overlay scopes itself to is
    // withdrawn after the approval and before the commit.
    let conn = h.provider.conn().expect("a connection");
    partner_taxonomy::Entity::update_many()
        .secure()
        .scope_with(&h.scope)
        .col_expr(partner_taxonomy::Column::State, Expr::value("retired"))
        .filter(
            Condition::all()
                .add(partner_taxonomy::Column::TenantId.eq(TENANT))
                .add(partner_taxonomy::Column::Value.eq("acme")),
        )
        .exec(&conn)
        .await
        .expect("the partner value is retired");

    let refusal = overlay_publish(&h)
        .commit(
            &ctx_of(TENANT),
            &h.scope,
            TENANT,
            OverlayPublishUnit::new(price_overlay_id, 0),
            // The pin still matches: the *content* is exactly what was approved.
            // Only the world moved, which is precisely what the content pin
            // cannot see and this run can.
            PublishAuthorization::approved(Uuid::new_v4(), ACTOR, Uuid::from_u128(0xbb), pin),
            stamp_of(ACTOR, at_min(12, 0)),
        )
        .await
        .expect_err("a withdrawn scope value must refuse the commit");

    match &refusal {
        DomainError::ValidationFailed(report) => assert!(
            report
                .violations
                .iter()
                .any(|v| v.code == "SCOPE_VALUE_UNKNOWN"),
            "the refusal names the rule that moved: {report:?}"
        ),
        other => panic!("expected the aggregate 422, got {other:?}"),
    }

    assert_eq!(
        overlays
            .load(&h.scope, TENANT, price_overlay_id, 0)
            .await
            .expect("load")
            .expect("the revision")
            .lifecycle_state,
        bss_pricing::domain::overlay::OverlayLifecycle::Draft,
        "a refused commit publishes nothing"
    );
    assert!(refs(&h).await.is_empty(), "a refused commit records no ref");
    // **The assertion that actually pins the ordering.** The two above are true
    // however late the run sits, because a refused commit rolls its database
    // writes back either way — a probe on 2026-08-08 moved the run to *after* the
    // registry request and reddened nothing until this line existed. The registry
    // is outside the transaction, so a handle requested before the refusal
    // survives it, and a pending handle nobody ever commits is what trips
    // `commit_overdue` for a publish that never happened.
    assert_eq!(
        h.registry.issued_count(),
        0,
        "a refused commit must request no CatalogVersion at all: the refusal has to precede the \
         handle, not merely roll back beside it"
    );
    assert!(
        events_named(&h, "PriceOverlayPublished").await.is_empty(),
        "a refused commit announces nothing"
    );
}

// ---------------------------------------------------------------------------
// Z10-5 — the Critical's own frontier read.
// ---------------------------------------------------------------------------

/// Take `pricing_pin_frontier` away, through the one door this crate opens the
/// schema with.
///
/// The finding says "verify with an unmigrated store", and withholding part of
/// the chain cannot deliver one here: the table is `pricing_pin_frontier`, so
/// stopping before it withholds every migration after it, and the pass would have
/// almost no tables to run against at all. (That is also why the harness no longer
/// carries the arm that could withhold them.) A migration of the
/// test's own is the same thing arriving from the other side, and it keeps the
/// DDL where the crate's rule puts DDL.
///
/// The fault is the narrowest one that reaches the read under test: nothing else
/// in a warm pass touches this table except `frontier_is_blocked`, which
/// documents its own `Ok`-only choice and answers `false`.
struct DropTheFrontierTable;

impl sea_orm_migration::MigrationName for DropTheFrontierTable {
    fn name(&self) -> &'static str {
        "ztest_drop_pricing_pin_frontier"
    }
}

#[async_trait::async_trait]
impl sea_orm_migration::MigrationTrait for DropTheFrontierTable {
    async fn up(&self, manager: &sea_orm_migration::SchemaManager) -> Result<(), sea_orm::DbErr> {
        manager
            .drop_table(
                sea_orm_migration::prelude::Table::drop()
                    .table(sea_orm_migration::prelude::Alias::new(
                        "pricing_pin_frontier",
                    ))
                    .to_owned(),
            )
            .await
    }

    async fn down(
        &self,
        _manager: &sea_orm_migration::SchemaManager,
    ) -> Result<(), sea_orm::DbErr> {
        Ok(())
    }
}

async fn drop_the_frontier_table(h: &Harness) {
    run_migrations_for_testing(&h.provider.db(), vec![Box::new(DropTheFrontierTable)])
        .await
        .expect("take the table the pin-eligibility pass reads away");
}

/// **The RED this case is about.** `observe_pin_eligibility` opened with an
/// `if let Ok(frontiers) = pin_frontier_repo::list_all(…)` and no `else` — no
/// log, no counter, nothing. On a storage failure the tenant map degenerated to
/// *only the tenants holding a pending ref this pass*, which is precisely the
/// population the method's own doc says the frontier read exists to go beyond:
/// "the first half is what finds a tenant whose frontier is stale precisely
/// because nothing of it has moved". So the arm that carries this gear's only
/// Critical about a stuck frontier disappeared with no trace, and the pass went
/// on answering `pin_eligibility_overdue: 0`, which reads as healthy.
///
/// **Armed at the observability, not at a proxy for it.** The scenario is one
/// that alarms either way — the tenant is also in the pending set — so the
/// alarm count cannot move, and the case asserts that: every other member of the
/// report is *identical* between the healthy pass and the broken one. That is
/// the claim. If any counter had moved, a test could have watched the counter
/// instead and this field would be redundant; none does, so before the `Err` arm
/// existed there was nothing in this crate a case could have asserted.
///
/// The first assertion is the positive control: a pass over a store that answers
/// must not report a failure, or a field wired to `true` unconditionally would
/// satisfy the second.
#[tokio::test]
async fn a_frontier_scan_that_cannot_read_reports_it_rather_than_disappearing() {
    let h = harness().await;
    let (_, pending_a) = seed_and_publish_at(&h, "gold", at_min(12, 0)).await;
    h.registry.commit(&pending_a, 5);
    sweep(&h, at_min(12, 1)).await;
    assert_eq!(frontier_version(&h).await, Some(5));

    // A later version whose subject cannot project: V6 stands committed and short
    // of pin-eligibility while the frontier ages at V5 — the sibling scenario of
    // `a_stale_frontier_with_a_version_waiting_behind_it_alarms_on_its_own_age`.
    let conn = h.provider.conn().expect("conn");
    catalog_version_ref_repo::record_pending(
        &conn,
        &h.scope,
        PendingVersionRow::for_subject(
            TENANT,
            "pend-stuck".to_owned(),
            &SubjectRef::Plan(Uuid::new_v4()),
            Some(0),
            Some(LifecycleState::Published),
            at_min(12, 2),
        ),
    )
    .await
    .expect("record the stuck subject");
    h.registry.commit("pend-stuck", 6);

    // Two passes, so the baseline is a steady state rather than the first pass's
    // one-off emissions, and the broken pass below is compared against a pass
    // that would otherwise repeat it exactly.
    sweep(&h, at(14)).await;
    let healthy = sweep(&h, at(14)).await;
    assert!(
        !healthy.frontier_scan_failed,
        "a store that answers is not a failed read: {healthy:?}"
    );
    assert_eq!(
        healthy.pin_eligibility_overdue, 1,
        "and the Critical this arm carries does fire on this fixture: {healthy:?}"
    );

    drop_the_frontier_table(&h).await;
    let broken = sweep(&h, at(14)).await;

    let mut only_the_read_failed = healthy.clone();
    only_the_read_failed.frontier_scan_failed = true;
    assert_eq!(
        broken, only_the_read_failed,
        "the frontier read failed and the report says so, and nothing else in the report moved \
         — so no other member of it was ever a proxy an operator or a case could have watched \
         instead: healthy={healthy:?}"
    );
}
