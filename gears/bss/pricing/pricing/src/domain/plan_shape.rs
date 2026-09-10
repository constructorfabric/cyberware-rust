//! The **shape of a plan** — the Slice-2 vocabulary
//! (`design/02-plan-definition.md`).
//!
//! Slice 3 owns what one price row costs; this slice owns what the plan around
//! those rows *is*: the billing-cycle matrix and its frequency metadata, the
//! phase graph with its `convertsToPhaseId` chain, the add-on composition
//! rules, the billing descriptor set, and the availability and
//! purchase-quantity window the plan sells in. Nothing here judges anything —
//! the four validators (`CycleShapeValidator`, `CompositionValidator`,
//! `PhaseGraph` rules, `DescriptorSet` rules) register into
//! [`crate::domain::validation`] the way the Slice-3 rules do, and every code
//! they report is declared once in [`crate::domain::plan_rules`].
//!
//! **Every field is public and unvalidated**, for the reason
//! [`crate::domain::price_row`] states: the value is the *subject* a
//! [`ValidationPipeline`](crate::domain::validation::ValidationPipeline) runs
//! over, and a constructor that refused an unpublishable combination would move
//! the rules into the type — where they cannot be enumerated into one aggregate
//! report, which is the whole point of the fail-closed pipeline. A plan is
//! authored incrementally while it is `draft` (`inst-pa-attach`), so this type
//! has to be able to hold a shape nobody has finished yet.
//!
//! ## `evaluated_at` is a field, never a clock read inside a rule
//!
//! [`crate::domain::validation`] states that the same rule set runs **twice** —
//! as a pre-check at submit and again inside the publish commit — and that a
//! rule must therefore be pure with respect to the state it is handed. A rule
//! that read the wall clock would be neither reproducible nor testable, and it
//! would answer differently in the two runs for no authored reason.
//! `AVAILABLE_FROM_IN_PAST` (`inst-cs-availability`) is the one Slice-2 rule
//! that needs a now, so the now is handed to it on
//! [`PlanShape::evaluated_at`].
//!
//! ## `rows` is the candidate set, and `baseline` is the published past
//!
//! [`PlanShape::rows`] is the row set **the publish would produce**, supplied
//! by the caller that holds the plan. No Slice-2 rule filters it by
//! `lifecycle_state`: published-state facts arrive through
//! [`PlanShape::baseline`] instead. A rule that re-derived "which rows count"
//! would be a second answer to a question the publish unit already answered,
//! and the two answers are free to disagree the day the publish unit changes
//! what it feeds in.
//!
//! ## `windows` is the plane the coverage rules range over, and it is
//! deliberately unfiltered
//!
//! [`PlanShape::windows`] carries the plan's window plane grouped per canonical
//! scope key — the input `inst-wc-required`, `inst-wc-perkey`, `inst-fg-detect`
//! and `inst-wc-availability` are evaluated against
//! ([`crate::domain::coverage`]).
//!
//! **It is subject content, not rule configuration**, which is why it is a field
//! here rather than an argument on
//! [`PublishRuleParams`](crate::domain::publish::rules::PublishRuleParams). That
//! type carries the world *outside* the subject a rule compares against — the
//! tenant's default rounding policy, its size caps, its descriptor requirements
//! — and a window set is not outside the subject: a version freezes the
//! intervals (D-99, D-121) and a reviewer approves what will be frozen. So the
//! approval content pin covers it, and
//! [`content_hash`](crate::domain::approval::content_pin::content_hash) hashes
//! it; a pin over less than the change set is a pin a window schedule walks
//! through.
//!
//! **The read behind it is draft-inclusive and filters no window state, and that
//! is the substance rather than an oversight.** The distinction is between
//! *validating a hypothetical* and *asserting a fact*:
//!
//! - The coverage rules and `window_repo`'s overlap check ask "**if** this row
//!   were current on this key, would the interval set be sound?" — over a change
//!   set made of `draft` rows about to publish. Filtering by lifecycle state
//!   there would let two intervals on one key pass by having one of them belong
//!   to a row the filter dropped, which is exactly what
//!   `price_repo::load_scope_keys_for_plan` states as its own reason for
//!   carrying no filter.
//! - `read_model::project_windows` and the activation sweep **assert facts to a
//!   consumer**, and both restrict to
//!   [`PROJECTED_ROW_STATES`](crate::domain::projection::PROJECTED_ROW_STATES)
//!   — the sweep by a subquery, added after it was found flipping a `draft`
//!   row's window and emitting `PriceWindowActivated` for it.
//!
//! A later group narrowing this read to make it match the projection's would be
//! moving it across that line. Which intervals *contribute coverage* is the
//! reader's question and each reader states its own answer:
//! [`KeyWindows::coverage_end`](crate::domain::window::KeyWindows::coverage_end)
//! drops `cancelled`, and [`crate::domain::coverage`] names the state set
//! `inst-wc-required` and `inst-fg-detect` read.
//!
//! ## The `ALL` lists live here, and the parsing does not
//!
//! Each of the four token enums exports an `ALL` const — one member per
//! variant, declaration order — in the shape
//! [`LifecycleState::ALL`](crate::domain::lifecycle::LifecycleState::ALL) uses.
//! The *parsing* stays at the storage boundary, where a column is read; what is
//! exported is the **variant list**, not a parser.
//!
//! `price_repo.rs` argues the opposite for the Slice-3 enums — it hand-writes
//! ten inverse `const` lists beside its own use of `ModelKind::ALL` and
//! `LifecycleState::ALL` — so a reader will notice the disagreement. The reason
//! this side is chosen here is that Slice 2's tokens have **three** consuming
//! call sites ahead of them, not one: the plan-shape repository, the read-model
//! projector and the authoring surface each have to turn a stored token back
//! into a value. Three hand-maintained copies of one enum's variant list is
//! three chances for a variant added later to go missing from a list that
//! still compiles, and the failure mode is a stored row read back as
//! `CorruptRow` — or worse, matched by a stale list that quietly no longer
//! covers the enum. One authority for "what values exist", three call sites
//! that map it.
//!
//! ## One name this module introduces
//!
//! The persisted token for a custom frequency is spelled **`custom_every_n`**
//! here, and **no document fixes it**. §6 decomposes the concept into a
//! `frequency` column plus `custom_interval_n` / `custom_interval_unit`, and
//! the PRD writes it as `customEveryN{Days|Months}(n)` — neither states what
//! the `frequency` column holds when the frequency is the custom one. The
//! spelling follows the `snake_case` convention every other persisted token in
//! this crate uses (`one_time_setup`, `all_subscriptions`, `whole_unit`); it is
//! recorded here because a token invented in code and never written down is
//! one a later document is free to contradict.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde_json::Value as JsonValue;
use toolkit_macros::domain_model;
use uuid::Uuid;

