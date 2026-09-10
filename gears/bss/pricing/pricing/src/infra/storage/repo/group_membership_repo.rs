//! Reads and writes of `pricing_group_membership` — the operator-facing half
//! of D-09's non-overlap invariant
//! (`design/09-price-overlays.md` §3 `inst-cg-record` / `inst-cg-resolve`,
//! `inst-mm-audit`, `inst-ms-time`).
//!
//! Free functions taking a **runner** rather than a provider,
//! [`window_repo`](super::window_repo)'s reason and one of this store's own:
//! [`refuse_overlap`] has to run inside the same transaction as the insert it
//! guards, and the audit record every mutation writes (`inst-mm-audit`) has to
//! commit with it — a repository holding a provider could not join either,
//! `Db::conn()` being refused outright inside an open transaction.
//!
//! # The invariant now lives in two layers, deliberately
//!
//! `pricing_group_membership`'s migration doc states why this table is *not*
//! [`window_repo`]'s situation: the collision domain is `(tenant_id,
//! payer_tenant_id)` — both columns of **this row** — so, unlike the canonical
//! scope key a window's non-overlap is judged against, the declarative form is
//! available here, and the migration carries it physically on both engines (a
//! Postgres `EXCLUDE USING gist`, a paired `SQLite` trigger). That constraint
//! is the **backstop**: it is what a concurrent writer that stepped past this
//! module's own check would still meet, which is exactly the promise
//! [`window_repo::refuse_overlap`]'s doc records that a repository-only guard
//! **cannot** make on its own — "an invariant a concurrent writer can step
//! through is not an invariant". Two layers is not redundancy here; it is what
//! makes the *whole* of D-09 an invariant rather than only its happy path.
//!
//! [`refuse_overlap`] below is the other layer, and it exists for a different
//! reason than correctness: the constraint refuses the row, but a caller who
//! meets it head-on reads a driver's constraint-violation string — a 500, on
//! the shape every other guarded table in this crate takes before its own
//! pre-check lands (`window_repo`'s history-immutability note is the same
//! story one table over). This module reads the payer's existing memberships
//! and answers **by name** — `MEMBERSHIP_OVERLAP` or `MEMBERSHIP_CONFLICT`,
//! §5's own two codes — before the statement that would trip the constraint is
//! even issued.
//!
//! **Where the two disagree the database wins, and this module does not
//! pretend otherwise.** A race that lands between this check's read and its
//! own insert — two concurrent enrollments of one payer, neither's read seeing
//! the other's write — is exactly the shape `window_repo::refuse_overlap`
//! documents as unserializable from this crate with the tools available to it,
//! and that limit is unchanged here: this function holds `&impl DBRunner`, and
//! `toolkit_db`'s advisory locks are file-based (not DB-native) and
//! `SecureSelect` exposes no row locking. What is different from the window
//! plane is the *consequence* of losing that race: there, an unserialized
//! writer could commit an overlapping row outright, because nothing else was
//! watching. Here, the exclusion constraint / trigger this migration wrote is
//! still watching, and the loser's `INSERT` is refused **physically** — the
//! data can never disagree with D-09, only the *message* the loser reads can
//! be worse than this module's own. That message is not parsed back into
//! `MembershipOverlap` / `MembershipConflict` here: neither backend's
//! constraint-violation rendering is this crate's to parse (the Postgres
//! `EXCLUDE` raises SQLSTATE `23P01`, not the unique-violation class
//! `contention_or_db` already recognises, and the `SQLite` trigger's
//! `RAISE(ABORT, …)` carries only a message string), so a race that slips past
//! this pre-check surfaces as [`RepoError::Db`] — a 500 — for that narrow
//! window. That is the same residue `window_repo`'s history-immutability guard
//! left before its own pre-check existed, reported rather than silently
//! assumed away.
//!
//! # What is deliberately not built here
//!
//! **Taxonomy validation.** `GROUP_UNKNOWN` (§5, `inst-cg-taxonomy`'s "values
//! validated at authoring") is not checked in this module: this is the storage
//! layer for the membership plane and it has no dependency on
//! `customer_group_taxonomy`'s own repository arm. A route that authors an
//! enrollment owes that check before calling [`enroll`].
//!
//! **The `If-Match` precondition on `row_version`.** [`end_membership`] takes
//! `expected_version` and puts it in the `UPDATE`'s own `WHERE` beside the row
//! id, `window_repo::adjust_effective_to`'s arrangement (D-191): a caller whose
//! read predates a concurrent editor's write loses the race at the statement
//! rather than silently overwriting it. Wired by the membership route task
//! (`design/09-price-overlays.md` §5's `PATCH.../members/{id}`), which is the
//! caller `group_membership::Model::row_version`'s own doc names.
//!
//! **The atomic move operation** (`inst-ms-move`, D-09: "end the active
//! membership + create the new one at the same instant"). Composing that out
//! of [`enroll`] and [`end_membership`] inside one transaction is a route's
//! job, not this module's; both primitives are transaction-agnostic exactly so
//! a caller can do that.

