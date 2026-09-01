//! `RbacServiceGear` — toolkit module declaration and lifecycle wiring.
//!
//! The struct stores one `ArcSwapOption<RbacRuntime>` slot that `init()`
//! populates and the `DatabaseCapability` / `RestApiCapability` impls read.
//!
//! `init()` runs whenever the `rbac:` entry is present in the `gears:`
//! block; it short-circuits (no DB / `ClientHub` resolution) only when the
//! module is absent from config entirely (`ConfigError::GearNotFound`).
//! When it runs it acquires the DB handle, resolves the upstream `ClientHub`
//! contracts, registers entity GTS schemas, builds the runtime, publishes
//! `dyn RbacServiceClientV1` in `ClientHub`, runs the built-in role seeder,
//! and runs the platform-admin bootstrap (idempotent, soft-skip when
//! `platform_admin_subject_id` is unset).
//!
//! `DatabaseCapability::migrations()` returns the migration list
//! unconditionally — migrations are collected before `init()` runs, so they
//! execute even on a default-disabled instance.
//!
//! `RestApiCapability::register_rest()` mounts the RBAC routes when the
//! runtime is populated; on a default-disabled boot it falls back to an
//! empty `/rbac/v1` mount.

use std::sync::Arc;

use anyhow::{Context, Result};
use arc_swap::ArcSwapOption;
use async_trait::async_trait;
use axum::Router;
use rbac_sdk::api::RbacServiceClientV1;
use rbac_sdk::models::PrincipalType;
use resource_group_sdk::api::ResourceGroupReadHierarchy;
use sea_orm_migration::{MigrationTrait, MigratorTrait};
use tenant_resolver_sdk::api::TenantResolverClient;
use toolkit::api::OpenApiRegistry;
use toolkit::config::ConfigError;
use toolkit::contracts::{DatabaseCapability, RestApiCapability};
use toolkit::{Gear, GearCtx};
use toolkit_db::{DBProvider, DbError};
use tracing::{info, warn};
use types_registry_sdk::{RegisterResult, TypesRegistryClient};

use crate::api::service::local_client::RbacServiceLocalClient;
use crate::config::{PrincipalNamesConfig, RbacServiceConfig};
use crate::domain::permission_catalog::PermissionCatalog;
use crate::domain::permission_evaluator::PermissionEvaluator;
use crate::domain::policy_enforcer::PolicyEnforcer;
use crate::domain::principal_name_reader::PrincipalNameReader;
use crate::domain::rg_port::RbacRgRead;
use crate::domain::role_assignment::PrincipalNameHydrator;
use crate::domain::scope_validator::ScopeValidator;
use crate::domain::target_type_validator::TargetTypeValidator;
use crate::infra::am_user_name_reader::AmUserNameReader;
use crate::infra::bootstrap::{
    BootstrapDecision, BootstrapOutcome, BootstrapPlatformAdmin, evaluate_bootstrap_decision,
    seed_configured_grant,
};
use crate::infra::metrics::RbacMetricsMeter;
use crate::infra::rg_adapter::ResourceGroupReadAdapter;
use crate::infra::seeder::BuiltinRoleSeeder;
use crate::infra::storage::migrations::Migrator;
// Impl modules are imported by path so the structs are reached as
// `role_assignment_repo::RoleAssignmentRepository::new(...)` / etc.
// without colliding with the like-named domain traits above.
use crate::infra::storage::{role_assignment_repo, role_definition_repo};
use crate::infra::types_registry_permission_catalog::TypesRegistryPermissionCatalog;
use crate::infra::types_registry_target_type_validator::TypesRegistryTargetTypeValidator;

/// Vendored entity GTS JSON Schema for `gts.cf.core.rbac.role_definition.v1~`.
#[doc(hidden)]
pub const ROLE_DEFINITION_SCHEMA_JSON: &str =
    include_str!("../schemas/role_definition.v1.schema.json");
/// Vendored entity GTS JSON Schema for `gts.cf.core.rbac.role_assignment.v1~`.
#[doc(hidden)]
pub const ROLE_ASSIGNMENT_SCHEMA_JSON: &str =
    include_str!("../schemas/role_assignment.v1.schema.json");

/// Canonical GTS identifier for the role-definition entity schema.
#[doc(hidden)]
pub const ROLE_DEFINITION_GTS_ID: &str = "gts://gts.cf.core.rbac.role_definition.v1~";
/// Canonical GTS identifier for the role-assignment entity schema.
#[doc(hidden)]
pub const ROLE_ASSIGNMENT_GTS_ID: &str = "gts://gts.cf.core.rbac.role_assignment.v1~";

