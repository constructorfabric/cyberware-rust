// Updated: 2026-04-28 by Constructor Tech
//! Type-safe API operation builder with compile-time guarantees
//!
//! This gear implements a type-state builder pattern that ensures:
//! - `register()` cannot be called unless a handler is set
//! - `register()` cannot be called unless at least one response is declared
//! - Descriptive methods remain available at any stage
//! - No panics or unwraps in production hot paths
//! - Request body support (`json_request`, `json_request_schema`) so POST/PUT calls are invokable in UI
//! - Schema-aware responses (`json_response_with_schema`)
//! - Typed Router state `S` usage pattern: pass a state type once via `Router::with_state`,
//!   then use plain function handlers (no per-route closures that capture/clones).
//! - Optional `method_router(...)` for advanced use (layers/middleware on route level).

use crate::api::api_dto;
use axum::{Router, handler::Handler, routing::MethodRouter};
use http::Method;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::marker::PhantomData;
use toolkit_canonical_errors::problem;
use toolkit_gts::gts_id;

/// Convert OpenAPI-style path placeholders to Axum 0.8+ style path parameters.
///
/// Axum 0.8+ uses `{id}` for path parameters and `{*path}` for wildcards, which is the same as `OpenAPI`.
/// However, `OpenAPI` wildcards are just `{path}` without the asterisk.
/// This function converts `OpenAPI` wildcards to Axum wildcards by detecting common wildcard names.
///
/// # Examples
///
/// ```
/// # use toolkit::api::operation_builder::normalize_to_axum_path;
/// assert_eq!(normalize_to_axum_path("/users/{id}"), "/users/{id}");
/// assert_eq!(normalize_to_axum_path("/projects/{project_id}/items/{item_id}"), "/projects/{project_id}/items/{item_id}");
/// // Note: Most paths don't need normalization in Axum 0.8+
/// ```
#[must_use]
pub fn normalize_to_axum_path(path: &str) -> String {
    // In Axum 0.8+, the path syntax is {param} for parameters and {*wildcard} for wildcards
    // which is the same as OpenAPI except wildcards need the asterisk prefix.
    // For now, we just pass through the path as-is since OpenAPI and Axum 0.8 use the same syntax
    // for regular parameters. Wildcards need special handling if used.
    path.to_owned()
}

/// Convert Axum 0.8+ style path parameters to OpenAPI-style placeholders.
///
/// Removes the asterisk prefix from Axum wildcards `{*path}` to make them OpenAPI-compatible `{path}`.
///
/// # Examples
///
/// ```
/// # use toolkit::api::operation_builder::axum_to_openapi_path;
/// assert_eq!(axum_to_openapi_path("/users/{id}"), "/users/{id}");
/// assert_eq!(axum_to_openapi_path("/static/{*path}"), "/static/{path}");
/// ```
#[must_use]
pub fn axum_to_openapi_path(path: &str) -> String {
    // In Axum 0.8+, wildcards are {*name} but OpenAPI expects {name}
    // Regular parameters are the same in both
    path.replace("{*", "{")
}

/// Canonical base license feature used by the example gears.
pub const CORE_GLOBAL_BASE_LICENSE_FEATURE: &str =
    gts_id!("cf.core.lic.feat.v1~cf.core.global.base.v1");

/// Type-state markers for compile-time enforcement
pub mod state {
    /// Marker for missing required components
    #[derive(Debug, Clone, Copy)]
    pub struct Missing;

    /// Marker for present required components
    #[derive(Debug, Clone, Copy)]
    pub struct Present;

    /// Marker for auth requirement not yet set
    #[derive(Debug, Clone, Copy)]
    pub struct AuthNotSet;

    /// Marker for auth requirement set (either `authenticated` or public)
    #[derive(Debug, Clone, Copy)]
    pub struct AuthSet;

    /// Marker for license requirement not yet set
    #[derive(Debug, Clone, Copy)]
    pub struct LicenseNotSet;

    /// Marker for license requirement set
    #[derive(Debug, Clone, Copy)]
    pub struct LicenseSet;
}

/// Internal trait mapping handler state to the concrete router slot type.
/// For `Missing` there is no router slot; for `Present` it is `MethodRouter<S>`.
/// Private sealed trait to enforce the implementation is only visible within this gear.
mod sealed {
    pub trait Sealed {}
    pub trait SealedAuth {}
    pub trait SealedLicenseReq {}
}

pub trait HandlerSlot<S>: sealed::Sealed {
    type Slot;
}

/// Sealed trait for auth state markers
pub trait AuthState: sealed::SealedAuth {}

impl sealed::Sealed for Missing {}
impl sealed::Sealed for Present {}

impl sealed::SealedAuth for state::AuthNotSet {}
impl sealed::SealedAuth for state::AuthSet {}

impl AuthState for state::AuthNotSet {}
impl AuthState for state::AuthSet {}

pub trait LicenseState: sealed::SealedLicenseReq {}

impl sealed::SealedLicenseReq for state::LicenseNotSet {}
impl sealed::SealedLicenseReq for state::LicenseSet {}

impl LicenseState for state::LicenseNotSet {}
impl LicenseState for state::LicenseSet {}

impl<S> HandlerSlot<S> for Missing {
    type Slot = ();
}
impl<S> HandlerSlot<S> for Present {
    type Slot = MethodRouter<S>;
}

pub use state::{AuthNotSet, AuthSet, LicenseNotSet, LicenseSet, Missing, Present};

/// Parameter specification for API operations
#[derive(Clone, Debug)]
pub struct ParamSpec {
    pub name: String,
    pub location: ParamLocation,
    pub required: bool,
    pub description: Option<String>,
    pub param_type: String, // JSON Schema type (string, integer, etc.)
    /// Whether the parameter repeats. When set, `param_type` describes the
    /// *item* type and the parameter renders as `type: array` with
    /// `style: form, explode: true` — i.e. `?tag=a&tag=b`, which is how the
    /// generated REST client encodes a `Vec<T>` query field.
    pub array: bool,
}

impl ParamSpec {
    /// A single-valued parameter of `param_type`.
    fn scalar(
        name: String,
        location: ParamLocation,
        required: bool,
        description: Option<String>,
        param_type: String,
    ) -> Self {
        Self {
            name,
            location,
            required,
            description,
            param_type,
            array: false,
        }
    }
}

pub trait LicenseFeature: AsRef<str> {}

impl<T: LicenseFeature + ?Sized> LicenseFeature for &T {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParamLocation {
    Path,
    Query,
    Header,
    Cookie,
}

/// Request body schema variants for different kinds of request bodies
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RequestBodySchema {
    /// Reference to a component schema in `#/components/schemas/{schema_name}`
    Ref { schema_name: String },
    /// Multipart form with a single file field
    MultipartFile { field_name: String },
    /// Raw binary body (e.g. application/octet-stream), represented as
    /// type: string, format: binary in `OpenAPI`.
    Binary,
    /// A generic inline object schema with no predefined properties
    InlineObject,
}

/// Request body specification for API operations
#[derive(Clone, Debug)]
pub struct RequestBodySpec {
    pub content_type: &'static str,
    pub description: Option<String>,
    /// The schema for this request body
    pub schema: RequestBodySchema,
    /// Whether request body is required (`OpenAPI` default is `false`).
    pub required: bool,
}

/// Response body schema variants.
///
/// Mirrors [`RequestBodySchema`]. `Array` exists because utoipa's default
/// `ToSchema::name()` strips generic arguments, so `Vec<A>` and `Vec<B>` both
/// resolve to the component name `Vec` and clobber each other in
/// `components.schemas`. A top-level array is therefore emitted **inline** —
/// `{type: array, items: {$ref: T}}` — registering only the item type as a
/// named component. That is both the `OpenAPI` norm and what utoipa's own
/// `#[utoipa::path]` produces for a `Vec<T>` body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResponseSchema {
    /// Reference to a component schema in `#/components/schemas/{schema_name}`
    Ref { schema_name: String },
    /// Inline array whose items `$ref` the named item component.
    Array { items_schema_name: String },
}

impl ResponseSchema {
    /// The component name this response ultimately references: the type itself
    /// for [`Self::Ref`], the item type for [`Self::Array`].
    #[must_use]
    pub fn schema_name(&self) -> &str {
        match self {
            Self::Ref { schema_name } => schema_name,
            Self::Array { items_schema_name } => items_schema_name,
        }
    }
}

/// Response specification for API operations
#[non_exhaustive]
#[derive(Clone, Debug)]
pub struct ResponseSpec {
    pub status: u16,
    pub content_type: &'static str,
    pub description: String,
    /// Schema of the response body (if any).
    pub schema: Option<ResponseSchema>,
    /// Headers that may be returned with this response.
    pub headers: Vec<ResponseHeaderSpec>,
}