use sea_orm::ActiveValue::Set;
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, EntityTrait, Order};
use time::OffsetDateTime;
use toolkit_db::odata::sea_orm_filter::paginate_odata;
use toolkit_db::secure::{
    AccessScope, DBRunner, SecureEntityExt, SecureInsertExt, SecureUpdateExt,
};
use toolkit_odata::{ODataQuery, Page, SortDir};
use uuid::Uuid;

use bss_pricing_sdk::odata::MembershipFilterField;

use crate::domain::audit::{AuditAction, AuditStamp, AuditSubjectKind};
use crate::domain::instant::format_rfc3339;
use crate::infra::storage::entity::group_membership;
use crate::infra::storage::odata_mapping::{
    LIST_LIMIT_CFG, MembershipODataMapper, OdataPageError, domain_page, map_odata_err,
    query_with_default_order,
};
use crate::infra::storage::repo::{audit_repo, check_authored_instant};
use crate::infra::storage::{RepoError, contention_or_db};

/// A membership to enroll.
///
/// Carries no `created_by` and no `created_at`: those are the [`AuditStamp`]'s,
/// [`crate::infra::storage::repo::window_repo::NewWindow`]'s reason — the
/// caller's request and the caller's identity are two different things to
/// authenticate, and only one of them belongs on the wire body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewMembership {
    /// The membership's durable name, minted by the caller so an authoring
    /// surface can return it before the row is durable.
    pub membership_id: Uuid,
    /// RLS scope.
    pub tenant_id: Uuid,
    /// The payer's commercial-profile key (`inst-cg-record`). AMS supplies
    /// this identity; tenant topology is never modified.
    pub payer_tenant_id: Uuid,
    /// Taxonomy value — not validated here; see the module doc.
    pub group_value: String,
    /// Inclusive start of the half-open interval, UTC.
    pub effective_from: OffsetDateTime,
    /// Exclusive end, UTC; `None` is open-ended.
    pub effective_to: Option<OffsetDateTime>,
}

/// One membership, read back into the vocabulary the rest of the system uses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MembershipRow {
    /// The membership's durable name.
    pub membership_id: Uuid,
    /// RLS scope.
    pub tenant_id: Uuid,
    /// The payer this interval covers.
    pub payer_tenant_id: Uuid,
    /// The taxonomy value the payer is enrolled in over this interval.
    pub group_value: String,
    /// Inclusive start, UTC.
    pub effective_from: OffsetDateTime,
    /// Exclusive end, UTC; `None` is open-ended — a membership not (yet)
    /// ended.
    pub effective_to: Option<OffsetDateTime>,
    /// Who recorded it — pseudonymous principal id.
    pub created_by: Uuid,
    /// When it was recorded, UTC.
    pub created_at: OffsetDateTime,
    /// The row's concurrency token. See the module doc: nothing in this module
    /// compares against it yet.
    pub row_version: u64,
}

