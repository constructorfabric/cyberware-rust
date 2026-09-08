//! GTS type validation step with three modes and a cache-first lookup.
//!
//! Three modes (per `config.gts_validation.mode`):
//! - `Strict` (default): an unknown type is a business deny.
//! - `Warn`: an unknown type emits a `tracing::warn!` line and allows the
//!   request to proceed. Intended for a rollout whose type registration is
//!   still incomplete.
//! - `Off`: skips the registry entirely — `validate_type` is a no-op.
//!
//! A registry outage is NOT a mode-dependent decision: it surfaces as
//! `Err(PluginError::GtsRegistryUnavailable)` in both `Strict` and `Warn`. A
//! resolver that cannot confirm the resource type is degraded, and letting a
//! `Warn` deployment proceed meant every request rode through unvalidated for
//! as long as the registry was down. `Off` never consults the registry, so it
//! has no outage to observe.
//!
//! Cache contract:
//! - In-memory LRU keyed by GTS type id (`String`), capacity 1024.
//! - `Known` and `Unknown` results are cached with a TTL from
//!   `config.cache.ttl_seconds`. `RegistryUnavailable` is NEVER cached —
//!   transient infrastructure state must not poison subsequent lookups.
//! - Cache hits whose `valid_until > clock.now()` serve the cached result
//!   without consulting the registry. Expired entries are evicted on read.
//!
//! The validator is wired into `evaluate()` between `validate(&request)` and
//! the scope enforcer. Subject type is validated before resource type;
//! the first failure short-circuits the resource lookup (fail-fast).

use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::{Duration, Instant};

use authz_resolver_sdk::EvaluationRequest;
use authz_resolver_sdk::models::EvaluationResponse;

use crate::domain::error::PluginError;
use lru::LruCache;
use std::sync::Mutex;
use toolkit::api::canonical_prelude::CanonicalError;
use tracing::{debug, warn};
use types_registry_sdk::{GtsTypeSchema, TypesRegistryClient, TypesRegistryError};

use crate::config::GtsValidationMode;
use crate::domain::clock::Clock;
use crate::domain::deny::build_deny_response;
use crate::domain::deny::error_codes::UNKNOWN_RESOURCE_TYPE_V1;
use crate::domain::subject_type::TrustedSystemActors;
use toolkit_macros::domain_model;

const CACHE_CAPACITY: usize = 1024;

/// Outcome of one registry lookup, after error-shape mapping.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ValidationResult {
    Known,
    Unknown,
    RegistryUnavailable,
}

/// Caller-facing outcome of `validate_type` / `validate_request`. `Allow`
/// means the pipeline proceeds; `Deny(response)` means the caller should
/// short-circuit and return `Ok(response)`. Infrastructure failures
/// (registry outage in Strict mode) propagate as `Err(PluginError)`
/// at the outer `Result` layer.
#[domain_model]
#[derive(Debug, Clone)]
pub(crate) enum TypeValidationOutcome {
    Allow,
    Deny(EvaluationResponse),
}

/// Cached entry — a `ValidationResult` paired with its TTL deadline.
#[domain_model]
#[derive(Debug, Clone)]
struct CacheEntry {
    result: ValidationResult,
    valid_until: Instant,
}

#[domain_model]
pub(crate) struct GtsTypeValidator {
    mode: GtsValidationMode,
    registry: Arc<dyn TypesRegistryClient>,
    cache: Mutex<LruCache<String, CacheEntry>>,
    ttl: Duration,
    clock: Arc<dyn Clock>,
}

impl GtsTypeValidator {
    pub(crate) fn new(
        mode: GtsValidationMode,
        registry: Arc<dyn TypesRegistryClient>,
        ttl: Duration,
        clock: Arc<dyn Clock>,
    ) -> Self {
        // CACHE_CAPACITY is a non-zero compile-time literal, so this never panics.
        #[allow(clippy::expect_used)]
        let capacity =
            NonZeroUsize::new(CACHE_CAPACITY).expect("CACHE_CAPACITY is a non-zero constant");
        Self {
            mode,
            registry,
            cache: Mutex::new(LruCache::new(capacity)),
            ttl,
            clock,
        }
    }

    /// Validate a single GTS type id. Returns `Ok(TypeValidationOutcome)`
    /// where `Allow` means proceed and `Deny(response)` means the caller
    /// should short-circuit. Strict-mode registry outage surfaces as
    /// `Err(ServiceUnavailable)` (infra error, not a business deny).
    pub(crate) async fn validate_type(
        &self,
        gts_type: &str,
        kind: &str,
    ) -> Result<TypeValidationOutcome, PluginError> {
        // Off mode: short-circuit before any cache lookup or registry call.
        if matches!(self.mode, GtsValidationMode::Off) {
            return Ok(TypeValidationOutcome::Allow);
        }

        let now = self.clock.now();

        // Cache lookup. Critical section: hold the mutex only long enough
        // to read (or evict) the entry, then drop it before any await.
        let cached = {
            let mut cache = match self.cache.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            if let Some(entry) = cache.peek(gts_type) {
                if entry.valid_until > now {
                    let result = entry.result.clone();
                    // Touch the recency list — call get(...) to bump LRU.
                    cache.get(gts_type);
                    Some(result)
                } else {
                    // Expired — evict and treat as miss.
                    cache.pop(gts_type);
                    None
                }
            } else {
                None
            }
        };

        if let Some(result) = cached {
            return self.apply_result(result, gts_type, kind);
        }

        // Cache miss — consult the registry.
        let result = map_registry_result(self.registry.get_type_schema(gts_type).await);

        // Cache write: only Known and Unknown are stable; RegistryUnavailable
        // is transient and must not poison the cache.
        if matches!(result, ValidationResult::Known | ValidationResult::Unknown) {
            let entry = CacheEntry {
                result: result.clone(),
                valid_until: now + self.ttl,
            };
            match self.cache.lock() {
                Ok(mut g) => g,
                Err(poisoned) => poisoned.into_inner(),
            }
            .put(gts_type.to_owned(), entry);
            debug!(
                gts_type = %gts_type,
                result = ?result,
                "gts type validated and cached"
            );
        }

        self.apply_result(result, gts_type, kind)
    }

