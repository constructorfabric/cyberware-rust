//! The `PriceOverlay` vocabulary — scope, lines, magnitudes and the resolution
//! rule (`design/09-price-overlays.md` §6, D-08, D-42, D-67, D-78, D-138).
//!
//! Where [`crate::domain::overlay_rules`] judges a whole candidate against the
//! world, this module is the **shapes** — and several of the design set's rules
//! are enforced here by being unrepresentable rather than checked, which is
//! stronger than a `CHECK` and cheaper than a rule:
//!
//! * **`(scope_class = 'global') = (scope_value = '')`** — [`ScopeSelector`] is a
//!   closed enum whose `Global` arm carries no value and whose other five arms
//!   each carry one. There is no way to build a classless overlay with a value.
//! * **`fixed` is always amount-based (D-138)** — [`Adjustment::Fixed`] carries
//!   an [`AmountSet`] and no `Magnitude`, so a percentage `fixed` line does not
//!   typecheck. This is the one rule the store spends a `CHECK` on that the
//!   domain makes impossible.
//! * **A `targetSku` line names its plan, and a `cohort` line does too** —
//!   [`LineKey`] has four constructors and no public fields, so the two
//!   `CHECK`s that pair those columns with `plan_id` have no representable
//!   counterexample in this crate.
//! * **The magnitude's type is declared, never inferred (D-08)** —
//!   [`Magnitude`] is a sum, so "a percent value and an amount set at once" and
//!   "neither" are both unconstructible.
//!
//! # What is deliberately **not** enforced here
//!
//! **D-67's ranges.** `0 < v <= 10000` on a discount and `v > 0` on a markup are
//! [`overlay_rules`](crate::domain::overlay_rules)' checks and not constructor
//! refusals, and the reason is §5's own wording: the refusal is
//! `ADJUSTMENT_MAGNITUDE_OUT_OF_RANGE` **naming the line**. A constructor cannot
//! name a line in an aggregate report, and the aggregate report is what makes an
//! overlay remediable in one pass rather than in as many round trips as it has
//! faults (Foundation §4.2). The store's two `CHECK`s are the backstop; the two
//! guards are in series, so the pure rule is unit-tested directly rather than
//! only through a path the store would have refused anyway.
//!
//! **Evaluation.** `markup` adds, `discount` subtracts and `fixed` replaces
//! (D-138), and none of that arithmetic is here: evaluation is Tariffs'. What
//! this crate owes is the authored shape, the resolution rule
//! [`resolve_line`] publishes, and the precedence and class order the read model
//! carries. In particular **no sort direction is encoded anywhere**, because
//! §F.1 leaves it undecided — see [`crate::domain::overlay_rules`] for the one
//! place that absence bites.
//!
//! # [`resolve_line`] has no consumer in this crate, and that is the design
//!
//! Stated here for the reason [`crate::domain::overlay_rules`]' own "rules this
//! module deliberately does not hold" section exists: that module names
//! `inst-plv-disclosure`'s exclusion half and `inst-plv-member-preview` by id
//! precisely so an **absent** arm does not read as an oversight.
//! [`resolve_line`] is the mirror image and had no such note — a
//! **present** function, documented "normative and adopted by Rating step 4",
//! with fifteen assertions across four cases, that nothing in this gear calls.
//!
//! Nothing should. The catalog's job on this plane is to **publish the whole
//! line set**: `infra::read_model` freezes every line of the overlay index into
//! the version a consumer pins, and resolution — which line applies to which
//! priced row — happens where the row and its class and generation are known,
//! which is Tariffs' step 4 and not here. There is no resolution route, no
//! in-process resolve method and no consumer gear in this tree;
//! `infra::read_model` contains no `resolve_line`, no `most_specific` and no
//! `lines`.
//!
//! So [`resolve_line`] exists as the **executable statement of a joint
//! contract**: D-78's eligibility filter and D-42's most-specific-wins ranking,
//! written once, in the gear that owns the vocabulary, so the adopting side
//! adopts an implementation rather than a paragraph. Its two helpers,
//! [`LineKey::eligible_for`] and [`LineKey::specificity`], have exactly one
//! caller each and it is this function. **The consequence for whoever adopts it
//! is worth saying plainly**: it has never run against a stored overlay in this
//! crate, so it is a specification that compiles and is unit-tested, not a path
//! this gear exercises in production. Do not delete it — the design set calls it
//! normative — and do not read its presence as enforcement.

use std::collections::BTreeMap;
use std::fmt;

use toolkit_macros::domain_model;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::money::CurrencyCode;
use crate::domain::scope_key::{PlanId, PriceEligibility};
use time::OffsetDateTime;

/// The six scope classes of §1.1, in the **class-specificity order** the read
/// model publishes as `inst-plv-class-tiebreak`'s tie-break:
/// `customerGroup > partner > orgTier > brand > region > global`.
///
/// Declared **least specific first**, so the derived [`Ord`] ranks them exactly
/// as that order does and no later code can build a second, disagreeing ranking
/// out of this type. That is [`PriceEligibility`]'s trick, applied to a
/// different order for the same reason.
///
/// **It is a rank, not a direction.** The class order breaks ties *inside* a
/// stack and never filters an overlay out — overlay application is stack-all,
/// never single-winner (`inst-plv-class-tiebreak`). The stack is walked
/// **ascending** since D-249 — lowest `precedence` first, most
/// specific last — which this type expresses exactly: the derived `Ord` ranks
/// least-specific lowest, so a class that sorts below another is applied before
/// it and is the one a `fixed` layer discards.
#[domain_model]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ScopeClass {
    /// The classless overlay — every payer of the tenant.
    Global,
    /// A pricing region.
    Region,
    /// A commercial brand.
    Brand,
    /// An organisation tier (D-120).
    OrgTier,
    /// A reseller / partner (D-120).
    Partner,
    /// A BSS customer group — the most specific class.
    CustomerGroup,
}

