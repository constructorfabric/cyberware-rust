//! Repository for `pricing_repricing_journal` — the rows a mass-repricing run
//! selected, and how far each got (`design/12-operator-efficiency.md` §6,
//! `inst-mr-journal`, `inst-mp-journal`; D-261, D-307).
//!
//! # The table had a schema, two behavioural suites and no caller
//!
//! It landed with D-261 and was reachable by nothing: no repository, and the two
//! suites drive raw SQL by design, so they exercised the **table** without
//! exercising any code. This module is its first writer, which is also what turns
//! `trg_pricing_repricing_journal_only_under_a_repricing_run` and
//! `chk_pricing_bulk_operation_import_never_awaits` from rules nothing can
//! violate into rules a production path can.
//!
//! # Every mutation takes the caller's runner
//!
//! Only runner-taking forms, and no method here opens a transaction. That is the
//! opposite of `infra::bulk`'s arrangement and it is D-134's requirement rather
//! than a preference: a repricing run's transaction unit is the **plan** — all of
//! a plan's selected rows commit together with the plan-level aggregate pass
//! inside that transaction — so the journal flip for a row has to land in the same
//! transaction as the successor it records. A method that opened its own would
//! leave a journal saying `applied` beside rows that rolled back.
//!
//! # The state machine is the trigger's
//!
//! `pending → applied | failed`, and a decided row is frozen whole. This module
//! names a target state and writes it, for [`super::bulk_repo`]'s stated reason:
//! a `match` here listing the legal edges would be a second spelling of a rule
//! that already exists on two engines.
//!
//! # Nothing in this chain bounds a run's size, and a caller may not assume otherwise
//!
//! `price_repo::load_published_for_selector` freezes the selector's row set with **no
//! page bound** — its own doc says so, in as many words, under "There is no page
//! bound, and that is a real edge" — [`open_rows`] inserts every row it is handed, and
//! [`list_for_run`]/[`pending_for_run`] read every row of a run back in one `Vec`.
//! Their doc comments used to claim the opposite (that a run's set was "bounded by the
//! page cap"), which was prose with no `.limit(...)` behind it anywhere in this file —
//! disproven by reading the code, confirmed empirically with rows past the crate's
//! own REST hard cap of 1,000 on one run, and it had already misled one reader: a
//! review's out-of-scope table repeated the claim while pricing a different unbounded
//! read, `load_published_for_selector`. **This is the fact the apply lane has to build
//! against**: a run may be as large as the selector froze, the selector is itself
//! unbounded, and consuming the journal has to be done in bounded batches rather than
//! by assuming this layer already capped it. Whether a cap ever gets imposed is a
//! design decision that belongs with the selector edge, not with this repository.

use sea_orm::ActiveValue::Set;
use sea_orm::{ColumnTrait, Condition, EntityTrait, Order};
use time::OffsetDateTime;
use toolkit_db::secure::{
    AccessScope, DBRunner, SecureEntityExt, SecureInsertExt, SecureUpdateExt,
};
use uuid::Uuid;

use crate::domain::bulk::JournalState;
use crate::infra::storage::RepoError;
use crate::infra::storage::entity::repricing_journal;

/// One journal row as this crate reads it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JournalRow {
    /// The run this row belongs to.
    pub run_id: Uuid,
    /// The selected price row.
    pub price_id: Uuid,
    /// RLS scope.
    pub tenant_id: Uuid,
    /// How far it got.
    pub state: JournalState,
    /// Why it failed. Present exactly on `failed`, which the `CHECK` holds.
    pub failure_reason: Option<String>,
    /// The successor the apply created. Present exactly on `applied`.
    pub applied_price_id: Option<Uuid>,
    /// When that commit landed.
    pub applied_at: Option<OffsetDateTime>,
}

