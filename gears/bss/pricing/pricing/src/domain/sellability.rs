//! The sellability gate's surface: four of six predicates, answered per
//! canonical scope key from one pinned plan-subject delta (§3 `inst-sg-*`,
//! `dod-sellability`).
//!
//! # It publishes intervals, states and a derived coverage end — never a
//! point-in-time boolean
//!
//! Said once, here, because it is the constraint the whole module is shaped by
//! (D-99, `inst-sg-surface`, Foundation §4.4). Nothing this module produces is a
//! boolean about a moment: [`KeySellability`] carries the key's frozen intervals
//! and its [`CoverageEnd`], and *"is it sellable"* is a
//! [`PlanMarketVerdict`] computed **at** an `at` the caller supplies.
//!
//! A stored boolean would be an INSERT-only answer to a question about the
//! reader's clock, on a store whose contract is that a completed version never
//! changes — and it would make every activation and expiry owe a re-projection,
//! which is exactly what `inst-ws-publishunit` exempts them from.
//!
//! **The guard here is the type, and this is where that is said rather than
//! manufactured into a test.** No field of [`KeySellability`],
//! [`SellabilitySurface`], [`PredicateAnswer`] or [`PlanMarketVerdict`] is a
//! `bool`, and no test can assert that: a test reads values and the property is
//! about the declarations, so the compiler is what holds it. What the suite asserts
//! is the two things a test *can* reach — the vocabulary those types render into
//! (`every_answer_is_one_of_the_three_tokens`) and the absence of a boolean
//! anywhere in the rendered document, at any depth and under any spelling
//! (`the_document_carries_intervals_and_a_coverage_end_and_no_boolean_anywhere`,
//! in `tests/rest_windows.rs`). The wire is where the type guard could still be
//! lost, because a renderer can mint a field the domain never declared.
//!
//! # "Active at `t`" is derived from the interval, never read off the state token
//!
//! [`PlanSubjectDelta::windows`](crate::domain::projection::PlanSubjectDelta::windows)
//! states the rule and this module is its first consumer, so the argument is
//! worth repeating in the place that codes against it: the token is **stale by
//! construction** for every window the activation sweep ever flips, because D-99
//! makes an activation re-project nothing. Predicate (1) reading the token would
//! therefore make an activation owe a re-projection — contradicting the decision
//! that makes the window plane cheap — and until one arrived the key would read
//! unsellable forever behind a token frozen at `scheduled`.
//!
//! **This is a divergence with the design set, reported and not reconciled
//! here.** `inst-sg-surface` spells predicate (1) as *"an **active** window
//! covers `t` … scheduled-only is NOT sellable"*, while D-99 makes active-at-`t`
//! a read-time derivation from frozen intervals; nothing in the set reconciles
//! the two, and no register entry has been minted for it. The settled reading is
//! implemented — derive from `interval ∧ at`.
//!
//! **The two readings part on every input where some interval covers `at` and
//! none of the covering ones is `active`-tokened — a class, not a case.** They
//! agree on the one `inst-sg-surface`'s sentence names: a scheduled-only key is one
//! whose interval has not *started*, and it fails the derived predicate too. But
//! [`KeyWindows::covers_at`](crate::domain::window::KeyWindows::covers_at)
//! consults no token except `cancelled`, so the readings part wherever the
//! intervals covering `at` carry only the other two:
//!
//! - **`scheduled`, not yet swept** — the interval already covers `at` and the
//!   token is frozen behind an activation D-99 makes re-project nothing. The
//!   literal reading answers *not sellable* and would go on doing so forever,
//!   since nothing will ever re-project it.
//!   `a_window_the_sweep_has_not_flipped_is_active_from_its_interval_not_its_token`
//!   is that input.
//! - **`expired`, in the past** — an `at` inside an interval that has since ended.
//!   The literal reading finds no *active* window and refuses; this one answers
//!   off the interval that was effective at that instant. That is the whole
//!   **historical-query class**, which is what D-121 keeps `expired` in the delta
//!   *for*: a frozen version has to answer a past order instant the same way
//!   forever, and folding the token in would make the answer depend on when it was
//!   asked. `a_past_instant_inside_an_expired_interval_still_answers_covered` and
//!   `an_expired_only_key_answers_a_past_instant_off_both_halves` are that input,
//!   the second of them with no `active` token anywhere in the world.
//!
//! The readings disagree on **both** inputs, not just the first — understating the
//! class to a case hides the one a pinned consumer replaying a past order instant
//! lives in.
//!
//! **The horizon half diverges on the same axis and is no better worded.** §3
//! spells it over the key's "**active-plus-scheduled** coverage", while
//! [`KeyWindows::coverage_end`](crate::domain::window::KeyWindows::coverage_end)
//! filters `cancelled` and nothing else — exactly as `covers_at` does — so
//! `expired` contributes to it too. A key whose intervals are all expired reads
//! [`CoverageEnd::Ends`] where §3 read literally reads [`CoverageEnd::Uncovered`],
//! and the two halves of predicate (1) therefore diverge **together** rather than
//! one of them alone. The derived reading is right on both halves, for the
//! argument above.
//!
//! # Four of six, and the two that are not this gear's are named
//!
//! `inst-sg-pinned` (D-167 clause 3) is the authoritative split and the handoff's
//! *"three sellability predicates"* is loose: **(2)** committed-`CatalogVersion`
//! addressability, **(3)** `availableFrom`/`availableTo` and **(4)** the plan
//! lifecycle state are the three a pre-Slice-7 version already answers, and
//! **(1)** the active-window-plus-horizon predicate becomes answerable here.
//! That is four. The remaining two are [`PredicateAnswer::NotEvaluable`] with the
//! slice that owes each: **(5)** Slice 4's per-market GA-gate flags and Slice
//! 10's prepaid-execution gate, **(6)** D-46's registry `sellable` flag and
//! therefore the products registry gear, which is not in this repository.
//!
//! A consumer must be able to tell *"this predicate is false"* from *"this
//! version cannot evaluate this predicate"*, which is the whole of D-167 clause
//! (3) — so the two are different arms of one answer type rather than one arm
//! with a nullable detail.
//!
//! # Where each predicate's answer lives, and why that is not a weakening of
//! D-94
//!
//! (1) and (5) are **per key** — (1) reads the key's own windows, and §3 says (5)
//! is "evaluated **per scope key / market**, not per plan". (2), (3), (4) and (6)
//! are **plan-level**: (2) is a property of the version the answer was read from,
//! (3) and (4) read plan-subject fields, and (6) applies to the offered SKU.
//!
//! `inst-sg-conjunction` says every bound key "passes predicates (1)–(5)", and
//! the conjunction this module computes is
//! `plan answers ∧ ⋀ over keys (that key's answers)` — identical to
//! `⋀ over keys (plan answers ∧ that key's answers)` for a plan-scoped fact,
//! since the value is the same for every key. Publishing a plan-scoped answer
//! once instead of once per key is therefore a rendering choice and not a
//! narrowing, and it removes the one thing a per-key copy could do: disagree with
//! itself.
//!
//! # Two instructions this gear cannot break, recorded rather than tested
//!
//! - **`inst-sg-joint`** — the gate is a joint rule: the catalog *publishes* the
//!   surface and **Subscriptions enforces at order time**. Nothing here can
//!   create a subscription, so there is no refusal for this gear to get wrong.
//! - **`inst-sg-renewal`** — a renewal is never gate-checked. This surface
//!   answers about a **purchase**: it takes an `at` and a market and returns
//!   whether a *new* subscription may be created, and it carries nothing a caller
//!   could mistake for a renewal decision — no subscription id, no in-flight
//!   state, no entitlement.
//!
//! Manufacturing a test for a rule this gear cannot break would assert the absence
//! of a feature. What *is* asserted is the half that is ours, and on **both** sides
//! of it: the surface's **inputs** are a delta, an instant and a market and that is
//! the whole of them (`the_surface_takes_no_payer_and_holds_no_cache`), and what it
//! **carries** is pinned as the rendered document's whole member set, by equality
//! and at every depth, so a subscription id or an entitlement reaching the wire
//! reddens (`the_document_carries_no_member_a_caller_could_read_as_a_renewal_check`
//! in `tests/rest_windows.rs`).
//!
//! Both sides are held: with only the input side asserted, the sentence above about
//! what the surface carries rests on inspection alone.
//!
//! # Payer independence is the type, and the cache ban is an absence
//!
//! `inst-sg-segment-boundary`: all six predicates are **payer-independent** — the
//! gate does not check group membership, and a plan whose pricing targets a
//! customer group as a separate `planId` is operator-only at launch (RBAC is that
//! gate, not this one).
//!
//! `inst-sg-eligibility-gated` (p2, F-88) adds one obligation this phase carries,
//! and it is negative: **no global sellability cache keyed by plan alone**,
//! because the seventh predicate is the first payer-dependent one and a
//! payer-agnostic cache would have to be torn out to land it. There is no cache
//! here at all, so this is a **type-and-absence** guard rather than a rule with a
//! runtime test: [`SellabilitySurface::of_delta`] takes a delta, an instant and a
//! market, and no payer and no cache handle can be passed to it. The test that
//! holds it asserts the constructor's inputs, which is the only thing a test can
//! assert about an absence.
//!
//! # `inst-sg-bundle` is not evaluated, and the operand it needs is the **frozen**
//! one
//!
//! **Wiring the walk is a projection dependency, not a Slice-8 one.** There *is* a
//! bundle store and there *are* `bundle`-type plans; reading the absence as Slice
//! 8's sends the work to the wrong slice:
//!
//! - **there is a bundle store** — `pricing_bundle` and
//!   `pricing_bundle_component` (`pricing_bundle`/`_000025`), read by
//!   [`bundle_repo`](crate::infra::storage::repo::bundle_repo), with the
//!   authoring and publish routes mounted;
//! - **a `bundle`-type plan is resolvable** — it is exactly a plan carrying a
//!   `pricing_bundle` row, which `bundle_repo::find_by_plan` answers and which
//!   [`bundle_rules`](crate::domain::bundle_rules)' `COMPONENT_IS_BUNDLE` already
//!   asks at publish time;
//! - **there is no frozen component key set**, and that one is still true. It is
//!   the whole of the blocker.
//!
//! ## What is missing is a member of the pin, and nothing in this gear mints one
//!
//! `inst-sg-bundle` does not merely ask for a walk; it says *"the
//! `SellabilitySurface` **exposes the frozen component key set** for this walk"*.
//! This surface answers from one pinned
//! [`PlanSubjectDelta`](crate::domain::projection::PlanSubjectDelta), and that
//! payload has **no bundle member of any kind** — not the composition, not the
//! basis, not even whether the plan is a bundle at all. The word *bundle* does not
//! occur in `domain::projection`, `domain::read_model`, `infra::read_model` or
//! `infra::jobs`; `read_model_repo_tests`'
//! `the_payloads_members_partition_into_the_read_and_the_ignored` is the roster
//! that says so exhaustively, and it reddens the day one arrives.
//!
//! **And no act would ever put one there.** `inst-ba-return` says a bundle publish
//! leaves *"composition frozen into the read model / snapshot"*;
//! `infra::bundle::publish_composition` records **no**
//! `PendingVersionRow` — it writes the effective shares and one `BundleUpdated`
//! outbox event, and that is all — so a composition change advances no
//! `CatalogVersion` and re-projects no subject. Every other publish unit in this
//! gear records one (`publish`, `window`, `supersession`, `cutover`,
//! `overlay_publish`, `retirement`, `membership_publish`); the composition act is
//! the only one that does not. That clause of `inst-ba-return` is unbuilt, and it
//! is upstream of this one.
//!
//! ## Why this is not the seam `infra::synthesis` wires empty
//!
//! Tier 2 of `SnapshotSynthesizer` has no store either, and it is nonetheless
//! *called* — with an empty candidate set — because the **shape** of the question
//! is known there: a key and an instant, with only the candidates absent. Here the
//! discriminator itself is absent. [`SellabilityFacts`] cannot say whether the
//! plan is a bundle, so there is no arm to wire empty: a
//! [`bundle_verdict`](crate::domain::bundle_sellability::bundle_verdict) over an
//! empty component set answers [`PlanMarketVerdict::NotSellable`] (that module's
//! empty-conjunction rule), so calling it for every plan would refuse the whole
//! catalog, and calling it for none is what this code already does. An empty call
//! is only honest when the caller knows it is the right caller.
//!
//! ## Which way the gap rounds, and that it is latent
//!
//! Reported rather than left to be discovered, on D-206's pattern:
//!
//! - a **`sum_of_parts`** bundle carries no rows of its own (`inst-bb-rowless`),
//!   so `gate_input_keys` finds none and
//!   [`SellabilitySurface::plan_market_verdict`] answers `NotSellable` on the
//!   *binds-no-key* arm. Fail-closed, and for a reason that names no component;
//! - an **`own_price`** bundle is gated on its own rows alone and its components
//!   are not consulted at all. That is the **fail-open** direction, and it is what
//!   `inst-sg-bundle` exists to close.
//!
//! **How much of the fail-open the truth side already covers, measured rather than
//! assumed.** A component's *publish* is judged against the bundles referencing it
//! (`infra::bundle::referencing_markets`, reached from `infra::publish`), and a
//! component's *retirement* is refused while a bundle's current published revision
//! lists it (`inst-re-references`, `infra::retirement`). What neither covers is
//! **drift with the clock**: a component key whose window expires, or a component
//! plan whose `availableTo` passes, changes no truth row and trips no guard — and
//! those are predicates (1) and (3) exactly, which is what the gate is *for*. So
//! the residual is the D-80 trailing-void argument one level up: a bundle sold
//! into a component's uncovered interval.
//!
//! It is **latent** today for the reason [`PlanMarketVerdict`] records: predicates
//! (5) and (6) are `NotEvaluable` on every version this gear can project, so no
//! answer can reach `Sellable` and the fail-open case is bounded by
//! `NotEvaluable`. It goes live the day either of those lands.
//!
//! The rule itself is built and tested — [`crate::domain::bundle_sellability`] —
//! and has **no caller in this crate**, which is a checked fact rather than an
//! impression: `bundle_sellability_tests`'
//! `nothing_in_this_crate_reaches_the_bundle_conjunction` is the census, and it
//! reddens the day one appears.

