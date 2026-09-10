use github_mirror_sdk::{
    Actor, Branch, CheckRun, Comment, Commit, CommitComment, CommitFile, CommitStatus, Contributor,
    Deployment, Issue, IssueEvent, IssueReaction, IssueTimelineEvent, Label, LabelRef, Milestone,
    PullRequest, PullRequestCommit, PullRequestFile, Release, ReleaseAsset, Repo, Review,
    ReviewComment, ReviewThread, Tag, WorkflowJob, WorkflowRun, WorkflowStep,
};

use super::entity::{
    branches, check_runs, comments, commit_comments, commit_files, commit_statuses, commits,
    contributors, deployments, issue_events, issue_reactions, issue_timeline, issues, labels,
    milestones, pull_request_commits, pull_request_files, pull_requests, releases, repositories,
    review_comments, review_threads, reviews, tags, workflow_jobs, workflow_runs,
};

#[derive(serde::Deserialize)]
pub(super) struct StoredActor {
    login: String,
    #[serde(default)]
    id: Option<i64>,
    #[serde(default)]
    node_id: Option<String>,
    #[serde(rename = "type", default)]
    account_type: Option<String>,
    #[serde(default)]
    avatar_url: Option<String>,
    #[serde(default)]
    html_url: Option<String>,
    #[serde(default)]
    site_admin: Option<bool>,
}

impl From<StoredActor> for Actor {
    fn from(a: StoredActor) -> Self {
        Self {
            login: a.login,
            id: a.id,
            node_id: a.node_id,
            account_type: a.account_type,
            avatar_url: a.avatar_url,
            html_url: a.html_url,
            site_admin: a.site_admin,
        }
    }
}

#[derive(serde::Deserialize)]
pub(super) struct StoredLabel {
    name: String,
    #[serde(default)]
    id: Option<i64>,
    #[serde(default)]
    node_id: Option<String>,
    #[serde(default)]
    color: Option<String>,
    #[serde(rename = "default", default)]
    is_default: Option<bool>,
    #[serde(default)]
    description: Option<String>,
}

impl From<StoredLabel> for LabelRef {
    fn from(l: StoredLabel) -> Self {
        Self {
            id: l.id,
            node_id: l.node_id,
            name: l.name,
            color: l.color,
            is_default: l.is_default,
            description: l.description,
        }
    }
}

#[derive(serde::Deserialize)]
pub(super) struct StoredAsset {
    name: String,
    #[serde(default)]
    id: Option<i64>,
    #[serde(default)]
    node_id: Option<String>,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    content_type: Option<String>,
    #[serde(default)]
    size: Option<i64>,
    #[serde(default)]
    download_count: Option<i64>,
    #[serde(default)]
    browser_download_url: Option<String>,
    #[serde(default)]
    created_at: Option<String>,
    #[serde(default)]
    updated_at: Option<String>,
}

impl From<StoredAsset> for ReleaseAsset {
    fn from(a: StoredAsset) -> Self {
        Self {
            id: a.id,
            node_id: a.node_id,
            name: a.name,
            label: a.label,
            content_type: a.content_type,
            size: a.size,
            download_count: a.download_count,
            browser_download_url: a.browser_download_url,
            created_at: a.created_at,
            updated_at: a.updated_at,
        }
    }
}

#[derive(serde::Deserialize)]
pub(super) struct StoredStep {
    name: String,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    conclusion: Option<String>,
    #[serde(default)]
    number: Option<i64>,
    #[serde(default)]
    started_at: Option<String>,
    #[serde(default)]
    completed_at: Option<String>,
    /// Everything GitHub sent that the six typed fields do not name.
    ///
    /// `steps_json` holds GitHub's own step payload, which used to reach
    /// clients whole; keeping the rest here means typing the known fields
    /// costs no data.
    #[serde(flatten)]
    extra: serde_json::Map<String, serde_json::Value>,
}

impl From<StoredStep> for WorkflowStep {
    fn from(st: StoredStep) -> Self {
        Self {
            name: st.name,
            status: st.status,
            conclusion: st.conclusion,
            number: st.number,
            started_at: st.started_at,
            completed_at: st.completed_at,
            extra: st.extra,
        }
    }
}

