//! The **canonical scope key** and its ten axes.
//!
//! One key serves four jobs at once (`design/01-foundation.md` §4.1): row
//! uniqueness, supersession scoping, `PriceWindow` non-overlap, and window
//! ownership. That is why it is a type rather than a tuple assembled per call
//! site — every one of those four rules has to agree, to the axis, about what
//! "the same key" means.
//!
//! ```text
//! (planId, currency, region, priceOverlay, phase, priceEligibility, chargeKind,
//!  cohort, meter, dimensionKey)
//! ```
//!
//! It extends the manifest's `(plan, currency, region, priceOverlay)` key
//! **additively** with `phase`, `priceEligibility`, `chargeKind` (ADR
//! `cpt-cf-bss-pricing-adr-canonical-scope-key`) and `cohort` (ADR
//! `cpt-cf-bss-pricing-adr-grandfathering-cohort-axis`), and supersedes it for
//! normative purposes.
//!
//! Axis defaults: `priceOverlay = base`, `priceEligibility = all_subscriptions`,
//! `cohort = none`. `phase` also has a default — the plan's terminal phase id —
//! but it is **data, not a constant**, so it is not a `Default` impl on
//! [`PhaseId`]; see that type.
//!
//! # The ninth and tenth axes (D-196), and they are built
//!
//! **D-196 is the product owner's decision and all four clauses
//! are paid.** The canonical scope key carries `(meter, dimensionKey)` **on
//! `chargeKind = 'usage'` rows**, because every usage line of one plan in one
//! market otherwise rendered one key and the second was refused
//! `DUPLICATE_SCOPE_KEY` at save — which made D-103's confirmed multi-meter plan
//! unstorable while the meter-line index, `MeterInjectivity` and
//! `plan_rules::cycle_shape_tests` all assumed it storable.
//!
//! The pairing rule is an implication and not a biconditional — a meter implies
//! `usage`, while a usage row with no meter stays admissible ([`ScopeKey::new`]
//! returns exactly that, and [`ScopeKey::with_usage_line`] is never needed to
//! omit the pair). [`fmt::Display`] keeps **fixed arity at ten segments**, `none`
//! filling both usage positions on a key that has no line. The physical half is
//! §3.7's: `meter` is nullable and NULLs are distinct inside a `UNIQUE`, so the
//! two scope-key indexes key over `COALESCE(meter, '')` — measured rather than
//! assumed, because the naive spelling *destroys* the uniqueness every non-usage
//! key had.
//!
//! **A module doc that describes the state of the work is load-bearing and goes
//! stale in the same commit that finishes it.** This paragraph is the instruction a
//! reader consults before deciding whether a neighbouring "eight axes" needs
//! revisiting: while it reads as unbuilt, every such site reads as correct.
//! `scope_key_columns`, `content_pin::put_scope_key`, `sellability::siblings` and
//! `ScopeKeyView` each had to be found separately, by four different routes.

use std::fmt;

use toolkit_macros::domain_model;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::instant;
use crate::domain::instant::timestamp_millis;
use crate::domain::money::CurrencyCode;
use crate::domain::validation::ValidationReport;
use time::OffsetDateTime;

/// Rule code for a `cohort` / `priceEligibility` disagreement.
///
/// The design set states the rule (`design/01-foundation.md` §4.1) without
/// naming a code; this is the machine-readable discriminator the gear reports
/// it under, in the shape the rest of the code set uses.
pub const COHORT_ELIGIBILITY_MISMATCH: &str = "COHORT_ELIGIBILITY_MISMATCH";

/// Rule code for a usage-line axis pair on a charge kind that cannot carry it,
/// or a dimension key with no meter (D-196).
///
/// Same shape and same reason as [`COHORT_ELIGIBILITY_MISMATCH`]: the design set
/// states the rule (`design/01-foundation.md` §4.1) without naming a code, and a
/// publish-blocking rule with no code cannot be reported to the operator who has
/// to fix it.
pub const USAGE_LINE_AXIS_MISMATCH: &str = "USAGE_LINE_AXIS_MISMATCH";

/// The one character the canonical rendering reserves.
///
/// [`fmt::Display`] joins the ten axes with this and nothing escapes it, so an
/// axis value carrying one renders a string that is **also** the rendering of a
/// different key: `Meter("p|q") + DimensionKey("r")` and
/// `Meter("p") + DimensionKey("q|r")` were one string and two keys, and the store
/// holds both — the scope-key UNIQUE indexes key over the separate columns
/// (`pricing_price`) and no `CHECK` forbids the character in either.
///
/// **Refused at the axis rather than escaped in the rendering**, because the
/// rendering is embedded in places that read it back as a whole: it is the
/// `DUPLICATE_SCOPE_KEY` message an operator acts on, the approval register's
/// held-key row (`infra::grandfather::refuse_held_key` compares by rendering),
/// `unit_request_id`'s cross-tenant registry idempotency key, and
/// `infra::publish::unit_row_set`'s key identity — whose own comment says "two
/// equal renderings are one key by construction". Escaping would keep those four
/// correct only for as long as every one of them escaped identically; refusing at
/// the three free-form constructors leaves every already-stored rendering
/// byte-for-byte what it was.
///
/// It is **half** of what makes those four correct. The other half is
/// [`ABSENT_AXIS_TOKEN`]: a free-form value equal to the string an absent axis
/// renders as collides with the absent axis rather than with a sibling one.
///
/// The seven other axes cannot carry it: a uuid, a three-letter currency and four
/// closed token enums.
pub const KEY_SEPARATOR: char = '|';

