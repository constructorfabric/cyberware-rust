//! `inst-mat-registered`'s **always-material trigger** list — §3 step 4 of
//! `design/05-governance.md`, as a closed enumeration.
//!
//! A registered trigger is material *whatever a threshold says and whatever delta
//! it carries*, which is why it is a list rather than a comparison: D-50, D-62,
//! D-104, D-109 and D-115 each added to it after a review found a change that
//! reached consumers approver-free precisely because the per-row delta evaluator
//! had nothing to trip on. Auto-publish is therefore what is left over, and §3
//! states the residue in one sentence: *"a pure amount change on unchanged
//! geometry, below an explicitly configured threshold, not a first publish."*
//!
//! # Triggers naming absent slices register **with their slice**
//!
//! **Five** of this list name a subject this crate has no table, no entity and
//! no surface for — a bulk group move, a retirement that unwinds a live cutover,
//! the two gate-clearing republishes, a prepaid grant's non-price fields.
//!
//! *(There is no historical-import variant and there cannot be: D-330 struck it
//! from the design set, taking `BackdateGrant`, the `historical_import` resource
//! label, the `POST /historical-imports` routes and the `pricing_historical_price`
//! store with it. `only_the_triggers_with_no_subject_in_this_crate_answer_false`
//! is the census that holds the `false` side to a count — without it a wrong one
//! keeps a green suite.)*
//!
//! **One of the five does not belong in that sentence either**: the
//! retirement unwind's table (`pricing_approval`), entity and surface
//! (`POST …/plans/{planId}/retire`) are all here and mounted, and what is missing
//! is the **act** plus two operands the crate cannot supply — see that variant's
//! own doc. Listing it as a missing table would read as "Slice 11 has not landed",
//! which is the understating error `PlanRetirement` is the standing example of,
//! arriving one variant later on the same plane. `RevenueShareChange` is not among
//! them: it is declared by a surface, and the section below records what that took.
//! The five are declared here anyway, each
//! with [`Trigger::owning_slice`] naming the document that owns it and
//! [`Trigger::subject_exists_in_this_crate`] answering `false`, for two reasons.
//! The set does not then read as **incomplete** — a reader who greps
//! `inst-mat-registered` finds every clause of it accounted for, rather than a
//! subset and no statement about the rest. And no token exists without a writer:
//! a variant that answers `false` is not claiming to be enforced, which is
//! exactly what a variant silently omitted from an `ALL` roster would let a reader
//! assume about the ones that are there. That is the same discipline D-158 and
//! D-175 apply to the audit vocabulary, applied to a materiality one.
//!
//! **A `false` here is a fact about this repository, not about the design set.**
//! The slice that lands the subject flips it, and the compiler finds every place
//! that has to change because [`Trigger::ALL`] and both `match`es below are
//! exhaustive.
//!
//! **`BundleComposition` and `RevenueShareChange` answer `true` on the whole
//! subject being here**: four tables, four entities, the composition's revision
//! lifecycle, and three mounted routes. Counting *"`infra::bundle`'s two
//! `ChangeSet::of_act` declarations"* among them is the error D-232 names — those
//! two functions are `pub fn`s that *build* a change set, and a constructor nobody
//! calls is not a declaration. `composition_change_set`'s caller is
//! `api::rest::bundles::bundle_publish_materiality`, which evaluates a verdict on
//! the mounted publish route in place of a hard-coded literal; without one,
//! `RevenueShareChange` answers `false` (D-321), and the section below is what
//! gives it a caller.
//!
//! # The trigger that had to wait for a reader before it could have a writer
//!
//! `RevenueShareChange` spent a day on the `false` side with its whole subject
//! present: `pricing_bundle_revshare` is a table here, the D-07 reconciler is here,
//! `PATCH …/bundles/{id}` authors the shares. What was absent was a **surface
//! declaring the act**, which on the act half is what this predicate is about — the
//! distinction `GrandfatheringCutover` waited three commits on, stated from the
//! other side.
//!
//! It survived both walks above for two waves because both asked whether this
//! crate's code *names* the trigger, and `rev_share_change_set`'s own body names
//! it. **Naming is not reachability**, and the walks now ask for a producing site
//! inside a function something else in the crate calls. That axis is what would
//! have caught this on the day Slice 8 landed, and it is what will catch the next
//! one: the two directions are a check of the attestation, and a check a dead
//! function can pay is a check of the vocabulary.
//!
//! **And the flip back was not the census being satisfied.** D-321 refused to wire
//! the constructor precisely *because* the census was the only thing that would
//! have noticed: `MaterialityVerdict` carried no trigger identity, so declaring
//! `RevenueShareChange` instead of `BundleComposition` produced a byte-identical
//! response and a byte-identical stored document. Two operands had to arrive first,
//! and both are here — somewhere for the answer to live
//! ([`MaterialityVerdict::trigger`](super::MaterialityVerdict::trigger), which
//! reaches every registered act and not only this pair) and a way to tell the two
//! acts apart (`infra::bundle::declared_act`, a diff of the composition being
//! published against the last one that was live). The order is the lesson: a
//! declaration nothing can observe is not a declaration, and the instrument that
//! would have called it one is the instrument being gamed by its own fix.
//!
//! **`GrandfatheringCutover` followed on the same rule, and the wait is worth
//! recording.** Its *store* landed first — `domain::cutover`'s compose, the
//! generation key, `price_repo::commit_cutover_rows`,
//! `infra::cutover::commit_cutover`, the act's identity — and it stayed `false`
//! through all of it, because a registered trigger of this kind is an **act** and
//! [`triggered`] reads it back from a change set *some surface declared*. Nothing
//! declared it until `infra::cutover::cutover_in` did. **The predicate is about a
//! declaration, not about a table**, and the three commits between the store and
//! the declaration are what that distinction looks like in practice.
//!
//! **`PlanRetirement` never belonged on the `false` side at all**, and the
//! paragraph above listed "retirement" among the absent subjects while
//! `infra::retirement::retire_in` was declaring the act on a mounted route. That
//! is the first error of this shape found in the *understating* direction, and it
//! is worse than the overstating one: an overstated `true` claims work that is not
//! there and a reader who greps finds nothing; an understated `false` denies work
//! that is, and a reader who trusts it builds the surface a second time. Both
//! directions are now walked —
//! `triggers_tests::every_act_half_trigger_answering_{true,false}_is_named_by_{a,no}_producing_site`.
//!
//! # Two halves, because a trigger is either a property of the act or of the diff
//!
//! [`triggered`] answers the **act** half: "this call is a window cancellation" is
//! not expressible in a change set of rows and a baseline, so the surface
//! performing the act declares it — [`ChangeSet::of_window_mutation`] for the window
//! plane, [`ChangeSet::of_act`] for the D-10 policy diff, which is an act and no
//! publish unit at all — and this function reads it back. That is the sentence
//! [`MaterialityReason::AlwaysMaterialTrigger`](super::MaterialityReason::AlwaysMaterialTrigger)'s
//! own doc used to make about why `evaluate` could never answer it; the change set
//! now carries the act, so it can.
//!
//! [`triggered_by_content`] and [`triggered_by_row`] answer the **diff** half —
//! D-115's two and the `grandfatherUntil` horizon — because those are not acts at
//! all: they are properties of what the change set says next to what is published,
//! and no surface can declare them without recomputing the comparison the
//! evaluator is already making. They are two functions because one of the three
//! ranges over the whole change set and two range over a row, and folding them
//! would make the whole-set one unreachable — see [`triggered_by_content`].
//!
//! # The horizon trigger has a dedicated surface, and this
//! # paragraph is the fourth correction of one sentence
//!
//! `inst-mat-registered`'s first clause is `grandfatherUntil` tightening
//! (Foundation §4.3), whose named surface is S7's
//! `PATCH /bss-pricing/v1/prices/{priceId}/grandfather-until`. **That route is now
//! mounted** — `api::rest::cutovers`, over `infra::grandfather` — and
//! `price_repo::tighten_grandfather_until` is the repository half. So
//! [`Trigger::subject_exists_in_this_crate`]'s `true` no longer rests on the
//! **column** existing on `PriceRecord` alone: the act is performable, and the
//! trigger has left `triggers_tests`' content-half exemption list because a census
//! can now ask for its producing site like any other's.
//!
//! **The act half is where that door declares it**, through
//! [`ChangeSet::of_horizon_tightening`](super::ChangeSet::of_horizon_tightening),
//! and the reason is `evaluate`'s ordering rather than a reclassification of the
//! fact: the content half below is reached only after the policy and baseline
//! steps, so a tenant with no configured threshold would be told
//! `noConfiguredThreshold` about an act no threshold governs. The door can declare
//! it honestly because it refuses a loosening before the evaluator is reached, so
//! every call that gets that far tightens. [`triggered_by_row`] is unchanged and
//! keeps answering for the other producer of a moved horizon — a superseding
//! successor row — which is the case the rest of this paragraph is about.
//!
//! This sentence has been corrected three times before: it claimed a reachable path,
//! then claimed for two waves that **no** mounted surface reached it, and the sequence
//! is worth keeping because the operand kept moving under it: *"a successor draft
//! row on a key that already carries a published row, whose `grandfatherUntil` is
//! earlier than the baseline's"*. The **authoring** door still refuses that row —
//! `price_repo::insert_prepared` refuses a draft whose key `find_key_occupant` finds
//! occupied, `DUPLICATE_SCOPE_KEY` — and the way to reprice an occupied key is the D-88
//! supersession unit. **That unit is whole**: the row half
//! (`price_repo::insert_successor_draft_on`, D-195, where the row *pair* below became
//! constructible), the compose (`domain::supersession`), the cross-plane commit
//! (`infra::supersession::commit_supersession`), the orchestrator and approval unit
//! (`infra::supersession::SupersessionService`) and the route
//! (`POST …/plans/{planId}/supersessions`). So a caller **can** now stage the pair, and
//! [`triggered_by_row`]'s **second** arm — the no-computable-delta one — is reachable
//! from a mounted surface for the first time.
//!
//! **The horizon arm of `triggered_by_row` is still not reachable that way**, and the
//! distinction is worth keeping sharp now that the *act* is: that arm needs a
//! **successor row** whose `grandfatherUntil` is earlier than its predecessor's, which
//! is a comparison between two rows on one key. The column is authorable only on an
//! `existing_grandfathered` generation (`GRANDFATHER_UNTIL_FORBIDDEN`), and
//! `price_repo::refuse_unsupersedable_class` refuses to supersede that class at all
//! (Foundation §4.3) — which `infra::supersession` also refuses from the request
//! alone, ahead of everything. So the *successor* shape stays unit-tested over
//! hand-built rows. A second mechanism reaches the same trigger without being a
//! diff: `infra::grandfather` moves **one** row's horizon in place, which
//! `update_draft`'s `lifecycle_state = 'draft'` swap guard cannot do. Two
//! producers, one trigger, and only one of them is a diff.

