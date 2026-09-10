//! Persistence layer for the catalog Foundation: the migration chain, the
//! `SeaORM` entities over its tables, and the repositories that read them.
//!
//! Everything here is infrastructure by construction (DE0301): the domain layer
//! never learns that a `Plan` is a row. Where a repository hands back a value
//! the rest of the system reasons about, it maps at this boundary — see
//! [`repo::PinFrontierRepo`], which reads a `pricing_pin_frontier` row and
//! returns the SDK's `PinFrontier`.
//!
//! Every physical table carries the gear-name prefix `pricing_`
//! (`design/01-foundation.md` §3.7) and lives in schema `bss` on Postgres.
//! `SQLite` is the fast non-production test backend: it mirrors the shape
//! (`uuid` -> `text`, `timestamptz` -> `text`, `jsonb` -> `text`) and, unlike
//! the sibling ledger's substrate, it also mirrors the **append-only guards** —
//! `RAISE(ABORT,...)` triggers stand in for PL/pgSQL `RAISE EXCEPTION`, so the
//! column whitelists are exercised by the fast test suite rather than only by
//! the Docker-gated Postgres one.

pub mod entity;
pub mod migrations;
pub mod odata_mapping;
pub mod repo;

use crate::domain::error::DomainError;

/// A storage-layer failure.
///
/// Deliberately small: the repositories translate a driver error into `Db` and
/// keep exactly the typed refusals the Foundation's own invariants need.
/// A variant per SQL error class would put the database's vocabulary in the
/// gear's error surface, which is the coupling this boundary exists to prevent.
///
/// The authoring refusals below are **three answers where a lesser design has
/// one**. A repository that reported every failed compare-and-swap as a single
/// "conflict" would leave the caller unable to tell "refresh and retry" from
/// "you are editing a revision that is frozen forever" from "that revision is
/// gone" — and those want three different behaviours from the operator, only
/// one of which is a retry.
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum RepoError {
    /// Underlying database or scope failure.
    #[error("pricing repo db error: {0}")]
    Db(String),
    /// A stored value could not be interpreted as the type its column is
    /// declared to hold — a `CHECK`-constrained token outside its enumeration,
    /// or a `catalog_version` outside the unsigned range the SDK type carries.
    /// An invariant breach, never a caller mistake.
    #[error("pricing repo: corrupt stored value: {0}")]
    CorruptRow(String),
    /// An attempt to move the pin-eligibility frontier to a version at or below
    /// where it already stands.
    ///
    /// Refused rather than swallowed: the projector advances the frontier only
    /// inside the transaction completing the frontier's **next** version in
    /// order, so an equal-or-lower target means the caller's ordering
    /// assumption has broken (a duplicate completion, or a re-drive running out
    /// of order). Treating it as a no-op would hide exactly the defect the
    /// materialized frontier exists to make impossible — a pin resolving two
    /// different contents over time (D-136).
    #[error(
        "pricing repo: pin frontier for tenant {tenant} stands at {current}, \
         refusing to advance to {requested}"
    )]
    FrontierRegression {
        /// The tenant whose frontier was targeted.
        tenant: String,
        /// Where the frontier stands now.
        current: u64,
        /// The version the caller asked to advance to.
        requested: u64,
    },
    /// The named subject does not exist **or lies outside the caller's scope**.
    ///
    /// Deliberately the same answer either way. A repository that answered
    /// "forbidden" for a row belonging to another tenant would confirm that the
    /// row exists, which is the existence leak the SQL-level scoping is there to
    /// close; the catalog is commercially sensitive, so absence is what a
    /// foreign scope sees.
    #[error("pricing repo: {subject} {id} not found")]
    NotFound {
        /// The kind of thing that was looked for (`plan revision`, `price`).
        subject: String,
        /// The reference the caller supplied.
        id: String,
    },
    /// A write whose submitted row version is not the row's current one.
    ///
    /// Carries **both** versions, because the difference is the diagnosis: a
    /// caller one version behind never refreshed, while a caller many versions
    /// behind is a bulk run colliding with interactive editing — the collision
    /// `fr-concurrent-edit` exists to make visible instead of silently letting
    /// one side win.
    #[error(
        "pricing repo: {subject} {id} stands at row version {current}, \
         refusing a write submitted against {submitted}"
    )]
    StaleRowVersion {
        /// The kind of thing that was written.
        subject: String,
        /// The reference the caller supplied.
        id: String,
        /// The row version the store holds.
        current: u64,
        /// The row version the caller submitted.
        submitted: u64,
    },
    /// A content mutation aimed at a revision that is not a `draft`.
    ///
    /// Distinct from [`RepoError::StaleRowVersion`] because it is not
    /// retryable: no refresh will make a published, superseded or retired
    /// revision editable, and a caller told merely "conflict" would loop. The
    /// remedy is a different operation entirely — open a new revision (§4.3).
    #[error("pricing repo: {subject} {id} is {state}; only draft content is mutable")]
    NotDraft {
        /// The kind of thing that was written.
        subject: String,
        /// The reference the caller supplied.
        id: String,
        /// The lifecycle state the row is actually in.
        state: String,
    },
    /// A supersession aimed at a row that is not the key's **current** one.
    ///
    /// Deliberately not [`RepoError::NotDraft`], for the reason D-146 carved
    /// [`RepoError::NoSuccessorRevision`] out of it: that variant's sentence —
    /// "only draft content is mutable" — names as the remedy an operation this
    /// caller is not attempting, and an operator following it would go and open a
    /// revision that has nothing to do with the refusal. Nor is it a stale
    /// version: the predecessor's tag is **frozen** (D-141), so there is no
    /// refresh-and-retry, and the two states it can actually be found in mean
    /// different things — `superseded` says this unit, or another one, has already
    /// committed on this key and the remedy is to recompose against the new
    /// current row (`inst-su-instant`'s recompose), while `draft` says the target
    /// was never published at all and the remedy is a plain publish plus a window
    /// schedule (`inst-su-compose`'s revival path).
    #[error("pricing repo: {subject} {id} is {state}; only a published row is supersedable")]
    NotSupersedable {
        /// The kind of thing that was aimed at.
        subject: String,
        /// The reference the caller supplied.
        id: String,
        /// The lifecycle state the row is actually in.
        state: String,
    },
    /// A request for a **successor** revision on a plan whose current revision
    /// can never be superseded.
    ///
    /// Deliberately not [`RepoError::NotDraft`], which it superficially
    /// resembles and which answered this case first. That variant's sentence
    /// — "only draft content is mutable" — is about *editing a revision's
    /// content*, and its whole value is the remedy it implies: stop editing
    /// this revision and open the next one. Here the caller asked for exactly
    /// that next revision, so the same sentence would name as the remedy the
    /// operation it is refusing, and an operator following it would loop. The
    /// real ground is different too: not that the current revision is frozen —
    /// every publishable predecessor is — but that it can never flip
    /// `superseded`, so the successor would be unpublishable from the moment it
    /// was opened. It carries `PLAN_RETIRED_NO_SUCCESSOR` (D-146) rather than
    /// the frozen revision's `LIFECYCLE_FORBIDDEN`: what the two refusals ask an
    /// operator to do differs, and until the codes did too, telling them apart
    /// meant reading prose.
    #[error(
        "pricing repo: plan {plan_id} stands at a {state} revision, which can never be \
         superseded; it takes no successor"
    )]
    NoSuccessorRevision {
        /// The plan a successor was asked for.
        plan_id: String,
        /// The lifecycle state its current revision is in.
        state: String,
    },
    /// A request to open a revision on a plan that already has its one open
    /// draft.
    ///
    /// A plan has at most one concurrently editable shape
    /// (`uq_pricing_plan_open_draft`). The refusal names the revision that
    /// holds the slot, so the caller can go and edit it rather than guess which
    /// of its own requests won — which is why it carries
    /// `OPEN_DRAFT_REVISION_EXISTS` (D-146) and not the lifecycle refusal it was
    /// once folded into: a uniqueness conflict on the draft slot is not a
    /// transition the state machine denies, and the operator's next action is a
    /// real one.
    #[error("pricing repo: plan {plan_id} already has an open draft at revision {revision}")]
    OpenDraftExists {
        /// The plan whose draft slot is taken.
        plan_id: String,
        /// The revision number of the draft holding it.
        revision: u64,
    },
    /// A client idempotency key was reused for a **different** request.
    ///
    /// Neither answer available is right, which is why this is its own refusal
    /// and not a conflict inviting a retry: replaying the stored response would
    /// report work the caller never asked for, and re-executing would break the
    /// at-most-once promise the key exists to make. The refusal names the key
    /// and the operation it was scoped to rather than either payload — the
    /// table stores digests on purpose, and what an operator needs is which key
    /// was reused, not what the two requests said.
    #[error(
        "pricing repo: idempotency key {client_key} on operation {operation} \
         was first used with a different request payload"
    )]
    IdempotencyPayloadMismatch {
        /// The operation the key is scoped to.
        operation: String,
        /// The client-supplied key that was reused.
        client_key: String,
    },
    /// Another current row already occupies the canonical scope key.
    ///
    /// The key's own rendering is carried verbatim: a `DUPLICATE_SCOPE_KEY`
    /// rejection that dropped an axis would report a collision between two rows
    /// that do not actually share a key.
    #[error("pricing repo: duplicate canonical scope key: {0}")]
    DuplicateScopeKey(String),
    /// A `phases` write named a `phase_id` this plan revision already holds
    /// (`pricing_plan_phase_pkey`, **D-340**).
    ///
    /// [`RepoError::DuplicateScopeKey`]'s shape, deliberately, because it is the
    /// same sentence about a different key: *this key is already held*. The id is
    /// carried verbatim for that variant's reason — a refusal that dropped it
    /// would report a collision without saying what collided, which is the fault
    /// D-340 measured. Before it existed the violation reached the caller as
    /// [`RepoError::Db`] and therefore as `500 Internal` advising a retry, for a
    /// class no retry can fix.
    ///
    /// After D-340 widened the key to `(tenant_id, plan_id, plan_revision,
    /// phase_id)` the only reachable collision is a payload naming one id twice:
    /// the facet deletes the revision's phase rows and re-inserts them wholesale
    /// in one transaction, so no other writer can hold a slot this write wants.
    /// It stays a typed refusal rather than a payload pre-check because the
    /// constraint is the store's and one code per constraint survives the key
    /// being narrowed again.
    #[error(
        "pricing repo: phase id {0} is named twice by this write, or is already held by this \
         plan revision; a revision holds each phase id at most once"
    )]
    PhaseIdInUse(String),

    /// The price row is already held by an **in-flight bulk operation**
    /// (`uq`/PK on `pricing_bulk_row_lock`, `inst-bk-lock`).
    ///
    /// The mutual exclusion is the key `(tenant_id, price_id)` itself: two runs
    /// cannot hold one row because the second insert collides. Raised by the key
    /// rather than by a read, [`RepoError::PendingKeyHeld`]'s arrangement — and
    /// unlike that variant this one **does** carry the holder, because a lock row
    /// is inserted by the run that is already `committing` and therefore already
    /// committed. Naming it is the whole of `fr-concurrent-edit`'s requirement:
    /// an interactive edit refused by a bulk run has to say which run.
    #[error(
        "pricing repo: price row {price_id} is held by bulk operation {bulk_operation_id}; wait \
         for it to finish or abort it"
    )]
    BulkRowLocked {
        /// The row somebody tried to take or edit.
        price_id: String,
        /// The run holding it — what the conflict names.
        bulk_operation_id: String,
    },
    /// A draft price row was discarded while a window stands on it
    /// (`fk_pricing_price_window_price`).
    ///
    /// [`RepoError::BulkRowLocked`]'s arrangement one table over: the key
    /// references `pricing_price (price_id)` with no cascade on either backend,
    /// so past this refusal the DELETE meets the foreign key and comes back as
    /// [`RepoError::Db`] — the operator told the store is broken about a row they
    /// can see. Raised by a read taken inside the delete's own transaction, which
    /// is what lets it name the window; the key can only say that something
    /// referred to the row.
    ///
    /// It names the window because that is the object the operator can act on —
    /// they cancel it so nothing takes effect. It does **not** promise the delete
    /// then succeeds: `pricing_price_window` refuses `DELETE` outright, so the
    /// referrer never leaves and the draft stays. See
    /// [`crate::domain::window::PRICE_WINDOW_SCHEDULED`], which is why this
    /// renders as a precondition rather than a retriable conflict.
    #[error(
        "pricing repo: price row {price_id} carries window {window_id}; a window binds the row \
         it is scheduled on and cancelling it does not release the binding, so this row cannot \
         be deleted - abandon the open plan revision instead, which leaves the row unsellable \
         under a revision nothing publishes from"
    )]
    PriceWindowScheduled {
        /// The row somebody tried to delete.
        price_id: String,
        /// The window standing on it — what the refusal names.
        window_id: String,
    },
    /// A run's lock acquisition failed in a way that may have left **its own**
    /// lock rows behind (`inst-bk-lock`).
    ///
    /// Distinct from [`RepoError::Db`] because the caller has to act differently:
    /// the run **must** be left `committing`. `infra::bulk::abandon_committing_run`
    /// — the body of `POST …/bulk-imports/{id}/abort`, the only remedy — refuses a
    /// run in any other state, `pricing_bulk_row_lock` has no sweeper, and D-37's
    /// lease takeover is unbuilt. A run landed terminal on this error freezes every
    /// row of its batch against interactive editing and against every later bulk
    /// run, permanently, named by an operation that is over.
    ///
    /// The underlying failure is flattened to a string for [`repo_failure`]'s
    /// reason: this type is `Clone + Eq` and cannot hold a boxed source. It is
    /// **not** named `source` — `thiserror` reads that name as `Error::source` and
    /// a `String` is not an `Error`.
    #[error(
        "pricing repo: bulk operation {operation_id} could not establish its row locks and \
         locks may still be held: {cause}; abort the run to release them"
    )]
    BulkLocksHeld {
        /// The run whose locks may still be held.
        operation_id: String,
        /// The underlying failure, flattened.
        cause: String,
    },
    /// The `(scope_class, precedence)` slot is already held by another
    /// **published** overlay (`uq_pricing_price_overlay_precedence`, L2).
    ///
    /// Raised by the index rather than by a comparison, and it is the other half
    /// of a rule `domain::overlay_rules::check_precedence` also checks — the
    /// loser of two submits that both read a free slot before either wrote.
    /// [`RepoError::PendingKeyHeld`]'s arrangement, and it carries no holder for
    /// that variant's reason: at the moment the index fires, the transaction
    /// that inserted the winning row may not have committed, so there is no
    /// overlay this transaction is entitled to read.
    #[error(
        "pricing repo: this scope class and precedence are already held by a published \
         overlay; choose another precedence, or supersede the overlay holding it"
    )]
    OverlayPrecedenceHeld,
    /// A row would publish with **no rounding resolution at all** — it carries no
    /// `rounding_policy_ref` and the tenant has no default.
    ///
    /// Raised where the resolution is frozen rather than where it is judged, and
    /// that is the whole point. `foundation.rounding_policy_resolved` refuses
    /// exactly this, and it runs on one of `publish_rows`' four callers: the
    /// other three — `infra::supersession`, `infra::cutover` and mass repricing
    /// through `commit_supersession` — resolve the default themselves and freeze
    /// whatever it is. So the guarantee `pricing_price`'s migration header states of the
    /// column ("`NULL` for a published row cannot happen") and
    /// [`price_repo::FrozenResolution`] repeats holds on one door in four.
    /// Refusing at the freeze restores it by
    /// construction, which is the argument
    /// [`AuthzError::Denied`](crate::api::rest::authz::AuthzError) was reshaped
    /// on: make the omission impossible rather than remembered.
    ///
    /// Carries `ROUNDING_POLICY_UNRESOLVED` through [`repo_failure`], the same
    /// code the publish rule reports, because it is the same fault reaching the
    /// operator through a different door — and the remedy the rule's message
    /// names (set the row's policy, or configure the tenant default) is the
    /// remedy here too.
    #[error(
        "pricing repo: price row {price_id} carries no roundingPolicyRef and this tenant has \
         no default rounding policy; rounding decides the last minor unit of every charge, so \
         the publish stops rather than freezing nothing"
    )]
    RoundingPolicyUnresolved {
        /// The first row of the set whose resolution is absent.
        price_id: String,
    },
    /// A row would publish with **no tax category at all** — it states no
    /// `tax_category_ref` and its region declares no default.
    ///
    /// [`RepoError::RoundingPolicyUnresolved`]'s twin, one frozen column over, and
    /// it exists for the identical reason stated in that variant's doc: the
    /// guarantee has to hold at the **freeze**, not at one of the four doors that
    /// reach it. `TaxBasisComplete` judges this and runs under `run_publish_rules`
    /// — the ordinary publish before the flip, and the cutover and mass repricing
    /// over the post-commit shape — while the plain supersession runs no such set
    /// and `infra::supersession::refuse_unresolved_tax_category` therefore asks
    /// ahead of its registry request. That closed the live gap; what
    /// it did not do is make `price_repo::publish_rows` hold the guarantee by
    /// construction, so a **new** caller that neither runs the rule set nor asks
    /// first could still freeze `resolved_tax_category` NULL onto a published row
    /// — the state `pricing_price`'s migration header and `price_repo::FrozenResolution`'s
    /// doc both call impossible — and `trg_pricing_price_append_only`'s frozen-column guard then
    /// makes it permanent, with no repair available at all.
    ///
    /// Carries `TAX_BASIS_INCOMPLETE` through [`repo_failure`] as a
    /// [`DomainError::ValidationFailed`] report, which is the shape
    /// `refuse_unresolved_tax_category` already answers with: one fault, four
    /// doors, one code, and the same remedy — set the row's category, or declare a
    /// default on the region.
    #[error(
        "pricing repo: price row {price_id} states no taxCategoryRef and its region declares \
         no default, so it resolves no tax category; `taxCategory` is a pinned D-48 v1 \
         descriptor element and a published row carries one, so the publish stops rather \
         than freezing nothing onto a row nothing can then repair"
    )]
    TaxCategoryUnresolved {
        /// The first row of the set whose category is absent.
        price_id: String,
    },
    /// The overlay already holds an open draft revision, named by the refusal
    /// (`uq_pricing_price_overlay_open_draft`, D-92).
    ///
    /// [`RepoError::OpenDraftExists`]' sibling one plane over, and deliberately
    /// not that variant: its sentence says *plan*, and an operator told a plan
    /// holds their overlay's draft slot would go looking in the wrong place.
    #[error(
        "pricing repo: price overlay {price_overlay_id} already has an open draft at \
         revision {revision}"
    )]
    OverlayOpenDraftExists {
        /// The overlay whose draft slot is taken.
        price_overlay_id: String,
        /// The revision number of the draft holding it.
        revision: u64,
    },
    /// A plan already carries a bundle (`uq_pricing_bundle_plan`).
    ///
    /// Minted here rather than reported as a driver error because it is a rule
    /// an operator can act on — a plan is a bundle or it is not — and because
    /// the index message differs between the two backends, so a caller matching
    /// on text would be matching on the engine.
    #[error("pricing repo: plan {plan_id} already carries a bundle")]
    DuplicateBundleOnPlan {
        /// The plan that already has one.
        plan_id: String,
    },
    /// Another write of this aggregate reached a per-aggregate serialization
    /// point first (D-159).
    ///
    /// Recognised from the driver's **own** unique-violation class
    /// ([`DbErr::sql_err`]), not from a message substring, so both backends are
    /// covered by one predicate: SQLSTATE `23505` on Postgres, error codes 1555
    /// and 2067 on `SQLite`.
    ///
    /// It narrows [`RepoError::Db`] rather than replacing it: everything that is
    /// not a unique violation at one of those three points is still a storage
    /// fault and still reaches the caller as one.
    #[error("pricing repo: another mutation of {aggregate} committed first; retry")]
    ConcurrentMutation {
        /// The aggregate whose serialization point refused — what the caller is
        /// told to retry against.
        aggregate: String,
    },
    /// The authored row names one `(meter, dimensionKey)` line and its key
    /// names another (D-196).
    ///
    /// A refusal rather than a rewrite, and `price_repo::check_authored_usage_line`
    /// carries the argument: the wire can express a meter, so a disagreement is
    /// the caller's mistake, and a door that silently picked a winner would make
    /// the D-82 unit guard's `meter` and `dimensionKey` clauses unreachable.
    ///
    /// It renders under the axis rule's own code — the pair is a key axis, and
    /// the fact the operator has to act on is that the two halves of their
    /// request disagree about it.
    #[error(
        "pricing repo: the authored row prices the line {row_line} while its canonical scope \
         key names {key_line}; a row's meter and dimensionKey are axes of its key"
    )]
    UsageLineDisagrees {
        /// The line the key names, as `meter/dimensionKey`.
        key_line: String,
        /// The line the row's own fields name.
        row_line: String,
    },

    /// A grandfathering horizon authored on a row that is not a grandfathered
    /// generation.
    ///
    /// `chk_pricing_price_grandfather_until` is a **physical** CHECK — only an
    /// `existing_grandfathered` row may carry a non-null `grandfather_until` —
    /// and `grandfather_until` is ordinary caller-supplied content on the draft
    /// plane. Without this refusal the pairing is discovered by the driver, so
    /// the caller is told the store failed and the operator reads a 500 for a
    /// request they could have fixed by clearing one field.
    ///
    /// **The published plane answers the same question one layer up.**
    /// `infra::grandfather` asks the class before it writes and renders the
    /// identical code from [`DomainError::GrandfatherUntilForbidden`] — not
    /// through this variant, because that door holds the whole canonical scope key
    /// and can refuse before the store is touched at all. Two sites, one code, one
    /// rule; the physical CHECK is the floor under both.
    ///
    /// [`DomainError::GrandfatherUntilForbidden`]: crate::domain::error::DomainError::GrandfatherUntilForbidden
    ///
    /// The pairing is now normative and has a code of its own —
    /// `GRANDFATHER_UNTIL_FORBIDDEN` (D-147), the `cohort` biconditional's
    /// sibling: one axis-conditioned field, one code. Before it, this refusal
    /// was the code's own and could only borrow the generic bad-request answer,
    /// which told a consumer nothing about which field to clear.
    #[error(
        "pricing repo: only an existing_grandfathered row may carry a grandfathering \
         horizon; this key is {eligibility}"
    )]
    GrandfatherHorizonOffClass {
        /// The eligibility class the row's canonical scope key actually holds.
        eligibility: String,
    },
    /// An authored value lies outside the range the column that stores it can
    /// hold.
    ///
    /// The domain counts quantities in `u64` because a quantity only ever goes
    /// up, while every column that stores one is a signed `bigint`, so the top
    /// half of the domain range has no storage at all. Deliberately **not**
    /// [`RepoError::CorruptRow`]: the same mismatch read *out* of a column
    /// means the table was written around, while a value arriving *on a
    /// request* is a caller mistake and the request can be reshaped — which is
    /// the line between an internal fault and a bad one. The field is named so
    /// an author corrects one number instead of resubmitting a row and
    /// guessing.
    #[error("pricing repo: {field} {value} is outside the range its column can hold")]
    ValueOutOfRange {
        /// The authored field, located precisely enough to edit.
        field: String,
        /// The value, as authored.
        value: String,
    },
    /// An authored instant carries precision below the millisecond quantum
    /// (D-144).
    ///
    /// Checked where the value enters the store rather than left to the column,
    /// which would take it: `timestamptz` holds microseconds, so a finer instant
    /// persists silently and is then compared for equality against one produced
    /// at the quantum. The field is named because clearing or rounding one value
    /// is the whole remedy.
    #[error("pricing repo: {field} {value} is finer than the millisecond quantum")]
    TimestampPrecisionExceeded {
        /// The authored field the instant arrived on.
        field: String,
        /// The instant, as authored.
        value: String,
    },
    /// A duplicate arrived while the claim it collided with is unanswered
    /// (D-143).
    ///
    /// There is nothing to replay and nothing to re-execute, so the only other
    /// answer available is a response nobody ever produced. Retrying is the
    /// remedy and it is one a client can act on: shortly after, the key is
    /// either answered — and the retry is replayed the stored response — or it
    /// carries a different payload, and the retry is
    /// [`RepoError::IdempotencyPayloadMismatch`]. The refusal names the key and
    /// the operation for the same reason that one does.
    #[error(
        "pricing repo: idempotency key {client_key} on operation {operation} \
         is claimed and not yet answered"
    )]
    IdempotencyKeyInFlight {
        /// The operation the key is scoped to.
        operation: String,
        /// The client-supplied key that is held.
        client_key: String,
    },
    /// A decision was asked of an approval record that has already been decided
    /// or voided (`inst-as-immutable`).
    ///
    /// **A conflict rather than a precondition failure**, and the classification
    /// is the useful part: the caller's request was well-formed and their
    /// authority was sufficient — somebody simply got there first, or the list
    /// they were acting on is stale. The remedy is to re-read the record, which
    /// is what a 409 asks for; told `LIFECYCLE_FORBIDDEN` (400) instead, a
    /// reviewer would read it as their own request being malformed.
    ///
    /// It carries the code `design/05-governance.md` §5 declares —
    /// `APPROVAL_NOT_PENDING` — and the state the record is actually in, because
    /// "somebody approved it" and "the submitter withdrew it" are different
    /// things to be told.
    #[error(
        "pricing repo: approval {approval_id} is {state}; only a submitted record is decidable"
    )]
    ApprovalNotPending {
        /// The record that was decided.
        approval_id: String,
        /// The state it is actually in.
        state: String,
    },
    /// The canonical scope key a unit tried to hold is already held by a
    /// `submitted` unit (`inst-co-single-pending`, `PENDING_CHANGE_UNIT_EXISTS`).
    ///
    /// **Raised by `uq_pricing_approval_key_pending`, not by a comparison**, and
    /// that is the whole reason the register is a table. `ApprovalService::submit`
    /// also *checks* for a holder inside its transaction and produces the ordinary
    /// 409 — one that can name the unit holding the key, which an index violation
    /// cannot. This variant is the other half: the loser of two concurrent submits,
    /// both of which read a free key before either wrote. Under `READ COMMITTED`
    /// the check cannot see that, so without the index both would commit and one
    /// key would be held by two approvable units.
    ///
    /// It carries the key and **not** the holding unit, deliberately: at the moment
    /// the index fires, the transaction that inserted the winning row may not have
    /// committed, so there is no unit this transaction is entitled to read. The
    /// caller's remedy is identical either way — re-read the register and find the
    /// holder — and naming a unit that a rollback might unmake would be worse than
    /// naming none.
    #[error(
        "pricing repo: scope key {key} is already held by a submitted approval unit; decide it, or \
         withdraw it to free the key"
    )]
    PendingKeyHeld {
        /// The canonical scope key the register refused a second hold on.
        key: String,
    },
    /// The tenant already holds a `submitted` approval-threshold policy proposal
    /// (D-192 clause (2), `PENDING_CHANGE_UNIT_EXISTS`).
    ///
    /// **Raised by `uq_pricing_approval_policy_pending`, not by a comparison**, and
    /// it is [`RepoError::PendingKeyHeld`]'s sibling one plane over: same rule shape,
    /// different subject. A threshold policy touches no canonical scope key — it is
    /// tenant-wide and prices nothing — so `pricing_approval_key` cannot carry this
    /// rule and the register is the approval store's own partial index instead.
    ///
    /// `infra::approval::open_policy_unit` also *checks* for a pending proposal
    /// inside its transaction and produces the ordinary 409 — the one that can name
    /// the unit holding the proposal open, which an index violation cannot. This
    /// variant is the other half: the loser of two concurrent proposals, both of
    /// which read a store with no pending policy unit before either wrote. Without
    /// the index both commit, both mint version *n* off the same `latest_version`,
    /// and the tenant is left with one version number holding the union of two
    /// disjoint currency sets — a row set no approver saw, on a table that then
    /// refuses `UPDATE` and `DELETE`.
    ///
    /// It carries the tenant and **not** the holding unit, for
    /// [`RepoError::PendingKeyHeld`]'s reason: at the moment the index fires the
    /// winning transaction may not have committed, so there is no unit this
    /// transaction is entitled to read, and naming one a rollback might unmake would
    /// be worse than naming none.
    #[error(
        "pricing repo: tenant {tenant_id} already holds a submitted approval-threshold policy \
         proposal; decide it, or withdraw it to propose another"
    )]
    PendingPolicyUnitHeld {
        /// The tenant the mint guard refused a second open proposal for.
        tenant_id: String,
    },
    /// A window interval intersects one already on the same canonical scope key
    /// (`07-pricewindow-linkage.md` §6, `WINDOW_OVERLAP`).
    ///
    /// **The only guarantee of this store that no constraint carries**, and the
    /// only one this crate could not have written as one: the canonical scope key
    /// is ten columns of `pricing_price` and none of them is on the window row,
    /// so no unique index reaches it, a partial-index predicate sees only its own
    /// row, and an exclusion constraint would need `btree_gist` on Postgres and
    /// has no `SQLite` expression at all. §6 says "enforced inside every
    /// mutation" for that reason, and this refusal is what that enforcement
    /// answers with.
    ///
    /// It names the key, the interval that was asked for and the window it
    /// collided with — three things because each answers a different question an
    /// operator has, and the collided window's id is the one they can act on. The
    /// two intervals are **rendered** rather than carried as four instants, for a
    /// mechanical reason worth stating: six `String`s put this variant past
    /// `clippy::result_large_err`'s 128-byte threshold, which every `Result` in
    /// this crate's storage layer would then pay for.
    #[error("pricing repo: the interval {requested} on scope key {key} overlaps {conflicting}")]
    WindowOverlap {
        /// The canonical scope key both intervals are filed under.
        key: String,
        /// The interval the caller asked for, `[from, to)`.
        requested: String,
        /// The window already there and the interval it holds.
        conflicting: String,
    },
    /// A mutation was asked of a window whose state does not admit it
    /// (`inst-ws-immutable`, `inst-ws-cancel`, and §4's three edges).
    ///
    /// **It mints no wire code, deliberately**, and its ground is
    /// narrower than it was. §5 names three refusals that live near here —
    /// `WINDOW_HISTORICAL_IMMUTABLE`, `WINDOW_NOT_CANCELLABLE` and, one plane
    /// over, `WINDOW_TRAILING_VOID`. The first of them **has moved out** to
    /// [`RepoError::WindowHistorical`], because the store enforces it physically
    /// (`trg_pricing_price_window_append_only`'s history and future-`effective_to`
    /// arms) and a rule the store already holds is a rule the store may answer.
    /// The other two are about a *request an operator made*, which a surface
    /// judges before a repository is reached.
    ///
    /// What is left here is the residue that was always the honest content of this
    /// variant: an **unsanctioned edge**. A background sweep whose ordering
    /// assumption has broken — asked to expire a window somebody cancelled between
    /// the scan and the flip — or a caller that went round the surface. That is
    /// [`RepoError::FrontierRegression`]'s shape, an internal ordering fault on a
    /// path no client can provoke, so it renders as the refused lifecycle edge it
    /// is and no code is invented for it.
    ///
    /// The state is carried because "the window was cancelled" and "the window
    /// already expired" are different things for an operator reading a job's log.
    #[error("pricing repo: window {window_id} is {state}; {attempted} is not permitted")]
    WindowStateForbidden {
        /// The window the mutation addressed.
        window_id: String,
        /// The state it is actually in.
        state: String,
        /// What was attempted, as a phrase — `the transition to expired`.
        attempted: String,
    },
    /// The outbox's deduplication index did its job: this exact act has already
    /// been announced.
    ///
    /// Split out of [`RepoError::ConcurrentMutation`] because the two unique
    /// indexes on `pricing_outbox` mean opposite things to a caller. A
    /// `uq_pricing_outbox_sequence` collision is a race another writer won and a
    /// retry resolves; a `uq_pricing_outbox_dedup_key` collision is deterministic —
    /// the retry renders the same key from the same inputs and collides
    /// identically — so the two cannot share a class whose whole remedy is "retry".
    /// [`DomainError::OutboxEventAlreadyEnqueued`] is where it lands and carries
    /// the argument for the status.
    #[error("pricing repo: outbox event {dedup_key} is already enqueued")]
    OutboxEventAlreadyEnqueued {
        /// The key that collided, which names the act.
        dedup_key: String,
    },
    /// The migration state machine's own refused edge, and the sibling of
    /// [`RepoError::WindowStateForbidden`] for the same reason it exists: a
    /// zero-row compare-and-swap is contention only while the row still stands
    /// where the statement expected it. Once it has moved, the predicate can never
    /// match again, and answering `ConcurrentMutation` tells a caller to retry a
    /// statement that is permanently refused — `complete` on a cancelled run,
    /// `cancel` on a completed one.
    ///
    /// The state is carried for the reason the window variant carries it: "the run
    /// was cancelled" and "the run already completed" are different facts to an
    /// operator reading a log.
    #[error("pricing repo: migration {migration_id} is {state}; {attempted} is not permitted")]
    MigrationStateForbidden {
        /// The migration the mutation addressed.
        migration_id: String,
        /// The state it is actually in.
        state: String,
        /// What was attempted, as a phrase — `the move to completed`.
        attempted: String,
    },
    /// A mutation reached for a window fact §4 froze (`inst-ws-immutable`,
    /// `WINDOW_HISTORICAL_IMMUTABLE`).
    ///
    /// **This one the store may answer, and the criterion is that it also enforces
    /// it physically.** Two arms of
    /// `trg_pricing_price_window_append_only` are this rule — the immutable-history
    /// arm, which refuses any `UPDATE` of an `expired`/`cancelled` row, and the
    /// fifth arm, which refuses an `effective_to` moved into or out of the past —
    /// so a repository that did not pre-check handed the caller a **trigger**, i.e.
    /// a 500, for a request whose whole remedy is a later instant. The pre-check
    /// exists to make that a 409 carrying the code §5 declares.
    ///
    /// It is deliberately **not** [`RepoError::WindowStateForbidden`], on either of
    /// its two grounds. That variant renders
    /// `LIFECYCLE_FORBIDDEN` (400, "no alternative action"), and this refusal
    /// describes a very specific alternative on both grounds: schedule a new
    /// window, or shorten to an instant that is still ahead.
    ///
    /// The `frozen` phrase names *what* is history rather than only that something
    /// is, because "the window is expired" and "the end you are moving passed nine
    /// minutes ago" send an operator to different places.
    #[error("pricing repo: window {window_id} may not be mutated: {frozen}")]
    WindowHistorical {
        /// The window the mutation addressed.
        window_id: String,
        /// What about it is frozen, as a phrase.
        frozen: String,
    },
    /// A window interval whose end is not strictly after its start.
    ///
    /// The application half of `chk_pricing_price_window_interval`, and it exists
    /// for [`RepoError::ValueOutOfRange`]'s reason: a value arriving *on a request*
    /// that a column refuses is a caller mistake the request can be reshaped
    /// around, and reaching the CHECK made it an internal fault. `effective_to =
    /// effective_from` is not a zero-length window — nothing is effective over it
    /// and no consumer can resolve a price at any instant of it.
    ///
    /// **It carries no window code, and that is a reported divergence rather than
    /// an omission**: §5 declares eight window codes and none of them covers an
    /// empty interval. It renders `INVALID_REQUEST`, which is what the ladder
    /// already answers for a value the gear cannot interpret.
    #[error(
        "pricing repo: the window interval {requested} is empty; effective_to must be \
         strictly after effective_from"
    )]
    WindowIntervalEmpty {
        /// The interval as the caller asked for it, `[from, to)`.
        requested: String,
    },
    /// A membership interval intersects one **of the same group** already on
    /// the payer (`design/09-price-overlays.md` §5, `MEMBERSHIP_OVERLAP`).
    ///
    /// D-09's non-overlap rule is per `(tenant, payer)` **across every group**
    /// — [`RepoError::MembershipConflict`] is that rule's own refusal — and this
    /// narrower variant exists only because §5 declares the same-group
    /// collision its own code, distinct from the cross-group one. Both are
    /// raised by [`group_membership_repo::refuse_overlap`](crate::infra::storage::repo::group_membership_repo::refuse_overlap)
    /// off the **same** read, which is what makes the choice between them a
    /// comparison of `group_value` rather than a second query.
    ///
    /// It names the payer, the interval that was asked for and the membership
    /// it collided with, [`RepoError::WindowOverlap`]'s shape and reason.
    #[error(
        "pricing repo: the interval {requested} for payer {payer_tenant_id} overlaps {conflicting}, \
         in the same group"
    )]
    MembershipOverlap {
        /// The payer both intervals are filed under.
        payer_tenant_id: String,
        /// The interval the caller asked for, `[from, to)`.
        requested: String,
        /// The membership already there and the interval it holds.
        conflicting: String,
    },
    /// A membership interval intersects one **of a different group** already on
    /// the payer (`design/09-price-overlays.md` §5, `MEMBERSHIP_CONFLICT`,
    /// D-09).
    ///
    /// **This is the case D-09 is actually about**: membership intervals are
    /// non-overlapping per payer *across all groups*, not merely within one —
    /// this migration's own doc calls out the narrower same-group reading
    /// (`MEMBERSHIP_OVERLAP`) as the false negative D-09 refuses and this
    /// variant does not admit. The remedy named in §5 is the atomic **move**
    /// operation, not a plain enrollment.
    #[error(
        "pricing repo: the interval {requested} for payer {payer_tenant_id} overlaps {conflicting}, \
         in another group; use the move operation"
    )]
    MembershipConflict {
        /// The payer both intervals are filed under.
        payer_tenant_id: String,
        /// The interval the caller asked for, `[from, to)`.
        requested: String,
        /// The membership already there and the interval it holds.
        conflicting: String,
    },
    /// A membership interval whose end is not strictly after its start.
    ///
    /// [`RepoError::WindowIntervalEmpty`]'s reason, one plane over: a value
    /// arriving on a request that no column will hold is a caller mistake the
    /// request can be reshaped around, not an internal fault. §5 declares no
    /// membership code for an empty interval either, so this folds into the
    /// same `INVALID_REQUEST` rendering rather than minting one.
    #[error(
        "pricing repo: the membership interval {requested} is empty; effective_to must be \
         strictly after effective_from"
    )]
    MembershipIntervalEmpty {
        /// The interval as the caller asked for it, `[from, to)`.
        requested: String,
    },
    /// A membership whose stored end has already passed, asked to end again.
    ///
    /// [`RepoError::WindowHistorical`]'s ground one plane over — `window_repo`'s
    /// `refuse_frozen_end` refuses a move of an end `inst-ws-immutable` froze —
    /// and the harm is the same shape: the payer *was* in that group over the
    /// elapsed interval and may already have been charged for it, so re-ending it
    /// rewrites what a bill was computed from.
    ///
    /// It folds into `INVALID_REQUEST` rather than minting a code, for
    /// [`RepoError::MembershipIntervalEmpty`]'s reason: §5 declares
    /// `MEMBERSHIP_OVERLAP` and `MEMBERSHIP_CONFLICT` and nothing for this, and a
    /// code minted here would be a second, narrower vocabulary the design set does
    /// not carry. What it costs is that a caller cannot branch on it separately
    /// from a malformed interval; the sentence names the frozen ground so an
    /// operator reading the refusal can.
    #[error("pricing repo: the end of membership {membership_id} is frozen: {frozen}")]
    MembershipHistorical {
        /// The membership whose end was asked to move.
        membership_id: String,
        /// Why it cannot, in the column's own spelling.
        frozen: String,
    },
}

