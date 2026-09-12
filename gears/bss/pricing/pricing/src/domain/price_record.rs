//! A price row as the store holds it: the authored shape, the canonical scope
//! key it is filed under, and the metadata that is neither.
//!
//! [`PriceRow`] is the **shape** the thirteen Slice-3 rules judge, and it
//! deliberately carries no identity and no key — a rule able to see which row it
//! was looking at would be free to reach a different verdict for two rows that
//! are the same shape. [`ScopeKey`] carries the ten axes. Neither is enough on
//! its own for a caller that has to name a row, tag it, or decide whether it may
//! still be edited, so this module composes them with exactly the columns that
//! answer those three questions.
//!
//! `billing_timing` stays a `String`. `02-plan-definition.md`
//! `inst-cs-recurring` says it in as many words — the requirement is **Slice
//! 6's registered rule**, cross-referenced elsewhere and never re-registered —
//! so an enum minted here would be a second registration of a rule this slice
//! does not own, free to disagree with the one that does.

use toolkit_macros::domain_model;
use uuid::Uuid;

use crate::domain::concurrency::RowVersion;
use crate::domain::contracts::ProrationContract;
use crate::domain::lifecycle::LifecycleState;
use crate::domain::price_row::PriceRow;
use crate::domain::scope_key::{DimensionKey, Meter, ScopeKey};
use time::OffsetDateTime;

/// Everything about a price row that an open draft may still change.
///
/// It is deliberately **not** a patch. [`crate::domain::plan::PlanShapePatch`]
/// can be one because its five columns are independent of each other; a price
/// row's are not. Moving `model_kind` from `graduated` to `flat` has to drop
/// the band set and set `amount_minor` in the *same* write, because every
/// intermediate state is one no Slice-3 rule can pass — and a per-field
/// `Some`/`None` encoding cannot say "clear this" at all without the double
/// option `PlanShapePatch` records as a G3 non-goal. So a draft edit submits
/// the content it wants the row to have and the store replaces it wholesale.
///
/// That is also the only encoding under which the band set stays a **set**:
/// bands have no identity a caller could address one at a time, and a partial
/// band update has no geometry for the rules to evaluate.
#[domain_model]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PriceContent {
    /// The authored Slice-3 shape, band set included.
    pub row: PriceRow,
    /// Whether the authored amounts are tax-inclusive.
    pub tax_inclusive: bool,
    /// The row's tax category (D-110): the **source of truth**, and the only
    /// place a category lives. `None` states none, which D-154 resolves against
    /// the region taxonomy's default at publish.
    pub tax_category_ref: Option<String>,
    /// `advance` | `arrears`, Slice-6-owned; see the module doc.
    pub billing_timing: Option<String>,
    /// The three proration inputs Subscriptions computes from, when the row has
    /// authored them (`inst-pi-required`).
    ///
    /// Unlike `billing_timing` this **is** typed here, and the difference is not
    /// an inconsistency: the timing's rule is registered by Slice 6 and merely
    /// *named* by Slice 2, so a second enum would be a second registration —
    /// while the proration vocabulary has exactly one owner (K1 says the enum is
    /// owned by Slice 6 and adopted verbatim downstream) and nothing else in the
    /// crate can spell it. See [`crate::domain::contracts`].
    pub proration_contract: Option<ProrationContract>,
    /// The named rounding policy this row resolves against, when one is set on
    /// the row rather than inherited from the tenant default.
    pub rounding_policy_ref: Option<String>,
    /// The grandfathering horizon. Only an `existing_grandfathered` row may
    /// carry one, and once published it may be tightened but never loosened.
    pub grandfather_until: Option<OffsetDateTime>,
    /// The predecessor this row supersedes on its canonical scope key.
    ///
    /// Set by the two sanctioned producers of `published -> superseded` — the
    /// D-88 supersession unit and the D-100 cutover commit — and it is what
    /// gives the D-127 unit guard its comparison referent. Neither of those
    /// paths exists yet, so in G3 this column is carried and returned and
    /// nothing reads it.
    pub supersedes_price_id: Option<Uuid>,
}

