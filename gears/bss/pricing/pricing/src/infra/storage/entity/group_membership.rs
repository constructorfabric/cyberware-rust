//! `SeaORM` entity for `bss.pricing_group_membership` — the effective-dated,
//! audited membership record on `payerTenantId`
//! (`design/09-price-overlays.md` §3 `inst-cg-record` / `inst-cg-resolve`, §6).
//!
//! `effective_to` is `Option` and its absence is the open-ended value, the
//! same reading [`super::price_window`]'s carries: `NULL` is a membership that
//! has not (yet) been ended, not a bound nobody got round to setting.
//!
//! # `group_value` is stored by value, not by foreign key
//!
//! Like [`super::price_overlay`]'s `scope_value`, this column holds the
//! taxonomy value directly rather than referencing
//! [`super::customer_group_taxonomy`]'s row: the taxonomy is a **governed**
//! value set whose retirement is guarded by a referential check
//! (`inst-tx-mutation`'s retire guard, `TAXONOMY_VALUE_IN_USE`) rather than by
//! a `FOREIGN KEY... ON DELETE CASCADE`, so a real foreign key would let a
//! retirement cascade-delete every payer's membership in the retiring group —
//! exactly the silent data loss the retire guard exists to refuse instead.
//!
//! # No stored `state`
//!
//! §4's three states (`scheduled` / `active` / `ended`) are a function of
//! `now()` against `[effective_from, effective_to)`, not a column: nothing here
//! could disagree with the interval, because nothing here duplicates it.
//!
//! # D-09's non-overlap invariant is not visible on this entity at all
//!
//! It is enforced by `excl_pricing_group_membership_no_overlap` (Postgres) and
//! a pair of `RAISE(ABORT, …)` triggers (`SQLite`), both declared in
//! `pricing_group_membership` and neither reachable through `SeaORM`'s model — the same
//! arrangement every guarded table in this crate takes, since a `SeaORM`
//! `Model` carries columns and not constraints.


use sea_orm::entity::prelude::*;
use time::OffsetDateTime;
use toolkit_db_macros::Scopable;
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
#[sea_orm(table_name = "pricing_group_membership")]
#[secure(
    tenant_col = "tenant_id",
    resource_col = "membership_id",
    no_owner,
    no_type
)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub membership_id: Uuid,
    pub tenant_id: Uuid,
    /// The payer's commercial-profile key membership resolves by
    /// (`inst-cg-record`) — AMS supplies identity only; tenant topology is
    /// never modified.
    pub payer_tenant_id: Uuid,
    /// Taxonomy-validated against [`super::customer_group_taxonomy`], stored by
    /// value. See the module doc for why this is not a foreign key.
    pub group_value: String,
    /// Inclusive start of the half-open interval, UTC.
    pub effective_from: OffsetDateTime,
    /// **Exclusive** end, UTC. `None` is open-ended — a membership not (yet)
    /// ended.
    pub effective_to: Option<OffsetDateTime>,
    /// **Pseudonymous** principal id of whoever recorded the membership.
    pub created_by: Uuid,
    pub created_at_utc: OffsetDateTime,
    /// The row's concurrency token, the `pricing_plan` / `pricing_price_overlay`
    /// revision's column under the same name — an authoring `PATCH` (ending or
    /// adjusting an interval) answers `If-Match` against it.
    pub row_version: i64,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