impl ScopeClass {
    /// Every class, least specific first.
    pub const ALL: &'static [Self] = &[
        Self::Global,
        Self::Region,
        Self::Brand,
        Self::OrgTier,
        Self::Partner,
        Self::CustomerGroup,
    ];

    /// The persisted / wire token, `snake_case` as the column and the wire both
    /// are (`toolkit_macros::api_dto` does not rename).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Region => "region",
            Self::Brand => "brand",
            Self::OrgTier => "org_tier",
            Self::Partner => "partner",
            Self::CustomerGroup => "customer_group",
        }
    }

    /// Parse a stored or authored token.
    #[must_use]
    pub fn parse(token: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|c| c.as_str() == token)
    }

    /// The physical taxonomy table this class's values are declared in, or
    /// `None` for [`ScopeClass::Global`], which has no value universe because it
    /// has no value.
    ///
    /// Named rather than returned as a repository handle so the domain stays
    /// free of infrastructure (DE0301): what the rule needs to *say* is which
    /// universe was consulted, and the consulting is
    /// [`overlay_repo`](crate::infra::storage::repo::overlay_repo)'s.
    #[must_use]
    pub const fn taxonomy_table(self) -> Option<&'static str> {
        match self {
            Self::Global => None,
            Self::Region => Some("pricing_region_taxonomy"),
            Self::Brand => Some("pricing_brand_taxonomy"),
            Self::OrgTier => Some("pricing_org_tier_taxonomy"),
            Self::Partner => Some("pricing_partner_taxonomy"),
            // The customer-group taxonomy belongs to the membership half, which
            // is not built. `ScopeSelector::customer_group` is refused at the
            // authoring edge for that reason; see its own doc.
            Self::CustomerGroup => Some("pricing_customer_group_taxonomy"),
        }
    }
}

impl fmt::Display for ScopeClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A declared taxonomy value. Never blank — the empty string is the store's
/// sentinel for the classless scope, so a blank value would make that sentinel
/// forgeable.
#[domain_model]
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ScopeValue(String);

impl ScopeValue {
    /// Wrap an authored value, refusing a blank one.
    #[must_use]
    pub fn new(raw: &str) -> Option<Self> {
        let trimmed = raw.trim();
        (!trimmed.is_empty()).then(|| Self(trimmed.to_owned()))
    }

    /// The value as stored.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ScopeValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// An overlay's scope: a class, and a value exactly when the class has one.
///
/// The store spends a biconditional `CHECK` on this pairing
/// (`chk_pricing_price_overlay_scope_value`); here it is unrepresentable, which
/// is why no rule in [`overlay_rules`](crate::domain::overlay_rules) checks it.
#[domain_model]
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ScopeSelector {
    /// Every payer of the tenant. Carries no value.
    Global,
    /// A class with a declared value.
    Scoped {
        /// The class — never [`ScopeClass::Global`], which the constructor
        /// refuses.
        class: ScopeClass,
        /// The value, validated against `class.taxonomy_table()` by
        /// `inst-plv-scope`.
        value: ScopeValue,
    },
}

/// Every member of a [`ScopeSelector`] at once, for a caller that must account
/// for **all** of them — the D-225 overlay content pin.
///
/// [`ThresholdVersionParts`](crate::domain::materiality::ThresholdVersionParts)'
/// construction and for its reason, applied to an enum: the selector's members
/// are reachable only through [`ScopeSelector::class`] and
/// [`ScopeSelector::value`], and two accessor calls are exactly what a third
/// member of [`ScopeSelector::Scoped`] arrives without breaking. Produced by
/// [`ScopeSelector::parts`], whose match binds every field with no rest pattern,
/// and destructured in turn by `approval::content_pin::put_overlay_revision` — so
/// widening the selector costs two compile errors and a decision at the pin.
pub struct ScopeSelectorParts<'a> {
    /// The class, [`ScopeClass::Global`] for the classless scope.
    pub class: ScopeClass,
    /// The value, `None` for the classless scope.
    pub value: Option<&'a ScopeValue>,
}

impl ScopeSelector {
    /// Build a scoped selector, refusing [`ScopeClass::Global`] — which would be
    /// a classless overlay carrying a value, the state the biconditional
    /// forbids.
    #[must_use]
    pub fn scoped(class: ScopeClass, value: ScopeValue) -> Option<Self> {
        (class != ScopeClass::Global).then_some(Self::Scoped { class, value })
    }

    /// The selector's class.
    #[must_use]
    pub const fn class(&self) -> ScopeClass {
        match self {
            Self::Global => ScopeClass::Global,
            Self::Scoped { class, .. } => *class,
        }
    }

    /// The selector's value, or `None` for the classless scope.
    #[must_use]
    pub const fn value(&self) -> Option<&ScopeValue> {
        match self {
            Self::Global => None,
            Self::Scoped { value, .. } => Some(value),
        }
    }