use bss_pricing_sdk::CatalogVersion;
use toolkit_macros::domain_model;

use crate::domain::coverage::longest_cycle_sold_on;
use crate::domain::instant::format_rfc3339;
use crate::domain::lifecycle::LifecycleState;
use crate::domain::money::CurrencyCode;
use crate::domain::plan_shape::Frequency;
use crate::domain::scope_key::{PlanId, PriceEligibility, Region, ScopeKey};
use crate::domain::window::{CoverageEnd, KeyWindows, WindowInterval};
use time::OffsetDateTime;

/// What Slice 4 and Slice 10 owe predicate (5).
const OWED_TO_GA_GATE: &str = "Slice 4's per-market `not_sellable_ga` flag and Slice 10's \
                               prepaid-execution gate, published as the same flag mechanism";

/// What the registry gear owes predicate (6).
const OWED_TO_REGISTRY: &str = "D-46's registry `sellable` flag on the offered SKU, frozen per \
                                `CatalogVersion` by the products registry gear, which is not in \
                                this repository";

/// What a predicate answered on the *other* plane is owed, if a roster ever puts
/// it on this one.
///
/// Reached from no input while `PER_KEY` and `PLAN_LEVEL` partition `ALL`, which
/// `every_predicate_is_answered_exactly_once_across_the_surface` holds. It exists
/// so every builder here can spell every variant out: an arm list is what stops
/// a seventh predicate compiling until its `owed_to` is decided, where a wildcard
/// publishes it as owed to whichever gear that wildcard happens to name.
const OWED_TO_THE_KEY_PLANE: &str = "this predicate is answered per canonical scope key (`Predicate::PER_KEY`); a plan-level \
     answer for it means a roster placed it on the wrong plane";

