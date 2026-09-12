//! The migration schedule's storage half (`inst-ms-api`, `inst-mg-idem`,
//! `inst-mst-start`, `inst-mst-complete`, `inst-mst-cancel`,
//! `inst-mst-cancel-inflight`, D-34, D-65).
//!
//! A runner-taking repository rather than a provider-holding one, for
//! [`super::audit_repo`]'s reason: every write here belongs to a transaction
//! that also emits an event or appends an audit record, and a schedule that
//! could commit separately from the `PlanMigrationScheduled` announcing it is a
//! schedule Subscriptions may never hear about.
//!
//! # M2 is the primary key, and [`insert_or_load`] is the whole of it
//!
//! `inst-ms-api` makes the create idempotent on a **client-supplied**
//! `migration_id`. The implementation is an `INSERT... ON CONFLICT DO NOTHING`
//! followed by a load — [`super::idempotency_repo`]'s shape, and for the same
//! reason it gives there: the at-most-once guarantee **is** the statement, so two
//! concurrent creates of one `migration_id` cannot both succeed however they
//! interleave. A read-then-insert would be a race with a window exactly as wide
//! as the round trip.
//!
//! **A conflicting retry returns the stored row rather than an error.** That is
//! `inst-ms-api`'s own sentence — "a timed-out client retry returns the original
//! schedule, never a second one" — and it is why this function's name says
//! `or_load` rather than `or_fail`. The caller learns which happened from
//! [`Scheduled::created`], because a route that answered `202 Created` to a
//! replay would tell a client it had just scheduled something it scheduled an
//! hour ago.
//!
//! # The flips are compare-and-swaps on the state they leave
//!
//! `plan_repo::retire_revision`'s shape. Each `UPDATE` carries the **expected
//! current state** in its `WHERE`, so a concurrent writer that moved the row
//! first loses rather than overwrites: `rows_affected == 0` is never a silent
//! success. It is reported as [`RepoError::ConcurrentMutation`] while the row
//! still stands in the state the statement expected, and as
//! [`RepoError::MigrationStateForbidden`] once it has left — a predicate that can
//! never match again is not contention. The
//! table's trigger refuses the same edges independently — the machine is stated
//! in `domain::migration`, in the `CHECK`s and here, and this layer is the one
//! that makes the refusal a typed error rather than a driver string.
//!
//! # D-65's replay is a read, not a second write
//!
//! [`start`] is the only flip that can be called twice in the ordinary course of
//! events, because Subscriptions retries. It resolves that **before** attempting
//! the `UPDATE`: an already-`in_progress` row returns its persisted
//! `exclusion_snapshot` verbatim and re-transitions nothing. That ordering is the
//! rule rather than an optimisation — a recompute could differ from the set the
//! executor already honoured, and the table's `trg_pricing_migration_exclusion_replay`
//! arm refuses the write that would install a different one.

use sea_orm::ActiveValue::Set;
use sea_orm::sea_query::{Expr, OnConflict, SimpleExpr};
use sea_orm::{ColumnTrait, Condition, DbErr, EntityTrait, Order};
use serde_json::Value as JsonValue;
use time::OffsetDateTime;
use toolkit_db::odata::sea_orm_filter::paginate_odata;
use toolkit_db::secure::{
    AccessScope, DBRunner, ScopeError, SecureEntityExt, SecureInsertExt, SecureUpdateExt,
};
use toolkit_odata::{ODataQuery, Page, SortDir};
use uuid::Uuid;

use bss_pricing_sdk::odata::MigrationFilterField;

use crate::domain::migration::MigrationState;
use crate::domain::scope_key::PlanId;
use crate::infra::storage::entity::migration;
use crate::infra::storage::odata_mapping::{
    LIST_LIMIT_CFG, MigrationODataMapper, OdataPageError, domain_page, map_odata_err,
    query_with_default_order,
};
use crate::infra::storage::repo::check_authored_instant;
use crate::infra::storage::{RepoError, contention_or_db};

