//! The Slice-2 **plan-shape rule codes** (`design/02-plan-definition.md` §5).
//!
//! The sibling of [`crate::domain::rules`], and the same contract: the `const`
//! codes below are the machine-readable discriminators the design set names
//! **verbatim** in §5, they are what an RFC 9457 response carries and what the
//! conformance corpus asserts against — so they are written once, here, and
//! referenced at every `report.violate` site rather than spelled at each call
//! site. A code spelled twice is a code that can be spelled two ways.
//!
//! The four validators of this slice — `CycleShapeValidator`,
//! `CompositionValidator`, the `PhaseGraph` rules and the `DescriptorSet` rules
//! — each register their own
//! [`ValidationRule`](crate::domain::validation::ValidationRule)s over a
//! [`PlanShape`](crate::domain::plan_shape::PlanShape) into the Foundation's
//! fail-closed pipeline. They append and never short-circuit: a plan with a
//! broken phase chain *and* an incomplete descriptor set reports both, because
//! an author remediates a plan in one pass.
//!
//! **No code is declared here that nothing emits.** Every `const` below has a
//! rule behind it in this group; the ones §5 names and this gear cannot enforce
//! are enumerated in the next section instead, because a `const` for an
//! unraised code reads as enforcement to everyone who greps for it.
//!
//! ## Codes referenced and never redefined
//!
//! The revision-lifecycle refusals — `LIFECYCLE_FORBIDDEN`,
//! `PLAN_RETIRED_NO_SUCCESSOR`, `OPEN_DRAFT_REVISION_EXISTS`,
//! `PLAN_ABANDONED_NO_SUCCESSOR`, `STALE_VERSION`, `DUPLICATE_SCOPE_KEY` — are
//! **Foundation-owned** (§3.3) and are referenced from this slice, never
//! redeclared (R-11).
//!
//! ## What is deliberately absent
//!
//! The precedent is [`crate::domain::rules`]'s own absence section: *a rule
//! that always passes is indistinguishable from a rule that holds*. Six of the
//! codes §5 names are **not** declared here, and five of the six share one
//! reason — they are reads against the product/SKU registry read model, and
//! this gear has no registry client at all ([`crate::domain::ports`] holds the
//! `CatalogVersion` registry and nothing else).
//!
//! | Code | Why absent |
//! |---|---|
//! | `SKU_NOT_PUBLISHED` | the parent SKU's publication state lives in the product/SKU registry read model |
//! | `PLANTIER_DIVERGENT` | needs the parent SKU's `PlanTier`, from the same registry. The `PLANTIER_MISSING` half needs nothing external and **is** implemented |
//! | `ADDON_INCOMPATIBLE` (registry half) | "add-on SKUs published + compatible with the base SKU" is a registry read. The **plan-authored** half — an edge pointing outside the plan's own add-on set, and a conflicting pair with both sides `required` — needs nothing external and **is** implemented under this code, because D-16 made those edges plan-authored |
//! | `ADDON_OVERRIDE_UNRESOLVED` | needs two things this gear does not have: the add-on SKU's plans (a registry join) and `PriceWindow` coverage for the covering-member-key half (Slice 7 storage, D-95 / D-97 / D-116) |
//! | `METER_USAGE_TYPE_UNBOUND` | the registry metering-unit declaration's `usageTypeRef` (UC3(a)) |
//! | `METER_DIMENSION_UNDECLARED` | the `UsageType`'s declared `metadata_fields` keys (UC3(c)) |
//!
//! **A seventh absence, and the only one the design set names no code for at
//! all.** `inst-cs-customfreq` states two requirements. `n > 0` and
//! `n <= cap` is implemented, under [`INVALID_CUSTOM_INTERVAL`]. Anchor
//! compatibility — `customEveryN Days(n)` MUST anchor `subscription_start` —
//! is not, because `billing_anchor_policy` is a **Slice-6** `pricing_price`
//! column (`06-consumer-contracts.md` §6) that does not exist in this schema
//! and must not be added here. Adding it would be Slice 2 minting Slice 6's
//! column, and the rule over it would then belong to whichever slice built it
//! first. Stretching [`INVALID_CUSTOM_INTERVAL`] to cover the anchor half was
//! rejected for the same reason `SETUP_ROW_INVALID` does not absorb the
//! `billingTiming` check: a code reported for something §5 does not say it
//! reports is a code no consumer can act on.
//!
//! **The eighth and ninth are signals rather than codes.**
//! `inst-cmp-tier-drift` and the drift arm of `inst-cmp-usagetype` both consume
//! a **registry change signal** this gear has no lane for — no inbound port, no
//! subscriber, no job — and both write the operator-plane flag store rather
//! than failing a publish. That store **exists**: `pricing_operator_flag` is a
//! table on both backends (D-85) carrying both flag names in its `CHECK`
//! already, and it has no repository. The absence is therefore the *signal*,
//! not the storage — worth stating, because a reader who finds the table would
//! otherwise reasonably conclude the feature is wired.
//!
//! ## Rules cross-referenced here and registered elsewhere
//!
//! Three requirements Slice 2's instructions state are **owned by other
//! slices**, and a second registration of any of them is two owners of one
//! rule, free to disagree:
//!
//! - `billingTiming` REQUIRED on every recurring row is **Slice 6's**
//!   (`inst-cs-recurring` says so in as many words: cross-referenced here,
//!   never re-registered);
//! - `billingGranularity` on all usage rows and `tierAggregationWindow` on
//!   tiered / `package` rows are **already implemented** as Slice 3's
//!   [`EVAL_POLICY_MISSING`](crate::domain::rules::EVAL_POLICY_MISSING), which
//!   `inst-cs-usage` cross-references;
//! - `taxCategory` is each row's Slice-4 `tax_category_ref` (D-110) — never a
//!   descriptor-set column and never a Slice-2 check.

