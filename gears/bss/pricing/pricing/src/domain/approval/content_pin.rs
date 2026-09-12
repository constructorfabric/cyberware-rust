//! The approval content pin — `inst-ap-pin` (`design/05-governance.md` §2, step
//! 2a) and the `content_hash` column §6 declares.
//!
//! One function, [`content_hash`], and the whole of the two-person rule's
//! integrity rests on it: submission pins the digest of the content a reviewer
//! is about to look at, and `approve` recomputes it. Equal ⇒ the reviewer
//! approved what they saw. Unequal ⇒ `APPROVAL_CONTENT_MISMATCH` (409), because
//! something moved between the two moments and the signature would be about a
//! different plan.
//!
//! # Why the encoding is hand-framed rather than JSON
//!
//! The discipline is [`crate::domain::audit::audit_row_hash`]'s, for its
//! reasons: a domain-separation prefix so this gear's pins cannot collide with
//! any other digest the platform computes, and **length-prefixed, NULL-safe**
//! fields so two different field *boundaries* cannot collide. Without the
//! prefixes a shape whose `plan_tier` is `ab` and `invoice_grouping_key` is `c`
//! hashes identically to one whose fields are `a` and `bc` — and a pin that
//! cannot distinguish those is a pin an author can move a character across and
//! still have verify.
//!
//! `sha2` is not used and is not available: dylint DE0708 blocks it. SHA-256
//! comes from the FIPS-validated `aws-lc-rs` provider the platform installs.
//!
//! # The framing primitives are copied from `audit.rs`, deliberately
//!
//! They are eight lines each and they could be shared. They are not, for the
//! reason `audit.rs` gives for not importing the ledger's: these are two
//! **frozen encodings**, and a shared helper puts both behind one edit. A change
//! to `put` that was reasoned about for the audit chain would silently
//! re-key every pending approval in every tenant — the pins would stop matching
//! the content they were taken over and every open unit would answer
//! `APPROVAL_CONTENT_MISMATCH` for a change nobody made.
//!
//! # The pin's domain is `PlanShape`, and that is a boundary rather than a rule
//!
//! [`content_hash`] takes a [`PlanShape`] and hashes everything in it but two
//! fields. What it therefore cannot hash is anything the plan revision row
//! carries that the shape does not, and **that set is not empty** — it is
//! `row_version`, `created_by`, `created_at_utc` and `lifecycle_state` off
//! `pricing_plan`. The rule is not "everything a `PlanShapePatch` can move plus
//! everything the row carries about itself"; it is *the shape*, and the
//! difference has to be argued rather than left to be read off the destructuring
//! guard, which can only speak for the type it destructures.
//!
//! `sku_id` **was** in that difference and is not any more. It is authored — a
//! `PlanShapePatch` moves it on an open draft — so it is now a field of
//! [`PlanShape`], assembled by `infra::publish::assemble` and hashed here. The
//! reason it could not stay an argument is the approve→commit window: a
//! re-derivation that cannot see a rebind verifies a plan selling one SKU
//! against a pin taken over another, with every digest equal.
//!
//! The four that remain outside, each with the reason it cannot move under a
//! pending unit rather than a reason it does not matter:
//!
//! - **`created_by` and `created_at_utc`** are minted with the revision row and
//!   no path updates them. `revision` *is* hashed, so a different provenance is
//!   a different `(plan_id, revision)`, which is a different pin by
//!   construction.
//! - **`lifecycle_state`** leaves `draft` by exactly two paths, and both are
//!   already accounted for: the publish commit, which is the act the approval
//!   authorizes, and `abandon_draft`, which voids every pending unit of the plan
//!   before it commits.
//! - **`row_version`** moves only when content moves, and every authoring
//!   mutation that moves it voids the unit first (`inst-ap-pin`). Hashing it
//!   would add nothing a content field does not already carry, and would make
//!   the pin depend on how many times a draft had been saved.
//!
//! **What keeps *this* boundary honest is not the compiler.** The `PlanShape`
//! destructuring below fails to compile on a field added to *the shape*; it
//! says nothing about a column added to `pricing_plan`. So the obligation is
//! written down instead: a revision column added by a later slice is either
//! carried onto [`PlanShape`] — where the guard picks it up — or argued into the
//! list above. `sqlite_plan_repo.rs`'s per-table copy tests are the same
//! discipline for the same table.
//!
//! That is the **only** residual, and the sentence above is not the whole account
//! of what the destructures miss — see *Every struct is destructured* below, which
//! covers the nested aggregates inside the shape.
//!
//! # Two fields of `PlanShape` are deliberately **not** hashed
//!
//! [`PlanShape::evaluated_at`] and [`PlanShape::baseline`], and the first is not
//! a preference:
//!
//! - **`evaluated_at`** is the instant the validation pipeline is being run at,
//!   handed in by the caller (`plan_shape.rs`'s own module doc: it is a field so
//!   that no rule reads a clock). The approve path re-assembles the shape at a
//!   *different* instant by construction. Hashing it would make every
//!   re-verification fail — `APPROVAL_CONTENT_MISMATCH` on every approve, for
//!   every plan, with nothing having changed — so the pin would be not merely
//!   over-wide but unusable.
//! - **`baseline`** is the plan's *published past*, re-derived on each assembly
//!   from rows this revision does not own. It is not content a reviewer is shown
//!   and not content this revision can author. It also cannot move while a unit
//!   pends: a `submitted` unit holds the plan's scope key through
//!   `PENDING_CHANGE_UNIT_EXISTS`, so no second publish of the same plan can
//!   land and change what the baseline is.
//!
//! Every other field of the shape is hashed, and the **price rows'** own
//! provenance (`created_by`, `created_at_utc`) and `row_version` with them: the
//! rule inside the shape is "everything, minus the two argued above", rather
//! than a hand-picked consumer-visible subset, because a subset is a list
//! somebody has to keep true and the failure mode of getting it wrong is a
//! mutation the guard cannot see. What that rule does **not** reach is the
//! revision row's own four; see the section above.
//!
//! # One field is framed **narrower** than it is stored: the window state
//!
//! [`WindowInterval::state`] has four values and the preimage has two tokens for
//! them — [`PIN_WINDOW_STATE_LIVE`] for `scheduled`, `active` and `expired`, and
//! [`PIN_WINDOW_STATE_CANCELLED`] for `cancelled`. It is the one place this
//! encoder frames something other than the value it was handed, so it is argued
//! here rather than read off [`put_window_interval`].
//!
//! **The three collapsed states are the clock's, and the clock is not an author.**
//! §4's `scheduled → active` (`inst-ws-activate`) and `active → expired`
//! (`inst-ws-expire`) fire on `now` reaching a stored bound; `WindowActivationJob`
//! performs them on a 60-second tick and D-99's paired clarification says in as
//! many words that they are **not publish units** and re-project nothing, *"so
//! the time-driven transitions change nothing projected"*. A digest that moved on
//! one of them would give a clock-driven transition a consequence the design set
//! spent that clause denying it, and the consequence is the worst available:
//! `inst-ws-future-start` (D-63) puts every newly scheduled window's start
//! strictly in the future and `inst-wc-required` refuses a billable row with no
//! window, so **every** pending plan-revision approval has an activation boundary
//! ahead of it. Framing `state` verbatim therefore made an ordinary tick refuse
//! the reviewer's decision with `APPROVAL_CONTENT_MISMATCH` (409) over content
//! nobody had touched — "the attack the pin exists to catch and is not one", with
//! no remedy at all, because the message asks the author to re-submit and time
//! cannot be put back.
//!
//! **`cancelled` stays distinct, and that is the same argument in the other
//! direction.** A cancellation is *mutation*-driven: it is a publish unit under
//! D-99, always-material under D-62, and a change a reviewer is entitled to see
//! before they sign — so it must move the digest. Dropping the token altogether
//! was rejected for that reason: `window_repo::list_for_plan` returns cancelled
//! windows too (`GET …/coverage` renders them deliberately), so a token-free
//! interval frame would make a cancel invisible to the pin while leaving it
//! visible on the wire.
//!
//! What the narrowing costs, stated because it is the residue: two shapes
//! differing **only** in that one key holds `[a, b)` as `scheduled` where the
//! other holds it as `expired` pin identically. That is a pair no clock can
//! produce out of order — a window reaches `expired` only through `active` — and
//! where it does arise it is the same interval at two moments of its own life,
//! which is precisely what must not move the digest.
//!
//! # Collections are hashed in a canonical order, not the order they arrive in
//!
//! `rows`, `phases`, `addon_rules`, `depends_on`, `conflicts_with`, `bands`,
//! `windows` and each key's `intervals` are sorted on their members' own
//! identities before framing. The repositories already return them in exactly
//! these orders (`price_repo::load_for_plan` by `price_id`,
//! `plan_shape_repo::load_phases` by `(ordinal, phase_id)`, `load_addon_rules`
//! by `addon_sku_id`, the band read by `from_qty`, and
//! `window::group_by_key_seeded` by the key's canonical rendering and then by
//! `(effective_from, effective_to, state)`), so this changes no digest today. It is here for the day one of
//! those `ORDER BY`s changes: without it, a query-plan-visible reordering would
//! fail every pending approval in the fleet with `APPROVAL_CONTENT_MISMATCH`,
//! which looks exactly like the attack the pin exists to catch and is not one.
//!
//! What it costs, stated because it is the residue: two shapes differing **only**
//! in the authored order of two phases that share an `ordinal` hash identically,
//! and `PhaseGraph::entry` tie-breaks on that order. A shape with a duplicate
//! ordinal is one `PHASE_CHAIN_NONLINEAR` refuses at publish, so it can never be
//! the subject of a pin — but if that rule is ever relaxed, this is the sentence
//! that has to be revisited.
//!
//! # Every struct is **destructured**, and that is the guard
//!
//! No field of any struct this encoder frames is reached through a dot. Each
//! encoder binds its struct with an exhaustive pattern and no `..`, so a field
//! added to `PlanShape`, `PriceRecord`, `PriceRow`, `PlanPhase`, `AddonRule`,
//! `DescriptorSet`, `TierBand`, `IncludedAllowance`, `KeyWindows`,
//! `WindowInterval`, `OverlayRevision`, `OverlayLine`, `OverlayInterval`,
//! `TargetRef`, `MembershipMoveProposal`,
//! `ThresholdEntry`, `GrantSet`, `EntitlementGrants`, `CompositeMeter`,
//! `PlanChangeContract`, `PeriodFloorCap` or `ProrationContract` **fails to
//! compile here** until somebody decides whether the pin covers it. A test
//! cannot give that guarantee: it can only fail for the fields it already knows
//! about, which is the wrong set by definition.
//!
//! It worked: `PlanShape::windows` could not be added without E0027 here, and
//! the decision it forced is recorded on [`CONTENT_PIN_DOMAIN_SEP`].
//!
//! **The nested types need the gate as much as the outer ones**, and a claim
//! written about the outer types does not reach them. `EntitlementGrants`,
//! `GrantSet`, `CompositeMeter`, `PlanChangeContract`, `PeriodFloorCap` and
//! `ProrationContract` can each be framed field by field with nothing holding the
//! field list to the type's — every field framed, no gate, and the gate is the
//! whole claim. The falsification is one line either way: `pub scratch: bool` on
//! `EntitlementGrants` compiles with **zero** errors from this file unless it is
//! destructured, while the same field on `PlanShape` raises E0027 at
//! `put_plan_shape`. Three of the six carry an inline note naming what an unpinned
//! field would cost — a trial capped at 20 000 instead of 20, a plan changeable to
//! `enterprise` instead of `standard`, a $500 period floor nobody approved — so the
//! arming condition sits beside its own consequence.
//!
//! **Four** types this cannot be done for directly, because their members are
//! private or live on an enum variant, and their constructors are load-bearing.
//! All four are now reached through a `parts()` accessor that binds the type
//! exhaustively, so a new member is a compile error in the owning module *and* —
//! because the parts struct is destructured here in turn — a decision at the pin:
//!
//! - [`ScopeKey`](crate::domain::scope_key::ScopeKey), whose ten axes are
//!   frozen by the canonical key itself and by ten columns of `pricing_price`,
//!   read through
//!   [`ScopeKeyParts`](crate::domain::scope_key::ScopeKeyParts).
//! - [`ThresholdVersion`](crate::domain::materiality::ThresholdVersion), read
//!   through [`ThresholdVersionParts`]. Its fields are private because its
//!   constructor is what refuses an empty or duplicated entry set, and a public
//!   field set would be that refusal bypassed. Reading it through three bare
//!   accessor calls instead would make a fourth field a version-level fact this
//!   encoder silently omits.
//! - [`LineKey`](crate::domain::overlay::LineKey), read through
//!   [`LineKeyParts`](crate::domain::overlay::LineKeyParts). Its three fields are
//!   private because its four constructors are what make two of §6's `CHECK`s
//!   unviolatable — `ThresholdVersion`'s case exactly, one plane over.
//! - [`ScopeSelector`](crate::domain::overlay::ScopeSelector), read through
//!   [`ScopeSelectorParts`](crate::domain::overlay::ScopeSelectorParts). This one
//!   is an **enum**, and its two accessors bind `Scoped` with a rest pattern, so a
//!   third member of that variant reaches neither of them. `parts()` binds both
//!   fields explicitly instead.
//!
//! [`PhaseGraph`](crate::domain::plan_shape::PhaseGraph) is the fifth private
//! type and needs no parts struct: this encoder frames exactly one thing off it,
//! `phases()`, and that accessor now destructures `Self`, so a second field stops
//! it compiling where it is declared.
//!
//! **The four the D-225 overlay pin depends on are the easiest to miss.**
//! Reaching `ScopeSelector` from `put_overlay_revision` through
//! `class()`/`value()`, `OverlayInterval` through `interval.from`/`interval.to`,
//! `TargetRef` through `target_ref.plans` and `LineKey` through its three
//! accessors — none of them in the list above, and none of them gated. The claim
//! at the head of this section was written about the plan pin and read as though
//! it held for every subject; a field added to any of those four compiled with
//! zero errors, dropped out of `overlay_content_hash`, and left a reviewer's
//! approve over one overlay verifying against a different one. Not one `put_*`
//! call changed order, argument or arity in the fix, so the preimage is
//! byte-identical and [`OVERLAY_PIN_DOMAIN_SEP`] does not move — which
//! `the_overlay_encoding_is_frozen` is the proof of rather than the claim.
//!
//! # The one residual, and it is not a hole in the encoding
//!
//! The destructures speak only for the types they destructure. What none of them
//! can see is **a column added to `pricing_plan` that never reaches
//! [`PlanShape`]** — the boundary argued at the head of this file. So the
//! obligation is written down instead: a revision column added by a later slice
//! is either carried onto [`PlanShape`], where the guard picks it up, or argued
//! into that list. `sqlite_plan_repo.rs`'s per-table copy tests are the same
//! discipline for the same table.
//!
//! [`ThresholdEntry`]: crate::domain::materiality::ThresholdEntry
//! [`ThresholdBasis`]: crate::domain::materiality::ThresholdBasis
//! [`ThresholdVersionParts`]: crate::domain::materiality::ThresholdVersionParts

