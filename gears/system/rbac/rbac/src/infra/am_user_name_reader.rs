//! [`crate::domain::principal_name_reader::PrincipalNameReader`] over the
//! Account Management SDK.
//!
//! Three properties drive this implementation, and none of them is
//! obvious from the code alone.
//!
//! 1. **Lazy client resolution.** RBAC cannot declare `account_management`
//!    in its gear `deps`: AM declares `deps = [authz_resolver, …]` and the
//!    authz resolver consumes RBAC, so the edge would close a dependency
//!    cycle and neither gear would start. The client is therefore looked
//!    up in `ClientHub` on *first use* and memoized behind a lock. A
//!    lookup that finds nothing is retried on the next read rather than
//!    memoized as absent, and yields no names: a deployment without
//!    account management must still serve role assignments.
//!
//! 2. **A pass, not point lookups.** AM stores no users; tenant
//!    membership *is* membership in the tenant's Keycloak group, and the
//!    `IdP` plugin serves *any* user listing — including the single-id
//!    point lookup — by draining that whole group membership and
//!    filtering in memory. The unit of cost is therefore the call, not
//!    the user: paging once over the membership (200 per page, AM's hard
//!    ceiling) names everyone it sees, while per-id lookups pay one full
//!    drain per principal. Point lookups survive only as the fallback for
//!    ids a *budget-truncated* pass did not cover.
//!
//! 3. **Positive and negative caching.** Without negative entries a
//!    deleted principal — exactly the case a role-assignment grid is
//!    likely to contain — costs a full drain on every single render.
//!
//! Everything here degrades rather than fails. A name is display data;
//! it must never change the status code, the row set, or the cursor of a
//! role-assignment read.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use account_management_sdk::idp_user::{IdpUser, IdpUserPagination};
use account_management_sdk::{AccountManagementClient, AccountManagementError, ListUsersQuery};
use async_trait::async_trait;
use parking_lot::RwLock;
use toolkit::client_hub::ClientHub;
use toolkit_canonical_errors::CanonicalError;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::config::PrincipalNamesConfig;
use crate::domain::ports::principal_name_reader::{
    PrincipalNameError, PrincipalNameReader, non_blank,
};

/// AM's hard page ceiling for `list_users` (`IdpUserPagination::MAX_TOP`).
/// Requesting more is a validation error, so the pass asks for exactly
/// this many and lets the page budget bound the number of calls.
const AM_PAGE_TOP: u32 = 200;

/// Result of a cache probe. `Miss` is a *cached* absence (the upstream
/// already said this id has no name here) and must not trigger another
/// lookup; `Absent` is "nothing cached, or expired" and must.
enum CacheLookup {
    /// A cached name.
    Hit(String),
    /// A cached absence.
    Miss,
    /// Not cached, or the entry aged past the TTL.
    Absent,
}

/// One cache entry. `name: None` is a cached *miss* — the entry whose
/// absence would cost a membership drain per render.
#[derive(Clone)]
struct CachedName {
    /// Insertion instant; compared against the configured TTL on read.
    at: Instant,
    /// Resolved display name, or `None` for a cached miss.
    name: Option<String>,
}

/// [`PrincipalNameReader`] backed by account management, with a TTL cache
/// and a lazily resolved client.
pub struct AmUserNameReader {
    /// Kept so the AM client can be resolved after gear init — see the
    /// module docs on why this cannot be a `deps` edge.
    hub: Arc<ClientHub>,
    /// Memoized client. `None` means "not resolved yet, or not
    /// registered" — the two are deliberately not distinguished, so a
    /// client that appears later is still picked up.
    client: RwLock<Option<Arc<dyn AccountManagementClient>>>,
    /// `(tenant, canonical principal id) -> name or cached miss`.
    cache: RwLock<HashMap<(Uuid, String), CachedName>>,
    cfg: PrincipalNamesConfig,
}

impl AmUserNameReader {
    /// Build a reader. Performs no I/O and resolves no client — both
    /// happen on the first read that actually needs a name.
    #[must_use]
    pub(crate) fn new(hub: Arc<ClientHub>, cfg: PrincipalNamesConfig) -> Self {
        Self {
            hub,
            client: RwLock::new(None),
            cache: RwLock::new(HashMap::new()),
            cfg,
        }
    }

    /// Resolve (and memoize) the AM client. `None` means "not registered
    /// in `ClientHub`", which is a supported deployment shape, not an
    /// error.
    fn client(&self) -> Option<Arc<dyn AccountManagementClient>> {
        // Scoped so the read guard is released before the write below —
        // `parking_lot::RwLock` is not reentrant.
        {
            if let Some(client) = self.client.read().as_ref() {
                return Some(Arc::clone(client));
            }
        }
        match self.hub.get::<dyn AccountManagementClient>() {
            Ok(c) => {
                *self.client.write() = Some(Arc::clone(&c));
                Some(c)
            }
            Err(err) => {
                tracing::debug!(
                    target: "rbac.principal_names",
                    error = ?err,
                    "AccountManagementClient is not registered; \
                     role-assignment rows will carry ids without user names"
                );
                None
            }
        }
    }