use crate::domain::contracts::{EntitlementGrants, PlanChangeContract};
use crate::domain::money::{CurrencyCode, MinorAmount};
use crate::domain::price_record::PriceRecord;
use crate::domain::scope_key::{ChargeKind, PhaseId, PlanId, Region};
use crate::domain::window::KeyWindows;
use time::OffsetDateTime;

/// The §17.1 billing-cycle matrix: what commercial shape the plan is.
///
/// The three predicates below are each named for the **rule that reads them**,
/// so no rule re-derives the cycle set for itself. That matters more than it
/// looks: `inst-ph-coverage` was scoped to `recurring`/`hybrid` by the
/// review fix precisely because its literal reading blocked one-time
/// and usage-only plans through their implicit terminal phase, and a rule that
/// re-spelled the set would be free to re-acquire the defect.
#[domain_model]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum BillingCycle {
    /// A single charge; no recurrence.
    OneTime,
    /// A subscription fee on a [`Frequency`].
    Recurring,
    /// Metered consumption only.
    Usage,
    /// A recurring base **and** a metered part on one `planId`, on distinct
    /// `chargeKind` keys.
    Hybrid,
}

impl BillingCycle {
    /// Every cycle, stable order.
    pub const ALL: &'static [Self] = &[Self::OneTime, Self::Recurring, Self::Usage, Self::Hybrid];

    /// Does the plan carry a recurring part?
    ///
    /// This is `inst-ph-coverage`'s **scope**: phase coverage by recurring rows
    /// is required on `recurring` and `hybrid` plans and on nothing else,
    /// because a one-time or usage-only plan has no recurring row that could
    /// ever cover a phase and would fail a rule it can never satisfy.
    #[must_use]
    pub const fn has_recurring_part(self) -> bool {
        matches!(self, Self::Recurring | Self::Hybrid)
    }

    /// Does the plan owe a metered part?
    ///
    /// `inst-cs-hybrid`'s half of `HYBRID_INCOMPLETE`, and the scope of D-84's
    /// per-market line completeness: on a `hybrid` plan the usage part is
    /// required **per sold market**, not merely somewhere, or the plan is
    /// sellable in a market whose usage events fail closed.
    #[must_use]
    pub const fn requires_usage_part(self) -> bool {
        matches!(self, Self::Usage | Self::Hybrid)
    }

    /// May the plan carry a `one_time_setup` row (`inst-cs-setup`)?
    ///
    /// It answers `true` on the same two cycles as
    /// [`BillingCycle::has_recurring_part`] and is deliberately **not**
    /// implemented in terms of it: the two are different sentences of the
    /// design set — one scopes phase coverage, one permits a setup row — that
    /// happen to name the same cycles today. Deriving one from the other would
    /// make a future divergence a silent change to the wrong rule.
    #[must_use]
    pub const fn admits_setup_row(self) -> bool {
        matches!(self, Self::Recurring | Self::Hybrid)
    }

    /// The persisted / wire token.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OneTime => "one_time",
            Self::Recurring => "recurring",
            Self::Usage => "usage",
            Self::Hybrid => "hybrid",
        }
    }
}

impl fmt::Display for BillingCycle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The unit a custom recurring interval counts in.
///
/// The two units are not interchangeable and they do not share a cap: PRD §14
/// ratified `customEveryNDays <= 366` and `customEveryNMonths <= 24`, so
/// `INVALID_CUSTOM_INTERVAL` reads the unit before it reads the bound
/// ([`crate::config::LimitsConfig`]).
#[domain_model]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CustomIntervalUnit {
    /// Days.
    Days,
    /// Months.
    Months,
}

impl CustomIntervalUnit {
    /// Every unit, stable order.
    pub const ALL: &'static [Self] = &[Self::Days, Self::Months];

