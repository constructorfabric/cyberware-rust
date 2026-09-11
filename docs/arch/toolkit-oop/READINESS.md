# OoP Gear Readiness

Sweep of every declared `#[toolkit::gear]` in the repo:

1. **What it is** - `examples`, `system`, or `business`.
2. **Where it is in the demo** - `oop` (own pod), `platform-host` (bundled in the host image), or `not incl` (not wired into any deployment).
3. **Scaling** - can it run as *N* pods? (orthogonal to OoP: a gear can own a pod yet still be pinned to one replica).
4. **Why it is there** - the concrete blocker keeping it out of its own pod.
5. **To go standalone OoP** - the minimum work to give it its own pod (and, where it differs, to scale it).

**Blocker shorthand** (the "why"):
- `hard-dep` - compile-time `deps=[...]` / Rust crate link forcing co-location.
- `no-contract` - other gears need it but it exposes no `#[toolkit::contract]`.
- `trust-coupled` - uses synthetic/anonymous `SecurityContext` across the chain.
- `db-coupled` - reads another gear's tables (or another gear reads its DB).
- `no-oop-bin` - no `oop_module`-gated binary.
- `no-chart` - no standalone Helm chart.
- `no-authn` - authenticated but no embedded tenant-plane authn stack.
- `plumbing` - transport/discovery seed; co-location is intentional.

**Scaling shorthand** (the "N pods?" column):
- `scalable` - stateless or shared-store (Postgres / object store); any replica count.
- `rebuildable` - in-memory, but repopulated per-replica by re-registration/heartbeat.
- `leader` - request path scales; a background role is leader-gated (`chat-engine` pattern).
- `config-gated` - scales only on the remote/clustered store option, not the local one.
- `singleton` - coordination primitive; single / leader by design.
- `embedded-locked` - **scaling blocker**: in-memory/local-disk only, no externalization.

---

## Examples - the reference set (all already standalone)

These exist to *be* the OoP pattern. They are the "done" bar every other gear is measured against.

| Gear | Demo | Scaling | Why there | To go standalone OoP |
|---|---|---|---|---|
| `hello` | oop | scalable | Reference: zero deps, anonymous. | ✅ Done - minimal template. |
| `users-info` | oop | scalable | Reference consumer: `#[toolkit::consumes] AuthZResolverApi`, own DB. | ✅ Done - the canonical authenticated + DB-backed pattern. |
| `api-contracts` | oop | scalable | Reference provider: `#[toolkit::provides] PaymentApi/PaymentApiV2` over REST. | ✅ Done - provider half of the OoP↔OoP pair. |
| `api-contracts-consumer` | oop | scalable | Reference consumer of `api-contracts`. | ✅ Done - consumer half of the pair. |

---

## System - platform core & plumbing

Everything in `gears/system/*` plus `credstore` (`[system]` capability). Split by *why* they sit where they do.

### In `platform-host` - plumbing / discovery seed (co-location intentional)

| Gear | Demo | Scaling | Why there | To go standalone OoP |
|---|---|---|---|---|
| `grpc-hub` | platform-host | rebuildable | `plumbing` - serves the DirectoryService gRPC transport. | Stays put; it *is* the transport backplane. Only split for HA sharding. |
| `gear-orchestrator` | platform-host | rebuildable | `plumbing` - DirectoryService itself; everything resolves *through* it via a raw `DirectoryGrpcClient`, not `#[toolkit::consumes]`. Bootstrap chicken/egg. | DESIGN marks it OoP-eligible (REST discovery surface); realistically the discovery seed stays in the host pod (usually 1 replica). |
| `api-gateway` | platform-host | rebuildable | edge reverse-proxy; `hard-dep` `deps=[grpc_hub, authn_resolver]`. Route table rebuilt from registrations per replica. | OoP-eligible as a separate edge pod (Mode A). Needs its own chart + directory-driven route registration; or drop it (Mode B: external gateway). |
| `authn-resolver` | platform-host | scalable | Embedded **by design** in every OoP pod (§5); `hard-dep` `deps=[types_registry]`. | Never extracted - each authenticated pod embeds its own copy. Requirement is "embed it", not "pod it". |

