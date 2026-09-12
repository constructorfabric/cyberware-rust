//! Bulk import, Phase 2 — the per-row optimistic commit
//! (`design/12-operator-efficiency.md` §3 `inst-bk-phase2`, §4 `inst-bi-commit`,
//! `inst-bk-lock`; D-141, D-260, D-262, D-290).
//!
//! # Every row is its own transaction, and that is the whole shape
//!
//! `inst-bk-phase2`: "each row commits under its own `ETag`; a conflict (concurrent
//! manual edit) fails **only that row**; committed rows stand". So this is the
//! opposite of the clone, which had to be one transaction: there the receipt was
//! worthless without every row, here a partial result is the **product**. The
//! repository methods open their own transactions and that is why they are used
//! rather than their runner-taking forms.
//!
//! The token is the price row's own version column (D-141). Until that decision
//! §3.7 carried an `ETag` on `pricing_plan` alone, under which a batch either
//! conflicts entirely or not at all and "fails only that row" has no referent.
//!
//! # The locks are taken after the run enters `committing`, because the store
//! says so
//!
//! `pricing_bulk_row_lock`'s trigger refuses a lock whose run is not
//! `committing`, and it is right: a lock held through Phase 1 would exclude every
//! interactive editor for the length of a *read*. The consequence is an ordering
//! this module cannot choose — advance first, then lock.
//!
//! # A run that cannot take its locks conflicts on every row
//!
//! If another run holds a row, `take_locks` refuses and this one is already
//! `committing` — from which §4's only edges are `completed` and
//! `completed_with_conflicts`. There is no failure state to reach, and inventing
//! one would be a state machine edit for an outcome that already has an honest
//! reading: **nothing committed, every row conflicted, the report lists them for
//! retry.** That is what `BULK_ROW_CONFLICT` means and what "committed rows
//! stand" says about a run where none did.
//!
//! **One refusal is not that, and it is why `take_locks` distinguishes it.** If
//! the refusal path could not clear the rows this run had already taken
//! ([`RepoError::BulkLocksHeld`]), an honest terminal landing does not exist: a
//! terminal run's locks are unreachable — [`abandon_committing_run`] refuses any
//! other state, `pricing_bulk_row_lock` has no sweeper, D-37's lease takeover is
//! unbuilt — so every row of the batch would be frozen by an operation that is
//! over. That run stays `committing`, which is where the remedy is.
//!
//! # Three ways to end, and only one of them runs a match arm
//!
//! [`commit_batch`]'s `Ok`/`Err` arms are the two this function's own code is
//! still running for, and they were the whole of the release for a long time
//! (D-294 took the `?` off `commit_rows`; D-300 took it off `release_locks`, which
//! review H9 put back — a release that did not happen is the one failure with no
//! honest terminal landing after it, so that path returns with the run still
//! `committing` and this guard still armed).
//! Review finding Z8-8 names what an arm-only release cannot reach:
//!
//! * **A panic** inside `commit_rows` unwinds past a match arm exactly as it
//!   unwinds past everything else.
//! * **A dropped future** — a client disconnect, a shutdown signal, a losing
//!   `select!` arm — stops at its next await point and never runs any of this
//!   crate's code again, arm or not.
//!
//! Neither is rolled back, and that is deliberate rather than accidental:
//! `bulk_repo::take_locks` **requires** an autocommit connection, because inside a
//! transaction Postgres degrades its refusal past the holder `fr-concurrent-edit`
//! needs named. So the locks outlive the future that took them, the rows stay
//! frozen against every interactive editor, and the run stays `committing` with no
//! timeout, no sweeper and D-37's lease takeover designed and unbuilt.
//!
//! [`Drop`] is the one thing the language itself guarantees on all three exits, so
//! the release is owned by a guard — `infra::repricing::RunLockGuard`'s
//! arrangement, whose own doc names this finding, and [`abandon_committing_run`]
//! is the sweep both it and the abort route perform, written once. What the
//! fallback cannot promise is the same there as here: `Drop` cannot `.await`, so
//! it spawns a detached task on whatever runtime is current, and a process killed
//! outright runs neither. That residue is the abort route's — which is why the
//! route stays.

use std::collections::HashMap;

use toolkit_db::secure::{AccessScope, DBRunner};
use toolkit_db::{DBProvider, DbError};
use uuid::Uuid;

use crate::domain::audit::AuditStamp;
use crate::domain::bulk::BulkState;
use crate::domain::error::DomainError;
use crate::domain::import::{BatchReport, ImportRow, RowOutcome, RowViolation};
use crate::domain::lifecycle::LifecycleState;
use crate::domain::price_record::PriceRecord;
use crate::domain::scope_key::{PlanId, ScopeKey};
use crate::infra::storage::repo::{
    BulkOperationRecord, NewPriceDraft, PriceRepo, bulk_repo, price_repo,
};
use crate::infra::storage::{RepoError, repo_failure};
use time::OffsetDateTime;

/// §5's per-row conflict code, reported in the operation report.
///
/// Declared by the design set and raised here for the first time: an `ETag` that no
/// longer matches, or a row another run holds. Both are the same fact for the
/// operator — somebody else moved it — and the remedy is the same: re-read and
/// resubmit the conflicted subset as a new import.
pub const BULK_ROW_CONFLICT: &str = "BULK_ROW_CONFLICT";

