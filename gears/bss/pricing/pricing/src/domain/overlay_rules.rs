//! `PriceOverlayValidator` — **ten of `design/09-price-overlays.md` §3's eleven
//! `inst-plv-*` rules**, as one aggregate pipeline over a snapshot.
//!
//! Where [`crate::domain::overlay`] is the shapes, this module is the judgement:
//! `inst-plv-scope`, `inst-plv-lines`, `inst-plv-adjustment`,
//! `inst-plv-eligibility`, `inst-plv-precedence`, `inst-plv-class-tiebreak`,
//! `inst-plv-dating`, `inst-plv-referential` and `inst-plv-taxbasis`, with
//! `inst-plv-disclosure`'s catalog half discussed below. The one genuinely
//! outside this crate is `inst-plv-member-preview`, and the section on absences
//! says so.
//!
//! **The count is deliberate**, and is what makes the line above a census rather
//! than a list: §3 carries a fixed roster of eleven, so "ten of eleven" is a
//! claim that can be audited against it — and a rule the census does not name
//! reads as unregistered whether or not it is. `inst-plv-class-tiebreak` is one
//! of the ten, in both its halves, which the absences section three screens down
//! states as well.
//!
//! # One pipeline, not two, and D-213's shape found once
//!
//! The set runs **twice** — as a pre-check at submit and again inside the
//! publish-commit transaction (Foundation §4.2) — so every rule here is pure
//! with respect to the [`OverlayWorld`] it is handed, and none of them fetches
//! its own inputs. That is what makes one pipeline right rather than two: the
//! authoring-edge run and the publish-time run ask the *same* questions of two
//! different worlds, and a rule split across two pipelines would answer one of
//! them in only one of the runs.
//!
//! **One rule is genuinely outside it**, and it is D-213's shape one slice over:
//! [`check_tax_basis_declared`]. L5 says a basis MUST be declared and *silence
//! fails*, but `tax_basis` is `NOT NULL` in the store and [`TaxBasis`] is a
//! closed enum, so a **stored** overlay always has one and the publish-time arm
//! of that rule is unreachable. It therefore belongs to the authoring edge,
//! which is where the surface calls it from, and not to the walk below — a
//! pipeline taking `Option<TaxBasis>` would carry an arm no stored row can
//! reach. This is exactly `bundle_rules::check_basis_declared`'s argument, found
//! independently here.
//!
//! # Rules this module deliberately does **not** hold
//!
//! * **The scope pairing.** `(scope_class = 'global') = (scope_value = '')` is
//!   unrepresentable in [`ScopeSelector`], so there is no arm for it. What is
//!   left checkable — *is this value declared in its universe* — is
//!   [`check_scope`]'s, and the lookup is the repository's.
//! * **`fixed` is amount-based, a SKU line names its plan, a cohort line names
//!   its plan.** All three are unconstructible; see [`crate::domain::overlay`].
//! * **`inst-plv-disclosure`.** There is nothing here to *reject*: `disclosure`
//!   is a closed enum with a fail-closed default, and §3 step 7's rule is about
//!   **exposure** — the overlay is excluded from consumer-facing enumeration and
//!   from the base-price preview, while operator and service reads are
//!   explicitly unaffected. This gear mounts **no consumer-facing enumeration**
//!   (`GET /bss-pricing/v1/price-overlays` is the admin/Tariffs read), so the
//!   catalog's whole half of that rule is: carry the flag, default it
//!   `restricted`, publish it. The exclusion half has no subject in this crate
//!   and is owed by Presentation and the Tariffs preview (F-34). Stated here
//!   rather than left as a missing rule, because an absent arm reads as an
//!   oversight.
//! * **`inst-plv-class-tiebreak`** is here in **both** its halves: the class
//!   order it publishes is [`ScopeClass`]'s derived `Ord`, because the read
//!   model has to carry it, and the cross-class equal-precedence *warning* it
//!   also asks for is [`EQUAL_PRECEDENCE_CROSS_CLASS_TIE`], raised by
//!   [`check_cross_class_tie`] over the ties `overlay_repo` collects. *(This
//!   paragraph called the warning "owed" for as long as it existed beside the
//!   rule that raises it — an owed-arm note is read as a to-do list, so one
//!   naming something already built sends a reader to write it twice.)*
//! * **`inst-plv-member-preview`** is outside this strand's census: it belongs
//!   to the Tariffs preview and has no subject in this crate.
//!
//! # The direction D-138's warning needs, and has had since D-249
//!
//! [`FIXED_LINE_DISCARDS_STACK`] fires when a `fixed` line *"is not the
//! lowest-precedence layer able to match its target"*. That condition presumes
//! the stack is applied from the low end, and **that is the decided direction**:
//! D-249 settled the sort as **ascending** — lowest `precedence`
//! first, most specific last — so a `fixed` layer discards everything at a lower
//! position in the total order `precedence → class order → overlay id`, and the
//! warning fires in the direction D-138 wrote it for. The constant's own doc,
//! [`ScopeClass`]'s and [`OverlayWorld::layers_beneath`]'s all say the same, and
//! the register entry is closed.
//!
//! **The uncertainty is history, kept because the shape of it recurs.** Before
//! D-249, §F.1 recorded the sort direction as undecided, and under the opposite
//! reading a `fixed` at a higher precedence would be applied *first* and discard
//! nothing while a `fixed` at the lowest precedence discarded everything above
//! it — the condition exactly **inverted**. What was built was D-138's own words
//! literally, correct under one of the two readings, which is why D-220 flagged
//! it rather than letting it stand. This section went on asserting that
//! undecidedness, and an open `[H]` entry with it, for as long as the decision
//! had been taken — telling the first reader of this rule set that a shipped,
//! live warning "fires on the wrong half of the cases" and inviting them to
//! invert or delete a rule D-249 had already settled in the direction
//! implemented.

