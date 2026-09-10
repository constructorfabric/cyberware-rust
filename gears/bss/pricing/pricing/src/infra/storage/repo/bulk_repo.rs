//! Repository for `pricing_bulk_operation` and `pricing_bulk_row_lock` — a bulk
//! run and the rows it holds while it commits
//! (`design/12-operator-efficiency.md` §4, §6; D-260, D-262, D-267).
//!
//! # The state machine is the trigger's, and this repository does not restate it
//!
//! [`advance`] names a target state and writes it. Whether the move is an edge is
//! the table's business, on both engines, and the refusal it raises is the one an
//! operator reads. A `match` here listing the legal edges would be a second
//! spelling of a rule that already exists in two places — and `rejected` arriving
//! four migrations after the table is the demonstration that the two would drift.
//!
//! **What the trigger cannot judge, and what [`advance`] therefore carries**. The deferral
//! above covers the *edge*; it does not cover the caller's *premise*. Two holes are left by it,
//! and both are the same shape:
//!
//! * A move to the state a run is already in returns early on both engines
//!   (`NEW.state = OLD.state` → `RETURN NEW`), so a repeat is admitted in
//!   silence — and `advance` rewrites `report` and `completed_at` wholesale, so
//!   the repeat clobbers the run's stored answer with whatever the late caller
//!   carried.
//! * A caller acting on a state it read a moment ago may name a target that is a
//!   legal edge from where the row *now* stands: `validating -> committing` and
//!   `awaiting_approval -> committing` are both edges, so an immateriality
//!   decision taken over `validating` would commit a run a concurrent evaluation
//!   had meanwhile parked for approval.
//!
//! So [`advance`] takes the state the caller believes the run is in and puts it in
//! the `WHERE`. That is not a second spelling of the edge list — it is the
//! caller's own premise, which no trigger can know — and it is the arrangement
//! every state-moving write in this crate already uses
//! (`approval_repo::swap`, `overlay_repo`, `window_repo`, `pin_frontier_repo`,
//! `policy_repo`, `idempotency_repo`). `window_repo`'s own sentence is the rule:
//! "a tag read, compared and then handed to a statement is a decision racing the
//! write it authorizes".
//!
//! # The lock's mutual exclusion is its primary key
//!
//! `pricing_bulk_row_lock` is keyed `(tenant_id, price_id)`, so two runs cannot
//! hold one row: the second insert collides. [`take_locks`] therefore does not
//! read first and then write — it writes, and reads only to *name* the holder in
//! the refusal, which is what `fr-concurrent-edit` asks for. The read-then-write
//! arrangement would be the check-then-act race the key exists to close.
//!
//! **A lock row names a holder that is committed.** Unlike the approval
//! register's pending-key conflict, which cannot name its holder because the
//! winning transaction may not have committed, a lock is inserted by a run that
//! is already `committing` — so the row this repository reads back is durable and
//! the refusal can carry it.

use sea_orm::ActiveValue::Set;
use sea_orm::{ColumnTrait, Condition, EntityTrait, JsonValue};
use time::OffsetDateTime;
use toolkit_db::secure::{
    AccessScope, DBRunner, SecureDeleteExt, SecureEntityExt, SecureInsertExt, SecureUpdateExt,
};
use toolkit_db::{DBProvider, DbError};
use uuid::Uuid;

use crate::domain::bulk::{BulkKind, BulkState};
use crate::infra::storage::entity::{bulk_operation, bulk_row_lock};
use crate::infra::storage::{RepoError, contention_or_db};

