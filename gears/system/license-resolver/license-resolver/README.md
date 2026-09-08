# License Resolver

Main gear for license checks in Gears. Validates every check against the licensing contracts it declares, discovers a backend plugin via GTS types-registry, and delegates the lookup.

## Overview

The `cf-gears-license-resolver` gear provides:

- **Contract validation** — Resolves the Subject / Resource contract types from types-registry and validates the request against them (schemas + `admitted_subjects`) before anything else runs
- **Plugin discovery** — Finds license-resolver plugins via GTS types-registry
- **Vendor-based selection** — Selects a plugin by vendor and priority, memoized after the first resolution
- **Fail-closed semantics** — A non-conforming request, a missing plugin and an unreachable backend all yield an error; none of them can produce a granted decision
- **ClientHub integration** — Registers `LicenseResolverClient` for inter-gear use

This is a **main gear** — it holds no grant store and decides no licensing question. It validates *shape and compatibility* and routes; what the contract properties mean, and whether the pair is licensed, belong to the plugin.

## Architecture

```
Consumer Gear
    │  projects its domain objects into registered contract types
    ▼
LicenseResolverClient  (SDK trait, registered in ClientHub)
    │
    ▼
license-resolver gateway  (this crate — validates, then routes)
    │                         │
    │                         └─► types-registry: contract schemas + plugin instances
    ▼
LicenseResolverPluginClient  (SDK trait, scoped by GTS instance ID)
    │
    ▼
Plugin implementation  (holds the grant facts)
```

## Usage

```rust
use license_resolver_sdk::{LicenseCheckContext, LicenseCheckRequest, LicenseResolverClient};

let resolver = hub.get::<dyn LicenseResolverClient>()?;

let request = LicenseCheckRequest::new(
    subject,   // instance of your derived `gts.cf.core.lic.subj.v1~…` contract
    resource,  // instance of your derived `gts.cf.core.lic.res.v1~…` contract
    LicenseCheckContext::from_security_context(&ctx),
);

if resolver.is_licensed(request).await?.granted {
    // allow
}
```

A not-granted answer is `LicenseDecision { granted: false }`, not an error. Every error variant is a cannot-determine condition the caller must treat as not-granted — see `LicenseResolverError` in the SDK.

## Validation

Each check resolves both contract types and reports **every** violation it finds, as `LicenseResolverError::InvalidRequest { violations }`. The `reason` codes are published in `license_resolver_sdk::field`:

| `reason` | Rejected because |
|---|---|
| `CONTRACT_NOT_REGISTERED` | the contract type is not in types-registry |
| `CONTRACT_TYPE_MALFORMED` | the contract type is not a well-formed GTS type id |
| `CONTRACT_NOT_DERIVED` | the contract does not derive from the licensing base for its slot |
| `CONTRACT_ABSTRACT` | the contract type is abstract — a check names a derived contract |
| `SCHEMA_MISMATCH` | the contract object does not conform to its registered schema |
| `SUBJECT_NOT_ADMITTED` | the Subject contract is not in the Resource's `admitted_subjects` |

Two properties of this are deliberate:

- **Validation precedes plugin selection.** An invalid check is refused whether or not a backend happens to be reachable — the refusal must not depend on one.
- **A violation is never a decision.** A mismatched pair reaching a backend would come back `granted: false`, turning a request-assembly bug into a licensing answer.

Structural validation runs against the contract's *effective* schema, so the base licensing envelope applies through the derived contract's `allOf` reference — the shape `#[gts_type_schema(base = LicenseResourceV1, …)]` emits. Declare contracts with that macro (see the SDK README) rather than hand-writing the schema.

## Configuration

```yaml
gears:
  license-resolver:
    config:
      vendor: "constructorfabric"   # selects the backend plugin
```

## Telemetry

| Instrument | Type | Labels |
|---|---|---|
| `license_check` | Counter | `contract_type`, `vendor`, `outcome` |
| `license_check_duration_ms` | Histogram | none — resolver-side only, excludes backend compute |
| `license_validation_failure` | Counter | `violation_kind` |

Every label comes from a bounded set. `contract_type` carries a real type id only once validation has confirmed it is registered, and `unvalidated` otherwise: a caller-supplied id would let any caller grow the label space without limit. `violation_kind` is the resolver's own violation vocabulary plus `other`, so a reason a backend raised — which may carry request content — cannot widen it. The latency histogram carries no label at all: the latency target covers the whole surface, and a per-contract breakdown belongs in the span (`resource_contract`), where sampling bounds the cost.

## Writing a Plugin

Implement `LicenseResolverPluginClient` from `cf-gears-license-resolver-sdk` and register a GTS instance derived from `LicenseResolverPluginSpecV1`, plus a scoped ClientHub entry keyed by that instance id. Requests reaching a plugin already conform to a registered contract.

## Testing

```bash
cargo test -p cf-gears-license-resolver
```

## License

Apache-2.0