/// Refresh cadence for the RBAC inventory gauges (`rbac_role_definitions`
/// / `rbac_role_assignments`). Matches the typical metric export interval —
/// inventory counts don't need finer resolution.
const INVENTORY_REFRESH_INTERVAL: std::time::Duration = std::time::Duration::from_mins(1);

/// Typed bundle of the per-process REST state that `init()` builds and
/// `register_rest()` reads.
pub struct RbacRuntime {
    pub role_definitions: Arc<crate::api::rest::role_definitions::ApiState>,
    pub role_assignments: Arc<crate::api::rest::role_assignments::ApiState>,
    pub permissions: Arc<crate::api::rest::permissions::ApiState>,
}

/// `rbac` module.
///
/// Capabilities: `db` (gives `init()` access to `ctx.db_required()` and
/// triggers migration collection at startup) and `rest` (registers the `/rbac/v1`
/// mount point).
///
/// Dependencies: `types-registry`, `tenant-resolver`, and `resource-group`
/// MUST complete their own `init()` first (`toolkit` enforces via `deps`).
/// `system` is intentionally NOT declared because `system` modules run
/// before all non-system modules and would override the topo-sorted `deps`
/// edges across that boundary (specifically, `resource-group` is
/// non-system).
#[toolkit::gear(
    name = "rbac",
    capabilities = [db, rest],
    deps = [types_registry, tenant_resolver, resource_group]
)]
pub struct RbacServiceGear {
    /// Typed runtime built inside [`Gear::init`] and consumed by
    /// [`RestApiCapability::register_rest`]. `None` when `init()` has not
    /// completed or short-circuited on a default-disabled boot.
    runtime: ArcSwapOption<RbacRuntime>,
}

impl Default for RbacServiceGear {
    fn default() -> Self {
        Self {
            runtime: ArcSwapOption::from(None),
        }
    }
}