/// Enroll a payer into a group, refusing an interval that overlaps any of
/// their existing memberships — same group or not (D-09).
///
/// Two statements in the caller's transaction and the order is the guarantee:
/// read every membership already on the payer, then insert. A caller that
/// inserted first and checked after would have to undo a row the database's
/// own guard already refused to let land in the first place.
///
/// # Errors
/// [`RepoError::TimestampPrecisionExceeded`] on an authored instant finer than
/// the millisecond quantum (D-144). [`RepoError::MembershipIntervalEmpty`]
/// when the end is not strictly after the start.
/// [`RepoError::MembershipOverlap`] when the interval intersects an existing
/// membership **in the same group**. [`RepoError::MembershipConflict`] when it
/// intersects one **in a different group** — D-09's own case, and the one the
/// atomic move operation exists to remedy instead of a plain enrollment.
/// [`RepoError::ConcurrentMutation`] when `membership_id` is already taken.
/// [`RepoError::Db`] on a scope or storage failure, including the database's
/// own guard refusing a race this function's pre-check did not see (see the
/// module doc).
#[tracing::instrument(
    skip_all,
    fields(tenant_id = %tenant_id, membership_id = %new.membership_id, payer_tenant_id = %new.payer_tenant_id)
)]
pub async fn enroll(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    new: NewMembership,
    stamp: AuditStamp,
) -> Result<MembershipRow, RepoError> {
    check_authored_instant("effectiveFrom", Some(new.effective_from))?;
    check_authored_instant("effectiveTo", new.effective_to)?;
    refuse_empty_interval(new.effective_from, new.effective_to)?;

    refuse_overlap(
        runner,
        scope,
        tenant_id,
        new.payer_tenant_id,
        &new.group_value,
        new.effective_from,
        new.effective_to,
        None,
    )
    .await?;

    let am = group_membership::ActiveModel {
        membership_id: Set(new.membership_id),
        tenant_id: Set(tenant_id),
        payer_tenant_id: Set(new.payer_tenant_id),
        group_value: Set(new.group_value.clone()),
        effective_from: Set(new.effective_from),
        effective_to: Set(new.effective_to),
        created_by: Set(stamp.actor_principal_id),
        created_at_utc: Set(stamp.recorded_at),
        // Act zero, `window_repo::schedule`'s reason: the enrollment **is** the
        // first act on this row, so there is no earlier version to disagree
        // with.
        row_version: Set(0),
    };
    group_membership::Entity::insert(am.clone())
        .secure()
        .scope_with_model(scope, &am)
        .map_err(|e| RepoError::Db(format!("pricing_group_membership scope: {e}")))?
        .exec(runner)
        .await
        .map_err(|e| {
            contention_or_db(
                &e,
                &format!("membership {}", new.membership_id),
                "insert pricing_group_membership",
            )
        })?;

    record_membership_mutation(
        runner,
        scope,
        tenant_id,
        new.payer_tenant_id,
        new.membership_id,
        AuditAction::Create,
        None,
        Some(after_state(
            &new.group_value,
            new.effective_from,
            new.effective_to,
        )),
        stamp,
    )
    .await?;

    Ok(MembershipRow {
        membership_id: new.membership_id,
        tenant_id,
        payer_tenant_id: new.payer_tenant_id,
        group_value: new.group_value,
        effective_from: new.effective_from,
        effective_to: new.effective_to,
        created_by: stamp.actor_principal_id,
        created_at: stamp.recorded_at,
        row_version: 0,
    })
}

/// End a membership — `inst-ms-time`'s "ending early = setting `to`". Records
/// are never mutated in place otherwise; history is retained.
///
/// The overlap check runs again, excluding the row's own previous self,
/// [`window_repo::adjust_effective_to`](super::window_repo::adjust_effective_to)'s
/// reason one plane over: `at` is a caller-supplied instant and not
/// constrained here to be *earlier* than the row's current end, so treating
/// this as pure narrowing would be a promise the signature does not make.
///
/// `expected_version` rides the `UPDATE`'s own `WHERE`, [`window_repo::adjust_effective_to`]'s
/// arrangement (D-191): the caller's precondition is presented, never re-read
/// and compared beforehand, because a tag read and then handed to a statement
/// is a decision racing the write it authorizes.
///
/// # Errors
/// [`RepoError::NotFound`] when no membership in scope answers to
/// `membership_id` — which is what a foreign tenant sees, deliberately
/// indistinguishable from absence. [`RepoError::TimestampPrecisionExceeded`] on
/// an instant finer than the millisecond quantum.
/// [`RepoError::MembershipIntervalEmpty`] when `at` is not strictly after the
/// row's `effective_from`. [`RepoError::MembershipHistorical`] when the row's
/// stored end had already passed when the act was recorded — an elapsed interval
/// is not ended early, it is rewritten. [`RepoError::MembershipOverlap`] /
/// [`RepoError::MembershipConflict`] as [`enroll`]'s.
/// [`RepoError::StaleRowVersion`] when `expected_version` no longer matches the
/// stored row. [`RepoError::Db`] on a scope or storage failure.
#[tracing::instrument(skip_all, fields(tenant_id = %tenant_id, membership_id = %membership_id))]
pub async fn end_membership(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    membership_id: Uuid,
    at: OffsetDateTime,
    expected_version: u64,
    stamp: AuditStamp,
) -> Result<MembershipRow, RepoError> {
    check_authored_instant("effectiveTo", Some(at))?;
    let current = require(runner, scope, tenant_id, membership_id).await?;
    refuse_empty_interval(current.effective_from, Some(at))?;
    refuse_frozen_end(&current, membership_id, stamp.recorded_at)?;

    refuse_overlap(
        runner,
        scope,
        tenant_id,
        current.payer_tenant_id,
        &current.group_value,
        current.effective_from,
        Some(at),
        Some(membership_id),
    )
    .await?;

    let next_version = current.row_version.saturating_add(1);
    let stored_version = i64::try_from(next_version).map_err(|_| {
        RepoError::CorruptRow(format!(
            "pricing_group_membership {membership_id} row_version would be {next_version}, \
             which is past what the column can hold"
        ))
    })?;
    let stored_expected = i64::try_from(expected_version).map_err(|_| {
        RepoError::CorruptRow(format!(
            "pricing_group_membership {membership_id} was asked to compare against row version \
             {expected_version}, which is past what the column can hold"
        ))
    })?;

    let result = group_membership::Entity::update_many()
        .secure()
        .scope_with(scope)
        .col_expr(group_membership::Column::EffectiveTo, Expr::value(Some(at)))
        .col_expr(
            group_membership::Column::RowVersion,
            Expr::value(stored_version),
        )
        .filter(
            Condition::all()
                .add(group_membership::Column::TenantId.eq(tenant_id))
                .add(group_membership::Column::MembershipId.eq(membership_id))
                .add(group_membership::Column::RowVersion.eq(stored_expected)),
        )
        .exec(runner)
        .await
        .map_err(|e| RepoError::Db(format!("end pricing_group_membership {membership_id}: {e}")))?;

    if result.rows_affected == 0 {
        // The row is known to exist (`require` above proved it); the only way
        // the `WHERE` can have matched nothing is that `row_version` has moved
        // since the caller read it — `window_repo::adjust_effective_to`'s own
        // diagnosis, re-read rather than guessed at.
        let fresh = require(runner, scope, tenant_id, membership_id).await?;
        return Err(RepoError::StaleRowVersion {
            subject: "membership".to_owned(),
            id: membership_id.to_string(),
            current: fresh.row_version,
            submitted: expected_version,
        });
    }

    record_membership_mutation(
        runner,
        scope,
        tenant_id,
        current.payer_tenant_id,
        membership_id,
        AuditAction::Update,
        Some(after_state(
            &current.group_value,
            current.effective_from,
            current.effective_to,
        )),
        Some(after_state(
            &current.group_value,
            current.effective_from,
            Some(at),
        )),
        stamp,
    )
    .await?;

    Ok(MembershipRow {
        effective_to: Some(at),
        row_version: next_version,
        ..current
    })
}

