---
status: accepted
date: 2026-08-17
decision-makers: usage-collector spec owners
---

Created:  2026-07-07 by Virtuozzo International GmbH
Updated:  2026-09-10 by Virtuozzo International GmbH

# Deterministic entry identity derived from the dedup identity over a covered period

<!-- toc -->

- [Context and Problem Statement](#context-and-problem-statement)
- [Decision Drivers](#decision-drivers)
- [Considered Options](#considered-options)
- [Decision Outcome](#decision-outcome)
  - [The canonical pre-image](#the-canonical-pre-image)
  - [Entry type is excluded](#entry-type-is-excluded)
  - [Consequences](#consequences)
  - [Confirmation](#confirmation)
- [Pros and Cons of the Options](#pros-and-cons-of-the-options)
  - [UUIDv5 over the full dedup identity](#uuidv5-over-the-full-dedup-identity)
  - [UUIDv5 over a single event instant](#uuidv5-over-a-single-event-instant)
  - [UUIDv5 over tenant, type and key alone](#uuidv5-over-tenant-type-and-key-alone)
  - [Random server-generated identity](#random-server-generated-identity)
  - [Client-supplied identity](#client-supplied-identity)
- [More Information](#more-information)
- [Traceability](#traceability)

<!-- /toc -->

**ID**: `cpt-cf-usage-collector-adr-record-identity-derivation`

## Context and Problem Statement

Every accepted entry carries one identifier, and
`cpt-cf-usage-collector-fr-record-identity` constrains it with five properties.
It is server-derived, stable under an exact-equality retry, reproducible offline
by the emitter, addressable, and unique per tenant, GTS type, idempotency key and
covered period.

An entry covers a period rather than an instant
(`cpt-cf-usage-collector-fr-usage-windows`).
`cpt-cf-usage-collector-fr-idempotency` fixes its dedup identity as tenant, GTS
type, idempotency key and both bounds of that period. The identifier must project
exactly that identity. An identifier over fewer inputs collapses entries that the
dedup identity keeps apart.

The question is how the gear derives the value, and where the algorithm lives.
`cpt-cf-usage-collector-fr-record-identity` states the five properties and
delegates the derivation and its namespace constant here. The REST contract
delegates the namespace constant here as well. This document is therefore
normative, not merely rationale.

## Decision Drivers

- `cpt-cf-usage-collector-fr-record-identity` — states the five properties, and
  delegates the algorithm and the namespace constant to this decision.
- `cpt-cf-usage-collector-fr-idempotency` — fixes the dedup identity as tenant,
  GTS type, idempotency key and both bounds of the covered period. The identifier
  projects that identity.
- `cpt-cf-usage-collector-fr-usage-windows` — supplies two of the five inputs,
  and makes a point event a zero-length period rather than a separate shape.
- `cpt-cf-usage-collector-principle-fail-closed` — a client-supplied identifier
  is never trusted, so the gear derives the value itself.
- Offline reproducibility — an emitter must compute a target's identifier before
  submission, so that a correction needs no round-trip.
- One derivation for every surface — REST, the in-process SDK and the Plugin SPI
  must agree on the identifier of one entry.

## Considered Options

- UUIDv5 over the full dedup identity — derive the identifier from tenant, type,
  idempotency key and both bounds of the covered period.
- UUIDv5 over a single event instant — derive it from tenant, type, key and one
  timestamp, in place of the two period bounds.
- UUIDv5 over tenant, type and key alone — leave the covered period out of the
  derivation.
- Random server-generated identity — the gear assigns an unpredictable value and
  excludes it from the canonical-equality comparison set.
- Client-supplied identity — the emitter sends the identifier as an ordinary
  field.

## Decision Outcome

Chosen option: "UUIDv5 over the full dedup identity". The identifier is a
UUIDv5 over that identity. The Ingestion Gateway computes it
at one choke point for every surface, before it dispatches to the active plugin.

```text
id = UUIDv5(NS, tenant_id <0x1F> gts_type_id <0x1F> idempotency_key <0x1F> window_start <0x1F> window_end)
```

The namespace constant `NS` is `56313026-863b-4de8-b32b-1f96b67306ed`. It is
fixed forever. A change re-maps every identifier the gear has ever issued.

`<0x1F>` is the ASCII unit separator, and the concatenation stays injective only
while no input carries that byte. Three inputs cannot carry it by construction:
the tenant is a UUID, and both window bounds are timestamps. The other two are
caller-supplied strings, so the wire contract constrains them with a pattern that
excludes every ASCII control character.

The derivation therefore needs a precondition, and
`cpt-cf-usage-collector-fr-record-identity` carries it as a requirement. The gear
rejects a submission whose idempotency key or GTS type reference contains a
control character, with an actionable validation error. Without that rule two
distinct dedup identities concatenate to one pre-image, which breaks the
uniqueness property the identifier must have.

The concatenation takes five inputs, while
`cpt-cf-usage-collector-fr-record-identity` counts four attributes, because the
covered period contributes both of its bounds. The two arities describe one
function.

A point event derives its identifier over a zero-length window, where the two
bounds are equal. The derivation needs no separate case for it.

An invalidation entry derives its identifier by the same function over the same
five inputs. That derivation is well defined, because an invalidation entry
copies the covered period of its target
(`cpt-cf-usage-collector-adr-append-only-invalidation`). Its identifier differs
from its target's for one reason only: the two carry different idempotency keys.

### The canonical pre-image

The formula fixes the order of the inputs. This subsection fixes their bytes.
Two spellings of one value would otherwise derive two identifiers, and both
deduplication and offline reproduction would fail.

`NS` enters the digest as its 16 raw bytes in network order, per RFC 4122, and
never as its 36-character text form. The five inputs and the four separators
enter as the byte string below.

| Input | Canonical form |
| --- | --- |
| `tenant_id` | The lowercase hyphenated 36-character form of RFC 4122, as UTF-8. |
| `gts_type_id` | The wire string, byte-exact and UTF-8, terminator `~` included. |
| `idempotency_key` | The wire string, byte-exact and UTF-8. |
| `window_start` | `YYYY-MM-DDTHH:MM:SS.ffffffZ`, as UTF-8. |
| `window_end` | `YYYY-MM-DDTHH:MM:SS.ffffffZ`, as UTF-8. |

Byte-exact means that the gear applies no case folding, no Unicode
normalization, no trimming and no escaping. Two Unicode spellings of one
grapheme are therefore two distinct inputs. That is deliberate: the gear treats
both values as opaque platform identifiers and parses neither.

The timestamp form is fixed at 27 characters. The date and time separator is an
uppercase `T`, the zone designator is an uppercase `Z`, and the fraction always
carries exactly six digits. A caller that sends `12:00:00Z`, `12:00:00.000Z` or
`13:00:00+01:00` therefore reaches one canonical form, after the UTC
normalization of `cpt-cf-usage-collector-fr-usage-windows`.

Six digits is the microsecond, and it is the precision ceiling of the
derivation. The gear rejects a covered period that carries a finer value, with
an actionable validation error, before the derivation runs. It does not
truncate one. Truncation would make a read-back entry derive an identifier
different from the one it carries. That breaks offline reproduction at the
point an emitter needs it. A second value of `60` is rejected on the same path,
so no leap second enters the derivation.

`cpt-cf-usage-collector-fr-record-identity` carries both preconditions as
requirements, next to the control-character rule above.

### Entry type is excluded

Entry type is deliberately not an input to the derivation. Admitting it lets one
idempotency key stand for both a measurement and its withdrawal. An emitter
defect that reused a key then produces both entries silently, instead of
surfacing a conflict.

An invalidation submitted under its target's own key therefore collides on all
five inputs. The gear rejects it as a same-key content mismatch, and the caller
observes a conflict. This decision makes that outcome authoritative for the case,
because it needs no rule beyond the derivation and canonical equality.
`cpt-cf-usage-collector-adr-append-only-invalidation` separately states key
distinctness as a wire-level rule. A gateway pre-check on that rule is a
redundant guard, and it must not report a different error for a collision on all
five inputs.

The complement of this exclusion lives in
`cpt-cf-usage-collector-adr-mandatory-idempotency`: the invalidation target is
part of the canonical-equality comparison set. Entry type itself is not
compared. The gear derives it from the target, so it cannot differ unless the
target differs. That asymmetry is the whole mechanism. The derivation collapses
a key reused across the pair onto one identifier, and the comparison then
converts the collision into a loud rejection.

### Consequences

- The namespace constant is permanent. It admits no rotation, no per-deployment
  value and no versioned variant, because each of those re-maps every identifier
  the gear has issued.
- A wire change to any of the five inputs changes every identifier the gear
  derives. Renaming, retyping or re-normalizing one input is therefore a breaking
  change, and it must be expressed as a new major version.
- Downstream consumers deduplicate on this identifier, and every read path
  carries it (`cpt-cf-usage-collector-fr-billing-fields-on-read`). Raw query,
  point lookup and the usage feed all return the same derived value.
- The covered period supplies two of the five inputs, so re-deriving an
  identifier means re-submitting the period it covers. The ingestion bounds are
  checked against that same period, which is what makes an identifier
  irreproducible once its period ages past the backfill window
  (`cpt-cf-usage-collector-adr-backfill-isolation`) and keeps every admissible
  submission inside its idempotency horizon
  (`cpt-cf-usage-collector-adr-mandatory-idempotency`).
- An emitter that recomputes its covered period on retry derives a different
  identifier. Deterministic period bounds are therefore an emitter obligation,
  and the gear does not quantize a period before it enters the derivation.
- The canonical form is frozen with the namespace constant. A change to the
  fraction width, the UUID case or the text encoding re-maps every identifier
  the gear has issued.
- The microsecond ceiling binds the storage contract too. A plugin that
  persists a covered period at a coarser precision returns entries whose
  identifiers no emitter can reproduce.
- UUIDv5 is a truncated SHA-1 digest, so the derivation is collision-resistant
  but not injective. Distinct dedup identities are overwhelmingly likely to yield
  distinct identifiers, and the residual collision probability is out of scope.
  The gear carries no runtime collision-handling path.

### Confirmation

- Unit tests that pin the namespace constant and golden vectors, and assert
  determinism across all three surfaces.
- A test asserting that a point event and a zero-length interval derive the same
  identifier.
- A test asserting that an invalidation entry and its target differ in their
  identifiers only through the idempotency key.
- A test asserting that an invalidation submitted under its target's own
  idempotency key is rejected as a same-key content mismatch. It must not be
  accepted as a second entry.
- A test asserting that a submission carrying the `0x1F` byte in the idempotency
  key or the GTS type reference is rejected before the derivation runs.
- Golden vectors that pair equivalent input formats and assert one identifier.
  The pairs cover UUID case, `Z` against a non-UTC offset, and a fraction of
  zero, three and six digits.
- A test asserting that a covered period carrying finer than microsecond
  precision is rejected before the derivation runs.

## Pros and Cons of the Options

### UUIDv5 over the full dedup identity

The identifier is a deterministic projection of the dedup identity that
`cpt-cf-usage-collector-fr-idempotency` states.

- Good, because the identifier and the dedup identity read the same five inputs,
  so the two can never disagree about what one entry is.
- Good, because an emitter reproduces the value offline and names a correction
  target before submission.
- Good, because two entries covering different periods stay distinct under one
  stable per-meter idempotency key.
- Good, because a point event needs no special case. A zero-length window is an
  ordinary input.
- Neutral, because the algorithm is shared code rather than gateway-private. The
  SDK crate carries the derivation helper and its reference vectors, so every
  surface and every emitter computes one value. Carrying the helper is not
  assigning the identifier: the Ingestion Gateway remains the sole authority
  that assigns the accepted `id`, at one choke point, before plugin dispatch.
- Bad, because a covered period can be recomputed inconsistently on a retry, and
  a different pair of bounds derives a different identifier. Deterministic bounds
  are an emitter obligation.
- Bad, because the namespace constant and the input set are both frozen. Any
  change to either re-maps every identifier.

### UUIDv5 over a single event instant

Derive the identifier from tenant, type, key and one timestamp, in place of the
two period bounds.

- Good, because a single instant cannot be recomputed inconsistently on a retry,
  while a pair of period bounds can.
- Neutral, because it needs no change to the namespace constant. Only the input
  list differs.
- Bad, because an entry carries no single instant. The derivation must elect one
  of the two bounds, and that choice is arbitrary.
- Bad, because one instant cannot represent an interval. Two entries covering
  different periods under one stable per-meter key collide on a single
  identifier.
- Bad, because that collision is silent. Point lookup resolves to an arbitrary
  member of the colliding set.

### UUIDv5 over tenant, type and key alone

Leave the covered period out of the derivation.

- Good, because it is the smallest input set that still yields a server-derived,
  offline-reproducible identifier.
- Bad, because a stable per-meter idempotency key then cannot cover more than one
  period at all. Every submission under that key resolves to one identifier.
- Bad, because it pushes the period into the idempotency key. The key becomes an
  emitter-side encoding problem rather than a caller-chosen value.

### Random server-generated identity

The gear assigns an unpredictable identifier and excludes it from the
canonical-equality comparison set.

- Good, because uniqueness holds by construction, with no digest and no namespace
  constant to freeze.
- Bad, because it is not reproducible offline. An emitter must read its target
  back before it can name a correction reference, which costs a round-trip on
  every correction.
- Bad, because the identifier then carries no relation to the dedup identity. A
  re-admitted entry beyond the horizon is indistinguishable from new consumption.

### Client-supplied identity

The emitter sends the identifier as an ordinary field.

- Good, because the emitter always holds the value it sent, so a correction
  reference costs nothing.
- Bad, because nothing enforces uniqueness. Two distinct dedup identities can
  carry one identifier, which makes point lookup ambiguous.
- Bad, because a client that regenerates the value on retry produces a false
  conflict, since the identifier takes part in canonical equality.
- Bad, because it trusts a caller-supplied value on the ingestion path, against
  `cpt-cf-usage-collector-principle-fail-closed`.

## More Information

The derivation is one algorithm, so one document carries all of it: the
function, the namespace constant, the separator and the excluded input. Splitting
it across records makes the authoritative form hard to locate, and a reader can
stop at the wrong formula.

Related decisions:

- `cpt-cf-usage-collector-adr-mandatory-idempotency` — states the dedup identity
  this derivation projects, and the canonical-equality comparison set that the
  invalidation target enters.
- `cpt-cf-usage-collector-adr-append-only-invalidation` — fixes the covered
  period an invalidation entry copies, which is what makes its own identifier
  derivable.

## Traceability

- **PRD**: [PRD.md](../PRD.md)
- **DESIGN**: [DESIGN.md](../DESIGN.md)

This decision directly addresses the following requirements or design elements:

- `cpt-cf-usage-collector-fr-record-identity` — the derivation that this
  requirement delegates here, together with its namespace constant.
- `cpt-cf-usage-collector-fr-idempotency` — the dedup identity that the
  derivation projects onto one identifier.
- `cpt-cf-usage-collector-fr-usage-windows` — the covered period supplying two of
  the five inputs, and the zero-length case for a point event.
- `cpt-cf-usage-collector-fr-billing-fields-on-read` — every read path carries the
  identifier, so a consumer can deduplicate and reference on it.
- `cpt-cf-usage-collector-principle-idempotency-by-key` — the design principle
  that this decision codifies, by making the identifier a function of the key.
- `cpt-cf-usage-collector-principle-fail-closed` — a client-supplied identity is
  never trusted, so the gear derives the value itself.
- `cpt-cf-usage-collector-interface-rest-api`,
  `cpt-cf-usage-collector-interface-sdk-client`, and
  `cpt-cf-usage-collector-interface-plugin` — one derivation, computed upstream of
  all three at the ingestion choke point.
- `cpt-cf-usage-collector-seq-emit-usage` and
  `cpt-cf-usage-collector-seq-invalidate-record` — the sequences that derive it,
  each before it dispatches to the active plugin.
