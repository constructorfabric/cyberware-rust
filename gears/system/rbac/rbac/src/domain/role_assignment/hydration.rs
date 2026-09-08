//! Display-name hydration for role-assignment reads.
//!
//! This module owns the two decisions that must not leak anywhere else:
//!
//! * **which reader answers for which name** — users through the
//!   [`PrincipalNameReader`] port, groups through [`RbacRgRead`],
//!   `ServicePrincipal` not at all (nothing on the platform maps an SP
//!   subject id back to its client id, so there is no reader to call),
//!   and the granted role definition through RBAC's own
//!   [`RoleDefinitionRepository`] — the one name on the row that needs
//!   no other gear, and so the one that stays resolvable when every
//!   upstream is down;
//! * **which tenant a user is looked up in** — the tenant carried by the
//!   row's scope, or the platform root tenant for a root-scoped row. That
//!   is a heuristic, and a deliberate one: a principal living in another
//!   tenant (a partner admin granted a role on a child tenant) does not
//!   resolve there and degrades to no name. The alternative — widening
//!   the create contract with the principal's tenant — buys exactness for
//!   new rows only and does nothing for existing ones.
//!
//! Two invariants hold throughout:
//!
//! * **Hydration is infallible.** Every upstream failure, miss, timeout
//!   or unsupported kind produces a row with an absent name. A caller
//!   cannot turn a display concern into a failed read, so the status
//!   code, the row set and the cursor are identical whether resolution
//!   succeeded, failed, or never ran.
//! * **Work is batched per page, never per row.** One call per distinct
//!   lookup tenant for users (holders and authors together, since they
//!   resolve through the same reader), one batched listing for every
//!   group id on the page, one batched local query for every role id on
//!   the page.
//!
//! And two bounds, because "batched per page" is not by itself a bound:
//! the page decides how many *distinct tenants* it spans, and the caller
//! who wrote the assignments decides that. So one request may visit at
//! most [`HydrationLimits::max_lookup_tenants`] tenants, and the whole
//! hydration shares one deadline
//! ([`HydrationLimits::resolve_timeout`]). Both degrade the same way
//! everything else here does — the page is served with the names that
//! resolved before the bound was hit.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use rbac_sdk::models::PrincipalType;
use tenant_resolver_sdk::TenantResolverClient;
use tokio::time::{Instant, timeout_at};
use toolkit_db::{DBProvider, DbError};
use toolkit_macros::domain_model;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::config::PrincipalNamesConfig;
use crate::domain::model::RoleAssignmentModel;
use crate::domain::ports::metrics::{NameKind, NameOutcome, PrincipalNameMetricsPort};
use crate::domain::ports::principal_name_reader::{PrincipalNameReader, non_blank};
use crate::domain::rg_port::RbacRgRead;
use crate::domain::role_assignment::HydratedRoleAssignment;
use crate::domain::role_definition_repo::{RoleDefinitionRepository, RoleDefinitionVisibility};

/// One user lookup: which tenant, which principal id.
type UserKey = (Uuid, String);

/// Per-request bounds on display-name resolution.
///
/// The per-tenant budgets live on [`PrincipalNamesConfig`] and bound one
/// *tenant*; these bound one *request*, and without them the two do not
/// compose. A page whose principals live in 60 tenants would otherwise
/// spend 60 × (pass budget + point-lookup budget) upstream calls
/// sequentially, with no deadline — which is not degradation, it is a
/// hang on a read path that holds a connection.
///
/// Carried as a small value type rather than the whole config so the
/// domain layer depends on two numbers instead of on the shape of the
/// gear's config file.
#[domain_model]
#[derive(Debug, Clone, Copy)]
pub struct HydrationLimits {
    /// Wall clock for the whole hydration of one request, across every
    /// lookup it makes. On expiry the page is served with whatever
    /// resolved so far.
    pub resolve_timeout: Duration,
    /// Maximum number of distinct lookup tenants one request resolves
    /// user names in. Rows whose tenant falls outside the budget keep
    /// their ids.
    pub max_lookup_tenants: usize,
}

impl Default for HydrationLimits {
    /// The configured defaults. A hydrator built without explicit limits
    /// is still bounded — an unbounded default would make the bound
    /// something an operator has to remember to switch on.
    fn default() -> Self {
        Self::from_config(&PrincipalNamesConfig::default())
    }
}

