use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;

fn default_require_auth_by_default() -> bool {
    true
}

fn default_throttle_status() -> u16 {
    429
}

fn default_body_limit_bytes() -> usize {
    16 * 1024 * 1024
}

fn default_healthcheck_timeout_ms() -> u64 {
    500
}

fn default_gateway_sync_interval_secs() -> u64 {
    10
}

/// API gateway configuration - reused from `api_gateway` gear
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
#[allow(clippy::struct_excessive_bools)]
pub struct ApiGatewayConfig {
    pub bind_addr: String,

    /// Base URL other pods use to reach this gateway's REST endpoint, published
    /// for directory registration (required in Kubernetes, where the pod binds
    /// `0.0.0.0`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub advertise_uri: Option<String>,

    #[serde(default)]
    pub enable_docs: bool,
    #[serde(default)]
    pub cors_enabled: bool,
    /// Optional detailed CORS configuration
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cors: Option<CorsConfig>,

    /// `OpenAPI` document metadata
    #[serde(default)]
    pub openapi: OpenApiConfig,

    /// Global defaults
    #[serde(default)]
    pub defaults: Defaults,

    /// Disable authentication and authorization completely.
    /// When true, middleware automatically injects a default `SecurityContext` for all requests,
    /// providing access with no tenant filtering.
    /// This bypasses all tenant isolation and should only be used for single-user on-premise installations.
    /// Default: false (authentication required via `AuthN` Resolver).
    #[serde(default)]
    pub auth_disabled: bool,

    /// If true, routes without explicit security requirement still require authentication (AuthN-only).
    #[serde(default = "default_require_auth_by_default")]
    pub require_auth_by_default: bool,

    /// Optional URL path prefix prepended to every route (e.g. `"/cf"` → `/cf/users`).
    /// Must start with a leading slash; trailing slashes are stripped automatically.
    /// Empty string (the default) means no prefix.
    #[serde(default)]
    pub prefix_path: String,

    /// Route-level policy configuration.
    /// Allows early rejection of requests based on token scopes without calling the PDP.
    /// Rules are evaluated in declaration order (first match wins).
    #[serde(default)]
    pub route_policies: RoutePoliciesConfig,

    /// HTTP metrics configuration.
    #[serde(default)]
    pub metrics: MetricsConfig,

    /// Per-check `/health`/`/readyz` timeout (ms); raise for slow dependencies.
    #[serde(default = "default_healthcheck_timeout_ms")]
    pub healthcheck_timeout_ms: u64,

    /// Health-probe serving configuration (listener placement, bind address).
    #[serde(default)]
    pub health: HealthConfig,

    /// Reverse-proxy (embedded edge) configuration. When enabled, the gateway
    /// discovers out-of-process gears via the `DirectoryService` and
    /// reverse-proxies their public routes. Default: disabled (Profile 1
    /// monolith is unaffected).
    #[serde(default)]
    pub gateway_proxy: GatewayProxyConfig,

    /// Platform-plane credential for the embedded platform-host, serving **both
    /// directions** from one block:
    ///
    /// - **Inbound** — `internal_auth_middleware`, installed ahead of the tenant
    ///   plane, validates the `X-ToolKit-Internal-Token` on co-hosted internal
    ///   routes (e.g. `authz-resolver`'s `/evaluate`) into a
    ///   `PlatformSecurityContext`. Permissive on a missing token, so tenant
    ///   traffic is unaffected. Kube uses `audiences`.
    /// - **Outbound** — the credential attached to the edge's own
    ///   `DirectoryService` polls (when [`GatewayProxyConfig::enabled`]). Kube
    ///   uses `token_path`; omit it to attach no outbound credential.
    ///
    /// Each direction reads only the fields it needs, so one block (e.g.
    /// `Kube { audiences, token_path }`) configures the whole platform plane
    /// (`cpt-cf-adr-two-plane-auth`). Default: `None` (Profile 1 monolith).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub internal_auth: Option<toolkit_security::InternalAuthConfig>,

    /// Named rate-limit zones (token-bucket limits keyed per zone strategy).
    #[serde(default)]
    pub rate_limit_zones: BTreeMap<String, RateLimitZone>,

    /// Named in-flight (concurrency) limit zones.
    #[serde(default)]
    pub in_flight_limit_zones: BTreeMap<String, InFlightLimitZone>,