/// Most bytes of stored JSON one column may hand the deserializer.
///
/// These payloads are GitHub objects the mirror wrote itself - the largest,
/// an issue-timeline entry, runs to a few kilobytes - so the cap is a guard
/// against a corrupt or tampered row, not an expected limit.
const MAX_STORED_JSON_BYTES: usize = 1 << 20;

/// Which row a stored JSON column belongs to.
///
/// Every issue shares the column name `labels_json`, so the column alone does
/// not say which row failed to decode. Carrying the repository and the row's
/// own number or id means a corrupt payload can be found and repaired rather
/// than only counted.
#[derive(Clone, Copy)]
pub(super) struct StoredRow {
    pub repo_id: i64,
    /// The row's own key: an issue or pull-request number, or the GitHub id
    /// of a release or workflow job.
    pub key: i64,
}

impl StoredRow {
    pub(super) const fn new(repo_id: i64, key: i64) -> Self {
        Self { repo_id, key }
    }
}

pub(super) fn decode<T: serde::de::DeserializeOwned>(
    field: &str,
    row: StoredRow,
    raw: Option<&str>,
) -> Option<T> {
    let raw = raw?;
    if raw.len() > MAX_STORED_JSON_BYTES {
        tracing::warn!(
            field,
            repo_id = row.repo_id,
            key = row.key,
            bytes = raw.len(),
            limit = MAX_STORED_JSON_BYTES,
            "stored JSON exceeds the decode limit; serving the empty value"
        );
        return None;
    }
    match serde_json::from_str::<T>(raw) {
        Ok(value) => Some(value),
        Err(e) => {
            tracing::warn!(
                field,
                repo_id = row.repo_id,
                key = row.key,
                error = %e,
                bytes = raw.len(),
                "stored JSON could not be decoded; serving the empty value"
            );
            None
        }
    }
}

pub(super) fn decode_list<S, T>(field: &str, row: StoredRow, raw: Option<&str>) -> Vec<T>
where
    S: serde::de::DeserializeOwned + Into<T>,
{
    decode::<Vec<S>>(field, row, raw)
        .unwrap_or_default()
        .into_iter()
        .map(Into::into)
        .collect()
}

impl From<repositories::Model> for Repo {
    fn from(m: repositories::Model) -> Self {
        Self {
            id: m.id,
            node_id: m.node_id,
            owner: m.owner,
            name: m.name,
            full_name: m.full_name,
            default_branch: m.default_branch,
            private: m.private,
            pushed_at: m.pushed_at,
            stars: m.stars,
            forks: m.forks,
            description: m.description,
            clone_url: m.clone_url,
        }
    }
}

impl From<issues::Model> for Issue {
    fn from(m: issues::Model) -> Self {
        let row = StoredRow::new(m.repo_id, m.number);
        Self {
            id: m.id,
            node_id: m.node_id,
            repo_id: m.repo_id,
            number: m.number,
            title: m.title,
            body: m.body,
            state: m.state,
            is_pull_request: m.is_pull_request,
            created_at: m.created_at,
            updated_at: m.updated_at,
            closed_at: m.closed_at,
            html_url: m.html_url,
            author_login: m.author_login,
            author: decode::<StoredActor>("author_json", row, m.author_json.as_deref())
                .map(Into::into),
            assignees: decode_list::<StoredActor, _>(
                "assignees_json",
                row,
                m.assignees_json.as_deref(),
            ),
            labels: decode_list::<StoredLabel, _>("labels_json", row, m.labels_json.as_deref()),
            comments_count: m.comments_count,
            locked: m.locked,
        }
    }
}