#[async_trait]
impl Gear for RbacServiceGear {
    /// Init orchestrator: read config, drive the [`RbacRuntimeBuilder`]
    /// through its `with_*` steps, then run the two post-build side effects
    /// (seeder + bootstrap).
    async fn init(&self, ctx: &GearCtx) -> Result<()> {
        // RBAC runs whenever its `rbac:` entry is present in the `gears:`
        // block — there is no `enabled` switch (disable = remove the entry).
        // `ctx.config()` lets us tell the cases apart:
        //   * GearNotFound        → rbac absent from `gears:`   → no-op.
        //   * MissingConfigSection→ block present, no `config:`   → run w/ defaults.
        //   * Ok(cfg)             → present + valid               → run.
        //   * other (InvalidConfig/InvalidGearStructure)        → abort loud.
        // The loud-abort case is the important one: a present-but-invalid
        // section (typo'd key under `deny_unknown_fields`, wrong type, …) MUST
        // fail init rather than silently boot a mis-configured security module.
        let cfg: RbacServiceConfig = match ctx.config() {
            Ok(cfg) => cfg,
            Err(ConfigError::GearNotFound { .. }) => {
                info!(
                    "rbac: not present in the `gears:` config block, \
                     skipping init() (module compiled in but unconfigured)"
                );
                return Ok(());
            }
            Err(ConfigError::MissingConfigSection { .. }) => RbacServiceConfig::default(),
            Err(e) => return Err(e).context("rbac: invalid `rbac` config section"),
        };
        // Reject a configuration that would write a privilege nobody asked for
        // (a grant naming a role this deployment does not seed), or one that
        // parses but degrades a feature in the worst direction — a zero
        // display-name bound turns every read into per-id point lookups, the
        // N+1 the batched pass exists to avoid. Both abort before any side
        // effect runs: a mis-configured security module must fail init rather
        // than boot.
        cfg.validate()
            .context("rbac: invalid `rbac` config section")?;
        warn_if_owner_cannot_administer_rbac(&cfg);
        info!("rbac: init() begin");

        let (runtime, db) = RbacRuntimeBuilder::new(ctx, cfg.principal_names.clone())
            .with_db()?
            .with_upstream_clients()?
            .with_repositories()
            .with_evaluator_and_local_client()
            .with_policy_layer()
            .register_schemas()
            .await?
            .into_runtime();

        // Side-effecting steps that need a fresh `DbConn` and an
        // application-config-derived subject id; failures surface as `init()` errors.
        // The runtime is published AFTER both side effects succeed so that a
        // failed init() never leaves a "ready" runtime in the ArcSwap slot —
        // a half-init `Err(_)` must not be observable as `runtime_is_populated() == true`.
        // One transaction for the whole roster: the seeder issues two
        // statements per role (upsert + invariant read-back), so a crash
        // between them must not leave the built-in catalogue partially seeded.
        // The roster is ordered by ascending id, which is what makes holding
        // the locks safe against a second concurrent seeder.
        let seed_integration = cfg.seed_integration_roles;
        let targets = cfg.builtin_role_targets.clone();
        db.transaction(move |tx| {
            Box::pin(async move {
                BuiltinRoleSeeder::new()
                    .seed(tx, seed_integration, &targets)
                    .await
                    .map_err(|err| {
                        toolkit_db::DbError::Other(anyhow::anyhow!(
                            "rbac: built-in role seeder failed: {err}"
                        ))
                    })
            })
        })
        .await
        .context("rbac: built-in role seeding transaction failed")?;

        // The bootstrap steps below take their own connection; they are
        // separate decisions from seeding and must not share its
        // transaction (a failed grant must not roll the roster back).
        let conn = db
            .conn()
            .context("rbac: failed to acquire DbConn for platform-admin bootstrap")?;

        match evaluate_bootstrap_decision(cfg.platform_admin_subject_id.as_deref()) {
            BootstrapDecision::Skip => {
                tracing::warn!(
                    "rbac: platform-admin bootstrap skipped \u{2014} \
                     `platform_admin_subject_id` is not configured; \
                     set it (hosts usually inject the value at startup from \
                     an environment variable or a mounted secret) to create \
                     the initial Owner-at-/ assignment on next restart"
                );
            }
            BootstrapDecision::Run(subject_id) => {
                let outcome = BootstrapPlatformAdmin::new()
                    .run(&conn, &subject_id)
                    .await
                    .context("rbac: platform-admin bootstrap failed")?;
                match outcome {
                    BootstrapOutcome::Created => info!(
                        principal_id = %subject_id,
                        "rbac: platform-admin bootstrap created Owner-at-/ assignment"
                    ),
                    BootstrapOutcome::AlreadyAssigned => tracing::debug!(
                        principal_id = %subject_id,
                        "rbac: platform-admin bootstrap skipped \u{2014} \
                         Owner-at-/ assignment already exists"
                    ),
                }
            }
        }

        // Grant each configured principal its built-in role at `/`. Idempotent,
        // and empty unless the deployment asked for it: a grant is a privilege,
        // so RBAC never invents one. Runs after `BuiltinRoleSeeder::seed` (the
        // role must exist first) and independently of the optional
        // platform-admin bootstrap. Role names were resolved during config
        // validation, so a miss here is impossible rather than silently skipped.
        //
        // The two lists differ only in `principal_type`, and that separation is
        // the point: the type must match what the caller's token classifies as,
        // and one list with a type field would let a typo produce a valid config
        // whose grant then matches nothing, with no error at any layer.
        for (grants, principal_type, list) in [
            (
                &cfg.service_principal_grants,
                PrincipalType::ServicePrincipal,
                "service_principal_grants",
            ),
            (&cfg.user_grants, PrincipalType::User, "user_grants"),
        ] {
            for grant in grants {
                let Some(role_id) =
                    BuiltinRoleSeeder::role_id_by_name(&grant.role, cfg.seed_integration_roles)
                else {
                    anyhow::bail!(
                        "rbac: {list} names built-in role {:?}, \
                         which this deployment does not seed",
                        grant.role
                    );
                };
                match seed_configured_grant(&conn, role_id, &grant.principal_id, principal_type)
                    .await
                    .with_context(|| {
                        format!("rbac: {list} grant failed for role {:?}", grant.role)
                    })? {
                    BootstrapOutcome::Created => info!(
                        role = %grant.role,
                        principal_type = principal_type.as_str(),
                        "rbac: seeded configured grant at scope /"
                    ),
                    BootstrapOutcome::AlreadyAssigned => tracing::debug!(
                        role = %grant.role,
                        principal_type = principal_type.as_str(),
                        "rbac: configured grant already exists"
                    ),
                }
            }
        }

        // Publish the typed runtime LAST so a half-init failure leaves
        // the slot `None`. `register_rest()` and `runtime_is_populated()`
        // both branch on this slot.
        self.runtime.store(Some(Arc::new(runtime)));

        info!("rbac: init() complete");
        Ok(())
    }
}

/// `DatabaseCapability` impl. Returns the RBAC migration list so `toolkit`
/// runs migrations at platform startup before any RBAC code reads the DB.
impl DatabaseCapability for RbacServiceGear {
    fn migrations(&self) -> Vec<Box<dyn MigrationTrait>> {
        info!("rbac: collecting database migrations");
        Migrator::migrations()
    }
}

