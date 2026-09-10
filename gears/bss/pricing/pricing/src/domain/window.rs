//! The price window's state machine — `cpt-cf-bss-pricing-state-price-window`
//! (`design/07-pricewindow-linkage.md` §4), its interval arithmetic, and the four
//! §5 refusals that are about a window rather than about a plan.
//!
//! A window is one half-open interval `[effectiveFrom, effectiveTo)` a price row
//! is effective over. Everything here is total and pure: the four states, the
//! edges between them, the containment arithmetic every consumer of a frozen
//! delta re-derives "active at `t`" from, and five checked forms a surface calls
//! before it attempts a mutation.
//!
//! **The transition table lives here rather than in the repository** for the
//! reason `ApprovalState::decide` lives in the domain: the store's compare-and-swap
//! and the migration's trigger arm are both *physical* halves of one rule, and a
//! rule with three spellings needs one of them to be the statement the other two
//! are checked against. `window_repo` consults this and then carries its own
//! `state = <expected>` predicate into the `UPDATE`, exactly as
//! `approval_repo::decide` does; the trigger is what an ad-hoc `UPDATE` meets.
//! [`transition`] is that statement's checked form and **delegates** to
//! [`WindowState::may_move_to`]; it does not restate the table, because a second
//! spelling of an edge set is a second answer to which edges exist.
//!
//! # Which refusals are the store's and which are a surface's
//!
//! The line is one criterion, and it is worth stating once because §5 declares
//! eight codes and this module implements four of them:
//!
//! **The store answers what it also enforces physically; a surface answers what
//! only a request can be judged against.**
//!
//! - `WINDOW_OVERLAP` and `WINDOW_HISTORICAL_IMMUTABLE` are the store's. Both
//!   have a physical half —
//!   `trg_pricing_price_window_append_only`'s immutable-history and
//!   future-`effective_to` arms, and §6's "inside every mutation" for the
//!   overlap — so the store is where the rule already lives, and a pre-check
//!   there is what turns a trigger's 500 into the 409 §5 names.
//!   [`crate::infra::storage::repo::window_repo`] raises both.
//! - `WINDOW_START_IN_PAST` and `WINDOW_NOT_CANCELLABLE` are a surface's. Neither
//!   has a physical half and neither can have one: "strictly in the future"
//!   compares against the instant a **request** arrived, and "only a not-yet-active
//!   window cancels" is about which operation the caller asked for, which a row
//!   does not carry. [`check_creation`] and [`check_cancellation`] are their sole
//!   producers.
//!
//! **All four are raised by a mounted surface.** `WindowService::schedule` calls
//! [`check_creation`] and `cancel` calls [`check_cancellation`], both before the
//! store, which is what turns a trigger's 500 into the 409 §5 names. The contrasting
//! posture is worth keeping: `SELF_APPROVAL_FORBIDDEN` gets **no constant** while
//! nothing raises it, because there the enforcement itself is absent and a constant
//! would read as a guard that exists.
//!
//! # `WINDOW_COVERAGE_MISSING`, `WINDOW_GAP`, `WINDOW_TRAILING_VOID` and
//! `AVAILABILITY_OUTSIDE_COVERAGE` are not here
//!
//! The remaining §5 codes are coverage's, not one window's — and the roster above is
//! the whole of them rather than a count, because a count beside a roster is a
//! sentence that stops being true when the roster grows. Each ranges over **every**
//! window of a canonical scope key, which is what puts them one plane up from this
//! module: `inst-wc-required`, `inst-fg-detect`, `inst-fg-trailing` and
//! `inst-wc-availability` in order. One of the four — `WINDOW_TRAILING_VOID` — needs
//! two inputs none of the others do: W6's longest billing cycle (D-80) and the D-79
//! inbound subscriber lane, the latter because `inst-fg-trailing`'s only exemption is
//! about in-flight subscribers. `CoverageChecker` owns all four; a code declared here
//! would put them under two owners the day it lands.

use std::fmt;

use toolkit_macros::domain_model;

use crate::domain::error::DomainError;
use crate::domain::instant::format_rfc3339;
use crate::domain::scope_key::ScopeKey;
use time::OffsetDateTime;

/// A scheduled or adjusted window intersects one already on the same canonical
/// scope key (§5, **409**).
///
/// Declared by `design/07-pricewindow-linkage.md` §5's problem-response list —
/// *"`WINDOW_OVERLAP` (409 — the scheduled/adjusted window overlaps an existing
/// one on the same canonical scope key)"* — so this is a code the design set
/// names rather than one minted here.
///
/// It lives beside the state token because the rule it names is the store's:
/// non-overlap is enforced inside every mutation (§6) rather than by any
/// constraint, so the refusal's only producer is
/// [`crate::infra::storage::repo::window_repo`] and its code has to be nameable
/// from the layer that maps that repository's refusals onto the wire.
pub const WINDOW_OVERLAP: &str = "WINDOW_OVERLAP";

/// A mutation reached for something §4 froze (§5, **409**).
///
/// §5's own scope, verbatim: *"mutation of a past `effectiveFrom`, an
/// expired/cancelled window, or the window↔price binding"*. `inst-ws-immutable`
/// adds the fourth member of the same set from the other direction — the only
/// permitted mutation of an `active` window is moving its **future**
/// `effectiveTo` — so a move of an end that has already passed, or to an instant
/// that has, is this refusal too.
///
/// A **409** rather than a 400 because the caller's request was well-formed and
/// what refused it is where the clock now stands: a `PATCH` composed against a
/// window whose end was five minutes away is a request that was right when it was
/// written. Re-reading the window is the remedy, which is what a 409 asks for.
pub const WINDOW_HISTORICAL_IMMUTABLE: &str = "WINDOW_HISTORICAL_IMMUTABLE";

/// A cancellation was asked of a window that has taken effect or is history
/// (§5, **409**; `inst-ws-cancel`).
///
/// §4's third edge leaves `scheduled` and nothing else: *"an **active or
/// historical window is never cancelled or deleted** — active windows are only
/// shortened via `effectiveTo`"*. The remedy is a different operation on the same
/// object, which is why it is worth its own code rather than
/// [`WINDOW_HISTORICAL_IMMUTABLE`]: an operator told the window is immutable stops,
/// and an operator told it is not *cancellable* shortens it instead.
pub const WINDOW_NOT_CANCELLABLE: &str = "WINDOW_NOT_CANCELLABLE";