/// Every membership a payer has ever held, oldest interval first.
///
/// The read [`inst-cg-resolve`]'s "the group at `t` = the membership interval
/// covering `t`" is answered against, and the same one an operator's history
/// view needs — [`window_repo::list_for_plan`](super::window_repo::list_for_plan)'s
/// reason for including terminal rows: **every** membership is returned,
/// ended or not, because a resolution walk over the payer's whole timeline and
/// an operator asking "what has this payer been enrolled in" are both this
/// read's business, and pre-filtering would only move the filter to a second
/// caller.
///
/// Ordered by `effective_from` then `membership_id`,
/// [`window_repo::list_for_plan`]'s reason: every consumer of this set walks
/// the payer's intervals in time order, and the id breaks a tie two rows may
/// legitimately share deterministically rather than by storage-engine paging
/// order.
///
/// # Errors
/// [`RepoError::Db`] on a scope or storage failure.
pub async fn intervals_for_payer(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    payer_tenant_id: Uuid,
) -> Result<Vec<MembershipRow>, RepoError> {
    let rows = load_for_payer(runner, scope, tenant_id, payer_tenant_id).await?;
    rows.into_iter().map(to_domain).collect()
}

/// One page of the memberships recorded **in one group** (D-322), optionally
/// narrowed to one payer.
///
/// [`intervals_for_payer`]'s mirror across the other axis of the same table, and
/// it exists for the reason that read's doc already gives for including ended
/// rows: an operator asking *"who is in this group"* and an auditor asking *"who
/// has been"* are the same read, and pre-filtering would only move the filter to
/// a second caller. Slice 9 names an `actor-auditor` who *"reads membership audit
/// history"* and gave that actor no surface; this is the store half of it.
///
/// # The walk is keyed on the pair the decision names, and both halves are needed
///
/// **Bounded**, against `api/rest.rs`'s own opening sentence that every collection
/// surface paginates on an opaque cursor (D-125). An `.all(runner)` with no
/// `LIMIT` here answers with every membership ever recorded in the group, and the
/// exposure is a function of the table's design rather than of its traffic:
/// memberships are effective-dated and ended rows are deliberately kept, so a
/// group's row count grows monotonically over a ≥7-year retention and is never
/// pruned.
///
/// The first paginated version dropped the interval order and walked
/// `membership_id` alone, on a premise stated here and true when it was written:
/// **a keyset walk must order by the key its cursor names**, and `cursor::decode`
/// named one `Uuid`. That premise is what changed.
/// [`crate::api::rest::cursor::IntervalPageRequest`] names the pair, so the walk is
/// D-322 clause 4's order — `(effective_from, membership_id)` — and the cursor
/// resumes from both columns.
///
/// Restoring the order **without** the cursor would have been the worst of the three
/// states rather than a partial fix: a walk whose sort key and resume key disagree
/// skips and repeats rows, and the suite's paging probe cannot see it, because it
/// compares the concatenated walk against the whole set and deliberately asserts no
/// order. The two move together or not at all.
///
/// **Why the pair and not the instant alone.** `effective_from` is *not* unique —
/// two memberships may legitimately begin at one instant — and a keyset cursor over
/// a non-unique column either loses the tied rows or repeats them. `membership_id`
/// is the table's primary key, so the pair is total.
///
/// **Why the order matters at all**, given that every row carries its own
/// `effective_from`: `membership_id` is a `Uuid::now_v7()` minted at the request, so
/// ordering by it is ordering by **write** time. An operator enrolling a payer today
/// with last month's `effectiveFrom` produces a row that sorts after one taking
/// effect later, and the reader D-322 names is an auditor answering *"who has been
/// in this group"* — a question asked in effective-date order. A reader can sort one
/// page it already holds; across pages only the walk's own order can answer it.
///
/// # Errors
/// [`RepoError::Db`] on a scope or storage failure.
pub async fn memberships_in_group(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    group_value: &str,
    payer_tenant_id: Option<Uuid>,
    after: Option<(OffsetDateTime, Uuid)>,
    limit: u64,
) -> Result<Vec<MembershipRow>, RepoError> {
    let mut filter = Condition::all()
        .add(group_membership::Column::TenantId.eq(tenant_id))
        .add(group_membership::Column::GroupValue.eq(group_value));
    if let Some(payer) = payer_tenant_id {
        filter = filter.add(group_membership::Column::PayerTenantId.eq(payer));
    }
    if let Some((from, id)) = after {
        // "Strictly after `(from, id)`", spelled out as a disjunction rather than as
        // a row-value comparison: `(a, b) > (c, d)` is standard SQL and both engines
        // take it, but sea-orm has no builder for it and a raw fragment would put a
        // dialect-shaped string into a repository that holds none.
        filter = filter.add(
            Condition::any()
                .add(group_membership::Column::EffectiveFrom.gt(from))
                .add(
                    Condition::all()
                        .add(group_membership::Column::EffectiveFrom.eq(from))
                        .add(group_membership::Column::MembershipId.gt(id)),
                ),
        );
    }
    let rows = group_membership::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(filter)
        .order_by(group_membership::Column::EffectiveFrom, Order::Asc)
        .order_by(group_membership::Column::MembershipId, Order::Asc)
        .limit(limit)
        .all(runner)
        .await
        .map_err(|e| RepoError::Db(format!("list pricing_group_membership: {e}")))?;
    rows.into_iter().map(to_domain).collect()
}