use aws_lc_rs::digest::{SHA256, digest as sha256};

use uuid::Uuid;

use crate::domain::concurrency::RowVersion;
use crate::domain::contracts::{
    AnchorDay, BillingAnchorPolicy, EntitlementGrants, GrantSet, PlanChangeContract,
    ProrationBasis, ProrationContract,
};
use crate::domain::instant::timestamp_micros;
use crate::domain::materiality::{
    ThresholdBasis, ThresholdEntry, ThresholdVersion, ThresholdVersionParts,
};
use crate::domain::membership_change::MembershipMoveSet;
use crate::domain::money::{CurrencyCode, MinorAmount, RateMinor};
use crate::domain::overlay::{
    Adjustment, AmountSet, LineKeyParts, Magnitude, OverlayInterval, OverlayLine, OverlayRevision,
    ScopeSelectorParts, ScopeValue, TargetRef, TargetSku,
};
use crate::domain::plan_shape::{
    AddonRule, BillingCycle, CompositeMeter, DescriptorSet, Frequency, PeriodFloorCap, PlanPhase,
    PlanShape,
};
use crate::domain::price_record::PriceRecord;
use crate::domain::price_row::{
    AggregationFunction, AggregationGranularity, BillingGranularity, IncludedAllowance,
    MinQtyUsageFallback, PriceRow, QuantitySource, ReservationFlavor, TierAggregationWindow,
    TierBand, TierQualificationWindow, model_kind_wire,
};
use crate::domain::scope_key::{Meter, PhaseId, PlanId, ScopeKey, ScopeKeyParts};
use crate::domain::taxonomy::{RegionTaxMarkers, TaxonomyEntry, TaxonomyValueChange};
use crate::domain::window::{KeyWindows, WindowInterval, WindowState};
use time::OffsetDateTime;

