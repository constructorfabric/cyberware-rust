//! The publish commit as a whole, against a real database.
//!
//! `design/01-foundation.md` §4.2 step 4 is five moves in one transaction, and
//! what this suite proves is the "one transaction" half: after a success the
//! database holds exactly five artifacts, and after **any** failure it holds
//! none of them. A unit test over the service could assert the calls happen; it
//! could not assert that a failure three statements in undoes the two before it,
//! which is the only property that makes a publish atomic.
//!
//! # The G5/G6 seam is asserted here, deliberately
//!
//! Every success case asserts `pricing_read_model` is **empty** and
//! `pricing_pin_frontier` is **untouched**. Those two assertions are the seam:
//! the commit holds a *pending* handle and no version number, and
//! `pricing_read_model` is keyed `catalog_version NOT NULL`, so a delta row is
//! not even expressible here. They are the tests that fail if G6's work ever
//! leaks backwards into this one.
//!
//! # The registry double, and why it may never leave this file
//!
//! The only registry this crate can have in production is
//! `UnconfiguredCatalogVersionRegistryV1`, because the registry gear has no code
//! in this repository — so the **fail-closed arm is the only arm the real wiring
//! can reach**, and the success path needs a double. It lives in the test crate
//! and must never move into `src/`, not even as a "development" default: the
//! single failure this port exists to prevent is a second incrementer shipping,
//! and a default that invents versions locally is exactly that. The double also
//! asserts the idempotency the commit depends on — two calls carrying one
//! `request_id` return one handle — because that is what makes a rolled-back and
//! retried commit re-request rather than orphan.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use toolkit_canonical_errors::CanonicalError;

use async_trait::async_trait;
use bss_pricing::config::LimitsConfig;
use bss_pricing::domain::approval::{ApprovalState, DecisionBy, WithdrawAuthority};
use bss_pricing::domain::audit::{
    AuditAction, AuditRecord, AuditSubjectKind, audit_row_hash, genesis_prev_hash,
};
use bss_pricing::domain::bulk::BulkKind;
use bss_pricing::domain::bundle::{InvoiceItemization, PriceBasis};
use bss_pricing::domain::concurrency::RowVersion;
use bss_pricing::domain::contracts::{
    BillingAnchorPolicy, EntitlementGrants, GrantSet, PlanChangeContract, ProrationBasis,
    ProrationContract, UsageCounterOnPlanChange,
};
use bss_pricing::domain::error::DomainError;
use bss_pricing::domain::evaluation_policy::EVALUATION_POLICY_GENERATION;
use bss_pricing::domain::instant::utc_ymd_hms;
use bss_pricing::domain::lifecycle::LifecycleState;
use bss_pricing::domain::money::{CurrencyCode, MinorAmount};
use bss_pricing::domain::plan::PlanShapePatch;
use bss_pricing::domain::plan_shape::{PhaseKind, PlanPhase};
use bss_pricing::domain::price_record::PriceContent;
use bss_pricing::domain::price_row::{ModelKind, PriceRow};
use bss_pricing::domain::publish::{PlanPublishUnit, PublishAuthorization};
use bss_pricing::domain::read_model::SubjectKind;
use bss_pricing::domain::scope_key::{
    ChargeKind, Cohort, PhaseId, PlanId, PriceEligibility, Region, ScopeKey,
};
use bss_pricing::domain::snapshot::VersionRef;
use bss_pricing::infra::approval::{ApprovalService, DecideRequest, RegionGrant};
use bss_pricing::infra::fixture_gate::FixtureGate;
use bss_pricing::infra::metrics::test_harness::MetricsHarness;
use bss_pricing::infra::publish::PublishService;
use bss_pricing::infra::storage::entity::{
    audit_log, catalog_version_ref, outbox, pin_frontier, read_model,
};
use bss_pricing::infra::storage::migrations::Migrator;
use bss_pricing::infra::storage::repo::{
    BundleComponentDraft, BundleRepo, CompositionDraft, NewBundle,
};
use bss_pricing::infra::storage::repo::{
    IdempotencyGate, NewAuditEntry, NewBulkOperation, NewMembership, NewOutboxEvent, NewPlanDraft,
    NewPriceDraft, PlanPublishedPayload, PlanRepo, PlanShapeRepo, PriceRepo, audit_repo, bulk_repo,
    group_membership_repo, outbox_repo,
};
use bss_pricing_sdk::catalog_version::CatalogVersion;
use bss_pricing_sdk::catalog_version_registry::{
    CatalogVersionRegistryV1, PendingVersionRef, UnconfiguredCatalogVersionRegistryV1,
};
use time::OffsetDateTime;

use sea_orm::{ColumnTrait, Condition, EntityTrait};
use sea_orm_migration::MigratorTrait;
use std::path::{Path, PathBuf};
use toolkit_db::migration_runner::run_migrations_for_testing;
use toolkit_db::secure::{AccessScope, SecureEntityExt, SecureInsertExt};
use toolkit_db::{ConnectOpts, DBProvider, DbError, connect_db};
use toolkit_security::SecurityContext;
use uuid::Uuid;

/// One value for a whole test binary: these suites drive a repository or a
/// service directly, where the value the HTTP edge would have established has
/// no producer. What each suite asserts *about* it is stated where it asserts
/// it.
const TEST_CORRELATION: uuid::Uuid = uuid::Uuid::from_u128(0x_c0_11_a7_10);

// ---------------------------------------------------------------------------
// The registry double.
// ---------------------------------------------------------------------------

/// A registry that hands out one pending handle per `request_id`.
///
/// Idempotent on purpose: the commit derives its `request_id` deterministically
/// from `(tenant, plan, revision)` so that a rolled-back-and-retried publish
/// re-requests the same assignment, and a double that minted a fresh handle per
/// call would let that property pass untested.
#[derive(Default)]
struct RegistryDouble {
    issued: Mutex<HashMap<String, String>>,
    calls: std::sync::atomic::AtomicUsize,
}

impl RegistryDouble {
    /// How many **distinct** handles the registry has minted.
    fn handles(&self) -> usize {
        self.issued.lock().expect("no panics in the double").len()
    }

    /// How many times the registry was asked.
    ///
    /// Kept apart from [`handles`](Self::handles) because the two answer
    /// different questions and the idempotence claim needs both: one handle
    /// across two asks is the property, and one handle across *one* ask is a
    /// retry that never reached the registry at all.
    fn calls(&self) -> usize {
        self.calls.load(std::sync::atomic::Ordering::Relaxed)
    }
}

#[async_trait]
impl CatalogVersionRegistryV1 for RegistryDouble {
    async fn request_version(
        &self,
        _ctx: &SecurityContext,
        request_id: &str,
    ) -> Result<PendingVersionRef, CanonicalError> {
        self.calls
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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
    ) -> Result<Option<CatalogVersion>, CanonicalError> {
        // G6's call, and nothing in this suite makes it.
        Ok(None)
    }
}

// ---------------------------------------------------------------------------
// The harness.
// ---------------------------------------------------------------------------

const TENANT: Uuid = Uuid::from_u128(0x7e_11);
const ACTOR: Uuid = Uuid::from_u128(0xac_10);
const CORRELATION: Uuid = Uuid::from_u128(0xc0_11);
/// The independent second principal. Distinct from [`ACTOR`], which is what
/// `chk_pricing_approval_distinct_principals` and `inst-tp-distinct` both
/// require of an approve.
const APPROVER: Uuid = Uuid::from_u128(0xac_11);

fn plan_id() -> PlanId {
    PlanId::new(Uuid::from_u128(0x9_1a4))
}

fn terminal_phase() -> PhaseId {
    PhaseId::new(Uuid::from_u128(0xfa_5e))
}

fn at(hour: u32) -> OffsetDateTime {
    utc_ymd_hms(2026, 8, 3, hour, 0, 0)
}

fn ctx() -> SecurityContext {
    SecurityContext::builder()
        .subject_id(ACTOR)
        .subject_tenant_id(TENANT)
        .build()
        .expect("a subject and a tenant are all a context needs")
}

/// The committed corpus registry, resolved from this crate's manifest so the
/// suite does not depend on the directory `cargo test` was invoked from.
fn committed_registry_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/corpus/registry.toml")
}

struct Harness {
    /// What the publish path reported about itself, over a private exporter.
    metrics: MetricsHarness,
    plans: PlanRepo,
    shapes: PlanShapeRepo,
    prices: PriceRepo,
    publish: PublishService,
    registry: Arc<RegistryDouble>,
    scope: AccessScope,
    provider: DBProvider<DbError>,
}

async fn harness() -> Harness {
    harness_with(Arc::new(RegistryDouble::default())).await
}

async fn harness_with(registry: Arc<RegistryDouble>) -> Harness {
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
    // The real adapter over a private exporter, not the no-op: this suite is
    // where the **commit's** rule run is staged, and that is the run whose
    // reporting nothing else can see.
    let metrics_harness = MetricsHarness::new();
    let publish = PublishService::new(
        provider.clone(),
        &LimitsConfig::default(),
        FixtureGate::load(&committed_registry_path()),
        Arc::clone(&registry) as Arc<dyn CatalogVersionRegistryV1>,
    )
    .with_metrics(Arc::new(metrics_harness.metrics()));
    Harness {
        metrics: metrics_harness,
        plans: PlanRepo::new(provider.clone()),
        shapes: PlanShapeRepo::new(provider.clone()),
        prices: PriceRepo::new(provider.clone()),
        publish,
        registry,
        scope: AccessScope::for_tenant(TENANT),
        provider,
    }
}

fn new_plan_draft() -> NewPlanDraft {
    NewPlanDraft {
        plan_name: None,
        plan_id: plan_id(),
        tenant_id: TENANT,
        created_by: ACTOR,
        created_at_utc: at(10),
        sku_id: Some(Uuid::from_u128(0x5_c1)),
        plan_tier: Some("gold".to_owned()),
        billing_cycle: Some(bss_pricing::domain::plan_shape::BillingCycle::Recurring),
        frequency: Some(bss_pricing::domain::plan_shape::Frequency::Monthly),
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
        // Its own policy, so the Foundation's rounding rule resolves without a
        // tenant policy row.
        rounding_policy_ref: Some("half_up".to_owned()),
        grandfather_until: None,
        supersedes_price_id: None,
    }
}

fn scope_key(eligibility: PriceEligibility) -> ScopeKey {
    ScopeKey::new(
        plan_id(),
        CurrencyCode::new("EUR").expect("three letters"),
        Region::new("eu").expect("a non-blank region"),
        terminal_phase(),
        eligibility,
        ChargeKind::Recurring,
        Cohort::None,
    )
    .expect("the class pairs with cohort none")
}

