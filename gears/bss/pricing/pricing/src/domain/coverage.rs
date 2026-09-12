//! `CoverageChecker` — the publish-time coverage rules of
//! `design/07-pricewindow-linkage.md` §3: `inst-wc-required`, `inst-wc-perkey`,
//! `inst-fg-detect` and `inst-wc-availability`, and the three §5 codes they
//! raise.
//!
//! Where [`crate::domain::window`] is about **one** window — its state machine,
//! its interval, the four refusals a single mutation can provoke — this module is
//! about a **key's whole window set** and about the plan those keys belong to.
//! That is the plane §5's remaining window codes live on, and that module's own
//! doc says so and names this one as their owner.
//!
//! # There is no second `KeyCoverage`, and that is deliberate
//!
//! The phase plan sketches a `KeyCoverage` carrying "the key, its ordered
//! intervals, its derived coverage end" with a `coverage_end()` and a `covers()`
//! of its own. **That type already exists**:
//! [`KeyWindows`](crate::domain::window::KeyWindows), built by G1, with
//! [`KeyWindows::coverage_end`](crate::domain::window::KeyWindows::coverage_end)
//! answering in a three-armed [`CoverageEnd`] and a writer already in the
//! projector. So [`KeyCoverage`] here is a *report entry* wrapped around one of
//! those and **every arithmetic answer it gives is a delegation**: the coverage
//! end is `KeyWindows`', and containment is
//! [`WindowInterval::covers`](crate::domain::window::WindowInterval::covers) —
//! the crate's single implementation of half-openness, which is what makes
//! adjacency legal in exactly one place.
//!
//! Two carriers of "when does this key's coverage end" would be free to
//! disagree, and one of them is already frozen into a projected payload
//! (D-121/D-167). The plan's sketch is followed in name and not in duplication;
//! the divergence is recorded here rather than left to be noticed.
//!
//! # "Billable" is exact, and today its overlay half is a no-op
//!
//! `inst-wc-required` binds *"every billable row's canonical scope key (resolved
//! on the base `priceOverlay`)"*. In this module that reads:
//!
//! - **a published-candidate row.** [`PlanShape::rows`] is already exactly that
//!   set — `infra::publish::assemble` builds it from `CANDIDATE_ROW_STATES` — so
//!   nothing here re-filters by `lifecycle_state`. `plan_shape.rs`'s own module
//!   doc gives the reason: a rule that re-derived "which rows count" is a second
//!   answer to a question the publish unit already answered, and the two are free
//!   to disagree the day the publish unit changes what it feeds in.
//! - **on the base overlay.** [`ScopeKey::new`] hard-codes
//!   `price_overlay: PriceOverlay::Base`, so no other value is *constructible*
//!   today and [`is_billable`]'s overlay test cannot be false. It is written
//!   anyway, for the reason `content_pin_tests.rs` gives for framing the same
//!   axis: the day Slice 9 adds an overlay, the rule already sees it.
//!
//! **What Slice 9 then owes, said loudly because the filter alone would hide
//! it.** An overlay row's key *resolves onto* the base overlay — that is what
//! `inst-wc-required`'s parenthesis means. So the day a non-base row exists, this
//! rule must **resolve** it onto its base key and check *that* key's coverage.
//! Merely skipping it, which is what the filter below does, would let an overlay
//! row publish with no coverage requirement at all — a discount plane selling
//! into a void. The filter is correct while non-base rows do not exist and is a
//! defect the moment they do.
//!
//! # Every charge kind is billable, per `inst-wc-perkey`
//!
//! No `ChargeKind` is excluded. `inst-wc-perkey` names the opposite obligation —
//! a hybrid's `recurring`, `usage` and `one_time_setup` components each carry
//! their **own** coverage, because each is its own canonical scope key (the
//! `chargeKind` axis exists precisely so they do not collide). Covering the
//! recurring key covers nothing else.

use toolkit_macros::domain_model;

use crate::domain::instant::format_rfc3339;
use crate::domain::money::CurrencyCode;
use crate::domain::plan_shape::{Frequency, PlanShape};
use crate::domain::price_record::PriceRecord;
use crate::domain::scope_key::{ChargeKind, PriceOverlay, Region, ScopeKey};
use crate::domain::validation::{ValidationReport, ValidationRule};
use crate::domain::window::{CoverageEnd, KeyWindows, WindowInterval, WindowState};
use time::OffsetDateTime;

/// A billable row's canonical scope key holds no active or scheduled window
/// (§5, **422 architectural**; `inst-wc-required`).
///
/// Declared by `design/07-pricewindow-linkage.md` §5's problem-response list —
/// *"`WINDOW_COVERAGE_MISSING` (422, names the scope key)"* — so this is a code
/// the design set names rather than one minted here.
///
/// It reaches the wire as a
/// [`Violation`](crate::domain::validation::Violation) inside the
/// `ValidationFailed` envelope, which the ladder renders as a **400** carrying
/// this string as the violation's `type_`. No `DomainError` variant and no new
/// mapping arm: the envelope already fans a report out one violation per line,
/// and a second home for the code would be a second answer to what a coverage
/// failure is.
pub const WINDOW_COVERAGE_MISSING: &str = "WINDOW_COVERAGE_MISSING";