impl HydrationLimits {
    /// Read the two per-request bounds out of the gear config.
    #[must_use]
    pub fn from_config(cfg: &PrincipalNamesConfig) -> Self {
        Self {
            resolve_timeout: cfg.resolve_timeout(),
            max_lookup_tenants: cfg.lookup_tenants_per_request(),
        }
    }
}

/// Resolves display names for a page of role-assignment rows.
#[domain_model]
pub struct PrincipalNameHydrator<DR: RoleDefinitionRepository> {
    db: DBProvider<DbError>,
    users: Arc<dyn PrincipalNameReader>,
    rg: Arc<dyn RbacRgRead>,
    /// RBAC's own role-definition store. Held as the repository rather
    /// than behind a narrow name-reader port because there is nothing to
    /// abstract: the rows are local, `find_by_ids` is already the batched,
    /// chunked, `allow_all` read the service uses on the create path, and
    /// a second port over the same table would only be a place for the two
    /// reads to drift apart.
    roles: Arc<DR>,
    tenant_resolver: Arc<dyn TenantResolverClient>,
    metrics: Arc<dyn PrincipalNameMetricsPort>,
    /// Per-request bounds. Defaulted at construction so a hydrator is
    /// never unbounded, and overridden from config through
    /// [`PrincipalNameHydrator::with_limits`].
    limits: HydrationLimits,
}

impl<DR: RoleDefinitionRepository> std::fmt::Debug for PrincipalNameHydrator<DR> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PrincipalNameHydrator")
            .finish_non_exhaustive()
    }
}

/// The author's kind and home tenant, when the row records them.
///
/// `created_by` alone cannot be resolved to a person: it carries no kind
/// (user or machine?) and no tenant (whom do we ask?). Both facts are free
/// at create time from the caller's `SecurityContext`, so they are stamped
/// onto the row and read back here.
///
/// Both columns are required, hence the `zip`: a kind with no tenant names
/// no reader's input, and a tenant with no kind cannot say *which* reader
/// to use. Either missing — a row written before the columns existed, a
/// machine author, or a stored kind this binary cannot parse — is the
/// documented legacy shape: `created_by` is served with no
/// `created_by_name`.
fn author_identity(model: &RoleAssignmentModel) -> Option<(PrincipalType, Uuid)> {
    model.created_by_type.zip(model.created_by_tenant_id)
}

/// The distinct upstream lookups one page needs, grouped by the reader that
/// answers them: user ids per lookup tenant (holders and authors share a
/// reader) and group ids. A named struct rather than a tuple because two
/// collections of ids are easy to transpose at a call site.
///
/// Role-definition ids are deliberately absent: they need no upstream and no
/// root-tenant lookup, so [`PrincipalNameHydrator::collect_role_ids`]
/// gathers them separately and the local name resolves before any gear call.
struct WantedNames {
    users: HashMap<Uuid, HashSet<String>>,
    groups: HashSet<Uuid>,
}

impl<DR: RoleDefinitionRepository> PrincipalNameHydrator<DR> {
    /// Assemble a hydrator. No I/O; the AM client behind `users` is
    /// resolved lazily on first use.
    #[must_use]
    pub fn new(
        db: DBProvider<DbError>,
        users: Arc<dyn PrincipalNameReader>,
        rg: Arc<dyn RbacRgRead>,
        roles: Arc<DR>,
        tenant_resolver: Arc<dyn TenantResolverClient>,
        metrics: Arc<dyn PrincipalNameMetricsPort>,
    ) -> Self {
        Self {
            db,
            users,
            rg,
            roles,
            tenant_resolver,
            metrics,
            limits: HydrationLimits::default(),
        }
    }

    /// Override the per-request bounds from the gear config.
    ///
    /// Chainable rather than a `new` parameter for the same reason
    /// `RoleAssignmentService::with_hydrator` is: the bounds are an
    /// operator concern, and every test construction site wants the
    /// defaults. A hydrator that never sees this call is still bounded —
    /// by [`HydrationLimits::default`], which is the configured default.
    #[must_use]
    pub fn with_limits(mut self, cfg: &PrincipalNamesConfig) -> Self {
        self.limits = HydrationLimits::from_config(cfg);
        self
    }

