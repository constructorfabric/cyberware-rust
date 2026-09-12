//! The SINGLE authoritative `DomainError` → AIP-193 `CanonicalError` ladder.
//!
//! Both surfaces consume it: REST handlers via `?` / an explicit map, and the
//! in-process client via `.map_err(CanonicalError::from)`. A domain variant is
//! therefore assigned a canonical category — and an HTTP status — in exactly
//! one place, which is what keeps the wire contract the design set states from
//! drifting per handler.
//!
//! The wire codes are the names the design set uses verbatim
//! (`DUPLICATE_SCOPE_KEY`, `STALE_VERSION`, `IDEMPOTENCY_PAYLOAD_MISMATCH`,
//! `IDEMPOTENCY_KEY_IN_FLIGHT`, `OPEN_DRAFT_REVISION_EXISTS`,
//! `ROUNDING_POLICY_UNRESOLVED`, `PRECISION_EXCEEDED`,
//! `TIMESTAMP_PRECISION_EXCEEDED`, `AMOUNT_NEGATIVE`, `CURRENCY_INVALID`,
//! `FIXTURE_MISSING`, `PLAN_RETIRED_NO_SUCCESSOR`,
//! `GRANDFATHER_UNTIL_FORBIDDEN`, `CONCURRENT_MUTATION`, the five
//! `05-governance.md` §5 declares — `APPROVAL_NOT_PENDING`,
//! `APPROVAL_CONTENT_MISMATCH`, `SELF_APPROVAL_FORBIDDEN`,
//! `REGION_SCOPE_DENIED`, `REASON_REQUIRED` — and
//! `PENDING_CHANGE_UNIT_EXISTS`, which `07-pricewindow-linkage.md` §5 declares
//! and this gear's submit path is the first writer of); a consumer matches the
//! category coarsely and the code **exactly**.
//!
//! # Why a registry refusal is a 400 and a registry outage is a 503
//!
//! `CatalogVersionRegistryError` has four variants, and folding all four into one
//! `CatalogVersionUnavailable` renders a bare 503 with no code and no
//! discriminator. Three of them deserve that: `Unconfigured`,
//! `Unreachable` and `Internal` are deployment states, and a later request may
//! find them changed. `Rejected` — an unknown SKU, a closed version — is the
//! registry having **answered**, and it will answer the same way for as long as
//! the request is made unchanged.
//!
//! A status code is a retry instruction, so the collapse was not merely a lost
//! diagnostic: it told every client to retry a decision that never moves, and a
//! client that honours it retries forever. `Rejected` therefore lands on
//! `CatalogVersionRejected` and this ladder's failed-precondition family — the
//! 400 the paragraph below calls architectural, carrying
//! `CATALOG_VERSION_REJECTED` as the discriminator — while the other three keep
//! the 503. The classification rule is the family's own: a rejection is
//! classified by **what it is**, and this one is a precondition on the plan's
//! content that the author has to change. `crate::domain::ports::registry_failure`
//! is the single place the split is made and carries the withdrawn argument.
//!
//! **Two codes below are *not* names the design set uses**, and they are called
//! out here rather than left to be assumed.
//!
//! `WITHDRAW_FORBIDDEN` is minted by this crate. §5 declares no code for a
//! withdraw refused on the withdrawer's identity, because the design set does not
//! contemplate the refusal existing — `inst-as-void` states the identity rule
//! while the endpoint map gates the route on `approval × approve`, and nothing
//! reconciles the two. See `domain::approval::decision::WithdrawAuthority`.
//!
//! `CATALOG_VERSION_REJECTED` is minted for the same kind of reason one plane
//! over: D-47 fixes the registry as the sole incrementer and the contract names
//! four failures, but the design set declares a code for none of them because it
//! treats every one of them as the same fail-closed publish. It is not — see the
//! section above — and a 400 has to carry a code or it discriminates nothing.
//!
//! *Exactly* is why the three 403 arms carry a **bare code** in `reason` and drop
//! their detail. `permission_denied()` has no detail slot — its detail is the
//! fixed platform sentence and `reason` is the only free field — so a detail
//! carried there would have to ride the code as `"CODE: detail"`, and a consumer
//! comparing `reason` to `SELF_APPROVAL_FORBIDDEN` would never match. What is
//! given up is nothing the caller lacks and nothing an operator loses: the
//! detail names the approval record, which is the id the caller addressed the
//! request to, and the attempt itself is already on `pricing_audit_log` as a
//! `deny` record carrying that id (`inst-tp-selfaudit`, `inst-rb-audit`) —
//! a durable trail rather than a log line.
//!
//! **That clause names a compensating control, so the control has to exist**, and
//! for two waves it did not: the table had a writer and no reader on any mounted
//! surface, which makes "recoverable from the trail" an argument about a place
//! nobody can look. `GET /bss-pricing/v1/audit`
//! ([`crate::api::rest::audit`]) is that reader — Auditor-only, and the `deny`
//! record's `approval_ref` is the dropped id. Two qualifications, because the
//! sentence above is stronger than what is built. The trail is behind
//! `audit × read`, so the principal who *met* the 403 is generally not the
//! principal who can read the record: what is preserved is the **operator's**
//! recourse, not the caller's, which is `inst-au-pii`'s and D-12's arrangement
//! rather than a shortfall. And of the two instructions cited, only
//! `inst-tp-selfaudit` has a writer on an HTTP path —
//! `REGION_SCOPE_DENIED` is unreachable over HTTP by construction
//! ([`crate::api::rest::approvals`] says why), so for that arm the clause is moot
//! rather than satisfied.
//!
//! **The design set's 422s are architectural, not wire** (normative:
//! `design/01-foundation.md` §3.3). They say *unprocessable content*; the
//! platform's canonical family has no 422 category at all —
//! `FailedPrecondition`, `InvalidArgument` and `OutOfRange` all render **400**
//! (`toolkit_canonical_errors::CanonicalError::status_code`) — so each reaches
//! the wire as a 400 carrying its code, and the **code is the discriminator**,
//! not the status. Two rules follow and are honoured below: a rejection is
//! classified by what it is, so a retriable conflict on mutable state stays a
//! **409** rather than collapsing into the 400 bucket; and no route may declare
//! a 422 in its `OpenAPI` registration, because no path can produce one.