/// Two of a key's windows leave an instant between them uncovered (§5, **422
/// architectural**; `inst-fg-detect`).
///
/// §5 declares it as *"`WINDOW_GAP` (422, names `[gapStart, gapEnd)`)"*, and the
/// report names exactly that pair and the key.
pub const WINDOW_GAP: &str = "WINDOW_GAP";

/// A cancel or an `effectiveTo` shortening would leave the key uncovered with no
/// successor (§5, **422 architectural**; `inst-fg-trailing`, D-62 → D-80 → D-94,
/// fail-closed under **D-182**).
///
/// # Why it is here and not beside the state machine's four
///
/// [`WindowRefusal`](crate::domain::window::WindowRefusal) is closed at four and
/// its members are all about **one window** — its start, its state, its frozen
/// content. This refusal is about a **key**: it is decided by the interval set of
/// every window on the key plus a margin derived from the plan, and the window the
/// caller named is merely the one whose removal exposed the hole. Its true
/// siblings are the two constants above it, both of which are also per-key
/// coverage rules.
///
/// # But unlike those two it is a [`DomainError`] and not a report violation
///
/// [`WINDOW_COVERAGE_MISSING`] and [`WINDOW_GAP`] are raised by registered publish
/// rules, which fan out through the validation envelope. This one is raised
/// **inside a window-mutating transaction** (`inst-fg-when`), where there is no
/// report to fan out into and the mutation must simply not happen — so it travels
/// as [`DomainError::WindowTrailingVoid`](crate::domain::error::DomainError::WindowTrailingVoid),
/// exactly as the phase's design asked. One code, one wire rendering, two raising
/// contexts is fine; two codes for one rule would not be.
pub const WINDOW_TRAILING_VOID: &str = "WINDOW_TRAILING_VOID";

/// A plan's purchasability interval reaches outside a billable key's coverage
/// (§5, **422 architectural**; `inst-wc-availability`, W5).
///
/// §5 declares it bare — *"`AVAILABILITY_OUTSIDE_COVERAGE` (422)"* — and W5 gives
/// the semantics: `availableFrom`/`availableTo` are **purchasability** dates
/// validated *against* coverage. They are not a publish scheduler, so a plan
/// offered for sale over an interval its prices are not effective across is an
/// authoring fault rather than a request to schedule one.
pub const AVAILABILITY_OUTSIDE_COVERAGE: &str = "AVAILABILITY_OUTSIDE_COVERAGE";

/// The window states that **contribute the coverage `inst-wc-required` and
/// `inst-fg-detect` range over**.
///
/// `inst-wc-required` says "an active or scheduled `PriceWindow`" and
/// `inst-fg-detect`'s stated input is "≥ 2 active/scheduled windows on one scope
/// key". Both, so one roster.
///
/// # Why `expired` is out, and why that is not the same set as `coverage_end`'s
///
/// [`KeyWindows::coverage_end`](crate::domain::window::KeyWindows::coverage_end)
/// counts `expired` intervals — deliberately, because "where does this key's
/// coverage end" must answer *last month* for a dormant key rather than
/// *nowhere*. These two rules must not, and the decisive reason is that
/// **including `expired` would produce a violation with no remedy**: the gap
/// between an expired window's end and the next scheduled start lies partly in
/// the past, and `inst-ws-future-start` (D-63) refuses a window whose
/// `effectiveFrom` is not strictly in the future. An author told to fill it could
/// not. The fail-closed direction is preserved elsewhere and not here: a key
/// whose only windows are expired has no *active* window, so sellability
/// predicate (1) refuses the sale, and rating on an uncovered instant fails
/// closed because no fallback pricing exists.
///
/// **And the same no-remedy argument is what keeps `inst-wc-availability`'s
/// `availableFrom` half off this roster**. That half asks
/// whether a **past** instant was covered, and there excluding `expired` is the
/// side that produces the unremediable refusal: the author would have to open a
/// window over an instant `inst-ws-future-start` forbids. So this roster is the
/// live-coverage one and [`KeyCoverage::covers_at`] is the past-instant one, and
/// which of the two a rule reads follows from whether its findings are remediable
/// — not from which of them came first.
///
/// # Why it is not derived from `window_repo`'s `OCCUPYING_STATES`
///
/// That constant names the same two states and answers a different question —
/// which intervals *compete for a key* under `WINDOW_OVERLAP`. Deriving one from
/// the other is the trap `BillingCycle::admits_setup_row` refuses one module
/// over: two sentences of the design set that happen to name the same values
/// today, where a later divergence would become a silent change to the wrong
/// rule. It is also in `infra`, which dylint DE0301 keeps out of here.
const COVERING_STATES: &[WindowState] = &[WindowState::Scheduled, WindowState::Active];

