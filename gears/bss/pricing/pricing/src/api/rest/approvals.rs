//! The approval surface — `design/05-governance.md` §5's four approval rows:
//! the reviewer's queue, the record, and the three decisions
//! (`inst-ap-decide`, `inst-as-approve`, `inst-as-reject`, `inst-as-void`).
//!
//! # The `GET` on one record carries the **pinned content**, and that is the
//! # whole reason it exists
//!
//! §3's reviewability invariant (D-61) is explicit: *"`GET
//! /bss-pricing/v1/approvals/{id}` MUST return the **pinned content** the
//! approval's `content_hash` covers (not the hash alone), so approval is never
//! hash-blind even where the subject resource is read-restricted"*. A reviewer
//! handed 32 bytes and an `approve` button is a signature on a digest, and the
//! two-person rule then certifies that somebody clicked.
//!
//! **What the store can and cannot give.** §6's `pricing_approval` carries a
//! `content_hash` and **no content column**, so there is no pinned document to
//! return: the only content this gear holds is the subject itself. So
//! [`ApprovalDetailView`] carries the subject **re-derived now** plus
//! [`ApprovalDetailView::content_matches_pin`], and the flag is not decoration —
//! it is the difference between "this is what was signed for" and "this is what
//! is there". On a `submitted` unit the two agree or the TOCTOU guard has
//! already voided the record, which is exactly the state a reviewer decides in.
//! Reported as a divergence rather than papered over: satisfying D-61 literally
//! needs a content column §6 does not declare.
//!
//! The rendering is the **authoring plane's own** — [`PlanPhaseView`],
//! [`AddonRuleView`], [`DescriptorSetView`], [`FrequencyView`],
//! [`PriceRowView`], [`ScopeKeyView`] — plus [`WindowIntervalView`], which is
//! `GET …/coverage`'s, so a reviewer reads the change set in the same shape the
//! author wrote it and the operator inspects it. A second rendering of one fact is
//! a second answer to it.
//!
//! [`PinnedContentView`] carries exactly the fields the pin hashes and no
//! others. `evaluated_at` and `baseline` are outside the digest
//! (`domain::approval::content_pin`'s module doc argues both), so putting them
//! here would show a reviewer content their signature does not cover. **The
//! conversion is an exhaustive destructure** and that is what keeps the sentence
//! true: under dot-access `PlanShape::windows` walks straight past it into the pin.
//!
//! # The three decisions declare no precondition header, and that is decided
//!
//! §5's idempotency cell for all three reads *per decision*, and the decision is
//! at-most-once **by construction**: `approval_repo`'s compare-and-swap carries
//! `state = 'submitted'` in its own predicate, so the second arrival is refused
//! `APPROVAL_NOT_PENDING` (409) whether it is a retry or a race. An `If-Match`
//! would be a second answer to a question the store already answers, over a
//! record that carries no version column at all; an `Idempotency-Key` would
//! store a response body for a replay the state machine makes unreachable. The
//! `POST`s therefore declare neither, and `tests/module_test.rs` asserts the
//! read routes declare none either.
//!
//! # `withdraw` is gated `approval × approve`, and no default role can reach it
//!
//! §3's endpoint map has one row for this whole path — *"`GET/POST
//! /bss-pricing/v1/approvals*` (S5) | `approval × read` / `approve`"* — so a
//! `POST` here is `approve`, and that is what this router enforces.
//!
//! **It is also a contradiction inside the design set, and it is reported
//! rather than resolved by widening the gate.** `inst-as-void` names the
//! withdrawer as *"the submitter (or a `CatalogAdmin`)"*; the role matrix in the
//! same section gives `CatalogAdmin` `approval × read` and deliberately **not**
//! `approval × approve` (*"it publishes, it does not approve itself"*), and a
//! submitter holds `plan × publish` under `FinanceManager` or `CatalogAdmin`,
//! neither of which carries `approve` either. So under the default matrix the
//! only principal who can withdraw a unit is a `FinanceReviewer`, who is neither
//! of the two the transition names. Gating on `read` instead would let anyone
//! who can see a unit close it, which is worse and is not what the map says.

use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Extension, Path, Query, RawQuery};
use axum::{Json, Router, http::HeaderMap, http::StatusCode};
use bss_pricing_sdk::odata::ApprovalFilterField;

use toolkit::api::canonical_prelude::CanonicalError;
use toolkit::api::odata::OData;
use toolkit::api::operation_builder::OperationBuilderODataExt;
use toolkit::api::{OpenApiRegistry, operation_builder::OperationBuilder};
use toolkit_db::secure::AccessScope;
use toolkit_odata::Page;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::api::rest::auth_context::{audit_stamp, require_authenticated};
use crate::api::rest::correlation::{CorrelationId, require_correlation};
use crate::api::rest::error::authz_error_to_canonical;
use crate::api::rest::odata_list::{map_odata_page_err, reject_non_odata_list_params};
use crate::api::rest::plans::{
    AddonRuleView, DescriptorSetView, FrequencyView, PeriodFloorCapView, PlanPhaseView,
};
use crate::api::rest::preconditions;
use crate::api::rest::prices::{PriceRowView, ScopeKeyView};
use crate::api::rest::state::GovernanceState;
use crate::api::rest::windows::WindowIntervalView;
use crate::domain::instant::rfc3339;
use std::collections::BTreeMap;
use time::OffsetDateTime;

use crate::domain::approval::{ApprovalState, DecisionBy, WithdrawAuthority};
use crate::domain::audit::AuditSubjectKind;
use crate::domain::bulk::BulkState;
use crate::domain::contracts::{EntitlementGrants, GrantSet, PlanChangeContract};
use crate::domain::error::DomainError;
use crate::domain::materiality::triggers::Trigger;
use crate::domain::materiality::{
    MaterialityReason, MaterialityVerdict, ThresholdBasis, ThresholdEntry, ThresholdVersion,
};
use crate::domain::plan_shape::{CompositeMeter, PlanShape};
use crate::domain::window::KeyWindows;
use crate::infra::approval::{ApprovalDetail, DecideRequest, PinnedSubject, RegionGrant};
use crate::infra::approval_participants::ParticipantName;
use crate::infra::storage::repo::approval_repo::ApprovalRecord;
use crate::infra::storage::repo::bulk_repo;

/// `OpenAPI` tag applied to every approval operation (DE0205).
const TAG: &str = "BSS Pricing Approvals";

/// The reviewer's queue.
///
/// The literal is repeated in the `OperationBuilder` call below because DE0801
/// validates a **literal** argument and silently passes a `const` one, so the
/// route-shape rule only binds where the literal is; the two spellings are
/// pinned together by `tests/module_test.rs`'s route census.
pub const APPROVALS: &str = "/bss-pricing/v1/approvals";
/// State totals over the full authorized queue.
pub const APPROVAL_COUNTS: &str = "/bss-pricing/v1/approvals/counts";
/// One approval record, with the content its pin covers (D-61).
pub const APPROVAL: &str = "/bss-pricing/v1/approvals/{approvalId}";
/// The approve action, as a sub-resource segment (D-140: never a colon method).
pub const APPROVAL_APPROVE: &str = "/bss-pricing/v1/approvals/{approvalId}/approve";
/// The reject action.
pub const APPROVAL_REJECT: &str = "/bss-pricing/v1/approvals/{approvalId}/reject";
/// The withdraw action (`inst-as-void`).
pub const APPROVAL_WITHDRAW: &str = "/bss-pricing/v1/approvals/{approvalId}/withdraw";

// ---------------------------------------------------------------------------
// Views and requests.
// ---------------------------------------------------------------------------

/// State totals, not the contents of one page. Withdrawals count as `voided`.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct ApprovalCountsView {
    /// Total number of visible approvals.
    pub total: u64,
    /// Awaiting review.
    pub submitted: u64,
    /// Approved.
    pub approved: u64,
    /// Rejected.
    pub rejected: u64,
    /// Withdrawn or automatically voided.
    pub voided: u64,
}

/// The evaluator's verdict, as it is stored and as it is read back.
///
/// **One shape for both**, which is why it is here rather than split between a
/// storage struct and a wire struct: `pricing_approval.materiality` is free-form
/// `jsonb` and §6 describes its payload — *"per-currency deltas, tripped rows,
/// trigger source"* — without declaring a schema. Two renderings of one column
/// would be two answers to what it holds.
///
/// **All three of the fields §6 names are carried now** — *"per-currency deltas,
/// tripped rows, trigger source"*. This doc claimed all three were absent, then
/// that two of them were; [`Self::tripped`] paid the first two (D-187) and
/// [`Self::trigger`] pays the third. Neither was a widening for completeness:
/// each closes a question a reviewer of a stored unit could not answer.
///
/// # Adding a member to this shape is a wire change and **not** a pin change
///
/// The two are easy to confuse and the register has already paid for confusing
/// them once. `pricing_approval.materiality` is a free-form `jsonb` column the
/// evaluator writes **about** the subject; the content pin is a digest over the
/// subject **itself** — `content_pin::content_hash` frames a `PlanShape` and
/// nothing else, and `bundle_content_hash`, the overlay, threshold, membership and
/// repricing pins each frame their own subject. No preimage in that module reads a
/// verdict, so no pending unit's digest moves when this shape gains a field and
/// `CONTENT_PIN_DOMAIN_SEP` does not move, whatever generation it stands at. (The
/// generation is deliberately not spelled here: the invariant is what the argument
/// needs and the literal is the only part that goes stale — `content_pin.rs`
/// records that failure mode about itself a dozen lines from the constant.)
/// `approval_repo::re_derive` re-derives
/// the **subject**, never this column, so no open unit becomes
/// `APPROVAL_CONTENT_MISMATCH` either.
///
/// What it **is** is a read-compatibility question, and the answer is the reason
/// this type is `(request, response)`: `ApprovalView::from` parses the stored
/// document back through [`materiality_of`], so a document that will not parse
/// becomes a `null` materiality on the reviewer's screen rather than an error — and
/// is told from an absent one in the log, which is the only place the two differ. Every member added here is therefore `Option`, which serde reads as
/// `None` when the key is absent — `approvals_tests::a_stored_verdict_written_before_the_trigger_member_still_parses`
/// is what holds that, over the exact bytes a unit minted before the widening carries.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request, response)]
pub struct MaterialityView {
    /// Whether a second principal is required.
    pub material: bool,
    /// Which rule fired: `noConfiguredThreshold` | `firstPublish` |
    /// `rowWithoutBaseline` | `alwaysMaterialTrigger` | `thresholdReached`. Absent on
    /// an auto-publishable change.
    ///
    /// The roster is [`MaterialityReason::ALL`]'s and is transcribed rather than
    /// counted: a count beside a roster leaves exactly one of the two true, and a
    /// list of three against four existing arms is how it reads when it goes wrong.
    pub reason: Option<String>,
    /// The row that reached its bar, in its own currency, and by how much — §6's
    /// *"per-currency deltas, tripped rows"* (D-187). Present only under
    /// `thresholdReached`, which is the only reason a **row** answered: the fail-safe is
    /// about the policy and `alwaysMaterialTrigger` about the act, so on those there is
    /// no row that tripped and naming one would mislead a reviewer.
    pub tripped: Option<TrippedRowView>,
    /// §6's **trigger source**: which registered always-material trigger fired.
    /// Present exactly under `alwaysMaterialTrigger`, absent under every other
    /// reason — a threshold or a fail-safe is not an act.
    ///
    /// The roster is `materiality::triggers::Trigger::as_str`'s, transcribed from
    /// the type that owns it for [`Self::reason`]'s reason. **It is not a second
    /// materiality vocabulary and not a wire code**: §5's Problem-response list is
    /// what a client branches on, and these tokens are the content of a column §6
    /// leaves unschematised — the argument `MaterialityReason::as_str` already
    /// makes for its own five.
    ///
    /// Why it is here at all: `subject_kind` is `bundle` for a component swap and
    /// `bundle` for a rev-share re-split, and the audit record of the act carries
    /// the same state pair for both. So the record could say a bundle publish
    /// needed two people and could not say **which act** required them — while
    /// D-104 registers two triggers rather than one precisely so that an operator
    /// *"should not have to infer"* whether what moved was the customer's
    /// composition or the vendor's payout.
    pub trigger: Option<String>,
}

