//! The mass-repricing run's apply lane — `inst-mr-apply`, `inst-mr-validate-scope`
//! (`design/12-operator-efficiency.md` §3; D-134).
//!
//! [`apply_run_in`] is the writer that takes a run the rest of the way, and it is
//! called: `RunApplyWorker::apply` is the gear's one production caller, off the
//! request that accepted the run. Both accepting surfaces — `api::rest::repricing_runs`'
//! `open_repricing_run` for the non-material path, `api::rest::approvals` for the run
//! a second principal has just approved — hand the apply to [`RunApplyLane`], and
//! each has taken the run to `committing` before it does: `open_run`'s own
//! `advance_on_verdict` for the first, [`begin_committing_in`] for the second.
//!
//! **A module header describing built work as owed is this gear's standing defect**,
//! and this file has carried it: "a run that reached `awaiting_approval` or
//! `committing` stops there today" cites another doc rather than the writer sitting
//! in this same file. The call sites are the measurement, and grepping them takes
//! fifteen seconds.
//!
//! [`abandon_committing_run`] is what **spends** what the gentle exits preserve: a
//! run they leave `committing` has a door out, and an operator is the one who opens
//! it. A redrive route — letting a second `POST` under a spent `run_id` drive a
//! `committing` run forward again — is still owed, and the redrive-contract
//! paragraph below is the argument for it rather than a claim about a caller.
//!
//! # The shape, settled and not re-opened here
//!
//! Three decisions cost real time to establish and are taken as given:
//!
//! * Successor rows are written through [`commit_supersession`], **not**
//!   [`crate::infra::supersession::supersede_in`] — the latter's per-call
//!   registry request, outbox enqueue, audit append and materiality
//!   evaluation are exactly the four things a repricing run does once for the
//!   whole run (materiality, already built) or once per plan (the other
//!   three), never once per row.
//! * The row-local rule set is [`plan_supersession`], run once per row,
//!   unchanged from the interactive supersession's own compose step.
//! * One `CatalogVersion` per plan, requested once after every row of that
//!   plan has committed — never per row.
//!
//! # The order inside one plan's transaction
//!
//! 1. For every pending row of the plan: resolve the predecessor **fresh, in
//!    this transaction**, run [`plan_supersession`] (the row-local rules plus
//!    the touched key's window overlap/gap/trailing-void check) at
//!    [`ChangeoverMoment::Commit`] — the stricter, batching-delay floor
//!    `api::rest::repricing_runs`' own module doc names as belonging to "a
//!    commit that does not exist", which this now is — stage the successor
//!    draft, and write it through [`commit_supersession`]. The journal row
//!    flips `pending -> applied` and one audit record lands on the plan's
//!    chain, in the same transaction as the write it describes (D-14).
//! 2. **Once**, over the plan's row set **as this transaction just left it**:
//!    [`crate::infra::publish::assemble_from`] against the plan's current
//!    revision (never `assemble`, which demands an open draft a repricing
//!    plan does not have), [`crate::infra::publish::rule_params`], then
//!    [`run_publish_rules`]. This is the step Step 0 exists for — it depends
//!    on the transaction seeing the writes step 1 just made, uncommitted.
//! 3. On a violation: `Err`, and every write above rolls back with it. The
//!    caller marks every one of this plan's rows `failed` with the one
//!    reason, in a **separate** transaction — this module doc's own
//!    consequence, decided rather than discovered mid-build: a rolled-back
//!    transaction cannot also be the one recording that it rolled back.
//! 4. On success: one `CatalogVersion` request for the plan, `record_pending`
//!    against [`SubjectRef::Plan`], and — now that the pending ref exists —
//!    the two outbox events per row step 1 could not enqueue yet, each keyed
//!    `(run_id, price_id)` so a redrive cannot double-fire either.
//!
//! # What the aggregate pass actually guards here
//!
//! Step 2's re-run of `run_publish_rules` cannot fail *because of this run's
//! own effect*. A markup/discount/fixed adjustment moves only a row's amount,
//! and every rule `run_publish_rules` registers that reads `amount_minor`,
//! `package_price_minor` or `unit_price_minor` is row-local — already run,
//! pre-commit, inside [`plan_supersession`]'s own `price_row_rules()` call.
//! None of the genuinely aggregate-only rules (phase coverage, descriptor
//! completeness, region declaration, window coverage) is sensitive to an
//! amount's value at all. So for this lane the post-commit pass is not a
//! soundness guard over what this transaction just wrote — it cannot catch a
//! bad *repricing*, because a repricing has no way to produce one. What it
//! guards against is **concurrency**: that nothing *else* changed the plan's
//! structure between this run's row selection and this transaction's commit —
//! a phase dropped, a region retired, a stray draft authored on an untouched
//! key. That is a real property worth its cost even though this run's own
//! adjustment can never be the thing that trips it.
//!
//! # The bulk lock is taken here, on the run's own rows — and here is exactly
//! # what that does and does not close
//!
//! `inst-bs-commit` gives the run's bulk lock over its rows starting at entry
//! to `committing`, and D-134 leans on it explicitly. [`apply_run_in`] takes
//! it now, over the price ids [`repricing_journal_repo::pending_for_run`]
//! answers — this call's whole target set — on the same autocommit connection
//! every other run-level statement here already uses, never inside a plan's
//! own transaction: [`bulk_repo::take_locks`]'s own doc is explicit that it
//! must run outside one, because Postgres aborts an enclosing transaction on
//! the insert its own conflict path issues, which would degrade
//! [`RepoError::BulkRowLocked`] to [`RepoError::BulkLocksHeld`] and lose the
//! holder `fr-concurrent-edit` needs named.
//!
//! Releasing it is split across two mechanisms, because [`apply_run_in`] does
//! not treat its four ways of ending alike:
//!
//! * **A clean finish, and an ordinary [`DomainError`] this function's own
//!   code is still running to handle** — the two paths its own `match` can
//!   still reach. A clean finish calls [`finish_run`]: release the lock,
//!   tally, land the run terminal. An ordinary `Err` from lock-taking onwards
//!   — [`bulk_repo::take_locks`]'s own refusal included, which is why that call
//!   sits inside the same `inner` block as everything after it — does
//!   deliberately *less*: it releases the lock and returns, leaving the run
//!   `committing` with its unreached rows exactly `pending` —
//!   [`domain::bulk::JournalState`]'s own doc is explicit that this is what
//!   lets a second call tell "never reached" from "decided", and nothing
//!   about a plain storage hiccup should cost that. Force-landing the run
//!   terminal here would freeze every unreached plan `failed` with no
//!   redrive possible; leaving it `committing` costs nothing, because
//!   `bulk_repo::take_locks` is called unconditionally at this function's own
//!   top for a run already `committing` exactly as for one just entering it
//!   — a second call simply retakes the lock and carries on.
//!
//!   **The abort is the second call, and the redrive is still owed.**
//!   `apply_run_in`'s one production caller is `RunApplyWorker::apply`, which is
//!   asked only for runs that are already `committing` — the two accepting surfaces
//!   spend that edge before they enqueue. So what the gentle exit preserves is
//!   spent by [`abandon_committing_run`] (`POST …/repricing-runs/{runId}/abort`):
//!   the lock goes, the unreached rows are decided, and the run lands terminal on
//!   an operator's own decision. That turns "`committing` forever with its
//!   unreached rows `pending` forever" into a state with a door out.
//!
//!   **`awaiting_approval` is the one state left with no door, and it is not on the
//!   apply's path.** The abort guards on `committing` and refuses everything else,
//!   so a run that is still `awaiting_approval` can only be ended by a `reject` of
//!   its batch unit — and a unit already decided is refused `APPROVAL_NOT_PENDING`.
//!   Two windows reach that state: a `reject` whose own `awaiting_approval ->
//!   rejected` write fails after the decision committed
//!   (`api::rest::approvals::reject_repricing_run`, D-267's strand through a crash
//!   rather than through a missing edge), and an `approve` whose
//!   [`begin_committing_in`] fails after the decision committed. Both leave a run
//!   whose unit is decided and whose rows are `pending`, and both need the same
//!   capability: driving a run's state machine from inside `ApprovalService::decide`'s
//!   own transaction. That is a decision about where a bulk run's states are written
//!   from, and it is what makes the strand the bulk plane's rather than either arm's.
//!   Nothing on the *enqueue* path adds to it — a refused enqueue, a lost applier and
//!   a shutdown mid-apply all leave `committing`, which the abort reaches.
//!
//!   The **redrive** — letting a `POST` under a spent `run_id` drive a
//!   `committing` run forward rather than end it — would recover the rows instead
//!   of failing them, and is not built. It needs no *new* protection against
//!   `inst-co-single-pending`: [`RunApplyWorker`] already reaches this function
//!   arbitrarily later than the open's own `refuse_rows_on_a_held_key`, so
//!   [`refuse_targets_on_a_held_key`] below is the check every arrival goes
//!   through and a redrive is one more arrival. The abort needs none of it — it
//!   takes nothing and writes no price.
//! * **A panic, and a dropped future** — the two paths no code of this
//!   function's own ever runs for again. Only [`Drop`] is the language's own
//!   guarantee across both (review findings Z8-8/Z9-5 name the gap an
//!   `Err`-arm-only release like `infra::bulk`'s own sibling leaves: a panic
//!   unwinds past a match arm exactly as it unwinds past everything else, and
//!   a dropped future — a client disconnect, a shutdown signal, a losing
//!   `select!` arm — never runs *any* of this function's own code again,
//!   match arm or not). [`RunLockGuard`]'s `Drop` releases the lock and stops
//!   there, exactly as the ordinary-`Err` exit does: with the abort door mounted
//!   there is a decision an operator can make about the run, so inferring one from
//!   a dropped future buys nothing and costs the rows their `pending` state. The
//!   release leaves the dropping thread on [`RunCompensation`]'s lane — a task the
//!   gear's lifecycle owns and cancels — rather than on a detached spawn nothing
//!   supervises. See [`RunLockGuard`]'s own doc for what it can and cannot
//!   promise.
//!
//! **And the lock does not close the concurrency hole even taken exactly as
//! D-134 describes it.** Two facts, both established by grep rather than by
//! argument, drive this:
//!
//! * **No aggregate rule is amount-sensitive.** Every rule `run_publish_rules`
//!   registers was checked against `amount_minor` / `package_price_minor` /
//!   `unit_price_rate`: zero references. Every amount-touching rule is
//!   row-local and already runs pre-commit in `plan_supersession`. A
//!   repricing run changes only amounts and rates, so it cannot make a clean
//!   plan's aggregate pass fail by its own effect — that pass is a
//!   concurrency guard, not a soundness check over this run's own writes (see
//!   the section above).
//! * **The lock is narrower than the pass.** `take_locks` covers the rows the
//!   run *selected*; the aggregate pass's candidate set is the *whole plan's*
//!   published-and-draft rows (`CANDIDATE_ROW_STATES`, `src/infra/publish.rs:117`).
//!   A stray draft on a key the run never targeted sits outside any lock
//!   scoped to "the run's rows" — `tests/sqlite_repricing_apply.rs`'s own
//!   atomicity test exploits exactly that key, and the lock this task builds
//!   would not have prevented it: its insert never names a row the run did
//!   not select.
//!
//! So: this lock serialises interactive edits **on the run's own rows**
//! against this run, and nothing wider. Whether to narrow the aggregate pass
//! to the run's own keys, widen the lock to the whole plan, or accept the
//! residual as a standing property of `inst-mr-validate-scope` is a design
//! decision nobody has taken, and this module does not take it here —
//! narrowing the pass or widening the lock in code would be answering a
//! question the design set has not.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use tokio_util::sync::CancellationToken;
use toolkit_db::secure::{AccessScope, DBRunner, DbTx};
use toolkit_db::{DBProvider, DbError};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::audit::{AuditAction, AuditStamp, AuditSubjectKind};
use crate::domain::bulk::{BulkState, JournalState};
use crate::domain::error::DomainError;
use crate::domain::lifecycle::LifecycleState;
use crate::domain::overlay::Adjustment;
use crate::domain::ports::CatalogVersionRegistryV1;
use crate::domain::price_record::PriceContent;
use crate::domain::price_row::ModelKind;
use crate::domain::publish::rules::run_publish_rules;
use crate::domain::read_model::SubjectRef;
use crate::domain::repricing::{adjusts_rate, project_row};
use crate::domain::scope_key::{PlanId, ScopeKey};
use crate::domain::supersession::{ChangeoverMoment, NamedWindow, plan_supersession};
use crate::domain::window::WindowInterval;
use crate::infra::publish::{assemble_from, rule_params};
use crate::infra::registry_deadline::request_version_now;
use crate::infra::storage::repo::outbox_repo::{
    NewOutboxEvent, PriceUpdatedPayload, PriceWindowTransitionPayload,
};
use crate::infra::storage::repo::repricing_journal_repo::JournalRow;
use crate::infra::storage::repo::{
    BulkOperationRecord, NewAuditEntry, NewPriceDraft, PendingVersionRow, PolicyObjectRepo,
    WindowMutationEvent, audit_repo, bulk_repo, catalog_version_ref_repo, outbox_repo, plan_repo,
    price_repo, repricing_journal_repo, window_repo,
};
use crate::infra::storage::repo_failure;
use crate::infra::supersession::{SupersessionCommit, commit_supersession, supersession_unit_ref};
use time::OffsetDateTime;

/// What one call to [`apply_run_in`] found once it finished — the run's whole
/// journal tally, not only what this particular call decided.
///
/// **Not "this call's delta".** [`apply_run_in`] is the re-drive as well as
/// the first attempt (`inst-mp-journal`), and a re-drive over a journal that
/// is already fully decided does no writes at all — so a delta-shaped
/// `RunOutcome` would answer `{0, 0}` for a run that plainly applied rows,
/// which is not what "the same outcome" means for an idempotent replay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunOutcome {
    /// How many of the run's journal rows now read `applied`.
    pub applied: usize,
    /// How many now read `failed`.
    pub failed: usize,
}

/// Re-ask `inst-co-single-pending` of the rows this apply is about to write.
///
/// It was asked once, at run open, in the API layer
/// (`api::rest::repricing_runs::refuse_rows_on_a_held_key`), and never again —
/// this module was the only one of the six mutating services with **no occurrence
/// of `refuse_held_key` at all**, where `window`, `supersession`, `cutover`,
/// `grandfather` and `approval` each re-ask at their own write.
///
/// **This is the check every arrival to the apply goes through, and it is the only
/// one there is.** [`RunApplyWorker`] applies a run off the future that accepted it,
/// so the open's own check — `api::rest::repricing_runs::refuse_rows_on_a_held_key`,
/// made once while the selector was being expanded — is separated from the write by
/// however long the lane is. A competing interactive unit can open over one of the
/// run's keys in that gap, and this is what refuses the run's write rather than
/// letting two pending decisions author one key.
///
/// The run's **own** unit is not what it catches. A material run registers every
/// selected key in `held_keys` while it pends, and its decision is durable before
/// the approve enqueues the apply, so this run holds nothing here — measured rather
/// than assumed: a first version of this check's test, written at the REST layer,
/// could not even seed the situation and failed with `PendingKeyHeld`. What it
/// catches is a *third party's* unit, and the redrive door, when it is built, is one
/// more arrival through the same check.
///
/// # Called above `inst-bs-commit`'s edge, and that is the point
///
/// The `awaiting_approval -> committing` edge below is single-spend, and this order
/// is what a redrive's arrival costs: refusing after the edge would spend it on a
/// call that then wrote nothing, so the run a held key turned away would have to be
/// aborted where it could instead be redriven again once the holder decides. Every
/// arrival off [`RunApplyLane`] is already `committing` — both accepting surfaces
/// spend the edge on the request — so for those the refusal leaves the state the
/// `202` handed back, which `abandon_committing_run` reaches.
///
/// # Errors
/// [`DomainError::PendingChangeUnitExists`] naming the holding unit and the key;
/// a storage failure reading either the keys or the register.
async fn refuse_targets_on_a_held_key(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    target_ids: &[Uuid],
) -> Result<(), DomainError> {
    let held = price_repo::load_scope_keys_for_ids(runner, scope, tenant_id, target_ids)
        .await
        .map_err(|e| repo_failure(&e))?
        .into_iter()
        .map(|(_, key)| key.to_string())
        .collect();
    crate::infra::approval::refuse_held_key(runner, scope, tenant_id, &held).await
}