/// Does this interval contribute the coverage the two rules range over?
fn contributes(interval: &WindowInterval) -> bool {
    COVERING_STATES.contains(&interval.state)
}

/// Is this row one `inst-wc-required` demands coverage for?
///
/// See the module doc for both halves — the candidate set is the caller's and the
/// overlay test is a no-op today with a named obligation attached.
#[must_use]
pub fn is_billable(record: &PriceRecord) -> bool {
    record.scope_key.price_overlay() == PriceOverlay::Base
}

/// One canonical scope key's coverage, as the rules and the operator's report
/// read it.
///
/// It **holds** a [`KeyWindows`] rather than restating one, and every derived
/// answer below forwards into it or into
/// [`WindowInterval::covers`](crate::domain::window::WindowInterval::covers). The
/// only fact of its own is [`KeyCoverage::required`], which is a statement about
/// the *plan* — whether a billable row sits on this key — and cannot be read off
/// a window set at all.
#[domain_model]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyCoverage {
    /// The key and its ordered intervals, every state included.
    pub windows: KeyWindows,
    /// Does a billable row of the subject sit on this key?
    ///
    /// `false` on a key that only history occupies — a `superseded`
    /// predecessor's key that no candidate row is filed under. Such a key still
    /// appears in the report, because an operator remediating coverage needs to
    /// see the intervals that exist, but `inst-wc-required` says nothing about
    /// it: there is no row on it whose publish could be refused.
    pub required: bool,
}

impl KeyCoverage {
    /// The ten axes this coverage is filed under.
    #[must_use]
    pub const fn scope_key(&self) -> &ScopeKey {
        &self.windows.scope_key
    }

    /// How far the key's coverage runs — [`KeyWindows::coverage_end`], forwarded.
    ///
    /// Forwarded and not recomputed: that answer is frozen into a projected
    /// payload (D-121, D-167 clause 1) and a second derivation of it here would
    /// be a second answer to a fact a consumer has already been given.
    #[must_use]
    pub fn coverage_end(&self) -> CoverageEnd {
        self.windows.coverage_end()
    }

    /// Does the key hold an active or scheduled window at all
    /// (`inst-wc-required`)?
    ///
    /// **Not `coverage_end() != Uncovered`**, and the difference is the whole
    /// rule: a key whose only window is `expired` has a coverage end in the past
    /// and no live window, so it answers `true` there and `false` here. Spelling
    /// this as a test on the coverage end would let a dormant key publish.
    #[must_use]
    pub fn has_live_window(&self) -> bool {
        self.windows.intervals.iter().any(contributes)
    }

    /// Is `at` inside the key's **contributing** coverage?
    ///
    /// Every instant question goes through
    /// [`WindowInterval::covers`](crate::domain::window::WindowInterval::covers),
    /// so the half-open `[from, to)` reading has one implementation in the crate.
    ///
    /// # Its one reader among the rules is [`Self::interior_gaps`], and the other
    /// question has [`Self::covers_at`]
    ///
    /// This filters on `COVERING_STATES`, so it is the right answer only where the
    /// instant asked about is one an author can still schedule a window over — the
    /// gap walk, whose findings are remediable by construction. A question about the
    /// **past** must not filter on the state token, and asking it here is the defect
    /// review H5 found in the availability rule.
    #[must_use]
    pub fn covers(&self, at: OffsetDateTime) -> bool {
        self.windows
            .intervals
            .iter()
            .filter(|i| contributes(i))
            .any(|i| i.covers(at))
    }

    /// Is `at` inside the key's coverage, **counting the intervals that have run
    /// out** — [`KeyWindows::covers_at`](crate::domain::window::KeyWindows::covers_at),
    /// forwarded?
    ///
    /// Forwarded and not recomputed, for [`Self::coverage_end`]'s reason: the past
    /// instant question has one implementation in the crate and it is the one
    /// sellability's predicate (1) already reads.
    ///
    /// # Why the availability rule asks this one and the gap walk does not
    ///
    /// It differs from [`Self::covers`] on exactly one input — an `expired` interval
    /// containing `at` — and that input is the whole of `inst-wc-availability`'s
    /// `availableFrom` half: a plan that has been on sale since a past instant was
    /// covered at that instant by a window the activation sweep has since expired.
    /// Filtering it out made the rule refuse every such re-publish **with no
    /// remedy**, since `inst-ws-future-start` (D-63) forbids opening a window over a
    /// past instant, while `AvailableFromNotBackdated` (`inst-cs-availability`) is
    /// written to permit exactly that re-publish. Two rules of one pipeline
    /// disagreeing about one publish is what makes this a second predicate rather
    /// than a widening of the first: `interior_gaps` must keep the live roster,
    /// because a gap it reported over an expired interval would be the same
    /// no-remedy refusal one rule over — which is `COVERING_STATES`' own argument
    /// for excluding `expired`.
    #[must_use]
    pub fn covers_at(&self, at: OffsetDateTime) -> bool {
        self.windows.covers_at(at)
    }

