//! The supersession unit's own domain rules — the ones that are preconditions
//! rather than pipeline rules.
//!
//! The unit's **content** rule lives elsewhere and is a
//! [`ValidationRule`](crate::domain::validation::ValidationRule):
//! [`SupersessionPair`](crate::domain::rules::SupersessionPair) is the D-82/D-98/
//! D-122/D-127/D-129 unit guard, and it belongs in the rule set because it judges
//! a pair of rows and reports violations alongside every other row rule. What is
//! here judges neither rows nor shape — it judges the **changeover instant**, and
//! it is asked twice about the same value at two different moments.
//!
//! # Why the floor is two floors (`inst-su-instant`)
//!
//! The instant MUST be strictly future **at submit** and at least the max
//! batching-delay SLO in the future **at approval commit**. Those are not two
//! spellings of one rule and neither implies the other in the direction that
//! matters:
//!
//! - Holding a **submit** to the commit floor would refuse a unit that is going to
//!   be perfectly legal by the time a second person approves it. A submit is a
//!   proposal; the batching delay has not started running against it.
//! - Holding a **commit** to the submit floor is the money defect the rule exists
//!   for. `CatalogVersion` assignment batches (D-47: p95 ≤ 60s, max 5 min), so an
//!   instant inside that lag activates the successor's window while its row is not
//!   yet addressable at any *completed* version — renewals and arrears fail closed
//!   for up to the whole delay, multiplied across every key of a repricing run.
//!
//! The design set gives the second refusal a remedy rather than a retry: the unit
//! is **recomposed** against a fresh instant. That word is in the message on
//! purpose — a caller told only "422" would resubmit the same instant, which is
//! refused identically and further behind the floor each time.
//!
//! # The delay is a constant here and a knob in `config`, deliberately
//!
//! [`MAX_BATCHING_DELAY`] is 5 minutes, D-47's ratified maximum, and it is **not**
//! read from [`JobsConfig`](crate::config::JobsConfig) even though
//! `catalog_version_overdue_secs` defaults to the same 300 seconds from the same
//! decision. The two are the same number with different natures: that field is an
//! **alarm threshold**, an ops question about how long to wait before shouting, and
//! lowering it costs an operator some noise. This is a **correctness floor**, and
//! lowering it buys a tenant transiently unrateable subscriptions. A floor that a
//! configuration file can move is not a floor.
//!
//! `inst-gc-compose` holds the cutover's changeover to the same bound. When that
//! unit is built it takes this constant, not a copy of the number — a
//! hand-maintained second copy is how the two mechanisms come to disagree about
//! the same SLO.

use toolkit_macros::domain_model;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::instant::format_rfc3339;
use crate::domain::price_row::PriceRow;
use crate::domain::rules::SupersessionPair;
use crate::domain::window::{OCCUPYING_STATES, WindowInterval, WindowState};
use time::Duration;
use time::OffsetDateTime;

/// The wire code §5 declares for a changeover instant that has fallen behind its
/// floor (architectural 422, rendered 400 — Foundation §3.3).
pub const SUPERSESSION_INSTANT_PASSED: &str = "SUPERSESSION_INSTANT_PASSED";

/// D-47's ratified **maximum** `CatalogVersion` batching delay, and the distance
/// `inst-su-instant` requires a changeover to clear at approval commit.
///
/// The p95 is 60s and this is the max; the floor takes the max because the floor's
/// job is to be right for the slowest batch rather than the median one.
pub const MAX_BATCHING_DELAY: Duration = Duration::minutes(5);

/// Which of `inst-su-instant`'s two floors a changeover instant is being held to.
///
/// An enum rather than two functions, because the two questions differ **only** in
/// the bound and a caller choosing between two similarly-named functions is a
/// caller who can pick the lenient one at the strict moment. Here the moment is
/// the argument and the bound is derived from it.
#[domain_model]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ChangeoverMoment {
    /// Submit: the instant must be strictly in the future.
    Submit,
    /// Approval commit: the instant must additionally clear the whole batching
    /// delay.
    Commit,
}

