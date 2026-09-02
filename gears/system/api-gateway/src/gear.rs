//! API Gateway Gear definition
//!
//! Contains the `ApiGateway` gear struct and its trait implementations.

use async_trait::async_trait;
use std::sync::{Arc, OnceLock};

use arc_swap::ArcSwap;
use dashmap::DashMap;

use anyhow::Result;
use axum::error_handling::HandleErrorLayer;
use axum::http::Method;
use axum::middleware::from_fn_with_state;
use axum::{Extension, Router, extract::DefaultBodyLimit, middleware::from_fn, routing::get};
use parking_lot::Mutex;
use std::net::SocketAddr;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use toolkit::api::{OpenApiRegistry, OpenApiRegistryImpl};
use toolkit::lifecycle::ReadySignal;
use tower::{BoxError, ServiceBuilder};
use tower_http::{
    catch_panic::CatchPanicLayer,
    limit::RequestBodyLimitLayer,
    request_id::{PropagateRequestIdLayer, SetRequestIdLayer},
};
use tracing::debug;

use crate::middleware::errors::ApiGatewayGatewayError;

/// Map a `tower::timeout` `Elapsed` (or any other unexpected `BoxError`)
/// into a canonical `application/problem+json` response.
async fn timeout_to_canonical(err: BoxError) -> axum::response::Response {
    use axum::response::IntoResponse;

    if err.is::<tower::timeout::error::Elapsed>() {
        let canonical =
            ApiGatewayGatewayError::deadline_exceeded("Request exceeded 30s timeout").create();
        return canonical.into_response();
    }

    let canonical = toolkit_canonical_errors::CanonicalError::internal(format!(
        "request pipeline error: {err}"
    ))
    .create();
    canonical.into_response()
}

use authn_resolver_sdk::AuthNResolverClient;

use cf_system_sdks::directory::{DirectoryClient, DirectoryGrpcClient};

use crate::config::{ApiGatewayConfig, GatewayProxyConfig, HealthServeMode};
use crate::middleware::auth;
use toolkit_http_middleware::internal_auth_middleware;
use toolkit_security::constants::{DEFAULT_SUBJECT_ID, DEFAULT_TENANT_ID};
use toolkit_security::{DynInternalAuthenticator, InternalAuthConfig, SecurityContext};

use crate::middleware;
use crate::router_cache::RouterCache;
use crate::web;

/// Main API Gateway gear — owns the HTTP server (`rest_host`) and collects
/// typed operation specs to emit a single `OpenAPI` document.
#[toolkit::gear(
	name = "api-gateway",
	capabilities = [rest_host, rest, stateful],
    deps = [grpc_hub, authn_resolver],
	lifecycle(entry = "serve", stop_timeout = "30s", await_ready)
)]
pub struct ApiGateway {
    // Lock-free config using arc-swap for read-mostly access
    pub(crate) config: ArcSwap<ApiGatewayConfig>,
    // OpenAPI registry for operations and schemas
    pub(crate) openapi_registry: Arc<OpenApiRegistryImpl>,
    // Built router cache for zero-lock hot path access
    pub(crate) router_cache: RouterCache<axum::Router>,
    // Store the finalized router from REST phase for serving
    pub(crate) final_router: Mutex<Option<axum::Router>>,
    // AuthN Resolver client (resolved during init, None when auth_disabled)
    pub(crate) authn_client: Mutex<Option<Arc<dyn AuthNResolverClient>>>,
    // Inbound platform-plane authenticator (built in init from
    // `config.internal_auth`, None in Profile 1). When present, the stack layers
    // `internal_auth_middleware` ahead of the tenant plane so co-hosted internal
    // routes get a validated `PlatformSecurityContext` (`cpt-cf-adr-two-plane-auth`).
    pub(crate) internal_authenticator: Mutex<Option<DynInternalAuthenticator>>,
    // Readiness registry, set once from `rest_prepare`; `OnceLock` = lock-free reads.
    pub(crate) healthcheck_registry: OnceLock<Arc<toolkit::RestHealthcheckRegistry>>,
    // Built-once standalone health router. Served on the separate health listener in
    // `separate`/`both` mode and merged onto the main router in `main`/`both` mode. Cached
    // so repeat `health_router()`/`build_health_router()` calls are cheap.
    pub(crate) health_router: OnceLock<axum::Router>,
    // Pending throttling keyed-store pruner from the last middleware build;
    // consumed by `serve` to spawn a prune task bound to the lifecycle token.
    pub(crate) throttle_key_pruner: Mutex<Option<middleware::throttling::ThrottleKeyPruner>>,

    // Duplicate detection (per (method, path) and per handler id)
    pub(crate) registered_routes: DashMap<(Method, String), ()>,
    pub(crate) registered_handlers: DashMap<String, ()>,

    // Reverse-proxy route table (embedded edge). Populated by the directory-sync
    // task and read by the Forwarder fallback; empty/unused when
    // `gateway_proxy` is disabled.
    pub(crate) proxy_registry: Arc<toolkit_gateway::ProxyRegistry>,

    // Base URL other pods use to reach this gateway, published once the main
    // listener binds (from `serve`). Read by the runtime's directory-register
    // phase (via `ApiGatewayCapability::bound_endpoint`) to advertise in-process
    // REST providers. Write-once/read-many, so `OnceLock` (lock-free reads),
    // matching `healthcheck_registry`/`health_router`; unset until the server binds.
    pub(crate) bound_endpoint: OnceLock<String>,
}

impl Default for ApiGateway {
    fn default() -> Self {
        let default_router = Router::new();
        Self {
            config: ArcSwap::from_pointee(ApiGatewayConfig::default()),
            openapi_registry: Arc::new(OpenApiRegistryImpl::new()),
            router_cache: RouterCache::new(default_router),
            final_router: Mutex::new(None),
            authn_client: Mutex::new(None),
            internal_authenticator: Mutex::new(None),
            healthcheck_registry: OnceLock::new(),
            health_router: OnceLock::new(),
            throttle_key_pruner: Mutex::new(None),
            registered_routes: DashMap::new(),
            registered_handlers: DashMap::new(),
            proxy_registry: Arc::new(toolkit_gateway::ProxyRegistry::new()),
            bound_endpoint: OnceLock::new(),
        }
    }
}

// Built-in health-probe paths, shared by route registration and auth policy so they can't drift.
const HEALTH_DETAIL_PATH: &str = "/health";
const HEALTHZ_PATH: &str = "/healthz";
const READYZ_PATH: &str = "/readyz";

impl ApiGateway {
    /// Nest `router` under `prefix`. Returns `router` unchanged when `prefix` is empty.
    ///
    /// Auth matching is keyed on unprefixed `OperationBuilder` paths, so the caller must
    /// apply `router`'s middleware before this strips/adds the prefix via `nest()`.
    fn apply_prefix(router: Router, prefix: &str) -> Router {
        if prefix.is_empty() {
            router
        } else {
            Router::new().nest(prefix, router)
        }
    }

    /// Standalone health-probe router (`/health`, `/healthz`, `/readyz`): all public, no
    /// gateway middleware, no auth, and NOT part of the `OpenAPI` document. In `separate`/`both`
    /// mode the framework binds this on the separate health listener (`health.bind_addr`) from
    /// [`serve`](Self::serve); it is also exposed for embedders that want to serve it themselves.
    ///
    /// # Errors
    /// Returns an error if [`rest_prepare`](toolkit::contracts::ApiGatewayCapability::rest_prepare)
    /// has not run yet (the healthcheck registry is unset).
    pub fn health_router(&self) -> Result<Router> {
        let hc_registry = self.healthcheck_registry.get().cloned().ok_or_else(|| {
            anyhow::anyhow!(
                "healthcheck_registry not set; call rest_prepare before health_router()"
            )
        })?;
        let config = self.get_cached_config();
        Ok(self.build_health_router(
            hc_registry,
            Duration::from_millis(config.healthcheck_timeout_ms),
        ))
    }

    /// Build (or return the cached) standalone health router. Deps (`hc_registry`,
    /// `healthcheck_timeout`) are fixed after `rest_prepare`, so the first build is
    /// authoritative; later calls reuse the clone.
    fn build_health_router(
        &self,
        hc_registry: Arc<toolkit::RestHealthcheckRegistry>,
        healthcheck_timeout: Duration,
    ) -> Router {
        if let Some(cached) = self.health_router.get() {
            return cached.clone();
        }
        let built = Self::health_routes(hc_registry, healthcheck_timeout);
        // First build wins; a concurrent racer's `set` is a no-op (builds are sequential
        // during lifecycle setup, so this is a belt-and-braces guard).
        drop(self.health_router.set(built.clone()));
        built
    }

    /// Health-probe routes as plain Axum routes (no `OperationBuilder`, so they never enter
    /// the `OpenAPI` registry). All three are public; deps injected via `Extension`. Merged
    /// onto the main router in `main`/`both` mode and used standalone on the separate listener.
    fn health_routes(
        hc_registry: Arc<toolkit::RestHealthcheckRegistry>,
        healthcheck_timeout: Duration,
    ) -> Router {
        Router::new()
            .route(HEALTH_DETAIL_PATH, get(web::health_detail))
            .route(HEALTHZ_PATH, get(|| async { "ok" }))
            .route(READYZ_PATH, get(web::readyz_check))
            .layer(Extension(hc_registry))
            .layer(Extension(web::HealthcheckTimeout(healthcheck_timeout)))
    }

    /// Create a new `ApiGateway` instance with the given configuration
    #[must_use]
    pub fn new(config: ApiGatewayConfig) -> Self {
        let default_router = Router::new();
        Self {
            config: ArcSwap::from_pointee(config),
            openapi_registry: Arc::new(OpenApiRegistryImpl::new()),
            router_cache: RouterCache::new(default_router),
            final_router: Mutex::new(None),
            authn_client: Mutex::new(None),
            internal_authenticator: Mutex::new(None),
            healthcheck_registry: OnceLock::new(),
            health_router: OnceLock::new(),
            throttle_key_pruner: Mutex::new(None),
            registered_routes: DashMap::new(),
            registered_handlers: DashMap::new(),
            proxy_registry: Arc::new(toolkit_gateway::ProxyRegistry::new()),
            bound_endpoint: OnceLock::new(),
        }
    }

    /// Get the current configuration (cheap clone from `ArcSwap`)
    pub fn get_config(&self) -> ApiGatewayConfig {
        (**self.config.load()).clone()
    }

    /// Get cached configuration (lock-free with `ArcSwap`)
    pub fn get_cached_config(&self) -> ApiGatewayConfig {
        (**self.config.load()).clone()
    }

    /// Get the cached router without rebuilding (useful for performance-critical paths)
    pub fn get_cached_router(&self) -> Arc<Router> {
        self.router_cache.load()
    }