    /// The uncovered intervals **between** two of the key's windows
    /// (`inst-fg-detect`), each as `[gapStart, gapEnd)`, ordered by `gapStart`.
    ///
    /// # It cannot see a void with no successor, and that is the point
    ///
    /// A gap is reported only where some window **starts after** the uncovered
    /// instant. So a key whose last window simply ends — a cancellation, or an
    /// `effectiveTo` shortened past the end of coverage — produces **no finding
    /// here at any time**. That is not a limitation to be fixed: it is the entire
    /// reason `inst-fg-trailing` (D-62) exists, and D-62's own problem statement
    /// says so — *"invisible to `inst-fg-detect` — an interior check that compares
    /// each window to its successor and by construction cannot see a void with no
    /// successor"*. `WINDOW_TRAILING_VOID` is the other rule and is **not** this
    /// one widened; it needs two inputs this has none of, W6's cycle and the D-79
    /// subscriber lane. A later change that made this walk report a trailing void
    /// would give one refusal two codes and two owners.
    ///
    /// # How half-openness enters, and which of the two tests carries it
    ///
    /// A gap opens at an interval's exclusive end **iff nothing covers that
    /// instant** — asked of [`KeyCoverage::covers`], so adjacency is decided by
    /// the same `[from, to)` reading as everywhere else: with
    /// `effectiveTo == next.effectiveFrom` the successor covers the boundary
    /// instant and no gap is reported. §9 names that false positive as one the
    /// implementation must not produce, and an inclusive end would refuse the
    /// shape every supersession and every cutover produces.
    ///
    /// **The two filters are not redundant, and which one refuses what was
    /// measured rather than argued.** On a key holding exactly two adjacent
    /// windows the containment test is dead: `next_start_after`'s strict `>`
    /// already refuses the boundary, because a successor does not start *after*
    /// the instant it starts at. Containment becomes load-bearing as soon as a
    /// **third, later** window exists — then a false gap has somewhere to end,
    /// and dropping the filter reports `[3,8)` on `[1,3) [3,5) [8,10)` and
    /// `[5,8)` on `[1,5) [3,7) [8,10)`. Those two shapes are the tests that hold
    /// this filter; the two-window adjacency case, which is §9's own criterion,
    /// does not and says so.
    #[must_use]
    pub fn interior_gaps(&self) -> Vec<(OffsetDateTime, OffsetDateTime)> {
        let mut ends: Vec<OffsetDateTime> = self
            .windows
            .intervals
            .iter()
            .filter(|i| contributes(i))
            .filter_map(|i| i.effective_to)
            .collect();
        ends.sort_unstable();
        ends.dedup();

        ends.into_iter()
            .filter(|end| !self.covers(*end))
            .filter_map(|end| self.next_start_after(end).map(|next| (end, next)))
            .collect()
    }

    /// The earliest contributing start strictly after `at`, when there is one.
    ///
    /// The successor half of `interior_gaps`: its `None` is precisely the
    /// trailing void that walk cannot see.
    fn next_start_after(&self, at: OffsetDateTime) -> Option<OffsetDateTime> {
        self.windows
            .intervals
            .iter()
            .filter(|i| contributes(i))
            .map(|i| i.effective_from)
            .filter(|from| *from > at)
            .min()
    }
}

/// The whole plan's coverage, one entry per canonical scope key.
///
/// Ordered by the key's canonical rendering, so the operator's report and two
/// runs over the same facts read identically —
/// [`group_by_key_seeded`](crate::domain::window::group_by_key_seeded)'s promise,
/// preserved rather than re-derived.
#[domain_model]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CoverageReport {
    /// Every key: the ones a billable row sits on, and the ones only windows
    /// mention.
    pub keys: Vec<KeyCoverage>,
}

impl CoverageReport {
    /// The keys `inst-wc-required` demands a live window on.
    pub fn required(&self) -> impl Iterator<Item = &KeyCoverage> {
        self.keys.iter().filter(|entry| entry.required)
    }

    /// This key's entry, when the report carries one.
    #[must_use]
    pub fn find(&self, key: &ScopeKey) -> Option<&KeyCoverage> {
        self.keys.iter().find(|entry| entry.scope_key() == key)
    }
}

