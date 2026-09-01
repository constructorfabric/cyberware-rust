//! `PermissionCatalog` — read-through proxy over the platform's
//! `AuthzPermissionV1` inventory. Production wraps
//! `Arc<dyn TypesRegistryClient>`; test doubles live in
//! `permission_catalog_mock`.

use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use thiserror::Error;
use toolkit_macros::domain_model;

/// Owned-strings projection of an `AuthzPermissionV1` GTS instance,
/// suitable for the REST DTO and the in-memory fake.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthzPermission {
    /// Full GTS instance id, e.g.
    /// `gts.cf.toolkit.authz.permission.v1~cf.core.rbac.role_definition_read.v1`.
    pub id: String,
    /// GTS resource-type expression this permission applies to. May be a
    /// concrete type id or a wildcard pattern; the catalog stores the
    /// string verbatim and does not interpret it.
    pub resource_type: String,
    /// Concrete action name (lowercase `snake_case`).
    pub action: String,
    /// Human-readable label for admin UIs.
    pub display_name: String,
}

/// Optional filters for [`PermissionCatalog::list_permissions`].
///
/// Both fields default to `None` (no filtering). When both are present
/// they combine with logical AND.
#[domain_model]
#[derive(Debug, Clone, Default)]
pub struct PermissionCatalogFilter {
    /// Exact-string filter on `action`.
    pub action: Option<String>,
    /// Left-anchored substring filter on `resource_type`.
    pub resource_type_prefix: Option<String>,
}

/// Failure surface for the catalog.
#[domain_model]
#[derive(Debug, thiserror::Error)]
pub enum PermissionCatalogError {
    /// Upstream `TypesRegistryClient` failed. REST maps to 503.
    #[error("types-registry lookup failed: {0}")]
    Registry(String),
    /// A registered GTS instance failed to deserialize.
    #[error("permission instance failed to deserialize: id={id}, error={cause}")]
    Deserialize { id: String, cause: String },
    /// The pagination cursor does not reference a known catalog
    /// position: the decoded `last_id` is absent from the catalog.
    /// Catches garbage cursors that happen to base64-decode to valid
    /// UTF-8 and foreign (schema-mismatched) cursors minted by other
    /// endpoints. REST maps to 400 `invalid_cursor`.
    #[error("cursor does not reference a known catalog entry")]
    InvalidCursor,
}

impl From<PermissionCatalogError> for crate::domain::error::DomainError {
    fn from(err: PermissionCatalogError) -> Self {
        match err {
            // The `DependencyUnavailable` variant has no `cause` slot,
            // so the upstream `Registry(msg)` body would otherwise be
            // dropped on conversion. Log it before discarding so
            // operators still have the diagnostic for audit
            // correlation (same philosophy as the bonus-1 fix in
            // `error_mapping.rs::AuthorizationDenied`).
            PermissionCatalogError::Registry(msg) => {
                tracing::warn!(
                    target: "rbac.permission_catalog",
                    cause = %msg,
                    "TypesRegistry lookup failed; surfacing as DependencyUnavailable, \
                     upstream message not forwarded to the wire"
                );
                Self::DependencyUnavailable {
                    dependency: "TypesRegistryClient",
                }
            }
            PermissionCatalogError::Deserialize { id, cause } => Self::internal(format!(
                "permission catalog: failed to deserialize instance '{id}': {cause}"
            )),
            // Not reachable on the DomainError path today — only
            // `list_permissions` (the REST handler, via
            // `catalog_error_to_rbac`) produces `InvalidCursor`. Mapped
            // here for exhaustiveness; a 400 `Validation` is the closest
            // domain surface.
            PermissionCatalogError::InvalidCursor => Self::Validation {
                detail: "cursor does not reference a known catalog entry".to_owned(),
            },
        }
    }
}

/// Which side of a [`CatalogCursor`]'s anchor the next page lies on.
/// A forward cursor pages past the anchor (`id > anchor`); a backward
/// cursor pages before it (`id < anchor`). The direction is baked into
/// the opaque wire form so a cursor round-trips its own direction.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CatalogCursorDirection {
    /// Next page: entries whose `id` is strictly greater than the anchor.
    Forward,
    /// Previous page: entries whose `id` is strictly less than the anchor.
    Backward,
}