/// §6's tripped row, as the wire and the stored `materiality` document render it.
///
/// The same document in both places by construction: this is what
/// `api::rest::windows::verdict_json` serialises into the column, so a reviewer reads
/// what was stored rather than a second rendering of the same verdict.
///
/// # The two amounts carry their [`Self::scale`], and are not converted to minor units
///
/// D-311 gave rates their own 10⁻⁹-minor-unit scale, and
/// `materiality::delta::MoveScale` rides the move so the comparer can raise the
/// **bar** into the operand's units rather than lower the operand into the bar's.
/// This document makes the same choice for the same reason, one layer out.
/// Converting would floor `$0.230777165` — a `per_unit` rate, which is the entire
/// reason `RateMinor` exists — to `0`, so a reviewer would read a real rate change
/// as no change at all; and the stored verdict is what an auditor re-computes years
/// later, which cannot be done from an operand recorded in units the comparison did
/// not use.
///
/// Absent, a rate move of `23_077_701_650 → 24_693_140_766` nano-minor renders
/// under a minor-unit label as `$230.7M → $246.9M` for a change of about `$0.23` —
/// a factor of `10⁹`, on the one screen the two-person rule exists to put in front
/// of a second principal, and `per_unit` rows reach this path.
// `(request, response)` because [`MaterialityView`] is: the stored `materiality`
// document is read **back** out of the column by the approvals surface, so every member
// of it has to parse as well as render. A response-only member would compile until the
// first read of a unit that tripped a row.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request, response)]
pub struct TrippedRowView {
    /// The price row whose move reached the bar — what an operator has to look at.
    pub price_id: Uuid,
    /// The currency whose bar it was. `inst-mat-percurrency` compares each row in its
    /// own currency, so this is part of *which bar was reached*.
    pub currency: String,
    /// The baseline amount, in the units [`Self::scale`] names — **not** always that
    /// currency's minor units, which is what this field's own doc used to claim.
    pub from_minor: i64,
    /// The proposed amount, same units.
    pub to_minor: i64,
    /// Which units the pair above is in: `minor` | `nanoMinor`.
    ///
    /// `minor` is the currency's own ISO-4217 minor unit — a `flat` amount or a
    /// `package` price. `nanoMinor` is 10⁻⁹ of one, the scale a `per_unit` rate and a
    /// tier band's rate are stored at (D-311). The roster is
    /// `materiality::delta::MoveScale::as_str`'s, transcribed from the type that owns
    /// it for [`MaterialityView::reason`]'s reason.
    pub scale: String,
}

/// **By reference, since the verdict stopped being `Copy`** (D-187): it carries a
/// currency, which is a heap value. A by-value `From` would make every render site a
/// move or a clone, and the two that render one verdict twice would have to clone it.
impl From<&MaterialityVerdict> for MaterialityView {
    fn from(verdict: &MaterialityVerdict) -> Self {
        Self {
            material: verdict.is_material(),
            reason: verdict
                .reason()
                .map(MaterialityReason::as_str)
                .map(str::to_owned),
            tripped: verdict.tripped().map(|tripped| TrippedRowView {
                price_id: tripped.price_id,
                currency: tripped.currency.as_str().to_owned(),
                from_minor: tripped.from_minor,
                to_minor: tripped.to_minor,
                scale: tripped.scale.as_str().to_owned(),
            }),
            trigger: verdict.trigger().map(Trigger::as_str).map(str::to_owned),
        }
    }
}

/// One approval record, as §6's columns stand. Mutation responses retain this
/// shape; GETs add current, independently authorized participant names.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct ApprovalView {
    /// The record's durable name.
    pub approval_id: Uuid,
    /// The pinned subject — `<planId>/<revision>` for a plan revision.
    pub subject_ref: String,
    /// What kind of thing the subject is (`plan_revision`).
    pub subject_kind: String,
    /// The pinned digest, lower-case hex. Carried **beside** the content rather
    /// than instead of it (D-61); it is what an auditor re-computes.
    pub content_hash: String,
    /// `submitted` | `approved` | `rejected` | `voided`.
    pub state: String,
    /// Who opened it — a pseudonymous principal id (`inst-au-pii`).
    pub submitter_principal: Uuid,
    /// Who approved or rejected it. `null` while pending **and on a void**: the
    /// column is `approver_principal`, a withdraw exercises no review authority,
    /// and `chk_pricing_approval_distinct_principals` refuses a submitter's own
    /// id there.
    pub approver_principal: Option<Uuid>,
    /// Mandatory on a reject; carried by the machine-driven voids too, which
    /// write why they closed.
    pub reason: Option<String>,
    /// The evaluator's verdict as stored. `null` when the column does not hold
    /// this shape — a later slice's writer, not an error.
    pub materiality: Option<MaterialityView>,
    /// When it was opened, UTC.
    #[serde(with = "rfc3339")]
    pub submitted_at: OffsetDateTime,
    /// When it was decided, UTC; `null` exactly while pending.
    #[serde(default, with = "rfc3339::option")]
    pub decided_at: Option<OffsetDateTime>,
}

impl From<&ApprovalRecord> for ApprovalView {
    fn from(record: &ApprovalRecord) -> Self {
        Self {
            approval_id: record.approval_id,
            subject_ref: record.subject_ref.clone(),
            subject_kind: record.subject_kind.as_str().to_owned(),
            content_hash: hex(&record.content_hash),
            state: record.state.as_str().to_owned(),
            submitter_principal: record.submitter_principal,
            approver_principal: record.approver_principal,
            reason: record.reason.clone(),
            materiality: materiality_of(record),
            submitted_at: record.submitted_at,
            decided_at: record.decided_at,
        }
    }
}

/// Availability of a current participant name, not the approval's state.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub enum ParticipantNameStatus {
    /// A nonblank name or username was returned by AM/IdP.
    Resolved,
    /// The caller is not authorized to read the profile in AM.
    Restricted,
    /// No user visible to this caller; does not assert deletion or account kind.
    NotFound,
    /// Missing/unsupported provider, source failure, timeout, or invalid profile.
    Unavailable,
}

/// Current identity presentation. Never persisted in Pricing or included in pins.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct ApprovalParticipantView {
    /// The same durable id as the corresponding `*_principal` field.
    pub principal_id: Uuid,
    /// Current display name, full name, or username from `IdP`; null unless resolved.
    pub display_name: Option<String>,
    /// Why a name is present or absent. Never treat null as an anonymous actor.
    pub name_status: ParticipantNameStatus,
}

impl ApprovalParticipantView {
    /// Project only the requested identity; no emails or provider errors escape.
    fn new(principal_id: Uuid, names: &BTreeMap<Uuid, ParticipantName>) -> Self {
        let (display_name, name_status) = match names.get(&principal_id) {
            Some(ParticipantName::Resolved(name)) => {
                (Some(name.clone()), ParticipantNameStatus::Resolved)
            }
            Some(ParticipantName::Restricted) => (None, ParticipantNameStatus::Restricted),
            Some(ParticipantName::NotFound) => (None, ParticipantNameStatus::NotFound),
            Some(ParticipantName::Unavailable) | None => (None, ParticipantNameStatus::Unavailable),
        };
        Self {
            principal_id,
            display_name,
            name_status,
        }
    }
}

/// The same enriched record in list items and `GET /approvals/{id}.approval`.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct ApprovalReadView {
    /// Stored metadata remains unchanged and flat on the wire.
    #[serde(flatten)]
    pub record: ApprovalView,
    /// Always present, including an explicit name-resolution failure.
    pub submitter: ApprovalParticipantView,
    /// Null when no reviewer is recorded, including a voided unit.
    pub approver: Option<ApprovalParticipantView>,
}

impl ApprovalReadView {
    /// Join request-local names onto an already-authorized record.
    fn new(record: &ApprovalRecord, names: &BTreeMap<Uuid, ParticipantName>) -> Self {
        Self {
            record: ApprovalView::from(record),
            submitter: ApprovalParticipantView::new(record.submitter_principal, names),
            approver: record
                .approver_principal
                .map(|id| ApprovalParticipantView::new(id, names)),
        }
    }
}

/// The identities stored on one record, without inventing a reviewer on a void.
fn participant_ids(record: &ApprovalRecord) -> impl Iterator<Item = Uuid> {
    std::iter::once(record.submitter_principal).chain(record.approver_principal)
}