use toolkit_macros::domain_model;

use super::delta::{RowDelta, row_delta};
use super::{ChangeSet, PublishedPriceBaseline};
use crate::domain::price_record::PriceRecord;
use time::OffsetDateTime;

/// One registered always-material trigger.
///
/// Closed and exhaustive: `inst-mat-registered` enumerates its list and a
/// trigger that arrived without a name would arrive without an owner — the
/// argument [`MaterialityReason`](super::MaterialityReason) already makes about
/// having no `Other` arm.
#[domain_model]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Trigger {
    /// `grandfatherUntil` tightening (Foundation §4.3).
    GrandfatherHorizonTightening,
    /// A grandfathering cutover (Slice 7).
    GrandfatheringCutover,
    /// Plan retirement while a cutover unit is pending or approved-not-yet-
    /// effective (Slice 11, D-05) — the retirement unwinds the approved unit.
    ///
    /// **It answers `false` below for a different reason than its five
    /// neighbours**, and the module doc's "no table, no entity and no surface"
    /// sentence does not describe it: the table is `pricing_approval`, the entity
    /// is here, and `POST …/plans/{planId}/retire` is mounted and serving. What is
    /// absent is the **act** — `infra::retirement::retire_in` performs no part of
    /// the unwind — and two of D-05's four clauses cannot be performed from this
    /// crate at all: the predecessor's pre-cutover `effectiveTo` is overwritten in
    /// place by `commit_cutover` and recorded by no row, event or audit entry, and
    /// `unwound` has no **store** column yet. The state itself *is* declared —
    /// `05-governance.md` §4 and §6 and `inst-as-unwind`, by D-333 —
    /// so what is owed is the column and the enum variant, in this crate, on a
    /// decision already taken — not a blocked external dependency.
    /// `infra::retirement::retire_in`'s "what this function does not
    /// do" section carries the argument in full and
    /// `tests/sqlite_cutover_unwind.rs` holds it open.
    ///
    /// **A producer here changes something observable.** D-109 makes retirement
    /// always material unconditionally and `retire_in` declares
    /// [`Self::PlanRetirement`] on every path, so the *materiality* is identical
    /// under either declaration and D-05's always-material clause is paid (D-327
    /// clause (4)). What is not identical is the **record**:
    /// [`MaterialityVerdict`](super::MaterialityVerdict) carries the trigger now, so
    /// a verdict declared under this variant reads `retirementUnwindingACutover`
    /// where one declared under [`Self::PlanRetirement`] reads `planRetirement`.
    ///
    /// D-327 clause (4) rested the payment on the two rendering byte-identically,
    /// and that ground is gone; the clause survives on the weaker and still-true
    /// claim that no publish's authorization requirement moves either way. **Owed
    /// back to the register**, and stated here rather than quietly corrected,
    /// because it also removes the argument D-321 used against wiring the bundle
    /// pair — a producer for this variant would now be visible to an operator and
    /// not only to a census. `tests/sqlite_cutover_unwind.rs` carries both halves.
    ///
    /// It is why this variant's absence had no symptom, and it is the reason it
    /// survived every census: the walks ask whether a producer exists, never
    /// whether one would be distinguishable.
    RetirementUnwindingACutover,
    /// An **immediate** customer-group membership re-resolution (Slice 9).
    /// Renewal-aligned single-membership changes are audit-only, not material.
    ImmediateMembershipReresolution,
    /// A bulk group discount or group move (Slice 9).
    BulkGroupMove,
    // `HistoricalImport` — "a governed backdated reference import (Slice 5, D-13)
    // — every one, since backdated rows shape `migrated-origin` snapshots" — stood
    // here and is **struck** (D-330). D-13's two controls, of which
    // registering the import as an always-material trigger was one, go with the
    // flow they governed. It is a **deletion** rather than a `false` arm: a `false`
    // here says "this repository has no subject for a trigger the design set
    // registers", and `inst-mat-registered` no longer registers this one — so
    // keeping it would make the roster claim a clause of §3 step 4 that is not
    // there, which is the reading the whole `ALL`/`match` discipline above exists
    // to prevent. The **registered trigger count falls by one**, and a denominator
    // here is not monotone (D-330 clause 1's own argument, D-239's precedent).
    /// Any diff of the approval-threshold policy itself (Slice 5, D-10),
    /// direction-agnostic: the two-person rule's foundation must not be
    /// single-person-editable.
    ThresholdPolicyDiff,
    /// A GA-gate-clearing re-publish (Slice 4 `inst-td-clear`). Content-identical
    /// by construction, so a per-row delta sees nothing.
    GaGateClearingRepublish,
    /// The Slice 10 prepaid analogue of the GA gate (D-29).
    PrepaidGateClearingRepublish,
    /// A prepaid grant's **non-price** fields — `category`, `applicability`,
    /// `drawdownPriority` (Slice 10 `inst-pg-material`). Grant *price* changes
    /// are ordinary per-currency deltas and are deliberately not here.
    GrantNonPriceField,
    /// Creating a `PriceOverlay`, adding or removing an adjustment line, or any
    /// line magnitude, kind, scope, precedence, dating or disclosure change
    /// (Slice 9, D-50).
    PriceOverlayMutation,
    /// Cancelling a scheduled window (Slice 7, D-62).
    WindowCancellation,
    /// Shortening a window's `effectiveTo` (Slice 7, D-62).
    ///
    /// **Not the shorten a D-88 supersession composes** (D-201):
    /// `inst-su-commit` carves that one out and `inst-mat-registered` repeats the
    /// carve-out from this side. The hazard D-62 registers is coverage *removal*; the
    /// supersession hands coverage over inside the same transaction, and
    /// `domain::supersession::compose_windows` refuses the composition outright if any
    /// window occupying the key begins at or after the changeover — so it cannot
    /// truncate an approved scheduled successor at all. The exemption is that
    /// composition's, not any caller's: a shorten reaching the window plane by any
    /// other route carries this trigger. That is why `SupersessionService` evaluates
    /// its unit on the per-currency delta alone and declares no trigger here.
    WindowShortening,
    /// Bundle creation or a component add / remove / replace (Slice 8, D-104).
    BundleComposition,
    /// A rev-share change — `share_bp`, `platform_cut_bp`,
    /// `residual_absorber_party` — or a `price_basis` / `invoiceItemization`
    /// change (Slice 8, D-104).
    RevenueShareChange,
    /// Plan retirement, unconditionally (Slice 11, D-109).
    PlanRetirement,
    /// A row whose change carries **no computable price delta** (D-115): a
    /// geometry or quantity-determining field, or one of the row contract
    /// fields. [`super::delta`] decides which, and names it.
    NoComputableRowDelta,
    /// A plan revision that moves **no price row at all** (D-115): its content is
    /// the plan's shape — the descriptor set, the phase graph and durations, the
    /// add-on rule set, the cycle, the availability dates, the tier override, the
    /// plan-change contract, the composite meter definitions, and the plan-level
    /// **period floor/cap** (D-319) — none of which carries a price delta. A
    /// trial stretched 7 → 90 days is this case; a $500-per-period minimum is
    /// the same case with a money consequence.
    ///
    /// **The condition is the change set moving no row, and that is narrower
    /// than the list above** (reported by review, not fixed here).
    /// A publish carrying a shape edit **and** a sub-threshold row edit does not
    /// reach this trigger at all: `moves_no_row` is false, the per-row deltas
    /// answer `Below`, and the shape edit rides out unjudged. Closing it needs a
    /// **shape diff** against the published revision, which no `ChangeSet`
    /// carries — every operand `materiality::evaluate` holds is a price row —
    /// so it is a change to what the act declares rather than to this predicate,
    /// and it moves five facets that predate D-319 as well as its own. Owed with
    /// its own decision; recorded here because a reader of this list would
    /// otherwise believe every member of it is judged on every publish.
    PlanShapeRevisionContent,
    /// An edit of one declared scope-value taxonomy entry — a relabel, a
    /// retirement or re-activation, a region's tax markers — through
    /// `PATCH /config/taxonomies/{class}/values/{value}` (D-353). Always
    /// material: the value is a universe publish validates against, and the
    /// region universe is also C4's tax-readiness input, so one principal
    /// moving it alone is the shape dual control exists to refuse. Declaring a
    /// **new** value is not this act — nothing references a value nobody has
    /// published against.
    ///
    /// The handler evaluates this trigger only for a **referenced** value: an
    /// unreferenced edit commits without consulting materiality at all, as a
    /// declaration does (D-355). "Always material" is therefore a statement
    /// about this trigger once evaluated, not about every `PATCH`.
    TaxonomyValueMutation,
}

