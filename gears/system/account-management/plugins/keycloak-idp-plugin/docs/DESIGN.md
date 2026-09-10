---
refs:
  - PRD.md
  - ../../../docs/DESIGN.md
  - ../../../docs/ADR/0001-cpt-cf-account-management-adr-idp-contract-separation.md
  - ../../../docs/ADR/0005-cpt-cf-account-management-adr-idp-user-identity-source-of-truth.md
  - ../../../docs/ADR/0006-cpt-cf-account-management-adr-idp-user-tenant-binding.md
---

Created:  2026-08-17 by Virtuozzo International GmbH
Updated:  2026-08-17 by Virtuozzo International GmbH

# Technical Design — Keycloak IdP Plugin

- [ ] `p3` - **ID**: `cpt-cf-keycloak-idp-plugin-design-keycloak-idp-plugin`

**Owners:** @platform-iam-team

**Scope:** Architecture of the Keycloak IdP plugin (crate `cf-gears-keycloak-idp-plugin`), which lives alongside this document at `gears/system/account-management/plugins/keycloak-idp-plugin`. The implementation is authoritative; this document describes what the code does, deliberately mirroring implementation-level constants and behaviors (an implementation-mirror altitude) — a change to those constants in code owns the matching edit here. In-code comments cite this document and the adjacent PRD by quoted section title (`DESIGN "Component Model"`, `PRD "Explicit Realm Binding"`), never by section number — titles survive renumbering, and the numbered citations the crate lineage arrived with referred to an ancestor specification that does not exist in this repo.

<!-- toc -->

