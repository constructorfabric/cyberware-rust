//! `SeaORM` entity for `bss.pricing_price` — the price rows and, in the same
//! table, the price **history** (`design/01-foundation.md` §3.7).
//!
//! **Eight** of the ten canonical scope-key columns come first and in normative
//! order (§4.1); the usage pair D-196 added — `meter` and `dimension_key` — sits
//! in the content block below, where the axes it also serves as content put it.
//! This is a claim about *this table's layout*, not about the axis count (D-289).
//!
//! `cohort` is a `String` holding the domain token (`none`, or the cutover
//! instant) rather than an `Option<OffsetDateTime>`, because it is a column of a
//! partial `UNIQUE` index and distinct `NULL`s do not collide.
//!
//! Money is integer minor units. `amount_minor` is nullable because its
//! placement is per-kind (Slice 3): required on `flat` / `per_unit`, and NULL on
//! the band and package kinds whose money lives elsewhere — so no row ever
//! carries two competing prices. "Elsewhere" is `package_price_minor` on this
//! very row for a `package`, and [`super::price_tier_band`] for
//! `graduated` / `volume`: the per-row Slice-3 columns sit here, and only the
//! band set, being many-per-row, needed a table.
//!
//! `included_allowance` is Slice-10-declared and rides along as opaque JSON.
//! This entity persists the authored declaration and compiles nothing from it.
//!
//! A published row's content is frozen — its only permitted UPDATEs are the
//! sanctioned `lifecycle_state` flip and a monotonic tightening of
//! `grandfather_until` — so `row_version` (the `ETag`) is frozen with it:
//! content that cannot change needs no new entity tag. The migration's module
//! doc records why the column exists at all, which Foundation §3.7 omits to say.


