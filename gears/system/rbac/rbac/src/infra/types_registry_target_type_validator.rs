//! Production [`TargetTypeValidator`] backed by `dyn TypesRegistryClient`.
//!
//! **Concrete target** — delegates to `get_type_schema(target_type)`; maps
//! `GtsTypeSchemaNotFound` / `InvalidGtsTypeId` to
//! [`TargetTypeValidationError::NotRegistered`] and every other variant
//! to [`TargetTypeValidationError::Internal`] (handler maps to 500/503,
//! NOT 400).
//!
//! **Wildcard target** (`gts.cf.core.am.*`, GTS §8.2) — a wildcard is not a
//! concrete type-schema, so `get_type_schema` can never resolve it. Instead
//! we ask the registry whether *any* registered type-schema matches the
//! pattern via `list_type_schemas(with_pattern)`. A match → valid; no match →
//! `warn!` and PASS anyway (fail-open), since a wildcard may legitimately
//! cover types not registered yet and this check is advisory. A hard registry
//! failure still maps to [`TargetTypeValidationError::Internal`].

// The struct is wired in `module.rs` as `Arc<dyn TargetTypeValidator>`;
// the lint can't see through that registration path, so silence it here.
#![allow(unreachable_pub)]

use std::sync::Arc;

use async_trait::async_trait;
use tracing::warn;
use types_registry_sdk::{TypeSchemaQuery, TypesRegistryClient, TypesRegistryError};

use crate::domain::target_type_validator::{TargetTypeValidationError, TargetTypeValidator};

pub struct TypesRegistryTargetTypeValidator {
    client: Arc<dyn TypesRegistryClient>,
}

impl TypesRegistryTargetTypeValidator {
    pub fn new(client: Arc<dyn TypesRegistryClient>) -> Self {
        Self { client }
    }

    /// Validate a wildcard target (`gts.cf.core.am.*`, GTS §8.2).
    ///
    /// A wildcard is not a concrete type-schema, so we ask the registry
    /// whether *any* registered type-schema matches the pattern:
    ///
    /// * **at least one match** → valid.
    /// * **no match** → emit a `warn!` and PASS anyway (fail-open). A wildcard
    ///   may legitimately cover types that aren't registered yet, and
    ///   target-type validation is advisory, not authoritative — so an empty
    ///   match must not become a 400.
    /// * **hard registry failure** → `Internal` (handler → 500/503), same as
    ///   the concrete path; sustained outages stay loud at the API surface.
    async fn ensure_wildcard_registered(
        &self,
        target_type: &str,
    ) -> Result<(), TargetTypeValidationError> {
        match self
            .client
            .list_type_schemas(TypeSchemaQuery::new().with_pattern(target_type))
            .await
        {
            Ok(schemas) if !schemas.is_empty() => Ok(()),
            Ok(_) => {
                warn!(
                    target_type,
                    "wildcard target_type matched no registered type-schema in the \
                     types-registry; allowing it (fail-open)"
                );
                Ok(())
            }
            Err(other) => Err(TargetTypeValidationError::Internal(other.to_string())),
        }
    }
}

/// A GTS wildcard target carries a `*` token (GTS §8.2, e.g.
/// `gts.cf.core.am.*`); a concrete type id never does. That single character
/// is enough to discriminate the two without a full parse.
fn is_wildcard(target_type: &str) -> bool {
    target_type.contains('*')
}

#[async_trait]
impl TargetTypeValidator for TypesRegistryTargetTypeValidator {
    async fn ensure_exists(&self, target_type: &str) -> Result<(), TargetTypeValidationError> {
        // A wildcard isn't a concrete type-schema; `get_type_schema` can't
        // resolve it, so route it to the pattern-based registry lookup.
        if is_wildcard(target_type) {
            return self.ensure_wildcard_registered(target_type).await;
        }
        match self.client.get_type_schema(target_type).await {
            Ok(_) => Ok(()),
            Err(canonical) => match TypesRegistryError::from(canonical) {
                TypesRegistryError::NotFound { .. } | TypesRegistryError::Validation { .. } => Err(
                    TargetTypeValidationError::NotRegistered(target_type.to_owned()),
                ),
                other => Err(TargetTypeValidationError::Internal(other.to_string())),
            },
        }
    }