/// The stored verdict, or `None` — and the two are told apart in the log.
///
/// A reviewer reads `materiality: null` either when the unit carries no verdict or
/// when the one it carries will not parse, and the second is a corrupt row: the
/// writer is this crate and the shape is its own. The wire keeps one spelling
/// because the field is a published DTO and a third state is an API change an owner
/// declares, so the discriminator goes where an operator can act on it. Silently,
/// the reviewer decides against a document the store could not render.
fn materiality_of(record: &ApprovalRecord) -> Option<MaterialityView> {
    if record.materiality.is_null() {
        return None;
    }
    match serde_json::from_value(record.materiality.clone()) {
        Ok(view) => Some(view),
        Err(e) => {
            tracing::error!(
                error = %e,
                approval_id = %record.approval_id,
                "bss-pricing: an approval's stored materiality verdict does not parse; the \
                 reviewer is shown none, and the record was written by this crate"
            );
            None
        }
    }
}

/// One composite meter as a reviewer sees it (Slice 10 §6).
///
/// A view rather than the domain type, on this module's convention: the wire
/// shape is the reviewer's document and is versioned by the API, not by the
/// domain.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct CompositeMeterView {
    /// Stable across revisions (D-106).
    pub composite_id: Uuid,
    /// The unit this composite rates as.
    pub output_unit: String,
    /// The constituent units it is built from.
    pub constituent_units: Vec<String>,
    /// The formula, verbatim as authored — this is the field the v11 pin covers
    /// and the one a reviewer is signing for.
    pub formula: serde_json::Value,
}

impl From<&CompositeMeter> for CompositeMeterView {
    fn from(meter: &CompositeMeter) -> Self {
        Self {
            composite_id: meter.composite_id,
            output_unit: meter.output_unit.clone(),
            constituent_units: meter.constituent_units.clone(),
            formula: meter.formula.clone(),
        }
    }
}

/// The entitlement grant set as a reviewer sees it (Slice 6, §6, D-41).
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct EntitlementGrantsView {
    /// The `PlanTier` policy the set resolved from, when it resolved from one.
    pub plan_tier_ref: Option<String>,
    /// The plan-level `featureFlag` entries.
    pub feature_flags: BTreeMap<String, bool>,
    /// The plan-level `quotaKey` entries.
    pub quotas: BTreeMap<String, i64>,
    /// The authored per-phase sets, keyed by `phaseId`. **The authored ones, not
    /// the materialized map**: what a reviewer signs is what an author wrote,
    /// and the complete map is derived from it at projection.
    pub per_phase: BTreeMap<Uuid, GrantSetView>,
}

/// One grant set on the wire.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct GrantSetView {
    /// `featureFlag: bool` entries.
    pub feature_flags: BTreeMap<String, bool>,
    /// `quotaKey: value` entries.
    pub quotas: BTreeMap<String, i64>,
}

impl From<&GrantSet> for GrantSetView {
    fn from(set: &GrantSet) -> Self {
        Self {
            feature_flags: set.feature_flags.clone(),
            quotas: set.quotas.clone(),
        }
    }
}

impl From<&EntitlementGrants> for EntitlementGrantsView {
    fn from(grants: &EntitlementGrants) -> Self {
        Self {
            plan_tier_ref: grants.plan_tier_ref.clone(),
            feature_flags: grants.plan_level.feature_flags.clone(),
            quotas: grants.plan_level.quotas.clone(),
            per_phase: grants
                .per_phase
                .iter()
                .map(|(id, set)| (*id, GrantSetView::from(set)))
                .collect(),
        }
    }
}

/// The plan-change contract as a reviewer sees it (Slice 6, §6).
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct PlanChangeContractView {
    /// The explicit published `planId`s a self-service change may travel to.
    /// **Absent means no self-service change** (`inst-pc-failsafe`), which is a
    /// value with a reading and not a missing field; an empty list is an author
    /// who stated the set and left it empty, and the two are kept apart.
    pub allowed_change_targets: Option<Vec<Uuid>>,
    /// The tenant-wide comparability rank (K4).
    pub comparability_rank: Option<i32>,
    /// `reset` | `carry` — D-113's tier-`Q` continuity flag.
    pub usage_counter_on_plan_change: String,
}

impl From<&PlanChangeContract> for PlanChangeContractView {
    fn from(contract: &PlanChangeContract) -> Self {
        Self {
            allowed_change_targets: contract.allowed_change_targets.clone(),
            comparability_rank: contract.comparability_rank,
            usage_counter_on_plan_change: contract.usage_counter_on_plan_change.as_str().to_owned(),
        }
    }
}

/// Exactly the plan content the pin hashes.
///
/// Every member is a field `content_pin::put_plan_shape` frames, and the two
/// fields of [`PlanShape`] it does **not** frame — `evaluated_at` and
/// `baseline` — are absent here for the same reason they are absent there: a
/// reviewer must not be shown content their signature does not cover.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct PinnedContentView {
    /// The plan under approval.
    pub plan_id: Uuid,
    /// Which revision of it.
    pub revision: u64,
    /// The catalog SKU this revision binds. Hashed, because a
    /// rebind inside the approve→commit window would otherwise re-derive to the
    /// same digest.
    pub sku_id: Option<Uuid>,
    /// `one_time` | `recurring` | `usage` | `hybrid`.
    pub billing_cycle: Option<String>,
    /// The recurring frequency, interval and all.
    pub frequency: Option<FrequencyView>,
    /// The plan's tier.
    pub plan_tier: Option<String>,
    /// The plan's human label (D-318).
    ///
    /// **Shown**, because the pin frames it: a rename is a change to what the
    /// catalog will call this plan, a reviewer's signature covers it, and the
    /// module doc above makes showing-or-not an explicit decision rather than an
    /// omission.
    pub plan_name: Option<String>,
    /// Whether the tier diverges from the parent SKU's under an audited
    /// override.
    pub plan_tier_override: bool,
    /// Start of the availability window, UTC.
    #[serde(default, with = "rfc3339::option")]
    pub available_from: Option<OffsetDateTime>,
    /// End of the availability window, UTC.
    #[serde(default, with = "rfc3339::option")]
    pub available_to: Option<OffsetDateTime>,
    /// Minimum purchasable quantity.
    pub purchase_min_qty: Option<u64>,
    /// Maximum purchasable quantity.
    pub purchase_max_qty: Option<u64>,
    /// The Billing invoice-layout hint (D-96).
    pub invoice_grouping_key: Option<String>,
    /// The phase chain.
    pub phases: Vec<PlanPhaseView>,
    /// The add-on composition rules.
    pub addon_rules: Vec<AddonRuleView>,
    /// The billing descriptor set.
    pub descriptor_set: Option<DescriptorSetView>,
    /// The candidate row set this publish would produce.
    pub rows: Vec<PriceRowView>,
    /// The plan-change contract this revision would publish (Slice 6, §6).
    ///
    /// **Shown because it is pinned.** The pin's module doc argues that showing
    /// a reviewer content their signature does not cover is the wrong direction;
    /// the mirror of that argument is that content the signature *does* cover
    /// must be on the document. An edge list decides who may move where, so a
    /// reviewer approving it unseen is approving an authorization change they
    /// were never shown.
    pub change_contract: PlanChangeContractView,
    /// The entitlement grant set this revision would publish (Slice 6, §6).
    ///
    /// Shown for the change contract's reason: it is pinned, so a reviewer's
    /// signature covers it, and a signature over unseen entitlements is one the
    /// reviewer did not give.
    pub entitlement_grants: EntitlementGrantsView,
    /// The composite meters this revision would publish (Slice 10 §6).
    ///
    /// Shown for the same reason as the two above and stated because it was
    /// **hashed and not shown for one commit** (D-259): the pin covers the
    /// formula since v11, so a reviewer's signature covers a weighting they were
    /// never displayed — which is precisely the signature they did not give. The
    /// module's invariant is "exactly the fields the pin hashes and no others".
    pub composites: Vec<CompositeMeterView>,
    /// The plan-level period floor/cap this revision would publish (S2 §6,
    /// **D-319**).
    ///
    /// Shown for the three above's reason, and it is the one with the most
    /// direct money consequence: the pin covers it since v14, so a reviewer's
    /// signature covers a minimum charge they were never displayed — and unlike
    /// a price row, a period floor changes what a subscriber pays without
    /// appearing as a line on the invoice that would explain it.
    pub period_floor_caps: Vec<PeriodFloorCapView>,
    /// The window plane the pin covers, one entry per canonical scope key.
    ///
    /// Hashed here, for the reason
    /// `sku_id` is: a field inside the digest and outside this document is a field
    /// a reviewer is told `content_matches_pin: false` about while looking at a
    /// page that does not contain it. Two plans differing **only** in their window
    /// intervals rendered byte-identical here and hashed differently.
    pub windows: Vec<PinnedWindowsView>,
}

/// One canonical scope key's window set, as the pinned document renders it.
///
/// The interval rendering is [`WindowIntervalView`] — `GET …/coverage`'s own, not
/// a second spelling of it — and the key is [`ScopeKeyView`], the authoring
/// plane's, which is what [`PriceRowView`] shows a reviewer a few lines above. A
/// second rendering of one fact is a second answer to it.
///
/// What it does **not** carry is a coverage end or a gap list: those are *derived*
/// from the intervals, the pin frames the intervals, and a document showing a
/// reviewer a derivation their signature does not cover is the failure this whole
/// view exists to avoid. `GET …/coverage` is where the derived answers live.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct PinnedWindowsView {
    /// The ten axes the windows are filed under.
    pub scope_key: ScopeKeyView,
    /// The key's intervals, in the order the pin frames them.
    pub intervals: Vec<WindowIntervalView>,
}

impl From<&KeyWindows> for PinnedWindowsView {
    fn from(group: &KeyWindows) -> Self {
        let KeyWindows {
            scope_key,
            intervals,
        } = group;
        Self {
            scope_key: ScopeKeyView::from(scope_key),
            intervals: intervals.iter().map(WindowIntervalView::from).collect(),
        }
    }
}

