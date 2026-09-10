//! The plan aggregate as the domain reasons about it: **one revision of it**,
//! and the fields an open draft may still move.
//!
//! A plan is not a row. It is a chain of revisions keyed `(plan_id, revision)`
//! (`design/01-foundation.md` §3.7 and §4.3, D-56) of which at most one is the
//! **current** one — `published` *or* `retired`, widened by D-128 — and at most
//! one is an open `draft`. The type below is named for the revision rather than
//! for the plan on purpose: a `Plan` type would invite exactly the read D-56
//! removed, "the plan's billing cycle", as though one answer existed
//! independently of which revision is being asked about. Every value here
//! answers *for its revision*.
//!
//! Content freezes at `published` (§4.3): a shape change opens a **new**
//! revision row in `draft`, which publishes through the standard §4.2 path and
//! flips its predecessor `superseded` in the same commit (D-90). That is why
//! [`PlanShapePatch`] is the mutable surface of an *open draft* and not of a
//! plan — there is no edit of a published revision for it to describe.
//!
//! A draft that is not wanted is **abandoned**, never deleted: the row survives
//! as a terminal tombstone so the `revision` number it consumed stays consumed
//! (D-145). What that costs is a gap in the numbering; what it buys is that
//! `(plan_id, revision)` never names two different rows.
//!
//! The child shape tables — phases, add-on rules, descriptor set — version with
//! the revision and are copied on a new one (D-83); they are Slice-2 storage and
//! are not modelled here yet.

use toolkit_macros::domain_model;
use uuid::Uuid;

use crate::domain::concurrency::RowVersion;
use crate::domain::contracts::{EntitlementGrants, PlanChangeContract};
use crate::domain::lifecycle::LifecycleState;
use crate::domain::plan_shape::{BillingCycle, Frequency};
use crate::domain::scope_key::PlanId;
use time::OffsetDateTime;

