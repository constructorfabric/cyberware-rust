//! What a `CatalogVersion` freezes for one **plan subject** — the value, and
//! the payload it renders into `pricing_read_model.payload`.
//!
//! A version's rows are per-subject deltas (D-86, subject-typed by D-91), and
//! this module is the `plan` kind's content: the plan's current revision, its
//! revision-scoped children (D-83) and the price rows the version publishes.
//! Pure vocabulary and rendering — no storage, no scope, no I/O. The projector
//! that reads truth rows and writes this value lives in
//! [`crate::infra::read_model`], because it holds an `AccessScope` and
//! repositories and dylint DE0301 forbids those here.
//!
//! # No document declares this payload's field list. **Reported.**
//!
//! `01-foundation.md` §4.4 states what a plan-subject delta must *carry* — the
//! window facts (D-121) and the revision's `lifecycle_state`, which is what
//! sellability predicate (4) reads at the pin (D-128) — and §3.7 states how the
//! row is *keyed*. Neither names a schema, and §3.1's one-line sketch of the
//! `ReadModel` aggregate (`{skuId, planId, priceId}`, model kind, ordered tier
//! bands, evaluation-policy fields, descriptors) is a sketch. The wire keys
//! below are therefore **this module's**, written down once so a later document
//! has something concrete to contradict — exactly the posture
//! [`outbox_repo`](crate::infra::storage::repo::outbox_repo) took for
//! `PlanPublished`, and for the same reason.
//!
//! They are `camelCase`, the spelling the design set uses for consumer-visible
//! fields (`pricingSnapshotRef`, `planId`, `availableFrom`), and they are built
//! with `json!` rather than a `Serialize` derive so the wire vocabulary is
//! visibly not the Rust field names and lives in exactly one place.
//!
//! # What this payload does not carry, and whose it is
//!
//! A version this gear produces today is **incomplete against §4.4**, and the
//! incompleteness is declared here rather than left for a reader to discover.
//! One line per absent fact, with the slice that owns it:
//!
//! - **The GA-gate flags** `not_sellable_ga` and the prepaid-execution gate
//!   (Slice 4 / Slice 10) — sellability predicate (5). Neither has a store
//!   here.
//! - **The registry `sellable` flag** per offered SKU (D-46) — predicate (6).
//!   It is the registry's fact, frozen per version registry-side, and the
//!   registry gear has no code in this repository.
//!
//! **A version answers four of `inst-sg-pinned`'s six predicates**, and the
//! arithmetic is stated against the list above. Predicate (1) — "an active window
//! covers `t` with the D-80 coverage horizon" (`07-pricewindow-linkage.md`
//! `inst-sg-surface`) — is answerable from the pin because
//! [`PlanSubjectDelta::windows`] carries the intervals and states grouped per
//! canonical scope key. The D-121 **horizon** `H` is a separate absence, and its
//! premise is stated where the projector applies it.
//!
//! The Slice-6 grant set is not on the list either: [`PlanSubjectDelta::entitlement_grants`]
//! carries the authored set and `phaseGrantMap` carries the complete map, so a
//! consumer resolves the active phase at `t` by one lookup. Its store is the
//! column `pricing_plan.entitlement_grants`, **not** `pricing_plan_grant` — that
//! table is Slice 10's (D-52) and holds D-43's prepaid **credit** grant, a
//! different object with a different lifecycle. Reading the two as one builds a
//! Slice-6 requirement inside a Slice-10 aggregate.
//!
//! The Slice-6 **cross-boundary contract** is not on it either:
//! [`CROSS_BOUNDARY_CHANGE_POLICY`] is stamped on every resolved plan subject, and
//! its other half is not a field at all — see that constant's own doc for why.
//!
//! # The one Slice-10 primitive still nothing judges
//!
//! [`row_value`] renders `tierQualificationWindow` and `includedAllowance` into
//! the delta. The **allowance half is now judged and compiled** — `inst-ac-gate`'s
//! six refusals are registered row-local rules and
//! [`crate::domain::allowance::compile`] materializes the `$0` band, the offset
//! ladder and the marker into this very payload — with one half still owed:
//! `rolloverPolicy = carry`, whose artifact is a `pricing_plan_grant` row
//! (`inst-ac-carry`, D-52) and whose table this gear does not have.
//!
//! `tierQualificationWindow` is untouched: not one of `inst-tt-forbidden` /
//! `inst-tt-window-pair` / `inst-tt-zero-band` / `inst-tt-fixture` exists, and
//! neither does the rate lock (`inst-tt-lock`) that would give a value meaning.
//! Rendered into a version, it would freeze — into an INSERT-only store on the
//! ≥ 7-year truth horizon — a field no rule in this gear has judged.
//!
//! **It is not reachable, and what holds the line is a rule rather than an
//! absence.** `POST …/plans/{planId}/publish` **is** mounted — `module.rs` merges
//! `publish::router` and `module_test`'s path census declares `PLAN_PUBLISH` — so
//! an unmounted route is not what stops the freeze. Three guards hold it, none of
//! them an absence:
//!
//! 1. [`crate::domain::publish::rules::NoUnjudgedPrimitive`], a **registered
//!    publish rule**, refuses the window and a `carry` allowance at precheck and
//!    again inside the commit transaction — this is the one that survives a new
//!    writer.
//! 2. `api::rest::prices::refuse_unlanded_primitives`, on the two interactive
//!    price routes.
//! 3. `domain::import`'s inherited `PRIMITIVE_RULES_UNBUILT` arm, on the bulk
//!    plane.
//!
//! Recorded rather than quietly rewritten (D-298), because a reader consulting the
//! old sentence would have got it wrong in *both* directions: believing an
//! unmounted route was the safety, and believing one surface refusal was the whole
//! of it. **A premise stated in prose outlives the code that made it true.**
//!
//! Both fields also sit in the evaluation-policy roster
//! ([`crate::domain::evaluation_policy`]), which says the opposite of a warning:
//! it tells a consumer both are part of the field set an evaluator reads.
//!
//! **Whoever adds a writer of a `PriceRow` owes either the remaining Slice-10
//! refusals or a refusal at their own boundary** — the publish rule catches what
//! reaches publish, and a writer that stores a value the rule then refuses has
//! built a row nobody can publish.
//!
//! Deleting the two fields from this renderer is *not* the fix — D-129's
//! supersession guard compares them between a predecessor and a successor, and a
//! delta that dropped them would lose a field that guard reads.
//!
//! No DTO in this gear sets `deny_unknown_fields`, so the surface refusal is also
//! contingent on both members remaining **modelled** fields rather than silently
//! ignored ones (D-174 clause 1) — a second reason the warning belongs here, next
//! to the renderer, rather than only at the boundary that refuses them.
//!
//! One absence is a **rule** rather than a gap, and it is listed so a later
//! reader does not close it: the operator-plane drift flags
//! (`tier_divergent`, `grants_divergent`, the tax-readiness and meter-binding
//! divergences) are **never** part of a frozen version (D-85). A version that
//! carried one would be a frozen artifact whose content changes when an external
//! signal arrives.
//!
//! Their home is `pricing_operator_flag`, and **no authoring surface reads it**.
//! The table has no repository, no reader and no writer: `operator_flag::` appears
//! outside its entity and its migration registration nowhere at all. D-85 does say
//! the flags are "surfaced on authoring/operator reads (`plan × read`)", which is a
//! design intention and not a built fact — D-305 settles which, saying the whole
//! plane is unbuilt, that the cost is "a plane, not a field" (a repository, a
//! producer on the publish path, a read surface and an alarm `PricingAlarm::ALL`
//! does not carry), and that the read surface is a design question it deliberately
//! does not settle. So the reader is **owed and unbuilt**, not present.
//!
//! [`crate::domain::plan_rules`] states the same absence correctly and gives the
//! reason it is worth stating at all: "the absence is therefore the *signal*, not
//! the storage … a reader who finds the table would otherwise reasonably conclude
//! the feature is wired." That is what this paragraph had been inviting.
//!
//! **No payload-generation marker is invented for any of this.** The gear has
//! exactly one such mechanism — the evaluation-policy generation
//! ([`EVALUATION_POLICY_GENERATION`], D-162) — with a declared roster, a
//! declared bump rule and a replayed log, and it qualifies the **price row's
//! evaluation field set**, not the delta's shape. Stretching it to mean "this
//! payload is missing Slice 7" is the same act as minting a code the design set
//! does not name.
//!
//! # The D-162 obligation, discharged
//!
//! D-162 records one hole against the implementation: `usageCounterOnPlanChange`
//! (D-113) "is an evaluation input outside the roster only because the plan-content
//! type it would be classified against does not exist in the crate yet, and the
//! slice landing it joins the roster under a bump". [`PlanSubjectDelta`] **is**
//! that plan-content type, so the walk is owed here and is done here.
//!
//! The boundary is D-162's own: a field is rostered **iff it tells an evaluator
//! how to derive the billable quantity or select the rate**. Field by field —
//! `plan_id`, `revision` and `sku_id` are identity; `lifecycle_state`,
//! `available_from` and `available_to` are sellability inputs (predicates (3)
//! and (4)), which decide *whether* a thing may be sold and never *at what
//! rate*; `billing_cycle` and `frequency` name the period a subscription's
//! cycle clock runs on, which `01-foundation.md` §4.4 puts in the
//! consumer-contract family D-162 excludes by name (proration, anchoring,
//! billing timing) and which no evaluator reads to derive a quantity — the
//! quantity-derivation fields are the row's own `billing_granularity` and
//! `aggregation_*`, already rostered; `plan_tier` and `plan_tier_override` are
//! the registry taxonomy and its audited divergence; the purchase bounds gate a
//! purchase, not a rate; `invoice_grouping_key` is a Billing layout hint (D-96);
//! the phase set, the add-on rules and the descriptor set are composition and
//! presentation. **None of them is rostered**, so the boundary analysis moves the
//! roster not at all — [`EVALUATION_POLICY_GENERATION`]'s generation turns for
//! other reasons and reads `ep-4` (D-298).
//!
//! D-162's named example **exists here**: `usage_counter_on_plan_change` is a
//! column, a field of [`PlanChangeContract`], written by `plan_repo` and rendered
//! into this payload as `usageCounterOnPlanChange`.
//!
//! **The bump D-162 owed was made, and it took the generation to `ep-2`.** The
//! document's log
//! gained `ep-2  D-113  + usage_counter_on_plan_change`, and
//! [`EVALUATION_POLICY_GENERATION`] followed it rather than the other way round —
//! D-162 clause (5) makes the roster *what replaying that log produces*, and
//! [`evaluation_policy_tests`](crate::domain::evaluation_policy) reads the block
//! with `include_str!`.
//!
//! **It cost more than a line, and the reason is the hazard this paragraph used
//! to describe.** The guard's exhaustive classification ran over
//! [`PriceRow`](crate::domain::price_row::PriceRow) alone, so a plan-scoped
//! roster member was invisible to it — exactly what D-162 warns of: *"a slice
//! that lands it and leaves the roster alone leaves the generation claiming more
//! than it covers."* Appending the log line therefore made the document's roster
//! disagree with the crate's, which is the test failing **as designed**. The
//! mechanism was widened instead of the document being trimmed to fit it:
//! `partition_plan_fields` is a second exhaustive destructure over
//! [`PlanChangeContract`], the roster is the **union** of the two, and each
//! struct keeps its own arm — because a pattern that misses a new field must
//! fail to compile, and one pattern cannot span two structs.
//!
//! The D-162 guard's **reach** is decided here too, and it stays where it is.
//! [`partition_row_fields`](crate::domain::evaluation_policy::partition_row_fields)
//! keeps `PriceRow` alone, because §4.4's roster block is a set of
//! **`pricing_price`** fields by its own words and its `outside:` half
//! enumerates `pricing_price` columns; a second partition over this type would
//! have to be asserted against a document block that does not exist, and an
//! expectation invented to satisfy a guard is the guard switched off. What this
//! type gets instead is a guard with a real obligation: [`PlanSubjectDelta::to_value`]
//! destructures with **no rest pattern**, so a field added to the delta is a
//! compile error until it is either rendered into the payload or deliberately
//! withheld — and that is where the next author meets the D-162 question, with
//! this paragraph beside it.
//!
//! ## The next author arrived, and the field is [`PlanSubjectDelta::composites`]
//!
//! Slice 10's derived-meter set joined the delta when the `composites` facet was
//! mounted on `PATCH /plans/{planId}` (before it, nothing could originate a
//! composite and this module contained the word zero times). The paragraph above
//! is the obligation and this is it discharged, with an answer that is
//! deliberately not "no".
//!
//! **On D-162's own boundary a composite meter reads as rostered.** The test is
//! whether a field "tells an evaluator how to derive the billable quantity or
//! select the rate", and a composite is a rule for deriving one billable quantity
//! from several constituent meters — nearer the centre of that test than
//! `usage_counter_on_plan_change`, which was rostered under `ep-2`.
//!
//! **It is nevertheless not rostered, and the reason is D-162 clause (5) rather
//! than the boundary.** The roster is *what replaying `01-foundation.md` §4.4's
//! log produces*, the log holds no line for this field, and a field cannot enter
//! the roster except by a line — that is exactly the mechanism
//! [`crate::domain::evaluation_policy`] argues is worth more than the constant.
//! Adding one here would be this module deciding a normative document's content
//! to make its own field fit, which is the "expectation invented to satisfy a
//! guard" that same module names as the guard switched off.
//!
//! Two facts bound the risk while it stands, and neither closes it. The
//! partition guard does **not** fail: it destructures [`PriceRow`] and
//! [`PlanChangeContract`], and this field is on neither, so nothing here is
//! compiling around a check. And a consumer is not misled about *content* — the
//! definition is in the payload and is readable. What the generation does not say
//! is that the evaluation field set moved when it did.
//!
//! **Reported, and owed to whoever holds §4.4:** either a log line for the
//! composite set (with the generation bump that line *is*), or a sentence in §4.4
//! putting plan-scoped derived-meter definitions outside the roster on purpose.
//! What must not happen is the question being answered by silence a third time.

