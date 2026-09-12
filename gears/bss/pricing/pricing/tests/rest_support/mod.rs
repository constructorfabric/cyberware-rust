//! The harness every authoring-route suite drives the real router with.
//!
//! It is shared for the reason `tests/common/mod.rs` is: none of it is
//! per-suite. A migrated in-memory `SQLite`, the four PDP doubles the gate can
//! meet, the seeded plan states, and the two readbacks an assertion needs — "did
//! the row move" and "what tag did the response carry". Each suite that copied
//! them would be a second description of the same gate, free to become a weaker
//! one.
//!
//! **The PDP doubles are the point of most of this file.** A suite that only
//! ever drove an allowing resolver would prove the happy path and nothing about
//! the gate; the denying, the erroring and the unconstrained ones are what make
//! "fail closed" observable.

#![allow(
    dead_code,
    reason = "each test binary compiles the whole module and uses part of it"
)]
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use toolkit_canonical_errors::CanonicalError;

use async_trait::async_trait;
use authz_resolver_sdk::constraints::{Constraint, InPredicate, Predicate};
use authz_resolver_sdk::models::{
    DenyReason, EvaluationRequest, EvaluationResponse, EvaluationResponseContext,
};
use authz_resolver_sdk::{AuthZResolverApi, PolicyEnforcer};
use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, Response};
use bss_fixtures::ModelKind;
use bss_pricing::api::rest::audit::ApiState as AuditState;
use bss_pricing::api::rest::frontier::ApiState as FrontierState;
use bss_pricing::api::rest::history::ApiState as HistoryState;
use bss_pricing::api::rest::state::{AuthoringState, GovernanceState};
use bss_pricing::config::LimitsConfig;
use bss_pricing::domain::concurrency::RowVersion;
use bss_pricing::domain::contracts::{
    AnchorDay, BillingAnchorPolicy, ProrationBasis, ProrationContract,
};
use bss_pricing::domain::instant::utc_ymd_hms;
use bss_pricing::domain::lifecycle::LifecycleState;
use bss_pricing::domain::money::CurrencyCode;
use bss_pricing::domain::money::{MinorAmount, RateMinor};
use bss_pricing::domain::plan::PlanRevision;
use bss_pricing::domain::plan_shape::Frequency;
use bss_pricing::domain::plan_shape::{
    AddonRule, BillingCycle, DescriptorSet, PhaseKind, PlanPhase,
};
use bss_pricing::domain::ports::metrics::PricingMetricsPort;
use bss_pricing::domain::price_record::PriceContent as PriceContentAlias;
use bss_pricing::domain::price_record::{PriceContent, PriceRecord};
use bss_pricing::domain::price_row::{
    AggregationFunction, AggregationGranularity, BillingGranularity, MinQtyUsageFallback, PriceRow,
    QuantitySource, ReservationFlavor, TierAggregationWindow, TierBand,
};
use bss_pricing::domain::scope_key::{
    ChargeKind, Cohort, DimensionKey, Meter, PhaseId, PlanId, PriceEligibility, Region, ScopeKey,
};
use bss_pricing::infra::approval::ApprovalService;
use bss_pricing::infra::fixture_gate::FixtureGate;
use bss_pricing::infra::metrics::test_harness::MetricsHarness;
use bss_pricing::infra::publish::PublishService;
use bss_pricing::infra::storage::entity::{
    approval, audit_log, catalog_version_ref, group_membership, outbox, plan,
};
use bss_pricing::infra::storage::migrations::Migrator;
use bss_pricing::infra::storage::repo::approval_repo::ApprovalRecord;
use bss_pricing::infra::storage::repo::{
    IdempotencyGate, NewPlanDraft, NewPriceDraft, PinFrontierRepo, PlanRepo, PlanShapeRepo,
    PriceRepo, plan_repo, price_repo,
};
use bss_pricing::infra::window::WindowService;
use bss_pricing_sdk::catalog_version::CatalogVersion;
use bss_pricing_sdk::catalog_version_registry::{CatalogVersionRegistryV1, PendingVersionRef};
use time::OffsetDateTime;

use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, EntityTrait, Order};
use sea_orm_migration::MigratorTrait;
use toolkit::api::OpenApiRegistryImpl;
use toolkit_db::migration_runner::run_migrations_for_testing;
use toolkit_db::secure::{AccessScope, SecureEntityExt, SecureUpdateExt};
use toolkit_db::{ConnectOpts, DBProvider, DbError, connect_db};
use toolkit_gts::gts_id;
use toolkit_security::{PlatformSecurityContext, SecurityContext, pep_properties};
use tower::ServiceExt;
use uuid::Uuid;

/// The D-48 category a seeded publish resolves.
///
/// A value, not a shape: no suite asserts *which* category a seeded row froze —
/// the resolution itself is asserted where it is the subject, in `rest_preview`
/// and the tax-display suites, which build their own readiness.
const FIXTURE_TAX_CATEGORY: &str = "standard";

/// The readiness a fixture publishes against: the row's own region, declaring a
/// default category.
///
/// **This was `RegionTaxReadiness::empty()` until 2026-08-20, and the comment
/// justifying it said a NULL `resolved_tax_category` "is exactly what a row stating
/// no category in a region declaring no default should carry".** That sentence was
/// false: `TaxBasisComplete` refuses exactly such a row, `pricing_price`'s migration header
/// calls a NULL on a published row impossible, and `trg_pricing_price_append_only` makes it
/// unrepairable. `price_repo::publish_rows` did not enforce the category half, so
/// every fixture-published row in the crate quietly carried the fault — and when
/// H14 of the 2026-08-19 review moved that refusal into the store, this seeder was
/// its first violator.
///
/// **Repaired rather than exempted.** A fixture carrying the fault a new rule judges
/// is what the rule exists to catch; teaching the rule to skip the seeder would have
/// left every suite in the crate publishing rows the ordinary door refuses.
///
/// Built from the row being published rather than from a fixed region list, because
/// the suites author regions freely (`eu`, `EU`, `us`, `apac`, `jp`, `mars`, `K0`…)
/// and a list would fail closed on the next one somebody invents.
///
/// `tax_rate_present` is `true` and is not load-bearing here: `publish_rows` reads
/// only `tax_category` off these markers. The **rate** arm of `inst-td-policy` is
/// `TaxBasisComplete`'s and runs on the publish rule set, which this seeder
/// deliberately bypasses; see [`Harness::publish`].
fn fixture_readiness(region: &str) -> bss_pricing::domain::tax_display::RegionTaxReadiness {
    bss_pricing::domain::tax_display::RegionTaxReadiness::new(BTreeMap::from([(
        region.to_owned(),
        bss_pricing::domain::tax_display::RegionReadiness {
            tax_category: Some(FIXTURE_TAX_CATEGORY.to_owned()),
            tax_rate_present: true,
        },
    )]))
}

/// One value for a whole test binary: these suites drive a repository or a
/// service directly, where the value the HTTP edge would have established has
/// no producer. What each suite asserts *about* it is stated where it asserts
/// it.
const TEST_CORRELATION: uuid::Uuid = uuid::Uuid::from_u128(0x_c0_11_a7_10);

/// The stamp an audited repository call is made under: who acted, when, and the
/// request's correlation.
///
/// Public, so a suite driving `ApprovalService` directly can open a unit as a
/// **named submitter**: the submitter *is* the actor of the open (`NewApproval`'s
/// doc), and `seed_stamp`'s `SEED_ACTOR` would make submitter and approver the same
/// principal wherever the approver is a third identity — which `inst-tp-distinct`
/// refuses on identity rather than on role.
pub fn stamp_of(actor: uuid::Uuid, when: OffsetDateTime) -> bss_pricing::domain::audit::AuditStamp {
    bss_pricing::domain::audit::AuditStamp {
        actor_principal_id: actor,
        recorded_at: when,
        correlation_id: TEST_CORRELATION,
    }
}

/// The response header the entity tag rides on, spelled once for every suite.
pub const PLAN_TAG_HEADER: &str = "etag";

/// The actor every seeded row is authored by.
pub const SEED_ACTOR: Uuid = Uuid::from_u128(0xac70);

// ---------------------------------------------------------------------------
// PDP doubles.
// ---------------------------------------------------------------------------

/// Allows, and constrains `owner_tenant_id` to `allowed` — the flat `In` shape
/// the real PDP returns for this gear, since the PEP advertises no tenant-subtree
/// capability and the subtree is pre-expanded.
pub struct FlatInResolver {
    pub allowed: Vec<Uuid>,
}

#[async_trait]
impl AuthZResolverApi for FlatInResolver {
    async fn evaluate(
        &self,
        _ctx: PlatformSecurityContext,
        _req: EvaluationRequest,
    ) -> Result<EvaluationResponse, CanonicalError> {
        Ok(EvaluationResponse {
            decision: true,
            context: EvaluationResponseContext {
                constraints: vec![Constraint {
                    predicates: vec![Predicate::In(InPredicate::new(
                        pep_properties::OWNER_TENANT_ID,
                        self.allowed.clone(),
                    ))],
                }],
                deny_reason: None,
            },
        })
    }
}

/// Refuses. Every gated route must surface this as 403 and must not have
/// touched a row on the way there.
pub struct DenyingResolver;

#[async_trait]
impl AuthZResolverApi for DenyingResolver {
    async fn evaluate(
        &self,
        _ctx: PlatformSecurityContext,
        _req: EvaluationRequest,
    ) -> Result<EvaluationResponse, CanonicalError> {
        Ok(EvaluationResponse {
            decision: false,
            context: EvaluationResponseContext {
                constraints: vec![],
                deny_reason: Some(DenyReason {
                    error_code: "no_catalog_role".to_owned(),
                    details: Some("no catalog role grants this pair".to_owned()),
                }),
            },
        })
    }
}

/// Cannot answer. A PDP outage must fail **closed** (503), never degrade into
/// an allow.
pub struct UnavailableResolver;

#[async_trait]
impl AuthZResolverApi for UnavailableResolver {
    async fn evaluate(
        &self,
        _ctx: PlatformSecurityContext,
        _req: EvaluationRequest,
    ) -> Result<EvaluationResponse, CanonicalError> {
        Err(CanonicalError::service_unavailable()
            .with_detail("the policy decision point is unreachable")
            .create())
    }
}

/// Allows with **no** constraints. `require_constraints = true` must refuse it:
/// an unconstrained allow compiles to a scope that filters nothing, which is
/// every tenant's price book.
pub struct UnconstrainedResolver;

#[async_trait]
impl AuthZResolverApi for UnconstrainedResolver {
    async fn evaluate(
        &self,
        _ctx: PlatformSecurityContext,
        _req: EvaluationRequest,
    ) -> Result<EvaluationResponse, CanonicalError> {
        Ok(EvaluationResponse {
            decision: true,
            context: EvaluationResponseContext {
                constraints: vec![],
                deny_reason: None,
            },
        })
    }
}

/// Allows like [`FlatInResolver`], and **records every request** so a suite can
/// assert which `(resource_type, action)` pair a route actually asked about.
///
/// An allow/deny test cannot catch a route gated on the wrong pair; this can.
pub struct RecordingResolver {
    pub allowed: Vec<Uuid>,
    pub seen: Arc<Mutex<Vec<EvaluationRequest>>>,
}

#[async_trait]
impl AuthZResolverApi for RecordingResolver {
    async fn evaluate(
        &self,
        _ctx: PlatformSecurityContext,
        req: EvaluationRequest,
    ) -> Result<EvaluationResponse, CanonicalError> {
        self.seen.lock().expect("recorder").push(req);
        Ok(EvaluationResponse {
            decision: true,
            context: EvaluationResponseContext {
                constraints: vec![Constraint {
                    predicates: vec![Predicate::In(InPredicate::new(
                        pep_properties::OWNER_TENANT_ID,
                        self.allowed.clone(),
                    ))],
                }],
                deny_reason: None,
            },
        })
    }
}

/// Allows exactly the `(resource_type, action)` pairs named, denies every other
/// one — the fixture the separate-route claim needs and no allow/deny fixture
/// above it can provide.
///
/// Every other double in this file answers the same decision for whatever it
/// was asked; a suite proving that `config x write` does not imply
/// `customer_group x write` needs a resolver whose answer actually depends on
/// which pair it was asked about, which is exactly what a route gated on the
/// wrong label is invisible to (`rest_authz.rs`'s own module doc, one clause
/// over).
pub struct SelectiveResolver {
    pub allowed_tenant: Uuid,
    pub allowed_pairs: Vec<(&'static str, &'static str)>,
    /// Pairs whose allow compiles to a tenant **other** than
    /// [`Self::allowed_tenant`], for the shape a single tenant cannot stage: a
    /// principal holding one authority here and a different authority somewhere
    /// else. Every route that asks two PDP questions can only be told apart from
    /// one that asks one when the two answers can disagree, and until this field
    /// existed no fixture in the crate could make them.
    pub pair_tenants: Vec<((&'static str, &'static str), Uuid)>,
}

#[async_trait]
impl AuthZResolverApi for SelectiveResolver {
    async fn evaluate(
        &self,
        _ctx: PlatformSecurityContext,
        req: EvaluationRequest,
    ) -> Result<EvaluationResponse, CanonicalError> {
        let asked = (
            req.resource.resource_type.as_str(),
            req.action.name.as_str(),
        );
        if !self.allowed_pairs.contains(&asked) {
            return Ok(EvaluationResponse {
                decision: false,
                context: EvaluationResponseContext {
                    constraints: vec![],
                    deny_reason: Some(DenyReason {
                        error_code: "no_catalog_role".to_owned(),
                        details: Some(format!("no catalog role grants {}x{}", asked.0, asked.1)),
                    }),
                },
            });
        }
        let tenant = self
            .pair_tenants
            .iter()
            .find(|(pair, _)| *pair == asked)
            .map_or(self.allowed_tenant, |(_, tenant)| *tenant);
        Ok(EvaluationResponse {
            decision: true,
            context: EvaluationResponseContext {
                constraints: vec![Constraint {
                    predicates: vec![Predicate::In(InPredicate::new(
                        pep_properties::OWNER_TENANT_ID,
                        vec![tenant],
                    ))],
                }],
                deny_reason: None,
            },
        })
    }
}

// ---------------------------------------------------------------------------
// The harness.
// ---------------------------------------------------------------------------

/// The `CatalogVersion` registry double, hoisted here because two route suites
/// need it.
///
/// One pending handle per `request_id`, idempotently: the commit derives its
/// `request_id` from `(tenant, plan, revision)` so a rolled-back-and-retried
/// publish re-requests the same assignment, and a double that minted a fresh
/// handle per call would let that property pass untested. It lives in the test
/// crate and must never move into `src/` — the single failure the registry port
/// exists to prevent is a second version incrementer shipping.
/// It also **answers commits**, which is what lets a route suite drive the projector
/// over a handle a mutation requested. `committed_version` answered `None`
/// unconditionally until D-99's re-projection half needed asserting: a pending ref
/// nothing can resolve is a ref the warm sweep skips forever, so the delta a window
/// mutation produces was unobservable and the write that carries it could be deleted
/// with the whole suite green.
#[derive(Default)]
pub struct RegistryDouble {
    issued: Mutex<HashMap<String, String>>,
    commits: Mutex<HashMap<String, CatalogVersion>>,
}

