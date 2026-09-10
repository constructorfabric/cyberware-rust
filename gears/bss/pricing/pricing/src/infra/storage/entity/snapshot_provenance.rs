//! `SeaORM` entity for `bss.pricing_snapshot_provenance` — the frozen
//! `migrated-origin` record (`design/11-lifecycle.md` §6, `pricing_snapshot_provenance`).
//!
//! Every column is written once and never again: the table's trigger refuses
//! `UPDATE` outright, because a `migrated-origin` ref resolves through no
//! `CatalogVersion` and this row is therefore the only thing that makes the
//! snapshot immutable.
//!
//! `source_revision` is the one nullable column that is not an absence: D-87
//! states that a tier-2 fully-legacy key may have **no plan revision at all**, so
//! `None` is a fact about where the row came from rather than something not yet
//! filled in.


use sea_orm::entity::prelude::*;
use serde_json::Value as JsonValue;
use toolkit_db_macros::Scopable;
use uuid::Uuid;
use time::OffsetDateTime;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
#[sea_orm(table_name = "pricing_snapshot_provenance")]
#[secure(
    tenant_col = "tenant_id",
    resource_col = "provenance_id",
    no_owner,
    no_type
)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub provenance_id: Uuid,
    pub tenant_id: Uuid,
    /// The subscription this snapshot belongs to. Unique per tenant — one
    /// subscription holds at most one `migrated-origin` snapshot, ever (§9's
    /// idempotency rule, as `uq_pricing_snapshot_provenance_subscription`).
    pub subscription_ref: Uuid,
    pub source_plan_id: Uuid,
    /// `None` for a tier-2 fully-legacy key (D-87).
    ///
    /// `i64` since `pricing_snapshot_provenance` — `pricing_migration.source_revision`'s note,
    /// and the same widening: the nullability is the D-87 signal, the width was an
    /// outlier.
    pub source_revision: Option<i64>,
    /// D-81's per-trigger instant `t`, UTC, frozen at execution.
    pub snapshot_instant: OffsetDateTime,
    /// `migration` | `first_rating`.
    pub trigger_kind: String,
    pub acting_principal: Uuid,
    /// The resolved row ids with the selection tier each came from (D-76).
    pub resolved: JsonValue,
    /// D-87's self-contained payload: row content plus the plan-level descriptor
    /// set and grant set. What rating evaluates from and Billing posts from,
    /// resolving no id through the read model.
    pub payload: JsonValue,
    pub created_at: OffsetDateTime,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