/// A window was created with a start that is not strictly in the future
/// (§5, D-63, `inst-ws-future-start`).
///
/// §5 types it 422, which is architectural: the canonical family has no 422 and
/// the code is the discriminator (`crate::infra::error_mapping`'s module note).
///
/// **It closes the backdating bypass, and that is the whole of why it exists.**
/// `inst-ws-immutable` guards only the *mutation* of a past start, so without
/// this rule a `plan × write` holder could POST a window starting sixty days ago,
/// have the `WindowActivationJob` activate it on its next pass, and reprice open
/// unposted arrears periods — going round the `BackdateGrant` that S2
/// `inst-cs-availability` calls the **only** sanctioned backdating
/// (reason-mandatory, two-person) and never tripping S5's `BACKDATE_SIDE_EFFECT`
/// predicate. Retroactive effectiveness exists only as reference rows on the
/// Slice 5 historical-import path, which schedules no windows at all.
pub const WINDOW_START_IN_PAST: &str = "WINDOW_START_IN_PAST";

/// A draft price row cannot be discarded while a window stands on it.
///
/// **The design set declares no code for this refusal and the entry is owed** —
/// stated here rather than left to be discovered, `PricingAlarm`'s two undeclared
/// variants' arrangement. §5 declares refusals about a *window*; this is a refusal
/// about the **price row underneath one**, and the set does not contemplate
/// discarding a row a schedule already points at.
///
/// **It is a precondition and not a conflict, and the measurement is the reason.**
/// `fk_pricing_price_window_price` references `pricing_price (price_id)` with no
/// cascade on either backend, and `pricing_price_window` refuses `DELETE`
/// outright — *"cancel is a state, not a deletion"* (§6). So no act removes the
/// referrer: a row a window has ever stood on can never be deleted, and telling
/// the caller to retry after cancelling would name a remedy that does not exist.
/// [`crate::domain::error::DomainError::PlanRetiredNoSuccessor`]'s class for
/// exactly that reason — state forbidding the operation, with no way round it —
/// and what it must not be is indistinguishable from the refusals an operator
/// *can* work around.
///
/// What the operator can still do is cancel the window, so nothing takes effect;
/// the draft then stands unpublished and prices nothing.
///
/// **Cascade is the rejected alternative.** Deleting the windows with the draft
/// costs the operator scheduled state they did not ask about and cannot see they
/// lost: a window is the subject of approval units, and its own table is
/// append-only precisely so that state is never removed silently.
pub const PRICE_WINDOW_SCHEDULED: &str = "PRICE_WINDOW_SCHEDULED";

/// The window states an interval competes for its canonical scope key with.
///
/// A `cancelled` window never took effect and an `expired` one cannot be
/// intersected by anything a caller may create: `inst-ws-future-start` bounds a new
/// interval into the future, and a window is `expired` only once its `effective_to`
/// has passed. Neither is therefore an occupant, and treating them as ones would
/// make a key unusable forever after its first reprice — the shape
/// `price_repo::find_key_occupant` rejects one table over, for the same reason.
///
/// **It lives here, in the domain, because both of its consumers need it and one of
/// them cannot see the other.** `window_repo::refuse_overlap` asks it of a `SELECT`
/// predicate; [`supersession::compose_windows`](crate::domain::supersession::compose_windows)
/// asks it of a key's plane in memory, to tell coverage at the changeover from an
/// interval that merely spans it. A copy in the domain beside the original in infra
/// is how two mechanisms come to disagree about one set — the drift the unit guard's
/// shared field list exists to prevent, one plane over.
pub const OCCUPYING_STATES: &[WindowState] = &[WindowState::Scheduled, WindowState::Active];

/// Where one window stands in §4's machine.
#[domain_model]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum WindowState {
    /// Created and not yet in force. The machine's initial state, and the only
    /// state a window may be cancelled from (`inst-ws-cancel`).
    #[default]
    Scheduled,
    /// In force: `now` has reached `effectiveFrom` and the
    /// `WindowActivationJob` flipped it (`inst-ws-activate`). Its only permitted
    /// mutation is moving a **future** `effectiveTo` (`inst-ws-immutable`).
    Active,
    /// `now` has reached `effectiveTo` (`inst-ws-expire`). **Terminal**, and
    /// immutable history — an open-ended window never reaches it.
    Expired,
    /// Unscheduled before it ever took effect (`inst-ws-cancel`). **Terminal**,
    /// and never carrying an `activatedAt`: the only edge into it leaves
    /// [`WindowState::Scheduled`].
    Cancelled,
}

impl WindowState {
    /// Every state, stable order — §4's order, which is also the order a window
    /// lives through.
    pub const ALL: &'static [Self] = &[
        Self::Scheduled,
        Self::Active,
        Self::Expired,
        Self::Cancelled,
    ];

    /// The persisted / wire token.
    ///
    /// These four literals are exactly what `chk_pricing_price_window_state`
    /// admits. A token renamed on one side only is a row the other side can
    /// neither write nor read.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Scheduled => "scheduled",
            Self::Active => "active",
            Self::Expired => "expired",
            Self::Cancelled => "cancelled",
        }
    }

    /// Read a stored token back into the machine, or `None` if it is not one of
    /// ours.
    ///
    /// `None` rather than a default, for [`crate::domain::approval::ApprovalState::from_token`]'s
    /// reason: an unrecognised token means the table was written around the
    /// CHECK, and defaulting it to `scheduled` would make an unreadable row look
    /// like a window that is about to take effect.
    #[must_use]
    pub fn from_token(token: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|s| s.as_str() == token)
    }

    /// Is this state immutable history (`inst-ws-immutable`)?
    ///
    /// The single reading of terminality in the crate. The migration says the
    /// same thing physically — an `UPDATE` of a row in one of these states is
    /// refused outright — and nothing reconstructs the answer from which flip
    /// timestamps happen to be set.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Expired | Self::Cancelled)
    }

    /// May a window move from `self` to `to` — §4's three edges and no others?
    ///
    /// `scheduled -> active` (`inst-ws-activate`), `scheduled -> cancelled`
    /// (`inst-ws-cancel`), `active -> expired` (`inst-ws-expire`). A self-edge is
    /// **not** a transition and answers `false`: the callers that want
    /// idempotence — the activation job re-driving a sweep it already completed
    /// — have to ask that question in the store, where "the row is already
    /// there" and "the row may move there" are different answers.
    #[must_use]
    pub const fn may_move_to(self, to: Self) -> bool {
        matches!(
            (self, to),
            (Self::Scheduled, Self::Active | Self::Cancelled) | (Self::Active, Self::Expired)
        )
    }
}