/// The mirror of [`OWED_TO_THE_KEY_PLANE`], on the per-key builder.
const OWED_TO_THE_PLAN_PLANE: &str = "this predicate is answered once for the plan-market (`Predicate::PLAN_LEVEL`); a per-key \
     answer for it means a roster placed it on the wrong plane";

/// What a plan with no committed version owes every predicate but (2).
const OWED_TO_A_COMMITTED_VERSION: &str = "a committed, pin-eligible `CatalogVersion` carrying \
                                           this plan's subject (D-99) - until one exists the \
                                           facts these predicates read do not exist either";

/// One of the six sellability predicates (`inst-sg-surface`).
///
/// The **ordinal** is the design set's own name for each — §3 numbers them (1) to
/// (6) and every decision refers to them that way — and [`Predicate::as_str`] is
/// a rendering of the sentence §3 gives it. Neither is a wire code the set
/// declares, because the set declares no response body for this surface at all;
/// see [`SellabilitySurface`].
#[domain_model]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Predicate {
    /// (1) An active window covers `t`, **with** the D-80 coverage horizon.
    ActiveWindowWithHorizon,
    /// (2) The content answered from is a committed, pin-eligible
    /// `CatalogVersion` — a pending fan-out is not sellable.
    CommittedVersion,
    /// (3) `availableFrom` / `availableTo`.
    AvailabilityDates,
    /// (4) The plan lifecycle state — `retired` blocks (D-128).
    PlanLifecycleState,
    /// (5) The GA-gate flags: `not_sellable_ga`, and the prepaid-execution gate.
    GaGateFlags,
    /// (6) The registry `sellable` flag per offered SKU (D-46) — standalone lines
    /// only.
    RegistrySellable,
}

