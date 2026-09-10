//! Slice 6's consumer contracts: the fields a downstream system computes from
//! (`design/06-consumer-contracts.md`).
//!
//! The catalog publishes **inputs**; the math and the enforcement live
//! downstream. Every rule here is therefore a presence/consistency statement
//! about what a publish freezes, never a computation over it — `inst-rc-nocompute`
//! is the slice's own name for that boundary and it is why no rule in this
//! module reads an amount.
//!
//! # Why the subject is the [`PlanShape`] and not a [`PriceRow`]
//!
//! Slice 3's row rules judge a [`PriceRow`](crate::domain::price_row::PriceRow),
//! which deliberately carries no identity and none of the columns this slice
//! owns: `billing_timing` is a column of the
//! [`PriceRecord`](crate::domain::price_record::PriceRecord) that wraps a row,
//! not of the row. A rule here could not read its own field from Slice 3's
//! subject, so the subject is the plan and each rule walks `shape.rows`.
//!
//! That is also what the market-uniformity rule needs (`inst-pi-uniform`): a
//! statement about a *set* of rows crossed with a set of markets has no
//! row-local form at all.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use toolkit_macros::domain_model;
use uuid::Uuid;

use crate::domain::money::CurrencyCode;
use crate::domain::plan_shape::{PlanPhase, PlanShape};
use crate::domain::price_record::PriceRecord;
use crate::domain::scope_key::{ChargeKind, PriceEligibility, Region};
use crate::domain::validation::{ValidationPipeline, ValidationReport, ValidationRule};

/// The day of the month a `fixed_day` anchor lands on, 1–31.
///
/// A newtype rather than a bare `u8` because the range is the whole of its
/// meaning and §6 states it as a constraint (`anchor_day BETWEEN 1 AND 31`) that
/// no `CHECK` expresses on either engine — see
/// [`pricing_price`](crate::infra::storage::migrations::m20260821_000023_create_pricing_price)
/// for why. Refusing the value at construction is the stronger form of the same
/// statement: a column can hold only what this renders.
///
/// **29, 30 and 31 are legal and are not a mistake.** K2 makes a day past the
/// month's length anchor on the **last day of the month**, with the anchor day
/// preserved across periods (31 → 28 → 31, an independent per-period clamp with
/// no drift). Refusing them here would make February the shortest legal anchor
/// and quietly forbid the month-end billing K2 exists to define.
#[domain_model]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct AnchorDay(u8);

impl AnchorDay {
    /// The day, if it is one a month can have.
    ///
    /// # Errors
    /// [`AnchorDayOutOfRange`] for `0` or anything past `31`.
    pub const fn new(day: u8) -> Result<Self, AnchorDayOutOfRange> {
        if day >= 1 && day <= 31 {
            Ok(Self(day))
        } else {
            Err(AnchorDayOutOfRange(day))
        }
    }

    /// The day as stored.
    #[must_use]
    pub const fn get(self) -> u8 {
        self.0
    }
}

impl fmt::Display for AnchorDay {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// An anchor day outside the 1–31 a month can offer.
#[domain_model]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AnchorDayOutOfRange(pub u8);

impl fmt::Display for AnchorDayOutOfRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "anchor_day must be between 1 and 31; got {}. A day past the \
             month's length is legal and anchors on the last day of the month \
             (K2), so the bound is the calendar's, not the shortest month's",
            self.0
        )
    }
}

impl std::error::Error for AnchorDayOutOfRange {}

/// Where a subscription's billing cycle boundary falls (K2, §6).
///
/// `FixedDay` **carries its day**, for the reason
/// [`Frequency::CustomEveryN`](crate::domain::plan_shape::Frequency) carries its
/// interval: §6 gives the policy and `anchor_day` as one fact in two columns,
/// and the two spellings a flat pair admits — a `fixed_day` with no day, a day
/// beside `calendar_month` — are both unpublishable. Holding them together makes
/// the pairing structural, so `inst-pi-anchor` needs no rule for it and no
/// engine needs a `CHECK` it cannot express.
#[domain_model]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BillingAnchorPolicy {
    /// The first of the calendar month, UTC.
    CalendarMonth,
    /// The subscription's own start day, clamped per period under monthly
    /// granular cycles (K2, D-20).
    SubscriptionStart,
    /// A named day of the month, clamped to the month's last day when the month
    /// is shorter.
    FixedDay(AnchorDay),
}

