# Quota Enforcement

Main gear of the Quota Enforcement (QE) system. This crate holds the
foundation slice: PDP-gated admission, plugin binding, fail-closed bootstrap,
readiness reporting, and the mount point of the REST surface in `api-gateway`.

## What the foundation delivers

- **Admission.** One `admit()` boundary runs the public shape check, calls
  `authz-resolver` through `PolicyEnforcer`, and passes the returned
  `AccessScope` through unchanged. Denial, compile failure, and an unreachable
  PDP all fail closed. QE keeps no decision cache.
- **Plugin binding.** The storage and coordination plugins are selected by
  vendor through the types registry and resolved as scoped `ClientHub`
  clients. Nothing is cached before bootstrap succeeds.
- **Bootstrap.** In the lifecycle entry, before the ready signal: resolve the
  storage plugin, run its `bootstrap()`, resolve the coordination plugin,
  probe every `LockScope` with `try_lock` + `release`, and confirm the PDP
  client. Any failure keeps the gear out of rotation and names the dependency
  in `/health`.
- **Telemetry conventions.** Instruments come from the PRD section 5.16
  catalogue only. Label values are closed enums. No tenant, subject, quota,
  policy, key, token, or caller identifier is a label.

Operational routes land with their features. The route registration function
exists so every feature mounts through the same entry point.

## Configuration

```yaml
gears:
  quota-enforcement:
    config:
      storage_vendor: "constructorfabric"        # selects the storage plugin
      coordination_vendor: "constructorfabric"   # selects the coordination plugin
      probe_lock_ttl_secs: 5                     # TTL of the bootstrap lock probe
      metrics:
        prefix: ""                               # empty: catalogue names verbatim
```

The gear owns no database. Both plugins declare `db` and get their own
binding.

## Runtime state of the foundation

The storage plugin does not publish a `QuotaEnforcementStoragePluginV1`
client until every primitive of the contract exists. Until then bootstrap
fails at "storage plugin client not registered" in a live server, by design.
The bootstrap path is exercised in tests against the SDK's complete
in-memory doubles.

## Design source

`gears/system/quota-enforcement/docs/features/foundation.md` and
`docs/DESIGN.md`, sections 3.2 (Gateway) and 3.7 (Bootstrap seeded state).