/// A migration schedule as this gear holds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MigrationRecord {
    /// The client-supplied id, and the idempotency key.
    pub migration_id: Uuid,
    /// The retiring side.
    pub source_plan_id: PlanId,
    /// The source plan's revision at schedule time.
    pub source_revision: u64,
    /// The published target.
    pub target_plan_id: PlanId,
    /// When the migration takes effect.
    pub effective_at: OffsetDateTime,
    /// The instant D-49's notice period is measured from.
    pub announced_at: OffsetDateTime,
    /// `all` or a subscription filter.
    pub scope: JsonValue,
    /// Where the run stands in §4's machine.
    pub state: MigrationState,
    /// The deltas as they were at schedule time.
    pub delta_report: JsonValue,
    /// D-65's persisted exclusion set; `None` until the first `start`.
    pub exclusion_snapshot: Option<JsonValue>,
    /// The completion or partial-cancel record.
    pub completion_record: Option<JsonValue>,
}

/// The row a schedule is created from.
#[derive(Clone, Debug)]
pub struct NewMigration {
    /// Client-supplied (`inst-ms-api`).
    pub migration_id: Uuid,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// The retiring side.
    pub source_plan_id: PlanId,
    /// The source plan's revision at schedule time.
    pub source_revision: u64,
    /// The published target.
    pub target_plan_id: PlanId,
    /// When the migration takes effect.
    pub effective_at: OffsetDateTime,
    /// D-49's announcement instant — the scheduling commit.
    pub announced_at: OffsetDateTime,
    /// `all` or a subscription filter.
    pub scope: JsonValue,
    /// The schedule-time delta report.
    pub delta_report: JsonValue,
    /// The principal scheduling it.
    pub created_by: Uuid,
    /// The commit instant.
    pub created_at: OffsetDateTime,
}

/// What [`insert_or_load`] answered with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Scheduled {
    /// The schedule, whether it was just created or already existed.
    pub record: MigrationRecord,
    /// `false` when this call was a replay of an earlier create.
    pub created: bool,
}