/// The report member [`abandon_committing_run`] stamps its note under.
///
/// One key, because an operator reading a run's report should not have to know
/// whether the sweep came from the abort route or from a dropped future to find
/// out that one happened.
pub const ABORTED_MEMBER: &str = "aborted";

/// Where a report that was not a JSON object is kept when the abort note is
/// stamped beside it.
///
/// Every current writer stores an object — [`report_of`] serializes a
/// `CommitReceipt`, and `commit_batch` writes that — so this is robustness rather
/// than a live path. It exists because the alternative was `as_object_mut()`
/// returning `None` and the note being **silently dropped**, and the note is the
/// only evidence the sweep ran at all: an operator reading the run then cannot
/// tell an abort from an ordinary completion. Refusing instead is not an option,
/// because this sweep *is* the remedy — a refusal would leave the run
/// `committing` with its locks already released.
pub const PRIOR_REPORT_MEMBER: &str = "priorReport";

/// The note `POST …/abort` stamps: an operator stopped a run that had stalled.
pub const ABORT_NOTE: &str = "uncommitted rows were not attempted";

/// The note [`CommitLockGuard`]'s fallback stamps.
///
/// It says what happened *and* what is owed, because the rows this run did commit
/// are in the store and are **not** in this report: the receipt that would have
/// listed them died with the future, and the column carries only what
/// [`commit_batch`] wrote on entry (how many rows the run was about). That is the
/// same limitation the abort route has had since D-300 and for the same reason —
/// there is nothing left running to build a receipt from.
///
/// The parenthetical names **three** exits, not two: since review H9 a
/// [`commit_batch`] whose `release_locks` failed also returns with this guard armed,
/// deliberately, because a run whose locks are still held has no honest terminal
/// state to land on. The rest of the sentence is true of that exit word for word —
/// the receipt exists but was never written to the report column, and the remedy is
/// the same re-read.
const INTERRUPTED_NOTE: &str = "the commit was interrupted before it reached its own terminal \
    move (a panic, a dropped future, or a release its own arm could not complete); its row locks \
    have been released and the run has been landed terminal, but the report does not list what \
    it had already committed - re-read the rows and resubmit whatever is still owed as a new \
    batch under a new idempotency key";

/// Release a `committing` run's row locks and land it terminal, stamping `note`
/// on the report it has already reached.
///
/// The one sweep with two callers — `POST …/abort` and [`CommitLockGuard`]'s
/// `Drop` fallback — written once rather than twice, because the ordering below is
/// load-bearing and a second spelling of it is a second place for the order to be
/// got wrong.
///
/// **The release goes first, and D-300 is why.** With the terminal move ahead of
/// it, a release that failed for any transient reason left the run terminal and
/// every lock held — and the abort route's own state guard then refuses the retry
/// that used to rescue it, because a terminal run is no longer `committing`. The
/// rows would be frozen by a finished operation with no remedy at all: nothing
/// else calls `release_locks`, the lock table has no sweeper, and D-37's lease
/// takeover is designed and unbuilt. Releasing first makes a failed sweep
/// **retryable** — the run is still `committing`, so the operator simply asks
/// again.
///
/// **The report is added to, never replaced**, so what the run committed survives
/// the note wherever a receipt did reach the column — including on the one shape
/// that has no member to add to, where the prior value moves under
/// [`PRIOR_REPORT_MEMBER`] rather than the note being dropped.
///
/// # Errors
/// [`RepoError::NotFound`] when no run in this scope answers to the id;
/// [`RepoError::ConcurrentMutation`] when the run is no longer `committing` — the
/// premise this sweep is only correct under, judged by the statement rather than
/// by a read in front of it; [`RepoError::Db`] on a scope or storage failure.
pub async fn abandon_committing_run(
    runner: &impl toolkit_db::secure::DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    operation_id: Uuid,
    note: &str,
    at: OffsetDateTime,
) -> Result<BulkOperationRecord, RepoError> {
    let run = bulk_repo::read(runner, scope, tenant_id, operation_id)
        .await?
        .ok_or_else(|| RepoError::NotFound {
            subject: "bulk import".to_owned(),
            id: operation_id.to_string(),
        })?;
    let mut report = run.report;
    if let Some(object) = report.as_object_mut() {
        object.insert(ABORTED_MEMBER.to_owned(), serde_json::json!(note));
    } else {
        // Not an object: keep what was there under [`PRIOR_REPORT_MEMBER`] rather
        // than dropping the note. "Added to, never replaced" still holds — the
        // prior value is carried, not discarded — and the run stops being
        // indistinguishable from one that completed normally.
        report = serde_json::json!({
            ABORTED_MEMBER: note,
            PRIOR_REPORT_MEMBER: report,
        });
    }

    bulk_repo::release_locks(runner, scope, tenant_id, operation_id).await?;
    bulk_repo::advance(
        runner,
        scope,
        tenant_id,
        operation_id,
        BulkState::Committing,
        BulkState::CompletedWithConflicts,
        report,
        at,
    )
    .await
}

