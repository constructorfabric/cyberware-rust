//! `DomainError` — the gear's internal rejection vocabulary.
//!
//! Every variant is mapped to the AIP-193
//! [`toolkit_canonical_errors::CanonicalError`] envelope by the SINGLE
//! authoritative ladder in [`crate::infra::error_mapping`], so a domain
//! rejection is assigned a canonical category in exactly one place and both
//! surfaces — REST and the in-process client — agree by construction.
//!
//! The Foundation owns the problem types listed in `design/01-foundation.md`
//! §3.3; slices **reference** them and never redefine them, which is why they
//! live here rather than beside the surface that raises them. The wire codes
//! (`DUPLICATE_SCOPE_KEY`, `AMOUNT_NEGATIVE`, …) are the machine-readable
//! discriminators a consumer matches after the coarse category; they are the
//! names the design set uses, verbatim.

use toolkit_macros::domain_model;

use crate::domain::validation::ValidationReport;

/// A catalog operation rejection.
///
/// Fail-closed is the rule the taxonomy encodes: there is no variant that means
/// "proceeded with a default". An absent required field is a publish failure
/// ([`DomainError::ValidationFailed`]), never a downstream substitution.
#[domain_model]
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum DomainError {
    // -- InvalidArgument (bad request shape / value) --
    /// A request the gear cannot interpret at all — malformed ids, an unknown
    /// enum value, a missing required body field.
    #[error("invalid request: {0}")]
    InvalidRequest(String),

    // -- FailedPrecondition (state forbids the operation) --
    /// A published row resolves neither a row-level `rounding_policy_ref` nor a
    /// tenant default. Rounding decides the last minor unit of every charge, so
    /// an unresolved policy fails publish rather than picking one.
    #[error("rounding policy unresolved: {0}")]
    RoundingPolicyUnresolved(String),
    /// An amount carries more precision than the currency's ISO 4217 minor
    /// unit. There is no flat two-decimal rule: JPY takes 0, BHD takes 3.
    #[error("precision exceeds the currency minor unit: {0}")]
    PrecisionExceeded(String),
    /// An authored instant carries finer precision than the millisecond quantum
    /// every instant this gear publishes or compares is expressed at (D-144).
    ///
    /// Refused rather than truncated, which is what an unstated quantum
    /// degenerates into. Truncation moves the instant a scope-key axis is
    /// matched on for equality across a gear boundary, so a truncating producer
    /// and a non-truncating consumer agree until the day they do not, with no
    /// failure in between — and the generation is then unfindable by exactly the
    /// subscribers grandfathering retains. The money-side sibling
    /// [`DomainError::PrecisionExceeded`] takes the same posture for the same
    /// reason.
    #[error("timestamp precision exceeds the millisecond quantum: {0}")]
    TimestampPrecisionExceeded(String),
    /// A negative amount. Typed credit rows are deliberately Future scope, so a
    /// negative price is a mistake, not an unsupported feature.
    #[error("amount must be >= 0: {0}")]
    AmountNegative(String),
    /// A currency code that is not valid ISO 4217.
    #[error("invalid ISO 4217 currency: {0}")]
    CurrencyInvalid(String),
    /// Publish of a `modelKind` (or a non-`sum` aggregation row) that no green
    /// joint fixture gates. The corpus is the conformance contract with Rating;
    /// publishing past a missing fixture would ship a shape no one has agreed
    /// how to evaluate.
    #[error("no green conformance fixture: {0}")]
    FixtureMissing(String),
    /// The state machine forbids the transition — mutating a published row's
    /// content, superseding a grandfathered row.
    ///
    /// Narrowed by D-146 to exactly the refusals that describe **no alternative
    /// action**. The two it used to swallow are back out:
    /// [`DomainError::PlanRetiredNoSuccessor`] and
    /// [`DomainError::OpenDraftRevisionExists`] are refusals an operator acts on
    /// differently, and a consumer that had to parse prose to tell them apart
    /// could not act on either.
    #[error("lifecycle transition forbidden: {0}")]
    LifecycleForbidden(String),
    /// A revision was opened on, or a publish attempted for, a plan that is
    /// retired.
    ///
    /// A stop, not a retry and not an alternative: retirement is terminal, so a
    /// successor would be unpublishable from the moment it was opened. Told as
    /// [`DomainError::LifecycleForbidden`] this was indistinguishable from
    /// refusals the operator can work around, and the only remedy the shared
    /// sentence implied was the very call being refused.
    #[error("plan is retired and takes no successor revision: {0}")]
    PlanRetiredNoSuccessor(String),
    /// An authoring call named a plan whose **every** revision is `abandoned`.
    ///
    /// It holds no current revision and no open draft and can acquire neither:
    /// a first draft is minted at revision `0` outright, and a successor
    /// presupposes a current revision to succeed from — so a plan created and
    /// discarded before its first publish has spent its id (D-145 as amended;
    /// `02-plan-definition.md` §5, `01-foundation.md` §3.3).
    ///
    /// Deliberately not [`DomainError::PlanRetiredNoSuccessor`], which would
    /// assert a retirement that never happened and which describes a plan that
    /// still has a current revision, a warm delta and a clone route forward.
    /// Deliberately not [`DomainError::LifecycleForbidden`] either, by D-146's
    /// own line: that code holds the refusals describing **no** alternative
    /// action, and this one describes a specific one — the id is spent, so mint
    /// a new plan and stop retrying this one.
    #[error("plan holds only abandoned revisions and takes no further revision: {0}")]
    PlanAbandonedNoSuccessor(String),
    /// A grandfathering horizon on a row whose eligibility class is not
    /// `existing_grandfathered`.
    ///
    /// The horizon expires a *retained generation*, and the other two classes
    /// retain nobody, so a horizon there names a moment nothing observes. The
    /// pairing was a physical column check with no rule behind it, which reached
    /// the caller as an internal fault — a 500 for a request whose author only
    /// has to clear one field.
    #[error("grandfathering horizon forbidden off the grandfathered class: {0}")]
    GrandfatherUntilForbidden(String),
    /// A grandfathering horizon moved **outwards**, or cleared
    /// (`07-pricewindow-linkage.md` §5 `GRANDFATHER_LOOSEN_FORBIDDEN`,
    /// `inst-gs-bound`/`inst-gs-tighten`).
    ///
    /// The horizon is the instant a retained generation stops being eligible, so
    /// pushing it later re-grants an eligibility a second principal already
    /// approved the end of, and clearing it makes a bounded generation indefinite
    /// — the state machine's `active_bounded` has no edge back to
    /// `active_indefinite`. Both are the same fact to the operator: this field
    /// only ever moves earlier.
    ///
    /// The code is **declared** by S7 §5 rather than minted here, which is what
    /// D-204 clause (2) requires of a refusal this crate renders on the wire. The
    /// physical guard beneath it — `trg_pricing_price_grandfather_monotonic` on
    /// both engines — states the identical predicate and had no caller in this
    /// crate until the horizon door landed; it stays as the floor under the rule,
    /// exactly as the append-only trigger stays under the lifecycle machine.
    #[error("grandfathering horizon may only be tightened: {0}")]
    GrandfatherLoosenForbidden(String),
    /// A `(meter, dimensionKey)` pair that does not belong where it was put
    /// (`01-foundation.md` §4.1, D-196).
    ///
    /// Two shapes reach it, and they are one rule: the pair on a charge kind
    /// that cannot carry it (or a `dimensionKey` naming no meter), refused by
    /// `domain::scope_key::check_usage_line_axes`; and an authored row whose own
    /// line is not the one its canonical scope key names, refused by
    /// `price_repo::check_authored_usage_line`. Both are "this line is not this
    /// key's", which is the fact the author has to act on.
    ///
    /// [`GrandfatherUntilForbidden`](Self::GrandfatherUntilForbidden)'s sibling
    /// in shape as well as in kind: one axis-conditioned rule, one code.
    #[error("usage line axes disagree with the key: {0}")]
    UsageLineAxisMismatch(String),
    /// A grandfathering cutover's instant has passed, or no longer clears the
    /// batching delay at approval commit (`07-pricewindow-linkage.md` §5,
    /// `inst-gc-compose`).
    ///
    /// [`SupersessionInstantPassed`](Self::SupersessionInstantPassed)'s twin, and
    /// the two are deliberately two codes over **one** floor: §5 declares the
    /// supersession's as "the same floor `inst-gc-compose` gives cutovers", so the
    /// bound is shared (`domain::supersession::changeover_floor`) while the code
    /// tells the operator which act they were performing.
    #[error("cutover instant passed: {0}")]
    CutoverInstantPassed(String),
    /// A cutover composed over a key with no coverage to shorten
    /// (`07-pricewindow-linkage.md` §5, `inst-co-shorten`).
    ///
    /// **The supersession's identical refusal has no code of its own** and renders
    /// as [`LifecycleForbidden`](Self::LifecycleForbidden) with the ground in its
    /// sentence, because §5 declares one and not the other. The asymmetry is the
    /// design set's; a gear may mint an internal variant freely but a wire code is
    /// the set's to declare, so the two units answer one fact under two spellings
    /// until §5 says otherwise.
    #[error("cutover leaves the key uncovered: {0}")]
    CutoverGap(String),
    /// A window was created with a start that is not strictly in the future
    /// (`07-pricewindow-linkage.md` §5, D-63, `inst-ws-future-start`).
    ///
    /// §5 types it 422, which is this crate's architectural 422 rendered **400**
    /// with the code as the discriminator. A precondition failure and not a
    /// malformed argument for [`DomainError::TimestampPrecisionExceeded`]'s reason:
    /// the instant parses and is a perfectly good instant — it is simply behind a
    /// clock the request cannot see.
    ///
    /// Deliberately **not** a conflict, unlike the three window refusals that are.
    /// Those three tell a caller the world moved under a request that was right
    /// when written, and a retry with fresh state resolves them. This one tells a
    /// caller their request was never right: retrying it unchanged only puts the
    /// start further into the past.
    ///
    /// **What it closes is a backdating bypass**, which is why it is not
    /// aesthetic. `inst-ws-immutable` guards only the mutation of a past start, so
    /// without this rule a `plan × write` holder could schedule a window starting
    /// sixty days ago, have the `WindowActivationJob` activate it, and reprice open
    /// unposted arrears periods — round the reason-mandatory two-person
    /// `BackdateGrant` S2 calls the only sanctioned backdating, and without
    /// tripping S5's `BACKDATE_SIDE_EFFECT` predicate.
    #[error("window start is not in the future: {0}")]
    WindowStartInPast(String),
    /// A cancel or an `effectiveTo` shortening would leave the canonical scope key
    /// uncovered with no successor (`07-pricewindow-linkage.md` §5,
    /// `inst-fg-trailing`, architectural **422** rendered 400). Names the key and
    /// the first uncovered instant.
    ///
    /// # It is `inst-fg-detect`'s blind spot, not a second spelling of it
    ///
    /// Gap detection compares each window against its **successor**, so by
    /// construction it cannot see a void with no successor. The two rules overlap on
    /// most statements each refuses and neither subsumes the other: the statement
    /// only *this* rule can refuse is a key whose windows are contiguous with no
    /// interior hole, whose last coverage is then removed.
    ///
    /// # Fail-closed with no exemption, and that is D-182 rather than a gap
    ///
    /// `inst-fg-trailing`'s only exemption (D-51, narrowed by D-80 and D-94) turns
    /// on whether the key has in-flight subscribers, a predicate that resolves
    /// through the **D-79 Subscriptions inbound lane**. That lane has no client, no
    /// contract type and no counterpart gear in this repository, and the
    /// counterparty's own SUB-P8 still specifies the pre-D-131 union count rather
    /// than the per-price-id presence map every consumer of the lane needs. D-131
    /// makes a lane that cannot answer the **subscribers-present** case, and D-182
    /// records that an *absent* lane is that case too — fail-closed is about the
    /// predicate's evaluability, not the mechanism of its silence. So this refusal
    /// covers the exempt cancels as well, **no exemption branch is written**, and no
    /// second code is minted for the absent-lane case.
    ///
    /// Not a conflict, and for [`DomainError::WindowStartInPast`]'s reason with the
    /// sign reversed: the world did not move under the caller. The coverage this
    /// refuses to remove is the coverage that was there when they composed the
    /// request, and a retry unchanged is refused identically — the check consumes
    /// nothing and clears nothing, so re-entry is stable (D-182 clause 5).
    #[error("window trailing void: {0}")]
    WindowTrailingVoid(String),
    /// A supersession's changeover instant has fallen behind the floor
    /// `inst-su-instant` holds it to (`07-pricewindow-linkage.md`, architectural
    /// **422** rendered 400). Names the instant, the floor and the remedy.
    ///
    /// **Two floors, one code**, and that is the design set's choice rather than a
    /// conflation: strictly future at submit, and at least D-47's max batching
    /// delay ahead at approval commit. The refusal names which moment it was asked
    /// at, because the same instant can pass one and fail the other and a caller
    /// told only the code cannot tell those apart.
    ///
    /// Not a conflict, for [`DomainError::WindowStartInPast`]'s reason — the world
    /// did not move under the caller, the clock did — but unlike that one it is
    /// **not** answered by a retry either. The design set's remedy is that the unit
    /// is *recomposed* against a fresh instant, so the message says so: resubmitting
    /// the same instant is refused identically and lands further behind the floor
    /// each time. That is why this carries its own code rather than joining
    /// `WINDOW_START_IN_PAST`, which it superficially resembles: the field is
    /// different, the floor is different at one of the two moments, and the remedy
    /// is a recomposition rather than a correction.
    #[error("supersession changeover instant passed: {0}")]
    SupersessionInstantPassed(String),
    /// A proposed approval-threshold policy breaks one of §6's shape rules
    /// (`design/05-governance.md` §5, which types it 422).
    ///
    /// The rules, and which layer owns each — because three of the five are held
    /// somewhere else and this variant must not be read as the only guard:
    ///
    /// * **keys are ISO 4217 codes** — [`CurrencyCode::new`] owns the shape and
    ///   answers [`DomainError::CurrencyInvalid`]; the surface folds that into this
    ///   code, because a caller of the policy PUT broke *this* rule and a
    ///   `CURRENCY_INVALID` on a route with no price row on it names nothing they
    ///   can find;
    /// * **`percent > 0`** — held by `ThresholdBasis::Percent`'s constructor path at
    ///   the surface and by `chk_pricing_approval_threshold_percent_positive` in the
    ///   store;
    /// * **`absolute_minor >= 0`** — the same pair one CHECK over;
    /// * **at least one entry, and no currency twice** —
    ///   [`ThresholdVersion::new`](crate::domain::materiality::ThresholdVersion::new),
    ///   whose two refusals are the ones only this variant carries.
    ///
    /// §6's *"`absolute_minor` ≥ 0 in minor units at the currency's ISO 4217
    /// precision"* adds **nothing checkable** and is not checked: the value is
    /// already an integer count of minor units, so every integer satisfies it at
    /// every precision. Stated rather than silently dropped, because a rule with no
    /// test looks like a rule somebody forgot.
    ///
    /// A precondition failure and not a conflict: nothing moved under the caller,
    /// and a retry unchanged is refused identically.
    ///
    /// [`CurrencyCode::new`]: crate::domain::money::CurrencyCode::new
    #[error("threshold policy invalid: {0}")]
    ThresholdInvalid(String),

    /// The `CatalogVersion` registry **answered, and refused** — an unknown SKU, a
    /// closed version.
    ///
    /// Publish stops here exactly as [`Self::CatalogVersionUnavailable`] does, and
    /// the two are separate variants for one reason: the refusal will be made
    /// identically for as long as the request is made unchanged, so it belongs to
    /// this family and not to the retriable one. Folded onto the unavailable arm it
    /// would reach every caller as a 503, telling them to retry a decision that never
    /// moves — see `infra::error_mapping`'s note on the pair, and
    /// [`crate::domain::ports::registry_failure`], which is the only producer of
    /// either.
    ///
    /// The detail is the registry's own sentence and it **is** carried to the
    /// caller, unlike the unavailable arm's: it names the SKU or version the
    /// author has to change, which is the whole remedy.
    #[error("catalog-version registry rejected the request: {0}")]
    CatalogVersionRejected(String),

    // -- Aborted (conflict; the caller may retry with fresh state) --
    /// Another current row already occupies the canonical scope key.
    #[error("duplicate canonical scope key: {0}")]
    DuplicateScopeKey(String),
    /// A `phases` write named a `phase_id` the plan revision already holds
    /// (`PHASE_ID_IN_USE`, §5, **409**, D-340).
    ///
    /// The conflict class for [`DomainError::DuplicateScopeKey`]'s reason and in
    /// the same words — *this key is already held* — and a separate variant for
    /// [`DomainError::PrecedenceDuplicate`]'s: that one names a price row's
    /// canonical key, and a caller told their scope key was duplicated would go
    /// looking for a price row that has nothing to do with the refusal.
    ///
    /// **Not a publish-pipeline violation**, which is why it is a variant of its
    /// own rather than a `ValidationReport` entry: the store raises it at the
    /// write, no rule emits it, and a code in the pipeline's roster that no rule
    /// emits would make the roster claim a rule that does not exist (D-340
    /// clause 3). §5 declares the code, so unlike
    /// [`DomainError::BundleExistsOnPlan`] the spelling is not this gear's to
    /// invent.
    #[error("phase id already in use: {0}")]
    PhaseIdInUse(String),
    /// The submitted `ETag` / row version is stale: an interactive edit and a
    /// bulk run collided, or the caller is working from a read it did not
    /// refresh. Neither change is silently overwritten.
    #[error("stale version: {0}")]
    StaleVersion(String),
    /// The same idempotency key arrived with a different payload. Never
    /// replayed and never re-executed — the two requests disagree about what
    /// they are.
    #[error("idempotency payload mismatch: {0}")]
    IdempotencyPayloadMismatch(String),
    /// The idempotency key is claimed and not yet answered, so there is no
    /// stored response to replay (D-143).
    ///
    /// A retry is the whole remedy, and it is one a client can act on: shortly
    /// after, the answer is either the stored response or
    /// [`DomainError::IdempotencyPayloadMismatch`]. Without a code of its own
    /// the surface had to answer a caller with a response nobody had produced —
    /// the one thing the dedup row exists to make impossible.
    #[error("idempotency key claimed and not yet answered: {0}")]
    IdempotencyKeyInFlight(String),
    /// Another mutation of this aggregate committed while yours was in flight,
    /// and the store's per-aggregate serialization point refused the loser
    /// (D-159).
    ///
    /// **Retriable, and it is nobody's mistake.** Three constructs serialize
    /// writes per aggregate: the audit chain head `(tenant_id, chain_id, seq)`,
    /// the outbox's per-`(tenant_id, aggregate_id)` sequence, and the
    /// current-revision partial `UNIQUE`. A loser at any of them used to reach
    /// the caller as `Internal` -> **500**, indistinguishable from a dead
    /// connection, for a request whose entire remedy is to try again.
    ///
    /// Deliberately **not** [`DomainError::StaleVersion`]: nothing the caller
    /// presented was stale, and a caller told their `If-Match` failed would
    /// refresh a version that was never wrong. Deliberately **not**
    /// [`DomainError::IdempotencyKeyInFlight`] either, whose subject is the
    /// caller's own duplicate request rather than somebody else's write.
    #[error("concurrent mutation: {0}")]
    ConcurrentMutation(String),
    /// The plan already holds an open draft revision, named by the refusal.
    ///
    /// A uniqueness conflict on the plan's one editable slot, not a state
    /// machine edge. The operator's next action is a real one — go and edit that
    /// revision — which is unreachable from a refusal that does not say which
    /// revision holds the slot.
    #[error("open draft revision exists: {0}")]
    OpenDraftRevisionExists(String),
    /// A bundle already exists on this plan (`uq_pricing_bundle_plan`).
    ///
    /// The conflict class for `OpenDraftRevisionExists`' reason: a uniqueness
    /// conflict on a slot the plan has exactly one of, not a state-machine edge.
    /// The operator's next action is a real one — go and edit that bundle — and
    /// it is unreachable from a refusal that does not say a bundle is what is in
    /// the way.
    ///
    /// **§5 declares no wire code for it**, so it renders under the existing
    /// `OPEN_DRAFT_REVISION_EXISTS`-shaped conflict ladder with a reason of its
    /// own only if the design set later mints one; the gap is owed-register entry
    /// B-9. A gear may mint a `DomainError` variant freely — a wire code is the
    /// design set's to declare — so the variant is here and the reason string is
    /// not invented.
    #[error("bundle exists on plan: {0}")]
    BundleExistsOnPlan(String),
    /// The act this announcement carries is already enqueued
    /// (`uq_pricing_outbox_dedup_key`).
    ///
    /// A uniqueness conflict on a slot the act holds one of, which is
    /// [`DomainError::OpenDraftRevisionExists`]'s class rather than
    /// [`DomainError::LifecycleForbidden`]'s: no state machine refused an edge, a
    /// key was taken. Filed as a lifecycle refusal it answers 400 for a duplicate
    /// key and makes `LIFECYCLE_FORBIDDEN` mean two unrelated things at once.
    ///
    /// **Deterministic, unlike the sequence race it is split from**, so it must not
    /// carry [`DomainError::ConcurrentMutation`]'s advice: the retry renders the
    /// same key from the same inputs and collides identically. The conflict class
    /// states that without advising anything — `OPEN_DRAFT_REVISION_EXISTS`,
    /// `BUNDLE_EXISTS_ON_PLAN` and `PRECEDENCE_DUPLICATE` are all 409 and all
    /// permanent until the caller changes what it sends.
    ///
    /// **§5 declares no wire code for it.** A gear may mint a `DomainError` variant
    /// freely — a wire code is the design set's to declare — so the reason string
    /// is this gear naming a refusal it owns, as `BUNDLE_EXISTS_ON_PLAN` does, and
    /// owes the design set the declaration owed-register entry B-9 asks for there.
    #[error("outbox event already enqueued: {0}")]
    OutboxEventAlreadyEnqueued(String),
    /// The canonical scope key is dormant at the changeover: no scheduled or active
    /// window covers that instant, and a supersession presupposes current coverage.
    ///
    /// Out of [`DomainError::LifecycleForbidden`] because that variant is narrowed by
    /// D-146 to refusals describing **no alternative action**, and this one describes
    /// one in its own message: schedule a window on the key's current row. A consumer
    /// that has to read prose to find a remedy cannot act on it, which is the whole
    /// of what the narrowing is for.
    ///
    /// **§5 declares no wire code for it**, so the reason string is this gear naming
    /// a refusal it owns, as `BUNDLE_EXISTS_ON_PLAN` does, and owes the design set
    /// the declaration owed-register entry B-9 asks for there.
    #[error("scope key dormant at the changeover: {0}")]
    SupersessionKeyDormant(String),
    /// The covering window begins at the changeover itself, so shortening it there
    /// would leave it empty and no coverage remains for a successor to take over.
    ///
    /// [`DomainError::SupersessionKeyDormant`]'s reason for being its own variant,
    /// and its own remedy: adjust or reschedule that window rather than superseding
    /// across it.
    #[error("supersession would empty the covering window: {0}")]
    SupersessionWouldEmptyWindow(String),
    /// Two published overlays of one scope class claim one `precedence`
    /// (`09-price-overlays.md` §5, L2, **409**).
    ///
    /// A conflict rather than a precondition failure, and §5 types it 409
    /// outright — one of only two overlay codes it does. The reading holds: the
    /// caller's request was well formed and their authority sufficient, and what
    /// refused it is a **sibling overlay** they cannot see from their own
    /// request. Re-reading the class's overlays is the remedy.
    ///
    /// Deliberately not [`DomainError::DuplicateScopeKey`], which it
    /// superficially resembles: that one is about a price row's canonical key,
    /// and a caller told their scope key was duplicated would go looking for a
    /// price row that has nothing to do with the refusal.
    #[error("precedence duplicate: {0}")]
    PrecedenceDuplicate(String),
    /// Two overlays' intervals claim one instant on one line key
    /// (`09-price-overlays.md` §5, D-107, **409**).
    ///
    /// [`DomainError::PrecedenceDuplicate`]'s sibling, and the second of the two
    /// overlay codes §5 types 409 outright. A conflict for
    /// [`DomainError::WindowOverlap`]'s reason, which it is the overlay analogue
    /// of: two intervals on **one** key claim one instant, and what refused the
    /// request is a sibling overlay the caller's own request never named.
    ///
    /// The collision domain is *other* overlays' **published** revisions and
    /// never another revision of the same overlay — D-107, without which every
    /// edit of a live overlay would be refused by its own predecessor.
    #[error("overlay interval overlap: {0}")]
    OverlayIntervalOverlap(String),
    /// A taxonomy value that cannot retire because something active still names
    /// it (`04-currency-tax.md` §5, `inst-tx-mutation`, **409**).
    ///
    /// A conflict rather than a precondition failure, for
    /// [`DomainError::OverlayIntervalOverlap`]'s reason: nothing the caller
    /// presented was wrong or stale, and what refused the request is a published
    /// price row or a published overlay the caller's own body never named.
    /// Re-reading — going to look at what still references the value — is the
    /// whole remedy, which is what a 409 asks for and what a 400 would send them
    /// looking for a field to correct.
    ///
    /// The code is **§5's**, not minted here: `TAXONOMY_VALUE_IN_USE` is declared
    /// in the design set's own problem-response list, so this variant implements
    /// a spelling rather than inventing one.
    #[error("taxonomy value in use: {0}")]
    TaxonomyValueInUse(String),
    /// `POST …/taxonomies/{class}/values` named a value the tenant already
    /// declares — with other content, or retired (**409**).
    ///
    /// A conflict rather than a bad request: the body was well-formed, and what
    /// refused it is a row the caller did not send. The value is the resource's
    /// natural key, so the remedy is to address the row that exists — `PATCH` on
    /// it — rather than to re-spell the create. An identical body is **not** this
    /// refusal: it replays the create (200), the way every guarded create here
    /// answers its own retry.
    ///
    /// The code is §5's: `TAXONOMY_VALUE_EXISTS`, declared beside
    /// `TAXONOMY_VALUE_IN_USE` in `04-currency-tax.md`'s problem-response list.
    #[error("taxonomy value exists: {0}")]
    TaxonomyValueExists(String),
    /// A plan cannot retire while something composes or overrides it
    /// (`11-lifecycle.md` §5, `inst-re-references`, **409**).
    ///
    /// A conflict for [`DomainError::TaxonomyValueInUse`]'s reason, and it is the
    /// same shape of fact one plane over: nothing the caller presented was wrong,
    /// and what refused the retirement is a **bundle** or an **add-on override**
    /// their own request never named. The refusal therefore enumerates the
    /// referrers — the remediation is to re-compose or retire a *named* referrer
    /// first, which is unreachable from a refusal that only says "referenced".
    ///
    /// Only the two **blocking** reference classes reach it. A plan listing the
    /// retiree in `allowedChangeTargets` (D-24) and an overlay targeting it
    /// (D-31) are enumerated as warnings and never raise this: the first goes
    /// inert because Subscriptions re-checks lifecycle state at change time, and
    /// the second stays deliberately evaluable for in-flight subscribers.
    ///
    /// The code is **§5's**, not minted here: `RETIRE_PLAN_REFERENCED` is in the
    /// design set's problem-response list.
    #[error("retire plan referenced: {0}")]
    RetirePlanReferenced(String),
    /// A plan cannot retire while a scheduled migration still targets it
    /// (`11-lifecycle.md` §2, §5, **409**).
    ///
    /// [`DomainError::RetirePlanReferenced`]'s sibling and a conflict for the
    /// same reason: the caller's request named nothing wrong, and what refuses it
    /// is a schedule pointing *at* this plan as the place subscribers are about
    /// to be moved to. Retiring it would leave that migration aimed at a plan
    /// that can no longer be sold, so the remedy is to cancel the migration or
    /// let it complete first — which is why the refusal names it.
    ///
    /// Only a **live** schedule refuses: `scheduled` and `in_progress`. A
    /// `completed` or `cancelled` run is history and moves nobody.
    ///
    /// The code is **§5's**, not minted here.
    #[error("retire target of migration: {0}")]
    RetireTargetOfMigration(String),
    /// A plan cannot retire while a **live cutover** stands over it
    /// (`07-pricewindow-linkage.md` `inst-co-retirement-unwind`, D-05, **409**).
    ///
    /// [`DomainError::RetireTargetOfMigration`]'s sibling and a conflict for its
    /// reason: the caller's request named nothing wrong and their authority was
    /// sufficient — what refuses it is a decided cutover whose instant is still
    /// ahead, which their own request never named. The remedy is to wait until
    /// the cutover takes effect, or to withdraw the unit while it is still
    /// `submitted`, and re-reading the plan's approval plane is how an operator
    /// finds out which — the act a 409 asks for.
    ///
    /// Kept apart from [`DomainError::RetirePlanReferenced`] deliberately: that
    /// one sends an operator to a bundle or an add-on override to re-compose,
    /// where this one sends them to a schedule to wait out or cancel.
    ///
    /// **The code is minted here rather than read off §5**; see
    /// [`crate::domain::retirement::RETIRE_OVER_LIVE_CUTOVER`] for what the design
    /// set declares about this collision instead, and what is owed.
    #[error("retire over live cutover: {0}")]
    RetireOverLiveCutover(String),
    /// A draft price row was discarded while a window stands on it
    /// (`fk_pricing_price_window_price`, **400** carrying its own code).
    ///
    /// A precondition rather than a conflict, and
    /// [`DomainError::PlanRetiredNoSuccessor`]'s reason: the window table refuses
    /// `DELETE` outright, so the referrer never goes away and there is no retry
    /// that succeeds. What the refusal buys is that the operator is told which
    /// window binds the row instead of being told the store is broken.
    ///
    /// See [`crate::domain::window::PRICE_WINDOW_SCHEDULED`] for the code, what
    /// the design set declares instead, and why the delete does not cascade.
    #[error("price window scheduled: {0}")]
    PriceWindowScheduled(String),
    /// A migration's target is not a published plan (`11-lifecycle.md` §2, §3,
    /// §5, `inst-mg-target`, **422 → 400**).
    ///
    /// An invalid argument rather than a conflict: the caller named a target and
    /// the target is the wrong kind of thing, so there is one field to correct.
    /// A `draft` target has no consumer-resolvable content to migrate onto and a
    /// `retired` one cannot take new subscribers at all, so both are refused by
    /// the same predicate — a migration may only land subscribers somewhere they
    /// could have subscribed directly.
    ///
    /// The code is **§5's**, not minted here.
    #[error("migration target invalid: {0}")]
    MigrationTargetInvalid(String),
    /// A migration's `effectiveAt` is closer than the tenant's configured notice
    /// period (`11-lifecycle.md` §3, §5, `inst-mg-target`, D-49, **422 → 400**).
    ///
    /// A precondition failure naming one field. **There is no silent override**:
    /// D-49 lets a tenant raise the notice period and never lower it below the
    /// 60-day floor, so an emergency shorter migration requires an explicit
    /// audited policy change first (itself material, the D-10 pattern) rather
    /// than a flag on this request. The refusal therefore reports the configured
    /// period and the earliest admissible instant, because the operator's next
    /// act is to pick a later date or to go and change the policy.
    ///
    /// The code is **§5's**, not minted here.
    #[error("migration notice too short: {0}")]
    MigrationNoticeTooShort(String),
    /// A migration carries unresolved blocking deltas (`11-lifecycle.md` §2, §5,
    /// `inst-ms-deltas`, **422 → 400**).
    ///
    /// The deltas are **enumerated**, for [`DomainError::RetirePlanReferenced`]'s
    /// reason: "this migration is blocked" is not an actionable sentence when the
    /// remedy is to change the target, scope a named subscription out, or fix a
    /// named add-on. Contract-locked subscriptions are **not** in this set — they
    /// are excluded and reported rather than blocking (`inst-md-locks`,
    /// `inst-cl-exclude`), because the lock is never broken and never needs the
    /// operator to resolve anything.
    ///
    /// An unresolved blocking delta **never persists a schedule** (§7), so this
    /// refusal counts a rejection rather than leaving a waiting record behind.
    ///
    /// The code is **§5's**, not minted here.
    #[error("migration blocked: {0}")]
    MigrationBlocked(String),
    /// A cancel was asked of a migration that has already completed
    /// (`11-lifecycle.md` §3, §4, §5, `inst-mg-cancel`,
    /// `inst-mst-cancel-inflight`, D-34, **409**).
    ///
    /// A conflict on mutable state: the request was well-formed and the caller's
    /// authority sufficient, but the run finished before the cancel arrived.
    ///
    /// **The code is `MIGRATION_COMPLETED` and deliberately not the pre-D-34
    /// `MIGRATION_ALREADY_EFFECTIVE`.** D-34 rescoped the cancel so that an
    /// `in_progress` run *is* cancellable — the stop-the-bleeding control — which
    /// left the old code naming the wrong fact: "already effective" describes
    /// every run past its effective date, and most of those can still be stopped.
    /// Only a **completed** one cannot, so the code names completion.
    #[error("migration completed: {0}")]
    MigrationCompleted(String),
    /// A price row naming a `region` the tenant's taxonomy does not declare
    /// active (`04-currency-tax.md` §2, §5, `inst-mc-region`, **422 → 400**).
    ///
    /// A caller mistake naming one field, so it is an invalid argument rather
    /// than a conflict: the remedy is to correct the region or declare it, and
    /// nothing about the world moved under the caller.
    ///
    /// The code is **§5's**, not minted here.
    #[error("region unknown: {0}")]
    RegionUnknown(String),
    /// A tenant default naming a rounding policy the tenant's own taxonomy does
    /// not declare active (D-334, `03-price-structure.md` §3.3, **422 → 400**).
    ///
    /// [`DomainError::RegionUnknown`]'s exact shape one taxonomy over, and it
    /// exists for the reason that one does: the remedy is to correct the
    /// reference or declare it, and nothing about the world moved under the
    /// caller. The code is `ROUNDING_POLICY_UNKNOWN`, §5's, not minted here.
    ///
    /// Raised only at the config write door. A *row's* unknown reference is still
    /// judged at publish by `RoundingPolicyDeclared` inside the report, because
    /// there the caller is authoring many rows and an enumerated report is the
    /// answer; here the caller is setting one field.
    #[error("rounding policy unknown: {0}")]
    RoundingPolicyUnknown(String),
    /// The preview names a `(currency, region)` this plan publishes no row on
    /// (`04-currency-tax.md` §2, §5, `inst-pv-return`, **404**).
    ///
    /// A **not-found** and not an empty success: the caller asked for a price on
    /// a market this plan does not sell, so the resource they named does not
    /// exist — and a 200 carrying no price would be indistinguishable, at a
    /// partner's end, from a price of zero. The catalog performs no FX, so there
    /// is nothing else to answer with (`inst-mc-nofx`).
    #[error("price row absent: {0}")]
    PriceRowAbsent(String),

    /// §5's `CLONE_SOURCE_NOT_FOUND`: the plan named in a clone's path holds no
    /// **current** revision, so there is nothing published to copy.
    ///
    /// Distinct from the generic not-found because the *plan* may well exist —
    /// a draft-only plan is readable and editable and simply has no
    /// configuration a clone could take — and an operator told only "not found"
    /// would go looking for a missing id. It was deliberately not minted until
    /// the route that returns it existed, so that a wire code the design set
    /// declares could not end up in two spellings (D-146's posture; D-277 built the
    /// route and D-278 minted this).
    #[error("clone source not found: {0}")]
    CloneSourceNotFound(String),

    /// §5's `BULK_VALIDATION_FAILED`: Phase 1 refused the batch, and nothing was
    /// committed.
    ///
    /// An **architectural 422 rendered 400** with the code as the discriminator,
    /// this gear's established reading of Foundation §3.3 — the platform has no
    /// 422 category. The per-row report is not in the refusal: it is on the run,
    /// which the `GET` serves, and a batch's answer lives there for the first
    /// caller and every replay alike (`inst-bi-return`).
    ///
    /// **`operation_id` is a field and not a phrase inside `detail`** (D-295).
    /// The report the refusal points at is only reachable through the ref, so a
    /// ref a client has to parse out of a sentence makes the whole of Phase 1's
    /// answer unreadable by anything but a human — and this is the one refusal
    /// whose entire value is the report behind it.
    #[error("bulk validation failed: {detail} (operation {operation_id})")]
    BulkValidationFailed {
        /// The run that holds the per-row report.
        operation_id: String,
        /// What was refused, in the operator's terms.
        detail: String,
    },
    /// §5's `RUN_SELECTOR_EMPTY`: a mass-repricing run whose selector matched no
    /// published row (architectural **422** rendered 400, this gear's established
    /// reading of Foundation §3.3).
    ///
    /// A precondition failure and **not** a malformed argument, though every field
    /// of the selector is well-formed by the time this can be raised: what is
    /// wrong is a relationship between the request and the world, which is the
    /// line [`DomainError::MigrationTargetInvalid`] and its two Slice-11 siblings
    /// are drawn on.
    ///
    /// Deliberately not a 404 either. The caller did not address a resource that
    /// is missing — they described a set, and the set is empty; a 404 would send
    /// an operator looking for an id that was never in their request.
    ///
    /// **The refusal exists because the success is indistinguishable from it.** A
    /// run is complete when no `pending` journal rows remain (§6), so a run that
    /// selected nothing is complete at birth: without this, an operator who
    /// mistyped a region would be told their mass adjustment succeeded, and the
    /// journal that is supposed to be the evidence would be empty in exactly the
    /// way a finished run's is.
    #[error("repricing run selector matched nothing: {0}")]
    RunSelectorEmpty(String),
    /// A decision was asked of an approval record that is no longer pending
    /// (`design/05-governance.md` §5, `inst-as-immutable`).
    ///
    /// A conflict on mutable state and classified as one: the request was
    /// well-formed and the reviewer's authority sufficient, but another decision
    /// — an approve, a reject, or the TOCTOU guard's void — reached the record
    /// first. Re-reading it is the whole remedy, which is what a 409 asks for.
    ///
    /// Deliberately **not** [`DomainError::LifecycleForbidden`], though it is a
    /// state-machine edge and that variant carries the others. Every refusal on
    /// that line is a 400 telling a caller their request was wrong about the
    /// world; this one tells them the world moved, and a reviewer sent to check
    /// their own request would find nothing to fix.
    #[error("approval not pending: {0}")]
    ApprovalNotPending(String),
    /// A second change unit was submitted over a subject a pending unit already
    /// holds (`07-pricewindow-linkage.md` §5 / `inst-co-single-pending`, **409**).
    ///
    /// The conflict class, beside [`DomainError::OpenDraftRevisionExists`] and
    /// for the same reading: it is a uniqueness conflict on a slot the subject
    /// has exactly one of, and the operator's next action is real — decide the
    /// unit the refusal names, or withdraw it (`inst-as-void`). The refusal
    /// therefore names it; a refusal that did not would leave the caller holding
    /// a lock they cannot find.
    ///
    /// Declared by the design set (S7 §5's problem responses, S12
    /// `inst-bk-approval-subset`, D-35) and referenced here rather than minted.
    #[error("pending change unit exists: {0}")]
    PendingChangeUnitExists(String),
    /// The submitter of an approval unit tried to decide it
    /// (`inst-tp-distinct`, §5, **403**).
    ///
    /// A **permission** refusal and not a precondition one, which is what §5
    /// types it and what it is: the request was well-formed, the record was
    /// pending, the content had not moved — the principal simply may not be the
    /// second person. Nothing about the request can be edited to make it
    /// succeed, so a 400 telling them to fix it would be a loop, and a 409
    /// inviting a retry would be worse.
    ///
    /// The attempt is written to `pricing_audit_log` as a `deny` record before
    /// this is returned (`inst-tp-selfaudit`); see
    /// [`crate::infra::approval`] for why that record is committed in its own
    /// transaction.
    #[error("self-approval forbidden: {0}")]
    SelfApprovalForbidden(String),
    /// The approver's pricing-region grant does not cover every region the
    /// pinned change set touches (`inst-ap-scope`, §5, **403**).
    ///
    /// Beside [`DomainError::SelfApprovalForbidden`] and for the same reason:
    /// it is an authority refusal, and it is audited.
    #[error("region scope denied: {0}")]
    RegionScopeDenied(String),
    /// A withdraw by a principal who is neither the unit's submitter nor a
    /// catalog authority (`inst-as-void`, **403**).
    ///
    /// Beside [`DomainError::SelfApprovalForbidden`] and for its reason: closing
    /// another principal's review releases the canonical scope keys the unit
    /// held, so it is an authority act, and the attempt is audited.
    #[error("withdraw forbidden: {0}")]
    WithdrawForbidden(String),
    /// The pinned content no longer matches the subject at decision time
    /// (`inst-ap-pin`, §5, **409**).
    ///
    /// A conflict, with the conflicts: somebody mutated the subject after the
    /// reviewer opened it. Re-reading — and, since the TOCTOU guard will have
    /// voided the record, re-submitting — is the remedy, which is what a 409
    /// asks for. Deliberately not a precondition failure: the reviewer's request
    /// was right about the world as they were shown it.
    #[error("approval content mismatch: {0}")]
    ApprovalContentMismatch(String),
    /// A rejection arrived with no reason (`inst-as-reject`, §5).
    ///
    /// §5 types it 422. The canonical family has no 422 category, so it reaches
    /// the wire as a **400** carrying `REASON_REQUIRED` and the code is the
    /// discriminator — the Foundation §3.3 rule this crate applies to every
    /// architectural 422.
    #[error("reason required: {0}")]
    ReasonRequired(String),
    /// A scheduled or adjusted window intersects one already on the same
    /// canonical scope key (`07-pricewindow-linkage.md` §5, **409**).
    ///
    /// A conflict rather than a precondition failure, and §5 types it 409
    /// outright — the only one of the eight window codes it does. The reading
    /// holds: the caller's request was well-formed and their authority was
    /// sufficient, and what refused it is the state of a *sibling* row on the key
    /// they cannot see from their own request. Re-reading the key's windows is
    /// the remedy, which is what a 409 asks for.
    ///
    /// Deliberately not [`DomainError::DuplicateScopeKey`], which it superficially
    /// resembles: that one says two rows claim one key, while this says two
    /// intervals on **one** key claim one instant. The key here is legitimately
    /// shared — a supersession's predecessor and successor are both on it — so a
    /// caller told the key was duplicated would go looking for a row to retire.
    ///
    /// It is **not** raised by any constraint. §6 puts non-overlap "inside every
    /// mutation" because the key is ten columns of `pricing_price` and none of
    /// them is on the window row, so the sole producer is
    /// [`window_repo`](crate::infra::storage::repo::window_repo) — see that
    /// module and the migration's own note.
    #[error("window overlap: {0}")]
    WindowOverlap(String),
    /// A mutation reached for something §4 froze — a past `effectiveFrom`, a
    /// terminal window, the window↔price binding, or an `effectiveTo` that has
    /// already passed (`07-pricewindow-linkage.md` §5, `inst-ws-immutable`,
    /// **409**).
    ///
    /// A conflict for [`DomainError::WindowOverlap`]'s reason and one of its own:
    /// what refused the request is **where the clock now stands**. A `PATCH`
    /// composed against a window whose end was five minutes away is a request that
    /// was right when it was written, and re-reading the window is the whole
    /// remedy — which is what a 409 asks for. §5 types it 409 outright.
    ///
    /// Deliberately **not** [`DomainError::LifecycleForbidden`], which carried this
    /// ground until the code had a producer. That variant is D-146's "refusals that
    /// describe no alternative action", and this one describes a very specific one
    /// on two of its three grounds: schedule a *new* window instead of moving this
    /// one's start, or shorten to an instant that is still ahead.
    ///
    /// Deliberately **not** [`DomainError::WindowNotCancellable`] either, though
    /// both refuse an operation on a window that has moved on. That one is the
    /// answer to a *cancellation* and names a different next action — shorten it —
    /// while this is the answer to a mutation of frozen content.
    #[error("window historical immutability: {0}")]
    WindowHistoricalImmutable(String),
    /// A cancellation was asked of a window that has taken effect or is history
    /// (`07-pricewindow-linkage.md` §5, `inst-ws-cancel`, **409**).
    ///
    /// §4's third edge leaves `scheduled` and nothing else, and §5 types the
    /// refusal 409. A conflict rather than a precondition failure because the
    /// caller's request was well-formed and their authority sufficient: the window
    /// activated between the read that showed it as `scheduled` and the `DELETE`.
    ///
    /// It is worth its own code beside
    /// [`DomainError::WindowHistoricalImmutable`] precisely because the operator's
    /// next action differs. Told the window is immutable an operator stops; told it
    /// is not *cancellable* they shorten it through `effectiveTo`, which is the
    /// operation §4 leaves them and the one a cutover's own shorten uses.
    #[error("window not cancellable: {0}")]
    WindowNotCancellable(String),
    /// An enrollment's interval intersects the payer's existing membership **in
    /// the same group** (`design/09-price-overlays.md` §5, `MEMBERSHIP_OVERLAP`,
    /// **409**).
    ///
    /// [`DomainError::WindowOverlap`]'s reason: the request was well-formed and
    /// what refused it is a sibling membership on the payer their own request
    /// never named, and re-reading the payer's memberships is the remedy.
    #[error("membership overlap: {0}")]
    MembershipOverlap(String),
    /// An enrollment's interval intersects the payer's existing membership **in
    /// another group** (`design/09-price-overlays.md` §5, `MEMBERSHIP_CONFLICT`,
    /// D-09, **409**).
    ///
    /// D-09's non-overlap rule is per payer *across all groups*, not merely
    /// within one, and this is the code §5 names for the case that rule is
    /// actually about. Deliberately not merged with
    /// [`DomainError::MembershipOverlap`]: §5 types the two codes separately
    /// because the remedies differ — a same-group collision is a plain
    /// scheduling mistake, while a cross-group one names the atomic **move**
    /// operation as the correct next call.
    #[error("membership conflict: {0}")]
    MembershipConflict(String),
    /// A membership route named a `{group}` the tenant's customer-group
    /// taxonomy does not currently declare — absent, or declared and then
    /// **retired** (`design/09-price-overlays.md` §5, `GROUP_UNKNOWN`, an
    /// architectural 422 rendered 400; see the module note in
    /// `infra::error_mapping`).
    ///
    /// The retired case is the interesting one and is deliberately not
    /// distinguished from "never declared": `inst-cg-taxonomy`'s retire guard
    /// exists to protect the references a value already carries — published
    /// overlay scopes and live `pricing_group_membership` rows alike
    /// (`taxonomy_repo::references_to_customer_group`'s own doc) — not to
    /// bless a **new** one naming a retired value. Blessing a new one is what
    /// this refusal itself prevents; a membership enrolled or moved into a
    /// retired group would leave that half of the guard's point unenforced
    /// for the one act that most needs it.
    #[error("customer group unknown: {0}")]
    GroupUnknown(String),

    // -- The aggregate validation envelope --
    /// The fail-closed validation pipeline rejected the publish. Carries the
    /// whole report rather than the first failure: authoring remediates a plan
    /// in one pass, and a truncated report turns one publish into N round
    /// trips.
    #[error("publish validation failed: {0} blocking violation(s)")]
    ValidationFailed(ValidationReport),

    // -- NotFound --
    /// The named subject does not exist, or lies outside the caller's scope —
    /// deliberately the same answer either way, so the surface leaks no
    /// existence. `subject` is the noun (`plan`, `price`, `overlay`), `id` the
    /// reference the caller supplied.
    #[error("{subject} {id} not found")]
    NotFound {
        /// The kind of thing that was not found.
        subject: String,
        /// The identifier the caller asked for.
        id: String,
    },

    // -- Unavailable (fail closed, retry later) --
    /// The `CatalogVersion` registry could not be reached or has no registered
    /// client. Publish stops here: addressability is not optional, and the
    /// registry is the sole incrementer.
    ///
    /// **Transient states only.** A registry that answered and said *no* is
    /// [`Self::CatalogVersionRejected`]; this variant is for the three that a
    /// later request may find changed.
    #[error("catalog-version registry unavailable: {0}")]
    CatalogVersionUnavailable(String),
    /// The read model is unavailable — the fail-closed answer of a **read** path
    /// this gear does not yet have, so nothing in the crate raises it.
    /// `infra::read_model` records the same absence from the projector's side.
    #[error("read model unavailable: {0}")]
    ReadModelUnavailable(String),
    /// The joint-conformance fixture registry could not be read, so the publish
    /// gate has no opinion about any row.
    ///
    /// **Told apart from [`Self::FixtureMissing`] because the remedy has a
    /// different owner.** `FixtureMissing` names a shape nobody has agreed how
    /// to evaluate, and the author's move is to author a different shape or to
    /// get the corpus row earned; this one is a deployment whose
    /// `registry.toml` is absent or unparseable, and the author has no move at
    /// all. Rendered as a 400 naming `model_kind` it told every operator of such
    /// a deployment that their row had no green fixture, on every publish of
    /// every kind, with a remedy they could not perform.
    #[error("conformance fixture gate unavailable: {0}")]
    FixtureGateUnavailable(String),

    // -- Internal --
    /// An infrastructure fault with no domain meaning.
    #[error("internal: {0}")]
    Internal(String),
}