use std::collections::{BTreeMap, BTreeSet};

use toolkit_macros::domain_model;
use uuid::Uuid;

use crate::domain::instant::format_rfc3339;
use crate::domain::money::CurrencyCode;
use crate::domain::overlay::{
    Adjustment, Disclosure, LineKey, OverlayInterval, OverlayLine, ScopeClass, ScopeSelector,
    TargetRef, TargetSku, TaxBasis,
};
use crate::domain::scope_key::PlanId;
use crate::domain::validation::ValidationReport;
use time::OffsetDateTime;

// ---------------------------------------------------------------------------
// The codes.
// ---------------------------------------------------------------------------

/// A non-`global` scope value that its declared universe does not carry
/// (`inst-plv-scope`, D-120).
///
/// **Minted by this gear.** §2 calls it *"taxonomy failure (422)"* and §5's
/// problem-response list declares no code string for it, so a consumer needs a
/// discriminator that is not the message. A gear may mint a `DomainError`
/// variant freely — a wire code is the design set's to declare — which is why
/// this is in the owed register beside Slice 8's `BUNDLE_EXISTS_ON_PLAN`.
pub const SCOPE_VALUE_UNKNOWN: &str = "SCOPE_VALUE_UNKNOWN";

/// An overlay carrying no adjustment line at all (D-42: *"≥ 1 line per
/// overlay"*; §9: *"a zero-line overlay fails"*).
///
/// **Minted by this gear**, for [`SCOPE_VALUE_UNKNOWN`]'s reason and with one
/// more: §5 declares nine overlay codes and none of them covers an empty line
/// set. Reporting it under [`OVERLAY_LINE_TARGET_UNKNOWN`] — the nearest
/// candidate — would send the operator to check a target that does not exist.
pub const OVERLAY_HAS_NO_LINE: &str = "OVERLAY_HAS_NO_LINE";

/// A second line on one `(planId, targetSku, cohort)` key within one overlay
/// revision (§5, D-42; `cohort` since D-78).
pub const OVERLAY_LINE_DUPLICATE: &str = "OVERLAY_LINE_DUPLICATE";

/// A line naming a plan outside the overlay's `target_ref` scope, or a SKU its
/// plan does not publish (§5, D-42).
pub const OVERLAY_LINE_TARGET_UNKNOWN: &str = "OVERLAY_LINE_TARGET_UNKNOWN";

/// A line naming a `cohort` no published `existing_grandfathered` row of its
/// target plan carries (§5, D-78).
pub const OVERLAY_LINE_COHORT_UNKNOWN: &str = "OVERLAY_LINE_COHORT_UNKNOWN";

/// An amount-based line missing a value for a currency its target scope sells
/// (§5, D-08 — no implicit FX).
pub const ADJUSTMENT_CURRENCY_NOT_COVERED: &str = "ADJUSTMENT_CURRENCY_NOT_COVERED";

/// A magnitude outside D-67's range: a `discount` bp outside `(0, 10000]`, a
/// non-positive `markup` bp, or a negative amount magnitude (§5).
pub const ADJUSTMENT_MAGNITUDE_OUT_OF_RANGE: &str = "ADJUSTMENT_MAGNITUDE_OUT_OF_RANGE";

/// A scope referencing an unpublished plan or SKU (§5, `inst-plv-referential`).
pub const TARGET_UNPUBLISHED: &str = "TARGET_UNPUBLISHED";

/// A duplicate `precedence` within one scope class (§5, L2).
pub const PRECEDENCE_DUPLICATE: &str = "PRECEDENCE_DUPLICATE";

/// Overlapping effective intervals for one line key
/// `(scope_class, scope_value, planId, targetSku, cohort)` (§5, D-107).
pub const OVERLAY_INTERVAL_OVERLAP: &str = "OVERLAY_INTERVAL_OVERLAP";

/// No tax basis declared and none explicitly delegated (§5, L5).
pub const TAX_BASIS_UNDECLARED: &str = "TAX_BASIS_UNDECLARED";

/// An overlay whose own `[effectiveFrom, effectiveTo)` is empty or inverted
/// (§1.7's *"effective-interval sanity"*, `inst-plv-dating`).
///
/// **Minted by this gear**, for [`SCOPE_VALUE_UNKNOWN`]'s reason: §5 declares
/// nine overlay codes and none covers an empty interval. This is the window
/// plane's `RepoError::WindowIntervalEmpty` situation exactly — *"§5 declares
/// eight window codes and none of them covers an empty interval"* — and the same
/// answer: the gear names a refusal it owns rather than borrowing a code that
/// names something else.
///
/// `effective_to == effective_from` is **not** a zero-length window: nothing is
/// effective over it and no consumer can resolve an overlay at any instant of it.
pub const OVERLAY_INTERVAL_INVALID: &str = "OVERLAY_INTERVAL_INVALID";