/// The `validating` half of the abort door: land a run that never reached a
/// decision, so an operator has the same remedy one phase earlier.
///
/// [`abandon_committing_run`]'s sibling, and deliberately **not** that function
/// with a widened premise: this run holds no row locks, so there is nothing to
/// release, and its edge is `validating -> validation_failed` rather than the
/// terminal conflict state. Sharing one body would mean a `release_locks` on a run
/// that never took any and a state pair chosen by an `if` — which is how a sweep
/// starts writing the wrong `WHERE`.
///
/// The note goes under the same [`ABORTED_MEMBER`] key for that constant's stated
/// reason: an operator reading a run's report should not have to know which door
/// stopped it.
///
/// # Errors
/// [`RepoError::NotFound`] when the run is not visible to `scope`;
/// [`RepoError::Db`] on a storage failure or when the run has left `validating`
/// since it was read, which is the compare-and-swap refusing rather than a lost
/// update.
pub async fn abandon_validating_run(
    runner: &impl toolkit_db::secure::DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    operation_id: Uuid,
    note: &str,
    at: OffsetDateTime,
) -> Result<BulkOperationRecord, RepoError> {
    let run = bulk_repo::read(runner, scope, tenant_id, operation_id)
        .await?
        .ok_or_else(|| RepoError::NotFound {
            subject: "bulk import".to_owned(),
            id: operation_id.to_string(),
        })?;
    let mut report = run.report;
    if let Some(object) = report.as_object_mut() {
        object.insert(ABORTED_MEMBER.to_owned(), serde_json::json!(note));
    } else {
        report = serde_json::json!({
            ABORTED_MEMBER: note,
            PRIOR_REPORT_MEMBER: report,
        });
    }

    bulk_repo::advance(
        runner,
        scope,
        tenant_id,
        operation_id,
        BulkState::Validating,
        BulkState::ValidationFailed,
        report,
        at,
    )
    .await
}

/// The note `POST …/abort` stamps on a run it stopped during **validation**.
pub const ABORT_VALIDATING_NOTE: &str = "an operator stopped this batch during validation; nothing was imported and nothing was \
     locked - resubmit under a new idempotency key";

/// Owns [`commit_batch`]'s row locks across the two exits no match arm reaches: a
/// panic, and a dropped future.
///
/// See the module doc for why an `Ok`/`Err`-arm-only release cannot cover either,
/// and why the locks are not rolled back when it fails to. This is
/// `infra::repricing::RunLockGuard`'s arrangement — the two are siblings, and that
/// one's doc names this finding as the gap it was built against.
///
/// [`commit_batch`] [`disarm`](Self::disarm)s this the moment its own terminal
/// [`bulk_repo::advance`] lands, which is the last statement of the release-and-land
/// contract the module doc states. It stays armed for everything before that,
/// including a terminal move that itself failed — where the fallback is the only
/// thing left that will try again.
///
/// # What the fallback cannot promise
///
/// [`Drop::drop`] cannot `.await`, so it spawns a detached task on whatever Tokio
/// runtime is current — best-effort, not synchronous with the drop, so a caller
/// reading the run back immediately afterwards may still see it `committing` for a
/// beat. A runtime that has gone away, or a process killed outright, runs neither.
/// That residue is the abort route's, which is why the route stays and is
/// deliberately retryable.
struct CommitLockGuard {
    db: DBProvider<DbError>,
    scope: AccessScope,
    tenant_id: Uuid,
    operation_id: Uuid,
    disarmed: bool,
}

impl CommitLockGuard {
    const fn new(
        db: DBProvider<DbError>,
        scope: AccessScope,
        tenant_id: Uuid,
        operation_id: Uuid,
    ) -> Self {
        Self {
            db,
            scope,
            tenant_id,
            operation_id,
            disarmed: false,
        }
    }

    /// Tell the guard the run has already been released and landed, so its `Drop`
    /// has nothing to do. `&mut self` rather than consuming it, for
    /// `RunLockGuard::disarm`'s reason: a caller disarms and lets the ordinary end
    /// of scope drop it, rather than naming a no-op `Drop` at every return point.
    fn disarm(&mut self) {
        self.disarmed = true;
    }
}

impl Drop for CommitLockGuard {
    fn drop(&mut self) {
        if self.disarmed {
            return;
        }
        let db = self.db.clone();
        let scope = self.scope.clone();
        let tenant_id = self.tenant_id;
        let operation_id = self.operation_id;
        // No runtime to spawn onto — a panic during shutdown, say — is the gap
        // this guard's own doc names as past what it can promise. Logged and left
        // rather than panicking a second time out of a `Drop`, which would abort
        // the process outright.
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            tracing::error!(
                operation_id = %operation_id,
                "bss-pricing: a bulk commit was dropped with its row locks still held and no \
                 Tokio runtime current to release them on; the locks and the run's committing \
                 state are both stuck until an operator aborts the run"
            );
            return;
        };
        // **Announced before it is handed off, because the hand-off can be a
        // no-op.** `Handle::try_current` succeeds during runtime shutdown, so the
        // guard takes the success path, spawns, and the task is never polled --
        // and shutdown is one of the three exits this guard is documented to
        // cover. Without this line that exit is completely silent: the run stays
        // in a state nothing else can leave, its idempotency key spent, and the
        // only error branch is the `try_current` failure above, which did not
        // fire. Landing it on a supervised, cancellation-aware task is the real
        // repair and is owed; until then the pair of lines is what makes the
        // silence detectable -- an operator seeing this one without the
        // completion line below knows the recovery never ran.
        tracing::warn!(
            operation_id = %operation_id,
            "bss-pricing: recovering a bulk commit's row locks on a detached task; if no matching \
             completion line follows, the runtime shut down before it ran and the \
             run needs an operator"
        );
        handle.spawn(async move {
            let conn = match db.conn() {
                Ok(conn) => conn,
                Err(e) => {
                    tracing::error!(
                        error = %e,
                        operation_id = %operation_id,
                        "bss-pricing: a dropped bulk commit's guard could not open a connection \
                         to release its row locks"
                    );
                    return;
                }
            };
            if let Err(e) = abandon_committing_run(
                &conn,
                &scope,
                tenant_id,
                operation_id,
                INTERRUPTED_NOTE,
                OffsetDateTime::now_utc(),
            )
            .await
            {
                tracing::error!(
                    error = %e,
                    operation_id = %operation_id,
                    "bss-pricing: a dropped bulk commit's guard failed to release its row locks \
                     and land the run terminal; the run may still be committing and its rows \
                     frozen, and `POST /bulk-imports/{{id}}/abort` is the remedy"
                );
            } else {
                // The other half of the pair announced at the spawn. Its absence is
                // the signal: a shutdown that swallowed the task logs the warning
                // and never reaches here.
                tracing::info!(
                    operation_id = %operation_id,
                    "bss-pricing: the dropped bulk commit's row locks were released and the \
                     run landed terminal"
                );
            }
        });
    }
}