impl ChangeoverMoment {
    /// How far ahead of `now` this moment requires the instant to be.
    ///
    /// `Submit`'s bound is zero and the comparison below is strict, which is what makes
    /// "strictly future" and "one delay ahead" one expression instead of two branches that
    /// could drift apart.
    ///
    /// **The strict comparison makes the commit arm one quantum stricter than
    /// `inst-su-instant`'s "at least the max batching-delay SLO"**, and that is deliberate
    /// rather than a reading of the spec: the direction is fail-safe, an instant exactly
    /// one delay out is one the last batch of that window can still miss, and
    /// `the_commit_floor_is_exclusive_at_exactly_the_batching_delay` pins the stricter
    /// reading on purpose. Said plainly here because the spec says "at least" and a reader
    /// checking the code against it should find the divergence named.
    const fn margin(self) -> Duration {
        match self {
            Self::Submit => Duration::ZERO,
            Self::Commit => MAX_BATCHING_DELAY,
        }
    }

    /// What the refusal tells the operator to do about it.
    ///
    /// The commit arm **formats [`MAX_BATCHING_DELAY`]** rather than restating it. It used
    /// to spell "300s (5 min)" by hand, which is precisely the hand-maintained second copy
    /// this module's own header argues is how two mechanisms come to disagree about one
    /// SLO — and the case guarding the sentence used an `||` over both spellings, so
    /// changing the constant would leave the message lying and the suite green.
    fn remedy(self) -> String {
        match self {
            Self::Submit => "name a future changeover instant".to_owned(),
            Self::Commit => format!(
                "the unit must be recomposed against a changeover at least {}s ahead",
                MAX_BATCHING_DELAY.whole_seconds()
            ),
        }
    }
}

/// Is `changeover` far enough ahead of `now` for `moment`?
///
/// # Errors
/// [`DomainError::SupersessionInstantPassed`] naming the instant, the floor it
/// missed and the remedy for that moment.
pub fn check_changeover_instant(
    changeover: OffsetDateTime,
    now: OffsetDateTime,
    moment: ChangeoverMoment,
) -> Result<(), DomainError> {
    changeover_floor(changeover, now, moment, "changeover")
        .map_err(DomainError::SupersessionInstantPassed)
}

/// The floor itself, without the code — because **two units share it and the
/// design set says so**.
///
/// §5 declares `SUPERSESSION_INSTANT_PASSED` as *"the same floor `inst-gc-compose`
/// gives cutovers, applied to the everyday mechanism"*, and the grandfathering
/// cutover's [`check_cutover_instant`] is the second caller. Extracted the moment
/// that caller appeared rather than copied: a hand-maintained second bound is
/// precisely how two mechanisms come to disagree about one SLO, which is the
/// argument this module's header already makes about the delay constant.
///
/// The **noun** is the caller's, not a parameterised code: what differs between the
/// two units is the word an operator reads (`changeover` / `cutover`) and the code
/// the envelope carries. The sentence, the bound and the remedy are one spelling.
///
/// [`check_cutover_instant`]: crate::domain::cutover::check_cutover_instant
///
/// # Errors
///
/// The refusal **sentence**, for the caller to wrap in its own code.
pub fn changeover_floor(
    instant: OffsetDateTime,
    now: OffsetDateTime,
    moment: ChangeoverMoment,
    noun: &str,
) -> Result<(), String> {
    let floor = now + moment.margin();
    if instant > floor {
        return Ok(());
    }
    Err(format!(
        "{noun} instant {} is not strictly after {} at {}; {}",
        format_rfc3339(instant),
        format_rfc3339(floor),
        match moment {
            ChangeoverMoment::Submit => "submit",
            ChangeoverMoment::Commit => "approval commit",
        },
        moment.remedy()
    ))
}

