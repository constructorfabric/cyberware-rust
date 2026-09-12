//! The **phase-schedule rules** (`cpt-cf-bss-pricing-algo-phases`).
//!
//! Ten rules over a [`PlanShape`], each one an instruction of the slice:
//! `inst-ph-graph` (in three halves), `inst-ph-duration`, `inst-ph-trial`,
//! `inst-ph-row-attached`, `inst-ph-coverage`, `inst-ph-usage-invariant`,
//! `inst-ph-override-units` and `inst-ph-terminal-stable`. They append and never
//! short-circuit, so a plan with a broken chain *and* an uncovered phase reports
//! both.
//!
//! Three of the ten implement `inst-ph-graph`, which §5 splits into three
//! problem responses, so their
//! [`name`](crate::domain::validation::ValidationRule::name)s carry a suffix -
//! `inst-ph-graph`, `inst-ph-graph/linear`, `inst-ph-graph/terminal-kind`. The
//! name is what the pipeline attributes a failing rule by, and three rules
//! answering to one name would make the attribution useless exactly where the
//! codes are already three.
//!
//! ## Terminality is structural, and it is derived in exactly one place
//!
//! Every rule here asks [`PhaseGraph`] which phase is terminal, which is the
//! entry, and what the ordinal order is. None of them re-derives any of the
//! three. That is the point of the accessors: four rule sets each free to answer
//! "which phase is terminal" its own way would eventually disagree, and a
//! disagreement between two derivations of one fact is invisible — both readings
//! look right at their own call site.
//!
//! ## The chain is linear, and that is stronger than acyclic
//!
//! `inst-ph-graph`'s review fix. Acyclicity plus a single terminal
//! admit a **dead phase**: one nothing converts into, which still demands
//! coverage rows under `inst-ph-coverage`, and which leaves "first" undefined
//! for the two readers that need it — `inst-cs-setup-timing` charges setup at
//! entry into "the first non-trial phase", and D-39 computes migration entry the
//! same way. So the ordinal order is **normative**, the entry phase is the
//! **lowest** ordinal, and every non-terminal phase converts into the next phase
//! in that order.
//!
//! A duplicate ordinal is reported by the same rule, and for the same reason: it
//! is not a total order, so "the lowest ordinal" names two phases and the entry
//! is undefined again — through the tie rather than through the topology.
//!
//! ## The terminal phase's kind
//!
//! C-4. Terminality is `converts_to_phase_id IS NULL` while `kind` is an
//! independent column admitting `trial | interim | evergreen`, so without this rule
//! nothing rejects a `trial`-terminal chain — which leaves "the first non-trial
//! phase" undefined for *both* readers above and collides with the
//! `display_trial_days = phase_duration_days` CHECK, since a terminal phase may
//! carry no duration at all. **Interim pricing forever is an `evergreen` terminal
//! phase priced at the interim rate, never an `interim` terminal**; that sentence is
//! the authoring answer this rule owes, so it is also in the rejection detail.
//!
//! ## Usage rows are phase-invariant, and an override inherits the counter
//!
//! One usage row on the terminal phase covers every phase (D-15/D-19); a usage
//! row on a non-terminal phase overrides it **for that phase**. Two rules guard
//! the override, and each closes a different hole:
//!
//! - **Its base must exist** (`PhaseOverrideBase`, D-117). Without the base the
//!   D-89 unit guard has no comparison target at all; D-84's per-market
//!   completeness exempts the line entirely, because phase-scoped overrides are
//!   additive; and after the phase converts the line resolves to **nothing**, so
//!   usage that keeps arriving fails closed on a published, sellable plan. That
//!   is "sold but unrateable" through the override door.
//! - **Its denomination must match** (`PhaseOverrideUnits`, D-89 extended by
//!   D-122). The tier counter `Q` is keyed
//!   `(subscription, meter, dimensionKey, window)` and is **phase-blind**, so a
//!   conversion never resets it: a `per_hour` trial row converting into a
//!   `per_day` evergreen row mid-window applies an hours-denominated counter to
//!   day-denominated bands — the D-77/D-82 factor-of-24 class through the phase
//!   axis — and a changed `package_size` re-buckets an already-accumulated
//!   `used`. Free-trial pricing stays fully expressible: a zero rate, or a zero
//!   band set, **at the same denomination**.
//!
//! The seven axes the second rule compares are
//! [`unit_determining_mismatch`], shared with the supersession guard rather than
//! restated. Both rules are one question asked of two mechanisms — "does the
//! continued counter still mean what it meant?" — and the D-127 lesson is that
//! such a guard must not be able to differ by which mechanism asked.
//!
//! ## What is deliberately absent
//!
//! The precedent is [`crate::domain::rules`]'s own absence section: *a rule that
//! always passes is indistinguishable from a rule that holds.*
//!
//! - **`inst-ph-map` and `inst-ph-default` are not rules.** The phase-to-price
//!   map and the auto-created implicit terminal phase are a projection and a
//!   creation-time act; there is nothing for a validator to reject. The rows'
//!   `phase` axis is typed [`PhaseId`] by construction (D-19), so the axis-typing
//!   half is carried by the type rather than by a check — **and the type carries
//!   only that much.** `PhaseId` says "this is a phase id" and not "this
//!   revision's", which is the gap [`RowPhaseAttached`] closes (D-337): a draft
//!   whose every row keyed on an id nobody attached was refused for having no
//!   terminal phase and never told about the rows.
//! - **`inst-ph-trial` is a rule, not a `CHECK`.** Reading `displayTrialDays` as a
//!   read-model projection whose equality with `phase_duration_days` the `CHECK` on
//!   `pricing_plan_phase` guarantees does not hold, and D-151 measured why: the
//!   `CHECK` is silent on `kind` and, by NULL propagation, satisfied whenever
//!   `phase_duration_days` is NULL, so the shape it exists to forbid passes it.
//!   [`DisplayTrialDaysOnTrialPhase`] closes both halves.
//! - **Reachability is not walked.** `PhaseChainLinear` requires every
//!   non-terminal phase to convert into its ordinal successor, which makes the
//!   whole set reachable from the entry phase by construction — a separate walk
//!   could only ever agree, and a check that cannot fail is not a check. The
//!   unreachable phase the design names is reported through the edge that
//!   strands it.
//! - **The per-market half of the override base is D-84's.**
//!   `PhaseOverrideBase` asks whether the `(meter, dimensionKey)` line has a
//!   phase-invariant home at all — the question `inst-ph-usage-invariant` asks,
//!   which is why the design has it name the line and the phase and not a
//!   market. Whether that home exists in **every** sold market is
//!   `USAGE_MARKET_INCOMPLETE`, whose scope is exactly the terminal-phase rows.
//!   Two owners of one requirement are free to disagree.