/// Versioned domain-separation tag for the approval content pin.
///
/// Disjoint from [`AUDIT_DOMAIN_SEP`](crate::domain::audit::AUDIT_DOMAIN_SEP) on
/// purpose: the two digests live in the same process and, once G6 lands the
/// approval trail, in adjacent columns of one transaction. A shared preimage
/// space is a shared collision space.
///
/// Bumped **only** on an intentional re-freeze of the encoding — which
/// invalidates every open approval unit in every tenant, so it is a migration
/// and not an edit.
///
/// # `v1` → `v2`: [`PlanShape::windows`] joined the preimage, and that is a
/// re-freeze
///
/// The window plane is subject content — a version freezes the intervals (D-99,
/// D-121) and a reviewer approves what will be frozen — so
/// [`put_plan_shape`]'s exhaustive pattern refused to compile until the pin
/// covered it, and covering it changed the encoding. That is exactly the
/// "intentional re-freeze" this constant reserves a bump for, so it is bumped.
///
/// **What the bump does and does not buy, measured rather than asserted.** It is
/// *not* what keeps an old pin from verifying against a new shape: the framing is
/// length-prefixed and every collection's count precedes its elements, so the
/// two preimages cannot be equal. A `rows` frame opens with a 16-byte-long
/// `price_id` and the `windows` frame opens with an 8-byte count, so the extra
/// field cannot be absorbed into a longer row list either. A `v1` pin therefore
/// already fails against a `v2` shape without the bump. What the bump buys is
/// that the freeze generation is **recorded in the preimage** instead of being
/// re-derived by that argument every time somebody asks which encoding a stored
/// digest was taken under.
///
/// **The consequence in a deployed world, stated because it is vacuous here and
/// will not stay vacuous.** Every pending approval unit in existence fails
/// `APPROVAL_CONTENT_MISMATCH` (409) after this change — which looks precisely
/// like the attack the pin exists to catch and is not one. What makes it an edit
/// and not a migration today is that **this gear is not deployed**: there is no
/// durable `pricing_approval` row anywhere holding a `v1` digest, so every pin
/// ever taken under `v1` was taken inside a test process that has exited. That
/// is a narrower argument than the one this paragraph carried before — it used
/// to say "no surface opens one (G5 mounts the route)", which stopped being true
/// when phase 3 mounted `POST /plans/{planId}/publish`: that route opens a
/// pending unit today, and `tests/rest_approvals.rs` drives it. **In a deployed
/// world the same change needs a drain of pending approvals first** — every open
/// unit decided or withdrawn before the encoding moves — because a re-freeze
/// under load is indistinguishable at the surface from content having been moved
/// under the reviewer.
///
/// # `v2` → `v3`: the window state is framed as two tokens, not four
///
/// Same day, one review later, and for the defect the `v2` shape carried: framing
/// [`WindowInterval::state`] verbatim let the `WindowActivationJob`'s 60-second
/// tick void a pending two-person approval, because `inst-ws-future-start` puts
/// an activation boundary ahead of **every** pending unit. The module doc argues
/// the narrowing; this records that it re-freezes the encoding, and that the
/// generation moves with it.
///
/// **Why the generation is recorded rather than argued from, which is the one
/// question the `v1` → `v2` note left open.** The construction that a stale digest
/// cannot verify against a re-derivation under a newer encoding is about
/// *collision*, and it survives this change untouched — every field is
/// `PRESENT|len|bytes` or a bare `ABSENT` and every collection's count precedes
/// its elements, so a preimage determines its own field sequence and two equal
/// preimages frame equal content. But the paragraphs above already say that is
/// **not** what the tag buys: what it buys is that the freeze generation sits *in
/// the preimage*, so "which encoding was this stored digest taken under" is read
/// rather than reconstructed. Two encodings sharing one tag is exactly the state
/// that question cannot be answered in, and it is the state `v1` → `v2` was
/// bumped to leave. The prefix argument also does not reach this change: a
/// narrowed token moves bytes in the **middle** of a preimage, not off its end.
///
/// **And the bump costs nothing here, measured rather than assumed.** The digests
/// it invalidates that the narrowing would not have invalidated anyway are
/// effectively none: `inst-wc-required` refuses a publish whose billable key holds
/// no `scheduled`/`active` window, so a shape that reached `submit` at all carries
/// a non-`cancelled` interval, whose token moves from `scheduled` to `live` under
/// this change with or without a new tag. Keeping `v2` would have preserved no
/// pending approval — it would only have left the generation ambiguous. The
/// **deployed-world** paragraph above applies unchanged and for its own reason:
/// this needs a drain of pending approvals before it ships, and it is an edit
/// today only because nothing durable holds a `v2` digest either.
///
/// # `v3` **stands**: the policy subject did not join this preimage
///
/// G6 gave the approval plane a second pinned subject — a
/// [`ThresholdVersion`](crate::domain::materiality::ThresholdVersion), the
/// always-material unit D-10 opens over a threshold-policy PUT — and the tag did
/// **not** move for it, which is a decision rather than an omission.
///
/// A re-freeze is what this constant reserves a bump for, and there was none: no
/// field of [`PlanShape`] changed, [`put_plan_shape`] frames the same bytes in the
/// same order, and `the_encoding_is_frozen`'s golden vector is unmoved. The policy
/// subject is a **separate preimage** under a separate tag
/// ([`THRESHOLD_PIN_DOMAIN_SEP`]) reached through a separate function, because the
/// module doc's rule about two frozen encodings not going behind one helper is the
/// same rule about two frozen encodings not going behind one tag: bumping this to
/// `v4` would have invalidated every pending **plan** approval to record the
/// arrival of a subject none of them are about.
///
/// What the separate tag buys is the property this constant exists for, applied
/// across kinds rather than across generations. `pricing_approval` holds one
/// `content_hash` column for both subjects and
/// `approval_repo::find_approved_for_content` matches on `(subject_ref,
/// content_hash)` — so two digests over different kinds of content share a column,
/// and disjoint domains are what make them unable to be read for each other. The
/// `subject_ref` would have caught it anyway; the tag means it does not have to.
///
/// # `v4`: D-196's usage line joined the key's framing
///
/// This **is** the re-freeze the counter is for, and unlike the `v3` case it is not
/// a narrowing of one token: [`put_scope_key`] gained two framed fields, so every
/// preimage containing any scope key — which is every plan shape carrying a row or a
/// window group — produces different bytes. A `v3` digest and a `v4` digest over the
/// same content differ, and the bump is what makes that legible instead of silent.
///
/// The **deployed-world** paragraph applies verbatim, for the reason it applied at
/// `v2 -> v3`: this needs pending approvals drained before it ships, and it is an
/// edit today only because nothing durable holds a `v3` digest either.
///
/// [`THRESHOLD_PIN_DOMAIN_SEP`] does **not** move with it. A
/// [`ThresholdVersion`](crate::domain::materiality::ThresholdVersion) contains no
/// scope key, so its encoding did not change, and moving it would invalidate every
/// pending policy unit to record a re-freeze of content those units are not about —
/// which is the argument for two counters, applied in the other direction from the
/// `v3` note above.
/// # `v6`: Slice 6's proration contract joined the row's framing
///
/// The `v4` case again, and for the same reason it was a bump rather than a
/// standing tag: [`put_price_record`] gained a framed field, so every preimage
/// containing a price row produces different bytes. The contract is **authored
/// draft content** — a `PATCH` on an open draft moves it — which is precisely
/// `tax_category_ref`'s argument for being in the pin at all: a reviewer who
/// approved a plan anchoring on the 1st and a commit that publishes one
/// anchoring on signup day, with every digest equal, is the re-verification hole
/// this preimage exists to close. It is also the field a downgrade's credit is
/// computed from (`inst-pi-credit-source`), so the money consequence of the hole
/// is direct.
///
/// The **deployed-world** paragraph applies verbatim once more: this needs
/// pending approvals drained before it ships, and it is an edit today only
/// because nothing durable holds a `v5` digest either.
/// # `v7`: Slice 6's plan-change contract joined the plan's framing
///
/// The third re-freeze of this counter and the same case as `v4` and `v6`:
/// [`put_plan_shape`] gained framed fields, so **every** preimage produces
/// different bytes, not only those carrying a price row. The contract is
/// authored content a `PATCH` moves, and unlike the two before it the hole it
/// closes is an **authorization** one rather than a billing one — an edge list
/// decides who may move where, so a reviewer who approved a plan changeable only
/// to `standard` and a commit publishing one changeable to `enterprise`, with
/// every digest equal, is the same re-verification hole with a different
/// consequence.
///
/// **`v6` and `v7` collapse for anyone deploying**, because nothing durable held
/// a `v6` digest either: both landed in the same unshipped series. They are two
/// bumps rather than one because each records a distinct re-freeze, and a
/// counter that skipped one would leave a later reader unable to tell which
/// change moved the bytes.
/// # `v8`: the entitlement grant set joined the same framing
///
/// `v7`'s case again, one field over, and the consequence is a third kind:
/// `v4` closed a billing hole, `v6` a money one, `v7` an authorization one, and
/// this closes an **entitlement** one — a reviewer who approved a trial capped
/// at 20 cloudlets and a commit publishing one capped at 20 000.
///
/// **`v6`, `v7` and `v8` all collapse for anyone deploying**: nothing durable
/// held any of them, and all three landed in one unshipped series. They are
/// three bumps rather than one because each records a distinct re-freeze, and a
/// counter that skipped any would leave a later reader unable to tell which
/// change moved the bytes.
/// # `v9`: Slice 10's reservation pair joined the row's framing
///
/// `v4`'s and `v6`'s case once more: [`put_price_row`] gained two framed fields,
/// so every preimage containing a price row produces different bytes. The pair
/// is authored draft content a `PATCH` moves, and the hole it closes is a
/// **money** one of the most direct kind available on this table — a reviewer
/// who approved a reserved rate of 1 000 minor units and a commit that publishes
/// 100, with every digest equal. `reservation_flavor` travels with it because it
/// decides whether the reserved quantity leaves the on-demand tier counter at
/// all (`inst-rv-tier-q`) or never enters it (`inst-rv-level`, D-139), so
/// flipping it moves the charge without moving the rate.
///
/// **`v6` through `v9` all collapse for anyone deploying**: nothing durable held
/// any of them, and all four landed in one unshipped series. They are four bumps
/// rather than one for the reason `v8` states — each records a distinct
/// re-freeze, and a counter that skipped any would leave a later reader unable
/// to tell which change moved the bytes.
/// # `v10`: Slice 10's typed floors and discount hook joined it
///
/// `v9`'s case, four fields over and in one group with it. Each closes a
/// distinct hole: `min_qty_purchase` decides who may buy the plan at all,
/// `min_qty_usage` decides what quantity is billable, `min_qty_usage_fallback`
/// decides whether below-floor usage becomes a rating exception or nothing, and
/// `discount_ref` names the instrument that discounts the line. All four are
/// authored draft content a `PATCH` moves, so all four were mutable between
/// submit and approve with every digest equal.
///
/// **`v6` through `v10` all collapse for anyone deploying**: nothing durable
/// held any of them, and all five landed in one unshipped series. They are five
/// bumps rather than one for `v8`'s reason -- each records a distinct re-freeze,
/// and a counter that skipped any would leave a later reader unable to tell
/// which change moved the bytes.
/// # `v11`: Slice 10's composite definitions joined the plan shape
///
/// `inst-cm-frozen` freezes the definition into the snapshot, so it is authored
/// draft content a `PATCH` moves and it enters this preimage. The hole it closes
/// is the formula's: a reviewer who approved `vCPU + RAM` weighted 1:1 and a
/// commit that publishes 1:4, with every digest equal. `v6` through `v11` all
/// collapse for anyone deploying — nothing durable held any of them.
///
/// # `v12`: a rate stopped being an amount (D-311)
///
/// The band rate and the `per_unit` price left [`MinorAmount`] for
/// [`RateMinor`], which counts 10⁻⁹ minor units — so one commercial price frames
/// to a different integer — and `unit_rate` is a field of the preimage that did
/// not exist before. Either alone would require the bump.
///
/// **This one is not free the way `v6`–`v11` were.** Those collapse for anyone
/// deploying because nothing durable ever held them; this invalidates any
/// pending unit that does exist. Taken deliberately, and cheap only because
/// nothing has been published on any stand — the two-person rule has held every
/// publish, so what exists is drafts. It is also the honest direction: a
/// reviewer who approved a ladder truncated to `0.01` at every band approved a
/// document that misstated the price.
/// # `v13`: the plan gained a name (D-318)
///
/// `pricing_plan.plan_name` is authored draft content a `PATCH` moves, so it is
/// `v4`'s case exactly: unframed, a reviewer could approve "Business Starter"
/// and the commit publish "Enterprise Unlimited" with every digest equal. It is
/// not a money field, and it does not need to be — the name is what a consumer
/// surface shows the plan as, so a swap between submit and approve changes what
/// a buyer is told they are buying.
///
/// **Not free.** Like `v12`, this invalidates any pending unit that exists;
/// unlike `v6`–`v11`, nothing about it collapses. Cheap only because what exists
/// on any stand is drafts and pending units, the two-person rule having held
/// every publish.
///
/// # `v14`: the plan-level period floor and cap joined the plan shape (D-319)
///
/// PRD §17.8's catalog authoring field landed as a revision-scoped child set,
/// and it is authored draft content a `PATCH` moves — `v11`'s case for the
/// composite definitions, with a money consequence. The hole it closes: a
/// reviewer who approved a plan carrying no minimum and a commit that publishes
/// one at $500 a period, with every digest equal. It is worse than the
/// composite formula's, because a floor changes what a subscriber pays without
/// changing any line of the invoice that would explain it.
///
/// Like `v12` and `v13`, and unlike `v6`–`v11`, this invalidates any pending
/// unit that exists rather than collapsing for free.
///
/// **A separate bump rather than a wider `v13`.** This field and `v13`'s
/// `plan_name` were authored concurrently and both first framed against `v12`.
/// They are not one version: the separator's whole job is that one tag means one
/// framing, so two different covered-content shapes sharing `v13` would be the
/// case this constant's own doc argues against — a digest that cannot say which
/// encoding produced it. `v13` is what the name was frozen under and stays that;
/// the bound arrives as `v14`.
///
/// # `v15`: the reserved rate is framed as a rate (D-311)
///
/// `reserved_rate_minor` became `reserved_rate_nano`, so [`put_price_row`] frames
/// a nano-minor rate where it framed whole minor units. `PRESENT|len|bytes` makes
/// the two preimages unequal by construction and the bump records **which**
/// framing a stored digest was taken under, which is the argument every entry
/// above makes for not leaving it to be re-derived.
///
/// This list is hand-maintained and the constant is not, so the constant can move
/// without it — the same class the *Every struct is destructured* section below
/// records, landing on the one list in this file the compiler cannot derive.
///
/// # What did **not** move it: the Z3-6 destructures
///
/// Six nested aggregates stopped being reached by dotted field access and
/// started being destructured, and `ThresholdVersion` and `PhaseGraph` gained
/// `parts()`-shaped accessors. Not one `put_*` call changed order, argument or
/// arity, so the preimage is byte-identical and the generation stays `v15`:
/// `the_encoding_is_frozen` and `the_threshold_encoding_is_frozen` pass on their
/// existing golden vectors, which is the proof rather than the claim. Recorded
/// because "a change to the encoder" and "a change to the encoding" are the two
/// things this constant exists to keep apart.
///
/// # `v16`: a phase frames its label, and its middle kind is `interim` (D-357, D-358)
///
/// [`put_plan_phase`] frames [`PlanPhase::display_name`] (a reviewer is shown the
/// label they approve), and `PhaseKind::as_str` renders `interim` where it
/// rendered `intro`. Either alone changes stored digests; the bump records which
/// framing a digest was taken under. Every unit pending at rollout answers
/// `APPROVAL_CONTENT_MISMATCH` and is re-submitted — D-358's runbook drains them
/// first.
pub const CONTENT_PIN_DOMAIN_SEP: &[u8] = b"VHP-BSS-PRICING-APPROVAL-PIN-v16\x1f";