/// One of a key's windows as **compose** needs to see it: its durable name and its
/// interval.
///
/// [`WindowInterval`] deliberately carries no `window_id` — its doc gives the
/// reason, and the reason is about the *consumer* side: a consumer resolving a price
/// at `t` asks which interval covers `t`, and freezing a truth-side identifier into
/// a ≥ 7-year INSERT-only store serves a reader with no call to make with it.
///
/// Compose is the opposite question. It is an **operator-side, truth-side** act that
/// must name the row it is about to shorten, so it needs the pair, and pairing it
/// here rather than widening `WindowInterval` is what keeps that argument intact.
#[domain_model]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NamedWindow {
    /// The window's durable name.
    pub window_id: Uuid,
    /// Its interval and state.
    pub interval: WindowInterval,
}

/// The predecessor window's `effectiveTo` and where it moves to.
#[domain_model]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WindowShorten {
    /// The window being shortened — the one that covers the changeover.
    pub window_id: Uuid,
    /// Its new end: the changeover instant itself.
    pub effective_to: OffsetDateTime,
}

/// The two window operations `inst-su-compose` composes, proven adjacent.
#[domain_model]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ComposedWindows {
    /// (b) — the predecessor's window, shortened to the changeover.
    pub shorten: WindowShorten,
    /// (c) — the successor's window, `[changeover, …)`.
    pub successor: WindowInterval,
}

/// Compose `inst-su-compose`'s two window operations over one key's window plane.
///
/// # What it decides, and what it deliberately does not
///
/// It answers two questions about the key's plane at the changeover instant, and
/// both are refusals `inst-su-compose` promises a *committed* unit can never
/// produce — which is precisely why they are produced here:
///
/// 1. **Is there coverage at the changeover?** The window that covers it is the one
///    being superseded, and its end moves to the changeover. A key with none is
///    **dormant** — coverage already ended, or never started — and the unit
///    presupposes current coverage. The design set's remedy is a plain publish plus
///    a window schedule, a *revival* rather than a supersession, so the refusal says
///    that instead of inviting a retry.
/// 2. **Would the successor collide?** The successor is open-ended (`[changeover,
///    …)`), so anything on the key beginning at or after the changeover is inside
///    it. That is `WINDOW_OVERLAP`, and compose is the last place it can be a
///    refusal rather than a half-applied unit.
///
/// It does **not** check the changeover instant's floors — that is
/// [`check_changeover_instant`], asked twice at two moments, and folding it in here
/// would give compose one of the two answers and hide which. It does not check the
/// millisecond quantum either: D-144's quantum is the store's, stated at
/// [`crate::domain::instant`], and a second owner of that rule is how the two come
/// to disagree. And it does not look for **pre-existing** interior gaps on the key:
/// the shorten moves one end earlier and the successor fills from exactly that
/// instant, so it can open no new gap, and an old one is `inst-fg-detect`'s at
/// publish. A compose that re-refused it would report a defect this act neither
/// caused nor can fix.
///
/// # Coverage is the occupying states, and that set is not a choice made here
///
/// `cancelled` and `expired` windows are not coverage: a cancelled window is a
/// schedule that never happened (which is why D-121 keeps it out of the read model)
/// and an expired one is history. That is exactly
/// [`crate::domain::window::OCCUPYING_STATES`] — the same set
/// `window_repo::refuse_overlap` asks of its `SELECT` — and it matters twice here, because an
/// interval-only reading would both *shorten* a cancelled window that carries
/// nothing and *ignore* a scheduled sibling that would collide.
///
/// A `scheduled` window that covers the changeover **is** coverage. Repricing a key
/// whose window has not opened yet is a supersession like any other: the covering
/// window's start is untouched and only its end moves, so nothing
/// `inst-ws-immutable` freezes is disturbed.
///
/// # The successor is open-ended even where its predecessor was not
///
/// `inst-su-compose` says `[changeover, …)` without qualification, and that is
/// right rather than merely literal: a predecessor's `effectiveTo` was a fact about
/// **the old price's** planned end, and inheriting it would silently plant a
/// trailing void at an instant nobody chose in this act. If the key's coverage
/// should end, that is a `PATCH` on the successor's window — its own act, with its
/// own materiality.
///
/// # Errors
/// [`DomainError::LifecycleForbidden`] when the key is dormant at the changeover;
/// [`DomainError::WindowOverlap`] naming the window the successor would collide
/// with.
pub fn compose_windows(
    plane: &[NamedWindow],
    changeover: OffsetDateTime,
) -> Result<ComposedWindows, DomainError> {
    let occupying = || {
        plane
            .iter()
            .filter(|window| OCCUPYING_STATES.contains(&window.interval.state))
    };

    let covering = occupying()
        .find(|window| window.interval.covers(changeover))
        .ok_or_else(|| {
            DomainError::SupersessionKeyDormant(format!(
                "the canonical scope key is dormant at {}: no scheduled or active window covers \
                 that instant, and a supersession presupposes current coverage. Schedule a window \
                 on the key's current row to revive it; the publish half of that remedy applies \
                 only where the key carries no published row at all",
                format_rfc3339(changeover)
            ))
        })?;

    // The covering window's own start, answered before the collision scan and on its
    // own terms.
    //
    // **`covers` is inclusive at the start** (`effective_from <= at`), so a window
    // beginning exactly at the changeover is the covering window *and* matches the
    // collision predicate below. The covering window is **not** "excluded by
    // construction, since it covers the changeover its start is strictly before":
    // without this arm such a key is told it "carries later coverage that this
    // supersession would not replace", naming a sibling that does not exist.
    //
    // Refusing is right on the substance: shortening a window to its own start leaves
    // the empty interval `[changeover, changeover)`, so there is no predecessor
    // coverage left to hand over — the same absence dormancy reports, reached from the
    // other side. What the operator wants on this key is to edit or reschedule the
    // window that has not opened yet, not to supersede across it.
    if covering.interval.effective_from == changeover {
        return Err(DomainError::SupersessionWouldEmptyWindow(format!(
            "window {} on this canonical scope key begins at {}, the changeover itself: \
             shortening it to that instant would leave it empty, so there is no coverage for a \
             successor to take over from. Adjust or reschedule that window instead of superseding \
             across it",
            covering.window_id,
            format_rfc3339(changeover)
        )));
    }

    // Anything beginning at or after the changeover is inside the successor's
    // open-ended interval. The covering window is excluded **by identity**, because it
    // cannot be excluded by construction — see the arm above.
    if let Some(collision) = occupying().find(|window| {
        window.window_id != covering.window_id && window.interval.effective_from >= changeover
    }) {
        return Err(DomainError::WindowOverlap(format!(
            "window {} begins at {}, which the successor's open-ended interval from {} already \
             covers; the key carries later coverage that this supersession would not replace",
            collision.window_id,
            format_rfc3339(collision.interval.effective_from),
            format_rfc3339(changeover)
        )));
    }

    Ok(ComposedWindows {
        shorten: WindowShorten {
            window_id: covering.window_id,
            effective_to: changeover,
        },
        successor: WindowInterval::new(changeover, None, WindowState::Scheduled),
    })
}

