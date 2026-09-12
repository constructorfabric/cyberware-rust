//! `TaxonomyValidator` — the three `inst-tx-*` rules of
//! `design/04-currency-tax.md` §3, plus the vocabulary the four tenant
//! taxonomies are addressed by.
//!
//! §1.7 names one entity for all of it: *"Registered rules: `region` membership
//! on price rows; overlay **scope-value** membership per class — `brand`,
//! `region`, `partner`, `orgTier` against their tenant taxonomies (D-120; rule
//! shared with Slice 9, which owns the `customerGroup` analogue)"*. The second
//! half of that sentence is **already built**, in
//! [`overlay_rules::check_scope`](crate::domain::overlay_rules), because Slice 9
//! could not validate an overlay scope without it and the four tables were
//! carried onto its chain to make that possible. What was missing is everything
//! else: the price-row half, the retire guard, and any surface at all through
//! which a value reaches one of the four tables.
//!
//! # Why this module does not re-declare a scope class
//!
//! [`ScopeClass`] already enumerates the six overlay scope classes, ranks them
//! for `inst-plv-class-tiebreak`, and maps the four taxonomy-backed ones to their
//! physical table. A second enumeration here would be a second answer to *which
//! table declares this class's values*, and the two would drift the day a class
//! is added. [`TaxonomyClass`] is therefore a **narrowing** of it — the four
//! classes §5's route can address — and it converts into `ScopeClass` rather than
//! restating its table map.
//!
//! The narrowing is the route's, not an opinion: `global` has no value universe
//! at all, and `customerGroup`'s universe is `pricing_customer_group_taxonomy`,
//! which belongs to Slice 9's membership half and does not exist (D-223 refuses
//! that class outright for exactly this reason). A `PUT` addressing either would
//! be a write to a table this gear does not have.
//!
//! # The refusal codes, and the one this module deliberately does not raise
//!
//! §5 declares `REGION_UNKNOWN` and `TAXONOMY_VALUE_IN_USE`, and both are minted
//! here. It used to declare `BRAND_UNKNOWN`, `PARTNER_UNKNOWN` and
//! `ORG_TIER_UNKNOWN` beside them; **D-239 struck the trio**, and the paragraph
//! below is kept as the record of why, because the shape it describes is one this
//! design set produces repeatedly.
//!
//! [`REGION_UNKNOWN`] is the **price-row** refusal and is genuinely this
//! module's: a row's `region` is a scope-key axis, §2's flow declares the code
//! against that flow, and the remedy is to fix the row. [`TAXONOMY_VALUE_IN_USE`]
//! is the retire guard's and has one owner by construction.
//!
//! The trio was **never raised anywhere in this crate**, and that was a recorded
//! finding rather than an omission. It named the overlay scope-value failure,
//! which `overlay_rules` already reports as `SCOPE_VALUE_UNKNOWN` — declared for
//! the same refusal by D-222 three days before this slice was built, in
//! `design/09-price-overlays.md` §5. Raising both would have made a consumer match
//! on whichever surface happened to answer; raising the trio *instead* would have
//! re-spelled a code Slice 9 shipped. And the trio was three codes for **four**
//! classes: a region-scoped overlay value, which D-120 explicitly brought under
//! the same rule, had a code in neither list.
//!
//! **D-239 split them by surface rather than by class**, which is why this module
//! keeps `REGION_UNKNOWN` and gains nothing: a *price row* naming an undeclared
//! region is a different fact from an *overlay scope* naming an undeclared value —
//! the first is a scope-key axis whose remedy is to fix the row, the second has one
//! remedy for all four classes, and the class rides the violation's subject.

use std::collections::BTreeSet;
use std::fmt;

use toolkit_macros::domain_model;

use crate::domain::concurrency::{PolicyTag, TaxonomyTagEntry};
use crate::domain::overlay::{ScopeClass, ScopeValue};
use crate::domain::plan_shape::PlanShape;
use crate::domain::scope_key::Region;
use crate::domain::validation::{Stage, ValidationReport, ValidationRule, Violation};

/// A price row naming a `region` the tenant's region taxonomy does not declare
/// as `active` (§2, §5, `inst-tx-region` / `inst-mc-region`).
///
/// The **price-row** refusal specifically. An overlay whose `region`-class scope
/// value is undeclared is `overlay_rules::SCOPE_VALUE_UNKNOWN`'s, not this — see
/// the module doc and register entry `T-1`.
pub const REGION_UNKNOWN: &str = "REGION_UNKNOWN";

/// A taxonomy value that cannot retire because something active still names it
/// (§5, 409; `inst-tx-mutation`).
pub const TAXONOMY_VALUE_IN_USE: &str = "TAXONOMY_VALUE_IN_USE";

/// `POST …/taxonomies/{class}/values` naming a value the tenant already declares
/// with **different** content (or a retired one): 409. The value is the
/// resource's natural key, so a second declaration of it is not a create — the
/// remedy is `PATCH` on the value, and the detail says so. A body identical to
/// what is held is **not** this refusal: it is the create's replay (200).
pub const TAXONOMY_VALUE_EXISTS: &str = "TAXONOMY_VALUE_EXISTS";

/// A price row, or a tenant default, naming a rounding reference the tenant's
/// rounding-policy taxonomy does not declare as `active` (D-334).
///
/// Its own code rather than a reuse of [`REGION_UNKNOWN`] for that code's own
/// stated reason: the two name different facts about different axes, and an
/// operator reading a report needs to know which vocabulary to go and fix.
pub const ROUNDING_POLICY_UNKNOWN: &str = "ROUNDING_POLICY_UNKNOWN";