impl BillingAnchorPolicy {
    /// K2's set, whole, for the readers that map a stored token back —
    /// [`ProrationBasis::ALL`]'s reason, on the enum two lines above it that had no
    /// roster at all (Z6/Z13-3).
    ///
    /// **`FixedDay` stands here on a placeholder day, and the placeholder is
    /// deliberate.** What a roster is for is the token→member direction, and
    /// [`Self::as_str`] renders `fixed_day` for *every* day, so one member per token
    /// is the whole vocabulary; the day is a second fact, and
    /// [`Self::with_anchor_day`] is what re-pairs it. A roster carrying 31 members
    /// would be a roster of `(policy, day)` pairs pretending to be a roster of
    /// policies, and it would answer the same token 31 times.
    ///
    /// Spelled out rather than derived so that adding a member is a change this
    /// array records: K2 makes any extension a versioned contract change, and one
    /// that slipped in without touching a declared roster is the drift
    /// `pricing.contracts.enum_drift` alarms on.
    pub const ALL: &'static [Self] = &[
        Self::CalendarMonth,
        Self::SubscriptionStart,
        Self::FixedDay(Self::ROSTER_DAY),
    ];

    /// The placeholder day [`Self::ALL`]'s `FixedDay` member carries.
    ///
    /// Named rather than inlined so a reader of `ALL` cannot mistake it for a
    /// default: nothing anchors on it, and every parse that reaches `FixedDay`
    /// replaces it through [`Self::with_anchor_day`] before the value escapes.
    const ROSTER_DAY: AnchorDay = match AnchorDay::new(1) {
        Ok(day) => day,
        // Unreachable: 1 is in 1..=31. Spelled as a match rather than an
        // `expect` because this is a `const`.
        Err(_) => panic!("1 is a day a month can have"),
    };

    /// This policy with `day` as its anchor, where it names one.
    ///
    /// The inverse of [`Self::anchor_day`], and what makes [`Self::ALL`] usable as a
    /// parse roster: a token read back through `ALL` arrives as `FixedDay` on the
    /// placeholder day, and the caller pairs the real day here rather than
    /// re-matching the token. The two hand-written token matches this replaced —
    /// one in `api::rest::prices`, one in `price_repo` — each re-spelled all three
    /// tokens next to an enum they had already imported.
    ///
    /// `CalendarMonth` and `SubscriptionStart` ignore `day` and return unchanged:
    /// this function pairs a day with a policy that takes one and is **not** where a
    /// day-beside-`calendar_month` request is refused. That refusal belongs to the
    /// caller, which knows whether the day was *sent*; a policy that silently
    /// absorbed it would make the two unpublishable spellings this enum's shape
    /// exists to prevent representable again.
    #[must_use]
    pub const fn with_anchor_day(self, day: AnchorDay) -> Self {
        match self {
            Self::FixedDay(_) => Self::FixedDay(day),
            Self::CalendarMonth | Self::SubscriptionStart => self,
        }
    }

    /// The persisted / wire token, which never carries the day.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CalendarMonth => "calendar_month",
            Self::SubscriptionStart => "subscription_start",
            Self::FixedDay(_) => "fixed_day",
        }
    }

    /// The day this policy anchors on, when it names one.
    #[must_use]
    pub const fn anchor_day(self) -> Option<AnchorDay> {
        match self {
            Self::FixedDay(day) => Some(day),
            Self::CalendarMonth | Self::SubscriptionStart => None,
        }
    }
}

impl fmt::Display for BillingAnchorPolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FixedDay(day) => write!(f, "fixed_day({day})"),
            other => f.write_str(other.as_str()),
        }
    }
}

/// The canonical proration basis (K1, §6).
///
/// **Owned here.** K1 makes this enum the one source Tariffs adopts verbatim and
/// Subscriptions computes from, and says any extension is a versioned contract
/// change — so a second spelling of this set anywhere is the enum-drift failure
/// class §1.2 exists to kill.
#[domain_model]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProrationBasis {
    /// Days actually in the period.
    CalendarDaysActual,
    /// A 30-day month regardless of the calendar's.
    CalendarDays30,
    /// Second-granular.
    BySecond,
    /// No partial unit: a started unit is a whole one.
    WholeUnit,
    /// No proration at all.
    None,
}

impl ProrationBasis {
    /// K1's set, whole, for the readers that map a stored token back.
    ///
    /// Spelled out rather than derived so that adding a member is a change this
    /// array records — K1 makes any extension a **versioned contract change**,
    /// and one that slipped in without touching a declared roster is exactly the
    /// drift `pricing.contracts.enum_drift` alarms on.
    pub const ALL: &'static [Self] = &[
        Self::CalendarDaysActual,
        Self::CalendarDays30,
        Self::BySecond,
        Self::WholeUnit,
        Self::None,
    ];

    /// The persisted / wire token.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CalendarDaysActual => "calendar_days_actual",
            Self::CalendarDays30 => "calendar_days_30",
            Self::BySecond => "by_second",
            Self::WholeUnit => "whole_unit",
            Self::None => "none",
        }
    }

    /// Is this the basis that computes nothing?
    ///
    /// Read through here rather than matched at each site: `inst-pi-credit-none`
    /// is the only rule that cares which member this is, and a second `match`
    /// on the variant is a second place to forget it.
    #[must_use]
    pub const fn is_none(self) -> bool {
        matches!(self, Self::None)
    }
}

impl fmt::Display for ProrationBasis {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The three fields a recurring row publishes for Subscriptions' proration
/// (`inst-pi-required`), held together because they are required together.
///
/// One `Option<ProrationContract>` on the record rather than three independent
/// `Option`s: §3 step 1 makes all three REQUIRED on a recurring row and absence
/// fail publish, so the only states worth representing are "authored" and "not
/// authored". Three loose options admit six partial states that no rule can
/// report usefully and that the market-uniformity comparison would have to
/// compare field by field anyway.
#[domain_model]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProrationContract {
    /// Where the cycle boundary falls (K2).
    pub billing_anchor_policy: BillingAnchorPolicy,
    /// The canonical basis Subscriptions and Tariffs both read (K1).
    pub proration_basis: ProrationBasis,
    /// Whether a downgrade off this row is credit-eligible.
    ///
    /// **The governing value on a plan change is the *source* row's**, read from
    /// the subscription's frozen snapshot — never the target's and never the
    /// live catalog (`inst-pi-credit-source`). That is a rule about which
    /// snapshot a downstream reader picks, so the catalog's whole obligation is
    /// to publish the field per row and freeze it; nothing here chooses.
    pub credit_on_downgrade: bool,
}

/// A published recurring row carries no `billingTiming`
/// (`06-consumer-contracts.md` §3 `inst-bt-required`, §5).
///
/// The code is §5's verbatim. **Slice 2 names it too** and does not register it:
/// `plan_rules::cycle_shape`'s module doc records that `billingTiming` REQUIRED
/// on a recurring row is Slice 6's rule, cross-referenced and never
/// re-registered, while Slice 2 owns the opposite statement about a *setup* row
/// (`inst-cs-setup`, `SETUP_ROW_INVALID`). The two can never disagree because
/// the `chargeKind` axis makes no row both.
pub const BILLING_TIMING_MISSING: &str = "BILLING_TIMING_MISSING";

