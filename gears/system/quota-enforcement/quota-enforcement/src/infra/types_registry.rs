//! `TypesRegistryContracts`: the `ContractRegistry` port over the platform
//! `types-registry` client (ADR-0007).
//!
//! Resolution re-derives a selected type with the `gts` library the registry
//! itself validates with. The type, its parent chain, and every type reached
//! through a `gts://` `$ref`, transitively, are loaded into a local `GtsStore`,
//! and `validate_schema` yields the fully inlined body, the abstract flag, and
//! the chain-merged traits. Neither SDK accessor is enough on its own: the
//! SDK's `effective_schema()` inlines only the parent reference, and
//! `GtsTypeSchema` does not surface `x-gts-abstract`. The resolved body keeps
//! `$schema`, `$id`, and `x-gts-ref`: the Draft-07 dialect and the GTS value
//! constraints are part of the contract the catalogue compiles.
//!
//! Discovery (`derived_types`) reads the listing alone and follows no
//! reference, so a contract nobody configured cannot fail bootstrap through a
//! broken reference graph. Every call is bounded by a deadline, and every
//! failure lifts to a domain error through a named function (DE1302).

use std::collections::{HashSet, VecDeque};
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use gts::{GtsInstanceId, GtsStore, GtsTypeId, extract_gts_refs};
use quota_enforcement_sdk::OwnedDefinition;
use serde_json::Value;
use toolkit_canonical_errors::CanonicalError;
use types_registry_sdk::{GtsTypeSchema, RegisterResult, TypeSchemaQuery, TypesRegistryClient};

use crate::domain::error::DomainError;
use crate::domain::ports::contracts::{ContractRegistry, DiscoveredType, RegisteredType};
use crate::domain::ports::metrics::ValidationReason;

const LOG_TARGET: &str = "qe.bootstrap";

/// Default budget for one registry call.
pub const DEFAULT_REGISTRY_DEADLINE: Duration = Duration::from_secs(10);

/// Upper bound on the types one resolution may load. A contract whose reference
/// graph is larger than this is not a QE contract.
const MAX_REFERENCED_TYPES: usize = 64;

/// The adapter over the registry client `init` resolved from the hub.
pub struct TypesRegistryContracts {
    registry: Arc<dyn TypesRegistryClient>,
    deadline: Duration,
}

impl TypesRegistryContracts {
    /// Bind to `registry` with the default per-call deadline.
    #[must_use]
    pub fn new(registry: Arc<dyn TypesRegistryClient>) -> Self {
        Self {
            registry,
            deadline: DEFAULT_REGISTRY_DEADLINE,
        }
    }

    /// Override the budget one registry call may take.
    #[must_use]
    pub const fn with_deadline(mut self, deadline: Duration) -> Self {
        self.deadline = deadline;
        self
    }

    /// Runs one registry call under the deadline and lifts its failure.
    async fn bounded<T>(
        &self,
        what: &'static str,
        call: impl Future<Output = Result<T, CanonicalError>>,
    ) -> Result<T, DomainError> {
        match tokio::time::timeout(self.deadline, call).await {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(err)) => Err(unavailable(what, &err)),
            Err(_elapsed) => Err(timed_out(what, self.deadline)),
        }
    }

    /// One type with its parent chain. `Ok(None)` when it is not registered.
    async fn fetch_type(&self, id: &str) -> Result<Option<GtsTypeSchema>, DomainError> {
        match tokio::time::timeout(self.deadline, self.registry.get_type_schema(id)).await {
            Ok(Ok(schema)) => Ok(Some(schema)),
            Ok(Err(CanonicalError::NotFound { .. })) => Ok(None),
            Ok(Err(err)) => Err(unavailable("get_type_schema", &err)),
            Err(_elapsed) => Err(timed_out("get_type_schema", self.deadline)),
        }
    }

    /// Every schema the resolution of `root` needs: its own chain plus the
    /// chain of every type reached through a `gts://` `$ref`, transitively.
    /// Iterative on purpose: an `async fn` cannot recurse without boxing.
    async fn reference_closure(
        &self,
        root: &GtsTypeSchema,
    ) -> Result<Vec<(String, Value)>, DomainError> {
        let mut schemas: Vec<(String, Value)> = Vec::new();
        let mut visited: HashSet<String> = HashSet::new();
        let mut to_scan: VecDeque<Value> = VecDeque::new();
        enqueue_chain(root, &mut schemas, &mut visited, &mut to_scan);

        while let Some(raw) = to_scan.pop_front() {
            let references =
                extract_gts_refs(&raw).map_err(|e| drift(root, &format!("invalid $ref: {e}")))?;
            for reference in references {
                if visited.contains(&reference) {
                    continue;
                }
                if visited.len() >= MAX_REFERENCED_TYPES {
                    return Err(drift(
                        root,
                        &format!("references more than {MAX_REFERENCED_TYPES} types"),
                    ));
                }
                let fetched = self.fetch_type(&reference).await?.ok_or_else(|| {
                    drift(root, &format!("references unregistered type {reference}"))
                })?;
                enqueue_chain(&fetched, &mut schemas, &mut visited, &mut to_scan);
            }
        }
        Ok(schemas)
    }
}

