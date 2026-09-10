#![allow(clippy::unwrap_used, clippy::expect_used)]

use github_mirror::infra::storage::migrations::Migrator;
use sea_orm_migration::sea_orm::Database;
use sea_orm_migration::{MigratorTrait, SchemaManager};

#[tokio::test]
async fn migrations_apply_and_roll_back_on_a_clean_database() {
    let conn = Database::connect("sqlite::memory:")
        .await
        .expect("in-memory database must connect");

    Migrator::up(&conn, None).await.expect("up must succeed");

    let manager = SchemaManager::new(&conn);
    for table in [
        "gm_repositories",
        "gm_issues",
        "gm_pull_requests",
        "gm_commits",
        "gm_comments",
        "gm_review_comments",
        "gm_reviews",
        "gm_labels",
        "gm_milestones",
        "gm_releases",
        "gm_branches",
        "gm_contributors",
        "gm_workflow_runs",
        "gm_pull_request_files",
        "gm_tags",
        "gm_commit_files",
        "gm_review_threads",
        "gm_commit_comments",
        "gm_issue_events",
        "gm_deployments",
        "gm_pull_request_commits",
        "gm_commit_statuses",
        "gm_workflow_jobs",
        "gm_issue_reactions",
        "gm_check_runs",
        "gm_issue_timeline",
    ] {
        assert!(
            manager.has_table(table).await.unwrap(),
            "{table} must exist after up()"
        );
    }

    // The two additive-column migrations that predate this test, plus the
    // one added alongside review-comment diff anchoring: `up`/`down` for
    // these never touched a whole table, so the table-existence loop above
    // cannot tell a working migration from a no-op one — only checking the
    // column itself can.
    for (table, column) in [
        ("gm_repositories", "clone_url"),
        ("gm_pull_requests", "html_url"),
        ("gm_pull_requests", "head_ref"),
        ("gm_pull_requests", "base_ref"),
        ("gm_review_comments", "position"),
        ("gm_review_comments", "original_position"),
    ] {
        assert!(
            manager.has_column(table, column).await.unwrap(),
            "{table}.{column} must exist after up()"
        );
    }

    // The 26-table extracted_at migration is column-additive: only checking
    // the column itself proves its up() did anything.
    for table in ["gm_issues", "gm_pull_requests", "gm_commits", "gm_labels"] {
        assert!(
            manager.has_column(table, "extracted_at").await.unwrap(),
            "{table}.extracted_at must exist after up()"
        );
    }

    assert!(
        manager
            .has_column("gm_releases", "assets_json")
            .await
            .unwrap(),
        "gm_releases.assets_json must exist after up()"
    );

    for (table, column) in [
        ("gm_review_comments", "pull_request_review_id"),
        ("gm_review_comments", "line"),
        ("gm_review_comments", "side"),
        ("gm_review_comments", "subject_type"),
        ("gm_pull_request_files", "patch"),
        ("gm_repositories", "node_id"),
        ("gm_issues", "node_id"),
        ("gm_pull_requests", "node_id"),
        ("gm_issues", "author_login"),
        ("gm_issues", "author_json"),
        ("gm_pull_requests", "author_json"),
        ("gm_issues", "labels_json"),
        ("gm_pull_requests", "requested_reviewers_json"),
    ] {
        assert!(
            manager.has_column(table, column).await.unwrap(),
            "{table}.{column} must exist after up()"
        );
    }

    for column in ["roles", "first_seen_at", "last_seen_at"] {
        assert!(
            manager.has_column("gm_contributors", column).await.unwrap(),
            "gm_contributors.{column} must exist after up()"
        );
    }

    for migration in Migrator::migrations().iter().rev() {
        migration.down(&manager).await.expect("down must succeed");
    }

    for table in [
        "gm_repositories",
        "gm_issues",
        "gm_pull_requests",
        "gm_commits",
        "gm_comments",
        "gm_review_comments",
        "gm_reviews",
        "gm_labels",
        "gm_milestones",
        "gm_releases",
        "gm_branches",
        "gm_contributors",
        "gm_workflow_runs",
        "gm_pull_request_files",
        "gm_tags",
        "gm_commit_files",
        "gm_review_threads",
        "gm_commit_comments",
        "gm_issue_events",
        "gm_deployments",
        "gm_pull_request_commits",
        "gm_commit_statuses",
        "gm_workflow_jobs",
        "gm_issue_reactions",
        "gm_check_runs",
        "gm_issue_timeline",
    ] {
        assert!(
            !manager.has_table(table).await.unwrap(),
            "{table} must be gone after down()"
        );
    }
    // The additive migrations roll back to no table at all (down() for the
    // 26 CREATE TABLE migrations already dropped these tables by this point),
    // so there is nothing further to assert for the individual columns.
}

/// The nine `z_`-prefixed migrations only add columns, and the runner applies
/// migrations in name order, so they are the last nine to run and the first
/// nine to roll back. Undoing exactly those leaves every table in place, which
/// is what makes their `down()` bodies observable: replace one with `Ok(())`
/// and its column survives here.
#[tokio::test]
async fn the_additive_migrations_drop_their_columns_on_rollback() {
    let conn = Database::connect("sqlite::memory:")
        .await
        .expect("in-memory database must connect");

    Migrator::up(&conn, None).await.expect("up must succeed");
    Migrator::down(&conn, Some(9))
        .await
        .expect("rolling back the additive migrations must succeed");

    let manager = SchemaManager::new(&conn);
    for (table, column) in [
        ("gm_repositories", "extracted_at"),
        ("gm_issues", "extracted_at"),
        ("gm_contributors", "roles"),
        ("gm_contributors", "first_seen_at"),
        ("gm_contributors", "last_seen_at"),
        ("gm_releases", "assets_json"),
        ("gm_review_comments", "pull_request_review_id"),
        ("gm_review_comments", "line"),
        ("gm_review_comments", "subject_type"),
        ("gm_repositories", "node_id"),
        ("gm_issues", "node_id"),
        ("gm_pull_request_files", "patch"),
        ("gm_issues", "author_login"),
        ("gm_issues", "labels_json"),
        ("gm_issues", "author_json"),
        ("gm_pull_requests", "requested_reviewers_json"),
    ] {
        assert!(
            manager.has_table(table).await.unwrap(),
            "{table} must still exist: only the additive migrations were rolled back"
        );
        assert!(
            !manager.has_column(table, column).await.unwrap(),
            "{table}.{column} must be gone after its migration's down()"
        );
    }
}