/// Take `inst-bs-commit`'s `awaiting_approval -> committing` edge on the request that
/// approved the run, before its apply is handed to [`RunApplyLane`].
///
/// **The state a run is queued in is the state an operator has to spend.** The apply
/// runs on [`RunApplyWorker`], arbitrarily later than the approve, and every way it
/// can fail to arrive — a full lane, a replica whose applier is gone, a shutdown
/// between the two — leaves the run exactly where the accepting request put it.
/// [`abandon_committing_run`] (`POST …/repricing-runs/{runId}/abort`) acts on
/// `committing` and refuses anything else, so a run queued at `awaiting_approval` is
/// a run with no door: the abort answers `LIFECYCLE_FORBIDDEN`, nothing sweeps and no
/// redrive is built, and the run keeps its journal rows `pending` for good. Spending
/// the edge here is what makes `api::rest::approvals`' arm hand the lane the same
/// shape `api::rest::repricing_runs`' non-material path already does — a durable
/// `committing` run — and what makes every message about a refused enqueue true.
///
/// The alternative considered and rejected: leave the edge on the worker and have
/// each message branch on the run's real state. That names the trap without giving
/// the operator anything to spend, because there is no remedy for
/// `awaiting_approval` to name.
///
/// **Idempotent over a run already `committing`.** A replayed approve, or an approve
/// racing the open's own edge, finds the run past this point and this call is a
/// no-op — the same posture [`apply_run_in`]'s own `committing` arm has, and the
/// reason the caller may enqueue unconditionally once this returns `Ok`.
///
/// Single-spend is the store's, not this read's: [`bulk_repo::advance`] filters on
/// `from`, so two approvals landing at once cannot both move the run and the loser
/// gets `ConcurrentMutation` rather than a second queued apply.
///
/// # Errors
/// [`DomainError::NotFound`] when `operation_id` names no run in scope;
/// [`DomainError::LifecycleForbidden`] when the run stands somewhere neither
/// `awaiting_approval` nor `committing` — a terminal run included, which is a run
/// whose rows are all decided and whose apply there is nothing left to queue; a
/// storage failure reading the run or writing the edge.
pub async fn begin_committing_in(
    db: &DBProvider<DbError>,
    scope: &AccessScope,
    tenant_id: Uuid,
    operation_id: Uuid,
    at: OffsetDateTime,
) -> Result<(), DomainError> {
    let conn = db.conn().map_err(|e| {
        DomainError::Internal(format!("bss-pricing: repricing run commit edge: conn: {e}"))
    })?;
    let run = bulk_repo::read(&conn, scope, tenant_id, operation_id)
        .await
        .map_err(|e| repo_failure(&e))?
        .ok_or_else(|| DomainError::NotFound {
            subject: "repricing run".to_owned(),
            id: operation_id.to_string(),
        })?;
    if run.state == BulkState::Committing {
        return Ok(());
    }
    if run.state != BulkState::AwaitingApproval {
        return Err(DomainError::LifecycleForbidden(format!(
            "repricing run {operation_id} is {}; the approved arm takes the run from \
             awaiting_approval, and a run that is elsewhere has no apply left to queue",
            run.state.as_str()
        )));
    }
    // The **report travels unchanged**, `abandon_committing_run`'s rule and
    // `reject_repricing_run`'s: nothing about the run's frozen parameters moved by
    // being approved, and the report is what the unit's pin was taken over — so
    // rewriting it here would make the digest of the record disagree with the
    // approval that authorized it.
    bulk_repo::advance(
        &conn,
        scope,
        tenant_id,
        operation_id,
        BulkState::AwaitingApproval,
        BulkState::Committing,
        run.report,
        at,
    )
    .await
    .map_err(|e| repo_failure(&e))?;
    Ok(())
}

/// Apply a mass-repricing run — `inst-mr-apply`'s per-plan commit and
/// `inst-mr-validate-scope`'s aggregate pass, over whatever the run's journal
/// still holds `pending`.
///
/// # What this call may find the run standing at, and what it does about it
///
/// * **`awaiting_approval`**: **not a state the lane's arrivals stand in** — both
///   accepting surfaces take the run to `committing` before they enqueue
///   ([`begin_committing_in`] is the approve's half). This arm is the arrival shape
///   a redrive has, and the one a direct caller with no lane has: it performs
///   `inst-bs-commit`'s own edge, `awaiting_approval -> committing`, so a caller
///   driving a run forward from where a failed approve left it needs no copy of that
///   transition. Deleting it would make the redrive's first act a `DomainError` on
///   the one state a redrive exists to move.
/// * **`committing`**: proceeds directly to the per-plan work below — and keeps
///   checking, because this is also the state an operator's abort acts on:
///   [`apply_by_plan`] re-reads the run before every plan and stops if it has left
///   `committing`, and the arm this function's own tail takes then leaves the finish
///   to whoever ended the run rather than spending [`finish_run`] over it a second
///   time.
/// * **A terminal state**: every row is already decided (or the run never
///   selected one). No write happens; [`RunOutcome`] answers the journal's
///   standing tally. This is what makes a second call over an already-decided
///   run idempotent rather than an error.
/// * **`validating`**: a caller error — nothing advances a run to `committing`
///   or `awaiting_approval` without evaluating materiality first, so this
///   state can only mean `apply_run_in` was invoked before that happened.
///   [`DomainError::Internal`], because there is no operator remedy for it.
///
/// # Grouping and the per-plan transaction
///
/// [`repricing_journal_repo::pending_for_run`] is the resume predicate
/// (`inst-mp-journal`): a row already `applied` or `failed` is frozen by the
/// journal's own trigger and is never re-read here. The pending set is
/// grouped by the plan its price row sits on — [`crate::domain::repricing`]'s
/// own module doc gives the reason a selector's `plan_id` axis and D-134's
/// transaction unit are the same column — and each plan's rows commit or fail
/// together in one transaction, per [`apply_plan_in`].
///
/// A plan whose transaction returns `Err` has every one of its rows marked
/// `failed` with that error's rendering as the **one shared reason**
/// (`inst-mr-validate-scope`), in a transaction of its own: the plan's
/// transaction already rolled back by the time this runs, so the failure
/// record cannot live inside it.
///
/// # Errors
/// [`DomainError::NotFound`] when `operation_id` names no run in scope;
/// [`DomainError::Internal`] when the run is not in a state this call may act
/// on, when its stored report cannot be read back into an adjustment or a
/// changeover, or on a storage failure recording a plan's failure or the
/// run's own final transition. A plan-level refusal — a row-local rule
/// violation, a window conflict, the aggregate pass, a lost registry request —
/// never surfaces here: it is absorbed into that plan's journal rows and
/// counted in the returned [`RunOutcome`].
#[allow(
    clippy::too_many_arguments,
    reason = "the crate's own practice for a function whose every argument is a fact only the \
              caller holds, `approval_repo::decide`'s and `overlay_repo::replace_lines`'s reason: \
              the runner and its two collaborators (the registry, the tenant policy reader), the \
              security context the registry request needs, the compiled scope and tenant, the \
              run's own identity, the stamp, and the compensation lane a cancellation hands its \
              lock release to. `policies` is not optional — `PublishRuleParams` cannot be built \
              without it, confirmed by review rather than assumed. Bundling the rest around it \
              would name a struct with exactly one reader"
)]
pub async fn apply_run_in(
    db: &DBProvider<DbError>,
    policies: &PolicyObjectRepo,
    registry: &Arc<dyn CatalogVersionRegistryV1>,
    ctx: &SecurityContext,
    scope: &AccessScope,
    tenant_id: Uuid,
    operation_id: Uuid,
    stamp: AuditStamp,
    compensation: Option<&RunCompensation>,
) -> Result<RunOutcome, DomainError> {
    let conn = db
        .conn()
        .map_err(|e| DomainError::Internal(format!("bss-pricing: repricing apply: conn: {e}")))?;
    let run = bulk_repo::read(&conn, scope, tenant_id, operation_id)
        .await
        .map_err(|e| repo_failure(&e))?
        .ok_or_else(|| DomainError::NotFound {
            subject: "repricing run".to_owned(),
            id: operation_id.to_string(),
        })?;

    if run.state.is_terminal() {
        return tally(&conn, scope, tenant_id, operation_id).await;
    }
    if run.state != BulkState::AwaitingApproval && run.state != BulkState::Committing {
        return Err(DomainError::Internal(format!(
            "bss-pricing: apply_run_in called on repricing run {operation_id} in state {}, \
             which is neither awaiting_approval nor committing nor terminal",
            run.state.as_str()
        )));
    }
    // **Both parses run before the edge is spent**. Neither reads the
    // store, and both are `DomainError::Internal` on a report that cannot be
    // decoded — so with them below the `advance`, a run whose report is
    // undecodable was left in `committing`, which is a state with no door back
    // out. Moving them up costs nothing and makes the failure leave the run
    // exactly where it was.
    let adjustment = crate::domain::repricing::adjustment_of_report(&run.report)?;
    let changeover = crate::domain::repricing::changeover_of_report(&run.report)?;

    let pending = repricing_journal_repo::pending_for_run(&conn, scope, tenant_id, operation_id)
        .await
        .map_err(|e| repo_failure(&e))?;

    let mut target_ids: Vec<Uuid> = pending.iter().map(|row| row.price_id).collect();
    target_ids.sort_unstable();
    target_ids.dedup();

    // Above the `advance` on purpose — see the function's own doc.
    refuse_targets_on_a_held_key(&conn, scope, tenant_id, &target_ids).await?;

    if run.state == BulkState::AwaitingApproval {
        // `inst-bs-commit`'s own state edge — `awaiting_approval -> committing`.
        // **This is the redrive's path, not either accepting surface's**: both of
        // those spend the edge on the request, before the enqueue, so that the run
        // the lane holds is one the abort route reaches. What arrives here in
        // `awaiting_approval` is a run a failed approve left behind, driven forward
        // by a caller that has the run's id and no lane.
        bulk_repo::advance(
            &conn,
            scope,
            tenant_id,
            operation_id,
            // The premise is the state read above, in the statement: two
            // approvals landing at once cannot both spend this edge.
            BulkState::AwaitingApproval,
            BulkState::Committing,
            run.report.clone(),
            stamp.recorded_at,
        )
        .await
        .map_err(|e| repo_failure(&e))?;
    }

    // **Armed before the first lock row is written, not after.** `RunLockGuard`'s
    // own doc is why it exists at all: everything below can still return early
    // through `?`, and this function's own code handles that explicitly — but a
    // panic or a dropped future runs none of it, and only `Drop` reaches those
    // two. **Where** it is armed is a second, separate property, and this line
    // is it: [`bulk_repo::take_locks`] writes one independent statement per row
    // (its own doc says so, and says why — a partial set has to stay
    // releasable), so the first lock row is *durable* while that loop is still
    // awaiting the next insert. A guard armed after the call would therefore
    // leave a window — the whole of `take_locks` — in which a lock is committed
    // and nothing owns it, and a cancellation in that window (a client
    // disconnect, a shutdown signal) is exactly the frozen-rows-and-a-run-stuck-
    // `committing` outcome this guard exists to prevent. It was measured, not
    // reasoned about: with the guard armed after the call, cancelling the apply
    // as soon as its first lock became visible left the locks held and the run
    // committing in every attempt, and about one full-binary run in forty hit
    // the same window by accident
    // (`tests/sqlite_repricing_apply.rs`'s two cancellation tests).
    //
    // **Arming early closes the window in which a lock is owned by nothing; it does
    // not make the release cover every row.** A cancellation inside `take_locks`
    // can leave one insert to land after the guard's own `DELETE`, because the
    // driver has already been handed that statement. `RunLockGuard`'s own doc
    // carries what that costs and why no retry is attempted here.
    let mut lock_guard = RunLockGuard::new(
        db.clone(),
        scope.clone(),
        tenant_id,
        operation_id,
        compensation.cloned(),
    );

    // Everything from here through `finish_run` below is wrapped so a `?`
    // inside it lands on `inner` rather than leaving this function early —
    // `finish_run` must run whether this block succeeds or fails, and an
    // inline `async` block reached by `.await` right here is the plain way to
    // get one `Result` out of a body whose every `?` would otherwise return
    // straight through `apply_run_in` itself.
    //
    // **`take_locks` is inside this block for the guard's sake**, not for the
    // grouping's: armed above, its `?` has to land on `inner` like every other
    // failure below, so that a refused lock still takes the ordinary-`Err` exit
    // (release, disarm, run left `committing` and redrivable) rather than the
    // `Drop` fallback, whose release is best-effort off-thread and can miss a row
    // `take_locks` was still inserting.
    //
    // **The ordinary-`Err` exit's release is what clears a partial hold here.**
    // `take_locks` clears what it took on the collision path, but not on
    // [`RepoError::BulkLocksHeld`] — that variant exists precisely to say the
    // release did not happen. This call site survives it for a reason worth
    // stating: the exit leaves the run `committing`, which is the state a redrive
    // and `abandon_committing_run` both reach, so the rows are recoverable even
    // when this release fails too. A landing that forced the run terminal here
    // would freeze them, which is why the force-terminal sweep is an operator's own
    // act rather than anything this exit infers.
    let inner: Result<PlanLoopEnd, DomainError> = async {
        // **The bulk lock, taken now that the run is `committing`.** See the
        // module doc for what this closes and — as plainly — what it does not.
        bulk_repo::take_locks(
            &conn,
            scope,
            tenant_id,
            operation_id,
            &target_ids,
            stamp.recorded_at,
        )
        .await
        .map_err(|e| repo_failure(&e))?;

        let mut by_plan: BTreeMap<PlanId, Vec<JournalRow>> = BTreeMap::new();
        if !pending.is_empty() {
            let ids: Vec<Uuid> = pending.iter().map(|row| row.price_id).collect();
            let plan_of: HashMap<Uuid, PlanId> =
                price_repo::load_plan_ids(&conn, scope, ids.iter().copied())
                    .await
                    .map_err(|e| repo_failure(&e))?
                    .into_iter()
                    .collect();
            for row in pending {
                match plan_of.get(&row.price_id) {
                    Some(&plan_id) => by_plan.entry(plan_id).or_default().push(row),
                    // Unreachable in this schema — a price row is append-only
                    // and never deleted — but a journal row cannot be left
                    // `pending` forever over an id that cannot be grouped, so
                    // it is decided here rather than silently skipped.
                    None => {
                        repricing_journal_repo::mark_failed(
                            &conn,
                            scope,
                            tenant_id,
                            operation_id,
                            row.price_id,
                            "bss-pricing: this row's price_id no longer resolves to any plan",
                        )
                        .await
                        .map_err(|e| repo_failure(&e))?;
                    }
                }
            }
        }

        apply_by_plan(
            db,
            policies,
            registry,
            ctx,
            scope,
            tenant_id,
            operation_id,
            by_plan,
            &adjustment,
            changeover,
            stamp,
        )
        .await
    }
    .await;

    match inner {
        // **The clean finish.** Every row this call reached is already
        // decided, so `finish_run`'s straggler sweep below is a no-op here —
        // this is the tally-then-advance the old tail did inline, plus the
        // lock release this task adds.
        Ok(PlanLoopEnd::Decided) => {
            land_the_finished_run(
                db,
                &conn,
                &mut lock_guard,
                scope,
                tenant_id,
                operation_id,
                stamp,
            )
            .await
        }
        // **The run was ended under this apply, and the party that ended it
        // already did the finish**: [`abandon_committing_run`] releases the locks,
        // decides every row it finds `pending` and lands the terminal state with
        // its own instant, all in one transaction. `finish_run` here would try to
        // spend `committing -> terminal` a second time over a run that has left
        // `committing`, which its `advance` reports as a `ConcurrentMutation` —
        // and the operator-facing cost of getting it wrong is the abort's
        // `completed_at` and its note being rewritten by the apply that arrived
        // after it. So this arm does what the ordinary-`Err` exit does and nothing
        // more: release whatever of *this run's own* lock rows is left (the
        // statement is keyed on `operation_id`, so a third run's locks over the
        // same price rows are not in its reach), disarm, and answer the journal as
        // it now stands.
        Ok(PlanLoopEnd::Yielded) => {
            yield_to_whoever_ended_the_run(&conn, &mut lock_guard, scope, tenant_id, operation_id)
                .await
        }
        // **An ordinary `Err`, preserving the redrive contract.** See the
        // module doc's own paragraph on why this releases the lock and stops
        // there rather than reaching for `finish_run`'s force-terminal sweep:
        // force-landing the run here freezes every unreached row `failed` with
        // no redrive possible. `RunLockGuard`'s `Drop` fallback makes the same
        // choice for the two exits no code of this function's own is left
        // running for; what this arm adds is a release synchronous with the
        // failure, on the connection already in hand.
        Err(err) => {
            match release_lock_after_ordinary_failure(&conn, scope, tenant_id, operation_id).await {
                Ok(()) => lock_guard.disarm(),
                Err(release_err) => {
                    // The guard stays armed, and its `Drop` fallback retries this
                    // same release off-thread — the same act, not a stronger one.
                    // A store fault that failed this call will usually fail that
                    // retry too, so nothing this gear runs is guaranteed to end
                    // the run: the log names the abort route because an operator
                    // is the remedy.
                    tracing::error!(
                        error = %release_err,
                        run_id = %operation_id,
                        "bss-pricing: repricing apply: releasing the bulk lock after an ordinary \
                         failure also failed; the run is left committing and may still hold its \
                         row locks, so `POST /repricing-runs/{{runId}}/abort` is the remedy and a \
                         redrive route is the capability still owed"
                    );
                }
            }
            Err(err)
        }
    }
}

