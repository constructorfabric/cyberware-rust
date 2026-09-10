//! Repository over `pricing_approval_threshold` **and its tombstone table** — the
//! versioned, per-currency approval-threshold policy (`design/05-governance.md` §6,
//! D-10, D-185).
//!
//! Four operations and no update, which is the store's whole shape. A proposal is
//! [`open_version`]; a **retirement** is [`open_tombstone`]; a reader of one version
//! is [`read_version`]; and [`latest_version`] is what a proposer asks before minting
//! the next number. There is no `apply`, no `activate` and no delete, because a
//! version's content is what an approval's `content_hash` covers and a mutated
//! version would leave a signature over content nobody can reconstruct.
//!
//! # Two tables, **one** version sequence, and this module is where they meet
//!
//! `pricing_approval_threshold` holds one row per currency of a version;
//! `pricing_approval_threshold_tombstone` holds one row per version that has **no**
//! currencies (D-185 — the authored way back to §6's *"unset ⇒ two-person rule
//! always"*, which the entry table alone cannot express, a zero-row version being
//! indistinguishable from a version nobody proposed). They are two tables and one
//! sequence: [`latest_version`] takes the maximum across both and [`versions_desc`]
//! merges both, so a tombstone consumes a number, is walked by the effective-policy
//! resolution, and is superseded by the next proposal exactly as an entry version is.
//!
//! Every reader above this module sees one sequence and never the seam. That is the
//! reason the join lives here rather than in `infra::threshold`: a caller that had to
//! remember to ask both tables is a caller that will one day ask one.
//!
//! # Runner-taking, not provider-holding, and the reason is D-10's
//!
//! Every entry point here takes a [`DBRunner`]. The proposal writes its version
//! rows **and** opens the approval unit that reviews them in one transaction —
//! a version nobody is reviewing is a proposal with no reviewer, and a unit
//! pinning content that failed to commit is a signature over nothing — and a
//! provider-holding repository could not join that transaction at all, `Db::conn()`
//! being refused inside an open one. [`read_model_repo`](super::read_model_repo)
//! set the precedent for the same reason.
//!
//! # Which version is in effect is not this module's answer
//!
//! It is `pricing_approval`'s: the greatest version whose unit an independent
//! principal approved. [`crate::infra::threshold::effective_policy`] composes the
//! two stores; nothing here reads the approval plane, so this module cannot
//! disagree with it.

use sea_orm::{ColumnTrait, Condition, EntityTrait, Order};
use toolkit_db::secure::{AccessScope, DBRunner, SecureEntityExt, SecureInsertExt};
use uuid::Uuid;

use crate::domain::audit::AuditStamp;
use crate::domain::materiality::ThresholdBasis;
use crate::infra::storage::RepoError;
use crate::infra::storage::entity::{approval_threshold, approval_threshold_tombstone};
use crate::infra::storage::repo::check_authored_instant;
use time::OffsetDateTime;

/// One currency's entry in one version, as the store holds it.
///
/// Exactly one of the two bases is `Some` — `chk_pricing_approval_threshold_basis`
/// is what makes that a fact rather than a convention, and
/// [`crate::domain::materiality::ThresholdEntry`] is the domain type that carries
/// the same guarantee in the type system.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThresholdEntryRow {
    /// The ISO 4217 code, as stored.
    pub currency: String,
    /// The absolute threshold in the currency's minor units.
    pub absolute_minor: Option<i64>,
    /// The relative threshold in basis points (`10_000` = 100%).
    pub percent_bp: Option<i32>,
}

impl ThresholdEntryRow {
    /// The row's two nullable columns read as the domain's one-of-two choice.
    ///
    /// `None` when the pair is both-set or neither-set —
    /// `chk_pricing_approval_threshold_basis` makes that unreachable through this
    /// chain, so a caller that meets it has been written around and should say so
    /// rather than pick a basis. The conversion lives here, beside the columns,
    /// because the alternative is every reader deciding for itself which column
    /// wins when both are set.
    #[must_use]
    pub fn basis(&self) -> Option<ThresholdBasis> {
        match (self.absolute_minor, self.percent_bp) {
            (Some(minor), None) => Some(ThresholdBasis::Absolute { minor }),
            (None, Some(bp)) => u32::try_from(bp)
                .ok()
                .map(|bp| ThresholdBasis::Percent { bp }),
            _ => None,
        }
    }
}