/// A per-aggregate serialization point's refusal, told apart from a storage
/// fault (D-159).
///
/// **Keyed on the platform's own predicate, never on a substring written here.**
/// `ScopeError::is_unique_violation` folds Postgres SQLSTATE `23505` and `SQLite`
/// extended code 2067 into one answer, via `sea_orm`'s `DbErr::sql_err()`. It
/// carries a message-matching **fallback** of its own, for proxies that strip the
/// SQLSTATE — that fallback is the toolkit's and is shared with every gear, which
/// is the difference between a shared predicate and a substring this crate
/// invented.
///
/// # Where it is called, stated as a rule rather than as a list
///
/// **Every unique index whose loser's remedy is to re-read and retry.** That
/// includes the three per-aggregate serialization points D-159 names — the audit
/// chain head, the outbox's per-aggregate sequence, the current-revision partial
/// `UNIQUE` — and it includes caller-minted primary keys, taxonomy values, policy
/// objects, migration schedules, threshold versions, bulk operations and
/// memberships. `grep -rn 'contention_or_db(' src` is the roster; there is no
/// transcription of it here, deliberately.
///
/// **No list of the call sites**, because the count moves faster than any prose
/// about it: a roster of five here stood while there were sixteen in ten modules,
/// and there are nineteen in eleven now. A reader consulting such a list to answer
/// *"does a duplicate taxonomy value, a second policy object, a racing enrollment
/// or a duplicate migration schedule come back as a 409 or a 500?"* is told 500 by
/// omission, and the answer is 409 at all four. `migrations.rs` records this crate
/// deleting a per-slice roster for the same reason, and `plan_repo.rs` records a
/// five that was seven.
///
/// # What it is **not** called on, which is the half that carries meaning
///
/// It is not called on every unique violation in this crate. Two are answered by
/// the check that owns them and must stay that way:
///
/// - **The canonical scope key.** `price_repo` answers a collision with
///   [`RepoError::DuplicateScopeKey`], which names the key and its occupant —
///   §5's `DUPLICATE_SCOPE_KEY`, a caller-fixable authoring fault, not a race.
/// - **The idempotency claim.** `idempotency_repo` has its own compare-and-swap
///   and a lost claim is a *replay*, whose remedy is to return the first
///   caller's answer rather than to retry.
///
/// `the_two_deliberate_exclusions_still_hold` derives that from the source rather
/// than restating it, so the exclusion cannot decay into an omission.
///
/// **The two primary-key sites are a divergence from D-159's own semantics and are
/// reported rather than quietly counted in.** D-159's code says *another mutation of
/// this aggregate committed first, and a retry is expected to succeed*. On a
/// duplicate caller-minted id the second half is false: the realistic reader of that
/// refusal is a client resubmitting the id it already used, and replaying the
/// request as sent collides identically, forever. A genuine race — two callers
/// minting one uuid — is what the sites were written for and is not what they will
/// mostly see. Naming the right refusal needs a decision and probably a code the
/// design set does not have, so this sentence is the report and the behaviour is
/// left as it stands.
///
/// **What it does not distinguish, stated because it is the residue.** The class
/// says *a* unique index refused and not *which*: `pricing_outbox` carries two,
/// its per-aggregate sequence and `uq_pricing_outbox_dedup_key`, and the driver
/// exposes the constraint identity only inside a message. Both mean "another
/// write of this aggregate got here first" and both remedy the same way, which is
/// why one answer serves — but a Postgres suite is what would assert the
/// constraint **names**, and until there is one that half rests on inspection.
///
/// `aggregate` is what the caller retries against, and it is named in the wire
/// message because D-159 requires it: a 409 that does not say which aggregate
/// contended tells a bulk run nothing about which row to re-drive.
#[must_use]
pub fn contention_or_db(
    err: &toolkit_db::secure::ScopeError,
    aggregate: &str,
    context: &str,
) -> RepoError {
    if err.is_unique_violation() {
        return RepoError::ConcurrentMutation {
            aggregate: aggregate.to_owned(),
        };
    }
    RepoError::Db(format!("{context}: {err}"))
}

