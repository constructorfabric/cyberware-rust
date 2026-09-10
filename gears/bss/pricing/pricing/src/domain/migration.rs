//! The migration schedule: its state machine, its scheduling rules and the
//! judgements that gate both (`inst-mst-start`, `inst-mst-complete`,
//! `inst-mst-cancel`, `inst-mst-cancel-inflight`, `inst-mg-target`,
//! `inst-mg-cancel`, `inst-mg-idem`, `inst-mg-boundary`, D-34, D-39, D-49).
//!
//! The catalog **schedules**; Subscriptions **executes**. Everything in this
//! module follows from that split: nothing here mutates a subscription, nothing
//! here touches a posted invoice, and the two non-terminal transitions are
//! driven by calls *from* the executor rather than by this gear's clock.
//!
//! # The wall clock does not move this machine
//!
//! `inst-mst-start` is explicit — the flip to `in_progress` happens when the
//! effective date has passed **and** Subscriptions calls `POST.../start`,
//! "never by the wall clock alone". A schedule whose `effective_at` has passed
//! and whose executor has not called is still `scheduled`, and that is the state
//! the `pricing.migration.stalled` alarm exists to notice. Had the catalog
//! flipped on time alone, D-36's execution-time re-resolution would have run
//! against a party that had not started, and the freshly-resolved exclusion set
//! would have had no delivery path to the party that executes (D-65).
//!
//! # D-34, and why `in_progress -> cancelled` is an edge
//!
//! A partially-executed run can be
//! stopped: further `PlanLink` processing halts, already-migrated subscriptions
//! are unaffected — the catalog never held their state, so "unaffected" is by
//! construction rather than by a compensating write — and the partial sets are
//! listed on the record. **Only a `completed` run is uncancellable**, which is
//! why [`MigrationState::ensure_cancellable`] answers
//! [`DomainError::MigrationCompleted`] and not the pre-D-34
//! `MIGRATION_ALREADY_EFFECTIVE`: that code named "past its effective date",
//! which is true of most runs that can still be stopped.
//!
//! # Cancellation propagates as a state handshake, not a clock
//!
//! `inst-mg-cancel`. Subscriptions MUST re-read the schedule state immediately
//! **before beginning execution** and per processing batch thereafter, so it
//! never starts or continues against a cancelled record. That closes the T-epsilon
//! race a "cancel before the effective date" wall-clock rule leaves open, and it
//! is why cancellation rides the record's own state plus audit rather than a new
//! event name (§7). [`MigrationState::admits_further_processing`] is the
//! predicate that handshake asks, stated once here rather than at each caller.
//!
//! # D-49's notice period is enforced here, and its floor is enforced twice
//!
//! [`NoticePeriod`] carries the tenant's configured lead time. The **60-day
//! floor** is a `CHECK` on `pricing_policy_object` as well as a rule here: a
//! tenant may raise the period and never silently lower it. Validation compares
//! `effective_at` against the **announcement instant** — the scheduling commit —
//! plus the period, because that is the moment a subscriber could first have
//! learned of the change.

use std::fmt;

use toolkit_macros::domain_model;

use crate::domain::error::DomainError;
use crate::domain::instant::try_days;
use crate::domain::lifecycle::LifecycleState;
use crate::domain::plan_shape::PhaseKind;
use time::OffsetDateTime;

/// Wire code for a migration whose target is not a published plan
/// (`11-lifecycle.md` §5, `inst-mg-target`, 422).
pub const MIGRATION_TARGET_INVALID: &str = "MIGRATION_TARGET_INVALID";

/// Wire code for a migration scheduled inside the configured notice period
/// (`11-lifecycle.md` §5, D-49, 422).
pub const MIGRATION_NOTICE_TOO_SHORT: &str = "MIGRATION_NOTICE_TOO_SHORT";

/// Wire code for a migration refused by unresolved blocking deltas
/// (`11-lifecycle.md` §5, `inst-ms-deltas`, 422).
pub const MIGRATION_BLOCKED: &str = "MIGRATION_BLOCKED";

/// Wire code for a cancel of a run that has already completed
/// (`11-lifecycle.md` §5, D-34, 409).
///
/// The retired `MIGRATION_ALREADY_EFFECTIVE` names this fault elsewhere; see the
/// module doc for why the code is completion and not effectiveness.
pub const MIGRATION_COMPLETED: &str = "MIGRATION_COMPLETED";