pub mod composite;
pub mod composition;
pub mod cycle_shape;
pub mod descriptor_set;
pub mod period_floor_cap;
pub mod phase_graph;

use crate::domain::plan_shape::PlanShape;
use crate::domain::validation::ValidationPipeline;

pub use cycle_shape::CustomIntervalBounds;
pub use descriptor_set::DescriptorSetComplete;

// ---------------------------------------------------------------------------
// Cycle shape (design/02-plan-definition.md 5, cpt-cf-bss-pricing-algo-cycle-shape)
// ---------------------------------------------------------------------------

/// A plan that has not said how it charges (`inst-cs-declared`, D-149 clause 2).
///
/// **One code, two fields, and the field is named in the report.** An unset
/// `billing_cycle`, and an unset `frequency` on a cycle that carries a recurring
/// part, are one code by D-146's line: the operator's next action is identical —
/// author the field the plan is missing — and a second code would be a second
/// thing to look up for the same remedy.
///
/// It is evaluated **before every other rule of the cycle-shape step**, and that
/// order is the whole reason the code exists. `billing_cycle` is nullable while a
/// plan is authored incrementally (§4.2 step 1), and every other rule of the step
/// is cycle-conditioned: each correctly declines to judge a plan whose cycle it
/// cannot read. So without it a `NULL` cycle passes the entire step **vacuously**
/// and reaches publish unjudged — a plan that has said nothing about how it
/// charges is exactly the plan a fail-closed validator cannot protect.
pub const CYCLE_METADATA_MISSING: &str = "CYCLE_METADATA_MISSING";

/// A sold `(currency, region)` with no base row of the kind its cycle mandates
/// (`inst-cs-onetime` / `inst-cs-recurring`, D-149 clause 1).
///
/// One rule over two `chargeKind` values, because the two instructions state one
/// sentence twice: the *at least one* half of the one-time rule and the `>= 1`
/// half of the recurring rule. A one-time plan selling in a market with no
/// `one_time` row is a plan that can be bought there for nothing.
///
/// It is [`USAGE_MARKET_INCOMPLETE`]'s base-side sibling and takes its shape
/// deliberately: same subject key, same "an absence is not a free market"
/// posture. [`HYBRID_INCOMPLETE`] keeps its own meaning — a part missing
/// **anywhere**, never naming a market — so the two never contest a case.
pub const BASE_MARKET_INCOMPLETE: &str = "BASE_MARKET_INCOMPLETE";

/// A custom interval `n` that is non-positive or over the configured cap
/// (`inst-cs-customfreq`, P1; the caps are the tenant's, from their
/// `pricing_policy_object` entry, else the ratified deployment default —
/// D-152). Over-cap is rejected at authoring rather than silently clamped.
pub const INVALID_CUSTOM_INTERVAL: &str = "INVALID_CUSTOM_INTERVAL";