/// A run as this crate reads it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BulkOperationRecord {
    /// The run's durable name.
    pub operation_id: Uuid,
    /// RLS scope.
    pub tenant_id: Uuid,
    /// Which flow it is.
    pub kind: BulkKind,
    /// Where it stands.
    pub state: BulkState,
    /// O4's client key, unique per tenant.
    pub client_key: String,
    /// The digest of the request the key was first spent on.
    ///
    /// **Empty for a run opened before `pricing_bulk_operation`**, and that is the only
    /// way it can be empty — the column's `NOT NULL DEFAULT` backfilled every
    /// pre-existing row and every writer since sets a 32-byte digest. A replay
    /// that reads an empty digest has nothing to compare against and must say so
    /// rather than treat "no record" as "a different request".
    pub request_hash: Vec<u8>,
    /// The per-row report, which grows as the run progresses.
    pub report: JsonValue,
    /// Who submitted it.
    pub submitted_by: Uuid,
    /// When.
    pub submitted_at: OffsetDateTime,
    /// Set exactly on the terminal states, which the `CHECK` keeps honest.
    pub completed_at: Option<OffsetDateTime>,
}

/// A run at its birth.
///
/// No `state`: a run is born `validating` and the table's insert trigger refuses
/// any other value outright, so offering the caller a choice would be offering
/// one the store takes back.
#[derive(Clone, Debug)]
pub struct NewBulkOperation {
    /// Minted by the caller, so the audit record of the same act can name it.
    pub operation_id: Uuid,
    /// RLS scope.
    pub tenant_id: Uuid,
    /// Which flow.
    pub kind: BulkKind,
    /// O4's client key.
    pub client_key: String,
    /// The digest of the request this key is being spent on.
    ///
    /// Required rather than optional, and the caller computes it from the request
    /// it has already parsed (`preconditions::request_digest`): a run's key is
    /// spent on **one** request, so a writer that could omit the digest could
    /// leave a key whose payload no replay can check. What each flow does with it
    /// on the replay is that surface's own decision — the import refuses a
    /// mismatch, the repricing surface answers the frozen run (see
    /// `api::rest::repricing_runs`' replay arm) — but recording it is not
    /// optional for either.
    pub request_hash: Vec<u8>,
    /// The report as it stands at birth — Phase 1 has not run.
    pub report: JsonValue,
    /// Who submitted it.
    pub submitted_by: Uuid,
    /// When.
    pub submitted_at: OffsetDateTime,
}

/// The repository handle, for callers that own no transaction.
#[derive(Clone)]
pub struct BulkRepo {
    db: DBProvider<DbError>,
}

impl BulkRepo {
    /// Wrap a provider.
    #[must_use]
    pub const fn new(db: DBProvider<DbError>) -> Self {
        Self { db }
    }

    /// [`read`] on a fresh connection.
    ///
    /// # Errors
    /// Exactly [`read`]'s.
    pub async fn read(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        operation_id: Uuid,
    ) -> Result<Option<BulkOperationRecord>, RepoError> {
        let conn = self
            .db
            .conn()
            .map_err(|e| RepoError::Db(format!("pricing_bulk_operation conn: {e}")))?;
        read(&conn, scope, tenant_id, operation_id).await
    }
}