    /// Number of trusted reverse-proxy hops in front of the gateway.
    ///
    /// Controls how the client IP is derived for IP-keyed throttling. When `0`
    /// (the default) the immediate peer address from `ConnectInfo` is used and
    /// the client-supplied `X-Forwarded-For` / `X-Real-IP` headers are ignored,
    /// preventing a caller from spoofing or rotating the throttling bucket key
    /// to bypass pre-auth IP limits. When set to `n >= 1`, the client IP is
    /// taken from the `X-Forwarded-For` entry `n` positions from the right (the
    /// value written by the outermost trusted proxy), which an untrusted client
    /// cannot forge.
    #[serde(default)]
    pub trusted_proxy_hops: usize,
}

/// Reverse-proxy (embedded edge) configuration.
///
/// When [`enabled`](Self::enabled), a background task polls the
/// `DirectoryService` at [`directory_endpoint`](Self::directory_endpoint) and
/// keeps a reverse-proxy route table in sync, so requests for out-of-process
/// gears' public routes are forwarded to the owning gear pod.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct GatewayProxyConfig {
    /// Enable the directory-driven reverse proxy.
    pub enabled: bool,
    /// gRPC endpoint of the `DirectoryService` (e.g. the grpc-hub endpoint,
    /// `http://gear-orchestrator:50051`). Required when `enabled` is true.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub directory_endpoint: Option<String>,
    /// Interval (seconds) between directory polls that refresh the proxy route
    /// table.
    pub sync_interval_secs: u64,
}

impl Default for GatewayProxyConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            directory_endpoint: None,
            sync_interval_secs: default_gateway_sync_interval_secs(),
        }
    }
}

impl GatewayProxyConfig {
    /// Validate the reverse-proxy configuration at config-load time.
    ///
    /// `enabled: true` without a `directory_endpoint` would otherwise start a
    /// proxy that can never populate its route table: it emits a single `warn!`
    /// and then silently serves `404` for every out-of-process route while the
    /// gateway still reports healthy. Failing `init` instead surfaces a clean,
    /// actionable error at startup rather than a runtime surprise.
    ///
    /// # Errors
    /// Returns a human-readable description when `enabled` is `true` but
    /// `directory_endpoint` is unset or blank.
    pub fn validate(&self) -> Result<(), String> {
        if !self.enabled {
            return Ok(());
        }
        match self.directory_endpoint.as_deref() {
            Some(endpoint) if !endpoint.trim().is_empty() => Ok(()),
            _ => Err(
                "invalid gateway_proxy configuration: `gateway_proxy.enabled` is true but \
                 `gateway_proxy.directory_endpoint` is unset or empty; set it to the \
                 DirectoryService gRPC endpoint (e.g. \"http://gear-orchestrator:50051\") \
                 or disable the reverse proxy"
                    .to_owned(),
            ),
        }
    }
}

/// Which listener(s) serve the health-probe endpoints (`/healthz`, `/readyz`, `/health`).
///
/// Health endpoints are unauthenticated in every mode; see [`HealthConfig`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum HealthServeMode {
    /// Serve health endpoints on the main gateway listener (`bind_addr`). Default.
    #[default]
    Main,
    /// Serve health endpoints only on a separate listener (`health.bind_addr`), e.g. a
    /// private/management port not exposed alongside the main API.
    Separate,
    /// Serve health endpoints on both the main and the separate listener.
    Both,
}

/// Health-probe serving configuration.
///
/// The endpoints (`/healthz`, `/readyz`, `/health`) never require authentication in any mode.
/// In `main`/`both` mode they ride the main listener as public routes on the gateway
/// middleware surface and inherit `prefix_path` (e.g. `/cf/healthz`); operators must keep the
/// main gateway on a private network if the per-component detail in `/health` is sensitive. In
/// `separate`/`both` mode they are also served at unprefixed paths on `bind_addr`, protected
/// only by network placement of that listener.
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(deny_unknown_fields, default)]
pub struct HealthConfig {
    /// Which listener(s) serve the health endpoints.
    pub serve: HealthServeMode,
    /// Bind address for the separate health listener (e.g. `"0.0.0.0:8081"`).
    /// Required when `serve` is `separate` or `both`; unused for `main`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bind_addr: Option<String>,
}

