//! Reads and writes of `pricing_price_window` behind the tenant gate
//! (`design/07-pricewindow-linkage.md` §6).
//!
//! Free functions taking a **runner** rather than a provider, for
//! [`approval_repo`](super::approval_repo)'s reason and one of this store's own.
//! D-99 makes every window mutation a publish unit — validation, a pending
//! `CatalogVersion` ref, a plan-subject re-projection and an outbox row — so a
//! window row that could commit separately from that unit would advertise
//! coverage no consumer's read model has, which is precisely the trailing void
//! D-62 → D-80 → D-94 closed. And [`refuse_overlap`] below has to run inside the
//! same transaction as the insert it guards; a repository holding a provider
//! could not join one, `Db::conn()` being refused outright inside an open
//! transaction.
//!
//! # Non-overlap is here, no **declarative** constraint could hold it, and a
//! trigger could
//!
//! §6: *"non-overlap per canonical scope key enforced **inside every
//! mutation**"*. That is not a stylistic choice and this module is not where a
//! constraint was skipped. The canonical scope key is ten columns of
//! `pricing_price`; `pricing_price_window` carries `price_id` and none of the
//! ten. So no `UNIQUE` index can state the rule, no partial-index predicate can
//! (a predicate sees only its own row's columns — neither the parent's key nor a
//! sibling window's interval), and a range exclusion constraint would need
//! `btree_gist` on Postgres, would still be per-`price_id` rather than per-key,
//! and has no `SQLite` expression at all.
//!
//! **What is *not* true is that the rule could not live in the schema at all.** A
//! cross-table trigger could carry it, and this chain already has one —
//! `pricing_price_tier_band_parent_kind` (`pricing_price_tier_band`) reads its parent row
//! to decide. Not building a second procedural spelling of the key resolution,
//! the occupying-state set and the half-open arithmetic below is a **choice with a
//! residue**, not an impossibility, and the residue is that non-overlap is the one
//! invariant of this table guarded at a single layer where every other is guarded
//! at two.
//!
//! §6's two clauses are two rules and not two readings of one. "REVOKE +
//! column-whitelist trigger discipline" governs **historical immutability**, and
//! it is implemented — as the portable half, per the migration's own note.
//! "Inside every mutation" governs **non-overlap**, and it is implemented here.
//! There is no contradiction between them to resolve; an earlier revision of this
//! doc claimed one, and that claim was invented.
//!
//! The interval is half-open, so [`intersects`] is
//! `a.from < b.to_or_infinity && b.from < a.to_or_infinity` and
//! `effective_to = next.effective_from` does **not** intersect. Adjacency is legal
//! and §9 names it as the false positive this arithmetic must not produce.
//!
//! # What the state machine says here, and how many times it is said
//!
//! Three times, and that is the same arrangement `approval_repo` argues for.
//! [`WindowState::may_move_to`] is the statement;
//! [`transition`]'s `UPDATE` carries its own `state = <expected>` predicate, so a
//! flip that lost a race matches no row rather than trusting the read that
//! preceded it; and `trg_pricing_price_window_append_only` is what an ad-hoc
//! `UPDATE` meets. None of the three is redundant, because each is what a
//! different caller reaches.
//!
//! §4's edges are **conditional**, and the conditions are said in the same three
//! places. `inst-ws-activate` fires WHEN `now ≥ effectiveFrom`, `inst-ws-expire`
//! WHEN `now ≥ effectiveTo`, and *"an open-ended window never expires"*.
//! [`WindowInterval::has_started`] and [`WindowInterval::is_due_to_expire`] are
//! the statement — the domain's, consulted rather than restated;
//! [`transition`]'s `UPDATE` carries them **into its `WHERE`** beside the state
//! predicate; and `chk_pricing_price_window_activation_order`,
//! `chk_pricing_price_window_expiry_order` and
//! `chk_pricing_price_window_open_ended` are the row-local half a CHECK can hold.
//! With the conditions in none of the three, [`transition`] activates a window a
//! week before its start and expires an open-ended one on request.
//!
//! # What this module refuses, what it does not, and the line between them
//!
//! The criterion is [`crate::domain::window`]'s: **the store answers what it also
//! enforces physically, and a surface answers what only a request can be judged
//! against.** Both halves cost something, so both are written out.
//!
//! Refused here, each by consulting the domain's own predicate rather than by
//! restating a rule:
//!
//! * **non-overlap per canonical scope key** (`WINDOW_OVERLAP`) — §6's, and this
//!   module is its only producer anywhere;
//! * **`inst-ws-immutable`** (`WINDOW_HISTORICAL_IMMUTABLE`) — a terminal window's
//!   `effective_to`, an end that has already passed, and an end moved *to* a past
//!   instant. Arms 2 and 5 of `trg_pricing_price_window_append_only` are the same
//!   rule physically, which is what makes it the store's to answer; before the
//!   pre-check a caller met the trigger and read a **500** for a request whose
//!   whole remedy is a later instant;
//! * **the interval's non-emptiness** — `chk_pricing_price_window_interval`'s
//!   application half, [`RepoError::WindowIntervalEmpty`], for the same reason: a
//!   CHECK is not an answer a caller can act on.
//!
//! Not refused here, and each somebody else's by ownership rather than by
//! omission:
//!
//! * `inst-ws-future-start` (`WINDOW_START_IN_PAST`) — a start strictly in the
//!   future at creation. It compares against the instant a **request** arrived,
//!   which no row carries and no trigger can see, so
//!   [`crate::domain::window::check_creation`] owns it whole. A consequence worth
//!   stating: this store will accept a window whose start is already behind the
//!   caller's own clock, and the fixtures and the activation-job suites depend on
//!   exactly that to seed a window that is due.
//! * `inst-ws-cancel`'s `WINDOW_NOT_CANCELLABLE` — also about the request, namely
//!   *which operation* was asked for, which a generic [`transition`] cannot see.
//!   [`crate::domain::window::check_cancellation`] owns it. What arrives here from
//!   an unsanctioned edge is [`RepoError::WindowStateForbidden`], which mints no
//!   code; that variant's doc says why.
//! * The coverage refusals — `WINDOW_COVERAGE_MISSING`, `WINDOW_GAP`,
//!   `WINDOW_TRAILING_VOID` **and `AVAILABILITY_OUTSIDE_COVERAGE`**, which §5
//!   declares and this roster used to omit. Each ranges over every window of a key;
//!   one of them, `WINDOW_TRAILING_VOID`, additionally needs the D-79 subscriber
//!   lane, `inst-fg-trailing`'s only exemption being about in-flight subscribers.
//!   `CoverageChecker`'s.
//! * **D-04's grandfathering horizon, which §6 names this function for — built one
//!   layer up.** §6: a grandfathered generation's window
//!   `effective_to` MUST stay
//!   `≥ grandfather_until + the longest billing cycle sold on the key`, *"enforced
//!   at cutover and on every `effectiveTo` adjustment"*. This entry stood here as
//!   an owed item on the ground that *"the rule's second input has no producer
//!   anywhere in this crate … no function, no column, no type"*. **That ceased to
//!   be true and the entry had not noticed**: `domain::coverage::longest_cycle_sold`
//!   is W6's term, built for D-80's coverage horizon and `inst-fg-trailing`'s
//!   floor, and it takes exactly the two facts the margin is a function of. The
//!   gap was live rather than theoretical — `pricing_price.grandfather_until` is a
//!   written column with its own CHECK, so a key could carry a horizon and a
//!   window be authored inside it, green.
//!
//!   It is now [`crate::infra::window`]'s `refuse_horizon_uncovered`, beside
//!   `refuse_trailing_void`, and it is **not** [`adjust_effective_to`]'s because
//!   the operands are not this layer's: the margin is a property of the plan
//!   shape and the horizon of the price row, and this function holds one window
//!   and its key. It runs on all three of `mutate_in`'s acts and not only the
//!   adjustment, because the reachable violation is a **schedule** — the first
//!   bounded window on a grandfathered key — which no coverage-removal predicate
//!   looks at.
//!
//!   **`mutate_in`'s acts are not every window mutation, and this entry said they
//!   were.** Four paths reach the three functions below without passing that
//!   guard, and each is argued shut rather than checked — which is a different
//!   claim and the one worth recording, since a reader taking "every mutation"
//!   literally would build the next one through this store and inherit no bound.
//!   `infra::cutover` schedules the copy **open-ended** (D-204 clause 4), which
//!   satisfies any margin whatever the horizon says. `infra::supersession` cannot
//!   reach a grandfathered key at all — [`super::price_repo`]'s
//!   `refuse_unsupersedable_class` turns it away one act earlier, on Foundation
//!   §4.3, and `infra::repricing` reaches these windows only through that same
//!   door. `infra::jobs::window_activation` moves a state token and no instant, and
//!   `KeyWindows::coverage_end` reads every non-cancelled state alike, so no
//!   coverage it judges changes.
//!
//!   **`infra::retirement` was the fourth, and it is now checked rather than
//!   argued — the argument that stood here did not survive being written down.**
//!   This entry said the guard *"needs the plan shape and the act's key set,
//!   neither of which is this entry's to add"*, and D-316's residual repeated it
//!   in the register as a claim about **the retirement path** rather than about
//!   this function. The first half is true of `window_repo` and the second half is
//!   false of `infra::retirement`: [`list_for_plan`] resolves every window's
//!   `ScopeKey` off `pricing_price` on every read, which *is* the act's key set,
//!   and `infra::retirement::retire_in` already assembled the `PlanShape` for the
//!   approval pin eleven lines below the composition. So the operand was never
//!   missing — it was missing **here**, one layer down, and the conclusion drawn
//!   from that was a claim about a layer this entry cannot see. The guard is
//!   `domain::retirement::strand_free_disposition`, and it **keeps** the window
//!   rather than refusing the act, because a refusal would make a plan carrying a
//!   generation unretirable for as long as its horizon runs.
//!
//!   What is still true, and is the reason this roster exists: a retirement's
//!   cancellations reach [`transition`] directly and pass no rule in
//!   `infra::window`. The bound is now enforced on both sides of that split by one
//!   walk — [`crate::domain::window::KeyWindows::first_uncovered_from`] — rather
//!   than by two copies of it.
//!
//!   Beware the name: W6 is *called* "the longest billing cycle sold on the key" and
//!   is *defined* per **plan** — the longest `frequency` among the plan's recurring
//!   rows on the key's `(currency, region)` — and is **zero** on a plan with no
//!   recurring part. The cutover half holds by construction (D-204 clause 4: the
//!   copy's window is composed open-ended).
//!
//! # The [`AuditStamp`] is taken here and the trail is written one layer up
//!
//! Every mutating call takes a stamp, because there is no unaudited entry point in
//! this crate by design. [`schedule`] consumes it: the scheduler **is** the actor
//! of the row and the submission instant **is** the row's instant, so `created_by`
//! and `created_at` come off the stamp rather than off two more fields that could
//! disagree with the record of the same act.
//!
//! **No statement in this module writes `pricing_audit_log`, and the trail is not
//! missing** — it is one layer up, at the single site all three of these mutations
//! reach it through. [`crate::infra::window`]'s `mutate_in` dispatches to
//! [`schedule`], [`adjust_effective_to`] and [`transition`] from one `write` and
//! then appends one `NewAuditEntry` on the same runner, keyed on the plan's chain
//! (`audit_repo::plan_chain`), with `actor_principal_id` taken off the stamp and
//! `subject_ref` rendered by `audit_repo::window_ref`. So "who *shortened* it" is
//! answerable, and it is answerable about the same act the approval plane names,
//! because both planes spell the subject through that one helper.
//!
//! Two qualifications, since the paragraph this replaced was read as an absolute
//! and it was only ever a statement about **this repository**:
//!
//! * The store still has no `updated_by` column, and that is unchanged and
//!   deliberate: the actor of a *change* rides the audit record, which D-135 keys
//!   on the audited subject's aggregate, rather than a column no decision names.
//!   `created_by` remains frozen by the whitelist.
//! * One token serves all three acts — `crate::infra::window`'s
//!   `AUDIT_ACTION` is `AuditAction::Publish` for a schedule, a shortening and a
//!   cancel alike — so the trail tells them apart only through `before_state` /
//!   `after_state` and the act segment of the unit's subject. That is an open
//!   coarseness in the action vocabulary, not a missing record.
//!
//! What binds the `window` subject token is [`AuditSubjectKind`]'s Rust
//! enumeration, paired with the schema's own CHECKs — which is why that pairing
//! is extended in the same change as a writer and not before.
//!
//! **State what binds a token, not what the schema currently declines to.** A doc
//! recording `pricing_audit_log.subject_kind` as free `text` with no CHECK, citing a
//! migration roster as proof, can be contradicted within hours by a constraint
//! joining that very roster — in the same fix wave, from a different finding. A
//! claim measured against the schema of the hour expires as fast as the hour does.
//!
//! [`transition`]'s parameter is spelled `_stamp` because that path uses none of
//! it, so the omission is visible at every call site instead of being a promise
//! the signature makes and the body does not keep. [`adjust_effective_to`] does
//! read it — `recorded_at` is the clock `inst-ws-immutable` is judged against
//! there.

