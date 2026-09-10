//! What the publish mount reads off a decided approval record.
//!
//! [`authorization_of`] is the whole of this file, and it is tested here rather
//! than through the route because **its refusal is unreachable through the
//! route**: the record it refuses cannot be stored.
//! `chk_pricing_approval_distinct_principals` refuses it at the table and
//! `domain::approval::authorize_decision` refuses the decision that would write
//! it, so a suite that drove HTTP could assert the happy arm and nothing else.
//! A hand-built record is the only world in which the check under test is what
//! answers.

use uuid::Uuid;

use super::authorization_of;
use crate::domain::approval::ApprovalState;
use crate::domain::audit::AuditSubjectKind;
use crate::domain::error::DomainError;
use crate::infra::storage::repo::approval_repo::ApprovalRecord;
use time::OffsetDateTime;

const SUBMITTER: Uuid = Uuid::from_u128(0x5_bb);
const APPROVER: Uuid = Uuid::from_u128(0xa_bb);
const APPROVAL: Uuid = Uuid::from_u128(0x1_11);

/// The digest a record pins. Its value is arbitrary; what matters is that the
/// authorization carries it out unchanged.
const PIN: [u8; 32] = [0x7c; 32];

/// An `approved` record, as the store would hold it.
fn approved(submitter: Uuid, approver: Option<Uuid>, content_hash: Vec<u8>) -> ApprovalRecord {
    ApprovalRecord {
        approval_id: APPROVAL,
        tenant_id: Uuid::from_u128(0x7e),
        subject_ref: "00000000-0000-0000-0000-0000000000aa/0".to_owned(),
        subject_kind: AuditSubjectKind::PlanRevision,
        content_hash,
        state: ApprovalState::Approved,
        submitter_principal: submitter,
        approver_principal: approver,
        reason: None,
        materiality: serde_json::json!({ "material": true }),
        submitted_at: OffsetDateTime::now_utc(),
        decided_at: Some(OffsetDateTime::now_utc()),
    }
}

/// **The world in which every refusal below is a fact about the rule it names.**
///
/// Without this, an `authorization_of` that refused everything would satisfy all
/// three tests under it.
#[test]
fn a_record_decided_by_an_independent_principal_authorizes_the_publish() {
    let authorization = authorization_of(&approved(SUBMITTER, Some(APPROVER), PIN.to_vec()))
        .expect("it authorizes");

    assert_eq!(authorization.approval_ref(), Some(APPROVAL));
    assert_eq!(authorization.principals(), Some((SUBMITTER, APPROVER)));
    // And the pin travels with it — this is what the commit re-derives inside
    // its own transaction, which is where the approve→commit window is closed.
    assert_eq!(authorization.pinned_content_hash(), Some(PIN));
}

/// `inst-tp-distinct`, read by the publish path rather than trusted from a
/// column.
///
/// `PublishAuthorization::approved` deliberately does not assert it — the rule
/// keeps one owner, the approval workflow — so this path has to compare rather
/// than take the stored `approver_principal` on faith. The record below is one
/// the store cannot hold; what it stands in for is a row written around the
/// store, by a migration, a restore, or a later slice's writer.
#[test]
fn an_approved_record_naming_one_principal_twice_is_refused_here() {
    let refused = authorization_of(&approved(SUBMITTER, Some(SUBMITTER), PIN.to_vec()))
        .expect_err("a record that names one principal twice authorizes nothing");

    assert!(
        matches!(refused, DomainError::SelfApprovalForbidden(_)),
        "got {refused:?}"
    );
}

/// An `approved` row with no approver is an invariant breach, not a refusal a
/// caller earned.
///
/// `chk_pricing_approval_approver` makes it unstorable, so reporting it as a
/// permission denial would tell an operator to find a second person for a row
/// the second person cannot fix.
#[test]
fn an_approved_record_carrying_no_approver_is_an_internal_fault() {
    let refused = authorization_of(&approved(SUBMITTER, None, PIN.to_vec()))
        .expect_err("an approved record must name its approver");

    assert!(
        matches!(refused, DomainError::Internal(_)),
        "got {refused:?}"
    );
}

/// A pin that is not a 32-byte digest, likewise.
///
/// This crate is the only writer of the column and it writes SHA-256, so a
/// different length means the row did not come from here. Truncating or padding
/// it would produce a digest that compares against nothing and refuse every
/// publish with `APPROVAL_CONTENT_MISMATCH` — a wrong answer that looks like the
/// right one.
#[test]
fn a_pin_that_is_not_a_sha256_digest_is_an_internal_fault() {
    let refused = authorization_of(&approved(SUBMITTER, Some(APPROVER), vec![0x01, 0x02]))
        .expect_err("the pin is a 32-byte digest");

    assert!(
        matches!(refused, DomainError::Internal(_)),
        "got {refused:?}"
    );
}