/// What a Phase-1 landing says when the future carrying it was dropped.
///
/// [`INTERRUPTED_NOTE`]'s sibling one phase earlier, and a different sentence
/// because a different thing happened: Phase 1 holds no locks and has committed
/// nothing, so the operator's remedy is simply a new batch under a new key. What
/// they must **not** be told is that the import succeeded, which is what the
/// replay arm answered while the run sat `validating`.
const VALIDATION_INTERRUPTED_NOTE: &str = "validation was interrupted (a client disconnect, a shutdown or a panic) before this batch \
     reached a decision; nothing was imported and nothing is locked - resubmit the batch under a \
     new idempotency key";

/// Owns a run that is `validating` and lands it `validation_failed` if the future
/// carrying Phase 1 never reaches a decision.
///
/// [`CommitLockGuard`]'s arrangement one phase earlier, and it exists for the same
/// reason its doc gives: a panic unwinds past every match arm and a dropped future
/// — "a client disconnect, a shutdown signal, a losing `select!` arm" — never
/// reaches one. Phase 2 was hardened for both three times (D-294, D-297, D-300)
/// and Phase 1 not once, although Phase 1 is the phase that awaits a store read
/// per plan while the client's own timeout is running.
///
/// **Nothing owned the run between `bulk_repo::open` and Phase 2's `advance`.**
/// `validating` is a state nothing else can leave: the abort route refuses
/// anything that is not `committing`, the lock table has no sweeper, and D-37's
/// lease takeover is unbuilt. So a stranded run held its idempotency key forever
/// and the retry — which is the same client, timed out — was answered `202` over
/// the placeholder report `{"rows": []}`. Z11-4 closed exactly that inversion for
/// a store fault inside `classify_against_store`; this closes it for the two exits
/// that run no code of ours.
///
/// **The `Drop` must not call [`abandon_committing_run`]**: that sweep's premise
/// is `committing` and rides its `WHERE`, so it would refuse this run and log a
/// failure rather than land it. It advances `Validating -> ValidationFailed`
/// instead, which is the same statement-level premise, so a Phase-1 landing that
/// raced the drop cannot be overwritten by it.
pub(crate) struct ValidationGuard {
    db: DBProvider<DbError>,
    scope: AccessScope,
    tenant_id: Uuid,
    operation_id: Uuid,
    disarmed: bool,
}

impl ValidationGuard {
    /// Arm the guard over a run that has just been opened `validating`.
    #[must_use]
    pub(crate) const fn new(
        db: DBProvider<DbError>,
        scope: AccessScope,
        tenant_id: Uuid,
        operation_id: Uuid,
    ) -> Self {
        Self {
            db,
            scope,
            tenant_id,
            operation_id,
            disarmed: false,
        }
    }

    /// The run has left `validating` under its own power, so `Drop` has nothing to
    /// do. [`CommitLockGuard::disarm`]'s shape, for that method's reason.
    pub(crate) fn disarm(&mut self) {
        self.disarmed = true;
    }
}

impl Drop for ValidationGuard {
    fn drop(&mut self) {
        if self.disarmed {
            return;
        }
        let db = self.db.clone();
        let scope = self.scope.clone();
        let tenant_id = self.tenant_id;
        let operation_id = self.operation_id;
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            tracing::error!(
                operation_id = %operation_id,
                "bss-pricing: a bulk import's validation was dropped with no Tokio runtime \
                 current to land the run on; it is left validating, which has no exit, and its \
                 idempotency key is spent"
            );
            return;
        };
        // **Announced before it is handed off, because the hand-off can be a
        // no-op.** `Handle::try_current` succeeds during runtime shutdown, so the
        // guard takes the success path, spawns, and the task is never polled --
        // and shutdown is one of the three exits this guard is documented to
        // cover. Without this line that exit is completely silent: the run stays
        // in a state nothing else can leave, its idempotency key spent, and the
        // only error branch is the `try_current` failure above, which did not
        // fire. Landing it on a supervised, cancellation-aware task is the real
        // repair and is owed; until then the pair of lines is what makes the
        // silence detectable -- an operator seeing this one without the
        // completion line below knows the recovery never ran.
        tracing::warn!(
            operation_id = %operation_id,
            "bss-pricing: recovering a bulk import's validation on a detached task; if no matching \
             completion line follows, the runtime shut down before it ran and the \
             run needs an operator"
        );
        handle.spawn(async move {
            let conn = match db.conn() {
                Ok(conn) => conn,
                Err(e) => {
                    tracing::error!(
                        error = %e,
                        operation_id = %operation_id,
                        "bss-pricing: a dropped bulk import's guard could not open a connection \
                         to land its run"
                    );
                    return;
                }
            };
            let report = serde_json::json!({
                "rows": [],
                "failure": VALIDATION_INTERRUPTED_NOTE,
            });
            if let Err(e) = bulk_repo::advance(
                &conn,
                &scope,
                tenant_id,
                operation_id,
                BulkState::Validating,
                BulkState::ValidationFailed,
                report,
                OffsetDateTime::now_utc(),
            )
            .await
            {
                tracing::error!(
                    error = %e,
                    operation_id = %operation_id,
                    "bss-pricing: a dropped bulk import's guard failed to land the run \
                     validation_failed; it is left validating, which has no exit, and its \
                     idempotency key is spent"
                );
            } else {
                // The other half of the pair announced at the spawn. Its absence is
                // the signal: a shutdown that swallowed the task logs the warning
                // and never reaches here.
                tracing::info!(
                    operation_id = %operation_id,
                    "bss-pricing: the dropped bulk import's run was landed validation_failed"
                );
            }
        });
    }
}

