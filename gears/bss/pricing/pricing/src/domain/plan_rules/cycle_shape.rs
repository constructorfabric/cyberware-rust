//! Billing-cycle shape rules (`cpt-cf-bss-pricing-algo-cycle-shape`).
//!
//! Rules over a [`PlanShape`], each registered under its own instruction id in
//! [`plan_rules::plan_shape_rules`](super::plan_shape_rules), which is the roster.
//! Most of them are
//! review fixes with a named defect behind them, and each
//! doc below carries the defect rather than a restatement of the code: a rule
//! restated without its defect is a rule reimplemented without its guard, and
//! the guard is the only part of it a later edit can silently drop.
//!
//! ## Why the subject is the whole plan and not a row
//!
//! Slice 3's rules are **row-local** (D-21) and judge one
//! [`PriceRow`](crate::domain::price_row::PriceRow) at a time. None of these
//! can be: `HYBRID_INCOMPLETE` is a statement about a row *set*, D-84's
//! completeness is a statement about a set of markets crossed with a set of
//! priced lines, and the setup-row rule has to read the plan's billing cycle to
//! decide whether the row may exist at all. So the subject is the whole
//! [`PlanShape`], which is also where the published baseline and the evaluation
//! instant the availability rule needs are carried.
//!
//! ## Reading `billing_timing` here is not a second registration
//!
//! Global Constraint 5 of this group says `billingTiming` REQUIRED on every
//! recurring row is **Slice 6's** registered rule, cross-referenced and never
//! re-registered. [`SetupRowShape`] reads the same column and is not that rule:
//! Slice 6 owns *required on a recurring row*, Slice 2 owns *forbidden on a
//! setup row*, and `inst-cs-setup` names the field itself. The two can never
//! disagree, because no row is both a recurring row and a setup row — the
//! `chargeKind` axis separates them, which is why that axis is in the scope key
//! at all.
//!
//! ## What is deliberately absent
//!
//! The precedent is [`crate::domain::rules`]'s own absence section: *a rule
//! that always passes is indistinguishable from a rule that holds*. Each
//! absence below names its owner.
//!
//! **Owned by another slice, cross-referenced here:**
//!
//! - `billingTiming` required on every recurring row — **Slice 6**
//!   (`inst-cs-recurring`).
//! - `billingGranularity` on all usage rows and `tierAggregationWindow` on
//!   tiered / `package` rows — **Slice 3**, already implemented as
//!   [`EVAL_POLICY_MISSING`](crate::domain::rules::EVAL_POLICY_MISSING), which
//!   `inst-cs-usage` cross-references.
//! - The setup row's charge-timing semantics (`inst-cs-setup-timing`) — a
//!   **read-model projection** and therefore G6. This slice validates the row's
//!   shape, never when it is charged.
//!
//! **Owned by the product / SKU registry, which this gear cannot read:**
//!
//! - "parent SKU `meteringUnit` required" and `SKU_NOT_PUBLISHED`
//!   (`inst-cs-usage`); the anchor half of `inst-cs-customfreq` is absent for a
//!   different reason, recorded in [`crate::domain::plan_rules`]'s table.
//!
//! **Stated by the design set with no code in §5 to report it.** These are not
//! deferrals, they are gaps: a rule with no code cannot be reported to a
//! caller, and minting one or stretching a neighbour would put a discriminator
//! on the wire that no consumer can act on and no document defines.
//!
//! | Requirement | Where it is stated | What exists |
//! |---|---|---|
//! | exactly one `one_time` base row per sold market | `inst-cs-onetime` | the **at most one** half is the physical partial `UNIQUE` `uq_pricing_price_scope_key_current`; the **at least one** half has no code |
//! | recurring-only add-ons rejected | `inst-cs-onetime` | no code, and no authored term for "recurring-only add-on" outside the registry |
//! | at least one recurring base row per sold market | `inst-cs-recurring` | no code |
//! | frequency metadata present | `inst-cs-recurring` | no code |
//!
//! [`HYBRID_INCOMPLETE`] is the hybrid step's code and [`USAGE_MARKET_INCOMPLETE`]
//! is D-84's usage-line code; neither is widened to cover the four rows above.