/// **Warning.** A `fixed` line that is not the lowest-precedence layer able to
/// match its target: it silently voids the layers below it (§5, D-138).
///
/// **Its condition is settled as of D-249**: the stack is applied
/// **ascending**, so a `fixed` layer discards everything at a lower position in
/// the total order and this warning fires in the direction D-138 wrote it for.
/// Until that decision the condition was not merely uncertain but *inverted*
/// under the other reading, which is why D-220 flagged it rather than letting it
/// stand.
pub const FIXED_LINE_DISCARDS_STACK: &str = "FIXED_LINE_DISCARDS_STACK";

/// **Warning.** Two published overlays of **different** classes hold the same
/// `precedence` and reach a common plan, so the class order — not the author —
/// decides which is beneath (`inst-plv-class-tiebreak`, D-230).
///
/// The tie is **legal** and the break is deterministic; nothing is refused. What
/// the warning buys is that the operator sees the tie before relying on it,
/// because `precedence` is unique only within a class and an author reading two
/// overlays at "the same precedence" has no reason to expect one to win.
pub const EQUAL_PRECEDENCE_CROSS_CLASS_TIE: &str = "EQUAL_PRECEDENCE_CROSS_CLASS_TIE";

/// **Warning.** A published overlay targets a retired plan: dangling-and-flagged
/// (D-31), remediation is to end or retarget the overlay.
///
/// **Minted by this gear.** D-31 names the read-model flag and the
/// `pricing.priceoverlay.target_retired` alarm and no wire code, because the
/// design set never expected this to reach a caller — it is a state an operator
/// discovers, not a request they made. It is a warning here so an author
/// retargeting an overlay onto a retired plan is told at the moment they do it.
pub const TARGET_RETIRED: &str = "TARGET_RETIRED";

// ---------------------------------------------------------------------------
// The snapshot.
// ---------------------------------------------------------------------------

/// One published line interval of some **other** overlay, as the caller's read
/// found it — `inst-plv-dating`'s collision domain.
///
/// The interval is the **overlay's**, not the line's: overlays carry the dating
/// and lines do not (§6 puts `effective_from` / `effective_to` on the header).
/// The line key is what the collision is *per*, which is D-42's reading rather
/// than a per-`(scope, target)` one.
#[domain_model]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublishedLineInterval {
    /// The overlay holding it. Compared against the candidate's own id, because
    /// the collision domain is *other* overlays (D-107).
    pub price_overlay_id: Uuid,
    /// The holding overlay's scope — part of the collision key.
    pub scope: ScopeSelector,
    /// The holding line's key.
    pub key: LineKey,
    /// The holding overlay's own interval.
    pub interval: OverlayInterval,
}

/// Everything the rules need to know about the world, resolved by the caller.
///
/// None of it is derivable here and all of it is a read. §4.2's contract
/// requires a rule to be pure with respect to the state it is handed, because
/// the same set runs twice and the world moves between the two runs; a validator
/// that fetched its own inputs would be answering two different questions.
#[domain_model]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OverlayWorld {
    /// Is the candidate's scope value declared in `class.taxonomy_table()`?
    ///
    /// Meaningless for [`ScopeSelector::Global`], which consults no universe —
    /// [`check_scope`] does not read it in that case.
    pub scope_value_declared: bool,
    /// Which of the candidate's `target_ref` plans are published.
    pub published_plans: BTreeSet<PlanId>,
    /// Which SKUs each published plan carries.
    pub published_skus: BTreeMap<PlanId, BTreeSet<TargetSku>>,
    /// Which of them are retired — D-31's flag, never a block.
    pub retired_plans: BTreeSet<PlanId>,
    /// The currencies each target plan sells: `ADJUSTMENT_CURRENCY_NOT_COVERED`'s
    /// domain.
    pub sold_currencies: BTreeMap<PlanId, BTreeSet<CurrencyCode>>,
    /// The cutover instants of each plan's published grandfathered generations
    /// (D-78).
    pub published_cohorts: BTreeMap<PlanId, BTreeSet<OffsetDateTime>>,
    /// The **other** published overlay holding this `(scope_class, precedence)`,
    /// if any. `None` is the ordinary answer.
    pub precedence_holder: Option<Uuid>,
    /// Other overlays' published line intervals.
    pub interval_holders: Vec<PublishedLineInterval>,
    /// The plans for which some published overlay sits **beneath** this
    /// candidate in the stack — D-138's warning domain.
    ///
    /// Renamed from `layers_beneath` by D-220/D-249,
    /// because that name became false: "beneath" is the **total** order
    /// `precedence → class order → overlay id` read ascending (D-249), so it is
    /// a strictly lower precedence **or** an equal precedence with a lower
    /// class. `precedence` is unique only *within* a class, so the second case
    /// is reachable and trips no duplicate refusal — and collecting only the
    /// first is why a `fixed` line discarding an equal-precedence lower-class
    /// layer was silently **not** warned (D-220's first clause).
    pub layers_beneath: BTreeSet<PlanId>,
    /// Published overlays of a **different** class holding this candidate's
    /// precedence, with the plans they overlap on (`inst-plv-class-tiebreak`,
    /// D-230).
    ///
    /// Distinct from [`Self::layers_beneath`] on purpose: that one answers *what
    /// a `fixed` discards*, this one answers *where the class order is deciding
    /// something the author may not have intended*. A tie is legal and broken
    /// deterministically; the warning exists so the operator sees it before
    /// relying on the break, which is what D-230 asks for.
    pub cross_class_ties: Vec<CrossClassTie>,
}