/// One row that landed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub struct CommittedRow {
    /// Its position in the submitted batch.
    pub row: usize,
    /// The draft row it wrote — minted here for a new key, the existing draft's
    /// id for an edit.
    pub price_id: Uuid,
}

/// What Phase 2 did.
///
/// `{committed, conflicted}` is `inst-bi-commit`'s own shape. The conflicted half
/// carries [`RowOutcome`]s rather than bare indices so the report a retry reads is
/// the report Phase 1 produces — one type, whichever phase filled it.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct CommitReceipt {
    /// Rows that landed, in batch order.
    pub committed: Vec<CommittedRow>,
    /// Rows that did not, each carrying its violation.
    pub conflicted: Vec<RowOutcome>,
}

impl CommitReceipt {
    /// The terminal state this receipt puts the run in.
    ///
    /// `completed_with_conflicts` is a **success**: `inst-bk-phase2`'s "committed
    /// rows stand" is the whole posture, and a run that landed nine of ten rows
    /// has done nine rows of work the operator keeps.
    #[must_use]
    pub fn terminal_state(&self) -> BulkState {
        if self.conflicted.is_empty() {
            BulkState::Completed
        } else {
            BulkState::CompletedWithConflicts
        }
    }
}

/// Run Phase 2 for an import whose Phase 1 passed.
///
/// The run must be `validating`; it is moved to `committing`, takes its locks,
/// commits row by row, releases, and lands on the terminal state
/// [`CommitReceipt::terminal_state`] names.
///
/// # Errors
/// [`DomainError`] for a storage or state-machine refusal that is about the
/// **run** rather than a row — a move that is not an edge, a run that does not
/// exist. A row's refusal is not an error: it is a conflicted entry in the
/// receipt.
#[allow(
    clippy::too_many_arguments,
    reason = "every argument is a fact only the caller holds: the provider the run's own \
              statements go through, the price repository whose methods each open the \
              transaction one row commits in, the scope, the tenant, the run, the batch, the \
              D-135 stamp and the instant. Bundling them would put values that are never \
              carried together anywhere else into a type existing only to satisfy a count"
)]
pub async fn commit_batch(
    db: &DBProvider<DbError>,
    prices: &PriceRepo,
    scope: &AccessScope,
    tenant_id: Uuid,
    operation_id: Uuid,
    rows: &[ImportRow],
    stamp: AuditStamp,
    now: OffsetDateTime,
) -> Result<CommitReceipt, DomainError> {
    let conn = db
        .conn()
        .map_err(|e| DomainError::Internal(format!("bss-pricing: bulk commit: {e}")))?;

    // Resolved before the run moves: the read is Phase 2's own and tells it which
    // rows are edits (and therefore which rows there are to lock at all).
    let drafts = draft_rows(&conn, scope, tenant_id, rows).await?;
    let mut targets: Vec<Uuid> = rows
        .iter()
        .filter_map(|row| drafts.get(&row.scope_key).map(|found| found.price_id))
        .collect();
    // Deduped, or a batch naming one draft twice collides with **its own** lock and
    // conflicts every row against itself. Phase 1 refuses in-batch duplicates, but
    // this function is `pub` and a caller is not obliged to have run it.
    targets.sort_unstable();
    targets.dedup();

    bulk_repo::advance(
        &conn,
        scope,
        tenant_id,
        operation_id,
        // Phase 1 has just passed on a run this function's own contract says is
        // `validating`; the premise rides into the statement so a second caller
        // on one run cannot re-enter `committing` over the first's work.
        BulkState::Validating,
        BulkState::Committing,
        // **Not a placeholder.** This column is what an abort reports from, and
        // overwriting it on entry left a run that died mid-flight with nothing
        // but `{"phase":"committing"}` — every committed row invisible. It now
        // carries how many rows the run is about, so an abort can say how many
        // were not attempted.
        serde_json::json!({ "phase": "committing", "rows": rows.len() }),
        now,
    )
    .await
    .map_err(|e| repo_failure(&e))?;

    // **Everything from here releases its locks and then lands the run terminal,
    // in that order, on every path.** §4 offers no failure edge out of
    // `committing`, so a run that enters it and stops has no exit of its own —
    // holding its rows against every interactive editor. `inst-bs-done` says the
    // lock is released "either way"; the `?` that used to sit on `commit_rows` made
    // that false.
    //
    // **The order is the whole of it, and it is `abandon_committing_run`'s** (review
    // H9): when the release fails there is no honest terminal landing to make, so
    // that path stops with the run still `committing` and the guard still armed,
    // which is the only state the release can be retried from at all.
    //
    // **There is an operator remedy**, and it is not D-37's lease takeover, which is
    // unbuilt and is not what rescues this run. The door is
    // `POST …/bulk-imports/{id}/abort`, its body is [`abandon_committing_run`], and
    // D-300 built it — `api::rest::bulk_imports`' own module doc puts it as "§4
    // already has that edge, which is why abort needs no state of its own, and why a
    // crashed import cannot freeze interactive authoring: the operator has a door".
    //
    // The two `?`s the arms below deliberately do not carry are the *arms*' half of
    // that sentence. The guard is the other half: a panic unwinds past an arm and a
    // dropped future never reaches one, and only `Drop` covers both. It is armed
    // here rather than after `take_locks`, so a panic inside `take_locks` itself —
    // which has already inserted rows by the time it can fail — is covered too.
    let mut lock_guard = CommitLockGuard::new(db.clone(), scope.clone(), tenant_id, operation_id);

    let (receipt, failure) =
        match bulk_repo::take_locks(&conn, scope, tenant_id, operation_id, &targets, now).await {
            Ok(()) => {
                // **The receipt survives the failure, and D-300 is why.** Every row
                // is its own transaction, so on a run-level fault at row `k` the
                // rows before it are *in the store*. This arm used to discard the
                // receipt and substitute `not_attempted_all`, so the run's stored
                // report asserted that rows which had committed were never
                // attempted — and an operator resubmitting on that report meets a
                // stale `ETag` on every one of them.
                let (partial, failed) = commit_rows(
                    prices,
                    scope,
                    tenant_id,
                    operation_id,
                    rows,
                    &drafts,
                    stamp,
                    now,
                )
                .await;
                // **A release that did not happen ends the landing here**, and this is
                // the one statement whose failure is *not* survivable by carrying on.
                //
                // Nothing else deletes a bulk run's locks: the abort route refuses a
                // run that is no longer `committing` (`LIFECYCLE_FORBIDDEN`, "a run
                // that is over has no locks to clear"), `pricing_bulk_row_lock` has no
                // sweeper and D-37's lease takeover is unbuilt. A terminal run over
                // undeleted locks therefore freezes every row of the batch against
                // interactive editing and against every later bulk run, permanently,
                // named by a run that is over.
                //
                // **Landing the run anyway is D-300's reading, and D-300's own sweep
                // answers it.** [`abandon_committing_run`] puts its `?` on exactly this
                // call for the opposite conclusion: a failed release must leave the run
                // `committing`, because that is the only state the remedy is reachable
                // from. The guard is still armed here, so its fallback retries the
                // release and the landing on whatever runtime is current, and the abort
                // route is the operator's own retry after that.
                release_or_land(
                    &conn,
                    scope,
                    tenant_id,
                    operation_id,
                    failed.as_ref(),
                    partial.committed.len(),
                )
                .await?;
                (partial, failed)
            }
            // Another run holds one of the rows, and `take_locks` released what this
            // one had taken. Nothing committed; every row is un-attempted.
            Err(held @ RepoError::BulkRowLocked { .. }) => (not_attempted(rows, &held), None),
            // **`not_attempted_all` is true of the rows; "the locks were not taken"
            // is not true of the table.** `take_locks` propagates from its own
            // `lock_holder(..)?` and from its own `release_locks(..)?`, so a
            // non-`BulkRowLocked` failure can return with lock rows still standing.
            // Falling through to the terminal `advance` below and disarming the
            // guard then lands the run `completed_with_conflicts` **with its locks
            // held**, which is the one shape nothing can undo: the abort route
            // refuses a run that is no longer `committing`, the lock table has no
            // sweeper and D-37 is unbuilt, so every price row of the batch is frozen
            // against interactive editing permanently.
            //
            // So this arm gets the `Ok` arm's treatment, for the `Ok` arm's reason.
            Err(other) => {
                let failed = Some(repo_failure(&other));
                release_or_land(&conn, scope, tenant_id, operation_id, failed.as_ref(), 0).await?;
                (not_attempted_all(rows), failed)
            }
        };

    // The run reaches a terminal state on every path **that got its locks back**
    // and the caller still learns the failure — both survive, which is what a state
    // machine with no failure edge forces. The paths that do not reach it return
    // early above, and for one reason: a terminal run with its locks held is the
    // one shape from which nothing can release them, so neither a release that
    // failed nor a lock set `take_locks` could not clear may be landed.
    bulk_repo::advance(
        &conn,
        scope,
        tenant_id,
        operation_id,
        BulkState::Committing,
        receipt.terminal_state(),
        report_of(&receipt),
        now,
    )
    .await
    .map_err(|e| repo_failure(&e))?;
    // The locks are released and the run is terminal, which is the whole of what
    // the guard exists to do. It stays armed if the statement above failed: its
    // fallback is then the only thing left that will try.
    lock_guard.disarm();
    failure.map_or(Ok(receipt), Err)
}