impl Predicate {
    /// All six, in §3's order.
    pub const ALL: &'static [Self] = &[
        Self::ActiveWindowWithHorizon,
        Self::CommittedVersion,
        Self::AvailabilityDates,
        Self::PlanLifecycleState,
        Self::GaGateFlags,
        Self::RegistrySellable,
    ];

    /// The predicates answered **per canonical scope key**, in §3's order.
    ///
    /// (1) reads the key's own window set; (5) is "evaluated per scope key /
    /// market, not per plan" in §3's own words. The rest read a plan-level or
    /// version-level fact — see the module doc for why answering those once is
    /// not a narrowing of `inst-sg-conjunction`.
    pub const PER_KEY: &'static [Self] = &[Self::ActiveWindowWithHorizon, Self::GaGateFlags];

    /// The predicates answered **once for the plan-market**, in §3's order.
    pub const PLAN_LEVEL: &'static [Self] = &[
        Self::CommittedVersion,
        Self::AvailabilityDates,
        Self::PlanLifecycleState,
        Self::RegistrySellable,
    ];

    /// §3's own number for this predicate, 1 to 6.
    #[must_use]
    pub const fn ordinal(self) -> u8 {
        match self {
            Self::ActiveWindowWithHorizon => 1,
            Self::CommittedVersion => 2,
            Self::AvailabilityDates => 3,
            Self::PlanLifecycleState => 4,
            Self::GaGateFlags => 5,
            Self::RegistrySellable => 6,
        }
    }

    /// The rendered name a reader of the surface sees.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ActiveWindowWithHorizon => "active_window_with_horizon",
            Self::CommittedVersion => "committed_version",
            Self::AvailabilityDates => "availability_dates",
            Self::PlanLifecycleState => "plan_lifecycle_state",
            Self::GaGateFlags => "ga_gate_flags",
            Self::RegistrySellable => "registry_sellable",
        }
    }
}

/// What one predicate answers, and the three answers are not two.
///
/// **`Failed` and `NotEvaluable` are different states and the whole of D-167
/// clause (3).** A consumer told a plan is not sellable because a predicate is
/// *false* knows what to fix; one told the same thing because the version cannot
/// *evaluate* that predicate knows the gate is not yet a gate. Collapsing them
/// would spend a gate's credibility on a promise the set knows is not yet true —
/// which is what that clause exists to stop the first consumer discovering.
#[domain_model]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PredicateAnswer {
    /// The predicate holds at the instant and market asked about.
    Satisfied,
    /// The predicate does **not** hold, and `detail` says why in the terms the
    /// operator can act on.
    Failed {
        /// One sentence naming the fact that refused, with its instant or state.
        detail: String,
    },
    /// This version cannot evaluate this predicate, and `owed_to` names what has
    /// to land before it can.
    ///
    /// `owed_to` is a **slice or a gear** for predicates (5) and (6), and for a
    /// plan with no committed version it is that version — the same sentence in
    /// both cases: the fact this predicate reads does not exist yet.
    NotEvaluable {
        /// What owes the fact.
        owed_to: &'static str,
    },
}

impl PredicateAnswer {
    /// The rendered token a reader of the surface sees.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Satisfied => "satisfied",
            Self::Failed { .. } => "failed",
            Self::NotEvaluable { .. } => "not_evaluable",
        }
    }
}