/// [`apply_run_in`]'s ordinary-`Err` exit: release the bulk lock and return,
/// touching neither the run's own state nor its journal.
///
/// **Deliberately does less than [`finish_run`].** This function's own caller
/// is reached only while `apply_run_in`'s own code is still running — the
/// module doc's own redrive-contract paragraph is why that distinction
/// matters: leaving the run `committing` with its unreached rows `pending` is
/// what lets a second call to `apply_run_in` retake the lock (taken
/// unconditionally at that function's own top for a run already `committing`)
/// and carry on, rather than freezing every unreached row `failed` with no
/// redrive possible — which is exactly what [`finish_run`]'s force-terminal
/// sweep costs when it lands on a run whose work is merely unreached, and why
/// that sweep is spent only by a clean finish and by
/// [`abandon_committing_run`]'s operator-driven abort.
///
/// # Errors
/// [`DomainError`] on a storage failure releasing the lock.
async fn release_lock_after_ordinary_failure(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    operation_id: Uuid,
) -> Result<(), DomainError> {
    bulk_repo::release_locks(runner, scope, tenant_id, operation_id)
        .await
        .map_err(|e| repo_failure(&e))?;
    Ok(())
}

/// How [`apply_by_plan`]'s loop ended, which decides whether [`apply_run_in`]
/// spends [`finish_run`] on the run or leaves the finish to whoever ended it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlanLoopEnd {
    /// Every plan the run selected was reached and decided, so the run is this
    /// call's to land.
    Decided,
    /// The run stopped reading `committing` while the loop was running: another
    /// party — [`abandon_committing_run`] is the only one there is — has taken it
    /// terminal, and the loop stopped at a plan boundary rather than writing on
    /// under it.
    Yielded,
}

/// [`apply_run_in`]'s clean finish: [`finish_run`] in its own transaction, and the
/// two answers to its failure.
///
/// A function rather than the tail's third nested `match`, which
/// `clippy::cognitive_complexity` is what says.
///
/// On success the guard is disarmed — the release happened inside the transaction
/// that landed the run. On failure it stays **armed**: the whole transaction rolled
/// back, so the lock is still held and the guard's `Drop` is the one thing left that
/// will release it.
///
/// # Errors
/// [`DomainError::Internal`] wrapping the finish transaction's own failure — unless
/// the run has left `committing`, in which case see [`finish_failure_or_yield`].
#[allow(
    clippy::too_many_arguments,
    reason = "apply_run_in's own reason: the provider the transaction needs and the \
              connection the yield reads on are different handles, and the rest is the \
              run's address plus the guard this exit disarms"
)]
async fn land_the_finished_run(
    db: &DBProvider<DbError>,
    conn: &impl DBRunner,
    lock_guard: &mut RunLockGuard,
    scope: &AccessScope,
    tenant_id: Uuid,
    operation_id: Uuid,
    stamp: AuditStamp,
) -> Result<RunOutcome, DomainError> {
    let scope_for_tx = scope.clone();
    let (_, outcome) = db
        .db()
        .in_transaction::<RunOutcome, DomainError, _>(move |txn| {
            let scope = scope_for_tx.clone();
            Box::pin(async move {
                finish_run(
                    txn,
                    &scope,
                    tenant_id,
                    operation_id,
                    UNEXPECTED_STRAGGLER_REASON,
                    stamp.recorded_at,
                )
                .await
            })
        })
        .await;
    match outcome.map_err(|infra_err| {
        infra_err.into_domain(|infra| {
            DomainError::Internal(format!(
                "bss-pricing: repricing apply finish transaction: {infra}"
            ))
        })
    }) {
        Ok(outcome) => {
            lock_guard.disarm();
            Ok(outcome)
        }
        // **Unless the abort won this last gap**, which is the one
        // [`apply_by_plan`]'s two checks cannot reach: between its post-loop read
        // and this transaction, `advance` refuses a move off a state the run has
        // left and the refusal arrives here as a failed finish.
        Err(finish_err) => {
            finish_failure_or_yield(conn, lock_guard, scope, tenant_id, operation_id, finish_err)
                .await
        }
    }
}

/// [`finish_run`]'s failure, after asking whether the abort won the last gap.
///
/// A function rather than two more branches in [`apply_run_in`]'s tail, which
/// `clippy::cognitive_complexity` is what says: that tail already carries three
/// exits and the reader's question here is a single one.
///
/// The read's own failure is not allowed to displace `err`: only a definite "the run
/// has left `committing`" diverts to the yield.
///
/// # Errors
/// `err` itself when the run is still the caller's to land, and
/// [`yield_to_whoever_ended_the_run`]'s otherwise.
async fn finish_failure_or_yield(
    runner: &impl DBRunner,
    lock_guard: &mut RunLockGuard,
    scope: &AccessScope,
    tenant_id: Uuid,
    operation_id: Uuid,
    err: DomainError,
) -> Result<RunOutcome, DomainError> {
    if matches!(
        still_committing(runner, scope, tenant_id, operation_id).await,
        Ok(false)
    ) {
        return yield_to_whoever_ended_the_run(runner, lock_guard, scope, tenant_id, operation_id)
            .await;
    }
    Err(err)
}

/// The exit an apply takes when the run it was applying was **ended under it**.
///
/// Two callers, both in [`apply_run_in`]'s own tail: the loop's yield, and the
/// finish transaction refused because the run had left `committing` before it ran.
///
/// [`abandon_committing_run`] did the finish inside its own transaction — locks
/// released, every `pending` row decided, the terminal state and `completed_at`
/// stamped — so there is nothing here to land and [`finish_run`] must not be spent a
/// second time. What is left is the release: keyed on `operation_id`, so a third run
/// that has since taken locks over the same price rows is not in its reach. The
/// guard is disarmed only when that release worked; left armed, its `Drop` retries
/// the same act off-thread, which is the ordinary-`Err` exit's arrangement and its
/// reason.
///
/// # Errors
/// [`DomainError`] on a storage failure tallying the journal.
async fn yield_to_whoever_ended_the_run(
    runner: &impl DBRunner,
    lock_guard: &mut RunLockGuard,
    scope: &AccessScope,
    tenant_id: Uuid,
    operation_id: Uuid,
) -> Result<RunOutcome, DomainError> {
    match release_lock_after_ordinary_failure(runner, scope, tenant_id, operation_id).await {
        Ok(()) => lock_guard.disarm(),
        Err(release_err) => {
            tracing::error!(
                error = %release_err,
                run_id = %operation_id,
                "bss-pricing: repricing apply: the run was ended under this apply and releasing \
                 its bulk lock failed; the lock rows this run holds may still stand, and the \
                 abort route's own release is the remedy"
            );
        }
    }
    tally(runner, scope, tenant_id, operation_id).await
}

/// The per-plan loop half of [`apply_run_in`], split out so the closure that
/// wraps it above has one call to make rather than the loop's own body typed
/// out a second time.
///
/// # It yields to the abort door, because the two can be in flight at once
///
/// `committing` is the state a run stands in *while* this loop applies it, so
/// `POST /repricing-runs/{runId}/abort` ([`abandon_committing_run`]) can land in the
/// middle of a run this function is still working. The abort is unconditional by
/// design — an operator's door has to work every time — so the party that yields is
/// the one still working, and this is where it yields: the run's state is re-read
/// before every plan and after the last, and the loop stops the moment it no longer
/// reads `committing`.
///
/// What that prevents is not a lost write but a **concurrent** one. The abort
/// deletes D-134's lock rows and lands the run terminal; a third run over the same
/// price rows may then take those locks and start writing. An applier that carried
/// on would be writing successors into rows another run holds the lock over, having
/// itself no lock any more — the exclusion `inst-bk-lock` exists for, gone while
/// both writers believe they have it. The journal's freeze makes that visible only
/// afterwards, and only sometimes: the abort marks the rows it found `pending`
/// `failed`, so this loop's next `mark_applied` is refused and the plan's whole
/// transaction rolls back — but a plan whose rows a *third* run had already taken
/// over does not come back through this journal at all.
///
/// **The state read alone does not close it**, and that is measured rather than
/// reasoned about. The read sits outside the plan's own transaction, so an abort
/// landing between the two is invisible to it — and that is not a rare interleaving
/// but the *usual* one: the abort's own transaction can only start in a gap between
/// this loop's transactions, so it starts in the gap this read just left. With the
/// pre-plan check as the only guard, `an_abort_landing_mid_apply_stops_the_applier_at_the_next_plan`
/// fails.
///
/// So the yield has a **second half, on the plan's failure path**: a plan whose
/// transaction returned `Err` while the run has left `committing` is the abort's
/// doing, and the loop yields there instead of recording a failure. Nothing lands
/// half-applied in that window — the plan's transaction is one transaction, and the
/// swap refusal that ends it rolls its successors back with it — so what the second
/// half saves is not a write but the misreport: the plan's rows already carry the
/// abort's own reason, and re-marking them is refused for the same reason the plan
/// was — which is how an operator's clean abort turns into an `Err` out of the
/// apply.
///
/// Between the two, the applier stops at the first plan boundary after the abort
/// lands, and the run's finish belongs to whoever ended it.
#[allow(
    clippy::too_many_arguments,
    reason = "apply_run_in's own reason: every argument here is a fact only that function's \
              caller holds, cut to exactly what grouping and committing the run's plans needs"
)]
async fn apply_by_plan(
    db: &DBProvider<DbError>,
    policies: &PolicyObjectRepo,
    registry: &Arc<dyn CatalogVersionRegistryV1>,
    ctx: &SecurityContext,
    scope: &AccessScope,
    tenant_id: Uuid,
    operation_id: Uuid,
    by_plan: BTreeMap<PlanId, Vec<JournalRow>>,
    adjustment: &Adjustment,
    changeover: OffsetDateTime,
    stamp: AuditStamp,
) -> Result<PlanLoopEnd, DomainError> {
    let conn = db.conn().map_err(|e| {
        DomainError::Internal(format!("bss-pricing: repricing apply: per-plan conn: {e}"))
    })?;
    for (plan_id, rows) in by_plan {
        if !still_committing(&conn, scope, tenant_id, operation_id).await? {
            return Ok(PlanLoopEnd::Yielded);
        }
        let row_ids: Vec<Uuid> = rows.iter().map(|row| row.price_id).collect();
        let scope_for_tx = scope.clone();
        let policies_for_tx = policies.clone();
        let registry_for_tx = Arc::clone(registry);
        let ctx_for_tx = ctx.clone();
        let adjustment_for_tx = adjustment.clone();
        let (_, outcome) = db
            .db()
            .in_transaction::<(), DomainError, _>(move |txn| {
                Box::pin(async move {
                    // Boxed a second time on purpose — `infra::cutover::cut_over`'s
                    // own precedent and reason: `apply_plan_in`'s future is large
                    // (a whole plan's row loop plus the aggregate pass live across
                    // its awaits) and `clippy::large_futures` is what says so. One
                    // allocation per plan, not that size on every task's stack.
                    Box::pin(apply_plan_in(
                        txn,
                        &policies_for_tx,
                        &registry_for_tx,
                        &ctx_for_tx,
                        &scope_for_tx,
                        tenant_id,
                        operation_id,
                        plan_id,
                        &rows,
                        &adjustment_for_tx,
                        changeover,
                        stamp,
                    ))
                    .await
                })
            })
            .await;

        if let Err(err) = outcome.map_err(|err| {
            err.into_domain(|infra| {
                DomainError::Internal(format!(
                    "bss-pricing: repricing apply transaction (plan {plan_id}): {infra}"
                ))
            })
        }) {
            // **A plan that failed under a run somebody ended is that party's act,
            // not a refusal of this plan's own rows**, and this is the second half
            // of the yield: the check above cannot catch an abort that lands *after*
            // it and before this plan's transaction commits, and the shape it takes
            // when it does is exactly this `Err` — the abort decides the rows it
            // finds `pending`, so the plan's `mark_applied` meets a row that now
            // reads `failed` and the journal's swap refusal rolls the whole plan
            // back. Recording a failure over it would then be refused for the same
            // reason and turn an operator's clean abort into an `Err` out of the
            // apply, which is what a run's own lane reports as a broken apply.
            if !still_committing(&conn, scope, tenant_id, operation_id).await? {
                return Ok(PlanLoopEnd::Yielded);
            }
            // **A separate transaction, never the one that just rolled back**
            // (module doc, and the task-5 brief's own consequence): every row of
            // this plan reads `failed` with one shared reason, the rolled-back
            // transaction's own rendering.
            let reason = failure_reason(&err);
            let scope_for_tx = scope.clone();
            let (_, mark_outcome) = db
                .db()
                .in_transaction::<(), DomainError, _>(move |txn| {
                    let scope = scope_for_tx.clone();
                    let reason = reason.clone();
                    Box::pin(async move {
                        for price_id in row_ids {
                            repricing_journal_repo::mark_failed(
                                txn,
                                &scope,
                                tenant_id,
                                operation_id,
                                price_id,
                                &reason,
                            )
                            .await
                            .map_err(|e| repo_failure(&e))?;
                        }
                        Ok(())
                    })
                })
                .await;
            mark_outcome.map_err(|infra_err| {
                infra_err.into_domain(|infra| {
                    DomainError::Internal(format!(
                        "bss-pricing: repricing apply failure recording (plan {plan_id}): {infra}"
                    ))
                })
            })?;
        }
    }

    // **The last check, and it is the one that guards `finish_run` itself.** An
    // abort landing after the final plan committed and before the finish's own
    // transaction leaves the run terminal with every row decided, and a
    // `finish_run` on top of that spends `committing -> terminal` over a run that
    // has left `committing` — the failure this loop's yield exists to avoid,
    // reached through the one gap a per-plan check leaves. The gap *this* read
    // leaves in turn, between it and that transaction, is the caller's own last
    // arm.
    if !still_committing(&conn, scope, tenant_id, operation_id).await? {
        return Ok(PlanLoopEnd::Yielded);
    }

    // No tally, no advance: `apply_run_in`'s own tail ([`finish_run`]) is the
    // one place both happen now, reached whether this loop finishes clean or
    // its caller's `inner` block short-circuited before ever calling this
    // function at all — a second copy here would be a second answer to "is
    // this run terminal yet" that `finish_run` does not read.
    Ok(PlanLoopEnd::Decided)
}

