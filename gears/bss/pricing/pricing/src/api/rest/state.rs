//! Per-request state for the authoring surface, built once in `init()`.
//!
//! One struct for the plan plane and the price plane together, rather than one
//! per module, because the two share a transaction seam and a gate: a
//! `POST …/plans/{planId}/prices` reads the plan repository to check the row's
//! parent and writes through the price repository under the idempotency gate, so
//! splitting the state would mean two `Extension`s wired identically and a
//! handler picking the wrong one being a compile error nobody sees until a route
//! is added.
//!
//! The repositories are cheap clones over the same provider (each holds only a
//! `DBProvider`), so carrying four of them costs nothing beyond the handle. `db`
//! is here for [`crate::infra::idempotent`], which is the one thing on this
//! surface that opens a transaction of its own.

use toolkit_db::{DBProvider, DbError};

use crate::infra::approval::ApprovalService;
use crate::infra::publish::PublishService;
use crate::infra::storage::repo::{
    BundleRepo, IdempotencyGate, OverlayRepo, PlanRepo, PlanShapeRepo, PriceRepo,
};

/// The authoring surface's dependencies, shared via
/// `Extension<Arc<AuthoringState>>` exactly as
/// [`frontier::ApiState`](crate::api::rest::frontier::ApiState) is.
#[derive(Clone)]
pub struct AuthoringState {
    /// The provider the transaction seam opens its one transaction on.
    pub db: DBProvider<DbError>,
    /// The plan revision chain.
    pub plans: PlanRepo,
    /// The revision-scoped child shape tables (phases, add-on rules, the
    /// descriptor set).
    pub shapes: PlanShapeRepo,
    /// Draft price rows and their bands.
    pub prices: PriceRepo,
    /// The bundle and its composition (`design/08-bundles.md` §6).
    pub bundles: BundleRepo,
    /// The composition's assemble/validate/publish seam — the reads
    /// `domain::bundle_rules` is deliberately kept from making for itself.
    pub bundle_service: crate::infra::bundle::BundleService,
    /// Slice 9's overlay store and its revision chain
    /// (`design/09-price-overlays.md` §6).
    ///
    /// On the **authoring** state and not the governance one, although the
    /// submit is always material (D-50): the criterion that split those two is
    /// which of them may request a `CatalogVersion`, and this repository
    /// requests none. The approval unit the submit opens is `GovernanceState`'s
    /// to hold when it is wired.
    pub overlays: OverlayRepo,
    /// The approval service, for the one authoring route that opens a unit.
    ///
    /// The overlay **submit** is always material (D-50), so it opens a Slice 5 unit
    /// before it answers — and it is an authoring route, so it is mounted here rather
    /// than on `GovernanceState` where the decide/withdraw surfaces live. Carrying the
    /// service is what keeps the transaction boundary in a service instead of putting
    /// one in a handler, which no route in this gear does.
    pub approvals: crate::infra::approval::ApprovalService,
    /// Slice 4's four scope-value taxonomies — the `config` plane's own store.
    ///
    /// Here rather than on [`GovernanceState`] for [`AuthoringState::overlays`]'
    /// reason and the criterion that split the two: a taxonomy `PUT` requests no
    /// `CatalogVersion`. It opens no approval unit either — taxonomy mutation is
    /// `CatalogAdmin` config (§10), audited, and not one of D-10's always-material
    /// acts — so nothing pulls it toward the governance plane.
    pub taxonomies: crate::infra::storage::repo::taxonomy_repo::TaxonomyRepo,
    /// The at-most-once gate, holding the configured retention window.
    pub idempotency: IdempotencyGate,
}