use std::collections::BTreeMap;

use serde_json::{Value as JsonValue, json};
use toolkit_macros::domain_model;
use uuid::Uuid;

use crate::domain::contracts::{
    EntitlementGrants, GrantSet, PlanChangeContract, published_billing_timing,
};
use crate::domain::evaluation_policy::EVALUATION_POLICY_GENERATION;
use crate::domain::instant::format_rfc3339;
use crate::domain::lifecycle::LifecycleState;
use crate::domain::money::MinorAmount;
use crate::domain::overlay::{OverlayInterval, OverlayLine, OverlayRevision, TargetSku};
use crate::domain::plan_shape::{
    AddonRule, BillingCycle, CompositeMeter, DescriptorSet, Frequency, PeriodFloorCap, PhaseKind,
    PlanPhase,
};
use crate::domain::price_record::PriceRecord;
use crate::domain::price_row::{
    AggregationFunction, AggregationGranularity, BillingGranularity, IncludedAllowance,
    MinQtyUsageFallback, PriceRow, QuantitySource, ReservationFlavor, TierAggregationWindow,
    TierBand, TierQualificationWindow, model_kind_wire,
};
use crate::domain::read_model::OverlayIndexShard;
use crate::domain::scope_key::{Meter, PlanId, ScopeKey, ScopeKeyParts};
use crate::domain::window::{KeyWindows, WindowInterval, WindowState};
use time::OffsetDateTime;

/// The lifecycle states a plan-subject delta draws its price rows from.
///
/// D-121: "every price row of the plan — **any lifecycle state except
/// never-published drafts**". Two states satisfy that on a price row today, and
/// `superseded` is the load-bearing one: rating pins *current* versions and
/// rates *past* instants, so after a changeover the superseded predecessor —
/// its window `expired` — must survive the next re-projection or resolution at
/// yesterday's `t` fails closed on a legitimately covered period.
///
/// **Both sanctioned producers of a `superseded` price row are built and
/// mounted** — the D-88 supersession unit as
/// `infra::supersession::commit_supersession` behind
/// `POST /plans/{planId}/supersessions`, the D-100 cutover as
/// `infra::cutover::commit_cutover` behind `POST /plans/{planId}/cutovers`, and
/// the repricing apply reaching the first per journal row behind
/// `POST /repricing-runs`. The D-121 horizon that would bound the set is **not**
/// paid; `infra::read_model`'s module doc carries what that costs and what does
/// *not* bound it in its place. Read that before repeating any claim about this
/// constant's blast radius.
///
/// `draft` is out because it is the never-published state D-121 excludes;
/// `abandoned` never occurs on a price row at all (it is a plan-revision state,
/// §4.3); `retired` is a plan-revision state too — the revision's, which the
/// delta carries as [`PlanSubjectDelta::lifecycle_state`].
pub const PROJECTED_ROW_STATES: &[LifecycleState] =
    &[LifecycleState::Published, LifecycleState::Superseded];

/// The window states a plan-subject delta carries (D-121).
///
/// `PROJECTED_ROW_STATES`' sibling, one plane over, and the two absences are the
/// rule rather than a filter that happened to be written:
///
/// - **`cancelled` is out.** It is not history a consumer resolves against; it is
///   a schedule that never happened. A frozen delta advertising a cancelled
///   interval would tell a reader a key is covered over a span nothing was ever
///   effective on — the trailing void D-62 → D-80 → D-94 close, arriving from the
///   projection side instead of the truth side.
/// - **`expired` is in**, and it is the load-bearing one. Rating pins *current*
///   versions and rates *past* instants, so after a changeover the predecessor's
///   expired interval is what a legitimately covered arrears period resolves
///   against. Dropping it would fail that period closed on a key that was covered
///   the whole time — the same mistake `superseded` in `PROJECTED_ROW_STATES`
///   avoids on the row plane, for the same reason.
pub const PROJECTED_WINDOW_STATES: &[WindowState] = &[
    WindowState::Scheduled,
    WindowState::Active,
    WindowState::Expired,
];