use std::collections::{BTreeMap, BTreeSet};

use toolkit_macros::domain_model;

use crate::domain::instant::format_rfc3339;
use crate::domain::money::CurrencyCode;
use crate::domain::plan_rules::{
    AVAILABLE_FROM_IN_PAST, BASE_MARKET_INCOMPLETE, CYCLE_METADATA_MISSING, HYBRID_INCOMPLETE,
    INVALID_CUSTOM_INTERVAL, PURCHASE_QTY_RANGE_INVALID, SETUP_ROW_INVALID,
    USAGE_MARKET_INCOMPLETE,
};
use crate::domain::plan_shape::{BillingCycle, CustomIntervalUnit, Frequency, PlanShape};
use crate::domain::price_record::PriceRecord;
use crate::domain::scope_key::{ChargeKind, PhaseId, Region};
use crate::domain::validation::{ValidationReport, ValidationRule};

/// The `(meter, dimensionKey)` lines a plan prices, each carrying the markets
/// its phase-invariant terminal-phase rows already cover.
///
/// A named alias rather than the type spelled inline: it is the one value
/// [`UsageMarketCompleteness`] is built around, and the two halves of D-84 —
/// which lines exist, and which markets each of them reaches — are the key and
/// the value of exactly this map.
type LineCoverage = BTreeMap<(String, String), BTreeSet<(CurrencyCode, Region)>>;

// ---------------------------------------------------------------------------
// inst-cs-customfreq
// ---------------------------------------------------------------------------

/// `inst-cs-customfreq` — a custom interval is positive and within its unit's
/// cap.
///
/// **An over-cap value is rejected, never silently clamped** (P1). A clamped
/// interval is a billing period the operator did not author and would never
/// see: the plan publishes, the invoice arrives on a cadence nobody chose, and
/// nothing in the catalog records that the authored number was replaced.
///
/// The two caps are **fields**, not a global read. They are the authoring
/// tenant's — their `pricing_policy_object` entry, else the ratified deployment
/// default (D-152) — and [`crate::domain::validation`] requires a rule to be
/// pure with respect to the state it is handed, because the same rule set runs
/// twice: once as a pre-check at submit and again inside the publish commit. A
/// rule that reached for a policy row itself could answer differently in the two
/// runs for no authored reason, and would put a storage read inside a domain
/// rule besides.
///
/// The units do not share a cap and do not share a bound check: PRD §14
/// ratified `customEveryN Days(n) <= 366` and `customEveryN Months(n) <= 24`,
/// so the rule reads the unit before it reads the bound.
///
/// The **anchor half** of this instruction — `customEveryN Days(n)` MUST anchor
/// `subscription_start` — is not here. It reads `billing_anchor_policy`, a
/// Slice-6 column that does not exist in this schema; the reasoning is recorded
/// in [`crate::domain::plan_rules`]'s absence table, and stretching
/// [`INVALID_CUSTOM_INTERVAL`] over it was rejected there.
#[domain_model]
#[derive(Clone, Copy, Debug)]
pub struct CustomIntervalBounds {
    /// Largest `n` a `customEveryN Days(n)` frequency may carry.
    max_custom_interval_days: u32,
    /// Largest `n` a `customEveryN Months(n)` frequency may carry.
    max_custom_interval_months: u32,
}

impl CustomIntervalBounds {
    /// The rule bound to one tenant's caps.
    ///
    /// There is deliberately no `Default`: both caps would derive to `0`, and a
    /// rule holding a zero cap rejects every custom frequency ever authored
    /// while looking exactly like a rule that is switched on.
    #[must_use]
    pub const fn new(max_custom_interval_days: u32, max_custom_interval_months: u32) -> Self {
        Self {
            max_custom_interval_days,
            max_custom_interval_months,
        }
    }