/// One predicate paired with its answer.
///
/// A list of these rather than one field per predicate, because the roster a
/// consumer reads has to be the roster [`Predicate::ALL`] declares: the builder
/// walks that constant, so a seventh predicate reaches every answer set by being
/// added to it once. What a list gives up — the compiler checking that each is
/// answered exactly once — is bought back by the test that asserts the roster.
#[domain_model]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PredicateOutcome {
    /// Which predicate.
    pub predicate: Predicate,
    /// Its answer at the instant and market asked about.
    pub answer: PredicateAnswer,
}

/// One canonical scope key's sellability: its frozen window facts and the
/// per-key predicates.
#[domain_model]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeySellability {
    /// The ten axes this answer is filed under.
    pub scope_key: ScopeKey,
    /// The key's frozen intervals, ordered, exactly as the version froze them.
    pub intervals: Vec<WindowInterval>,
    /// The coverage end derived from those intervals —
    /// [`KeyWindows::coverage_end`], forwarded rather than recomputed.
    pub coverage_end: CoverageEnd,
    /// One answer per member of [`Predicate::PER_KEY`], in that order.
    pub answers: Vec<PredicateOutcome>,
}

/// The D-94 conjunction over one plan-market.
///
/// # `Sellable` has no producer in this build, and that is stated rather than
/// removed
///
/// Predicates (5) and (6) are [`PredicateAnswer::NotEvaluable`] on every version
/// this gear can project, so the conjunction cannot reach `Sellable` today: with
/// nothing failed and something unevaluable the answer is
/// [`PlanMarketVerdict::NotEvaluable`]. The arm is declared anyway, for
/// `PROJECTED_ROW_STATES`' reason one plane over — the function is total over the
/// six answers, and the arm the finished gate reaches is not invented on the day
/// it becomes reachable. A test pins the unreachability so it is a fact of this
/// build rather than an impression.
#[domain_model]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PlanMarketVerdict {
    /// Every one of the six predicates is satisfied on every bound key.
    Sellable,
    /// At least one predicate **failed** on at least one bound key, or the plan
    /// binds no key on this market at all. D-94: never partially sellable.
    NotSellable,
    /// Nothing failed, and at least one predicate could not be evaluated. **A
    /// consumer MUST NOT read this as sellable** — it is the gate saying it is
    /// not yet a gate.
    NotEvaluable,
}

impl PlanMarketVerdict {
    /// The rendered token a reader of the surface sees.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Sellable => "sellable",
            Self::NotSellable => "not_sellable",
            Self::NotEvaluable => "not_evaluable",
        }
    }
}

/// Exactly the facts of a pinned plan-subject delta that the six predicates
/// read.
///
/// **A narrow set named for the predicate set it answers, rather than a
/// `PlanSubjectDelta::from_value`.** The inverse of
/// [`to_value`](crate::domain::projection::PlanSubjectDelta::to_value) would be
/// symmetric and large, and every field of it would be read by nothing — while
/// the exhaustive-destructure guard that makes the renderer safe cuts the other
/// way on a reader: a field added to the delta and *not* read here is the
/// ordinary case, so the guard would fire on every future field for no reason.
///
/// The one risk a narrow reader takes is that a payload key could be renamed on
/// one side only, and it is paid by a round-trip test beside the reader itself —
/// `read_model_repo::sellability_facts`, which builds these facts out of a frozen
/// payload. **The reader is in `infra` and not here**, and that is DE0301 rather
/// than a preference: reading a stored token means matching it against the
/// roster the store's `CorruptRow` path already depends on
/// (`price_repo::PRICE_ELIGIBILITIES` and its siblings), and those lists are
/// deliberately single — their own comment refuses a second copy "free to
/// disagree the day a variant lands in one of them". A domain-side parser would
/// have needed exactly that second copy.
#[domain_model]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SellabilityFacts {
    /// The plan's content as one committed, pin-eligible `CatalogVersion` froze
    /// it.
    Pinned(PinnedFacts),
    /// No committed, pin-eligible version carries this plan's subject.
    ///
    /// It is an **arm of the facts** and not a synthesised delta with no rows,
    /// which would read as a plan that publishes nothing on every market — a
    /// different and answerable statement. Here predicate (2) is `Failed` and
    /// every other predicate has no operand at all.
    NotAddressable {
        /// The plan asked about.
        plan_id: PlanId,
    },
}

/// The facts a committed, warm delta carries for the six predicates.
#[domain_model]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PinnedFacts {
    /// The plan the delta is the subject of.
    pub plan_id: PlanId,
    /// The version that froze these facts — predicate (2)'s operand.
    pub catalog_version: CatalogVersion,
    /// The current revision's lifecycle state — predicate (4)'s operand (D-128).
    pub lifecycle_state: LifecycleState,
    /// Start of the plan's availability — predicate (3).
    pub available_from: Option<OffsetDateTime>,
    /// End of the plan's availability — predicate (3).
    pub available_to: Option<OffsetDateTime>,
    /// The plan's recurring frequency — W6's margin, per D-123 a plan-level fact.
    pub frequency: Option<Frequency>,
    /// The canonical scope keys of the version's price rows: the gate's roster
    /// before eligibility resolution, and W6's "the plan's recurring rows".
    pub price_keys: Vec<ScopeKey>,
    /// The version's window facts, grouped per key.
    pub windows: Vec<KeyWindows>,
}

