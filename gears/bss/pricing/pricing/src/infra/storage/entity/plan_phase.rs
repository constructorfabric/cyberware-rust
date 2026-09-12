//! `SeaORM` entity for `bss.pricing_plan_phase` — one phase of **one plan
//! revision** (`design/02-plan-definition.md` §6), keyed
//! `(tenant_id, plan_id, plan_revision, phase_id)`.
//!
//! The tuple is not a composite id: `plan_revision` says *which copy* of the
//! phase this row is, and `phase_id` is the phase itself, stable for the life
//! of the plan. A new revision copies these rows under its own number without
//! re-minting an id (D-83), because the `phase` axis of the canonical scope key
//! holds a bare `phase_id` (D-19) and same-key supersession compares it (D-56).
//!
//! **`tenant_id` and `plan_id` are part of the key** (D-340). Without them it says
//! that one phase id belongs to one plan per revision *number* across the whole
//! table, every tenant's included — so five drafts sharing an id leave four members
//! that can never attach it, unrecoverably, a scope key being a price row's
//! identity. What the widening leaves standing is the half the phase-graph rules
//! rely on: one revision may not hold the same phase id twice.
//!
//! The four attributes are in field order rather than in the physical key's order,
//! and that is not a divergence anything can observe: `SeaORM` names columns in
//! every statement it builds, and the one construct where key order is a signature
//! — `Entity::find_by_id`'s tuple — has no call site in this gear.
//!
//! There is no `lifecycle_state` here. A phase row is frozen when **its**
//! revision publishes, so the parent `pricing_plan` row is the referent and the
//! table's append-only triggers read it — the same arrangement
//! `pricing_price_tier_band` has with its price row.

use sea_orm::entity::prelude::*;
use toolkit_db_macros::Scopable;
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
#[sea_orm(table_name = "pricing_plan_phase")]
#[secure(tenant_col = "tenant_id", resource_col = "phase_id", no_owner, no_type)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub phase_id: Uuid,
    /// The revision this copy belongs to — and the row it is frozen with.
    #[sea_orm(primary_key, auto_increment = false)]
    pub plan_revision: i64,
    /// Copied from the parent revision by the repository, never taken from a
    /// request: the foreign key covers `(plan_id, plan_revision)` alone, so a row
    /// claiming a tenant its parent revision does not belong to is refused by the
    /// append-only trigger's parent-tenant arm rather than by the key.
    ///
    /// In the key since D-340, which is what makes a phase id private to its
    /// tenant rather than a name in a deployment-wide namespace.
    #[sea_orm(primary_key, auto_increment = false)]
    pub tenant_id: Uuid,
    /// In the key since D-340: a phase id belongs to a **plan**, so two plans of
    /// one tenant may hold the same one — which D-19's clone remap and D-83's
    /// copy-forward both need, since both supply an id the server did not mint.
    #[sea_orm(primary_key, auto_increment = false)]
    pub plan_id: Uuid,
    /// `trial` | `interim` | `evergreen`, `CHECK`-constrained to those three; the
    /// value set is `domain::plan_shape::PhaseKind`. It is **not** terminality
    /// (C-4) — see [`Model::converts_to_phase_id`]. `interim` was spelled `intro`
    /// until D-358; frozen snapshots taken before it still carry the old token.
    pub kind: String,
    /// The operator's label for the phase (D-357): optional, free-form, no
    /// uniqueness, no behaviour. Versions with the revision like every column
    /// here.
    pub display_name: Option<String>,
    /// The phase's position in the chain. The lowest ordinal is the entry
    /// phase (`inst-ph-graph`).
    pub ordinal: i32,
    /// Where this phase converts to. NULL **is** terminality, which is why the
    /// partial `UNIQUE` that admits one terminal phase per revision is written
    /// over this column's nullity rather than over [`Model::kind`].
    pub converts_to_phase_id: Option<Uuid>,
    /// How long the phase lasts. Required `> 0` on a non-terminal phase and
    /// forbidden on the terminal one — a pipeline rule
    /// (`inst-ph-duration`, `PHASE_DURATION_INVALID`), never a CHECK here.
    pub phase_duration_days: Option<i32>,
    /// The PRD-named projection of [`Model::phase_duration_days`] on a `trial`
    /// phase (`inst-ph-trial`). `chk_pricing_plan_phase_display_trial_days`
    /// guards drift between the two persisted columns.
    pub display_trial_days: Option<i32>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