    /// The cap that binds `unit`.
    const fn cap_for(self, unit: CustomIntervalUnit) -> u32 {
        match unit {
            CustomIntervalUnit::Days => self.max_custom_interval_days,
            CustomIntervalUnit::Months => self.max_custom_interval_months,
        }
    }
}

impl ValidationRule<PlanShape> for CustomIntervalBounds {
    fn name(&self) -> &'static str {
        "inst-cs-customfreq"
    }

    fn evaluate(&self, subject: &PlanShape, report: &mut ValidationReport) {
        let Some(Frequency::CustomEveryN { n, unit }) = subject.frequency else {
            return;
        };
        let cap = self.cap_for(unit);
        if n == 0 {
            report.violate(
                INVALID_CUSTOM_INTERVAL,
                subject.subject(),
                format!(
                    "customEveryN {unit}({n}) counts no {unit} at all: a custom frequency \
                     recurs every n of its unit, and n must be positive"
                ),
            );
            return;
        }
        if n > cap {
            report.violate(
                INVALID_CUSTOM_INTERVAL,
                subject.subject(),
                format!(
                    "customEveryN {unit}({n}) is above the configured cap of {cap} {unit}; the \
                     interval is rejected here and never clamped, because a clamped interval is \
                     a billing period the operator did not author and would never see"
                ),
            );
        }
    }
}

// ---------------------------------------------------------------------------
// inst-cs-hybrid
// ---------------------------------------------------------------------------

/// `inst-cs-hybrid` — a hybrid plan carries both of its parts.
///
/// BOTH at least one `recurring` row and at least one `usage` row on the same
/// `planId`, on distinct `chargeKind` keys. A hybrid missing either part is a
/// plan whose commercial shape says one thing and whose row set says another:
/// the subscriber is sold a metered plan that meters nothing, or a subscription
/// fee that never recurs.
///
/// It reports **once per missing part**, so a plan carrying neither reports
/// both. The author remediates a plan in one pass, and a report that named only
/// the first missing part would send them round the pipeline twice.
///
/// This is the *presence* half only. The per-market half — the usage part is
/// required in every market where a recurring row exists, not merely somewhere
/// — is D-84 and belongs to [`UsageMarketCompleteness`], because it is the same
/// rule `inst-cs-usage` states and one rule has one owner.
#[domain_model]
#[derive(Clone, Copy, Debug, Default)]
pub struct HybridCompleteness;

impl ValidationRule<PlanShape> for HybridCompleteness {
    fn name(&self) -> &'static str {
        "inst-cs-hybrid"
    }

    fn evaluate(&self, subject: &PlanShape, report: &mut ValidationReport) {
        if subject.billing_cycle != Some(BillingCycle::Hybrid) {
            return;
        }
        for part in [ChargeKind::Recurring, ChargeKind::Usage] {
            if subject
                .rows
                .iter()
                .any(|record| record.scope_key.charge_kind() == part)
            {
                continue;
            }
            report.violate(
                HYBRID_INCOMPLETE,
                subject.subject(),
                format!(
                    "a hybrid plan carries a recurring part and a usage part on one planId, on \
                     distinct chargeKind keys; the {part} part is missing"
                ),
            );
        }
    }
}

// ---------------------------------------------------------------------------
// inst-cs-usage / inst-cs-hybrid (D-84)
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// inst-cs-declared (D-149 clause 2)
// ---------------------------------------------------------------------------

