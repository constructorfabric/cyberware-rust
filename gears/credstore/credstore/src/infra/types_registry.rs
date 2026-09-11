//! `GtsSecretTypeResolver` — the production [`SecretTypeResolver`] wired
//! against `types_registry_sdk::TypesRegistryClient` resolved from
//! `ClientHub`.
//!
//! Mirrors AM's `GtsTenantTypeChecker` shape:
//!
//! 1. Resolve the [`GtsTypeSchema`] for the stored/declared type UUID via
//!    [`TypesRegistryClient::get_type_schema_by_uuid`] (the SDK's local
//!    client keeps a short TTL cache, so this is one cached lookup per
//!    operation — no cache here).
//! 2. Reject schemas whose chain does not descend from the credstore
//!    credential base type (`gts.cf.core.credstore.credential.v1~`,
//!    renamed from `secret.v1~` by ADR-0004) — anything else cannot
//!    legitimately carry `SecretTypeTraits`.
//! 3. Read the effective traits via [`GtsTypeSchema::effective_traits`]
//!    (chain merge: leaf-declared values win, the base fills the rest)
//!    and deserialize them into [`SecretTypeTraits`].
//! 4. Map failures fail-closed: not-registered / non-secret types onto
//!    [`DomainError::TypeViolation`] (reason `UNKNOWN_SECRET_TYPE`, 400);
//!    registry transport failures, timeouts, and trait-resolution
//!    failures onto [`DomainError::ServiceUnavailable`] (503).
//!
//! Every call is recorded on the `types_registry` dependency-health
//! metrics so a registry outage shows up on the unified dashboard.

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use credstore_sdk::{CREDENTIAL_RESOURCE_TYPE, SecretTypeTraits};
use toolkit_canonical_errors::CanonicalError;
use types_registry_sdk::{GtsTypeSchema, TypesRegistryClient};
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::ports::metrics::{CredStoreMetricsPort, Dep, DepOp, Outcome};
use crate::domain::secret::type_resolver::{ResolvedSecretType, SecretTypeResolver};
use crate::domain::secret::typing::reasons;

/// Default registry probe timeout (ms). Keeps a hung registry from
/// stalling an operation past the 503 fail-closed boundary; a healthy
/// registry round-trip (usually a local cache hit) is well under this.
/// Mirrors AM's `GtsTenantTypeChecker` default for operational
/// consistency.
const DEFAULT_PROBE_TIMEOUT_MS: u64 = 2_000;

/// Production [`SecretTypeResolver`] backed by the GTS types-registry.
pub struct GtsSecretTypeResolver {
    registry: Arc<dyn TypesRegistryClient>,
    metrics: Arc<dyn CredStoreMetricsPort>,
    probe_timeout: Duration,
}

impl GtsSecretTypeResolver {
    /// Construct a resolver around a registry client resolved from
    /// `ClientHub`, using the default probe timeout.
    #[must_use]
    pub fn new(
        registry: Arc<dyn TypesRegistryClient>,
        metrics: Arc<dyn CredStoreMetricsPort>,
    ) -> Self {
        Self::with_timeout(registry, metrics, DEFAULT_PROBE_TIMEOUT_MS)
    }

    /// Construct a resolver with an explicit probe timeout.
    #[must_use]
    pub fn with_timeout(
        registry: Arc<dyn TypesRegistryClient>,
        metrics: Arc<dyn CredStoreMetricsPort>,
        probe_timeout_ms: u64,
    ) -> Self {
        Self {
            registry,
            metrics,
            probe_timeout: Duration::from_millis(probe_timeout_ms.max(1)),
        }
    }

    /// Whether `schema` descends from the credstore credential base type.
    /// Walks `ancestors()` (self → parent → ...), so the base type itself
    /// also passes here; [`SecretTypeResolver::resolve`] then rejects it
    /// separately — it is an abstract trait carrier, not a concrete type.
    fn descends_from_secret_envelope(schema: &GtsTypeSchema) -> bool {
        schema
            .ancestors()
            .any(|s| s.type_id.as_ref() == CREDENTIAL_RESOURCE_TYPE)
    }
}

