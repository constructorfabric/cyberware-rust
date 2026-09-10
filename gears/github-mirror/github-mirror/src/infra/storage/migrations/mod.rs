use sea_orm_migration::prelude::*;

pub mod branches_011;
pub mod check_runs_025;
pub mod comments_005;
pub mod commit_comments_018;
pub mod commit_files_016;
pub mod commit_statuses_022;
pub mod commits_004;
pub mod contributors_012;
pub mod deployments_020;
pub mod initial_001;
pub mod issue_events_019;
pub mod issue_reactions_024;
pub mod issue_timeline_026;
pub mod issues_002;
pub mod labels_008;
pub mod milestones_009;
pub mod pull_request_commits_021;
pub mod pull_request_files_014;
pub mod pull_requests_003;
pub mod pull_requests_refs_028;
pub mod releases_010;
pub mod repo_clone_url_027;
pub mod review_comments_006;
pub mod review_comments_diff_anchors_029;
pub mod review_threads_017;
pub mod reviews_007;
pub mod support;
pub mod tags_015;
pub mod workflow_jobs_023;
pub mod workflow_runs_013;
pub mod z_contributor_derivation_032;
pub mod z_extracted_at_030;
pub mod z_issue_pull_author_038;
pub mod z_issue_pull_people_037;
pub mod z_node_id_034;
pub mod z_pull_request_file_patch_036;
pub mod z_release_assets_031;
pub mod z_review_comment_anchors_035;
pub mod z_review_comment_review_id_033;

pub struct Migrator;

#[async_trait::async_trait]
impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![
            Box::new(initial_001::Migration),
            Box::new(issues_002::Migration),
            Box::new(pull_requests_003::Migration),
            Box::new(commits_004::Migration),
            Box::new(comments_005::Migration),
            Box::new(review_comments_006::Migration),
            Box::new(reviews_007::Migration),
            Box::new(labels_008::Migration),
            Box::new(milestones_009::Migration),
            Box::new(releases_010::Migration),
            Box::new(branches_011::Migration),
            Box::new(contributors_012::Migration),
            Box::new(workflow_runs_013::Migration),
            Box::new(pull_request_files_014::Migration),
            Box::new(tags_015::Migration),
            Box::new(commit_files_016::Migration),
            Box::new(review_threads_017::Migration),
            Box::new(commit_comments_018::Migration),
            Box::new(issue_events_019::Migration),
            Box::new(deployments_020::Migration),
            Box::new(pull_request_commits_021::Migration),
            Box::new(commit_statuses_022::Migration),
            Box::new(workflow_jobs_023::Migration),
            Box::new(issue_reactions_024::Migration),
            Box::new(check_runs_025::Migration),
            Box::new(issue_timeline_026::Migration),
            Box::new(repo_clone_url_027::Migration),
            Box::new(pull_requests_refs_028::Migration),
            Box::new(review_comments_diff_anchors_029::Migration),
            Box::new(z_extracted_at_030::Migration),
            Box::new(z_contributor_derivation_032::Migration),
            Box::new(z_release_assets_031::Migration),
            Box::new(z_issue_pull_author_038::Migration),
            Box::new(z_issue_pull_people_037::Migration),
            Box::new(z_node_id_034::Migration),
            Box::new(z_pull_request_file_patch_036::Migration),
            Box::new(z_review_comment_anchors_035::Migration),
            Box::new(z_review_comment_review_id_033::Migration),
        ]
    }
}