/// A plan's billing-descriptor `glCode` naming a code the tenant's declared
/// GL-code vocabulary does not hold as `active` (D-356).
///
/// Its own code rather than a reuse of [`ROUNDING_POLICY_UNKNOWN`] for that
/// code's own stated reason: an operator reading a report has to know which
/// vocabulary to go and fix, and the two are declared at two routes.
pub const GL_CODE_UNKNOWN: &str = "GL_CODE_UNKNOWN";

/// The four taxonomy classes `GET/PUT /config/taxonomies/{…}` addresses.
///
/// Ordered as §5 and §6 list them. The order carries no ranking — the ranking is
/// [`ScopeClass`]'s derived `Ord` and there is exactly one of those.
#[domain_model]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TaxonomyClass {
    /// Pricing regions: the price-row axis **and** an overlay scope class.
    Region,
    /// Commercial brands: an overlay scope class only — §3 step 2 is explicit
    /// that `brand` is *"**not** a price-row field (Foundation §4.1)"*.
    Brand,
    /// Resellers / partners (D-120).
    Partner,
    /// Organisation tiers (D-120).
    OrgTier,
}

impl TaxonomyClass {
    /// Every addressable class, in §5's order.
    pub const ALL: &'static [Self] = &[Self::Region, Self::Brand, Self::Partner, Self::OrgTier];

    /// The path segment this class is addressed by — **the token its overlay
    /// scope carries** ([`ScopeClass::as_str`], and the `scope_class` column
    /// Slice 9 stores).
    ///
    /// §5 spelled the last one `orgTier`, and this returned that spelling: §5 is
    /// the normative statement of the route, a path segment is not a JSON field,
    /// and no rule of the set required the two to agree. **D-241 closed it the
    /// other way** — an operator meets both in one sitting and an `OpenAPI`-generated
    /// client carries two spellings for one class, which is the cost that decided
    /// it. The gear is not deployed, so nothing routed under the old segment.
    ///
    /// The old spelling is **not** accepted as an alias, deliberately: two
    /// spellings that both route is the state in which neither is canonical, and
    /// `rest_taxonomies::an_unaddressable_class_is_refused_naming_the_four` pins
    /// that `orgTier` is refused rather than routed.
    ///
    /// Because the delegation makes *segment equals scope token* unfalsifiable,
    /// no test asserts it — that would be decoration.
    /// `taxonomy_tests::a_class_is_addressed_by_the_token_the_overlay_plane_stores_for_it`
    /// pins the four literal wire strings instead, which is the part delegation
    /// cannot carry: it catches a drift in [`ScopeClass::as_str`] itself.
    #[must_use]
    pub fn path_segment(self) -> &'static str {
        self.scope_class().as_str()
    }

    /// Parse a path segment back into a class.
    #[must_use]
    pub fn parse_segment(segment: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|class| class.path_segment() == segment)
    }

    /// The overlay scope class this taxonomy declares the values of.
    ///
    /// The conversion is the point: it is what keeps one table map in the crate.
    #[must_use]
    pub const fn scope_class(self) -> ScopeClass {
        match self {
            Self::Region => ScopeClass::Region,
            Self::Brand => ScopeClass::Brand,
            Self::Partner => ScopeClass::Partner,
            Self::OrgTier => ScopeClass::OrgTier,
        }
    }

    /// The physical table, by way of [`ScopeClass::taxonomy_table`].
    ///
    /// Every one of the four classes has one, which is what makes the `None` arm
    /// below unreachable in practice; it is written as a total function rather
    /// than an `Option` because *this* enum's whole domain is the classes that
    /// have a table. The arm fails safe rather than panicking: its sentinel is
    /// only ever interpolated into a `RepoError::CorruptRow` diagnostic, never
    /// into SQL and never compared.
    #[must_use]
    pub const fn table(self) -> &'static str {
        match self.scope_class().taxonomy_table() {
            Some(table) => table,
            // Unreachable: all four narrow classes map to a declared table. A
            // panic here would be a claim about `ScopeClass` that this enum
            // exists to guarantee.
            None => "a taxonomy this class does not declare",
        }
    }

    /// Does this class carry D-01's two `tax_*` markers?
    ///
    /// §6: *"the `tax_*` columns below are region-only"*, and
    /// `sqlite_taxonomy_store::the_other_three_taxonomies_carry_no_tax_columns`
    /// asserts that absence against the schema.
    #[must_use]
    pub const fn carries_tax_markers(self) -> bool {
        matches!(self, Self::Region)
    }
}

impl fmt::Display for TaxonomyClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.path_segment())
    }
}

/// `active | retired` — the whole state machine §6 gives these tables.
#[domain_model]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TaxonomyState {
    /// Declared and usable. The only state that validates anything.
    #[default]
    Active,
    /// Withdrawn from new use. Existing references survive — retirement is
    /// **guarded**, never cascading — but a retired value declares nothing, which
    /// is `overlay_repo::declares`' `state = 'active'` predicate.
    Retired,
}

impl TaxonomyState {
    /// Both states.
    pub const ALL: &'static [Self] = &[Self::Active, Self::Retired];

    /// The stored / wire token.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Retired => "retired",
        }
    }

    /// Parse a stored token.
    #[must_use]
    pub fn parse(token: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|s| s.as_str() == token)
    }
}

