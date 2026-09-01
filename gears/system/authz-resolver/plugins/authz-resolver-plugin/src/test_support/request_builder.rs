//! Fluent builder over `EvaluationRequest`.
//!
//! `Default` produces a fully valid request so tests mutate only the field
//! they are exercising. Validation tests can then write
//! `EvaluationRequestBuilder::default().with_subject_type(None).build()`
//! without spelling out the unrelated fields.

use std::collections::HashMap;

use authz_resolver_sdk::models::{
    Action, Capability, EvaluationRequest, EvaluationRequestContext, Resource, Subject,
    TenantContext,
};
use uuid::Uuid;

/// Default GTS subject type used by the builder — the user variant.
pub const DEFAULT_SUBJECT_TYPE: &str = "gts.cf.core.security.subject_user.v1~";

/// Default subject home tenant the builder stamps into
/// `subject.properties["tenant_id"]`. A real authenticated subject always
/// carries a home tenant, so the default request is realistic — the plugin's
/// RBAC caller-context builder needs it. Tests exercising the no-tenant path
/// opt out via `without_subject_tenant`.
pub const DEFAULT_SUBJECT_TENANT_ID: Uuid = Uuid::from_u128(0x5004);

/// Default action name — `read` is a known-mapped operation in the
/// scope-enforcement default map.
pub const DEFAULT_ACTION_NAME: &str = "read";

/// Default resource type GTS identifier for tests. The 5-segment form
/// (vendor=`cf`, package=`core`, namespace=`resources`, type=`test`,
/// version=`v1`) is required to pass `GtsTypeId` parser validation —
/// the GTS spec rejects shorter type ids.
pub const DEFAULT_RESOURCE_TYPE: &str = "gts.cf.core.resources.test.v1~";

#[derive(Debug, Clone)]
pub struct EvaluationRequestBuilder {
    subject_id: Uuid,
    subject_type: Option<String>,
    subject_tenant_id: Option<Uuid>,
    /// Overrides `subject_tenant_id` with a verbatim string, so a test can
    /// present a `tenant_id` claim that is present but unreadable.
    raw_subject_tenant: Option<String>,
    action_name: String,
    resource_type: String,
    resource_id: Option<Uuid>,
    token_scopes: Vec<String>,
    tenant_context: Option<TenantContext>,
    supported_properties: Vec<String>,
    capabilities: Vec<Capability>,
    require_constraints: bool,
}

impl Default for EvaluationRequestBuilder {
    fn default() -> Self {
        Self {
            subject_id: Uuid::nil(),
            subject_type: Some(DEFAULT_SUBJECT_TYPE.to_owned()),
            subject_tenant_id: Some(DEFAULT_SUBJECT_TENANT_ID),
            raw_subject_tenant: None,
            action_name: DEFAULT_ACTION_NAME.to_owned(),
            resource_type: DEFAULT_RESOURCE_TYPE.to_owned(),
            resource_id: None,
            // Mirrors the SDK's struct default (empty Vec). Tests that need
            // to reach post-scope evaluation steps must set this explicitly
            // via `with_token_scopes`.
            token_scopes: Vec::new(),
            // Mirrors the SDK's struct default (None). The policy evaluator
            // translates None → `Scope::Root` per design.
            tenant_context: None,
            // Mirrors the SDK's struct default (empty Vec). Tests that reach
            // the constraint generator must populate this via
            // `with_supported_properties` — empty denies any predicate with
            // `unsupported_property.v1`.
            supported_properties: Vec::new(),
            // Mirrors the SDK's struct default (empty Vec) — no PEP
            // capabilities advertised. Tests exercising capability-driven
            // push-down opt in via `with_capabilities`.
            capabilities: Vec::new(),
            // Default is `true` so existing tests that assert on the
            // generated constraints still observe them. Tests that want to
            // exercise the `create` path opt out via
            // `with_require_constraints(false)`.
            require_constraints: true,
        }
    }
}

impl EvaluationRequestBuilder {
    #[must_use]
    pub fn with_subject_id(mut self, id: Uuid) -> Self {
        self.subject_id = id;
        self
    }