/// The K3 cross-boundary marker, on every resolved `plan` subject row (D-169
/// clause 1, `06-consumer-contracts.md` §3 `inst-pi-crossboundary` and §6).
///
/// A **launch constant, tenant-wide**: a cross-currency, cross-region or
/// cross-frequency change publishes no credit basis, so the change is cancel plus
/// new. The value is named verbatim in §6 and is not derived from anything, which
/// is why it is a `const` here rather than a column — a per-tenant carrier is
/// D-169's rejected option (b).
///
/// **Written once, and a second literal spelling of it in this crate is the
/// defect.** The delta is the only artifact that carries the marker and this is
/// the only place its value exists; a second spelling is a second answer to a
/// question with one.
///
/// # Its former other half is not a field, and that is a decision rather than a
/// gap
///
/// §6 required a **pair** — this marker beside a `crossBoundaryWarningText` whose
/// value no document of the set ever named — so D-168 clause (1) held the line at
/// both-or-neither and G6 stamped neither. D-169 struck the text: what is
/// published is machine-readable and derivable by nobody but this gear, and what
/// a human is shown is not a catalog fact. The warning's normative home is **PRD
/// AC #66**, on the plan-change preview that renders it and takes the operator's
/// confirmation.
///
/// The reason it could not stay is this store: a delta row is INSERT-only over the
/// ≥ 7-year truth horizon in a store whose contract is that a completed version
/// never changes, and this design set has no localization story anywhere. A
/// customer-visible sentence frozen in one language, for every version already
/// stamped, is irreversible — bought for a convenience obtainable at render time.
pub const CROSS_BOUNDARY_CHANGE_POLICY: &str = "cancel_plus_new";

/// The overlay-document content of one `CatalogVersion` (D-91, D-112).
///
/// **It never re-projects the targeted plans.** `SubjectKind::PriceOverlay`'s own
/// doc states the reason — Tariffs joins overlays to base rows at evaluation — so a
/// `global`-scope overlay commit writes one row rather than a tenant's worth.
///
/// Composed from [`OverlayRevision`] rather than restating its ten fields, which is
/// [`PlanSubjectDelta`]'s principle applied to a type that already exists: the
/// approval unit pins exactly this value (D-225), so the delta and the pin cannot
/// describe the revision differently.
#[domain_model]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OverlaySubjectDelta {
    /// The revision this version freezes — the one the publish unit judged, read
    /// back from the revision its `pricing_catalog_version_ref` row pinned.
    pub content: OverlayRevision,
}

impl OverlaySubjectDelta {
    /// The payload one `price_overlay` delta row carries.
    ///
    /// # The pattern is a guard
    ///
    /// The `let OverlayRevision {.. }` below has **no rest pattern**, so a field
    /// added to the revision does not compile here (E0027) until it is named. The
    /// payload is the only thing a consumer ever sees, and this is what stops a
    /// field reaching the store and not the wire.
    #[must_use]
    pub fn to_value(&self) -> JsonValue {
        let OverlayRevision {
            price_overlay_id,
            revision,
            lifecycle_state,
            scope,
            precedence,
            interval,
            tax_basis,
            disclosure,
            target_ref,
            lines,
        } = &self.content;

        json!({
            "priceOverlayId": price_overlay_id,
            "revision": revision,
            "lifecycleState": lifecycle_state.as_str(),
            // The scope is rendered as the shard key's two halves rather than as
            // a nested object, so an index entry and a document agree on the
            // spelling a consumer matches a payer context against.
            "scopeClass": scope.class().as_str(),
            "scopeValue": if scope.value().is_some() {
                JsonValue::String(scope.stored_value().to_owned())
            } else {
                JsonValue::String(crate::domain::read_model::GLOBAL_SCOPE.to_owned())
            },
            "precedence": precedence,
            "effectiveFrom": interval.from.map(format_rfc3339),
            "effectiveTo": interval.to.map(format_rfc3339),
            "taxBasis": tax_basis.as_str(),
            "disclosure": disclosure.as_str(),
            "targetPlans": target_ref.plans.iter().map(|plan| plan.get()).collect::<Vec<_>>(),
            "lines": lines.iter().map(overlay_line_value).collect::<Vec<_>>(),
            "evaluationPolicyVersion": EVALUATION_POLICY_GENERATION,
        })
    }
}

/// One adjustment line, as the frozen payload carries it.
fn overlay_line_value(line: &OverlayLine) -> JsonValue {
    json!({
        "lineId": line.line_id,
        "planId": line.key.plan_id().map(PlanId::get),
        "targetSku": line.key.target_sku().map(TargetSku::as_str),
        "cohort": line.key.cohort().map(format_rfc3339),
        "kind": line.adjustment.kind().as_str(),
        "magnitudeKind": line.adjustment.magnitude_kind().as_str(),
        "percentBp": line.adjustment.percent_bp(),
        "amounts": line.adjustment.amounts().map(|set| {
            set.iter()
                .map(|(currency, minor)| json!({ "currency": currency.as_str(), "minor": minor }))
                .collect::<Vec<_>>()
        }),
    })
}

/// One overlay's entry in a shard (D-112 as D-133 narrowed it).
///
/// `(effective interval, precedence)` beside the id, which is §7's list exactly:
/// the scope is the **shard key** since D-133, so repeating it per entry would be
/// a second place for it to disagree with the row it is filed under.
#[domain_model]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OverlayIndexEntry {
    /// Which overlay.
    pub price_overlay_id: Uuid,
    /// Its own `[effectiveFrom, effectiveTo)`.
    pub interval: OverlayInterval,
    /// Its precedence, the first key of the stack order.
    pub precedence: i32,
}

/// One `overlay_index` shard as one `CatalogVersion` freezes it (D-112, D-133).
///
/// **The access path evaluation has and per-subject resolution cannot give.**
/// Resolution answers "overlay X at pin V"; evaluation needs the *set*, and the
/// delta store is keyed by subject, so without this the only route was a
/// `DISTINCT subject_ref` scan across years of retained overlay deltas on the
/// order-time path.
#[domain_model]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OverlayIndexDelta {
    /// Which shard — `subject_ref` on the row.
    pub shard: OverlayIndexShard,
    /// The overlays live at this version, in **no assumed order**: [`to_value`]
    /// sorts them.
    ///
    /// [`to_value`]: OverlayIndexDelta::to_value
    pub entries: Vec<OverlayIndexEntry>,
}

impl OverlayIndexDelta {
    /// The payload one `overlay_index` delta row carries.
    ///
    /// **Sorted here rather than trusted from the caller.** The payload is
    /// INSERT-only on the seven-year horizon, so its byte order is part of what
    /// the version says; taken in the store's row order, two projections of one
    /// set would be two payloads. The order is `inst-plv-class-tiebreak`'s total
    /// order with its middle key dropped — `precedence`, then overlay id —
    /// because inside one shard the class is constant by construction.
    #[must_use]
    pub fn to_value(&self) -> JsonValue {
        let Self { shard, entries } = self;
        let mut ordered: Vec<&OverlayIndexEntry> = entries.iter().collect();
        ordered.sort_by(|a, b| {
            a.precedence
                .cmp(&b.precedence)
                .then_with(|| a.price_overlay_id.cmp(&b.price_overlay_id))
        });

        json!({
            "scopeClass": shard.scope_class(),
            "scopeValue": shard.scope_value(),
            "overlays": ordered
                .into_iter()
                .map(|entry| {
                    json!({
                        "priceOverlayId": entry.price_overlay_id,
                        "effectiveFrom": entry.interval.from.map(format_rfc3339),
                        "effectiveTo": entry.interval.to.map(format_rfc3339),
                        "precedence": entry.precedence,
                    })
                })
                .collect::<Vec<_>>(),
            "evaluationPolicyVersion": EVALUATION_POLICY_GENERATION,
        })
    }
}

/// One `group_membership` document, as one `CatalogVersion` freezes it
/// (`design/09-price-overlays.md` §1.7's `MembershipLedger`, `SubjectKind::GroupMembership`).
///
/// **This is the surface Tariffs reads to do its own resolve-and-freeze.**
/// `ResolvedGroupFreezer` is the joint contract (D-30): this gear publishes
/// membership into the read model and stops there — Tariffs performs the
/// interval resolution at `t` and composes `pricingSnapshotRef` from what it
/// finds here. Nothing in this crate resolves a group for a payer, freezes one
/// into a snapshot, or exposes a resolve-for-payer endpoint; §1.7 (`:110`),
/// `inst-gm-return` (`:173`) and D-30 (`:466`) all say so, and building any of
/// the three here would be a second, unauthorised composition.
///
/// **Pinned at the commit, like every other subject — with the pin's shape set
/// by how a membership is stored.** Unlike a plan or an overlay,
/// `pricing_group_membership` carries no revision-scoped content table:
/// `group_membership_repo`'s own doc records that a membership row is mutated
/// in place (`end_membership` narrows `effective_to`) rather than superseded by
/// a new row, so there is no immutable revision for a ref to name the way
/// [`project_plan_subject`](crate::infra::read_model::project_plan_subject) and
/// [`project_overlay_subject`](crate::infra::read_model::project_overlay_subject)
/// name theirs. What a mutation *moves*, though, is one column, and that is what
/// the ref pins: `pricing_catalog_version_ref.subject_effective_to`, written by
/// the publish unit itself (`pricing_catalog_version_ref`), with the row version beside it
/// in `subject_revision` so that a pinned `None` reads as **open-ended** rather
/// than as no pin at all. The fields no mutation touches — payer, group value,
/// interval start — are read from the row, which is D-165's split exactly, at
/// the boundary this store's shape puts it.
///
/// **The live read is reachable, and it is why this is frozen.** `POST …/members`
/// and `PATCH …/members/{id}` mint one publish unit each, so an enrollment
/// followed by an end before the sweep would freeze the **ended** state under
/// both versions: a consumer pinned at the enrollment's version would read a
/// membership that had already finished at an instant when it had not.
/// Permanently, on an INSERT-only row with a ≥ 7-year horizon, on the surface
/// Tariffs resolves the group at `t` against (D-30).
#[domain_model]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MembershipSubjectDelta {
    /// The membership this delta is the subject of. `subject_ref` on the row.
    pub membership_id: Uuid,
    /// The payer this interval covers (`inst-cg-record`).
    pub payer_tenant_id: Uuid,
    /// The taxonomy value the payer is enrolled in over this interval.
    pub group_value: String,
    /// Inclusive start of the half-open interval, UTC.
    pub effective_from: OffsetDateTime,
    /// Exclusive end, UTC; `None` is open-ended — a membership not (yet) ended.
    pub effective_to: Option<OffsetDateTime>,
}

