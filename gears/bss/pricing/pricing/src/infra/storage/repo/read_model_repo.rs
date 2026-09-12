//! The writer of `pricing_read_model` — one per-subject delta of one
//! `CatalogVersion`, and the warm set a re-drive subtracts against.
//!
//! Runner-taking free functions rather than a provider-holding struct, and the
//! reason is the sharpest form of [`audit_repo`](super::audit_repo)'s and
//! [`outbox_repo`](super::outbox_repo)'s: **D-136 requires the frontier's
//! advance to happen in the transaction that sets the last outstanding
//! `warm_completed` marker of the frontier's next version in order.** Every
//! write on this path is therefore somebody else's transaction's, and a
//! repository that opened a connection of its own could not participate in it
//! — inside an open transaction `Db::conn()` is refused outright by the
//! toolkit's transaction-bypass guard.
//!
//! # The row is written **warm**, and that is not a shortcut
//!
//! §4.4's marker is per **row** and discriminates a subject that is resolvable
//! from one that is not. In this gear projection and warming are **one act**:
//! the payload is materialized from the truth rows in the same transaction that
//! writes it, so there is no interval in which a delta row exists and is not
//! complete. What the marker therefore discriminates here is *projected* from
//! *not yet projected* — and it stays per row rather than per version because a
//! version's subjects are projected independently (D-91), so a version's
//! completeness is the question "is every one of its ref rows' subjects warm",
//! which is what [`warm_subjects_at`] answers.
//!
//! `chk_pricing_read_model_warm_marker` already makes the marker and its
//! instant move together, so a half-warm row is not expressible on either
//! backend.
//!
//! # An INSERT, never an upsert
//!
//! A completed version never mutates. So a second projection of one
//! `(version, subject)` is either a duplicated sweep or a re-drive that failed
//! to skip an already-warm subject, and in both cases the primary key refusing
//! it is the guard — an upsert would silently rewrite a frozen version's
//! content, which is the one thing a pinned consumer is entitled to assume
//! cannot happen. The refusal arrives as [`RepoError::Db`] rather than a typed
//! variant because no caller can provoke it: the projector is a background
//! sweep with no client, so a refusal here has nobody to report to and no wire
//! code the design set names (D-146's line about the pin frontier, one table
//! over).
//!
//! # The resolution query is here now, and this paragraph is its answer
//!
//! **§4.4's rule** — "resolving `(pin, subject)` reads the subject's row with
//! the greatest `catalog_version <= V` whose `warm_completed` is set" — is
//! [`delta_at`], and `idx_pricing_read_model_resolve` finally has the query it
//! was created for.
//!
//! The caller is `GET /bss-pricing/v1/plans/{planId}/sellability`, whose four
//! answerable predicates are read off one plan subject's frozen delta. The other
//! two of the six read facts this gear cannot project at all (D-167 clause 3);
//! [`crate::domain::sellability`] answers those `NotEvaluable` with the slice
//! that owes each.
//!
//! It is a **read of one subject**, not of a version: the completeness question
//! stays [`warm_subjects_at`]'s, and a caller that wanted a version's whole
//! content would be asking for the payload of every subject in a batch, which no
//! surface asks for.

use bss_pricing_sdk::CatalogVersion;

use sea_orm::ActiveValue::Set;
use sea_orm::{ColumnTrait, Condition, EntityTrait, Order};
use serde_json::Value as JsonValue;
use toolkit_db::secure::{AccessScope, DBRunner, SecureEntityExt, SecureInsertExt};
use uuid::Uuid;

use crate::domain::lifecycle::LifecycleState;
use crate::domain::money::CurrencyCode;
use crate::domain::plan_shape::{CustomIntervalUnit, Frequency};
use crate::domain::projection::PROJECTED_WINDOW_STATES;
use crate::domain::read_model::{SubjectKind, SubjectRef};
use crate::domain::scope_key::{
    ChargeKind, Cohort, DimensionKey, Meter, PhaseId, PlanId, PriceEligibility, PriceOverlay,
    Region, ScopeKey,
};
use crate::domain::sellability::{PinnedFacts, SellabilityFacts};
use crate::domain::window::{KeyWindows, WindowInterval, WindowState};
use crate::infra::storage::RepoError;
use crate::infra::storage::entity::read_model;
use crate::infra::storage::repo::plan_repo::read_token;
use crate::infra::storage::repo::price_repo::{CHARGE_KINDS, PRICE_ELIGIBILITIES, PRICE_OVERLAYS};
use time::OffsetDateTime;

