//! Value-object models for the license resolver.
//!
//! These are the transport-agnostic Rust value objects passed in and out of a
//! check. Each [`Subject`] / [`Resource`] is an instance of a registered,
//! derived licensing contract type (see [`crate::gts`]); the resolver
//! validates every request against those contracts before delegating.

use std::collections::HashMap;

use gts::GtsTypeId;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use toolkit_security::SecurityContext;
use uuid::Uuid;

/// The subject of a check — whom the license is checked for.
///
/// An instance of a derived Subject contract type (base
/// `gts.cf.core.lic.subj.v1~`). Polymorphic: a tenant, a user, or any future
/// subject type — the resolver never assumes the subject is a tenant. Its wire
/// shape is pinned by [`LicenseSubjectV1`](crate::gts::LicenseSubjectV1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Subject {
    /// The derived Subject contract type this object instantiates (e.g.
    /// `gts.cf.core.lic.subj.v1~cf.genai.llm_gateway.user.v1~`). Wire key
    /// `type`; the resolver resolves its schema to validate `metadata`.
    #[serde(rename = "type")]
    pub gts_type: GtsTypeId,
    /// Optional instance id — a well-known name or a UUID (natural key);
    /// non-empty, at most 255 characters per the base schema.
    /// Absent for a type-level subject.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Licensing-relevant properties conforming to the derived contract's
    /// metadata schema. Semantically opaque to the resolver; shape-validated
    /// then forwarded to the plugin unchanged.
    ///
    /// Required on the wire, so an omitted `metadata` is non-conforming and
    /// **must not** be defaulted to `{}` here. Pass an empty map when the
    /// contract declares no metadata.
    pub metadata: Map<String, Value>,
}

/// The resource of a check — the licensable thing.
///
/// An instance of a derived Resource contract type (base
/// `gts.cf.core.lic.res.v1~`). Without [`id`](Self::id) the check targets the
/// whole resource type (e.g. gating a `POST`); with it, a specific instance. Its
/// wire shape is pinned by [`LicenseResourceV1`](crate::gts::LicenseResourceV1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Resource {
    /// The derived Resource contract type this object instantiates (e.g.
    /// `gts.cf.core.lic.res.v1~cf.genai.llm_gateway.model_usage.v1~`). Wire key
    /// `type`; the resolver resolves its schema to validate `metadata` and read
    /// `admitted_subjects`.
    #[serde(rename = "type")]
    pub gts_type: GtsTypeId,
    /// Optional instance id — a well-known name or a UUID (natural key);
    /// non-empty, at most 255 characters per the base schema.
    /// Absent = whole-type check; how that is answered is the backend's policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Licensing-relevant properties conforming to the derived contract's
    /// metadata schema. Semantically opaque to the resolver; shape-validated
    /// then forwarded to the plugin unchanged.
    ///
    /// Required on the wire, so an omitted `metadata` is non-conforming and
    /// **must not** be defaulted to `{}` here. Pass an empty map when the
    /// contract declares no metadata.
    pub metadata: Map<String, Value>,
}

/// The request's tenant context — the isolation scope of a check.
///
/// Built from the caller's `SecurityContext` with
/// [`from_security_context`](Self::from_security_context). Minimal today
/// (tenant scope only); the extension point for future contextual evaluation
/// semantics. Every resolution is scoped to this tenant.
///
/// Unknown fields are ignored, so a contextual input can be added without
/// breaking callers or older readers — but only one that satisfies **absence
/// must not grant more than presence**. An input that cannot is a new version of
/// the licensing contract, not an optional field (see "Envelope evolution" in
/// the module's DESIGN doc).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct LicenseCheckContext {
    /// The tenant the check is scoped to. No cross-tenant resolution.
    pub tenant_id: Uuid,
}

impl LicenseCheckContext {
    /// Derive the check's tenant scope from the caller's authenticated context.
    ///
    /// Takes the **subject's** tenant — the tenant the authenticated principal
    /// belongs to — so the scope of a check follows the caller rather than being
    /// asserted independently. This is the intended constructor: a hand-assembled
    /// context is not verified by the resolver, which receives no
    /// `SecurityContext` of its own.
    #[must_use]
    pub fn from_security_context(ctx: &SecurityContext) -> Self {
        Self {
            tenant_id: ctx.subject_tenant_id(),
        }
    }

