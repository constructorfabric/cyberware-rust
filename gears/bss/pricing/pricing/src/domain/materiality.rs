//! The materiality evaluator — `cpt-cf-bss-pricing-algo-materiality`
//! (`design/05-governance.md` §3), steps 1, 2, 3, 3a and 4.
//!
//! One question: does this change set need a second principal, or may it
//! publish on one? [`evaluate`] answers it, and the design set's G1 fixes the
//! default direction — **material unless proved otherwise**. Five rules make
//! a change material, and this module is exactly those five:
//!
//! | Rule | The world it refuses to auto-publish |
//! |---|---|
//! | `inst-mat-registered` | the act, or its content, is on §3 step 4's trigger list |
//! | `inst-mat-failsafe` | the tenant has no explicitly configured threshold policy |
//! | `inst-mat-first` | the plan has no published baseline, so no delta is computable |
//! | `inst-mat-newrow` | a row in the change set has no baseline row of its own |
//! | `inst-mat-percurrency` | a row's delta reaches its **own currency's** threshold, or its currency has no entry |
//!
//! Five rules and **five** reasons, and the pairing is not one-to-one:
//! `inst-mat-percurrency` answers with two of them, because "no entry for this
//! currency" and "reached the entry it has" are two states an operator acts on
//! differently. [`MaterialityReason`] is the roster.
//!
//! [`MaterialityVerdict::AutoPublishable`] is what is left when all five
//! decline, and §3 states that residue in one sentence: *"a pure amount change on
//! unchanged geometry, below an explicitly configured threshold, not a first
//! publish."* It is the arm [`PublishAuthorization::auto_publishable`] says must
//! not be reached by an evaluator that cannot answer.
//!
//! [`PublishAuthorization::auto_publishable`]:
//! crate::domain::publish::PublishAuthorization::auto_publishable
//!
//! # The verdict carries which rule fired, and that is not decoration
//!
//! A bool would make the arms one event. Three things need them kept
//! apart. `pricing_approval.materiality` is a stored column an auditor reads
//! years later, and "material" alone does not say whether a reviewer was
//! required because the tenant never configured a policy or because a new
//! market row appeared. An operator told only "approval required" cannot tell
//! which of them they could act on — only the threshold ones are a configuration
//! they own, and a registered trigger is not a configuration at all. And a test
//! suite that asserts materiality without asserting *which rule produced it*
//! cannot tell an implemented arm from a missing one: every such assertion
//! passes against `fn evaluate(..) -> Material`. The reason is
//! what makes each arm removable-and-observable one at a time.
//!
//! # Two things named "baseline", and they are not the same thing
//!
//! [`crate::domain::plan_shape::PublishedBaseline`] already exists and is the
//! **plan shape's** referent: the terminal phase id, the phases in use and the
//! published availability window, for three Slice-2 rules that are comparisons.
//! It carries no price rows at all, so `inst-mat-newrow` — "has *this row* a
//! baseline of its own" — cannot be asked of it.
//!
//! [`PublishedPriceBaseline`] is the price-row referent, and it is a separate
//! type with a distinguishable name rather than a field added to the other one.
//! Two rules with different subjects sharing one carrier is how a rule silently
//! acquires a second owner, and a reader who greps `PublishedBaseline` should
//! not have to work out which of two meanings the hit has.
//!
//! # An empty policy object is not a configured policy
//!
//! [`ThresholdPolicy::of_entries`] answers `None` for an empty entry set, so
//! `inst-mat-failsafe` sees it as the absence it is. The design set makes the
//! per-currency reading explicit — *"the G1 fail-safe applies per currency, not
//! per policy object"* — and a policy object carrying zero entries has a
//! threshold for no currency at all. Admitting one as "configured" would let a
//! tenant clear every entry and auto-publish everything, which is the fail-open
//! this rule exists to close.
//!
//! # Which part of §3 each piece answers
//!
//! | Requirement | Answered by |
//! |---|---|
//! | Per-currency thresholds and the delta comparison (§3 step 3, `inst-mat-percurrency`) | [`evaluate`]'s row walk, against [`ThresholdPolicy::entry`] |
//! | The D-115 delta domain | [`delta::row_delta`] |
//! | The registered always-material triggers (§3 step 4, `inst-mat-registered`) | [`triggers`] |
//!
//! **Configuring a threshold is itself an approved act.** **D-10 makes the
//! `GET/PUT /bss-pricing/v1/config/approval-threshold-policy` an always-material
//! act**, so a tenant cannot configure a threshold without first completing an
//! approved unit — the PUT's own unit is `Trigger::ThresholdPolicyDiff`. *"A
//! single-reviewer tenant simply leaves the policy unset ⇒ everything material"*
//! is the design set's own sentence and what this module does.
//!
//! **The policy store, the delta domain, the trigger registry and this evaluator
//! are one mechanism, not four.** An evaluator that examined neither the threshold
//! nor the triggers would answer [`MaterialityVerdict::AutoPublishable`] for an
//! **above**-threshold change and for **every** registered trigger — precisely the
//! fail-open the design set has closed by hand five times (D-50, D-62, D-104,
//! D-109, D-115).
//!
//! **Five registered triggers name subjects this crate cannot author** — a bulk
//! group move, a retirement that unwinds a live cutover, the two gate-clearing
//! republishes, a prepaid grant's non-price fields. They are not omitted:
//! [`triggers::Trigger`] declares each with its owning slice and answers
//! [`triggers::Trigger::subject_exists_in_this_crate`] `false`, so the set does not
//! read as half-built and no token claims a writer it has not got.
//!
//! **All four window acts are decided here, and the division of labour is the only
//! thing left outside.** [`crate::infra::window`] decides *inside its mutating
//! transaction* whether a `PATCH` shortens (D-176 — a classification made before the
//! transaction opened is a hint) and hands that fact over as
//! [`ChangeSet::of_window_mutation`]'s `act`; [`evaluate`] turns it into a verdict.
//! No caller carries a second copy of §3 step 4: a surface that stamped
//! `material(AlwaysMaterialTrigger)` by hand would be a second owner of the rule,
//! free to disagree with this one.
//!
//! What a **window mutation** brings to the row walk is worth stating, because it is
//! the only production caller that reaches it: its change set is the plan's published
//! rows, unchanged, so every delta is zero and no bar is ever reached. The rule that
//! decides a schedule or a lengthening is therefore `inst-mat-percurrency`'s fail-safe
//! half — has this plan's currency an entry at all.
//!
//! **Implementing half a registered instruction gives it two owners, free to
//! disagree the day the other half lands.** Both halves of `inst-mat-percurrency`
//! — the per-currency comparison and the no-entry fail-safe — are [`evaluate`]'s,
//! so the instruction has one owner. [`crate::domain::publish`] makes the same
//! argument for `inst-tp-distinct`.

use std::collections::HashMap;

use toolkit_macros::domain_model;

use crate::domain::money::CurrencyCode;
use crate::domain::price_record::PriceRecord;
use crate::domain::publish::PublishUnitKind;
use crate::domain::scope_key::ScopeKey;
use time::OffsetDateTime;

pub mod delta;
pub mod triggers;