use sea_orm::ActiveValue::Set;
use sea_orm::sea_query::{Expr, SelectStatement};
use sea_orm::{ColumnTrait, Condition, EntityTrait, Order, QueryFilter, QuerySelect, QueryTrait};
use time::OffsetDateTime;
use toolkit_db::odata::sea_orm_filter::paginate_odata;
use toolkit_db::secure::{
    AccessScope, DBRunner, SecureEntityExt, SecureInsertExt, SecureUpdateExt,
};
use toolkit_odata::{ODataQuery, Page, SortDir};
use uuid::Uuid;

use crate::domain::audit::AuditStamp;
use crate::domain::projection::PROJECTED_ROW_STATES;
use std::collections::HashMap;

use crate::domain::scope_key::{PlanId, ScopeKey};
use crate::domain::window::{
    FrozenEnd, WindowInterval, WindowState, frozen_end, interval_is_non_empty,
};
use crate::infra::storage::entity::{price, price_window};
use crate::infra::storage::odata_mapping::{
    LIST_LIMIT_CFG, OdataPageError, WindowODataMapper, domain_page, map_odata_err,
    query_with_default_order,
};
use crate::infra::storage::repo::{check_authored_instant, price_repo};
use crate::infra::storage::{RepoError, contention_or_db};
use bss_pricing_sdk::odata::WindowFilterField;

use crate::domain::instant::{format_rfc3339, truncate_millis};
/// The window states an interval competes for its key with —
/// [`crate::domain::window::OCCUPYING_STATES`], where the argument for the set
/// lives.
///
/// **Kept in `domain::window` and imported here rather than copied**, alongside
/// D-88's compose. Its whole justification is a chain of domain facts — a
/// cancelled window never took effect, an expired one cannot be intersected by
/// anything `inst-ws-future-start` admits — so a second consumer that could not reach
/// this module would otherwise have hand-maintained its own list. That is exactly how
/// the unit-guard field list nearly drifted, and the fix there was the same one: one
/// place, two callers.
use crate::domain::window::OCCUPYING_STATES;

/// A window to schedule.
///
/// Carries no `state`: a window is created `scheduled` or it is not created, which
/// is §4's initial state and what every other rule in this module is written
/// about. It carries no `created_by` and no `created_at` either — those are the
/// [`AuditStamp`]'s, for the reason `NewApproval` gives.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewWindow {
    /// The window's durable name, minted by the caller so an authoring surface
    /// can return it before the row is durable.
    pub window_id: Uuid,
    /// RLS scope.
    pub tenant_id: Uuid,
    /// The price row the interval belongs to, and thereby the key it is filed
    /// under. Immutable once written (§6).
    pub price_id: Uuid,
    /// Inclusive start of the half-open interval, UTC.
    pub effective_from: OffsetDateTime,
    /// Exclusive end, UTC; `None` is open-ended (`inst-ws-expire`).
    pub effective_to: Option<OffsetDateTime>,
    /// The operator-supplied change reason (§6, from the legacy UC scenarios).
    pub reason_code: String,
}

/// One window, read back into the vocabulary the rest of the system uses.
///
/// **It carries the canonical scope key of its price row**, resolved on the read.
/// The row is what the window is bound to and the key is what non-overlap and
/// coverage are per, so every reader would otherwise resolve one into the other
/// itself — and a reader that got it wrong would compute coverage for a key the
/// window is not on. `state` arrives typed, so a token the store admits and this
/// crate does not is a [`RepoError::CorruptRow`] at this boundary rather than a
/// string carried into a handler.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WindowRecord {
    /// The window's durable name.
    pub window_id: Uuid,
    /// RLS scope.
    pub tenant_id: Uuid,
    /// The price row this interval belongs to.
    pub price_id: Uuid,
    /// The ten axes that row is filed under — resolved from `pricing_price` on
    /// every read, never stored here.
    pub scope_key: ScopeKey,
    /// Inclusive start, UTC.
    pub effective_from: OffsetDateTime,
    /// Exclusive end, UTC; `None` is open-ended.
    pub effective_to: Option<OffsetDateTime>,
    /// Where the window stands in §4's machine.
    pub state: WindowState,
    /// The operator-supplied change reason.
    pub reason_code: String,
    /// Who scheduled it — pseudonymous principal id.
    pub created_by: Uuid,
    /// When it was scheduled, UTC.
    pub created_at: OffsetDateTime,
    /// When it took effect; set exactly on an `active` or `expired` window.
    pub activated_at: Option<OffsetDateTime>,
    /// When it expired; set exactly on an `expired` window.
    pub expired_at: Option<OffsetDateTime>,
    /// When it was cancelled; set exactly on a `cancelled` window.
    pub cancelled_at: Option<OffsetDateTime>,
    /// How many **operator acts** this window has been the subject of — `0` at its
    /// schedule, `+1` per adjustment and per cancellation, unmoved by the activation
    /// and expiry sweeps (D-190).
    ///
    /// It is the window's act identity and its entity tag at once, which is why one
    /// column pays two owed items: the act token built from it is distinct across two
    /// acts and stable across a retry of one, and
    /// [`adjust_effective_to`]'s precondition compares against it in the `UPDATE`'s
    /// own `WHERE` (D-191). `pricing_price_window`'s migration doc carries the argument for
    /// the sweeps leaving it alone, and it is the load-bearing half.
    pub mutation_seq: u64,
}

/// Schedule a window on a price row, refusing an overlap on its canonical scope
/// key.
///
/// Three statements in the caller's transaction and their order is the guarantee:
/// resolve the row's key, read every occupying window on that key, then insert.
/// A caller that inserted first and checked after would have to undo a row the
/// trigger forbids it to delete.
///
/// # Errors
/// [`RepoError::NotFound`] when no price row in scope answers to `price_id` —
/// which is what a foreign tenant sees, deliberately indistinguishable from
/// absence, and what makes this refusal a scope gate rather than a foreign-key
/// error. [`RepoError::WindowOverlap`] when the interval intersects one already on
/// the key. [`RepoError::WindowIntervalEmpty`] when the end is not strictly after
/// the start. [`RepoError::TimestampPrecisionExceeded`] on an authored instant
/// finer than the millisecond quantum (D-144). [`RepoError::ConcurrentMutation`]
/// when the id is already taken — the primary key is this table's one
/// serialization point, and a loser there is told to retry rather than told the
/// store failed (D-159). [`RepoError::Db`] on a scope or storage failure.
/// [`RepoError::CorruptRow`] on a stored axis outside its enumeration.
pub async fn schedule(
    runner: &impl DBRunner,
    scope: &AccessScope,
    new: NewWindow,
    stamp: AuditStamp,
) -> Result<WindowRecord, RepoError> {
    check_authored_instant("effectiveFrom", Some(new.effective_from))?;
    check_authored_instant("effectiveTo", new.effective_to)?;
    refuse_empty_interval(new.effective_from, new.effective_to)?;

    let key = price_repo::load_scope_key(runner, scope, new.tenant_id, new.price_id)
        .await?
        .ok_or_else(|| RepoError::NotFound {
            subject: "price".to_owned(),
            id: new.price_id.to_string(),
        })?;

    refuse_overlap(
        runner,
        scope,
        new.tenant_id,
        &key,
        new.effective_from,
        new.effective_to,
        None,
    )
    .await?;

    let am = price_window::ActiveModel {
        window_id: Set(new.window_id),
        tenant_id: Set(new.tenant_id),
        price_id: Set(new.price_id),
        effective_from: Set(new.effective_from),
        effective_to: Set(new.effective_to),
        state: Set(WindowState::Scheduled.as_str().to_owned()),
        reason_code: Set(new.reason_code.clone()),
        created_by: Set(stamp.actor_principal_id),
        created_at: Set(stamp.recorded_at),
        activated_at: Set(None),
        expired_at: Set(None),
        cancelled_at: Set(None),
        // Act zero: the schedule **is** an act on this window, and it is the one act
        // that cannot collide with an earlier one because there is no earlier one.
        // Set explicitly rather than left to the column default, so the row this
        // function returns and the row the store holds agree without a read.
        mutation_seq: Set(0),
    };
    price_window::Entity::insert(am.clone())
        .secure()
        .scope_with_model(scope, &am)
        .map_err(|e| RepoError::Db(format!("pricing_price_window scope: {e}")))?
        .exec(runner)
        .await
        .map_err(|e| {
            overlap_or(
                &e,
                &key,
                new.effective_from,
                new.effective_to,
                &format!("window {}", new.window_id),
                "insert pricing_price_window",
            )
        })?;

    Ok(WindowRecord {
        window_id: new.window_id,
        tenant_id: new.tenant_id,
        price_id: new.price_id,
        scope_key: key,
        effective_from: new.effective_from,
        effective_to: new.effective_to,
        state: WindowState::Scheduled,
        reason_code: new.reason_code,
        created_by: stamp.actor_principal_id,
        created_at: stamp.recorded_at,
        activated_at: None,
        expired_at: None,
        cancelled_at: None,
        mutation_seq: 0,
    })
}