/// One stored version: its authored instant and its entries, ordered by currency.
///
/// The two are returned together because a version is what an approval pin covers
/// and `effective_from` is inside that pin. [`read_version`] used to hand back the
/// entries alone, which made the pinned subject unreconstructible from the store
/// that holds it — the reader had the currencies and the thresholds and no way to
/// say when they start applying.
///
/// **An empty `entries` is the tombstone (D-185), and it is not a hole.**
/// [`read_version`] answers `None` for a version nobody proposed and
/// `Some(StoredVersion { entries: [] })` only for a version whose row sits in
/// `pricing_approval_threshold_tombstone` — so the emptiness is a fact read off a
/// stored row, not the absence of one. A `tombstone: bool` beside the vector would
/// be a second answer to what the vector already says, and the domain makes the same
/// choice for the same reason (`ThresholdVersion::tombstone`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredVersion {
    /// When the thresholds start applying — or stop, on a tombstone — once the
    /// version is approved.
    ///
    /// One value for the whole version: every row of a version is written by one
    /// [`open_version`] call with one instant, so a per-entry reading would be a
    /// column with N spellings of one fact. A tombstone has one row and therefore
    /// nothing to disagree with.
    pub effective_from: OffsetDateTime,
    /// The entries, ordered by currency — the order the pin is taken over. Empty
    /// exactly on a tombstone.
    pub entries: Vec<ThresholdEntryRow>,
}

/// The greatest version number this tenant has ever proposed, **across both
/// tables**.
///
/// `None` is a tenant that has never proposed one — which is **not** version 0
/// and is the state `inst-mat-failsafe` is named for. The next proposal is
/// therefore `0` on `None` and `n + 1` otherwise, minted by the caller inside the
/// transaction that writes it so two concurrent proposals cannot mint one number
/// twice (the primary key refuses the loser either way).
///
/// **The maximum is taken over the entry table and the tombstone table**, and that
/// is load-bearing rather than symmetric: a tombstone that `latest_version` could
/// not see would let the *next* proposal mint the number the tombstone already
/// holds, which leaves one version number carrying both an authored retirement and
/// an authored entry set — a version neither approver signed. See
/// [`read_version`] for what happens to such a version, and
/// `pricing_approval_threshold_tombstone`'s migration doc for why no schema constraint can refuse it.
///
/// # Errors
/// [`RepoError::Db`] on a scope or storage failure.
pub async fn latest_version(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
) -> Result<Option<i64>, RepoError> {
    let entries = approval_threshold::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(Condition::all().add(approval_threshold::Column::TenantId.eq(tenant_id)))
        .order_by(approval_threshold::Column::Version, Order::Desc)
        .limit(1)
        .one(runner)
        .await
        .map_err(|e| RepoError::Db(format!("read latest threshold version: {e}")))?;
    let tombstones = approval_threshold_tombstone::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(Condition::all().add(approval_threshold_tombstone::Column::TenantId.eq(tenant_id)))
        .order_by(approval_threshold_tombstone::Column::Version, Order::Desc)
        .limit(1)
        .one(runner)
        .await
        .map_err(|e| RepoError::Db(format!("read latest threshold tombstone version: {e}")))?;
    Ok(entries
        .map(|row| row.version)
        .into_iter()
        .chain(tombstones.map(|row| row.version))
        .max())
}