impl MembershipSubjectDelta {
    /// The payload one `group_membership` delta row carries.
    ///
    /// No rest pattern on the destructure below, [`OverlaySubjectDelta::to_value`]'s
    /// reason: a field added to the row does not compile here until it is named.
    #[must_use]
    pub fn to_value(&self) -> JsonValue {
        let Self {
            membership_id,
            payer_tenant_id,
            group_value,
            effective_from,
            effective_to,
        } = self;
        json!({
            "membershipId": membership_id,
            "payerTenantId": payer_tenant_id,
            "groupValue": group_value,
            "effectiveFrom": format_rfc3339(*effective_from),
            "effectiveTo": effective_to.map(format_rfc3339),
        })
    }
}

/// The plan-subject content of one `CatalogVersion`.
///
/// Composed from the types that already model each part rather than restating
/// them: a second spelling of a phase or a price row is a second thing that can
/// disagree with the truth table it was projected from.
#[domain_model]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanSubjectDelta {
    /// The plan this delta is the subject of. `subject_ref` on the row.
    pub plan_id: PlanId,
    /// The revision the content was projected from — the plan's **current**
    /// one, `published` or `retired` (D-128).
    pub revision: u64,
    /// The current revision's own lifecycle state.
    ///
    /// A **projected plan-subject field**, and the whole of D-128 in one line:
    /// it is what sellability predicate (4) reads at the pin. A retired plan
    /// can never publish again, so if this were not frozen into the version
    /// nothing would ever re-project it and the read model would advertise a
    /// retired plan as sellable permanently.
    pub lifecycle_state: LifecycleState,
    /// The catalog SKU this plan realizes, when one is bound.
    pub sku_id: Option<Uuid>,
    /// The plan's tier, from the registry taxonomy.
    pub plan_tier: Option<String>,
    /// Whether the tier deliberately diverges from the parent SKU's under an
    /// audited override.
    pub plan_tier_override: bool,
    /// The plan's billing cycle.
    pub billing_cycle: Option<BillingCycle>,
    /// The recurring frequency, custom interval riding the variant.
    pub frequency: Option<Frequency>,
    /// Start of the plan's availability window, UTC — sellability predicate (3).
    pub available_from: Option<OffsetDateTime>,
    /// End of the plan's availability window, UTC — sellability predicate (3).
    pub available_to: Option<OffsetDateTime>,
    /// Minimum purchasable quantity (one-time plans).
    pub purchase_min_qty: Option<u64>,
    /// Maximum purchasable quantity (one-time plans).
    pub purchase_max_qty: Option<u64>,
    /// The Billing invoice-layout hint (D-96).
    pub invoice_grouping_key: Option<String>,
    /// The revision's phase chain (D-83 — child rows version with the revision
    /// and are the projection source).
    pub phases: Vec<PlanPhase>,
    /// The revision's add-on composition rules (D-83).
    pub addon_rules: Vec<AddonRule>,
    /// The revision's billing descriptor set (D-83).
    pub descriptor_set: Option<DescriptorSet>,
    /// The revision's plan-level period floor/cap set, one entry per market it
    /// is authored for (S2 §6, **D-319**).
    ///
    /// **This is the half of PRD §17.8 a consumer can act on.** Rating resolves
    /// the amount, currency and attachment scope for its
    /// `PeriodFloorCapObligation` *from the pinned snapshot*, and Billing
    /// executes `max(total, floor)` / `min(total, cap)` after step 9 — so a
    /// bound outside this payload is a bound nothing downstream can apply,
    /// however carefully it was authored and approved.
    ///
    /// The **attachment scope is not a member** and is not authored anywhere:
    /// rating PRD §17.2 makes it `usage` by default and `recurring+usage` when
    /// the bound is plan-level, and a bound published on the *plan subject* is
    /// plan-level by construction. Stamping a constant here would be this gear
    /// declaring a wire fact its design set does not (D-204 clause (2)).
    pub period_floor_caps: Vec<PeriodFloorCap>,
    /// The revision's derived (composite) meter definitions (Slice 10 §6, D-83,
    /// D-106).
    ///
    /// **This is the half of `inst-cm-frozen` a consumer can act on.** The
    /// instruction says the catalog *"persists and freezes the definition"* and
    /// Rating *"computes from the snapshot"*, and until this field existed the
    /// freezing half was true of the approval content pin alone: `content_pin`
    /// hashed the set from v11 and `approvals.rs` showed it to a reviewer, so a
    /// definition was covered by a signature and reached no artifact any consumer
    /// reads. The word "composite" appeared in this module **zero** times. Rating
    /// resolves a plan through `pricingSnapshotRef` into a delta row, so a formula
    /// outside this payload is a formula Rating cannot evaluate however carefully
    /// it was approved.
    ///
    /// Carried as [`CompositeMeter`] rather than restated field by field, on this
    /// type's stated principle: a second spelling of a definition is a second thing
    /// that can disagree with the truth rows it was projected from - and here with
    /// the pin as well, which frames the same four members.
    ///
    /// **The formula is opaque and stays opaque** (A4). It is rendered verbatim,
    /// which is what makes the payload a *snapshot* rather than a derivation: this
    /// gear never evaluates a formula, so it has nothing to render but the data the
    /// author wrote and the reviewer signed for.
    ///
    /// Empty is the ordinary case and means the revision defines no composite. What
    /// it does **not** mean is "none was projected": the projector reads the same
    /// revision-scoped table the publish rules judged, so an empty list here and an
    /// empty set there are the same fact.
    pub composites: Vec<CompositeMeter>,
    /// The entitlement grant set this revision publishes (Slice 6, §6, D-41),
    /// as authored. The **materialized** `phase → grant-set` map is derived from
    /// it and the phase chain at render time.
    pub entitlement_grants: EntitlementGrants,
    /// The plan-change contract this revision publishes (Slice 6, §6).
    ///
    /// **A projected plan-subject field**, because it is what a consumer reads to
    /// know whether a self-service change is offered at all — and
    /// `inst-pc-failsafe` makes its *absence* the fail-safe answer rather than an
    /// unknown, which only holds if the field is part of the contract.
    pub change_contract: PlanChangeContract,
    /// The price rows the version freezes, drawn from [`PROJECTED_ROW_STATES`].
    pub prices: Vec<PriceRecord>,
    /// The **derived** tax facts each projected row carries: D-154's resolved
    /// effective category, and C3's `not_sellable_ga` flag.
    ///
    /// **Derived at publish and never authored** (§6), which is why they are a
    /// side table keyed by `price_id` rather than fields on [`PriceRecord`]:
    /// that type is the authored truth row, and a resolved value living on it
    /// would be a second place a category can come from — precisely what D-110
    /// removed when it deleted the descriptor set's mirroring column.
    ///
    /// Empty is a legitimate state and means *nothing was resolved yet*: only
    /// the publish path holds the readiness to coalesce against, so a delta built
    /// by any other caller carries none and renders the authored column alone.
    pub tax_projection: BTreeMap<Uuid, RowResolutionProjection>,
    /// The plan's window facts, grouped per canonical scope key, drawn from
    /// [`PROJECTED_WINDOW_STATES`] (D-99, D-121).
    ///
    /// **Intervals, states and a derived coverage end — never a point-in-time
    /// boolean.** `inst-sg-surface` is explicit about it and the reason is what
    /// makes activation and expiry *not* publish units: a consumer re-derives
    /// "active at `t`" from the frozen interval, so `now` crossing an
    /// `effectiveFrom` changes a truth row and changes nothing projected. An
    /// `activeNow` flag would put the answer to a question about the reader's
    /// clock into an artifact frozen years earlier, and every time-driven flip
    /// would then owe a re-projection of a store whose whole contract is that a
    /// completed version never changes.
    ///
    /// Grouped **per key** rather than per price row because every consumer of
    /// these facts asks a per-key question — predicate (1), the D-80 horizon,
    /// `inst-el-bootstrap`'s generation match — and one key legitimately spans two
    /// rows (a supersession leaves a `superseded` predecessor beside its
    /// `published` successor, and their two windows are one coverage run).
    ///
    /// # Which keys this enumerates: exactly the ones [`PlanSubjectDelta::prices`]
    /// does
    ///
    /// One group per distinct canonical scope key of the projected price rows, and
    /// **no others** — a consumer-visible fact rather than an implementation
    /// detail, which is why it is stated here beside the field a consumer reads.
    /// Two consequences, and each closes a way the two lists could disagree:
    ///
    /// - a projected key whose windows were all cancelled (or that has none at all)
    ///   is **present**, with an empty interval list and a `coverageEnd` of
    ///   [`CoverageEnd::Uncovered`](crate::domain::window::CoverageEnd::Uncovered).
    ///   "This key is uncovered" therefore has one declared spelling (D-167 clause
    ///   1) rather than being inferred from a missing group;
    /// - a window hanging off a row D-121 excludes — a never-published draft — is
    ///   absent, and contributes no key of its own. A draft row legitimately shares
    ///   a key with the published row it will supersede, so this is a filter on the
    ///   **row**, never on the key.
    ///
    /// Without it a consumer walking `windows` would meet keys no projected row
    /// backs, and a key's frozen coverage end could run past anything published is
    /// effective over.
    ///
    /// # The `state` token is the state **at projection time**
    ///
    /// **"Active at `t`" is derived from `interval ∧ now`, never read off the
    /// token.** The token records where the window stood when the version was
    /// projected, and a consumer that branched on it would be asking about a moment
    /// that has passed.
    ///
    /// The argument, rather than the assertion: the token is **stale by
    /// construction** for every window the `WindowActivationJob` ever flips, because
    /// D-99 makes an activation re-project nothing. So if sellability predicate (1)
    /// read the token, an activation *would* owe a re-projection — contradicting the
    /// very decision that makes the window plane cheap — and until one arrived the
    /// key would read unsellable forever behind a token frozen at `scheduled`. The
    /// derived reading is the only coherent one, and it is also why
    /// [`PROJECTED_WINDOW_STATES`] carries `scheduled` at all: a scheduled window's
    /// interval is future-covering and has to be visible before the window is
    /// active, which is exactly what the D-80 coverage horizon looks ahead over.
    ///
    /// What the token *is* for is the two questions about the past that the interval
    /// cannot answer: whether a key's coverage over some span was ever real
    /// (`cancelled` never reaches here at all) and whether a run of intervals is one
    /// coverage run or a predecessor plus a successor.
    ///
    /// **This reading is a divergence to report, not a document to edit.** The design
    /// set nowhere says which reading a consumer takes: `inst-sg-surface` says "an
    /// **active** window covers `t`" while D-99 says active-at-`t` is derived at read
    /// time, and nothing reconciles the two. Stated here because it is what G5 codes
    /// predicate (1) against and this is where the payload is declared.
    pub windows: Vec<KeyWindows>,
}

