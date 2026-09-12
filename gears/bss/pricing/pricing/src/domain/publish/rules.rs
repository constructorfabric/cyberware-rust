//! The **aggregate** publish rule set: one report over one publish subject.
//!
//! Until this module the rules existed as disconnected pipelines — Slice-3 rules
//! over a [`PriceRow`], Slice-2 rules over a [`PlanShape`], one supersession
//! guard over a pair — each registered and none of them run by anything but a
//! test and a dev-only conformance validator. (The counts this sentence carried,
//! "thirteen" and "twenty", were of pipelines that still exist under those names
//! and had grown to 18 and 30, with nothing in the sentence telling a reader they
//! were frozen at a date — review T-6.)
//! [`run_publish_rules`] is what §4.2 step 2 and step 4 actually call, and it is
//! the same call in both: approval approves *content*, the commit re-validates
//! *state*, and the only way those two runs can be compared is if they are the
//! same function over the same subject.
//!
//! # Order is a reading order, never a short-circuit
//!
//! [`ValidationPipeline::run`] runs every rule and the aggregate report is the
//! product — an author remediates a plan in one pass. What the order decides is
//! how the report **reads**, and it is fixed here: the per-row rules first, then
//! the plan-shape set. That is the conformance validator's own argument
//! (`examples/regen_registry/validator.rs`): a malformed row is malformed
//! regardless of the plan it sits in, while a plan-level rule is a statement
//! *about a set of rows* and reads oddly ahead of the news that one of them is
//! not a row at all.
//!
//! The **slice-7 coverage set runs last**, after both plan-level sets, for the
//! same reason: `inst-wc-required` is a statement about a key's window plane, and
//! a report that named an uncovered key before it named the row that is not a row
//! reads back to front. [`window_coverage_rules`] is registered here as its own
//! pipeline — not appended to `foundation_plan_rules`, whose doc reserves that
//! place for the next *Foundation* rule, and not to Slice 2's set, which
//! `GrandfatherHorizonOnItsClass` already argues a slice-7 rule cannot join.
//!
//! Within the row half the Foundation's own rule runs after Slice 3's, because
//! Slice 3 answers "is this a row" and the rounding rule answers "does this row
//! resolve the policy every charge's last minor unit depends on" — the second
//! question presupposes the first.
//!
//! # `supersession_rules` is not run here, and that is not an oversight
//!
//! It takes a `SupersessionPair`, and **this** publish unit produces no successor
//! on an occupied key: `PriceRepo::create_draft` refuses a scope key already held
//! by a `draft` **or** `published` row, so nothing the plan-revision publish
//! authors can set `supersedes_price_id`.
//!
//! **One clause of that argument has expired and the conclusion has not.** It used
//! to close "the two sanctioned producers of that flip are Slice 7's and neither
//! exists", and D-88's **row half** exists:
//! `price_repo::insert_successor_draft_on` authors a successor draft on an occupied
//! key through a door of its own and stamps `supersedes_price_id` from the
//! predecessor it found (D-195), and `commit_supersession_rows` flips the
//! predecessor. **The compose exists too** — `domain::supersession::plan_supersession`,
//! which runs this very guard.
//!
//! **And so do the orchestrator, the approval unit and the route**, which this
//! paragraph is the easiest place to deny: `POST /plans/{planId}/supersessions`
//! (`api::rest::supersessions`), `infra::supersession`'s `supersede` /
//! `supersede_in` / `commit_supersession`, and
//! `ApprovalService::submit_supersession_on` — driven end to end by
//! `tests/rest_supersessions.rs`. Nothing is owed. Saying otherwise matters here
//! more than elsewhere, because this paragraph is the stated justification for
//! `run_publish_rules` **not** registering `supersession_rules`: a reader auditing
//! that omission is handed the premise.
//!
//! The conclusion survives on the premise that always did the work: the pair this
//! guard judges cannot arrive on **this** path, because `create_draft` refuses an
//! occupied key and `infra::publish::validated_draft_rows` excludes a draft on an
//! occupied key from the unit's set (D-195) — and the supersession unit, which
//! does produce such a pair, is its own publish unit and runs the guard itself in
//! `plan_supersession` (`inst-su-compose`). Running it over a fabricated pair here
//! would be a rule reporting on a subject nobody authored.
//!
//! # The Foundation's base set: what is enforced structurally, and what was
//! missing
//!
//! [`crate::domain::validation`] says the Foundation "registers the money and
//! rounding rules itself rather than leaving the base set to whichever slice
//! happens to load first", and §3.3 says `ROUNDING_POLICY_UNRESOLVED` is
//! "registered into the pipeline by the Foundation itself". Of those:
//!
//! - **The money rules need no pipeline rule, because they are unrepresentable
//!   failures.** `MinorAmount::new` refuses a negative (`AMOUNT_NEGATIVE`) and
//!   `CurrencyCode::new` refuses a non-ISO-4217 code (`CURRENCY_INVALID`). A
//!   [`PriceRow`] cannot hold a violation of either, so a rule here would be a
//!   rule with nothing to reject — and a rule that always passes is
//!   indistinguishable from a rule that holds. The absence is a decision,
//!   recorded so it reads as one.
//!
//!   **`PRECISION_EXCEEDED` is in that paragraph for a different reason, and this
//!   sentence used to blur the two**. It said
//!   `check_scale` / `check_decimal` "carry" the code, which reads as *these are
//!   where it is enforced*; they carry it in the sense that they are the functions
//!   that would raise it, and **nothing calls either of them**. The reason is the
//!   same representational one — an amount arrives as an `i64` of minor units, so
//!   no request declares a scale — but the conclusion differs: `AMOUNT_NEGATIVE`
//!   and `CURRENCY_INVALID` are enforced elsewhere and need no rule here, while
//!   this refusal is enforced *nowhere* and is waiting for the first authoring
//!   surface that takes a decimal literal. [`crate::domain::money`]'s doc names
//!   what that surface owes; a reader checking the FR against this bullet should
//!   not be told the rule is already held.
//! - **The rounding rule was genuinely missing.** `DomainError::RoundingPolicyUnresolved`
//!   existed and was mapped, `pricing_price.rounding_policy_ref` existed,
//!   `pricing_policy_object.default_rounding_policy_ref` existed — and **no rule
//!   read any of them**, so a row that resolved neither would have published.
//!   [`RoundingPolicyResolved`] is that rule.
//!
//! # The §14 soft caps are an advisory, and the code is now the set's
//!
//! §1.2's NFR allocation names "publish-time size validation (100 bands/row, 500
//! rows/plan soft)" and PRD §14 says the system **SHOULD** enforce them "emitting
//! a publish **warning** above the cap". An earlier version of this doc reported
//! the whole thing as a gap for one reason and one only: no document named an
//! advisory code. D-160 named it — `PLAN_SIZE_SOFT_CAP_EXCEEDED`, in
//! `01-foundation.md` §3.3 — so [`PlanSizeWithinSoftCaps`] is the rule.
//!
//! **It never blocks.** The finding rides `warnings[]`, the channel
//! `TIER_BAND_PRICE_INCREASE` already uses, and `is_publishable()` does not read
//! it. Making the caps blocking so they could reuse a violation code is D-160's
//! rejected option (b): a ratified `SHOULD` enforced as a `MUST` is a plan an
//! operator could author yesterday and cannot publish today.
//!
//! **The two *interval* caps are a different thing and stay hard.** §1.2's
//! allocation row had blurred all four under one heading; D-160 unblurs them.
//! `customEveryN` bounds fail publish under
//! [`INVALID_CUSTOM_INTERVAL`](crate::domain::plan_rules::INVALID_CUSTOM_INTERVAL)
//! and this rule must never reach them — a soft advisory over a hard cap would
//! tell an author their over-cap interval published.
//!
//! # What is deliberately absent
//!
//! **The fixture gate is not a rule and is not run here.** `FixtureGate::check`
//! returns `DomainError::FixtureMissing` and stays a separate fail-closed step
//! beside the report; [`crate::infra::publish`] is where the two meet. Two
//! reasons. It is not a statement about the plan but about what this
//! deployment's joint corpus has agreed to evaluate, so its remedy is not an
//! edit the author can make — which is the whole difference between a violation
//! and a refusal. And synthesizing a violation from it needs a code, which this
//! group will not mint on its own authority.

