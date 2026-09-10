//! `SeaORM` entity for `bss.pricing_bulk_row_lock` — one price row held by an
//! in-flight bulk operation (`design/12-operator-efficiency.md` §6,
//! `inst-bk-lock`; Foundation `fr-concurrent-edit`).
//!
//! Keyed `(tenant_id, price_id)`, which **is** the mutual exclusion: two runs
//! cannot hold one row, because the key admits one lock per row per tenant and
//! the second insert collides.
//!
//! The row exists to be read by the concurrent-edit check and to name the
//! operation in the conflict it raises, so `bulk_operation_id` is the payload
//! rather than an audit trail. Nothing here ever moves: a lock is inserted when
//! its run enters `committing` and deleted when the run leaves it, and the
//! table's trigger refuses `UPDATE` outright while leaving `DELETE` unguarded —
//! see the migration's module doc for why the asymmetry is the rule.

use sea_orm::entity::prelude::*;
use uuid::Uuid;

use toolkit_db_macros::Scopable;
use time::OffsetDateTime;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
#[sea_orm(table_name = "pricing_bulk_row_lock")]
#[secure(tenant_col = "tenant_id", resource_col = "price_id", no_owner, no_type)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub tenant_id: Uuid,
    /// The held price row. Second half of the key rather than first, because the
    /// tenant leads every `SecureORM` predicate.
    #[sea_orm(primary_key, auto_increment = false)]
    pub price_id: Uuid,
    /// The operation holding it — the fact the concurrent-edit conflict names,
    /// and the column the release sweep keys on.
    pub bulk_operation_id: Uuid,
    /// When the lock was taken. **Written once and read by nothing in this crate**
    /// — forensic, not live.
    ///
    /// **No stalled-run alarm reads it**, and none exists to: the `stalled` matches in
    /// `src/` are the window-activation lease singleton and
    /// `pricing.migration.stalled`, a different subject on a different table. `grep
    /// -rn "PricingAlarm::" src/infra/metrics.rs` enumerates the alarms that do exist,
    /// and none is about a lock's age.
    ///
    /// The second half of that sentence was always right and is the interesting
    /// half: **nothing expires a lock on age**, by design, because D-37 releases
    /// through lease takeover and operator abort instead. So a reader owes this
    /// column an alarm or nothing at all; today it is nothing.
    pub locked_at: OffsetDateTime,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