impl fmt::Display for TaxonomyState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// D-01's two tenant-declared markers, which live on the region taxonomy alone.
///
/// The MVP `RegionTaxReadiness` source. Absence of the whole struct is an
/// **undeclared region**, which C4 fails closed on; absence of
/// [`RegionTaxMarkers::tax_category`] is a declared region with no default
/// category, which is a different fact and one `inst-td-policy`'s coalesce is
/// about.
#[domain_model]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RegionTaxMarkers {
    /// The region's default tax category. `None` is undeclared, not empty.
    pub tax_category: Option<String>,
    /// Tenant-declared *"a tax rate is configured for this region"*. Defaults to
    /// **false** — the fail-closed reading: a region nobody has declared a rate
    /// for is a region with no rate, not one with an unknown rate.
    pub tax_rate_present: bool,
}

/// One declared value of one taxonomy.
#[domain_model]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaxonomyEntry {
    /// The declared code. Never blank — [`ScopeValue`] refuses one, and the
    /// store spends a `CHECK` on the same rule because the empty string is
    /// `pricing_price_overlay`'s sentinel for the classless scope.
    pub value: ScopeValue,
    /// The operator's label.
    pub display_name: String,
    /// Where the value stands.
    pub state: TaxonomyState,
    /// D-01's markers, on the region taxonomy only. `None` on the other three,
    /// where the columns do not exist.
    pub tax: Option<RegionTaxMarkers>,
}

/// The entity tag of one taxonomy's representation (§5's `ETag` cell).
///
/// **Here rather than in the surface**, because two layers need it and they must
/// not compute it twice. The `GET` renders it, and the `PUT`'s precondition is
/// compared against it **inside the write transaction** — so a second
/// construction would be a second answer to "what does this taxonomy hash to",
/// and the two would disagree exactly when it mattered.
#[must_use]
pub fn tag_of(class: TaxonomyClass, entries: &[TaxonomyEntry]) -> PolicyTag {
    PolicyTag::of_taxonomy(
        class.path_segment(),
        entries.iter().map(|entry| TaxonomyTagEntry {
            value: entry.value.as_str(),
            state: entry.state.as_str(),
            display_name: entry.display_name.as_str(),
            tax_category: entry.tax.as_ref().and_then(|t| t.tax_category.as_deref()),
            tax_rate_present: entry.tax.as_ref().is_some_and(|t| t.tax_rate_present),
        }),
    )
}

/// The entity tag of **one** value's representation — what
/// `GET/PATCH …/taxonomies/{class}/values/{value}` carry and assert.
///
/// The set tag ([`tag_of`]) moves whenever *any* value moves, which is right for
/// a whole-set `PUT` and wrong for an edit of one value: two admins re-labelling
/// two different regions would refuse each other on a set they never disagreed
/// about. This tag digests one entry under the segment `{class}/{value}`, so it
/// moves with that value alone — and cannot collide with a set tag, whose
/// segment is the bare class.
#[must_use]
pub fn tag_of_value(class: TaxonomyClass, entry: &TaxonomyEntry) -> PolicyTag {
    PolicyTag::of_taxonomy(
        &format!("{}/{}", class.path_segment(), entry.value.as_str()),
        std::iter::once(TaxonomyTagEntry {
            value: entry.value.as_str(),
            state: entry.state.as_str(),
            display_name: entry.display_name.as_str(),
            tax_category: entry.tax.as_ref().and_then(|t| t.tax_category.as_deref()),
            tax_rate_present: entry.tax.as_ref().is_some_and(|t| t.tax_rate_present),
        }),
    )
}

/// The three things a `PATCH` can say about a region's default tax category:
/// nothing, **clear** it, or set it. A single `Option` carries two of the three,
/// which is why this is its own type rather than an `Option<Option<_>>`.
/// Clearing is a guarded act (D-245).
#[domain_model]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum TaxCategoryPatch {
    /// Leave the category as held.
    #[default]
    Keep,
    /// Remove the default category (the wire's explicit `null`).
    Clear,
    /// Set the default category.
    Set(String),
}

/// What a `PATCH …/values/{value}` asks to change. Every field is optional and
/// an absent one leaves the held value as it is.
#[domain_model]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TaxonomyValuePatch {
    /// A new label.
    pub display_name: Option<String>,
    /// `active` or `retired`; a retirement is guarded.
    pub state: Option<TaxonomyState>,
    /// **Region only.**
    pub tax_category: TaxCategoryPatch,
    /// **Region only.**
    pub tax_rate_present: Option<bool>,
}

impl TaxonomyValuePatch {
    /// Does this patch touch either region marker?
    #[must_use]
    pub const fn touches_tax_markers(&self) -> bool {
        !matches!(self.tax_category, TaxCategoryPatch::Keep) || self.tax_rate_present.is_some()
    }

    /// The entry as it would stand with this patch applied over `held`.
    ///
    /// Pure: the guards read the result against the store, and the write
    /// happens only once they pass. A patch that names nothing returns `held`
    /// unchanged, which is what lets the caller treat it as the no-op it is.
    #[must_use]
    pub fn apply(&self, held: &TaxonomyEntry) -> TaxonomyEntry {
        let mut next = held.clone();
        if let Some(label) = &self.display_name {
            next.display_name.clone_from(label);
        }
        if let Some(state) = self.state {
            next.state = state;
        }
        if let Some(tax) = next.tax.as_mut() {
            match &self.tax_category {
                TaxCategoryPatch::Keep => {}
                TaxCategoryPatch::Clear => tax.tax_category = None,
                TaxCategoryPatch::Set(category) => tax.tax_category = Some(category.clone()),
            }
            if let Some(present) = self.tax_rate_present {
                tax.tax_rate_present = present;
            }
        }
        next
    }
}