use std::collections::BTreeSet;

use toolkit_macros::domain_model;
use uuid::Uuid;

use crate::domain::bundle_rules::BUNDLE_TAX_BASIS_MIXED;
use crate::domain::contracts::{ChangeTargetIndex, consumer_contract_rules};
use crate::domain::coverage::window_coverage_rules;
use crate::domain::currency_binding::{AddonCoverage, RequiredAddonsCoverMarkets};
use crate::domain::money::CurrencyCode;
use crate::domain::plan_rules::{CustomIntervalBounds, DescriptorSetComplete, plan_shape_rules};
use crate::domain::plan_shape::PlanShape;
use crate::domain::price_row::PriceRow;
use crate::domain::rules::price_row_rules;
use crate::domain::scope_key::{PriceEligibility, Region};
use crate::domain::tax_display::{
    MarketBasisUniform, RegionTaxReadiness, TaxBasisComplete, TaxDisplayPolicy,
};
use crate::domain::taxonomy::{GlCodeDeclared, RegionsDeclared, RoundingPolicyDeclared};
use crate::domain::validation::{ValidationPipeline, ValidationReport, ValidationRule};

/// A published row resolves neither its own `rounding_policy_ref` nor a tenant
/// default (`01-foundation.md` §3.3, PRD §17.4).
///
/// The code is §3.3's verbatim. **It has two homes in the design set and that is
/// reported rather than resolved here**: `DomainError::RoundingPolicyUnresolved`
/// is a mapped rejection of its own, and this is a `Violation` inside the
/// `ValidationFailed` envelope. The pipeline rendering is the right one — §17.4
/// calls it a validation rule and the report must enumerate every finding in one
/// pass, which a single-refusal error cannot do — but the variant is mapped and
/// may have a non-pipeline caller, so it is not deleted.
pub const ROUNDING_POLICY_UNRESOLVED: &str = "ROUNDING_POLICY_UNRESOLVED";

/// A plan or a row above one of the two **soft** size caps (`nfr-size-limits`,
/// D-160; declared in `01-foundation.md` §3.3).
///
/// **Advisory, never blocking**, and that is ratified rather than chosen: PRD §14
/// makes the caps a `SHOULD` that emits a publish warning. It rides `warnings[]`
/// and `is_publishable()` never reads it.
///
/// One code for both caps, by D-146's line: the operator's next action is the
/// same shape — shorten the ladder, or split the plan — and the report says which
/// cap and by how much. The two **interval** caps are hard and keep
/// [`INVALID_CUSTOM_INTERVAL`](crate::domain::plan_rules::INVALID_CUSTOM_INTERVAL);
/// §1.2's allocation row had blurred all four under one heading and D-160 unblurs
/// them.
pub const PLAN_SIZE_SOFT_CAP_EXCEEDED: &str = "PLAN_SIZE_SOFT_CAP_EXCEEDED";

/// A grandfathering horizon on a row whose eligibility class may not carry one
/// (S7 §5, D-147).
///
/// **The wire code already existed and the publish-side rule did not.** The
/// refusal is enforced on both authoring paths by
/// [`price_repo`](crate::infra::storage::repo::price_repo), which pre-empts the
/// column `CHECK`'s 500 and reaches the caller as this code through
/// `DomainError::GrandfatherUntilForbidden`. What was missing is the **registered
/// rule**: without it, a publish carrying such a row is refused only because
/// whichever authoring write happened to reach the store first refused, and the
/// 422 report never enumerates it beside the plan's other violations.
///
/// The two are **not** redundant. The repository guard is the guarantee — a
/// predicate on a statement, which is the only thing that holds against a caller
/// reaching the store by another path — and this rule is the report line an author
/// acts on. Deleting either leaves a hole the other does not cover.
pub const GRANDFATHER_UNTIL_FORBIDDEN: &str = "GRANDFATHER_UNTIL_FORBIDDEN";