use toolkit::api::canonical_prelude::{CanonicalError, resource_error};

use crate::domain::error::DomainError;
use crate::domain::migration;
use crate::domain::migration::RETIRE_TARGET_OF_MIGRATION;
use crate::domain::repricing;
use crate::domain::retirement::{RETIRE_OVER_LIVE_CUTOVER, RETIRE_PLAN_REFERENCED};
use crate::domain::window::PRICE_WINDOW_SCHEDULED;

#[resource_error(gts_id!("cf.bss.pricing.plan.v1~"))]
struct PlanResource;

/// One architectural 422 (rendered 400, the code the discriminator).
///
/// **Arms fold into it as the file grows**, rather than each spelling the builder
/// out inline: `clippy::too_many_lines` bounds [`From<DomainError>`] at 200
/// (`clippy.toml`) and an inline builder spends three lines on one variant, so a
/// ladder that spelled every arm out would sit over the cap and buy nothing a
/// reader can use. Folding an arm here is what leaves room for the next one.
fn precondition(field: &'static str, detail: &str, code: &'static str) -> CanonicalError {
    PlanResource::failed_precondition()
        .with_precondition_violation(field, detail, code)
        .create()
}

/// The 403 the **authority** refusals share, as one line each.
///
/// [`precondition`]'s reason and the same lint: each of these arms carries nothing
/// but its code, so an inline builder would spend several lines saying so and push
/// this `From` over `clippy::too_many_lines`' cap. The detail is dropped rather than
/// folded into `reason` — see the module note on why the code has to stand alone
/// there.
fn denied(code: &'static str) -> CanonicalError {
    PlanResource::permission_denied().with_reason(code).create()
}

/// The conflict class's plain shape: a retriable 409 carrying its detail and a
/// bare reason code, no violation list.
///
/// [`denied`]'s reason and the same lint, one class over: the "Aborted (409)"
/// family below is two dozen arms deep and every one of them but three (a 404,
/// a 404, and a precondition) was this same three-line builder repeated with a
/// different code — which is what "the family already reads as a group" means
/// here: the comments beside each arm already narrate one shared shape
/// (`WindowOverlap`'s doc: "It joins the conflict class for `APPROVAL_NOT_
/// PENDING`'s reason", `MembershipOverlap`'s: "`WindowOverlap`'s reason one
/// plane over") without the code ever *being* shared. Unlike [`precondition`]
/// and [`bulk_refusal`], which exist because clippy bounds `From<DomainError>`
/// at 200 lines and an inline builder is five, this one exists because the ladder
/// crossed that line at 203 the moment `MembershipOverlap` and
/// `MembershipConflict` joined it — not a new shape, the shape this family
/// already had, finally given the one name its two dozen call sites deserved.
///
/// Unlike `denied`, the detail is **kept**: a conflict is retriable and the
/// caller needs to see what collided, where a permission refusal names only
/// the authority that was missing.
fn aborted(detail: String, code: &'static str) -> CanonicalError {
    PlanResource::aborted(detail).with_reason(code).create()
}