impl fmt::Display for WindowState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One window as everything downstream of the store reads it: its half-open
/// interval and the state that interval is held under.
///
/// **This is the unit D-99 freezes, and the reason the frozen delta needs no
/// re-projection when a window activates.** The read model carries intervals and
/// states — never "active at `t`" — so `now` crossing an `effectiveFrom` changes a
/// truth row and changes nothing projected: a consumer re-derives the predicate
/// from the same frozen interval and gets the new answer
/// (`inst-sg-surface`, `inst-ws-publishunit`, Foundation §4.4).
///
/// It carries no `window_id` deliberately. A consumer resolving a price at `t`
/// asks which interval covers `t`, and the window's durable name answers a
/// different question — the operator's, on the truth side, which
/// `GET …/coverage` and the `PriceWindow*` events serve. Putting it here would
/// freeze a truth-side identifier into a ≥ 7-year INSERT-only store for a reader
/// that has no call to make with it.
#[domain_model]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WindowInterval {
    /// Inclusive start, UTC.
    pub effective_from: OffsetDateTime,
    /// Exclusive end, UTC; `None` is open-ended (`inst-ws-expire`).
    pub effective_to: Option<OffsetDateTime>,
    /// Where the window stands in §4's machine.
    pub state: WindowState,
}

impl WindowInterval {
    /// Name an interval.
    #[must_use]
    pub const fn new(
        effective_from: OffsetDateTime,
        effective_to: Option<OffsetDateTime>,
        state: WindowState,
    ) -> Self {
        Self {
            effective_from,
            effective_to,
            state,
        }
    }

    /// Does the interval contain `at` — `effective_from <= at < effective_to`?
    ///
    /// **Half-open, and the exclusive end is the whole of the adjacency rule.**
    /// A window does not cover its own `effectiveTo`, so `effectiveTo =
    /// next.effectiveFrom` leaves every instant covered exactly once: §9 names the
    /// false positive an inclusive end would produce, and it would refuse the
    /// shape a supersession and a cutover both produce.
    ///
    /// **It asks nothing about the state**, and that separation is deliberate.
    /// Sellability predicate (1) is "an **active** window covers `t` *with the
    /// D-80 coverage horizon*" — a conjunction over three facts, of which this is
    /// one — and a `covers` that folded the state in would leave the other two
    /// consumers of pure containment (the coverage walk, `inst-el-bootstrap`'s
    /// generation match) unable to ask the question they actually have.
    #[must_use]
    pub fn covers(&self, at: OffsetDateTime) -> bool {
        self.effective_from <= at && self.effective_to.is_none_or(|end| at < end)
    }

    /// Is the end absent — a window that never expires (`inst-ws-expire`)?
    #[must_use]
    pub const fn is_open_ended(&self) -> bool {
        self.effective_to.is_none()
    }

    /// Has `effective_from` arrived — `inst-ws-activate`'s trigger condition?
    #[must_use]
    pub fn has_started(&self, now: OffsetDateTime) -> bool {
        now >= self.effective_from
    }

    /// Has `effective_to` arrived — `inst-ws-expire`'s trigger condition?
    ///
    /// **An open-ended window answers `false` forever**, which is the instruction's
    /// own words and not a convenience: no fallback pricing exists, so a key whose
    /// coverage simply stopped fails closed downstream rather than falling back to
    /// anything.
    #[must_use]
    pub fn is_due_to_expire(&self, now: OffsetDateTime) -> bool {
        self.effective_to.is_some_and(|end| now >= end)
    }

    /// Is the interval a non-empty one — `effective_to > effective_from`?
    ///
    /// The predicate half of `chk_pricing_price_window_interval`, in the shape
    /// [`crate::domain::instant::is_quantized`] takes: one statement, consulted by
    /// the store (which answers with a `RepoError`) and by
    /// [`check_creation`] (which answers on the wire). `effective_to =
    /// effective_from` is not a zero-length window, it is a mistake — nothing is
    /// effective over it and no consumer can resolve a price at any instant of it.
    #[must_use]
    pub fn is_non_empty(&self) -> bool {
        interval_is_non_empty(self.effective_from, self.effective_to)
    }
}

/// [`WindowInterval::is_non_empty`] before there is an interval to ask it of —
/// the form a creation check needs, which has two instants and no row.
#[must_use]
pub fn interval_is_non_empty(
    effective_from: OffsetDateTime,
    effective_to: Option<OffsetDateTime>,
) -> bool {
    effective_to.is_none_or(|end| end > effective_from)
}

/// The four §5 refusals about one window, each carrying the code §5 declares for
/// it.
///
/// **The only place those four strings are paired with a rejection.** A refusal
/// that spelled its own code at the raising site would be a second spelling, and
/// the crate has one measured instance of what that costs: `WINDOW_OVERLAP`
/// corrupted to `WINDOW_OVERLAPX` left the whole error suite green because the
/// assertion looked for the code *inside* a rendered document.
///
/// It carries no detail. A detail is one sentence per raising site — which window,
/// which instant, which clock — and folding it in here would make the type a
/// constructor for messages rather than the pairing of a rejection with its code.
/// [`WindowRefusal::refuse`] takes the sentence and hands back the
/// [`DomainError`] the ladder maps.
#[domain_model]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum WindowRefusal {
    /// Two intervals on one canonical scope key claim one instant.
    Overlap,
    /// A mutation reached for something §4 froze.
    HistoricalImmutable,
    /// A cancellation was asked of a window that has taken effect or is history.
    NotCancellable,
    /// A creation named a start that is not strictly in the future.
    StartInPast,
}