/// One proposed edit of one declared value — the subject a `PATCH …/values/{value}`
/// unit is opened over (D-353).
///
/// Carried whole in the unit's `subject_ref` (as a membership move's set is),
/// because a taxonomy value has no draft table for a pending edit to live in.
#[domain_model]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaxonomyValueProposal {
    /// Which universe.
    pub class: TaxonomyClass,
    /// Which value.
    pub value: ScopeValue,
    /// What changes.
    pub patch: TaxonomyValuePatch,
}

/// A proposal read against the value **as it stands**: the pair a reviewer is
/// shown and the content the unit's hash pins.
///
/// Pinning `held` as well as the proposal is what makes the pin a TOCTOU guard
/// (`inst-ap-pin`): if the value moves under a submitted unit — a relabel, a
/// retirement by another door — the re-derived pair no longer digests to the
/// record's hash, the detail view says so, and the approve is refused.
#[domain_model]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaxonomyValueChange {
    /// The edit.
    pub proposal: TaxonomyValueProposal,
    /// The value the edit was authored against.
    pub held: TaxonomyEntry,
}

impl TaxonomyValueChange {
    /// The value as it would stand once the proposal commits.
    #[must_use]
    pub fn next(&self) -> TaxonomyEntry {
        self.proposal.patch.apply(&self.held)
    }
}

/// The one region every tenant starts with (D-354): `global`.
///
/// The same token the overlay plane stores for the classless scope
/// ([`GLOBAL_SCOPE`](crate::domain::read_model::GLOBAL_SCOPE)), by reference
/// rather than by a second literal: both spell "the whole market", and the
/// domain-code census would otherwise read two rules rendering one string.
pub const SEEDED_REGION: &str = crate::domain::read_model::GLOBAL_SCOPE;

/// The region taxonomy of a tenant that has declared **no** region row yet
/// (D-354): one active value, `global`, labelled `Global`, with no default tax
/// category and no rate declared — C4's fail-closed reading, because the seed
/// asserts no tax fact nobody declared.
///
/// `inst-tx-region` is fail-closed and a fresh tenant declares nothing, so
/// without this a tenant that does not segment its market by territory could
/// publish nothing until an admin invented a region to satisfy a rule about
/// territories it does not have. The seed is **virtual while the tenant holds
/// no row** and is written by the tenant's first region write; afterwards it is
/// an ordinary value — retire it, re-label it, give it markers, every edit
/// governed like any other (D-353).
///
/// # Panics
///
/// Never: the literal is a non-blank value.
#[must_use]
pub fn seeded_region() -> TaxonomyEntry {
    TaxonomyEntry {
        value: ScopeValue::new(SEEDED_REGION)
            .unwrap_or_else(|| unreachable!("`{SEEDED_REGION}` is a non-blank literal")),
        display_name: "Global".to_owned(),
        state: TaxonomyState::Active,
        tax: Some(RegionTaxMarkers::default()),
    }
}

// ---------------------------------------------------------------------------
// `inst-tx-mutation` — the retire guard.
// ---------------------------------------------------------------------------

/// What still names a value the operator is retiring.
///
/// **Two counts and not one**, because §3 step 3 enumerates two referencing
/// shapes with different remedies: *"referenced by an active published price row
/// (`region`) **or** an active `PriceOverlay` scope of any taxonomy-backed
/// class"*. An operator told only that *something* references the value cannot
/// act; told which plane it is on, they can.
///
/// Resolved by the caller, never here — the same purity contract every rule in
/// this crate holds to.
#[domain_model]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ValueReferences {
    /// Published, non-superseded price rows carrying this value as their
    /// `region` axis. Always zero for the three non-`region` classes, whose value
    /// is not a price-row field at all (§3 step 2).
    pub published_price_rows: u64,
    /// Published `PriceOverlay` revisions scoped to this class and value.
    pub active_overlay_scopes: u64,
}

impl ValueReferences {
    /// Does anything name it?
    #[must_use]
    pub const fn any(self) -> bool {
        self.published_price_rows > 0 || self.active_overlay_scopes > 0
    }
}

/// D-355: an edit of a value **nothing published names** is not a governed act.
///
/// The same reasoning that lets a `POST` declare commit at once (D-353) — "a
/// value nothing has published against has moved nothing a second principal
/// could protect" — reaches every edit while the value is unreferenced: a
/// relabel, a retirement, a region's tax markers. [`references_to`]'s **broad**
/// set is the gate: any published price row on the `region` axis or any
/// published overlay scope of the class makes an edit governed; none makes it
/// the operator's alone. The narrower `rows_resolving_category_through` set is
/// D-245's clear guard and is not this question.
///
/// Retirement follows from this rather than adding to it: a referenced value's
/// retirement is **refused** by [`check_retirable`], and an unreferenced one's
/// is **ungoverned** here — so a "governed admissible retirement" is a null
/// case.
///
/// [`references_to`]: crate::infra::storage::repo::taxonomy_repo::references_to
#[must_use]
pub const fn edit_is_governed(references: ValueReferences) -> bool {
    references.any()
}