/// Why a change set is material.
///
/// Five arms, all five of which [`evaluate`] answers with. There is no `Other` and
/// no catch-all — a reason means a rule, and a rule that arrives without a name
/// arrives without an owner.
///
/// # Why "over its threshold" is an arm of its own
///
/// [`Self::ThresholdReached`] was absent while the comparison was, and its absence
/// made the module say the opposite of what it meant: a row that **reached** its
/// configured bar was reported `noConfiguredThreshold`, which is the state in which
/// no bar exists. That is the exact fault [`evaluate`]'s trigger ordering is argued
/// from forty lines on — *"told `noConfiguredThreshold` about a window cancellation,
/// an operator would configure a threshold and watch the cancellation stay
/// material"* — and it was committed one arm over. It also defeated the per-currency
/// fail-safe's own purpose: a reader of the stored column could not tell *"GBP has
/// no entry"* from *"USD moved 900 over its bar of 500"*, and those are the two
/// answers `inst-mat-percurrency` exists to keep apart — one is a configuration the
/// operator owns, the other is a change they must have reviewed.
#[domain_model]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum MaterialityReason {
    /// `inst-mat-failsafe` — the tenant configured no threshold policy, so
    /// there is nothing to be below.
    ///
    /// **And `inst-mat-percurrency`'s fail-safe half**: a policy is configured but
    /// *this row's currency* has no entry in it. The design set folds the two
    /// deliberately — *"the G1 fail-safe applies **per currency**, not per policy
    /// object"* — because what the operator can act on is identical: configure an
    /// entry. It does **not** cover a row that reached an entry it has;
    /// [`Self::ThresholdReached`] is that.
    NoConfiguredThreshold,
    /// `inst-mat-first` — the plan has no published baseline. A first publish
    /// has no prior state to delta against, so no threshold can be applied to
    /// it at all.
    FirstPublish,
    /// `inst-mat-newrow` — at least one row in the change set is on a scope key
    /// the baseline does not carry. A new currency/region/phase/chargeKind row
    /// has no predecessor to compare with.
    RowWithoutBaseline,
    /// `inst-mat-registered` — the act, or its content, is on §3 step 4's
    /// **registered always-material trigger** list, so it is material whatever a
    /// threshold says and whatever delta it carries.
    ///
    /// **[`evaluate`] produces it.** [`ChangeSet`] carries the act
    /// ([`ChangeSet::of_act`]), declared by the surface performing it, because only
    /// that surface — inside its own mutating transaction — can tell a shortening
    /// from an extension. The content-derived half, D-115's two and the
    /// `grandfatherUntil` horizon, the evaluator derives itself.
    ///
    /// **Which** trigger fired rides beside this token as
    /// [`MaterialityVerdict::trigger`], and that paragraph is D-232's remaining
    /// half rather than a widening for its own sake.
    ///
    /// **The unit's own fields cannot stand in for it.** Naming the subject
    /// (`subject_kind` plus the subject's id) and the audit record's state pair are
    /// both real operands, and neither answers the question D-104 registers **two**
    /// bundle triggers to answer: `subject_kind` is `bundle` for a component swap
    /// and `bundle` for a rev-share re-split, and a composition publish's state
    /// pair is identical for both. An operator reading the approval record would
    /// not be able to tell a change to what the customer receives from a change to
    /// what a vendor is paid — *"an operator reading the approval record should not
    /// have to infer it"*.
    ///
    /// It is not a **second vocabulary**: §6 already describes this column's
    /// payload as *"per-currency deltas, tripped rows, **trigger source**"*, and
    /// the token is [`triggers::Trigger::as_str`]'s, which the registry owns and
    /// `triggers_tests` holds distinct.
    AlwaysMaterialTrigger,
    /// `inst-mat-percurrency` proper — a row's delta **reaches** the threshold its
    /// own currency's entry states (§3's G3: any row trips the whole change).
    ///
    /// The arm the comparison needed and did not have. It is deliberately **not**
    /// distinguished by direction or by basis: §3 thresholds the size of a move and
    /// says nothing about its sign, and an operator's remedy — have a second
    /// principal look, or configure a different bar — is the same either way.
    ///
    /// **Which** row and which currency tripped is not in the token, and that is a
    /// gap rather than a decision. §6 describes the stored payload as *"per-currency
    /// deltas, tripped rows, trigger source"*, and this enum is a `Copy` roster of
    /// unit variants that [`Self::ALL`] and [`Self::as_str`] both range over — so the
    /// operands would have to ride the jsonb beside the token, which is
    /// `api::rest::approvals::MaterialityView`'s shape and not this one. Reported as
    /// owed rather than half-built here.
    ThresholdReached,
}

impl MaterialityReason {
    /// Every reason, stable order — the order §3 states the rules in.
    pub const ALL: &'static [Self] = &[
        Self::NoConfiguredThreshold,
        Self::FirstPublish,
        Self::RowWithoutBaseline,
        Self::AlwaysMaterialTrigger,
        Self::ThresholdReached,
    ];

    /// The token that lands in `pricing_approval.materiality`.
    ///
    /// The design set describes that column's payload — *"per-currency deltas,
    /// tripped rows, trigger source"* (§6) — but declares no schema for it. **Two of
    /// those three are now computed and still not stored**, which is a gap this group
    /// reports rather than the absence the sentence here used to claim: [`evaluate`]
    /// walks per-currency deltas and knows which row tripped, and what it hands on is
    /// the rule that fired. The token is minted here rather than at whichever surface
    /// stores it first, so the spellings have one owner.
    ///
    /// **A token, not a wire code.** §5's Problem-response list is the code
    /// vocabulary a client branches on (`THRESHOLD_INVALID`,
    /// `APPROVAL_CONTENT_MISMATCH`, …) and none of these is in it; these are the
    /// content of a `jsonb` column §6 leaves unschematised. Adding [`Self::ThresholdReached`]
    /// is therefore not minting a code the design set does not name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NoConfiguredThreshold => "noConfiguredThreshold",
            Self::FirstPublish => "firstPublish",
            Self::RowWithoutBaseline => "rowWithoutBaseline",
            Self::AlwaysMaterialTrigger => "alwaysMaterialTrigger",
            Self::ThresholdReached => "thresholdReached",
        }
    }
}

/// The row that reached its bar, in its own currency, and by how much — §6's
/// *"per-currency deltas, tripped rows"* (D-187).
///
/// The three facts a `FinanceReviewer` needs in order to sign for a
/// [`MaterialityReason::ThresholdReached`] unit, and the three the evaluator holds at
/// the moment it answers: it walks rows in their own currency, and [`compare`] knows
/// which move reached the bar. Before D-187 it threw the move away, so a stored verdict
/// said a bar was reached and could not say which row, in which currency, or by how
/// much.
///
/// **The move, not a re-derivation of it.** `from_minor`/`to_minor` are the operand that
/// actually tripped — which for a band vector is *one* band of several, and the one an
/// operator has to look at. Re-deriving it at read time cannot work: the policy may have
/// moved since, and §3 evaluates materiality **once at submit**, so a re-derivation would
/// answer a different question from the one that was signed.
#[domain_model]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrippedRow {
    /// The price row whose move reached the bar.
    pub price_id: uuid::Uuid,
    /// The currency the bar belongs to. `inst-mat-percurrency` compares each row in
    /// its own currency, so the currency is part of *which bar was reached* and not
    /// decoration.
    pub currency: CurrencyCode,
    /// The baseline amount, in the units [`Self::scale`] names.
    pub from_minor: i64,
    /// The proposed amount, same units.
    pub to_minor: i64,
    /// Which units the two amounts above are in (D-311).
    ///
    /// **Carried, not converted**, and the choice is the one
    /// [`delta::AmountMove::reaches_absolute`] already makes one layer down:
    /// the bar is raised into the move's scale rather than the move lowered
    /// into the bar's. Lowering here would floor `$0.230777165` — a `per_unit`
    /// rate, the entire reason [`crate::domain::money::RateMinor`] exists — to
    /// `0`, so a reviewer would read a real rate change as no change at all;
    /// and the verdict is stored, read years later, and re-computed by an
    /// auditor, which it cannot be from an operand recorded in units the
    /// comparison did not use.
    ///
    /// Without it the field pair is unreadable rather than merely imprecise: a
    /// nano-minor move under a minor-unit label is a factor of `10⁹` out, which
    /// is what the second principal signing a `thresholdReached` unit would be
    /// signing for.
    pub scale: delta::MoveScale,
}

