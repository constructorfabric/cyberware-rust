//! The audit record's canonical, byte-reproducible encoding and its hash link.
//!
//! `pricing_audit_log` is evidence, not a log: D-14 makes tamper-evidence an
//! **in-database hash chain** written inside the mutation's own ACID
//! transaction, so there are no lost records on a crash and an unavailable
//! audit store cannot exist separately from an unavailable database. What makes
//! that chain evidence is that the bytes it hashes are reproducible — a
//! verification job walks a segment years later and recomputes every link, so
//! any encoding this module can produce two ways is a break the job will report
//! that nobody caused.
//!
//! Pure domain: no storage, no `AccessScope`, no I/O. The writer is
//! [`crate::infra::storage::repo::audit_repo`].
//!
//! ## Why the chain is segmented, and what segmentation costs (D-135)
//!
//! A chain is a **strict sequence**: writing row *N* needs row *N-1*'s hash. One
//! chain per tenant therefore serialized **every** audited mutation of that
//! tenant behind a single head, *inside* the mutation transaction — all
//! authoring serialized by construction, against a `>= 50 rows/s` repricing SLO
//! whose per-row cost model never listed the audit write at all. Segmented per
//! `(tenant_id, chain_id)`, where `chain_id` is the audited subject's aggregate
//! (plan, overlay, payer, policy, bulk operation), concurrent mutations of
//! different aggregates proceed independently, while a bulk run's rows — one
//! plan, one `chain_id` — extend sequentially inside that plan's own
//! transaction anyway.
//!
//! What segmentation **costs** is cross-segment evidence: removing a whole
//! segment leaves no gap in any surviving chain. That is restored by a periodic
//! per-tenant **roll-up** row chaining the segment heads — deleting a row breaks
//! its segment, deleting a segment breaks the roll-up. **The roll-up is not
//! written here and is not written anywhere in this gear yet.** It is periodic
//! rather than on the mutation path, and this module deliberately declares no
//! roll-up encoding, so a reader does not go looking for a writer that is
//! absent by design. `chk_pricing_audit_log_rollup` already makes a mutation row
//! carrying segment heads impossible, so nothing written through this encoding
//! can be mistaken for one.
//!
//! ## The genesis seed is bound to the segment, not to the tenant
//!
//! [`genesis_prev_hash`] takes **both** `tenant_id` and `chain_id`. A genesis
//! bound to the tenant alone would give every one of that tenant's segments the
//! same first link, and a whole segment could then be lifted onto another
//! aggregate — same tenant, same seeds, every link still verifying — with
//! nothing in the chain able to tell.
//!
//! ## Two vocabularies, one of them borrowed and one of them ours
//!
//! The migration types `action` and `subject_kind` as free `text` with no
//! `CHECK`, and **no document in the design set declares either vocabulary**.
//! S5 §6 declares a `subject_kind` enumeration for `pricing_approval`
//! (`plan_revision | price_unit | window | overlay | membership | bundle |
//! retirement | policy | bulk_batch` — `historical_import` left it, and the audit
//! half with it, when D-330 struck the import flow) and nothing declares
//! one for `pricing_audit_log`. [`AuditSubjectKind::PlanRevision`] therefore
//! **borrows S5 §6's spelling** for the one value this group writes, so the two
//! stores do not end up naming one thing two ways; the borrowing is recorded
//! here because a token taken from a neighbouring table is one a later document
//! is free to contradict.
//!
//! [`AuditAction::Publish`] is this module's own naming decision, in the
//! `snake_case` shape every other persisted token in this crate uses. It is
//! deliberately **not** `PlanPublished`: that is a frozen *event* name owned by
//! [`CatalogEvent`](crate::domain::events::CatalogEvent), and an audit action
//! spelled the same would be a second home for one string.
//!
//! Only what the crate **writes** is declared. A variant with no writer would
//! read as coverage to everyone who greps for it — D-158's own constraint, and
//! the reason `retire`, `backdate_import` and `policy_update` are absent below
//! though S5 §6 declares them. [`AuditAction::Deny`], [`AuditAction::Approve`]
//! and [`AuditAction::Reject`] each left that list when their writer arrived;
//! see the note at the end of this doc.
//!
//! ## Two of the ten actions are this module's spelling and are **owed to the
//! document**. Reported.
//!
//! `create`, `update` and `delete` are **D-175**'s and are not named here. What
//! this section names is the same gap on the approval plane, found by the same rule
//! and reported the same way.
//!
//! S5 §6's `action` roster is `create | update | delete | publish | abandon |
//! retire | approve | reject | deny | backdate_import | policy_update`, and
//! D-175 clause (2) closes it with **no writer without a token**: every mutating
//! surface this design set specifies carries an `action` there. Two of this
//! slice's surfaces do not.
//!
//! - **[`AuditAction::Submit`]** — the approval unit `POST …/plans/{planId}/publish`
//!   opens on a material change (§4's initial state: *"submitted (opened by a
//!   material change unit … submitter recorded)"*). Its warrant is **`dod-audit`
//!   and nothing wider**: §8's *"**every mutation MUST record**"*, and an insert
//!   into `pricing_approval` is a mutation. That is D-175's own ground, and it is
//!   enough.
//!
//!   **`inst-tp-record` is not a warrant for this token**, and citing it would
//!   overstate the debt. That instruction — *"submitter and approver identities +
//!   timestamps land on the approval record **and in the audit trail**"* — is
//!   assigned by S5 §6 to `approve`/`reject`, whose records satisfy it through
//!   `approval_repo`'s `unit_state` rendering `submitterPrincipal` on both halves of
//!   the pair, with no `submit` token at all. Nor is the submit a *distinct surface*
//!   the roster overlooks: it is the non-committing arm of
//!   `POST …/plans/{planId}/publish`, which the roster names through `publish`.
//! - **[`AuditAction::Withdraw`]** — `POST …/approvals/{approvalId}/withdraw`,
//!   which `inst-as-void` specifies **and calls audited** in as many words. It is
//!   not `reject` (a different edge, by a different authority, with a mandatory
//!   reason) and not `abandon` (D-145's plan-draft flip); the roster has no verb
//!   for it either.
//!
//! Both are **this module's spelling of a record the set requires and does not
//! name**, in the `snake_case` shape D-158 fixes, and both are **reported rather
//! than treated as declared** — D-158's instruction is that "a slice that adds an
//! audited record adds its token **here**", meaning the document, and a code
//! group may not edit one. It is not the same act as minting a wire code: an
//! `action` is a stored discriminator on a vocabulary D-158 makes explicitly
//! **additive**, whose only reader (S12 `inst-he-read`'s Auditor-only sibling)
//! does not exist yet, in a greenfield store — so a document choosing `void` over
//! `withdraw` costs a rename and not a contract change. This is D-175's situation
//! reproduced exactly, and D-175 was ratified on the condition that the debt land
//! in S5 §6 in the next documentation wave.
//!
//! The alternative was to leave the submit and the withdraw writing nothing.
//! That is the state the code-debt register classes *wrong now* rather than
//! *absent*: `pricing_audit_log` is the store D-12 confines to the Auditor, and a
//! withdrawn unit would answer "who withdrew this" with nothing —
//! indistinguishably from "the guard voided it", which is the *other* way a unit
//! reaches `voided` and the one no principal asked for.
//!
//! ## The machine-driven void writes **no** record, and that is a boundary
//!
//! [`AuditAction::Withdraw`] is a **human** act. The TOCTOU guard
//! (`approval_repo::void_pending_for_plan`, `inst-ap-pin`) closes a unit with no
//! principal at all, from inside the transaction of the mutation that
//! invalidated it — and that mutation has already written its own record, on the
//! **same** segment (D-135 keys both on the plan) at the same instant, under the
//! actor who caused it. A second record there would need an actor the act does
//! not have: `actor_principal_id` is a non-null `uuid` precisely because
//! `inst-au-pii` makes the actor a fact rather than a nicety, and a synthetic one
//! would be the trail asserting a principal that did not act.
//!
//! ## The audit **read** surface
//!
//! `inst-au-read` puts the Auditor-only trail behind `audit × read`, paginated
//! per D-125, and [`crate::api::rest::audit`] is it: `GET /bss-pricing/v1/audit`,
//! merged into the gear router, reading the trail through
//! [`crate::infra::audit_read`]. `infra::error_mapping` leans on that route — its
//! `denied` arms drop their detail on the ground that the deny record is
//! readable — so the compensating control is reachable rather than owed, and a
//! second read surface would be a second answer to a question this one already
//! answers. What is still owed of the trail is below.
//!
//! ## What is deliberately absent
//!
//! - **The roll-up encoding and its writer** — see above.
//! - **The verification job** (`pricing_audit_chain_verified`). It walks
//!   segments and the roll-up alike and is off the mutation path by definition;
//!   this module gives it the primitives it will recompute with and nothing
//!   more.
//! - **`backdate_import` and `policy_update`.** S5 §6 declares both and this
//!   gear has no writer for either: there is no `pricing_historical_price` store
//!   (`inst-bd-store`) and no threshold-policy store (D-10). A token declared
//!   here with no writer is indistinguishable from a record nobody writes, so
//!   they stay owed.
//!
//!   **`retire` is not among them**: S5 §6 declares it and
//!   `crate::infra::retirement` writes it, inside the flip's own transaction
//!   (D-128, Slice 11).
//!
//!   **`migrate` is not either, and is minted rather than inherited.** It comes
//!   from Slice 11's migration plane rather than S5 §6's list, so the
//!   one-token-one-writer rule is satisfied by construction:
//!   `crate::infra::migration` writes it inside the schedule's and the cancel's
//!   own transactions. S5 §6 enumerates `migrate` as an **authz action** on the
//!   `plan` resource; whether its audit-token list should name it too is in the
//!   Slice 11 hand-back's documentation register, because that list is `docs/`
//!   and not this module's to extend.
//!
//! `approve`, `reject` and `deny` **are** declared, and each has its writer:
//! `pricing_approval` holds the records, a plan-revision subject's chain is the
//! plan's by D-135, `crate::infra::storage::repo::approval_repo` takes the stamp
//! and writes the decision's record inside the deciding transaction, and
//! `crate::infra::approval::decide` writes the refused attempt's. The rule is not
//! "declare nothing until everything is built"; it is one token, one writer,
//! landing together.