/// A row carries a value in a field that is **declared and not yet authorable**
/// (`01-foundation.md` §3.3, D-179; the hazard is D-177's).
///
/// Today that is `tierQualificationWindow` (D-40) and the **`carry` half** of
/// `includedAllowance` (D-45): Slice 10's allowance gate and its band compile
/// have landed, so a `{N, none}` declaration is judged and honoured, while
/// `inst-ac-carry`'s promotional grant has no `pricing_plan_grant` table to be
/// written into and stays here. The code names the **reason**, so a half nobody
/// can honour yet needs no code of its own.
///
/// Both are refused at authoring by D-177 clause (1) as malformed requests
/// carrying **no code** — the class D-141 settled — and this is deliberately a
/// *different* refusal rather than the same one moved:
///
/// - The acts differ. A stored row's publish request is well-formed and its
///   preconditions hold; the operator's remedy is to clear the field, not to
///   reshape a payload.
/// - The authoring refusal *can* be codeless because the validation envelope has
///   a no-code arm. This path has none — a publish refusal travels as a
///   [`Violation`](crate::domain::validation::Violation) in the report, which
///   carries a code by construction, and an empty report short-circuits to an
///   internal fault. A codeless refusal here would reach the operator as a 500.
/// - D-177 clause (3) makes the authoring refusal load-bearing and leaves the
///   freezing act itself ungated, which is safe only while **every** authoring
///   path refuses — a condition the design set cannot assert while the
///   bulk-import arm is unbuilt and rows authored before the refusal may exist.
///
/// It names the **reason** rather than the field, so a third
/// rostered-but-unauthorable field needs no second code and the operator's action
/// does not change — D-146's line.
///
/// **Its removal has an order** (D-177 clause 3, restated by D-179 clause 3): a
/// field leaves this rule in the same change that lands the rules *and* the
/// artifact that gives its value meaning — never before, and never by a group
/// tidying what looks like an unreachable branch. That is how the allowance's
/// `none` half left: `inst-ac-gate`'s six refusals and
/// [`crate::domain::allowance::compile`] landed together, and the field's arm
/// narrowed to `carry` in the same change rather than being deleted whole. The
/// window and the `carry` half wait on the same terms.
pub const PRIMITIVE_RULES_UNBUILT: &str = "PRIMITIVE_RULES_UNBUILT";

/// The per-tenant configuration the publish rule set is run under.
///
/// Held as **fields** rather than looked up inside a rule, for the reason
/// [`crate::domain::validation`] states: the set runs twice and a rule that
/// reached for configuration itself could answer differently in the two runs for
/// no authored reason. It is also what keeps a storage read out of the domain.
///
/// There is deliberately no `Default`. [`CustomIntervalBounds`] has none either,
/// and for the sharper version of the same reason — zero caps reject every
/// custom frequency ever authored while looking exactly like a rule that is
/// switched on.
#[domain_model]
#[derive(Clone, Debug)]
pub struct PublishRuleParams {
    interval_bounds: CustomIntervalBounds,
    descriptors: DescriptorSetComplete,
    default_rounding_policy: Option<String>,
    size_caps: SoftSizeCaps,
    referencing_markets: Vec<ReferencingMarket>,
    declared_regions: BTreeSet<Region>,
    declared_rounding_policies: BTreeSet<String>,
    /// The tenant's declared GL codes (D-356). Empty means **unconstrained**, as
    /// the rounding vocabulary's does and for its reason — see
    /// [`Self::with_declared_gl_codes`].
    declared_gl_codes: BTreeSet<String>,
    addon_coverage: AddonCoverage,
    tax_display_policy: TaxDisplayPolicy,
    region_readiness: RegionTaxReadiness,
    change_targets: ChangeTargetIndex,
    /// **Will the caller open coverage for a key this run is freezing?** (D-332)
    ///
    /// True from the publish, which writes the initial window itself, and false
    /// everywhere else the same rule set runs — the repricing apply's aggregate
    /// pass above all. That distinction cannot be read off the shape: a draft
    /// row's key is identical in both, and only the caller knows whether a
    /// window is coming. Held as a field for this struct's own stated reason —
    /// a rule that decided it for itself would answer differently in the two
    /// runs for no authored reason.
    ///
    /// Defaults to **false**, which is the fail-closed direction: a caller that
    /// forgets to say so gets the refusal rather than a silently skipped rule.
    opens_initial_coverage: bool,
}

/// One market of one **bundle that references this plan as a component**, and the
/// tax display basis its *other* members have already established there
/// (`inst-bc-taxbasis`'s reverse half, D-119, homed here by D-212).
///
/// **A resolved fact, handed in, exactly as the tenant's rounding default is.**
/// The rule that reads it is a property of another aggregate entirely, so the
/// walk cannot go and look: it would need the set of bundles referencing this
/// plan, which is a storage read, and this set runs twice on one publish with the
/// same answer required both times. `infra::bundle::referencing_markets` is what
/// resolves it, off `idx_pricing_bundle_component_plan`.
///
/// **The basis is the one the bundle's *other* members carry**, never this plan's
/// own rows — those are the thing being judged. Where the others already disagree
/// among themselves, the first is taken and this guard says nothing more: that
/// bundle's own publish fails the forward half, and reporting it twice would send
/// an operator to fix a market whose fault is not theirs.
#[domain_model]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReferencingMarket {
    bundle_id: Uuid,
    currency: CurrencyCode,
    region: Region,
    tax_inclusive: bool,
}