/// The plan has said how it charges — and it is the **first** rule of the step.
///
/// **The defect, which is a defect of the step and not of any one rule.**
/// `billing_cycle` is `Option`, legitimately: a plan is authored incrementally in
/// `draft` (§4.2 step 1) and nothing in the §17.1 matrix is implied. Every other
/// rule of the cycle-shape step then reads the cycle first and returns when it is
/// absent — correctly, because a rule that read no cycle and reported a fault
/// would be reporting one the author did not make. The consequence nobody had
/// looked at is that the *step* passed a `NULL` cycle **vacuously**: five rules,
/// each right on its own, adding up to no judgement at all.
///
/// So this rule is the one that does not decline. A plan that has said nothing
/// about how it charges is exactly the plan a fail-closed validator cannot
/// protect, and without this rule it reaches publish unjudged.
///
/// **Two fields, one code** (D-146's line): an absent `billing_cycle`, and an
/// absent `frequency` on a cycle that carries a recurring part. The operator's
/// next action is identical — author the field the plan is missing — and the
/// report names which one it is.
///
/// The frequency half is scoped by [`BillingCycle::has_recurring_part`] rather
/// than by a cycle list spelled here: `inst-cs-recurring` states it of
/// `recurring` and `hybrid` plans, which is what that predicate means, and a
/// `one_time` or `usage` plan has no recurrence to describe.
///
/// **A first publish with a `None` baseline still declines to judge.** Nothing
/// here reads the baseline, so this rule is unaffected by it — which is the
/// `inst-cs-declared` posture: the rule asks what the plan says about itself, not
/// what it used to say.
#[domain_model]
#[derive(Clone, Copy, Debug, Default)]
pub struct CycleDeclared;

impl ValidationRule<PlanShape> for CycleDeclared {
    fn name(&self) -> &'static str {
        "inst-cs-declared"
    }

    fn evaluate(&self, subject: &PlanShape, report: &mut ValidationReport) {
        let Some(cycle) = subject.billing_cycle else {
            report.violate(
                CYCLE_METADATA_MISSING,
                subject.subject(),
                "billingCycle is unset: a plan MUST carry a cycle from the 17.1 matrix at \
                 publish. Every other rule of the cycle-shape step is conditioned on it, so an \
                 unset cycle is not a plan judged and passed - it is a plan nothing judged",
            );
            return;
        };
        if cycle.has_recurring_part() && subject.frequency.is_none() {
            report.violate(
                CYCLE_METADATA_MISSING,
                subject.subject(),
                format!(
                    "frequency is unset on a {cycle} plan: a plan that recurs and does not say \
                     when it charges has told a subscriber nothing about the interval it is \
                     billed over"
                ),
            );
        }
    }
}

// ---------------------------------------------------------------------------
// inst-cs-onetime / inst-cs-recurring (D-149 clause 1)
// ---------------------------------------------------------------------------

/// Every sold market carries the base row its cycle mandates.
///
/// **The defect.** A `one_time` plan selling in EUR and USD with a `one_time` row
/// only in EUR is **sellable in USD** and there is nothing to charge for there —
/// the plan is bought for nothing. The recurring arm is the same sentence over
/// `chargeKind = recurring`. `inst-cs-onetime` and `inst-cs-recurring` both stated
/// it from the start and §5 named no code for either until D-149.
///
/// One rule over two `chargeKind` values, because the *at most one* half of the
/// one-time rule is already the published-plane scope-key partial `UNIQUE`
/// (`chargeKind` is a key axis) — what is missing is only the *at least one*
/// half, and it is the same question on both cycles.
///
/// ## The mandated kind, and the cycle that mandates none
///
/// `one_time` mandates a `one_time` row; `recurring` and `hybrid` mandate a
/// `recurring` one. `usage` mandates **no** base row at all, so this rule says
/// nothing there — a usage-only plan's market completeness is
/// [`UsageMarketCompleteness`]'s, over the metered lines.
///
/// ## It never contests [`HYBRID_INCOMPLETE`], and that is enforced here
///
/// The two codes answer two questions and must not both fire on one plan.
/// `HYBRID_INCOMPLETE` means *a part is missing **anywhere*** and never names a
/// market; this one means *a part exists and does not reach every sold market*.
/// So on a `hybrid` plan carrying no row of the mandated kind at all, this rule
/// stays silent and lets the hybrid rule speak: the author's remediation is "add
/// the recurring part", not "add it in four markets".
///
/// On a non-`hybrid` cycle there is no hybrid rule to defer to, so a plan with no
/// base row anywhere is reported here, once per sold market.
#[domain_model]
#[derive(Clone, Copy, Debug, Default)]
pub struct BaseMarketCompleteness;