    /// Validate both `subject.type` and `resource.type` from the request,
    /// fail-fast in subject-then-resource order. Returns
    /// `Ok(Allow)` only when both lookups allow; a single Deny short-
    /// circuits and is propagated.
    pub(crate) async fn validate_request(
        &self,
        request: &EvaluationRequest,
        trusted: &TrustedSystemActors,
    ) -> Result<TypeValidationOutcome, PluginError> {
        // `subject_type` is optional — an absent value defaults to `User` in
        // policy evaluation (mirrors RBAC), so there is no GTS type to validate.
        // Validate it as a GTS type only when present.
        //
        // A trusted system actor's `subject_type` is a private in-process marker
        // rather than a registered GTS type, so the registry would answer
        // "unknown" for it and `Strict` would deny the actor before the
        // trusted-allow in policy evaluation ever ran. Skip the subject leg for
        // it, exactly as `validation::validate` and the scope enforcer already
        // do, so `trusted_system_actors` and `mode: strict` compose.
        //
        // The RESOURCE leg is still validated: the resource type is an ordinary
        // registered GTS type no matter who is asking.
        if let Some(subject_type) = request.subject.subject_type.as_deref()
            && !trusted.matches(request.subject.id, Some(subject_type))
        {
            match self.validate_type(subject_type, "subject").await? {
                TypeValidationOutcome::Allow => {}
                deny @ TypeValidationOutcome::Deny(_) => return Ok(deny),
            }
        }

        let resource_type = request.resource.resource_type.as_str();
        self.validate_type(resource_type, "resource").await
    }

    /// Dispatch a `ValidationResult` to the appropriate outcome per the
    /// mode policy table in `docs/DESIGN.md` section 3.3. Strict + Unknown is a
    /// business deny (`Ok(Deny(...))`); a `RegistryUnavailable` in any mode that
    /// consults the registry is an infrastructure error.
    fn apply_result(
        &self,
        result: ValidationResult,
        gts_type: &str,
        kind: &str,
    ) -> Result<TypeValidationOutcome, PluginError> {
        match (self.mode, result) {
            // Off should be filtered out before this function runs, but the
            // arm makes the match exhaustive without an unreachable!().
            (GtsValidationMode::Off, _) => Ok(TypeValidationOutcome::Allow),

            (GtsValidationMode::Strict | GtsValidationMode::Warn, ValidationResult::Known) => {
                Ok(TypeValidationOutcome::Allow)
            }

            (GtsValidationMode::Strict, ValidationResult::Unknown) => {
                // Code stays `UNKNOWN_RESOURCE_TYPE_V1` — it deliberately covers
                // both subject and resource paths (PRD wording; see the constant
                // doc in deny.rs). The human-readable detail names which one so
                // operators aren't misled when it was the subject type.
                Ok(TypeValidationOutcome::Deny(build_deny_response(
                    UNKNOWN_RESOURCE_TYPE_V1,
                    Some(format!("unknown {kind} gts type: '{gts_type}'")),
                )))
            }
            // A registry outage is NOT mode-dependent. `Warn` exists to tolerate
            // an incomplete type REGISTRATION, not a types-registry that is
            // down: allowing here let every request through unvalidated for the
            // whole outage. Use the shared const (not a bare literal) so the
            // metrics classifier labels this a `gts_registry_unavailable` fault
            // rather than the catch-all `resolver_timeout`, which would page
            // on-call for a phantom resolver outage when the registry is what
            // is down.
            (
                GtsValidationMode::Strict | GtsValidationMode::Warn,
                ValidationResult::RegistryUnavailable,
            ) => Err(PluginError::GtsRegistryUnavailable),

            (GtsValidationMode::Warn, ValidationResult::Unknown) => {
                warn!(
                    gts_type = %gts_type,
                    mode = "warn",
                    "unknown gts type, mode=warn -> allowed"
                );
                Ok(TypeValidationOutcome::Allow)
            }
        }
    }
}

/// Translate a `TypesRegistryClient::get_type_schema` result into our
/// `ValidationResult` enum.
fn map_registry_result(result: Result<GtsTypeSchema, CanonicalError>) -> ValidationResult {
    match result {
        Ok(_) => ValidationResult::Known,
        // Project the canonical envelope to the typed enum for dispatch: a
        // registry outage is retryable (RegistryUnavailable); NotFound /
        // Validation / other deterministic per-id errors collapse to Unknown —
        // the type is not usable from the plugin's perspective.
        Err(canonical) => match TypesRegistryError::from(canonical) {
            TypesRegistryError::Unavailable { .. } => ValidationResult::RegistryUnavailable,
            _ => ValidationResult::Unknown,
        },
    }
}

#[cfg(test)]
#[path = "gts_type_validator_tests.rs"]
mod tests;
