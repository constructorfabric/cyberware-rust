// Created: 2026-04-16 by Constructor Tech
// Updated: 2026-04-28 by Constructor Tech
// @cpt-begin:cpt-cf-resource-group-dod-entity-hier-entity-service:p1:inst-full
// @cpt-dod:cpt-cf-resource-group-dod-testing-entity-hierarchy:p1
//! Domain service for resource group entity management.
//!
//! Implements business rules: type validation, parent compatibility,
//! cycle detection, closure table management, query profile enforcement,
//! and CRUD orchestration.
//!
//! Every write runs in a transaction with bounded retry (max 3 attempts).
//! The isolation level is chosen per operation, not fixed:
//!
//! - `SERIALIZABLE` where a write depends on a predicate over rows it does
//!   not itself lock — `create_group`, `move_group`, `update_group`'s
//!   parent-change branch, and a force delete, all of which rewrite closure
//!   rows across a subtree.
//! - The backend default where there is no such predicate: `update_group`'s
//!   rename/metadata path, which changes one row by primary key, and a
//!   non-force delete, which takes a row lock on its target so the children
//!   and membership checks it decides from stay true until it commits.
//!
//! See `update_group` for how a level is picked before the transaction opens
//! when the answer is not yet known, and how the race with that guess is
//! closed. `delete_group` needs none of that: `force` is a request field.

use std::sync::Arc;

use authz_resolver_sdk::pep::{PolicyEnforcer, ResourceType};
use resource_group_sdk::models::{
    CreateGroupRequest, GroupHierarchy, ResourceGroup, ResourceGroupWithDepth, UpdateGroupRequest,
};
use resource_group_sdk::{GROUP_RESOURCE_TYPE, TENANT_RG_TYPE_PATH};
use toolkit_db::secure::{DBRunner, Db, TxConfig};
use toolkit_odata::{ODataQuery, Page};
use toolkit_security::{SecurityContext, pep_properties};
use tracing::debug;
use uuid::Uuid;

use crate::domain::DbProvider;
use crate::domain::error::DomainError;
use crate::domain::metrics::{NoopMetrics, Operation, Outcome, RgMetricsPort};
use crate::domain::repo::{GroupRepositoryTrait, TypeRepositoryTrait};
use crate::domain::validation;

/// `AuthZ` resource type descriptor for resource groups.
///
/// Deliberately carries no `with_group_membership_type` mapping: groups have
/// no RG member-handle type. Group nesting is modeled through
/// `resource_group.parent_id` and `resource_group_closure` (hierarchy), never
/// through `resource_group_membership` rows, and `GROUP_RESOURCE_TYPE`
/// (`gts.cf.core.rg.group.v1~`) is a PEP policy resource name that can never
/// be registered in `gts_type` (`validate_type_code` requires the
/// `gts.cf.core.rg.type.v1~` prefix) — a mapping here would make every native
/// `in_group`/`in_group_subtree` predicate silently match zero rows. Without
/// the mapping, `PolicyEnforcer` suppresses the group capabilities per
/// request and the PDP must expand group scopes to explicit `in` predicates
/// on `id`, or deny.
pub const RG_GROUP_RESOURCE: ResourceType = ResourceType::from_static(
    GROUP_RESOURCE_TYPE,
    &[pep_properties::OWNER_TENANT_ID, pep_properties::RESOURCE_ID],
);

/// Query profile configuration for depth/width limits.
#[allow(unknown_lints, de0309_must_have_domain_model)]
#[derive(Debug, Clone)]
pub struct QueryProfile {
    /// Maximum depth allowed. `None` disables depth limit.
    pub max_depth: Option<u32>,
    /// Maximum width (children per parent) allowed. `None` disables width limit.
    pub max_width: Option<u32>,
}

impl Default for QueryProfile {
    fn default() -> Self {
        Self {
            max_depth: Some(10),
            max_width: None,
        }
    }
}

// @cpt-dod:cpt-cf-resource-group-dod-entity-hier-entity-service:p1
// @cpt-dod:cpt-cf-resource-group-dod-integration-auth-tenant-scope:p1
// @cpt-dod:cpt-cf-resource-group-dod-integration-auth-jwt:p1
// @cpt-flow:cpt-cf-resource-group-flow-integration-auth-jwt-request:p1
/// Service for resource group entity lifecycle management.
#[allow(unknown_lints, de0309_must_have_domain_model)]
#[derive(Clone)]
pub struct GroupService<GR: GroupRepositoryTrait, TR: TypeRepositoryTrait> {
    db: Arc<DbProvider>,
    profile: QueryProfile,
    enforcer: PolicyEnforcer,
    group_repo: Arc<GR>,
    type_repo: Arc<TR>,
    types_registry: Arc<dyn types_registry_sdk::TypesRegistryClient>,
    /// Defaults to `NoopMetrics`; the composition root installs the real
    /// recorder through `with_metrics`, whose doc carries the rationale for
    /// keeping it out of the constructor. An uninstrumented run -- including
    /// every test here -- records nothing and pays nothing.
    metrics: Arc<dyn RgMetricsPort>,
}

/// The isolation level one `update_group` attempt opens its transaction at.
///
/// Named rather than passed as a bare `bool`: `attempt_update_group` and
/// `update_group_inner` thread this value straight from the pre-transaction
/// hint through to `TxConfig` and back out to the `NeedsSerializable` check,
/// and a `bool` at those call sites reads as "the flag" rather than "which
/// level" -- exactly backwards for the one parameter this whole branch of
/// the PR is about getting right.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WriteIsolation {
    /// The backend default. Correct for a rename or metadata-only edit:
    /// one row by primary key, locked explicitly
    /// (`find_model_by_id_for_update`) rather than protected by a
    /// cross-row predicate.
    RowLocked,
    /// A parent change. Cycle detection and the closure rebuild read a
    /// predicate over rows the write does not itself lock.
    Serializable,
}

impl WriteIsolation {
    fn tx_config(self) -> TxConfig {
        match self {
            Self::RowLocked => TxConfig::default(),
            Self::Serializable => TxConfig::serializable(),
        }
    }

    fn is_serializable(self) -> bool {
        matches!(self, Self::Serializable)
    }
}

/// Result of one `update_group_inner` attempt.
///
/// Internal control flow only -- it never crosses a `?` boundary into
/// `DomainError`, so it changes nothing a caller can observe.
/// `update_group` picks the transaction's isolation level from a
/// pre-transaction hint about whether the request moves the group;
/// `NeedsSerializable` is how the authoritative in-transaction read reports
/// that the hint was wrong in the direction that matters. It is returned
/// before that attempt writes anything, so abandoning the attempt is always
/// a no-op, never a partial write.
enum UpdateGroupOutcome {
    /// The update finished under a transaction strong enough for what it
    /// turned out to need.
    Done(ResourceGroup),
    /// The move branch is required, but this transaction is not
    /// SERIALIZABLE. Nothing was written.
    NeedsSerializable,
}

/// Enough of a new-parent row for `move_group_internal_impl`'s checks: the
/// GTS type id, which it resolves itself for the allowed-parent-type check,
/// and the tenant, which it does nothing with except hand back so the
/// caller can run its own cross-tenant check against the same read (see
/// `MoveOutcome`).
///
/// Built by the caller from a single `find_model_by_id`, not by this
/// function -- both call sites already need that row for their own purposes
/// (a pre-call cross-tenant check on the update path, the snapshot itself on
/// the move path), and a second read of the same id inside this function on
/// top of that was the redundant one this type exists to remove.
#[allow(unknown_lints, de0309_must_have_domain_model)]
pub(crate) struct ParentSnapshot {
    pub tenant_id: Uuid,
    pub gts_type_id: i16,
}

/// What a subtree move hands back to its caller: enough to assemble the
/// response and record the metric, and nothing of the persistence layer.
#[allow(unknown_lints, de0309_must_have_domain_model)]
pub(crate) struct MoveOutcome {
    pub parent_tenant_id: Option<Uuid>,
    pub closure_rows: u64,
}

impl<GR: GroupRepositoryTrait, TR: TypeRepositoryTrait> GroupService<GR, TR> {
    /// Create a new `GroupService` with the given database provider, query profile,
    /// and `PolicyEnforcer` for AuthZ-scoped queries.
    #[must_use]
    pub fn new(
        db: Arc<DbProvider>,
        profile: QueryProfile,
        enforcer: PolicyEnforcer,
        group_repo: Arc<GR>,
        type_repo: Arc<TR>,
        types_registry: Arc<dyn types_registry_sdk::TypesRegistryClient>,
    ) -> Self {
        Self {
            db,
            profile,
            enforcer,
            group_repo,
            type_repo,
            types_registry,
            metrics: Arc::new(NoopMetrics),
        }
    }

    /// Record one operation's wall time and whether it succeeded.
    ///
    /// Taken around the whole public method, so it includes the retries the
    /// caller never sees as separate attempts -- which is the point: a
    /// latency tail made of retried work looks like slow work from outside,
    /// and that is what a caller experiences.
    fn record_op<T>(
        &self,
        operation: Operation,
        started: std::time::Instant,
        result: &Result<T, DomainError>,
    ) {
        self.metrics.operation_duration(
            operation,
            if result.is_ok() {
                Outcome::Ok
            } else {
                Outcome::Error
            },
            started.elapsed().as_secs_f64(),
        );
    }

    /// Install a metrics recorder.
    ///
    /// Separate from `new` on purpose. The constructor has 150-odd call
    /// sites, nearly all of them tests that have no interest in metrics, and
    /// a required parameter would bury the change that matters in churn. The
    /// composition root -- `gear.rs`, which is where infrastructure belongs
    /// -- calls this; everything else keeps the no-op and records nothing.
    #[must_use]
    pub fn with_metrics(mut self, metrics: Arc<dyn RgMetricsPort>) -> Self {
        self.metrics = metrics;
        self
    }