/// Refuse an axis value carrying [`KEY_SEPARATOR`].
///
/// # Errors
///
/// [`DomainError::InvalidRequest`] naming the axis and the character.
fn check_no_separator(axis: &str, value: &str) -> Result<(), DomainError> {
    if value.contains(KEY_SEPARATOR) {
        return Err(DomainError::InvalidRequest(format!(
            "{axis} axis value must not contain `{KEY_SEPARATOR}`: it is the canonical \
             scope key's segment separator, so a value carrying one renders the same \
             string as a different key"
        )));
    }
    Ok(())
}

/// The string the canonical rendering writes where an axis is absent.
///
/// [`KEY_SEPARATOR`]'s sibling, and the other half of the same premise. The
/// rendering has fixed arity (D-196), so an absent ninth or tenth axis is filled
/// with this token rather than left empty — and a free-form value **equal** to it
/// therefore renders the string the absent axis renders. `Meter("none")` on an
/// undimensioned line and no meter at all are two keys, held apart in the store by
/// separate columns (`COALESCE(meter, '')` in `pricing_price`'s scope-key
/// indexes), that render one string. The tenth axis carries the same collision:
/// [`DimensionKey`] is total and renders its empty value as this token too.
///
/// Refused at the axis rather than escaped or re-spelled in the rendering, for the
/// reason [`KEY_SEPARATOR`] gives: the same four surfaces read the rendering back
/// as the key's identity. Changing the token to something unauthorable would move
/// every rendering already embedded in an approval register row, a
/// `DUPLICATE_SCOPE_KEY` message and a `unit_request_id`.
///
/// Exactly two axes can collide with it. `region` is free-form and mandatory, so
/// it has no absent form; the seven others are a uuid, a three-letter currency and
/// four closed token enums, and `Cohort::Generation` renders epoch milliseconds.
pub const ABSENT_AXIS_TOKEN: &str = "none";

/// Refuse a free-form axis value that renders as [`ABSENT_AXIS_TOKEN`].
///
/// # Errors
///
/// [`DomainError::InvalidRequest`] naming the axis and the token.
fn check_not_absent_token(axis: &str, value: &str) -> Result<(), DomainError> {
    if value == ABSENT_AXIS_TOKEN {
        return Err(DomainError::InvalidRequest(format!(
            "{axis} axis value must not be `{ABSENT_AXIS_TOKEN}`: it is what the canonical \
             scope key renders where this axis is absent, so a row carrying it renders the \
             same string as the row that has no {axis}"
        )));
    }
    Ok(())
}

/// A plan identifier — the first axis, and the aggregate every other axis
/// discriminates within.
#[domain_model]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PlanId(Uuid);

impl PlanId {
    /// Wrap a plan id.
    #[must_use]
    pub const fn new(id: Uuid) -> Self {
        Self(id)
    }

    /// The underlying uuid.
    #[must_use]
    pub const fn get(self) -> Uuid {
        self.0
    }
}

impl fmt::Display for PlanId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A phase identifier — the `phase` axis, **always id-typed** (D-19).
///
/// The axis was once a union of per-plan uuids and one reserved literal
/// `evergreen`; D-19 collapsed it to a uuid in every case. A phased plan's rows
/// carry its authored terminal `phase_id`; a non-phased or one-time plan gets
/// **one implicit terminal phase row** (kind `evergreen`) auto-created at plan
/// creation, and its id is the default. The literal `evergreen` survives only
/// as the phase *kind*, never as a value of this axis.
///
/// There is deliberately **no `Default`**. The default phase is *the plan's*
/// terminal phase id, which is per-plan data; a nil-uuid default would be a
/// phase id resolving to no phase row at all, and phase coverage, supersession
/// and duplicate-key comparisons would all silently compare rows against a
/// phase nobody authored.
#[domain_model]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PhaseId(Uuid);

impl PhaseId {
    /// Wrap a phase id.
    #[must_use]
    pub const fn new(id: Uuid) -> Self {
        Self(id)
    }

    /// The underlying uuid.
    #[must_use]
    pub const fn get(self) -> Uuid {
        self.0
    }
}

impl fmt::Display for PhaseId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A pricing region — the market axis, validated against the tenant region
/// taxonomy by Slice 4, not here.
#[domain_model]
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Region(String);

impl Region {
    /// Wrap a region value.
    ///
    /// # Errors
    ///
    /// [`DomainError::InvalidRequest`] when the value is blank — an empty axis
    /// value is not "no region", it is a key component that cannot be compared —
    /// or when it carries [`KEY_SEPARATOR`]; see that constant.
    pub fn new(value: &str) -> Result<Self, DomainError> {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            return Err(DomainError::InvalidRequest(
                "region axis value must not be blank".to_owned(),
            ));
        }
        check_no_separator("region", trimmed)?;
        Ok(Self(trimmed.to_owned()))
    }

    /// The region value.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Region {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The `priceOverlay` axis.