/// A `hybrid` plan missing one of its two mandatory parts (`inst-cs-hybrid`).
pub const HYBRID_INCOMPLETE: &str = "HYBRID_INCOMPLETE";

/// A priced `(meter, dimensionKey)` line with no usage row in a
/// `(currency, region)` the plan sells (D-84).
///
/// The "sold but unrateable" state D-15/D-17 declare impossible by
/// construction: a hybrid selling recurring in EUR and USD with usage priced
/// only in EUR is sellable in USD, and the USD subscriber's usage events fail
/// closed. A market where usage is genuinely free is an explicit `$0` row,
/// never an absence.
pub const USAGE_MARKET_INCOMPLETE: &str = "USAGE_MARKET_INCOMPLETE";

/// A `one_time_setup` row on a plan whose cycle does not admit one, or one
/// carrying recurrence, `billingTiming` or tier fields (`inst-cs-setup`).
pub const SETUP_ROW_INVALID: &str = "SETUP_ROW_INVALID";

/// `purchase_min_qty > purchase_max_qty` (`inst-cs-onetime`).
///
/// Raised at **both** stages since a review: at publish by the rule,
/// and at the authoring write by `api::rest::plans`'
/// `require_authorable_purchase_window`, which runs the same rule over the submitted
/// shape. Before that door it was a declared code with **no raiser** —
/// `chk_pricing_plan_purchase_qty` refuses the row and every publish subject is
/// assembled from storage, so the inverted window reached the caller as a `500`
/// naming a constraint.
pub const PURCHASE_QTY_RANGE_INVALID: &str = "PURCHASE_QTY_RANGE_INVALID";

/// The plan's name is present but not a name: blank, or longer than the bound
/// (D-318).
///
/// Its own code rather than a reuse, because the fault is neither a missing
/// field nor a field on the wrong shape — the column is nullable and an unnamed
/// plan is perfectly legal. What is refused is a *second spelling* of unnamed
/// (`""`, or whitespace) and a value no surface can render.
pub const PLAN_NAME_INVALID: &str = "PLAN_NAME_INVALID";

/// A **newly set or changed** `availableFrom` in the past, on any billing cycle
/// (`inst-cs-availability`).
///
/// The rule is cycle-independent (hoisted from the one-time step)
/// and binds only changed values: a revision re-publishing an
/// unchanged date that has legitimately passed is not backdating, and blocking
/// it would make every later re-publish of a once-future-dated plan impossible
/// until the operator erased the date. The Slice-5 historical-import path is
/// the only sanctioned backdating.
pub const AVAILABLE_FROM_IN_PAST: &str = "AVAILABLE_FROM_IN_PAST";

// ---------------------------------------------------------------------------
// Composition (cpt-cf-bss-pricing-algo-composition)
// ---------------------------------------------------------------------------

/// No `PlanTier` declared at publish (`inst-cmp-plantier`). Optional at draft,
/// required at publish.
pub const PLANTIER_MISSING: &str = "PLANTIER_MISSING";

/// Two priced lines for one `(meter, dimensionKey)` within one scope-key slice
/// (`inst-cmp-injective`, D-103).
///
/// Injectivity is **per line per slice**, not per plan: `currency`, `region`,
/// `priceOverlay`, `phase`, `priceEligibility` and `cohort` legitimately
/// multiply rows, and a plan MAY price several `meteringUnit`s. Only a
/// duplicate line *within* one slice is the ambiguity that fails publish.
pub const METER_AMBIGUOUS: &str = "METER_AMBIGUOUS";

/// A dependency cycle over the plan-authored `depends_on` edges
/// (`inst-cmp-addons`, D-16).
pub const ADDON_CYCLE: &str = "ADDON_CYCLE";

/// A plan-authored add-on edge pointing outside the plan's own add-on set, or a
/// conflicting pair with both sides `required` (`inst-cmp-addons`, D-16).
///
/// The registry half of this code — add-on SKUs published and compatible with
/// the base SKU — is absent; see the module doc.
pub const ADDON_INCOMPATIBLE: &str = "ADDON_INCOMPATIBLE";