/// One revision of a plan: the unit `pricing_plan` stores, the unit the
/// projector sources a plan subject from, and the unit an `ETag` denotes.
#[domain_model]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanRevision {
    /// The plan this revision belongs to.
    ///
    /// Stable across the whole chain: the plan's identity, the canonical
    /// scope-key axis and the `pricing_price` attachment all stay on `plan_id`
    /// when a new revision opens (§4.3), so a reprice never has to chase a
    /// moved parent.
    pub plan_id: PlanId,
    /// The revision number, minted `max(revision) + 1` from `0` within the plan.
    ///
    /// Together with [`PlanRevision::plan_id`] it is this row's identity, and it
    /// is an **identity rather than a counter** (D-145): a number minted for a
    /// plan is never minted again, so the sequence may have **gaps** where a
    /// draft was discarded — rev 1 published, rev 2 abandoned, rev 3 published.
    /// A reader that treats the numbers as consecutive is reading a display
    /// convention that was never promised; what is promised is that
    /// `(plan_id, revision)` denotes one row for the life of the plan, which is
    /// what makes it safe as the durable name the grant table, the child copies
    /// and the audit trail all dereference.
    ///
    /// It is **not** how "the current revision" is decided — that is the partial
    /// `UNIQUE` index over `published`/`retired` (D-128), storage-defined
    /// precisely so it is never a max-scan convention that two readers could
    /// implement differently.
    pub revision: u64,
    /// The catalog SKU this plan realizes, when one is bound.
    pub sku_id: Option<Uuid>,
    /// The plan's tier.
    ///
    /// A `String` on purpose, and it stays one now that Slice 2 has landed
    /// beside it: the `PlanTier` taxonomy is supplied by the **product/SKU
    /// registry** (§1.3), not enumerated by this gear. An enum minted here
    /// would fix a value set somebody else owns, and the first tier the
    /// registry published that it disagreed with would be a migration rather
    /// than a fix. Slice 2 requires a tier (`PLANTIER_MISSING`) and checks it
    /// against the parent SKU's; neither of those is a claim about which tiers
    /// exist.
    pub plan_tier: Option<String>,
    /// The plan's human label (D-318), or `None` when it has never been named.
    ///
    /// Free text an operator chose. Distinct from [`PlanRevision::plan_tier`],
    /// which is a **classification** the catalog reasons about — a tier is
    /// compared, overridden and inherited from the SKU, and a name is none of
    /// those things. Every surface showed the tier only because there was
    /// nothing else to show.
    pub plan_name: Option<String>,
    /// The plan's billing cycle.
    ///
    /// Typed, unlike [`PlanRevision::plan_tier`], and for the reason that field
    /// is not: **Slice 2 owns this value set**. `billing_cycle` was a `String`
    /// only while the rules that constrain it did not exist; they do now
    /// ([`crate::domain::plan_shape::BillingCycle`], §17.1), so an enum here
    /// fixes nothing prematurely — it names what the slice already fixed. The
    /// tier's taxonomy is still **registry-owned**, which is why it stays a
    /// `String` and the note on it still holds.
    ///
    /// `None` is an authored-but-unfinished draft, never a default: nothing in
    /// the matrix is implied.
    pub billing_cycle: Option<BillingCycle>,
    /// The recurring frequency, with a custom interval riding the variant.
    ///
    /// One field, three columns. `frequency`, `custom_interval_n` and
    /// `custom_interval_unit` can express a `monthly` row carrying an interval
    /// and a custom row carrying none; [`Frequency`] can express neither, so the
    /// repository boundary is the only place either can appear and it refuses
    /// both as corrupt rows.
    pub frequency: Option<Frequency>,
    /// Whether the tier deliberately diverges from the parent SKU's under an
    /// explicit audited override (§6, P3).
    ///
    /// Not an `Option`: the column is `NOT NULL DEFAULT false` because "nobody
    /// said" and "no override" are the same claim about a plan, and a third
    /// state would make the audited exception depend on which of two absences a
    /// reader met.
    pub plan_tier_override: bool,
    /// Minimum purchasable quantity (one-time plans).
    pub purchase_min_qty: Option<u64>,
    /// Maximum purchasable quantity (one-time plans).
    pub purchase_max_qty: Option<u64>,
    /// The Billing invoice-layout hint (D-96). `None` or empty means no
    /// grouping; it never overrides the single-currency-per-invoice invariant.
    pub invoice_grouping_key: Option<String>,
    /// Start of the plan's availability window, UTC.
    pub available_from: Option<OffsetDateTime>,
    /// End of the plan's availability window, UTC.
    pub available_to: Option<OffsetDateTime>,
    /// The entitlement grant set this revision publishes (Slice 6, §6, D-41).
    ///
    /// Revision-scoped like every other plan column (D-83).
    pub entitlement_grants: EntitlementGrants,
    /// The plan-change contract this revision publishes (Slice 6, §6).
    ///
    /// Revision-scoped like every other plan column (D-83): an edge list is
    /// authored content, a change to it is a plan mutation, and Slice 5's
    /// materiality applies to it (`inst-pc-governed`).
    pub change_contract: PlanChangeContract,
    /// Where this revision stands.
    ///
    /// `draft` is the only state whose content may still change
    /// ([`LifecycleState::is_content_mutable`]), and `published`/`retired` are
    /// the two the plan may be *current* in
    /// ([`LifecycleState::is_current_revision`]). Both questions are answered by
    /// the state machine rather than re-spelled at each call site.
    pub lifecycle_state: LifecycleState,
    /// Pseudonymous principal id of the authoring actor.
    ///
    /// Carried on the revision itself so the Slice-12 history surface can read
    /// actor identity under `plan x read`, without the Auditor-only
    /// `pricing_audit_log`.
    pub created_by: Uuid,
    /// When this revision row was created, UTC.
    pub created_at_utc: OffsetDateTime,
    /// The plan this one was cloned from (`inst-cl-copy`, D-19), or `None` for
    /// an authored plan.
    ///
    /// **Provenance, not authored content.** It sits with `created_by` rather
    /// than with the shape: a clone is an *ordinary* draft (`inst-cl-draft`),
    /// taking the full pipeline and an approval on its first publish exactly as
    /// any other first publish does, so no rule reads this and the content pin
    /// does not frame it. It carries forward to later revisions of the same plan,
    /// because lineage is the plan's and not one revision's.
    pub cloned_from: Option<PlanId>,
    /// The optimistic-concurrency version this revision is at.
    ///
    /// It moves only on the draft plane: an edit advances it, and so does the
    /// abandon that ends the draft's life — the last tag the row will ever
    /// carry. A published revision's content is frozen, and a tag that moved
    /// under frozen content would tell a caller its cached copy is stale when it
    /// is not.
    pub row_version: RowVersion,
}