impl RegistryDouble {
    /// Answer `pending_ref` with `version` from now on — the registry's batching, as
    /// a suite scripts it.
    pub fn commit(&self, pending_ref: &str, version: u64) {
        self.commits
            .lock()
            .expect("no panics in the double")
            .insert(pending_ref.to_owned(), CatalogVersion::new(version));
    }

    /// Every `request_id` a handle has been issued for, in no order.
    ///
    /// The **negative** half of a hoisted-refusal probe, and the only one that can
    /// be asserted: D-156's whole argument for hoisting a *permanent* refusal above
    /// the registry call is that a handle requested past it stands pending forever
    /// and trips `pricing.catalogversion.commit_overdue` for a publish that can
    /// never happen. A case that only checked which rows were written could not see
    /// that, because the stranded handle leaves no row.
    ///
    /// Returned as the ids rather than as a count so a failure names *which* act
    /// took one, and asserted as a delta against a pre-call reading rather than
    /// against zero — a probe carrying a positive control has a legitimate handle
    /// in flight, and one asserting absence outright would redden on it.
    #[must_use]
    pub fn requested(&self) -> Vec<String> {
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
        pending_ref: &str,
    ) -> Result<Option<CatalogVersion>, CanonicalError> {
        Ok(self
            .commits
            .lock()
            .expect("no panics in the double")
            .get(pending_ref)
            .copied())
    }
}

/// The committed corpus registry, resolved from this crate's manifest so a suite
/// does not depend on the directory `cargo test` ran in.
#[must_use]
pub fn committed_registry_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/corpus/registry.toml")
}

/// A migrated database, the authoring state over it, and two tenants.
pub struct Harness {
    /// The provider every repository and the transaction seam share.
    pub db: DBProvider<DbError>,
    /// The caller's tenant.
    pub tenant: Uuid,
    /// A tenant the caller is never authorized for.
    pub other: Uuid,
    /// The state the routers are built over.
    pub state: Arc<AuthoringState>,
    /// The state the approval routes and the publish mount are built over.
    pub governance: Arc<GovernanceState>,
    /// What the routes under this harness reported about themselves.
    ///
    /// Read through [`Self::force_flush`] first — the reader is periodic, so an
    /// assertion made without flushing reads zero for every instrument and
    /// passes whenever it expected zero.
    pub metrics: MetricsHarness,
    /// The state the read-only frontier route is built over.
    ///
    /// Mounted here — rather than left to `tests/rest_frontier.rs`'s own
    /// harness — so the router this file builds is the **whole** gear, every
    /// mounted route of it. A set-level property that ranged over all but one
    /// would be a census with a hole in exactly the place a census exists to
    /// close.
    pub frontier: Arc<FrontierState>,
    /// The state the Auditor read is built over (`inst-au-read`), here for the
    /// history read's reason — and it is the state a suite reaches for when it
    /// asserts that a governed act left the trail the error ladder's 403 arms
    /// point at.
    pub audit: Arc<AuditState>,
    /// The state the read-only history route is built over, here for the
    /// frontier's reason: the router this file builds is the whole gear.
    pub history: Arc<HistoryState>,
    /// The one registry both services hold, kept concretely so a suite can script a
    /// commit on it. See [`RegistryDouble::commit`].
    pub registry: Arc<RegistryDouble>,
    /// The state the three membership mutations are built over (Task 6,
    /// `dod-customer-group`'s MUST). Its own state rather than a field on
    /// [`GovernanceState`] — `api::rest::customer_groups`'s section banner.
    pub membership: Arc<bss_pricing::api::rest::customer_groups::MembershipState>,
    /// The lane the two surfaces that accept a repricing run hand the apply to.
    ///
    /// Built once here and cloned into every state, never per client: a lane per
    /// [`Client`] would put each request's accepted run on a queue nothing in this
    /// harness holds the other half of.
    pub apply_lane: bss_pricing::infra::repricing::RunApplyLane,
    /// The applying half, driven by [`Harness::drain_repricing_applies`].
    ///
    /// A mutex because the drain needs `&mut` and every suite holds the harness by
    /// shared reference — and `tokio`'s rather than `std`'s because the drain awaits a
    /// whole apply while holding it.
    apply_worker: tokio::sync::Mutex<bss_pricing::infra::repricing::RunApplyWorker>,
}

impl Harness {
    /// A fresh database with the whole migration chain applied.
    pub async fn new() -> Self {
        let db = connect_db("sqlite::memory:", ConnectOpts::default())
            .await
            .expect("connect in-memory sqlite");
        run_migrations_for_testing(&db, Migrator::migrations())
            .await
            .expect("run the gear migrator");
        let db = DBProvider::<DbError>::new(db);
        let approvals = ApprovalService::new(db.clone());
        // **The real adapter over a private exporter**, not the no-op: a suite
        // that held `NoopPricingMetrics` could assert a route answered and learn
        // nothing about whether it reported, which is exactly the claim a
        // dashboard depends on. Private to this harness, so one suite's counter
        // can never decide another's assertion.
        //
        // Built **before** the first state that takes it, and that placement was
        // a repair (Z12-9): it used to be built after `AuthoringState`, so the
        // bundle plane could not be handed it and silently kept the no-op the
        // paragraph above says this harness does not hold. Production wires it
        // (`module.rs`'s `bundle_service`), so section 10's currency-binding
        // counter was live in production and unobservable in every test.
        let metrics_harness = MetricsHarness::new();
        let metrics: Arc<dyn PricingMetricsPort> = Arc::new(metrics_harness.metrics());
        let state = Arc::new(AuthoringState {
            approvals: approvals.clone(),
            db: db.clone(),
            plans: PlanRepo::new(db.clone()),
            shapes: PlanShapeRepo::new(db.clone()),
            prices: PriceRepo::new(db.clone()),
            bundles: bss_pricing::infra::storage::repo::BundleRepo::new(db.clone()),
            bundle_service: bss_pricing::infra::bundle::BundleService::new(db.clone())
                .with_metrics(Arc::clone(&metrics)),
            overlays: bss_pricing::infra::storage::repo::OverlayRepo::new(db.clone()),
            taxonomies: bss_pricing::infra::storage::repo::taxonomy_repo::TaxonomyRepo::new(
                db.clone(),
            ),
            idempotency: IdempotencyGate::new(Duration::from_hours(1)),
        });
        // **One registry, handed to both services**, which is `src/module.rs`'s own
        // rule — *"The same registry `Arc` the engine holds. Two requesters of one
        // registry, never two incrementers."* Two doubles are not a weaker version
        // of that arrangement, they are a different one: the double numbers its
        // handles per instance, so a second instance re-issues `pend-0` after the
        // first has already written it, and `pricing_catalog_version_ref`'s
        // `pending_version_ref` is unique. The staging that hit it is the ordinary
        // one — publish a plan through the route, then mutate a window on it — which
        // answered **500** out of a `UNIQUE` violation the caller never touched, and
        // which is why nothing in the suite covered that pair. Production shares one
        // `Arc` and `PublishUnitKind::request_token` keeps the two units' handles
        // apart, so the fault was the harness's alone.
        let registry = Arc::new(RegistryDouble::default());
        // The lane every accepted repricing run's apply leaves on, and the applier
        // this harness drives by hand.
        //
        // The compensation half is built and dropped: nothing here cancels an apply,
        // because `drain_repricing_applies` awaits each one to completion — and a
        // closed lane is what `RunLockGuard`'s own fallback covers, exactly as the
        // absent one it covered before.
        let (compensation, _) = bss_pricing::infra::repricing::run_compensation_lane(db.clone());
        let (apply_lane, apply_worker) = bss_pricing::infra::repricing::run_apply_lane(
            db.clone(),
            bss_pricing::infra::storage::repo::PolicyObjectRepo::new(
                db.clone(),
                &LimitsConfig::default(),
            ),
            Arc::clone(&registry) as Arc<_>,
            compensation,
        );
        let governance = Arc::new(GovernanceState {
            participants: bss_pricing::infra::approval_participants::ApprovalParticipants::new(
                Arc::new(toolkit::ClientHub::new()),
            ),
            apply_lane: apply_lane.clone(),
            db: db.clone(),
            plans: PlanRepo::new(db.clone()),
            prices: PriceRepo::new(db.clone()),
            approvals,
            overlays: bss_pricing::infra::storage::repo::OverlayRepo::new(db.clone()),
            overlay_publish: bss_pricing::infra::overlay_publish::OverlayPublishService::new(
                db.clone(),
                Arc::clone(&registry) as Arc<_>,
            ),
            windows: WindowService::new(db.clone(), Arc::clone(&registry) as Arc<_>),
            supersessions: bss_pricing::infra::supersession::SupersessionService::new(
                db.clone(),
                Arc::clone(&registry) as Arc<_>,
            ),
            // D-344: the cutover runs the plan aggregate and the joint gate over
            // the rows it publishes, so it takes the same limits and the **same**
            // committed corpus the publish door takes here.
            cutovers: bss_pricing::infra::cutover::CutoverService::new(
                db.clone(),
                &LimitsConfig::default(),
                FixtureGate::load(&committed_registry_path()),
                Arc::clone(&registry) as Arc<_>,
            ),
            grandfather: bss_pricing::infra::grandfather::GrandfatherService::new(
                db.clone(),
                Arc::clone(&registry) as Arc<_>,
            ),
            retirements: bss_pricing::infra::retirement::RetirementService::new(
                db.clone(),
                Arc::clone(&registry) as Arc<_>,
            ),
            // Slice 11's migration plane. Requests no `CatalogVersion`, so it
            // takes no registry - only the limits its policy reader is bound to.
            migrations: bss_pricing::infra::migration::MigrationService::new(
                db.clone(),
                &LimitsConfig::default(),
            ),
            // Slice 11's synthesis half. No registry and no limits: it freezes a
            // payload nothing can look up (D-87).
            synthesis: bss_pricing::infra::synthesis::SynthesisService::new(db.clone()),
            publish: PublishService::new(
                db.clone(),
                &LimitsConfig::default(),
                FixtureGate::load(&committed_registry_path()),
                Arc::clone(&registry) as Arc<_>,
            )
            .with_metrics(Arc::clone(&metrics)),
            thresholds: bss_pricing::infra::threshold::ThresholdService::new(db.clone()),
            // The window `POST`'s gate (D-191), under the production default TTL: a
            // harness with a different expiry would make the replay tests pass or fail
            // on a knob rather than on the guarantee.
            idempotency: bss_pricing::infra::storage::repo::IdempotencyGate::new(
                LimitsConfig::default().idempotency_key_ttl(),
            ),
            metrics: Arc::clone(&metrics),
        });
        // Task 6's membership state — the same registry `Arc` `governance` holds,
        // one more requester of one registry.
        let membership = Arc::new(bss_pricing::api::rest::customer_groups::MembershipState {
            db: db.clone(),
            idempotency: bss_pricing::infra::storage::repo::IdempotencyGate::new(
                LimitsConfig::default().idempotency_key_ttl(),
            ),
            registry: Arc::clone(&registry) as Arc<_>,
        });
        let frontier = Arc::new(FrontierState {
            pin_frontier: PinFrontierRepo::new(db.clone()),
            db: db.clone(),
        });
        let history = Arc::new(HistoryState {
            history: bss_pricing::infra::history::HistoryExporter::new(db.clone()),
        });
        let audit = Arc::new(AuditState {
            audit: bss_pricing::infra::audit_read::AuditReader::new(db.clone()),
        });
        let tenant = Uuid::now_v7();
        // **The tenant declares the regions its fixtures sell in.**
        // `inst-tx-region` is registered in the Foundation rule set and C2 is
        // fail-closed, so a tenant whose region taxonomy is empty publishes
        // nothing. Every suite built on this harness publishes, so the harness
        // does what a real operator does first: declares the universe.
        //
        // Seeded through the entity rather than the route, because what these
        // suites are about is never the taxonomy — routing every one of them
        // through a `PUT /config/taxonomies/region` would make an unrelated
        // failure there look like a failure in whatever they actually test.
        crate::common::declare_fixture_regions(&db, tenant).await;
        Self {
            db,
            tenant,
            other: Uuid::now_v7(),
            state,
            governance,
            metrics: metrics_harness,
            frontier,
            audit,
            history,
            registry,
            membership,
            apply_lane,
            apply_worker: tokio::sync::Mutex::new(apply_worker),
        }
    }

    /// Apply every repricing run this harness has accepted, and answer how many.
    ///
    /// **The worker's own seam, not a second applier written for tests** —
    /// `RunApplyWorker::drain_pending`, whose production sibling is the `run` loop
    /// `module.rs`'s `serve` spawns. No lifecycle runs here, so nothing spawns that
    /// loop; a suite that wants a run's terminal state calls this and then asserts,
    /// rather than polling a deadline, which on a saturated box is a flake rather
    /// than a finding.
    ///
    /// The count is what discriminates: a drain that applied nothing is a `POST` that
    /// accepted a run and enqueued none, and no assertion on the run's state can tell
    /// that from an apply that ran.
    pub async fn drain_repricing_applies(&self) -> usize {
        self.apply_worker.lock().await.drain_pending().await
    }

    /// One approval record, read past the router.
    ///
    /// A route that opens an approval unit can only be checked from out here by
    /// **reading the unit** — the response can be made to say anything, and a test
    /// that only asserted the response would pass against a handler that minted an id
    /// and opened nothing. That is the exact defect D-225 records for the overlay
    /// submit's 202.
    pub async fn read_approval(
        &self,
        approval_id: Uuid,
    ) -> Option<bss_pricing::infra::storage::repo::ApprovalRecord> {
        let conn = self.db.conn().expect("a scoped connection");
        bss_pricing::infra::storage::repo::approval_repo::read(
            &conn,
            &self.scope(),
            self.tenant,
            approval_id,
        )
        .await
        .expect("the approval reads")
    }

    /// The SQL-level scope a seeding helper writes under.
    pub fn scope(&self) -> AccessScope {
        AccessScope::for_tenant(self.tenant)
    }

    /// The scope of the tenant the caller is never authorized for.
    pub fn other_scope(&self) -> AccessScope {
        AccessScope::for_tenant(self.other)
    }

    fn client(&self, resolver: Arc<dyn AuthZResolverApi>, ctx: Option<Uuid>) -> Client {
        self.client_as(resolver, ctx.map(|tenant| (tenant, Uuid::now_v7())))
    }