/// `inst-tx-mutation`: refuse a retirement while the value is referenced.
///
/// D-120 widened this guard and the widening is the whole of why it is written
/// over a class rather than over `region`: *"the guard previously enumerated
/// price rows and brand overlays only, so a region value retired cleanly while
/// region-scoped overlays still named it"*. Every one of the four classes is
/// checked on the overlay plane; only `region` is checked on the row plane,
/// because only `region` is an axis of a price row.
///
/// Answers a [`ValidationReport`] rather than a `Result` so the refusal carries
/// §5's code and a message naming **both** planes' counts — an operator who
/// retires a value referenced by nine overlays and one row has two jobs, and a
/// refusal naming one of them sends them back a second time.
#[must_use]
pub fn check_retirable(
    class: TaxonomyClass,
    value: &ScopeValue,
    references: ValueReferences,
) -> ValidationReport {
    let mut report = ValidationReport::default();
    if !references.any() {
        return report;
    }
    // **Only the planes this class actually has.** The row plane belongs to
    // `region` alone — §3 step 2 is explicit that `brand` is "**not** a price-row
    // field", and the same holds for the two D-120 classes — so `references_to`
    // does not even run that query for them and the count is a structural zero,
    // not a measurement. Rendering it unconditionally told an operator retiring a
    // brand that "0 published price row(s) carry it on their region axis",
    // inviting them to audit a plane their value could never have been on. Found
    // by review.
    let mut named = Vec::with_capacity(2);
    if references.published_price_rows > 0 {
        named.push(format!(
            "{} published price row(s) carry it on their region axis",
            references.published_price_rows
        ));
    }
    if references.active_overlay_scopes > 0 {
        named.push(format!(
            "{} published overlay scope(s) select on it",
            references.active_overlay_scopes
        ));
    }
    report.violate(
        TAXONOMY_VALUE_IN_USE,
        value.as_str(),
        format!(
            "{class} value `{value}` cannot retire: {}; retirement is guarded rather than \
             cascading, so end or retarget every reference first (D-120)",
            named.join(" and ")
        ),
    );
    report
}

/// D-245: a region's `tax_category` marker may not be **removed** while a
/// published row resolves its category through it.
///
/// # Why this is a second guard and not another `ValueReferences` field
///
/// [`check_retirable`] answers *may this value stop existing*. This answers *may
/// this value keep existing with one fewer marker* — a different act, reachable
/// by a `PUT` that retires nothing at all, and with a different remedy: there is
/// nothing to retarget, the operator either re-declares the default or authors
/// the category onto the rows that were leaning on it. Folding it into
/// `ValueReferences` would have made one refusal stand for two acts, and the
/// message could then only name one of them.
///
/// # What it closes, and what it does not
///
/// `TaxBasisComplete` ranges over **every** candidate row, including the
/// `existing_grandfathered` generation that `MarketBasisUniform` beside it
/// excludes under D-132. So an immutable grandfathered row published with
/// `tax_category_ref = NULL` takes the region default, the tenant later drops
/// that default, and every subsequent publish of that plan fails
/// `TAX_BASIS_INCOMPLETE` on a row that MUST NOT be superseded and cannot be
/// `PATCH`ed — a market frozen out of publishing by a config edit.
///
/// D-245 chose this over mirroring D-132's carve-out onto the completeness arm,
/// and the reason is that the two exclusions are not the same fact: D-132
/// excludes a generation whose *basis* is frozen and harmless, whereas a missing
/// *category* is a descriptor element genuinely absent from something still
/// published. It also closes half of `T-13` — the value can no longer vanish from
/// under a category a publish already froze. What it does not close is the value
/// **changing**, handled by freezing the resolved category at
/// the publish commit.
///
/// # The code is `TAXONOMY_VALUE_IN_USE`, and that is a choice worth naming
///
/// §5 declares no code for this refusal, and this crate's standing rule is that a
/// gear mints domain errors freely but **a wire code is the design set's to
/// declare**. `TAXONOMY_VALUE_IN_USE`'s own declaration is *"409, on retire — any
/// referencing shape"*, and a marker something still resolves through is a
/// referencing shape, so this rides it rather than inventing a token ahead of the
/// route. If the set wants the two acts told apart on the wire, that is a
/// D-number and not an implementation detail.
#[must_use]
pub fn check_tax_category_removable(value: &ScopeValue, dependents: u64) -> ValidationReport {
    let mut report = ValidationReport::default();
    if dependents == 0 {
        return report;
    }
    report.violate(
        TAXONOMY_VALUE_IN_USE,
        value.as_str(),
        format!(
            "region `{value}` cannot drop its `taxCategory`: {dependents} published price \
             row(s) state no category of their own and resolve theirs through it (D-154). \
             Re-declare the default, or author `taxCategoryRef` onto those rows first — note \
             that a grandfathered row can do neither, which is why the marker is guarded \
             rather than the publish rule relaxed (D-245)"
        ),
    );
    report
}

// ---------------------------------------------------------------------------
// `inst-tx-region` / `inst-mc-region` — the price-row membership rule.
// ---------------------------------------------------------------------------