use std::collections::BTreeSet;

use toolkit_macros::domain_model;

use crate::domain::plan_rules::{
    DISPLAY_TRIAL_DAYS_INVALID, PHASE_CHAIN_NONLINEAR, PHASE_DURATION_INVALID, PHASE_GRAPH_INVALID,
    PHASE_IN_USE, PHASE_OVERRIDE_ORPHANED, PHASE_OVERRIDE_UNIT_MISMATCH, PHASE_ROW_ORPHANED,
    PHASE_UNCOVERED, TERMINAL_PHASE_CHANGED, TERMINAL_PHASE_KIND_INVALID,
};
use crate::domain::plan_shape::{BillingCycle, PhaseGraph, PhaseKind, PlanShape};
use crate::domain::price_record::PriceRecord;
use crate::domain::price_row::unit_determining_mismatch;
use crate::domain::scope_key::{ChargeKind, PhaseId};
use crate::domain::validation::{Stage, ValidationReport, ValidationRule};

// ---------------------------------------------------------------------------
// inst-ph-graph, first half: the edges
// ---------------------------------------------------------------------------

/// `inst-ph-graph` (edges) — no dangling target, no cycle, **exactly one**
/// terminal phase.
///
/// Zero terminals and two terminals are both violations, and the partial
/// `UNIQUE` on `pricing_plan_phase` can only hold the at-most-one half — the
/// pipeline runs *before* the write, and no index can require a row to exist.
///
/// **Judged at the `phases` write as well as at publish.**
/// The at-most-one half was reaching the store: a payload carrying two terminal
/// phases tripped `uq_pricing_plan_phase_terminal` and answered `500 … please
/// retry later` for a fault no retry can fix, which is the class D-340 filed as
/// `[H]` one wave earlier and fixed for the *other* unique constraint on the same
/// statement. The remedy is this rule at the door rather than a second typed code
/// at the store: one requirement, one owner, and the constraint left standing as a
/// backstop no caller can reach.
///
/// All three of its faults take that door, not the terminal count alone. The
/// `phases` facet is a **wholesale replace**, so nothing a later call adds
/// completes a dangling edge or breaks a cycle — the next call replaces the whole
/// set — which is D-312's test for a write-stage fault: every operand is in the
/// request and the resolving call retracts what was just sent.
#[domain_model]
#[derive(Clone, Copy, Debug, Default)]
pub struct PhaseGraphIntegrity {
    /// Which stage this instance stamps its findings with, on
    /// [`RowPhaseAttached::stage`]'s reasoning exactly: the rule emits one fault
    /// set, and whether the surface asking can judge it is a property of **the
    /// surface**. `Default` is [`Stage::Publish`], which is what the aggregate
    /// pipeline registers.
    pub stage: Stage,
}

