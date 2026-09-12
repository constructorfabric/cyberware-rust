//! `SeaORM` entity for `bss.pricing_plan` — one **revision** of a plan
//! (`design/01-foundation.md` §3.7, D-56), keyed `(plan_id, revision)`.
//!
//! At most one revision is the plan's **current** one (`published` or
//! `retired`, D-128) and at most one is an open `draft`; both are partial
//! `UNIQUE` indexes on the table, and a discarded draft's `abandoned` tombstone
//! is outside both, so a plan may hold any number of them (D-145). A published
//! revision's content is frozen — the only permitted UPDATE is the sanctioned
//! `lifecycle_state` flip — so `row_version` (the `ETag`) is frozen with it:
//! content that cannot change needs no new entity tag.


use sea_orm::JsonValue;
use sea_orm::entity::prelude::*;
use time::OffsetDateTime;
use toolkit_db_macros::Scopable;
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
#[sea_orm(table_name = "pricing_plan")]
#[secure(tenant_col = "tenant_id", resource_col = "plan_id", no_owner, no_type)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub plan_id: Uuid,
    #[sea_orm(primary_key, auto_increment = false)]
    pub revision: i64,
    pub tenant_id: Uuid,
    pub sku_id: Option<Uuid>,
    pub plan_tier: Option<String>,
    /// The plan's human label (D-318). `NULL` is "not named", and the empty
    /// string is refused at the write stage rather than stored beside it.
    pub plan_name: Option<String>,
    /// `one_time` | `recurring` | `usage` | `hybrid`, `CHECK`-constrained to
    /// those four; the value set is `domain::plan_shape::BillingCycle`.
    pub billing_cycle: Option<String>,
    /// `monthly` | `quarterly` | `semiannual` | `annual` | `custom_every_n`.
    ///
    /// The custom token is the bare discriminator: its interval rides the two
    /// columns below, and `chk_pricing_plan_custom_interval_pairing` binds the
    /// three together so the only pairings the table admits are the ones
    /// `domain::plan_shape::Frequency` can represent.
    pub frequency: Option<String>,
    /// The interval count of a `custom_every_n` frequency, `> 0`.
    pub custom_interval_n: Option<i32>,
    /// `days` | `months` — what [`Model::custom_interval_n`] counts.
    pub custom_interval_unit: Option<String>,
    /// Whether the tier deliberately diverges from the parent SKU's under an
    /// explicit audited override (§6, P3). `NOT NULL DEFAULT false`, so a plan
    /// that never mentions it is not overriding anything.
    pub plan_tier_override: bool,
    /// Minimum purchasable quantity (one-time plans).
    pub purchase_min_qty: Option<i64>,
    /// Maximum purchasable quantity (one-time plans), `>=`
    /// [`Model::purchase_min_qty`] where both are set.
    pub purchase_max_qty: Option<i64>,
    /// The Billing invoice-layout hint (D-96). NULL or empty means no grouping.
    pub invoice_grouping_key: Option<String>,
    /// The §17.6 grant set as authored (D-41, `pricing_plan`): the
    /// plan-level flags and quotas, the `PlanTier` it resolved from when it did,
    /// and any per-phase sets. **Not** `pricing_plan_grant`, which is Slice 10's
    /// table for D-43's prepaid credit grant; see the migration's own doc.
    pub entitlement_grants: Option<JsonValue>,
    // --- plan-change contract, Slice 6 (`pricing_plan`) ---
    /// A JSON array of explicit published `planId`s. NULL is the fail-safe and
    /// means **no self-service change** (`inst-pc-failsafe`) -- never any-to-any
    /// and never "unknown". `jsonb` on Postgres, `text` on `SQLite`, which is
    /// `included_allowance`'s convention.
    pub allowed_change_targets: Option<JsonValue>,
    /// The tenant-wide comparability scale (K4). Higher is an upgrade, lower a
    /// downgrade, equal a switch.
    pub comparability_rank: Option<i32>,
    /// `reset` | `carry` -- D-113's tier-`Q` continuity flag, read by Rating off
    /// the **target** plan's frozen snapshot. NULL reads as `reset`, which is
    /// the ratified default and the safe direction.
    pub usage_counter_on_plan_change: Option<String>,
    /// `draft` | `abandoned` | `published` | `superseded` | `retired`.
    /// `CHECK`-constrained to those five; the legal edges between them are
    /// `domain::lifecycle::LifecycleState`.
    pub lifecycle_state: String,
    pub available_from: Option<OffsetDateTime>,
    pub available_to: Option<OffsetDateTime>,
    /// Pseudonymous principal id of the authoring actor. The Slice-12 history
    /// surface reads it under `plan x read`, so actor identity never requires
    /// the Auditor-only `pricing_audit_log`.
    pub created_by: Uuid,
    pub created_at_utc: OffsetDateTime,
    /// The plan this one was cloned from (`inst-cl-copy`, D-19), or `None` for
    /// an authored plan. Lineage only: a clone is an ordinary draft and nothing
    /// reads this to decide behaviour.
    pub cloned_from: Option<Uuid>,
    /// The `ETag` / optimistic-concurrency row version.
    ///
    /// Attached to this field and not to [`Model::cloned_from`] above it: a field
    /// inserted directly above an undocumented one takes the following doc line with
    /// it, leaving the clone-lineage column's rendered doc opening by calling it the
    /// entity tag while the column an `If-Match` precondition answers against carries
    /// nothing at all.
    pub row_version: i64,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