impl From<pull_requests::Model> for PullRequest {
    fn from(m: pull_requests::Model) -> Self {
        let row = StoredRow::new(m.repo_id, m.number);
        Self {
            id: m.id,
            node_id: m.node_id,
            repo_id: m.repo_id,
            number: m.number,
            title: m.title,
            body: m.body,
            state: m.state,
            draft: m.draft,
            merged: m.merged,
            head_sha: m.head_sha,
            base_sha: m.base_sha,
            lines_added: m.lines_added,
            lines_removed: m.lines_removed,
            created_at: m.created_at,
            updated_at: m.updated_at,
            closed_at: m.closed_at,
            merged_at: m.merged_at,
            html_url: m.html_url,
            head_ref: m.head_ref,
            base_ref: m.base_ref,
            author_login: m.author_login,
            author: decode::<StoredActor>("author_json", row, m.author_json.as_deref())
                .map(Into::into),
            assignees: decode_list::<StoredActor, _>(
                "assignees_json",
                row,
                m.assignees_json.as_deref(),
            ),
            labels: decode_list::<StoredLabel, _>("labels_json", row, m.labels_json.as_deref()),
            comments_count: m.comments_count,
            locked: m.locked,
            requested_reviewers: decode_list::<StoredActor, _>(
                "requested_reviewers_json",
                row,
                m.requested_reviewers_json.as_deref(),
            ),
        }
    }
}

impl From<commits::Model> for Commit {
    fn from(m: commits::Model) -> Self {
        Self {
            repo_id: m.repo_id,
            sha: m.sha,
            message: m.message,
            author_login: m.author_login,
            committer_login: m.committer_login,
            authored_at: m.authored_at,
            committed_at: m.committed_at,
            additions: m.additions,
            deletions: m.deletions,
        }
    }
}

impl From<comments::Model> for Comment {
    fn from(m: comments::Model) -> Self {
        Self {
            id: m.id,
            repo_id: m.repo_id,
            issue_number: m.issue_number,
            author_login: m.author_login,
            body: m.body,
            created_at: m.created_at,
            updated_at: m.updated_at,
            html_url: m.html_url,
        }
    }
}

impl From<review_comments::Model> for ReviewComment {
    fn from(m: review_comments::Model) -> Self {
        Self {
            id: m.id,
            repo_id: m.repo_id,
            pull_number: m.pull_number,
            author_login: m.author_login,
            body: m.body,
            path: m.path,
            diff_hunk: m.diff_hunk,
            in_reply_to_id: m.in_reply_to_id,
            commit_id: m.commit_id,
            created_at: m.created_at,
            updated_at: m.updated_at,
            html_url: m.html_url,
            position: m.position,
            original_position: m.original_position,
            line: m.line,
            original_line: m.original_line,
            start_line: m.start_line,
            original_start_line: m.original_start_line,
            side: m.side,
            start_side: m.start_side,
            subject_type: m.subject_type,
            pull_request_review_id: m.pull_request_review_id,
        }
    }
}

impl From<reviews::Model> for Review {
    fn from(m: reviews::Model) -> Self {
        Self {
            id: m.id,
            repo_id: m.repo_id,
            pull_number: m.pull_number,
            author_login: m.author_login,
            state: m.state,
            body: m.body,
            commit_id: m.commit_id,
            submitted_at: m.submitted_at,
            html_url: m.html_url,
        }
    }
}

impl From<labels::Model> for Label {
    fn from(m: labels::Model) -> Self {
        Self {
            id: m.id,
            repo_id: m.repo_id,
            name: m.name,
            color: m.color,
            is_default: m.is_default,
            description: m.description,
        }
    }
}

impl From<milestones::Model> for Milestone {
    fn from(m: milestones::Model) -> Self {
        Self {
            id: m.id,
            repo_id: m.repo_id,
            number: m.number,
            title: m.title,
            state: m.state,
            description: m.description,
            open_issues: m.open_issues,
            closed_issues: m.closed_issues,
            due_on: m.due_on,
            created_at: m.created_at,
            updated_at: m.updated_at,
            closed_at: m.closed_at,
            html_url: m.html_url,
        }
    }
}

impl From<releases::Model> for Release {
    fn from(m: releases::Model) -> Self {
        Self {
            id: m.id,
            repo_id: m.repo_id,
            tag_name: m.tag_name,
            name: m.name,
            draft: m.draft,
            prerelease: m.prerelease,
            body: m.body,
            author_login: m.author_login,
            created_at: m.created_at,
            published_at: m.published_at,
            html_url: m.html_url,
            assets: decode_list::<StoredAsset, _>(
                "assets_json",
                StoredRow::new(m.repo_id, m.id),
                m.assets_json.as_deref(),
            ),
        }
    }
}