impl PlanSubjectDelta {
    /// Render the delta as `pricing_read_model.payload` holds it.
    ///
    /// `evaluationPolicyVersion` is read from [`EVALUATION_POLICY_GENERATION`]
    /// rather than carried on the struct, exactly as the `PlanPublished`
    /// payload reads it: the generation is a property of the gear that
    /// projected, not of the projection, so a caller able to supply one could
    /// freeze a version under a semantics its rows were never authored to.
    ///
    /// # The pattern is a guard
    ///
    /// The `let Self {.. }` below has **no rest pattern**. A field added to
    /// [`PlanSubjectDelta`] does not compile here (E0027) until it is named,
    /// which is what stops a field being added to the delta and silently not
    /// reaching the payload — the payload being the only thing a consumer ever
    /// sees. The module doc carries the D-162 question the same author owes.
    #[must_use]
    pub fn to_value(&self) -> JsonValue {
        let Self {
            plan_id,
            revision,
            lifecycle_state,
            sku_id,
            plan_tier,
            plan_tier_override,
            billing_cycle,
            frequency,
            available_from,
            available_to,
            purchase_min_qty,
            purchase_max_qty,
            invoice_grouping_key,
            phases,
            addon_rules,
            descriptor_set,
            period_floor_caps,
            composites,
            entitlement_grants,
            change_contract,
            prices,
            tax_projection,
            windows,
        } = self;

        json!({
            "planId": plan_id.get(),
            "revision": revision,
            "lifecycleState": lifecycle_state.as_str(),
            "skuId": sku_id,
            "planTier": plan_tier,
            "planTierOverride": plan_tier_override,
            "billingCycle": billing_cycle.map(BillingCycle::as_str),
            "frequency": frequency.map(frequency_value),
            "availableFrom": available_from.map(format_rfc3339),
            "availableTo": available_to.map(format_rfc3339),
            "purchaseMinQty": purchase_min_qty,
            "purchaseMaxQty": purchase_max_qty,
            "invoiceGroupingKey": invoice_grouping_key,
            "phases": phases.iter().map(phase_value).collect::<Vec<_>>(),
            "addonRules": addon_rules.iter().map(addon_rule_value).collect::<Vec<_>>(),
            "descriptorSet": descriptor_set.as_ref().map(descriptor_set_value),
                    // D-319's period floor/cap (PRD §17.8). Rendered as a list of
                    // market-keyed objects rather than an object keyed by a joined
                    // `"USD/us"` string: the market is a **pair** everywhere else in
                    // this payload, and a consumer that had to split a key back apart
                    // would be parsing a spelling this gear invented for one member.
                    //
                    // An empty list is the ordinary plan. It is not `null`, because
                    // there is nothing for a null to mean here that the empty list does
                    // not — unlike `allowedChangeTargets`, where absence is a fail-safe
                    // and emptiness is a statement.
            "periodFloorCaps": period_floor_caps
                .iter()
                .map(period_floor_cap_value)
                .collect::<Vec<_>>(),
                    // Slice 10's derived meters (`inst-cm-frozen`). Rendered as a list and
                    // not as a map keyed by `outputUnit`, even though
                    // `uq_pricing_composite_meter_output` makes the unit unique per
                    // revision: the id is the definition's identity (D-106) and a map keyed
                    // by anything else would put a consumer's lookup key on a column an
                    // author may edit.
            "composites": composites.iter().map(composite_value).collect::<Vec<_>>(),
                    // The plan-change contract (`inst-pc-targets` / `inst-pc-rank` /
                    // `inst-pc-counter-carry`). `allowedChangeTargets` renders `null`
                    // when the plan states none, and that null **is** the answer: absence
                    // means no self-service change (`inst-pc-failsafe`), never "unknown"
                    // — which is why an empty array is a different payload and a
                    // different fact.
                    //
                    // No `inPlace` / `cancelPlusNew` classification is stamped. D-93
                    // moved it to change time in Subscriptions, computed from both plans'
                    // published facts at the pinned version; a stamped value here could
                    // never be re-computed, because a target's publish warms only its own
                    // delta (D-86/D-91) and the source's revision is immutable. What is
                    // published is the input (`inst-pc-boundary`).
                    // The grant set, and the **complete** map beside it
                    // (`inst-gs-perphase`). `entitlementGrants` is what the author
                    // wrote; `phaseGrantMap` is every phase of the schedule mapped to
                    // its effective set, so Subscriptions resolves the active phase at
                    // `t` by one lookup and never merges fallbacks at runtime. Both,
                    // because `inst-gs-resolved` wants the reference kept for
                    // auditability beside the set that is actually read.
            "entitlementGrants": grants_value(entitlement_grants),
            "phaseGrantMap": entitlement_grants
                .phase_map(phases)
                .iter()
                .map(|(id, set)| (id.to_string(), grant_set_value(set)))
                .collect::<serde_json::Map<_, _>>(),
            "allowedChangeTargets": change_contract.allowed_change_targets,
            "comparabilityRank": change_contract.comparability_rank,
            "usageCounterOnPlanChange": change_contract.usage_counter_on_plan_change.as_str(),
            "prices": prices
                .iter()
                .map(|record| price_value(record, tax_projection.get(&record.price_id)))
                .collect::<Vec<_>>(),
            "windows": windows.iter().map(key_windows_value).collect::<Vec<_>>(),
            "evaluationPolicyVersion": EVALUATION_POLICY_GENERATION,
                    // Read from the constant for `evaluationPolicyVersion`'s reason: it
                    // is a property of the gear that projected, not of the projection,
                    // and a delta able to carry its own would be a version answering
                    // the contract differently from its siblings.
            "crossBoundaryChangePolicy": CROSS_BOUNDARY_CHANGE_POLICY,
        })
    }
}

/// The authored grant set, whole.
fn grants_value(grants: &EntitlementGrants) -> JsonValue {
    json!({
        "planTierRef": grants.plan_tier_ref,
        "featureFlags": grants.plan_level.feature_flags,
        "quotas": grants.plan_level.quotas,
        "perPhase": grants
            .per_phase
            .iter()
            .map(|(id, set)| (id.to_string(), grant_set_value(set)))
            .collect::<serde_json::Map<_, _>>(),
    })
}

