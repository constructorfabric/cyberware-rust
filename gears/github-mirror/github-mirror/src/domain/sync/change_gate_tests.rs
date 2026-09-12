use chrono::{Duration, Utc};

use super::{
    GateInputs, GateReason, REFINEMENT_COMPLETE, REFINEMENT_PENDING, child_counts_hash, entities,
    evaluate_refinement_gate, family_ttl, fingerprint,
};
use crate::domain::repo::EntityFingerprintRecord;

fn inputs(fp: &str, counts: Option<&str>, terminal: bool) -> GateInputs {
    GateInputs {
        fingerprint: fp.to_owned(),
        child_counts_hash: counts.map(ToOwned::to_owned),
        updated_at: None,
        node_id: None,
        terminal,
    }
}

fn stored(
    fp: &str,
    counts: Option<&str>,
    status: &str,
    refined_ago: Option<Duration>,
) -> EntityFingerprintRecord {
    EntityFingerprintRecord {
        repo_id: 1,
        family: entities::ISSUE.to_owned(),
        entity_id: "11".to_owned(),
        fingerprint: fp.to_owned(),
        updated_at: None,
        node_id: None,
        child_counts_hash: counts.map(ToOwned::to_owned),
        last_refined_at: refined_ago
            .map(|ago| (Utc::now() - ago).to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
        refinement_status: status.to_owned(),
    }
}

#[test]
fn field_order_does_not_change_the_fingerprint() {
    let one = fingerprint(vec![("b", "2".to_owned()), ("a", "1".to_owned())]);
    let other = fingerprint(vec![("a", "1".to_owned()), ("b", "2".to_owned())]);
    assert_eq!(one, other);
}

#[test]
fn a_changed_field_changes_the_fingerprint() {
    let one = fingerprint(vec![("state", "open".to_owned())]);
    let other = fingerprint(vec![("state", "closed".to_owned())]);
    assert_ne!(one, other);
}

#[test]
fn a_listing_without_counts_has_no_child_hash() {
    assert!(child_counts_hash(&[("comments", None)]).is_none());
    assert!(child_counts_hash(&[("comments", Some(0))]).is_some());
}

#[test]
fn an_unseen_entity_is_refined() {
    let reason = evaluate_refinement_gate(
        None,
        &inputs("a", None, false),
        entities::ISSUE,
        Utc::now(),
        false,
    );
    assert_eq!(reason, Some(GateReason::New));
}

#[test]
fn force_refines_even_an_unchanged_entity() {
    let row = stored("a", None, REFINEMENT_COMPLETE, Some(Duration::minutes(1)));
    let reason = evaluate_refinement_gate(
        Some(&row),
        &inputs("a", None, false),
        entities::ISSUE,
        Utc::now(),
        true,
    );
    assert_eq!(reason, Some(GateReason::Forced));
}

#[test]
fn an_unchanged_entity_within_its_ttl_is_skipped() {
    let row = stored("a", None, REFINEMENT_COMPLETE, Some(Duration::minutes(1)));
    let reason = evaluate_refinement_gate(
        Some(&row),
        &inputs("a", None, false),
        entities::ISSUE,
        Utc::now(),
        false,
    );
    assert_eq!(reason, None);
}

#[test]
fn a_moved_fingerprint_is_refined() {
    let row = stored("a", None, REFINEMENT_COMPLETE, Some(Duration::minutes(1)));
    let reason = evaluate_refinement_gate(
        Some(&row),
        &inputs("b", None, false),
        entities::ISSUE,
        Utc::now(),
        false,
    );
    assert_eq!(reason, Some(GateReason::FingerprintChanged));
}

#[test]
fn a_new_child_count_is_refined_even_when_the_parent_is_unchanged() {
    let row = stored(
        "a",
        Some("one"),
        REFINEMENT_COMPLETE,
        Some(Duration::minutes(1)),
    );
    let reason = evaluate_refinement_gate(
        Some(&row),
        &inputs("a", Some("two"), false),
        entities::ISSUE,
        Utc::now(),
        false,
    );
    assert_eq!(reason, Some(GateReason::ChildCountsChanged));
}

#[test]
fn a_refinement_that_never_finished_is_retried() {
    let row = stored("a", None, REFINEMENT_PENDING, Some(Duration::minutes(1)));
    let reason = evaluate_refinement_gate(
        Some(&row),
        &inputs("a", None, false),
        entities::ISSUE,
        Utc::now(),
        false,
    );
    assert_eq!(reason, Some(GateReason::Incomplete));
}

#[test]
fn an_open_issue_is_refined_again_once_its_backstop_expires() {
    let row = stored("a", None, REFINEMENT_COMPLETE, Some(Duration::hours(5)));
    let reason = evaluate_refinement_gate(
        Some(&row),
        &inputs("a", None, false),
        entities::ISSUE,
        Utc::now(),
        false,
    );
    assert_eq!(reason, Some(GateReason::TtlExpired));
}

#[test]
fn a_closed_issue_keeps_a_longer_backstop_than_an_open_one() {
    assert!(
        family_ttl(entities::ISSUE, true) > family_ttl(entities::ISSUE, false),
        "a settled issue must be refined less often than a live one"
    );
}

#[test]
fn a_commit_without_ci_never_expires() {
    assert_eq!(family_ttl(entities::COMMIT, true), None);
    let row = stored("a", None, REFINEMENT_COMPLETE, Some(Duration::days(365)));
    let reason = evaluate_refinement_gate(
        Some(&row),
        &inputs("a", None, true),
        entities::COMMIT,
        Utc::now(),
        false,
    );
    assert_eq!(
        reason, None,
        "a commit body cannot change, so age alone must not refetch it"
    );
}

#[test]
fn a_commit_with_ci_is_refreshed_on_its_backstop() {
    let row = stored("a", None, REFINEMENT_COMPLETE, Some(Duration::hours(2)));
    let reason = evaluate_refinement_gate(
        Some(&row),
        &inputs("a", None, false),
        entities::COMMIT,
        Utc::now(),
        false,
    );
    assert_eq!(reason, Some(GateReason::TtlExpired));
}