/// One published overlay tying with the candidate on precedence across classes.
#[domain_model]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CrossClassTie {
    /// The tying overlay.
    pub price_overlay_id: Uuid,
    /// Its class — the tie-break's other side.
    pub class: ScopeClass,
    /// The plans both reach.
    pub plans: BTreeSet<PlanId>,
}

/// One overlay revision as an author submits it, plus the world it is judged in.
#[domain_model]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OverlayCandidate {
    /// The overlay's identity.
    pub price_overlay_id: Uuid,
    /// The revision being validated.
    pub revision: u64,
    /// Scope class and value.
    pub scope: ScopeSelector,
    /// L2's explicit precedence.
    pub precedence: i32,
    /// The overlay's own interval.
    pub interval: OverlayInterval,
    /// The declared basis. Already past [`check_tax_basis_declared`] by the time
    /// a candidate exists — see the module doc.
    pub tax_basis: TaxBasis,
    /// L6's exposure flag. Carried, defaulted and published; see the module doc
    /// for why no rule below reads it.
    pub disclosure: Disclosure,
    /// The plans the lines may target.
    pub target_ref: TargetRef,
    /// The adjustment lines, whole (D-42).
    pub lines: Vec<OverlayLine>,
    /// The resolved world.
    pub world: OverlayWorld,
}

// ---------------------------------------------------------------------------
// The authoring-edge entry point.
// ---------------------------------------------------------------------------

/// `inst-plv-taxbasis` / L5: the basis MUST be declared, and *silence fails*.
///
/// A **declaration** and not merely a value: [`TaxBasis::DelegatedTariffs`] is
/// an explicit delegation to Tariffs and passes, which is the distinction L5
/// draws — what fails is saying nothing.
///
/// Its own entry point rather than an arm of [`validate`], because `tax_basis`
/// is `NOT NULL` in the store and [`TaxBasis`] is closed, so a stored overlay
/// always has one and the publish-time arm would be unreachable. See the module
/// doc; this is D-213's shape.
///
/// # Errors
/// [`TAX_BASIS_UNDECLARED`] when the request carried none.
pub const fn check_tax_basis_declared(basis: Option<TaxBasis>) -> Result<TaxBasis, &'static str> {
    match basis {
        Some(basis) => Ok(basis),
        None => Err(TAX_BASIS_UNDECLARED),
    }
}

// ---------------------------------------------------------------------------
// The pipeline.
// ---------------------------------------------------------------------------

/// Run every overlay rule, collecting the whole report.
///
/// Aggregate rather than fail-fast, per §4.2: one pass tells the author
/// everything that is wrong, which makes an overlay remediable in one edit
/// instead of in as many edits as it has faults.
#[must_use]
pub fn validate(candidate: &OverlayCandidate) -> ValidationReport {
    let mut report = ValidationReport::default();
    check_scope(candidate, &mut report);
    check_referential(candidate, &mut report);
    check_lines(candidate, &mut report);
    check_eligibility(candidate, &mut report);
    check_adjustment(candidate, &mut report);
    check_precedence(candidate, &mut report);
    check_dating(candidate, &mut report);
    report
}

/// `inst-plv-scope`: every non-`global` value validates against its universe.
///
/// The class/value **pairing** is not checked because it is unrepresentable; the
/// universe lookup is the caller's, and this reports its answer naming the table
/// consulted so an operator knows where to declare the value.
fn check_scope(candidate: &OverlayCandidate, report: &mut ValidationReport) {
    let Some(value) = candidate.scope.value() else {
        // The classless scope has no value and therefore no universe.
        return;
    };
    if candidate.world.scope_value_declared {
        return;
    }
    let class = candidate.scope.class();
    let universe = class
        .taxonomy_table()
        .unwrap_or("a taxonomy this class does not declare");
    report.violate(
        SCOPE_VALUE_UNKNOWN,
        value.as_str(),
        format!(
            "scope value `{value}` is not declared in {universe}; a {class}-scoped overlay \
             selects who receives an adjustment, so its value must exist in the tenant's \
             declared universe (D-120)"
        ),
    );
}

/// `inst-plv-referential`: the overlay's targets must be published, and a
/// retired one dangles rather than blocking (D-31).
fn check_referential(candidate: &OverlayCandidate, report: &mut ValidationReport) {
    for plan in &candidate.target_ref.plans {
        if !candidate.world.published_plans.contains(plan) {
            report.violate(
                TARGET_UNPUBLISHED,
                plan.to_string(),
                format!(
                    "overlay target plan {plan} has not published; an overlay on an unpublished \
                     plan is rejected and never exposed in the read model"
                ),
            );
        }
        if candidate.world.retired_plans.contains(plan) {
            // D-31: dangling-and-flagged. In-flight subscribers legitimately
            // keep resolving a retired plan's rows, so the overlay stays
            // evaluable for them and a retire-time block is exactly the rule
            // D-31 refused.
            report.warn(
                TARGET_RETIRED,
                plan.to_string(),
                format!(
                    "overlay target plan {plan} is retired; the overlay stays evaluable for \
                     in-flight subscribers and is flagged — remediation is to end or retarget it \
                     (D-31)"
                ),
            );
        }
    }
}