///
/// Rows authored by this gear always carry [`PriceOverlay::Base`], which is why
/// that is the only variant. Partner / orgTier / brand overlays are **separate
/// `PriceOverlay` rows** with their own scope, precedence and dating, joined to
/// base rows by Tariffs at evaluation — they are never a value of this axis on
/// an authored price row. The axis stays in the key because the manifest key it
/// extends carries it and because a future non-base authored row would have to
/// be a distinct key, not an overwrite.
#[domain_model]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PriceOverlay {
    /// The base price plane — every row this gear authors.
    #[default]
    Base,
}

impl PriceOverlay {
    /// The persisted / wire token.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Base => "base",
        }
    }
}

impl fmt::Display for PriceOverlay {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The `priceEligibility` axis: which subscriptions a row may price.
///
/// Three classes, not two. When more than one holds an active window on the
/// same remaining axes, Tariffs selects **most-specific-wins** —
/// `existing_grandfathered` > `new_subscriptions_only` > `all_subscriptions`
/// (PRD §1.4, `design/07-pricewindow-linkage.md` W3). The variants are declared
/// in that order **reversed**, least specific first, so the derived [`Ord`]
/// ranks them exactly as the class order does and no later code can build a
/// second, disagreeing ranking out of this type.
#[domain_model]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PriceEligibility {
    /// Every subscription on the key. The default, and the class a routine
    /// supersession stays inside.
    #[default]
    AllSubscriptions,
    /// Priced only for subscriptions created on or after the row's cutover.
    /// Existing subscriptions are never re-bound to it and keep their prior
    /// snapshot (PRD AC #59). Like [`PriceEligibility::AllSubscriptions`] and
    /// unlike a grandfathered generation it carries `cohort = none`: the cohort
    /// axis discriminates *retained* generations, and this class retains
    /// nobody.
    NewSubscriptionsOnly,
    /// A grandfathered generation: an immutable copy retained for subscribers
    /// who were on the key when a cutover closed it. Always paired with a
    /// non-`none` [`Cohort`].
    ExistingGrandfathered,
}

impl PriceEligibility {
    /// The persisted / wire token.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AllSubscriptions => "all_subscriptions",
            Self::NewSubscriptionsOnly => "new_subscriptions_only",
            Self::ExistingGrandfathered => "existing_grandfathered",
        }
    }
}

impl fmt::Display for PriceEligibility {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The `chargeKind` axis: which component of the plan a row prices.
///
/// It is in the key because a single plan legitimately carries several
/// components **at once**: a hybrid plan holds a `recurring` **and** a `usage`
/// row (optionally a `one_time_setup` row) on one `planId`, all on the same
/// currency, region and phase. Without this axis those rows would collide on
/// the duplicate-key index and the second one would be rejected as a duplicate
/// of the first. With it they are **distinct keys**, each with its own windows
/// and its own supersession chain.
#[domain_model]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ChargeKind {
    /// A recurring subscription charge.
    Recurring,
    /// A metered usage charge.
    Usage,
    /// A one-off charge that is not a setup fee.
    OneTime,
    /// A one-off setup fee, distinguished from [`ChargeKind::OneTime`] so a
    /// hybrid plan can carry both without them colliding on one key.
    OneTimeSetup,
}

impl ChargeKind {
    /// The persisted / wire token.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Recurring => "recurring",
            Self::Usage => "usage",
            Self::OneTime => "one_time",
            Self::OneTimeSetup => "one_time_setup",
        }
    }

    /// [`Self::as_str`]'s inverse, for the one caller that meets the kind as a
    /// stored string rather than as a scope key.
    ///
    /// `infra::synthesis` renders a `migrated-origin` row from `price::Model`,
    /// which carries `charge_kind` as text; without this it could not reach
    /// `contracts::published_billing_timing` and rendered the raw column instead
    /// — a second reading of a rule that has exactly one. Written as the inverse
    /// of the match above so the two cannot drift: a token added to one arm is a
    /// compile error in the other.
    #[must_use]
    pub fn parse(token: &str) -> Option<Self> {
        [
            Self::Recurring,
            Self::Usage,
            Self::OneTime,
            Self::OneTimeSetup,
        ]
        .into_iter()
        .find(|kind| kind.as_str() == token)
    }
}

impl fmt::Display for ChargeKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The `cohort` axis: the grandfathering **generation** discriminator.
///
/// It is in the key because grandfathering is repeatable. Every cutover creates
/// a **new** generation on **its own key**, identified by the UTC instant the
/// cutover happened. Repricing a key three times with per-cohort retention
/// therefore produces three retained generations on three distinct keys, each
/// with its own window — so their windows can all be active at once without
/// ever violating the non-overlap rule, which is enforced per key. Were the
/// cohort not in the key, the second cutover would land on the first
/// generation's key and either be rejected as a duplicate or overwrite a row
/// that some subscriber is still priced from.
#[domain_model]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Cohort {
    /// Not a grandfathered row. The default.
    #[default]
    None,
    /// A grandfathered generation, stamped with the UTC cutover instant that
    /// created it. Tariffs selects within the `existing_grandfathered` class by
    /// matching this instant.
    Generation(OffsetDateTime),
}