/// Every recurring row of the publish subject states its `billingTiming`.
///
/// **Recurring only, and every recurring row.** Usage and one-time rows do not
/// author the field — `inst-bt-usage` projects a constant for them — and a rule
/// that demanded it there would reject a row whose value is not the author's to
/// give. `existing_grandfathered` rows are *not* excluded: D-132's exclusion is
/// an argument about comparing a frozen generation against current rows, which
/// is the uniformity rule's problem and not this one. A grandfathered row that
/// published without the field would leave Billing deriving a deferral policy
/// for a live subscriber by heuristic, which is exactly what `inst-bt-frozen`
/// exists to prevent.
#[domain_model]
#[derive(Clone, Copy, Debug, Default)]
pub struct BillingTimingPresent;

impl ValidationRule<PlanShape> for BillingTimingPresent {
    fn name(&self) -> &'static str {
        "inst-bt-required"
    }

    fn evaluate(&self, subject: &PlanShape, report: &mut ValidationReport) {
        for record in &subject.rows {
            if !is_recurring(record.scope_key.charge_kind()) || record.billing_timing.is_some() {
                continue;
            }
            report.violate(
                BILLING_TIMING_MISSING,
                record.price_id.to_string(),
                "a recurring row MUST state its billingTiming (advance | arrears): Billing \
                 derives its deferral policy from the frozen value and never from a heuristic, so \
                 an absent one is a row no consumer can defer correctly (inst-bt-required, \
                 inst-bt-frozen)"
                    .to_owned(),
            );
        }
    }
}

/// A recurring row publishes without its proration inputs
/// (`06-consumer-contracts.md` §3 `inst-pi-required`, §5).
pub const PRORATION_INPUTS_MISSING: &str = "PRORATION_INPUTS_MISSING";

/// `creditOnDowngrade = true` on a row whose basis computes nothing
/// (`inst-pi-credit-none`, §5).
pub const PRORATION_INPUTS_CONTRADICTORY: &str = "PRORATION_INPUTS_CONTRADICTORY";

/// The recurring rows of one plan-market disagree on the contract
/// (D-123 as scoped by D-132, `inst-pi-uniform`, §5).
pub const PRORATION_CONTRACT_MIXED_MARKET: &str = "PRORATION_CONTRACT_MIXED_MARKET";

/// Every recurring row states all three proration inputs (`inst-pi-required`).
///
/// The domain type makes "all three or none" the only representable pair, so
/// this rule has exactly one thing to say: the set is absent on a row that owes
/// it. What it does **not** say is anything about the values — `inst-pi-enum`
/// (K1's five bases) and the `fixed_day`/`anchor_day` pairing of
/// `inst-pi-anchor` are both discharged by [`ProrationBasis`] and
/// [`BillingAnchorPolicy`] being unable to hold a violation, which is the
/// stronger form of the same statement and the one `publish::rules` already
/// takes for the money rules.
#[domain_model]
#[derive(Clone, Copy, Debug, Default)]
pub struct ProrationInputsPresent;

impl ValidationRule<PlanShape> for ProrationInputsPresent {
    fn name(&self) -> &'static str {
        "inst-pi-required"
    }

    fn evaluate(&self, subject: &PlanShape, report: &mut ValidationReport) {
        for record in &subject.rows {
            if !is_recurring(record.scope_key.charge_kind()) || record.proration_contract.is_some()
            {
                continue;
            }
            report.violate(
                PRORATION_INPUTS_MISSING,
                record.price_id.to_string(),
                "a recurring row MUST publish billingAnchorPolicy, prorationBasis and \
                 creditOnDowngrade: Subscriptions computes its proration from the frozen values \
                 and the catalog substitutes no defaults, so an absent set is a row no consumer \
                 can prorate (inst-pi-required)"
                    .to_owned(),
            );
        }
    }
}

/// A credited downgrade needs a basis to compute the partial period with
/// (`inst-pi-credit-none`).
///
/// The contradiction is `creditOnDowngrade = true` beside
/// `prorationBasis = none`: the row promises a credit and denies the arithmetic
/// that would size it. Both halves are individually legal, which is why this is
/// a rule and not a type — `none` is a real basis (a plan that never prorates)
/// and `true` is a real flag; only the pair is unpublishable.
#[domain_model]
#[derive(Clone, Copy, Debug, Default)]
pub struct ProrationCreditHasBasis;

impl ValidationRule<PlanShape> for ProrationCreditHasBasis {
    fn name(&self) -> &'static str {
        "inst-pi-credit-none"
    }

    fn evaluate(&self, subject: &PlanShape, report: &mut ValidationReport) {
        for record in &subject.rows {
            let Some(contract) = record.proration_contract else {
                continue;
            };
            if !(contract.credit_on_downgrade && contract.proration_basis.is_none()) {
                continue;
            }
            report.violate(
                PRORATION_INPUTS_CONTRADICTORY,
                record.price_id.to_string(),
                "creditOnDowngrade = true with prorationBasis = none is a contradiction: the row \
                 grants a credit for a surrendered part-period and states no basis to compute \
                 that part with (inst-pi-credit-none)"
                    .to_owned(),
            );
        }
    }
}

/// One proration/anchor contract per plan-market (D-123, scoped by D-132).
///
/// **A subscription is one cycle clock** — D-110's "an invoice is one document"
/// applied to the time axis. Phase is a scope-key axis, so under D-15 a phased
/// plan carries one recurring row per charging phase per market, each with its
/// own anchor as authored; the consuming side reads **one** value, because
/// Subscriptions' `billingAnchor` is a single field on the aggregate. Nothing
/// related the N authored values to the one consumed value, so an intro-pricing
/// plan anchoring `subscription_start` on the interim row and `fixed_day(1)` on
/// the terminal row published both into one frozen snapshot with no rule saying
/// which sets the boundary. This rule makes the question not arise: the phase
/// axis becomes cycle-clock-neutral by construction.
///
/// **Per market, not per plan.** Anchoring EU on the 1st and US on signup day
/// stays legal, exactly as D-110 lets two markets differ on tax basis.
///
/// **`existing_grandfathered` generations are excluded before the grouping**,
/// not filtered out of a finding afterwards — [`MarketBasisUniform`]'s reason
/// and D-132's: an immutable, never-superseded generation must not even
/// contribute a value to compare against, or one cutover would permanently
/// freeze the market's cycle clock and every later publish would fail on a row
/// nobody can fix. A grandfathered subscriber reads these fields from its own
/// frozen snapshot, so "a subscription is one cycle clock" still holds per
/// subscription.
///
/// [`MarketBasisUniform`]: crate::domain::tax_display::MarketBasisUniform
///
/// **`billingTiming` is exempt** and is not read here: it is deliberately
/// per-row, because a hybrid mixes an `advance` base with an `arrears` usage
/// line (`inst-bt-usage`) and Billing consumes it per line rather than as a
/// subscription-level clock.
#[domain_model]
#[derive(Clone, Copy, Debug, Default)]
pub struct ProrationContractMarketUniform;

