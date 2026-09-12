//! Storage entities for the mirrored GitHub data.
//!
//! # Timestamps follow their provenance
//!
//! Two kinds of instant live in these tables, and they are typed differently
//! on purpose:
//!
//! - **Ours** — `extracted_at`, `first_seen_at`, `last_seen_at`: produced by
//!   the mirror's own clock or merge, never echoed verbatim to a caller, and
//!   compared in SQL (deletion reconciliation runs `extracted_at < watermark`).
//!   These are real timestamp columns (`DateTimeUtc`), as
//!   `cpt-cf-github-mirror-dbtable-*` in DESIGN specifies.
//! - **GitHub's** — `created_at`, `updated_at`, `closed_at`, `merged_at`,
//!   `committed_at`, and the rest: copied out of the upstream payload and
//!   handed back byte-for-byte by the GitHub-compatible surface, which PRD 5.8
//!   requires. They stay `String` so the mirror cannot alter what GitHub sent;
//!   round-tripping them through a datetime type would re-render them (`SeaORM`
//!   writes `SQLite` datetimes as `2026-08-20 00:00:00`, not RFC3339).
//!
//! A new column belongs to whichever group produced its value.

use sea_orm::entity::prelude::*;
use toolkit_db_macros::Scopable;
use uuid::Uuid;

pub mod repositories {
    use super::{DeriveEntityModel, DerivePrimaryKey, DeriveRelation, EnumIter, Scopable, Uuid};
    use sea_orm::entity::prelude::*;

    /// Mirrored GitHub repository, tenant-scoped from the start (gears-rust#4536).
    ///
    /// The reference implementation keys this table by the GitHub repository id
    /// alone; the gear keys it by `(tenant_id, id)` so two tenants can mirror
    /// the same repository without collision.
    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
    #[sea_orm(table_name = "gm_repositories")]
    #[secure(tenant_col = "tenant_id", resource_col = "id", no_owner, no_type)]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub tenant_id: Uuid,
        /// GitHub repository id.
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: i64,
        /// GitHub's GraphQL global id (DESIGN's `node_id`); `NULL` on
        /// rows mirrored before the column existed.
        pub node_id: Option<String>,
        pub owner: String,
        pub name: String,
        /// `owner/name` slug.
        pub full_name: String,
        pub default_branch: String,
        pub private: bool,
        /// GitHub's push stamp, kept verbatim (see the module doc); `None`
        /// when the repository has never been pushed to.
        pub pushed_at: Option<String>,
        pub stars: i64,
        pub forks: i64,
        pub description: Option<String>,
        /// HTTPS clone URL as GitHub reported it; null for rows mirrored
        /// before the column existed.
        pub clone_url: Option<String>,
        /// When a sync last wrote this row. Doubles as the
        /// deletion-reconciliation watermark; `None` on rows from before
        /// the column existed.
        pub extracted_at: Option<DateTimeUtc>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

pub mod http_cache {
    use super::{DeriveEntityModel, DerivePrimaryKey, DeriveRelation, EnumIter, Scopable, Uuid};
    use sea_orm::entity::prelude::*;

    /// One cached GitHub response, keyed by a content hash of the request.
    ///
    /// Tenant-partitioned: the design permits sharing public-repository
    /// responses across tenants, but that needs the visibility tracking and
    /// access grants of ADR-0002, which do not exist yet, so nothing is shared.
    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
    #[sea_orm(table_name = "gm_http_cache")]
    #[secure(
        tenant_col = "tenant_id",
        resource_col = "cache_key",
        no_owner,
        no_type
    )]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub tenant_id: Uuid,
        /// Hex SHA-256 of method + URL + `Accept`.
        #[sea_orm(primary_key, auto_increment = false)]
        pub cache_key: String,
        /// The URL this entry came from. Also what `clear_cache` matches on
        /// when a caller drops one repository's entries.
        pub url: String,
        /// The HTTP status the body came with (PRD §5.6 metadata).
        pub status: i32,
        /// `ETag` to replay as `If-None-Match`.
        pub etag: Option<String>,
        /// `Last-Modified` to replay as `If-Modified-Since`.
        pub last_modified: Option<String>,
        /// `rel="next"` from the `Link` header, when the list has more pages.
        /// Kept because a `304` may omit the header.
        pub next_page: Option<String>,
        /// The response body, encoded per `compression`.
        pub body: Vec<u8>,
        /// `none`, `gzip` or `zstd`.
        pub compression: String,
        /// SHA-256 of the **uncompressed** body, so the check is independent
        /// of the mode in force when the row was written.
        pub content_hash: String,
        pub fetched_at: String,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

pub mod issue_reactions {
    use super::{DeriveEntityModel, DerivePrimaryKey, DeriveRelation, EnumIter, Scopable, Uuid};
    use sea_orm::entity::prelude::*;

    /// Mirrored GitHub issue reaction, tenant-scoped.
    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
    #[sea_orm(table_name = "gm_issue_reactions")]
    #[secure(tenant_col = "tenant_id", resource_col = "id", no_owner, no_type)]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub tenant_id: Uuid,
        /// GitHub reaction id.
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: i64,
        /// Owning repository's GitHub id.
        pub repo_id: i64,
        /// Number of the issue or pull request the reaction belongs to.
        pub issue_number: i64,
        /// `+1`, `-1`, `laugh`, `confused`, `heart`, `hooray`, `rocket`, `eyes`.
        pub content: String,
        pub user_login: Option<String>,
        /// GitHub's own stamps, kept verbatim (see the module doc).
        pub created_at: String,
        /// When a sync last wrote this row. Doubles as the
        /// deletion-reconciliation watermark; `None` on rows from before
        /// the column existed.
        pub extracted_at: Option<DateTimeUtc>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

pub mod issue_timeline {
    use super::{DeriveEntityModel, DerivePrimaryKey, DeriveRelation, EnumIter, Scopable, Uuid};
    use sea_orm::entity::prelude::*;