    // @cpt-flow:cpt-cf-resource-group-flow-entity-hier-create-group:p1
    /// Create a new resource group.
    ///
    /// Runs inside a `SERIALIZABLE` transaction with bounded retry (max 3 attempts)
    /// to ensure invariant checks and closure table mutations are atomic.
    pub async fn create_group(
        &self,
        ctx: &SecurityContext,
        req: CreateGroupRequest,
        tenant_id: Uuid,
    ) -> Result<ResourceGroup, DomainError> {
        let started = std::time::Instant::now();
        // @cpt-begin:cpt-cf-resource-group-flow-entity-hier-create-group:p1:inst-create-group-1
        // Pre-validation (stateless, outside transaction)
        validation::validate_type_code(&req.code)?;
        Self::validate_name(&req.name)?;

        // Metadata validation belongs here rather than inside the transaction:
        // it resolves the chained GTS schema through `TypesRegistryClient` --
        // a network round-trip -- and then compiles it. Held open, that time
        // is snapshot lifetime and SSI read-set age, and every retry paid for
        // the lookup and the compile again. It reads no database state, so
        // nothing about the transaction makes its answer more correct.
        // @cpt-begin:cpt-cf-resource-group-flow-entity-hier-create-group:p1:inst-create-group-5b
        let validation_started = std::time::Instant::now();
        let validated = validation::validate_metadata_via_gts(
            req.metadata.as_ref(),
            &req.code,
            &*self.types_registry,
        )
        .await;
        self.metrics.metadata_validation_duration(
            Operation::Create,
            validation_started.elapsed().as_secs_f64(),
        );
        validated?;
        // @cpt-end:cpt-cf-resource-group-flow-entity-hier-create-group:p1:inst-create-group-5b

        // Derive `is_tenant` for AuthZ properties from the code prefix: any type
        // whose path starts with `TENANT_RG_TYPE_PATH` opens a new tenant scope.
        let is_tenant = req.code.starts_with(TENANT_RG_TYPE_PATH);

        // Reject a caller-supplied `tenant_id` on tenant-typed groups: the
        // effective tenant is always the group's own (generated) id.
        Self::reject_tenant_id_on_tenant_type(is_tenant, req.tenant_id)?;

        // Resolve the target tenant: omitted `tenant_id` defaults to the
        // caller's own tenant. A present `tenant_id` lets an authorized
        // caller (platform admin / onboarding) target a different tenant.
        let target_tenant_id = req.tenant_id.unwrap_or(tenant_id);

        // Guardrail: explicit id + cross-tenant target is rejected while
        // identifier ownership policy is undecided.
        if req.id.is_some() && target_tenant_id != tenant_id {
            return Err(DomainError::validation(
                "id and tenant_id cannot both be set on group creation: an explicit id \
                 combined with a cross-tenant target is not accepted while identifier \
                 ownership policy is undecided"
                    .to_owned(),
            ));
        }

        // AuthZ gate with provisioning context
        let scope =
            self.enforcer
                .access_scope_with(
                    ctx,
                    &RG_GROUP_RESOURCE,
                    "create",
                    None,
                    &authz_resolver_sdk::pep::enforcer::AccessRequest::default()
                        .resource_properties(std::collections::HashMap::from([
                            ("is_tenant".to_owned(), serde_json::Value::Bool(is_tenant)),
                            (
                                "parent_id".to_owned(),
                                req.parent_id.map_or(serde_json::Value::Null, |id| {
                                    serde_json::Value::String(id.to_string())
                                }),
                            ),
                            (
                                pep_properties::OWNER_TENANT_ID.to_owned(),
                                serde_json::Value::String(target_tenant_id.to_string()),
                            ),
                        ])),
                )
                .await
                .map_err(DomainError::from)?;
        // @cpt-end:cpt-cf-resource-group-flow-entity-hier-create-group:p1:inst-create-group-1

        // When the target tenant differs from the caller's own, re-verify
        // it against the compiled `AccessScope` rather than trusting the
        // PDP's `decision: true` alone: a policy that grants "create"
        // unconditionally must not become an unbounded cross-tenant create.
        // Skipped when the target is the caller's own tenant, so the common
        // path is unchanged.
        if target_tenant_id != tenant_id {
            let permitted = scope.is_unconstrained()
                || scope.contains_uuid(pep_properties::OWNER_TENANT_ID, target_tenant_id);
            if !permitted {
                debug!(
                    caller_tenant_id = %tenant_id,
                    target_tenant_id = %target_tenant_id,
                    "create_group rejected: target tenant outside caller's AccessScope"
                );
                return Err(DomainError::tenant_not_found(target_tenant_id));
            }
        }

        let profile = self.profile.clone();
        let db = self.db.db();
        let group_repo = self.group_repo.clone();
        let type_repo = self.type_repo.clone();

        // @cpt-begin:cpt-cf-resource-group-flow-entity-hier-create-group:p1:inst-create-group-2
        // @cpt-begin:cpt-cf-resource-group-flow-entity-hier-create-group:p1:inst-create-group-10
        // @cpt-begin:cpt-cf-resource-group-flow-entity-hier-create-group:p1:inst-create-group-9
        // @cpt-begin:cpt-cf-resource-group-flow-entity-hier-create-group:p1:inst-create-group-11
        let result = db
            .transaction_with_retry(TxConfig::serializable(), DomainError::db_err, |tx| {
                let req = req.clone();
                let profile = profile.clone();
                let group_repo = group_repo.clone();
                let type_repo = type_repo.clone();
                Box::pin(async move {
                    Self::create_group_inner(
                        &*group_repo,
                        &*type_repo,
                        tx,
                        &req,
                        target_tenant_id,
                        &profile,
                    )
                    .await
                })
            })
            .await;
        self.record_op(Operation::Create, started, &result);
        result
        // @cpt-end:cpt-cf-resource-group-flow-entity-hier-create-group:p1:inst-create-group-11
        // @cpt-end:cpt-cf-resource-group-flow-entity-hier-create-group:p1:inst-create-group-9
        // @cpt-end:cpt-cf-resource-group-flow-entity-hier-create-group:p1:inst-create-group-10
        // @cpt-end:cpt-cf-resource-group-flow-entity-hier-create-group:p1:inst-create-group-2
    }

    /// Get a resource group by ID (AuthZ-scoped).
    pub async fn get_group(
        &self,
        ctx: &SecurityContext,
        group_id: Uuid,
    ) -> Result<ResourceGroup, DomainError> {
        let scope = self
            .enforcer
            .access_scope(ctx, &RG_GROUP_RESOURCE, "get", Some(group_id))
            .await
            .map_err(DomainError::from)?;
        let conn = self.db.conn()?;
        self.group_repo
            .find_by_id(&conn, &scope, group_id)
            .await?
            .ok_or_else(|| DomainError::group_not_found(group_id))
    }

    // @cpt-algo:cpt-cf-resource-group-algo-integration-auth-auth-mode-decision:p1
    /// List resource groups with `OData` filtering and pagination (AuthZ-scoped).
    pub async fn list_groups(
        &self,
        ctx: &SecurityContext,
        query: &ODataQuery,
    ) -> Result<Page<ResourceGroup>, DomainError> {
        // @cpt-begin:cpt-cf-resource-group-algo-integration-auth-auth-mode-decision:p1:inst-auth-decide-3
        // IF request has JWT bearer token — the SecurityContext arrives here
        // already authenticated by the API Gateway / AuthNResolverClient.
        // @cpt-begin:cpt-cf-resource-group-algo-integration-auth-auth-mode-decision:p1:inst-auth-decide-3a
        // Authenticate via AuthNResolverClient → SecurityContext (performed
        // upstream by the API Gateway; `ctx` carries the resulting subject).
        // @cpt-end:cpt-cf-resource-group-algo-integration-auth-auth-mode-decision:p1:inst-auth-decide-3a
        // @cpt-begin:cpt-cf-resource-group-algo-integration-auth-auth-mode-decision:p1:inst-auth-decide-3b
        // Run PolicyEnforcer.access_scope() → AccessScope
        let scope = self
            .enforcer
            .access_scope(ctx, &RG_GROUP_RESOURCE, "list", None)
            .await
            .map_err(DomainError::from)?;
        // @cpt-end:cpt-cf-resource-group-algo-integration-auth-auth-mode-decision:p1:inst-auth-decide-3b
        // @cpt-begin:cpt-cf-resource-group-algo-integration-auth-auth-mode-decision:p1:inst-auth-decide-3c
        // RETURN JWT mode with SecurityContext + AccessScope (the AccessScope
        // is propagated to the data layer below).
        let conn = self.db.conn()?;
        self.group_repo.list_groups(&conn, &scope, query).await
        // @cpt-end:cpt-cf-resource-group-algo-integration-auth-auth-mode-decision:p1:inst-auth-decide-3c
        // @cpt-end:cpt-cf-resource-group-algo-integration-auth-auth-mode-decision:p1:inst-auth-decide-3
        // @cpt-begin:cpt-cf-resource-group-algo-integration-auth-auth-mode-decision:p1:inst-auth-decide-4
        // ELSE → RETURN 401 Unauthorized (handled upstream by the API Gateway
        // before SecurityContext is constructed; an absent/invalid JWT never
        // reaches this service path).
        // @cpt-end:cpt-cf-resource-group-algo-integration-auth-auth-mode-decision:p1:inst-auth-decide-4
    }