/// `inst-plv-lines` (D-42): at least one line, no duplicate key, every non-default
/// line inside the overlay's target scope.
fn check_lines(candidate: &OverlayCandidate, report: &mut ValidationReport) {
    if candidate.lines.is_empty() {
        report.violate(
            OVERLAY_HAS_NO_LINE,
            candidate.price_overlay_id.to_string(),
            "an overlay carries at least one adjustment line: a container of nothing adjusts \
             nothing (D-42)",
        );
        return;
    }

    check_duplicate_keys(&candidate.lines, report);

    for line in &candidate.lines {
        let Some(plan) = line.key.plan_id() else {
            // The list-default line targets every plan of `target_ref` and has
            // nothing of its own to place.
            continue;
        };
        if !candidate.target_ref.covers(plan) {
            report.violate(
                OVERLAY_LINE_TARGET_UNKNOWN,
                render_key(&line.key),
                format!(
                    "line targets plan {plan}, which is outside this overlay's target_ref scope"
                ),
            );
            continue;
        }
        if let Some(sku) = line.key.target_sku()
            && !candidate
                .world
                .published_skus
                .get(&plan)
                .is_some_and(|published| published.contains(sku))
        {
            report.violate(
                OVERLAY_LINE_TARGET_UNKNOWN,
                render_key(&line.key),
                format!("line targets SKU `{sku}`, which plan {plan} does not publish"),
            );
        }
    }
}

/// `inst-plv-eligibility` (D-78): a `cohort` must name a generation the line's
/// target plan actually published.
///
/// The `plan_id` requirement is not checked — a cohort key without one is
/// unconstructible ([`LineKey::for_cohort`]), which is §6's
/// `CHECK (cohort IS NULL OR plan_id IS NOT NULL)` with no `CHECK`.
fn check_eligibility(candidate: &OverlayCandidate, report: &mut ValidationReport) {
    for line in &candidate.lines {
        let (Some(cohort), Some(plan)) = (line.key.cohort(), line.key.plan_id()) else {
            continue;
        };
        let known = candidate
            .world
            .published_cohorts
            .get(&plan)
            .is_some_and(|generations| generations.contains(&cohort));
        if !known {
            report.violate(
                OVERLAY_LINE_COHORT_UNKNOWN,
                render_key(&line.key),
                format!(
                    "line names cohort {} on plan {plan}, which carries no published \
                     existing_grandfathered row at that cutover instant; adjusting a \
                     grandfathered generation takes an explicit line naming its own instant \
                     (D-78)",
                    format_rfc3339(cohort)
                ),
            );
        }
    }
}

/// `inst-plv-adjustment`: D-67's ranges, D-08's per-currency coverage and
/// D-138's replacement warning.
fn check_adjustment(candidate: &OverlayCandidate, report: &mut ValidationReport) {
    for line in &candidate.lines {
        check_magnitude_range(line, report);
        check_currency_coverage(candidate, line, report);
        check_replacement(candidate, line, report);
    }
    // Once per candidate, not per line: the tie is between two **overlays**, and
    // the class order that breaks it is the same for every line either carries.
    check_cross_class_tie(candidate, report);
}

/// **Every rule that needs no world at all** — the authoring edge's entry point.
///
/// Three of `PriceOverlayValidator`'s rules are decidable from the authored
/// document alone: D-67's magnitude ranges, D-42's line-key uniqueness, and
/// §1.7's *"effective-interval sanity"*. They belong at the **save**, because
/// each of them is otherwise enforced only by the store — and a `CHECK` or a
/// unique index reaches the caller as a **driver error**, i.e.
/// `DomainError::Internal` and a 500, for a request whose whole remedy is to
/// correct one field.
///
/// # This was measured three times, not reasoned about once
///
/// Each of the three answered 500 before it was moved here:
/// `discount / percent_bp = 15000` (D-67's own "150% of list" example, refused by
/// `chk_pricing_price_overlay_line_discount_ceiling`); two list-default lines in
/// one overlay (D-42's *"one default line"*, refused by
/// `uq_pricing_price_overlay_line_key`); and `effective_to < effective_from`
/// (refused by `chk_pricing_price_overlay_interval`).
///
/// The line-key case is the sharpest, because the store refusing it at **save**
/// also made `OVERLAY_LINE_DUPLICATE` unreachable at **submit** — a code §5
/// declares, with no path that could raise it.
///
/// The store keeps every one of those guards: they hold against a writer that is
/// not this crate. This entry point is what makes the refusal legible, which is
/// `RepoError::GrandfatherHorizonOffClass`'s argument one plane over — a value
/// arriving *on a request* that a column refuses is a caller mistake the request
/// can be reshaped around, and reaching the constraint makes it an internal
/// fault.
/// # Every violation it raises is stamped `Stage::Write`, and that is a safety
/// property rather than bookkeeping
///
/// This is a **write door**: the surface calls it on the create and on the line-set `PATCH`, and
/// each of its three faults is decidable from the authored document alone — `validation.rs`'s own
/// criterion for the stamp. Building the report with `report.violate(...)` stamps
/// `Stage::Publish` unconditionally, so
/// [`ValidationReport::write_stage_only`](crate::domain::validation::ValidationReport::write_stage_only)
/// answers `None` for it — which makes an ordinary refactor lethal: routing this report through the
/// same filter the plan and price doors use, the obvious move if the two rule planes are ever
/// unified, collapses the whole D-67 / D-42 / §1.7 save-time guard into `Ok(())` with nothing
/// reddening, because `overlay_rules_tests` calls this function directly and never through a stage
/// filter.
#[must_use]
pub fn check_authored_shape(interval: OverlayInterval, lines: &[OverlayLine]) -> ValidationReport {
    let mut report = ValidationReport::default();
    check_interval_sanity(interval, &mut report);
    check_duplicate_keys(lines, &mut report);
    for line in lines {
        check_magnitude_range(line, &mut report);
    }
    report
}