impl WindowRefusal {
    /// Every refusal, stable order — the order §5 lists them in.
    pub const ALL: &'static [Self] = &[
        Self::Overlap,
        Self::HistoricalImmutable,
        Self::NotCancellable,
        Self::StartInPast,
    ];

    /// The wire code §5 declares for this refusal.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::Overlap => WINDOW_OVERLAP,
            Self::HistoricalImmutable => WINDOW_HISTORICAL_IMMUTABLE,
            Self::NotCancellable => WINDOW_NOT_CANCELLABLE,
            Self::StartInPast => WINDOW_START_IN_PAST,
        }
    }

    /// This refusal as the rejection the ladder maps, carrying `detail`.
    ///
    /// The arm → variant pairing lives here and nowhere else, so the categories
    /// §5 assigns — three conflicts and one precondition failure — are decided
    /// once rather than at four raising sites.
    #[must_use]
    pub fn refuse(self, detail: String) -> DomainError {
        match self {
            Self::Overlap => DomainError::WindowOverlap(detail),
            Self::HistoricalImmutable => DomainError::WindowHistoricalImmutable(detail),
            Self::NotCancellable => DomainError::WindowNotCancellable(detail),
            Self::StartInPast => DomainError::WindowStartInPast(detail),
        }
    }
}

impl fmt::Display for WindowRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code())
    }
}

/// May a window be created over `[effective_from, effective_to)` at `now`?
///
/// Two rules, in the order an author can act on them: the interval has to *be* an
/// interval, and its start has to be **strictly** in the future
/// (`inst-ws-future-start`, D-63 — see [`WINDOW_START_IN_PAST`] for what that
/// closes). Strictly, so `effective_from == now` is refused: a window created at
/// the instant it takes effect is one the `WindowActivationJob` may flip before
/// the publish unit carrying it reaches a committed `CatalogVersion`, which is the
/// same unwarmed-changeover exposure D-101 raises `pricing.window.changeover_unwarmed`
/// for.
///
/// The **millisecond quantum** (D-144) is deliberately not re-checked here.
/// **The store is where a window's instants meet it**:
/// `window_repo::schedule` runs [`crate::infra::storage::repo::check_authored_instant`]
/// over both authored instants and `window_repo::adjust_effective_to` over the
/// target end, while `transition`'s `at` and every `stamp.recorded_at` are exempt —
/// machine-generated flips, which is D-144's own split between an authored instant
/// and a recorded one.
///
/// **The window authoring DTOs exist and do not re-check it.**
/// `api::rest::windows`' `ScheduleWindowRequest` and `AdjustWindowRequest` carry
/// the instants through to `WindowService`, and the rule stays the store's: a DTO
/// is a shape, and a second site applying one rule is what this crate's
/// single-owner discipline exists to prevent.
///
/// [`crate::domain::instant::check_quantum`] is the same rule's domain-side
/// entry — `check_authored_instant` shares its `is_quantized` predicate and
/// differs only in raising a `RepoError` — and it has producers of its own on the
/// changeover and cohort instants (`api::rest::repricing_runs`,
/// `domain::supersession`, `domain::scope_key`). Neither of those is a window.
///
/// # Errors
/// [`DomainError::InvalidRequest`] when `effective_to <= effective_from`;
/// [`DomainError::WindowStartInPast`] when the start is not strictly future.
pub fn check_creation(
    effective_from: OffsetDateTime,
    effective_to: Option<OffsetDateTime>,
    now: OffsetDateTime,
) -> Result<(), DomainError> {
    if !interval_is_non_empty(effective_from, effective_to) {
        return Err(empty_interval(effective_from, effective_to));
    }
    if effective_from > now {
        return Ok(());
    }
    Err(WindowRefusal::StartInPast.refuse(format!(
        "effectiveFrom {} is not strictly after {}; a window's start must be in \
         the future at creation",
        format_rfc3339(effective_from),
        format_rfc3339(now)
    )))
}

/// May a window in `state` be cancelled (`inst-ws-cancel`)?
///
/// `scheduled` and nothing else. **An active window is refused here rather than
/// by the edge table**, and the difference is the code: `may_move_to` answers
/// that `active → cancelled` is not an edge, which is what a stalled job needs to
/// hear, while an operator who asked to cancel needs to hear that this window is
/// shortened instead — [`WINDOW_NOT_CANCELLABLE`] rather than a refused
/// transition.
///
/// `cancelled` is refused too, and not as an idempotent no-op. A caller asking to
/// cancel a cancelled window is working from a stale read of a plane where a
/// second cancellation is a **second publish unit** (D-99) with its own
/// `CatalogVersion` request and its own materiality; answering `Ok` would let
/// that unit run over a window that never changes.
///
/// # Errors
/// [`DomainError::WindowNotCancellable`] for any state but
/// [`WindowState::Scheduled`].
pub fn check_cancellation(state: WindowState) -> Result<(), DomainError> {
    if state == WindowState::Scheduled {
        return Ok(());
    }
    Err(WindowRefusal::NotCancellable.refuse(format!(
        "a {state} window is not cancellable; only a not-yet-active window is, and \
         an active one is shortened through its effectiveTo instead"
    )))
}