    /// The persisted / wire token.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Days => "days",
            Self::Months => "months",
        }
    }
}

impl fmt::Display for CustomIntervalUnit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The recurring frequency, with the custom interval **riding the variant**.
///
/// §6 persists this as three columns — `frequency`, `custom_interval_n`,
/// `custom_interval_unit` — and that decomposition admits two states no plan
/// may hold: a `monthly` row carrying an interval, and a custom row carrying
/// none. Here the payload is part of the variant, so neither is representable
/// and no rule has to exist to reject them. The storage boundary is where the
/// three columns are reassembled into this, and that is the only place the
/// illegal pairing can appear at all.
///
/// The `custom_every_n` token is this module's own naming decision; see the
/// module doc.
#[domain_model]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Frequency {
    /// Every calendar month.
    Monthly,
    /// Every three months.
    Quarterly,
    /// Every six months.
    Semiannual,
    /// Every twelve months.
    Annual,
    /// `customEveryN{Days|Months}(n)`: every `n` of `unit`.
    CustomEveryN {
        /// The interval count. `> 0` and within the unit's configured cap
        /// (`INVALID_CUSTOM_INTERVAL`) — checked by the rule, not here.
        n: u32,
        /// What `n` counts.
        unit: CustomIntervalUnit,
    },
}

impl Frequency {
    /// The interval a [`Frequency::CustomEveryN`] member of
    /// [`Frequency::ALL`] carries.
    ///
    /// It is a **placeholder**, and it is named so that no call site can mistake
    /// it for data. A custom frequency's interval lives in two sibling columns,
    /// so the `custom_every_n` token cannot reconstruct it and this value is
    /// the only thing a variant list has to put there.
    pub const CUSTOM_INTERVAL_PLACEHOLDER: u32 = 1;

    /// Every frequency, stable order — **one member per variant**.
    ///
    /// The four fixed frequencies are whole values. The custom member is a
    /// *variant* representative carrying
    /// [`Frequency::CUSTOM_INTERVAL_PLACEHOLDER`] and
    /// [`CustomIntervalUnit::Days`]: this list answers "what values exist", and
    /// for the one variant whose payload rides two other columns
    /// (`custom_interval_n`, `custom_interval_unit`) the token alone cannot
    /// answer it. **A reader that matches a stored `custom_every_n` against
    /// this list MUST rebuild `n` and `unit` from those columns** rather than
    /// keeping what it found here — the placeholder is a shape, not an
    /// interval.
    pub const ALL: &'static [Self] = &[
        Self::Monthly,
        Self::Quarterly,
        Self::Semiannual,
        Self::Annual,
        Self::CustomEveryN {
            n: Self::CUSTOM_INTERVAL_PLACEHOLDER,
            unit: CustomIntervalUnit::Days,
        },
    ];

    /// The persisted / wire token of the `frequency` column.
    ///
    /// A custom frequency renders as the bare discriminator: `n` and `unit`
    /// are their own columns, so folding them into this string would give the
    /// column two spellings of one value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Monthly => "monthly",
            Self::Quarterly => "quarterly",
            Self::Semiannual => "semiannual",
            Self::Annual => "annual",
            Self::CustomEveryN { .. } => "custom_every_n",
        }
    }
}

impl fmt::Display for Frequency {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The `kind` column of a phase — **never** its terminality.
///
/// C-4 exists because the two were conflated: terminality is structural
/// (`converts_to_phase_id IS NULL`) while this column independently admits all
/// three values, so nothing rejected a `trial`-terminal chain. That chain
/// leaves "the **first non-trial phase**" undefined for both setup timing
/// (`inst-cs-setup-timing`) and migration entry (D-39), and collides with the
/// `display_trial_days = phase_duration_days` CHECK. The rule that closes it is
/// `TERMINAL_PHASE_KIND_INVALID`; this type deliberately still **represents**
/// the illegal combination, because a rule with nothing to reject cannot be
/// tested.
#[domain_model]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PhaseKind {
    /// A free or discounted trial phase; publishes `displayTrialDays`.
    Trial,
    /// Any time-boxed non-trial phase before the terminal one — an introductory
    /// price, an onboarding period, a ramp step. The token names the **slot**,
    /// not a business meaning: the operator's word for the phase is
    /// [`PlanPhase::display_name`] (D-357). It was `intro` until D-358, which
    /// renamed it because the word promised "introductory pricing" while nothing
    /// in this gear ever behaved differently for it — the only kind-dependent
    /// rules are `trial`'s and the terminal's.
    Interim,
    /// The steady-state phase. The only legal kind for the terminal phase.
    Evergreen,
}

impl PhaseKind {
    /// Every kind, stable order.
    pub const ALL: &'static [Self] = &[Self::Trial, Self::Interim, Self::Evergreen];

    /// The persisted / wire token.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Trial => "trial",
            Self::Interim => "interim",
            Self::Evergreen => "evergreen",
        }
    }
}

