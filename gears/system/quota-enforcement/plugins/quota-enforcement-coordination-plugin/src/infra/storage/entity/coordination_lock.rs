//! `qe_coordination_locks`: one row per [`LockScope`](quota_enforcement_sdk::LockScope).
//!
//! Schema per dialect:
//!
//! * `key`          `TEXT` PRIMARY KEY                — the `LockScope` key.
//! * `holder_id`    `UUID` / `TEXT` NULL              — current holder; `NULL` is free.
//! * `locked_until` `TIMESTAMPTZ` / `TEXT` NOT NULL   — database-clock expiry; epoch when free.
//! * `attempts`     `INTEGER` NOT NULL DEFAULT `0`    — steal counter for operators.
//!
//! Every comparison runs on the database clock through dialect SQL in
//! [`crate::domain::DbCoordination`]. The `OffsetDateTime` field carries the
//! worker's read of `locked_until`; the row's truth is what the database
//! committed.
//!
//! The row is process-coordination state, not a tenant resource, so it is
//! declared `no_tenant, no_resource, no_owner, no_type` and read under
//! `AccessScope::allow_all()` by this plugin only.

use sea_orm::entity::prelude::*;
use time::OffsetDateTime;
use toolkit_db_macros::Scopable;
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
#[sea_orm(table_name = "qe_coordination_locks")]
#[secure(no_tenant, no_resource, no_owner, no_type)]
pub struct Model {
    /// `LockScope::key()`.
    #[sea_orm(primary_key, auto_increment = false)]
    pub key: String,
    /// Current holder. `NULL` when the row is free.
    pub holder_id: Option<Uuid>,
    /// Database-clock expiry. Epoch sentinel when free.
    pub locked_until: OffsetDateTime,
    /// Increments on every steal. Reset to `0` on a clean release.
    pub attempts: i32,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