/// §1.7's *"effective-interval sanity"*: an authored interval must be non-empty.
///
/// The one rule of `PriceOverlayValidator` that `validate` cannot hold, and the
/// reason is `OverlayInterval::intersects`: an inverted interval intersects
/// **nothing**, so `check_dating` would silently find no collision for it rather
/// than reporting one. The sanity check has to run before the collision walk can
/// mean anything.
fn check_interval_sanity(interval: OverlayInterval, report: &mut ValidationReport) {
    let (Some(from), Some(to)) = (interval.from, interval.to) else {
        // An open end is not an empty interval; both unbounded arms are legal.
        return;
    };
    if to > from {
        return;
    }
    // `violate_at_write`, not `violate`. Every operand of this fault is in the
    // request — an authored `[from, to)` — so it is judgeable at the door that
    // receives it, which is D-312's criterion exactly. `Stage::Publish` here is not
    // cosmetic: `write_stage_only()` then answers `None` for `check_authored_shape`'s
    // whole report, so routing this door through the same filter the plan and price
    // doors use — the obvious refactor if the two planes are ever unified — deletes
    // the §1.7 save-time guard into `Ok(())` with nothing reddening.
    report.violate_at_write(
        OVERLAY_INTERVAL_INVALID,
        "effective_to",
        format!(
            "the overlay interval [{}, {}) is empty; effective_to must be strictly after \
             effective_from, because nothing is effective over an empty interval",
            format_rfc3339(from),
            format_rfc3339(to)
        ),
    );
}

/// D-42's line-key uniqueness, over a line set alone.
///
/// Shared by [`check_authored_shape`] and [`check_lines`] so the two cannot
/// disagree about what a duplicate is — the null-safe key the store enforces
/// counts `(plan, sku, cohort)` with absence significant, and `LineKey`'s derived
/// `Ord` is that same key.
fn check_duplicate_keys(lines: &[OverlayLine], report: &mut ValidationReport) {
    let mut seen: BTreeSet<&LineKey> = BTreeSet::new();
    for line in lines {
        if !seen.insert(&line.key) {
            // `violate_at_write`: two lines of the submitted set carry one key,
            // which the request states in full. See `check_interval_sanity`.
            report.violate_at_write(
                OVERLAY_LINE_DUPLICATE,
                render_key(&line.key),
                format!(
                    "line key {} appears twice in this revision; one default line, one line per \
                     plan, one per (plan, sku), and one of each per targeted generation (D-42, \
                     D-78)",
                    render_key(&line.key)
                ),
            );
        }
    }
}

// No second entry point here. A `check_magnitudes` over D-67's ranges on a line
// set alone is a strict subset of `check_authored_shape` thirty lines up, which
// runs `check_magnitude_range` over every line as its third step. Two functions
// in one module each documented as *"the authoring edge's entry point"* is the
// shape that sends a reader to the wrong one: a doc link explaining what a door
// does *not* check by pointing at the unreachable one leaves the reader thinking
// the range check lives somewhere it does not run.
//
// Deleted rather than wired: the argument it carried — the store's `CHECK` fires
// on the INSERT and reaches the caller as a 500 for a request whose whole remedy
// is one number, measured rather than reasoned about — is already
// `check_authored_shape`'s, in the same words. Nothing is lost with it.
// `check_magnitude_range` keeps its live call sites.

/// D-67's range over an [`Adjustment`] alone, with no [`OverlayLine`] around it.
///
/// `0 < v <= 10000` on a discount, `v > 0` on a markup, `>= 0` on money.
///
/// Extracted so the **mass-repricing door can ask it**. A run's
/// adjustment is never persisted as an overlay line, so the rule form below could
/// not reach that path and neither could the store's two `CHECK`s — the range went
/// unasked on the repricing plane entirely, and a `discount` of `15000` bp, the
/// very inversion the ceiling clause names, floored every selected row to zero and
/// published without an approval because an absolute materiality bar above the
/// row's own price cannot see that move.
///
/// The `Err` carries the operator-facing clause **without** the line's key, so a
/// caller holding no [`ValidationReport`] can put it straight into a
/// [`DomainError::InvalidRequest`]; the rule form re-attaches the key.
///
/// # Errors
/// The clause naming which bound the magnitude broke.
pub(crate) fn magnitude_out_of_range(adjustment: &Adjustment) -> Result<(), String> {
    if let Some(bp) = adjustment.percent_bp() {
        if bp <= 0 {
            return Err(format!(
                "a {} magnitude of {bp} bp; a magnitude must be strictly positive — a \
                 line that adjusts nothing is a line that should not have been authored, \
                 and the kind carries the direction (D-67)",
                adjustment.kind().as_str()
            ));
        }
        if matches!(adjustment, Adjustment::Discount(_)) && bp > 10_000 {
            return Err(format!(
                "a discount of {bp} bp; a discount above 100% is not authorable, and \
                 {bp} is the shape the \"150% of list\" data-entry inversion takes (D-67)"
            ));
        }
    }
    if let Some(amounts) = adjustment.amounts() {
        for (currency, value) in amounts.iter() {
            if value < 0 {
                return Err(format!(
                    "{value} minor units in {}; an amount magnitude is >= 0 at the \
                     currency's ISO 4217 minor unit (D-67)",
                    currency.as_str()
                ));
            }
        }
    }
    Ok(())
}

