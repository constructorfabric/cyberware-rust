---
status: accepted
date: 2026-05-24
---

Created:  2026-05-22 by Virtuozzo International GmbH
Updated:  2026-09-09 by Virtuozzo International GmbH

# Caller-supplied attribution on the ingestion contract

<!-- toc -->

- [Context and Problem Statement](#context-and-problem-statement)
- [Decision Drivers](#decision-drivers)
- [Considered Options](#considered-options)
- [Decision Outcome](#decision-outcome)
  - [Consequences](#consequences)
  - [Confirmation](#confirmation)
- [Pros and Cons of the Options](#pros-and-cons-of-the-options)
  - [Caller-supplied attribution + PDP authorization](#caller-supplied-attribution--pdp-authorization)
  - [Implicit attribution from the security context](#implicit-attribution-from-the-security-context)
  - [Hybrid attribution](#hybrid-attribution)
- [More Information](#more-information)
- [Traceability](#traceability)

<!-- /toc -->

**ID**: `cpt-cf-usage-collector-adr-caller-supplied-attribution`

## Context and Problem Statement

Every ingestion entry carries an attribution tuple: tenant, resource (id and
type), GTS type, and an optional subject. The gear must decide where those fields
come from. Three sources are possible: the caller's resolved security context, an
explicit field on the wire, or the gear itself.

Two facts constrain the answer. Platform forwarders and parent-to-subtenant
emission mean that the caller's security context does not always match the
emission's logical attribution. The platform identity layer, not the gear, owns
PII management for the subject identifier.

The calling gear's own identity is a separate matter. The security context
carries it intrinsically, and the PDP reads it from there. It is not part of the
attribution tuple.

## Decision Drivers

- `cpt-cf-usage-collector-fr-tenant-attribution` — tenant is a mandatory
  caller-supplied field on every ingestion request.
- `cpt-cf-usage-collector-fr-subject-attribution` — subject is an optional
  caller-supplied field.
- `cpt-cf-usage-collector-fr-resource-attribution` — resource id and type are
  mandatory fields.
- `cpt-cf-usage-collector-fr-ingestion-authorization` — the PDP authorizes the
  attribution tuple, so the tuple must be explicit on the wire.
- `cpt-cf-usage-collector-constraint-pii-identity-layer` — subject and tenant
  identifiers are opaque platform identifiers. PII management lives in the
  identity layer, not the gear.
- One uniform ingestion path must serve direct and forwarded emission alike, with
  no separate code path. `cpt-cf-usage-collector-fr-tenant-attribution` and
  `cpt-cf-usage-collector-fr-subject-attribution` record the forwarder and
  parent-to-subtenant scenarios.

## Considered Options

- Caller-supplied attribution + PDP authorization — tenant, resource, and subject
  are explicit fields on the ingestion contract. The PDP authorizes the caller
  against the supplied tuple, and reads the calling gear's identity from the
  security context.
- Implicit attribution from the security context — the gear derives tenant and
  subject from the caller's security context. Only resource and GTS type stay
  caller-supplied.
- Hybrid attribution — the caller supplies tenant, the gear derives subject, and
  a forwarder bypass flag covers cross-tenant emission.

## Decision Outcome

Chosen option: "Caller-supplied attribution + PDP authorization". It is the only
option that serves platform forwarders and parent-to-subtenant emission through
one uniform ingestion path. It also leaves PII management with the platform
identity layer.

The ingestion contract carries tenant, resource (id and type), GTS type, and an
optional subject. The PDP authorizes the caller's security context against that
supplied tuple. The gear never derives tenant, resource, or subject from the
caller's identity. Subject identifiers stay opaque to the gear through ingestion,
persistence, and query.

The calling gear's identity is not a field on the contract. The security context
already carries it. An explicit field is therefore either redundant or
spoofable, and the PDP rejects any disagreement between the two.

### Consequences

- The REST, SDK, and Plugin SPI contracts all carry the same explicit attribution
  tuple. No implicit-attribution path exists to maintain.
- The PDP receives the full tuple on every check. A policy author therefore sees
  both the caller and the emission's logical attribution.
- Forwarder gears and parent-tenant emission use the same code path as direct
  emission. The gear has no "on behalf of" mode.
- The gear applies no attribution policy of its own. It accepts what the PDP's
  returned scope admits, and nothing more, which reinforces
  `cpt-cf-usage-collector-adr-pdp-centric-authorization`.
- Subject identifiers stay opaque strings. The gear does not interpret, redact,
  or classify them, and the data model carries no PII beyond opaque identifiers.

### Confirmation

- Review of the published REST and SDK contracts
  (`cpt-cf-usage-collector-interface-rest-api`,
  `cpt-cf-usage-collector-interface-sdk-client`), to show tenant, resource, GTS
  type, and subject as explicit ingestion fields. No field names the calling
  gear.
- Authorization tests over forwarder and parent-to-subtenant emission, with PDP
  grants and denials that read the calling gear's identity from the security
  context.
- A negative authorization test, in which the PDP permits the caller and the
  returned scope does not admit the tuple. The gear denies the entry.
- Data-classification review, to show that subject and tenant stay opaque strings
  through ingestion, persistence, and query.

## Pros and Cons of the Options

### Caller-supplied attribution + PDP authorization

Every attribution field is explicit on the wire. The PDP authorizes the caller
against the supplied tuple before plugin dispatch.

- Good, because forwarder and parent-tenant emission use the same path as direct
  emission.
- Good, because the PDP sees the full logical attribution, so policy can grant
  forwarder rights explicitly.
- Good, because PII management stays in the identity layer. The gear needs no
  knowledge of subject identifier semantics.
- Neutral, because the ingestion contract grows by a few fields. PDP
  authorization needs those fields in any case.
- Bad, because a caller can supply an attribution that the PDP denies. The denial
  must be deterministic, and the contract must state plainly what it requires.

### Implicit attribution from the security context

The gear derives tenant and subject from the resolved security context. Only
resource and GTS type stay caller-supplied.

- Good, because the wire contract is smaller, and each check reads as one
  security context plus one resource plus one GTS type.
- Bad, because a forwarder cannot emit for another tenant or subject without a
  separate impersonation path.
- Bad, because subject derivation forces the gear to know how a security context
  maps to a subject identifier. That mapping belongs to the identity layer.
- Bad, because parent-to-subtenant emission needs either a second code path or a
  cross-tenant trust boundary inside the gear. The no-business-logic and PII
  constraints permit neither.

### Hybrid attribution

The caller supplies tenant, the gear derives subject, and a forwarder bypass flag
covers cross-tenant emission.

- Good, because it keeps tenant flexible and keeps subject derivation simple in
  the common case.
- Bad, because the bypass flag is special-case behavior that the gear must
  understand. It duplicates what the PDP already authorizes.
- Bad, because a mix of supplied and derived attribution makes both the contract
  and the PDP check harder to reason about.
- Bad, because subject derivation returns PII concerns to the gear, which is what
  `cpt-cf-usage-collector-constraint-pii-identity-layer` exists to prevent.

## More Information

Related decisions:

- `cpt-cf-usage-collector-adr-pdp-centric-authorization` — the gate that
  authorizes this attribution tuple.
- `cpt-cf-usage-collector-adr-mandatory-idempotency` — the contract field that
  makes a retry against this attribution safe.

## Traceability

- **PRD**: [PRD.md](../PRD.md)
- **DESIGN**: [DESIGN.md](../DESIGN.md)

This decision directly addresses the following requirements or design elements:

- `cpt-cf-usage-collector-fr-tenant-attribution` — tenant as a mandatory
  caller-supplied field.
- `cpt-cf-usage-collector-fr-subject-attribution` — subject as an optional
  caller-supplied field.
- `cpt-cf-usage-collector-fr-resource-attribution` — resource id and type as
  mandatory caller-supplied fields.
- `cpt-cf-usage-collector-fr-ingestion-authorization` — the PDP check that this
  explicit tuple makes possible.
- `cpt-cf-usage-collector-constraint-pii-identity-layer` — opaque identifiers,
  with PII managed by the identity layer.
- `cpt-cf-usage-collector-principle-pdp-centric-authorization` — pairs with the
  attribution arm of the authorization principle.
- `cpt-cf-usage-collector-interface-plugin` — the SPI surface that carries the
  attribution tuple onto every persisted entry.