impl ValidationRule<PlanShape> for ProrationContractMarketUniform {
    fn name(&self) -> &'static str {
        "inst-pi-uniform"
    }

    fn evaluate(&self, subject: &PlanShape, report: &mut ValidationReport) {
        // The contract is carried **beside** the row rather than left on it as
        // an `Option`, so nothing below has to re-assert that a row in this map
        // has one. A row with no contract at all is `inst-pi-required`'s
        // finding: admitting it here would report one omission under two codes
        // and send the author to remediate it in two places.
        let mut markets: BTreeMap<Market, Vec<ContractedRow<'_>>> = BTreeMap::new();
        for record in &subject.rows {
            if !is_recurring(record.scope_key.charge_kind())
                || !in_uniformity_set(record.scope_key.price_eligibility())
            {
                continue;
            }
            let Some(contract) = record.proration_contract else {
                continue;
            };
            markets
                .entry((
                    record.scope_key.currency().clone(),
                    record.scope_key.region().clone(),
                ))
                .or_default()
                .push((record, contract));
        }

        for ((currency, region), rows) in markets {
            let Some(((_, reference), rest)) = rows.split_first() else {
                continue;
            };
            if !rest.iter().any(|(_, c)| c != reference) {
                continue;
            }

            let diverges = |field: fn(&ProrationContract, &ProrationContract) -> bool| {
                rest.iter().any(|(_, c)| field(c, reference))
            };
            let mut fields = Vec::new();
            if diverges(|c, r| c.billing_anchor_policy != r.billing_anchor_policy) {
                fields.push("billingAnchorPolicy");
            }
            if diverges(|c, r| c.proration_basis != r.proration_basis) {
                fields.push("prorationBasis");
            }
            if diverges(|c, r| c.credit_on_downgrade != r.credit_on_downgrade) {
                fields.push("creditOnDowngrade");
            }

            // Every row of the market is named, not only the ones that differ
            // from whichever happened to be first: an operator told "these two
            // disagree" still has to find what the market's contract is, and
            // rendering each row beside its own values answers that in one read.
            let sides: Vec<String> = rows
                .iter()
                .map(|(record, c)| {
                    format!(
                        "{}: billingAnchorPolicy={}, prorationBasis={}, creditOnDowngrade={}",
                        record.price_id,
                        c.billing_anchor_policy,
                        c.proration_basis,
                        c.credit_on_downgrade
                    )
                })
                .collect();

            report.violate(
                PRORATION_CONTRACT_MIXED_MARKET,
                format!("{}/{region}", currency.as_str()),
                format!(
                    "recurring rows of this plan on market {}/{region} disagree on {} - {}. A \
                     subscription is one cycle clock, and phase is a scope-key axis, so a phased \
                     plan's rows on one market must carry one contract or nothing says which of \
                     them sets the boundary (D-123). Grandfathered generations are excluded from \
                     this set (D-132)",
                    currency.as_str(),
                    fields.join(", "),
                    sides.join(" | ")
                ),
            );
        }
    }
}

/// Slice 6's registered set over one publish subject.
#[must_use]
pub fn consumer_contract_rules(index: &ChangeTargetIndex) -> ValidationPipeline<PlanShape> {
    ValidationPipeline::new()
        .with_rule(Box::new(BillingTimingPresent))
        .with_rule(Box::new(ProrationInputsPresent))
        .with_rule(Box::new(ProrationCreditHasBasis))
        .with_rule(Box::new(ProrationContractMarketUniform))
        .with_rule(Box::new(ChangeGraphAuthorable {
            index: index.clone(),
        }))
        .with_rule(Box::new(GrantSetPhasesKnown))
}

/// The `(currency, region)` pair D-123 scopes uniformity to. Named because the
/// rule groups by it and `tax_display`'s D-110 sibling groups by the same pair.
type Market = (CurrencyCode, Region);

/// One row of a market beside the contract it published. The contract is not an
/// `Option` here: rows without one are `inst-pi-required`'s finding and never
/// enter the grouping.
type ContractedRow<'a> = (&'a PriceRecord, ProrationContract);

/// Is this row's eligibility class inside D-123's uniformity row set?
///
/// The two classes D-123 names, and not the third. An `existing_grandfathered`
/// generation is immutable and never superseded, so a divergence it carries can
/// never be remediated — including it would let one cutover permanently freeze
/// the market's cycle clock and fail every later publish on a row nobody can
/// fix (D-132). It is excluded **before** the grouping rather than filtered out
/// of a finding, so it cannot decide the market's verdict either.
fn in_uniformity_set(eligibility: PriceEligibility) -> bool {
    matches!(
        eligibility,
        PriceEligibility::AllSubscriptions | PriceEligibility::NewSubscriptionsOnly
    )
}

/// Is this row one the recurring-row contracts apply to?
fn is_recurring(charge_kind: ChargeKind) -> bool {
    matches!(charge_kind, ChargeKind::Recurring)
}