/// `UNKNOWN_SECRET_TYPE` violation (canonical 400): the UUID does not
/// name a registered credstore credential type.
fn unknown_type(detail: String) -> DomainError {
    DomainError::TypeViolation {
        field: "type",
        reason: reasons::UNKNOWN_SECRET_TYPE,
        detail,
    }
}

/// Registry outage / trait-resolution failure (canonical 503). The
/// wire-visible detail stays curated; the raw registry error travels in
/// the cause chain only.
fn unavailable(detail: impl Into<String>, cause: Option<CanonicalError>) -> DomainError {
    DomainError::ServiceUnavailable {
        detail: detail.into(),
        retry_after: None,
        cause: cause.map(|e| Box::new(e) as _),
    }
}

#[async_trait]
impl SecretTypeResolver for GtsSecretTypeResolver {
    async fn resolve(&self, type_uuid: Uuid) -> Result<ResolvedSecretType, DomainError> {
        let t0 = Instant::now();
        let result = tokio::time::timeout(
            self.probe_timeout,
            self.registry.get_type_schema_by_uuid(type_uuid),
        )
        .await;

        // Transport-level outcome for the dependency-health dashboard:
        // a per-key NotFound is a domain condition (unknown type), not a
        // health signal.
        let outcome = match &result {
            Ok(Ok(_)) => Outcome::Success,
            Ok(Err(CanonicalError::NotFound { .. })) => Outcome::NotFound,
            Ok(Err(_)) | Err(_) => Outcome::Error,
        };
        self.metrics.dependency(
            Dep::TypesRegistry,
            DepOp::GetTypeSchemaByUuid,
            outcome,
            t0.elapsed().as_secs_f64(),
        );

        let schema = match result {
            Err(_elapsed) => {
                return Err(unavailable("types-registry: timeout exceeded", None));
            }
            Ok(Err(CanonicalError::NotFound { .. })) => {
                return Err(unknown_type(format!(
                    "credential type {type_uuid} is not registered"
                )));
            }
            Ok(Err(err)) => {
                tracing::warn!(uuid = %type_uuid, err = %err, "types-registry resolve failed");
                return Err(unavailable("types-registry unavailable", Some(err)));
            }
            Ok(Ok(schema)) => schema,
        };

        if !Self::descends_from_secret_envelope(&schema) {
            return Err(unknown_type(format!(
                "type {} ({type_uuid}) is not a credstore credential type (does not descend from {CREDENTIAL_RESOURCE_TYPE})",
                schema.type_id.as_ref(),
            )));
        }

        // The base type is abstract (`x-gts-abstract`) — a pure trait/schema
        // carrier, never a concrete credential type. The types-registry SDK
        // does not surface abstractness, so enforce the one abstract type we
        // know by id.
        if schema.type_id.as_ref() == CREDENTIAL_RESOURCE_TYPE {
            return Err(unknown_type(format!(
                "type {CREDENTIAL_RESOURCE_TYPE} ({type_uuid}) is the abstract credential base type and cannot type a credential; use a derived type",
            )));
        }

        // The base type declares `x-gts-traits` values for every trait, so
        // a properly registered descendant always resolves a complete map;
        // a deserialization failure means the registered chain is malformed
        // — fail closed as a (registration-fixable) 503, not a caller error.
        let traits: SecretTypeTraits =
            serde_json::from_value(schema.effective_traits()).map_err(|e| {
                tracing::warn!(
                    uuid = %type_uuid,
                    type_id = %schema.type_id.as_ref(),
                    err = %e,
                    "secret type effective traits failed to resolve"
                );
                unavailable(
                    format!(
                        "types-registry: secret type {} has malformed effective traits",
                        schema.type_id.as_ref(),
                    ),
                    None,
                )
            })?;

        Ok(ResolvedSecretType {
            gts_id: schema.type_id.as_ref().to_owned(),
            traits,
        })
    }
}

#[cfg(test)]
#[path = "types_registry_tests.rs"]
mod tests;