    /// How the store renders the value — the empty string for the classless
    /// scope. See `pricing_price_overlay` for why the sentinel and not a NULL.
    #[must_use]
    pub fn stored_value(&self) -> &str {
        self.value().map_or("", ScopeValue::as_str)
    }

    /// Every member at once, for the caller that must frame all of them.
    ///
    /// See [`ScopeSelectorParts`]. The `Scoped` arm binds both fields explicitly
    /// with **no rest pattern**, which is the whole point: [`Self::class`] and
    /// [`Self::value`] each use `..`, so a third member of that variant compiles
    /// through both of them and drops silently out of the overlay pin — an
    /// approve taken over one overlay verifying against a different one, with
    /// every digest equal.
    #[must_use]
    pub fn parts(&self) -> ScopeSelectorParts<'_> {
        match self {
            Self::Global => ScopeSelectorParts {
                class: ScopeClass::Global,
                value: None,
            },
            Self::Scoped { class, value } => ScopeSelectorParts {
                class: *class,
                value: Some(value),
            },
        }
    }
}

/// L5's declared tax basis. `NOT NULL` in the store, and a closed enum here —
/// which is exactly why *"silence fails"* needs an entry point of its own; see
/// [`overlay_rules::check_tax_basis_declared`](crate::domain::overlay_rules::check_tax_basis_declared).
#[domain_model]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TaxBasis {
    /// The adjustment is stated tax-inclusive.
    Inclusive,
    /// The adjustment is stated tax-exclusive.
    Exclusive,
    /// Explicitly delegated to Tariffs — a **declaration**, not a silence.
    DelegatedTariffs,
}

impl TaxBasis {
    /// Every basis, in §6's order.
    pub const ALL: &'static [Self] = &[Self::Inclusive, Self::Exclusive, Self::DelegatedTariffs];

    /// The persisted / wire token.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Inclusive => "inclusive",
            Self::Exclusive => "exclusive",
            Self::DelegatedTariffs => "delegated_tariffs",
        }
    }

    /// Parse a stored or authored token.
    #[must_use]
    pub fn parse(token: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|b| b.as_str() == token)
    }
}

/// L6's consumer-facing exposure flag.
///
/// `restricted` means the overlay — **including its existence** — is never
/// exposed on a consumer-facing enumeration or preview and resolves only inside
/// the member payer's own evaluation context. Operator and service reads are
/// unaffected: the flag governs consumer-facing exposure only, and this gear
/// mounts no consumer-facing enumeration at all (see
/// [`overlay_rules`](crate::domain::overlay_rules) for what that leaves owed).
#[domain_model]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Disclosure {
    /// The fail-closed default.
    #[default]
    Restricted,
    /// Presentation and the Tariffs effective-price preview may disclose the
    /// adjusted price to anyone.
    Public,
}

impl Disclosure {
    /// Both values, default first.
    pub const ALL: &'static [Self] = &[Self::Restricted, Self::Public];

    /// The persisted / wire token.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Restricted => "restricted",
            Self::Public => "public",
        }
    }

    /// Parse a stored or authored token.
    #[must_use]
    pub fn parse(token: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|d| d.as_str() == token)
    }
}

/// The lifecycle of one overlay **revision** (D-92).
///
/// **Four states, and the fourth is a tombstone** — [`Self::Abandoned`], added by
/// D-231 for the reason `pricing_plan` has carried one since D-145: a discarded
/// draft that left by `DELETE` gave its revision number back to `max(revision) +
/// 1`, so a client holding the discarded revision's entity tag found a different
/// revision under the same overlay identity and its stale `If-Match` matched the
/// fresh row's compare-and-swap. The variant's own doc states it in the same
/// words, and [`OverlayLifecycle::ALL`] lists all four.
///
/// **A draft has no `DELETE` exit**, so a caller or a repository path built
/// around one has nothing to call: the tombstone three lines below is the exit,
/// and it is what keeps the discarded revision number spent.
#[domain_model]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum OverlayLifecycle {
    /// The editable revision. At most one per overlay.
    #[default]
    Draft,
    /// The revision consumers resolve.
    Published,
    /// A predecessor, flipped by the submit that published its successor.
    Superseded,
    /// A discarded draft's tombstone (D-231). Terminal: it never publishes, never
    /// supersedes, and is never a candidate for a read.
    ///
    /// It exists to hold the **revision number** the discarded draft consumed. A
    /// discarded draft used to leave by `DELETE`, so `max(revision) + 1` re-minted
    /// its number and a client holding the discarded revision's entity tag found a
    /// different revision under the same overlay identity — its stale `If-Match`
    /// matching the fresh row's compare-and-swap. `pricing_plan` has carried the
    /// same tombstone since D-145, for the reason stated in the same words.
    Abandoned,
}

impl OverlayLifecycle {
    /// Every state, in §6's order.
    pub const ALL: &'static [Self] = &[
        Self::Draft,
        Self::Published,
        Self::Superseded,
        Self::Abandoned,
    ];

    /// The persisted / wire token.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Draft => "draft",
            Self::Published => "published",
            Self::Superseded => "superseded",
            Self::Abandoned => "abandoned",
        }
    }

    /// Parse a stored token.
    #[must_use]
    pub fn parse(token: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|s| s.as_str() == token)
    }
}

/// A SKU reference on a line. Never blank, for [`ScopeValue`]'s reason: the
/// store's line key coalesces an absent SKU to the empty string.
#[domain_model]
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TargetSku(String);