/// One version's entries, ordered by currency.
///
/// The order is the store's and not the proposer's: a version is a **set** of
/// per-currency entries, so rendering it in insertion order would make one policy
/// hash two ways depending on which currency the operator typed first, and the
/// approval pin is taken over exactly this rendering.
///
/// An unknown version reads as `None` rather than an error — the caller that cares
/// (the pinned-content render) reports "no longer derivable", which is the same
/// answer a plan whose draft has published gives.
///
/// # One version, one instant — **derived rather than assumed**
///
/// `effective_from` is a column on every entry row, so taking it off `rows.first()`
/// rests on an invariant nothing asserts: *all rows of one version share one
/// instant*. Nothing in the schema says so — the column is per row and the primary
/// key is `(tenant_id, version, currency)` — so `first()` is a silent choice among
/// rows that may disagree, made in the `ORDER BY currency` order, which means the
/// answer depends on which currency sorts first.
///
/// It is now **derived**: the maximum over the version's rows, with a disagreement
/// reported as [`RepoError::CorruptRow`] rather than resolved. The invariant is real —
/// `open_version` writes one authored instant across the whole entry set in one
/// statement, and the append-only trigger refuses every `UPDATE` — so a disagreement
/// means the table was written around, and the pin a reviewer signed was taken over one
/// of the two values. `max` rather than `min` never matters for a well-formed version
/// and is the fail-closed direction for a malformed one: the later instant is the one
/// that has not yet taken effect.
///
/// # The tombstone arm, and the one state it refuses (D-185)
///
/// A version with no entry rows is not automatically absent any more: it is the
/// tombstone if `pricing_approval_threshold_tombstone` carries a row for it, and
/// absent otherwise. The two are read here, together, because they are two halves of
/// one question and a caller that asked them separately could see a version appear
/// between the reads.
///
/// A version that carries **both** a tombstone row and entry rows is
/// [`RepoError::CorruptRow`], on the same warrant as the two-`effective_from` case
/// above and not as a defensive `else`. It is reachable — two proposals that read one
/// `latest_version` and mint one number, one retiring and one configuring, collide on
/// nothing, the two tables having two primary keys — and no schema constraint can
/// refuse it without making each table's append path query the other. What makes
/// refusing the *right* answer rather than picking one is that such a version is one
/// **no approver signed**: the retirement's reviewer signed the empty digest, the
/// entry set's reviewer signed the entry digest, and the stored version is neither.
/// `infra::approval::read_threshold_version` skips a corrupt version, so the tenant
/// stays on the version they already had and neither proposal takes effect — which is
/// the only outcome that is not somebody's signature applied to content they were not
/// shown.
///
/// # Errors
/// [`RepoError::Db`] on a scope or storage failure; [`RepoError::CorruptRow`] when one
/// version's rows carry two different `effective_from` values, or when one version is
/// both a tombstone and an entry set.
pub async fn read_version(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    version: i64,
) -> Result<Option<StoredVersion>, RepoError> {
    let rows = approval_threshold::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(approval_threshold::Column::TenantId.eq(tenant_id))
                .add(approval_threshold::Column::Version.eq(version)),
        )
        .order_by(approval_threshold::Column::Currency, Order::Asc)
        .all(runner)
        .await
        .map_err(|e| RepoError::Db(format!("read threshold version {version}: {e}")))?;
    let retired = read_tombstone(runner, scope, tenant_id, version).await?;
    if let Some(retired_at) = retired {
        if !rows.is_empty() {
            return Err(RepoError::CorruptRow(format!(
                "pricing_approval_threshold: version {version} is both a tombstone starting \
                 {retired_at} and a set of {} entries - so which of the two an approver signed is \
                 not determined, and the version takes effect as neither",
                rows.len()
            )));
        }
        return Ok(Some(StoredVersion {
            effective_from: retired_at,
            entries: Vec::new(),
        }));
    }
    let Some(effective_from) = rows.iter().map(|row| row.effective_from).max() else {
        return Ok(None);
    };
    if let Some(disagreeing) = rows.iter().find(|row| row.effective_from != effective_from) {
        return Err(RepoError::CorruptRow(format!(
            "pricing_approval_threshold: version {version} carries two effective_from values - \
             {effective_from} on one entry and {} on {} - so which instant an approver signed is \
             not determined",
            disagreeing.effective_from, disagreeing.currency
        )));
    }
    Ok(Some(StoredVersion {
        effective_from,
        entries: rows
            .into_iter()
            .map(|row| ThresholdEntryRow {
                currency: row.currency,
                absolute_minor: row.absolute_minor,
                percent_bp: row.percent_bp,
            })
            .collect(),
    }))
}

