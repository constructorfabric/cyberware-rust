//! Request-local participant names from AM's authorized `IdP` passthrough.
//!
//! No names are persisted or cached between requests. AM receives the original
//! caller and tenant, not a privileged service context. UUIDs are deduplicated
//! and read in bounded ID-set batches, never by listing an unfiltered tenant.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use account_management_sdk::{AccountManagementClient, IdpUser, IdpUserPagination, ListUsersQuery};
use async_trait::async_trait;
use futures_util::{StreamExt, stream};
use toolkit::ClientHub;
use toolkit_canonical_errors::CanonicalError;
use toolkit_odata::Page;
use toolkit_security::SecurityContext;
use uuid::Uuid;

/// Maximum simultaneous AM lookups within one approval response.
const LOOKUP_CONCURRENCY: usize = 4;
/// SDK's maximum ID-set batch. Each batch may follow provider pagination.
const LOOKUP_BATCH_SIZE: usize = IdpUserPagination::MAX_TOP as usize;
/// One budget for the entire response, including queued lookups.
const LOOKUP_BUDGET: Duration = Duration::from_secs(2);

/// A current label, or an explicit reason it cannot be presented.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParticipantName {
    /// Current nonblank `IdP` display name, full name, or username.
    Resolved(String),
    /// AM refused this caller's access; no additional permission is minted.
    Restricted,
    /// AM found no visible user; this does not prove deletion or principal kind.
    NotFound,
    /// Missing/unsupported source, source failure, deadline, or invalid profile.
    Unavailable,
}

/// Narrow adapter boundary for AM's public user-profile read.
#[async_trait]
pub trait ParticipantDirectory: Send + Sync {
    /// Read one filtered page in the caller's tenant with the caller's rights.
    ///
    /// # Errors
    /// Propagates AM authorization, absence, and provider failures unchanged.
    async fn list_users(
        &self,
        ctx: &SecurityContext,
        query: ListUsersQuery,
    ) -> Result<Page<IdpUser>, CanonicalError>;
}

/// Resolve lazily so optional AM registration order cannot disable names forever.
struct AmParticipantDirectory {
    hub: Arc<ClientHub>,
}

#[async_trait]
impl ParticipantDirectory for AmParticipantDirectory {
    async fn list_users(
        &self,
        ctx: &SecurityContext,
        query: ListUsersQuery,
    ) -> Result<Page<IdpUser>, CanonicalError> {
        let am = self
            .hub
            .try_get::<dyn AccountManagementClient>()
            .ok_or_else(|| CanonicalError::service_unavailable().create())?;
        am.list_users(ctx, ctx.subject_tenant_id(), query).await
    }
}

/// Read-side enrichment only; not part of approval transactions, pins, or audit.
#[derive(Clone)]
pub struct ApprovalParticipants {
    directory: Arc<dyn ParticipantDirectory>,
}

impl ApprovalParticipants {
    /// Bind to AM's public SDK through the process client hub.
    #[must_use]
    pub fn new(hub: Arc<ClientHub>) -> Self {
        Self::with_directory(Arc::new(AmParticipantDirectory { hub }))
    }

    /// Supply the narrow profile reader, also used by transport-level tests.
    #[must_use]
    pub fn with_directory(directory: Arc<dyn ParticipantDirectory>) -> Self {
        Self { directory }
    }

    /// Resolve unique IDs in batches, preserving names from successful pages.
    ///
    /// Caller must first authorize and load the approval records. No lookup is
    /// started for a queued batch after the shared deadline; outstanding reads are
    /// cancelled by dropping their futures. No profile/error details are logged.
    pub async fn resolve(
        &self,
        ctx: &SecurityContext,
        principals: impl IntoIterator<Item = Uuid>,
    ) -> BTreeMap<Uuid, ParticipantName> {
        let unique: Vec<_> = principals
            .into_iter()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let deadline = tokio::time::Instant::now() + LOOKUP_BUDGET;
        let batches: Vec<_> = unique
            .chunks(LOOKUP_BATCH_SIZE)
            .map(<[Uuid]>::to_vec)
            .collect();
        stream::iter(batches)
            .map(|ids| async move { self.resolve_batch(ctx, &ids, deadline).await })
            .buffer_unordered(LOOKUP_CONCURRENCY)
            .flat_map(stream::iter)
            .collect()
            .await
    }

    /// Exhaust the exact filtered set before claiming absence. Every page must
    /// make progress and return only new requested IDs; provider drift and
    /// pagination failures never disclose unrelated profiles or imply deletion.
    async fn resolve_batch(
        &self,
        ctx: &SecurityContext,
        ids: &[Uuid],
        deadline: tokio::time::Instant,
    ) -> BTreeMap<Uuid, ParticipantName> {
        let mut names = BTreeMap::new();
        let mut remaining: BTreeSet<_> = ids.iter().copied().collect();
        let mut seen_cursors = BTreeSet::new();
        let mut cursor = None;
        let failure = loop {
            if tokio::time::Instant::now() >= deadline {
                break ParticipantName::Unavailable;
            }
            let Ok(mut query) = ListUsersQuery::with_ids(ids.iter().copied()) else {
                break ParticipantName::Unavailable;
            };
            let Ok(pagination) = IdpUserPagination::new(query.pagination.top(), cursor) else {
                break ParticipantName::Unavailable;
            };
            query.pagination = pagination;
            let result =
                tokio::time::timeout_at(deadline, self.directory.list_users(ctx, query)).await;
            let page = match result {
                Ok(Ok(page)) => page,
                Ok(Err(
                    CanonicalError::PermissionDenied { .. }
                    | CanonicalError::Unauthenticated { .. },
                )) => break ParticipantName::Restricted,
                Ok(Err(CanonicalError::NotFound { .. })) => break ParticipantName::NotFound,
                _ => break ParticipantName::Unavailable,
            };
            let returned: BTreeSet<_> = page.items.iter().map(|user| user.id).collect();
            if returned.len() != page.items.len() || !returned.is_subset(&remaining) {
                break ParticipantName::Unavailable;
            }
            for user in page.items {
                remaining.remove(&user.id);
                names.insert(user.id, project_name(&user));
            }
            if remaining.is_empty() || page.page_info.next_cursor.is_none() {
                break ParticipantName::NotFound;
            }
            let Some(next) = page.page_info.next_cursor else {
                break ParticipantName::Unavailable;
            };
            if returned.is_empty() || !seen_cursors.insert(next.clone()) {
                break ParticipantName::Unavailable;
            }
            cursor = Some(next);
        };
        names.extend(remaining.into_iter().map(|id| (id, failure.clone())));
        names
    }
}

/// Provider fields only, with whitespace-only values treated as missing.
fn project_name(user: &IdpUser) -> ParticipantName {
    if let Some(name) = user
        .display_name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        return ParticipantName::Resolved(name.to_owned());
    }
    let full_name = [user.first_name.as_deref(), user.last_name.as_deref()]
        .into_iter()
        .flatten()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if !full_name.is_empty() {
        return ParticipantName::Resolved(full_name);
    }
    let username = user.username.trim();
    if username.is_empty() {
        ParticipantName::Unavailable
    } else {
        ParticipantName::Resolved(username.to_owned())
    }
}

#[cfg(test)]
#[path = "approval_participants_tests.rs"]
mod tests;