impl ReferencingMarket {
    /// Name one referencing bundle's established basis in one market.
    #[must_use]
    pub const fn new(
        bundle_id: Uuid,
        currency: CurrencyCode,
        region: Region,
        tax_inclusive: bool,
    ) -> Self {
        Self {
            bundle_id,
            currency,
            region,
            tax_inclusive,
        }
    }
}

impl PublishRuleParams {
    /// Bind the rule set to one tenant's configuration.
    #[must_use]
    pub const fn new(
        interval_bounds: CustomIntervalBounds,
        descriptors: DescriptorSetComplete,
        default_rounding_policy: Option<String>,
        size_caps: SoftSizeCaps,
    ) -> Self {
        Self {
            interval_bounds,
            descriptors,
            default_rounding_policy,
            size_caps,
            referencing_markets: Vec::new(),
            declared_regions: BTreeSet::new(),
            declared_rounding_policies: BTreeSet::new(),
            declared_gl_codes: BTreeSet::new(),
            // Empty is fail-closed here too: an add-on nobody resolved covers
            // nothing, so a required one blocks. See `AddonCoverage`.
            addon_coverage: AddonCoverage::empty(),
            // C4's ratified default, so a caller that says nothing gets the
            // fail-closed rule rather than the permissive one.
            tax_display_policy: TaxDisplayPolicy::FailClosed,
            region_readiness: RegionTaxReadiness::empty(),
            // Empty is fail-closed here too, and the direction is the safe one:
            // an index nobody resolved knows of no published plan, so every
            // authored edge reads as dangling and the publish is refused. A
            // caller who forgets is loud immediately.
            change_targets: ChangeTargetIndex::empty(),
            // False by construction: only the publish may say otherwise, and a
            // caller that forgets gets the refusal rather than a skipped rule.
            opens_initial_coverage: false,
        }
    }

    /// Attach the bundle markets this plan's rows are judged against (D-212).
    ///
    /// A **second call** rather than a fifth parameter on [`Self::new`], for
    /// `ScopeKey::with_usage_line`'s reason: almost no plan is a component of any
    /// bundle, so every caller would be passing an empty vector to satisfy a
    /// signature it has nothing to say to. The empty set is what [`Self::new`]
    /// already produces, so this is never needed to express "no referencing
    /// bundle".
    #[must_use]
    pub fn with_referencing_markets(mut self, markets: Vec<ReferencingMarket>) -> Self {
        self.referencing_markets = markets;
        self
    }

    /// Attach the tenant's **active** region universe (`inst-tx-region`).
    ///
    /// A second call rather than a sixth parameter on [`Self::new`], for
    /// [`Self::with_referencing_markets`]' reason — but the empty set means
    /// something different here and it is the **fail-closed** meaning, not "no
    /// constraint": a tenant who has declared no region publishes no row at all
    /// (C2). That asymmetry is why this is documented rather than mirrored.
    ///
    /// It follows that every caller assembling a real publish **must** call this.
    /// A caller who forgets gets the empty set and refuses everything, which is
    /// the safe direction to fail and is loud immediately — the opposite default
    /// would let the rule pass silently for a tenant whose taxonomy nobody read.
    #[must_use]
    pub fn with_declared_regions(mut self, regions: BTreeSet<Region>) -> Self {
        self.declared_regions = regions;
        self
    }

    /// Attach the tenant's declared rounding vocabulary (D-334).
    ///
    /// Its empty set means **unconstrained**, which is neither
    /// [`Self::with_declared_regions`]' reading nor
    /// [`Self::with_change_targets`]': a tenant that has declared no rounding
    /// values has not opted into the check, and refusing there would have broken
    /// every catalog the day the taxonomy landed. The three defaults are
    /// deliberately different and each says so where it is set.
    #[must_use]
    pub fn with_declared_rounding_policies(mut self, values: BTreeSet<String>) -> Self {
        self.declared_rounding_policies = values;
        self
    }

    /// Attach the tenant's declared GL-code vocabulary (D-356).
    ///
    /// [`Self::with_declared_rounding_policies`]' reading of the empty set —
    /// **unconstrained** — and its reason: a tenant that has declared no GL
    /// codes has not opted into the check, and refusing there would have failed
    /// every existing plan the day the vocabulary landed. The caller resolves the
    /// set from `taxonomy_repo::active_gl_codes`; the rule never learns whether a
    /// tenant or a future ERP gear wrote it, which is the provider seam D-356
    /// names.
    #[must_use]
    pub fn with_declared_gl_codes(mut self, values: BTreeSet<String>) -> Self {
        self.declared_gl_codes = values;
        self
    }

    /// Declare that this run opens coverage for the keys it is freezing (D-332).
    ///
    /// The publish says so; nothing else may. `sqlite_repricing_apply`'s
    /// aggregate case is what makes the distinction real rather than
    /// theoretical: it runs this set over a plan carrying a stray draft, and a
    /// draft whose key the apply will never cover has to keep failing.
    #[must_use]
    pub const fn opening_initial_coverage(mut self) -> Self {
        self.opens_initial_coverage = true;
        self
    }

    /// Does the caller open coverage itself? See the field.
    #[must_use]
    pub const fn opens_initial_coverage(&self) -> bool {
        self.opens_initial_coverage
    }

    /// Attach what the store found about the plans this one's change contract
    /// touches (`inst-pc-targets`, `inst-pc-mutual`, D-54).
    ///
    /// A second call for [`Self::with_referencing_markets`]' reason — almost no
    /// plan authors a change edge — but the empty index means what
    /// [`Self::with_declared_regions`]' empty set means and not what that one's
    /// does: **fail-closed**. An index nobody resolved knows of no published
    /// plan, so every authored edge reads as dangling. That is the safe
    /// direction and it is loud immediately; the opposite default would let a
    /// dangling edge publish for a caller who forgot to look.
    #[must_use]
    pub fn with_change_targets(mut self, index: ChangeTargetIndex) -> Self {
        self.change_targets = index;
        self
    }

