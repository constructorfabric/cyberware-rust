//! `SeaORM` entity for `bss.secured_audit_record` (the secured audit store).
//! Append-only: each row is born sealed (`row_hash` / `prev_hash` non-NULL)
//! linked into the tenant's own per-tenant audit hash chain, and is never
//! updated or deleted (the Postgres `reject_mutation()` trigger enforces it).

use sea_orm::entity::prelude::*;
use serde_json::Value as JsonValue;
use time::OffsetDateTime;
use toolkit_db_macros::Scopable;
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
#[sea_orm(table_name = "secured_audit_record")]
#[secure(tenant_col = "tenant_id", no_resource, no_owner, no_type)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub audit_id: Uuid,
    pub tenant_id: Uuid,
    pub event_type: String,
    pub actor_ref: Option<String>,
    pub reason_code: Option<String>,
    pub before_after: JsonValue,
    pub correlation_id: Option<Uuid>,
    pub row_hash: Vec<u8>,
    pub prev_hash: Vec<u8>,
    pub at_utc: OffsetDateTime,
    pub retain_until: Option<OffsetDateTime>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
