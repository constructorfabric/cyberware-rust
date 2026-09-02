# Technical Design — Throttling Follow-Up: Config-Owned Keys, a Real `max_keys` Bound, and OpenAPI Extension Recovery

Status: draft, pending review
Authorizing decisions: [ADR-0002](ADR/0002-throttling-model-config-boundary-and-portability.md), [ADR-0001](ADR/0001-distributed-throttling-cluster-cache.md)
Supersedes nothing. Scoped follow-up to the zone-based throttling middleware.

**On placement.** These documents live under `docs/arch/`, alongside
`docs/arch/errors/`, `docs/arch/authorization/` and `docs/arch/toolkit-*`, and
are *not* Constructor Studio SDLC artifacts. `.cf-studio/config/artifacts.toml`
registers artifacts only under `gears/$system/docs`, `gears/system/$system/docs`
and `gears/bss/$system/docs`; nothing under `docs/arch/**` or `docs/adrs/**` is
in the registry (`cfs validate --artifact` reports *not in registry* for every
path in both trees, including the pre-existing ones). Registering them would mean
SDLC-documenting the api-gateway gear as a whole: `gears/system/api-gateway/*` is
currently listed in `[[ignore]]` as "not SDLC-documented yet: missing required
PRD/DESIGN", `PRD.md` is `required = true` at that scope, and `traceability` is
`FULL`. That is a separate undertaking, deliberately not bundled here.

## 1. Architecture Overview

### 1.1 Architectural Vision

ADR-0002 chose Option A — a declarative, config-owned throttling policy with a
runtime-agnostic contract. That ADR deliberately records intent only and defers
concrete types, config schema, and migration to a follow-up design. This is that
follow-up, narrowed to the three items that are defects in the shipped code
rather than shape improvements:

1. **The contract layer must not depend on a runtime.** `ThrottlingSpec` stores
   `IdentityKeyFn = Arc<dyn Fn(&axum::extract::Request) -> String>`, which pulls
   `axum` into `libs/toolkit`'s contract surface. ADR-0002 states removing this
   is a hard requirement.
2. **`max_keys` must bound something.** `RateLimitZone.max_keys` is parsed,
   validated (`> 0`) and documented as a key-count bound, but no code path reads
   it. Rate-zone memory is bounded only by a 10-second recency sweep.
3. **The OpenAPI throttling extensions must survive the zone model.** The
   throttling commit removed the `x-rate-limit-*` vendor-extension emission
   together with its only test, silently breaking spec consumers (W3).

The end state: a throttling zone is fully described by configuration. Code binds
an operation to a zone by name and nothing else. The key strategy is data the
gateway compiles into extraction logic, which is the precondition for ever
emitting a zone to an external gateway (Kong / Envoy / AWS API Gateway).

### 1.2 Architecture Drivers

* **Runtime-agnostic contract** — `libs/toolkit` contract types must not name
  `axum` or any other runtime. Hard requirement on `feature/toolkit_contracts`.
* **Honest configuration** — a setting the operator can write, that the loader
  validates, must have an effect. A validated-but-ignored knob is worse than an
  absent one: it reports a protection that does not exist.
* **Bounded memory under hostile input** — pre-auth zones are keyed by client IP,
  which is attacker-influenced. The key set must be bounded by a value the
  operator controls, not only by arrival rate.
* **No behavioral change for current users** — the single in-tree consumer of
  `with_throttling` passes `identity_key_func: None`. Whatever replaces the
  closure must keep that path byte-identical.
* **Minimum surface** — no new public type is introduced for a capability that
  has no consumer.

### 1.3 Architecture Layers

```text
  libs/toolkit  (contract layer — no runtime types)
  ┌──────────────────────────────────────────────────────────────┐
  │  ThrottlingSpec                                              │
  │    rate_limit_zone:     Option<String>                       │
  │    in_flight_limit_zone: Option<String>                      │
  │    require_security_context: bool                            │
  │    dry_run: bool                                             │
  │                          ← no closure, no axum, derivable    │
  └──────────────────────────────────────────────────────────────┘
                                │ zone name
                                ▼
  gears/system/api-gateway  (config + enforcement)
  ┌──────────────────────────────────────────────────────────────┐
  │  ApiGatewayConfig.rate_limit_zones["rl_x"]                   │
  │    key: { type: identity | ip }   ← the whole key strategy   │
  │    max_keys: u64                  ← enforced, see §3.2       │
  │                                                              │
  │  middleware::throttling                                      │
  │    compute_key(KeyType, …)  ← compiles config into extraction│
  └──────────────────────────────────────────────────────────────┘
```