/// One `OData` page of a group's memberships. Path `{group}` stays in the SQL
/// scope. Default order is `effective_from asc, membership_id asc`.
///
/// # Errors
/// [`OdataPageError::Db`] on storage failure; [`OdataPageError::Odata`] on a
/// malformed `$filter` / `$orderby` / cursor.
pub async fn list_odata(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    group_value: &str,
    query: &ODataQuery,
) -> Result<Page<MembershipRow>, OdataPageError> {
    let base_select = group_membership::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(group_membership::Column::TenantId.eq(tenant_id))
                .add(group_membership::Column::GroupValue.eq(group_value)),
        );
    let query = query_with_default_order(
        query,
        &[
            MembershipFilterField::EffectiveFrom,
            MembershipFilterField::MembershipId,
        ],
    );
    let page = paginate_odata::<
        MembershipFilterField,
        MembershipODataMapper,
        group_membership::Entity,
        group_membership::Model,
        _,
        _,
    >(
        base_select,
        runner,
        &query,
        ("membership_id", SortDir::Asc),
        LIST_LIMIT_CFG,
        |m| m,
    )
    .await
    .map_err(map_odata_err)?;
    domain_page(page, to_domain)
}

/// `inst-cg-resolve`'s narrowing rule: which of the payer's membership
/// intervals covers `at`, if any.
///
/// Pure — no repository access, layered over [`intervals_for_payer`]'s output
/// rather than reading anything itself, so the resolution rule is testable
/// without a store. `window::WindowInterval::covers`'s half-open reading,
/// verbatim, over this table's own pair of instants: `effective_from` is
/// included, `effective_to` is excluded, and `effective_to = None` reads as
/// open-ended.
///
/// # What this function is not
///
/// It does not compose a `pricingSnapshotRef` and it does not freeze anything.
/// `design/09-price-overlays.md` draws that seam at Tariffs three times — §1.7
/// (`:110`), `inst-gm-return` (`:173`) and D-30 (`:466`) — and all three say the
/// same thing: the catalog resolves nothing *for a subscription* and stamps no
/// snapshot. This is the narrowing arithmetic alone, callable by whichever
/// gear needs "the interval covering `t`" without needing this crate's store.
///
/// # Which interval wins when more than one could
///
/// D-09's non-overlap invariant (`refuse_overlap`, backstopped physically by
/// `pricing_group_membership`'s exclusion constraint / trigger) makes this a
/// non-question over a real payer's stored intervals: at most one interval in
/// the same group can cover any instant, and cross-group overlap is refused
/// outright, so `intervals_for_payer`'s own output never presents this
/// function with two covering candidates. It is not written to lean on that
/// guarantee, though: it returns the **first** covering interval in
/// `intervals`' own order, so a caller that hands it a synthetic or
/// already-filtered slice — a test, or a future caller assembling candidates
/// from more than one source — gets a deterministic answer rather than one
/// that depends on iteration order silently matching insertion order.
#[must_use]
pub fn resolve_active_membership(
    intervals: &[MembershipRow],
    at: OffsetDateTime,
) -> Option<&MembershipRow> {
    intervals
        .iter()
        .find(|row| row.effective_from <= at && row.effective_to.is_none_or(|end| at < end))
}

