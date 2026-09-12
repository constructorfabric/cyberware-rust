//! The catalogue's entities and its read API.
//!
//! Every contract schema is compiled once, here, under the Draft-07 dialect it
//! declares, with format assertion on, the GTS id formats registered, and
//! `x-gts-ref` compiled as a keyword of the schema itself. A GTS reference
//! constraint therefore counts toward a branch's validity like `type` or
//! `format` does: a `oneOf` over two branches that differ only in their
//! `x-gts-ref` picks exactly one, and a branch that carries no GTS constraint
//! is not "matched" by default. The request path then validates a document
//! with no allocation beyond the envelope it wraps.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::Arc;

use gts::{GTS_ID_URI_PREFIX, GtsId, GtsIdPattern, GtsInstanceId, GtsTypeId};
use jsonschema::paths::Location;
use jsonschema::{Draft, Keyword, ValidationError, Validator};
use quota_enforcement_sdk::{ContractRef, METRIC_BASE_TYPE, MetricId, SubjectScope};
use serde_json::{Map, Value};
use toolkit_macros::domain_model;

/// The GTS reference keyword of the `gts` schema dialect.
const X_GTS_REF: &str = "x-gts-ref";

/// A well-formed instance id whose declaring type is the metric base. Whether
/// the registry knows it is a separate question the bootstrap answers.
#[must_use]
pub fn parse_metric_under_base(text: &str) -> Option<MetricId> {
    let parsed = GtsId::try_new(text).ok()?;
    if parsed.is_type() || parsed.get_type_id().as_deref() != Some(METRIC_BASE_TYPE) {
        return None;
    }
    MetricId::parse(text).ok()
}

/// A resolved contract schema compiled for validation.
pub struct CompiledContract {
    type_id: GtsTypeId,
    validator: Validator,
}

impl CompiledContract {
    /// Compile `schema`, the fully resolved body of `type_id`.
    ///
    /// # Errors
    ///
    /// Returns the compiler's diagnostic when the schema does not compile,
    /// including an `x-gts-ref` whose value is not a GTS id or pattern, or a
    /// pointer form that does not resolve to one in this schema.
    pub fn compile(type_id: GtsTypeId, schema: Value) -> Result<Self, String> {
        let root = Arc::new(schema);
        let root_for_keyword = Arc::clone(&root);
        let validator = jsonschema::options()
            .with_draft(Draft::Draft7)
            .should_validate_formats(true)
            .with_format("gts-type-id", |s| GtsTypeId::try_new(s).is_ok())
            .with_format("gts-instance-id", |s| GtsInstanceId::try_new(s).is_ok())
            .with_keyword(X_GTS_REF, move |parent, value, location| {
                GtsRefKeyword::compile(&root_for_keyword, parent, value, location)
            })
            .build(&root)
            .map_err(|e| e.to_string())?;
        Ok(Self { type_id, validator })
    }

    /// The contract type.
    #[must_use]
    pub fn type_id(&self) -> &GtsTypeId {
        &self.type_id
    }

    /// Validate the whole `document` (the `{type, metadata}` envelope, never
    /// the inner object alone) against the schema, its GTS formats, and its
    /// `x-gts-ref` constraints in one pass. The diagnostics are for the log;
    /// callers surface a closed reason token.
    ///
    /// # Errors
    ///
    /// Returns every violation found.
    pub fn validate(&self, document: &Value) -> Result<(), Vec<String>> {
        if self.validator.is_valid(document) {
            return Ok(());
        }
        Err(self
            .validator
            .iter_errors(document)
            .map(|e| format!("{}: {e}", e.instance_path()))
            .collect())
    }
}

/// `x-gts-ref` as a schema keyword: a string value must be a well-formed GTS
/// id that matches the declared pattern. Non-string values are left to `type`,
/// as the `gts` validator does, so the keyword adds a constraint and never
/// duplicates one.
///
/// The value is an absolute id or pattern (`gts.cf.core.qe.scope.v1~`,
/// `gts.cf.*`) or a JSON pointer (`/$id`, `/properties/type`) into the root
/// schema whose target is one; a `gts://` prefix on the target is stripped.
/// Both forms are checked when the schema compiles, so a request never meets
/// a malformed constraint.
struct GtsRefKeyword {
    pattern: GtsIdPattern,
    declared: String,
}