impl TargetSku {
    /// Wrap an authored SKU, refusing a blank one.
    #[must_use]
    pub fn new(raw: &str) -> Option<Self> {
        let trimmed = raw.trim();
        (!trimmed.is_empty()).then(|| Self(trimmed.to_owned()))
    }

    /// The SKU as stored.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for TargetSku {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// One line's key inside a revision: `(planId?, targetSku?, cohort?)`.
///
/// Four constructors and no public fields, because two of §6's `CHECK`s are
/// pairings this type can simply not violate: a `targetSku` requires a
/// `planId` (a bare SKU is ambiguous per `(currency, region)`) and a `cohort`
/// requires one too (`inst-plv-eligibility` validates a cohort against *"the
/// line's target plan"*, which the list-default line has none of).
///
/// **`cohort` is a filter and not a level**, which is why it is in the key and
/// **not** in [`LineKey::specificity`]: a cohort-targeted line and a cohort-less
/// line on one `(plan, sku)` are disjoint by eligibility rather than ranked
/// against each other (D-78).
#[domain_model]
#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LineKey {
    plan_id: Option<PlanId>,
    target_sku: Option<TargetSku>,
    cohort: Option<OffsetDateTime>,
}

/// Every field of a [`LineKey`] at once, for a caller that must account for
/// **all** of them — the D-225 overlay content pin.
///
/// [`ThresholdVersionParts`](crate::domain::materiality::ThresholdVersionParts)'
/// construction and for its reason, and the case is the same one exactly: the
/// fields are private because the four constructors are what make two of §6's
/// `CHECK`s unviolatable, so a consumer that has to frame the whole key can only
/// reach it through [`LineKey::plan_id`], [`LineKey::target_sku`] and
/// [`LineKey::cohort`] — three accessor calls, which are three things a fourth
/// field is silently absent from. Produced by [`LineKey::parts`], which
/// destructures `Self` exhaustively, and destructured in turn at the pin.
pub struct LineKeyParts<'a> {
    /// The line's target plan.
    pub plan_id: Option<PlanId>,
    /// The line's SKU narrowing.
    pub target_sku: Option<&'a TargetSku>,
    /// The generation this line filters to.
    pub cohort: Option<OffsetDateTime>,
}

impl LineKey {
    /// The list-default line: no plan, no SKU, no cohort. It applies to every
    /// target of the overlay's `target_ref`.
    #[must_use]
    pub const fn list_default() -> Self {
        Self {
            plan_id: None,
            target_sku: None,
            cohort: None,
        }
    }

    /// A per-plan line.
    #[must_use]
    pub const fn for_plan(plan_id: PlanId) -> Self {
        Self {
            plan_id: Some(plan_id),
            target_sku: None,
            cohort: None,
        }
    }

    /// A per-`(plan, sku)` line — the most specific level.
    #[must_use]
    pub const fn for_sku(plan_id: PlanId, target_sku: TargetSku) -> Self {
        Self {
            plan_id: Some(plan_id),
            target_sku: Some(target_sku),
            cohort: None,
        }
    }

    /// Narrow this key to one grandfathered generation (D-78).
    ///
    /// Takes `self` rather than being a fifth constructor so the `plan_id`
    /// requirement is carried by the key it narrows: `list_default().for_cohort`
    /// is refused, which is §6's `CHECK (cohort IS NULL OR plan_id IS NOT NULL)`
    /// with no `CHECK`.
    #[must_use]
    pub fn for_cohort(self, cohort: OffsetDateTime) -> Option<Self> {
        self.plan_id.is_some().then_some(Self {
            cohort: Some(cohort),
            ..self
        })
    }

    /// The line's target plan, or `None` on the list-default line.
    #[must_use]
    pub const fn plan_id(&self) -> Option<PlanId> {
        self.plan_id
    }

    /// The line's SKU narrowing.
    #[must_use]
    pub const fn target_sku(&self) -> Option<&TargetSku> {
        self.target_sku.as_ref()
    }

    /// The generation this line filters to, or `None` for the ordinary
    /// eligibility classes.
    #[must_use]
    pub const fn cohort(&self) -> Option<OffsetDateTime> {
        self.cohort
    }

    /// Every field at once, for the caller that must frame all of them.
    ///
    /// See [`LineKeyParts`]. The destructure has no rest pattern, so a fourth
    /// field stops **this** compiling, and the parts struct is destructured at the
    /// pin, so the same field then stops the encoder compiling until somebody
    /// decides whether the pin covers it.
    #[must_use]
    pub fn parts(&self) -> LineKeyParts<'_> {
        let Self {
            plan_id,
            target_sku,
            cohort,
        } = self;
        LineKeyParts {
            plan_id: *plan_id,
            target_sku: target_sku.as_ref(),
            cohort: *cohort,
        }
    }

    /// The most-specific rank of `inst-plv-lines`: `(plan, sku)` > `(plan)` >
    /// default, as `2 > 1 > 0`.
    ///
    /// **`cohort` is deliberately not an input.** It selects which rows a line
    /// is *eligible* for; the ranking then runs unchanged inside the eligible
    /// set, so nothing about existing resolution moves (D-78).
    #[must_use]
    pub const fn specificity(&self) -> u8 {
        match (self.plan_id.is_some(), self.target_sku.is_some()) {
            (true, true) => 2,
            (true, false) => 1,
            // A SKU without a plan is unconstructible, so this arm is the
            // list-default line and nothing else.
            (false, _) => 0,
        }
    }

    /// Is this line eligible for a row of `eligibility` at `cohort`?
    ///
    /// D-78, and it is a **filter**: a `cohort`-less line applies only to
    /// `all_subscriptions` / `new_subscriptions_only` rows, and a
    /// `cohort`-carrying line applies **only** to `existing_grandfathered` rows
    /// of that generation.
    #[must_use]
    pub fn eligible_for(
        &self,
        eligibility: PriceEligibility,
        cohort: Option<OffsetDateTime>,
    ) -> bool {
        match self.cohort {
            None => eligibility != PriceEligibility::ExistingGrandfathered,
            Some(generation) => {
                eligibility == PriceEligibility::ExistingGrandfathered && cohort == Some(generation)
            }
        }
    }
}