/// Which ground of `inst-ws-immutable` refuses a move of a window's end, when one
/// does.
///
/// **This is the statement, and the two spellings of the rule are checked against
/// it.** [`check_effective_to_adjustment`] renders it as the
/// [`DomainError::WindowHistoricalImmutable`] a surface answers with;
/// `window_repo::refuse_frozen_end` renders it as the `RepoError::WindowHistorical`
/// the store answers with. Spelling the three grounds independently in each —
/// byte-for-byte identical, in the same order, with neither consulting the other
/// — puts only the store's copy on a live path, so a change to one of them is a
/// change nothing can notice.
///
/// The failure that closes: G4 mounts `PATCH …/price-windows/{windowId}` calling
/// the domain check for the wire answer and then `adjust_effective_to`. A later
/// tightening of one copy only — most likely the **D-04 bound**, which is owed on
/// exactly this path — makes the store admit what the surface refused, or the
/// pre-check more permissive than the floor beneath it, which
/// `adjust_effective_to`'s own doc forbids in as many words.
///
/// It is a **ground** rather than a rendered message because the two callers do not
/// answer in one vocabulary: the surface says `effectiveTo`, the store says
/// `effective_to` and names the window. Each variant carries the value its own
/// sentence needs, which is also what removes the unreachable `String::new`
/// fallbacks both renderings used to carry for an `Option` the ground had already
/// proved present.
#[domain_model]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FrozenEnd {
    /// The window is immutable history — `expired` or `cancelled`. Carries which.
    TerminalState(WindowState),
    /// The **stored** end has already passed at `now`, so moving it in either
    /// direction rewrites what was chargeable. Carries the end that passed.
    StoredEndPassed(OffsetDateTime),
    /// The **target** end is not strictly after `now`, which would make the move a
    /// retroactive reprice rather than a schedule. Carries the target.
    TargetNotFuture(OffsetDateTime),
}

/// `inst-ws-immutable`'s three grounds, as one total function — `None` when the
/// move is permitted.
///
/// 1. a **terminal** window moves nothing — `expired`/`cancelled` are immutable
///    history;
/// 2. the end being moved **must still be in the future** — an end that has
///    already passed is the instant a price stopped applying, and rewriting it
///    rewrites what was charged;
/// 3. the end being moved **to** must be in the future for the same reason, which
///    is what makes a shorten a *schedule* rather than a retroactive reprice.
///
/// The order is load-bearing on ground 1: a terminal window's stored end has
/// usually passed too, so asking 2 first would answer about the clock on a window
/// whose refusal is about its state.
///
/// `None` — open-ended — is a legal target throughout: it removes a bound rather
/// than any coverage, and every instant it adds is in the future by construction.
/// Whether the *coverage* the move leaves behind is legal is a different question
/// with a different owner: `inst-fg-trailing`'s `WINDOW_TRAILING_VOID` ranges over
/// the whole key and is `CoverageChecker`'s.
///
/// Grounds 2 and 3 mirror `trg_pricing_price_window_append_only`'s fifth arm
/// exactly, which is the point — that arm is a 500 to whoever trips it, and both
/// callers here answer the same rule as something actionable.
#[must_use]
pub fn frozen_end(
    current: &WindowInterval,
    to: Option<OffsetDateTime>,
    now: OffsetDateTime,
) -> Option<FrozenEnd> {
    if current.state.is_terminal() {
        return Some(FrozenEnd::TerminalState(current.state));
    }
    if let Some(stored) = current.effective_to
        && stored <= now
    {
        return Some(FrozenEnd::StoredEndPassed(stored));
    }
    if let Some(target) = to
        && target <= now
    {
        return Some(FrozenEnd::TargetNotFuture(target));
    }
    None
}

/// May `current`'s end move to `to` at `now` (`inst-ws-immutable`)?
///
/// The instruction's one permitted mutation of a window that has taken effect.
/// **The three grounds are [`frozen_end`]'s, consulted rather than restated** —
/// that function's doc says which they are and why a second spelling of them was a
/// defect; this one turns the ground into the 409 §5 names.
///
/// # Errors
/// [`DomainError::InvalidRequest`] when the resulting interval would be empty;
/// [`DomainError::WindowHistoricalImmutable`] for each of [`frozen_end`]'s grounds.
pub fn check_effective_to_adjustment(
    current: &WindowInterval,
    to: Option<OffsetDateTime>,
    now: OffsetDateTime,
) -> Result<(), DomainError> {
    if let Some(ground) = frozen_end(current, to, now) {
        return Err(WindowRefusal::HistoricalImmutable.refuse(match ground {
            FrozenEnd::TerminalState(state) => {
                format!("a {state} window is immutable history; its effectiveTo cannot move")
            }
            FrozenEnd::StoredEndPassed(stored) => format!(
                "effectiveTo {} has already passed at {}; only a future end may be moved",
                format_rfc3339(stored),
                format_rfc3339(now)
            ),
            FrozenEnd::TargetNotFuture(target) => format!(
                "effectiveTo {} is not after {}; an end may only be moved to a future instant",
                format_rfc3339(target),
                format_rfc3339(now)
            ),
        }));
    }
    if !interval_is_non_empty(current.effective_from, to) {
        return Err(empty_interval(current.effective_from, to));
    }
    Ok(())
}

/// Does moving `current`'s end to `to` **shorten** it — remove coverage the window
/// carries today?
///
/// D-62's discriminator, and the **one** definition of "shorten" in this crate.
/// §5's Idempotency table and `inst-mat-registered` both scope the always-material
/// trigger to a *shortening* `PATCH`; an extension is not on the trigger list, and
/// treating one as material would put a second principal in front of an act that
/// removes nothing.
///
/// Open-ended is `+∞` on both sides, which is what makes the four cases fall out of
/// one comparison rather than a table:
///
/// * stored `None` → any bound at all shortens (it is the whole of the coverage past
///   that instant);
/// * stored `Some(a)`, target `None` → an extension to forever;
/// * stored `Some(a)`, target `Some(b)` → `b < a`.
///
/// A move to the **same** end is not a shortening: it removes nothing, and answering
/// otherwise would make a no-op `PATCH` need a reviewer.
///
/// It says nothing about whether the move is *legal* — [`frozen_end`] owns that — and
/// nothing about the coverage the key is left with, which is `inst-fg-trailing`'s and
/// ranges over every window of the key rather than this one.
#[must_use]
pub fn shortens(current: &WindowInterval, to: Option<OffsetDateTime>) -> bool {
    match (current.effective_to, to) {
        // An open-ended window bounded anywhere loses its whole tail; open-ended
        // either side of the move loses nothing.
        (None, Some(_)) => true,
        (None | Some(_), None) => false,
        (Some(stored), Some(target)) => target < stored,
    }
}