impl Cohort {
    /// Is this the classless (non-grandfathered) axis value?
    #[must_use]
    pub const fn is_none(self) -> bool {
        matches!(self, Self::None)
    }

    /// The cutover instant, when there is one.
    #[must_use]
    pub fn generation(self) -> Option<OffsetDateTime> {
        match self {
            Self::None => Option::None,
            Self::Generation(at) => Some(at),
        }
    }
}

impl fmt::Display for Cohort {
    /// Epoch milliseconds, and **lossless** because of it: [`ScopeKey::new`]
    /// refuses a generation below the quantum (D-144), so this rendering can
    /// never be the place an instant quietly loses precision on its way into a
    /// key that is then matched for equality.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::None => f.write_str("none"),
            Self::Generation(at) => write!(f, "{}", timestamp_millis(*at)),
        }
    }
}

/// The `cohort` / `priceEligibility` biconditional:
/// **`cohort != none` if and only if `priceEligibility == existing_grandfathered`**.
///
/// Both directions matter and they fail differently. A cohort on a
/// non-grandfathered row mints a key no resolution class ever selects, so the
/// row is published and never priced from. A grandfathered row without a cohort
/// lands on the `all_subscriptions` successor's own key, where it is either
/// rejected as a duplicate or — worse — becomes an immutable row occupying the
/// key the next reprice needs.
///
/// It is a **checked function** rather than a type-level guarantee because the
/// two axes are separate key columns: they are read back from the database as
/// two independent values, so the pairing has to be re-established on every
/// rehydration, not only at first construction. This is the entry point that
/// path calls; [`ScopeKey::new`] calls it too.
///
/// The rejection is carried as a [`DomainError::ValidationFailed`] envelope
/// with one violation rather than a bespoke variant, so the rule has exactly
/// one representation whether it is hit here or as a rule registered in the
/// publish pipeline — the error mapping renders the code either way, and the
/// taxonomy does not grow a variant per rule.
///
/// # Errors
///
/// [`DomainError::ValidationFailed`] carrying a single
/// [`COHORT_ELIGIBILITY_MISMATCH`] violation when the two axes disagree.
pub fn check_cohort_eligibility(
    price_eligibility: PriceEligibility,
    cohort: Cohort,
) -> Result<(), DomainError> {
    let grandfathered = matches!(price_eligibility, PriceEligibility::ExistingGrandfathered);
    let generational = !cohort.is_none();
    if grandfathered == generational {
        return Ok(());
    }
    let mut report = ValidationReport::default();
    report.violate(
        COHORT_ELIGIBILITY_MISMATCH,
        format!("{price_eligibility}/{cohort}"),
        "cohort is set if and only if priceEligibility is existing_grandfathered",
    );
    Err(DomainError::ValidationFailed(report))
}

/// A published metering unit — the ninth axis, on usage rows only (D-196).
///
/// Blank is refused, and that refusal is load-bearing rather than tidy: the
/// store's two scope-key indexes key over `COALESCE(meter, '')`, so the empty
/// string is the sentinel meaning *no meter*. A blank meter would land on the
/// meterless line's key instead of minting its own.
#[domain_model]
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Meter(String);

impl Meter {
    /// The axis spelling of a raw authored value — [`Meter::new`]'s trim with
    /// none of its refusal.
    ///
    /// It exists because the trim is **not** only this type's business: the
    /// `meter` column is both this axis and [`crate::domain::price_row::PriceRow`]'s
    /// own field, so the row's copy has to be normalized the same way or the two
    /// disagree by whitespace and every gate over the key compares one against
    /// the other. [`crate::domain::price_record::canonical_usage_line`] is that
    /// normalization and this is the one trim it spends, so there is exactly one
    /// statement in the crate of what an axis value's spelling is.
    ///
    /// Total, deliberately: it is applied to values that are not legal meters
    /// (the blank one), and refusing there would make the normalization unable to
    /// run before the refusal that judges it.
    #[must_use]
    pub fn normalized(value: &str) -> &str {
        value.trim()
    }

    /// Wrap a metering unit.
    ///
    /// # Errors
    ///
    /// [`DomainError::InvalidRequest`] when the value is blank — see the type
    /// doc: blank is the store's "no meter" sentinel, not a meter — or when it
    /// carries [`KEY_SEPARATOR`], or when it is [`ABSENT_AXIS_TOKEN`]; see those
    /// two constants.
    pub fn new(value: &str) -> Result<Self, DomainError> {
        let trimmed = Self::normalized(value);
        if trimmed.is_empty() {
            return Err(DomainError::InvalidRequest(
                "meter axis value must not be blank".to_owned(),
            ));
        }
        check_no_separator("meter", trimmed)?;
        check_not_absent_token("meter", trimmed)?;
        Ok(Self(trimmed.to_owned()))
    }