/// An amount-based magnitude: money, and therefore **per currency** (D-08, no
/// implicit FX).
#[domain_model]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AmountSet(BTreeMap<CurrencyCode, i64>);

impl AmountSet {
    /// Build from authored pairs. A later pair on one currency replaces an
    /// earlier one.
    ///
    /// # The last-wins rule is total here because nothing reaches it holding a duplicate
    ///
    /// `UNIQUE (line_id, currency)` never sees a collision — a map has already
    /// collapsed the pairs by the time a row is written — so the index cannot be
    /// what refuses one. What refuses it is
    /// [`adjustment_of`], the authoring door, which
    /// names the currency: `amounts: [{USD, 100}, {usd, 9999}]` is one currency
    /// twice, because [`CurrencyCode::new`] normalises case, and an author who
    /// silently lost one of two numbers has no way to see which survived. The two
    /// sibling authored-proposal constructors refuse the same shape on the same
    /// ground —
    /// [`ThresholdVersion::new`](crate::domain::materiality::ThresholdVersion::new)
    /// with `ThresholdRefusal::DuplicateCurrency`, and
    /// [`MembershipMoveSet::new`](crate::domain::membership_change::MembershipMoveSet::new)
    /// on a duplicate payer.
    ///
    /// Refusing here as well costs a rule with no operand on the other production
    /// path: `overlay_repo`'s read assembles from rows the `UNIQUE` has already
    /// made distinct, so a fallible constructor would make every read of a legal
    /// row handle an error no legal row can produce.
    #[must_use]
    pub fn new(values: impl IntoIterator<Item = (CurrencyCode, i64)>) -> Self {
        Self(values.into_iter().collect())
    }

    /// The value for one currency, or `None` when the market is uncovered.
    #[must_use]
    pub fn get(&self, currency: &CurrencyCode) -> Option<i64> {
        self.0.get(currency).copied()
    }

    /// Every covered currency, in a stable order.
    pub fn currencies(&self) -> impl Iterator<Item = &CurrencyCode> {
        self.0.keys()
    }

    /// Every pair, in a stable order.
    pub fn iter(&self) -> impl Iterator<Item = (&CurrencyCode, i64)> {
        self.0.iter().map(|(c, v)| (c, *v))
    }

    /// Is the set empty? An amount-based line with no values covers no market
    /// at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// The persisted `magnitude_kind` token, as a closed enum.
///
/// [`Magnitude`]'s discriminant, in the shape [`ScopeClass`] is
/// [`ScopeSelector`]'s: the value-carrying type answers with this one and this
/// one owns the spelling. It exists because a data-carrying enum cannot have an
/// `ALL` slice, and without an `ALL` slice the repository cannot call
/// `read_token`. Absent this type `overlay_repo::line_of` hand-writes the inverse
/// list, thirteen lines above a sibling that parses four tokens through their
/// types' own `parse`.
///
/// The failure mode that fix closes is stated in `read_token`'s own doc: *"a
/// hand-written inverse list is one a variant added later goes missing from
/// while everything still compiles, and what that produces is a perfectly legal
/// row read back as a corrupt one."* Renaming a token here now moves the writer,
/// the reader and the wire parser together, because there is one spelling.
#[domain_model]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum MagnitudeKind {
    /// A currency-neutral basis-points value.
    PercentBp,
    /// Absolute money, per currency.
    Amount,
}

impl MagnitudeKind {
    /// Both kinds, in [`Magnitude`]'s declaration order.
    pub const ALL: &'static [Self] = &[Self::PercentBp, Self::Amount];

    /// The persisted / wire token.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PercentBp => "percent_bp",
            Self::Amount => "amount",
        }
    }

    /// Parse a stored or authored token.
    #[must_use]
    pub fn parse(token: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|k| k.as_str() == token)
    }
}

/// A line's magnitude, with its type **declared** (D-08).
///
/// A sum rather than a pair of nullable fields, so "a percent value and an
/// amount set at once" and "neither" are both unconstructible. The store spends
/// a biconditional `CHECK` on the same rule, because the store has no sums.
#[domain_model]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Magnitude {
    /// A single basis-points value — currency-neutral.
    PercentBp(i64),
    /// Absolute money, per currency.
    Amount(AmountSet),
}

/// The persisted `adjustment_kind` token, as a closed enum.
///
/// [`Adjustment`]'s discriminant; see [`MagnitudeKind`] for why the pair exists
/// and what a hand-written inverse list costs.
#[domain_model]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AdjustmentKind {
    /// Adds its magnitude to the layer's input.
    Markup,
    /// Subtracts its magnitude from the layer's input.
    Discount,
    /// **Replaces** the running line amount (D-138).
    Fixed,
}

