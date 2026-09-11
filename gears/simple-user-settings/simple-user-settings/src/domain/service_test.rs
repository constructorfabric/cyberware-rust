//! Integration tests for the Settings service.
//!
//! These tests use an in-memory `SQLite` database since `DBRunner` is a sealed trait
//! and cannot be mocked. All tests use real database operations.

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use async_trait::async_trait;
    use authz_resolver_sdk::{
        AuthZResolverApi, PolicyEnforcer,
        constraints::{Constraint, InPredicate, Predicate},
        models::{EvaluationRequest, EvaluationResponse, EvaluationResponseContext},
    };
    use simple_user_settings_sdk::models::{SimpleUserSettingsPatch, SimpleUserSettingsUpdate};
    use toolkit::api::canonical_prelude::CanonicalError;
    use toolkit_db::migration_runner::run_migrations_for_testing;
    use toolkit_db::{ConnectOpts, DBProvider, Db, connect_db};
    use toolkit_security::{PlatformSecurityContext, SecurityContext, pep_properties};
    use uuid::Uuid;

    use crate::domain::error::DomainError;
    use crate::domain::service::{Service, ServiceConfig};
    use crate::infra::storage::migrations::Migrator;
    use crate::infra::storage::sea_orm_repo::SeaOrmSettingsRepository;

    type ConcreteService = Service<SeaOrmSettingsRepository>;

    /// Mock `AuthZ` resolver for personal user settings.
    ///
    /// Derives tenant from `context.tenant_context.root_id` if present,
    /// otherwise falls back to `subject.properties.tenant_id` (like a real PDP).
    /// Always returns:
    /// - `OWNER_TENANT_ID` constraint from the resolved tenant
    /// - `RESOURCE_ID` constraint from `resource.id` (the user whose settings are accessed)
    struct MockAuthZResolver;

    #[async_trait]
    impl AuthZResolverApi for MockAuthZResolver {
        async fn evaluate(
            &self,
            _ctx: PlatformSecurityContext,
            request: EvaluationRequest,
        ) -> Result<EvaluationResponse, CanonicalError> {
            // Resolve tenant: explicit context > subject property (like a real PDP)
            let root_id = request
                .context
                .tenant_context
                .as_ref()
                .and_then(|tc| tc.root_id)
                .or_else(|| {
                    request
                        .subject
                        .properties
                        .get("tenant_id")
                        .and_then(|v| v.as_str())
                        .and_then(|s| Uuid::parse_str(s).ok())
                })
                .ok_or_else(|| {
                    CanonicalError::internal("tenant context is required".to_owned()).create()
                })?;

            let mut predicates = vec![Predicate::In(InPredicate::new(
                pep_properties::OWNER_TENANT_ID,
                [root_id],
            ))];

            // Use resource.id for RESOURCE_ID constraint
            if let Some(resource_id) = request.resource.id {
                predicates.push(Predicate::In(InPredicate::new(
                    pep_properties::RESOURCE_ID,
                    [resource_id],
                )));
            }

            Ok(EvaluationResponse {
                decision: true,
                context: EvaluationResponseContext {
                    constraints: vec![Constraint { predicates }],
                    ..Default::default()
                },
            })
        }
    }

    /// Create an in-memory database with migrations applied.
    async fn inmem_db() -> Db {
        use sea_orm_migration::MigratorTrait;

        let opts = ConnectOpts {
            max_conns: Some(1),
            min_conns: Some(1),
            ..Default::default()
        };
        let db = connect_db("sqlite::memory:", opts)
            .await
            .expect("Failed to connect to in-memory database");

        run_migrations_for_testing(&db, Migrator::migrations())
            .await
            .expect("Failed to run migrations");

        db
    }

    fn create_test_context() -> SecurityContext {
        SecurityContext::builder()
            .subject_id(Uuid::new_v4())
            .subject_tenant_id(Uuid::new_v4())
            .build()
            .unwrap()
    }

    /// A caller in a named organization.
    ///
    /// Settings are keyed on `(user, tenant)`, so a test about *whose* settings
    /// these are has to hold the tenant still or it measures both at once.
    fn context_in(tenant_id: Uuid) -> SecurityContext {
        SecurityContext::builder()
            .subject_id(Uuid::new_v4())
            .subject_tenant_id(tenant_id)
            .build()
            .unwrap()
    }

    fn build_service(db: Db, config: ServiceConfig) -> ConcreteService {
        let repo = Arc::new(SeaOrmSettingsRepository::new());
        let db: Arc<DBProvider<toolkit_db::DbError>> = Arc::new(DBProvider::new(db));
        let authz: Arc<dyn AuthZResolverApi> = Arc::new(MockAuthZResolver);
        let policy_enforcer = PolicyEnforcer::new(authz);
        Service::new(db, repo, policy_enforcer, config)
    }

    /// A deployment whose people hold more than one login.
    ///
    /// Answers with one fixed key for every caller, which is the shape that
    /// matters: two different subjects must reach the same settings.
    struct OnePerson(Option<Uuid>);

    #[async_trait]
    impl simple_user_settings_sdk::SettingsOwnerResolver for OnePerson {
        async fn settings_owner(
            &self,
            _ctx: &SecurityContext,
        ) -> Result<Option<Uuid>, CanonicalError> {
            Ok(self.0)
        }
    }

    /// A resolver that is having a bad day.
    struct Broken;

    #[async_trait]
    impl simple_user_settings_sdk::SettingsOwnerResolver for Broken {
        async fn settings_owner(
            &self,
            _ctx: &SecurityContext,
        ) -> Result<Option<Uuid>, CanonicalError> {
            Err(CanonicalError::internal("directory unavailable").create())
        }
    }

    // =========================================================================
    // whose settings these are
    // =========================================================================

    /// The point of the resolver: one human, two logins, one set of settings.
    #[tokio::test]
    async fn two_subjects_resolving_to_one_person_share_their_settings() {
        let db = inmem_db().await;
        let service = build_service(db, ServiceConfig::default());
        let person = Uuid::new_v4();
        service.attach_owner_resolver(Some(Arc::new(OnePerson(Some(person)))));

        let org = Uuid::new_v4();
        let first_login = context_in(org);
        service
            .update_settings(
                &first_login,
                SimpleUserSettingsUpdate {
                    theme: "dark".to_owned(),
                    language: "en".to_owned(),
                },
            )
            .await
            .expect("stored under the person");

        // A different subject entirely — the same human signing in the other way.
        let second_login = context_in(org);
        assert_ne!(first_login.subject_id(), second_login.subject_id());

        let seen = service
            .get_settings(&second_login)
            .await
            .expect("read under the person");
        assert_eq!(seen.theme.as_deref(), Some("dark"));
        assert_eq!(seen.user_id, person, "settings belong to the person");
    }

    /// Without a resolver the gear behaves exactly as it always has, which is
    /// what makes this addition safe for every deployment that has one login
    /// per human.
    #[tokio::test]
    async fn with_no_resolver_the_subject_is_still_the_key() {
        let db = inmem_db().await;
        let service = build_service(db, ServiceConfig::default());
        service.attach_owner_resolver(None);

        let ctx = create_test_context();
        let stored = service
            .update_settings(
                &ctx,
                SimpleUserSettingsUpdate {
                    theme: "light".to_owned(),
                    language: "en".to_owned(),
                },
            )
            .await
            .expect("stored");
        assert_eq!(stored.user_id, ctx.subject_id());
    }

    /// A resolver with no opinion about this caller yields to the subject
    /// rather than denying somebody their preferences.
    #[tokio::test]
    async fn a_resolver_that_declines_falls_back_to_the_subject() {
        let db = inmem_db().await;
        let service = build_service(db, ServiceConfig::default());
        service.attach_owner_resolver(Some(Arc::new(OnePerson(None))));

        let ctx = create_test_context();
        let stored = service
            .update_settings(
                &ctx,
                SimpleUserSettingsUpdate {
                    theme: "light".to_owned(),
                    language: "en".to_owned(),
                },
            )
            .await
            .expect("stored");
        assert_eq!(stored.user_id, ctx.subject_id());
    }

    /// A resolver that fails is reported, not guessed around: filing settings
    /// under a key the next request will not produce loses them silently.
    #[tokio::test]
    async fn a_resolver_that_fails_fails_the_request() {
        let db = inmem_db().await;
        let service = build_service(db, ServiceConfig::default());
        service.attach_owner_resolver(Some(Arc::new(Broken)));

        let err = service
            .get_settings(&create_test_context())
            .await
            .expect_err("the read must not fall back");
        assert!(matches!(err, DomainError::Internal(_)), "got {err:?}");
    }

    // =========================================================================
    // get_settings tests
    // =========================================================================

    #[tokio::test]
    async fn test_get_settings_returns_defaults_when_not_found() {
        let db = inmem_db().await;
        let service = build_service(db, ServiceConfig::default());
        let ctx = create_test_context();

        let result = service.get_settings(&ctx).await.unwrap();

        assert_eq!(result.user_id, ctx.subject_id());
        assert_eq!(result.tenant_id, ctx.subject_tenant_id());
        assert_eq!(result.theme, None);
        assert_eq!(result.language, None);
    }

    #[tokio::test]
    async fn test_get_settings_returns_existing() {
        let db = inmem_db().await;
        let service = build_service(db, ServiceConfig::default());
        let ctx = create_test_context();

        // First, create settings
        let _ = service
            .update_settings(
                &ctx,
                SimpleUserSettingsUpdate {
                    theme: "dark".to_owned(),
                    language: "en".to_owned(),
                },
            )
            .await
            .unwrap();

        // Then retrieve them
        let result = service.get_settings(&ctx).await.unwrap();

        assert_eq!(result.theme, Some("dark".to_owned()));
        assert_eq!(result.language, Some("en".to_owned()));
    }

    // =========================================================================
    // update_settings tests
    // =========================================================================

    #[tokio::test]
    async fn test_update_settings_success() {
        let db = inmem_db().await;
        let service = build_service(db, ServiceConfig::default());
        let ctx = create_test_context();

        let result = service
            .update_settings(
                &ctx,
                SimpleUserSettingsUpdate {
                    theme: "light".to_owned(),
                    language: "es".to_owned(),
                },
            )
            .await
            .unwrap();

        assert_eq!(result.theme, Some("light".to_owned()));
        assert_eq!(result.language, Some("es".to_owned()));
        assert_eq!(result.user_id, ctx.subject_id());
        assert_eq!(result.tenant_id, ctx.subject_tenant_id());
    }

    #[tokio::test]
    async fn test_update_settings_validates_max_length_for_theme() {
        let db = inmem_db().await;
        let service = build_service(
            db,
            ServiceConfig {
                max_field_length: 10,
            },
        );
        let ctx = create_test_context();

        let too_long = "a".repeat(11);
        let result = service
            .update_settings(
                &ctx,
                SimpleUserSettingsUpdate {
                    theme: too_long,
                    language: "en".to_owned(),
                },
            )
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, DomainError::Validation { field, .. } if field == "theme"));
    }

    #[tokio::test]
    async fn test_update_settings_validates_max_length_for_language() {
        let db = inmem_db().await;
        let service = build_service(
            db,
            ServiceConfig {
                max_field_length: 10,
            },
        );
        let ctx = create_test_context();

        let too_long = "a".repeat(11);
        let result = service
            .update_settings(
                &ctx,
                SimpleUserSettingsUpdate {
                    theme: "dark".to_owned(),
                    language: too_long,
                },
            )
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, DomainError::Validation { field, .. } if field == "language"));
    }

    // =========================================================================
    // patch_settings tests
    // =========================================================================

    #[tokio::test]
    async fn test_patch_settings_updates_only_provided_fields() {
        let db = inmem_db().await;
        let service = build_service(db, ServiceConfig::default());
        let ctx = create_test_context();

        // First create initial settings
        let _ = service
            .update_settings(
                &ctx,
                SimpleUserSettingsUpdate {
                    theme: "dark".to_owned(),
                    language: "en".to_owned(),
                },
            )
            .await
            .unwrap();

        // Patch only theme
        let result = service
            .patch_settings(
                &ctx,
                SimpleUserSettingsPatch {
                    theme: Some("light".to_owned()),
                    language: None,
                },
            )
            .await
            .unwrap();

        assert_eq!(result.theme, Some("light".to_owned()));
        assert_eq!(result.language, Some("en".to_owned())); // Should remain unchanged
    }

    #[tokio::test]
    async fn test_patch_settings_validates_max_length() {
        let db = inmem_db().await;
        let service = build_service(
            db,
            ServiceConfig {
                max_field_length: 10,
            },
        );
        let ctx = create_test_context();

        let too_long = "a".repeat(11);
        let result = service
            .patch_settings(
                &ctx,
                SimpleUserSettingsPatch {
                    theme: None,
                    language: Some(too_long),
                },
            )
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, DomainError::Validation { field, .. } if field == "language"));
    }

    #[tokio::test]
    async fn test_patch_settings_empty_patch_returns_existing() {
        let db = inmem_db().await;
        let service = build_service(db, ServiceConfig::default());
        let ctx = create_test_context();

        // First create settings
        let _ = service
            .update_settings(
                &ctx,
                SimpleUserSettingsUpdate {
                    theme: "dark".to_owned(),
                    language: "en".to_owned(),
                },
            )
            .await
            .unwrap();

        // Empty patch - no fields to update
        let result = service
            .patch_settings(
                &ctx,
                SimpleUserSettingsPatch {
                    theme: None,
                    language: None,
                },
            )
            .await
            .unwrap();

        // Should return existing values unchanged
        assert_eq!(result.theme, Some("dark".to_owned()));
        assert_eq!(result.language, Some("en".to_owned()));
    }

    #[tokio::test]
    async fn test_patch_settings_creates_if_not_exists() {
        let db = inmem_db().await;
        let service = build_service(db, ServiceConfig::default());
        let ctx = create_test_context();

        // Patch without existing settings
        let result = service
            .patch_settings(
                &ctx,
                SimpleUserSettingsPatch {
                    theme: Some("dark".to_owned()),
                    language: None,
                },
            )
            .await
            .unwrap();

        assert_eq!(result.theme, Some("dark".to_owned()));
        assert_eq!(result.language, None);
    }

    // =========================================================================
    // Tenant isolation tests
    // =========================================================================

    #[tokio::test]
    async fn test_settings_isolated_by_user() {
        let db = inmem_db().await;
        let service = build_service(db, ServiceConfig::default());

        let tenant_id = Uuid::new_v4();
        let user1 = SecurityContext::builder()
            .subject_id(Uuid::new_v4())
            .subject_tenant_id(tenant_id)
            .build()
            .unwrap();
        let user2 = SecurityContext::builder()
            .subject_id(Uuid::new_v4())
            .subject_tenant_id(tenant_id)
            .build()
            .unwrap();

        // User 1 creates settings
        let _ = service
            .update_settings(
                &user1,
                SimpleUserSettingsUpdate {
                    theme: "dark".to_owned(),
                    language: "en".to_owned(),
                },
            )
            .await
            .unwrap();

        // User 2 should get default settings
        let result = service.get_settings(&user2).await.unwrap();
        assert_eq!(result.theme, None);
        assert_eq!(result.language, None);
        assert_eq!(result.user_id, user2.subject_id());
    }

    #[tokio::test]
    async fn test_settings_isolated_by_tenant() {
        let db = inmem_db().await;
        let service = build_service(db, ServiceConfig::default());

        let user_id = Uuid::new_v4();
        let tenant1 = SecurityContext::builder()
            .subject_id(user_id)
            .subject_tenant_id(Uuid::new_v4())
            .build()
            .unwrap();
        let tenant2 = SecurityContext::builder()
            .subject_id(user_id)
            .subject_tenant_id(Uuid::new_v4())
            .build()
            .unwrap();

        // Same user in tenant 1 creates settings
        let _ = service
            .update_settings(
                &tenant1,
                SimpleUserSettingsUpdate {
                    theme: "dark".to_owned(),
                    language: "en".to_owned(),
                },
            )
            .await
            .unwrap();

        // Same user in tenant 2 should get default settings
        let result = service.get_settings(&tenant2).await.unwrap();
        assert_eq!(result.theme, None);
        assert_eq!(result.language, None);
        assert_eq!(result.tenant_id, tenant2.subject_tenant_id());
    }
}