/// Create a schedule, or return the one this `migration_id` already names
/// (`inst-ms-api`, `inst-mg-idem`, M2).
///
/// # `effectiveAt` meets D-144's quantum and `announcedAt` deliberately does not
///
/// [`super::check_authored_instant`] holds the millisecond quantum for the tables
/// storing an instant an operator authored, and `effective_at` is one: it arrives
/// on the schedule request, is carried back in a contract field, and is
/// **compared** — D-49's notice floor is `effective_at - announced_at`, and the
/// table's own `CHECK` compares the pair too.
///
/// `announced_at` is the scheduling **commit** instant, minted here from the act's
/// stamp rather than authored, and it is outside the rule for the reason
/// [`super::window_repo::transition`] records after measuring it: `OffsetDateTime::now_utc()`
/// carries sub-millisecond precision, so gating a machine-generated timestamp
/// refuses every write that would ever be attempted. `created_at` is outside it on
/// the same footing — `domain::instant` names storage bookkeeping explicitly.
///
/// # A conflict is a replay only if it carries the same request
///
/// **This closes the race, not the ordinary second `POST`** — see
/// [`StatedRequest`]'s doc, which names the one edit that would, and why it is not
/// made here. `infra::migration::schedule_in` returns the stored record from its
/// own replay arm before this function is reached, so the sequential case never
/// arrives at the comparison below; what does arrive is the loser of two concurrent
/// creates under one id, whose pre-read missed.
///
/// The `ON CONFLICT DO NOTHING` below says *this id is taken*; it does not say
/// *by this*. Reading every conflict as a replay and returning the stored row
/// unconditionally answers a second `POST` under a spent `migration_id` naming a
/// **different** target plan, effective date or scope with the first schedule, and
/// discards the arriving request — the caller's `source_plan_id`,
/// `target_plan_id`, `scope` and `effective_at` all dropped, with no later guard.
/// `bulk_repo::NewBulkOperation`'s doc states the rule this keeps: *"a key that
/// opened a run under a different body must not read as a key that opened nothing
/// at all"*. Three of those four fields are compared; `effective_at` is not, and
/// [`StatedRequest`] records why.
///
/// **Compared against the stored row, not against a digest.** `idempotency_repo`
/// and `bulk_repo` hash the body because their tables hold no copy of it; this one
/// does — `source_plan_id`, `target_plan_id`, `effective_at` and `scope` are the
/// request, columns the append-only trigger then freezes — so the comparison is
/// exact rather than collision-bounded and needs no schema change.
///
/// The compared fields are the ones a caller states **that change what the
/// migration does**. `announced_at`, `created_at` and `source_revision` are minted
/// or read at commit time and move between a call and its retry by construction,
/// and `delta_report` is recomputed from the target's shape; comparing any of them
/// would refuse the identical retry M2 exists to serve. `effective_at` is stated
/// and is still excluded — [`StatedRequest`] records why, and by whose decision.
///
/// # Errors
/// [`RepoError::TimestampPrecisionExceeded`] when `effective_at` is finer than the
/// millisecond quantum; [`RepoError::IdempotencyPayloadMismatch`] when this
/// `migration_id` is already spent on a different request;
/// [`RepoError::Db`] on a storage failure;
/// [`RepoError::CorruptRow`] when the
/// stored row cannot be read back into the domain's vocabulary;
/// [`RepoError::ConcurrentMutation`] when the insert is refused and the row it
/// conflicted with has vanished, which the table's `DELETE` ban makes
/// unreachable in practice and which is reported rather than unwrapped.
pub async fn insert_or_load(
    runner: &impl DBRunner,
    scope: &AccessScope,
    new: NewMigration,
) -> Result<Scheduled, RepoError> {
    check_authored_instant("effectiveAt", Some(new.effective_at))?;
    // The guard is the **column's** range and not a third one: `bigint` since
    // `pricing_snapshot_provenance`, so this refuses only a revision above `i64::MAX`, which
    // no plan reachable through `pricing_plan.revision` — itself `bigint` — can
    // stand at. It was `i32::try_from` against an `integer` column, and that
    // narrowing is what the widening removed.
    let Ok(revision) = i64::try_from(new.source_revision) else {
        return Err(RepoError::CorruptRow(format!(
            "plan {} stands at a revision {} no column can address",
            new.source_plan_id, new.source_revision
        )));
    };

    // Taken before the row is built, because building it moves `new`'s two JSON
    // fields. See [`StatedRequest`].
    let migration_id = new.migration_id;
    let stated = StatedRequest::of(&new);

    let model = migration::ActiveModel {
        migration_id: Set(new.migration_id),
        tenant_id: Set(new.tenant_id),
        source_plan_id: Set(new.source_plan_id.get()),
        source_revision: Set(revision),
        target_plan_id: Set(new.target_plan_id.get()),
        effective_at: Set(new.effective_at),
        announced_at: Set(new.announced_at),
        scope: Set(new.scope),
        state: Set(MigrationState::Scheduled.as_str().to_owned()),
        delta_report: Set(new.delta_report),
        exclusion_snapshot: Set(None),
        completion_record: Set(None),
        created_by: Set(new.created_by),
        created_at: Set(new.created_at),
        started_at: Set(None),
        completed_at: Set(None),
        cancelled_at: Set(None),
    };

    let on_conflict =
        OnConflict::columns([migration::Column::TenantId, migration::Column::MigrationId])
            .do_nothing()
            .to_owned();

    // The at-most-once guarantee **is** this statement; see the module doc.
    // `RecordNotInserted` is the conflict swallowing the insert, which is
    // `idempotency_repo::claim`'s signal and means the same thing here: this
    // `migration_id` was scheduled before, so the create is a replay.
    let created = match migration::Entity::insert(model.clone())
        .secure()
        .scope_with_model(scope, &model)
        .map_err(|e| RepoError::Db(format!("migration schedule scope: {e}")))?
        .on_conflict_raw(on_conflict)
        .exec(runner)
        .await
    {
        Ok(_) => true,
        Err(ScopeError::Db(DbErr::RecordNotInserted)) => false,
        Err(e) => {
            return Err(RepoError::Db(format!(
                "schedule migration {}: {e}",
                new.migration_id
            )));
        }
    };

    let record = load(runner, scope, new.tenant_id, new.migration_id)
        .await?
        .ok_or_else(|| RepoError::ConcurrentMutation {
            aggregate: format!("migration {}", new.migration_id),
        })?;

    // See the doc: the conflict is a replay only if the arriving request is the one
    // the key was spent on. Asked only on the conflict arm — on `created` the row
    // *is* this request and the comparison would be against itself.
    if !created && !stated.matches(&record) {
        return Err(RepoError::IdempotencyPayloadMismatch {
            operation: SCHEDULE_MIGRATION_OPERATION.to_owned(),
            client_key: migration_id.to_string(),
        });
    }

    Ok(Scheduled { record, created })
}