impl AdjustmentKind {
    /// Every kind, in [`Adjustment`]'s declaration order.
    pub const ALL: &'static [Self] = &[Self::Markup, Self::Discount, Self::Fixed];

    /// The persisted / wire token.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Markup => "markup",
            Self::Discount => "discount",
            Self::Fixed => "fixed",
        }
    }

    /// Parse a stored or authored token.
    #[must_use]
    pub fn parse(token: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|k| k.as_str() == token)
    }
}

/// A line's adjustment: what it does to the running amount, and by how much.
///
/// **D-138 is normative and is encoded in the shape**: `Fixed` carries an
/// [`AmountSet`] rather than a [`Magnitude`], so a percentage `fixed` line does
/// not typecheck. `fixed` **replaces** the running line amount with an absolute
/// price at that layer; an additive reading would make it a duplicate of
/// `Markup`, which already expresses absolute money through
/// [`Magnitude::Amount`].
#[domain_model]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Adjustment {
    /// Adds its magnitude to the layer's input.
    Markup(Magnitude),
    /// Subtracts its magnitude from the layer's input.
    Discount(Magnitude),
    /// **Replaces** the running line amount. Always amount-based.
    Fixed(AmountSet),
}

impl Adjustment {
    /// The persisted / wire kind.
    ///
    /// Answers with [`AdjustmentKind`] rather than a `&str` for
    /// [`ScopeSelector::class`]'s reason: the token has one spelling, it lives on
    /// the discriminant, and a repository reading the column back can then ask
    /// `read_token` for it instead of writing its own inverse list.
    #[must_use]
    pub const fn kind(&self) -> AdjustmentKind {
        match self {
            Self::Markup(_) => AdjustmentKind::Markup,
            Self::Discount(_) => AdjustmentKind::Discount,
            Self::Fixed(_) => AdjustmentKind::Fixed,
        }
    }

    /// The persisted `magnitude_kind`.
    #[must_use]
    pub const fn magnitude_kind(&self) -> MagnitudeKind {
        match self {
            Self::Markup(Magnitude::PercentBp(_)) | Self::Discount(Magnitude::PercentBp(_)) => {
                MagnitudeKind::PercentBp
            }
            // `fixed` shares this arm rather than carrying its own: D-138
            // makes it amount-based by construction, so `Amount` here is the
            // same fact reached through a different variant.
            Self::Markup(Magnitude::Amount(_))
            | Self::Discount(Magnitude::Amount(_))
            | Self::Fixed(_) => MagnitudeKind::Amount,
        }
    }

    /// The basis-points value, on a percent line only — the store's
    /// `adjustment_value` column.
    #[must_use]
    pub const fn percent_bp(&self) -> Option<i64> {
        match self {
            Self::Markup(Magnitude::PercentBp(v)) | Self::Discount(Magnitude::PercentBp(v)) => {
                Some(*v)
            }
            _ => None,
        }
    }

    /// The per-currency values, on an amount line only.
    #[must_use]
    pub const fn amounts(&self) -> Option<&AmountSet> {
        match self {
            Self::Markup(Magnitude::Amount(set)) | Self::Discount(Magnitude::Amount(set)) => {
                Some(set)
            }
            Self::Fixed(set) => Some(set),
            _ => None,
        }
    }

    /// Does this line's magnitude need per-currency coverage
    /// (`ADJUSTMENT_CURRENCY_NOT_COVERED`)?
    #[must_use]
    pub const fn is_amount_based(&self) -> bool {
        self.amounts().is_some()
    }

    /// Does this adjustment **replace** the running amount, voiding the layers
    /// it discards (D-138)?
    #[must_use]
    pub const fn replaces(&self) -> bool {
        matches!(self, Self::Fixed(_))
    }
}

/// One authored adjustment line.
#[domain_model]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OverlayLine {
    /// The line's identity — stable across a copy-on-new-revision where the
    /// line is unchanged (D-92).
    pub line_id: Uuid,
    /// `(planId?, targetSku?, cohort?)`.
    pub key: LineKey,
    /// What it does, and by how much.
    pub adjustment: Adjustment,
}

/// The plans and SKUs an overlay's lines may target (`inst-plv-referential`).
#[domain_model]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TargetRef {
    /// The plans in scope. Empty means the overlay targets nothing in
    /// particular, which only a list-default line can serve.
    pub plans: Vec<PlanId>,
}

impl TargetRef {
    /// Does this reference cover `plan`?
    #[must_use]
    pub fn covers(&self, plan: PlanId) -> bool {
        self.plans.contains(&plan)
    }
}

/// The overlay's own `[effectiveFrom, effectiveTo)` — **not** a price-row key
/// axis, because an overlay is not a price row.
#[domain_model]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OverlayInterval {
    /// Inclusive start, or `None` for "from the beginning of time".
    pub from: Option<OffsetDateTime>,
    /// Exclusive end, or `None` for open-ended.
    pub to: Option<OffsetDateTime>,
}

impl OverlayInterval {
    /// Does this interval intersect `other`?
    ///
    /// Half-open on both sides, so a `to` equal to the other's `from` is
    /// **adjacent and not overlapping** — the rule the window plane already
    /// holds, and the one an operator relies on to schedule a successor.
    #[must_use]
    pub fn intersects(&self, other: &Self) -> bool {
        let starts_before_other_ends = match (self.from, other.to) {
            (Some(from), Some(to)) => from < to,
            _ => true,
        };
        let other_starts_before_this_ends = match (other.from, self.to) {
            (Some(from), Some(to)) => from < to,
            _ => true,
        };
        starts_before_other_ends && other_starts_before_this_ends
    }
}

