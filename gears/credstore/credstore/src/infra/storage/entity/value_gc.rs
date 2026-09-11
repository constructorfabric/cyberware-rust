//! `SeaORM` entity for the `credstore_value_gc` table (ADR-0006).
//!
//! The intent log and work queue for backend-value garbage collection: a
//! `pending` row proves bytes were about to be written for a `value_id`
//! before the backend call happened; every other reason is an
//! already-decided deletion the maintenance job has not yet drained. It
//! carries only `tenant_id` and `value_id` — no `reference`, no `owner_id`,
//! no secret bytes, no fingerprint — and is internal bookkeeping never
//! queried under a PDP scope, so it is `Scopable` with `no_owner, no_type`
//! purely to satisfy the toolkit's secure-query plumbing; every access in
//! this gear passes `AccessScope::allow_all()`.
use sea_orm::entity::prelude::*;
use time::OffsetDateTime;
use toolkit_db::secure::Scopable;
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
#[sea_orm(table_name = "credstore_value_gc")]
#[secure(tenant_col = "tenant_id", resource_col = "value_id", no_owner, no_type)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub value_id: Uuid,
    pub tenant_id: Uuid,
    /// Reason: 1=Pending, 2=Superseded, 3=Removed, 4=Aborted.
    pub reason: i16,
    pub enqueued_at: OffsetDateTime,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