impl ValidationRule<PlanShape> for BaseMarketCompleteness {
    fn name(&self) -> &'static str {
        // `inst-cs-recurring`, whose only registered clause this is: its other two
        // are Slice 6's `billingTiming` (cross-referenced, never re-registered) and
        // the frequency half of `CYCLE_METADATA_MISSING`.
        "inst-cs-recurring"
    }

    fn evaluate(&self, subject: &PlanShape, report: &mut ValidationReport) {
        let Some(cycle) = subject.billing_cycle else {
            return;
        };
        let Some(base) = mandated_base_kind(cycle) else {
            return;
        };
        let carried = subject.markets_with(base);
        // The hybrid deferral: a part missing everywhere is `HYBRID_INCOMPLETE`'s
        // sentence, and two codes for one remediation is one code too many.
        if carried.is_empty() && matches!(cycle, BillingCycle::Hybrid) {
            return;
        }
        for (currency, region) in subject.markets().difference(&carried) {
            report.violate(
                BASE_MARKET_INCOMPLETE,
                format!("{}|{currency}|{region}", subject.subject()),
                format!(
                    "a {cycle} plan sells in {currency}/{region} and carries no {base} base row \
                     there: a subscriber in that market is sold a plan with nothing to charge \
                     them for. A market the plan does not sell in is one with no rows at all, \
                     never one with a row of another kind"
                ),
            );
        }
    }
}

/// The base `chargeKind` a cycle mandates in every market it sells in, or `None`
/// for a cycle that mandates no base row.
///
/// Written as a `match` over [`BillingCycle`] rather than as two predicates, so a
/// cycle added to the matrix is a compile error here — the same reason
/// `partition_row_fields` has no rest pattern.
const fn mandated_base_kind(cycle: BillingCycle) -> Option<ChargeKind> {
    match cycle {
        BillingCycle::OneTime => Some(ChargeKind::OneTime),
        BillingCycle::Recurring | BillingCycle::Hybrid => Some(ChargeKind::Recurring),
        // A usage-only plan has no base row to be complete about.
        BillingCycle::Usage => None,
    }
}

/// D-84 — every priced line reaches every market the plan sells in.
///
/// **The defect, stated because the rule is unreadable without it.** A hybrid
/// selling recurring in EUR and USD with usage priced only in EUR is
/// **sellable in USD**, and the USD subscriber's usage events fail closed at
/// rating time — there is no row to price them from. That is the "sold but
/// unrateable" state D-15/D-17 declare impossible by construction, and nothing
/// else in the pipeline sees it: the row rules pass (every row is well formed),
/// the hybrid presence rule passes (a usage row exists), and the plan
/// publishes.
///
/// A market where usage is genuinely **free is an explicit `$0` row, never an
/// absence**. That sentence is the one an author acts on: the rule is satisfied
/// by pricing the line at zero in the market, not by arguing the line does not
/// apply there.
///
/// Registered under `inst-cs-usage`. The design set states the same rule twice
/// — once in the usage step and once in the hybrid step, with the same
/// rationale — and it is registered once, because two registrations of one rule
/// are two owners free to disagree.
///
/// ## The market set differs by cycle, and both spellings are normative
///
/// On `hybrid` it is every `(currency, region)` **where a recurring row
/// exists** — the recurring row is what makes the plan sellable there. On
/// `usage` there is no recurring row, so it is the **union of the plan's usage
/// rows' markets**. The scope of the rule is
/// [`BillingCycle::requires_usage_part`], never a set of cycles re-spelled
/// here.
///
/// ## The line set is terminal-phase rows only
///
/// Usage rows are **phase-invariant by default** (D-15/D-19): one row on the
/// terminal phase covers all phases, and a phase-scoped row overrides it for
/// its own phase. Phase-scoped overrides are therefore **additive and exempt**
/// — from both halves of the evaluation. They do not add a line to the set,
/// and they do not discharge a market obligation either: after the phase
/// converts, an override resolves to nothing, so counting it as coverage would
/// re-open the very hole the rule closes (the same hole D-117's
/// `PHASE_OVERRIDE_ORPHANED` closes from the other side).
///
/// When the phase set does not name **exactly one** terminal phase, this rule
/// says nothing: "the terminal-phase rows" has no referent, and
/// `PHASE_GRAPH_INVALID` has already recorded the one fault the author has to
/// fix first. The publish is still blocked by that violation, so the guard is
/// deferred and never dropped.
#[domain_model]
#[derive(Clone, Copy, Debug, Default)]
pub struct UsageMarketCompleteness;