/// The idempotency scope `migration_id` is a key within, for
/// [`RepoError::IdempotencyPayloadMismatch`]'s `operation`.
///
/// Named in the shape the routes' own `*_OPERATION` constants use, even though this
/// create carries no `Idempotency-Key` header: `migration_id` **is** the key
/// (`inst-ms-api`, `inst-mg-idem`), so the refusal an operator reads must name the
/// same operation a header-keyed one would.
const SCHEDULE_MIGRATION_OPERATION: &str = "bss_pricing.schedule_migration";

/// What a caller **states** when it schedules a migration — the part of
/// [`NewMigration`] that a replay under the same `migration_id` has to repeat.
///
/// A named value rather than four comparisons inline, for three reasons. It is
/// taken before the `ActiveModel` is built, which moves `new`'s two JSON fields;
/// its constructor is the one place the request/derived split is written down, so
/// it is the one place a reader checks it; and it is **public**, because the seam
/// that most needs it is one layer up.
///
/// # Two doors ask this question, and neither one subsumes the other
///
/// **Both are live**, and a reader needs the pair rather than
/// either alone, because each covers exactly the case the other cannot reach:
///
/// * `infra::migration::schedule_in`'s replay arm — *"1. The replay arm, before
///   any validation"* — meets the **ordinary sequential second `POST`**. It loads
///   by `migration_id` and returns before [`insert_or_load`] is called at all, so
///   nothing downstream can see that request. It builds one of these from the
///   `ScheduleRequest`'s stated fields and asks [`StatedRequest::matches`] before
///   returning the stored record.
/// * [`insert_or_load`] below meets the **race**: two concurrent creates under one
///   id, where the loser's pre-read missed and its insert then conflicted. That
///   caller never took the arm above, so the store is the only place left to ask.
///
/// Both answer [`RepoError::IdempotencyPayloadMismatch`] / its domain sibling —
/// 409 — and **this type is what keeps them one comparison instead of two that
/// drift**: `matches` is written once, the constructor's destructure makes a
/// twelfth stated field a compile error, and that is the whole reason it is `pub`.
///
/// The first door is not declined: the arm compares, and
/// `rest_migrations::a_reused_migration_id_naming_another_plan_pair_is_refused_under_the_spent_key`
/// asserts the 409 and `IDEMPOTENCY_PAYLOAD_MISMATCH`, with a byte-identical
/// resubmission beside it as the positive control that a genuine replay still
/// answers `200`. A refusal without that control cannot be told from a replay arm
/// that conflicts on everything.
///
/// # `effective_at` is deliberately **outside** it, and that is an open question
///
/// The finding this closes named four discarded fields, `effective_at`
/// among them. It is left out because two green cases pin the opposite, and both
/// pin it **deliberately** rather than by accident — which is the one shape a
/// repository refusal must not overrule on its own authority:
///
/// * `sqlite_migration_repo::a_retry_of_one_migration_id_returns_the_original_schedule_and_never_a_second`
///   retries with a different `effective_at` and asserts the stored schedule
///   answers, quoting `inst-ms-api` verbatim;
/// * `rest_migrations::a_replay_of_one_migration_id_answers_200_with_the_original_schedule`
///   does the same through the route. The refusal case named above cites it as the
///   route-level pin on this field and records that neither pin moved when the
///   diverging-body arm was closed — which is what makes these two questions
///   separable rather than one.
///
/// Neither is a fixture quietly carrying a fault; both are contract pins. Whether
/// `inst-ms-api`'s *"a timed-out client retry returns the original schedule"* means
/// **whatever body you send** or **the body you sent** is a design question for the
/// owner of `infra::migration` and for the design set, not something this seam
/// should decide. What is not in question is the other three: a resubmission naming
/// other plans or another subscriber set is a different migration under a spent key,
/// and answering it with the stored one discards an operator's act outright.
///
/// Adding `effective_at` here is a one-line change plus a rewrite of those two
/// cases. Do both together or neither — and the first thing to redden is neither
/// of them but
/// `migration_repo_tests::a_differing_effective_date_is_still_a_replay_today`,
/// which needs no database and exists for exactly that purpose: its own doc says
/// it is there so that flipping the reading is **visible** rather than silent.
#[derive(Debug, PartialEq, Eq)]
pub struct StatedRequest {
    /// The retiring side.
    pub source_plan_id: PlanId,
    /// The published target.
    pub target_plan_id: PlanId,
    /// `all` or a subscription filter.
    pub scope: JsonValue,
}

