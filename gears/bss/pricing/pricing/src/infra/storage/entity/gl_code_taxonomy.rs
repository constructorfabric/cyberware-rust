//! `SeaORM` entity for `bss.pricing_gl_code_taxonomy` — the general-ledger codes
//! a tenant declares (D-356).
//!
//! [`super::rounding_policy_taxonomy`]'s shape exactly, and its migration
//! (`pricing_gl_code_taxonomy`) carries the argument for why the vocabulary is
//! the tenant's rather than this gear's or ledger's: ledger stores account
//! *class* and takes the concrete code from the Catalog snapshot, so Catalog was
//! already the source and the only thing it can honestly refuse is a reference to
//! something nobody declared.
//!
//! It is **not** a fifth `TaxonomyClass`: that enum's token is the overlay
//! `scope_class` column, and an overlay cannot be scoped by GL code.
//!
//! `resource_col` is the tenant, as on [`super::policy_object`]: per-tenant
//! configuration has no row-level resource id for a single-row gate to pin.

use sea_orm::entity::prelude::*;
use toolkit_db_macros::Scopable;
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
#[sea_orm(table_name = "pricing_gl_code_taxonomy")]
#[secure(
    tenant_col = "tenant_id",
    resource_col = "tenant_id",
    no_owner,
    no_type
)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub tenant_id: Uuid,
    /// The declared code, e.g. `4000-REV`. Never blank: the `CHECK` refuses it,
    /// and an unset `glCode` is spelled `NULL` on the descriptor set rather than
    /// as an empty member here.
    #[sea_orm(primary_key, auto_increment = false)]
    pub value: String,
    /// The operator's label for the code.
    pub display_name: String,
    /// `active` | `retired`. A retired code keeps resolving for the published
    /// descriptor sets that already name it and cannot be newly authored —
    /// retirement on every taxonomy in this gear means the same thing.
    pub state: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