### In `platform-host` - trust-coupled core (needs identity migration)

Uses synthetic/anonymous `SecurityContext` internally; cannot cross a network boundary until the chain moves to IdP/S2S credentials.

| Gear | Demo | Scaling | Why there | To go standalone OoP |
|---|---|---|---|---|
| `resource-group` | platform-host | scalable | `no-contract` + `db-coupled` (tenant-resolver reads its DB); `hard-dep` `deps=[authz_resolver, types_registry]`. | Add a REST contract (hierarchy reads) → unblocks authz-resolver + tenant-resolver. Then own DB + chart. |
| `authz-resolver` | platform-host | scalable | `trust-coupled` - chains authz→TR→RG via `SecurityContext::anonymous()`; `hard-dep` `deps=[types_registry]`. Already `provides` REST `AuthZResolverApi`. | RG REST surface + IdP-issued credentials for the authz→TR→RG chain; swap `deps=[types_registry]` → `consumes`. |
| `tenant-resolver` | platform-host | scalable | `db-coupled` - `rg-tr-plugin` reads resource-group's DB directly; `no-contract`; `hard-dep` `deps=[types_registry]`. | Re-point `rg-tr-plugin` at resource-group's REST contract; add own contract/surface. |
| `account-management` | platform-host | scalable | `trust-coupled` - synthetic `am.system` actor for background flows; `hard-dep` `deps=[authz, types, rg, tenant]`; `no-contract`. | Migrate `am.system` → real S2S client-credentials (the one blocker not solved by "add a contract"); then contract + consumes + chart. |

### In `platform-host` - OoP-eligible, just not extracted yet

| Gear | Demo | Scaling | Why there | To go standalone OoP |
|---|---|---|---|---|
| `types-registry` | platform-host | scalable | `no-contract`; embedded by many gears. The link-time in-memory GTS inventory is deterministic per replica (built from compiled-in `inventory` collectors + static config, not from runtime registration), and the admission path is Postgres-backed (migrations, version-family locks, CAS on `resource_version`, outbox). | Add REST contract (register + query) + `consumes` wiring to remove the embedding crutch. Residual scaling work: **leader-gate the outbox worker** (`NullDispatch` today) and **serve reads from the DB** (unify the in-memory inventory and admission paths). |
| `credstore` | platform-host | scalable | `no-contract`; `hard-dep` `deps=[authz, tenant, types]`. Stateless, so OoP-eligible per DESIGN. | Add REST contract (secret retrieval, mTLS on the wire) → also unblocks `oagw`; swap deps → consumes; own chart. |

### Not in the demo - extractable system gears

| Gear | Demo | Scaling | Why there | To go standalone OoP |
|---|---|---|---|---|
| `cluster` | not incl | singleton | `no-chart` only. Has `cluster-oop` bin, `k8s-auth`, and 4 gRPC contracts. Coordination primitive → single/leader by design. | **Closest system gear to done** - just add a standalone Helm chart + wire into the umbrella. |
| `nodes-registry` | not incl | embedded-locked | Plain REST, zero deps, zero auth - simply never converted. Store is in-memory `RwLock<HashMap>`, no persistence. | Add oop bin + chart **and** externalize the store (or make it `rebuildable` via node re-registration) - else 1 replica only. |
| `event-broker` | not incl | config-gated | `hard-dep` `deps=[cluster]`, resolves `cluster-sdk` facades from `ClientHub` in-process. `DeploymentMode` standalone is single-node. | Wire onto cluster's gRPC contracts remotely (architecture change, not a mechanical deps→consumes swap); run the clustered mode to scale. |
| `usage-collector` | not incl | scalable | `hard-dep` `deps=[types_registry, authz_resolver]`; plugin-owned storage. | Swap both deps → consumes (needs types-registry contract); add oop bin + authn stack + chart. |
| `oagw` | not incl | scalable | `hard-dep` `deps=[types, authz, credstore, tenant]`; `no-contract`. | Blocked on credstore + types-registry contracts; then deps→consumes, authn stack, chart. |