impl ValidationRule<PlanShape> for PhaseGraphIntegrity {
    fn name(&self) -> &'static str {
        "inst-ph-graph"
    }

    fn evaluate(&self, subject: &PlanShape, report: &mut ValidationReport) {
        let graph = &subject.phases;

        for phase in graph.phases() {
            if let Some(target) = phase.converts_to_phase_id
                && graph.find(target).is_none()
            {
                report.violate_at(
                    self.stage,
                    PHASE_GRAPH_INVALID,
                    phase_subject(subject, phase.phase_id),
                    format!(
                        "convertsToPhaseId names {target}, which is not a phase of this \
                         revision; a subscription reaching the end of this phase would convert \
                         into nothing"
                    ),
                );
            }
        }

        let on_cycle = phases_on_a_cycle(graph);
        if !on_cycle.is_empty() {
            report.violate_at(
                self.stage,
                PHASE_GRAPH_INVALID,
                subject.subject(),
                format!(
                    "the convertsToPhaseId edges cycle through {}; a chain that returns to \
                     itself has no terminal phase and no end",
                    join_ids(&on_cycle)
                ),
            );
        }

        let terminals = graph.terminals();
        if terminals.len() == 1 {
            return;
        }
        let detail = if terminals.is_empty() {
            "no phase of this revision is terminal (convertsToPhaseId IS NULL); every chain \
             must end, and the terminal phase is what a non-phased row's scope-key default \
             names"
                .to_owned()
        } else {
            let named: BTreeSet<PhaseId> = terminals.iter().map(|phase| phase.phase_id).collect();
            format!(
                "{} phases of this revision are terminal ({}); exactly one is allowed, because \
                 usage phase-invariance and the scope-key phase default are both defined \
                 relative to which phase is terminal",
                terminals.len(),
                join_ids(&named)
            )
        };
        report.violate_at(self.stage, PHASE_GRAPH_INVALID, subject.subject(), detail);
    }
}

// ---------------------------------------------------------------------------
// inst-ph-graph, second half: the chain is linear
// ---------------------------------------------------------------------------

/// `inst-ph-graph` (linearity, review fix) — the chain is the
/// **ordinal order**.
///
/// Every non-terminal phase converts into the next phase in ordinal order, and
/// no two phases share an ordinal. A skip, a branch, a terminal phase in the
/// middle of the order, a non-terminal phase at its end, and a tie are all one
/// fault reported one way: the order the design set calls normative is not the
/// order the edges walk, so "first" is undefined.
#[domain_model]
#[derive(Clone, Copy, Debug, Default)]
pub struct PhaseChainLinear;

impl ValidationRule<PlanShape> for PhaseChainLinear {
    fn name(&self) -> &'static str {
        "inst-ph-graph/linear"
    }

    fn evaluate(&self, subject: &PlanShape, report: &mut ValidationReport) {
        let ordered = subject.phases.in_ordinal_order();

        for pair in ordered.windows(2) {
            let (left, right) = (pair[0], pair[1]);
            if left.ordinal == right.ordinal {
                report.violate(
                    PHASE_CHAIN_NONLINEAR,
                    phase_subject(subject, right.phase_id),
                    format!(
                        "ordinal {} is shared with phase {}; the entry phase is the lowest \
                         ordinal and the chain is the ordinal order, so a tie leaves both \
                         undefined",
                        right.ordinal, left.phase_id
                    ),
                );
            }
        }

        for (index, phase) in ordered.iter().enumerate() {
            let successor = ordered.get(index + 1).map(|next| next.phase_id);
            if phase.converts_to_phase_id == successor {
                continue;
            }
            report.violate(
                PHASE_CHAIN_NONLINEAR,
                phase_subject(subject, phase.phase_id),
                format!(
                    "this phase converts to {} and the next phase in ordinal order is {}; the \
                     chain is linear, so a skip, a branch or a phase left unreachable from the \
                     entry phase fails publish",
                    describe(phase.converts_to_phase_id),
                    describe(successor)
                ),
            );
        }
    }
}

// ---------------------------------------------------------------------------
// inst-ph-graph, C-4: the terminal phase's kind
// ---------------------------------------------------------------------------

/// C-4 — the terminal phase's `kind` MUST be `evergreen`.
///
/// See the module doc for why a `trial` terminal is not merely unusual but
/// undefined. Every terminal phase is judged, not only the first: with two
/// terminals the graph is already invalid, and picking one of them to judge
/// would be a derivation of "the terminal phase" this module owes to
/// [`PhaseGraph`].
#[domain_model]
#[derive(Clone, Copy, Debug, Default)]
pub struct TerminalPhaseKind;