/// `RestApiCapability` impl. Falls back to an empty `/rbac/v1` mount on a
/// default-disabled boot.
impl RestApiCapability for RbacServiceGear {
    fn register_rest(
        &self,
        _ctx: &GearCtx,
        router: Router,
        openapi: &dyn OpenApiRegistry,
    ) -> Result<Router> {
        // Mount RBAC routes when `init()` has populated the runtime; otherwise
        // keep an empty `/rbac/v1` mount.
        //
        // `openapi` is forwarded to each sub-router so the generated
        // OpenAPI document at `/openapi.json` lists every RBAC operation.
        if let Some(runtime) = self.runtime.load_full() {
            // Apply the canonical-error middleware once at the module level
            // so every handler benefits from `trace_id` / `instance` enrichment
            // and the structured response-path log. `From<RbacServiceError>
            // for CanonicalError` does the per-variant mapping (see
            // `api/rest/error.rs`); the middleware renders the wire RFC 9457
            // `Problem`.
            let r = router
                .merge(crate::api::rest::role_definitions::router(
                    Arc::clone(&runtime.role_definitions),
                    openapi,
                ))
                .merge(crate::api::rest::role_assignments::router(
                    Arc::clone(&runtime.role_assignments),
                    openapi,
                ))
                .merge(crate::api::rest::permissions::router(
                    Arc::clone(&runtime.permissions),
                    openapi,
                ))
                .layer(axum::middleware::from_fn(
                    toolkit::api::canonical_error_middleware,
                ));
            Ok(r)
        } else {
            Ok(router.nest("/rbac/v1", Router::new()))
        }
    }
}

impl RbacServiceGear {
    /// Returns `true` iff `init()` populated the [`RbacRuntime`] (every
    /// dependency wired). Partial init is not representable — `init()`
    /// either commits the full runtime or returns `Err`.
    pub fn runtime_is_populated(&self) -> bool {
        self.runtime.load().is_some()
    }
}

/// Fluent builder for the per-process [`RbacRuntime`].
///
/// Linear data dependencies between steps:
/// * `with_db` precedes `with_repositories`.
/// * `with_upstream_clients` precedes `with_evaluator_and_local_client` and
///   `with_policy_layer`.
///
/// Out-of-order calls trip [`unreachable!`] inside [`take_required`] /
/// [`clone_required`].
pub(crate) struct RbacRuntimeBuilder<'a> {
    ctx: &'a GearCtx,
    /// Display-name settings, read from gear config in `init()`. Carried
    /// on the builder because the read path — not the wiring — is where
    /// they take effect.
    principal_names: PrincipalNamesConfig,
    db: Option<DBProvider<DbError>>,
    tenant_resolver: Option<Arc<dyn TenantResolverClient>>,
    rg_read: Option<Arc<dyn RbacRgRead>>,
    registry: Option<Arc<dyn TypesRegistryClient>>,
    scope_validator: Option<Arc<ScopeValidator>>,
    role_repo: Option<Arc<ConcreteRoleDefinitionRepo>>,
    assignment_repo: Option<Arc<ConcreteRoleAssignmentRepo>>,
    evaluator: Option<Arc<ConcretePermissionEvaluator>>,
    policy_enforcer: Option<Arc<dyn PolicyEnforcer>>,
    target_type_validator: Option<Arc<dyn TargetTypeValidator>>,
    permission_catalog: Option<Arc<dyn PermissionCatalog>>,
}

use crate::domain::role_assignment::RoleAssignmentService;
use crate::domain::role_definition::RoleDefinitionService;

/// Concrete repository types, and the service instantiations built over
/// them.
///
/// The repository traits take the executor as `<C: DBRunner>`, which is not
/// dyn-compatible, so the services name their repositories concretely.
/// These aliases are the single place that naming happens — the same role
/// `gear.rs` plays for `resource-group`'s `ConcreteGroupService`.
pub type ConcreteRoleDefinitionRepo =
    crate::infra::storage::role_definition_repo::RoleDefinitionRepository;
/// See [`ConcreteRoleDefinitionRepo`].
pub type ConcreteRoleAssignmentRepo =
    crate::infra::storage::role_assignment_repo::RoleAssignmentRepository;
/// Production [`RoleDefinitionService`] instantiation.
pub type ConcreteRoleDefinitionService =
    RoleDefinitionService<ConcreteRoleDefinitionRepo, ConcreteRoleAssignmentRepo>;
/// Production [`RoleAssignmentService`] instantiation.
pub type ConcreteRoleAssignmentService =
    RoleAssignmentService<ConcreteRoleAssignmentRepo, ConcreteRoleDefinitionRepo>;
/// Production [`PermissionEvaluator`] instantiation.
pub type ConcretePermissionEvaluator =
    PermissionEvaluator<ConcreteRoleAssignmentRepo, ConcreteRoleDefinitionRepo>;