/// Every candidate row's `region` is declared `active` in the tenant's region
/// taxonomy (§3 step 1, §2 step 2).
///
/// # Registered, and enforced on both halves
///
/// `domain::publish::rules::foundation_plan_rules` constructs this rule and
/// `infra::publish::rule_params` resolves its universe through
/// `taxonomy_repo::active_regions`, so every publish evaluates it. The **save**
/// half of `inst-mc-region` — "validated at save **and** publish" — is
/// `api::rest::prices`, through [`RegionsDeclared::violation_for`].
///
/// **A doc comment asserting a wiring is the most expensive defect shape this
/// program has met, and it is expensive in both directions.** Claiming the wiring
/// exists when it does not sends a reader past a missing rule; claiming the rule
/// runs on nothing after it is registered tells them `REGION_UNKNOWN` is
/// unreachable while it is blocking publishes. This block has been wrong each
/// way.
///
/// # Why the declared set is a field and not a lookup
///
/// The rule set runs **twice** on one publish — as a pre-check and again inside
/// the commit transaction (Foundation §4.2) — so a rule that fetched its own
/// inputs would be free to answer differently in the two runs for no authored
/// reason. That is `RoundingPolicyResolved`'s arrangement and
/// `ReferencingMarket`'s, and it is why the set is shaped to be resolved once by
/// the caller when the wiring above lands.
///
/// # `active`, and why an empty set is not a special case
///
/// The set the caller resolves carries **active** values only, which is
/// `overlay_repo::declares`' predicate one plane over: a value that reached
/// `retired` anyway must not validate a new row against itself. A tenant that has
/// declared no region at all therefore fails every row, and that is C2's
/// fail-closed reading rather than an edge case — *"membership is validated at
/// save/publish (unknown value fails before publish)"* does not carve out the
/// tenant who configured nothing.
#[domain_model]
#[derive(Debug, Clone)]
pub struct RegionsDeclared {
    /// The tenant's `active` region values, resolved by the caller.
    pub declared: BTreeSet<Region>,
}

impl RegionsDeclared {
    /// The violation this region earns, if any — **the authoring edge's entry
    /// point**.
    ///
    /// `inst-mc-region` validates membership *"at save **and** publish"*, and the
    /// two halves must not be two spellings of one refusal: an operator who meets
    /// it at save and again at publish has to meet one message. So the pipeline
    /// arm below and `api::rest::prices` both come through here.
    ///
    /// Its own entry point rather than an arm of [`ValidationRule::evaluate`]
    /// because the save has **one** region and no `PlanShape` — building a shape
    /// to judge one axis would be inventing a subject to satisfy a signature.
    /// This is `overlay_rules::check_authored_shape`'s arrangement.
    #[must_use]
    pub fn violation_for(&self, region: &Region) -> Option<Violation> {
        if self.declared.contains(region) {
            return None;
        }
        Some(Violation {
            code: REGION_UNKNOWN.to_owned(),
            subject: region.as_str().to_owned(),
            detail: format!(
                "region `{region}` is not an active value of this tenant's region taxonomy; a \
                 price row's region is validated at save and at publish, and an unknown value \
                 fails before publish (C2) — declare it at PUT \
                 /bss-pricing/v1/config/taxonomies/region first"
            ),
            // `Stage::Write` states what this arrangement already does — D-312.
            // The doc above says it: judged at save and again at publish, through
            // one entry point so an operator meets one message. That predates the
            // stage field and is the precedent D-312 generalises; the field makes
            // the property declared rather than implied by two call sites.
            stage: Stage::Write,
        })
    }
}

impl ValidationRule<PlanShape> for RegionsDeclared {
    fn name(&self) -> &'static str {
        "inst-tx-region"
    }

    fn evaluate(&self, subject: &PlanShape, report: &mut ValidationReport) {
        let mut reported: BTreeSet<&Region> = BTreeSet::new();
        for record in &subject.rows {
            let region = record.scope_key.region();
            if self.declared.contains(region) {
                continue;
            }
            // One violation per undeclared **value**, not per row. A plan with
            // forty rows in one bad region is one authoring mistake, and forty
            // copies of it is a report an operator cannot read — and C1's
            // 20-currency floor makes forty a real number rather than a
            // hypothetical.
            if !reported.insert(region) {
                continue;
            }
            if let Some(violation) = self.violation_for(region) {
                report.violations.push(violation);
            }
        }
    }
}

/// Every price row's rounding reference is one the tenant declared (D-334).
///
/// # The vocabulary is the tenant's, and that is the whole design
///
/// This gear persists a rounding reference and neither defines nor applies the
/// policy (PRD §15), so it cannot say whether a given mode is the right rounding
/// rule — and D-320 refused to invent a list.
///
/// **The owner is Billing, and D-321's correction records it**: `bss/ledger` §6.8
/// fixes the scale to the currency's ISO 4217 minor unit and names half-to-even
/// as the platform default, admitting a tenant override only under recorded
/// evidence. It enumerates no override set, which is why membership is checked
/// against what the **tenant** declared rather than against a list invented
/// here. What it *can* say is that
/// a reference names nothing the tenant ever declared, which is
/// [`RegionsDeclared`]'s argument on a different axis.
///
/// The hazard is measured rather than imagined. On the stand the day this
/// landed, one plan's seven rows carried `half_up` while the tenant default
/// carried `half_up_2dp`; both published, both resolved, and the two spellings
/// of one intention were invisible to everything.
///
/// # An empty set means **unconstrained**, and that is the opposite of
/// [`RegionsDeclared`]
///
/// A tenant with no declared regions fails every row, because `region` is a
/// scope-key axis every row carries and C2 requires it to be declared. A tenant
/// with no declared rounding vocabulary is simply a tenant that has not opted
/// in: refusing there would break every catalog on the day the taxonomy landed,
/// for a vocabulary nobody had been given a chance to write. Declaring the first
/// value is what turns the check on, and the asymmetry is stated here because
/// the two rules otherwise read alike.
///
/// # One violation per undeclared **value**
///
/// [`RegionsDeclared`]'s reason, and the same arithmetic: forty rows sharing one
/// bad reference are one authoring mistake, and forty copies of it is a report
/// nobody reads to the end.
#[domain_model]
#[derive(Clone, Debug, Default)]
pub struct RoundingPolicyDeclared {
    /// The tenant's `active` rounding references, resolved by the caller. Empty
    /// means the tenant declared no vocabulary — see the type doc.
    pub declared: BTreeSet<String>,
    /// The tenant's `default_rounding_policy_ref`, which every row carrying none
    /// of its own falls back to.
    ///
    /// **Carried because otherwise the default is judged by nothing.** This rule
    /// walked the rows and skipped every `None` — precisely the set the default
    /// stands in for — while [`RoundingPolicyResolved`] opens with `if
    /// self.tenant_default.is_some() { return; }`, so merely *having* a default
    /// lifted the resolution check off the whole plan. Between the two, a default
    /// naming a value D-334's vocabulary does not contain published everything.
    /// The retirement path meanwhile already treats the default as a first-class
    /// reference and refuses to retire a value it names, so the two ends
    /// disagreed about whether the default is a reference at all.
    ///
    /// [`RoundingPolicyResolved`]: crate::domain::publish::rules::RoundingPolicyResolved
    pub tenant_default: Option<String>,
}