/// Write one proposed version's entries, inside `runner`'s transaction.
///
/// The whole version in one call, because a version is the unit an approval pins:
/// two calls could commit half a policy, and a reviewer would then be shown a
/// proposal the proposer never made.
///
/// `effective_from` is an authored instant and is quantized to the millisecond
/// (D-144) by the same boundary check every other authored instant in this gear
/// passes — an unquantized one is refused here rather than truncated, because a
/// truncating producer and a non-truncating consumer agree until the day they do
/// not.
///
/// An empty `entries` writes nothing and is not an error at this layer: the domain
/// decides whether an empty entry set is a legal proposal, and
/// [`crate::domain::materiality::ThresholdPolicy::of_entries`] answers `None` for
/// one, so a policy of no entries is the absence `inst-mat-failsafe` is named for
/// rather than a storage fault.
///
/// # Errors
/// [`RepoError::TimestampPrecisionExceeded`] on an unquantized `effective_from`;
/// [`RepoError::ConcurrentMutation`] on a duplicate `(tenant, version, currency)`
/// — a **lost mint**, not a storage fault, and see the note below;
/// [`RepoError::Db`] on a scope failure or any CHECK the row violates.
///
/// # Why a duplicate version is a 409 and was a 500
///
/// The caller mints the version number off [`latest_version`] and then inserts, so
/// two proposals racing on one tenant can both read the same number and the
/// physical key decides which lands. That is the ordinary optimistic shape and its
/// loser wants *retry*, which is what `CONCURRENT_MUTATION` says. Mapping **every**
/// failure here to [`RepoError::Db`] — a 500 — would leave the two policy-plane
/// writes disagreeing about whether a lost mint is a conflict or a crash, since
/// `approval_repo::open` one file over classifies the identical shape through
/// `policy_guard_or_contention`.
pub async fn open_version(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    version: i64,
    effective_from: OffsetDateTime,
    entries: &[ThresholdEntryRow],
    stamp: AuditStamp,
) -> Result<(), RepoError> {
    check_authored_instant("effectiveFrom", Some(effective_from))?;
    for entry in entries {
        let model = approval_threshold::ActiveModel {
            tenant_id: sea_orm::ActiveValue::Set(tenant_id),
            version: sea_orm::ActiveValue::Set(version),
            currency: sea_orm::ActiveValue::Set(entry.currency.clone()),
            absolute_minor: sea_orm::ActiveValue::Set(entry.absolute_minor),
            percent_bp: sea_orm::ActiveValue::Set(entry.percent_bp),
            effective_from: sea_orm::ActiveValue::Set(effective_from),
            created_by: sea_orm::ActiveValue::Set(stamp.actor_principal_id),
            created_at: sea_orm::ActiveValue::Set(stamp.recorded_at),
        };
        approval_threshold::Entity::insert(model.clone())
            .secure()
            .scope_with_model(scope, &model)
            .map_err(|e| RepoError::Db(format!("threshold entry scope: {e}")))?
            .exec(runner)
            .await
            .map_err(|e| {
                crate::infra::storage::contention_or_db(
                    &e,
                    &format!("threshold policy of tenant {tenant_id}"),
                    &format!(
                        "write threshold entry {}/{version}/{}",
                        tenant_id, entry.currency
                    ),
                )
            })?;
    }
    Ok(())
}

/// Write one proposed **tombstone** version, inside `runner`'s transaction (D-185).
///
/// The retirement half of [`open_version`], and one row rather than N because a
/// tombstone's whole content is that it has no entries. It is the same transaction
/// discipline for the same reason: the row and the approval unit that pins it commit
/// together or a tenant is left with a retirement nobody is reviewing, or a signature
/// over a retirement that never landed.
///
/// `effective_from` passes the same D-144 quantization boundary check every other
/// authored instant in this gear passes — and it matters more here rather than less,
/// the instant being *when the two-person rule comes back*.
///
/// **This does not refuse a version number the entry table already holds**, and it
/// cannot: the two tables have two primary keys and neither sees the other. The
/// caller mints the number off [`latest_version`], which reads both;
/// [`read_version`] is what fails closed if a race gets past that.
///
/// # Errors
/// [`RepoError::TimestampPrecisionExceeded`] on an unquantized `effective_from`;
/// [`RepoError::ConcurrentMutation`] on a duplicate `(tenant, version)`, for
/// [`open_version`]'s stated reason — a lost mint is a retriable conflict;
/// [`RepoError::Db`] on a scope failure or any CHECK the row violates.
pub async fn open_tombstone(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    version: i64,
    effective_from: OffsetDateTime,
    stamp: AuditStamp,
) -> Result<(), RepoError> {
    check_authored_instant("effectiveFrom", Some(effective_from))?;
    let model = approval_threshold_tombstone::ActiveModel {
        tenant_id: sea_orm::ActiveValue::Set(tenant_id),
        version: sea_orm::ActiveValue::Set(version),
        effective_from: sea_orm::ActiveValue::Set(effective_from),
        created_by: sea_orm::ActiveValue::Set(stamp.actor_principal_id),
        created_at: sea_orm::ActiveValue::Set(stamp.recorded_at),
    };
    approval_threshold_tombstone::Entity::insert(model.clone())
        .secure()
        .scope_with_model(scope, &model)
        .map_err(|e| RepoError::Db(format!("threshold tombstone scope: {e}")))?
        .exec(runner)
        .await
        .map_err(|e| {
            crate::infra::storage::contention_or_db(
                &e,
                &format!("threshold policy of tenant {tenant_id}"),
                &format!("write threshold tombstone {tenant_id}/{version}"),
            )
        })?;
    Ok(())
}

