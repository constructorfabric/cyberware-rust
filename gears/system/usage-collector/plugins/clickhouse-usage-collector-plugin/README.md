# ClickHouse Usage Collector Plugin

ClickHouse storage-backend plugin that implements the Usage Collector `UsageCollectorPluginV1` SPI. It is the durable system of record for usage records and the usage-type catalog: the Usage Collector gateway gear discovers it via the types registry and dispatches all persistence to it. The plugin owns nothing of the host's domain logic — it is pure persistence over a ClickHouse columnar OLAP database, with no coordination backend: every write is a plain read-then-insert whose convergence rests on the engine's `insert_deduplication_token` window, `ReplacingMergeTree(version)` merges, and — on the hot `list`/`aggregate` reads — an anti-join against the deactivation-marker ids rather than read-time version resolution.

## Configuration

Config maps to `ClickHousePluginConfig` (`src/config.rs`). Durations are whole seconds (repo convention).

| Key | Default | Description |
| --- | --- | --- |
| `database_url` | _(required)_ | ClickHouse HTTP endpoint URL including credentials, e.g. `https://user:${CH_PASSWORD}@host:8443/db`. Wrapped in `SecretFromEnv` (Debug-redacted, no Display/Serialize); `${VAR}` placeholders are expanded at startup. Only the `http` and `https` schemes are accepted; a plaintext `http://` URL is additionally rejected unless `allow_insecure_http = true` (see [TLS enforcement](#tls-enforcement) below). |
| `allow_insecure_http` | `false` | Explicit development/test opt-out that permits a plaintext `http://` `database_url`. Has no effect on a `https://` URL, and does not admit a non-HTTP scheme. **MUST NOT** be set in production. |
| `request_timeout_secs` | `30` | Per-request timeout budget in seconds (reads and writes). Drives three mechanisms: the ClickHouse *server* settings `send_timeout`/`receive_timeout`; a *client-side* deadline 5s later on every individual ClickHouse await; and, while `async_insert` is on, the budget an `INSERT` has to absorb the server-side buffer flush (which is why startup validation requires ≥ 2s in that case). The client-side one is the backstop for a connection that is accepted and then never answered (or held open by an intermediary), which the server settings cannot bound because they never reach a server. Sized 5s apart so a responsive server's own timeout fires first and callers get its descriptive error. |
| `async_insert` | `true` | Send single-row `usage_records` `INSERT`s with the ClickHouse settings `async_insert = 1` and `wait_for_async_insert = 1`, applied **per statement** so no `SELECT` is affected. The server buffers and coalesces concurrent inserts into shared parts instead of writing one small part per request, which is what keeps `ReplacingMergeTree` part count and merge pressure down under a request-shaped ingest stream — the write path is one `INSERT` per `create_usage_record`. `wait_for_async_insert = 1` is pinned, not separately configurable: without it an acknowledged record can be lost on a server restart, and `create_usage_record`'s dedup pre-read would stop seeing its own prior insert, so a retry would insert a second row under an idempotency key already in use. The flush wait is bounded by the server-side `async_insert_busy_timeout_ms` (adaptive 50-200ms on ClickHouse 24.x+) and is charged against `request_timeout_secs`, which must therefore be ≥ 2s while this is enabled (startup validation). The busy timeout itself is deliberately **not** exposed — it is a cluster-wide property the server adapts on its own; pin it in a ClickHouse settings profile for the plugin's DB user if you must. **Scope:** multi-row `INSERT`s (`create_usage_records` and the deactivation cascade) and all `usage_type_catalog` writes stay synchronous regardless of this setting — see [Asynchronous inserts](#asynchronous-inserts). **Dedup interaction:** every `usage_records` `INSERT` carries an `insert_deduplication_token`; ClickHouse enforces it on synchronous inserts, and on asynchronous inserts only for `Replicated*` tables, so with this on a racing duplicate single-record create is collapsed by `optimize_on_insert` when both land in one flush and by merge otherwise (visible twice to `list`/`aggregate` until then). Set to `false` for deterministic engine-side dedup of single creates at the cost of one part per request. |
| `retention_period_secs` | `31536000` (365d) | `usage_records` retention window; rows older than this are dropped via ClickHouse TTL. Must be in `(0, 100 years]`. Migration DDL defaults to 1 year; on every startup `ensure_retention_ttl` issues `ALTER TABLE … MODIFY TTL` when the live interval differs from this value — see [Retention window management](#retention-window-management). |
| `vendor` | `cyberfabric` | Vendor name for GTS instance registration. Must not be empty or blank; an empty value fails startup validation. |
| `priority` | `10` | Plugin priority (lower = higher precedence when multiple plugins are registered). |

```yaml
gears:
  clickhouse-usage-collector-plugin:
    config:
      database_url: "https://user:${CH_PASSWORD}@host:8443/usage"
      request_timeout_secs: 30
      async_insert: true
      retention_period_secs: 31536000
      vendor: "cyberfabric"
      priority: 10
```

The plugin's only gear dependency is `types-registry` (for the registration handshake). It does not require the `cluster` gear.

## Operational requirements

### TLS enforcement

- `database_url` embeds ClickHouse credentials and is the transport for every usage record, so a plaintext `http://` connection is a credential- and data-exposure risk, not just a style choice.
- On startup, `ClickHousePluginConfig::validate` (called before any connection is made) rejects a `http://` `database_url` **unless** `allow_insecure_http = true` is set explicitly. A `https://` URL never needs the override.
- The check reads the scheme of the *parsed* URL, which is normalized to lowercase, so `HTTP://` is gated exactly like `http://` rather than slipping past a raw string comparison.
- `validate` also rejects any scheme other than `http`/`https` — including the native-protocol `clickhouse://` and `tcp://` forms — since this plugin talks to ClickHouse's HTTP interface only. That is a separate failure from the TLS gate: `allow_insecure_http` is consent to skip TLS, not consent to an unusable scheme. Only the offending scheme appears in the error; the DSN never does.
- `allow_insecure_http` exists for local development/test against an unencrypted ClickHouse instance (e.g. a Docker test container) — it **MUST NOT** be set in production.
- Even with the override, `build_client` still emits a `tracing::warn!` on every plaintext connection so operators have a durable, per-startup signal that TLS is off.

### Retention window management

- `migrations/0001_init.sql` creates `usage_records` with a fixed 1-year TTL default (`INTERVAL 31536000 SECOND`).
- On every plugin `init`, after `apply_migrations`, `ensure_retention_ttl` reads the live TTL from `system.tables.create_table_query` and compares it to `retention_period_secs`. When missing or different, it runs:

      ALTER TABLE usage_records MODIFY TTL created_at + INTERVAL <n> SECOND DELETE

- Changing `retention_period_secs` in config and restarting therefore updates the table TTL automatically; no manual operator `ALTER` is required for retention changes.
- Verify the effective clause with `SHOW CREATE TABLE usage_records` (ClickHouse may rewrite `INTERVAL <n> SECOND` as `toIntervalSecond(<n>)`).
- `usage_type_catalog` carries no TTL clause and is never retention-bounded.
- ClickHouse applies TTL eviction asynchronously during background merges, so rows can outlive the threshold for a while; expiry is not a synchronous delete.

### Data-skipping index management

Same class of gotcha for indexes (not TTL): everything in `CREATE TABLE IF NOT EXISTS` other than what `ensure_retention_ttl` and `ensure_insert_dedup_window` reconcile applies **only at first provisioning**.

- `migrations/0001_init.sql` declares two `bloom_filter` data-skipping indexes on `usage_records` — `idx_records_id` on `id` and `idx_records_corrects_id` on `corrects_id` — so that `get_usage_record` (`WHERE id = ?`) and the deactivation cascade (`WHERE id = ? OR corrects_id = ?`) prune granules instead of scanning the table. The `ORDER BY` key is deliberately unchanged, since it is the dedup identity `ReplacingMergeTree` collapses on.
- A deployment provisioned **before** these indexes existed does not get them from a restart: the DDL re-runs as a no-op. Add them manually, then materialize them over the existing parts:

      ALTER TABLE usage_records ADD INDEX idx_records_id id TYPE bloom_filter GRANULARITY 1
      ALTER TABLE usage_records ADD INDEX idx_records_corrects_id corrects_id TYPE bloom_filter GRANULARITY 1
      ALTER TABLE usage_records MATERIALIZE INDEX idx_records_id
      ALTER TABLE usage_records MATERIALIZE INDEX idx_records_corrects_id

- `MATERIALIZE INDEX` is a background mutation over existing parts; new parts are indexed on write, so read latency improves gradually until it finishes. Verify with `SHOW CREATE TABLE usage_records` and watch `system.mutations`.

### Insert dedup window

- `migrations/0001_init.sql` creates `usage_records` with `SETTINGS non_replicated_deduplication_window = 10000`. Every `usage_records` `INSERT` carries an `insert_deduplication_token` derived from its row ids, and the window is what lets ClickHouse drop a racing identical block instead of storing a duplicate row — `list` and `aggregate` no longer collapse duplicates at read time (see [Storage semantics](#storage-semantics)).
- On every plugin `init`, after `ensure_retention_ttl`, `ensure_insert_dedup_window` reads the live setting from `system.tables.create_table_query` and, when it is missing or differs, runs:

      ALTER TABLE usage_records MODIFY SETTING non_replicated_deduplication_window = 10000

  `MODIFY SETTING` is metadata-only, so a deployment provisioned before the setting existed picks it up on its next restart with no manual step.
- On a table provisioned as `ReplicatedReplacingMergeTree` the setting is accepted but inert: `replicated_deduplication_window` governs, and asynchronous inserts are deduplicated by token too (`async_insert_deduplicate = 1` is already sent).
- Verify the token is on the wire:

      SYSTEM FLUSH LOGS;
      SELECT event_time, Settings['insert_deduplication_token'], written_rows
      FROM system.query_log
      WHERE type = 'QueryFinish' AND query_kind = 'Insert'
        AND positionCaseInsensitive(query, 'usage_records') > 0
      ORDER BY event_time DESC LIMIT 5

  A deduplicated block shows `written_rows = 0` for the losing insert.

### Asynchronous inserts

`async_insert` (default `true`) moves part formation for **single-row** `usage_records` writes into a server-side buffer that coalesces concurrent inserts into shared parts. The write path is one `INSERT` per `create_usage_record`, so without it a request-shaped ingest stream writes one small part per record and drives `ReplacingMergeTree` part count and merge pressure up.

- **Dedup token.** Every `usage_records` `INSERT` carries `insert_deduplication_token` (UUIDv5 of its sorted row ids, namespaced so a deactivation-marker write never collides with the create of the same ids) and the table keeps `non_replicated_deduplication_window = 10000` (`ensure_insert_dedup_window` retrofits it on startup). The token is enforced on synchronous inserts — batches, marker writes, and single creates when `async_insert = false`. Async inserts are deduplicated by token only on `Replicated*` engines (`async_insert_deduplicate = 1` is sent for that case); on the non-replicated default, two racing identical single creates that land in one flush still collapse through `optimize_on_insert`, and otherwise at merge.

- **Scope.** Applied per statement, on the `INSERT` only, so no `SELECT` plan changes. Three write sites are deliberately left **synchronous**:
  - the multi-row `INSERT` behind `create_usage_records`, and
  - the deactivation cascade's marker write, because both depend on all of a statement's rows becoming visible together and the async buffer does not guarantee that (`ch_deactivation_cascade_is_atomic` fails when they go through it); and
  - every `usage_type_catalog` write, which is control-plane traffic with no concurrent stream to coalesce and no part count to reduce.
- **`wait_for_async_insert = 1` is pinned.** With `0`, `create_usage_record` would return before its row was committed — losing acknowledged records on a restart, and breaking the dedup pre-read's read-your-writes so a retry inserts a second row under a used idempotency key. There is no config field for it.
- **Latency.** Each insert now waits for its buffer to flush, bounded server-side by `async_insert_busy_timeout_ms` (adaptive 50-200ms on ClickHouse 24.x+). Watch `uc_clickhouse_insert_duration_seconds{mode="single"}` across the switch: a small rise is expected and correct; a rise anywhere near `request_timeout_secs` means the busy timeout is misconfigured server-side. Pin it in a settings profile for the plugin's DB user if you must — it is not exposed as plugin config because it is a cluster-wide property the server adapts on its own.
- **Verifying it is on the wire:**

      SYSTEM FLUSH LOGS;
      SELECT event_time, Settings['async_insert'], query_duration_ms
      FROM system.query_log
      WHERE type = 'QueryFinish' AND query_kind = 'Insert'
        AND positionCaseInsensitive(query, 'usage_records') > 0
      ORDER BY event_time DESC LIMIT 5

  Note `Settings` records only values that **differ from the server default**, so `wait_for_async_insert = 1` shows as absent (its default is already `1`) — check `system.settings` for the effective value rather than reading the blank as "not sent".
- **Confirming the queue coalesces**, and the part-count improvement it buys (run under concurrent ingest — a single writer shows no difference):

      SELECT query, first_update, total_bytes FROM system.asynchronous_inserts;

      SELECT partition, count() AS parts, sum(rows) AS rows,
             round(sum(rows) / count()) AS avg_rows_per_part
      FROM system.parts
      WHERE database = currentDatabase() AND table = 'usage_records' AND active
      GROUP BY partition ORDER BY partition

  Expect `parts` to fall sharply and `avg_rows_per_part` to rise for the current month. Corroborate merge pressure with `system.merges` and `system.part_log` (`event_type = 'MergeParts'`).

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

- **Deduplication** — application-level read-before-insert, then the engine's `insert_deduplication_token` window, then `ReplacingMergeTree(version)` merges as the backstop. Nothing serializes concurrent creates: two callers with the same dedup key can both pass the pre-read and both insert, and both get `Ok`. Because the record `id` is derived from the dedup tuple, both inserts carry the same token and — on a synchronous insert — the engine drops the second block (**first-writer-wins**, no `IdempotencyConflict`). With `async_insert` on (the default) the twins collapse through `optimize_on_insert` when they land in one flush and otherwise share a sort key until the merge; `get` resolves to the higher `version` either way, while `list`/`aggregate` see the twin twice until then. Once the earlier row is visible, a later create sees it and the usual absorb / conflict rules apply. See DESIGN.md §3.6.
- **Batch ingest** — three statements per `create_usage_records` call regardless of how many usage types it spans: one catalog existence query over the batch's distinct `gts_id`s, one dedup pre-read over every record that passed it, one multi-row `INSERT`. That `INSERT` commits **one part per partition it touches**, and `usage_records` is `PARTITION BY toYYYYMM(created_at)` — so a reader sees all of a batch's new rows or none of them only when the batch falls in a single calendar month; a batch spanning two months is two commits and can be observed part-way through. The statement is kept synchronous (never `async_insert`) precisely so this is the only window — see [Asynchronous inserts](#asynchronous-inserts).
- **Consistency profile** — on a single-node deployment: effectively immediate read-after-write for any reader. On a replicated deployment: bounded by ClickHouse's own replication lag. No read uses the `FINAL` modifier, so a read never waits on — or pays for — a merge. Point reads (`get`, the deactivation cascade) resolve `ReplacingMergeTree` versions in SQL (`ORDER BY version DESC LIMIT 1 BY <sort key>`) over a bloom-filter-pruned candidate set; `list` and `aggregate` do not resolve versions at all — they scan raw rows and exclude the ids that carry a deactivation marker (`id NOT IN (SELECT id … WHERE … AND status = 'inactive')`), which is exact for deactivation before any merge and costs one hash probe per row instead of a sort or hash aggregation over the whole scan.
  For strict cross-replica read-your-writes, configure ClickHouse's native `insert_quorum` write-quorum setting on the server; this plugin does not enable it by default and enabling it incurs a proportional write-latency cost — see DESIGN.md §3.8.
- **Workload isolation** — the ClickHouse client/pool is shared by both the ingestion and query paths (v1 design), so query bursts can degrade ingestion throughput; see [Workload isolation and pool contention](#workload-isolation-and-pool-contention).
- **Referential integrity** — application-emulated on both sides, with a known gap on the delete side. Create side: `create_usage_record(s)` checks that every referenced `gts_id` exists in `usage_type_catalog` (an existence read, which needs no version resolution) and rejects absent ones with `UsageTypeNotFound`. Delete side: `delete_usage_type` runs an existence read, then a capped reference probe over `usage_records` (refusing with `UsageTypeReferenced` → HTTP 409 if any row references the type), then removes the catalog row with `ALTER TABLE … DELETE` under `mutations_sync = 1`, then re-probes and sweeps any record that landed in between. **This is not race-free.** ClickHouse has no foreign key and this plugin has no coordination primitive, so an insert whose own catalog check passed before the catalog row was removed can commit after the sweep and orphan a row — `plugin-spi.md` Method 9's "MUST NOT admit a window" clause is *not* satisfied by this backend. The window is bounded, not closed: the catalog row is deleted **before** the sweep on purpose, so from that moment the insert-time check refuses new records on its own. `async_insert` (the default) widens the window to the server-side flush interval. Watch `uc_clickhouse_orphaned_reference_detected_total` — a non-zero rate means deletes are being run against types with live ingest. Prefer deleting a type while no ingest for it is in flight. See DESIGN.md §3.6.
- **Retention** — ClickHouse TTL clause on `usage_records`: fixed 1-year default in `CREATE TABLE`, then reconciled to `retention_period_secs` on every startup via `ensure_retention_ttl` (`ALTER TABLE … MODIFY TTL` when needed) — see [Retention window management](#retention-window-management).
- **Error classification** — a failure is reported as retryable `Transient` when the client could not reach ClickHouse at all (network, timeout, compression), when the client-side deadline expires, or when ClickHouse itself answers with one of a fixed allowlist of overload/backpressure codes: `159` `TIMEOUT_EXCEEDED`, `202` `TOO_MANY_SIMULTANEOUS_QUERIES`, `203` `NO_FREE_CONNECTION`, `209` `SOCKET_TIMEOUT`, `210` `NETWORK_ERROR`, `252` `TOO_MANY_PARTS`, `279` `ALL_CONNECTION_TRIES_FAILED`, `285` `TOO_FEW_LIVE_REPLICAS`, `999` `KEEPER_EXCEPTION` — plus HTTP `502`/`503`/`504` when ClickHouse returned no readable body (typically an intermediary). Anything else, including `241` `MEMORY_LIMIT_EXCEEDED` (permanent for an over-large batch, so retrying it would loop) and `319` `UNKNOWN_STATUS_OF_INSERT`, is `Internal`. Only the unreachable-backend cases clear the readiness gauge: a server that answers with backpressure is degraded, not down.
- **Deactivation** — applied as a single multi-row `INSERT` of versioned marker rows (depth-1 only); no `UPDATE` or `ALTER TABLE … DELETE` is issued on the request path. A plain retry of a record that races its own deactivation and misses the original row at pre-read can re-insert it as active with a higher version (the host's Method 5 rule protects compensations, not retries); see DESIGN.md §3.6.
- **Schema** — provisioned idempotently at startup via `CREATE TABLE IF NOT EXISTS`; indexes still apply only at first provisioning (see [Data-skipping index management](#data-skipping-index-management)), while TTL is updated on startup when config differs.

## SPI conformance

The crate implements `usage_collector_sdk::UsageCollectorPluginV1` (via `StorageAdapter` over the record and catalog stores). Conformance is enforced at compile time. All ten SPI methods are implemented. One does not meet its contract in full: `delete_usage_type` emulates `ON DELETE RESTRICT` without a foreign key and admits a race the SPI forbids — see [Storage semantics](#storage-semantics) and the deviations table in `docs/PRD.md`.

## Running integration tests

The real-DB suites are gated behind the `clickhouse` feature and require Docker for a ClickHouse image:

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
API that both plugins must satisfy, `delete_usage_type` included: the
referenced-type delete seam
(`test_delete_usage_type_referenced_by_record_is_rejected`) runs unmodified on
both backends, since the uncontended contract — 409 for a referenced type, 204
for an unreferenced one — is identical either way. The race this backend admits
between probe and delete is not reachable from a single-threaded HTTP test and
is covered by documentation rather than by an E2E assertion.

Config lives in `testing/e2e/suites/usage_collector/config-clickhouse.yaml`;
the container is managed by `ClickHouseSidecar` in
`testing/e2e/lib/sidecars.py`, whose image tag must stay in sync with
`tests/common/mod.rs`.