/// One projected delta, as its writer is handed it.
///
/// The subject arrives as a [`SubjectRef`] rather than as a kind and a string,
/// so a writer cannot produce a pair that disagrees — the same property
/// [`PendingVersionRow::for_subject`](super::catalog_version_ref_repo::PendingVersionRow::for_subject)
/// carries one table over, and the two tables are keyed alike on purpose (the
/// ref names the subject the projector will write).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewDelta {
    /// The owning tenant.
    pub tenant_id: Uuid,
    /// The committed version this delta belongs to.
    pub catalog_version: CatalogVersion,
    /// What the delta is about.
    pub subject: SubjectRef,
    /// The frozen payload, as [`crate::domain::projection`] renders it.
    pub payload: JsonValue,
    /// When the subject was projected — and, because the two are one act here,
    /// when it became warm.
    pub projected_at: OffsetDateTime,
}

/// Write one subject's delta inside `runner`'s transaction, warm.
///
/// # Errors
/// [`RepoError::Db`] on a scope or storage failure — which **includes** a
/// second delta on one `(tenant, version, subject)`, refused by the primary
/// key. That is the guard, not an inconvenience; see the module doc.
/// [`RepoError::CorruptRow`] when the version exceeds the signed range the
/// column stores.
pub async fn project_subject(
    runner: &impl DBRunner,
    scope: &AccessScope,
    delta: NewDelta,
) -> Result<(), RepoError> {
    let stored_version = stored_version(delta.catalog_version)?;
    let am = read_model::ActiveModel {
        tenant_id: Set(delta.tenant_id),
        catalog_version: Set(stored_version),
        subject_kind: Set(delta.subject.kind().as_str().to_owned()),
        subject_ref: Set(delta.subject.to_string()),
        // The marker and its instant in the same statement: projection and
        // warming are one act here, and the CHECK ties the pair anyway.
        warm_completed: Set(true),
        warm_completed_at: Set(Some(delta.projected_at)),
        payload: Set(delta.payload.clone()),
        projected_at: Set(delta.projected_at),
    };
    read_model::Entity::insert(am.clone())
        .secure()
        .scope_with_model(scope, &am)
        .map_err(|e| RepoError::Db(format!("pricing_read_model scope: {e}")))?
        .exec(runner)
        .await
        .map_err(|e| RepoError::Db(format!("project read model subject: {e}")))?;
    Ok(())
}

/// One subject's frozen delta as of `catalog_version`, with the version it was
/// actually found at.
///
/// The version is carried rather than assumed equal to the one asked for, which
/// is the whole of §4.4's resolution rule: a pin names a version of the
/// *tenant's* catalog, and a plan re-projects only when a publish unit touches
/// it, so the row that answers a pin at `V` is almost always stamped with some
/// earlier version.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredDelta {
    /// The version the answering row was projected under — `<=` the version
    /// asked for.
    pub catalog_version: CatalogVersion,
    /// When that projection happened.
    pub projected_at: OffsetDateTime,
    /// The frozen payload, exactly as [`crate::domain::projection`] rendered it.
    pub payload: JsonValue,
}

