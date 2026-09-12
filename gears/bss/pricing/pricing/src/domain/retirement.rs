//! Retirement's compose-time judgement (`inst-rt-cancel`, `inst-re-cancelflow`,
//! `inst-re-references`, D-51, D-131, D-182).
//!
//! Retirement stops **selling**, never **rating**. Everything in this module
//! follows from that one sentence: not-sellable comes from the lifecycle
//! predicate of the sellability gate (`domain::sellability` predicate (4)), so
//! nothing here has to cancel a window in order to stop a sale, and every window
//! it *does* cancel it cancels for a different reason — an unoccupied key's
//! scheduled window is coverage nobody is going to use.
//!
//! # The presence question is asked once, and answered per price id
//!
//! D-131. Retirement submits the **union** of its keys' price-id sets in a single
//! call on the D-79 in-flight-subscription lane and reads a **per-price-id
//! presence map** back. The two alternatives it was chosen over are both wrong in
//! ways worth keeping written down: a lane that answers a *count* over the
//! submitted set answers only "does this plan have any subscriber at all", under
//! which retirement cancels nothing whenever a single key is occupied; and a
//! per-key call puts N synchronous cross-gear round trips (N = keys × markets)
//! inside the retirement transaction, holding the price and window row locks and
//! the audit chain segment across the fan-out.
//!
//! # The lane is absent, so every key reads occupied
//!
//! D-182, normative. The D-79 lane has no client, no contract type and no
//! counterpart gear in the built system. D-131's outage clause is fail-closed —
//! on lane outage or timeout, **windows are kept** — and D-182 makes an absent
//! lane that same case. So in the system as built [`PresenceMap::fail_closed`] is
//! the only value this module is ever handed, every key reads occupied, and
//! retirement keeps every scheduled window rather than cancelling on the keys the
//! design set would have let it cancel on.
//!
//! That is not a stub standing in for the rule: it *is* the rule, evaluated
//! against the world as it is. [`dispose_windows`] is written against the map and
//! not against its absence, so the day the lane client lands a
//! [`PresenceMap::resolved`] flows through the same code and the presence half of
//! `inst-rt-cancel` becomes what that step describes.
//!
//! **The presence predicate is not the only thing that decides a scheduled
//! window's fate** (D-316), and reading it as though it were is the error three
//! surfaces have carried at once — here, [`crate::infra::retirement`]'s call site
//! and `inst-rt-cancel`'s own normative prose. D-04's `inst-co-bounds` binds a
//! grandfathered generation's coverage across a span, retirement's cancellations
//! reach the window store without passing `infra::window`'s enforcement of it, and
//! a generation reading *unoccupied* is precisely the case the presence lane waves
//! through. [`strand_free_disposition`] is the edit that answers it.
//!
//! # Two questions, two populations, and neither subsumes the other
//!
//! `dispose_windows` asks *who is bound to this row now*. The bound asks *what
//! must this generation still cover*. They are asked in that order and the second
//! only ever moves a verdict from cancelled back to kept, so the presence lane
//! keeps its full authority to **keep** and loses only its authority to cancel
//! something D-04 has spoken for.
//!
//! # Kept is not the same fact as cancelled, and the operator sees both
//!
//! `inst-re-cancelflow` requires the confirm screen to label kept windows
//! distinctly from cancelled ones, which is why [`WindowDisposition`] carries a
//! reason rather than a bool: an operator who is shown "3 windows" cannot tell a
//! plan whose coverage is being preserved from one whose coverage is being torn
//! down.

use std::collections::BTreeSet;

use toolkit_macros::domain_model;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::scope_key::ScopeKey;
use crate::domain::window::{KeyWindows, WindowInterval, WindowState};
use time::OffsetDateTime;

/// Wire code for a retirement refused by a blocking reference
/// (`11-lifecycle.md` §5, `inst-re-references`, 409).
pub const RETIRE_PLAN_REFERENCED: &str = "RETIRE_PLAN_REFERENCED";

/// Wire code for a retirement refused by a live cutover standing over the plan
/// (409).
///
/// **The design set does not declare it, and the entry is owed** — stated here
/// rather than left for a reader to discover, `PricingAlarm`'s two undeclared
/// variants' arrangement. What the set declares about this collision is the
/// *unwind* (`inst-co-retirement-unwind`, `inst-rt-cancel`, D-05): the retirement
/// restores the predecessor's bound, cancels the copy and the successor and closes
/// the unit. `infra::retirement::ensure_no_live_cutover` carries the account of why
/// that act is a slice of its own and refuses meanwhile, and a refusal the set never
/// contemplated has no code in it. Until the entry exists an operator greps
/// `11-lifecycle.md` for this string and finds nothing, which is the cost; the
/// alternative is `LIFECYCLE_FORBIDDEN` (400) telling them their request was
/// malformed when the remedy is to wait, so the sibling refusals'
/// discriminator is the smaller loss.
///
/// Spelled to the convention its two siblings use — [`RETIRE_PLAN_REFERENCED`] and
/// `migration::RETIRE_TARGET_OF_MIGRATION` — so the day the entry lands it lands on
/// this name.
pub const RETIRE_OVER_LIVE_CUTOVER: &str = "RETIRE_OVER_LIVE_CUTOVER";

