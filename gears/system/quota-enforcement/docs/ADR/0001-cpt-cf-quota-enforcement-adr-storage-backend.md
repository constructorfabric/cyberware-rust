---
status: accepted
date: 2026-05-07
---

# Pluggable storage backend — capability-based contract

<!-- toc -->

- [Context and Problem Statement](#context-and-problem-statement)
- [Decision Drivers](#decision-drivers)
- [Considered Options](#considered-options)
- [Decision Outcome](#decision-outcome)
  - [Capability contract](#capability-contract)
  - [Reference implementation (non-normative)](#reference-implementation-non-normative)
  - [Operator freedom](#operator-freedom)
  - [Consequences](#consequences)
  - [Confirmation](#confirmation)
- [Pros and Cons of the Options](#pros-and-cons-of-the-options)
  - [Pluggable trait with capability contract](#pluggable-trait-with-capability-contract)
  - [Pin a specific backend in QE-core](#pin-a-specific-backend-in-qe-core)
  - [Multi-backend federation in QE-core](#multi-backend-federation-in-qe-core)
- [More Information](#more-information)
- [Traceability](#traceability)

<!-- /toc -->

**ID**: `cpt-cf-quota-enforcement-adr-storage-backend`

## Context and Problem Statement

Quota Enforcement requires a persistence layer for Quotas, counters, leases, Policy versions, idempotency records, the
operation log, and the same-tx outbox. Different deployments have radically different operational profiles — from edge /
single-process embedded contexts to multi-region production clusters; from RDBMS-shaped operational expertise to
distributed-KV-shaped expertise to in-memory + WAL setups. Pinning a specific backend in QE-core would either
over-constrain the small-deployment story or under-serve production-scale and specialized operators.

QE-core therefore needs to specify **what the storage layer must guarantee**, not **which product** provides it. The
decision this ADR records is *how* storage is selected — not *which* storage is selected.

## Decision Drivers

- Storage-pluggable contract (`cpt-cf-quota-enforcement-fr-pluggable-storage`).
- No QE-core lock-in to a specific vendor, product, or even a backend class.
- Cross-deployment NFR portability — operator chooses backend per their NFR / operational / security profile, not per
  QE-core opinion.
- Atomic multi-Quota acquisition (`cpt-cf-quota-enforcement-fr-lease-acquire`).
- Strong consistency within tenant scope (I10 / `cpt-cf-quota-enforcement-fr-tenant-isolation`).
- RPO = 0 fault-tolerance (`cpt-cf-quota-enforcement-nfr-fault-tolerance`).
- Same-tx outbox (I11) for notification durability.
- Single, unambiguous evolution path: changes to storage semantics happen at the trait surface, not via per-backend
  special cases inside QE-core.

## Considered Options

- **(a) Pluggable trait with capability contract** — QE-core defines a single Rust trait
  (`QuotaEnforcementStoragePluginV1`) and a capability list; any backend that satisfies the contract is acceptable.
- **(b) Pin a specific backend in QE-core** — e.g., hardcode PostgreSQL or hardcode «`toolkit-db` only».
- **(c) Multi-backend federation in QE-core** — QE-core layered over multiple backends simultaneously.

## Decision Outcome

Chosen option: **(a) — pluggable trait with capability contract.** Storage is decoupled into
`QuotaEnforcementStoragePluginV1` (defined in `cpt-cf-quota-enforcement-fr-pluggable-storage`). QE-core specifies only
outcome-based **capability requirements** (DESIGN §3.5 «Required backend capabilities») plus the storage-trait
invariants I1–I13 (DESIGN §3.3). Any backend that satisfies the contract is contract-compliant — QE-core does not name,
prefer, or constrain any specific backend.

### Capability contract

The plugin contract requires the backend to provide (full list in DESIGN §3.5):

- Multi-statement ACID transactions with isolation sufficient to serialize concurrent row mutations under the
  deterministic acquisition ordering of `cpt-cf-quota-enforcement-adr-acquisition-ordering`.
- Bounded-latency row mutation under contention (I8 contention-timeout discipline).
- Durable commit with RPO = 0.
- Filterable metadata predicates (for attribute-gated arbitration per
  `cpt-cf-quota-enforcement-fr-attribute-based-quota-selection`).
- Hot-path access patterns satisfying `cpt-cf-quota-enforcement-nfr-evaluation-latency` and
  `cpt-cf-quota-enforcement-nfr-subject-scale`.
- Same-tx outbox enqueue (I11).
- Schema-versioned migrations validated at `bootstrap()` (I12).

A backend that meets every capability is plugin-compliant. Concrete realisation — isolation level, locking discipline,
indexing strategy, partitioning, replication topology, storage medium (RDBMS, distributed KV, log-structured, in-memory
\+ WAL, embedded, …) — is **plugin-internal**.

### Reference implementation (non-normative)

For default-deployment ergonomics and platform alignment with sibling gears (Usage Collector, Simple
Resource Registry), QE ships a reference plugin built on `toolkit-db`. PostgreSQL is the recommended default within the
`toolkit-db` family — natively delivers the P1 NFR profile (deterministic row locking with `lock_timeout`, synchronous
replication for RPO = 0, JSONB for metadata predicates, partial indexes for active-row narrowing). MariaDB / MySQL /
SQLite are equally valid `toolkit-db` targets with documented operational trade-offs.

This reference plugin is a **default**, not a normative choice. Operators are free to ship a different plugin against
the same contract — distributed KV, in-memory + WAL, embedded engines, custom solutions, anything that satisfies the
§3.5 capability list + I1–I13 invariants. QE-core remains unchanged across plugin choices.

### Operator freedom

Selecting a storage backend is an **operator-deployment decision**, not a QE-core decision. The single normative
constraint is conformance to the §3.5 capability list plus I1–I13 invariants. Operators may:

- adopt the reference plugin as-is;
- swap the reference plugin for another `toolkit-db`-family target (different RDBMS);
- ship a completely different plugin (different storage class) without touching QE-core or the reference plugin;
- run a hybrid where the reference plugin handles some deployments while a custom plugin handles others.

QE-core neither requires nor inspects the operator's choice — it only requires that whichever plugin is loaded conforms
to the contract.

### Consequences

- Switching backend = swap the plugin crate. No QE-core, PRD, or sibling-ADR change required.
- Each plugin owner authors a separate plugin DESIGN document covering its concrete realisation (table / collection /
  file layouts, indexes, partitioning, locking discipline). The plugin DESIGN file path is plugin-internal — QE-core
  does not pin it.
- Reference impl exists for default deployment ergonomics; alternative plugins (any vendor, any architecture) are
  first-class peers, not second-tier alternatives.
- The capability contract (DESIGN §3.5 + I1–I13) is the single point of evolution. Changes to the contract are
  major-version bumps of `QuotaEnforcementStoragePluginV1`; backwards-compatible additive changes are allowed within a
  major.
- Sweeper singleton coordination is **out of scope** of this contract; it lives in
  `cpt-cf-quota-enforcement-adr-coordination-plugin`, which consumes the platform `cluster` gear. Storage and
  coordination evolve independently.

### Confirmation

Confirmed for any storage-plugin impl (reference or third-party) by:

- code review of the impl against the §3.5 capability list and the I1–I13 invariants;
- benchmark suite covering `cpt-cf-quota-enforcement-nfr-evaluation-latency` and
  `cpt-cf-quota-enforcement-nfr-throughput` under the impl's target NFR profile;
- DR drill validating the impl's claimed RPO under `cpt-cf-quota-enforcement-nfr-fault-tolerance`.

The reference impl carries this gate at the QE-side CI pipeline; alternative impls run their own copy of the suite
against their own backend.

## Pros and Cons of the Options

### Pluggable trait with capability contract

- Good, because it gives operators absolute freedom of backend choice — RDBMS, distributed KV, embedded, in-memory +
  WAL, custom — within the capability envelope.
- Good, because it isolates QE-core from vendor-specific behaviour; no leak of Postgres-isms (or any other backend's
  quirks) into the QE-core code.
- Good, because specialized deployments (edge, embedded, dev/test, regulated on-premise) ship their own plugin without
  QE-core changes.
- Good, because contract evolution is localized to the trait surface.
- Good, because separates storage concerns from coordination concerns
  (`cpt-cf-quota-enforcement-adr-coordination-plugin` is the sibling decision).
- Bad, because each plugin owner carries the cost of their own conformance suite — benchmark, chaos drill, DR
  validation.

### Pin a specific backend in QE-core

- Good, because simplest possible mental model for QE-core readers.
- Bad, because locks every deployment to one backend; unsuitable for edge / embedded / specialized / regulated
  environments.
- Bad, because vendor-specific quirks leak into QE-core, complicating future migrations.
- Bad, because operators with strong opinions about ops infrastructure cannot adopt QE without forking it.

### Multi-backend federation in QE-core

- Good, because no operator-side plugin authoring required.
- Bad, because complexity explosion: cross-backend transactions, mutation routing, inconsistent failure modes, divergent
  acquisition ordering proofs per backend.
- Bad, because adds backends QE-core must understand and test, defeating the separation of concerns.

## More Information

The capability-level contract lives in DESIGN §3.5 «Required backend capabilities» plus the I1–I13 invariants on
`QuotaEnforcementStoragePluginV1` (DESIGN §3.3). Concrete plugin DESIGN (table / collection / file layouts, indexes,
partitioning, locking discipline, replication strategy, metadata-storage shape) is authored by the plugin owner
alongside the plugin crate; QE-core does not pin the file path.

## Traceability

- **PRD**: [PRD.md](../PRD.md)
- **DESIGN**: [DESIGN.md](../DESIGN.md)

This decision directly addresses:

- `cpt-cf-quota-enforcement-fr-pluggable-storage` — establishes the plugin-trait shape and forbids QE-core from locking
  to any specific backend.
- `cpt-cf-quota-enforcement-nfr-fault-tolerance` — RPO = 0 is a capability obligation; per-impl realisation varies.
- `cpt-cf-quota-enforcement-nfr-throughput` — throughput is a capability obligation; per-impl realisation varies.
- Sibling ADR `cpt-cf-quota-enforcement-adr-acquisition-ordering` — deterministic acquisition ordering as a capability;
  every plugin must respect it.
- Sibling ADR `cpt-cf-quota-enforcement-adr-coordination-plugin`: singleton coordination comes from the platform
  `cluster` gear; storage and coordination evolve independently.