impl fmt::Display for PhaseKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One derived (composite) meter definition (Slice 10 §6, A4, D-32, D-106).
///
/// The formula travels as **data** — operands plus operator/weights — and never
/// as executable code, which is A4's rule and the reason this carries an opaque
/// [`JsonValue`] rather than anything this crate evaluates: the catalog persists
/// and freezes the definition, and Rating computes from the snapshot
/// (`inst-cm-frozen`). Nothing here reads inside it.
///
/// `composite_id` is stable across revisions (D-106): opening a draft copies the
/// row under the new `plan_revision` without re-minting the id, so a formula edit
/// on a draft leaves the published revision byte-identical.
#[domain_model]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompositeMeter {
    /// Stable across revisions; the definition's identity, not its copy.
    pub composite_id: Uuid,
    /// The registry-declared unit this composite rates as (`inst-cm-output`).
    pub output_unit: String,
    /// The constituent unit ids. `>= 2` is `CompositeArity`'s rule; whether each
    /// is *published* is unasked, because there is no registry client here.
    pub constituent_units: Vec<String>,
    /// The formula as data. Opaque to this crate on purpose (A4).
    pub formula: JsonValue,
}

/// One row of `pricing_plan_phase`, as the rules read it.
///
/// Not `Copy` since D-357: the label is an owned string.
#[domain_model]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanPhase {
    /// The phase's identity and the value of the `phase` scope-key axis.
    ///
    /// Stable across plan revisions (D-56 + D-83): a new revision **copies**
    /// the phase rows under its own `plan_revision` and never re-mints the id,
    /// so continuing price rows keep pointing at the same phase.
    pub phase_id: PhaseId,
    /// `trial` | `interim` | `evergreen`. Not terminality; see [`PhaseKind`].
    pub kind: PhaseKind,
    /// The operator's label — "Onboarding", "Ramp 2" — free-form and optional
    /// (D-357). Not an identity (that is `phase_id`), not a scope-key axis, and
    /// carrying no behaviour: `kind` answers what the phase *does*, this answers
    /// what the operator *calls* it. Versions with the revision like every other
    /// field here (D-83). `None` on the implicit terminal phase a non-phased plan
    /// is given (D-19), which nobody authors.
    pub display_name: Option<String>,
    /// The phase's position in the chain. The **lowest** ordinal is the entry
    /// phase (`inst-ph-graph`); see [`PhaseGraph::entry`].
    pub ordinal: i32,
    /// Where this phase converts to. `None` **is** terminality.
    pub converts_to_phase_id: Option<PhaseId>,
    /// How long the phase lasts. Required `> 0` on a non-terminal phase and
    /// forbidden on the terminal one (`inst-ph-duration`,
    /// `PHASE_DURATION_INVALID`): `convertsToPhaseId` says *where* a phase
    /// converts and this says *when*.
    pub phase_duration_days: Option<u32>,
    /// The PRD-named projection of [`PlanPhase::phase_duration_days`] on a
    /// `trial` phase (`inst-ph-trial`) — one value, two projections, with a
    /// `CHECK` on the table guarding drift between the two persisted columns.
    pub display_trial_days: Option<u32>,
}

impl PlanPhase {
    /// Is this the terminal phase?
    ///
    /// **Structural, never the kind.** The absence of a successor is what makes
    /// a phase terminal; [`PlanPhase::kind`] is an independent column, and
    /// answering this question from it is exactly the conflation C-4 was
    /// written to undo.
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        self.converts_to_phase_id.is_none()
    }
}

/// The ordered phase set with its `convertsToPhaseId` edges.
///
/// It has **reading accessors and no validating constructor**. The four
/// derivations below are the ones more than one rule needs, and they live here
/// so that "the terminal phase", "the entry phase" and "the ordinal order" have
/// exactly one definition each — a rule free to re-derive "first phase" as
/// "first authored" would silently disagree with `inst-ph-graph`, which says
/// lowest ordinal, and no column records authoring order at all.
///
/// [`PhaseGraph::new`] accepts any set: an empty graph, a graph with two
/// terminals and a graph with a dangling edge are all constructible, because
/// they are precisely the subjects `PHASE_GRAPH_INVALID` and
/// `PHASE_CHAIN_NONLINEAR` exist to reject.
#[domain_model]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PhaseGraph {
    phases: Vec<PlanPhase>,
}

impl PhaseGraph {
    /// Wrap an authored phase set, **unvalidated**.
    #[must_use]
    pub fn new(phases: Vec<PlanPhase>) -> Self {
        Self { phases }
    }

    /// The phases, in the order they were authored.
    ///
    /// Bound through an exhaustive destructure rather than `&self.phases`, and
    /// that is a guard rather than a style: this is the **only** way the content
    /// pin reaches a phase graph (`put_plan_shape` frames `phases.phases()`), so
    /// a second field on this type would be a graph-level fact the pin silently
    /// omits — a reviewer approving one graph and a commit publishing another
    /// with every digest equal. The pattern has no `..`, so that field stops this
    /// accessor compiling and the decision gets made.
    #[must_use]
    pub fn phases(&self) -> &[PlanPhase] {
        let Self { phases } = self;
        phases
    }

    /// Every phase with no successor.
    ///
    /// A `Vec` rather than an `Option`, because **zero and two are exactly the
    /// states `PHASE_GRAPH_INVALID` reports** and an `Option` collapses them
    /// into one another: `None` cannot distinguish "no terminal phase" from a
    /// first-wins pick among two. The partial `UNIQUE` on the table forbids the
    /// two-terminal case once the rows are written, but the pipeline runs
    /// *before* the write and §6 says so in as many words — an index cannot
    /// enforce the `>= 1` half.
    #[must_use]
    pub fn terminals(&self) -> Vec<&PlanPhase> {
        self.phases
            .iter()
            .filter(|phase| phase.is_terminal())
            .collect()
    }

