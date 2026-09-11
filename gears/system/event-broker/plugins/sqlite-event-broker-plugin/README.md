# SQLite event-broker plugin

`cf-gears-sqlite-event-broker-plugin` (lib `sqlite_event_broker_plugin`) is the
durable storage backend for the `event-broker` gear: the append-only
`(topic, partition)` event log, the per-partition bookkeeping that assigns
sequences and dedups outbox retries, and the retention pass that keeps every
partition bounded.

It implements `event_broker_sdk::EventBrokerBackend` and depends on the SDK
only, never on the gear.

## Lifecycle

Not a ToolKit `RunnableCapability` and not a `Gear`. Following the cluster
plugins, it exposes a provider the host gear injects at wiring:

```rust,ignore
let provider = SqliteBackendProvider::new(db);
let backend = provider.build_backend(&serde_json::Map::new()).await?;
```

The backend owns no task and no timer. Its retention pass is a trait method the
gear drives on its own tick, so a test forces a pass deterministically instead
of sleeping and hoping a background thread ran.

## Schema

The gear owns the `DatabaseCapability` seam, so this crate exports its tables
as migrations rather than applying them: `migrations()` returns the
`event_broker_event` and `event_broker_partition_state` definitions for the
gear's own migrator to include.