/// Opaque bidirectional pagination cursor for the permission catalog.
/// Anchors on a catalog entry `id` plus a [`CatalogCursorDirection`].
/// The wire form is `base64url("f:<id>")` (forward) or `base64url("b:<id>")`
/// (backward) — unsigned, matching the cursor style every other module
/// uses (RMS, account-management, resource-group). Tenant/visibility
/// scoping is enforced by the query each request, so the cursor only
/// positions within an already-fenced result set.
///
/// The permission catalog is not backed by a `SeaORM` table (it reads
/// from the GTS registry and paginates the full id-sorted set in
/// memory), so `toolkit_odata::paginate_odata` does not apply; we keep a
/// hand-rolled id cursor here. Because the whole set is materialised in
/// memory, paging backward is symmetric to paging forward.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogCursor {
    /// The boundary entry `id` this cursor positions against.
    pub id: String,
    /// Which side of `id` the requested page lies on.
    pub direction: CatalogCursorDirection,
}

impl CatalogCursor {
    /// Forward cursor anchored on `id` (next page = entries after `id`).
    #[must_use]
    pub fn forward(id: String) -> Self {
        Self {
            id,
            direction: CatalogCursorDirection::Forward,
        }
    }

    /// Backward cursor anchored on `id` (previous page = entries before `id`).
    #[must_use]
    pub fn backward(id: String) -> Self {
        Self {
            id,
            direction: CatalogCursorDirection::Backward,
        }
    }

    /// Encode `self` to the wire form: `base64url("<tag>:<id>")` where
    /// `<tag>` is `f` (forward) or `b` (backward).
    #[must_use]
    pub fn encode(&self) -> String {
        let tag = match self.direction {
            CatalogCursorDirection::Forward => 'f',
            CatalogCursorDirection::Backward => 'b',
        };
        URL_SAFE_NO_PAD.encode(format!("{tag}:{}", self.id).as_bytes())
    }

    /// Decode the wire form. Invalid base64, non-UTF-8 bytes, a missing
    /// `<tag>:` prefix, or an unknown direction tag all surface as
    /// [`CatalogCursorDecodeError::InvalidEncoding`].
    pub fn decode(s: &str) -> Result<Self, CatalogCursorDecodeError> {
        let bytes = URL_SAFE_NO_PAD
            .decode(s.as_bytes())
            .map_err(|_| CatalogCursorDecodeError::InvalidEncoding)?;
        let raw =
            String::from_utf8(bytes).map_err(|_| CatalogCursorDecodeError::InvalidEncoding)?;
        let (tag, id) = raw
            .split_once(':')
            .ok_or(CatalogCursorDecodeError::InvalidEncoding)?;
        let direction = match tag {
            "f" => CatalogCursorDirection::Forward,
            "b" => CatalogCursorDirection::Backward,
            _ => return Err(CatalogCursorDecodeError::InvalidEncoding),
        };
        Ok(Self {
            id: id.to_owned(),
            direction,
        })
    }
}

/// Failure surface for [`CatalogCursor::decode`]. The REST layer maps
/// it to a 400 `invalid_cursor`.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CatalogCursorDecodeError {
    #[error("cursor encoding is malformed")]
    InvalidEncoding,
}

/// A page of catalog entries — the catalog's analogue of the
/// `toolkit_odata::Page` the role-definition repo returns. Defined locally so
/// the catalog can carry its own id-only [`CatalogCursor`] rather than
/// the `(created_at, id)` cursor the role-list pages use.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogListPage {
    pub items: Vec<AuthzPermission>,
    /// Cursor for the next page (matching entries after the last item),
    /// or `None` when nothing matches past the page — e.g. the last page.
    pub next_cursor: Option<CatalogCursor>,
    /// Cursor for the previous page (matching entries before the first
    /// item), or `None` when nothing matches before — e.g. the first page.
    pub prev_cursor: Option<CatalogCursor>,
}