/// What the evaluator answers: `material` with its reason, or
/// `auto_publishable`.
///
/// **Not `Copy`, and the cost is deliberate** (D-187 clause (2)). [`MaterialityReason`]
/// is a `Copy` roster its own `ALL` ranges over, and this type was `Copy` only because it
/// held one. Carrying §6's declared operands means carrying a [`CurrencyCode`], which is
/// a heap value — a tripped row's id alone could have ridden as a `Uuid` and stayed
/// `Copy`, and would have left the reviewer unable to tell which currency's bar was
/// reached, which is the distinction `inst-mat-percurrency` is built around. The
/// verdict is cloned rather than copied, and the register priced that.
///
/// # The trigger rides beside the reason, and it is the operand D-232 left owed
///
/// [`MaterialityReason::AlwaysMaterialTrigger`] is one token for eighteen acts.
/// While that was all a verdict carried, `Trigger::as_str` had **no production
/// consumer anywhere in the crate** and every registered act rendered
/// byte-identically whichever member of [`triggers::Trigger`] declared it — which
/// is precisely why D-321 refused to give `infra::bundle::rev_share_change_set` a
/// caller: declaring `RevenueShareChange` instead of `BundleComposition` would have
/// moved no observable byte and existed only to satisfy the census that reads it.
///
/// [`Self::trigger`] is where the answer goes, and it reaches **every** registered
/// act rather than only the bundle pair: a window cancel is now distinguishable
/// from a shortening, a horizon tightening from a plan retirement, a pure-shape
/// revision from a policy diff — all of which shared one stored token.
#[domain_model]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MaterialityVerdict {
    /// A second principal is required. The publish opens an approval unit.
    Material {
        /// Which rule made it material.
        reason: MaterialityReason,
        /// The row that reached its bar, when a row is what answered.
        ///
        /// `None` for every other reason, and that is not an omission:
        /// `alwaysMaterialTrigger` is an answer about the **act** and
        /// `noConfiguredThreshold` one about the **policy**, so neither names a row
        /// that tripped — filling it in with the change set's first row would put a
        /// row in front of a reviewer as though it had tripped something.
        tripped: Option<TrippedRow>,
        /// Which registered trigger fired, when a registered trigger is what
        /// answered.
        ///
        /// `Some` exactly under [`MaterialityReason::AlwaysMaterialTrigger`] and
        /// `None` under every other reason, and the biconditional is the property
        /// rather than a convention: a threshold or a fail-safe is not an act, so
        /// there is no trigger to name, and an `alwaysMaterialTrigger` with no
        /// trigger is the state this member exists to make unrepresentable in the
        /// record. Nothing in the type enforces it — [`Self::material`] would
        /// accept the reason with no trigger — so
        /// `materiality_tests::a_registered_act_names_its_trigger_and_nothing_else_does`
        /// walks every arm [`evaluate`] can take and asserts it. That is the same
        /// arrangement [`Self::tripped_row`] takes for its own operand, and it is
        /// stated here rather than left to the constructor's doc because the
        /// **reader** of a stored verdict is who the property is for.
        trigger: Option<triggers::Trigger>,
    },
    /// The change may publish on one principal. See the module doc for the five
    /// rules that must all have declined for this to be the answer.
    AutoPublishable,
}

impl MaterialityVerdict {
    /// Material, for `reason`, with no row and no act to name.
    ///
    /// The constructor for the three reasons that are neither: `firstPublish`,
    /// `noConfiguredThreshold` and `rowWithoutBaseline`.
    /// [`MaterialityReason::ThresholdReached`] uses [`Self::tripped_row`] and
    /// [`MaterialityReason::AlwaysMaterialTrigger`] uses [`Self::triggered`], so a
    /// caller cannot answer either and forget its operand by taking the shorter
    /// constructor.
    #[must_use]
    pub const fn material(reason: MaterialityReason) -> Self {
        Self::Material {
            reason,
            tripped: None,
            trigger: None,
        }
    }

    /// Material because `trigger` is on §3 step 4's registered list.
    ///
    /// The **only** producer of [`MaterialityReason::AlwaysMaterialTrigger`] in
    /// this crate, which is what makes the reason and the act unable to disagree:
    /// there is no path that mints the token without the trigger that earned it.
    /// [`Self::material`] can still be handed the reason and the compiler cannot
    /// stop it — [`MaterialityReason`] is a `Copy` roster [`MaterialityReason::ALL`]
    /// ranges over, so folding the trigger into the variant would cost that roster
    /// its shape (D-187 priced the same trade for the tripped row and made the same
    /// call) — so the guard is a case rather than a type, and it is named on
    /// `Material`'s own `trigger` member.
    #[must_use]
    pub const fn triggered(trigger: triggers::Trigger) -> Self {
        Self::Material {
            reason: MaterialityReason::AlwaysMaterialTrigger,
            tripped: None,
            trigger: Some(trigger),
        }
    }

    /// Material because `tripped` reached its currency's bar.
    #[must_use]
    pub const fn tripped_row(tripped: TrippedRow) -> Self {
        Self::Material {
            reason: MaterialityReason::ThresholdReached,
            tripped: Some(tripped),
            trigger: None,
        }
    }

    /// Does this change need a second principal?
    #[must_use]
    pub const fn is_material(&self) -> bool {
        matches!(self, Self::Material { .. })
    }

    /// The rule that fired, or `None` on an auto-publishable change.
    #[must_use]
    pub const fn reason(&self) -> Option<MaterialityReason> {
        match self {
            Self::Material { reason, .. } => Some(*reason),
            Self::AutoPublishable => None,
        }
    }

    /// The row that reached its bar, when one did.
    #[must_use]
    pub const fn tripped(&self) -> Option<&TrippedRow> {
        match self {
            Self::Material { tripped, .. } => tripped.as_ref(),
            Self::AutoPublishable => None,
        }
    }

    /// The registered trigger that fired, when one did — §6's *"trigger source"*.
    #[must_use]
    pub const fn trigger(&self) -> Option<triggers::Trigger> {
        match self {
            Self::Material { trigger, .. } => *trigger,
            Self::AutoPublishable => None,
        }
    }
}

/// The tenant's approval-threshold policy: one entry per currency it has
/// explicitly configured.
///
/// The entry's **value** was deliberately not modelled while nothing compared
/// against it — "a field no comparison reads is a shape guessed before the code
/// that would have constrained it". [`evaluate`] compares against it now, so the
/// entry carries its basis, and the shape is the one `pricing_approval_threshold`
/// stores rather than one guessed for it.
///
/// Two load-bearing properties: whether a policy was configured at all
/// (`inst-mat-failsafe`), and whether **this row's currency** has an entry
/// (`inst-mat-percurrency`'s fail-safe half). The second is why the type is keyed
/// per currency and not a single pair.
///
/// Its production caller is `infra::threshold::effective_policy`, which reads the
/// greatest version an independent principal approved.
#[domain_model]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThresholdPolicy {
    entries: Vec<ThresholdEntry>,
}