/// **An exhaustive destructure, and that is the guard** —
/// `domain::approval::content_pin`'s discipline, here for its reason.
///
/// A dot-access version of this function lets a new field — [`PlanShape::windows`]
/// was one — into the pin and past this view in the same commit:
/// `put_plan_shape`'s pattern refuses to compile and reports it,
/// `PlanSubjectDelta`'s `let Self {.. }` does the same, and the conversion says
/// nothing, because a field nobody reads produces no error. So the next field
/// added to the shape is an E0027 here as well, and
/// whoever adds it decides whether a reviewer is shown it instead of discovering
/// later that they were not.
///
/// The two the pin does not frame are named and discarded rather than skipped with
/// `..`, so the pattern stays a census.
impl From<&PlanShape> for PinnedContentView {
    fn from(shape: &PlanShape) -> Self {
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
            // Outside the digest, so outside this document: showing a reviewer
            // content their signature does not cover is what the pin's module doc
            // argues against for both of these.
            baseline: _,
            evaluated_at: _,
        } = shape;
        Self {
            plan_id: plan_id.get(),
            revision: *revision,
            sku_id: *sku_id,
            billing_cycle: billing_cycle.map(|cycle| cycle.as_str().to_owned()),
            frequency: frequency.map(FrequencyView::from),
            plan_tier: plan_tier.clone(),
            plan_name: plan_name.clone(),
            plan_tier_override: *plan_tier_override,
            available_from: *available_from,
            available_to: *available_to,
            purchase_min_qty: *purchase_min_qty,
            purchase_max_qty: *purchase_max_qty,
            invoice_grouping_key: invoice_grouping_key.clone(),
            phases: phases
                .phases()
                .iter()
                .cloned()
                .map(PlanPhaseView::from)
                .collect(),
            addon_rules: addon_rules
                .iter()
                .cloned()
                .map(AddonRuleView::from)
                .collect(),
            descriptor_set: descriptor_set.clone().map(DescriptorSetView::from),
            rows: rows.iter().map(PriceRowView::from).collect(),
            change_contract: PlanChangeContractView::from(change_contract),
            entitlement_grants: EntitlementGrantsView::from(entitlement_grants),
            composites: composites.iter().map(CompositeMeterView::from).collect(),
            period_floor_caps: period_floor_caps
                .iter()
                .cloned()
                .map(PeriodFloorCapView::from)
                .collect(),
            windows: windows.iter().map(PinnedWindowsView::from).collect(),
        }
    }
}

/// Exactly the threshold-policy version the pin hashes — the reviewer's document
/// for a D-10 unit.
///
/// A **second** pinned-content member rather than a widening of
/// [`PinnedContentView`], for the reason `PinnedSubject` is a sum type: the two
/// subjects share no field, and a single view carrying the union of both would
/// have every member `null` for one of the two kinds. A reviewer would then be
/// reading a document whose shape does not say what they are approving, and the
/// exhaustive-destructure guard below would have nothing to be exhaustive over.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct PinnedThresholdPolicyView {
    /// Which version of the tenant's policy this proposal is.
    pub version: u64,
    /// When its thresholds start applying, once approved.
    #[serde(with = "rfc3339")]
    pub effective_from: OffsetDateTime,
    /// The per-currency entries, in the order the pin frames them.
    pub entries: Vec<ThresholdEntryView>,
}

/// One currency's proposed threshold.
///
/// The two bases are two nullable members and **exactly one is set**, which is the
/// store's column shape rather than the domain's enum. The wire keeps the columns
/// because a generated client reads `absolute_minor` or `percent_bp` — the
/// spelling `toolkit_macros::api_dto` emits, `#[serde(rename_all = "snake_case")]`
/// unconditionally — and a tagged union would make the common case, read the
/// number, a two-step. Not `absoluteMinor`: four refusals in
/// `threshold_policy::parse_entries` are spelled against this. The invariant
/// is held one layer in, by `ThresholdBasis`, and it is the constructor that a
/// caller of the `PUT` meets.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request, response)]
pub struct ThresholdEntryView {
    /// The ISO 4217 code, uppercase.
    pub currency: String,
    /// The absolute threshold in the currency's minor units, or `null`.
    ///
    /// **§6's *"in minor units at the currency's ISO 4217 precision"* is a semantic
    /// anchor and not a validation this surface can perform**, and the distinction is
    /// worth stating rather than dismissing. As a *rule* about an integer already
    /// declared to be in minor units it constrains nothing — there is no value of this
    /// field a precision check could refuse, which is why `parse_entries` has no arm
    /// for it. What it does fix is what the number **means**: `50000` on `EUR` is
    /// €500.00 and not €50000, and on a zero-decimal currency (`JPY`, `KRW`) minor
    /// units *are* major units, so the same integer is ¥50000. That is what a client
    /// authoring a threshold and an auditor reading one both need, and it is the same
    /// anchor `MinorAmount` carries on a price row. The scale itself is another gear's
    /// reference data (`ledger_currency_scale_registry`), which this gear reaches
    /// through SDK clients and never through a schema.
    pub absolute_minor: Option<i64>,
    /// The relative threshold in **basis points** (`10000` = 100%), or `null`.
    pub percent_bp: Option<u32>,
}

/// **An exhaustive destructure, and that is the guard** — [`From<&PlanShape>`]'s
/// discipline, applied to the second subject on the day it arrived rather than
/// after a field went missing from a reviewer's document.
///
/// [`ThresholdEntry`] is destructured; [`ThresholdVersion`] cannot be, its fields
/// being private behind the constructor that refuses an empty or duplicated entry
/// set — which is the exposure `content_pin`'s module doc counts and carries the
/// same obligation here: a fourth field on the version is a field this view will
/// silently omit unless somebody adds it.
///
/// [`From<&PlanShape>`]: PinnedContentView
/// One taxonomy value before and after a proposed edit (D-353).
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct PinnedTaxonomyValueView {
    /// The universe: `region`, `brand`, `partner` or `org_tier`.
    pub class: String,
    /// The value being edited.
    pub value: String,
    /// The value as it stands.
    pub before: crate::api::rest::taxonomies::TaxonomyValueView,
    /// The value as the edit would leave it.
    pub after: crate::api::rest::taxonomies::TaxonomyValueView,
}

impl From<&crate::domain::taxonomy::TaxonomyValueChange> for PinnedTaxonomyValueView {
    fn from(change: &crate::domain::taxonomy::TaxonomyValueChange) -> Self {
        Self {
            class: change.proposal.class.path_segment().to_owned(),
            value: change.proposal.value.as_str().to_owned(),
            before: crate::api::rest::taxonomies::view_of(&change.held),
            after: crate::api::rest::taxonomies::view_of(&change.next()),
        }
    }
}

impl From<&ThresholdVersion> for PinnedThresholdPolicyView {
    fn from(version: &ThresholdVersion) -> Self {
        Self {
            version: version.version(),
            effective_from: version.effective_from(),
            entries: version
                .entries()
                .iter()
                .map(|entry| {
                    let ThresholdEntry { currency, basis } = entry;
                    let (absolute_minor, percent_bp) = match *basis {
                        ThresholdBasis::Absolute { minor } => (Some(minor), None),
                        ThresholdBasis::Percent { bp } => (None, Some(bp)),
                    };
                    ThresholdEntryView {
                        currency: currency.as_str().to_owned(),
                        absolute_minor,
                        percent_bp,
                    }
                })
                .collect(),
        }
    }
}

/// One record **and what its pin covers** — D-61.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct ApprovalDetailView {
    /// The record.
    pub approval: ApprovalReadView,
    /// The pinned **plan**, re-derived. `null` when the unit is not about a plan,
    /// and when it is but can no longer be derived at all — the draft published,
    /// or was abandoned.
    pub pinned_content: Option<PinnedContentView>,
    /// The pinned **threshold-policy version**, re-derived. `null` on every unit
    /// that is not a D-10 policy proposal.
    ///
    /// At most one of this and `pinned_content` is ever set, and which one is a
    /// fact about `approval.subject_kind`. Two members rather than one polymorphic
    /// one because the two documents share no field; see
    /// [`PinnedThresholdPolicyView`].
    pub pinned_threshold_policy: Option<PinnedThresholdPolicyView>,
    /// **The act this unit is being decided on**, read off the record's own
    /// subject. `null` on every unit that is not a window mutation.
    ///
    /// D-61 asks that a reviewer read what they are signing for, and for a window
    /// unit `pinned_content` alone cannot answer it: the pinned subject is the plan
    /// as it already stands, so a cancel and a lengthening render the same document.
    /// This names the operation and the interval it moves. It is not part of the
    /// digest and does not need to be — it rides `subject_ref` on an append-only
    /// store, so nothing can move it after the signature.
    pub proposed_act: Option<ProposedActView>,
    /// The pinned **composition**, on a `bundleComposition` unit (D-104). `null`
    /// on every other kind.
    ///
    /// `pinned_content` carries the plan the composition rides and says nothing
    /// about the component set or the revenue split, so without this a reviewer of
    /// the one act that divides third-party money approved a document in which the
    /// act was invisible. D-61 in its own terms: the pinned content, not the hash.
    pub pinned_composition: Option<PinnedCompositionView>,
    /// The pinned **mass-repricing run**, on a `bulkOperation` unit
    /// (`inst-bs-approval`). `null` on every other kind.
    ///
    /// The run's frozen `report` — selector, adjustment, changeover instant and
    /// selected-row count — which is the document
    /// [`repricing_run_content_hash`](crate::domain::approval::content_pin::repricing_run_content_hash)
    /// pins and therefore the one a reviewer is signing for. D-61 in its own
    /// terms, and it is not decorative here: a run spans plans, so
    /// `pinned_content` is structurally `null` on this kind, and without this
    /// member the reviewer of a tenant-wide reprice was shown a digest and
    /// nothing else. The run's own `GET` is not the escape either — it is
    /// addressed by the caller's `run_id`, and the unit's `subject_ref` carries
    /// the *minted* `operation_id`, which that route does not take.
    ///
    /// Rendered as the stored value rather than re-typed: it is this crate's own
    /// controlled rendering (`api::rest::repricing_runs::frozen_report`), and a
    /// second projection of it here would be a second answer to what the run was
    /// accepted as.
    pub pinned_repricing_run: Option<serde_json::Value>,
    /// The pinned **taxonomy-value edit**, on a `taxonomy_value` unit (D-353).
    /// `null` on every other kind — and on a taxonomy-value unit whose value is
    /// no longer declared, in which case `content_matches_pin` is `false`.
    ///
    /// `before` is the value as the reviewer must see it stand, `after` as the
    /// edit would leave it; the pin covers both, so a relabel landing under the
    /// unit shows here as a mismatch rather than as a silently different `after`.
    pub pinned_taxonomy_value: Option<PinnedTaxonomyValueView>,
    /// Whether the pinned content above still digests to `approval.content_hash`.
    ///
    /// **Read this before deciding.** `false` means the document above is *not*
    /// the one the pin was taken over, and an approve would be refused
    /// `APPROVAL_CONTENT_MISMATCH` (409). See the module doc for why the field
    /// exists at all rather than the stored content being returned.
    pub content_matches_pin: bool,
}