    /// Canonical cache / match key for a principal id.
    ///
    /// A user principal id is an opaque string in RBAC's schema but a
    /// `Uuid` in AM's, and the two sides can disagree on spelling
    /// (uppercase hex, or a braced form) for the very same identity.
    /// Canonicalising both sides through `Uuid` turns that into a match
    /// instead of a silent, permanent "no name". A non-UUID id is compared
    /// as-is.
    fn key_for(id: &str) -> String {
        match Uuid::parse_str(id.trim()) {
            Ok(u) => u.to_string(),
            Err(_) => id.trim().to_owned(),
        }
    }

    /// Display name for an AM user: `display_name`, else `first last`,
    /// else `username`. Whitespace-only values count as absent — the
    /// shared [`non_blank`] rule — so a directory attribute holding a
    /// stray space falls through to the next candidate instead of
    /// becoming the name, and a user with nothing renderable resolves to
    /// no name rather than to `""`.
    fn display_name(u: &IdpUser) -> Option<String> {
        let full_name = match (u.first_name.as_deref(), u.last_name.as_deref()) {
            (Some(f), Some(l)) => Some(format!("{f} {l}")),
            (Some(one), None) | (None, Some(one)) => Some(one.to_owned()),
            (None, None) => None,
        };
        [u.display_name.clone(), full_name, Some(u.username.clone())]
            .into_iter()
            .flatten()
            .find_map(non_blank)
    }

    /// Cache read. The three outcomes are genuinely distinct — a cached
    /// miss must suppress the upstream call, an absent entry must trigger
    /// it — so they are named rather than encoded as nested options.
    fn cached(&self, tenant: Uuid, key: &str) -> CacheLookup {
        let guard = self.cache.read();
        let Some(entry) = guard.get(&(tenant, key.to_owned())) else {
            return CacheLookup::Absent;
        };
        if entry.at.elapsed() >= self.cfg.cache_ttl() {
            return CacheLookup::Absent;
        }
        match entry.name.clone() {
            Some(name) => CacheLookup::Hit(name),
            None => CacheLookup::Miss,
        }
    }

    /// Cache write, positive or negative.
    fn remember(&self, tenant: Uuid, key: String, name: Option<String>) {
        let mut guard = self.cache.write();
        // `cache_capacity()` rather than the raw field: a zero bound
        // would clear the cache on every insert, which is the one value
        // that makes this method actively harmful.
        if guard.len() >= self.cfg.cache_capacity() {
            // Coarse but bounded: a full cache is dropped wholesale
            // rather than evicted entry by entry. Simpler than an LRU,
            // and the TTL already makes a cold cache self-healing.
            guard.clear();
        }
        guard.insert(
            (tenant, key),
            CachedName {
                at: Instant::now(),
                name,
            },
        );
    }

    /// Project an AM error onto the port's error surface. `Denied` is
    /// kept distinct only so metrics and logs can tell "this caller may
    /// not read users here" (an expected, permanent condition for some
    /// personas) from an upstream outage.
    fn project_error(err: CanonicalError) -> PrincipalNameError {
        match AccountManagementError::from(err) {
            AccountManagementError::PermissionDenied { .. } => PrincipalNameError::Denied,
            other => PrincipalNameError::Unavailable {
                detail: other.to_string(),
            },
        }
    }
}

