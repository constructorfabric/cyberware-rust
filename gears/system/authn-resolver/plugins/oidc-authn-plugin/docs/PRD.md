Created:  2026-04-09 by Virtuozzo International GmbH
Updated:  2026-07-24 by Virtuozzo International GmbH

# PRD — OIDC AuthN Resolver Plugin

<!-- toc -->

- [1. Overview](#1-overview)
  - [1.1 Purpose](#11-purpose)
  - [1.2 Background / Problem Statement](#12-background--problem-statement)
  - [1.3 Goals (Business Outcomes)](#13-goals-business-outcomes)
  - [1.4 Glossary](#14-glossary)
- [2. Actors](#2-actors)
  - [2.1 Human Actors](#21-human-actors)
  - [2.2 System Actors](#22-system-actors)
- [3. Operational Concept & Environment](#3-operational-concept--environment)
- [4. Scope](#4-scope)
  - [4.1 In Scope](#41-in-scope)
  - [4.2 Out of Scope](#42-out-of-scope)
- [5. Functional Requirements](#5-functional-requirements)
  - [5.1 Token Validation](#51-token-validation)
  - [5.2 Claim Extraction & Mapping](#52-claim-extraction--mapping)
  - [5.3 OIDC Auto-Configuration](#53-oidc-auto-configuration)
  - [5.4 S2S Client Credentials Exchange](#54-s2s-client-credentials-exchange)
  - [5.5 Plugin Discovery & Registration](#55-plugin-discovery--registration)
  - [5.6 Resilience & Timeouts](#56-resilience--timeouts)
- [6. Non-Functional Requirements](#6-non-functional-requirements)
  - [6.1 Gear-Specific NFRs](#61-gear-specific-nfrs)
  - [6.2 NFR Exclusions](#62-nfr-exclusions)
- [7. Public Library Interfaces](#7-public-library-interfaces)
  - [7.1 Public API Surface](#71-public-api-surface)
  - [7.2 External Integration Contracts](#72-external-integration-contracts)
- [8. Use Cases](#8-use-cases)
  - [Authenticate API Request](#authenticate-api-request)
  - [Service-to-Service Authentication](#service-to-service-authentication)
- [9. Acceptance Criteria](#9-acceptance-criteria)
- [10. Dependencies](#10-dependencies)
- [11. Assumptions](#11-assumptions)
- [12. Risks](#12-risks)
- [13. Open Questions](#13-open-questions)
- [14. Traceability](#14-traceability)

<!-- /toc -->

> **Abbreviation**: AuthN = **Authentication**. Used throughout this document.

## 1. Overview

### 1.1 Purpose

The OIDC AuthN Resolver Plugin provides authentication services for the Gears middleware by integrating with OpenID Connect-compliant Identity Providers (IdPs). It validates JWT access tokens, extracts identity claims, and produces the platform's `SecurityContext` structure that downstream gears consume for authorization and tenant-scoped access control.

The plugin is a vendor-specific implementation of the AuthN Resolver plugin interface, following the Gears gateway + plugin architecture pattern. It ships as the default authentication plugin for deployments using standard OIDC-compliant IdPs.

### 1.2 Background / Problem Statement

Gears need a consistent, high-performance mechanism to authenticate incoming API requests and establish caller identity. Without a centralized authentication plugin, each gear would independently validate tokens, parse claims, and construct identity context — leading to duplicated logic, inconsistent claim interpretation, and security gaps.

The platform requires multi-tenant authentication where tenant isolation is achieved via claims embedded in access tokens rather than per-tenant IdP instances. A single OIDC issuer can serve many tenants (each tenant identified by a claim), while multiple issuers can coexist for different tenant groups. The target is 10,000+ tenants without requiring per-tenant IdP configuration.

Additionally, gears performing background work (scheduled jobs, event handlers) require authenticated `SecurityContext` instances without an incoming HTTP request. This necessitates a service-to-service credential exchange mechanism that follows the same authentication pipeline.

### 1.3 Goals (Business Outcomes)

- Enable any OIDC-compliant IdP to serve as the platform identity provider without code changes — configuration only. The IdP must use UUID-format `sub` claims (see Assumptions) and include a tenant identifier claim; IdPs that use non-UUID subject identifiers (e.g., Auth0 `auth0|…`, Azure AD OIDs) require a protocol mapper / claim transformation to produce UUID-format `sub` values. **At launch**: validated with at least two OIDC-compliant IdPs (e.g., Keycloak, Auth0).
- Provide sub-5ms p95 authentication latency for the request hot path, eliminating the IdP as a per-request bottleneck. **At launch**: measured via load test at 10K rps with warm JWKS cache.
- Support claim-based multi-tenancy scaling to 10,000+ tenants per issuer. Multiple trusted issuers are supported for deployments that partition tenants across IdP instances. **At launch**: validated at 100 tenants; **within 6 months**: validated at 10,000+ tenants via scale test.
- Ensure zero authentication bypasses — every error path produces a rejection, never a default-allow. **Continuously**: verified via unit tests covering every error variant and CI enforcement.

### 1.4 Glossary

| Term | Definition |
|------|------------|
| AuthN | Authentication — verifying the identity of a caller. |
| AuthZ | Authorization — determining what an authenticated caller is allowed to do. |
| SecurityContext | Platform-wide identity structure containing subject ID, tenant ID, subject type, token scopes, and bearer token. Produced by the AuthN plugin, consumed by all downstream gears. |
| OIDC | OpenID Connect — an identity layer on top of OAuth 2.0 that provides standard endpoints for discovery, token validation, and identity claims. |
| JWKS | JSON Web Key Set — a set of cryptographic keys used to verify JWT signatures, published by the IdP. |
| S2S | Service-to-service — communication between platform gears without an end-user HTTP request. |
| IdP | Identity Provider — the external OIDC-compliant service that issues and manages access tokens. |
| First-party app | A platform-owned application (portal, CLI) that receives unrestricted scopes. |
| Third-party app | A partner integration that receives only its granted OAuth2 scopes. |

## 2. Actors

### 2.1 Human Actors

#### Platform Administrator

**ID**: `cpt-cf-authn-plugin-actor-platform-admin`

- **Role**: Configures the OIDC plugin — trusted issuers, claim mappings, audience settings, first-party client lists, S2S credentials.
- **Needs**: Clear configuration schema; fail-fast on misconfiguration; observable plugin health via metrics and logs.

### 2.2 System Actors

#### API Gateway / AuthN Middleware

**ID**: `cpt-cf-authn-plugin-actor-api-gateway`

- **Role**: Extracts bearer tokens from incoming HTTP `Authorization` headers and delegates authentication to the AuthN Resolver gateway, which routes to this plugin.

#### Domain Gears (Request Path)

**ID**: `cpt-cf-authn-plugin-actor-domain-gear`

- **Role**: Consume the `SecurityContext` produced by the plugin for PEP (Policy Enforcement Point) decisions. Do not interact with the plugin directly — receive `SecurityContext` via middleware injection.

#### Domain Gears (Background Tasks)

**ID**: `cpt-cf-authn-plugin-actor-background-gear`

- **Role**: Invoke `exchange_client_credentials` to obtain an authenticated `SecurityContext` for background work (scheduled jobs, event handlers, async processing) when no incoming HTTP request is available.

#### OIDC Identity Provider

**ID**: `cpt-cf-authn-plugin-actor-idp`

- **Role**: External system that issues JWT access tokens, publishes JWKS for signature verification, exposes OIDC Discovery endpoints, and provides a token endpoint for S2S credential exchange.

## 3. Operational Concept & Environment

No gear-specific environment constraints beyond project defaults. The plugin runs as an in-process library within the host gear's runtime. All IdP communication uses HTTPS with certificate validation.

## 4. Scope

### 4.1 In Scope

- JWT access token validation (local, via cached JWKS)
- OIDC Discovery auto-configuration (`.well-known/openid-configuration`)
- Configurable claim mapping to `SecurityContext` fields
- Claim-based multi-tenancy (tenant claim in token, one or more trusted issuers)
- First-party vs third-party app detection via client ID
- S2S client credentials exchange (OAuth2 `client_credentials` grant)
- Configurable HTTP request timeout for all outbound IdP calls
- Transient-failure retry with exponential backoff + jitter (network errors, HTTP 5xx, HTTP 429)
- Per-host circuit breaker for IdP resilience (globally enable/disable)
- Plugin registration via Gears ClientHub with GTS identity

### 4.2 Out of Scope

- **Opaque token introspection** (RFC 7662) — JWT-only; non-JWT tokens are rejected. If needed, implement as a separate plugin.
- **Authorization decisions** — AuthZ Resolver and Tenant Resolver handle policy evaluation, access filtering, and barrier enforcement downstream.
- **Token issuance, refresh, or revocation** — managed by the IdP.
- **User management or identity provisioning** — managed by the IdP.
- **MFA enforcement** — MFA is an IdP-side concern; tokens arriving at the plugin have already passed IdP authentication flows.
- **Token revocation checking** — JWT local validation cannot detect revoked tokens before expiry. Short token lifetimes (5–15 min) mitigate this. Real-time revocation requires a separate plugin.

## 5. Functional Requirements

### 5.1 Token Validation

#### JWT Local Validation

- [x] `p1` - **ID**: `cpt-cf-authn-plugin-fr-jwt-validation`

The plugin MUST validate JWT access tokens locally using cached JWKS fetched from the IdP's standard `jwks_uri` endpoint. Validation MUST include: signature verification (RS256, ES256), `exp` claim validation (must be in future, with configurable clock skew leeway — default 60s), `iss` claim validation (must be in trusted issuers allowlist), and `aud` claim validation (when configured). The `alg: none` algorithm MUST never be accepted, even if misconfigured — tokens presenting `alg: none` MUST be rejected.

- **Rationale**: Local validation provides sub-millisecond latency and eliminates the IdP as a per-request bottleneck.
- **Actors**: `cpt-cf-authn-plugin-actor-api-gateway`

#### Non-JWT Token Rejection

- [x] `p1` - **ID**: `cpt-cf-authn-plugin-fr-non-jwt-rejection`

The plugin MUST reject tokens that are not valid JWTs (not three base64url-encoded segments) with `Unauthorized("unsupported token format")`.

- **Rationale**: Scope constraint — this plugin handles JWT only. Prevents undefined behavior on unexpected token formats.
- **Actors**: `cpt-cf-authn-plugin-actor-api-gateway`

#### Trusted Issuer Enforcement

- [x] `p1` - **ID**: `cpt-cf-authn-plugin-fr-trusted-issuers`

The plugin MUST maintain a configurable allowlist of trusted issuers. Tokens with an `iss` claim not matching any trusted issuer MUST be rejected with `Unauthorized("untrusted issuer")`.

The `jwt.trusted_issuers` configuration MUST be an ordered array. Entries are evaluated from top to bottom, and the first matching entry determines the discovery URL.

Each trusted issuer entry MUST define exactly one of:
- `issuer` — exact literal match against the token `iss`
- `issuer_pattern` — regular expression match against the token `iss`

Each entry MAY define `discovery_url`. If omitted, the plugin MUST use the token `iss` value as the discovery base URL. If `discovery_url` contains `{issuer}`, the placeholder MUST be replaced with the actual `iss` value from the token after the entry is matched.

When an entry matched via `issuer_pattern` is used, the matched pattern and the actual `iss` value MUST be logged at `WARN` level on first successful use to alert operators that pattern-based trust is active.

- **Rationale**: Ordered entries make precedence explicit, preserve fail-fast validation for invalid regexes at startup, and support deployments with many IdP instances sharing a common URL structure (e.g., per-tenant IdP partitions) without requiring an entry per instance. Optional `discovery_url` keeps the common case (`iss` is already the discovery base) simple while still supporting non-standard IdP layouts.
- **Actors**: `cpt-cf-authn-plugin-actor-platform-admin`, `cpt-cf-authn-plugin-actor-api-gateway`

#### JWKS Key Rotation Handling

- [x] `p1` - **ID**: `cpt-cf-authn-plugin-fr-key-rotation`

When a JWT presents a `kid` (Key ID) not found in the cached JWKS, the plugin MUST force-refresh the JWKS from the IdP. If the `kid` is still not found after refresh, the plugin MUST reject the token with `Unauthorized("signing key not found")`.

- **Rationale**: IdPs rotate signing keys periodically; the plugin must handle this without manual intervention or downtime.
- **Actors**: `cpt-cf-authn-plugin-actor-idp`

#### Audience Validation

- [x] `p2` - **ID**: `cpt-cf-authn-plugin-fr-audience-validation`

When `jwt.require_audience` is `true`, the plugin MUST reject tokens without an `aud` claim. When `jwt.expected_audience` is configured, the plugin MUST verify that at least one `aud` value matches the expected audience list (with glob pattern support).

- **Rationale**: Audience validation prevents token confusion attacks where a token intended for a different service is presented to this platform.
- **Actors**: `cpt-cf-authn-plugin-actor-platform-admin`

### 5.2 Claim Extraction & Mapping

#### Configurable Claim Mapping

- [x] `p1` - **ID**: `cpt-cf-authn-plugin-fr-claim-mapping`

The plugin MUST extract claims from validated JWTs and map them to `SecurityContext` fields using configurable claim names:
- `subject_id` (default: `"sub"`) — MUST parse as UUID; reject if missing or invalid.
- `subject_tenant_id` (vendor-configured) — MUST parse as UUID; reject if missing or invalid (see `cpt-cf-authn-plugin-fr-tenant-claim`).
- `subject_type` (vendor-configured, optional) — `None` when absent.
- `token_scopes` (default: `"scope"`) — split on spaces.

- **Rationale**: Different IdPs use different claim names for the same semantic fields. Configurable mapping enables IdP-agnostic operation.
- **Actors**: `cpt-cf-authn-plugin-actor-platform-admin`

#### Tenant Claim Requirement

- [x] `p1` - **ID**: `cpt-cf-authn-plugin-fr-tenant-claim`

The plugin MUST require a tenant identifier claim in every access token (claim name is vendor-configurable, e.g., `"tenant_id"`, `"org_id"`, `"account_id"`). The claim value MUST parse as UUID (RFC 4122). Tokens without the tenant claim MUST be rejected with `Unauthorized("missing tenant_id")`; tokens with a non-UUID tenant claim MUST be rejected with `Unauthorized("invalid tenant id")`.

- **Rationale**: Tenant isolation is foundational to the platform's multi-tenancy model. Allowing tokens without tenant identity would break downstream tenant-scoped access control.
- **Actors**: `cpt-cf-authn-plugin-actor-api-gateway`

#### First-Party vs Third-Party App Detection

- [x] `p2` - **ID**: `cpt-cf-authn-plugin-fr-first-party-detection`

When `jwt.first_party_clients` is configured, the plugin MUST check the token's `client_id`/`azp` claim against the list. First-party apps MUST receive `token_scopes = ["*"]` (unrestricted). Third-party apps MUST receive only their granted OAuth2 scopes from the token.

- **Rationale**: Platform-owned applications (portal, CLI) should not be capability-restricted by token scopes. Third-party integrations must be limited to their granted scopes.
- **Actors**: `cpt-cf-authn-plugin-actor-platform-admin`

### 5.3 OIDC Auto-Configuration

#### OIDC Discovery

- [x] `p1` - **ID**: `cpt-cf-authn-plugin-fr-oidc-discovery`

The plugin MUST fetch and cache the IdP's `.well-known/openid-configuration` document to resolve `jwks_uri` and `token_endpoint` dynamically. Only the issuer URL needs to be configured.

- **Rationale**: Eliminates manual endpoint configuration; standard OIDC Discovery makes the plugin work with any compliant IdP.
- **Actors**: `cpt-cf-authn-plugin-actor-idp`

#### JWKS Caching

- [x] `p1` - **ID**: `cpt-cf-authn-plugin-fr-jwks-caching`

The plugin MUST cache JWKS key sets in memory with configurable TTL (default: 1h) and max entries (default: 10). When the cache reaches `max_entries`, it MUST evict the least-recently-used (`LRU`) entry. Cache MUST be refreshable on unknown `kid` with a configurable minimum refresh interval to prevent abuse.

- **Rationale**: Caching eliminates per-request IdP network calls for the common case, achieving sub-millisecond validation latency.
- **Actors**: `cpt-cf-authn-plugin-actor-api-gateway`

### 5.4 S2S Client Credentials Exchange

#### Client Credentials Grant

- [x] `p1` - **ID**: `cpt-cf-authn-plugin-fr-s2s-exchange`

The plugin MUST implement OAuth2 `client_credentials` grant (RFC 6749 §4.4) to obtain access tokens for service-to-service communication. The obtained JWT MUST be validated through the same pipeline as bearer token authentication. The resulting `SecurityContext` MUST include the obtained access token as `bearer_token`.

- **Rationale**: Gears performing background work need authenticated `SecurityContext` instances without an incoming HTTP request.
- **Actors**: `cpt-cf-authn-plugin-actor-background-gear`

#### S2S Token Caching

- [x] `p2` - **ID**: `cpt-cf-authn-plugin-fr-s2s-caching`

The plugin MUST cache S2S authentication results keyed by `(client_id, normalized_scopes, credential_fingerprint)` with TTL = `min(token expires_in, s2s_oauth.token_cache.ttl)`.

`normalized_scopes` means the requested scope set after whitespace normalization, deduplication, lexicographic sorting, and joining with a single space. Missing or empty `scopes` MUST normalize to the empty set.

`credential_fingerprint` MUST be derived from `client_secret` without storing the raw secret in the cache key.

Concurrent cache misses for the same full cache key MUST NOT cause duplicate IdP requests. When the S2S result cache reaches `s2s_oauth.token_cache.max_entries`, it MUST evict the least-recently-used (`LRU`) entry. TTL expiry still applies independently.

- **Rationale**: Avoids repeated IdP round-trips for the same S2S token variant while preventing reuse across different scope sets or credential values; also prevents thundering herd on cache miss.
- **Actors**: `cpt-cf-authn-plugin-actor-background-gear`

#### S2S Default Subject Type

- [x] `p3` - **ID**: `cpt-cf-authn-plugin-fr-s2s-default-subject-type`

When `s2s_oauth.default_subject_type` is configured and the obtained S2S token does not contain a `subject_type` claim, the plugin MUST apply the configured default value as `SecurityContext.subject_type`.

- **Rationale**: Many IdPs omit type claims from `client_credentials` tokens. This ensures `SecurityContext.subject_type` is always populated for S2S flows.
- **Actors**: `cpt-cf-authn-plugin-actor-platform-admin`

### 5.5 Plugin Discovery & Registration

#### ClientHub Registration

- [x] `p1` - **ID**: `cpt-cf-authn-plugin-fr-clienthub-registration`

The plugin MUST register `dyn AuthNResolverPluginClient` with Gears ClientHub using GTS schema identity at startup. Registration MUST include vendor key (`"constructorfabric"`), priority, and display name.

- **Rationale**: Enables the AuthN Resolver gateway to discover and select the active plugin at runtime via ClientHub lookup.
- **Actors**: `cpt-cf-authn-plugin-actor-api-gateway`

### 5.6 Resilience & Timeouts

> **Breaking config change**: `discovery_timeout` is removed. Its role is replaced by `http_client.request_timeout` (default `5s`), which applies uniformly to every outbound IdP HTTP call (discovery, JWKS, S2S token endpoint). Deployments that set a non-default `discovery_timeout` MUST migrate to `http_client.request_timeout`.

A complete operator configuration example is maintained at [`config-example.yaml`](config-example.yaml).

#### Configurable Request Timeout

- [x] `p1` - **ID**: `cpt-cf-authn-plugin-fr-request-timeout`

The plugin MUST apply a configurable `http_client.request_timeout` (default `5s`) to every outbound IdP HTTP call — OIDC discovery, JWKS fetch, and the S2S token endpoint. The timeout MUST be applied **per attempt**; a retried call receives a fresh `request_timeout` on each attempt.

- **Rationale**: Bounds worst-case latency contributed by IdP calls and prevents unbounded requests from exhausting connection pools. A single knob keeps operator configuration simple.
- **Actors**: `cpt-cf-authn-plugin-actor-platform-admin`

#### Transient-Failure Retry

- [x] `p1` - **ID**: `cpt-cf-authn-plugin-fr-retry-policy`

The plugin MUST retry transient outbound IdP failures with exponential backoff plus jitter, bounded by a configurable retry policy:

- Retryable: connection errors (DNS, refused, TLS, reset), HTTP 5xx, HTTP 429.
- Not retryable: request timeout (a slow failure — retrying multiplies user-facing latency and adds load to a struggling IdP; repeated timeouts are absorbed by the per-host circuit breaker instead), HTTP 4xx other than 429 (permanent), 2xx with unparseable body.
- `retry_policy.max_retries` (default `3`) MUST bound the number of retry attempts **after** the initial call. `0` MUST disable retries. `max_retries = 3` means the worst case is 1 initial call + 3 retries = 4 total requests.
- When an HTTP 429 response includes a `Retry-After` header, the plugin MUST honor it, capped by `retry_policy.max_backoff_ms`.
- Retries MUST occur **inside** each circuit-breaker call — one logical operation MUST equal one breaker attempt, so exhausted retries count as a single failure toward the breaker's `failure_threshold`.

- **Rationale**: Transient blips (one-off network hiccups, brief IdP 5xx, rate-limit bursts) should not surface as failed authentications. Inside-the-breaker semantics prevent retry amplification from prematurely tripping the breaker.
- **Actors**: `cpt-cf-authn-plugin-actor-platform-admin`, `cpt-cf-authn-plugin-actor-api-gateway`

#### Per-Host Circuit Breaker (Toggleable)

- [x] `p1` - **ID**: `cpt-cf-authn-plugin-fr-circuit-breaker`

The plugin MUST maintain circuit-breaker state keyed by the outbound HTTP host so that degradation of one IdP host does not block calls to other hosts (including other trusted issuers served by a different host). When a host's breaker is Open, calls to **that host** that require a fresh IdP response MUST fail with `ServiceUnavailable`; calls to other hosts MUST continue unaffected. The breaker MUST be globally enable/disable-able via `circuit_breaker.enabled` (default `true`). When disabled, every host MUST behave as if permanently Closed — retries and `http_client.request_timeout` still apply.

- **Rationale**: The plugin already supports multiple trusted issuers that can live on different IdP hosts with independent availability. A single shared breaker would cause one flaky IdP to block authentication against a healthy one. The global toggle lets operators disable breaker behavior in test/dev or when relying solely on caching + retry.
- **Actors**: `cpt-cf-authn-plugin-actor-platform-admin`, `cpt-cf-authn-plugin-actor-api-gateway`

## 6. Non-Functional Requirements

### 6.1 Gear-Specific NFRs

#### JWT Validation Latency

- [x] `p1` - **ID**: `cpt-cf-authn-plugin-nfr-jwt-latency`

JWT local validation MUST complete within 5ms at p95 with a warm JWKS cache at 10K requests per second.

- **Threshold**: p95 ≤ 5ms, p99 ≤ 10ms (warm cache)
- **Rationale**: Authentication is on the critical request path — every API call flows through it. Latency here directly impacts user-perceived response times.
- **Architecture Allocation**: See DESIGN.md § NFR Allocation — Token Validator + JWKS Cache

#### Plugin Availability

- [x] `p1` - **ID**: `cpt-cf-authn-plugin-nfr-availability`

Plugin availability MUST be ≥ 99.99%. When the IdP is unreachable, the plugin MUST continue to validate JWTs using cached JWKS (stale-while-revalidate). A per-host circuit breaker (globally toggleable via `circuit_breaker.enabled`) MUST prevent cascade failure when one IdP host is degraded, without blocking calls to other healthy hosts. Transient outbound failures (connection errors, HTTP 5xx, HTTP 429) MUST be retried per a configurable retry policy before being treated as terminal.

- **Threshold**: ≥ 99.99% availability
- **Rationale**: Authentication unavailability blocks all authenticated API traffic. Cached JWKS, bounded retries, and a per-host circuit breaker ensure continued operation during IdP degradation and isolate blast radius when multiple trusted issuers are served by different IdP hosts.
- **Architecture Allocation**: See DESIGN.md § Reliability Architecture — Circuit Breaker + JWKS Cache

#### Fail-Closed Guarantee

- [x] `p1` - **ID**: `cpt-cf-authn-plugin-nfr-fail-closed`

Every error path MUST return an explicit rejection error — never a default-allow. No authentication bypass is permitted under any failure condition.

- **Threshold**: Zero authentication bypasses under all failure modes
- **Rationale**: A single authentication bypass could expose all tenant data. This is the foundational security commitment.
- **Architecture Allocation**: See DESIGN.md § Principles — Fail-Closed Authentication

#### Tenant Isolation

- [x] `p1` - **ID**: `cpt-cf-authn-plugin-nfr-tenant-isolation`

Zero cross-tenant data leaks via the authentication layer. Tokens without a valid tenant claim MUST be rejected.

- **Threshold**: Zero cross-tenant leaks
- **Rationale**: Multi-tenancy is the platform's core value proposition. AuthN-level tenant isolation is the first line of defense.
- **Architecture Allocation**: See DESIGN.md § Principles — Claim-Based Tenant Isolation

#### S2S Exchange Latency

- [x] `p2` - **ID**: `cpt-cf-authn-plugin-nfr-s2s-latency`

S2S credential exchange MUST complete within 500ms at p95 (cold cache, including IdP round-trip). Warm cache (token cached) MUST complete within 1ms at p95.

- **Threshold**: p95 ≤ 500ms cold, p95 ≤ 1ms warm
- **Rationale**: Background tasks should not be delayed by authentication overhead. Token caching ensures sub-millisecond for the common case.
- **Architecture Allocation**: See DESIGN.md § NFR Allocation — Token Client + Token Cache

#### Token Security

- [x] `p1` - **ID**: `cpt-cf-authn-plugin-nfr-security`

Bearer tokens and client secrets MUST never be logged, persisted, or included in debug output. Tokens MUST be wrapped in `SecretString` types that are excluded from `Debug`/`Display`/serialization.

- **Threshold**: Zero token exposures in logs or debug output
- **Rationale**: Token leakage enables session hijacking and impersonation across all tenants.
- **Architecture Allocation**: See DESIGN.md § Security Architecture

### 6.2 NFR Exclusions

- **Database performance**: Not applicable — the plugin has no persistent database storage; in-memory caches only.
- **Frontend performance**: Not applicable — backend authentication component with no user-facing interface.

## 7. Public Library Interfaces

### 7.1 Public API Surface

#### AuthN Resolver Gateway Interface

- [ ] `p1` - **ID**: `cpt-cf-authn-plugin-interface-gateway`

- **Type**: Rust async trait (`AuthNResolverClient`)
- **Stability**: stable
- **Description**: Public API for platform gears. Provides `authenticate(bearer_token)` and `exchange_client_credentials(request)`. Delegates to the configured plugin.
- **Breaking Change Policy**: Major version bump required; within a version, only additive changes.

#### AuthN Resolver Plugin Interface

- [ ] `p1` - **ID**: `cpt-cf-authn-plugin-interface-plugin`

- **Type**: Rust async trait (`AuthNResolverPluginClient`)
- **Stability**: stable
- **Description**: Implemented by each vendor-specific plugin. Provides `authenticate(bearer_token)` and `exchange_client_credentials(request)`.
- **Breaking Change Policy**: Major version bump required; within a version, only additive changes (new methods with default implementations).

### 7.2 External Integration Contracts

#### OIDC Identity Provider Contract

- [ ] `p1` - **ID**: `cpt-cf-authn-plugin-contract-oidc-idp`

- **Direction**: required from external system
- **Protocol/Format**: HTTPS GET (discovery, JWKS), HTTPS POST (token endpoint)
- **Compatibility**: OIDC Discovery 1.0, RFC 7517 (JWKS), RFC 6749 §4.4 (client_credentials grant)

#### SecurityContext Contract

- [ ] `p1` - **ID**: `cpt-cf-authn-plugin-contract-security-context`

- **Direction**: provided by library
- **Protocol/Format**: Rust struct (`SecurityContext` from `toolkit-security`)
- **Compatibility**: Platform `SecurityContext` builder contract; fields: `subject_id`, `subject_tenant_id`, `subject_type`, `token_scopes`, `bearer_token`.

## 8. Use Cases

### Authenticate API Request

- [ ] `p1` - **ID**: `cpt-cf-authn-plugin-usecase-authenticate-request`

**Actor**: `cpt-cf-authn-plugin-actor-api-gateway`

**Preconditions**:
- Plugin is registered with ClientHub
- Trusted issuers and claim mappings are configured
- JWKS is cached (warm) or IdP is reachable (cold)

**Main Flow**:
1. API Gateway extracts bearer token from `Authorization` header.
2. Gateway calls `authenticate(bearer_token)` on AuthN Resolver.
3. Plugin validates JWT signature, expiry, issuer, and audience.
4. Plugin extracts claims and maps to `SecurityContext`.
5. Plugin returns `AuthenticationResult` containing `SecurityContext`.
6. Middleware injects `SecurityContext` into the request for downstream PEP.

**Postconditions**:
- `SecurityContext` is populated with `subject_id`, `subject_tenant_id`, `token_scopes`.

**Alternative Flows**:
- **Invalid token**: Plugin returns `Unauthorized` with reason. Middleware returns 401 to client.
- **IdP unreachable, no cached JWKS**: Plugin returns `ServiceUnavailable`. Middleware returns 503 to client.

### Service-to-Service Authentication

- [ ] `p2` - **ID**: `cpt-cf-authn-plugin-usecase-s2s-exchange`

**Actor**: `cpt-cf-authn-plugin-actor-background-gear`

**Preconditions**:
- S2S configuration (IdP discovery URL, client credentials) is provided
- Plugin is registered with ClientHub

**Main Flow**:
1. Background gear constructs a `ClientCredentialsRequest` (containing `client_id`, `client_secret`, and `scopes`) and calls `exchange_client_credentials(request)`.
2. Plugin normalizes `scopes`, derives a `credential_fingerprint` from `client_secret`, builds the S2S cache key, and checks token cache.
3. On cache miss: Plugin resolves token endpoint and performs `client_credentials` grant.
4. Plugin validates the obtained JWT through the standard validation pipeline.
5. Plugin maps claims to `SecurityContext`, applies `default_subject_type` if needed.
6. Plugin caches the token and returns `AuthenticationResult`.

**Postconditions**:
- Background gear has an authenticated `SecurityContext` with a valid `bearer_token`.

**Alternative Flows**:
- **Cached token available**: Plugin returns cached result without IdP call (sub-millisecond).
- **Invalid credentials**: Plugin returns `TokenAcquisitionFailed`. No token is cached.
- **IdP unreachable, no cached S2S result**: Plugin returns `ServiceUnavailable`. No token is cached.

## 9. Acceptance Criteria

- [ ] JWT validation with warm JWKS cache completes within 5ms at p95 under 10K rps load.
- [ ] Tokens from untrusted issuers are rejected; tokens without `tenant_id` claim are rejected; non-JWT tokens are rejected; trusted issuer precedence is deterministic by declaration order.
- [ ] JWKS key rotation is handled automatically (unknown `kid` triggers refresh).
- [ ] S2S credential exchange produces a valid `SecurityContext` with cached token reuse.
- [ ] Plugin registers with ClientHub and is discoverable by the AuthN Resolver gateway.
- [ ] No bearer tokens or client secrets appear in any log output.
- [ ] Circuit breaker opens **per host** after consecutive IdP failures; JWT validation continues with cached JWKS for that host; other hosts remain unaffected.
- [ ] `circuit_breaker.enabled: false` disables tripping globally — every host behaves as permanently Closed; the plugin relies on retries and caches.
- [ ] Transient IdP failures (connection errors, HTTP 5xx, HTTP 429) are retried up to `retry_policy.max_retries` additional attempts after the initial call (default `3`; `0` disables); HTTP 4xx other than 429 is not retried; `Retry-After` on 429 is honored (capped by `retry_policy.max_backoff_ms`).
- [ ] Every outbound IdP HTTP call respects `http_client.request_timeout` per attempt (default `5s`).

## 10. Dependencies

| Dependency | Description | Criticality |
|------------|-------------|-------------|
| `authn-resolver-sdk` | SDK traits, models, error types, GTS schema definition | `p1` |
| `toolkit-security` | `SecurityContext` and `SecurityContextBuilder` types | `p1` |
| `toolkit` | Plugin framework, GTS integration, ClientHub registration | `p1` |
| OIDC Identity Provider | External IdP that issues tokens, publishes JWKS, exposes discovery and token endpoints | `p1` |
| `jsonwebtoken` crate | JWT signature verification library (MIT) | `p1` |
| `reqwest` crate | HTTP client for IdP communication (MIT/Apache-2.0) | `p1` |

## 11. Assumptions

- The OIDC Identity Provider is compliant with OpenID Connect Discovery 1.0 and publishes a valid `.well-known/openid-configuration` document.
- Access tokens issued by the IdP are JWTs signed with RS256 or ES256 algorithms.
- The IdP includes a tenant identifier claim (vendor-configurable name) in every access token.
- The `sub` claim in access tokens is a valid UUID (RFC 4122). **Deployment constraint**: Many OIDC providers use non-UUID subject identifiers by default. The IdP must be configured with a protocol mapper / claim transformation to produce UUID-format `sub` values. Tokens with non-UUID `sub` claims are rejected with `Unauthorized("invalid subject id")`.
- Token lifetimes are short (5–15 minutes), providing adequate revocation protection without explicit revocation checking.
- Each tenant is served by exactly one OIDC issuer. A single issuer may serve many tenants (via the `tenant_id` claim). Multiple issuers may coexist in the same deployment (each configured in `jwt.trusted_issuers`), but a given tenant's users always originate from the same issuer.

## 12. Risks

| Risk | Impact | Mitigation |
|------|--------|------------|
| IdP outage blocks all authentication | All API requests fail with 503 | Stale-while-revalidate JWKS cache + bounded retry (`retry_policy`) for transient failures + **per-host** circuit breaker so degradation of one IdP host does not block calls to other hosts; JWT validation continues with cached keys |
| CVE in `jsonwebtoken` crate | Signature verification bypass — full authentication compromise | `cargo-audit` in CI; pin to reviewed versions; monitor RustSec advisories |
| IdP issues tokens without tenant claim | Tenant isolation broken for those tokens | `tenant_id` claim is required — tokens without it are rejected. IdP configuration must be validated during deployment |
| Token replay within expiry window | Unauthorized access using stolen valid token | Short token lifetimes (5–15 min) limit exposure window. For real-time revocation, a separate introspection plugin is needed |
| JWKS refresh flood (DoS) | Rate limit exhaustion or IdP overload | Rate-limited JWKS refresh (min 30s interval per issuer); single in-flight refresh; circuit breaker |

## 13. Open Questions

| # | Question | Impact | Owner | Target Date | Resolution |
|---|----------|--------|-------|-------------|------------|
| 1 | Should audience validation glob patterns support only `*` (substring wildcard) or also `?` (single character)? Overly broad patterns could weaken audience enforcement. | Security | Platform Architect | Before implementation start | **Resolved in DESIGN**: `*` substring wildcard only; no `?` or `**` support. See DESIGN.md § Token Validator `jwt.expected_audience` configuration. |
| 2 | What is the maximum acceptable JWKS staleness window when the IdP is unreachable? Indefinite stale-while-revalidate vs bounded grace period (e.g., 24h). | Availability vs Security | Platform Architect | Before implementation start | **Resolved in DESIGN**: Configurable via `jwks_cache.stale_ttl` (default 24h). Set to `0` to disable stale-while-revalidate. See DESIGN.md § OIDC Discovery configuration. |

## 14. Traceability

- **Design**: [DESIGN.md](DESIGN.md)
- **ADRs**: No standalone ADRs — key decisions documented in platform ADRs ([0002](../../../../../../docs/arch/authorization/ADR/0002-split-authn-authz-resolvers.md), [0003](../../../../../../docs/arch/authorization/ADR/0003-authn-resolver-minimalist-interface.md)) and in DESIGN.md Principles/Constraints sections.
- **Platform Auth Architecture**: [`docs/arch/authorization/DESIGN.md`](../../../../../../docs/arch/authorization/DESIGN.md)