    /// Mirrored GitHub issue-timeline entry, tenant-scoped.
    ///
    /// Timeline entries have no id of their own across all event types, so
    /// the row is keyed by its position in the issue's timeline.
    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
    #[sea_orm(table_name = "gm_issue_timeline")]
    #[secure(tenant_col = "tenant_id", resource_col = "repo_id", no_owner, no_type)]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub tenant_id: Uuid,
        /// Owning repository's GitHub id.
        #[sea_orm(primary_key, auto_increment = false)]
        pub repo_id: i64,
        /// Number of the issue or pull request the entry belongs to.
        #[sea_orm(primary_key, auto_increment = false)]
        pub issue_number: i64,
        /// Zero-based position in the timeline as GitHub ordered it.
        #[sea_orm(primary_key, auto_increment = false)]
        pub position: i64,
        /// `commented`, `committed`, `labeled`, `renamed`, ...
        pub event: String,
        /// GitHub's own stamps, kept verbatim (see the module doc).
        pub created_at: Option<String>,
        pub actor_login: Option<String>,
        /// The whole GitHub timeline entry, kept as raw JSON.
        pub payload_json: String,
        /// When a sync last wrote this row. Doubles as the
        /// deletion-reconciliation watermark; `None` on rows from before
        /// the column existed.
        pub extracted_at: Option<DateTimeUtc>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

pub mod issues {
    use super::{DeriveEntityModel, DerivePrimaryKey, DeriveRelation, EnumIter, Scopable, Uuid};
    use sea_orm::entity::prelude::*;

    /// Mirrored GitHub issue (pull requests included, flagged by
    /// `is_pull_request`), tenant-scoped like every mirror table.
    ///
    /// Keys: `(tenant_id, id)` primary key mirrors `gm_repositories`; a unique
    /// index on `(tenant_id, repo_id, number)` covers the natural GitHub
    /// lookup path.
    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
    #[sea_orm(table_name = "gm_issues")]
    #[secure(tenant_col = "tenant_id", resource_col = "id", no_owner, no_type)]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub tenant_id: Uuid,
        /// GitHub issue id.
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: i64,
        /// GitHub's GraphQL global id (DESIGN's `node_id`); `NULL` on
        /// rows mirrored before the column existed.
        pub node_id: Option<String>,
        /// Owning repository's GitHub id.
        pub repo_id: i64,
        pub number: i64,
        pub title: String,
        pub body: Option<String>,
        pub state: String,
        pub is_pull_request: bool,
        /// GitHub's own stamps, kept verbatim (see the module doc).
        pub created_at: String,
        pub updated_at: String,
        pub closed_at: Option<String>,
        pub html_url: Option<String>,
        /// When a sync last wrote this row. Doubles as the
        /// deletion-reconciliation watermark; `None` on rows from before
        /// the column existed.
        pub extracted_at: Option<DateTimeUtc>,
        /// What a GitHub client renders the row with: author, assignees,
        /// labels, comment count. Arrays are JSON.
        pub author_login: Option<String>,
        /// The author as GitHub's `user` object, JSON.
        pub author_json: Option<String>,
        pub assignees_json: Option<String>,
        pub labels_json: Option<String>,
        pub comments_count: Option<i64>,
        pub locked: Option<bool>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

pub mod pull_requests {
    use super::{DeriveEntityModel, DerivePrimaryKey, DeriveRelation, EnumIter, Scopable, Uuid};
    use sea_orm::entity::prelude::*;

    /// Mirrored GitHub pull request, tenant-scoped like every mirror table.
    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
    #[sea_orm(table_name = "gm_pull_requests")]
    #[secure(tenant_col = "tenant_id", resource_col = "id", no_owner, no_type)]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub tenant_id: Uuid,
        /// GitHub pull-request id.
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: i64,
        /// GitHub's GraphQL global id (DESIGN's `node_id`); `NULL` on
        /// rows mirrored before the column existed.
        pub node_id: Option<String>,
        /// Owning repository's GitHub id.
        pub repo_id: i64,
        pub number: i64,
        pub title: String,
        pub body: Option<String>,
        pub state: String,
        pub draft: bool,
        pub merged: bool,
        pub head_sha: Option<String>,
        pub base_sha: Option<String>,
        pub lines_added: i64,
        pub lines_removed: i64,
        /// GitHub's own stamps, kept verbatim (see the module doc).
        pub created_at: String,
        pub updated_at: String,
        pub closed_at: Option<String>,
        pub merged_at: Option<String>,
        pub html_url: Option<String>,
        /// Branch names of the pull request's head and base.
        pub head_ref: Option<String>,
        pub base_ref: Option<String>,
        /// When a sync last wrote this row. Doubles as the
        /// deletion-reconciliation watermark; `None` on rows from before
        /// the column existed.
        pub extracted_at: Option<DateTimeUtc>,
        /// What a GitHub client renders the row with: author, assignees,
        /// labels, comment count. Arrays are JSON.
        pub author_login: Option<String>,
        /// The author as GitHub's `user` object, JSON.
        pub author_json: Option<String>,
        pub assignees_json: Option<String>,
        pub labels_json: Option<String>,
        pub comments_count: Option<i64>,
        pub locked: Option<bool>,
        pub requested_reviewers_json: Option<String>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

pub mod commits {
    use super::{DeriveEntityModel, DerivePrimaryKey, DeriveRelation, EnumIter, Scopable, Uuid};
    use sea_orm::entity::prelude::*;