/// A selected row at expansion.
///
/// No `state`: a journal row is born `pending` and the table's insert trigger
/// refuses any other birth value, so offering the caller a choice would offer one
/// the store takes back — [`super::bulk_repo::NewBulkOperation`]'s reason.
#[derive(Clone, Copy, Debug)]
#[allow(
    clippy::struct_field_names,
    reason = "the three are the table's own column names, and a journal row's whole content is \
              three identities: the run, the row it selected and the tenant it is scoped to. \
              Renaming them to satisfy the lint would put this type and its columns one \
              translation apart, which is the distance a mis-scoped insert hides in"
)]
pub struct NewJournalRow {
    /// The run.
    pub run_id: Uuid,
    /// The price row this run selected.
    pub price_id: Uuid,
    /// RLS scope. **Written by the expansion, never taken from a request** — and
    /// the schema holds the same rule from the other side, so the
    /// two agree rather than the discipline standing alone.
    ///
    /// `fk_pricing_repricing_journal_run` covers `run_id` alone, so the key is not
    /// what stops a journal row carrying a foreign tenant — the table's own trigger
    /// is. It looks the run up and refuses the insert when its `tenant_id` differs:
    /// the PL/pgSQL `IF FOUND AND run_tenant IS DISTINCT FROM NEW.tenant_id` arm on
    /// Postgres, and
    /// `trg_pricing_repricing_journal_same_tenant_as_its_run` on `SQLite`. Both are
    /// conditioned on the run *existing*, so a row naming no run is still the
    /// foreign key's refusal and not theirs.
    ///
    /// The discipline stays where it is regardless: a caller that has to be told by
    /// a trigger which tenant it is writing for has already lost the scope it was
    /// handed, and the refusal reaches it as a storage failure rather than as a
    /// scoped answer.
    pub tenant_id: Uuid,
}

/// Freeze a run's selected row set into the journal, all rows born `pending`.
///
/// **A loop of inserts on the caller's runner, and the atomicity is the
/// caller's.** The set is the run's frozen membership, so a partial insert would
/// leave a run whose completion predicate — no `pending` rows remain — is
/// satisfiable by rows that were never selected. This module cannot make that
/// impossible on its own, because it opens no transaction by design (see the
/// module doc); what it can do is say so, and the expansion that calls it is
/// obliged to hold a transaction across the whole set.
///
/// It is deliberately *not* `take_locks`' shape. That one is a loop on an
/// **autocommit** connection and releases what it took on a refusal, because a
/// lock's holder has to be readable after the failed insert. Here the caller owns
/// a transaction and a rollback is the release.
///
/// # Errors
/// [`RepoError::Db`] on a scope or storage failure, which includes the trigger's
/// refusal of a birth state that is not `pending`, its refusal of a journal row
/// under a run that is not `kind = repricing`, and either foreign key's refusal of
/// an id that does not exist.
pub async fn open_rows(
    runner: &impl DBRunner,
    scope: &AccessScope,
    rows: &[NewJournalRow],
) -> Result<(), RepoError> {
    for new in rows {
        let row = repricing_journal::ActiveModel {
            run_id: Set(new.run_id),
            price_id: Set(new.price_id),
            tenant_id: Set(new.tenant_id),
            state: Set(JournalState::Pending.as_str().to_owned()),
            failure_reason: Set(None),
            applied_price_id: Set(None),
            applied_at: Set(None),
        };
        repricing_journal::Entity::insert(row.clone())
            .secure()
            .scope_with_model(scope, &row)
            .map_err(|e| RepoError::Db(format!("pricing_repricing_journal scope: {e}")))?
            .exec(runner)
            .await
            .map_err(|e| RepoError::Db(format!("insert pricing_repricing_journal: {e}")))?;
    }
    Ok(())
}