/// A plan the whole rule set passes: one evergreen terminal phase, a descriptor
/// set, a tier, and one flat recurring row.
async fn seed_publishable(h: &Harness) -> (u64, RowVersion, Uuid) {
    let created = h
        .plans
        .create_draft(&h.scope, new_plan_draft())
        .await
        .expect("create the draft");
    let after_phases = h
        .shapes
        .replace_phases(
            &h.scope,
            TENANT,
            plan_id(),
            created.revision,
            created.row_version,
            vec![PlanPhase {
                phase_id: terminal_phase(),
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
            &h.scope,
            TENANT,
            plan_id(),
            created.revision,
            after_phases.row_version,
            bss_pricing::domain::plan_shape::DescriptorSet {
                invoice_line_template: Some("{plan}".to_owned()),
                gl_code: Some("4000".to_owned()),
                itemization_rule: Some("per_charge".to_owned()),
                additional: std::collections::BTreeMap::new(),
            },
            stamp(),
        )
        .await
        .expect("attach the descriptor set");

    let price_id = Uuid::from_u128(0xb_0001);
    h.prices
        .create_draft(
            &h.scope,
            TENANT,
            NewPriceDraft {
                price_id,
                scope_key: scope_key(PriceEligibility::AllSubscriptions),
                content: flat_row(),
                created_by: ACTOR,
                created_at_utc: at(10),
                correlation_id: TEST_CORRELATION,
            },
        )
        .await
        .expect("author the price row");

    // `inst-wc-required`: the row does not publish until its canonical scope key
    // holds an active or scheduled window. Thirteen tests in this file reddened
    // when the rule registered, every one of them because the plan they publish
    // had no window at all.
    //
    // The interval is `common::schedule_coverage_window`'s, not this file's, so
    // the seed cannot drift from the three other suites that owe the same thing.
    let conn = h.provider.conn().expect("conn");
    common::schedule_coverage_window(&conn, &h.scope, TENANT, price_id, stamp()).await;

    (created.revision, after_descriptors.row_version, price_id)
}

// ---------------------------------------------------------------------------
// Reading the five artifacts back.
// ---------------------------------------------------------------------------

async fn version_refs(h: &Harness) -> Vec<catalog_version_ref::Model> {
    let conn = h.provider.conn().expect("conn");
    catalog_version_ref::Entity::find()
        .secure()
        .scope_with(&h.scope)
        .all(&conn)
        .await
        .expect("read the version refs")
}

/// The three names a **seed** in this file emits, which is everything the
/// authoring door announces: the plan coming into existence, each `PATCH` that
/// shaped it, and the price row authored on it.
///
/// `PlanCreated` / `PlanUpdated` joined `PriceCreated` here on 2026-08-13, when
/// the plan plane got the producers `inst-pa-return` and S2 §7 had always asked
/// for. Adding them was the same judgement the original exclusion records below,
/// taken a second time and for the same act.
const SEEDED_EVENT_NAMES: [&str; 3] = ["PlanCreated", "PlanUpdated", "PriceCreated"];

/// The events **this commit** wrote.
///
/// **The authoring door's events are excluded, and the exclusion is what keeps
/// the assertions honest rather than what weakens them.** Every seed in this file
/// creates a plan, patches it and authors a price row, and each of those three
/// acts announces itself (`inst-pa-return`; S2 §7; S3 §17.5) — so an unfiltered
/// read makes `is_empty()` false on paths that write nothing, and the tempting
/// repair is to bump each expectation from 0 to 4. That would leave every
/// `..._writes_nothing` case named for a guarantee it no longer checked.
/// Filtering the seed's own events out instead keeps "the commit wrote nothing"
/// meaning exactly that.
///
/// **It is correct here for a reason that does not travel**, and it did not:
/// `write_prepared` — the only path to `record_price_mutation(Create)` — has two
/// callers, the authoring door and `insert_successor_draft_on`, and neither is
/// reachable from a publish or a window op; `plan_repo::create_draft_on` and
/// `plan_repo::record_revision_mutation` — the two the plan pair rides — are
/// likewise unreachable from a commit, which freezes a revision rather than
/// authoring one, and `open_revision` deliberately passes through neither. So in
/// this file every excluded row really is a seed's. `sqlite_supersession_unit`
/// copied the filter without re-reading its own act, which *does* stage a draft,
/// and spent a stretch asserting "nothing was announced" over an announcement; it
/// excludes the fixture by sequence now. **Before reusing this helper, check
/// whether the act under test can author a plan or a row.**
async fn outbox_rows(h: &Harness) -> Vec<outbox::Model> {
    let conn = h.provider.conn().expect("conn");
    outbox::Entity::find()
        .secure()
        .scope_with(&h.scope)
        .filter(Condition::all().add(outbox::Column::TenantId.eq(TENANT)))
        .all(&conn)
        .await
        .expect("read the outbox")
        .into_iter()
        .filter(|row| !SEEDED_EVENT_NAMES.contains(&row.event_name.as_str()))
        .collect()
}

/// Every `seq` the plan's stream holds, seed events included, ascending.
///
/// The unfiltered read, because the counter is the aggregate's and a drain reads
/// it whole: a caller asking "is the publish the newest event" is asking about
/// the numbers the filter above hides.
async fn plan_stream_seqs(h: &Harness) -> Vec<i64> {
    let conn = h.provider.conn().expect("conn");
    let mut seqs: Vec<i64> = outbox::Entity::find()
        .secure()
        .scope_with(&h.scope)
        .filter(
            Condition::all()
                .add(outbox::Column::TenantId.eq(TENANT))
                .add(outbox::Column::AggregateId.eq(plan_id().get())),
        )
        .all(&conn)
        .await
        .expect("read the outbox")
        .into_iter()
        .map(|row| row.seq)
        .collect();
    seqs.sort_unstable();
    seqs
}

async fn audit_rows(h: &Harness) -> Vec<audit_log::Model> {
    let conn = h.provider.conn().expect("conn");
    audit_log::Entity::find()
        .secure()
        .scope_with(&h.scope)
        .filter(Condition::all().add(audit_log::Column::TenantId.eq(TENANT)))
        .all(&conn)
        .await
        .expect("read the audit chain")
}

/// The chain's **publish** records, in seq order.
///
/// The authoring seeds this suite builds a publishable plan with are themselves
/// audited now (D-135, G8) - a create and three facet edits - so a count over the
/// whole segment stopped being a count of what a publish wrote. Filtering by
/// action is the honest narrowing: this suite's subject is the publish commit,
/// and the seeds' records are `sqlite_audit_chain.rs`'s.
async fn publish_records(h: &Harness) -> Vec<audit_log::Model> {
    let mut rows: Vec<audit_log::Model> = audit_rows(h)
        .await
        .into_iter()
        .filter(|row| row.action == "publish")
        .collect();
    rows.sort_by_key(|row| row.seq);
    rows
}

/// How many records the authoring seeds leave on the plan's segment: the plan
/// create, the two facet edits [`seed_publishable`] makes (`replace_phases` and
/// `set_descriptor_set`), and the price row's own `create` — which
/// `record_price_mutation` files on the **plan's** chain, not a chain of its own.
///
/// The old decomposition said "one per facet the seed sets (phases, add-on rules,
/// descriptors)", and the seed sets no add-on rules; the fourth record is the
/// price row's. Three cases derive an expected `seq` from this constant, so a
/// maintainer re-deriving it from a wrong decomposition after a seed change gets
/// the offset wrong in a way that still compiles.
const SEEDED_AUTHORING_RECORDS: i64 = 4;

async fn read_model_rows(h: &Harness) -> Vec<read_model::Model> {
    let conn = h.provider.conn().expect("conn");
    read_model::Entity::find()
        .secure()
        .scope_with(&h.scope)
        .all(&conn)
        .await
        .expect("read the read model")
}

async fn frontier_rows(h: &Harness) -> Vec<pin_frontier::Model> {
    let conn = h.provider.conn().expect("conn");
    pin_frontier::Entity::find()
        .secure()
        .scope_with(&h.scope)
        .all(&conn)
        .await
        .expect("read the pin frontier")
}

/// The G5/G6 seam, asserted rather than assumed.
async fn assert_seam_holds(h: &Harness) {
    assert!(
        read_model_rows(h).await.is_empty(),
        "a commit holds a pending handle and no version, so no delta row is even \
         expressible: pricing_read_model is G6's"
    );
    assert!(
        frontier_rows(h).await.is_empty(),
        "the pin frontier advances only inside the transaction that completes the \
         frontier's next version in order, which is G6's"
    );
}

// ---------------------------------------------------------------------------
// The success path.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_first_publish_leaves_exactly_the_five_artifacts_and_nothing_else() {
    let h = harness().await;
    let (revision, version, price_id) = seed_publishable(&h).await;

    let receipt = h
        .publish
        .commit(
            &ctx(),
            &h.scope,
            TENANT,
            PlanPublishUnit::plan_content(plan_id(), revision),
            version,
            PublishAuthorization::auto_publishable(),
            ACTOR,
            CORRELATION,
            at(12),
        )
        .await
        .expect("the publish commits");

    // The receipt holds a handle, never a version.
    assert_eq!(receipt.version_ref(), &VersionRef::Pending("pend-0".into()));
    assert_eq!(receipt.published_price_ids(), [price_id]);
    // The publish record extends the segment the authoring seeds already built,
    // so its position is after theirs rather than at genesis.
    assert_eq!(
        i64::try_from(receipt.audit_seq()).expect("a small seq"),
        SEEDED_AUTHORING_RECORDS
    );

    // 1. The revision is published and is the plan's current one.
    let current = h
        .plans
        .find_current(&h.scope, TENANT, plan_id())
        .await
        .expect("read the current revision")
        .expect("the plan has one");
    assert_eq!(current.revision, revision);
    assert_eq!(current.lifecycle_state, LifecycleState::Published);

    // 2. The price row is published.
    let row = h
        .prices
        .find(&h.scope, TENANT, price_id)
        .await
        .expect("read the row")
        .expect("it is there");
    assert_eq!(row.lifecycle_state, LifecycleState::Published);

    // 3. One pending ref, uncommitted, naming the subject the projector needs.
    let refs = version_refs(&h).await;
    assert_eq!(refs.len(), 1);
    assert_eq!(refs[0].pending_ref, "pend-0");
    assert_eq!(refs[0].catalog_version, None);
    assert_eq!(refs[0].committed_at, None);
    assert_eq!(refs[0].subject_kind, SubjectKind::Plan.as_str());
    assert_eq!(refs[0].subject_ref, plan_id().to_string());

    // 4. One undrained `PlanPublished`, carrying the pending handle.
    let events = outbox_rows(&h).await;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event_name, "PlanPublished");
    // The counter is asserted rather than dropped, because it is what orders a
    // drain — but as the **property** and not as a literal. This read `seq == 1`
    // while the seed emitted exactly one event; the seed emits four now
    // (`PlanCreated`, two `PlanUpdated`, `PriceCreated`), and a literal that has
    // to be re-derived every time a producer lands was never asserting the thing
    // it named. What a drain needs is that the plan's stream is dense from zero
    // and that the publish sits at its head.
    let stream = plan_stream_seqs(&h).await;
    assert_eq!(
        stream,
        (0..i64::try_from(stream.len()).expect("a small count")).collect::<Vec<_>>(),
        "the plan's stream is dense from zero, so a drain misses nothing"
    );
    assert_eq!(
        Some(events[0].seq),
        stream.last().copied(),
        "and the publish is its newest event"
    );
    assert_eq!(events[0].published_at, None);
    assert_eq!(events[0].aggregate_id, plan_id().get());
    assert_eq!(
        events[0].payload.get("pendingVersionRef"),
        Some(&serde_json::json!("pend-0"))
    );
    // All three snapshot segments reach the stored row, not just the two the
    // commit could produce before D-162 gave the third a producer.
    assert_eq!(
        events[0].payload.get("evaluationPolicyVersion"),
        Some(&serde_json::json!(EVALUATION_POLICY_GENERATION))
    );

    // 5. One audit row, on the plan's own segment, after the seeds'.
    let records = publish_records(&h).await;
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].seq, SEEDED_AUTHORING_RECORDS);
    assert_eq!(records[0].entry_kind, "mutation");
    assert_eq!(records[0].segment_heads, None);
    assert_eq!(records[0].chain_id, plan_id().get());
    assert_eq!(records[0].actor_principal_id, ACTOR);
    assert_eq!(records[0].action, "publish");
    assert_eq!(records[0].subject_kind, "plan_revision");
    assert_eq!(records[0].approval_ref, None);
    assert_eq!(records[0].correlation_id, Some(CORRELATION));

    assert_seam_holds(&h).await;
}

#[tokio::test]
async fn an_approved_publish_puts_its_record_on_the_audit_trail() {
    let h = harness().await;
    let (revision, version, _) = seed_publishable(&h).await;
    let approval = Uuid::from_u128(0xa9_01);

    // The authorization is minted from a **real** decided record rather than
    // assembled by hand, and since 2026-08-04 it has to be: `Approved` carries
    // the digest the decision was over, and the commit re-derives it inside its
    // own transaction. A hand-built pin would be a pin over nothing.
    let record = approve_unit(&h, approval).await;

    h.publish
        .commit(
            &ctx(),
            &h.scope,
            TENANT,
            PlanPublishUnit::plan_content(plan_id(), revision),
            version,
            authorization_of(&record),
            ACTOR,
            CORRELATION,
            at(12),
        )
        .await
        .expect("the publish commits");

    let records = publish_records(&h).await;
    assert_eq!(records[0].approval_ref, Some(approval));
}

/// The publish commit refuses content the decision was not over
/// (`inst-ap-pin`'s approve→commit window).
///
/// **Staged with the world held still and one byte of the pin moved**, which is
/// what makes the digest the only variable: same plan, same revision, same rows,
/// same `row_version` everywhere, and an authorization carrying a pin that no
/// longer matches what the commit re-derives. Staging a *mutation* instead would
/// move a `row_version`, and the compare-and-swap would answer `STALE_VERSION`
/// whether or not the pin is bound to anything — which is the trap this staging
/// exists to avoid.
///
/// **This doc used to describe a different staging** — a unit opened over one
/// draft and a commit aimed at a second plan's identically-shaped draft — which
/// the body never performed. The prose was left over from an abandoned attempt;
/// the behaviour proved is the stronger of the two, because a corrupted pin
/// isolates the check from every other guard in the commit.
///
/// # What this does **not** exercise, said plainly
///
/// The in-transaction re-derivation is driven here only against a **corrupted
/// authorization**, never against a world that actually moved between the
/// decision and the commit. There is no test anywhere that stages the latter,
/// and the reason is structural rather than an omission of effort: the publish
/// route looks an approval up by subject **and content**
/// (`approval_repo::find_approved_for_content`), so a subject that moved after
/// the approve has no approved unit and never reaches `commit` at all — that
/// path is `rest_publish::a_row_edited_after_the_approve_does_not_publish_under_
/// the_stale_decision`, and it ends in a fresh 202. The only way to reach the
/// re-derivation with a genuinely moved world is the approve→commit race, where
/// the mutation lands after the route's lookup and before the commit's
/// transaction, and nothing in this crate stages it. What is proved here is that
/// the check is bound to the digest; that it is bound to the *current* world is
/// carried by `infra::publish`'s placement of the re-derivation inside the
/// commit's own transaction, and by nothing executable.
#[tokio::test]
async fn a_commit_whose_content_is_not_what_was_approved_is_refused() {
    let h = harness().await;
    let (revision, version, _) = seed_publishable(&h).await;
    let record = approve_unit(&h, Uuid::from_u128(0xa9_02)).await;

    // The same authorization, with one byte of the pin moved. Nothing else in
    // the world changes — same revision, same version, same rows.
    let mut moved = record.content_hash.clone();
    moved[0] ^= 0xff;
    let stale = PublishAuthorization::approved(
        record.approval_id,
        record.submitter_principal,
        record.approver_principal.expect("an approved record"),
        moved.as_slice().try_into().expect("a 32-byte digest"),
    );

    let refused = h
        .publish
        .commit(
            &ctx(),
            &h.scope,
            TENANT,
            PlanPublishUnit::plan_content(plan_id(), revision),
            version,
            stale,
            ACTOR,
            CORRELATION,
            at(12),
        )
        .await
        .expect_err("the content approved is not the content this would freeze");
    assert!(
        matches!(refused, DomainError::ApprovalContentMismatch(_)),
        "got {refused:?}"
    );

    // And nothing was frozen: the refusal is ahead of every write.
    assert_eq!(
        h.plans
            .find_revision(&h.scope, TENANT, plan_id(), revision)
            .await
            .expect("read the revision")
            .expect("it is there")
            .lifecycle_state,
        LifecycleState::Draft
    );
}

/// Open a unit over the seeded plan and approve it as an independent principal.
async fn approve_unit(
    h: &Harness,
    approval_id: Uuid,
) -> bss_pricing::infra::storage::repo::approval_repo::ApprovalRecord {
    let approvals = ApprovalService::new(h.provider.clone());
    approvals
        .submit(
            &h.scope,
            TENANT,
            plan_id(),
            approval_id,
            serde_json::json!({ "material": true, "reason": "noConfiguredThreshold" }),
            stamp_of(ACTOR, at(11)),
        )
        .await
        .expect("open the pending unit");
    approvals
        .decide(
            &h.scope,
            TENANT,
            DecideRequest {
                approval_id,
                decision: DecisionBy::Approve(APPROVER),
                reason: None,
                approver_regions: RegionGrant::Explicit(std::collections::BTreeSet::from([
                    Region::new("eu").expect("a non-blank region"),
                ])),
                stamp: stamp_of(APPROVER, at(11)),
                withdraw_authority: WithdrawAuthority::OwnUnitsOnly,
            },
        )
        .await
        .expect("an independent principal approves it")
}

/// The authorization a decided record yields, spelled once for this suite.
fn authorization_of(
    record: &bss_pricing::infra::storage::repo::approval_repo::ApprovalRecord,
) -> PublishAuthorization {
    PublishAuthorization::approved(
        record.approval_id,
        record.submitter_principal,
        record.approver_principal.expect("an approved record"),
        record
            .content_hash
            .as_slice()
            .try_into()
            .expect("a 32-byte digest"),
    )
}

#[tokio::test]
async fn a_second_publish_extends_every_counter_by_one() {
    let h = harness().await;
    let (revision, version, _) = seed_publishable(&h).await;
    h.publish
        .commit(
            &ctx(),
            &h.scope,
            TENANT,
            PlanPublishUnit::plan_content(plan_id(), revision),
            version,
            PublishAuthorization::auto_publishable(),
            ACTOR,
            CORRELATION,
            at(12),
        )
        .await
        .expect("the first publish commits");

    let opened = h
        .plans
        .open_revision(&h.scope, TENANT, plan_id(), stamp_of(ACTOR, at(13)))
        .await
        .expect("open the successor");
    h.publish
        .commit(
            &ctx(),
            &h.scope,
            TENANT,
            PlanPublishUnit::plan_content(plan_id(), opened.revision),
            opened.row_version,
            PublishAuthorization::auto_publishable(),
            ACTOR,
            CORRELATION,
            at(14),
        )
        .await
        .expect("the second publish commits");

    // The predecessor is superseded and exactly one revision is current.
    let first = h
        .plans
        .find_revision(&h.scope, TENANT, plan_id(), revision)
        .await
        .expect("read the predecessor")
        .expect("it is there");
    assert_eq!(first.lifecycle_state, LifecycleState::Superseded);
    assert_eq!(
        h.plans
            .find_current(&h.scope, TENANT, plan_id())
            .await
            .expect("read the current revision")
            .expect("the plan has one")
            .revision,
        opened.revision
    );

    let mut events = outbox_rows(&h).await;
    events.sort_by_key(|row| row.seq);
    assert_eq!(events.len(), 2);
    assert_eq!(
        events[1].seq,
        events[0].seq + 1,
        "the aggregate's counter advanced by one"
    );

    let records = publish_records(&h).await;
    assert_eq!(records.len(), 2);
    // The two publish records are **not** adjacent, and that is right: opening
    // the successor is itself an audited mutation, so its `create` record sits
    // between them on the plan's segment. What the chain guarantees is that every
    // row links to whatever preceded IT, which
    // `the_chain_verifies_across_a_mixed_sequence_of_authoring_and_publish_records`
    // walks in full. Here the claim is only that the second publish extended the
    // segment rather than restarting it.
    assert!(
        records[1].seq > records[0].seq,
        "the second publish extended the segment: {} then {}",
        records[0].seq,
        records[1].seq
    );
    assert!(
        records[1].prev_hash.is_some(),
        "and linked to whatever preceded it"
    );

    assert_eq!(version_refs(&h).await.len(), 2);
    assert_seam_holds(&h).await;
}