impl From<branches::Model> for Branch {
    fn from(m: branches::Model) -> Self {
        Self {
            repo_id: m.repo_id,
            name: m.name,
            commit_sha: m.commit_sha,
            protected: m.protected,
        }
    }
}

impl From<contributors::Model> for Contributor {
    fn from(m: contributors::Model) -> Self {
        Self {
            repo_id: m.repo_id,
            user_id: m.user_id,
            // '' is the NOT NULL column's stand-in for "no login".
            login: (!m.login.is_empty()).then_some(m.login),
            roles: m
                .roles
                .as_deref()
                .map(|raw| {
                    raw.split(',')
                        .filter(|role| !role.is_empty())
                        .map(ToOwned::to_owned)
                        .collect()
                })
                .unwrap_or_default(),
            first_seen_at: m.first_seen_at,
            last_seen_at: m.last_seen_at,
            account_type: m.account_type,
            avatar_url: m.avatar_url,
            html_url: m.html_url,
        }
    }
}

impl From<workflow_runs::Model> for WorkflowRun {
    fn from(m: workflow_runs::Model) -> Self {
        Self {
            id: m.id,
            repo_id: m.repo_id,
            workflow_id: m.workflow_id,
            run_number: m.run_number,
            run_attempt: m.run_attempt,
            name: m.name,
            event: m.event,
            status: m.status,
            conclusion: m.conclusion,
            head_branch: m.head_branch,
            head_sha: m.head_sha,
            created_at: m.created_at,
            updated_at: m.updated_at,
            html_url: m.html_url,
            actor_login: m.actor_login,
        }
    }
}

impl From<pull_request_files::Model> for PullRequestFile {
    fn from(m: pull_request_files::Model) -> Self {
        Self {
            repo_id: m.repo_id,
            pull_number: m.pull_number,
            filename: m.filename,
            status: m.status,
            additions: m.additions,
            deletions: m.deletions,
            changes: m.changes,
            previous_filename: m.previous_filename,
            patch: m.patch,
            sha: m.sha,
        }
    }
}

impl From<tags::Model> for Tag {
    fn from(m: tags::Model) -> Self {
        Self {
            repo_id: m.repo_id,
            name: m.name,
            commit_sha: m.commit_sha,
        }
    }
}

impl From<commit_files::Model> for CommitFile {
    fn from(m: commit_files::Model) -> Self {
        Self {
            repo_id: m.repo_id,
            commit_sha: m.commit_sha,
            filename: m.filename,
            status: m.status,
            additions: m.additions,
            deletions: m.deletions,
            changes: m.changes,
            previous_filename: m.previous_filename,
            sha: m.sha,
        }
    }
}

impl From<review_threads::Model> for ReviewThread {
    fn from(m: review_threads::Model) -> Self {
        Self {
            id: m.id,
            repo_id: m.repo_id,
            pull_number: m.pull_number,
            is_resolved: m.is_resolved,
            is_outdated: m.is_outdated,
            path: m.path,
            line: m.line,
            resolved_by: m.resolved_by,
            comments_count: m.comments_count,
        }
    }
}

impl From<commit_comments::Model> for CommitComment {
    fn from(m: commit_comments::Model) -> Self {
        Self {
            id: m.id,
            repo_id: m.repo_id,
            commit_sha: m.commit_sha,
            path: m.path,
            position: m.position,
            author_login: m.author_login,
            body: m.body,
            created_at: m.created_at,
            updated_at: m.updated_at,
            html_url: m.html_url,
        }
    }
}

impl From<issue_events::Model> for IssueEvent {
    fn from(m: issue_events::Model) -> Self {
        Self {
            id: m.id,
            repo_id: m.repo_id,
            issue_number: m.issue_number,
            event: m.event,
            actor_login: m.actor_login,
            label_name: m.label_name,
            assignee_login: m.assignee_login,
            milestone_title: m.milestone_title,
            commit_id: m.commit_id,
            created_at: m.created_at,
        }
    }
}