/// Resolve one subject's delta at a pinned version — §4.4's own rule.
///
/// The greatest `catalog_version <= catalog_version` whose `warm_completed` is
/// set, or `None` when the subject has no warm row at or below it.
///
/// # `None` is "not addressable at this pin", and the caller must fail closed
///
/// It covers two states a reader cannot tell apart from here and does not need
/// to: a plan that has never been projected into a committed version at all, and
/// one whose only projections are *later* than the pin. Both mean the same thing
/// to a consumer — **this pin does not carry this plan's content** — and the
/// sellability surface answers predicate (2) `Failed` for it rather than
/// synthesising a plan with no rows, which would read as a plan that publishes
/// nothing on every market.
///
/// # Ordering, not `max`
///
/// `ORDER BY catalog_version DESC LIMIT 1` under the `warm_completed` filter, so
/// the index does the work and no row-set is materialized to be folded in
/// process. The filter is not vacuous the way [`warm_subjects_at`]'s is today: a
/// row is written warm, but this query is the one whose answer would be *wrong*
/// rather than merely larger if a slice ever wrote a row that is not — it would
/// hand a consumer the payload of a subject that is not resolvable.
///
/// # One predicate here has a removal proof of **zero**, and it is said rather
/// than left to be found
///
/// `tenant_id` is already the compiled scope's own filter, so deleting the
/// `TenantId.eq` term reddens **nothing**:
/// `an_unprojected_subject_and_a_foreign_tenants_subject_read_alike` keeps
/// passing, because SQL-level BOLA is what refuses the foreign row. It is kept for
/// [`warm_subjects_at`]'s shape — the two reads of this table name the predicate
/// they depend on rather than relying on a scope compiled elsewhere — and it is
/// recorded as redundant so nobody reads it as the guard that holds the boundary.
/// The guard is the scope; this is a restatement of it.
///
/// # Errors
/// [`RepoError::Db`] on a scope or storage failure;
/// [`RepoError::CorruptRow`] when the version asked for, or the version stored
/// on the answering row, lies outside the range the column holds.
pub async fn delta_at(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    subject: &SubjectRef,
    catalog_version: CatalogVersion,
) -> Result<Option<StoredDelta>, RepoError> {
    let stored_version = stored_version(catalog_version)?;
    let row = read_model::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(read_model::Column::TenantId.eq(tenant_id))
                .add(read_model::Column::SubjectKind.eq(subject.kind().as_str()))
                .add(read_model::Column::SubjectRef.eq(subject.to_string()))
                .add(read_model::Column::CatalogVersion.lte(stored_version))
                .add(read_model::Column::WarmCompleted.eq(true)),
        )
        .order_by(read_model::Column::CatalogVersion, Order::Desc)
        .one(runner)
        .await
        .map_err(|e| RepoError::Db(format!("resolve read-model subject: {e}")))?;

    row.map(|row| {
        let version = u64::try_from(row.catalog_version).map_err(|e| {
            RepoError::CorruptRow(format!(
                "pricing_read_model.catalog_version holds {}: {e}",
                row.catalog_version
            ))
        })?;
        Ok(StoredDelta {
            catalog_version: CatalogVersion::new(version),
            projected_at: row.projected_at,
            payload: row.payload,
        })
    })
    .transpose()
}

/// The `(subject_kind, subject_ref)` pairs already warm at `catalog_version`.
///
/// Two callers, both in the projector: the re-drive subtracts this from the
/// version's ref rows to get exactly what failed last time, and the
/// completeness check compares the two sets to decide whether the version is
/// complete. Returned as the stored pair rather than as a [`SubjectRef`]
/// because that is what both callers compare against — the ref row's own
/// columns — and because three of the four kinds have no writer in this gear,
/// so a read that could only reconstruct the fourth would refuse rows a later
/// group legitimately wrote.
///
/// The `warm_completed` filter is redundant today and is kept deliberately: a
/// row is written warm, but the predicate the callers depend on is "warm at
/// this version", not "present at this version", and a filter dropped because
/// it is currently vacuous is a filter absent the day a slice writes a row that
/// is not.
///
/// # Errors
/// [`RepoError::Db`] on a scope or storage failure;
/// [`RepoError::CorruptRow`] when the version exceeds the signed range the
/// column stores, or when a stored `subject_kind` lies outside the enumeration
/// its `CHECK` constrains it to.
pub async fn warm_subjects_at(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    catalog_version: CatalogVersion,
) -> Result<Vec<(SubjectKind, String)>, RepoError> {
    let stored_version = stored_version(catalog_version)?;
    let rows = read_model::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(read_model::Column::TenantId.eq(tenant_id))
                .add(read_model::Column::CatalogVersion.eq(stored_version))
                .add(read_model::Column::WarmCompleted.eq(true)),
        )
        .all(runner)
        .await
        .map_err(|e| RepoError::Db(format!("read warm read-model subjects: {e}")))?;

    rows.into_iter()
        .map(|row| {
            let kind = read_token(
                "pricing_read_model.subject_kind",
                &row.subject_kind,
                SubjectKind::ALL,
                SubjectKind::as_str,
            )?;
            Ok((kind, row.subject_ref))
        })
        .collect()
}