// ---------------------------------------------------------------------------
// The failure paths, and the claim this group exists to make.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_commit_time_validation_failure_writes_nothing_at_all() {
    // The world moved between approval and commit: the subject passed a
    // pre-check, and by the time the commit ran a second draft row had been
    // authored on a market with no covering phase row... in this construction,
    // a second row of the same plan on a class with no phase coverage. What
    // matters is that the second run finds it and that the transaction takes
    // everything back with it.
    let h = harness().await;
    let (revision, version, _) = seed_publishable(&h).await;

    // Passing before the world moves.
    let clean = h
        .publish
        .precheck(&h.scope, TENANT, plan_id(), at(11))
        .await
        .expect("the pre-check runs");
    assert!(
        clean.is_publishable(),
        "the subject was publishable: {clean:?}"
    );

    // The world moves: a row authored with no `roundingPolicyRef`, on a tenant
    // that has no default. Nothing about the approved content changed.
    h.prices
        .create_draft(
            &h.scope,
            TENANT,
            NewPriceDraft {
                price_id: Uuid::from_u128(0xb_0002),
                scope_key: scope_key(PriceEligibility::NewSubscriptionsOnly),
                content: PriceContent {
                    rounding_policy_ref: None,
                    ..flat_row()
                },
                created_by: ACTOR,
                created_at_utc: at(11),
                correlation_id: TEST_CORRELATION,
            },
        )
        .await
        .expect("author the second row");

    // The late row gets its coverage window too, so the commit-time refusal is
    // the **one** this test names. Without it the row is unpublishable for two
    // reasons at once, and an `any(code == ...)` assertion would stay green with
    // the rounding rule deleted — the second fault answering for the first.
    common::schedule_coverage_window(
        &h.provider.conn().expect("conn"),
        &h.scope,
        TENANT,
        Uuid::from_u128(0xb_0002),
        stamp(),
    )
    .await;

    let refusal = h
        .publish
        .commit(
            &ctx(),
            &h.scope,
            TENANT,
            PlanPublishUnit::plan_content(plan_id(), revision),
            version,
            PublishAuthorization::auto_publishable(),
            ACTOR,
            CORRELATION,
            at(12),
        )
        .await
        .expect_err("the commit-time run must refuse");

    match &refusal {
        DomainError::ValidationFailed(report) => {
            let codes: Vec<&str> = report.violations.iter().map(|v| v.code.as_str()).collect();
            assert_eq!(
                codes,
                ["ROUNDING_POLICY_UNRESOLVED"],
                "exactly the fault this test staged, and nothing standing in for it"
            );
        }
        other => panic!("expected a validation report, got {other:?}"),
    }

    // The subject is exactly where it was.
    let still = h
        .plans
        .find_revision(&h.scope, TENANT, plan_id(), revision)
        .await
        .expect("read the revision")
        .expect("it is there");
    assert_eq!(still.lifecycle_state, LifecycleState::Draft);
    assert_eq!(still.row_version, version, "its tag did not move either");

    // And nothing was written anywhere.
    assert!(version_refs(&h).await.is_empty());
    assert!(outbox_rows(&h).await.is_empty());
    assert!(
        publish_records(&h).await.is_empty(),
        "the refusal wrote no publish record; the authoring seeds' records are theirs"
    );
    assert_seam_holds(&h).await;
}

#[tokio::test]
async fn registry_absence_stops_the_publish_and_writes_nothing() {
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
    let h = Harness {
        // Unread by this case, which is about an unconfigured registry: the
        // publish it stages never reaches a rule run.
        metrics: MetricsHarness::new(),
        plans: PlanRepo::new(provider.clone()),
        shapes: PlanShapeRepo::new(provider.clone()),
        prices: PriceRepo::new(provider.clone()),
        publish: PublishService::new(
            provider.clone(),
            &LimitsConfig::default(),
            FixtureGate::load(&committed_registry_path()),
            // The crate's real default, and the only registry it can have until
            // the registry gear exists.
            Arc::new(UnconfiguredCatalogVersionRegistryV1),
        ),
        registry: Arc::new(RegistryDouble::default()),
        scope: AccessScope::for_tenant(TENANT),
        provider,
    };
    let (revision, version, _) = seed_publishable(&h).await;

    let refusal = h
        .publish
        .commit(
            &ctx(),
            &h.scope,
            TENANT,
            PlanPublishUnit::plan_content(plan_id(), revision),
            version,
            PublishAuthorization::auto_publishable(),
            ACTOR,
            CORRELATION,
            at(12),
        )
        .await
        .expect_err("a publish with no registry must stop");

    assert!(
        matches!(refusal, DomainError::CatalogVersionUnavailable(_)),
        "got {refusal:?}"
    );
    assert_eq!(
        h.plans
            .find_revision(&h.scope, TENANT, plan_id(), revision)
            .await
            .expect("read the revision")
            .expect("it is there")
            .lifecycle_state,
        LifecycleState::Draft
    );
    assert!(version_refs(&h).await.is_empty());
    assert!(outbox_rows(&h).await.is_empty());
    assert!(
        publish_records(&h).await.is_empty(),
        "the refusal wrote no publish record; the authoring seeds' records are theirs"
    );
    assert_seam_holds(&h).await;
}

#[tokio::test]
async fn a_stale_row_version_is_refused_and_writes_nothing() {
    let h = harness().await;
    let (revision, _, _) = seed_publishable(&h).await;

    let refusal = h
        .publish
        .commit(
            &ctx(),
            &h.scope,
            TENANT,
            PlanPublishUnit::plan_content(plan_id(), revision),
            RowVersion::new(99),
            PublishAuthorization::auto_publishable(),
            ACTOR,
            CORRELATION,
            at(12),
        )
        .await
        .expect_err("a stale version must be refused");

    assert!(
        matches!(refusal, DomainError::StaleVersion(_)),
        "got {refusal:?}"
    );
    assert_eq!(
        h.plans
            .find_revision(&h.scope, TENANT, plan_id(), revision)
            .await
            .expect("read the revision")
            .expect("it is there")
            .lifecycle_state,
        LifecycleState::Draft
    );
    assert!(version_refs(&h).await.is_empty());
    assert!(outbox_rows(&h).await.is_empty());
    assert!(
        publish_records(&h).await.is_empty(),
        "the refusal wrote no publish record; the authoring seeds' records are theirs"
    );
}

#[tokio::test]
async fn a_retried_commit_re_requests_the_same_handle_rather_than_orphaning_one() {
    // The registry's own idempotency is what bounds the cost of holding the
    // request inside an open transaction, and the deterministic `request_id` is
    // what lets a caller use it.
    let registry = Arc::new(RegistryDouble::default());
    let h = harness_with(Arc::clone(&registry)).await;
    let (revision, version, _) = seed_publishable(&h).await;

    // A commit that fails after the request: the stale version is checked
    // inside `publish_revision`, which runs after the registry call.
    let _ = h
        .publish
        .commit(
            &ctx(),
            &h.scope,
            TENANT,
            PlanPublishUnit::plan_content(plan_id(), revision),
            RowVersion::new(99),
            PublishAuthorization::auto_publishable(),
            ACTOR,
            CORRELATION,
            at(12),
        )
        .await
        .expect_err("the stale version refuses it");
    assert_eq!(h.registry.handles(), 1, "one handle was minted");
    assert_eq!(h.registry.calls(), 1, "and the refused commit asked once");

    h.publish
        .commit(
            &ctx(),
            &h.scope,
            TENANT,
            PlanPublishUnit::plan_content(plan_id(), revision),
            version,
            PublishAuthorization::auto_publishable(),
            ACTOR,
            CORRELATION,
            at(12),
        )
        .await
        .expect("the retry commits");

    assert_eq!(
        h.registry.handles(),
        1,
        "the retry presented the same request_id and got the same handle back, \
         rather than orphaning the first at the registry"
    );
    assert_eq!(
        h.registry.calls(),
        2,
        "and it did present it: one handle across one ask is a retry that never \
         reached the registry, which satisfies the count above for the wrong reason"
    );
    assert_eq!(version_refs(&h).await.len(), 1);
}

#[tokio::test]
async fn a_plan_with_no_open_draft_revision_has_nothing_to_publish() {
    let h = harness().await;

    let refusal = h
        .publish
        .commit(
            &ctx(),
            &h.scope,
            TENANT,
            PlanPublishUnit::plan_content(plan_id(), 0),
            RowVersion::new(0),
            PublishAuthorization::auto_publishable(),
            ACTOR,
            CORRELATION,
            at(12),
        )
        .await
        .expect_err("there is no draft revision");

    assert!(
        matches!(refusal, DomainError::NotFound { .. }),
        "got {refusal:?}"
    );
}

#[tokio::test]
async fn the_unit_must_name_the_revision_the_plan_actually_holds_open() {
    let h = harness().await;
    let (revision, version, _) = seed_publishable(&h).await;

    let refusal = h
        .publish
        .commit(
            &ctx(),
            &h.scope,
            TENANT,
            // A revision this plan does not hold open.
            PlanPublishUnit::plan_content(plan_id(), revision + 7),
            version,
            PublishAuthorization::auto_publishable(),
            ACTOR,
            CORRELATION,
            at(12),
        )
        .await
        .expect_err("the unit must name the open draft");

    assert!(
        matches!(refusal, DomainError::NotFound { .. }),
        "got {refusal:?}"
    );
    assert!(version_refs(&h).await.is_empty());
}

#[tokio::test]
async fn a_failure_at_the_last_write_takes_the_four_before_it_back() {
    // The property this suite's module doc claims and the three refusal tests
    // above do **not** exercise: each of those refuses before the first write,
    // so their "nothing was written" assertions are true vacuously - there was
    // nothing to roll back. This one fails at step 6, the audit append, with the
    // revision flip, the price flip, the ref row and the outbox row already
    // issued inside the transaction.
    //
    // The failure is injected by putting the plan's audit segment into a state
    // `append` cannot extend: a head row whose `row_hash` is not 32 bytes, which
    // is an invariant breach the writer refuses rather than pads, because a
    // padded link is one the verification job reports as a break with no way
    // back to what was hashed.
    let h = harness().await;
    let (revision, version, price_id) = seed_publishable(&h).await;
    let conn = h.provider.conn().expect("conn");
    let corrupt = audit_log::ActiveModel {
        tenant_id: sea_orm::ActiveValue::Set(TENANT),
        chain_id: sea_orm::ActiveValue::Set(plan_id().get()),
        // Above the authoring seeds' records, so this row IS the head the
        // append will try to link to.
        seq: sea_orm::ActiveValue::Set(SEEDED_AUTHORING_RECORDS),
        entry_kind: sea_orm::ActiveValue::Set("mutation".to_owned()),
        recorded_at: sea_orm::ActiveValue::Set(at(9)),
        actor_principal_id: sea_orm::ActiveValue::Set(ACTOR),
        action: sea_orm::ActiveValue::Set("publish".to_owned()),
        subject_kind: sea_orm::ActiveValue::Set("plan_revision".to_owned()),
        subject_ref: sea_orm::ActiveValue::Set("plan/0".to_owned()),
        before_state: sea_orm::ActiveValue::Set(None),
        after_state: sea_orm::ActiveValue::Set(None),
        approval_ref: sea_orm::ActiveValue::Set(None),
        correlation_id: sea_orm::ActiveValue::Set(None),
        segment_heads: sea_orm::ActiveValue::Set(None),
        prev_hash: sea_orm::ActiveValue::Set(None),
        row_hash: sea_orm::ActiveValue::Set(vec![0_u8]),
    };
    audit_log::Entity::insert(corrupt.clone())
        .secure()
        .scope_with_model(&h.scope, &corrupt)
        .expect("scope the seeded head")
        .exec(&conn)
        .await
        .expect("seed a head the writer cannot link to");

    let refusal = h
        .publish
        .commit(
            &ctx(),
            &h.scope,
            TENANT,
            PlanPublishUnit::plan_content(plan_id(), revision),
            version,
            PublishAuthorization::auto_publishable(),
            ACTOR,
            CORRELATION,
            at(12),
        )
        .await
        .expect_err("the audit append must refuse");
    assert!(
        matches!(refusal, DomainError::Internal(_)),
        "got {refusal:?}"
    );

    // The four writes that had already landed are gone.
    let still = h
        .plans
        .find_revision(&h.scope, TENANT, plan_id(), revision)
        .await
        .expect("read the revision")
        .expect("it is there");
    assert_eq!(
        still.lifecycle_state,
        LifecycleState::Draft,
        "the revision flip rolled back"
    );
    assert_eq!(still.row_version, version, "and so did its tag");
    assert_eq!(
        h.prices
            .find(&h.scope, TENANT, price_id)
            .await
            .expect("read the row")
            .expect("it is there")
            .lifecycle_state,
        LifecycleState::Draft,
        "the price flip rolled back"
    );
    assert!(
        version_refs(&h).await.is_empty(),
        "the pending ref rolled back"
    );
    assert!(outbox_rows(&h).await.is_empty(), "the event rolled back");
    assert_eq!(
        audit_rows(&h).await.len(),
        usize::try_from(SEEDED_AUTHORING_RECORDS).expect("a small count") + 1,
        "only the authoring seeds and the seeded head remain; the append never landed"
    );
    assert_seam_holds(&h).await;
}