    // @cpt-flow:cpt-cf-resource-group-flow-entity-hier-update-group:p1
    /// Update a resource group (full replacement via PUT, AuthZ-scoped).
    ///
    /// Runs inside one transaction with bounded retry (max 3 attempts) so the
    /// invariant checks, the closure-table mutations and the update itself are
    /// atomic. The isolation level is chosen per call, not fixed: a rename or a
    /// metadata edit touches one row by primary key and runs at the backend
    /// default, while a parent change needs `SERIALIZABLE` for the cycle,
    /// depth and width predicates it decides from. See the comment on the
    /// isolation hint below -- when the hint turns out to be wrong, the whole
    /// operation reruns at `SERIALIZABLE`.
    pub async fn update_group(
        &self,
        ctx: &SecurityContext,
        group_id: Uuid,
        req: UpdateGroupRequest,
    ) -> Result<ResourceGroup, DomainError> {
        let started = std::time::Instant::now();
        // @cpt-begin:cpt-cf-resource-group-flow-entity-hier-update-group:p1:inst-update-group-1
        // Actor sends PUT /api/resource-group/v1/groups/{group_id}
        // AuthZ gate: verify the caller can update this group (tenant check).
        let scope = self
            .enforcer
            .access_scope(ctx, &RG_GROUP_RESOURCE, "update", Some(group_id))
            .await
            .map_err(DomainError::from)?;

        // Pre-validation (stateless, outside transaction).
        // Type is immutable on update — `UpdateGroupRequest` deliberately
        // does not carry a `code` field — so there is nothing to validate
        // syntactically here besides the display name.
        Self::validate_name(&req.name)?;
        // @cpt-end:cpt-cf-resource-group-flow-entity-hier-update-group:p1:inst-update-group-1

        let profile = self.profile.clone();
        let db = self.db.db();
        let group_repo = self.group_repo.clone();
        let type_repo = self.type_repo.clone();

        // One pre-transaction read, serving two purposes.
        //
        // Metadata validation goes over the network and compiles a schema, so
        // it must not run with the transaction open -- see `create_group`.
        // Unlike create, the type is not in the request; it has to be read,
        // and it cannot go stale, because a group's type is immutable (which
        // is why `UpdateGroupRequest` has no `code` field).
        //
        // The same row also carries the current `parent_id`, which decides
        // the transaction's isolation level below. `conn` is scoped to this
        // block so the pooled connection is back before the transaction asks
        // for its own.
        let existing = {
            let conn = db
                .conn()
                .map_err(|e| DomainError::database(e.to_string()))?;
            // Scoped read first, before anything whose *shape* the caller can
            // observe. `find_model_by_id` builds `system_scope()`, so on its
            // own it answers for a group in any tenant -- and the metadata
            // validation right below returns a schema-shaped 400, which a
            // cross-tenant id must not be able to tell apart from the 404 an
            // unknown id gets. Same rule the delete path states: a foreign id
            // and a non-existent one look identical from outside.
            group_repo
                .find_by_id(&conn, &scope, group_id)
                .await?
                .ok_or_else(|| DomainError::group_not_found(group_id))?;
            let existing = group_repo
                .find_model_by_id(&conn, group_id)
                .await?
                .ok_or_else(|| DomainError::group_not_found(group_id))?;
            if req.metadata.is_some() {
                let type_path =
                    Self::resolve_type_path_from_id(&conn, existing.gts_type_id).await?;
                let validation_started = std::time::Instant::now();
                let validated = validation::validate_metadata_via_gts(
                    req.metadata.as_ref(),
                    &type_path,
                    &*self.types_registry,
                )
                .await;
                self.metrics.metadata_validation_duration(
                    Operation::Update,
                    validation_started.elapsed().as_secs_f64(),
                );
                validated?;
            }
            existing
        };

        // Take the least-strict level that stays correct for this update.
        // Write-skew here -- cycle detection racing a concurrent create or
        // move, and the closure rebuild -- is reachable only on the
        // parent-change branch. A rename or metadata edit touches one row by
        // primary key and has no cross-row predicate to protect, so the
        // backend default is enough. On SQLite that changes nothing, since it
        // is serializable regardless; this is a PostgreSQL saving.
        //
        // It is a *hint*: computed before the transaction opens, so a
        // concurrent request can change this group's parent in the gap. The
        // authoritative answer is always `update_group_inner`'s own fresh
        // read. If the hint was wrong in the dangerous direction -- it said
        // "no move", the fresh read says otherwise -- the inner function
        // returns `NeedsSerializable` before writing anything and the whole
        // operation reruns at `TxConfig::serializable()`. Wrong in the other
        // direction is harmless: SERIALIZABLE is a safe superset, so a hint
        // that overshoots costs a little and protects the same invariants.
        let guessed_parent_changed = existing.parent_id != req.parent_id;
        let hinted_isolation = if guessed_parent_changed {
            WriteIsolation::Serializable
        } else {
            WriteIsolation::RowLocked
        };

        let first = Self::attempt_update_group(
            &db,
            &group_repo,
            &type_repo,
            &scope,
            group_id,
            &req,
            &profile,
            hinted_isolation,
        )
        .await?;

        let result = match first {
            UpdateGroupOutcome::Done(group) => Ok(group),
            UpdateGroupOutcome::NeedsSerializable => {
                self.metrics.isolation_escalation(Operation::Update);
                match Self::attempt_update_group(
                    &db,
                    &group_repo,
                    &type_repo,
                    &scope,
                    group_id,
                    &req,
                    &profile,
                    WriteIsolation::Serializable,
                )
                .await?
                {
                    UpdateGroupOutcome::Done(group) => Ok(group),
                    // Unreachable: this attempt ran at SERIALIZABLE, which is
                    // the level `NeedsSerializable` asks for. Reported rather
                    // than looped, so a future change that breaks the
                    // invariant surfaces instead of spinning.
                    UpdateGroupOutcome::NeedsSerializable => Err(DomainError::database(
                        "update_group still reported NeedsSerializable after escalating to \
                         SERIALIZABLE",
                    )),
                }
            }
        };
        self.record_op(Operation::Update, started, &result);
        result
    }

    /// Run one `update_group` attempt at the isolation level `isolation`
    /// selects, with the usual bounded retry inside it.
    #[allow(clippy::too_many_arguments)]
    async fn attempt_update_group(
        db: &Db,
        group_repo: &Arc<GR>,
        type_repo: &Arc<TR>,
        scope: &toolkit_security::AccessScope,
        group_id: Uuid,
        req: &UpdateGroupRequest,
        profile: &QueryProfile,
        isolation: WriteIsolation,
    ) -> Result<UpdateGroupOutcome, DomainError> {
        db.transaction_with_retry(isolation.tx_config(), DomainError::db_err, |tx| {
            let req = req.clone();
            let scope = scope.clone();
            let profile = profile.clone();
            let group_repo = group_repo.clone();
            let type_repo = type_repo.clone();
            Box::pin(async move {
                Self::update_group_inner(
                    &*group_repo,
                    &*type_repo,
                    tx,
                    &scope,
                    group_id,
                    &req,
                    &profile,
                    isolation,
                )
                .await
            })
        })
        .await
    }

    // @cpt-flow:cpt-cf-resource-group-flow-entity-hier-move-group:p1
    /// Move a group to a new parent (or make it a root).
    ///
    /// Runs inside a `SERIALIZABLE` transaction with bounded retry (max 3 attempts)
    /// to ensure cycle detection, invariant checks, and closure table rebuild are atomic.
    pub async fn move_group(
        &self,
        group_id: Uuid,
        new_parent_id: Option<Uuid>,
    ) -> Result<ResourceGroup, DomainError> {
        let started = std::time::Instant::now();
        // @cpt-begin:cpt-cf-resource-group-flow-entity-hier-move-group:p1:inst-move-group-1
        // Actor sends PUT /api/resource-group/v1/groups/{group_id} with new hierarchy.parent_id
        // @cpt-end:cpt-cf-resource-group-flow-entity-hier-move-group:p1:inst-move-group-1
        let profile = self.profile.clone();
        let db = self.db.db();
        let group_repo = self.group_repo.clone();
        let type_repo = self.type_repo.clone();

        // @cpt-begin:cpt-cf-resource-group-flow-entity-hier-move-group:p1:inst-move-group-2
        // @cpt-begin:cpt-cf-resource-group-flow-entity-hier-move-group:p1:inst-move-group-12
        // @cpt-begin:cpt-cf-resource-group-flow-entity-hier-move-group:p1:inst-move-group-11
        // @cpt-begin:cpt-cf-resource-group-flow-entity-hier-move-group:p1:inst-move-group-13
        let result = db
            .transaction_with_retry(TxConfig::serializable(), DomainError::db_err, |tx| {
                let profile = profile.clone();
                let group_repo = group_repo.clone();
                let type_repo = type_repo.clone();
                Box::pin(async move {
                    Self::move_group_inner(
                        &*group_repo,
                        &*type_repo,
                        tx,
                        group_id,
                        new_parent_id,
                        &profile,
                    )
                    .await
                })
            })
            .await;
        self.record_op(Operation::Move, started, &result);
        // Record closure-row metrics outside the retry closure so retries
        // do not double-count (CodeRabbit).
        if let Ok((_, closure_rows)) = &result {
            self.metrics
                .closure_rows_written(Operation::Move, *closure_rows);
        }
        result.map(|(group, _)| group)
        // @cpt-end:cpt-cf-resource-group-flow-entity-hier-move-group:p1:inst-move-group-13
        // @cpt-end:cpt-cf-resource-group-flow-entity-hier-move-group:p1:inst-move-group-11
        // @cpt-end:cpt-cf-resource-group-flow-entity-hier-move-group:p1:inst-move-group-12
        // @cpt-end:cpt-cf-resource-group-flow-entity-hier-move-group:p1:inst-move-group-2
    }

    // @cpt-flow:cpt-cf-resource-group-flow-entity-hier-delete-group:p1
    /// Delete a resource group (AuthZ-scoped).
    ///
    /// Runs inside one transaction with bounded retry (max 3 attempts) so the
    /// reference checks and the deletes are atomic. The isolation level
    /// depends on `force`: a force delete rewrites closure rows across a whole
    /// subtree and keeps `SERIALIZABLE`, while a non-force delete removes one
    /// leaf, takes a row lock on it, and runs at the backend default. The
    /// comment inside spells out why the row lock is enough there.
    pub async fn delete_group(
        &self,
        ctx: &SecurityContext,
        group_id: Uuid,
        force: bool,
    ) -> Result<(), DomainError> {
        let started = std::time::Instant::now();
        // @cpt-begin:cpt-cf-resource-group-flow-entity-hier-delete-group:p1:inst-delete-group-1
        // Actor sends DELETE /api/resource-group/v1/groups/{group_id}?force={true|false}
        // AuthZ gate: verify the caller can delete this group (tenant check).
        // Runs outside the transaction since AuthZ is idempotent.
        let scope = self
            .enforcer
            .access_scope(ctx, &RG_GROUP_RESOURCE, "delete", Some(group_id))
            .await
            .map_err(DomainError::from)?;
        // @cpt-end:cpt-cf-resource-group-flow-entity-hier-delete-group:p1:inst-delete-group-1

        let db = self.db.db();
        let group_repo = self.group_repo.clone();
        // A force delete rewrites a whole subtree -- closure rows for every
        // node, memberships, the group rows themselves -- and races a
        // concurrent create or move anywhere inside it. That is write skew,
        // and it keeps SERIALIZABLE.
        //
        // A non-force delete removes one leaf, and refuses if it has children
        // or memberships. The only thing it needs is for that refusal to
        // still be true when the delete runs. No cross-row predicate, so
        // nothing for SERIALIZABLE to protect.
        //
        // What actually serializes it against a concurrent `create_group`
        // under the same parent is not this row lock on its own -- a plain
        // `SELECT` does not wait on `FOR UPDATE`, so the create reads the
        // parent and proceeds. It is the foreign key: inserting a child takes
        // `FOR KEY SHARE` on the parent row, and that conflicts with the
        // `FOR UPDATE` taken here. Whichever arrives first, the other waits.
        // If the delete won, the create fails on `ON DELETE RESTRICT`; if the
        // create won, the delete re-reads and sees the child. The lock is what
        // makes the ordering decidable rather than what does the blocking, and
        // the same holds for memberships and closure rows -- every one of
        // those foreign keys is `RESTRICT`.
        //
        // Unlike `update_group`, no hint is involved: `force` is a request
        // field, known before the transaction opens.
        let config = if force {
            TxConfig::serializable()
        } else {
            TxConfig::default()
        };

        let result = db
            .transaction_with_retry(config, DomainError::db_err, |tx| {
                let scope = scope.clone();
                let group_repo = group_repo.clone();
                Box::pin(async move {
                    Self::delete_group_inner(&*group_repo, tx, &scope, group_id, force).await
                })
            })
            .await;
        // Record subtree-node metric outside the retry closure (CodeRabbit).
        if let Ok(Some(subtree_count)) = &result {
            self.metrics
                .subtree_nodes(Operation::ForceDelete, *subtree_count);
        }
        self.record_op(
            if force {
                Operation::ForceDelete
            } else {
                Operation::Delete
            },
            started,
            &result,
        );
        result.map(|_| ())
    }