impl Trigger {
    /// Every registered trigger, in the order §3 step 4 names them.
    pub const ALL: &'static [Self] = &[
        Self::GrandfatherHorizonTightening,
        Self::GrandfatheringCutover,
        Self::RetirementUnwindingACutover,
        Self::ImmediateMembershipReresolution,
        Self::BulkGroupMove,
        Self::ThresholdPolicyDiff,
        Self::GaGateClearingRepublish,
        Self::PrepaidGateClearingRepublish,
        Self::GrantNonPriceField,
        Self::PriceOverlayMutation,
        Self::WindowCancellation,
        Self::WindowShortening,
        Self::BundleComposition,
        Self::RevenueShareChange,
        Self::PlanRetirement,
        Self::NoComputableRowDelta,
        Self::PlanShapeRevisionContent,
        Self::TaxonomyValueMutation,
    ];

    /// The design document that owns the trigger's subject.
    ///
    /// A path rather than a slice number, so a reader greps once. **A trigger with
    /// no owning slice is a trigger with no owner**, and this being a total
    /// `match` over a closed enum is what makes that unrepresentable.
    ///
    /// Totality is not the same property as truth: an arm can name a document that
    /// does not exist and still be exhaustive. A plausible-looking wrong name —
    /// `04-market-tax.md` for `04-currency-tax.md`, `09-overlays-groups.md` for
    /// `09-price-overlays.md` — passes any check of the string's *shape*, so the
    /// roster test opens the file instead:
    /// `triggers_tests::every_trigger_names_a_design_document_that_opens`.
    #[must_use]
    pub const fn owning_slice(self) -> &'static str {
        match self {
            Self::GrandfatherHorizonTightening => "design/01-foundation.md",
            Self::GaGateClearingRepublish | Self::TaxonomyValueMutation => {
                "design/04-currency-tax.md"
            }
            Self::ThresholdPolicyDiff
            | Self::NoComputableRowDelta
            | Self::PlanShapeRevisionContent => "design/05-governance.md",
            Self::GrandfatheringCutover | Self::WindowCancellation | Self::WindowShortening => {
                "design/07-pricewindow-linkage.md"
            }
            Self::BundleComposition | Self::RevenueShareChange => "design/08-bundles.md",
            Self::ImmediateMembershipReresolution
            | Self::BulkGroupMove
            | Self::PriceOverlayMutation => "design/09-price-overlays.md",
            Self::PrepaidGateClearingRepublish | Self::GrantNonPriceField => {
                "design/10-advanced-primitives.md"
            }
            Self::RetirementUnwindingACutover | Self::PlanRetirement => "design/11-lifecycle.md",
        }
    }

    /// Can this trigger's subject be authored in this crate at all?
    ///
    /// `false` is a fact about **this repository** and not about the design set —
    /// see the module doc. The ones that answer `true` are the ones G0–G6 gave a
    /// subject: the two window acts, the policy diff G6 mounts, and D-115's two, whose
    /// subjects (`PriceRecord` and the plan revision) this crate has carried since
    /// Slice 2. The roster is [`Self::ALL`]'s and is transcribed by
    /// `triggers_tests::only_the_triggers_with_a_subject_in_this_crate_answer_true`
    /// rather than counted here, because a count in prose beside a roster in code
    /// goes stale on the next variant.
    ///
    /// **`true` means the subject exists, not that the act is performable**, and the
    /// two come apart in exactly one place:
    /// [`Self::GrandfatherHorizonTightening`], whose column is here and whose authoring
    /// surface is not. The module doc has that at full strength.
    ///
    /// **A `true` on the act half is now census-checked.** This is a hand-written
    /// `match`, so a `true` added to it compiles and every other case in the file
    /// stays green — including the transcription, which copies whatever the match
    /// says. `triggers_tests::every_act_half_trigger_answering_true_is_named_by_a_producing_site`
    /// walks the crate's sources and requires a producing site outside this
    /// module for each one; it exists because
    /// [`Self::BulkGroupMove`] answered `true` for two days under a dated comment
    /// claiming a writer that makes no such declaration.
    ///
    /// **And a `false` is census-checked too**, which is not symmetry for its own
    /// sake: the very next reading of this `match` found
    /// [`Self::PlanRetirement`] answering `false` while
    /// `infra::retirement::retire_in` constructed it on a **mounted** route. The
    /// `true`-side census cannot see that by construction — it only visits the
    /// `true` arm — and the transcription agrees with whatever the `match` says.
    /// `triggers_tests::every_act_half_trigger_answering_false_is_named_by_no_producing_site`
    /// is the other walk. An understated `false` is the worse of the two errors:
    /// it reads as "that slice has not landed", and the next author builds the
    /// surface again.
    ///
    /// **Both walks ask whether the naming site is *reachable*, and that is a third
    /// axis rather than a refinement of the first two** (D-321). They used to ask
    /// only whether this crate's code *names* the trigger, which
    /// [`Self::RevenueShareChange`] satisfied for a day from inside
    /// `infra::bundle::rev_share_change_set` — a `pub fn` with no caller anywhere in
    /// `src/`. A trigger produced by a function nothing calls is produced by nothing,
    /// and it is the **worst** of the three errors found in this `match`, not the
    /// mildest: an overstated `true` normally corrects itself on contact, because the
    /// reader greps and finds nothing, and this one hands the reader a real function
    /// with a real body and a doc explaining what it declares. That constructor has
    /// a caller (`infra::bundle::declared_act`) and the arm has
    /// moved back — which is the obligation running in the direction the pair of
    /// walks was built to make it run.
    ///
    /// So on the **act** half this predicate is a claim about a *declaration a
    /// surface makes*, which is the bar every flip in the register was actually
    /// decided on. `true` still means "the subject exists" only where no surface
    /// could declare the trigger at all — the content half, whose three members both
    /// censuses exempt by name — and [`Self::GrandfatherHorizonTightening`] is that
    /// case, not a counterexample to this one.
    #[must_use]
    pub const fn subject_exists_in_this_crate(self) -> bool {
        match self {
            Self::WindowCancellation
            | Self::WindowShortening
            | Self::ThresholdPolicyDiff
            | Self::NoComputableRowDelta
            | Self::PlanShapeRevisionContent
            | Self::GrandfatherHorizonTightening
            | Self::BundleComposition
            | Self::GrandfatheringCutover
            | Self::PriceOverlayMutation
            // The customer-group plane's one-payer act.
            // `api::rest::customer_groups::immediate_membership_materiality`
            // builds `ChangeSet::of_act(Trigger::ImmediateMembershipReresolution,
            // …)` on every arrival of `POST …/members/{payerId}/move`.
            | Self::ImmediateMembershipReresolution
            // The bulk door: `api::rest::customer_groups::bulk_membership_materiality`
            // builds `ChangeSet::of_act(Trigger::BulkGroupMove, …)` on every
            // arrival of `POST …/members/move`. The route is the declaration;
            // `MembershipMoveSet` being able to hold many proposals is not.
            | Self::BulkGroupMove
            // **D-104's second bundle act, declared.** It is on this side because
            // `infra::bundle::rev_share_change_set` has a caller:
            // `infra::bundle::declared_act`, which diffs the composition being
            // published against the last one that was live and answers this
            // trigger when the component set is unchanged and the rev-share moved.
            // D-321 clause (3) keeps the constructor on the standing expectation
            // that a caller is what puts the trigger here — a constructor with no
            // caller belongs on the `false` side, because the act is then declared
            // by nothing.
            //
            // The bar is the one every flip in this register was decided on: a
            // **surface declares the act**. `POST …/bundles/{id}/publish` does,
            // through `BundleService::declared_publish_act`, and the declaration
            // is now observable — the verdict carries the trigger and the stored
            // `materiality` document renders it, which is the half D-321
            // measured as missing and refused to build around.
            | Self::RevenueShareChange
            // D-109's act, and it was on the wrong side of this `match` from the
            // day `infra::retirement` landed. `retire_in` builds
            // `ChangeSet::of_act(Trigger::PlanRetirement,..)` and hands it to
            // `materiality::evaluate` on the mounted
            // `POST …/plans/{planId}/retire` — the "declared, not merely stored"
            // bar met in full, and met by more than the bar asks: the subject is
            // a whole service here, `RetirementService`, with a preview, a
            // reference check, an approval unit of its own and a window sweep.
            // The `false` said this crate has no surface for retirement while a
            // route was serving one.
            | Self::PlanRetirement
            | Self::TaxonomyValueMutation => true,
            Self::RetirementUnwindingACutover
            | Self::GaGateClearingRepublish
            | Self::PrepaidGateClearingRepublish
            | Self::GrantNonPriceField => false,
        }
    }

    /// The trigger's stable token — §6's *"trigger source"*.
    ///
    /// **Not** a second materiality vocabulary: `pricing_approval.materiality`
    /// stores [`MaterialityReason::as_str`](super::MaterialityReason::as_str)'s
    /// `alwaysMaterialTrigger` as the discriminator and this rides **beside** it,
    /// which is the arrangement §6 names in the same sentence as the per-currency
    /// deltas and the tripped rows.
    ///
    /// **It acquired its first production consumer, and the
    /// preceding state is worth keeping.** Until then this doc claimed a reader it
    /// did not have — *"this token is for the diagnostic the verdict carries beside
    /// it"* — and D-321 measured the claim: the verdict was
    /// `Material { reason, tripped } | AutoPublishable`, so the trigger's identity
    /// entered nothing, reached no column and no response, and every registered act
    /// rendered byte-identically whichever member of this enum declared it. That
    /// fact is what decided D-321 **against** giving
    /// `infra::bundle::rev_share_change_set` a caller: the call would have been
    /// observable to a census and to nothing else. So the reader had to be built
    /// before the caller could be, and both land together —
    /// [`MaterialityVerdict::trigger`](super::MaterialityVerdict::trigger) is the
    /// member and `api::rest::approvals::MaterialityView::trigger` is the column
    /// and the wire. *"The operator reading the approval record should not have to
    /// infer the act"* is the reason [`Self::RevenueShareChange`] is a separate
    /// trigger from [`Self::BundleComposition`] at all, and the record now carries
    /// which of them it was.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::GrandfatherHorizonTightening => "grandfatherHorizonTightening",
            Self::GrandfatheringCutover => "grandfatheringCutover",
            Self::RetirementUnwindingACutover => "retirementUnwindingACutover",
            Self::ImmediateMembershipReresolution => "immediateMembershipReresolution",
            Self::BulkGroupMove => "bulkGroupMove",
            Self::ThresholdPolicyDiff => "thresholdPolicyDiff",
            Self::GaGateClearingRepublish => "gaGateClearingRepublish",
            Self::PrepaidGateClearingRepublish => "prepaidGateClearingRepublish",
            Self::GrantNonPriceField => "grantNonPriceField",
            Self::PriceOverlayMutation => "priceOverlayMutation",
            Self::WindowCancellation => "windowCancellation",
            Self::WindowShortening => "windowShortening",
            Self::BundleComposition => "bundleComposition",
            Self::RevenueShareChange => "revenueShareChange",
            Self::PlanRetirement => "planRetirement",
            Self::NoComputableRowDelta => "noComputableRowDelta",
            Self::PlanShapeRevisionContent => "planShapeRevisionContent",
            Self::TaxonomyValueMutation => "taxonomyValueMutation",
        }
    }
}