    /// Attach C4's tax-display inputs: the tenant's enforcement mode and the
    /// `RegionTaxReadiness` lookup (`inst-td-policy`, `inst-td-readiness`).
    ///
    /// The two travel together because neither is usable alone — the mode says
    /// how to treat an incomplete basis and the readiness is what decides
    /// whether one *is* incomplete — and a caller holding one without the other
    /// would be a caller free to enforce a policy against an empty world, which
    /// answers "incomplete" for every row.
    #[must_use]
    pub fn with_tax_display(
        mut self,
        policy: TaxDisplayPolicy,
        readiness: RegionTaxReadiness,
    ) -> Self {
        self.tax_display_policy = policy;
        self.region_readiness = readiness;
        self
    }

    /// The readiness lookup, for the publish path's own projection of D-154's
    /// resolved category.
    #[must_use]
    pub const fn region_readiness(&self) -> &RegionTaxReadiness {
        &self.region_readiness
    }

    /// The resolved add-on coverage, for a reader that is not a rule.
    ///
    /// A reader beside [`Self::region_readiness`]: the publish path counts
    /// `inst-cb-addon`'s blocks (`T-17`) and has to ask the same question the
    /// rule asks, from the same handed-in facts, rather than re-deriving it.
    #[must_use]
    pub const fn addon_coverage(&self) -> &AddonCoverage {
        &self.addon_coverage
    }

    /// Attach the resolved coverage of this plan's add-ons (`inst-cb-addon`).
    #[must_use]
    pub fn with_addon_coverage(mut self, coverage: AddonCoverage) -> Self {
        self.addon_coverage = coverage;
        self
    }

    /// The tenant's default rounding policy, when it has one.
    #[must_use]
    pub fn default_rounding_policy(&self) -> Option<&str> {
        self.default_rounding_policy.as_deref()
    }
}

/// The two **soft** size caps in force for the authoring tenant (D-152, D-160).
///
/// A type rather than two loose numbers because they are read together, reported
/// under one code, and must not be confused with the two **interval** caps
/// [`CustomIntervalBounds`] carries — those are hard, and §1.2's allocation row
/// had blurred all four under one heading until D-160.
///
/// Deliberately no `Default`, for [`CustomIntervalBounds`]' reason: zero caps
/// would warn on every plan ever authored while looking exactly like a rule that
/// is switched on.
#[domain_model]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SoftSizeCaps {
    max_tier_bands_per_row: u32,
    max_price_rows_per_plan: u32,
}

impl SoftSizeCaps {
    /// Bind the caps to the values in force.
    #[must_use]
    pub const fn new(max_tier_bands_per_row: u32, max_price_rows_per_plan: u32) -> Self {
        Self {
            max_tier_bands_per_row,
            max_price_rows_per_plan,
        }
    }

    /// The tier-band cap per price row.
    #[must_use]
    pub const fn max_tier_bands_per_row(self) -> u32 {
        self.max_tier_bands_per_row
    }

    /// The price-row cap per plan.
    #[must_use]
    pub const fn max_price_rows_per_plan(self) -> u32 {
        self.max_price_rows_per_plan
    }
}

/// Every rule a publish of `shape` must pass, in one report.
///
/// Runs the Slice-3 row rules over each candidate row, the Foundation's own
/// rounding rule, and the Slice-2 plan-shape set — every one of them, absorbing
/// each report into the aggregate. A non-empty `violations` blocks the publish
/// wherever the finding appeared.
///
/// The pipelines are rebuilt per call rather than held: one built per tenant
/// would enforce whichever tenant's limits it was built for, and one held across
/// time would keep rejecting plans after an operator had raised a cap.
#[must_use]
pub fn run_publish_rules(shape: &PlanShape, params: &PublishRuleParams) -> ValidationReport {
    let mut report = ValidationReport::default();

    let row_rules = price_row_rules();
    for record in &shape.rows {
        report.absorb(row_rules.run(&record.row));
    }
    report.absorb(foundation_plan_rules(params).run(shape));
    report.absorb(plan_shape_rules(params.interval_bounds, params.descriptors.clone()).run(shape));
    // Slice 6's consumer contracts, after both plan-level sets and before the
    // coverage set, for the reading order this module's doc fixes. A contract
    // field is a statement about a row that *is* a row — Slice 3 answers that —
    // and about a plan whose shape holds — Slice 2 answers that — so a report
    // naming an absent `billingTiming` ahead of the news that the row is not a
    // row reads back to front. It stays ahead of `window_coverage_rules` for the
    // same doc's reason: coverage is a statement about a key's window plane and
    // reads last.
    report.absorb(consumer_contract_rules(&params.change_targets).run(shape));
    report.absorb(window_coverage_rules(params.opens_initial_coverage()).run(shape));
    report
}