    /// Get descendants of a group (depth >= 0, AuthZ-scoped).
    pub async fn get_group_descendants(
        &self,
        ctx: &SecurityContext,
        group_id: Uuid,
        query: &ODataQuery,
    ) -> Result<Page<ResourceGroupWithDepth>, DomainError> {
        let scope = self
            .enforcer
            .access_scope(ctx, &RG_GROUP_RESOURCE, "list", Some(group_id))
            .await
            .map_err(DomainError::from)?;
        let conn = self.db.conn()?;
        // Scope-aware preflight: a cross-tenant id must look the same as a
        // non-existent id from the caller's viewpoint, otherwise we leak the
        // existence of cross-tenant roots (random id → 404, foreign id → 200
        // with empty page).
        self.group_repo
            .find_by_id(&conn, &scope, group_id)
            .await?
            .ok_or_else(|| DomainError::group_not_found(group_id))?;
        self.group_repo
            .get_descendants(&conn, &scope, group_id, query)
            .await
    }

    /// Get ancestors of a group (depth <= 0, AuthZ-scoped).
    pub async fn get_group_ancestors(
        &self,
        ctx: &SecurityContext,
        group_id: Uuid,
        query: &ODataQuery,
    ) -> Result<Page<ResourceGroupWithDepth>, DomainError> {
        let scope = self
            .enforcer
            .access_scope(ctx, &RG_GROUP_RESOURCE, "list", Some(group_id))
            .await
            .map_err(DomainError::from)?;
        let conn = self.db.conn()?;
        // Scope-aware preflight: see comment in `get_group_descendants`.
        self.group_repo
            .find_by_id(&conn, &scope, group_id)
            .await?
            .ok_or_else(|| DomainError::group_not_found(group_id))?;
        self.group_repo
            .get_ancestors(&conn, &scope, group_id, query)
            .await
    }

    // -- Unscoped reads (for integration read service, bypasses AuthZ) --
    //
    // These methods are exposed via `ResourceGroupReadHierarchy` trait
    // (registered in ClientHub as `dyn ResourceGroupReadHierarchy`).
    // They use `AccessScope::allow_all()` — no tenant WHERE clause.
    //
    // This is by design (DESIGN §3.6): the AuthZ plugin is the primary
    // consumer of these reads. It cannot evaluate itself (circular dep),
    // so the in-process ClientHub path skips AuthZ entirely.
    //
    // SECURITY: do NOT expose these methods via REST handlers.
    // REST uses the scoped variants (`get_group_descendants` / `get_group_ancestors`).

    /// Get descendants without `AuthZ` enforcement (private API, no tenant scoping).
    pub async fn get_group_descendants_unscoped(
        &self,
        group_id: Uuid,
        query: &ODataQuery,
    ) -> Result<Page<ResourceGroupWithDepth>, DomainError> {
        let conn = self.db.conn()?;
        let scope = toolkit_security::AccessScope::allow_all();
        self.group_repo
            .get_descendants(&conn, &scope, group_id, query)
            .await
    }

    /// Get ancestors without `AuthZ` enforcement (private API, no tenant scoping).
    ///
    /// Used by `ResourceGroupReadHierarchy` consumers (e.g., tenant-resolver plugin)
    /// that need full ancestor visibility regardless of the caller's tenant scope.
    pub async fn get_group_ancestors_unscoped(
        &self,
        group_id: Uuid,
        query: &ODataQuery,
    ) -> Result<Page<ResourceGroupWithDepth>, DomainError> {
        let conn = self.db.conn()?;
        let scope = toolkit_security::AccessScope::allow_all();
        self.group_repo
            .get_ancestors(&conn, &scope, group_id, query)
            .await
    }

    /// List groups without `AuthZ` enforcement (private API, no tenant scoping).
    ///
    /// Used by `ResourceGroupReadHierarchy::list_groups` consumers (e.g.,
    /// the tenant-resolver RG plugin's batch `get_tenants` path) which need
    /// to resolve groups by id/type predicates regardless of the caller's
    /// tenant scope. Mirrors the pattern of `get_group_*_unscoped`.
    pub async fn list_groups_unscoped(
        &self,
        query: &ODataQuery,
    ) -> Result<Page<ResourceGroup>, DomainError> {
        let conn = self.db.conn()?;
        let scope = toolkit_security::AccessScope::allow_all();
        self.group_repo.list_groups(&conn, &scope, query).await
    }

    /// Get a single group by id without `AuthZ` enforcement.
    ///
    /// **Internal API** — never expose this through a REST handler. Used by
    /// the seeding path (which runs at gear init, before any caller
    /// security context exists) to check whether a seeded group is already
    /// present. Mirrors the pattern of the other `*_unscoped` methods.
    pub async fn get_group_unscoped(&self, group_id: Uuid) -> Result<ResourceGroup, DomainError> {
        let conn = self.db.conn()?;
        let scope = toolkit_security::AccessScope::allow_all();
        self.group_repo
            .find_by_id(&conn, &scope, group_id)
            .await?
            .ok_or_else(|| DomainError::group_not_found(group_id))
    }

    /// Create a group without `AuthZ` enforcement.
    ///
    /// **Internal API** — never expose this through a REST handler. Used by
    /// the seeding path to provision required groups at gear init, before
    /// any caller security context exists. Domain invariants (type
    /// validation, parent compatibility, tenant scoping, closure table
    /// maintenance) still run because this method calls the same
    /// `create_group_inner` as the public path; only the `PolicyEnforcer`
    /// gate is skipped.
    pub async fn create_group_unscoped(
        &self,
        req: CreateGroupRequest,
        tenant_id: Uuid,
    ) -> Result<ResourceGroup, DomainError> {
        validation::validate_type_code(&req.code)?;
        Self::validate_name(&req.name)?;

        let is_tenant = req.code.starts_with(TENANT_RG_TYPE_PATH);
        Self::reject_tenant_id_on_tenant_type(is_tenant, req.tenant_id)?;

        if let Some(req_tenant_id) = req.tenant_id
            && req_tenant_id != tenant_id
        {
            return Err(DomainError::validation(format!(
                "create_group_unscoped: req.tenant_id ({req_tenant_id}) disagrees with the \
                 trusted tenant_id argument ({tenant_id}); this indicates a caller bug, not a \
                 policy decision to make silently"
            )));
        }

        // Before `BEGIN`, for the reason spelled out in `create_group`.
        validation::validate_metadata_via_gts(
            req.metadata.as_ref(),
            &req.code,
            &*self.types_registry,
        )
        .await?;

        let profile = self.profile.clone();
        let db = self.db.db();
        let group_repo = self.group_repo.clone();
        let type_repo = self.type_repo.clone();

        db.transaction_with_retry(TxConfig::serializable(), DomainError::db_err, |tx| {
            let req = req.clone();
            let profile = profile.clone();
            let group_repo = group_repo.clone();
            let type_repo = type_repo.clone();
            Box::pin(async move {
                Self::create_group_inner(&*group_repo, &*type_repo, tx, &req, tenant_id, &profile)
                    .await
            })
        })
        .await
    }

    // -- Transaction-inner implementations --