use std::fmt;
use toolkit_macros::domain_model;
use uuid::Uuid;

use aws_lc_rs::digest::{SHA256, digest as sha256};
use time::OffsetDateTime;

/// Versioned domain-separation tag for this gear's audit chain.
///
/// Bumped **only** on an intentional re-freeze of the encoding, which also
/// means regenerating the byte-repro vector in `audit_tests.rs`. The tag is
/// what keeps this chain's digests disjoint from every other chain the platform
/// hashes — the sibling ledger's posting chain and its own audit chain both sit
/// in the same process space, and a shared preimage space is a shared collision
/// space.
pub const AUDIT_DOMAIN_SEP: &[u8] = b"VHP-BSS-PRICING-AUDIT-v1\x1f";

/// NULL-safe presence marker for an absent field: a bare byte.
const ABSENT: u8 = 0x00;

/// NULL-safe presence marker for a present field: `PRESENT` + u32-BE len + bytes.
const PRESENT: u8 = 0x01;

/// What was done to the audited subject.
///
/// One variant per audited path this crate has. See the module doc for
/// why two of the spellings are owed back to S5 §6, and why the tokens are not
/// the event names.
#[domain_model]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AuditAction {
    /// A draft plan revision or a draft price row was authored.
    ///
    /// **And a `pricing_bulk_operation` row's open** — a
    /// mass-repricing run's `POST /repricing-runs`
    /// (`crate::api::rest::repricing_runs`), inside the transaction that opens
    /// the row and freezes its journal. Not a new token: opening a bulk
    /// operation is the same kind of act `Create` already names for a draft, and
    /// giving it a verb of its own would be minting where the roster already has
    /// one.
    Create,
    /// A draft's content was replaced — a plan facet, or a price row's whole
    /// content.
    Update,
    /// A never-published draft price row was removed (`inst-ps-nodelete` keeps
    /// this off a published one).
    Delete,
    /// A plan's open draft revision was discarded — the flip, not a deletion
    /// (D-145). It is a distinct action from [`AuditAction::Delete`] because the
    /// row survives and its number stays consumed.
    Abandon,
    /// A publish commit moved the subject into `published`.
    Publish,
    /// A material change unit was opened over the subject and awaits an
    /// independent reviewer (§4's initial state).
    ///
    /// **Minted here, owed to S5 §6, on `dod-audit` alone** — the module doc says
    /// why `inst-tp-record` is not this token's warrant. Its writer is
    /// [`approval_repo::open`](crate::infra::storage::repo::approval_repo::open),
    /// inside the transaction that takes the content pin.
    Submit,
    /// An independent reviewer agreed and the publish may continue
    /// (`inst-as-approve`, `inst-tp-record`).
    Approve,
    /// A reviewer refused the change unit, with the mandatory reason
    /// `inst-as-reject` requires.
    Reject,
    /// The submitter, or a `CatalogAdmin`, withdrew a pending unit without
    /// mutating its subject — freeing the pinned scope key (`inst-as-void`).
    ///
    /// **Minted here, owed to S5 §6.** Distinct from [`AuditAction::Reject`]
    /// (another edge, another authority, a mandatory reason) and from
    /// [`AuditAction::Abandon`] (D-145's plan-draft flip, on the other plane).
    /// The **machine-driven** void writes no record at all; the module doc says
    /// why.
    Withdraw,
    /// An attempt to exercise an authority the principal does not hold, refused
    /// (`inst-rb-audit`, and `inst-tp-selfaudit`'s attempted-violation record).
    ///
    /// **The one token here whose record is not a mutation**, and the reason it
    /// still belongs: a self-approval attempt is the financial-fraud vector this
    /// slice exists for, and a trail that recorded only what succeeded would
    /// hold no evidence that it was ever tried. Its `before_state` /
    /// `after_state` pair therefore reads differently from every other action's
    /// - see `crate::infra::approval` for what each holds and why.
    ///
    /// Its writer is `crate::infra::approval::record_denial`, **inside the
    /// judgement transaction** — like every other record in this crate, and not in a
    /// second one on the premise that the judgement rolls back with the refusal: the
    /// judgement returns the refusal as `Ok` and therefore commits, so the record
    /// commits with it. Its siblings [`AuditAction::Approve`] and
    /// [`AuditAction::Reject`] record the decisions that were *taken*.
    Deny,
    /// A published plan was **retired** — the terminal, sales-stopping flip
    /// (`inst-rt-cancel`, `inst-rt-event`, D-128).
    ///
    /// **Owed to S5 §6 since this gear had a `pricing_audit_log`, and no longer
    /// owed as of Slice 11**: the module doc's absent list named `retire` as a
    /// token whose writer did not exist, and the rule there is one token, one
    /// writer, landing together. The writer is `crate::infra::retirement`, inside
    /// the transaction that performs the flip.
    ///
    /// Distinct from [`AuditAction::Publish`] though retirement **is** a publish
    /// unit (D-128, running the same engine path): what an auditor needs to find
    /// is the act that stopped the plan selling, and a trail on which retirement
    /// rendered as one more `publish` would hide the one plan-lifecycle event
    /// that can never be undone. Distinct from [`AuditAction::Abandon`] for the
    /// opposite reason — that one is the *draft* revision's flip, and a single
    /// retirement writes both records: this one for the plan, `abandon` for the
    /// draft revision D-145 tombstones alongside it.
    Retire,
    /// A migration was **scheduled or cancelled** (`inst-ms-api`,
    /// `inst-mst-cancel`, `inst-mst-cancel-inflight`, D-34, Slice 11).
    ///
    /// **One token for both acts, deliberately.** The schedule and its
    /// cancellation are two transitions of one `pricing_migration` row, and the
    /// record's `before_state`/`after_state` carry the schedule's `state` —
    /// so an auditor reads which transition happened from the states rather
    /// than from a second verb. Splitting them would put the state machine in
    /// the token vocabulary as well as in the row, which is the duplication
    /// [`AuditAction::Retire`]'s note argues against one act over.
    ///
    /// Distinct from [`AuditAction::Retire`] though the two are usually paired
    /// (§3 step 4 recommends a retirement pair with a migration schedule): what
    /// an auditor needs to separate is the act that stopped the plan selling
    /// from the act that offered its subscribers somewhere to go. Distinct from
    /// [`AuditAction::Update`] because a migration mutates no plan content at
    /// all — the catalog emits a schedule and Subscriptions executes it.
    Migrate,
}