/// Append this membership mutation's audit record — D-14, `inst-mm-audit`.
///
/// Called **inside** each mutation's own transaction: a record that commits
/// with its mutation cannot be lost by a crash between the two, and a failure
/// to write it rolls the mutation back rather than leaving a trail that is
/// silently incomplete — `overlay_repo::record_overlay_mutation`'s
/// arrangement, one plane over.
///
/// The chain is [`audit_repo::payer_chain`] and not [`audit_repo::plan_chain`]:
/// see the module doc's chain note.
#[allow(
    clippy::too_many_arguments,
    reason = "every argument is a fact only the caller holds, `approval_repo::decide`'s own \
              justification for the same lint one construct over. `tenant_id` and \
              `payer_tenant_id` are two different axes and not one: the first is the RLS scope \
              this call is made under, the second is `audit_repo::payer_chain`'s key, and \
              folding them into one value would make the chain computation silently wrong for \
              whichever axis got dropped. `membership_id` is the `subject_ref`; `action`, \
              `before_state` and `after_state` are three of `NewAuditEntry`'s own fields, not a \
              second grouping invented here — this function's whole job is assembling that \
              struct, and `chain_id` is the one field it cannot take from the caller because \
              only this function knows to derive it from `payer_tenant_id` rather than from \
              `tenant_id` alone. `stamp` is the actor/instant/correlation triple every mutation \
              in this crate threads through, unaudited-free by design."
)]
async fn record_membership_mutation(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    payer_tenant_id: Uuid,
    membership_id: Uuid,
    action: AuditAction,
    before_state: Option<serde_json::Value>,
    after_state: Option<serde_json::Value>,
    stamp: AuditStamp,
) -> Result<(), RepoError> {
    audit_repo::append(
        runner,
        scope,
        audit_repo::NewAuditEntry {
            tenant_id,
            chain_id: audit_repo::payer_chain(payer_tenant_id),
            recorded_at: stamp.recorded_at,
            actor_principal_id: stamp.actor_principal_id,
            action,
            subject_kind: AuditSubjectKind::Membership,
            subject_ref: audit_repo::membership_ref(membership_id),
            before_state,
            after_state,
            // No approval unit to name: membership mutations are not yet wired
            // to the Slice 5 approval plane (`inst-mm-pending`'s bulk-move case
            // is the one that would need it, and it is not built here).
            approval_ref: None,
            correlation_id: stamp.correlation_id,
        },
    )
    .await
    .map(|_| ())
}

/// A membership's group and interval, as the audit record's before/after
/// halves hold it.
///
/// One rendering for both the create and the end mutation —
/// [`crate::domain::audit::subject_state`]'s reason: a second one is a second
/// answer to "what did this row look like". Wire keys `camelCase`, as that
/// function's are.
fn after_state(
    group_value: &str,
    effective_from: OffsetDateTime,
    effective_to: Option<OffsetDateTime>,
) -> serde_json::Value {
    serde_json::json!({
        "groupValue": group_value,
        "effectiveFrom": format_rfc3339(effective_from),
        "effectiveTo": effective_to.map(format_rfc3339),
    })
}