/// Which basis one currency's threshold is stated in.
///
/// **An enum rather than two `Option`s, and the compiler is the guard**: §6's
/// `{absolute_minor | percent}` is a choice, and a pair of options can represent
/// "both" and "neither" — the first leaves the evaluator picking one with nothing
/// in the design set saying which, and the second is an entry that thresholds
/// nothing while still counting as "this currency has an entry", which is
/// `inst-mat-percurrency`'s fail-safe switched off by an empty row. The store says
/// the same thing with `chk_pricing_approval_threshold_basis`; this says it where
/// no test is needed to prove it.
#[domain_model]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThresholdBasis {
    /// An absolute move, in the currency's own minor units.
    Absolute {
        /// §6's `absolute_minor ≥ 0`; zero is a real threshold and makes
        /// everything in that currency material.
        minor: i64,
    },
    /// A relative move, in **basis points** (`10_000` = 100%).
    ///
    /// Basis points rather than a percentage, per `pricing_approval_threshold`'s recorded
    /// decision: the design set declares no representation for §6's `percent > 0`,
    /// and D-104's `share_bp` / `platform_cut_bp` are the set's own idiom. An
    /// integer also keeps the comparison integral — no floating point beside
    /// money.
    Percent {
        /// §6's `percent > 0`, in basis points.
        bp: u32,
    },
}

/// One currency's threshold entry.
#[domain_model]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThresholdEntry {
    /// The ISO 4217 code this entry thresholds.
    pub currency: CurrencyCode,
    /// The basis, exactly one of two by construction.
    pub basis: ThresholdBasis,
}

impl ThresholdPolicy {
    /// A policy over `entries`, or `None` when there are none — an empty entry set
    /// is not a configured policy, for the reason the module doc gives.
    ///
    /// Duplicates collapse and **the first entry for a currency wins**. An entry
    /// set is a set, and two entries for one currency would be a policy with two
    /// answers for one row, whichever order they were read in. First-wins rather
    /// than last-wins because the store hands them back ordered by currency, so
    /// there is no "later" for a reader to appeal to; the surface refuses a
    /// duplicate key outright, which is where an operator hears about it.
    #[must_use]
    pub fn of_entries(entries: impl IntoIterator<Item = ThresholdEntry>) -> Option<Self> {
        let mut kept: Vec<ThresholdEntry> = Vec::new();
        for entry in entries {
            if !kept.iter().any(|held| held.currency == entry.currency) {
                kept.push(entry);
            }
        }
        (!kept.is_empty()).then_some(Self { entries: kept })
    }

    /// This currency's entry, or `None` — which is `inst-mat-percurrency`'s
    /// fail-safe half and not a missing lookup: *"a row whose currency has no
    /// threshold entry in the configured policy is material (the G1 fail-safe
    /// applies per currency, not per policy object)"*.
    #[must_use]
    pub fn entry(&self, currency: &CurrencyCode) -> Option<&ThresholdEntry> {
        self.entries
            .iter()
            .find(|entry| &entry.currency == currency)
    }

    /// The currencies with an explicit entry, in the order they were configured.
    pub fn currencies(&self) -> impl Iterator<Item = &CurrencyCode> {
        self.entries.iter().map(|entry| &entry.currency)
    }
}

/// Why a proposed threshold version is not one — the domain half of
/// `THRESHOLD_INVALID` (`design/05-governance.md` §5).
///
/// An enum rather than a string, for [`MaterialityReason`]'s reason: the surface
/// renders one wire code and the operator needs to know which of the shape rules
/// they broke, and a caller of [`ThresholdVersion::new`] that had to parse a
/// sentence to find out would be a second reading of the same refusal.
#[domain_model]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ThresholdRefusal {
    /// The proposal names no currency at all.
    ///
    /// The refusal itself is sound and local: an empty version writes **zero rows**,
    /// which `latest_version` cannot see, `read_version` cannot tell from a version
    /// nobody proposed, and an approval pin would cover nothing. So it is refused here,
    /// where the operator hears about it, rather than written and lost.
    ///
    /// **The capability gap behind it is paid, and the refusal still stands** — which
    /// is the pairing D-185 turns on. This doc used to report the gap as owed: §6's
    /// *"unset ⇒ two-person rule always"* is a **state**, and the store could only
    /// enter it as the bootstrap, because retiring a policy needed a version that says
    /// "no thresholds" and is distinguishable from a version nobody proposed. That
    /// version now exists — [`ThresholdVersion::tombstone`], stored in
    /// `pricing_approval_threshold_tombstone` — and it is emphatically **not** this
    /// refusal being relaxed. An empty `PUT` is still refused, because an empty entry
    /// set is indistinguishable from absence at every layer that would have to read it;
    /// a tombstone is a *positive* marker authored through its own door, minted with
    /// the next version number, pinned, and approved by an independent principal like
    /// every other policy diff.
    NoEntries,
    /// Two entries name one currency, which is a policy with two answers for one
    /// row.
    ///
    /// [`ThresholdPolicy::of_entries`] collapses duplicates first-wins because a
    /// *read* of the store must be total. A **proposal** is authored, so the
    /// operator is told rather than silently having one of their two entries
    /// dropped — the same split `of_entries`' own doc names.
    DuplicateCurrency(String),
}

impl ThresholdRefusal {
    /// The sentence the surface renders, naming the field the operator can move.
    #[must_use]
    pub fn detail(&self) -> String {
        match self {
            Self::NoEntries => "entries: a threshold policy version names at least one currency; \
                                an empty proposal writes no rows, so it is a version no approval \
                                can pin and no reader can distinguish from one nobody proposed"
                .to_owned(),
            Self::DuplicateCurrency(currency) => format!(
                "entries: currency {currency} appears twice; one currency has one threshold, or \
                 the evaluator has two answers for one row"
            ),
        }
    }
}

/// One **version** of the tenant's threshold policy, as an approval unit pins it.
///
/// # Why the pinned subject is a version and not a [`ThresholdPolicy`]
///
/// D-10 makes a policy PUT an always-material approval unit, so what a reviewer is
/// shown is a *proposal*: content that is not yet the tenant's policy and may never
/// become it. `pricing_approval` carries a `content_hash` and no content column, so
/// the pin has to cover something re-derivable — which is exactly what the versioned
/// store gives it (`pricing_approval_threshold`'s migration doc argues the shape). A
/// [`ThresholdPolicy`] carries no version and no `effective_from`, so it cannot name
/// the row set a pin was taken over; this can.
///
/// The two extra fields are hashed for that reason and not for completeness:
/// `version` is what [`crate::infra::threshold`] resolves the effective policy
/// through, and `effective_from` is authored — an operator who could move it after a
/// reviewer signed would move when the approved thresholds start applying.
#[domain_model]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThresholdVersion {
    version: u64,
    effective_from: OffsetDateTime,
    entries: Vec<ThresholdEntry>,
}

/// Every field of a [`ThresholdVersion`], handed over at once so a consumer that
/// must account for all of them cannot miss one.
///
/// [`ScopeKeyParts`](crate::domain::scope_key::ScopeKeyParts)' construction and
/// for its reason: the fields are private, so the only alternative is a set of
/// accessor calls — and a set of accessor calls is exactly what a fourth field
/// arrives without breaking. Produced by [`ThresholdVersion::parts`], which
/// destructures the version exhaustively, and consumed by
/// `approval::content_pin::put_threshold_version`, which destructures this
/// exhaustively. Widening the version therefore costs two compile errors and a
/// decision at the pin, which is the point.
pub(crate) struct ThresholdVersionParts<'a> {
    /// The version number — the `version` half of the store's primary key.
    pub version: u64,
    /// The instant the version starts applying.
    pub effective_from: OffsetDateTime,
    /// The entries, in the order the pin frames them.
    pub entries: &'a [ThresholdEntry],
}