impl GtsRefKeyword {
    fn compile<'a>(
        root: &Value,
        _parent: &'a Map<String, Value>,
        value: &'a Value,
        _location: Location,
    ) -> Result<Box<dyn Keyword>, ValidationError<'a>> {
        let Some(declared) = value.as_str() else {
            return Err(ValidationError::custom(format!(
                "{X_GTS_REF} must be a string, got {value}"
            )));
        };
        let text = if declared.starts_with('/') {
            Self::resolve_pointer(root, declared).ok_or_else(|| {
                ValidationError::custom(format!(
                    "{X_GTS_REF} '{declared}' does not point at a GTS pattern in this schema"
                ))
            })?
        } else {
            declared.to_owned()
        };
        let pattern = GtsIdPattern::try_new(&text).map_err(|e| {
            ValidationError::custom(format!(
                "{X_GTS_REF} '{declared}' is not a GTS id or pattern: {e}"
            ))
        })?;
        Ok(Box::new(Self {
            pattern,
            declared: declared.to_owned(),
        }))
    }

    /// The pointer form resolves against the root schema to a string, or to a
    /// sub-schema whose own `x-gts-ref` is absolute; deeper pointer chains are
    /// not followed.
    fn resolve_pointer(root: &Value, pointer: &str) -> Option<String> {
        let target = root.pointer(pointer)?;
        let text = match target {
            Value::String(s) => s.as_str(),
            Value::Object(map) => map.get(X_GTS_REF)?.as_str()?,
            _ => return None,
        };
        if text.starts_with('/') {
            return None;
        }
        Some(
            text.strip_prefix(GTS_ID_URI_PREFIX)
                .unwrap_or(text)
                .to_owned(),
        )
    }

    fn matches(&self, instance: &Value) -> bool {
        match instance.as_str() {
            Some(text) => GtsId::try_new(text).is_ok_and(|id| id.matches_pattern(&self.pattern)),
            None => true,
        }
    }
}

impl Keyword for GtsRefKeyword {
    fn validate<'i>(&self, instance: &'i Value) -> Result<(), ValidationError<'i>> {
        if self.matches(instance) {
            return Ok(());
        }
        Err(ValidationError::custom(format!(
            "{instance} is not a GTS id under {X_GTS_REF} '{}'",
            self.declared
        )))
    }

    fn is_valid(&self, instance: &Value) -> bool {
        self.matches(instance)
    }
}

impl fmt::Debug for CompiledContract {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CompiledContract")
            .field("type_id", &self.type_id)
            .finish_non_exhaustive()
    }
}

/// A configured concrete subject projection: the type half of a Quota's
/// subject identity.
#[domain_model]
#[derive(Debug)]
pub struct SubjectProjectionContract {
    /// The projection type, derived from `gts.cf.core.qe.subj.v1~`.
    pub type_id: GtsTypeId,
    /// The scope its `scope` trait declares.
    pub scope: SubjectScope,
    /// The metrics its `admitted_metrics` trait declares.
    pub admitted_metrics: HashSet<MetricId>,
}

/// A configured concrete resource projection.
#[domain_model]
#[derive(Debug)]
pub struct ResourceProjectionContract {
    /// The projection type, derived from `gts.cf.core.qe.res.v1~`.
    pub type_id: GtsTypeId,
    /// Validates the complete `{type, id?, metadata}` document.
    pub contract: CompiledContract,
}

/// The constraint contract attached to a metric request contract.
#[domain_model]
#[derive(Debug)]
pub struct ConstraintContract {
    /// The contract at the version its id declares.
    pub reference: ContractRef,
    /// Validates the `{type, metadata}` envelope of a Quota's metadata.
    pub contract: CompiledContract,
}

/// The one request contract of an admitted metric.
#[domain_model]
#[derive(Debug)]
pub struct MetricRequestContract {
    /// The contract type, derived from `gts.cf.core.qe.request.v1~`.
    pub type_id: GtsTypeId,
    /// The metric its `metric` trait names.
    pub metric: MetricId,
    /// The constraint contract its `constraint_contract` trait attaches.
    pub constraint: ConstraintContract,
    /// Validates the `{type, metadata}` envelope of operation metadata.
    pub contract: CompiledContract,
}

/// Everything the catalogue knows about one admitted metric.
#[derive(Debug)]
struct MetricEntry {
    request: MetricRequestContract,
    /// The reverse index: which configured projection admits the metric at
    /// each scope. Unique per pair by construction.
    by_scope: HashMap<SubjectScope, GtsTypeId>,
}