## 2. Principles & Constraints

### 2.1 Design Principles

* **Delete before you abstract.** The axum leak exists to support a
  customization point with zero consumers. Removing the customization point
  removes the leak without introducing a replacement type.
* **A knob means an invariant.** Every field the config loader validates must
  map to a runtime check. If no such check is practical, the field is removed
  rather than documented away.
* **The reviewer's stated property is the requirement**, not the shortest remedy
  they happened to list. `max_keys` is closed when the tracked key count cannot
  exceed the configured value — not when a periodic sweep exists.

### 2.2 Constraints

* `libs/toolkit` must not gain a dependency on the api-gateway gear; the current
  closure indirection exists to avoid that cycle. Moving the key strategy into
  gateway config dissolves the cycle instead of routing around it.
* `KeyType::Identity` requires an authenticated request. Config validation
  already rejects identity-keyed zones referenced from pre-auth operations
  (`require_security_context = false`); that rule is unchanged.
* Zones are shared across the pre-auth and post-auth partitions by one `Arc`
  each (`build_maps`); any key accounting added must be per-zone, not per
  partition, or the bound doubles.
* The distributed backend (ADR-0001) will replace per-key state entirely for
  rate zones. Whatever accounting is added here must be cheap to delete.

## 3. Key Decisions

### D1: Remove the closure rather than replace it with `enum Key`

ADR-0002 proposes replacing `IdentityKeyFn` with a serializable
`enum Key { Ip, Header(String), JwtClaim(String), … }`. This design does **not**
introduce that enum now.

Rationale: the key strategy already exists as data, in the zone config, as
`KeyType { Identity, Ip }`. The closure is an *override* of how the `Identity`
variant is extracted. A survey of the tree found no consumer of that override —
the only non-test caller (`gears/mini-chat/.../routes/chats.rs`) passes `None`,
and the `None` branch resolves the subject id from `SecurityContext`. Introducing
`enum Key` in the contract would therefore add a public type, a config schema
change, and a compile step for a capability nothing requests.

Removing the closure achieves ADR-0002's stated hard requirement — no `axum` in
the contract layer — with strictly less surface than adding the enum. Extra
variants (`Header`, `JwtClaim`) remain available later as a backward-compatible
addition to the existing `KeyConfig`, driven by a real requirement.

**Consequence:** `ThrottlingSpec` becomes a plain data struct — derivable
`Debug`, `Clone`, `PartialEq`, and serializable if needed — which is the actual
portability property ADR-0002 is after.

### D2: `KeyType` stays `{ Identity, Ip }` and stays in gateway config

The zone owns the key strategy. An operation names a zone; it does not describe
how that zone keys. This is the config/code boundary ADR-0002 asks for, applied
to keying.

`Identity` resolves to `SecurityContext::subject_id()`, falling back to
`"anonymous"` — the behavior the `None` branch has today. `Ip` resolves through
`client_ip()` with `trusted_proxy_hops`, unchanged.

### D3: `max_keys` becomes an enforced bound on rate zones

The reviewer's requirement is that the tracked key count cannot exceed the
configured limit. The current implementation prunes by recency every 10 seconds
(`retain_recent()` + `shrink_to_fit()`), which bounds staleness, not count.
Between ticks the key set grows with the arrival rate of distinct keys, which for
a pre-auth IP zone is attacker-influenced.

Note precisely what did and did not change since the review. The neighbouring
finding — an all-shard `DashMap::retain` on the request hot path — was addressed
by moving pruning to a background sweep. `get_or_build_rate_zone`, the function
the reviewer named, is unchanged: it reads `rate_limit.rps` and `burst_limit`,
stores the zone config wholesale (`cfg: cfg.clone()`), and never consults
`max_keys`. Both `cfg.max_keys` references in the middleware belong to the
in-flight gate. The rate-zone half of that review comment is open, not partially
closed.