impl ThresholdVersion {
    /// One proposed or stored version.
    ///
    /// The entry order is **the caller's**, and every caller hands them over sorted
    /// by currency: `threshold_repo::read_version` orders in SQL, and the surface
    /// sorts an authored set before it proposes one. That is load-bearing rather
    /// than tidy — the pin is taken over this rendering, so a version that hashed
    /// in the order an operator typed would re-derive to a different digest than it
    /// was signed under. It is not re-sorted here, because a constructor that
    /// quietly reordered its argument would leave the two callers free to disagree
    /// about the order and never find out.
    ///
    /// # Errors
    /// [`ThresholdRefusal::NoEntries`] for an empty entry set;
    /// [`ThresholdRefusal::DuplicateCurrency`] when two entries name one currency.
    pub fn new(
        version: u64,
        effective_from: OffsetDateTime,
        entries: Vec<ThresholdEntry>,
    ) -> Result<Self, ThresholdRefusal> {
        if entries.is_empty() {
            return Err(ThresholdRefusal::NoEntries);
        }
        for (at, entry) in entries.iter().enumerate() {
            if entries[..at]
                .iter()
                .any(|held| held.currency == entry.currency)
            {
                return Err(ThresholdRefusal::DuplicateCurrency(
                    entry.currency.as_str().to_owned(),
                ));
            }
        }
        Ok(Self {
            version,
            effective_from,
            entries,
        })
    }

    /// **The tombstone (D-185)**: a version that positively says this tenant has no
    /// thresholds.
    ///
    /// The way back to §6's *"unset ⇒ two-person rule always"*, which every tenant
    /// starts in and which — before this — no tenant could return to. It is a version
    /// like any other: minted with the next number, framed by the pin, reviewed by the
    /// always-material unit D-10 opens over every policy diff, and in force only once
    /// an independent principal has approved it and its `effective_from` has arrived.
    ///
    /// **Total, where [`Self::new`] is fallible**, and that is the shape of the
    /// decision rather than a convenience. `new`'s two refusals are both about an entry
    /// set — it is empty, or it names a currency twice — and a tombstone has no entry
    /// set to be wrong about. There is nothing here for an operator to get wrong and
    /// therefore nothing to report.
    ///
    /// **The empty entry vector is the representation, and it is the only one.**
    /// `new` refuses an empty set, so this constructor is the sole producer of a
    /// version with no entries and `entries().is_empty()` is a total, single-owner
    /// reading of "is this the tombstone" ([`Self::is_tombstone`]). A `bool` field
    /// beside the vector would be a second answer to a question the vector already
    /// answers, free to disagree with it — the shape this crate refuses on the store
    /// side for the same reason there is no `state` column on
    /// `pricing_approval_threshold`.
    #[must_use]
    pub const fn tombstone(version: u64, effective_from: OffsetDateTime) -> Self {
        Self {
            version,
            effective_from,
            entries: Vec::new(),
        }
    }

    /// Is this the version that says the tenant has **no** thresholds (D-185)?
    ///
    /// Derived rather than stored; see [`Self::tombstone`]. It is the exact
    /// complement of [`Self::policy`] answering `Some`, and a reader that needs the
    /// policy should ask for the policy — this is for the readers that need to say
    /// *which kind of version* is in force, which is a different question from *what
    /// does it threshold*: the surface renders an authored `none` differently from a
    /// tenant that never configured one, and only this tells them apart.
    #[must_use]
    pub fn is_tombstone(&self) -> bool {
        self.entries.is_empty()
    }

    /// Which version this is — the `version` half of the store's primary key.
    #[must_use]
    pub const fn version(&self) -> u64 {
        self.version
    }

    /// The instant the version starts applying.
    ///
    /// Enforced rather than merely authored: [`Self::is_effective_at`] is the
    /// instant's reader (D-188), so `infra::threshold::effective_version` cannot
    /// make the greatest approved version effective ahead of it.
    #[must_use]
    pub const fn effective_from(&self) -> OffsetDateTime {
        self.effective_from
    }

    /// Has this version's start arrived at `now`?
    ///
    /// The comparison D-188 added, and it lives here rather than in the walk that asks
    /// it because it is the meaning of the field: *not effective before* its instant,
    /// effective from it inclusive. The boundary is inclusive for the reason
    /// [`crate::domain::window::WindowInterval::has_started`]'s is — an interval that
    /// begins at `t` covers `t` — and because the exclusive reading would leave the
    /// authored instant itself governed by the *previous* policy, which is a state no
    /// operator asked for and no reviewer signed.
    ///
    /// Nothing here reads a clock. `now` is the caller's, so the same version answers
    /// the same way for the same instant however many times it is asked — which is
    /// what lets `infra::threshold`'s walk stay idempotent across two reads.
    #[must_use]
    pub fn is_effective_at(&self, now: OffsetDateTime) -> bool {
        now >= self.effective_from
    }

    /// The entries, in the order the pin frames them.
    #[must_use]
    pub fn entries(&self) -> &[ThresholdEntry] {
        &self.entries
    }

    /// Every field of this version at once, for a caller that must account for
    /// **all** of them — the content pin.
    ///
    /// [`ScopeKey::parts`](crate::domain::scope_key::ScopeKey::parts)' shape and
    /// for its reason. The fields are private because [`Self::new`] is what
    /// refuses an empty or duplicated entry set, so a consumer that has to frame
    /// the whole version can only reach it through accessors — and three separate
    /// accessor calls are three things a fourth field is silently absent from.
    /// The destructure below is exhaustive with no rest pattern, so a fourth
    /// field stops **this** compiling, and [`ThresholdVersionParts`] is
    /// destructured in turn at the pin, so the same field then stops the encoder
    /// compiling until somebody decides whether the pin covers it.
    ///
    /// Without it there is no such gate anywhere on this path:
    /// `put_threshold_version` called `version()`, `effective_from()` and
    /// `entries()`, and a fourth field would have compiled, pinned nothing, and
    /// left every test in the crate green.
    #[must_use]
    pub(crate) fn parts(&self) -> ThresholdVersionParts<'_> {
        let Self {
            version,
            effective_from,
            entries,
        } = self;
        ThresholdVersionParts {
            version: *version,
            effective_from: *effective_from,
            entries,
        }
    }

    /// This version read as a policy [`evaluate`] can compare against.
    ///
    /// `Option` because [`ThresholdPolicy::of_entries`] is the one authority on
    /// what counts as configured, and re-deciding it here would be a second answer.
    ///
    /// **The `None` arm is reached, and that is D-185.**
    /// [`Self::tombstone`] is a version with no entries, so `of_entries` answers `None`
    /// for it, so `infra::threshold::effective_policy` answers `None` for a tenant
    /// whose effective version is the tombstone — and `None` is what makes
    /// [`evaluate`] answer `NoConfiguredThreshold` and everything material again. The
    /// chain from an approved retirement back to the G1 fail-safe is that sentence and
    /// no other code.
    #[must_use]
    pub fn policy(&self) -> Option<ThresholdPolicy> {
        ThresholdPolicy::of_entries(self.entries.iter().cloned())
    }
}