/// Release this run's row locks, or land the run where the remedy can reach it.
///
/// The one thing both lock-failure arms of [`commit_batch`] owe: a terminal run
/// holding its locks is the shape nothing undoes — the abort route refuses a run
/// that is no longer `committing`, and the lock table has no sweeper. So a failed
/// release returns early, leaving the run `committing` with the guard still armed.
///
/// `committed` is the count the operator needs to size the divergence: those rows
/// are in the store and not in the run's report, since the report lands with the
/// run and the run does not land here.
///
/// # Errors
/// [`unreleased_landing`]'s value when the release itself failed.
async fn release_or_land(
    conn: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    operation_id: Uuid,
    failed: Option<&DomainError>,
    committed: usize,
) -> Result<(), DomainError> {
    let Err(unreleased) = bulk_repo::release_locks(conn, scope, tenant_id, operation_id).await
    else {
        return Ok(());
    };
    tracing::error!(
        error = %unreleased,
        operation_id = %operation_id,
        rows_committed = committed,
        run_failed = failed.is_some(),
        "bss-pricing: a bulk commit could not release its row locks; the run is left committing \
         with its locks held - which is the retryable state, swept by this commit's guard and by \
         POST /bulk-imports/{{id}}/abort - and any rows it did commit are in the store but not \
         in the run's report"
    );
    Err(unreleased_landing(&unreleased, failed.cloned()))
}