/// The coverage of every key the plan touches: those `billable` names, and those
/// `windows` mentions.
///
/// **The two inputs are separate on purpose.** At publish they are the candidate
/// rows' keys and `PlanShape::windows`; at `GET …/coverage` they are the
/// *published* rows' keys and a fresh store read, because that surface answers
/// about a plan that may have no open draft revision at all and so cannot go
/// through `assemble`. One function, two callers, no second definition of what
/// coverage is.
///
/// **A billable key the window plane does not mention gets an entry with no
/// intervals**, whose [`KeyCoverage::coverage_end`] is
/// [`CoverageEnd::Uncovered`] and whose [`KeyCoverage::has_live_window`] is
/// `false`. That is done here rather than left to the caller's seeding, for
/// `group_by_key_seeded`'s own reason: if "this key is uncovered" were spelled as
/// the *absence* of an entry, a caller that forgot to seed would silently lose
/// the finding, and the answer would have two spellings.
#[must_use]
pub fn check(billable: &[ScopeKey], windows: &[KeyWindows]) -> CoverageReport {
    let mut keys: Vec<KeyCoverage> = windows
        .iter()
        .map(|group| KeyCoverage {
            windows: group.clone(),
            required: billable.contains(&group.scope_key),
        })
        .collect();
    for key in billable {
        if !keys.iter().any(|entry| entry.scope_key() == key) {
            keys.push(KeyCoverage {
                windows: KeyWindows {
                    scope_key: key.clone(),
                    intervals: Vec::new(),
                },
                required: true,
            });
        }
    }
    keys.sort_by_key(|entry| entry.scope_key().to_string());
    CoverageReport { keys }
}

/// Will this publish open the key's coverage itself (D-332)?
///
/// True when every billable row on the key is a **draft** this publish is about
/// to freeze. A key that already carries a published row is not this case: its
/// coverage existed and stopped, which is a gap an author has to answer for.
#[must_use]
pub fn opened_by_this_publish(shape: &PlanShape, key: &ScopeKey) -> bool {
    let mut seen = false;
    for record in shape
        .rows
        .iter()
        .filter(|record| is_billable(record) && &record.scope_key == key)
    {
        if record.lifecycle_state != crate::domain::lifecycle::LifecycleState::Draft {
            return false;
        }
        seen = true;
    }
    seen
}

/// The billable keys of a publish subject, in `rows` order and de-duplicated.
///
/// Two rows legitimately share a key — a `superseded` predecessor and its
/// `published` successor — so the set is what the rules range over, never the row
/// list.
#[must_use]
pub fn billable_keys(shape: &PlanShape) -> Vec<ScopeKey> {
    let mut keys: Vec<ScopeKey> = Vec::new();
    for record in shape.rows.iter().filter(|record| is_billable(record)) {
        if !keys.contains(&record.scope_key) {
            keys.push(record.scope_key.clone());
        }
    }
    keys
}

/// The coverage of a publish subject — [`check`] over the subject's own two
/// halves.
#[must_use]
pub fn check_shape(shape: &PlanShape) -> CoverageReport {
    check(&billable_keys(shape), &shape.windows)
}

// ---------------------------------------------------------------------------
// W6's term
// ---------------------------------------------------------------------------

/// W6's margin: the longest billing cycle the plan sells on `(currency, region)`.
///
/// # The name lies, and the reduction is the design set's own
///
/// W6 *calls* it "the longest billing cycle sold on the **key**" and *defines* it
/// **per plan** — "the longest `frequency` among the plan's **recurring** rows on
/// the key's `(currency, region)`" — with its own reason: a `usage`, `one_time` or
/// `one_time_setup` key carries no `frequency` at all, so read literally the term
/// was undefined on most keys of a hybrid. §1.6 matches it to D-121's `H` ("2 ×
/// the longest cycle sold on the plan").
///
/// **Reported divergence:** W6's phrasing presumes a **per-row** frequency, and
/// this schema does not carry one. `frequency` is a field of [`PlanShape`] and
/// not of `PriceRow` — one proration/anchor contract per plan-market, per D-123 —
/// so "the longest among the plan's recurring rows" is a maximum over a set whose
/// members all carry the same value, i.e. that value. The reduction is sound
/// rather than a shortcut, and it is recorded rather than silently absorbed.
///
/// # Three answers, and `None` is not zero
///
/// - **`Some(zero)`** — the plan carries no `recurring` row on that market. W6's
///   own words: *"On a plan with no recurring part … the margin is **zero** — a
///   one-time purchase needs no forward coverage"*, and the horizon predicate
///   reduces to "an active window covers `t`".
/// - **`Some(d)`** — the plan's `frequency`, as a duration.
/// - **`None`** — the market carries a `recurring` row and the plan authored no
///   `frequency`, so the term has **no value**. A caller enforcing a margin must
///   refuse rather than read it as zero.
///
/// The phase plan's sketch glosses `None` as "a zero margin", which would collapse
/// the first and third of those. That is the collapse
/// [`CoverageEnd`](crate::domain::window::CoverageEnd) refused for the same
/// reason one type over — the two readings are opposites under a fail-closed
/// predicate — so the gloss is not followed and the divergence is reported.
///
/// The third case is **unreachable from the publish path**: `inst-cs-declared`'s
/// `CYCLE_METADATA_MISSING` refuses a `recurring`/`hybrid` plan with no
/// frequency, so no such subject publishes. It is reachable in principle for a
/// caller on another path (a `one_time` plan carrying a recurring row), which is
/// why the arm exists and answers `None` instead of guessing.
#[must_use]
pub fn longest_cycle_sold(
    shape: &PlanShape,
    currency: &CurrencyCode,
    region: &Region,
) -> Option<time::Duration> {
    longest_cycle_sold_on(
        shape.rows.iter().map(|record| &record.scope_key),
        shape.frequency,
        currency,
        region,
    )
}

