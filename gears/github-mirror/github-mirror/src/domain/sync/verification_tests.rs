use super::{CountGap, GapOutcome, MAX_REPAIR, pull_gaps};
use crate::domain::ports::github::{DeclaredCounts, PullDetail};
use crate::domain::repo::{PullRequestCommitRecord, PullRequestFileRecord, PullRequestRecord};

fn gap(expected: u64, stored: u64, attempts: u32, previous: Option<u64>) -> CountGap {
    CountGap {
        entity_type: "pull_request_commits".to_owned(),
        expected,
        stored,
        repair_attempts: attempts,
        previous_gap: previous,
    }
}

fn pull(declared: DeclaredCounts, commits: usize, files: usize) -> PullDetail {
    PullDetail {
        pull_request: PullRequestRecord {
            id: 3,
            node_id: None,
            repo_id: 42,
            number: 13,
            title: "a pr".to_owned(),
            body: None,
            state: "open".to_owned(),
            draft: false,
            merged: false,
            head_sha: None,
            base_sha: None,
            lines_added: 0,
            lines_removed: 0,
            created_at: "2026-09-01T10:00:00Z".to_owned(),
            updated_at: "2026-09-01T10:00:00Z".to_owned(),
            closed_at: None,
            merged_at: None,
            html_url: None,
            head_ref: None,
            base_ref: None,
            author_login: None,
            author_json: None,
            assignees_json: None,
            labels_json: None,
            comments_count: None,
            locked: None,
            requested_reviewers_json: None,
        },
        reviews: Vec::new(),
        files: (0..files)
            .map(|i| PullRequestFileRecord {
                repo_id: 42,
                pull_number: 13,
                filename: format!("f{i}.rs"),
                status: "modified".to_owned(),
                additions: 0,
                deletions: 0,
                changes: 0,
                previous_filename: None,
                sha: None,
                patch: None,
            })
            .collect(),
        commits: (0..commits)
            .map(|i| PullRequestCommitRecord {
                repo_id: 42,
                pull_number: 13,
                sha: format!("c{i}"),
                message: String::new(),
                author_login: None,
                committer_login: None,
                authored_at: None,
                committed_at: None,
            })
            .collect(),
        review_threads: Vec::new(),
        declared,
        contributors: Vec::new(),
    }
}

#[test]
fn a_matching_count_is_complete() {
    assert_eq!(gap(12, 12, 0, None).outcome(), GapOutcome::Complete);
}

#[test]
fn a_short_count_asks_for_a_repair() {
    assert_eq!(gap(12, 9, 0, None).outcome(), GapOutcome::Repair);
}

#[test]
fn a_gap_that_does_not_shrink_is_accepted_drift() {
    assert_eq!(
        gap(12, 9, 1, Some(3)).outcome(),
        GapOutcome::AcceptedDrift,
        "a repair that changed nothing will not change anything next time either"
    );
}

#[test]
fn a_shrinking_gap_keeps_repairing() {
    assert_eq!(gap(12, 11, 1, Some(3)).outcome(), GapOutcome::Repair);
}

#[test]
fn the_repair_budget_is_bounded() {
    assert_eq!(
        gap(12, 9, MAX_REPAIR, Some(5)).outcome(),
        GapOutcome::AcceptedDrift
    );
}

#[test]
fn advancing_records_the_gap_it_started_from() {
    let advanced = gap(12, 9, 0, None).advance(11);
    assert_eq!(advanced.stored, 11);
    assert_eq!(advanced.repair_attempts, 1);
    assert_eq!(advanced.previous_gap, Some(3));
}

#[test]
fn a_stored_count_above_the_declared_one_is_not_a_gap() {
    assert_eq!(gap(9, 12, 0, None).size(), 0);
    assert_eq!(gap(9, 12, 0, None).outcome(), GapOutcome::Complete);
}

#[test]
fn a_pull_request_that_holds_what_it_declares_has_no_gaps() {
    let detail = pull(
        DeclaredCounts {
            commits: Some(2),
            files: Some(1),
            review_comments: None,
        },
        2,
        1,
    );
    assert!(pull_gaps(&detail).is_empty());
}

#[test]
fn a_short_walk_is_reported_per_entity_type() {
    let detail = pull(
        DeclaredCounts {
            commits: Some(12),
            files: Some(7),
            review_comments: None,
        },
        9,
        7,
    );
    let gaps = pull_gaps(&detail);
    assert_eq!(gaps.len(), 1, "only the commits walk came up short");
    assert_eq!(gaps[0].entity_type, "pull_request_commits");
    assert_eq!(gaps[0].expected, 12);
    assert_eq!(gaps[0].stored, 9);
}

#[test]
fn a_payload_without_counts_reports_nothing() {
    let detail = pull(DeclaredCounts::default(), 0, 0);
    assert!(
        pull_gaps(&detail).is_empty(),
        "a missing count is not a count of zero"
    );
}
