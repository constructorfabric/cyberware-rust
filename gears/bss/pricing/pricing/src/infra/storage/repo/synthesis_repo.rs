//! The `migrated-origin` record's storage half (`inst-sy-provenance`,
//! `inst-sy-surface`, D-87, D-102).
//!
//! A runner-taking repository, for [`super::audit_repo`]'s reason: the freeze and
//! the audit record that governs it belong to one transaction.
//!
//! # Idempotency is the unique index, and the read-back is the point
//!
//! §9 requires a second synthesis attempt to be idempotent — *the same frozen
//! ref*. [`freeze_or_load`] is `INSERT... ON CONFLICT DO NOTHING` plus a load,
//! [`super::idempotency_repo::claim`]'s shape: two concurrent syntheses of one
//! subscription cannot both land, and the loser is handed the winner's row rather
//! than an error. That is stronger than "check then write", which would leave a
//! window exactly as wide as the round trip — and in that window the two calls
//! would freeze **different instants**, because D-81 computes `t` per trigger and
//! the two triggers disagree.
//!
//! There is no `update` and no `delete` here, and none is missing: the table
//! refuses both outright.

use sea_orm::ActiveValue::Set;
use sea_orm::sea_query::OnConflict;
use sea_orm::{ColumnTrait, Condition, DbErr, EntityTrait};
use serde_json::Value as JsonValue;
use toolkit_db::secure::{AccessScope, DBRunner, ScopeError, SecureEntityExt, SecureInsertExt};
use uuid::Uuid;

use crate::domain::scope_key::PlanId;
use crate::domain::synthesis::SynthesisTrigger;
use crate::infra::storage::RepoError;
use crate::infra::storage::entity::snapshot_provenance;
use crate::infra::storage::repo::check_authored_instant;
use time::OffsetDateTime;

/// A frozen `migrated-origin` snapshot as this gear holds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProvenanceRecord {
    /// The record's own id.
    pub provenance_id: Uuid,
    /// The subscription it belongs to.
    pub subscription_ref: Uuid,
    /// The plan synthesis was about.
    pub source_plan_id: PlanId,
    /// The revision, where the resolved rows had one (D-87: tier 2 has none).
    pub source_revision: Option<u64>,
    /// D-81's instant `t`.
    pub snapshot_instant: OffsetDateTime,
    /// Which trigger froze it.
    pub trigger: SynthesisTrigger,
    /// Who acted.
    pub acting_principal: Uuid,
    /// The resolved ids and their selection tiers.
    pub resolved: JsonValue,
    /// The self-contained payload.
    pub payload: JsonValue,
}

/// The row a snapshot is frozen from.
#[derive(Clone, Debug)]
pub struct NewProvenance {
    /// The record's own id.
    pub provenance_id: Uuid,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// The subscription.
    pub subscription_ref: Uuid,
    /// The plan.
    pub source_plan_id: PlanId,
    /// The revision, where there is one.
    pub source_revision: Option<u64>,
    /// D-81's instant.
    pub snapshot_instant: OffsetDateTime,
    /// The trigger.
    pub trigger: SynthesisTrigger,
    /// The acting principal.
    pub acting_principal: Uuid,
    /// The resolved ids with their tiers.
    pub resolved: JsonValue,
    /// The materialized payload.
    pub payload: JsonValue,
    /// The commit instant.
    pub created_at: OffsetDateTime,
}

/// What [`freeze_or_load`] answered with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frozen {
    /// The record, whether it was just frozen or already existed.
    pub record: ProvenanceRecord,
    /// `false` when this call found an earlier snapshot (§9's idempotency).
    pub created: bool,
}