/// Phase 1's refusal, carrying **two** violations under one code: what was
/// refused, and the run that holds the per-row report.
///
/// The ref is a violation rather than a phrase inside the detail because the
/// report is the entire value of this refusal and nothing else points at it — a
/// ref a client has to parse out of a sentence leaves Phase 1's answer readable
/// by a human and by nothing else (D-295). One code, because these are two
/// halves of one refusal rather than two things that went wrong.
fn bulk_refusal(operation_id: &str, detail: &str) -> CanonicalError {
    let code = crate::api::rest::bulk_imports::BULK_VALIDATION_FAILED;
    PlanResource::failed_precondition()
        .with_precondition_violation("rows", detail, code)
        .with_precondition_violation("operation_id", operation_id, code)
        .create()
}

/// The 503s: fail closed, and tell the caller to retry.
///
/// The detail stays **server-side** and this renders none of it. An operator needs
/// to know which dependency is down; a caller only needs to know to retry, and a
/// dependency's internals -- an address, an artifact path -- are not its business.
/// The arms read alike because the answer *is* alike: no code, no detail, one
/// instruction.
///
/// **Whether the event is logged is each arm's own decision, not this function's.**
/// A log here reports every arm at request rate, which is right only for an event
/// that is per-request and reported nowhere else. `FixtureGateUnavailable` is a
/// boot-time fact already reported twice (the boot log and
/// `PricingAlarm::FixtureGateUnreadable`), so a line per refused publish, cutover,
/// window schedule, supersession and bulk-import row adds volume that tracks
/// traffic to an estate alerting on `level=error`; and `CatalogVersionUnavailable`
/// is reported by whichever seam produced it, each of which can name the registry's
/// own sentence or the budget it blew, where this function knows neither.
fn unavailable() -> CanonicalError {
    CanonicalError::service_unavailable().create()
}