impl Default for ApiGatewayConfig {
    // Manual (not derived) so defaults match the `#[serde(default = ...)]` fns above;
    // a derived Default would zero each field (e.g. timeout 0 = instant probe failure).
    fn default() -> Self {
        Self {
            bind_addr: String::default(),
            advertise_uri: None,
            enable_docs: false,
            cors_enabled: false,
            cors: None,
            openapi: OpenApiConfig::default(),
            defaults: Defaults::default(),
            auth_disabled: false,
            require_auth_by_default: default_require_auth_by_default(),
            prefix_path: String::default(),
            route_policies: RoutePoliciesConfig::default(),
            metrics: MetricsConfig::default(),
            healthcheck_timeout_ms: default_healthcheck_timeout_ms(),
            health: HealthConfig::default(),
            gateway_proxy: GatewayProxyConfig::default(),
            internal_auth: None,
            rate_limit_zones: BTreeMap::new(),
            in_flight_limit_zones: BTreeMap::new(),
            trusted_proxy_hops: 0,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct Defaults {
    /// Global request body size limit in bytes
    pub body_limit_bytes: usize,
}

impl Default for Defaults {
    fn default() -> Self {
        Self {
            body_limit_bytes: default_body_limit_bytes(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct CorsConfig {
    /// Allowed origins: `["*"]` means any
    pub allowed_origins: Vec<String>,
    /// Allowed HTTP methods, e.g. `["GET","POST","OPTIONS","PUT","DELETE","PATCH"]`
    pub allowed_methods: Vec<String>,
    /// Allowed request headers; `["*"]` means any
    pub allowed_headers: Vec<String>,
    /// Response headers exposed to browser JavaScript via
    /// `Access-Control-Expose-Headers`; `["*"]` means any (do not combine
    /// with `allow_credentials` — the wildcard is invalid with credentials).
    /// Defaults to `["ETag"]`: the optimistic-concurrency write contract
    /// requires clients to read the `ETag` response header to build
    /// `If-Match` guarded writes, and `ETag` is not on the CORS safelist —
    /// without exposing it every ETag-guarded write is impossible from a
    /// cross-origin browser client.
    pub exposed_headers: Vec<String>,
    /// Whether to allow credentials
    pub allow_credentials: bool,
    /// Max age for preflight caching in seconds
    pub max_age_seconds: u64,
}

impl Default for CorsConfig {
    fn default() -> Self {
        Self {
            allowed_origins: vec!["*".to_owned()],
            allowed_methods: vec![
                "GET".to_owned(),
                "POST".to_owned(),
                "PUT".to_owned(),
                "PATCH".to_owned(),
                "DELETE".to_owned(),
                "OPTIONS".to_owned(),
            ],
            allowed_headers: vec!["*".to_owned()],
            exposed_headers: vec!["ETag".to_owned()],
            allow_credentials: false,
            max_age_seconds: 600,
        }
    }
}

/// HTTP metrics configuration.
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(deny_unknown_fields, default)]
pub struct MetricsConfig {
    /// Optional prefix for HTTP metrics instrument names.
    ///
    /// When set, metric names become `{prefix}.http.server.request.duration`
    /// and `{prefix}.http.server.active_requests` instead of the default
    /// OpenTelemetry semantic convention names.
    ///
    /// Empty string (the default) means no prefix — standard `OTel` names are used.
    pub prefix: String,
}

/// `OpenAPI` document metadata configuration
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct OpenApiConfig {
    /// API title shown in `OpenAPI` documentation
    pub title: String,
    /// API version
    pub version: String,
    /// API description (optional)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

impl Default for OpenApiConfig {
    fn default() -> Self {
        Self {
            title: "API Documentation".to_owned(),
            version: "0.1.0".to_owned(),
            description: None,
        }
    }
}

/// Route-level policy configuration.
///
/// Enables coarse-grained early rejection of requests based on token scopes
/// without calling the PDP. This is an optimization for performance-critical routes.
///
/// # Example YAML
///
/// ```yaml
/// route_policies:
///   enabled: true
///   rules:
///     - path: "/admin/**"
///       required_scopes: ["admin"]
///     - path: "/events/v1/*"
///       required_scopes: ["read:events", "write:events"]  # any of these
/// ```
///
/// # Behavior
///
/// - Rules are evaluated in declaration order (first match wins)
/// - If `token_scopes: ["*"]` → always pass (first-party app)
/// - If `token_scopes` contains any of `required_scopes` → pass
/// - Otherwise → 403 Forbidden (before PDP call)
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct RoutePoliciesConfig {
    /// Whether route policy enforcement is enabled.
    pub enabled: bool,
    /// Route policy rules evaluated in declaration order.
    /// Patterns support glob syntax (e.g., `/admin/*`, `/events/v1/**`).
    pub rules: Vec<RoutePolicyRule>,
}

/// A single route policy rule.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RoutePolicyRule {
    /// Path pattern to match. Supports glob syntax (`*` = one segment, `**` = any depth).
    pub path: String,
    /// HTTP method to match (GET, POST, PUT, PATCH, DELETE, etc.).
    /// If not specified, matches any method.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    /// Required scopes for this route. Request passes if token has ANY of these scopes.
    /// Must not be empty.
    pub required_scopes: Vec<String>,
    // Future fields: rate_limit, timeout, operation_id, etc.
}

// =================================================================================================
// Throttling configuration (zone-based, NGINX-style)
// =================================================================================================

/// A steady-state rate expressed as requests-per-second.
///
/// Parsed from the string form `"<n>/s"` (e.g. `"50/s"`). Only the `/s` unit is
/// supported; any other unit is rejected with an explanatory error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateSpec {
    /// Requests per second.
    pub rps: u32,
}

impl<'de> Deserialize<'de> for RateSpec {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        let s = raw.trim();
        let value = s.strip_suffix("/s").ok_or_else(|| {
            de::Error::custom(format!(
                "invalid rate '{s}': only the '/s' (per-second) unit is supported, e.g. '50/s'"
            ))
        })?;
        let rps: u32 = value.trim().parse().map_err(|_| {
            de::Error::custom(format!(
                "invalid rate '{s}': '{value}' is not a valid integer"
            ))
        })?;
        Ok(Self { rps })
    }
}

impl Serialize for RateSpec {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&format!("{}/s", self.rps))
    }
}

/// `Retry-After` response policy for a rate-limit zone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RetryAfter {
    /// Compute the retry delay automatically from the limiter state.
    #[default]
    Auto,
    /// Always advertise a fixed number of seconds.
    Seconds(u64),
}

impl<'de> Deserialize<'de> for RetryAfter {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct RetryAfterVisitor;