    /// The phases sorted by [`PlanPhase::ordinal`], ascending.
    ///
    /// The sort is stable, so two phases sharing an ordinal keep their authored
    /// order rather than swapping between runs — a duplicate ordinal is an
    /// authoring fault `PHASE_CHAIN_NONLINEAR` reports, and a report whose
    /// contents depended on sort luck would be unreproducible.
    #[must_use]
    pub fn in_ordinal_order(&self) -> Vec<&PlanPhase> {
        let mut ordered: Vec<&PlanPhase> = self.phases.iter().collect();
        ordered.sort_by_key(|phase| phase.ordinal);
        ordered
    }

    /// The entry phase: the **lowest ordinal**, normative per `inst-ph-graph`.
    ///
    /// Not the first authored. D-39's "first non-trial phase" and
    /// `inst-cs-setup-timing`'s both read this order, so if the two readings
    /// could differ, setup would be charged at one phase and migration entry
    /// computed at another. Among equal lowest ordinals the first authored
    /// wins, which is a deterministic tie-break rather than a meaningful one.
    #[must_use]
    pub fn entry(&self) -> Option<&PlanPhase> {
        self.phases.iter().min_by_key(|phase| phase.ordinal)
    }

    /// **The** terminal phase, when [`Self::terminals`] names exactly one.
    ///
    /// `None` is *"the question is not answerable"* and not *"there is no
    /// terminal phase"*: zero terminals and two terminals are both states
    /// `PHASE_GRAPH_INVALID` reports, and every rule that needs the terminal
    /// phase is undefined in both. A first-wins pick among two would give D-84's
    /// completeness rule a referent the author never authored, which is why
    /// [`Self::terminals`] answers with a `Vec` and this answers with an
    /// `Option` over it rather than instead of it.
    ///
    /// It lives here beside [`Self::terminals`], [`Self::entry`] and
    /// [`Self::find`] because `plan_rules`' module doc makes terminality one of
    /// the three readings derived in exactly one place — *"a disagreement between
    /// two derivations of one fact is invisible; both readings look right at
    /// their own call site"*. Split across `phase_graph` and `cycle_shape` it is two
    /// byte-identical copies, each with its own doc arguing the same point.
    #[must_use]
    pub fn sole_terminal(&self) -> Option<PhaseId> {
        match self.terminals().as_slice() {
            [only] => Some(only.phase_id),
            _ => None,
        }
    }

    /// The phase carrying `phase_id`, when the graph holds one.
    ///
    /// `None` is what a dangling `convertsToPhaseId` looks like from inside a
    /// rule, and what a price row on a phase this revision dropped looks like
    /// to `PHASE_IN_USE`.
    #[must_use]
    pub fn find(&self, phase_id: PhaseId) -> Option<&PlanPhase> {
        self.phases.iter().find(|phase| phase.phase_id == phase_id)
    }
}

/// One row of `pricing_plan_addon_rule`: an add-on SKU the plan composes with,
/// its quantity bounds, and its plan-authored graph edges.
///
/// The edges are **plan-authored** (D-16) and hold **other add-on SKU ids of
/// this same plan's add-on set** — an edge pointing outside the set fails
/// publish under `ADDON_INCOMPATIBLE`, and a cycle over
/// [`AddonRule::depends_on`] fails under `ADDON_CYCLE`. They are not registry
/// compatibility metadata: if the registry ever declares intrinsic SKU
/// compatibility it becomes an *additional* validation input the authored edges
/// are checked against, which is additive and needs no migration (D-16).
#[domain_model]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AddonRule {
    /// The add-on SKU this rule is about, and the discriminator that lets one
    /// revision hold several rules (D-105).
    pub addon_sku_id: Uuid,
    /// Whether the add-on must be taken. A required add-on needs
    /// `max_qty >= 1`, and two required add-ons that conflict fail publish.
    pub required: bool,
    /// Selection-time lower bound.
    pub min_qty: Option<u32>,
    /// Selection-time upper bound.
    pub max_qty: Option<u32>,
    /// Selection-time quantity step.
    pub step_qty: Option<u32>,
    /// An optional alternative price for this add-on when taken with this plan.
    ///
    /// It resolves to a published `priceId` on a plan of the **add-on SKU
    /// itself** and binds that row's canonical scope key as a key *family*
    /// modulo the market axes (D-97 as amended by D-116). Resolving it needs
    /// the add-on SKU's plans and `PriceWindow` coverage, neither of which this
    /// gear has; see [`crate::domain::plan_rules`].
    pub price_override_ref: Option<Uuid>,
    /// Add-on SKU ids this one requires. The `ADDON_CYCLE` walk runs over this
    /// set.
    pub depends_on: Vec<Uuid>,
    /// Add-on SKU ids this one excludes. Stored normalized symmetric, so a
    /// conflict authored on one side is a conflict on both.
    pub conflicts_with: Vec<Uuid>,
}