/// Which price ids carry in-flight subscribers (D-51's predicate, D-131's shape).
///
/// Built from **one** lane call over the union of the retiring plan's price ids.
/// The absent-lane and outage cases collapse into [`PresenceMap::fail_closed`],
/// because D-131 gives them the same answer and D-182 makes the first of them the
/// only one this system reaches.
#[domain_model]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PresenceMap {
    /// The price ids the lane reported in-flight subscribers on. Meaningless
    /// when `fail_closed` is set, and empty there so a reader that ignores the
    /// flag still cannot conclude "unoccupied" from a lookup.
    occupied: BTreeSet<Uuid>,
    /// The lane could not answer. Every lookup then reads occupied.
    fail_closed: bool,
}

impl PresenceMap {
    /// The lane answered: exactly `occupied` carry in-flight subscribers.
    #[must_use]
    pub fn resolved(occupied: impl IntoIterator<Item = Uuid>) -> Self {
        Self {
            occupied: occupied.into_iter().collect(),
            fail_closed: false,
        }
    }

    /// The lane is absent, out, or timed out (D-131's fail-closed clause; D-182
    /// makes it the built system's only case).
    #[must_use]
    pub fn fail_closed() -> Self {
        Self {
            occupied: BTreeSet::new(),
            fail_closed: true,
        }
    }

    /// Does this price id carry in-flight subscribers?
    ///
    /// `true` for **every** id when the map failed closed — which is the whole
    /// of the fail-closed posture, stated once here rather than at each caller.
    #[must_use]
    pub fn is_occupied(&self, price_id: Uuid) -> bool {
        self.fail_closed || self.occupied.contains(&price_id)
    }

    /// Did the lane answer at all?
    ///
    /// The dry-run preview reports this, because "kept because the lane could not
    /// tell us" and "kept because a subscriber is on it" are different facts for
    /// the operator confirming, and `inst-re-warn` puts the list in front of them.
    #[must_use]
    pub const fn is_fail_closed(&self) -> bool {
        self.fail_closed
    }
}

/// A scheduled window retirement has to decide about.
///
/// Only **not-yet-active** windows are candidates: an active window runs to its
/// natural end for the in-flight subscribers this slice preserves coverage for,
/// so it is never a member of the input set at all.
#[domain_model]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScheduledWindow {
    /// The window row.
    pub window_id: Uuid,
    /// The price row it schedules — the presence map's key (D-131).
    pub price_id: Uuid,
}

/// Why a scheduled window is being kept.
#[domain_model]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeptReason {
    /// The key has in-flight subscribers, so the window is their continuing
    /// coverage (D-51). Cancelling it opens the trailing void no gap check can
    /// see: the active window expires at its natural end (`inst-ws-expire`) and
    /// every arrears charge and renewal after it fails closed.
    InFlightSubscribers,
    /// The lane could not be asked, so the key reads occupied (D-131's
    /// fail-closed clause, D-182's absent lane).
    PresenceUnresolved,
    /// Cancelling it would open a void inside a grandfathered generation's D-04
    /// span, so the window stands whatever the presence lane says (D-316's
    /// residual).
    ///
    /// The presence lane and this bound answer **different populations**, which is
    /// why one does not subsume the other. The lane is asked per **price id** and
    /// reports who is bound to a row *now*; D-04's bound is about a generation's
    /// coverage having to outlast every bound period that started before its
    /// horizon, which is a claim about the whole span `[max(cohort, now),
    /// grandfatherUntil + the margin)` rather than about the instant the question
    /// was asked. A generation reading unoccupied at retirement time and holding
    /// the sole window covering that span is exactly the shape D-04 exists to
    /// protect, and D-51's predicate cannot see it.
    GrandfatheredCoverageBound,
}

/// What retirement will do to one scheduled window.
#[domain_model]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowDisposition {
    /// The key has no in-flight subscribers: Slice 7's cancellation flow is
    /// invoked on it (`inst-re-cancelflow` — invoked, never marked invalid, so
    /// `PriceWindowCancelled` is emitted and the consumer caches evict).
    Cancelled,
    /// The window stands, for this reason.
    Kept(KeptReason),
}