/// Read one window, scoped, with its price row's key resolved.
///
/// `None` means the window does not exist **or** lies outside the caller's scope,
/// deliberately the same answer either way: what a window tells an observer is
/// that a price change is scheduled, and the catalog is commercially sensitive.
///
/// # Errors
/// [`RepoError::CorruptRow`] when the window's price row is missing — the foreign
/// key makes that an invariant breach rather than a caller's mistake — or when a
/// stored token is outside the enumeration its CHECK admits. [`RepoError::Db`] on
/// a scope or storage failure.
pub async fn find(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    window_id: Uuid,
) -> Result<Option<WindowRecord>, RepoError> {
    let row = price_window::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(price_window::Column::TenantId.eq(tenant_id))
                .add(price_window::Column::WindowId.eq(window_id)),
        )
        .one(runner)
        .await
        .map_err(|e| RepoError::Db(format!("read pricing_price_window: {e}")))?;
    let Some(row) = row else {
        return Ok(None);
    };
    let key = price_repo::load_scope_key(runner, scope, tenant_id, row.price_id)
        .await?
        .ok_or_else(|| {
            RepoError::CorruptRow(format!(
                "window {} names price row {}, which does not exist",
                row.window_id, row.price_id
            ))
        })?;
    to_domain(row, key).map(Some)
}

/// Every window of every price row of one plan, oldest interval first.
///
/// The read the coverage checker, the projector and the activation job's per-plan
/// batch all take, which is why it is one function: three callers computing "the
/// plan's windows" from three queries are three answers free to disagree about
/// which intervals a key is covered by.
///
/// Ordered by `effective_from` then `window_id`. `effective_from` because every
/// consumer of this set walks a key's intervals in time order, and `window_id`
/// because two windows may legitimately share a start on **different** keys and a
/// tie broken by the storage engine would page differently on each read. On
/// `SQLite` the instant is `text`, so that first comparison is lexicographic and
/// coincides with chronological order for the canonical fixed-width UTC rendering
/// `SeaORM` writes — `pricing_price`'s `grandfather_until` caveat, one column
/// over.
///
/// **Cancelled and expired windows are included.** This is the store's read and
/// not the projection: D-121 keeps cancelled windows out of the *read model*, and
/// a repository that pre-filtered them would leave the operator's coverage report
/// (§5's `GET …/coverage`) unable to say why a key lost its successor.
///
/// # Errors
/// [`RepoError::CorruptRow`] when a window names a price row that does not exist,
/// or when a stored token is outside its enumeration. [`RepoError::Db`] on a scope
/// or storage failure.
pub async fn list_for_plan(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    plan_id: PlanId,
) -> Result<Vec<WindowRecord>, RepoError> {
    let keys: HashMap<Uuid, ScopeKey> =
        price_repo::load_scope_keys_for_plan(runner, scope, tenant_id, plan_id)
            .await?
            .into_iter()
            .collect();
    if keys.is_empty() {
        return Ok(Vec::new());
    }
    let rows = load_windows(runner, scope, tenant_id, keys.keys().copied(), &[]).await?;
    rows.into_iter()
        .map(|row| {
            let key = keys.get(&row.price_id).cloned().ok_or_else(|| {
                RepoError::CorruptRow(format!(
                    "window {} names price row {}, which is not on plan {plan_id}",
                    row.window_id, row.price_id
                ))
            })?;
            to_domain(row, key)
        })
        .collect()
}

/// One page of the tenant's price windows, in `window_id` order.
///
/// D-125's keyset walk over the window plane — `crate::api::rest::cursor`'s
/// module doc argues once, for every list surface, why the resume is a key and
/// never an offset.
///
/// # Why the narrowing parameter is `price_id` and not `plan_id`
///
/// Because `pricing_price_window` has no plan reference and is not meant to:
/// [`super::super::entity::price_window`]'s module doc records that the plan and
/// the canonical scope key both live on `pricing_price`, and that a window is
/// bound to a **row** rather than to a key precisely so the two cannot disagree.
/// A `plan_id` filter here would therefore be a join this repository invents, and
/// the plan-shaped question already has its own answer in [`list_for_plan`].
///
/// **Every state is included**, [`list_for_plan`]'s reason exactly: a cancelled
/// window is what explains why a key lost its successor, and a repository that
/// pre-filtered it would put that answer out of reach of the surface that needs
/// it.
///
/// The keys are resolved in **one** further query for the whole page
/// ([`price_repo::load_scope_keys_for_ids`]) rather than one per window, so a
/// hundred-row page costs two round trips and not a hundred and one.
///
/// # Errors
/// [`RepoError::CorruptRow`] when a window names a price row that does not exist
/// — the foreign key makes that an invariant breach rather than a caller's
/// mistake — or when a stored token is outside its enumeration. [`RepoError::Db`]
/// on a scope or storage failure.
pub async fn list_page(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    price_id: Option<Uuid>,
    after: Option<Uuid>,
    limit: u64,
) -> Result<Vec<WindowRecord>, RepoError> {
    let mut filter = Condition::all().add(price_window::Column::TenantId.eq(tenant_id));
    if let Some(price_id) = price_id {
        filter = filter.add(price_window::Column::PriceId.eq(price_id));
    }
    if let Some(after) = after {
        filter = filter.add(price_window::Column::WindowId.gt(after));
    }
    let rows = price_window::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(filter)
        .order_by(price_window::Column::WindowId, Order::Asc)
        .limit(limit)
        .all(runner)
        .await
        .map_err(|e| RepoError::Db(format!("list pricing_price_window: {e}")))?;

    let mut price_ids: Vec<Uuid> = rows.iter().map(|row| row.price_id).collect();
    price_ids.sort_unstable();
    price_ids.dedup();
    // Indexed once rather than scanned per row: the mapping closure below runs for
    // every row of a page capped at `LIST_LIMIT_CFG`, and a linear `find` over the
    // key set made that `page x price_ids` comparisons.
    let keys: HashMap<Uuid, ScopeKey> =
        price_repo::load_scope_keys_for_ids(runner, scope, tenant_id, &price_ids)
            .await?
            .into_iter()
            .collect();

    rows.into_iter()
        .map(|row| {
            let key = keys.get(&row.price_id).cloned().ok_or_else(|| {
                RepoError::CorruptRow(format!(
                    "window {} names price row {}, which does not exist",
                    row.window_id, row.price_id
                ))
            })?;
            to_domain(row, key)
        })
        .collect()
}

/// One `OData` page of the tenant's windows. `$filter` is additive over the
/// tenant + `SecureORM` scope. Default order is `window_id asc`.
///
/// # Errors
/// [`OdataPageError::Db`] on storage failure; [`OdataPageError::Odata`] on a
/// malformed `$filter` / `$orderby` / cursor.
pub async fn list_odata(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    query: &ODataQuery,
) -> Result<Page<WindowRecord>, OdataPageError> {
    let base_select = price_window::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(Condition::all().add(price_window::Column::TenantId.eq(tenant_id)));
    let query = query_with_default_order(query, &[WindowFilterField::WindowId]);
    let page = paginate_odata::<
        WindowFilterField,
        WindowODataMapper,
        price_window::Entity,
        price_window::Model,
        _,
        _,
    >(
        base_select,
        runner,
        &query,
        ("window_id", SortDir::Asc),
        LIST_LIMIT_CFG,
        |m| m,
    )
    .await
    .map_err(map_odata_err)?;

    let mut price_ids: Vec<Uuid> = page.items.iter().map(|row| row.price_id).collect();
    price_ids.sort_unstable();
    price_ids.dedup();
    // Indexed once, for the reason `load_for_plan`'s twin above carries.
    let keys: HashMap<Uuid, ScopeKey> =
        price_repo::load_scope_keys_for_ids(runner, scope, tenant_id, &price_ids)
            .await
            .map_err(OdataPageError::Repo)?
            .into_iter()
            .collect();

    domain_page(page, |row| {
        let key = keys.get(&row.price_id).cloned().ok_or_else(|| {
            RepoError::CorruptRow(format!(
                "window {} names price row {}, which does not exist",
                row.window_id, row.price_id
            ))
        })?;
        to_domain(row, key)
    })
}

/// Which of §4's two **time-driven** boundaries a sweep is asking about.
///
/// Two members and not three: `scheduled → cancelled` is an operator's act
/// (`inst-ws-cancel`), so no instant makes a window "due" for it and no sweep
/// ever asks. The two that are here are the two §4 states as *conditions* —
/// `inst-ws-activate` WHEN `now ≥ effectiveFrom`, `inst-ws-expire` WHEN
/// `now ≥ effectiveTo` — which is why one type carries both the state the
/// boundary leaves and the state it leads to rather than the caller pairing them
/// up at each site.
/// It carries **no ordering**: which of the two comes first when both fall on one
/// instant is a rule about changeovers, and
/// `crate::infra::jobs::window_activation`'s own `boundary_rank` states it. A
/// derived `Ord` here would be a second answer to that, decided by declaration
/// order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DueBoundary {
    /// `effectiveFrom` has arrived on a `scheduled` window.
    Activation,
    /// `effectiveTo` has arrived on an `active` one.
    Expiry,
}

impl DueBoundary {
    /// The state a window stands in while this boundary is still ahead of it.
    ///
    /// Not spelled `from_state`: clippy's `wrong_self_convention` reads a
    /// `from_*` method as a constructor, and this is a projection of the value.
    #[must_use]
    pub const fn origin_state(self) -> WindowState {
        match self {
            Self::Activation => WindowState::Scheduled,
            Self::Expiry => WindowState::Active,
        }
    }

    /// The state crossing it moves the window to.
    #[must_use]
    pub const fn target_state(self) -> WindowState {
        match self {
            Self::Activation => WindowState::Active,
            Self::Expiry => WindowState::Expired,
        }
    }
}

/// A window whose §4 boundary has arrived, as a sweep needs it.
///
/// Not a [`WindowRecord`], and the difference is the point. A record carries the
/// **canonical scope key**, which costs a `pricing_price` read per row and
/// answers the question non-overlap and coverage have; a sweep's question is
/// which aggregate the event belongs to, so this carries the `plan_id` and
/// nothing else off the parent row. On a page of a thousand due windows that is
/// two queries instead of a thousand and one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DueWindow {
    /// The window's durable name.
    pub window_id: Uuid,
    /// RLS scope — what the caller narrows to before writing.
    pub tenant_id: Uuid,
    /// The price row the interval belongs to.
    pub price_id: Uuid,
    /// The plan the row is on: the aggregate §7 orders this window's events
    /// within.
    pub plan_id: PlanId,
    /// Inclusive start, UTC.
    pub effective_from: OffsetDateTime,
    /// Exclusive end, UTC; `None` is open-ended.
    pub effective_to: Option<OffsetDateTime>,
    /// Which boundary arrived.
    pub boundary: DueBoundary,
    /// **The instant that boundary is** — `effective_from` for an activation,
    /// `effective_to` for an expiry.
    ///
    /// Non-optional, which is what makes the open-ended case unrepresentable
    /// rather than merely filtered: an open-ended window has no expiry instant,
    /// so there is no value this field could hold and no flip a caller could
    /// stamp. [`list_due`] refuses such a row rather than dropping it, because a
    /// row the expiry predicate returned without an end means the predicate and
    /// this mapping have stopped agreeing.
    pub at: OffsetDateTime,
}

