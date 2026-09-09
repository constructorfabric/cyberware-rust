//! Response bodies of the mirror's REST surfaces.
//!
//! The GitHub-compatible endpoints (PRD §5.8) serve GitHub-shaped bodies:
//! bare arrays, GitHub field names, and nested objects, limited to the
//! subset of GitHub's schemas that the mirror stores. The extended
//! endpoints under `/github-mirror/v1/` (PRD §5.9) keep the platform
//! shapes.

use github_mirror_sdk::{
    Actor, Branch, CheckRun, Comment, Commit, CommitComment, CommitFile, CommitStatus, Contributor,
    Deployment, Issue, IssueEvent, IssueReaction, IssueTimelineEvent, Label, LabelRef, Milestone,
    PullRequest, PullRequestCommit, PullRequestFile, Release, ReleaseAsset, Repo, Review,
    ReviewComment, ReviewThread, Tag, WorkflowJob, WorkflowRun, WorkflowStep,
};
use github_mirror_sdk::{MirrorStatus, SyncSummary};

/// Deliberately without `api_base_url`: `/health` is registered
/// `.anonymous()`, and the configured upstream host is infrastructure detail
/// an unauthenticated caller has no business learning. It stays on
/// [`MirrorStatus`], which only authenticated callers reach.
#[derive(Debug)]
#[toolkit_macros::api_dto(response)]
pub struct GithubMirrorHealthDto {
    pub gear: String,
    pub version: String,
}

impl From<MirrorStatus> for GithubMirrorHealthDto {
    fn from(status: MirrorStatus) -> Self {
        Self {
            gear: status.gear,
            version: status.version,
        }
    }
}

/// GitHub `{ "login": ... }` actor object.
#[derive(Debug)]
#[toolkit_macros::api_dto(response)]
pub struct ActorDto {
    pub login: String,
    /// GitHub's numeric user id, when the mirror stored one — a login can be
    /// renamed and later reused, the id cannot.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node_id: Option<String>,
    /// `User`, `Bot` or `Organization`, under GitHub's own field name.
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub user_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub avatar_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub html_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub site_admin: Option<bool>,
}

/// One person as the sync stored them inside an issue or pull row.
impl From<Actor> for ActorDto {
    fn from(a: Actor) -> Self {
        Self {
            login: a.login,
            id: a.id,
            node_id: a.node_id,
            user_type: a.account_type,
            avatar_url: a.avatar_url,
            html_url: a.html_url,
            site_admin: a.site_admin,
        }
    }
}