impl ValidationRule<PlanShape> for TerminalPhaseKind {
    fn name(&self) -> &'static str {
        "inst-ph-graph/terminal-kind"
    }

    fn evaluate(&self, subject: &PlanShape, report: &mut ValidationReport) {
        for terminal in subject.phases.terminals() {
            if matches!(terminal.kind, PhaseKind::Evergreen) {
                continue;
            }
            report.violate(
                TERMINAL_PHASE_KIND_INVALID,
                phase_subject(subject, terminal.phase_id),
                format!(
                    "the terminal phase has kind {}, and only evergreen is legal there: a \
                     non-evergreen terminal leaves the first non-trial phase undefined for \
                     setup timing and for migration entry. Interim pricing forever is an \
                     evergreen terminal phase priced at the interim rate, never an interim \
                     terminal",
                    terminal.kind
                ),
            );
        }
    }
}

// ---------------------------------------------------------------------------
// inst-ph-duration
// ---------------------------------------------------------------------------

/// `inst-ph-duration` — where a phase converts, and **when**.
///
/// `convertsToPhaseId` answers the first; `phaseDurationDays` answers the
/// second, and Subscriptions enforces phase runtime from it as the single
/// source. So every non-terminal phase carries one and it is positive, and the
/// terminal phase carries none — nothing follows it, so a duration on it is a
/// conversion instant for a conversion that never happens.
#[domain_model]
#[derive(Clone, Copy, Debug, Default)]
pub struct PhaseDuration;

impl ValidationRule<PlanShape> for PhaseDuration {
    fn name(&self) -> &'static str {
        "inst-ph-duration"
    }

    fn evaluate(&self, subject: &PlanShape, report: &mut ValidationReport) {
        for phase in subject.phases.in_ordinal_order() {
            let detail = match (phase.is_terminal(), phase.phase_duration_days) {
                (false, None) => "a non-terminal phase must author phaseDurationDays: \
                     convertsToPhaseId says where this phase converts and the duration says \
                     when, and Subscriptions has no other source for the instant"
                    .to_owned(),
                (false, Some(0)) => "phaseDurationDays must be greater than zero; a phase that \
                     lasts no days converts at the instant it is entered"
                    .to_owned(),
                (true, Some(days)) => format!(
                    "the terminal phase must not author phaseDurationDays, and this one \
                     authors {days}: nothing follows it, so the duration dates a conversion \
                     that never happens"
                ),
                (false, Some(_)) | (true, None) => continue,
            };
            report.violate(
                PHASE_DURATION_INVALID,
                phase_subject(subject, phase.phase_id),
                detail,
            );
        }
    }
}

// ---------------------------------------------------------------------------
// inst-ph-row-attached
// ---------------------------------------------------------------------------

/// `inst-ph-row-attached` (D-337) — every row of the revision names a phase the
/// revision attaches.
///
/// The exact inverse of [`PhaseCoverage`], which asks whether a phase has rows,
/// and registered immediately before it so the pair reads together in one
/// report. Both are needed, and each is invisible to the other's iteration: a
/// phase with no rows leaves a conversion resolving nothing, and a row with no
/// phase resolves against a phase no subscription can ever be in.
///
/// **Unconditional, where `PhaseCoverage` is scoped to plans whose cycle carries
/// a recurring part.** Coverage cannot be demanded of a plan that has no
/// recurring row to give; attachment is a statement about a row that exists, so
/// there is no cycle under which it does not apply.
///
/// **Not folded into [`TerminalPhaseStable`] arm (b).** That arm asks the same
/// question of the *baseline*'s phases and begins with
/// `let Some(baseline) = … else { return }`, which is precisely why a draft could
/// carry this fault unreported: a plan that has never published has nothing to be
/// stable against, and the fault is not about stability.
///
/// **Publish-stage by default, and judged at the `phases` **write** as well
/// (D-342).** Adding a price row and attaching its phase in either order is a
/// state §4.2 allows a draft to be in, so the *price-row* write cannot judge this —
/// the phase set legitimately arrives in the next call. The `phases` write can: the
/// submitted set is the whole request and the rows are already stored, so a replace
/// that drops a phase they name is complete and knowably unpublishable the instant
/// it arrives. Which is what [`RowPhaseAttached::stage`] expresses.
///
/// D-337 registered the rule at publish only, which is the write path's default,
/// and publish is the one moment at which the author's options are strictly fewer:
/// at the write there are two remedies — keep the phase, or drop the rows
/// deliberately — while at publish a published row cannot be re-pointed at all
/// (Foundation §3.7) and a draft row can only be deleted.
///
/// **Both stages, not one.** The publish-stage registration stays, because the
/// phase set and the price rows are authored through *different* facets: a rule
/// that ran only at the `phases` write would be defeated by a price row authored
/// after the phase set was.
#[domain_model]
#[derive(Clone, Copy, Debug, Default)]
pub struct RowPhaseAttached {
    /// Which stage this instance stamps its findings with (**D-342**).
    ///
    /// **Not the marker [`Stage`]'s own doc warns against.** That caution is that a
    /// rule cannot carry *the* stage of its violations, because most of the rules
    /// emitting a write-stage violation also emit publish-stage ones and a
    /// rule-level marker would over-refuse. This rule emits one fault, and whether
    /// the surface asking can judge it is a property of **the surface**: at the
    /// `phases` write every operand is in hand, at the price-row write the phase set
    /// may still be coming. So the stage belongs to the door, and this is how the
    /// door says which one it is.
    ///
    /// `Default` is [`Stage::Publish`], which is what the aggregate pipeline
    /// registers — a rule is publish-stage unless a caller states otherwise, and
    /// that direction is the safe one.
    pub stage: Stage,
}

