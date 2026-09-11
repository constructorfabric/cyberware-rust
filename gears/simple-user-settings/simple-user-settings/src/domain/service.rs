use std::sync::{Arc, OnceLock};

use authz_resolver_sdk::PolicyEnforcer;
use authz_resolver_sdk::pep::{AccessRequest, ResourceType};
use simple_user_settings_sdk::models::{
    SimpleUserSettings, SimpleUserSettingsPatch, SimpleUserSettingsUpdate,
};
use simple_user_settings_sdk::owner::SettingsOwnerResolver;
use toolkit_db::DBProvider;
use toolkit_macros::domain_model;
use toolkit_security::{SecurityContext, pep_properties};
use uuid::Uuid;

use super::error::DomainError;
use super::fields::SettingsFields;
use super::repo::SettingsRepository;

pub(crate) type DbProvider = DBProvider<toolkit_db::DbError>;

/// Authorization resource type for user settings.
///
/// Settings are scoped by tenant + user (resource). The PDP uses
/// `supported_properties` to decide which predicates it can return.
pub(crate) const SETTINGS_RESOURCE: ResourceType = ResourceType::from_static(
    "simple_user_settings.settings",
    &[pep_properties::OWNER_TENANT_ID, pep_properties::RESOURCE_ID],
);

pub(crate) mod actions {
    pub const GET: &str = "get";
    pub const UPDATE: &str = "update";
}

// ============================================================================
// Service Configuration
// ============================================================================

#[domain_model]
pub struct ServiceConfig {
    pub max_field_length: usize,
}

impl Default for ServiceConfig {
    fn default() -> Self {
        Self {
            max_field_length: 100,
        }
    }
}

// ============================================================================
// Service Implementation
// ============================================================================

#[domain_model]
pub struct Service<R: SettingsRepository> {
    db: Arc<DbProvider>,
    repo: Arc<R>,
    policy_enforcer: PolicyEnforcer,
    config: ServiceConfig,
    /// Supplied by the deployment in the REST phase; `Some(None)` means it
    /// published none and the token subject is the key.
    owner_resolver: OnceLock<Option<Arc<dyn SettingsOwnerResolver>>>,
}

impl<R: SettingsRepository> Service<R> {
    pub fn new(
        db: Arc<DbProvider>,
        repo: Arc<R>,
        policy_enforcer: PolicyEnforcer,
        config: ServiceConfig,
    ) -> Self {
        Self {
            db,
            repo,
            policy_enforcer,
            config,
            owner_resolver: OnceLock::new(),
        }
    }

    /// Hand the service the deployment's view of who a caller is.
    ///
    /// Separate from `new` because the resolver comes from another gear, and
    /// the REST phase is the first point at which every gear is known to have
    /// initialized. Calling it twice is ignored.
    pub fn attach_owner_resolver(&self, resolver: Option<Arc<dyn SettingsOwnerResolver>>) {
        if self.owner_resolver.set(resolver).is_err() {
            tracing::debug!("settings owner resolver already attached; keeping the first");
        }
    }

    /// The key this caller's settings are filed under.
    ///
    /// The token subject unless the deployment published a resolver that knows
    /// better — see [`SettingsOwnerResolver`]. One place decides it, so a read
    /// and a write can never disagree about whose settings they are.
    async fn owner_of(&self, ctx: &SecurityContext) -> Result<Uuid, DomainError> {
        let Some(Some(resolver)) = self.owner_resolver.get() else {
            return Ok(ctx.subject_id());
        };
        Ok(resolver
            .settings_owner(ctx)
            .await
            .map_err(DomainError::from)?
            .unwrap_or_else(|| ctx.subject_id()))
    }

    pub async fn get_settings(
        &self,
        ctx: &SecurityContext,
    ) -> Result<SimpleUserSettings, DomainError> {
        let user_id = self.owner_of(ctx).await?;
        let tenant_id = ctx.subject_tenant_id();

        let scope = self
            .policy_enforcer
            .access_scope_with(
                ctx,
                &SETTINGS_RESOURCE,
                actions::GET,
                Some(user_id),
                &AccessRequest::new().resource_property(pep_properties::OWNER_TENANT_ID, tenant_id),
            )
            .await?;

        let conn = self.db.conn().map_err(DomainError::from)?;

        if let Some(settings) = self.repo.find_by_user(&conn, &scope).await? {
            Ok(settings)
        } else {
            Ok(SimpleUserSettings {
                user_id,
                tenant_id,
                theme: None,
                language: None,
            })
        }
    }

    pub async fn update_settings(
        &self,
        ctx: &SecurityContext,
        update: SimpleUserSettingsUpdate,
    ) -> Result<SimpleUserSettings, DomainError> {
        self.validate_field(SettingsFields::THEME, &update.theme)?;
        self.validate_field(SettingsFields::LANGUAGE, &update.language)?;

        let user_id = self.owner_of(ctx).await?;
        let tenant_id = ctx.subject_tenant_id();

        let scope = self
            .policy_enforcer
            .access_scope_with(
                ctx,
                &SETTINGS_RESOURCE,
                actions::UPDATE,
                Some(user_id),
                &AccessRequest::new().resource_property(pep_properties::OWNER_TENANT_ID, tenant_id),
            )
            .await?;

        let conn = self.db.conn().map_err(DomainError::from)?;

        let settings = self
            .repo
            .upsert_full(
                &conn,
                &scope,
                user_id,
                tenant_id,
                Some(update.theme),
                Some(update.language),
            )
            .await?;
        Ok(settings)
    }

    pub async fn patch_settings(
        &self,
        ctx: &SecurityContext,
        patch: SimpleUserSettingsPatch,
    ) -> Result<SimpleUserSettings, DomainError> {
        if let Some(ref theme) = patch.theme {
            self.validate_field(SettingsFields::THEME, theme)?;
        }
        if let Some(ref language) = patch.language {
            self.validate_field(SettingsFields::LANGUAGE, language)?;
        }

        let user_id = self.owner_of(ctx).await?;
        let tenant_id = ctx.subject_tenant_id();

        let scope = self
            .policy_enforcer
            .access_scope_with(
                ctx,
                &SETTINGS_RESOURCE,
                actions::UPDATE,
                Some(user_id),
                &AccessRequest::new().resource_property(pep_properties::OWNER_TENANT_ID, tenant_id),
            )
            .await?;

        let conn = self.db.conn().map_err(DomainError::from)?;

        let settings = self
            .repo
            .upsert_patch(&conn, &scope, user_id, tenant_id, patch)
            .await?;
        Ok(settings)
    }

    fn validate_field(&self, field: &str, value: &str) -> Result<(), DomainError> {
        if value.len() > self.config.max_field_length {
            return Err(DomainError::validation(
                field,
                format!("exceeds maximum length of {}", self.config.max_field_length),
            ));
        }
        Ok(())
    }
}
