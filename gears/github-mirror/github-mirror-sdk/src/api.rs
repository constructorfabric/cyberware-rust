//! `GithubMirrorClientV1` trait definition.
//!
//! Public API of the github-mirror gear (Version 1, unstable pre-1.0).
//! All methods take a `SecurityContext` for authorization and access
//! control and return the platform's canonical error type.

use async_trait::async_trait;
use toolkit_canonical_errors::CanonicalError;
use toolkit_odata::{ODataQuery, Page};
use toolkit_security::SecurityContext;

use crate::models::{MirrorStatus, Repo, SyncSummary};

/// Public API trait for the github-mirror gear (Version 1).
///
/// Registered in `ClientHub` by the gear at init:
/// ```ignore
/// let mirror = hub.get::<dyn GithubMirrorClientV1>()?;
/// ```
///
/// The surface covers what the mirror serves today: identity, the mirrored
/// repository listing, and a sync trigger. Write-back operations arrive with
/// the increment that adds them.
#[async_trait]
pub trait GithubMirrorClientV1: Send + Sync {
    /// Report the mirror's identity: gear name, crate version, and the
    /// GitHub API base URL it is configured against.
    async fn status(&self, ctx: &SecurityContext) -> Result<MirrorStatus, CanonicalError>;

    /// List repositories from the mirrored store, one page at a time.
    async fn list_repos(
        &self,
        ctx: &SecurityContext,
        query: ODataQuery,
    ) -> Result<Page<Repo>, CanonicalError>;

    /// Fetch one repository from GitHub and upsert it into the caller's
    /// tenant mirror (the PRD's `sync_repo` entry point; first slice —
    /// repo + first page of issues, pull requests, commits).
    async fn sync_repository(
        &self,
        ctx: &SecurityContext,
        owner: &str,
        name: &str,
    ) -> Result<SyncSummary, CanonicalError>;
}