impl ValidationRule<PlanShape> for RowPhaseAttached {
    fn name(&self) -> &'static str {
        "inst-ph-row-attached"
    }

    fn evaluate(&self, subject: &PlanShape, report: &mut ValidationReport) {
        for record in &subject.rows {
            let phase = record.scope_key.phase();
            if subject.phases.find(phase).is_some() {
                continue;
            }
            // Subject is the phase and not the row: one act — attaching this id —
            // clears every finding under it, so a consumer grouping by subject
            // shows the author one entry per remediation rather than one per row.
            // The row is named in the detail, which is the other remediation.
            let subject = phase_subject(subject, phase);
            let detail = format!(
                "price row {} keys on phase {phase}, which this revision does not attach; \
                 the row resolves against a phase no subscription can be in, and a scope \
                 key is the row's identity, so the phase has to be attached. Deleting the \
                 row instead is available only while that row is a draft: the candidate set \
                 this rule judges includes published rows, and the published plane is \
                 append-only",
                record.price_id
            );
            // One fault, one detail, one of two stamps. Through the shared dispatch
            // rather than a `report.violate*` pair with their own arguments, so the
            // sentence an author reads cannot come to depend on which door refused
            // them — and so the second rule to take its stage from a parameter
            // (`PhaseGraphIntegrity`, with three faults) does not copy the match.
            report.violate_at(self.stage, PHASE_ROW_ORPHANED, subject, detail);
        }
    }
}

// ---------------------------------------------------------------------------
// inst-ph-coverage
// ---------------------------------------------------------------------------

/// `inst-ph-coverage` (D-15) — every phase is covered by a recurring row in
/// every sold market.
///
/// **Scoped to plans whose cycle carries a recurring part.** One-time and
/// usage-only plans have no recurring
/// row that could ever cover a phase, so a literal reading would fail them
/// through their implicit terminal phase - a rule they can never satisfy. The
/// scope is read from [`BillingCycle::has_recurring_part`] rather than
/// re-spelled here, so widening it is one visible edit.
///
/// The rule exists because a phase conversion must never resolve to nothing, and
/// the row-based Slice-7 coverage check cannot see a phase that has **no rows at
/// all** - it has nothing to iterate.
#[domain_model]
#[derive(Clone, Copy, Debug, Default)]
pub struct PhaseCoverage;