/// The fields an **open draft** revision may still change.
///
/// Every field carries one meaning: `Some(v)` sets the column to `v`, and
/// `None` means **leave it alone**. An entirely absent patch is still a valid
/// request — it asserts the caller's `ETag` and advances it, which is how a
/// no-op edit stays distinguishable from a lost one.
///
/// It is deliberately **not** `Option<Option<T>>`. The double option is the
/// usual way to make "set this nullable column back to NULL" expressible, and
/// until the REST layer landed no surface could express it at all: the request
/// shape that distinguishes an omitted JSON member from an explicit `null` is a
/// transport shape. **The surface exists now** (`api::rest::plans`,
/// `PATCH /bss-pricing/v1/plans/{planId}`) **and the limitation does not move
/// with it**, because paying it is a change to *this type* and to
/// `plan_repo::patched_columns` rather than to the surface: every field would
/// gain a third state that every repository method, every rule and every test
/// has to reason about, and `serde`'s `Option<Option<T>>` needs
/// `#[serde(default, deserialize_with =...)]` per member to distinguish absent
/// from null at all. So this is a **known limitation, stated rather than
/// designed around**, and it is now owed by whichever wave next changes the
/// draft patch shape — not by a surface group. Slice 2 widens what it costs
/// rather than quietly inheriting it: a `sku_id`, `plan_tier`, `billing_cycle`,
/// `frequency`, `purchase_min_qty`, `purchase_max_qty`, `invoice_grouping_key`,
/// `available_from` or `available_to` that has been set cannot be cleared
/// through a patch — only replaced, or discarded by abandoning the draft
/// revision, which keeps the revision number it consumed (D-145).
///
/// Two of the Slice-2 fields fall outside that sentence, and both for reasons
/// of their own rather than by exception:
///
/// * [`PlanShapePatch::plan_tier_override`] is an `Option<bool>` over a
///   `NOT NULL` column, so `Some(false)` really does withdraw the override —
///   there is no null state to be unable to reach.
/// * [`PlanShapePatch::frequency`] moves **three** columns as one value.
///   Setting a fixed frequency clears `custom_interval_n` and
///   `custom_interval_unit` with it, because the interval is part of the
///   variant and not an independently patchable column: a patch that moved only
///   the token would leave a `monthly` row wearing a custom interval, which is
///   the pairing both [`Frequency`] and `chk_pricing_plan_custom_interval_pairing`
///   exist to make unreachable.
#[domain_model]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PlanShapePatch {
    /// Bind the plan to a different catalog SKU.
    pub sku_id: Option<Uuid>,
    /// Move the plan's tier.
    pub plan_tier: Option<String>,
    /// Move the plan's human label (D-318).
    ///
    /// Like every other member here, `None` means "leave it alone" and not
    /// "clear it" — a plan is unnamed back by sending the empty string, which
    /// the write stage refuses, so **there is no way to un-name a named plan
    /// through this patch**. Deliberate: the two-spellings hazard is worse than
    /// the missing verb, and a plan that has been shown to an operator under a
    /// name is not improved by losing it.
    pub plan_name: Option<String>,
    /// Move the plan's billing cycle.
    pub billing_cycle: Option<BillingCycle>,
    /// Move the recurring frequency, interval and all; see the type doc.
    pub frequency: Option<Frequency>,
    /// Declare or withdraw the audited tier override (P3).
    pub plan_tier_override: Option<bool>,
    /// Move the minimum purchasable quantity.
    pub purchase_min_qty: Option<u64>,
    /// Move the maximum purchasable quantity.
    pub purchase_max_qty: Option<u64>,
    /// Move the Billing invoice-layout hint (D-96).
    pub invoice_grouping_key: Option<String>,
    /// Move the start of the availability window, UTC.
    pub available_from: Option<OffsetDateTime>,
    /// Move the end of the availability window, UTC.
    pub available_to: Option<OffsetDateTime>,
    /// Replace the entitlement grant set wholesale (Slice 6, §6, D-41).
    ///
    /// Wholesale for [`PlanShapePatch::change_contract`]'s reason: the
    /// plan-level set, the `PlanTier` reference and the per-phase map are one
    /// authored fact, and a per-member encoding could express a per-phase entry
    /// with no plan-level set to fall back to.
    pub entitlement_grants: Option<EntitlementGrants>,
    /// Replace the plan-change contract wholesale (Slice 6, §6).
    ///
    /// **Wholesale, not per member**, and this is the one field of the patch
    /// that is not independent of its neighbours — the reason `PriceContent` is
    /// not a patch at all. K4 ties `comparability_rank` to whether
    /// `allowed_change_targets` names anyone, so a per-member encoding could
    /// express "drop the rank, keep the edges", which is a state no publish
    /// accepts and which the caller cannot have meant. Submitting the contract
    /// it wants makes every intermediate state unrepresentable.
    ///
    /// The double `Option` a "clear this" would need is the same G3 non-goal
    /// this type's own doc records: to leave self-service change, send a
    /// contract whose `allowed_change_targets` is `None`.
    pub change_contract: Option<PlanChangeContract>,
}

#[cfg(test)]
#[path = "plan_tests.rs"]
mod plan_tests;
