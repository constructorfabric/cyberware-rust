# Quota Enforcement Coordination Plugin

Default `CoordinationPluginV1` backend for the `quota-enforcement` gear. It
gives the sweeper singletons a TTL-bounded lock per `LockScope`, on the
database the plugin is bound to. No extra infrastructure is needed.

## How it works

One table, `qe_coordination_locks`, one row per scope:

| column | meaning |
|---|---|
| `key` | the `LockScope` key, primary key |
| `holder_id` | current holder, `NULL` when free |
| `locked_until` | database-clock expiry; the lock is live while `locked_until > NOW()` |
| `attempts` | steal counter, for operators |

- `try_lock` inserts a free row or steals an expired one in one serializable
  transaction. A live row answers `Conflict`.
- `renew` pushes `locked_until` forward for the same holder while the row is
  live. An expired or stolen row answers `LockExpired`.
- `release` frees the row for the same holder. It is best-effort.

Time comparisons run on the database clock. Clock drift between replicas can
delay an acquisition. It can never grant two holders at the same time.

## Configuration

```yaml
gears:
  quota-enforcement-coordination-plugin:
    database:
      server: "sqlite_users"
      file: "quota_enforcement_coordination.db"
    config:
      vendor: "constructorfabric"   # must match gears.quota-enforcement.config.coordination_vendor
      priority: 100                 # lower wins when several plugins share a vendor
```

The plugin registers its GTS instance in the types registry at `init` and
publishes a scoped `CoordinationPluginV1` client under that instance id.

## Design source

`gears/system/quota-enforcement/docs/ADR/0006-cpt-cf-quota-enforcement-adr-coordination-plugin.md`
and `docs/features/foundation.md`, "Coordination Default Implementation".