/// Whether the run still reads `committing` — [`apply_by_plan`]'s yield predicate.
///
/// A run that has left `committing` has left it *terminal*: nothing moves a
/// `committing` run anywhere else, and `pricing_bulk_operation`'s transition
/// trigger is what says so. So one comparison answers the whole question and a
/// roster of terminal states is not needed here.
///
/// A run that does not read back at all is [`DomainError::NotFound`] rather than a
/// yield: the row is never deleted, so this is the scope answering differently
/// mid-apply, which no caller should absorb as "somebody else finished it".
///
/// # Errors
/// [`DomainError::Internal`] on a storage failure reading the run;
/// [`DomainError::NotFound`] when the run no longer reads back in this scope.
async fn still_committing(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    operation_id: Uuid,
) -> Result<bool, DomainError> {
    let run = bulk_repo::read(runner, scope, tenant_id, operation_id)
        .await
        .map_err(|e| repo_failure(&e))?
        .ok_or_else(|| DomainError::NotFound {
            subject: "repricing run".to_owned(),
            id: operation_id.to_string(),
        })?;
    Ok(run.state == BulkState::Committing)
}

/// The run's journal, summed into [`RunOutcome`] — [`apply_run_in`]'s answer
/// on every path, not only the one that just wrote.
async fn tally(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    operation_id: Uuid,
) -> Result<RunOutcome, DomainError> {
    let journal = repricing_journal_repo::list_for_run(runner, scope, tenant_id, operation_id)
        .await
        .map_err(|e| repo_failure(&e))?;
    let applied = journal
        .iter()
        .filter(|row| row.state == JournalState::Applied)
        .count();
    let failed = journal
        .iter()
        .filter(|row| row.state == JournalState::Failed)
        .count();
    Ok(RunOutcome { applied, failed })
}

/// Release the run's bulk lock and land it on a terminal state, marking every
/// journal row this call still finds `pending` `failed` first.
///
/// **This is the force-terminal sweep, not the release every exit gets.**
/// [`apply_run_in`]'s own explicit tail calls this on a clean finish only,
/// where the per-plan loop has already decided every row it touched and the
/// straggler loop below is a no-op — so on that path this is the run-level
/// tally-then-advance plus the lock release, and nothing else. An **ordinary
/// `Err`** after lock-taking does not reach this function at all: it releases
/// the lock and leaves the run `committing` with its unreached rows `pending`
/// — see the module doc's own paragraph on why that distinction is
/// load-bearing rather than tidiness.
///
/// **[`RunLockGuard`]'s `Drop` fallback deliberately does not reach this**, and
/// [`abandon_committing_run`] is the one other place a force-terminal sweep runs:
/// an operator asking for the abort is a decision, where a dropped future is an
/// inference — and inferring one costs the unreached rows the `pending` state a
/// redrive needs. So
/// the straggler loop below is a no-op on every path that reaches this function,
/// and it stays because "no straggler is expected" and "a straggler cannot happen"
/// are different claims — [`UNEXPECTED_STRAGGLER_REASON`] is what an operator reads
/// if the second one is ever false.
///
/// # The transaction is the argument, not the runner
///
/// It takes `&DbTx<'_>` because the four writes below are one act: release the
/// lock, decide the stragglers, and land the run on the state the tally names. Run
/// as independent statements, a failure between any two of them left a run whose
/// lock was released and whose state still said `committing`, or one landed
/// terminal over a journal only half decided — and neither is a state the run's own
/// invariants admit. The type is the contract, as `plan_repo::publish_revision`
/// says of itself: a caller cannot hand this a plain connection by accident.
///
/// [`bulk_repo::take_locks`] is the one sibling that must **not** run inside a
/// transaction (its own doc says why: Postgres aborts the enclosing transaction on
/// the insert its conflict path issues), and nothing here takes a lock.
///
/// # Errors
/// [`DomainError`] on a storage failure releasing the lock, marking a
/// straggler, tallying the journal or advancing the run — the same failures
/// [`bulk_repo`] and [`repricing_journal_repo`] themselves raise.
async fn finish_run(
    runner: &DbTx<'_>,
    scope: &AccessScope,
    tenant_id: Uuid,
    operation_id: Uuid,
    straggler_reason: &str,
    now: OffsetDateTime,
) -> Result<RunOutcome, DomainError> {
    bulk_repo::release_locks(runner, scope, tenant_id, operation_id)
        .await
        .map_err(|e| repo_failure(&e))?;

    let stragglers =
        repricing_journal_repo::pending_for_run(runner, scope, tenant_id, operation_id)
            .await
            .map_err(|e| repo_failure(&e))?;
    for row in stragglers {
        repricing_journal_repo::mark_failed(
            runner,
            scope,
            tenant_id,
            operation_id,
            row.price_id,
            straggler_reason,
        )
        .await
        .map_err(|e| repo_failure(&e))?;
    }

    let outcome = tally(runner, scope, tenant_id, operation_id).await?;
    let run = bulk_repo::read(runner, scope, tenant_id, operation_id)
        .await
        .map_err(|e| repo_failure(&e))?
        .ok_or_else(|| DomainError::NotFound {
            subject: "repricing run".to_owned(),
            id: operation_id.to_string(),
        })?;
    bulk_repo::advance(
        runner,
        scope,
        tenant_id,
        operation_id,
        // Only a `committing` run reaches here — `apply_run_in` returns early on a
        // terminal one, and this function is otherwise reached from the drop
        // guard's fallback over a run that never left `committing`. Carried into
        // the statement so a fallback racing a clean finish cannot re-stamp the
        // report the clean finish wrote.
        BulkState::Committing,
        if outcome.failed > 0 {
            BulkState::CompletedWithConflicts
        } else {
            BulkState::Completed
        },
        run.report,
        now,
    )
    .await
    .map_err(|e| repo_failure(&e))?;

    Ok(outcome)
}

/// The reason a clean finish's own call to [`finish_run`] would mark a
/// straggler with, if it ever found one.
///
/// **Never expected to fire.** By the time `apply_run_in`'s `inner` block
/// returns `Ok(())`, the per-plan loop has decided every row it reached —
/// `pending_for_run`'s own doc calls this the re-drive's whole safety
/// property — so a row still `pending` here would mean that property broke,
/// not that an ordinary failure occurred. The message says so rather than
/// reusing [`ABORTED_STRAGGLER_REASON`]'s wording, which would misdescribe
/// a logic gap as an interrupted apply.
const UNEXPECTED_STRAGGLER_REASON: &str = "bss-pricing: this journal row was still pending after \
    the apply's own per-plan loop finished without error; every row the loop reaches should \
    already be decided, so this indicates a logic gap rather than an ordinary failure";

/// The reason [`abandon_committing_run`] marks a straggler row with.
///
/// It says what happened *and* what is owed, `infra::bulk::INTERRUPTED_NOTE`'s
/// rule: the rows this run did apply are in the store and are not undone by the
/// abort, so an operator reading only "aborted" would not know which half of their
/// batch landed. The journal is where they find out, which is why every row it
/// stops is stopped with a reason rather than left `pending` under a terminal run.
const ABORTED_STRAGGLER_REASON: &str = "bss-pricing: an operator aborted this mass-repricing run \
    before this row's plan was reached; the run's bulk lock has been released and the run has \
    landed on a terminal state without applying it - resubmit a new run over its key if the \
    repricing still needs to land";

/// Stop a `committing` mass-repricing run: release its bulk lock, decide every row
/// its apply never reached, and land it terminal with the abort noted on its report.
///
/// `POST /bss-pricing/v1/repricing-runs/{runId}/abort`'s whole body, and
/// `infra::bulk::abandon_committing_run`'s counterpart one plane over. It exists
/// because `committing` needs an owner: [`apply_run_in`]'s ordinary-`Err` exit and
/// [`RunLockGuard`]'s `Drop` both leave a run there deliberately — that is what
/// keeps a redrive possible — and this is the only thing that spends what they
/// preserve. Without it a run that meets a transient storage fault mid-apply is
/// `committing` forever with its unreached rows `pending` forever.
///
/// # The premise is `committing`, and a terminal run is refused
///
/// [`DomainError::LifecycleForbidden`] for any other state, which is the sibling's
/// rule and its reason: a terminal run's locks are already released and every row
/// it selected is already decided, and a `bulk_repo::advance` to the state a run is
/// already in returns early on both engines — so a sweep let loose on a terminal
/// run would rewrite `completed_at` and stamp an abort note over a report whose
/// every row *was* attempted. The caller is the one place that can tell a
/// **replay** of this operation from that, because the note is written here and by
/// nothing else.
///
/// # `committing` is a state a run can be *working* in, and the applier is what yields
///
/// The premise is not that nothing else is running. `committing` is where a run
/// stands **while** [`RunApplyLane`] applies it — both accepting surfaces spend the
/// edge on the request and hand the apply to the lane — so this door and
/// [`apply_run_in`] can be in flight over one run at the same time, and the release
/// below can delete D-134's lock rows out from under a loop that is still writing.
///
/// This call stays unconditional anyway: an operator's door has to work every time,
/// and "is somebody applying this right now" is not a question the store can answer
/// — a live applier and a dead replica's abandoned run look identical from here,
/// and refusing the second is the frozen-rows outcome this route exists to end. So
/// the party that yields is the one still working: [`apply_by_plan`] re-reads the
/// run's state before every plan and after the last, and stops when it no longer
/// reads `committing`. That is what keeps the rows this call marks `failed` from
/// being re-marked, and the terminal state and `completed_at` it stamps from being
/// rewritten by an apply that arrived after it. That loop's own doc carries what the
/// yield does **not** close.
///
/// # One transaction, where the sibling orders two statements
///
/// `infra::bulk::abandon_committing_run` releases the lock *before* the terminal
/// move on autocommit, and D-300 is why: with the order reversed, a release that
/// failed left the run terminal and every lock held, and the state guard then
/// refused the retry that would have rescued it. This one gets the same property
/// from atomicity instead — a failure anywhere rolls the release back with
/// everything else, so the run is still `committing` and the operator simply asks
/// again. The ordering trick is not needed when nothing can land half-applied, and
/// the sibling cannot have it: `commit_batch`'s own guard calls it from a `Drop`,
/// where there is no transaction to be inside.
///
/// # The rows are marked `failed`, and there is no `abandoned` token to mark them
///
/// `chk_pricing_repricing_journal_state` admits `pending | applied | failed` and
/// [`crate::domain::bulk::JournalState`] has three variants; a fourth would be a
/// wire token the design set has not declared, which D-204 clause (2) refuses. So
/// an aborted straggler is `failed` with [`ABORTED_STRAGGLER_REASON`], which is
/// what an operator can read and resubmit from — and force-failing them here is the
/// operator's own decision, where the same act inside [`RunLockGuard`]'s `Drop`
/// fallback would be an inference from a dropped future.
///
/// # Errors
/// [`DomainError::NotFound`] when `operation_id` names no run in scope;
/// [`DomainError::LifecycleForbidden`] when the run is not `committing`;
/// [`DomainError::Internal`] on a storage failure inside the sweep.
pub async fn abandon_committing_run(
    db: &DBProvider<DbError>,
    scope: &AccessScope,
    tenant_id: Uuid,
    operation_id: Uuid,
    note: &str,
    at: OffsetDateTime,
) -> Result<BulkOperationRecord, DomainError> {
    let conn = db.conn().map_err(|e| {
        DomainError::Internal(format!("bss-pricing: repricing run abort: conn: {e}"))
    })?;
    let run = bulk_repo::read(&conn, scope, tenant_id, operation_id)
        .await
        .map_err(|e| repo_failure(&e))?
        .ok_or_else(|| DomainError::NotFound {
            subject: "repricing run".to_owned(),
            id: operation_id.to_string(),
        })?;
    if run.state != BulkState::Committing {
        return Err(DomainError::LifecycleForbidden(format!(
            "repricing run {operation_id} is {}; the abort acts on a run that is still \
             committing, and a run that is over has no locks to clear and no work to stop",
            run.state.as_str()
        )));
    }

    let mut report = run.report;
    if let Some(object) = report.as_object_mut() {
        object.insert(
            crate::infra::bulk::ABORTED_MEMBER.to_owned(),
            serde_json::json!(note),
        );
    } else {
        // Not an object: keep what was there rather than dropping the note, the
        // sibling's rule. "Added to, never replaced" still holds — the prior value
        // is carried — and the run stops being indistinguishable from one that
        // completed on its own, which is the only thing telling a replay of this
        // operation from an ordinary finish.
        report = serde_json::json!({
            crate::infra::bulk::ABORTED_MEMBER: note,
            crate::infra::bulk::PRIOR_REPORT_MEMBER: report,
        });
    }

    let scope_for_tx = scope.clone();
    let (_, outcome) = db
        .db()
        .in_transaction::<BulkOperationRecord, DomainError, _>(move |txn| {
            let scope = scope_for_tx.clone();
            let report = report.clone();
            Box::pin(async move {
                bulk_repo::release_locks(txn, &scope, tenant_id, operation_id)
                    .await
                    .map_err(|e| repo_failure(&e))?;
                let stragglers =
                    repricing_journal_repo::pending_for_run(txn, &scope, tenant_id, operation_id)
                        .await
                        .map_err(|e| repo_failure(&e))?;
                for row in stragglers {
                    repricing_journal_repo::mark_failed(
                        txn,
                        &scope,
                        tenant_id,
                        operation_id,
                        row.price_id,
                        ABORTED_STRAGGLER_REASON,
                    )
                    .await
                    .map_err(|e| repo_failure(&e))?;
                }
                let outcome = tally(txn, &scope, tenant_id, operation_id).await?;
                bulk_repo::advance(
                    txn,
                    &scope,
                    tenant_id,
                    operation_id,
                    // Carried into the statement rather than re-read: the guard
                    // above judged the state, and a second abort racing this one
                    // must lose on the compare-and-swap rather than both landing.
                    BulkState::Committing,
                    if outcome.failed > 0 {
                        BulkState::CompletedWithConflicts
                    } else {
                        BulkState::Completed
                    },
                    report,
                    at,
                )
                .await
                .map_err(|e| repo_failure(&e))
            })
        })
        .await;
    outcome.map_err(|infra_err| {
        infra_err.into_domain(|infra| {
            DomainError::Internal(format!("bss-pricing: repricing run abort sweep: {infra}"))
        })
    })
}

/// The note [`abandon_committing_run`] stamps on the report it lands.
///
/// Under `infra::bulk::ABORTED_MEMBER` rather than a key of this plane's own, for
/// that constant's stated reason: an operator reading a run's report should not have
/// to know which plane's abort door stopped it.
pub const ABORT_NOTE: &str = "an operator aborted this run; rows its apply had not reached were not \
    attempted and are marked failed in the journal";

/// One dropped apply's owed bulk-lock release, as a message.
///
/// A value rather than a closure because it crosses a channel into a task the
/// lifecycle owns, and because it is the whole of what a dropped apply still owes:
/// [`RunLockGuard`]'s `Drop` releases the lock and stops, leaving the run
/// `committing` for `POST …/repricing-runs/{runId}/abort` or a redrive to finish.
pub struct RunLockRelease {
    scope: AccessScope,
    tenant_id: Uuid,
    operation_id: Uuid,
}

/// The lane a dropped [`apply_run_in`] hands its lock release to.
///
/// [`Drop::drop`] cannot `.await`, so the release has to leave the dropping thread
/// somehow. Spawning it detached on [`tokio::runtime::Handle::try_current`] — which
/// is what this type replaced — makes it a task nothing owns: no cancellation
/// token, no lifecycle registration, and so a shutdown abandons it mid-statement
/// against a database the process is closing, which is the one state a
/// `stop_timeout` cannot help with. Sending it here instead puts the release on
/// [`RunCompensationWorker`], which `module.rs`'s `serve` spawns under the gear's
/// own token and joins on the way out.
///
/// Cloned into every state that reaches [`apply_run_in`]; the channel is bounded,
/// because an unbounded queue of owed releases is a memory leak with a
/// database-shaped cause and no back pressure to notice it by.
#[derive(Clone)]
pub struct RunCompensation {
    sender: tokio::sync::mpsc::Sender<RunLockRelease>,
}