/// D-67: `0 < v <= 10000` on a discount, `v > 0` on a markup, `>= 0` on money.
///
/// A pipeline rule and not a constructor refusal, because §5 requires the
/// refusal to **name the line** and a constructor cannot appear in an aggregate
/// report. The store's two `CHECK`s are the backstop; the two guards are in
/// series, which is why `overlay_rules_tests` drives this one directly.
///
/// The predicate itself lives in [`magnitude_out_of_range`], which the repricing
/// door asks too; this function is the line-naming half.
fn check_magnitude_range(line: &OverlayLine, report: &mut ValidationReport) {
    if let Err(clause) = magnitude_out_of_range(&line.adjustment) {
        // `violate_at_write`: a magnitude is in range or it is not, whatever the
        // tenant has published — this rule's own doc calls it the one rule here
        // that needs no world at all, and D-67 says it "fails save **and**
        // publish". See `check_interval_sanity`.
        report.violate_at_write(
            ADJUSTMENT_MAGNITUDE_OUT_OF_RANGE,
            render_key(&line.key),
            format!("line {} carries {clause}", render_key(&line.key)),
        );
    }
}

/// D-08: an amount-based line covers **every** currency its target scope sells.
///
/// A **percent** line is currency-neutral and needs no values at all, which is
/// the whole reason the magnitude's type is declared rather than inferred from
/// the presence of amount rows.
fn check_currency_coverage(
    candidate: &OverlayCandidate,
    line: &OverlayLine,
    report: &mut ValidationReport,
) {
    let Some(amounts) = line.adjustment.amounts() else {
        return;
    };
    for currency in coverage_domain(candidate, &line.key) {
        if amounts.get(&currency).is_none() {
            report.violate(
                ADJUSTMENT_CURRENCY_NOT_COVERED,
                render_key(&line.key),
                format!(
                    "line {} carries no value for {}, which its target scope sells; the catalog \
                     performs no FX, so an absolute magnitude exists only per currency (D-08)",
                    render_key(&line.key),
                    currency.as_str()
                ),
            );
        }
    }
}

/// The currencies one line must cover.
///
/// A per-plan or per-`(plan, sku)` line covers its own plan's markets; the
/// **list-default** line applies to every target of the overlay, so its domain
/// is the union over `target_ref`.
fn coverage_domain(candidate: &OverlayCandidate, key: &LineKey) -> BTreeSet<CurrencyCode> {
    let plans: Vec<PlanId> = key
        .plan_id()
        .map_or_else(|| candidate.target_ref.plans.clone(), |plan| vec![plan]);
    plans
        .into_iter()
        .filter_map(|plan| candidate.world.sold_currencies.get(&plan))
        .flat_map(|set| set.iter().cloned())
        .collect()
}

/// D-138: a `fixed` line that is not the lowest matching layer voids the layers
/// below it, and authoring warns.
///
/// **A warning, never a refusal** — voiding a lower layer is a legitimate act
/// and the operator may well intend it; what they must not do is intend it by
/// accident. Read the module doc on the sort direction before trusting the
/// condition.
fn check_replacement(
    candidate: &OverlayCandidate,
    line: &OverlayLine,
    report: &mut ValidationReport,
) {
    if !line.adjustment.replaces() {
        return;
    }
    let targets: Vec<PlanId> = line
        .key
        .plan_id()
        .map_or_else(|| candidate.target_ref.plans.clone(), |plan| vec![plan]);
    let discarded: Vec<String> = targets
        .into_iter()
        .filter(|plan| candidate.world.layers_beneath.contains(plan))
        .map(|plan| plan.to_string())
        .collect();
    if discarded.is_empty() {
        return;
    }
    report.warn(
        FIXED_LINE_DISCARDS_STACK,
        render_key(&line.key),
        format!(
            "line {} is a fixed replacement at precedence {} and is not the lowest-precedence \
             layer able to match {}; a replacement discards every layer beneath it (D-138)",
            render_key(&line.key),
            candidate.precedence,
            discarded.join(", ")
        ),
    );
}

/// `inst-plv-class-tiebreak` (D-230): the operator sees a cross-class tie before
/// relying on the break.
///
/// One warning per tying overlay rather than per plan: the operator's question is
/// *which overlay ties with mine*, and a plan list inside one message answers it
/// where one message per plan would bury it.
///
/// Nothing is refused. `precedence` is unique only **within** a class, so the tie
/// is legal, and the class order breaks it deterministically — the warning exists
/// because an author reading two overlays at "the same precedence" has no reason
/// to expect one to be beneath the other.
fn check_cross_class_tie(candidate: &OverlayCandidate, report: &mut ValidationReport) {
    for tie in &candidate.world.cross_class_ties {
        if tie.plans.is_empty() {
            continue;
        }
        let plans: Vec<String> = tie.plans.iter().map(ToString::to_string).collect();
        let (beneath, above) = if tie.class < candidate.scope.class() {
            (tie.class, candidate.scope.class())
        } else {
            (candidate.scope.class(), tie.class)
        };
        report.warn(
            EQUAL_PRECEDENCE_CROSS_CLASS_TIE,
            tie.price_overlay_id.to_string(),
            format!(
                "overlay {} holds precedence {} in class {} and reaches {}; the class order \
                 decides the stack position, putting {} beneath {} (inst-plv-class-tiebreak). \
                 The tie is legal and the break deterministic — this says so before you rely \
                 on it",
                tie.price_overlay_id,
                candidate.precedence,
                tie.class.as_str(),
                plans.join(", "),
                beneath.as_str(),
                above.as_str(),
            ),
        );
    }
}

