# Gear Orchestrator Gear

System gear for service discovery.

## Overview

The `cf-gears-ochestrator` crate implements the `gear_orchestrator` gear.

It:

- Registers `DirectoryClient` in `ClientHub` for in-process gears
- Exposes the `DirectoryService` gRPC service (via `grpc-hub`)
- Uses the runtime `GearManager` for instance tracking and service resolution

## Registration authorization

`RegisterInstance` / `DeregisterInstance` / `Heartbeat` authorize the
authenticated platform-plane peer against the `gear_name` it claims. Per-gear
binding applies only to authenticated per-gear identities (SA-per-gear /
SPIFFE), which may act only on their own gear (or a `trusted_registrars` entry);
a shared-secret identity (`PlatformIdentity::Shared`) is authorized for any
gear. See the `DirectoryServiceImpl` docs and `cpt-cf-adr-platform-plane-auth`
for the mechanism.

A request with no `PlatformSecurityContext` is handled per the listener's auth
posture, which rides the request itself: the platform-plane enforcement layer
(`grpc-hub`'s `InternalAuthGrpcLayer`) stamps a `PlatformAuthEnforced` marker on
every non-exempt request whenever enforcement is active. There is no orchestrator
knob to keep in sync, and it never reads another gear's config:

- **Marker present** (the hub enforces platform auth and let an anonymous caller
  through under `Permissive`) — the token-less request is rejected
  (`unauthenticated`). This prevents a `Permissive` listener from *inverting* the
  incentive, where an anonymous caller (no token, so no stamped identity) could
  act on any gear while an honest per-gear token holder is bound to its own.
- **Marker absent** (Profile 1 / in-process, enforcement disabled) —
  authorization is skipped (fails open); the process boundary is the trust root.

### `trusted_registrars`

Peers allowed to register on behalf of *other* gears (e.g. a central registrar
whose `ServiceAccount` name differs from the gears it manages). Empty by
default — with SA-per-gear / SPIFFE a gear registers under its own name.

### `platform_namespaces` / `trust_domains`

The peer name alone is not sufficient to authorize a per-gear identity: the
token authenticator has no namespace/trust-domain allowlist, so a `billing`
`ServiceAccount` in *any* namespace (or a `billing` workload from *any* SPIFFE
trust domain) would otherwise be authorized for gear `billing`. Configure the
platform-controlled Kubernetes namespaces (`platform_namespaces`) and/or SPIFFE
trust domains (`trust_domains`) that per-gear identities must belong to; a peer
whose qualifier is not in the allowlist is rejected even if its name matches the
gear. Gears may be spread across several listed namespaces.

Both are empty by default, which **disables** the respective qualifier check and
silently falls back to unqualified-name authorization. Configure them in any
deployment where untrusted workloads can mint tokens.

```yaml
gears:
  gear-orchestrator:
    config:
      trusted_registrars:
        - flight-control
      platform_namespaces:
        - platform-system
        - platform-gears
      trust_domains:
        - platform.example.org
```

## License

Licensed under Apache-2.0.