/// [`longest_cycle_sold`] over the two facts it actually reads — the keys the
/// plan publishes and the plan's frequency.
///
/// **One implementation, two entry points**, the shape
/// [`interval_is_non_empty`](crate::domain::window::interval_is_non_empty) and
/// `pin_frontier_repo::read_at` both take, and it exists because the term's second
/// consumer holds no [`PlanShape`]: the sellability surface reads a **frozen
/// delta**, whose keys and frequency are the same two facts under different
/// carriers. A second computation of W6's term would be a second answer to a
/// margin three fail-closed rules are enforced against.
///
/// The keys are the plan's **whole** published set on the market, not the
/// eligibility-resolved subset a gate walks: W6 says "the plan's recurring rows",
/// and a grandfathered generation is a recurring row the plan still bills. The
/// publish-side caller passes every candidate row's key for the same reason.
#[must_use]
pub fn longest_cycle_sold_on<'a>(
    keys: impl IntoIterator<Item = &'a ScopeKey>,
    frequency: Option<Frequency>,
    currency: &CurrencyCode,
    region: &Region,
) -> Option<time::Duration> {
    let sells_recurring = keys.into_iter().any(|key| {
        key.charge_kind() == ChargeKind::Recurring
            && key.currency() == currency
            && key.region() == region
    });
    if !sells_recurring {
        return Some(time::Duration::ZERO);
    }
    frequency.map(cycle_length)
}

