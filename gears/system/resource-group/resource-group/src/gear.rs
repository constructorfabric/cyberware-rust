// Created: 2026-04-16 by Constructor Tech
// Updated: 2026-04-28 by Constructor Tech
// @cpt-dod:cpt-cf-resource-group-dod-e2e-test-suite:p1
use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use authz_resolver_sdk::{AuthZResolverApi, PolicyEnforcer};
use resource_group_sdk::{
    ResourceGroupClient, ResourceGroupReadHierarchy, ResourceGroupTypeBootstrap,
};
use sea_orm_migration::MigrationTrait;
use toolkit::api::OpenApiRegistry;
use toolkit::{DatabaseCapability, Gear, GearCtx, RestApiCapability};
use toolkit_db::DBProvider;
use toolkit_db::DbError;
use tracing::info;

use crate::api::rest::routes;
use crate::domain::group_service::{GroupService, QueryProfile};
use crate::domain::membership_service::MembershipService;
use crate::domain::read_service::RgReadService;
use crate::domain::rg_service::RgService;
use crate::domain::type_bootstrap_service::RgTypeBootstrapService;
use crate::domain::type_service::TypeService;
use crate::infra::storage::group_repo::GroupRepository;
use crate::infra::storage::membership_repo::MembershipRepository;
use crate::infra::storage::type_repo::TypeRepository;

pub type ConcreteTypeService = TypeService<TypeRepository>;
pub type ConcreteGroupService = GroupService<GroupRepository, TypeRepository>;
pub type ConcreteMembershipService =
    MembershipService<GroupRepository, TypeRepository, MembershipRepository>;
pub type ConcreteRgService = RgService<GroupRepository, TypeRepository, MembershipRepository>;
pub type ConcreteTypeBootstrapService = RgTypeBootstrapService<TypeRepository>;

// @cpt-dod:cpt-cf-resource-group-dod-sdk-foundation-gear-scaffold:p1
/// Main gear struct for the resource-group gear.
#[toolkit::gear(
    name = "resource-group",
    deps = [authz_resolver, types_registry],
    capabilities = [db, rest]
)]
#[allow(clippy::struct_field_names)]
pub struct ResourceGroup {
    type_service: OnceLock<Arc<ConcreteTypeService>>,
    group_service: OnceLock<Arc<ConcreteGroupService>>,
    membership_service: OnceLock<Arc<ConcreteMembershipService>>,
    /// Kept as the concrete type, not just the `Arc<dyn
    /// ResourceGroupTypeBootstrap>` handed to `ClientHub` -- `register_rest`
    /// needs to call `seal()` on this exact instance once the bootstrap
    /// window closes, and that method isn't part of the SDK trait object.
    type_bootstrap: OnceLock<Arc<ConcreteTypeBootstrapService>>,
}

impl Default for ResourceGroup {
    fn default() -> Self {
        Self {
            type_service: OnceLock::new(),
            group_service: OnceLock::new(),
            membership_service: OnceLock::new(),
            type_bootstrap: OnceLock::new(),
        }
    }
}

#[async_trait]
impl Gear for ResourceGroup {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        // Acquire DB capability (secure wrapper)
        let db: Arc<DBProvider<DbError>> = Arc::new(ctx.db_required()?);

        // Resolve AuthZ client from ClientHub and create PolicyEnforcer
        let authz = ctx
            .client_hub()
            .get::<dyn AuthZResolverApi>()
            .map_err(|e| anyhow::anyhow!("failed to get AuthZ resolver: {e}"))?;
        // RG owns the canonical membership and closure tables, but none of
        // its own resource descriptors carries an RG member-handle type
        // mapping: groups are nested through `parent_id`/`resource_group_closure`,
        // not membership rows, so there is no `gts_type` row a native
        // `in_group`/`in_group_subtree` predicate against groups could
        // qualify. Advertising the group capabilities here would be inert —
        // `PolicyEnforcer` suppresses them per request for unmapped
        // descriptors — so none are advertised and the PDP must expand group
        // scopes to explicit `in` predicates, or deny.
        let enforcer = PolicyEnforcer::new(authz);

        // Create repo instances
        let group_repo = Arc::new(GroupRepository);
        let type_repo = Arc::new(TypeRepository);
        let membership_repo = Arc::new(MembershipRepository);

        // Create TypeService
        let type_service = Arc::new(TypeService::new(
            db.clone(),
            enforcer.clone(),
            type_repo.clone(),
        ));

        self.type_service
            .set(type_service)
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;

        // Resolve TypesRegistryClient for GTS metadata validation
        let types_registry: Arc<dyn types_registry_sdk::TypesRegistryClient> = ctx
            .client_hub()
            .get::<dyn types_registry_sdk::TypesRegistryClient>()
            .map_err(|e| anyhow::anyhow!("failed to get TypesRegistryClient: {e}"))?;