/// The draining half of [`RunCompensation`], owned by the gear's lifecycle.
pub struct RunCompensationWorker {
    receiver: tokio::sync::mpsc::Receiver<RunLockRelease>,
    db: DBProvider<DbError>,
}

/// How many owed releases the lane holds before a sender falls back to a detached
/// task.
///
/// One dropped apply per queued entry, and a drop is a cancellation or a panic —
/// so a queue this deep already means the release side is not keeping up, which is
/// what the `warn` on the full path is for. Sized to absorb a burst of client
/// disconnects rather than to bound a steady rate.
const COMPENSATION_LANE_DEPTH: usize = 64;

/// Build the compensation lane: the handle every apply carries, and the worker the
/// lifecycle runs.
///
/// One function rather than two constructors, because the two halves are only ever
/// useful as a pair and a lane whose worker was never spawned is exactly the state
/// this type exists to remove.
#[must_use]
pub fn run_compensation_lane(db: DBProvider<DbError>) -> (RunCompensation, RunCompensationWorker) {
    let (sender, receiver) = tokio::sync::mpsc::channel(COMPENSATION_LANE_DEPTH);
    (
        RunCompensation { sender },
        RunCompensationWorker { receiver, db },
    )
}

impl RunCompensationWorker {
    /// Drain owed lock releases until `cancel` fires, then drain what is already
    /// queued and stop.
    ///
    /// **The post-cancellation drain is not tidiness.** Every entry in the queue is
    /// a bulk lock held over price rows by a run nothing will drive again; the lock
    /// table has no sweeper and D-37's lease takeover is designed and unbuilt, so an
    /// entry dropped here freezes those rows until an operator clears them by hand.
    /// It is bounded by what the channel holds at that instant — never by new
    /// arrivals — so it cannot outrun the gear's `stop_timeout`.
    ///
    /// A release that fails is logged and dropped rather than retried: the run is
    /// still `committing`, which is the state the abort route and a redrive both
    /// reach, so the remedy survives the failure. Retrying here would be a second
    /// scheduler inside a drain.
    pub async fn run(mut self, cancel: CancellationToken) {
        loop {
            tokio::select! {
                () = cancel.cancelled() => break,
                request = self.receiver.recv() => {
                    let Some(request) = request else { return };
                    self.release(request).await;
                }
            }
        }
        self.drain_pending().await;
    }

    /// Release every lock request the lane holds **right now**, and answer how many.
    ///
    /// [`Self::run`]'s post-cancellation arm, and a seam a caller can drive one step
    /// with. It takes nothing new from the channel once it has caught up, so it
    /// terminates on a queue that is still being written to — which is what makes it
    /// bounded work rather than a second scheduler.
    ///
    /// The count is what a caller learns from: a drain that handled nothing is a
    /// guard that never enqueued, and those are different defects from a release that
    /// ran and failed.
    pub async fn drain_pending(&mut self) -> usize {
        let mut handled = 0;
        while let Ok(request) = self.receiver.try_recv() {
            self.release(request).await;
            handled += 1;
        }
        handled
    }

    /// Release one run's bulk lock, saying so when it cannot.
    async fn release(&self, request: RunLockRelease) {
        let conn = match self.db.conn() {
            Ok(conn) => conn,
            Err(e) => {
                tracing::error!(
                    error = %e,
                    run_id = %request.operation_id,
                    "bss-pricing: the repricing compensation lane could not open a connection to \
                     release a dropped apply's bulk lock"
                );
                return;
            }
        };
        if let Err(e) = bulk_repo::release_locks(
            &conn,
            &request.scope,
            request.tenant_id,
            request.operation_id,
        )
        .await
        {
            tracing::error!(
                error = %e,
                run_id = %request.operation_id,
                "bss-pricing: the repricing compensation lane could not release a dropped apply's \
                 bulk lock; the run is still committing, so the abort route is the remedy"
            );
        }
    }
}

/// One accepted run's owed apply, as a message.
///
/// The scope and the security context are the ones the accepting request already
/// **decided**: it compiled them from the PDP before it answered, so this lane
/// replays an authority a principal earned rather than deciding one where no
/// principal is present to decide it for. The stamp carries that request's
/// correlation id, which is what keeps every record the apply writes on the one id
/// the acceptance's own record carries (D-178 clause (2)).
pub struct RunApplyRequest {
    /// The accepting request's security context — what the apply's `CatalogVersion`
    /// request is made under.
    pub ctx: SecurityContext,
    /// The scope the accepting request compiled, never one this lane resolves.
    pub scope: AccessScope,
    /// The run's tenant.
    pub tenant_id: Uuid,
    /// The run's **minted** `operation_id`, not the caller's `run_id`.
    pub operation_id: Uuid,
    /// The stamp every record this apply writes carries.
    pub stamp: AuditStamp,
}

/// The lane a surface that accepts a repricing run hands the apply to.
///
/// Cloned into every state that mounts such a surface; the channel is bounded,
/// because an unbounded queue of accepted-but-unapplied runs is a memory leak whose
/// only symptom is latency and which no back pressure would report.
#[derive(Clone)]
pub struct RunApplyLane {
    sender: tokio::sync::mpsc::Sender<RunApplyRequest>,
}

/// The applying half of [`RunApplyLane`], owned by the gear's lifecycle.
pub struct RunApplyWorker {
    receiver: tokio::sync::mpsc::Receiver<RunApplyRequest>,
    db: DBProvider<DbError>,
    policies: PolicyObjectRepo,
    registry: Arc<dyn CatalogVersionRegistryV1>,
    compensation: RunCompensation,
}

/// How many accepted runs the lane holds before an enqueue is refused.
///
/// One run per entry, and every entry is a run already durable and already
/// `committing` — so a full lane is not lost work, it is work an operator has to
/// spend `POST …/repricing-runs/{runId}/abort` on. Sized to absorb a burst of
/// submissions rather than to bound a steady rate: a lane that stays full means the
/// applies are not keeping up, which is what the `error` on the refused path says.
const APPLY_LANE_DEPTH: usize = 64;

/// Build the apply lane: the handle every surface that accepts a run holds, and the
/// worker the lifecycle runs.
///
/// One function rather than two constructors, [`run_compensation_lane`]'s reason: a
/// lane whose worker was never spawned is a surface that accepts runs and applies
/// none.
///
/// The compensation handle is taken **by value** because this worker is the gear's
/// only caller of [`apply_run_in`]: a dropped apply's owed lock release can only be
/// owed by an apply this worker started.
#[must_use]
pub fn run_apply_lane(
    db: DBProvider<DbError>,
    policies: PolicyObjectRepo,
    registry: Arc<dyn CatalogVersionRegistryV1>,
    compensation: RunCompensation,
) -> (RunApplyLane, RunApplyWorker) {
    let (sender, receiver) = tokio::sync::mpsc::channel(APPLY_LANE_DEPTH);
    (
        RunApplyLane { sender },
        RunApplyWorker {
            receiver,
            db,
            policies,
            registry,
            compensation,
        },
    )
}

impl RunApplyLane {
    /// Hand the lane one accepted run's apply, saying so when it cannot take it.
    ///
    /// **Never fails the request that accepted the run.** The run and its journal
    /// are durable before this is reached and the run stands at `committing` — the
    /// open's non-material path was left there by `open_run`, and the approve spends
    /// [`begin_committing_in`] before enqueueing precisely so this holds for both
    /// arms — which `POST …/repricing-runs/{runId}/abort` reaches. A refused enqueue
    /// therefore costs an operator act rather than the acceptance, and it is that
    /// invariant, not the wording below, that makes the two `error` messages true.
    /// Falling back to running the apply
    /// inline on the caller's own future would put an unbounded hold back on the
    /// request exactly when the gear is most loaded, which is the whole of what this
    /// lane exists to remove.
    pub fn enqueue(&self, request: RunApplyRequest) {
        // `try_send` and not `send`: the caller is a request future that must answer
        // now, so awaiting a full lane would make one tenant's acceptance wait out
        // another tenant's applies.
        match self.sender.try_send(request) {
            Ok(()) => (),
            Err(tokio::sync::mpsc::error::TrySendError::Full(request)) => {
                tracing::error!(
                    run_id = %request.operation_id,
                    "bss-pricing: the repricing apply lane is full; this run stays committing \
                     with its journal rows pending, so `POST \
                     /bss-pricing/v1/repricing-runs/{{runId}}/abort` is what ends it"
                );
            }
            Err(tokio::sync::mpsc::error::TrySendError::Closed(request)) => {
                tracing::error!(
                    run_id = %request.operation_id,
                    "bss-pricing: the repricing apply lane has no worker left to apply this run; \
                     it stays committing with its journal rows pending, so `POST \
                     /bss-pricing/v1/repricing-runs/{{runId}}/abort` is what ends it"
                );
            }
        }
    }
}

impl RunApplyWorker {
    /// Apply accepted runs until `cancel` fires, then stop.
    ///
    /// **Nothing is drained after cancellation, and that is the deliberate
    /// difference from [`RunCompensationWorker::run`].** A queued release is one
    /// `DELETE`; a queued apply is a per-plan commit over however many rows the run
    /// selected, so a post-cancellation drain here would be unbounded work inside
    /// the gear's `stop_timeout`. A run still queued stands at `committing` with its
    /// journal rows `pending` — the state that tells "never reached" from "decided"
    /// and the state `POST …/repricing-runs/{runId}/abort` reaches.
    ///
    /// **A run in flight at cancellation has its apply dropped, not awaited.**
    /// Awaiting it would let one large run hold the shutdown open until the runtime
    /// aborts this task anyway, and an abort at an arbitrary await point is the same
    /// drop with less said about it. The drop is a path [`RunLockGuard`] already
    /// owns: it hands the run's bulk-lock release to [`RunCompensation`] and leaves
    /// the run `committing`. If that lane's own worker has already finished its
    /// post-cancellation drain, the release is lost and the run's locks stand until
    /// the abort route clears them, which [`abandon_committing_run`] does in the same
    /// sweep that decides its rows.
    pub async fn run(mut self, cancel: CancellationToken) {
        loop {
            let request = tokio::select! {
                () = cancel.cancelled() => return,
                request = self.receiver.recv() => match request {
                    Some(request) => request,
                    None => return,
                },
            };
            let operation_id = request.operation_id;
            tokio::select! {
                () = cancel.cancelled() => {
                    tracing::warn!(
                        run_id = %operation_id,
                        "bss-pricing: shutdown dropped a repricing apply in flight; the run stays \
                         committing, so `POST /bss-pricing/v1/repricing-runs/{{runId}}/abort` is \
                         what ends it"
                    );
                    return;
                }
                () = self.apply(request) => (),
            }
        }
    }

    /// Apply every run the lane holds **right now**, and answer how many.
    ///
    /// [`RunCompensationWorker::drain_pending`]'s shape and its reason: a seam a
    /// caller can drive to a fixed point without a clock. It takes nothing new from
    /// the channel once it has caught up, so it terminates on a queue that is still
    /// being written to.
    ///
    /// The count is what a caller learns from: a drain that applied nothing is a
    /// surface that accepted a run and enqueued none, which is a different defect
    /// from an apply that ran and failed.
    ///
    /// **[`Self::run`] is what production spawns; this is what a process with no
    /// `bss-pricing` lifecycle drives** — every harness that composes the routers
    /// itself. Those suites therefore assert a run's terminal state directly instead
    /// of polling a deadline for it, which on a saturated box is a flake rather than
    /// a finding.
    pub async fn drain_pending(&mut self) -> usize {
        let mut handled = 0;
        while let Ok(request) = self.receiver.try_recv() {
            self.apply(request).await;
            handled += 1;
        }
        handled
    }

    /// Apply one accepted run, saying so when it fails.
    ///
    /// A failure is logged and not retried: the run stays `committing`, which the
    /// abort route and a redrive both reach, so the remedy survives it. Retrying
    /// here would be a second scheduler inside a worker whose queue already is the
    /// schedule.
    async fn apply(&self, request: RunApplyRequest) {
        if let Err(err) = apply_run_in(
            &self.db,
            &self.policies,
            &self.registry,
            &request.ctx,
            &request.scope,
            request.tenant_id,
            request.operation_id,
            request.stamp,
            Some(&self.compensation),
        )
        .await
        {
            tracing::error!(
                error = %err,
                run_id = %request.operation_id,
                "bss-pricing: a repricing run's apply failed; the run stays committing, so `POST \
                 /bss-pricing/v1/repricing-runs/{{runId}}/abort` is the remedy and a redrive \
                 route is the capability still owed"
            );
        }
    }
}

/// Release one run's bulk lock on a task nothing owns.
///
/// The path [`RunCompensation`] exists to avoid, kept as the fallback for the one
/// process shape that has no lane: a runtime with no `bss-pricing` lifecycle
/// running, which in a real boot cannot happen — the gear declares `stateful` and
/// `serve` is its entry — and which is every unit and integration test that calls
/// [`apply_run_in`] directly. Losing the release outright in those processes would
/// be worse than an unsupervised one, and the `warn` is what stops the fallback
/// from being mistaken for the supervised path.
///
/// **Nothing can observe when this has finished**, which is the property that makes
/// it the fallback and not the design. The task is unowned: there is no handle to
/// join, so a test asserting its effect can only poll a deadline, and a saturated
/// box turns that deadline into a flake rather than into a finding.
/// [`RunCompensationWorker::drain_pending`] is the observable path, and it is why
/// the suites that assert the release drive the lane instead of this.
fn spawn_detached_release(db: &DBProvider<DbError>, request: RunLockRelease) {
    // No runtime to spawn onto — a bare panic during shutdown, say — is past what
    // any of this can promise. Logged and left rather than panicking a second time
    // out of a `Drop`, which would abort the process outright.
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        tracing::error!(
            run_id = %request.operation_id,
            "bss-pricing: repricing apply dropped with its bulk lock still held, no compensation \
             lane to hand it to and no Tokio runtime current to release it on; the lock and the \
             run's committing state are both stuck until an operator aborts the run"
        );
        return;
    };
    let db = db.clone();
    handle.spawn(async move {
        let conn = match db.conn() {
            Ok(conn) => conn,
            Err(e) => {
                tracing::error!(
                    error = %e,
                    run_id = %request.operation_id,
                    "bss-pricing: repricing apply drop-guard could not open a connection to \
                     release its bulk lock"
                );
                return;
            }
        };
        if let Err(e) = bulk_repo::release_locks(
            &conn,
            &request.scope,
            request.tenant_id,
            request.operation_id,
        )
        .await
        {
            tracing::error!(
                error = %e,
                run_id = %request.operation_id,
                "bss-pricing: repricing apply drop-guard failed to release its bulk lock; the run \
                 is still committing, so the abort route is the remedy"
            );
        }
    });
}