/// Declares a persisted vocabulary's roster **and** the `match` that turns each
/// member into its stored token, from one list.
///
/// One source for both halves, `infra::storage_tests`'s `repo_error_roster!` shape
/// and its reason. Written out by hand the two force different things: the `match`
/// forces a *token* per variant, while `ALL` is a plain array a new variant leaves
/// **short** — and short is the shape that reads as coverage. Every roster walk in
/// this crate iterates `ALL`, so the positive control that inserts one row per
/// token never reaches the member nobody wrote into the array, `domain::audit_tests`
/// compares one hand-list against another, and the vocabulary `CHECK` refuses the
/// first production write of that action inside the mutation's own transaction,
/// rolling the mutation back with it.
///
/// The arms below are **expanded rather than written**, so there is no `_` an author
/// can reach for: a variant with no entry leaves the generated `match`
/// non-exhaustive and the crate does not compile until it is written into the list,
/// which is also what puts it in `ALL`. The diagnostic is `E0004`, naming the
/// variant.
macro_rules! persisted_token_roster {
    (
        $enum:ident,
        $(#[$all_meta:meta])*
        ALL,
        $(#[$as_str_meta:meta])*
        as_str,
        $($variant:ident => $token:literal,)+
    ) => {
        impl $enum {
            $(#[$all_meta])*
            pub const ALL: &'static [Self] = &[$(Self::$variant),+];

            $(#[$as_str_meta])*
            #[must_use]
            pub const fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $token,)+
                }
            }
        }
    };
}

persisted_token_roster! {
    AuditAction,
    /// Every action, stable order.
    ///
    /// **Every entry here has a production writer**, which is D-158's constraint
    /// and what `sqlite_publish_commit::every_declared_action_has_a_production_writer`
    /// asserts by walking this slice against the actions the crate's own paths
    /// actually emit: a vocabulary entry nobody writes reads as coverage to
    /// everyone who greps for it.
    ///
    /// *(`tests/sqlite_audit_chain.rs` walks this slice too and asserts something
    /// else: `every_action_the_domain_declares_is_storable_in_the_trail` inserts each
    /// token against `chk_pricing_audit_log_action`, so what it proves is that the
    /// **store** admits the vocabulary — never that anything writes it. A reader who
    /// takes that walk for the writer claim reads D-158's constraint as enforced when
    /// only the column's is.)*
    ALL,
    /// The persisted `action` token.
    as_str,
    Create => "create",
    Update => "update",
    Delete => "delete",
    Abandon => "abandon",
    Publish => "publish",
    Submit => "submit",
    Approve => "approve",
    Reject => "reject",
    Withdraw => "withdraw",
    Deny => "deny",
    Retire => "retire",
    Migrate => "migrate",
}

impl fmt::Display for AuditAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What kind of thing the audited subject is.
///
/// Both variants are spelled as S5 §6 spells the same subjects for
/// `pricing_approval`, **verbatim** — unlike the three actions above, nothing
/// here is minted. See the module doc.
#[domain_model]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AuditSubjectKind {
    /// One revision row of a plan — the `(plan_id, revision)` durable name.
    PlanRevision,
    /// One price row, named by its `price_id`.
    ///
    /// Its **chain** is still the plan's (D-135 keys the segment on the audited
    /// subject's *aggregate*, and a price row's aggregate is the plan it prices),
    /// which is what makes "who changed this plan" one segment to walk rather
    /// than a join.
    PriceUnit,
    /// One `pricing_price_window` row, named by its `window_id`.
    ///
    /// **Declared in S5 §6 already**, in the enumeration D-158 takes verbatim from
    /// `pricing_approval` — so this member implements a token the design set names
    /// rather than minting one, and the gear's standing rule that a token with no
    /// writer is not declared is what kept it out until now. It has **two** writers,
    /// and they arrived one change apart: `infra::window`'s three mutations write the
    /// audit record, and `infra::approval::ApprovalService::submit_window_mutation`
    /// opens a pending unit under the same token — the D-62/D-99 window unit
    /// `inst-co-single-pending` names among the units that hold a key.
    ///
    /// Its **chain** is the plan's, exactly as [`AuditSubjectKind::PriceUnit`]'s
    /// is and for the same reason: D-135 keys a segment on the audited subject's
    /// *aggregate*, a window's aggregate is the plan its row prices, and D-99 makes
    /// windows plan facts outright. That is also why S5 §6's aggregate list —
    /// plan, overlay, payer, policy, bulk operation — correctly omits `window`: a
    /// subject kind is not an aggregate, and reading the omission as a gap would
    /// give one plan two chains to interleave.
    Window,
    /// One **version** of the tenant's approval-threshold policy, named by its
    /// version number.
    ///
    /// **Declared in S5 §6 already**, in the same enumeration D-158 takes verbatim
    /// from `pricing_approval` — so this member implements a token the design set
    /// names rather than minting one, and it stayed out until its writer existed.
    /// That writer is
    /// [`ApprovalService::submit_threshold_policy`](crate::infra::approval::ApprovalService::submit_threshold_policy),
    /// the D-10 unit a `PUT /config/approval-threshold-policy` opens, and it landed
    /// in the same change as this member and as
    /// `chk_pricing_approval_subject_kind`'s fourth token.
    ///
    /// Its **chain** is *not* the plan's, and it is the first member of this
    /// enumeration that is not. D-135 keys a segment on the audited subject's
    /// aggregate; a threshold policy has no plan and S5 §6's own aggregate list —
    /// plan, overlay, payer, **policy**, bulk operation — names `policy` as one in
    /// its own right. So it gets [`policy_chain`](crate::infra::storage::repo::audit_repo::policy_chain),
    /// one segment per tenant, which is also what keeps a policy proposal off the
    /// head every mutation of some arbitrary plan is contending for.
    Policy,
    /// One `pricing_price_overlay` row, named by its `price_overlay_id`.
    ///
    /// **Not declared in S5 §6**, and the second member of this enumeration that is
    /// not the plan's — for `Policy`'s reason read one entry along S5 §6's own
    /// aggregate list: plan, **overlay**, payer, policy, bulk operation. An overlay
    /// is an aggregate in its own right there, and it has to be: an overlay **is not
    /// a plan and has no plan**, so borrowing [`Self::PlanRevision`] would put two
    /// aggregates on one chain and make "who changed this plan" answer about an
    /// object the plan does not contain.
    ///
    /// Its writer is `OverlayRepo`'s four mutations, which is why it is here — without
    /// it the overlay plane writes **no audit record at all**, a plain
    /// regression against D-14 on an always-material subject, and the whole obstacle
    /// was this enumeration having no token for it (Slice 9's register, O-3).
    ///
    /// **Its approval-plane writer does not exist yet**, and that is the one respect
    /// in which this member differs from `Policy`, whose token, Rust member and
    /// writer all landed together. D-50 makes every overlay mutation an approval
    /// subject, so the token is owed on that plane too; the unit that would open it
    /// is Slice 9's O-7 and is not wired. D-158 makes `pricing_approval` and
    /// `pricing_audit_log` one enumeration and requires them to be **extended
    /// together**, so `chk_pricing_approval_subject_kind`
    /// (`m20260821_000003_create_pricing_approval`) already admits the token this
    /// member renders — `overlay`, in the roster `plan_revision | price_unit |
    /// window | policy | overlay | bulk_operation | membership`. A kind that is
    /// storable and not yet stored is what D-158 asks for, and is strictly
    /// narrower than the "no token without a writer" rule the audit plane's
    /// writer satisfies. The token is `overlay` and not `price_overlay`: the
    /// CHECK refuses the latter, which is what an approval-plane writer following
    /// this note would otherwise learn at runtime.
    Overlay,
    /// One `pricing_bulk_operation` row — a mass-repricing run or a bulk import,
    /// named by its `operation_id`.
    ///
    /// **Not declared in S5 §6**, and named for the table it addresses rather than
    /// for the section's own spelling: §6's `subject_kind` enumeration lists
    /// `bulk_batch`, and this member is a different token, minted the way
    /// [`Self::Overlay`] was — the section's own rule is that a token with no
    /// writer is not declared, this gear has no writer for `bulk_batch`, and it
    /// does have one for `bulk_operation`. S5 §6's **aggregate** list (D-135's,
    /// not the subject-kind enumeration's) already names this member correctly:
    /// "plan, overlay, payer, policy, bulk operation" — the four words this
    /// variant is spelled from, and what
    /// [`audit_repo::bulk_operation_chain`](crate::infra::storage::repo::audit_repo::bulk_operation_chain)
    /// segments on.
    ///
    /// Its writer is the mass-repricing run's open
    /// (`crate::api::rest::repricing_runs`), inside the transaction that opens
    /// the `pricing_bulk_operation` row and freezes its journal — the debt that
    /// module's own doc named: opening a run wrote no audit record because there
    /// was no token for one. The bulk import's open owes the identical record
    /// still; this member gives it a token but only the repricing run has a
    /// writer so far.
    ///
    /// **Its approval-plane writer exists.** D-158 makes `pricing_approval` and
    /// `pricing_audit_log` one enumeration extended together, so
    /// `chk_pricing_approval_subject_kind`
    /// (`m20260821_000003_create_pricing_approval`) admits `bulk_operation` —
    /// storable, and here stored. The unit that opens one is
    /// `inst-bs-approval`'s batch approval, on the material edge out of
    /// `validating` (`api::rest::repricing_runs::advance_on_verdict`);
    /// `approval_repo::SUBJECT_KINDS_WITH_A_WRITER` carries it now.
    BulkOperation,
    /// One `pricing_group_membership` row, named by its `membership_id`
    /// (`design/09-price-overlays.md` `inst-mm-audit`).
    ///
    /// **Declared in S5 §6 already**, in the same enumeration D-158 takes
    /// verbatim from `pricing_approval` — `plan_revision | price_unit | window |
    /// overlay | membership | bundle | retirement | policy | bulk_batch`
    /// (`historical_import` was a tenth and left **both** enumerations together
    /// with D-330's strike, which is the narrowing half of the same rule that
    /// makes them extend together) — so this member implements a token the design
    /// set names
    /// rather than minting one, held out until its writer existed the way
    /// [`Self::Window`] and [`Self::Policy`] were.
    ///
    /// Its writer is
    /// [`group_membership_repo`](crate::infra::storage::repo::group_membership_repo)'s
    /// `enroll` and `end_membership`, inside each mutation's own transaction.
    ///
    /// Its **chain** is *not* the plan's and not [`Self::Overlay`]'s either: a
    /// membership has no plan and is not authored inside an overlay revision. S5
    /// §6's **aggregate** list — plan, overlay, **payer**, policy, bulk operation
    /// — names `payer` as the aggregate a membership mutation belongs to, so it
    /// segments on
    /// [`audit_repo::payer_chain`](crate::infra::storage::repo::audit_repo::payer_chain),
    /// one segment per payer: a payer's whole membership history — every
    /// enrollment, every ending, across every group it has ever moved through —
    /// is one segment to walk, which is what D-135 asks a chain key to be.
    Membership,
    /// One declared scope-value taxonomy entry — the subject of a governed edit
    /// (`PATCH /config/taxonomies/{class}/values/{value}`, D-353) and of the
    /// per-value audit records. Its own kind rather than `policy`, because the
    /// approval store's *"one pending policy proposal per tenant"* index and
    /// `find_pending_policy_unit` both key on that token, and a taxonomy edit
    /// must neither block a threshold proposal nor be read back as one.
    TaxonomyValue,
}