/// Every window whose `boundary` has arrived by `at`, oldest boundary first.
///
/// **§10's read**: *"the activation job scans by `(state, effective_from)` index
/// in batches"*, which is `idx_pricing_price_window_due`. Cross-tenant by
/// construction — no `tenant_id` argument — because one sweep is one pass over
/// every tenant; the sanctioned [`AccessScope::allow_all`] system scope is what
/// admits that, and the caller narrows to `AccessScope::for_tenant` before it
/// writes anything (`crate::infra::jobs` states the rule).
///
/// # The predicate is the whole of the sweep's selection, and all three parts
/// earn their keep
///
/// `state = <the boundary's from-state>`, the boundary instant `<= at`, and the
/// **price row's lifecycle state** — §4's edge, §4's condition, and D-121's row
/// set. None is redundant with the store's own guards and none with the others:
///
/// * the **state** half is what keeps history out, and it keeps a *permanently
///   false alarm* out as well as a failed attempt. A `cancelled` window whose
///   start has passed satisfies the instant half forever, and handing it to
///   [`transition`] as an activation would be refused there — so without this
///   half a pass reports one failure per cancelled window per tick. It would also
///   count every such window `overdue`, because the overdue condition is read off
///   the due set: `pricing.window.activation_overdue` — the Warn that means the
///   lease singleton is **stalled** — would fire on settled history, on every
///   tick, forever. A failure counter and a permanently false alarm are not the
///   same stake, and removing this half moves both: most of the sweep's suite
///   reddens, and most of those reports gain an `overdue` beside the `failed`
///   (`an_open_ended_window_never_expires` goes from reporting nothing at all to
///   `{windows_due: 1, failed: 1, overdue: 1}`, which is the clearest single case).
/// * the **instant** half is the only thing bounding the sweep to windows whose
///   time has come. [`transition`]'s own `effective_from <= at` cannot stand in
///   for it: `at` is the instant §4 puts the transition at, so a caller that
///   selected a window starting next year and passed that start as `at` would
///   satisfy the store's predicate trivially. The store's clause guards the race
///   between a read and a write; this one guards the selection.
/// * the **lifecycle** half puts the sweep on the **published** side, and it is
///   the one part of this predicate that is about the parent row rather than the
///   window. It is a subquery over `pricing_price` rather than a filter applied
///   after the page, so a row that can never flip cannot consume the page either
///   — see the paging note below, and `projected_price_rows`.
///
/// `effective_to IS NOT NULL` rides with the expiry half rather than being a
/// fourth rule: *"an open-ended window never expires"* (`inst-ws-expire`), and
/// on this read it is also what makes [`DueWindow::at`] constructible.
///
/// # Why the sweep is draft-**exclusive** where the overlap check is
/// draft-inclusive
///
/// The two are different kinds of question and the distinction is written here
/// because its absence is how the opposite behaviour arrived — inherited by
/// default and never argued.
///
/// * The **overlap check** and the coverage-at-publish check are *validation over
///   a hypothetical*: "if this row were current on this key, would the interval
///   set be sound?" A draft row's windows have to be in that comparison, because
///   `inst-wc-required` fails a publish whose key has no covering window, so
///   coverage is authored **before** the row publishes.
///   `price_repo::load_scope_keys_for_plan` states that as substance rather than
///   omission and it is untouched; this read does not use it.
/// * The **projection** and this **sweep** are *assertion of fact to a consumer*.
///   `PriceWindowActivated` states that a price took effect; a draft row is
///   addressable at no `CatalogVersion` and resolvable by no pin, so there is no
///   consumer for whom that could be true. And the sweep's only reader —
///   `read_model::project_windows` — restricts to [`PROJECTED_ROW_STATES`] by
///   `price_id`, so this is not a seam between two consumers but a producer that
///   would otherwise disagree with its single reader.
///
/// **The sharpest form is not the false event but the state loss.** `expired` is
/// terminal ([`WindowState::may_move_to`] has no edge back to `scheduled` or
/// `active`), so a sweep that flipped a draft row's windows would drive any window
/// whose interval passed while its row was unpublished to terminal `expired` —
/// permanently losing its ability to ever take effect, and needing nothing more
/// exotic than a draft that publishes later than its own window's end.
///
/// ## What that costs, stated rather than left to be discovered
///
/// * **Windows on a `draft` row never flip.** `PROJECTED_ROW_STATES` is
///   `{published, superseded}` and `chk_pricing_price_lifecycle_state` admits
///   exactly `draft | published | superseded`, so `draft` is the whole of the
///   excluded set on this table. ([`LifecycleState`](crate::domain::lifecycle::LifecycleState)
///   has five variants, but `abandoned` and `retired` are plan-revision states the
///   price row's CHECK does not admit — so there is no such thing as a retired
///   price row for a window to be stranded on.)
/// * **A frozen `scheduled` window keeps occupying its canonical scope key**,
///   [`OCCUPYING_STATES`] including `scheduled`. That is bounded and it is the
///   right direction: the overlap read is per plan, a row that publishes flips on
///   the very next tick — late, and *visibly* late through the overdue alarm the
///   pass raises — and a discarded draft self-cleans, because `abandoned` is not a
///   price-row state and draft price rows stay deletable. A visible refusal beats
///   a durable false assertion.
/// * **No data repair is owed.** Nothing drains `pricing_outbox` in this
///   repository (`published_at` is NULL and stays NULL, and the relay does not
///   exist), so no false event has reached a consumer.
///
/// `limit` bounds the page, and a full page is a fact the caller **does** act on:
/// the sweep spends one budget across both boundaries and reads expiries first, so
/// a saturated expiry page leaves an activation read no room at all
/// (`jobs::window_activation`'s `activation_budget` states why). There are then due
/// windows this pass did not see, and the next pass takes them with their
/// boundaries older, which is what the overdue alarm is measured on.
/// Ordered by the boundary instant then `window_id`: the boundary because a
/// backlog must drain in the order it accumulated, and the id because two
/// windows legitimately share a boundary (a changeover, adjacency) and a tie
/// broken by the storage engine would page differently on each read.
///
/// # Errors
/// [`RepoError::Db`] on a scope or storage failure. [`RepoError::CorruptRow`]
/// when a row selected as due to expire carries no `effective_to`, which the
/// predicate excludes, or when a stored token is outside its enumeration.
pub async fn list_due(
    runner: &impl DBRunner,
    scope: &AccessScope,
    boundary: DueBoundary,
    at: OffsetDateTime,
    limit: u64,
) -> Result<Vec<DueWindow>, RepoError> {
    let (column, filter) = match boundary {
        DueBoundary::Activation => (
            price_window::Column::EffectiveFrom,
            Condition::all().add(price_window::Column::EffectiveFrom.lte(sql_instant(at))),
        ),
        DueBoundary::Expiry => (
            price_window::Column::EffectiveTo,
            Condition::all()
                .add(price_window::Column::EffectiveTo.is_not_null())
                .add(price_window::Column::EffectiveTo.lte(sql_instant(at))),
        ),
    };
    let rows = price_window::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            filter
                .add(price_window::Column::State.eq(boundary.origin_state().as_str()))
                .add(price_window::Column::PriceId.in_subquery(projected_price_rows())),
        )
        .order_by(column, Order::Asc)
        .order_by(price_window::Column::WindowId, Order::Asc)
        .limit(limit)
        .all(runner)
        .await
        .map_err(|e| RepoError::Db(format!("list due pricing_price_window rows: {e}")))?;
    if rows.is_empty() {
        return Ok(Vec::new());
    }

    let plans =
        price_repo::load_plan_ids(runner, scope, rows.iter().map(|row| row.price_id)).await?;
    rows.into_iter()
        .map(|row| {
            let plan_id = plans
                .iter()
                .find(|(price_id, _)| *price_id == row.price_id)
                .map(|(_, plan_id)| *plan_id)
                .ok_or_else(|| {
                    RepoError::CorruptRow(format!(
                        "window {} names price row {}, which does not exist",
                        row.window_id, row.price_id
                    ))
                })?;
            let at = match boundary {
                DueBoundary::Activation => row.effective_from,
                DueBoundary::Expiry => row.effective_to.ok_or_else(|| {
                    RepoError::CorruptRow(format!(
                        "window {} was read as due to expire and carries no effective_to; an \
                         open-ended window never expires",
                        row.window_id
                    ))
                })?,
            };
            Ok(DueWindow {
                window_id: row.window_id,
                tenant_id: row.tenant_id,
                price_id: row.price_id,
                plan_id,
                effective_from: row.effective_from,
                effective_to: row.effective_to,
                boundary,
                at,
            })
        })
        .collect()
}

/// The price rows a fact may be asserted about: `SELECT price_id FROM
/// pricing_price WHERE lifecycle_state IN (…)`, over [`PROJECTED_ROW_STATES`].
///
/// **A subquery rather than a filter applied to the page** [`list_due`] already
/// read, and the difference is a starvation the second shape would introduce. A
/// window whose row stays a draft past its own boundary is due by the instant half
/// forever, and the page is ordered by the boundary instant ascending — so
/// unflippable rows would accumulate at the **head** of every page, and once
/// enough of them existed the sweep would stop reaching any window it could
/// actually flip, silently: dropped after the read, they raise no alarm on the way
/// out. Excluded in the statement they consume no page at all.
///
/// The subquery is not scope-narrowed and does not need to be. It yields
/// `price_id`s only, `price_id` is a primary key, and a window's `price_id` names
/// the row of its own tenant — so intersecting it with a scoped outer read cannot
/// admit a window the outer scope did not already admit. The sweep reads under the
/// sanctioned [`AccessScope::allow_all`] system scope in any case.
///
/// The state set is [`PROJECTED_ROW_STATES`] itself rather than a second roster
/// spelling the same two tokens: the reason this predicate exists is that the
/// producer must agree with its single reader, and two rosters are two answers.
fn projected_price_rows() -> SelectStatement {
    price::Entity::find()
        .select_only()
        .column(price::Column::PriceId)
        .filter(
            price::Column::LifecycleState
                .is_in(PROJECTED_ROW_STATES.iter().map(|state| state.as_str())),
        )
        .into_query()
}

