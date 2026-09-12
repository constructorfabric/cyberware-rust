//! `SeaORM` entity for `bss.pricing_bulk_operation` — one bulk run and its
//! position in §4's state machine (`design/12-operator-efficiency.md` §6).
//!
//! Keyed on `operation_id` alone: a run is not revision-scoped and not part of
//! any plan's shape, so unlike `pricing_composite_meter` it owes no
//! copy-forward and no drop-on-abandon (D-256's obligation does not reach it).
//!
//! Only `state`, `report` and `completed_at` ever move; everything else is
//! frozen by the table's trigger, because a run's identity, kind and submitter
//! are what its report is *about* — `request_hash` among them since
//! `pricing_bulk_operation`, the digest being what the replay compares an arriving body
//! against.

use sea_orm::entity::prelude::*;
use uuid::Uuid;

use toolkit_db_macros::Scopable;
use time::OffsetDateTime;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
#[sea_orm(table_name = "pricing_bulk_operation")]
#[secure(
    tenant_col = "tenant_id",
    resource_col = "operation_id",
    no_owner,
    no_type
)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub operation_id: Uuid,
    pub tenant_id: Uuid,
    /// `import | repricing`, `CHECK`-constrained.
    pub kind: String,
    /// One of §4's seven states, `CHECK`-constrained; the edges between them are
    /// the table's trigger, not a caller's convention. `rejected` is D-267's,
    /// reachable only from `awaiting_approval` and left by nothing.
    pub state: String,
    /// O4's idempotency key, unique per tenant.
    pub client_key: String,
    /// The SHA-256 of the canonical rendering of the request the key was first
    /// spent on (`pricing_bulk_operation`) — a digest and never the payload, for
    /// `pricing_idempotency_dedup.request_hash`'s reason.
    ///
    /// **Empty means "opened before the column existed"**, which is the only value
    /// no writer produces: the `ALTER` backfilled every pre-existing row with it
    /// and a real digest is 32 bytes. A replay reading an empty digest cannot
    /// compare and does not pretend to.
    pub request_hash: Vec<u8>,
    /// Per-row outcomes. Grows as the run progresses; one of the three columns
    /// the trigger lets move.
    pub report: Json,
    pub submitted_by: Uuid,
    pub submitted_at: OffsetDateTime,
    /// Set exactly on the terminal states — `rejected` among them since D-267,
    /// a refused batch approval ending the run — and the `CHECK` keeps the two
    /// from disagreeing about whether the run is over.
    pub completed_at: Option<OffsetDateTime>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