/// The submitted change set, as materiality sees it: the rows it would publish,
/// **and** the act that submitted them.
///
/// It carries whole [`PriceRecord`]s rather than scope keys alone, because
/// `inst-mat-percurrency` compares a delta and [`delta::row_delta`] takes that
/// delta over a row's content, not its key — the amounts, the band geometry and
/// the contract fields are all operands of the comparison.
///
/// # The act is here, and it is what made `AlwaysMaterialTrigger` answerable
///
/// "This call is a window cancellation" has to be expressible on this type, or §3
/// step 4 cannot be evaluated where §3 puts it and has to be evaluated at each
/// surface that performs a registered act instead — and a second copy of the
/// trigger list per surface is how a rule acquires as many owners as it has
/// callers.
///
/// The act is **declared by the surface**, not inferred: whether a `PATCH` is a
/// shortening is decided inside the mutating transaction against the stored row
/// (D-176), and nothing reconstructable from a row set could tell it from an
/// extension.
///
/// # And the **subject** is here, because one rule is about the plan's revision and
/// # not about the rows
///
/// `inst-mat-registered`'s pure-shape clause (D-115) is a statement about a **plan
/// revision**: *"a pure-shape revision contains zero price rows, so the per-row
/// evaluation had nothing to trip on"*. Its detectable signature — a change set that
/// moves no row — is also the signature of every **window mutation**, which by
/// construction publishes the plan's rows exactly as they already stand. Without the
/// subject on the change set, `evaluate` answered `planShapeRevisionContent` for a
/// window schedule that revised no shape, which makes every window mutation material
/// whatever the threshold says and takes D-62's answer for the two acts it does not
/// govern.
///
/// The subject is [`PublishUnitKind`], the discriminator `PlanPublishUnit` already
/// carries, rather than a second enumeration of the same fact.
/// [`Option::None`](Option) is *"this act is no publish unit at all"* — the D-10
/// policy diff, whose unit is an approval unit over a threshold version and which
/// publishes nothing.
///
/// [`PublishUnitKind`]: crate::domain::publish::PublishUnitKind
/// [`PlanPublishUnit`]: crate::domain::publish::PlanPublishUnit
#[domain_model]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangeSet {
    unit: Option<PublishUnitKind>,
    act: Option<triggers::Trigger>,
    rows: Vec<PriceRecord>,
}

impl ChangeSet {
    /// A **plan-revision** publish that is not itself a registered trigger — §4.2's
    /// ordinary path.
    ///
    /// Its materiality genuinely is the threshold policy's question, and it is the
    /// one subject D-115's pure-shape clause is about.
    #[must_use]
    pub fn of_records(rows: impl IntoIterator<Item = PriceRecord>) -> Self {
        Self {
            unit: Some(PublishUnitKind::PlanContent),
            act: None,
            rows: rows.into_iter().collect(),
        }
    }

    /// A **window mutation** (D-99), with the registered trigger it is when it is
    /// one.
    ///
    /// `act` is `Some` for D-62's two — a cancel, and a `PATCH` that shortens — and
    /// `None` for a schedule and a lengthening, whose materiality is the threshold
    /// policy's question. The caller decides which, inside its mutating transaction
    /// and against the stored row (D-176); what it must not do is decide the
    /// *verdict*, which is why it hands the act over instead of minting one.
    ///
    /// `rows` is the row set the mutation's re-projection would freeze — the plan's
    /// **published** rows, which is why a window mutation's delta is zero on every
    /// one of them and its threshold question is really *"does this plan's every
    /// currency have an entry"*. That is `inst-mat-percurrency`'s fail-safe half
    /// reaching the window plane through its one owner rather than a second copy of
    /// it in `infra::window`.
    #[must_use]
    pub fn of_window_mutation(
        window_id: uuid::Uuid,
        act: Option<triggers::Trigger>,
        rows: impl IntoIterator<Item = PriceRecord>,
    ) -> Self {
        Self {
            unit: Some(PublishUnitKind::WindowMutation { window_id }),
            act,
            rows: rows.into_iter().collect(),
        }
    }

    /// A **grandfathering horizon tightening** — `inst-mat-registered`'s first
    /// clause, arriving as the act it is rather than as a diff to be recomputed.
    ///
    /// [`Self::of_window_mutation`]'s shape with the plan's own unit, and the two
    /// differences are both deliberate.
    ///
    /// **The unit is [`PublishUnitKind::PlanContent`] and not a kind of its own.**
    /// What the act moves is a published row's content, so the subject that
    /// re-projects is the plan — the same subject a publish freezes — and S5 §6
    /// declares no `grandfather` subject kind for this crate to mint (D-204 clause
    /// (2), the argument `infra::supersession::unit_request_id` already makes for
    /// its own handle).
    ///
    /// **The act is `Some` unconditionally**, where a window mutation's is `Some`
    /// on two of four. There is no direction for this door to be undecided about:
    /// a loosening is refused before the evaluator is reached
    /// ([`DomainError::GrandfatherLoosenForbidden`](crate::domain::error::DomainError::GrandfatherLoosenForbidden)),
    /// so every call that gets this far tightens. Declaring it here rather than
    /// leaving it to [`triggers::triggered_by_row`] buys the **reason**: the
    /// content half is reached only after the policy and baseline steps, so a
    /// tenant with no configured threshold would be told `noConfiguredThreshold`
    /// about an act no threshold governs — the precise substitution `evaluate`'s
    /// act-half-first ordering exists to prevent. The content half stays exactly
    /// where it is and keeps answering for the *other* producer of a moved
    /// horizon, a superseding successor row.
    #[must_use]
    pub fn of_horizon_tightening(rows: impl IntoIterator<Item = PriceRecord>) -> Self {
        Self {
            unit: Some(PublishUnitKind::PlanContent),
            act: Some(triggers::Trigger::GrandfatherHorizonTightening),
            rows: rows.into_iter().collect(),
        }
    }

    /// A change set from a registered act (§3 step 4) that is **no publish unit** —
    /// today exactly the D-10 threshold-policy diff.
    ///
    /// `rows` is still whatever row set the act would publish, because the act being
    /// material does not excuse the evaluator from seeing what it touches: the
    /// verdict is stored and read years later. For the policy diff that set is
    /// genuinely empty — a threshold prices nothing — and the unit's own
    /// `subject_kind` is what tells it apart in the store.
    #[must_use]
    pub fn of_act(act: triggers::Trigger, rows: impl IntoIterator<Item = PriceRecord>) -> Self {
        Self {
            unit: None,
            act: Some(act),
            rows: rows.into_iter().collect(),
        }
    }

    /// A **mass-repricing run**'s change set (`inst-mr-coalesce`): the rows the
    /// run selected, as currently published — no forced act, and no publish
    /// unit.
    ///
    /// `act` is `None` **and not [`Self::of_act`]'s `Some`**: nothing declares a
    /// repricing run a registered trigger by construction, so forcing one would
    /// make every run material whatever its rows and whatever the policy says,
    /// which is the opposite of `inst-mr-coalesce`'s *"any row over its
    /// own-currency threshold trips the run"* — a real per-currency comparison,
    /// not a foregone one.
    ///
    /// `unit` is `None` for [`Self::of_act`]'s reason applied to a run instead of
    /// a policy diff: [`PublishUnitKind`] names a **plan's** publish unit, and a
    /// run's rows may span several plans (`inst-mr-api`) with no `CatalogVersion`
    /// coalescing built yet to be the unit (O5, still unbuilt). Naming
    /// [`PublishUnitKind::PlanContent`] here — [`Self::of_records`]'s shape —
    /// would be the wrong fix reaching for the nearest constructor: this change
    /// set's rows are (until `inst-mr-apply` computes a real successor) the same
    /// content the baseline already carries, and
    /// [`triggers::triggered_by_content`]'s `moves_no_row` answers `true` for
    /// exactly that shape. Under `revises_plan_content() == true` step 4 would read
    /// that as D-115's pure-shape revision and answer `alwaysMaterialTrigger` for
    /// *every* run, silently — the same failure mode this constructor exists to
    /// keep out, one unit kind over.
    ///
    /// **Until `inst-mr-apply` computes the run's real successor content, this
    /// change set's rows are the published rows themselves and every row's delta
    /// is zero** — the same "zero-delta act" shape [`Self::of_window_mutation`]
    /// carries for a schedule or a lengthening. What decides materiality today is
    /// therefore `inst-mat-failsafe` ([`MaterialityReason::NoConfiguredThreshold`]):
    /// a currency the run touches with no configured entry is material; any
    /// currency the tenant has configured a threshold for is not, whatever the
    /// run's own adjustment is sized at. That is a real, honestly-scoped answer
    /// and not a placeholder — the row-level move a completed apply would compute
    /// is `inst-mr-apply`'s debt, not this constructor's.
    #[must_use]
    pub fn of_repricing_run(rows: impl IntoIterator<Item = PriceRecord>) -> Self {
        Self {
            unit: None,
            act: None,
            rows: rows.into_iter().collect(),
        }
    }