/// The per-plan billing descriptor aggregate (`pricing_plan_descriptor_set`).
///
/// **Three named fields, not five.** D-48 pinned a five-element v1 contract and
/// D-110 revised its composition: `billingTiming` and `taxCategory` **ride the
/// price row** — the first because Slice 6 owns the rule, the second because
/// `tax_category_ref` is per row and a per-plan column cannot mirror a per-row
/// source of truth (the promised consistency check was undefined the moment two
/// rows of one plan carried different categories). Adding either back as a
/// column here would be a second, disagreeing home for a value that already has
/// one.
///
/// [`DescriptorSet::additional`] is what makes P5's "config-extensible
/// required-set without a schema change" reachable: a deployment that must
/// require a fourth descriptor names it in configuration and carries its value
/// here, instead of waiting for a migration.
#[domain_model]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DescriptorSet {
    /// The invoice line template Billing renders from.
    pub invoice_line_template: Option<String>,
    /// The general-ledger code the posting lands on.
    pub gl_code: Option<String>,
    /// How the plan's charges are composed into invoice lines.
    pub itemization_rule: Option<String>,
    /// Extra descriptor keys a deployment's required-set names (P5).
    pub additional: BTreeMap<String, String>,
}

/// The plan-level **period floor and cap** in one market (S2 §6,
/// `inst-pfc-*`, **D-319**).
///
/// *"This plan bills at least X per period in this market"* — money compared
/// against a **period total**, not a price. Nothing in this gear evaluates it:
/// Rating reads it out of the pinned snapshot and emits a
/// `PeriodFloorCapObligation`, and **Billing** applies `max(total, floor)` /
/// `min(total, cap)` after step 9 (rating PRD `fr-period-floor-cap-obligation`).
///
/// # Three things it is not
///
/// - **Not a quantity floor.** `min_qty_purchase` / `min_qty_usage` are on the
///   price row and bound a *quantity*; rating §6.2 forbids conflating the two.
/// - **Not a committed-spend pool.** Negotiated commitments with a monetary
///   true-up are Contracts' system of record (rating T-D-14); this is a
///   self-service catalog field with no true-up and no contract.
/// - **Not cohort-scoped.** The `cohort` axis selects a *key*, and a
///   subscription pins one price id per line, so a subscription whose lines
///   straddle two generations has no single cohort for a bound to be read
///   under. The bound is therefore a fact about the plan revision, and a
///   grandfathered generation is answerable to the revision's bound at the
///   version its subscription is pinned to (D-319).
///
/// # Why the currency is not a field
///
/// [`PeriodFloorCap::currency`] **is** the denomination of both amounts. The
/// same doctrine [`MinorAmount`](crate::domain::money::MinorAmount) states for
/// a price row: pairing a currency into the amount creates a second place to
/// disagree about which currency the money is in.
#[domain_model]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PeriodFloorCap {
    /// ISO 4217 — the first half of the market, and the denomination of both
    /// amounts below.
    pub currency: CurrencyCode,
    /// The second half of the market pair.
    pub region: Region,
    /// The period floor in minor units of [`PeriodFloorCap::currency`].
    ///
    /// Strictly positive when present: a `0` floor is `max(total, 0)`, which
    /// the per-line non-negative guard already guarantees, so the two spellings
    /// would name one state (`PERIOD_FLOOR_CAP_AMOUNT_INVALID`).
    pub floor_minor: Option<MinorAmount>,
    /// The period cap, same denomination and same positivity rule.
    pub cap_minor: Option<MinorAmount>,
}

impl PeriodFloorCap {
    /// The market this bound is filed under.
    #[must_use]
    pub fn market(&self) -> (CurrencyCode, Region) {
        (self.currency.clone(), self.region.clone())
    }
}

/// What the plan's **current published revision** holds, for the three Slice-2
/// rules that are comparisons rather than predicates.
///
/// `inst-cs-availability` and both arms of `inst-ph-terminal-stable` are each
/// defined against the *previous* published state, and a rule with no referent
/// has only two behaviours available to it, both wrong: pass vacuously, which
/// is the guard silently absent, or treat every value as changed, which blocks
/// every re-publish. Those are precisely the two failure modes the three fixes
/// were written to prevent — the availability rule is narrowed
/// so that re-publishing an **unchanged** `availableFrom` that has since passed
/// is not backdating, and D-64 added the terminal-phase arms because
/// `inst-ph-coverage` cannot see a usage-only plan at all.
///
/// `None` on [`PlanShape::baseline`] means a **first publish**: there is no
/// previous state, so all three rules have nothing to compare and say nothing.
#[domain_model]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublishedBaseline {
    /// The terminal `phase_id` of the published revision. It is immutable for
    /// the life of the plan (`TERMINAL_PHASE_CHANGED`).
    pub terminal_phase_id: PhaseId,
    /// The phases referenced by the plan's current published `pricing_price`
    /// rows. A revision that drops one of these fails with `PHASE_IN_USE`.
    pub phase_ids_in_use: BTreeSet<PhaseId>,
    /// The published `available_from`, so a re-publish of an unchanged value
    /// is distinguishable from newly backdating one.
    pub available_from: Option<OffsetDateTime>,
    /// The published `available_to`.
    ///
    /// **Nothing reads it.** `infra::publish::published_baseline` populates it and
    /// no rule in the crate consumes it: `AvailableFromNotBackdated` is the only
    /// rule that reads the baseline at all, and it reads
    /// [`Self::available_from`]. Stated the way
    /// [`PriceContent::supersedes_price_id`](crate::domain::price_record::PriceContent::supersedes_price_id)
    /// states its own unread status. Saying "for the same reason" as its sibling
    /// instead reads as a guard that exists, and sends a reader looking for the rule
    /// that consumes it.
    ///
    /// It is carried rather than dropped because the availability rule's
    /// `available_to` half is the obvious next registration and the operand would
    /// otherwise have to be re-plumbed to write it.
    pub available_to: Option<OffsetDateTime>,
}