persisted_token_roster! {
    AuditSubjectKind,
    /// Every subject kind, stable order.
    ALL,
    /// The persisted `subject_kind` token.
    as_str,
    PlanRevision => "plan_revision",
    PriceUnit => "price_unit",
    Window => "window",
    Policy => "policy",
    Overlay => "overlay",
    BulkOperation => "bulk_operation",
    Membership => "membership",
    TaxonomyValue => "taxonomy_value",
}

/// Who caused a mutation and under which request — the audit fields a repository
/// cannot derive from the row it is writing.
///
/// Threaded through every mutating repository call rather than defaulted, and
/// there is **no unaudited form of any of them**: a second entry point that
/// skipped the record would make `pricing_audit_log` a trail that is *silently*
/// incomplete, which is strictly worse than one visibly absent because a reader
/// cannot tell "nobody changed this" from "this path does not write".
///
/// `recorded_at` is the **caller's** instant for [`AuditRecord`]'s reason: the
/// record is hashed over it, so a repository reading its own clock would make two
/// records of one transaction disagree about when the transaction happened.
#[domain_model]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AuditStamp {
    /// **Pseudonymous** principal id of the acting operator (`inst-au-pii`).
    pub actor_principal_id: Uuid,
    /// When the mutation was recorded, UTC — the caller's instant.
    pub recorded_at: OffsetDateTime,
    /// The correlation id of the causing request (D-178, `inst-au-complete`).
    ///
    /// **Not an `Option`, and that is the guard rather than a tidy-up.** D-178
    /// clause (1) makes the field *always satisfiable and never NULL* — the value
    /// the platform propagates inbound when there is one, minted at the gear's
    /// HTTP edge when there is not
    /// ([`crate::api::rest::correlation`]). A `None` here was the shape that let
    /// every authoring record write NULL while the type said the field was
    /// carried, so the absence is removed from the type instead of being
    /// forbidden by a convention: a caller that has no correlation cannot build a
    /// stamp, and a repository that takes a stamp is holding a real one.
    ///
    /// Clause (2) is what makes it worth threading rather than minting per
    /// record: **every** record and every `pricing_outbox` row one operator call
    /// produces carries **one** value. D-135 segments the chain per aggregate, so
    /// the two records a single `PATCH` writes when it opens a successor revision
    /// sit at two positions — and after Slice 12's bulk plane, on two segments —
    /// with this as the only thing saying they were one act.
    pub correlation_id: Uuid,
}