/// Wire code for a retirement refused by a live migration aimed at the plan
/// (`11-lifecycle.md` §5, 409).
pub const RETIRE_TARGET_OF_MIGRATION: &str = "RETIRE_TARGET_OF_MIGRATION";

/// D-49's floor: the shortest notice period any tenant may configure.
///
/// Restated from `pricing_policy_object`'s
/// `chk_pricing_policy_object_notice_floor` rather than owned here — the store
/// is the enforcement point for what may be *written*, and this constant is what
/// lets a tenant with **no** policy row be governed by the same number.
pub const NOTICE_FLOOR_DAYS: i64 = 60;

// ---------------------------------------------------------------------------
// The state machine (§4).
// ---------------------------------------------------------------------------

/// The state of one migration schedule (`cpt-cf-bss-pricing-state-migration`).
#[domain_model]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum MigrationState {
    /// Deltas resolved, event emitted, nothing executed. The initial state.
    #[default]
    Scheduled,
    /// Subscriptions has declared execution start (`inst-mst-start`) and the
    /// D-36 re-validation has run. `PlanLink`s are being created.
    InProgress,
    /// Subscriptions has reported the scope processed (`inst-mst-complete`).
    /// **Terminal, and the only state a cancel cannot reach** (D-34).
    Completed,
    /// Invalidated before the effective date (`inst-mst-cancel`, M3) or stopped
    /// part-way (`inst-mst-cancel-inflight`, D-34). Terminal.
    Cancelled,
}

impl MigrationState {
    /// Every state, stable order.
    pub const ALL: &'static [Self] = &[
        Self::Scheduled,
        Self::InProgress,
        Self::Completed,
        Self::Cancelled,
    ];

    /// The persisted / wire token.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Scheduled => "scheduled",
            Self::InProgress => "in_progress",
            Self::Completed => "completed",
            Self::Cancelled => "cancelled",
        }
    }

    /// Read a stored token back.
    ///
    /// `None` rather than a default: the column's `CHECK` admits exactly these
    /// four, so a fifth is a corrupt row and resolving it to `Scheduled` would
    /// hide it behind the one state that still admits every transition.
    #[must_use]
    pub fn parse(token: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|s| s.as_str() == token)
    }

    /// Has this run reached a state nothing moves it out of?
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Cancelled)
    }

    /// May Subscriptions begin or continue processing against this record?
    ///
    /// The predicate `inst-mg-cancel`'s state handshake asks, before execution
    /// and per batch. `false` for both terminal states, and the two answers are
    /// different facts the executor treats identically: a `completed` run has
    /// nothing left to do and a `cancelled` one must stop having things to do.
    #[must_use]
    pub const fn admits_further_processing(self) -> bool {
        matches!(self, Self::Scheduled | Self::InProgress)
    }

    /// Is a move from `self` to `next` legal (§4)?
    #[must_use]
    pub const fn can_transition(self, next: Self) -> bool {
        matches!(
            (self, next),
            (Self::Scheduled, Self::InProgress | Self::Cancelled)
                | (Self::InProgress, Self::Completed | Self::Cancelled)
        )
    }

    /// Assert a transition, refusing every edge §4 does not sanction.
    ///
    /// There is deliberately **no self-edge**: a repeat `start` is not a second
    /// transition but a *replay* (D-65), and the caller that handles it must be
    /// the one that returns the persisted exclusion set rather than this
    /// function silently accepting the flip twice.
    ///
    /// # Errors
    /// [`DomainError::MigrationCompleted`] for a cancel of a `completed` run —
    /// D-34's one named refusal;
    /// [`DomainError::LifecycleForbidden`] for every other edge outside the four
    /// legal ones, including every self-edge and every move out of a terminal
    /// state.
    pub fn transition(self, next: Self) -> Result<(), DomainError> {
        if self.can_transition(next) {
            return Ok(());
        }
        if matches!((self, next), (Self::Completed, Self::Cancelled)) {
            return Err(DomainError::MigrationCompleted(format!(
                "migration has completed and cannot be cancelled; already-migrated subscriptions \
                 are unaffected either way ({self} -> {next})"
            )));
        }
        Err(DomainError::LifecycleForbidden(format!("{self} -> {next}")))
    }

    /// Refuse a cancel this run is past (`inst-mg-cancel`, D-34).
    ///
    /// Accepts `scheduled` **and** `in_progress` — that is the whole of what D-34
    /// changed — and refuses `completed` in its own words. A cancel of an
    /// already-`cancelled` run is refused as a lifecycle edge rather than as a
    /// completion, because the two say different things to the operator: one has
    /// finished doing work, the other was already stopped.
    ///
    /// # Errors
    /// [`DomainError::MigrationCompleted`] on a `completed` run;
    /// [`DomainError::LifecycleForbidden`] on an already-`cancelled` one.
    pub fn ensure_cancellable(self) -> Result<(), DomainError> {
        self.transition(Self::Cancelled)
    }
}