    /// Inner logic for `create_group`, runs inside a SERIALIZABLE transaction.
    ///
    /// Takes no `TypesRegistryClient`, and that is the point: metadata
    /// validation resolves a schema over the network and compiles it, which
    /// must not happen with a transaction open. Both callers do it before
    /// `BEGIN`. Without the parameter the call cannot drift back in here.
    #[allow(clippy::cognitive_complexity)]
    async fn create_group_inner(
        group_repo: &GR,
        type_repo: &TR,
        tx: &impl DBRunner,
        req: &CreateGroupRequest,
        tenant_id: Uuid,
        profile: &QueryProfile,
    ) -> Result<ResourceGroup, DomainError> {
        // @cpt-begin:cpt-cf-resource-group-flow-entity-hier-create-group:p1:inst-create-group-3
        // One lookup for both the surrogate id and the type itself. Asking
        // for them separately cost two `gts_type` SELECTs per create for the
        // same row (RG-11).
        let (type_model, rg_type) = type_repo
            .find_by_code_with_model(tx, &req.code)
            .await?
            .ok_or_else(|| DomainError::type_not_found(&req.code))?;
        let type_id = type_model.id;
        // @cpt-end:cpt-cf-resource-group-flow-entity-hier-create-group:p1:inst-create-group-3

        // Determine effective tenant_id by code-prefix rule:
        // - code starts with TENANT_RG_TYPE_PATH → tenant_id = group.id (new scope)
        // - otherwise                           → tenant_id from caller / parent
        let group_id = req.id.unwrap_or_else(Uuid::now_v7);
        let is_tenant_type = req.code.starts_with(TENANT_RG_TYPE_PATH);
        let effective_tenant_id = if is_tenant_type { group_id } else { tenant_id };

        // @cpt-begin:cpt-cf-resource-group-flow-entity-hier-create-group:p1:inst-create-group-4
        if let Some(parent_id) = req.parent_id {
            // @cpt-begin:cpt-cf-resource-group-flow-entity-hier-create-group:p1:inst-create-group-4a
            // @cpt-begin:cpt-cf-resource-group-flow-entity-hier-create-group:p1:inst-create-group-4b
            let parent = group_repo
                .find_model_by_id(tx, parent_id)
                .await?
                .ok_or_else(|| DomainError::group_not_found(parent_id))?;
            // @cpt-end:cpt-cf-resource-group-flow-entity-hier-create-group:p1:inst-create-group-4b
            // @cpt-end:cpt-cf-resource-group-flow-entity-hier-create-group:p1:inst-create-group-4a

            // @cpt-begin:cpt-cf-resource-group-flow-entity-hier-create-group:p1:inst-create-group-4c
            // @cpt-begin:cpt-cf-resource-group-flow-entity-hier-create-group:p1:inst-create-group-4d
            let parent_type_path = Self::resolve_type_path_from_id(tx, parent.gts_type_id).await?;
            if !rg_type.allowed_parent_types.contains(&parent_type_path) {
                return Err(DomainError::invalid_parent_type(format!(
                    "Type '{}' does not allow parent type '{}'",
                    req.code, parent_type_path
                )));
            }
            // @cpt-end:cpt-cf-resource-group-flow-entity-hier-create-group:p1:inst-create-group-4d
            // @cpt-end:cpt-cf-resource-group-flow-entity-hier-create-group:p1:inst-create-group-4c

            // @cpt-algo:cpt-cf-resource-group-algo-integration-auth-tenant-scope-enforcement:p1
            // @cpt-begin:cpt-cf-resource-group-algo-integration-auth-tenant-scope-enforcement:p1:inst-tenant-enforce-1
            // Extract caller effective tenant scope from SecurityContext.subject_tenant_id
            // (tenant_id is passed as parameter from caller's context)
            // @cpt-end:cpt-cf-resource-group-algo-integration-auth-tenant-scope-enforcement:p1:inst-tenant-enforce-1
            // @cpt-begin:cpt-cf-resource-group-algo-integration-auth-tenant-scope-enforcement:p1:inst-tenant-enforce-2
            // IF caller is privileged platform-admin -> pass (but data invariants still checked)
            // (platform-admin bypass handled by middleware; data invariants enforced below)
            // @cpt-end:cpt-cf-resource-group-algo-integration-auth-tenant-scope-enforcement:p1:inst-tenant-enforce-2
            // @cpt-begin:cpt-cf-resource-group-algo-integration-auth-tenant-scope-enforcement:p1:inst-tenant-enforce-3
            // Validate tenant compatibility (child must be same tenant as parent)
            // @cpt-begin:cpt-cf-resource-group-algo-integration-auth-tenant-scope-enforcement:p1:inst-tenant-enforce-4
            // IF membership write: validate target group's tenant_id is compatible
            // @cpt-end:cpt-cf-resource-group-algo-integration-auth-tenant-scope-enforcement:p1:inst-tenant-enforce-4
            // @cpt-begin:cpt-cf-resource-group-algo-integration-auth-tenant-scope-enforcement:p1:inst-tenant-enforce-5
            // Skip tenant enforcement for tenant-typed groups — they intentionally
            // create a new tenant scope (tenant_id = group.id != parent.tenant_id).
            if !is_tenant_type && parent.tenant_id != tenant_id {
                return Err(DomainError::validation(
                    "Child group tenant must match parent tenant -- cannot create a child \
                     group in a tenant different from its parent group"
                        .to_owned(),
                ));
            }
            // @cpt-end:cpt-cf-resource-group-algo-integration-auth-tenant-scope-enforcement:p1:inst-tenant-enforce-5
            // @cpt-end:cpt-cf-resource-group-algo-integration-auth-tenant-scope-enforcement:p1:inst-tenant-enforce-3
            // @cpt-begin:cpt-cf-resource-group-algo-integration-auth-tenant-scope-enforcement:p1:inst-tenant-enforce-6
            // RETURN pass (tenant enforcement passed)
            // @cpt-end:cpt-cf-resource-group-algo-integration-auth-tenant-scope-enforcement:p1:inst-tenant-enforce-6

            // @cpt-begin:cpt-cf-resource-group-flow-entity-hier-create-group:p1:inst-create-group-4e
            // Check query profile: depth limit
            if let Some(max_depth) = profile.max_depth {
                let parent_depth = group_repo.get_depth(tx, parent_id).await?;
                #[allow(clippy::cast_possible_wrap)]
                if parent_depth + 1 >= max_depth as i32 {
                    return Err(DomainError::limit_violation(format!(
                        "Depth limit exceeded: adding child at depth {} exceeds max_depth {}",
                        parent_depth + 1,
                        max_depth
                    )));
                }
            }
            // @cpt-end:cpt-cf-resource-group-flow-entity-hier-create-group:p1:inst-create-group-4e

            // @cpt-begin:cpt-cf-resource-group-flow-entity-hier-create-group:p1:inst-create-group-4f
            // Check query profile: width limit
            if let Some(max_width) = profile.max_width {
                let sibling_count = group_repo.count_children(tx, parent_id).await?;
                if sibling_count >= u64::from(max_width) {
                    return Err(DomainError::limit_violation(format!(
                        "Width limit exceeded: parent already has {sibling_count} children, max_width is {max_width}"
                    )));
                }
            }
            // @cpt-end:cpt-cf-resource-group-flow-entity-hier-create-group:p1:inst-create-group-4f
            // @cpt-end:cpt-cf-resource-group-flow-entity-hier-create-group:p1:inst-create-group-4

            // @cpt-begin:cpt-cf-resource-group-flow-entity-hier-create-group:p1:inst-create-group-6
            // Insert group
            let model = group_repo
                .insert(
                    tx,
                    group_id,
                    Some(parent_id),
                    type_id,
                    &req.name,
                    req.metadata.as_ref(),
                    effective_tenant_id,
                )
                .await?;
            // @cpt-end:cpt-cf-resource-group-flow-entity-hier-create-group:p1:inst-create-group-6

            // @cpt-begin:cpt-cf-resource-group-flow-entity-hier-create-group:p1:inst-create-group-7
            // Insert closure: self-row
            group_repo.insert_closure_self_row(tx, group_id).await?;
            // @cpt-end:cpt-cf-resource-group-flow-entity-hier-create-group:p1:inst-create-group-7

            // @cpt-begin:cpt-cf-resource-group-flow-entity-hier-create-group:p1:inst-create-group-8
            // @cpt-begin:cpt-cf-resource-group-flow-entity-hier-create-group:p1:inst-create-group-8a
            // Insert ancestor closure rows from parent's ancestors with depth+1
            group_repo
                .insert_ancestor_closure_rows(tx, group_id, parent_id)
                .await?;
            // @cpt-end:cpt-cf-resource-group-flow-entity-hier-create-group:p1:inst-create-group-8a
            // @cpt-end:cpt-cf-resource-group-flow-entity-hier-create-group:p1:inst-create-group-8

            // The row the insert returned, not a re-read of it. Its type path
            // is the code this request named -- `type_id` was resolved from
            // it above -- so nothing here needs the database again (RG-08).
            Ok(ResourceGroup {
                id: model.id,
                code: req.code.clone(),
                name: model.name,
                hierarchy: GroupHierarchy {
                    parent_id: model.parent_id,
                    tenant_id: model.tenant_id,
                },
                metadata: model.metadata,
            })
        } else {
            // @cpt-begin:cpt-cf-resource-group-flow-entity-hier-create-group:p1:inst-create-group-5
            // @cpt-begin:cpt-cf-resource-group-flow-entity-hier-create-group:p1:inst-create-group-5a
            // Root group: validate can_be_root
            if !rg_type.can_be_root {
                return Err(DomainError::invalid_parent_type(format!(
                    "Type '{}' cannot be a root group (can_be_root=false)",
                    req.code
                )));
            }
            // @cpt-end:cpt-cf-resource-group-flow-entity-hier-create-group:p1:inst-create-group-5a

            // @cpt-begin:cpt-cf-resource-group-flow-entity-hier-create-group:p1:inst-create-group-5c
            // Tenant-root uniqueness: at most one tenant-type group may be a
            // forest root. `cpt-cf-resource-group-fr-enforce-tenant-root-uniqueness`.
            if is_tenant_type
                && let Some(existing_root_id) = group_repo
                    .find_root_id_with_type_prefix(tx, TENANT_RG_TYPE_PATH)
                    .await?
            {
                return Err(DomainError::tenant_root_already_exists(
                    existing_root_id,
                    format!(
                        "Cannot create tenant-type root '{}' ({}): tenant root already exists",
                        req.name, req.code
                    ),
                ));
            }
            // @cpt-end:cpt-cf-resource-group-flow-entity-hier-create-group:p1:inst-create-group-5c
            // @cpt-end:cpt-cf-resource-group-flow-entity-hier-create-group:p1:inst-create-group-5

            // Insert group
            let model = group_repo
                .insert(
                    tx,
                    group_id,
                    None,
                    type_id,
                    &req.name,
                    req.metadata.as_ref(),
                    effective_tenant_id,
                )
                .await?;

            // Insert closure: self-row only
            group_repo.insert_closure_self_row(tx, group_id).await?;

            // As in the child branch: the inserted row, not a read of it.
            Ok(ResourceGroup {
                id: model.id,
                code: req.code.clone(),
                name: model.name,
                hierarchy: GroupHierarchy {
                    parent_id: model.parent_id,
                    tenant_id: model.tenant_id,
                },
                metadata: model.metadata,
            })
        }
    }