#[tokio::test]
async fn a_row_authored_after_the_precheck_is_judged_by_the_second_run() {
    // §4.2's clause, from the other side. The pre-check is a pre-check: it does
    // not pin the subject, and the commit re-assembles and re-validates. So a
    // row authored between the two is **not** left behind — it is judged by the
    // second run, and publishes because it passed. The sibling case, where the
    // late row fails, is `a_commit_time_validation_failure_writes_nothing_at_all`.
    //
    // (The name used to promise the opposite and the body did neither. What
    // cannot be shown at this level is a row landing *inside* the commit's own
    // transaction — `sqlite::memory:` will not schedule a concurrent writer —
    // and that half is proven at the repository, where the validated set is an
    // argument: `a_row_authored_after_validation_is_not_published_by_this_commit`
    // in `tests/sqlite_price_repo.rs`.)
    let h = harness().await;
    let (revision, version, judged) = seed_publishable(&h).await;

    let clean = h
        .publish
        .precheck(&h.scope, TENANT, plan_id(), at(11))
        .await
        .expect("the pre-check runs");
    assert!(
        clean.is_publishable(),
        "the subject was publishable: {clean:?}"
    );

    // Authored after the pre-check said yes, and perfectly valid.
    let late = Uuid::from_u128(0xb_0002);
    h.prices
        .create_draft(
            &h.scope,
            TENANT,
            NewPriceDraft {
                price_id: late,
                scope_key: scope_key(PriceEligibility::NewSubscriptionsOnly),
                content: flat_row(),
                created_by: ACTOR,
                created_at_utc: at(11),
                correlation_id: TEST_CORRELATION,
            },
        )
        .await
        .expect("author the late row");

    // The late row is on a **different** canonical scope key
    // (`new_subscriptions_only`), so `inst-wc-perkey` gives it its own coverage
    // obligation: covering the seed's key covers nothing else. This test is about
    // the late row *publishing*, so it has to be publishable.
    common::schedule_coverage_window(
        &h.provider.conn().expect("conn"),
        &h.scope,
        TENANT,
        late,
        stamp(),
    )
    .await;

    let receipt = h
        .publish
        .commit(
            &ctx(),
            &h.scope,
            TENANT,
            PlanPublishUnit::plan_content(plan_id(), revision),
            version,
            PublishAuthorization::auto_publishable(),
            ACTOR,
            CORRELATION,
            at(12),
        )
        .await
        .expect("the publish commits");

    let mut published = receipt.published_price_ids().to_vec();
    published.sort();
    let mut both = vec![judged, late];
    both.sort();
    assert_eq!(
        published, both,
        "the second run judged the late row and published it"
    );
    for price_id in both {
        let row = h
            .prices
            .find(&h.scope, TENANT, price_id)
            .await
            .expect("read the row")
            .expect("it is there");
        assert_eq!(row.lifecycle_state, LifecycleState::Published);
        assert_eq!(
            row.row_version,
            RowVersion::new(0),
            "publishing changes no content, so the tag it was validated at is the tag it keeps"
        );
    }
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

/// The stamp a decision is taken under: who acted, when, and the request's
/// correlation.
fn stamp_of(actor: uuid::Uuid, when: OffsetDateTime) -> bss_pricing::domain::audit::AuditStamp {
    bss_pricing::domain::audit::AuditStamp {
        actor_principal_id: actor,
        recorded_at: when,
        correlation_id: TEST_CORRELATION,
    }
}

// ---------------------------------------------------------------------------
// The vocabulary and the chain, over every writer the crate has (D-135, D-158).
// ---------------------------------------------------------------------------

/// Drive **every** audited path this crate has, on one plan, and return the
/// segment's rows in seq order.
///
/// One plan on purpose: D-135 keys the segment on the audited subject's
/// aggregate, so the whole sequence extends one chain and the walk below is the
/// walk a verification job makes.
/// The approval plane's five audited paths, on the plan the caller has already
/// driven the authoring ones over.
///
/// Split out of [`drive_every_audited_path`] because the two planes are two
/// stories and one function telling both is what `clippy::cognitive_complexity`
/// is measuring; the segment is the same either way, which is the point of the
/// caller.
async fn drive_the_approval_plane(h: &Harness) {
    // A self-approval attempt, refused: `deny`. It is the one audited path whose
    // record is **not** a mutation - see `AuditAction::Deny` - and it is driven
    // here rather than only in `tests/sqlite_approval_service.rs` because this
    // is the test that holds the vocabulary and its writers to each other.
    //
    // The unit is opened over the plan's *current* draft revision, which the
    // abandon above consumed, so a fresh one is opened first.
    h.plans
        .open_revision(&h.scope, TENANT, plan_id(), stamp_of(ACTOR, at(15)))
        .await
        .expect("open a revision to submit");
    let approvals = ApprovalService::new(h.provider.clone());
    let approval_id = Uuid::from_u128(0xa_0001);
    approvals
        .submit(
            &h.scope,
            TENANT,
            plan_id(),
            approval_id,
            serde_json::json!({ "reason": "noConfiguredThreshold" }),
            stamp_of(ACTOR, at(15)),
        )
        .await
        .expect("open the pending unit");
    let refused = approvals
        .decide(
            &h.scope,
            TENANT,
            DecideRequest {
                approval_id,
                // The submitter, which is what makes it a violation.
                decision: DecisionBy::Approve(ACTOR),
                reason: None,
                approver_regions: RegionGrant::Explicit(std::collections::BTreeSet::from([
                    Region::new("eu").expect("a non-blank region"),
                ])),
                stamp: stamp_of(ACTOR, at(16)),
                withdraw_authority: WithdrawAuthority::OwnUnitsOnly,
            },
        )
        .await
        .expect_err("the submitter may not approve their own unit");
    assert!(
        matches!(refused, DomainError::SelfApprovalForbidden(_)),
        "got {refused:?}"
    );

    // The three decisions an independent reviewer can take, each on its own
    // unit: `approve`, `reject`, `withdraw`. They are driven in sequence rather
    // than in parallel because `inst-co-single-pending` allows a plan **one**
    // pending unit at a time, so each has to be decided before the next opens -
    // which is also why each `submit` below lands its own `submit` record.
    let approved = decide_one(
        h,
        &approvals,
        approval_id,
        DecisionBy::Approve(APPROVER),
        None,
    )
    .await;
    assert_eq!(approved.state, ApprovalState::Approved);

    let rejected_id = Uuid::from_u128(0xa_0002);
    open_unit(h, &approvals, rejected_id).await;
    let rejected = decide_one(
        h,
        &approvals,
        rejected_id,
        DecisionBy::Reject(APPROVER),
        Some("margin below floor".to_owned()),
    )
    .await;
    assert_eq!(rejected.state, ApprovalState::Rejected);

    let withdrawn_id = Uuid::from_u128(0xa_0003);
    open_unit(h, &approvals, withdrawn_id).await;
    let withdrawn = decide_one(
        h,
        &approvals,
        withdrawn_id,
        // The **submitter's own** withdraw, which is the case `inst-as-void`
        // names and the one whose actor `approver_principal` cannot hold.
        DecisionBy::Void(Some(ACTOR)),
        Some("superseded by a later change set".to_owned()),
    )
    .await;
    assert_eq!(withdrawn.state, ApprovalState::Voided);
}

/// Open one pending unit over the plan's current draft revision.
async fn open_unit(h: &Harness, approvals: &ApprovalService, approval_id: Uuid) {
    approvals
        .submit(
            &h.scope,
            TENANT,
            plan_id(),
            approval_id,
            serde_json::json!({ "reason": "noConfiguredThreshold" }),
            stamp_of(ACTOR, at(15)),
        )
        .await
        .expect("open the pending unit");
}

/// Schedule a window through the service, so the `window` subject kind has a writer.
///
/// The plan is already `published` by the time this runs, which is the ordinary
/// subject of a window mutation and the reason the service resolves the plan's
/// **current** revision rather than an open draft.
/// The D-10 threshold-policy proposal — the writer of the `policy` subject kind.
///
/// It is here rather than in a suite of its own because this file owns the
/// **vocabulary** property: `every_declared_action_has_a_production_writer` walks
/// what driving the crate's own paths actually wrote, and a token whose writer is
/// never driven is indistinguishable from a token with no writer. Adding
/// `AuditSubjectKind::Policy` without this line fails that test, which is the
/// direction D-158 asks the guard to fail in.
///
/// The proposal writes a `submit` record on the **policy** chain, not on any plan's
/// — `audit_repo::policy_chain`, a per-tenant segment — so it also exercises the
/// only subject kind whose aggregate is not a plan.
async fn drive_the_threshold_policy_plane(h: &Harness) {
    let thresholds = bss_pricing::infra::threshold::ThresholdService::new(h.provider.clone());
    let unit = Uuid::now_v7();
    // The `If-Match` premise the proposal asserts (D-186), read off the service so
    // this fixture cannot drift from the one producer of the tag. This tenant has no
    // policy, so it is the bootstrap's tag — the case the withdrawn premise claimed
    // was unreachable. The instant is the act's own `at(16)`, not a second wall-clock
    // reading, which is what `AssertedPolicy` exists to keep together.
    let asserted = bss_pricing::infra::threshold::AssertedPolicy {
        tag: thresholds
            .state(&h.scope, TENANT)
            .await
            .expect("the policy state reads")
            .tag(),
        now: at(16),
    };
    thresholds
        .propose(
            &h.scope,
            TENANT,
            unit,
            at(16),
            vec![bss_pricing::domain::materiality::ThresholdEntry {
                currency: bss_pricing::domain::money::CurrencyCode::new("EUR")
                    .expect("a valid code"),
                basis: bss_pricing::domain::materiality::ThresholdBasis::Absolute { minor: 500 },
            }],
            asserted,
            serde_json::json!({ "material": true, "reason": "alwaysMaterialTrigger" }),
            stamp_of(ACTOR, at(16)),
        )
        .await
        .expect("the proposal opens its unit");
    // **And an independent principal approves it**, which is what makes it the
    // tenant's policy (D-10). Not decoration for this census: the window plane below
    // needs a configured entry for `EUR` or its mutation is material and writes
    // nothing. `approver_regions` is the explicit grant the scope rule measures
    // against, and a policy unit's change set touches no region at all.
    ApprovalService::new(h.provider.clone())
        .decide(
            &h.scope,
            TENANT,
            DecideRequest {
                approval_id: unit,
                decision: DecisionBy::Approve(APPROVER),
                reason: None,
                approver_regions: RegionGrant::Explicit(std::collections::BTreeSet::from([
                    Region::new("eu").expect("a non-blank region"),
                ])),
                stamp: stamp_of(APPROVER, at(17)),
                withdraw_authority: WithdrawAuthority::OwnUnitsOnly,
            },
        )
        .await
        .expect("an independent principal puts the policy in force");
}

async fn drive_the_window_plane(h: &Harness) {
    let windows = bss_pricing::infra::window::WindowService::new(
        h.provider.clone(),
        Arc::clone(&h.registry) as Arc<dyn CatalogVersionRegistryV1>,
    );
    let price_id = h
        .prices
        .list_for_plan(&h.scope, TENANT, plan_id(), &[LifecycleState::Published])
        .await
        .expect("read the published rows")
        .first()
        .expect("the commit above published one")
        .price_id;
    windows
        .schedule(
            &ctx(),
            &h.scope,
            TENANT,
            price_id,
            Uuid::now_v7(),
            // **Exactly** where the plan's seeded window ends, which is legal and
            // deliberate on two counts: the intervals are half-open, so
            // `effectiveTo == next.effectiveFrom` is adjacency and not an overlap,
            // and it therefore opens no interior gap for `inst-fg-detect` either.
            // Scheduling anywhere earlier collides with that window; anywhere later
            // opens a hole, and either would make this census fail for a reason
            // that has nothing to do with the vocabulary it is about.
            //
            // 2099 is a fact rather than a date off the clock: a window dated today
            // races the activation sweep, which is a defect this program has already
            // paid for once.
            utc_ymd_hms(2099, 9, 1, 0, 0, 0),
            None,
            "audited-window-writer".to_owned(),
            bss_pricing::api::rest::windows::verdict_json,
            stamp_of(ACTOR, at(18)),
        )
        .await
        .expect("schedule a window as a publish unit");
}

/// Take one decision on a pending unit, as an in-scope reviewer.
async fn decide_one(
    h: &Harness,
    approvals: &ApprovalService,
    approval_id: Uuid,
    decision: DecisionBy,
    reason: Option<String>,
) -> bss_pricing::infra::storage::repo::approval_repo::ApprovalRecord {
    let actor = decision
        .decider()
        .expect("a human decision names its actor");
    approvals
        .decide(
            &h.scope,
            TENANT,
            DecideRequest {
                approval_id,
                decision,
                reason,
                approver_regions: RegionGrant::Explicit(std::collections::BTreeSet::from([
                    Region::new("eu").expect("a non-blank region"),
                ])),
                stamp: stamp_of(actor, at(17)),
                withdraw_authority: WithdrawAuthority::OwnUnitsOnly,
            },
        )
        .await
        .expect("the decision is taken")
}

/// The overlay plane, for the `price_overlay` subject kind.
///
/// The census below insists every **declared** kind is produced by driving the
/// crate's own paths, so a fifth member of `AuditSubjectKind` obliges a fifth
/// driver — which is the whole point of the census and is why this is here rather
/// than the assertion being narrowed. `OverlayRepo::create` alone is enough: what is
/// being censused is the kind, and the four actions it writes are all covered by
/// the plan plane above. `sqlite_overlay_repo::every_overlay_mutation_appends_exactly_one_audit_record`
/// is where the four are held apart.
///
/// It reaches the repository directly rather than the route, deliberately: the route
/// would drag §5's whole validation pipeline and a taxonomy fixture into a test about
/// a vocabulary, and the repository **is** the writer — unlike `window_repo`, which
/// takes an `AuditStamp` and writes nothing, and whose census entry therefore has to
/// go through its service.
async fn drive_the_overlay_plane(h: &Harness) {
    let overlays = bss_pricing::infra::storage::repo::OverlayRepo::new(h.provider.clone());
    overlays
        .create(
            &h.scope,
            bss_pricing::infra::storage::repo::NewOverlay {
                price_overlay_id: Uuid::from_u128(0xb_0009),
                tenant_id: TENANT,
                scope: bss_pricing::domain::overlay::ScopeSelector::scoped(
                    bss_pricing::domain::overlay::ScopeClass::Brand,
                    bss_pricing::domain::overlay::ScopeValue::new("acme")
                        .expect("a non-blank value"),
                )
                .expect("brand is not the global class"),
                precedence: 10,
                interval: bss_pricing::domain::overlay::OverlayInterval::default(),
                tax_basis: bss_pricing::domain::overlay::TaxBasis::DelegatedTariffs,
                disclosure: bss_pricing::domain::overlay::Disclosure::Restricted,
                // Empty: the overlay targets no plan in particular, which is the
                // one shape a list-default line can serve on its own.
                target_ref: bss_pricing::domain::overlay::TargetRef { plans: Vec::new() },
            },
            vec![bss_pricing::domain::overlay::OverlayLine {
                line_id: Uuid::from_u128(0xb_000a),
                key: bss_pricing::domain::overlay::LineKey::list_default(),
                adjustment: bss_pricing::domain::overlay::Adjustment::Discount(
                    bss_pricing::domain::overlay::Magnitude::PercentBp(1000),
                ),
            }],
            stamp_of(ACTOR, at(19)),
        )
        .await
        .expect("an overlay is created, and audited");
}

/// The mass-repricing run's open, the writer of the `bulk_operation` subject kind
/// (D-158, the apply-lane's task 3).
///
/// There is no infra service to call through here, unlike every other plane
/// above: the run's open lives directly in `api::rest::repricing_runs`, composed
/// from three `pub` repository calls inside one transaction —
/// `bulk_repo::open`, `repricing_journal_repo::open_rows`, then
/// `audit_repo::append`. This driver makes the same two calls this census cares
/// about (the journal freeze is `rest_repricing_runs.rs`'s to prove, not this
/// suite's), in the same transaction, so what the census reads is the real
/// writer's own record and not a stand-in for it.
async fn drive_the_bulk_operation_plane(h: &Harness) {
    let operation_id = Uuid::from_u128(0xb_000b);
    let (_, outcome) = h
        .provider
        .db()
        .in_transaction::<(), bss_pricing::infra::storage::RepoError, _>({
            let scope = h.scope.clone();
            move |txn| {
                Box::pin(async move {
                    let run = bulk_repo::open(
                        txn,
                        &scope,
                        NewBulkOperation {
                            operation_id,
                            tenant_id: TENANT,
                            kind: BulkKind::Repricing,
                            client_key: operation_id.to_string(),
                            request_hash: IdempotencyGate::payload_hash(&operation_id.to_string()),
                            report: serde_json::json!({}),
                            submitted_by: ACTOR,
                            submitted_at: at(20),
                        },
                    )
                    .await?;
                    audit_repo::append(
                        txn,
                        &scope,
                        NewAuditEntry {
                            tenant_id: TENANT,
                            chain_id: audit_repo::bulk_operation_chain(run.operation_id),
                            recorded_at: at(20),
                            actor_principal_id: ACTOR,
                            action: AuditAction::Create,
                            subject_kind: AuditSubjectKind::BulkOperation,
                            subject_ref: audit_repo::bulk_operation_ref(run.operation_id),
                            before_state: None,
                            after_state: Some(serde_json::json!({ "kind": "repricing" })),
                            approval_ref: None,
                            correlation_id: TEST_CORRELATION,
                        },
                    )
                    .await?;
                    Ok(())
                })
            }
        })
        .await;
    outcome.expect("the run opens and records itself");
}

/// The scope-value taxonomy plane — the writer of the `taxonomy_value` subject
/// kind (D-353). A declaration is one `create` record naming the value; the
/// governed edit's `update` record needs an approved unit and is driven by
/// `rest_taxonomies`, so only the kind is new here, `create` being produced above.
async fn drive_the_taxonomy_plane(h: &Harness) {
    use bss_pricing::domain::overlay::ScopeValue;
    use bss_pricing::domain::taxonomy::{TaxonomyClass, TaxonomyEntry, TaxonomyState};
    use bss_pricing::infra::storage::repo::taxonomy_repo::TaxonomyRepo;

    TaxonomyRepo::new(h.provider.clone())
        .declare_value(
            &h.scope,
            TENANT,
            TaxonomyClass::Brand,
            TaxonomyEntry {
                value: ScopeValue::new("acme").expect("a value"),
                display_name: "Acme".to_owned(),
                state: TaxonomyState::Active,
                tax: None,
            },
            stamp_of(ACTOR, at(22)),
        )
        .await
        .expect("the value declares, and audited");
}

/// The customer-group membership plane — the writer of the `membership`
/// subject kind (D-09, `inst-mm-audit`, Task 4 of the customer-group plane).
///
/// There is no infra service to call through here either, `drive_the_bulk_
/// operation_plane`'s exact situation: no route exists yet for this plane
/// (`group_membership_repo`'s own module doc names the gap), so this driver
/// calls `group_membership_repo::enroll` and `end_membership` directly, each
/// inside its own transaction — the shape a real caller gets once a route
/// exists, since both functions take a runner rather than a provider and open
/// no transaction of their own.
///
/// Two calls and not one so the segment holds `inst-ms-time`'s two acts —
/// "ending early = setting `to` (audited)" is a distinct write from the
/// enrollment it ends. **Neither adds a new action token**: `enroll` writes
/// `create` and `end_membership` writes `update`, both already produced above
/// (the plan's own draft and its facet edits), so only the `membership`
/// **subject kind** is new to this census — the half that was actually
/// failing.
async fn drive_the_membership_plane(h: &Harness) {
    let payer_tenant_id = Uuid::from_u128(0xb_000c);
    let membership_id = Uuid::from_u128(0xb_000d);

    let (_, enrolled) = h
        .provider
        .db()
        .in_transaction::<(), bss_pricing::infra::storage::RepoError, _>({
            let scope = h.scope.clone();
            move |txn| {
                Box::pin(async move {
                    group_membership_repo::enroll(
                        txn,
                        &scope,
                        TENANT,
                        NewMembership {
                            membership_id,
                            tenant_id: TENANT,
                            payer_tenant_id,
                            group_value: "acme-tier".to_owned(),
                            effective_from: at(21),
                            effective_to: None,
                        },
                        stamp_of(ACTOR, at(21)),
                    )
                    .await
                    .map(|_| ())
                })
            }
        })
        .await;
    enrolled.expect("the payer enrolls, and audited");

    let (_, ended) = h
        .provider
        .db()
        .in_transaction::<(), bss_pricing::infra::storage::RepoError, _>({
            let scope = h.scope.clone();
            move |txn| {
                Box::pin(async move {
                    group_membership_repo::end_membership(
                        txn,
                        &scope,
                        TENANT,
                        membership_id,
                        at(22),
                        // Fresh from `enroll`, which always writes `row_version = 0`.
                        0,
                        stamp_of(ACTOR, at(22)),
                    )
                    .await
                    .map(|_| ())
                })
            }
        })
        .await;
    ended.expect("the membership ends, and audited");
}

async fn drive_every_audited_path(h: &Harness) -> Vec<audit_log::Model> {
    // create (plan), update x2 (facets), create (price)
    let (revision, version, _) = seed_publishable(h).await;
    // publish
    h.publish
        .commit(
            &ctx(),
            &h.scope,
            TENANT,
            PlanPublishUnit::plan_content(plan_id(), revision),
            version,
            PublishAuthorization::auto_publishable(),
            ACTOR,
            CORRELATION,
            at(12),
        )
        .await
        .expect("the publish commits");

    // A second draft price row, authored and then discarded: `delete`.
    let doomed = Uuid::from_u128(0xb_0002);
    h.prices
        .create_draft(
            &h.scope,
            TENANT,
            NewPriceDraft {
                price_id: doomed,
                scope_key: scope_key(PriceEligibility::NewSubscriptionsOnly),
                content: flat_row(),
                created_by: ACTOR,
                created_at_utc: at(13),
                correlation_id: TEST_CORRELATION,
            },
        )
        .await
        .expect("author a second row");
    h.prices
        .delete_draft(&h.scope, TENANT, doomed, RowVersion::new(0), stamp(), None)
        .await
        .expect("discard it");

    // A successor revision, opened and then discarded: `abandon`.
    let opened = h
        .plans
        .open_revision(&h.scope, TENANT, plan_id(), stamp_of(ACTOR, at(14)))
        .await
        .expect("open a successor");
    h.plans
        .abandon_draft(
            &h.scope,
            TENANT,
            plan_id(),
            opened.revision,
            opened.row_version,
            stamp(),
        )
        .await
        .expect("discard it");

    // The approval plane's five: `submit`, `deny`, `approve`, `reject`, `withdraw`.
    drive_the_approval_plane(h).await;

    // Slice 7's window plane, which writes the `window` subject kind. It is driven
    // through `WindowService` and not through the repository, because the record is
    // the *service's* — `window_repo` takes an `AuditStamp` and deliberately writes
    // no row, and a driver that reached past the service would leave this census
    // green with no window record in the store at all.
    //
    // The action it writes is `publish`, already covered by the plan commit above;
    // what this adds to the census is the **subject kind**, which is the half that
    // was failing. `inst-ws-publishunit` is the warrant: a window mutation runs the
    // Foundation §4.2 engine path, and S5 §6 declares `publish` against §4.2.
    // **The policy plane runs first, and the order is now load-bearing.** A window
    // schedule is not on `inst-mat-registered`'s trigger list, so what decides it is
    // the tenant's threshold policy: with none configured, `inst-mat-failsafe` answers,
    // the mutation writes **nothing at all** and this census loses the `window` subject
    // kind entirely. So the policy is proposed *and approved* before the window plane
    // is driven — which is also the operator sequence D-10 forces.
    drive_the_threshold_policy_plane(h).await;
    drive_the_window_plane(h).await;
    drive_the_overlay_plane(h).await;
    // Slice 12's mass-repricing run, which writes the `bulk_operation` subject
    // kind (D-158, the apply-lane's task 3). Its own aggregate, so its position
    // among the drivers is free — unlike the window plane it does not depend on
    // an approved policy, and unlike retirement it touches no plan the drivers
    // below still need alive.
    drive_the_bulk_operation_plane(h).await;
    // Task 4 of the customer-group plane's membership repository, which writes
    // the `membership` subject kind (D-09, `inst-mm-audit`). Its own aggregate —
    // `audit_repo::payer_chain`, keyed on the payer and not on any plan — so its
    // position among the drivers is as free as `drive_the_bulk_operation_plane`'s
    // own note says its neighbour's is.
    drive_the_membership_plane(h).await;
    drive_the_taxonomy_plane(h).await;
    // **Last, and the position is load-bearing.** Retirement is terminal: it
    // flips the plan's current revision to `retired`, after which no plane above
    // can publish anything on it. Driven here so the census sees the `retire`
    // record without every earlier driver having to work around a dead plan.
    // **Before retirement**, and the position is load-bearing for the same reason
    // retirement is last: a migration is scheduled *off* a plan that must still be
    // published, and retirement flips it to `retired` permanently.
    drive_the_migration_plane(h).await;
    drive_the_retirement_plane(h).await;

    let mut rows = audit_rows(h).await;
    rows.sort_by_key(|row| row.seq);
    rows
}

/// The migration plane — the writer of the `migrate` action (Slice 11).
///
/// It is here for `drive_the_threshold_policy_plane`'s stated reason: this file
/// owns the **vocabulary** property, and a token whose writer is never driven is
/// indistinguishable from a token with no writer. Adding `AuditAction::Migrate`
/// without this function fails `every_declared_action_has_a_production_writer`,
/// which is the direction D-158 asks the guard to fail in — and it did fail that
/// way when the variant landed, which is how this function came to exist.
///
/// **A migration needs a published target that is not the source**, so this seeds
/// a second plan and publishes its revision through the repository rather than
/// through the publish engine. That is deliberate and the shortcut is bounded:
/// what the scheduler asks of a target is its `lifecycle_state` and its price
/// rows, and a target with no rows is a legitimate shape here — no subscription
/// can be enumerated in this gear, so no boundary delta can be computed against
/// it either. Driving the whole engine for the target would prove the engine, not
/// the migration.
///
/// Unlike retirement this opens **no approval unit**: §5 types migration
/// scheduling `plan x migrate` and `inst-mat-registered` registers no migration
/// trigger, so the schedule commits on one principal and the record carries no
/// `approval_ref`.
async fn drive_the_migration_plane(h: &Harness) {
    let target = PlanId::new(Uuid::from_u128(0x_7a_46_e7));
    let mut draft = new_plan_draft();
    draft.plan_id = target;
    draft.created_at_utc = at(17);
    let created = h
        .plans
        .create_draft(&h.scope, draft)
        .await
        .expect("create the target draft");

    let (_, published) = h
        .provider
        .db()
        .in_transaction::<(), bss_pricing::infra::storage::RepoError, _>({
            let scope = h.scope.clone();
            move |txn| {
                Box::pin(async move {
                    bss_pricing::infra::storage::repo::plan_repo::publish_revision(
                        txn,
                        &scope,
                        TENANT,
                        target,
                        created.revision,
                        created.row_version,
                    )
                    .await
                    .map(|_| ())
                })
            }
        })
        .await;
    published.expect("publish the target revision");

    let migrations = bss_pricing::infra::migration::MigrationService::new(
        h.provider.clone(),
        &bss_pricing::config::LimitsConfig::default(),
    );
    migrations
        .schedule(
            &h.scope,
            TENANT,
            bss_pricing::infra::migration::ScheduleRequest {
                migration_id: Uuid::now_v7(),
                source_plan_id: plan_id(),
                target_plan_id: target,
                effective_at: at(17) + time::Duration::days(120),
                scope_json: serde_json::json!({ "kind": "all" }),
            },
            stamp_of(ACTOR, at(17)),
        )
        .await
        .expect("the migration schedules");
}

/// The retirement plane — the writer of the `retire` action (D-128, Slice 11).
///
/// It takes **two** calls, and that is the property rather than an inconvenience:
/// retirement is a registered always-material trigger (D-109), so the first call
/// can only open a unit, and no threshold policy can make it do otherwise. The
/// commit happens on the second call, after an **independent** approver has
/// decided - which is also what makes the record this census reads carry an
/// `approval_ref`.
///
/// A driver that reached past `RetirementService` into `plan_repo` would flip the
/// row and leave this census green with no audit record in the store at all,
/// which is the failure mode `drive_the_window_plane` records one plane over.
async fn drive_the_retirement_plane(h: &Harness) {
    let approvals = ApprovalService::new(h.provider.clone());
    let retirements = bss_pricing::infra::retirement::RetirementService::new(
        h.provider.clone(),
        Arc::clone(&h.registry) as Arc<_>,
    );

    let opened = retirements
        .retire(
            &ctx(),
            &h.scope,
            TENANT,
            plan_id(),
            bss_pricing::api::rest::windows::verdict_json,
            stamp_of(ACTOR, at(16)),
        )
        .await
        .expect("compose the retirement");
    let bss_pricing::infra::retirement::RetirementOutcome::SubmittedForApproval(pending) = opened
    else {
        panic!("a retirement may not commit on one principal (D-109)");
    };

    decide_one(
        h,
        &approvals,
        pending.approval.approval_id,
        DecisionBy::Approve(APPROVER),
        None,
    )
    .await;

    let committed = retirements
        .retire(
            &ctx(),
            &h.scope,
            TENANT,
            plan_id(),
            bss_pricing::api::rest::windows::verdict_json,
            stamp_of(ACTOR, at(17)),
        )
        .await
        .expect("commit the retirement");
    assert!(
        matches!(
            committed,
            bss_pricing::infra::retirement::RetirementOutcome::Retired(_)
        ),
        "an approved retirement commits"
    );
}

#[tokio::test]
async fn every_declared_action_has_a_production_writer() {
    // D-158's constraint, as a guard rather than a promise: **a token with no
    // writer is not declared**, because a vocabulary entry nobody writes reads as
    // coverage to everyone who greps for it. This walks `AuditAction::ALL` and
    // insists each one is produced by driving the crate's own paths.
    //
    // Delete a writer and this fails; add a token without one and this fails.
    // Those are the two directions the vocabulary can drift in.
    let h = harness().await;
    let rows = drive_every_audited_path(&h).await;

    let written: std::collections::BTreeSet<String> =
        rows.iter().map(|row| row.action.clone()).collect();
    let declared: std::collections::BTreeSet<String> = AuditAction::ALL
        .iter()
        .map(|action| action.as_str().to_owned())
        .collect();

    assert_eq!(
        written, declared,
        "every declared action has a writer, and every writer's action is declared"
    );

    // The same, one column over.
    let kinds: std::collections::BTreeSet<String> =
        rows.iter().map(|row| row.subject_kind.clone()).collect();
    let declared_kinds: std::collections::BTreeSet<String> = AuditSubjectKind::ALL
        .iter()
        .map(|kind| kind.as_str().to_owned())
        .collect();
    assert_eq!(kinds, declared_kinds);
}

#[tokio::test]
async fn the_chain_verifies_across_a_mixed_sequence_of_authoring_and_publish_records() {
    // The property a chain exists for, over the writers this group added. Every
    // row's digest is recomputed from the columns that stored it and compared to
    // the `row_hash` the writer put there - a test that only asserted "a hash was
    // written" would keep passing after the encoding and the store had stopped
    // agreeing, and a new writer is exactly the occasion for that.
    let h = harness().await;
    let rows = drive_every_audited_path(&h).await;
    assert!(rows.len() >= 7, "the sequence is mixed: {}", rows.len());

    // **Three segments, not one.** D-135 keys a segment on the audited subject's
    // *aggregate*, so each aggregate the driven set touches opens a chain of its own
    // at `seq 0` — which a single walk over every row reads as a gap in the plan's
    // segment. Partitioning is not a workaround for that: the partition **is** the
    // property, and each segment's size is asserted so that a record silently filed
    // on the wrong chain fails here rather than verifying perfectly on it.
    //
    // It was **two** until 2026-08-06 and the partition was binary — the plan's chain
    // against everything else, with "everything else" named `policy_rows`. That held
    // only while the policy was the sole non-plan aggregate: the overlay plane's
    // first audit record landed in the else-branch and the count moved 2 → 3, which
    // is the binary partition failing rather than the overlay misfiling. A partition
    // whose second half is "the rest" cannot say which aggregate a row belongs to,
    // which is the one thing this assertion is for.
    let policy_chain = bss_pricing::infra::storage::repo::audit_repo::policy_chain();
    let overlay_chain =
        bss_pricing::infra::storage::repo::audit_repo::overlay_chain(Uuid::from_u128(0xb_0009));
    let bulk_operation_chain = bss_pricing::infra::storage::repo::audit_repo::bulk_operation_chain(
        Uuid::from_u128(0xb_000b),
    );
    let membership_chain =
        bss_pricing::infra::storage::repo::audit_repo::payer_chain(Uuid::from_u128(0xb_000c));
    let plan_rows: Vec<_> = rows
        .iter()
        .filter(|row| row.chain_id == plan_id().get())
        .cloned()
        .collect();
    let policy_rows: Vec<_> = rows
        .iter()
        .filter(|row| row.chain_id == policy_chain)
        .cloned()
        .collect();
    let overlay_rows: Vec<_> = rows
        .iter()
        .filter(|row| row.chain_id == overlay_chain)
        .cloned()
        .collect();
    let bulk_operation_rows: Vec<_> = rows
        .iter()
        .filter(|row| row.chain_id == bulk_operation_chain)
        .cloned()
        .collect();
    let membership_rows: Vec<_> = rows
        .iter()
        .filter(|row| row.chain_id == membership_chain)
        .cloned()
        .collect();
    // **Four since Slice 11's migration plane**, and the move is the partition
    // working rather than a misfiling. `drive_the_migration_plane` seeds a second
    // plan to migrate *onto* — a migration's target must be a published plan that
    // is not the source — and that plan's own `create` record is a fourth
    // aggregate. The migration record itself is filed on the **source** plan's
    // chain, which is what an auditor asking "what happened to this plan" needs;
    // naming the target's chain here is what keeps "and none to a fifth" true.
    let migration_target_rows = rows
        .iter()
        .filter(|row| row.chain_id == Uuid::from_u128(0x_7a_46_e7))
        .count();
    // **Five since the apply lane's task 3.** `drive_the_bulk_operation_plane`'s
    // record is on the run's own segment — S5 §6's aggregate list names a bulk
    // operation in its own right, for `overlay_chain`'s reason: it is not a plan
    // and has no plan, so filing it on `plan_id().get()` would put two aggregates
    // on one chain.
    //
    // **Six since the customer-group plane's task 4.** `drive_the_membership_
    // plane`'s two records are on the payer's own segment
    // (`audit_repo::payer_chain`) — S5 §6's aggregate list names `payer`, not
    // `membership`, in its own right, for the same reason the run above is not
    // filed on a plan: a membership has no plan and D-135 keys a segment on the
    // audited subject's aggregate.
    assert_eq!(
        plan_rows.len()
            + policy_rows.len()
            + overlay_rows.len()
            + migration_target_rows
            + bulk_operation_rows.len()
            + membership_rows.len(),
        rows.len(),
        "every driven record belongs to one of the six aggregates, and none to a \
         seventh chain nobody named"
    );
    assert_eq!(
        policy_rows.len(),
        3,
        "the proposal and its approval, both on the policy segment - the approve joined the \
         driven set because the window plane below it needs a policy that is actually in force - \
         and `drive_the_taxonomy_plane`'s declaration (D-353): a taxonomy value is a row of the \
         tenant's config object and files on the same singleton segment"
    );
    assert_eq!(
        overlay_rows.len(),
        1,
        "the overlay's create, on the overlay's own segment and not the plan's"
    );
    assert_eq!(
        bulk_operation_rows.len(),
        1,
        "the run's create, on the run's own segment and not the plan's"
    );
    assert_eq!(
        membership_rows.len(),
        2,
        "the enrollment's create and the ending's update, both on the payer's own segment"
    );
    verify_segment(&plan_rows, plan_id().get());
    verify_segment(&policy_rows, policy_chain);
    verify_segment(&overlay_rows, overlay_chain);
    verify_segment(&bulk_operation_rows, bulk_operation_chain);
    verify_segment(&membership_rows, membership_chain);
}

/// Walk one segment link by link and recompute every row's digest.
///
/// Extracted when the driven set grew a second aggregate. It takes the `chain_id`
/// rather than reading it off the first row on purpose: the genesis digest is a
/// function of `(tenant, chain)`, so a helper that derived it from the rows it was
/// checking would verify any segment against itself.
fn verify_segment(rows: &[audit_log::Model], chain_id: Uuid) {
    let mut prev = genesis_prev_hash(TENANT, chain_id);
    for (position, row) in rows.iter().enumerate() {
        assert_eq!(row.chain_id, chain_id, "the segment holds one chain");
        assert_eq!(
            u64::try_from(position).expect("a small position"),
            u64::try_from(row.seq).expect("a non-negative seq"),
            "the segment has no gaps"
        );
        assert_eq!(
            row.prev_hash.as_deref(),
            Some(prev.as_slice()),
            "row {} links to its predecessor",
            row.seq
        );
        let action = AuditAction::ALL
            .iter()
            .copied()
            .find(|candidate| candidate.as_str() == row.action)
            .expect("a declared action");
        let kind = AuditSubjectKind::ALL
            .iter()
            .copied()
            .find(|candidate| candidate.as_str() == row.subject_kind)
            .expect("a declared subject kind");
        let record = AuditRecord {
            tenant_id: row.tenant_id,
            chain_id: row.chain_id,
            seq: u64::try_from(row.seq).expect("a non-negative seq"),
            recorded_at: row.recorded_at,
            actor_principal_id: row.actor_principal_id,
            action,
            subject_kind: kind,
            subject_ref: &row.subject_ref,
            before_state: row.before_state.as_ref(),
            after_state: row.after_state.as_ref(),
            approval_ref: row.approval_ref,
            correlation_id: row.correlation_id,
        };
        let recomputed = audit_row_hash(&record, &prev).expect("recompute the digest");
        assert_eq!(
            recomputed.as_slice(),
            row.row_hash.as_slice(),
            "row {} reproduces its own digest",
            row.seq
        );
        prev = recomputed;
    }
}

/// **Two publish records on one segment, walked link by link.**
///
/// The combination Phase 2 recorded and could not close: it had a link-by-link
/// walk over a segment holding **one** publish record
/// (`the_chain_verifies_across_a_mixed_sequence_of_authoring_and_publish_records`)
/// and a two-publish test that asserted extension without walking
/// (`a_second_publish_extends_every_counter_by_one`). Neither covers a chain
/// that has to stay connected **across** two freezes of one plan, and the second
/// publish is the first writer that appends to a segment a previous publish
/// already extended.
///
/// # This is the connectedness half, and it is deliberately blind to content
///
/// It asserts one property — *the segment is a chain* — and nothing about what
/// any row holds. It does **not** recompute a single digest, and that is the
/// point rather than an omission: `audit_row_hash` over a preimage rebuilt from
/// the stored columns is self-consistent by construction, so a writer that
/// blanked `before_state` and `after_state` would store NULLs, hash NULLs, and
/// agree with itself. The content half is owned by the test above (per-row
/// recompute) and, from an expectation built outside the store,
/// `postgres_audit_chain::every_row_holds_the_record_its_writer_was_handed_and_hashes_to_it`.
/// Letting one test carry both is how a suite ends up unable to tell a broken
/// link from a dropped field, because whichever assertion fires first hides the
/// other.
///
/// The two independent facts it does bring from outside the segment are the
/// genesis seed — computed from the tenant and the chain, never read from a
/// column — and the distinctness of the digests, without which a writer storing
/// one constant `row_hash` everywhere would satisfy an adjacency walk from row 1
/// on.
#[tokio::test]
async fn a_segment_holding_two_publishes_is_one_unbroken_chain() {
    let h = harness().await;

    // The first freeze, under a real approval: the two-person rule is what makes
    // a publish record's `approval_ref` non-null, and a segment whose publishes
    // were both auto-publishable would not be the segment a governed deployment
    // ever holds.
    let (revision, version, _) = seed_publishable(&h).await;
    let first_unit = approve_unit(&h, Uuid::from_u128(0xa_7001)).await;
    h.publish
        .commit(
            &ctx(),
            &h.scope,
            TENANT,
            PlanPublishUnit::plan_content(plan_id(), revision),
            version,
            authorization_of(&first_unit),
            ACTOR,
            CORRELATION,
            at(12),
        )
        .await
        .expect("the first publish commits");

    // The successor, opened and frozen in turn. Opening it is itself an audited
    // mutation, so the two publish records are **not** adjacent - which is
    // exactly why the walk has to be a walk rather than a comparison of the two.
    let opened = h
        .plans
        .open_revision(&h.scope, TENANT, plan_id(), stamp_of(ACTOR, at(13)))
        .await
        .expect("open the successor");
    let second_unit = approve_unit(&h, Uuid::from_u128(0xa_7002)).await;
    h.publish
        .commit(
            &ctx(),
            &h.scope,
            TENANT,
            PlanPublishUnit::plan_content(plan_id(), opened.revision),
            opened.row_version,
            authorization_of(&second_unit),
            ACTOR,
            CORRELATION,
            at(14),
        )
        .await
        .expect("the second publish commits");

    let mut rows = audit_rows(&h).await;
    rows.sort_by_key(|row| row.seq);

    // The world is the one this test claims: one segment, two freezes, and
    // something between them. Without this the walk below could be green over a
    // segment with one publish record, or none.
    let publishes: Vec<i64> = rows
        .iter()
        .filter(|row| row.action == AuditAction::Publish.as_str())
        .map(|row| row.seq)
        .collect();
    assert_eq!(
        publishes.len(),
        2,
        "the segment must hold two publish records for this walk to be the one that was missing: \
         {:?}",
        rows.iter().map(|row| &row.action).collect::<Vec<_>>()
    );
    assert!(
        publishes[1] > publishes[0] + 1,
        "the successor's own `create` record sits between them: {publishes:?}"
    );

    // The walk. Every link, from the genesis seed to the head.
    let genesis = genesis_prev_hash(TENANT, plan_id().get());
    assert_eq!(
        rows[0].prev_hash.as_deref(),
        Some(genesis.as_slice()),
        "the first link is the segment's own seed, computed from the tenant and the chain rather \
         than read from the store"
    );
    for pair in rows.windows(2) {
        assert_eq!(
            pair[1].seq,
            pair[0].seq + 1,
            "the segment's positions are dense; a gap after {} is a link nobody can walk",
            pair[0].seq
        );
        assert_eq!(
            pair[1].prev_hash.as_deref(),
            Some(pair[0].row_hash.as_slice()),
            "row {} does not point at its predecessor's digest",
            pair[1].seq
        );
    }

    // A constant digest would satisfy every link above from row 1 on.
    let distinct: std::collections::BTreeSet<&[u8]> =
        rows.iter().map(|row| row.row_hash.as_slice()).collect();
    assert_eq!(
        distinct.len(),
        rows.len(),
        "two rows share a digest, so the adjacency above proves nothing"
    );
}

// ---------------------------------------------------------------------------
// A same-aggregate contention is tellable from a dead connection (D-159)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_repeat_of_one_publish_is_refused_as_already_enqueued_and_not_as_a_retriable_race() {
    // **The dedup index, and it is the only one of the table's two a single-writer
    // suite can provoke.** Two enqueues of one `PlanPublished` render one
    // `PlanPublished/<plan>/<revision>` key, so `uq_pricing_outbox_dedup_key` is
    // what refuses; `uq_pricing_outbox_sequence` needs two writers reading one head
    // concurrently and has no probe here.
    //
    // That distinction is what this case is for. One `ConcurrentMutation` over both
    // indexes answers *retry* on an act whose every retry renders the same key and
    // collides identically, rolling the caller's transaction back forever. The
    // refusal is deterministic, so it keeps the conflict status and carries a code
    // that says which index refused.
    let h = harness().await;
    let payload = PlanPublishedPayload {
        plan_id: plan_id(),
        revision: 0,
        pending_version_ref: "pend-0".to_owned(),
        price_ids: Vec::new(),
        correlation_id: CORRELATION,
    };

    let (_, first) = h
        .provider
        .db()
        .in_transaction::<u64, bss_pricing::infra::storage::RepoError, _>(|txn| {
            let scope = AccessScope::for_tenant(TENANT);
            let event = NewOutboxEvent::plan_published(TENANT, &payload, at(12));
            Box::pin(async move { outbox_repo::enqueue(txn, &scope, event).await })
        })
        .await;
    first.expect("the first enqueue lands");

    let (_, second) = h
        .provider
        .db()
        .in_transaction::<u64, bss_pricing::infra::storage::RepoError, _>(|txn| {
            let scope = AccessScope::for_tenant(TENANT);
            let event = NewOutboxEvent::plan_published(TENANT, &payload, at(13));
            Box::pin(async move { outbox_repo::enqueue(txn, &scope, event).await })
        })
        .await;
    let refusal = second.expect_err("the second is refused by a unique index");
    let refusal = refusal.into_domain(|infra| {
        bss_pricing::infra::storage::RepoError::Db(format!("outbox transaction: {infra}"))
    });

    assert!(
        matches!(
            refusal,
            bss_pricing::infra::storage::RepoError::OutboxEventAlreadyEnqueued { .. }
        ),
        "the repeat is the dedup index refusing, not a race and not a storage fault: {refusal:?}"
    );
    assert!(
        refusal.to_string().contains(&plan_id().to_string()),
        "and the key it names carries the aggregate: {refusal}"
    );

    // **The wire answer: 409 with its own code, never a 500 and never
    // `CONCURRENT_MUTATION`.** A taken key is the conflict class — where
    // `OPEN_DRAFT_REVISION_EXISTS` and `BUNDLE_EXISTS_ON_PLAN` sit — and not a
    // state machine's refused edge; the code is what says the collision is
    // deterministic, so the status can state the conflict without advising the
    // retry that would collide identically.
    //
    // The code is read off the **typed context** and compared by equality. A
    // containment test over a rendered document is satisfied by the code with a
    // character *appended* — `WINDOW_OVERLAP` passes as `WINDOW_OVERLAPX` under
    // that shape — and over `Debug` of the whole error it is looser still, since
    // any field that happens to quote the reason satisfies it.
    let canonical = toolkit::api::canonical_prelude::CanonicalError::from(
        bss_pricing::infra::storage::repo_failure(&refusal),
    );
    assert_eq!(canonical.status_code(), 409, "{canonical:?}");
    match canonical {
        toolkit::api::canonical_prelude::CanonicalError::Aborted { ctx, .. } => {
            assert_eq!(ctx.reason, "OUTBOX_EVENT_ALREADY_ENQUEUED");
        }
        other => panic!("expected a 409 conflict, got {other:?}"),
    }
}