/// Open a run, on a runner the caller owns.
///
/// **A unique violation here is a contention, not a storage failure**. Two
/// indexes stand on this table and, unusually, both want the same answer:
///
/// * `uq_pricing_bulk_operation_client_key` on `(tenant_id, kind, client_key)` is
///   O4's client idempotency key. Both callers replay by reading
///   [`find_by_client_key`] first and opening second — in a **different
///   transaction** from this insert — so two concurrent submits carrying one
///   `Idempotency-Key` both see the key free and both arrive here. That is the
///   read-then-write race `idempotency_repo`'s module doc argues is the wrong
///   shape for a client key, one table over: *"between the read and the write
///   nothing holds the key, so both callers see it free."* The bulk lane cannot
///   simply adopt `IdempotencyGate` — its key lives on the run row so a replay can
///   serve the run's report — but it can classify, and as a bare
///   [`RepoError::Db`] the loser got a 500 for a request whose contract promises
///   either the winner's `202` or a refusal it can act on;
/// * the primary key `operation_id`, which is minted by
///   `Uuid::now_v7()` at both call sites rather than by the client.
///
/// So [`contention_or_db`] is right for both, and this is **not** the divergence
/// from D-159 its own doc reports: there, a caller-minted id told to retry
/// resubmits the same id and collides identically, forever. Here a retry does
/// succeed — the second attempt reads the winner's run through
/// [`find_by_client_key`] and replays it, and a fresh `now_v7` cannot collide
/// twice.
///
/// The at-most-once guarantee never depended on the class: the loser executes
/// nothing either way. What was wrong is only the answer.
///
/// # Errors
/// [`RepoError::ConcurrentMutation`] naming the client key, when either unique
/// index refuses; [`RepoError::Db`] on a scope or other storage failure — which
/// includes the insert trigger's refusal of any birth state but `validating`.
pub async fn open(
    runner: &impl DBRunner,
    scope: &AccessScope,
    new: NewBulkOperation,
) -> Result<BulkOperationRecord, RepoError> {
    let row = bulk_operation::ActiveModel {
        operation_id: Set(new.operation_id),
        tenant_id: Set(new.tenant_id),
        kind: Set(new.kind.as_str().to_owned()),
        state: Set(BulkState::Validating.as_str().to_owned()),
        client_key: Set(new.client_key.clone()),
        request_hash: Set(new.request_hash.clone()),
        report: Set(new.report.clone()),
        submitted_by: Set(new.submitted_by),
        submitted_at: Set(new.submitted_at),
        completed_at: Set(None),
    };
    bulk_operation::Entity::insert(row.clone())
        .secure()
        .scope_with_model(scope, &row)
        .map_err(|e| RepoError::Db(format!("pricing_bulk_operation scope: {e}")))?
        .exec(runner)
        .await
        .map_err(|e| {
            contention_or_db(
                &e,
                &format!(
                    "bulk operation under {} client key {}",
                    new.kind.as_str(),
                    new.client_key
                ),
                "insert pricing_bulk_operation",
            )
        })?;

    read(runner, scope, new.tenant_id, new.operation_id)
        .await?
        .ok_or_else(|| {
            RepoError::CorruptRow(format!(
                "pricing_bulk_operation {} was inserted and does not read back",
                new.operation_id
            ))
        })
}

/// One run by id.
///
/// # Errors
/// [`RepoError::Db`] on a scope or storage failure; [`RepoError::CorruptRow`] for
/// a stored kind or state token no `CHECK` should have admitted.
pub async fn read(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    operation_id: Uuid,
) -> Result<Option<BulkOperationRecord>, RepoError> {
    let row = bulk_operation::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(bulk_operation::Column::TenantId.eq(tenant_id))
                .add(bulk_operation::Column::OperationId.eq(operation_id)),
        )
        .one(runner)
        .await
        .map_err(|e| RepoError::Db(format!("read pricing_bulk_operation: {e}")))?;
    row.as_ref().map(record_of).transpose()
}

/// The run a client key already opened **for this kind**, if any.
///
/// **`kind` is a filter and not a convenience** (D-307). §5 gives the two flows
/// two different idempotency columns — the import's `Idempotency-Key` and a
/// repricing run's own `run_id` — and without this predicate an import replay
/// would answer `202` describing a *repricing run*, import nothing, and hand back
/// a view carrying no `kind` member to reveal the substitution. The index behind
/// it moved to `(tenant_id, kind, client_key)` in the same wave; either half
/// alone leaves the other's hole open.
///
/// **The payload is not a predicate here, and deliberately not**. The
/// record this returns carries `request_hash`, and the caller compares it: a
/// mismatch has to be *answered* — `IDEMPOTENCY_PAYLOAD_MISMATCH` — rather than
/// filtered away, because a key that opened a run under a different body must not
/// read as a key that opened nothing at all. Folding the digest into the `WHERE`
/// would turn the refusal into a second `open` under a spent key, which the unique
/// index would then refuse with a storage collision naming nothing an operator can
/// act on.
///
/// # Errors
/// Exactly [`read`]'s.
pub async fn find_by_client_key(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    kind: BulkKind,
    client_key: &str,
) -> Result<Option<BulkOperationRecord>, RepoError> {
    let row = bulk_operation::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(bulk_operation::Column::TenantId.eq(tenant_id))
                .add(bulk_operation::Column::Kind.eq(kind.as_str()))
                .add(bulk_operation::Column::ClientKey.eq(client_key)),
        )
        .one(runner)
        .await
        .map_err(|e| RepoError::Db(format!("read pricing_bulk_operation by client key: {e}")))?;
    row.as_ref().map(record_of).transpose()
}