    /// Inner logic for `update_group`, runs inside the transaction its caller
    /// opened -- at `SERIALIZABLE` or at the backend default, per the
    /// `isolation` argument. When that argument is `WriteIsolation::RowLocked`
    /// and the authoritative read finds the parent changing after all, this
    /// returns `UpdateGroupOutcome::NeedsSerializable` without writing
    /// anything, and the caller reruns the operation at
    /// `WriteIsolation::Serializable`.
    ///
    /// **Type immutability.** A group's GTS type is fixed at creation —
    /// `UpdateGroupRequest` does not carry a `code` field. The existing
    /// `gts_type_id` is reused unchanged for the persisted update, so all
    /// type-driven validation (allowed parents/children, tenant-root rule,
    /// metadata schema lookup) is anchored on the existing type, not on a
    /// caller-supplied one.
    ///
    /// **Tenant immutability.** A group's `tenant_id` is also fixed at
    /// creation. Reparenting is therefore allowed only **within the same
    /// tenant** — the new parent's `tenant_id` must equal the group's
    /// `existing.tenant_id`, otherwise the move is rejected with the same
    /// rule `create_group_inner` uses for non-tenant children. Tenant-type
    /// roots already have `tenant_id = group_id`, so the same equality check
    /// trivially holds for them as well.
    #[allow(clippy::too_many_arguments)]
    async fn update_group_inner(
        group_repo: &GR,
        type_repo: &TR,
        tx: &impl DBRunner,
        scope: &toolkit_security::AccessScope,
        group_id: Uuid,
        req: &UpdateGroupRequest,
        profile: &QueryProfile,
        isolation: WriteIsolation,
    ) -> Result<UpdateGroupOutcome, DomainError> {
        // @cpt-begin:cpt-cf-resource-group-flow-entity-hier-update-group:p1:inst-update-group-2
        // DB: SELECT FROM resource_group WHERE id = {group_id} -- load existing group
        group_repo
            .find_by_id(tx, scope, group_id)
            .await?
            .ok_or_else(|| DomainError::group_not_found(group_id))?;

        // Locked, not a plain read. This attempt may be running at the
        // backend default (the rename/metadata path), where the UPDATE below
        // matches on `id` alone and always assigns `parent_id` -- so a
        // `SERIALIZABLE` reparent committing between this read and that
        // write would have its `parent_id` silently reverted while the
        // closure table kept the ancestry of the move. Nothing detects that:
        // an UPDATE by primary key under READ COMMITTED never raises
        // `40001`, and SSI pairs only transactions that are all
        // `SERIALIZABLE`. `FOR UPDATE` waits for that in-flight move and
        // returns the parent it committed, so `parent_changed` below sees it
        // and escalates before anything is written.
        let existing = group_repo
            .find_model_by_id_for_update(tx, group_id)
            .await?
            .ok_or_else(|| DomainError::group_not_found(group_id))?;
        // @cpt-end:cpt-cf-resource-group-flow-entity-hier-update-group:p1:inst-update-group-2

        // @cpt-begin:cpt-cf-resource-group-flow-entity-hier-update-group:p1:inst-update-group-3
        // IF group not found -> RETURN NotFound (handled by ok_or_else above)
        // @cpt-end:cpt-cf-resource-group-flow-entity-hier-update-group:p1:inst-update-group-3

        // @cpt-begin:cpt-cf-resource-group-flow-entity-hier-update-group:p1:inst-update-group-4
        // IF type is changed — `UpdateGroupRequest` deliberately does not carry
        // a `code` field, so `gts_type_id` is reused unchanged below. The
        // structural-change validation that would run on a type change is
        // therefore enforced via the parent-change branch (move semantics)
        // and the closure-table compatibility checks performed by
        // `move_group_internal_impl`. The 4a/4b/4c/4d sub-steps are realized
        // by that helper and the metadata validation block right below.
        // Type is immutable on update — reuse the existing `gts_type_id`.
        //
        // Only the path is resolved unconditionally here: the response's
        // `code` field needs it (see the final `Ok` below) regardless of
        // whether the parent changes, and it is a single row read by primary
        // key. The full type -- `rg_type`, which `find_by_code` builds by
        // also reading `gts_type_allowed_parent` and
        // `gts_type_allowed_membership` -- is loaded further down, inside
        // the `parent_changed` branch (see `inst-update-group-4a` there): it
        // feeds nothing but `move_group_internal_impl`'s parent-compatibility
        // check, so a plain rename or metadata edit has no use for it and no
        // longer pays for those two junction-table reads.
        let existing_type_path = Self::resolve_type_path_from_id(tx, existing.gts_type_id).await?;

        // @cpt-begin:cpt-cf-resource-group-flow-entity-hier-update-group:p1:inst-update-group-4e
        // Already validated against this same type before `BEGIN` -- the type
        // is immutable, so the path resolved here and the one resolved there
        // are the same string. See the caller for why it does not run under
        // an open transaction.
        // @cpt-end:cpt-cf-resource-group-flow-entity-hier-update-group:p1:inst-update-group-4e

        // Cross-tenant parent change is forbidden. `tenant_id` is established
        // at creation and never rewritten — see the function-level doc above
        // for the invariant. Mirror `create_group_inner`'s tenant-scope
        // enforcement for non-tenant children. (Tenant-type roots have
        // `tenant_id == group_id` by construction; reparenting one under a
        // different parent is also rejected here because the equality check
        // would fail.)
        //
        // Also the one read of the new parent this whole update makes: kept
        // as a `ParentSnapshot` and handed to `move_group_internal_impl`
        // below instead of letting it read the same row again.
        let new_parent_snapshot = if let Some(new_parent_id) = req.parent_id
            && new_parent_id != existing.parent_id.unwrap_or_default()
        {
            let new_parent = group_repo
                .find_model_by_id(tx, new_parent_id)
                .await?
                .ok_or_else(|| DomainError::group_not_found(new_parent_id))?;
            if new_parent.tenant_id != existing.tenant_id {
                // Generic message: do not interpolate tenant ids — the caller
                // can't act on them legitimately, and disclosing the foreign
                // tenant_id would leak ownership of `new_parent_id` across the
                // tenant boundary.
                return Err(DomainError::validation(format!(
                    "Cannot move group {group_id} to a parent in a different tenant; \
                     cross-tenant moves are not supported"
                )));
            }
            Some(ParentSnapshot {
                tenant_id: new_parent.tenant_id,
                gts_type_id: new_parent.gts_type_id,
            })
        } else {
            None
        };

        // @cpt-begin:cpt-cf-resource-group-flow-entity-hier-update-group:p1:inst-update-group-4b
        // DB: SELECT gts_type_id FROM resource_group WHERE parent_id = {group_id}
        // — load children types (performed inside `move_group_internal_impl`'s
        // closure-table queries when a parent change occurs; type itself is
        // immutable here so a type-driven children rescan is unnecessary).
        // @cpt-begin:cpt-cf-resource-group-flow-entity-hier-update-group:p1:inst-update-group-4c
        // FOR EACH child: verify child's type includes new type in
        // allowed_parents (no-op for immutable-type updates; the move helper
        // runs the equivalent allowed_parent_types check against the new
        // parent on a parent change).
        // @cpt-begin:cpt-cf-resource-group-flow-entity-hier-update-group:p1:inst-update-group-4d
        // IF any child would become invalid → RETURN InvalidParentType with
        // child details (returned by `move_group_internal_impl` as
        // `DomainError::invalid_parent_type` when the parent's type is not in
        // the moved subtree's `allowed_parent_types`).
        let parent_changed = existing.parent_id != req.parent_id;
        // The fresh read is the authoritative one. If it says this request
        // moves the group but the transaction was opened below SERIALIZABLE
        // on a stale hint, stop here -- before the move branch and before the
        // update below, so nothing has been written -- and let the caller
        // rerun the whole operation at the right level.
        if parent_changed && !isolation.is_serializable() {
            return Ok(UpdateGroupOutcome::NeedsSerializable);
        }
        if parent_changed {
            // @cpt-begin:cpt-cf-resource-group-flow-entity-hier-update-group:p1:inst-update-group-4a
            // Validate new type's allowed_parents permits current parent's type
            // (or the new type allows root if no parent). For the immutable-type
            // case this collapses into `move_group_internal_impl` running the
            // `rg_type.allowed_parent_types` check on a parent change.
            //
            // Loaded here rather than unconditionally above: `rg_type` is
            // read only to hand to `move_group_internal_impl` a few lines
            // down, and `find_by_code` loads it by also reading the
            // `gts_type_allowed_parent` and `gts_type_allowed_membership`
            // junction tables. A rename or metadata edit never reaches this
            // branch, so it no longer pays for either.
            let rg_type = type_repo
                .find_by_code(tx, &existing_type_path)
                .await?
                .ok_or_else(|| DomainError::type_not_found(&existing_type_path))?;
            // @cpt-end:cpt-cf-resource-group-flow-entity-hier-update-group:p1:inst-update-group-4a

            // Delegate to move logic (cycle detection + closure rebuild).
            // Type stays the same, so use the resolved `rg_type` for parent
            // compatibility checks inside the move helper. Its
            // `MoveOutcome::parent_tenant_id` is not read here: the
            // cross-tenant check already ran above, against the exact row
            // `new_parent_snapshot` came from.
            Self::move_group_internal_impl(
                group_repo,
                tx,
                group_id,
                req.parent_id,
                new_parent_snapshot,
                &rg_type,
                profile,
            )
            .await?;
        }
        // @cpt-end:cpt-cf-resource-group-flow-entity-hier-update-group:p1:inst-update-group-4d
        // @cpt-end:cpt-cf-resource-group-flow-entity-hier-update-group:p1:inst-update-group-4c
        // @cpt-end:cpt-cf-resource-group-flow-entity-hier-update-group:p1:inst-update-group-4b
        // @cpt-end:cpt-cf-resource-group-flow-entity-hier-update-group:p1:inst-update-group-4

        // @cpt-begin:cpt-cf-resource-group-flow-entity-hier-update-group:p1:inst-update-group-5
        // Persist name/parent/metadata. `gts_type_id` is reused from the
        // existing row — type is immutable on update.
        let rows = group_repo
            .update(
                tx,
                group_id,
                req.parent_id,
                existing.gts_type_id,
                &req.name,
                req.metadata.as_ref(),
            )
            .await?;
        // The pre-read above happened in this same transaction, so `id`
        // vanishing between it and this UPDATE should be impossible -- but
        // `update` returning `rows_affected` instead of `()` means this is no
        // longer just an assumption to trust.
        if rows == 0 {
            return Err(DomainError::group_not_found(group_id));
        }
        // @cpt-end:cpt-cf-resource-group-flow-entity-hier-update-group:p1:inst-update-group-5

        // @cpt-begin:cpt-cf-resource-group-flow-entity-hier-update-group:p1:inst-update-group-6
        // Assembled, not read back. Every field was either just written from
        // this request or reused from `existing` because it is immutable, and
        // the type path was resolved above -- so the row the database now
        // holds is fully determined here. The read this replaces was the
        // second one after the write, the first being inside `update` itself.
        Ok(UpdateGroupOutcome::Done(ResourceGroup {
            id: group_id,
            code: existing_type_path,
            name: req.name.clone(),
            hierarchy: GroupHierarchy {
                parent_id: req.parent_id,
                tenant_id: existing.tenant_id,
            },
            metadata: req.metadata.clone(),
        }))
        // @cpt-end:cpt-cf-resource-group-flow-entity-hier-update-group:p1:inst-update-group-6
    }