/// The `billingTiming` a row **publishes**, which on every kind but `recurring`
/// is a constant the author never gave (`inst-bt-usage`).
///
/// Usage is implicitly `arrears` — its quantity is not known until the period
/// closes — and a one-time or setup row is charged at the event, which is
/// `advance`. A hybrid therefore publishes `advance` on its base line and
/// `arrears` on its usage line, which is the mix `inst-bt-usage` sanctions and
/// the reason `inst-pi-uniform` exempts this field from market uniformity.
///
/// **The constant wins over anything in the column**, and that is the point: it
/// is a projection, not a default. Were an authored value allowed to displace
/// it, Billing's deferral on a usage line would depend on whether someone had
/// typed into a column the design says is not theirs to author — and no rule
/// would report the difference, because there is nothing to report about a
/// field that is not authored. Slice 2 already refuses the value outright on a
/// setup row (`inst-cs-setup`, `SETUP_ROW_INVALID`); this closes the same gap
/// on the read side for every non-recurring kind at once.
///
/// The tokens are the **stored** spelling (`advance` / `arrears`), which is what
/// `chk_pricing_price_billing_timing` admits and what a recurring row already
/// projects. See this module's note in the hand-back on the design set's
/// `in_advance` / `in_arrears` prose.
#[must_use]
pub fn published_billing_timing(charge_kind: ChargeKind, authored: Option<&str>) -> Option<&str> {
    match charge_kind {
        ChargeKind::Recurring => authored,
        ChargeKind::Usage => Some(BILLING_TIMING_ARREARS),
        ChargeKind::OneTime | ChargeKind::OneTimeSetup => Some(BILLING_TIMING_ADVANCE),
    }
}

/// Charged at the start of the period it covers.
pub const BILLING_TIMING_ADVANCE: &str = "advance";

/// Charged after the period it covers has closed.
pub const BILLING_TIMING_ARREARS: &str = "arrears";

#[cfg(test)]
#[path = "contracts_tests.rs"]
mod contracts_tests;

// ---------------------------------------------------------------------------
// The plan-change contract (§3 Plan-Change Contract, §6).
// ---------------------------------------------------------------------------

/// What happens to a tier counter's `Q` when a subscription changes plan
/// (D-113, `inst-pc-counter-carry`).
///
/// **The snapshot-frozen flag Rating consults**, and the pricing side's whole
/// obligation is to publish it: at an in-place change Rating routes the
/// **target** plan's frozen value — the plan whose bands consume the continued
/// `Q` accepts the continuity liability — and honours `Carry` only per shared
/// `(meter, dimensionKey)` line whose D-82/D-98/D-122 unit fields match across
/// both frozen snapshots. A mismatched line resets. None of that is decided
/// here; the catalog publishes the input.
///
/// **Absence is [`Self::Reset`]**, which is why this is not an `Option` on the
/// contract: an old snapshot without the field is a reset, never a rating
/// failure. Representing "unset" would create a third state no consumer has a
/// reading for.
///
/// The default matters for money. An unguarded carry is the ×24 class through
/// its fourth door — supersession (D-82), kind flip (D-98), phase axis (D-89)
/// and plan change here — where an hours-denominated counter is applied to
/// day-denominated bands. `Reset` is the safe direction and is what an
/// unauthored plan gets.
#[domain_model]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum UsageCounterOnPlanChange {
    /// The counter restarts at the change boundary. The default, and what an
    /// absent value means.
    #[default]
    Reset,
    /// The counter continues, per unit-matched shared line only.
    Carry,
}

impl UsageCounterOnPlanChange {
    /// Both members, for the reader that maps a stored token back.
    pub const ALL: &'static [Self] = &[Self::Reset, Self::Carry];

    /// The persisted / wire token.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Reset => "reset",
            Self::Carry => "carry",
        }
    }
}

impl fmt::Display for UsageCounterOnPlanChange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The plan-change contract a revision publishes (§6).
///
/// # Why `allowed_change_targets` is an `Option<Vec<..>>` and not a bare `Vec`
///
/// `inst-pc-failsafe` makes **absence** mean *no self-service change*, and it is
/// a value with a reading rather than the lack of one. An empty vector says the
/// same thing, but only by convention — and a consumer that received `[]` where
/// the author meant "I have not decided" would have no way to tell. The `Option`
/// keeps "the author stated no edges" distinct from "the author stated nothing",
/// which is exactly the distinction `inst-cr-nodefault` promises a consumer it
/// can rely on.
///
/// # Why the rank is optional and the counter flag is not
///
/// K4 makes `comparability_rank` REQUIRED only of a plan **participating** in
/// self-service change, so a plan with no edges and no inbound edges legitimately
/// has none. `usage_counter_on_plan_change` has a ratified default (D-113,
/// `reset`) and absence *is* that default, so an `Option` would represent a
/// state with no distinct meaning.
#[domain_model]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PlanChangeContract {
    /// The explicit published `planId`s a self-service change may travel to.
    /// `None` is the fail-safe: no self-service change (`inst-pc-failsafe`).
    pub allowed_change_targets: Option<Vec<Uuid>>,
    /// The tenant-wide comparability scale (K4). Higher is an upgrade, lower a
    /// downgrade, equal a switch.
    pub comparability_rank: Option<i32>,
    /// D-113's tier-`Q` continuity flag, frozen into the snapshot Rating reads.
    pub usage_counter_on_plan_change: UsageCounterOnPlanChange,
}

impl PlanChangeContract {
    /// Does this plan participate in self-service change as a **source**?
    ///
    /// Read through here rather than matched at each site: "participates" is the
    /// predicate K4's rank requirement is stated against, and an edge list that
    /// is `Some` but empty must not count — an author who cleared their edges has
    /// left self-service change, and demanding a rank of them would refuse a
    /// publish that removes the last edge.
    #[must_use]
    pub fn offers_self_service_change(&self) -> bool {
        self.allowed_change_targets
            .as_ref()
            .is_some_and(|targets| !targets.is_empty())
    }

    /// The targets this contract names, or nothing.
    #[must_use]
    pub fn targets(&self) -> &[Uuid] {
        self.allowed_change_targets.as_deref().unwrap_or(&[])
    }
}