/// Versioned domain-separation tag for the **threshold-policy** content pin.
///
/// Its own generation counter, starting at `v1`, and deliberately not
/// [`CONTENT_PIN_DOMAIN_SEP`]'s: the two encodings freeze different content and
/// are re-frozen by different changes, so one counter would make every bump of
/// either invalidate the pending units of both. See the note on that constant for
/// why the arrival of this one did not move it.
pub const THRESHOLD_PIN_DOMAIN_SEP: &[u8] = b"VHP-BSS-PRICING-THRESHOLD-PIN-v1\x1f";

/// The domain separator of the **overlay** pin (D-225).
///
/// Its own generation counter, and its own domain, for [`THRESHOLD_PIN_DOMAIN_SEP`]'s
/// reason: three subject kinds share one `content_hash` column, and a separator per
/// kind is what makes a digest taken over an overlay unable to verify against a plan
/// shape or a policy version that happened to frame to the same bytes. `v1` because
/// nothing has been pinned under it yet — **and it moves only with a drain**, exactly
/// as the other two do: bumping it invalidates every pending overlay unit in every
/// tenant, which is a decision and not a repair.
pub const OVERLAY_PIN_DOMAIN_SEP: &[u8] = b"VHP-BSS-PRICING-OVERLAY-PIN-v1\x1f";

/// The domain separator for a **bundle composition** unit's pin (D-104).
///
/// Its own, for [`OVERLAY_PIN_DOMAIN_SEP`]'s reason: a composition unit's subject
/// is the plan shape **and** the composition, and a digest that could collide with
/// a plain plan-revision pin would let an approve taken for a publish authorize a
/// component swap.
pub const BUNDLE_PIN_DOMAIN_SEP: &[u8] = b"VHP-BSS-PRICING-BUNDLE-PIN-v1\x1f";

/// The domain separator of the **membership move** pin (`inst-mm-pending`).
///
/// Its own, for [`OVERLAY_PIN_DOMAIN_SEP`]'s reason: a digest that could
/// collide with a plan, overlay, threshold or bundle pin would let an approve
/// taken for one kind of unit authorize a decision about another. `v1`
/// because nothing has been pinned under it yet.
pub const MEMBERSHIP_PIN_DOMAIN_SEP: &[u8] = b"VHP-BSS-PRICING-MEMBERSHIP-PIN-v1\x1f";

/// The taxonomy-value edit's own separator (D-353), disjoint from every other
/// pin's for the reason each of them is: two encodings freezing different
/// content must not share a preimage.
pub const TAXONOMY_VALUE_PIN_DOMAIN_SEP: &[u8] = b"VHP-BSS-PRICING-TAXONOMY-VALUE-PIN-v1\x1f";

/// The token the preimage frames an absolute threshold basis as.
///
/// Written out here rather than taken from any wire or column spelling, for
/// [`PIN_WINDOW_STATE_LIVE`]'s reason: a frozen encoding must not move when a
/// persisted token is renamed. The two bases are framed as a **tag plus a value**
/// rather than as two nullable fields, so that a `1000` absolute and a `1000`
/// basis-point entry on one currency cannot frame alike.
const PIN_THRESHOLD_BASIS_ABSOLUTE: &str = "absolute_minor";

/// The token the preimage frames a basis-point threshold as.
const PIN_THRESHOLD_BASIS_PERCENT: &str = "percent_bp";

/// The token the preimage frames a `scheduled`, `active` or `expired` window
/// state as — see the module doc's section on the narrowing.
///
/// A literal of this module's own and deliberately **not**
/// [`WindowState::as_str`](crate::domain::window::WindowState::as_str)'s output:
/// those four tokens are pinned to `chk_pricing_price_window_state` and to the
/// wire, and a frozen encoding must not move when a persisted token is renamed.
/// `live` is none of the four, so the two vocabularies cannot be read for each
/// other either.
const PIN_WINDOW_STATE_LIVE: &str = "live";

/// The token the preimage frames a `cancelled` window state as.
///
/// It spells the word the wire spells, and that is a coincidence rather than a
/// dependency: it is written out here for [`PIN_WINDOW_STATE_LIVE`]'s reason, so
/// that a rename on the persisted side re-keys no pin.
const PIN_WINDOW_STATE_CANCELLED: &str = "cancelled";

/// NULL-safe presence marker for an absent field: a bare byte.
const ABSENT: u8 = 0x00;

/// NULL-safe presence marker for a present field: `PRESENT` + u32-BE len + bytes.
const PRESENT: u8 = 0x01;

/// `SHA-256(domain_sep || the canonical framing of everything a reviewer is
/// shown)`.
///
/// Total: there is no shape this cannot hash, which is why it returns the digest
/// rather than a `Result`. See the module doc for the two fields it does not
/// cover and why.
#[must_use]
pub fn content_hash(shape: &PlanShape) -> [u8; 32] {
    let mut buf = Vec::with_capacity(1024);
    buf.extend_from_slice(CONTENT_PIN_DOMAIN_SEP);
    put_plan_shape(&mut buf, shape);
    digest32(&buf)
}

/// The pin a **mass-repricing run's** batch approval unit is opened under
/// (`inst-bs-approval`, `inst-ap-pin`) — and re-derived under when that unit is
/// decided.
///
/// `SHA-256("bss-pricing/repricing-run/v1/" || the run's frozen report ||
/// its own operation id)`.
///
/// # Not framed like its five neighbours, and that is the existing argument
/// rather than an exemption taken here
///
/// Every other function in this module frames its subject field by field,
/// because it hashes a **caller-authored** shape a submitter could otherwise
/// steer into a collision. A run's `report` is not that: it is this crate's own
/// controlled rendering (`api::rest::repricing_runs::frozen_report`), built once
/// from the *parsed* domain values rather than echoed from the request, so
/// hashing its serialization is total and needs no second encoder. That
/// reasoning was written where this function used to live and travels with it
/// unchanged.
///
/// `operation_id` and not the caller's `run_id`: it is the value the unit's own
/// `subject_ref` carries, so the pin and the subject name the run by one id and
/// not by two.
///
/// # The preimage is preserved byte for byte, deliberately
///
/// This function **moved** here from `api::rest::repricing_runs::repricing_pin`
/// so that `infra::approval::re_derive` could reach it — a decide surface may
/// not name an `api` item, which is `DE0202`'s rule and D-270's recorded
/// encounter with it. Re-framing it on the way would have changed every digest,
/// and every unit opened before the re-derivation arm existed would have become
/// `APPROVAL_CONTENT_MISMATCH` instead of decidable. Those units are exactly the
/// population this move exists to unstrand, so the bytes do not move. It carries
/// no `*_PIN_DOMAIN_SEP` constant for the same reason: its separator is the
/// literal below, spelled as it has always been.
///
/// # What it does **not** yet pin
///
/// `inst-bk-approval-subset`'s **per-row** content hash, which is what would let
/// a committed subset shrink by row rather than only by whole plan (D-134). The
/// report names the selector, the adjustment, the changeover and the selected
/// count — so a run whose selector matched a different row set would pin
/// differently only if the *count* moved. Named here rather than left to be
/// inferred from an unframed digest.
#[must_use]
pub fn repricing_run_content_hash(report: &serde_json::Value, operation_id: Uuid) -> [u8; 32] {
    let mut buf = Vec::with_capacity(256);
    buf.extend_from_slice(b"bss-pricing/repricing-run/v1/");
    buf.extend_from_slice(report.to_string().as_bytes());
    buf.extend_from_slice(operation_id.as_bytes());
    digest32(&buf)
}

/// `SHA-256(threshold_domain_sep || the canonical framing of one policy version)`.
///
/// The pin of the D-10 unit, and total for the same reason [`content_hash`] is:
/// there is no version this cannot hash.
///
/// **Every field of the version is covered, and the two beyond the entries are the
/// point.** `version` is what `infra::threshold` resolves the effective policy
/// through, so a pin that omitted it would verify a reviewer's signature over
/// entries against a different row set of the same shape; `effective_from` is
/// authored, so a pin that omitted it would let the proposer move when approved
/// thresholds start applying after the reviewer had signed. There is no analogue
/// here of [`PlanShape`]'s two unhashed fields — nothing on this subject is
/// re-derived at a different instant, and nothing on it is the subject's past.
#[must_use]
pub fn threshold_content_hash(version: &ThresholdVersion) -> [u8; 32] {
    let mut buf = Vec::with_capacity(256);
    buf.extend_from_slice(THRESHOLD_PIN_DOMAIN_SEP);
    put_threshold_version(&mut buf, version);
    digest32(&buf)
}