    /// Inner logic for `move_group`, runs inside a SERIALIZABLE transaction.
    /// Returns the moved group and the number of closure rows written.
    async fn move_group_inner(
        group_repo: &GR,
        type_repo: &TR,
        tx: &impl DBRunner,
        group_id: Uuid,
        new_parent_id: Option<Uuid>,
        profile: &QueryProfile,
    ) -> Result<(ResourceGroup, u64), DomainError> {
        // @cpt-begin:cpt-cf-resource-group-flow-entity-hier-move-group:p1:inst-move-group-3
        // Load group and new parent in transaction
        let existing = group_repo
            .find_model_by_id(tx, group_id)
            .await?
            .ok_or_else(|| DomainError::group_not_found(group_id))?;

        let type_path = Self::resolve_type_path_from_id(tx, existing.gts_type_id).await?;
        let rg_type = type_repo
            .find_by_code(tx, &type_path)
            .await?
            .ok_or_else(|| DomainError::type_not_found(&type_path))?;
        // @cpt-end:cpt-cf-resource-group-flow-entity-hier-move-group:p1:inst-move-group-3

        // @cpt-begin:cpt-cf-resource-group-flow-entity-hier-move-group:p1:inst-move-group-4
        // @cpt-begin:cpt-cf-resource-group-flow-entity-hier-move-group:p1:inst-move-group-5
        // @cpt-begin:cpt-cf-resource-group-flow-entity-hier-move-group:p1:inst-move-group-6
        // @cpt-begin:cpt-cf-resource-group-flow-entity-hier-move-group:p1:inst-move-group-7
        // @cpt-begin:cpt-cf-resource-group-flow-entity-hier-move-group:p1:inst-move-group-8
        // @cpt-begin:cpt-cf-resource-group-flow-entity-hier-move-group:p1:inst-move-group-9
        // The one read of the new parent this move makes -- see
        // `move_group_internal_impl`'s doc for why it takes this as a
        // snapshot instead of reading the row itself.
        let new_parent_snapshot = match new_parent_id {
            Some(new_pid) => {
                let parent = group_repo
                    .find_model_by_id(tx, new_pid)
                    .await?
                    .ok_or_else(|| DomainError::group_not_found(new_pid))?;
                Some(ParentSnapshot {
                    tenant_id: parent.tenant_id,
                    gts_type_id: parent.gts_type_id,
                })
            }
            None => None,
        };

        // Cycle detect, type compat, profile enforce, closure rebuild
        let outcome = Self::move_group_internal_impl(
            group_repo,
            tx,
            group_id,
            new_parent_id,
            new_parent_snapshot,
            &rg_type,
            profile,
        )
        .await?;
        // @cpt-end:cpt-cf-resource-group-flow-entity-hier-move-group:p1:inst-move-group-9
        // @cpt-end:cpt-cf-resource-group-flow-entity-hier-move-group:p1:inst-move-group-8
        // @cpt-end:cpt-cf-resource-group-flow-entity-hier-move-group:p1:inst-move-group-7
        // @cpt-end:cpt-cf-resource-group-flow-entity-hier-move-group:p1:inst-move-group-6
        // @cpt-end:cpt-cf-resource-group-flow-entity-hier-move-group:p1:inst-move-group-5
        // @cpt-end:cpt-cf-resource-group-flow-entity-hier-move-group:p1:inst-move-group-4

        // Cross-tenant moves are forbidden (`tenant_id` is immutable per the
        // gear-wide invariant). Reject the move when the new parent lives
        // in a different tenant than the moved group; tenant-type roots have
        // `tenant_id == group_id`, so the equality check covers them too.
        if let Some(parent_tenant_id) = outcome.parent_tenant_id
            && parent_tenant_id != existing.tenant_id
        {
            // Generic message: do not interpolate tenant ids — the caller
            // can't act on them legitimately, and disclosing the foreign
            // tenant_id would leak ownership of `new_parent_id` across the
            // tenant boundary.
            return Err(DomainError::validation(format!(
                "Cannot move group {group_id} to a parent in a different tenant; \
                 cross-tenant moves are not supported"
            )));
        }

        // @cpt-begin:cpt-cf-resource-group-flow-entity-hier-move-group:p1:inst-move-group-10
        // Update parent_id on the group. Type and tenant_id are immutable —
        // both reuse the existing row's values.
        let rows = group_repo
            .update(
                tx,
                group_id,
                new_parent_id,
                existing.gts_type_id,
                &existing.name,
                existing.metadata.as_ref(),
            )
            .await?;
        // The pre-read above happened in this same transaction, so `id`
        // vanishing between it and this UPDATE should be impossible -- but
        // `update` returning `rows_affected` instead of `()` means this is no
        // longer just an assumption to trust.
        if rows == 0 {
            return Err(DomainError::group_not_found(group_id));
        }
        // @cpt-end:cpt-cf-resource-group-flow-entity-hier-move-group:p1:inst-move-group-10

        // Assembled rather than read back, as in `update_group_inner`: a move
        // writes exactly one column, and the rest of the row is `existing`
        // unchanged.
        //
        // `closure_rows` is returned alongside the group so the caller can
        // record the metric outside the retryable transaction, avoiding
        // double-count on retry.
        let closure_rows = outcome.closure_rows;
        Ok((
            ResourceGroup {
                id: group_id,
                code: type_path,
                name: existing.name,
                hierarchy: GroupHierarchy {
                    parent_id: new_parent_id,
                    tenant_id: existing.tenant_id,
                },
                metadata: existing.metadata,
            },
            closure_rows,
        ))
    }

    /// Inner logic for `delete_group`, runs inside the transaction its caller
    /// opened -- `SERIALIZABLE` for a force delete, the backend default for a
    /// non-force one.
    /// Returns the subtree node count (for metric recording) on force delete.
    async fn delete_group_inner(
        group_repo: &GR,
        tx: &impl DBRunner,
        scope: &toolkit_security::AccessScope,
        group_id: Uuid,
        force: bool,
    ) -> Result<Option<u64>, DomainError> {
        // @cpt-begin:cpt-cf-resource-group-flow-entity-hier-delete-group:p1:inst-delete-group-2
        // @cpt-begin:cpt-cf-resource-group-flow-entity-hier-delete-group:p1:inst-delete-group-3
        // DB: SELECT FROM resource_group WHERE id = {group_id}
        group_repo
            .find_by_id(tx, scope, group_id)
            .await?
            .ok_or_else(|| DomainError::group_not_found(group_id))?;

        // The row was just read and its absence already answered; a second
        // read of the same id added a round-trip inside the transaction and
        // was discarded. The artifact declares one SELECT here, not two.
        // @cpt-end:cpt-cf-resource-group-flow-entity-hier-delete-group:p1:inst-delete-group-3
        // @cpt-end:cpt-cf-resource-group-flow-entity-hier-delete-group:p1:inst-delete-group-2

        if force {
            // @cpt-begin:cpt-cf-resource-group-flow-entity-hier-delete-group:p1:inst-delete-group-5
            // @cpt-begin:cpt-cf-resource-group-flow-entity-hier-delete-group:p1:inst-delete-group-5a
            // @cpt-begin:cpt-cf-resource-group-flow-entity-hier-delete-group:p1:inst-delete-group-5b
            // @cpt-begin:cpt-cf-resource-group-flow-entity-hier-delete-group:p1:inst-delete-group-5c
            // @cpt-begin:cpt-cf-resource-group-flow-entity-hier-delete-group:p1:inst-delete-group-5d
            // Force delete: cascade entire subtree + memberships + closure
            #[allow(clippy::let_and_return)]
            let result = Self::force_delete_subtree(group_repo, tx, group_id)
                .await
                .map(Some);
            // @cpt-end:cpt-cf-resource-group-flow-entity-hier-delete-group:p1:inst-delete-group-5d
            // @cpt-end:cpt-cf-resource-group-flow-entity-hier-delete-group:p1:inst-delete-group-5c
            // @cpt-end:cpt-cf-resource-group-flow-entity-hier-delete-group:p1:inst-delete-group-5b
            // @cpt-end:cpt-cf-resource-group-flow-entity-hier-delete-group:p1:inst-delete-group-5a
            // @cpt-end:cpt-cf-resource-group-flow-entity-hier-delete-group:p1:inst-delete-group-5
            // @cpt-begin:cpt-cf-resource-group-flow-entity-hier-delete-group:p1:inst-delete-group-7
            result
            // @cpt-end:cpt-cf-resource-group-flow-entity-hier-delete-group:p1:inst-delete-group-7
        } else {
            // @cpt-begin:cpt-cf-resource-group-flow-entity-hier-delete-group:p1:inst-delete-group-4
            // Non-force: check children and memberships
            //
            // Lock the target first. The two checks below decide from rows
            // that reference this group, and the delete acts on that
            // decision; without the lock a concurrent `create_group` under
            // this parent can land between them and leave an orphan. Holding
            // the row makes that writer wait and then find the parent gone.
            // This is what lets the transaction run below SERIALIZABLE --
            // see `delete_group`.
            group_repo
                .find_model_by_id_for_update(tx, group_id)
                .await?
                .ok_or_else(|| DomainError::group_not_found(group_id))?;

            // @cpt-begin:cpt-cf-resource-group-flow-entity-hier-delete-group:p1:inst-delete-group-4a
            let children = Self::get_direct_children(tx, group_id).await?;
            // @cpt-end:cpt-cf-resource-group-flow-entity-hier-delete-group:p1:inst-delete-group-4a
            // @cpt-begin:cpt-cf-resource-group-flow-entity-hier-delete-group:p1:inst-delete-group-4b
            let has_memberships = group_repo.has_memberships(tx, group_id).await?;
            // @cpt-end:cpt-cf-resource-group-flow-entity-hier-delete-group:p1:inst-delete-group-4b
            // @cpt-begin:cpt-cf-resource-group-flow-entity-hier-delete-group:p1:inst-delete-group-4c
            if !children.is_empty() {
                return Err(DomainError::conflict_active_references(format!(
                    "Cannot delete group '{group_id}': has {} child group(s). Use force=true to cascade.",
                    children.len()
                )));
            }

            if has_memberships {
                return Err(DomainError::conflict_active_references(format!(
                    "Cannot delete group '{group_id}': has active memberships. Use force=true to cascade."
                )));
            }
            // @cpt-end:cpt-cf-resource-group-flow-entity-hier-delete-group:p1:inst-delete-group-4c
            // @cpt-end:cpt-cf-resource-group-flow-entity-hier-delete-group:p1:inst-delete-group-4

            // @cpt-begin:cpt-cf-resource-group-flow-entity-hier-delete-group:p1:inst-delete-group-6
            // @cpt-begin:cpt-cf-resource-group-flow-entity-hier-delete-group:p1:inst-delete-group-6a
            // Delete closure rows, then the group
            group_repo.delete_all_closure_rows(tx, group_id).await?;
            // @cpt-end:cpt-cf-resource-group-flow-entity-hier-delete-group:p1:inst-delete-group-6a
            // @cpt-begin:cpt-cf-resource-group-flow-entity-hier-delete-group:p1:inst-delete-group-6b
            group_repo.delete_by_id(tx, group_id).await.map(|()| None)
            // @cpt-end:cpt-cf-resource-group-flow-entity-hier-delete-group:p1:inst-delete-group-6b
            // @cpt-end:cpt-cf-resource-group-flow-entity-hier-delete-group:p1:inst-delete-group-6
        }
    }

    // -- Internal helpers --