/// The failure a landing reports when its `release_locks` did not happen.
///
/// **Both halves reach the caller when both failed**: the run-level commit fault
/// says why nothing more was committed, and the unreleased locks say what state the
/// run is in and which door clears it. Folding the two with `Option::or_else` keeps
/// only the first — the closure does not run when the first is `Some` — which
/// reports a permanent freeze of every row in the batch as a plain commit failure.
///
/// The combined arm is [`DomainError::Internal`] rather than either input's own
/// variant: neither of the two facts is the whole answer, and both inputs are
/// storage-class already — a row's *own* refusal is never an error here, it is a
/// conflicted entry in the receipt.
fn unreleased_landing(unreleased: &RepoError, failed: Option<DomainError>) -> DomainError {
    let unreleased = repo_failure(unreleased);
    let Some(failed) = failed else {
        return unreleased;
    };
    DomainError::Internal(format!(
        "bss-pricing: bulk commit: the run failed ({failed}) and its row locks could not be \
         released either ({unreleased}); the run is left committing with its locks held, so \
         `POST /bulk-imports/{{id}}/abort` is the remedy and is retryable"
    ))
}

/// Commit each row on its own, collecting what landed and what did not.
#[allow(
    clippy::too_many_arguments,
    reason = "the repository, the scope, the tenant, the run whose lock its own edits are \
              allowed through, the batch, the drafts already read, the stamp and the instant"
)]
async fn commit_rows(
    prices: &PriceRepo,
    scope: &AccessScope,
    tenant_id: Uuid,
    operation_id: Uuid,
    rows: &[ImportRow],
    drafts: &HashMap<ScopeKey, PriceRecord>,
    stamp: AuditStamp,
    now: OffsetDateTime,
) -> (CommitReceipt, Option<DomainError>) {
    let mut receipt = CommitReceipt::default();
    for (index, row) in rows.iter().enumerate() {
        let outcome = if let Some(found) = drafts.get(&row.scope_key) {
            // An edit: the `ETag` the row asserted, against the draft it names.
            let Some(expected) = row.if_match else {
                receipt.conflicted.push(conflicted(
                    index,
                    format!(
                        "a draft row ({}) already holds this key and this row asserted no \
                         version; re-read it and resubmit with its `ETag`",
                        found.price_id
                    ),
                ));
                continue;
            };
            prices
                .update_draft(
                    scope,
                    tenant_id,
                    found.price_id,
                    expected,
                    row.content.clone(),
                    stamp,
                    // The run that locked this row is the one editing it, which is
                    // the whole distinction: the lock excludes somebody else, not
                    // its own holder.
                    Some(operation_id),
                )
                .await
                .map(|record| record.price_id)
        } else {
            // A new key. `if_match` on a key nothing holds is the mirror fault.
            if row.if_match.is_some() {
                receipt.conflicted.push(conflicted(
                    index,
                    "this row asserted a version and no draft holds its key; the row it meant \
                     to edit is gone, so re-read and resubmit"
                        .to_owned(),
                ));
                continue;
            }
            let price_id = Uuid::now_v7();
            prices
                .create_draft(
                    scope,
                    tenant_id,
                    NewPriceDraft {
                        price_id,
                        scope_key: row.scope_key.clone(),
                        content: row.content.clone(),
                        created_by: stamp.actor_principal_id,
                        created_at_utc: now,
                        correlation_id: stamp.correlation_id,
                    },
                )
                .await
                .map(|record| record.price_id)
        };

        match outcome {
            Ok(price_id) => receipt.committed.push(CommittedRow {
                row: index,
                price_id,
            }),
            // **Only a row's own refusal is a conflict.** A storage failure is the
            // run's, and swallowing it here would report a batch as conflicted
            // when the database was down.
            // **`DuplicateScopeKey` is per-row, and the design says so outright**:
            // "one racing a concurrent author fails at commit on the draft-plane
            // partial UNIQUE, reported per-row like every other row outcome".
            // `NotFound` is the same shape — a concurrent author deleted the draft
            // this row named. D-291 caught neither and took the whole run down for
            // a fact about one row.
            // **Four more joined the partition in D-300**, each a fact about one
            // row that Phase 1 does not screen and a bulk body can set: a
            // `grandfatherUntil` on a non-grandfathered key, a sub-millisecond
            // horizon, an authored quantity past its column, and content whose
            // usage line disagrees with its key. `inst-bk-phase1`'s "reported
            // per-row like every other row outcome" covers these too, and taking
            // the run down for one of them also erased what the run had committed.
            Err(
                e @ (RepoError::StaleRowVersion { .. }
                | RepoError::NotDraft { .. }
                | RepoError::DuplicateScopeKey(_)
                | RepoError::NotFound { .. }
                | RepoError::GrandfatherHorizonOffClass { .. }
                | RepoError::TimestampPrecisionExceeded { .. }
                | RepoError::ValueOutOfRange { .. }
                | RepoError::UsageLineDisagrees { .. }),
            ) => {
                receipt.conflicted.push(conflicted(index, e.to_string()));
            }
            // A run-level fault. **The rows already committed stay in the receipt**
            // — they are in the store — and the rows from here on are reported
            // un-attempted, which is true of them and of nothing else.
            Err(other) => {
                let failure = repo_failure(&other);
                // **Row `index` was attempted, and it is the row that failed.**
                // Swept into the loop below it is told "not attempted: the run
                // failed at row {index}" — a sentence false of the one row it is
                // most specific about, and about the only row in the receipt whose
                // own transaction the operator can read something into. Its own
                // sentence names its failure; the rows after it are the ones
                // nothing reached.
                //
                // `not_attempted`'s doc one screen down records this family being
                // corrected once already, for putting the contended row's
                // `price_id` on every row's violation.
                receipt.conflicted.push(conflicted(
                    index,
                    format!(
                        "this row was attempted and its own transaction failed, so nothing was \
                         committed for it and the run stopped here. {failure}"
                    ),
                ));
                for unreached in index.saturating_add(1)..rows.len() {
                    receipt.conflicted.push(conflicted(
                        unreached,
                        format!("not attempted: the run failed at row {index}. {failure}"),
                    ));
                }
                return (receipt, Some(failure));
            }
        }
    }
    (receipt, None)
}