/// The Foundation's own rules over a publish subject.
///
/// A pipeline rather than a bare call so that the next Foundation-owned rule
/// registers beside the others instead of being appended to a slice's set —
/// which is exactly the "whichever slice happens to load first" outcome §4.2
/// keeps the base set out of.
///
/// **The roster below is the roster**, and no count of it is stated here for
/// `if_match_routes`' reason: a number beside a list is one more thing to keep
/// true, and nothing holds this one true. This sentence is cited by name three
/// times inside the body, so a stale count here tells a reader placing a new rule
/// that the base set is a placeholder — the outcome the sentence exists to
/// prevent.
#[must_use]
fn foundation_plan_rules(params: &PublishRuleParams) -> ValidationPipeline<PlanShape> {
    ValidationPipeline::new()
        .with_rule(Box::new(RoundingPolicyResolved {
            tenant_default: params.default_rounding_policy.clone(),
        }))
        .with_rule(Box::new(PlanSizeWithinSoftCaps {
            caps: params.size_caps,
        }))
        .with_rule(Box::new(GrandfatherHorizonOnItsClass))
        .with_rule(Box::new(NoUnjudgedPrimitive))
        .with_rule(Box::new(BundleMarketBasisUnmixed {
            markets: params.referencing_markets.clone(),
        }))
        // `inst-tx-region` (Slice 4). **Registered here, in the Foundation's own
        // set, rather than called from a slice seam** — this module's doc gives
        // the reason and it is the same one `BundleMarketBasisUnmixed` cites: a
        // rule bolted on at a call site is the "whichever slice happens to load
        // first" outcome §4.2 keeps the base set out of.
        //
        // It belongs to the base set rather than to a Slice-4-only pipeline
        // because `region` is a **scope-key axis**: every publish of every plan
        // has one on every row, so there is no publish this rule does not apply
        // to.
        .with_rule(Box::new(RegionsDeclared {
            declared: params.declared_regions.clone(),
        }))
        // D-334's vocabulary check, in the base set for `RegionsDeclared`'s
        // reason, and a tenant who declared no vocabulary is not constrained by
        // it.
        //
        // `rounding_policy_ref` is **not** on every price row: the column is nullable
        // and a `None` row resolves to the tenant default, which
        // `RoundingPolicyResolved` short-circuits on. That is why the rule is handed
        // the default as well — see its type doc.
        .with_rule(Box::new(RoundingPolicyDeclared {
            declared: params.declared_rounding_policies.clone(),
            tenant_default: params.default_rounding_policy.clone(),
        }))
        // D-356's GL-code vocabulary check, beside the other two vocabulary rules
        // and for `RegionsDeclared`'s reason: it reads a tenant-declared set the
        // Foundation resolves, so registering it inside the Slice-2 descriptor
        // set would be a rule bolted on at a call site. It judges the
        // descriptor's one `glCode` — a present value only; absence is
        // `inst-ds-required`'s finding — and a tenant who declared no vocabulary
        // is not constrained by it.
        .with_rule(Box::new(GlCodeDeclared {
            declared: params.declared_gl_codes.clone(),
        }))
        // Slice 4's tax-display pair. In the Foundation set for
        // `RegionsDeclared`'s reason: `tax_inclusive` is a column on every price
        // row, so there is no publish these do not apply to.
        .with_rule(Box::new(TaxBasisComplete {
            policy: params.tax_display_policy,
            readiness: params.region_readiness.clone(),
        }))
        .with_rule(Box::new(MarketBasisUniform))
        // `inst-cb-addon` — case (i) of the single-currency-per-invoice binding.
        // Cases (ii)/(iii) are the bundle plane's and are enforced by
        // `domain::bundle_rules::check_coverage`, which reaches
        // its **currency** arm through `currency_binding::uncovered_pairs` — the
        // delegation D-211 ordered. The region arm stayed S8's. This comment said
        // that delegation was "owed and is not made here"; it outlived the swap.
        .with_rule(Box::new(RequiredAddonsCoverMarkets {
            coverage: params.addon_coverage.clone(),
        }))
}

/// `inst-bc-taxbasis`'s **reverse** half: this plan is a component, and its rows
/// must not mix the tax display basis of a bundle that sells them (D-119, D-212).
///
/// # Why it is here and not in the bundle's own validator
///
/// The forward half — every row of one sold market shares one `tax_inclusive` —
/// is a property of a composition handed to `domain::bundle_rules`, and it holds
/// at the instant that bundle publishes. This half is about **another plan's**
/// publish, so it needs the set of bundles referencing that plan, which no pure
/// walk over one plan shape can have. Without it the bundle-side check is *"a
/// point-in-time promise a later component publish silently breaks"* — D-119's
/// own words for the hazard.
///
/// It registers **here**, in the Foundation's pipeline, rather than as a call
/// bolted into `infra::publish`: this module's own doc gives the reason, and it
/// is the reason a second assembly point is refused — a slice-side call is the
/// *"whichever slice happens to load first"* outcome §4.2 keeps the base set out
/// of.
///
/// # The two derivations D-212 settles, so nothing here is invented
///
/// **The markets are this publish's own candidate rows'**, not the referencing
/// bundle's declared set. A component's publish does not carry that set — D-216
/// leaves it on the *bundle's* publish request — and the narrower one is complete
/// for this guard, because a basis conflict cannot arise in a market this plan
/// contributes no row to.
///
/// **The revision is the referencing bundle's current published one**, resolved
/// by the caller. A draft revision's component set is not yet anybody's truth,
/// and guarding against it would fail a publish over a composition no consumer
/// can see.
///
/// Reported **once per (bundle, market)** that disagrees, naming the bundle,
/// because D-119 requires the referencing bundle enumerated and because a
/// component sitting in two bundles gives an operator two things to fix — a
/// refusal naming one of them makes the next attempt fail for a reason the first
/// refusal did not mention.
#[domain_model]
#[derive(Clone, Debug)]
struct BundleMarketBasisUnmixed {
    markets: Vec<ReferencingMarket>,
}

impl ValidationRule<PlanShape> for BundleMarketBasisUnmixed {
    fn name(&self) -> &'static str {
        "foundation.bundle_market_basis_unmixed"
    }

    fn evaluate(&self, subject: &PlanShape, report: &mut ValidationReport) {
        for market in &self.markets {
            let mut divergent: BTreeSet<Uuid> = BTreeSet::new();
            for record in subject.rows.iter().filter(|record| {
                record.scope_key.currency() == &market.currency
                    && record.scope_key.region() == &market.region
            }) {
                if record.tax_inclusive != market.tax_inclusive {
                    divergent.insert(record.price_id);
                }
            }
            if divergent.is_empty() {
                continue;
            }
            let rows: Vec<String> = divergent.iter().map(ToString::to_string).collect();
            report.violate(
                BUNDLE_TAX_BASIS_MIXED,
                format!(
                    "{}/{}/{}",
                    market.bundle_id,
                    market.currency.as_str(),
                    market.region.as_str()
                ),
                format!(
                    "bundle {} sells market ({}, {}) on tax_inclusive = {}, and this publish would \
                     put {} row(s) of a different basis into it: {}. A bundle composes several \
                     plans onto one invoice, so one market carries one display basis",
                    market.bundle_id,
                    market.currency.as_str(),
                    market.region.as_str(),
                    market.tax_inclusive,
                    rows.len(),
                    rows.join(", ")
                ),
            );
        }
    }
}

