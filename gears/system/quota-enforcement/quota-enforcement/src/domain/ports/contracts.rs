//! Output port for the projection contract registry
//! (`features/projection-contracts.md`, ADR-0007).
//!
//! The domain reads owner-published contracts from the platform
//! `types-registry` through this port and re-asserts the QE-owned definitions
//! through it at bootstrap. Its only production implementation is
//! `infra::types_registry::TypesRegistryContracts`.
//!
//! Discovery and resolution are separate methods on purpose. `derived_types`
//! reads a listing and follows no reference, so an unrelated owner's contract
//! with a broken reference graph cannot fail this deployment's bootstrap.
//! `type_schema` resolves one type's complete reference graph and is called
//! only for the contracts the catalogue selects.

use async_trait::async_trait;
use gts::{GtsInstanceId, GtsTypeId};
use quota_enforcement_sdk::OwnedDefinition;
use serde_json::Value;
use toolkit_macros::domain_model;

use crate::domain::error::DomainError;

/// A registered type, fully resolved.
#[domain_model]
#[derive(Debug, Clone)]
pub struct RegisteredType {
    /// The type id.
    pub id: GtsTypeId,
    /// `x-gts-abstract: true` on the type itself.
    pub is_abstract: bool,
    /// The parent chain, nearest parent first. Empty for a root type.
    pub ancestors: Vec<GtsTypeId>,
    /// Chain-merged trait values. `Value::Null` when the chain declares none.
    pub effective_traits: Value,
    /// The type body with every `#/` and `gts://` `$ref` inlined. `$id`,
    /// `$schema`, and `x-gts-ref` are kept: the dialect and the GTS value
    /// constraints are part of the contract.
    pub schema: Value,
}

impl RegisteredType {
    /// True when `base` is an ancestor of this type.
    #[must_use]
    pub fn derives_from(&self, base: &str) -> bool {
        self.ancestors.iter().any(|a| a.as_ref() == base)
    }
}

/// A type as a listing reports it: identity, abstractness, and the traits it
/// declares. No reference is followed to produce it.
#[domain_model]
#[derive(Debug, Clone)]
pub struct DiscoveredType {
    /// The type id.
    pub id: GtsTypeId,
    /// `x-gts-abstract: true` on the type itself.
    pub is_abstract: bool,
    /// The trait values the type's own chain declares.
    pub declared_traits: Value,
}

/// The registry as the catalogue needs it.
#[async_trait]
pub trait ContractRegistry: Send + Sync {
    /// Register the QE-owned definitions that are missing. A byte-identical
    /// definition already present is a success.
    ///
    /// # Errors
    ///
    /// - [`DomainError::CatalogInvalid`] with `DefinitionConflict` when a
    ///   definition exists with different content or is rejected.
    /// - [`DomainError::TypesRegistryUnavailable`] when the registry cannot
    ///   answer.
    async fn ensure_registered(&self, definitions: &[OwnedDefinition]) -> Result<(), DomainError>;

    /// Resolve one type with its complete reference graph. `Ok(None)` when it
    /// is not registered.
    ///
    /// # Errors
    ///
    /// Returns [`DomainError::TypesRegistryUnavailable`] when the registry
    /// cannot answer or the registered content does not resolve.
    async fn type_schema(&self, id: &GtsTypeId) -> Result<Option<RegisteredType>, DomainError>;

    /// List the types derived from `base`, from the listing alone.
    ///
    /// # Errors
    ///
    /// Returns [`DomainError::TypesRegistryUnavailable`] when the registry
    /// cannot answer.
    async fn derived_types(&self, base: &GtsTypeId) -> Result<Vec<DiscoveredType>, DomainError>;

    /// The declaring type of a registered instance. `Ok(None)` when the
    /// instance is not registered.
    ///
    /// # Errors
    ///
    /// Returns [`DomainError::TypesRegistryUnavailable`] when the registry
    /// cannot answer.
    async fn instance_type(&self, id: &GtsInstanceId) -> Result<Option<GtsTypeId>, DomainError>;
}
