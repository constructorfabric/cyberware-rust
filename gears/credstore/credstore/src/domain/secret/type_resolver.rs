//! Registry-driven secret-type resolution — the runtime source of truth
//! for a secret type's PDP resource id and enforceable traits.
//!
//! Secrets store their type as the deterministic v5 UUID of the type's GTS
//! id (`credstore_sdk::type_uuid`). Every operation resolves that UUID
//! against the GTS types-registry to recover the full type id (the PDP
//! resource the single authz evaluation targets) and the effective traits
//! the write path enforces. New secret types are added by registering a
//! GTS schema derived from `gts.cf.core.credstore.credential.v1~` — no
//! credstore release involved; the compiled-in catalog only seeds the
//! built-in schemas.
//!
//! This module owns the **trait abstraction**; the production
//! implementation ([`crate::infra::types_registry::GtsSecretTypeResolver`])
//! wraps `types_registry_sdk::TypesRegistryClient` resolved from
//! `ClientHub`. The resolver deliberately holds no cache of its own: the
//! registry's local client already keeps a TTL cache of `Arc<GtsTypeSchema>`
//! keyed by id and UUID, so a per-operation resolve is one cached lookup.

use async_trait::async_trait;
use credstore_sdk::SecretTypeTraits;
use toolkit_macros::domain_model;
use uuid::Uuid;

use crate::domain::error::DomainError;

/// A secret type resolved from the types-registry by its deterministic
/// v5 UUID (the stored representation).
#[domain_model]
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedSecretType {
    /// Full chained GTS type id — the PDP resource type and the
    /// wire-visible type name (e.g.
    /// `gts.cf.core.credstore.credential.v1~cf.core.credstore.api_key.v1~`).
    pub gts_id: String,
    /// Effective enforcement traits (chain-merged; leaf-declared values
    /// win, the secret base type fills the rest).
    pub traits: SecretTypeTraits,
}

/// Per-operation secret-type resolution barrier.
///
/// Implementations MUST fail closed: a type that cannot be positively
/// resolved to a registered schema descending from the credstore secret
/// base type never reaches trait validation or the PDP.
#[async_trait]
pub trait SecretTypeResolver: Send + Sync {
    /// Resolve `type_uuid` to its GTS type id and effective traits.
    ///
    /// # Errors
    ///
    /// * [`DomainError::TypeViolation`] (reason `UNKNOWN_SECRET_TYPE`) —
    ///   no type-schema is registered under this UUID, or the registered
    ///   schema does not descend from the credstore secret base type.
    /// * [`DomainError::ServiceUnavailable`] — the types-registry is
    ///   unreachable, times out, or the registered schema's effective
    ///   traits fail to resolve into [`SecretTypeTraits`].
    async fn resolve(&self, type_uuid: Uuid) -> Result<ResolvedSecretType, DomainError>;
}