/// Production [`PrincipalNameHydrator`] instantiation.
pub type ConcretePrincipalNameHydrator = PrincipalNameHydrator<ConcreteRoleDefinitionRepo>;

impl<'a> RbacRuntimeBuilder<'a> {
    /// Start a new builder; call the `with_*` methods in order.
    pub(crate) fn new(ctx: &'a GearCtx, principal_names: PrincipalNamesConfig) -> Self {
        Self {
            ctx,
            principal_names,
            db: None,
            tenant_resolver: None,
            rg_read: None,
            registry: None,
            scope_validator: None,
            role_repo: None,
            assignment_repo: None,
            evaluator: None,
            policy_enforcer: None,
            target_type_validator: None,
            permission_catalog: None,
        }
    }

    /// Acquire the database handle via `ctx.db_required()`.
    pub(crate) fn with_db(mut self) -> Result<Self> {
        let db = self.ctx.db_required().context(
            "rbac: ctx.db_required() failed; the `db` capability is declared but no DbHandle is available",
        )?;
        self.db = Some(db);
        Ok(self)
    }

    /// Resolve upstream `ClientHub` contracts (`TenantResolver`,
    /// `ResourceGroupReadHierarchy`, `TypesRegistry`), build the
    /// `RbacRgRead` adapter and the `ScopeValidator`. RG reads go through
    /// the unscoped, PEP-bypassing `ResourceGroupReadHierarchy` (wrapped
    /// behind [`ResourceGroupReadAdapter`]) so resolving group memberships
    /// while acting as the PDP does not re-enter it.
    pub(crate) fn with_upstream_clients(mut self) -> Result<Self> {
        let hub = self.ctx.client_hub();

        let tenant_resolver: Arc<dyn TenantResolverClient> =
            hub.get::<dyn TenantResolverClient>().map_err(|err| {
                anyhow::anyhow!(
                    "rbac: TenantResolverClient not found in ClientHub; \
                     the tenant-resolver module must be registered before rbac \
                     (deps declaration enforces ordering, but the module also has to be \
                     loaded at server startup): {err:?}"
                )
            })?;

        let rg_read_client: Arc<dyn ResourceGroupReadHierarchy> =
            hub.get::<dyn ResourceGroupReadHierarchy>().map_err(|err| {
                anyhow::anyhow!(
                    "rbac: ResourceGroupReadHierarchy not found in ClientHub; \
                     the resource-group module must be registered before rbac: {err:?}"
                )
            })?;
        let rg_read: Arc<dyn RbacRgRead> = Arc::new(ResourceGroupReadAdapter::new(rg_read_client));

        let registry: Arc<dyn TypesRegistryClient> =
            hub.get::<dyn TypesRegistryClient>().map_err(|err| {
                anyhow::anyhow!(
                    "rbac: TypesRegistryClient not found in ClientHub; \
                     the types-registry module must be registered before \
                     rbac: {err:?}"
                )
            })?;

        let scope_validator = Arc::new(ScopeValidator::new(
            Arc::clone(&tenant_resolver),
            Arc::clone(&rg_read),
        ));

        self.tenant_resolver = Some(tenant_resolver);
        self.rg_read = Some(rg_read);
        self.registry = Some(registry);
        self.scope_validator = Some(scope_validator);
        Ok(self)
    }

    /// Construct the production `SeaORM` repositories.
    pub(crate) fn with_repositories(mut self) -> Self {
        let db = take_required(&mut self.db, "with_db must run before with_repositories");
        // Stateless now: the executor arrives per call, so one instance of
        // each repository serves every caller.
        let role_repo: Arc<ConcreteRoleDefinitionRepo> =
            Arc::new(role_definition_repo::RoleDefinitionRepository);
        let assignment_repo: Arc<ConcreteRoleAssignmentRepo> =
            Arc::new(role_assignment_repo::RoleAssignmentRepository);
        // RBAC has no serve() loop, so refresh the inventory gauges
        // (rbac_role_definitions / rbac_role_assignments) from a detached
        // task spawned here, where `db` + `ctx` are in scope. Read-only
        // COUNTs; observes the module cancellation token for shutdown.
        // TODO: fold into a serve() lifecycle if RBAC ever grows one.
        let inventory_db = db.clone();
        let cancel = self.ctx.cancellation_token().clone();
        tokio::spawn(async move {
            let metrics = RbacMetricsMeter::from_global();
            let mut ticker = tokio::time::interval(INVENTORY_REFRESH_INTERVAL);
            loop {
                tokio::select! {
                    biased;
                    () = cancel.cancelled() => break,
                    _ = ticker.tick() => {
                        match role_definition_repo::count_role_inventory(&inventory_db).await {
                            Ok((defs, assigns)) => metrics.record_inventory(
                                i64::try_from(defs).unwrap_or(i64::MAX),
                                i64::try_from(assigns).unwrap_or(i64::MAX),
                            ),
                            Err(err) => warn!(
                                target: "rbac",
                                error = %err,
                                "rbac inventory gauge refresh failed; gauges not updated this tick"
                            ),
                        }
                    }
                }
            }
        });

        // Put the db back — the seeder/bootstrap still need it.
        self.db = Some(db);
        self.role_repo = Some(role_repo);
        self.assignment_repo = Some(assignment_repo);
        self
    }