impl StatedRequest {
    /// **Destructured, so a twelfth field on [`NewMigration`] is a compile error
    /// here** rather than a request component silently outside the comparison —
    /// which is the whole failure this type exists to end, and the shape
    /// `ScopeKey`'s `Display` uses for the same reason one plane over.
    #[must_use]
    pub fn of(new: &NewMigration) -> Self {
        let NewMigration {
            // The key itself: `load` found the row *by* it, so it is equal by
            // construction and comparing it would assert the query.
            migration_id: _,
            // Likewise — `load` is scoped to it.
            tenant_id: _,
            source_plan_id,
            // Read at commit time off the source plan, so a retry after the plan moved
            // carries a different one. Not a request component.
            source_revision: _,
            target_plan_id,
            // Stated, and still excluded — see the type doc. Two suites pin a
            // differing effective date as a replay, deliberately, and the reading
            // of `inst-ms-api` behind that is not this seam's to settle.
            effective_at: _,
            // Minted here from the act's stamp; differs on every call by design.
            announced_at: _,
            scope,
            // Recomputed from the target's current shape.
            delta_report: _,
            // The principal and the commit instant, not the request.
            created_by: _,
            created_at: _,
        } = new;
        Self {
            source_plan_id: *source_plan_id,
            target_plan_id: *target_plan_id,
            scope: scope.clone(),
        }
    }

    /// Is the stored row the one this request wrote?
    ///
    /// `scope` is compared as a parsed `JsonValue` rather than as text, so key
    /// order surviving a `jsonb` round trip on Postgres and a `text` one on
    /// `SQLite` cannot make an identical retry read as a different request.
    #[must_use]
    pub fn matches(&self, held: &MigrationRecord) -> bool {
        *self
            == Self {
                source_plan_id: held.source_plan_id,
                target_plan_id: held.target_plan_id,
                scope: held.scope.clone(),
            }
    }
}

/// Read one schedule.
///
/// # Errors
/// [`RepoError::Db`] on a storage failure; [`RepoError::CorruptRow`] when the
/// stored state token is outside the column's `CHECK`.
pub async fn load(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    migration_id: Uuid,
) -> Result<Option<MigrationRecord>, RepoError> {
    let row = migration::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(migration::Column::TenantId.eq(tenant_id))
                .add(migration::Column::MigrationId.eq(migration_id)),
        )
        .one(runner)
        .await
        .map_err(|e| RepoError::Db(format!("read migration {migration_id}: {e}")))?;

    row.map(into_record).transpose()
}