impl ValidationRule<PlanShape> for PhaseCoverage {
    fn name(&self) -> &'static str {
        "inst-ph-coverage"
    }

    fn evaluate(&self, subject: &PlanShape, report: &mut ValidationReport) {
        if !subject
            .billing_cycle
            .is_some_and(BillingCycle::has_recurring_part)
        {
            return;
        }
        let markets = subject.markets();
        for phase in subject.phases.in_ordinal_order() {
            for (currency, region) in &markets {
                let covered = subject.rows.iter().any(|record| {
                    record.scope_key.phase() == phase.phase_id
                        && record.scope_key.charge_kind() == ChargeKind::Recurring
                        && record.scope_key.currency() == currency
                        && record.scope_key.region() == region
                });
                if covered {
                    continue;
                }
                report.violate(
                    PHASE_UNCOVERED,
                    format!(
                        "{}/{currency}/{region}",
                        phase_subject(subject, phase.phase_id)
                    ),
                    format!(
                        "no recurring price row covers this phase in {currency}/{region}; a \
                         conversion into it would resolve to nothing on a plan the tenant sells \
                         in that market"
                    ),
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// inst-ph-usage-invariant
// ---------------------------------------------------------------------------

/// `inst-ph-usage-invariant` (D-117) — a phase-scoped usage override needs its
/// phase-invariant base.
///
/// The base is the terminal-phase row of the override's own
/// `(meter, dimensionKey)` line. See the module doc for the three-part defect an
/// orphan opens, and for why the per-market half belongs to `USAGE_MARKET_INCOMPLETE`.
///
/// Says nothing when the graph does not have exactly one terminal phase: "the
/// phase-invariant row" is undefined there, `PHASE_GRAPH_INVALID` is the fault
/// the author fixes first, and reporting the consequences of a fault as further
/// faults is how a report stops being remediable in one pass.
#[domain_model]
#[derive(Clone, Copy, Debug, Default)]
pub struct PhaseOverrideBase;

impl ValidationRule<PlanShape> for PhaseOverrideBase {
    fn name(&self) -> &'static str {
        "inst-ph-usage-invariant"
    }

    fn evaluate(&self, subject: &PlanShape, report: &mut ValidationReport) {
        let Some(terminal) = subject.phases.sole_terminal() else {
            return;
        };
        let usage_rows = subject.usage_rows();
        for over in usage_rows
            .iter()
            .filter(|record| record.scope_key.phase() != terminal)
        {
            let based = usage_rows.iter().any(|candidate| {
                candidate.scope_key.phase() == terminal && line_of(candidate) == line_of(over)
            });
            if based {
                continue;
            }
            report.violate(
                PHASE_OVERRIDE_ORPHANED,
                override_subject(subject, over),
                format!(
                    "this usage row is a phase-scoped override of a line that has no \
                     phase-invariant row on the terminal phase {terminal}; after the phase \
                     converts the line resolves to nothing, and usage that keeps arriving \
                     fails closed on a published, sellable plan"
                ),
            );
        }
    }
}

// ---------------------------------------------------------------------------
// inst-ph-override-units
// ---------------------------------------------------------------------------

/// `inst-ph-override-units` (D-89, extended by D-122) — the override keeps the
/// base row's denomination.
///
/// The referent is the terminal-phase row of the same line **in the override's
/// own market**: the two scope keys differ in the `phase` axis alone, so that is
/// precisely the row a subscriber in that market converts into. When the line
/// has no terminal-phase row in that market this rule says nothing rather than
/// borrowing another market's row as a referent - the missing base is
/// `PHASE_OVERRIDE_ORPHANED`'s or `USAGE_MARKET_INCOMPLETE`'s to report, and a
/// comparison against a row nobody converts into would name fields no
/// subscriber's counter ever crosses.
///
/// The seven compared axes are [`unit_determining_mismatch`], shared with the
/// supersession guard; see the module doc.
#[domain_model]
#[derive(Clone, Copy, Debug, Default)]
pub struct PhaseOverrideUnits;

impl ValidationRule<PlanShape> for PhaseOverrideUnits {
    fn name(&self) -> &'static str {
        "inst-ph-override-units"
    }

    fn evaluate(&self, subject: &PlanShape, report: &mut ValidationReport) {
        let Some(terminal) = subject.phases.sole_terminal() else {
            return;
        };
        let usage_rows = subject.usage_rows();
        for over in usage_rows
            .iter()
            .filter(|record| record.scope_key.phase() != terminal)
        {
            let base = usage_rows.iter().find(|candidate| {
                candidate.scope_key.phase() == terminal
                    && line_of(candidate) == line_of(over)
                    && candidate.scope_key.currency() == over.scope_key.currency()
                    && candidate.scope_key.region() == over.scope_key.region()
            });
            let Some(base) = base else {
                continue;
            };
            // The override runs first and the terminal-phase row runs after the
            // conversion, so that is the order the chronological argument names
            // read in.
            let changed = unit_determining_mismatch(&over.row, &base.row);
            if changed.is_empty() {
                continue;
            }
            report.violate(
                PHASE_OVERRIDE_UNIT_MISMATCH,
                override_subject(subject, over),
                format!(
                    "this phase-scoped override changes {} against the terminal-phase row of \
                     its line; the tier counter is phase-blind, so a conversion never resets \
                     it and the row serving the meter afterwards inherits the continued \
                     counter. Free-trial pricing is a zero rate or a zero band set at the same \
                     denomination",
                    changed.join(", ")
                ),
            );
        }
    }
}

// ---------------------------------------------------------------------------
// inst-ph-terminal-stable
// ---------------------------------------------------------------------------

/// `inst-ph-terminal-stable` (D-64) — the terminal phase is immutable, and a
/// phase in use is re-attached.
///
/// Both arms are comparisons against the plan's current published revision, so
/// both say nothing when [`PlanShape::baseline`] is `None`: a first publish has
/// no previous state to be stable against, and a rule with no referent either
/// passes vacuously or blocks every publish.
///
/// The defect behind (a) is the one `inst-ph-coverage` cannot see. A usage-only
/// plan published non-phased carries its metered row on the implicit terminal
/// `T0`; a revision adding a trial plus a new evergreen phase makes `T0`
/// non-terminal, the metered row is no longer phase-invariant, a subscription in
/// the new phase resolves **no** usage row, and Tariffs fails closed on a
/// published sellable plan. Coverage cannot catch it because it is scoped to
/// recurring rows on `recurring`/`hybrid` plans -
/// usage-only plans are entirely unguarded there.
///
/// Arm (a) needs a single terminal phase to compare and stays quiet without one;
/// arm (b) needs only the phase set, so it runs whatever the chain looks like.
#[domain_model]
#[derive(Clone, Copy, Debug, Default)]
pub struct TerminalPhaseStable;

impl ValidationRule<PlanShape> for TerminalPhaseStable {
    fn name(&self) -> &'static str {
        "inst-ph-terminal-stable"
    }

    fn evaluate(&self, subject: &PlanShape, report: &mut ValidationReport) {
        let Some(baseline) = subject.baseline.as_ref() else {
            return;
        };

        if let Some(terminal) = subject.phases.sole_terminal()
            && terminal != baseline.terminal_phase_id
        {
            report.violate(
                TERMINAL_PHASE_CHANGED,
                phase_subject(subject, terminal),
                format!(
                    "the published revision's terminal phase is {}, and this revision \
                     terminates on {terminal}; the terminal phase id is immutable for the life \
                     of the plan, because the scope-key phase default and usage \
                     phase-invariance are both defined relative to which phase is terminal",
                    baseline.terminal_phase_id
                ),
            );
        }

        for phase_id in &baseline.phase_ids_in_use {
            if subject.phases.find(*phase_id).is_some() {
                continue;
            }
            report.violate(
                PHASE_IN_USE,
                phase_subject(subject, *phase_id),
                "a current published price row is filed on this phase and the revision does \
                 not re-attach it; the row would point at a phase this revision does not hold"
                    .to_owned(),
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Shared readings
// ---------------------------------------------------------------------------

/// The phases lying on a `convertsToPhaseId` cycle.
///
/// Each phase has at most one outgoing edge, so the walk from a phase either
/// ends (terminal or dangling), returns to where it started, or runs into a
/// cycle it is not itself part of. Only the first of those makes a phase a cycle
/// member, which is why the walk tests for the start rather than for any repeat:
/// `A -> B -> C -> B` puts `B` and `C` on the cycle and leaves `A` off it, where
/// it belongs.
fn phases_on_a_cycle(graph: &PhaseGraph) -> BTreeSet<PhaseId> {
    let mut on_cycle = BTreeSet::new();
    for phase in graph.phases() {
        let mut cursor = phase.converts_to_phase_id;
        // A walk longer than the phase set has already repeated a phase, so it
        // is inside a cycle that does not contain this one.
        for _ in 0..=graph.phases().len() {
            let Some(next) = cursor else {
                break;
            };
            if next == phase.phase_id {
                on_cycle.insert(phase.phase_id);
                break;
            }
            cursor = graph
                .find(next)
                .and_then(|found| found.converts_to_phase_id);
        }
    }
    on_cycle
}

/// The `(meter, dimensionKey)` line a usage row prices.
///
/// The line is what `inst-cmp-injective` keys on and what the counter is keyed
/// by, so it is what decides whether one row overrides another. An absent meter
/// is its own line rather than a wildcard: two rows that name no meter are the
/// same line, and neither is the base of a row that names one.
fn line_of(record: &PriceRecord) -> (Option<&str>, &str) {
    (
        record.row.meter.as_deref(),
        record.row.dimension_key.as_str(),
    )
}

/// How a phase-level finding locates its subject for the author.
///
/// The plan-level [`PlanShape::subject`] extended with the phase, so a consumer
/// grouping a report by subject sees one subject per phase rather than one per
/// rule.
fn phase_subject(shape: &PlanShape, phase_id: PhaseId) -> String {
    format!("{}/phase/{phase_id}", shape.subject())
}

/// How an override finding locates its subject: the phase, then the line.
///
/// A trailing empty dimension is the empty-tuple sentinel - the row prices the
/// whole meter and declares no dimension.
fn override_subject(shape: &PlanShape, record: &PriceRecord) -> String {
    let (meter, dimension) = line_of(record);
    format!(
        "{}/{}#{dimension}",
        phase_subject(shape, record.scope_key.phase()),
        meter.unwrap_or("(no meter)")
    )
}

/// A phase id set, rendered for a violation detail.
fn join_ids(ids: &BTreeSet<PhaseId>) -> String {
    ids.iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

/// A `convertsToPhaseId` for a violation detail, naming the absent case.
fn describe(phase_id: Option<PhaseId>) -> String {
    phase_id.map_or_else(|| "nothing (terminal)".to_owned(), |id| id.to_string())
}

#[cfg(test)]
#[path = "phase_graph_tests.rs"]
mod phase_graph_tests;

// ---------------------------------------------------------------------------
// inst-ph-trial (D-151)
// ---------------------------------------------------------------------------

/// `displayTrialDays` is authorable only on a `trial` phase that has a duration
/// to project.
///
/// **The case nothing caught, stated because the rule reads as redundant without
/// it.** `displayTrialDays` is the PRD-named alias of `phaseDurationDays` on a
/// trial phase — one value, two projections — and it is the single source
/// Subscriptions enforces trial runtime from and preview quotes. A plan with **no
/// trial phase at all** could publish a trial length:
///
/// - §6's `CHECK (display_trial_days IS NULL OR display_trial_days =
///   phase_duration_days)` is silent on `kind`, and NULL propagation makes it
///   *satisfied* whenever `phase_duration_days` is NULL while
///   `display_trial_days` is set;
/// - [`PhaseDuration`] is **correct** to find no duration on a terminal phase;
/// - [`TerminalPhaseKind`] is **correct** to find `evergreen` there.
///
/// Three guards, each right, and the shape they exist to forbid passed all of
/// them. That is the same arithmetic [`CycleDeclared`](super::cycle_shape::CycleDeclared)
/// closes one step over.
///
/// The `CHECK` is deliberately **not** tightened (D-151): a phase graph is
/// authored across successive `PATCH`es, and a `phase_duration_days IS NOT NULL`
/// conjunct would make the half-authored draft unsavable. The schema stands
/// behind this rule rather than in front of it, exactly as it does for
/// `inst-ph-duration` and the terminal `kind`.
///
/// # The third fault, and why it is the only one stamped at the write
///
/// The two arms above are the ones the `CHECK` **cannot** see. The one it can —
/// two numbers set and disagreeing — this rule did not report at all, so
/// `chk_pricing_plan_phase_display_trial_days` was the first thing to read the
/// pair and the author was answered `500 … please retry later` about a payload
/// they had typed themselves. It is the class the three commits before it
/// closed on sibling tables.
///
/// It is `violate_at_write` and its two siblings are not, and the asymmetry is
/// the whole point:
///
/// - the drift is read from the submitted phase alone, the `phases` facet is a
///   wholesale replace so the resolving call retracts what was just sent, and
///   **no stored row can ever carry it** — the `CHECK` guarantees that — so at
///   publish this arm has no subject and nothing is lost by judging it earlier;
/// - a non-trial `kind` and a missing `phaseDurationDays` are both states the
///   store accepts, which is exactly what makes them legitimate intermediate
///   drafts (D-151 above). Stamping them would refuse a half-authored trial
///   phase, and D-312's safe direction is to leave a fault at publish rather
///   than refuse an author's intermediate state.
///
/// The drift check runs **before** the `kind` arm's `continue`, because a
/// non-trial phase can drift too and the `CHECK` is silent on `kind`: a payload
/// that is both faults reports both, and the door — which takes
/// `write_stage_only()` — keeps the one it can act on.
///
/// **The phase is named in the report**, because a plan may carry many and the
/// author has to know which one to edit.
#[domain_model]
#[derive(Clone, Copy, Debug, Default)]
pub struct DisplayTrialDaysOnTrialPhase;

impl ValidationRule<PlanShape> for DisplayTrialDaysOnTrialPhase {
    fn name(&self) -> &'static str {
        "inst-ph-trial"
    }

    fn evaluate(&self, subject: &PlanShape, report: &mut ValidationReport) {
        for phase in subject.phases.in_ordinal_order() {
            let Some(days) = phase.display_trial_days else {
                continue;
            };
            let subject_ref = format!("{}|{}", subject.subject(), phase.phase_id);
            // The section 6 CHECK's own predicate, and the only arm of this rule
            // the store refuses — so the only one stamped for the write. Ahead of
            // the `kind` arm's `continue` because the CHECK is silent on `kind`
            // and a non-trial phase drifts the same way.
            if let Some(duration) = phase.phase_duration_days
                && duration != days
            {
                report.violate_at_write(
                    DISPLAY_TRIAL_DAYS_INVALID,
                    subject_ref.clone(),
                    format!(
                        "phase {} carries displayTrialDays {days} and phaseDurationDays \
                         {duration}: the two are one value under two names, so one of them is \
                         the number to change — the section 6 CHECK refuses the pair outright, \
                         and a plan publishing a trial length that is not its trial's length is \
                         one Subscriptions enforces the wrong runtime from",
                        phase.phase_id
                    ),
                );
            }
            if phase.kind != PhaseKind::Trial {
                report.violate(
                    DISPLAY_TRIAL_DAYS_INVALID,
                    subject_ref.clone(),
                    format!(
                        "phase {} is a {} phase and carries displayTrialDays {days}: the value is \
                         the PRD alias of a TRIAL phase's duration, and a plan publishing a trial \
                         length it has no trial phase for is one Subscriptions enforces a trial \
                         runtime from",
                        phase.phase_id,
                        PhaseKind::as_str(phase.kind)
                    ),
                );
                continue;
            }
            if phase.phase_duration_days.is_none() {
                report.violate(
                    DISPLAY_TRIAL_DAYS_INVALID,
                    subject_ref,
                    format!(
                        "trial phase {} carries displayTrialDays {days} and no phaseDurationDays \
                         to project: the two are one value, and the section 6 CHECK is SATISFIED \
                         here because comparing to NULL is NULL",
                        phase.phase_id
                    ),
                );
            }
        }
    }
}
