//! `SeaORM` entity for `bss.pricing_price_window` — one half-open interval a
//! price row is effective over (`design/07-pricewindow-linkage.md` §6).
//!
//! `effective_to` is `Option` and the absence is a **value**: `NULL` is
//! open-ended, a window that never expires (`inst-ws-expire`), and not a bound
//! nobody got round to setting. Nothing here reconstructs it as "unknown".
//!
//! The three flip timestamps are nullable and their nullability is not
//! independent of `state`: the migration's three biconditional CHECKs make
//! `activated_at` present exactly on an `active` or `expired` window,
//! `expired_at` exactly on an `expired` one, and `cancelled_at` exactly on a
//! `cancelled` one. So `state` is the reading and
//! [`crate::domain::window::WindowState`] is what carries it —
//! `crate::infra::storage::repo::window_repo` reads the token through that
//! type's `ALL` slice rather than reconstructing a lifecycle from which columns
//! happen to be set, for the reason
//! [`super::approval`] gives one table over.
//!
//! **This entity carries no canonical scope key**, and no `plan_id` either. Both
//! live on `pricing_price`, and the window is bound to a row rather than to a
//! key precisely so the two cannot disagree; the repository resolves the key on
//! every read and hands back a `WindowRecord` that carries it, which is what the
//! non-overlap and coverage rules are per.
//!
//! The `resource_col` is `window_id`, so a scope naming one window reaches that
//! row and no other — `pricing_approval`'s shape.


use sea_orm::entity::prelude::*;
use toolkit_db_macros::Scopable;
use uuid::Uuid;
use time::OffsetDateTime;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
#[sea_orm(table_name = "pricing_price_window")]
#[secure(
    tenant_col = "tenant_id",
    resource_col = "window_id",
    no_owner,
    no_type
)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub window_id: Uuid,
    pub tenant_id: Uuid,
    /// The price row this interval belongs to, and thereby the canonical scope
    /// key it is filed under. Immutable after creation (§6), which the
    /// migration's frozen-column whitelist holds.
    pub price_id: Uuid,
    /// Inclusive start of the half-open interval, UTC.
    pub effective_from: OffsetDateTime,
    /// **Exclusive** end, UTC. `None` is open-ended. Exclusive is what makes
    /// `effective_to = next.effective_from` adjacency rather than an overlap and
    /// rather than a gap (§9).
    pub effective_to: Option<OffsetDateTime>,
    /// `scheduled` | `active` | `expired` | `cancelled` (§4's machine).
    pub state: String,
    /// The operator-supplied change reason carried for the audit trail.
    pub reason_code: String,
    /// **Pseudonymous** principal id of whoever scheduled the window.
    pub created_by: Uuid,
    pub created_at: OffsetDateTime,
    /// Set exactly on an `active` or `expired` window — an expired one **was**
    /// active, so it keeps the instant it became so.
    pub activated_at: Option<OffsetDateTime>,
    /// Set exactly on an `expired` window.
    pub expired_at: Option<OffsetDateTime>,
    /// Set exactly on a `cancelled` window, which — since only a `scheduled`
    /// window cancels — never carries an `activated_at`.
    pub cancelled_at: Option<OffsetDateTime>,
    /// How many **operator acts** this window has been the subject of: `0` at its
    /// own schedule, `+1` per adjustment and per cancellation, and **unmoved by the
    /// activation and expiry sweeps** (D-190, `pricing_price_window`).
    ///
    /// Signed because the column is, and every backend's integer column is; the
    /// repository converts it to a `u64` at the boundary and answers `CorruptRow` on
    /// a negative, which is where the bound this table carries no CHECK for is
    /// actually enforced.
    pub mutation_seq: i64,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