    /// The registered trigger this act is, if it is one.
    #[must_use]
    pub const fn act(&self) -> Option<triggers::Trigger> {
        self.act
    }

    /// Which publish unit's content this is, if it is a publish unit's at all.
    #[must_use]
    pub const fn unit(&self) -> Option<PublishUnitKind> {
        self.unit
    }

    /// Is this change set a **plan revision's** content — the one subject D-115's
    /// pure-shape clause ranges over?
    ///
    /// The predicate lives here rather than as a `matches!` at the call site so that
    /// the day a third unit kind appears, the compiler asks this question once.
    #[must_use]
    pub const fn revises_plan_content(&self) -> bool {
        matches!(self.unit, Some(PublishUnitKind::PlanContent))
    }

    /// The rows the change would publish.
    pub fn rows(&self) -> impl Iterator<Item = &PriceRecord> {
        self.rows.iter()
    }

    /// The keys those rows sit on — `inst-mat-newrow`'s whole question.
    pub fn keys(&self) -> impl Iterator<Item = &ScopeKey> {
        self.rows.iter().map(|row| &row.scope_key)
    }
}

/// The plan's currently published price rows, by scope key — the referent
/// `inst-mat-first` and `inst-mat-newrow` are each defined against.
///
/// `None` at the [`evaluate`] call site is a **first publish**; an instance
/// carrying no key for a row is that **row's** first publish. The two are
/// different rules and the design set states them separately, which is why the
/// absence is representable at both levels.
#[domain_model]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublishedPriceBaseline {
    rows: Vec<PriceRecord>,
    /// Each row's scope key to its position in `rows`.
    ///
    /// [`evaluate`] asks `covers` once per change-set key and `row` once per
    /// row, so a scan here is the whole product — and each comparison of two
    /// `ScopeKey`s walks four `String` axes. The baseline spans **every**
    /// published row of every plan a repricing selector touched, which is the
    /// one path a run cannot avoid and the one carrying the design's rows/s
    /// budget.
    ///
    /// Nothing iterates this map, so its order is unobservable and a
    /// `HashMap` costs nothing the `BTreeMap`s elsewhere in this crate are
    /// chosen for.
    by_key: HashMap<ScopeKey, usize>,
}

impl PublishedPriceBaseline {
    /// The baseline of a plan whose published rows are `rows`.
    ///
    /// Whole records rather than keys, for [`ChangeSet`]'s reason: a delta needs
    /// the amount it moved **from**, and a baseline that carried only keys could
    /// answer `inst-mat-newrow` and nothing else.
    #[must_use]
    pub fn of_records(rows: impl IntoIterator<Item = PriceRecord>) -> Self {
        let rows: Vec<PriceRecord> = rows.into_iter().collect();
        let mut by_key = HashMap::with_capacity(rows.len());
        for (position, row) in rows.iter().enumerate() {
            // First-wins, which is what a linear scan answers. Two
            // rows on one key cannot arrive: `plan_id` is an axis of the key and
            // the published plane carries a UNIQUE index over the whole of it,
            // so a baseline accumulated across several plans still holds one row
            // per key. The `or_insert` decides nothing today and is here so that
            // the answer never comes to depend on insertion order.
            by_key.entry(row.scope_key.clone()).or_insert(position);
        }
        Self { rows, by_key }
    }

    /// Is there a published row on `key`?
    #[must_use]
    pub fn covers(&self, key: &ScopeKey) -> bool {
        self.by_key.contains_key(key)
    }

    /// The published row on `key`, when there is one.
    ///
    /// `None` is `inst-mat-newrow`'s world and the delta domain's absent operand
    /// at once, which is why the two rules read the same accessor: a row the
    /// baseline does not carry has no predecessor to delta against **because** it
    /// has no predecessor at all.
    #[must_use]
    pub fn row(&self, key: &ScopeKey) -> Option<&PriceRecord> {
        self.by_key
            .get(key)
            .and_then(|position| self.rows.get(*position))
    }
}