/// An add-on quantity bound that makes the add-on unselectable
/// (`inst-cmp-addons`, D-150).
///
/// Three bounds, one code, the offending bound named: `required => maxQty >= 1`,
/// `minQty <= maxQty` when both are set, and `stepQty > 0` when set. Each is an
/// unsatisfiable-selection defect on one row, and the sibling of
/// [`PURCHASE_QTY_RANGE_INVALID`] one object over.
///
/// **The three `CHECK`s on `pricing_plan_addon_rule` stay.** The rule is the
/// explanatory path and the constraint the guarantee — the read-then-index
/// arrangement D-148 states — so what changes is that an author gets a report
/// line naming the bound instead of a driver error naming a constraint, and gets
/// **all** the offending bounds at once rather than whichever one the database
/// hit first.
pub const ADDON_QTY_RANGE_INVALID: &str = "ADDON_QTY_RANGE_INVALID";

// ---------------------------------------------------------------------------
// Phase schedule (cpt-cf-bss-pricing-algo-phases)
// ---------------------------------------------------------------------------

/// A `convertsToPhaseId` chain that dangles or cycles, or a phase set without
/// **exactly one** terminal phase (`inst-ph-graph`).
pub const PHASE_GRAPH_INVALID: &str = "PHASE_GRAPH_INVALID";

/// A chain that skips the ordinal order, branches, or leaves a phase
/// unreachable from the entry phase (`inst-ph-graph`, review fix).
///
/// Acyclicity plus a single terminal alone admit dead phases that still demand
/// coverage rows and leave "first" undefined.
pub const PHASE_CHAIN_NONLINEAR: &str = "PHASE_CHAIN_NONLINEAR";

/// A terminal phase whose `kind` is not `evergreen`.
///
/// The constraint was carried only by a parenthetical while terminality is
/// structural and the column admits all three kinds, so nothing rejected a
/// `trial`-terminal chain — which leaves "the first non-trial phase" undefined
/// for setup timing and D-39 and collides with the
/// `display_trial_days = phase_duration_days` CHECK. "Interim pricing forever" is
/// an `evergreen` terminal phase at the interim price, not an `interim` terminal.
pub const TERMINAL_PHASE_KIND_INVALID: &str = "TERMINAL_PHASE_KIND_INVALID";

/// A non-terminal phase without `phaseDurationDays`, or a terminal phase with
/// one (`inst-ph-duration`).
pub const PHASE_DURATION_INVALID: &str = "PHASE_DURATION_INVALID";

/// `displayTrialDays` on a phase that is not a `trial`, or on one carrying no
/// `phaseDurationDays` to project (`inst-ph-trial`, D-151).
///
/// **§6's `CHECK` cannot carry either half, and is deliberately not tightened.**
/// `CHECK (display_trial_days IS NULL OR display_trial_days = phase_duration_days)`
/// is silent on `kind` altogether, and SQL's NULL propagation makes it
/// *satisfied* whenever `phase_duration_days` is NULL while `display_trial_days`
/// is set — both engines count a NULL comparison as passing. So the shape it
/// exists to forbid, a phase publishing a trial length it does not have, passed
/// it.
///
/// The `evergreen` **terminal** phase is where nothing else caught it either:
/// `inst-ph-duration` is correct to find no duration on a terminal phase and
/// `inst-ph-graph`'s terminal-`kind` rule is correct to find `evergreen`. Both
/// pass, and `displayTrialDays` is the single source Subscriptions enforces trial
/// runtime from and preview quotes — so a plan with **no trial phase at all**
/// could publish a trial length.
///
/// D-151 keeps the `CHECK` as written, NULL propagation and all: a phase graph is
/// authored across successive `PATCH`es, and a `phase_duration_days IS NOT NULL`
/// conjunct would make the half-authored draft unsavable. The schema stands
/// **behind** this rule rather than in front of it.
///
/// **A third fault, and the one the `CHECK` stands in front of** (
/// review, H8): two numbers set and disagreeing. The rule reported only the two
/// faults above, so the `CHECK` was the first thing to read the pair and the author
/// was answered `500 … please retry later` about two numbers they had typed. That
/// arm is `violate_at_write` and runs at the `phases` facet's door as well as at
/// publish, where it can have no subject at all; its two siblings stay at publish
/// for D-151's reason, being states the store accepts.
pub const DISPLAY_TRIAL_DAYS_INVALID: &str = "DISPLAY_TRIAL_DAYS_INVALID";