/// Move one window along §4's machine, stamping the instant of the flip.
///
/// **Idempotent on the two states a clock decides, and on no others.** A window that
/// already stands `active` or `expired` is returned unchanged rather than refused,
/// and that is what makes a re-driven activation sweep safe: the job flips
/// `(tenant, plan)` batches under a lease it can lose, so the second run necessarily
/// re-reads windows the first one already moved. It is not a widening of the machine
/// — [`WindowState::may_move_to`] still answers `false` for a self-edge, and the
/// row's `activated_at` is **not** re-stamped, so the instant a price took effect is
/// written once. A **second cancellation is refused**, because no clock decides one:
/// see `idempotent_arrival` below, which is where both the pre-check and the zero-rows
/// re-read ask which arrivals are a no-op.
///
/// # `at` is the edge's **condition** as well as its timestamp, and the `UPDATE`
/// carries it
///
/// §4's edges are conditional: `inst-ws-activate` fires WHEN
/// `now ≥ effectiveFrom`, `inst-ws-expire` WHEN `now ≥ effectiveTo`, and *"an
/// open-ended window never expires"*. Both conditions are functions of `to` and
/// `at` and of nothing else, so **the interface takes no new parameter to express
/// them** — `at`'s contract is tightened instead: it is the instant the caller
/// asserts the boundary was crossed at, and a flip whose boundary `at` has not
/// reached is refused rather than performed. That is a narrower contract than
/// before and it is the shape chosen deliberately over a separate `premise`
/// argument, whose only possible values would have been the two instants already
/// on the row.
///
/// The condition then goes **into the statement**, beside the state predicate:
/// `WHERE … AND state = <expected> AND effective_from <= at` for an activation,
/// `… AND state = 'active' AND effective_to IS NOT NULL AND effective_to <= at`
/// for an expiry, nothing extra for a cancellation (`inst-ws-cancel` is
/// "cancelled before activation", which `state = 'scheduled'` already carries).
/// This is what an activation sweep needs from a store, and the reason it is a
/// `WHERE` rather than only a pre-check: with the condition in the statement a
/// second pass over the same instant matches **zero rows** and needs no marker
/// column, and a concurrent writer cannot step between the read and the write.
/// A scan-then-flip built on the pre-check alone would be a race.
///
/// `at` remains a separate argument from `stamp.recorded_at` because the two are
/// still not one fact: the sweep instant is when the flip was *recorded*, `at` is
/// where §4 puts the transition, and a job running behind its SLO makes the
/// difference observable. **`at` is not subject to D-144's quantum.** That quantum
/// is an *authoring* rule — §5 scopes it to `effectiveFrom`/`effectiveTo`, the
/// cutover instant, the D-88 changeover instant and `grandfatherUntil` — and this
/// is a machine-generated flip timestamp. `OffsetDateTime::now_utc()` carries sub-millisecond
/// precision, so applying it here failed **every** flip an activation sweep would
/// ever attempt with `TIMESTAMP_PRECISION_EXCEEDED`; [`schedule`] does not apply
/// it to `stamp.recorded_at` either, for the same reason.
///
/// # Errors
/// [`RepoError::NotFound`] when no window in scope answers to `window_id`.
/// [`RepoError::WindowStateForbidden`] when the window's state does not admit the
/// edge, when the edge's §4 condition is unmet at `at`, or when a concurrent flip
/// landed first. [`RepoError::ConcurrentMutation`] when the row moved under a
/// premise that still holds. [`RepoError::WindowOverlap`] when an activation
/// re-offers its interval to `excl_pricing_price_window_no_overlap` and the key is already occupied —
/// reachable only on a database that carries an overlap the constraint was applied
/// over, never on one it has guarded from the start.
/// [`RepoError::Db`] on a scope or storage failure;
/// [`RepoError::CorruptRow`] on a stored token outside its enumeration or a window
/// whose price row is gone.
pub async fn transition(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    window_id: Uuid,
    to: WindowState,
    at: OffsetDateTime,
    _stamp: AuditStamp,
) -> Result<WindowRecord, RepoError> {
    let current = require(runner, scope, tenant_id, window_id).await?;
    if current.state == to && idempotent_arrival(to) {
        return Ok(current);
    }
    refuse_unsanctioned_edge(&current, to, at)?;

    let column = match to {
        WindowState::Active => price_window::Column::ActivatedAt,
        WindowState::Expired => price_window::Column::ExpiredAt,
        WindowState::Cancelled => price_window::Column::CancelledAt,
        // Unreachable: nothing may move *to* `scheduled` — the state has no
        // inbound edge and `may_move_to` refused above. Named rather than
        // wildcarded so a fourth state cannot fall through to whichever column
        // happened to be the `else`.
        WindowState::Scheduled => {
            return Err(forbidden(
                window_id,
                current.state,
                "the transition to scheduled",
            ));
        }
    };

    // **Only the operator's edge advances the act sequence.** §4 has three edges and
    // two of them are the clock's: a sweep that advanced the counter would move a
    // window's act identity with no operator act, and the retry that follows an
    // approve would then name a subject no unit was opened under — D-184's approval
    // loop with no exit, reached through the clock. `pricing_price_window`'s migration doc
    // carries the argument; `inst-ws-cancel` is the one edge an operator drives.
    let advances = matches!(to, WindowState::Cancelled);
    let result = price_window::Entity::update_many()
        .secure()
        .scope_with(scope)
        .col_expr(price_window::Column::State, Expr::value(to.as_str()))
        .col_expr(column, Expr::value(sql_instant(at)))
        .col_expr(
            price_window::Column::MutationSeq,
            Expr::value(advanced_seq(window_id, current.mutation_seq, advances)?),
        )
        .filter(
            Condition::all()
                .add(price_window::Column::TenantId.eq(tenant_id))
                .add(price_window::Column::WindowId.eq(window_id))
                .add(price_window::Column::State.eq(current.state.as_str()))
                // **The act sequence is a precondition, not only a write.** Without
                // it this statement writes `mutation_seq` back at the value the read
                // at the top of the function saw, so an `adjust_effective_to` that
                // commits in between has its bump silently reverted — N+1 back to N.
                // `mutation_seq` is the D-191 entity tag *and* the window's act
                // identity, so a revert hands a caller holding the stale
                // `If-Match: "N"` a second adjustment the compare-and-swap exists to
                // refuse, and lets two operator acts mint one act token. With the
                // predicate the moved row matches nothing and falls into the
                // zero-rows re-read below, which re-asks the edge against `fresh`.
                .add(
                    price_window::Column::MutationSeq
                        .eq(stored_seq(window_id, current.mutation_seq)?),
                )
                .add(edge_condition(to, at)),
        )
        .exec(runner)
        .await
        .map_err(|e| {
            // **The narrow sibling of [`adjust_effective_to`]'s recognizer, and it
            // is narrow for a stated reason.** This statement moves no boundary, so
            // the only edge that can trip `excl_pricing_price_window_no_overlap` is
            // `scheduled -> active`: it is the one flip whose row is inside the
            // constraint's partial predicate both before and after, and it
            // therefore re-offers the row's interval to the exclusion index. On a
            // database the constraint has always guarded there is nothing for it to
            // collide with; on one that already carries an overlap — a chain
            // applied over rows written before D-352, which the migration validates
            // rather than repairs — the activation sweep is what meets it, and a
            // sweep told "an internal error occurred, retry later" retries forever.
            // `WINDOW_OVERLAP` names the two windows an operator has to reconcile
            // instead.
            overlap_or(
                &e,
                &current.scope_key,
                current.effective_from,
                current.effective_to,
                &format!("window {window_id}"),
                &format!("transition pricing_price_window {window_id}"),
            )
        })?;

    if result.rows_affected == 0 {
        let fresh = require(runner, scope, tenant_id, window_id).await?;
        if fresh.state == to && idempotent_arrival(to) {
            return Ok(fresh);
        }
        // The statement matched nothing, so the state moved, the §4 condition
        // stopped holding of the row the statement saw, or a sibling act advanced
        // the sequence. The first two are re-asked against the fresh read, so the
        // refusal names whichever it was rather than reporting the state when it
        // was the condition.
        refuse_unsanctioned_edge(&fresh, to, at)?;
        // None of them names it: the sequence moved, or the row moved and moved
        // back — and neither is this window's state machine refusing. Told to retry
        // rather than told the window forbids it (D-159).
        return Err(RepoError::ConcurrentMutation {
            aggregate: format!("window {window_id}"),
        });
    }

    let stamped = sql_instant(at);
    Ok(WindowRecord {
        state: to,
        activated_at: pick(to, WindowState::Active, stamped, current.activated_at),
        expired_at: pick(to, WindowState::Expired, stamped, current.expired_at),
        cancelled_at: pick(to, WindowState::Cancelled, stamped, current.cancelled_at),
        mutation_seq: if advances {
            current.mutation_seq.saturating_add(1)
        } else {
            current.mutation_seq
        },
        ..current
    })
}

/// The caller's submitted sequence, as a precondition operand.
///
/// Separate from [`advanced_seq`] because the two read different people's
/// numbers, and only one of them can be the store's fault. `advanced_seq`
/// converts a sequence **the row holds**, so a failure there really is a corrupt
/// row. This converts one the **caller sent** in `If-Match`, and
/// `RowVersion::from_etag` accepts any `u64` -- so `If-Match: "9223372036854775808"`
/// reached `advanced_seq` and came back as `CorruptRow`, which
/// `repo_failure` renders **500 Internal** blaming
/// `pricing_price_window.mutation_seq of window {id}` for a value the column
/// never held. An operator was sent to inspect a row that is fine.
///
/// A tag no row can carry cannot match the row, so the answer is the ordinary
/// stale-version refusal. The price-row door already answers exactly that for
/// the same input -- measured, 409 naming current and submitted -- and this
/// brings the window door level with it.
fn storable_seq(window_id: Uuid, submitted: u64, current: u64) -> Result<i64, RepoError> {
    i64::try_from(submitted).map_err(|_| RepoError::StaleRowVersion {
        subject: "window".to_owned(),
        id: window_id.to_string(),
        current,
        submitted,
    })
}

/// The act sequence one act on from `current`, in the signed shape the column holds.
///
/// `advance` is false for the clock's two edges, which write the number back
/// unchanged rather than skipping the assignment: one statement shape for all three
/// edges, and a same-value write is what the sixth trigger arm admits.
///
/// The sequence it converts is one **the row holds**, which is why the refusal
/// here is [`RepoError::CorruptRow`] and [`storable_seq`]'s is a stale-version
/// answer: a stored value past `i64::MAX` is a column this crate wrote and cannot
/// read back, and no caller can be sent to fix their own request over it.
fn advanced_seq(window_id: Uuid, current: u64, advance: bool) -> Result<i64, RepoError> {
    let next = if advance {
        current.saturating_add(1)
    } else {
        current
    };
    stored_seq(window_id, next)
}

/// `mutation_seq` as the column holds it.
///
/// The checked conversion every read-side use of the sequence owes, and the
/// counterpart of [`storable_seq`]: the input here came *out* of an `i64` column,
/// so a value that will not fit is a corrupt row rather than a caller's stale tag.
/// Binding the `u64` raw instead would put a `BigUnsigned` beside an `i64` column
/// and leave the refusal to the driver, in whichever shape the engine gives it.
fn stored_seq(window_id: Uuid, seq: u64) -> Result<i64, RepoError> {
    i64::try_from(seq).map_err(|_| {
        RepoError::CorruptRow(format!(
            "pricing_price_window.mutation_seq of window {window_id} is past what the column \
             can hold at {seq}"
        ))
    })
}