/// Read the facts the six sellability predicates need out of one frozen delta.
///
/// # Why the reader is here and not beside [`SellabilityFacts`]
///
/// Reading a stored token means matching it against the roster the store's
/// `CorruptRow` path already depends on — [`PRICE_ELIGIBILITIES`],
/// [`CHARGE_KINDS`], [`PRICE_OVERLAYS`] and [`read_token`] — and those lists are
/// deliberately single-copy: their own comment refuses a second set "free to
/// disagree the day a variant lands in one of them". They live in `infra`, which
/// dylint DE0301 keeps out of the domain layer, so a domain-side parser would have
/// needed exactly that second copy. The phase plan sketched this reader into
/// `domain::sellability`; the placement moved for that reason and is reported.
///
/// # A payload this cannot read is [`RepoError::CorruptRow`], not a bad request
///
/// This gear wrote the payload. A member that is absent, of the wrong JSON type or
/// carrying a token outside its enumeration therefore means the reader and the
/// writer disagree about the vocabulary — the same class as a stored
/// `charge_kind` outside its `CHECK`, and the same answer `price_repo::to_scope_key`
/// gives it. No caller can reshape the request to fix it, so there is nothing to
/// report on the wire beyond a 500.
///
/// # What is deliberately not read
///
/// The payload's own `coverageEnd` object. Each key's [`KeyWindows`] is rebuilt
/// from its intervals and the end comes from
/// [`KeyWindows::coverage_end`](crate::domain::window::KeyWindows::coverage_end),
/// the crate's single implementation of that arithmetic — so the surface has one
/// source for the fact rather than a parser beside the function that derives it.
/// The two cannot disagree today: the renderer writes exactly what that function
/// returns. **If a later change moves what `coverage_end` means, the frozen token
/// becomes the authority and this reader owes the parse** — recorded because that
/// is the day the choice matters.
///
/// # `price_keys` takes **every** row of the payload, with no lifecycle filter
///
/// Deliberate, and stated here because the divergence from the sibling reader is
/// real and looks like an omission. `api::rest::preview` walks the same
/// `payload["prices"]` array and filters `lifecycleState == published` at `:397`
/// before selecting; this reader does not. The payload carries
/// [`PROJECTED_ROW_STATES`](crate::domain::projection::PROJECTED_ROW_STATES) —
/// `published` **and** `superseded` — so the difference is exactly the superseded
/// rows.
///
/// The two surfaces answer different questions and the filter belongs to only one
/// of them. `preview` **quotes a price**, so it must select a row that is live;
/// a superseded predecessor quoted as current is a wrong price. This reader feeds
/// a **gate**, and its two consumers both round the other way:
///
/// - `domain::coverage::longest_cycle_sold_on` asks whether the market sells a
///   recurring row at all, and answers `Some(zero)` when it does not — no forward
///   coverage required, so predicate (1) admits a key right up to its coverage
///   end. Dropping a superseded recurring row from a market whose only recurring
///   row is superseded flips `sells_recurring` false and collapses the D-80
///   margin from `Some(cycle)` to `Some(zero)`. **That is the fail-open
///   direction**, and it is the same collapse `active_window_with_horizon`
///   refuses one type over. Filtering here would be a strictly weaker gate.
/// - `domain::sellability::gate_input_keys` resolves the most-specific key per
///   sibling set and the plan-market verdict is a conjunction over what it
///   returns, so an extra key can only make the verdict harder to satisfy.
///
/// So the absence of the filter is fail-closed at both consumers and adding one
/// would be fail-open at the first. What the absence *does* cost is stated rather
/// than argued away: the day a path leaves a `superseded` row whose key no
/// `published` row backs — a row-level retirement, an unwind — this reader counts
/// that key as present where `preview` does not, and the two readings of one
/// payload diverge on a key rather than on a duplicate. Nothing produces such a
/// row today: a supersession and a repricing both write the successor on the
/// predecessor's canonical key, and a cutover's grandfathered copy is `published`.
/// Whoever builds the first such path owes this paragraph a second look, and the
/// answer will be a horizon on the projection (see `infra::read_model`'s D-121
/// note) rather than a filter here.
///
/// # Errors
/// [`RepoError::CorruptRow`] for any of the above.
pub fn sellability_facts(delta: &StoredDelta) -> Result<SellabilityFacts, RepoError> {
    let payload = &delta.payload;
    let mut price_keys = Vec::new();
    for row in array(payload, "prices")? {
        price_keys.push(read_scope_key(member(row, "scopeKey")?)?);
    }
    let mut windows = Vec::new();
    for group in array(payload, "windows")? {
        windows.push(KeyWindows {
            scope_key: read_scope_key(member(group, "scopeKey")?)?,
            intervals: read_intervals(group)?,
        });
    }
    Ok(SellabilityFacts::Pinned(PinnedFacts {
        plan_id: PlanId::new(uuid(payload, "planId")?),
        catalog_version: delta.catalog_version,
        lifecycle_state: read_token(
            "pricing_read_model.payload.lifecycleState",
            string(payload, "lifecycleState")?,
            LifecycleState::ALL,
            LifecycleState::as_str,
        )?,
        available_from: optional_instant(payload, "availableFrom")?,
        available_to: optional_instant(payload, "availableTo")?,
        frequency: read_frequency(payload)?,
        price_keys,
        windows,
    }))
}