/// One grant set: the §17.6 shape, flags and quotas.
fn grant_set_value(set: &GrantSet) -> JsonValue {
    json!({
        "featureFlags": set.feature_flags,
        "quotas": set.quotas,
    })
}

/// The recurring frequency, token and interval together.
///
/// [`Frequency::as_str`] renders the token alone, and a custom interval carries
/// its `n` and unit **inside the variant** — the pairing
/// `chk_pricing_plan_custom_interval_pairing` exists to keep — so a payload
/// carrying only the token would freeze a `custom_every_n` with no interval,
/// which is a period nobody can compute.
fn frequency_value(frequency: Frequency) -> JsonValue {
    match frequency {
        Frequency::CustomEveryN { n, unit } => json!({
            "token": frequency.as_str(),
            "n": n,
            "unit": unit.as_str(),
        }),
        _ => json!({ "token": frequency.as_str() }),
    }
}

/// One phase of the revision's chain.
fn phase_value(phase: &PlanPhase) -> JsonValue {
    let PlanPhase {
        phase_id,
        kind,
        display_name,
        ordinal,
        converts_to_phase_id,
        phase_duration_days,
        display_trial_days,
    } = phase;
    json!({
        "phaseId": phase_id.get(),
        "kind": PhaseKind::as_str(*kind),
        // Presentation (D-357): the operator's label, `null` when none. Not an
        // evaluator input, so it is not rostered under
        // `EVALUATION_POLICY_GENERATION` — the module doc's boundary analysis puts
        // the phase set among the presentation fields for that reason.
        "displayName": display_name,
        "ordinal": ordinal,
        "convertsToPhaseId": converts_to_phase_id.map(super::scope_key::PhaseId::get),
        "phaseDurationDays": phase_duration_days,
        "displayTrialDays": display_trial_days,
    })
}

/// One add-on composition rule of the revision.
fn addon_rule_value(rule: &AddonRule) -> JsonValue {
    let AddonRule {
        addon_sku_id,
        required,
        min_qty,
        max_qty,
        step_qty,
        price_override_ref,
        depends_on,
        conflicts_with,
    } = rule;
    json!({
        "addonSkuId": addon_sku_id,
        "required": required,
        "minQty": min_qty,
        "maxQty": max_qty,
        "stepQty": step_qty,
        "priceOverrideRef": price_override_ref,
        "dependsOn": depends_on,
        "conflictsWith": conflicts_with,
    })
}

/// One derived (composite) meter of the revision.
///
/// Destructured without a rest pattern for [`PlanSubjectDelta::to_value`]'s
/// reason, and here it carries the same second obligation the pin does: a field
/// added to [`CompositeMeter`] meets this renderer **and**
/// `content_pin::put_plan_shape`, which is the pair of questions a new member of a
/// frozen definition owes - one about what a consumer reads, one about what a
/// reviewer's signature covers.
fn composite_value(composite: &CompositeMeter) -> JsonValue {
    let CompositeMeter {
        composite_id,
        output_unit,
        constituent_units,
        formula,
    } = composite;
    json!({
        "compositeId": composite_id,
        "outputUnit": output_unit,
        "constituentUnits": constituent_units,
        // Verbatim, and never re-serialized through a type of this crate's: A4
        // makes the formula data this gear persists and does not read, so any
        // normalization here would be this gear deciding what a formula means.
        "formula": formula,
    })
}

/// The revision's billing descriptor set.
fn descriptor_set_value(set: &DescriptorSet) -> JsonValue {
    let DescriptorSet {
        invoice_line_template,
        gl_code,
        itemization_rule,
        additional,
    } = set;
    json!({
        "invoiceLineTemplate": invoice_line_template,
        "glCode": gl_code,
        "itemizationRule": itemization_rule,
        "additional": additional,
    })
}

/// One market's period floor and cap (**D-319**).
///
/// The currency is rendered once, as the market axis, and the two amounts carry
/// no currency of their own — the denomination has one home, which is the same
/// rule the price row's `MinorAmount` follows.
///
/// # `pub(crate)` because there is a **second** renderer of this shape
///
/// `infra::synthesis` carries its own hand-written `periodFloorCaps` block with
/// its own `currency` / `region` / `floorMinor` / `capMinor` literals, under a
/// comment claiming it renders *"the same members the read-model delta uses, so a
/// consumer reads one shape whichever door the snapshot came through"*. Nothing
/// enforces that: the destructure below has no rest pattern, so a member added to
/// [`PeriodFloorCap`] stops the **delta** side compiling and the synthesis side
/// compiles unchanged — and the two doors of one seven-year payload silently
/// diverge, with only `tests/rest_migrated_origin_snapshots.rs` asserting the
/// synthesis copy and nothing comparing the two.
///
/// The visibility is the half of that fix this module owns. The other half is one
/// call site in `infra::synthesis`, and until it lands the claim above is a
/// comment rather than a mechanism.
pub(crate) fn period_floor_cap_value(bound: &PeriodFloorCap) -> JsonValue {
    let PeriodFloorCap {
        currency,
        region,
        floor_minor,
        cap_minor,
    } = bound;
    json!({
        "currency": currency.as_str(),
        "region": region.as_str(),
        "floorMinor": floor_minor.map(MinorAmount::get),
        "capMinor": cap_minor.map(MinorAmount::get),
    })
}

/// One row's **frozen resolutions** and the GA gate derived beside them.
///
/// Not named for tax, though it began as the tax projection alone: the rounding
/// policy has a frozen column of its own and joins it here. The type is about the
/// values a publish resolved and a consumer must not re-derive.
#[domain_model]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RowResolutionProjection {
    /// `coalesce(row.tax_category_ref, readiness.taxCategory)`, frozen with the
    /// `CatalogVersion` so Billing never re-resolves the fallback against the
    /// mutable region taxonomy (D-154).
    pub resolved_tax_category: Option<String>,
    /// `coalesce(row.rounding_policy_ref, tenant.default_rounding_policy_ref)`,
    /// frozen with the `CatalogVersion` for the same reason the category is: the
    /// tenant default is a live setting, and a charge replayed from a pinned
    /// version must round the way it rounded when the version was cut.
    pub resolved_rounding_policy: Option<String>,
    /// C3's gate: a tax-inclusive row is authorable and previewable but **not
    /// sellable** on its market until Tax Engine GA. Per row, hence per
    /// `(currency, region)` market — never per plan.
    pub not_sellable_ga: bool,
}

/// One frozen price row: its identity, its canonical scope key, its authored
/// shape and the row-level metadata a consumer resolves.
///
/// Destructured without a rest pattern for [`PlanSubjectDelta::to_value`]'s
/// reason, and here it carries a second obligation: a field added to
/// [`PriceRecord`] meets this renderer **and**
/// [`partition_row_fields`](crate::domain::evaluation_policy::partition_row_fields)'s
/// D-162 classification, which is the pair of questions a new row field owes.
fn price_value(record: &PriceRecord, tax: Option<&RowResolutionProjection>) -> JsonValue {
    let PriceRecord {
        price_id,
        scope_key,
        row,
        tax_inclusive,
        tax_category_ref,
        billing_timing,
        proration_contract,
        rounding_policy_ref,
        grandfather_until,
        supersedes_price_id,
        lifecycle_state,
        created_by: _,
        created_at_utc: _,
        row_version: _,
    } = record;

    let mut value = json!({
        "priceId": price_id,
        "scopeKey": scope_key_value(scope_key),
        "lifecycleState": lifecycle_state.as_str(),
        "taxInclusive": tax_inclusive,
            // The **authored** column. D-154's *resolved effective* category is a
            // different field, filled by the projector — the layer holding the
            // readiness to coalesce against. See `infra::read_model`'s subject
            // assembly; there is no `with_tax_projection` builder.
        "taxCategoryRef": tax_category_ref,
            // D-154's **resolved** value and C3's gate, both derived at projection.
            // `resolvedTaxCategory` renders `null` when nothing was resolved, so a
            // consumer can tell "this version resolved nothing" from "this field is
            // not part of the contract". `notSellableGa` cannot say that with a
            // bare `bool` — see its own line.
        "resolvedTaxCategory": tax.and_then(|t| t.resolved_tax_category.clone()),
            // The rounding half, and it renders `null` on the same terms. Before it
            // existed the publish wrote its value over the authored
            // `roundingPolicyRef` below, so this payload could not tell a row that
            // named a policy from one that fell back to the tenant default.
        "resolvedRoundingPolicy": tax.and_then(|t| t.resolved_rounding_policy.clone()),
        "notSellableGa": tax.is_some_and(|t| t.not_sellable_ga),
            // The **published** timing, not the column: on every kind but
            // `recurring` it is a constant the author never gave (`inst-bt-usage`),
            // and Billing consumes the field per line. A payload that rendered the
            // raw column would hand a hybrid's usage line a `null` for Billing to
            // interpret, which is the heuristic `inst-bt-frozen` forbids.
        "billingTiming": published_billing_timing(
            scope_key.charge_kind(),
            billing_timing.as_deref(),
        ),
            // The proration input contract (`inst-pi-required`), flat beside the
            // row's own keys for `row_value`'s reason: it is what a consumer
            // computes from, and a nesting level no document declares would be a
            // wire structure invented here. `anchorDay` renders only under the
            // policy that has one -- the pairing is structural in the domain, and a
            // payload that carried a null beside `calendar_month` would invite a
            // consumer to read it as "unset" rather than "not applicable".
        "billingAnchorPolicy": proration_contract.map(|c| c.billing_anchor_policy.as_str()),
        "anchorDay": proration_contract
            .and_then(|c| c.billing_anchor_policy.anchor_day())
            .map(super::contracts::AnchorDay::get),
        "prorationBasis": proration_contract.map(|c| c.proration_basis.as_str()),
        "creditOnDowngrade": proration_contract.map(|c| c.credit_on_downgrade),
        "roundingPolicyRef": rounding_policy_ref,
        "grandfatherUntil": grandfather_until.map(format_rfc3339),
        "supersedesPriceId": supersedes_price_id,
    });
    merge(&mut value, row_value(row));
    value
}