impl RoundingPolicyDeclared {
    /// The violation this reference earns, if any.
    ///
    /// # The two callers, and which door each is
    ///
    /// 1. [`Self::evaluate`], the publish-pipeline registration, over a whole
    ///    [`PlanShape`].
    /// 2. **`api::rest::rounding_policy`'s tenant-default config write** (D-348),
    ///    which builds the rule from a fresh `active_rounding_policies`
    ///    read and calls `violation_for("defaultRoundingPolicyRef", reference)`
    ///    before the store sees it — `api::rest::prices::require_declared_region`'s
    ///    arrangement, one taxonomy over, including its empty-set reading.
    ///
    /// *(D-348 took the write-door decision and the door is mounted, so this is not
    /// "left for a decision rather than taken here" and `Self::evaluate` is not the
    /// only caller. Either claim would justify the `Stage` stamp below and leaving
    /// the symmetry with [`RegionsDeclared::violation_for`] unfixed, which is how a
    /// reader re-takes a decision already taken and adds a second door.)*
    ///
    /// **What is still asymmetric** is the *price* write: `RegionsDeclared` is
    /// consulted at `POST …/prices`, and nothing consults this rule there — a
    /// `rounding_policy_ref` on a price row is still first judged at publish.
    ///
    /// It also stamped its violation [`Stage::Write`], which nothing reads.
    /// `write_stage_only()` is the sole reader of a violation's stage, and its
    /// three callers filter `price_row_rules()`, `price_row_rules()` again and a
    /// hand-built phase report — never `foundation_plan_rules`, where this rule is
    /// registered. So the stamp asserted that some door judges this fault at the
    /// write, and no door does. It is gone rather than corrected in prose: a
    /// marker nobody reads is what makes the next author trust `validation.rs`'s
    /// census.
    ///
    /// `RegionsDeclared` keeps its stamp, and the asymmetry is the point: there
    /// the price-write door exists, so the claim is true even though nothing reads
    /// the field to act on it.
    ///
    /// **D-348's door does not change that, and the re-decision is that the stamp
    /// stays `Stage::Publish`.** A stage is a claim about which door of the
    /// *plan/price* pipeline judges a fault, and `write_stage_only()` — the sole
    /// reader — is only ever applied to `price_row_rules()` and a hand-built phase
    /// report. The config-surface door calls `violation_for` directly and converts
    /// the detail to a `DomainError`; it never looks at `stage`. So stamping
    /// `Stage::Write` would assert an authoring door that still does not exist for
    /// this rule, which is exactly what review B5-2 removed. The fix that would
    /// make the pair symmetric is still a `require_declared_rounding_policy`
    /// beside `require_declared_region` on the **price** write, and that remains a
    /// behaviour change for a decision.
    #[must_use]
    pub fn violation_for(&self, subject: &str, reference: &str) -> Option<Violation> {
        if self.declared.is_empty() || self.declared.contains(reference) {
            return None;
        }
        Some(Violation {
            code: ROUNDING_POLICY_UNKNOWN.to_owned(),
            subject: subject.to_owned(),
            detail: format!(
                "rounding policy `{reference}` is not an active value of this tenant's \
                 rounding-policy taxonomy; rounding decides the last minor unit of every charge, \
                 so a reference to something nobody declared is refused - declare it at PUT \
                 /bss-pricing/v1/config/rounding-policies first, or clear the taxonomy to stop \
                 constraining references at all"
            ),
            // `Stage::Publish`, which is where this fault is actually judged.
            // D-312's criterion is arguably met for `Stage::Write` — but a stage
            // is a claim about a door, and this rule has only the publish door.
            // See the method doc.
            stage: Stage::Publish,
        })
    }
}