    /// Build the [`PermissionEvaluator`] and publish the SDK-facing
    /// [`RbacServiceLocalClient`] in `ClientHub`.
    pub(crate) fn with_evaluator_and_local_client(mut self) -> Self {
        let assignment_repo = clone_required(
            self.assignment_repo.as_ref(),
            "with_repositories must run before with_evaluator_and_local_client",
        );
        let role_repo = clone_required(
            self.role_repo.as_ref(),
            "with_repositories must run before with_evaluator_and_local_client",
        );
        let tenant_resolver = clone_required(
            self.tenant_resolver.as_ref(),
            "with_upstream_clients must run before with_evaluator_and_local_client",
        );
        let rg_read = clone_required(
            self.rg_read.as_ref(),
            "with_upstream_clients must run before with_evaluator_and_local_client",
        );

        let db = clone_required(
            self.db.as_ref(),
            "with_database must run before with_evaluator_and_local_client",
        );
        let evaluator = Arc::new(PermissionEvaluator::new(
            db,
            assignment_repo,
            role_repo,
            tenant_resolver,
            rg_read,
            Arc::new(RbacMetricsMeter::from_global()),
        ));

        let local_client: Arc<dyn RbacServiceClientV1> =
            Arc::new(RbacServiceLocalClient::new(Arc::clone(&evaluator)))
                as Arc<dyn RbacServiceClientV1>;
        self.ctx
            .client_hub()
            .register::<dyn RbacServiceClientV1>(local_client);

        self.evaluator = Some(evaluator);
        self
    }

    /// Build the policy enforcer, target-type validator, and permission
    /// catalog.
    pub(crate) fn with_policy_layer(mut self) -> Self {
        let evaluator = clone_required(
            self.evaluator.as_ref(),
            "with_evaluator_and_local_client must run before with_policy_layer",
        );
        let registry = clone_required(
            self.registry.as_ref(),
            "with_upstream_clients must run before with_policy_layer",
        );

        // `PermissionEvaluator` implements `PolicyEnforcer` directly.
        let policy_enforcer: Arc<dyn PolicyEnforcer> = evaluator;
        // Real target-type validator: concrete targets must resolve to a
        // registered type-schema; wildcard targets (`gts.cf.core.am.*`) are
        // looked up by pattern and fail-open (warn, but pass) when nothing
        // matches yet — see `TypesRegistryTargetTypeValidator`. This is what
        // keeps the not-yet-registered AM/RG wildcard targets from 400ing.
        let target_type_validator: Arc<dyn TargetTypeValidator> =
            Arc::new(TypesRegistryTargetTypeValidator::new(Arc::clone(&registry)));
        // Real permission catalog: listing/existence are served from the
        // types-registry-backed inventory. The role-definition service no
        // longer validates `(operation, target_type)` pairs against it.
        let permission_catalog: Arc<dyn PermissionCatalog> =
            Arc::new(TypesRegistryPermissionCatalog::new(Arc::clone(&registry)));

        self.policy_enforcer = Some(policy_enforcer);
        self.target_type_validator = Some(target_type_validator);
        self.permission_catalog = Some(permission_catalog);
        self
    }

    /// Register the two vendored RBAC entity GTS schemas with the
    /// types-registry.
    pub(crate) async fn register_schemas(self) -> Result<Self> {
        let registry = clone_required(
            self.registry.as_ref(),
            "with_upstream_clients must run before register_schemas",
        );

        let role_definition_schema: serde_json::Value =
            serde_json::from_str(ROLE_DEFINITION_SCHEMA_JSON)
                .context("rbac: failed to parse vendored role_definition.v1.schema.json")?;
        let role_assignment_schema: serde_json::Value =
            serde_json::from_str(ROLE_ASSIGNMENT_SCHEMA_JSON)
                .context("rbac: failed to parse vendored role_assignment.v1.schema.json")?;

        let results = registry
            .register(vec![role_definition_schema, role_assignment_schema])
            .await
            .context(
                "rbac: TypesRegistryClient::register(...) failed for the two \
                 RBAC entity schemas",
            )?;
        for result in results {
            if let RegisterResult::Err { gts_id, error } = result {
                return Err(anyhow::anyhow!(
                    "rbac: failed to register entity schema {} \
                     in types-registry: {error}",
                    gts_id.as_deref().unwrap_or("<unknown gts_id>")
                ));
            }
        }
        info!(
            "rbac: registered entity GTS schemas {} and {}",
            ROLE_DEFINITION_GTS_ID, ROLE_ASSIGNMENT_GTS_ID
        );
        Ok(self)
    }