// ---------------------------------------------------------------------------
// The encoders. Each destructures; see the module doc.
// ---------------------------------------------------------------------------

/// `SHA-256(overlay_domain_sep || the canonical framing of one overlay revision)`.
///
/// The pin of D-225's overlay approval unit, and **total** for [`content_hash`]'s
/// reason: there is no revision this cannot hash, so a unit can always be opened and
/// a reviewer is never shown content the pin did not cover.
///
/// `content_pin_tests::every_field_of_an_overlay_revision_moves_the_pin` ranges a
/// mutator over every field of every struct framed below and asserts not only that
/// each moves the digest but that **no two land on the same one** — which is the
/// half a framing bug fails, because an unframed encoding does not leave a digest
/// unchanged, it makes two different overlays agree.
#[must_use]
pub fn overlay_content_hash(revision: &OverlayRevision) -> [u8; 32] {
    let mut buf = Vec::with_capacity(512);
    buf.extend_from_slice(OVERLAY_PIN_DOMAIN_SEP);
    put_overlay_revision(&mut buf, revision);
    digest32(&buf)
}

/// The pin a bundle composition unit is opened under (D-104).
///
/// **The plan shape alone is not the composition's content, and assuming it was
/// shipped an approval bypass.** The first version of D-104's unit pinned
/// `content_hash(&shape)` on the reasoning that a composition normalizes onto its
/// absorber inside the plan. That is true at *publish*, not at submit — and D-104
/// exists precisely because a `sum_of_parts` recomposition carries **no price-row
/// delta at all**, so a component swap moved nothing the plan-shape digest covers.
/// An approve taken over one component set then authorized a different one:
/// `an_approved_composition_does_not_authorize_a_different_one` drove it end to
/// end and watched the edited composition publish on the old approval.
///
/// # Why the revision's `row_version` and not the composition itself
///
/// The composition lives in `infra`'s `CompositionDraft`, and dylint DE0301 keeps
/// this layer from naming it. What is available here is the fact that settles the
/// question anyway: **a composition edit advances the plan revision's
/// `row_version`** — `PATCH …/bundles/{id}` takes the revision's entity tag and
/// `a_composition_is_written_under_the_revisions_tag` asserts the tag moves — so a
/// digest folding it answers "the subject moved since the approval" for every edit
/// to the composition, including the ones that move no price row at all.
///
/// This is the one unit that hashes `row_version`, and the exception is argued
/// rather than assumed. [`content_hash`]'s own doc excludes it for plan units on
/// the ground that "every authoring mutation that moves it voids the unit first
/// (`inst-ap-pin`)" — and `a_composition_edit_voids_the_pending_unit_over_its_plan`
/// shows that holding for **pending** units. It does not hold here, because the
/// hazard is an **approved** unit and voiding reaches only `submitted` ones.
#[must_use]
pub fn bundle_content_hash(shape: &PlanShape, revision_version: RowVersion) -> [u8; 32] {
    let mut buf = Vec::with_capacity(64);
    buf.extend_from_slice(BUNDLE_PIN_DOMAIN_SEP);
    // The shape by its digest rather than inlined: that encoding is
    // `put_plan_shape`'s to own, and a second copy of it here would be two
    // framings of one document, free to disagree.
    put(&mut buf, &content_hash(shape));
    put_u64(&mut buf, revision_version.get());
    digest32(&buf)
}

/// The pin a material membership-move unit is opened under (`inst-mm-pending`).
///
/// **Total**, for [`content_hash`]'s reason: there is no non-empty set this
/// cannot hash, so a unit can always be opened and a reviewer is never shown
/// content the pin did not cover. [`MembershipMoveSet::new`] is what refuses
/// the empty and duplicate-payer cases before one of these is ever built.
///
/// Proposals are framed in the set's own canonical (payer-sorted) order — see
/// [`MembershipMoveSet`]'s doc — so two requests naming the same payers in a
/// different authored order pin identically.
#[must_use]
pub fn membership_content_hash(set: &MembershipMoveSet) -> [u8; 32] {
    let mut buf = Vec::with_capacity(256);
    buf.extend_from_slice(MEMBERSHIP_PIN_DOMAIN_SEP);
    let proposals = set.proposals();
    put_u64(&mut buf, count_of(proposals.len()));
    for proposal in proposals {
        // Destructured with no rest pattern, so a field added to
        // `MembershipMoveProposal` fails to compile here until it is decided
        // whether the pin covers it — `content_pin`'s own module-doc guard,
        // applied to this type.
        let crate::domain::membership_change::MembershipMoveProposal {
            payer_tenant_id,
            group_value,
            effective_from,
        } = proposal;
        put_uuid(&mut buf, *payer_tenant_id);
        put_str(&mut buf, group_value);
        put_instant(&mut buf, *effective_from);
    }
    digest32(&buf)
}

/// The pin over a proposed edit of one taxonomy value (D-353): the class, the
/// value, the value **as held** when the edit was authored, and the value as it
/// would stand — every field the per-value `GET` renders, for both.
///
/// The held half is what makes this a TOCTOU guard rather than a description of
/// the request: a relabel or a retirement landing under a submitted unit changes
/// the re-derived digest, and the approve is refused (`inst-ap-pin`).
#[must_use]
pub fn taxonomy_value_content_hash(change: &TaxonomyValueChange) -> [u8; 32] {
    let mut buf = Vec::with_capacity(256);
    buf.extend_from_slice(TAXONOMY_VALUE_PIN_DOMAIN_SEP);
    put_str(&mut buf, change.proposal.class.path_segment());
    put_str(&mut buf, change.proposal.value.as_str());
    put_taxonomy_entry(&mut buf, &change.held);
    put_taxonomy_entry(&mut buf, &change.next());
    digest32(&buf)
}

fn put_taxonomy_entry(buf: &mut Vec<u8>, entry: &TaxonomyEntry) {
    // Destructured with no rest pattern, so a field added to `TaxonomyEntry` is a
    // compile error here rather than content that silently stops being pinned.
    let TaxonomyEntry {
        value,
        display_name,
        state,
        tax,
    } = entry;
    put_str(buf, value.as_str());
    put_str(buf, display_name);
    put_str(buf, state.as_str());
    match tax {
        None => put_bool(buf, false),
        Some(RegionTaxMarkers {
            tax_category,
            tax_rate_present,
        }) => {
            put_bool(buf, true);
            put_opt_str(buf, tax_category.as_deref());
            put_bool(buf, *tax_rate_present);
        }
    }
}

fn put_overlay_revision(buf: &mut Vec<u8>, revision: &OverlayRevision) {
    // Destructured with no rest pattern, so a field added to `OverlayRevision`
    // is a compile error here rather than content that silently stops being pinned.
    let OverlayRevision {
        price_overlay_id,
        revision: number,
        lifecycle_state,
        scope,
        precedence,
        interval,
        tax_basis,
        disclosure,
        target_ref,
        lines,
    } = revision;

    // The selector through `parts()` rather than through `class()` / `value()`:
    // those two bind `Scoped` with a rest pattern, so a third member of that
    // variant reaches neither and drops out of the pin with zero errors here.
    let ScopeSelectorParts {
        class: scope_class,
        value: scope_value,
    } = scope.parts();
    // Destructured with no rest pattern, for `OverlayRevision`'s reason: a field
    // added to `OverlayInterval` or `TargetRef` is a compile error here rather
    // than content that silently stops being pinned. Both are `pub`-fielded, so
    // dotted access would compile straight through either of them.
    let OverlayInterval {
        from: interval_from,
        to: interval_to,
    } = interval;
    let TargetRef {
        plans: target_plans,
    } = target_ref;

    put_uuid(buf, *price_overlay_id);
    put_u64(buf, *number);
    put_str(buf, lifecycle_state.as_str());
    put_str(buf, scope_class.as_str());
    put_opt_str(buf, scope_value.map(ScopeValue::as_str));
    put_i64(buf, i64::from(*precedence));
    put_opt_instant(buf, *interval_from);
    put_opt_instant(buf, *interval_to);
    put_str(buf, tax_basis.as_str());
    put_str(buf, disclosure.as_str());

    // **A set, not a list.** `target_ref.plans` arrives in whatever order the row's
    // jsonb held, and the order is not content.
    let mut plans: Vec<Uuid> = target_plans.iter().map(|plan| plan.get()).collect();
    plans.sort_unstable();
    put_u64(buf, count_of(plans.len()));
    for plan in plans {
        put_uuid(buf, plan);
    }

    // The line set is a set too, ordered here by the one field that is unique within
    // a revision. `read_lines` happens to order by the same column today, and that is
    // exactly why the pin must not depend on it: a repository that changed its
    // `ORDER BY` would otherwise invalidate every pending unit without touching an
    // overlay.
    let mut ordered: Vec<&OverlayLine> = lines.iter().collect();
    ordered.sort_unstable_by_key(|line| line.line_id);
    put_u64(buf, count_of(ordered.len()));
    for line in ordered {
        put_overlay_line(buf, line);
    }
}

fn put_overlay_line(buf: &mut Vec<u8>, line: &OverlayLine) {
    let OverlayLine {
        line_id,
        key,
        adjustment,
    } = line;
    // `parts()` rather than the three accessors: `LineKey`'s fields are private,
    // so three accessor calls are three things a fourth field is silently absent
    // from -- `ThresholdVersion`'s case exactly, arriving one type over.
    let LineKeyParts {
        plan_id,
        target_sku,
        cohort,
    } = key.parts();
    put_uuid(buf, *line_id);
    put_opt_uuid(buf, plan_id.map(PlanId::get));
    put_opt_str(buf, target_sku.map(TargetSku::as_str));
    put_opt_instant(buf, cohort);
    put_str(buf, adjustment.kind().as_str());
    match adjustment {
        Adjustment::Markup(magnitude) | Adjustment::Discount(magnitude) => {
            put_magnitude(buf, magnitude);
        }
        // `fixed` carries an amount set and no magnitude — the two arms frame
        // different shapes, and `adjustment.as_str()` above is what tells them apart
        // in the preimage rather than the shape of what follows.
        Adjustment::Fixed(amounts) => put_amount_set(buf, amounts),
    }
}