impl ResponseSpec {
    /// Create a response specification without declared headers.
    #[must_use]
    pub fn new(
        status: u16,
        content_type: &'static str,
        description: impl Into<String>,
        schema: Option<ResponseSchema>,
    ) -> Self {
        Self {
            status,
            content_type,
            description: description.into(),
            schema,
            headers: Vec::new(),
        }
    }

    /// Add headers to this response specification.
    ///
    /// # Panics
    /// Panics when the response already has, or the supplied headers contain,
    /// a header with the same case-insensitive name.
    #[must_use]
    pub fn with_headers(mut self, headers: impl IntoIterator<Item = ResponseHeaderSpec>) -> Self {
        let headers: Vec<_> = headers.into_iter().collect();
        for (index, header) in headers.iter().enumerate() {
            assert!(
                !self
                    .headers
                    .iter()
                    .chain(headers[..index].iter())
                    .any(|existing| existing.name.eq_ignore_ascii_case(&header.name)),
                "response {} already declares header '{}'",
                self.status,
                header.name
            );
        }
        self.headers.extend(headers);
        self
    }

    /// Name of the component schema this response references, if any.
    ///
    /// For an array response this is the **item** component, not the array.
    #[must_use]
    pub fn schema_name(&self) -> Option<&str> {
        self.schema.as_ref().map(ResponseSchema::schema_name)
    }
}

/// JSON Schema scalar type of a response header.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResponseHeaderType {
    String,
    Integer,
    Boolean,
}

/// Header declared on one API response.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResponseHeaderSpec {
    pub name: String,
    pub description: Option<String>,
    pub header_type: ResponseHeaderType,
}

impl ResponseHeaderSpec {
    /// Create a response header with a description.
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        header_type: ResponseHeaderType,
    ) -> Self {
        Self {
            name: name.into(),
            description: Some(description.into()),
            header_type,
        }
    }

    /// Create a response header without a description.
    #[must_use]
    pub fn without_description(name: impl Into<String>, header_type: ResponseHeaderType) -> Self {
        Self {
            name: name.into(),
            description: None,
            header_type,
        }
    }
}

/// License requirement specification for an operation
#[derive(Clone, Debug)]
pub struct LicenseReqSpec {
    pub license_names: Vec<String>,
}

/// Simplified operation specification for the type-safe builder
#[derive(Clone, Debug)]
pub struct OperationSpec {
    pub method: Method,
    pub path: String,
    pub operation_id: Option<String>,
    pub summary: Option<String>,
    pub description: Option<String>,
    pub tags: Vec<String>,
    pub params: Vec<ParamSpec>,
    pub request_body: Option<RequestBodySpec>,
    pub responses: Vec<ResponseSpec>,
    /// Internal handler id; can be used by registry/generator to map a handler identity
    pub handler_id: String,
    /// Auth axis: whether this operation requires a validated tenant JWT.
    /// `true` = authenticated (bearer required); `false` = anonymous (a missing
    /// bearer is allowed — a present bearer is still always re-validated).
    /// Independent of [`exposed`](Self::exposed); maps 1:1 to the
    /// `AnonymousRoute` marker in the `OoP` per-gear middleware (`!authenticated`).
    pub authenticated: bool,
    /// Visibility axis: whether this route is registered in the gateway for
    /// external access (`true`) or is internal-only, reachable only via
    /// inter-gear communication (`false`). Defaults to `false` (internal).
    /// Independent of [`authenticated`](Self::authenticated) — an exposed route
    /// may still require a JWT.
    pub exposed: bool,
    /// Optional zone-based throttling configuration for this operation.
    /// Binds the operation to gateway throttling zones and supplies the
    /// identity extractor for identity-keyed zones.
    pub throttling: Option<ThrottlingSpec>,
    /// Optional whitelist of allowed request Content-Type values (without parameters).
    /// Example: Some(vec!["application/json", "multipart/form-data", "application/pdf"])
    /// When set, gateway middleware will enforce these types and return HTTP 415 for
    /// requests with disallowed Content-Type headers. This is independent of the
    /// request body schema and should not be used to create synthetic request bodies.
    pub allowed_request_content_types: Option<Vec<&'static str>>,
    /// `OpenAPI` vendor extensions (x-*)
    pub vendor_extensions: VendorExtensions,
    pub license_requirement: Option<LicenseReqSpec>,
}