/// May a window move from `from` to `to` — §4's edge set, checked?
///
/// **It delegates to [`WindowState::may_move_to`] and states no table of its
/// own.** The table has three spellings already (that function, the store's
/// `state = <expected>` predicate, and the migration's transition arm) and a
/// fourth would be a fourth answer to which edges exist.
///
/// The refusal is [`DomainError::LifecycleForbidden`] and **mints no window
/// code**, for [`crate::infra::storage::RepoError::WindowStateForbidden`]'s
/// reason: §5 names a code for every window refusal an operator can *provoke* —
/// a cancellation of an active window, a start in the past, a mutation of
/// history — and an unsanctioned edge is not one of them. What reaches this
/// function is a background sweep whose ordering assumption broke (asked to
/// expire a window somebody cancelled between the scan and the flip), which is
/// the refused lifecycle edge it is.
///
/// A **self-edge is refused**, because it is not a transition. Idempotence is a
/// question about the row — "it is already there" — and the store is where that
/// question can be asked; folding the two would let a re-driven sweep re-stamp
/// the instant a price took effect.
///
/// # Errors
/// [`DomainError::LifecycleForbidden`] when `from → to` is not one of §4's three
/// edges.
pub fn transition(from: WindowState, to: WindowState) -> Result<(), DomainError> {
    if from.may_move_to(to) {
        return Ok(());
    }
    Err(DomainError::LifecycleForbidden(format!(
        "a window does not move from {from} to {to}"
    )))
}

/// The refusal of an interval that is not one.
///
/// [`DomainError::InvalidRequest`] and **no new code**, which is a reported
/// divergence rather than an omission: §5 declares eight window codes and none of
/// them covers `effectiveTo <= effectiveFrom`. §6 states the rule as a CHECK
/// (`chk_pricing_price_window_interval`) and nothing else, so a caller tripping it
/// read a 500. `INVALID_REQUEST` is what the ladder already answers for a value
/// the gear cannot interpret, and an empty interval is exactly that — not a
/// conflict with any state, not a precondition of the world, just two instants
/// that do not describe a span.
///
/// **It is also why the four checked forms diverge from the phase plan's sketched
/// interface, and that divergence is reported here rather than left to be noticed.**
/// The plan declares `check_creation(effective_from, now)` and
/// `Result<(), WindowRefusal>` for all four; what is built takes `effective_to` as
/// well and returns `Result<(), DomainError>`. Both follow from this refusal: the
/// empty-interval rule needs the end to judge it, and it is a rejection with **no**
/// `WindowRefusal` arm — §5 declares no window code for it — so a
/// `Result<(), WindowRefusal>` could not carry it and the two checks that can raise
/// it would have had to answer in two types. [`WindowRefusal`] keeps its whole job,
/// which is pairing each of the four rejections with the one code §5 declares for
/// it; [`WindowRefusal::refuse`] is where it becomes the ladder's value.
fn empty_interval(
    effective_from: OffsetDateTime,
    effective_to: Option<OffsetDateTime>,
) -> DomainError {
    DomainError::InvalidRequest(format!(
        "the window interval [{}, {}) is empty; effectiveTo must be strictly after \
         effectiveFrom",
        format_rfc3339(effective_from),
        effective_to.map_or_else(|| "open-ended".to_owned(), format_rfc3339)
    ))
}

/// One canonical scope key's window set, and the coverage end derived from it.
///
/// The grouping every consumer of window facts needs and none of them should
/// build: non-overlap, the interior-gap walk, the D-80 horizon and
/// `inst-el-bootstrap`'s generation match are all **per key**, while the store's
/// rows are per `price_id` — and a key legitimately spans several rows (a
/// supersession leaves a `superseded` predecessor and a `published` successor on
/// one key). A reader that grouped by `price_id` would compute coverage for
/// something that is not a key.
///
/// Intervals are ordered by `(effective_from, effective_to, state)` — see
/// [`group_by_key_seeded`] for why the order is the type's promise rather than the
/// caller's, and why the state is part of the key.
///
/// **An empty `intervals` is a meaningful value**, not a group that should not
/// have been built: it is a key its producer is answering about which has no
/// window contributing coverage, and [`KeyWindows::coverage_end`] reads it as
/// [`CoverageEnd::Uncovered`].
#[domain_model]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyWindows {
    /// The ten axes this set is filed under.
    pub scope_key: ScopeKey,
    /// The key's windows, ordered.
    pub intervals: Vec<WindowInterval>,
}

impl KeyWindows {
    /// How far the key's coverage runs (`inst-sg-surface`'s "derived coverage
    /// end").
    ///
    /// **Derived, never stored**, and the D-80 horizon predicate is what needs it:
    /// "the key's active-plus-scheduled coverage MUST additionally extend through
    /// `now + the longest billing cycle sold on the key`". A surface that had only
    /// the interval list would re-derive this per request; a surface that had only
    /// a boolean could not answer it at another `t` at all.
    ///
    /// That quotation is D-80's own and its wording is a trap: §1.6's W6 defines the
    /// term **per plan**, not per key — the longest `frequency` among the plan's
    /// recurring rows on the key's `(currency, region)` — and **zero** on a plan with
    /// no recurring part, where the horizon predicate reduces to "an active window
    /// covers `t`". Nothing in this crate produces the term yet; see
    /// [`crate::infra::read_model`]'s horizon note for the chain that owes it.
    ///
    /// `cancelled` intervals contribute nothing: a cancelled window is a schedule
    /// that never happened (D-121 keeps it out of the read model for the same
    /// reason). `expired` ones **do** contribute — the end of a key's coverage is
    /// where its last window ends whether that instant is past or future, and a
    /// dormant key whose coverage ended last month must read as ending last month
    /// rather than as uncovered.
    ///
    /// It presumes the gap-freeness `inst-fg-detect` enforces at publish: with an
    /// interior gap the honest answer would be the gap's start, and this returns
    /// the far end. The presumption is safe **because** the coverage rule refuses
    /// a gapped key before a version can freeze one, and it is written down here
    /// because that is the rule this answer depends on rather than an assumption
    /// about the data.
    #[must_use]
    pub fn coverage_end(&self) -> CoverageEnd {
        let mut end: Option<OffsetDateTime> = None;
        for interval in self
            .intervals
            .iter()
            .filter(|i| i.state != WindowState::Cancelled)
        {
            if interval.is_open_ended() {
                return CoverageEnd::OpenEnded;
            }
            end = end.max(interval.effective_to);
        }
        end.map_or(CoverageEnd::Uncovered, CoverageEnd::Ends)
    }

