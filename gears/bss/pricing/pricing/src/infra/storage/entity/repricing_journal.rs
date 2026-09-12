//! `SeaORM` entity for `bss.pricing_repricing_journal` — one selected price row
//! of one mass-repricing run, and how far that row got
//! (`design/12-operator-efficiency.md` §6, `inst-mr-journal`).
//!
//! Keyed `(run_id, price_id)`, which is also the dedup key the run's events
//! carry: the journal row and the event about it name the same pair, so a
//! consumer de-duplicating on redelivery and a re-drive skipping applied rows
//! are reading one identity rather than two that have to be kept in step.
//!
//! Only `state`, `failure_reason`, `applied_price_id` and `applied_at` ever
//! move, and only while the row is `pending` — the table's trigger freezes a
//! decided row whole, so an `applied` row cannot later be made to name a
//! different successor.

use sea_orm::entity::prelude::*;
use uuid::Uuid;

use toolkit_db_macros::Scopable;
use time::OffsetDateTime;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
#[sea_orm(table_name = "pricing_repricing_journal")]
#[secure(tenant_col = "tenant_id", resource_col = "price_id", no_owner, no_type)]
pub struct Model {
    /// The run — a `pricing_bulk_operation` of `kind = repricing`, which the
    /// table's own trigger holds it to rather than leaving to a caller: a bulk
    /// import's per-row outcomes live in the operation's `report`, so a row here
    /// under an import is a record no code will ever complete.
    #[sea_orm(primary_key, auto_increment = false)]
    pub run_id: Uuid,
    /// The selected price row, frozen into the run's row set at expansion, and
    /// keyed to `pricing_price`.
    #[sea_orm(primary_key, auto_increment = false)]
    pub price_id: Uuid,
    /// Written by the expansion, never taken from a request: the foreign key
    /// covers `run_id` alone, so nothing in the schema stops a journal row
    /// carrying a foreign tenant.
    pub tenant_id: Uuid,
    /// `pending | applied | failed`, `CHECK`-constrained; the edges out of
    /// `pending` and the finality of the other two are the table's trigger.
    pub state: String,
    /// Why this row failed — shared with every other row of its plan when the
    /// D-134 plan-level pass is what refused it. Present exactly on `failed`.
    pub failure_reason: Option<String>,
    /// The successor row the apply created (`inst-mp-standard`: a new immutable
    /// row, never a mutation in place). Present exactly on `applied`, keyed to
    /// `pricing_price`, and the `CHECK` refuses it naming the selected row
    /// itself.
    pub applied_price_id: Option<Uuid>,
    /// When the apply committed. Present exactly on `applied`.
    pub applied_at: Option<OffsetDateTime>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
