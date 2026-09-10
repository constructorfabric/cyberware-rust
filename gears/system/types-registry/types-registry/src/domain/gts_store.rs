//! The transient `gts-rust` store: one owned [`GtsStore`] per admission unit,
//! built from database rows and dropped when the unit ends (SPEC D2, §8.2).
//!
//! Nothing here is cached, published or shared — no `ArcSwap`, no snapshot, no
//! process-lifetime store, no lock. Reads are served from the database (§8.2), and
//! this store exists only so one worker invocation can run the computations that
//! need related documents: resolution, `compare_documents`, derivation chains,
//! Instance validation.
//!
//! `GtsStore` is `Send` but not `Sync`, because `GtsReader` has no `Sync`
//! supertrait — the reason the old in-memory repository held a `Mutex<GtsOps>`. A
//! store owned by one invocation is never shared, so the missing `Sync` is
//! irrelevant here; it can still cross an `.await`.
//!
//! # Three properties of `gts` 0.12.0 this module is shaped by
//!
//! **There is no builder.** `GtsStore` offers `new()`, `with_reader()` and
//! `register*`; its map is private and `with_reader` merely drains a reader
//! eagerly. For a fully known row set, `new()` plus a loop is the whole vocabulary,
//! so implementing `GtsReader` would buy nothing.
//!
//! **Registration is order-free; validation is not.** `register_schema` inserts
//! into a map and checks nothing; `validate_schema` / `resolve_schema_refs` walk
//! `GtsId::chain_ids()` and fail on an absent base. Loading in `gts_id` order does
//! not make registration succeed — the store is complete before anything is asked
//! of it — but it is kept so a caller that later registers incrementally is not one
//! refactor away from a spurious "base schema not found".
//!
//! **A document with no `$schema` is silently not a schema.** `register_schema`
//! passes `is_schema: true` to `GtsEntity::new`, which overwrites it with
//! `has_schema_field()`, so a dialect-less document registers as an *Instance* and
//! `$ref`s pointing at it stay unresolved with no error anywhere. [`build_store`]
//! rejects such a document by name rather than letting it corrupt a resolution two
//! layers away; the acceptance dialect gate (T7) is the same rule applied earlier.
//! It is asked of **Type Schemas only** — an Instance legitimately has no
//! `$schema` — so the gate is keyed on the identifier's `~`.

use std::collections::{HashMap, HashSet};

use gts::{GtsEntity, GtsId, GtsStore, StoreError};
use serde_json::Value;
use thiserror::Error;
use toolkit_db::DbTx;
use toolkit_db::secure::{AccessScope, ScopeError};
use toolkit_macros::domain_model;

use crate::domain::dependency::{extract_edges, reference_targets};
use crate::domain::enums::EntityKind;
use crate::domain::ports::{EntityRow, Stores};

/// One authored document destined for the transient store.
///
/// A committed row and an in-flight candidate arrive in the same shape, because
/// the store cannot tell them apart and must not: an in-batch reference resolves
/// against the candidate, not against whatever is committed under that
/// identifier.
#[domain_model]
#[derive(Clone, Debug)]
pub struct UnitDocument {
    pub gts_id: String,
    pub content: Value,
}

/// An owned store plus the three facts its builder learned on the way.
///
/// Deliberately not `Arc`, not behind a lock, and with no way to clone the store
/// out: one admission unit owns this and drops it.
#[domain_model]
pub struct UnitStore {
    store: GtsStore,
    load_order: Vec<String>,
    roots: Vec<String>,
    closure: Vec<EntityRow>,
    missing_candidates: Vec<String>,
    missing_references: Vec<String>,
}

/// `GtsStore` is not `Debug`, and `finish_non_exhaustive` is the honest way to
/// say so — the store's contents are the `load_order` below, which is printed.
impl std::fmt::Debug for UnitStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UnitStore")
            .field("load_order", &self.load_order)
            .field("missing_candidates", &self.missing_candidates)
            .field("missing_references", &self.missing_references)
            .finish_non_exhaustive()
    }
}