/// The subject the Slice-2 pipeline runs over: one plan revision's whole shape,
/// the rows its publish would produce, and the published state it is measured
/// against.
///
/// It is a **candidate**, not a stored aggregate. The plan revision, its phase
/// rows, its add-on rules, its descriptor set and its price rows live in five
/// tables; this is the one value that carries all of them at once, because a
/// slice whose rules are per-market completeness, phase coverage and meter
/// injectivity cannot be evaluated from any one of them alone.
#[domain_model]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanShape {
    /// The plan under judgement.
    pub plan_id: PlanId,
    /// Which revision of it — the second half of the durable name
    /// `(plan_id, revision)` (D-145).
    pub revision: u64,
    /// The catalog SKU this revision binds, when one is bound.
    ///
    /// **Carried here because it is authored content**
    /// ([`PlanShapePatch::sku_id`](crate::domain::plan::PlanShapePatch::sku_id)
    /// moves it on an open draft) **and because the approval content pin hashes
    /// this type and nothing else.** A field a patch can move and the pin cannot
    /// see is a rebind that survives a re-verification: the reviewer approved a
    /// plan selling one SKU and the commit sells another, with every digest
    /// equal. No rule in [`crate::domain::plan_rules`] reads it yet — the
    /// tier-equality half of P3 is the one that will, and it needs the registry
    /// this gear does not call.
    pub sku_id: Option<Uuid>,
    /// The §17.1 cycle. `None` is an authored-but-unfinished draft, not a
    /// default: nothing in the matrix is implied.
    pub billing_cycle: Option<BillingCycle>,
    /// The recurring frequency, when the cycle has one.
    pub frequency: Option<Frequency>,
    /// The plan's tier. Optional at draft and required at publish
    /// (`PLANTIER_MISSING`).
    pub plan_tier: Option<String>,
    /// The plan's human label (D-318).
    ///
    /// Framed into the content pin (`v13`) rather than left out as a mere
    /// label: it is authored draft content a `PATCH` moves, so leaving it
    /// unframed would let a reviewer approve "Business Starter" and the commit
    /// publish "Enterprise Unlimited" with every digest equal — and the name is
    /// what a consumer surface shows the plan as.
    pub plan_name: Option<String>,
    /// Whether the tier deliberately diverges from the parent SKU's under an
    /// explicit audited override (P3). The equality half of the check needs the
    /// registry; see [`crate::domain::plan_rules`].
    pub plan_tier_override: bool,
    /// Start of the plan's availability window, UTC.
    pub available_from: Option<OffsetDateTime>,
    /// End of the plan's availability window, UTC.
    pub available_to: Option<OffsetDateTime>,
    /// Minimum purchasable quantity (one-time plans).
    pub purchase_min_qty: Option<u64>,
    /// Maximum purchasable quantity (one-time plans).
    pub purchase_max_qty: Option<u64>,
    /// The Billing invoice-layout hint (D-96). Shape-checked only; it never
    /// overrides the single-currency-per-invoice invariant.
    pub invoice_grouping_key: Option<String>,
    /// The phase chain.
    pub phases: PhaseGraph,
    /// The plan's add-on composition rules.
    pub addon_rules: Vec<AddonRule>,
    /// The billing descriptor set. `None` is an unauthored set, which
    /// `DESCRIPTOR_INCOMPLETE` reports the same way an incomplete one is
    /// reported.
    pub descriptor_set: Option<DescriptorSet>,
    /// The plan-level period floor/cap this revision publishes, one entry per
    /// market it is authored for (**D-319**).
    ///
    /// A `Vec` and not an `Option<..>`: the empty set **is** the unauthored
    /// state, and a plan that authored no bound and one that authored the empty
    /// set are the same plan — [`PlanChangeContract`]'s reason, one object over.
    /// The publish rules range over it against [`PlanShape::markets`], which is
    /// a fact about the row set and not about any one entry.
    pub period_floor_caps: Vec<PeriodFloorCap>,
    /// The candidate row set the publish would produce; see the module doc.
    pub rows: Vec<PriceRecord>,
    /// The entitlement grant set this revision publishes (Slice 6, §6, D-41):
    /// the plan-level set, the `PlanTier` it resolved from when it did, and any
    /// per-phase sets.
    ///
    /// Not an `Option`, for [`PlanChangeContract`]'s reason: the default **is**
    /// the unauthored state, so a plan that authored nothing and one that
    /// authored the empty set are the same plan and must not validate
    /// differently.
    pub entitlement_grants: EntitlementGrants,
    /// The derived (composite) meters this revision defines (Slice 10 §6,
    /// `inst-cm-formula`).
    ///
    /// A `Vec` and not a map: the self-reference walk needs the whole set at
    /// once, because a transitive cycle spans two definitions, and nothing
    /// addresses one composite by key inside the shape.
    pub composites: Vec<CompositeMeter>,
    /// The plan-change contract this revision publishes (Slice 6, §6): the
    /// edges a self-service change may travel, the comparability rank that
    /// classifies one, and D-113's tier-`Q` continuity flag.
    ///
    /// Not an `Option`: [`PlanChangeContract`]'s own default is the fail-safe
    /// (no edges, no rank, `reset`), so an unauthored plan and one that authored
    /// the fail-safe are the same plan and must not validate differently.
    pub change_contract: PlanChangeContract,
    /// The plan's window plane, one entry per canonical scope key; see the
    /// module doc for why it is here and why the read behind it is unfiltered.
    ///
    /// Built by `infra::publish::assemble` through
    /// [`group_by_key_seeded`](crate::domain::window::group_by_key_seeded),
    /// seeded with [`PlanShape::rows`]' keys — so a candidate key with no window
    /// at all is present with an empty interval list rather than absent, and
    /// reads as [`CoverageEnd::Uncovered`](crate::domain::window::CoverageEnd).
    /// Keys the candidate set does not mention are here too when a window sits
    /// on one: a `superseded` predecessor's shortened interval is part of its
    /// key's coverage, and that key's successor is a candidate row.
    pub windows: Vec<KeyWindows>,
    /// The plan's current published revision, when it has one. `None` is a
    /// first publish; see [`PublishedBaseline`].
    pub baseline: Option<PublishedBaseline>,
    /// The instant the pipeline is being run at; see the module doc.
    pub evaluated_at: OffsetDateTime,
}

