# PRD — GitHub Mirror

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
  - [3.1 Gear-Specific Environment Constraints](#31-gear-specific-environment-constraints)
- [4. Scope](#4-scope)
  - [4.1 In Scope](#41-in-scope)
  - [4.2 Out of Scope](#42-out-of-scope)
  - [4.3 Increment Scope (current state)](#43-increment-scope-current-state)
- [5. Functional Requirements](#5-functional-requirements)
  - [5.1 Synchronization Engine](#51-synchronization-engine)
  - [5.2 Entity Coverage](#52-entity-coverage)
  - [5.3 Synchronization Quality Attributes](#53-synchronization-quality-attributes)
  - [5.4 Storage and Persistence](#54-storage-and-persistence)
  - [5.5 Synchronization Phases](#55-synchronization-phases)
  - [5.6 Completeness Verification](#56-completeness-verification)
  - [5.7 Configurable Freshness Policies](#57-configurable-freshness-policies)
  - [5.8 REST API — Standard GitHub-Compatible Surface](#58-rest-api--standard-github-compatible-surface)
  - [5.9 REST API — Extended Analytics and Query Surface](#59-rest-api--extended-analytics-and-query-surface)
  - [5.10 Write-Back Operations](#510-write-back-operations)
  - [5.11 Multi-Tenancy and Access Control](#511-multi-tenancy-and-access-control)
  - [5.12 Token Pool Management](#512-token-pool-management)
  - [5.13 Public Library API](#513-public-library-api)
  - [5.14 Python Bindings](#514-python-bindings)
  - [5.15 Logging and Observability](#515-logging-and-observability)
  - [5.16 Telemetry](#516-telemetry)
  - [5.17 Environment Independence](#517-environment-independence)
  - [5.18 State Change Events](#518-state-change-events)
  - [5.19 CLI Tool](#519-cli-tool)
- [6. Non-Functional Requirements](#6-non-functional-requirements)
  - [6.1 Gear-Specific NFRs](#61-gear-specific-nfrs)
  - [6.2 NFR Exclusions](#62-nfr-exclusions)
  - [6.3 Data Governance](#63-data-governance)
- [7. Public Library Interfaces](#7-public-library-interfaces)
  - [7.1 Public API Surface](#71-public-api-surface)
  - [7.2 External Integration Contracts](#72-external-integration-contracts)
- [8. Use Cases](#8-use-cases)
- [9. Acceptance Criteria](#9-acceptance-criteria)
- [10. Dependencies](#10-dependencies)
- [11. Assumptions](#11-assumptions)
- [12. Risks](#12-risks)
- [13. Open Questions](#13-open-questions)
- [14. Traceability](#14-traceability)

<!-- /toc -->

## 1. Overview

### 1.1 Purpose

**GitHub Mirror** is a CF/Gears subsystem (gear) that maintains a full, continuously synchronized local replica of GitHub repository metadata and exposes that replica through both a GitHub-compatible REST API and an extended analytics API. Clients may interact with the mirror instead of GitHub directly, eliminating rate-limit constraints, GitHub availability dependencies, and API quota concerns while enabling deep data analytics, sophisticated dashboards, advanced search, and programmatic management of GitHub objects.

The gear operates as a Rust library with an HTTP server (the primary deployment mode), as a standalone CLI tool for one-shot and scheduled synchronization, and exposes Python bindings for scripting and data-pipeline integration.

### 1.2 Background / Problem Statement

Organizations that rely heavily on GitHub for code hosting, issue tracking, pull-request workflows, CI/CD pipelines, and security posture face five structural problems:

1. **Rate limits and API quotas** — GitHub enforces per-token rate limits (5,000 REST requests/hour, point-based GraphQL budgets) and secondary rate limits that trigger temporary bans under moderate concurrency. Tools performing deep analytics, building dashboards, or running large-scale searches routinely exhaust these limits, forcing degraded functionality or manual retry loops.

2. **GitHub availability dependency** — When GitHub experiences outages or degraded performance, every tool and workflow that depends on GitHub API data stalls. There is no local fallback for querying repository metadata, reviewing PR status, or inspecting CI results during downtime windows.

3. **No programmatic write-back with offline resilience** — Users who need to merge pull requests, post comments, update labels, or trigger workflows cannot do so when GitHub is unavailable or their API quota is exhausted. There is no mechanism to enqueue such operations and execute them when capacity is restored.

4. **Limited cross-repository metrics and virtual project views** — GitHub's native repository boundaries make it difficult to analyze work across multiple repositories as one product, program, or operational stream. Teams need virtual projects that aggregate issues, pull requests, workflows, releases, contributors, and health metrics across arbitrary repository sets without duplicating data or building one-off reporting pipelines.

5. **Incomplete security and isolation controls for complex review and merge scenarios** — GitHub's native security configuration may be too coarse, incomplete, or difficult to adapt when organizations need custom review policies, user/role isolation, privileged merge gates, repository-specific or file/folder-specific approval rules, or separation between read access and mutation authority. A mirror can enforce fully customized security and isolation policies before exposing data, approving actions, or forwarding write-back operations to GitHub.

A full local mirror that synchronizes incrementally with GitHub, serves a GitHub-compatible API, supports queued write-back operations, and enforces customizable security and isolation policies addresses all five problems while also enabling use cases that GitHub's native API cannot serve cost-effectively: cross-repository analytics, virtual project reporting, historical trend analysis, custom search indexes, and compliance archival.

### 1.3 Goals (Business Outcomes)

- Make GitHub metadata available as a dependable internal data service for engineering analytics, dashboards, compliance checks, operational reporting, and automation without making every consumer depend directly on GitHub API availability or rate limits
- Provide a complete and verifiable local replica of configured GitHub entities so business-critical decisions, reporting, and automated workflows are based on consistent data rather than partial, paginated, or quota-constrained API snapshots
- Enable cross-repository and virtual-project views that let teams measure products, programs, release trains, ownership areas, and operational streams across arbitrary repository sets instead of being constrained by GitHub's repository-centric model
- Support resilient GitHub operations by accepting management requests locally, applying custom validation and approval policies, and executing write-back operations when GitHub capacity and availability permit
- Allow organizations to implement security, isolation, review, and merge policies that are stricter or more specialized than GitHub's native controls, including user/role isolation and repository-specific or path-specific approval rules
- Preserve compatibility with existing GitHub-oriented tools through a GitHub-compatible REST API while adding extended analytics and governance capabilities that are specific to the mirror
- Scale GitHub data access for large repositories and organizations by reducing repeated upstream API calls, bounding synchronization resource usage, and serving high-volume read workloads from local storage
- Deliver a single gear that can be used in three flavours: as an HTTP REST API service within the CF/Gears runtime, as a standalone CLI tool for one-shot or scheduled synchronization, and as an embeddable Rust library with Python bindings for scripting and data-pipeline integration

### 1.4 Glossary

| Term | Definition |
|------|------------|
| Synchronization Session | A stateful context that tracks configuration, storage backends, rate-limit state, and task progress for one or more repository synchronizations |
| Cache Key | A canonical identifier derived from HTTP method, URL, sorted query parameters, and API version, used to deduplicate and look up cached responses |
| ETag | An opaque HTTP validator returned by GitHub that enables conditional requests; sending it back via `If-None-Match` yields a 304 when the resource is unchanged |
| Conditional Request | An HTTP request that includes `If-None-Match` or `If-Modified-Since` headers to avoid re-downloading unchanged data |
| Task Graph | A directed acyclic graph of synchronization tasks with priority ordering and dependency edges, enabling breadth-first list completion before deep refinement |
| Refinement | A second-pass synchronization that fills missing details, refreshes stale objects, or repairs failed pages for entities already discovered during the initial fetch |
| Secondary Rate Limit | An additional GitHub throttle applied when the server detects excessive concurrency or request volume beyond the primary per-token limit |
| Hybrid Cache | A caching mode that stores raw JSON on the filesystem and normalized entities in a relational database simultaneously |
| Derived Contributor | A person record deduced from embedded user objects already present in synchronized entities — built during normalization with no additional API calls |
| Token Pool | A set of GitHub PATs managed by the gear to distribute API requests across multiple tokens, effectively multiplying the available rate-limit budget |
| Write-Back Operation | A queued mutation (merge PR, post comment, etc.) that the mirror enqueues locally and executes against GitHub when API capacity is available |
| Mirror API | The HTTP REST API surface served by this gear, consisting of a GitHub-compatible layer and an extended analytics layer |

## 2. Actors

### 2.1 Human Actors

#### API Consumer

**ID**: `cpt-cf-github-mirror-actor-api-consumer`

- **Role**: A developer or tool operator who queries the mirror's REST API for repository data instead of querying GitHub directly.
- **Needs**: Low-latency, quota-free access to GitHub repository metadata; GitHub-compatible response schemas; extended query capabilities beyond GitHub's native API.

#### Library Consumer

**ID**: `cpt-cf-github-mirror-actor-lib-consumer`

- **Role**: A Rust developer who adds the library as a crate dependency and calls the public API to drive synchronizations, query entities, and manage write-back operations programmatically.
- **Needs**: A stable, well-documented async API that returns typed results, supports partial synchronization, and makes no environment decisions.

#### Python Consumer

**ID**: `cpt-cf-github-mirror-actor-python-consumer`

- **Role**: A Python developer who uses Python bindings to drive synchronizations from scripts, notebooks, or data pipelines.
- **Needs**: A Pythonic API surface via PyO3 with async support or synchronous wrappers, installable via pip/pipx/maturin.

#### CLI Operator

**ID**: `cpt-cf-github-mirror-actor-cli-operator`

- **Role**: A user who runs the CLI binary to synchronize repositories, query local data, manage the token pool, and inspect reports.
- **Needs**: A CLI that accepts repository references, configuration paths, and produces human-readable and machine-parseable output.

#### Platform Administrator

**ID**: `cpt-cf-github-mirror-actor-platform-admin`

- **Role**: An operator managing the gear in a multi-tenant server environment — configuring tenants, managing access control, monitoring synchronization health, and managing the token pool.
- **Needs**: Administrative APIs and CLI commands for tenant management, token pool configuration, and operational monitoring.

### 2.2 System Actors

#### GitHub REST API

**ID**: `cpt-cf-github-mirror-actor-github-rest`

- **Role**: Primary upstream data source for paginated entity lists, entity details, and conditional request support. Also the target for write-back operations. Enforces per-token rate limits and secondary rate limits.

#### GitHub GraphQL API

**ID**: `cpt-cf-github-mirror-actor-github-graphql`

- **Role**: Alternative upstream data source using cursor-based pagination and point-based rate limits. Used when it reduces total request count compared to REST.

#### Storage Backend

**ID**: `cpt-cf-github-mirror-actor-storage-backend`

- **Role**: A relational database engine (PostgreSQL, MariaDB, or SQLite) storing normalized entities, session state, cache metadata, tenant configuration, and write-back queues.

#### Filesystem Cache

**ID**: `cpt-cf-github-mirror-actor-filesystem-cache`

- **Role**: A local directory structure storing raw API response bodies (optionally compressed) and per-response metadata files.

#### Downstream API Client

**ID**: `cpt-cf-github-mirror-actor-downstream-client`

- **Role**: Any HTTP client (dashboards, analytics tools, CI integrations) that consumes the mirror's REST API instead of GitHub's API directly.

## 3. Operational Concept & Environment

> **Note**: Runtime, OS, architecture, lifecycle policy, and gear integration patterns are defined in this repository's foundational documents — the [architecture manifest](../../../docs/ARCHITECTURE_MANIFEST.md) and [guidelines/](../../../guidelines/). This section captures only this gear's deviations.

### 3.1 Gear-Specific Environment Constraints

- Requires an async Rust runtime (tokio) for concurrent HTTP requests, database operations, and serving the mirror API
- Requires at least one valid GitHub Personal Access Token (PAT) with appropriate scopes; supports token pools for distributed rate-limit budgets
- The gear MUST be deployable in two modes: as an in-process gear within the CF/Gears runtime (library + HTTP server + DB), and as a standalone CLI tool built from a dedicated CLI crate
- Python bindings require PyO3 and maturin for building and distributing wheels
- Filesystem cache requires write access to the configured cache root directory
- Database backends require the corresponding client library (libpq, libmariadb, or libsqlite3)

## 4. Scope

### 4.1 In Scope

- Full synchronization of all supported GitHub entity types: repository metadata, branches, tags, releases, milestones, labels, issues, issue comments, issue events, issue timeline, issue reactions, pull requests, PR reviews, PR review comments, PR issue comments, PR commits, PR files, PR diff/patch, PR merge status, commits, commit comments, commit statuses, check runs, check suites, workflow runs, workflow jobs, deployments, deployment statuses, project/card references (where API access allows), and security/dependabot/code-scanning alerts (when token scopes allow)
- Derived contributors / people index from embedded user objects (no additional API calls)
- Detection of `pull_request` field on issues and automatic PR-specific refinement
- GitHub-compatible REST API surface mirroring GitHub's native REST API v3
- Extended analytics and query REST API with cross-repository queries, advanced filtering, and aggregations
- Write-back operations: enqueue and execute mutations against GitHub with offline resilience
- Multi-tenancy and access control for both HTTP server and CLI tool
- Token pool management for distributing API requests across multiple PATs
- TOML-based configuration for all synchronization, caching, storage, API, and logging options
- Persistency layer to be implemented as plugins - Filesystem only, DB, hybrid
- In-memory task orchestration with priority queue, deduplication, and idempotent tasks
- ETag and Last-Modified conditional request support
- Per-endpoint stale TTLs with entity-state-aware refresh policies
- Re-enterable synchronization with force mode
- Completeness verification with retry of failed tasks
- Python bindings via PyO3
- CLI binary for command-line synchronization, query, and management (§6)
- CLI tool - structured logging via `tracing`
- Cache plugin system via gears plugin framework

### 4.2 Out of Scope

- Git repository cloning or source-code analysis
- GitHub App authentication (PAT-only for v1.0)
- GitHub mirror sync workers scalability across nodes
- Non-GitHub forges (GitLab, Bitbucket, Gitea)
- Dedicated people/user collection via user-profile endpoints (deferred; v1.0 derives contributors from synchronized entities only)
- GitHub webhook ingestion or real-time event streaming (future consideration)
- Web UI or built-in dashboard (consumers build dashboards on the mirror API)

### 4.3 Increment Scope (current state)

Sections 4.1, 5 and 9 describe the **full product**. Delivery is incremental; this
section is the boundary between what the current code delivers and what is deferred,
so the unchecked boxes below read as "not yet", not "abandoned".

**Delivered by the gear skeleton increment**
([gears-rust#4532](https://github.com/constructorfabric/gears-rust/issues/4532)):

- On-demand synchronization (`POST /github-mirror/v1/repos/{owner}/{name}/sync`) of a
  repository plus 25 entity families into tenant-scoped storage, in one transaction,
  with rate-limit retry/backoff and completeness-gated reconciliation of upstream
  deletions (every row carries `extracted_at`)
- GitHub-compatible read surface (29 endpoints, GitHub-shaped JSON,
  `page`/`per_page` + `Link` pagination) and the native `/github-mirror/v1` surface
  (health, repository listing with keyset cursor, sync trigger, commit-files and
  review-threads analytics reads)
- Multi-tenant isolation via SecureORM scoping on every query
- `github-mirror-sdk` client crate

**Deferred to tracked follow-ups**:

- Sync sessions, resume, background execution, scope configuration, per-repo run
  status: [gears-rust#4632](https://github.com/constructorfabric/gears-rust/issues/4632)
- ETag/Last-Modified conditional requests, upstream pagination cache, cache eviction
  and compression:
  [gears-rust#4630](https://github.com/constructorfabric/gears-rust/issues/4630)
- Credstore-backed per-tenant credentials and token pools:
  [gears-rust#4534](https://github.com/constructorfabric/gears-rust/issues/4534)
- Write-back operations, Python bindings, CLI tool, state-change events: later
  increments of this PRD, not yet scheduled
- Annotated-tag metadata: tags are mirrored as list-tags returns them (`name` plus
  the resolved commit SHA), so lightweight and annotated tags are indistinguishable
  and the annotated tag object's message/tagger/SHA are not captured — the Git Data
  API is not called in this increment
- List-endpoint filters GitHub accepts but the mirror does not yet apply:
  `labels`, `assignee`, `creator`, `mentioned` and `milestone` on issues and
  pull requests, and `sha`/`path`/`author` on commits. `state`, `sort`,
  `direction` and `since` are honored. An unsupported filter is ignored rather
  than rejected, so a client relying on it gets a wider result set than it
  asked for — the mirrored data is there, the narrowing is not
- `since` is an inclusive bound: a row stamped exactly at the given instant is
  returned. GitHub's own wording is "updated after", and `ALGORITHMS.md` §3.3
  covers the sync cutoff rather than this endpoint, so the choice is the
  mirror's. It is inclusive because the stored timestamps are GitHub's own,
  at whole-second precision: an exclusive bound would drop a row updated later
  in the same second, while an inclusive one at worst hands an incremental
  client one row it already had, and every write is an idempotent upsert. A
  `since` carrying fractional seconds is rounded up to the next whole second,
  so `00:00:00.500Z` excludes a row stamped `00:00:00Z`
- Security synchronization (§5.2 `cpt-cf-github-mirror-fr-security-sync`): security
  advisories, Dependabot alerts and code-scanning alerts have no entities, tables or
  endpoints yet; they need token scopes the shared credential cannot be assumed to
  carry, so they land with per-tenant credentials
  ([gears-rust#4534](https://github.com/constructorfabric/gears-rust/issues/4534))
- PR-specific refinement tasks (§5.2 `cpt-cf-github-mirror-fr-issue-pr-detection`):
  an issue carrying `pull_request` is flagged as one, but no refinement task is
  enqueued for it — pull requests are instead picked up by the `/pulls` listing, so
  one outside that listing's reach keeps `is_pull_request` without reviews, files or
  commits until the task queue arrives
  ([gears-rust#4632](https://github.com/constructorfabric/gears-rust/issues/4632))

**Decisions recorded for the current increment**:

- *Visibility inside a tenant is flat.* Authorization is tenant-scoping plus the AuthZ
  Resolver's decision; the mirror does not re-derive GitHub-actor-level visibility
  (e.g. it serves draft PRs to any caller the platform authorizes for the tenant).
  Mirrored data is treated as shared within the tenant that configured the sync.
- *`target_url`/`details_url` pass through verbatim.* The mirror is a faithful
  replica; redacting or allow-listing CI/status URLs is a consumer-side concern.
- *One gear-wide `github_token` is an interim shortcut.* Until
  [gears-rust#4534](https://github.com/constructorfabric/gears-rust/issues/4534)
  lands, every tenant's sync authenticates to GitHub with the same credential; the
  README documents the operational caveat.
- *The health endpoint is runtime infrastructure.* `/github-mirror/v1/health` reports
  gear liveness/version/configuration and sits outside this PRD's requirement
  tracking, like every other gear's health probe.

## 5. Functional Requirements

> **Testing strategy**: All requirements verified via automated tests (unit, integration, e2e) targeting 90%+ code coverage unless otherwise specified. Document verification method only for non-test approaches.

### 5.1 Synchronization Engine

#### Session Initialization

- [ ] `p1` - **ID**: `cpt-cf-github-mirror-fr-session-init`

The system **MUST** accept a session configuration struct (provided by the caller) containing: GitHub token (or token pool reference), database connection credentials, telemetry log file path, cache configuration, and synchronization scope. The system **MUST** validate the GitHub token(s) and their scopes where possible, open the selected storage backend, create a synchronization session record, and load previous cursors and failed tasks from prior sessions. The library **MUST NOT** read environment variables or configuration files — all inputs come from the caller (see §5.20).

Multiple synchronization sessions **MUST** be able to run in parallel, each with its own configuration. Sessions that share the same GitHub token **MUST** share rate-limit budgets. The global request semaphore and per-token rate-limit controllers are managed by the engine singleton.

Synchronization **MUST** always run at the repository level: `sync_repo(session, repo, options)` is the sole entry point. Each call independently drives one repository through the synchronization phases. Multiple calls run concurrently with no cross-repo phase barrier. Global concurrency is controlled solely by the engine's request semaphore.

- **Rationale**: Every synchronization operation depends on a correctly initialized session; invalid tokens or misconfigured backends must be caught before any API calls are made.
- **Actors**: `cpt-cf-github-mirror-actor-lib-consumer`, `cpt-cf-github-mirror-actor-cli-operator`, `cpt-cf-github-mirror-actor-python-consumer`

#### Session Resumability

- [ ] `p1` - **ID**: `cpt-cf-github-mirror-fr-session-resume`

Synchronization **MUST** be re-enterable: the whole job is orchestrated in memory and individual tasks are NOT persisted. Re-running **MUST** rescan the full repository and rely on (a) the HTTP ETag/Last-Modified cache to avoid re-fetching unchanged data and (b) durable change-detection state (watermarks + entity fingerprints) to skip unchanged entities. The system **MUST** record a per-repo run status (`in_progress`/`complete`) and provide a resume operation that re-runs every repository still marked `in_progress`. A force mode **MUST** bypass the cache entirely.

- **Rationale**: Large-repo synchronization can take hours; re-entrancy via caching is simpler than persisting per-task state.
- **Actors**: `cpt-cf-github-mirror-actor-lib-consumer`, `cpt-cf-github-mirror-actor-cli-operator`, `cpt-cf-github-mirror-actor-python-consumer`

### 5.2 Entity Coverage

#### Configurable Entity Scope

- [ ] `p1` - **ID**: `cpt-cf-github-mirror-fr-sync-scope`

The system **MUST** support enabling or disabling synchronization of each entity type (issues, pull requests, reviews, comments, reactions, timeline, commits, files, statuses, checks, workflows, milestones, labels, releases, branches, tags, deployments, security alerts) via configuration. The default scope **MUST** enable standard repository metadata and collaboration entities (`issues`, `pull_requests`, `commits`, `releases`, `branches`, `labels`, `milestones`, `github_actions`, `contributors`). Security-related synchronization **MUST** be opt-in by default because GitHub requires elevated token permissions.

The system **MUST** separately support per-sub-resource collection breadth for expensive child resources: actions, reactions, and timeline. Supported modes **MUST** include `all`, `open`, and `none`. Defaults **MUST** collect actions and reactions for open issues/PRs only and disable timeline collection unless explicitly enabled.

- **Rationale**: Different use cases require different entity subsets; synchronizing unnecessary entities wastes API quota.
- **Actors**: `cpt-cf-github-mirror-actor-lib-consumer`, `cpt-cf-github-mirror-actor-cli-operator`, `cpt-cf-github-mirror-actor-python-consumer`

#### Issue-PR Detection

- [ ] `p1` - **ID**: `cpt-cf-github-mirror-fr-issue-pr-detection`

The system **MUST** detect the `pull_request` field on issue objects and automatically enqueue PR-specific refinement tasks (PR detail, reviews, review comments, commits, files, diff, patch, timeline, reactions, mergeability, statuses, check suites).

- **Rationale**: GitHub REST treats pull requests as issues; failing to detect and refine them results in incomplete data.
- **Actors**: `cpt-cf-github-mirror-actor-github-rest`

#### Security and Dependabot Synchronization

- [ ] `p2` - **ID**: `cpt-cf-github-mirror-fr-security-sync`

The system **MUST** attempt synchronization of security advisories, Dependabot alerts, and code-scanning alerts when token scopes allow, and **MUST** gracefully skip with a logged warning when scopes are insufficient.

- **Rationale**: Security data is valuable for compliance but requires elevated token permissions.
- **Actors**: `cpt-cf-github-mirror-actor-lib-consumer`, `cpt-cf-github-mirror-actor-github-rest`

#### Contributor Derivation

- [ ] `p2` - **ID**: `cpt-cf-github-mirror-fr-contributor-derivation`

The system **MUST** derive a per-repository set of distinct contributors from embedded user objects in synchronized entities, without additional GitHub API requests. The system **MUST** extract user objects (`login`, `id`, `node_id`, `avatar_url`, `html_url`, `type`) from: issue authors/assignees, PR authors/assignees/requested reviewers, comment authors, review authors, and commit `author`/`committer`. Each distinct person **MUST** be upserted into a normalized contributor record keyed by GitHub user `id` (login fallback), merging association roles (`author`, `assignee`, `reviewer`, `commenter`, `committer`) and maintaining `first_seen_at`/`last_seen_at` timestamps.

The system **MUST NOT** fetch user-profile or organization-membership endpoints in v1.0. The contributor schema **MUST** be forward-compatible for future profile enrichment.

- **Rationale**: Downstream analysis needs people as first-class entities; deriving from existing data achieves this at zero additional API cost.
- **Actors**: `cpt-cf-github-mirror-actor-lib-consumer`, `cpt-cf-github-mirror-actor-python-consumer`, `cpt-cf-github-mirror-actor-api-consumer`

### 5.3 Synchronization Quality Attributes

#### Cost-Efficient Synchronization

- [ ] `p1` - **ID**: `cpt-cf-github-mirror-fr-cost-efficiency`

Synchronization **MUST** minimize GitHub API token consumption. The system **MUST** use conditional requests (ETag, Last-Modified) to avoid re-fetching unchanged data, cache raw responses for reuse, and detect changes at the list level before fetching entity details. Incremental re-synchronization of an unchanged repository **MUST** consume at least 90% fewer API calls than a full synchronization.

- **Rationale**: GitHub API quota is the primary cost constraint; wasteful re-fetching exhausts budgets and triggers secondary rate limits.
- **Actors**: `cpt-cf-github-mirror-actor-lib-consumer`, `cpt-cf-github-mirror-actor-cli-operator`

#### Memory-Efficient Processing

- [ ] `p1` - **ID**: `cpt-cf-github-mirror-fr-memory-efficiency`

The system **MUST** process and persist data incrementally without loading entire entity sets into memory. Repositories with 100,000+ issues and pull requests **MUST** be synchronizable without unbounded memory growth.

- **Rationale**: Large repositories can contain millions of sub-entities; loading them all into memory is infeasible.
- **Actors**: `cpt-cf-github-mirror-actor-lib-consumer`

#### Low-Latency Parallel Synchronization

- [ ] `p1` - **ID**: `cpt-cf-github-mirror-fr-parallel-fetch`

The system **MUST** support parallel fetching of independent entities and sub-resources within a single repository synchronization, and parallel synchronization of multiple repositories. The system **MUST** respect GitHub rate limits while maximizing throughput.

- **Rationale**: Sequential fetching of large repositories is prohibitively slow; parallelism reduces wall-clock synchronization time.
- **Actors**: `cpt-cf-github-mirror-actor-lib-consumer`, `cpt-cf-github-mirror-actor-cli-operator`

#### Rate-Limit Compliance

- [ ] `p1` - **ID**: `cpt-cf-github-mirror-fr-rate-limit`

The system **MUST NOT** trigger GitHub secondary rate limits under normal operation. The system **MUST** track per-token rate-limit budgets, respect `Retry-After` headers, and automatically pause and resume when limits are near exhaustion. When multiple tokens are available via a token pool, the system **MUST** distribute requests to maximize aggregate throughput.

- **Rationale**: Rate-limit violations cause temporary bans that halt synchronization for extended periods.
- **Actors**: `cpt-cf-github-mirror-actor-github-rest`, `cpt-cf-github-mirror-actor-github-graphql`

#### Idempotent and Resumable Operations

- [ ] `p1` - **ID**: `cpt-cf-github-mirror-fr-idempotent`

All synchronization operations **MUST** be idempotent — running the same synchronization twice **MUST** produce identical state without corruption or duplicates. Interrupted synchronizations **MUST** be resumable without re-fetching already-cached unchanged data.

- **Rationale**: Idempotency is required for safe resume, retry, and concurrent execution.
- **Actors**: `cpt-cf-github-mirror-actor-lib-consumer`, `cpt-cf-github-mirror-actor-cli-operator`

### 5.4 Storage and Persistence

#### Raw Response Preservation

- [ ] `p1` - **ID**: `cpt-cf-github-mirror-fr-raw-storage`

The system **MUST** preserve every raw API response before normalization, together with its metadata (URL, HTTP status, conditional-request validators, fetch timestamp, content hash, schema version, rate-limit metadata, pagination metadata, and compression algorithm). Raw responses **MUST** be available for re-normalization, debugging, and audit.

The raw-response store **MUST** support configurable body compression modes: `none`, `gzip`, and `zstd`. The content hash **MUST** be computed over the uncompressed response body so integrity checks are independent of the chosen compression mode.

The raw-response store **MUST** use visibility-aware cache partitioning. Public repository responses **MAY** be cached once per canonical org/repo/request key and reused across tenants and tokens. Private or visibility-unknown repository responses **MUST** be cached in a tenant-scoped partition and **MUST NOT** be reused across tenants. API consumers **MUST NOT** receive raw cache entries directly; tenant-specific access is enforced by the GitHub Mirror API over normalized data.

- **Rationale**: Raw responses are the source of truth; normalized data can always be regenerated from them.
- **Actors**: `cpt-cf-github-mirror-actor-lib-consumer`, `cpt-cf-github-mirror-actor-storage-backend`

#### Normalized Entity Storage

- [ ] `p1` - **ID**: `cpt-cf-github-mirror-fr-normalized-storage`

The system **MUST** normalize raw responses into typed entity records for all supported entity types and persist them via idempotent upserts. Every record **MUST** include an `extracted_at` timestamp recording when the data was synchronized locally.

- **Rationale**: Normalized storage enables efficient querying and the `extracted_at` timestamp enables incremental consumption patterns.
- **Actors**: `cpt-cf-github-mirror-actor-storage-backend`

#### Multi-Engine Database Support

- [ ] `p1` - **ID**: `cpt-cf-github-mirror-fr-multi-db`

The system **MUST** support PostgreSQL, MariaDB, and SQLite as database backends. No engine-specific SQL **MUST** leak into business logic.

- **Rationale**: SQLite for local/CLI use; PostgreSQL/MariaDB for multi-tenant server deployments.
- **Actors**: `cpt-cf-github-mirror-actor-storage-backend`

#### Pluggable Persistence Layer

- [ ] `p1` - **ID**: `cpt-cf-github-mirror-fr-persistence-plugins`

The persistence layer **MUST** be implemented as plugins with at least three built-in modes: filesystem-only (raw responses on disk, suitable for CLI), database-only (all data in DB, no filesystem dependency, suitable for REST API service), and hybrid (filesystem + database). The gear **MUST** be fully functional without any filesystem access when using the database-only plugin.

- **Rationale**: The CLI tool benefits from filesystem caching for manual inspection, while the REST API service deployment should avoid filesystem dependencies for containerized and ephemeral environments.
- **Actors**: `cpt-cf-github-mirror-actor-lib-consumer`, `cpt-cf-github-mirror-actor-platform-admin`

### 5.5 Synchronization Phases

#### Repo Discovery and Base Metadata

- [ ] `p1` - **ID**: `cpt-cf-github-mirror-fr-repo-discovery`

The system **MUST** fetch `/repos/{owner}/{repo}`, store the raw response, normalize repository metadata, and seed the task graph with list tasks for all configured entity types.

- **Rationale**: Repository metadata is the foundation; the seeded task graph drives the entire process.
- **Actors**: `cpt-cf-github-mirror-actor-github-rest`

#### Issue Refinement

- [ ] `p1` - **ID**: `cpt-cf-github-mirror-fr-issue-refinement`

For each issue: synchronize detail, comments, events, timeline, reactions, labels, milestone, and assignees. If `pull_request` field is present, enqueue PR refinement.

- **Rationale**: Issues are the core entity; complete data requires following all sub-resource endpoints.
- **Actors**: `cpt-cf-github-mirror-actor-github-rest`

#### PR Refinement

- [ ] `p1` - **ID**: `cpt-cf-github-mirror-fr-pr-refinement`

For each PR: synchronize detail, reviews, review comments, issue comments, commits, files, diff, patch, timeline, reactions, mergeability (with retry for delayed computation), statuses, check suites, check runs, and workflow runs.

- **Rationale**: PRs have the richest sub-resources; incomplete data undermines code-review and CI analysis.
- **Actors**: `cpt-cf-github-mirror-actor-github-rest`

#### Commit and CI Refinement

- [ ] `p2` - **ID**: `cpt-cf-github-mirror-fr-commit-ci-refinement`

For each commit SHA: synchronize detail (author, committer, message, tree SHA, parent SHAs), file changes (filename, status, additions, deletions, patch, previous filename for renames), commit-level stats, comments, combined/individual statuses, check suites, check runs, and workflow jobs.

- **Rationale**: Commit body data is essential for change impact assessment and migration completeness.
- **Actors**: `cpt-cf-github-mirror-actor-github-rest`

#### Synchronization Order and Date Window

- [ ] `p1` - **ID**: `cpt-cf-github-mirror-fr-sync-order`

The system **MUST** prioritize work as: (1) open PRs + content, (2) open issues + content, (3) global metrics (releases, branches, milestones, labels, tags), (4) closed PRs (most-recently-closed first), (5) closed issues (most-recently-closed first). A `--since` window **MUST** bound only closed partitions (groups 4–5); open items are always synchronized in full.

- **Rationale**: Current open workload takes priority; bounding only closed partitions keeps incremental runs cheap.
- **Actors**: `cpt-cf-github-mirror-actor-lib-consumer`, `cpt-cf-github-mirror-actor-cli-operator`

### 5.6 Completeness Verification

#### Post-Synchronization Verification

- [ ] `p1` - **ID**: `cpt-cf-github-mirror-fr-completeness-check`

After synchronization, the system **MUST** compare expected entity counts from list endpoints with stored normalized counts. The system **MUST** verify PR child resources (files, reviews, commits, timeline), issue child resources (comments, events/timeline, reactions), and commit file changes. The system **MUST** retry failed or partial tasks and persist a final synchronization report.

- **Rationale**: Without verification, silent data loss goes undetected; the report provides auditable completeness evidence.
- **Actors**: `cpt-cf-github-mirror-actor-lib-consumer`, `cpt-cf-github-mirror-actor-cli-operator`

### 5.7 Configurable Freshness Policies

#### Stale-Aware Refresh

- [ ] `p2` - **ID**: `cpt-cf-github-mirror-fr-stale-refresh`

The system **MUST** support configurable per-entity-type freshness policies that control how aggressively previously-synchronized entities are re-checked. Active entities (open PRs, open issues, pending CI) **MUST** be refreshed more frequently than stable entities (closed items, labels, releases). Freshness intervals **MUST** be configurable by the operator.

- **Rationale**: Different entity types have different change velocities; a single refresh policy wastes API quota on stable data or misses changes on active data.
- **Actors**: `cpt-cf-github-mirror-actor-lib-consumer`, `cpt-cf-github-mirror-actor-cli-operator`

### 5.8 REST API — Standard GitHub-Compatible Surface

#### GitHub-Compatible Read API

- [ ] `p1` - **ID**: `cpt-cf-github-mirror-fr-github-compat-api`

The gear **MUST** expose a REST API surface that mirrors GitHub's native REST API v3 endpoints and response schemas for all synchronized entity types. Clients that currently call GitHub's REST API **MUST** be able to point at the mirror API with minimal changes (base URL substitution). The API **MUST** support the same pagination model (Link headers), the same query parameters, and return response bodies schema-compatible with GitHub's responses.

The following endpoint families **MUST** be supported (reading from the local normalized store — no upstream GitHub calls):

- `/repos/{owner}/{repo}` — repository metadata
- `/repos/{owner}/{repo}/issues` and `/repos/{owner}/{repo}/issues/{number}` — issues
- `/repos/{owner}/{repo}/issues/{number}/comments` — issue comments
- `/repos/{owner}/{repo}/issues/{number}/events` — issue events
- `/repos/{owner}/{repo}/issues/{number}/timeline` — issue timeline
- `/repos/{owner}/{repo}/issues/{number}/reactions` — issue reactions
- `/repos/{owner}/{repo}/pulls` and `/repos/{owner}/{repo}/pulls/{number}` — pull requests
- `/repos/{owner}/{repo}/pulls/{number}/reviews` — PR reviews
- `/repos/{owner}/{repo}/pulls/{number}/comments` — PR review comments
- `/repos/{owner}/{repo}/pulls/{number}/commits` — PR commits
- `/repos/{owner}/{repo}/pulls/{number}/files` — PR files
- `/repos/{owner}/{repo}/commits` and `/repos/{owner}/{repo}/commits/{sha}` — commits
- `/repos/{owner}/{repo}/commits/{sha}/comments` — commit comments
- `/repos/{owner}/{repo}/commits/{sha}/statuses` — commit statuses
- `/repos/{owner}/{repo}/commits/{sha}/check-runs` — check runs
- `/repos/{owner}/{repo}/branches` — branches
- `/repos/{owner}/{repo}/tags` — tags
- `/repos/{owner}/{repo}/releases` — releases
- `/repos/{owner}/{repo}/milestones` — milestones
- `/repos/{owner}/{repo}/labels` — labels
- `/repos/{owner}/{repo}/actions/runs` — workflow runs
- `/repos/{owner}/{repo}/actions/runs/{run_id}/jobs` — workflow jobs
- `/repos/{owner}/{repo}/deployments` — deployments
- `/repos/{owner}/{repo}/contributors` — derived contributors

- **Rationale**: A GitHub-compatible API allows existing tools, dashboards, and integrations to consume mirror data with minimal code changes.
- **Actors**: `cpt-cf-github-mirror-actor-downstream-client`, `cpt-cf-github-mirror-actor-api-consumer`

### 5.9 REST API — Extended Analytics and Query Surface

#### Extended Query API

- [ ] `p2` - **ID**: `cpt-cf-github-mirror-fr-extended-api`

The gear **MUST** expose an extended REST API surface under a versioned gear path (e.g., `/github-mirror/v1/`) with capabilities beyond GitHub's native API:

- Cross-repository entity queries (e.g., list all open PRs across all mirrored repos)
- Advanced filtering by `extracted_since` timestamp (entities synchronized after a given point)
- Derived logical conversation grouping: inline conversations (chains of review comments linked by `in_reply_to_id`) and top-level conversations (chains of comments where a later comment blockquotes at least 16 contiguous characters of an earlier comment)
- Contributor analytics: list contributors across repositories with association roles and activity windows
- Synchronization status and reports per repository
- Session management: list prior synchronization sessions with status, timing, and entity counts
- Session telemetry: retrieve per-request telemetry for a specific session
- Composite synchronization summary with per-object-type metrics, API usage, and storage footprint
- Rate-limit status: current token/pool quota state

- **Rationale**: GitHub's native API cannot serve cross-repository queries, custom aggregations, or incremental consumption patterns. The extended API justifies running a mirror.
- **Actors**: `cpt-cf-github-mirror-actor-api-consumer`, `cpt-cf-github-mirror-actor-downstream-client`

### 5.10 Write-Back Operations

#### Queued Write Operations

- [ ] `p3` - **ID**: `cpt-cf-github-mirror-fr-write-back`

The gear **MUST** support enqueuing write operations against GitHub through the mirror API. Supported operations **MUST** include:

- Merge a pull request
- Post a comment on a pull request (inline review comment or top-level PR comment)
- Assign or change PR reviewers
- Add or remove labels on a pull request
- Create, update, or close an issue

Each operation **MUST** be enqueued in a durable queue with a unique operation ID. The system **MUST** execute enqueued operations when GitHub API capacity is available and record the result. Failed operations **MUST** be retried with backoff up to a configurable maximum. The queue **MUST** be queryable: consumers can check status, retrieve results, and cancel pending operations.

- **Rationale**: Users need to manage GitHub content without depending on GitHub availability or quota.
- **Actors**: `cpt-cf-github-mirror-actor-api-consumer`, `cpt-cf-github-mirror-actor-lib-consumer`, `cpt-cf-github-mirror-actor-cli-operator`

### 5.11 Multi-Tenancy and Access Control

#### Multi-Tenant Isolation

- [ ] `p1` - **ID**: `cpt-cf-github-mirror-fr-multi-tenancy`

The gear **MUST** implement standard gears multi-tenancy: tenant-scoped data isolation in the database, tenant-aware API routing, and tenant-scoped configuration (synchronization targets, token pools, cache policies). Each tenant **MUST** only access repositories and data authorized for that tenant. Public repository raw cache entries **MAY** be shared across tenants, but private or visibility-unknown repository raw cache entries **MUST** remain tenant-partitioned.

- **Rationale**: Server deployments serve multiple teams or organizations; data isolation is a security and operational requirement.
- **Actors**: `cpt-cf-github-mirror-actor-platform-admin`, `cpt-cf-github-mirror-actor-api-consumer`

#### Access Control

- [ ] `p1` - **ID**: `cpt-cf-github-mirror-fr-access-control`

The gear **MUST** implement standard gears access control for both the HTTP server and the CLI tool. API endpoints **MUST** enforce authentication and authorization consistent with the platform's security model. The CLI **MUST** support the same access control model adapted for command-line usage. The raw cache **MUST NOT** be exposed as a consumer-facing authorization surface; all consumer reads are mediated by GitHub Mirror API tenancy and access checks.

- **Rationale**: Both server and CLI need access control to prevent unauthorized data access.
- **Actors**: `cpt-cf-github-mirror-actor-platform-admin`, `cpt-cf-github-mirror-actor-api-consumer`, `cpt-cf-github-mirror-actor-cli-operator`

### 5.12 Token Pool Management

#### Token Pool Support

- [ ] `p2` - **ID**: `cpt-cf-github-mirror-fr-token-pool`

The system **MUST** support configuring multiple GitHub PATs as a token pool. The system **MUST** distribute API requests across pool tokens to multiply the available rate-limit budget. Each token **MUST** have independent rate-limit tracking. The system **MUST** automatically rotate to the next available token when the current token's limit is near exhaustion.

Token pool management **MUST** support: adding/removing tokens, per-token scope annotations, and health monitoring (detect revoked or invalid tokens and remove from the active pool).

- **Rationale**: A single PAT is limited to 5,000 REST requests/hour; a pool removes the single-token bottleneck for large-scale synchronization.
- **Actors**: `cpt-cf-github-mirror-actor-platform-admin`, `cpt-cf-github-mirror-actor-cli-operator`

### 5.13 Public Library API

#### Library API Requirements

- [ ] `p1` - **ID**: `cpt-cf-github-mirror-fr-public-api`

The gear **MUST** expose a stable async Rust library API that covers: engine and session lifecycle, repository synchronization (unified entry point for initial and incremental), entity retrieval (detail and list for all synchronized entity types), cache management, session listing and telemetry, and write-back operations. All entity retrieval functions **MUST** read from the local normalized store without making GitHub API calls. The API **MUST** support an `extracted_since` filter for incremental consumption.

The specific function signatures and groupings are defined in DESIGN.md.

- **Rationale**: A comprehensive library API enables both the HTTP server and CLI to be built on the same core, and allows third-party consumers to embed synchronization capabilities.
- **Actors**: `cpt-cf-github-mirror-actor-lib-consumer`

### 5.14 Python Bindings

#### PyO3 Python Module

- [ ] `p3` - **ID**: `cpt-cf-github-mirror-fr-python-bindings`

The Python bindings **MUST** be developed and maintained in this repository. The bindings **MUST** expose all library public functions via PyO3 with Pythonic naming and async support or synchronous wrappers. The bindings **MUST** be installable via pip and pipx using maturin-built wheels.

A rich pytest test suite **MUST** exercise the full pipeline through the Python wrapper: `pytest -> Python wrapper -> Rust library`.

- **Rationale**: Python is the dominant language for data analysis; Python bindings dramatically expand the addressable user base.
- **Actors**: `cpt-cf-github-mirror-actor-python-consumer`

### 5.15 Logging and Observability

#### Layered Structured Logging

- [ ] `p2` - **ID**: `cpt-cf-github-mirror-fr-logging`

The system **MUST** emit structured logs via `tracing`, organized into independently filterable severity layers. Tokens and `Authorization` headers **MUST NEVER** be logged. Every log line **MUST** include a worker/thread identifier.

- **`info`**: startup config dump, critical-phase counts per entity family, end-of-run summary (storage size, REST/GraphQL totals, remaining budgets, reset time in local timezone, elapsed, cache-hit ratio), rate-limit pauses, retry/backoff events
- **`debug`**: per-API-action intent and completion (method, endpoint family, conditional-request decision, remaining quota, duration), task enqueue/dequeue, per-entity DB operations (table, operation, key — not raw SQL)
- **`trace`**: request/response wire detail, cache hit/miss with canonical cache keys, SQL statements, filesystem operations — subject to the redaction rules below

#### Redaction and Retention

- [ ] `p1` - **ID**: `cpt-cf-github-mirror-fr-log-redaction`

`trace` carries mirrored GitHub content, which includes private repository bodies and the personal data attached to every author, so what it may contain is bounded rather than left to the verbosity switch:

- Response and request **bodies MUST be opt-in**, off by default, behind their own configuration flag rather than implied by `trace`. With the flag off, a wire-detail line records method, URL, status, size and duration only.
- SQL **MUST** be logged as the statement with its bind values elided; a bind value can be an issue body, a token expansion, or a contributor's name. Logging bound values **MUST** require the same explicit opt-in as bodies.
- `Authorization`, `Cookie`, `Set-Cookie` and `X-Hub-Signature*` **MUST** be redacted wherever headers are logged, in addition to the token prohibition above. Redaction **MUST** be applied by the logging layer, not by each call site.
- Cache keys **MUST NOT** embed credentials; the canonical key is derived from method, URL and `Accept` (DESIGN §3.5).
- **Production defaults MUST be `info`** with bodies and bind values off. A deployment raising verbosity beyond `debug` **MUST** be a deliberate, temporary act.
- Log **retention MUST be bounded** by the platform's own log policy; the gear **MUST NOT** write logs to its own files outside the runtime's logging configuration, so that policy is the single place retention is enforced.

- **Rationale**: Layered logging lets operators dial in the right verbosity for their use case, but the mirror's `trace` layer would otherwise carry private repository content and personal data into whatever collects logs.
- **Actors**: `cpt-cf-github-mirror-actor-lib-consumer`, `cpt-cf-github-mirror-actor-cli-operator`

#### Composite Synchronization Summary

- [ ] `p2` - **ID**: `cpt-cf-github-mirror-fr-sync-summary`

`sync_repo` **MUST** return a composite summary: session ID; repository slug and URL; repository stars and forks; overall verification status; unresolved-mergeability flag; completion timestamp; elapsed duration; per-object-type metrics (total in storage, synchronized this session, REST API calls, GraphQL points); grand totals; API usage totals (REST, GraphQL, transient errors, permanent errors, remaining budgets, reset time); storage footprint (database, cache, total bytes).

- **Rationale**: A single structured summary gives operators an at-a-glance, cost-aware picture of synchronization results.
- **Actors**: `cpt-cf-github-mirror-actor-lib-consumer`, `cpt-cf-github-mirror-actor-cli-operator`

#### Progress Counter and Duration

- [ ] `p2` - **ID**: `cpt-cf-github-mirror-fr-progress`

The system **MUST** compute and persist an effective progress percentage (0–100) per repository run, together with run duration. Progress **MUST** be weighted by phase (discovery, indexing, change detection, refinement, verification) and **MUST** be monotonically non-decreasing. Progress **MUST** be persisted incrementally so interrupted runs report partial progress.

- **Rationale**: Long synchronizations need progress feedback.
- **Actors**: `cpt-cf-github-mirror-actor-lib-consumer`, `cpt-cf-github-mirror-actor-cli-operator`

### 5.16 Telemetry

#### Synchronization Session Telemetry

- [ ] `p1` - **ID**: `cpt-cf-github-mirror-fr-telemetry`

The library **MUST** collect structured telemetry per synchronization session and expose it programmatically so that callers can consume it. Telemetry **MUST** include:

- Per-request: endpoint URL, HTTP method, status, duration (ms), rate-limit remaining/reset, cache hit/miss, ETag used, response size, GraphQL points
- Overall progress: tasks pending/running/completed/failed, entities indexed/refined/skipped, queue depth snapshots
- Session summary: total API calls, total 304s, bytes downloaded/saved, cache hit ratio, elapsed time, final report

The library **MUST** support writing telemetry to a caller-specified file (append-only, JSON Lines) and **MUST** expose telemetry via the public API so that the CLI tool can print it and the REST API service can attach it to synchronization job status responses.

- **Rationale**: Telemetry is essential for cost analysis, diagnostics, and optimization. Programmatic access enables both CLI rendering and REST API job status enrichment.
- **Actors**: `cpt-cf-github-mirror-actor-lib-consumer`, `cpt-cf-github-mirror-actor-cli-operator`, `cpt-cf-github-mirror-actor-api-consumer`

### 5.17 Environment Independence

#### Library Environment Independence

- [ ] `p1` - **ID**: `cpt-cf-github-mirror-fr-env-independence`

The library crate **MUST NOT** read environment variables, access configuration files, write to stdout/stderr directly, or make any decisions about storage paths, log paths, or credential sources. All inputs **MUST** be provided by the caller.

The CLI tool **MAY** read environment variables (e.g., `GITHUB_TOKEN`), access configuration files, and resolve token sources — it is the boundary between the user's environment and the library.

- **Rationale**: Libraries that read environment variables are difficult to embed, test, and compose. The CLI tool is the appropriate place for environment resolution.
- **Actors**: `cpt-cf-github-mirror-actor-lib-consumer`, `cpt-cf-github-mirror-actor-python-consumer`, `cpt-cf-github-mirror-actor-cli-operator`

### 5.18 State Change Events

#### Entity Lifecycle Events

- [ ] `p2` - **ID**: `cpt-cf-github-mirror-fr-state-events`

The system **MUST** emit events on key entity state changes detected during synchronization:

- Pull request created, updated, merged, or closed
- Issue created, updated, or closed
- Comment created or replied to (issue comment, review comment, PR comment)
- Branch created or deleted
- Milestone created, updated, or closed
- Release created or updated
- Label created, updated, or deleted
- Check run / workflow run completed
- Contributor first seen in a repository

Events **MUST** be consumable by downstream systems via the gear's standard event/notification mechanism. Event payloads **MUST** include the entity type, entity ID, repository, change type, and timestamp.

- **Rationale**: Downstream systems (dashboards, automation, alerting) need to react to state changes without polling the mirror API. Events enable push-based integration.
- **Actors**: `cpt-cf-github-mirror-actor-downstream-client`, `cpt-cf-github-mirror-actor-api-consumer`

### 5.19 CLI Tool

> The CLI is a secondary deployment mode. The gear's primary interface is the REST API served by the HTTP server. The CLI provides equivalent synchronization, query, and management capabilities for command-line use, built on the same library crate. Requirements below apply to the dedicated CLI binary crate only.

#### CLI Binary Crate

- [ ] `p1` - **ID**: `cpt-cf-github-mirror-fr-cli-crate`

The CLI tool **MUST** be built as a dedicated binary crate within the gear's workspace, separate from the library crate. The CLI crate is the only component that reads environment variables, accesses configuration files, resolves GitHub tokens, and writes to stdout/stderr. It delegates all synchronization, query, and management logic to the library API.

- **Rationale**: Separating the CLI from the library preserves environment independence for the library while providing a user-friendly command-line interface.
- **Actors**: `cpt-cf-github-mirror-actor-cli-operator`

#### CLI Synchronization Commands

- [ ] `p1` - **ID**: `cpt-cf-github-mirror-fr-cli-sync`

The CLI **MUST** provide the following synchronization commands:

- `sync <ORG/REPO>` — Run a full synchronization for a repository. Options: `--force-full` (force re-index), `--force` (bypass all caches), `--max-concurrent <N>`, `--since <CUTOFF>` (date window: `YYYY-MM-DD`, `Nd`, `Nw`, `Nm`), `--config <PATH>`, `--include <TYPES>` (comma-separated), `--exclude <TYPES>` (comma-separated)
- `resume <ORG/REPO>` — Continue an interrupted synchronization

**Collection scope flags:** `--actions-scope <MODE>` (`open`/`all`/`none`), `--reactions-scope <MODE>`, `--timeline-scope <MODE>`, `--inline-comment-snippet-before <N>`, `--inline-comment-snippet-after <N>`

**Database and storage flags:** `--storage-dir <DIR>`, `--database-url <URL>` (SQLite/PostgreSQL/MariaDB), `--database-placement <MODE>` (`per_repo`/`per_org`/`shared`)

**Synchronizable entity types** (for `--include`/`--exclude`): `issues`, `pull_requests` (aliases: `prs`, `pulls`), `commits`, `releases`, `branches`, `labels`, `milestones`, `github_actions` (aliases: `actions`, `gha`), `contributors`, `security`

- **Rationale**: CLI synchronization commands mirror the library API for command-line operators.
- **Actors**: `cpt-cf-github-mirror-actor-cli-operator`

#### CLI Query Commands

- [ ] `p1` - **ID**: `cpt-cf-github-mirror-fr-cli-query`

The CLI **MUST** provide `query <ENTITY> <ORG/REPO>` for querying normalized entities from the local database (no network requests). Supported entities: `issues`, `prs`, `commits`, `contributors`, `repos`, `comments`, `conversations`, `review-threads`, `reviews`, `review-comments`, `reactions`, `timeline`, `branches`, `labels`, `milestones`, `releases`, `workflow_runs`

Query options: `--number <N>`, `--subject-type <TYPE>`, `--since <CUTOFF>`, `--extracted-since <ISO8601>`, `--limit <N>`, `--output-format <FMT>`

- **Rationale**: Local queries enable data exploration without network calls.
- **Actors**: `cpt-cf-github-mirror-actor-cli-operator`

#### CLI Management Commands

- [ ] `p1` - **ID**: `cpt-cf-github-mirror-fr-cli-management`

The CLI **MUST** provide:

- `status <ORG/REPO>` — Show synchronization status, entity counts, storage metrics, progress, and duration for prior/current runs
- `check-rate-limit` — Check token/pool GitHub API rate-limit status (REST + GraphQL quotas)
- `clear-cache <ORG/REPO>` — Remove all cached data for a repository (database entities, change-detection state, HTTP response cache)

- **Rationale**: Management commands support operational workflows.
- **Actors**: `cpt-cf-github-mirror-actor-cli-operator`

#### CLI TOML Configuration

- [ ] `p1` - **ID**: `cpt-cf-github-mirror-fr-cli-config`

The CLI **MUST** support a TOML configuration file for all synchronization, caching, storage, and logging options. The CLI **MUST** provide `--print-config` to dump the built-in default template. The configuration file path is specified via `--config <PATH>` (default: `./github-mirror.toml`).

#### CLI Global Options

- [ ] `p1` - **ID**: `cpt-cf-github-mirror-fr-cli-global-opts`

The CLI **MUST** support: `--token-file <PATH>` or `--token-fd <FD>` for the GitHub token (falling back to the `GITHUB_TOKEN` env var, then `~/.github-mirror/gh_token.txt`, and once credstore integration lands, a credential-store reference), `-v`/`-vv`/`-vvv` (log verbosity), `--log-level <LEVEL>`, `-q`/`--quiet` (suppress progress output)

The CLI **MUST NOT** accept the token as a plain `--token <TOKEN>` argument: command-line arguments are visible in process listings, shell history and CI logs, which contradicts the zero-exposure requirement above.

- **Rationale**: Standard CLI options for token resolution, verbosity, and configuration.
- **Actors**: `cpt-cf-github-mirror-actor-cli-operator`

#### CLI Output Formats

- [ ] `p1` - **ID**: `cpt-cf-github-mirror-fr-cli-output`

The CLI **MUST** support `--output-format <FMT>` with `table` (human-readable, default for status/sync) and `json` (machine-parseable, default for query) output formats. The synchronization summary **MUST** render as four aligned tables: repository info, per-object metrics, API usage, and storage.

- **Rationale**: Human-readable output for operators; JSON for pipeline integration.
- **Actors**: `cpt-cf-github-mirror-actor-cli-operator`

#### CLI Access Control

- [ ] `p1` - **ID**: `cpt-cf-github-mirror-fr-cli-access-control`

The CLI **MUST** support the same multi-tenancy and access control model as the HTTP server, adapted for command-line usage: token-based authentication, configuration-file-based tenant selection, and tenant-scoped data isolation.

- **Rationale**: CLI operators need the same access control guarantees as API consumers.
- **Actors**: `cpt-cf-github-mirror-actor-cli-operator`, `cpt-cf-github-mirror-actor-platform-admin`

## 6. Non-Functional Requirements

> **Default guidelines**: Project-wide NFR baselines are defined in this repository's [architecture manifest](../../../docs/ARCHITECTURE_MANIFEST.md) and [guidelines/](../../../guidelines/). This section captures only gear-specific NFRs.

### 6.1 Gear-Specific NFRs

#### Reliability and Idempotency

- [ ] `p1` - **ID**: `cpt-cf-github-mirror-nfr-reliability`

All database writes **MUST** be idempotent upserts. Every synchronization task **MUST** be retryable without corrupting state. Every failed request **MUST** be recorded and retryable in subsequent sessions.

- **Threshold**: Zero data corruption after any combination of interruption and resume cycles during synchronization of a repository with 100,000+ entities.
- **Architecture Allocation**: See DESIGN.md NFR Allocation.

#### Rate-Limit Compliance

- [ ] `p1` - **ID**: `cpt-cf-github-mirror-nfr-rate-compliance`

The system **MUST NOT** trigger GitHub secondary rate limits under normal operation with configured parallelism. The system **MUST** respect `Retry-After` headers and exponential backoff.

- **Threshold**: Zero secondary-rate-limit bans during synchronization with parallelism <= 8 and max retries <= 5 on repositories of any size.
- **Architecture Allocation**: See DESIGN.md NFR Allocation.

#### Memory Efficiency

- [ ] `p1` - **ID**: `cpt-cf-github-mirror-nfr-memory-efficiency`

The system **MUST** support synchronization of repositories with 100,000+ entities without unbounded memory growth. Pages **MUST** be streamed to storage immediately; in-memory buffers **MUST** be bounded.

- **Threshold**: Peak resident memory does not exceed 500 MB when synchronizing a repository with 100,000 issues and 50,000 pull requests.
- **Architecture Allocation**: See DESIGN.md NFR Allocation.

#### Code Coverage

- [ ] `p1` - **ID**: `cpt-cf-github-mirror-nfr-code-coverage`

The library crate **MUST** achieve at least 85% line coverage. Combined Rust + Python test coverage **MUST** meet or exceed 90%.

- **Threshold**: 85% line coverage verified by `cargo llvm-cov` and pytest coverage reports.
- **Architecture Allocation**: See DESIGN.md NFR Allocation.

#### Security

- [ ] `p1` - **ID**: `cpt-cf-github-mirror-nfr-security`

The system **MUST NOT** log, persist, or expose GitHub tokens in any output, cache file, database record, telemetry log, or error message. Tokens **MUST** be provided by the caller; the library **MUST NOT** read tokens from environment variables or configuration files. Private or visibility-unknown repository raw response bodies and validators **MUST NOT** cross tenant cache partitions.

When a platform credential store gear is available, the gear **MUST** use it for storing and retrieving GitHub tokens and other secrets. The gear **MUST NOT** implement its own credential storage — it **MUST** delegate to the existing credential store gear.

- **Threshold**: Zero instances of token exposure in any output path.
- **Architecture Allocation**: See DESIGN.md NFR Allocation.

#### Parallel Synchronization Efficiency

- [ ] `p1` - **ID**: `cpt-cf-github-mirror-nfr-parallel-sync`

The system **MUST** support parallel synchronization of multiple repositories within a single session. Per-token and global concurrency controls **MUST** prevent starvation and ensure fair scheduling across repositories.

- **Threshold**: Synchronizing 10 repositories in parallel completes within 1.5x the wall-clock time of synchronizing the single largest repository alone.
- **Architecture Allocation**: See DESIGN.md NFR Allocation.

### 6.2 NFR Exclusions

- **Accessibility** (UX-PRD-002): Not applicable — the gear has no graphical interface
- **Internationalization** (UX-PRD-003): Not applicable — English-only developer tooling
- **Regulatory Compliance** (COMPL-PRD-001/002/003): **Applicable** — see §6.3. The gear stores personal data by design: contributor records key on GitHub user identity, and mirrored issues, comments and commits carry author identities and user-authored text.

### 6.3 Data Governance

The mirror is a replica of personal data: every contributor row is a person, and every mirrored issue, comment, review and commit carries an author identity alongside user-authored text. The raw-response store (§5.2) keeps GitHub's payloads verbatim, including those identities. The following requirements apply to a deployment holding that data.

- [ ] `p1` - **ID**: `cpt-cf-github-mirror-nfr-data-governance`

- **Lawful basis and scope**: the mirror holds only what its configured repositories expose to the credential it syncs with. It **MUST NOT** fetch user-profile or organization-membership endpoints (§5.2), so it never widens the personal data beyond what the mirrored entities already carry.
- **Retention**: mirrored data **MUST** be deletable per repository and per tenant. Removing a repository from a tenant's scope **MUST** remove its mirrored rows and its raw responses; the raw-response store **MUST** honor a configurable maximum age independent of the normalized rows.
- **Erasure**: a deployment **MUST** be able to delete a person's mirrored footprint — contributor row, and the author identity on their entities — without deleting the entities themselves, so an erasure request can be answered without discarding the repository's history.
- **Access control**: personal data **MUST** be reachable only through the tenant-scoped surfaces (§5.11); no endpoint may return rows outside the caller's tenant, which is enforced in storage rather than per handler.
- **Audit**: reads and deletions of mirrored data **MUST** be attributable to a caller through the platform's `SecurityContext`, and sync runs **MUST** be attributable through their session records (§5.1).
- **Onward transfer**: mirrored content **MUST NOT** be sent to third parties by the gear itself; telemetry and logs are bounded by `cpt-cf-github-mirror-fr-log-redaction`.

- **Rationale**: The gear cannot claim regulatory exclusion while storing contributor identities and raw responses. Stating retention, erasure, access and audit requirements makes a deployment's obligations explicit rather than implicitly the consumer's.

## 7. Public Library Interfaces

### 7.1 Public API Surface

#### REST API — GitHub-Compatible

- [ ] `p1` - **ID**: `cpt-cf-github-mirror-interface-github-compat-rest`

- **Type**: Versioned HTTP/JSON REST API
- **Stability**: stable
- **Description**: GitHub REST API v3 compatible endpoint surface for all synchronized entity types. Serves from local store.
- **Breaking Change Policy**: Breaking changes to GitHub-compatible endpoints require a major version change.

#### REST API — Extended Analytics

- [ ] `p2` - **ID**: `cpt-cf-github-mirror-interface-extended-rest`

- **Type**: Versioned HTTP/JSON REST API under `/github-mirror/v1/`
- **Stability**: unstable (pre-1.0)
- **Description**: Extended query, analytics, session management, write-back operations, and synchronization control endpoints.
- **Breaking Change Policy**: Semver; breaking changes require minor version bump until 1.0, major after.

#### Rust Library API

- [ ] `p1` - **ID**: `cpt-cf-github-mirror-interface-rust-lib`

- **Type**: Rust async public functions
- **Stability**: unstable (pre-1.0)
- **Description**: async public functions organized as engine/session management, entity detail retrieval, entity list retrieval, and write-back operations.
- **Breaking Change Policy**: Semver.

#### Rust SDK Client

- [ ] `p1` - **ID**: `cpt-cf-github-mirror-interface-sdk`

- **Type**: Rust trait resolved from `ClientHub`
- **Stability**: unstable (pre-1.0)
- **Description**: In-process SDK contract for other gears to invoke synchronization, query, and write-back operations.
- **Breaking Change Policy**: Trait method removal or incompatible signature change requires a major version change.

#### Python Bindings Module

- [ ] `p3` - **ID**: `cpt-cf-github-mirror-interface-python`

- **Type**: Python module (via PyO3)
- **Stability**: unstable (pre-1.0)
- **Description**: Python-importable module exposing all library public functions with Pythonic naming and type hints.
- **Breaking Change Policy**: Semver aligned with Rust crate version.

#### CacheStore Trait

- [ ] `p1` - **ID**: `cpt-cf-github-mirror-interface-cache-store`

- **Type**: Rust trait
- **Stability**: unstable (pre-1.0)
- **Description**: Async trait defining visibility-aware `get`, validator-only `peek`, `put`, and metadata operations for cache backends. Public repositories use shared org/repo/request cache keys; private or visibility-unknown repositories use tenant-scoped cache keys. Implemented by filesystem, database, hybrid, and plugin cache modules.
- **Breaking Change Policy**: Semver; trait changes are breaking.

#### MetadataStore Trait

- [ ] `p1` - **ID**: `cpt-cf-github-mirror-interface-metadata-store`

- **Type**: Rust trait
- **Stability**: unstable (pre-1.0)
- **Description**: Async trait defining upsert operations for normalized entities. Implemented over SeaORM for SQLite, PostgreSQL, and MariaDB.
- **Breaking Change Policy**: Semver; trait changes are breaking.

### 7.2 External Integration Contracts

#### GitHub REST API Contract

- [ ] `p1` - **ID**: `cpt-cf-github-mirror-contract-github-rest`

- **Direction**: required from client (the gear consumes GitHub REST API)
- **Protocol/Format**: HTTPS/REST, JSON, Link-header pagination, ETag/Last-Modified conditional headers, `X-RateLimit-*` response headers
- **Compatibility**: GitHub REST API v3 (2022+ stable schema); tracks `X-GitHub-Api-Version` header

#### GitHub GraphQL API Contract

- [ ] `p3` - **ID**: `cpt-cf-github-mirror-contract-github-graphql`

- **Direction**: required from client (the gear consumes GitHub GraphQL API)
- **Protocol/Format**: HTTPS/POST to `/graphql`, JSON, cursor-based pagination, point-based rate limiting
- **Compatibility**: GitHub GraphQL API v4

#### Mirror API Contract

- [ ] `p1` - **ID**: `cpt-cf-github-mirror-contract-mirror-api`

- **Direction**: provided by the gear
- **Protocol/Format**: HTTP/JSON REST API for downstream clients (GitHub-compatible surface + extended analytics surface)
- **Compatibility**: Backward-compatible additive evolution within a major version

## 8. Use Cases

#### Full Repository Synchronization

- [ ] `p1` - **ID**: `cpt-cf-github-mirror-usecase-full-sync`

**Actor**: `cpt-cf-github-mirror-actor-cli-operator`

**Preconditions**:
- GitHub PAT is available (env/config/flag)
- Storage backend is accessible

**Main Flow**:
1. Consumer runs `sync org/repo`
2. System detects no prior data, runs full index pass
3. System applies change detection (all entities "new" on first run), enqueues detail tasks
4. System refines entities (comments, events, timeline, reactions, reviews, commits, files, statuses)
5. System runs completeness verification and retries failures
6. System outputs summary with entity counts, API usage, and storage metrics

**Postconditions**: All configured entities stored; synchronization report shows pass/fail per entity type.

**Alternative Flows**:
- **Token invalid**: error at step 2
- **Rate limit exhausted**: system pauses, waits for reset, resumes
- **Interruption**: next `sync` or `resume` picks up via cache

#### Incremental Refresh

- [ ] `p1` - **ID**: `cpt-cf-github-mirror-usecase-incremental-refresh`

**Actor**: `cpt-cf-github-mirror-actor-lib-consumer`

**Preconditions**:
- Prior synchronization exists with cached data

**Main Flow**:
1. Consumer calls `sync org/repo` again (no special incremental flag)
2. System runs incremental index using conditional requests (ETags, etc)
3. System compares fingerprints, skips unchanged entities
4. System refines only changed/new/incomplete entities
5. System returns updated report

**Postconditions**: API call count reduced by >= 90% compared to full synchronization.

#### Write-Back Operation

- [ ] `p3` - **ID**: `cpt-cf-github-mirror-usecase-write-back`

**Actor**: `cpt-cf-github-mirror-actor-api-consumer`

**Preconditions**:
- Mirror API is running with at least one synchronized repository
- Consumer is authenticated

**Main Flow**:
1. Consumer sends a write-back request (e.g., merge PR #42) to the mirror API
2. System validates the request and enqueues the operation
3. System returns the operation ID
4. System executes the operation when GitHub API capacity is available
5. Consumer queries the operation status and retrieves the result

**Postconditions**: The operation is executed on GitHub; the result is stored and queryable.

**Alternative Flows**:
- **GitHub unavailable**: operation remains queued; executed when GitHub is reachable
- **Operation fails**: retried with exponential backoff; marked permanently failed after max retries

#### Dashboard Consuming Mirror API

- [ ] `p1` - **ID**: `cpt-cf-github-mirror-usecase-dashboard`

**Actor**: `cpt-cf-github-mirror-actor-downstream-client`

**Preconditions**:
- Mirror API is running with synchronized repositories

**Main Flow**:
1. Dashboard queries the GitHub-compatible API (e.g., list open PRs across repos)
2. System returns data from the local store with zero GitHub API calls
3. Dashboard renders data with no rate-limit concerns

**Postconditions**: Dashboard operates with unlimited read throughput and zero GitHub API consumption.

## 9. Acceptance Criteria

> Checked state tracks the **full product**. For what the current increment already
> delivers (several criteria are partially met by it), see
> [§4.3 Increment Scope](#43-increment-scope-current-state) and the crate README.

- [ ] Full synchronization of a repository with 10,000+ issues and 5000+ PRs completes with 100% entity coverage
- [ ] Repeated `sync repo` reduces API request count by at least 90% via ETag-based conditional requests and change detection
- [ ] Interrupted synchronization resumes from last checkpoint without re-fetching cached unchanged data
- [ ] Synchronization of a repository with 100,000+ issues completes without memory exhaustion or data corruption
- [ ] Python bindings execute the full pipeline (`init_engine`, `sync_repo`, `query_entities`, etc.) and produce equivalent results to the Rust API
- [ ] Rich pytest test suite covers the full pipeline through the Python wrapper
- [ ] Library crate achieves >= 85% line coverage
- [ ] Zero token exposure in any log output, cache file, database record, or telemetry log
- [ ] Library makes no environment decisions (no env vars, no stdout/stderr without callback)
- [ ] GitHub-compatible REST API serves all synchronized entity types with schema-compatible responses
- [ ] Extended API supports cross-repository queries, `extracted_since` filtering, and logical conversation grouping
- [ ] Write-back operations are enqueued, executed when capacity is available, and results are queryable
- [ ] Multi-tenancy isolates data between tenants in both server and CLI modes
- [ ] Token pool distributes requests across multiple tokens and handles rotation on near-exhaustion
- [ ] Derived contributors are populated with zero additional GitHub API requests
- [ ] CLI provides sync, resume, query, status, check-rate-limit, and clear-cache commands with table/JSON output

## 10. Dependencies

| Dependency | Description | Criticality |
|------------|-------------|-------------|
| GitHub REST API v3 | Primary upstream data source for synchronization and write-back | p1 |
| GitHub GraphQL API v4 | Alternative data source for cost-optimized batch queries | p2 |
| AuthN Resolver gear | Validates bearer tokens and produces `SecurityContext` for protected HTTP and CLI entry paths | p1 |
| AuthZ Resolver gear | Provides authorization decisions and query constraints for API reads, sync control, and write-back operations | p1 |
| Tenant Resolver gear | Provides tenant hierarchy, subtree, and barrier semantics used by multi-tenant authorization flows | p1 |
| Resource Group Resolver gear | Provides resource-group hierarchy and membership inputs when authorization policies are group-scoped | p2 |
| Credential Store gear | Secure storage and retrieval of GitHub tokens and secrets | p2 |
| Event Broker gear (future) | Event delivery for entity lifecycle state changes | p2 |

## 11. Assumptions

- GitHub REST API v3 and GraphQL API v4 remain stable and backward-compatible for the entity types synchronized
- Valid GitHub PAT(s) with appropriate scopes are provided by the caller (CLI, Python, or library consumer); the library never reads environment variables for tokens
- The configured storage backend is available and writable at synchronization and API-serving time
- GitHub rate limits follow documented behavior: 5,000 requests/hour for authenticated REST, point-based budget for GraphQL
- Secondary rate limits are triggered by concurrency/volume, not request content
- The host machine has sufficient disk space and memory for cache and database
- Python consumers have a compatible Python version (3.8+) and can install maturin-built wheels
- The gears ORM (SeaORM) supports all required DDL/DML across SQLite, PostgreSQL, and MariaDB without engine-specific SQL in business logic

## 12. Risks

| Risk | Impact | Mitigation |
|------|--------|------------|
| GitHub secondary rate limits triggered despite throttling | Synchronization halts for minutes to hours | Conservative concurrency defaults, exponential backoff, `Retry-After` compliance; configurable parallelism |
| GitHub API schema changes break deserialization | Synchronization fails or produces incorrect data | Store raw JSON before normalization; schema versioning in cache metadata; integration tests against live API |
| Large repository synchronization exhausts disk or memory | Process crash or corrupt output | Stream pages to disk immediately; bounded in-memory buffers; per-entity processing |
| ETag behavior varies across GitHub endpoints | Reduced cache hit rate | Fall back to timestamp-based diffing when ETags absent; log cache miss reasons |
| PyO3/maturin compatibility issues across Python versions | Bindings fail on some platforms | Test against Python 3.8–3.12; use maturin CI integration; publish multi-platform wheels |
| Database engine differences cause upsert bugs | Data corruption on specific engines | Abstract engine-specific SQL via SeaORM traits; per-engine integration tests; use transactions |
| Write-back operations conflict with concurrent GitHub changes | Unexpected merge conflicts or stale data | Record operation preconditions; support idempotent retries; refresh mirror state after write-back execution |
| Token pool tokens with different scopes cause inconsistent access | Some entities fail to synchronize for some tokens | Per-token scope annotations; scope-aware request routing; log scope mismatches |
| Multi-tenant data isolation breach | Cross-tenant data exposure | Tenant-scoped database queries; tenant-aware caching; security review and testing |

## 13. Open Questions

- What is the maximum supported number of tokens in a token pool, and how is token rotation prioritized?
- How are write-back operation conflicts resolved when the same entity is modified both locally and on GitHub between synchronization cycles?
- What is the retention policy for write-back operation history (completed/failed operations)?
- How is the GitHub-compatible API versioned when GitHub changes their API schema and the mirror's response format must be updated?
- What is the maximum number of repositories that can be synchronized in a single tenant?

## 14. Traceability

Design docs (DESIGN.md, ADRs, feature specs) have not been ported into this tree yet;
they land with the follow-up increments listed in
[§4.3 Increment Scope](#43-increment-scope-current-state). Until then this PRD is the
single in-tree source of truth for the gear.