impl UnitStore {
    /// The identifiers as they were registered, in order.
    ///
    /// This exists because registration leaves no trace of its order in the
    /// store's contents, so "loaded base-before-derived" is otherwise an
    /// unobservable claim. It is the assertion surface for the load-order test.
    #[must_use]
    pub fn load_order(&self) -> &[String] {
        &self.load_order
    }

    /// Candidate and reference roots used to build this store.
    #[must_use]
    pub fn roots(&self) -> &[String] {
        &self.roots
    }

    /// Resolved closure rows, sorted by `gts_id`.
    #[must_use]
    pub fn closure_entities(&self) -> &[EntityRow] {
        &self.closure
    }

    /// Candidate identifiers with no entity row — reported, not fatal. A first
    /// admission's own candidate is always here.
    #[must_use]
    pub fn missing_candidates(&self) -> &[String] {
        &self.missing_candidates
    }

    /// `$ref` targets that no entity carries.
    #[must_use]
    pub fn missing_references(&self) -> &[String] {
        &self.missing_references
    }

    /// Every read on `GtsStore` takes `&mut self` (`get` writes the reader
    /// fallback), so the store is reached mutably even to look something up.
    pub fn store_mut(&mut self) -> &mut GtsStore {
        &mut self.store
    }
}

/// What can go wrong between a row set and a usable store.
///
/// Its own error type rather than a `DomainError` variant: T7 is the first caller
/// with a REST surface to map onto, and `DomainError` has no `From<ScopeError>`
/// yet. Adding a variant and a `CanonicalError` arm now would be a guess about a
/// mapping that does not exist.
#[domain_model]
#[derive(Debug, Error)]
pub enum StoreBuildError {
    #[error("reading the dependency closure failed: {0}")]
    Storage(#[from] ScopeError),

    /// A Type Schema entity in the closure has no current document. Structurally
    /// impossible for an admitted schema — `type_schema` is written in the same
    /// transaction as the entity (D3) — so this is a corrupt row, not a race.
    #[error("entity '{gts_id}' has no current Type Schema document")]
    MissingDocument { gts_id: String },

    #[error("the stored document for '{gts_id}' is not valid JSON: {source}")]
    Content {
        gts_id: String,
        source: serde_json::Error,
    },

    /// See the module header: without `$schema` the document would register as an
    /// Instance and references to it would silently fail to resolve.
    #[error("document '{gts_id}' declares no JSON Schema dialect ($schema)")]
    MissingDialect { gts_id: String },

    /// `register_schema` overwrites an existing identifier without complaint, so
    /// two documents under one identifier would mean the store's content depends
    /// on load order. Refused instead: merging is the caller's decision, and the
    /// loader makes it explicitly in favour of the candidate.
    #[error("document '{gts_id}' appears twice in one admission unit")]
    Duplicate { gts_id: String },

    /// An Instance identifier with no conforming type. Unreachable today — `gts-id`
    /// refuses a single-segment instance id in `try_new` first — but the `Option` is
    /// real in the signature, and a named error beats an `expect`.
    #[error("instance '{gts_id}' has no conforming type: its identifier has one segment")]
    InstanceWithoutType { gts_id: String },

    /// The mirror of [`Self::MissingDocument`]: `instance` is written in the same
    /// transaction as the entity, so this is a corrupt row, not a race.
    #[error("entity '{gts_id}' has no current Instance value")]
    MissingValue { gts_id: String },

    #[error("registering '{gts_id}' in the transient store failed: {source}")]
    Register { gts_id: String, source: StoreError },
}

/// Register one Instance into the store, with the conforming type it declares by
/// its identifier.
///
/// **`type_id` is passed explicitly, not extracted.** Given a `GtsConfig`,
/// `GtsEntity::new` derives it from a *content* field — the wrong authority: a value
/// could then claim conformance to a type it was not registered under. So the config
/// is `None` and `GtsId::get_type_id()` is the only source.
///
/// `is_schema` is not passed either: `GtsEntity::new` overwrites it with
/// `has_schema_field()`, so an Instance cannot be mislabelled from here.
fn register_instance(store: &mut GtsStore, doc: &UnitDocument) -> Result<(), StoreBuildError> {
    let gts_id = GtsId::try_new(&doc.gts_id).map_err(|source| StoreBuildError::Register {
        gts_id: doc.gts_id.clone(),
        source: StoreError::InvalidTypeId(source),
    })?;
    let type_id = gts_id
        .get_type_id()
        .ok_or_else(|| StoreBuildError::InstanceWithoutType {
            gts_id: doc.gts_id.clone(),
        })?;
    let entity = GtsEntity::new(
        /* file */ None,
        /* list_sequence */ None,
        /* content */ &doc.content,
        /* cfg */ None,
        /* gts_id */ Some(gts_id),
        /* is_schema */ false,
        /* label */ String::new(),
        /* validation */ None,
        /* type_id */ Some(type_id),
    );
    store
        .register(entity)
        .map_err(|source| StoreBuildError::Register {
            gts_id: doc.gts_id.clone(),
            source,
        })
}

/// Build an owned store from a document set. Pure: no database, no clock, no
/// global state, nothing retained.
///
/// Documents are sorted by `gts_id` and registered in that order, which is
/// base-before-derived because a base identifier is a byte prefix of everything
/// derived from it (`GtsId::chain_ids()` is the same walk). Sorting happens here
/// rather than being assumed of the caller: the dependency closure already
/// returns sorted rows, but a caller that merges a candidate overlay into them
/// destroys that order, and this function is the one place that can guarantee it.
///
/// # Errors
/// [`StoreBuildError::Duplicate`] for a repeated identifier,
/// [`StoreBuildError::MissingDialect`] for a **Type Schema** with no `$schema`,
/// [`StoreBuildError::InstanceWithoutType`] for a single-segment Instance, and
/// [`StoreBuildError::Register`] if `gts-rust` rejects the identifier.
pub fn build_store(mut documents: Vec<UnitDocument>) -> Result<UnitStore, StoreBuildError> {
    documents.sort_by(|a, b| a.gts_id.cmp(&b.gts_id));

    let mut seen: HashSet<&str> = HashSet::with_capacity(documents.len());
    for doc in &documents {
        if !seen.insert(doc.gts_id.as_str()) {
            return Err(StoreBuildError::Duplicate {
                gts_id: doc.gts_id.clone(),
            });
        }
    }

    let mut store = GtsStore::new();
    let mut load_order = Vec::with_capacity(documents.len());
    for doc in documents {
        // `register_schema` refuses an Instance identifier as bad syntax, so the two
        // kinds take different calls.
        if doc.gts_id.ends_with('~') {
            if !has_dialect(&doc.content) {
                return Err(StoreBuildError::MissingDialect { gts_id: doc.gts_id });
            }
            store
                .register_schema(&doc.gts_id, &doc.content)
                .map_err(|source| StoreBuildError::Register {
                    gts_id: doc.gts_id.clone(),
                    source,
                })?;
        } else {
            register_instance(&mut store, &doc)?;
        }
        load_order.push(doc.gts_id);
    }

    Ok(UnitStore {
        store,
        load_order,
        // This constructor receives documents, not a resolved closure.
        roots: Vec::new(),
        closure: Vec::new(),
        missing_candidates: Vec::new(),
        missing_references: Vec::new(),
    })
}

/// Build the store one admission unit needs: the candidates, plus the transitive
/// closure of what they consume, read from the database.
///
/// Load a snapshot store with candidate documents overriding committed versions.
///
/// # Errors
/// Propagates the closure and document reads, and every [`StoreBuildError`] the
/// row set can produce.
pub async fn load_unit_store(
    stores: &dyn Stores,
    tx: &DbTx<'_>,
    scope: &AccessScope,
    candidates: Vec<UnitDocument>,
) -> Result<UnitStore, StoreBuildError> {
    let candidate_ids: Vec<String> = candidates.iter().map(|c| c.gts_id.clone()).collect();
    let overlay: HashSet<&str> = candidate_ids.iter().map(String::as_str).collect();

    let mut roots = candidate_ids.clone();
    roots.extend(candidate_reference_targets(&candidates));
    roots.sort();
    roots.dedup();

    let closure = stores.closure(tx, scope, &roots).await?;
    // Preserve closure rows before the loops consume their data.
    let closure_entities = closure.entities.clone();

    // Authored text lives in a different table per kind, and `entity_kind` is the
    // only thing that says which. Two batched reads, not one per row.
    let mut schemas: Vec<(i64, String)> = Vec::with_capacity(closure.entities.len());
    let mut instances: Vec<(i64, String)> = Vec::new();
    for row in &closure.entities {
        if overlay.contains(row.gts_id.as_str()) {
            continue;
        }
        match row.entity_kind {
            EntityKind::TypeSchema => schemas.push((row.id, row.gts_id.clone())),
            EntityKind::Instance => instances.push((row.id, row.gts_id.clone())),
        }
    }

    let schema_ids: Vec<i64> = schemas.iter().map(|(id, _)| *id).collect();
    let mut raw_schemas: HashMap<i64, String> = stores
        .current_documents(tx, scope, &schema_ids)
        .await?
        .into_iter()
        .map(|d| (d.entity_id, d.raw_schema))
        .collect();

    let instance_ids: Vec<i64> = instances.iter().map(|(id, _)| *id).collect();
    let mut raw_values: HashMap<i64, String> = stores
        .current_values(tx, scope, &instance_ids)
        .await?
        .into_iter()
        .map(|v| (v.entity_id, v.canonical_value))
        .collect();

    let mut documents = candidates;
    documents.reserve(schemas.len() + instances.len());
    for (entity_id, gts_id) in schemas {
        let Some(text) = raw_schemas.remove(&entity_id) else {
            return Err(StoreBuildError::MissingDocument { gts_id });
        };
        let content = serde_json::from_str(&text).map_err(|source| StoreBuildError::Content {
            gts_id: gts_id.clone(),
            source,
        })?;
        documents.push(UnitDocument { gts_id, content });
    }
    for (entity_id, gts_id) in instances {
        let Some(text) = raw_values.remove(&entity_id) else {
            return Err(StoreBuildError::MissingValue { gts_id });
        };
        let content = serde_json::from_str(&text).map_err(|source| StoreBuildError::Content {
            gts_id: gts_id.clone(),
            source,
        })?;
        documents.push(UnitDocument { gts_id, content });
    }

    let mut unit = build_store(documents)?;
    unit.roots = roots;
    unit.closure = closure_entities;
    // One read, split by which root it was.
    let (missing_candidates, missing_references): (Vec<String>, Vec<String>) = closure
        .missing_roots
        .into_iter()
        .partition(|root| overlay.contains(root.as_str()));
    unit.missing_candidates = missing_candidates;
    unit.missing_references = missing_references;
    Ok(unit)
}

/// The reference targets of every candidate document, as extra closure roots.
fn candidate_reference_targets(candidates: &[UnitDocument]) -> Vec<String> {
    let mut targets = Vec::new();
    for candidate in candidates {
        let Ok(id) = GtsId::try_new(&candidate.gts_id) else {
            continue;
        };
        if let Ok(edges) = extract_edges(&id, &candidate.content) {
            targets.extend(reference_targets(&edges));
        }
    }
    targets
}

/// The exact test `GtsEntity::has_schema_field` applies: an object carrying a
/// non-empty `$schema` string. Reproduced rather than approximated, because a
/// looser test here would let a document register as an Instance.
fn has_dialect(content: &Value) -> bool {
    content
        .get("$schema")
        .and_then(Value::as_str)
        .is_some_and(|s| !s.is_empty())
}

#[cfg(test)]
#[path = "gts_store_tests.rs"]
mod gts_store_tests;