/// The registered trigger this **act** is, if it is one.
///
/// Read back off the change set rather than derived: whether a call is a window
/// cancellation is knowable only at the surface performing it, and inside the
/// mutating transaction at that (D-176 — a classification made before the
/// transaction opened is a hint, and a concurrent extension committed between two
/// reads would send a shortening down the direct path).
///
/// `None` is *"not on the trigger list"* and never *"not material"*: a schedule and a
/// lengthening `PATCH` answer `None` here and are decided by the rules below, which is
/// the whole of the split `infra::window`'s module doc records.
#[must_use]
pub fn triggered(change: &ChangeSet) -> Option<Trigger> {
    change.act()
}

/// The registered trigger this **whole change set's** content is, if it is one.
///
/// Exactly one lives here — D-115's pure-shape revision — because it is the only
/// registered trigger that is a property of the change set rather than of a row.
/// The per-row ones are [`triggered_by_row`]'s, and the split is not tidiness: a
/// revision that moves no row has, by definition, no row whose delta is
/// incomputable, so a single function asking the rows first would answer `None`
/// and let the shape change through. [`super::evaluate`] therefore asks this one
/// **before** it walks the rows.
///
/// An **empty** change set answers the pure-shape trigger, and the reasoning that
/// said otherwise was a false premise. It read: *"a change set with no rows on a plan
/// whose baseline also has none is a plan that has never carried a price …
/// `inst-mat-first` owns that world, and it runs before this"*. `inst-mat-first`
/// owns the world where there is **no baseline at all**, and [`super::evaluate`]
/// answers it two steps above — so by the time this function is called the plan
/// *has* a published revision. A plan that published an empty revision and is
/// publishing a second one is not a first publish; it is a revision whose entire
/// content is the plan's shape, which is precisely D-115. The `any` flag that
/// encoded the old reading let exactly that revision auto-publish under a
/// configured policy: a trial stretched 7 → 90 days on a plan whose price rows are
/// not authored yet, approver-free.
#[must_use]
pub fn triggered_by_content(
    change: &ChangeSet,
    baseline: &PublishedPriceBaseline,
) -> Option<Trigger> {
    moves_no_row(change, baseline).then_some(Trigger::PlanShapeRevisionContent)
}