/// Owns [`apply_run_in`]'s bulk lock across its two abnormal exits that no
/// match arm can reach: a panic, and a dropped future (a client disconnect, a
/// shutdown signal, a losing `select!` arm).
///
/// `infra::bulk`'s own sibling (`commit_batch`) releases the lock in its
/// `Ok`/`Err` match arms alone — review findings Z8-8/Z9-5 name the gap that
/// leaves: a panic unwinds *past* a match arm exactly as it unwinds past
/// everything else, and a dropped future never runs any of this crate's code
/// again at all, match arm or not. [`Drop`] is the one thing the language
/// itself guarantees runs on all three exits (a normal return, a panic, and a
/// cancellation), which is why this type exists rather than a fourth attempt
/// to enumerate exits by hand.
///
/// [`apply_run_in`]'s own explicit tail [`disarm`](Self::disarm)s this guard
/// on two of its four outcomes: a clean finish, where [`finish_run`] has
/// released the lock and landed the run terminal; and an ordinary
/// [`DomainError`] whose own, lighter release ([`bulk_repo::release_locks`]
/// alone — see the module doc's redrive-contract paragraph for why it stops
/// there) succeeded. It stays armed on the other two: a panic or a
/// cancellation, where no code of this function's own is left running to
/// reach either disarm site at all; and the rare case where even that
/// ordinary-`Err` release itself failed.
///
/// # The fallback releases the lock and lands nothing
///
/// It does the one thing that cannot wait for an operator — release the lock, so
/// interactive authoring on the run's rows is not frozen while the run waits to be
/// aborted or redriven — and stops there. Running [`finish_run`]'s force-terminal
/// sweep from here instead (mark every still-`pending` row `failed`, tally, advance
/// the run out of `committing`) costs those rows the one state that tells "never
/// reached" from "decided", on nothing better than a dropped future; the door that
/// spends that state (`POST …/repricing-runs/{runId}/abort`,
/// [`abandon_committing_run`]) is an operator's own decision, and it is the reason
/// a `pending` row under a run no caller will drive forward again is not
/// unreachable.
///
/// What that keeps is [`crate::domain::bulk::JournalState`]'s "an aborted run
/// leaves `pending` rows standing" — the property that lets a second call tell
/// "never reached" from "decided" — on every exit instead of all but two.
///
/// # What the fallback cannot promise
///
/// The release leaves this thread either way, because [`Drop::drop`] cannot
/// `.await`. With a [`RunCompensation`] handle it goes to a task the lifecycle owns
/// and joins; without one it goes to [`spawn_detached_release`]. Both are
/// best-effort rather than synchronous with the drop, so a caller that reads the
/// run back immediately afterwards may still see its locks held for a beat. And a
/// process killed outright runs neither: that residue is what the abort route is
/// for, which is why the route is deliberately retryable.
///
/// **It clears the lock rows the store holds when it runs, which is not always the
/// run's whole set.** [`bulk_repo::take_locks`] writes one independent statement per
/// row, and cancelling a future cancels the *await*, not a statement the driver has
/// already been handed — so a cancellation landing inside that loop can leave one
/// insert to complete **after** this release's `DELETE`. Measured, not reasoned
/// about: a saturated box reproduced a release that deleted one row of a two-row run
/// and a row of that same run standing afterwards.
///
/// The row is not stranded — it is held by a run that is still `committing`, so
/// [`abandon_committing_run`] clears it and a redrive's `take_locks` retakes it
/// (`holder == Some(operation_id)` is that function's success path). What it costs
/// until then is an interactive edit on that one price row.
///
/// **A retry loop here would cost more than it buys.** It would have to delete
/// again after re-reading, and the lock it deleted might be one a redrive had just
/// legitimately retaken — turning the idempotence `take_locks` was given for D-37's
/// redrive into a race against a sweeper. The bounded, already-built remedy is the
/// abort door.
struct RunLockGuard {
    db: DBProvider<DbError>,
    scope: AccessScope,
    tenant_id: Uuid,
    operation_id: Uuid,
    compensation: Option<RunCompensation>,
    disarmed: bool,
}

impl RunLockGuard {
    fn new(
        db: DBProvider<DbError>,
        scope: AccessScope,
        tenant_id: Uuid,
        operation_id: Uuid,
        compensation: Option<RunCompensation>,
    ) -> Self {
        Self {
            db,
            scope,
            tenant_id,
            operation_id,
            compensation,
            disarmed: false,
        }
    }

    /// Tell the guard its own caller already released the lock, so its `Drop` has
    /// nothing left to do. Takes `&mut self` rather than consuming the guard so a
    /// caller can disarm it and then let the ordinary end of scope drop it, rather
    /// than having to name a no-op `Drop` path explicitly at every return point.
    fn disarm(&mut self) {
        self.disarmed = true;
    }
}

impl Drop for RunLockGuard {
    fn drop(&mut self) {
        if self.disarmed {
            return;
        }
        let request = RunLockRelease {
            scope: self.scope.clone(),
            tenant_id: self.tenant_id,
            operation_id: self.operation_id,
        };
        let Some(compensation) = &self.compensation else {
            spawn_detached_release(&self.db, request);
            return;
        };
        // `try_send` and not `send`: a `Drop` cannot await, which is the whole
        // reason this lane exists. A full or closed lane falls back rather than
        // dropping the release, because the lock outlives this process either way.
        match compensation.sender.try_send(request) {
            Ok(()) => (),
            Err(tokio::sync::mpsc::error::TrySendError::Full(request)) => {
                tracing::warn!(
                    run_id = %request.operation_id,
                    "bss-pricing: the repricing compensation lane is full; this dropped apply's \
                     lock release falls back to an unsupervised task"
                );
                spawn_detached_release(&self.db, request);
            }
            Err(tokio::sync::mpsc::error::TrySendError::Closed(request)) => {
                tracing::warn!(
                    run_id = %request.operation_id,
                    "bss-pricing: the repricing compensation lane is closed; this dropped apply's \
                     lock release falls back to an unsupervised task"
                );
                spawn_detached_release(&self.db, request);
            }
        }
    }
}

/// The one shared reason a plan's every pending row is marked `failed` with.
///
/// [`DomainError`]'s own `Display` is deliberately terse on
/// [`DomainError::ValidationFailed`] — "N blocking violation(s)", because a
/// route rendering the full report has the structured envelope to put it in
/// and a log line repeating it per rule would be noise. A journal's
/// `failure_reason` is that structured envelope's only substitute here — a
/// plan-transactional refusal has no response body of its own to carry the
/// report in — so this renders every violation's code and detail rather than
/// the bare count `{err}` would give an operator nothing to act on from.
fn failure_reason(err: &DomainError) -> String {
    let DomainError::ValidationFailed(report) = err else {
        return err.to_string();
    };
    let mut rendered = format!("{err}: ");
    let details: Vec<String> = report
        .violations
        .iter()
        .map(|violation| {
            format!(
                "{} ({}): {}",
                violation.code, violation.subject, violation.detail
            )
        })
        .collect();
    rendered.push_str(&details.join("; "));
    rendered
}

/// One row this plan's transaction committed, carrying what the outbox events
/// enqueued after the aggregate pass need — collected in [`apply_rows_in`]
/// because neither event can be built until the plan's one `CatalogVersion`
/// request has answered.
struct AppliedRow {
    predecessor_price_id: Uuid,
    successor_price_id: Uuid,
    scope_key: ScopeKey,
    scheduled_window: crate::infra::storage::repo::WindowRecord,
}

/// One plan's whole commit, inside the caller's transaction — [`apply_run_in`]'s
/// per-plan unit, and the body D-134 makes normative.
///
/// See the module doc for the order and the reasons behind it. Split into the
/// two halves the module doc already distinguishes — the row loop
/// ([`apply_rows_in`]) and the once-per-plan aggregate pass
/// ([`commit_plan_aggregate_in`]) — **for `clippy::too_many_lines` alone**.
/// Both take the same `txn: &DbTx<'_>` this function does and neither opens
/// or commits anything of its own, so the one property this whole task turns
/// on — every write for a plan lands in one transaction, and the aggregate
/// pass runs after the row loop on that same transaction — does not move by
/// being split across two function bodies that still share this one call
/// stack. A `?` anywhere in either rolls every write above it back together
/// with the caller's own transaction wrapper.
#[allow(
    clippy::too_many_arguments,
    reason = "every argument is a precondition apply_run_in already resolved once for the whole \
              run — the runner and its collaborators, the tenant and the run's own identity, the \
              plan and its rows, the adjustment and changeover the report carried, and the stamp \
              — and this function is the one place they are all needed together, before the split \
              below narrows each half's own share of them. Bundling them would name a struct with \
              exactly one reader"
)]
async fn apply_plan_in(
    txn: &DbTx<'_>,
    policies: &PolicyObjectRepo,
    registry: &Arc<dyn CatalogVersionRegistryV1>,
    ctx: &SecurityContext,
    scope: &AccessScope,
    tenant_id: Uuid,
    operation_id: Uuid,
    plan_id: PlanId,
    rows: &[JournalRow],
    adjustment: &Adjustment,
    changeover: OffsetDateTime,
    stamp: AuditStamp,
) -> Result<(), DomainError> {
    // **Re-asked here, inside the transaction that writes.** The run-level call of
    // this same guard runs on the autocommit connection before `take_locks`, so a
    // unit opened over one of this plan's keys after that point was invisible to
    // the transaction that then supersedes those rows — and the bulk lock does not
    // cover the approval register. D-176 is the rule the three sibling doors state
    // in as many words: a comparison made before the transaction opened is a hint
    // and not a precondition.
    let plan_targets: Vec<Uuid> = rows.iter().map(|row| row.price_id).collect();
    refuse_targets_on_a_held_key(txn, scope, tenant_id, &plan_targets).await?;

    // Boxed for `clippy::large_futures`, `infra::cutover::cut_over`'s own
    // precedent: the row loop's future is large (a whole plan's writes live
    // across its awaits).
    let applied_rows = Box::pin(apply_rows_in(
        txn,
        scope,
        tenant_id,
        operation_id,
        plan_id,
        rows,
        adjustment,
        changeover,
        stamp,
    ))
    .await?;

    // Boxed for the identical reason — the aggregate pass's own future
    // carries a whole `PlanShape` across its awaits.
    Box::pin(commit_plan_aggregate_in(
        txn,
        policies,
        registry,
        ctx,
        scope,
        tenant_id,
        operation_id,
        plan_id,
        changeover,
        stamp,
        &applied_rows,
    ))
    .await
}

/// The row loop half of [`apply_plan_in`] — [`plan_supersession`] plus
/// [`commit_supersession`] for every pending row of the plan, in `txn`.
///
/// Returns what [`commit_plan_aggregate_in`] needs to finish: the successors
/// it cannot enqueue outbox events for until the plan's one `CatalogVersion`
/// request has answered.
#[allow(
    clippy::too_many_arguments,
    reason = "apply_plan_in's own reason, split rather than solved: every argument here is a \
              precondition apply_run_in already resolved once for the whole run, cut to only \
              what the row loop itself reads — the runner and scope, the tenant and the run's \
              own identity, the plan and its rows, the adjustment and changeover the report \
              carried, and the stamp. Bundling them would name a struct with exactly one reader"
)]
async fn apply_rows_in(
    txn: &DbTx<'_>,
    scope: &AccessScope,
    tenant_id: Uuid,
    operation_id: Uuid,
    plan_id: PlanId,
    rows: &[JournalRow],
    adjustment: &Adjustment,
    changeover: OffsetDateTime,
    stamp: AuditStamp,
) -> Result<Vec<AppliedRow>, DomainError> {
    let now = stamp.recorded_at;

    // Every published row of the plan, read **fresh in this transaction** —
    // the predecessor of every row this plan's part of the run selected has
    // to be found here, not carried from `apply_run_in`'s own read minutes
    // earlier and a world away.
    let mut published_by_id: HashMap<_, _> =
        price_repo::load_for_plan(txn, scope, tenant_id, plan_id, &[LifecycleState::Published])
            .await
            .map_err(|e| repo_failure(&e))?
            .into_iter()
            .map(|record| (record.price_id, record))
            .collect();

    let stored_windows = window_repo::list_for_plan(txn, scope, tenant_id, plan_id)
        .await
        .map_err(|e| repo_failure(&e))?;

    let mut applied_rows: Vec<AppliedRow> = Vec::with_capacity(rows.len());
    for row in rows {
        let predecessor = published_by_id.remove(&row.price_id).ok_or_else(|| {
            DomainError::LifecycleForbidden(format!(
                "price {} is no longer the published occupant of its canonical scope key; a \
                 concurrent change moved it after this run selected it, and it cannot be \
                 superseded by this repricing run",
                row.price_id
            ))
        })?;
        // `inst-mp-grandfathered` clause 2: a selector that names the eligibility
        // axis outright still expands over `existing_grandfathered` rows and
        // freezes them into the journal `pending` (`RunSelector::admits_grandfathered`,
        // `domain::repricing`'s own module doc) — dropping them here would be the
        // silent skip the clause forbids, and a journal row cannot be *born*
        // `failed` (D-261). So this is the only place the clause's own words —
        // "an explicit attempt to include one fails **that row** with a per-row
        // validation error" — can be honoured. `price_repo::refuse_unsupersedable_class`
        // is the one floor `infra::cutover` and `infra::supersession` already read for
        // the identical class, reused here rather than a second spelling of "is this
        // row immutable" — but reached *before* `insert_successor_draft_on` would hit
        // it, and refused **per row** rather than let it propagate: an `Err` here
        // would roll back this whole plan's transaction and fail every one of its
        // other rows too (`adjusts_rate`'s shape below), which is not what "fails
        // that row" says. The class is immutable regardless of what this run's
        // adjustment would otherwise compute, so this check runs before
        // `adjusts_rate`'s and independently of the row's `model_kind`.
        if let Err(err) = price_repo::refuse_unsupersedable_class(&predecessor.scope_key) {
            repricing_journal_repo::mark_failed(
                txn,
                scope,
                tenant_id,
                operation_id,
                row.price_id,
                &format!("inst-mp-grandfathered: {err}"),
            )
            .await
            .map_err(|e| repo_failure(&e))?;
            continue;
        }

        // D-311: a rate is a multiplier, not an amount, and only a percentage
        // adjustment is well-defined on one (`domain::repricing::project_rate`'s
        // own doc). An `amount` markup/discount or a `fixed` line on a row
        // whose money is a rate is refused here rather than silently applying
        // nothing to it while the journal still reports the row `applied` —
        // `domain::repricing::adjusts_rate` is the one predicate both this
        // refusal and `project_rate` read.
        //
        // **Three kinds, not two.** `per_unit` prices from `unit_rate` and not
        // from `amount_minor` after D-311 — `rules::model_kind::check_amount_placement`
        // is the matrix, and it makes `amount_minor` NULL by rule on such a
        // row — so a `per_unit` row under an amount/fixed adjustment is the
        // identical silent-no-op this guard exists for. It was omitted while
        // `project_row` still routed `per_unit` through `flat`'s arm, where
        // the omission was invisible because *no* adjustment moved such a row.
        //
        // **The failure unit here is the plan, and that is the design set's
        // choice rather than this function's.** `inst-mr-apply` (S12 §5, D-134,
        // and the Mass Repricing DoD's own MUST in §10): *"The transaction unit
        // is the plan, not the row — a run commits all of one plan's selected
        // rows together, and a per-row validation failure fails **every** row
        // of that plan with the shared reason — never a partial plan."* So an
        // `Err` is right here, and the per-row `mark_failed`-and-`continue`
        // above is **not** the shape to copy: `inst-mp-grandfathered` names its
        // own unit in as many words (*"fails **that row**"*), and the general
        // rule governs every case the design set does not carve out. D-134's
        // reason is the plan-level aggregate pass below, which runs over the
        // plan's row set *as it will stand post-commit* — a partial plan is a
        // set that pass never evaluated.
        //
        // Which makes the **rendering** load-bearing: `apply_by_plan` stamps
        // `failure_reason`'s one string onto every row of the plan, so a
        // message opening `price <id>:` reads, on a `flat` row's journal entry,
        // as the false claim that *that* row holds a rate. The refusing row is
        // named as the trigger and the plan is named as the unit, so an entry
        // on a row the run could have repriced says why it did not.
        // **The second way a rate-priced row can have no answer**,
        // and it is not the one above. `adjusts_rate` is a predicate over the
        // *adjustment* — under a `percent_bp` markup it answers `true`
        // unconditionally — so it cannot see that the arithmetic left `i64` on one
        // band of a ladder and not another. `project_row`'s loop then wrote the
        // bands that fit and left the rest at their published rate, and the
        // successor committed with a ladder nobody authored: cheaper at the top
        // than at the bottom, marked `applied`, and approved over the same partial
        // move because `run_materiality` runs the identical projection.
        // `magnitude_out_of_range` does not close it — it bounds a *discount* at
        // 10 000 bp and leaves a markup unbounded above zero.
        //
        // The plan is the unit here for D-134's reason, exactly as below.
        if crate::domain::repricing::projection_out_of_range(&predecessor, adjustment) {
            return Err(DomainError::InvalidRequest(format!(
                "inst-mr-apply (D-134): this plan's whole selected row set failed together and \
                 none of it applied — the run's transaction unit is the plan, not the row. The \
                 row that refused is price {}: at least one of its rates leaves the representable \
                 range under this adjustment, and a run that wrote the rates that fit would \
                 commit a ladder nobody authored — one band moved, its sibling left at its \
                 published rate. Re-run with a smaller magnitude, or under a selector that \
                 excludes this row",
                row.price_id
            )));
        }
        // **The third way a selected row can have no answer** — the amount-priced
        // kinds. `projection_out_of_range` above answers `false` for `flat` and
        // `package` because neither holds a rate, and `adjusts_rate` below gates
        // only the rate-priced kinds, so between them nothing asked whether an
        // `AmountSet` adjustment names this row's currency at all. It need not:
        // `project_amount` returns `None` on `set.get(currency)?`, `project_row`
        // then leaves the published amount where it was, and the run supersedes the
        // row with a byte-identical successor, journals it `applied` and mints a
        // CatalogVersion — telling the operator the money moved when it did not.
        //
        // The plan is the unit here for D-134's reason, exactly as above.
        if crate::domain::repricing::amount_projection_unresolved(&predecessor, adjustment) {
            return Err(DomainError::InvalidRequest(format!(
                "inst-mr-apply (D-134): this plan's whole selected row set failed together and \
                 none of it applied — the run's transaction unit is the plan, not the row. The \
                 row that refused is price {}: this adjustment resolves no amount in that row's \
                 currency, and a run that left it published unchanged would report the row \
                 repriced while its money did not move. Re-run with an amount for that currency, \
                 or under a selector that excludes this row",
                row.price_id
            )));
        }
        if matches!(
            predecessor.row.model_kind,
            Some(ModelKind::PerUnit | ModelKind::Graduated | ModelKind::Volume)
        ) && !adjusts_rate(adjustment)
        {
            return Err(DomainError::InvalidRequest(format!(
                "inst-mr-apply (D-134): this plan's whole selected row set failed together and \
                 none of it applied — the run's transaction unit is the plan, not the row, so a \
                 row of it this run cannot reprice fails the rest with it. The row that refused \
                 is price {}: a per_unit row's unit rate and a graduated/volume row's tier bands \
                 hold a rate, not an amount, and only a percent_bp markup/discount is a \
                 well-defined mutation of one; an amount markup/discount has no minor-unit floor \
                 to apply to a rate, and a fixed line reads a currency amount as a rate — which \
                 would collapse a ladder to one rate and can only ever author a whole-minor-unit \
                 one — so this run refuses rather than guess. Re-run with a percent_bp \
                 markup/discount, or under a selector that excludes the rate-priced rows",
                row.price_id
            )));
        }

        let key = predecessor.scope_key.clone();
        let plane: Vec<NamedWindow> = stored_windows
            .iter()
            .filter(|window| window.scope_key == key)
            .map(|window| NamedWindow {
                window_id: window.window_id,
                interval: WindowInterval::new(
                    window.effective_from,
                    window.effective_to,
                    window.state,
                ),
            })
            .collect();

        // The row-local rule set already exists as one call
        // (`domain::supersession::plan_supersession`): `price_row_rules()`,
        // `supersession_rules()` (D-82/D-98) and `compose_windows`'
        // overlap/gap/trailing-void check, over the successor built from the
        // run's own adjustment.
        let projected = project_row(predecessor.clone(), adjustment);
        let successor_content = price_repo::authored_content(
            &key,
            PriceContent {
                // `inst-mp-grandfathered`: the selector structurally excludes
                // the retained class, so a row this apply ever reaches never
                // carries a horizon to begin with — cleared explicitly rather
                // than trusted to already be absent, `supersede_in`'s own
                // reason for refusing one outright on this same field.
                grandfather_until: None,
                supersedes_price_id: Some(predecessor.price_id),
                ..projected.content()
            },
        );

        // `ChangeoverMoment::Commit`: the batching-delay floor
        // `api::rest::repricing_runs`'s module doc names as belonging to "a
        // commit that does not exist" — this transaction is that commit.
        let plan = plan_supersession(
            &predecessor.row,
            &successor_content.row,
            &plane,
            changeover,
            now,
            ChangeoverMoment::Commit,
        )?;

        let (successor, _) = Box::pin(price_repo::insert_successor_draft_on(
            txn,
            scope,
            tenant_id,
            NewPriceDraft {
                price_id: Uuid::now_v7(),
                scope_key: key.clone(),
                content: successor_content,
                created_by: stamp.actor_principal_id,
                created_at_utc: now,
                correlation_id: stamp.correlation_id,
            },
        ))
        .await
        .map_err(|e| repo_failure(&e))?;

        let shorten_target = stored_windows
            .iter()
            .find(|window| window.window_id == plan.windows.shorten.window_id)
            .ok_or_else(|| {
                // Unreachable by construction: `compose_windows` picked this id
                // off the very plane just built from `stored_windows`.
                DomainError::Internal(format!(
                    "bss-pricing: repricing apply composed shorten names window {} which is not \
                     on the plane it was composed from",
                    plan.windows.shorten.window_id
                ))
            })?;

        let written = commit_supersession(
            txn,
            scope,
            tenant_id,
            SupersessionCommit::of_plan(
                &plan,
                plan_id,
                predecessor.price_id,
                shorten_target.mutation_seq,
                (successor.price_id, successor.row_version),
                Uuid::now_v7(),
                format!("mass repricing run {operation_id}"),
            ),
            stamp,
        )
        .await
        .map_err(|e| repo_failure(&e))?;

        repricing_journal_repo::mark_applied(
            txn,
            scope,
            tenant_id,
            operation_id,
            row.price_id,
            successor.price_id,
            now,
        )
        .await
        .map_err(|e| repo_failure(&e))?;

        // The audit row §10's own cost breakdown for a repricing run's per-row
        // commit names alongside the row, the outbox and the journal: one
        // record per row, on the plan's chain, in this same transaction
        // (D-14) — `supersede_in`'s identical record, minted here instead
        // because this act is not that one.
        audit_repo::append(
            txn,
            scope,
            NewAuditEntry {
                tenant_id,
                chain_id: audit_repo::plan_chain(plan_id),
                recorded_at: now,
                actor_principal_id: stamp.actor_principal_id,
                action: AuditAction::Publish,
                subject_kind: AuditSubjectKind::PriceUnit,
                subject_ref: supersession_unit_ref(plan_id, &key, changeover),
                before_state: Some(serde_json::json!({
                    "predecessorPriceId": predecessor.price_id,
                    "predecessorState": LifecycleState::Published.as_str(),
                    "successorPriceId": successor.price_id,
                    "successorState": LifecycleState::Draft.as_str(),
                    "scopeKey": key.to_string(),
                })),
                after_state: Some(serde_json::json!({
                    "predecessorPriceId": predecessor.price_id,
                    "predecessorState": LifecycleState::Superseded.as_str(),
                    "successorPriceId": successor.price_id,
                    "successorState": LifecycleState::Published.as_str(),
                    "scopeKey": key.to_string(),
                })),
                // `None`: this row's own approval, if the run was material, is
                // the run's — `AuditSubjectKind::BulkOperation`'s record, not a
                // per-row `price_unit` unit this run never opens.
                approval_ref: None,
                correlation_id: stamp.correlation_id,
            },
        )
        .await
        .map_err(|e| repo_failure(&e))?;

        applied_rows.push(AppliedRow {
            predecessor_price_id: predecessor.price_id,
            successor_price_id: successor.price_id,
            scope_key: key,
            scheduled_window: written.scheduled,
        });
    }

    Ok(applied_rows)
}