    #[must_use]
    pub fn with_subject_type(mut self, subject_type: Option<String>) -> Self {
        self.subject_type = subject_type;
        self
    }

    /// Set the subject's home tenant, stamped into
    /// `subject.properties["tenant_id"]`.
    #[must_use]
    pub fn with_subject_tenant_id(mut self, tenant_id: Uuid) -> Self {
        self.subject_tenant_id = Some(tenant_id);
        self
    }

    /// Put a verbatim string in `subject.properties["tenant_id"]`, bypassing
    /// UUID formatting.
    ///
    /// For the present-but-unreadable case, which is distinct from absent:
    /// absent takes the documented `tenant_context.root_id` fallback, while
    /// unreadable must fail closed rather than inherit the root tenant.
    #[must_use]
    pub fn with_raw_subject_tenant(mut self, raw: impl Into<String>) -> Self {
        self.raw_subject_tenant = Some(raw.into());
        self
    }

    /// Drop the subject's home tenant entirely (no `tenant_id` property) — for
    /// exercising the no-tenant-resolvable fail-closed path.
    #[must_use]
    pub fn without_subject_tenant(mut self) -> Self {
        self.subject_tenant_id = None;
        self
    }

    #[must_use]
    pub fn with_action_name(mut self, name: impl Into<String>) -> Self {
        self.action_name = name.into();
        self
    }

    #[must_use]
    pub fn with_resource_type(mut self, resource_type: impl Into<String>) -> Self {
        self.resource_type = resource_type.into();
        self
    }

    #[must_use]
    pub fn with_resource_id(mut self, id: Option<Uuid>) -> Self {
        self.resource_id = id;
        self
    }

    #[must_use]
    pub fn with_token_scopes(mut self, scopes: Vec<String>) -> Self {
        self.token_scopes = scopes;
        self
    }

    #[must_use]
    pub fn with_tenant_context(mut self, ctx: Option<TenantContext>) -> Self {
        self.tenant_context = ctx;
        self
    }

    /// Set the PEP-declared property names the request advertises.
    /// Empty (default) means "PEP supports nothing" and denies any predicate
    /// via `unsupported_property.v1`.
    #[must_use]
    pub fn with_supported_properties(mut self, properties: Vec<String>) -> Self {
        self.supported_properties = properties;
        self
    }

    /// Set the PEP-advertised capabilities (e.g. `Capability::TenantHierarchy`
    /// to opt into `InTenantSubtree` push-down).
    #[must_use]
    pub fn with_capabilities(mut self, capabilities: Vec<Capability>) -> Self {
        self.capabilities = capabilities;
        self
    }

    /// Toggle `require_constraints`. The builder default is `true` to match
    /// existing tests that assert on generated constraints; `create`-action
    /// tests should pass `false` to exercise the empty-constraints allow.
    #[must_use]
    pub fn with_require_constraints(mut self, require_constraints: bool) -> Self {
        self.require_constraints = require_constraints;
        self
    }

    #[must_use]
    pub fn build(self) -> EvaluationRequest {
        let mut properties = HashMap::new();
        if let Some(raw) = self.raw_subject_tenant {
            properties.insert("tenant_id".to_owned(), serde_json::Value::String(raw));
        } else if let Some(tenant_id) = self.subject_tenant_id {
            // AuthZEN convention: the subject's home tenant lives under
            // `properties["tenant_id"]` as a UUID string.
            properties.insert(
                "tenant_id".to_owned(),
                serde_json::Value::String(tenant_id.to_string()),
            );
        }
        EvaluationRequest {
            subject: Subject {
                id: self.subject_id,
                subject_type: self.subject_type,
                properties,
            },
            action: Action {
                name: self.action_name,
            },
            resource: Resource {
                resource_type: self.resource_type,
                id: self.resource_id,
                properties: HashMap::new(),
            },
            context: EvaluationRequestContext {
                tenant_context: self.tenant_context,
                token_scopes: self.token_scopes,
                require_constraints: self.require_constraints,
                capabilities: self.capabilities,
                supported_properties: self.supported_properties,
                bearer_token: None,
            },
        }
    }
}