        impl Visitor<'_> for RetryAfterVisitor {
            type Value = RetryAfter;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("the string \"auto\" or a non-negative integer number of seconds")
            }

            fn visit_u64<E: de::Error>(self, v: u64) -> Result<Self::Value, E> {
                Ok(RetryAfter::Seconds(v))
            }

            fn visit_i64<E: de::Error>(self, v: i64) -> Result<Self::Value, E> {
                u64::try_from(v)
                    .map(RetryAfter::Seconds)
                    .map_err(|_| E::custom("response_retry_after must be non-negative"))
            }

            fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
                if v.eq_ignore_ascii_case("auto") {
                    Ok(RetryAfter::Auto)
                } else {
                    Err(E::custom(format!(
                        "invalid response_retry_after '{v}': expected \"auto\" or a number of seconds"
                    )))
                }
            }
        }

        deserializer.deserialize_any(RetryAfterVisitor)
    }
}

impl Serialize for RetryAfter {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Auto => serializer.serialize_str("auto"),
            Self::Seconds(n) => serializer.serialize_u64(*n),
        }
    }
}

/// The keying strategy used by a throttling zone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyType {
    /// Key by a per-operation identity (supplied in code via `ThrottlingSpec`).
    /// Identity keying requires authentication, so identity-keyed zones may only
    /// be referenced by operations marked `require_security_context = true`.
    Identity,
    /// Key by client IP address. Usable before authentication.
    Ip,
}

/// Key configuration block for a zone (`key: { type: identity }`).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct KeyConfig {
    /// Keying strategy.
    #[serde(rename = "type")]
    pub key_type: KeyType,
}

/// A rate-limit zone definition.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RateLimitZone {
    /// Steady-state rate (e.g. `50/s`).
    pub rate_limit: RateSpec,
    /// Maximum burst size (token-bucket capacity).
    pub burst_limit: u32,
    /// HTTP status returned when the limit is exceeded.
    #[serde(default = "default_throttle_status")]
    pub response_status_code: u16,
    /// `Retry-After` policy.
    #[serde(default)]
    pub response_retry_after: RetryAfter,
    /// Keying strategy.
    pub key: KeyConfig,
    /// Cap on distinct keys tracked at once (approximate). Requests with a new
    /// key beyond the cap are rejected with `response_status_code` until the
    /// periodic prune drops stale keys and reopens admission.
    pub max_keys: u64,
}