/// The mint guard's refusal, or [`contention_or_db`]'s ordinary answer (D-192
/// clause (2)).
///
/// `pricing_approval` carries **two** unique indexes since
/// `uq_pricing_approval_policy_pending`: its primary key, `approval_id`, which the caller mints; and
/// `uq_pricing_approval_policy_pending`, "one open policy proposal per tenant". They
/// want different answers, and this is the one place that can tell them apart:
///
/// * a duplicate `approval_id` is [`RepoError::ConcurrentMutation`] — 409
///   `CONCURRENT_MUTATION`, *retry*. It is one of the two caller-minted primary keys
///   [`contention_or_db`]'s doc reports as a divergence from D-159, and
///   `rest_threshold_policy::a_policy_unit_whose_id_is_taken_is_a_retriable_conflict_and_not_a_storage_failure`
///   pins it;
/// * a second open proposal is [`RepoError::PendingPolicyUnitHeld`] — 409
///   `PENDING_CHANGE_UNIT_EXISTS`, *decide or withdraw the one you have*. Told to
///   retry instead, the caller would retry, be refused by
///   `open_policy_unit`'s own check, and reach the right answer one round trip late
///   under a code that sent them somewhere else first.
///
/// # It reads the driver's message, which this crate otherwise refuses to do
///
/// Stated plainly because [`contention_or_db`]'s doc argues the other way — that a
/// shared predicate beats "a substring this crate invented" — and the residue it
/// names is exactly this: *"the class says a unique index refused and not **which**
/// … the driver exposes the constraint identity only inside a message"*. Three
/// things make this the narrow case rather than a precedent:
///
/// * **the literals are DDL this chain owns**, not vendor phrasing. `sea_orm` has no
///   structured constraint identity — `SqlErr::UniqueConstraintViolation` carries a
///   `String` — so the alternatives were an `ON CONFLICT DO NOTHING` on the primary
///   key of a table whose whole discipline is that nothing is silently swallowed, or
///   a read-then-write pre-check for the *other* index, which is the shape D-192 is
///   about removing;
/// * **the fallback is today's behaviour.** An unrecognised message is
///   [`contention_or_db`], so a driver or proxy that renders violations differently
///   degrades to "retry" — safe, because a retry then meets the in-transaction check
///   — and never to a 500;
/// * **it is asserted on the mirror**, by
///   `tests/sqlite_approval_repo.rs::a_second_open_policy_proposal_is_refused_by_the_mint_guard_and_not_as_contention`
///   and its primary-key twin, so a rename of the index that forgot this function
///   reddens. The Postgres half of the predicate rests on the index name reaching the
///   message, which is that server's documented rendering and what
///   `postgres_migrations.rs`'s partial-index census keeps true of the name.
///
/// **The residue runs the other way and is worth naming.** An unrecognised message
/// degrades safely, but a *recognised* one can be wrong: a future second unique index on
/// `pricing_approval` whose key involves `tenant_id` would render the same substring on
/// the `SQLite` mirror and be reported as this guard — a specific wrong answer rather
/// than a generic retry, which is the one direction the fallback does not cover. What
/// keeps it visible is the index census in `tests/sqlite_migrations.rs` and
/// `tests/postgres_migrations.rs`: a second index on this table cannot be added without
/// appearing there, so the reader adding it meets this function's name in the same diff.
/// If one is ever added, narrow the `SQLite` arm to the index name the way the Postgres
/// arm already is.
///
/// `tenant_id` is what the refusal names; the *unit* it cannot name, and
/// [`RepoError::PendingPolicyUnitHeld`] says why.
#[must_use]
pub fn policy_guard_or_contention(
    err: &toolkit_db::secure::ScopeError,
    tenant_id: uuid::Uuid,
    aggregate: &str,
    context: &str,
) -> RepoError {
    if err.is_unique_violation() && names_the_policy_guard(&err.to_string()) {
        return RepoError::PendingPolicyUnitHeld {
            tenant_id: tenant_id.to_string(),
        };
    }
    contention_or_db(err, aggregate, context)
}