#[tokio::test]
async fn the_publish_record_carries_the_before_and_after_state_its_transition_implies() {
    // The third before/after writer, and the one the existing suite could not
    // see. Blanking `before_state`/`after_state` to `None` in `infra/publish.rs`
    // left 846/846 green - because the only readers of this pair are the two
    // digest-recompute walks, which rebuild each record's preimage **from the
    // stored columns** and are therefore self-consistent under any substitution.
    // A walk that derives its expectation from the data it is checking cannot
    // catch the data being replaced.
    //
    // So this asserts the CONTENT against values held independently of the row:
    // the version the publish was submitted against, the state the flip produced,
    // and the pending handle. Blank the pair and exactly this test fails.
    let h = harness().await;
    let (revision, version, _) = seed_publishable(&h).await;
    h.publish
        .commit(
            &ctx(),
            &h.scope,
            TENANT,
            PlanPublishUnit::plan_content(plan_id(), revision),
            version,
            PublishAuthorization::auto_publishable(),
            ACTOR,
            CORRELATION,
            at(12),
        )
        .await
        .expect("the publish commits");

    let records = publish_records(&h).await;
    assert_eq!(records.len(), 1);
    let before = records[0]
        .before_state
        .as_ref()
        .expect("a publish records what it moved from");
    let after = records[0]
        .after_state
        .as_ref()
        .expect("and what it moved to");

    // The draft the commit judged, at the version the caller's precondition
    // matched - `version` comes from the seed, not from this row.
    assert_eq!(
        before.get("lifecycleState"),
        Some(&serde_json::json!("draft"))
    );
    assert_eq!(
        before.get("rowVersion"),
        Some(&serde_json::json!(version.get()))
    );

    // The state the flip produced, one version on.
    assert_eq!(
        after.get("lifecycleState"),
        Some(&serde_json::json!("published"))
    );
    assert_eq!(
        after.get("rowVersion"),
        Some(&serde_json::json!(version.get() + 1))
    );

    // The one thing this record carries that no other audit record does: the
    // pending handle. Without it an auditor has the flip and no way to reach the
    // version it landed in, which is the whole reason `subject_state` takes a
    // `pending_ref` at all.
    assert_eq!(
        after.get("pendingVersionRef"),
        Some(&serde_json::json!("pend-0"))
    );
    assert!(
        before.get("pendingVersionRef").is_none(),
        "and the before-state has none: there was no handle before the commit \
         requested one: {before}"
    );
}