/// **`inst-plv-lines`' resolution rule, normative and adopted by Rating step 4.**
///
/// Within one overlay the **most-specific line wins** for the priced row —
/// `(planId, targetSku)` > `(planId)` > list-default — and exactly one line of a
/// list applies per row. The eligibility filter (D-78) runs **first**: a line is
/// a candidate only if it is eligible for the row's class and generation, and
/// the ranking then runs unchanged inside that eligible set.
///
/// It is **never `precedence`**: precedence orders *lists* against each other
/// and never lines inside one.
///
/// `None` means no line of this overlay applies to the row, which is an ordinary
/// outcome and not a failure — a `plan`-scoped list simply says nothing about a
/// plan it does not name.
///
/// **Nothing in this crate calls it**, and the module doc's section on that is
/// the statement of why — the catalog publishes the whole line set and
/// resolution belongs to the consumer that holds the priced row. Read it before
/// concluding this function is live enforcement.
#[must_use]
pub fn resolve_line<'a>(
    lines: &'a [OverlayLine],
    plan: PlanId,
    sku: Option<&TargetSku>,
    eligibility: PriceEligibility,
    cohort: Option<OffsetDateTime>,
) -> Option<&'a OverlayLine> {
    lines
        .iter()
        .filter(|line| line.key.eligible_for(eligibility, cohort))
        .filter(|line| match line.key.plan_id() {
            None => true,
            Some(target) => target == plan,
        })
        .filter(|line| match line.key.target_sku() {
            None => true,
            Some(target) => sku == Some(target),
        })
        .max_by_key(|line| line.key.specificity())
}

/// One overlay revision's **content**, as an approval unit pins it (D-225).
///
/// `PlanShape`'s counterpart for this plane: the shape a reviewer is shown and the
/// shape `content_pin::overlay_content_hash` frames. It is a domain type and not the
/// repository's `OverlayRecord` because the pin is pure domain — the encoder may not
/// depend on a storage row's field list, or a column added for an unrelated reason
/// would silently re-freeze every pending unit in every tenant.
///
/// # `row_version` is deliberately not here
///
/// The record carries one and this shape does not. A pin exists to answer *"is this
/// the content the reviewer saw"*, and the row version is a **concurrency token**:
/// it moves when the content moves, so including it adds nothing, and it can move
/// for reasons the content did not — which is a pin that fails for a reason the
/// reviewer cannot see. That is the same argument `PlanShape` makes for excluding
/// its `evaluated_at` and its baseline, and the exclusion is asserted rather than
/// described by `overlay_repo_tests::the_pin_ignores_the_row_version`.
#[domain_model]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OverlayRevision {
    /// The overlay's identity.
    pub price_overlay_id: Uuid,
    /// Which revision of it.
    pub revision: u64,
    /// Its state — pinned, because approving a draft and approving a published
    /// revision are not the same act.
    pub lifecycle_state: OverlayLifecycle,
    /// Scope class and value.
    pub scope: ScopeSelector,
    /// Its precedence within the class.
    pub precedence: i32,
    /// Its own dating.
    pub interval: OverlayInterval,
    /// Its declared tax basis.
    pub tax_basis: TaxBasis,
    /// Its exposure flag.
    pub disclosure: Disclosure,
    /// The plans its lines may target.
    pub target_ref: TargetRef,
    /// The whole line set — the money.
    pub lines: Vec<OverlayLine>,
}

#[cfg(test)]
#[path = "overlay_tests.rs"]
mod overlay_tests;