- [1. Architecture Overview](#1-architecture-overview)
  - [1.1 Architectural Vision](#11-architectural-vision)
  - [1.2 Architecture Drivers](#12-architecture-drivers)
  - [1.3 Architecture Layers](#13-architecture-layers)
- [2. Principles & Constraints](#2-principles--constraints)
  - [2.1 Design Principles](#21-design-principles)
  - [2.2 Constraints](#22-constraints)
  - [2.3 Applicability Matrix](#23-applicability-matrix)
- [3. Technical Architecture](#3-technical-architecture)
  - [3.1 Domain Model](#31-domain-model)
  - [3.2 Component Model](#32-component-model)
  - [3.3 API Contracts](#33-api-contracts)
  - [3.4 Internal Dependencies](#34-internal-dependencies)
  - [3.5 External Dependencies](#35-external-dependencies)
  - [3.6 Interactions & Sequences](#36-interactions--sequences)
  - [3.7 Database schemas & tables](#37-database-schemas--tables)
- [4. Additional context](#4-additional-context)
  - [4.1 Security and Data Protection](#41-security-and-data-protection)
  - [4.2 Verification Architecture](#42-verification-architecture)
  - [4.3 Risks and Enablement Gates](#43-risks-and-enablement-gates)
  - [4.4 Observability](#44-observability)
- [5. Traceability](#5-traceability)
  - [5.1 Authoritative Contracts](#51-authoritative-contracts)
  - [5.2 P1 Requirement Allocation](#52-p1-requirement-allocation)

<!-- /toc -->

The adjacent [PRD](./PRD.md) is authoritative for WHAT, WHY, release priority, actors, and acceptance criteria. The Account Management SDK is authoritative for request, result, failure, filtering, ordering, and cursor contracts; the service-account SDK is authoritative for the machine-identity contract. This document defines the architecture of the shipped implementation and its safety invariants.

## 1. Architecture Overview

### 1.1 Architectural Vision

The Keycloak IdP plugin is a provider adapter running as an in-process ToolKit gear (`keycloak-idp-plugin`) in the host process next to Account Management. It implements two ClientHub contracts:

- `account_management_sdk::idp::IdpPluginClient` — tenant and user identity lifecycle, registered **scoped** under its GTS catalogue instance id so Account Management selects it by vendor/priority.
- the service-account half of `account_management_sdk::IdpPluginClient` — tenant-scoped machine identities (confidential OAuth `client_credentials` clients), served through the **same scoped registration** as tenant and user lifecycle rather than a second entry.

The plugin talks to the Keycloak Admin REST and OAuth token endpoints directly over HTTPS (reqwest transport). It authenticates with OAuth2 `client_credentials` using two administrator tiers: a **bootstrap admin** client (in the `master` realm by default, secret supplied via environment-expanded configuration) for realm-level work, and a per-realm **realm-admin** client for tenant/user work inside a bound realm. Realm-admin secrets are environment-backed for the default shared realm and Credential-Store-backed (OpenBao) for adopted and created realms; the plugin reads and — for realms it creates — writes those secrets itself through `credstore-sdk` under a plugin-owned system security context.

Three realm bindings are supported: `shared` (default; tenants share an operator-provisioned realm), `adopted` (an existing empty operator realm bound to one tenant), and `created` (the plugin creates and lifecycle-manages a dedicated realm — generated as `realm-{tenant_id}` for child tenants, while a root-target created realm requires an explicit operator-supplied name). Keycloak remains the user source of truth; the plugin persists nothing locally and returns a versioned opaque metadata envelope that Account Management stores and replays.

`update_user` is intentionally not implemented in this revision: the plugin inherits the SDK default, which returns `IdpUserOperationFailure::UnsupportedOperation`.

### 1.2 Architecture Drivers

| Driver | Design allocation |
|---|---|
| Provider-neutral publication and selection | `PluginV1<IdpPluginSpecV1>` published to types-registry (instance segment `cf.builtin.keycloak_idp.plugin.v1`; full catalogue id in §3.2, vendor `keycloak`, priority 50); scoped ClientHub registration keyed on the instance id; AM resolves via `choose_plugin_instance` against `idp.vendor` |
| Fail-fast configuration errors | Init validates typed config (`deny_unknown_fields`), the `secret_ref_template`, and the service-account section, then pre-warms the bootstrap admin token with a bounded retry budget; exhausting the budget fails `Gear::init` |
| Shared-realm tenant isolation | Per-tenant Keycloak group under `/tenants`, immutable `tenant_id` user attribute (ADR-0006), and an ownership marker attribute (`cf.provisioning.tenant_id`) on created realms and service-account clients |
| Safe external mutation | Parse-then-probe saga ordering, per-step reactive-401 wrapping, idempotent replay paths (probe-adopt realm ensure, 409 group reuse), and stage-attributed `AmbiguousCreated` classification with an `ambig:` prefix contract for reconciliation tooling |
| Secret custody | `SecretFromEnv`/`SecretString` wrappers with redacted `Debug`, token cache redaction, `redact_secrets` on every error body, and Credential Store writes only for plugin-created realm-admin secrets |
| Credential rotation without restart | Token cache keyed `(realm, client_id)` with single-flight refresh and an exactly-once reactive-401 retry that re-resolves the secret from its source (env or Credential Store) |
| User query correctness | Full tenant-group member drain (hard cap 10,000) sorted client-side on the caller's effective `order` (Account-Management default `username ASC, id ASC`), client-side typed filters, `CursorV1` continuation with an FNV-1a filter hash |
| Observability | Segregated metric ports over one OTel adapter (`keycloak_idp_plugin_*` instruments), realm-label cardinality cap, and structured audit events on the `keycloak_idp_plugin.events` tracing target |

#### NFR Allocation

| NFR | Architectural response | Release verification |
|---|---|---|
| Tenant isolation | Tenant group membership scopes listing; the `tenant_id` user attribute guards user deprovision; `cf.provisioning.tenant_id` markers guard realm and service-account ownership | Negative-isolation unit and integration tests |
| Secret non-disclosure | Typed secret wrappers, redacted token cache, `redact_secrets` + 2 KiB truncation on every provider error body before it crosses the AM boundary | Redaction unit tests; log/metric scanning |
| Failure classification | Closed `PluginError` taxonomy translated at the SDK boundary per fixed tables; `debug_assert` that provisioning never surfaces `MetadataDecode` | Exhaustive translation unit tests and failure injection |
| Availability recovery | Per-call pre-saga health probe; reactive-401 secret re-resolution; no cached provider state besides tokens; recovery needs no restart once init has succeeded | Dependency loss/recovery tests |
| Personal-data lifecycle | No local user directory; user deprovision deletes the provider identity; audit events carry provider IDs plus `username` only on `user.provisioned` | Hard-delete and output-minimization tests |
| Lifecycle latency | Bounded per-request HTTP timeout (5 s default), bounded retry policy, 30 s provision/deprovision saga timeout, list drain cap | Release qualification profile per PRD §6.1 |

#### Decision Provenance

The significant decisions inherit these accepted records and standards:

- [Separate Account Management from the IdP provider](../../../docs/ADR/0001-cpt-cf-account-management-adr-idp-contract-separation.md).
- [Keep the IdP as user source of truth](../../../docs/ADR/0005-cpt-cf-account-management-adr-idp-user-identity-source-of-truth.md).
- [Use provider-enforced tenant binding](../../../docs/ADR/0006-cpt-cf-account-management-adr-idp-user-tenant-binding.md) — implemented as the `tenant_id` user attribute plus tenant-group membership.
- Follow [ClientHub and plugin scoping](../../../../../../docs/toolkit_unified_system/03_clienthub_and_plugins.md), [lifecycle rules](../../../../../../docs/toolkit_unified_system/08_lifecycle_stateful_tasks.md), the [Architecture Manifest](../../../../../../docs/ARCHITECTURE_MANIFEST.md), and [Security Guidelines](../../../../../../guidelines/SECURITY.md).

Durable audit delivery remains owned by Account Management and the platform audit owner. The plugin's structured-event emitters (`keycloak_idp_plugin.events`) are an explicit development stand-in until the platform audit sink lands.

### 1.3 Architecture Layers

- [ ] `p3` - **ID**: `cpt-cf-keycloak-idp-plugin-tech-inprocess-gear-stack`

```mermaid
flowchart LR
  operator([Platform operator])
  iac[IaC and protected configuration]
  cred[(Credential Store / OpenBao)]
  kc[(Keycloak shared, adopted, and created realms)]
  authn[OIDC AuthN Resolver]
  subgraph host[Host process]
    am[Account Management]
    subgraph plugin[keycloak-idp-plugin gear]
      root[Plugin root: KeycloakIdpPlugin]
      facades[Tenant / User / Service-account facades]
      kcc[KC admin client factory + token cache]
      csrw[CredStore reader / writer]
      transport[Reqwest KC transport]
      obs[Metrics adapter + audit emitters]
    end
    tr[types-registry gear]
    cs[credstore gear]
  end

  operator --> iac
  iac -->|realms, bootstrap client, env secrets| kc
  am -->|tenant, user, and machine-identity lifecycle over one scoped IdpPluginClient| root
  root --> facades
  facades --> kcc
  kcc --> transport
  facades --> csrw
  csrw --> cs
  root -->|PluginV1 catalogue publish| tr
  cs --> cred
  transport -->|OAuth + Admin REST over HTTPS| kc
  authn -->|OIDC discovery and JWKS; no plugin call| kc
```

| Layer | Responsibility |
|---|---|
| Module wiring | Config probe/expansion, static validation, bootstrap token pre-warm, facade construction, types-registry publication, ClientHub registration |
| Plugin root | SDK trait implementations, metadata encode, `PluginError` → SDK failure translation |
| Domain facades | Tenant/user/service-account sagas, boundary enforcement, replay safety, error classification, audit emission |
| KC client layer (`domain/kc`) | Token acquisition and caching, single-flight refresh, reactive-401, admin REST helpers |
| Credential Store wrappers (`domain/credstore`) | System-context-bound read (secrets, CA bundle) and write (created-realm admin secrets) |
| Infrastructure (`infra`) | Reqwest transport with retry/backoff/Retry-After, TLS CA bundle wiring, OTel metrics adapter |

## 2. Principles & Constraints

### 2.1 Design Principles

#### Explicit provider intent

- [ ] `p3` - **ID**: `cpt-cf-keycloak-idp-plugin-principle-explicit-provider-intent`

Provisioning intent is parsed fail-closed (`deny_unknown_fields`); an unknown mode or contradictory realm intent is rejected before any provider call. Absent intent defaults to `shared`, where a child tenant inherits its parent's realm from replayed metadata rather than guessing.

#### Two-factor tenant binding

- [ ] `p3` - **ID**: `cpt-cf-keycloak-idp-plugin-principle-two-factor-tenant-binding`

Tenant-group membership scopes queries; the immutable `tenant_id` user attribute guards destructive user operations. A mismatch performs no mutation and follows non-disclosing absence semantics.

#### Operator-owned realms stay operator-owned

- [ ] `p3` - **ID**: `cpt-cf-keycloak-idp-plugin-principle-operator-owned-realm`

Shared and adopted realms are asserted, never created, repaired, or deleted. Only `created`-mode realms — marked with the plugin's ownership attribute — are created and, on last-tenant deprovisioning, deleted. A realm without the plugin's ownership marker is never touched.

#### Correctness before retry

- [ ] `p3` - **ID**: `cpt-cf-keycloak-idp-plugin-principle-correctness-before-retry`

Automatic HTTP retry splits by what is known. A retryable status (5xx/429) means Keycloak answered: bounded retry applies to all methods — including administrative `POST`s, whose replays converge through the idempotent-ensure and 409-reuse paths. A connect or timeout failure means the outcome is unknown: only idempotent requests (GET/PUT/DELETE and the token form-POST) are replayed, and administrative `POST`s never are. Reactive-401 re-mints exactly once because an auth rejection provably had no side effect. Uncertain created-mode work is stage-attributed `AmbiguousCreated`, never a clean failure.

#### SDK authority

- [ ] `p3` - **ID**: `cpt-cf-keycloak-idp-plugin-principle-sdk-authority`

DTOs, cursors, and failure vocabularies all come from `account-management-sdk`, machine identities included. The plugin adds no parallel public contract.

#### No silent success

- [ ] `p3` - **ID**: `cpt-cf-keycloak-idp-plugin-principle-no-silent-success`

Unsupported operations (`update_user`, unsupported filters) return typed `UnsupportedOperation` failures. Truncated list drains are logged loudly. Already-absent resources are explicit success-equivalents, not silent no-ops.

#### Least privilege by tier

- [ ] `p3` - **ID**: `cpt-cf-keycloak-idp-plugin-principle-least-privilege`

Realm-scoped work uses a per-realm admin client granted exactly seven `realm-management` roles (`view-realm`, `view-clients`, `query-groups`, `manage-realm`, `manage-users`, `query-users`, `manage-clients`). The bootstrap admin is used only where realm-level authority is required: realm existence probes, created-realm lifecycle, service-account client administration, and the tenant-group emptiness and sibling probes on adopted and created realms — which is why the bootstrap client also needs `query-groups` on those realms.

#### Privacy-preserving evidence

- [ ] `p3` - **ID**: `cpt-cf-keycloak-idp-plugin-principle-privacy-preserving-evidence`

Metrics labels are closed enums plus a cardinality-capped realm label; no metric carries a profile value or secret. Error bodies are redacted and truncated to 2 KiB before crossing the boundary. Audit events carry provider IDs; `user.provisioned` additionally records the username as operational evidence.

#### Durable audit stays upstream

- [ ] `p3` - **ID**: `cpt-cf-keycloak-idp-plugin-principle-durable-audit-intent`

The plugin returns one classified outcome per call and emits structured stand-in events; durable, idempotent audit persistence belongs to Account Management and the platform audit owner.

### 2.2 Constraints

#### Init-time provider dependency

- [ ] `p3` - **ID**: `cpt-cf-keycloak-idp-plugin-constraint-init-pre-warm`

When enabled, the plugin fails `Gear::init` if the bootstrap admin token cannot be acquired within the configured pre-warm budget (default 5 attempts × 3 s backoff, ≈37 s worst case including attempt time). Transient failures (5xx/429/transport/timeout, Credential Store readiness) are retried within the budget; permanent failures (non-429 4xx, malformed configuration) fail fast. Deployments that must start without Keycloak set `enabled: false`.

#### External consistency boundaries

- [ ] `p3` - **ID**: `cpt-cf-keycloak-idp-plugin-constraint-external-consistency-boundaries`

Keycloak and the Credential Store are external consistency boundaries. Plugin calls occur outside Account Management database transactions; the architecture does not claim atomic commit across them. Created-mode provisioning has an explicit point of no return: after Keycloak state exists, a failed Credential Store write is `AmbiguousCreated { OpenBaoPutAfterKcSuccess }`, never clean.

#### External durable audit prerequisite

- [ ] `p3` - **ID**: `cpt-cf-keycloak-idp-plugin-constraint-durable-audit-wrapper`

The plugin owns no audit table, recovery worker, relay, or sink integration. Its `tracing`-based event emitters are a development stand-in. Production requires the parent Account Management design and platform audit contract to guarantee durable call/outcome correlation.

#### Opaque metadata replay

- [ ] `p3` - **ID**: `cpt-cf-keycloak-idp-plugin-constraint-opaque-metadata-replay`

Account Management persists the plugin's metadata envelope as opaque JSON and replays it on later calls. Decoding is fail-closed: a missing `version`, a version other than `"v1"`, or a malformed shape yields a typed failure without provider access. The envelope carries no secrets — only routing identifiers and a Credential Store reference name.

#### No plugin persistence

- [ ] `p3` - **ID**: `cpt-cf-keycloak-idp-plugin-constraint-no-plugin-persistence`

The plugin owns no database table and no persistent user cache. Its only in-memory state is the admin token cache and metrics cardinality tracking.

#### Scoped provider selection

- [ ] `p3` - **ID**: `cpt-cf-keycloak-idp-plugin-constraint-scoped-provider-selection`

Account Management selects the plugin through the types-registry catalogue instance and scoped ClientHub resolution. On restart the plugin re-registers idempotently: an `AlreadyExists` catalogue answer is accepted only when the stored spec is structurally equal JSON to the current registration (key order and formatting are irrelevant; any field-value difference fails init).

#### Bounded offline-token exposure

- [ ] `p3` - **ID**: `cpt-cf-keycloak-idp-plugin-constraint-bounded-offline-token-exposure`

A previously issued access JWT can remain valid until `exp`. The plugin revokes sessions on user deprovisioning (best-effort, configurable) but never claims real-time token invalidation; bounding exposure is an operator realm-policy obligation (15-minute access-token lifetime per the PRD).

#### Bounded query scan

- [ ] `p3` - **ID**: `cpt-cf-keycloak-idp-plugin-constraint-correct-global-query-ordering`

Keycloak does not guarantee stable ordering across group-member offset pages, so `list_users` drains the entire tenant-group membership per request (pages of `list_users_page_limit_max`, default 200), sorts globally on the caller's effective `order`, and filters client-side. Sorting the full drained set in memory is what makes arbitrary caller orderings affordable: honoring `order` is a comparator choice, not an extra provider round-trip. The drain is bounded by a 10,000-member hard cap: it stops after the first page that reaches the cap (bounded overshoot of at most one fetch page) and logs a loud truncation warning. Page size to the caller is capped at 200.

### 2.3 Applicability Matrix

| Checklist domain | Disposition |
|---|---|
| Architecture and semantic alignment | Applicable; addressed in §§1–3 and §5. |
| Performance and capacity | Applicable; query scan bounds, timeouts, and retry budgets are addressed in §§2.2 and 3.6. |
| Security and compliance controls | Applicable; trust boundaries and controls are addressed in §4.1. |
| Reliability | Applicable; mutation uncertainty, replay, timeout, and recovery are addressed in §3.6. |
| Data | Applicable only to ownership and lifecycle because the plugin owns no database; addressed in §§3.1, 3.7, and 4.1. |
| Integration | Applicable; Account Management, Keycloak, Credential Store, types-registry, and ClientHub boundaries are addressed in §§3.2–3.6. |
| Operations | Applicable; configuration, observability, and enablement gates are addressed in §§3.2, 3.6, and 4.3. |
| Maintainability | Applicable; SDK authority, component boundaries, and decision provenance are addressed in §§1.2, 2.1, and 3.2–3.4. |
| Testing | Applicable; verification architecture is addressed in §4.2. |
| Usability | Not applicable because the plugin exposes no human interface. |
| Accessibility | Not applicable because the plugin exposes no visual or interactive user interface. |
| Business and time-to-market | Not applicable; owned by the adjacent PRD and planning artifacts. |
| Plugin-specific cost budget | Not applicable because the plugin adds no independently deployed service or plugin-owned store. |
| Documentation quality | Applicable; authoritative references and traceability are provided in §5. |

## 3. Technical Architecture

### 3.1 Domain Model

#### Tenant IdP metadata envelope

- [ ] `p3` - **ID**: `cpt-cf-keycloak-idp-plugin-entity-tenant-idp-metadata`

The provider metadata is the versioned, non-secret routing envelope `TenantIdpMetadataV1`, whose Rust type and serde shape are owned by the plugin's metadata codec and persisted opaquely by Account Management:

| Field | Meaning |
|---|---|
| `version` | Envelope version; `"v1"` at this revision. Missing → `MissingVersion`; other values → `UnsupportedVersion`; shape mismatch → `Malformed`. All three fail closed before provider access. |
| `realm_name` | Realm the tenant is bound to. |
| `realm_binding` | `shared` \| `adopted` \| `created` (also a public metric label). |
| `tenant_group_id` | Keycloak UUID of the plugin-owned per-tenant group. |
| `admin_secret_ref` | `None` for the env-backed default shared realm; `Some(ref)` naming the Credential Store reference for adopted/created realms (template `keycloak-idp-realm-admin-{realm_name}-secret` by default). |
| `admin_client_id` | OAuth2 `client_id` of the per-realm admin client (default `keycloak-idp-plugin-realm-admin`). |

Provider ownership is split as follows:

| Resource | Owner | Plugin authority |
|---|---|---|
| Shared and adopted realms, their authentication profile, protocol mappers, and the bootstrap/realm-admin clients in them | Platform operator (IaC / realm bootstrap) | Assert existence (and emptiness for adopted); use the configured admin clients |
| Created realms (child tenants: generated `realm-{tenant_id}`; root target: operator-named; all marked `cf.provisioning.tenant_id`) | Plugin | Create, administer, and delete on last-tenant deprovisioning |
| Realm-admin client and its 7 `realm-management` roles in created realms | Plugin | Create and grant during created-mode provisioning |
| Per-tenant group under `/tenants` and the `tenant_id`/`user_type` user attributes | Plugin | Create, inspect, delete for the owning tenant |
| Human identities and sessions in the tenant boundary | Plugin through Account Management | Create, delete, revoke, and query after boundary verification; updates are unsupported in this revision |
| Service-account clients `svc-{tenant_id}-{name}` and their service-account users | Plugin through the service-account half of `IdpPluginClient` | Create, rotate, revoke, list, and purge on tenant deprovisioning |
| Created-realm admin secret in the Credential Store | Plugin (system actor) | Write on provisioning, read on later calls, delete on realm teardown |
| Shared/adopted realm-admin secrets | Platform operator | Read only (env expansion or Credential Store reference) |
| Opaque provider metadata row | Account Management | Persist/replay only; plugin owns interpretation |
| Access-token validation | OIDC AuthN Resolver | No call to this plugin |

### 3.2 Component Model

#### Keycloak IdP plugin

- [ ] `p3` - **ID**: `cpt-cf-keycloak-idp-plugin-component-keycloak-idp-plugin`

The plugin is one logical architecture component. The following entries are responsibility partitions inside that component (concrete Rust modules), not independently deployed services.

| Responsibility partition | Architectural responsibility |
|---|---|
| Module wiring | Enabled probe, typed config expansion, static validation, bootstrap pre-warm, DI, catalogue publish, ClientHub registration |
| Plugin root | SDK trait surface; failure translation tables |
| Tenant lifecycle coordinator | Provision/deprovision sagas for all three bindings, saga timeout, idempotent replay, ambiguity staging, service-account purge hook |
| User lifecycle coordinator | User provision/deprovision/list, tenant boundary guard, orphan compensation, cursor pagination |
| Service-account facade | Machine-identity lifecycle against the configured service-account realm (`service_account.realm`, default `platform`); ownership markers; quota and scope allowlist |
| Metadata codec | Versioned fail-closed envelope encode/decode |
| KC admin client factory | `client_credentials` token acquisition, `(realm, client_id)` token cache with single-flight refresh, reactive-401, health probe |
| Credential Store wrappers | System-ctx-bound reader (secrets, CA bundle) and writer (create-or-replace, delete with absent-as-success) |
| Transport | Reqwest HTTPS client, per-request timeout, bounded retry with exponential backoff + full jitter + `Retry-After`, W3C trace propagation, body redaction/truncation |
| Observability | One OTel adapter behind seven segregated metric ports (instrument catalogue in §4.4); structured audit emitters |
| System actor | Stable plugin service-account identity for Credential Store calls |

#### Deployment, Publication, and Readiness

The plugin is an in-process gear (gear name `keycloak-idp-plugin`, with declared gear dependencies on `types-registry` and `credstore`) in each host replica. It opens no listener and owns no independent deployment or database.

Initialization proceeds in phases:

1. **Enabled probe** — `enabled` (default `true`) is read leniently; a disabled plugin skips everything else and registers nothing.
2. **Typed config expansion** — `${VAR}` substitution over the marked secret fields; unknown keys are rejected.
3. **Static validation** — the `secret_ref_template` is validated by interpolating a 64-character synthetic realm name through `SecretRef::new`; the service-account section is validated.
4. **Transport construction** — the reqwest client is built; if `tls_ca_bundle_ref` is set, the PEM bundle is read from the Credential Store and added as a root certificate.
5. **Bootstrap pre-warm** — the bootstrap admin token is acquired with bounded retry (default 5 × 3 s). Retryable causes: 5xx/429/transport/timeout and Credential Store read failures (deploy-ordering races). Permanent causes fail on the first attempt. Budget exhaustion fails init.
6. **Catalogue publish** — `PluginV1<IdpPluginSpecV1>` is registered with types-registry under the full catalogue instance id `gts.cf.toolkit.plugins.plugin.v1~cf.core.idp.plugin.v1~cf.builtin.keycloak_idp.plugin.v1` (base plugin type `gts.cf.toolkit.plugins.plugin.v1~`, derived IdP spec type `…~cf.core.idp.plugin.v1~`, instance segment `cf.builtin.keycloak_idp.plugin.v1`), carrying the configured vendor/priority. `AlreadyExists` is accepted only when the stored spec is structurally equal JSON to the current registration.
7. **ClientHub registration** — one `Arc<dyn IdpPluginClient>`, scoped by the catalogue instance id, carrying the tenant, user, and machine-identity halves together.

After init, readiness is operation-based: every tenant provision/deprovision saga begins with an unauthenticated Keycloak health probe (`GET realms/master`), and every call re-resolves credentials on 401. Recovery from provider or Credential Store outages therefore needs no process restart.

### 3.3 API Contracts

The [`idp.rs`](../../../account-management-sdk/src/idp.rs), [`idp_user.rs`](../../../account-management-sdk/src/idp_user.rs), and [`idp_service_account.rs`](../../../account-management-sdk/src/idp_service_account.rs) contracts are authoritative for tenant, user, and machine-identity work respectively — all three halves of one trait. This boundary realizes the PRD interface contracts `cpt-cf-keycloak-idp-plugin-interface-idp-plugin-client`, `cpt-cf-keycloak-idp-plugin-interface-service-account-client`, and `cpt-cf-keycloak-idp-plugin-interface-provider-instance`, under the external contracts `cpt-cf-keycloak-idp-plugin-contract-account-management`, `cpt-cf-keycloak-idp-plugin-contract-keycloak-admin`, and `cpt-cf-keycloak-idp-plugin-contract-credstore`.

Contract invariants implemented at the plugin's SDK boundary:

- malformed or contradictory provisioning intent (`ProvisionInputRejected`, metadata decode failures) maps to `IdpProvisionFailure::InvalidInput` with `field = "provisioning_metadata"`, before provider mutation;
- a foreign or unmarked existing realm in `created` mode is `CleanFailure` (no state was created); permanent provider 4xx and pre-saga failures are `CleanFailure`; so is any 5xx, transport, or timeout failure raised before the saga attempts its first mutation — the health probe and the shared/adopted existence checks are reads, so their outcome may be unknown while the provider is provably untouched;
- provider 5xx, transport/timeout, saga timeout, and staged `AmbiguousCreated` map to `IdpProvisionFailure::Ambiguous` for the provisioning reaper **once a mutation has been attempted and follow-up evidence cannot rule out retained state**; realm creation is the explicit reconciliation case: a post-failure 404 proves the realm was not retained and maps to `CleanFailure`, while an unsuccessful re-probe preserves `Ambiguous` — retained-state evidence, not the error class alone, separates this row from the one above;
- missing bootstrap permissions (`BootstrapPermsMissing`) map to `UnsupportedOperation` with an operator-runbook detail;
- tenant deprovisioning maps `DeprovisionNotFound` → `NotFound` (success-equivalent), `DeprovisionRetryable` → `Retryable`, and everything else — including metadata decode failures — → `Terminal`;
- user failures use only `IdpUserOperationFailure` variants: KC 409 on create is `DuplicateUser` with the field refined from the provider message (`Username`/`Email`/`UsernameOrEmail`), password-policy rejections are `PasswordPolicy`, unsupported filters and the unimplemented `update_user` are `UnsupportedOperation`, provider/transport trouble is `Unavailable`;
- service-account failures use the `IdpServiceAccountFailure` set: `InvalidInput` (bad name, disallowed scope, quota, taken client id), `NotFound` (absent or foreign account — indistinguishable by design), `Ambiguous` (stage-attributed transport uncertainty; stage tokens tabulated in §3.6), `CleanFailure` (pre-mutation failures). The taxonomy also carries `UnsupportedOperation`, which this plugin never returns for these methods because it implements them - it is the category an adapter that ships only the tenant and user halves surfaces through the contract's default implementations;
- every mutating return carries redacted, truncated, grep-friendly detail: provider failures use `kc:{METHOD} {path_template} -> {status}: {body≤2KiB}`, internal component failures `internal:{component}: …`, and classified domain rejections a stable variant prefix (`ambig:{stage}: …`, `deprovision retryable: …`, `sa invalid input: …`, and so on).

The plugin emits no public HTTP API.

### 3.4 Internal Dependencies

Domain components depend on plugin-owned ports rather than transport types: `reqwest` is confined to the transport adapter behind the `KcTransport` trait, and metrics are consumed through seven segregated port traits implemented by one adapter.

| Internal dependency | Purpose |
|---|---|
| Account Management SDK | Provider-neutral trait, DTO, failure, and pagination authority |
| Service-account SDK | Machine-identity trait, models, and failure taxonomy |
| ToolKit and ClientHub | Gear lifecycle, config expansion, scoped publication and dependency resolution |
| types-registry SDK + gear | `PluginV1` catalogue publication (hard init prerequisite) |
| credstore SDK + gear | Secret read/write under the plugin system actor |
| `toolkit-odata` | `CursorV1` continuation-token envelope |

### 3.5 External Dependencies

| Dependency | Contract and failure boundary |
|---|---|
| Account Management | Authorizes calls, selects the scoped plugin, persists/replays opaque metadata, consumes typed outcomes, owns durable audit |
| Keycloak | User/session/group/client source of truth; Admin REST + OAuth token endpoints reached directly over HTTPS; per-request timeout 5 s (default), bounded retry for idempotent requests |
| Credential Store (OpenBao-backed) | Custody of adopted/created realm-admin secrets and the optional TLS CA bundle; written by the plugin only for created realms; unavailability blocks affected operations and init when the CA bundle or pre-warm needs it |
| Operator IaC / realm bootstrap | Provides shared/adopted realms, their authentication profile and protocol mappers (including the `tenant_id`/`user_type` usermodel-attribute mappers), the bootstrap admin client and its secret, shared-realm admin client and secret, adopted-realm secrets under the template reference, and realm-default client scopes for service accounts |
| OIDC AuthN Resolver | Independently validates tokens; consumes the `user_type`/`tenant_id` claims the realm mappers project; the two components never call each other |
| Account Management, machine-identity surface | Resolves the same scoped `IdpPluginClient` and authorizes each verb against the service-account resource type before delegating. The contract, the consumer, and this plugin's implementation all ship |

### 3.6 Interactions & Sequences

#### Realm Binding and Tenant Provisioning

**ID**: `cpt-cf-keycloak-idp-plugin-seq-tenant-provisioning`

**Use cases**: `cpt-cf-keycloak-idp-plugin-usecase-bind-shared-tenant`, `cpt-cf-keycloak-idp-plugin-usecase-adopt-tenant-realm`, `cpt-cf-keycloak-idp-plugin-usecase-create-tenant-realm` — **Actors**: `cpt-cf-keycloak-idp-plugin-actor-account-management`, `cpt-cf-keycloak-idp-plugin-actor-keycloak`, `cpt-cf-keycloak-idp-plugin-actor-credstore`, `cpt-cf-keycloak-idp-plugin-actor-platform-operator`

`provision_tenant` runs under a 30 s (configurable) timeout; a timeout is `AmbiguousCreated { Timeout }` once the saga has attempted a mutation, and a clean failure if it expires while the saga is still in its read-only prologue. The saga parses intent first (fail-fast, no provider round-trip), then health-probes Keycloak (failure = clean `Config` error — nothing mutated), then dispatches on the binding:

- **Shared** (default; child tenants inherit the parent's realm from replayed parent metadata; root bootstrap must name the realm): assert the realm exists (bootstrap admin), then under the env-backed realm-admin ensure the per-tenant group `/tenants/{tenant_id}` and optionally bind the operator-declared admin user's `tenant_id`/`user_type` attributes (idempotent; a foreign or multi-valued existing binding is rejected as a hijack attempt).
- **Adopted**: assert the realm exists and is empty of tenant boundaries (no subgroups under the group root), then ensure the tenant group using the operator-provisioned Credential Store secret reference.
- **Created** (child tenants use the generated name `realm-{tenant_id}` and reject an explicit one; a root-target created realm requires an explicit operator-supplied name): idempotent realm ensure (probe → adopt if marked as ours → reject foreign → create with the ownership attribute and a fail-closed allowlist over operator `realm_defaults`: `displayNameHtml`, `defaultLocale`, `supportedLocales`, `internationalizationEnabled`), unconditional bootstrap-token invalidation after realm ensure — whether the realm was just created or idempotently adopted — so the next bootstrap call carries the new realm's auto-granted role, realm-admin client creation, grant of the seven `realm-management` roles, secret read-back, Credential Store write (point of no return — failure is `AmbiguousCreated { OpenBaoPutAfterKcSuccess }`), and tenant-group creation under the new realm-admin.

```mermaid
sequenceDiagram
  participant P as Plugin (created-mode saga)
  participant K as Keycloak
  participant CS as Credential Store

  P->>K: Probe realm (adopt if ours / reject foreign)
  P->>K: Create realm + ownership marker [ambig:kc_realm_create]
  P-->>P: Invalidate bootstrap token (unconditional)
  P->>K: Create realm-admin client [ambig:kc_client_create]
  P->>K: Grant 7 realm-management roles [ambig:kc_role_mapping]
  P->>K: Read generated client secret [ambig:kc_client_secret_read]
  P->>CS: Store secret under templated ref [ambig:openbao_put_after_kc_success]
  P->>K: Ensure tenant group (new realm-admin)
```

Every step that can fail after provider state exists carries a stage token that reconciliation tooling parses by its `ambig:` prefix. The complete stage contract (one row per `AmbiguousStage` variant):

| Stage token | Emitting saga |
|---|---|
| `ambig:kc_realm_create` | Created-mode realm creation |
| `ambig:kc_client_create` | Created-mode realm-admin client creation; service-account client creation and post-create lookup |
| `ambig:kc_role_mapping` | Created-mode `realm-management` role grants |
| `ambig:kc_client_secret_read` | Created-mode realm-admin secret read-back; service-account secret read at creation |
| `ambig:openbao_put_after_kc_success` | Created-mode Credential Store write after Keycloak state exists |
| `ambig:admin_user_bind` | Shared-mode admin-user attribute bind |
| `ambig:timeout` | Provisioning saga timeout |
| `ambig:kc_sa_attr_set` | Service-account service-account attribute bind after client creation |
| `ambig:kc_client_scope_attach` | Service-account client-scope attachment |
| `ambig:kc_client_secret_rotate` | Service-account secret rotation |
| `ambig:kc_client_delete` | Service-account revocation and tenant-deprovision purge |

Success returns the metadata envelope, records `provision_tenant_duration` and `realms_bound`, and emits `tenant.bound` (plus `realm.created` and/or `admin_user.bound` where applicable).

#### User Mutation Safety

**ID**: `cpt-cf-keycloak-idp-plugin-seq-user-mutation`

**Use cases**: user provisioning/deprovisioning are exercised through the parent Account Management use cases; `cpt-cf-keycloak-idp-plugin-usecase-update-user` is p2 — **Actors**: `cpt-cf-keycloak-idp-plugin-actor-tenant-admin`, `cpt-cf-keycloak-idp-plugin-actor-account-management`, `cpt-cf-keycloak-idp-plugin-actor-keycloak`

User operations resolve realm, admin client, and tenant group from replayed metadata and run each Keycloak step under an exactly-once reactive-401 wrapper.

*Provisioning* creates the user with `enabled = true`, the immutable `tenant_id` and `user_type = "user"` attributes, configured required actions (default `VERIFY_EMAIL`; `UPDATE_PASSWORD` added for temporary passwords), `emailVerified` derived as the complement of `VERIFY_EMAIL`, and an optional embedded initial password; then resolves the created UUID by exact username lookup and joins the user to the tenant group. A failed group join triggers best-effort orphan compensation (delete the just-created user) with a dedicated outcome metric; the original error is always the one returned. Duplicate detection: any 409 from user-create is `DuplicateUser`; the offending field is refined from the provider error message when possible.

*Deprovisioning* first reads the target user's stored `tenant_id` attribute; a mismatch against the request tenant performs no mutation and returns success-equivalently (non-disclosing, per ADR-0006 — this attribute check is the cross-tenant deletion guard). It then best-effort revokes sessions (configurable, default on) and deletes the identity; 404/410 are success-equivalent.

*Updates* are not implemented: the SDK default returns `UnsupportedOperation`.

#### Tenant Deprovisioning

**ID**: `cpt-cf-keycloak-idp-plugin-seq-hard-tenant-deprovisioning`

**Use cases**: `cpt-cf-keycloak-idp-plugin-usecase-retire-tenant` — **Actors**: `cpt-cf-keycloak-idp-plugin-actor-account-management`, `cpt-cf-keycloak-idp-plugin-actor-keycloak`, `cpt-cf-keycloak-idp-plugin-actor-credstore`, `cpt-cf-keycloak-idp-plugin-actor-service-account-consumer`

Ordering: local metadata resolution → service-account purge → tenant-group deletion → (created-mode only) last-tenant realm teardown → Credential Store secret deletion.

- Missing metadata returns `NotFound` (success-equivalent) with an `already_absent` audit event and a trigger metric when invoked by the AM system actor. A blob that decodes to no metadata is also `NotFound` but records only the failure counter (the branch is unreachable in practice). A malformed blob is `Terminal`.
- A pre-saga health-probe failure is `Retryable` — nothing was attempted.
- The service-account purge deletes every `svc-{tenant_id}-*` client owned by the tenant (ownership marker checked client-side); purge failures classify to `Retryable` (retryable provider status or staged ambiguity) or `Terminal` (permanent provider rejection) and abort the saga so no live machine credential survives its tenant.
- Tenant-group deletion treats 404/410 as `NotFound`; other failures are `Retryable`.
- For created bindings only, if no sibling tenant groups remain the realm itself is deleted (404/410 tolerated), then the realm-admin secret is deleted from the Credential Store (absent-as-success). The realm-admin client is not deleted separately — it dies with the realm.
- Shared and adopted realms are never deleted, and per-user deletion is not part of tenant deprovisioning: Account Management's hard-delete pipeline deprovisions users through `deprovision_user` before retiring the tenant boundary.

The whole saga shares the 30 s timeout (timeout → `Retryable`).

#### User Query Architecture

**ID**: `cpt-cf-keycloak-idp-plugin-seq-user-query`

**Use cases**: tenant-scoped user queries back the parent Account Management listing surfaces — **Actors**: `cpt-cf-keycloak-idp-plugin-actor-tenant-admin`, `cpt-cf-keycloak-idp-plugin-actor-account-management`, `cpt-cf-keycloak-idp-plugin-actor-keycloak`

`IdpListUsersRequest` is authoritative. Because Keycloak documents no stable global order for group-member offset pages, each list call drains the full tenant-group membership (pages of `list_users_page_limit_max`, default 200) into memory, bounded by a 10,000-member hard cap (the drain stops after the first page that reaches the cap — overshoot of at most one fetch page — and logs a loud truncation warning), then sorts on the effective `order`, deduplicates by id, applies the cursor skip, applies filters, and pages.

Supported filters, lowered client-side: `eq` (exact) and `contains` (case-insensitive) over `username`, `email`, `first_name`, `last_name`, `display_name`; `id eq` as a point filter; `and`/`or` composition. Everything else (`ne`, ranges, `in`, `not`, `id eq` inside `or`) returns `UnsupportedOperation` before any provider call.

Ordering is honored, not fixed. The effective order is `IdpListUsersRequest.order`; Account Management injects the default `username ASC` and appends an `id ASC` tiebreaker before SPI dispatch, so the plugin receives a totally ordered, already-validated key list on every call. Absent `order` (a direct SPI caller bypassing Account Management) falls back to `username ASC, id ASC`. Each key projects to a comparable string — `id` via its canonical text form, absent `Option<String>` fields as the empty string, so absent sorts ahead of any non-empty value under `ASC` — and keys compare in sequence with per-key direction applied, matching `static-idp-plugin`'s `compare_by_order` semantics so the two providers order identically. Order keys are restricted to the `IdpUserFilterField` set (`id`, `username`, `email`, `display_name`, `first_name`, `last_name`); Account Management's REST layer already rejects anything else with `400`, and a key outside the set arriving over the SPI returns `UnsupportedOperation` before any provider call. Note that `created_at` is deliberately not an order key: it is absent from `IdpUserFilterField`, so no caller can express it and no cursor may pin it.

The continuation token is a `CursorV1` (the same envelope Account Management uses elsewhere) pinning the effective order as signed tokens, forward-only direction, the last emitted key tuple projected under that order, and an FNV-1a hash folding tenant, realm, and the complete filter tree — changing the order or the filter mid-walk invalidates the cursor. Account Management cooperates with this: on a continuation request its REST extractor rejects `cursor + $orderby`, so it recovers the order from the cursor's signed tokens and re-forwards it rather than falling back to the default, which would otherwise present as an order mismatch at the plugin's cursor-validation step. A one-release legacy cursor fallback covers rolling deploys. Page size is `min(requested top, 200)`.

#### Failure, Retry, and Reconciliation

**ID**: `cpt-cf-keycloak-idp-plugin-seq-reconciliation`

**Use cases**: `cpt-cf-keycloak-idp-plugin-usecase-reconcile-ambiguous-mutation` — **Actors**: `cpt-cf-keycloak-idp-plugin-actor-platform-operator`, `cpt-cf-keycloak-idp-plugin-actor-account-management`, `cpt-cf-keycloak-idp-plugin-actor-keycloak`, `cpt-cf-keycloak-idp-plugin-actor-credstore`

HTTP-level retry is bounded (default 3 retries, exponential backoff 100 ms → 5 s cap, full jitter, `Retry-After` honored in delta-seconds form) and splits by what is known: retryable statuses (5xx/429) — where Keycloak answered — are retried for all methods, including administrative POSTs (create-path replays converge through the idempotent-ensure and 409-reuse paths); connect/timeout errors — where the outcome is unknown — are retried only for idempotent requests (GET/PUT/DELETE and the token form-POST), never for administrative POSTs. Reactive-401 re-mints credentials exactly once per call.

Classification is closed: internal `PluginError` variants translate to SDK failures per the fixed tables in §3.3, and each variant has a stable metric label on `keycloak_idp_plugin_failure_total`. Ambiguous created-mode provisioning remains operator-owned: terminal evidence carries the `ambig:{stage}` token, the realm, and redacted provider detail. Production enablement requires a reconciliation runbook keyed on those stage tokens.

#### Credentials, Token Cache, and Rotation

**ID**: `cpt-cf-keycloak-idp-plugin-seq-credential-resolution`

**Use cases**: credential handling underpins every mutating use case above — **Actors**: `cpt-cf-keycloak-idp-plugin-actor-platform-operator`, `cpt-cf-keycloak-idp-plugin-actor-keycloak`, `cpt-cf-keycloak-idp-plugin-actor-credstore`

All Keycloak traffic authenticates with OAuth2 `client_credentials`. Secrets resolve by tier:

| Tier | Client | Secret source |
|---|---|---|
| Bootstrap admin | `keycloak-idp-plugin-bootstrap` in `master` (defaults) | `${VAR}`-expanded configuration (`SecretFromEnv`) |
| Shared-realm admin | `keycloak-idp-plugin-realm-admin` (default) | `${VAR}`-expanded configuration (`default_shared_realm_secret`) |
| Adopted/created-realm admin | `admin_client_id` from metadata | Credential Store reference from `secret_ref_template` (created-mode secrets are written by the plugin during provisioning) |

Service-account administration always uses the bootstrap admin tier, against the dedicated service-account realm (`service_account.realm`, default `platform` — the shared realm whose issuer tenants trust), independent of any tenant's realm binding.

Tokens are cached per `(realm, client_id)` with a configurable expiry safety margin (default 30 s), a 5 s floor, and a defensive 24 h ceiling on the resulting cache TTL. The ceiling treats provider-reported `expires_in` as untrusted, prevents monotonic-clock overflow, and only refreshes early when Keycloak reports an implausibly long lifetime. The cache uses borrowed realm/client lookups on its warm path, single-flight refresh (one concurrent mint per key), and read-time eviction. A 401 on any wrapped call invalidates the cache entry, re-resolves the secret from its source, and retries exactly once — so operator secret rotation converges without restart. Cache hits emit no metrics; real refreshes emit `credential_refresh_total` and `kc_admin_token_refresh_total` separately so "Credential Store unreachable" and "Keycloak rejected the secret" are distinguishable.

Optional TLS: `tls_ca_bundle_ref` names a Credential Store secret holding a PEM CA bundle added to the reqwest trust roots at init; otherwise the system trust store applies.

#### Audit and Metrics Boundary

**ID**: `cpt-cf-keycloak-idp-plugin-seq-terminal-outcome`

**Use cases**: audit/metrics evidence underpins `cpt-cf-keycloak-idp-plugin-usecase-reconcile-ambiguous-mutation` and the durable-audit handoff — **Actors**: `cpt-cf-keycloak-idp-plugin-actor-account-management`, `cpt-cf-keycloak-idp-plugin-actor-platform-operator`

```mermaid
sequenceDiagram
  participant AM as Account Management
  participant P as Keycloak plugin
  participant K as Keycloak
  participant A as Platform audit infrastructure

  AM->>P: Invoke mutating contract
  P->>K: Admin REST (bearer, HTTPS)
  P-->>P: tracing event on keycloak_idp_plugin.events
  P-->>AM: Classified redacted outcome
  AM->>A: Durably correlate terminal outcome
```

The plugin emits structured events on the `keycloak_idp_plugin.events` tracing target — `tenant.bound`, `realm.created`, `admin_user.bound`, `tenant.unbound`, `realm.removed`, `service_accounts.purged`, `user.provisioned` (includes `username`), `user.deprovisioned` — each carrying the acting subject (id, closed-set type, raw type, tenant). `user.deprovisioned` is emitted on every successful `deprovision_user` return, including the non-mutating cross-tenant-guard and already-absent paths, and its `already_absent` field is currently always `false`. This is an explicit development stand-in until the platform audit sink lands; durable one-outcome-per-call correlation is owned by Account Management and the platform audit owner. `list_users` and the service-account read/mutate paths emit no plugin audit events of their own; the only service-account audit signal is `service_accounts.purged` on tenant deprovisioning.

Metrics: `keycloak_idp_plugin_provision_tenant_duration_seconds`, `user_op_duration_seconds`, `sa_op_duration_seconds`, `kc_admin_request_duration_seconds` (declared, not yet wired), `failure_total{op,failure_variant}`, `kc_admin_token_refresh_total` and `credential_refresh_total` (`{outcome,tier,realm}`), `credstore_write_total`, `metadata_decode_failure_total{version_observed}`, `deprovision_missing_metadata_total`, `orphan_user_compensation_total`, `realms_bound{realm_binding,realm_name}`. Every label is a closed enum except `version_observed` (an uncapped observed version string) and `realm`/`realm_name`, which share a cardinality budget (default 500 distinct realms) after which the label is dropped and a warning is logged on every observation of an over-cap realm.

Operational signals: the reconciliation gate consumes `failure_total{failure_variant="ambiguous_created"}` and the `ambig:{stage}` warn events; dependency-health attribution splits `credential_refresh_total{outcome="error"}` (Credential Store) from `kc_admin_token_refresh_total{outcome="error"}` (Keycloak) plus `credstore_write_total{outcome="error"}`; metadata integrity consumes `metadata_decode_failure_total` (any increase is operator-actionable); list capacity consumes the drain-cap truncation warning. Alert thresholds against the PRD availability and latency targets are owned by the deployment repository's monitoring configuration, not by this design.

#### Lifecycle and Shutdown

**ID**: `cpt-cf-keycloak-idp-plugin-seq-lifecycle-shutdown`

**Use cases**: none (host lifecycle behavior) — **Actors**: `cpt-cf-keycloak-idp-plugin-actor-platform-operator`

The plugin owns no periodic, detached, or audit-delivery task; all work is call-scoped and bounded by the per-request HTTP timeout, the bounded retry policy, and the saga timeout. Host shutdown therefore has nothing plugin-specific to drain beyond in-flight calls, which preserve their normal classification and reconciliation evidence.

### 3.7 Database schemas & tables

Not applicable: the plugin owns no database schema, table, migration, audit store, or persistent user index. Account Management owns its opaque provider-metadata row; Keycloak owns identity state; the Credential Store owns secret persistence.

## 4. Additional context

### 4.1 Security and Data Protection

#### Trust Boundaries

| Boundary | Control |
|---|---|
| Account Management caller to plugin | AM authorizes; the forwarded `SecurityContext` identifies the actor for audit but never substitutes for provider tenant checks |
| Service-account consumer to plugin | Trusted platform modules only (in-process ClientHub); caller RBAC/PDP authorizes before delegation; `ctx` is audit-only |
| Plugin to Keycloak | HTTPS with optional operator CA bundle; bearer tokens minted per admin tier; bounded bodies on error paths; no caller-controlled URL — paths are built from configuration and validated identifiers |
| Plugin to Credential Store | Plugin-owned system actor (stable service-account UUID, platform tenant, wildcard token scopes) authorized through ordinary RBAC grants — no PEP bypass; reader and writer are separate types so read-only sites cannot mutate |
| Tenant A to tenant B | Group-scoped listing; `tenant_id`-attribute guard on user deletion; ownership-marker guard on realms and service-account clients; foreign resources are reported as absent, not as denied |
| Plugin to metrics/logs | Closed-enum labels, capped realm label, `redact_secrets` (bearer and key-value secret patterns) plus 2 KiB truncation on every provider body |

Secret custody: the bootstrap and shared-realm admin secrets enter the process through `${VAR}` config expansion into redacted wrapper types; adopted/created realm secrets and the optional CA bundle live in the Credential Store; created-realm secrets are generated by Keycloak, read back once per provisioning, and stored under the templated reference with tenant sharing. Cached tokens are `SecretString`s with redacted debug output. Metadata envelopes carry reference names, never values.

The plugin's system actor identity (`KEYCLOAK_IDP_PLUGIN_ACTOR_UUID`) must remain stable across releases — created-realm secret ownership in the Credential Store is keyed to it.

Keycloak is the sole user directory. The plugin holds request data only for the operation lifetime. Hard user deprovisioning deletes the provider identity; created-realm teardown removes the realm and its secret. Audit events use provider IDs, with `username` additionally present on `user.provisioned`.

### 4.2 Verification Architecture

| Level | Purpose |
|---|---|
| Unit (in-crate, 300+ tests) | Config parsing/defaults/validation, metadata codec compatibility, error classification and translation tables, redaction, cursor encode/decode incl. legacy fallback and order-token round-trip, filter lowering, order-key projection/comparison (per-key direction, absent-field placement, unsupported order key rejection), boundary predicates, token cache/single-flight/reactive-401 (wiremock), transport retry/backoff/Retry-After (wiremock), metrics label and cardinality behavior, saga step ordering via configurable stubs |
| SDK contract | Conformance to the `IdpPluginClient` failure enums and semantics across all three halves; the crate carries a placeholder awaiting the upstream AM-SDK conformance harness |
| Real-Keycloak integration | Realm ensure idempotency, group lifecycle, role grants, user replay, cross-tenant denial, deletion ordering, provider failures, query behavior (owned by the deployment repository's E2E stacks) |
| Lifecycle integration | Enabled/disabled init paths, pre-warm retry/fast-bail budgets, catalogue re-registration drift detection |
| Cross-system audit contract | Account Management and the platform audit owner prove durable terminal outcomes; external production gate |

Test doubles are limited to deterministic classification and failure injection (wiremock for HTTP, stub credstore clients); they do not establish Keycloak compatibility, tenant isolation, TLS, or latency.

The release-qualification query matrix for the PRD latency NFR covers unfiltered results and filters matching approximately 100%, 10%, and 1% of the tenant group, each over cached-token and forced-token-refresh runs.

### 4.3 Risks and Enablement Gates

| Gate | Required resolution before production enablement |
|---|---|
| Scoped provider selection | Account Management resolves the configured GTS instance (`idp.vendor` = the plugin's vendor); catalogue drift on restart fails init; provider changes require an operator migration |
| Realm profile | Operator realm bootstrap provisions the authentication profile, protocol mappers (`tenant_id`, `user_type`), clients, scopes, and token policy; the plugin assumes rather than verifies it — a runtime profile verifier is future work |
| Provider compatibility | The implementation is developed and qualified against Keycloak 26.x; no runtime version gate exists — the compatibility matrix is a release-qualification obligation |
| Query capacity | Release qualification proves the PRD latency profile within the 200-item page cap and 10,000-member drain cap; population growth beyond the cap requires the realm-wide attribute-query evolution before support is claimed |
| User replay | Failure injection after each provider effect converges to one correctly bound identity (orphan compensation verified) |
| Hard deletion | Two-tenant integration proves the retiring tenant's boundary, service accounts, and (created mode) realm and secret are removed without touching the active tenant |
| Audit ownership | Parent Account Management and platform audit designs assign persistence, recovery, delivery, and retention; the plugin's tracing stand-in is not a production substitute |
| Reconciliation | Operator runbook exists and consumes `ambig:{stage}` evidence without secret inspection |
| Init dependency | Deployment ordering tolerates the pre-warm budget (Keycloak/Credential Store reachable within ≈37 s of plugin init) or explicitly disables the plugin |
| User updates | `update_user` support requires implementing the SDK contract (JSON Merge Patch semantics, duplicate/password-policy classification) before any product surface promises profile editing through this provider |
| Service-account contract | Implemented against the contract as shipped in `account-management-sdk` (PRD §5.4). The published trait is authoritative wherever it differs from the design below. Tenant and user lifecycle carry no dependency on it and gate independently, so a service-account regression cannot block them |
| Host wiring | The crate is deliberately landed **unwired**: no host in this repository links it, so its `inventory`-based gear registration never fires, and the repository carries no worked configuration sample. Enabling it requires a host that links the crate, a `keycloak:` configuration tree satisfying `Gear::init` (`base_url`, `realm_admin.secret_ref_template`, `bootstrap_pre_warm.*`, `service_account.*`, `security_context.platform_tenant_id`, the metrics cap, `vendor`/`priority`), and `account-management.idp.vendor` aligned to this plugin's vendor. Until then the code is compiled and unit-tested but never initialized, which is why the gates below are stated as deployment obligations rather than satisfied conditions |

### 4.4 Observability

#### Instrument catalogue

The §3.6 "Audit and Metrics Boundary" summary enumerates the same instruments in prose; this section is the per-instrument breakdown of types, labels, and owning ports. Names are literal Prometheus instrument names declared as constants in `src/domain/metrics.rs`, and the emitter set is what `src/infra/metrics.rs` and its call sites actually do — check both against the code rather than trusting either document when they diverge.

The wire-side suffix is baked into the name (`_total` for monotonic counters, a unit suffix for histograms, no suffix for up-down-counters) and the adapter never calls `with_unit(...)`. The rendered series name is therefore identical whether the downstream collector appends metric suffixes or not. Each instrument is emitted through exactly one of the seven segregated metric ports (§3.2).

| Instrument | Type | Labels | Port | Status |
|---|---|---|---|---|
| `keycloak_idp_plugin_provision_tenant_duration_seconds` | Histogram, seconds | `realm_binding` ∈ {`shared`, `adopted`, `created`} | `TenantLifecycleMetricsPort` | Live |
| `keycloak_idp_plugin_realms_bound` | UpDownCounter (no suffix) | `realm_binding` × `realm_name` (capped) | `TenantLifecycleMetricsPort` | Live |
| `keycloak_idp_plugin_deprovision_missing_metadata_total` | Counter | none — `realm_name` is unknowable by definition on this path, metadata was its only source | `TenantLifecycleMetricsPort` | Live |
| `keycloak_idp_plugin_user_op_duration_seconds` | Histogram, seconds | `op` ∈ {`provision_user`, `deprovision_user`, `list_users`} | `UserOpMetricsPort` | Live |
| `keycloak_idp_plugin_orphan_user_compensation_total` | Counter | `outcome` ∈ {`ok`, `failed`} | `UserOpMetricsPort` | Live |
| `keycloak_idp_plugin_sa_op_duration_seconds` | Histogram, seconds | `op` ∈ {`sa_create`, `sa_rotate_secret`, `sa_revoke`, `sa_list`, `sa_purge`} | `SaOpMetricsPort` | Live |
| `keycloak_idp_plugin_credstore_write_total` | Counter | `op` ∈ {`put`, `delete`} × `outcome` ∈ {`ok`, `error`} | `CredstoreMetricsPort` | Live |
| `keycloak_idp_plugin_metadata_decode_failure_total` | Counter | `version_observed` ∈ {`missing`, `malformed`, observed version string} | `MetadataCodecMetricsPort` | Live |
| `keycloak_idp_plugin_failure_total` | Counter | `op` × `failure_variant` (vocabularies below) | `FailureMetricsPort` | Live |
| `keycloak_idp_plugin_kc_admin_token_refresh_total` | Counter | `outcome` ∈ {`success`, `error`} × `tier` ∈ {`static_env`, `openbao`} × **`realm`** (capped) | `KcAdminMetricsPort` | Live |
| `keycloak_idp_plugin_credential_refresh_total` | Counter | `outcome` × `tier` × **`realm`** (capped) | `KcAdminMetricsPort` | Live |
| `keycloak_idp_plugin_kc_admin_request_duration_seconds` | Histogram, seconds | `endpoint_class` (sealed; one `unknown` placeholder today) | `KcAdminMetricsPort` | Reserved — no live emitters |

Note the label key is `realm` on the two refresh counters and `realm_name` on `realms_bound`. The two spellings are historical, both are subject to the same cardinality budget, and a dashboard selector must use the exact key for the instrument it queries.

Both refresh counters are emitted from the token-acquisition slow path in `src/domain/kc/factory.rs` — every credential resolution for a `(realm, client_id)` pair, including first acquisition, not only rotations. Only `kc_admin_request_duration_seconds` is reserved: it is declared without emitters so the follow-up PR wiring the Keycloak HTTP layer does not have to reshape dependency injection. `endpoint_class` is a sealed newtype for the same reason — new endpoint families are added as `pub const`s alongside their emitting call sites, and no free-`&str` constructor exists, which is what keeps the cardinality surface closed.

#### `keycloak_idp_plugin_failure_total` label vocabularies

Both labels are closed sets, but the `failure_variant` vocabulary is **not** the `FailureVariant` constant list: every emit site converts through `From<&PluginError>`, so the wire values are whatever `failure_variant_label` in `src/domain/error.rs` produces. That is the authoritative list.

`op` is a superset of every plugin-level operation: `provision_tenant`, `deprovision_tenant`, `provision_user`, `deprovision_user`, `list_users`, `sa_create`, `sa_rotate_secret`, `sa_revoke`, `sa_list`, `sa_purge`.

`failure_variant` classifies the failure, not its text — 18 values: `config`, `credstore_read`, `metadata_decode`, `kc_rest`, `provision_input_rejected`, `ambiguous_created`, `created_realm_exists`, `bootstrap_perms_missing`, `deprovision_not_found`, `deprovision_retryable`, `deprovision_terminal`, `user_op_rejected`, `user_op_unavailable`, `user_op_unsupported`, `user_op_duplicate`, `user_op_password_policy`, `sa_invalid_input`, `sa_not_found`. A provider body never reaches a label — the counter is the metric-side counterpart of the redaction posture in §4.1.

#### Instrument prefix

The adapter substitutes the leading `keycloak_idp_plugin` namespace token for its `prefix` argument, so a host embedding the plugin under a different name can rebrand every emission in one place. This is an API capability of `KeycloakIdpPluginMetricsAdapter::new`, not an operator-facing setting: there is no `prefix` config key, and the in-crate wiring always passes `DEFAULT_PREFIX`, for which the substitution is a no-op.

#### Realm-label cardinality watchdog

The realm label is partner-controlled and grows with every created-mode realm, so it is capped rather than emitted freely. `keycloak.metrics.drop_realm_label_at_cardinality` (default `500`, `0` disables the cap) bounds the number of distinct values that may appear as a label.

The cap applies only to realms not already recorded: a realm that was labelled before the cap was reached keeps its label indefinitely, and the unbind path never consumes budget. Once the budget is full, a genuinely new realm is emitted without the label and a `tracing::warn!` naming that realm fires on **every** such observation — over-cap realms are deliberately never recorded, so nothing suppresses the repeat. Alert on the presence of that warning, not on a single transition. The adapter also offers the dropped value to the current tracing span as a `realm.name` field, but no span in this crate declares that field, so today an over-cap realm name is dropped rather than relocated.

The watchdog lives inside the adapter, not on the domain struct — call sites pass `&str` and the adapter decides whether to attach the label, so no emitter can bypass the cap.

All three realm-keyed instruments share one cap and one observed-realm set, which means a high-churn refresh path can consume the budget that `keycloak_idp_plugin_realms_bound` would otherwise use. The cap lives in plugin configuration rather than deferring to deploy-time SRE judgement because the label set is fixed at compile time and narrowing it later breaks dashboards and alerts.

## 5. Traceability

### 5.1 Authoritative Contracts

- [Adjacent PRD](./PRD.md)
- [Account Management DESIGN](../../../docs/DESIGN.md)
- [`IdpPluginClient` tenant contract](../../../account-management-sdk/src/idp.rs)
- [`IdpPluginClient` user contract](../../../account-management-sdk/src/idp_user.rs)
- the service-account half of the `IdpPluginClient` contract — `account-management-sdk`, delivered with the tenant and user halves
- Service-account product PRD and DESIGN — the owning contract for machine identities; this plugin is its registered adapter
- [Unified ToolKit architecture](../../../../../../docs/toolkit_unified_system/README.md)
- Implementation: crate `cf-gears-keycloak-idp-plugin`, delivered in a separate change

### 5.2 P1 Requirement Allocation

| PRD requirement IDs | Design sections |
|---|---|
| `cpt-cf-keycloak-idp-plugin-fr-provider-publication`, `cpt-cf-keycloak-idp-plugin-fr-readiness`, `cpt-cf-keycloak-idp-plugin-interface-provider-instance` | §§3.2, 3.6, 4.3 |
| `cpt-cf-keycloak-idp-plugin-fr-tenant-realm-binding`, `cpt-cf-keycloak-idp-plugin-fr-shared-realm-admissibility`, `cpt-cf-keycloak-idp-plugin-fr-adopted-realm-admissibility`, `cpt-cf-keycloak-idp-plugin-fr-created-realm-admissibility`, `cpt-cf-keycloak-idp-plugin-fr-tenant-provision`, `cpt-cf-keycloak-idp-plugin-usecase-bind-shared-tenant`, `cpt-cf-keycloak-idp-plugin-usecase-adopt-tenant-realm`, `cpt-cf-keycloak-idp-plugin-usecase-create-tenant-realm` | §§3.1, 3.6 |
| `cpt-cf-keycloak-idp-plugin-fr-provider-metadata` | §§3.1, 3.3 |
| `cpt-cf-keycloak-idp-plugin-fr-tenant-deprovision`, `cpt-cf-keycloak-idp-plugin-fr-tenant-service-account-cleanup`, `cpt-cf-keycloak-idp-plugin-fr-tenant-user-access-termination`, `cpt-cf-keycloak-idp-plugin-usecase-retire-tenant` | §§3.6, 4.1 |
| `cpt-cf-keycloak-idp-plugin-fr-tenant-failure-contract`, `cpt-cf-keycloak-idp-plugin-fr-external-mutation-resilience`, `cpt-cf-keycloak-idp-plugin-nfr-failure-classification` | §§3.3, 3.6 |
| `cpt-cf-keycloak-idp-plugin-fr-user-provision`, `cpt-cf-keycloak-idp-plugin-fr-user-deprovision` | §3.6 |
| `cpt-cf-keycloak-idp-plugin-fr-user-query` | §3.6 |
| `cpt-cf-keycloak-idp-plugin-fr-user-source-of-truth` | §§3.1, 4.1 |
| `cpt-cf-keycloak-idp-plugin-fr-service-account-lifecycle`, `cpt-cf-keycloak-idp-plugin-fr-service-account-state`, `cpt-cf-keycloak-idp-plugin-fr-service-account-safeguards`, `cpt-cf-keycloak-idp-plugin-fr-service-account-rotation`, `cpt-cf-keycloak-idp-plugin-fr-service-account-list`, `cpt-cf-keycloak-idp-plugin-fr-service-account-mutation-safety`, `cpt-cf-keycloak-idp-plugin-fr-service-account-secret-disclosure`, `cpt-cf-keycloak-idp-plugin-fr-service-account-recovery`, `cpt-cf-keycloak-idp-plugin-fr-service-account-failure-contract`, `cpt-cf-keycloak-idp-plugin-interface-service-account-client`, `cpt-cf-keycloak-idp-plugin-usecase-service-account-credentials`, `cpt-cf-keycloak-idp-plugin-usecase-list-revoke-service-accounts` | §§3.2–3.3, 3.6, 4.1 |
| `cpt-cf-keycloak-idp-plugin-fr-administrator-credentials`, `cpt-cf-keycloak-idp-plugin-contract-credstore` | §§3.5–3.6, 4.1 |
| `cpt-cf-keycloak-idp-plugin-fr-operator-reconciliation`, `cpt-cf-keycloak-idp-plugin-usecase-reconcile-ambiguous-mutation` | §§3.6, 4.3 |
| `cpt-cf-keycloak-idp-plugin-fr-offline-token-lifetime` | §§2.2, 3.6 |
| `cpt-cf-keycloak-idp-plugin-fr-audit-metrics`, `cpt-cf-keycloak-idp-plugin-fr-operational-metrics`, `cpt-cf-keycloak-idp-plugin-nfr-audit-completeness` | §§3.3, 3.5–3.6, 4.1–4.3 |
| `cpt-cf-keycloak-idp-plugin-nfr-tenant-isolation` | §§3.6, 4.1 |
| `cpt-cf-keycloak-idp-plugin-nfr-secret-nondisclosure` | §§3.6, 4.1 |
| `cpt-cf-keycloak-idp-plugin-nfr-lifecycle-latency` | §§1.2, 2.2, 3.6, 4.3 |
| `cpt-cf-keycloak-idp-plugin-nfr-personal-data-lifecycle` | §4.1 |
| `cpt-cf-keycloak-idp-plugin-nfr-availability-recovery` | §§2.2, 3.2, 3.6 |
| `cpt-cf-keycloak-idp-plugin-nfr-provider-compatibility` | §4.3 |
| `cpt-cf-keycloak-idp-plugin-interface-idp-plugin-client`, `cpt-cf-keycloak-idp-plugin-contract-account-management`, `cpt-cf-keycloak-idp-plugin-contract-keycloak-admin` | §§1.1, 2.1, 3.2–3.3 |

P2 requirements (user update, realm profile verification, runtime provider-version gate) remain traceable in the PRD but are intentionally not allocated to implementation components in this DESIGN.
