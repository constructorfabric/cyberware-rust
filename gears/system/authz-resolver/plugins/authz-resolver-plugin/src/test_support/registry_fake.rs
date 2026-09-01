//! In-memory `TypesRegistryClient` fake.
//!
//! Two scripted surfaces:
//!
//! - **`register` recording** — every batch is appended to an internal log
//!   so init tests can assert the plugin registered
//!   its GTS instance correctly.
//! - **`get_type_schema` lookup** — `with_known_types(...)`
//!   pre-populates the set of known type ids; `set_unavailable(true)`
//!   makes every lookup return `Err(ServiceUnavailable)`. Tests pair these
//!   with `get_type_schema_call_count()` to assert the validator's
//!   cache-first behavior.
//!
//! Other trait methods (`register_type_schemas`, `get_instance*`, etc.)
//! remain `unreachable!()` — they're not exercised by any current test.

use std::collections::HashSet;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use async_trait::async_trait;
use serde_json::Value;
use toolkit::api::canonical_prelude::CanonicalError;
use types_registry_sdk::testing::make_test_type_schema;
use types_registry_sdk::{
    GtsInstance, GtsTypeSchema, InstanceQuery, RegisterResult, TypeSchemaQuery, TypesRegistryClient,
};
use uuid::Uuid;

/// Records every `register` batch and answers `get_type_schema` per the
/// scripted `known_types` set / `unavailable` flag.
pub struct RecordingTypesRegistry {
    calls: Mutex<Vec<Vec<Value>>>,
    failure: Option<CanonicalError>,
    known_types: Mutex<HashSet<String>>,
    unavailable: AtomicBool,
    get_type_schema_calls: AtomicUsize,
}