/// Does this driver message name `uq_pricing_approval_policy_pending` rather than
/// `pricing_approval`'s primary key?
///
/// Two renderings, one per backend, both of them names `uq_pricing_approval_policy_pending` writes:
///
/// * **Postgres** names the index — `duplicate key value violates unique constraint
///   "uq_pricing_approval_policy_pending"`;
/// * **`SQLite`** names the indexed **columns** rather than the index, so the guard
///   reads `UNIQUE constraint failed: pricing_approval.tenant_id` where the primary
///   key reads `… pricing_approval.approval_id`. `tenant_id` is unique on this table
///   under no other index, which is what makes the column a sufficient
///   discriminator; the qualified form is matched so that another table's
///   `tenant_id` cannot be mistaken for it.
///
/// Split out and taking a `&str` so both renderings are asserted directly, without
/// staging a database race for either.
fn names_the_policy_guard(message: &str) -> bool {
    message.contains("uq_pricing_approval_policy_pending")
        || message.contains("pricing_approval.tenant_id")
}

/// The one emission an invariant breach gets, and the reason it is not the
/// storage-failure line.
///
/// A dead connection is an operations fault that clears itself; a corrupt row is
/// something that reached the table outside this gear and stands until someone
/// looks at it. Folded into one line reading "storage failure" the second was
/// indistinguishable from the first, and the storage layer is the only place that
/// ever sees which row it was.
fn corrupt_row_met(err: &RepoError) {
    tracing::error!(
        error = %err,
        "bss-pricing: a stored row breaks an invariant this gear relies on; it was written \
         outside this gear, or by a version whose rules no longer hold"
    );
}