/// Refuse an interval that intersects an occupying membership already on this
/// payer — D-09, across every group.
///
/// Every membership of the payer is a sibling here, unlike
/// [`window_repo::refuse_overlap`](super::window_repo::refuse_overlap)'s
/// per-canonical-scope-key walk: the collision domain is `(tenant_id,
/// payer_tenant_id)` alone, and every stored row is occupying — there is no
/// cancelled or superseded state on this table for a row to be excluded by
/// (`pricing_group_membership`'s migration doc: "no `state` column").
///
/// `except` drops the membership being ended, or [`end_membership`] narrowing
/// its own interval would collide with the interval it is replacing.
///
/// The code is chosen by comparing `group_value` against the collision it
/// found: the same value is [`RepoError::MembershipOverlap`] (§5's narrower
/// same-group code), any other value is [`RepoError::MembershipConflict`]
/// (D-09's own cross-group case) — one read answering both, because the
/// distinction is a property of the row this function already has in hand and
/// not a second query.
#[allow(
    clippy::too_many_arguments,
    reason = "window_repo::refuse_overlap holds seven of these under the same lint's threshold \
              because it resolves one already-assembled `&ScopeKey`; this function's collision \
              domain is `(tenant_id, payer_tenant_id)` with no existing domain type over that \
              pair to resolve into first, and `group_value` cannot fold into either — it is the \
              one field the whole function reads to decide which of the two D-09 codes applies, \
              not a third axis of the key. Minting a struct over `(payer_tenant_id, \
              group_value)` purely to satisfy this count would be exactly what \
              overlay_repo::replace_lines' own doc warns against: a type nothing else in this \
              crate ever carries together, existing only to satisfy a count. `from`/`to` are \
              the half-open interval every overlap check here keeps as two arguments \
              (`window_repo::refuse_overlap`'s own shape), and `except` is `end_membership`'s \
              self-exclusion, verbatim from that same sibling."
)]
async fn refuse_overlap(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    payer_tenant_id: Uuid,
    group_value: &str,
    from: OffsetDateTime,
    to: Option<OffsetDateTime>,
    except: Option<Uuid>,
) -> Result<(), RepoError> {
    for row in load_for_payer(runner, scope, tenant_id, payer_tenant_id).await? {
        if except == Some(row.membership_id) {
            continue;
        }
        if !intersects(from, to, row.effective_from, row.effective_to) {
            continue;
        }
        let payer_tenant_id = payer_tenant_id.to_string();
        let requested = render_interval(from, to);
        let conflicting = format!(
            "membership {} in group {} at {}",
            row.membership_id,
            row.group_value,
            render_interval(row.effective_from, row.effective_to)
        );
        return Err(if row.group_value == group_value {
            RepoError::MembershipOverlap {
                payer_tenant_id,
                requested,
                conflicting,
            }
        } else {
            RepoError::MembershipConflict {
                payer_tenant_id,
                requested,
                conflicting,
            }
        });
    }
    Ok(())
}

/// Do two half-open intervals `[from, to)` share an instant?
///
/// [`window_repo::intersects`](super::window_repo::intersects)'s arithmetic,
/// verbatim, over this table's own pair of instants rather than copied by
/// reference: `a.from < b.to_or_infinity && b.from < a.to_or_infinity`, with
/// `None` reading as infinity on either side and the strictness of both
/// comparisons the reason `effective_to == next.effective_from` does not
/// collide (the half-open reading `pricing_group_membership`'s migration doc and the
/// migration's own `[)` range spec both state).
fn intersects(
    a_from: OffsetDateTime,
    a_to: Option<OffsetDateTime>,
    b_from: OffsetDateTime,
    b_to: Option<OffsetDateTime>,
) -> bool {
    let a_before_b_ends = b_to.is_none_or(|end| a_from < end);
    let b_before_a_ends = a_to.is_none_or(|end| b_from < end);
    a_before_b_ends && b_before_a_ends
}

/// Refuse a move of an end that has already passed.
///
/// [`window_repo::refuse_frozen_end`](super::window_repo)'s `StoredEndPassed`
/// ground, one plane over: an elapsed interval is what a bill was computed from,
/// so re-ending it rewrites the past rather than ending something early.
/// `inst-ms-time` reads "ending early = setting `to`", and a row whose `to` is
/// behind the act cannot be ended early.
///
/// **Only that ground, and the deviation from the window plane is deliberate.**
/// `frozen_end` also refuses a *target* that is not in the future, which the
/// membership plane admits: `move_membership` forks on the clock and takes an
/// immediate arm for `effective_from <= now()`, so an end at or before the act's
/// own instant is the ordinary immediate move, and refusing it would leave the
/// move with no arm at all. The window plane has no such fork — a window's end is
/// scheduled — which is why the two rules differ where they do.
///
/// Measured against `stamp.recorded_at` rather than the wall clock, for the reason
/// every other authored instant here is the caller's: a store that read the clock
/// would judge one request differently depending on when its transaction ran.
fn refuse_frozen_end(
    current: &MembershipRow,
    membership_id: Uuid,
    recorded_at: OffsetDateTime,
) -> Result<(), RepoError> {
    let Some(stored) = current.effective_to else {
        return Ok(());
    };
    if stored > recorded_at {
        return Ok(());
    }
    Err(RepoError::MembershipHistorical {
        membership_id: membership_id.to_string(),
        frozen: format!(
            "its effective_to {} had already passed at {}; an elapsed interval is what a bill \
             was computed from",
            format_rfc3339(stored),
            format_rfc3339(recorded_at)
        ),
    })
}