    /// Force rebuild and cache of the router.
    ///
    /// # Errors
    /// Returns an error if router building fails.
    pub fn rebuild_and_cache_router(&self) -> Result<()> {
        let new_router = self.build_router()?;
        self.router_cache.store(new_router);
        Ok(())
    }

    /// Build route policy from operation specs.
    fn build_route_policy_from_specs(&self) -> Result<auth::GatewayRoutePolicy> {
        let mut authenticated_routes = std::collections::HashSet::new();
        // Anonymous (no-auth) routes. This is the *auth* axis: routes here skip
        // bearer-token enforcement. It is NOT external visibility (`exposed`);
        // an anonymous route may still be externally exposed or not.
        let mut anonymous_routes = std::collections::HashSet::new();

        anonymous_routes.insert((Method::GET, "/docs".to_owned()));
        anonymous_routes.insert((Method::GET, "/openapi.json".to_owned()));

        // In main/both mode the health probes are merged onto the main router *before* the
        // middleware stack (see `rest_finalize`), so the auth layer resolves them: mark them
        // explicitly anonymous so no bearer token is ever required. Auth matches on the unprefixed
        // path, so these keys stay unprefixed even when `prefix_path` is set.
        let config = self.get_cached_config();
        if matches!(
            config.health.serve,
            HealthServeMode::Main | HealthServeMode::Both
        ) {
            for path in [HEALTHZ_PATH, READYZ_PATH, HEALTH_DETAIL_PATH] {
                anonymous_routes.insert((Method::GET, path.to_owned()));
            }
        }

        for spec in &self.openapi_registry.operation_specs {
            let spec = spec.value();

            let route_key = (spec.method.clone(), spec.path.clone());

            // Auth axis: `authenticated` requires a JWT; `!authenticated` is
            // anonymous (auth-skip). Visibility (`exposed`) is a *separate*
            // axis (gateway registration) and does NOT affect the auth decision.
            // The builder typestate forces an explicit choice, so every spec'd
            // route lands in exactly one set; `require_auth_by_default` remains
            // the fallback for paths with no matching spec.
            if spec.authenticated {
                authenticated_routes.insert(route_key);
            } else {
                anonymous_routes.insert(route_key);
            }
        }

        let requirements_count = authenticated_routes.len();
        let anonymous_routes_count = anonymous_routes.len();

        // When the embedded-edge reverse proxy is enabled, hand the auth policy the
        // shared proxy registry so dynamically-registered proxy routes are enforced
        // per-route (authenticated vs anonymous) instead of falling back to the
        // `require_auth_by_default` global.
        let proxy_registry = config
            .gateway_proxy
            .enabled
            .then(|| Arc::clone(&self.proxy_registry));
        let route_policy = auth::build_route_policy(
            &config,
            authenticated_routes,
            anonymous_routes,
            proxy_registry,
        )?;

        tracing::info!(
            auth_disabled = config.auth_disabled,
            require_auth_by_default = config.require_auth_by_default,
            requirements_count = requirements_count,
            anonymous_routes_count = anonymous_routes_count,
            "Route policy built from operation specs"
        );

        Ok(route_policy)
    }

    fn normalize_prefix_path(raw: &str) -> Result<String> {
        let trimmed = raw.trim();
        // Collapse consecutive slashes then strip trailing slash(es).
        let collapsed: String =
            trimmed
                .chars()
                .fold(String::with_capacity(trimmed.len()), |mut acc, c| {
                    if c == '/' && acc.ends_with('/') {
                        // skip duplicate slash
                    } else {
                        acc.push(c);
                    }
                    acc
                });
        let prefix = collapsed.trim_end_matches('/');
        let result = if prefix.is_empty() {
            String::new()
        } else if prefix.starts_with('/') {
            prefix.to_owned()
        } else {
            format!("/{prefix}")
        };
        // Reject characters that are unsafe in URL paths or HTML attributes.
        if !result
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'/' || b == b'_' || b == b'-' || b == b'.')
        {
            anyhow::bail!(
                "prefix_path contains invalid characters (must match [a-zA-Z0-9/_\\-.]): {raw:?}"
            );
        }

        if result.split('/').any(|seg| seg == "." || seg == "..") {
            anyhow::bail!("prefix_path must not contain '.' or '..' segments: {raw:?}");
        }

