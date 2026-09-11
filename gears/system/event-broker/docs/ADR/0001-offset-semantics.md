# ADR-0001: Offset Semantics — Sequences Start at 1

<!-- toc -->

- [Status](#status)
- [Context and Problem Statement](#context-and-problem-statement)
- [Decision Drivers](#decision-drivers)
- [Considered Options](#considered-options)
- [Decision Outcome](#decision-outcome)
  - [Consequences](#consequences)
  - [Confirmation](#confirmation)
- [Pros and Cons of the Options](#pros-and-cons-of-the-options)
  - [Sequences Start at 1](#sequences-start-at-1)
  - [Sequences Start at 0](#sequences-start-at-0)
  - [Leave Unspecified](#leave-unspecified)
- [More Information](#more-information)

<!-- /toc -->

**ID**: `cpt-cf-evbk-adr-offset-semantics`

## Status

Accepted

## Context and Problem Statement

The event broker exposes consumer-visible **sequences** (also called offsets) to events through the storage backend boundary. The design describes public sequences as monotonically increasing within a `(topic, partition)` but did not originally specify the floor — whether they start at 0 or 1. This gap has consequences for every component that reasons about cursors, valid SEEK ranges, or offset types.

The **cursor** model used throughout the broker is **last-processed-offset**: a consumer stores the sequence of the last event it successfully processed, and the broker delivers the events stored strictly above that cursor - `cursor + 1` is where scanning resumes, not necessarily where the next event is, because the space is populated sparsely. For a consumer that has never processed any event, a special "nothing processed yet" cursor value is needed. That value is `cursor = RF - 1`, where RF is the retention floor (the sequence of the oldest available event).

The decision needs to keep three things aligned:

- Broker-visible event sequences must have one unambiguous floor.
- Cursor arithmetic must not require negative sentinel values.
- Backend adapters must expose the same broker-logical sequence space even when their native offset model differs.

## Decision Drivers

- Keep consumer-visible offsets non-negative across wire, database, and SDK surfaces.
- Preserve the last-processed cursor model where the broker delivers what is stored strictly above the cursor.
- Avoid backend-specific offset floors leaking into broker APIs.
- Make the fresh-topic and retention-floor cases mechanically obvious.

## Considered Options

Two choices were considered:

| Sequences start at | RF on fresh topic | "nothing yet" cursor | cursor space | Minimum type |
|---|---|---|---|---|
| 0 | 0 | `0 - 1 = -1` | `{-1, 0, 1, ...}` | signed (i64) |
| 1 | 1 | `1 - 1 = 0`  | `{0, 1, 2, ...}`  | unsigned (u64) |

Starting at 0 requires a signed integer type everywhere offsets appear (wire, DB, SDK) and co-opts -1 as a sentinel with no semantic business meaning. Starting at 1 eliminates negative values entirely.

## Decision Outcome

**Consumer-visible broker sequences MUST start from 1 on a fresh `(topic, partition)`. Sequence 0 is never exposed as an event sequence.**

This is a hard conformance requirement for every backend implementation (built-in and third-party), not a convention or default. A backend MAY use a different native position model internally, but it MUST translate that native position into the broker-logical sequence space before any value reaches a public broker surface.

For Kafka-backed storage, native Kafka offset `N` maps to broker sequence `N + 1`; a broker cursor `N` resumes from native Kafka offset `N`.

### Consequences

**Cursor space is non-negative.**

With sequences starting at 1, the retention floor RF ≥ 1 always. Therefore:

```
cursor ∈ {0, 1, 2, ...}

cursor = 0  →  "nothing processed yet; broker emits from RF"
cursor = N  →  "last processed event had sequence N; broker emits the next event stored above N"
```

**Valid SEEK range is always non-negative.**

```
valid range: [RF - 1, HWM]
           = [≥ 0,    HWM]   (since RF ≥ 1)
```

No negative value is reachable on the wire, in the database, or in SDK types.

**Cursor type may be u64.**

Implementations MAY represent cursors as `u64` (unsigned 64-bit integer). Implementations that already use `i64` for cursors MUST enforce a `≥ 0` invariant. A future SDK design change may tighten this to `u64` across all layers.

**Backend conformance contract.**

Every storage backend that implements the `StorageBackend` trait MUST satisfy:

- The first event visible through the broker on a fresh `(topic, partition)` has `sequence = 1`.
- Sequence 0 is never exposed as an event sequence in stream frames, query results, SEEK responses, topology frames, control frames, or SDK offset stores.
- On idempotent retry, previously persisted broker-logical sequences are returned as-is; no public sequence is assigned or re-assigned to 0.
- If the backend's native position space does not already satisfy this contract, the backend adapter owns the native-to-logical mapping at its boundary.

A backend that exposes sequence 0 violates this ADR and breaks the cursor non-negativity guarantee for all consumers of that partition.

### Confirmation

Confirm backend implementations and SDK offset stores by checking that no public broker surface exposes sequence `0` as an event sequence and that fresh-topic `"earliest"` resolves to cursor `0` before emitting sequence `1`.

## Pros and Cons of the Options

### Sequences Start at 1

Pros:

- Cursor space is non-negative.
- Fresh-topic `"nothing processed yet"` is represented as cursor `0`.
- Backend adapters have one explicit public mapping contract.

Cons:

- Backends with native zero-based positions must map native offsets at the boundary.

### Sequences Start at 0

**Start at 0**: requires signed integer types; cursor = -1 as a "nothing yet" sentinel has no positive semantic meaning and creates edge cases in arithmetic (e.g., `cursor + 1 = 0` looks like "first sequence" but is actually "start of stream"). Rejected.

### Leave Unspecified

**Leave unspecified**: implicit assumptions about the floor diverge across backend implementations and SDK components. The gap was discovered during scenario review when `"earliest"` on a fresh topic was described as resolving to cursor = -1. Leaving it unspecified delays the conflict until runtime. Rejected.

## More Information

- DESIGN.md §3.1 "Offset Semantics" — normative vocabulary section derived from this decision
- ADR-0002 (partition-selection) — references `(topic, partition)` sequences without specifying their floor; this ADR is its prerequisite
- ADR-0006 (offset-authority) — establishes that consumers own their offset tracking; this ADR establishes what those offsets ARE