/// The governance surface's dependencies — the approval routes and the publish
/// mount, shared via `Extension<Arc<GovernanceState>>`.
///
/// One struct for the two, for [`AuthoringState`]'s reason: they are one seam
/// rather than two. `POST /plans/{planId}/publish` reads the plan repository to
/// resolve the revision under the caller's `If-Match`, asks the approval
/// workflow whether that exact revision carries an approved unit, and then hands
/// the publish engine the authorization it minted from it — so a split would
/// mean three `Extension`s wired identically and a handler reaching for the
/// wrong one being a compile error nobody sees until a route is added.
///
/// It is deliberately **not** folded into [`AuthoringState`]. That one is the
/// draft plane: four repositories and an idempotency gate, all cheap clones over
/// one provider. This one carries the services that hold the catalog-version
/// registry handle, and the authoring routes must not be able to reach one: a
/// version request is what decides what this deployment may freeze.
///
/// **The engine is not the only requester of a `CatalogVersion`, and that is
/// still one incrementer.** D-99 makes every window *mutation* a publish unit
/// running the same engine path, and the acts that followed it are publish units
/// too; the fields below whose own docs say they request one are the roster. What
/// matters is
/// unchanged: the registry handle stays inside this state, every requester takes
/// the **same** `Arc`, and none invents a version locally. Many requesters of one
/// registry is one incrementer; two *incrementers* is what `pricing-sdk`'s
/// registry contract refuses, and nothing here is one.
///
/// They are kept apart by their **request id** and not by hope: the first segment
/// names the act — `PublishUnitKind::request_token` gives a plan publish
/// `plan-publish/...` and a window mutation `window-mutation/<window_id>/...`, and
/// each field below states its own — so a retry finds its own pending handle and
/// never another's.
#[derive(Clone)]
pub struct GovernanceState {
    /// The provider the publish subject is assembled over. Its own read, not
    /// the engine's: `PublishService::precheck` answers a report and the
    /// materiality evaluator needs the shape.
    pub db: DBProvider<DbError>,
    /// The lane an approved repricing run's apply is handed to
    /// ([`crate::infra::repricing::RunApplyLane`]).
    ///
    /// Not optional, and that is the point: a state built without a lane would be a
    /// surface that approves a run and applies none, silently. What a process with no
    /// `bss-pricing` lifecycle gets instead is a lane whose applier nothing spawned —
    /// the enqueue says so at `error`, and a harness drives
    /// [`crate::infra::repricing::RunApplyWorker::drain_pending`] itself.
    pub apply_lane: crate::infra::repricing::RunApplyLane,