    /// Single batched registry round-trip via `get_type_schemas`
    /// (which dedups input ids and returns a per-id result map) instead of
    /// one `get_type_schema` per rule. Errors are reported in the caller's
    /// input order so the surfaced `NotRegistered` is deterministic.
    async fn ensure_all_exist(
        &self,
        target_types: &[&str],
    ) -> Result<(), TargetTypeValidationError> {
        if target_types.is_empty() {
            return Ok(());
        }
        // Only concrete ids can go through `get_type_schemas` (it resolves one
        // exact id per entry); batch those in a single round-trip and resolve
        // each wildcard separately via its pattern lookup below.
        let concrete_ids: Vec<String> = target_types
            .iter()
            .filter(|t| !is_wildcard(t))
            .map(|t| (*t).to_owned())
            .collect();
        let results = if concrete_ids.is_empty() {
            std::collections::HashMap::new()
        } else {
            self.client.get_type_schemas(concrete_ids).await
        };
        // Walk the original order so the first failure is deterministic.
        for &target_type in target_types {
            if is_wildcard(target_type) {
                self.ensure_wildcard_registered(target_type).await?;
                continue;
            }
            match results.get(target_type) {
                Some(Ok(_)) => {}
                Some(Err(canonical)) => {
                    return match TypesRegistryError::from(canonical.clone()) {
                        TypesRegistryError::NotFound { .. }
                        | TypesRegistryError::Validation { .. } => Err(
                            TargetTypeValidationError::NotRegistered(target_type.to_owned()),
                        ),
                        other => Err(TargetTypeValidationError::Internal(other.to_string())),
                    };
                }
                None => {
                    return Err(TargetTypeValidationError::Internal(format!(
                        "types-registry returned no result for '{target_type}'"
                    )));
                }
            }
        }
        Ok(())
    }
}

/// Pass-through validator, currently UNWIRED — `module.rs` wires the real
/// [`TypesRegistryTargetTypeValidator`]. Kept as a documented escape hatch: if
/// the real validator's wildcard fail-open proves insufficient and
/// AM/RG role provisioning regresses, swap this back in `module.rs` to unblock
/// e2e while a proper fix lands.
///
/// TODO: delete once Account Management emits a user-created event and RBAC
/// auto-provisions a built-in tenant-member role in response. At that point the
/// e2e factories stop hand-authoring that role and the wildcard AM/RG targets go
/// away with it.
#[allow(dead_code)]
pub struct NoopTargetTypeValidator;

#[async_trait]
impl TargetTypeValidator for NoopTargetTypeValidator {
    async fn ensure_exists(&self, _target_type: &str) -> Result<(), TargetTypeValidationError> {
        Ok(())
    }

    async fn ensure_all_exist(
        &self,
        _target_types: &[&str],
    ) -> Result<(), TargetTypeValidationError> {
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use std::sync::Arc;

    use toolkit_gts::gts_id;
    use types_registry_sdk::testing::{MockTypesRegistryClient, make_test_type_schema};

    use super::*;

    /// A registered RBAC entity type-schema id (concrete, `~`-terminated).
    const CONCRETE: &str = gts_id!("cf.core.rbac.role_definition.v1~");
    /// An AM family wildcard target (GTS §8.2).
    const AM_WILDCARD: &str = gts_id!("cf.core.am.*");

    fn validator(client: MockTypesRegistryClient) -> TypesRegistryTargetTypeValidator {
        TypesRegistryTargetTypeValidator::new(Arc::new(client))
    }

    #[tokio::test]
    async fn concrete_registered_type_passes() {
        let client =
            MockTypesRegistryClient::new().with_type_schemas([make_test_type_schema(CONCRETE)]);
        validator(client).ensure_exists(CONCRETE).await.unwrap();
    }

    #[tokio::test]
    async fn concrete_unregistered_type_is_not_registered() {
        let err = validator(MockTypesRegistryClient::new())
            .ensure_exists(CONCRETE)
            .await
            .unwrap_err();
        assert!(matches!(err, TargetTypeValidationError::NotRegistered(t) if t == CONCRETE));
    }

    #[tokio::test]
    async fn wildcard_with_a_matching_schema_passes() {
        // Registry has at least one type-schema → the pattern resolves.
        let client = MockTypesRegistryClient::new()
            .with_type_schemas([make_test_type_schema(gts_id!("cf.core.am.tenant.v1~"))]);
        validator(client).ensure_exists(AM_WILDCARD).await.unwrap();
    }

    #[tokio::test]
    async fn wildcard_with_no_match_warns_but_passes() {
        // Empty registry → no match → fail-open Ok (a warn! is emitted).
        validator(MockTypesRegistryClient::new())
            .ensure_exists(AM_WILDCARD)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn wildcard_hard_registry_failure_is_internal() {
        let client = MockTypesRegistryClient::new().with_list_error(
            toolkit_canonical_errors::CanonicalError::internal("registry down").create(),
        );
        let err = validator(client)
            .ensure_exists(AM_WILDCARD)
            .await
            .unwrap_err();
        assert!(matches!(err, TargetTypeValidationError::Internal(_)));
    }

    #[tokio::test]
    async fn ensure_all_exist_passes_mixed_concrete_and_wildcard() {
        let client =
            MockTypesRegistryClient::new().with_type_schemas([make_test_type_schema(CONCRETE)]);
        validator(client)
            .ensure_all_exist(&[CONCRETE, AM_WILDCARD])
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn ensure_all_exist_reports_unregistered_concrete_before_wildcard() {
        // Concrete comes first in input order and is missing → NotRegistered
        // short-circuits before the (fail-open) wildcard is ever reached.
        let err = validator(MockTypesRegistryClient::new())
            .ensure_all_exist(&[CONCRETE, AM_WILDCARD])
            .await
            .unwrap_err();
        assert!(matches!(err, TargetTypeValidationError::NotRegistered(t) if t == CONCRETE));
    }
}
