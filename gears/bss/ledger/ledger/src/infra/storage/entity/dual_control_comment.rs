//! `SeaORM` entity for `bss.ledger_approval_comment` — the append-only
//! preparer↔approver thread on a dual-control approval (§4.5). Carries free
//! comments / questions (no state change) and the mandatory reason attached to a
//! `reject` / `request-changes` decision. Append-only: `UPDATE`/`DELETE` are
//! revoked at the DB role (same encapsulation as posted financial facts);
//! tenant-scoped via `SecureORM`, resource col is the parent `approval_id`.

use sea_orm::entity::prelude::*;
use time::OffsetDateTime;
use toolkit_db_macros::Scopable;
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
#[sea_orm(table_name = "ledger_approval_comment")]
#[secure(
    tenant_col = "tenant_id",
    resource_col = "approval_id",
    no_owner,
    no_type
)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub comment_id: Uuid,
    pub approval_id: Uuid,
    pub tenant_id: Uuid,
    /// The approval `revision` this comment was made against.
    pub revision: i32,
    pub author_actor: Uuid,
    pub body: String,
    pub created_at: OffsetDateTime,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