    /// The same, with the caller's **principal** named.
    ///
    /// The two-person rule is about identity, so every approval test has to be
    /// able to say who is calling: `client` mints a fresh subject id per call,
    /// which is right for the authoring suites and would make a submitter and
    /// an approver accidentally distinct here — a self-approval test that could
    /// never stage a self-approval.
    pub fn client_as(
        &self,
        resolver: Arc<dyn AuthZResolverApi>,
        ctx: Option<(Uuid, Uuid)>,
    ) -> Client {
        let openapi = OpenApiRegistryImpl::new();
        let router = bss_pricing::api::rest::frontier::router(Arc::clone(&self.frontier), &openapi)
            // Slice 12's bulk import and its history read. Both merged here
            // rather than only in the two censuses, because this is the router
            // the gate properties drive: one absent from it is one no authz
            // property is asked about. (D-293 inserted the bulk mount **above**
            // the history one and left the history comment sitting over it, so
            // this block named the wrong router for two commits.)
            .merge(bss_pricing::api::rest::bulk_imports::router(
                Arc::new(bss_pricing::api::rest::bulk_imports::ApiState {
                    authoring: Arc::clone(&self.state),
                }),
                &openapi,
            ))
            // Slice 12's mass repricing, merged here for the bulk import's
            // reason: this is the router the gate properties drive, and one
            // absent from it is one no authz property is asked about.
            .merge(bss_pricing::api::rest::repricing_runs::router(
                Arc::new(bss_pricing::api::rest::repricing_runs::ApiState {
                    authoring: Arc::clone(&self.state),
                    // The harness's one lane, not a fresh one per client: what this
                    // router accepts has to reach the applier
                    // `drain_repricing_applies` drives.
                    apply_lane: self.apply_lane.clone(),
                }),
                &openapi,
            ))
            .merge(bss_pricing::api::rest::history::router(
                Arc::clone(&self.history),
                &openapi,
            ))
            // Slice 5's Auditor read, merged here for the history read's reason.
            .merge(bss_pricing::api::rest::audit::router(
                Arc::clone(&self.audit),
                &openapi,
            ))
            .merge(bss_pricing::api::rest::plans::router(
                Arc::clone(&self.state),
                &openapi,
            ))
            .merge(bss_pricing::api::rest::prices::router(
                Arc::clone(&self.state),
                &openapi,
            ))
            .merge(bss_pricing::api::rest::overlays::router(
                Arc::clone(&self.state),
                &openapi,
            ))
            // The submit route publishes (D-234) and is mounted on the
            // governance state, apart from its authoring siblings.
            .merge(bss_pricing::api::rest::overlays::governance_router(
                Arc::clone(&self.governance),
                &openapi,
            ))
            .merge(bss_pricing::api::rest::taxonomies::router(
                Arc::clone(&self.state),
                &openapi,
            ))
            .merge(bss_pricing::api::rest::customer_groups::router(
                Arc::clone(&self.state),
                &openapi,
            ))
            .merge(bss_pricing::api::rest::customer_groups::governance_router(
                Arc::clone(&self.membership),
                &openapi,
            ))
            .merge(bss_pricing::api::rest::tax_display_policy::router(
                Arc::clone(&self.state),
                &openapi,
            ))
            .merge(bss_pricing::api::rest::rounding_policy::router(
                Arc::clone(&self.state),
                &openapi,
            ))
            .merge(bss_pricing::api::rest::rounding_policies::router(
                Arc::clone(&self.state),
                &openapi,
            ))
            .merge(bss_pricing::api::rest::gl_codes::router(
                Arc::clone(&self.state),
                &openapi,
            ))
            .merge(bss_pricing::api::rest::catalog_skus::router(
                Arc::new(bss_pricing::api::rest::catalog_skus::ApiState {
                    catalog: Arc::new(
                        bss_pricing::domain::ports::UnconfiguredProductCatalogClientV1,
                    ),
                    source: "unconfigured",
                }),
                &openapi,
            ))
            .merge(bss_pricing::api::rest::preview::router(
                Arc::clone(&self.governance),
                &openapi,
            ))
            .merge(bss_pricing::api::rest::bundles::router(
                Arc::clone(&self.state),
                &openapi,
            ))
            .merge(bss_pricing::api::rest::windows::router(
                Arc::clone(&self.governance),
                &openapi,
            ))
            .merge(bss_pricing::api::rest::supersessions::router(
                Arc::clone(&self.governance),
                &openapi,
            ))
            .merge(bss_pricing::api::rest::cutovers::router(
                Arc::clone(&self.governance),
                &openapi,
            ))
            .merge(bss_pricing::api::rest::retirement::router(
                Arc::clone(&self.governance),
                &openapi,
            ))
            .merge(bss_pricing::api::rest::migrations::router(
                Arc::clone(&self.governance),
                &openapi,
            ))
            .merge(bss_pricing::api::rest::migrated_origin_snapshots::router(
                Arc::clone(&self.governance),
                &openapi,
            ))
            .merge(bss_pricing::api::rest::approvals::router(
                Arc::clone(&self.governance),
                &openapi,
            ))
            .merge(bss_pricing::api::rest::publish::router(
                Arc::clone(&self.governance),
                &openapi,
            ))
            .merge(bss_pricing::api::rest::threshold_policy::router(
                Arc::clone(&self.governance),
                &openapi,
            ))
            .layer(axum::Extension(PolicyEnforcer::new(resolver)))
            // **Production's last layer, and it was in no test harness.**
            //
            // `module.rs:1523` mounts it and a crate-wide grep returned exactly
            // that one hit, so every functional test in this crate asserted an
            // error body production then rewrites. What the middleware does is not
            // cosmetic: on any `application/problem+json` response it deserializes
            // the body into `Problem`, fills `instance` from the request path and
            // `trace_id` from the request headers, and **re-serializes** — a round
            // trip through one type, which is a projection. A member a handler
            // emits that `Problem` does not carry is dropped in production and
            // present in every test, on the envelope this gear's whole contract
            // tells a consumer to key on; and `instance`/`trace_id` were never
            // populated in a test response, so no assertion in the corpus could
            // pin either.
            .layer(axum::middleware::from_fn(
                toolkit::api::canonical_error_middleware,
            ));
        let router = match ctx {
            Some((tenant, principal)) => {
                router.layer(axum::Extension(ctx_for_principal(tenant, principal)))
            }
            None => router,
        };
        Client { router }
    }

    /// An authorized caller in this tenant, acting as `principal`.
    pub fn allowed_as(&self, principal: Uuid) -> Client {
        self.client_as(
            Arc::new(FlatInResolver {
                allowed: vec![self.tenant],
            }),
            Some((self.tenant, principal)),
        )
    }

    /// An authorized caller in the harness's own tenant.
    pub fn allowed(&self) -> Client {
        self.client(
            Arc::new(FlatInResolver {
                allowed: vec![self.tenant],
            }),
            Some(self.tenant),
        )
    }

    /// An authenticated caller the PDP refuses.
    pub fn denied(&self) -> Client {
        self.client(Arc::new(DenyingResolver), Some(self.tenant))
    }

    /// A caller with no authenticated context at all.
    pub fn anonymous(&self) -> Client {
        self.client(
            Arc::new(FlatInResolver {
                allowed: vec![self.tenant],
            }),
            None,
        )
    }

    /// A caller authorized only for the **other** tenant, reading this one's
    /// rows: the genuine cross-tenant probe.
    pub fn other_tenant(&self) -> Client {
        self.client(
            Arc::new(FlatInResolver {
                allowed: vec![self.other],
            }),
            Some(self.other),
        )
    }

    /// A caller authenticated in **this** tenant whose PDP authorizes only the
    /// **other** one.
    ///
    /// The only shape that exercises `access_scope`'s write-target membership
    /// assertion: the degraded flat-`In` decision does not re-check
    /// `owner_tenant_id`, so a write anchored to a tenant outside the compiled
    /// scope is refused there or nowhere.
    pub fn scope_mismatch(&self) -> Client {
        self.client(
            Arc::new(FlatInResolver {
                allowed: vec![self.other],
            }),
            Some(self.tenant),
        )
    }

    /// A caller whose PDP is down.
    pub fn unavailable(&self) -> Client {
        self.client(Arc::new(UnavailableResolver), Some(self.tenant))
    }

    /// A caller the PDP allows without constraining anything.
    pub fn unconstrained(&self) -> Client {
        self.client(Arc::new(UnconstrainedResolver), Some(self.tenant))
    }

    /// An authorized caller whose every `EvaluationRequest` is captured.
    pub fn recording(&self) -> (Client, Arc<Mutex<Vec<EvaluationRequest>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let client = self.client(
            Arc::new(RecordingResolver {
                allowed: vec![self.tenant],
                seen: Arc::clone(&seen),
            }),
            Some(self.tenant),
        );
        (client, seen)
    }

    /// A caller authorized for exactly the `(resource_type, action)` pairs
    /// named, in this tenant, acting as `principal` — and refused for every
    /// other pair.
    ///
    /// The fixture a separate-route claim needs: `allowed_as` grants every
    /// pair uniformly, which cannot distinguish "this route is on the right
    /// gate" from "this route is on some gate everything happens to satisfy".
    pub fn selectively_allowed_as(
        &self,
        principal: Uuid,
        pairs: &[(&'static str, &'static str)],
    ) -> Client {
        self.client_as(
            Arc::new(SelectiveResolver {
                allowed_tenant: self.tenant,
                allowed_pairs: pairs.to_vec(),
                pair_tenants: Vec::new(),
            }),
            Some((self.tenant, principal)),
        )
    }

    /// [`Self::selectively_allowed_as`] with one pair's allow compiled to
    /// [`Self::other`] instead of this tenant.
    ///
    /// The shape a route asking two PDP questions needs and no other fixture here
    /// can stage: `SelectiveResolver` answered one tenant for every pair, so a
    /// second question that forgot to check tenant membership answered
    /// identically to one that checked it, under every case in the crate.
    pub fn selectively_allowed_as_with_foreign_pair(
        &self,
        principal: Uuid,
        pairs: &[(&'static str, &'static str)],
        foreign: (&'static str, &'static str),
    ) -> Client {
        self.client_as(
            Arc::new(SelectiveResolver {
                allowed_tenant: self.tenant,
                allowed_pairs: pairs.to_vec(),
                pair_tenants: vec![(foreign, self.other)],
            }),
            Some((self.tenant, principal)),
        )
    }

    /// Publish the plan's revision `0`, so the plan holds a **current**
    /// revision.
    ///
    /// Straight through `plan_repo::publish_revision` and **not** through
    /// `POST /plans/{planId}/publish`, and the reason is isolation rather than
    /// absence: the route is mounted, but reaching its commit arm takes a
    /// publishable shape, a submit, an independent approve and a registry that
    /// answers — four preconditions a suite whose subject is something else would
    /// then be asserting about. `rest_publish.rs` and `rest_approvals.rs` drive
    /// that sequence deliberately, and
    /// `rest_windows.rs::a_plans_first_window_is_authorable_through_the_routes_after_an_empty_publish`
    /// drives it to prove the routes compose.
    pub async fn publish(&self, plan_id: Uuid, revision: u64) {
        let plan_id = PlanId::new(plan_id);
        let scope = self.scope();
        let tenant = self.tenant;
        let current = self
            .state
            .plans
            .find_revision(&scope, tenant, plan_id, revision)
            .await
            .expect("read the revision")
            .expect("the revision exists");
        let (_, outcome) = self
            .db
            .db()
            .in_transaction::<PlanRevision, bss_pricing::infra::storage::RepoError, _>(move |txn| {
                Box::pin(async move {
                    plan_repo::publish_revision(
                        txn,
                        &scope,
                        tenant,
                        plan_id,
                        revision,
                        current.row_version,
                    )
                    .await
                })
            })
            .await;
        outcome.expect("publish the seeded revision");
    }

    /// Publish a seeded price row so a suite can aim a mutating verb at a frozen
    /// one.
    ///
    /// Through the repository's own `publish_rows`, which is the sanctioned
    /// producer of `draft -> published` for a price row. [`Harness::publish`]'s
    /// note applies verbatim: the route exists, and what this avoids is staging
    /// its four preconditions in a suite that is about something else.
    pub async fn publish_price(&self, plan_id: Uuid, price_id: Uuid) {
        let scope = self.scope();
        let tenant = self.tenant;
        let plan_id = PlanId::new(plan_id);
        // The row's own market, so the readiness below declares a category for the
        // region this row actually names. Read with `allow_all` for `price_rows`'
        // reason: a seeder must see what landed, not what the caller may read.
        let region = self
            .state
            .prices
            .list_for_plan(
                &AccessScope::allow_all(),
                tenant,
                plan_id,
                &[LifecycleState::Draft],
            )
            .await
            .expect("list the plan's draft price rows")
            .into_iter()
            .find(|row| row.price_id == price_id)
            .unwrap_or_else(|| panic!("price row {price_id} is a draft of plan {plan_id}"))
            .scope_key
            .region()
            .as_str()
            .to_owned();
        let readiness = fixture_readiness(&region);
        let (_, outcome) = self
            .db
            .db()
            .in_transaction::<Vec<Uuid>, bss_pricing::infra::storage::RepoError, _>(move |txn| {
                Box::pin(async move {
                    price_repo::publish_rows(
                        txn,
                        &scope,
                        tenant,
                        plan_id,
                        &[(price_id, RowVersion::new(0))],
                        &readiness,
                        // **A tenant default, because a published row must resolve
                        // a rounding policy at all** (review F1, 2026-08-19).
                        // This passed `None` under a comment saying the harness
                        // seeds a row-level policy — true of `publishable_row` and
                        // not of every content a caller hands this seeder, and the
                        // bundle suite's rows carry none. `publish_rows` refuses a
                        // set that resolves nothing, so a seeder passing `None`
                        // refuses four cases on a ground none of them is about.
                        Some("half_up"),
                    )
                    .await
                })
            })
            .await;
        outcome.expect("publish the seeded price row");
    }

    /// Move a revision straight to `retired`.
    ///
    /// Retirement is Slice 11's publish unit (D-128). **This comment used to say
    /// the edge "has no producer in this gear at all", and that has been false
    /// since 2026-08-07**: `retirement::retire_in` reaches
    /// `plan_repo::retire_revision`, behind the mounted `POST` at `PLAN_RETIRE`,
    /// with its own suite. The 2026-08-10 review read this comment rather than
    /// the code and filed the whole retirement commit lane as absent-by-design;
    /// the 2026-08-14 audit of that review found the plane had existed for three
    /// days. A finding sourced from a doc inherits the doc's errors.
    ///
    /// The helper stays, because a suite that only needs a retired row should not
    /// have to drive the whole unit to get one — that is a fixture shortcut, not
    /// a statement about what the gear can do. The append-only trigger permits
    /// the edge: it fires only once a row is past `draft`, and
    /// `published -> retired` is one of the two flips it whitelists.
    pub async fn retire(&self, plan_id: Uuid, revision: i64) {
        let conn = self.db.conn().expect("conn");
        let result = plan::Entity::update_many()
            .secure()
            .scope_with(&self.scope())
            .col_expr(
                plan::Column::LifecycleState,
                Expr::value(LifecycleState::Retired.as_str()),
            )
            .filter(
                Condition::all()
                    .add(plan::Column::PlanId.eq(plan_id))
                    .add(plan::Column::Revision.eq(revision)),
            )
            .exec(&conn)
            .await
            .expect("retire the seeded revision");
        assert_eq!(result.rows_affected, 1, "the seed must have moved one row");
    }

    /// Open the plan's next revision, so it holds a draft **and** a current one.
    pub async fn open_successor(&self, plan_id: Uuid) {
        self.state
            .plans
            .open_revision(
                &self.scope(),
                self.tenant,
                PlanId::new(plan_id),
                stamp_of(SEED_ACTOR, at(11)),
            )
            .await
            .expect("open the successor revision");
    }