/// An in-flight (concurrency) limit zone definition.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct InFlightLimitZone {
    /// Maximum number of concurrently in-flight requests per key.
    pub in_flight_limit: u32,
    /// Maximum number of queued (waiting) requests per key.
    pub backlog_limit: u32,
    /// Maximum time a request may wait in the backlog before rejection.
    #[serde(with = "humantime_serde")]
    pub backlog_timeout: Duration,
    /// HTTP status returned when the limit is exceeded.
    #[serde(default = "default_throttle_status")]
    pub response_status_code: u16,
    /// Keying strategy.
    pub key: KeyConfig,
    /// Cap on distinct keys tracked at once (approximate). Requests with a new
    /// key beyond the cap are rejected with `response_status_code` until the
    /// periodic prune drops gates not held by an in-flight request and reopens
    /// admission (a held gate is never evicted, so a cap filled by live
    /// requests stays closed until they finish).
    pub max_keys: u64,
    /// Keys that bypass this zone entirely (e.g. privileged identities).
    #[serde(default)]
    pub excluded_keys: Vec<String>,
}

impl ApiGatewayConfig {
    /// Validate the throttling configuration at load time.
    ///
    /// Checks that:
    /// - zone numeric fields are non-zero where required,
    /// - response status codes are valid HTTP statuses.
    ///
    /// # Errors
    /// Returns an error describing the first invalid entry encountered.
    pub fn validate_throttling(&self) -> anyhow::Result<()> {
        for (name, zone) in &self.rate_limit_zones {
            if zone.rate_limit.rps == 0 {
                anyhow::bail!("rate_limit_zone '{name}': rate_limit must be greater than 0");
            }
            if zone.burst_limit == 0 {
                anyhow::bail!("rate_limit_zone '{name}': burst_limit must be greater than 0");
            }
            if zone.max_keys == 0 {
                anyhow::bail!("rate_limit_zone '{name}': max_keys must be greater than 0");
            }
            validate_status(name, zone.response_status_code)?;
        }

        for (name, zone) in &self.in_flight_limit_zones {
            if zone.in_flight_limit == 0 {
                anyhow::bail!(
                    "in_flight_limit_zone '{name}': in_flight_limit must be greater than 0"
                );
            }
            if zone.max_keys == 0 {
                anyhow::bail!("in_flight_limit_zone '{name}': max_keys must be greater than 0");
            }
            validate_status(name, zone.response_status_code)?;
        }

        Ok(())
    }
}