impl ValidationRule<PlanShape> for RoundingPolicyDeclared {
    fn name(&self) -> &'static str {
        "inst-tx-rounding"
    }

    fn evaluate(&self, subject: &PlanShape, report: &mut ValidationReport) {
        if self.declared.is_empty() {
            return;
        }
        let mut reported: BTreeSet<&str> = BTreeSet::new();
        // The rows that lean on the tenant default, judged once for the whole
        // plan: the remediation is a single act on the tenant's config, not an
        // edit to every row that names nothing.
        if let Some(default) = self.tenant_default.as_deref()
            && subject
                .rows
                .iter()
                .any(|record| record.rounding_policy_ref.is_none())
            && !self.declared.contains(default)
            && reported.insert(default)
        {
            report.violations.push(Violation {
                code: ROUNDING_POLICY_UNKNOWN.to_owned(),
                subject: subject.subject(),
                detail: format!(
                    "this tenant's default rounding policy `{default}` is not an active value \
                     of its own rounding-policy taxonomy, and rows in this plan carry no \
                     policy of their own, so they resolve to it; declare it at PUT \
                     /bss-pricing/v1/config/rounding-policies, change the default, or give \
                     those rows a policy of their own"
                ),
                stage: Stage::Publish,
            });
        }
        for record in &subject.rows {
            let Some(reference) = record.rounding_policy_ref.as_deref() else {
                continue;
            };
            if self.declared.contains(reference) || !reported.insert(reference) {
                continue;
            }
            if let Some(violation) = self.violation_for(&record.price_id.to_string(), reference) {
                report.violations.push(violation);
            }
        }
    }
}

/// `inst-ds-glcode` — the plan's billing-descriptor `glCode` names a code the
/// tenant declared (D-356).
///
/// [`RoundingPolicyDeclared`]'s argument on the descriptor plane. A GL code
/// freezes into every `CatalogVersion` an ERP posts against, and before D-356
/// publish checked only that one was *present* (`inst-ds-required`), so a typo'd
/// or non-existent code froze into an immutable seven-year version and was met
/// at posting time rather than by its author. This gear cannot say what
/// `4000-REV` means — ledger's own PRD stores account *class* and takes the
/// concrete code from the Catalog snapshot, so there is no list anywhere in BSS
/// to defer to — but it can say that a code names nothing the tenant ever
/// declared.
///
/// # One value per revision, not a row walk
///
/// `glCode` is a single field on the plan's descriptor set, so this rule reads
/// `descriptor_set.gl_code` once and reports at most once. It judges a
/// **present** value only: an absent or blank code is `inst-ds-required`'s
/// finding, and a second report of the same absence would send the author to a
/// vocabulary for a field they have not filled in.
///
/// # An empty set means **unconstrained** — the opt-in reading
///
/// [`RoundingPolicyDeclared`]'s asymmetry with [`RegionsDeclared`], for the same
/// reason: a tenant with no declared GL codes has not opted in, and refusing
/// there would have failed every existing plan on the day the table landed, for
/// a vocabulary nobody had been given a chance to write. Declaring the first
/// value is what turns the check on.
///
/// # The provider seam
///
/// `declared` is resolved by the caller from `taxonomy_repo::active_gl_codes`,
/// and this rule never learns where the set came from. Today the provider is the
/// tenant-declared set behind `PUT /bss-pricing/v1/config/gl-codes`; a future
/// ERP gear (D-356 *Owed*) populates or reconciles that same table, and nothing
/// on this side of the seam changes — exactly `RegionTaxReadiness`'s arrangement
/// under D-01, tenant-declared today and reconciled against Tax Engine post-GA.
///
/// # One door
///
/// Only the publish path consults this rule. The descriptor **write**
/// (`PUT …/descriptors`) does not, which is the asymmetry
/// [`RoundingPolicyDeclared::violation_for`] records for the price write: a
/// `glCode` is first judged at publish, and a write-door check is a behaviour
/// change for a decision. The violation is stamped [`Stage::Publish`] for the
/// reason that doc gives — a stage is a claim about which door judges a fault.
#[domain_model]
#[derive(Clone, Debug, Default)]
pub struct GlCodeDeclared {
    /// The tenant's `active` GL codes, resolved by the caller. Empty means the
    /// tenant declared no vocabulary — see the type doc.
    pub declared: BTreeSet<String>,
}

impl GlCodeDeclared {
    /// The violation this code earns, if any.
    ///
    /// `subject` is the violation's subject — the plan, from [`Self::evaluate`].
    /// Public with the subject as a parameter, as [`RoundingPolicyDeclared::violation_for`]
    /// is, so that a write door added later judges the same fault with the same
    /// words rather than a second spelling of it.
    #[must_use]
    pub fn violation_for(&self, subject: &str, code: &str) -> Option<Violation> {
        if self.declared.is_empty() || self.declared.contains(code) {
            return None;
        }
        Some(Violation {
            code: GL_CODE_UNKNOWN.to_owned(),
            subject: subject.to_owned(),
            detail: format!(
                "glCode `{code}` is not an active value of this tenant's declared GL-code \
                 vocabulary; the code freezes into the catalog version an ERP posts against, so \
                 a reference to something nobody declared is refused - declare it at PUT \
                 /bss-pricing/v1/config/gl-codes first, correct the descriptor, or clear the \
                 vocabulary to stop constraining codes at all"
            ),
            stage: Stage::Publish,
        })
    }
}

impl ValidationRule<PlanShape> for GlCodeDeclared {
    fn name(&self) -> &'static str {
        "inst-ds-glcode"
    }

    fn evaluate(&self, subject: &PlanShape, report: &mut ValidationReport) {
        if self.declared.is_empty() {
            return;
        }
        // A blank is an absence wearing a value's shape — `inst-ds-required`'s
        // reading, and its finding.
        let Some(code) = subject
            .descriptor_set
            .as_ref()
            .and_then(|set| set.gl_code.as_deref())
            .filter(|code| !code.trim().is_empty())
        else {
            return;
        };
        if let Some(violation) = self.violation_for(&subject.subject(), code) {
            report.violations.push(violation);
        }
    }
}

#[cfg(test)]
#[path = "taxonomy_tests.rs"]
mod taxonomy_tests;