/// One price row, whole: what it is, where it is filed, and what state it is
/// in.
///
/// The fields are flat rather than nesting a [`PriceContent`] because reading a
/// row is the common case and `record.content.row.model_kind` would make every
/// reader pay for a distinction only a writer needs.
/// [`PriceRecord::content`] is the bridge between the two.
#[domain_model]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PriceRecord {
    /// The row's identity. Caller-supplied at creation, for the reason
    /// `NewPlanDraft` gives: an authoring surface has to be able to return the
    /// id before the row is durable, and a store that minted ids would make an
    /// idempotent retry create a second row.
    pub price_id: Uuid,
    /// The ten axes this row is the (at most one) current row on — D-196's usage pair
    /// included, and `uq_pricing_price_scope_key_current` and
    /// `uq_pricing_price_scope_key_draft` carry all ten. The count matters: a rule
    /// built from eight refuses work the store admits (D-283).
    pub scope_key: ScopeKey,
    /// The authored shape the Slice-3 rules judge.
    pub row: PriceRow,
    /// Whether the authored amounts are tax-inclusive.
    pub tax_inclusive: bool,
    /// The row's tax category (D-110): the **source of truth**, and the only
    /// place a category lives. `None` states none, which D-154 resolves against
    /// the region taxonomy's default at publish.
    pub tax_category_ref: Option<String>,
    /// `advance` | `arrears`, Slice-6-owned; see the module doc.
    pub billing_timing: Option<String>,
    /// The three proration inputs Subscriptions computes from, when the row has
    /// authored them (`inst-pi-required`).
    ///
    /// Unlike `billing_timing` this **is** typed here, and the difference is not
    /// an inconsistency: the timing's rule is registered by Slice 6 and merely
    /// *named* by Slice 2, so a second enum would be a second registration —
    /// while the proration vocabulary has exactly one owner (K1 says the enum is
    /// owned by Slice 6 and adopted verbatim downstream) and nothing else in the
    /// crate can spell it. See [`crate::domain::contracts`].
    pub proration_contract: Option<ProrationContract>,
    /// The named rounding policy resolved for this row, when one is set on it.
    pub rounding_policy_ref: Option<String>,
    /// The grandfathering horizon, on a grandfathered row.
    pub grandfather_until: Option<OffsetDateTime>,
    /// The predecessor this row supersedes on its key, when it has one.
    pub supersedes_price_id: Option<Uuid>,
    /// Where the row stands. `draft` is the only state whose content may still
    /// change ([`LifecycleState::is_content_mutable`]).
    pub lifecycle_state: LifecycleState,
    /// Pseudonymous principal id of the authoring actor, carried on the row so
    /// the Slice-12 history surface never needs the Auditor-only audit log.
    pub created_by: Uuid,
    /// When the row was authored, UTC.
    pub created_at_utc: OffsetDateTime,
    /// The optimistic-concurrency version, and therefore the row's `ETag`.
    ///
    /// It covers the **band set as well as the row**: bands carry no entity tag
    /// of their own, so if a band edit left this alone, two authors editing
    /// different bands of one draft would both satisfy `If-Match` and silently
    /// interleave.
    pub row_version: RowVersion,
}

impl PriceRecord {
    /// The mutable half of this record, ready to be edited and submitted back
    /// under [`PriceRecord::row_version`].
    ///
    /// Read-modify-write is the shape every authoring surface has, and doing it
    /// by hand means restating which columns are content — the restatement that
    /// silently drops one the day a slice adds it.
    #[must_use]
    pub fn content(&self) -> PriceContent {
        PriceContent {
            row: self.row.clone(),
            tax_inclusive: self.tax_inclusive,
            tax_category_ref: self.tax_category_ref.clone(),
            billing_timing: self.billing_timing.clone(),
            proration_contract: self.proration_contract,
            rounding_policy_ref: self.rounding_policy_ref.clone(),
            grandfather_until: self.grandfather_until,
            supersedes_price_id: self.supersedes_price_id,
        }
    }
}

