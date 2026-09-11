# Static `CredStore` Plugin

`CredStore` **value-store** backend for development and testing: an in-memory
store of immutable secret versions. Implements the `CredStorePluginClientV1`
contract (`get`/`put`/`delete`) so the stateful `credstore` gear can use it as
a backend without a full secrets vault.

## Overview

The `cf-gears-static-credstore-plugin` module provides:

- **A dumb per-tenant key-value store** — entries are keyed by
  `(tenant_id, value_id)`. The plugin knows nothing about references, owners,
  sharing or hierarchy; all of that lives in the gear's metadata row, which
  names the current version through its `value_id` (ADR-0006).
- **Immutable entries** — a `put` on an id that already exists fails with
  `Conflict`; the gear never issues one, and the fence-key bootstrap relies on
  the refusal to settle a race between replicas. `delete` of a missing id is
  a success (idempotent).
- **Writable at runtime** — the gear's write protocol (`put` a fresh version,
  switch the row, `delete` the superseded one) mutates the in-memory store, so
  it works as a development backend, not just a fixture.
- **No config seeding** — values enter the store only through the credstore
  API (`PUT /credstore/v1/credentials/{ref}`), which mints the `value_id` and
  writes the metadata row that makes the value reachable. A `secrets:` block
  in this plugin's config is rejected at startup.

The plugin registers itself via the types registry as a `CredStorePluginClientV1`
implementation and is discovered by the `credstore` gear module.

## Rust usage

`ToolKit` normally discovers and instantiates the plugin through inventory. Direct
construction is useful for host wiring tests:

```rust
use static_credstore_plugin::StaticCredStorePlugin;

let plugin = StaticCredStorePlugin::default();
```

## Configuration

```yaml
static-credstore-plugin:
  config:
    vendor: "constructorfabric"   # GTS vendor name (default: "constructorfabric")
    priority: 100                 # Plugin priority, lower = higher (default: 100)
```

Both keys are GTS-instance registration input; there is nothing else to
configure. Unknown keys — including the former `secrets:` list — fail
validation (`deny_unknown_fields`).

## Contract

The gear calls the plugin with a tenant and an opaque version id:

| Method | Behaviour |
|---|---|
| `get(ctx, tenant_id, value_id)` | Returns the bytes stored under `(tenant_id, value_id)`, or `None`. |
| `put(ctx, tenant_id, value_id, value)` | Stores a new immutable entry; `Conflict` if the id already exists. |
| `delete(ctx, tenant_id, value_id)` | Removes the entry; a missing id is `Ok(())`. |

The reserved fence-key entry lives under the nil tenant and the SDK constant
`FENCE_KEY_VALUE_ID`; no metadata row ever points at it.

## Architecture

```text
gear.rs            ToolKit gear — initialization and GTS/ClientHub registration
config.rs          Config model (vendor, priority)
domain/
  service.rs       In-memory (tenant_id, value_id) → bytes store
  client.rs        CredStorePluginClientV1 adapter
  mod.rs           Domain exports
```

### Init sequence

1. Load `StaticCredStorePluginConfig` from module config
2. Register GTS plugin instance in types-registry
3. Store `Arc<Service>` in module state
4. Register `CredStorePluginClientV1` scoped client in `ClientHub`

## Testing

```bash
cargo test -p cf-gears-static-credstore-plugin
```

The test suite covers:

- `get`/`put`/`delete` round-trips and tenant isolation
- Immutability (`put` on an existing id is `Conflict`) and idempotent `delete`
- Config validation (unknown keys, including a legacy `secrets:` block, are rejected)
- The `CredStorePluginClientV1` trait impl

## License

Apache-2.0