/// The plan-level half of [`apply_plan_in`] — the aggregate pass **once**,
/// over the plan's row set as [`apply_rows_in`]'s writes just left it, and —
/// only on success, **and only when that pass actually applied a row** — the
/// plan's one `CatalogVersion` request and the outbox events every applied row
/// owes.
#[allow(
    clippy::too_many_arguments,
    reason = "apply_plan_in's own reason, split rather than solved: every argument here is a \
              precondition apply_run_in already resolved once for the whole run, cut to only \
              what the aggregate half itself reads — the runner and its two collaborators (the \
              registry, the tenant policy reader), the security context the registry request \
              needs, the compiled scope and tenant, the run's own identity, the changeover, the \
              stamp, and what the row loop just wrote. Bundling them would name a struct with \
              exactly one reader"
)]
async fn commit_plan_aggregate_in(
    txn: &DbTx<'_>,
    policies: &PolicyObjectRepo,
    registry: &Arc<dyn CatalogVersionRegistryV1>,
    ctx: &SecurityContext,
    scope: &AccessScope,
    tenant_id: Uuid,
    operation_id: Uuid,
    plan_id: PlanId,
    changeover: OffsetDateTime,
    stamp: AuditStamp,
    applied_rows: &[AppliedRow],
) -> Result<(), DomainError> {
    let now = stamp.recorded_at;

    // **Once**, over the plan's row set as this transaction just left it —
    // Step 0's premise is what makes this a real post-commit read rather than
    // a re-statement of what was just asked to be true.
    let current = plan_repo::load_current(txn, scope, tenant_id, plan_id)
        .await
        .map_err(|e| repo_failure(&e))?
        .ok_or_else(|| DomainError::NotFound {
            subject: "current plan revision".to_owned(),
            id: plan_id.to_string(),
        })?;
    let revision_no = current.revision;
    let revision_lifecycle = current.lifecycle_state;
    let shape = assemble_from(txn, scope, tenant_id, plan_id, current, now).await?;
    let params = rule_params(policies, txn, scope, tenant_id, &shape).await?;
    let report = run_publish_rules(&shape, &params);
    if !report.is_publishable() {
        return Err(DomainError::ValidationFailed(report));
    }

    // A plan whose current revision can never be superseded takes no successor
    // row either — `PublishService::commit`'s hoisted refusal, `supersede_in`'s
    // step 1a, and now the cutover's, and this was the fourth path that
    // superseded a published row without asking.
    //
    // Reachable rather than theoretical: `plan_repo::retire_revision` flips only
    // the `plan` row, so a retired plan's price rows still read `published` and
    // `load_current` above answers `published` **or** `retired` without
    // distinguishing them. Nothing beneath asks either — `commit_supersession_rows`
    // runs `refuse_mispaired` / `supersede_row` / `publish_rows`, none of which
    // reads the plan's lifecycle, and `RepoError::NoSuccessorRevision` comes from
    // the revision-opening path this act never takes.
    //
    // Ahead of the registry request because the refusal is **permanent** (D-156):
    // a handle taken past it stands pending forever and trips
    // `pricing.catalogversion.commit_overdue` for a publish that can never happen.
    // It is deliberately **after** the aggregate report rather than at the top of
    // this function: an operator whose run touches a retired plan wants both
    // findings, and the report is the one they can act on.
    crate::infra::publish::refuse_unpublishable_predecessor(txn, scope, tenant_id, plan_id).await?;

    // **A plan this run wrote nothing to takes no version**. Reachable rather than
    // theoretical: the row loop refuses a row **per row** and `continue`s when
    // `price_repo::refuse_unsupersedable_class` rejects it, so a run over a plan whose selected
    // rows are all `existing_grandfathered` — a set the selector deliberately expands over
    // (`inst-mp-grandfathered` clause 2) — journals every one of them `failed` and applies
    // none. Everything below mints a `CatalogVersion`, records a pending ref the projector then
    // has to resolve and `pricing.catalogversion.commit_overdue` then has to watch, and
    // enqueues one event per applied row — of which there are none. A version whose delta is
    // identical to its predecessor's is a version nobody can act on.
    //
    // **After** both passes above and not at the top of the function, for the
    // reason each is placed where it is: the aggregate report is the finding an
    // operator can act on, and `refuse_unpublishable_predecessor` is a permanent
    // refusal an operator wants to hear whether or not this run's rows landed.
    // Both are read-only, so skipping the writes below is the whole change.
    if applied_rows.is_empty() {
        return Ok(());
    }

    // One `CatalogVersion` per **plan**, not per row (`PublishService::commit`'s
    // own keying, `SubjectRef::Plan`) — D-47's batching then coalesces across
    // plans and runs, which is `inst-mr-coalesce`'s ask.
    let pending = request_version_now(
        registry.as_ref(),
        ctx,
        &repricing_request_id(tenant_id, operation_id, plan_id),
    )
    .await?;
    catalog_version_ref_repo::record_pending(
        txn,
        scope,
        PendingVersionRow::for_subject(
            tenant_id,
            pending.pending_ref.clone(),
            &SubjectRef::Plan(plan_id.get()),
            Some(revision_no),
            Some(revision_lifecycle),
            now,
        ),
    )
    .await
    .map_err(|e| repo_failure(&e))?;

    // The two events every applied row owes, now that the pending ref they
    // both carry exists. `(run_id, price_id)` dedup keys, per the task-5
    // brief: keyed on the journal's own identity rather than on the freshly
    // minted successor/window ids, so a dedup key survives a hypothetical
    // retry that mints different ones for the same journal row.
    for applied in applied_rows {
        outbox_repo::enqueue(
            txn,
            scope,
            NewOutboxEvent {
                dedup_key: format!(
                    "repricing/{operation_id}/{}/price",
                    applied.predecessor_price_id
                ),
                ..NewOutboxEvent::price_updated(
                    tenant_id,
                    &PriceUpdatedPayload {
                        plan_id,
                        price_id: applied.successor_price_id,
                        scope_key: applied.scope_key.to_string(),
                        supersedes_price_id: applied.predecessor_price_id,
                        changeover,
                        pending_version_ref: pending.pending_ref.clone(),
                        correlation_id: stamp.correlation_id,
                    },
                    now,
                )
            },
        )
        .await
        .map_err(|e| repo_failure(&e))?;

        outbox_repo::enqueue(
            txn,
            scope,
            NewOutboxEvent {
                dedup_key: format!(
                    "repricing/{operation_id}/{}/window",
                    applied.predecessor_price_id
                ),
                ..NewOutboxEvent::price_window_mutation(
                    tenant_id,
                    WindowMutationEvent::Scheduled,
                    &PriceWindowTransitionPayload {
                        window_id: applied.scheduled_window.window_id,
                        plan_id,
                        price_id: applied.successor_price_id,
                        effective_from: applied.scheduled_window.effective_from,
                        effective_to: applied.scheduled_window.effective_to,
                        correlation_id: stamp.correlation_id,
                    },
                    now,
                    &format!("repricing/{operation_id}"),
                )
            },
        )
        .await
        .map_err(|e| repo_failure(&e))?;
    }

    Ok(())
}

/// The registry's idempotency handle for one plan's repricing-run commit.
///
/// Deterministic in `(tenant, run, plan)`, `unit_request_id`'s and
/// `publish_request_id`'s own reason: a commit refused after the request —
/// storage fault, a lost race — is retried under the **same** handle rather
/// than orphaning one, and two plans of one run (or the same plan across two
/// runs) never share a handle.
fn repricing_request_id(tenant_id: Uuid, operation_id: Uuid, plan_id: PlanId) -> String {
    format!("repricing-run/{tenant_id}/{operation_id}/{plan_id}")
}

#[cfg(test)]
mod ordinary_failure_release {
    //! **The Critical review found and this pins.** [`super::release_lock_after_ordinary_failure`]
    //! is what an ordinary `Err` out of `apply_run_in`'s own `inner` block reaches — see the
    //! module doc's redrive-contract paragraph — and its whole contract is *doing less* than
    //! [`super::finish_run`]: release the lock, and touch neither the run's own state nor its
    //! journal, so a run left `committing` with a row still `pending` stays exactly that
    //! ([`crate::domain::bulk::JournalState`]'s own doc names why: that is what lets a second
    //! call tell "never reached" from "decided").
    //!
    //! **What this test is, and what it is not.** It is a unit of the release itself: the
    //! function is called directly and the lock is gone afterwards. That the *arm* reaches it —
    //! that an ordinary `Err` out of `inner` releases and stops, rather than landing on
    //! [`super::finish_run`]'s force-terminal sweep — is a property of `apply_run_in` and is
    //! pinned where the run can be driven whole, by
    //! `sqlite_repricing_apply::an_ordinary_failure_leaves_the_run_committing_with_its_rows_pending`.
    //! Asserting the run's state and its journal here would prove nothing either way: this
    //! function's whole body is the release, so it could not move them however it were written.
    //!
    //! **The `Err` that probe forces is a lock collision, not a lost race.** Timing a competing
    //! write against `apply_run_in`'s in-flight processing of the same row — the technique
    //! `tests/sqlite_repricing_apply.rs`'s cancellation tests use for a different property —
    //! cannot produce this one: this crate's `sqlite::memory:` harness gives every `DBProvider`
    //! a single-connection pool (`libs/toolkit-db`), and `SQLite`'s single-writer semantics mean
    //! a transaction holds that one connection for its whole duration, so a second task's write
    //! lands only in the gaps *between* transactions and a lock-visibility poll gets its turn
    //! after the target transaction has already finished. A sibling run holding the lock before
    //! the call has no such window: the collision is one statement's answer.