/// The draft rows occupying any key the batch aims at, by key.
///
/// One read per plan, `infra::import`'s arrangement and its reason.
async fn draft_rows(
    runner: &impl toolkit_db::secure::DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    rows: &[ImportRow],
) -> Result<HashMap<ScopeKey, PriceRecord>, DomainError> {
    let mut plans: Vec<PlanId> = rows.iter().map(|row| row.scope_key.plan_id()).collect();
    plans.sort_by_key(|plan| plan.get());
    plans.dedup();

    let mut occupied = HashMap::new();
    for plan in plans {
        let found =
            price_repo::load_for_plan(runner, scope, tenant_id, plan, &[LifecycleState::Draft])
                .await
                .map_err(|e| repo_failure(&e))?;
        for record in found {
            occupied.insert(record.scope_key.clone(), record);
        }
    }
    Ok(occupied)
}

/// One conflicted row.
fn conflicted(row: usize, detail: String) -> RowOutcome {
    RowOutcome {
        row,
        violations: vec![RowViolation {
            code: BULK_ROW_CONFLICT.to_owned(),
            detail,
        }],
    }
}

/// Nothing committed, every row un-attempted, because the run could not take its
/// locks.
///
/// **Each row's sentence is true of that row.** The first version put the holder's
/// `price_id` on all of them, so row 3's violation asserted something about row
/// 0's price. §4's reading of a run that committed nothing is "uncommitted rows
/// reported as not-attempted"; the contended row is named once, as context.
fn not_attempted(rows: &[ImportRow], held: &RepoError) -> CommitReceipt {
    let detail = format!(
        "not attempted: the run could not take its row locks and committed nothing. {held}"
    );
    CommitReceipt {
        committed: Vec::new(),
        conflicted: (0..rows.len())
            .map(|row| conflicted(row, detail.clone()))
            .collect(),
    }
}

/// The same, for a failure that is the run's rather than any row's.
fn not_attempted_all(rows: &[ImportRow]) -> CommitReceipt {
    CommitReceipt {
        committed: Vec::new(),
        conflicted: (0..rows.len())
            .map(|row| {
                conflicted(
                    row,
                    "not attempted: the run failed before this row was reached".to_owned(),
                )
            })
            .collect(),
    }
}

/// The receipt as the run's stored report.
///
/// `serde_json::to_value` of the receipt itself, not a JSON object assembled
/// beside it. **That was the second spelling D-285 named and left standing**: a
/// hand-built object and the type it describes are two things that can disagree,
/// and `inst-bk-idem` replays this column to a retry, so the shape is a contract.
/// The conflicted half is normalised through [`BatchReport`] first, so a report
/// written by two phases still holds one entry per row.
fn report_of(receipt: &CommitReceipt) -> serde_json::Value {
    let mut conflicts = BatchReport::default();
    for outcome in &receipt.conflicted {
        for violation in &outcome.violations {
            conflicts.add(outcome.row, violation.clone());
        }
    }
    let normalised = CommitReceipt {
        committed: receipt.committed.clone(),
        conflicted: conflicts.rows().to_vec(),
    };
    serde_json::to_value(&normalised).unwrap_or_else(|_| {
        // Unreachable: every field is a `Uuid`, a `usize` or a `String`. A JSON
        // failure here would be a serde bug, and losing the report to it would be
        // worse than storing an empty one the operator can see is empty.
        serde_json::json!({ "committed": [], "conflicted": [] })
    })
}