    /// Assemble a context field by field.
    ///
    /// Prefer [`from_security_context`](Self::from_security_context); reach for
    /// the builder only where no `SecurityContext` exists (tests, fixtures, a
    /// caller that has already derived the tenant itself).
    #[must_use]
    pub fn builder() -> LicenseCheckContextBuilder {
        LicenseCheckContextBuilder::default()
    }
}

/// Error returned when [`LicenseCheckContextBuilder::build`] is called without
/// required fields.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum LicenseCheckContextBuildError {
    /// No tenant scope was set, and a check is always tenant-scoped.
    #[error(
        "tenant_id is required - derive it from the caller's authenticated context with \
         LicenseCheckContext::from_security_context"
    )]
    MissingTenantId,
}

/// Builder for [`LicenseCheckContext`].
///
/// Fields are private so that adding a contextual input stays additive: new
/// inputs become new methods here rather than breaking existing call sites.
#[derive(Debug, Default)]
pub struct LicenseCheckContextBuilder {
    tenant_id: Option<Uuid>,
}

impl LicenseCheckContextBuilder {
    /// Scope the check to an explicit tenant.
    #[must_use]
    pub fn tenant_id(mut self, tenant_id: Uuid) -> Self {
        self.tenant_id = Some(tenant_id);
        self
    }

    /// Build the context.
    ///
    /// # Errors
    ///
    /// [`LicenseCheckContextBuildError::MissingTenantId`] if no tenant scope
    /// was set.
    pub fn build(self) -> Result<LicenseCheckContext, LicenseCheckContextBuildError> {
        Ok(LicenseCheckContext {
            tenant_id: self
                .tenant_id
                .ok_or(LicenseCheckContextBuildError::MissingTenantId)?,
        })
    }
}

/// The single input to a check — the contract's growth surface.
///
/// New inputs are added as fields here, never as new method parameters. Because
/// that growth is expected, the type is `#[non_exhaustive]` and is built through
/// [`new`](Self::new), so adding an input stays additive instead of breaking
/// every caller's struct literal. Contextual inputs belong on
/// [`LicenseCheckContext`].
///
/// Unknown fields are **ignored** here too, under the same rule as
/// [`LicenseCheckContext`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct LicenseCheckRequest {
    /// Whom the license is checked for.
    pub subject: Subject,
    /// The licensable thing being checked.
    pub resource: Resource,
    /// The tenant isolation scope (caller-derived from its `SecurityContext`).
    pub context: LicenseCheckContext,
}

impl LicenseCheckRequest {
    /// Assemble a check from its three parts.
    ///
    /// Build `context` with [`LicenseCheckContext::from_security_context`] so the
    /// tenant scope follows the authenticated caller.
    #[must_use]
    pub fn new(subject: Subject, resource: Resource, context: LicenseCheckContext) -> Self {
        Self {
            subject,
            resource,
            context,
        }
    }
}

/// The result of a check.
///
/// A negative answer is **not** an error — it is `LicenseDecision { granted:
/// false, .. }`. [`diagnostics`](Self::diagnostics) is non-authoritative debug
/// info about how the decision was reached (e.g. backend id, matched grant,
/// denial cause) and MUST NOT be required to interpret the boolean.
///
/// Unknown fields are **ignored**; adding one is non-breaking in both directions
/// because [`granted`](Self::granted) is authoritative on its own.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct LicenseDecision {
    /// Whether the resource is licensed to the subject at the time of the call.
    pub granted: bool,
    /// Advisory, string-keyed debug information. Never authoritative.
    #[serde(default)]
    pub diagnostics: HashMap<String, serde_json::Value>,
}

impl LicenseDecision {
    /// The decision itself, with an empty [`diagnostics`](Self::diagnostics) map.
    #[must_use]
    pub fn new(granted: bool) -> Self {
        Self {
            granted,
            diagnostics: HashMap::new(),
        }
    }

    /// Attach one advisory diagnostic — never authoritative for the answer.
    #[must_use]
    pub fn with_diagnostic(mut self, key: impl Into<String>, value: impl Into<Value>) -> Self {
        self.diagnostics.insert(key.into(), value.into());
        self
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "models_tests.rs"]
mod models_tests;