/// A price row whose `phase` scope-key axis names a phase the revision does not
/// attach (`inst-ph-row-attached`, D-337).
///
/// The exact inverse of [`PHASE_UNCOVERED`], and its immediate neighbour in the
/// pipeline so the pair reads together in one report.
///
/// [`PHASE_IN_USE`] guards the published set through the baseline; a draft has no
/// baseline, so `TerminalPhaseStable` returns at its first line and before this
/// code existed a revision whose every row keyed on a phase nobody authored was
/// refused only for having no terminal phase. Nothing else could see it either:
/// `inst-ph-coverage` iterates the phases the revision *holds* and asks which
/// lack rows, so a row filed outside that set is never a subject it visits.
///
/// The finding names the phase **and** the row because a scope key is the row's
/// identity and cannot be re-pointed: the only remediations are attaching that
/// exact phase id or deleting the row, and an author told merely to add a
/// terminal phase strands every row they authored.
pub const PHASE_ROW_ORPHANED: &str = "PHASE_ROW_ORPHANED";

/// A phase with no covering recurring row for a sold `(currency, region)`, on a
/// plan whose cycle carries a recurring part (`inst-ph-coverage`, D-15).
///
/// A phase conversion must never resolve to nothing, and the row-based Slice-7
/// coverage check cannot see a phase that has no rows at all.
pub const PHASE_UNCOVERED: &str = "PHASE_UNCOVERED";

/// A phase-scoped usage row whose `(meter, dimensionKey)` line has no
/// phase-invariant terminal-phase row (`inst-ph-usage-invariant`, D-117).
///
/// Without the base row the D-89 unit guard has no comparison target, D-84's
/// per-market completeness exempts the line entirely as an additive override,
/// and after the phase converts the line resolves to **nothing** — "sold but
/// unrateable" through the override door.
pub const PHASE_OVERRIDE_ORPHANED: &str = "PHASE_OVERRIDE_ORPHANED";

/// A phase-scoped usage override that changes a unit- or counter-determining
/// field of the terminal-phase row it overrides (`inst-ph-override-units`,
/// D-89, extended by D-122).
///
/// The tier counter `Q` is keyed `(subscription, meter, dimensionKey, window)`
/// and is **phase-blind**, so conversion never resets it: a `per_hour` trial row
/// converting into a `per_day` evergreen row mid-window applies an
/// hours-denominated `Q` to day-denominated bands. Free-trial pricing stays
/// fully expressible — a `$0` rate at the same denomination.
pub const PHASE_OVERRIDE_UNIT_MISMATCH: &str = "PHASE_OVERRIDE_UNIT_MISMATCH";

/// A revision re-terminalizing an existing phase or introducing a different
/// terminal phase (`inst-ph-terminal-stable` (a), D-64).
///
/// The scope-key phase default and usage phase-invariance are both defined
/// relative to *which* phase is terminal, so re-terminalizing moves them
/// silently: a usage-only plan published on an implicit terminal `T0`, revised
/// to add a trial plus a new evergreen phase, leaves its metered row no longer
/// phase-invariant and a subscription in the new phase resolving no usage row.
pub const TERMINAL_PHASE_CHANGED: &str = "TERMINAL_PHASE_CHANGED";

/// A revision dropping a phase still referenced by a current published price
/// row (`inst-ph-terminal-stable` (b), D-64).
pub const PHASE_IN_USE: &str = "PHASE_IN_USE";

// ---------------------------------------------------------------------------
// Descriptors (cpt-cf-bss-pricing-algo-descriptors)
// ---------------------------------------------------------------------------

/// A missing element of the descriptor required-set (`inst-ds-required`, D-48
/// v1 as recomposed by D-110).
///
/// The set is the **three** descriptor-set fields plus whatever the tenant's
/// config-extensible required-set adds (P5, carried per tenant in
/// `pricing_policy_object` — D-152). `billingTiming` and `taxCategory`
/// are row-borne and are **not** checked here: the first is Slice 6's
/// registered rule and the second is each row's `tax_category_ref` (Slice 4),
/// and a second registration of either would be two owners of one rule, free to
/// disagree.
pub const DESCRIPTOR_INCOMPLETE: &str = "DESCRIPTOR_INCOMPLETE";

// ---------------------------------------------------------------------------
// Period floor / cap (cpt-cf-bss-pricing-algo-period-floor-cap)
// ---------------------------------------------------------------------------