/// A reject: the mandatory reason (`inst-as-reject`).
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request, response)]
pub struct RejectApprovalRequest {
    /// Why the change is refused. Blank is absent — a `reason` column holding a
    /// space satisfies `chk_pricing_approval_reason` and tells an auditor
    /// nothing.
    pub reason: String,
}

/// A withdraw: an optional note.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request, response)]
pub struct WithdrawApprovalRequest {
    /// Why the unit is being closed without a decision. Optional — §4 makes no
    /// reason mandatory on this edge, and the machine-driven voids write their
    /// own.
    pub reason: Option<String>,
}

/// Build the Axum router for the approval surface and register its operations.
///
/// No route declares a 422: §3.3's status-rendering rule makes every
/// architectural 422 in the design set reach the wire as a 400 carrying its
/// code, so a 422 here would document a response no path can emit.
#[allow(
    clippy::too_many_lines,
    reason = "one builder chain per operation; flat is clearer than helpers that hide which route declares which response"
)]
pub fn router(state: Arc<GovernanceState>, openapi: &dyn OpenApiRegistry) -> Router {
    let mut router = Router::new();

    router = OperationBuilder::get("/bss-pricing/v1/approvals")
        .operation_id("bss_pricing.list_approvals")
        .summary("List the tenant's approval units (cursor-paginated)")
        .description(
            "One page of the tenant's approval records, in `approval_id` order, with an opaque \
             `cursor` and a `limit` whose server default is 100 and whose hard cap is 1,000 \
             (D-125). Narrow with `$filter=state eq 'submitted'` (or `approved` / `rejected` / \
             `voided`); omitting `$filter` returns every state, which is what a reviewer's queue \
             over pending **and** decided units asks for. The pinned content is **not** on this \
             page - a page of a hundred units would be a hundred plan assemblies - so a \
             reviewer opens `GET /bss-pricing/v1/approvals/{approvalId}` before deciding, which \
             is the surface D-61's reviewability invariant binds.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .query_param_typed(
            "limit",
            false,
            "Records per page (default 100, hard cap 1,000)",
            "integer",
        )
        .query_param("cursor", false, "Opaque base64url pagination cursor")
        .handler(list_approvals)
        .with_odata_filter::<ApprovalFilterField>()
        .with_odata_orderby::<ApprovalFilterField>()
        .json_response_with_schema::<Page<ApprovalReadView>>(
            openapi,
            StatusCode::OK,
            "One page of the tenant's approval units, with live AM/IdP participant names. \
             Profile access is separately authorized by AM; unresolved names have explicit \
             name_status. Cache-Control: private, no-store.",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/bss-pricing/v1/approvals/counts")
        .operation_id("bss_pricing.count_approvals")
        .summary("Count visible approval units by state")
        .description(
            "Totals over the whole approval-read scope, including resource ID restrictions. \
             No filtering or pagination parameters. Withdrawn units count as voided. \
             No participant profile lookup. Cache-Control: private, no-store.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .handler(count_approvals)
        .json_response_with_schema::<ApprovalCountsView>(openapi, StatusCode::OK, "State totals")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/bss-pricing/v1/approvals/{approvalId}")
        .operation_id("bss_pricing.get_approval")
        .summary("Read an approval unit and the content its pin covers")
        .description(
            "Returns the record **and the pinned content** - D-61's reviewability invariant, \
             which exists because deny-by-default otherwise turns the two-person rule into a \
             hash-blind signature: an approver who cannot read the subject resource can still \
             read what they are approving here. `pinned_content` carries exactly the fields the \
             content hash covers, and `content_matches_pin` says whether the subject as it \
             stands still digests to the pin. A record outside the caller's scope reads exactly \
             like an absent one (404, no existence leak) - what a pending unit tells an observer \
             is that a price change is in flight. `approval.submitter` and `approval.approver` \
             project current AM/IdP names under the caller's profile-read rights, with explicit \
             name_status on failure. Names are not pinned or stored in Pricing. \
             Cache-Control: private, no-store.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("approvalId", "The approval unit to read.")
        .handler(get_approval)
        .json_response_with_schema::<ApprovalDetailView>(
            openapi,
            StatusCode::OK,
            "The record and the content its pin covers.",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    router = OperationBuilder::post("/bss-pricing/v1/approvals/{approvalId}/approve")
        .operation_id("bss_pricing.approve_approval")
        .summary("Approve a pending unit as the independent second principal")
        .description(
            "Moves a `submitted` unit to `approved` (`inst-as-approve`). The submitter may not \
             be the approver - identity, never role, so one human holding both `plan x publish` \
             and `approval x approve` is still refused `SELF_APPROVAL_FORBIDDEN` (403), and the \
             attempt is written to the audit trail as a `deny` record before the refusal is \
             returned (`inst-tp-selfaudit`). The pinned content hash is re-verified against the \
             subject as it now stands; a mismatch is `APPROVAL_CONTENT_MISMATCH` (409) - a \
             reviewer can only ever approve exactly what they saw. A record that has already \
             been decided or voided is `APPROVAL_NOT_PENDING` (409). The decision needs no \
             precondition header: the store's compare-and-swap carries `state = 'submitted'`, \
             so the second arrival is refused whether it is a retry or a race. For a \
             `taxonomy_value` unit, approval also applies the pinned patch and its audit in \
             this transaction: a stale pin or failed domain guard rolls back the decision. \
             No second PATCH or additional `config x write` role is required. Plan approvals \
             still do not publish; `POST /bss-pricing/v1/plans/{planId}/publish` does and \
             re-verifies the pin at commit.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("approvalId", "The pending unit to approve.")
        .handler(approve_approval)
        .json_response_with_schema::<ApprovalView>(
            openapi,
            StatusCode::OK,
            "The record as it stands after the decision.",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    router = OperationBuilder::post("/bss-pricing/v1/approvals/{approvalId}/reject")
        .operation_id("bss_pricing.reject_approval")
        .summary("Reject a pending unit, with its mandatory reason")
        .description(
            "Moves a `submitted` unit to `rejected` (`inst-as-reject`), returning the plan's \
             draft revision to the author. The `reason` is **mandatory** and a blank one is \
             refused `REASON_REQUIRED` (400 carrying the code - the design set types it 422 and \
             the canonical family has no such category, so the code is the discriminator). The \
             two-person rule and the approver's scope apply exactly as they do to an approve: a \
             reject returns the plan to `draft`, so it is not the harmless direction, and a \
             reviewer denied the authority to approve a change is denied the authority to unwind \
             it. The pin is **not** re-verified - the subject returns to `draft` either way, so \
             there is nothing a mismatch would protect.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("approvalId", "The pending unit to reject.")
        .json_request::<RejectApprovalRequest>(openapi, "The mandatory reason.")
        .handler(reject_approval)
        .json_response_with_schema::<ApprovalView>(
            openapi,
            StatusCode::OK,
            "The record as it stands after the decision.",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    router = OperationBuilder::post("/bss-pricing/v1/approvals/{approvalId}/withdraw")
        .operation_id("bss_pricing.withdraw_approval")
        .summary("Withdraw a pending unit without deciding it")
        .description(
            "Moves a `submitted` unit to `voided` (`inst-as-void`) **and frees the subject it \
             held**: a pending unit holds its plan's subject under \
             `PENDING_CHANGE_UNIT_EXISTS`, so without this path the only escape was mutating \
             the subject's content. A fresh submit afterwards opens a **new** record - approval \
             records are immutable once decided (`inst-as-immutable`) and there is no re-open. \
             The two-person rule does **not** apply here: a withdraw's actor is the submitter, \
             which is exactly who S4 names, so a distinctness rule would make the escape hatch \
             unreachable by the person it is for. `approver_principal` therefore stays null - a \
             withdraw exercises no review authority. An already-decided record is \
             `APPROVAL_NOT_PENDING` (409).",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("approvalId", "The pending unit to withdraw.")
        .json_request::<WithdrawApprovalRequest>(openapi, "An optional note.")
        .handler(withdraw_approval)
        .json_response_with_schema::<ApprovalView>(openapi, StatusCode::OK, "The voided record.")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    // D-178's edge, applied here rather than where the routers are merged so it
    // travels with the routes: a surface reachable without it cannot build an
    // `AuditStamp`, and `correlation::require_correlation` answers 500 rather
    // than minting a second value per record.
    router
        .layer(Extension(state))
        .layer(axum::middleware::from_fn(
            crate::api::rest::correlation::establish,
        ))
}

// ---------------------------------------------------------------------------
// Handlers.
// ---------------------------------------------------------------------------

/// `GET /approvals`.
#[allow(clippy::implicit_hasher)]
async fn list_approvals(
    Extension(state): Extension<Arc<GovernanceState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    Query(extras): Query<HashMap<String, String>>,
    OData(odata): OData,
) -> Result<(HeaderMap, Json<Page<ApprovalReadView>>), CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    reject_non_odata_list_params(&extras)?;
    let scope = read_scope(&enforcer, &ctx, None).await?;
    let page = state
        .approvals
        .list_odata(&scope, ctx.subject_tenant_id(), &odata)
        .await
        .map_err(map_odata_page_err)?;
    let names = state
        .participants
        .resolve(&ctx, page.items.iter().flat_map(participant_ids))
        .await;
    Ok((
        participant_read_headers(),
        Json(Page {
            items: page
                .items
                .iter()
                .map(|record| ApprovalReadView::new(record, &names))
                .collect(),
            page_info: page.page_info,
        }),
    ))
}

/// `GET /approvals/counts`; no list filters and no participant resolution.
async fn count_approvals(
    Extension(state): Extension<Arc<GovernanceState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    RawQuery(query): RawQuery,
) -> Result<(HeaderMap, Json<ApprovalCountsView>), CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    let scope = read_scope(&enforcer, &ctx, None).await?;
    if query.is_some_and(|text| !text.is_empty()) {
        return Err(CanonicalError::from(DomainError::InvalidRequest(
            "approval counts does not accept query parameters".to_owned(),
        )));
    }
    let counts = state
        .approvals
        .counts(&scope, ctx.subject_tenant_id())
        .await
        .map_err(|e| CanonicalError::from(crate::infra::storage::repo_failure(&e)))?;
    Ok((
        participant_read_headers(),
        Json(ApprovalCountsView {
            total: counts.total,
            submitted: counts.submitted,
            approved: counts.approved,
            rejected: counts.rejected,
            voided: counts.voided,
        }),
    ))
}

/// `GET /approvals/{approvalId}`.
async fn get_approval(
    Extension(state): Extension<Arc<GovernanceState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    Path(approval_id): Path<Uuid>,
) -> Result<(HeaderMap, Json<ApprovalDetailView>), CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    let scope = read_scope(&enforcer, &ctx, Some(approval_id)).await?;

    let detail = state
        .approvals
        .find(
            &scope,
            ctx.subject_tenant_id(),
            approval_id,
            OffsetDateTime::now_utc(),
        )
        .await
        .map_err(CanonicalError::from)?
        .ok_or_else(|| CanonicalError::from(not_readable(approval_id)))?;
    let names = state
        .participants
        .resolve(&ctx, participant_ids(&detail.record))
        .await;
    Ok((
        participant_read_headers(),
        Json(detail_view(&detail, &names)),
    ))
}

/// Current PII and caller-specific visibility must not enter shared/stale caches.
fn participant_read_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        axum::http::header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("private, no-store"),
    );
    headers
}

/// `POST /approvals/{approvalId}/approve`.
async fn approve_approval(
    Extension(state): Extension<Arc<GovernanceState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    extension_correlation: Option<Extension<CorrelationId>>,
    Path(approval_id): Path<Uuid>,
) -> Result<Json<ApprovalView>, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    let correlation = require_correlation(extension_correlation)?;
    let scope = decide_scope(&enforcer, &ctx, approval_id).await?;
    let record = decide_record(
        &state,
        &scope,
        &ctx,
        correlation,
        approval_id,
        DecisionBy::Approve(ctx.subject_id()),
        None,
        // Unread on this arm: the authority is `inst-as-void`'s and only a
        // withdraw is judged against it.
        WithdrawAuthority::OwnUnitsOnly,
    )
    .await?;

    // The approved bulk-operation subject: a mass-repricing run's batch unit
    // just reached `approved`, so its apply is spent here — see
    // `apply_approved_repricing_run`'s own doc for why this never fails the
    // response the decision already earned.
    if record.subject_kind == AuditSubjectKind::BulkOperation
        && record.state == ApprovalState::Approved
    {
        match Uuid::parse_str(&record.subject_ref) {
            Ok(operation_id) => {
                apply_approved_repricing_run(&state, &enforcer, &ctx, correlation, operation_id)
                    .await;
            }
            Err(e) => unaddressable_bulk_subject(&record, &e),
        }
    }

    Ok(Json(ApprovalView::from(&record)))
}

/// `POST /approvals/{approvalId}/reject`.
async fn reject_approval(
    Extension(state): Extension<Arc<GovernanceState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    extension_correlation: Option<Extension<CorrelationId>>,
    Path(approval_id): Path<Uuid>,
    _headers: HeaderMap,
    body: Bytes,
) -> Result<Json<ApprovalView>, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    let correlation = require_correlation(extension_correlation)?;
    let scope = decide_scope(&enforcer, &ctx, approval_id).await?;
    // `Bytes` + `parse_body`, never axum's `Json` extractor: its rejection for a
    // body that parses as JSON but not as the target type is a 422, and no path
    // in this gear may emit one.
    let body: RejectApprovalRequest = preconditions::parse_body(&body)?;
    let record = decide_record(
        &state,
        &scope,
        &ctx,
        correlation,
        approval_id,
        DecisionBy::Reject(ctx.subject_id()),
        Some(body.reason),
        // Unread on this arm: the authority is `inst-as-void`'s and only a
        // withdraw is judged against it.
        WithdrawAuthority::OwnUnitsOnly,
    )
    .await?;

    // §4 transition 6 (`inst-bs-reject`, D-267): the refused batch approval of a
    // mass-repricing run takes that run to `rejected`. The mirror of
    // `approve_approval`'s own bulk arm one function up, and the two are
    // deliberately the same shape — see `reject_repricing_run`.
    if record.subject_kind == AuditSubjectKind::BulkOperation
        && record.state == ApprovalState::Rejected
    {
        match Uuid::parse_str(&record.subject_ref) {
            Ok(operation_id) => reject_repricing_run(&state, &enforcer, &ctx, operation_id).await,
            Err(e) => unaddressable_bulk_subject(&record, &e),
        }
    }

    Ok(Json(ApprovalView::from(&record)))
}

/// A decided bulk unit whose subject names no run.
///
/// The decision stands — it was authorized, recorded and answered, and unwinding it
/// because its subject is unreadable would refuse a reviewer an act they lawfully
/// made. What does not happen is the apply or the reject, so the run stays where the
/// decision left it and only this line says why. `pricing_approval.subject_ref` is
/// written by this crate from a `BulkOperation`'s own id, so a value that is not a
/// uuid is a row from outside it.
fn unaddressable_bulk_subject(record: &ApprovalRecord, err: &uuid::Error) {
    tracing::error!(
        error = %err,
        approval_id = %record.approval_id,
        subject_ref = %record.subject_ref,
        state = %record.state.as_str(),
        "bss-pricing: a decided bulk-operation approval names a subject that is not a run id; \
         the decision stands and its run was neither applied nor rejected"
    );
}

/// `POST /approvals/{approvalId}/withdraw`.
async fn withdraw_approval(
    Extension(state): Extension<Arc<GovernanceState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    extension_correlation: Option<Extension<CorrelationId>>,
    Path(approval_id): Path<Uuid>,
    _headers: HeaderMap,
    body: Bytes,
) -> Result<Json<ApprovalView>, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    let correlation = require_correlation(extension_correlation)?;
    let scope = decide_scope(&enforcer, &ctx, approval_id).await?;
    // An empty body is a withdraw with no note, which is the ordinary case; a
    // body that is present must still be well-formed.
    let body: WithdrawApprovalRequest = if body.is_empty() {
        WithdrawApprovalRequest { reason: None }
    } else {
        preconditions::parse_body(&body)?
    };
    // **`inst-as-void`'s identity half, asked the only way this layer can ask it.**
    // The instruction names a role — "the submitter (or a `CatalogAdmin`)" — and
    // nothing here can answer a question about roles: `SecurityContext` carries a
    // subject, a tenant and token scopes, and the PDP answers `resource × action`.
    // So the second clause is asked as `plan × publish`, which is the authority
    // `CatalogAdmin` and `FinanceManager` carry and no `FinanceReviewer` does.
    //
    // Tenant-wide (`resource_id: None`): the question is whether this principal is
    // a catalog authority at all, not whether they may publish one particular plan
    // — and the unit's plan is not known here, only its approval id. A denial is an
    // answer and not a failure, so it narrows the authority rather than refusing the
    // request; the refusal, if one is owed, is `judge`'s to make against the
    // record's own submitter.
    //
    // **`require_constraints: true`, and the two halves are unrelated.** `false`
    // here does not merely accept an allow carrying no constraints:
    // `access_scope`'s tenant-membership assertion is conjoined with that same flag,
    // so the `Some(subject_tenant)` passed one line down does not constrain the
    // answer either. A principal whose `plan × publish` compiles to a *different*
    // tenant is then handed `CatalogAuthority` here — and that is the authority
    // `authorize_decision` accepts in place of being the submitter, so an authority
    // held only in B closes a unit in A and releases the canonical scope keys it
    // held. `authz`'s own rule is unconditional: `require_constraints` must be
    // `true` on every authorizing path. Being tenant-wide in *plan* is not being
    // tenant-wide in *tenant*.
    let withdraw_authority = if crate::authz::access_scope(
        &enforcer,
        &ctx,
        &crate::authz::resource_types::PLAN,
        crate::authz::actions::PUBLISH,
        Some(crate::authz::OwnerTenant(ctx.subject_tenant_id())),
        None,
    )
    .await
    .is_ok()
    {
        WithdrawAuthority::CatalogAuthority
    } else {
        WithdrawAuthority::OwnUnitsOnly
    };
    decide(
        &state,
        &scope,
        &ctx,
        correlation,
        approval_id,
        // `Some(subject_id)`: the withdrawer is a human and the record's audit
        // trail names them. It is **not** written to `approver_principal` - see
        // `infra::approval::judge`, which passes `approver()` precisely so a
        // submitter's own withdraw does not collide with
        // `chk_pricing_approval_distinct_principals`.
        DecisionBy::Void(Some(ctx.subject_id())),
        body.reason,
        withdraw_authority,
    )
    .await
}

// ---------------------------------------------------------------------------
// Shared pieces.
// ---------------------------------------------------------------------------

/// The three decisions, spelled once — the record, undressed.
///
/// [`decide`] is the thin wrapper every route but `approve` uses; the approve
/// route calls this directly because it has one more thing to do with the
/// record than render it (see [`approve_approval`]).
#[allow(
    clippy::too_many_arguments,
    reason = "the three routes' whole request, gathered: the service and the compiled scope, the \
              authenticated principal and the correlation its request was given, the record \
              addressed, and the decision with its reason. Folding them would name a DTO none of \
              the three routes has"
)]
async fn decide_record(
    state: &GovernanceState,
    scope: &AccessScope,
    ctx: &SecurityContext,
    correlation: Uuid,
    approval_id: Uuid,
    decision: DecisionBy,
    reason: Option<String>,
    withdraw_authority: WithdrawAuthority,
) -> Result<ApprovalRecord, CanonicalError> {
    let now = OffsetDateTime::now_utc();
    let tenant = ctx.subject_tenant_id();
    state
        .approvals
        .decide(
            scope,
            tenant,
            DecideRequest {
                approval_id,
                decision,
                reason,
                approver_regions: region_grant_of_this_surface(),
                stamp: audit_stamp(ctx, now, correlation),
                withdraw_authority,
            },
        )
        .await
        .map_err(CanonicalError::from)
}