impl fmt::Display for MigrationState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

// ---------------------------------------------------------------------------
// Scheduling rules (§3 step 1, `inst-mg-target`).
// ---------------------------------------------------------------------------

/// The tenant's enforced-migration notice period (D-49, M5).
///
/// A newtype over days rather than a bare `i64` so the floor is applied at
/// construction and cannot be forgotten at a call site.
#[domain_model]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct NoticePeriod {
    days: i64,
}

impl NoticePeriod {
    /// The period a tenant configured, **raised to the floor** if it is below it.
    ///
    /// Raising rather than refusing is the fail-closed direction and it is
    /// deliberate: the column's `CHECK` already refuses a sub-floor *write*, so a
    /// value below the floor arriving here means the row predates the constraint
    /// or was written around it, and the safe reading of a corrupt notice period
    /// is the longest one rather than the stored one. D-49's sentence is that
    /// tenants "may raise, never silently lower" — lowering is the direction that
    /// costs a subscriber their warning.
    #[must_use]
    pub const fn resolved(configured_days: i64) -> Self {
        Self {
            days: if configured_days < NOTICE_FLOOR_DAYS {
                NOTICE_FLOOR_DAYS
            } else {
                configured_days
            },
        }
    }

    /// The deployment default for a tenant with no policy row.
    #[must_use]
    pub const fn floor() -> Self {
        Self {
            days: NOTICE_FLOOR_DAYS,
        }
    }

    /// The configured period in days.
    #[must_use]
    pub const fn days(self) -> i64 {
        self.days
    }

    /// The earliest instant a migration announced at `announced_at` may take
    /// effect.
    ///
    /// A configured period has no upper `CHECK` — the column only enforces
    /// `>= 60` — so this cannot assume the addition fits. `Duration::days`
    /// panics on its own internal overflow before the addition is even
    /// attempted (a large-enough `i64` day count overflows the
    /// seconds-multiply inside `chrono` itself), and the addition onto
    /// `announced_at` can separately walk outside chrono's representable
    /// range even for a `Duration` that itself constructed cleanly. Both are
    /// refused here rather than left to panic on a request path.
    ///
    /// # Errors
    /// [`DomainError::MigrationNoticeTooShort`]: an unrepresentable period
    /// **is** "too short" from the caller's vantage — no `effective_at` a
    /// request could carry can be at or past an earliest instant that cannot
    /// even be computed, so every request is inside the (unbounded) notice
    /// window.
    pub fn earliest_effective(
        self,
        announced_at: OffsetDateTime,
    ) -> Result<OffsetDateTime, DomainError> {
        try_days(self.days)
            .and_then(|notice| announced_at.checked_add(notice))
            .ok_or_else(|| {
                DomainError::MigrationNoticeTooShort(format!(
                    "this tenant's {} day migration notice period cannot be honoured: it does \
                     not fit within the representable date range from the announcement instant \
                     {announced_at:?}. There is no override on this request: a shorter migration \
                     requires an audited change to the tenant's notice policy first",
                    self.days
                ))
            })
    }