/// Everything the unit needs to survive between compose and commit.
///
/// It carries the **changeover instant** rather than only the composed intervals,
/// and that is the load-bearing field: `inst-su-commit` applies both window
/// operations at *commit*, so nothing about the plane is written at compose and the
/// commit re-derives the intervals from this instant against the plane as it then
/// stands. A plan that carried only the intervals would leave the commit
/// re-validating numbers it could not re-derive if a window had moved under it.
#[domain_model]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SupersessionPlan {
    /// The changeover — the one input the unit's payload must preserve.
    pub changeover: OffsetDateTime,
    /// The two window operations, as they stand against the plane just read.
    pub windows: ComposedWindows,
}

/// The whole compose-time judgement of a supersession: the instant, the plane, and
/// the successor's content.
///
/// Called at **both** of `inst-su-compose`'s moments — "validated at compose **and**
/// re-validated at commit" — with the same world and a different
/// [`ChangeoverMoment`]. Everything except the floor is re-read identically, which
/// is what makes a plan that submitted legally refusable at commit with nothing else
/// having moved.
///
/// # The three refusals are ordered, and the order is a decision
///
/// Any two of them can apply at once, so what an operator hears first is chosen
/// rather than incidental:
///
/// 1. **The instant** ([`check_changeover_instant`]). It is the only one of the
///    three that is wrong about the *request* rather than about what the request
///    would do, it needs no reading of the world, and its remedy — recomposition
///    against a fresh instant — is what produces a request the other two can even be
///    asked of.
/// 2. **The plane** ([`compose_windows`]: dormancy, then collision). These decide
///    whether a *supersession* is the right operation at all: a dormant key's remedy
///    is a publish plus a window schedule, an entirely different act, and a key
///    carrying later coverage is one this act's open-ended successor cannot be shaped
///    for.
/// 3. **The content** (the D-82/D-98/D-122/D-127/D-129 unit guard). It presupposes
///    there is a supersession to do and asks only whether this one is shaped right.
///
/// The reverse order has a real argument — the content guard's violations are
/// fixable in the caller's own payload, while (1) and (2) send them elsewhere — and
/// it loses on this: validating the content of an act that must not be attempted
/// tells someone to correct a payload they should not send at all. It is written
/// down so the next reader can disagree with an argument rather than with a coin
/// flip.
///
/// # Errors
/// [`DomainError::SupersessionInstantPassed`], [`DomainError::LifecycleForbidden`]
/// or [`DomainError::WindowOverlap`] per the order above, and
/// [`DomainError::ValidationFailed`] carrying the unit guard's report — the same
/// shape a publish reports its rule violations in, because
/// `SUPERSESSION_UNIT_MISMATCH` is one of those rules rather than a precondition.
pub fn plan_supersession(
    predecessor: &PriceRow,
    successor: &PriceRow,
    plane: &[NamedWindow],
    changeover: OffsetDateTime,
    now: OffsetDateTime,
    moment: ChangeoverMoment,
) -> Result<SupersessionPlan, DomainError> {
    // D-144's quantum, **before** the distance is compared: a malformed instant is not
    // an instant whose distance is worth measuring.
    //
    // The store owns this rule and both write paths the commit uses do refuse a
    // non-quantized instant — `check_quantum` here is that statement *consulted*, not a
    // second owner of it, exactly as `check_creation` re-checks the emptiness
    // `window_repo::schedule` also checks. It is consulted because the changeover is
    // the one authored instant whose first arrival at the store is **inside the commit
    // transaction**, after two people have approved: without this, a microsecond in the
    // payload survives compose at submit, survives it again at commit, and dies as a
    // storage refusal at the one moment the caller can no longer fix it — on the very
    // path whose sibling refusal exists to avoid wasting a recomposition.
    crate::domain::instant::check_quantum("changeover", changeover)?;
    check_changeover_instant(changeover, now, moment)?;
    let windows = compose_windows(plane, changeover)?;

    // The content step, and it is **two** rule sets rather than one. `inst-su-compose`
    // clause (a) says the successor is "the successor draft row on the same canonical
    // scope key (**S3 rules apply** — incl. the D-82/D-98 unit guard)", and only the
    // guard was being run: `price_row_rules`'s other callers are `run_publish_rules` and
    // — since D-312 — the price authoring write, which keeps only the write-stage
    // subset. A supersession successor reaches neither —
    // `infra::publish::validated_draft_rows` deliberately excludes a draft on an
    // occupied key from the plan-revision unit's set (D-195). So the row that publishes
    // through this unit was judged by **no** row-local rule at all: a `graduated`
    // successor with an empty band set had nothing to refuse it, no CHECK requires bands
    // for a tiered kind, and with zero band rows no band trigger fires.
    //
    // One report for both, so an operator gets one document naming every violation
    // rather than the first set's and then, on the next attempt, the second's.
    let mut report = crate::domain::rules::price_row_rules().run(successor);
    report.absorb(
        crate::domain::rules::supersession_rules().run(&SupersessionPair::new(
            predecessor.clone(),
            successor.clone(),
        )),
    );
    if !report.is_publishable() {
        return Err(DomainError::ValidationFailed(report));
    }

    Ok(SupersessionPlan {
        changeover,
        windows,
    })
}

#[cfg(test)]
#[path = "supersession_tests.rs"]
mod supersession_tests;