/// One member of an object, present or a refusal naming it.
fn member<'a>(value: &'a JsonValue, key: &str) -> Result<&'a JsonValue, RepoError> {
    value.get(key).ok_or_else(|| malformed(key, "is absent"))
}

/// One array member of an object.
fn array<'a>(value: &'a JsonValue, key: &str) -> Result<&'a Vec<JsonValue>, RepoError> {
    member(value, key)?
        .as_array()
        .ok_or_else(|| malformed(key, "is not an array"))
}

/// One string member of an object.
fn string<'a>(value: &'a JsonValue, key: &str) -> Result<&'a str, RepoError> {
    member(value, key)?
        .as_str()
        .ok_or_else(|| malformed(key, "is not a string"))
}

/// One `Uuid` member of an object.
fn uuid(value: &JsonValue, key: &str) -> Result<Uuid, RepoError> {
    string(value, key)?
        .parse()
        .map_err(|e| malformed(key, &format!("is not a uuid: {e}")))
}

/// One required instant.
///
/// `serde_json` renders a `OffsetDateTime` as RFC 3339, so this is the inverse of
/// what [`crate::domain::projection`] wrote and not a second format.
fn instant(value: &JsonValue, key: &str) -> Result<OffsetDateTime, RepoError> {
    crate::domain::instant::parse_rfc3339(string(value, key)?)
        .map_err(|e| malformed(key, &format!("is not an RFC 3339 instant: {e}")))
}

/// One nullable instant.
///
/// An absent member and a `null` one read alike: `serde_json` renders `None` as
/// `null`, so a reader that distinguished them would be reading the serializer
/// rather than the fact.
fn optional_instant(value: &JsonValue, key: &str) -> Result<Option<OffsetDateTime>, RepoError> {
    match value.get(key) {
        None | Some(JsonValue::Null) => Ok(None),
        Some(_) => instant(value, key).map(Some),
    }
}

/// The plan's frequency, token **and** interval.
///
/// A `custom_every_n` member of [`Frequency::ALL`] carries
/// `Frequency::CUSTOM_INTERVAL_PLACEHOLDER` rather than an interval, so the
/// variant found by token is rebuilt from the payload's own `n` and `unit` — that
/// constant's own instruction, and without it every custom frequency would
/// contribute a one-day margin whatever the plan authored. It is the same
/// reconstruction `plan_repo` performs off `custom_interval_n` /
/// `custom_interval_unit`, one carrier over.
fn read_frequency(payload: &JsonValue) -> Result<Option<Frequency>, RepoError> {
    let Some(value) = payload.get("frequency").filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    let found = read_token(
        "pricing_read_model.payload.frequency.token",
        string(value, "token")?,
        Frequency::ALL,
        Frequency::as_str,
    )?;
    if !matches!(found, Frequency::CustomEveryN { .. }) {
        return Ok(Some(found));
    }
    let n = member(value, "n")?
        .as_u64()
        .and_then(|n| u32::try_from(n).ok())
        .ok_or_else(|| malformed("frequency.n", "is not a u32"))?;
    Ok(Some(Frequency::CustomEveryN {
        n,
        unit: read_token(
            "pricing_read_model.payload.frequency.unit",
            string(value, "unit")?,
            CustomIntervalUnit::ALL,
            CustomIntervalUnit::as_str,
        )?,
    }))
}