/// The three decisions, rendered.
#[allow(
    clippy::too_many_arguments,
    reason = "decide_record's own reason, passed straight through"
)]
async fn decide(
    state: &GovernanceState,
    scope: &AccessScope,
    ctx: &SecurityContext,
    correlation: Uuid,
    approval_id: Uuid,
    decision: DecisionBy,
    reason: Option<String>,
    withdraw_authority: WithdrawAuthority,
) -> Result<Json<ApprovalView>, CanonicalError> {
    let record = decide_record(
        state,
        scope,
        ctx,
        correlation,
        approval_id,
        decision,
        reason,
        withdraw_authority,
    )
    .await?;
    Ok(Json(ApprovalView::from(&record)))
}

/// Hand an approved repricing run's apply to the lane, `inst-bs-commit`'s approved
/// arm.
///
/// **Never fails the `approve` request**, and it never could report the apply's
/// outcome either: the decision is durable before this runs, so this response has
/// always been about the decision and not about the run.
/// `api::rest::repricing_runs`'s own non-material path enqueues on the same lane for
/// the same reason — a mass repricing is a per-plan commit over every row the run
/// selected, and holding a governance request open for it makes a small act cost the
/// duration of a large one.
///
/// **`inst-bs-commit`'s `awaiting_approval -> committing` edge is spent here, before
/// the enqueue** — `infra::repricing::begin_committing_in` — so the run this arm
/// hands the lane is already `committing`. That is what gives the state an owner: an
/// approve whose enqueue is refused, or whose replica loses its applier, leaves a
/// `committing` run, which `POST …/repricing-runs/{runId}/abort` reaches
/// (`abandon_committing_run` guards on exactly that state). Leaving the edge to the
/// applier would queue the run at `awaiting_approval`, where the abort answers
/// `LIFECYCLE_FORBIDDEN`, no sweeper runs and no redrive is built — a run with no
/// door, reachable with no failure at all, since a full lane is enough.
///
/// **A failed edge is not enqueued.** The run stays `awaiting_approval` under a unit
/// that is already `approved`, which is where a redrive reaches it and which is the
/// same strand `reject_repricing_run`'s own crash window leaves; enqueueing anyway
/// would hand the lane a run whose refused-enqueue message could not be true.
///
/// The scope asked is **fresh**, `plan × write`, never the approval-decide
/// scope `approve_approval` already holds: deciding an approval and writing
/// the plan rows it approved are two different questions the PDP answers
/// separately, and an approver who cannot write the plan is not silently
/// handed the authority to apply it by having approved it. It is asked **here**,
/// on the request, rather than on the lane: the principal is present only here, and
/// the lane replays a decision rather than making one.
async fn apply_approved_repricing_run(
    state: &GovernanceState,
    enforcer: &authz_resolver_sdk::PolicyEnforcer,
    ctx: &SecurityContext,
    correlation: Uuid,
    operation_id: Uuid,
) {
    let tenant = ctx.subject_tenant_id();
    let scope = match crate::authz::access_scope(
        enforcer,
        ctx,
        &crate::authz::resource_types::PLAN,
        crate::authz::actions::WRITE,
        Some(crate::authz::OwnerTenant(tenant)),
        None,
    )
    .await
    {
        Ok(scope) => scope,
        Err(err) => {
            tracing::error!(
                error = ?err,
                run_id = %operation_id,
                "bss-pricing: repricing run apply not attempted: the approver holds no plan x \
                 write scope; the run stays awaiting_approval for a later redrive"
            );
            return;
        }
    };
    let stamp = crate::domain::audit::AuditStamp {
        actor_principal_id: ctx.subject_id(),
        recorded_at: OffsetDateTime::now_utc(),
        correlation_id: correlation,
    };
    if let Err(err) = crate::infra::repricing::begin_committing_in(
        &state.db,
        &scope,
        tenant,
        operation_id,
        stamp.recorded_at,
    )
    .await
    {
        tracing::error!(
            error = %err,
            run_id = %operation_id,
            "bss-pricing: repricing run apply not enqueued: the commit edge did not land; the run \
             stays awaiting_approval for a later redrive"
        );
        return;
    }
    state
        .apply_lane
        .enqueue(crate::infra::repricing::RunApplyRequest {
            ctx: ctx.clone(),
            scope,
            tenant_id: tenant,
            operation_id,
            stamp,
        });
}