/// The sellability of one plan on one market at one instant.
///
/// # The design set declares no response body for this surface, and this is it
///
/// §5 gives the route and its query parameters —
/// `GET /plans/{planId}/sellability?at=&currency=&region=` — and `dod-sellability`
/// gives what it must expose: the six predicates point-in-time evaluable, the
/// per-key coverage end, and the D-94 conjunction. **No document of the set names
/// a field, a token or a shape**, so the vocabulary below is designed here and
/// reported as designed rather than presented as declared. What it deliberately
/// does not do is mint an *error code*: this surface refuses nothing, and every
/// unsellable answer is a `200` carrying the reason per predicate.
#[domain_model]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SellabilitySurface {
    /// The plan asked about.
    pub plan_id: PlanId,
    /// The instant every predicate was evaluated at — the caller's, never a
    /// clock this module reads.
    pub at: OffsetDateTime,
    /// The currency half of the bound market.
    pub currency: CurrencyCode,
    /// The region half of the bound market.
    pub region: Region,
    /// The version the answer was read from, when one carries the plan.
    pub catalog_version: Option<CatalogVersion>,
    /// One answer per member of [`Predicate::PLAN_LEVEL`], in that order.
    pub plan_answers: Vec<PredicateOutcome>,
    /// Every key the purchase binds on this market, eligibility-resolved.
    pub keys: Vec<KeySellability>,
}

impl SellabilitySurface {
    /// Evaluate the answerable predicates over one pinned delta, at `at`, on
    /// `(currency, region)`.
    ///
    /// **The parameters are the whole of the input, and that is the
    /// `inst-sg-segment-boundary` guard**: there is no payer, no group, no
    /// authenticated principal and no cache handle to pass. See the module doc.
    #[must_use]
    pub fn of_delta(
        facts: &SellabilityFacts,
        at: OffsetDateTime,
        currency: &CurrencyCode,
        region: &Region,
    ) -> Self {
        let pinned = match facts {
            SellabilityFacts::Pinned(pinned) => pinned,
            SellabilityFacts::NotAddressable { plan_id } => {
                return Self {
                    plan_id: *plan_id,
                    at,
                    currency: currency.clone(),
                    region: region.clone(),
                    catalog_version: None,
                    plan_answers: Predicate::PLAN_LEVEL
                        .iter()
                        .map(|predicate| PredicateOutcome {
                            predicate: *predicate,
                            answer: match predicate {
                                Predicate::CommittedVersion => PredicateAnswer::Failed {
                                    detail: "no committed, pin-eligible CatalogVersion carries \
                                             this plan's subject, so its content is not \
                                             addressable from any pin"
                                        .to_owned(),
                                },
                                // Spelled out for the reason the two builders
                                // below are. That a wildcard's answer would be
                                // right here is not the point: a seventh
                                // predicate has to be looked at once, and this
                                // is the site that would not have made anyone
                                // look at it.
                                Predicate::ActiveWindowWithHorizon
                                | Predicate::AvailabilityDates
                                | Predicate::PlanLifecycleState
                                | Predicate::GaGateFlags
                                | Predicate::RegistrySellable => PredicateAnswer::NotEvaluable {
                                    owed_to: OWED_TO_A_COMMITTED_VERSION,
                                },
                            },
                        })
                        .collect(),
                    keys: Vec::new(),
                };
            }
        };

        let margin =
            longest_cycle_sold_on(pinned.price_keys.iter(), pinned.frequency, currency, region);
        let keys = gate_input_keys(&pinned.price_keys, currency, region)
            .into_iter()
            .map(|scope_key| key_sellability(pinned, scope_key, at, margin))
            .collect();

        Self {
            plan_id: pinned.plan_id,
            at,
            currency: currency.clone(),
            region: region.clone(),
            catalog_version: Some(pinned.catalog_version),
            plan_answers: Predicate::PLAN_LEVEL
                .iter()
                .map(|predicate| PredicateOutcome {
                    predicate: *predicate,
                    answer: match predicate {
                        // The delta was read from a committed version whose
                        // subject is warm, which *is* the predicate: content
                        // still pending fan-out is not in any delta, so a
                        // consumer never sees it and the version it does see is
                        // addressable by construction (D-99 - the surface
                        // reports a window mutation only at the next pin-eligible
                        // version).
                        Predicate::CommittedVersion => PredicateAnswer::Satisfied,
                        Predicate::AvailabilityDates => availability(pinned, at),
                        Predicate::PlanLifecycleState => lifecycle(pinned),
                        Predicate::RegistrySellable => PredicateAnswer::NotEvaluable {
                            owed_to: OWED_TO_REGISTRY,
                        },
                        // Answered on the key plane, so never reached from this
                        // roster. Spelled out rather than folded into a wildcard:
                        // "not evaluable, and here is who owes it" is D-167
                        // clause (3)'s contract, and a wildcard hands a predicate
                        // nobody has decided about to whichever gear it names.
                        Predicate::ActiveWindowWithHorizon | Predicate::GaGateFlags => {
                            PredicateAnswer::NotEvaluable {
                                owed_to: OWED_TO_THE_KEY_PLANE,
                            }
                        }
                    },
                })
                .collect(),
            keys,
        }
    }