    // @cpt-algo:cpt-cf-resource-group-algo-entity-hier-cycle-detect:p1
    // @cpt-algo:cpt-cf-resource-group-algo-entity-hier-enforce-query-profile:p1
    /// Internal move logic shared between `move_group` and `update_group`.
    ///
    /// Performs cycle detection, type compatibility checks, query profile
    /// enforcement, and closure table rebuild. Must be called within a
    /// SERIALIZABLE transaction.
    ///
    /// Takes the new parent as a `ParentSnapshot` the caller already read,
    /// rather than reading the row again here. Both callers need this same
    /// row for their own cross-tenant check -- `update_group_inner` before
    /// calling in, `move_group_inner` after, via the returned
    /// `MoveOutcome::parent_tenant_id` -- so a second read of the same id in
    /// here on top of that was purely redundant. `parent` is `None` exactly
    /// when `new_parent_id` is; a caller passing `Some(new_parent_id)` with
    /// no snapshot behind it (which should not happen given the two callers
    /// above) is treated the same as the read this replaces finding nothing.
    #[allow(clippy::cognitive_complexity)]
    async fn move_group_internal_impl(
        group_repo: &GR,
        conn: &impl DBRunner,
        group_id: Uuid,
        new_parent_id: Option<Uuid>,
        parent: Option<ParentSnapshot>,
        rg_type: &resource_group_sdk::ResourceGroupType,
        profile: &QueryProfile,
    ) -> Result<MoveOutcome, DomainError> {
        let mut parent_tenant_id: Option<Uuid> = None;
        if let Some(new_pid) = new_parent_id {
            // @cpt-begin:cpt-cf-resource-group-algo-entity-hier-cycle-detect:p1:inst-cycle-1
            // Cycle detection: self-parent check (covered by is_descendant via self-row)
            // @cpt-end:cpt-cf-resource-group-algo-entity-hier-cycle-detect:p1:inst-cycle-1
            // @cpt-begin:cpt-cf-resource-group-algo-entity-hier-cycle-detect:p1:inst-cycle-2
            // @cpt-begin:cpt-cf-resource-group-algo-entity-hier-cycle-detect:p1:inst-cycle-3
            let is_desc = group_repo.is_descendant(conn, group_id, new_pid).await?;
            if is_desc {
                debug!(group_id = %group_id, new_parent = %new_pid, "Cycle detected in move_group");
                return Err(DomainError::cycle_detected(format!(
                    "Cannot move group '{group_id}' under '{new_pid}': would create a cycle"
                )));
            }
            // @cpt-end:cpt-cf-resource-group-algo-entity-hier-cycle-detect:p1:inst-cycle-3
            // @cpt-end:cpt-cf-resource-group-algo-entity-hier-cycle-detect:p1:inst-cycle-2

            // Validate parent type compatibility. `parent` came from the
            // caller's own read -- see the function doc -- so `None` here
            // means that read found nothing, same as this function's own
            // `find_model_by_id` used to.
            let parent = parent.ok_or_else(|| DomainError::group_not_found(new_pid))?;
            parent_tenant_id = Some(parent.tenant_id);

            let parent_type_path =
                Self::resolve_type_path_from_id(conn, parent.gts_type_id).await?;
            if !rg_type.allowed_parent_types.contains(&parent_type_path) {
                return Err(DomainError::invalid_parent_type(format!(
                    "Type '{}' does not allow parent type '{}'",
                    rg_type.code, parent_type_path
                )));
            }

            // @cpt-begin:cpt-cf-resource-group-algo-entity-hier-cycle-detect:p1:inst-cycle-4
            // Cycle detection passed
            // @cpt-end:cpt-cf-resource-group-algo-entity-hier-cycle-detect:p1:inst-cycle-4

            // @cpt-begin:cpt-cf-resource-group-algo-entity-hier-enforce-query-profile:p1:inst-profile-1
            // Load profile config: max_depth (optional), max_width (optional)
            // (profile is passed as parameter with max_depth and max_width)
            // @cpt-end:cpt-cf-resource-group-algo-entity-hier-enforce-query-profile:p1:inst-profile-1

            // Check query profile: depth limit
            // @cpt-begin:cpt-cf-resource-group-algo-entity-hier-enforce-query-profile:p1:inst-profile-2
            if let Some(max_depth) = profile.max_depth {
                // @cpt-begin:cpt-cf-resource-group-algo-entity-hier-enforce-query-profile:p1:inst-profile-2a
                let parent_depth = group_repo.get_depth(conn, new_pid).await?;
                // A single `MAX(depth)` aggregate, not the whole subtree
                // pulled into this process to fold it down to one scalar:
                // this check has never needed the descendant rows
                // themselves, and it reruns on every move inside the
                // SERIALIZABLE transaction the default profile
                // (`max_depth: Some(10)`) puts every move through. See
                // `get_descendant_ids_with_depth` for the callers -- force
                // delete -- that do need the rows.
                let max_subtree_depth = group_repo.get_max_descendant_depth(conn, group_id).await?;
                let new_deepest = parent_depth + 1 + max_subtree_depth;
                // @cpt-end:cpt-cf-resource-group-algo-entity-hier-enforce-query-profile:p1:inst-profile-2a
                // @cpt-begin:cpt-cf-resource-group-algo-entity-hier-enforce-query-profile:p1:inst-profile-2b
                #[allow(clippy::cast_possible_wrap)]
                if new_deepest >= max_depth as i32 {
                    debug!(group_id = %group_id, new_deepest, max_depth, "Depth limit exceeded on move");
                    return Err(DomainError::limit_violation(format!(
                        "Depth limit exceeded: moving subtree would create depth {new_deepest}, max_depth is {max_depth}"
                    )));
                }
                // @cpt-end:cpt-cf-resource-group-algo-entity-hier-enforce-query-profile:p1:inst-profile-2b
            }
            // @cpt-end:cpt-cf-resource-group-algo-entity-hier-enforce-query-profile:p1:inst-profile-2

            // Check query profile: width limit
            // @cpt-begin:cpt-cf-resource-group-algo-entity-hier-enforce-query-profile:p1:inst-profile-3
            if let Some(max_width) = profile.max_width {
                // @cpt-begin:cpt-cf-resource-group-algo-entity-hier-enforce-query-profile:p1:inst-profile-3a
                let sibling_count = group_repo.count_children(conn, new_pid).await?;
                // @cpt-end:cpt-cf-resource-group-algo-entity-hier-enforce-query-profile:p1:inst-profile-3a
                // @cpt-begin:cpt-cf-resource-group-algo-entity-hier-enforce-query-profile:p1:inst-profile-3b
                if sibling_count >= u64::from(max_width) {
                    return Err(DomainError::limit_violation(format!(
                        "Width limit exceeded: new parent already has {sibling_count} children, max_width is {max_width}"
                    )));
                }
                // @cpt-end:cpt-cf-resource-group-algo-entity-hier-enforce-query-profile:p1:inst-profile-3b
            }
            // @cpt-end:cpt-cf-resource-group-algo-entity-hier-enforce-query-profile:p1:inst-profile-3
            // @cpt-begin:cpt-cf-resource-group-algo-entity-hier-enforce-query-profile:p1:inst-profile-4
            // Profile checks passed
            // @cpt-end:cpt-cf-resource-group-algo-entity-hier-enforce-query-profile:p1:inst-profile-4
        } else {
            // Moving to root: validate can_be_root + tenant-root uniqueness.
            if !rg_type.can_be_root {
                return Err(DomainError::invalid_parent_type(format!(
                    "Type '{}' cannot be a root group (can_be_root=false)",
                    rg_type.code
                )));
            }

            // Tenant-root uniqueness: at most one tenant-type group may be a
            // forest root. Mirrors the guard in `create_group_inner` —
            // `cpt-cf-resource-group-fr-enforce-tenant-root-uniqueness`. We
            // exclude the moved group itself so a no-op move (already root)
            // does not falsely fire.
            if rg_type.code.starts_with(TENANT_RG_TYPE_PATH)
                && let Some(existing_root_id) = group_repo
                    .find_root_id_with_type_prefix(conn, TENANT_RG_TYPE_PATH)
                    .await?
                && existing_root_id != group_id
            {
                return Err(DomainError::tenant_root_already_exists(
                    existing_root_id,
                    format!(
                        "Cannot move tenant-type group '{}' ({group_id}) to root: tenant root already exists",
                        rg_type.code
                    ),
                ));
            }
        }

        // Rebuild closure table for the subtree
        let closure_rows = group_repo
            .rebuild_subtree_closure(conn, group_id, new_parent_id)
            .await?;

        Ok(MoveOutcome {
            parent_tenant_id,
            closure_rows,
        })
    }

    /// Force-delete an entire subtree (group + descendants + memberships + closure).
    async fn force_delete_subtree(
        group_repo: &GR,
        conn: &impl DBRunner,
        root_id: Uuid,
    ) -> Result<u64, DomainError> {
        let descendants_with_depth = group_repo
            .get_descendant_ids_with_depth(conn, root_id)
            .await?;

        let all_ids: Vec<Uuid> = std::iter::once(root_id)
            .chain(descendants_with_depth.iter().map(|(id, _depth)| *id))
            .collect();
        let subtree_count: u64 = all_ids.len().try_into().unwrap_or(u64::MAX);

        // Memberships and closure rows have no FK ordering constraint among
        // themselves, so both go in one statement for the whole subtree
        // rather than two per node.
        group_repo.delete_memberships_many(conn, &all_ids).await?;
        group_repo
            .delete_all_closure_rows_many(conn, &all_ids)
            .await?;

        // Group rows do have one: a parent cannot go before its children.
        // Deleting depth level by depth level, deepest first, keeps that
        // order while still batching each level into a single statement --
        // so the statement count follows tree depth, not node count.
        let mut ids_by_depth: std::collections::BTreeMap<i32, Vec<Uuid>> =
            std::collections::BTreeMap::new();
        ids_by_depth.entry(0).or_default().push(root_id);
        for (id, depth) in descendants_with_depth {
            ids_by_depth.entry(depth).or_default().push(id);
        }
        for ids in ids_by_depth.into_values().rev() {
            group_repo.delete_by_id_many(conn, &ids).await?;
        }

        Ok(subtree_count)
    }

    /// Get direct children of a group.
    async fn get_direct_children(
        conn: &impl DBRunner,
        parent_id: Uuid,
    ) -> Result<Vec<crate::infra::storage::entity::resource_group::Model>, DomainError> {
        use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
        use toolkit_db::secure::SecureEntityExt;

        let scope = toolkit_security::AccessScope::allow_all();
        crate::infra::storage::entity::resource_group::Entity::find()
            .filter(crate::infra::storage::entity::resource_group::Column::ParentId.eq(parent_id))
            .secure()
            .scope_with(&scope)
            .all(conn)
            .await
            .map_err(|e| DomainError::database(e.to_string()))
    }

    /// Resolve a type ID to its GTS path.
    async fn resolve_type_path_from_id(
        conn: &impl DBRunner,
        type_id: i16,
    ) -> Result<String, DomainError> {
        use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
        use toolkit_db::secure::SecureEntityExt;

        let scope = toolkit_security::AccessScope::allow_all();
        let model = crate::infra::storage::entity::gts_type::Entity::find()
            .filter(crate::infra::storage::entity::gts_type::Column::Id.eq(type_id))
            .secure()
            .scope_with(&scope)
            .one(conn)
            .await
            .map_err(|e| DomainError::database(e.to_string()))?
            .ok_or_else(|| DomainError::database(format!("Type ID {type_id} not found")))?;
        Ok(model.schema_id)
    }

    fn validate_name(name: &str) -> Result<(), DomainError> {
        // Count Unicode scalar values, not UTF-8 bytes, so the limit matches
        // the documented "255 characters" and aligns with the DB-level
        // `length(name) BETWEEN 1 AND 255` CHECK on PostgreSQL/SQLite, where
        // `length(text)` is character-based on both engines.
        if name.is_empty() || name.chars().count() > 255 {
            return Err(DomainError::validation(
                "Group name must be between 1 and 255 characters",
            ));
        }
        Ok(())
    }

    /// Reject a caller-supplied `tenant_id` on tenant-typed groups: tenant
    /// groups derive their `tenant_id` from the group's own id, not from a
    /// request field.
    fn reject_tenant_id_on_tenant_type(
        is_tenant: bool,
        tenant_id: Option<Uuid>,
    ) -> Result<(), DomainError> {
        if is_tenant && tenant_id.is_some() {
            return Err(DomainError::validation(
                "Tenant-typed groups cannot have an explicit tenant_id: \
                 their effective tenant is always the group's own id"
                    .to_owned(),
            ));
        }
        Ok(())
    }
}
// @cpt-end:cpt-cf-resource-group-dod-entity-hier-entity-service:p1:inst-full