/// Record that a row applied, naming the successor it created and when.
///
/// **Both are written together and neither is optional here**, because the table
/// asserts each against the state per column: a pending row carrying a successor
/// id with a null instant is admitted by the pair-wise spelling and would be
/// applied a second time by the re-drive.
///
/// **The swap is the guard, and `pending` is part of it.** A statement matching
/// zero rows — the wrong `(run_id, price_id)` pair, a row the scope gate filtered
/// out, a row already decided — is refused rather than answered `Ok(())`; the
/// caller here is the re-drive, and a silent no-op would leave the row `pending`
/// for it to apply a second time, minting a second successor on the key.
///
/// The third case is the one this predicate carries a conjunct for. Without it
/// [`row_of`]'s three equalities **match** a decided row, and what refuses the
/// write is `trg_pricing_repricing_journal_decided_is_final` — whose `RAISE`
/// reaches the caller as [`RepoError::Db`], a 500, for what is a race another pass
/// won. Every other state-moving write in this layer carries its premise in the
/// statement (`bulk_repo::advance`, `approval_repo::swap`,
/// `window_repo::transition` and ten more), so that the swap does the refusing and
/// not the trigger.
///
/// The trigger stays and stays load-bearing: it is what makes a decided row final
/// against **any** writer, including one that never came through this function.
/// The conjunct only decides which of the two speaks first, and therefore what the
/// caller is told.
///
/// # Errors
/// [`RepoError::ConcurrentMutation`] when the row exists and is no longer
/// `pending`, naming the state it reads now; [`RepoError::NotFound`] when there is
/// no such row at all; [`RepoError::Db`] on a scope or storage failure, including
/// the `CHECK` refusing a successor that wears the selected row's own id.
pub async fn mark_applied(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    run_id: Uuid,
    price_id: Uuid,
    applied_price_id: Uuid,
    applied_at: OffsetDateTime,
) -> Result<(), RepoError> {
    let affected = repricing_journal::Entity::update_many()
        .secure()
        .scope_with(scope)
        .col_expr(
            repricing_journal::Column::State,
            sea_orm::sea_query::Expr::value(JournalState::Applied.as_str()),
        )
        .col_expr(
            repricing_journal::Column::AppliedPriceId,
            sea_orm::sea_query::Expr::value(applied_price_id),
        )
        .col_expr(
            repricing_journal::Column::AppliedAt,
            sea_orm::sea_query::Expr::value(applied_at),
        )
        .filter(pending_row_of(tenant_id, run_id, price_id))
        .exec(runner)
        .await
        .map_err(|e| RepoError::Db(format!("apply pricing_repricing_journal: {e}")))?
        .rows_affected;
    if affected == 0 {
        return Err(swap_refusal(runner, scope, tenant_id, run_id, price_id).await);
    }
    Ok(())
}

/// Record that a row did not apply, with the reason an operator reads.
///
/// The reason is **shared with every other row of its plan** when the D-134
/// plan-level pass is what refused it, which is why this takes a `&str` a caller
/// can hand to several rows rather than deriving one per row.
///
/// # Errors
/// Exactly [`mark_applied`]'s.
pub async fn mark_failed(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    run_id: Uuid,
    price_id: Uuid,
    reason: &str,
) -> Result<(), RepoError> {
    let affected = repricing_journal::Entity::update_many()
        .secure()
        .scope_with(scope)
        .col_expr(
            repricing_journal::Column::State,
            sea_orm::sea_query::Expr::value(JournalState::Failed.as_str()),
        )
        .col_expr(
            repricing_journal::Column::FailureReason,
            sea_orm::sea_query::Expr::value(reason),
        )
        .filter(pending_row_of(tenant_id, run_id, price_id))
        .exec(runner)
        .await
        .map_err(|e| RepoError::Db(format!("fail pricing_repricing_journal: {e}")))?
        .rows_affected;
    if affected == 0 {
        return Err(swap_refusal(runner, scope, tenant_id, run_id, price_id).await);
    }
    Ok(())
}

/// A run's whole journal, in `price_id` order.
///
/// **Unbounded**: a bare `.all(runner)`, with no `.limit(...)` anywhere in this
/// function. A run's size is whatever `price_repo::load_published_for_selector` froze
/// it at, and that selector is itself unbounded (`price_repo.rs`, "There is no page
/// bound, and that is a real edge") — so this read has to be, or it would silently
/// report a prefix of a run as the whole of it.
///
/// Ordered so a report rendered from it is stable across reads — the table has no
/// secondary index (D-261), so the ordering is the caller's stability rather than the
/// store's optimisation.
///
/// # Errors
/// [`RepoError::Db`] on a scope or storage failure; [`RepoError::CorruptRow`] for
/// a stored state token no `CHECK` should have admitted.
pub async fn list_for_run(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    run_id: Uuid,
) -> Result<Vec<JournalRow>, RepoError> {
    read_run(runner, scope, tenant_id, run_id, None).await
}

/// The run's rows, optionally narrowed to one state.
///
/// One query for both readers, because the state predicate belongs in it: reading
/// the whole journal and dropping the decided rows in Rust carries every applied
/// row of a re-driven run across the wire, for a run whose last row is pending.
async fn read_run(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    run_id: Uuid,
    state: Option<JournalState>,
) -> Result<Vec<JournalRow>, RepoError> {
    let mut filter = Condition::all()
        .add(repricing_journal::Column::TenantId.eq(tenant_id))
        .add(repricing_journal::Column::RunId.eq(run_id));
    if let Some(state) = state {
        filter = filter.add(repricing_journal::Column::State.eq(state.as_str()));
    }
    let rows = repricing_journal::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(filter)
        .order_by(repricing_journal::Column::PriceId, Order::Asc)
        .all(runner)
        .await
        .map_err(|e| RepoError::Db(format!("read pricing_repricing_journal: {e}")))?;
    let journal: Vec<JournalRow> = rows.iter().map(record_of).collect::<Result<_, _>>()?;
    Ok(journal)
}