    /// The D-94 conjunction: one answer for the whole plan-market.
    ///
    /// **Never partially sellable.** One failed predicate on one bound key makes
    /// the plan-market not sellable, and a market the plan binds **no** key on is
    /// not sellable either — a purchase there would bind nothing, and a
    /// conjunction over an empty set answering `true` is the one direction a
    /// fail-closed gate must not round in.
    #[must_use]
    pub fn plan_market_verdict(&self) -> PlanMarketVerdict {
        let answers = || {
            self.plan_answers
                .iter()
                .chain(self.keys.iter().flat_map(|key| key.answers.iter()))
                .map(|outcome| &outcome.answer)
        };
        if self.keys.is_empty() {
            return PlanMarketVerdict::NotSellable;
        }
        if answers().any(|answer| matches!(answer, PredicateAnswer::Failed { .. })) {
            return PlanMarketVerdict::NotSellable;
        }
        if answers().any(|answer| matches!(answer, PredicateAnswer::NotEvaluable { .. })) {
            return PlanMarketVerdict::NotEvaluable;
        }
        PlanMarketVerdict::Sellable
    }
}

/// The keys a purchase on `(currency, region)` binds, eligibility-resolved
/// (`inst-sg-conjunction`, D-94).
///
/// Two resolutions, both of them the design set's:
///
/// - **grandfathered generations are never gate inputs.** An
///   `existing_grandfathered` row exists for subscribers a cutover retained, and
///   nobody new ever binds one — so a generation's coverage neither blocks nor
///   enables a sale.
/// - **`new_subscriptions_only` wins over `all_subscriptions` where both exist**,
///   which is `PriceEligibility`'s own most-specific-wins order (W3). Sibling
///   keys are the ones equal on every *other* axis — eight of them since D-196,
///   and [`ScopeKey::is_sibling_of`] is where they are enumerated, because that
///   is where the compiler can insist the list stays complete. **A six-axis copy
///   of the comparison here** would read two usage lines of one market as siblings
///   and drop the less specific one from the roster below, so the gate would
///   answer over a market whose second line it had never looked at a window for.
fn gate_input_keys(
    price_keys: &[ScopeKey],
    currency: &CurrencyCode,
    region: &Region,
) -> Vec<ScopeKey> {
    let candidates: Vec<&ScopeKey> = price_keys
        .iter()
        .filter(|key| key.currency() == currency && key.region() == region)
        .filter(|key| key.price_eligibility() != PriceEligibility::ExistingGrandfathered)
        .collect();

    let mut resolved: Vec<ScopeKey> = Vec::new();
    for key in candidates.iter().copied() {
        let most_specific = candidates
            .iter()
            .copied()
            .filter(|sibling| sibling.is_sibling_of(key))
            .map(ScopeKey::price_eligibility)
            .max()
            .unwrap_or_else(|| key.price_eligibility());
        if key.price_eligibility() == most_specific && !resolved.contains(key) {
            resolved.push(key.clone());
        }
    }
    resolved.sort_by_key(ScopeKey::to_string);
    resolved
}

/// One key's window facts and its per-key predicates.
fn key_sellability(
    pinned: &PinnedFacts,
    scope_key: ScopeKey,
    at: OffsetDateTime,
    margin: Option<time::Duration>,
) -> KeySellability {
    // A key the window plane does not mention gets an empty set, whose coverage
    // end is `Uncovered`. `PlanSubjectDelta::windows` declares that the two lists
    // enumerate the same keys, so this is a total function over a payload that
    // broke that promise rather than a case a correct projection produces - and
    // the answer it produces is the fail-closed one.
    let windows = pinned
        .windows
        .iter()
        .find(|group| group.scope_key == scope_key)
        .cloned()
        .unwrap_or_else(|| KeyWindows {
            scope_key: scope_key.clone(),
            intervals: Vec::new(),
        });
    let coverage_end = windows.coverage_end();
    KeySellability {
        scope_key,
        intervals: windows.intervals.clone(),
        coverage_end,
        answers: Predicate::PER_KEY
            .iter()
            .map(|predicate| PredicateOutcome {
                predicate: *predicate,
                answer: match predicate {
                    Predicate::ActiveWindowWithHorizon => {
                        active_window_with_horizon(&windows, at, margin)
                    }
                    Predicate::GaGateFlags => PredicateAnswer::NotEvaluable {
                        owed_to: OWED_TO_GA_GATE,
                    },
                    // Answered on the plan plane; see the sibling builder for why
                    // the arms are spelled out.
                    Predicate::CommittedVersion
                    | Predicate::AvailabilityDates
                    | Predicate::PlanLifecycleState
                    | Predicate::RegistrySellable => PredicateAnswer::NotEvaluable {
                        owed_to: OWED_TO_THE_PLAN_PLANE,
                    },
                },
            })
            .collect(),
    }
}