/// One window and what retirement will do to it.
#[domain_model]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WindowVerdict {
    /// The window row.
    pub window_id: Uuid,
    /// The price row it schedules.
    pub price_id: Uuid,
    /// Cancelled, or kept and why.
    pub disposition: WindowDisposition,
}

impl WindowVerdict {
    /// Is this window one the cancellation flow will be invoked on?
    #[must_use]
    pub const fn is_cancelled(&self) -> bool {
        matches!(self.disposition, WindowDisposition::Cancelled)
    }
}

/// Decide every scheduled window against one presence map (D-51, D-131).
///
/// The map is asked per **price id**, which is what makes a plan with mixed keys
/// — some occupied, some not — cancel exactly the unoccupied ones. A union count
/// could not distinguish them, and that is D-131's whole argument.
#[must_use]
pub fn dispose_windows(
    scheduled: &[ScheduledWindow],
    presence: &PresenceMap,
) -> Vec<WindowVerdict> {
    scheduled
        .iter()
        .map(|w| WindowVerdict {
            window_id: w.window_id,
            price_id: w.price_id,
            disposition: if presence.is_occupied(w.price_id) {
                WindowDisposition::Kept(if presence.is_fail_closed() {
                    KeptReason::PresenceUnresolved
                } else {
                    KeptReason::InFlightSubscribers
                })
            } else {
                WindowDisposition::Cancelled
            },
        })
        .collect()
}

/// The interval a generation is answerable for, or the fact that it has none.
///
/// A named pair rather than a `Result<_, ()>`: the error arm carries no
/// information and the success arm is two instants whose meaning is not
/// recoverable from their types — which is the shape `CoverageEnd` is three arms
/// instead of an `Option` for, one type over.
enum BoundSpan {
    /// Walk from `anchor` and report the first uncovered instant before `until`.
    /// A `None` `until` is D-147's indefinite generation: nothing short of an
    /// unbroken run into an open interval answers it.
    Walk {
        anchor: OffsetDateTime,
        until: Option<OffsetDateTime>,
    },
    /// The floor cannot be computed at all, so no interval set can be shown to
    /// reach it. Two causes, one arm: W6's term has **no value** on this key, or
    /// `grandfather_until + margin` is not a representable instant. Both are the
    /// absence of an operand rather than a bound that was tested and failed, and
    /// the caller's answer to either is the same — keep.
    Incomputable,
}

/// One grandfathered generation's coverage, as retirement finds it
/// (D-04, D-316's residual).
///
/// Built by [`crate::infra::retirement`] for **every grandfathered key of the
/// retiring plan** — one per key, because ADR-0002's cohort axis makes each
/// generation a key of its own and each carries its own horizon. A key that is
/// not `existing_grandfathered` has no bound to be judged against and no
/// instance here.
#[domain_model]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GenerationCoverage {
    /// The generation's key. Its `cohort` axis is D-316 clause (3)'s lower
    /// anchor, and its `price_eligibility` is what selected it.
    pub scope_key: ScopeKey,
    /// Every window on this key **as stored**, paired with the id that names it.
    ///
    /// The whole set and not the condemned subset: the bound is a question about
    /// the key's coverage, so the windows retirement is *not* touching are what
    /// decide whether the ones it is touching can go.
    pub windows: Vec<(Uuid, WindowInterval)>,
    /// D-04's horizon, off the key's published row. `None` is D-147's indefinite
    /// generation, whose coverage no finite end satisfies.
    pub grandfather_until: Option<OffsetDateTime>,
    /// W6's margin on the key's market. `None` when the term has **no value** —
    /// the market sells recurring and the plan authored no frequency — which is
    /// not `Some(zero)` and is refused rather than read as no margin.
    pub margin: Option<time::Duration>,
}