/// Refuse an interval whose end is not strictly after its start.
///
/// [`window_repo::refuse_empty_interval`](super::window_repo)'s reason and
/// shape, over `chk_pricing_group_membership_interval`'s own CHECK.
fn refuse_empty_interval(
    from: OffsetDateTime,
    to: Option<OffsetDateTime>,
) -> Result<(), RepoError> {
    if to.is_none_or(|end| end > from) {
        return Ok(());
    }
    Err(RepoError::MembershipIntervalEmpty {
        requested: render_interval(from, to),
    })
}

/// One half-open interval as a refusal renders it —
/// [`window_repo::render_interval`](super::window_repo)'s reason: the bracket
/// asymmetry and the spelled-out open end are both the rule an operator is
/// being told about, not a formatting preference.
fn render_interval(from: OffsetDateTime, to: Option<OffsetDateTime>) -> String {
    match to {
        Some(to) => format!("[{}, {})", format_rfc3339(from), format_rfc3339(to)),
        None => format!("[{}, open-ended)", format_rfc3339(from)),
    }
}

/// Every stored row of one payer, scoped, unordered — [`refuse_overlap`]'s own
/// read and [`intervals_for_payer`]'s before the domain mapping.
async fn load_for_payer(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    payer_tenant_id: Uuid,
) -> Result<Vec<group_membership::Model>, RepoError> {
    group_membership::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(group_membership::Column::TenantId.eq(tenant_id))
                .add(group_membership::Column::PayerTenantId.eq(payer_tenant_id)),
        )
        .order_by(group_membership::Column::EffectiveFrom, Order::Asc)
        .order_by(group_membership::Column::MembershipId, Order::Asc)
        .all(runner)
        .await
        .map_err(|e| RepoError::Db(format!("list pricing_group_membership: {e}")))
}

/// Read one membership, scoped.
///
/// `None` means the membership does not exist **or** lies outside the
/// caller's scope, deliberately the same answer either way —
/// [`window_repo::find`](super::window_repo::find)'s reason: membership is
/// payer-level commercial data.
///
/// `pub(crate)`, not `pub`: [`require`] is `enroll`/`end_membership`'s own
/// caller, and [`crate::infra::read_model::project_membership_subject`] is the
/// projector's — the read a `group_membership` delta is built from, since this
/// table carries no revision-scoped content to pin against instead (see
/// `MembershipSubjectDelta`'s doc). Both are inside this crate; neither is a
/// second public entry point this module owes documentation or a stability
/// promise for.
///
/// # Errors
/// [`RepoError::CorruptRow`] when a stored `row_version` is negative.
/// [`RepoError::Db`] on a scope or storage failure.
pub(crate) async fn find(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    membership_id: Uuid,
) -> Result<Option<MembershipRow>, RepoError> {
    let row = group_membership::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(group_membership::Column::TenantId.eq(tenant_id))
                .add(group_membership::Column::MembershipId.eq(membership_id)),
        )
        .one(runner)
        .await
        .map_err(|e| RepoError::Db(format!("read pricing_group_membership: {e}")))?;
    row.map(to_domain).transpose()
}

/// [`find`], or the refusal that the membership is not there.
async fn require(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    membership_id: Uuid,
) -> Result<MembershipRow, RepoError> {
    find(runner, scope, tenant_id, membership_id)
        .await?
        .ok_or_else(|| RepoError::NotFound {
            subject: "membership".to_owned(),
            id: membership_id.to_string(),
        })
}

/// Read a stored row into the domain's vocabulary.
fn to_domain(row: group_membership::Model) -> Result<MembershipRow, RepoError> {
    Ok(MembershipRow {
        membership_id: row.membership_id,
        tenant_id: row.tenant_id,
        payer_tenant_id: row.payer_tenant_id,
        group_value: row.group_value,
        effective_from: row.effective_from,
        effective_to: row.effective_to,
        created_by: row.created_by,
        created_at: row.created_at_utc,
        // `window_repo::to_domain`'s `mutation_seq` conversion, one column
        // over: the one place the column's lower bound is enforced, since no
        // portable CHECK holds it.
        row_version: u64::try_from(row.row_version).map_err(|_| {
            RepoError::CorruptRow(format!(
                "pricing_group_membership.row_version of membership {} is {}, and a \
                 concurrency token counts acts",
                row.membership_id, row.row_version
            ))
        })?,
    })
}

#[cfg(test)]
#[path = "group_membership_repo_tests.rs"]
mod group_membership_repo_tests;