fn validate_status(zone: &str, status: u16) -> anyhow::Result<()> {
    let code = http::StatusCode::from_u16(status)
        .map_err(|_| anyhow::anyhow!("zone '{zone}': invalid response_status_code {status}"))?;
    if !(code.is_client_error() || code.is_server_error()) {
        anyhow::bail!(
            "zone '{zone}': response_status_code {status} must be a 4xx or 5xx error status"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::GatewayProxyConfig;

    #[test]
    fn disabled_proxy_needs_no_endpoint() {
        let cfg = GatewayProxyConfig::default();
        assert!(!cfg.enabled);
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn enabled_proxy_without_endpoint_is_rejected() {
        let cfg = GatewayProxyConfig {
            enabled: true,
            directory_endpoint: None,
            ..GatewayProxyConfig::default()
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn enabled_proxy_with_blank_endpoint_is_rejected() {
        let cfg = GatewayProxyConfig {
            enabled: true,
            directory_endpoint: Some("   ".to_owned()),
            ..GatewayProxyConfig::default()
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn enabled_proxy_with_endpoint_is_accepted() {
        let cfg = GatewayProxyConfig {
            enabled: true,
            directory_endpoint: Some("http://gear-orchestrator:50051".to_owned()),
            ..GatewayProxyConfig::default()
        };
        assert!(cfg.validate().is_ok());
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod throttling_tests {
    use super::*;

    const FULL_CONFIG: &str = r#"
bind_addr: "0.0.0.0:8086"
rate_limit_zones:
  rl_identity:
    rate_limit: 50/s
    burst_limit: 100
    response_status_code: 429
    response_retry_after: auto
    key:
      type: identity
    max_keys: 50000
in_flight_limit_zones:
  ifl_identity:
    in_flight_limit: 64
    backlog_limit: 128
    backlog_timeout: 30s
    response_status_code: 429
    key:
      type: identity
    max_keys: 50000
  ifl_identity_expensive_op:
    in_flight_limit: 4
    backlog_limit: 8
    backlog_timeout: 30s
    response_status_code: 429
    key:
      type: identity
    max_keys: 50000
    excluded_keys:
      - "150853ab-322c-455d-9793-8d71bf6973d9"
"#;

    fn parse(yaml: &str) -> ApiGatewayConfig {
        serde_saphyr::from_str(yaml).expect("config should deserialize")
    }

    #[test]
    fn parses_rate_limit_zones() {
        let cfg = parse(FULL_CONFIG);
        assert_eq!(cfg.rate_limit_zones.len(), 1);
        let rl = &cfg.rate_limit_zones["rl_identity"];
        assert_eq!(rl.rate_limit, RateSpec { rps: 50 });
        assert_eq!(rl.burst_limit, 100);
        assert_eq!(rl.response_status_code, 429);
        assert_eq!(rl.response_retry_after, RetryAfter::Auto);
        assert_eq!(rl.key.key_type, KeyType::Identity);
        assert_eq!(rl.max_keys, 50000);
    }

    #[test]
    fn parses_in_flight_zones() {
        let cfg = parse(FULL_CONFIG);
        assert_eq!(cfg.in_flight_limit_zones.len(), 2);

        let ifl = &cfg.in_flight_limit_zones["ifl_identity"];
        assert_eq!(ifl.in_flight_limit, 64);
        assert_eq!(ifl.backlog_limit, 128);
        assert_eq!(ifl.backlog_timeout, Duration::from_secs(30));
        assert!(ifl.excluded_keys.is_empty());

        let expensive = &cfg.in_flight_limit_zones["ifl_identity_expensive_op"];
        assert_eq!(expensive.in_flight_limit, 4);
        assert_eq!(
            expensive.excluded_keys,
            vec!["150853ab-322c-455d-9793-8d71bf6973d9".to_owned()]
        );
    }

    #[test]
    fn full_config_validates() {
        parse(FULL_CONFIG)
            .validate_throttling()
            .expect("config is valid");
    }

    #[test]
    fn rate_spec_rejects_non_per_second_unit() {
        let err = serde_saphyr::from_str::<RateSpec>("50/m")
            .unwrap_err()
            .to_string();
        assert!(err.contains("/s"), "unexpected error: {err}");
    }

    #[test]
    fn rate_spec_rejects_non_integer() {
        assert!(serde_saphyr::from_str::<RateSpec>("abc/s").is_err());
    }

    #[test]
    fn retry_after_parses_auto_and_seconds() {
        assert_eq!(
            serde_saphyr::from_str::<RetryAfter>("auto").unwrap(),
            RetryAfter::Auto
        );
        assert_eq!(
            serde_saphyr::from_str::<RetryAfter>("15").unwrap(),
            RetryAfter::Seconds(15)
        );
    }

    #[test]
    fn validate_status_accepts_4xx_5xx_rejects_others() {
        // 4xx / 5xx are valid throttle statuses.
        assert!(validate_status("z", 429).is_ok());
        assert!(validate_status("z", 503).is_ok());
        // 2xx / 3xx are parseable but not error statuses.
        let err = validate_status("zone_a", 200).unwrap_err().to_string();
        assert!(err.contains("zone_a") && err.contains("200"), "got: {err}");
        assert!(validate_status("z", 302).is_err());
        // Out-of-range codes remain rejected as invalid.
        assert!(validate_status("z", 999).is_err());
    }

    #[test]
    fn validation_rejects_zero_limit() {
        let yaml = r#"
bind_addr: "0.0.0.0:8086"
rate_limit_zones:
  bad:
    rate_limit: 0/s
    burst_limit: 100
    key:
      type: identity
    max_keys: 100
"#;
        let cfg = parse(yaml);
        assert!(cfg.validate_throttling().is_err());
    }

    #[test]
    fn empty_config_validates() {
        let cfg = ApiGatewayConfig::default();
        cfg.validate_throttling().expect("empty config is valid");
    }
}