// ---------------------------------------------------------------------------
// inst-bc-taxbasis's reverse half, through the real resolution (D-119, D-212)
// ---------------------------------------------------------------------------

/// A bundle on its own plan, at that plan's **current published** revision,
/// composed of this file's plan plus one sibling whose published row carries
/// `tax_inclusive`.
///
/// The whole point of the fixture is that nothing about the bundle is handed to
/// the publish path: it is written to the store, and the resolver has to find it
/// from `component_plan_id` alone.
async fn seed_referencing_bundle(h: &Harness, sibling_tax_inclusive: bool) {
    let bundle_plan = PlanId::new(Uuid::from_u128(0xb0_a1));
    let sibling_plan = PlanId::new(Uuid::from_u128(0xb0_a2));

    let mut draft = new_plan_draft();
    draft.plan_id = bundle_plan;
    let bundle_rev = h
        .plans
        .create_draft(&h.scope, draft)
        .await
        .expect("create the bundle's own plan");

    let mut sibling_draft = new_plan_draft();
    sibling_draft.plan_id = sibling_plan;
    h.plans
        .create_draft(&h.scope, sibling_draft)
        .await
        .expect("create the sibling component's plan");

    // The sibling's published row, on the market this file's plan also prices.
    let sibling_price = Uuid::from_u128(0xb0_a3);
    let mut content = flat_row();
    content.tax_inclusive = sibling_tax_inclusive;
    h.prices
        .create_draft(
            &h.scope,
            TENANT,
            NewPriceDraft {
                price_id: sibling_price,
                scope_key: ScopeKey::new(
                    sibling_plan,
                    CurrencyCode::new("EUR").expect("three letters"),
                    Region::new("eu").expect("a non-blank region"),
                    terminal_phase(),
                    PriceEligibility::AllSubscriptions,
                    ChargeKind::Recurring,
                    Cohort::None,
                )
                .expect("the class pairs with cohort none"),
                content,
                created_by: ACTOR,
                created_at_utc: at(10),
                correlation_id: TEST_CORRELATION,
            },
        )
        .await
        .expect("author the sibling's row");
    common::publish_row_directly(&h.provider, &h.scope, sibling_price).await;

    // A **second** market, on which this file's plan already sells a published
    // row. It is what makes D-119's actual scenario reachable — *"a component
    // **re-publish** whose basis change would mix a referencing bundle's
    // market"* — and it is the only shape in which excluding the publishing
    // plan's own rows from the referent is observable at all: on a first publish
    // the plan has no published row, so the exclusion is a no-op and a probe over
    // it reddens nothing.
    for (owner, price, inclusive) in [
        (plan_id(), Uuid::from_u128(0xb0_a6), false),
        (
            sibling_plan,
            Uuid::from_u128(0xb0_a7),
            sibling_tax_inclusive,
        ),
    ] {
        let mut second = flat_row();
        second.tax_inclusive = inclusive;
        h.prices
            .create_draft(
                &h.scope,
                TENANT,
                NewPriceDraft {
                    price_id: price,
                    scope_key: ScopeKey::new(
                        owner,
                        CurrencyCode::new("USD").expect("three letters"),
                        Region::new("us").expect("a non-blank region"),
                        terminal_phase(),
                        PriceEligibility::AllSubscriptions,
                        ChargeKind::Recurring,
                        Cohort::None,
                    )
                    .expect("the class pairs with cohort none"),
                    content: second,
                    created_by: ACTOR,
                    created_at_utc: at(10),
                    correlation_id: TEST_CORRELATION,
                },
            )
            .await
            .expect("author the second market's row");
        common::publish_row_directly(&h.provider, &h.scope, price).await;
        let conn = h.provider.conn().expect("conn");
        common::schedule_coverage_window(&conn, &h.scope, TENANT, price, stamp()).await;
    }

    // The bundle, and its composition at revision 0 — the revision
    // `plan_repo::load_current` will answer with once the plan is published.
    let bundles = BundleRepo::new(h.provider.clone());
    bundles
        .create(
            &h.scope,
            NewBundle {
                bundle_id: Uuid::from_u128(0xb0_1d),
                tenant_id: TENANT,
                plan_id: bundle_plan,
                price_basis: PriceBasis::SumOfParts,
                invoice_itemization: InvoiceItemization::Aggregate,
            },
            stamp(),
        )
        .await
        .expect("create the bundle");
    bundles
        .replace_composition(
            &h.scope,
            TENANT,
            bundle_plan,
            bundle_rev.revision,
            bundle_rev.row_version,
            CompositionDraft {
                components: vec![
                    BundleComponentDraft {
                        component_plan_id: plan_id().get(),
                        included_sku_id: Uuid::from_u128(0xb0_a4),
                        min_qty: None,
                        max_qty: None,
                    },
                    BundleComponentDraft {
                        component_plan_id: sibling_plan.get(),
                        included_sku_id: Uuid::from_u128(0xb0_a5),
                        min_qty: None,
                        max_qty: None,
                    },
                ],
                rev_share_groups: Vec::new(),
            },
            stamp(),
        )
        .await
        .expect("write the composition");
    common::publish_plan_directly(&h.provider, &h.scope, bundle_plan, bundle_rev.revision).await;
}

