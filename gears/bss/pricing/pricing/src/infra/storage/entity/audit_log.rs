//! `SeaORM` entity for `bss.pricing_audit_log` — one link of a
//! `(tenant_id, chain_id)`-segmented hash chain
//! (`design/01-foundation.md` §3.7, D-135).
//!
//! `actor_principal_id` is a `Uuid` on purpose: the actor is a **pseudonymous
//! principal id**, never a display name or an email (D-61 / `inst-au-pii`), and
//! a retention horizon of seven-plus years must hold no directly identifying
//! operator PII. Typing the column makes that structural rather than a
//! convention a later writer could break.

use sea_orm::entity::prelude::*;
use time::OffsetDateTime;
use serde_json::Value as JsonValue;
use toolkit_db_macros::Scopable;
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
#[sea_orm(table_name = "pricing_audit_log")]
#[secure(tenant_col = "tenant_id", resource_col = "chain_id", no_owner, no_type)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub tenant_id: Uuid,
    /// The audited subject's aggregate: plan, overlay, payer, policy or bulk
    /// operation. One chain segment per value (D-135).
    #[sea_orm(primary_key, auto_increment = false)]
    pub chain_id: Uuid,
    #[sea_orm(primary_key, auto_increment = false)]
    pub seq: i64,
    /// `mutation` | `rollup`. A roll-up row chains the tenant's segment heads.
    pub entry_kind: String,
    pub recorded_at: OffsetDateTime,
    /// Pseudonymous principal id. Never a name, never an email.
    pub actor_principal_id: Uuid,
    pub action: String,
    pub subject_kind: String,
    pub subject_ref: String,
    pub before_state: Option<JsonValue>,
    pub after_state: Option<JsonValue>,
    pub approval_ref: Option<Uuid>,
    pub correlation_id: Option<Uuid>,
    /// Present exactly on `rollup` rows: the segment heads this row chains.
    pub segment_heads: Option<JsonValue>,
    pub prev_hash: Option<Vec<u8>>,
    pub row_hash: Vec<u8>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