    /// Validate `effective_at` against this period (`inst-mg-target`, D-49).
    ///
    /// The comparison is against the **announcement** instant — the scheduling
    /// commit — because that is the moment a subscriber could first have learned
    /// of the change. The bound is inclusive: a migration exactly one notice
    /// period out has given the whole notice.
    ///
    /// # Errors
    /// [`DomainError::MigrationNoticeTooShort`] naming the configured period and
    /// the earliest admissible instant, because the operator's next act is
    /// either to pick a later date or to go and change the policy — and neither
    /// is reachable from a refusal that only says "too short".
    pub fn ensure_honoured(
        self,
        announced_at: OffsetDateTime,
        effective_at: OffsetDateTime,
    ) -> Result<(), DomainError> {
        let earliest = self.earliest_effective(announced_at)?;
        if effective_at >= earliest {
            return Ok(());
        }
        Err(DomainError::MigrationNoticeTooShort(format!(
            "effectiveAt {effective_at:?} is inside this tenant's {} day migration notice period; \
             the earliest admissible instant is {earliest:?}. There is no override on this \
             request: a shorter migration requires an audited change to the tenant's notice \
             policy first",
            self.days
        )))
    }
}

/// Refuse a target a migration may not land subscribers on
/// (`inst-mg-target`, §3 step 1).
///
/// The predicate is "could a subscriber have subscribed to this directly", which
/// is stricter than a foreign key could state and is why the schema carries no
/// FK on `target_plan_id`: a `draft` target has no consumer-resolvable content
/// at all, and a `retired` one blocks new subscriptions by definition, so
/// migrating onto either would produce `PlanLink`s pointing at something the
/// sellability gate refuses.
///
/// # Errors
/// [`DomainError::MigrationTargetInvalid`] naming the state the target is
/// actually in.
pub fn ensure_target_publishable(
    target_plan_id: uuid::Uuid,
    target_state: LifecycleState,
) -> Result<(), DomainError> {
    if target_state == LifecycleState::Published {
        return Ok(());
    }
    Err(DomainError::MigrationTargetInvalid(format!(
        "migration target plan {target_plan_id} stands at a {target_state} revision; a migration \
         may only land subscribers on a published plan"
    )))
}

/// Refuse a migration whose source and target are the same plan.
///
/// The domain half of `chk_pricing_migration_distinct_plans`, so the refusal
/// reaches the caller as a named code rather than as a driver constraint
/// violation. Reported in the hand-back as an addition to §6, which states the
/// two columns and says nothing about their relationship.
///
/// # Errors
/// [`DomainError::MigrationTargetInvalid`], because the target is the field the
/// operator must change.
pub fn ensure_distinct_plans(
    source_plan_id: uuid::Uuid,
    target_plan_id: uuid::Uuid,
) -> Result<(), DomainError> {
    if source_plan_id != target_plan_id {
        return Ok(());
    }
    Err(DomainError::MigrationTargetInvalid(format!(
        "migration source and target are both plan {source_plan_id}; a migration onto the plan \
         every subscriber is already on would ask Subscriptions to re-bind them to themselves"
    )))
}

// ---------------------------------------------------------------------------
// D-39: the phase a migrated subscription enters.
// ---------------------------------------------------------------------------

/// Which phase of the target a migrated subscription enters (D-39, confirmed, `inst-mg-boundary`).
///
/// **A migration never grants a new `trial`, and does grant `interim`.** The rule
/// is the catalog's to state and Subscriptions' to honour — it rides the
/// `PlanMigrationScheduled` contract — so what lives here is the selection, not
/// an enforcement: this gear owns no subscription to place.
///
/// The asymmetry is deliberate and worth keeping written down. A `trial` is an
/// acquisition instrument, granted once per subscriber to let them evaluate a
/// product they have not bought; a migrated subscriber has already bought, and
/// re-granting it would hand free time to every existing customer of a plan
/// being consolidated. An `interim` phase (`intro` until D-358) is a *price*
/// concession on a purchase that has been made, which is exactly what a migration
/// is imposing.
///
/// Returns the index of the first non-trial phase in the target's ordered phase
/// chain, or `None` when the chain is empty or entirely trial — the latter being
/// a target no migration can land on, which its caller surfaces as a boundary
/// delta rather than deciding here.
/// Takes [`PhaseKind`] rather than its tokens: the kinds are a closed set the
/// domain owns, and a `&[&str]` signature accepts a vocabulary it does not — the
/// literal `"trial"` here would go on matching nothing if the token were ever
/// respelled, and `"standard"` is not a kind at all.
#[must_use]
pub fn entry_phase_index(target_phase_kinds: &[PhaseKind]) -> Option<usize> {
    target_phase_kinds
        .iter()
        .position(|kind| *kind != PhaseKind::Trial)
}

#[cfg(test)]
#[path = "migration_tests.rs"]
mod migration_tests;