/// A mutated subject's before/after state, as the audit record's `jsonb` columns
/// hold it (`inst-au-complete`: before/after version refs).
///
/// One rendering for every audited subject in the crate — a plan revision, a
/// price row, a publish commit — because a second one is a second answer to
/// "what did this row look like". `pending_ref` is `Some` only on the publish
/// commit's after-state, which is what connects that mutation to the
/// addressability it produced; an auditor reading the record otherwise has the
/// flip and no way to reach the version it landed in.
///
/// Wire keys `camelCase`, as the outbox payload's are.
#[must_use]
pub fn subject_state(
    state: crate::domain::lifecycle::LifecycleState,
    row_version: u64,
    pending_ref: Option<&str>,
) -> serde_json::Value {
    match pending_ref {
        Some(pending) => serde_json::json!({
            "lifecycleState": state.as_str(),
            "rowVersion": row_version,
            "pendingVersionRef": pending,
        }),
        None => serde_json::json!({
            "lifecycleState": state.as_str(),
            "rowVersion": row_version,
        }),
    }
}

impl fmt::Display for AuditSubjectKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The hashing input for one audit record.
///
/// Borrows its string and JSON fields: the record is hashed on the way into the
/// store and never round-trips through this type, so copying every field to
/// hash it would be a copy taken for nothing. It carries `#[domain_model]`
/// because it is a `pub` domain type (DE0309), even though it never crosses the
/// repository boundary.
///
/// The field set is `inst-au-complete`'s: actor, timestamp, before/after refs,
/// approval trail, correlation id — plus the segment's own coordinates, which
/// is what binds a record to its position rather than merely to its content.
#[domain_model]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AuditRecord<'a> {
    /// The owning tenant.
    pub tenant_id: Uuid,
    /// The audited subject's aggregate — this record's chain segment.
    pub chain_id: Uuid,
    /// Position within the segment, `0` at genesis.
    pub seq: u64,
    /// When the mutation was recorded, UTC.
    pub recorded_at: OffsetDateTime,
    /// **Pseudonymous** principal id of the acting operator, never a display
    /// name and never an email (`inst-au-pii`).
    pub actor_principal_id: Uuid,
    /// What was done.
    pub action: AuditAction,
    /// What kind of thing it was done to.
    pub subject_kind: AuditSubjectKind,
    /// Which one — for a plan revision, the `plan_id/revision` reference.
    pub subject_ref: &'a str,
    /// The subject's before state, as the record's own `jsonb` column holds it.
    pub before_state: Option<&'a serde_json::Value>,
    /// The subject's after state.
    pub after_state: Option<&'a serde_json::Value>,
    /// The approval record the mutation ran under, when it had one.
    pub approval_ref: Option<Uuid>,
    /// The correlation id of the request that caused the mutation.
    ///
    /// **Still an `Option` where [`AuditStamp`]'s is not**, deliberately: this is
    /// the *hashing* input, and the encoding has to span everything the column
    /// can hold. `pricing_audit_log.correlation_id` is nullable, so a
    /// verification job re-walking a row written before D-178 must be able to
    /// rebuild its preimage — and it cannot if the only representable value is a
    /// present one. What D-178 removes is the ability of a **writer** to produce
    /// NULL, not the ability of a **verifier** to read one.
    pub correlation_id: Option<Uuid>,
}