/// One canonical scope key, axis by axis.
///
/// Through [`ScopeKey::new`], the crate's only constructor, so the cohort /
/// eligibility biconditional and the D-144 quantum are re-established on this
/// rehydration exactly as `price_repo::to_scope_key` re-establishes them on its
/// own. `priceOverlay` is read and **checked** rather than passed: the constructor
/// answers `base` for everything, so a payload naming another plane must be
/// refused rather than silently flattened — `to_scope_key`'s own comment, one
/// carrier over.
fn read_scope_key(value: &JsonValue) -> Result<ScopeKey, RepoError> {
    read_token(
        "pricing_read_model.payload.scopeKey.priceOverlay",
        string(value, "priceOverlay")?,
        PRICE_OVERLAYS,
        PriceOverlay::as_str,
    )?;
    let cohort = match value.get("cohort").filter(|v| !v.is_null()) {
        None => Cohort::None,
        Some(_) => Cohort::Generation(instant(value, "cohort")?),
    };
    // Axes 9 and 10 (D-196), read back the way `cohort` is: an absent member and
    // a `null` both mean "no line". The payload is JSON and needs no fixed
    // arity — only the key's *rendering* does, and for a different reason: it is
    // embedded in strings a consumer cannot parse by field.
    let meter = match value.get("meter").filter(|v| !v.is_null()) {
        None => None,
        Some(_) => Some(
            Meter::new(string(value, "meter")?)
                .map_err(|e| malformed("scopeKey.meter", &e.to_string()))?,
        ),
    };
    let dimension = match value.get("dimensionKey").filter(|v| !v.is_null()) {
        None => DimensionKey::none(),
        Some(_) => DimensionKey::new(string(value, "dimensionKey")?),
    };
    ScopeKey::new(
        PlanId::new(uuid(value, "planId")?),
        CurrencyCode::new(string(value, "currency")?)
            .map_err(|e| malformed("scopeKey.currency", &e.to_string()))?,
        Region::new(string(value, "region")?)
            .map_err(|e| malformed("scopeKey.region", &e.to_string()))?,
        PhaseId::new(uuid(value, "phase")?),
        read_token(
            "pricing_read_model.payload.scopeKey.priceEligibility",
            string(value, "priceEligibility")?,
            PRICE_ELIGIBILITIES,
            PriceEligibility::as_str,
        )?,
        read_token(
            "pricing_read_model.payload.scopeKey.chargeKind",
            string(value, "chargeKind")?,
            CHARGE_KINDS,
            ChargeKind::as_str,
        )?,
        cohort,
    )
    .and_then(|key| key.with_usage_line(meter, dimension))
    .map_err(|e| malformed("scopeKey", &e.to_string()))
}

/// One key group's intervals.
///
/// # The roster is what the projector **writes**, not what the column admits
///
/// [`PROJECTED_WINDOW_STATES`] and not [`WindowState::ALL`]. The renderer filters
/// `cancelled` out — it is a schedule that never happened, not history a consumer
/// resolves against — so a frozen payload carrying one is a payload **this gear
/// could not have written**, which is the [`RepoError::CorruptRow`] class exactly
/// as an alien `charge_kind` is.
///
/// Validated against the wider roster it was *accepted*, and invisibly: both
/// [`KeyWindows::covers_at`](crate::domain::window::KeyWindows::covers_at) and
/// [`KeyWindows::coverage_end`](crate::domain::window::KeyWindows::coverage_end)
/// drop `cancelled` again, so the answer stayed fail-closed and nothing ever
/// reported the disagreement — while `covers_at`'s own doc leans on
/// `PROJECTED_WINDOW_STATES` having kept it out one layer up. A reader that admits
/// more than its writer emits is a reader that cannot say the two agree.
fn read_intervals(group: &JsonValue) -> Result<Vec<WindowInterval>, RepoError> {
    array(group, "intervals")?
        .iter()
        .map(|interval| {
            Ok(WindowInterval::new(
                instant(interval, "effectiveFrom")?,
                optional_instant(interval, "effectiveTo")?,
                read_token(
                    "pricing_read_model.payload.windows[].intervals[].state",
                    string(interval, "state")?,
                    PROJECTED_WINDOW_STATES,
                    WindowState::as_str,
                )?,
            ))
        })
        .collect()
}

/// A frozen payload this gear cannot read back.
fn malformed(key: &str, complaint: &str) -> RepoError {
    RepoError::CorruptRow(format!("pricing_read_model.payload: `{key}` {complaint}"))
}

/// The version as its `bigint` column holds it.
///
/// A value outside the signed range is [`RepoError::CorruptRow`] rather than a
/// caller mistake: `CatalogVersion` is minted by the registry, so a version
/// this large means the sequence itself has left the range every column in this
/// gear stores it in, and no caller can reshape the request.
fn stored_version(version: CatalogVersion) -> Result<i64, RepoError> {
    i64::try_from(version.get()).map_err(|e| {
        RepoError::CorruptRow(format!(
            "catalog version {} exceeds the storable range: {e}",
            version.get()
        ))
    })
}

#[cfg(test)]
#[path = "read_model_repo_tests.rs"]
mod read_model_repo_tests;