fn put_magnitude(buf: &mut Vec<u8>, magnitude: &Magnitude) {
    match magnitude {
        // The discriminator is framed, so `percent_bp = 1200` and an amount of 1200
        // minor units cannot frame alike — D-08's whole point, in the preimage.
        Magnitude::PercentBp(bp) => {
            put_str(buf, "percent_bp");
            put_i64(buf, *bp);
        }
        Magnitude::Amount(amounts) => {
            put_str(buf, "amount");
            put_amount_set(buf, amounts);
        }
    }
}

fn put_amount_set(buf: &mut Vec<u8>, amounts: &AmountSet) {
    // `AmountSet` is a `BTreeMap`, so its iteration order is already the currency
    // order; the count is framed anyway, because a count is what stops two adjacent
    // collections from being re-split.
    let entries: Vec<(&CurrencyCode, i64)> = amounts.iter().collect();
    put_u64(buf, count_of(entries.len()));
    for (currency, minor) in entries {
        put_str(buf, currency.as_str());
        put_i64(buf, minor);
    }
}

/// One grant set: the flags, then the quotas, each behind its count.
fn put_grant_set(buf: &mut Vec<u8>, set: &GrantSet) {
    let GrantSet {
        feature_flags,
        quotas,
    } = set;
    put_u64(buf, count_of(feature_flags.len()));
    for (key, on) in feature_flags {
        put_str(buf, key);
        put_bool(buf, *on);
    }
    put_u64(buf, count_of(quotas.len()));
    for (key, value) in quotas {
        put_str(buf, key);
        put_i64(buf, *value);
    }
}

/// Slice 6's entitlement grant set: the tier it resolved from, the plan-level
/// set, then the per-phase sets behind their count.
///
/// **In the pin because a `PATCH` moves it and because it decides what a
/// subscriber may use** — a reviewer who approved a trial capped at 20 cloudlets
/// and a commit that publishes one capped at 20 000, with every digest equal, is
/// `sku_id`'s re-verification hole with an entitlement consequence.
///
/// Every map is framed with its **count** first, per [`put_descriptor_set`]'s
/// rule: without one, two adjacent collections can be re-split, and a plan with
/// two feature flags and no quota would pin identically to one with one of each.
/// `BTreeMap` is what makes the order deterministic across replicas.
fn put_entitlement_grants(buf: &mut Vec<u8>, grants: &EntitlementGrants) {
    let EntitlementGrants {
        plan_tier_ref,
        plan_level,
        per_phase,
    } = grants;
    put_opt_str(buf, plan_tier_ref.as_deref());
    put_grant_set(buf, plan_level);
    put_u64(buf, count_of(per_phase.len()));
    for (phase_id, set) in per_phase {
        put_uuid(buf, *phase_id);
        put_grant_set(buf, set);
    }
}

/// One composite meter definition (`inst-cm-frozen`, D-256).
fn put_composite_meter(buf: &mut Vec<u8>, composite: &CompositeMeter) {
    let CompositeMeter {
        composite_id,
        output_unit,
        constituent_units,
        formula,
    } = composite;
    put_uuid(buf, *composite_id);
    put_str(buf, output_unit);
    put_u64(buf, count_of(constituent_units.len()));
    for unit in constituent_units {
        put_str(buf, unit);
    }
    // The formula is framed as its **canonical JSON text** — this crate never
    // reads inside it (A4), and a structural walk would be a second spelling of a
    // value the store already holds as one string.
    put_str(buf, &formula.to_string());
}

/// Slice 6's plan-change contract.
///
/// **In the pin because a `PATCH` moves it**, and because an edge list is what
/// decides who may move where — a reviewer who approved a plan changeable only
/// to `standard` and a commit that publishes one changeable to `enterprise`,
/// with every digest equal, is the re-verification hole `sku_id`'s own note
/// describes, with an authorization consequence rather than a billing one.
///
/// Framed **unconditionally and member by member**, per [`put_scope_key`]'s rule
/// about ambiguous runs.
fn put_plan_change_contract(buf: &mut Vec<u8>, contract: &PlanChangeContract) {
    let PlanChangeContract {
        allowed_change_targets,
        comparability_rank,
        usage_counter_on_plan_change,
    } = contract;
    // The edge list's `None` and its empty vector are framed **differently** — a
    // leading `bool` then the count — because `inst-pc-failsafe` gives them
    // different meanings and a preimage that collapsed them would let a plan
    // leave self-service change without moving its digest.
    match allowed_change_targets {
        None => put_bool(buf, false),
        Some(targets) => {
            put_bool(buf, true);
            let mut ordered: Vec<&uuid::Uuid> = targets.iter().collect();
            ordered.sort_unstable();
            put_u64(buf, count_of(ordered.len()));
            for target in ordered {
                put_uuid(buf, *target);
            }
        }
    }
    put_opt_u64(
        buf,
        comparability_rank.map(|rank| rank.cast_unsigned().into()),
    );
    put_str(buf, usage_counter_on_plan_change.as_str());
}

/// One market's period floor/cap bound (D-319).
fn put_period_floor_cap(buf: &mut Vec<u8>, bound: &PeriodFloorCap) {
    let PeriodFloorCap {
        currency,
        region,
        floor_minor,
        cap_minor,
    } = bound;
    put_str(buf, currency.as_str());
    put_str(buf, region.as_str());
    put_opt_i64(buf, floor_minor.map(MinorAmount::get));
    put_opt_i64(buf, cap_minor.map(MinorAmount::get));
}

fn put_plan_shape(buf: &mut Vec<u8>, shape: &PlanShape) {
    let PlanShape {
        plan_id,
        revision,
        sku_id,
        billing_cycle,
        frequency,
        plan_tier,
        plan_name,
        plan_tier_override,
        available_from,
        available_to,
        purchase_min_qty,
        purchase_max_qty,
        invoice_grouping_key,
        phases,
        addon_rules,
        descriptor_set,
        period_floor_caps,
        rows,
        entitlement_grants,
        composites,
        change_contract,
        windows,
        // Not hashed; the module doc argues both, and the first one is the
        // difference between a pin that can ever verify and one that cannot.
        baseline: _,
        evaluated_at: _,
    } = shape;

    put_uuid(buf, plan_id.get());
    put_u64(buf, *revision);
    put_opt_uuid(buf, *sku_id);
    put_opt_str(buf, billing_cycle.map(BillingCycle::as_str));
    put_frequency(buf, *frequency);
    put_opt_str(buf, plan_tier.as_deref());
    put_opt_str(buf, plan_name.as_deref());
    put_bool(buf, *plan_tier_override);
    put_opt_instant(buf, *available_from);
    put_opt_instant(buf, *available_to);
    put_opt_u64(buf, *purchase_min_qty);
    put_opt_u64(buf, *purchase_max_qty);
    put_opt_str(buf, invoice_grouping_key.as_deref());

    put_entitlement_grants(buf, entitlement_grants);

    // Slice 10's composite definitions (`inst-cm-frozen`, D-256). Length-framed
    // like every other collection here, for the reason stated above: without the
    // count two adjacent collections can be re-split.
    put_u64(buf, count_of(composites.len()));
    for composite in composites {
        put_composite_meter(buf, composite);
    }

    put_plan_change_contract(buf, change_contract);

    // D-319's period floor/cap set. **In the pin because a `PATCH` moves it and
    // because it decides what a subscriber is billed at minimum** -- a reviewer
    // who approved a plan with no minimum and a commit that publishes one at
    // $500 a period, with every digest equal, is `sku_id`'s re-verification hole
    // with a money consequence and no line on the invoice to explain it.
    //
    // Sorted on the market pair before framing, because the set arrives in the
    // repository's read order and a pin whose bytes depend on that order would
    // move for a re-read. Length-framed like every other collection here.
    let mut ordered_bounds: Vec<&PeriodFloorCap> = period_floor_caps.iter().collect();
    ordered_bounds.sort_unstable_by(|a, b| {
        (a.currency.as_str(), a.region.as_str()).cmp(&(b.currency.as_str(), b.region.as_str()))
    });
    put_u64(buf, count_of(ordered_bounds.len()));
    for bound in ordered_bounds {
        put_period_floor_cap(buf, bound);
    }

    let mut ordered_phases: Vec<&PlanPhase> = phases.phases().iter().collect();
    ordered_phases.sort_unstable_by_key(|phase| (phase.ordinal, phase.phase_id.get()));
    put_u64(buf, count_of(ordered_phases.len()));
    for phase in ordered_phases {
        put_plan_phase(buf, phase);
    }

    let mut ordered_rules: Vec<&AddonRule> = addon_rules.iter().collect();
    ordered_rules.sort_unstable_by_key(|rule| rule.addon_sku_id);
    put_u64(buf, count_of(ordered_rules.len()));
    for rule in ordered_rules {
        put_addon_rule(buf, rule);
    }

    match descriptor_set {
        None => put_bool(buf, false),
        Some(set) => {
            put_bool(buf, true);
            put_descriptor_set(buf, set);
        }
    }

    let mut ordered_rows: Vec<&PriceRecord> = rows.iter().collect();
    ordered_rows.sort_unstable_by_key(|record| record.price_id);
    put_u64(buf, count_of(ordered_rows.len()));
    for record in ordered_rows {
        put_price_record(buf, record);
    }

    let mut ordered_windows: Vec<&KeyWindows> = windows.iter().collect();
    ordered_windows.sort_by_key(|group| group.scope_key.to_string());
    put_u64(buf, count_of(ordered_windows.len()));
    for group in ordered_windows {
        put_key_windows(buf, group);
    }
}

/// One canonical scope key's window set.
///
/// **Sorted here as well as by its producer**, which is the module doc's own
/// discipline for `rows`: `window::group_by_key_seeded` already promises exactly
/// these two orders — groups by the key's canonical rendering, intervals by
/// `(effective_from, effective_to, state)` — so this changes no digest today. It
/// is here for the day that promise is restated, and because
/// [`PlanShape::windows`] is a public field a caller can fill in any order at
/// all.
///
/// `state` is part of the within-group **order** for `group_by_key_seeded`'s
/// reason: the two instants are not a total order over the elements, because a
/// `scheduled` window may carry exactly the bounds of an `expired` one on the same
/// key. It is the full four-valued state that sorts, not the narrowed token
/// [`put_window_interval`] frames, so the order is total and deterministic — and
/// the digest is order-free even so, since intervals that tie on their bounds
/// frame to the same token sequence in whichever order they are visited.
fn put_key_windows(buf: &mut Vec<u8>, group: &KeyWindows) {
    let KeyWindows {
        scope_key,
        intervals,
    } = group;
    put_scope_key(buf, scope_key);
    let mut ordered: Vec<&WindowInterval> = intervals.iter().collect();
    ordered.sort_unstable_by_key(|i| (i.effective_from, i.effective_to, i.state));
    put_u64(buf, count_of(ordered.len()));
    for interval in ordered {
        put_window_interval(buf, interval);
    }
}