/// Why a `(metric, kind)` pair has no projection.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CatalogMiss {
    /// No configured projection admits the metric at any scope.
    MetricNotAdmitted,
    /// The kind is a scope no configured projection declares.
    KindUnknown,
    /// The kind is a known scope, but no projection admits the metric there.
    KindNotAdmitted,
}

/// The immutable process-local catalogue. Built once at bootstrap, read on
/// every subject-based request without a registry call.
///
/// `cpt-cf-quota-enforcement-dod-projection-catalog` stays open until the
/// active-Quota compatibility check runs against the database.
#[domain_model]
#[derive(Debug)]
pub struct ProjectionContractCatalog {
    subjects: HashMap<GtsTypeId, SubjectProjectionContract>,
    resources: HashMap<GtsTypeId, ResourceProjectionContract>,
    metrics: HashMap<MetricId, MetricEntry>,
    scopes: HashSet<SubjectScope>,
}

impl ProjectionContractCatalog {
    /// A catalogue with no projections. Every mapping misses.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            subjects: HashMap::new(),
            resources: HashMap::new(),
            metrics: HashMap::new(),
            scopes: HashSet::new(),
        }
    }

    /// Assemble the catalogue from validated parts. The builder has already
    /// proven `(metric, scope)` uniqueness and that every admitted metric has
    /// exactly one request contract.
    pub(super) fn assemble(
        subjects: Vec<SubjectProjectionContract>,
        resources: Vec<ResourceProjectionContract>,
        requests: Vec<MetricRequestContract>,
    ) -> Self {
        let mut metrics: HashMap<MetricId, MetricEntry> = requests
            .into_iter()
            .map(|request| {
                (
                    request.metric.clone(),
                    MetricEntry {
                        request,
                        by_scope: HashMap::new(),
                    },
                )
            })
            .collect();
        let mut scopes = HashSet::new();
        for projection in &subjects {
            scopes.insert(projection.scope.clone());
            for metric in &projection.admitted_metrics {
                if let Some(entry) = metrics.get_mut(metric) {
                    entry
                        .by_scope
                        .insert(projection.scope.clone(), projection.type_id.clone());
                }
            }
        }
        Self {
            subjects: subjects
                .into_iter()
                .map(|p| (p.type_id.clone(), p))
                .collect(),
            resources: resources
                .into_iter()
                .map(|r| (r.type_id.clone(), r))
                .collect(),
            metrics,
            scopes,
        }
    }

    /// The projection that admits `metric` at `scope`.
    ///
    /// # Errors
    ///
    /// Returns which part of the pair the catalogue does not know.
    pub fn map_subject(
        &self,
        metric: &MetricId,
        scope: &SubjectScope,
    ) -> Result<&GtsTypeId, CatalogMiss> {
        let entry = self
            .metrics
            .get(metric)
            .ok_or(CatalogMiss::MetricNotAdmitted)?;
        entry
            .by_scope
            .get(scope)
            .ok_or(if self.scopes.contains(scope) {
                CatalogMiss::KindNotAdmitted
            } else {
                CatalogMiss::KindUnknown
            })
    }

    /// The request contract of an admitted metric.
    #[must_use]
    pub fn request_contract(&self, metric: &MetricId) -> Option<&MetricRequestContract> {
        self.metrics.get(metric).map(|e| &e.request)
    }

    /// A configured resource projection.
    #[must_use]
    pub fn resource_projection(&self, type_id: &GtsTypeId) -> Option<&ResourceProjectionContract> {
        self.resources.get(type_id)
    }

    /// A configured subject projection.
    #[must_use]
    pub fn subject_projection(&self, type_id: &GtsTypeId) -> Option<&SubjectProjectionContract> {
        self.subjects.get(type_id)
    }

    /// True when `projection` is configured and admits `metric`.
    #[must_use]
    pub fn admits(&self, projection: &GtsTypeId, metric: &MetricId) -> bool {
        self.subjects
            .get(projection)
            .is_some_and(|p| p.admitted_metrics.contains(metric))
    }

    /// True when some configured projection declares `scope`.
    #[must_use]
    pub fn knows_scope(&self, scope: &SubjectScope) -> bool {
        self.scopes.contains(scope)
    }

    /// The configured subject projections.
    pub fn subject_projections(&self) -> impl Iterator<Item = &SubjectProjectionContract> {
        self.subjects.values()
    }

    /// The admitted metrics.
    pub fn admitted_metrics(&self) -> impl Iterator<Item = &MetricId> {
        self.metrics.keys()
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "model_tests.rs"]
mod model_tests;