/// Move a run from `from` to `to`, writing the report it has reached.
///
/// `completed_at` is supplied for a terminal state and `None` otherwise, which
/// the `CHECK` pairs with the state — so a caller that disagrees with itself is
/// refused by the store rather than by a comparison here.
///
/// **`from` is the caller's premise, carried into the `WHERE`**. See the
/// module doc for the two holes a target-only statement leaves: the self-edge the
/// trigger returns early on, and a stale premise whose target happens to be a
/// legal edge from where the row now stands. Matching no row is **re-read before
/// it is reported**, `approval_repo::swap`'s arrangement and its reason: the state
/// named in the refusal is the row's own as of now, not the one the caller read.
///
/// # Errors
/// [`RepoError::Db`] on a scope or storage failure — which includes the
/// transition trigger's refusal of a move that is not an edge, and the `CHECK`'s
/// refusal of a terminal state without an instant;
/// [`RepoError::ConcurrentMutation`] when the run has moved off `from`;
/// [`RepoError::NotFound`] when nothing in this caller's scope answers to the id.
#[allow(
    clippy::too_many_arguments,
    reason = "the run's address, the move's two ends, the report it carries and the instant; \
              the two ends are the whole of what Z8-7 added and neither is derivable from \
              the other"
)]
pub async fn advance(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    operation_id: Uuid,
    from: BulkState,
    to: BulkState,
    report: JsonValue,
    at: OffsetDateTime,
) -> Result<BulkOperationRecord, RepoError> {
    let completed_at = to.is_terminal().then_some(at);
    let affected = bulk_operation::Entity::update_many()
        .secure()
        .scope_with(scope)
        .col_expr(
            bulk_operation::Column::State,
            sea_orm::sea_query::Expr::value(to.as_str()),
        )
        .col_expr(
            bulk_operation::Column::Report,
            sea_orm::sea_query::Expr::value(report),
        )
        .col_expr(
            bulk_operation::Column::CompletedAt,
            sea_orm::sea_query::Expr::value(completed_at),
        )
        .filter(
            Condition::all()
                .add(bulk_operation::Column::TenantId.eq(tenant_id))
                .add(bulk_operation::Column::OperationId.eq(operation_id))
                .add(bulk_operation::Column::State.eq(from.as_str())),
        )
        .exec(runner)
        .await
        .map_err(|e| RepoError::Db(format!("advance pricing_bulk_operation: {e}")))?;
    if affected.rows_affected == 0 {
        return Err(match read(runner, scope, tenant_id, operation_id).await? {
            Some(fresh) => RepoError::ConcurrentMutation {
                aggregate: format!(
                    "bulk operation {operation_id}: the move {} -> {} names a run that now \
                     reads {}",
                    from.as_str(),
                    to.as_str(),
                    fresh.state.as_str()
                ),
            },
            None => RepoError::NotFound {
                subject: "bulk operation".to_owned(),
                id: operation_id.to_string(),
            },
        });
    }
    read(runner, scope, tenant_id, operation_id)
        .await?
        .ok_or_else(|| {
            RepoError::CorruptRow(format!(
                "pricing_bulk_operation {operation_id} moved and does not read back"
            ))
        })
}