    /// The plan revision chain — the publish route resolves its subject here.
    pub plans: PlanRepo,
    /// Published price rows, for `inst-mat-newrow`'s baseline.
    pub prices: PriceRepo,
    /// The approval workflow: open a pinned unit, read it, decide it.
    pub approvals: ApprovalService,
    /// Live participant names on approval GETs, independently authorized by AM.
    pub participants: crate::infra::approval_participants::ApprovalParticipants,
    /// The publish engine. Its `commit` is the act an approval authorizes, and it
    /// requests the `CatalogVersion` the rest are measured against; its handle's
    /// first segment is `plan-publish/…`.
    pub publish: PublishService,
    /// Slice 9's overlay store, on **this** state since D-234's route arm.
    ///
    /// It is also on [`AuthoringState`], and the duplication is deliberate rather
    /// than sloppy: the authoring routes author drafts and the submit route
    /// publishes them, and a repository is a handle on a provider rather than
    /// state, so two holders is two readers of one store.
    pub overlays: OverlayRepo,
    /// The overlay plane's publish commit (D-234).
    ///
    /// **This is why the submit route moved here.** This state's own criterion —
    /// which of the two may request a `CatalogVersion` — is what put the overlay
    /// routes on `AuthoringState` in the first place, when the submit had one act
    /// and requested none. Gaining the publish arm is exactly the condition the
    /// criterion names, so the route moved rather than the criterion bending; the
    /// plan plane's identical two-act route (`api::rest::publish`) has always been
    /// here, and it requests a `CatalogVersion` like the plan plane's; its handle's
    /// first segment is `overlay-content/…`, from this module's own
    /// `publish_request_id` rather than from `PublishUnitKind::request_token`.
    pub overlay_publish: crate::infra::overlay_publish::OverlayPublishService,
    /// The window mutation workflow — the three §5 surfaces, each a publish unit
    /// (D-99).
    ///
    /// Here and not on [`AuthoringState`] because it requests a
    /// `CatalogVersion`, which is the whole criterion this split is drawn on. It
    /// shares the registry `Arc` with [`GovernanceState::publish`]; the module doc
    /// says why that is one incrementer and not two.
    pub windows: crate::infra::window::WindowService,
    /// The supersession unit's workflow — `POST …/plans/{planId}/supersessions`
    /// (D-88), the interactive repricing of one published canonical scope key.
    ///
    /// Here for [`GovernanceState::windows`]' reason, and a requester of the one
    /// registry `Arc`. The module doc's argument covers it without amendment: it
    /// invents no version, and its handle's first segment is `supersession/…`. That
    /// segment does not come from `PublishUnitKind::request_token`, because S5 §6
    /// declares no `supersession` subject kind and this gear mints no token the
    /// design set has not;
    /// `infra::supersession::unit_request_id` carries that argument.
    pub supersessions: crate::infra::supersession::SupersessionService,
    /// The grandfathering cutover's workflow — `POST …/plans/{planId}/cutovers`
    /// (D-100), the atomic reprice-plus-retain of one canonical scope key.
    ///
    /// A requester of the one registry `Arc`, and the field above's argument covers
    /// it again without amendment: it invents no version, and its handle's first
    /// segment is `cutover/…`, from `infra::cutover`'s own builder for
    /// `unit_request_id`'s reason.
    pub cutovers: crate::infra::cutover::CutoverService,
    /// The grandfathering **horizon** door — `PATCH …/prices/{priceId}/grandfather-until`
    /// (S7 §4's `inst-gs-bound`/`inst-gs-tighten`), the only writer of
    /// `grandfather_until` on a published row.
    ///
    /// A requester of the one registry `Arc`, and the field above's argument covers
    /// it once more: it invents no version, and its handle's first segment is
    /// `grandfather-until/…`. It is a service of its own rather than a
    /// method on [`GovernanceState::cutovers`] although its route is mounted in the
    /// cutover's module — the surfaces are one story and the transactions are not,
    /// a cutover composing eleven steps across three planes while this one moves a
    /// column and re-projects a subject.
    pub grandfather: crate::infra::grandfather::GrandfatherService,
    /// The retirement orchestrator — `POST …/plans/{planId}/retire` (D-128,
    /// Slice 11), the plan's terminal flip.
    ///
    /// A requester of the one registry `Arc`, and the argument above covers it once
    /// more without amendment: retirement is a publish unit, so it requests a
    /// version like its neighbours and invents none. Its request
    /// id's first segment is the retirement's own subject
    /// (`<plan_id>/retirement/<revision>`), which is what keeps its handle apart
    /// from theirs.
    pub retirements: crate::infra::retirement::RetirementService,
    /// The `MigrationScheduler` of Slice 11 §1.7 — schedule, read and cancel a
    /// plan migration (`inst-ms-api`, `inst-mg-cancel`, D-34).
    ///
    /// Requests **no** `CatalogVersion`, unlike the requesters above: a
    /// migration schedule mutates no plan content and projects no subject, so
    /// there is nothing for a consumer to pin. It emits a schedule and
    /// Subscriptions executes it.
    pub migrations: crate::infra::migration::MigrationService,
    /// The `SnapshotSynthesizer` of Slice 11 §1.7 — D-76's two-tier selection,
    /// D-87's self-contained payload and the D-102 read surface.
    ///
    /// Requests **no** `CatalogVersion` either, and here the reason is normative
    /// rather than incidental: D-87 makes a `migrated-origin` ref the one
    /// deliberately non-version-pinned reference in the system, so there is no
    /// version for it to request.
    pub synthesis: crate::infra::synthesis::SynthesisService,
    /// The at-most-once gate the `POST …/windows` claims under (D-191).
    ///
    /// Here as well as on [`AuthoringState`] rather than instead of it: the two planes
    /// claim under different `operation` tokens over the same table, so one gate value
    /// shared by both would still key them apart, and a second field is what keeps each
    /// state carrying the collaborators its own routes reach for. The window `POST` was
    /// the last mutating create in this gear with **no** gate — it parsed the required
    /// `Idempotency-Key` and dropped it.
    pub idempotency: IdempotencyGate,
    /// The approval-threshold policy: the effective version, and the proposal that
    /// opens a D-10 unit.
    ///
    /// Here rather than on [`AuthoringState`] because it is governance twice over:
    /// its `PUT` gates on `approval_policy × write`, which the authoring roles do
    /// not hold, and its proposal opens an approval unit through the same store
    /// `approvals` writes. It requests no `CatalogVersion`, so it does not bear on
    /// the split's own criterion either way.
    pub thresholds: crate::infra::threshold::ThresholdService,
    /// What the catalog reports about itself (`T-17`).
    ///
    /// Here rather than on [`AuthoringState`] because the two surfaces that
    /// report today are governance-plane reads — the base-price preview's
    /// fail-closed counter and the publish path's currency-binding and GA-gate
    /// instruments. A later slice reporting from an authoring route adds the
    /// field there; the port is the same one.
    pub metrics: std::sync::Arc<dyn crate::domain::ports::metrics::PricingMetricsPort>,
}