/// `inst-plv-precedence` (L2): unique within one scope class, among published
/// revisions.
///
/// The refusal names the overlay holding the slot, because the operator's next
/// action is a real one — go and look at that overlay — and it is unreachable
/// from a refusal that does not say which one is in the way.
fn check_precedence(candidate: &OverlayCandidate, report: &mut ValidationReport) {
    let Some(holder) = candidate.world.precedence_holder else {
        return;
    };
    report.violate(
        PRECEDENCE_DUPLICATE,
        candidate.precedence.to_string(),
        format!(
            "precedence {} is already held by published overlay {holder} in scope class {}; \
             precedence is unique within a class, and a tie across classes is broken by the \
             published class order instead",
            candidate.precedence,
            candidate.scope.class()
        ),
    );
}

/// `inst-plv-dating` (D-107): overlapping intervals collide **per line key**, over
/// published revisions of **other** overlays only.
///
/// The two exclusions are the rule rather than optimisations. Restricting the
/// domain to published revisions is what lets a draft exist at all; restricting
/// it to *other* overlays is D-107 — since D-92 gave overlays draft-revision
/// rows, an unscoped check matched the overlay's own published revision and
/// rejected every edit of a live overlay.
fn check_dating(candidate: &OverlayCandidate, report: &mut ValidationReport) {
    for line in &candidate.lines {
        for holder in &candidate.world.interval_holders {
            if holder.price_overlay_id == candidate.price_overlay_id {
                // D-107: never against another revision of the same overlay.
                continue;
            }
            if holder.scope != candidate.scope || holder.key != line.key {
                continue;
            }
            if !holder.interval.intersects(&candidate.interval) {
                continue;
            }
            report.violate(
                OVERLAY_INTERVAL_OVERLAP,
                render_key(&line.key),
                format!(
                    "line {} on scope {}/{} overlaps the interval published overlay {} holds on \
                     the same line key",
                    render_key(&line.key),
                    candidate.scope.class(),
                    candidate.scope.stored_value(),
                    holder.price_overlay_id
                ),
            );
        }
    }
}

/// The first violation in `report` that §5 types **409** rather than as an
/// architectural 422, if there is one.
///
/// §5 types seven of the nine overlay codes 422 — which this platform renders
/// 400 with the code as the discriminator — and **two of them 409 outright**:
/// [`PRECEDENCE_DUPLICATE`] and [`OVERLAY_INTERVAL_OVERLAP`]. The difference is
/// not cosmetic. The seven tell a caller their own request is wrong and name a
/// field to fix; these two tell them a **sibling overlay** they never named holds
/// something, and re-reading is the whole remedy — which is what a 409 asks for
/// and what a 400 would send them looking for a field to correct.
///
/// So both halves are true at once: the rules raise these codes into the
/// one-pass report an author remediates from, and the surface lifts them back
/// out to answer the status the design set assigns. Splitting them at the rule
/// instead would have kept them out of the report altogether, and an author
/// fixing four things would then discover the fifth on the next round trip.
///
/// The first rather than all of them, because a conflict is a single fact about
/// the world: a caller told two sibling overlays are in the way acts on one of
/// them and re-reads.
///
/// # A conflict truncates the report, and that is the residue
///
/// Both call sites (`api::rest::overlays`'s submit and `infra::overlay_publish`'s
/// commit) return the lifted violation as a `DomainError` and drop the rest of
/// the [`ValidationReport`] on the floor. So the one-pass property argued above
/// holds for a candidate whose only faults are 422s, and **not** for one carrying
/// a conflict *and* other faults: an author with a precedence duplicate plus a
/// `SCOPE_VALUE_UNKNOWN` and an `OVERLAY_LINE_DUPLICATE` is told about the
/// conflict alone and discovers the other two on the next round trip — the exact
/// cost the paragraph above says the arrangement avoids, in the one case this
/// function exists for.
///
/// Closing it means carrying the whole report alongside the lifted status, which
/// is a change to what the two surfaces put in the 409 body rather than a change
/// here; it is stated rather than fixed so that no reader takes the one-pass
/// property to be total.
#[must_use]
pub fn conflict_of(report: &ValidationReport) -> Option<&crate::domain::validation::Violation> {
    report
        .violations
        .iter()
        .find(|v| v.code == PRECEDENCE_DUPLICATE || v.code == OVERLAY_INTERVAL_OVERLAP)
}

/// A line key as an operator reads it.
///
/// The rendering carries all three components even when two are absent, because
/// `OVERLAY_LINE_DUPLICATE` on `default` and on `plan=… sku=…` are different
/// facts and a refusal naming only what is present cannot say which.
fn render_key(key: &LineKey) -> String {
    let plan = key
        .plan_id()
        .map_or_else(|| "default".to_owned(), |plan| plan.to_string());
    let sku = key
        .target_sku()
        .map_or_else(|| "-".to_owned(), TargetSku::to_string);
    let cohort = key.cohort().map_or_else(|| "-".to_owned(), format_rfc3339);
    format!("plan={plan} sku={sku} cohort={cohort}")
}

#[cfg(test)]
#[path = "overlay_rules_tests.rs"]
mod overlay_rules_tests;