---

## Business - application gears (none in the demo)

Everything under `gears/*` that isn't `system` and isn't `credstore`. All `not incl`.

| Gear | Demo | Scaling | Why there | To go standalone OoP |
|---|---|---|---|---|
| `bss-rate-provider` | not incl | scalable | `hard-dep` `deps=[types_registry]` only; anonymous. | Simplest business blocker - just the types-registry contract, then oop bin + chart (no authn needed). |
| `file-parser` | not incl | scalable | No deps; `no-oop-bin`, `no-authn`, `no-chart`. Stateless. | `hello`-shape: add oop bin + embedded authn stack + chart. No dep untangling. |
| `simple-user-settings` | not incl | scalable | `hard-dep` `deps=[authz_resolver]`. | Swap deps → `consumes AuthZResolverApi` (like `users-info`); add oop bin + authn stack + own DB + chart. |
| `file-storage` | not incl | config-gated | `hard-dep` `deps=[authz_resolver]`; stateful, metadata DB + pluggable blob backend. | deps → consumes + full OoP kit + interim s2s finalize/report-part secret → real S2S. **Scale on `S3Backend`; `LocalFsBackend`/`InMemoryBackend` are single-node.** |
| `github-mirror` | not incl | scalable | `hard-dep` `deps=[authz_resolver]`; DB. | deps → consumes; oop bin + authn stack + own DB + chart. |
| `chat-engine` | not incl | leader | `hard-dep` `deps=[authz_resolver]`; DB; has its own `k8s` leader-election feature (unrelated to `k8s-auth`). | deps → consumes; oop bin + authn stack + own DB + chart. Scaling already solved: request path is stateless, cleanup loop is `LeaderElector`-gated. |
| `bss-pricing` | not incl | scalable | `hard-dep` `deps=[types_registry, authz_resolver]`; DB. | Needs types-registry contract; deps → consumes; oop bin + authn stack + own DB + chart. |
| `bss-ledger` | not incl | scalable | `hard-dep` `deps=[types, authz, account_management]`; DB. | Blocked by `account-management` S2S migration; then deps → consumes + full OoP kit. |
| `mini-chat` | not incl | leader | `hard-dep` `deps=[types, authn, authz, oagw]`; DB + outbox/background workers. | Blocked by `oagw` (itself blocked by credstore/types contracts). Deepest chain. **Leader-gate the outbox/workers (copy `chat-engine`) before >1 replica.** |

---

## Key levers

1. **`resource-group` REST contract** → unblocks authz-resolver + tenant-resolver.
2. **`types-registry` contract + `consumes` wiring** → removes the
   embed-everywhere crutch; unblocks usage-collector, bss-rate-provider, oagw,
   bss-pricing. (State is not part of this lever: the admission path is
   Postgres-backed - see the scaling note.)
3. **`credstore` contract** → unblocks `oagw` (→ then `mini-chat`).
4. **`account-management` `am.system` → S2S** → the only identity-design blocker;
   unblocks `bss-ledger`.
5. **Cheapest wins today:** `cluster` (chart only), `nodes-registry` (zero deps),
   `bss-rate-provider` / `file-parser` (one or zero deps).

**Scaling note:** OoP-readiness and horizontal scaling are separate axes. Only
`nodes-registry` is genuinely `embedded-locked`; `file-storage` / `event-broker`
just need the scalable backend selected; `mini-chat` needs the leader-gating
pattern `chat-engine` already proves. The in-memory system registries are
`rebuildable`, not blockers. `types-registry` is not state-blocked: its
admission path is Postgres-backed and its inventory is deterministic per
replica - its only residual scaling task is leader-gating its outbox worker (the
`chat-engine` pattern). The one true horizontal-scaling blocker in the host is
the **DirectoryService** (`gear-orchestrator` + `grpc-hub`): its instance
registry is an in-memory `GearManager` map with no cross-replica sync, so behind
a single service N replicas hold divergent partial views. It scales only
once that registry is externalized to a shared store (with leader-gated
heartbeat eviction), gossiped between replicas, or deliberately kept a single /
leader-elected singleton.