    /// The metering unit.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Meter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The dimension discriminator on a `(meter, dimensionKey)` line — the tenth
/// axis (D-196).
///
/// Absent is the *ordinary* usage line rather than an exceptional one, which is
/// why this is a total type with a `none` value and not an `Option`: the column
/// is `NOT NULL DEFAULT ''` for the same reason, and an undimensioned line has
/// to compare equal to itself across a rehydration.
#[domain_model]
#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DimensionKey(String);

impl DimensionKey {
    /// The undimensioned line — the empty-tuple sentinel the column defaults to.
    #[must_use]
    pub const fn none() -> Self {
        Self(String::new())
    }

    /// Wrap a dimension key; a blank value **is** [`Self::none`] rather than an
    /// error, because "no dimension" is the ordinary case and every caller that
    /// reads the column back gets `''` for it.
    ///
    /// **Total, so [`KEY_SEPARATOR`] is refused one door up** — by
    /// [`ScopeKey::with_usage_line`], the only way this value becomes the tenth
    /// axis of a key. Refusing here would make this fallible, and it is also the
    /// normalization `price_record::canonical_usage_line` spends on the *column*;
    /// a normalization that cannot run before the refusal that judges it is the
    /// shape [`Meter::normalized`] exists to avoid.
    #[must_use]
    pub fn new(value: &str) -> Self {
        Self(value.trim().to_owned())
    }

    /// Is this the undimensioned line?
    #[must_use]
    pub fn is_none(&self) -> bool {
        self.0.is_empty()
    }

    /// The dimension key as stored — `''` when undimensioned.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for DimensionKey {
    /// `none` when undimensioned, for [`Cohort`]'s reason: an empty segment
    /// between two separators cannot be told from a rendering bug.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0.is_empty() {
            f.write_str("none")
        } else {
            f.write_str(&self.0)
        }
    }
}

/// The usage-line implication:
/// **`meter` or `dimensionKey` set ⇒ `chargeKind = usage`** (D-196).
///
/// It is an implication and deliberately **not** a biconditional, which is what
/// separates it from [`check_cohort_eligibility`] next door. The converse — a
/// usage row must carry a meter — is a rule this gear cannot make: the
/// meter/usage-type binding (`inst-cmp-usagetype`) is registry-dependent and
/// deferred, so a usage row with no meter is a shape the authoring plane admits
/// today, and asserting the converse here would refuse rows every other door
/// accepts.
///
/// **A dimension key without a meter is refused even on a usage row** (the
/// amendment is recorded on D-196): a dimension discriminates the dimensions *of*
/// a meter, so with no
/// meter it names nothing — and it would hand the store a second key for one
/// meterless usage line, which is exactly the duplicate this axis pair exists to
/// prevent.
///
/// It is a **checked function** rather than a type-level guarantee for
/// [`check_cohort_eligibility`]'s reason: the axes are separate columns, read
/// back as independent values, so the pairing has to be re-established on every
/// rehydration and not only at first construction.
///
/// # Errors
///
/// [`DomainError::ValidationFailed`] carrying a single
/// [`USAGE_LINE_AXIS_MISMATCH`] violation when the axes and the charge kind
/// disagree.
pub fn check_usage_line_axes(
    charge_kind: ChargeKind,
    meter: Option<&Meter>,
    dimension_key: &DimensionKey,
) -> Result<(), DomainError> {
    let metered = matches!(charge_kind, ChargeKind::Usage);
    let subject = format!(
        "{charge_kind}/{}/{dimension_key}",
        meter.map_or("none", Meter::as_str)
    );
    let mut report = ValidationReport::default();
    if !metered && (meter.is_some() || !dimension_key.is_none()) {
        report.violate(
            USAGE_LINE_AXIS_MISMATCH,
            subject,
            "meter and dimensionKey are axes of a usage row; a non-usage charge kind carries \
             neither",
        );
        return Err(DomainError::ValidationFailed(report));
    }
    if meter.is_none() && !dimension_key.is_none() {
        report.violate(
            USAGE_LINE_AXIS_MISMATCH,
            subject,
            "a dimensionKey discriminates the dimensions of a meter; without a meter it names \
             nothing",
        );
        return Err(DomainError::ValidationFailed(report));
    }
    Ok(())
}

/// The canonical scope key.
///
/// Fields are private and the constructor validates, so a key that violates the
/// cohort biconditional cannot be handed to the duplicate-key index or the
/// window rules. Axis order below is the normative order and is the order
/// [`fmt::Display`] renders in.
#[domain_model]
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ScopeKey {
    plan_id: PlanId,
    currency: CurrencyCode,
    region: Region,
    price_overlay: PriceOverlay,
    phase: PhaseId,
    price_eligibility: PriceEligibility,
    charge_kind: ChargeKind,
    cohort: Cohort,
    meter: Option<Meter>,
    dimension_key: DimensionKey,
}