    /// Mirrored GitHub commit, tenant-scoped like every mirror table.
    ///
    /// Commits have no numeric GitHub id, so the key is
    /// `(tenant_id, repo_id, sha)` — the reference keys by `(repo_id, sha)`.
    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
    #[sea_orm(table_name = "gm_commits")]
    #[secure(tenant_col = "tenant_id", resource_col = "sha", no_owner, no_type)]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub tenant_id: Uuid,
        /// Owning repository's GitHub id.
        #[sea_orm(primary_key, auto_increment = false)]
        pub repo_id: i64,
        #[sea_orm(primary_key, auto_increment = false)]
        pub sha: String,
        pub message: String,
        pub author_login: Option<String>,
        pub committer_login: Option<String>,
        /// GitHub's own stamps, kept verbatim (see the module doc).
        pub authored_at: Option<String>,
        pub committed_at: Option<String>,
        pub additions: i64,
        pub deletions: i64,
        /// When a sync last wrote this row. Doubles as the
        /// deletion-reconciliation watermark; `None` on rows from before
        /// the column existed.
        pub extracted_at: Option<DateTimeUtc>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

pub mod check_runs {
    use super::{DeriveEntityModel, DerivePrimaryKey, DeriveRelation, EnumIter, Scopable, Uuid};
    use sea_orm::entity::prelude::*;

    /// Mirrored GitHub check run, tenant-scoped.
    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
    #[sea_orm(table_name = "gm_check_runs")]
    #[secure(tenant_col = "tenant_id", resource_col = "id", no_owner, no_type)]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub tenant_id: Uuid,
        /// GitHub check-run id.
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: i64,
        /// Owning repository's GitHub id.
        pub repo_id: i64,
        /// SHA of the commit the check ran against.
        pub head_sha: String,
        pub name: String,
        /// `queued`, `in_progress`, or `completed`.
        pub status: Option<String>,
        /// `success`, `failure`, `neutral`, `cancelled`, ... — absent until completed.
        pub conclusion: Option<String>,
        /// GitHub's own stamps, kept verbatim (see the module doc).
        pub started_at: Option<String>,
        pub completed_at: Option<String>,
        pub html_url: Option<String>,
        pub details_url: Option<String>,
        pub check_suite_id: Option<i64>,
        pub app_slug: Option<String>,
        pub app_name: Option<String>,
        pub output_title: Option<String>,
        pub output_summary: Option<String>,
        pub annotations_count: i64,
        /// When a sync last wrote this row. Doubles as the
        /// deletion-reconciliation watermark; `None` on rows from before
        /// the column existed.
        pub extracted_at: Option<DateTimeUtc>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

pub mod comments {
    use super::{DeriveEntityModel, DerivePrimaryKey, DeriveRelation, EnumIter, Scopable, Uuid};
    use sea_orm::entity::prelude::*;

    /// Mirrored GitHub issue/PR comment, tenant-scoped like every mirror table.
    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
    #[sea_orm(table_name = "gm_comments")]
    #[secure(tenant_col = "tenant_id", resource_col = "id", no_owner, no_type)]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub tenant_id: Uuid,
        /// GitHub comment id.
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: i64,
        /// Owning repository's GitHub id.
        pub repo_id: i64,
        /// Owning issue/PR number.
        pub issue_number: i64,
        pub author_login: Option<String>,
        pub body: Option<String>,
        /// GitHub's own stamps, kept verbatim (see the module doc).
        pub created_at: String,
        pub updated_at: String,
        pub html_url: Option<String>,
        /// When a sync last wrote this row. Doubles as the
        /// deletion-reconciliation watermark; `None` on rows from before
        /// the column existed.
        pub extracted_at: Option<DateTimeUtc>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

pub mod review_comments {
    use super::{DeriveEntityModel, DerivePrimaryKey, DeriveRelation, EnumIter, Scopable, Uuid};
    use sea_orm::entity::prelude::*;

    /// Mirrored GitHub PR review comment (inline diff comment), tenant-scoped.
    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
    #[sea_orm(table_name = "gm_review_comments")]
    #[secure(tenant_col = "tenant_id", resource_col = "id", no_owner, no_type)]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub tenant_id: Uuid,
        /// GitHub review-comment id.
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: i64,
        /// Owning repository's GitHub id.
        pub repo_id: i64,
        /// Owning pull-request number.
        pub pull_number: i64,
        pub author_login: Option<String>,
        pub body: Option<String>,
        pub path: Option<String>,
        pub diff_hunk: Option<String>,
        pub in_reply_to_id: Option<i64>,
        pub commit_id: Option<String>,
        /// GitHub's own stamps, kept verbatim (see the module doc).
        pub created_at: String,
        pub updated_at: String,
        pub html_url: Option<String>,
        /// Line position in the current diff. GitHub omits this once the
        /// commented-on line is outdated by a later push.
        pub position: Option<i64>,
        /// Line position at comment-creation time — the stable anchor for
        /// re-resolving where a comment pointed before later force-pushes.
        pub original_position: Option<i64>,
        /// GitHub's current diff anchors, replacing `position`: the line
        /// and side a comment sits on, plus the start of a multi-line
        /// selection.
        pub line: Option<i64>,
        pub original_line: Option<i64>,
        pub start_line: Option<i64>,
        pub original_start_line: Option<i64>,
        pub side: Option<String>,
        pub start_side: Option<String>,
        pub subject_type: Option<String>,
        /// The review this inline comment belongs to; `NULL` when it
        /// belongs to none.
        pub pull_request_review_id: Option<i64>,
        /// When a sync last wrote this row. Doubles as the
        /// deletion-reconciliation watermark; `None` on rows from before
        /// the column existed.
        pub extracted_at: Option<DateTimeUtc>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

pub mod reviews {
    use super::{DeriveEntityModel, DerivePrimaryKey, DeriveRelation, EnumIter, Scopable, Uuid};
    use sea_orm::entity::prelude::*;

