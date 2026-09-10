# Quota Enforcement Storage Plugin

Reference `QuotaEnforcementStoragePluginV1` backend for the
`quota-enforcement` gear, on `toolkit-db` with `SecureConn` only.

## Scope

This crate ships the foundation slice and the Quota tables of the storage
plugin:

- the schema-version table `qe_schema_meta` and the `bootstrap()` check
  against the contract major (invariant I12),
- the three configuration tables `qe_contention_timeout_config`,
  `qe_lease_capacity_config`, and `qe_idempotency_retention_config`, with
  idempotent seeding of their platform-default rows,
- the Quota tables `qe_quotas`, `qe_quota_allocation_counters`, and
  `qe_operation_log`, with the four lifecycle primitives (`create_quota`,
  `update_quota`, `deactivate_quota`, `read_quotas`) and the two
  platform-plane reads (`read_active_projection_bindings`,
  `read_active_quota_counts`) of the contract,
- the enqueue side of the notification outbox (invariant I11),
- the migrations for all of it.

The plugin gear binds to its database, applies the migrations, and validates
its configuration. It does **not** yet publish a `QuotaEnforcementStoragePluginV1`
client. The contract names primitives that later features deliver, and the
foundation Definition of Done forbids a partial implementation. The client
registration lands with the last storage primitive. Until then the Quota
primitives are reached through `StoragePlugin` by tests only.

## Quota tables

| table | rows | scope columns |
|---|---|---|
| `qe_quotas` | one per Quota; `id` is a `UUIDv7`, so ascending id is creation order and the list cursor | `tenant_id`, `id` |
| `qe_quota_allocation_counters` | one per allocation Quota, created with it; the in-flight counter the cap guard reads | `tenant_id`, `quota_id` |
| `qe_operation_log` | one per accepted mutation: who did what, content-free | `tenant_id`, `quota_id` |

Every mutation is one transaction: the Quota row, its counter row, the
operation-log row, and the notification events. `update_quota` and
`deactivate_quota` lock the Quota row first (`FOR UPDATE` on PostgreSQL; SQLite
serializes writers), decide on the merged row inside the lock (I6 cap guard,
I14 thresholds versus unbounded cap), and write by compare-and-set on
`record_version`. Caps live in `0..=i64::MAX` under a check constraint; enums
are stored as their GTS instance ids; `notification_thresholds` and `metadata`
are canonical JSON text. `read_quotas` orders by id, clamps the page to 500,
accepts at most 500 explicit ids, and re-applies the caller's scope on every
page: the cursor is the last id, base64url, and grants nothing.

Consumption-operations adds `qe_quota_consumption_counters` and the consumption
arm of the cap guard; lease-operations adds the lease tables and fills the
deactivation cascade, which today resolves no lease.

## Notification outbox

The toolkit outbox runs under the table prefix `qe_outbox` (tables
`qe_outbox_body`, `qe_outbox_incoming`, ...). Events go to the queue
`qe_notifications` over eight partitions, one tenant always on one partition,
with the event kind as the payload type. The plugin only enqueues: the handle
(`StoragePluginGear::notification_outbox()`) stays unbound until the
notification dispatcher starts the pipeline and binds it. An unbound handle
fails every mutation as `Unavailable` and the transaction rolls back, so no
mutation can commit without its events.

## Configuration tables

Rows use sentinel keys instead of `NULL` so every table has a real primary key:

| key value | meaning |
|---|---|
| `*` | platform default row, seeded at bootstrap |
| any other value | a per-metric or per-tenant override |

The tables are operator configuration, not tenant data. They are declared
without tenant scoping and read by the plugin under `AccessScope::allow_all()`.

## Configuration

```yaml
gears:
  quota-enforcement-storage-plugin:
    database:
      server: "sqlite_users"
      file: "quota_enforcement.db"
    config:
      vendor: "constructorfabric"   # must match gears.quota-enforcement.config.storage_vendor
      priority: 100
```

## Tests

The unit and store suites run on in-memory SQLite. The concurrency suite runs
on a PostgreSQL container and is compiled out unless asked for:

```bash
cargo test -p cf-gears-quota-enforcement-storage-plugin --features postgres --test quota_store_integration_pg
```

## Design source

`gears/system/quota-enforcement/docs/DESIGN.md`, sections 3.3 and 3.7,
`docs/features/foundation.md`, "Reference Storage Plugin on toolkit-db", and
`docs/features/quota-lifecycle.md`.