/// The instant a version retires the tenant's thresholds, if that version is a
/// tombstone.
///
/// Private, because "is this version a tombstone" is not a question any layer above
/// this module should have to ask separately: [`read_version`] folds the answer into
/// the one `StoredVersion` every reader already takes.
///
/// # Errors
/// [`RepoError::Db`] on a scope or storage failure.
async fn read_tombstone(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    version: i64,
) -> Result<Option<OffsetDateTime>, RepoError> {
    let row = approval_threshold_tombstone::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(approval_threshold_tombstone::Column::TenantId.eq(tenant_id))
                .add(approval_threshold_tombstone::Column::Version.eq(version)),
        )
        .one(runner)
        .await
        .map_err(|e| RepoError::Db(format!("read threshold tombstone {version}: {e}")))?;
    Ok(row.map(|row| row.effective_from))
}

/// Every version number this tenant has proposed, greatest first — **entry versions
/// and tombstones in one sequence**.
///
/// The effective-policy resolution needs it: "the greatest version whose unit was
/// approved" is a walk down this list asking the approval store about each, and
/// asking about a version that does not exist would report a policy the tenant
/// never proposed.
///
/// The tombstones are in it for the converse reason, and it is the single line that
/// makes D-185 executable rather than stored: a retirement the walk never visits is a
/// retirement that can never be in force, however many principals approved it. A list
/// built from the entry table alone would leave an approved tombstone invisible and
/// the tenant on the thresholds they had asked to be rid of — which is the
/// fail-**open** direction, since those thresholds are what let a change publish on
/// one principal.
///
/// # Errors
/// [`RepoError::Db`] on a scope or storage failure.
pub async fn versions_desc(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
) -> Result<Vec<i64>, RepoError> {
    let rows = approval_threshold::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(Condition::all().add(approval_threshold::Column::TenantId.eq(tenant_id)))
        .order_by(approval_threshold::Column::Version, Order::Desc)
        .all(runner)
        .await
        .map_err(|e| RepoError::Db(format!("list threshold versions: {e}")))?;
    let retired = approval_threshold_tombstone::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(Condition::all().add(approval_threshold_tombstone::Column::TenantId.eq(tenant_id)))
        .order_by(approval_threshold_tombstone::Column::Version, Order::Desc)
        .all(runner)
        .await
        .map_err(|e| RepoError::Db(format!("list threshold tombstone versions: {e}")))?;
    // Deduplicated here rather than by `SELECT DISTINCT`: `SecureSelect` carries
    // no projection, and a version holds one row per configured currency, so the
    // set is a handful of rows per proposal rather than a scan of anything.
    //
    // Sorted after the merge rather than merged in order: the two `ORDER BY`s are
    // each descending, but a tombstone at version 4 and an entry version at 3 arrive
    // in two lists, and the walk's whole contract is *greatest first* over the one
    // sequence. A merge that trusted the two lists' relative order would silently
    // visit an older version before a newer one, which is the union reading
    // `infra::threshold`'s module doc rules out.
    let mut versions: Vec<i64> = rows
        .into_iter()
        .map(|row| row.version)
        .chain(retired.into_iter().map(|row| row.version))
        .collect();
    versions.sort_unstable_by(|left, right| right.cmp(left));
    versions.dedup();
    Ok(versions)
}