/// One interval, with its state framed through [`pin_window_state`] rather than
/// verbatim — the module doc's narrowing, and the only place in this encoder where
/// the bytes are not the value.
fn put_window_interval(buf: &mut Vec<u8>, interval: &WindowInterval) {
    let WindowInterval {
        effective_from,
        effective_to,
        state,
    } = interval;
    put_instant(buf, *effective_from);
    put_opt_instant(buf, *effective_to);
    put_str(buf, pin_window_state(*state));
}

/// The two-token reading of a window state: clock-driven or cancelled.
///
/// **Exhaustive on purpose**, with no wildcard arm: a fifth window state would
/// have to be classified here, and a `_ =>` would classify it silently — as
/// `live`, which is the direction that hides a mutation from a reviewer. The
/// classification is the argument in the module doc, not a formatting choice, so
/// the compiler is what asks the next author for it.
const fn pin_window_state(state: WindowState) -> &'static str {
    match state {
        WindowState::Scheduled | WindowState::Active | WindowState::Expired => {
            PIN_WINDOW_STATE_LIVE
        }
        WindowState::Cancelled => PIN_WINDOW_STATE_CANCELLED,
    }
}

/// The custom interval **rides the variant**, so the token alone would collapse
/// `customEveryN(7, days)` and `customEveryN(90, days)` into one pin — a trial
/// length change with no digest change, which is D-115's own example of a
/// commercially material edit carrying no price delta.
fn put_frequency(buf: &mut Vec<u8>, frequency: Option<Frequency>) {
    match frequency {
        None => {
            put_none(buf);
            put_none(buf);
            put_none(buf);
        }
        Some(value) => {
            put_str(buf, value.as_str());
            match value {
                Frequency::CustomEveryN { n, unit } => {
                    put_u64(buf, u64::from(n));
                    put_str(buf, unit.as_str());
                }
                Frequency::Monthly
                | Frequency::Quarterly
                | Frequency::Semiannual
                | Frequency::Annual => {
                    put_none(buf);
                    put_none(buf);
                }
            }
        }
    }
}

fn put_plan_phase(buf: &mut Vec<u8>, phase: &PlanPhase) {
    let PlanPhase {
        phase_id,
        kind,
        display_name,
        ordinal,
        converts_to_phase_id,
        phase_duration_days,
        display_trial_days,
    } = phase;
    put_uuid(buf, phase_id.get());
    put_str(buf, kind.as_str());
    // D-357: a reviewer is shown the label they approve, so it is framed.
    put_opt_str(buf, display_name.as_deref());
    put_i64(buf, i64::from(*ordinal));
    put_opt_uuid(buf, converts_to_phase_id.map(PhaseId::get));
    put_opt_u64(buf, phase_duration_days.map(u64::from));
    put_opt_u64(buf, display_trial_days.map(u64::from));
}

fn put_addon_rule(buf: &mut Vec<u8>, rule: &AddonRule) {
    let AddonRule {
        addon_sku_id,
        required,
        min_qty,
        max_qty,
        step_qty,
        price_override_ref,
        depends_on,
        conflicts_with,
    } = rule;
    put_uuid(buf, *addon_sku_id);
    put_bool(buf, *required);
    put_opt_u64(buf, min_qty.map(u64::from));
    put_opt_u64(buf, max_qty.map(u64::from));
    put_opt_u64(buf, step_qty.map(u64::from));
    put_opt_uuid(buf, *price_override_ref);
    put_uuid_set(buf, depends_on);
    put_uuid_set(buf, conflicts_with);
}

/// An edge set, hashed as a **set**: sorted and de-duplicated.
///
/// `conflicts_with` is stored normalized symmetric and neither list has an
/// authored order the design set gives meaning to, so a re-ordering by a repo
/// read is not a content change and must not read as one.
fn put_uuid_set(buf: &mut Vec<u8>, ids: &[Uuid]) {
    let mut ordered: Vec<Uuid> = ids.to_vec();
    ordered.sort_unstable();
    ordered.dedup();
    put_u64(buf, count_of(ordered.len()));
    for id in ordered {
        put_uuid(buf, id);
    }
}

fn put_descriptor_set(buf: &mut Vec<u8>, set: &DescriptorSet) {
    let DescriptorSet {
        invoice_line_template,
        gl_code,
        itemization_rule,
        additional,
    } = set;
    put_opt_str(buf, invoice_line_template.as_deref());
    put_opt_str(buf, gl_code.as_deref());
    put_opt_str(buf, itemization_rule.as_deref());
    // A `BTreeMap` and never a `HashMap`, for the reason
    // `preconditions::request_digest` gives: a `HashMap`'s iteration order is
    // seeded per process, so the same descriptor set would pin differently in
    // two replicas and an approve served by the other one would answer
    // `APPROVAL_CONTENT_MISMATCH` about nothing.
    put_u64(buf, count_of(additional.len()));
    for (key, value) in additional {
        put_str(buf, key);
        put_str(buf, value);
    }
}

fn put_price_record(buf: &mut Vec<u8>, record: &PriceRecord) {
    let PriceRecord {
        price_id,
        scope_key,
        row,
        tax_inclusive,
        tax_category_ref,
        billing_timing,
        proration_contract,
        rounding_policy_ref,
        grandfather_until,
        supersedes_price_id,
        lifecycle_state,
        created_by,
        created_at_utc,
        row_version,
    } = record;
    put_uuid(buf, *price_id);
    put_scope_key(buf, scope_key);
    put_price_row(buf, row);
    put_bool(buf, *tax_inclusive);
    // **In the pin, because a `PATCH` can move it.** `tax_category_ref` is
    // authored draft content and D-48 makes it one of the descriptor set's
    // pinned v1 five, riding the row — so a reviewer who approved a plan billing
    // one tax category and a commit that publishes another, with every digest
    // equal, is exactly the re-verification hole `sku_id`'s own note describes.
    put_opt_str(buf, tax_category_ref.as_deref());
    put_opt_str(buf, billing_timing.as_deref());
    // The proration contract, framed **unconditionally** and field by field --
    // `put_scope_key`'s rule: a conditional field count is how two adjacent
    // fields become one ambiguous run. An absent contract frames the same
    // number of members as a present one, with the empty token and `0`, so an
    // absent contract and one anchoring `calendar_month` on day 0 cannot
    // collide.
    //
    // Destructured through the `Option` rather than reached with four dotted
    // `map_or`s, so a fourth field of the contract stops this compiling instead
    // of leaving the pin quietly short by one.
    let (billing_anchor_policy, proration_basis, credit_on_downgrade) = match proration_contract {
        None => (None, None, false),
        Some(ProrationContract {
            billing_anchor_policy,
            proration_basis,
            credit_on_downgrade,
        }) => (
            Some(*billing_anchor_policy),
            Some(*proration_basis),
            *credit_on_downgrade,
        ),
    };
    put_str(
        buf,
        billing_anchor_policy.map_or("", BillingAnchorPolicy::as_str),
    );
    put_u64(
        buf,
        u64::from(
            billing_anchor_policy
                .and_then(BillingAnchorPolicy::anchor_day)
                .map_or(0, AnchorDay::get),
        ),
    );
    put_str(buf, proration_basis.map_or("", ProrationBasis::as_str));
    put_bool(buf, credit_on_downgrade);
    put_opt_str(buf, rounding_policy_ref.as_deref());
    put_opt_instant(buf, *grandfather_until);
    put_opt_uuid(buf, *supersedes_price_id);
    put_str(buf, lifecycle_state.as_str());
    put_uuid(buf, *created_by);
    put_instant(buf, *created_at_utc);
    put_u64(buf, row_version.get());
}

/// The scope key's axes, bound through [`ScopeKeyParts`]: `parts()` destructures
/// [`ScopeKey`] exhaustively, so a frame that went short here would not compile.
///
/// **This is where the exhaustive binding earns its keep.** [`put_price_record`]
/// frames the row straight after the key, and `meter` and `dimension_key` are
/// **also** columns of [`PriceRow`], so a record's digest moves with them whatever
/// this does. The damage would land on [`put_key_windows`], where a key is framed
/// with no row beside it: two window plans on two meters of one market would pin
/// identically, and an approve could be satisfied by a re-derivation over the
/// other line's coverage.
///
/// The pair is framed **unconditionally**, `none` and `''` on a non-usage key,
/// rather than only on `usage` rows: a conditional field count is how two adjacent
/// values become re-splittable, which is the hazard [`count_of`] exists for one
/// level up.
fn put_scope_key(buf: &mut Vec<u8>, key: &ScopeKey) {
    // Destructured through `parts()`, and this is the site where that matters
    // most: a short frame here pins two window plans on two meters of one market
    // identically, so an approve can be satisfied by a re-derivation over the other
    // line's coverage. An eleventh axis is a compile error here rather than a digest
    // that quietly stops discriminating. `put_price_row` below has the same shape.
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
    } = key.parts();
    put_uuid(buf, plan_id.get());
    put_str(buf, currency.as_str());
    put_str(buf, region.as_str());
    put_str(buf, price_overlay.as_str());
    put_uuid(buf, phase.get());
    put_str(buf, price_eligibility.as_str());
    put_str(buf, charge_kind.as_str());
    // The generation instant rather than `Display`'s rendering of it: the axis
    // is the instant, and hashing a rendering would make the pin depend on a
    // formatting choice.
    put_opt_instant(buf, cohort.generation());
    // The usage line (D-196). `meter` is genuinely optional — a usage row with no
    // meter is admissible — while `dimension_key` is total, `''` being the
    // undimensioned line, so the two take the two different framings rather than
    // one spelling for both.
    put_opt_str(buf, meter.map(Meter::as_str));
    put_str(buf, dimension_key.as_str());
}