/// Take the run's row locks (`inst-bk-lock`).
///
/// **The run must already be `committing`.** The lock table's own trigger says so
/// — "the bulk lock takes effect only on entry to committing" — and it is not a
/// formality: a lock held by a run that is still validating would exclude every
/// interactive editor for the length of Phase 1, which is a read.
///
/// Writes first and reads only to name a holder, for the module doc's reason: the
/// key is the mutual exclusion, and a read-then-write would be the check-then-act
/// race it exists to close.
///
/// **`runner` must be an autocommit connection, not a transaction.** Postgres
/// aborts an enclosing transaction on a failed statement, and everything this
/// function does after a refused insert is a further statement — so inside a
/// transaction the refusal degrades from [`RepoError::BulkRowLocked`] to
/// [`RepoError::BulkLocksHeld`], losing exactly the holder `fr-concurrent-edit`
/// requires it to name.
///
/// **Measured on Postgres rather than reasoned about** (`postgres_bulk_repo`,
/// D-297): the statement that dies is the **holder read**, not the release. The
/// read runs first, so the release is never reached at all and the error carries
/// `read pricing_bulk_row_lock: … current transaction is aborted`.
///
/// **This paragraph has now been wrong twice, in opposite directions**, which is
/// why it says who moved the statement rather than only which one dies. D-294 put
/// the release in front of the read and this sentence, which had said the read,
/// was corrected to match. Review **M44** put the read back in front
/// — for the idempotency reason the body comment gives: a release-first ordering
/// deletes this run's own lock rows before the read that would have recognized
/// them — and the corrected sentence was then describing the code it had just
/// replaced. So the order is back where it started and this text is the third
/// statement of it. The observable half is pinned by
/// `take_locks_inside_a_transaction_loses_the_holder`, which asserts the message
/// names the read: that case, and not this sentence, is what noticed the swap and
/// what will notice the next one.
///
/// **Every lock or none holds for [`RepoError::BulkRowLocked`], and there only.**
/// That arm reads the holder and releases what this run already took before it
/// answers. The two `?` paths below it — the holder read, and the release
/// statement itself — return with rows still locked, because the release has
/// either not run yet or is the statement that failed. Those answer
/// [`RepoError::BulkLocksHeld`] rather than a bare [`RepoError::Db`], so a caller
/// can tell a partial hold from a storage fault and leave such a run `committing`.
///
/// # Errors
/// [`RepoError::BulkRowLocked`] naming the run that already holds a row;
/// [`RepoError::BulkLocksHeld`] when the refusal path could not clear this run's
/// own rows; [`RepoError::Db`] on a scope or storage failure before any lock was
/// taken.
pub async fn take_locks(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    operation_id: Uuid,
    price_ids: &[Uuid],
    at: OffsetDateTime,
) -> Result<(), RepoError> {
    for &price_id in price_ids {
        let row = bulk_row_lock::ActiveModel {
            tenant_id: Set(tenant_id),
            price_id: Set(price_id),
            bulk_operation_id: Set(operation_id),
            locked_at: Set(at),
        };
        let taken = bulk_row_lock::Entity::insert(row.clone())
            .secure()
            .scope_with_model(scope, &row)
            .map_err(|e| RepoError::Db(format!("pricing_bulk_row_lock scope: {e}")))?
            .exec(runner)
            .await;
        if let Err(e) = taken {
            // **Read the holder before releasing anything.** The other order makes this
            // function non-idempotent for its own run: `release_locks` deletes every lock
            // row of *this* `operation_id`, so when the colliding row is this run's own,
            // the subsequent `lock_holder` finds nothing and the function answers
            // `RepoError::Db` -- a 500 -- having just unlocked every row the run
            // still needed.
            //
            // That ordering also quietly falsified `infra::repricing`'s stated
            // reason for leaving a wedged run `committing`: "`take_locks`... a
            // second call simply retakes the lock and carries on". It would not
            // have. No path reaches it today -- there is no redrive route -- but
            // the redrive that D-37 decided would have been built on it.
            //
            // Because the read comes first, its own failure leaves the rows this
            // run has already taken — so it is named rather than flattened to
            // `Db`: the caller has to leave the run `committing`, which is the only
            // state the abort route can release them from.
            let holder = match lock_holder(runner, scope, tenant_id, price_id).await {
                Ok(holder) => holder,
                Err(e) => {
                    return Err(RepoError::BulkLocksHeld {
                        operation_id: operation_id.to_string(),
                        cause: e.to_string(),
                    });
                }
            };

            // **This run's own lock is success, not a collision.** The row is
            // already held by exactly the operation asking for it, which is the
            // state a redrive of a `committing` run arrives in.
            if holder == Some(operation_id) {
                continue;
            }

            // **All or none.** The inserts above are independent statements on
            // whatever runner the caller holds, so a refusal partway leaves this
            // run holding the rows it already took — and if the caller then ends
            // the run, those rows are frozen by an operation that is over. That is
            // the freeze `inst-bs-done`'s "lock released either way" and D-37's
            // release path both exist to prevent, so the release happens here,
            // where the partial set is known to be exactly this run's.
            //
            // A release that did not happen is the one outcome this paragraph's
            // "all or none" does not deliver, so it says so instead of answering
            // `Db`: the partial set is still held and only the abort route clears
            // it.
            if let Err(e) = release_locks(runner, scope, tenant_id, operation_id).await {
                return Err(RepoError::BulkLocksHeld {
                    operation_id: operation_id.to_string(),
                    cause: e.to_string(),
                });
            }

            let Some(holder) = holder else {
                return Err(RepoError::Db(format!(
                    "insert pricing_bulk_row_lock for {price_id}: {e}"
                )));
            };
            return Err(RepoError::BulkRowLocked {
                price_id: price_id.to_string(),
                bulk_operation_id: holder.to_string(),
            });
        }
    }
    Ok(())
}