/// One page of the tenant's migration schedules, in `migration_id` order.
///
/// D-125's keyset walk over the migration plane — `crate::api::rest::cursor`'s
/// module doc argues once, for every list surface, why the resume is a key and
/// never an offset.
///
/// `states` narrows the walk. An **empty** slice is every state rather than none,
/// which is `approval_repo::list_page`'s reading and for its reason: a caller that
/// named no filter asked for everything, and answering an empty page to that would
/// be a filter nobody applied. It is also the right default *here* specifically —
/// a completed run is the record of what moved, and an operator's queue over
/// scheduled **and** finished runs is the ordinary case.
///
/// **`migration_id` alone is the walk's key even though the primary key is
/// `(migration_id, tenant_id)`.** The second half is a constant across the walk —
/// every row is filtered to `tenant_id` before ordering — so a single-column
/// resume names exactly one row and the cursor stays the 16 bytes
/// `crate::api::rest::cursor` encodes.
///
/// # Errors
/// [`RepoError::Db`] on a storage failure; [`RepoError::CorruptRow`] when a stored
/// state token is outside the column's `CHECK`.
pub async fn list_page(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    states: &[MigrationState],
    after: Option<Uuid>,
    limit: u64,
) -> Result<Vec<MigrationRecord>, RepoError> {
    let mut filter = Condition::all().add(migration::Column::TenantId.eq(tenant_id));
    if !states.is_empty() {
        let tokens: Vec<&str> = states.iter().copied().map(MigrationState::as_str).collect();
        filter = filter.add(migration::Column::State.is_in(tokens));
    }
    if let Some(after) = after {
        filter = filter.add(migration::Column::MigrationId.gt(after));
    }
    migration::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(filter)
        .order_by(migration::Column::MigrationId, Order::Asc)
        .limit(limit)
        .all(runner)
        .await
        .map_err(|e| RepoError::Db(format!("list pricing_migration: {e}")))?
        .into_iter()
        .map(into_record)
        .collect()
}

/// One `OData` page of the tenant's migration schedules. Default order is
/// `migration_id asc`.
///
/// # Errors
/// [`OdataPageError::Db`] on storage failure; [`OdataPageError::Odata`] on a
/// malformed `$filter` / `$orderby` / cursor.
pub async fn list_odata(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    query: &ODataQuery,
) -> Result<Page<MigrationRecord>, OdataPageError> {
    let base_select = migration::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(Condition::all().add(migration::Column::TenantId.eq(tenant_id)));
    let query = query_with_default_order(query, &[MigrationFilterField::MigrationId]);
    let page = paginate_odata::<
        MigrationFilterField,
        MigrationODataMapper,
        migration::Entity,
        migration::Model,
        _,
        _,
    >(
        base_select,
        runner,
        &query,
        ("migration_id", SortDir::Asc),
        LIST_LIMIT_CFG,
        |m| m,
    )
    .await
    .map_err(map_odata_err)?;
    domain_page(page, into_record)
}

/// The **live** migrations aimed at a plan (`RETIRE_TARGET_OF_MIGRATION`).
///
/// Live means `scheduled` or `in_progress`: a completed or cancelled run moves
/// nobody, so it must not refuse a retirement. The narrowing is here rather than
/// at the caller because "what blocks a retirement" is a question about the
/// store's state and not about the retirement.
///
/// # Errors
/// [`RepoError::Db`] on a storage failure; [`RepoError::CorruptRow`] on an
/// unreadable state token.
pub async fn live_targeting(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    target_plan_id: PlanId,
) -> Result<Vec<MigrationRecord>, RepoError> {
    let rows = migration::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(migration::Column::TenantId.eq(tenant_id))
                .add(migration::Column::TargetPlanId.eq(target_plan_id.get()))
                .add(migration::Column::State.is_in([
                    MigrationState::Scheduled.as_str(),
                    MigrationState::InProgress.as_str(),
                ])),
        )
        .all(runner)
        .await
        .map_err(|e| {
            RepoError::Db(format!(
                "read live migrations targeting plan {target_plan_id}: {e}"
            ))
        })?;

    rows.into_iter().map(into_record).collect()
}