    /// Mirrored GitHub PR review (the verdict object), tenant-scoped.
    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
    #[sea_orm(table_name = "gm_reviews")]
    #[secure(tenant_col = "tenant_id", resource_col = "id", no_owner, no_type)]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub tenant_id: Uuid,
        /// GitHub review id.
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: i64,
        /// Owning repository's GitHub id.
        pub repo_id: i64,
        /// Owning pull-request number.
        pub pull_number: i64,
        pub author_login: Option<String>,
        /// `APPROVED`, `CHANGES_REQUESTED`, `COMMENTED`, `DISMISSED`, or `PENDING`.
        pub state: String,
        pub body: Option<String>,
        pub commit_id: Option<String>,
        /// GitHub's own stamp, kept verbatim (see the module doc); absent for
        /// PENDING reviews.
        pub submitted_at: Option<String>,
        pub html_url: Option<String>,
        /// When a sync last wrote this row. Doubles as the
        /// deletion-reconciliation watermark; `None` on rows from before
        /// the column existed.
        pub extracted_at: Option<DateTimeUtc>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

pub mod labels {
    use super::{DeriveEntityModel, DerivePrimaryKey, DeriveRelation, EnumIter, Scopable, Uuid};
    use sea_orm::entity::prelude::*;

    /// Mirrored GitHub issue/PR label, tenant-scoped.
    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
    #[sea_orm(table_name = "gm_labels")]
    #[secure(tenant_col = "tenant_id", resource_col = "id", no_owner, no_type)]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub tenant_id: Uuid,
        /// GitHub label id.
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: i64,
        /// Owning repository's GitHub id.
        pub repo_id: i64,
        pub name: String,
        /// Hex color without the leading `#`.
        pub color: String,
        /// True for GitHub's default label set.
        pub is_default: bool,
        pub description: Option<String>,
        /// When a sync last wrote this row. Doubles as the
        /// deletion-reconciliation watermark; `None` on rows from before
        /// the column existed.
        pub extracted_at: Option<DateTimeUtc>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

pub mod milestones {
    use super::{DeriveEntityModel, DerivePrimaryKey, DeriveRelation, EnumIter, Scopable, Uuid};
    use sea_orm::entity::prelude::*;

    /// Mirrored GitHub milestone, tenant-scoped.
    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
    #[sea_orm(table_name = "gm_milestones")]
    #[secure(tenant_col = "tenant_id", resource_col = "id", no_owner, no_type)]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub tenant_id: Uuid,
        /// GitHub milestone id.
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: i64,
        /// Owning repository's GitHub id.
        pub repo_id: i64,
        /// Milestone number within the repository.
        pub number: i64,
        pub title: String,
        /// open or closed.
        pub state: String,
        pub description: Option<String>,
        pub open_issues: i64,
        pub closed_issues: i64,
        /// GitHub's own stamps, kept verbatim (see the module doc).
        pub due_on: Option<String>,
        pub created_at: String,
        pub updated_at: String,
        pub closed_at: Option<String>,
        pub html_url: Option<String>,
        /// When a sync last wrote this row. Doubles as the
        /// deletion-reconciliation watermark; `None` on rows from before
        /// the column existed.
        pub extracted_at: Option<DateTimeUtc>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

pub mod releases {
    use super::{DeriveEntityModel, DerivePrimaryKey, DeriveRelation, EnumIter, Scopable, Uuid};
    use sea_orm::entity::prelude::*;

    /// Mirrored GitHub release, tenant-scoped.
    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
    #[sea_orm(table_name = "gm_releases")]
    #[secure(tenant_col = "tenant_id", resource_col = "id", no_owner, no_type)]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub tenant_id: Uuid,
        /// GitHub release id.
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: i64,
        /// Owning repository's GitHub id.
        pub repo_id: i64,
        pub tag_name: String,
        pub name: Option<String>,
        pub draft: bool,
        pub prerelease: bool,
        pub body: Option<String>,
        pub author_login: Option<String>,
        /// GitHub's own stamps, kept verbatim (see the module doc).
        pub created_at: String,
        /// Absent for drafts.
        pub published_at: Option<String>,
        pub html_url: Option<String>,
        /// The release's assets serialized as JSON (name, download URL,
        /// size); `NULL` when the release has none.
        pub assets_json: Option<String>,
        /// When a sync last wrote this row. Doubles as the
        /// deletion-reconciliation watermark; `None` on rows from before
        /// the column existed.
        pub extracted_at: Option<DateTimeUtc>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

pub mod branches {
    use super::{DeriveEntityModel, DerivePrimaryKey, DeriveRelation, EnumIter, Scopable, Uuid};
    use sea_orm::entity::prelude::*;

    /// Mirrored GitHub branch head, tenant-scoped.
    ///
    /// Branches have no numeric GitHub id, so the key is
    /// `(tenant_id, repo_id, name)` — like `gm_commits` keys by sha.
    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
    #[sea_orm(table_name = "gm_branches")]
    #[secure(tenant_col = "tenant_id", resource_col = "name", no_owner, no_type)]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub tenant_id: Uuid,
        /// Owning repository's GitHub id.
        #[sea_orm(primary_key, auto_increment = false)]
        pub repo_id: i64,
        #[sea_orm(primary_key, auto_increment = false)]
        pub name: String,
        /// SHA the branch currently points at.
        pub commit_sha: String,
        pub protected: bool,
        /// When a sync last wrote this row. Doubles as the
        /// deletion-reconciliation watermark; `None` on rows from before
        /// the column existed.
        pub extracted_at: Option<DateTimeUtc>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

pub mod contributors {
    use super::{DeriveEntityModel, DerivePrimaryKey, DeriveRelation, EnumIter, Scopable, Uuid};
    use sea_orm::entity::prelude::*;

