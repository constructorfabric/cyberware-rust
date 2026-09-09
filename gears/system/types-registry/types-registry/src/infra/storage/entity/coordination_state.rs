//! Installation-wide coordination state.
//!
//! Each entity commit advances `entity_write_order` first, using a transaction-bound
//! row lock for serialization. `state_seq` is diagnostic; federation will also use
//! this table for its `routing` generation.

use sea_orm::entity::prelude::*;
use time::OffsetDateTime;
use toolkit_db_macros::Scopable;

// Installation-global state has no tenant scope.
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
#[sea_orm(table_name = "types_registry__coordination_state")]
#[secure(unrestricted)]
pub struct Model {
    /// The state's purpose (`entity_write_order`, later `routing`).
    #[sea_orm(primary_key, auto_increment = false)]
    pub state_name: String,
    pub state_seq: i64,
    /// When the state last changed; never used for decisions.
    pub updated_at: OffsetDateTime,
}

/// No relations: nothing references these rows and they reference nothing.
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