        // Create GroupService with default query profile and PolicyEnforcer
        let profile = QueryProfile::default();
        // The composition root is where the metrics recorder is chosen. The
        // service itself defaults to the no-op one, so nothing that builds a
        // `GroupService` outside this path -- tests, mostly -- records or
        // pays for anything.
        let group_service = Arc::new(
            GroupService::new(
                db.clone(),
                profile,
                enforcer.clone(),
                group_repo.clone(),
                type_repo.clone(),
                types_registry,
            )
            .with_metrics(Arc::new(
                crate::infra::metrics::RgMetricsMeter::from_global(),
            )),
        );

        self.group_service
            .set(group_service)
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;

        // Create MembershipService with PolicyEnforcer for AuthZ enforcement
        let membership_service = Arc::new(MembershipService::new(
            db,
            enforcer,
            group_repo,
            type_repo,
            membership_repo,
        ));
        self.membership_service
            .set(membership_service.clone())
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;

        // Phase 1 (SystemCapability): register SDK clients in ClientHub
        let type_svc = self
            .type_service
            .get()
            .ok_or_else(|| anyhow::anyhow!("{} type_service not initialized", Self::MODULE_NAME))?
            .clone();
        let group_svc = self
            .group_service
            .get()
            .ok_or_else(|| anyhow::anyhow!("{} group_service not initialized", Self::MODULE_NAME))?
            .clone();

        let rg_client: Arc<dyn ResourceGroupClient> = Arc::new(RgService::new(
            type_svc.clone(),
            group_svc.clone(),
            membership_service.clone(),
        ));
        ctx.client_hub()
            .register::<dyn ResourceGroupClient>(rg_client);

        let read_client: Arc<dyn ResourceGroupReadHierarchy> =
            Arc::new(RgReadService::new(group_svc, membership_service));
        ctx.client_hub()
            .register::<dyn ResourceGroupReadHierarchy>(read_client);

        // TEMPORARY: bootstrap-only, un-gated surface for another gear's
        // `init` to register RG types before AuthZ is reachable — see
        // `ResourceGroupTypeBootstrap`'s doc comment ("Temporary — revisit
        // when the type registry becomes its own gear"). Kept in its own
        // `OnceLock` as the concrete type, not just the `Arc<dyn
        // ResourceGroupTypeBootstrap>` handed to `ClientHub` below --
        // `register_rest` calls `.seal()` on this exact instance once the
        // bootstrap window closes.
        let type_bootstrap = Arc::new(RgTypeBootstrapService::new(type_svc));
        self.type_bootstrap
            .set(type_bootstrap.clone())
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;
        let type_bootstrap_client: Arc<dyn ResourceGroupTypeBootstrap> = type_bootstrap;
        ctx.client_hub()
            .register::<dyn ResourceGroupTypeBootstrap>(type_bootstrap_client);

        info!(
            "Resource Group gear initialized (ClientHub: ResourceGroupClient + ResourceGroupReadHierarchy + ResourceGroupTypeBootstrap)"
        );
        Ok(())
    }
}

impl DatabaseCapability for ResourceGroup {
    fn migrations(&self) -> Vec<Box<dyn MigrationTrait>> {
        use sea_orm_migration::MigratorTrait;
        info!("Providing resource_group database migrations");
        crate::infra::storage::migrations::Migrator::migrations()
    }
}

impl RestApiCapability for ResourceGroup {
    fn register_rest(
        &self,
        ctx: &GearCtx,
        router: axum::Router,
        openapi: &dyn OpenApiRegistry,
    ) -> anyhow::Result<axum::Router> {
        info!("Registering resource_group REST routes");

        // The bootstrap window closes here. The toolkit runtime calls
        // `register_rest` only after every gear's `init` and `post_init`
        // have completed -- in both the in-process and out-of-process
        // profiles -- so the one sanctioned caller
        // (account-management's `Gear::init`) has already run by now.
        // `ClientHub::remove` stops a future lookup; `seal()` on the kept
        // instance also fails every method for a caller that resolved and
        // held onto an `Arc` from inside the window, which removal alone
        // would not reach. See `ResourceGroupTypeBootstrap`'s doc comment.
        ctx.client_hub().remove::<dyn ResourceGroupTypeBootstrap>();
        if let Some(type_bootstrap) = self.type_bootstrap.get() {
            type_bootstrap.seal();
        }

        let type_service = self
            .type_service
            .get()
            .ok_or_else(|| anyhow::anyhow!("TypeService not initialized"))?
            .clone();

        let group_service = self
            .group_service
            .get()
            .ok_or_else(|| anyhow::anyhow!("GroupService not initialized"))?
            .clone();

        let membership_service = self
            .membership_service
            .get()
            .ok_or_else(|| anyhow::anyhow!("MembershipService not initialized"))?
            .clone();

        let router = routes::register_routes(
            router,
            openapi,
            type_service,
            group_service,
            membership_service,
        );

        info!("Resource Group REST routes registered successfully");
        Ok(router)
    }
}