impl From<deployments::Model> for Deployment {
    fn from(m: deployments::Model) -> Self {
        Self {
            id: m.id,
            repo_id: m.repo_id,
            git_ref: m.git_ref,
            sha: m.sha,
            environment: m.environment,
            task: m.task,
            description: m.description,
            creator_login: m.creator_login,
            created_at: m.created_at,
            updated_at: m.updated_at,
        }
    }
}

impl From<pull_request_commits::Model> for PullRequestCommit {
    fn from(m: pull_request_commits::Model) -> Self {
        Self {
            repo_id: m.repo_id,
            pull_number: m.pull_number,
            sha: m.sha,
            message: m.message,
            author_login: m.author_login,
            committer_login: m.committer_login,
            authored_at: m.authored_at,
            committed_at: m.committed_at,
        }
    }
}

impl From<commit_statuses::Model> for CommitStatus {
    fn from(m: commit_statuses::Model) -> Self {
        Self {
            id: m.id,
            repo_id: m.repo_id,
            commit_sha: m.commit_sha,
            state: m.state,
            context: m.context,
            description: m.description,
            target_url: m.target_url,
            creator_login: m.creator_login,
            created_at: m.created_at,
            updated_at: m.updated_at,
        }
    }
}

impl From<workflow_jobs::Model> for WorkflowJob {
    fn from(m: workflow_jobs::Model) -> Self {
        Self {
            id: m.id,
            repo_id: m.repo_id,
            run_id: m.run_id,
            run_attempt: m.run_attempt,
            name: m.name,
            status: m.status,
            conclusion: m.conclusion,
            head_sha: m.head_sha,
            runner_name: m.runner_name,
            started_at: m.started_at,
            completed_at: m.completed_at,
            html_url: m.html_url,
            steps: decode_list::<StoredStep, _>(
                "steps_json",
                StoredRow::new(m.repo_id, m.id),
                m.steps_json.as_deref(),
            ),
        }
    }
}

impl From<issue_reactions::Model> for IssueReaction {
    fn from(m: issue_reactions::Model) -> Self {
        Self {
            id: m.id,
            repo_id: m.repo_id,
            issue_number: m.issue_number,
            content: m.content,
            user_login: m.user_login,
            created_at: m.created_at,
        }
    }
}

impl From<check_runs::Model> for CheckRun {
    fn from(m: check_runs::Model) -> Self {
        Self {
            id: m.id,
            repo_id: m.repo_id,
            head_sha: m.head_sha,
            name: m.name,
            status: m.status,
            conclusion: m.conclusion,
            started_at: m.started_at,
            completed_at: m.completed_at,
            html_url: m.html_url,
            details_url: m.details_url,
            check_suite_id: m.check_suite_id,
            app_slug: m.app_slug,
            app_name: m.app_name,
            output_title: m.output_title,
            output_summary: m.output_summary,
            annotations_count: m.annotations_count,
        }
    }
}

/// The stored timeline entry, or a stand-in naming the event when the row
/// cannot be decoded: the entry's shape is GitHub's, not the mirror's, so
/// there is nothing else to fall back to.
///
/// The warning carries the row's own key, since every timeline payload lives
/// in the same column and an operator repairing one needs to know which row
/// it was.
pub(super) fn timeline_payload(
    repo_id: i64,
    issue_number: i64,
    position: i64,
    event: &str,
    raw: Option<&str>,
) -> serde_json::Value {
    decode::<serde_json::Value>("payload_json", StoredRow::new(repo_id, issue_number), raw)
        .unwrap_or_else(|| {
            tracing::warn!(
                repo_id,
                issue_number,
                position,
                event,
                "issue-timeline payload could not be decoded; serving the event name alone"
            );
            serde_json::json!({ "event": event })
        })
}

impl From<issue_timeline::Model> for IssueTimelineEvent {
    fn from(m: issue_timeline::Model) -> Self {
        Self {
            repo_id: m.repo_id,
            issue_number: m.issue_number,
            position: m.position,
            payload: timeline_payload(
                m.repo_id,
                m.issue_number,
                m.position,
                &m.event,
                Some(&m.payload_json),
            ),
            event: m.event,
            created_at: m.created_at,
            actor_login: m.actor_login,
        }
    }
}
