//! The proposed member set a **material** membership change carries as its
//! approval payload (`inst-mm-pending`, `design/09-price-overlays.md:215`).
//!
//! Memberships have no draft table (`inst-mm-pending`'s own words: "no
//! `pricing_group_membership` row is written before approval"), so unlike a
//! plan revision, an overlay revision or a threshold-policy version, a material
//! membership change has nothing durable in this crate's other stores for a
//! pending unit to point at. What `inst-mm-pending` settles instead: the
//! proposed member set — per payer, the group and the effective instant they
//! are moving into — travels **inside the approval record itself**, and is
//! read back off the record rather than off a second store.
//!
//! [`MembershipMoveSet`] is that payload. `crate::domain::approval::content_pin`
//! is what pins it (`membership_content_hash`); `crate::infra::approval` is
//! what carries it through `pricing_approval.subject_ref` — the one column of
//! that table with room for it, and the reason a re-derivation needs no store
//! read at all: the content the pin was taken over is the `subject_ref`'s own
//! text, so it cannot have moved between submit and decide the way a plan's
//! rows can.

use uuid::Uuid;

use crate::domain::error::DomainError;
use time::OffsetDateTime;

/// One payer's proposed membership change: end whatever membership they hold
/// active at `effective_from` and enroll them into `group_value`, both at that
/// instant — [`crate::infra::membership_publish::move_payer_in`]'s atomic
/// move, staged as a proposal rather than committed.
///
/// The **ended** membership is deliberately not named here. A reviewer is
/// approving a target — this payer moves into this group at this instant —
/// not a specific prior row, which may not even exist yet at submit time and
/// which [`crate::infra::membership_publish::move_payer_in`] already resolves
/// dynamically at commit (`resolve_active_membership` over the payer's
/// intervals **as they stand when the commit runs**). Naming the predecessor
/// here would pin a fact the proposal does not need and the commit does not
/// use.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MembershipMoveProposal {
    /// AMS's identity for the payer — [`crate::infra::storage::repo::group_membership_repo`]'s
    /// own key.
    pub payer_tenant_id: Uuid,
    /// The target group, already validated non-blank by the surface that
    /// built this proposal (`customer_groups::required_group`'s shape).
    pub group_value: String,
    /// The instant the move pivots on.
    pub effective_from: OffsetDateTime,
}

/// A non-empty, payer-deduplicated set of [`MembershipMoveProposal`]s — the
/// whole of one material membership change's payload, whether it names one
/// payer (`inst-mm-immediate`) or many (`inst-mm-bulk`).
///
/// Stored in one canonical order — sorted by `payer_tenant_id` — so that
/// [`Self::proposals`]'s iteration order, the content pin's preimage and the
/// `subject_ref` encoding all agree without each having to re-sort. Two
/// requests naming the same payers in a different authored order therefore
/// pin identically, which is the same "collections hashed in canonical order,
/// not the order they arrive in" discipline `content_pin`'s module doc states
/// for `PlanShape`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MembershipMoveSet {
    proposals: Vec<MembershipMoveProposal>,
}

impl MembershipMoveSet {
    /// Build a set, refusing what cannot be a reviewable payload.
    ///
    /// # Errors
    /// [`DomainError::InvalidRequest`] on an empty set — a unit approving
    /// nothing is not a reviewable change, and an operator handed the pin of
    /// an empty payload could not tell it apart from a corrupt one — or on a
    /// set naming one payer twice, which is two proposed destinations for one
    /// payer and no rule says which the commit should apply.
    pub fn new(mut proposals: Vec<MembershipMoveProposal>) -> Result<Self, DomainError> {
        if proposals.is_empty() {
            return Err(DomainError::InvalidRequest(
                "a membership move set names at least one payer; an empty set is not a \
                 reviewable change"
                    .to_owned(),
            ));
        }
        proposals.sort_unstable_by_key(|p| p.payer_tenant_id);
        for pair in proposals.windows(2) {
            if pair[0].payer_tenant_id == pair[1].payer_tenant_id {
                return Err(DomainError::InvalidRequest(format!(
                    "payer {} names two proposed destinations in one membership move set; a \
                     set names each payer once",
                    pair[0].payer_tenant_id
                )));
            }
        }
        Ok(Self { proposals })
    }

    /// The proposals, in the set's canonical (payer-sorted) order.
    #[must_use]
    pub fn proposals(&self) -> &[MembershipMoveProposal] {
        &self.proposals
    }
}