/// The wire fields that say **what to do to an amount**, as the domain's
/// [`Adjustment`].
///
/// `pub(crate)`, and extracted rather than copied the moment a second caller
/// appeared: [`crate::api::rest::overlays`] authors a line,
/// [`crate::api::rest::repricing_runs`] submits a run, and
/// [`crate::domain::repricing`] reads a run's frozen report back. A
/// mass-repricing run's adjustment is the same
/// question over the same three tokens — `markup | discount | fixed`,
/// `percent_bp | amount`, and the per-currency values — and
/// `12-operator-efficiency.md` `inst-mr-api` names the field without defining it,
/// so a second reading of "what to do to a price" would have been a second
/// vocabulary for one fact. Four of the six pairings this rejects are rejected by
/// **no** store constraint on the repricing path, because a run's adjustment is
/// not persisted as an overlay line at all: a copy that drifted would drift
/// silently.
///
/// Every refusal here is decidable from the request alone, and **D-67's magnitude
/// range is one of them**: [`magnitude_out_of_range`](crate::domain::overlay_rules::magnitude_out_of_range)
/// is asked below, so this door and the overlay rule refuse the same values.
///
/// Not excluded on the ground that the range is a rule about an authored overlay
/// document rather than about this shape. That ground is sound and the conclusion
/// does not follow: the mass-repricing door reaches this parser too, a run's
/// adjustment is never persisted as an overlay line, so the rule form has no
/// subject on that path and the range is asked by nothing at all there. The rule
/// form ([`check_authored_shape`](crate::domain::overlay_rules::check_authored_shape))
/// runs for an authored document, where the refusal is per line and names the
/// line's key — and it is the only entry point, so a doc link to any other sends
/// a reader to a range check that does not run.
///
/// # Errors
/// [`DomainError::InvalidRequest`] for an unknown token, a `percent_bp` with no
/// value, an `amount` carrying one, or a `fixed` declared `percent_bp` (D-08,
/// D-138); [`DomainError::CurrencyInvalid`] for a malformed currency.
pub(crate) fn adjustment_of(
    adjustment_kind: &str,
    magnitude_kind: &str,
    adjustment_value: Option<i64>,
    amounts: &[(String, i64)],
) -> Result<Adjustment, DomainError> {
    let amounts = {
        let mut set = Vec::with_capacity(amounts.len());
        for amount in amounts {
            let currency = CurrencyCode::new(&amount.0)?;
            // **Refused here and not in `AmountSet::new`.** That constructor
            // collects into a map, so a repeated currency silently keeps the last
            // value and an author who wrote two numbers for one market cannot see
            // which survived — and `CurrencyCode::new` upper-cases, so `usd` and
            // `USD` are one key. The constructor cannot be the place: the set is
            // also assembled from stored rows, which `UNIQUE (line_id, currency)`
            // has already made distinct, so a refusal there would be a rule with no
            // operand on that path. The two sibling authored-proposal constructors
            // that refuse this shape — `ThresholdVersion::new` and
            // `MembershipMoveSet::new` — are both authoring doors, which is the same
            // placement as this one.
            if set.iter().any(|(held, _)| *held == currency) {
                return Err(DomainError::InvalidRequest(format!(
                    "currency {currency} is named twice in one line; a line carries one value per \
                     market, and currency codes are compared case-insensitively"
                )));
            }
            set.push((currency, amount.1));
        }
        AmountSet::new(set)
    };

    // Both tokens go through the domain's own `parse`, which is the same list
    // `Adjustment::kind()` renders and `overlay_repo::line_of` reads back. One
    // producer and no inverse: hand-written inverse lists are what this vocabulary
    // grows otherwise, one per reader.
    let magnitude_kind = MagnitudeKind::parse(magnitude_kind).ok_or_else(|| {
        DomainError::InvalidRequest(format!(
            "magnitude_kind `{magnitude_kind}` is neither percent_bp nor amount"
        ))
    })?;
    let magnitude = match magnitude_kind {
        MagnitudeKind::PercentBp => Magnitude::PercentBp(adjustment_value.ok_or_else(|| {
            DomainError::InvalidRequest(
                "a percent_bp line must carry an adjustment_value: the magnitude's type is \
                 declared and never inferred (D-08)"
                    .to_owned(),
            )
        })?),
        MagnitudeKind::Amount => {
            if adjustment_value.is_some() {
                return Err(DomainError::InvalidRequest(
                    "an amount line must not carry an adjustment_value: its magnitude is money \
                     and lives per currency (D-08)"
                        .to_owned(),
                ));
            }
            Magnitude::Amount(amounts.clone())
        }
    };

    let adjustment_kind = AdjustmentKind::parse(adjustment_kind).ok_or_else(|| {
        DomainError::InvalidRequest(format!(
            "adjustment_kind `{adjustment_kind}` is none of markup, discount, fixed"
        ))
    })?;
    let adjustment = match adjustment_kind {
        AdjustmentKind::Markup => Adjustment::Markup(magnitude),
        AdjustmentKind::Discount => Adjustment::Discount(magnitude),
        AdjustmentKind::Fixed => {
            // Asked of the **value** the parse above built, not of the token it
            // was built from: the two can only disagree if this comparison and
            // that `match` drift apart, and D-138 is a rule about the magnitude
            // rather than about a spelling of it.
            if !matches!(magnitude, Magnitude::Amount(_)) {
                return Err(DomainError::InvalidRequest(
                    "a fixed line is always amount-based: it replaces the running amount with \
                     an absolute price, and a percentage of the amount it replaces evaluates to \
                     nothing (D-138)"
                        .to_owned(),
                ));
            }
            Adjustment::Fixed(amounts)
        }
    };

    // D-67's range, asked here rather than only in the overlay rule pipeline.
    //
    // It is decidable from the request alone, which is this function's own stated
    // admission criterion, and the mass-repricing door reaches this parser without
    // ever producing an overlay line — so the rule form cannot see a run's
    // adjustment and neither can the store's `CHECK`s. Without this call a
    // `discount` of 15000 bp is admitted here and refused at `PUT /overlays`: the
    // same value on the same vocabulary, answered two different ways.
    //
    // Reported as a `ValidationFailed` carrying `ADJUSTMENT_MAGNITUDE_OUT_OF_RANGE`
    // rather than as an `InvalidRequest`, for the reason
    // [`crate::api::rest::overlays`]'s `tax_basis` refusal states: the client needs
    // the discriminator §5 declares, not a sentence. An `InvalidRequest` here makes
    // the two doors agree on the status and disagree on the code, which is the
    // drift the range check exists to close.
    if let Err(clause) = crate::domain::overlay_rules::magnitude_out_of_range(&adjustment) {
        let mut report = crate::domain::validation::ValidationReport::default();
        report.violate(
            crate::domain::overlay_rules::ADJUSTMENT_MAGNITUDE_OUT_OF_RANGE,
            "adjustment",
            format!("the adjustment carries {clause}"),
        );
        return Err(DomainError::ValidationFailed(report));
    }
    Ok(adjustment)
}