#[async_trait]
impl PrincipalNameReader for AmUserNameReader {
    async fn user_names(
        &self,
        ctx: &SecurityContext,
        tenant_id: Uuid,
        ids: &[String],
    ) -> Result<HashMap<String, String>, PrincipalNameError> {
        // Output is keyed by the id string the *caller* passed, so the
        // hydrator can look rows up by `principal_id` verbatim; matching
        // happens on the canonical form.
        let mut out: HashMap<String, String> = HashMap::new();
        let mut pending: HashMap<String, String> = HashMap::new();
        for id in ids {
            let key = Self::key_for(id);
            match self.cached(tenant_id, &key) {
                CacheLookup::Hit(name) => {
                    out.insert(id.clone(), name);
                }
                // Cached miss: nothing to add, and nothing to fetch.
                CacheLookup::Miss => {}
                CacheLookup::Absent => {
                    pending.insert(key, id.clone());
                }
            }
        }
        if pending.is_empty() {
            return Ok(out);
        }

        let Some(client) = self.client() else {
            return Ok(out);
        };

        // ---- One bounded pass over the tenant's membership. ----
        let mut cursor: Option<String> = None;
        let mut pages = 0_u32;
        let mut drained_fully = false;
        // A pass failure is captured rather than propagated immediately:
        // names the cache already answered must survive it (see below).
        let mut pass_error: Option<PrincipalNameError> = None;
        // `pages_per_tenant()` rather than the raw field: a zero page
        // budget would skip the pass entirely and hand every id to the
        // point-lookup fallback — one full membership drain each, the
        // N+1 the pass exists to prevent.
        while pages < self.cfg.pages_per_tenant() {
            let pagination = match IdpUserPagination::new(AM_PAGE_TOP, cursor.take()) {
                Ok(pagination) => pagination,
                Err(err) => {
                    pass_error = Some(PrincipalNameError::Unavailable {
                        detail: format!("list_users pagination rejected: {err}"),
                    });
                    break;
                }
            };
            let page = match client
                .list_users(ctx, tenant_id, ListUsersQuery::new(pagination))
                .await
            {
                Ok(page) => page,
                Err(err) => {
                    pass_error = Some(Self::project_error(err));
                    break;
                }
            };
            for user in &page.items {
                let key = Self::key_for(&user.id.to_string());
                let name = Self::display_name(user);
                // Cache every name the pass saw, not only the ids this
                // page asked about: that is what makes consecutive pages
                // of the same tenant free.
                self.remember(tenant_id, key.clone(), name.clone());
                if let (Some(original), Some(name)) = (pending.get(&key), name) {
                    out.insert(original.clone(), name);
                }
            }
            pages += 1;
            if let Some(next) = page.page_info.next_cursor {
                cursor = Some(next);
            } else {
                drained_fully = true;
                break;
            }
        }

        // A failed pass must not discard what the cache already answered:
        // a partially named page is strictly better than an unnamed one,
        // and the point-lookup fallback would fail the same way the pass
        // just did (a denied caller stays denied, an outage stays an
        // outage), so it is skipped. The error surfaces only when nothing
        // at all could be produced, which is what lets the caller log the
        // cause once per tenant instead of once per row.
        if let Some(err) = pass_error {
            if out.is_empty() {
                return Err(err);
            }
            tracing::debug!(
                target: "rbac.principal_names",
                tenant_id = %tenant_id,
                error = %err,
                resolved = out.len(),
                "membership pass failed after the cache answered some ids; \
                 serving the partial result"
            );
            return Ok(out);
        }

        // ---- Ids the pass did not answer. ----
        let missing: Vec<(String, String)> = pending
            .into_iter()
            .filter(|(_, original)| !out.contains_key(original))
            .collect();
        if drained_fully {
            // The tenant was fully enumerated, so an id the pass never
            // saw genuinely has no name here. Cache the miss.
            for (key, _) in missing {
                self.remember(tenant_id, key, None);
            }
            return Ok(out);
        }

        // Budget-truncated pass: fall back to point lookups for the ids
        // the caller actually asked about. Each one costs another full
        // membership drain upstream, so the count is capped — beyond the
        // cap the remaining rows keep their ids, which is the documented
        // degradation, not a failure.
        let budget = self.cfg.max_point_lookups_per_tenant as usize;
        if missing.len() > budget {
            tracing::debug!(
                target: "rbac.principal_names",
                tenant_id = %tenant_id,
                wanted = missing.len(),
                budget,
                "membership pass hit its page budget and more ids remain than the \
                 point-lookup budget allows; the remainder stay unnamed"
            );
        }
        for (key, original) in missing.into_iter().take(budget) {
            let Ok(uuid) = Uuid::parse_str(&key) else {
                // Not UUID-shaped, so AM cannot be asked at all. Cache
                // the miss so the next render skips it.
                self.remember(tenant_id, key, None);
                continue;
            };
            match client.get_user(ctx, tenant_id, uuid).await {
                Ok(user) => {
                    let name = Self::display_name(&user);
                    self.remember(tenant_id, key, name.clone());
                    if let Some(name) = name {
                        out.insert(original, name);
                    }
                }
                Err(err) => {
                    // A not-found here is authoritative; any other error
                    // is transient. Caching the miss either way is the
                    // wrong call for a transient failure, so only the
                    // absence is remembered — an outage must not pin
                    // "no name" for a whole TTL.
                    match AccountManagementError::from(err) {
                        AccountManagementError::NotFound { .. } => {
                            self.remember(tenant_id, key, None);
                        }
                        other => {
                            tracing::debug!(
                                target: "rbac.principal_names",
                                tenant_id = %tenant_id,
                                error = %other,
                                "point lookup for a role-assignment principal failed; \
                                 the row keeps its id"
                            );
                        }
                    }
                }
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
#[path = "am_user_name_reader_tests.rs"]
mod am_user_name_reader_tests;