/// A period floor/cap authored on a `(currency, region)` the plan prices
/// nothing in (`inst-pfc-market`, **D-319**).
///
/// The mirror image of [`BASE_MARKET_INCOMPLETE`] and
/// [`USAGE_MARKET_INCOMPLETE`], which ask whether every sold market carries the
/// rows it owes. This one asks whether an authored bound has a market to apply
/// in: a floor on a market the plan does not sell is not a smaller floor, it is
/// no floor at all, frozen into an immutable snapshot on a plan whose author
/// believes it has a minimum.
pub const PERIOD_FLOOR_CAP_MARKET_UNSOLD: &str = "PERIOD_FLOOR_CAP_MARKET_UNSOLD";

/// A period bound that admits no bill (`inst-pfc-amount`, **D-319**): a `0`
/// floor, a `0` cap, a floor above its cap, or a row authoring neither.
///
/// One code for four faults, the offending bound named —
/// [`ADDON_QTY_RANGE_INVALID`]'s arrangement (D-150), because the operator's
/// next action is the same shape in every case. Each of them is also a `CHECK`
/// on `pricing_plan_period_floor_cap`: the constraint is the guarantee and this
/// is the explanatory path (D-148).
///
/// The explanatory path needs a door to be one, and until a review
/// it had none: all four faults reached the caller as a `500` naming a
/// constraint, and none was raisable at publish because the `CHECK`s keep the row
/// the rule judges out of the table it is read back from. `api::rest::plans`'
/// `require_authorable_period_bounds` is the door, on the `period_floor_caps`
/// facet — [`ADDON_QTY_RANGE_INVALID`]'s door one table over, which is what the
/// arrangement above always described.
pub const PERIOD_FLOOR_CAP_AMOUNT_INVALID: &str = "PERIOD_FLOOR_CAP_AMOUNT_INVALID";