/// Best-effort: §4 transition 6, spent the moment a second principal **refuses**
/// a repricing run's batch unit (`inst-bs-reject`, D-267).
///
/// `awaiting_approval -> rejected`, which the store's own trigger is the state
/// machine for — this names the edge and `bulk_repo::advance` writes it, exactly
/// as `advance_on_verdict` does at the other end. The terminal instant is not
/// passed: `advance` derives `completed_at` from `BulkState::is_terminal`, which
/// is what keeps the instant and the state from disagreeing about whether the run
/// ended (`chk_pricing_bulk_operation_completed_at`).
///
/// **The report travels unchanged.** Nothing about the run's frozen parameters
/// moved by being refused — and it must not, because the report is what the
/// unit's pin was taken over: rewriting it here would make the digest of the very
/// record that documents the refusal disagree with the approval that carries it.
///
/// **Never fails the `reject` request**, `apply_approved_repricing_run`'s posture
/// and its reason: the decision itself already committed, so a failure here is
/// not that request's to report. **What it costs is stated rather than left to be
/// found**, because it is not the same cost as the approve arm's. An apply that
/// fails leaves a `committing` run an operator can abort; a refusal that fails
/// to land leaves the run in `awaiting_approval` under a unit that is already
/// `rejected` — and nothing re-drives *that*, since a second `reject` is refused
/// `APPROVAL_NOT_PENDING`. The run is then stranded exactly as D-267 found it,
/// through a crash window rather than through a missing edge. Closing that needs
/// the transition inside `ApprovalService::decide`'s own transaction, which is a
/// decision about where a bulk run's state machine is driven from and not this
/// arm's to take.
///
/// The scope asked is **fresh**, `plan × write`, never the approval-decide scope
/// the caller already holds — `apply_approved_repricing_run`'s rule, for its
/// reason: deciding a unit and writing the run it was about are two questions the
/// PDP answers separately.
async fn reject_repricing_run(
    state: &GovernanceState,
    enforcer: &authz_resolver_sdk::PolicyEnforcer,
    ctx: &SecurityContext,
    operation_id: Uuid,
) {
    if let Err(reason) = advance_run_to_rejected(state, enforcer, ctx, operation_id).await {
        tracing::error!(
            run_id = %operation_id,
            reason = %reason,
            "bss-pricing: the batch approval was refused and the run did not reach `rejected`; \
             it is stranded where the decision left it, and a fresh run under a new client key \
             is the operator's remedy (D-267)"
        );
    }
}

/// [`reject_repricing_run`]'s body, with every failure rendered as the one
/// sentence its caller logs.
///
/// Split out for the reason `clippy::cognitive_complexity` gave when it was one
/// function: four fallible steps whose *only* handling is "say what went wrong
/// and stop" read as four branches, and the branching was the whole of the
/// complexity. `Result<(), String>` rather than a domain error because nothing
/// upstream can act on the distinction — this arm never fails the request — so
/// the value of the failure is entirely in what it says.
///
/// `Ok(())` when the run is **absent**, which is not a failure: `re_derive`
/// already answered `None` for such a unit and the reviewer was told their
/// subject had moved, so there is no state left to advance.
async fn advance_run_to_rejected(
    state: &GovernanceState,
    enforcer: &authz_resolver_sdk::PolicyEnforcer,
    ctx: &SecurityContext,
    operation_id: Uuid,
) -> Result<(), String> {
    let tenant = ctx.subject_tenant_id();
    let scope = crate::authz::access_scope(
        enforcer,
        ctx,
        &crate::authz::resource_types::PLAN,
        crate::authz::actions::WRITE,
        Some(crate::authz::OwnerTenant(tenant)),
        None,
    )
    .await
    .map_err(|err| format!("the approver holds no plan x write scope: {err}"))?;
    let conn = state
        .db
        .conn()
        .map_err(|err| format!("no connection: {err}"))?;
    let Some(run) = bulk_repo::read(&conn, &scope, tenant, operation_id)
        .await
        .map_err(|err| format!("the run could not be read: {err}"))?
    else {
        return Ok(());
    };
    bulk_repo::advance(
        &conn,
        &scope,
        tenant,
        operation_id,
        // The state the refusal was taken over, carried into the statement rather
        // than trusted to still hold — `advance_on_verdict`'s own rule.
        run.state,
        BulkState::Rejected,
        run.report.clone(),
        OffsetDateTime::now_utc(),
    )
    .await
    .map(|_| ())
    .map_err(|err| {
        format!(
            "the move {} -> rejected was refused: {err}",
            run.state.as_str()
        )
    })
}

/// The `approval × read` gate, for the two reads.
async fn read_scope(
    enforcer: &authz_resolver_sdk::PolicyEnforcer,
    ctx: &SecurityContext,
    approval_id: Option<Uuid>,
) -> Result<AccessScope, CanonicalError> {
    crate::authz::access_scope(
        enforcer,
        ctx,
        &crate::authz::resource_types::APPROVAL,
        crate::authz::actions::READ,
        /* owner_tenant_id */ None,
        /* resource_id */ approval_id.map(crate::authz::ResourceRef),
    )
    .await
    .map_err(authz_error_to_canonical)
}

/// The `approval × approve` gate, for the three decisions.
///
/// `owner_tenant_id = Some(caller's tenant)` because these are writes, so
/// `access_scope`'s membership assertion refuses a target outside the compiled
/// scope — the degraded flat-`In` decision does not re-check the property.
async fn decide_scope(
    enforcer: &authz_resolver_sdk::PolicyEnforcer,
    ctx: &SecurityContext,
    approval_id: Uuid,
) -> Result<AccessScope, CanonicalError> {
    crate::authz::access_scope(
        enforcer,
        ctx,
        &crate::authz::resource_types::APPROVAL,
        crate::authz::actions::APPROVE,
        /* owner_tenant_id */ Some(crate::authz::OwnerTenant(ctx.subject_tenant_id())),
        /* resource_id */ Some(crate::authz::ResourceRef(approval_id)),
    )
    .await
    .map_err(authz_error_to_canonical)
}