        Ok(result)
    }

    /// Apply all middleware layers to a router (request ID, tracing, timeout, body limit, CORS, rate limiting, error mapping, auth)
    pub(crate) fn apply_middleware_stack(
        &self,
        mut router: Router,
        authn_client: Option<Arc<dyn AuthNResolverClient>>,
    ) -> Result<Router> {
        // Build route policy once
        let route_policy = self.build_route_policy_from_specs()?;

        // IMPORTANT: `axum::Router::layer(...)` behaves like Tower layers: the **last** added layer
        // becomes the **outermost** layer and therefore runs **first** on the request path.
        //
        // Desired request execution order (outermost -> innermost):
        // SetRequestId -> PropagateRequestId -> Trace -> push_req_id_to_extensions
        // -> Timeout -> BodyLimit -> CORS -> MIME validation -> PreAuthThrottling -> ErrorMapping
        // -> Auth -> ScopeEnforcement -> PostAuthThrottling -> License -> Router
        //
        // Therefore we must add layers in the reverse order (innermost -> outermost) below.
        // Due future refactoring, this order must be maintained.

        // 14) Propagate MatchedPath to response extensions (route_layer — innermost).
        // This copies MatchedPath from the request (populated by Axum route matching)
        // into the response so outer layer() middleware (metrics) can read it.
        // `route_layer` panics on a routeless router — reachable when no REST provider
        // has registered anything yet.
        if router.has_routes() {
            router = router.route_layer(from_fn(middleware::http_metrics::propagate_matched_path));
        }

        let config = self.get_cached_config();

        // Fail fast on invalid throttling configuration (undefined zone refs, zero limits, etc.).
        config.validate_throttling()?;

        // Collect specs once; used by MIME validation + throttling maps.
        let specs: Vec<_> = self
            .openapi_registry
            .operation_specs
            .iter()
            .map(|e| e.value().clone())
            .collect();

        // 12) License validation
        let license_map = middleware::license_validation::LicenseRequirementMap::from_specs(&specs);

        router = router.layer(from_fn(
            move |req: axum::extract::Request, next: axum::middleware::Next| {
                let map = license_map.clone();
                middleware::license_validation::license_validation_middleware(map, req, next)
            },
        ));

        // Build both throttling partitions together so a zone referenced from
        // both pre-auth and post-auth operations shares a single limiter/gate
        // instance. The pre-auth map is layered later (step 9b).
        let (throttling_map, throttling_map_noauth, throttle_key_pruner) =
            middleware::throttling::build_maps(&specs, &config)?;
        // Hand the keyed-store pruner to `serve`, which owns the lifecycle
        // CancellationToken needed to bound the background prune task.
        *self.throttle_key_pruner.lock() = Some(throttle_key_pruner);

        // 11b) Post-auth throttling (identity-keyed zones). Added before the auth
        // layer so it runs AFTER auth and can read SecurityContext for identity keys.
        router = router.layer(from_fn(
            move |req: axum::extract::Request, next: axum::middleware::Next| {
                let map = throttling_map.clone();
                middleware::throttling::throttling_middleware(map, req, next)
            },
        ));

        // 11) Route Policy Enforcement (runs after auth, checks token_scopes against route requirements)
        if config.route_policies.enabled {
            // Reject invalid combination: route_policies requires authentication to work
            if config.auth_disabled {
                return Err(anyhow::anyhow!(
                    "Invalid configuration: route_policies.enabled=true requires authentication. \
                     Set auth_disabled=false or disable route_policies."
                ));
            }

            let scope_rules = middleware::scope_enforcement::ScopeEnforcementRules::from_config(
                &config.route_policies,
            )?;
            let scope_state =
                middleware::scope_enforcement::ScopeEnforcementState { rules: scope_rules };
            router = router.layer(from_fn_with_state(
                scope_state,
                middleware::scope_enforcement::scope_enforcement_middleware,
            ));
        }

        // 10) Auth
        if config.auth_disabled {
            // Build security contexts for compatibility during migration
            let default_security_context = SecurityContext::builder()
                .subject_id(DEFAULT_SUBJECT_ID)
                .subject_tenant_id(DEFAULT_TENANT_ID)
                .build()?;

            tracing::warn!(
                "API Gateway auth is DISABLED: all requests will run with default tenant SecurityContext. \
                 This mode bypasses authentication and is intended ONLY for single-user on-premises deployments without an IdP. \
                 Permission checks and secure ORM still apply. DO NOT use this mode in multi-tenant or production environments."
            );
            router = router.layer(from_fn(
                move |mut req: axum::extract::Request, next: axum::middleware::Next| {
                    let sec_context = default_security_context.clone();
                    async move {
                        req.extensions_mut().insert(sec_context);
                        next.run(req).await
                    }
                },
            ));
        } else if let Some(client) = authn_client {
            let auth_state = auth::AuthState {
                authn_client: client,
                route_policy,
            };
            router = router.layer(from_fn_with_state(auth_state, auth::authn_middleware));
        } else {
            return Err(anyhow::anyhow!(
                "auth is enabled but no AuthN Resolver client is available; \
                 ensure `authn_resolver` gear is loaded or set `auth_disabled: true`"
            ));
        }

        if let Some(internal) = self.internal_authenticator.lock().clone() {
            router = router.layer(from_fn_with_state(
                Arc::new(internal),
                internal_auth_middleware::<DynInternalAuthenticator>,
            ));
        }

        // 11) Error mapping (outer to auth so it can translate auth/handler errors)
        router = router.layer(from_fn(toolkit::api::error_layer::error_mapping_middleware));

        // 9b) Pre-auth throttling (IP-keyed zones). Added after the auth layer so
        // it runs BEFORE auth; identity keying is not available here. The map was
        // built together with the post-auth map (above) to share zone runtimes.
        router = router.layer(from_fn(
            move |req: axum::extract::Request, next: axum::middleware::Next| {
                let map = throttling_map_noauth.clone();
                middleware::throttling::throttling_no_auth_middleware(map, req, next)
            },
        ));

        // 9) MIME type validation
        let mime_map = middleware::mime_validation::build_mime_validation_map(&specs);
        router = router.layer(from_fn(
            move |req: axum::extract::Request, next: axum::middleware::Next| {
                let map = mime_map.clone();
                middleware::mime_validation::mime_validation_middleware(map, req, next)
            },
        ));

        // 8) CORS (must be outer to auth/limits so OPTIONS preflight short-circuits)
        if config.cors_enabled {
            router = router.layer(crate::cors::build_cors_layer(&config));
        }

        // 7) Body limit
        router = router.layer(RequestBodyLimitLayer::new(config.defaults.body_limit_bytes));
        router = router.layer(DefaultBodyLimit::max(config.defaults.body_limit_bytes));

        // 6) Timeout — emits canonical `deadline_exceeded` Problem with
        //    `application/problem+json` body when the inner service exceeds
        //    the deadline. Layer position is unchanged (between BodyLimit
        //    and CatchPanic).
        router = router.layer(
            ServiceBuilder::new()
                .layer(HandleErrorLayer::new(timeout_to_canonical))
                .timeout(Duration::from_secs(30)),
        );

        // 5) CatchPanic (converts panics to 500 before metrics sees them)
        router = router.layer(CatchPanicLayer::new());

        // 4.5) Canonical error middleware — fills trace_id / instance on
        // application/problem+json bodies and logs WARN/ERROR. Sits inside
        // http_metrics so metrics observe the canonical-final body, and
        // outside CatchPanicLayer so panics still reach the panic handler
        // before this middleware tries to rewrite them.
        router = router.layer(from_fn(toolkit::api::canonical_error_middleware));

        // 4) HTTP metrics (layer — captures all middleware responses including auth/rate-limit/timeout)
        let http_metrics = Arc::new(middleware::http_metrics::HttpMetrics::new(
            Self::MODULE_NAME,
            &config.metrics.prefix,
        ));
        router = router.layer(from_fn_with_state(
            http_metrics,
            middleware::http_metrics::http_metrics_middleware,
        ));

        // 3.5) Structured access log (runs after push_req_id populates XRequestId extension)
        router = router.layer(from_fn(middleware::access_log::access_log_middleware));

        // 3) Record request_id into span + extensions (requires span to exist first => must be inner to Trace)
        router = router.layer(from_fn(middleware::request_id::push_req_id_to_extensions));

        // 2) Trace (outer to push_req_id_to_extensions)
        router = router.layer({
            use toolkit_http::otel;
            use tower_http::trace::TraceLayer;
            use tracing::field::Empty;

            TraceLayer::new_for_http()
                .make_span_with(move |req: &axum::http::Request<axum::body::Body>| {
                    let hdr = middleware::request_id::header();
                    let rid = req
                        .headers()
                        .get(&hdr)
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("n/a");

                    let span = tracing::info_span!(
                        "http_request",
                        method = %req.method(),
                        uri = %req.uri().path(),
                        version = ?req.version(),
                        gear =  "api_gateway",
                        endpoint = %req.uri().path(),
                        request_id = %rid,
                        status = Empty,
                        latency_ms = Empty,
                        // OpenTelemetry semantic conventions
                        "http.method" = %req.method(),
                        "http.target" = %req.uri().path(),
                        "http.scheme" = req.uri().scheme_str().unwrap_or("http"),
                        "http.host" = req.headers().get("host")
                            .and_then(|h| h.to_str().ok())
                            .unwrap_or("unknown"),
                        "user_agent.original" = req.headers().get("user-agent")
                            .and_then(|h| h.to_str().ok())
                            .unwrap_or("unknown"),
                        // Trace context placeholders (for log correlation)
                        trace_id = Empty,
                        parent.trace_id = Empty
                    );

                    // Set parent OTel trace context (W3C traceparent), if any
                    // This also populates trace_id and parent.trace_id from headers
                    otel::set_parent_from_headers(&span, req.headers());

                    span
                })
                .on_response(
                    |res: &axum::http::Response<axum::body::Body>,
                     latency: std::time::Duration,
                     span: &tracing::Span| {
                        let ms = latency.as_millis();
                        span.record("status", res.status().as_u16());
                        span.record("latency_ms", ms);
                    },
                )
        });

        // 1) Request ID handling (outermost)
        let x_request_id = crate::middleware::request_id::header();
        // If missing, generate x-request-id first; then propagate it to the response.
        router = router.layer(PropagateRequestIdLayer::new(x_request_id.clone()));
        router = router.layer(SetRequestIdLayer::new(
            x_request_id,
            crate::middleware::request_id::MakeReqId,
        ));

        Ok(router)
    }

    /// Build the HTTP router from registered routes and operations.
    ///
    /// # Errors
    /// Returns an error if router building or middleware setup fails.
    pub fn build_router(&self) -> Result<Router> {
        // If the cached router is currently held elsewhere (e.g., by the running server),
        // return it without rebuilding to avoid unnecessary allocations.
        let cached_router = self.router_cache.load();
        if Arc::strong_count(&cached_router) > 1 {
            tracing::debug!("Using cached router");
            return Ok((*cached_router).clone());
        }

        tracing::debug!("Building new router (standalone/fallback mode)");
        // No "main" routes here — the empty router is tolerated (`has_routes` guard).
        // Health probes are not part of this router; see `health_router`.
        let config = self.get_cached_config();
        let authn_client = self.authn_client.lock().clone();
        let mut router = Router::new();

        // Embedded-edge reverse proxy: mount the Forwarder as the fallback BEFORE the
        // middleware stack (mirrors `rest_finalize`) so unmatched external requests are
        // proxied to the owning OoP gear pod instead of returning 404. Required on this
        // default/fallback path too, for when `rest_finalize` produced no stored router.
        if config.gateway_proxy.enabled {
            router = self.mount_proxy_fallback(router)?;
        }

        let router = self.apply_middleware_stack(router, authn_client)?;

        let prefix = Self::normalize_prefix_path(&config.prefix_path)?;
        let router = Self::apply_prefix(router, &prefix);

        // Cache the built router for future use
        self.router_cache.store(router.clone());

        Ok(router)
    }

    /// Build `OpenAPI` specification from registered routes and components.
    ///
    /// # Errors
    /// Returns an error if `OpenAPI` specification building fails.
    pub fn build_openapi(&self) -> Result<utoipa::openapi::OpenApi> {
        let config = self.get_cached_config();
        let prefix = Self::normalize_prefix_path(&config.prefix_path)?;
        let info = toolkit::api::OpenApiInfo {
            title: config.openapi.title.clone(),
            version: config.openapi.version.clone(),
            description: config.openapi.description.clone(),
            servers: (!prefix.is_empty()).then_some(prefix).into_iter().collect(),
        };
        let mut openapi = self.openapi_registry.build_openapi(&info)?;
        enrich_openapi_with_zone_limits(&mut openapi, &config);
        Ok(openapi)
    }

    /// Parse bind address from configuration string.
    fn parse_bind_address(bind_addr: &str) -> anyhow::Result<SocketAddr> {
        bind_addr
            .parse()
            .map_err(|e| anyhow::anyhow!("Invalid bind address '{bind_addr}': {e}"))
    }

    /// Resolve the separate health listener address, if the config demands one.
    ///
    /// `Ok(None)` for `serve = main`. For `separate`/`both`, `health.bind_addr` must be present
    /// and parseable — used both to fail fast at `init` and to bind at `serve`.
    ///
    /// # Errors
    /// Returns an error if `serve` needs a separate listener but `health.bind_addr` is missing
    /// or not a valid socket address.
    fn health_bind_addr(cfg: &ApiGatewayConfig) -> anyhow::Result<Option<SocketAddr>> {
        match cfg.health.serve {
            HealthServeMode::Main => Ok(None),
            HealthServeMode::Separate | HealthServeMode::Both => {
                let raw = cfg.health.bind_addr.as_deref().ok_or_else(|| {
                    anyhow::anyhow!(
                        "health.serve = {:?} requires health.bind_addr to be set",
                        cfg.health.serve
                    )
                })?;
                Ok(Some(Self::parse_bind_address(raw)?))
            }
        }
    }

    /// Get the finalized router or build a default one.
    fn get_or_build_router(self: &Arc<Self>) -> anyhow::Result<Router> {
        let stored = { self.final_router.lock().take() };

        if let Some(router) = stored {
            tracing::debug!("Using router from REST phase");
            Ok(router)
        } else {
            tracing::debug!("No router from REST phase, building default router");
            self.build_router()
        }
    }

    /// Background HTTP server: bind, notify ready, serve until cancelled.
    ///
    /// This method is the lifecycle entry-point generated by the macro
    /// (`#[toolkit::gear(..., lifecycle(...))]`).
    pub(crate) async fn serve(
        self: Arc<Self>,
        cancel: CancellationToken,
        ready: ReadySignal,
    ) -> anyhow::Result<()> {
        let cfg = self.get_cached_config();
        let addr = Self::parse_bind_address(&cfg.bind_addr)?;
        let router = self.get_or_build_router()?;

        // Bound the throttling keyed stores: spawn the periodic pruner produced
        // during middleware build, tied to the lifecycle token so it stops on
        // shutdown. Without this the keyed stores grow one entry per distinct
        // (attacker-influenced) key with no eviction, and pruning off the hot
        // path avoids per-request all-shard write-locking scans under a flood.
        if let Some(pruner) = self.throttle_key_pruner.lock().take() {
            // The task self-terminates when `cancel` fires, so the handle is
            // intentionally discarded rather than joined.
            drop(pruner.spawn(cancel.clone()));
        }

        // Bind the main socket.
        let listener = tokio::net::TcpListener::bind(addr).await?;
        let bound_addr = listener.local_addr().unwrap_or(addr);
        tracing::info!("HTTP server bound on {}", bound_addr);

        self.publish_bound_endpoint(&cfg, bound_addr);

        // Bind the separate health listener (if `serve` = separate|both) BEFORE signalling
        // ready, so readiness reflects every listener the pod must accept traffic on.
        let health_bound = self.bind_health_listener(&cfg).await?;

        // Embedded-edge reverse proxy: start the directory-sync task that keeps the
        // proxy route table current. Non-fatal — the gateway still serves native routes
        // if the directory is unavailable, and startup does not block on the first
        // successful directory connection.
        if cfg.gateway_proxy.enabled {
            self.start_proxy_sync(&cfg, cancel.clone());
        }

        ready.notify(); // Starting -> Running

        let main_server = axum::serve(
            listener,
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(Self::shutdown_signal(cancel.clone(), "HTTP server"));

        // Both listeners share the runtime cancellation token, so shutdown fans out to both.
        if let Some((health_listener, health_router)) = health_bound {
            let health_server = axum::serve(health_listener, health_router.into_make_service())
                .with_graceful_shutdown(Self::shutdown_signal(cancel, "health server"));
            tokio::try_join!(
                async { main_server.await.map_err(|e| anyhow::anyhow!(e)) },
                async { health_server.await.map_err(|e| anyhow::anyhow!(e)) },
            )?;
            Ok(())
        } else {
            main_server.await.map_err(|e| anyhow::anyhow!(e))
        }
    }

    /// Publish the endpoint other pods use to reach this gateway, so the
    /// runtime's directory-register phase can advertise in-process REST
    /// providers (via `#[toolkit::provides]`). Called from `serve` once the main
    /// listener binds.
    ///
    /// The URL is chosen by
    /// [`resolve_advertised_endpoint`](Self::resolve_advertised_endpoint);
    /// nothing is published when it returns `None`. Write-once.
    fn publish_bound_endpoint(&self, cfg: &ApiGatewayConfig, bound_addr: SocketAddr) {
        let Some(advertised) = Self::resolve_advertised_endpoint(cfg, bound_addr) else {
            return;
        };
        // Write-once: a second publish means `serve` ran twice, a lifecycle bug.
        if self.bound_endpoint.set(advertised.clone()).is_err() {
            tracing::warn!(
                endpoint = %advertised,
                "bound_endpoint already published; ignoring duplicate publish"
            );
            return;
        }
        tracing::info!(
            endpoint = %advertised,
            "REST host endpoint published for directory registration"
        );
    }

    /// Resolve the base URL to advertise for directory registration.
    ///
    /// Prefers the explicitly configured `advertise_uri` (required in
    /// Kubernetes, where the pod binds `0.0.0.0`), used verbatim; otherwise
    /// falls back to the bound address plus the normalized `prefix_path`, so the
    /// base URL matches the path under which `rest_finalize` serves the routes.
    ///
    /// Returns `None` (skipping publication) when `prefix_path` is invalid, or
    /// when the bind address is a wildcard (`0.0.0.0`/`::`) with no
    /// `advertise_uri` — an unspecified address is unreachable by other pods.
    fn resolve_advertised_endpoint(
        cfg: &ApiGatewayConfig,
        bound_addr: SocketAddr,
    ) -> Option<String> {
        // Normalize the prefix once (see `normalized_prefix_or_skip`).
        let prefix = Self::normalized_prefix_or_skip(&cfg.prefix_path)?;

        match cfg.advertise_uri.as_deref() {
            Some(uri) => {
                // `advertise_uri` is used verbatim (the prefix is NOT appended).
                Self::warn_if_advertise_uri_missing_prefix(uri, &prefix);
                Some(uri.to_owned())
            }
            None if bound_addr.ip().is_unspecified() => {
                tracing::warn!(
                    bound_addr = %bound_addr,
                    "REST host bound on a wildcard address with no `advertise_uri`; \
                     skipping directory registration endpoint (set `advertise_uri`)"
                );
                None
            }
            // The router is served under `prefix_path` (see `rest_finalize`), so
            // the fallback base URL must include it.
            None => Some(format!("http://{bound_addr}{prefix}")),
        }
    }

    /// Normalize `prefix_path`, or `None` (logged) when it is invalid.
    /// `get_or_build_router` already validated it, but treat any error as a real
    /// misconfiguration and skip publishing rather than silently advertising a
    /// prefix-less — and therefore wrong — endpoint.
    fn normalized_prefix_or_skip(raw: &str) -> Option<String> {
        match Self::normalize_prefix_path(raw) {
            Ok(prefix) => Some(prefix),
            Err(e) => {
                tracing::error!(
                    error = %e,
                    "invalid `prefix_path`; skipping directory registration endpoint"
                );
                None
            }
        }
    }

    /// Warn when a verbatim `advertise_uri` omits the `prefix_path` the routes
    /// are actually served under, which would silently break out-of-process
    /// requests.
    fn warn_if_advertise_uri_missing_prefix(uri: &str, prefix: &str) {
        if !prefix.is_empty() && !uri.trim_end_matches('/').ends_with(prefix) {
            tracing::warn!(
                advertise_uri = %uri,
                prefix_path = %prefix,
                "`advertise_uri` does not include the gateway's `prefix_path`; \
                 out-of-process requests may not reach the prefixed routes"
            );
        }
    }

    /// Start the embedded-edge reverse-proxy directory-sync task in the
    /// background. Returns immediately; connecting to the `DirectoryService` is
    /// retried with backoff until it succeeds or the runtime shuts down, so a
    /// transient directory outage at startup no longer permanently disables the
    /// reverse proxy. Non-fatal: the gateway keeps serving its native routes
    /// throughout, and startup does not block on the first connection.
    fn start_proxy_sync(&self, cfg: &ApiGatewayConfig, cancel: CancellationToken) {
        let proxy_cfg = cfg.gateway_proxy.clone();
        // The platform-plane credential is the single top-level `internal_auth`
        // (outbound direction): the same block that gates inbound validation
        // supplies the credential attached to directory polls.
        let internal_auth = cfg.internal_auth.clone();
        let registry = Arc::clone(&self.proxy_registry);
        tokio::spawn(Self::proxy_sync_supervisor(
            proxy_cfg,
            internal_auth,
            registry,
            cancel,
        ));
    }

    /// Retry the `DirectoryService` connection with exponential backoff until it
    /// succeeds — then hand off to the directory-sync loop — or `cancel` fires.
    /// A missing `directory_endpoint` is a permanent misconfiguration, not a
    /// transient failure, so it is reported once and the task exits without
    /// spinning.
    async fn proxy_sync_supervisor(
        cfg: GatewayProxyConfig,
        internal_auth: Option<InternalAuthConfig>,
        registry: Arc<toolkit_gateway::ProxyRegistry>,
        cancel: CancellationToken,
    ) {
        if cfg.directory_endpoint.is_none() {
            tracing::warn!(
                "reverse proxy inactive: gateway_proxy.enabled but directory_endpoint is unset"
            );
            return;
        }

        let interval = Duration::from_secs(cfg.sync_interval_secs);
        if let Some(directory) =
            Self::connect_with_backoff(&cfg, internal_auth.as_ref(), &cancel).await
        {
            // Drive the sync loop through the `GatewayProvider` trait so the
            // edge is pluggable (built-in reverse proxy here; a Kong/Tyk adapter
            // for Mode B). The built-in provider writes into the shared registry
            // the `Forwarder` reads.
            let provider: Arc<dyn toolkit_gateway::GatewayProvider> =
                Arc::new(toolkit_gateway::ToolKitGatewayProvider::new(registry));
            crate::proxy::spawn_directory_sync(provider, directory, interval, cancel);
            tracing::info!("reverse-proxy directory-sync started");
        }
    }

    /// Retry [`connect_directory`](Self::connect_directory) with exponential
    /// backoff until it succeeds (returning the client) or `cancel` fires
    /// (returning `None`).
    async fn connect_with_backoff(
        cfg: &GatewayProxyConfig,
        internal_auth: Option<&InternalAuthConfig>,
        cancel: &CancellationToken,
    ) -> Option<Arc<dyn DirectoryClient>> {
        const INITIAL_BACKOFF: Duration = Duration::from_millis(100);
        const MAX_BACKOFF: Duration = Duration::from_secs(30);

        let mut backoff = INITIAL_BACKOFF;
        loop {
            match Self::connect_directory(cfg, internal_auth).await {
                Ok(directory) => return Some(directory),
                Err(err) => tracing::warn!(
                    error = %err,
                    retry_in = ?backoff,
                    "reverse proxy: directory connect failed, retrying",
                ),
            }

            if Self::backoff_or_cancel(backoff, cancel).await {
                tracing::info!("reverse proxy: shutdown before directory connect succeeded");
                return None;
            }
            backoff = (backoff * 2).min(MAX_BACKOFF);
        }
    }

    /// Sleep for `backoff`, returning `true` early if `cancel` fires first.
    async fn backoff_or_cancel(backoff: Duration, cancel: &CancellationToken) -> bool {
        tokio::select! {
            () = cancel.cancelled() => true,
            () = tokio::time::sleep(backoff) => false,
        }
    }

    /// Connect to the `DirectoryService`, attaching the platform-plane
    /// credential when configured.
    ///
    /// # Errors
    /// Returns an error if `directory_endpoint` is unset or the connection fails.
    async fn connect_directory(
        cfg: &GatewayProxyConfig,
        internal_auth: Option<&InternalAuthConfig>,
    ) -> anyhow::Result<Arc<dyn DirectoryClient>> {
        let endpoint = cfg.directory_endpoint.clone().ok_or_else(|| {
            anyhow::anyhow!("gateway_proxy.enabled but directory_endpoint is unset")
        })?;

        let client = if let Some(internal_auth) = internal_auth {
            let interceptor =
                toolkit_transport_grpc::build_internal_auth_interceptor(internal_auth).await?;
            tracing::info!("attaching platform-plane credential to edge DirectoryService polls");
            DirectoryGrpcClient::connect_with_interceptor(endpoint.clone(), interceptor).await?
        } else {
            DirectoryGrpcClient::connect(endpoint.clone()).await?
        };
        Ok(Arc::new(client))
    }

    /// Mount the reverse-proxy [`Forwarder`](toolkit_gateway::Forwarder) as the
    /// router fallback, so any request not matched by a gateway-owned route is
    /// proxied to the owning out-of-process gear pod.
    ///
    /// # Errors
    /// Returns an error if the outbound HTTP client cannot be constructed.
    fn mount_proxy_fallback(&self, router: Router) -> Result<Router> {
        // Use the dedicated reverse-proxy client profile rather than the
        // general-purpose defaults: no retries (the edge must not re-send or
        // amplify load), no client-side concurrency limit (no gateway-wide 503
        // Overloaded), no response body cap (large downloads stream through),
        // and no 30s request timeout (SSE / streaming upstreams stay open).
        let client =
            toolkit_http::HttpClientBuilder::with_config(toolkit_http::HttpClientConfig::proxy())
                .build()?;
        let forwarder = toolkit_gateway::Forwarder::new(Arc::clone(&self.proxy_registry), client);
        let router = router.fallback(move |req: axum::extract::Request| {
            let forwarder = forwarder.clone();
            async move { forwarder.forward(req).await }
        });
        tracing::info!("reverse-proxy fallback mounted (gateway_proxy enabled)");
        Ok(router)
    }

    /// Bind the separate health listener when `serve` = separate|both, else `None`.
    async fn bind_health_listener(
        &self,
        cfg: &ApiGatewayConfig,
    ) -> anyhow::Result<Option<(tokio::net::TcpListener, Router)>> {
        let Some(health_addr) = Self::health_bind_addr(cfg)? else {
            return Ok(None);
        };
        let health_listener = tokio::net::TcpListener::bind(health_addr).await?;
        tracing::info!("health server bound on {}", health_addr);
        Ok(Some((health_listener, self.health_router()?)))
    }

    /// Future that resolves when `cancel` fires, logging the graceful-shutdown of `name`.
    async fn shutdown_signal(cancel: CancellationToken, name: &'static str) {
        cancel.cancelled().await;
        tracing::info!("{name} shutting down gracefully (cancellation)");
    }

    /// Check if `handler_id` is already registered (returns true if duplicate)
    fn check_duplicate_handler(&self, spec: &toolkit::api::OperationSpec) -> bool {
        if self
            .registered_handlers
            .insert(spec.handler_id.clone(), ())
            .is_some()
        {
            tracing::error!(
                handler_id = %spec.handler_id,
                method = %spec.method.as_str(),
                path = %spec.path,
                "Duplicate handler_id detected; ignoring subsequent registration"
            );
            return true;
        }
        false
    }

    /// Check if route (method, path) is already registered (returns true if duplicate)
    fn check_duplicate_route(&self, spec: &toolkit::api::OperationSpec) -> bool {
        let route_key = (spec.method.clone(), spec.path.clone());
        if self.registered_routes.insert(route_key, ()).is_some() {
            tracing::error!(
                method = %spec.method.as_str(),
                path = %spec.path,
                "Duplicate (method, path) detected; ignoring subsequent registration"
            );
            return true;
        }
        false
    }

    /// Log successful operation registration
    fn log_operation_registration(&self, spec: &toolkit::api::OperationSpec) {
        let current_count = self.openapi_registry.operation_specs.len();
        tracing::debug!(
            handler_id = %spec.handler_id,
            method = %spec.method.as_str(),
            path = %spec.path,
            summary = %spec.summary.as_deref().unwrap_or("No summary"),
            total_operations = current_count,
            "Registered API operation"
        );
    }

    /// Add `OpenAPI` documentation routes to the router
    fn add_openapi_routes(&self, mut router: axum::Router) -> anyhow::Result<axum::Router> {
        // Build once, serve as static JSON (no per-request parsing)
        let op_count = self.openapi_registry.operation_specs.len();
        tracing::info!(
            "rest_finalize: emitting OpenAPI with {} operations",
            op_count
        );

        let openapi_doc = Arc::new(self.build_openapi()?);
        let config = self.get_cached_config();
        let prefix = Self::normalize_prefix_path(&config.prefix_path)?;
        let html_doc = web::serve_docs(&prefix);

        router = router
            .route(
                "/openapi.json",
                get({
                    use axum::{http::header, response::IntoResponse};
                    let doc = openapi_doc;
                    move || async move {
                        let json_string = match serde_json::to_string_pretty(doc.as_ref()) {
                            Ok(json) => json,
                            Err(e) => {
                                tracing::error!("Failed to serialize OpenAPI doc: {}", e);
                                return (http::StatusCode::INTERNAL_SERVER_ERROR).into_response();
                            }
                        };
                        (
                            [
                                (header::CONTENT_TYPE, "application/json"),
                                (header::CACHE_CONTROL, "no-store"),
                            ],
                            json_string,
                        )
                            .into_response()
                    }
                }),
            )
            .route("/docs", get(move || async move { html_doc }));

        #[cfg(feature = "embed_elements")]
        {
            router = router.route(
                "/docs/assets/{*file}",
                get(crate::assets::serve_elements_asset),
            );
        }

        Ok(router)
    }
}