/// Adds every member of `schema`'s parent chain not seen before.
fn enqueue_chain(
    schema: &GtsTypeSchema,
    schemas: &mut Vec<(String, Value)>,
    visited: &mut HashSet<String>,
    to_scan: &mut VecDeque<Value>,
) {
    for member in schema.ancestors() {
        let id = member.type_id.as_ref().to_owned();
        if visited.insert(id.clone()) {
            schemas.push((id, member.raw_schema.clone()));
            to_scan.push_back(member.raw_schema.clone());
        }
    }
}

/// Resolves `root` in a local store holding its reference closure.
fn resolve(
    root: &GtsTypeSchema,
    closure: &[(String, Value)],
) -> Result<RegisteredType, DomainError> {
    let mut store = GtsStore::new();
    for (id, raw) in closure {
        store
            .register_schema(id, raw)
            .map_err(|e| drift(root, &e.to_string()))?;
    }
    let resolved = store
        .validate_schema(root.type_id.as_ref())
        .map_err(|e| drift(root, &e.to_string()))?;
    Ok(RegisteredType {
        id: root.type_id.clone(),
        is_abstract: resolved.is_abstract,
        ancestors: root
            .ancestors()
            .skip(1)
            .map(|s| s.type_id.clone())
            .collect(),
        effective_traits: resolved.effective_traits,
        schema: resolved.schema,
    })
}

fn is_abstract(raw: &Value) -> bool {
    raw.get("x-gts-abstract") == Some(&Value::Bool(true))
}

#[async_trait]
impl ContractRegistry for TypesRegistryContracts {
    async fn ensure_registered(&self, definitions: &[OwnedDefinition]) -> Result<(), DomainError> {
        let documents: Vec<Value> = definitions.iter().map(|d| d.document.clone()).collect();
        let results = self
            .bounded("register", self.registry.register(documents))
            .await?;
        // The registry compares content itself: a byte-identical definition is
        // a silent success, so any per-item failure is a definition that
        // exists with other content or was rejected outright.
        for result in &results {
            if let RegisterResult::Err { gts_id, error } = result {
                let subject = gts_id.clone().unwrap_or_else(|| "<unknown>".to_owned());
                tracing::error!(
                    target: LOG_TARGET,
                    gts_id = %subject,
                    error = %error,
                    "a QE-owned GTS definition could not be asserted"
                );
                return Err(DomainError::CatalogInvalid {
                    reason: ValidationReason::DefinitionConflict,
                    subject,
                });
            }
        }
        Ok(())
    }

    async fn type_schema(&self, id: &GtsTypeId) -> Result<Option<RegisteredType>, DomainError> {
        let Some(root) = self.fetch_type(id.as_ref()).await? else {
            return Ok(None);
        };
        let closure = self.reference_closure(&root).await?;
        resolve(&root, &closure).map(Some)
    }

    async fn derived_types(&self, base: &GtsTypeId) -> Result<Vec<DiscoveredType>, DomainError> {
        let query = TypeSchemaQuery::new().with_pattern(format!("{base}*"));
        let listed = self
            .bounded("list_type_schemas", self.registry.list_type_schemas(query))
            .await?;
        // The prefix filter keeps the result correct against a registry that
        // ignores the pattern; the ancestry check excludes look-alike ids.
        Ok(listed
            .into_iter()
            .filter(|s| s.type_id != *base && s.ancestors().any(|a| a.type_id == *base))
            .map(|s| DiscoveredType {
                id: s.type_id.clone(),
                is_abstract: is_abstract(&s.raw_schema),
                declared_traits: s.effective_traits(),
            })
            .collect())
    }

    async fn instance_type(&self, id: &GtsInstanceId) -> Result<Option<GtsTypeId>, DomainError> {
        match tokio::time::timeout(self.deadline, self.registry.get_instance(id.as_ref())).await {
            Ok(Ok(instance)) => Ok(Some(instance.type_id().clone())),
            Ok(Err(CanonicalError::NotFound { .. })) => Ok(None),
            Ok(Err(err)) => Err(unavailable("get_instance", &err)),
            Err(_elapsed) => Err(timed_out("get_instance", self.deadline)),
        }
    }
}

/// A transport or registry failure. The cause is kept as text: the domain
/// error is `Clone + Eq` and crosses the layer boundary as a value.
fn unavailable(what: &str, err: &CanonicalError) -> DomainError {
    DomainError::TypesRegistryUnavailable(format!("types registry `{what}` failed: {err}"))
}

fn timed_out(what: &str, deadline: Duration) -> DomainError {
    DomainError::TypesRegistryUnavailable(format!(
        "types registry did not answer `{what}` within {deadline:?}"
    ))
}

/// Registry-served content that does not resolve locally: the registry and
/// this process disagree about a contract. Fail closed and say why.
fn drift(root: &GtsTypeSchema, detail: &str) -> DomainError {
    tracing::error!(
        target: LOG_TARGET,
        type_id = %root.type_id,
        detail,
        "a registered contract did not resolve"
    );
    DomainError::TypesRegistryUnavailable(format!(
        "contract {} did not resolve: {detail}",
        root.type_id
    ))
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "types_registry_tests.rs"]
mod types_registry_tests;