/// Move a window's exclusive end — the shorten and extend of `inst-ws-immutable`.
///
/// The overlap check runs again and it has to: an extension is a new claim on the
/// key, and the interval it claims may be one a later window already holds. The
/// window itself is excluded from the comparison, or every adjustment would
/// collide with the interval it is replacing.
///
/// `None` is the open-ended end (`inst-ws-expire`), and it is a legal target: it
/// removes a bound rather than any coverage.
///
/// # `inst-ws-immutable` is answered here, against the **stamp's** clock
///
/// Three refusals, all [`RepoError::WindowHistorical`] and all mirroring
/// `trg_pricing_price_window_append_only`'s arms 2 and 5: a terminal window moves
/// nothing, an end that has already passed cannot be moved even forward, and an
/// end cannot be moved *to* a past instant. Before the pre-check the second and
/// third met the trigger and reached the caller as a **500**.
///
/// The instant compared against is `stamp.recorded_at` — **the caller's**, not the
/// database's, and the difference is real rather than pedantic. The trigger
/// compares against `now()`, so the two can disagree by the clock skew between an
/// application node and the server, and a request that this check admits can still
/// be refused by the trigger. That ordering is the safe one: the pre-check is the
/// caller's answer about the request they composed, and the trigger is the floor
/// nothing gets under. It is not the other way round, and it must not become so —
/// a pre-check made *more* permissive than the trigger only moves where the 500
/// comes from.
///
/// # What is **not** checked here that §6 names this function for, and where it is
///
/// D-04: a grandfathered generation's window `effective_to` MUST stay
/// `>= grandfather_until + the longest billing cycle sold on the key`, *"enforced at
/// cutover and on every `effectiveTo` adjustment"*. This statement is still not
/// made **here**, and it is no longer unmade: it is
/// [`crate::infra::window`]'s `refuse_horizon_uncovered`, beside
/// `refuse_trailing_void`, run inside the same transaction over every window
/// mutation.
///
/// It could not be made here and still cannot. The rule's operands are the
/// **row's** `grandfather_until` and W6's margin — the longest `frequency` among
/// the plan's recurring rows on the key's `(currency, region)` — and this
/// function holds one window and its key. The doc that stood here said the margin
/// *"has no producer anywhere in this crate"*, which was true when it was written
/// and stopped being true when `domain::coverage::longest_cycle_sold` landed for
/// D-80's horizon; the layer that reads it is the one that assembles the plan
/// shape. So this sentence stays for its original purpose — nobody should read
/// the three refusals above as the complete set of what a shorten is judged
/// against — with the owner named rather than the gap.
///
/// # The `expected_seq` precondition
///
/// `expected_seq` is the act sequence the caller read the window at — D-191's
/// `If-Match`, arriving here because this is where it can be compared without a race.
/// It is **not optional**: a precondition that disappears when it is not supplied is
/// a precondition that disappears exactly when two writers are racing, and the route
/// requires the header (D-171). The comparison and the advance are one statement.
///
/// # Errors
/// [`RepoError::NotFound`] when no window in scope answers to `window_id`.
/// [`RepoError::WindowHistorical`] for the three refusals above.
/// [`RepoError::StaleRowVersion`] naming both sequences when the window has been
/// acted on since `expected_seq` was read.
/// [`RepoError::WindowIntervalEmpty`] when the new end is not strictly after the
/// start. [`RepoError::WindowOverlap`] when the new interval intersects a sibling
/// on the key — from the pre-check when the sibling was already committed when
/// this transaction read, and from `excl_pricing_price_window_no_overlap` by way of [`overlap_or`]
/// when it committed between that read and the statement below.
/// [`RepoError::ConcurrentMutation`] when the row moved without becoming frozen,
/// which is somebody else's flip having landed first.
/// [`RepoError::TimestampPrecisionExceeded`] on an instant finer than
/// the millisecond quantum. [`RepoError::Db`] on a scope or storage failure.
/// [`RepoError::CorruptRow`] on a stored token outside its enumeration.
pub async fn adjust_effective_to(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    window_id: Uuid,
    effective_to: Option<OffsetDateTime>,
    expected_seq: u64,
    stamp: AuditStamp,
) -> Result<WindowRecord, RepoError> {
    check_authored_instant("effectiveTo", effective_to)?;
    let current = require(runner, scope, tenant_id, window_id).await?;
    refuse_frozen_end(&current, effective_to, stamp.recorded_at)?;
    refuse_empty_interval(current.effective_from, effective_to)?;

    refuse_overlap(
        runner,
        scope,
        tenant_id,
        &current.scope_key,
        current.effective_from,
        effective_to,
        Some(window_id),
    )
    .await?;

    // **The precondition rides in the statement, not in a comparison before it.**
    // `api::rest::preconditions`' doc draws the line: that module refuses a request it
    // cannot understand, and "the repository's compare-and-swap is the authority" on
    // one whose premise has moved — because a tag read, compared and then handed to a
    // statement is a decision racing the write it authorizes. So `mutation_seq` joins
    // the `WHERE` beside the state, and the same statement advances it (D-191).
    let result = price_window::Entity::update_many()
        .secure()
        .scope_with(scope)
        .col_expr(price_window::Column::EffectiveTo, Expr::value(effective_to))
        .col_expr(
            price_window::Column::MutationSeq,
            Expr::value(advanced_seq(window_id, current.mutation_seq, true)?),
        )
        .filter(
            Condition::all()
                .add(price_window::Column::TenantId.eq(tenant_id))
                .add(price_window::Column::WindowId.eq(window_id))
                .add(price_window::Column::State.eq(current.state.as_str()))
                .add(price_window::Column::MutationSeq.eq(storable_seq(
                    window_id,
                    expected_seq,
                    current.mutation_seq,
                )?)),
        )
        .exec(runner)
        .await
        .map_err(|e| {
            // **Through [`overlap_or`], for the same reason [`schedule`]'s INSERT
            // is.** `refuse_overlap` above answered for the world this transaction
            // read, and an extension is the one adjustment that makes a *new* claim
            // on the key — so the writer that loses the race between that read and
            // this statement is exactly the caller `excl_pricing_price_window_no_overlap` exists for.
            // Left as [`RepoError::Db`] the exclusion violation reached them as a
            // bare 500 advising a retry that would never come good, while the
            // uncontended path one screen up named the collision. The constraint
            // fires on an UPDATE of these columns as readily as on an INSERT —
            // `EXCLUDE` ranges over `effective_from`/`effective_to`/`state`, and the
            // `SQLite` mirror carries a `BEFORE UPDATE` arm for precisely this
            // statement. Everything else this statement can fail with still falls
            // through to `contention_or_db`.
            overlap_or(
                &e,
                &current.scope_key,
                current.effective_from,
                effective_to,
                &format!("window {window_id}"),
                &format!("adjust pricing_price_window {window_id}"),
            )
        })?;

    if result.rows_affected == 0 {
        // Either predicate can have missed, so the fresh read decides which, and the
        // sequence is asked first: an act sequence that has moved means another
        // **act** landed, which is the caller's precondition and not this window's
        // state machine. The two cannot both be the answer — the clock's edges leave
        // the sequence alone (`pricing_price_window`) — so asking in this order names the
        // cause rather than whichever check happens to be written first.
        let fresh = require(runner, scope, tenant_id, window_id).await?;
        if fresh.mutation_seq != expected_seq {
            return Err(RepoError::StaleRowVersion {
                subject: "price window".to_owned(),
                id: window_id.to_string(),
                current: fresh.mutation_seq,
                submitted: expected_seq,
            });
        }
        refuse_frozen_end(&fresh, effective_to, stamp.recorded_at)?;
        // It moved without becoming frozen, and `scheduled -> active` is the only
        // such move: the adjustment is still perfectly legal and what happened is
        // that somebody's flip got here first. Told to retry rather than told the
        // window forbids it (D-159) — a caller sent to look at their own request
        // would find nothing wrong with it.
        return Err(RepoError::ConcurrentMutation {
            aggregate: format!("window {window_id}"),
        });
    }

    Ok(WindowRecord {
        effective_to,
        mutation_seq: current.mutation_seq.saturating_add(1),
        ..current
    })
}

/// The schema's own overlap refusal, recognized back into the domain's.
///
/// The non-overlap rule lives in the schema as `excl_pricing_price_window_no_overlap`
/// — an `EXCLUDE USING gist` on Postgres, a `RAISE(ABORT)` trigger pair on `SQLite`
/// — because [`refuse_overlap`] below is a read and its caller inserts afterwards,
/// so two concurrent writers both saw the key free. The constraint closes that,
/// and **without this recognizer it would close it into a 500**: the violation
/// is SQLSTATE `23P01` (`exclusion_violation`), and `is_unique_violation` — the
/// only contention shape `contention_or_db` knows — matches `23505` alone. The
/// caller who lost a race would get "an internal error occurred, retry later",
/// which is both wrong and unactionable, in place of the named refusal the
/// uncontended path already gives them.
///
/// Keyed on the constraint's own name and on the trigger's abort text, one per
/// engine, because those are the only two markers the two backends share nothing
/// else of. A message that matches neither falls through to
/// [`contention_or_db`], which is the correct home for every other failure of
/// this statement.
///
/// # It belongs on **every** statement that writes the table, not on the one that
/// was written first
///
/// A recognizer wired into one door and not its siblings is not a recognizer —
/// it is a door that happens to answer well. `pricing_price_window` has exactly
/// three writers and all three are in this module: [`schedule`]'s INSERT,
/// [`transition`]'s UPDATE and [`adjust_effective_to`]'s. The constraint ranges
/// over `effective_from`, `effective_to` and `state`, so an UPDATE re-offers the
/// row's interval to it exactly as an INSERT offers a new one, and the `SQLite`
/// mirror spells that out with a `BEFORE UPDATE` arm beside its `BEFORE INSERT`
/// one. Landed with `excl_pricing_price_window_no_overlap` on the INSERT alone, this left the loser
/// of a race on `adjust_effective_to` — the door an extension of an end goes
/// through, which is the mutation that makes a genuinely *new* claim on the key
/// — with the 500 the recognizer exists to prevent. Add a
/// fourth writer and it wants this too.
fn overlap_or(
    err: &toolkit_db::secure::ScopeError,
    key: &ScopeKey,
    from: OffsetDateTime,
    to: Option<OffsetDateTime>,
    subject: &str,
    action: &str,
) -> RepoError {
    let message = err.to_string();
    let is_overlap = message.contains("excl_pricing_price_window_no_overlap")
        || message.contains("interval overlaps an occupying window on this price row");
    if is_overlap {
        return RepoError::WindowOverlap {
            key: key.to_string(),
            requested: render_interval(from, to),
            // The row it collided with is **not** in the constraint's error, and
            // re-reading to find it would be a second query on the losing path.
            // The uncontended refusal below names the window; this one can only
            // say that one exists, so it says exactly that rather than
            // fabricating an id.
            conflicting: "a concurrently committed window on this key".to_owned(),
        };
    }
    contention_or_db(err, subject, action)
}

