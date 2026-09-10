//! `SeaORM` entity for `bss.pricing_approval_threshold_tombstone` — one version of
//! a tenant's approval-threshold policy that says the tenant has **no** thresholds
//! (D-185, `design/05-governance.md` §6).
//!
//! One row is one version, and it carries no `currency`: a tombstone is a statement
//! about the whole policy rather than about any currency in it, which is exactly
//! what `approval_threshold`'s three-part key cannot express. Its counterpart holds
//! one row *per currency* of a version; this holds one row per version *and no
//! entries at all*, and the two together are the version sequence
//! `threshold_repo::latest_version` walks.
//!
//! **There is no `state` column here either**, for [`approval_threshold`]'s reason:
//! whether this version is the tenant's policy is a fact about `pricing_approval` —
//! the greatest version whose unit an independent principal approved and whose
//! `effective_from` has arrived — and a column here would be a second answer to it.
//!
//! The store refuses both `DELETE` and `UPDATE`: a retirement that could be edited
//! after a reviewer signed it is a signature over content nobody can reconstruct,
//! and the whole point of the tombstone is that the reviewer signed *this*.
//!
//! [`approval_threshold`]: super::approval_threshold


use sea_orm::entity::prelude::*;
use toolkit_db_macros::Scopable;
use uuid::Uuid;
use time::OffsetDateTime;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
#[sea_orm(table_name = "pricing_approval_threshold_tombstone")]
#[secure(
    tenant_col = "tenant_id",
    resource_col = "tenant_id",
    no_owner,
    no_type
)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub tenant_id: Uuid,
    /// The policy version this tombstone **is**. Drawn from the same sequence as
    /// [`super::approval_threshold::Model::version`], because a tombstone is a
    /// version like any other — pinned, approved, and superseded by the next one.
    #[sea_orm(primary_key, auto_increment = false)]
    pub version: i64,
    /// When the tenant stops having thresholds, UTC. Authored and inside the
    /// approval pin, exactly as it is on an entry version: an operator who could
    /// move it after a reviewer signed would move when the two-person rule comes
    /// back.
    pub effective_from: OffsetDateTime,
    /// The pseudonymous principal that proposed the retirement. **Not** the
    /// approval trail — D-10 puts the second principal on `pricing_approval`, and
    /// a tenant must not be able to revert the two-person rule single-handed.
    pub created_by: Uuid,
    pub created_at: OffsetDateTime,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