/// The authored Slice-3 shape, flat beside the row's own keys.
///
/// Flat rather than nested because it is what a consumer evaluates a charge
/// from and a nesting level nothing in the design set declares would be a wire
/// structure invented here. Split out of [`price_value`] so neither exceeds the
/// line budget, and destructured exhaustively for the reason that one is.
///
/// # This is where the D-45 allowance compile is materialized (D-130)
///
/// The compile is a **projection, never a write-back**: the row's truth stays
/// exactly as authored and the compiled artifacts are materialized *here*, into
/// the read model and the `pricingSnapshotRef` the version freezes. So on an
/// allowance-carrying row three of the members below are the **compiled** value
/// and not the stored column — `modelKind` (the presented `graduated` kind),
/// `bands` (the `$0` band plus the offset ladder), and `unitRateNanoMinor`
/// (`null`, because an untiered row's rate is folded into the top band) — and one
/// member is added, `allowanceMarker`. `includedAllowance` keeps carrying the
/// **authored** declaration beside them, which is what makes the compile
/// re-entrant: the input a supersession, repricing successor or clone recompiles
/// from is right there.
///
/// **Compile-equivalence (AC #90a)** is what makes that safe to do silently: the
/// projected form is byte-identical in evaluation terms to the hand-authored
/// `$0`-band row, so a consumer needs no allowance-specific arithmetic and the
/// marker is for display and the included-vs-billed reporting split only.
fn row_value(row: &PriceRow) -> JsonValue {
    let PriceRow {
        charge_kind,
        model_kind,
        amount_minor,
        unit_rate,
        bands,
        package_size,
        package_price_minor,
        quantity_source,
        manual_quantity,
        meter,
        dimension_key,
        billing_granularity,
        tier_aggregation_window,
        tier_qualification_window,
        aggregation_function,
        aggregation_granularity,
        max_hold_granules,
        included_allowance,
        reserved_rate,
        reservation_flavor,
        min_qty_purchase,
        min_qty_usage,
        min_qty_usage_fallback,
        discount_ref,
    } = row;
    // `inst-ac-band` / `inst-ac-marker`. `None` on every row that declares no
    // allowance -- which is every row until one does -- and the three members it
    // overrides then read the stored columns exactly as they always have.
    let compiled = crate::domain::allowance::compile(row);
    json!({
        "chargeKind": charge_kind.as_str(),
        "modelKind": compiled.as_ref().map_or_else(
            || model_kind.map(model_kind_wire),
            |compiled| Some(model_kind_wire(compiled.presented_kind)),
        ),
        "amountMinor": amount_minor.map(crate::domain::money::MinorAmount::get),
        // The `per_unit` rate, in 10^-9 minor units (D-311). A member of its own
        // rather than a second meaning for `amountMinor`: a consumer reading the
        // old name would otherwise get a number a billion times too large, on a
        // read-model row frozen for seven years.
        //
        // `null` on a compiled row: the authored rate is the top band's rate
        // there, and a payload carrying both would hand a consumer two competing
        // prices -- the exact thing `inst-mk-required`'s placement matrix keeps
        // off every other row.
        "unitRateNanoMinor": compiled.as_ref().map_or_else(
            || unit_rate.map(crate::domain::money::RateMinor::nano_minor),
            |_| None,
        ),
        "bands": compiled.as_ref().map_or_else(
            || bands.iter().map(band_value).collect::<Vec<_>>(),
            |compiled| compiled.bands.iter().map(band_value).collect::<Vec<_>>(),
        ),
        "packageSize": package_size,
        "packagePriceMinor": package_price_minor.map(crate::domain::money::MinorAmount::get),
        "quantitySource": quantity_source.map(QuantitySource::as_str),
        "manualQuantity": manual_quantity,
        "meter": meter,
        // The same `null`-for-absent convention `scope_key_value` renders axis 10
        // under, because this member and that one are one fact: a consumer
        // resolving which line a usage row prices joins them, and an absence
        // spelled two ways in one document makes that join say the row is
        // dimensioned differently from its own key.
        //
        // **The frozen corpus carries both spellings**, and a reader of it must
        // treat `""` and `null` alike here. The plane is INSERT-only over the
        // seven-year horizon, so documents are not rewritten and there is no
        // payload version to discriminate on; what a version *can* be told is
        // that its row member and its own `scopeKey` member agree, which is the
        // property a join needs and the one that did not hold.
        "dimensionKey": (!dimension_key.is_empty()).then_some(dimension_key.as_str()),
        "billingGranularity": billing_granularity.map(BillingGranularity::as_str),
        "tierAggregationWindow": tier_aggregation_window.map(TierAggregationWindow::as_str),
        "tierQualificationWindow": tier_qualification_window.map(TierQualificationWindow::as_str),
        "aggregationFunction": aggregation_function.map(AggregationFunction::as_str),
        "aggregationGranularity": aggregation_granularity.map(AggregationGranularity::as_str),
        "maxHoldGranules": max_hold_granules,
        // The **authored** declaration, unchanged -- the compile's own input,
        // which D-130 keeps in truth precisely so that it still exists on the
        // second run.
        "includedAllowance": included_allowance.map(|allowance| {
            let IncludedAllowance {
                quantity,
                rollover_policy,
            } = allowance;
            json!({ "quantity": quantity, "rolloverPolicy": rollover_policy.as_str() })
        }),
        // The first-class marker (`inst-ac-marker`). Display ("includes N
        // units") and the included-vs-billed reporting split read **this**, never
        // a `$0` first band -- a hand-authored free band carries no marker, and
        // that difference is what D-45 exists for. `authoredModelKind` rides
        // beside it per D-59 ("the authored kind retained beside the marker"):
        // it is what tells a reader the top band's rate is a folded
        // `unitRateNanoMinor` rather than an authored band. Neither the member
        // name nor the marker's is spelled anywhere in the design set; both are
        // minted here and reported as mints.
        "allowanceMarker": compiled.as_ref().map(|compiled| json!({
            "quantity": compiled.marker.quantity,
            "rolloverPolicy": compiled.marker.rollover_policy.as_str(),
            "source": compiled.marker.source.as_str(),
            "authoredModelKind": model_kind_wire(compiled.authored_kind),
        })),
        // The reservation pair (`inst-rv-attrs`), flat beside the row's other
        // authored facts for this function's stated reason. Rating sources the
        // self-service reserved rate from here rather than from Contracts
        // (`inst-rv-runtime`); the reserved *quantity* is runtime input and is
        // deliberately absent -- the catalog neither meters nor allocates it.
        "reservedRateNanoMinor": reserved_rate.map(crate::domain::money::RateMinor::nano_minor),
        "reservationFlavor": reservation_flavor.map(ReservationFlavor::as_str),
        // The typed floors and the discount hook. Enforcement is downstream --
        // Subscriptions at order time, Tariffs/Rating at eligibility, Promotions
        // for the instrument -- so the snapshot's job is to carry them verbatim
        // (`inst-ft-both`, `inst-dr-boundary`).
        "minQtyPurchase": min_qty_purchase,
        "minQtyUsage": min_qty_usage,
        "minQtyUsageFallback": min_qty_usage_fallback.map(MinQtyUsageFallback::as_str),
        "discountRef": discount_ref,
    })
}

/// One canonical scope key's window facts: its ordered intervals and the coverage
/// end derived from them (`inst-sg-surface`).
///
/// Destructured without a rest pattern for [`PlanSubjectDelta::to_value`]'s
/// reason.
fn key_windows_value(group: &KeyWindows) -> JsonValue {
    let KeyWindows {
        scope_key,
        intervals,
    } = group;
    json!({
        "scopeKey": scope_key_value(scope_key),
        "coverageEnd": coverage_end_value(group.coverage_end()),
        "intervals": intervals.iter().map(interval_value).collect::<Vec<_>>(),
    })
}

/// One window interval and the state it is held under.
fn interval_value(interval: &WindowInterval) -> JsonValue {
    let WindowInterval {
        effective_from,
        effective_to,
        state,
    } = interval;
    json!({
        "effectiveFrom": format_rfc3339(*effective_from),
        "effectiveTo": effective_to.map(format_rfc3339),
        "state": state.as_str(),
    })
}