/// Refuse an interval that intersects an occupying window on the same canonical
/// scope key.
///
/// §6's "inside every mutation" **against one writer at a time**, and every clause
/// of the walk is load-bearing:
///
/// * the sibling set is **every price row of the plan whose key equals this one** —
///   not the subject row alone. A supersession leaves a `superseded` predecessor
///   and a `published` successor on one key, so a per-row check would let their
///   windows overlap;
/// * only [`OCCUPYING_STATES`] compete, for that constant's reason;
/// * `except` drops the window being adjusted, or every extension would collide
///   with the interval it replaces.
///
/// # The serialization point is in the schema
///
/// This function reads and its caller inserts, with no lock and no advisory key
/// in the path — so on its own, under `READ COMMITTED`, two concurrent mutations
/// on one key both read a key with no conflict and both commit one. **An
/// invariant a concurrent writer can step through is not an invariant**, and
/// calling this "the whole of §6 inside every mutation" was a claim measured
/// against a single-writer suite.
///
/// `excl_pricing_price_window_no_overlap` closes it declaratively, the way
/// `pricing_group_membership`'s exclusion constraint closes the identical shape one
/// table over: an `EXCLUDE USING gist` over
/// `tstzrange(effective_from, effective_to, '[)')` on Postgres, a `RAISE(ABORT)`
/// trigger pair on `SQLite`, both partial over [`OCCUPYING_STATES`]. This
/// function keeps its place unchanged — it is the **explanatory** path, naming
/// the colliding window and rendering the key, and the constraint is the
/// guarantee behind it. Losing that race now answers `WINDOW_OVERLAP` rather
/// than committing, which [`overlap_or`] is what makes true: an exclusion
/// violation is SQLSTATE `23P01` and `contention_or_db` recognizes `23505`
/// alone, so an unrecognized one would have rendered 500.
///
/// **One half is still owed, and it is the cross-row half this walk exists
/// for.** A constraint sees only its own table's columns, and
/// `pricing_price_window` carries `price_id`, not the key — so the schema
/// enforces non-overlap per **row**, and the `mates` case above (a `superseded`
/// predecessor and its `published` successor sharing a key) is still guarded by
/// this read alone. Closing it needs the rendered key denormalised onto the
/// window table; `pricing_price_window`'s migration doc carries why that is a data migration
/// owing its own proof rather than a clause of this one.
///
/// # A per-key serialization primitive **cannot be written in this crate**
///
/// The obvious prescription is a lock taken before the read — on Postgres
/// `pg_advisory_xact_lock` over a hash of the canonical key. It is unavailable here
/// for two reasons, measured rather than argued:
///
/// * **There is no way to issue `pg_advisory_xact_lock` from here.** This function
///   holds `&impl DBRunner`, and `DBRunner`'s only supertrait — `DBRunnerInternal`,
///   which carries the single method that yields a `SeaOrm` connection — is
///   **private at `toolkit_db::secure`'s re-export boundary**. Naming it from this
///   crate is `error[E0603]: trait DBRunnerInternal is private`. So no raw statement
///   of any kind can be executed through a runner, advisory lock or otherwise.
/// * **`toolkit-db`'s advisory locks are not transaction-scoped.** They hang off
///   `Db` rather than off a transaction, so a guard taken here would not be
///   released by the same commit that makes the insert durable.
///
///   They do contend across pods, so host scope is not one of the reasons: on
///   Postgres they are session-level `pg_try_advisory_lock` on a pinned pool
///   connection. Quoting `advisory_locks.rs` for the stronger claim is how this
///   analysis has gone wrong before — the quotation outlives the file.
/// * **`SecureSelect` exposes no row locking.** There is no `lock_exclusive`, no
///   `lock_shared` and no `FOR UPDATE` anywhere in `toolkit_db::secure`, so the
///   sibling read cannot be taken under a lock either.
///
/// A constraint **over the key** is out for its own reason, which the module doc
/// already gives and which G4 did not change: the canonical scope key is ten
/// columns of `pricing_price` and **none of them is on the window row**, so no
/// unique index reaches it; a Postgres exclusion constraint would additionally need
/// `btree_gist` *and* the key denormalised onto this table, and `SQLite` has no
/// exclusion constraints at all. That is the argument against closing the
/// **cross-row** half declaratively, and it is untouched by `pricing_price_window`,
/// which is scoped by `price_id` and needed no denormalised copy of anything.
///
/// # So the cross-row half is unserialized, and what it would take is named
///
/// Two routes exist and **both are outside a code group's remit**, which is why this
/// is a recorded hole rather than a deferred task:
///
/// 1. **A `toolkit-db` change** exposing a DB-native, transaction-scoped advisory
///    lock through the runner — `pg_advisory_xact_lock` on Postgres, a no-op on
///    `SQLite`, whose single-writer engine already supplies the ordering. That is a
///    platform library's API surface, not this gear's.
/// 2. **Denormalising the canonical scope key onto `pricing_price_window`** plus a
///    Postgres exclusion constraint over `(scope_key, interval)`. That duplicates the
///    key the design set puts on `pricing_price` (§3.7, ADR-0002) and makes the two
///    copies a thing that can disagree, so it is a data-model decision with a
///    register entry, not a repository edit.
///
/// The half that **was** closed is pinned by a test rather than left to a comment:
/// `tests/postgres_window.rs`'s
/// `a_second_overlapping_window_on_one_key_is_refused_by_the_store` runs two
/// writers at one price row, waits until the server reports one of them blocked on
/// the other, and asserts one survivor and [`RepoError::WindowOverlap`] for the
/// loser. It is the inversion of the case that used to pin the hole — same two
/// writers, opposite verdict — and it reaches only the same-row half, because that
/// is the half the constraint sees. Its sibling
/// `an_extension_that_loses_a_race_is_refused_by_name_and_not_as_a_storage_failure`
/// does the same for [`adjust_effective_to`]'s UPDATE, which is the door where this
/// walk passes on its merits and the *statement* is what collides: the colliding
/// neighbour is written by the other transaction and left uncommitted, so a
/// `READ COMMITTED` read cannot see it.
/// **The cross-mate case above has no such case
/// and no serialization point**; a test of it would need `mates` on two `price_id`s
/// and would today assert that both commit.
///
/// # Errors
/// [`RepoError::WindowOverlap`] naming the key, the requested interval and the
/// window it collided with. [`RepoError::Db`] on a scope or storage failure;
/// [`RepoError::CorruptRow`] on a stored axis outside its enumeration.
async fn refuse_overlap(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    key: &ScopeKey,
    from: OffsetDateTime,
    to: Option<OffsetDateTime>,
    except: Option<Uuid>,
) -> Result<(), RepoError> {
    let mates: Vec<Uuid> =
        price_repo::load_scope_keys_for_plan(runner, scope, tenant_id, key.plan_id())
            .await?
            .into_iter()
            .filter(|(_, mate)| mate == key)
            .map(|(price_id, _)| price_id)
            .collect();
    if mates.is_empty() {
        return Ok(());
    }

    for row in load_windows(
        runner,
        scope,
        tenant_id,
        mates.into_iter(),
        OCCUPYING_STATES,
    )
    .await?
    {
        if except == Some(row.window_id) {
            continue;
        }
        if intersects(from, to, row.effective_from, row.effective_to) {
            return Err(RepoError::WindowOverlap {
                key: key.to_string(),
                requested: render_interval(from, to),
                conflicting: format!(
                    "window {} at {}",
                    row.window_id,
                    render_interval(row.effective_from, row.effective_to)
                ),
            });
        }
    }
    Ok(())
}

/// Refuse a flip that is not one of §4's edges — the edge set **and** the edge's
/// condition, which are one rule stated in one sentence each in §4.
///
/// Two grounds, and they are asked in this order because the second presumes the
/// first: `scheduled → expired` is not an edge at all, so asking whether its
/// `effectiveTo` has arrived would answer about a transition that does not exist.
///
/// The conditions are **consulted, not restated**:
/// [`WindowInterval::has_started`] is `inst-ws-activate`'s `now ≥ effectiveFrom`
/// and [`WindowInterval::is_due_to_expire`] is `inst-ws-expire`'s
/// `now ≥ effectiveTo` — the latter answering `false` forever for an open-ended
/// window, which is that instruction's own "an open-ended window never expires"
/// and the whole reason no separate open-ended arm is written here.
///
/// The refusal is [`RepoError::WindowStateForbidden`] for both grounds and mints
/// no code, because §4 states the condition *as part of the edge*: a flip whose
/// condition is unmet is not one of §4's transitions, which is exactly what that
/// variant is for. The sentence distinguishes the two, and the open-ended case
/// gets its own, because "wait" and "this window will never expire" send a stalled
/// sweep to different places.
fn refuse_unsanctioned_edge(
    current: &WindowRecord,
    to: WindowState,
    at: OffsetDateTime,
) -> Result<(), RepoError> {
    if !current.state.may_move_to(to) {
        return Err(forbidden(
            current.window_id,
            current.state,
            &format!("the transition to {to}"),
        ));
    }
    let interval = WindowInterval::new(current.effective_from, current.effective_to, current.state);
    let unmet = match to {
        WindowState::Active if !interval.has_started(at) => Some(format!(
            "the transition to active at {}, before its effective_from {}",
            format_rfc3339(at),
            format_rfc3339(current.effective_from)
        )),
        WindowState::Expired if !interval.is_due_to_expire(at) => {
            Some(match current.effective_to {
                None => "the transition to expired of an open-ended window, which never expires"
                    .to_owned(),
                Some(end) => format!(
                    "the transition to expired at {}, before its effective_to {}",
                    format_rfc3339(at),
                    format_rfc3339(end)
                ),
            })
        }
        WindowState::Active
        | WindowState::Expired
        | WindowState::Scheduled
        | WindowState::Cancelled => None,
    };
    match unmet {
        Some(attempted) => Err(forbidden(current.window_id, current.state, &attempted)),
        None => Ok(()),
    }
}

/// Is finding the window **already** in `to` an answer, or a refusal?
///
/// An answer for the two states a **time-driven** boundary leads to — exactly
/// [`DueBoundary::target_state`]'s range, `active` and `expired` — and a refusal for
/// everything else. Two callers ask it, the pre-check and the zero-rows re-read, and
/// they ask it once because they are one rule: which arrivals at a state the row is
/// already in are a no-op.
///
/// **`active` and `expired` are idempotent because a clock decided them.** The
/// activation sweep flips `(tenant, plan)` batches under a lease it can lose, so a
/// re-driven run can re-read a window the previous one already moved, and the
/// boundary it re-observes is the same boundary; the row's flip timestamp is not
/// re-stamped either, so the instant a price took effect is written once. Which of
/// the two branches each caller reaches is worth stating exactly, because the sweep's
/// own idempotence is **not** either of them:
///
/// * `a_second_pass_over_the_same_instant_is_a_no_op` (`sqlite_window_activation`)
///   never reaches this predicate at all. Its second pass finds nothing due, because
///   [`list_due`] selects on `state = <the boundary's origin state>` and the window
///   has left that state — the sweep's idempotence is its **selection**, one level
///   above this function, and the test says so in its own words.
/// * the **pre-check** branch is `a_flip_to_the_state_the_window_already_holds_is_idempotent`
///   (`sqlite_window_repo`), which calls [`transition`] twice directly.
/// * the **zero-rows** branch is `two_sweeps_in_flight_flip_a_window_once_and_emit_one_event`
///   (`postgres_window_activation`): the loser reads `scheduled`, blocks on the row
///   lock, and after the winner commits its `UPDATE` matches nothing. It answers `Ok`
///   here — and the loser's pass still reports a failure, because the **outbox dedup
///   key** is what refuses the duplicate event. The lease is not what makes that
///   safe, and neither is this predicate.
///
/// **`cancelled` is not, and that is the whole of why this predicate exists.** No
/// instant makes a window due for cancellation — it is an operator's act
/// (`inst-ws-cancel`), which is why [`DueBoundary`] has two members and not three —
/// and [`crate::domain::window::check_cancellation`] refuses a second cancellation
/// "**and not as an idempotent no-op** … a second cancellation is a second publish
/// unit (D-99) … answering `Ok` would let that unit run over a window that never
/// changes". A store that answered `Ok` where the domain refuses would let
/// `DELETE …/price-windows/{windowId}` on an already-cancelled window answer 202
/// and run a publish unit, with its own `CatalogVersion` request, over a window
/// that cannot change. What it meets instead is
/// [`RepoError::WindowStateForbidden`] — the self-edge `may_move_to` already refuses
/// — while the wire code an operator sees, `WINDOW_NOT_CANCELLABLE`, stays the
/// domain check's on the DELETE path.
///
/// `scheduled` rides with `cancelled` for a stronger reason: nothing may move *to*
/// `scheduled` at all, so an arrival there is refused whether or not the row is
/// already in it.
const fn idempotent_arrival(to: WindowState) -> bool {
    matches!(to, WindowState::Active | WindowState::Expired)
}