`governor` 0.10.4's *public `RateLimiter` API* cannot enforce this — it exposes
`len()` but neither per-key removal nor a key-presence test. One level down the
crate does provide the lever: the `StateStore` trait is public and pluggable,
and a custom bounded store is feasible (see §3.2's rejected alternatives). The
chosen bound is instead a small admission structure owned by the gateway; the
mechanism, its costs, and why the custom store was rejected are in §3.2.

### D4: `max_keys` on in-flight zones is left as-is

The in-flight gate already reads `max_keys` (`prune_idle_keys` skips the scan
below the cap and evicts unreferenced gates above it) and is covered by tests. It
is a soft bound — a gate held by an in-flight request cannot be evicted — but
that ceiling is bounded by real concurrency, not by attacker-supplied key
cardinality. No change.

## 3.1 Work Item W1 — Remove the runtime dependency from the contract

**Removals**

| Symbol | Location |
|---|---|
| `IdentityKeyFn` type alias | `libs/toolkit/src/api/operation_builder.rs` |
| `ThrottlingSpec::identity_key_func` field | `libs/toolkit/src/api/operation_builder.rs` |
| manual `impl Debug for ThrottlingSpec` | `libs/toolkit/src/api/operation_builder.rs` |
| `trait IdentityExtractor` | `gears/system/api-gateway/src/middleware/throttling.rs` |
| `fn identity_key_fn` adapter | `gears/system/api-gateway/src/middleware/throttling.rs` |

**Changes**

* `ThrottlingSpec` gains `#[derive(Debug)]` (and `PartialEq`, if no field
  blocks it) now that no `dyn Fn` field remains.
* `compute_key` loses its `map_or_else` over the extractor; `KeyType::Identity`
  resolves `SecurityContext` directly.
* `libs/toolkit/src/api/operation_builder.rs` drops its `axum::extract::Request`
  usage in the contract type. Verify no other contract-layer symbol reintroduces
  it before claiming ADR-0002's requirement closed.

**Call sites to update**

* `gears/mini-chat/mini-chat/src/api/rest/routes/chats.rs` — drop the
  `identity_key_func: None` field from the struct literal. No behavior change.
* Tests referencing the extractor: `builder_with_throttling_sets_spec_and_extractor`
  (toolkit), `compute_key_identity_uses_extractor_then_subject`,
  `StaticIdentity` and the `thr(…)` helper (gateway).

**Test consequences**

`compute_key_identity_uses_extractor_then_subject` currently asserts the
extractor is preferred over the subject id. With the extractor gone, the
remaining behavior — identity resolves to `subject_id()`, `"anonymous"` when no
`SecurityContext` is present — must still be asserted; rename accordingly rather
than delete. `builder_with_throttling_sets_spec_and_extractor` loses its reason
to exist in its current form; what survives is a `Debug`/`Clone` round-trip on a
now-plain struct, which is low value — prefer deleting it over keeping a test
that asserts derive macros work.

**Acceptance**

* `rg 'axum' libs/toolkit/src/api/operation_builder.rs` returns nothing in the
  contract types (imports for handler plumbing elsewhere in the file are out of
  scope and must be judged individually, not by grep alone).
* `cargo test -p cf-gears-toolkit -p cf-gears-api-gateway` green.
* No config schema change; existing YAML keeps working untouched.

## 3.2 Work Item W2 — Make `max_keys` bound the rate-zone key set

**Verified capability of `governor` 0.10.4** (pinned via workspace `governor = "0.10"`),
from `src/state/keyed.rs`:

| Primitive | Available | Notes |
|---|---|---|
| `RateLimiter::len()` | yes | bounded on `S: ShrinkableKeyedStateStore<K>`; documented as possibly an estimate or out of date |
| `RateLimiter::is_empty()` | yes | same caveat |
| `retain_recent()` | yes | bulk; drops only keys whose state is indistinguishable from fresh |
| `shrink_to_fit()` | yes | capacity only |
| per-key removal | **no** | no `remove`, no eviction by key |
| key-presence test | **no** | no `contains_key` on the keyed API |

The table describes the public `RateLimiter` API: at that level there is no
containment test and no per-key removal, so the limiter cannot be bounded from
the outside. It does **not** follow that `governor` cannot be bounded at all —
the `StateStore` trait is a public extension point (`RateLimiter::new(quota,
state, clock)` is public, `KeyedStateStore` is blanket-implemented for any
`StateStore<Key = K>`, and `measure_and_replace`'s closure receives
`Option<Nanos>`, which is `None` exactly when a key is new). A custom bounded
store is therefore possible; it is considered and rejected below.

**Chosen mechanism — bounded admission set owned by `RateZone`.**
This is the reviewer's second suggested remedy ("a bounded keyed store").

* `RateZone` gains a key set (a `DashMap<String, ()>` or equivalent) capped at
  `max_keys`.
* On each request the middleware consults the set first. A key already present
  proceeds to `check_key` unchanged. A key not present is admitted only while
  the set is below `max_keys`; the insertion is what accounts for the key.
* When the set is full, an unknown key is rejected with the zone's configured
  status **without** reaching `check_key`, so `governor`'s internal map cannot
  grow past the admitted set.
* Unadmitted keys never reach `check_key` in **any** mode: enforce mode rejects
  them, dry-run mode logs the would-be rejection and serves the request without
  consulting the limiter. Feeding unadmitted dry-run keys to the limiter would
  create keyed-store state past the cap, unbounding memory exactly in the mode
  operators use for tuning.
* The background sweep clears the admission set in the same tick it calls
  `retain_recent()` / `shrink_to_fit()`, keeping the two views from drifting.
  Clearing rather than selectively pruning is deliberate: without a containment
  test on `governor`'s side there is no way to prune the two structures in
  agreement, and a full clear is self-correcting within one interval. The known
  residual desynchronization: keys still active enough to survive
  `retain_recent()` stay in the limiter while admission reopens, so combined
  state is transiently bounded by ~2× `max_keys` immediately after a sweep,
  converging within one interval. Eliminating that window requires the
  custom-`StateStore` variant below, where admission and eviction live in one
  structure by construction.

Cost: one extra hash lookup on the throttling path, and a duplicated copy of the
key string per tracked key. The duplication is the price of a bound that
`governor` cannot provide; peak memory becomes `O(max_keys)` in both structures
rather than unbounded in one.

**Rejected alternatives**

* *Custom bounded `StateStore` plugged into `governor`* — a `DashMap`-backed
  store implementing `StateStore` + `ShrinkableKeyedStateStore` with a hard cap
  at insertion and staleness-based eviction (sample-K over the stored `Nanos`).
  Strictly stronger than the admission set: exact bound, no second copy of the
  key, no extra hot-path lookup, and graceful eviction of the stalest key
  instead of rejecting new clients. Rejected for this PR because
  `InMemoryState`'s CAS methods are `pub(crate)`, so the store must hand-write
  its own `compare_exchange_weak` loop over `AtomicU64` — bespoke lock-free
  concurrency plus semantic coupling to GCRA state encoding is a
  review-and-maintenance burden out of proportion to a follow-up PR. Recorded
  here as the natural upgrade path if admission-set behavior under saturation
  proves problematic in production.
* *Adaptive sweep only* — shorten the prune interval as `len()` approaches
  `max_keys`. Cheap, never rejects a legitimate client, requires no second
  structure. Rejected as the primary mechanism because it bounds the key set by
  `arrival_rate × interval`, not by `max_keys`; it is a better sweep, not a
  bound, and does not satisfy the stated requirement. Worth adding alongside the
  admission set as a second-order improvement.
* *Reject all traffic on a saturated zone* — avoids needing a containment test
  by rejecting every request while `len() >= max_keys`. Rejected: it converts a
  key-cardinality attack into a full outage for that zone, which is a worse
  failure than the one being fixed.

**Additional requirements**

* Saturation must be observable: capacity rejections go through the existing
  `record_rejection` path (counter + `info` log with low-cardinality `zone`
  attributes) with a distinguishing `kind = "max_keys"`, so an operator can
  tell a real rate rejection from a capacity rejection. ADR-0001 already
  anticipates a reason label on this counter; this is a compatible shape.
* Capacity rejections carry `Retry-After` equal to the prune interval — the
  upper bound on how long admission stays closed.
* Accounting is per zone (`Arc<RateZone>`), shared across both auth partitions,
  consistent with `shared_zone_arc_across_partitions`.
* Tests: admission refuses a new key beyond `max_keys` while admitted keys keep
  flowing, and admission reopens after the sweep resets the set (unit +
  end-to-end through the middleware).
* The cap is documented as approximate: the check-then-insert is not atomic
  (concurrent races can overshoot by a few entries), dry-run operations still
  feed unadmitted keys to the limiter, and the sweep resets admission while the
  limiter may retain recently-active keys. Bounding memory growth, not a
  precise count, is the goal; making it exact would require locking the hot
  path.

**Alternative considered and rejected: remove the field.** Deleting `max_keys`
from `RateLimitZone` and documenting recency pruning as the only bound is
cheaper and honest. It is rejected because it removes the operator's only lever
against key-cardinality exhaustion on precisely the zones that face untrusted
input, and because it is a breaking config-schema change for a field already
shipped in example configs (`max_keys: 50000`).

## 3.3 Work Item W3 — Restore throttling OpenAPI vendor extensions

`main` emitted `x-rate-limit-rps` / `x-rate-limit-burst` on throttled
operations from `openapi_registry.rs`. The zone model removed both the emission
and its only test in the same commit, so the regression was invisible to CI.
The numbers now live in `ApiGatewayConfig`, which the contract layer must not
see (ADR-0002) — but `ApiGatewayGear::build_openapi()` has the config in scope.

**Mechanism — layered enrichment, zone name as the join key.**

* *Toolkit* (`openapi_registry.rs`): operations with a `ThrottlingSpec` gain
  `x-throttling-rate-limit-zone` / `x-throttling-in-flight-limit-zone` vendor
  extensions carrying the zone names — the only throttling facts the contract
  layer owns.
* *Gateway* (`gear.rs::build_openapi`): after the registry builds the document,
  a post-pass reads `x-throttling-rate-limit-zone` from each operation, looks
  the zone up in `config.rate_limit_zones`, and adds `x-rate-limit-rps` /
  `x-rate-limit-burst` — restoring `main`'s contract verbatim. The zone name is
  the join key, so no route-matching logic is duplicated; an unknown zone name
  is skipped silently (zone-reference validation already fails the router
  build).

Each layer emits only what it knows: the contract emits the binding, the
gateway emits the numbers. The ADR-0002 boundary is untouched from both sides.

**Tests**: toolkit — zone names emitted, absent without a `ThrottlingSpec`;
gateway — numbers added by zone name, unknown zone and unthrottled operations
untouched.

## 4. Out of Scope

Recorded so the boundary is explicit rather than implied by omission.

| Item | Why deferred | Tracked by |
|---|---|---|
| `x-in-flight-limit` numeric extension | W3 restores the rate-limit numbers `main` published; `main` never emitted in-flight numbers and no consumer is known. The zone-name binding (`x-throttling-in-flight-limit-zone`) is emitted; numbers can be added by the same gateway post-pass when a consumer appears | W3 |
| Per-replica limit multiplication | In-process state means a configured `N/s` admits `N × replicas`. Requires the cluster-cache backend | ADR-0001 |
| `dry_run` and `operation → zone` binding moving into config | Operator-facing ergonomics, not a defect; part of ADR-0002 Option A | ADR-0002 §Decision Outcome 1 |
| `tower::Layer` composition and group-level zones | Restructures enforcement; independent of the two defects here | ADR-0002 §Decision Outcome 3 |
| `enum Key` with `Header` / `JwtClaim` variants | No consumer; additive to `KeyConfig` when one appears | D1 |
| In-flight limiter distribution | Different problem (live lease, closer to a distributed lock) | ADR-0001 §Scope |

## 5. Risks / Trade-offs

### [Risk] The admission bound penalizes new keys under saturation

Once a zone's admission set is full, a legitimate new client is rejected while an
established abuser already inside the set continues at its configured rate. This
is inherent to any hard count bound — the alternative is admitting unbounded
keys — and is why the bound must be observable and the default `max_keys`
generous. Document the interaction; do not silently ship it.

The full-clear on each sweep bounds how long a client stays locked out to one
prune interval, but it also means an established client loses its tracked rate
state at the same moment. Under sustained saturation this makes admission
roughly round-robin by arrival rather than stable per client. Accept it as the
cost of a hard bound, or revisit if a zone in production shows churn.

### [Risk] `max_keys` semantics differ between zone kinds

After this work, `max_keys` is a hard bound on rate zones and a soft bound on
in-flight zones. Two meanings for one field name is a documentation hazard.
Mitigation: state the difference in both zone structs' doc comments, or rename
one. Renaming is a config-schema break and is not proposed here.

### [Trade-off] Removing the extractor closes an extension point

A gear that genuinely needs custom identity extraction now has no path until
`KeyConfig` grows a variant. Accepted: there is no such gear, and the config
route is the one ADR-0002 wants. Reopening it means adding data variants, not
restoring a closure.

### [Constraint] W2 must be cheap to delete

ADR-0001's distributed backend replaces per-key state for rate zones outright.
Any accounting introduced by W2 should live in `RateZone` behind the same seam
the distributed backend will replace, so it does not become an obstacle to that
work.

## 6. Traceability

| Requirement | Source | Work item |
|---|---|---|
| Contract must not depend on a runtime | ADR-0002 §Decision Drivers, §Decision Outcome | W1 |
| Sentinel `""` → `Option` | ADR-0002 §Decision Outcome | already done in the shipped branch |
| `max_keys` bounds the tracked key count | PR #4171 review, `throttling.rs:446` | W2 |
| Prune off the hot path | PR #4171 review, `throttling.rs:126` | already done (background pruner) |
| Rejection metric + `info` level | PR #4171 review, `throttling.rs:579` | already done; extended by W2 |
| Trusted-proxy client IP derivation | PR #4171 review, `throttling.rs:523` | already done (`trusted_proxy_hops`) |
| OpenAPI throttling extensions survive the zone model | `main` `openapi_registry.rs` (removed by the throttling commit) | W3 |