/// The registered trigger one row's move against its published predecessor is, if
/// it is one.
///
/// Two, and the order is the order of specificity. The horizon precedes the delta
/// because a row can carry both moves and the horizon is the sharper fact: a
/// tightened `grandfatherUntil` cuts a grandfathered subscriber's remaining life,
/// which is not what "no computable delta" would tell an operator reading the
/// stored verdict.
///
/// Called from inside [`super::evaluate`]'s step 3 loop rather than from a second
/// pass over the rows, so the row this answers about is the row the threshold
/// comparison is standing on. A separate pass would be a second walk that could
/// disagree with the first about which rows the change set holds.
#[must_use]
pub fn triggered_by_row(current: &PriceRecord, published: &PriceRecord) -> Option<Trigger> {
    if tightens_horizon(current.grandfather_until, published.grandfather_until) {
        return Some(Trigger::GrandfatherHorizonTightening);
    }
    matches!(row_delta(current, published), RowDelta::NotComputable(_))
        .then_some(Trigger::NoComputableRowDelta)
}

/// Does this change set move **no** price row — because it carries none, or because
/// every row it carries is exactly what is already published?
///
/// The detectable signature of D-115's pure-shape revision. It also catches a
/// **content-identical re-publish**, which is Slice 4's `inst-td-clear` trigger
/// (and D-29's prepaid analogue) arriving through the same door — those stay
/// registered with `subject_exists_in_this_crate() == false` because their gate
/// flags do not exist here, and the coincidence is in the safe direction: if such
/// a re-publish could be authored, it would be material.
///
/// **The empty row set is included**, and the caller's doc has the argument: the
/// only world an empty change set could be innocent in is a plan with no published
/// baseline, and that world never reaches here.
fn moves_no_row(change: &ChangeSet, baseline: &PublishedPriceBaseline) -> bool {
    for row in change.rows() {
        let Some(published) = baseline.row(&row.scope_key) else {
            return false;
        };
        if row.content() != published.content() {
            return false;
        }
    }
    true
}

/// Is `proposed` a **tightening** of `published`?
///
/// Two ways to cut a grandfathered subscriber's remaining life: move the horizon
/// earlier, or put one on a row that had none — an absent horizon is indefinite
/// (D-147's converse, stated in the register), so setting any horizon at all is a
/// tightening of infinity. Loosening is not here: `GRANDFATHER_LOOSEN_FORBIDDEN`
/// refuses it outright at publish, so a rule that also called it material would be
/// a second owner of a refusal.
fn tightens_horizon(proposed: Option<OffsetDateTime>, published: Option<OffsetDateTime>) -> bool {
    match (proposed, published) {
        (Some(proposed), Some(published)) => proposed < published,
        (Some(_), None) => true,
        (None, _) => false,
    }
}

#[cfg(test)]
#[path = "triggers_tests.rs"]
mod triggers_tests;
