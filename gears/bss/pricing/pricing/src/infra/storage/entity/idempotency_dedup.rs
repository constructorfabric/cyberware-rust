//! `SeaORM` entity for `bss.pricing_idempotency_dedup` — the at-most-once gate
//! and the replay-response source (`design/01-foundation.md` §3.7).
//!
//! A replay whose `request_hash` matches returns the stored response; a replay
//! whose hash differs is rejected with `IDEMPOTENCY_PAYLOAD_MISMATCH` and is
//! neither replayed nor re-executed. The check precedes the `ETag` check.
//!
//! The response pair is optional because the row is written by the claim, which
//! happens before the guarded operation has an answer — see the migration's
//! module doc. `None` reads as "claimed, not yet answered", and the two columns
//! move together or not at all.


use sea_orm::entity::prelude::*;
use serde_json::Value as JsonValue;
use toolkit_db_macros::Scopable;
use uuid::Uuid;
use time::OffsetDateTime;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
#[sea_orm(table_name = "pricing_idempotency_dedup")]
#[secure(
    tenant_col = "tenant_id",
    resource_col = "client_key",
    no_owner,
    no_type
)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub tenant_id: Uuid,
    #[sea_orm(primary_key, auto_increment = false)]
    pub operation: String,
    #[sea_orm(primary_key, auto_increment = false)]
    pub client_key: String,
    /// Digest of the request payload, not the payload: the gate needs to know
    /// whether two requests are the same, not what they said.
    pub request_hash: Vec<u8>,
    /// The status the caller was told, once it has been told anything.
    pub response_status: Option<i32>,
    /// The body the caller was told, once it has been told anything.
    pub response_body: Option<JsonValue>,
    /// When the claim was taken. Read at claim time to decide whether the key
    /// has outlived its TTL and may be taken over.
    pub created_at_utc: OffsetDateTime,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