    /// Hydrate a page of rows. Never fails; unresolved names are absent.
    ///
    /// Output is 1:1 with the input, in the same order — callers rely on
    /// that to keep the page envelope `list` produced.
    ///
    /// `role_visibility` is the caller's own role-definition visibility,
    /// derived on the assignment read path. `None` means "it could not be
    /// derived", and the *only* correct reaction to that on a display
    /// path is to resolve no role names: the alternative would either
    /// fail an assignment read over a decoration, or fall back to an
    /// unnarrowed read and disclose the name of a role the catalog
    /// answers `404` for.
    ///
    /// Everything below shares one deadline. It is deliberately taken
    /// here, once, rather than per lookup: the point is to bound *the
    /// request*, and a per-lookup timeout multiplied by the number of
    /// lookups is not a bound on anything a caller can observe.
    pub async fn hydrate(
        &self,
        ctx: &SecurityContext,
        rows: Vec<RoleAssignmentModel>,
        role_visibility: Option<RoleDefinitionVisibility>,
    ) -> Vec<HydratedRoleAssignment> {
        if rows.is_empty() {
            return Vec::new();
        }

        let deadline = Instant::now() + self.limits.resolve_timeout;
        // Cheapest and most available first. Every phase shares one deadline,
        // so whichever runs first is the one that still resolves when the rest
        // of the platform is slow — and role names are the only ones this gear
        // can answer on its own, from its own table, with no upstream in the
        // path. Resolving them last would have handed their budget to exactly
        // the readers most likely to burn it, and a page would lose a
        // millisecond-cheap local name because Keycloak was busy.
        let role_names = self
            .resolve_role_names(Self::collect_role_ids(&rows), role_visibility, deadline)
            .await;
        // Upstream-backed names follow, groups before users: an RG listing is
        // one bounded call per chunk, while a user pass can spend a full
        // membership drain per tenant.
        let root_tenant = self.root_tenant_if_needed(ctx, &rows, deadline).await;
        let wanted = Self::collect_work(&rows, root_tenant);
        let group_names = self.resolve_group_names(ctx, wanted.groups, deadline).await;
        let user_names = self.resolve_user_names(ctx, wanted.users, deadline).await;

        self.merge(rows, root_tenant, &user_names, &group_names, &role_names)
    }

    /// Resolve the platform root tenant, but only when a root-scoped row
    /// is actually on the page: a page of tenant-scoped rows must not pay
    /// for a tenant-resolver round trip it cannot use.
    async fn root_tenant_if_needed(
        &self,
        ctx: &SecurityContext,
        rows: &[RoleAssignmentModel],
        deadline: Instant,
    ) -> Option<Uuid> {
        if !rows.iter().any(|r| r.scope.tenant_id().is_none()) {
            return None;
        }
        match timeout_at(deadline, self.tenant_resolver.get_root_tenant(ctx)).await {
            Ok(Ok(info)) => Some(info.id.0),
            Ok(Err(err)) => {
                tracing::debug!(
                    target: "rbac.principal_names",
                    error = %err,
                    "root-tenant lookup failed; root-scoped rows stay unnamed"
                );
                None
            }
            Err(_elapsed) => {
                tracing::debug!(
                    target: "rbac.principal_names",
                    "display-name resolution hit its deadline resolving the root tenant; \
                     root-scoped rows stay unnamed"
                );
                None
            }
        }
    }

    /// Gather the distinct lookups a page needs: user ids grouped by
    /// lookup tenant (holders and authors together), the set of group ids,
    /// and the set of role-definition ids. Deduplication here is what makes
    /// the read cost a function of the page's *distinct* principals and
    /// roles rather than its row count — an authorization grid typically
    /// shows the same handful of roles on every row, so the role set is
    /// usually far smaller than the page.
    /// Role-definition ids referenced by the page.
    ///
    /// Split out from [`Self::collect_work`] because it needs nothing from
    /// upstream: every row carries a non-null FK to a role definition, so this
    /// set is available before the root-tenant lookup and lets the local name
    /// resolve ahead of any gear call.
    fn collect_role_ids(rows: &[RoleAssignmentModel]) -> HashSet<Uuid> {
        rows.iter().map(|row| row.role_definition_id).collect()
    }