    /// Mirrored GitHub repository contributor, tenant-scoped.
    ///
    /// A row is one user's contribution count in one repository, so the key
    /// is `(tenant_id, repo_id, user_id)`.
    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
    #[sea_orm(table_name = "gm_contributors")]
    #[secure(tenant_col = "tenant_id", resource_col = "user_id", no_owner, no_type)]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub tenant_id: Uuid,
        /// Owning repository's GitHub id.
        #[sea_orm(primary_key, auto_increment = false)]
        pub repo_id: i64,
        /// The contributor's GitHub user id.
        #[sea_orm(primary_key, auto_increment = false)]
        pub user_id: i64,
        pub login: String,
        /// User, Bot, or Organization.
        pub account_type: String,
        pub avatar_url: Option<String>,
        pub html_url: Option<String>,
        /// The association roles this person was seen in, comma-separated as
        /// DESIGN's contributors table specifies; `NULL` on rows written
        /// before contributors were derived.
        pub roles: Option<String>,
        /// The window this person was seen across.
        pub first_seen_at: Option<DateTimeUtc>,
        pub last_seen_at: Option<DateTimeUtc>,
        /// When a sync last wrote this row. Doubles as the
        /// deletion-reconciliation watermark; `None` on rows from before
        /// the column existed.
        pub extracted_at: Option<DateTimeUtc>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

pub mod workflow_runs {
    use super::{DeriveEntityModel, DerivePrimaryKey, DeriveRelation, EnumIter, Scopable, Uuid};
    use sea_orm::entity::prelude::*;

    /// Mirrored GitHub Actions workflow run, tenant-scoped.
    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
    #[sea_orm(table_name = "gm_workflow_runs")]
    #[secure(tenant_col = "tenant_id", resource_col = "id", no_owner, no_type)]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub tenant_id: Uuid,
        /// GitHub run id.
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: i64,
        /// Owning repository's GitHub id.
        pub repo_id: i64,
        /// Id of the workflow definition this run belongs to.
        pub workflow_id: i64,
        pub run_number: i64,
        /// Retry attempt, 1 for the first run.
        pub run_attempt: i64,
        pub name: Option<String>,
        /// Event that triggered the run (`push`, `pull_request`, ...).
        pub event: String,
        /// `queued`, `in_progress`, or `completed`.
        pub status: Option<String>,
        /// `success`, `failure`, `cancelled`, ... — absent until completed.
        pub conclusion: Option<String>,
        pub head_branch: Option<String>,
        pub head_sha: String,
        /// GitHub's own stamps, kept verbatim (see the module doc).
        pub created_at: String,
        pub updated_at: String,
        pub html_url: Option<String>,
        pub actor_login: Option<String>,
        /// When a sync last wrote this row. Doubles as the
        /// deletion-reconciliation watermark; `None` on rows from before
        /// the column existed.
        pub extracted_at: Option<DateTimeUtc>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

pub mod pull_request_files {
    use super::{DeriveEntityModel, DerivePrimaryKey, DeriveRelation, EnumIter, Scopable, Uuid};
    use sea_orm::entity::prelude::*;

    /// Mirrored changed file of one pull request, tenant-scoped.
    ///
    /// Changed files have no numeric GitHub id, so the key is
    /// `(tenant_id, repo_id, pull_number, filename)`.
    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
    #[sea_orm(table_name = "gm_pull_request_files")]
    #[secure(tenant_col = "tenant_id", resource_col = "filename", no_owner, no_type)]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub tenant_id: Uuid,
        /// Owning repository's GitHub id.
        #[sea_orm(primary_key, auto_increment = false)]
        pub repo_id: i64,
        /// Owning pull-request number.
        #[sea_orm(primary_key, auto_increment = false)]
        pub pull_number: i64,
        #[sea_orm(primary_key, auto_increment = false)]
        pub filename: String,
        /// added, modified, removed, or renamed.
        pub status: String,
        pub additions: i64,
        pub deletions: i64,
        pub changes: i64,
        /// Present when status is renamed.
        pub previous_filename: Option<String>,
        /// The file's unified diff; `NULL` when GitHub omitted it.
        pub patch: Option<String>,
        /// Blob SHA of the file version in this pull request.
        pub sha: Option<String>,
        /// When a sync last wrote this row. Doubles as the
        /// deletion-reconciliation watermark; `None` on rows from before
        /// the column existed.
        pub extracted_at: Option<DateTimeUtc>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

pub mod repo_sync_status {
    use super::{DeriveEntityModel, DerivePrimaryKey, DeriveRelation, EnumIter, Scopable, Uuid};
    use sea_orm::entity::prelude::*;