    /// Does any **real** interval of this key contain `at`?
    ///
    /// The containment half of sellability predicate (1), and it lives here for
    /// the reason every instant question does: [`WindowInterval::covers`] is the
    /// crate's one reading of `[from, to)`, and this is the fold over a key's set.
    ///
    /// # It does not consult the state token, and that is the settled reading
    ///
    /// `interval ∧ at`, never the token — see
    /// [`PlanSubjectDelta::windows`](crate::domain::projection::PlanSubjectDelta::windows)
    /// for the argument. The token is stale by construction for every window the
    /// activation sweep flips, because D-99 makes an activation re-project
    /// nothing, so a predicate that read it would make activation owe a
    /// re-projection.
    ///
    /// The one exception is `cancelled`, and it is not a token read in the sense
    /// the rule forbids: a cancelled window is a schedule that **never happened**,
    /// which is a fact about the past no interval can carry. `PROJECTED_WINDOW_STATES`
    /// keeps it out of a delta entirely, so the filter is what makes this total
    /// over a hand-built set rather than a case a projection produces.
    ///
    /// # Why this is not [`KeyCoverage::covers`](crate::domain::coverage::KeyCoverage::covers)
    ///
    /// That one filters on `COVERING_STATES` — `scheduled` and `active` — because
    /// the rules it serves (`inst-wc-required`, `inst-fg-detect`) ask whether a
    /// key's coverage is **live at publish**. The two answers differ on exactly
    /// one input: an `expired` interval and an `at` inside it, i.e. a question
    /// about the **past**, which is the question this one has to answer and that
    /// one never asks. A frozen version must answer a past instant the same way
    /// forever — that is what a pinned consumer replaying an order instant reads —
    /// so folding the state in here would make the answer depend on when it was
    /// asked. Two rosters for two sentences, exactly as `COVERING_STATES`' own doc
    /// argues against deriving itself from `OCCUPYING_STATES`.
    #[must_use]
    pub fn covers_at(&self, at: OffsetDateTime) -> bool {
        self.intervals
            .iter()
            .filter(|i| i.state != WindowState::Cancelled)
            .any(|i| i.covers(at))
    }

    /// The first instant from `from` onward that no interval of this key covers,
    /// when there is one before `until`.
    ///
    /// One walk for the three shapes a coverage bound can be broken by — an
    /// interval that opens **late**, a hole **between** two of them, and a run
    /// that **stops** short — because each of them is the same fact, "here is an
    /// instant nothing covers", and a rule that asked them as three questions is
    /// how the leading one went unasked for a whole landing (D-316 clause (1)).
    ///
    /// `until` is exclusive and `None` means *never*: the caller that passes it
    /// is asking for coverage that runs on forever, and only reaching an
    /// open-ended interval with no break on the way answers that.
    ///
    /// **`cancelled` is the only state filtered**, matching [`Self::coverage_end`]
    /// exactly rather than [`coverage::KeyCoverage`](crate::domain::coverage::KeyCoverage)'s
    /// `COVERING_STATES`. A cancelled window is a schedule that never happened;
    /// `expired` and the stale `scheduled` of a window the activation sweep has
    /// not reached yet both describe real coverage, and filtering on the token
    /// would make this answer depend on when the sweep last ran (D-99).
    ///
    /// # It orders the set itself, because the field cannot promise it
    ///
    /// The walk is only correct on intervals ascending by `effective_from`: the
    /// `break` below reads the first interval starting after `covered_through` as
    /// *"the void opens here"*, and on an unordered set that is a **false** first
    /// uncovered instant — under-reporting coverage on a bound that decides money
    /// (both consumers, `infra::window::refuse_horizon_span_uncovered` and
    /// [`strand_free_disposition`](crate::domain::retirement::strand_free_disposition),
    /// enforce D-04's).
    ///
    /// This doc used to call that ordering *"the type's own promise, restated by
    /// every producer that builds a set by hand"*. It is neither.
    /// [`KeyWindows::intervals`] is a plain `pub` field with no constructor
    /// enforcing anything, and the producers do **not** all restate it:
    /// `infra::window::project`, `infra::grandfather` and `infra::retirement` each
    /// sort and say why, while `infra::window`'s `PlanContext::before` collects
    /// straight out of a `list_for_plan` read — correct today only because that
    /// query happens to `ORDER BY effective_from` three layers away, which is not
    /// a promise this walk can hold. So the order is established here, on a local
    /// copy, and the invariant stops being a comment.
    ///
    /// # It lives here, not privately inside `infra::window`
    ///
    /// D-04's bound has **two** enforcement sites since the retirement guard
    /// landed — `infra::window`'s `refuse_horizon_uncovered`, which judges what a
    /// window act *produces*, and `domain::retirement`'s
    /// [`strand_free_disposition`](crate::domain::retirement::strand_free_disposition),
    /// which judges what a retirement's cancellations would *leave*. A second
    /// copy of this walk would be a second answer to one money bound, which is
    /// the defect class three copies of a rate conversion already cost this
    /// crate. So it is one function on the type it is a fact about.
    #[must_use]
    pub fn first_uncovered_from(
        &self,
        from: OffsetDateTime,
        until: Option<OffsetDateTime>,
    ) -> Option<OffsetDateTime> {
        let mut covered_through = from;
        // Sorted here rather than trusted; see the doc. `effective_to` is the
        // second component so that two intervals sharing a start are walked
        // shortest-first, which is the order the `max` below is a no-op under.
        let mut ordered: Vec<&WindowInterval> = self
            .intervals
            .iter()
            .filter(|i| i.state != WindowState::Cancelled)
            .collect();
        ordered.sort_by_key(|i| (i.effective_from, i.effective_to));
        for interval in ordered {
            if interval.effective_from > covered_through {
                // A void opens here. Whether it matters is `until`'s question,
                // asked once below for this and for the short run alike.
                break;
            }
            // `?` reads as *"covered forever from here"*: an open-ended interval
            // leaves no instant after `covered_through` uncovered, whatever
            // `until` asks for, so the walk's answer is `None` and it is
            // answered now.
            covered_through = covered_through.max(interval.effective_to?);
        }
        match until {
            Some(end) if covered_through >= end => None,
            _ => Some(covered_through),
        }
    }
}