/// The genesis `prev_hash` of a `(tenant_id, chain_id)` segment.
///
/// Bound to both, and never NULL, so a segment's first row carries a real link
/// rather than an absence a verifier has to interpret. See the module doc for
/// what a tenant-only seed would let somebody transplant.
#[must_use]
pub fn genesis_prev_hash(tenant_id: Uuid, chain_id: Uuid) -> [u8; 32] {
    let mut buf = Vec::with_capacity(96);
    buf.extend_from_slice(AUDIT_DOMAIN_SEP);
    put_uuid(&mut buf, tenant_id);
    put_uuid(&mut buf, chain_id);
    put_str(&mut buf, "GENESIS");
    digest32(&buf)
}

/// `row_hash = SHA-256(domain_sep || fields || prev_hash)`.
///
/// `prev_hash` is the predecessor row's `row_hash`, or [`genesis_prev_hash`] at
/// `seq = 0`. SHA-256 comes from the FIPS-validated `aws-lc-rs` provider the
/// platform installs; `sha2` is blocked by dylint DE0708.
///
/// Every field is length-prefixed and NULL-safe, which is not decoration: it is
/// what makes two different field **boundaries** unable to collide. Without the
/// prefixes a record whose `action` is `ab` and `subject_kind` is `c` hashes
/// identically to one whose fields are `a` and `bc`, and a chain that cannot
/// distinguish those can be forged by moving a character across a field border
/// with every link still verifying.
///
/// **There are two hazards of that shape and the prefixes close one.** A field
/// that is not in the preimage at all is a dimension along which a record can be
/// altered with every link still verifying, and no framing rule can see it — only
/// the field list can, so the field list is not written down. It is destructured
/// out of [`AuditRecord`] in the body, exhaustively and with no `..`, so a
/// thirteenth field stops this function compiling. `changing_any_single_field_-`
/// `changes_the_hash` is the behavioural half and cannot be the guard: a
/// hand-maintained mutation table can only fail for the fields it already knows
/// about.
///
/// # Errors
///
/// The `serde_json::Error` from serializing a canonicalized `before_state` or
/// `after_state`. Unreachable for an in-memory `Value`, and propagated rather
/// than degraded to a fixed byte string all the same: a silent fallback would
/// make an un-canonicalizable record hash **identically** to one with no state
/// at all, which is a collision target rather than a fallback. A record whose
/// content cannot be canonicalized must never be sealed into the chain.
pub fn audit_row_hash(
    record: &AuditRecord<'_>,
    prev_hash: &[u8; 32],
) -> Result<[u8; 32], serde_json::Error> {
    // **Destructured, with no rest pattern, and that is the membership guard.**
    // The paragraph above closes the *boundary* hazard — two adjacent fields
    // cannot be re-split, because every one carries its length. It says nothing
    // about a field that never enters the preimage at all, and that is the other
    // way a record is alterable with every link still verifying. Twelve
    // `record.field` reads could not tell the two apart: a thirteenth field would
    // compile here, hash to nothing, and leave every chain in the fleet verifying
    // over content it does not cover.
    //
    // The type has grown once already. `correlation_id` arrived with D-178 and
    // its `Option` was reasoned about carefully — see its own doc — because a
    // human noticed, not because anything forced it. This is what forces it.
    let AuditRecord {
        tenant_id,
        chain_id,
        seq,
        recorded_at,
        actor_principal_id,
        action,
        subject_kind,
        subject_ref,
        before_state,
        after_state,
        approval_ref,
        correlation_id,
    } = record;

    let mut buf = Vec::with_capacity(320);
    buf.extend_from_slice(AUDIT_DOMAIN_SEP);

    put_uuid(&mut buf, *tenant_id);
    put_uuid(&mut buf, *chain_id);
    put_u64(&mut buf, *seq);
    put_i64(
        &mut buf,
        crate::domain::instant::timestamp_micros(*recorded_at),
    );
    put_uuid(&mut buf, *actor_principal_id);
    put_str(&mut buf, action.as_str());
    put_str(&mut buf, subject_kind.as_str());
    put_str(&mut buf, subject_ref);
    put_opt_json(&mut buf, *before_state)?;
    put_opt_json(&mut buf, *after_state)?;
    put_opt_uuid(&mut buf, *approval_ref);
    put_opt_uuid(&mut buf, *correlation_id);

    put(&mut buf, prev_hash);
    Ok(digest32(&buf))
}

