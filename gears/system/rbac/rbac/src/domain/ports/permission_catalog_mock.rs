#![allow(unknown_lints, de0309_must_have_domain_model)]

//! In-memory and accept-all test doubles for [`PermissionCatalog`].
//! Both names remain reachable via
//! `crate::domain::permission_catalog::*` through a re-export.

use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;

use super::permission_catalog::{
    AuthzPermission, CatalogCursor, CatalogListPage, PermissionCatalog, PermissionCatalogError,
    PermissionCatalogFilter, paginate_catalog_page,
};

/// In-memory fake for unit tests. The call counters witness "catalog not
/// consulted" assertions (built-in-seeder bypass, shape-validation
/// short-circuit, description-only PATCH).
pub struct InMemoryPermissionCatalog {
    entries: Vec<AuthzPermission>,
    exists_calls: AtomicUsize,
    list_calls: AtomicUsize,
}

/// Refuse to construct any of the test doubles in this module when
/// compiled with `debug_assertions = false` (release profile). The
/// module is gated `#[cfg(any(test, feature = "test-support"))]`, but
/// cargo feature unification means a workspace member depending on
/// `rbac/test-support` would re-export these constructors into a
/// release binary. The runtime guard makes any such misuse fail loud
/// on first call instead of silently exposing a permissive fake.
// Intentional fail-loud guard (see fn doc). `manual_assert` is suppressed
// too: converting to `assert!(cfg!(debug_assertions), …)` would trip
// `assertions_on_constants` since the condition is a compile-time const.
#[allow(clippy::panic, clippy::manual_assert)]
fn release_build_guard() {
    if !cfg!(debug_assertions) {
        panic!(
            "rbac::domain::ports::permission_catalog_mock test double constructed in a \
             release-profile build \u{2014} this type is a test fixture and must never reach \
             production. Use the real `TypesRegistryPermissionCatalog` (see \
             infra::types_registry_permission_catalog) instead."
        );
    }
}

impl InMemoryPermissionCatalog {
    /// Seed the fake with `(action, resource_type)` pairs. `id` and
    /// `display_name` are synthesized so callers don't have to spell
    /// them out for every test.
    pub fn with_pairs<I, S1, S2>(pairs: I) -> Self
    where
        I: IntoIterator<Item = (S1, S2)>,
        S1: Into<String>,
        S2: Into<String>,
    {
        release_build_guard();
        let mut entries: Vec<AuthzPermission> = pairs
            .into_iter()
            .map(|(action, resource_type)| {
                let action = action.into();
                let resource_type = resource_type.into();
                let id = format!(
                    "gts.cf.toolkit.authz.permission.v1~test._.{action}__{}.v1",
                    resource_type.replace([':', '/', '~', '.'], "_")
                );
                let display_name = format!("{action} on {resource_type}");
                AuthzPermission {
                    id,
                    resource_type,
                    action,
                    display_name,
                }
            })
            .collect();
        // Keep the fake sorted by id so the binary-search seek in
        // [`PermissionCatalog::list_permissions`] sees the same
        // invariant the production catalog upholds.
        entries.sort_by(|a, b| a.id.cmp(&b.id));
        Self {
            entries,
            exists_calls: AtomicUsize::new(0),
            list_calls: AtomicUsize::new(0),
        }
    }

    /// Construct an empty catalog (every membership check returns `false`).
    #[must_use]
    pub fn empty() -> Self {
        release_build_guard();
        Self {
            entries: Vec::new(),
            exists_calls: AtomicUsize::new(0),
            list_calls: AtomicUsize::new(0),
        }
    }

    /// How many times `exists` has been invoked.
    pub fn exists_call_count(&self) -> usize {
        self.exists_calls.load(Ordering::SeqCst)
    }

    /// How many times `list_permissions` has been invoked.
    pub fn list_call_count(&self) -> usize {
        self.list_calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl PermissionCatalog for InMemoryPermissionCatalog {
    async fn list_permissions(
        &self,
        filter: PermissionCatalogFilter,
        cursor: Option<CatalogCursor>,
        limit: u32,
    ) -> Result<CatalogListPage, PermissionCatalogError> {
        self.list_calls.fetch_add(1, Ordering::SeqCst);
        // Delegate to the same helper the production catalog uses, so the
        // fake and the real impl agree on forward/backward cursor
        // semantics and on rejecting unknown cursors. `entries` is sorted
        // by id in `with_pairs`, satisfying the helper's precondition.
        paginate_catalog_page(&self.entries, &filter, cursor.as_ref(), limit)
    }

    async fn exists(
        &self,
        operation: &str,
        target_type: &str,
    ) -> Result<bool, PermissionCatalogError> {
        self.exists_calls.fetch_add(1, Ordering::SeqCst);
        Ok(self
            .entries
            .iter()
            .any(|p| p.action == operation && p.resource_type == target_type))
    }
}

/// Accept-all stub for unit tests that don't care about catalog content.
/// `exists` is `true` and `list` is empty.
///
/// Carries a private field so construction must go through
/// [`Self::new`]: a `pub` unit struct is itself a constructor
/// expression, so `Arc::new(AcceptAllPermissionCatalog)` would bypass
/// both [`release_build_guard`] and any `Default` impl — and this
/// double answers `exists` with `true` for every permission.
#[derive(Debug)]
pub struct AcceptAllPermissionCatalog {
    /// Forces construction through [`Self::new`].
    _guarded: (),
}

impl AcceptAllPermissionCatalog {
    /// Construct the accept-all double. Panics in a release-profile
    /// build — see [`release_build_guard`].
    #[must_use]
    pub fn new() -> Self {
        release_build_guard();
        Self { _guarded: () }
    }
}

impl Default for AcceptAllPermissionCatalog {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl PermissionCatalog for AcceptAllPermissionCatalog {
    async fn list_permissions(
        &self,
        _filter: PermissionCatalogFilter,
        _cursor: Option<CatalogCursor>,
        _limit: u32,
    ) -> Result<CatalogListPage, PermissionCatalogError> {
        Ok(CatalogListPage {
            items: Vec::new(),
            next_cursor: None,
            prev_cursor: None,
        })
    }

    async fn exists(
        &self,
        _operation: &str,
        _target_type: &str,
    ) -> Result<bool, PermissionCatalogError> {
        Ok(true)
    }
}