/// Every axis of a [`ScopeKey`], borrowed — the shape that makes "all ten axes"
/// a compile-time obligation at a call site instead of a count somebody has to
/// re-check.
///
/// # Why this exists
///
/// The fields of [`ScopeKey`] are private, so the exhaustive `let Self {.. }`
/// that [`ScopeKey::is_sibling_of`] and [`ScopeKey::to_generation`] use is
/// available only inside this module. Every site outside it reached in through
/// accessors one axis at a time, which compiles unchanged when the key grows —
/// and D-196, which widened this key from eight axes to ten, is the record of
/// what that costs. The sweep that widened it missed three sites, and two of the
/// three shipped a defect:
///
/// - `price_repo.rs`'s row comparator, which reading eight columns of a ten-axis
///   key makes read a successor on a **different meter of the same market** as
///   being on the predecessor's key.
/// - `content_pin.rs`'s digest, which framing eight axes makes pin two window
///   plans on two meters of one market identically — so an approve is satisfied
///   by a re-derivation over the other line's coverage. That is an approval
///   bypass, and it has shipped once.
///
/// A stale-count grep cannot find these sites, which is why the last sweep missed
/// three. A destructure can: add an axis to [`ScopeKey`] and every consumer that
/// takes its parts stops compiling until it says what to do with the new one.
///
/// # What this does *not* gate
///
/// **Four sites**, not three, and their cover is not the same. All four build or
/// compare a key **from** a stored row or a JSON payload rather than consuming a
/// `ScopeKey`, so none of them can take its parts.
///
/// Three of them — `price_repo::to_scope_key`, `price_repo::scope_key_columns`,
/// `read_model_repo::read_scope_key` — have [`ScopeKey::new`]'s positional
/// signature as a partial cover, and that cover fails for exactly the widening
/// D-196 performed: an axis pair added through a `with_*` builder rather than a
/// constructor parameter, which is how `meter` and `dimension_key` arrived.
///
/// **`price_repo::market_columns` has no cover at all.** It touches no
/// constructor — it is a bare eight-element tuple literal off `price::Model` — so
/// neither kind
/// of widening reaches it, and it is deliberately *partial* ("all of them but
/// `priceEligibility` and `cohort`"), which is what makes an omission invisible
/// there: an eleventh axis silently absent does not read as "eight of ten", it
/// reads as "modulo three axes", which is the shape the function is supposed to
/// have. It also decides `refuse_ungenerational`, the row plane's last guard
/// before `publish_rows`. It sits ten lines from `scope_key_columns`, whose doc is
/// this crate's own account of this exact defect shipping once, and a sweep can
/// still walk past it.
///
/// Its cover is now a test rather than a type: `price_repo_tests` drives one case
/// per axis from an exhaustive [`ScopeKeyParts`] destructure, so an eleventh axis
/// stops **that file** compiling. A refactor was rejected on `scope_key_columns`'
/// own stated ground — a comparison that had to parse first would answer
/// "corrupt" where the honest answer is "these two rows are not on one key".
pub(crate) struct ScopeKeyParts<'a> {
    pub plan_id: PlanId,
    pub currency: &'a CurrencyCode,
    pub region: &'a Region,
    pub price_overlay: PriceOverlay,
    pub phase: PhaseId,
    pub price_eligibility: PriceEligibility,
    pub charge_kind: ChargeKind,
    pub cohort: Cohort,
    pub meter: Option<&'a Meter>,
    pub dimension_key: &'a DimensionKey,
}