impl OperationSpec {
    /// Replace a response with the same status and content type while retaining
    /// headers that were already declared for that response. The declared
    /// response becomes the most recent response so subsequent headers attach
    /// to it.
    ///
    /// Different content types for the same status are kept as separate specs;
    /// the `OpenAPI` registry combines them into one response object.
    fn upsert_response(&mut self, mut response: ResponseSpec) {
        let Some(index) = self.responses.iter().position(|existing| {
            existing.status == response.status && existing.content_type == response.content_type
        }) else {
            self.responses.push(response);
            return;
        };

        let mut existing = self.responses.remove(index);
        response.headers.append(&mut existing.headers);
        self.responses.push(response);
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct VendorExtensions {
    #[serde(rename = "x-odata-filter", skip_serializing_if = "Option::is_none")]
    pub x_odata_filter: Option<ODataPagination<BTreeMap<String, Vec<String>>>>,
    #[serde(rename = "x-odata-orderby", skip_serializing_if = "Option::is_none")]
    pub x_odata_orderby: Option<ODataPagination<Vec<String>>>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ODataPagination<T> {
    #[serde(rename = "allowedFields")]
    pub allowed_fields: T,
}

/// Computes a throttling key (identity) from an incoming request.
///
/// A code-supplied closure referenced by an operation's [`ThrottlingSpec`]. It
/// is used by the API gateway when a throttling zone is configured with
/// `key.type = identity`: the returned string becomes the per-key bucket
/// identifier (e.g. a subject id, tenant id, or a value derived from a request
/// header).
///
/// Storing a plain closure here (rather than a named trait object) keeps
/// `toolkit` free of any dependency on the API gateway gear. The gateway
/// provides an ergonomic `IdentityExtractor` trait plus an adapter that produces
/// one of these closures.
pub type IdentityKeyFn = std::sync::Arc<dyn Fn(&axum::extract::Request) -> String + Send + Sync>;

/// Per-operation throttling specification.
///
/// References throttling zones (defined in the API gateway configuration) by
/// name and, for identity-keyed zones, supplies the [`IdentityKeyFn`] used to
/// compute per-request keys. Limits themselves live in config (zones are the
/// primary source of truth); this struct only binds an operation to zones and
/// provides the code-side behavior that config cannot express.
#[derive(Clone, Default)]
pub struct ThrottlingSpec {
    /// Name of the rate-limit zone this operation participates in, or `None`
    /// when the operation is not rate-limited.
    pub rate_limit_zone: Option<String>,
    /// Name of the in-flight-limit zone this operation participates in, or
    /// `None` when the operation has no in-flight limit.
    pub in_flight_limit_zone: Option<String>,
    /// Identity key function used when the referenced zone is identity-keyed.
    pub identity_key_func: Option<IdentityKeyFn>,
    /// Whether this operation's throttling must run after authentication
    /// (so a `SecurityContext` / subject identity is available).
    ///
    /// - `false` (default): the operation is throttled *before* auth, using
    ///   IP-keyed zones only.
    /// - `true`: the operation is throttled *after* auth, allowing
    ///   identity-keyed zones (keyed by the subject id or a custom extractor).
    pub require_security_context: bool,
    /// Observe-but-don't-enforce mode.
    ///
    /// - `false` (default): limits are enforced normally (over-limit requests
    ///   are rejected).
    /// - `true`: requests are never rejected by this operation's rate-limit or
    ///   in-flight limits. Instead, whenever a limit *would* have triggered, the
    ///   gateway emits a `warn` log (with the offending key) and serves the
    ///   request. Useful for tuning zones before enabling enforcement.
    pub dry_run: bool,
}

impl std::fmt::Debug for ThrottlingSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ThrottlingSpec")
            .field("rate_limit_zone", &self.rate_limit_zone)
            .field("in_flight_limit_zone", &self.in_flight_limit_zone)
            .field(
                "identity_key_func",
                &self.identity_key_func.as_ref().map(|_| "<fn>"),
            )
            .field("require_security_context", &self.require_security_context)
            .field("dry_run", &self.dry_run)
            .finish()
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct XPagination {
    pub filter_fields: BTreeMap<String, Vec<String>>,
    pub order_by: Vec<String>,
}

//
pub trait OperationBuilderODataExt<S, H, R> {
    /// Adds optional `$filter` query parameter to `OpenAPI`.
    #[must_use]
    fn with_odata_filter<T>(self) -> Self
    where
        T: toolkit_odata::filter::FilterField;

    /// Adds optional `$select` query parameter to `OpenAPI`.
    #[must_use]
    fn with_odata_select(self) -> Self;

    /// Adds optional `$orderby` query parameter to `OpenAPI`.
    #[must_use]
    fn with_odata_orderby<T>(self) -> Self
    where
        T: toolkit_odata::filter::FilterField;
}

impl<S, H, R, A, L> OperationBuilderODataExt<S, H, R> for OperationBuilder<H, R, S, A, L>
where
    H: HandlerSlot<S>,
    A: AuthState,
    L: LicenseState,
{
    fn with_odata_filter<T>(mut self) -> Self
    where
        T: toolkit_odata::filter::FilterField,
    {
        use std::fmt::Write as _;
        use toolkit_odata::filter::FieldKind;

        let mut filter = self
            .spec
            .vendor_extensions
            .x_odata_filter
            .unwrap_or_default();

        let mut description = "OData v4 filter expression".to_owned();
        for field in T::FIELDS {
            let name = field.name().to_owned();
            let kind = field.kind();

            let ops: Vec<String> = match kind {
                FieldKind::String => vec!["eq", "ne", "contains", "startswith", "endswith", "in"],
                FieldKind::Uuid => vec!["eq", "ne", "in"],
                FieldKind::Bool => vec!["eq", "ne"],
                FieldKind::I64
                | FieldKind::F64
                | FieldKind::Decimal
                | FieldKind::DateTimeUtc
                | FieldKind::Date
                | FieldKind::Time => {
                    vec!["eq", "ne", "gt", "ge", "lt", "le", "in"]
                }
            }
            .into_iter()
            .map(String::from)
            .collect();

            _ = write!(description, "\n- {}: {}", name, ops.join("|"));
            filter.allowed_fields.insert(name.clone(), ops);
        }
        self.spec.params.push(ParamSpec::scalar(
            "$filter".to_owned(),
            ParamLocation::Query,
            false,
            Some(description),
            "string".to_owned(),
        ));
        self.spec.vendor_extensions.x_odata_filter = Some(filter);
        self
    }

    fn with_odata_select(mut self) -> Self {
        self.spec.params.push(ParamSpec::scalar(
            "$select".to_owned(),
            ParamLocation::Query,
            false,
            Some("OData v4 select expression".to_owned()),
            "string".to_owned(),
        ));
        self
    }

    fn with_odata_orderby<T>(mut self) -> Self
    where
        T: toolkit_odata::filter::FilterField,
    {
        use std::fmt::Write as _;
        let mut order_by = self
            .spec
            .vendor_extensions
            .x_odata_orderby
            .unwrap_or_default();
        let mut description = "OData v4 orderby expression".to_owned();
        for field in T::FIELDS {
            let name = field.name().to_owned();

            // Add sort options (asc/desc)
            let asc = format!("{name} asc");
            let desc = format!("{name} desc");

            _ = write!(description, "\n- {asc}\n- {desc}");
            if !order_by.allowed_fields.contains(&asc) {
                order_by.allowed_fields.push(asc);
            }
            if !order_by.allowed_fields.contains(&desc) {
                order_by.allowed_fields.push(desc);
            }
        }
        self.spec.params.push(ParamSpec::scalar(
            "$orderby".to_owned(),
            ParamLocation::Query,
            false,
            Some(description),
            "string".to_owned(),
        ));
        self.spec.vendor_extensions.x_odata_orderby = Some(order_by);
        self
    }
}

// Re-export from openapi_registry for backward compatibility
pub use crate::api::openapi_registry::{OpenApiRegistry, ensure_schema};

/// Type-safe operation builder with compile-time guarantees.
///
/// Generic parameters:
/// - `H`: Handler state (Missing | Present)
/// - `R`: Response state (Missing | Present)
/// - `S`: Router state type (what you put into `Router::with_state(S)`).
/// - `A`: Auth state (`AuthNotSet` | `AuthSet`)
/// - `L`: License requirement state (`LicenseNotSet` | `LicenseSet`)
#[must_use]
pub struct OperationBuilder<H = Missing, R = Missing, S = (), A = AuthNotSet, L = LicenseNotSet>
where
    H: HandlerSlot<S>,
    A: AuthState,
    L: LicenseState,
{
    spec: OperationSpec,
    method_router: <H as HandlerSlot<S>>::Slot,
    _has_handler: PhantomData<H>,
    _has_response: PhantomData<R>,
    #[allow(clippy::type_complexity)]
    _state: PhantomData<fn() -> S>, // Zero-sized marker for type-state pattern
    _auth_state: PhantomData<A>,
    _license_state: PhantomData<L>,
}

// -------------------------------------------------------------------------------------------------
// Constructors — starts with both handler and response missing, auth not set
// -------------------------------------------------------------------------------------------------
impl<S> OperationBuilder<Missing, Missing, S, AuthNotSet> {
    /// Create a new operation builder with an HTTP method and path
    pub fn new(method: Method, path: impl Into<String>) -> Self {
        let path_str = path.into();
        let handler_id = format!(
            "{}:{}",
            method.as_str().to_lowercase(),
            path_str.replace(['/', '{', '}'], "_")
        );

        Self {
            spec: OperationSpec {
                method,
                path: path_str,
                operation_id: None,
                summary: None,
                description: None,
                tags: Vec::new(),
                params: Vec::new(),
                request_body: None,
                responses: Vec::new(),
                handler_id,
                authenticated: false,
                exposed: false,
                throttling: None,
                allowed_request_content_types: None,
                vendor_extensions: VendorExtensions::default(),
                license_requirement: None,
            },
            method_router: (), // no router in Missing state
            _has_handler: PhantomData,
            _has_response: PhantomData,
            _state: PhantomData,
            _auth_state: PhantomData,
            _license_state: PhantomData,
        }
    }

    /// Convenience constructor for GET requests
    pub fn get(path: impl Into<String>) -> Self {
        let path_str = path.into();
        Self::new(Method::GET, normalize_to_axum_path(&path_str))
    }

    /// Convenience constructor for POST requests
    pub fn post(path: impl Into<String>) -> Self {
        let path_str = path.into();
        Self::new(Method::POST, normalize_to_axum_path(&path_str))
    }

    /// Convenience constructor for PUT requests
    pub fn put(path: impl Into<String>) -> Self {
        let path_str = path.into();
        Self::new(Method::PUT, normalize_to_axum_path(&path_str))
    }

    /// Convenience constructor for DELETE requests
    pub fn delete(path: impl Into<String>) -> Self {
        let path_str = path.into();
        Self::new(Method::DELETE, normalize_to_axum_path(&path_str))
    }

    /// Convenience constructor for PATCH requests
    pub fn patch(path: impl Into<String>) -> Self {
        let path_str = path.into();
        Self::new(Method::PATCH, normalize_to_axum_path(&path_str))
    }
}

// -------------------------------------------------------------------------------------------------
// Descriptive methods — available at any stage
// -------------------------------------------------------------------------------------------------
impl<H, R, S, A, L> OperationBuilder<H, R, S, A, L>
where
    H: HandlerSlot<S>,
    A: AuthState,
    L: LicenseState,
{
    /// Inspect the spec (primarily for tests)
    pub fn spec(&self) -> &OperationSpec {
        &self.spec
    }

    /// Set the operation ID
    pub fn operation_id(mut self, id: impl Into<String>) -> Self {
        self.spec.operation_id = Some(id.into());
        self
    }

    /// Attach zone-based throttling configuration to this operation.
    ///
    /// Binds the operation to gateway throttling zones (by name) and, for
    /// identity-keyed zones, supplies the [`IdentityKeyFn`] used to compute
    /// per-request keys. The limits themselves are defined in the gateway
    /// configuration.
    pub fn with_throttling(mut self, spec: ThrottlingSpec) -> Self {
        self.spec.throttling = Some(spec);
        self
    }

    /// Set the operation summary
    pub fn summary(mut self, text: impl Into<String>) -> Self {
        self.spec.summary = Some(text.into());
        self
    }

    /// Set the operation description
    pub fn description(mut self, text: impl Into<String>) -> Self {
        self.spec.description = Some(text.into());
        self
    }

    /// Add a tag to the operation
    pub fn tag(mut self, tag: impl Into<String>) -> Self {
        self.spec.tags.push(tag.into());
        self
    }

    /// Add a parameter to the operation
    pub fn param(mut self, param: ParamSpec) -> Self {
        self.spec.params.push(param);
        self
    }

    /// Add a path parameter with type inference (defaults to string)
    pub fn path_param(mut self, name: impl Into<String>, description: impl Into<String>) -> Self {
        self.spec.params.push(ParamSpec::scalar(
            name.into(),
            ParamLocation::Path,
            true,
            Some(description.into()),
            "string".to_owned(),
        ));
        self
    }

    /// Add a query parameter (defaults to string)
    pub fn query_param(
        mut self,
        name: impl Into<String>,
        required: bool,
        description: impl Into<String>,
    ) -> Self {
        self.spec.params.push(ParamSpec::scalar(
            name.into(),
            ParamLocation::Query,
            required,
            Some(description.into()),
            "string".to_owned(),
        ));
        self
    }

    /// Add a typed query parameter with explicit `OpenAPI` type
    pub fn query_param_typed(
        mut self,
        name: impl Into<String>,
        required: bool,
        description: impl Into<String>,
        param_type: impl Into<String>,
    ) -> Self {
        self.spec.params.push(ParamSpec::scalar(
            name.into(),
            ParamLocation::Query,
            required,
            Some(description.into()),
            param_type.into(),
        ));
        self
    }

    /// Register every query parameter declared by a
    /// `#[derive(toolkit_contract::QueryParams)]` struct.
    ///
    /// The generated REST routes use this so the spec and the wire format come
    /// from one declaration. Fields render as scalars or, for `Vec` fields, as
    /// `style: form, explode: true` arrays.
    pub fn query_params_from<T: toolkit_contract::query::QueryParams>(mut self) -> Self {
        for p in T::openapi_params() {
            self.spec.params.push(ParamSpec {
                name: p.name.to_owned(),
                location: ParamLocation::Query,
                required: p.required,
                description: None,
                param_type: p.openapi_type.to_owned(),
                array: p.array,
            });
        }
        self
    }

    /// Add a repeating query parameter — `?tag=a&tag=b`.
    ///
    /// `item_type` is the `OpenAPI` type of one element; the parameter renders
    /// as an array with `style: form, explode: true`, which is the encoding the
    /// generated REST client and its server extractor agree on for a `Vec<T>`
    /// field.
    pub fn query_param_array(
        mut self,
        name: impl Into<String>,
        required: bool,
        description: impl Into<String>,
        item_type: impl Into<String>,
    ) -> Self {
        self.spec.params.push(ParamSpec {
            name: name.into(),
            location: ParamLocation::Query,
            required,
            description: Some(description.into()),
            param_type: item_type.into(),
            array: true,
        });
        self
    }

    /// Attach a JSON request body by *schema name* that you've already registered.
    /// This variant sets a description (`Some(desc)`) and marks the body as **required**.
    pub fn json_request_schema(
        mut self,
        schema_name: impl Into<String>,
        desc: impl Into<String>,
    ) -> Self {
        self.spec.request_body = Some(RequestBodySpec {
            content_type: "application/json",
            description: Some(desc.into()),
            schema: RequestBodySchema::Ref {
                schema_name: schema_name.into(),
            },
            required: true,
        });
        self
    }

    /// Attach a JSON request body by *schema name* with **no** description (`None`).
    /// Marks the body as **required**.
    pub fn json_request_schema_no_desc(mut self, schema_name: impl Into<String>) -> Self {
        self.spec.request_body = Some(RequestBodySpec {
            content_type: "application/json",
            description: None,
            schema: RequestBodySchema::Ref {
                schema_name: schema_name.into(),
            },
            required: true,
        });
        self
    }

    /// Attach a JSON request body and auto-register its schema using `utoipa`.
    /// This variant sets a description (`Some(desc)`) and marks the body as **required**.
    pub fn json_request<T>(
        mut self,
        registry: &dyn OpenApiRegistry,
        desc: impl Into<String>,
    ) -> Self
    where
        T: utoipa::ToSchema + utoipa::PartialSchema + api_dto::RequestApiDto + 'static,
    {
        let name = ensure_schema::<T>(registry);
        self.spec.request_body = Some(RequestBodySpec {
            content_type: "application/json",
            description: Some(desc.into()),
            schema: RequestBodySchema::Ref { schema_name: name },
            required: true,
        });
        self
    }

    /// Attach a JSON request body (auto-register schema) with **no** description (`None`).
    /// Marks the body as **required**.
    pub fn json_request_no_desc<T>(mut self, registry: &dyn OpenApiRegistry) -> Self
    where
        T: utoipa::ToSchema + utoipa::PartialSchema + api_dto::RequestApiDto + 'static,
    {
        let name = ensure_schema::<T>(registry);
        self.spec.request_body = Some(RequestBodySpec {
            content_type: "application/json",
            description: None,
            schema: RequestBodySchema::Ref { schema_name: name },
            required: true,
        });
        self
    }

    /// Make the previously attached request body **optional** (if any).
    pub fn request_optional(mut self) -> Self {
        if let Some(rb) = &mut self.spec.request_body {
            rb.required = false;
        }
        self
    }

    /// Configure a multipart/form-data file upload request.
    ///
    /// This is a convenience helper for file upload endpoints that:
    /// - Sets the request body content type to "multipart/form-data"
    /// - Sets a description for the request body
    /// - Configures an inline object schema with a binary file field
    /// - Restricts allowed Content-Type to only "multipart/form-data"
    ///
    /// The file field will be documented in `OpenAPI` as a binary string with the
    /// given field name. This generates the correct `OpenAPI` schema for UI tools
    /// like Stoplight to display a file upload control.
    ///
    /// # Arguments
    /// * `field_name` - Name of the multipart form field (e.g., "file")
    /// * `description` - Optional description for the request body
    ///
    /// # Example
    /// ```rust
    /// # use axum::Router;
    /// # use http::StatusCode;
    /// # use toolkit::api::{
    /// #     openapi_registry::OpenApiRegistryImpl,
    /// #     operation_builder::OperationBuilder,
    /// # };
    /// # async fn upload_handler() -> &'static str { "uploaded" }
    /// # let registry = OpenApiRegistryImpl::new();
    /// # let router: Router<()> = Router::new();
    /// let router = OperationBuilder::post("/files/v1/upload")
    ///     .operation_id("upload_file")
    ///     .summary("Upload a file")
    ///     .multipart_file_request("file", Some("File to upload"))
    ///     .anonymous()
    ///     .handler(upload_handler)
    ///     .json_response(StatusCode::OK, "Upload successful")
    ///     .register(router, &registry);
    /// # let _ = router;
    /// ```
    pub fn multipart_file_request(mut self, field_name: &str, description: Option<&str>) -> Self {
        // Set request body with multipart/form-data content type
        self.spec.request_body = Some(RequestBodySpec {
            content_type: "multipart/form-data",
            description: description
                .map(|s| format!("{s} (expects field '{field_name}' with file data)")),
            schema: RequestBodySchema::MultipartFile {
                field_name: field_name.to_owned(),
            },
            required: true,
        });

        // Also configure MIME type validation
        self.spec.allowed_request_content_types = Some(vec!["multipart/form-data"]);

        self
    }

    /// Configure the request body as raw binary (application/octet-stream).
    ///
    /// This is intended for endpoints that accept the entire request body
    /// as a file or arbitrary bytes, without multipart form encoding.
    ///
    /// The `OpenAPI` schema will be:
    /// ```yaml
    /// requestBody:
    ///   required: true
    ///   content:
    ///     application/octet-stream:
    ///       schema:
    ///         type: string
    ///         format: binary
    /// ```
    ///
    /// Tools like Stoplight will render this as a single file upload control
    /// for the entire body.
    ///
    /// # Arguments
    /// * `description` - Optional description for the request body
    ///
    /// # Example
    /// ```rust
    /// # use axum::Router;
    /// # use http::StatusCode;
    /// # use toolkit::api::{
    /// #     openapi_registry::OpenApiRegistryImpl,
    /// #     operation_builder::OperationBuilder,
    /// # };
    /// # async fn upload_handler() -> &'static str { "uploaded" }
    /// # let registry = OpenApiRegistryImpl::new();
    /// # let router: Router<()> = Router::new();
    /// let router = OperationBuilder::post("/files/v1/upload")
    ///     .operation_id("upload_file")
    ///     .summary("Upload a file")
    ///     .octet_stream_request(Some("Raw file bytes to parse"))
    ///     .anonymous()
    ///     .handler(upload_handler)
    ///     .json_response(StatusCode::OK, "Upload successful")
    ///     .register(router, &registry);
    /// # let _ = router;
    /// ```
    pub fn octet_stream_request(mut self, description: Option<&str>) -> Self {
        self.spec.request_body = Some(RequestBodySpec {
            content_type: "application/octet-stream",
            description: description.map(str::to_owned),
            schema: RequestBodySchema::Binary,
            required: true,
        });

        // Also configure MIME type validation
        self.spec.allowed_request_content_types = Some(vec!["application/octet-stream"]);

        self
    }

    /// Configure allowed request MIME types for this operation.
    ///
    /// This attaches a whitelist of allowed Content-Type values (without parameters),
    /// which will be enforced by gateway middleware. If a request arrives with a
    /// Content-Type that is not in this list, gateway will return HTTP 415.
    ///
    /// This is independent of the request body schema - it only configures gateway
    /// validation and does not affect `OpenAPI` request body specifications.
    ///
    /// # Example
    /// ```rust
    /// # use axum::Router;
    /// # use http::StatusCode;
    /// # use toolkit::api::{
    /// #     openapi_registry::OpenApiRegistryImpl,
    /// #     operation_builder::OperationBuilder,
    /// # };
    /// # async fn upload_handler() -> &'static str { "uploaded" }
    /// # let registry = OpenApiRegistryImpl::new();
    /// # let router: Router<()> = Router::new();
    /// let router = OperationBuilder::post("/files/v1/upload")
    ///     .operation_id("upload_file")
    ///     .allow_content_types(&["multipart/form-data", "application/pdf"])
    ///     .anonymous()
    ///     .handler(upload_handler)
    ///     .json_response(StatusCode::OK, "Upload successful")
    ///     .register(router, &registry);
    /// # let _ = router;
    /// ```
    pub fn allow_content_types(mut self, types: &[&'static str]) -> Self {
        self.spec.allowed_request_content_types = Some(types.to_vec());
        self
    }

    /// Mark this route as **publicly visible** — registered in the gateway for
    /// external access (the *visibility* axis).
    ///
    /// This is independent of authentication (`.authenticated()` /
    /// `.anonymous()`): an exposed route may still require a JWT. Routes are
    /// **internal by default** (not registered in the gateway). Available at any
    /// stage of the builder.
    pub fn exposed(mut self) -> Self {
        self.spec.exposed = true;
        self
    }
}

/// License requirement setting — transitions `LicenseNotSet` -> `LicenseSet`
impl<H, R, S> OperationBuilder<H, R, S, AuthSet, LicenseNotSet>
where
    H: HandlerSlot<S>,
{
    /// Set (or explicitly clear) the license feature requirement for this operation.
    ///
    /// This method is only available after the auth requirement has been decided
    /// (i.e. after calling `authenticated()`).
    ///
    /// **Mandatory for authenticated endpoints:** operations configured with `authenticated()`
    /// must call `require_license_features(...)` before `register()`, because `register()` is only
    /// available once the license requirement state has transitioned to `LicenseSet`.
    ///
    /// **Not available for public endpoints:** public routes cannot (and do not need to) call this method.
    ///
    /// Pass an empty iterator (e.g. `[]`) to explicitly declare that no license feature is required.
    pub fn require_license_features<F>(
        mut self,
        licenses: impl IntoIterator<Item = F>,
    ) -> OperationBuilder<H, R, S, AuthSet, LicenseSet>
    where
        F: LicenseFeature,
    {
        let license_names: Vec<String> = licenses
            .into_iter()
            .map(|l| l.as_ref().to_owned())
            .collect();

        self.spec.license_requirement =
            (!license_names.is_empty()).then_some(LicenseReqSpec { license_names });

        OperationBuilder {
            spec: self.spec,
            method_router: self.method_router,
            _has_handler: self._has_handler,
            _has_response: self._has_response,
            _state: self._state,
            _auth_state: self._auth_state,
            _license_state: PhantomData,
        }
    }

    /// Explicitly declare that this operation does not require any license.
    ///
    /// Use this for system/infrastructure endpoints that need authentication
    /// but are not gated behind application-level license features.
    ///
    /// This transitions from `LicenseNotSet` to `LicenseSet` without
    /// attaching any license requirement.
    pub fn no_license_required(self) -> OperationBuilder<H, R, S, AuthSet, LicenseSet> {
        OperationBuilder {
            spec: self.spec,
            method_router: self.method_router,
            _has_handler: self._has_handler,
            _has_response: self._has_response,
            _state: self._state,
            _auth_state: self._auth_state,
            _license_state: PhantomData,
        }
    }
}

// -------------------------------------------------------------------------------------------------
// Auth requirement setting — transitions AuthNotSet -> AuthSet
// -------------------------------------------------------------------------------------------------
impl<H, R, S, L> OperationBuilder<H, R, S, AuthNotSet, L>
where
    H: HandlerSlot<S>,
    L: LicenseState,
{
    /// Mark this route as requiring authentication.
    ///
    /// This is a binary marker — the route requires a valid bearer token.
    /// Scope enforcement (which scopes are needed) is configured at the
    /// gateway level, not per-route.
    ///
    /// This method transitions from `AuthNotSet` to `AuthSet` state.
    ///
    /// # Example
    /// ```rust
    /// # use toolkit::api::operation_builder::{
    /// #     OperationBuilder, LicenseFeature, CORE_GLOBAL_BASE_LICENSE_FEATURE,
    /// # };
    /// # use axum::{extract::Json, Router };
    /// # use serde::{Serialize};
    /// #
    /// # #[derive(Serialize)]
    /// # pub struct User;
    /// #
    /// enum License {
    ///     Base,
    /// }
    ///
    /// impl AsRef<str> for License {
    ///     fn as_ref(&self) -> &str {
    ///         match self {
    ///             License::Base => CORE_GLOBAL_BASE_LICENSE_FEATURE,
    ///         }
    ///     }
    /// }
    ///
    /// impl LicenseFeature for License {}
    ///
    /// #
    /// # fn register_rest(
    /// #   router: axum::Router,
    /// #   api: &dyn toolkit::api::OpenApiRegistry,
    /// # ) -> anyhow::Result<axum::Router> {
    /// let router = OperationBuilder::get("/users-info/v1/users")
    ///     .authenticated()
    ///     .require_license_features::<License>([])
    ///     .handler(list_users_handler)
    ///     .json_response(axum::http::StatusCode::OK, "List of users")
    ///     .register(router, api);
    /// #  Ok(router)
    /// # }
    ///
    /// # async fn list_users_handler() -> Json<Vec<User>> {
    /// #   unimplemented!()
    /// # }
    /// ```
    pub fn authenticated(mut self) -> OperationBuilder<H, R, S, AuthSet, L> {
        self.spec.authenticated = true;
        OperationBuilder {
            spec: self.spec,
            method_router: self.method_router,
            _has_handler: self._has_handler,
            _has_response: self._has_response,
            _state: self._state,
            _auth_state: PhantomData,
            _license_state: self._license_state,
        }
    }

    /// Mark this route as **anonymous** — no authentication required (the *auth*
    /// axis).
    ///
    /// A missing `Authorization: Bearer` header is allowed; a present bearer is
    /// still always re-validated. This explicitly opts out of the
    /// `require_auth_by_default` setting and maps to the `AnonymousRoute` marker
    /// in the `OoP` per-gear middleware. It is independent of visibility — use
    /// [`exposed`](Self::exposed) to also register the route in the gateway.
    /// This method transitions from `AuthNotSet` to `AuthSet` state.
    ///
    /// # Example
    /// ```rust
    /// # use axum::Router;
    /// # use http::StatusCode;
    /// # use toolkit::api::{
    /// #     openapi_registry::OpenApiRegistryImpl,
    /// #     operation_builder::OperationBuilder,
    /// # };
    /// # async fn health_check() -> &'static str { "OK" }
    /// # let registry = OpenApiRegistryImpl::new();
    /// # let router: Router<()> = Router::new();
    /// let router = OperationBuilder::get("/users-info/v1/health")
    ///     .anonymous()
    ///     .handler(health_check)
    ///     .json_response(StatusCode::OK, "OK")
    ///     .register(router, &registry);
    /// # let _ = router;
    /// ```
    pub fn anonymous(mut self) -> OperationBuilder<H, R, S, AuthSet, LicenseSet> {
        self.spec.authenticated = false;
        OperationBuilder {
            spec: self.spec,
            method_router: self.method_router,
            _has_handler: self._has_handler,
            _has_response: self._has_response,
            _state: self._state,
            _auth_state: PhantomData,
            _license_state: PhantomData,
        }
    }

    /// Deprecated alias for the old single-axis `.public()`.
    ///
    /// The old `.public()` meant both **anonymous** (no auth) *and* **edge
    /// visible**. Those are now separate axes: [`anonymous`](Self::anonymous)
    /// (auth) and [`exposed`](Self::exposed) (visibility). This shim maps to
    /// `.anonymous().exposed()` so out-of-tree gears keep compiling for one
    /// release; a bare `.anonymous()` (the naive mechanical replacement) would
    /// silently drop the route from the edge, which this warning surfaces at
    /// compile time instead.
    #[deprecated(
        since = "0.6.21",
        note = "`.public()` split into two axes; use `.anonymous().exposed()` \
                (this alias forwards to exactly that)"
    )]
    pub fn public(self) -> OperationBuilder<H, R, S, AuthSet, LicenseSet> {
        self.anonymous().exposed()
    }
}

// -------------------------------------------------------------------------------------------------
// Handler setting — transitions Missing -> Present for handler
// -------------------------------------------------------------------------------------------------
impl<R, S, A, L> OperationBuilder<Missing, R, S, A, L>
where
    S: Clone + Send + Sync + 'static,
    A: AuthState,
    L: LicenseState,
{
    /// Set the handler for this operation (function handlers are recommended).
    ///
    /// This transitions the builder from `Missing` to `Present` handler state.
    pub fn handler<F, T>(self, h: F) -> OperationBuilder<Present, R, S, A, L>
    where
        F: Handler<T, S> + Clone + Send + 'static,
        T: 'static,
    {
        let method_router = match self.spec.method {
            Method::GET => axum::routing::get(h),
            Method::POST => axum::routing::post(h),
            Method::PUT => axum::routing::put(h),
            Method::DELETE => axum::routing::delete(h),
            Method::PATCH => axum::routing::patch(h),
            _ => axum::routing::any(|| async { axum::http::StatusCode::METHOD_NOT_ALLOWED }),
        };

        OperationBuilder {
            spec: self.spec,
            method_router, // concrete MethodRouter<S> in Present state
            _has_handler: PhantomData::<Present>,
            _has_response: self._has_response,
            _state: self._state,
            _auth_state: self._auth_state,
            _license_state: self._license_state,
        }
    }

    /// Alternative path: provide a pre-composed `MethodRouter<S>` yourself
    /// (useful to attach per-route middleware/layers).
    pub fn method_router(self, mr: MethodRouter<S>) -> OperationBuilder<Present, R, S, A, L> {
        OperationBuilder {
            spec: self.spec,
            method_router: mr, // concrete MethodRouter<S> in Present state
            _has_handler: PhantomData::<Present>,
            _has_response: self._has_response,
            _state: self._state,
            _auth_state: self._auth_state,
            _license_state: self._license_state,
        }
    }
}

// -------------------------------------------------------------------------------------------------
// Response setting — transitions Missing -> Present for response (first response)
// -------------------------------------------------------------------------------------------------
impl<H, S, A, L> OperationBuilder<H, Missing, S, A, L>
where
    H: HandlerSlot<S>,
    A: AuthState,
    L: LicenseState,
{
    /// Add a raw response spec (transitions from Missing to Present).
    pub fn response(mut self, resp: ResponseSpec) -> OperationBuilder<H, Present, S, A, L> {
        self.spec.responses.push(resp);
        OperationBuilder {
            spec: self.spec,
            method_router: self.method_router,
            _has_handler: self._has_handler,
            _has_response: PhantomData::<Present>,
            _state: self._state,
            _auth_state: self._auth_state,
            _license_state: self._license_state,
        }
    }

    /// Add a JSON response (transitions from Missing to Present).
    pub fn json_response(
        mut self,
        status: http::StatusCode,
        description: impl Into<String>,
    ) -> OperationBuilder<H, Present, S, A, L> {
        self.spec.responses.push(ResponseSpec {
            status: status.as_u16(),
            content_type: "application/json",
            description: description.into(),
            schema: None,
            headers: Vec::new(),
        });
        OperationBuilder {
            spec: self.spec,
            method_router: self.method_router,
            _has_handler: self._has_handler,
            _has_response: PhantomData::<Present>,
            _state: self._state,
            _auth_state: self._auth_state,
            _license_state: self._license_state,
        }
    }

    /// Add a body-less response (e.g. `204 No Content`) — transitions from
    /// Missing to Present.
    ///
    /// `OpenAPI` consumers and code-generators treat a `204` response with a
    /// `content` block as advertising a body, which is incorrect. Use this
    /// helper for any handler that intentionally returns no payload (typical
    /// for `DELETE` / `PUT` semantics).
    pub fn no_content_response(
        mut self,
        status: http::StatusCode,
        description: impl Into<String>,
    ) -> OperationBuilder<H, Present, S, A, L> {
        self.spec.responses.push(ResponseSpec {
            status: status.as_u16(),
            content_type: "",
            description: description.into(),
            schema: None,
            headers: Vec::new(),
        });
        OperationBuilder {
            spec: self.spec,
            method_router: self.method_router,
            _has_handler: self._has_handler,
            _has_response: PhantomData::<Present>,
            _state: self._state,
            _auth_state: self._auth_state,
            _license_state: self._license_state,
        }
    }

    /// Add a JSON response with a registered schema (transitions from Missing to Present).
    pub fn json_response_with_schema<T>(
        mut self,
        registry: &dyn OpenApiRegistry,
        status: http::StatusCode,
        description: impl Into<String>,
    ) -> OperationBuilder<H, Present, S, A, L>
    where
        T: utoipa::ToSchema + utoipa::PartialSchema + api_dto::ResponseApiDto + 'static,
    {
        let name = ensure_schema::<T>(registry);
        self.spec.responses.push(ResponseSpec {
            status: status.as_u16(),
            content_type: "application/json",
            description: description.into(),
            schema: Some(ResponseSchema::Ref { schema_name: name }),
            headers: Vec::new(),
        });
        OperationBuilder {
            spec: self.spec,
            method_router: self.method_router,
            _has_handler: self._has_handler,
            _has_response: PhantomData::<Present>,
            _state: self._state,
            _auth_state: self._auth_state,
            _license_state: self._license_state,
        }
    }

    /// Add a JSON response whose body is a **top-level array** of `T`
    /// (transitions from Missing to Present).
    ///
    /// `T` is the *item* type — pass `GearDto`, not `Vec<GearDto>`. Registers
    /// `T` as a named component and emits an inline
    /// `{type: array, items: {$ref: T}}` schema for the response body.
    ///
    /// Never pass `Vec<T>` to [`Self::json_response_with_schema`]: utoipa's
    /// default `ToSchema::name()` strips generic arguments, so every `Vec<_>`
    /// registers under the single component name `Vec` and two such responses
    /// collide fatally in `OpenApiRegistryImpl::ensure_schema_raw`.
    pub fn json_array_response_with_schema<T>(
        mut self,
        registry: &dyn OpenApiRegistry,
        status: http::StatusCode,
        description: impl Into<String>,
    ) -> OperationBuilder<H, Present, S, A, L>
    where
        T: utoipa::ToSchema + utoipa::PartialSchema + api_dto::ResponseApiDto + 'static,
    {
        let items_schema_name = ensure_schema::<T>(registry);
        self.spec.responses.push(ResponseSpec {
            status: status.as_u16(),
            content_type: "application/json",
            description: description.into(),
            schema: Some(ResponseSchema::Array { items_schema_name }),
            headers: Vec::new(),
        });
        OperationBuilder {
            spec: self.spec,
            method_router: self.method_router,
            _has_handler: self._has_handler,
            _has_response: PhantomData::<Present>,
            _state: self._state,
            _auth_state: self._auth_state,
            _license_state: self._license_state,
        }
    }

    /// Add a text response with a custom content type (transitions from Missing to Present).
    ///
    /// # Arguments
    /// * `status` - HTTP status code
    /// * `description` - Description of the response
    /// * `content_type` - **Pure media type without parameters** (e.g., `"text/plain"`, `"text/markdown"`)
    ///
    /// # Important
    /// The `content_type` must be a pure media type **without parameters** like `; charset=utf-8`.
    /// `OpenAPI` media type keys cannot include parameters. Use `"text/markdown"` instead of
    /// `"text/markdown; charset=utf-8"`. Actual HTTP response headers in handlers should still
    /// include the charset parameter.
    pub fn text_response(
        mut self,
        status: http::StatusCode,
        description: impl Into<String>,
        content_type: &'static str,
    ) -> OperationBuilder<H, Present, S, A, L> {
        self.spec.responses.push(ResponseSpec {
            status: status.as_u16(),
            content_type,
            description: description.into(),
            schema: None,
            headers: Vec::new(),
        });
        OperationBuilder {
            spec: self.spec,
            method_router: self.method_router,
            _has_handler: self._has_handler,
            _has_response: PhantomData::<Present>,
            _state: self._state,
            _auth_state: self._auth_state,
            _license_state: self._license_state,
        }
    }

    /// Add an HTML response (transitions from Missing to Present).
    pub fn html_response(
        mut self,
        status: http::StatusCode,
        description: impl Into<String>,
    ) -> OperationBuilder<H, Present, S, A, L> {
        self.spec.responses.push(ResponseSpec {
            status: status.as_u16(),
            content_type: "text/html",
            description: description.into(),
            schema: None,
            headers: Vec::new(),
        });
        OperationBuilder {
            spec: self.spec,
            method_router: self.method_router,
            _has_handler: self._has_handler,
            _has_response: PhantomData::<Present>,
            _state: self._state,
            _auth_state: self._auth_state,
            _license_state: self._license_state,
        }
    }

    /// Add an RFC 9457 `application/problem+json` response (transitions from Missing to Present).
    pub fn problem_response(
        mut self,
        registry: &dyn OpenApiRegistry,
        status: http::StatusCode,
        description: impl Into<String>,
    ) -> OperationBuilder<H, Present, S, A, L> {
        // Canonical Problem schema (RFC 9457 + GTS-typed). Component name "Problem".
        let problem_name = ensure_schema::<toolkit_canonical_errors::Problem>(registry);
        self.spec.responses.push(ResponseSpec {
            status: status.as_u16(),
            content_type: problem::APPLICATION_PROBLEM_JSON,
            description: description.into(),
            schema: Some(ResponseSchema::Ref {
                schema_name: problem_name,
            }),
            headers: Vec::new(),
        });
        OperationBuilder {
            spec: self.spec,
            method_router: self.method_router,
            _has_handler: self._has_handler,
            _has_response: PhantomData::<Present>,
            _state: self._state,
            _auth_state: self._auth_state,
            _license_state: self._license_state,
        }
    }

    /// First response: SSE stream of JSON events (`text/event-stream`).
    pub fn sse_json<T>(
        mut self,
        openapi: &dyn OpenApiRegistry,
        description: impl Into<String>,
    ) -> OperationBuilder<H, Present, S, A, L>
    where
        T: utoipa::ToSchema + utoipa::PartialSchema + api_dto::ResponseApiDto + 'static,
    {
        let name = ensure_schema::<T>(openapi);
        self.spec.responses.push(ResponseSpec {
            status: http::StatusCode::OK.as_u16(),
            content_type: "text/event-stream",
            description: description.into(),
            schema: Some(ResponseSchema::Ref { schema_name: name }),
            headers: Vec::new(),
        });
        OperationBuilder {
            spec: self.spec,
            method_router: self.method_router,
            _has_handler: self._has_handler,
            _has_response: PhantomData::<Present>,
            _state: self._state,
            _auth_state: self._auth_state,
            _license_state: self._license_state,
        }
    }
}

// -------------------------------------------------------------------------------------------------
// Additional responses — for Present response state (additional responses)
// -------------------------------------------------------------------------------------------------
impl<H, S, A, L> OperationBuilder<H, Present, S, A, L>
where
    H: HandlerSlot<S>,
    A: AuthState,
    L: LicenseState,
{
    /// Declare a header on the most recently declared response.
    ///
    /// Call this immediately after the response declaration it describes.
    /// Consecutive calls attach multiple headers to that same response.
    ///
    /// # Panics
    /// Panics when the response status already has a header with the same
    /// case-insensitive name.
    pub fn response_header(mut self, header: ResponseHeaderSpec) -> Self {
        let Some(response) = self.spec.responses.last() else {
            unreachable!("Present response state guarantees a response");
        };
        let status = response.status;
        assert!(
            !self.spec.responses.iter().any(|response| {
                response.status == status
                    && response
                        .headers
                        .iter()
                        .any(|existing| existing.name.eq_ignore_ascii_case(&header.name))
            }),
            "response {status} already declares header '{}'",
            header.name
        );
        let Some(response) = self.spec.responses.last_mut() else {
            unreachable!("Present response state guarantees a response");
        };
        response.headers.push(header);
        self
    }

    /// Add a JSON response (additional).
    pub fn json_response(
        mut self,
        status: http::StatusCode,
        description: impl Into<String>,
    ) -> Self {
        self.spec.upsert_response(ResponseSpec {
            status: status.as_u16(),
            content_type: "application/json",
            description: description.into(),
            schema: None,
            headers: Vec::new(),
        });
        self
    }

    /// Add a body-less response (e.g. `204 No Content`) — additional variant.
    pub fn no_content_response(
        mut self,
        status: http::StatusCode,
        description: impl Into<String>,
    ) -> Self {
        self.spec.upsert_response(ResponseSpec {
            status: status.as_u16(),
            content_type: "",
            description: description.into(),
            schema: None,
            headers: Vec::new(),
        });
        self
    }

    /// Add a JSON response with a registered schema (additional).
    pub fn json_response_with_schema<T>(
        mut self,
        registry: &dyn OpenApiRegistry,
        status: http::StatusCode,
        description: impl Into<String>,
    ) -> Self
    where
        T: utoipa::ToSchema + utoipa::PartialSchema + api_dto::ResponseApiDto + 'static,
    {
        let name = ensure_schema::<T>(registry);
        self.spec.upsert_response(ResponseSpec {
            status: status.as_u16(),
            content_type: "application/json",
            description: description.into(),
            schema: Some(ResponseSchema::Ref { schema_name: name }),
            headers: Vec::new(),
        });
        self
    }

    /// Add a JSON response whose body is a **top-level array** of `T` (additional).
    ///
    /// `T` is the *item* type — pass `GearDto`, not `Vec<GearDto>`. See
    /// [`OperationBuilder::json_array_response_with_schema`] on the
    /// `Missing`-response builder for why arrays are emitted inline.
    pub fn json_array_response_with_schema<T>(
        mut self,
        registry: &dyn OpenApiRegistry,
        status: http::StatusCode,
        description: impl Into<String>,
    ) -> Self
    where
        T: utoipa::ToSchema + utoipa::PartialSchema + api_dto::ResponseApiDto + 'static,
    {
        let items_schema_name = ensure_schema::<T>(registry);
        self.spec.upsert_response(ResponseSpec {
            status: status.as_u16(),
            content_type: "application/json",
            description: description.into(),
            schema: Some(ResponseSchema::Array { items_schema_name }),
            headers: Vec::new(),
        });
        self
    }

    /// Add a text response with a custom content type (additional).
    ///
    /// # Arguments
    /// * `status` - HTTP status code
    /// * `description` - Description of the response
    /// * `content_type` - **Pure media type without parameters** (e.g., `"text/plain"`, `"text/markdown"`)
    ///
    /// # Important
    /// The `content_type` must be a pure media type **without parameters** like `; charset=utf-8`.
    /// `OpenAPI` media type keys cannot include parameters. Use `"text/markdown"` instead of
    /// `"text/markdown; charset=utf-8"`. Actual HTTP response headers in handlers should still
    /// include the charset parameter.
    pub fn text_response(
        mut self,
        status: http::StatusCode,
        description: impl Into<String>,
        content_type: &'static str,
    ) -> Self {
        self.spec.upsert_response(ResponseSpec {
            status: status.as_u16(),
            content_type,
            description: description.into(),
            schema: None,
            headers: Vec::new(),
        });
        self
    }

    /// Add an HTML response (additional).
    pub fn html_response(
        mut self,
        status: http::StatusCode,
        description: impl Into<String>,
    ) -> Self {
        self.spec.upsert_response(ResponseSpec {
            status: status.as_u16(),
            content_type: "text/html",
            description: description.into(),
            schema: None,
            headers: Vec::new(),
        });
        self
    }

    /// Add an additional RFC 9457 `application/problem+json` response.
    pub fn problem_response(
        mut self,
        registry: &dyn OpenApiRegistry,
        status: http::StatusCode,
        description: impl Into<String>,
    ) -> Self {
        // Canonical Problem schema (RFC 9457 + GTS-typed). Component name "Problem".
        let problem_name = ensure_schema::<toolkit_canonical_errors::Problem>(registry);
        self.spec.upsert_response(ResponseSpec {
            status: status.as_u16(),
            content_type: problem::APPLICATION_PROBLEM_JSON,
            description: description.into(),
            schema: Some(ResponseSchema::Ref {
                schema_name: problem_name,
            }),
            headers: Vec::new(),
        });
        self
    }

    /// Additional SSE response (if the operation already has a response).
    pub fn sse_json<T>(
        mut self,
        openapi: &dyn OpenApiRegistry,
        description: impl Into<String>,
    ) -> Self
    where
        T: utoipa::ToSchema + utoipa::PartialSchema + api_dto::ResponseApiDto + 'static,
    {
        let name = ensure_schema::<T>(openapi);
        self.spec.upsert_response(ResponseSpec {
            status: http::StatusCode::OK.as_u16(),
            content_type: "text/event-stream",
            description: description.into(),
            schema: Some(ResponseSchema::Ref { schema_name: name }),
            headers: Vec::new(),
        });
        self
    }

    /// Add standard error responses (400, 401, 403, 404, 409, 422, 429, 500).
    ///
    /// All responses reference the shared Problem schema (RFC 9457) for consistent
    /// error handling across your API. This is the recommended way to declare
    /// common error responses without repeating boilerplate.
    ///
    /// # Example
    ///
    /// ```rust
    /// # use axum::Router;
    /// # use http::StatusCode;
    /// # use toolkit::api::{
    /// #     openapi_registry::OpenApiRegistryImpl,
    /// #     operation_builder::OperationBuilder,
    /// # };
    /// # async fn list_users() -> &'static str { "[]" }
    /// # let registry = OpenApiRegistryImpl::new();
    /// # let router: Router<()> = Router::new();
    /// let op = OperationBuilder::get("/user-info/v1/users")
    ///     .anonymous()
    ///     .handler(list_users)
    ///     .json_response(StatusCode::OK, "List of users")
    ///     .standard_errors(&registry);
    ///
    /// let router = op.register(router, &registry);
    /// # let _ = router;
    /// ```
    ///
    /// This adds the following error responses:
    /// - 400 Bad Request
    /// - 401 Unauthorized
    /// - 403 Forbidden
    /// - 404 Not Found
    /// - 409 Conflict
    /// - 429 Too Many Requests
    /// - 500 Internal Server Error
    ///
    /// 413/415/422 are intentionally absent here: canonical `InvalidArgument`
    /// maps to 400 per `docs/arch/errors/DESIGN.md` §1.2, so no
    /// canonical-handler path alone produces those statuses. An operation
    /// whose handler takes `toolkit::api::rest::extract::Json<T>` can produce
    /// all three (oversized body, wrong `Content-Type`, schema violation) -
    /// add [`Self::error_413`]/[`Self::error_415`]/[`Self::error_422`]
    /// individually for such an operation.
    pub fn standard_errors(mut self, registry: &dyn OpenApiRegistry) -> Self {
        use http::StatusCode;
        // Canonical Problem schema (RFC 9457 + GTS-typed). Component name "Problem".
        let problem_name = ensure_schema::<toolkit_canonical_errors::Problem>(registry);

        let standard_errors = [
            (StatusCode::BAD_REQUEST, "Bad Request"),
            (StatusCode::UNAUTHORIZED, "Unauthorized"),
            (StatusCode::FORBIDDEN, "Forbidden"),
            (StatusCode::NOT_FOUND, "Not Found"),
            (StatusCode::CONFLICT, "Conflict"),
            (StatusCode::TOO_MANY_REQUESTS, "Too Many Requests"),
            (StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error"),
        ];

        for (status, description) in standard_errors {
            self.spec.upsert_response(ResponseSpec {
                status: status.as_u16(),
                content_type: problem::APPLICATION_PROBLEM_JSON,
                description: description.to_owned(),
                schema: Some(ResponseSchema::Ref {
                    schema_name: problem_name.clone(),
                }),
                headers: Vec::new(),
            });
        }

        self
    }

    /// Add 400 validation error response using the canonical `Problem` schema.
    ///
    /// Field-level violations surface under `context.field_violations[]`
    /// (canonical `InvalidArgument` category, HTTP 400 per
    /// `docs/arch/errors/DESIGN.md` §1.2 / §3.5).
    ///
    /// # Example
    ///
    /// ```rust
    /// # use axum::Router;
    /// # use http::StatusCode;
    /// # use toolkit::api::{
    /// #     openapi_registry::OpenApiRegistryImpl,
    /// #     operation_builder::OperationBuilder,
    /// # };
    /// # use serde::{Deserialize, Serialize};
    /// # use utoipa::ToSchema;
    /// #
    /// #[toolkit_macros::api_dto(request)]
    /// struct CreateUserRequest {
    ///     email: String,
    /// }
    ///
    /// # async fn create_user() -> &'static str { "created" }
    /// # let registry = OpenApiRegistryImpl::new();
    /// # let router: Router<()> = Router::new();
    /// let op = OperationBuilder::post("/users-info/v1/users")
    ///     .anonymous()
    ///     .handler(create_user)
    ///     .json_request::<CreateUserRequest>(&registry, "User data")
    ///     .json_response(StatusCode::CREATED, "User created")
    ///     .with_400_validation_error(&registry);
    ///
    /// let router = op.register(router, &registry);
    /// # let _ = router;
    /// ```
    pub fn with_400_validation_error(mut self, registry: &dyn OpenApiRegistry) -> Self {
        let problem_name = ensure_schema::<toolkit_canonical_errors::Problem>(registry);

        self.spec.upsert_response(ResponseSpec {
            status: http::StatusCode::BAD_REQUEST.as_u16(),
            content_type: problem::APPLICATION_PROBLEM_JSON,
            description: "Validation Error".to_owned(),
            schema: Some(ResponseSchema::Ref {
                schema_name: problem_name,
            }),
            headers: Vec::new(),
        });

        self
    }

    /// Add a 400 Bad Request error response.
    ///
    /// This is a convenience wrapper around `problem_response`.
    pub fn error_400(self, registry: &dyn OpenApiRegistry) -> Self {
        self.problem_response(registry, http::StatusCode::BAD_REQUEST, "Bad Request")
    }

    /// Add a 401 Unauthorized error response.
    ///
    /// This is a convenience wrapper around `problem_response`.
    pub fn error_401(self, registry: &dyn OpenApiRegistry) -> Self {
        self.problem_response(registry, http::StatusCode::UNAUTHORIZED, "Unauthorized")
    }

    /// Add a 403 Forbidden error response.
    ///
    /// This is a convenience wrapper around `problem_response`.
    pub fn error_403(self, registry: &dyn OpenApiRegistry) -> Self {
        self.problem_response(registry, http::StatusCode::FORBIDDEN, "Forbidden")
    }

    /// Add a 404 Not Found error response.
    ///
    /// This is a convenience wrapper around `problem_response`.
    pub fn error_404(self, registry: &dyn OpenApiRegistry) -> Self {
        self.problem_response(registry, http::StatusCode::NOT_FOUND, "Not Found")
    }

    /// Add a 409 Conflict error response.
    ///
    /// This is a convenience wrapper around `problem_response`.
    pub fn error_409(self, registry: &dyn OpenApiRegistry) -> Self {
        self.problem_response(registry, http::StatusCode::CONFLICT, "Conflict")
    }

    /// Add a 413 Payload Too Large error response.
    ///
    /// This is a convenience wrapper around `problem_response`. Relevant to
    /// any operation whose handler takes
    /// [`toolkit::api::rest::extract::Json<T>`](crate::api::rest::extract::Json)
    /// as a parameter - an oversized request body produces this status.
    pub fn error_413(self, registry: &dyn OpenApiRegistry) -> Self {
        self.problem_response(
            registry,
            http::StatusCode::PAYLOAD_TOO_LARGE,
            "Payload Too Large",
        )
    }

    /// Add a 415 Unsupported Media Type error response.
    ///
    /// This is a convenience wrapper around `problem_response`.
    pub fn error_415(self, registry: &dyn OpenApiRegistry) -> Self {
        self.problem_response(
            registry,
            http::StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "Unsupported Media Type",
        )
    }

    /// Add a 422 Unprocessable Entity error response.
    ///
    /// This is a convenience wrapper around `problem_response`.
    pub fn error_422(self, registry: &dyn OpenApiRegistry) -> Self {
        self.problem_response(
            registry,
            http::StatusCode::UNPROCESSABLE_ENTITY,
            "Unprocessable Entity",
        )
    }

    /// Add a 429 Too Many Requests error response.
    ///
    /// This is a convenience wrapper around `problem_response`.
    pub fn error_429(self, registry: &dyn OpenApiRegistry) -> Self {
        self.problem_response(
            registry,
            http::StatusCode::TOO_MANY_REQUESTS,
            "Too Many Requests",
        )
    }

    /// Add a 500 Internal Server Error response.
    ///
    /// This is a convenience wrapper around `problem_response`.
    pub fn error_500(self, registry: &dyn OpenApiRegistry) -> Self {
        self.problem_response(
            registry,
            http::StatusCode::INTERNAL_SERVER_ERROR,
            "Internal Server Error",
        )
    }

    /// Add a 502 Bad Gateway error response.
    ///
    /// This is a convenience wrapper around `problem_response`.
    pub fn error_502(self, registry: &dyn OpenApiRegistry) -> Self {
        self.problem_response(registry, http::StatusCode::BAD_GATEWAY, "Bad Gateway")
    }

    /// Add a 503 Service Unavailable error response.
    ///
    /// This is a convenience wrapper around `problem_response`.
    pub fn error_503(self, registry: &dyn OpenApiRegistry) -> Self {
        self.problem_response(
            registry,
            http::StatusCode::SERVICE_UNAVAILABLE,
            "Service Unavailable",
        )
    }

    /// Add a 504 Gateway Timeout error response.
    ///
    /// This is a convenience wrapper around `problem_response`.
    pub fn error_504(self, registry: &dyn OpenApiRegistry) -> Self {
        self.problem_response(
            registry,
            http::StatusCode::GATEWAY_TIMEOUT,
            "Gateway Timeout",
        )
    }
}

// -------------------------------------------------------------------------------------------------
// Registration — only available when handler, response, AND auth are all set
// -------------------------------------------------------------------------------------------------
impl<S> OperationBuilder<Present, Present, S, AuthSet, LicenseSet>
where
    S: Clone + Send + Sync + 'static,
{
    /// Register the operation with the router and `OpenAPI` registry.
    ///
    /// This method is only available when:
    /// - Handler is present
    /// - Response is present
    /// - Auth requirement is set (either `authenticated` or `public`)
    ///
    /// All conditions are enforced at compile time by the type system.
    pub fn register(self, router: Router<S>, openapi: &dyn OpenApiRegistry) -> Router<S> {
        // Inform the OpenAPI registry (the implementation will translate OperationSpec
        // into an OpenAPI Operation + RequestBody + Responses with component refs).
        openapi.register_operation(&self.spec);

        // In Present state the method_router is guaranteed to be a real MethodRouter<S>.
        router.route(&self.spec.path, self.method_router)
    }
}

// -------------------------------------------------------------------------------------------------
// Tests
// -------------------------------------------------------------------------------------------------
#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "operation_builder_tests.rs"]
mod tests;