    use sea_orm_migration::MigratorTrait;
    use toolkit_db::migration_runner::run_migrations_for_testing;
    use toolkit_db::secure::AccessScope;
    use toolkit_db::{ConnectOpts, DBProvider, DbError, connect_db};
    use uuid::Uuid;

    use tokio_util::sync::CancellationToken;

    use crate::domain::bulk::{BulkKind, BulkState};
    use crate::domain::instant::utc_ymd_hms;
    use crate::domain::money::{CurrencyCode, MinorAmount};
    use crate::domain::price_record::PriceContent;
    use crate::domain::price_row::{ModelKind, PriceRow};
    use crate::domain::scope_key::{
        ChargeKind, Cohort, PhaseId, PlanId, PriceEligibility, Region, ScopeKey,
    };
    use crate::infra::storage::migrations::Migrator;
    use crate::infra::storage::repo::repricing_journal_repo::NewJournalRow;
    use crate::infra::storage::repo::{
        NewBulkOperation, NewPlanDraft, NewPriceDraft, PlanRepo, PriceRepo, bulk_repo,
        repricing_journal_repo,
    };

    /// A `committing` repricing run with one `pending` journal row and its bulk lock
    /// held over that row - [`super::apply_run_in`]'s own preamble, replicated at the
    /// repo seam so a test can call one piece of the release machinery directly
    /// rather than through the whole apply.
    struct Fixture {
        provider: DBProvider<DbError>,
        scope: AccessScope,
        tenant_id: Uuid,
        operation_id: Uuid,
        price_id: Uuid,
    }

    async fn a_committing_run_holding_its_lock() -> Fixture {
        let db = connect_db("sqlite::memory:", ConnectOpts::default())
            .await
            .expect("connect in-memory sqlite");
        run_migrations_for_testing(&db, Migrator::migrations())
            .await
            .expect("run migrator");
        let provider = DBProvider::<DbError>::new(db);
        let conn = provider.conn().expect("conn");

        let tenant_id = Uuid::from_u128(0x7e);
        let scope = AccessScope::for_tenant(tenant_id);
        let plan_id = PlanId::new(Uuid::from_u128(0x9a));
        let now = utc_ymd_hms(2026, 8, 12, 0, 0, 0);

        // The plan, minimally - `step0_probe`'s own shape, since what is
        // under test here has nothing to do with the plan's own revision.
        PlanRepo::new(provider.clone())
            .create_draft(
                &scope,
                NewPlanDraft {
                    plan_id,
                    tenant_id,
                    created_by: Uuid::from_u128(0x1),
                    created_at_utc: now,
                    sku_id: None,
                    plan_tier: None,
                    plan_name: None,
                    billing_cycle: None,
                    frequency: None,
                    plan_tier_override: false,
                    purchase_min_qty: None,
                    purchase_max_qty: None,
                    invoice_grouping_key: None,
                    available_from: None,
                    available_to: None,
                    cloned_from: None,
                    correlation_id: Uuid::from_u128(0x2),
                },
            )
            .await
            .expect("create the plan's first draft");

        let price_id = Uuid::from_u128(0xb_00);
        let key = ScopeKey::new(
            plan_id,
            CurrencyCode::new("EUR").expect("three letters"),
            Region::new("eu").expect("non-blank"),
            PhaseId::new(Uuid::from_u128(0xc0)),
            PriceEligibility::AllSubscriptions,
            ChargeKind::Recurring,
            Cohort::None,
        )
        .expect("the class pairs with cohort none");
        let mut row = PriceRow::new(ChargeKind::Recurring, Some(ModelKind::Flat));
        row.amount_minor = Some(MinorAmount::new(1_000).expect("non-negative"));
        PriceRepo::new(provider.clone())
            .create_draft(
                &scope,
                tenant_id,
                NewPriceDraft {
                    price_id,
                    scope_key: key,
                    content: PriceContent {
                        row,
                        tax_inclusive: false,
                        tax_category_ref: None,
                        billing_timing: None,
                        proration_contract: None,
                        rounding_policy_ref: None,
                        grandfather_until: None,
                        supersedes_price_id: None,
                    },
                    created_by: Uuid::from_u128(0x1),
                    created_at_utc: now,
                    correlation_id: Uuid::from_u128(0x2),
                },
            )
            .await
            .expect("author the price row");

        // A committing repricing run, its journal row frozen `pending`, and
        // its bulk lock taken over that one row - `apply_run_in`'s own
        // preamble, replicated by hand at the repo seam so this test can
        // call the function under test directly rather than through the
        // whole apply.
        let operation_id = Uuid::now_v7();
        bulk_repo::open(
            &conn,
            &scope,
            NewBulkOperation {
                operation_id,
                tenant_id,
                kind: BulkKind::Repricing,
                client_key: operation_id.to_string(),
                // A fixture, so the digest is of the fixture's own request text:
                // nothing here replays a key, and an empty digest is reserved for
                // the runs that predate `pricing_bulk_operation`.
                request_hash: crate::infra::storage::repo::IdempotencyGate::payload_hash(
                    "repricing-fixture",
                ),
                report: serde_json::json!({}),
                submitted_by: Uuid::from_u128(0x1),
                submitted_at: now,
            },
        )
        .await
        .expect("open the run");
        repricing_journal_repo::open_rows(
            &conn,
            &scope,
            &[NewJournalRow {
                run_id: operation_id,
                price_id,
                tenant_id,
            }],
        )
        .await
        .expect("freeze the journal");
        bulk_repo::advance(
            &conn,
            &scope,
            tenant_id,
            operation_id,
            BulkState::Validating,
            BulkState::Committing,
            serde_json::json!({}),
            now,
        )
        .await
        .expect("enter committing");
        bulk_repo::take_locks(&conn, &scope, tenant_id, operation_id, &[price_id], now)
            .await
            .expect("take the lock");

        // Sanity: the lock is held before the call under test, so its
        // absence afterward is that call's own doing and not a fixture bug.
        assert_eq!(
            bulk_repo::lock_holder(&conn, &scope, tenant_id, price_id)
                .await
                .expect("read the lock"),
            Some(operation_id),
            "the fixture itself must hold the lock before the call under test"
        );

        Fixture {
            provider,
            scope,
            tenant_id,
            operation_id,
            price_id,
        }
    }

    #[tokio::test]
    async fn releases_the_lock() {
        let Fixture {
            provider,
            scope,
            tenant_id,
            operation_id,
            price_id,
        } = a_committing_run_holding_its_lock().await;
        let conn = provider.conn().expect("conn");

        super::release_lock_after_ordinary_failure(&conn, &scope, tenant_id, operation_id)
            .await
            .expect("release after an ordinary failure");

        assert_eq!(
            bulk_repo::lock_holder(&conn, &scope, tenant_id, price_id)
                .await
                .expect("read the lock"),
            None,
            "the lock is released"
        );
    }

    /// **The dropped apply's release is drained by a task the lifecycle owns**, and
    /// it stops when that lifecycle is cancelled.
    ///
    /// The finding this lane answers: `RunLockGuard`'s `Drop` spawned its
    /// compensation onto `Handle::try_current()` — no token, no registration — so a
    /// shutdown abandoned it mid-statement against a database the process was
    /// closing, the one state a `stop_timeout` cannot help with. Both halves are
    /// asserted, because either alone is satisfiable by something useless: a worker
    /// that releases but never stops hangs the shutdown, and one that stops but
    /// never releases is the frozen rows the lane exists to prevent.
    ///
    /// The queued entry is drained **after** cancellation on purpose — see
    /// [`super::RunCompensationWorker::run`] for why an entry dropped there freezes
    /// price rows with no sweeper to clear them — so the release is armed by
    /// cancelling the token and letting the worker return, which also proves the
    /// stop.
    #[tokio::test]
    async fn the_compensation_lane_releases_a_dropped_applys_lock_and_stops_when_cancelled() {
        let Fixture {
            provider,
            scope,
            tenant_id,
            operation_id,
            price_id,
        } = a_committing_run_holding_its_lock().await;
        let conn = provider.conn().expect("conn");

        let (lane, worker) = super::run_compensation_lane(provider.clone());
        let cancel = CancellationToken::new();
        let running = tokio::spawn(worker.run(cancel.clone()));

        // The guard's own send, driven through the guard rather than around it: a
        // test that pushed onto the channel by hand would prove the worker drains a
        // queue and say nothing about whether `Drop` ever fills it.
        let guard = super::RunLockGuard::new(
            provider.clone(),
            scope.clone(),
            tenant_id,
            operation_id,
            Some(lane),
        );
        drop(guard);

        cancel.cancel();
        // The stop, asserted by the join completing at all — a worker that ignored
        // its token would leave this pending and the test would time out rather than
        // fail on a value, which is the honest shape for "it never returned".
        running.await.expect("the drainer stops when cancelled");

        assert_eq!(
            bulk_repo::lock_holder(&conn, &scope, tenant_id, price_id)
                .await
                .expect("read the lock"),
            None,
            "the queued release ran on the lifecycle's own task"
        );
        let run = bulk_repo::read(&conn, &scope, tenant_id, operation_id)
            .await
            .expect("read the run")
            .expect("the run exists");
        assert_eq!(
            run.state,
            super::BulkState::Committing,
            "and it released the lock without landing the run - the abort door is what ends it: \
             {run:?}"
        );
    }
}

#[cfg(test)]
mod step0_probe {
    //! The premise the module's shape rests on: [`apply_run_in`] runs the
    //! plan-level aggregate pass *inside* the transaction that just wrote the
    //! plan's successor rows, over the state
    //! [`crate::infra::publish::assemble_from`] reads back. It works only if
    //! that read goes through the handed transaction and sees its uncommitted
    //! writes.
    //!
    //! **What this pins is the plumbing, not the isolation level.** The probe
    //! runs against `sqlite::memory:`, and read-your-own-writes inside one
    //! transaction is behaviour every SQL engine has — so a green run here says
    //! nothing about Postgres that Postgres was ever in doubt about. What it
    //! does say is that `assemble_from` reads through the `DbTx` it is given
    //! rather than off a fresh pooled connection: change the transaction runner
    //! so that it does not, and this reddens on any engine. That is the failure
    //! mode the module is exposed to, and the one worth a case.
    //!
    //! The engine-differential half — a failed statement aborting the whole
    //! transaction, which `SQLite` does not do — is
    //! `tests/postgres_repricing_apply.rs`', and its module doc says why it
    //! cannot live here.

    use sea_orm_migration::MigratorTrait;
    use toolkit_db::migration_runner::run_migrations_for_testing;
    use toolkit_db::secure::AccessScope;
    use toolkit_db::{ConnectOpts, DBProvider, DbError, connect_db};
    use uuid::Uuid;

    use crate::domain::error::DomainError;
    use crate::domain::instant::utc_ymd_hms;
    use crate::domain::money::{CurrencyCode, MinorAmount};
    use crate::domain::price_record::PriceContent;
    use crate::domain::price_row::{ModelKind, PriceRow};
    use crate::domain::scope_key::{
        ChargeKind, Cohort, PhaseId, PlanId, PriceEligibility, Region, ScopeKey,
    };
    use crate::infra::publish::assemble_from;
    use crate::infra::storage::migrations::Migrator;
    use crate::infra::storage::repo::{
        NewPlanDraft, NewPriceDraft, PlanRepo, plan_repo, price_repo,
    };
    use crate::infra::storage::repo_failure;

    #[tokio::test]
    async fn a_transaction_sees_its_own_uncommitted_price_row_through_assemble_from() {
        let db = connect_db("sqlite::memory:", ConnectOpts::default())
            .await
            .expect("connect in-memory sqlite");
        run_migrations_for_testing(&db, Migrator::migrations())
            .await
            .expect("run migrator");
        let provider = DBProvider::<DbError>::new(db);

        let tenant_id = Uuid::from_u128(0x7e);
        let scope = AccessScope::for_tenant(tenant_id);
        let plan_id = PlanId::new(Uuid::from_u128(0x9a));
        let now = utc_ymd_hms(2026, 8, 11, 0, 0, 0);

        // The plan and its first draft revision, committed ahead of the probe:
        // what is under test is the price row's visibility, not the plan's.
        // `PlanRepo::create_draft` opens its own transaction for the pair of
        // rows it writes (the revision and its audit record) — `create_draft_on`
        // is the runner-generic body every caller must supply a transaction to.
        PlanRepo::new(provider.clone())
            .create_draft(
                &scope,
                NewPlanDraft {
                    plan_id,
                    tenant_id,
                    created_by: Uuid::from_u128(0x1),
                    created_at_utc: now,
                    sku_id: None,
                    plan_tier: None,
                    plan_name: None,
                    billing_cycle: None,
                    frequency: None,
                    plan_tier_override: false,
                    purchase_min_qty: None,
                    purchase_max_qty: None,
                    invoice_grouping_key: None,
                    available_from: None,
                    available_to: None,
                    cloned_from: None,
                    correlation_id: Uuid::from_u128(0x2),
                },
            )
            .await
            .expect("create the plan's first draft");

        let price_id = Uuid::from_u128(0xb_00);
        let scope_for_tx = scope.clone();
        let (_, outcome) = provider
            .db()
            .in_transaction::<bool, DomainError, _>(move |txn| {
                let scope = scope_for_tx.clone();
                Box::pin(async move {
                    let key = ScopeKey::new(
                        plan_id,
                        CurrencyCode::new("EUR").expect("three letters"),
                        Region::new("eu").expect("non-blank"),
                        PhaseId::new(Uuid::from_u128(0xc0)),
                        PriceEligibility::AllSubscriptions,
                        ChargeKind::Recurring,
                        Cohort::None,
                    )
                    .expect("the class pairs with cohort none");
                    let mut row = PriceRow::new(ChargeKind::Recurring, Some(ModelKind::Flat));
                    row.amount_minor = Some(MinorAmount::new(1_000).expect("non-negative"));

                    // The write under test: **not yet committed** when the read below
                    // runs, because both are inside this one `txn`.
                    Box::pin(price_repo::create_draft_on(
                        txn,
                        &scope,
                        tenant_id,
                        NewPriceDraft {
                            price_id,
                            scope_key: key,
                            content: PriceContent {
                                row,
                                tax_inclusive: false,
                                tax_category_ref: None,
                                billing_timing: None,
                                proration_contract: None,
                                rounding_policy_ref: None,
                                grandfather_until: None,
                                supersedes_price_id: None,
                            },
                            created_by: Uuid::from_u128(0x1),
                            created_at_utc: now,
                            correlation_id: Uuid::from_u128(0x2),
                        },
                    ))
                    .await
                    .map_err(|e| repo_failure(&e))?;

                    // `load_open_draft`, not `load_current`: `load_current` answers
                    // only `published | retired` (`is_current_revision`), and this
                    // probe's plan was never published — it only needs a revision
                    // to assemble against, and the property under test is about the
                    // price row's visibility, not about which revision state reads
                    // it back.
                    let revision = plan_repo::load_open_draft(txn, &scope, tenant_id, plan_id)
                        .await
                        .map_err(|e| repo_failure(&e))?
                        .expect("the plan carries the draft revision just created");

                    // The read under test, in the same transaction as the write
                    // above and before either commits.
                    let shape =
                        assemble_from(txn, &scope, tenant_id, plan_id, revision, now).await?;

                    Ok(shape.rows.iter().any(|record| record.price_id == price_id))
                })
            })
            .await;

        let saw_own_write = outcome.expect("the transaction itself does not fail");
        assert!(
            saw_own_write,
            "a transaction must see its own uncommitted price-row insert through \
             assemble_from, or D-134's design - the aggregate pass over post-commit state, \
             inside the plan's own transaction - is unbuildable as written"
        );
    }
}