// Manual implementation of Gear trait with config loading
#[async_trait]
impl toolkit::Gear for ApiGateway {
    async fn init(&self, ctx: &toolkit::context::GearCtx) -> anyhow::Result<()> {
        let cfg = ctx.config_or_default::<crate::config::ApiGatewayConfig>()?;
        // Fail init on invalid CORS combinations (wildcard+credentials):
        // tower-http would otherwise assert-panic during eager router
        // layering — a startup crash-loop with no pointer at the config.
        crate::cors::validate_cors_config(&cfg).map_err(|e| anyhow::anyhow!(e))?;
        // Fail fast when health.serve needs a separate listener but health.bind_addr is
        // missing or unparseable, rather than crashing later in serve() after other gears
        // have started.
        Self::health_bind_addr(&cfg)?;
        // Fail fast on an enabled reverse proxy with no directory_endpoint, rather than
        // starting a proxy that silently serves 404 for every out-of-process route.
        cfg.gateway_proxy
            .validate()
            .map_err(|e| anyhow::anyhow!(e))?;
        self.config.store(Arc::new(cfg.clone()));

        debug!(
            "Effective api_gateway configuration:\n{:#?}",
            self.config.load()
        );

        if cfg.auth_disabled {
            tracing::info!(
                tenant_id = %DEFAULT_TENANT_ID,
                "Auth-disabled mode enabled with default tenant"
            );
        } else {
            // Resolve AuthN Resolver client from ClientHub
            let authn_client = ctx.client_hub().get::<dyn AuthNResolverClient>()?;
            *self.authn_client.lock() = Some(authn_client);
            tracing::info!("AuthN Resolver client resolved from ClientHub");
        }

        // Build the inbound platform-plane authenticator (None in Profile 1).
        // Done here so a misconfigured `internal_auth` fails init rather than
        // surfacing later as a per-request 500 on co-hosted internal routes.
        *self.internal_authenticator.lock() =
            build_internal_authenticator(cfg.internal_auth.as_ref()).await?;

        Ok(())
    }
}