    /// Abandon the plan's draft revision at `revision`.
    pub async fn abandon_draft(&self, plan_id: Uuid, revision: u64) {
        let plan_id = PlanId::new(plan_id);
        let current = self
            .state
            .plans
            .find_revision(&self.scope(), self.tenant, plan_id, revision)
            .await
            .expect("read the revision")
            .expect("the revision exists");
        self.state
            .plans
            .abandon_draft(
                &self.scope(),
                self.tenant,
                plan_id,
                revision,
                current.row_version,
                stamp(),
            )
            .await
            .expect("abandon the seeded draft");
    }

    /// The plan's current entity tag, read the way a caller obtains one.
    ///
    /// Read rather than constructed: a tag this harness built itself would keep
    /// matching after the route stopped minting the one the store expects, which
    /// is the whole failure `If-Match` exists to catch.
    pub async fn plan_etag(&self, plan_id: Uuid) -> String {
        let response = self
            .allowed()
            .send(with_headers(
                "GET",
                &format!("{}/{plan_id}", bss_pricing::api::rest::plans::PLANS),
                None,
                &[],
            ))
            .await;
        etag_of(&response).expect("a plan read must carry its entity tag")
    }

    /// Attach one of each child set to a draft revision, so a read has all three
    /// facets to answer with.
    pub async fn attach_shape(&self, plan_id: Uuid, revision: u64) {
        let plan_id = PlanId::new(plan_id);
        let scope = self.scope();
        let mut version = self
            .state
            .plans
            .find_revision(&scope, self.tenant, plan_id, revision)
            .await
            .expect("read")
            .expect("exists")
            .row_version;

        version = self
            .state
            .shapes
            .replace_phases(
                &scope,
                self.tenant,
                plan_id,
                revision,
                version,
                vec![PlanPhase {
                    phase_id: seeded_phase(),
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
            .expect("replace phases")
            .row_version;

        version = self
            .state
            .shapes
            .replace_addon_rules(
                &scope,
                self.tenant,
                plan_id,
                revision,
                version,
                vec![AddonRule {
                    addon_sku_id: Uuid::from_u128(0xadd0),
                    required: false,
                    min_qty: None,
                    max_qty: Some(3),
                    step_qty: None,
                    price_override_ref: None,
                    depends_on: Vec::new(),
                    conflicts_with: Vec::new(),
                }],
                stamp(),
            )
            .await
            .expect("replace add-on rules")
            .row_version;

        self.state
            .shapes
            .set_descriptor_set(
                &scope,
                self.tenant,
                plan_id,
                revision,
                version,
                DescriptorSet {
                    invoice_line_template: Some("{plan} subscription".to_owned()),
                    gl_code: Some("4000".to_owned()),
                    itemization_rule: Some("per_line".to_owned()),
                    additional: std::collections::BTreeMap::new(),
                },
                stamp(),
            )
            .await
            .expect("attach the descriptor set");
    }
}

/// The terminal phase [`Harness::attach_shape`] seeds, for a price row's `phase`
/// scope-key axis.
#[must_use]
pub fn seeded_phase() -> PhaseId {
    PhaseId::new(Uuid::from_u128(0x9ba5e))
}

/// A router with one PDP double and one authentication state bound.
pub struct Client {
    router: Router,
}

impl Client {
    /// Wrap a router this module did not build.
    ///
    /// For the one surface whose collaborator the shared `Harness` cannot vary:
    /// `catalog_skus` is mounted here, in `module_test` and in `rest_authz` with
    /// `UnconfiguredProductCatalogClientV1`, which is the right double for a
    /// census and for the authz properties and leaves the handler's success arm
    /// unreachable. `rest_catalog_skus.rs` mounts the same router with a
    /// configured state and drives it through this.
    ///
    /// Deliberately not a general escape hatch: it takes an assembled `Router`,
    /// so a caller still owns the middleware stack it is asserting against, and
    /// nothing here reaches into the harness's own state.
    pub fn over(router: Router) -> Self {
        Self { router }
    }

    /// Drive one request through the whole stack.
    pub async fn send(&self, request: Request<Body>) -> Response<Body> {
        self.router
            .clone()
            .oneshot(request)
            .await
            .expect("the router answers")
    }
}

// ---------------------------------------------------------------------------
// Request and response helpers.
// ---------------------------------------------------------------------------

/// Build a request with an optional JSON body and no preconditions.
pub fn request(method: &str, path: &str, body: Option<serde_json::Value>) -> Request<Body> {
    with_headers(method, path, body, &[])
}

/// Build a request carrying the named headers.
pub fn with_headers(
    method: &str,
    path: &str,
    body: Option<serde_json::Value>,
    headers: &[(&str, &str)],
) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(path);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    match body {
        Some(json) => builder
            .header("content-type", "application/json")
            .body(Body::from(json.to_string()))
            .expect("build request"),
        None => builder.body(Body::empty()).expect("build request"),
    }
}

/// The response's `ETag`, when it set one.
pub fn etag_of(response: &Response<Body>) -> Option<String> {
    response
        .headers()
        .get(PLAN_TAG_HEADER)
        .map(|value| value.to_str().expect("the tag is ASCII").to_owned())
}

/// The response's `Location`, when it set one.
pub fn location_of(response: &Response<Body>) -> Option<String> {
    response
        .headers()
        .get("location")
        .map(|value| value.to_str().expect("the location is ASCII").to_owned())
}

/// Read a response body as JSON.
pub async fn body_json(response: Response<Body>) -> serde_json::Value {
    let bytes = to_bytes(response.into_body(), 4_000_000)
        .await
        .expect("read body");
    if bytes.is_empty() {
        return serde_json::Value::Null;
    }
    serde_json::from_slice(&bytes).expect("the body is JSON")
}

/// The canonical **family** the problem document names, as the bare family
/// segment: `"unauthenticated"`, `"permission_denied"`, `"not_found"`,
/// `"invalid_argument"`, `"aborted"`, ....
///
/// [`problem_code`]'s counterpart for the refusals that carry no code at all. The
/// 401 and 403 families have no `reason` slot — `permission_denied()` has no detail
/// slot by design, argued in `error_mapping.rs` on the ground that a detail riding
/// the code as `"CODE: detail"` would break exact matching — so a suite asserting
/// only the status was asserting the weaker half of the only two things those
/// documents carry. Four suites did exactly that.
///
/// Also proves the body **is** a problem document, which is the half the
/// canonical-error layer is about: it deserializes and re-serializes every
/// `application/problem+json`, so a response that merely had the right status would
/// fail here.
pub async fn problem_family(response: Response<Body>) -> String {
    let body = body_json(response).await;
    let raw = body["type"]
        .as_str()
        .unwrap_or_else(|| panic!("no canonical type in the problem document: {body}"))
        .to_owned();
    // `rsplit_once` and not `rsplit(..).next()`, because only the former can say
    // *no*: `rsplit` yields the whole string when the pattern is absent and
    // `split(..).next()` is always `Some`, so the diagnostic below used to be
    // unreachable and a `type` of `about:blank` came back as the "family"
    // `about:blank` — the four suites that read this then failed on a confusing
    // equality mismatch instead of the message written to explain it.
    let Some((_, tail)) = raw.rsplit_once("cf.core.err.") else {
        panic!("the canonical type is not a `cf.core.err.*` id: {raw}");
    };
    // The version suffix off, so a `~`-terminated GTS id yields the bare family.
    if let Some((family, _)) = tail.split_once(".v1") {
        family.to_owned()
    } else {
        tail.to_owned()
    }
}

/// The RFC 9457 problem document's machine-readable code.
///
/// Asserted instead of the status wherever a refusal has a code, because §3.3
/// makes the code the discriminator a consumer matches on — several distinct
/// refusals share one status, and a test that only read the status would pass
/// with the wrong one.
pub async fn problem_code(response: Response<Body>) -> String {
    let body = body_json(response).await;
    code_in(&body)
}

/// A denial's own reason, off the 403 that carries it.
///
/// **Not [`problem_family`], which cannot name a door.** Every 403 this gear can
/// emit goes through `permission_denied()`, so the canonical family is constant
/// across the PDP's refusal and each of the gear's own arms; `with_reason` is
/// the only free field such an error has, which makes `context.reason` the
/// discriminator §3.3 asks a consumer to match on. A case asserting the family
/// alone stays green when a guard is replaced by a hand-built 403 that skips the
/// `pricing.authz.deny` funnel `inst-rb-audit` requires.
///
/// [`find_code`] cannot reach it: the PDP's reasons are lower-case and that
/// filter takes screaming snake case only.
pub async fn denial_reason(response: Response<Body>) -> String {
    let body = body_json(response).await;
    body["context"]["reason"]
        .as_str()
        .unwrap_or_else(|| panic!("a 403 carries the reason it was denied for: {body}"))
        .to_owned()
}

/// The wire code off a **404** problem document.
///
/// Not [`problem_code`], and the difference is a property of the platform rather
/// than a helper preference: that one reads the `reason` an
/// `aborted`/`failed_precondition` document carries, and the canonical
/// **not-found** family has no such slot — so a 404's code rides
/// `context.resource_name`. §5 declares code and status together and does not say
/// where the code sits, so this is a divergence in *rendering* rather than in the
/// contract; it is recorded as `T-11`.
///
/// Here rather than in each suite because two suites now read it — `rest_preview`
/// for `PRICE_ROW_ABSENT` and `rest_plans` for `CLONE_SOURCE_NOT_FOUND` — and two
/// spellings of one reading are two things that can disagree about the platform.
pub async fn not_found_code(response: Response<Body>) -> String {
    body_json(response).await["context"]["resource_name"]
        .as_str()
        .expect("a 404 that declares a code names it")
        .to_owned()
}

/// The same code, off a problem document a caller already holds.
///
/// [`problem_code`] consumes the response, so a case that asserts the code **and**
/// something else about the body — that a refusal names the unit holding a key, say —
/// could otherwise only read one of the two. Both spellings go through [`find_code`],
/// which is the point: a second reading of the document here would be free to disagree
/// with the one every other case uses.
#[must_use]
pub fn code_in(body: &serde_json::Value) -> String {
    find_code(body).unwrap_or_else(|| panic!("no wire code in the problem document: {body}"))
}

/// A refusal's discriminator when it mints **no wire code**: the canonical family
/// and the guard's own sentence.
///
/// `DomainError::InvalidRequest` renders `invalid_argument` with the detail as a
/// constraint and no `reason`, so [`problem_code`] finds nothing to read - which is
/// why the cases using this asserted their status alone, on doors where several
/// independent guards answer one status. The sentence is the only thing that
/// separates them, and it is the guard's own words rather than a paraphrase so a
/// reworded guard has to be visited here.
pub fn refused_by(body: &serde_json::Value, family: &str, sentence: &str) {
    // The family is a *segment* of the canonical type, not its tail:
    // `gts://gts.cf.core.errors.err.v1~cf.core.err.invalid_argument.v1~`. Matched
    // through the `cf.core.err.` prefix the way `problem_family` derives it, so a
    // `type` naming a different family cannot satisfy it by sharing a suffix.
    let declared = body["type"].as_str().unwrap_or_default();
    assert!(
        declared.contains(&format!("cf.core.err.{family}.")),
        "the refusal's canonical family must be `{family}`: {body}"
    );
    assert!(
        body.to_string().contains(sentence),
        "and it must be the refusal under test (`{sentence}`): {body}"
    );
}

/// The code can ride either the `reason` of an aborted/denied error or a
/// precondition violation's `type`; both spellings are the platform's, so both
/// are read rather than one being assumed.
fn find_code(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::Object(map) => {
            for key in ["reason", "type", "code"] {
                if let Some(serde_json::Value::String(found)) = map.get(key)
                    && found.chars().all(|c| c.is_ascii_uppercase() || c == '_')
                    && found.len() > 3
                {
                    return Some(found.clone());
                }
            }
            map.values().find_map(find_code)
        }
        serde_json::Value::Array(items) => items.iter().find_map(find_code),
        _ => None,
    }
}

/// The `description` of the precondition violation filed under `subject`.
///
/// Here rather than in a suite for [`code_in`]'s stated reason: a second reading
/// of the same document would be free to disagree with this one. The array is
/// found by key at any depth because its enclosing wrapper is the platform's to
/// name, and a case pinning a violation should fail when the violation is gone,
/// not when the envelope is renamed.
///
/// The key is the **rendered** `violations`, not the builder's
/// `precondition_violations`: the two differ, and only the first is on the wire.
#[must_use]
pub fn violation_for(body: &serde_json::Value, subject: &str) -> Option<String> {
    fn violations(value: &serde_json::Value) -> Option<&Vec<serde_json::Value>> {
        match value {
            serde_json::Value::Object(map) => map
                .get("violations")
                .and_then(serde_json::Value::as_array)
                .or_else(|| map.values().find_map(violations)),
            serde_json::Value::Array(items) => items.iter().find_map(violations),
            _ => None,
        }
    }
    violations(body)?
        .iter()
        .find(|violation| violation["subject"] == serde_json::json!(subject))
        .and_then(|violation| violation["description"].as_str())
        .map(ToOwned::to_owned)
}

/// A seeded instant, quantized to the millisecond the catalog compares at.
pub fn at(hour: u32) -> OffsetDateTime {
    utc_ymd_hms(2026, 8, 3, hour, 0, 0)
}

/// A draft plan carrying enough shape to be recognizable, seeded straight
/// through the repository.
pub async fn seed_draft_plan(harness: &Harness, plan_id: Uuid) {
    harness
        .state
        .plans
        .create_draft(&harness.scope(), new_draft(plan_id, harness.tenant))
        .await
        .expect("seed the draft plan");
}

/// A plan whose revision `0` is published, so it holds a **current** revision
/// and no open draft.
pub async fn seed_current_plan(harness: &Harness, plan_id: Uuid) {
    seed_draft_plan(harness, plan_id).await;
    harness.publish(plan_id, 0).await;
}

/// [`seed_current_plan`], with a shape the aggregate publish rule set actually
/// passes: [`seed_publishable_shape`]'s own phase, frequency and descriptor
/// set, but at [`seeded_phase`] — the id every fixed-key seed
/// (`seed_price_keyed`, `seed_priced_row`) already files its rows under —
/// rather than a freshly minted one, and published rather than left open.
///
/// `seed_current_plan` alone leaves a plan with **zero** phases and no
/// frequency or descriptor set, which `PHASE_GRAPH_INVALID`,
/// `CYCLE_METADATA_MISSING` and `DESCRIPTOR_INCOMPLETE` refuse outright. That
/// was never reachable through this crate's raw seed doors —
/// `Harness::publish`/`publish_price` bypass the rule set entirely — until a
/// caller ran the **real** aggregate pass over a plan seeded this way, which
/// is exactly what `crate::infra::repricing::apply_run_in`'s aggregate pass
/// does.
pub async fn seed_current_plan_with_phase(harness: &Harness, plan_id: Uuid) {
    let plan = PlanId::new(plan_id);
    let scope = harness.scope();
    let created = harness
        .state
        .plans
        .create_draft(
            &scope,
            NewPlanDraft {
                plan_name: None,
                plan_id: plan,
                tenant_id: harness.tenant,
                created_by: SEED_ACTOR,
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
                correlation_id: TEST_CORRELATION,
            },
        )
        .await
        .expect("seed the draft plan");
    let after_phases = harness
        .state
        .shapes
        .replace_phases(
            &scope,
            harness.tenant,
            plan,
            created.revision,
            created.row_version,
            vec![PlanPhase {
                phase_id: seeded_phase(),
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
    harness
        .state
        .shapes
        .set_descriptor_set(
            &scope,
            harness.tenant,
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
    harness.publish(plan_id, created.revision).await;
}

/// The same, in the **other** tenant, for the cross-tenant probes.
pub async fn seed_foreign_plan(harness: &Harness, plan_id: Uuid) {
    harness
        .state
        .plans
        .create_draft(&harness.other_scope(), new_draft(plan_id, harness.other))
        .await
        .expect("seed the foreign draft plan");
}

/// A **published** plan in the other tenant.
///
/// [`seed_foreign_plan`] leaves a draft, which every scoped read reports as
/// unpublished for the trivial reason that it *is* unpublished — so a probe about
/// scoping that used it would pass against no scoping at all. The published state
/// is the one where an unscoped read and a scoped read give different answers, and
/// therefore the only one that can tell them apart.
pub async fn seed_foreign_current_plan(harness: &Harness, plan_id: Uuid) {
    let plan = PlanId::new(plan_id);
    let scope = harness.other_scope();
    let tenant = harness.other;
    harness
        .state
        .plans
        .create_draft(&scope, new_draft(plan_id, tenant))
        .await
        .expect("seed the foreign draft plan");
    let current = harness
        .state
        .plans
        .find_revision(&scope, tenant, plan, 0)
        .await
        .expect("read the foreign revision")
        .expect("the foreign revision exists");
    let (_, outcome) = harness
        .db
        .db()
        .in_transaction::<PlanRevision, bss_pricing::infra::storage::RepoError, _>(move |txn| {
            Box::pin(async move {
                plan_repo::publish_revision(txn, &scope, tenant, plan, 0, current.row_version).await
            })
        })
        .await;
    outcome.expect("publish the foreign revision");
}

/// The row version a revision stands at, or `None` when there is no such row.
///
/// The readback every denial test needs: a 403 alone would also be produced by a
/// handler that wrote first and checked second.
pub async fn plan_row_version(harness: &Harness, plan_id: Uuid, revision: u64) -> Option<u64> {
    harness
        .state
        .plans
        .find_revision(
            &AccessScope::allow_all(),
            harness.tenant,
            PlanId::new(plan_id),
            revision,
        )
        .await
        .expect("read the revision")
        .map(|row| row.row_version.get())
}

/// How many plan revisions the caller's tenant holds.
///
/// The readback a create's denial needs: "403" says nothing about whether a row
/// landed, and a handler that wrote first and checked second would answer the
/// same status.
pub async fn plan_count(harness: &Harness) -> usize {
    let conn = harness.db.conn().expect("conn");
    plan::Entity::find()
        .secure()
        .scope_with(&AccessScope::allow_all())
        .filter(Condition::all().add(plan::Column::TenantId.eq(harness.tenant)))
        .all(&conn)
        .await
        .expect("count plan revisions")
        .len()
}

/// The lifecycle state a revision stands in, or `None` when there is no such row.
pub async fn plan_state(harness: &Harness, plan_id: Uuid, revision: u64) -> Option<String> {
    harness
        .state
        .plans
        .find_revision(
            &AccessScope::allow_all(),
            harness.tenant,
            PlanId::new(plan_id),
            revision,
        )
        .await
        .expect("read the revision")
        .map(|row| row.lifecycle_state.to_string())
}

fn new_draft(plan_id: Uuid, tenant_id: Uuid) -> NewPlanDraft {
    NewPlanDraft {
        plan_name: None,
        plan_id: PlanId::new(plan_id),
        tenant_id,
        created_by: SEED_ACTOR,
        created_at_utc: at(10),
        sku_id: None,
        plan_tier: Some("gold".to_owned()),
        billing_cycle: Some(BillingCycle::Recurring),
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

/// The price rows the caller's tenant holds on a plan, in `price_id` order.
///
/// Read with `AccessScope::allow_all()` so a denial test sees what actually
/// landed rather than what the caller was allowed to see.
pub async fn price_rows(harness: &Harness, plan_id: Uuid) -> Vec<PriceRecord> {
    harness
        .state
        .prices
        .list_for_plan(
            &AccessScope::allow_all(),
            harness.tenant,
            PlanId::new(plan_id),
            &[
                LifecycleState::Draft,
                LifecycleState::Published,
                LifecycleState::Superseded,
            ],
        )
        .await
        .expect("list the plan's price rows")
}

/// Every audit record the caller's tenant holds, in `(chain_id, seq)` order.
///
/// Read with `AccessScope::allow_all()` for `price_rows`' reason: a test asserting
/// what a mutation recorded must see what landed rather than what the caller was
/// allowed to see.
pub async fn audit_rows(harness: &Harness) -> Vec<audit_log::Model> {
    let conn = harness.db.conn().expect("conn");
    audit_log::Entity::find()
        .secure()
        .scope_with(&AccessScope::allow_all())
        .filter(Condition::all().add(audit_log::Column::TenantId.eq(harness.tenant)))
        .order_by(audit_log::Column::ChainId, Order::Asc)
        .order_by(audit_log::Column::Seq, Order::Asc)
        .all(&conn)
        .await
        .expect("read the audit chain")
}

/// The correlation id of every `pricing_outbox` row the caller's tenant holds.
///
/// The column is `NOT NULL` there, unlike the audit log's, which is D-178
/// clause (1) already made physical on one of the two stores.
pub async fn outbox_correlations(harness: &Harness) -> Vec<Uuid> {
    let conn = harness.db.conn().expect("conn");
    outbox::Entity::find()
        .secure()
        .scope_with(&AccessScope::allow_all())
        .filter(Condition::all().add(outbox::Column::TenantId.eq(harness.tenant)))
        .order_by(outbox::Column::Seq, Order::Asc)
        .all(&conn)
        .await
        .expect("read the outbox")
        .into_iter()
        // `PriceCreated` is the **authoring** door's (S3 §17.5), so it belongs to
        // whatever seeded the row rather than to the act under test. Excluded here
        // for `sqlite_publish_commit::outbox_rows`' reason: the callers assert what
        // one commit emitted, and counting the seed's event in would have been
        // repaired by bumping each expectation, which leaves the assertions named
        // for guarantees they stopped checking.
        //
        // **True only while no caller's act authors a price row.** Today's callers
        // publish plans and mutate windows, and neither path reaches
        // `write_prepared`. A caller whose act stages a draft — a supersession —
        // would have this helper delete its event: that is what happened in
        // `sqlite_supersession_unit`, which excludes the fixture by sequence now.
        .filter(|row| row.event_name != "PriceCreated")
        .map(|row| row.correlation_id)
        .collect()
}

/// The correlation id of every `pricing_outbox` row carrying **one named
/// event**, in `seq` order.
///
/// [`outbox_correlations`] answers "what did this tenant's outbox accumulate",
/// which is the right question for a caller measuring a **delta** across an act
/// and the wrong one for a caller asserting what a single commit emitted. Its
/// own doc said as much — *"true only while no caller's act authors a price
/// row"* — and the same sentence held for the fixture: since the plan plane got
/// its `PlanCreated`/`PlanUpdated` producers, seeding a publishable plan leaves
/// three rows in the outbox before the act under test runs.
///
/// The repair is to name the event rather than to bump a count. A count is a
/// proxy for "the row this act wrote", and it passes just as happily when the
/// one row present is some other event entirely.
pub async fn outbox_correlations_of(harness: &Harness, event_name: &str) -> Vec<Uuid> {
    let conn = harness.db.conn().expect("conn");
    outbox::Entity::find()
        .secure()
        .scope_with(&AccessScope::allow_all())
        .filter(
            Condition::all()
                .add(outbox::Column::TenantId.eq(harness.tenant))
                .add(outbox::Column::EventName.eq(event_name)),
        )
        .order_by(outbox::Column::Seq, Order::Asc)
        .all(&conn)
        .await
        .expect("read the outbox")
        .into_iter()
        .map(|row| row.correlation_id)
        .collect()
}

/// Every `pricing_catalog_version_ref` row the caller's tenant holds, in
/// `requested_at` then `pending_ref` order.
///
/// The readback D-99's re-projection half needs, and it had none: a mutation's
/// pending ref is what carries the plan subject to the projector, so a test that only
/// read the *response* body's handle was asserting about a value the registry supplied
/// **before** the row was written. Deleting the write left the whole suite green.
///
/// Read with `AccessScope::allow_all()` for `price_rows`' reason: what landed, not
/// what the caller was allowed to see.
pub async fn pending_version_refs(harness: &Harness) -> Vec<catalog_version_ref::Model> {
    let conn = harness.db.conn().expect("conn");
    catalog_version_ref::Entity::find()
        .secure()
        .scope_with(&AccessScope::allow_all())
        .filter(Condition::all().add(catalog_version_ref::Column::TenantId.eq(harness.tenant)))
        .order_by(catalog_version_ref::Column::RequestedAt, Order::Asc)
        .order_by(catalog_version_ref::Column::PendingRef, Order::Asc)
        .all(&conn)
        .await
        .expect("read the pending version refs")
}

/// One `pricing_group_membership` row, read back the way the projector does —
/// through the entity rather than the repository, so a route-level suite can
/// assert what actually landed without going back through the authz gate a
/// second time.
///
/// Read with `AccessScope::allow_all()` for `price_rows`' reason: what
/// landed, not what the caller was allowed to see.
pub async fn membership_row(
    harness: &Harness,
    membership_id: Uuid,
) -> Option<group_membership::Model> {
    let conn = harness.db.conn().expect("conn");
    group_membership::Entity::find()
        .secure()
        .scope_with(&AccessScope::allow_all())
        .filter(
            Condition::all()
                .add(group_membership::Column::TenantId.eq(harness.tenant))
                .add(group_membership::Column::MembershipId.eq(membership_id)),
        )
        .one(&conn)
        .await
        .expect("read pricing_group_membership")
}

/// Seed one draft price row on a distinct region, so several can coexist.
pub async fn seed_price(harness: &Harness, plan_id: Uuid, region: &str) -> PriceRecord {
    seed_price_keyed(
        harness,
        plan_id,
        region,
        PriceEligibility::AllSubscriptions,
        Cohort::None,
    )
    .await
}

/// The same fixture on a **named eligibility class and cohort**.
///
/// [`seed_price`] delegates here rather than the two carrying a copy of one
/// fixture: what a suite about the grandfathered class needs is that row's *key*
/// to differ and everything else — the model kind, the proration contract, the
/// actor, the instant — to stay the row every other suite is seeded with. Two
/// bodies would drift on the content, and a suite asserting about the class would
/// then be asserting about a row that differs in ways it never named.
///
/// The pairing is `ScopeKey::new`'s to enforce: `cohort != none` if and only if
/// the class is `existing_grandfathered`, so a caller that gets it wrong is
/// refused here rather than seeding a key no resolution class selects.
pub async fn seed_price_keyed(
    harness: &Harness,
    plan_id: Uuid,
    region: &str,
    price_eligibility: PriceEligibility,
    cohort: Cohort,
) -> PriceRecord {
    seed_price_keyed_with_horizon(harness, plan_id, region, price_eligibility, cohort, None).await
}

/// [`seed_price_keyed`] carrying a **grandfathering horizon**.
///
/// The one field the class exists for, and the only fixture that can reach it:
/// `chk_pricing_price_grandfather_until` pairs a non-null `grandfather_until`
/// with `price_eligibility = 'existing_grandfathered'` (D-147,
/// `GRANDFATHER_UNTIL_FORBIDDEN`), so a caller passing one on any other class is
/// refused by the store. Split off rather than added to every call site for
/// [`seed_price_keyed`]'s own stated reason — one body, so the row a suite
/// asserts about differs only in what it named.
pub async fn seed_price_keyed_with_horizon(
    harness: &Harness,
    plan_id: Uuid,
    region: &str,
    price_eligibility: PriceEligibility,
    cohort: Cohort,
    grandfather_until: Option<OffsetDateTime>,
) -> PriceRecord {
    let key = ScopeKey::new(
        PlanId::new(plan_id),
        CurrencyCode::new("USD").expect("currency"),
        Region::new(region).expect("region"),
        seeded_phase(),
        price_eligibility,
        ChargeKind::Recurring,
        cohort,
    )
    .expect("scope key");
    harness
        .state
        .prices
        .create_draft(
            &harness.scope(),
            harness.tenant,
            NewPriceDraft {
                price_id: Uuid::now_v7(),
                scope_key: key,
                content: PriceContent {
                    row: PriceRow::new(ChargeKind::Recurring, Some(ModelKind::Flat)),
                    tax_inclusive: false,
                    tax_category_ref: None,
                    billing_timing: None,
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
                    rounding_policy_ref: None,
                    grandfather_until,
                    supersedes_price_id: None,
                },
                created_by: SEED_ACTOR,
                created_at_utc: at(10),
                correlation_id: TEST_CORRELATION,
            },
        )
        .await
        .expect("seed a draft price row")
}

/// [`seed_price`], with a real `amount_minor` set.
///
/// [`seed_price`]'s row leaves `amount_minor` unset — fine for every suite that
/// never diffs the row's content, and wrong for the one that does: `None` on a
/// `flat` row is a real `NotComputable("amount_minor")` per D-115's delta domain
/// (`domain::materiality::delta::amount_delta`), which is `alwaysMaterialTrigger`
/// whatever a threshold policy says. A suite asserting the **per-currency**
/// comparison — `inst-mat-percurrency`, `inst-mr-coalesce` — needs a row with an
/// operand for that comparison to have anything to compare.
pub async fn seed_priced_row(
    harness: &Harness,
    plan_id: Uuid,
    region: &str,
    amount_minor: i64,
) -> PriceRecord {
    seed_priced_row_on_phase(harness, plan_id, region, amount_minor, seeded_phase()).await
}

/// [`seed_priced_row`] on a **named** phase.
///
/// A row's key names a phase and `PHASE_UNCOVERED` refuses a publish whose phase
/// no recurring row covers, so a fixture pairing this row with a plan whose
/// phase was minted elsewhere — `seed_publishable_shape`'s, for one — has to say
/// which phase it means. The default is [`seeded_phase`] because most suites
/// seed both halves and never look.
pub async fn seed_priced_row_on_phase(
    harness: &Harness,
    plan_id: Uuid,
    region: &str,
    amount_minor: i64,
    phase: PhaseId,
) -> PriceRecord {
    let key = ScopeKey::new(
        PlanId::new(plan_id),
        CurrencyCode::new("USD").expect("currency"),
        Region::new(region).expect("region"),
        phase,
        PriceEligibility::AllSubscriptions,
        ChargeKind::Recurring,
        Cohort::None,
    )
    .expect("scope key");
    let mut row = PriceRow::new(ChargeKind::Recurring, Some(ModelKind::Flat));
    row.amount_minor = Some(MinorAmount::new(amount_minor).expect("a non-negative amount"));
    harness
        .state
        .prices
        .create_draft(
            &harness.scope(),
            harness.tenant,
            NewPriceDraft {
                price_id: Uuid::now_v7(),
                scope_key: key,
                content: PriceContent {
                    row,
                    tax_inclusive: false,
                    tax_category_ref: None,
                    // `Some("advance")`, `publishable_row`'s own reason: a
                    // fixture asserting the run's real apply — not only the
                    // materiality it decides against — needs a row the whole
                    // aggregate rule set actually passes.
                    billing_timing: Some("advance".to_owned()),
                    proration_contract: Some(ProrationContract {
                        billing_anchor_policy: BillingAnchorPolicy::CalendarMonth,
                        proration_basis: ProrationBasis::CalendarDaysActual,
                        credit_on_downgrade: false,
                    }),
                    // `ROUNDING_POLICY_UNRESOLVED` otherwise: the tenant this
                    // harness builds configures no default, so a row that
                    // wants a clean pass through the real aggregate rule set
                    // has to name its own — `publishable_row`'s reason.
                    rounding_policy_ref: Some("half_up".to_owned()),
                    grandfather_until: None,
                    supersedes_price_id: None,
                },
                created_by: SEED_ACTOR,
                created_at_utc: at(10),
                correlation_id: TEST_CORRELATION,
            },
        )
        .await
        .expect("seed a draft price row")
}

/// [`seed_priced_row`]'s `per_unit` sibling: a row whose money is a **rate**.
///
/// A separate fixture rather than a flag on [`seed_priced_row`], because after
/// D-311 the two kinds do not differ by a value — they differ by **which
/// column is NULL**. `check_amount_placement` requires `amount_minor` to be
/// absent and `unit_rate` to be present on a `per_unit` row and refuses the
/// reverse, so a `per_unit` fixture built by setting `amount_minor` would be a
/// row no publish accepts, and one built by setting both would be the "two
/// competing prices" the rule exists to forbid.
///
/// `rate_nano_minor` is the stored 10⁻⁹-minor-unit count, stated rather than
/// authored from a decimal literal: the suites that use this are pinning an
/// exact projected rate, and a literal would put a scaling step between the
/// number a test asserts and the number it seeded.
///
/// `quantity_source` is `subscription_seat_count` because `inst-mk-required`
/// makes a **non-usage** `per_unit` row declare where its quantity comes from,
/// and the seat count is the one answer that needs no second field beside it
/// (`manual` additionally requires `manual_quantity`).
pub async fn seed_per_unit_rate_row(
    harness: &Harness,
    plan_id: Uuid,
    region: &str,
    rate_nano_minor: i64,
) -> PriceRecord {
    let key = ScopeKey::new(
        PlanId::new(plan_id),
        CurrencyCode::new("USD").expect("currency"),
        Region::new(region).expect("region"),
        seeded_phase(),
        PriceEligibility::AllSubscriptions,
        ChargeKind::Recurring,
        Cohort::None,
    )
    .expect("scope key");
    let mut row = PriceRow::new(ChargeKind::Recurring, Some(ModelKind::PerUnit));
    row.unit_rate = Some(RateMinor::from_nano_minor(rate_nano_minor).expect("a non-negative rate"));
    row.quantity_source = Some(QuantitySource::SubscriptionSeatCount);
    harness
        .state
        .prices
        .create_draft(
            &harness.scope(),
            harness.tenant,
            NewPriceDraft {
                price_id: Uuid::now_v7(),
                scope_key: key,
                content: PriceContent {
                    row,
                    tax_inclusive: false,
                    tax_category_ref: None,
                    // `seed_priced_row`'s three clean-publish inputs, verbatim:
                    // a fixture asserting the run's real apply needs a row the
                    // whole aggregate rule set passes.
                    billing_timing: Some("advance".to_owned()),
                    proration_contract: Some(ProrationContract {
                        billing_anchor_policy: BillingAnchorPolicy::CalendarMonth,
                        proration_basis: ProrationBasis::CalendarDaysActual,
                        credit_on_downgrade: false,
                    }),
                    rounding_policy_ref: Some("half_up".to_owned()),
                    grandfather_until: None,
                    supersedes_price_id: None,
                },
                created_by: SEED_ACTOR,
                created_at_utc: at(10),
                correlation_id: TEST_CORRELATION,
            },
        )
        .await
        .expect("seed a draft per_unit price row")
}

/// Declare one **active** customer-group taxonomy value, straight through the
/// entity — `seed_window`'s reason: a membership-route suite that needs
/// `GROUP_UNKNOWN` to stay out of its way must not depend on the taxonomy
/// `PUT` route working, or a defect there would redden every membership
/// suite at once and none of them would say which rule broke.
pub async fn declare_customer_group(harness: &Harness, value: &str) {
    use bss_pricing::infra::storage::entity::customer_group_taxonomy;
    use toolkit_db::secure::SecureInsertExt;

    let conn = harness.db.conn().expect("conn");
    let row = customer_group_taxonomy::ActiveModel {
        tenant_id: sea_orm::ActiveValue::Set(harness.tenant),
        value: sea_orm::ActiveValue::Set(value.to_owned()),
        display_name: sea_orm::ActiveValue::Set(value.to_owned()),
        state: sea_orm::ActiveValue::Set("active".to_owned()),
    };
    customer_group_taxonomy::Entity::insert(row.clone())
        .secure()
        .scope_with_model(&AccessScope::allow_all(), &row)
        .expect("scope")
        .exec(&conn)
        .await
        .expect("declare the customer-group value");
}

/// Retire an already-declared customer-group value — [`declare_customer_group`]
/// then this, the "declared and then retired" sequence
/// `DomainError::GroupUnknown`'s own doc names as the interesting case: it is
/// not the same as never having declared the value, because
/// `inst-cg-taxonomy`'s retire guard is what makes retirement mean something.
pub async fn retire_customer_group(harness: &Harness, value: &str) {
    use bss_pricing::infra::storage::entity::customer_group_taxonomy;

    let conn = harness.db.conn().expect("conn");
    let updated = customer_group_taxonomy::Entity::update_many()
        .secure()
        .scope_with(&AccessScope::allow_all())
        .col_expr(
            customer_group_taxonomy::Column::State,
            Expr::value("retired"),
        )
        .filter(
            Condition::all()
                .add(customer_group_taxonomy::Column::TenantId.eq(harness.tenant))
                .add(customer_group_taxonomy::Column::Value.eq(value)),
        )
        .exec(&conn)
        .await
        .expect("retire the customer-group value");
    assert_eq!(
        updated.rows_affected, 1,
        "the seed must have moved exactly the one declared row"
    );
}

/// Seed one `scheduled` window on a price row, straight through the repository.
///
/// **The repository and not the route**, deliberately: a suite that needs a window
/// to exist in order to drive `PATCH`/`DELETE` must not depend on `POST` working,
/// or a single defect in the schedule path would redden every window suite at once
/// and none of them would say which rule broke.
///
/// The interval is dated **2099** and carries no relation to the wall clock, which
/// is the fixtures' standing rule: a window dated *today* makes the suite race the
/// activation sweep, and one dated relatively goes stale the day the test data
/// ages past it.
pub async fn seed_window(harness: &Harness, price_id: Uuid) -> Uuid {
    /// Far enough out that no wall clock reaches it and no sweep activates the
    /// row mid-suite. A **fact**, not a date derived from `OffsetDateTime::now_utc()`.
    const SEEDED_WINDOW_YEAR: i32 = 2099;
    let effective_from = utc_ymd_hms(SEEDED_WINDOW_YEAR, 1, 1, 0, 0, 0);
    let window_id = Uuid::now_v7();
    let conn = harness.db.conn().expect("conn");
    bss_pricing::infra::storage::repo::window_repo::schedule(
        &conn,
        &harness.scope(),
        bss_pricing::infra::storage::repo::window_repo::NewWindow {
            window_id,
            tenant_id: harness.tenant,
            price_id,
            effective_from,
            effective_to: None,
            reason_code: "seeded".to_owned(),
        },
        seed_stamp(),
    )
    .await
    .expect("seed a scheduled window");
    window_id
}

/// A `RowVersion`, so a suite never builds one from a magic integer inline.
pub fn version(value: u64) -> RowVersion {
    RowVersion::new(value)
}

/// The authenticated context a service call needs when no route is minting one.
///
/// The same builder the harness's own clients use, exposed so a suite driving
/// `WindowService` or `ApprovalService` directly asks with the identity a route
/// would have compiled rather than with a second, weaker spelling of it.
#[must_use]
pub fn security_context(principal: Uuid, tenant: Uuid) -> SecurityContext {
    ctx_for_principal(tenant, principal)
}

/// The authenticated context every client in this harness carries.
///
/// **`token_scopes` is a wildcard, and it hides nothing *today***: nothing under
/// `src/` reads the field, so no assertion in the corpus turns on it. The day one
/// does, this line would grant it universally and every refusal built on it would
/// be untestable while reading as covered — the shape a fixture-grant finding takes
/// every time. `module_test`'s `no_source_reads_token_scopes` is what surfaces that
/// day rather than leaving it to be discovered.
fn ctx_for_principal(tenant: Uuid, principal: Uuid) -> SecurityContext {
    SecurityContext::builder()
        .subject_id(principal)
        .subject_tenant_id(tenant)
        .subject_type(gts_id!("cf.core.security.subject_user.v1~"))
        .token_scopes(vec!["*".to_owned()])
        .build()
        .expect("authed SecurityContext must build")
}

/// The actor and instant every mutating repository call now records (D-135 - the
/// audit row commits inside the mutation's own transaction).
///
/// Public as `seed_stamp` so a suite can stage a repository-level mutation that
/// no route offers - the price-row edit that stages the approve→commit window
/// is one, because the authoring route would need its own tag round trip and
/// the suite's subject is the publish.
pub fn seed_stamp() -> bss_pricing::domain::audit::AuditStamp {
    stamp()
}

/// **A fixed instant, not the wall clock.** Every other instant this file seeds is
/// `at(hour)`, and the neighbouring `stamp_of` takes the instant explicitly; a
/// `OffsetDateTime::now_utc()` here put a different value on every audit row and shape mutation
/// the harness seeds, so no suite could assert the recorded instant by equality
/// and every fixture computed from it asserted something different each day it
/// ran.
fn stamp() -> bss_pricing::domain::audit::AuditStamp {
    stamp_of(uuid::Uuid::from_u128(0xac_10), at(10))
}

// ---------------------------------------------------------------------------
// The publishable seed, and reading the approval store back.
// ---------------------------------------------------------------------------

/// The revision and version a publishable seed left behind.
pub struct Publishable {
    /// The terminal phase this plan's rows are filed under.
    ///
    /// **Minted per plan**, which was once a workaround and is now merely the
    /// truthful shape. `pricing_plan_phase`'s primary key was
    /// `(phase_id, plan_revision)` and carried neither `plan_id` nor `tenant_id`,
    /// so two plans — of one tenant or of two — could not share a phase id at the
    /// same revision number, and a fixed `seeded_phase()` here made the second
    /// publishable plan in one test fail on a UNIQUE violation. **D-340
    /// (`pricing_plan_phase`) widened the key**, so a shared id would no longer
    /// collide; a phase per plan is kept because that is what a real catalog
    /// looks like, not because the schema forces it.
    pub phase: PhaseId,
    /// The open draft revision.
    pub revision: u64,
    /// Its row version — what the composite `If-Match` tag has to carry.
    pub version: RowVersion,
    /// The one price row the plan carries.
    pub price_id: Uuid,
}

impl Publishable {
    /// The composite plan-plane entity tag: the revision **and** its version.
    ///
    /// Built here rather than in each suite because a plan route's tag names
    /// both, and a test that quoted a bare version would be testing a
    /// precondition the surface does not offer.
    #[must_use]
    pub fn etag(&self) -> String {
        format!("\"{}-{}\"", self.revision, self.version.get())
    }
}

/// A plan whose **shape** the publish rule set passes, carrying **no price row at
/// all**.
///
/// The other half of [`seed_publishable_plan`], split out rather than copied
/// because a plan with no rows is a state the rule set genuinely admits — the
/// coverage rule ranges over the *billable* set, and an empty set has no key to
/// find uncovered — so it is the world an empty first publish happens in. One
/// description of the shape, two seeds over it.
pub struct PublishableShape {
    /// The terminal phase this plan's rows would be filed under.
    pub phase: PhaseId,
    /// The open draft revision.
    pub revision: u64,
    /// Its row version — what the composite `If-Match` tag has to carry.
    pub version: RowVersion,
}

impl PublishableShape {
    /// The composite plan-plane entity tag: the revision **and** its version.
    #[must_use]
    pub fn etag(&self) -> String {
        format!("\"{}-{}\"", self.revision, self.version.get())
    }
}

/// A plan the publish rule set passes on its shape alone: one evergreen terminal
/// phase, a descriptor set, a tier and a frequency, and nothing priced.
pub async fn seed_publishable_shape(harness: &Harness, plan_id: Uuid) -> PublishableShape {
    let plan = PlanId::new(plan_id);
    let phase = PhaseId::new(Uuid::now_v7());
    let scope = harness.scope();
    let created = harness
        .state
        .plans
        .create_draft(
            &scope,
            NewPlanDraft {
                plan_name: None,
                plan_id: plan,
                tenant_id: harness.tenant,
                created_by: SEED_ACTOR,
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
                correlation_id: TEST_CORRELATION,
            },
        )
        .await
        .expect("seed the publishable draft plan");

    let after_phases = harness
        .state
        .shapes
        .replace_phases(
            &scope,
            harness.tenant,
            plan,
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

    let after_descriptors = harness
        .state
        .shapes
        .set_descriptor_set(
            &scope,
            harness.tenant,
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

    PublishableShape {
        phase,
        revision: created.revision,
        version: after_descriptors.row_version,
    }
}

/// A plan the **whole** publish rule set passes: [`seed_publishable_shape`] plus
/// one flat recurring row carrying an amount and a rounding policy, and the
/// window `inst-wc-required` demands of it.
///
/// `seed_draft_plan` + `attach_shape` + `seed_price` do not add up to this and
/// deliberately are not changed to: those seeds exist so the authoring suites
/// can drive save-time refusals over half-finished drafts, which is the state
/// §4.2 says authoring is allowed to be in. A publish suite needs the other
/// state, and it is a different seed rather than a widened one.
pub async fn seed_publishable_plan(harness: &Harness, plan_id: Uuid) -> Publishable {
    seed_publishable_plan_with(
        harness,
        plan_id,
        |plan, phase| publishable_scope_key(plan, phase, "eu"),
        publishable_row(),
    )
    .await
}

/// [`seed_publishable_plan`] with the key and the content chosen by the caller.
///
/// The three seeds above and below it differ in **exactly** those two values and
/// in nothing else: the plan shape, the descriptor set, the authoring actor and
/// the coverage window `inst-wc-required` demands are the same facts every
/// publishable fixture needs. A third and fourth copy of that body would each be
/// free to drift from it, and a fixture whose window or descriptor set differs
/// from the one the positive control uses cannot serve as its comparison.
///
/// The key is a closure rather than a value because the phase it names is
/// created inside — a caller cannot build the key until this function has run.
pub async fn seed_publishable_plan_with(
    harness: &Harness,
    plan_id: Uuid,
    key_for: impl FnOnce(PlanId, PhaseId) -> ScopeKey,
    content: PriceContentAlias,
) -> Publishable {
    let plan = PlanId::new(plan_id);
    let scope = harness.scope();
    let shape = seed_publishable_shape(harness, plan_id).await;
    let phase = shape.phase;

    let price_id = Uuid::now_v7();
    harness
        .state
        .prices
        .create_draft(
            &scope,
            harness.tenant,
            NewPriceDraft {
                price_id,
                scope_key: key_for(plan, phase),
                content,
                created_by: SEED_ACTOR,
                created_at_utc: at(10),
                correlation_id: TEST_CORRELATION,
            },
        )
        .await
        .expect("author the price row");

    // `inst-wc-required`: the row cannot publish until its canonical scope key
    // holds an active or scheduled window. Twenty-four tests across
    // `rest_publish.rs` and `rest_approvals.rs` reach the publish route through
    // this one seed, and every one of them asserted a 202 that the rule refuses
    // without this.
    //
    // The decision is `common::schedule_coverage_window`'s and not this file's,
    // so the two `sqlite_*` seeds that owe the same window cannot drift from it.
    //
    // **Scheduled through the repository for isolation and speed, and no longer
    // because the route could not do it.** The sentence here used to read that
    // `POST …/prices/{priceId}/windows` "is not mounted: a seed cannot use a door
    // that does not exist yet", and it is withdrawn rather than edited: the route
    // is mounted and this suite's own cases drive it. What the route cannot do is
    // author *this* window, because a window mutation resolves the plan's current
    // revision and this seed's plan has not published yet — the seed exists to make
    // that publish possible. Reaching the route from here would also mean every
    // publish fixture in the crate depended on the schedule path, so one defect in
    // it would redden four suites at once with none of them naming the rule that
    // broke. `rest_support::seed_window` carries the same note for the same reason.
    let conn = harness.state.db.conn().expect("conn");
    crate::common::schedule_coverage_window(&conn, &scope, harness.tenant, price_id, stamp()).await;

    Publishable {
        phase,
        revision: shape.revision,
        version: shape.version,
        price_id,
    }
}

/// [`seed_publishable_plan`]'s `per_unit` sibling: the same plan shape, and a
/// row whose money is a **rate** rather than an amount.
///
/// A second seed rather than a parameter on [`seed_publishable_plan`], for
/// [`seed_per_unit_rate_row`]'s reason one level up: after D-311 the two kinds
/// do not differ by a value, they differ by **which column is NULL**, so there
/// is no flag that turns one into the other — `check_amount_placement` requires
/// `amount_minor` present on a `flat` row and **forbids** it on a `per_unit`
/// one.
///
/// [`seed_per_unit_rate_row`] is not reused either: it files its row under the
/// fixed `seeded_phase()` and a fixed `USD`, and what a synthesis fixture needs
/// is a row on **this plan's** phase and on the `(EUR, eu)` market
/// [`publishable_scope_key`] puts every publishable seed on — which is the
/// frozen key a `FrozenKey` names.
pub async fn seed_publishable_per_unit_plan(
    harness: &Harness,
    plan_id: Uuid,
    rate_nano_minor: i64,
) -> Publishable {
    seed_publishable_plan_with(
        harness,
        plan_id,
        |plan, phase| publishable_scope_key(plan, phase, "eu"),
        publishable_per_unit_row(rate_nano_minor),
    )
    .await
}

/// [`seed_publishable_plan`]'s **tiered** sibling: a `usage` `graduated` row
/// whose whole price is its band ladder.
///
/// `usage` and not `recurring`, and that is a rule rather than a taste:
/// `inst-mk-chargekind` makes `flat` and `per_unit` the only non-usage kinds, so
/// a recurring `graduated` row is unpublishable and a fixture built as one would
/// have failed at the driver rather than at an assertion.
///
/// The row carries **every** Slice-10 content column the kind admits — the
/// reservation pair, both typed floors and their fallback, the discount hook,
/// the level-aggregation set and its hold bound. That is
/// `sqlite_price_repo::graduated_content`'s discipline for its reason: a payload
/// builder that dropped one column would otherwise leave a hole that stays a
/// hole, and nothing here would read differently. It costs three extra
/// conformance variants (`level_aggregation`, `supersession_continuity`,
/// `reserved`), and the corpus registry opens all three for `graduated`.
///
/// It carries **no** proration contract and no `billingTiming`: both are
/// `inst-pi-required` / `inst-bt-required` obligations of a *recurring* row, and
/// the S6 members of the payload are read off the recurring fixtures instead.
pub async fn seed_publishable_tiered_usage_plan(
    harness: &Harness,
    plan_id: Uuid,
    bands: Vec<TierBand>,
) -> Publishable {
    seed_publishable_plan_with(
        harness,
        plan_id,
        |plan, phase| publishable_usage_scope_key(plan, phase, "eu"),
        publishable_tiered_usage_row(bands),
    )
    .await
}

/// [`seed_publishable_plan`]'s `manual`-quantity sibling: a non-usage `per_unit`
/// row whose quantity is authored on the row, under a `fixed_day` anchor.
///
/// Two facts in one fixture because one row is the only place they can both
/// stand: `quantitySource = manual` is authorable on a **non-usage** `per_unit`
/// row and nowhere else (`inst-mk-forbidden`), and `anchorDay` is a number only
/// under `fixed_day` — under `calendar_month`, which every other publishable
/// seed uses, the column is NULL and an assertion on it would pass against a
/// payload that never rendered the member at all.
pub async fn seed_publishable_manual_quantity_plan(
    harness: &Harness,
    plan_id: Uuid,
    rate_nano_minor: i64,
    manual_quantity: u64,
    anchor_day: u8,
) -> Publishable {
    seed_publishable_plan_with(
        harness,
        plan_id,
        |plan, phase| publishable_scope_key(plan, phase, "eu"),
        publishable_manual_quantity_row(rate_nano_minor, manual_quantity, anchor_day),
    )
    .await
}

/// A flat recurring row that passes the Slice-3 rule set.
#[must_use]
pub fn publishable_row() -> PriceContentAlias {
    let mut row = PriceRow::new(ChargeKind::Recurring, Some(ModelKind::Flat));
    row.amount_minor = Some(MinorAmount::new(9_900).expect("a non-negative amount"));
    PriceContentAlias {
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

/// [`publishable_row`]'s `per_unit` sibling: the money is a **rate**, and
/// `amount_minor` is left NULL because the placement matrix forbids it here.
///
/// `quantity_source` is `subscription_seat_count` for
/// [`seed_per_unit_rate_row`]'s reason: `inst-mk-required` makes a **non-usage**
/// `per_unit` row declare where its quantity comes from, and the seat count is
/// the one answer that needs no second field beside it.
#[must_use]
pub fn publishable_per_unit_row(rate_nano_minor: i64) -> PriceContentAlias {
    let mut row = PriceRow::new(ChargeKind::Recurring, Some(ModelKind::PerUnit));
    row.unit_rate = Some(RateMinor::from_nano_minor(rate_nano_minor).expect("a non-negative rate"));
    row.quantity_source = Some(QuantitySource::SubscriptionSeatCount);
    // Everything but the row is [`publishable_row`]'s, taken from it rather than
    // restated: the billing timing, the proration contract and the rounding
    // policy are the three inputs that make a seeded row publishable at all, and
    // a second copy of them here would be free to drift from the one the flat
    // seed uses — which is the row this fixture's positive control reads.
    let mut content = publishable_row();
    content.row = row;
    content
}

/// [`publishable_row`]'s **tiered** sibling: the money is the band ladder, and
/// both scalar money columns are left NULL because
/// `inst-mk-required`'s placement matrix forbids **both** on a `graduated` row.
///
/// That is what makes this fixture worth having: on a `flat` row the amount is a
/// price beside the bands, and on a `per_unit` row the rate is; here there is no
/// third column a payload could be carrying the price in, so a rendering that
/// omits the ladder omits the whole price.
///
/// `aggregationFunction = time_weighted` makes it a **level** row, which is what
/// obliges `maxHoldGranules` (`inst-la-hold`) and what makes
/// `reservationFlavor = capacity` the only legal flavor here (D-53).
#[must_use]
pub fn publishable_tiered_usage_row(bands: Vec<TierBand>) -> PriceContentAlias {
    let mut row = PriceRow::new(ChargeKind::Usage, Some(ModelKind::Graduated));
    row.bands = bands;
    row.meter = Some(USAGE_METER.to_owned());
    row.billing_granularity = Some(BillingGranularity::PerHour);
    row.tier_aggregation_window = Some(TierAggregationWindow::CalendarMonth);
    row.aggregation_function = Some(AggregationFunction::TimeWeighted);
    row.aggregation_granularity = Some(AggregationGranularity::Hour);
    row.max_hold_granules = Some(6);
    row.reserved_rate =
        Some(RateMinor::from_nano_minor(3_100_000_000_000).expect("a non-negative rate"));
    row.reservation_flavor = Some(ReservationFlavor::Capacity);
    row.min_qty_purchase = Some(7);
    row.min_qty_usage = Some(11);
    row.min_qty_usage_fallback = Some(MinQtyUsageFallback::Exception);
    row.discount_ref = Some("promo/spring".to_owned());
    PriceContentAlias {
        row,
        tax_inclusive: false,
        tax_category_ref: None,
        // Both absent by rule rather than by omission: `inst-bt-required` and
        // `inst-pi-required` oblige a **recurring** row, and `inst-bt-usage`
        // makes a usage line's timing a constant the author never gives.
        billing_timing: None,
        proration_contract: None,
        rounding_policy_ref: Some("half_up".to_owned()),
        grandfather_until: None,
        supersedes_price_id: None,
    }
}

/// [`publishable_per_unit_row`]'s `manual` sibling, under a `fixed_day` anchor.
///
/// `manual_quantity` is what `check_quantity_source` requires beside
/// `quantitySource = manual` — *"the fixed quantity is the whole of what
/// `manual` supplies"* — so on this row it is half the arithmetic: the rate is
/// the multiplier and this is the multiplicand.
///
/// `creditOnDowngrade` is `true` here against the flat seed's `false`, so the
/// member is pinned to a value rather than to whatever a builder might default
/// to. It pairs with a basis that can compute the credit, which is what
/// `inst-pi-credit-none` requires of the pair.
#[must_use]
pub fn publishable_manual_quantity_row(
    rate_nano_minor: i64,
    manual_quantity: u64,
    anchor_day: u8,
) -> PriceContentAlias {
    let mut content = publishable_per_unit_row(rate_nano_minor);
    content.row.quantity_source = Some(QuantitySource::Manual);
    content.row.manual_quantity = Some(manual_quantity);
    content.proration_contract = Some(ProrationContract {
        billing_anchor_policy: BillingAnchorPolicy::FixedDay(
            AnchorDay::new(anchor_day).expect("a day of the month"),
        ),
        proration_basis: ProrationBasis::CalendarDaysActual,
        credit_on_downgrade: true,
    });
    content
}

/// The meter a publishable usage row prices.
pub const USAGE_METER: &str = "api_calls";

/// [`publishable_scope_key`]'s usage sibling — the same market, on the usage
/// component of the charge-kind axis, with the `(meter, dimensionKey)` line a
/// usage row is keyed by.
#[must_use]
pub fn publishable_usage_scope_key(plan_id: PlanId, phase: PhaseId, region: &str) -> ScopeKey {
    ScopeKey::new(
        plan_id,
        CurrencyCode::new("EUR").expect("three letters"),
        Region::new(region).expect("a non-blank region"),
        phase,
        PriceEligibility::AllSubscriptions,
        ChargeKind::Usage,
        Cohort::None,
    )
    .expect("the class pairs with cohort none")
    .with_usage_line(
        Some(Meter::new(USAGE_METER).expect("a non-blank meter")),
        DimensionKey::none(),
    )
    .expect("a usage line names its meter")
}

/// The canonical scope key a publishable row sits on.
#[must_use]
pub fn publishable_scope_key(plan_id: PlanId, phase: PhaseId, region: &str) -> ScopeKey {
    ScopeKey::new(
        plan_id,
        CurrencyCode::new("EUR").expect("three letters"),
        Region::new(region).expect("a non-blank region"),
        phase,
        PriceEligibility::AllSubscriptions,
        ChargeKind::Recurring,
        Cohort::None,
    )
    .expect("the class pairs with cohort none")
}

/// Every approval record the caller's tenant holds, in `submitted_at` order.
///
/// Read with `AccessScope::allow_all()` for `price_rows`' reason: a test
/// asserting what a call did to the store must see what landed rather than what
/// the caller was allowed to see.
pub async fn approval_rows(harness: &Harness) -> Vec<approval::Model> {
    let conn = harness.db.conn().expect("conn");
    approval::Entity::find()
        .secure()
        .scope_with(&AccessScope::allow_all())
        .filter(Condition::all().add(approval::Column::TenantId.eq(harness.tenant)))
        .order_by(approval::Column::SubmittedAt, Order::Asc)
        .order_by(approval::Column::ApprovalId, Order::Asc)
        .all(&conn)
        .await
        .expect("read the approval store")
}

/// One approval record, by id, read outside the caller's scope.
pub async fn approval_row(harness: &Harness, approval_id: Uuid) -> ApprovalRecord {
    let conn = harness.db.conn().expect("conn");
    bss_pricing::infra::storage::repo::approval_repo::read(
        &conn,
        &AccessScope::allow_all(),
        harness.tenant,
        approval_id,
    )
    .await
    .expect("read the approval record")
    .expect("the record is there")
}

/// One bulk operation (a repricing run or an import), by id, read outside the
/// caller's scope and through the repository rather than a `GET` — the run's own
/// stored `state`, not a rendering of it. A suite asserting the two edges out of
/// `validating` (`inst-mr-coalesce`) needs this rather than the `GET` response:
/// the `GET` is a second, independent handler and reads the store correctly, but
/// this is the more direct readback and it is what makes an assertion about
/// "the stored run" literal rather than "what one more handler rendered".
pub async fn bulk_operation_row(
    harness: &Harness,
    operation_id: Uuid,
) -> bss_pricing::infra::storage::repo::BulkOperationRecord {
    let conn = harness.db.conn().expect("conn");
    bss_pricing::infra::storage::repo::bulk_repo::read(
        &conn,
        &AccessScope::allow_all(),
        harness.tenant,
        operation_id,
    )
    .await
    .expect("read the bulk operation store")
    .expect("the record is there")
}

/// The principal who proposes a threshold policy in
/// [`approve_threshold_policy`], and the independent one who approves it.
///
/// Two ids because `chk_pricing_approval_distinct_principals` is a real constraint and
/// D-10's whole content is that the two-person rule's own foundation takes two people.
/// They are deliberately unlike any suite's own actors, so a suite asserting on *its*
/// submitter never matches these.
pub const POLICY_PROPOSER: Uuid = Uuid::from_u128(0x_7005_ec01);
/// The independent `FinanceReviewer` of [`approve_threshold_policy`].
pub const POLICY_REVIEWER: Uuid = Uuid::from_u128(0x_7005_ec02);

/// Configure the tenant's approval-threshold policy **through the real surfaces**,
/// and carry it to effective by approving it with a second principal.
///
/// # Why a suite ever needs this
///
/// Without it every act this gear can perform is material, and a suite of nothing but
/// refusals passes against a service that refuses everything. `inst-mat-failsafe` is
/// the reason: a tenant with no configured policy has *every* change material, so the
/// commit arm of any mutation — a plan publish, a window schedule, a lengthening
/// `PATCH` — is unreachable until one currency has an entry.
///
/// # Why it goes through `PUT` + approve rather than writing the rows
///
/// `infra::threshold::effective_version` resolves the policy by finding the **approved
/// unit whose pin still matches**, so a fixture that inserted version rows directly
/// would install a policy the evaluator never sees, and every test built on it would
/// assert the fail-safe while claiming to assert the comparison. The two calls here
/// are the same two an operator makes.
///
/// `entries` is `(currency, absolute_minor)`. A bar of any size is *below* nothing —
/// what matters for a zero-delta act is only that the currency **has** an entry.
///
/// # Why the start is in the past
///
/// It was `2099-01-01T00:00:00Z`, and under D-188 that is a policy which is **not in
/// force**: a version is not effective before its authored `effectiveFrom`, so this
/// fixture would have installed a policy no publish and no window mutation could
/// see, and every suite built on it would have gone back to asserting the fail-safe
/// while claiming to assert the comparison — the exact failure this function's own
/// doc warns about one paragraph up.
pub async fn approve_threshold_policy(harness: &Harness, entries: &[(&str, i64)]) {
    approve_threshold_policy_from(harness, "2020-01-01T00:00:00Z", entries).await;
}

/// The tenant's current policy `ETag`, read the way a caller has to read it.
///
/// A helper rather than an inline `GET` because every policy `PUT` in every suite
/// needs one and there is no other way to get it: the tag is opaque by construction
/// (D-186), so a fixture cannot compute one and a fixture that hard-coded one would
/// pin the digest's framing instead of the precondition.
pub async fn policy_etag_of(harness: &Harness, principal: Uuid) -> String {
    let read = harness
        .allowed_as(principal)
        .send(with_headers(
            "GET",
            bss_pricing::api::rest::threshold_policy::APPROVAL_THRESHOLD_POLICY,
            None,
            &[],
        ))
        .await;
    assert_eq!(
        read.status(),
        axum::http::StatusCode::OK,
        "the policy read always answers, unset or not"
    );
    etag_of(&read).expect(
        "the policy GET carries the ETag its PUT demands; without it the precondition is \
         unsatisfiable",
    )
}

/// [`approve_threshold_policy`] over an authored start, for the suites whose
/// subject is the start itself.
pub async fn approve_threshold_policy_from(
    harness: &Harness,
    effective_from: &str,
    entries: &[(&str, i64)],
) {
    let body = serde_json::json!({
        "effective_from": effective_from,
        "entries": entries
            .iter()
            .map(|(currency, minor)| serde_json::json!({
                "currency": currency,
                "absolute_minor": minor,
            }))
            .collect::<Vec<_>>(),
    });
    // The `PUT` requires the policy's `ETag` (D-186), and the read that supplies it
    // is the same read an operator makes: there is no tag to fabricate, because the
    // value is an opaque digest over the representation the `GET` serves. A tenant
    // with no policy at all is answered 200 and carries one, which is what makes this
    // fixture work on a fresh harness.
    let tag = policy_etag_of(harness, POLICY_PROPOSER).await;
    let proposed = harness
        .allowed_as(POLICY_PROPOSER)
        .send(with_headers(
            "PUT",
            bss_pricing::api::rest::threshold_policy::APPROVAL_THRESHOLD_POLICY,
            Some(body),
            &[("if-match", tag.as_str())],
        ))
        .await;
    assert_eq!(
        proposed.status(),
        axum::http::StatusCode::ACCEPTED,
        "the D-10 unit opens; the diff is not applied yet"
    );
    let opened = body_json(proposed).await;
    let approval_id = opened["approval"]["approval_id"]
        .as_str()
        .and_then(|raw| Uuid::parse_str(raw).ok())
        .unwrap_or_else(|| panic!("the 202 names the unit it opened: {opened}"));
    let approved = harness
        .allowed_as(POLICY_REVIEWER)
        .send(with_headers(
            "POST",
            &format!("/bss-pricing/v1/approvals/{approval_id}/approve"),
            None,
            &[],
        ))
        .await;
    assert_eq!(
        approved.status(),
        axum::http::StatusCode::OK,
        "an independent principal is what puts the policy in force (D-10)"
    );
}

/// Read one entity's rows for **both** of the harness's tenants into the plane map.
///
/// A macro and not a generic function: every entity carries its own `Column` enum
/// and `SeaORM`'s `EntityTrait` offers no bound that names `TenantId` across them,
/// so there is nothing for a type parameter to stand for.
///
/// Both tenants, because the caller is one of them. A refusal probe drives the
/// route as `harness.other_tenant()`, and a handler that wrote before resolving
/// its object writes under whichever tenant the *request* named — so a plane read
/// for `harness.tenant` alone leaves every write a refusing handler makes on its
/// own caller's side of the schema outside the measurement, while the assertion
/// above it says "no plane moved".
macro_rules! plane {
    ($out:expr, $conn:expr, $harness:expr, $entity:ident) => {{
        use bss_pricing::infra::storage::entity::$entity as e;
        let rows = e::Entity::find()
            .secure()
            .scope_with(&AccessScope::allow_all())
            .filter(
                Condition::all().add(e::Column::TenantId.is_in([$harness.tenant, $harness.other])),
            )
            .all($conn)
            .await
            .unwrap_or_else(|err| panic!("read the {} plane: {err}", stringify!($entity)));
        let mut rendered: Vec<String> = rows.iter().map(|row| format!("{row:?}")).collect();
        // Sorted, because a plane is a set here: two reads of one unchanged table
        // may order rows differently and that is not a write.
        rendered.sort();
        $out.insert(stringify!($entity), rendered);
    }};
}

/// The plane map this file builds, keyed by entity module.
type Planes = std::collections::BTreeMap<&'static str, Vec<String>>;

/// The plan and everything filed under a revision of it.
async fn planes_of_the_plan_aggregate(harness: &Harness, out: &mut Planes) {
    let conn = harness.db.conn().expect("conn");
    plane!(out, &conn, harness, plan);
    plane!(out, &conn, harness, plan_phase);
    plane!(out, &conn, harness, plan_addon_rule);
    plane!(out, &conn, harness, plan_descriptor_set);
    plane!(out, &conn, harness, plan_period_floor_cap);
    plane!(out, &conn, harness, composite_meter);
}

/// The price row, its bands and its windows.
async fn planes_of_the_price_aggregate(harness: &Harness, out: &mut Planes) {
    let conn = harness.db.conn().expect("conn");
    plane!(out, &conn, harness, price);
    plane!(out, &conn, harness, price_tier_band);
    plane!(out, &conn, harness, price_window);
}

/// The discount and composition planes.
async fn planes_of_overlays_and_bundles(harness: &Harness, out: &mut Planes) {
    let conn = harness.db.conn().expect("conn");
    plane!(out, &conn, harness, price_overlay);
    plane!(out, &conn, harness, price_overlay_line);
    plane!(out, &conn, harness, price_overlay_line_amount);
    plane!(out, &conn, harness, bundle);
    plane!(out, &conn, harness, bundle_component);
    plane!(out, &conn, harness, bundle_revshare);
    plane!(out, &conn, harness, bundle_revshare_group);
}

/// The tenant's vocabularies, its memberships and its config objects.
async fn planes_of_taxonomies_and_policy(harness: &Harness, out: &mut Planes) {
    let conn = harness.db.conn().expect("conn");
    plane!(out, &conn, harness, brand_taxonomy);
    plane!(out, &conn, harness, customer_group_taxonomy);
    plane!(out, &conn, harness, org_tier_taxonomy);
    plane!(out, &conn, harness, partner_taxonomy);
    plane!(out, &conn, harness, region_taxonomy);
    plane!(out, &conn, harness, rounding_policy_taxonomy);
    plane!(out, &conn, harness, gl_code_taxonomy);
    plane!(out, &conn, harness, group_membership);
    plane!(out, &conn, harness, policy_object);
}

/// The approval register, its key reservations, the threshold versions and the trail.
async fn planes_of_governance(harness: &Harness, out: &mut Planes) {
    let conn = harness.db.conn().expect("conn");
    plane!(out, &conn, harness, approval);
    plane!(out, &conn, harness, approval_key);
    plane!(out, &conn, harness, approval_threshold);
    plane!(out, &conn, harness, approval_threshold_tombstone);
    plane!(out, &conn, harness, audit_log);
}

/// The run planes, the outbox, the projections and the singletons.
async fn planes_of_operations_and_projections(harness: &Harness, out: &mut Planes) {
    let conn = harness.db.conn().expect("conn");
    plane!(out, &conn, harness, bulk_operation);
    plane!(out, &conn, harness, bulk_row_lock);
    plane!(out, &conn, harness, migration);
    plane!(out, &conn, harness, repricing_journal);
    plane!(out, &conn, harness, catalog_version_ref);
    plane!(out, &conn, harness, idempotency_dedup);
    plane!(out, &conn, harness, operator_flag);
    plane!(out, &conn, harness, outbox);
    plane!(out, &conn, harness, pin_frontier);
    plane!(out, &conn, harness, read_model);
    plane!(out, &conn, harness, snapshot_provenance);
}

/// Every tenant-scoped plane of the schema, as `entity module -> ordered row
/// renderings`, over the harness's tenant **and** its `other`.
///
/// **What a denied call must leave untouched**, and over the whole schema rather
/// than over a chosen part of it. The readback this joins named three planes —
/// `plan_count`, `plan_row_version`, `price_rows` and the approval unit — while most
/// of the mutating census routes write a fourth. The argument the loop's own doc
/// makes for the approval readback ("a handler that decided the unit and then
/// checked the gate moves no plan row and no price row, so every assertion below
/// would hold") applies verbatim to every plane it omitted.
///
/// Rows are rendered with `Debug` rather than compared field by field, so a change
/// to **any** column of any row moves the string. That is deliberate: a
/// hand-enumerated column list is the defect this helper exists to close, one level
/// down. The plane roster is [`bss_pricing::infra::storage::entity::MODULES`], for
/// the same reason one level up — a hand-maintained plane list makes "the denied
/// call wrote nothing" a claim about the tables somebody remembered.
///
/// # Every entity, and not only the ones a route addresses
///
/// A plane whose rows no census route can write costs one empty read and buys the
/// case a table nobody expected to be reachable: `audit_log` and `outbox` are
/// written as a *side effect* of an act, so a denial that produced one is a handler
/// that performed the act. Choosing the reachable subset instead means choosing it
/// again, correctly, after every schema change.
///
/// # The six groups are a lint bound and judge nothing
///
/// One function reading forty planes is past clippy's cognitive-complexity cap, so
/// the roster is split — by schema area, so a reader looking for a table has
/// somewhere to look, and with the coverage assertion below in the parent so no
/// group can be forgotten.
///
/// # Reading the schema itself is not reachable from a test
///
/// A roster read from `sqlite_master` would be better still and is not available:
/// `toolkit-db` seals raw `SeaORM` access from downstream crates by design
/// ("downstream crates must never see or name `ConnectionTrait`,
/// `DatabaseConnection`"), and the harness database is `sqlite::memory:`, so a
/// second connection opened alongside it is a different, empty database. Recorded
/// so the next reader does not re-derive the dead end.
pub async fn mutable_planes(harness: &Harness) -> Planes {
    let mut out = Planes::new();

    planes_of_the_plan_aggregate(harness, &mut out).await;
    planes_of_the_price_aggregate(harness, &mut out).await;
    planes_of_overlays_and_bundles(harness, &mut out).await;
    planes_of_taxonomies_and_policy(harness, &mut out).await;
    planes_of_governance(harness, &mut out).await;
    planes_of_operations_and_projections(harness, &mut out).await;

    // **The roster is the crate's and not this file's.** `entity::MODULES` comes
    // from the macro that declares the modules, so a table added to the schema has
    // no plane here until somebody adds one — instead of reading as a table the
    // denied call left alone.
    let declared: std::collections::BTreeSet<&str> = bss_pricing::infra::storage::entity::MODULES
        .iter()
        .copied()
        .collect();
    let covered: std::collections::BTreeSet<&str> = out.keys().copied().collect();
    assert_eq!(
        covered, declared,
        "every entity module owes this map a plane; a denied call that wrote an unlisted table \
         is indistinguishable here from one that wrote nothing"
    );

    out
}

/// The two refusals a foreign caller must not be able to tell apart on a
/// **by-id write**, plus the proof that no plane of the schema moved between
/// them.
///
/// `at_real` addresses an object of `harness.tenant`; `at_absent` addresses an id
/// nothing points at. Both are sent as [`Harness::other_tenant`], whose PDP double
/// *allows* and constrains `owner_tenant_id` to the other tenant — so the gate
/// passes and the compiled scope's SQL predicate is the only thing left that can
/// refuse. [`Harness::denied`] and [`Harness::scope_mismatch`] hand no
/// caller-supplied id of another tenant's row to a repository at all, which is why
/// neither exercises that predicate.
///
/// Two claims, because each alone is satisfied by a broken other:
///
///   * the refusal is **indistinguishable** from an absent id, in status and in the
///     problem `type` — a `409` for another tenant's row and a `404` for a random
///     id is a probe for which uuids name real rows. Both are named rather than
///     merely compared: `404` is the answer, and the `type` must be **present**,
///     because two absent bodies compare equal and satisfy an agreement test with
///     nothing behind it;
///   * **no plane moved**, over [`mutable_planes`]'s whole roster and both of the
///     harness's tenants. A handler that resolved its object before narrowing would
///     write the row **and** its audit record and its outbox row, so a case reading
///     the route's own plane alone would still see two of the three survive a later
///     fix. Both tenants because the caller is the *other* one: a handler that
///     writes before it resolves writes under the tenant the request named.
///
/// **The third claim is the caller's**, and it belongs after this call: an owner's
/// identical request that is *accepted*. Without it a route refusing every caller
/// — a stale precondition, a body its rules reject — reads as tenant isolation.
/// It is last because it succeeds, and a success writes.
pub async fn foreign_is_indistinguishable(
    harness: &Harness,
    at_real: Request<Body>,
    at_absent: Request<Body>,
) {
    let before = mutable_planes(harness).await;

    let foreign = harness.other_tenant().send(at_real).await;
    let absent = harness.other_tenant().send(at_absent).await;

    let foreign_status = foreign.status();
    let absent_status = absent.status();
    assert_eq!(
        foreign_status,
        axum::http::StatusCode::NOT_FOUND,
        "another tenant's object must answer exactly as an id that names nothing does, and this \
         is that answer named rather than inferred: `!is_success()` alone is satisfied by a \
         handler that 500s, and two handlers that 500 are indistinguishable from each other \
         while telling the caller the row exists and something broke reaching it"
    );
    assert_eq!(
        foreign_status, absent_status,
        "another tenant's object answers {foreign_status} and an absent id {absent_status}; the \
         difference is a probe for which ids name real rows"
    );
    let foreign_type = body_json(foreign).await.get("type").cloned();
    let absent_type = body_json(absent).await.get("type").cloned();
    assert!(
        foreign_type.is_some(),
        "the refusal must be an RFC-9457 problem carrying a `type`; without this the comparison \
         below is two `None`s, which agree however the two handlers answered"
    );
    assert_eq!(
        foreign_type, absent_type,
        "and alike in the problem `type`, or the body distinguishes what the status does not"
    );

    let after = mutable_planes(harness).await;
    for (plane, rows) in &before {
        assert_eq!(
            after.get(plane),
            Some(rows),
            "a refused foreign caller wrote the {plane} plane"
        );
    }
}
