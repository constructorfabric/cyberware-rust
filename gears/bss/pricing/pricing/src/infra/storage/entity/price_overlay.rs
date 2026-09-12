//! `SeaORM` entity for `bss.pricing_price_overlay` — the overlay header, keyed
//! `(price_overlay_id, revision)` (`design/09-price-overlays.md` §6, D-42, D-92,
//! D-107).
//!
//! The revision half of the key is D-92 and is load-bearing: editing a published
//! overlay opens a **new revision row in `draft`**, the submit publishes it and
//! flips its predecessor `published -> superseded` in one commit, and the
//! projector reads the **published revision's** rows. Without it a draft edit
//! would mutate published truth.
//!
//! # `scope_value` is never NULL, and the classless scope is the empty string
//!
//! `pricing_price_overlay`'s migration doc carries the argument. The consequence for a
//! reader of this type is worth stating here too: comparing this column needs no
//! null-safe form, and a `global` overlay is the one whose value is `""`. The
//! typed reading lives in `domain::overlay::ScopeSelector`, which makes the
//! pairing unrepresentable rather than merely checked.
//!
//! # `row_version` is not in §6's column list
//!
//! It is the `pricing_plan` revision's concurrency column under the same name and
//! the same D-170 entity-tag shape (`"<revision>-<version>"`). Without it the
//! authoring `PATCH` has no precondition to answer and two authors editing one
//! draft revision both satisfy `If-Match`. Reported in the owed register.
//!
//! `target_ref` is `jsonb` on Postgres and `text` on `SQLite`, so it is
//! [`Json`](sea_orm::entity::prelude::Json) here — the same treatment
//! [`super::approval`]'s pinned payload takes.


use sea_orm::entity::prelude::*;
use toolkit_db_macros::Scopable;
use uuid::Uuid;
use time::OffsetDateTime;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
#[sea_orm(table_name = "pricing_price_overlay")]
#[secure(
    tenant_col = "tenant_id",
    resource_col = "price_overlay_id",
    no_owner,
    no_type
)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub price_overlay_id: Uuid,
    /// PK half; monotonic per overlay (D-92).
    #[sea_orm(primary_key, auto_increment = false)]
    pub revision: i64,
    pub tenant_id: Uuid,
    /// `draft` | `abandoned` | `published` | `superseded`. A discarded draft
    /// revision leaves by the terminal `draft -> abandoned` flip and **never** by
    /// `DELETE`, which the store refuses outright for every state
    /// (`pricing_price_overlay`, D-231): the number a discarded revision consumed is
    /// never freed, so a stale `If-Match` cannot match a re-minted one.
    pub lifecycle_state: String,
    /// `partner` | `org_tier` | `brand` | `region` | `customer_group` | `global`.
    pub scope_class: String,
    /// Taxonomy-validated per class, and the **empty string** exactly when the
    /// class is `global`. See the module doc.
    pub scope_value: String,
    /// L2's explicit integer precedence — unique within one scope class among
    /// **published** revisions (D-107's partial index). It orders **lists**
    /// against each other and never lines inside one.
    pub precedence: i32,
    /// The overlay's **own** interval, not a price-row key axis: an overlay is
    /// not a price row.
    pub effective_from: Option<OffsetDateTime>,
    /// Exclusive end of the overlay's own interval.
    pub effective_to: Option<OffsetDateTime>,
    /// `inclusive` | `exclusive` | `delegated_tariffs`. `NOT NULL` is what L5's
    /// *"silence fails"* means physically.
    pub tax_basis: String,
    /// `restricted` (default) | `public` — consumer-facing exposure of the
    /// overlay, **including its existence** (L6). Fail-closed default.
    pub disclosure: String,
    /// The plan/SKU targets the overlay's lines must fall inside
    /// (`inst-plv-referential`).
    pub target_ref: Json,
    /// The revision's concurrency token. Not in §6; see the module doc.
    pub row_version: i64,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