/// Predicate (1): an active window covers `at`, **and** the key's coverage
/// extends through the D-80 horizon.
///
/// # The two halves, and the token is not consulted for either
///
/// `KeyWindows::covers_at` derives the first half from `interval ∧ at`, never
/// from the state token — see the module doc for why a token read would make an
/// activation owe a re-projection. The second half is the D-80 margin applied to
/// an ordinary key: a finitely-covered key stops selling one full billing cycle
/// before its coverage ends, so nobody can buy into a trailing void.
///
/// # `None` margin refuses, and refusal is not zero
///
/// W6's term has **no value** when the market sells a recurring row and the plan
/// authored no frequency, and that is a different answer from `Some(zero)` — W6's
/// "a plan with no recurring part needs no forward coverage". Folding the first
/// into the second is the direction a fail-closed gate must never round in, and
/// it is the same collapse [`CoverageEnd`] refuses one type over.
/// `infra::window::refuse_trailing_void` refuses on the same arm for the same
/// reason, which is the precedent rather than a coincidence.
///
/// It is `Failed` and not [`PredicateAnswer::NotEvaluable`]: the missing fact is
/// **authored data on a plan this version froze**, not a slice that has not
/// landed, and a consumer told "not evaluable" may conclude the gate is not yet a
/// gate and proceed. A plan selling recurring with no cycle is a plan that cannot
/// be shown safe to sell, which is a false predicate.
fn active_window_with_horizon(
    windows: &KeyWindows,
    at: OffsetDateTime,
    margin: Option<time::Duration>,
) -> PredicateAnswer {
    if !windows.covers_at(at) {
        return PredicateAnswer::Failed {
            detail: format!(
                "no window of this key covers {}: coverage {}",
                format_rfc3339(at),
                describe(windows.coverage_end())
            ),
        };
    }
    let Some(margin) = margin else {
        return PredicateAnswer::Failed {
            detail: "the plan sells a recurring row on this market and authors no frequency, so \
                     the D-80 horizon - now plus the longest billing cycle sold on the key - has \
                     no value, and this key cannot be shown safe to sell"
                .to_owned(),
        };
    };
    // A caller-supplied `at` near the end of the representable range would panic
    // `Add`, so the horizon is computed fallibly on a read surface whose instant
    // is request input - unlike the mutation path, whose `now` is this server's
    // clock. An unrepresentable horizon is refused rather than skipped: the
    // predicate has no operand and the fail-closed answer is the false one.
    let Some(horizon) = at.checked_add(margin) else {
        return PredicateAnswer::Failed {
            detail: format!(
                "{} plus the longest billing cycle sold on this key is not a representable \
                 instant, so the D-80 horizon cannot be evaluated",
                format_rfc3339(at)
            ),
        };
    };
    match windows.coverage_end() {
        CoverageEnd::OpenEnded => PredicateAnswer::Satisfied,
        CoverageEnd::Ends(end) if end >= horizon => PredicateAnswer::Satisfied,
        CoverageEnd::Ends(end) => PredicateAnswer::Failed {
            detail: format!(
                "this key's coverage ends at {} and the D-80 horizon runs to {}: a purchase at {} \
                 would bind a line whose coverage stops inside its first billing cycle",
                format_rfc3339(end),
                format_rfc3339(horizon),
                format_rfc3339(at)
            ),
        },
        // Unreachable behind `covers_at`, and answered rather than asserted: a
        // key with no contributing interval covers no instant, so the first half
        // has already refused. A panic here would be a panic on a read path.
        CoverageEnd::Uncovered => PredicateAnswer::Failed {
            detail: "this key has no window contributing coverage at all".to_owned(),
        },
    }
}

/// Predicate (3): `availableFrom <= at < availableTo`.
///
/// **Half-open, for the window plane's reason.** The design set gives the two
/// dates and never says whether the end is inclusive; reading `[from, to)` keeps
/// one convention in the gear for every interval a purchase is judged against,
/// and puts the boundary instant outside rather than inside, which is the
/// fail-closed direction. Reported as a reading taken here, since no document
/// settles it.
fn availability(pinned: &PinnedFacts, at: OffsetDateTime) -> PredicateAnswer {
    if let Some(from) = pinned.available_from
        && at < from
    {
        return PredicateAnswer::Failed {
            detail: format!(
                "the plan is purchasable from {} and {} is before it",
                format_rfc3339(from),
                format_rfc3339(at)
            ),
        };
    }
    if let Some(to) = pinned.available_to
        && at >= to
    {
        return PredicateAnswer::Failed {
            detail: format!(
                "the plan is purchasable until {} (exclusive) and {} is not before it",
                format_rfc3339(to),
                format_rfc3339(at)
            ),
        };
    }
    PredicateAnswer::Satisfied
}

/// Predicate (4): the plan's lifecycle state — `retired` blocks (D-128).
///
/// Every state but `published` refuses. `retired` is the one §3 names and the one
/// D-128 made a publish unit so that the pin could learn of it; the other three
/// are not reachable in a projected delta — D-121 draws its revision from the
/// plan's **current** one, which is `published` or `retired` — and are refused
/// rather than defaulted, because a state this reader does not recognise as
/// sellable is one it must not treat as sellable.
fn lifecycle(pinned: &PinnedFacts) -> PredicateAnswer {
    if pinned.lifecycle_state == LifecycleState::Published {
        return PredicateAnswer::Satisfied;
    }
    PredicateAnswer::Failed {
        detail: format!(
            "the plan's current revision is {} and only a published revision is sellable",
            pinned.lifecycle_state.as_str()
        ),
    }
}

/// A coverage end in one clause, for a `detail` sentence.
fn describe(end: CoverageEnd) -> String {
    match end {
        CoverageEnd::Uncovered => "does not exist on this key at any instant".to_owned(),
        CoverageEnd::Ends(at) => format!("ends at {}", format_rfc3339(at)),
        CoverageEnd::OpenEnded => "is open-ended".to_owned(),
    }
}

#[cfg(test)]
#[path = "sellability_tests.rs"]
mod sellability_tests;