impl PlanShape {
    /// An otherwise-empty shape for `plan_id` at `revision`, evaluated at
    /// `evaluated_at`.
    ///
    /// Every other field starts absent or empty, which is the shape of a plan
    /// nobody has authored yet — not a publishable one. There is deliberately
    /// no `Default`: `evaluated_at` would derive to the Unix epoch, and an
    /// availability rule measured against 1970 answers confidently and wrongly.
    #[must_use]
    pub fn new(plan_id: PlanId, revision: u64, evaluated_at: OffsetDateTime) -> Self {
        Self {
            plan_id,
            revision,
            sku_id: None,
            billing_cycle: None,
            frequency: None,
            plan_tier: None,
            plan_name: None,
            plan_tier_override: false,
            available_from: None,
            available_to: None,
            purchase_min_qty: None,
            purchase_max_qty: None,
            invoice_grouping_key: None,
            phases: PhaseGraph::default(),
            addon_rules: Vec::new(),
            descriptor_set: None,
            period_floor_caps: Vec::new(),
            rows: Vec::new(),
            entitlement_grants: EntitlementGrants::default(),
            composites: Vec::new(),
            change_contract: PlanChangeContract::default(),
            windows: Vec::new(),
            baseline: None,
            evaluated_at,
        }
    }

    /// How a plan-level finding locates its subject for the author.
    ///
    /// The plan-level sibling of
    /// [`PriceRow::subject`](crate::domain::price_row::PriceRow::subject), and
    /// here for the same reason: six of this slice's codes name the plan and
    /// nothing finer, so without one renderer they would name it six ways and a
    /// consumer grouping a report by subject would see six subjects.
    #[must_use]
    pub fn subject(&self) -> String {
        format!("{}/{}", self.plan_id, self.revision)
    }

    /// Every `(currency, region)` the plan sells in — the union over its rows.
    ///
    /// "Sold market" is a derived fact, and D-84's per-market completeness,
    /// `inst-ph-coverage`'s per-market coverage and the override-home check all
    /// range over it. Deriving it once means they cannot disagree about which
    /// markets a plan is in.
    #[must_use]
    pub fn markets(&self) -> BTreeSet<(CurrencyCode, Region)> {
        self.rows.iter().map(market_of).collect()
    }

    /// The markets in which the plan carries a row of `charge_kind`.
    ///
    /// D-84 is a statement about two of these sets: every market with a
    /// recurring row must also carry the plan's usage lines.
    #[must_use]
    pub fn markets_with(&self, charge_kind: ChargeKind) -> BTreeSet<(CurrencyCode, Region)> {
        self.rows
            .iter()
            .filter(|record| record.scope_key.charge_kind() == charge_kind)
            .map(market_of)
            .collect()
    }

    /// The plan's metered rows.
    #[must_use]
    pub fn usage_rows(&self) -> Vec<&PriceRecord> {
        self.rows
            .iter()
            .filter(|record| record.scope_key.charge_kind() == ChargeKind::Usage)
            .collect()
    }

    /// The rows filed on one phase.
    #[must_use]
    pub fn rows_on_phase(&self, phase_id: PhaseId) -> Vec<&PriceRecord> {
        self.rows
            .iter()
            .filter(|record| record.scope_key.phase() == phase_id)
            .collect()
    }
}

/// The market axes of a row, read from the **scope key**.
///
/// Every derivation above reads the key rather than
/// [`PriceRow::charge_kind`](crate::domain::price_row::PriceRow::charge_kind),
/// which carries the same axis for the Slice-3 shape rules' convenience. The
/// two agree by construction, and picking one of them everywhere means no later
/// rule has to know that.
fn market_of(record: &PriceRecord) -> (CurrencyCode, Region) {
    (
        record.scope_key.currency().clone(),
        record.scope_key.region().clone(),
    )
}

#[cfg(test)]
#[path = "plan_shape_tests.rs"]
mod plan_shape_tests;
