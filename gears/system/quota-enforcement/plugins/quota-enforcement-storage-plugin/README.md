# Quota Enforcement Storage Plugin

Reference `QuotaEnforcementStoragePluginV1` backend for the
`quota-enforcement` gear, on `toolkit-db` with `SecureConn` only.

## Foundation scope

This crate ships the foundation slice of the storage plugin:

- the schema-version table `qe_schema_meta` and the `bootstrap()` check
  against the contract major (invariant I12),
- the three configuration tables `qe_contention_timeout_config`,
  `qe_lease_capacity_config`, and `qe_idempotency_retention_config`, with
  idempotent seeding of their platform-default rows,
- the migrations for those tables.

The plugin gear binds to its database, applies the migrations, and validates
its configuration. It does **not** yet publish a `QuotaEnforcementStoragePluginV1`
client. The contract names primitives that later features deliver, and the
foundation Definition of Done forbids a partial implementation. The client
registration lands with the last storage primitive.

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

## Design source

`gears/system/quota-enforcement/docs/DESIGN.md`, sections 3.3 and 3.7, and
`docs/features/foundation.md`, "Reference Storage Plugin on toolkit-db".