impl From<DomainError> for CanonicalError {
    #[allow(
        clippy::too_many_lines,
        reason = "one arm per `DomainError` variant, which is what makes the ladder the \
                  single authority a reader can check against the enum. The cap therefore \
                  measures the enum's size, not this function's complexity, and the \
                  extractions that do reduce complexity are already here -- `precondition`, \
                  `denied`, `aborted` and `unavailable` carry every shared body. Splitting the \
                  match by class would put an arm's class in one function and its code in \
                  another, so a reader checking a variant's answer would need both."
    )]
    fn from(err: DomainError) -> Self {
        use DomainError as D;
        match err {
            // -- InvalidArgument (400) --
            D::InvalidRequest(detail) => PlanResource::invalid_argument()
                .with_constraint(detail)
                .create(),

            // -- FailedPrecondition -- the fail-closed publish rejections
            // (architectural 422, rendered 400; see the module note).
            D::RoundingPolicyUnresolved(detail) => {
                precondition("price", &detail, "ROUNDING_POLICY_UNRESOLVED")
            }
            D::PrecisionExceeded(detail) => {
                precondition("amount_minor", &detail, "PRECISION_EXCEEDED")
            }
            // §5's batch-level refusal, the same shape: architectural 422,
            // rendered 400, the code the discriminator. The subject is `rows`
            // because that is what the operator edits — the refusal is about the
            // file, not about any one row, which is what all-or-nothing means.
            // Through the helper for `precondition`'s stated reason: this ladder is
            // bounded by `clippy::too_many_lines` and an inline builder spends
            // several lines on one variant.
            D::BulkValidationFailed {
                operation_id,
                detail,
            } => bulk_refusal(&operation_id, &detail),
            // §5's second bulk-level refusal, and the repricing run's only
            // pre-commit one. Through `precondition` for that helper's own reason:
            // this ladder is bounded by `clippy::too_many_lines` and an inline
            // builder spends several lines on one variant. The subject
            // is `selector` because that is the whole of what an operator edits to
            // fix it — the adjustment and the instant were never consulted.
            D::RunSelectorEmpty(d) => precondition("selector", &d, repricing::RUN_SELECTOR_EMPTY),
            // The temporal sibling of the line above, and classified with it
            // rather than as a malformed argument: the instant parses, it is
            // simply finer than the quantum the catalog compares instants at
            // (D-144).
            D::TimestampPrecisionExceeded(detail) => {
                precondition("timestamp", &detail, "TIMESTAMP_PRECISION_EXCEEDED")
            }
            D::AmountNegative(detail) => precondition("amount_minor", &detail, "AMOUNT_NEGATIVE"),
            D::CurrencyInvalid(detail) => precondition("currency", &detail, "CURRENCY_INVALID"),
            D::FixtureMissing(detail) => precondition("model_kind", &detail, "FIXTURE_MISSING"),
            D::LifecycleForbidden(detail) => {
                precondition("lifecycle_state", &detail, "LIFECYCLE_FORBIDDEN")
            }
            // Beside the line above and in its class for `PlanRetiredNoSuccessor`'s
            // reason: state forbids the operation and there is no act that lifts it,
            // so it must not read as a conflict a retry resolves. It carries a code of
            // its own all the same — the remedy an operator does have is to cancel the
            // window the detail names, which `LIFECYCLE_FORBIDDEN` would not tell them.
            D::PriceWindowScheduled(detail) => {
                precondition("price_id", &detail, PRICE_WINDOW_SCHEDULED)
            }
            // Narrowed out of the line above (D-146). It stays in the
            // precondition class because retirement really is state forbidding
            // the operation — what it must not stay is indistinguishable from
            // the refusals an operator can work around.
            D::PlanRetiredNoSuccessor(detail) => {
                precondition("lifecycle_state", &detail, "PLAN_RETIRED_NO_SUCCESSOR")
            }
            // The sibling of the line above and refused for the same structural
            // reason — no further revision will ever open — but kept apart from
            // it deliberately: that code would assert a retirement that never
            // happened, and a retired plan still has a current revision, a warm
            // delta and a clone route forward where this one has none (D-145 as
            // amended, `02-plan-definition.md` §5).
            D::PlanAbandonedNoSuccessor(detail) => PlanResource::failed_precondition()
                .with_precondition_violation(
                    "lifecycle_state",
                    detail,
                    "PLAN_ABANDONED_NO_SUCCESSOR",
                )
                .create(),
            // The `cohort` rule's sibling (D-147): one axis-conditioned field,
            // one code. It is a publish refusal about the row's eligibility
            // class, not a malformed request, which is why it lands here and no
            // longer on the generic bad-request answer.
            D::GrandfatherUntilForbidden(detail) => PlanResource::failed_precondition()
                .with_precondition_violation(
                    "grandfather_until",
                    detail,
                    "GRANDFATHER_UNTIL_FORBIDDEN",
                )
                .create(),
            // The horizon door's own refusal (`inst-gs-tighten`), mapped beside its
            // sibling above and for the same reason: §5 types it 422, this platform
            // renders that 400, and the submitted instant is well-formed — it is
            // simply on the wrong side of the one the store already published. The
            // field named is the one the author edits.
            D::GrandfatherLoosenForbidden(detail) => PlanResource::failed_precondition()
                .with_precondition_violation(
                    "grandfather_until",
                    detail,
                    "GRANDFATHER_LOOSEN_FORBIDDEN",
                )
                .create(),
            // D-196's axis pair, and the same treatment for the same reason: a
            // usage line is a key axis, so a pair in the wrong place is a
            // precondition failure about where the row is filed rather than a
            // malformed field. The subject is the pair rather than one field —
            // the two halves are only wrong *together*.
            D::UsageLineAxisMismatch(detail) => PlanResource::failed_precondition()
                .with_precondition_violation(
                    "meter,dimension_key",
                    detail,
                    "USAGE_LINE_AXIS_MISMATCH",
                )
                .create(),
            // `SupersessionInstantPassed`'s twin, mapped the same way for the same
            // reason: §5 types both 422, which this platform renders 400, and the
            // instant parses perfectly well — it is simply behind a clock the
            // request cannot see. Two codes over one floor, because the operator is
            // told which act they were performing.
            D::CutoverInstantPassed(detail) => {
                precondition("cutover_at", &detail, "CUTOVER_INSTANT_PASSED")
            }
            // A composed unit that would leave the key uncovered. §5 types it 422 and
            // declares the code; the supersession's identical refusal has none, which
            // is why that one still travels as `LIFECYCLE_FORBIDDEN`.
            D::CutoverGap(detail) => precondition("cutover_at", &detail, "CUTOVER_GAP"),
            // Slice 11's three architectural 422s (§5), rendered 400 with the code
            // as the discriminator; see the module note.
            //
            // All three are precondition failures rather than malformed arguments,
            // and each names the field an operator actually edits: the body parsed
            // in every case, and what is wrong is a relationship between the request
            // and the world (a target's lifecycle state, a tenant's notice policy,
            // a delta set) rather than the shape of a value.
            D::MigrationTargetInvalid(d) => {
                precondition("target_plan_id", &d, migration::MIGRATION_TARGET_INVALID)
            }
            // D-49. The field is `effective_at` and not the policy, because the
            // policy is a different object under a different grant - an operator
            // who cannot wait must go and change that one, audited, first.
            D::MigrationNoticeTooShort(d) => {
                precondition("effective_at", &d, migration::MIGRATION_NOTICE_TOO_SHORT)
            }
            // The deltas ride the detail, enumerated: "blocked" alone is not an
            // actionable sentence when the remedy is to change the target, scope a
            // named subscription out, or fix a named add-on.
            D::MigrationBlocked(d) => precondition("scope", &d, migration::MIGRATION_BLOCKED),
            // D-63's future-only start. An architectural 422 (§5) rendered 400,
            // and a precondition failure rather than a malformed argument for
            // `TIMESTAMP_PRECISION_EXCEEDED`'s reason: the instant parses and is a
            // perfectly good instant, it is simply behind a clock the request
            // cannot see. It is the one of the four window refusals that is **not**
            // a conflict — a retry unchanged only puts the start further into the
            // past.
            D::WindowStartInPast(detail) => {
                precondition("effective_from", &detail, "WINDOW_START_IN_PAST")
            }
            // `inst-su-instant`'s two floors. An architectural 422 (§5) rendered
            // 400, and a precondition failure rather than a conflict for the reason
            // its neighbour above is one — the clock moved, not the world. It is
            // **not** a retry, though: the design set's remedy is that the unit is
            // recomposed against a fresh instant, and the detail carries that word,
            // because a caller who resubmits the same instant is refused identically
            // and lands further behind the floor each time.
            D::SupersessionInstantPassed(detail) => PlanResource::failed_precondition()
                .with_precondition_violation(
                    "changeover_instant",
                    detail,
                    crate::domain::supersession::SUPERSESSION_INSTANT_PASSED,
                )
                .create(),
            // `inst-fg-trailing`, fail-closed under D-182. An architectural 422 (§5)
            // rendered 400, and a precondition failure rather than a conflict for
            // the reason its sibling above is one, with the sign reversed: nothing
            // moved under the caller. The coverage this refuses to remove is the
            // coverage they composed the request against, so a retry unchanged is
            // refused identically — which is exactly what a 409 would falsely
            // promise a remedy for.
            //
            // The subject is `effective_to` on both producing paths, including the
            // cancel: a cancellation *is* the removal of an interval's coverage, and
            // naming the field the operator can still move — shorten to an instant
            // inside the floor, or schedule a successor — is D-146's line.
            D::WindowTrailingVoid(detail) => {
                precondition("effective_to", &detail, "WINDOW_TRAILING_VOID")
            }
            // §5's `THRESHOLD_INVALID`, an architectural 422 rendered 400 with the
            // code as the discriminator. The subject is `entries` on every producing
            // path: all five shape rules are about the entry set - its size, its
            // keys, and the basis inside each - and there is no second field on the
            // request an operator could move instead. `effective_from` has its own
            // refusal one layer down (`TIMESTAMP_PRECISION_EXCEEDED`), so folding it
            // in here would give one field two codes.
            D::ThresholdInvalid(detail) => precondition("entries", &detail, "THRESHOLD_INVALID"),

            // -- Aborted (409) -- conflicts the caller can resolve and retry.
            //
            // Almost every arm below shares one shape — [`aborted`] — and its own
            // doc says why the shape finally got a name: the comments already
            // narrated one family, the code never had.
            D::DuplicateScopeKey(detail) => {
                aborted(detail, crate::domain::import::DUPLICATE_SCOPE_KEY)
            }
            // §5 types `PHASE_ID_IN_USE` **409 outright** (D-340), and it is the
            // line above's class one table over: the store's own key refused, and
            // the remedy is to name an id the revision does not hold. It replaced
            // a bare `500` advising a retry that could never help. The spelling is
            // a literal here rather than a constant because no rule emits it —
            // `plan_rules`' roster is the codes the publish pipeline produces, and
            // a constant filed there would read as a rule this gear does not have.
            D::PhaseIdInUse(detail) => aborted(detail, "PHASE_ID_IN_USE"),
            D::StaleVersion(detail) => aborted(detail, "STALE_VERSION"),
            D::IdempotencyPayloadMismatch(detail) => {
                aborted(detail, "IDEMPOTENCY_PAYLOAD_MISMATCH")
            }
            // A conflict on mutable state that a retry resolves, so it keeps the
            // 409 the three above hold rather than collapsing into the 400
            // bucket (D-143): retry, and the answer is the stored response or
            // the mismatch refusal.
            D::IdempotencyKeyInFlight(detail) => aborted(detail, "IDEMPOTENCY_KEY_IN_FLIGHT"),
            // The same class one construct over (D-159): somebody else's write
            // reached the aggregate's serialization point first. It belongs with
            // the four above and not with `Internal`, because "retry" is the
            // whole remedy - and a 500 tells a client to page an operator about
            // a race.
            D::ConcurrentMutation(detail) => aborted(detail, "CONCURRENT_MUTATION"),
            // A uniqueness conflict on the plan's one draft slot (D-146), the
            // `DUPLICATE_SCOPE_KEY` class rather than a state-machine edge —
            // which is why it is here and not beside `LIFECYCLE_FORBIDDEN`.
            D::OpenDraftRevisionExists(detail) => aborted(detail, "OPEN_DRAFT_REVISION_EXISTS"),
            // One bundle per plan. The conflict class for the line above's
            // reason — a uniqueness conflict on a slot the plan has one of — and
            // the reason string is `BUNDLE_EXISTS_ON_PLAN` because §5 declares no
            // code for it and a caller still needs a discriminator that is not
            // the message. Owed-register entry B-9 asks the design set to
            // declare one; until it does this is the gear naming a refusal it
            // owns rather than borrowing a code that names something else.
            D::BundleExistsOnPlan(detail) => aborted(detail, "BUNDLE_EXISTS_ON_PLAN"),
            // One announcement per act. The line above's class and the line above's
            // reason for the spelling — §5 declares no code, and a caller still
            // needs a discriminator that is not the message. It is here and not
            // beside `LIFECYCLE_FORBIDDEN` because a taken key is not a state
            // machine's refusal, and here rather than with `ConcurrentMutation`
            // because the collision is deterministic: this class states the
            // conflict, it does not advise a retry.
            D::OutboxEventAlreadyEnqueued(detail) => {
                aborted(detail, "OUTBOX_EVENT_ALREADY_ENQUEUED")
            }
            // The precondition class and not `LIFECYCLE_FORBIDDEN`: each carries its
            // own remedy — see the two variants' declarations.
            D::SupersessionKeyDormant(detail) => {
                precondition("supersession", &detail, "SUPERSESSION_KEY_DORMANT")
            }
            D::SupersessionWouldEmptyWindow(detail) => {
                precondition("supersession", &detail, "SUPERSESSION_WOULD_EMPTY_WINDOW")
            }
            // A decision that lost a race with another decision
            // (`design/05-governance.md` §5, which types it 409 outright rather
            // than as one of the section's architectural 422s). It joins the
            // conflict class and not the precondition one: re-reading the record
            // is the remedy, and the caller's own request was never wrong.
            // §5's `PRECEDENCE_DUPLICATE`, one of the two overlay codes it
            // types **409 outright** rather than as an architectural 422. It
            // belongs in the conflict class for `WINDOW_OVERLAP`'s reason: what
            // refused the request is the state of a sibling row on a slot the
            // caller's own request never named.
            D::PrecedenceDuplicate(detail) => {
                aborted(detail, crate::domain::overlay_rules::PRECEDENCE_DUPLICATE)
            }
            // §5's second 409: the overlay analogue of `WINDOW_OVERLAP`, and
            // classified with it. Two intervals on one line key claim one
            // instant, and re-reading the key's overlays is the remedy.
            D::OverlayIntervalOverlap(detail) => aborted(
                detail,
                crate::domain::overlay_rules::OVERLAY_INTERVAL_OVERLAP,
            ),
            D::TaxonomyValueInUse(detail) => {
                aborted(detail, crate::domain::taxonomy::TAXONOMY_VALUE_IN_USE)
            }
            D::TaxonomyValueExists(detail) => {
                aborted(detail, crate::domain::taxonomy::TAXONOMY_VALUE_EXISTS)
            }
            D::PriceRowAbsent(detail) => PlanResource::not_found(detail)
                .with_resource(crate::api::rest::preview::PRICE_ROW_ABSENT)
                .create(),
            // A 404 that carries a code, for the line above's reason: the plan in
            // the path may exist and be perfectly editable, holding only a draft.
            // "Not found" alone sends an operator looking for a missing id.
            D::CloneSourceNotFound(detail) => PlanResource::not_found(detail)
                .with_resource(crate::api::rest::plans::CLONE_SOURCE_NOT_FOUND)
                .create(),
            D::RegionUnknown(detail) => PlanResource::failed_precondition()
                .with_precondition_violation(
                    "scope_key.region",
                    detail,
                    crate::domain::taxonomy::REGION_UNKNOWN,
                )
                .create(),
            // `RegionUnknown` one clause up, one taxonomy over: the subject is the
            // field the caller actually sent, so an operator reads the refusal and
            // knows which of the two rounding references — the row's or the
            // tenant default's — was refused.
            D::RoundingPolicyUnknown(detail) => PlanResource::failed_precondition()
                .with_precondition_violation(
                    "default_rounding_policy_ref",
                    detail,
                    crate::domain::taxonomy::ROUNDING_POLICY_UNKNOWN,
                )
                .create(),
            D::ApprovalNotPending(detail) => aborted(detail, "APPROVAL_NOT_PENDING"),
            // The one-pending-unit-per-subject conflict
            // (`07-pricewindow-linkage.md` `inst-co-single-pending`). It sits in
            // the conflict class with `OPEN_DRAFT_REVISION_EXISTS`, which is the
            // same shape one construct over: a slot the subject has exactly one
            // of is taken, the caller's request was never malformed, and the
            // remedy is to act on the unit the detail names.
            D::PendingChangeUnitExists(detail) => aborted(detail, "PENDING_CHANGE_UNIT_EXISTS"),
            // The TOCTOU pin's own refusal (`inst-ap-pin`). It joins the
            // conflict class rather than the precondition one for the reason
            // `APPROVAL_NOT_PENDING` does: the reviewer's request was right
            // about the world they were shown, and somebody else moved it.
            D::ApprovalContentMismatch(detail) => aborted(detail, "APPROVAL_CONTENT_MISMATCH"),
            // Two intervals on one canonical scope key claim one instant
            // (`07-pricewindow-linkage.md` §5, which types this 409 outright
            // rather than as one of the section's architectural 422s). It joins
            // the conflict class for `APPROVAL_NOT_PENDING`'s reason: the
            // request was right about what the caller could see, and what
            // refused it is a sibling window on a key their own request never
            // named.
            D::WindowOverlap(detail) => aborted(detail, "WINDOW_OVERLAP"),
            // A mutation of what §4 froze (`inst-ws-immutable`), 409 in §5's own
            // words. It joins the conflict class rather than the precondition one
            // because what refused it is where the clock stands: a `PATCH`
            // composed against a window whose end was five minutes away was right
            // when it was written, and re-reading the window is the remedy.
            D::WindowHistoricalImmutable(detail) => aborted(detail, "WINDOW_HISTORICAL_IMMUTABLE"),
            // The cancel path's own refusal (`inst-ws-cancel`), 409 in §5's own
            // words and kept apart from the line above deliberately: an operator
            // told the window is immutable stops, and one told it is not
            // *cancellable* shortens it through `effectiveTo` instead, which is
            // the operation §4 leaves them.
            D::WindowNotCancellable(detail) => aborted(detail, "WINDOW_NOT_CANCELLABLE"),
            // D-09's own two codes, `WindowOverlap`'s reason one plane over: the
            // request was well-formed and what refused it is a sibling
            // membership on the payer their own request never named.
            D::MembershipOverlap(detail) => aborted(detail, "MEMBERSHIP_OVERLAP"),
            D::MembershipConflict(detail) => aborted(detail, "MEMBERSHIP_CONFLICT"),
            // Architectural 422 (rendered 400; see the module note): the
            // request was well-formed and what refused it is the taxonomy
            // never having declared `{group}`, or having retired it.
            D::GroupUnknown(detail) => precondition("group", &detail, "GROUP_UNKNOWN"),
            // `inst-re-references`, 409 in §5's own words. The conflict class for
            // `TaxonomyValueInUse`'s reason and it is the same fact one plane
            // over: what refused the retirement is a bundle or an add-on
            // override the caller's own request never named, and going to look
            // at the enumerated referrers is the whole remedy.
            D::RetirePlanReferenced(detail) => aborted(detail, RETIRE_PLAN_REFERENCED),
            // The line above's sibling, and a conflict for the same reason: what
            // refuses the retirement is a live schedule aimed at this plan, which
            // the caller's own request never named. Kept apart from it because the
            // remedies are different acts on different objects - one goes and
            // re-composes a bundle, the other cancels a migration or waits for it.
            D::RetireTargetOfMigration(detail) => aborted(detail, RETIRE_TARGET_OF_MIGRATION),
            // The line above's other sibling, and a conflict for the same reason
            // one plane over: what refuses the retirement is a decided cutover
            // still ahead of the clock, which the caller's own request never
            // named. Kept apart from both because the remedy is a third act — wait
            // for the instant, or withdraw the unit while it can still be
            // withdrawn. Its code is the one this gear mints rather than reads off
            // §5; the constant's doc holds what is owed for that.
            D::RetireOverLiveCutover(detail) => aborted(detail, RETIRE_OVER_LIVE_CUTOVER),
            // D-34's one named refusal, 409 in §5's own words. A conflict on
            // mutable state and not a 400: the request was well-formed and the
            // authority sufficient, the run simply finished first. The code names
            // **completion** rather than the pre-D-34 `MIGRATION_ALREADY_EFFECTIVE`
            // because an `in_progress` run past its effective date is still
            // cancellable, so "already effective" named the wrong fact.
            D::MigrationCompleted(detail) => {
                aborted(detail, crate::domain::migration::MIGRATION_COMPLETED)
            }

            // -- PermissionDenied (403) -- the two audited authority refusals.
            //
            // Both are **403 in the design set's own words** (§5), and both are
            // written to `pricing_audit_log` as a `deny` record before they
            // reach here. Neither is retriable and neither names a field to fix,
            // which is why they are not in the 400 bucket with the malformed
            // requests: there is nothing about the request to correct, only
            // somebody else to ask.
            //
            // The detail is **dropped** rather than folded into `reason`; see
            // the module note on why the code has to stand alone there.
            D::SelfApprovalForbidden(_detail) => denied("SELF_APPROVAL_FORBIDDEN"),
            D::RegionScopeDenied(_detail) => denied("REGION_SCOPE_DENIED"),
            // Beside the two above and for their reason: an authority refusal,
            // 403, detail dropped. `inst-as-void`'s identity half — closing a
            // review that is not yours releases the scope keys it held.
            D::WithdrawForbidden(_detail) => denied("WITHDRAW_FORBIDDEN"),

            // An architectural 422 (§5) rendered 400, with the code as the
            // discriminator; see the module note. It is a precondition failure
            // and not a malformed argument because the body parsed — the field
            // is simply absent, and `reason` is the field an author fills in.
            D::ReasonRequired(detail) => precondition("reason", &detail, "REASON_REQUIRED"),

            // -- The aggregate validation envelope (architectural 422, rendered 400) --
            //
            // Every blocking violation is carried as its own precondition
            // violation, so the author sees the whole remediation list in one
            // response. Advisory warnings are deliberately NOT rendered here:
            // this envelope exists only on the rejecting path, and a warning must
            // reach the author on the succeeding path too.
            //
            // **The warning does not ride the validation report on the response body**
            // (D-197). No response body carries it — all three publish success views are
            // `{ outcome, materiality, approval, receipt }`, and the pre-check's report is
            // read only to build *this* envelope. So `PLAN_SIZE_SOFT_CAP_EXCEEDED` and
            // `TIER_BAND_PRICE_INCREASE` are computed and discarded, and D-160's reason for
            // making the caps advisory rather than blocking has no channel to be advisory
            // through. Leaving warnings out of this envelope is right; what is owed is the
            // field on the succeeding path, by the group that owns the publish response DTO.
            D::ValidationFailed(report) => {
                let mut violations = report.violations.into_iter();
                let Some(first) = violations.next() else {
                    // A rejection with nothing to remediate is a bug in the
                    // pipeline, not a client error: reporting it as a
                    // precondition failure would tell the author to fix
                    // something the response does not name.
                    return Self::internal(
                        "pricing: publish validation failed with an empty report",
                    )
                    .create();
                };
                let mut builder = PlanResource::failed_precondition().with_precondition_violation(
                    first.subject,
                    first.detail,
                    first.code,
                );
                for violation in violations {
                    builder = builder.with_precondition_violation(
                        violation.subject,
                        violation.detail,
                        violation.code,
                    );
                }
                builder.create()
            }

            // -- NotFound (404) -- absent and scoped-out are the same answer.
            D::NotFound { subject, id } => {
                PlanResource::not_found(format!("{subject} {id} not found"))
                    .with_resource(id)
                    .create()
            }

            // -- The registry's own refusal (400) -- permanent, and told apart
            // from its outage by both the status and the code.
            //
            // The status is chosen against the caller's retry policy and nothing
            // else. See the module note "Why a registry refusal is a 400 and a
            // registry outage is a 503" and
            // `crate::domain::ports::registry_failure`.
            //
            // The detail is **kept**, where the 503 arm below drops its own: it is
            // the registry's sentence naming the SKU or the closed version, which
            // is the only thing the author can act on. A 400 that names nothing to
            // fix leaves retrying as the caller's only move, which is the
            // behaviour this arm exists to end.
            D::CatalogVersionRejected(detail) => {
                precondition("catalog_version", &detail, "CATALOG_VERSION_REJECTED")
            }

            // -- Unavailable (503) -- fail closed, retry later. See
            // [`unavailable`] for why the detail does not travel, and why the log
            // is each arm's decision rather than the helper's.
            //
            // **The two unreported-here refusals share an arm** because they share
            // the whole answer, not because they share a cause. Each is silent for
            // its own reason. A registry outage is reported where it is raised, by
            // a seam holding what this ladder does not: `infra::registry_deadline`
            // has the budget on a timeout and the registry's own sentence on an
            // unreachable peer, `api::rest::catalog_skus` has the product
            // catalog's. And a closed fixture gate refuses *every* call while it is
            // closed, so a line here would report one boot-time fact at the publish
            // plane's traffic rate — the boot log and
            // `PricingAlarm::FixtureGateUnreadable` already carry it, and neither
            // grows with load.
            //
            // The fixture gate belongs here and **not** beside `FixtureMissing`,
            // which is a 400 naming `model_kind`: an undeployed or unparseable
            // `registry.toml` is a deployment state a later request may find
            // changed, so retry is the right instruction and the caller is not the
            // party at fault.
            D::CatalogVersionUnavailable(_) | D::FixtureGateUnavailable(_) => unavailable(),
            // The one 503 that reports itself, because nothing else can: the read
            // path this answers for has no producer yet, so no seam upstream raises
            // it, and a read-model outage is per-request by nature rather than a
            // deployment fact standing until somebody redeploys.
            D::ReadModelUnavailable(detail) => {
                tracing::error!(
                    dependency = "read_model",
                    detail,
                    "bss-pricing: dependency unavailable"
                );
                unavailable()
            }

            // -- Internal (500) --
            D::Internal(detail) => CanonicalError::internal(format!("pricing: {detail}")).create(),
        }
    }
}

#[cfg(test)]
#[path = "error_mapping_tests.rs"]
mod error_mapping_tests;
