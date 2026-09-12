//! `SeaORM` entity for `bss.pricing_catalog_version_ref` — the pending-vs-
//! committed `CatalogVersion` linkage of one publish
//! (`design/01-foundation.md` §3.7).
//!
//! `catalog_version` and `committed_at` are set together at commit and never
//! re-pointed: an already-posted period resolves through the pin, so a ref that
//! could move would change what that period was priced from.
//!
//! `subject_kind` / `subject_ref` name **what the publish unit projects**. The
//! projector arrives at `CatalogVersionPublished` holding committed refs and
//! has to write exactly those subjects (§4.4, D-86/D-91); without them there is
//! no path from a pending handle back to what it published. The four tokens are
//! `pricing_read_model`'s, rendered from `domain::read_model::SubjectKind`.


use sea_orm::entity::prelude::*;
use toolkit_db_macros::Scopable;
use uuid::Uuid;
use time::OffsetDateTime;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
#[sea_orm(table_name = "pricing_catalog_version_ref")]
#[secure(
    tenant_col = "tenant_id",
    resource_col = "pending_ref",
    no_owner,
    no_type
)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub tenant_id: Uuid,
    /// The registry's pending handle, stamped at publish.
    #[sea_orm(primary_key, auto_increment = false)]
    pub pending_ref: String,
    /// What kind of subject this publish unit projects.
    ///
    /// **In the key since `pricing_catalog_version_ref`** (D-234): one publish unit
    /// projects one subject on the plan plane and two or three on the overlay
    /// plane (D-112, D-133), so the handle alone does not identify a row.
    #[sea_orm(primary_key, auto_increment = false)]
    pub subject_kind: String,
    /// Which one.
    #[sea_orm(primary_key, auto_increment = false)]
    pub subject_ref: String,
    /// The revision of the subject the publish unit judged; `NULL` for a
    /// subject kind that has no revision concept.
    pub subject_revision: Option<i64>,
    /// The lifecycle state the publish unit judged — `published` or `retired`,
    /// the two D-128 sanctions for a projected subject. `NULL` for a subject
    /// kind with no lifecycle.
    pub subject_lifecycle_state: Option<String>,
    /// The end of the interval the publish unit judged — the membership plane's
    /// pin (`pricing_catalog_version_ref`), and `NULL` on every other kind.
    ///
    /// `NULL` is also what an **open-ended** membership pins, which is why the
    /// membership arm additionally pins the row's version in `subject_revision`
    /// and the projector refuses a membership subject that arrives without one:
    /// the presence of the pin is read off that column, so this one is free to
    /// carry a fact rather than an absence.
    pub subject_effective_to: Option<OffsetDateTime>,
    /// `None` until `CatalogVersionPublished` resolves the handle.
    pub catalog_version: Option<i64>,
    pub requested_at: OffsetDateTime,
    /// When this gear first saw the registry's answer for the handle (D-166).
    ///
    /// Deliberately **not** paired with `catalog_version`: it is set while that
    /// column is still NULL, which is the state it exists to describe. See the
    /// migration's doc for why a recorded observation cannot be derived from the
    /// commit — the finalize and the warm share a transaction, so "committed but
    /// unwarm" is unreachable in storage.
    pub commit_observed_at: Option<OffsetDateTime>,
    pub committed_at: Option<OffsetDateTime>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
