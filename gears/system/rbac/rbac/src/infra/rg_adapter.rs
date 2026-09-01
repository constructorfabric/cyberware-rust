//! Adapter from the narrow upstream `dyn ResourceGroupReadHierarchy`
//! read contract onto the narrow internal
//! [`crate::domain::rg_port::RbacRgRead`] port.
//!
//! `ResourceGroupReadHierarchy` resolves its reads **unscoped** — it
//! bypasses the RG `PolicyEnforcer`. That is what lets RBAC read a
//! subject's group memberships while *being* the PDP without re-entering
//! it (the RBAC→RG→PDP recursion that the PEP-gated `ResourceGroupClient`
//! caused).
//!
//! The adapter also owns the projection translation: upstream
//! `ResourceGroup` / `ResourceGroupMembership` / `ResourceGroupError`
//! values land here, then are projected onto the RBAC-owned
//! [`RbacRgGroup`], [`RbacRgMembership`], and [`RbacRgReadError`] types
//! that the domain consumes. The domain crate has no
//! `resource_group_sdk::` imports (the port-isolation invariant).

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use resource_group_sdk::api::ResourceGroupReadHierarchy;
use resource_group_sdk::error::ResourceGroupError;
use resource_group_sdk::models::{ResourceGroup, ResourceGroupMembership};
use resource_group_sdk::odata::GroupFilterField;
use toolkit_odata::filter::FilterField;
use toolkit_odata::{CursorV1, ODataQuery, Page};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::ports::principal_name_reader::non_blank;
use crate::domain::rg_port::{RbacRgGroup, RbacRgMembership, RbacRgRead, RbacRgReadError};

/// Ids per `id in (...)` filter, and the page size requested for each
/// such listing.
///
/// Bounded on both ends: small enough that a large role-assignment page
/// cannot build a filter the upstream parser rejects, and at or below the
/// RG group listing's own `max` page size (200) so the requested limit is
/// never clamped down under the chunk size. A page needing more group
/// names issues more calls rather than silently losing names.
pub const GROUP_NAME_CHUNK: usize = 50;

/// Safety bound on the per-chunk cursor drain. One chunk asks for
/// `GROUP_NAME_CHUNK` rows and can therefore be answered in a single
/// page; the drain exists only so a smaller upstream page size (or a
/// clamped limit) still yields every requested name. The bound stops a
/// misbehaving upstream that always returns a `next_cursor` from
/// spinning here forever.
const GROUP_NAME_MAX_PAGES_PER_CHUNK: usize = 8;

/// Adapter bridging the unscoped `dyn ResourceGroupReadHierarchy` read
/// contract onto the narrow `RbacRgRead` port.
pub struct ResourceGroupReadAdapter {
    inner: Arc<dyn ResourceGroupReadHierarchy>,
}

impl ResourceGroupReadAdapter {
    /// Wrap a `ResourceGroupReadHierarchy` resolved from `ClientHub`.
    #[must_use]
    pub(crate) fn new(inner: Arc<dyn ResourceGroupReadHierarchy>) -> Self {
        Self { inner }
    }
}

#[async_trait]
impl RbacRgRead for ResourceGroupReadAdapter {
    async fn get_group(
        &self,
        ctx: &SecurityContext,
        id: Uuid,
    ) -> Result<RbacRgGroup, RbacRgReadError> {
        self.inner
            .get_group(ctx, id)
            .await
            .map(|g| project_group(&g))
            .map_err(project_error)
    }