fn put_price_row(buf: &mut Vec<u8>, row: &PriceRow) {
    let PriceRow {
        charge_kind,
        model_kind,
        amount_minor,
        unit_rate,
        bands,
        package_size,
        package_price_minor,
        quantity_source,
        manual_quantity,
        meter,
        dimension_key,
        billing_granularity,
        tier_aggregation_window,
        tier_qualification_window,
        aggregation_function,
        aggregation_granularity,
        max_hold_granules,
        included_allowance,
        reserved_rate,
        reservation_flavor,
        min_qty_purchase,
        min_qty_usage,
        min_qty_usage_fallback,
        discount_ref,
    } = row;
    put_str(buf, charge_kind.as_str());
    put_opt_str(buf, model_kind.map(model_kind_wire));
    put_opt_i64(buf, amount_minor.map(MinorAmount::get));
    // The `per_unit` rate (D-311). Framed beside the amount rather than
    // through it: they are two fields, and a preimage that folded them into one
    // slot would let a row that moved its price between them hash identically to
    // its own predecessor.
    put_opt_i64(buf, unit_rate.map(RateMinor::nano_minor));

    let mut ordered: Vec<&TierBand> = bands.iter().collect();
    ordered.sort_unstable_by_key(|band| {
        (
            band.from_qty,
            band.to_qty.closed_at(),
            band.unit_price_rate.nano_minor(),
        )
    });
    put_u64(buf, count_of(ordered.len()));
    for band in ordered {
        put_tier_band(buf, band);
    }

    put_opt_u64(buf, *package_size);
    put_opt_i64(buf, package_price_minor.map(MinorAmount::get));
    put_opt_str(buf, quantity_source.map(QuantitySource::as_str));
    put_opt_u64(buf, *manual_quantity);
    put_opt_str(buf, meter.as_deref());
    put_str(buf, dimension_key);
    put_opt_str(buf, billing_granularity.map(BillingGranularity::as_str));
    put_opt_str(
        buf,
        tier_aggregation_window.map(TierAggregationWindow::as_str),
    );
    put_opt_str(
        buf,
        tier_qualification_window.map(TierQualificationWindow::as_str),
    );
    put_opt_str(buf, aggregation_function.map(AggregationFunction::as_str));
    put_opt_str(
        buf,
        aggregation_granularity.map(AggregationGranularity::as_str),
    );
    put_opt_u64(buf, *max_hold_granules);

    match included_allowance {
        None => put_bool(buf, false),
        Some(allowance) => {
            put_bool(buf, true);
            let IncludedAllowance {
                quantity,
                rollover_policy,
            } = allowance;
            put_u64(buf, *quantity);
            put_str(buf, rollover_policy.as_str());
        }
    }

    // The reservation pair (`inst-rv-attrs`). Framed as two independent optional
    // fields rather than as one optional pair, because that is what the row
    // holds: the pairing is a publish rule, so a half-authored reservation is a
    // state a *draft* can be in and a reviewer can be shown. Framing it as a
    // pair would make the two half-states frame alike.
    put_opt_i64(buf, reserved_rate.map(RateMinor::nano_minor));
    put_opt_str(buf, reservation_flavor.map(ReservationFlavor::as_str));

    // The typed floors and the discount hook (`inst-ft-typed`,
    // `inst-ft-fallback`, `inst-dr-referential`). Four independent optional
    // fields: the floors are distinct facts (`inst-ft-both` permits both on one
    // row) and the fallback is a third, so nothing here is a pair.
    put_opt_u64(buf, *min_qty_purchase);
    put_opt_u64(buf, *min_qty_usage);
    put_opt_str(buf, min_qty_usage_fallback.map(MinQtyUsageFallback::as_str));
    put_opt_str(buf, discount_ref.as_deref());
}

fn put_tier_band(buf: &mut Vec<u8>, band: &TierBand) {
    let TierBand {
        from_qty,
        to_qty,
        unit_price_rate,
    } = band;
    put_u64(buf, *from_qty);
    // `None` is `open`, which is a *state* of the band and not an absent value —
    // and the ABSENT marker distinguishes it from every closed top including
    // zero.
    put_opt_u64(buf, to_qty.closed_at());
    put_i64(buf, unit_price_rate.nano_minor());
}

/// One policy version: its number, the instant it takes effect, and its entries.
///
/// The container's fields are private — [`ThresholdVersion::new`] is what refuses
/// an empty or duplicated entry set — so it is reached through
/// [`ThresholdVersion::parts`], whose own body destructures the version
/// exhaustively. That is [`put_scope_key`]'s arrangement and it replaced three
/// bare accessor calls: under those, a fourth field on the version compiles, pins
/// nothing, and leaves every test green.
fn put_threshold_version(buf: &mut Vec<u8>, version: &ThresholdVersion) {
    let ThresholdVersionParts {
        version,
        effective_from,
        entries,
    } = version.parts();
    put_u64(buf, version);
    put_instant(buf, effective_from);
    // The count precedes the elements, as every collection's does: without it two
    // versions differing only in where one entry ends and the next begins could
    // frame alike.
    //
    // **And a count of zero is the tombstone (D-185), which is why no token was added
    // for it.** `ThresholdVersion::new` refuses an empty entry set, so
    // `ThresholdVersion::tombstone` is the sole producer of a version framing `0`
    // here — an approver signing "this tenant has no thresholds" is signing a preimage
    // no particular set can produce, which is the property the decision asks of this
    // encoder. A dedicated marker byte would have said the same thing and moved the
    // bytes of **every** existing version, re-freezing the encoding and invalidating
    // every pending D-10 unit to record the arrival of a subject none of them are
    // about — the argument `CONTENT_PIN_DOMAIN_SEP`'s own doc makes for not bumping
    // itself when this subject arrived.
    put_u64(buf, count_of(entries.len()));
    // **Not sorted here.** Every other collection in this encoder is, because the
    // repositories' `ORDER BY`s are the thing that could move under it. This one
    // arrives ordered by `ThresholdVersion`'s own contract — the store orders by
    // currency and the surface sorts before it proposes — and sorting again would
    // put a second answer beside that contract, free to disagree with the order the
    // reviewer's document renders in.
    for entry in entries {
        put_threshold_entry(buf, entry);
    }
}

/// One currency's entry.
fn put_threshold_entry(buf: &mut Vec<u8>, entry: &ThresholdEntry) {
    let ThresholdEntry { currency, basis } = entry;
    put_str(buf, currency.as_str());
    put_threshold_basis(buf, *basis);
}

/// One basis, framed as its tag and then its value.
///
/// **Exhaustive on purpose**, with no wildcard arm and for [`pin_window_state`]'s
/// reason: a third basis would have to be framed here, and a `_ =>` would frame it
/// as one of these two — which is a threshold a reviewer signed off in one unit
/// applying as another.
fn put_threshold_basis(buf: &mut Vec<u8>, basis: ThresholdBasis) {
    match basis {
        ThresholdBasis::Absolute { minor } => {
            put_str(buf, PIN_THRESHOLD_BASIS_ABSOLUTE);
            put_i64(buf, minor);
        }
        ThresholdBasis::Percent { bp } => {
            put_str(buf, PIN_THRESHOLD_BASIS_PERCENT);
            put_u64(buf, u64::from(bp));
        }
    }
}

// ---------------------------------------------------------------------------
// Length-prefixed, NULL-safe framing primitives.
//
// `audit.rs`'s, copied rather than shared; see the module doc for why two frozen
// encodings do not go behind one edit.
// ---------------------------------------------------------------------------

/// A present field: marker, u32-BE length, bytes.
fn put(buf: &mut Vec<u8>, bytes: &[u8]) {
    buf.push(PRESENT);
    // Saturating rather than failing, as in `audit.rs`: no field this gear
    // authors reaches 4 GiB, and a truncated length is still hashed together
    // with the bytes that follow it.
    let len = u32::try_from(bytes.len()).unwrap_or(u32::MAX);
    buf.extend_from_slice(&len.to_be_bytes());
    buf.extend_from_slice(bytes);
}

/// An absent field: a bare marker, so `None` and an empty value differ.
fn put_none(buf: &mut Vec<u8>) {
    buf.push(ABSENT);
}

fn put_str(buf: &mut Vec<u8>, value: &str) {
    put(buf, value.as_bytes());
}

fn put_opt_str(buf: &mut Vec<u8>, value: Option<&str>) {
    match value {
        Some(value) => put_str(buf, value),
        None => put_none(buf),
    }
}

fn put_uuid(buf: &mut Vec<u8>, value: Uuid) {
    put(buf, value.as_bytes());
}

fn put_opt_uuid(buf: &mut Vec<u8>, value: Option<Uuid>) {
    match value {
        Some(value) => put_uuid(buf, value),
        None => put_none(buf),
    }
}

fn put_u64(buf: &mut Vec<u8>, value: u64) {
    put(buf, &value.to_be_bytes());
}

fn put_opt_u64(buf: &mut Vec<u8>, value: Option<u64>) {
    match value {
        Some(value) => put_u64(buf, value),
        None => put_none(buf),
    }
}

fn put_i64(buf: &mut Vec<u8>, value: i64) {
    put(buf, &value.to_be_bytes());
}

fn put_opt_i64(buf: &mut Vec<u8>, value: Option<i64>) {
    match value {
        Some(value) => put_i64(buf, value),
        None => put_none(buf),
    }
}

fn put_bool(buf: &mut Vec<u8>, value: bool) {
    put(buf, &[u8::from(value)]);
}

/// An instant, in **microseconds** — the same resolution `audit_row_hash` uses
/// and the resolution `timestamptz` stores, so a value that round-tripped
/// through the column hashes as it did before it was written.
fn put_instant(buf: &mut Vec<u8>, value: OffsetDateTime) {
    put_i64(buf, timestamp_micros(value));
}

fn put_opt_instant(buf: &mut Vec<u8>, value: Option<OffsetDateTime>) {
    match value {
        Some(value) => put_instant(buf, value),
        None => put_none(buf),
    }
}

/// A collection's element count, saturating.
///
/// The count is framed **before** the elements so that two adjacent collections
/// cannot be re-split between them — a plan with two phases and no add-on rules
/// must not hash as one with one phase and one rule.
fn count_of(len: usize) -> u64 {
    u64::try_from(len).unwrap_or(u64::MAX)
}

/// The 32-byte SHA-256 digest of a preimage.
fn digest32(buf: &[u8]) -> [u8; 32] {
    let digested = sha256(&SHA256, buf);
    let mut out = [0_u8; 32];
    out.copy_from_slice(digested.as_ref());
    out
}

#[cfg(test)]
#[path = "content_pin_tests.rs"]
mod content_pin_tests;