// REST host role: prepare/finalize the router, but do not start the server here.
impl toolkit::contracts::ApiGatewayCapability for ApiGateway {
    fn bound_endpoint(&self) -> Option<String> {
        self.bound_endpoint.get().cloned()
    }

    fn rest_prepare(
        &self,
        _ctx: &toolkit::context::GearCtx,
        router: axum::Router,
        hc_registry: Arc<toolkit::RestHealthcheckRegistry>,
    ) -> anyhow::Result<axum::Router> {
        // Store for use when health routes are added in rest_finalize. A second set
        // means rest_prepare ran twice — a lifecycle bug; fail fast rather than mask it.
        if self.healthcheck_registry.set(hc_registry).is_err() {
            anyhow::bail!("healthcheck_registry already set; rest_prepare called more than once");
        }

        tracing::debug!("REST host prepared base router");
        Ok(router)
    }

    fn rest_finalize(
        &self,
        _ctx: &toolkit::context::GearCtx,
        mut router: axum::Router,
        hc_registry: Arc<toolkit::RestHealthcheckRegistry>,
    ) -> anyhow::Result<axum::Router> {
        let config = self.get_cached_config();

        if config.enable_docs {
            router = self.add_openapi_routes(router)?;
        }

        // Health probes (`main`/`both` mode): merge them onto the main router BEFORE the
        // middleware stack, so they share the gateway's unified surface (request id, tracing,
        // metrics, error mapping, ...). They are marked public in the route policy (see
        // `build_route_policy_from_specs`), so the auth layer lets them through without a bearer
        // token. Like every other route they inherit `prefix_path` via the nesting below.
        // `separate` mode omits them here; `serve()` binds them on the dedicated health listener.
        if matches!(
            config.health.serve,
            HealthServeMode::Main | HealthServeMode::Both
        ) {
            let health = Self::health_routes(
                hc_registry,
                Duration::from_millis(config.healthcheck_timeout_ms),
            );
            router = router.merge(health);
        }

        // Embedded-edge reverse proxy: mount the Forwarder as the router fallback so any
        // request that doesn't match a gateway-owned route is proxied to the owning OoP
        // gear pod. The route table is kept current by the directory-sync task started in
        // `serve()`. Mounted BEFORE the middleware stack so proxied requests traverse the
        // same auth / tracing / error-mapping layers as native routes.
        if config.gateway_proxy.enabled {
            router = self.mount_proxy_fallback(router)?;
        }

        // Middleware on the main router before nesting (auth matching keyed on
        // unprefixed OperationBuilder paths; layers run before nest() strips the prefix).
        tracing::debug!("Applying middleware stack to finalized router");
        let authn_client = self.authn_client.lock().clone();
        router = self.apply_middleware_stack(router, authn_client)?;

        let prefix = Self::normalize_prefix_path(&config.prefix_path)?;
        router = Self::apply_prefix(router, &prefix);

        // Keep the finalized router to be used by `serve()`.
        *self.final_router.lock() = Some(router.clone());

        tracing::info!("REST host finalized router with OpenAPI endpoints and auth middleware");
        Ok(router)
    }

    fn as_registry(&self) -> &dyn toolkit::contracts::OpenApiRegistry {
        self
    }
}

impl toolkit::contracts::RestApiCapability for ApiGateway {
    fn register_rest(
        &self,
        _ctx: &toolkit::context::GearCtx,
        router: axum::Router,
        _openapi: &dyn toolkit::contracts::OpenApiRegistry,
    ) -> anyhow::Result<axum::Router> {
        // This gear acts as both rest_host and rest, but actual REST endpoints
        // are handled in the host methods above.
        Ok(router)
    }
}

impl OpenApiRegistry for ApiGateway {
    fn register_operation(&self, spec: &toolkit::api::OperationSpec) {
        // Reject duplicates with "first wins" policy (second registration = programmer error).
        if self.check_duplicate_handler(spec) {
            return;
        }

        if self.check_duplicate_route(spec) {
            return;
        }

        // Delegate to the internal registry
        self.openapi_registry.register_operation(spec);
        self.log_operation_registration(spec);
    }