/// Map a storage failure into the gear's rejection vocabulary.
///
/// A frontier regression is a **precondition** failure, not an internal fault:
/// the row is intact and the store is healthy, the requested transition is
/// simply one the watermark's monotonicity forbids — the same shape as any other
/// refused lifecycle edge, so it lands on `LIFECYCLE_FORBIDDEN`. The draft-only
/// refusals join it there: a frozen revision is state forbidding an operation,
/// not a malformed request. Those two are **all** that variant keeps.
///
/// **The collapse D-146 exists to undo.** Four refusals used to land on
/// `LIFECYCLE_FORBIDDEN`, and two of them ask an operator for different things:
/// [`RepoError::NoSuccessorRevision`] is a **stop** — a retired plan can never
/// publish again, so no successor of it is publishable — while
/// [`RepoError::OpenDraftExists`] is a **different and available action**, go
/// and edit the draft you already have. A consumer that had to read the sentence
/// to tell those apart could act on neither, so each now carries its own code
/// (`PLAN_RETIRED_NO_SUCCESSOR`, `OPEN_DRAFT_REVISION_EXISTS`). The second is
/// also not a state-machine transition at all: it is a uniqueness conflict on
/// the plan's one draft slot, which is why it leaves the precondition class for
/// the conflict one and becomes retriable.
///
/// The line is held on the other side too. The read model's forward-only pin
/// frontier is advanced by the projector and is unreachable by a caller, so a
/// regression there stays where it is: a wire code for it would document an API
/// no client can provoke.
///
/// [`RepoError::ValueOutOfRange`] is the one arm left on
/// [`DomainError::InvalidRequest`], and it is the reason that arm exists rather
/// than a second flavour of [`RepoError::CorruptRow`]: an authored quantity the
/// column cannot hold arrived on a request and the caller can change it, which
/// is not true of anything on the internal arm. It mints no wire code — it
/// carries the sentence, and the Foundation's bad-request category carries the
/// classification. [`RepoError::GrandfatherHorizonOffClass`] stood beside it
/// only for want of a code and now has one (D-147), which puts it with the
/// publish refusals it is a sibling of rather than with malformed input.
///
/// [`RepoError::StaleRowVersion`] is rendered into
/// [`DomainError::StaleVersion`] ending in the **same
/// `current {c}, submitted {s}` clause** `domain::concurrency::require_match`
/// produces, so the numbers a runbook greps for read identically whether the
/// submit was refused by the transport's pre-check or by the store's
/// compare-and-swap. The store's rendering carries one thing the pre-check
/// cannot — a `{subject} {id}:` prefix naming the row — so the two are not
/// byte-identical and are not claimed to be; what they must not do is spell the
/// same collision two different ways, which is how a runbook ends up believing
/// there are two failures. `storage_tests.rs` pins the shared clause.
///
/// Deliberately a named function rather than a `From` impl, and it **logs the
/// storage failure before flattening it**. `DomainError` is a `Clone + Eq` value
/// type carried into responses and compared in tests, so it cannot hold a boxed
/// source; the chain would be lost either way, and the log is where an operator
/// gets it back. The sibling ledger converts at its call sites for the same
/// reason.
#[must_use]
pub fn repo_failure(err: &RepoError) -> DomainError {
    match err {
        RepoError::Db(_) => {
            tracing::error!(error = %err, "bss-pricing: storage failure");
            DomainError::Internal(err.to_string())
        }
        // Told apart from a storage failure because they need different people;
        // [`corrupt_row_met`] carries the argument.
        RepoError::CorruptRow(_) => {
            corrupt_row_met(err);
            DomainError::Internal(err.to_string())
        }
        // Every refused state-machine edge, in one arm — `WindowStateForbidden`
        // included, though it is declared among the window variants and reads as
        // theirs. `clippy::match_same_arms` cannot see the duplication when one body
        // is a block and the other an expression, so the grouping is the only thing
        // holding them together. None of them mints a code of its own.
        RepoError::FrontierRegression { .. }
        | RepoError::NotDraft { .. }
        | RepoError::NotSupersedable { .. }
        | RepoError::WindowStateForbidden { .. }
        | RepoError::MigrationStateForbidden { .. } => {
            DomainError::LifecycleForbidden(err.to_string())
        }
        RepoError::OutboxEventAlreadyEnqueued { .. } => {
            DomainError::OutboxEventAlreadyEnqueued(err.to_string())
        }
        RepoError::NoSuccessorRevision { .. } => {
            DomainError::PlanRetiredNoSuccessor(err.to_string())
        }
        RepoError::OpenDraftExists { .. } => DomainError::OpenDraftRevisionExists(err.to_string()),
        RepoError::NotFound { subject, id } => DomainError::NotFound {
            subject: subject.clone(),
            id: id.clone(),
        },
        RepoError::StaleRowVersion {
            subject,
            id,
            current,
            submitted,
        } => DomainError::StaleVersion(format!(
            "{subject} {id}: current {current}, submitted {submitted}"
        )),
        RepoError::DuplicateScopeKey(key) => DomainError::DuplicateScopeKey(key.clone()),
        // D-340's `PHASE_ID_IN_USE`, in the conflict class beside the line above
        // for the reason the variant's own doc gives: the sentence is *this key is
        // already held*. The whole `Display` is carried rather than the bare id —
        // `DuplicateScopeKey` above passes the key alone because the key's
        // rendering *is* the sentence there, while here the id needs the clause
        // saying what holds it, which is the half the old `500` withheld.
        RepoError::PhaseIdInUse(_) => DomainError::PhaseIdInUse(err.to_string()),
        // A conflicting state the caller can act on, and not a scope-key
        // collision: `DUPLICATE_SCOPE_KEY` names a price row's canonical key and
        // reporting a bundle under it would tell an operator to go and look at a
        // key that has nothing to do with the refusal. `Conflict` is the class
        // this platform renders 409 from, which is what "the plan already has
        // one" is.
        RepoError::DuplicateBundleOnPlan { .. } => DomainError::BundleExistsOnPlan(err.to_string()),
        // §5 types `PRECEDENCE_DUPLICATE` **409 outright**, unlike the seven
        // overlay codes it types as architectural 422s, and this is that code's
        // constraint half. It joins the conflict class with
        // `PENDING_CHANGE_UNIT_EXISTS`: the caller's request was well formed and
        // what refused it is a sibling overlay their own request never named, so
        // re-reading the class's overlays is the remedy — which is what a 409
        // asks for.
        RepoError::OverlayPrecedenceHeld => DomainError::PrecedenceDuplicate(err.to_string()),
        // The publish rule's own code, for the reason the variant's doc gives:
        // one fault, three doors. `RoundingPolicyUnresolved` is an architectural
        // 422 rendered 400 like every other fail-closed publish rejection, so the
        // caller of a supersession sees exactly what the caller of a publish sees.
        RepoError::RoundingPolicyUnresolved { .. } => {
            DomainError::RoundingPolicyUnresolved(err.to_string())
        }
        // The rule's own code rather than a new one, for the reason the variant's
        // doc gives: one fault, four doors. `TAX_BASIS_INCOMPLETE` is a publish
        // rule code and is reported the way publish rules are — a
        // `ValidationReport` naming the row — so the caller of a supersession, of a
        // cutover and of a bare `publish_rows` all read what the caller of an
        // ordinary publish reads. `infra::supersession::refuse_unresolved_tax_category`
        // builds the identical report on the door that asks ahead of its registry
        // request.
        RepoError::TaxCategoryUnresolved { price_id } => {
            let mut report = crate::domain::validation::ValidationReport::default();
            report.violate(
                crate::domain::tax_display::TAX_BASIS_INCOMPLETE,
                price_id.clone(),
                err.to_string(),
            );
            DomainError::ValidationFailed(report)
        }
        // A uniqueness conflict on the overlay's one editable slot, and the
        // `OPEN_DRAFT_REVISION_EXISTS` class for `OpenDraftExists`' reason: the
        // operator's next action is a real one — go and edit that revision.
        RepoError::OverlayOpenDraftExists { .. } => {
            DomainError::OpenDraftRevisionExists(err.to_string())
        }
        RepoError::IdempotencyPayloadMismatch { .. } => {
            DomainError::IdempotencyPayloadMismatch(err.to_string())
        }
        RepoError::IdempotencyKeyInFlight { .. } => {
            DomainError::IdempotencyKeyInFlight(err.to_string())
        }
        // `inst-bk-lock` and §5 together: an edit hitting a row under the bulk
        // lock "surfaces as the Foundation's concurrent-edit conflict **naming
        // the bulk operation**". So it joins `ConcurrentMutation` rather than
        // minting a code — the operator's situation is the one that variant
        // already describes, and the run's id is in the sentence.
        RepoError::BulkRowLocked { .. }
        | RepoError::BulkLocksHeld { .. }
        | RepoError::ConcurrentMutation { .. } => {
            // At `warn`, not `error`: a lost race is the store working, and the
            // caller is told to retry. What it is not is invisible — the rate of it
            // is the only evidence of a contended aggregate, and nothing else in the
            // gear counts one.
            //
            // `BulkLocksHeld` joins them: rows held by an operation the caller can
            // name and abort, which is retryable and has a door, not an `Internal`.
            tracing::warn!(error = %err, "bss-pricing: a writer lost a race and is told to retry");
            DomainError::ConcurrentMutation(err.to_string())
        }
        // Deliberately **not** in the arm above, though both refuse a write over a
        // neighbouring aggregate's row: a bulk lock is released when its run ends and
        // `CONCURRENT_MUTATION` tells the caller to retry, where this one is released
        // by nothing at all — `pricing_price_window` refuses `DELETE` on both engines
        // and the foreign key neither cascades nor defers, so the row stays bound for
        // the rest of time. **The operator is not stuck, and the message says which
        // door is open:** `plan_repo::abandon_draft` clears phases, addon rules, the
        // descriptor set, composites, floor caps and the bundle composition and moves
        // the revision to `abandoned` **without touching price rows** — so the row
        // survives, under a revision nothing publishes from. Naming only what fails
        // would leave a caller hunting for a remedy the schema does not have.
        RepoError::PriceWindowScheduled { .. } => {
            DomainError::PriceWindowScheduled(err.to_string())
        }
        // Two shapes of one answer: a value arriving on a request that no column
        // will hold, which the author can reshape. `WindowIntervalEmpty` is the
        // window plane's — section 5 declares no window code for an empty
        // interval — and the arms are merged because inventing a difference
        // between them would be inventing a distinction no consumer can act on.
        RepoError::ValueOutOfRange { .. }
        | RepoError::WindowIntervalEmpty { .. }
        | RepoError::MembershipIntervalEmpty { .. }
        | RepoError::MembershipHistorical { .. } => DomainError::InvalidRequest(err.to_string()),
        RepoError::GrandfatherHorizonOffClass { .. } => {
            DomainError::GrandfatherUntilForbidden(err.to_string())
        }
        // The axis rule's own answer: `USAGE_LINE_AXIS_MISMATCH` is what §4.1
        // declares for a usage pair that does not belong where it was put, and
        // this is that fact reached through the other half of the request.
        RepoError::UsageLineDisagrees { .. } => DomainError::UsageLineAxisMismatch(err.to_string()),
        RepoError::TimestampPrecisionExceeded { .. } => {
            DomainError::TimestampPrecisionExceeded(err.to_string())
        }
        RepoError::ApprovalNotPending { .. } => DomainError::ApprovalNotPending(err.to_string()),
        // The register's constraint and the register's check answer the **same code
        // and the same remedy**, which is what a caller branches on and acts on: the
        // condition is one condition, and which of two transactions committed first
        // must not change what the caller is told to do about it.
        //
        // **The claim is narrowed to that, deliberately.** A caller "must not read two
        // different refusals" is false of the *message*: the constraint path cannot name
        // the holding unit — at the moment the index fires, the transaction that
        // inserted the winning row may not have committed — so one body names a unit and
        // the other does not. Both name the key and both end in the same instruction;
        // `storage_tests` asserts that rather than this comment claiming it.
        RepoError::PendingKeyHeld { .. } => DomainError::PendingChangeUnitExists(err.to_string()),
        // The mint guard's loser, on the same code and for the same reason one arm up:
        // "another open proposal already holds this" is one condition, and which of two
        // transactions committed first must not change what the caller is told to do
        // about it. `PendingPolicyUnitHeld`'s own doc says what it cannot carry.
        RepoError::PendingPolicyUnitHeld { .. } => {
            DomainError::PendingChangeUnitExists(err.to_string())
        }
        // Asserted end to end by
        // `rest_windows::an_overlapping_schedule_is_refused_by_its_code_and_writes_nothing`,
        // which was the seventh arm of this fold found with no observation of what it
        // maps to: the store's own suite asserted the `RepoError` and stopped, so
        // corrupting this line to `Internal` left the whole crate green while a
        // colliding interval answered 500.
        RepoError::WindowOverlap { .. } => DomainError::WindowOverlap(err.to_string()),
        // `WindowStateForbidden` belongs in the refused-edge arm above, not here; its
        // own doc says why the two §5 window refusals that remain a surface's are not
        // it. This is the one window refusal the store both enforces physically and
        // answers itself, so a caller who asks to move a window's end into the past
        // reads the 409 §5 declares instead of the trigger's 500.
        RepoError::WindowHistorical { .. } => {
            DomainError::WindowHistoricalImmutable(err.to_string())
        }
        // D-09's own two codes, `WindowOverlap`'s shape one plane over: the
        // request was well-formed and what refused it is a sibling membership on
        // the payer their own request never named.
        RepoError::MembershipOverlap { .. } => DomainError::MembershipOverlap(err.to_string()),
        RepoError::MembershipConflict { .. } => DomainError::MembershipConflict(err.to_string()),
    }
}

#[cfg(test)]
#[path = "storage_tests.rs"]
mod storage_tests;