/// The derived coverage end, as a **discriminated object** rather than a nullable
/// instant.
///
/// A bare `null` would have to stand for two answers that are opposites under the
/// D-80 horizon predicate — covered forever, and not covered at all — so a
/// consumer reading `"coverageEnd": null` could not tell "this key sells
/// indefinitely" from "this key sells nowhere". The `kind` token is what makes the
/// two distinguishable to a reader that has only the JSON, which is every reader
/// this payload has. See [`CoverageEnd`](crate::domain::window::CoverageEnd).
fn coverage_end_value(end: crate::domain::window::CoverageEnd) -> JsonValue {
    json!({ "kind": end.as_str(), "at": end.at().map(format_rfc3339) })
}

/// One tier band. The open top is rendered as `null` rather than as a sentinel
/// number: [`BandTop::Open`](crate::domain::price_row::BandTop::Open) is a
/// *state* of the band (D-17), and a number standing in for it would be a
/// quantity a reader could compare against.
fn band_value(band: &TierBand) -> JsonValue {
    let TierBand {
        from_qty,
        to_qty,
        unit_price_rate,
    } = band;
    json!({
        "fromQty": from_qty,
        "toQty": to_qty.closed_at(),
        // Renamed, not reinterpreted (D-311): the member carried whole minor
        // units and now carries 10^-9 of one. Reusing the name would multiply
        // every rendered band rate by a billion in any consumer that did not
        // know — silently. An unknown name reads `undefined`, which is visible.
        "unitPriceNanoMinor": unit_price_rate.nano_minor(),
    })
}

/// The ten canonical axes, in the normative order §4.1 fixes.
///
/// Rendered axis by axis rather than through [`ScopeKey`]'s `Display`: the
/// display form is one string for a log line, and a consumer resolving a row
/// matches on axes.
fn scope_key_value(key: &ScopeKey) -> JsonValue {
    // Destructured through `parts()`, so an eleventh axis is a compile error here
    // rather than a member missing from the read model — which is how the last
    // widening of this key (D-196) reached production with three sites unchanged.
    let ScopeKeyParts {
        plan_id,
        currency,
        region,
        price_overlay,
        phase,
        price_eligibility,
        charge_kind,
        cohort,
        meter,
        dimension_key,
    } = key.parts();
    json!({
        "planId": plan_id.get(),
        "currency": currency.as_str(),
        "region": region.as_str(),
        "priceOverlay": price_overlay.as_str(),
        "phase": phase.get(),
        "priceEligibility": price_eligibility.as_str(),
        "chargeKind": charge_kind.as_str(),
        "cohort": cohort.generation().map(format_rfc3339),
        // Axes 9 and 10 (D-196). `null` rather than the rendering's `none`
        // sentinel on a row that has no line: the rendering needs fixed arity
        // because it is embedded in strings, a JSON member does not, and the
        // read side already reads `cohort` back the same way. A consumer
        // resolving a metered plan needs the line here or it cannot tell one
        // published usage row of a market from another — which, before this
        // decision, could not happen because there could only be one.
        "meter": meter.map(Meter::as_str),
        "dimensionKey": (!dimension_key.is_none()).then(|| dimension_key.as_str()),
    })
}

/// Fold `from`'s members into `into`, which both callers built with `json!` and
/// which are therefore both objects.
///
/// A silent no-op on anything else is right here rather than a panic: the two
/// call sites are literals in this file, so a non-object is unreachable, and a
/// panic reachable from no input is a panic in a projection path that must not
/// have one.
fn merge(into: &mut JsonValue, from: JsonValue) {
    let (Some(target), JsonValue::Object(source)) = (into.as_object_mut(), from) else {
        return;
    };
    for (key, value) in source {
        target.insert(key, value);
    }
}

/// Every member of [`PlanSubjectDelta`], sorted by whether the evaluation-policy
/// roster reaches its content.
///
/// Returns `(reached, not_reached)`, each in declaration order. Both halves are
/// returned for [`partition_row_fields`](crate::domain::evaluation_policy::partition_row_fields)'s
/// reason: a member quietly filed on the harmless side would otherwise change
/// nothing anyone has to record, and the guard exists to make every member of
/// this payload a **stated** classification rather than a silent one.
///
/// # Why it is here and not one level down
///
/// `evaluation_policy`'s two guards destructure [`PriceRow`] and
/// [`PlanChangeContract`], so they are **structurally blind to a member added to
/// the delta itself** — and `composites` landed exactly there (D-298), a new key
/// in a payload an INSERT-only store freezes on the ≥ 7-year horizon, with
/// nothing obliging anyone to decide whether the generation had to move. Two
/// versions of one plan now carry the same `ep-4` stamp, one without the member
/// and one with it empty, and a consumer cannot tell *"this era did not carry
/// composites"* from *"this plan defines none"* (D-303).
///
/// It lives beside the type rather than beside the other two because
/// `evaluation_policy` is what this module reads for the generation string, and
/// the reverse import would be a module cycle.
///
/// # The pattern is the point
///
/// The `let PlanSubjectDelta {.. }` below has **no rest pattern**. A member
/// added to the payload is a compile error here until it is named on one side or
/// the other, which is the one step an author adding a member cannot route
/// around. The `delta` argument exists only to give that pattern something to
/// match.
#[must_use]
#[allow(
    clippy::too_many_lines,
    reason = "one line per payload member plus the two classified lists; splitting it would put \
              the destructure and the classification in different functions, which is exactly the \
              distance that let `composites` land unclassified"
)]
pub fn partition_delta_members(delta: &PlanSubjectDelta) -> (Vec<&'static str>, Vec<&'static str>) {
    /// Consume one destructured binding and yield the name it is classified
    /// under.
    ///
    /// **This is the guard, and D-309 is why it is a function rather than a
    /// literal list.** Every field below is bound by name and handed to this
    /// exactly once, so a member named in the pattern and forgotten in both
    /// classifications is an **unused variable** — denied, because this crate
    /// denies warnings. The first spelling counted the classified names against
    /// `22` and was armed backwards: a forgotten member left the count at 22 and
    /// passed, while classifying it correctly made 23 and failed. It fired on the
    /// fix and was silent on the omission.
    fn named<T>(name: &'static str, _binding: T) -> &'static str {
        name
    }

    let PlanSubjectDelta {
        plan_id,
        revision,
        lifecycle_state,
        sku_id,
        plan_tier,
        plan_tier_override,
        billing_cycle,
        frequency,
        available_from,
        available_to,
        purchase_min_qty,
        purchase_max_qty,
        invoice_grouping_key,
        phases,
        addon_rules,
        descriptor_set,
        period_floor_caps,
        composites,
        entitlement_grants,
        change_contract,
        prices,
        tax_projection,
        windows,
    } = delta;

    // The roster reaches these two through the guards that own them, so a field
    // added inside either is already a compile error one level down.
    let reached = vec![
        named("prices", prices),
        named("change_contract", change_contract),
    ];

    // Everything else, and each for a stated reason. `plan_tier` and its override
    // are the registry taxonomy and its audited divergence; the purchase bounds
    // gate a purchase rather than a rate; `invoice_grouping_key` is a Billing
    // layout hint (D-96); the phase set, the add-on rules and the descriptor set
    // are composition and presentation; `windows` and `tax_projection` answer
    // sellability and display, not derivation.
    //
    // **`composites` is the one that is arguable**, and it is filed here on
    // D-162 clause (5) rather than on the boundary test: a composite meter does
    // tell an evaluator how to derive one billable quantity from several
    // constituents — nearer the centre of that test than
    // `usage_counter_on_plan_change`, which *was* rostered — but the roster is
    // what replaying section 4.4's log produces, and no line exists. Adding one
    // would be code deciding a normative document's content. Owed to whoever
    // holds that section: a log line and the bump it implies, or a sentence
    // putting plan-scoped derived-meter definitions outside the roster on purpose.
    let not_reached = vec![
        named("plan_id", plan_id),
        named("revision", revision),
        named("lifecycle_state", lifecycle_state),
        named("sku_id", sku_id),
        named("plan_tier", plan_tier),
        named("plan_tier_override", plan_tier_override),
        named("billing_cycle", billing_cycle),
        named("frequency", frequency),
        named("available_from", available_from),
        named("available_to", available_to),
        named("purchase_min_qty", purchase_min_qty),
        named("purchase_max_qty", purchase_max_qty),
        named("invoice_grouping_key", invoice_grouping_key),
        named("phases", phases),
        named("addon_rules", addon_rules),
        named("descriptor_set", descriptor_set),
        // A period bound is money compared against a **period total** by
        // Billing after step 9; it selects no rate and derives no quantity, so
        // it is outside the D-162 roster on the boundary test rather than on
        // `composites`' clause-(5) technicality.
        named("period_floor_caps", period_floor_caps),
        named("composites", composites),
        named("entitlement_grants", entitlement_grants),
        named("tax_projection", tax_projection),
        named("windows", windows),
    ];
    (reached, not_reached)
}

#[cfg(test)]
#[path = "projection_tests.rs"]
mod projection_tests;