    fn ensure_schema_raw(
        &self,
        root_name: &str,
        schemas: Vec<(
            String,
            utoipa::openapi::RefOr<utoipa::openapi::schema::Schema>,
        )>,
    ) -> String {
        // Delegate to the internal registry
        self.openapi_registry.ensure_schema_raw(root_name, schemas)
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// Build the inbound platform-plane validator from configuration.
///
/// Shared-secret is built directly; kube (`TokenReview`) requires the
/// `k8s-auth` feature and uses the shared
/// `toolkit_k8s_auth::build_cached_k8s_authenticator` (also used by `grpc-hub`
/// and the `OoP` bootstrap) so all three agree on what `provider: kube` means
/// and how it is cached. Without the feature, `provider: kube` is a hard error
/// rather than a silent enforcement downgrade. `Ok(None)` only when no
/// `internal_auth` is configured (Profile 1).
#[cfg_attr(not(feature = "k8s-auth"), allow(clippy::unused_async))]
async fn build_internal_authenticator(
    cfg: Option<&InternalAuthConfig>,
) -> Result<Option<DynInternalAuthenticator>> {
    let Some(cfg) = cfg else {
        return Ok(None);
    };

    // Shared-secret (and any future dependency-light provider) builds here.
    if let Some(auth) = cfg.build_authenticator() {
        tracing::info!("Initializing shared-secret platform-plane authenticator");
        return Ok(Some(auth));
    }

    #[cfg(feature = "k8s-auth")]
    {
        if cfg.is_kube() {
            tracing::info!("Initializing Kubernetes TokenReview platform-plane authenticator");
            let audiences = cfg.kube_audiences().unwrap_or_default().to_vec();
            let auth = toolkit_k8s_auth::build_cached_k8s_authenticator(
                audiences,
                Some(toolkit_security::DEFAULT_TOKEN_REVIEW_CACHE_TTL),
            )
            .await
            .map_err(|e| {
                anyhow::anyhow!("failed to init Kubernetes TokenReview authenticator: {e}")
            })?;
            return Ok(Some(auth));
        }
    }
    #[cfg(not(feature = "k8s-auth"))]
    {
        if cfg.is_kube() {
            anyhow::bail!(
                "api-gateway internal_auth provider=kube requires the `k8s-auth` feature"
            );
        }
    }

    anyhow::bail!(
        "internal_auth is configured but no authenticator could be built for the selected provider"
    )
}

/// Enrich throttled operations with their rate-limit zone's numeric limits.
///
/// The toolkit registry emits only zone *names* (`x-throttling-rate-limit-zone`)
/// because the contract layer does not know the gateway configuration. Here,
/// where the zone definitions are in scope, every operation carrying that
/// extension additionally gains `x-rate-limit-rps` / `x-rate-limit-burst`, so
/// spec consumers keep seeing the numeric limits (as they did before the zone
/// model, when the limits lived inline on the operation).
fn enrich_openapi_with_zone_limits(
    openapi: &mut utoipa::openapi::OpenApi,
    config: &ApiGatewayConfig,
) {
    for item in openapi.paths.paths.values_mut() {
        let operations = [
            item.get.as_mut(),
            item.put.as_mut(),
            item.post.as_mut(),
            item.delete.as_mut(),
            item.options.as_mut(),
            item.head.as_mut(),
            item.patch.as_mut(),
            item.trace.as_mut(),
        ];
        for op in operations.into_iter().flatten() {
            let Some(zone_name) = op
                .extensions
                .as_ref()
                .and_then(|ext| ext.get("x-throttling-rate-limit-zone"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
            else {
                continue;
            };
            let Some(zone) = config.rate_limit_zones.get(zone_name.as_str()) else {
                continue;
            };
            let ext = op.extensions.get_or_insert_with(Default::default);
            ext.insert(
                "x-rate-limit-rps".to_owned(),
                serde_json::json!(zone.rate_limit.rps),
            );
            ext.insert(
                "x-rate-limit-burst".to_owned(),
                serde_json::json!(zone.burst_limit),
            );
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn enrich_openapi_adds_zone_limits_by_zone_name() {
        use crate::config::{KeyConfig, KeyType, RateLimitZone, RateSpec, RetryAfter};
        use utoipa::openapi::extensions::Extensions;
        use utoipa::openapi::path::OperationBuilder;
        use utoipa::openapi::{HttpMethod, OpenApiBuilder, PathItem, PathsBuilder};

        let throttled = OperationBuilder::new()
            .extensions(Some(Extensions::from_iter([(
                "x-throttling-rate-limit-zone",
                serde_json::json!("z1"),
            )])))
            .build();
        let unknown_zone = OperationBuilder::new()
            .extensions(Some(Extensions::from_iter([(
                "x-throttling-rate-limit-zone",
                serde_json::json!("missing"),
            )])))
            .build();
        let plain = OperationBuilder::new().build();
        let mut openapi = OpenApiBuilder::new()
            .paths(
                PathsBuilder::new()
                    .path("/throttled", PathItem::new(HttpMethod::Get, throttled))
                    .path("/unknown", PathItem::new(HttpMethod::Get, unknown_zone))
                    .path("/plain", PathItem::new(HttpMethod::Get, plain))
                    .build(),
            )
            .build();

        let mut config = ApiGatewayConfig::default();
        config.rate_limit_zones.insert(
            "z1".to_owned(),
            RateLimitZone {
                rate_limit: RateSpec { rps: 50 },
                burst_limit: 100,
                response_status_code: 429,
                response_retry_after: RetryAfter::Auto,
                key: KeyConfig {
                    key_type: KeyType::Ip,
                },
                max_keys: 1000,
            },
        );

        enrich_openapi_with_zone_limits(&mut openapi, &config);
        let json = serde_json::to_value(&openapi).unwrap();

        let throttled = &json["paths"]["/throttled"]["get"];
        assert_eq!(throttled["x-rate-limit-rps"], serde_json::json!(50));
        assert_eq!(throttled["x-rate-limit-burst"], serde_json::json!(100));
        // The zone-name binding emitted by the toolkit stays in place.
        assert_eq!(
            throttled["x-throttling-rate-limit-zone"],
            serde_json::json!("z1")
        );

        // A binding to an unknown zone and an unthrottled operation gain nothing.
        assert!(
            json["paths"]["/unknown"]["get"]
                .get("x-rate-limit-rps")
                .is_none()
        );
        assert!(
            json["paths"]["/plain"]["get"]
                .get("x-rate-limit-rps")
                .is_none()
        );
    }

    #[test]
    fn test_openapi_generation() {
        let mut config = ApiGatewayConfig::default();
        config.openapi.title = "Test API".to_owned();
        config.openapi.version = "1.0.0".to_owned();
        config.openapi.description = Some("Test Description".to_owned());
        let api = ApiGateway::new(config);

        // Test that we can build OpenAPI without any operations
        let doc = api.build_openapi().unwrap();
        let json = serde_json::to_value(&doc).unwrap();

        // Verify it's valid OpenAPI document structure
        assert!(json.get("openapi").is_some());
        assert!(json.get("info").is_some());
        assert!(json.get("paths").is_some());

        // Verify info section
        let info = json.get("info").unwrap();
        assert_eq!(info.get("title").unwrap(), "Test API");
        assert_eq!(info.get("version").unwrap(), "1.0.0");
        assert_eq!(info.get("description").unwrap(), "Test Description");
    }

    #[test]
    fn bound_endpoint_is_none_before_serve() {
        use toolkit::contracts::ApiGatewayCapability;

        let api = ApiGateway::new(ApiGatewayConfig::default());
        assert_eq!(api.bound_endpoint(), None);
    }

    /// No `internal_auth` configured (Profile 1) yields no authenticator, so
    /// the platform-plane middleware is never layered.
    #[tokio::test]
    async fn build_internal_authenticator_is_none_without_config() {
        let auth = build_internal_authenticator(None).await.unwrap();
        assert!(auth.is_none());
    }

    /// The dependency-light shared-secret provider is built in-crate (no feature
    /// required), so a shared-secret config produces an inbound authenticator.
    #[tokio::test]
    async fn build_internal_authenticator_builds_shared_secret() {
        let cfg = InternalAuthConfig::SharedSecret {
            secret: "dev-internal-token".to_owned(),
            peer_name: "toolkit-internal".to_owned(),
        };
        let auth = build_internal_authenticator(Some(&cfg)).await.unwrap();
        assert!(auth.is_some());
    }

    /// Without the `k8s-auth` feature, `provider: kube` is a hard error rather
    /// than a silent enforcement downgrade to an open platform plane.
    #[cfg(not(feature = "k8s-auth"))]
    #[tokio::test]
    async fn build_internal_authenticator_rejects_kube_without_feature() {
        let cfg = InternalAuthConfig::Kube {
            audiences: vec!["toolkit-internal".to_owned()],
            token_path: None,
        };
        let err = build_internal_authenticator(Some(&cfg))
            .await
            .expect_err("kube provider must error without the k8s-auth feature");
        assert!(err.to_string().contains("k8s-auth"));
    }

    /// Profile 1 (no inbound authenticator): the middleware stack still builds
    /// and simply omits the platform-plane layer.
    #[tokio::test]
    async fn apply_middleware_stack_skips_platform_plane_when_absent() {
        let config = ApiGatewayConfig {
            auth_disabled: true,
            ..Default::default()
        };
        let api = ApiGateway::new(config);
        let _router = api
            .apply_middleware_stack(Router::new(), None)
            .expect("stack builds without a platform-plane authenticator");
    }

    /// When an inbound authenticator is installed, the platform-plane layer is
    /// added to the stack (`cpt-cf-adr-two-plane-auth`).
    #[tokio::test]
    async fn apply_middleware_stack_layers_platform_plane_when_present() {
        let config = ApiGatewayConfig {
            auth_disabled: true,
            ..Default::default()
        };
        let api = ApiGateway::new(config);

        let auth = build_internal_authenticator(Some(&InternalAuthConfig::SharedSecret {
            secret: "dev-internal-token".to_owned(),
            peer_name: "toolkit-internal".to_owned(),
        }))
        .await
        .unwrap()
        .expect("shared-secret authenticator");
        *api.internal_authenticator.lock() = Some(auth);

        let _router = api
            .apply_middleware_stack(Router::new(), None)
            .expect("stack builds with the platform-plane layer");
    }

    /// `connect_directory` fails fast (before any network attempt) when the
    /// directory endpoint is unset.
    #[tokio::test]
    async fn connect_directory_errors_without_endpoint() {
        let cfg = GatewayProxyConfig {
            enabled: true,
            directory_endpoint: None,
            ..Default::default()
        };
        let err = ApiGateway::connect_directory(&cfg, None)
            .await
            .err()
            .expect("missing directory_endpoint must error");
        assert!(err.to_string().contains("directory_endpoint"));
    }

    /// `connect_directory` takes the credential-attach branch when an
    /// `internal_auth` config is supplied: the shared-secret interceptor is
    /// built and attached before the connect attempt. The endpoint is
    /// unreachable, so the connect itself fails after exhausting retries — but
    /// the outbound platform-plane path (`cpt-cf-adr-two-plane-auth`) is
    /// exercised.
    #[tokio::test]
    async fn connect_directory_attaches_internal_auth_credential() {
        let cfg = GatewayProxyConfig {
            enabled: true,
            // Loopback port 1 refuses fast — the retry budget is small
            // (3 attempts, sub-second backoff), so this stays quick without a
            // live directory.
            directory_endpoint: Some("http://127.0.0.1:1".to_owned()),
            ..Default::default()
        };
        let internal = InternalAuthConfig::SharedSecret {
            secret: "dev-internal-token".to_owned(),
            peer_name: "toolkit-internal".to_owned(),
        };
        let result = ApiGateway::connect_directory(&cfg, Some(&internal)).await;
        assert!(
            result.is_err(),
            "connect must fail against an unreachable directory endpoint"
        );
    }

    /// The reverse-proxy supervisor returns immediately (no connect attempt)
    /// when `directory_endpoint` is unset, treating it as a permanent
    /// misconfiguration rather than a transient failure.
    #[tokio::test]
    async fn proxy_sync_supervisor_exits_without_endpoint() {
        let cfg = GatewayProxyConfig {
            enabled: true,
            directory_endpoint: None,
            ..Default::default()
        };
        let registry = Arc::new(toolkit_gateway::ProxyRegistry::new());
        ApiGateway::proxy_sync_supervisor(cfg, None, registry, CancellationToken::new()).await;
    }

    /// `start_proxy_sync` spawns the supervisor task; with no endpoint the task
    /// exits promptly. Exercises the top-level credential-threading path.
    #[tokio::test]
    async fn start_proxy_sync_spawns_supervisor() {
        let config = ApiGatewayConfig {
            gateway_proxy: GatewayProxyConfig {
                enabled: true,
                directory_endpoint: None,
                ..Default::default()
            },
            ..Default::default()
        };
        let api = ApiGateway::new(config.clone());
        api.start_proxy_sync(&config, CancellationToken::new());
        tokio::task::yield_now().await;
    }

    /// With an endpoint configured, the supervisor enters the connect path:
    /// `connect_with_backoff` calls `connect_directory` (which fails fast
    /// against an unreachable endpoint) and then honors the already-fired
    /// cancellation, returning `None` so the supervisor exits without starting
    /// the sync loop. Exercises the credential-threading connect/backoff branch
    /// that the endpoint-less test skips over.
    #[tokio::test]
    async fn proxy_sync_supervisor_connect_path_exits_on_cancel() {
        let cfg = GatewayProxyConfig {
            enabled: true,
            // Loopback port 1 refuses immediately; the retry budget is small so
            // the failed connect stays quick without a live directory.
            directory_endpoint: Some("http://127.0.0.1:1".to_owned()),
            sync_interval_secs: 5,
        };
        let registry = Arc::new(toolkit_gateway::ProxyRegistry::new());
        let cancel = CancellationToken::new();
        // Pre-cancel so `backoff_or_cancel` returns immediately after the first
        // failed connect attempt, keeping the test fast and deterministic.
        cancel.cancel();
        ApiGateway::proxy_sync_supervisor(cfg, None, registry, cancel).await;
    }

    #[test]
    fn publish_bound_endpoint_falls_back_to_bound_addr() {
        use toolkit::contracts::ApiGatewayCapability;

        let api = ApiGateway::new(ApiGatewayConfig::default());
        let addr: SocketAddr = "127.0.0.1:8087".parse().unwrap();

        api.publish_bound_endpoint(&ApiGatewayConfig::default(), addr);

        assert_eq!(
            api.bound_endpoint(),
            Some("http://127.0.0.1:8087".to_owned())
        );
    }

    #[test]
    fn publish_bound_endpoint_prefers_advertise_uri() {
        use toolkit::contracts::ApiGatewayCapability;

        let cfg = ApiGatewayConfig {
            advertise_uri: Some("http://platform-host:8087".to_owned()),
            ..Default::default()
        };
        let api = ApiGateway::new(cfg.clone());
        let addr: SocketAddr = "0.0.0.0:8087".parse().unwrap();

        api.publish_bound_endpoint(&cfg, addr);

        assert_eq!(
            api.bound_endpoint(),
            Some("http://platform-host:8087".to_owned())
        );
    }

    #[test]
    fn publish_bound_endpoint_fallback_includes_prefix_path() {
        use toolkit::contracts::ApiGatewayCapability;

        let cfg = ApiGatewayConfig {
            prefix_path: "cf/".to_owned(), // normalizes to "/cf"
            ..Default::default()
        };
        let api = ApiGateway::new(cfg.clone());
        let addr: SocketAddr = "127.0.0.1:8087".parse().unwrap();

        api.publish_bound_endpoint(&cfg, addr);

        assert_eq!(
            api.bound_endpoint(),
            Some("http://127.0.0.1:8087/cf".to_owned())
        );
    }

    #[test]
    fn publish_bound_endpoint_skips_wildcard_without_advertise_uri() {
        use toolkit::contracts::ApiGatewayCapability;

        for wildcard in ["0.0.0.0:8087", "[::]:8087"] {
            let api = ApiGateway::new(ApiGatewayConfig::default());
            let addr: SocketAddr = wildcard.parse().unwrap();

            api.publish_bound_endpoint(&ApiGatewayConfig::default(), addr);

            assert_eq!(
                api.bound_endpoint(),
                None,
                "wildcard {wildcard} must not publish an unusable endpoint"
            );
        }
    }

    #[test]
    fn publish_bound_endpoint_advertise_uri_used_verbatim_with_prefix() {
        use toolkit::contracts::ApiGatewayCapability;

        // With both `advertise_uri` and a non-empty `prefix_path`, the URI is
        // published verbatim: the prefix is NOT appended, so the operator must
        // include it themselves (here it does). The prefix is not doubled.
        let cfg = ApiGatewayConfig {
            advertise_uri: Some("http://platform-host:8087/cf".to_owned()),
            prefix_path: "/cf".to_owned(),
            ..Default::default()
        };
        let api = ApiGateway::new(cfg.clone());
        let addr: SocketAddr = "0.0.0.0:8087".parse().unwrap();

        api.publish_bound_endpoint(&cfg, addr);

        assert_eq!(
            api.bound_endpoint(),
            Some("http://platform-host:8087/cf".to_owned())
        );
    }

    #[test]
    fn publish_bound_endpoint_skips_invalid_prefix_path() {
        use toolkit::contracts::ApiGatewayCapability;

        // A `prefix_path` that fails normalization is a real misconfiguration:
        // publishing is skipped rather than falling back to an empty prefix and
        // advertising a wrong endpoint.
        let cfg = ApiGatewayConfig {
            prefix_path: "/../etc".to_owned(),
            ..Default::default()
        };
        let api = ApiGateway::new(cfg.clone());
        let addr: SocketAddr = "127.0.0.1:8087".parse().unwrap();

        api.publish_bound_endpoint(&cfg, addr);

        assert_eq!(
            api.bound_endpoint(),
            None,
            "an invalid prefix_path must skip publishing, not default to empty"
        );
    }

    #[tokio::test]
    async fn serve_publishes_os_assigned_ephemeral_port() {
        use toolkit::contracts::ApiGatewayCapability;

        // Binding to `:0` lets the OS pick a free port; `serve` must publish the
        // actual bound address (via `listener.local_addr()`), not the configured
        // `:0`.
        let cfg = ApiGatewayConfig {
            bind_addr: "127.0.0.1:0".to_owned(),
            ..Default::default()
        };
        let api = Arc::new(ApiGateway::new(cfg));
        let cancel = CancellationToken::new();
        let (tx, rx) = tokio::sync::oneshot::channel();

        let serve =
            tokio::spawn(Arc::clone(&api).serve(cancel.clone(), ReadySignal::from_sender(tx)));

        // `serve` publishes the endpoint before signalling ready.
        rx.await.expect("serve should signal ready");

        let endpoint = api
            .bound_endpoint()
            .expect("endpoint published once the listener binds");
        let port: u16 = endpoint
            .strip_prefix("http://127.0.0.1:")
            .and_then(|s| s.parse().ok())
            .unwrap_or_else(|| panic!("unexpected endpoint format: {endpoint}"));
        assert_ne!(
            port, 0,
            "must reflect the OS-assigned port, not the configured :0"
        );

        cancel.cancel();
        let outcome = serve.await.expect("serve task should not panic");
        assert!(
            outcome.is_ok(),
            "serve should exit cleanly on cancel: {outcome:?}"
        );
    }

    #[test]
    fn test_openapi_servers_with_prefix() {
        let config = ApiGatewayConfig {
            prefix_path: "/cf".to_owned(),
            ..Default::default()
        };
        let api = ApiGateway::new(config);

        let doc = api.build_openapi().unwrap();
        let json = serde_json::to_value(&doc).unwrap();

        let servers = json
            .get("servers")
            .expect("servers field should be present");
        let arr = servers.as_array().expect("servers should be an array");
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0].get("url").unwrap(), "/cf");
    }

    #[test]
    fn test_openapi_no_servers_without_prefix() {
        let config = ApiGatewayConfig::default(); // prefix_path is ""
        let api = ApiGateway::new(config);

        let doc = api.build_openapi().unwrap();
        let json = serde_json::to_value(&doc).unwrap();

        // When prefix is empty, servers should be absent (None → omitted from JSON)
        assert!(
            json.get("servers").is_none(),
            "servers should be absent when prefix_path is empty"
        );
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod normalize_prefix_path_tests {
    use super::*;

    #[test]
    fn empty_string_returns_empty() {
        assert_eq!(ApiGateway::normalize_prefix_path("").unwrap(), "");
    }

    #[test]
    fn sole_slash_returns_empty() {
        assert_eq!(ApiGateway::normalize_prefix_path("/").unwrap(), "");
    }

    #[test]
    fn multiple_slashes_return_empty() {
        assert_eq!(ApiGateway::normalize_prefix_path("///").unwrap(), "");
    }

    #[test]
    fn whitespace_only_returns_empty() {
        assert_eq!(ApiGateway::normalize_prefix_path("   ").unwrap(), "");
    }

    #[test]
    fn simple_prefix_preserved() {
        assert_eq!(ApiGateway::normalize_prefix_path("/cf").unwrap(), "/cf");
    }

    #[test]
    fn trailing_slash_stripped() {
        assert_eq!(ApiGateway::normalize_prefix_path("/cf/").unwrap(), "/cf");
    }

    #[test]
    fn leading_slash_prepended_when_missing() {
        assert_eq!(ApiGateway::normalize_prefix_path("cf").unwrap(), "/cf");
    }

    #[test]
    fn consecutive_leading_slashes_collapsed() {
        assert_eq!(ApiGateway::normalize_prefix_path("//cf").unwrap(), "/cf");
    }

    #[test]
    fn consecutive_slashes_mid_path_collapsed() {
        assert_eq!(
            ApiGateway::normalize_prefix_path("/api//v1").unwrap(),
            "/api/v1"
        );
    }

    #[test]
    fn many_consecutive_slashes_collapsed() {
        assert_eq!(
            ApiGateway::normalize_prefix_path("///api///v1///").unwrap(),
            "/api/v1"
        );
    }

    #[test]
    fn surrounding_whitespace_trimmed() {
        assert_eq!(ApiGateway::normalize_prefix_path("  /cf  ").unwrap(), "/cf");
    }

    #[test]
    fn nested_path_preserved() {
        assert_eq!(
            ApiGateway::normalize_prefix_path("/api/v1").unwrap(),
            "/api/v1"
        );
    }

    #[test]
    fn dot_in_path_allowed() {
        assert_eq!(
            ApiGateway::normalize_prefix_path("/api/v1.0").unwrap(),
            "/api/v1.0"
        );
    }

    #[test]
    fn rejects_html_injection() {
        let result = ApiGateway::normalize_prefix_path(r#""><script>alert(1)</script>"#);
        assert!(result.is_err());
    }

    #[test]
    fn rejects_spaces_in_path() {
        let result = ApiGateway::normalize_prefix_path("/my path");
        assert!(result.is_err());
    }

    #[test]
    fn rejects_query_string_chars() {
        let result = ApiGateway::normalize_prefix_path("/api?foo=bar");
        assert!(result.is_err());
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod problem_openapi_tests {
    use super::*;
    use axum::Json;
    use serde_json::Value;
    use toolkit::api::{Missing, OperationBuilder};

    async fn dummy_handler() -> Json<Value> {
        Json(serde_json::json!({"ok": true}))
    }

    #[tokio::test]
    async fn openapi_includes_problem_schema_and_response() {
        let api = ApiGateway::default();
        let router = axum::Router::new();

        // Build a route with a problem+json response
        let _router = OperationBuilder::<Missing, Missing, ()>::get("/tests/v1/problem-demo")
            .anonymous()
            .summary("Problem demo")
            .problem_response(&api, http::StatusCode::BAD_REQUEST, "Bad Request") // <-- registers Problem + sets content type
            .handler(dummy_handler)
            .register(router, &api);

        let doc = api.build_openapi().expect("openapi");
        let v = serde_json::to_value(&doc).expect("json");

        // 1) Problem exists in components.schemas
        let problem = v
            .pointer("/components/schemas/Problem")
            .expect("Problem schema missing");
        assert!(
            problem.get("$ref").is_none(),
            "Problem must be a real object, not a self-ref"
        );

        // 2) Response under /paths/... references Problem and has correct media type
        let path_obj = v
            .pointer("/paths/~1tests~1v1~1problem-demo/get/responses/400")
            .expect("400 response missing");

        // Check what content types exist
        let content_obj = path_obj.get("content").expect("content object missing");
        assert!(
            content_obj.get("application/problem+json").is_some(),
            "application/problem+json content missing. Available content: {}",
            serde_json::to_string_pretty(content_obj).unwrap()
        );

        let content = path_obj
            .pointer("/content/application~1problem+json")
            .expect("application/problem+json content missing");
        // $ref to Problem
        let schema_ref = content
            .pointer("/schema/$ref")
            .and_then(|r| r.as_str())
            .unwrap_or("");
        assert_eq!(schema_ref, "#/components/schemas/Problem");
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod sse_openapi_tests {
    use super::*;
    use axum::Json;
    use serde_json::Value;
    use toolkit::api::{Missing, OperationBuilder};

    #[derive(Clone)]
    #[toolkit_macros::api_dto(request, response)]
    struct UserEvent {
        id: u32,
        message: String,
    }

    async fn sse_handler() -> axum::response::sse::Sse<
        impl futures_core::Stream<Item = Result<axum::response::sse::Event, std::convert::Infallible>>,
    > {
        let b = toolkit::SseBroadcaster::<UserEvent>::new(4);
        b.sse_response()
    }

    #[tokio::test]
    async fn openapi_has_sse_content() {
        let api = ApiGateway::default();
        let router = axum::Router::new();

        let _router = OperationBuilder::<Missing, Missing, ()>::get("/tests/v1/demo/sse")
            .summary("Demo SSE")
            .handler(sse_handler)
            .anonymous()
            .sse_json::<UserEvent>(&api, "SSE of UserEvent")
            .register(router, &api);

        let doc = api.build_openapi().expect("openapi");
        let v = serde_json::to_value(&doc).expect("json");

        // schema is materialized
        let schema = v
            .pointer("/components/schemas/UserEvent")
            .expect("UserEvent missing");
        assert!(schema.get("$ref").is_none());

        // content is text/event-stream with $ref to our schema
        let refp = v
            .pointer("/paths/~1tests~1v1~1demo~1sse/get/responses/200/content/text~1event-stream/schema/$ref")
            .and_then(|x| x.as_str())
            .unwrap_or_default();
        assert_eq!(refp, "#/components/schemas/UserEvent");
    }

    #[tokio::test]
    async fn openapi_sse_additional_response() {
        async fn mixed_handler() -> Json<Value> {
            Json(serde_json::json!({"ok": true}))
        }

        let api = ApiGateway::default();
        let router = axum::Router::new();

        let _router = OperationBuilder::<Missing, Missing, ()>::get("/tests/v1/demo/mixed")
            .summary("Mixed responses")
            .anonymous()
            .handler(mixed_handler)
            .json_response(http::StatusCode::OK, "Success response")
            .sse_json::<UserEvent>(&api, "Additional SSE stream")
            .register(router, &api);

        let doc = api.build_openapi().expect("openapi");
        let v = serde_json::to_value(&doc).expect("json");

        // Check that both response types are present
        let responses = v
            .pointer("/paths/~1tests~1v1~1demo~1mixed/get/responses")
            .expect("responses");

        // JSON response exists
        assert!(responses.get("200").is_some());

        // SSE response exists (could be another 200 or different status)
        let response_content = responses.get("200").and_then(|r| r.get("content"));
        assert!(response_content.is_some());

        // UserEvent schema is registered
        let schema = v
            .pointer("/components/schemas/UserEvent")
            .expect("UserEvent missing");
        assert!(schema.get("$ref").is_none());
    }

    #[tokio::test]
    async fn test_axum_to_openapi_path_conversion() {
        // Define a route with path parameters using Axum 0.8+ style {id}
        async fn user_handler() -> Json<Value> {
            Json(serde_json::json!({"user_id": "123"}))
        }

        let api = ApiGateway::default();
        let router = axum::Router::new();

        let _router = OperationBuilder::<Missing, Missing, ()>::get("/tests/v1/users/{id}")
            .summary("Get user by ID")
            .anonymous()
            .path_param("id", "User ID")
            .handler(user_handler)
            .json_response(http::StatusCode::OK, "User details")
            .register(router, &api);

        // Verify the operation was stored with {id} path (same for Axum 0.8 and OpenAPI)
        let ops: Vec<_> = api
            .openapi_registry
            .operation_specs
            .iter()
            .map(|e| e.value().clone())
            .collect();
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].path, "/tests/v1/users/{id}");

        // Verify OpenAPI doc also has {id} (no conversion needed for regular params)
        let doc = api.build_openapi().expect("openapi");
        let v = serde_json::to_value(&doc).expect("json");

        let paths = v.get("paths").expect("paths");
        assert!(
            paths.get("/tests/v1/users/{id}").is_some(),
            "OpenAPI should use {{id}} placeholder"
        );
    }

    #[tokio::test]
    async fn test_multiple_path_params_conversion() {
        async fn item_handler() -> Json<Value> {
            Json(serde_json::json!({"ok": true}))
        }

        let api = ApiGateway::default();
        let router = axum::Router::new();

        let _router = OperationBuilder::<Missing, Missing, ()>::get(
            "/tests/v1/projects/{project_id}/items/{item_id}",
        )
        .summary("Get project item")
        .anonymous()
        .path_param("project_id", "Project ID")
        .path_param("item_id", "Item ID")
        .handler(item_handler)
        .json_response(http::StatusCode::OK, "Item details")
        .register(router, &api);

        // Verify storage and OpenAPI both use {param} syntax
        let ops: Vec<_> = api
            .openapi_registry
            .operation_specs
            .iter()
            .map(|e| e.value().clone())
            .collect();
        assert_eq!(
            ops[0].path,
            "/tests/v1/projects/{project_id}/items/{item_id}"
        );

        let doc = api.build_openapi().expect("openapi");
        let v = serde_json::to_value(&doc).expect("json");
        let paths = v.get("paths").expect("paths");
        assert!(
            paths
                .get("/tests/v1/projects/{project_id}/items/{item_id}")
                .is_some()
        );
    }

    #[tokio::test]
    async fn test_wildcard_path_conversion() {
        async fn static_handler() -> Json<Value> {
            Json(serde_json::json!({"ok": true}))
        }

        let api = ApiGateway::default();
        let router = axum::Router::new();

        // Axum 0.8 uses {*path} for wildcards
        let _router = OperationBuilder::<Missing, Missing, ()>::get("/tests/v1/static/{*path}")
            .summary("Serve static files")
            .anonymous()
            .handler(static_handler)
            .json_response(http::StatusCode::OK, "File content")
            .register(router, &api);

        // Verify internal storage keeps Axum wildcard syntax {*path}
        let ops: Vec<_> = api
            .openapi_registry
            .operation_specs
            .iter()
            .map(|e| e.value().clone())
            .collect();
        assert_eq!(ops[0].path, "/tests/v1/static/{*path}");

        // Verify OpenAPI converts wildcard to {path} (without asterisk)
        let doc = api.build_openapi().expect("openapi");
        let v = serde_json::to_value(&doc).expect("json");
        let paths = v.get("paths").expect("paths");
        assert!(
            paths.get("/tests/v1/static/{path}").is_some(),
            "Wildcard {{*path}} should be converted to {{path}} in OpenAPI"
        );
        assert!(
            paths.get("/static/{*path}").is_none(),
            "OpenAPI should not have Axum-style {{*path}}"
        );
    }

    #[tokio::test]
    async fn test_multipart_file_upload_openapi() {
        async fn upload_handler() -> Json<Value> {
            Json(serde_json::json!({"uploaded": true}))
        }

        let api = ApiGateway::default();
        let router = axum::Router::new();

        let _router = OperationBuilder::<Missing, Missing, ()>::post("/tests/v1/files/upload")
            .operation_id("upload_file")
            .anonymous()
            .summary("Upload a file")
            .multipart_file_request("file", Some("File to upload"))
            .handler(upload_handler)
            .json_response(http::StatusCode::OK, "Upload successful")
            .register(router, &api);

        // Build OpenAPI and verify multipart schema
        let doc = api.build_openapi().expect("openapi");
        let v = serde_json::to_value(&doc).expect("json");

        let paths = v.get("paths").expect("paths");
        let upload_path = paths
            .get("/tests/v1/files/upload")
            .expect("/tests/v1/files/upload path");
        let post_op = upload_path.get("post").expect("POST operation");

        // Verify request body exists
        let request_body = post_op.get("requestBody").expect("requestBody");
        let content = request_body.get("content").expect("content");
        let multipart = content
            .get("multipart/form-data")
            .expect("multipart/form-data content type");

        // Verify schema structure
        let schema = multipart.get("schema").expect("schema");
        assert_eq!(
            schema.get("type").and_then(|v| v.as_str()),
            Some("object"),
            "Schema should be of type object"
        );

        // Verify properties
        let properties = schema.get("properties").expect("properties");
        let file_prop = properties.get("file").expect("file property");
        assert_eq!(
            file_prop.get("type").and_then(|v| v.as_str()),
            Some("string"),
            "File field should be of type string"
        );
        assert_eq!(
            file_prop.get("format").and_then(|v| v.as_str()),
            Some("binary"),
            "File field should have format binary"
        );

        // Verify required fields
        let required = schema.get("required").expect("required");
        let required_arr = required.as_array().expect("required should be array");
        assert_eq!(required_arr.len(), 1);
        assert_eq!(required_arr[0].as_str(), Some("file"));
    }
}