/// Paginate an id-sorted, fully-materialised catalog slice in either
/// direction. Shared by the production catalog
/// ([`crate::infra::types_registry_permission_catalog`]) and the
/// in-memory fake so both agree on cursor semantics.
///
/// Preconditions: `sorted` is sorted by `id` ascending (the production
/// catalog sorts in `fetch_from_registry`; the fake sorts in
/// `with_pairs`). A cursor whose `id` is absent from `sorted` is
/// rejected as [`PermissionCatalogError::InvalidCursor`] — it is either
/// garbage that base64-decoded to valid UTF-8, or a foreign cursor
/// minted by another endpoint.
pub(crate) fn paginate_catalog_page(
    sorted: &[AuthzPermission],
    filter: &PermissionCatalogFilter,
    cursor: Option<&CatalogCursor>,
    limit: u32,
) -> Result<CatalogListPage, PermissionCatalogError> {
    // A zero limit can never fill a page, so the collection below would
    // otherwise drain the whole catalog. Return an empty page instead.
    if limit == 0 {
        return Ok(CatalogListPage {
            items: Vec::new(),
            next_cursor: None,
            prev_cursor: None,
        });
    }

    let matches = |p: &AuthzPermission| -> bool {
        filter.action.as_ref().is_none_or(|a| &p.action == a)
            && filter
                .resource_type_prefix
                .as_ref()
                .is_none_or(|prefix| p.resource_type.starts_with(prefix.as_str()))
    };
    let limit_us = limit as usize;

    // Resolve the page's items as an ascending slice of clones.
    let items: Vec<AuthzPermission> = match cursor {
        // First page: the first `limit` matches from the start.
        None => sorted
            .iter()
            .filter(|p| matches(p))
            .take(limit_us)
            .cloned()
            .collect(),
        Some(c) => {
            let idx = sorted
                .binary_search_by(|p| p.id.as_str().cmp(c.id.as_str()))
                .map_err(|_| PermissionCatalogError::InvalidCursor)?;
            match c.direction {
                // Next page: matches with id strictly greater than the anchor.
                CatalogCursorDirection::Forward => sorted[idx + 1..]
                    .iter()
                    .filter(|p| matches(p))
                    .take(limit_us)
                    .cloned()
                    .collect(),
                // Previous page: the `limit` matches closest to (and
                // below) the anchor, returned in ascending order.
                CatalogCursorDirection::Backward => {
                    let mut back: Vec<AuthzPermission> = sorted[..idx]
                        .iter()
                        .rev()
                        .filter(|p| matches(p))
                        .take(limit_us)
                        .cloned()
                        .collect();
                    back.reverse();
                    back
                }
            }
        }
    };

    // Uniform neighbour probe: emit a cursor for a side iff a matching
    // entry actually exists there. This holds however the page was
    // reached (first / forward / backward), so next/prev stay consistent.
    let (prev_cursor, next_cursor) = match (items.first(), items.last()) {
        (Some(first), Some(last)) => {
            // Both ids came out of `sorted`, so these searches always hit.
            let first_idx = sorted
                .binary_search_by(|p| p.id.as_str().cmp(first.id.as_str()))
                .unwrap_or(0);
            let last_idx = sorted
                .binary_search_by(|p| p.id.as_str().cmp(last.id.as_str()))
                .unwrap_or(0);
            let has_before = sorted[..first_idx].iter().any(&matches);
            let has_after = sorted[last_idx + 1..].iter().any(matches);
            (
                has_before.then(|| CatalogCursor::backward(first.id.clone())),
                has_after.then(|| CatalogCursor::forward(last.id.clone())),
            )
        }
        // Empty page (filtered everything out, or paged past the end):
        // no anchors to position against.
        _ => (None, None),
    };

    Ok(CatalogListPage {
        items,
        next_cursor,
        prev_cursor,
    })
}

/// Read-through proxy over the platform's permission inventory.
#[async_trait]
pub trait PermissionCatalog: Send + Sync {
    /// Return a page of catalog entries matching `filter`, sorted by
    /// `id` ascending. Both filter fields default to no-op when `None`.
    /// `cursor` advances past the previous page's last id (strictly
    /// greater); `next_cursor` in the returned page is `Some` iff more
    /// matching entries exist after the returned items.
    async fn list_permissions(
        &self,
        filter: PermissionCatalogFilter,
        cursor: Option<CatalogCursor>,
        limit: u32,
    ) -> Result<CatalogListPage, PermissionCatalogError>;

    /// Return `true` iff some catalog entry has
    /// `entry.action == operation AND entry.resource_type == target_type`.
    async fn exists(
        &self,
        operation: &str,
        target_type: &str,
    ) -> Result<bool, PermissionCatalogError>;
}

// Test doubles live in [`permission_catalog_mock`]; re-exported here so
// existing import paths stay stable. Gated behind the `test-support`
// feature so the test doubles never appear in a release artifact.
#[cfg(any(test, feature = "test-support"))]
pub use crate::domain::permission_catalog_mock::{
    AcceptAllPermissionCatalog, InMemoryPermissionCatalog,
};