impl ValidationRule<PlanShape> for UsageMarketCompleteness {
    fn name(&self) -> &'static str {
        "inst-cs-usage"
    }

    fn evaluate(&self, subject: &PlanShape, report: &mut ValidationReport) {
        let Some(cycle) = subject.billing_cycle else {
            return;
        };
        if !cycle.requires_usage_part() {
            return;
        }
        let markets = if matches!(cycle, BillingCycle::Hybrid) {
            subject.markets_with(ChargeKind::Recurring)
        } else {
            subject.markets_with(ChargeKind::Usage)
        };
        let Some(terminal) = subject.phases.sole_terminal() else {
            return;
        };

        let lines = priced_lines(subject, terminal);
        for ((meter, dimension_key), covered) in &lines {
            for (currency, region) in markets.difference(covered) {
                report.violate(
                    USAGE_MARKET_INCOMPLETE,
                    format!(
                        "{}|{meter}|{dimension_key}|{currency}|{region}",
                        subject.subject()
                    ),
                    format!(
                        "the line (meter {meter}, dimensionKey '{dimension_key}') is priced by \
                         this plan but has no usage row in {currency}/{region}, a market the \
                         plan sells in: a subscriber there is sold a plan whose usage events \
                         nothing can rate. Usage that is genuinely free in a market is an \
                         explicit 0 row, never an absence"
                    ),
                );
            }
        }
    }
}

/// The lines the plan prices on `terminal`, each with the markets it reaches.
fn priced_lines(subject: &PlanShape, terminal: PhaseId) -> LineCoverage {
    let mut lines = LineCoverage::new();
    for record in subject.usage_rows() {
        if record.scope_key.phase() != terminal {
            continue;
        }
        // A usage row with no meter prices no line. `inst-cs-usage` requires
        // the parent SKU's `meteringUnit`, and that check is a registry read
        // this gear cannot make (see the module doc), so the row is passed over
        // rather than reported here under a code that does not mean it.
        let Some(meter) = record.row.meter.clone() else {
            continue;
        };
        lines
            .entry((meter, record.row.dimension_key.clone()))
            .or_default()
            .insert((
                record.scope_key.currency().clone(),
                record.scope_key.region().clone(),
            ));
    }
    lines
}

// ---------------------------------------------------------------------------
// inst-cs-setup
// ---------------------------------------------------------------------------