impl ScopeKey {
    /// Borrow every axis at once.
    ///
    /// The `let Self {.. }` below carries **no** rest pattern, so an eleventh
    /// axis is a compile error here — and, because [`ScopeKeyParts`] gains the
    /// field too, at every site that destructures the result.
    #[must_use]
    pub(crate) fn parts(&self) -> ScopeKeyParts<'_> {
        let Self {
            plan_id,
            currency,
            region,
            price_overlay,
            phase,
            price_eligibility,
            charge_kind,
            cohort,
            meter,
            dimension_key,
        } = self;
        ScopeKeyParts {
            plan_id: *plan_id,
            currency,
            region,
            price_overlay: *price_overlay,
            phase: *phase,
            price_eligibility: *price_eligibility,
            charge_kind: *charge_kind,
            cohort: *cohort,
            meter: meter.as_ref(),
            dimension_key,
        }
    }

    /// Build a validated key for a row this gear authors.
    ///
    /// `price_overlay` is not a parameter: rows authored here always carry
    /// [`PriceOverlay::Base`], and partner / orgTier / brand overlays are
    /// separate overlay rows rather than a value of this axis. Passing it would
    /// offer a choice the authoring plane does not have.
    ///
    /// `phase` is required rather than defaulted: the default is the plan's
    /// terminal phase id, which only the caller holding the plan can resolve
    /// (D-19).
    ///
    /// # Errors
    ///
    /// [`DomainError::ValidationFailed`] when `price_eligibility` and `cohort`
    /// disagree; see [`check_cohort_eligibility`].
    /// [`DomainError::TimestampPrecisionExceeded`] when a generational `cohort`
    /// carries precision below the millisecond quantum (D-144) — this axis is
    /// matched for **equality** against an instant a different gear produced, so
    /// an unquantized value would build a key nobody can find rather than a key
    /// that is wrong.
    pub fn new(
        plan_id: PlanId,
        currency: CurrencyCode,
        region: Region,
        phase: PhaseId,
        price_eligibility: PriceEligibility,
        charge_kind: ChargeKind,
        cohort: Cohort,
    ) -> Result<Self, DomainError> {
        check_cohort_eligibility(price_eligibility, cohort)?;
        if let Some(generation) = cohort.generation() {
            instant::check_quantum("cohort", generation)?;
        }
        Ok(Self {
            plan_id,
            currency,
            region,
            price_overlay: PriceOverlay::Base,
            phase,
            price_eligibility,
            charge_kind,
            cohort,
            meter: None,
            dimension_key: DimensionKey::none(),
        })
    }

    /// Attach the usage line — the ninth and tenth axes (D-196).
    ///
    /// Separate from [`Self::new`] rather than two more parameters on it, and
    /// the reason is the axes' own shape: they exist on `usage` rows and
    /// nowhere else, so every non-usage caller would pass `None` and
    /// [`DimensionKey::none`] to satisfy a signature that cannot use them. The
    /// eight unconditional axes stay one call; the conditional pair is a second
    /// one, taken only by the callers that have a line to name.
    ///
    /// A key with no usage line is what [`Self::new`] already returns, so this
    /// is never needed to *omit* the pair.
    ///
    /// # Errors
    ///
    /// [`DomainError::ValidationFailed`] carrying a single
    /// [`USAGE_LINE_AXIS_MISMATCH`] violation; see [`check_usage_line_axes`].
    /// [`DomainError::InvalidRequest`] when either axis value carries
    /// [`KEY_SEPARATOR`] or equals [`ABSENT_AXIS_TOKEN`] — see those constants for
    /// why the key refuses them rather than escaping or re-spelling them.
    pub fn with_usage_line(
        mut self,
        meter: Option<Meter>,
        dimension_key: DimensionKey,
    ) -> Result<Self, DomainError> {
        check_usage_line_axes(self.charge_kind, meter.as_ref(), &dimension_key)?;
        // The tenth axis's separator guard, here rather than in
        // [`DimensionKey::new`] because that constructor is total by design; see
        // its doc. The meter's is [`Meter::new`]'s and is repeated here for the
        // reason the key rehydration exists at all — this is the door every
        // *loaded* key comes through too, and a row written around the domain is
        // exactly what `to_scope_key`'s `CorruptRow` is for.
        check_no_separator("dimensionKey", dimension_key.as_str())?;
        // And the absent-axis token, on the same two axes and at the same door.
        // The tenth axis needs it as much as the ninth: `DimensionKey` is total
        // and renders its empty value as that token, so an authored `none`
        // dimension renders what an undimensioned line renders.
        check_not_absent_token("dimensionKey", dimension_key.as_str())?;
        if let Some(meter) = meter.as_ref() {
            check_no_separator("meter", meter.as_str())?;
            check_not_absent_token("meter", meter.as_str())?;
        }
        self.meter = meter;
        self.dimension_key = dimension_key;
        Ok(self)
    }

    /// Axis 1 — the plan.
    #[must_use]
    pub const fn plan_id(&self) -> PlanId {
        self.plan_id
    }

    /// Axis 2 — the currency.
    #[must_use]
    pub const fn currency(&self) -> &CurrencyCode {
        &self.currency
    }

    /// Axis 3 — the pricing region.
    #[must_use]
    pub const fn region(&self) -> &Region {
        &self.region
    }

    /// Axis 4 — the overlay plane (always `base` on an authored row).
    #[must_use]
    pub const fn price_overlay(&self) -> PriceOverlay {
        self.price_overlay
    }

    /// Axis 5 — the phase.
    #[must_use]
    pub const fn phase(&self) -> PhaseId {
        self.phase
    }

    /// Axis 6 — the eligibility class.
    #[must_use]
    pub const fn price_eligibility(&self) -> PriceEligibility {
        self.price_eligibility
    }

    /// Axis 7 — the charge component.
    #[must_use]
    pub const fn charge_kind(&self) -> ChargeKind {
        self.charge_kind
    }

    /// Axis 8 — the grandfathering generation.
    #[must_use]
    pub const fn cohort(&self) -> Cohort {
        self.cohort
    }

    /// Axis 9 — the metering unit, on a usage row (D-196).
    #[must_use]
    pub const fn meter(&self) -> Option<&Meter> {
        self.meter.as_ref()
    }

    /// Axis 10 — the dimension discriminator on the line (D-196).
    #[must_use]
    pub const fn dimension_key(&self) -> &DimensionKey {
        &self.dimension_key
    }

    /// This key's **grandfathered generation** at `cutover` (D-309).
    ///
    /// A cutover moves exactly two axes — `price_eligibility` to
    /// `existing_grandfathered` and `cohort` to the generation — and carries the
    /// other eight across. That is the whole of what a copy key is, and it lives
    /// here for [`is_sibling_of`]'s reason: **the destructure below has no rest
    /// pattern**, so an eleventh axis is a compile error until somebody decides
    /// whether a generation carries it.
    ///
    /// It was `domain::cutover::generation_key` first, reading the predecessor's
    /// axes through accessors one call at a time — which compiles unchanged when
    /// the key grows and drops the new axis from every grandfathered copy in
    /// silence. That is D-205's defect verbatim, minted in the same wave that
    /// repaired its fifth and sixth instances (D-296, D-300), and it is why the
    /// construction belongs to the type that owns the fields rather than to a
    /// caller reaching in through getters.
    ///
    /// # Errors
    ///
    /// [`DomainError::TimestampPrecisionExceeded`] when `cutover` is finer than
    /// the millisecond quantum (D-144) — this axis is matched for **equality**
    /// against an instant another gear produced, so an unquantized value builds a
    /// key nobody can find. The cohort/eligibility biconditional is satisfied by
    /// construction and re-checked anyway, because a check that costs nothing and
    /// documents an invariant is cheaper than the invariant going unstated.
    pub fn to_generation(&self, cutover: OffsetDateTime) -> Result<Self, DomainError> {
        let Self {
            plan_id,
            currency,
            region,
            price_overlay,
            phase,
            price_eligibility: _,
            charge_kind,
            cohort: _,
            meter,
            dimension_key,
        } = self;

        let cohort = Cohort::Generation(cutover);
        check_cohort_eligibility(PriceEligibility::ExistingGrandfathered, cohort)?;
        instant::check_quantum("cohort", cutover)?;

        Ok(Self {
            plan_id: *plan_id,
            currency: currency.clone(),
            region: region.clone(),
            price_overlay: *price_overlay,
            phase: *phase,
            price_eligibility: PriceEligibility::ExistingGrandfathered,
            charge_kind: *charge_kind,
            cohort,
            meter: meter.clone(),
            dimension_key: dimension_key.clone(),
        })
    }

    /// Do these two keys compete for **one** sale — equal on every axis but the
    /// eligibility class and the cohort?
    ///
    /// The relation W3's most-specific-wins ranks over
    /// (`design/07-pricewindow-linkage.md`, PRD §1.4): two rows a single purchase
    /// could bind, of which the more specific class is the one that binds. Two
    /// rows that are *not* siblings are two different things being bought, and
    /// ranking them against each other drops one from the sale entirely.
    ///
    /// **It lives on the key, and it destructures.** The caller that needs this is
    /// `domain::sellability`, and a six-axis spelling of its own reads two usage
    /// lines of one market as siblings, dropping the less specific one from the
    /// sellability gate and answering over a key whose window plane nobody has looked
    /// at. A comparison stated as "every axis except two"
    /// is the one kind that cannot be written safely at a distance: it has to be
    /// re-read whenever the key gains an axis, and nothing makes that happen. So
    /// the `let Self {.. }` below carries **no** rest pattern, and an eleventh axis
    /// is a compile error here rather than a gate input silently disappearing.
    #[must_use]
    pub fn is_sibling_of(&self, other: &Self) -> bool {
        let Self {
            plan_id,
            currency,
            region,
            price_overlay,
            phase,
            price_eligibility: _,
            charge_kind,
            cohort: _,
            meter,
            dimension_key,
        } = self;
        *plan_id == other.plan_id
            && *currency == other.currency
            && *region == other.region
            && *price_overlay == other.price_overlay
            && *phase == other.phase
            && *charge_kind == other.charge_kind
            && *meter == other.meter
            && *dimension_key == other.dimension_key
    }
}