/// One frequency as a duration, rounded **up** to its calendar maximum.
///
/// Every consumer of this value is a **margin** — D-04's copy-window bound, D-80's
/// coverage horizon, `inst-fg-trailing`'s floor — and a margin rounded down leaves
/// the tail of a bound period uncovered, which is exactly the hole D-04 exists to
/// close. So a month contributes 31 days and a year 366, not an average: the
/// question these callers ask is "could a period that started before the bound
/// still be running", and the answer has to hold for the longest such period.
///
/// **This gear computes no charges**, so this is not billing arithmetic and does
/// not have to agree with a proration basis. It is a lower bound on how long
/// coverage must outlast a bound period.
///
/// `n` on a custom frequency is the **authored** interval: `plan_repo` rebuilds it
/// from `custom_interval_n` / `custom_interval_unit` at the storage boundary, so
/// the value reaching here is data. [`Frequency::ALL`]'s custom member carries
/// [`Frequency::CUSTOM_INTERVAL_PLACEHOLDER`] and is a *variant representative*,
/// not an interval — handing a member of that list to this function would compute
/// a one-day margin for whatever the plan actually authored, and no call site does.
fn cycle_length(frequency: Frequency) -> time::Duration {
    /// The longest a calendar month can run.
    const LONGEST_MONTH: i64 = 31;
    /// The longest a calendar year can run.
    const LONGEST_YEAR: i64 = 366;

    match frequency {
        Frequency::Monthly => time::Duration::days(LONGEST_MONTH),
        Frequency::Quarterly => time::Duration::days(3 * LONGEST_MONTH),
        Frequency::Semiannual => time::Duration::days(6 * LONGEST_MONTH),
        Frequency::Annual => time::Duration::days(LONGEST_YEAR),
        Frequency::CustomEveryN { n, unit } => {
            let n = i64::from(n);
            match unit {
                crate::domain::plan_shape::CustomIntervalUnit::Days => time::Duration::days(n),
                crate::domain::plan_shape::CustomIntervalUnit::Months => {
                    time::Duration::days(n.saturating_mul(LONGEST_MONTH))
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The registered rules
// ---------------------------------------------------------------------------

/// Slice 7's own publish pipeline: the three coverage rules, in reading order.
///
/// **Its own pipeline rather than an arm of `foundation_plan_rules`**, whose doc
/// says the next *Foundation* rule registers beside it "instead of being appended
/// to a slice's set". Coverage is Slice 7's, not the Foundation's, and it is not
/// Slice 2's either — so a third pipeline, in the shape
/// `crate::domain::publish::rules::GrandfatherHorizonOnItsClass` already argues
/// for a slice rule that cannot live in the slice-2 set.
///
/// **Three rules and not one**, which is a testability decision rather than a
/// structural one: each of the three instructions is then removable on its own,
/// and "exactly one test reddens" stays legible for each. One rule owning all
/// three would have one registration and three reasons to redden.
///
/// The cost is that each rule builds the report for itself — three passes over a
/// key set bounded by the §14 per-plan soft cap. A shared build would need a
/// carrier between rules, which [`ValidationPipeline`](crate::domain::validation::ValidationPipeline)
/// deliberately does not have: a rule that carried results between runs is the
/// one thing §4.2's twice-run clause cannot survive.
#[must_use]
pub fn window_coverage_rules(
    opens_initial_coverage: bool,
) -> crate::domain::validation::ValidationPipeline<PlanShape> {
    crate::domain::validation::ValidationPipeline::new()
        .with_rule(Box::new(KeyCoverageRequired {
            opens_initial_coverage,
        }))
        .with_rule(Box::new(NoInteriorGap))
        .with_rule(Box::new(AvailabilityInsideCoverage))
}

/// Every billable row's key holds an active or scheduled window
/// (`inst-wc-required`, `inst-wc-perkey`).
///
/// **No silent fallback**, which is the instruction's own clause and the reason
/// the refusal is a publish blocker rather than a warning: Tariffs step 2
/// resolves a price by finding the window covering `t`, and on an uncovered key it
/// resolves *nothing*. There is no fallback price to fall back to, so the
/// alternative to refusing the publish is a plan that sells and cannot be rated.
///
/// Reported **once per uncovered key**, naming the key, because scheduling one
/// window is the edit — not once per row, which would name the same remedy twice
/// for two rows sharing a key.
#[domain_model]
#[derive(Clone, Copy, Debug, Default)]
pub struct KeyCoverageRequired {
    /// **Will the caller open coverage for a key it is freezing?** (D-332)
    ///
    /// Carried on the rule because `ValidationRule::evaluate` is handed the
    /// subject and nothing else, and this is not a fact about the subject: a
    /// draft row's key looks identical whether a publish is about to cover it or
    /// a repricing apply is merely re-judging it. False is the fail-closed
    /// default — a caller that says nothing gets the refusal.
    pub opens_initial_coverage: bool,
}

impl ValidationRule<PlanShape> for KeyCoverageRequired {
    /// `inst-wc-required` — **and this rule holds `inst-wc-perkey` too**, which
    /// makes it the one place in this set where `rule_names()` is not 1:1 with the
    /// instruction ids the module doc names.
    ///
    /// The fold is deliberate: `inst-wc-perkey`'s obligation — no `ChargeKind` is
    /// excluded — is a property of the same walk over the same key set, so
    /// splitting it out would be two rules asking one question twice. It is
    /// recorded here rather than left to be noticed because a census written over
    /// this set from the design document's ids would look for four names, find
    /// three, and be "fixed" by weakening it against a pipeline that is correct.
    /// The register of what this module holds is `coverage.rs`'s module doc; the
    /// register of what it *names* is `rule_names()`, and they are one entry
    /// apart on purpose.
    fn name(&self) -> &'static str {
        "inst-wc-required"
    }

    fn evaluate(&self, subject: &PlanShape, report: &mut ValidationReport) {
        let coverage = check_shape(subject);
        for entry in coverage.required() {
            if entry.has_live_window() {
                continue;
            }
            // **D-332: a key whose rows this publish is freezing for the first
            // time is covered by the publish itself**, which opens a window on
            // it at the commit instant. Skipped here so the *submit* arm agrees
            // with the commit — the rules run on both, and a refusal at submit
            // would mean the unit never opens and the commit is never reached.
            //
            // The rule keeps its whole force on every other key: one carrying a
            // row that is **already published** has had coverage and lost it,
            // which is an author's mistake and not an artefact of ordering.
            // That distinction is the reason this is a filter on the key's rows
            // rather than a flag on the publish.
            if self.opens_initial_coverage && opened_by_this_publish(subject, entry.scope_key()) {
                continue;
            }
            report.violate(
                WINDOW_COVERAGE_MISSING,
                entry.scope_key().to_string(),
                format!(
                    "no active or scheduled PriceWindow covers scope key {}: a billable row on \
                     an uncovered key resolves no price at any instant, and no fallback pricing \
                     exists. Schedule a window on the key before publishing",
                    entry.scope_key()
                ),
            );
        }
    }
}

/// No two of a key's windows leave an instant between them uncovered
/// (`inst-fg-detect`).
///
/// It ranges over **every** key the report carries, not only the required ones: a
/// gap on a key held by a `superseded` predecessor and its successor is a gap
/// rating falls into at an instant inside it, and which row is current on the key
/// says nothing about whether the interval set is sound.
#[domain_model]
#[derive(Clone, Copy, Debug, Default)]
pub struct NoInteriorGap;

impl ValidationRule<PlanShape> for NoInteriorGap {
    fn name(&self) -> &'static str {
        "inst-fg-detect"
    }

    fn evaluate(&self, subject: &PlanShape, report: &mut ValidationReport) {
        let coverage = check_shape(subject);
        for entry in &coverage.keys {
            for (start, end) in entry.interior_gaps() {
                report.violate(
                    WINDOW_GAP,
                    entry.scope_key().to_string(),
                    format!(
                        "scope key {} is uncovered over [{}, {}): every instant a billable \
                         period can fall in must be covered, and no fallback pricing exists for \
                         the ones inside this interval",
                        entry.scope_key(),
                        format_rfc3339(start),
                        format_rfc3339(end)
                    ),
                );
            }
        }
    }
}

/// The plan's purchasability interval lies inside every billable key's coverage
/// (`inst-wc-availability`, W5).
///
/// # The bound is plan-level and the coverage is per key, so the rule is a
/// conjunction
///
/// `available_from`/`available_to` are fields of the plan; coverage is per
/// canonical scope key. A purchase binds **every** key of the plan on the bound
/// market (D-94, `inst-sg-conjunction`), so a plan offered until `availableTo`
/// while one component key's coverage stops earlier is a plan that sells a line
/// it cannot rate. One key short makes the plan unpublishable.
///
/// # What is checked, and what an unset bound means
///
/// - **`availableTo`**: coverage must reach it. `OpenEnded` passes trivially;
///   `Ends(e)` needs `e >= availableTo`, the end being exclusive on both sides.
/// - **`availableFrom`**: it must itself be covered, asked of
///   [`KeyCoverage::covers_at`] — the predicate that counts an interval which has
///   run out, which is the same roster [`KeyCoverage::coverage_end`] above it
///   reads. **Both halves must ask the same interval set.** With the
///   `availableFrom` half asking the live one, a plan on sale long enough for its
///   first window to expire is refused `AVAILABILITY_OUTSIDE_COVERAGE` on every
///   later re-publish, with no edit available that would satisfy it —
///   `inst-ws-future-start` refuses a window over a past instant, so the only
///   remedy left is falsifying a date `AvailableFromNotBackdated`
///   (`inst-cs-availability`) deliberately permits re-publishing. One rule of this
///   pipeline permitting what another refuses is the defect, and the roster is what
///   avoids it; the fail-closed direction is untouched because a key whose
///   windows have **all** expired holds no live window and is refused by
///   `inst-wc-required` before this rule looks at it.
/// - **Either unset**: not checked, which is W5's own "(when set)". The residue is
///   real and is named rather than papered over: with `availableFrom` unset the
///   plan is purchasable from publish while its coverage may start later, and what
///   refuses a sale in that interval is sellability predicate (1) — an **active**
///   window covering `t`, which a not-yet-started window is not. That is the
///   fail-closed direction, so the gap in this rule costs a clearer error message
///   rather than a sale into a void.
/// - **An uncovered key is skipped here.** `WINDOW_COVERAGE_MISSING` already names
///   it and the remedy is the same edit; a second line would tell the author to do
///   one thing twice.
///
/// Gap-freeness is presumed, exactly as
/// [`KeyWindows::coverage_end`](crate::domain::window::KeyWindows::coverage_end)
/// presumes it: "covered at the start, and coverage reaches the end" is the whole
/// interval only if there is no hole in between, and [`NoInteriorGap`] is what
/// refuses one. The presumption is safe **because** that rule runs in the same
/// pipeline, and it is written down because it is a dependency rather than an
/// assumption about the data.
#[domain_model]
#[derive(Clone, Copy, Debug, Default)]
pub struct AvailabilityInsideCoverage;

impl ValidationRule<PlanShape> for AvailabilityInsideCoverage {
    fn name(&self) -> &'static str {
        "inst-wc-availability"
    }

    fn evaluate(&self, subject: &PlanShape, report: &mut ValidationReport) {
        if subject.available_from.is_none() && subject.available_to.is_none() {
            return;
        }
        let coverage = check_shape(subject);
        for entry in coverage.required() {
            if !entry.has_live_window() {
                continue;
            }
            if let Some(from) = subject.available_from
                && !entry.covers_at(from)
            {
                report.violate(
                    AVAILABILITY_OUTSIDE_COVERAGE,
                    entry.scope_key().to_string(),
                    format!(
                        "the plan is purchasable from {} and no window of scope key {} covers \
                         that instant: availability dates gate purchase, they do not schedule a \
                         window",
                        format_rfc3339(from),
                        entry.scope_key()
                    ),
                );
            }
            if let Some(to) = subject.available_to
                && let CoverageEnd::Ends(end) = entry.coverage_end()
                && end < to
            {
                report.violate(
                    AVAILABILITY_OUTSIDE_COVERAGE,
                    entry.scope_key().to_string(),
                    format!(
                        "the plan is purchasable until {} and the coverage of scope key {} ends \
                         at {}: a purchase made in between binds a line no window makes \
                         effective",
                        format_rfc3339(to),
                        entry.scope_key(),
                        format_rfc3339(end)
                    ),
                );
            }
        }
    }
}

#[cfg(test)]
#[path = "coverage_tests.rs"]
mod coverage_tests;