/// `inst-cs-setup` — where a setup row may live, and what it may not carry.
///
/// A `one_time_setup` row is permitted only on a plan whose cycle admits one
/// ([`BillingCycle::admits_setup_row`]), and it is **validated as a one-time
/// charge**: no `billingTiming`, no tier bands, no `tierAggregationWindow`, no
/// `tierQualificationWindow`. Each of those is machinery for a charge that
/// repeats or accumulates, and a setup fee does neither — it is charged once
/// per subscription lifetime. A setup row carrying a tier band prices from a
/// column nothing reads; one carrying `billingTiming` states an advance /
/// arrears position for a period it does not have.
///
/// **Reading `billingTiming` here is not the Global Constraint 5 clash.**
/// Slice 6 owns *required on a recurring row*; this owns *forbidden on a setup
/// row*; `inst-cs-setup` names the field itself. See the module doc — the two
/// rules cannot disagree, because the `chargeKind` axis makes "recurring row"
/// and "setup row" disjoint.
///
/// "No recurrence" is the fourth thing the instruction asks for and there is no
/// row-level field to check: `frequency` is a **plan** column, so a setup row
/// cannot carry a recurrence of its own. The instruction is satisfied by the
/// shape of the type rather than by a check, which is the outcome the type was
/// built for and not an omission.
///
/// The setup row's **timing** — once per subscription lifetime, at activation
/// or at entry into the first non-trial phase — is `inst-cs-setup-timing`, a
/// read-model projection and therefore G6. This rule judges the row's shape,
/// never when it is charged.
#[domain_model]
#[derive(Clone, Copy, Debug, Default)]
pub struct SetupRowShape;

impl ValidationRule<PlanShape> for SetupRowShape {
    fn name(&self) -> &'static str {
        "inst-cs-setup"
    }

    fn evaluate(&self, subject: &PlanShape, report: &mut ValidationReport) {
        for record in &subject.rows {
            if record.scope_key.charge_kind() != ChargeKind::OneTimeSetup {
                continue;
            }
            check_setup_cycle(subject, record, report);
            check_setup_fields(record, report);
        }
    }
}

/// The setup row lives on a plan whose cycle admits it.
///
/// A shape with no authored `billing_cycle` says nothing: the cycle is what
/// this arm judges against, and inventing a default for it would let an
/// unfinished draft be rejected for a cycle nobody chose.
fn check_setup_cycle(subject: &PlanShape, record: &PriceRecord, report: &mut ValidationReport) {
    let Some(cycle) = subject.billing_cycle else {
        return;
    };
    if cycle.admits_setup_row() {
        return;
    }
    report.violate(
        SETUP_ROW_INVALID,
        record.price_id.to_string(),
        format!(
            "a one_time_setup row is authorable only on a plan whose cycle carries a recurring \
             part (recurring, hybrid); this plan's cycle is {cycle}. A setup fee on a {cycle} \
             plan is a second one-off charge, which is a one_time row"
        ),
    );
}

/// The setup row carries none of the recurrence or tier machinery.
fn check_setup_fields(record: &PriceRecord, report: &mut ValidationReport) {
    let mut carried = Vec::new();
    if record.billing_timing.is_some() {
        carried.push("billingTiming");
    }
    if !record.row.bands.is_empty() {
        carried.push("tier bands");
    }
    if record.row.tier_aggregation_window.is_some() {
        carried.push("tierAggregationWindow");
    }
    if record.row.tier_qualification_window.is_some() {
        carried.push("tierQualificationWindow");
    }
    if carried.is_empty() {
        return;
    }
    report.violate(
        SETUP_ROW_INVALID,
        record.price_id.to_string(),
        format!(
            "a one_time_setup row is validated as a one-time charge and carries no recurrence or \
             tier machinery; this one carries {}",
            carried.join(", ")
        ),
    );
}

// ---------------------------------------------------------------------------
// inst-cs-onetime
// ---------------------------------------------------------------------------

/// `inst-cs-onetime` — the purchase-quantity window admits a quantity.
///
/// `purchase_min_qty <= purchase_max_qty` when both are set. An inverted window
/// is a plan nobody can buy: every quantity is below the floor or above the
/// ceiling, and the refusal arrives at checkout rather than at publish.
///
/// The physical `CHECK` Task 2 puts on the table makes this **unreachable
/// through `PlanRepo`**, and the rule exists anyway for the reason the whole
/// pipeline exists: it judges a subject the **caller assembled**, not a row the
/// store handed back. A caller validating a candidate shape before writing it,
/// or a shape assembled from somewhere other than this gear's own tables, gets
/// the finding in the enumerated report where the author can read it — instead
/// of a driver error at `INSERT` time, which truncates the report at the first
/// fault and names a constraint rather than a rule.
///
/// **That caller is `api::rest::plans`' `require_authorable_purchase_window`**, and
/// naming it is the correction a review made: until then there
/// was no such caller anywhere, every publish subject being assembled from storage,
/// so the only path an inverted window had was the `INSERT`/`UPDATE` — answered
/// `500 … please retry later` — and `PURCHASE_QTY_RANGE_INVALID` was a declared code
/// no path could raise. The door runs this rule over the submitted shape at the
/// parse of both `POST /plans` and `PATCH /plans/{id}`; it reports through `violate`
/// unchanged, because a door that asks the pipeline whether a shape is publishable
/// needs no second stamp.
#[domain_model]
#[derive(Clone, Copy, Debug, Default)]
pub struct PurchaseQtyRange;