/// No published row may carry a Slice-10 primitive whose rules are unbuilt
/// (D-179; the hazard is D-177's).
///
/// **This is the gate that makes mounting the publish route safe.** Publish is
/// the freezing act: a stored value becomes an immutable version on a ≥ 7-year
/// horizon, with no further code change and no other gate that would notice.
/// D-177 refuses the remaining fields on every *authoring* path, which is the
/// right posture and an insufficient one — it holds only while every authoring
/// path exists and refuses, while a row authored before that refusal landed was
/// never offered to it at all. This rule does not ask how the value got there.
///
/// **A row carrying an allowance it *can* honour passes** — a `{N, none}`
/// declaration on a `usage` row is judged by `inst-ac-gate` like any other
/// authored fact and compiled by publish. What this rule refuses of that field is
/// the `carry` half alone.
///
/// *(The bulk-import arm is **built**: `domain::import` raises this same code on
/// the bulk plane and the submit route is mounted (D-300). Calling it owed here
/// is the premise two other module docs inherit, this rule being the one they
/// name as guard number one.)*
///
/// Reported **once per row**, naming the row and the field, because that is the
/// edit.
///
/// See [`PRIMITIVE_RULES_UNBUILT`] for why the code is separate from the
/// authoring refusal's, and for the order its removal takes.
#[domain_model]
#[derive(Clone, Copy, Debug)]
struct NoUnjudgedPrimitive;

/// Which unjudged Slice-10 primitives a row carries, and what is missing for each.
///
/// **One list, three refusals.** This rule is the publish backstop (D-179); the
/// bulk import's Phase-1 arm (D-177) and `api::rest::prices` refuse the same
/// things at authoring, where the operator can still fix the batch. A second
/// spelling is a second thing to update — and this list is what proved the point
/// when Slice 10's allowance landed: the allowance arm narrowed from *the field*
/// to *`rolloverPolicy = carry`* in **one** place and all three surfaces moved
/// together. [`PRIMITIVE_RULES_UNBUILT`] names the **reason** rather than the
/// field for the same economy, so a fourth rostered-but-unhonourable value needs
/// no second code.
///
/// The second element is what is missing, which differs per field and is what
/// makes the two refusals readable rather than generic; each caller wraps it in
/// its own sentence, because "publish stops" and "the batch stops" are different
/// acts.
#[must_use]
pub fn unjudged_primitives(row: &PriceRow) -> Vec<(&'static str, &'static str)> {
    let mut carried = Vec::new();
    if row
        .included_allowance
        .is_some_and(|allowance| allowance.rollover_policy.is_carry())
    {
        carried.push((
            "includedAllowance with rolloverPolicy = carry",
            "its compiled per-period grant has no store in this gear: `pricing_plan_grant` \
             (D-52) does not exist, so `inst-ac-carry`'s promotional grant would be neither \
             written nor issued and the carried allowance would silently never arrive",
        ));
    }
    if row.tier_qualification_window.is_some() {
        carried.push((
            "tierQualificationWindow",
            "the Slice-10 rules that would judge it are unbuilt, as is the rate lock \
             (`inst-tt-lock`) that gives the value meaning",
        ));
    }
    carried
}

impl ValidationRule<PlanShape> for NoUnjudgedPrimitive {
    fn name(&self) -> &'static str {
        "foundation.no_unjudged_primitive"
    }

    fn evaluate(&self, subject: &PlanShape, report: &mut ValidationReport) {
        for record in &subject.rows {
            for (field, missing) in unjudged_primitives(&record.row) {
                report.violate(
                    PRIMITIVE_RULES_UNBUILT,
                    record.price_id.to_string(),
                    format!(
                        "this row carries `{field}`, and {missing}; publish stops rather than \
                         freezing an unjudged value into an immutable version"
                    ),
                );
            }
        }
    }
}

/// Every published row must resolve a rounding policy — its own, or the
/// tenant's.
///
/// Rounding decides the last minor unit of **every** charge computed from the
/// row, so an unresolved policy is not a missing nicety: it is a plan whose
/// prices are exact to within a unit nobody agreed on, and downstream would have
/// to pick a mode to compute anything at all. The design set's answer is to fail
/// publish rather than to pick one (PRD §17.4's no-implicit-rounding rule), and
/// the resolved id then freezes into the read model and the snapshot.
///
/// The tenant default is a **field**, resolved by the caller from
/// `pricing_policy_object` — see [`PublishRuleParams`] for why it is not read
/// here.
///
/// Reported **once per row**, naming the row, because that is the edit: the
/// author sets `rounding_policy_ref` on the rows that lack one, or an operator
/// configures the tenant default and every one of them resolves at once.
#[domain_model]
#[derive(Clone, Debug)]
struct RoundingPolicyResolved {
    /// The tenant's `default_rounding_policy_ref`, when they have configured
    /// one. `None` is the fail-closed reading: every row then carries its own or
    /// the plan does not publish.
    tenant_default: Option<String>,
}

impl ValidationRule<PlanShape> for RoundingPolicyResolved {
    fn name(&self) -> &'static str {
        "foundation.rounding_policy_resolved"
    }

    fn evaluate(&self, subject: &PlanShape, report: &mut ValidationReport) {
        // **Blank is not a default.** `Some("")` and `Some("   ")` are `is_some()`,
        // and this guard lifts the resolution check off *every row of the plan* — so
        // a tenant whose `default_rounding_policy_ref` holds whitespace publishes
        // plans on which rounding is decided by nothing at all, which is the one
        // outcome this rule exists to stop. The column carries no non-blank CHECK,
        // so the store admits the value; D-242 spells that guard
        // `length(btrim(value)) > 0` for the taxonomies and this is its reading in
        // the domain.
        if self
            .tenant_default
            .as_deref()
            .is_some_and(|policy| !policy.trim().is_empty())
        {
            return;
        }
        for record in &subject.rows {
            if record.rounding_policy_ref.is_none() {
                report.violate(
                    ROUNDING_POLICY_UNRESOLVED,
                    record.price_id.to_string(),
                    "this row carries no roundingPolicyRef and the tenant has no default \
                     rounding policy; rounding decides the last minor unit of every charge, \
                     so publish stops rather than choosing one",
                );
            }
        }
    }
}