impl From<LabelRef> for LabelRefDto {
    fn from(l: LabelRef) -> Self {
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

fn actors(people: Vec<Actor>) -> Vec<ActorDto> {
    people.into_iter().map(Into::into).collect()
}

fn labels(refs: Vec<LabelRef>) -> Vec<LabelRefDto> {
    refs.into_iter().map(Into::into).collect()
}

/// A downloadable file attached to a release.
#[derive(Debug)]
#[toolkit_macros::api_dto(response)]
pub struct ReleaseAssetDto {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node_id: Option<String>,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub download_count: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub browser_download_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
}

impl From<ReleaseAsset> for ReleaseAssetDto {
    fn from(a: ReleaseAsset) -> Self {
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

/// One step of a workflow job.
#[derive(Debug)]
#[toolkit_macros::api_dto(response)]
pub struct WorkflowStepDto {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conclusion: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub number: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<String>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

impl From<WorkflowStep> for WorkflowStepDto {
    fn from(st: WorkflowStep) -> Self {
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

/// A label as GitHub embeds it in an issue or pull request.
#[derive(Debug)]
#[toolkit_macros::api_dto(response)]
pub struct LabelRefDto {
    /// GitHub's label id; absent on rows mirrored before it was stored.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node_id: Option<String>,
    pub name: String,
    /// The chip colour a client paints the label with.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    /// Whether GitHub created the label with the repository.
    #[serde(rename = "default", skip_serializing_if = "Option::is_none")]
    pub is_default: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

fn actor(login: Option<String>) -> Option<ActorDto> {
    login.map(|login| ActorDto {
        login,
        id: None,
        node_id: None,
        user_type: None,
        avatar_url: None,
        html_url: None,
        site_admin: None,
    })
}

/// The author of an issue or pull request: GitHub's whole `user` object when
/// the sync stored one, and the bare login for rows mirrored before it did.
fn author(author: Option<Actor>, author_login: Option<String>) -> Option<ActorDto> {
    author.map(Into::into).or_else(|| actor(author_login))
}

/// GitHub `{ "sha": ... }` git reference object.
#[derive(Debug)]
#[toolkit_macros::api_dto(response)]
pub struct GitRefDto {
    pub sha: Option<String>,
}

/// GitHub-shaped `head`/`base` of a pull request: the branch name plus the
/// commit it pointed at. Distinct from [`GitRefDto`], which is the
/// commit-only shape branches and tags use.
#[derive(Debug)]
#[toolkit_macros::api_dto(response)]
pub struct PullRefDto {
    /// Branch name, under GitHub's own key.
    #[serde(rename = "ref")]
    pub git_ref: Option<String>,
    pub sha: Option<String>,
}

/// Marker object GitHub attaches to issues that are pull requests.
///
/// The braces are load-bearing: serde serializes a braced empty struct as
/// `{}` (GitHub's marker shape) but a unit struct as `null`.
#[allow(clippy::empty_structs_with_brackets)]
#[derive(Debug)]
#[toolkit_macros::api_dto(response)]
pub struct PullRequestMarkerDto {}

/// GitHub-shaped repository.
#[derive(Debug)]
#[toolkit_macros::api_dto(response)]
pub struct RepoDto {
    pub id: i64,
    /// GitHub's GraphQL global id, as GitHub itself returns it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node_id: Option<String>,
    pub name: String,
    pub full_name: String,
    pub owner: ActorDto,
    pub private: bool,
    pub description: Option<String>,
    pub default_branch: String,
    pub stargazers_count: i64,
    pub forks_count: i64,
    pub pushed_at: Option<String>,
    /// Always present: falls back to the canonical github.com URL when the
    /// row was mirrored before the column existed.
    pub clone_url: String,
}

impl From<Repo> for RepoDto {
    fn from(repo: Repo) -> Self {
        let clone_url = repo.clone_url_or_default();

        Self {
            id: repo.id,
            node_id: repo.node_id,
            name: repo.name,
            full_name: repo.full_name,
            owner: ActorDto {
                login: repo.owner,
                id: None,
                node_id: None,
                user_type: None,
                avatar_url: None,
                html_url: None,
                site_admin: None,
            },
            private: repo.private,
            description: repo.description,
            default_branch: repo.default_branch,
            stargazers_count: repo.stars,
            forks_count: repo.forks,
            pushed_at: repo.pushed_at,
            clone_url,
        }
    }
}

/// GitHub-shaped issue.
#[derive(Debug)]
#[toolkit_macros::api_dto(response)]
pub struct IssueDto {
    pub id: i64,
    /// GitHub's GraphQL global id, as GitHub itself returns it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node_id: Option<String>,
    pub number: i64,
    pub title: String,
    pub body: Option<String>,
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pull_request: Option<PullRequestMarkerDto>,
    pub created_at: String,
    pub updated_at: String,
    pub closed_at: Option<String>,
    pub html_url: Option<String>,
    /// Who opened it, who it is assigned to, and the labels it carries —
    /// the fields a GitHub client renders a list row from.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<ActorDto>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub assignees: Vec<ActorDto>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<LabelRefDto>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comments: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub locked: Option<bool>,
}

impl From<Issue> for IssueDto {
    fn from(i: Issue) -> Self {
        Self {
            id: i.id,
            node_id: i.node_id,
            number: i.number,
            title: i.title,
            body: i.body,
            state: i.state,
            pull_request: i.is_pull_request.then_some(PullRequestMarkerDto {}),
            created_at: i.created_at,
            updated_at: i.updated_at,
            closed_at: i.closed_at,
            html_url: i.html_url,
            user: author(i.author, i.author_login),
            assignees: actors(i.assignees),
            labels: labels(i.labels),
            comments: i.comments_count,
            locked: i.locked,
        }
    }
}

/// GitHub-shaped issue comment.
#[derive(Debug)]
#[toolkit_macros::api_dto(response)]
pub struct CommentDto {
    pub id: i64,
    pub user: Option<ActorDto>,
    pub body: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub html_url: Option<String>,
}

impl From<Comment> for CommentDto {
    fn from(c: Comment) -> Self {
        Self {
            id: c.id,
            user: actor(c.author_login),
            body: c.body,
            created_at: c.created_at,
            updated_at: c.updated_at,
            html_url: c.html_url,
        }
    }
}

/// GitHub-shaped pull request.
#[derive(Debug)]
#[toolkit_macros::api_dto(response)]
pub struct PullRequestDto {
    pub id: i64,
    /// GitHub's GraphQL global id, as GitHub itself returns it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node_id: Option<String>,
    pub number: i64,
    pub title: String,
    pub body: Option<String>,
    pub state: String,
    pub draft: bool,
    pub merged: bool,
    pub head: PullRefDto,
    pub base: PullRefDto,
    pub additions: i64,
    pub deletions: i64,
    pub created_at: String,
    pub updated_at: String,
    pub closed_at: Option<String>,
    pub merged_at: Option<String>,
    pub html_url: Option<String>,
    /// Who opened it, who it is assigned to, who was asked to review, and
    /// the labels it carries.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<ActorDto>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub assignees: Vec<ActorDto>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub requested_reviewers: Vec<ActorDto>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<LabelRefDto>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comments: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub locked: Option<bool>,
}

impl From<PullRequest> for PullRequestDto {
    fn from(p: PullRequest) -> Self {
        Self {
            id: p.id,
            node_id: p.node_id,
            number: p.number,
            title: p.title,
            body: p.body,
            state: p.state,
            draft: p.draft,
            merged: p.merged,
            head: PullRefDto {
                git_ref: p.head_ref,
                sha: p.head_sha,
            },
            base: PullRefDto {
                git_ref: p.base_ref,
                sha: p.base_sha,
            },
            additions: p.lines_added,
            deletions: p.lines_removed,
            created_at: p.created_at,
            updated_at: p.updated_at,
            closed_at: p.closed_at,
            merged_at: p.merged_at,
            html_url: p.html_url,
            user: author(p.author, p.author_login),
            assignees: actors(p.assignees),
            requested_reviewers: actors(p.requested_reviewers),
            labels: labels(p.labels),
            comments: p.comments_count,
            locked: p.locked,
        }
    }
}

/// GitHub-shaped pull-request review.
#[derive(Debug)]
#[toolkit_macros::api_dto(response)]
pub struct ReviewDto {
    pub id: i64,
    pub user: Option<ActorDto>,
    pub state: String,
    pub body: Option<String>,
    pub commit_id: Option<String>,
    pub submitted_at: Option<String>,
    pub html_url: Option<String>,
}

impl From<Review> for ReviewDto {
    fn from(r: Review) -> Self {
        Self {
            id: r.id,
            user: actor(r.author_login),
            state: r.state,
            body: r.body,
            commit_id: r.commit_id,
            submitted_at: r.submitted_at,
            html_url: r.html_url,
        }
    }
}

/// GitHub-shaped pull-request review comment.
#[derive(Debug)]
#[toolkit_macros::api_dto(response)]
pub struct ReviewCommentDto {
    pub id: i64,
    pub user: Option<ActorDto>,
    pub body: Option<String>,
    pub path: Option<String>,
    pub diff_hunk: Option<String>,
    pub in_reply_to_id: Option<i64>,
    pub commit_id: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub html_url: Option<String>,
    /// Line position in the current diff; absent once GitHub considers the
    /// commented-on line outdated.
    pub position: Option<i64>,
    /// Line position at comment-creation time — the stable anchor across
    /// later force-pushes.
    pub original_position: Option<i64>,
    /// GitHub's current diff anchors, which its own UI positions inline
    /// comments by; `position` above is the deprecated form.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub original_line: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_line: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub original_start_line: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub side: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_side: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subject_type: Option<String>,
    /// The review this inline comment belongs to, as GitHub reports it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pull_request_review_id: Option<i64>,
}

impl From<ReviewComment> for ReviewCommentDto {
    fn from(c: ReviewComment) -> Self {
        Self {
            id: c.id,
            user: actor(c.author_login),
            body: c.body,
            path: c.path,
            diff_hunk: c.diff_hunk,
            in_reply_to_id: c.in_reply_to_id,
            commit_id: c.commit_id,
            created_at: c.created_at,
            updated_at: c.updated_at,
            html_url: c.html_url,
            position: c.position,
            original_position: c.original_position,
            line: c.line,
            original_line: c.original_line,
            start_line: c.start_line,
            original_start_line: c.original_start_line,
            side: c.side,
            start_side: c.start_side,
            subject_type: c.subject_type,
            pull_request_review_id: c.pull_request_review_id,
        }
    }
}

/// GitHub-shaped changed file of a pull request.
#[derive(Debug)]
#[toolkit_macros::api_dto(response)]
pub struct PullRequestFileDto {
    pub sha: Option<String>,
    pub filename: String,
    pub status: String,
    pub additions: i64,
    pub deletions: i64,
    pub changes: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_filename: Option<String>,
    /// The file's unified diff, as GitHub returns it on this endpoint.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub patch: Option<String>,
}

impl From<PullRequestFile> for PullRequestFileDto {
    fn from(f: PullRequestFile) -> Self {
        Self {
            sha: f.sha,
            filename: f.filename,
            status: f.status,
            additions: f.additions,
            deletions: f.deletions,
            changes: f.changes,
            previous_filename: f.previous_filename,
            patch: f.patch,
        }
    }
}

impl From<CommitFile> for PullRequestFileDto {
    fn from(f: CommitFile) -> Self {
        Self {
            sha: f.sha,
            filename: f.filename,
            status: f.status,
            additions: f.additions,
            deletions: f.deletions,
            changes: f.changes,
            previous_filename: f.previous_filename,
            // A commit file carries no diff text: GitHub returns `patch`
            // only on the pull-request files endpoint.
            patch: None,
        }
    }
}

/// GitHub `commit.author` / `commit.committer` object.
#[derive(Debug)]
#[toolkit_macros::api_dto(response)]
pub struct GitPersonDto {
    pub date: Option<String>,
}

/// GitHub nested `commit` object.
#[derive(Debug)]
#[toolkit_macros::api_dto(response)]
pub struct CommitDetailDto {
    pub message: String,
    pub author: GitPersonDto,
    pub committer: GitPersonDto,
}

/// GitHub `stats` object of a commit detail response.
#[derive(Debug)]
#[toolkit_macros::api_dto(response)]
pub struct CommitStatsDto {
    pub additions: i64,
    pub deletions: i64,
    pub total: i64,
}

/// GitHub-shaped commit. The list endpoint leaves `stats` and `files`
/// out, matching GitHub; the single-commit endpoint fills them in.
#[derive(Debug)]
#[toolkit_macros::api_dto(response)]
pub struct CommitDto {
    pub sha: String,
    pub commit: CommitDetailDto,
    pub author: Option<ActorDto>,
    pub committer: Option<ActorDto>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stats: Option<CommitStatsDto>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<PullRequestFileDto>,
}

impl From<Commit> for CommitDto {
    fn from(c: Commit) -> Self {
        Self {
            sha: c.sha,
            commit: CommitDetailDto {
                message: c.message,
                author: GitPersonDto {
                    date: c.authored_at,
                },
                committer: GitPersonDto {
                    date: c.committed_at,
                },
            },
            author: actor(c.author_login),
            committer: actor(c.committer_login),
            stats: None,
            files: Vec::new(),
        }
    }
}

/// GitHub-shaped branch.
#[derive(Debug)]
#[toolkit_macros::api_dto(response)]
pub struct BranchDto {
    pub name: String,
    pub commit: GitRefDto,
    pub protected: bool,
}

impl From<Branch> for BranchDto {
    fn from(b: Branch) -> Self {
        Self {
            name: b.name,
            commit: GitRefDto {
                sha: Some(b.commit_sha),
            },
            protected: b.protected,
        }
    }
}

/// GitHub-shaped tag.
#[derive(Debug)]
#[toolkit_macros::api_dto(response)]
pub struct TagDto {
    pub name: String,
    pub commit: GitRefDto,
}

impl From<Tag> for TagDto {
    fn from(t: Tag) -> Self {
        Self {
            name: t.name,
            commit: GitRefDto {
                sha: Some(t.commit_sha),
            },
        }
    }
}

/// GitHub-shaped release.
#[derive(Debug)]
#[toolkit_macros::api_dto(response)]
pub struct ReleaseDto {
    pub id: i64,
    pub tag_name: String,
    pub name: Option<String>,
    pub draft: bool,
    pub prerelease: bool,
    pub body: Option<String>,
    pub author: Option<ActorDto>,
    pub created_at: String,
    pub published_at: Option<String>,
    pub html_url: Option<String>,
    /// The release's downloadable files; `[]` when it has none.
    pub assets: Vec<ReleaseAssetDto>,
}

impl From<Release> for ReleaseDto {
    fn from(r: Release) -> Self {
        Self {
            id: r.id,
            tag_name: r.tag_name,
            name: r.name,
            draft: r.draft,
            prerelease: r.prerelease,
            body: r.body,
            author: actor(r.author_login),
            created_at: r.created_at,
            published_at: r.published_at,
            html_url: r.html_url,
            assets: r.assets.into_iter().map(Into::into).collect(),
        }
    }
}

/// GitHub-shaped milestone.
#[derive(Debug)]
#[toolkit_macros::api_dto(response)]
pub struct MilestoneDto {
    pub id: i64,
    pub number: i64,
    pub title: String,
    pub state: String,
    pub description: Option<String>,
    pub open_issues: i64,
    pub closed_issues: i64,
    pub due_on: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub closed_at: Option<String>,
    pub html_url: Option<String>,
}

impl From<Milestone> for MilestoneDto {
    fn from(m: Milestone) -> Self {
        Self {
            id: m.id,
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

/// GitHub-shaped label.
#[derive(Debug)]
#[toolkit_macros::api_dto(response)]
pub struct LabelDto {
    pub id: i64,
    pub name: String,
    pub color: String,
    #[serde(rename = "default")]
    pub is_default: bool,
    pub description: Option<String>,
}

impl From<Label> for LabelDto {
    fn from(l: Label) -> Self {
        Self {
            id: l.id,
            name: l.name,
            color: l.color,
            is_default: l.is_default,
            description: l.description,
        }
    }
}

/// GitHub-shaped workflow run.
#[derive(Debug)]
#[toolkit_macros::api_dto(response)]
pub struct WorkflowRunDto {
    pub id: i64,
    pub workflow_id: i64,
    pub run_number: i64,
    pub run_attempt: i64,
    pub name: Option<String>,
    pub event: String,
    pub status: Option<String>,
    pub conclusion: Option<String>,
    pub head_branch: Option<String>,
    pub head_sha: String,
    pub actor: Option<ActorDto>,
    pub created_at: String,
    pub updated_at: String,
    pub html_url: Option<String>,
}

impl From<WorkflowRun> for WorkflowRunDto {
    fn from(w: WorkflowRun) -> Self {
        Self {
            id: w.id,
            workflow_id: w.workflow_id,
            run_number: w.run_number,
            run_attempt: w.run_attempt,
            name: w.name,
            event: w.event,
            status: w.status,
            conclusion: w.conclusion,
            head_branch: w.head_branch,
            head_sha: w.head_sha,
            actor: actor(w.actor_login),
            created_at: w.created_at,
            updated_at: w.updated_at,
            html_url: w.html_url,
        }
    }
}

/// GitHub `GET .../actions/runs` wrapper object.
#[derive(Debug)]
#[toolkit_macros::api_dto(response)]
pub struct WorkflowRunsPageDto {
    pub total_count: i64,
    pub workflow_runs: Vec<WorkflowRunDto>,
}

/// GitHub-shaped contributor.
#[derive(Debug)]
#[toolkit_macros::api_dto(response)]
pub struct ContributorDto {
    pub id: i64,
    /// Omitted for anonymous contributors, matching GitHub's shape.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub login: Option<String>,
    #[serde(rename = "type")]
    pub account_type: String,
    pub avatar_url: Option<String>,
    pub html_url: Option<String>,
    /// Beyond GitHub's shape: the capacities this person was seen in, and
    /// the window they were seen across.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub roles: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_seen_at: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_seen_at: Option<chrono::DateTime<chrono::Utc>>,
}

impl From<Contributor> for ContributorDto {
    fn from(c: Contributor) -> Self {
        Self {
            id: c.user_id,
            login: c.login,
            account_type: c.account_type,
            avatar_url: c.avatar_url,
            html_url: c.html_url,
            roles: c.roles,
            first_seen_at: c.first_seen_at,
            last_seen_at: c.last_seen_at,
        }
    }
}

/// Result of one sync pass, as served by the extended sync endpoint.
#[derive(Debug)]
#[toolkit_macros::api_dto(response)]
pub struct SyncSummaryDto {
    pub repository: String,
    pub issues_synced: u64,
    pub pull_requests_synced: u64,
    pub commits_synced: u64,
    pub comments_synced: u64,
    pub review_comments_synced: u64,
    pub reviews_synced: u64,
    pub labels_synced: u64,
    pub milestones_synced: u64,
    pub releases_synced: u64,
    pub branches_synced: u64,
    pub contributors_synced: u64,
    pub workflow_runs_synced: u64,
    pub pull_request_files_synced: u64,
    pub tags_synced: u64,
    pub commit_files_synced: u64,
    pub review_threads_synced: u64,
    pub commit_comments_synced: u64,
    pub issue_events_synced: u64,
    pub deployments_synced: u64,
    pub pull_request_commits_synced: u64,
    pub commit_statuses_synced: u64,
    pub workflow_jobs_synced: u64,
    pub issue_reactions_synced: u64,
    pub check_runs_synced: u64,
    pub issue_timeline_synced: u64,
    /// Rows hard-deleted because a complete listing no longer contained them.
    pub stale_rows_deleted: u64,
}

impl From<SyncSummary> for SyncSummaryDto {
    fn from(s: SyncSummary) -> Self {
        Self {
            repository: s.repository,
            issues_synced: s.issues_synced,
            pull_requests_synced: s.pull_requests_synced,
            commits_synced: s.commits_synced,
            comments_synced: s.comments_synced,
            review_comments_synced: s.review_comments_synced,
            reviews_synced: s.reviews_synced,
            labels_synced: s.labels_synced,
            milestones_synced: s.milestones_synced,
            releases_synced: s.releases_synced,
            branches_synced: s.branches_synced,
            contributors_synced: s.contributors_synced,
            workflow_runs_synced: s.workflow_runs_synced,
            pull_request_files_synced: s.pull_request_files_synced,
            tags_synced: s.tags_synced,
            commit_files_synced: s.commit_files_synced,
            review_threads_synced: s.review_threads_synced,
            commit_comments_synced: s.commit_comments_synced,
            issue_events_synced: s.issue_events_synced,
            deployments_synced: s.deployments_synced,
            pull_request_commits_synced: s.pull_request_commits_synced,
            commit_statuses_synced: s.commit_statuses_synced,
            workflow_jobs_synced: s.workflow_jobs_synced,
            issue_reactions_synced: s.issue_reactions_synced,
            check_runs_synced: s.check_runs_synced,
            issue_timeline_synced: s.issue_timeline_synced,
            stale_rows_deleted: s.stale_rows_deleted,
        }
    }
}

/// A mirrored changed file of a commit, served by the extended API.
#[derive(Debug)]
#[toolkit_macros::api_dto(response)]
pub struct CommitFileDto {
    pub repo_id: i64,
    pub commit_sha: String,
    pub filename: String,
    pub status: String,
    pub additions: i64,
    pub deletions: i64,
    pub changes: i64,
    pub previous_filename: Option<String>,
    pub sha: Option<String>,
}

impl From<CommitFile> for CommitFileDto {
    fn from(f: CommitFile) -> Self {
        Self {
            repo_id: f.repo_id,
            commit_sha: f.commit_sha,
            filename: f.filename,
            status: f.status,
            additions: f.additions,
            deletions: f.deletions,
            changes: f.changes,
            previous_filename: f.previous_filename,
            sha: f.sha,
        }
    }
}

/// A mirrored PR review conversation thread, served by the extended API.
#[derive(Debug)]
#[toolkit_macros::api_dto(response)]
pub struct ReviewThreadDto {
    pub id: String,
    pub repo_id: i64,
    pub pull_number: i64,
    pub is_resolved: bool,
    pub is_outdated: bool,
    pub path: Option<String>,
    pub line: Option<i64>,
    pub resolved_by: Option<String>,
    pub comments_count: i64,
}

impl From<ReviewThread> for ReviewThreadDto {
    fn from(t: ReviewThread) -> Self {
        Self {
            id: t.id,
            repo_id: t.repo_id,
            pull_number: t.pull_number,
            is_resolved: t.is_resolved,
            is_outdated: t.is_outdated,
            path: t.path,
            line: t.line,
            resolved_by: t.resolved_by,
            comments_count: t.comments_count,
        }
    }
}

/// GitHub-shaped commit comment.
#[derive(Debug)]
#[toolkit_macros::api_dto(response)]
pub struct CommitCommentDto {
    pub id: i64,
    pub user: Option<ActorDto>,
    pub body: Option<String>,
    pub path: Option<String>,
    pub position: Option<i64>,
    pub commit_id: String,
    pub created_at: String,
    pub updated_at: String,
    pub html_url: Option<String>,
}

impl From<CommitComment> for CommitCommentDto {
    fn from(c: CommitComment) -> Self {
        Self {
            id: c.id,
            user: actor(c.author_login),
            body: c.body,
            path: c.path,
            position: c.position,
            commit_id: c.commit_sha,
            created_at: c.created_at,
            updated_at: c.updated_at,
            html_url: c.html_url,
        }
    }
}

/// GitHub `{ "name": ... }` label reference inside an issue event.
#[derive(Debug)]
#[toolkit_macros::api_dto(response)]
pub struct EventLabelDto {
    pub name: String,
}

/// GitHub `{ "title": ... }` milestone reference inside an issue event.
#[derive(Debug)]
#[toolkit_macros::api_dto(response)]
pub struct EventMilestoneDto {
    pub title: String,
}

/// GitHub-shaped issue event.
#[derive(Debug)]
#[toolkit_macros::api_dto(response)]
pub struct IssueEventDto {
    pub id: i64,
    pub event: String,
    pub actor: Option<ActorDto>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<EventLabelDto>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assignee: Option<ActorDto>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub milestone: Option<EventMilestoneDto>,
    pub commit_id: Option<String>,
    pub created_at: String,
}

impl From<IssueEvent> for IssueEventDto {
    fn from(e: IssueEvent) -> Self {
        Self {
            id: e.id,
            event: e.event,
            actor: actor(e.actor_login),
            label: e.label_name.map(|name| EventLabelDto { name }),
            assignee: actor(e.assignee_login),
            milestone: e.milestone_title.map(|title| EventMilestoneDto { title }),
            commit_id: e.commit_id,
            created_at: e.created_at,
        }
    }
}

/// GitHub-shaped issue reaction.
#[derive(Debug)]
#[toolkit_macros::api_dto(response)]
pub struct IssueReactionDto {
    pub id: i64,
    pub user: Option<ActorDto>,
    /// `+1`, `-1`, `laugh`, `confused`, `heart`, `hooray`, `rocket`, `eyes`.
    pub content: String,
    pub created_at: String,
}

impl From<IssueReaction> for IssueReactionDto {
    fn from(r: IssueReaction) -> Self {
        Self {
            id: r.id,
            user: actor(r.user_login),
            content: r.content,
            created_at: r.created_at,
        }
    }
}

/// GitHub-shaped deployment.
#[derive(Debug)]
#[toolkit_macros::api_dto(response)]
pub struct DeploymentDto {
    pub id: i64,
    #[serde(rename = "ref")]
    pub git_ref: String,
    pub sha: String,
    pub environment: String,
    pub task: String,
    pub description: Option<String>,
    pub creator: Option<ActorDto>,
    pub created_at: String,
    pub updated_at: String,
}

impl From<Deployment> for DeploymentDto {
    fn from(d: Deployment) -> Self {
        Self {
            id: d.id,
            git_ref: d.git_ref,
            sha: d.sha,
            environment: d.environment,
            task: d.task,
            description: d.description,
            creator: actor(d.creator_login),
            created_at: d.created_at,
            updated_at: d.updated_at,
        }
    }
}

impl From<PullRequestCommit> for CommitDto {
    fn from(c: PullRequestCommit) -> Self {
        Self {
            sha: c.sha,
            commit: CommitDetailDto {
                message: c.message,
                author: GitPersonDto {
                    date: c.authored_at,
                },
                committer: GitPersonDto {
                    date: c.committed_at,
                },
            },
            author: actor(c.author_login),
            committer: actor(c.committer_login),
            stats: None,
            files: Vec::new(),
        }
    }
}

/// GitHub-shaped commit status.
#[derive(Debug)]
#[toolkit_macros::api_dto(response)]
pub struct CommitStatusDto {
    pub id: i64,
    pub state: String,
    pub context: String,
    pub description: Option<String>,
    pub target_url: Option<String>,
    pub creator: Option<ActorDto>,
    pub created_at: String,
    pub updated_at: String,
}

impl From<CommitStatus> for CommitStatusDto {
    fn from(s: CommitStatus) -> Self {
        Self {
            id: s.id,
            state: s.state,
            context: s.context,
            description: s.description,
            target_url: s.target_url,
            creator: actor(s.creator_login),
            created_at: s.created_at,
            updated_at: s.updated_at,
        }
    }
}

/// GitHub-shaped workflow job. `steps` is replayed from the raw GitHub
/// JSON the mirror stored, or an empty array when the sync never saw any.
#[derive(Debug)]
#[toolkit_macros::api_dto(response)]
pub struct WorkflowJobDto {
    pub id: i64,
    pub run_id: i64,
    pub run_attempt: i64,
    pub name: String,
    pub status: Option<String>,
    pub conclusion: Option<String>,
    pub head_sha: String,
    pub runner_name: Option<String>,
    pub started_at: Option<String>,
    pub completed_at: Option<String>,
    pub html_url: Option<String>,
    pub steps: Vec<WorkflowStepDto>,
}

impl From<WorkflowJob> for WorkflowJobDto {
    fn from(j: WorkflowJob) -> Self {
        Self {
            id: j.id,
            run_id: j.run_id,
            run_attempt: j.run_attempt,
            name: j.name,
            status: j.status,
            conclusion: j.conclusion,
            head_sha: j.head_sha,
            runner_name: j.runner_name,
            started_at: j.started_at,
            completed_at: j.completed_at,
            html_url: j.html_url,
            steps: j.steps.into_iter().map(Into::into).collect(),
        }
    }
}

/// GitHub `GET .../actions/runs/{id}/jobs` wrapper object.
#[derive(Debug)]
#[toolkit_macros::api_dto(response)]
pub struct WorkflowJobsPageDto {
    pub total_count: i64,
    pub jobs: Vec<WorkflowJobDto>,
}

/// GitHub-shaped `check_suite` back-reference of a check run.
#[derive(Debug)]
#[toolkit_macros::api_dto(response)]
pub struct CheckSuiteRefDto {
    pub id: i64,
}

/// GitHub-shaped `app` that produced a check run.
#[derive(Debug)]
#[toolkit_macros::api_dto(response)]
pub struct CheckAppDto {
    pub slug: Option<String>,
    pub name: Option<String>,
}

/// GitHub-shaped `output` of a check run, without the annotation bodies.
#[derive(Debug)]
#[toolkit_macros::api_dto(response)]
pub struct CheckRunOutputDto {
    pub title: Option<String>,
    pub summary: Option<String>,
    pub annotations_count: i64,
}

/// GitHub-shaped check run.
#[derive(Debug)]
#[toolkit_macros::api_dto(response)]
pub struct CheckRunDto {
    pub id: i64,
    pub head_sha: String,
    pub name: String,
    pub status: Option<String>,
    pub conclusion: Option<String>,
    pub started_at: Option<String>,
    pub completed_at: Option<String>,
    pub html_url: Option<String>,
    pub details_url: Option<String>,
    pub check_suite: Option<CheckSuiteRefDto>,
    pub app: Option<CheckAppDto>,
    pub output: CheckRunOutputDto,
}

impl From<CheckRun> for CheckRunDto {
    fn from(c: CheckRun) -> Self {
        let has_app = c.has_app();
        let app = has_app.then_some(CheckAppDto {
            slug: c.app_slug,
            name: c.app_name,
        });

        Self {
            id: c.id,
            head_sha: c.head_sha,
            name: c.name,
            status: c.status,
            conclusion: c.conclusion,
            started_at: c.started_at,
            completed_at: c.completed_at,
            html_url: c.html_url,
            details_url: c.details_url,
            check_suite: c.check_suite_id.map(|id| CheckSuiteRefDto { id }),
            app,
            output: CheckRunOutputDto {
                title: c.output_title,
                summary: c.output_summary,
                annotations_count: c.annotations_count,
            },
        }
    }
}

/// GitHub `GET .../commits/{sha}/check-runs` wrapper object.
#[derive(Debug)]
#[toolkit_macros::api_dto(response)]
pub struct CheckRunsPageDto {
    pub total_count: i64,
    pub check_runs: Vec<CheckRunDto>,
}

/// GitHub-shaped issue-timeline entry.
///
/// The timeline mixes about forty event types with incompatible payloads,
/// so the mirror replays the GitHub object it stored instead of forcing it
/// into one schema; `event` and `created_at` come back inside it, exactly
/// where GitHub puts them.
#[derive(Debug)]
#[toolkit_macros::api_dto(response)]
pub struct IssueTimelineEventDto {
    #[serde(flatten)]
    pub entry: serde_json::Value,
}

impl From<IssueTimelineEvent> for IssueTimelineEventDto {
    fn from(e: IssueTimelineEvent) -> Self {
        Self { entry: e.payload }
    }
}

/// GitHub-shaped `GET /user`. The mirror has no authenticated GitHub user
/// of its own, so it answers as itself: enough for a client to accept the
/// connection and show who it is talking to.
#[derive(Debug)]
#[toolkit_macros::api_dto(response)]
pub struct AuthenticatedUserDto {
    pub id: i64,
    pub login: String,
    pub name: Option<String>,
    #[serde(rename = "type")]
    pub user_type: String,
}