    /// Per-repository run status — the durable half of resume-by-rescan
    /// (DESIGN §2 "recoverability comes from ... per-repo run-status rows").
    ///
    /// Keyed by the slug rather than the GitHub id because the row is written
    /// when a sync is requested, before anything about the repository is
    /// known. A run that never completes leaves `in_progress` behind, which is
    /// what the resume operation looks for.
    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
    #[sea_orm(table_name = "gm_repo_sync_status")]
    #[secure(
        tenant_col = "tenant_id",
        resource_col = "repo_full_name",
        no_owner,
        no_type
    )]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub tenant_id: Uuid,
        /// `owner/name` slug.
        #[sea_orm(primary_key, auto_increment = false)]
        pub repo_full_name: String,
        /// GitHub repository id, filled once a run has fetched it.
        pub repo_id: Option<i64>,
        /// `in_progress` or `complete` (PRD §5.2).
        pub status: String,
        /// The run that last wrote this row.
        pub last_session_id: Option<Uuid>,
        /// RFC3339 time of the last run that completed.
        pub last_synced_at: Option<String>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

pub mod sync_sessions {
    use super::{DeriveEntityModel, DerivePrimaryKey, DeriveRelation, EnumIter, Scopable, Uuid};
    use sea_orm::entity::prelude::*;

    /// One sync run of one repository, tenant-scoped. The row is the durable
    /// face of a session: what was asked, where it stands, how it ended.
    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
    #[sea_orm(table_name = "gm_sync_sessions")]
    #[secure(tenant_col = "tenant_id", resource_col = "id", no_owner, no_type)]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub tenant_id: Uuid,
        /// Session id, minted by the gear (not by GitHub).
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: Uuid,
        /// `owner/name` slug the session was asked to sync.
        pub repo_full_name: String,
        /// GitHub repository id, filled once the repository row exists.
        pub repo_id: Option<i64>,
        /// `queued`, `in_progress`, `complete`, `failed`, or `interrupted`.
        /// The middle three are DESIGN §3.7's vocabulary; `queued` and
        /// `interrupted` are additions the background worker needs.
        pub status: String,
        /// 0-100, monotonically non-decreasing, written by the run's
        /// heartbeat (DESIGN §4 "Progress").
        pub progress_percent: i32,
        /// Failure detail when `status = failed`.
        pub error: Option<String>,
        /// The run's `SyncSummary` as raw JSON, when it completed.
        pub summary_json: Option<String>,
        /// RFC3339 timestamps kept as text (engine-agnostic), as elsewhere.
        pub created_at: String,
        pub started_at: Option<String>,
        /// Stamped by every heartbeat, so duration is readable mid-run.
        pub ended_at: Option<String>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

pub mod sync_watermarks {
    use super::{DeriveEntityModel, DerivePrimaryKey, DeriveRelation, EnumIter, Scopable, Uuid};
    use sea_orm::entity::prelude::*;

    /// Incremental-sweep watermark for one `(repository, endpoint family)`
    /// pair: the promoted high-water mark plus the staged candidate, so an
    /// interrupted sweep never advances the mark it did not finish earning.
    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
    #[sea_orm(table_name = "gm_sync_watermarks")]
    #[secure(tenant_col = "tenant_id", resource_col = "repo_id", no_owner, no_type)]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub tenant_id: Uuid,
        /// Owning repository's GitHub id.
        #[sea_orm(primary_key, auto_increment = false)]
        pub repo_id: i64,
        /// Endpoint family (`issues`, `pull_requests`, ...).
        #[sea_orm(primary_key, auto_increment = false)]
        pub family: String,
        /// RFC3339 last promoted high-water mark; absent on first run.
        pub last_seen_updated_at: Option<String>,
        /// `ETag` of page 1 from the last successful sweep (short-circuit).
        pub page1_etag: Option<String>,
        /// Crash-safety flag: a sweep is currently running.
        pub sweep_in_progress: bool,
        /// RFC3339 staged high-water, promoted on sweep success.
        pub candidate_high_water: Option<String>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

pub mod tags {
    use super::{DeriveEntityModel, DerivePrimaryKey, DeriveRelation, EnumIter, Scopable, Uuid};
    use sea_orm::entity::prelude::*;

    /// Mirrored GitHub tag, tenant-scoped.
    ///
    /// Tags have no numeric GitHub id, so the key is
    /// `(tenant_id, repo_id, name)` — like `gm_branches`.
    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
    #[sea_orm(table_name = "gm_tags")]
    #[secure(tenant_col = "tenant_id", resource_col = "name", no_owner, no_type)]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub tenant_id: Uuid,
        /// Owning repository's GitHub id.
        #[sea_orm(primary_key, auto_increment = false)]
        pub repo_id: i64,
        #[sea_orm(primary_key, auto_increment = false)]
        pub name: String,
        /// SHA the tag points at.
        pub commit_sha: String,
        /// When a sync last wrote this row. Doubles as the
        /// deletion-reconciliation watermark; `None` on rows from before
        /// the column existed.
        pub extracted_at: Option<DateTimeUtc>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

pub mod commit_files {
    use super::{DeriveEntityModel, DerivePrimaryKey, DeriveRelation, EnumIter, Scopable, Uuid};
    use sea_orm::entity::prelude::*;

    /// Mirrored changed file of one commit, tenant-scoped.
    ///
    /// Changed files have no numeric GitHub id, so the key is
    /// `(tenant_id, repo_id, commit_sha, filename)`.
    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
    #[sea_orm(table_name = "gm_commit_files")]
    #[secure(tenant_col = "tenant_id", resource_col = "filename", no_owner, no_type)]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub tenant_id: Uuid,
        /// Owning repository's GitHub id.
        #[sea_orm(primary_key, auto_increment = false)]
        pub repo_id: i64,
        /// Owning commit SHA.
        #[sea_orm(primary_key, auto_increment = false)]
        pub commit_sha: String,
        #[sea_orm(primary_key, auto_increment = false)]
        pub filename: String,
        /// added, modified, removed, or renamed.
        pub status: String,
        pub additions: i64,
        pub deletions: i64,
        pub changes: i64,
        /// Present when status is renamed.
        pub previous_filename: Option<String>,
        /// Blob SHA of the file version in this commit.
        pub sha: Option<String>,
        /// When a sync last wrote this row. Doubles as the
        /// deletion-reconciliation watermark; `None` on rows from before
        /// the column existed.
        pub extracted_at: Option<DateTimeUtc>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

pub mod review_threads {
    use super::{DeriveEntityModel, DerivePrimaryKey, DeriveRelation, EnumIter, Scopable, Uuid};
    use sea_orm::entity::prelude::*;