/// What [`start`] answered with (`inst-mst-start`, D-65).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Started {
    /// The schedule, now `in_progress`.
    pub record: MigrationRecord,
    /// `false` when this call replayed an earlier start (D-65's persist-and-replay).
    pub transitioned: bool,
}

/// Declare execution start, or replay the persisted exclusion set
/// (`inst-mst-start`, D-65).
///
/// `exclusion_snapshot` is written **only** on the transitioning call. A repeat
/// call returns the stored one verbatim and re-runs no D-36 re-validation; see
/// the module doc for why that ordering is the rule rather than an optimisation.
///
/// # Errors
/// [`RepoError::Db`] on a storage failure; [`RepoError::ConcurrentMutation`]
/// when another writer moved the row between the read and the write;
/// [`RepoError::CorruptRow`] on an unreadable state token.
pub async fn start(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    migration_id: Uuid,
    exclusion_snapshot: JsonValue,
    now: OffsetDateTime,
) -> Result<Option<Started>, RepoError> {
    let Some(current) = load(runner, scope, tenant_id, migration_id).await? else {
        return Ok(None);
    };

    // D-65's replay arm, resolved before any write is attempted.
    if current.state == MigrationState::InProgress {
        return Ok(Some(Started {
            record: current,
            transitioned: false,
        }));
    }

    let updated = flip(
        runner,
        scope,
        tenant_id,
        migration_id,
        MigrationState::Scheduled,
        MigrationState::InProgress,
        vec![
            (migration::Column::StartedAt, Expr::value(now)),
            (
                migration::Column::ExclusionSnapshot,
                Expr::value(exclusion_snapshot),
            ),
        ],
    )
    .await?;

    Ok(Some(Started {
        record: updated,
        transitioned: true,
    }))
}

/// Report the scope processed and close the run (`inst-mst-complete`).
///
/// # Errors
/// [`RepoError::Db`]; [`RepoError::ConcurrentMutation`] when the row still stands
/// in `in_progress` and another writer took it first;
/// [`RepoError::MigrationStateForbidden`] naming the state when the row has left
/// `in_progress`, where no retry can ever match; [`RepoError::CorruptRow`] on an
/// unreadable state token or a row that vanished under the flip.
pub async fn complete(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    migration_id: Uuid,
    completion_record: JsonValue,
    now: OffsetDateTime,
) -> Result<MigrationRecord, RepoError> {
    flip(
        runner,
        scope,
        tenant_id,
        migration_id,
        MigrationState::InProgress,
        MigrationState::Completed,
        vec![
            (migration::Column::CompletedAt, Expr::value(now)),
            (
                migration::Column::CompletionRecord,
                Expr::value(completion_record),
            ),
        ],
    )
    .await
}

/// Cancel a run from whichever live state it stands in (`inst-mst-cancel`,
/// `inst-mst-cancel-inflight`, D-34, M3).
///
/// The caller has already asked
/// [`MigrationState::ensure_cancellable`](crate::domain::migration::MigrationState::ensure_cancellable)
/// so that a `completed` run is refused in D-34's own words; this function
/// carries the state it expects to leave into the `WHERE` regardless, because
/// the interval between that check and this write is a window a concurrent
/// `complete` can land in.
///
/// `partial_record` carries the migrated / not-attempted sets when the run was
/// `in_progress` (D-34) and is `None` for a cancel of a `scheduled` one, where
/// nothing was attempted at all.
///
/// # Errors
/// [`RepoError::Db`]; [`RepoError::ConcurrentMutation`] when the row still stands
/// in `from` and another writer took it first;
/// [`RepoError::MigrationStateForbidden`] naming the state when the row has left
/// `from`, where no retry can ever match; [`RepoError::CorruptRow`] on an
/// unreadable state token or a row that vanished under the flip.
pub async fn cancel(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    migration_id: Uuid,
    from: MigrationState,
    partial_record: Option<JsonValue>,
    now: OffsetDateTime,
) -> Result<MigrationRecord, RepoError> {
    flip(
        runner,
        scope,
        tenant_id,
        migration_id,
        from,
        MigrationState::Cancelled,
        // The partial sets ride the same column the completion record does: they
        // are the same fact asked at a different moment, and a second column
        // would let one run answer "what was processed" two ways.
        match partial_record {
            Some(record) => vec![
                (migration::Column::CancelledAt, Expr::value(now)),
                (migration::Column::CompletionRecord, Expr::value(record)),
            ],
            None => vec![(migration::Column::CancelledAt, Expr::value(now))],
        },
    )
    .await
}