/// Where a canonical scope key's coverage stops.
///
/// **Three arms and not `Option<OffsetDateTime>`**, which is a divergence from the
/// signature the phase plan sketches for `KeyCoverage::coverage_end` and is worth
/// the words. `Option` has to spend its `None` on one of two answers that are
/// **opposites** under the D-80 horizon predicate: a key covered forever passes
/// the horizon trivially, and a key covered not at all fails every predicate
/// there is. Collapsing them makes "this key has no windows" read as "this key
/// sells forever" — the exact direction a fail-closed gate must never round in.
#[domain_model]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CoverageEnd {
    /// No window contributes coverage at all — the key is not sellable at any
    /// instant and no publish should have frozen it.
    Uncovered,
    /// Coverage runs to this exclusive instant.
    Ends(OffsetDateTime),
    /// An open-ended window covers every instant from its start onward.
    OpenEnded,
}

impl CoverageEnd {
    /// The persisted / wire token, for the payload the read model freezes.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Uncovered => "uncovered",
            Self::Ends(_) => "ends",
            Self::OpenEnded => "open_ended",
        }
    }

    /// The instant coverage ends at, when it ends at one.
    ///
    /// `None` on **both** of the other arms, which is why this is a reader's
    /// convenience and not the type: a caller that branches on nothing but this
    /// has the ambiguity back.
    #[must_use]
    pub const fn at(self) -> Option<OffsetDateTime> {
        match self {
            Self::Ends(at) => Some(at),
            Self::Uncovered | Self::OpenEnded => None,
        }
    }
}

/// Group `(key, interval)` pairs into one [`KeyWindows`] per canonical scope key,
/// over exactly the keys the pairs mention.
///
/// The form a caller has when the key set **is** whatever the windows are on — the
/// operator's coverage report over a store read. A caller that knows which keys it
/// is answering *about*, and therefore needs a key with no surviving window to be
/// present and uncovered rather than absent, wants [`group_by_key_seeded`].
#[must_use]
pub fn group_by_key(
    pairs: impl IntoIterator<Item = (ScopeKey, WindowInterval)>,
) -> Vec<KeyWindows> {
    group_by_key_seeded(std::iter::empty(), pairs)
}

/// Group `(key, interval)` pairs into one [`KeyWindows`] per canonical scope key,
/// over the union of `keys` and the keys the pairs mention.
///
/// **A seeded key with no surviving window gets a group with no intervals**, whose
/// [`KeyWindows::coverage_end`] is [`CoverageEnd::Uncovered`]. That is the whole
/// point of the seed and it is a payload fact rather than a convenience: without
/// it, "this key is uncovered" is spelled as the *absence* of a group — a second,
/// undeclared spelling of a fact D-167 clause 1 requires be declared once — and
/// `Uncovered`'s `"uncovered"` token has no producer at all.
///
/// **The output order is this function's promise and not the caller's**, both
/// across groups (by the key's canonical rendering) and within one (by
/// `(effective_from, effective_to, state)`). The reason is the store this feeds: a
/// projected delta is INSERT-only on the ≥ 7-year truth horizon, and a re-drive
/// that produced the same facts in a different order would write a payload that
/// differs byte-for-byte from the one it is meant to be idempotent with.
///
/// **`state` is in the within-group key because the two instants are not a total
/// order over the elements.** [`crate::infra::storage::repo::window_repo`]'s
/// `OCCUPYING_STATES` excludes `expired`, so `refuse_overlap` admits a `scheduled`
/// window with exactly the same bounds as an `expired` one on the same key, and
/// both are projected. Sorting on the instants alone left that tie to be broken by
/// the input order — which is the store's row order, i.e. precisely the thing the
/// byte-stability promise says the output does not depend on. A stable
/// `sort_by_key` would have preserved the input order rather than removed the
/// dependence on it; adding the third component makes the key total over the whole
/// value, so the output is a function of the multiset of facts and of nothing else.
#[must_use]
pub fn group_by_key_seeded(
    keys: impl IntoIterator<Item = ScopeKey>,
    pairs: impl IntoIterator<Item = (ScopeKey, WindowInterval)>,
) -> Vec<KeyWindows> {
    let mut groups: Vec<KeyWindows> = Vec::new();
    for scope_key in keys {
        if !groups.iter().any(|g| g.scope_key == scope_key) {
            groups.push(KeyWindows {
                scope_key,
                intervals: Vec::new(),
            });
        }
    }
    for (scope_key, interval) in pairs {
        match groups.iter_mut().find(|g| g.scope_key == scope_key) {
            Some(group) => group.intervals.push(interval),
            None => groups.push(KeyWindows {
                scope_key,
                intervals: vec![interval],
            }),
        }
    }
    for group in &mut groups {
        group
            .intervals
            .sort_unstable_by_key(|i| (i.effective_from, i.effective_to, i.state));
    }
    // `sort_by_cached_key`, not `sort_by_key`: the latter re-invokes the key
    // function on **every comparison**, and this key allocates and formats the
    // full ten-axis canonical rendering of a scope key. On the path whose whole
    // purpose is a byte-stable INSERT-only payload that is O(n log n) `String`
    // allocations for an order the cached form produces identically.
    groups.sort_by_cached_key(|g| g.scope_key.to_string());
    groups
}

#[cfg(test)]
#[path = "window_tests.rs"]
mod window_tests;