use time::OffsetDateTime;
use sea_orm::entity::prelude::*;
use serde_json::Value as JsonValue;
use toolkit_db_macros::Scopable;
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
#[sea_orm(table_name = "pricing_price")]
#[secure(tenant_col = "tenant_id", resource_col = "price_id", no_owner, no_type)]
#[allow(
    clippy::struct_field_names,
    reason = "`model_kind` is the normative column name (design/03-price-structure.md 6); \
              the struct is called `Model` only because SeaORM's DeriveEntityModel requires it"
)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub price_id: Uuid,
    pub tenant_id: Uuid,
    // --- the canonical scope key, normative order (design 4.1) ---
    pub plan_id: Uuid,
    pub currency: String,
    pub region: String,
    /// Always `base` on a row this gear authors; partner / orgTier / brand
    /// overlays are separate overlay documents, not a value of this axis.
    pub price_overlay: String,
    pub phase: Uuid,
    pub price_eligibility: String,
    pub charge_kind: String,
    /// `none`, or the UTC cutover instant that created the grandfathering
    /// generation, rendered by `domain::scope_key::Cohort`.
    pub cohort: String,
    // --- price / model ---
    pub amount_minor: Option<i64>,
    /// The `per_unit` **rate**, in 10^-9 minor units (D-311).
    ///
    /// Its own column rather than a second meaning for `amount_minor`, which
    /// documented itself as "the single amount on `flat`, the unit price on
    /// `per_unit`" — one column standing for an invoice sum and for a multiplier,
    /// which is the defect D-311 names one level down.
    pub unit_rate_nano: Option<i64>,
    pub model_kind: Option<String>,
    pub tax_inclusive: bool,
    /// The row's tax category (D-110, `pricing_price`).
    ///
    /// **The source of truth and the only place a category lives** — §6 removes
    /// the per-plan descriptor-set column that claimed to mirror it. `None` is
    /// *the row states none*, which D-154 resolves against the region taxonomy's
    /// default at publish; it is not "no category", and the two are what
    /// `inst-td-policy`'s coalesce is about.
    pub tax_category_ref: Option<String>,
    /// D-154's **resolved effective** category, frozen at the publish commit
    /// (`pricing_price`).
    ///
    /// `coalesce(tax_category_ref, readiness.taxCategory)` as it stood *when the
    /// rule set judged this row*, not as the region taxonomy stands when a
    /// consumer asks. NULL on a draft row — there is nothing to freeze until a
    /// publish resolves it.
    pub resolved_tax_category: Option<String>,
    /// The rounding policy this row's publish **resolved**, frozen at the publish
    /// commit (`pricing_price`) and kept apart from the authored
    /// [`rounding_policy_ref`](Self::rounding_policy_ref).
    ///
    /// `coalesce(rounding_policy_ref, tenant.default_rounding_policy_ref)` as it
    /// stood when the rule set judged this row — in this column, never over the
    /// authored one. A publish that wrote it over the author's makes a row that
    /// deliberately carries no policy of its own, leaning on a live tenant setting,
    /// come back indelibly naming one, and carries that value into every successor and
    /// copy draft as if a person had typed it.
    ///
    /// NULL on a draft row, exactly like its sibling above: there is nothing to
    /// freeze until a publish resolves it.
    pub resolved_rounding_policy: Option<String>,
    pub billing_timing: Option<String>,
    // --- proration input contract, Slice 6 (`pricing_price`) ---
    /// `calendar_month` | `subscription_start` | `fixed_day` (K2). The token
    /// never carries the day; [`Model::anchor_day`] does.
    pub billing_anchor_policy: Option<String>,
    /// The day a `fixed_day` policy anchors on, 1–31. NULL under every other
    /// policy — the pairing is held by the domain enum, which carries the day
    /// inside the variant, so no state reaching these two columns can be
    /// mismatched.
    pub anchor_day: Option<i32>,
    /// The canonical K1 basis, owned by this gear and adopted verbatim by
    /// Tariffs.
    pub proration_basis: Option<String>,
    /// Catalog-sanctioned downgrade credit eligibility. On a plan change the
    /// governing value is the **source** row's, read from the subscriber's
    /// frozen snapshot (`inst-pi-credit-source`).
    pub credit_on_downgrade: Option<bool>,
    /// `subscription_seat_count` | `manual` — where a **non-usage** `per_unit`
    /// row gets its quantity. Usage rows never carry it: the meter supplies `Q`,
    /// and an authored quantity beside a metered one would be two answers to
    /// "how much was consumed".
    pub quantity_source: Option<String>,
    /// The fixed quantity a `manual` [`Model::quantity_source`] promises.
    pub manual_quantity: Option<i64>,
    /// Units per block; `package` only, `> 0`. Supersession-preserved (D-122):
    /// block math is non-linear in the window, so a mid-window change
    /// re-buckets an already-accumulated counter.
    pub package_size: Option<i64>,
    /// Price per block; `package` only. The legitimate price lever on a
    /// `package` supersession, unlike [`Model::package_size`].
    pub package_price_minor: Option<i64>,
    // --- evaluation policy (usage rows) ---
    pub meter: Option<String>,
    /// Dimension discriminator. `NOT NULL DEFAULT ''` — the empty string is the
    /// empty-tuple sentinel, so the Slice-2 injectivity index collides
    /// undimensioned rows instead of treating them as distinct `NULL`s.
    pub dimension_key: String,
    pub billing_granularity: Option<String>,
    pub aggregation_function: Option<String>,
    pub aggregation_granularity: Option<String>,
    pub tier_aggregation_window: Option<String>,
    pub tier_qualification_window: Option<String>,
    /// How many granules a level fold may carry a stale reading across. A
    /// `bigint` like every other count on the row: a granule count is a duration
    /// in the row's own granularity, and at `per_second` an ordinary
    /// multi-year hold does not fit an `integer`.
    pub max_hold_granules: Option<i64>,
    /// The **authored** D-45 declaration `{quantity, rolloverPolicy}`, stored as
    /// written. Slice-10-declared and Slice-10-compiled; this gear persists and
    /// returns it and nothing more, because the D-129 supersession guard reads
    /// it and a round trip that dropped it would report "nothing changed".
    pub included_allowance: Option<JsonValue>,
    /// The reserved rate, in the row's currency (`inst-rv-attrs`). A `bigint`
    /// like every other money column on this row.
    pub reserved_rate_nano: Option<i64>,
    /// `consumption | capacity`; present iff `reserved_rate_nano` is. No column
    /// `CHECK` on either engine -- see `pricing_price` for why the vocabulary
    /// lives in the domain enum and the pairing in a publish rule.
    pub reservation_flavor: Option<String>,
    /// The order-time purchase floor (`inst-ft-typed`); Subscriptions enforces.
    pub min_qty_purchase: Option<i64>,
    /// The eligibility usage floor (`inst-ft-typed`); Tariffs/Rating enforces.
    pub min_qty_usage: Option<i64>,
    /// What happens beneath `min_qty_usage`; REQUIRED with it at publish
    /// (`inst-ft-fallback`). Launch vocabulary is one value, `exception`.
    pub min_qty_usage_fallback: Option<String>,
    /// The external discount instrument (`inst-dr-referential`). Opaque here:
    /// Promotions owns it and this gear only checks that it resolves.
    pub discount_ref: Option<String>,
    /// The named rounding-policy id resolved at publish (row-level, else the
    /// tenant default, else `ROUNDING_POLICY_UNRESOLVED`).
    pub rounding_policy_ref: Option<String>,
    /// Grandfathering horizon. Monotonically tightenable only; the table
    /// trigger rejects loosening it.
    pub grandfather_until: Option<OffsetDateTime>,
    /// Set on a successor row; it is what gives the supersession unit guard its
    /// comparison referent (D-127).
    pub supersedes_price_id: Option<Uuid>,
    pub lifecycle_state: String,
    /// Pseudonymous authoring principal (the Slice-12 history-export actor).
    pub created_by: Uuid,
    pub created_at_utc: OffsetDateTime,
    /// The `ETag` / optimistic-concurrency row version. The storage spelling of
    /// [`crate::domain::concurrency::RowVersion`], which is the one place that
    /// renders it as an entity tag and parses one back.
    pub row_version: i64,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