/// A published `allowedChangeTargets` edge names a plan that is not published
/// (`06-consumer-contracts.md` §5, `inst-pc-targets`).
pub const CHANGE_TARGET_UNPUBLISHED: &str = "CHANGE_TARGET_UNPUBLISHED";

/// A plan participating in self-service change carries no `comparabilityRank`
/// (K4, §5, `inst-pc-rank` / `inst-pc-mutual`).
pub const COMPARABILITY_RANK_REQUIRED: &str = "COMPARABILITY_RANK_REQUIRED";

/// A re-publish drops the rank while published inbound edges still reference the
/// plan (D-54, §5, `inst-pc-mutual`).
pub const COMPARABILITY_RANK_REVOKED: &str = "COMPARABILITY_RANK_REVOKED";

/// What the publish path knows about the *other* plans this one's change
/// contract touches.
///
/// **A resolved fact, handed in**, exactly as
/// [`ReferencingMarket`](crate::domain::publish::ReferencingMarket) is and for
/// the same reason: the rules below are statements about aggregates this walk
/// does not hold, answering them would need a storage read, and the rule set
/// runs twice on one publish with the same answer required both times.
///
/// It carries **both directions**, because `inst-pc-mutual` needs both. The
/// outbound half answers "is this target published, and does it carry a rank" —
/// `inst-pc-targets` and the forward half of mutual comparability. The inbound
/// half answers "who already points at me", which is D-54's reverse guard: a
/// plan referenced by a published edge must not re-publish with its rank dropped
/// to NULL, or every one of those already-published edges becomes unclassifiable
/// at read time — the same read-time drift D-23 cut rule-based targets to avoid.
#[domain_model]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ChangeTargetIndex {
    /// The rank of each **published** plan this publish's edges name. A plan
    /// absent from the map is one no published revision exists for.
    published_targets: BTreeMap<Uuid, Option<i32>>,
    /// The published plans whose own `allowedChangeTargets` name the plan under
    /// judgement.
    inbound_edges: BTreeSet<Uuid>,
}

impl ChangeTargetIndex {
    /// The fail-closed empty index: no plan is published, nobody points here.
    ///
    /// A `const fn` rather than the derived `Default`, so
    /// [`PublishRuleParams::new`](crate::domain::publish::PublishRuleParams::new)
    /// can stay `const` — the same shape `RegionTaxReadiness::empty` takes for
    /// the same reason.
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            published_targets: BTreeMap::new(),
            inbound_edges: BTreeSet::new(),
        }
    }

    /// Name what the store found.
    #[must_use]
    pub const fn new(
        published_targets: BTreeMap<Uuid, Option<i32>>,
        inbound_edges: BTreeSet<Uuid>,
    ) -> Self {
        Self {
            published_targets,
            inbound_edges,
        }
    }

    /// Is `plan` a published plan?
    #[must_use]
    pub fn is_published(&self, plan: Uuid) -> bool {
        self.published_targets.contains_key(&plan)
    }

    /// The rank `plan` publishes, when it is published and carries one.
    #[must_use]
    pub fn rank_of(&self, plan: Uuid) -> Option<i32> {
        self.published_targets.get(&plan).copied().flatten()
    }

    /// The published plans pointing at the subject.
    #[must_use]
    pub fn inbound(&self) -> &BTreeSet<Uuid> {
        &self.inbound_edges
    }
}

/// The change graph is authorable: every edge names a published plan, and both
/// ends of every edge carry a rank (`inst-pc-targets`, `inst-pc-mutual`,
/// `inst-pc-rank`, D-54).
///
/// **One rule, three codes**, rather than three rules over one subject. The
/// three findings are readings of the same walk over the same edge list, and a
/// split would make an author remediate one contract in three reports — while
/// the walk would have to be repeated, with the index it needs handed to each
/// copy.
///
/// # What is deliberately absent
///
/// **The retirement case.** An edge whose target is later *retired* is **inert**,
/// not invalid: D-24 puts the re-check at change time in Subscriptions, because a
/// target's retirement is not an event the source's frozen revision can react to.
/// A rule refusing it here would refuse a publish for a fact the author cannot
/// fix and Subscriptions already handles.
///
/// **The `in_place` / `cancel_plus_new` classification.** D-93 removed the
/// publish-time stamp; see [`pricing_plan`](crate::infra::storage::migrations::m20260821_000021_create_pricing_plan).
#[domain_model]
#[derive(Clone, Debug, Default)]
pub struct ChangeGraphAuthorable {
    /// What the store found about the other plans; see [`ChangeTargetIndex`].
    pub index: ChangeTargetIndex,
}