/// Every Slice-2 plan-shape rule, in report order.
///
/// **Ordered by theme** — cycle shape, then composition, then the phase
/// schedule, then the descriptors — so a report reads the way a plan is
/// authored: what commercial shape it is, what it is made of, how it runs over
/// time, and how it is billed. That is [`crate::domain::rules::price_row_rules`]'s
/// arrangement applied to a subject one level up. Order is cosmetic to the
/// verdict, because the pipeline never short-circuits and a single blocking
/// violation blocks the publish wherever it appears — but the report is the
/// product, and an author reads it top-down.
///
/// One ordering preference was considered and not adopted: running D-84's
/// per-market completeness **after** the phase rules, so an author meets a
/// broken phase graph before a completeness finding computed over it. The theme
/// order is kept instead, because grouping by algorithm is the property a reader
/// can predict from the design set, and a single exception to it is a rule
/// nobody can locate.
///
/// # Two rules are configured **per tenant**, and that is why this takes arguments
///
/// [`CustomIntervalBounds`] carries the `customEveryN` caps and
/// [`DescriptorSetComplete`] carries P5's required-field list. Both hold their
/// configuration as **fields**, because [`crate::domain::validation`] requires a
/// rule to be pure with respect to the state it is handed; a rule that read
/// configuration inside `evaluate` could answer differently in the two runs
/// below for no authored reason. Neither can be defaulted into existence here:
/// `CustomIntervalBounds` deliberately has no `Default`, since zero caps reject
/// every custom frequency ever authored while looking exactly like a rule that
/// is switched on.
///
/// Both values are **the authoring tenant's** (D-152) — their
/// `pricing_policy_object` entry, falling back per column to the ratified
/// deployment default — so the caller resolves them for the tenant whose plan is
/// being judged and hands them in. That resolution is a storage read against
/// `crate::config`'s defaults, and neither is anything the domain may import,
/// which is the whole of why this function has parameters instead of a body that
/// looks them up:
///
/// ```ignore
/// let policy = policies.authoring_policy(scope, tenant_id).await?;
/// plan_shape_rules(policy.interval_bounds(), policy.descriptor_rule())
/// ```
///
/// The pipeline is therefore built per authoring run rather than once at init.
/// A pipeline held across tenants would enforce whichever tenant's limits it was
/// built for, and one held across time would keep rejecting plans after an
/// operator had raised the cap.
///
/// # What this pipeline is **not**: it is not run here
///
/// G4 registers; the publish unit executes. `01-foundation.md` §4.2 runs this
/// set **twice** on the way to a publish — as the step-2 pre-check when the
/// author submits, and again inside the commit transaction, because approval
/// approves *content* while the commit re-validates *state* and the world moved
/// between the two. No rule may therefore carry anything between runs, and none
/// of the rules below holds mutable state at all.
///
/// **No count of the rules below**, because this is a stated invariant *with a
/// scope* and a reviewer auditing it walks as far as the number says and stops.
/// The invariant holds for all of them — the two that look stateful,
/// `PhaseGraphIntegrity` and `RowPhaseAttached`, hold a `Stage` that is
/// configuration.
#[must_use]
pub fn plan_shape_rules(
    interval_bounds: CustomIntervalBounds,
    descriptors: DescriptorSetComplete,
) -> ValidationPipeline<PlanShape> {
    ValidationPipeline::new()
        // Cycle shape: what commercial shape the plan is.
        //
        // `CycleDeclared` is **first**, and the order is load-bearing rather
        // than cosmetic (D-149 clause 2): every other rule of this step is
        // cycle-conditioned and correctly declines to judge a plan whose cycle
        // it cannot read, so a NULL cycle passed the whole step vacuously.
        .with_rule(Box::new(cycle_shape::CycleDeclared))
        .with_rule(Box::new(interval_bounds))
        .with_rule(Box::new(cycle_shape::HybridCompleteness))
        .with_rule(Box::new(cycle_shape::BaseMarketCompleteness))
        .with_rule(Box::new(cycle_shape::UsageMarketCompleteness))
        .with_rule(Box::new(cycle_shape::SetupRowShape))
        .with_rule(Box::new(cycle_shape::PurchaseQtyRange))
        .with_rule(Box::new(cycle_shape::AvailableFromNotBackdated))
        // Composition: what the plan is made of.
        .with_rule(Box::new(composition::PlanNameWellFormed))
        .with_rule(Box::new(composition::PlanTierDeclared))
        .with_rule(Box::new(composition::MeterInjectivity))
        .with_rule(Box::new(composition::AddonEdgeMembership))
        .with_rule(Box::new(composition::AddonDependencyAcyclic))
        .with_rule(Box::new(composition::AddonConflictBothRequired))
        .with_rule(Box::new(composition::AddonQtyRange))
        // Slice 10's derived meters. Plan-shape rather than row-local because a
        // transitive self-reference is a cycle across two definitions and no
        // single row can see one.
        .with_rule(Box::new(composite::CompositeArity))
        .with_rule(Box::new(composite::CompositeSelfReference))
        // Phase schedule: how the plan runs over time.
        // `default()` since a review gave this rule a stage too, and
        // the default is `Publish` for `RowPhaseAttached`'s reason below: this
        // pipeline **is** the publish pre-check and the commit's re-validation, and
        // the write-stage instance is the `phases` facet's own.
        .with_rule(Box::new(phase_graph::PhaseGraphIntegrity::default()))
        .with_rule(Box::new(phase_graph::PhaseChainLinear))
        .with_rule(Box::new(phase_graph::TerminalPhaseKind))
        .with_rule(Box::new(phase_graph::PhaseDuration))
        .with_rule(Box::new(phase_graph::DisplayTrialDaysOnTrialPhase))
        // Immediately before `PhaseCoverage`, and the adjacency is D-337's: the
        // two are exact inverses over one relation — a row with no phase, a phase
        // with no rows — so a report carrying both names them together, and an
        // author reading "phase P is uncovered" above "row R keys on phase Q,
        // which this revision does not attach" has the whole mismatch in front of
        // them.
        // `default()` rather than a unit struct since D-342 gave the rule a stage,
        // and the default is `Publish` — the aggregate pipeline is the publish
        // pre-check and the commit's re-validation, and the write-stage instance is
        // the `phases` facet's own, built at that door.
        .with_rule(Box::new(phase_graph::RowPhaseAttached::default()))
        .with_rule(Box::new(phase_graph::PhaseCoverage))
        .with_rule(Box::new(phase_graph::PhaseOverrideBase))
        .with_rule(Box::new(phase_graph::PhaseOverrideUnits))
        .with_rule(Box::new(phase_graph::TerminalPhaseStable))
        // Period floor/cap: what the plan bills at least, and at most, per
        // period. After the phase schedule and before the descriptors: it is a
        // fact about a whole period's total, so it reads after the rules that
        // establish what the periods are.
        .with_rule(Box::new(period_floor_cap::PeriodFloorCapAmounts))
        .with_rule(Box::new(period_floor_cap::PeriodFloorCapMarketSold))
        // Descriptors: how the plan is billed.
        .with_rule(Box::new(descriptors))
}

#[cfg(test)]
#[path = "plan_rules_tests.rs"]
mod plan_rules_tests;