impl GenerationCoverage {
    /// The span this generation is answerable for, as
    /// [`KeyWindows::first_uncovered_from`]'s two arguments.
    ///
    /// `Err` when the margin has no value: the floor cannot be computed at all,
    /// so no interval set can be *shown* to reach it and the caller keeps every
    /// window rather than guessing. That is `refuse_horizon_uncovered`'s reading
    /// of the same `None`, in the direction this surface refuses in — it keeps
    /// coverage where the window surface refuses the act.
    ///
    /// **And `Err` for the same reason when `horizon + margin` is not a
    /// representable instant.** `grandfather_until` is an operator-authored stored
    /// instant that `infra::storage::repo::check_authored_instant` bounds only for
    /// millisecond precision and never for range, and chrono's
    /// `impl Add<time::Duration> for DateTime<Tz>` is `checked_add_signed(..).expect(..)`
    /// — so this was an `expect` on request-derived data that aborts a **release**
    /// build too, reachable from `PLAN_RETIRE` through `preview` and through the
    /// commit arm. `sellability::active_window_with_horizon` does the identical add
    /// fallibly with the hazard documented; this now matches it, and folds the
    /// unrepresentable horizon into the arm the caller already reads as
    /// keep-the-window.
    fn span(&self, now: OffsetDateTime) -> BoundSpan {
        // D-316 clause (3): the cohort, raised by `now`. A generation cannot
        // strand anybody before it exists, and a void strictly in the past is
        // repairable by no act at all.
        let anchor = self
            .scope_key
            .cohort()
            .generation()
            .map_or(now, |cutover| cutover.max(now));
        match self.grandfather_until {
            // D-147: an indefinite generation's coverage must never end, which
            // `None` spells as the walk's own argument.
            None => BoundSpan::Walk {
                anchor,
                until: None,
            },
            Some(horizon) => self.margin.map_or(BoundSpan::Incomputable, |m| {
                horizon
                    .checked_add(m)
                    .map_or(BoundSpan::Incomputable, |until| BoundSpan::Walk {
                        anchor,
                        until: Some(until),
                    })
            }),
        }
    }

    /// The key's interval set, with `cancelled` written onto every id in
    /// `condemned`.
    ///
    /// Sorted, because [`KeyWindows::first_uncovered_from`] walks in
    /// `effective_from` order and the type's promise is the caller's to keep —
    /// the same restatement `infra::window`'s `project` makes.
    fn projected(&self, condemned: &BTreeSet<Uuid>) -> KeyWindows {
        let mut intervals: Vec<WindowInterval> = self
            .windows
            .iter()
            .map(|(id, interval)| {
                if condemned.contains(id) {
                    WindowInterval::new(
                        interval.effective_from,
                        interval.effective_to,
                        WindowState::Cancelled,
                    )
                } else {
                    *interval
                }
            })
            .collect();
        intervals.sort_by_key(|i| (i.effective_from, i.effective_to));
        KeyWindows {
            scope_key: self.scope_key.clone(),
            intervals,
        }
    }
}

/// Re-keep every condemned window whose cancellation would strand a
/// grandfathered generation (D-04's `inst-co-bounds`, reached from retirement).
///
/// # It keeps rather than refuses, and that is the decision
///
/// `infra::window::refuse_horizon_uncovered` answers the same bound with a
/// **refusal**, because there the act *is* the window mutation and refusing it
/// leaves the operator holding an act they can re-author. Here the act is the
/// retirement of a plan, and the cancellations are a **consequence** of it. A
/// refusal would make a plan carrying a grandfathered generation unretirable for
/// as long as its horizon runs — years, on a typical grandfathering term — with
/// no remedy available to the operator at all, since the windows that would
/// satisfy the bound are the very ones the act wants gone. Keeping costs nothing
/// that retirement is for: **retirement stops selling, never rating**, and
/// not-sellable comes from the lifecycle flip rather than from any window. So the
/// plan retires, sales stop, and the generation keeps the coverage D-04 promised
/// it — which is D-51's own composition applied to a second protected population.
///
/// It also mints **no wire code**, which D-204 clause (2) would refuse: the
/// outcome is a `kept` window with a reason beside the two `inst-re-cancelflow`
/// already renders, not a fifth window refusal.
///
/// # It keeps only what the cancellation itself would break
///
/// Two walks, not one. A generation whose span is **already** broken — a leading
/// void strictly in the past, which D-316 clause (3) deliberately accepts and
/// this rule must not start refusing — gains nothing from keeping its windows,
/// and a rule that kept them would freeze exactly the keys D-316 argued must stay
/// mutable. So the condemned set is re-kept only when the void opens **after**
/// the cancellations and did not exist before them.
///
/// # The whole key's condemned set moves together
///
/// A void is a fact about the key's interval set, not about one window, so
/// attributing it to a single member and cancelling the rest would be a
/// minimisation this bound never asked for — and one that can still leave a void,
/// since two condemned windows may each be load-bearing only in the other's
/// absence. Per key, all or none.
pub fn strand_free_disposition(
    verdicts: &mut [WindowVerdict],
    generations: &[GenerationCoverage],
    now: OffsetDateTime,
) {
    for generation in generations {
        let on_key: BTreeSet<Uuid> = generation.windows.iter().map(|(id, _)| *id).collect();
        let condemned: BTreeSet<Uuid> = verdicts
            .iter()
            .filter(|v| v.is_cancelled() && on_key.contains(&v.window_id))
            .map(|v| v.window_id)
            .collect();
        if condemned.is_empty() {
            continue;
        }
        let BoundSpan::Walk { anchor, until } = generation.span(now) else {
            // The floor has no value — either the margin does not, or the horizon
            // plus the margin is not a representable instant — so nothing can be
            // shown to reach it. Keep, for `refuse_horizon_uncovered`'s reason at
            // full strength.
            re_keep(verdicts, &condemned);
            continue;
        };
        let broken_after = generation
            .projected(&condemned)
            .first_uncovered_from(anchor, until)
            .is_some();
        let broken_before = generation
            .projected(&BTreeSet::new())
            .first_uncovered_from(anchor, until)
            .is_some();
        if broken_after && !broken_before {
            re_keep(verdicts, &condemned);
        }
    }
}

