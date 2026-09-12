//! Conditional-request cache for the GitHub REST client.
//!
//! GitHub does not charge a rate-limit unit for a request it answers with
//! `304 Not Modified`, so storing each response's `ETag` and replaying it as
//! `If-None-Match` turns a repeat sync from ~115 charged calls into ~115 free
//! ones. That is the whole point of the mirror, and this module is where it
//! happens.
//!
//! Entries are **tenant-partitioned**. The design allows public-repository
//! responses to be shared across tenants ([`PRD` §5.7], ADR-0002), but sharing
//! needs the visibility and grant machinery that does not exist yet, so every
//! entry is scoped to the tenant that fetched it. That is strictly safe — it
//! only forgoes an optimisation.

use async_trait::async_trait;
use aws_lc_rs::digest::{self, SHA256};

use crate::domain::error::DomainError;

/// A content-addressed cache key: SHA-256 over the request's observable
/// properties.
///
/// The token is **never** part of the key — two callers with different tokens
/// requesting the same URL produce the same key, and tenant partitioning
/// rather than the key is what keeps their entries apart.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CacheKey(String);

impl CacheKey {
    /// Compute the key for one request.
    #[must_use]
    pub fn compute(method: &str, url: &str, accept: &str) -> Self {
        let mut hasher = digest::Context::new(&SHA256);
        hasher.update(method.to_ascii_uppercase().as_bytes());
        hasher.update(b"\x00");
        hasher.update(url.as_bytes());
        hasher.update(b"\x00");
        hasher.update(accept.as_bytes());
        Self(hex::encode(hasher.finish().as_ref()))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for CacheKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// One cached response: the body plus the validators needed to revalidate it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedResponse {
    pub body: String,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    /// `rel="next"` from the `Link` header, when the list had more pages.
    ///
    /// Stored because GitHub may answer a revalidation with a `304` that omits
    /// the header, and a lost `next` link would silently truncate the listing.
    pub next_page: Option<String>,
}

impl CachedResponse {
    /// Whether this entry can be revalidated rather than re-fetched.
    #[must_use]
    pub fn is_revalidatable(&self) -> bool {
        self.etag.is_some() || self.last_modified.is_some()
    }
}

/// Storage behind the conditional-request cache.
///
/// Kept as a trait so the client does not depend on `SeaORM`, and so tests can
/// drive it in memory.
#[async_trait]
pub trait HttpCache: Send + Sync {
    /// The entry for `key` within `tenant`, if one exists.
    ///
    /// # Errors
    /// Storage failures. A cache miss is `Ok(None)`, not an error.
    async fn get(
        &self,
        tenant_id: uuid::Uuid,
        key: &CacheKey,
    ) -> Result<Option<CachedResponse>, DomainError>;

    /// Store or replace the entry for `key` within `tenant`.
    ///
    /// # Errors
    /// Storage failures.
    async fn put(
        &self,
        tenant_id: uuid::Uuid,
        key: &CacheKey,
        url: &str,
        entry: CachedResponse,
    ) -> Result<(), DomainError>;

    /// Drop every entry whose URL starts with `url_prefix`, returning how many
    /// went. This is `clear_cache(session, scope)` from DESIGN §4: the prefix
    /// is how an org or a single repository is named, since the key itself is
    /// an opaque hash.
    ///
    /// # Errors
    /// Storage failures.
    async fn clear(&self, tenant_id: uuid::Uuid, url_prefix: &str) -> Result<u64, DomainError>;
}

/// A cache that stores nothing, for callers that do not want one.
///
/// Every request is a full fetch, which is the behaviour the gear had before
/// conditional requests existed.
pub struct NoCache;

#[async_trait]
impl HttpCache for NoCache {
    async fn get(
        &self,
        _tenant_id: uuid::Uuid,
        _key: &CacheKey,
    ) -> Result<Option<CachedResponse>, DomainError> {
        Ok(None)
    }

    async fn put(
        &self,
        _tenant_id: uuid::Uuid,
        _key: &CacheKey,
        _url: &str,
        _entry: CachedResponse,
    ) -> Result<(), DomainError> {
        Ok(())
    }

    async fn clear(&self, _tenant_id: uuid::Uuid, _url_prefix: &str) -> Result<u64, DomainError> {
        Ok(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_key_is_stable_for_the_same_request() {
        let a = CacheKey::compute(
            "GET",
            "https://api.github.com/repos/a/b",
            "application/json",
        );
        let b = CacheKey::compute(
            "get",
            "https://api.github.com/repos/a/b",
            "application/json",
        );
        assert_eq!(a, b, "the method is normalised");
        assert_eq!(a.as_str().len(), 64, "hex-encoded SHA-256");
    }

    #[test]
    fn the_key_changes_with_the_url_and_the_accept_header() {
        let base = CacheKey::compute(
            "GET",
            "https://api.github.com/repos/a/b",
            "application/json",
        );
        assert_ne!(
            base,
            CacheKey::compute(
                "GET",
                "https://api.github.com/repos/a/c",
                "application/json"
            )
        );
        assert_ne!(
            base,
            CacheKey::compute("GET", "https://api.github.com/repos/a/b", "text/plain")
        );
    }

    #[test]
    fn an_entry_without_validators_cannot_be_revalidated() {
        let bare = CachedResponse {
            body: "{}".to_owned(),
            etag: None,
            last_modified: None,
            next_page: None,
        };
        assert!(!bare.is_revalidatable());

        let tagged = CachedResponse {
            etag: Some("W/\"abc\"".to_owned()),
            ..bare
        };
        assert!(tagged.is_revalidatable());
    }
}