impl ValidationRule<PlanShape> for ChangeGraphAuthorable {
    fn name(&self) -> &'static str {
        "inst-pc-targets"
    }

    fn evaluate(&self, subject: &PlanShape, report: &mut ValidationReport) {
        let contract = &subject.change_contract;
        let subject_name = subject.plan_id.get().to_string();

        // D-54's reverse guard runs whether or not this plan authors edges of
        // its own: what it judges is the plan's rank against the edges pointing
        // **at** it, and a plan that dropped its last outbound edge is exactly
        // the one most likely to drop its rank in the same breath.
        if contract.comparability_rank.is_none() && !self.index.inbound().is_empty() {
            let referencing: Vec<String> = self
                .index
                .inbound()
                .iter()
                .map(ToString::to_string)
                .collect();
            report.violate(
                COMPARABILITY_RANK_REVOKED,
                subject_name.clone(),
                format!(
                    "this plan re-publishes with no comparabilityRank while published \
                     allowedChangeTargets edges still reference it ({}). Every one of those edges \
                     would become unclassifiable at read time, which is the drift D-23 cut \
                     rule-based targets to avoid; remove the inbound edges first (D-54)",
                    referencing.join(", ")
                ),
            );
        }

        if !contract.offers_self_service_change() {
            return;
        }

        // K4, the source half. Reported once for the plan rather than once per
        // edge: the omission is one field on one plan, and an author told the
        // same thing five times has five places to look and one thing to fix.
        if contract.comparability_rank.is_none() {
            report.violate(
                COMPARABILITY_RANK_REQUIRED,
                subject_name.clone(),
                "a plan participating in self-service change MUST carry a comparabilityRank: it \
                 is a single tenant-wide scale and the runtime classification of a change is \
                 uncomputable without both ends of the edge (K4, inst-pc-rank)"
                    .to_owned(),
            );
        }

        for target in contract.targets() {
            if !self.index.is_published(*target) {
                report.violate(
                    CHANGE_TARGET_UNPUBLISHED,
                    subject_name.clone(),
                    format!(
                        "allowedChangeTargets names {target}, which has no published revision. \
                         Targets are explicit published planIds -- a rule-based target resolves \
                         only at read time and defeats every publish-time guarantee here (D-23, \
                         inst-pc-targets)"
                    ),
                );
                continue;
            }
            if self.index.rank_of(*target).is_none() {
                report.violate(
                    COMPARABILITY_RANK_REQUIRED,
                    subject_name.clone(),
                    format!(
                        "allowedChangeTargets names {target}, which publishes no \
                         comparabilityRank. Both ends of an edge carry one or the runtime \
                         classification A -> B is uncomputable (K4, inst-pc-mutual)"
                    ),
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The entitlement grant set (§3 Entitlement Grant Set, §6, D-41).
// ---------------------------------------------------------------------------

/// A per-phase grant-set key names no phase of the plan's schedule, or a
/// non-phased plan authors one at all (§5, `inst-gs-perphase`, D-41, D-19).
pub const GRANT_SET_PHASE_UNKNOWN: &str = "GRANT_SET_PHASE_UNKNOWN";

/// One grant set as §17.6 shapes it (`inst-gs-shape`): feature flags and quotas.
///
/// **Semantics are not defined here** — §3 step 2 says so in as many words.
/// Subscriptions consumes these as Entitlements; the catalog's whole obligation
/// is to publish the pair of maps and freeze them.
///
/// Two maps rather than one of a sum type, because they are two different
/// questions with two different value spaces: a feature is on or off, a quota is
/// a number. A single map keyed by name would let `"seats": true` and
/// `"sso": 40` past the type, and neither has a reading downstream.
///
/// `BTreeMap` rather than `HashMap` throughout, for the reason `put_descriptor_set`
/// gives: the projection and the content pin both render this, and a
/// randomly-seeded iteration order would make one plan pin two different ways in
/// two replicas.
#[domain_model]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GrantSet {
    /// `featureFlag: bool` entries.
    pub feature_flags: BTreeMap<String, bool>,
    /// `quotaKey: value` entries.
    pub quotas: BTreeMap<String, i64>,
}

impl GrantSet {
    /// Does this set state anything at all?
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.feature_flags.is_empty() && self.quotas.is_empty()
    }
}

/// What a plan revision publishes about entitlements (§6, D-41).
///
/// # The `PlanTier` reference is carried **beside** the resolved set, not
/// instead of it
///
/// `inst-gs-resolved` requires both: the resolved set so Subscriptions'
/// provisioning does not re-derive from the taxonomy at runtime, and the
/// reference so the resolution is auditable. Carrying only the reference would
/// make every provisioning read depend on a taxonomy that D-27 says can change
/// after publish — which is the drift `grants_divergent` exists to flag, turned
/// from a signal into a silent retro-change.
#[domain_model]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EntitlementGrants {
    /// The `PlanTier` policy this set resolved from, when it was resolved from
    /// one. Auditability only — nothing reads it to derive entitlements.
    pub plan_tier_ref: Option<String>,
    /// The plan-level set: what applies to a phase with no entry of its own.
    pub plan_level: GrantSet,
    /// Optional per-phase sets, keyed by `phaseId` (D-41).
    pub per_phase: BTreeMap<Uuid, GrantSet>,
}

impl EntitlementGrants {
    /// Has anything been authored at all?
    #[must_use]
    pub fn is_absent(&self) -> bool {
        self.plan_tier_ref.is_none() && self.plan_level.is_empty() && self.per_phase.is_empty()
    }

    /// The **complete** `phase → grant-set` map this revision publishes
    /// (`inst-gs-perphase`).
    ///
    /// Every phase of `schedule` maps to its effective set: the authored
    /// per-phase entry where one exists, else the plan-level set. Materialized
    /// at publish rather than resolved at read, so Subscriptions answers "what
    /// is granted in the active phase at `t`" with **one lookup** and never
    /// merges fallbacks at runtime — the `phase → price` map's own shape, and the
    /// reason it is a map at all rather than a pair of fields a consumer joins.
    ///
    /// An **unphased** plan produces an empty map rather than a one-entry one:
    /// its grant set is the plan-level set, which is already published beside
    /// this, and a map with one synthetic key would invite a consumer to key off
    /// a phase D-19 makes implicit.
    #[must_use]
    pub fn phase_map(&self, schedule: &[PlanPhase]) -> BTreeMap<Uuid, GrantSet> {
        if schedule.len() < 2 {
            return BTreeMap::new();
        }
        schedule
            .iter()
            .map(|phase| {
                let id = phase.phase_id.get();
                let set = self
                    .per_phase
                    .get(&id)
                    .cloned()
                    .unwrap_or_else(|| self.plan_level.clone());
                (id, set)
            })
            .collect()
    }
}

/// Per-phase grant-set keys name phases the plan actually has
/// (`inst-gs-perphase`, D-41, D-19).
///
/// Two refusals under one code, because §5 gives them one:
///
/// - a key naming no phase of this plan's schedule — a dangling `phaseId`, which
///   would publish a map entry no `phase` scope-key axis value can ever select;
/// - **any** per-phase entry on a **non-phased** plan, which D-19 defines as one
///   whose schedule is only the implicit terminal phase. An entry keyed to that
///   sole phase fails too: it must be authored as the plan-level set, or the
///   plan would publish the same fact twice with nothing saying which wins.
///
/// # What this rule does **not** check
///
/// The referential half — that each feature, quota or `PlanTier` policy is
/// **defined in the registry** (`inst-gs-referential`, `GRANT_REF_UNDEFINED`) —
/// is absent, and it is absent for `PLANTIER_DIVERGENT`'s reason rather than by
/// oversight: this gear **has no registry client at all**
/// ([`crate::domain::ports`] holds the `CatalogVersion` registry and nothing
/// else), so the rule has no world to check against. Registering it over an
/// empty lookup would either refuse every grant set ever authored or pass every
/// one — and a rule that always passes is indistinguishable from a rule that
/// holds. Named here rather than written, exactly as
/// [`plan_rules`](crate::domain::plan_rules) names `SKU_NOT_PUBLISHED` and the
/// divergence half of `PLANTIER_MISSING`.
///
/// **`inst-gs-drift` is absent for the same reason, one layer out.** D-27 has
/// the catalog consume the registry's *tier-policy-change signal* and flag every
/// affected published plan `grants_divergent` in `pricing_operator_flag` (D-85),
/// with the `pricing.contracts.grants_divergent` alarm beside it. The store
/// exists — `pricing_operator_flag` is that table — and the **signal does not**: there is
/// no registry client to receive it and no inbound seam that could carry it. A
/// flag written on no signal would be an operator remediation prompt for a drift
/// nobody detected, which is worse than the gap, and D-85 is explicit that the
/// flag must never ride the versioned read model instead. Named here so the
/// slice that lands the registry seam lands this with it.
#[domain_model]
#[derive(Clone, Copy, Debug, Default)]
pub struct GrantSetPhasesKnown;

impl ValidationRule<PlanShape> for GrantSetPhasesKnown {
    fn name(&self) -> &'static str {
        "inst-gs-perphase"
    }

    fn evaluate(&self, subject: &PlanShape, report: &mut ValidationReport) {
        let grants = &subject.entitlement_grants;
        if grants.per_phase.is_empty() {
            return;
        }
        let schedule = subject.phases.phases();
        // D-19: a schedule of one is the implicit terminal phase, which makes
        // the plan **non-phased**. Checked before the per-key walk so the
        // refusal can say which of the two failures it is -- an author told
        // "unknown phase" about a phase their plan does have would go looking
        // for a typo that is not there.
        if schedule.len() < 2 {
            for phase_id in grants.per_phase.keys() {
                report.violate(
                    GRANT_SET_PHASE_UNKNOWN,
                    subject.plan_id.get().to_string(),
                    format!(
                        "this plan is non-phased -- its schedule is only the D-19 implicit \
                         terminal phase -- so it may author no per-phase grant set, including \
                         the one keyed {phase_id} to that sole phase. Author it as the \
                         plan-level set instead, or the plan publishes one fact twice with \
                         nothing saying which wins (inst-gs-perphase, D-41)"
                    ),
                );
            }
            return;
        }

        let known: BTreeSet<Uuid> = schedule.iter().map(|p| p.phase_id.get()).collect();
        for phase_id in grants.per_phase.keys() {
            if known.contains(phase_id) {
                continue;
            }
            report.violate(
                GRANT_SET_PHASE_UNKNOWN,
                subject.plan_id.get().to_string(),
                format!(
                    "the per-phase grant set is keyed {phase_id}, which names no phase of this \
                     plan's schedule. The entry would publish into a map no value of the phase \
                     scope-key axis can ever select (inst-gs-perphase, D-41)"
                ),
            );
        }
    }
}