/// Freeze a snapshot, or return the one this subscription already holds
/// (`inst-sy-freeze`, §9).
///
/// # `snapshot_instant` meets D-144's quantum; `created_at` does not
///
/// D-81's `t` is supplied by the trigger, carried back in a contract field
/// (`MigratedOriginSnapshotView.snapshot_instant`) and **compared** — the whole
/// selection is "the row whose window covered `t`" — which is
/// [`super::check_authored_instant`]'s scope exactly. Refused rather than
/// truncated, for D-144's reason and one of this table's own: the freeze records
/// the instant it resolved at, and a truncating write would record an instant it
/// did not resolve at.
///
/// `created_at` is the commit instant, which `domain::instant` names as bookkeeping
/// outside the rule — the same split [`super::migration_repo::insert_or_load`]
/// makes between its two columns. The two are equal on today's only caller and
/// that is a coincidence of the caller, not a fact about the columns: the migration
/// declares no relation between them.
///
/// # Errors
/// [`RepoError::TimestampPrecisionExceeded`] when `snapshot_instant` is finer than
/// the millisecond quantum; [`RepoError::Db`] on a storage failure;
/// [`RepoError::CorruptRow`] when the
/// stored row cannot be read back; [`RepoError::ConcurrentMutation`] when the
/// insert is refused and the conflicting row cannot then be read, which the
/// table's `DELETE` ban makes unreachable and which is reported rather than
/// unwrapped.
pub async fn freeze_or_load(
    runner: &impl DBRunner,
    scope: &AccessScope,
    new: NewProvenance,
) -> Result<Frozen, RepoError> {
    check_authored_instant("snapshotInstant", Some(new.snapshot_instant))?;
    // `i64` and not `i32`, which is `pricing_snapshot_provenance`'s own column type —
    // `migration_repo::insert_or_load`'s note, the same narrowing on the other of
    // the two tables that carried it.
    let revision = match new.source_revision {
        Some(revision) => match i64::try_from(revision) {
            Ok(revision) => Some(revision),
            Err(_) => {
                return Err(RepoError::CorruptRow(format!(
                    "plan {} stands at a revision {revision} no column can address",
                    new.source_plan_id
                )));
            }
        },
        None => None,
    };

    let model = snapshot_provenance::ActiveModel {
        provenance_id: Set(new.provenance_id),
        tenant_id: Set(new.tenant_id),
        subscription_ref: Set(new.subscription_ref),
        source_plan_id: Set(new.source_plan_id.get()),
        source_revision: Set(revision),
        snapshot_instant: Set(new.snapshot_instant),
        trigger_kind: Set(new.trigger.as_str().to_owned()),
        acting_principal: Set(new.acting_principal),
        resolved: Set(new.resolved),
        payload: Set(new.payload),
        created_at: Set(new.created_at),
    };

    // The conflict target is the **subscription**, not the primary key: two
    // syntheses of one subscription mint different `provenance_id`s, so a
    // primary-key conflict would never fire and both would land.
    let on_conflict = OnConflict::columns([
        snapshot_provenance::Column::TenantId,
        snapshot_provenance::Column::SubscriptionRef,
    ])
    .do_nothing()
    .to_owned();

    let created = match snapshot_provenance::Entity::insert(model.clone())
        .secure()
        .scope_with_model(scope, &model)
        .map_err(|e| RepoError::Db(format!("snapshot provenance scope: {e}")))?
        .on_conflict_raw(on_conflict)
        .exec(runner)
        .await
    {
        Ok(_) => true,
        Err(ScopeError::Db(DbErr::RecordNotInserted)) => false,
        Err(e) => {
            return Err(RepoError::Db(format!(
                "freeze the migrated-origin snapshot of subscription {}: {e}",
                new.subscription_ref
            )));
        }
    };

    let record = load(runner, scope, new.tenant_id, new.subscription_ref)
        .await?
        .ok_or_else(|| RepoError::ConcurrentMutation {
            aggregate: format!("subscription {}", new.subscription_ref),
        })?;

    Ok(Frozen { record, created })
}

/// Read one subscription's frozen snapshot (`inst-sy-surface`, D-102).
///
/// **Keyed by subscription, not by provenance id**, because that is what the read
/// surface's path carries and what Rating holds: a consumer of a
/// `migrated-origin` ref knows the subscription and nothing else — it cannot look
/// the record up any other way, there being no `CatalogVersion` to resolve
/// through.
///
/// # Errors
/// [`RepoError::Db`] on a storage failure; [`RepoError::CorruptRow`] on an
/// unreadable trigger token or a negative revision.
pub async fn load(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    subscription_ref: Uuid,
) -> Result<Option<ProvenanceRecord>, RepoError> {
    let row = snapshot_provenance::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(snapshot_provenance::Column::TenantId.eq(tenant_id))
                .add(snapshot_provenance::Column::SubscriptionRef.eq(subscription_ref)),
        )
        .one(runner)
        .await
        .map_err(|e| {
            RepoError::Db(format!(
                "read the migrated-origin snapshot of subscription {subscription_ref}: {e}"
            ))
        })?;

    row.map(into_record).transpose()
}

/// Read a stored row into the domain's vocabulary.
fn into_record(row: snapshot_provenance::Model) -> Result<ProvenanceRecord, RepoError> {
    let trigger = SynthesisTrigger::parse(&row.trigger_kind).ok_or_else(|| {
        RepoError::CorruptRow(format!(
            "pricing_snapshot_provenance.trigger_kind `{}` on subscription {}",
            row.trigger_kind, row.subscription_ref
        ))
    })?;
    let source_revision = match row.source_revision {
        Some(revision) => match u64::try_from(revision) {
            Ok(revision) => Some(revision),
            Err(_) => {
                return Err(RepoError::CorruptRow(format!(
                    "pricing_snapshot_provenance.source_revision {revision} on subscription {} is \
                     negative",
                    row.subscription_ref
                )));
            }
        },
        None => None,
    };

    Ok(ProvenanceRecord {
        provenance_id: row.provenance_id,
        subscription_ref: row.subscription_ref,
        source_plan_id: PlanId::new(row.source_plan_id),
        source_revision,
        snapshot_instant: row.snapshot_instant,
        trigger,
        acting_principal: row.acting_principal,
        resolved: row.resolved,
        payload: row.payload,
    })
}