// ---------------------------------------------------------------------------
// Length-prefixed, NULL-safe framing primitives.
//
// The ledger's `domain/canonical.rs` discipline, copied rather than imported:
// this gear does not depend on that crate, and a shared helper would put two
// gears' frozen encodings behind one edit.
// ---------------------------------------------------------------------------

/// A present field: marker, u32-BE length, bytes.
fn put(buf: &mut Vec<u8>, bytes: &[u8]) {
    buf.push(PRESENT);
    // Saturating rather than failing: a field longer than 4 GiB cannot arise
    // from any column this gear writes, and a truncated length would still be
    // hashed together with the bytes that follow it.
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

fn put_i64(buf: &mut Vec<u8>, value: i64) {
    put(buf, &value.to_be_bytes());
}

/// A `jsonb` field, hashed over its **canonical** bytes.
fn put_opt_json(
    buf: &mut Vec<u8>,
    value: Option<&serde_json::Value>,
) -> Result<(), serde_json::Error> {
    if let Some(value) = value {
        let bytes = serde_json::to_vec(&canonicalized(value))?;
        put(buf, &bytes);
    } else {
        put_none(buf);
    }
    Ok(())
}

/// Rebuild `value` with every object's keys in sorted order, recursively.
///
/// The `jsonb` columns are hashed over this rather than over whatever byte
/// order a serializer happened to emit. Two reasons, and the second is the one
/// that bites: `jsonb` does not preserve key order at all, so a value written
/// one way and read back another is the normal case; and in a monorepo build
/// another crate may enable `serde_json/preserve_order`, which turns
/// `serde_json::Map` into an insertion-ordered `IndexMap` and would otherwise
/// make this hash depend on how the caller happened to build the value. Either
/// way the verification job would report a break that never happened.
///
/// Array order is left alone — order is semantic in a JSON array.
fn canonicalized(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let mut entries: Vec<(&String, &serde_json::Value)> = map.iter().collect();
            entries.sort_unstable_by(|left, right| left.0.cmp(right.0));
            let mut out = serde_json::Map::with_capacity(map.len());
            for (key, nested) in entries {
                out.insert(key.clone(), canonicalized(nested));
            }
            serde_json::Value::Object(out)
        }
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.iter().map(canonicalized).collect())
        }
        other => other.clone(),
    }
}