impl Default for RecordingTypesRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl RecordingTypesRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            failure: None,
            known_types: Mutex::new(HashSet::new()),
            unavailable: AtomicBool::new(false),
            get_type_schema_calls: AtomicUsize::new(0),
        }
    }

    /// Build a registry that fails every `register` call with the given
    /// message. Used by tests that exercise the error propagation path through
    /// `Gear::init`. Uses `ServiceUnavailable` (not `Internal`) so the detail
    /// survives the canonical envelope — `Internal` redacts its detail, which
    /// would defeat the propagation assertion.
    #[must_use]
    pub fn failing(message: impl Into<String>) -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            failure: Some(
                CanonicalError::service_unavailable()
                    .with_detail(message)
                    .create(),
            ),
            known_types: Mutex::new(HashSet::new()),
            unavailable: AtomicBool::new(false),
            get_type_schema_calls: AtomicUsize::new(0),
        }
    }

    /// Build a registry that recognizes the given GTS type ids. Subsequent
    /// `get_type_schema` calls return `Ok(_)` for these ids and
    /// `Err(GtsTypeSchemaNotFound)` for anything else (unless
    /// `set_unavailable(true)` is also active).
    #[must_use]
    pub fn with_known_types(types: Vec<&str>) -> Self {
        let fake = Self::new();
        fake.add_known_types(types);
        fake
    }

    /// Add a single GTS type id to the known-set. Idempotent.
    pub fn add_known_type(&self, gts_type: &str) {
        match self.known_types.lock() {
            Ok(mut guard) => {
                guard.insert(gts_type.to_owned());
            }
            Err(poisoned) => {
                poisoned.into_inner().insert(gts_type.to_owned());
            }
        }
    }

    /// Bulk variant of `add_known_type`.
    pub fn add_known_types(&self, types: Vec<&str>) {
        for t in types {
            self.add_known_type(t);
        }
    }

    /// Flip the "registry unavailable" flag. While `true`, every
    /// `get_type_schema` call returns `Err(ServiceUnavailable)`.
    pub fn set_unavailable(&self, unavailable: bool) {
        self.unavailable.store(unavailable, Ordering::SeqCst);
    }

    /// Snapshot of every batch passed to [`Self::register`], in call order.
    #[must_use]
    pub fn calls(&self) -> Vec<Vec<Value>> {
        match self.calls.lock() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    /// Count of `get_type_schema` invocations since construction. Separate
    /// from the `register` log so tests can assert validator-cache behavior
    /// independently.
    #[must_use]
    pub fn get_type_schema_call_count(&self) -> usize {
        self.get_type_schema_calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl TypesRegistryClient for RecordingTypesRegistry {
    async fn register(&self, entities: Vec<Value>) -> Result<Vec<RegisterResult>, CanonicalError> {
        if let Some(err) = &self.failure {
            return Err(err.clone());
        }

        let results = entities
            .iter()
            .map(|entity| {
                let gts_id = entity
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                RegisterResult::Ok { gts_id }
            })
            .collect();

        match self.calls.lock() {
            Ok(mut guard) => guard.push(entities),
            Err(poisoned) => poisoned.into_inner().push(entities),
        }

        Ok(results)
    }

    async fn register_type_schemas(
        &self,
        _type_schemas: Vec<Value>,
    ) -> Result<Vec<RegisterResult>, CanonicalError> {
        unreachable!("RecordingTypesRegistry::register_type_schemas unused in foundation")
    }

    async fn get_type_schema(&self, type_id: &str) -> Result<GtsTypeSchema, CanonicalError> {
        self.get_type_schema_calls.fetch_add(1, Ordering::SeqCst);

        if self.unavailable.load(Ordering::SeqCst) {
            return Err(CanonicalError::service_unavailable()
                .with_detail("simulated registry outage")
                .create());
        }

        let known = match self.known_types.lock() {
            Ok(guard) => guard.contains(type_id),
            Err(poisoned) => poisoned.into_inner().contains(type_id),
        };

        if known {
            // Build a synthetic placeholder schema. The validator only
            // distinguishes Ok(_) from Err(_); it never inspects the
            // schema's content. `make_test_type_schema`
            // handles the chain construction so any well-formed GTS id
            // produces a valid schema.
            Ok(make_test_type_schema(type_id))
        } else {
            Err(types_registry_sdk::testing::not_found(type_id))
        }
    }

    async fn get_type_schema_by_uuid(
        &self,
        _type_uuid: Uuid,
    ) -> Result<GtsTypeSchema, CanonicalError> {
        unreachable!("RecordingTypesRegistry::get_type_schema_by_uuid unused in foundation")
    }

    async fn get_type_schemas(
        &self,
        _type_ids: Vec<String>,
    ) -> std::collections::HashMap<String, Result<GtsTypeSchema, CanonicalError>> {
        unreachable!("RecordingTypesRegistry::get_type_schemas unused in foundation")
    }

    async fn get_type_schemas_by_uuid(
        &self,
        _type_uuids: Vec<Uuid>,
    ) -> std::collections::HashMap<Uuid, Result<GtsTypeSchema, CanonicalError>> {
        unreachable!("RecordingTypesRegistry::get_type_schemas_by_uuid unused in foundation")
    }

    async fn list_type_schemas(
        &self,
        _query: TypeSchemaQuery,
    ) -> Result<Vec<GtsTypeSchema>, CanonicalError> {
        unreachable!("RecordingTypesRegistry::list_type_schemas unused in foundation")
    }

    async fn register_instances(
        &self,
        _instances: Vec<Value>,
    ) -> Result<Vec<RegisterResult>, CanonicalError> {
        unreachable!("RecordingTypesRegistry::register_instances unused in foundation")
    }

    async fn get_instance(&self, _id: &str) -> Result<GtsInstance, CanonicalError> {
        unreachable!("RecordingTypesRegistry::get_instance unused in foundation")
    }

    async fn get_instance_by_uuid(&self, _uuid: Uuid) -> Result<GtsInstance, CanonicalError> {
        unreachable!("RecordingTypesRegistry::get_instance_by_uuid unused in foundation")
    }

    async fn get_instances(
        &self,
        _ids: Vec<String>,
    ) -> std::collections::HashMap<String, Result<GtsInstance, CanonicalError>> {
        unreachable!("RecordingTypesRegistry::get_instances unused in foundation")
    }

    async fn get_instances_by_uuid(
        &self,
        _uuids: Vec<Uuid>,
    ) -> std::collections::HashMap<Uuid, Result<GtsInstance, CanonicalError>> {
        unreachable!("RecordingTypesRegistry::get_instances_by_uuid unused in foundation")
    }

    async fn list_instances(
        &self,
        _query: InstanceQuery,
    ) -> Result<Vec<GtsInstance>, CanonicalError> {
        unreachable!("RecordingTypesRegistry::list_instances unused in foundation")
    }
}