impl fmt::Display for ScopeKey {
    /// The canonical rendering: ten axes, normative order, one separator.
    /// This is the string a `DUPLICATE_SCOPE_KEY` rejection names, so it has to
    /// be stable and complete — a rendering that dropped an axis would report a
    /// collision between two rows that do not actually share a key.
    ///
    /// **The arity is fixed at ten whatever the row is (D-196)**, `none` filling
    /// both usage positions on a non-usage key. A rendering whose segment count
    /// depended on the charge kind would be a parsing hazard in the three places
    /// this string is embedded rather than read: the rejection message, the
    /// approval register's held-key rows, and `unit_request_id`, the
    /// cross-tenant registry idempotency key.
    ///
    /// **Ten segments, always.** The three free-form axes refuse
    /// [`KEY_SEPARATOR`] — [`Region::new`], [`Meter::new`] and
    /// [`ScopeKey::with_usage_line`] — which is why this impl may join with a
    /// bare literal and count on ten.
    ///
    /// **And the rendering is injective**, which is the property the four surfaces
    /// that read it back as identity actually need. The two axes with an absent
    /// form refuse [`ABSENT_AXIS_TOKEN`] at the same two constructors, so the token
    /// below means "absent" and cannot also mean an authored value.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Destructured, so an eleventh axis is a compile error here rather than a
        // segment silently missing from the string a `DUPLICATE_SCOPE_KEY`
        // rejection names.
        let ScopeKeyParts {
            plan_id,
            currency,
            region,
            price_overlay,
            phase,
            price_eligibility,
            charge_kind,
            cohort,
            meter,
            dimension_key,
        } = self.parts();
        write!(
            f,
            "{plan_id}|{currency}|{region}|{price_overlay}|{phase}|{price_eligibility}|\
             {charge_kind}|{cohort}|{}|{dimension_key}",
            meter.map_or("none", Meter::as_str),
        )
    }
}

#[cfg(test)]
#[path = "scope_key_tests.rs"]
mod scope_key_tests;