    /// Consume the builder and produce the assembled [`RbacRuntime`] plus
    /// the database handle `init()` still needs for the seeder and
    /// platform-admin bootstrap.
    pub(crate) fn into_runtime(mut self) -> (RbacRuntime, DBProvider<DbError>) {
        let db = take_required(&mut self.db, "with_db must run before into_runtime");
        let role_repo = clone_required(
            self.role_repo.as_ref(),
            "with_repositories must run before into_runtime",
        );
        let assignment_repo = clone_required(
            self.assignment_repo.as_ref(),
            "with_repositories must run before into_runtime",
        );
        let policy_enforcer = clone_required(
            self.policy_enforcer.as_ref(),
            "with_policy_layer must run before into_runtime",
        );
        let scope_validator = clone_required(
            self.scope_validator.as_ref(),
            "with_upstream_clients must run before into_runtime",
        );
        let target_type_validator = clone_required(
            self.target_type_validator.as_ref(),
            "with_policy_layer must run before into_runtime",
        );
        let permission_catalog = clone_required(
            self.permission_catalog.as_ref(),
            "with_policy_layer must run before into_runtime",
        );
        let rg_read = clone_required(
            self.rg_read.as_ref(),
            "with_upstream_clients must run before into_runtime",
        );

        let tenant_resolver = clone_required(
            self.tenant_resolver.as_ref(),
            "with_upstream_clients must run before into_runtime",
        );

        let deps = WireDeps {
            db: db.clone(),
            repo: role_repo,
            assignment_repo,
            policy_enforcer,
            scope_validator,
            target_type_validator,
            permission_catalog,
            rg_read,
            tenant_resolver,
            // Stored, not consumed: the account-management client is
            // resolved from the hub at request time, never here. See
            // `AmUserNameReader` for why an `init()`-time lookup is not
            // an option (it would need a `deps` edge that closes a cycle).
            client_hub: self.ctx.client_hub(),
            principal_names: self.principal_names.clone(),
        };
        let runtime = RbacRuntime {
            role_definitions: build_role_definitions_api_state(&deps),
            role_assignments: build_role_assignments_api_state(&deps),
            permissions: build_permissions_api_state(&deps),
        };
        (runtime, db)
    }
}

/// Bundle of resolved dependencies consumed by the three `build_*_api_state`
/// helpers; every field is freshly cloned from the builder's slots.
struct WireDeps {
    /// Connection source handed to each service, which owns the
    /// transaction boundary and passes an executor to every repo call.
    db: DBProvider<DbError>,
    repo: Arc<ConcreteRoleDefinitionRepo>,
    assignment_repo: Arc<ConcreteRoleAssignmentRepo>,
    policy_enforcer: Arc<dyn PolicyEnforcer>,
    scope_validator: Arc<ScopeValidator>,
    target_type_validator: Arc<dyn TargetTypeValidator>,
    permission_catalog: Arc<dyn PermissionCatalog>,
    rg_read: Arc<dyn RbacRgRead>,
    /// Also held by the `ScopeValidator`; the display-name hydrator needs
    /// it directly to resolve the platform root tenant for root-scoped
    /// rows.
    tenant_resolver: Arc<dyn TenantResolverClient>,
    /// Kept so late-bound clients (account management) can be resolved on
    /// first use instead of at init.
    client_hub: Arc<toolkit::client_hub::ClientHub>,
    principal_names: PrincipalNamesConfig,
}

fn build_role_definitions_api_state(
    deps: &WireDeps,
) -> Arc<crate::api::rest::role_definitions::ApiState> {
    Arc::new(crate::api::rest::role_definitions::ApiState {
        service: Arc::new(crate::domain::role_definition::RoleDefinitionService::new(
            deps.db.clone(),
            Arc::clone(&deps.repo),
            // Read-only here: role-definition reads carry a per-role
            // assignment count, which is an aggregate over `role_assignments`
            // taken under the caller's own readable scopes.
            Arc::clone(&deps.assignment_repo),
            Arc::clone(&deps.policy_enforcer),
            Arc::clone(&deps.scope_validator),
            Arc::clone(&deps.target_type_validator),
        )),
    })
}