    /// Batch group-name resolution over `list_groups` with an
    /// `id in (...)` filter — the batch-read pattern this narrow trait
    /// documents and the tenant-resolver RG plugin already uses for
    /// `get_tenants(&[TenantId])`. Group ids are globally unique, so one
    /// listing covers every group principal on a role-assignment page
    /// regardless of tenant.
    ///
    /// Two details that are easy to get wrong and cost silent name loss:
    ///
    /// * the request MUST carry an explicit limit. `ODataQuery::default()`
    ///   leaves `limit = None`, which the RG repository resolves to its
    ///   *default* page size (25) — well below one chunk — so half a
    ///   chunk's names would simply never arrive;
    /// * the response is still a page, so a `next_cursor` has to be
    ///   followed rather than assumed absent.
    ///
    /// A failing chunk stops the drain but does **not** discard the names
    /// the earlier chunks already produced: a page with more group
    /// principals than one chunk holds would otherwise lose 100 resolved
    /// names because the third listing timed out, and a partially named
    /// page is strictly better than an unnamed one. The error surfaces
    /// only when nothing at all could be resolved, which is what lets the
    /// caller log the cause once instead of once per row. Same shape as
    /// the user reader's membership pass, deliberately — a consumer needs
    /// one rule for both, not two.
    async fn group_names(
        &self,
        ctx: &SecurityContext,
        ids: &[Uuid],
    ) -> Result<HashMap<Uuid, String>, RbacRgReadError> {
        let mut unique: Vec<Uuid> = ids.to_vec();
        unique.sort_unstable();
        unique.dedup();

        let mut out: HashMap<Uuid, String> = HashMap::with_capacity(unique.len());
        // Captured rather than propagated immediately; see the doc above.
        let mut failure: Option<RbacRgReadError> = None;
        'chunks: for chunk in unique.chunks(GROUP_NAME_CHUNK) {
            // `Expr` is not `Copy` and `ODataQuery` takes the filter by
            // value, so build it once per chunk and clone it per page —
            // the same idiom the evaluator's membership pagination uses.
            let filter = toolkit_odata::ast::Expr::In(
                Box::new(toolkit_odata::ast::Expr::Identifier(
                    GroupFilterField::Id.name().to_owned(),
                )),
                chunk
                    .iter()
                    .map(|id| toolkit_odata::ast::Expr::Value(toolkit_odata::ast::Value::Uuid(*id)))
                    .collect(),
            );

            let mut cursor: Option<CursorV1> = None;
            for _ in 0..GROUP_NAME_MAX_PAGES_PER_CHUNK {
                let mut query = ODataQuery::default()
                    .with_filter(filter.clone())
                    .with_limit(chunk.len() as u64);
                if let Some(c) = cursor.take() {
                    query = query.with_cursor(c);
                }
                let page = match self.inner.list_groups(ctx, &query).await {
                    Ok(page) => page,
                    Err(err) => {
                        failure = Some(project_error(err));
                        break 'chunks;
                    }
                };
                for group in &page.items {
                    // A blank upstream name is treated as no name at all:
                    // `"principal_name": "   "` renders as an empty cell
                    // that reads as a bug, while an absent field renders
                    // as the group id. Same rule as every other name
                    // source; see `non_blank`.
                    if let Some(name) = non_blank(group.name.clone()) {
                        out.insert(group.id, name);
                    }
                }
                match page.page_info.next_cursor {
                    // Upstream emits `CursorV1::encode()`; decode
                    // round-trips its keyset/sort/filter fields — a
                    // hand-built literal would fail the DB-side
                    // `k.len() == order.len()` check.
                    Some(token) => match CursorV1::decode(&token) {
                        Ok(c) => cursor = Some(c),
                        Err(err) => {
                            // A name is never worth failing a read for:
                            // stop draining and keep what resolved.
                            tracing::debug!(
                                target: "rbac.principal_names",
                                error = %err,
                                "group-name listing returned an undecodable cursor; \
                                 keeping the names resolved so far"
                            );
                            break;
                        }
                    },
                    None => break,
                }
            }
        }

        if let Some(err) = failure {
            if out.is_empty() {
                return Err(err);
            }
            tracing::debug!(
                target: "rbac.principal_names",
                error = %err,
                resolved = out.len(),
                "group-name listing failed after earlier chunks resolved; \
                 serving the partial result"
            );
        }
        Ok(out)
    }

    async fn list_memberships(
        &self,
        ctx: &SecurityContext,
        query: &ODataQuery,
    ) -> Result<Page<RbacRgMembership>, RbacRgReadError> {
        let page = self
            .inner
            .list_memberships(ctx, query)
            .await
            .map_err(project_error)?;
        Ok(Page {
            items: page.items.iter().map(project_membership).collect(),
            page_info: page.page_info,
        })
    }
}

/// Project the upstream `ResourceGroup` shape onto RBAC's slice.
fn project_group(group: &ResourceGroup) -> RbacRgGroup {
    RbacRgGroup {
        id: group.id,
        tenant_id: group.hierarchy.tenant_id,
        name: group.name.clone(),
    }
}

/// Project the upstream `ResourceGroupMembership` shape onto RBAC's slice.
fn project_membership(membership: &ResourceGroupMembership) -> RbacRgMembership {
    RbacRgMembership {
        group_id: membership.group_id,
    }
}

/// Translate the upstream error discriminator onto the RBAC-owned one.
/// `NotFound` is callable-handleable; everything else rides in
/// `Upstream` with the source preserved through `Box<dyn Error>`.
fn project_error(err: toolkit_canonical_errors::CanonicalError) -> RbacRgReadError {
    match ResourceGroupError::from(err) {
        ResourceGroupError::NotFound { .. } => RbacRgReadError::NotFound,
        other => RbacRgReadError::Upstream(Box::new(other)),
    }
}

#[cfg(test)]
#[path = "rg_adapter_tests.rs"]
mod rg_adapter_tests;