/// The 32-byte SHA-256 digest of a preimage.
fn digest32(buf: &[u8]) -> [u8; 32] {
    let digested = sha256(&SHA256, buf);
    let mut out = [0_u8; 32];
    out.copy_from_slice(digested.as_ref());
    out
}

/// Render a digest as lowercase hex — the spelling the byte-repro vector and
/// every operator-facing diagnostic use.
#[must_use]
pub fn hex32(digest: &[u8; 32]) -> String {
    hex_bytes(digest)
}

/// The same rendering over a slice whose length the type does not fix.
///
/// The stored `content_hash` of an approval unit is a `Vec<u8>` — the column is
/// `bytes`, and a batch subject's pin is a digest *over a set* (§6), so the
/// length is the store's business rather than this function's. One rendering for
/// both, so a digest an operator reads in a diagnostic and the same digest in an
/// audit record are the same string.
#[must_use]
pub fn hex_bytes(digest: &[u8]) -> String {
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        out.push(hex_nibble(*byte >> 4));
        out.push(hex_nibble(*byte & 0x0f));
    }
    out
}

/// One lowercase hex digit. Total over its only two call sites, which hand it
/// the high and low nibble of a byte and so can never exceed 15.
fn hex_nibble(nibble: u8) -> char {
    char::from(if nibble < 10 {
        b'0' + nibble
    } else {
        b'a' + nibble - 10
    })
}

#[cfg(test)]
#[path = "audit_tests.rs"]
mod audit_tests;