#[tokio::test]
async fn a_component_publish_that_would_mix_a_referencing_bundles_market_is_refused() {
    // **The whole chain, not just the rule.** `domain::publish::rules` has its own
    // cases over a handed-in market set; this one asserts the half that finds it —
    // the resolver reaches `pricing_bundle_component` by `component_plan_id`,
    // resolves the bundle's current published revision, reads the *other*
    // components' rows and hands the basis to the pipeline. Nothing about the
    // bundle is passed to the publish path.
    let h = harness().await;
    let (_revision, _tag, _price) = seed_publishable(&h).await;
    seed_referencing_bundle(&h, true).await;

    let report = h
        .publish
        .precheck(&h.scope, TENANT, plan_id(), at(11))
        .await
        .expect("the precheck runs");

    let codes: Vec<String> = report
        .violations
        .iter()
        .map(|violation| violation.code.clone())
        .collect();
    assert!(
        codes.contains(&"BUNDLE_TAX_BASIS_MIXED".to_owned()),
        "this plan's rows are tax_exclusive and the bundle's market is inclusive: {codes:?}"
    );
    // **Named by market, not merely counted.** The `(USD, us)` one is the market
    // this plan *already* sells in, so it is the only one whose referent could be
    // this plan's own published row — asserting it by name is what makes
    // excluding those rows from the referent an observable property rather than a
    // claim in a doc comment.
    let subjects: Vec<String> = report
        .violations
        .iter()
        .filter(|violation| violation.code == "BUNDLE_TAX_BASIS_MIXED")
        .map(|violation| violation.subject.clone())
        .collect();
    assert!(
        subjects.iter().any(|subject| subject.contains("USD/us")),
        "the market this plan already sells in must be named: {subjects:?}"
    );
}

#[tokio::test]
async fn a_component_publish_agreeing_with_the_bundles_market_is_not_refused() {
    // The control, and it is what keeps the case above from passing against a
    // resolver that reports every referencing market as a conflict.
    let h = harness().await;
    seed_publishable(&h).await;
    seed_referencing_bundle(&h, false).await;

    let report = h
        .publish
        .precheck(&h.scope, TENANT, plan_id(), at(11))
        .await
        .expect("the precheck runs");

    let codes: Vec<String> = report
        .violations
        .iter()
        .map(|violation| violation.code.clone())
        .collect();
    assert!(
        !codes.contains(&"BUNDLE_TAX_BASIS_MIXED".to_owned()),
        "an agreeing basis is not a mix: {codes:?}"
    );
}

// ---------------------------------------------------------------------------
// The commit's rule run reports (`T-17`, §7/§10).
// ---------------------------------------------------------------------------

/// **The commit reports, and it is the run whose reporting matters most.**
///
/// The publish route's approved arm reaches `commit` **without** running a
/// pre-check at all — the pre-check belongs to the submit arm — and a plan that
/// failed its pre-check is never approved. So a finding raised by the commit's
/// rule run is one that appeared between the reviewer's decision and the commit:
/// exactly the kind an operator cannot see coming, and for a while the only kind
/// this gear did not count.
///
/// Staged with a tax-inclusive row, whose §7 Info alarm the derivation raises off
/// the same candidate set. `precheck` is deliberately never called, so the count
/// asserted here can only have come from the commit — and the commit **refuses**,
/// which is the sharper half: the report is made about what was judged, not only
/// about what went on to publish.
#[tokio::test]
async fn the_commits_rule_run_reports_what_it_judged() {
    let h = harness().await;
    let (revision, version, _) = seed_publishable(&h).await;

    // A second row on its own canonical key, tax-inclusive — so the candidate
    // set gates a market and §7's alarm has something to say about it.
    let gated = Uuid::from_u128(0xb_0003);
    h.prices
        .create_draft(
            &h.scope,
            TENANT,
            NewPriceDraft {
                price_id: gated,
                scope_key: scope_key(PriceEligibility::NewSubscriptionsOnly),
                content: PriceContent {
                    tax_inclusive: true,
                    tax_category_ref: Some("standard".to_owned()),
                    ..flat_row()
                },
                created_by: ACTOR,
                created_at_utc: at(11),
                correlation_id: TEST_CORRELATION,
            },
        )
        .await
        .expect("author the tax-inclusive row");
    common::schedule_coverage_window(
        &h.provider.conn().expect("conn"),
        &h.scope,
        TENANT,
        gated,
        stamp(),
    )
    .await;

    h.metrics.force_flush();
    assert_eq!(
        h.metrics.counter_value(
            "pricing_alarm_total",
            &[("alarm", "pricing.tax.not_sellable_ga_active")]
        ),
        0,
        "nothing has judged this subject yet"
    );

    let refusal = h
        .publish
        .commit(
            &ctx(),
            &h.scope,
            TENANT,
            PlanPublishUnit::plan_content(plan_id(), revision),
            version,
            PublishAuthorization::auto_publishable(),
            ACTOR,
            CORRELATION,
            at(12),
        )
        .await
        .expect_err("the mixed basis is refused, and that is the path under test");

    // The refusal is the staged one and not something standing in for it: a
    // tax-inclusive row beside a tax-exclusive one on `EUR/eu` is D-110's mixed
    // market. **The reporting happens anyway**, and that is the claim — the
    // derivation runs on the judged candidate set, before the publishability
    // verdict decides whether the transaction lives.
    match &refusal {
        DomainError::ValidationFailed(report) => {
            let codes: Vec<&str> = report.violations.iter().map(|v| v.code.as_str()).collect();
            assert_eq!(codes, ["TAX_BASIS_MIXED_MARKET"]);
        }
        other => panic!("expected a validation report, got {other:?}"),
    }

    h.metrics.force_flush();
    assert_eq!(
        h.metrics.counter_value(
            "pricing_alarm_total",
            &[("alarm", "pricing.tax.not_sellable_ga_active")]
        ),
        1,
        "the commit's rule run reported, with no pre-check anywhere in this case"
    );
}