    /// Mirrored PR review conversation thread, tenant-scoped.
    ///
    /// Threads come from GitHub's GraphQL API, so the id is an opaque node
    /// string rather than a number.
    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
    #[sea_orm(table_name = "gm_review_threads")]
    #[secure(tenant_col = "tenant_id", resource_col = "id", no_owner, no_type)]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub tenant_id: Uuid,
        /// GraphQL node id of the thread.
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: String,
        /// Owning repository's GitHub id.
        pub repo_id: i64,
        /// Owning pull-request number.
        pub pull_number: i64,
        pub is_resolved: bool,
        pub is_outdated: bool,
        /// File path the thread is anchored to.
        pub path: Option<String>,
        /// Line the thread is anchored to.
        pub line: Option<i64>,
        /// Login of whoever resolved the thread.
        pub resolved_by: Option<String>,
        pub comments_count: i64,
        /// When a sync last wrote this row. Doubles as the
        /// deletion-reconciliation watermark; `None` on rows from before
        /// the column existed.
        pub extracted_at: Option<DateTimeUtc>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

pub mod commit_comments {
    use super::{DeriveEntityModel, DerivePrimaryKey, DeriveRelation, EnumIter, Scopable, Uuid};
    use sea_orm::entity::prelude::*;

    /// Mirrored comment left directly on a commit, tenant-scoped.
    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
    #[sea_orm(table_name = "gm_commit_comments")]
    #[secure(tenant_col = "tenant_id", resource_col = "id", no_owner, no_type)]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub tenant_id: Uuid,
        /// GitHub comment id.
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: i64,
        /// Owning repository's GitHub id.
        pub repo_id: i64,
        /// SHA of the commit the comment is on.
        pub commit_sha: String,
        /// File path when the comment is pinned to a line.
        pub path: Option<String>,
        /// Diff position when the comment is pinned to a line.
        pub position: Option<i64>,
        pub author_login: Option<String>,
        pub body: Option<String>,
        /// GitHub's own stamps, kept verbatim (see the module doc).
        pub created_at: String,
        pub updated_at: String,
        pub html_url: Option<String>,
        /// When a sync last wrote this row. Doubles as the
        /// deletion-reconciliation watermark; `None` on rows from before
        /// the column existed.
        pub extracted_at: Option<DateTimeUtc>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

pub mod entity_fingerprints {
    use super::{DeriveEntityModel, DerivePrimaryKey, DeriveRelation, EnumIter, Scopable, Uuid};
    use sea_orm::entity::prelude::*;

    /// Durable change-detection state for one mirrored entity: what the sync
    /// last saw of it in a list response, so the next sweep can skip
    /// unchanged rows without fetching their detail (GAPS.md #19).
    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
    #[sea_orm(table_name = "gm_entity_fingerprints")]
    #[secure(tenant_col = "tenant_id", resource_col = "repo_id", no_owner, no_type)]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub tenant_id: Uuid,
        /// Owning repository's GitHub id.
        #[sea_orm(primary_key, auto_increment = false)]
        pub repo_id: i64,
        /// Endpoint family the entity came from (`issues`, `pull_requests`, ...).
        #[sea_orm(primary_key, auto_increment = false)]
        pub family: String,
        /// The entity's id within its family, as GitHub prints it.
        #[sea_orm(primary_key, auto_increment = false)]
        pub entity_id: String,
        /// Hex fingerprint of the list-visible fields.
        pub fingerprint: String,
        /// RFC3339 `updated_at` from the list row, if the family carries one.
        pub updated_at: Option<String>,
        /// GraphQL node id, when a GraphQL leg reported it.
        pub node_id: Option<String>,
        /// Hash over child-collection counts (comments/reviews/commits/files).
        pub child_counts_hash: Option<String>,
        /// RFC3339 completion time of the last refinement pass.
        pub last_refined_at: Option<String>,
        /// `pending` until refinement finishes, then `complete`.
        pub refinement_status: String,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

pub mod issue_events {
    use super::{DeriveEntityModel, DerivePrimaryKey, DeriveRelation, EnumIter, Scopable, Uuid};
    use sea_orm::entity::prelude::*;

    /// Mirrored issue event (audit trail of an issue), tenant-scoped.
    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
    #[sea_orm(table_name = "gm_issue_events")]
    #[secure(tenant_col = "tenant_id", resource_col = "id", no_owner, no_type)]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub tenant_id: Uuid,
        /// GitHub event id.
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: i64,
        /// Owning repository's GitHub id.
        pub repo_id: i64,
        /// Number of the issue or pull request the event belongs to.
        pub issue_number: i64,
        /// `labeled`, `assigned`, `closed`, `reopened`, `referenced`, ...
        pub event: String,
        pub actor_login: Option<String>,
        /// Label name for label events.
        pub label_name: Option<String>,
        /// Assignee login for assignment events.
        pub assignee_login: Option<String>,
        /// Milestone title for milestone events.
        pub milestone_title: Option<String>,
        /// Commit SHA for referenced/closed-by-commit events.
        pub commit_id: Option<String>,
        /// GitHub's own stamp, kept verbatim (see the module doc).
        pub created_at: String,
        /// When a sync last wrote this row. Doubles as the
        /// deletion-reconciliation watermark; `None` on rows from before
        /// the column existed.
        pub extracted_at: Option<DateTimeUtc>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

pub mod deployments {
    use super::{DeriveEntityModel, DerivePrimaryKey, DeriveRelation, EnumIter, Scopable, Uuid};
    use sea_orm::entity::prelude::*;

