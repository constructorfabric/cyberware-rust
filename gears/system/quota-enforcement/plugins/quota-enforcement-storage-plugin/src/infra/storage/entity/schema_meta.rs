//! `qe_schema_meta`: the installed contract major (invariant I12).
//!
//! One row. `bootstrap()` inserts it on a fresh schema and compares it on
//! every later start.

use sea_orm::entity::prelude::*;
use time::OffsetDateTime;
use toolkit_db_macros::Scopable;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
#[sea_orm(table_name = "qe_schema_meta")]
#[secure(no_tenant, no_resource, no_owner, no_type)]
pub struct Model {
    /// Installed contract major. Primary key so the table holds one row per
    /// major, and the plugin expects exactly one.
    #[sea_orm(primary_key, auto_increment = false)]
    pub contract_major: i32,
    /// When the row was written.
    pub applied_at: OffsetDateTime,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