/// One compare-and-swap on the state machine, with the columns that flip
/// alongside it.
///
/// Factored rather than copied three times: the branch that must never drift is
/// "the expected current state is in the `WHERE`", and there is exactly one copy
/// of it.
async fn flip(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    migration_id: Uuid,
    from: MigrationState,
    to: MigrationState,
    extra: Vec<(migration::Column, SimpleExpr)>,
) -> Result<MigrationRecord, RepoError> {
    let mut query = migration::Entity::update_many()
        .secure()
        .scope_with(scope)
        .col_expr(migration::Column::State, Expr::value(to.as_str()));
    for (column, value) in extra {
        query = query.col_expr(column, value);
    }
    let result = query
        .filter(
            Condition::all()
                .add(migration::Column::TenantId.eq(tenant_id))
                .add(migration::Column::MigrationId.eq(migration_id))
                // The expected current state. A concurrent writer that moved the
                // row first loses here rather than overwriting.
                .add(migration::Column::State.eq(from.as_str())),
        )
        .exec(runner)
        .await
        .map_err(|e| {
            contention_or_db(
                &e,
                &format!("migration {migration_id}"),
                &format!("move a migration {from} -> {to}"),
            )
        })?;

    if result.rows_affected == 0 {
        // **Zero rows is contention only while the row still stands in `from`.**
        // Answered unconditionally it advises a retry of a predicate that can never
        // match again — `complete` on a cancelled run, `cancel` on a completed one.
        // So the re-read, which is what both sibling compare-and-swaps in this slice
        // (`window_repo::transition`, `approval_repo::swap`) do to name the state
        // that refused.
        let fresh = load(runner, scope, tenant_id, migration_id).await?;
        return Err(match fresh {
            Some(row) if row.state == from => RepoError::ConcurrentMutation {
                aggregate: format!("migration {migration_id}"),
            },
            Some(row) => RepoError::MigrationStateForbidden {
                migration_id: migration_id.to_string(),
                state: row.state.to_string(),
                attempted: format!("the move {from} -> {to}"),
            },
            None => RepoError::CorruptRow(format!(
                "migration {migration_id} refused a {from} -> {to} move and then could not be read"
            )),
        });
    }

    load(runner, scope, tenant_id, migration_id)
        .await?
        .ok_or_else(|| {
            RepoError::CorruptRow(format!(
                "migration {migration_id} moved {from} -> {to} and then could not be read back"
            ))
        })
}

/// Read a stored row into the domain's vocabulary.
fn into_record(row: migration::Model) -> Result<MigrationRecord, RepoError> {
    let state = MigrationState::parse(&row.state).ok_or_else(|| {
        RepoError::CorruptRow(format!(
            "pricing_migration.state `{}` on migration {}",
            row.state, row.migration_id
        ))
    })?;
    let Ok(source_revision) = u64::try_from(row.source_revision) else {
        return Err(RepoError::CorruptRow(format!(
            "pricing_migration.source_revision {} on migration {} is negative",
            row.source_revision, row.migration_id
        )));
    };

    Ok(MigrationRecord {
        migration_id: row.migration_id,
        source_plan_id: PlanId::new(row.source_plan_id),
        source_revision,
        target_plan_id: PlanId::new(row.target_plan_id),
        effective_at: row.effective_at,
        announced_at: row.announced_at,
        scope: row.scope,
        state,
        delta_report: row.delta_report,
        exclusion_snapshot: row.exclusion_snapshot,
        completion_record: row.completion_record,
    })
}

#[cfg(test)]
#[path = "migration_repo_tests.rs"]
mod migration_repo_tests;