    /// Mirrored GitHub deployment record, tenant-scoped.
    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
    #[sea_orm(table_name = "gm_deployments")]
    #[secure(tenant_col = "tenant_id", resource_col = "id", no_owner, no_type)]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub tenant_id: Uuid,
        /// GitHub deployment id.
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: i64,
        /// Owning repository's GitHub id.
        pub repo_id: i64,
        /// Branch, tag, or SHA that was deployed.
        pub git_ref: String,
        /// SHA the deployment points at.
        pub sha: String,
        /// Target environment (`production`, `staging`, ...).
        pub environment: String,
        /// Deployment task, `deploy` by default.
        pub task: String,
        pub description: Option<String>,
        pub creator_login: Option<String>,
        /// GitHub's own stamps, kept verbatim (see the module doc).
        pub created_at: String,
        pub updated_at: String,
        /// When a sync last wrote this row. Doubles as the
        /// deletion-reconciliation watermark; `None` on rows from before
        /// the column existed.
        pub extracted_at: Option<DateTimeUtc>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

pub mod pull_request_commits {
    use super::{DeriveEntityModel, DerivePrimaryKey, DeriveRelation, EnumIter, Scopable, Uuid};
    use sea_orm::entity::prelude::*;

    /// Commit belonging to one pull request, tenant-scoped.
    ///
    /// Keyed by `(tenant_id, repo_id, pull_number, sha)`: the same commit
    /// can appear in several pull requests.
    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
    #[sea_orm(table_name = "gm_pull_request_commits")]
    #[secure(tenant_col = "tenant_id", resource_col = "sha", no_owner, no_type)]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub tenant_id: Uuid,
        /// Owning repository's GitHub id.
        #[sea_orm(primary_key, auto_increment = false)]
        pub repo_id: i64,
        /// Owning pull-request number.
        #[sea_orm(primary_key, auto_increment = false)]
        pub pull_number: i64,
        #[sea_orm(primary_key, auto_increment = false)]
        pub sha: String,
        pub message: String,
        pub author_login: Option<String>,
        pub committer_login: Option<String>,
        /// GitHub's own stamps, kept verbatim (see the module doc).
        pub authored_at: Option<String>,
        pub committed_at: Option<String>,
        /// When a sync last wrote this row. Doubles as the
        /// deletion-reconciliation watermark; `None` on rows from before
        /// the column existed.
        pub extracted_at: Option<DateTimeUtc>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

pub mod commit_statuses {
    use super::{DeriveEntityModel, DerivePrimaryKey, DeriveRelation, EnumIter, Scopable, Uuid};
    use sea_orm::entity::prelude::*;

    /// Mirrored commit status (external check on a commit), tenant-scoped.
    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
    #[sea_orm(table_name = "gm_commit_statuses")]
    #[secure(tenant_col = "tenant_id", resource_col = "id", no_owner, no_type)]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub tenant_id: Uuid,
        /// GitHub status id.
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: i64,
        /// Owning repository's GitHub id.
        pub repo_id: i64,
        /// SHA the status was reported against.
        pub commit_sha: String,
        /// `error`, `failure`, `pending`, or `success`.
        pub state: String,
        /// Reporter-defined check name, unique per commit.
        pub context: String,
        pub description: Option<String>,
        pub target_url: Option<String>,
        pub creator_login: Option<String>,
        /// GitHub's own stamps, kept verbatim (see the module doc).
        pub created_at: String,
        pub updated_at: String,
        /// When a sync last wrote this row. Doubles as the
        /// deletion-reconciliation watermark; `None` on rows from before
        /// the column existed.
        pub extracted_at: Option<DateTimeUtc>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

pub mod workflow_jobs {
    use super::{DeriveEntityModel, DerivePrimaryKey, DeriveRelation, EnumIter, Scopable, Uuid};
    use sea_orm::entity::prelude::*;

    /// Mirrored GitHub Actions workflow job, tenant-scoped.
    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
    #[sea_orm(table_name = "gm_workflow_jobs")]
    #[secure(tenant_col = "tenant_id", resource_col = "id", no_owner, no_type)]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub tenant_id: Uuid,
        /// GitHub job id.
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: i64,
        /// Owning repository's GitHub id.
        pub repo_id: i64,
        /// Id of the workflow run the job belongs to.
        pub run_id: i64,
        /// Retry attempt of the owning run.
        pub run_attempt: i64,
        pub name: String,
        /// `queued`, `in_progress`, or `completed`.
        pub status: Option<String>,
        /// `success`, `failure`, `cancelled`, ... — absent until completed.
        pub conclusion: Option<String>,
        pub head_sha: String,
        pub runner_name: Option<String>,
        /// GitHub's own stamps, kept verbatim (see the module doc).
        pub started_at: Option<String>,
        pub completed_at: Option<String>,
        pub html_url: Option<String>,
        /// Raw GitHub JSON array of the job steps: they carry no ids and are
        /// only ever read back together with their job.
        pub steps_json: Option<String>,
        /// When a sync last wrote this row. Doubles as the
        /// deletion-reconciliation watermark; `None` on rows from before
        /// the column existed.
        pub extracted_at: Option<DateTimeUtc>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}
