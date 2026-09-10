//! `SeaORM` entity for `bss.ledger_journal_entry` (append-only truth header).

use chrono::NaiveDate;
use sea_orm::entity::prelude::*;
use serde_json::Value as JsonValue;
use time::OffsetDateTime;
use toolkit_db_macros::Scopable;
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
#[sea_orm(table_name = "ledger_journal_entry")]
#[secure(tenant_col = "tenant_id", resource_col = "entry_id", no_owner, no_type)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub entry_id: Uuid,
    #[sea_orm(primary_key, auto_increment = false)]
    pub tenant_id: Uuid,
    pub legal_entity_id: Uuid,
    #[sea_orm(primary_key, auto_increment = false)]
    pub period_id: String,
    pub entry_currency: String,
    pub source_doc_type: String,
    pub source_business_id: String,
    pub reverses_entry_id: Option<Uuid>,
    pub reverses_period_id: Option<String>,
    pub posted_at_utc: OffsetDateTime,
    pub effective_at: NaiveDate,
    pub origin: String,
    pub posted_by_actor_id: Uuid,
    pub correlation_id: Uuid,
    pub rounding_evidence: JsonValue,
    pub created_seq: i64,
    pub row_hash: Option<Vec<u8>>,
    pub prev_hash: Option<Vec<u8>>,
    pub prev_entry_id: Option<Uuid>,
    pub prev_period_id: Option<String>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