/// The deciding principal's **pricing-region** grant, as this surface can
/// establish it.
///
/// **It cannot, and this function is the report rather than the plumbing.**
/// `inst-ap-scope` requires the approver's grant to cover every pricing region
/// the pinned change set touches, and `inst-rb-preview-scope` says a grant
/// *"carries an explicit pricing-region set"* and stops there: **no document in
/// the set says how that set is transported** — which claim, which PDP
/// constraint, which resource property. `SecurityContext` exposes a subject, a
/// tenant, a subject type and token scopes; `authz::SUPPORTED_PROPERTIES` is
/// `owner_tenant_id` and `resource_id`, both uuid-typed, while a pricing region
/// is a string on a price row's scope key. There is nothing here to read a grant
/// from that would not be a property name invented in this file.
///
/// So it answers [`RegionGrant::Untransported`], whose own doc carries the
/// consequence: **`inst-ap-scope` is not enforced on this surface** and
/// `REGION_SCOPE_DENIED` is unreachable over HTTP. The rule itself is built and
/// both its directions are driven through the service
/// (`domain::approval::decision` judges it, `tests/sqlite_approval_service.rs`
/// exercises it under [`RegionGrant::Explicit`]); what is missing is one
/// transport, not the rule.
///
/// # It used to return a set, and that was a defect rather than a spelling
///
/// It read the pinned subject through `ApprovalService::find` and returned the
/// change set's own reach — the same *value* the untransported variant now
/// resolves to, established at a different **time**. `judge` re-derives the
/// change set inside the judgement transaction, so a mutation adding a row in a
/// new region between the two reads made `is_subset` fail and answered
/// `OutOfScope`: a 403, and a `deny` audit record classing an innocent reviewer
/// as an attempted authority violation. Handing the *fact* over instead of a
/// stale answer leaves one read where there were two, and drops a whole plan
/// assembly off every decision as a side effect.
///
/// Whichever wave declares the transport replaces this function's body with a
/// read of it and returns [`RegionGrant::Explicit`];
/// `approvals_tests::the_region_rule_is_not_enforced_at_this_surface` is the
/// assertion that has to change when it does.
const fn region_grant_of_this_surface() -> RegionGrant {
    RegionGrant::Untransported
}

/// Report at boot what this deployment enforces of the pricing-region rules.
///
/// **Reads [`region_grant_of_this_surface`] rather than restating its answer**,
/// so the day that function returns [`RegionGrant::Explicit`] the warning stops
/// firing by construction. A boot line kept alive by a literal outlives the gap
/// it describes, and an operator then alerts on a condition that has been fixed.
///
/// The alarm as well as the `warn!`, for the reason `module::report_fixture_gate`
/// gives of its own: `05-governance.md` §5 marks the mutation clause `p1`, and a
/// normative rule that nothing enforces is a standing authorization gap, not a
/// boot detail. On the log plane alone there is nothing an alerting rule can
/// match, and the only readers of the gap are then the source comments in
/// [`crate::authz::actions`] and [`RegionGrant::Untransported`].
pub(crate) fn report_region_grant_transport(
    metrics: &dyn crate::domain::ports::metrics::PricingMetricsPort,
) {
    // Exhaustively, and not `if let`: the transport arriving must be a compile
    // error here rather than a branch that quietly stops reporting.
    match region_grant_of_this_surface() {
        RegionGrant::Untransported => {
            let alarm = crate::domain::ports::metrics::PricingAlarm::RegionGrantUntransported;
            metrics.alarm(alarm);
            tracing::warn!(
                alarm = alarm.as_str(),
                supported_properties = ?crate::authz::SUPPORTED_PROPERTIES,
                "bss-pricing: nothing transports a pricing-region grant to the PDP, so \
                 inst-ap-scope (who may approve a change set) and inst-rb-region's p1 clause \
                 (who may mutate a row) are NOT enforced: a principal holding a role intended \
                 for one region approves and authors in every region of its tenant, \
                 REGION_SCOPE_DENIED is unreachable over HTTP, and no denial is audited \
                 because none happens"
            );
        }
        RegionGrant::Explicit(_) => {}
    }
}

/// The composition a `bundleComposition` unit is being decided on (D-104, D-61).
///
/// **What a plan shape cannot say.** `PinnedContentView` renders the plan the
/// composition rides — its phases, rows and descriptor set — and carries no
/// component set and no revenue split. D-104 exists because a `sum_of_parts`
/// recomposition moves no price row at all, so a reviewer shown only the plan was
/// shown a document in which the act they are approving is invisible. This is the
/// one act in the gear whose subject is money belonging to third parties.
///
/// The values are the ones the pin was taken over, re-derived in the same read.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct PinnedCompositionView {
    /// The referenced components, in stored order.
    pub components: Vec<PinnedComponentView>,
    /// The rev-share groups, one per included vendor SKU.
    pub rev_share: Vec<PinnedRevShareGroupView>,
}

/// One component of the pinned composition.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct PinnedComponentView {
    /// The component's plan (B1).
    pub component_plan_id: Uuid,
    /// The registry SKU it publishes under.
    pub included_sku_id: Uuid,
    /// Selection-time lower bound.
    pub min_qty: Option<i32>,
    /// Selection-time upper bound.
    pub max_qty: Option<i32>,
}

/// One rev-share group of the pinned composition — the vendor payout split.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct PinnedRevShareGroupView {
    /// The included vendor SKU whose revenue this group allocates.
    pub vendor_sku_id: Uuid,
    /// The group's explicit platform cut, in basis points.
    pub platform_cut_bp: i32,
    /// Who absorbs the publish-time residual (D-07).
    pub residual_absorber: String,
    /// The parties and their typed shares, in basis points.
    pub parties: Vec<PinnedPartyShareView>,
}

/// One party's typed share.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct PinnedPartyShareView {
    /// The recipient.
    pub party: String,
    /// What the operator authored, in basis points.
    pub share_bp: i32,
}

impl From<&crate::infra::storage::repo::CompositionDraft> for PinnedCompositionView {
    fn from(draft: &crate::infra::storage::repo::CompositionDraft) -> Self {
        // Destructured with no rest pattern: a field added to the composition is a
        // compile error here rather than content the reviewer silently stops being
        // shown — which is the defect this view exists to close.
        let crate::infra::storage::repo::CompositionDraft {
            components,
            rev_share_groups,
        } = draft;
        Self {
            components: components
                .iter()
                .map(|c| PinnedComponentView {
                    component_plan_id: c.component_plan_id,
                    included_sku_id: c.included_sku_id,
                    min_qty: c.min_qty,
                    max_qty: c.max_qty,
                })
                .collect(),
            rev_share: rev_share_groups
                .iter()
                .map(|g| PinnedRevShareGroupView {
                    vendor_sku_id: g.vendor_sku_id,
                    platform_cut_bp: g.platform_cut_bp,
                    residual_absorber: g.residual_absorber.as_str().to_owned(),
                    parties: g
                        .parties
                        .iter()
                        .map(|p| PinnedPartyShareView {
                            party: p.party.get().to_owned(),
                            share_bp: p.share_bp,
                        })
                        .collect(),
                })
                .collect(),
        }
    }
}

/// The act a window unit is being decided on, for the reviewer's read.
///
/// A projection of [`crate::infra::window::ProposedAct`], which is parsed from the
/// record's own subject. Instants are rendered as the subject spells them rather
/// than re-parsed: the subject is the authenticated artifact, and a re-parse that
/// normalised an offset would show a reviewer something other than what was signed.
#[toolkit_macros::api_dto(response)]
#[derive(Debug, Clone)]
pub struct ProposedActView {
    /// `schedule`, `adjust` or `cancel`.
    pub operation: String,
    /// The window the act is on; `null` for a schedule, whose window does not exist yet.
    pub window_id: Option<Uuid>,
    /// The price row a schedule would put a window on; `null` for the other two acts.
    pub price_id: Option<Uuid>,
    /// The start a schedule proposes; `null` for the other two.
    pub effective_from: Option<String>,
    /// The end the act proposes; `null` means open-ended.
    pub effective_to: Option<String>,
    /// The end as it stands before the act; `null` for a schedule, and for a window
    /// that is already open-ended.
    pub current_effective_to: Option<String>,
}

impl From<crate::infra::window::ProposedAct> for ProposedActView {
    fn from(act: crate::infra::window::ProposedAct) -> Self {
        Self {
            operation: act.operation,
            window_id: act.window_id,
            price_id: act.price_id,
            effective_from: act.effective_from,
            effective_to: act.effective_to,
            current_effective_to: act.current_effective_to,
        }
    }
}

/// Render one detail, pin and all.
fn detail_view(
    detail: &ApprovalDetail,
    names: &BTreeMap<Uuid, ParticipantName>,
) -> ApprovalDetailView {
    ApprovalDetailView {
        approval: ApprovalReadView::new(&detail.record, names),
        pinned_content: detail
            .subject
            .as_ref()
            .and_then(PinnedSubject::plan)
            .map(PinnedContentView::from),
        pinned_threshold_policy: detail
            .subject
            .as_ref()
            .and_then(PinnedSubject::threshold_policy)
            .map(PinnedThresholdPolicyView::from),
        // Only a window unit's subject names an act; every other kind's subject is
        // an id, and the parser answers `None` for anything it did not build.
        proposed_act: (detail.record.subject_kind
            == crate::domain::audit::AuditSubjectKind::Window)
            .then(|| crate::infra::window::parse_unit_subject(&detail.record.subject_ref))
            .flatten()
            .map(ProposedActView::from),
        pinned_composition: detail
            .subject
            .as_ref()
            .and_then(PinnedSubject::composition)
            .map(PinnedCompositionView::from),
        pinned_repricing_run: detail
            .subject
            .as_ref()
            .and_then(PinnedSubject::repricing_run)
            .cloned(),
        pinned_taxonomy_value: detail
            .subject
            .as_ref()
            .and_then(PinnedSubject::taxonomy_value)
            .map(PinnedTaxonomyValueView::from),
        content_matches_pin: detail.content_matches_pin,
    }
}

/// "Absent, or not yours" — one answer for both.
///
/// A 403 for another tenant's unit would confirm it exists, and what a pending
/// unit tells an observer is that a price change is in flight.
fn not_readable(approval_id: Uuid) -> DomainError {
    DomainError::NotFound {
        subject: "approval".to_owned(),
        id: approval_id.to_string(),
    }
}

/// Lower-case hex, so the digest survives a JSON round trip a byte array would
/// not.
///
/// Hand-rolled rather than through `write!` for one reason: writing into a
/// `String` cannot fail, so every spelling that uses the formatter has to
/// discard a `Result` that carries a `#[must_use]`, and this crate's lint set
/// refuses all three ways of doing that. Two table lookups per byte is the
/// smaller cost.
fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from(DIGITS[usize::from(byte >> 4)]));
        out.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    out
}

#[cfg(test)]
#[path = "approvals_tests.rs"]
mod approvals_tests;
