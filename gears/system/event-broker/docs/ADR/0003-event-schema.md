---
status: proposed
date: 2026-05-12
decision-makers: Event Broker Team
---

# Event Schema — Single Resource With Read/Write Field Markers And Optional `meta` Block

<!-- toc -->

- [Context and Problem Statement](#context-and-problem-statement)
- [Decision Drivers](#decision-drivers)
- [Considered Options](#considered-options)
- [Decision Outcome](#decision-outcome)
  - [One Schema With Read/Write Field Markers](#one-schema-with-readwrite-field-markers)
  - [Optional Versioned `meta` Block](#optional-versioned-meta-block)
  - [Field-Level Changes](#field-level-changes)
  - [Terminology Cleanup: `offset` → `sequence`](#terminology-cleanup-offset--sequence)
  - [Event Field Encoding: ASCII Only](#event-field-encoding-ascii-only)
  - [Optional CloudEvents Converter, Not Wire Conformance](#optional-cloudevents-converter-not-wire-conformance)
  - [Codegen Note](#codegen-note)
  - [Consequences](#consequences)
  - [Confirmation](#confirmation)
- [Pros and Cons of the Options](#pros-and-cons-of-the-options)
  - [Single Schema With Markers (chosen)](#single-schema-with-markers-chosen)
  - [Keep Today's Single Schema Unchanged](#keep-todays-single-schema-unchanged)
  - [Two Schemas (publish + read)](#two-schemas-publish--read)
  - [Full CloudEvents Conformance](#full-cloudevents-conformance)
- [More Information](#more-information)
- [Traceability](#traceability)

<!-- /toc -->

**ID**: `cpt-cf-evbk-adr-event-schema`

## Context and Problem Statement

The event broker's `schemas/gts.cf.core.events.event.v1~.schema.json` today mixes four categories of fields onto one event:

| Category | Fields |
|---|---|
| Event-semantic | `id`, `type`, `source`, `subject`, `subject_type`, `occurred_at`, `trace_parent`, `data` |
| Partition-routing | `partition` |
| Producer-protocol | `producer_id`, `previous`, `sequence` |
| Server-stamped | `offset`, `offset_time`, `tenant_id`, `created_at` |

Three problems with this state, surfaced during pre-implementation review:

1. **SDK authors cannot tell from the schema** which fields are publish input vs. read output. `readOnly: true` is convention-enforced; code generated from the schema gives a single struct with all fields together, and runtime validation cannot catch a producer that sets a server-stamped field.
2. **Producer-protocol fields leak transport mechanics to consumers.** `producer_id`, `previous`, and the producer-side `sequence` describe *how the event was published*, not *what the event is*. Surfacing them to consumers exposes producer-side state that should be opaque.
3. **The `offset` field and the `max_sequence` / `sent` / `received` / `acked` positions on the cursor model refer to the same value space but use different names.** Wire and cursor speak two dialects for one concept.

The broker is still in design — unshipped, no production data, no live producers or consumers. This ADR locks in the canonical event schema before implementation begins.

Reference specs surveyed for what categories of fields belong on an event at all (NOT for target wire-format conformance):

- CloudEvents v1.0 core attributes (`id`, `source`, `specversion`, `type`) and optional (`subject`, `time`, `datacontenttype`, `dataschema`, `data`); `distributedtracing` extension.
- AsyncAPI message metadata conventions.
- Kafka record headers + key/value/timestamp split.
- NATS message metadata (subject, reply-to, headers).

The decision is about *what fields exist on the broker's event* and *how the schema expresses per-direction (publish vs read) semantics*. Broker-native field names stay broker-native; CloudEvents wire conformance is explicitly rejected.

## Decision Drivers

* SDK authors MUST be able to tell from the schema alone which fields are inputs and which are outputs
* Consumers MUST NOT see producer-side transport mechanics (`producer_id`, `previous`, producer-side `sequence`)
* Single source of truth for the consumer-visible ordering key (wire and cursor agree on naming)
* The producer protocol MUST be able to evolve independently of the event schema
* Batch publish MUST support per-event chain values (each event in a contiguous chain has its own `previous` / `sequence`)
* No silent leak: ingest-internal data (e.g., the `created_at` accept timestamp) is observability data and does not need to live on the event record
* Broker stays as a thin validator: validation logic that can live in JSON Schema does

## Considered Options

* Single schema with field-level `readOnly` / `writeOnly` markers + optional versioned `meta` block + broker-native field names + optional external CloudEvents converter (chosen)
* Keep today's single schema unchanged
* Two schemas (publish + read), separate files
* Full CloudEvents conformance (broker adopts CloudEvents naming and field set)

## Decision Outcome

Adopt **a single canonical event JSON Schema (`schemas/gts.cf.core.events.event.v1~.schema.json`) with field-level `readOnly` / `writeOnly` markers for per-direction semantics, an optional versioned `meta` block for publish-time transport mechanics, broker-native field names, and an *optional, external* CloudEvents converter for cross-broker interop if and when needed.**

### One Schema With Read/Write Field Markers

One JSON Schema replaces the publish-input / read-projection pair:

- **`gts.cf.core.events.event.v1~.schema.json`** is the single source of truth for the event resource shape.
- Field-level markers encode per-direction semantics:
  - `meta`: `"writeOnly": true` — accepted on publish, stripped on read.
  - `partition`, `sequence`, `sequence_time`: `"readOnly": true` — server-stamped on read, rejected with `400 BadRequest` if supplied on publish.
  - All other fields round-trip without markers.
- The top-level `required` array is the **union** of publish-required and read-required fields. Per-direction enforcement is the broker's responsibility at the wire, NOT the schema's responsibility at validation. Strict-validator producers must filter `readOnly` fields before submission.

The detailed field-by-field reference (descriptions, types, validators, examples) lives in [`DESIGN.md §3.1`](../DESIGN.md). This ADR records only the schema-shape decision and the per-direction-semantics mechanism.

### Optional Versioned `meta` Block

A top-level publish-input field `meta` (marked `writeOnly`) carries producer-protocol fields in a versioned, optional block:

```jsonc
"meta": {
  "version": 1,                  // required when meta is present
  "producer_id": "<uuid>",       // chained / monotonic
  "previous": <i64>,             // chained only
  "sequence": <i64>              // chained / monotonic
}
```

- **Optional**: omit `meta` entirely for stateless publish. The simplest case (no broker dedup, no chain machinery) is the simplest wire.
- **Versioned**: `meta.version` lets the producer protocol evolve independently of the event schema. The broker accepts `meta.version <= current_supported`; rejects newer with `400 UnknownMetaVersion`. New producer-protocol fields land under a bumped `meta.version` without an event schema change.
- **Per-event in batches**: each event in a publish batch carries its own `meta`. Contiguous-chain batches "just work" — no per-event header gymnastics; HTTP headers cannot carry per-event values across a request.
- **Transport-agnostic**: same shape over REST, gRPC, message-queue replay, or file import. No header/body split per transport.
- **Stripped on read**: the public read API does NOT echo `meta` to consumers. Storage MAY retain `meta` for audit; the read projection layer strips it. The `writeOnly` marker makes this contract explicit in the schema.

`meta` namespacing eliminates the body↔header duplication that an HTTP-header design (`Producer-Id` header vs. `event.producer_id` body field) would create — there is only one canonical location for each field.

### Field-Level Changes

Concrete edits to the event shape vs. today's `gts.cf.core.events.event.v1~.schema.json`:

| Field | Today | After this ADR | Rationale |
|---|---|---|---|
| `id`, `type`, `source`, `subject`, `occurred_at`, `data` | body | body (unchanged) | Event-semantic, consumer-visible, names stay broker-native |
| `subject_type` | body | body (kept) | Carries entity-kind not derivable from `type` (e.g., generic `rule_applied` may apply to many subject kinds; body-less events have no `data` to introspect) |
| `trace_parent` | body | body (kept) | Distributed-tracing context is event-correlated, not publish-correlated; useful to consumers for post-hoc trace correlation |
| `tenant_id` | body, `readOnly: true` | body, **producer-supplied** | A system service legitimately publishes events on behalf of arbitrary tenants (billing aggregator, audit emitter); authz delegated to platform resolver |
| `producer_id` | body | **moved to `meta`** | Producer-protocol mechanic; should not appear on the consumer-visible body |
| `previous` | body | **moved to `meta`** | Producer-protocol mechanic; per-event in batches |
| `sequence` (producer-side) | body | **moved to `meta`** | Producer-protocol mechanic; renamed under `meta` (the body-level `sequence` after this ADR is the server-assigned consumer-visible field, see "Terminology Cleanup") |
| `partition` | body, producer-set | body, `readOnly` | Broker derives from the member the event type's partition-key pointer names, per [ADR-0002](0002-partition-selection.md); rejected on publish; surfaced on read |
| `offset` | body, `readOnly: true` | **renamed `sequence`**, `readOnly` | Wire / cursor terminology alignment |
| `offset_time` | body, `readOnly: true` | **renamed `sequence_time`**, `readOnly` | Same |
| `created_at` | body, `readOnly: true` | **dropped entirely** | Redundant between `occurred_at` (producer-stamped) and `sequence_time` (server-stamped); the ingest-accept moment is observability data, not event-record data |
| `meta` | (did not exist) | body, `writeOnly` | Producer-protocol block; stripped on read |

### Terminology Cleanup: `offset` → `sequence`

The server-assigned, consumer-visible ordering key is renamed:

- `event.offset` → `event.sequence`
- `event.offset_time` → `event.sequence_time`

The cursor model already uses `max_sequence`, `received`, `sent`, `acked` for the same value space. Renaming the field aligns wire and cursor. The DESIGN.md "Three Sequences" section (§3.6) collapses to **two**:

1. **Producer chain** — `meta.previous`, `meta.sequence` (publish-time only, `writeOnly`, stripped on read).
2. **Server-assigned `sequence`** — consumer-visible ordering key per `(topic, partition)`, `readOnly`.

The outbox sequence inside `toolkit-db` is internal plumbing and not part of the broker's wire surface.

No collision between `meta.sequence` and body-level `sequence`: the `meta.` qualifier disambiguates. Within `meta`, `previous` and `sequence` are the producer-chain pair (the canonical naming idiom; renaming to `producer_sequence` would break that idiom unnecessarily).

### Event Field Encoding: ASCII Only

All event string fields (`id`, `type`, `source`, `subject`, `subject_type`, `trace_parent`, and all `meta.*` string fields) MUST be ASCII on the publish wire. UTF-8 is permitted only inside the `data` payload. The broker rejects publishes containing non-ASCII bytes in any event field with `400 InvalidEventFieldEncoding`. Per-field byte caps apply (e.g., `subject` ≤ 1024 bytes; `400 EventFieldTooLong` on overflow).

This is a platform-wide convention, not a broker-specific choice. It keeps first-party partition-hint derivation deterministic without normalization concerns, and it keeps event-field parsing in any language cheap.

### Optional CloudEvents Converter, Not Wire Conformance

If cross-broker interop ever needs CloudEvents 1.0 wire format, AsyncAPI message bindings, or another standard, the broker event stays as-is and an **external converter** does the field-name translation at the boundary. Two viable shapes for that converter, neither built in this ADR:

- SDK-side adapter that maps broker-native fields to/from CloudEvents (`occurred_at` ↔ `time`, `trace_parent` ↔ `traceparent`, etc.) at publish/consume boundaries.
- Broker-side projection endpoint `GET /v1/events:cloudevents` returning the CloudEvents 1.0 wire shape — separate codepath, separate schema, the canonical storage stays broker-native.

This deliberately avoids paying CloudEvents-conformance cost in the broker's core path against a hypothetical future requirement. If/when such a requirement lands, it ships as an additive feature.

### Codegen Note

JSON Schema codegen tools vary in how they handle `readOnly` / `writeOnly`:

- Tools that **honor** the markers produce two struct variants (or one struct with markers translated to language-level read/write controls).
- Tools that **ignore** the markers produce a single struct with all fields present, including server-stamped ones.

For the codegen-ignores-markers case, producers using such SDKs MUST filter `readOnly` fields (`partition`, `sequence`, `sequence_time`) before submitting publish requests. The broker enforces the rule at the wire: any publish carrying a `readOnly` field is rejected with `400 BadRequest` naming the offending field. SDK contract docs make this requirement explicit. The opposite direction is symmetric: the broker never emits `writeOnly` `meta` on read, so consumers cannot accidentally observe producer-protocol state.

### Consequences

- Good, because SDK authors get a single schema with explicit per-direction markers; codegen tools that honor `readOnly` / `writeOnly` produce correct types; tools that don't are handled by SDK-side filters + broker-side wire enforcement.
- Good, because consumers never see `producer_id`, `previous`, or producer-side `sequence` — producer-side state is opaque to them by construction (the `meta` `writeOnly` marker + broker read-projection logic enforce this together).
- Good, because the producer protocol can evolve without bumping the event schema; new `meta.version`s ship behind feature flags on producer SDKs.
- Good, because batch publish ergonomics are uniform — each event carries its own `meta`; HTTP headers do not block batch chain support.
- Good, because wire and cursor agree on `sequence` terminology; the "Three Sequences" mental model collapses to two.
- Good, because the broker stays a thin validator: `required` + property markers in JSON Schema cover most of the shape; only runtime checks (mode lookup, partition re-hash, `readOnly`-on-publish rejection) need broker code.
- Good, because one schema file replaces two — no `$ref` duplication burden.
- Bad / accepted, because strict JSON-Schema validators that don't honor `readOnly` will demand `partition` on publish unless the SDK filters first. Mitigation: prose descriptions are explicit; broker `400 BadRequest` is unambiguous; SDK contract spells out the filter requirement.
- Bad / accepted, because `meta.sequence` (producer-side) and body-level `sequence` (server-assigned) share a word. Mitigation: the `meta.` qualifier disambiguates; documentation always shows the full path; `writeOnly` vs. `readOnly` markers reinforce the directional distinction.
- Bad / accepted, because the change requires editing DESIGN.md significantly (§3.1 field tables, §3.2 producer modes, §3.6 sequences) and the existing JSON Schema file. The change is unshipped, so this cost is one-time.
- Bad / accepted, because the ASCII-only rule rejects legitimate UTF-8 subject identifiers in some legacy systems. Mitigation: producers normalize to ASCII (e.g., URL-encoded forms, UUIDs, ULIDs) at the broker boundary; the platform convention applies broadly, not just to the broker.
- Neutral, because the optional CloudEvents converter is deferred. The cost of building it is paid only if and when real interop demand emerges.

### Confirmation

The decision is verified by:

- **Schema codegen run**: generating Rust types from the single schema and confirming the producer-facing API on a `readOnly`-honoring tool excludes `partition` / `sequence` / `sequence_time` from publish constructors; the consumer-facing API excludes `meta`.
- **Round-trip test**: a producer publishes an event with `meta`; storage retains `meta`; the read response strips `meta` and surfaces server-assigned `partition`, `sequence`, `sequence_time`. Assertion: the consumer-visible body contains no producer-protocol fields.
- **Mode-shape rejection tests**: publishes carrying `partition`, `sequence`, `sequence_time` as top-level body fields are rejected with `400 BadRequest`. Publishes carrying `producer_id`, `previous`, producer-side `sequence` *outside* `meta` (i.e., as top-level body fields) are rejected with `400 BadRequest`.
- **Encoding tests**: publishes with non-ASCII bytes in any event field are rejected with `400 InvalidEventFieldEncoding`; publishes with UTF-8 inside `data` and ASCII elsewhere are accepted.
- **`meta.version` compatibility test**: a publish with `meta.version > current_supported` is rejected with `400 UnknownMetaVersion`; a publish with `meta.version <= current_supported` is accepted (assuming other validation passes).

## Pros and Cons of the Options

### Single Schema With Markers (chosen)

* Good, because broker-native naming stays stable regardless of which external wire format is adopted downstream
* Good, because per-direction semantics are expressed inside the schema (markers + union `required`) plus enforced at the wire (broker rejects offending fields)
* Good, because one source of truth — no two-file maintenance burden
* Good, because batch ergonomics are uniform via per-event `meta`
* Good, because the producer protocol can evolve under `meta.version` without bumping the event schema
* Good, because optional converter pays no cost in the core path; only invoked at interop boundaries if and when needed
* Bad, because strict validators that don't honor `readOnly` mis-trip on publish (mitigated via SDK-side filter + broker enforcement)
* Bad, because `meta.sequence` vs. body `sequence` share a word

### Keep Today's Single Schema Unchanged

**Description**: Leave all 17 fields on a single schema with `readOnly` annotations; producer-protocol stays on body; `offset` keeps its name.

* Good, because zero migration cost (the broker is unshipped, so this argument is weak)
* Bad, because SDK authors cannot tell input from output without inspecting `readOnly` annotations that codegen may strip
* Bad, because consumers see `producer_id`, `previous`, producer-side `sequence` — leak of transport mechanics
* Bad, because wire and cursor disagree on terminology (`offset` vs. `max_sequence`/`sent`/`received`/`acked`)
* Bad, because batch publish + HTTP-header alternatives are forced into convention-based handling; per-event values fundamentally don't fit in headers
* Bad, because the producer-protocol surface cannot evolve without bumping the event schema

### Two Schemas (publish + read)

**Description**: Maintain `gts.cf.core.events.event.v1~.schema.json` for publish input and a separate read-side schema file. SDK authors get type-safe schemas; codegen produces two structs.

* Good, because per-direction semantics are type-system-enforceable via separate files
* Bad, because two files must be kept in sync — duplicate field declarations (mitigated via `$ref`, but still maintenance)
* Bad, because the read schema was never `$ref`'d from `openapi.yaml`; it served as documentation-as-a-file, not as a wire contract. The intended codegen-separation benefit was never realized.
* Bad, because the publish/read split is essentially a per-direction `required` puzzle that JSON Schema `readOnly` / `writeOnly` markers already solve in one file

### Full CloudEvents Conformance

**Description**: Drop broker-native field names entirely. Rename `occurred_at`→`time`, `trace_parent`→`traceparent`, etc. Broker becomes a CloudEvents 1.0 transport; events on the wire are valid CloudEvents.

* Good, because external interop is "free" — CloudEvents consumers can read events without translation
* Good, because the field set is normalized against a public standard
* Bad, because broker-specific concerns (topic identity, partition, idempotent-producer chain) need to be expressed as CloudEvents extensions — adds attribute-name boilerplate (`io.constructorfabric.broker.topic`) for every internal field
* Bad, because the per-direction surface still needs the marker treatment; CloudEvents conformance does not solve the input/output problem
* Bad, because CloudEvents-conformance cost is paid on every event on every code path, against a hypothetical future requirement; YAGNI
* Bad, because future CloudEvents-spec changes become broker-version dependencies

## More Information

- **Converter realization (post-MVP)**: when / if real interop demand emerges, the converter ships as either an SDK-side adapter (in `cf-gears-event-broker-sdk`) mapping broker-native ↔ CloudEvents at publish/consume boundaries, or a broker-side projection endpoint (`GET /v1/events:cloudevents`) returning CloudEvents 1.0 wire shape. Both are additive and require no change to the canonical storage shape.
- **AsyncAPI documentation export**: similarly post-MVP; an AsyncAPI 2.x document describing the broker's event schema + topic conventions can be generated from the JSON Schema file, with no schema-shape impact.
- **Per-type payload schema**: the contents of the `data` field are described by the narrowing the event type's derived schema applies to this base's `data` member in the types registry. `GET /v1/event-types` projects that contract as `data_schema`. See [`DESIGN.md §3.1`](../DESIGN.md) for the concept relationship and the Validation Pipeline walkthrough.
- **Field-name stability commitment**: changes to the broker-native field names after this ADR is accepted require a major event schema version (`event.v2.schema.json`). Field additions and `meta.*` changes do not.

External references:

- CloudEvents v1.0 — core attributes + extension model (cited as reference for *what fields belong on an event*, NOT for wire-format conformance): <https://github.com/cloudevents/spec/blob/v1.0.2/cloudevents/spec.md>
- CloudEvents `distributedtracing` extension: <https://github.com/cloudevents/spec/blob/v1.0.2/cloudevents/extensions/distributed-tracing.md>
- AsyncAPI 2.x message metadata: <https://www.asyncapi.com/docs/reference/specification/v2.6.0#messageObject>
- Apache Kafka — record header conventions: <https://kafka.apache.org/documentation/#recordheaders>
- NATS message metadata (subject / reply-to / headers): <https://docs.nats.io/nats-concepts/subjects>
- W3C Trace Context (`traceparent` / `tracestate` header format used as the basis for the broker's `trace_parent` field validation): <https://www.w3.org/TR/trace-context/>
- RFC 2119 / RFC 8174 — keyword definitions (MUST, SHOULD, MAY)
- RFC 9457 — Problem Details for HTTP APIs (used for error response shapes)

## Traceability

- **PRD**: [PRD.md](../PRD.md)
  - `cpt-cf-evbk-fr-publish-single` — single-event publish accepts the event shape defined here
  - `cpt-cf-evbk-fr-publish-batch` — batch publish accepts the event with per-event `meta`
  - `cpt-cf-evbk-fr-producer-modes` — producer modes consume `meta.{producer_id, previous, sequence}` per [ADR-0004](0004-idempotent-producer-protocol.md)
- **DESIGN**: [DESIGN.md](../DESIGN.md)
  - §3.1 Domain Model — `Event` field table is updated against this ADR
  - §3.2 Producer Modes — references [ADR-0004](0004-idempotent-producer-protocol.md)
  - §3.1 Validation Pipeline — end-to-end discovery + validation walkthrough
  - §3.6 Three Sequences → "Two Sequences" (producer chain in `meta` + server-assigned `sequence`)
- **Related ADRs**:
  - [`0002-partition-selection`](0002-partition-selection.md) — partition derivation contract
  - [`0004-idempotent-producer-protocol`](0004-idempotent-producer-protocol.md) — producer modes / `meta`-block-shape enforcement / registration TTL / reset endpoint
- **Schemas**:
  - [`schemas/gts.cf.core.events.event.v1~.schema.json`](../schemas/gts.cf.core.events.event.v1~.schema.json) — single canonical event schema (publish + read; per-direction semantics via `readOnly` / `writeOnly` markers)