/// **A self-referential composite is refused by the real publish path** — D-257,
/// and the case exists because the rule passing in isolation proved nothing.
///
/// `CompositeArity` and `CompositeSelfReference` were registered, unit-tested and
/// green while `assemble_from` never populated `PlanShape::composites`, so both
/// iterated an empty vec on every publish and the v11 content pin framed a count
/// of zero. A one-constituent or `vm → pod → vm` revision published unrejected,
/// and a formula edit moved no digest — the exact hole D-256 claimed to close.
///
/// That is D-254's defect class a second time: a rule with an operand nobody
/// loads is a rule that always passes. So this asserts through `precheck`, which
/// runs the same assembly a commit does, rather than through a hand-built shape.
#[tokio::test]
async fn a_self_referential_composite_is_refused_by_the_publish_path() {
    let h = harness().await;
    let (_revision, _version, _) = seed_publishable(&h).await;

    let clean = h
        .publish
        .precheck(&h.scope, TENANT, plan_id(), at(11))
        .await
        .expect("the pre-check runs");
    assert!(clean.is_publishable(), "the subject was publishable first");

    // `vm` is built from `pod`, and `pod` from `vm`. Neither definition is
    // self-referential alone; the cycle exists only across the pair.
    let current = h
        .plans
        .find_open_draft(&h.scope, TENANT, plan_id())
        .await
        .expect("read the draft")
        .expect("there is one");
    h.shapes
        .replace_composites(
            &h.scope,
            TENANT,
            plan_id(),
            current.revision,
            current.row_version,
            vec![
                bss_pricing::domain::plan_shape::CompositeMeter {
                    composite_id: Uuid::from_u128(0xc0_f1),
                    output_unit: "vm-hour".to_owned(),
                    constituent_units: vec!["vcpu-hour".to_owned(), "pod-hour".to_owned()],
                    formula: serde_json::json!({ "op": "weighted_sum" }),
                },
                bss_pricing::domain::plan_shape::CompositeMeter {
                    composite_id: Uuid::from_u128(0xc0_f2),
                    output_unit: "pod-hour".to_owned(),
                    constituent_units: vec!["ram-gb-hour".to_owned(), "vm-hour".to_owned()],
                    formula: serde_json::json!({ "op": "weighted_sum" }),
                },
            ],
            stamp_of(ACTOR, at(11)),
        )
        .await
        .expect("author the cycle on the open draft");

    let report = h
        .publish
        .precheck(&h.scope, TENANT, plan_id(), at(11))
        .await
        .expect("the pre-check runs");
    assert!(
        !report.is_publishable(),
        "a transitive composite cycle must not publish: {report:?}"
    );
    assert!(
        report
            .violations
            .iter()
            .any(|v| v.code == "COMPOSITE_SELF_REFERENCE"),
        "and it must say which rule refused: {report:?}"
    );
}

/// The trial half of the two-phase chain the grant-set case needs.
fn trial_phase() -> PhaseId {
    PhaseId::new(Uuid::from_u128(0xfa_51))
}

/// The seed's canonical scope key moved onto another phase.
fn scope_key_in_phase(phase: PhaseId) -> ScopeKey {
    ScopeKey::new(
        plan_id(),
        CurrencyCode::new("EUR").expect("three letters"),
        Region::new("eu").expect("a non-blank region"),
        phase,
        PriceEligibility::AllSubscriptions,
        ChargeKind::Recurring,
        Cohort::None,
    )
    .expect("the class pairs with cohort none")
}

/// Give the seeded plan a real two-phase schedule, covered in its one market.
///
/// The seed is D-19's single implicit terminal phase, which makes it
/// **non-phased** — and `GrantSetPhasesKnown` answers a non-phased plan from its
/// first arm, before it ever walks the authored keys. Reaching the arm that
/// judges an *unknown* key needs a schedule of two, and `PhaseCoverage` then
/// needs a recurring row in every market for both of them or it refuses the plan
/// for a reason the case is not about.
///
/// The terminal phase keeps the seed's id so the seeded row still covers it.
async fn make_two_phased(h: &Harness) {
    let current = h
        .plans
        .find_open_draft(&h.scope, TENANT, plan_id())
        .await
        .expect("read the draft")
        .expect("there is one");
    h.shapes
        .replace_phases(
            &h.scope,
            TENANT,
            plan_id(),
            current.revision,
            current.row_version,
            vec![
                PlanPhase {
                    phase_id: trial_phase(),
                    kind: PhaseKind::Trial,
                    display_name: None,
                    ordinal: 0,
                    converts_to_phase_id: Some(terminal_phase()),
                    phase_duration_days: Some(14),
                    display_trial_days: Some(14),
                },
                PlanPhase {
                    phase_id: terminal_phase(),
                    kind: PhaseKind::Evergreen,
                    display_name: None,
                    ordinal: 1,
                    converts_to_phase_id: None,
                    phase_duration_days: None,
                    display_trial_days: None,
                },
            ],
            stamp(),
        )
        .await
        .expect("attach the two-phase chain");

    let trial_price = Uuid::from_u128(0xb_0002);
    h.prices
        .create_draft(
            &h.scope,
            TENANT,
            NewPriceDraft {
                price_id: trial_price,
                scope_key: scope_key_in_phase(trial_phase()),
                content: flat_row(),
                created_by: ACTOR,
                created_at_utc: at(10),
                correlation_id: TEST_CORRELATION,
            },
        )
        .await
        .expect("author the trial-phase row");
    let conn = h.provider.conn().expect("conn");
    common::schedule_coverage_window(&conn, &h.scope, TENANT, trial_price, stamp()).await;
}

/// **A per-phase grant set keyed to a phase the schedule does not have is refused
/// by the real publish path** — D-258's first owed case.
///
/// `GrantSetPhasesKnown` was registered, unit-tested and green while
/// `assemble_from` never copied `entitlement_grants` off the draft. The shape's
/// grant set was `EntitlementGrants::default()` on **every** publish, so
/// `per_phase` was always empty and the rule returned on its first line — and the
/// content pin framed that same default, leaving open the "approved a trial
/// capped at 20, published 20 000, with an equal digest" hole `content_pin`'s own
/// doc claims to close.
///
/// D-258 landed the correction gated but **untested** and said so rather than
/// hiding it; this is the case it owed. Asserted through `precheck`, which runs
/// the same assembly a commit does, because the defect is a field the assembly
/// never filled and no test that hand-builds a `PlanShape` can see one.
///
/// The clean pre-check ahead of the patch is load-bearing twice over: it proves
/// the subject was publishable before the grant set was authored, and — on a
/// schedule of two, in a market `PhaseCoverage` walks — that the phase set
/// reached the shape too, so the refusal below is the unknown-key arm and not
/// the non-phased one wearing the same code.
#[tokio::test]
async fn a_grant_set_naming_an_unknown_phase_is_refused_by_the_publish_path() {
    let h = harness().await;
    let (_revision, _version, _) = seed_publishable(&h).await;
    make_two_phased(&h).await;

    let clean = h
        .publish
        .precheck(&h.scope, TENANT, plan_id(), at(11))
        .await
        .expect("the pre-check runs");
    assert!(
        clean.is_publishable(),
        "the two-phase subject was publishable first: {clean:?}"
    );

    // A key that names no phase of the schedule. The set is non-empty because an
    // absent grant set is stored as `NULL` and would never read back.
    let stranger = Uuid::from_u128(0x_9051);
    let current = h
        .plans
        .find_open_draft(&h.scope, TENANT, plan_id())
        .await
        .expect("read the draft")
        .expect("there is one");
    h.plans
        .update_draft(
            &h.scope,
            TENANT,
            plan_id(),
            current.revision,
            current.row_version,
            PlanShapePatch {
                entitlement_grants: Some(EntitlementGrants {
                    plan_tier_ref: None,
                    plan_level: GrantSet::default(),
                    per_phase: std::collections::BTreeMap::from([(
                        stranger,
                        GrantSet {
                            feature_flags: std::collections::BTreeMap::from([(
                                "bss.pricing/api-access".to_owned(),
                                true,
                            )]),
                            quotas: std::collections::BTreeMap::new(),
                        },
                    )]),
                }),
                ..PlanShapePatch::default()
            },
            stamp(),
        )
        .await
        .expect("author the grant set on the open draft");

    let report = h
        .publish
        .precheck(&h.scope, TENANT, plan_id(), at(11))
        .await
        .expect("the pre-check runs");
    assert!(
        !report.is_publishable(),
        "a grant set keyed to no phase of the schedule must not publish: {report:?}"
    );
    assert!(
        report
            .violations
            .iter()
            .any(|v| v.code == "GRANT_SET_PHASE_UNKNOWN"),
        "and it must say which rule refused: {report:?}"
    );
}

/// **A ranked plan that another published plan points at still publishes** —
/// D-258's second owed case, and the one that was a live wrong answer rather
/// than a silent gap.
///
/// D-54's reverse guard reads the subject's `comparability_rank` off the shape
/// and the inbound edges off the store, independently. While `assemble_from`
/// never copied `change_contract`, the shape's rank was `None` on every publish
/// while the edges were real — so `COMPARABILITY_RANK_REVOKED` fired against
/// **any** plan another published plan named, however carefully that plan had
/// authored its rank. An operator was refused for a rule they satisfied, and
/// their remediation — publish a rank — could not work.
///
/// So this case asserts a **publish**, not a refusal. That is the awkward shape
/// and it is the necessary one: the defect made a passing subject fail, and only
/// a green subject with a real inbound edge can see it.
#[tokio::test]
async fn a_ranked_plan_another_published_plan_points_at_still_publishes() {
    let h = harness().await;
    let (_revision, _version, _) = seed_publishable(&h).await;

    // The referencing plan. It is published straight at the table because what
    // this case needs of it is one stored fact — a published edge naming the
    // subject — and driving it through the engine would make the case depend on
    // a second plan being publishable in its own right.
    let referrer = PlanId::new(Uuid::from_u128(0x9_1a5));
    let other = h
        .plans
        .create_draft(
            &h.scope,
            NewPlanDraft {
                plan_id: referrer,
                ..new_plan_draft()
            },
        )
        .await
        .expect("create the referring draft");
    h.plans
        .update_draft(
            &h.scope,
            TENANT,
            referrer,
            other.revision,
            other.row_version,
            PlanShapePatch {
                change_contract: Some(PlanChangeContract {
                    allowed_change_targets: Some(vec![plan_id().get()]),
                    comparability_rank: Some(20),
                    usage_counter_on_plan_change: UsageCounterOnPlanChange::default(),
                }),
                ..PlanShapePatch::default()
            },
            stamp(),
        )
        .await
        .expect("author the edge pointing at the subject");
    common::publish_plan_directly(&h.provider, &h.scope, referrer, other.revision).await;

    // The subject carries a rank and no edges of its own: K4 asks a rank of it
    // for the inbound edges alone, which is exactly D-54's guard.
    let current = h
        .plans
        .find_open_draft(&h.scope, TENANT, plan_id())
        .await
        .expect("read the draft")
        .expect("there is one");
    h.plans
        .update_draft(
            &h.scope,
            TENANT,
            plan_id(),
            current.revision,
            current.row_version,
            PlanShapePatch {
                change_contract: Some(PlanChangeContract {
                    allowed_change_targets: None,
                    comparability_rank: Some(10),
                    usage_counter_on_plan_change: UsageCounterOnPlanChange::default(),
                }),
                ..PlanShapePatch::default()
            },
            stamp(),
        )
        .await
        .expect("author the subject's rank");

    let report = h
        .publish
        .precheck(&h.scope, TENANT, plan_id(), at(11))
        .await
        .expect("the pre-check runs");
    assert!(
        !report
            .violations
            .iter()
            .any(|v| v.code == "COMPARABILITY_RANK_REVOKED"),
        "the subject publishes a rank, so D-54's reverse guard has nothing to \
         report: {report:?}"
    );
    assert!(
        report.is_publishable(),
        "and the plan publishes: {report:?}"
    );
}

/// **A period bound on a market the plan does not sell is refused by the real
/// publish path** (D-319) — and the case goes through `precheck` for the reason
/// the composite case above does.
///
/// `PeriodFloorCapMarketSold`'s operand is `PlanShape::period_floor_caps`, which
/// `assemble_from` has to load; a rule whose operand nobody loads is a rule that
/// always passes, and this function's neighbours record that landing three times
/// (D-254, D-257, D-258). A hand-built `PlanShape` cannot see the missing line,
/// so the assertion is made against the shape a commit would actually judge.
///
/// The positive control is deliberately the **same amount** on a market the plan
/// does sell: what separates the two subjects is one axis value, so a rule that
/// refused every bound would fail the control, and a rule that refused none
/// would fail the refusal.
#[tokio::test]
async fn a_period_bound_on_an_unsold_market_is_refused_by_the_publish_path() {
    let h = harness().await;
    let (_revision, _version, _) = seed_publishable(&h).await;

    let clean = h
        .publish
        .precheck(&h.scope, TENANT, plan_id(), at(11))
        .await
        .expect("the pre-check runs");
    assert!(clean.is_publishable(), "the subject was publishable first");

    let current = h
        .plans
        .find_open_draft(&h.scope, TENANT, plan_id())
        .await
        .expect("read the draft")
        .expect("there is one");

    // The seeded plan prices `(EUR, eu)`. This is the same amount one region
    // over — the shape of the typo the rule exists to catch.
    let after_bad = h
        .shapes
        .replace_period_floor_caps(
            &h.scope,
            TENANT,
            plan_id(),
            current.revision,
            current.row_version,
            vec![bss_pricing::domain::plan_shape::PeriodFloorCap {
                currency: CurrencyCode::new("EUR").expect("three letters"),
                region: Region::new("us").expect("a non-blank region"),
                floor_minor: Some(MinorAmount::new(50_000).expect("non-negative")),
                cap_minor: None,
            }],
            stamp_of(ACTOR, at(11)),
        )
        .await
        .expect("author the bound on the open draft");

    let report = h
        .publish
        .precheck(&h.scope, TENANT, plan_id(), at(11))
        .await
        .expect("the pre-check runs");
    assert!(
        !report.is_publishable(),
        "a bound on a market the plan prices nothing in must not publish: {report:?}"
    );
    assert!(
        report
            .violations
            .iter()
            .any(|v| v.code == "PERIOD_FLOOR_CAP_MARKET_UNSOLD"),
        "and it must say which rule refused: {report:?}"
    );

    // The control: the same amount on the market the plan actually sells.
    h.shapes
        .replace_period_floor_caps(
            &h.scope,
            TENANT,
            plan_id(),
            current.revision,
            after_bad.row_version,
            vec![bss_pricing::domain::plan_shape::PeriodFloorCap {
                currency: CurrencyCode::new("EUR").expect("three letters"),
                region: Region::new("eu").expect("a non-blank region"),
                floor_minor: Some(MinorAmount::new(50_000).expect("non-negative")),
                cap_minor: None,
            }],
            stamp_of(ACTOR, at(11)),
        )
        .await
        .expect("move the bound onto a sold market");

    let fixed = h
        .publish
        .precheck(&h.scope, TENANT, plan_id(), at(11))
        .await
        .expect("the pre-check runs");
    assert!(
        fixed.is_publishable(),
        "the same bound on a sold market is the whole point of the field: {fixed:?}"
    );
}
