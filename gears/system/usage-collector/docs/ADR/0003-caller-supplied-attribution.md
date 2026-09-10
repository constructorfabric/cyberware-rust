---
status: accepted
date: 2026-05-24
---

Created:  2026-05-22 by Virtuozzo International GmbH
Updated:  2026-06-10 by Virtuozzo International GmbH

# Caller-supplied attribution for ingestion records

<!-- toc -->

- [Context and Problem Statement](#context-and-problem-statement)
- [Decision Drivers](#decision-drivers)
- [Considered Options](#considered-options)
- [Decision Outcome](#decision-outcome)
  - [Consequences](#consequences)
  - [Confirmation](#confirmation)
- [Pros and Cons of the Options](#pros-and-cons-of-the-options)
  - [Caller-supplied attribution + PDP authorization](#caller-supplied-attribution--pdp-authorization)
  - [Implicit attribution from SecurityContext](#implicit-attribution-from-securitycontext)
  - [Hybrid attribution](#hybrid-attribution)
- [More Information](#more-information)
- [Traceability](#traceability)

<!-- /toc -->

**ID**: `cpt-cf-usage-collector-adr-caller-supplied-attribution`

## Context and Problem Statement

Every ingestion record carries an attribution tuple — tenant, resource (id + type), UsageType, and optionally subject — and the core must decide whether these fields are derived from the caller's SecurityContext, supplied by the caller and authorized by the PDP, or partially synthesized by the collector. Platform-level forwarders and parent-tenant-to-subtenant emission scenarios mean the caller's SecurityContext does not always match the emission's logical attribution; PII handling for the subject identifier is owned by the platform identity layer rather than the collector. The decision shapes the ingestion contract, the PDP authorization tuple, and the boundary between the metering substrate and the identity layer. The calling gear's own identity is intrinsic to the SecurityContext at the SDK call boundary and is not part of the caller-supplied attribution tuple — the PDP reads it directly from the resolved SecurityContext.

## Decision Drivers

- `cpt-cf-usage-collector-fr-tenant-attribution` — tenant is a mandatory caller-supplied field on every ingestion request.
- `cpt-cf-usage-collector-fr-subject-attribution` — subject is an optional caller-supplied field on the ingestion contract.
- `cpt-cf-usage-collector-fr-resource-attribution` — resource id and type are mandatory ingestion-contract fields.
- `cpt-cf-usage-collector-constraint-pii-identity-layer` — subject and tenant identifiers are opaque internal platform identifiers; PII management lives in the identity layer, not the collector.
- PRD §5.3 platform-forwarder and parent-to-subtenant emission scenarios (`cpt-cf-usage-collector-fr-tenant-attribution`) — a single uniform ingestion path must serve both direct and forwarded emissions without separate code paths.
- `cpt-cf-usage-collector-fr-ingestion-authorization` — the PDP authorizes the attribution tuple, so the tuple must be explicit on the wire.

## Considered Options

- Caller-supplied attribution + PDP authorization — tenant, resource, and subject are explicit ingestion-contract fields; the calling gear's identity is read from the resolved SecurityContext; the PDP authorizes the caller against the (ctx-derived caller-gear, supplied tuple) pair before plugin dispatch.
- Implicit attribution from SecurityContext — the collector derives tenant and subject from the caller's resolved SecurityContext; only resource and UsageType remain caller-supplied.
- Hybrid attribution — tenant supplied by caller, subject derived from SecurityContext, with a forwarder bypass flag for cross-tenant emission scenarios.

## Decision Outcome

Chosen option: "Caller-supplied attribution + PDP authorization", because it is the only option that supports platform-level forwarders and parent-to-subtenant emission with a single uniform ingestion path, and it keeps the PII-management responsibility cleanly on the platform identity layer rather than smuggling it into the collector. The ingestion contract carries tenant, resource (id + type), UsageType, and optional subject; the calling gear's identity is intrinsic to the SecurityContext at the SDK call boundary and is not a wire field on the ingestion contract — making it caller-supplied would be either redundant (it must agree with the SecurityContext) or spoofable (the PDP would have to reject any disagreement anyway). The PDP authorizes the caller's SecurityContext (which carries the calling gear's identity) against the supplied tenant/resource/subject tuple; the core never derives tenant, resource, or subject from the caller's identity. Subject IDs remain opaque to the collector throughout ingestion, persistence, and query.

### Consequences

- The ingestion contract on SDK, REST, and Plugin SPI all carry the same explicit attribution tuple; there is no implicit-attribution path to maintain.
- The PDP receives the full tuple on every ingestion authorization call; PDP policy authors gain full visibility into both the caller and the emission's logical attribution.
- Forwarder gears and parent-tenant emissions share the same code path with non-forwarded emissions; there is no special-case "on behalf of" mode in the collector.
- The collector cannot prevent a caller from supplying a tenant or subject the PDP grants; the PDP is the single authoritative gate, which reinforces `cpt-cf-usage-collector-adr-pdp-centric-authorization`.
- Subject identifiers remain opaque strings; the collector does not interpret, redact, or classify them, and the data model carries no PII-sensitive fields beyond opaque identifiers.

### Confirmation

Compliance is confirmed through (a) the OpenAPI contract and SDK trait definitions showing tenant, resource, UsageType, and subject as explicit ingestion fields with the calling gear's identity carried only by SecurityContext (no `source_gear` field on `UsageRecord` or the ingestion projection), (b) authorization tests covering forwarder and parent-to-subtenant emission scenarios with PDP grants and denials that read the caller's gear identity from SecurityContext, and (c) data-classification review confirming subject and tenant remain opaque strings throughout the ingestion, persistence, and query paths.

## Pros and Cons of the Options

### Caller-supplied attribution + PDP authorization

Every attribution field is explicit on the wire; PDP authorizes the caller against the supplied tuple before plugin dispatch.

- Good, because forwarder and parent-tenant scenarios share the same uniform path as direct emission.
- Good, because the PDP sees the full logical attribution and policy can encode forwarder-specific grants explicitly.
- Good, because PII management stays in the identity layer; the collector remains agnostic to subject identifier semantics.
- Neutral, because the ingestion contract grows by a few fields versus an implicit model; this is acceptable since the fields are required by PDP authorization anyway.
- Bad, because a caller can supply an attribution that the PDP denies; the denial must be deterministic and the contract must be unambiguous about what is required.

### Implicit attribution from SecurityContext

Tenant and subject derive from the resolved SecurityContext; only resource and UsageType remain caller-supplied.

- Good, because the wire contract is slimmer and authorization checks read like simple "this SecurityContext + this resource + this UsageType".
- Bad, because forwarders cannot emit on behalf of another tenant or subject without a separate impersonation path.
- Bad, because subject derivation forces the collector to know how SecurityContext maps to a subject identifier, which is identity-layer concern.
- Bad, because parent-to-subtenant emission requires either a second code path or a cross-tenant trust boundary inside the collector — neither acceptable under the no-business-logic and PII constraints.

### Hybrid attribution

Tenant supplied by caller, subject derived from SecurityContext, with a forwarder bypass flag for cross-tenant scenarios.

- Good, because it preserves tenant flexibility while keeping subject derivation simple in the common case.
- Bad, because the bypass flag is a special-case behavior the collector must understand; it duplicates what the PDP should already authorize.
- Bad, because mixing supplied and derived attribution complicates the contract and the PDP authorization story.
- Bad, because subject derivation re-introduces PII-shaped concerns inside the collector that the identity-layer constraint exists to remove.

## More Information

Related decisions: `cpt-cf-usage-collector-adr-pdp-centric-authorization` (the gate that authorizes the attribution tuple); `cpt-cf-usage-collector-adr-mandatory-idempotency` (the contract field that makes safe retries possible against this attribution). The SPI persist surface (`plugin-spi.md` Method 1 / 2) and the §3.2 ingestion path are the structural anchors.

## Traceability

- **PRD**: [PRD.md](../PRD.md)
- **DESIGN**: [DESIGN.md](../DESIGN.md)

This decision directly addresses the following requirements or design elements:

- `cpt-cf-usage-collector-fr-tenant-attribution` — tenant as a mandatory caller-supplied field.
- `cpt-cf-usage-collector-fr-subject-attribution` — subject as an optional caller-supplied field.
- `cpt-cf-usage-collector-fr-resource-attribution` — resource id and type as mandatory caller-supplied fields.
- `cpt-cf-usage-collector-constraint-pii-identity-layer` — opaque identifiers, PII managed by the identity layer.
- `cpt-cf-usage-collector-principle-pdp-centric-authorization` — pairs with the attribution arm of the authorization principle.
- `cpt-cf-usage-collector-interface-plugin` — the SPI persist surface that materializes the attribution tuple on every persisted record.