/// Release every lock the run holds, returning how many moved.
///
/// # Errors
/// [`RepoError::Db`] on a scope or storage failure.
pub async fn release_locks(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    operation_id: Uuid,
) -> Result<u64, RepoError> {
    let result = bulk_row_lock::Entity::delete_many()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(bulk_row_lock::Column::TenantId.eq(tenant_id))
                .add(bulk_row_lock::Column::BulkOperationId.eq(operation_id)),
        )
        .exec(runner)
        .await
        .map_err(|e| RepoError::Db(format!("release pricing_bulk_row_lock: {e}")))?;
    Ok(result.rows_affected)
}

/// Which run holds this row, if any.
///
/// The read `fr-concurrent-edit` makes: an interactive edit asks before it
/// writes, so the refusal can name the run rather than surfacing a key collision.
///
/// # Errors
/// [`RepoError::Db`] on a scope or storage failure.
pub async fn lock_holder(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    price_id: Uuid,
) -> Result<Option<Uuid>, RepoError> {
    let row = bulk_row_lock::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(bulk_row_lock::Column::TenantId.eq(tenant_id))
                .add(bulk_row_lock::Column::PriceId.eq(price_id)),
        )
        .one(runner)
        .await
        .map_err(|e| RepoError::Db(format!("read pricing_bulk_row_lock: {e}")))?;
    Ok(row.map(|row| row.bulk_operation_id))
}

/// One stored row, read into the vocabulary the domain uses.
fn record_of(row: &bulk_operation::Model) -> Result<BulkOperationRecord, RepoError> {
    let corrupt = |what: &str, value: &str| {
        RepoError::CorruptRow(format!(
            "pricing_bulk_operation {}: {what} `{value}` is outside its enumeration",
            row.operation_id
        ))
    };
    Ok(BulkOperationRecord {
        operation_id: row.operation_id,
        tenant_id: row.tenant_id,
        kind: BulkKind::parse(&row.kind).map_err(|_| corrupt("kind", &row.kind))?,
        state: BulkState::parse(&row.state).map_err(|_| corrupt("state", &row.state))?,
        client_key: row.client_key.clone(),
        request_hash: row.request_hash.clone(),
        report: row.report.clone(),
        submitted_by: row.submitted_by,
        submitted_at: row.submitted_at,
        completed_at: row.completed_at,
    })
}