fn build_role_assignments_api_state(
    deps: &WireDeps,
) -> Arc<crate::api::rest::role_assignments::ApiState> {
    let service = crate::domain::role_assignment::RoleAssignmentService::new(
        deps.db.clone(),
        Arc::clone(&deps.assignment_repo),
        Arc::clone(&deps.repo),
        Arc::clone(&deps.policy_enforcer),
        Arc::clone(&deps.scope_validator),
        Arc::clone(&deps.rg_read),
    );
    // Display-name hydration is additive and optional: with it switched
    // off the service serves exactly the rows it served before. No
    // upstream client is resolved here — `AmUserNameReader` looks the
    // account-management client up on first use, because RBAC cannot
    // declare a `deps` edge to a gear that transitively depends on RBAC.
    //
    // The same switch also governs the role-definition name, which needs no
    // upstream at all. One flag for "decorate reads with display names" is
    // the behaviour an operator can reason about; a second flag for the one
    // name that cannot fail would be a knob nobody would ever turn.
    let service = if deps.principal_names.enabled {
        let reader: Arc<dyn PrincipalNameReader> = Arc::new(AmUserNameReader::new(
            Arc::clone(&deps.client_hub),
            deps.principal_names.clone(),
        ));
        service.with_hydrator(Arc::new(
            PrincipalNameHydrator::new(
                deps.db.clone(),
                reader,
                Arc::clone(&deps.rg_read),
                // The role-definition store the service already holds for
                // assignable-scope checks on create. Reusing it keeps the
                // role-name read on the same rows, chunking and access scope as
                // every other role-definition read in the gear.
                Arc::clone(&deps.repo),
                Arc::clone(&deps.tenant_resolver),
                Arc::new(RbacMetricsMeter::from_global()),
            )
            // Without this the resolve deadline and the per-request tenant
            // budget silently run at their compiled-in defaults and every
            // operator override of those two fields is ignored.
            .with_limits(&deps.principal_names),
        ))
    } else {
        info!(
            "rbac: display-name resolution is disabled by config; \
             role-assignment reads will carry principal, author and role \
             definition ids only"
        );
        service
    };
    Arc::new(crate::api::rest::role_assignments::ApiState {
        service: Arc::new(service),
    })
}

fn build_permissions_api_state(deps: &WireDeps) -> Arc<crate::api::rest::permissions::ApiState> {
    Arc::new(crate::api::rest::permissions::ApiState {
        catalog: Arc::clone(&deps.permission_catalog),
    })
}

/// Take an `Option` slot whose presence is a static call-ordering
/// invariant. Out-of-order builder calls are programmer errors and surface
/// as `unreachable!` rather than typed errors.
fn take_required<T>(slot: &mut Option<T>, precondition: &'static str) -> T {
    match slot.take() {
        Some(value) => value,
        None => unreachable!("rbac module builder precondition violated: {precondition}"),
    }
}

/// Clone-by-reference variant of [`take_required`].
fn clone_required<T: Clone>(slot: Option<&T>, precondition: &'static str) -> T {
    match slot {
        Some(value) => value.clone(),
        None => unreachable!("rbac module builder precondition violated: {precondition}"),
    }
}

/// Warn when `builtin_role_targets.platform` covers none of RBAC's own types.
///
/// `Owner`'s single rule is a wildcard over that list, and RBAC's
/// `role_definition` / `role_assignment` types are `gts.cf.core.…` regardless of
/// which vendor the deployment publishes under. A platform list that names only
/// a fork's own family therefore produces an `Owner` who cannot create role
/// assignments — including the very grants that would fix it. The failure is
/// otherwise silent and only shows up as a denied `POST /rbac/v1/role-assignments`
/// long after startup.
///
/// A warning rather than an error: a deployment may deliberately keep RBAC
/// administration on `User Access Administrator` instead, whose targets are
/// fixed and unaffected by this setting.
fn warn_if_owner_cannot_administer_rbac(cfg: &RbacServiceConfig) {
    let covered = cfg.builtin_role_targets.platform.iter().any(|target| {
        crate::domain::permission_matcher::matches_target_type(
            target,
            crate::domain::resource_types::ROLE_ASSIGNMENT,
        )
    });
    if !covered {
        warn!(
            platform_targets = ?cfg.builtin_role_targets.platform,
            role_assignment_type = crate::domain::resource_types::ROLE_ASSIGNMENT,
            "rbac: no `builtin_role_targets.platform` entry covers RBAC's own \
             role_assignment type, so the built-in Owner cannot administer role \
             assignments; add the family that covers it (the default `gts.cf.*` \
             does) or rely on User Access Administrator instead"
        );
    }
}