/// The rows a re-drive still has to reach.
///
/// **The re-drive's whole safety is this predicate**: a decided row is frozen by
/// the trigger, so a crashed run resumed by asking for `pending` rows cannot apply
/// anything twice — and it cannot silently skip a row either, because `pending` is
/// what a row that was never reached still holds.
///
/// # Errors
/// Exactly [`list_for_run`]'s.
pub async fn pending_for_run(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    run_id: Uuid,
) -> Result<Vec<JournalRow>, RepoError> {
    read_run(
        runner,
        scope,
        tenant_id,
        run_id,
        Some(JournalState::Pending),
    )
    .await
}

/// The composite key, spelled once. Carries `tenant_id` beside `run_id` and
/// `price_id`, the belt-and-braces every sibling filter in this layer already
/// has — `.scope_with(scope)` is the RLS gate, so this is not load-bearing for
/// isolation on its own, but a predicate that omits it is the one filter here
/// that would not stop at a foreign row if the gate above it ever did.
///
/// Identity only: it is what [`swap_refusal`] re-reads by, and it is deliberately
/// **not** what either swap writes by — see [`pending_row_of`].
fn row_of(tenant_id: Uuid, run_id: Uuid, price_id: Uuid) -> Condition {
    Condition::all()
        .add(repricing_journal::Column::TenantId.eq(tenant_id))
        .add(repricing_journal::Column::RunId.eq(run_id))
        .add(repricing_journal::Column::PriceId.eq(price_id))
}

/// [`row_of`] plus the premise both swaps hold: the row is still undecided.
///
/// The premise is in the statement rather than in a preceding read, for the reason
/// this layer states everywhere it does the same thing — a read-then-write admits
/// both of two concurrent callers, because between the read and the write nothing
/// holds the row.
fn pending_row_of(tenant_id: Uuid, run_id: Uuid, price_id: Uuid) -> Condition {
    row_of(tenant_id, run_id, price_id)
        .add(repricing_journal::Column::State.eq(JournalState::Pending.as_str()))
}

/// Why a swap matched no row: it is gone, or somebody else decided it.
///
/// Read **after** the statement and only to name the refusal — the swap is still
/// the guard, and this read cannot change what was written. Same shape and same
/// reason as `bulk_repo::advance`'s zero-row arm.
///
/// A read that itself fails is returned as-is rather than folded into a `NotFound`,
/// so a storage fault is never reported as an absent row.
async fn swap_refusal(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    run_id: Uuid,
    price_id: Uuid,
) -> RepoError {
    let fresh = repricing_journal::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(row_of(tenant_id, run_id, price_id))
        .all(runner)
        .await
        .map_err(|e| RepoError::Db(format!("read pricing_repricing_journal: {e}")));
    match fresh {
        Err(e) => e,
        Ok(rows) => match rows.first().map(record_of) {
            None => RepoError::NotFound {
                subject: "repricing journal row".to_owned(),
                id: format!("{run_id}/{price_id}"),
            },
            Some(Err(e)) => e,
            Some(Ok(row)) => RepoError::ConcurrentMutation {
                aggregate: format!(
                    "repricing journal row {run_id}/{price_id}: the swap names a row that now \
                     reads {}",
                    row.state.as_str()
                ),
            },
        },
    }
}

/// A stored row as this crate's vocabulary.
fn record_of(row: &repricing_journal::Model) -> Result<JournalRow, RepoError> {
    Ok(JournalRow {
        run_id: row.run_id,
        price_id: row.price_id,
        tenant_id: row.tenant_id,
        state: JournalState::parse(&row.state).map_err(|e| {
            RepoError::CorruptRow(format!(
                "pricing_repricing_journal ({}, {}): {e}",
                row.run_id, row.price_id
            ))
        })?,
        failure_reason: row.failure_reason.clone(),
        applied_price_id: row.applied_price_id,
        applied_at: row.applied_at,
    })
}