/// Flip the named verdicts from cancelled to kept, on this bound's reason.
fn re_keep(verdicts: &mut [WindowVerdict], condemned: &BTreeSet<Uuid>) {
    for verdict in verdicts
        .iter_mut()
        .filter(|v| condemned.contains(&v.window_id))
    {
        verdict.disposition = WindowDisposition::Kept(KeptReason::GrandfatheredCoverageBound);
    }
}

/// A reference to the retiring plan that **blocks** the retirement
/// (`inst-re-references`).
#[domain_model]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockingReferenceKind {
    /// The plan is a component of a bundle (`sum_of_parts` / `own_price`
    /// composition, Slice 8). Remediation is to re-compose the bundle or retire
    /// it first.
    BundleComponent,
    /// The plan is the target of an add-on price override (Slice 2).
    AddOnPriceOverrideTarget,
}

/// A reference that is **enumerated and does not block**.
#[domain_model]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WarningReferenceKind {
    /// Another plan lists the retiree in `allowedChangeTargets` (D-24). The edge
    /// goes inert rather than dangling: Subscriptions re-checks the target's
    /// lifecycle state at change time, so blocking here would refuse a retirement
    /// on the strength of an edge nobody can traverse anyway.
    AllowedChangeTarget,
    /// A `PriceOverlay` targets the retiree (D-31). It goes dangling-and-flagged
    /// (`pricing.priceoverlay.target_retired`) and stays evaluable for in-flight
    /// subscribers, which is the same reason the windows above are kept.
    PriceOverlayTarget,
}

/// One reference to the retiring plan, of either weight.
#[domain_model]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanReference {
    /// The bundle, plan or overlay that refers to the retiree.
    pub referrer_id: Uuid,
    /// A caller-facing name for the referrer, so the dry-run enumerates something
    /// an operator can act on rather than a bare id.
    pub referrer_label: String,
}

/// What refers to the plan, blocking and otherwise (`inst-re-references`).
#[domain_model]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReferenceReport {
    /// Blocking references, by kind.
    pub blocking: Vec<(BlockingReferenceKind, PlanReference)>,
    /// Enumerated-only references, by kind.
    pub warnings: Vec<(WarningReferenceKind, PlanReference)>,
}

impl ReferenceReport {
    /// Refuse the retirement if anything blocking refers to the plan.
    ///
    /// The refusal enumerates the referrers, because "this plan is referenced" is
    /// not an actionable sentence: the remediation is to re-compose or retire a
    /// *named* referrer first.
    ///
    /// # Errors
    /// [`DomainError::RetirePlanReferenced`] listing every blocking referrer.
    pub fn ensure_retirable(&self, plan_id: Uuid) -> Result<(), DomainError> {
        if self.blocking.is_empty() {
            return Ok(());
        }
        let references = self
            .blocking
            .iter()
            .map(|(kind, r)| format!("{} {} ({})", kind.as_str(), r.referrer_label, r.referrer_id))
            .collect::<Vec<_>>()
            .join(", ");
        Err(DomainError::RetirePlanReferenced(format!(
            "plan {plan_id} cannot be retired while it is referenced by: {references}; re-compose \
             or retire the referrer first"
        )))
    }
}

impl BlockingReferenceKind {
    /// The token the refusal and the dry-run render.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::BundleComponent => "bundle component",
            Self::AddOnPriceOverrideTarget => "add-on price-override target",
        }
    }
}

impl WarningReferenceKind {
    /// The token the dry-run renders.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AllowedChangeTarget => "allowed change target",
            Self::PriceOverlayTarget => "price overlay target",
        }
    }
}

#[cfg(test)]
#[path = "retirement_tests.rs"]
mod retirement_tests;