/// Evaluate a change set's materiality — §3's steps 1, 2, 3, 3a and 4.
///
/// `policy` is the tenant's approval-threshold policy and `baseline` the plan's
/// published price rows; `None` on either is the absence its rule is named for.
/// Materiality is evaluated **once, at submit** (§3 step 3): a later policy
/// change neither re-evaluates nor voids a pending approval, so the arguments
/// are the world as it stood when the unit was opened, never as it stands when
/// the unit is decided.
#[must_use]
pub fn evaluate(
    change: &ChangeSet,
    policy: Option<&ThresholdPolicy>,
    baseline: Option<&PublishedPriceBaseline>,
) -> MaterialityVerdict {
    // §3 step 4's **act** half, ahead of everything, and the order is a decision
    // rather than §3's numbering. A registered act is material "whatever a
    // threshold says and whatever delta it carries", so no answer below could
    // change it — while every answer below would *replace* its reason with one the
    // operator cannot act on. Told `noConfiguredThreshold` about a window
    // cancellation, an operator would configure a threshold and watch the
    // cancellation stay material; `alwaysMaterialTrigger` is the fact. §3 numbers
    // its rules; it does not fix their precedence, and the three below already
    // carry a precedence this module chose on the same ground.
    if let Some(act) = triggers::triggered(change) {
        return MaterialityVerdict::triggered(act);
    }
    // Step 1, `inst-mat-failsafe`. First of the three because it is the only one
    // that holds with no knowledge of the subject at all.
    let Some(policy) = policy else {
        return MaterialityVerdict::material(MaterialityReason::NoConfiguredThreshold);
    };
    // Step 2, `inst-mat-first`.
    let Some(baseline) = baseline else {
        return MaterialityVerdict::material(MaterialityReason::FirstPublish);
    };
    // Step 3a, `inst-mat-newrow`. One row without a predecessor trips the whole
    // change, as one row over its threshold does (§3's any-row-trips reading, G3).
    if change.keys().any(|key| !baseline.covers(key)) {
        return MaterialityVerdict::material(MaterialityReason::RowWithoutBaseline);
    }
    // Step 4's whole-change-set half — D-115's pure-shape revision. Before the row
    // walk, because a revision that moves no row has no row for the walk to trip
    // on: that is exactly the hole D-115 exists to close.
    //
    // **Only for a plan-revision publish**, and the gate is the rule's own scope
    // rather than a carve-out: D-115 is about a *revision* whose content is the
    // plan's shape. A window mutation moves an interval and no row, so it has the
    // same detectable signature and none of the meaning — and answering
    // `planShapeRevisionContent` for one would make every schedule and every
    // lengthening material whatever a threshold said, which is D-62's answer taken
    // for the two acts D-62 deliberately does not govern.
    if change.revises_plan_content()
        && let Some(content) = triggers::triggered_by_content(change, baseline)
    {
        return MaterialityVerdict::triggered(content);
    }
    // Step 4's per-row half — the horizon and D-115's incomputable deltas — walked
    // over the **whole** change set before the threshold walk, the way the act half
    // above already runs ahead of the policy and baseline steps.
    //
    // **A registered trigger is material at every threshold**, so it must not lose to
    // a reason that tells an operator to configure one. Inside the threshold loop it
    // loses twice: to the fail-safe on its own row, and — because that loop returns
    // on the first row it can answer for — to a *different* row's fail-safe reached
    // first. Both verdicts are material either way; what differs is the reason, which
    // is the whole of what an operator acts on, and "configure a bar for this
    // currency" is a remedy they can carry out and that changes nothing.
    //
    // A row with no predecessor is skipped rather than reported here: the trigger
    // reads the predecessor, and `RowWithoutBaseline` is the threshold walk's to
    // answer once no trigger has claimed the change set.
    for row in change.rows() {
        if let Some(published) = baseline.row(&row.scope_key)
            && let Some(moved) = triggers::triggered_by_row(row, published)
        {
            return MaterialityVerdict::triggered(moved);
        }
    }
    // Step 3, `inst-mat-percurrency`: each row's delta, in its **own** currency
    // (S5's constraint G3), against that currency's own entry.
    for row in change.rows() {
        // Step 3a guaranteed a predecessor for every key above; this is that row.
        let Some(published) = baseline.row(&row.scope_key) else {
            return MaterialityVerdict::material(MaterialityReason::RowWithoutBaseline);
        };
        let Some(entry) = policy.entry(row.scope_key.currency()) else {
            // `inst-mat-percurrency`'s fail-safe half. The reason is
            // `inst-mat-failsafe`'s and not a fourth token, because the design set
            // says so in as many words: "the G1 fail-safe applies **per currency,
            // not per policy object**". What the operator can act on is identical —
            // configure an entry for this currency.
            return MaterialityVerdict::material(MaterialityReason::NoConfiguredThreshold);
        };
        match compare(&delta::row_delta(row, published), entry.basis) {
            Comparison::Below => {}
            // `inst-mat-percurrency` proper, and its **own** reason: this currency
            // has an entry and the row reached it. Reporting the fail-safe token
            // here would tell an operator to configure a threshold that is already
            // configured, and would make the stored column unable to tell that
            // world from "this currency has no entry" — which is the one
            // distinction this rule is built around.
            Comparison::Reached(moved) => {
                return MaterialityVerdict::tripped_row(TrippedRow {
                    price_id: row.price_id,
                    currency: row.scope_key.currency().clone(),
                    from_minor: moved.from_minor,
                    to_minor: moved.to_minor,
                    // The whole move, not two thirds of it. `moved` is the
                    // operand [`compare`] measured against the bar and its
                    // scale is what made that comparison meaningful (D-311);
                    // dropping it here left the two amounts above labelled as
                    // minor units by `api::rest::approvals::TrippedRowView`,
                    // and a `per_unit` rate reaches this arm.
                    scale: moved.scale,
                });
            }
            // The bar could not be evaluated: a percent entry against a zero
            // baseline. §3 puts that under the G1 fail-safe in the same breath as
            // the no-entry case — *"no percentage is computable"* — so it carries
            // the fail-safe's token and not the tripped one.
            Comparison::NotComparable => {
                return MaterialityVerdict::material(MaterialityReason::NoConfiguredThreshold);
            }
        }
    }
    MaterialityVerdict::AutoPublishable
}

/// What comparing one row's delta against its currency's entry produced.
///
/// Three states rather than a bool, because the comparison has three outcomes and
/// two of them are material for **different** reasons an operator acts on
/// differently. Folding [`Self::NotComparable`] into [`Self::Reached`] — which the
/// predicate this replaced did, with `unwrap_or(true)` — stores "this row moved past
/// its bar" about a row whose bar could not be evaluated at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Comparison {
    /// Every operand is under the bar. The only outcome that does not end the walk.
    Below,
    /// At least one operand **reaches** the bar (`>=`, per
    /// [`delta::AmountMove::reaches_absolute`]'s stated boundary), carrying **the
    /// operand that did**.
    ///
    /// Carried rather than recomputed at the call site, because for a band vector the
    /// tripping move is one band of several and the caller has no way to tell which
    /// (D-187): a caller re-deriving it would have to re-run the comparison and pick,
    /// which is this function's own decision spelled a second time.
    Reached(delta::AmountMove),
    /// The bar cannot be evaluated against this delta at all.
    NotComparable,
}

/// Compare `delta` against `basis`.
///
/// **Any operand trips it**, because §3's G3 is an any-row-trips rule and a band
/// vector is that rule one level down: a `graduated` row whose top band doubled has
/// moved, whatever the bands beneath it did, and an aggregate over the vector would
/// let a large move in one band hide behind small ones in the others.
///
/// [`Comparison::Reached`] **dominates** [`Comparison::NotComparable`] across a band
/// vector, and the precedence is the same one [`evaluate`] applies to its own rules:
/// a band that is provably over its bar is the sharper and more actionable fact than
/// a sibling band whose percentage has no denominator, and both answers are material
/// either way. Reported in the direction that names something the operator can look
/// at.
///
/// A percent basis with a zero baseline is [`Comparison::NotComparable`], which is §3
/// step 3 verbatim — *"A percent-only policy against a zero (or NULL) baseline is
/// likewise material — no percentage is computable"* — and its verdict at the call
/// site is the **G1 fail-safe**, on the same warrant the per-currency no-entry case
/// takes it: no delta computable ⇒ material.
///
/// The `NotComputable` arm of [`delta::RowDelta`] answers `NotComparable` and is
/// **not** the arm that reports it: [`triggers::triggered_by_row`] has already
/// answered `AlwaysMaterialTrigger` for exactly this row, one line above the only
/// call site, so this arm has **no removal proof and that is reported rather than
/// hidden**. It is written out rather than left to a wildcard so that reordering
/// those two lines fails safe instead of auto-publishing a change with no computable
/// delta.
fn compare(delta: &delta::RowDelta, basis: ThresholdBasis) -> Comparison {
    let moves: &[delta::AmountMove] = match delta {
        delta::RowDelta::Amount(one) | delta::RowDelta::PackagePrice(one) => {
            std::slice::from_ref(one)
        }
        delta::RowDelta::BandVector(many) => many,
        delta::RowDelta::NotComputable(_) => return Comparison::NotComparable,
    };
    let mut answer = Comparison::Below;
    for moved in moves {
        match basis {
            ThresholdBasis::Absolute { minor } => {
                // The scale conversion is `reaches_absolute`'s (D-311), not this
                // loop's: the bar arrives in minor units and the move may be in
                // either scale, and putting the arithmetic beside the `>=` keeps
                // the two from drifting apart.
                if moved.reaches_absolute(minor) {
                    return Comparison::Reached(*moved);
                }
            }
            ThresholdBasis::Percent { bp } => match moved.reaches_percent(i64::from(bp)) {
                Some(true) => return Comparison::Reached(*moved),
                Some(false) => {}
                None => answer = Comparison::NotComparable,
            },
        }
    }
    answer
}

#[cfg(test)]
#[path = "materiality_tests.rs"]
mod materiality_tests;
