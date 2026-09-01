# ClickHouse Usage Collector Plugin

ClickHouse storage-backend plugin that implements the Usage Collector `UsageCollectorPluginV1` SPI. It is the durable system of record for usage records and the usage-type catalog: the Usage Collector gateway gear discovers it via the types registry and dispatches all persistence to it. The plugin owns nothing of the host's domain logic — it is pure persistence over a ClickHouse columnar OLAP database, with the **cluster gear** `DistributedLockV1` (profile `usage-collector`) backing the per-`gts_id` exclusive coordination lock.

## Configuration

Config maps to `ClickHousePluginConfig` (`src/config.rs`). Durations are whole seconds (repo convention).

| Key | Default | Description |
| --- | --- | --- |
| `database_url` | _(required)_ | ClickHouse HTTP endpoint URL including credentials, e.g. `https://user:${CH_PASSWORD}@host:8443/db`. Wrapped in `SecretFromEnv` (Debug-redacted, no Display/Serialize); `${VAR}` placeholders are expanded at startup. Only the `http` and `https` schemes are accepted; a plaintext `http://` URL is additionally rejected unless `allow_insecure_http = true` (see [TLS enforcement](#tls-enforcement) below). |
| `allow_insecure_http` | `false` | Explicit development/test opt-out that permits a plaintext `http://` `database_url`. Has no effect on a `https://` URL, and does not admit a non-HTTP scheme. **MUST NOT** be set in production. |
| `request_timeout_secs` | `30` | Per-request timeout budget in seconds (reads and writes). Drives two mechanisms: the ClickHouse *server* settings `send_timeout`/`receive_timeout`, and a *client-side* deadline 5s later on every individual ClickHouse await. The client-side one is the backstop for a connection that is accepted and then never answered (or held open by an intermediary), which the server settings cannot bound because they never reach a server. Sized 5s apart so a responsive server's own timeout fires first and callers get its descriptive error. |
| `lock_ttl_secs` | `60` | Cluster lock lease TTL. Should exceed worst-case create/delete critical-section latency (ClickHouse I/O while the lock is held). Renewed immediately before the mutating write. Startup validation enforces the floor that makes that renew useful: `lock_ttl_secs` **must be strictly greater than** the client deadline (`request_timeout_secs + 5s`, so `35s` at the default), because a single ClickHouse round-trip may burn the whole deadline and must not outlive the lease it was just granted. Raising `request_timeout_secs` therefore requires raising `lock_ttl_secs` with it. |
| `lock_timeout_secs` | `5` | Maximum wait when acquiring the per-`gts_id` exclusive cluster lock. On timeout the operation fails closed with `Transient`. |
| `retention_period_secs` | `31536000` (365d) | `usage_records` retention window; rows older than this are dropped via ClickHouse TTL. Must be in `(0, 100 years]`. Migration DDL defaults to 1 year; on every startup `ensure_retention_ttl` issues `ALTER TABLE … MODIFY TTL` when the live interval differs from this value — see [Retention window management](#retention-window-management). |
| `vendor` | `cyberfabric` | Vendor name for GTS instance registration. Must not be empty or blank; an empty value fails startup validation. |
| `priority` | `10` | Plugin priority (lower = higher precedence when multiple plugins are registered). |

```yaml
gears:
  cluster:
    config:
      profiles:
        usage-collector:
          cache: { provider: standalone }
  clickhouse-usage-collector-plugin:
    config:
      database_url: "https://user:${CH_PASSWORD}@host:8443/usage"
      request_timeout_secs: 30
      lock_ttl_secs: 60
      lock_timeout_secs: 5
      retention_period_secs: 31536000
      vendor: "cyberfabric"
      priority: 10
```

## Operational requirements

### TLS enforcement

- `database_url` embeds ClickHouse credentials and is the transport for every usage record, so a plaintext `http://` connection is a credential- and data-exposure risk, not just a style choice.
- On startup, `ClickHousePluginConfig::validate` (called before any connection is made) rejects a `http://` `database_url` **unless** `allow_insecure_http = true` is set explicitly. A `https://` URL never needs the override.
- The check reads the scheme of the *parsed* URL, which is normalized to lowercase, so `HTTP://` is gated exactly like `http://` rather than slipping past a raw string comparison.
- `validate` also rejects any scheme other than `http`/`https` — including the native-protocol `clickhouse://` and `tcp://` forms — since this plugin talks to ClickHouse's HTTP interface only. That is a separate failure from the TLS gate: `allow_insecure_http` is consent to skip TLS, not consent to an unusable scheme. Only the offending scheme appears in the error; the DSN never does.
- `allow_insecure_http` exists for local development/test against an unencrypted ClickHouse instance (e.g. a Docker test container) — it **MUST NOT** be set in production.
- Even with the override, `build_client` still emits a `tracing::warn!` on every plaintext connection so operators have a durable, per-startup signal that TLS is off.

### Cluster distributed-lock dependency

- The plugin requires the **cluster** gear with profile name `usage-collector` (see `UsageCollectorProfile`). A standalone in-process cache provider is sufficient for single-node deployments; multi-node deployments use whatever linearizable lock backend the operator registers for that profile.
- Both `create_usage_record(s)` and `delete_usage_type` acquire the **same exclusive** lock name per `gts_id` (hashed leaf under a `usage-collector/` scope). Concurrent creates for the same `gts_id` therefore serialize.
- Locks are resolved lazily on first acquire (cluster backends register during cluster `start`, after this plugin's `init`).
- If the lock cannot be granted (unbound profile, timeout, provider error), both create and delete fail closed with a retryable `Transient` error rather than proceeding unlocked.
- **ADR-002 deviation**: this plugin holds the cluster lock across ClickHouse remote I/O (required for referential integrity). Call sites `renew` immediately before the mutating write and always `release` explicitly (cluster `LockGuard` drop is a no-op).

For the full lock-model rationale and critical-section sequences, see DESIGN.md §3.5 and PRD.md §5 (`cpt-cf-uc-ch-plugin-fr-referential-integrity`).

### Retention window management

- `migrations/0001_init.sql` creates `usage_records` with a fixed 1-year TTL default (`INTERVAL 31536000 SECOND`).
- On every plugin `init`, after `apply_migrations`, `ensure_retention_ttl` reads the live TTL from `system.tables.create_table_query` and compares it to `retention_period_secs`. When missing or different, it runs:

      ALTER TABLE usage_records MODIFY TTL created_at + INTERVAL <n> SECOND DELETE

- Changing `retention_period_secs` in config and restarting therefore updates the table TTL automatically; no manual operator `ALTER` is required for retention changes.
- Verify the effective clause with `SHOW CREATE TABLE usage_records` (ClickHouse may rewrite `INTERVAL <n> SECOND` as `toIntervalSecond(<n>)`).
- `usage_type_catalog` carries no TTL clause and is never retention-bounded.
- ClickHouse applies TTL eviction asynchronously during background merges, so rows can outlive the threshold for a while; expiry is not a synchronous delete.

### Data-skipping index management

Same class of gotcha for indexes (not TTL): everything in `CREATE TABLE IF NOT EXISTS` other than what `ensure_retention_ttl` reconciles applies **only at first provisioning**.

- `migrations/0001_init.sql` declares two `bloom_filter` data-skipping indexes on `usage_records` — `idx_records_id` on `id` and `idx_records_corrects_id` on `corrects_id` — so that `get_usage_record` (`WHERE id = ?`) and the deactivation cascade (`WHERE id = ? OR corrects_id = ?`) prune granules instead of scanning the table. The `ORDER BY` key is deliberately unchanged, since it is the dedup identity `ReplacingMergeTree` collapses on.
- A deployment provisioned **before** these indexes existed does not get them from a restart: the DDL re-runs as a no-op. Add them manually, then materialize them over the existing parts:

      ALTER TABLE usage_records ADD INDEX idx_records_id id TYPE bloom_filter GRANULARITY 1
      ALTER TABLE usage_records ADD INDEX idx_records_corrects_id corrects_id TYPE bloom_filter GRANULARITY 1
      ALTER TABLE usage_records MATERIALIZE INDEX idx_records_id
      ALTER TABLE usage_records MATERIALIZE INDEX idx_records_corrects_id

- `MATERIALIZE INDEX` is a background mutation over existing parts; new parts are indexed on write, so read latency improves gradually until it finishes. Verify with `SHOW CREATE TABLE usage_records` and watch `system.mutations`.

### Workload isolation and pool contention

- One `clickhouse::Client` (and therefore one underlying HTTP connection pool) serves **both** the ingestion and the query paths. This is a deliberate v1 choice, not a solved isolation guarantee.
- **The pool is not tunable from config.** There is no `pool_max_connections`-style setting: `clickhouse` 0.15.1 exposes no pool-bound builder (`with_setting`/`with_option` set ClickHouse *server* settings, and the `with_http_client` seam cannot be used from outside the crate), so no config field could drive it. Everything below is therefore an operational mitigation, not a knob.
- Risk to the ingestion-throughput NFR: a burst of aggregation or list queries competes with ingest writes for the same pool and the same ClickHouse server resources, so query bursts can delay pool acquisition on the write path and push ingest below its throughput budget. The plugin has no internal reservation, priority, or queueing that protects ingest from read traffic.
- Operator guidance:
  - Watch `uc_clickhouse_pool_acquire_duration_seconds` together with `uc_clickhouse_insert_duration_seconds{mode="batch"}` and `uc_clickhouse_query_requests_total{query_kind="aggregated"}`: rising pool-acquire time correlated with query volume is this contention, not a ClickHouse slowdown.
  - Bound read cost server-side with ClickHouse's own controls (settings profiles / quotas per query user, `max_concurrent_queries_for_user`) so heavy analytical queries cannot consume the whole server.
  - `request_timeout_secs` bounds how long any single starved request waits; sizing it too high lets a query burst hold pool capacity longer.
  - For hard separation, run **two plugin instances** with distinct GTS priorities pointed at different endpoints (write primary vs. read replica). This is an operational workaround; the plugin does not split pools internally.
- A future revision may split ingestion and query onto independently-pooled clients (additive, non-breaking to the SPI); until then this contention point is accepted and documented rather than assumed away.

## Storage semantics

- **Deduplication** — application-level read-before-insert using `ReplacingMergeTree(version)` + `FINAL`. With exclusive locking, concurrent creates for the same `gts_id` are serialized, closing the former same-`gts_id` shared-lock residual race. See DESIGN.md §3.6 and PRD.md §5.
- **Consistency profile** — on a single-node deployment: effectively immediate read-after-write for any reader. On a replicated deployment: bounded by ClickHouse's own replication lag. Every read path uses `FINAL`.
  For strict cross-replica read-your-writes, configure ClickHouse's native `insert_quorum` write-quorum setting on the server; this plugin does not enable it by default and enabling it incurs a proportional write-latency cost — see DESIGN.md §3.8.
- **Workload isolation** — the ClickHouse client/pool is shared by both the ingestion and query paths (v1 design), so query bursts can degrade ingestion throughput; see [Workload isolation and pool contention](#workload-isolation-and-pool-contention).
- **Referential integrity** — the per-`gts_id` exclusive cluster lock serializes every `create_usage_record` call against every `delete_usage_type` call for the same `gts_id`. Deleting a referenced type returns `UsageTypeReferenced`; deleting an unreferenced type removes the row; inserting against an absent/deleted type returns `UsageTypeNotFound`.
  The `DELETE` sets `lightweight_deletes_sync = 2` on the statement, so it waits for the removal to be applied instead of returning while the row is still readable — a re-create of the same `gts_id` must not see it. That wait happens inside the lock's critical section, so operators sizing `lock_ttl_secs` and `request_timeout_secs` should count it. On a replicated deployment `2` means waiting for all replicas: an unavailable replica surfaces as delete latency, and past the lock lease as `Transient`.
- **Retention** — ClickHouse TTL clause on `usage_records`: fixed 1-year default in `CREATE TABLE`, then reconciled to `retention_period_secs` on every startup via `ensure_retention_ttl` (`ALTER TABLE … MODIFY TTL` when needed) — see [Retention window management](#retention-window-management).
- **Error classification** — a failure is reported as retryable `Transient` when the client could not reach ClickHouse at all (network, timeout, compression), when the client-side deadline expires, or when ClickHouse itself answers with one of a fixed allowlist of overload/backpressure codes: `159` `TIMEOUT_EXCEEDED`, `202` `TOO_MANY_SIMULTANEOUS_QUERIES`, `203` `NO_FREE_CONNECTION`, `209` `SOCKET_TIMEOUT`, `210` `NETWORK_ERROR`, `252` `TOO_MANY_PARTS`, `279` `ALL_CONNECTION_TRIES_FAILED`, `285` `TOO_FEW_LIVE_REPLICAS`, `999` `KEEPER_EXCEPTION` — plus HTTP `502`/`503`/`504` when ClickHouse returned no readable body (typically an intermediary). Anything else, including `241` `MEMORY_LIMIT_EXCEEDED` (permanent for an over-large batch, so retrying it would loop) and `319` `UNKNOWN_STATUS_OF_INSERT`, is `Internal`. Only the unreachable-backend cases clear the readiness gauge: a server that answers with backpressure is degraded, not down.
- **Deactivation** — applied as a single multi-row `INSERT` of versioned marker rows (depth-1 only); no `UPDATE` or `ALTER TABLE … DELETE` is issued on the request path.
- **Schema** — provisioned idempotently at startup via `CREATE TABLE IF NOT EXISTS`; indexes still apply only at first provisioning (see [Data-skipping index management](#data-skipping-index-management)), while TTL is updated on startup when config differs.

## SPI conformance

The crate implements `usage_collector_sdk::UsageCollectorPluginV1` (via `StorageAdapter` over the record and catalog stores). Conformance is enforced at compile time.

## Running integration tests

The real-DB suites are gated behind the `clickhouse` feature and require Docker for a ClickHouse image. Cluster locks are registered in-process (no ZooKeeper):

    cargo test -p cf-gears-clickhouse-usage-collector-plugin --features clickhouse

Without the feature, only unit tests run (no Docker needed):

    cargo test -p cf-gears-clickhouse-usage-collector-plugin

## Running E2E tests

The suites above exercise this crate directly. To exercise it as the bound
storage plugin behind the usage-collector gear's HTTP API — over a real server,
through a real ClickHouse container:

    make e2e-usage-collector

That target runs the usage-collector E2E suite twice, once per storage backend
(TimescaleDB, then this one), building a dedicated binary for each: every gear
linked into the server is initialized, and both plugins fail `init` without
their own live database, so they cannot share one process. The test bodies are
shared and speak HTTP only — anything they assert is a contract of the gear's
API that both plugins must satisfy.

Config lives in the **repo-root** `config/e2e-usage-collector-clickhouse.yaml`
(at the monorepo root, not under this plugin); the container is
managed by `ClickHouseSidecar` in `testing/e2e/lib/sidecars.py`, whose image tag
must stay in sync with `tests/common/mod.rs`.