impl ValidationRule<PlanShape> for PurchaseQtyRange {
    fn name(&self) -> &'static str {
        "inst-cs-onetime"
    }

    fn evaluate(&self, subject: &PlanShape, report: &mut ValidationReport) {
        let (Some(min), Some(max)) = (subject.purchase_min_qty, subject.purchase_max_qty) else {
            return;
        };
        if min <= max {
            return;
        }
        report.violate(
            PURCHASE_QTY_RANGE_INVALID,
            subject.subject(),
            format!(
                "purchase_min_qty {min} is above purchase_max_qty {max}: the window admits no \
                 quantity, so the plan is publishable and unbuyable"
            ),
        );
    }
}

// ---------------------------------------------------------------------------
// inst-cs-availability
// ---------------------------------------------------------------------------

/// `inst-cs-availability` — a past `availableFrom` is backdating, unless it is
/// the date the plan already published with.
///
/// **Cycle-independent.** The rule was hoisted out of the one-time step by the
/// review fix and binds every billing cycle: backdating a recurring
/// plan's availability is the same act as backdating a one-time plan's, and
/// leaving it in the one-time step meant three of the four cycles were
/// unguarded. The Slice-5 historical-import path is the only sanctioned
/// backdating, and it is not this pipeline.
///
/// **It binds newly set or changed values only** (fix), and
/// that qualifier is the whole rule. Without it, every later re-publish of a
/// once-future-dated plan — a descriptor fix, a new market, a corrected GL code
/// — is blocked the moment the authored date passes, until the operator erases
/// a date that was correct when it was authored and is still correct now. The
/// referent is [`PublishedBaseline::available_from`](crate::domain::plan_shape::PublishedBaseline::available_from);
/// a `None` baseline is a **first publish**, where every authored date is newly
/// set and there is nothing to compare it against.
///
/// The now is [`PlanShape::evaluated_at`], never a clock read here: the same
/// rule set runs twice, and a rule reading the wall clock would answer
/// differently in the two runs for no authored reason.
#[domain_model]
#[derive(Clone, Copy, Debug, Default)]
pub struct AvailableFromNotBackdated;

impl ValidationRule<PlanShape> for AvailableFromNotBackdated {
    fn name(&self) -> &'static str {
        "inst-cs-availability"
    }

    fn evaluate(&self, subject: &PlanShape, report: &mut ValidationReport) {
        let Some(available_from) = subject.available_from else {
            return;
        };
        if available_from >= subject.evaluated_at {
            return;
        }
        let published = subject
            .baseline
            .as_ref()
            .and_then(|baseline| baseline.available_from);
        if published == Some(available_from) {
            return;
        }
        report.violate(
            AVAILABLE_FROM_IN_PAST,
            subject.subject(),
            format!(
                "availableFrom {} is newly set or changed and is before the evaluation instant \
                 {}: a plan may not be made available in the past. The historical-import path is \
                 the only sanctioned backdating; re-publishing the date this plan already \
                 published with is not backdating and is allowed",
                format_rfc3339(available_from),
                format_rfc3339(subject.evaluated_at)
            ),
        );
    }
}

#[cfg(test)]
#[path = "cycle_shape_tests.rs"]
mod cycle_shape_tests;