#[cfg(test)]
#[path = "rules_tests.rs"]
mod rules_tests;

/// The §14 soft size caps, reported and never enforced (`nfr-size-limits`,
/// D-160).
///
/// Two caps, one code, `warnings[]` only:
///
/// - **price rows per plan** — a plan above the cap is one whose publish, read
///   model delta and snapshot are all proportional to a number nobody bounded;
/// - **tier bands per row** — a ladder above the cap is one every rating call
///   walks.
///
/// **Why this is an advisory and not a violation.** PRD §14 ratifies both as a
/// `SHOULD`. Making them blocking would let a raised-then-lowered cap turn a
/// published plan into one its own tenant cannot re-publish, and it would reject
/// a legitimately large catalogue with no remedy but to split it — D-160's
/// rejected option (b), taken there rather than here.
///
/// **The caps are the authoring tenant's** (D-152), resolved by the caller from
/// `pricing_policy_object` and held as a **field**: a rule that read
/// configuration inside `evaluate` could answer differently in §4.2's two runs
/// for no authored reason. A tenant with no policy row takes the ratified
/// deployment default, which is what makes the number in force readable at all.
///
/// The report names **which** cap, **its limit** and **the count**, because those
/// three are the whole of the operator's next action: shorten this ladder by six
/// bands, or split this plan.
#[domain_model]
#[derive(Clone, Copy, Debug)]
pub struct PlanSizeWithinSoftCaps {
    /// The caps in force for the authoring tenant.
    pub caps: SoftSizeCaps,
}

impl ValidationRule<PlanShape> for PlanSizeWithinSoftCaps {
    fn name(&self) -> &'static str {
        "nfr-size-limits"
    }

    fn evaluate(&self, subject: &PlanShape, report: &mut ValidationReport) {
        let rows = u32::try_from(subject.rows.len()).unwrap_or(u32::MAX);
        let row_cap = self.caps.max_price_rows_per_plan();
        if rows > row_cap {
            report.warn(
                PLAN_SIZE_SOFT_CAP_EXCEEDED,
                subject.subject(),
                format!(
                    "the plan carries {rows} price rows, above the soft cap of {row_cap} rows per \
                     plan: the publish, the read-model delta and the snapshot are all \
                     proportional to this number. Advisory - the publish proceeds"
                ),
            );
        }

        let band_cap = self.caps.max_tier_bands_per_row();
        for record in &subject.rows {
            let bands = u32::try_from(record.row.bands.len()).unwrap_or(u32::MAX);
            if bands <= band_cap {
                continue;
            }
            report.warn(
                PLAN_SIZE_SOFT_CAP_EXCEEDED,
                format!("{}|{}", subject.subject(), record.price_id),
                format!(
                    "row {} carries {bands} tier bands, above the soft cap of {band_cap} bands \
                     per row: every rating call walks the ladder. Advisory - the publish proceeds",
                    record.price_id
                ),
            );
        }
    }
}

/// A grandfathering horizon belongs only to an `existing_grandfathered` row
/// (S7 §5, D-147).
///
/// **Why the subject is the plan and not the row.** `grandfather_until` lives on
/// [`PriceRecord`](crate::domain::price_record::PriceRecord), not on
/// [`PriceRow`](crate::domain::price_row::PriceRow), so
/// [`price_row_rules`](crate::domain::rules::price_row_rules) cannot see it —
/// and widening `PriceRow` to reach it would move a field across the D-162
/// roster boundary and imply a generation bump for something that is not
/// evaluation policy at all. The eligibility class it pairs with is on the
/// **scope key**, one level up again. So the rule's subject is the plan shape,
/// which already carries both.
///
/// **It is registered in the aggregate's own pipeline rather than in Slice 2's.**
/// The rule is S7's, and `plan_rules::plan_shape_rules` is the Slice-2 set by its
/// own doc — a rule filed there would make Slice 2 the owner of a Slice-7
/// sentence. This is the same reason [`RoundingPolicyResolved`] sits here.
///
/// **What it does not duplicate.** The repository refuses the same pairing on
/// both authoring paths and pre-empts the column `CHECK`'s 500 there. That guard
/// is the guarantee and this is the report: an author who submits a plan with
/// three faults gets three lines, one of them this one, instead of discovering it
/// one write at a time.
#[domain_model]
#[derive(Clone, Copy, Debug, Default)]
pub struct GrandfatherHorizonOnItsClass;

impl ValidationRule<PlanShape> for GrandfatherHorizonOnItsClass {
    fn name(&self) -> &'static str {
        "inst-el-fields"
    }

    fn evaluate(&self, subject: &PlanShape, report: &mut ValidationReport) {
        for record in &subject.rows {
            let Some(horizon) = record.grandfather_until else {
                continue;
            };
            let class = record.scope_key.price_eligibility();
            if class == PriceEligibility::ExistingGrandfathered {
                continue;
            }
            report.violate(
                GRANDFATHER_UNTIL_FORBIDDEN,
                format!("{}|{}", subject.subject(), record.price_id),
                format!(
                    "row {} is filed under priceEligibility {class} and carries a grandfathering \
                     horizon of {horizon}: the horizon says when a retained cohort stops being \
                     retained, and a row that retains nobody has no cohort for it to end",
                    record.price_id
                ),
            );
        }
    }
}