// ---------------------------------------------------------------------------
// The rating compatibility contract (§3 Rating Compatibility Contract, §8).
// ---------------------------------------------------------------------------

/// The rules `inst-rc-union` asserts a publish runs, by instruction id.
///
/// §3 step 2 is explicit that this bundle **delegates**: it "asserts the
/// **union**" of the owning slices' registered rules and its enumeration is
/// "illustrative", with exhaustiveness left to those slices. So this is not a
/// rule and mints no code — it is the union written down once, and
/// [`crate::domain::publish`]'s own test asserts the aggregate pipeline actually
/// carries every member.
///
/// # Why a roster and not a rule
///
/// A rule re-checking what thirteen other rules already check would be a second
/// registration of each — free to disagree with the one that owns it, which is
/// the exact hazard `price_record.rs` cites for keeping `billing_timing` a
/// `String`. What the contract actually promises Rating is that the checks *are
/// run on the publish path*, and the only thing that can break that promise is a
/// rule quietly leaving the pipeline. A roster compared against
/// [`ValidationPipeline::rule_names`](crate::domain::validation::ValidationPipeline::rule_names)
/// catches precisely that and nothing else.
///
/// # What each member answers, in §3 step 2's own order
///
/// `modelKind` + `quantitySource` + `packageSize`/`packagePrice` (Slice 3);
/// `tierAggregationWindow`/`billingGranularity` on usage rows (Slice 3);
/// `aggregationFunction`/`aggregationGranularity`/`maxHoldGranules` on non-`sum`
/// rows (Slice 3, D-44); meter injectivity and the descriptor set (Slice 2).
///
/// **`tierQualificationWindow` (Slice 10, D-40) is named by §3 step 2 and is
/// absent from this roster**, because the rule that would judge it does not
/// exist: `api::rest::prices::refuse_unlanded_primitives` refuses the field at
/// the boundary instead, and a roster entry for an unregistered rule would make
/// this census fail on a gap Slice 10 owns rather than on a rule that left.
pub const RATING_COMPAT_UNION: &[&str] = &[
    // Slice 3, the row plane.
    "inst-mk-explicit",
    "inst-mk-required",
    "inst-mk-forbidden",
    "inst-mk-chargekind",
    "inst-pk-fields",
    "inst-pk-window",
    "inst-tb-window",
    "inst-la-fields",
    "inst-la-granularity",
    "inst-la-maxhold",
    // Slice 2, the plan plane.
    "inst-cmp-injective",
    "inst-ds-required",
];