/// The `(meter, dimensionKey)` line as the **canonical scope key** spells it.
///
/// The pair is D-196's ninth and tenth axis *and* two ordinary fields on
/// [`PriceRow`], stored in one column each — so the axis and the field are the
/// same value and there is nowhere for them to disagree in the schema. They can
/// still disagree in the **code**: [`ScopeKey`]'s constructors trim
/// (`Meter::new`, `DimensionKey::new`), so a render into the columns that took
/// the caller's raw string would file a row authored `meter: "api_calls "` under
/// the key whose ninth axis is `api_calls` and store a column reading
/// `api_calls `. Every gate over the key compares the one against the other:
/// `price_repo::scope_key_filter` — and so `find_key_occupant`, the *only*
/// duplicate gate on the create path — renders the trimmed literal, and both
/// partial UNIQUE indexes compare the raw columns. Two spellings of one axis
/// would therefore pass every check and leave two rows on one canonical scope
/// key, which is the §3.7 invariant rating selection, window non-overlap and
/// coverage all rest on.
///
/// So this is a normalization and not a validation: it is spent by
/// [`authored_content`] on the record a door will answer, by
/// `price_repo::content_model` on the columns every door writes, and by
/// `price_repo::check_update_keeps_the_line`'s submitted side, so that all three
/// mean one value. The trim itself is [`Meter::normalized`] and
/// [`DimensionKey::new`] — the axes' own, so a change to what an axis spelling is
/// cannot reach the columns and miss the key.
///
/// It is deliberately **not** "take the line from the key", which is what
/// `charge_kind` does one function down: the wire can express a meter and
/// `ScopeKeyRequest` cannot, so the row's fields are the author's statement and a
/// door that overwrote them from the key would make the D-82/D-98/D-127 unit
/// guard's `meter` and `dimensionKey` clauses unreachable — see
/// `price_repo::resolve_authored_usage_line`, which refuses a genuine
/// disagreement rather than resolving it. Trimming preserves the statement; only
/// its spelling is fixed.
#[must_use]
pub fn canonical_usage_line(row: &PriceRow) -> (Option<String>, String) {
    (
        row.meter
            .as_deref()
            .map(|meter| Meter::normalized(meter).to_owned()),
        DimensionKey::new(&row.dimension_key).as_str().to_owned(),
    )
}

/// The rewrites a price row's **content** undergoes on its way into the store,
/// as one function.
///
/// `infra::storage::repo::price_repo::prepare_draft` performed both inline for as
/// long as it was the only thing that needed to know them, and that stopped being
/// true when D-88's orchestrator had to judge and compare a row *before* the store
/// held it. Three of them:
///
/// - **`charge_kind` comes from the key.** The row's own copy is not stored: the axis
///   is the canonical scope key's and the field on [`PriceRow`] is a convenience the
///   shape rules read. So a record read back always agrees with its own key, and a
///   caller that handed the two different answers gets the key's.
/// - **The bands are sorted by `from_qty`.** A read answers in that order — the table
///   carries no ordinal — so a create that kept the authored order would hand the
///   caller a record that stops equalling itself after one round trip.
/// - **The usage line is spelled the way its axes are** —
///   [`canonical_usage_line`]. The third for the same reason as
///   the first two and with a sharper cost: the pair *is* two axes of the key, so
///   a row whose column read `"api_calls "` was filed under the key whose axis
///   read `api_calls`, and the occupancy read that renders the axis could not
///   find it. See [`canonical_usage_line`] for what that admitted.
///
/// # Why this had to become shared, and what it cost while it was not
///
/// Both rewrites are invisible from the wire, and `api::rest::prices::content_of`
/// fills `charge_kind` with a **placeholder** (`ChargeKind::Recurring`) because
/// `PriceContentView` has no such field. Two consequences follow, both Critical:
///
/// 1. `domain::rules::SupersessionUnitGuard` gates on `successor.is_usage()`. Handed
///    the un-normalized row, it saw `Recurring` on every supersession — so on a
///    **usage** key the D-82/D-98/D-122/D-127/D-129 guard returned without evaluating,
///    and a successor could move `meter`, `billing_granularity` or `model_kind` under a
///    continued tier counter. The guard was live, correct, and unreachable through the
///    one surface that has it.
/// 2. `infra::supersession`'s divergent-successor guard compares the request's content
///    against the **stored** successor's. Un-normalized, the two differ in
///    `charge_kind` on every non-recurring key and in band order on any body that
///    lists bands out of order — so a legitimately approved unit was refused
///    `DUPLICATE_SCOPE_KEY` on its committing call, permanently, with a remedy sentence
///    the caller could not act on.
///
/// One spelling, six callers, and the rule is: **anything that judges or compares a
/// row the store will hold must pass it through here first.**
///
/// # Why it lives in the domain and is re-exported from the repository
///
/// It was written in `price_repo` and every caller still spells it
/// `price_repo::authored_content`, which is kept working by a re-export rather than
/// by a rename. It moved here because D-312's bulk-import arm needs it and
/// `domain::import` holds Phase 1's **store-free** rules — a domain module may not
/// reach into `infra`, and the alternative was a second copy of the projection,
/// which is the exact fault the two Criticals above were.
#[must_use]
pub fn authored_content(key: &ScopeKey, content: PriceContent) -> PriceContent {
    let (meter, dimension_key) = canonical_usage_line(&content.row);
    let mut row = PriceRow {
        charge_kind: key.charge_kind(),
        meter,
        dimension_key,
        ..content.row
    };
    row.bands.sort_by_key(|band| band.from_qty);
    PriceContent { row, ..content }
}

#[cfg(test)]
#[path = "price_record_tests.rs"]
mod price_record_tests;