    fn collect_work(rows: &[RoleAssignmentModel], root_tenant: Option<Uuid>) -> WantedNames {
        let mut wanted_users: HashMap<Uuid, HashSet<String>> = HashMap::new();
        let mut wanted_groups: HashSet<Uuid> = HashSet::new();
        for row in rows {
            let row_tenant = row.scope.tenant_id().or(root_tenant);
            match row.principal_type {
                PrincipalType::User => {
                    if let Some(tenant) = row_tenant {
                        wanted_users
                            .entry(tenant)
                            .or_default()
                            .insert(row.principal_id.clone());
                    }
                }
                PrincipalType::Group => {
                    if let Ok(id) = Uuid::parse_str(&row.principal_id) {
                        wanted_groups.insert(id);
                    }
                }
                // `ServicePrincipal`, and any kind added later: no reader
                // exists, so nothing to ask for.
                _ => {}
            }
            // The author rides the same reader, keyed by the identity the
            // row recorded at create time — never by the reader's guess.
            if let Some((PrincipalType::User, tenant)) = author_identity(row) {
                wanted_users
                    .entry(tenant)
                    .or_default()
                    .insert(row.created_by.clone());
            }
        }
        WantedNames {
            users: wanted_users,
            groups: wanted_groups,
        }
    }

    /// One reader call per distinct lookup tenant. A failing tenant leaves
    /// its ids unresolved without affecting the others.
    ///
    /// This loop is where the per-request bounds bite, and it is the only
    /// place they can: each iteration may cost the whole per-tenant budget
    /// (a bounded membership pass plus point lookups), and each of those
    /// calls is a full Keycloak group-membership drain upstream. So the
    /// number of iterations is capped, and the loop stops at the shared
    /// deadline. Both leave the remaining tenants' rows carrying their
    /// ids, which is the ordinary degradation — no error, no short page.
    ///
    /// Tenants are visited most-ids-first, ties broken by id. Not
    /// cosmetic: `HashMap` iteration order varies per process, so an
    /// arbitrary order would spend the budget on a different subset of
    /// tenants on every request and a row would be named or unnamed at
    /// random across two renders of the same page. Most-ids-first also
    /// buys the most named rows per call.
    // The budget, deadline and fallback arms ARE this function: the branch
    // count is the per-request bound, and hiding half of it in a helper would
    // put the two halves of one budget in two places.
    #[allow(clippy::cognitive_complexity)]
    async fn resolve_user_names(
        &self,
        ctx: &SecurityContext,
        wanted: HashMap<Uuid, HashSet<String>>,
        deadline: Instant,
    ) -> HashMap<UserKey, String> {
        let mut resolved: HashMap<UserKey, String> = HashMap::new();
        let mut by_tenant: Vec<(Uuid, HashSet<String>)> = wanted.into_iter().collect();
        by_tenant.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then_with(|| a.0.cmp(&b.0)));

        let budget = self.limits.max_lookup_tenants;
        if by_tenant.len() > budget {
            tracing::debug!(
                target: "rbac.principal_names",
                tenants = by_tenant.len(),
                budget,
                "page spans more lookup tenants than one request may visit; \
                 the tenants beyond the budget keep their ids"
            );
        }