/// §4's condition for the edge into `to`, as a predicate the `UPDATE` carries.
///
/// The same two instructions as [`refuse_unsanctioned_edge`], in SQL, so that a
/// second sweep over one instant matches zero rows instead of trusting a read.
/// Cancellation adds nothing: `inst-ws-cancel` is "cancelled before activation"
/// and the statement's `state = 'scheduled'` predicate already says that.
///
/// An empty [`Condition::all`] renders as no clause at all, which is what the two
/// unconditioned targets want.
///
/// **It has no independent test, and that is measured rather than assumed.**
/// Neutering this predicate reddens **zero** of 1127: the pre-check in
/// [`refuse_unsanctioned_edge`] answers first on every sequential path, so what is
/// left here is only what the pre-check cannot hold — the window between a read and
/// a write with another writer in it. Proving that needs a race and not two calls,
/// which is the choreography `tests/postgres_approval_race.rs` and
/// `pg_support::wait_until_a_backend_blocks` establish and which the activation
/// job's own Postgres concurrency suite is the place for. Until then this clause is
/// defence-in-depth whose absence a suite cannot see, and saying so is the point:
/// the guard-by-removal answer for it is `0`, not `1`.
fn edge_condition(to: WindowState, at: OffsetDateTime) -> Condition {
    match to {
        WindowState::Active => {
            Condition::all().add(price_window::Column::EffectiveFrom.lte(sql_instant(at)))
        }
        WindowState::Expired => Condition::all()
            .add(price_window::Column::EffectiveTo.is_not_null())
            .add(price_window::Column::EffectiveTo.lte(sql_instant(at))),
        WindowState::Scheduled | WindowState::Cancelled => Condition::all(),
    }
}

/// Bind `at` at the authored millisecond quantum for SQL instant comparisons.
///
/// Window bounds are D-144-quantized. `SeaORM`'s `time` TEXT form writes those as
/// `…00.000Z`. A later machine instant with extra fractional digits
/// (`…00.000001Z`) then compares *smaller* as `SQLite` TEXT, because `Z` > `0`.
/// Flooring the bind to milliseconds makes `bound <= at` the same comparison
/// the domain already makes when the bound sits on the quantum.
fn sql_instant(at: OffsetDateTime) -> OffsetDateTime {
    truncate_millis(at)
}

/// Refuse an interval whose end is not strictly after its start.
///
/// The application half of `chk_pricing_price_window_interval`, consulting
/// [`interval_is_non_empty`] rather than restating the comparison — the
/// [`check_authored_instant`] arrangement, one rule over: the domain holds the
/// predicate, this layer holds the [`RepoError`] and the domain's own checked form
/// holds the wire answer.
fn refuse_empty_interval(
    from: OffsetDateTime,
    to: Option<OffsetDateTime>,
) -> Result<(), RepoError> {
    if interval_is_non_empty(from, to) {
        return Ok(());
    }
    Err(RepoError::WindowIntervalEmpty {
        requested: render_interval(from, to),
    })
}

/// Refuse a move of an end that `inst-ws-immutable` froze.
///
/// **The three grounds are [`frozen_end`]'s, consulted rather than restated**, the
/// [`interval_is_non_empty`] arrangement one rule over: the domain holds the
/// statement, this layer holds the [`RepoError`], and the domain's own checked form
/// ([`crate::domain::window::check_effective_to_adjustment`]) holds the wire answer.
/// Spelling the grounds here instead would be a second copy of the domain's, in
/// the same order, with neither consulting the other: a tightening of one would
/// then make the store admit what the surface refuses, or this floor more
/// permissive than the pre-check above it, which [`adjust_effective_to`]'s own
/// doc forbids.
///
/// What is this function's own is the **sentence**: `effective_to` in the column's
/// spelling, naming the window, because an operator reading a store's refusal is
/// looking at a row. The domain says `effectiveTo` and names no window, because a
/// caller reading a 409 is looking at their own request.
fn refuse_frozen_end(
    current: &WindowRecord,
    to: Option<OffsetDateTime>,
    now: OffsetDateTime,
) -> Result<(), RepoError> {
    let interval = WindowInterval::new(current.effective_from, current.effective_to, current.state);
    let Some(ground) = frozen_end(&interval, to, now) else {
        return Ok(());
    };
    let frozen = match ground {
        FrozenEnd::TerminalState(state) => {
            format!("it is {state}, and an expired or cancelled window is immutable history")
        }
        FrozenEnd::StoredEndPassed(stored) => format!(
            "its effective_to {} had already passed at {}; only a future end may be moved",
            format_rfc3339(stored),
            format_rfc3339(now)
        ),
        FrozenEnd::TargetNotFuture(target) => format!(
            "an end may only be moved to a future instant, and {} is not after {}",
            format_rfc3339(target),
            format_rfc3339(now)
        ),
    };
    Err(RepoError::WindowHistorical {
        window_id: current.window_id.to_string(),
        frozen,
    })
}

/// Do two half-open intervals `[from, to)` share an instant?
///
/// `a.from < b.to_or_infinity && b.from < a.to_or_infinity`, with `None` reading
/// as infinity on either side.
///
/// **The strictness of both comparisons is the adjacency rule.** With
/// `a.to == b.from` the second comparison is `b.from < a.to` → false, so
/// `effectiveTo = next.effectiveFrom` does not intersect: two windows may share a
/// boundary instant, which the earlier one does not cover and the later one does.
/// §9 names the false positive a `<=` here would produce, and it would refuse
/// exactly the shape a supersession and a cutover both produce.
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

/// The windows of a set of price rows, in the order [`list_for_plan`] promises.
///
/// An **empty** `states` is every state rather than nothing, for
/// `approval_repo::list_page`'s reason: a caller that named no filter asked for
/// everything.
async fn load_windows(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    price_ids: impl Iterator<Item = Uuid>,
    states: &[WindowState],
) -> Result<Vec<price_window::Model>, RepoError> {
    let mut filter = Condition::all()
        .add(price_window::Column::TenantId.eq(tenant_id))
        .add(price_window::Column::PriceId.is_in(price_ids));
    if !states.is_empty() {
        let tokens: Vec<&str> = states.iter().copied().map(WindowState::as_str).collect();
        filter = filter.add(price_window::Column::State.is_in(tokens));
    }
    price_window::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(filter)
        .order_by(price_window::Column::EffectiveFrom, Order::Asc)
        .order_by(price_window::Column::WindowId, Order::Asc)
        .all(runner)
        .await
        .map_err(|e| RepoError::Db(format!("list pricing_price_window: {e}")))
}

/// [`find`], or the refusal that the window is not there.
///
/// The mutating paths all begin with it, so absence has one spelling: a
/// `NotFound` naming `window`, which is also what a foreign scope sees.
async fn require(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    window_id: Uuid,
) -> Result<WindowRecord, RepoError> {
    find(runner, scope, tenant_id, window_id)
        .await?
        .ok_or_else(|| RepoError::NotFound {
            subject: "window".to_owned(),
            id: window_id.to_string(),
        })
}

/// The state refusal, built in one place so its producers cannot spell it
/// differently.
fn forbidden(window_id: Uuid, state: WindowState, attempted: &str) -> RepoError {
    RepoError::WindowStateForbidden {
        window_id: window_id.to_string(),
        state: state.as_str().to_owned(),
        attempted: attempted.to_owned(),
    }
}

/// The flip instant for the column this transition writes, and the stored value
/// for the two it does not.
///
/// A window that reaches `expired` keeps the `activated_at` it got when it took
/// effect — `chk_pricing_price_window_activated_at` requires exactly that — so the
/// returned record must carry it forward rather than reconstruct it from the new
/// state.
fn pick(
    to: WindowState,
    column: WindowState,
    at: OffsetDateTime,
    stored: Option<OffsetDateTime>,
) -> Option<OffsetDateTime> {
    if to == column { Some(at) } else { stored }
}

/// One half-open interval as a refusal renders it.
///
/// `[from, to)` with the bracket asymmetry kept, because the asymmetry **is** the
/// rule an operator is being told about: a message that rendered both ends the
/// same way would leave them unable to see why the window that ends where theirs
/// begins was not the one that collided.
///
/// An absent end is spelled rather than dropped, for the same reason: "\[t, )"
/// leaves an open-ended window indistinguishable from one whose end the rendering
/// lost.
fn render_interval(from: OffsetDateTime, to: Option<OffsetDateTime>) -> String {
    match to {
        Some(to) => format!("[{}, {})", format_rfc3339(from), format_rfc3339(to)),
        None => format!("[{}, open-ended)", format_rfc3339(from)),
    }
}

/// Read a stored row into the domain's vocabulary, given its price row's key.
///
/// The state token is read through `WindowState::ALL` rather than parsed, for
/// `approval_repo::to_domain`'s reason: the enumeration lives in one place per
/// type, and a token the CHECK admits while this crate does not is an invariant
/// breach the boundary reports rather than a string a handler renders.
fn to_domain(row: price_window::Model, scope_key: ScopeKey) -> Result<WindowRecord, RepoError> {
    let state = super::plan_repo::read_token(
        "pricing_price_window.state",
        &row.state,
        WindowState::ALL,
        WindowState::as_str,
    )?;
    Ok(WindowRecord {
        window_id: row.window_id,
        tenant_id: row.tenant_id,
        price_id: row.price_id,
        scope_key,
        effective_from: row.effective_from,
        effective_to: row.effective_to,
        state,
        reason_code: row.reason_code,
        created_by: row.created_by,
        created_at: row.created_at,
        activated_at: row.activated_at,
        expired_at: row.expired_at,
        cancelled_at: row.cancelled_at,
        // The one place the column's lower bound is enforced. `pricing_price_window`
        // carries no `CHECK (mutation_seq >= 0)` — the portable form of one is a
        // whole-table rebuild on `SQLite` — so the boundary that converts the signed
        // column to the unsigned counter is where a negative is refused, in the same
        // breath as a state token this crate does not know.
        mutation_seq: u64::try_from(row.mutation_seq).map_err(|_| {
            RepoError::CorruptRow(format!(
                "pricing_price_window.mutation_seq of window {} is {}, and an act \
                 sequence counts acts",
                row.window_id, row.mutation_seq
            ))
        })?,
    })
}

#[cfg(test)]
#[path = "window_repo_tests.rs"]
mod window_repo_tests;