        for (tenant, ids) in by_tenant.into_iter().take(budget) {
            let ids: Vec<String> = ids.into_iter().collect();
            match timeout_at(deadline, self.users.user_names(ctx, tenant, &ids)).await {
                Ok(Ok(found)) => {
                    for (id, name) in found {
                        resolved.insert((tenant, id), name);
                    }
                }
                Ok(Err(err)) => {
                    tracing::debug!(
                        target: "rbac.principal_names",
                        tenant_id = %tenant,
                        error = %err,
                        "user name resolution degraded; rows keep their ids"
                    );
                }
                Err(_elapsed) => {
                    // The deadline is shared, so a later tenant cannot
                    // succeed where this one ran out of time: stop rather
                    // than pay a round trip per remaining tenant to be
                    // told the same thing.
                    tracing::debug!(
                        target: "rbac.principal_names",
                        tenant_id = %tenant,
                        resolved = resolved.len(),
                        "display-name resolution hit its deadline; serving the page with \
                         the names resolved so far"
                    );
                    break;
                }
            }
        }
        resolved
    }

    /// One batched listing for every group id on the page — group ids are
    /// globally unique, so tenant plays no part here.
    async fn resolve_group_names(
        &self,
        ctx: &SecurityContext,
        wanted: HashSet<Uuid>,
        deadline: Instant,
    ) -> HashMap<Uuid, String> {
        if wanted.is_empty() {
            return HashMap::new();
        }
        let ids: Vec<Uuid> = wanted.into_iter().collect();
        match timeout_at(deadline, self.rg.group_names(ctx, &ids)).await {
            Ok(Ok(found)) => found,
            Ok(Err(err)) => {
                tracing::debug!(
                    target: "rbac.principal_names",
                    error = %err,
                    "group name resolution degraded; rows keep their ids"
                );
                HashMap::new()
            }
            Err(_elapsed) => {
                tracing::debug!(
                    target: "rbac.principal_names",
                    "display-name resolution hit its deadline before the group listing; \
                     group rows keep their ids"
                );
                HashMap::new()
            }
        }
    }

    /// One batched local query for every role id on the page, narrowed by
    /// the caller's own role-definition visibility.
    ///
    /// This read is cheap in a way the other two are not: the rows live in
    /// RBAC's own `role_definitions` table, the query is already chunked,
    /// and no upstream gear, tenant heuristic or cache is involved. It is
    /// still allowed to fail, and failing costs the page nothing but the
    /// names — the same contract the upstream-backed names have, so a
    /// consumer needs one rule rather than three.
    ///
    /// The narrowing is not an optimization. `find_by_ids` reads with
    /// `AccessScope::allow_all()` and applies no visibility, which is
    /// right for the create path and wrong here: an ancestor-tenant admin
    /// may grant an ancestor-owned *custom* role at a descendant scope,
    /// and the descendant's admin may then read that assignment row — so
    /// an unnarrowed name would tell them what a custom role they cannot
    /// fetch is called, while `GET /rbac/v1/role-definitions/{id}` answers
    /// `404` for exactly that id to avoid the disclosure. Built-in roles
    /// stay visible to every authenticated caller, so the common
    /// case is unchanged.
    ///
    /// `visibility = None` means the caller's visibility could not be
    /// derived. That resolves no names at all rather than falling back to
    /// the unnarrowed read: a decoration must never be the reason a read
    /// discloses something, and it must never be the reason a read fails
    /// either.
    ///
    /// A role id with no row is not an anomaly worth shouting about: the FK
    /// is `ON DELETE RESTRICT`, but a definition deleted inside the race
    /// window between the assignment read and this one leaves exactly this
    /// shape — and so, indistinguishably and by design, does a role the
    /// caller may not see.
    // Same shape as `resolve_user_names`: the deadline, the visibility arms
    // and the missing-row arm are the contract, not incidental branching.
    #[allow(clippy::cognitive_complexity)]
    async fn resolve_role_names(
        &self,
        wanted: HashSet<Uuid>,
        visibility: Option<RoleDefinitionVisibility>,
        deadline: Instant,
    ) -> HashMap<Uuid, String> {
        if wanted.is_empty() {
            return HashMap::new();
        }
        let Some(visibility) = visibility else {
            tracing::debug!(
                target: "rbac.principal_names",
                "role-definition visibility could not be derived; rows keep their role ids"
            );
            return HashMap::new();
        };
        let ids: Vec<Uuid> = wanted.into_iter().collect();
        // Display-name resolution is additive: a connection we cannot get
        // degrades to unnamed rows, the same as a read error or a missed
        // deadline below. It must never fail the page.
        let conn = match self.db.conn() {
            Ok(conn) => conn,
            Err(err) => {
                tracing::debug!(
                    target: "rbac.principal_names",
                    error = %err,
                    "no connection for role-definition names; rows keep their role ids"
                );
                return HashMap::new();
            }
        };
        match timeout_at(
            deadline,
            self.roles.find_by_ids_visible(&conn, visibility, &ids),
        )
        .await
        {
            Ok(Ok(found)) => found
                .into_iter()
                // A blank stored name is treated as no name; see the
                // merge step for why absence beats an empty string.
                .filter_map(|role| {
                    let id = role.id;
                    non_blank(role.name).map(|name| (id, name))
                })
                .collect(),
            Ok(Err(err)) => {
                tracing::debug!(
                    target: "rbac.principal_names",
                    error = %err,
                    "role-definition name resolution degraded; rows keep their role ids"
                );
                HashMap::new()
            }
            Err(_elapsed) => {
                tracing::debug!(
                    target: "rbac.principal_names",
                    "display-name resolution hit its deadline before the role-definition \
                     read; rows keep their role ids"
                );
                HashMap::new()
            }
        }
    }

    /// Attach whatever resolved to each row, and report the per-name
    /// outcome.
    ///
    /// Counting happens here rather than at the call sites so each
    /// *name on the page* is counted exactly once, whatever mix of
    /// cache hits, upstream calls and failures produced the maps. That
    /// makes the counter answer the operator's real question — "how many
    /// rows came out named?" — instead of "how many upstream calls
    /// happened".
    ///
    /// This is also the last gate a name passes before the wire, so it is
    /// where the "never an empty string" rule is enforced for *every*
    /// source at once: each name goes through [`non_blank`] on its way
    /// onto the row. Each source applies the same rule at its own end,
    /// and that is not redundant — a source added later would land here
    /// whether or not its author remembered, and the failure mode this
    /// prevents (a blank cell where the id should have rendered) is
    /// invisible in tests that only assert "a name arrived".
    fn merge(
        &self,
        rows: Vec<RoleAssignmentModel>,
        root_tenant: Option<Uuid>,
        user_names: &HashMap<UserKey, String>,
        group_names: &HashMap<Uuid, String>,
        role_names: &HashMap<Uuid, String>,
    ) -> Vec<HydratedRoleAssignment> {
        let mut counts: HashMap<(NameKind, NameOutcome), u64> = HashMap::new();
        // The counting closure borrows `counts`; the block scope ends that
        // borrow before the samples are drained below.
        let hydrated: Vec<HydratedRoleAssignment> = {
            let mut bump = |kind: NameKind, outcome: NameOutcome| {
                *counts.entry((kind, outcome)).or_default() += 1;
            };
            rows.into_iter()
                .map(|model| {
                    let row_tenant = model.scope.tenant_id().or(root_tenant);
                    let principal_name = match model.principal_type {
                        PrincipalType::User => {
                            let name = row_tenant
                                .and_then(|tenant| {
                                    user_names
                                        .get(&(tenant, model.principal_id.clone()))
                                        .cloned()
                                })
                                .and_then(non_blank);
                            bump(NameKind::User, outcome_of(name.as_deref()));
                            name
                        }
                        PrincipalType::Group => {
                            let name = Uuid::parse_str(&model.principal_id)
                                .ok()
                                .and_then(|id| group_names.get(&id).cloned())
                                .and_then(non_blank);
                            bump(NameKind::Group, outcome_of(name.as_deref()));
                            name
                        }
                        // No reader for this kind — a permanent platform
                        // gap, not an outage, so it is counted under its
                        // own kind and its own outcome.
                        _ => {
                            bump(NameKind::Other, NameOutcome::Unsupported);
                            None
                        }
                    };
                    let created_by_name =
                        if let Some((PrincipalType::User, tenant)) = author_identity(&model) {
                            let name = user_names
                                .get(&(tenant, model.created_by.clone()))
                                .cloned()
                                .and_then(non_blank);
                            bump(NameKind::Author, outcome_of(name.as_deref()));
                            name
                        } else {
                            // Either no author identity was recorded (a legacy
                            // row, or a machine author such as the platform
                            // bootstrap) or the author is not a user. Nothing can
                            // name it, and that is expected rather than degraded.
                            bump(NameKind::Author, NameOutcome::Unsupported);
                            None
                        };
                    // The role name needs no tenant, no kind and no
                    // upstream — only the row's FK — so it is the one
                    // lookup that resolves identically for every row shape.
                    let role_definition_name = role_names
                        .get(&model.role_definition_id)
                        .cloned()
                        .and_then(non_blank);
                    bump(
                        NameKind::RoleDefinition,
                        outcome_of(role_definition_name.as_deref()),
                    );
                    HydratedRoleAssignment {
                        model,
                        principal_name,
                        created_by_name,
                        role_definition_name,
                    }
                })
                .collect()
        };

        for ((kind, outcome), count) in counts {
            self.metrics.principal_name_resolve(kind, outcome, count);
        }
        hydrated
    }
}

/// A name either resolved or it did not; "why not" belongs in the logs,
/// not in a metric label.
fn outcome_of(name: Option<&str>) -> NameOutcome {
    if name.is_some() {
        NameOutcome::Resolved
    } else {
        NameOutcome::Degraded
    }
}

#[cfg(test)]
#[path = "hydration_tests.rs"]
mod hydration_tests;
