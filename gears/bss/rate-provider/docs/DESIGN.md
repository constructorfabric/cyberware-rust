Created:  2026-07-17 by Virtuozzo International GmbH
Updated:  2026-07-17 by Virtuozzo International GmbH

<!-- CONFLUENCE_TITLE: [BSS]: FX Rate Provider (Adapter Gear) — Technical Design -->
<!-- Related: ../../ledger/docs/DESIGN.md, ../../ledger/docs/design/06-fx-multicurrency.md, ../../ledger/docs/PRD.md | Owners: @vstudzinskyi (BSS Billing Platform team) -->

# Technical Design — FX Rate Provider (Adapter Gear)

<!-- toc -->

- [1. Architecture Overview](#1-architecture-overview)
  - [1.1 Architectural Vision](#11-architectural-vision)
  - [1.2 Architecture Drivers](#12-architecture-drivers)
  - [1.3 Architecture Layers](#13-architecture-layers)
- [2. Principles & Constraints](#2-principles--constraints)
  - [2.1 Design Principles](#21-design-principles)
  - [2.2 Constraints](#22-constraints)
- [3. Technical Architecture](#3-technical-architecture)
  - [3.1 Domain Model](#31-domain-model)
  - [3.2 Component Model](#32-component-model)
  - [3.3 API Contracts](#33-api-contracts)
  - [3.4 Internal Dependencies](#34-internal-dependencies)
  - [3.5 External Dependencies](#35-external-dependencies)
  - [3.6 Interactions & Sequences](#36-interactions--sequences)
  - [3.7 Database schemas & tables](#37-database-schemas--tables)
  - [3.8 Deployment Topology](#38-deployment-topology)
- [4. Additional context](#4-additional-context)
  - [Security & AuthZ](#security--authz)
  - [Feature metrics](#feature-metrics)
  - [Testing architecture](#testing-architecture)
  - [Decision register](#decision-register)
  - [Companion ledger change (hard dependency, from O-3)](#companion-ledger-change-hard-dependency-from-o-3)
- [5. Traceability](#5-traceability)

<!-- /toc -->

- [ ] `p1` - **ID**: `cpt-cf-bss-rate-provider-design-main`

> **Canonical design entry point.** This document is the FX rate-provider gear's technical
> design and the anchor for spec traceability. The gear is small enough that the design is
> **self-contained** — there is no slice set; component, contract, and sequence detail is
> normative here.
>
> **Status**: DRAFT — decisions recorded (O-1 & O-3 decided; all other defaults accepted
> 2026-07-08). Ready for implementation planning. The O-3 companion `bss-ledger` change
> (§4 "Companion ledger change") is a linked hard dependency.
>
> **Implementation revision (2026-07-23) — `*-plugin` pattern.** The gear was
> built on the platform's plugin pattern (types-registry `PluginV1` instances +
> `ClientHub` scoped registration + host-side discovery), which revises three
> decisions below. This note is authoritative where it conflicts with the older
> prose:
>
> - **Two crates → four.** Instead of one composite gear with a config `sources[]`
>   list, each source is its own **plugin gear** — `bss-rate-provider-ecb-plugin`
>   and `bss-rate-provider-http-json-plugin` — and a **core gear**
>   (`bss-rate-provider`) discovers and composes them. Shared source utilities
>   (conversion, error mapping, fetch metrics) and the source-plugin GTS spec live
>   in `bss-rate-provider-sdk`.
> - **O-1 revised — scoped plugin registration, two layers.** *Level 1:* each
>   source plugin registers a `PluginV1<RateProviderSourcePluginSpecV1>` instance
>   in the types-registry and a scoped `RateProviderV1` client (`priority` = the
>   fallback order). The core gear lists those instances, orders by `priority`,
>   resolves each via `get_scoped`, and composes them (first whole successful
>   document; provenance stamped per rate — see §3.2). *Level 2:* the core gear
>   registers its composite as a `PluginV1<RateProviderPluginSpecV1>` (spec in
>   `bss-ledger-sdk`) + scoped `RateProviderV1`; the ledger discovers **that**.
> - **O-7 revised — no `deps` edge.** The ledger discovers the composite **lazily
>   on every `RateSyncJob` tick** (types-registry `list_instances` → vendor/priority
>   select → `get_scoped`), falling back to `UnconfiguredRateProviderV1`. A
>   late-registered adapter self-heals on the next tick, so the startup-ordering
>   concern is gone and the ledger stays decoupled (schedulable without the adapter).
> - **O-12 revised — per-plugin config, not `sources[]`.** Source assembly is now
>   plugin discovery ordered by each plugin's `priority`; the `fx.provider_order` /
>   `sources[]` cross-gear order check is replaced by matching `vendor` between the
>   core gear, its source plugins, and the ledger's `fx.provider_vendor`. Unknown
>   `kind` no longer exists (each source is a distinct plugin crate). Config
>   validation is per plugin (http-json requires `mapping` + https `base_url`).
>
> The domain model (§3.1), the fetch-only/direct-pairs/deterministic-conversion
> principles (§2.1), the metrics (§4), and the O-3 triangulation boundary are
> unchanged.

## 1. Architecture Overview

### 1.1 Architectural Vision

The **FX rate-provider gear** is a **stateless adapter**: it implements the ledger's
`RateProviderV1` contract (`bss-ledger-sdk`, GTS `gts.cf.bss.ledger.rate-provider.v1`) and
registers an `Arc<dyn RateProviderV1>` into the platform `ClientHub`, so the ledger's
background `RateSyncJob` resolves a live adapter instead of the fail-safe
`UnconfiguredRateProviderV1` default. The registered instance is a
**`DiscoveringRateProvider`** that lazily discovers source plugins and builds a
**`CompositeRateProvider`** to try its ordered sources in the PRD-ratified provider
order (2026-06-10) and returns the **first whole document** that succeeds (one source per
document, never a merge; O-1 — a rule about provenance, not about coverage, see §2.1). The
fallback **mechanism** ships in v1, but the **v1
configuration is ECB-only** (O-10): the bank / PSP feed is a *future* source, added later
by deploying/configuring one more source-plugin gear (its own crate, its own `vendor` +
`priority`) — no change to this gear's code. It performs no persistence, no HTTP surface,
and no accounting logic.

**This gear only fetches rates.** The ledger declares the FX rate provider out of its own
scope ("The FX rate provider itself (integration, feeds) — external; the ledger consumes
rates and snapshots them",
[`../../ledger/docs/design/06-fx-multicurrency.md`](../../ledger/docs/design/06-fx-multicurrency.md))
and already ships the consuming seam: the `RateProviderV1` SDK trait, the `RateSyncJob`
that pulls it, the `ledger_fx_rate` local store, the lock-time `RateSource`, staleness /
provider-precedence resolution, and the immutable `rate_snapshot`. Everything ledger-owned
stays there and is NOT restated here:

- Functional-currency **translation** and the dual-column balance.
- **Triangulation** through EUR (X→EUR→Y) — ledger-owned (O-3); requires the companion
  ledger change (§4 "Companion ledger change").
- **Staleness** rules (G10 > 24 h; others ≤ 7 d) and `stale` marking.
- **Provider precedence / fallback-order** resolution over the local store.
- **`rate_snapshot`** freezing, `ledger_fx_rate` upsert, per-tenant fan-out.
- Realized / unrealized FX, revaluation runs.
- The `RateSyncJob` tick cadence and its `FX_SNAPSHOT_MISSING` alarm (ledger-side).

Also out of scope: **pricing-side FX / rate-lock governance** (Catalog module) and
**provider commercial contracts / credential procurement** (ops).

### 1.2 Architecture Drivers

Requirements from the ledger [`PRD.md`](../../ledger/docs/PRD.md) and the FX slice design
that significantly shape this gear.

#### Functional Drivers

| Requirement | Design Response |
|-------------|-----------------|
| `cpt-cf-bss-ledger-fr-multi-currency-fx` | The ledger needs a live rate feed to translate transaction currency into functional currency; this gear supplies it through the fixed `RateProviderV1` seam — implement `provider_id()`, `fetch_latest()`, `health()` (§3.3). |
| `cpt-cf-bss-ledger-fr-fx-rate-source-failure` | Provider outage must never produce a silent wrong rate. The composite returns the last `RateProviderError` when **all** sources fail; the ledger job then alarms and FX posts block (`FX_RATE_UNAVAILABLE`) — fail-safe by absence (§2.2). |
| Rate-source fallback ([ledger FX design](../../ledger/docs/design/06-fx-multicurrency.md)) | The ledger resolves precedence over its **local store**; cross-source fallback at fetch time is this gear's `CompositeRateProvider` — ordered sources, first whole successful document, true-source provenance (§3.2). |
| Provider onboarding without code change | Plugin-discovery source assembly: the core gear discovers registered source-plugin instances by `vendor`, ordered by `priority`; a plain REST feed is onboarded by configuring `bss-rate-provider-http-json-plugin` alone (no code), a new provider *family* costs one new plugin crate (§3.2). |

#### NFR Allocation

| NFR theme | Allocated to | Design Response |
|-----------|--------------|-----------------|
| Post-path isolation (hard) | Consumption model | `fetch_latest` is called only by the background `RateSyncJob`, never on the posting path; a provider outage fails the job (ledger alarms), never a post. |
| Feed freshness | Fetch path + ledger tick | A successful fetch SHOULD complete within one `rate_sync_tick` (ledger default 1 h) so G10 pairs never cross the 24 h staleness window under normal operation. |
| Fetch latency | Sources + HTTP client | `fx_provider_fetch_duration_seconds{provider}` p95 ≤ 2 s **per source** (draft; confirm against ECB response times). One bounded attempt per source per call, no unbounded retry. The composite's worst case is the **sum of the configured per-source `timeout_ms`** (every source down). No shared total deadline is imposed in v1 — see the budget rule below for why that is safe and where it stops being safe. |
| Availability | Ledger fail-safe | Best-effort; the ledger's fail-safe (block, not guess) absorbs adapter downtime. |

**Fetch-budget rule.** Both the number of sources and each `timeout_ms` are
configuration-driven, so the worst-case tick duration is not bounded by anything in the
code. The rule that keeps it safe is a budget, not a deadline:

> **sum of all configured `timeout_ms` MUST stay below one `rate_sync_tick`**, with an
> order of magnitude to spare.

At the defaults that is comfortable — 5 s per source against a 3600 s tick, so ~720 sources
would be needed to reach the interval. It stops being comfortable in exactly two setups: a
tick shortened to seconds, or per-source timeouts raised into the minutes. Both are
deliberate operator choices, and both are worth checking against this rule before rollout.

The **per-source half** of that budget is enforced in code: `build_source_http_client`
rejects any `timeout_ms` above `MAX_TIMEOUT_MS` (30 s) at the plugin's `init()`, so a single
misconfigured source cannot consume a tick on its own. The sum itself is deliberately not
checked, because no single `init()` can see both halves of the rule — the timeouts live in
each source plugin's config while the interval lives in the ledger's. The ceiling therefore
catches the failure that actually occurs (a unit mix-up: seconds written into a milliseconds
field) and leaves the genuinely cross-gear part of the rule to this document.

No shared deadline is enforced in v1 because it would buy nothing on the failure path this
gear actually has: the fetch runs only inside the background `RateSyncJob`, never on the
posting path, so a slow tick delays a rate refresh but blocks no post. Overrunning the
interval is the ledger scheduler's concern — it does not start a second concurrent tick,
so an overrun manifests as a *skipped* refresh, not overlapping fetches.

An overrun is measured where it happens, on the ledger side: it times the whole pass into
`ledger_fx_rate_sync_duration_seconds` and alerts that p95 against the configured interval
(`FxRateSyncOverrunning`). Nothing in this gear can substitute for that series — the
per-source `fx_provider_fetch_duration_seconds` times one HTTP round-trip, while the pass
costs the sum of every source tried plus discovery. What this gear's own
`FxNoFetchAttempted` covers is the different, more severe state where
`fx_provider_fetch_duration_seconds_count` stops advancing entirely (§4 "Feature metrics"),
which against the ledger's still-rising tick heartbeat reads as skipped refreshes rather
than a dead job.

If a deployment ever needs the harder guarantee, the place to add it is a composite-level
deadline in `CompositeRateProvider::fetch_latest` that stops trying further sources once
the budget is spent — deliberately not v1 scope.

#### Key Decisions

The load-bearing decisions are recorded in the decision register (§4 "Decision register");
the two that shape the architecture:

| Decision | Summary |
|----------|---------|
| **O-1 — Composite adapter, no merge** | The ledger resolves exactly one `RateProviderV1` (a scoped `ClientHub` lookup as implemented — see §1.3), so per-provider registrations do not work today. This gear registers ONE composite (`DiscoveringRateProvider` building a `CompositeRateProvider`) that returns the first whole successful document — a snapshot period stays single-source-coherent for audit. Source provenance rides on each `ProviderRate` (§3.1), so the ledger stamps the true upstream per row rather than one id per pass (§3.2). |
| **O-3 — Ledger owns triangulation** | The adapter emits **only the source's native direct pairs** (ECB's EUR pairs) — no cross-rate synthesis here. Cross-base rates (X→EUR→Y) are computed ledger-side in `RateSource`; enabling the ledger's deferred triangulation is a hard companion dependency (§4). |

### 1.3 Architecture Layers

Four crates, not one — each source plugin self-registers; the core gear discovers and
composes them:

```text
Source-plugin gears   Each source is its OWN gear crate. Its init() builds the shared HTTP
(bss-rate-provider-    client, registers a PluginV1<RateProviderSourcePluginSpecV1>
  ecb-plugin,          instance in types-registry (vendor + priority in the instance JSON),
  -http-json-plugin)   and register_scoped::<dyn RateProviderV1>(gts_id) in ClientHub.
       │
       ▼
Core gear init()      Registers ITSELF as a PluginV1<RateProviderPluginSpecV1> instance +
(bss-rate-provider)    scoped RateProviderV1 (the one the ledger discovers). Does NOT
                       discover sources yet — that is deferred (see below).
       │
       ▼
DiscoveringRateProvider   The registered instance. On EVERY fetch_latest/health call: lists
(re-discovers every       types-registry instances of RateProviderSourcePluginSpecV1,
 tick, self-healing)      keeps the ones whose vendor == source_vendor, sorts by priority,
                          resolves each via get_scoped, and builds a fresh
                          CompositeRateProvider — never cached across ticks, so a source
                          plugin's registration, removal, or priority change takes effect
                          on the very next tick, no restart needed. A failed discovery
                          (zero matches) errors the tick — no composite is retained, and
                          no source identity is carried over (provenance is per-rate).
       │
       ▼
CompositeRateProvider   impl RateProviderV1 · ordered sources · first whole successful
(selection)             document, returned unchanged — source-agnostic, stateless
       │
       ▼
Sources            EcbRateProvider (bss-rate-provider-ecb-plugin: XML fetch/parse) ·
(one per plugin)   HttpJsonRateProvider (bss-rate-provider-http-json-plugin: generic
                   GET-JSON + field mapping)
       │
       ▼
HTTP client        toolkit-http (hyper + rustls), outbound HTTPS only, built once per
                   source plugin via the shared bss-rate-provider-sdk::http_client helper
                   → ECB eurofxref-daily.xml (primary) · bank/PSP feed (fallback, post-v1 O-10)
```

Shared source utilities (exact-decimal conversion, HTTP-error mapping, fetch metrics, the
shared HTTP client builder, the `PluginV1` registration helper) live in
`bss-rate-provider-sdk`, used by every source plugin and the core gear alike.

The ledger-side `RateSyncJob` (outside this boundary) resolves the registered composite
instance the same way this gear resolves its own sources — a types-registry lookup +
scoped `ClientHub` get, matched by the ledger's `fx.provider_vendor` — then calls
`fetch_latest(ctx, &[], request_id)` once per tick and persists each returned rate under
the `provider` that rate carries. The composite's own `provider_id()` is its constant
configured identity, used for log attribution only, never as a row stamp. (The exact
ledger-side resolution code is out of this gear's boundary and not re-verified here — see
the ledger's own design docs.)

## 2. Principles & Constraints

### 2.1 Design Principles

#### Fetch-only adapter

- [ ] `p1` - **ID**: `cpt-cf-bss-rate-provider-principle-fetch-only`

The gear fetches rates and nothing else: no persistence, no translation, no triangulation,
no staleness marking, no snapshotting — those are ledger-owned (§1.1). `fetch_latest` MUST
be side-effect-free and safe to call repeatedly (the ledger job is idempotent).

#### Config-driven source assembly

- [ ] `p1` - **ID**: `cpt-cf-bss-rate-provider-principle-config-driven-assembly`

The active sources and their fallback order are config — the composite is assembled lazily
by discovery, never hardcoded. Each source is its own deployed plugin gear with a `vendor` +
`priority` in its config; add a source by deploying/configuring its plugin gear, remove one
by un-configuring it, reorder the fallback chain by changing `priority` values. A new
provider *family* costs one new plugin crate implementing `RateProviderV1`; a new *simple
REST feed* costs zero code — configure `bss-rate-provider-http-json-plugin` with a
`mapping`.

#### One source per document — provenance, not coverage

- [ ] `p1` - **ID**: `cpt-cf-bss-rate-provider-principle-single-source-provenance`

**Normative rule, stated once so the two halves are not read as competing:** a served
document comes from exactly **one** source, taken **whole**, never stitched together across
sources — and **coverage is not a success criterion**. Those are one rule about *provenance*,
not two rules in tension. Concretely:

| A source returns | Result | Fallback to the next source? |
|---|---|---|
| Some pairs, some entries skipped as invalid | `Ok` with the survivors | **No** — partial coverage is a success |
| Fewer pairs than the ledger would like | `Ok` with what it publishes | **No** |
| Nothing usable (every entry failed) | `Err(Internal)` | Yes |
| Requested pairs matched nothing it publishes | `Err(PairUnavailable)` | Yes |
| Transport / status / whole-document parse failure | `Err` | Yes |

So fallback triggers only when a source returns `Err` for the **whole fetch** — never per
missing pair, never a cross-source merge. A pair absent from the chosen source's document is
simply absent (the ledger treats it as no rate). A snapshot period stays
single-source-coherent for audit (O-1).

Read the older phrasings — "first whole document", "all-or-nothing per source" — strictly as
this provenance rule. Neither is a completeness rule. A source that publishes 20 of the 30
pairs the ledger would like returns those 20 and succeeds; a single entry that fails to
convert is skipped and the rest of the document still serves (§3.2, both sources). **Per-pair
fallback does not exist in v1** and is explicitly deferred (O-1 variant (b), O-13).

**Why a coverage gate was rejected, not merely left out.** The two shapes such a gate could
take — "all requested pairs" or "an explicit minimum expected set" — each fail on their own:

- *All requested pairs* has nothing to bind to on the real call path. The ledger's
  `RateSyncJob` calls `fetch_latest(&[])` — the whole published table, no requested pairs — so
  the gate would be vacuous exactly where the rates actually come from. Where pairs *are*
  passed, ECB publishes EUR-based pairs only, and a pair it does not publish is omitted by
  design ("Direct pairs only" below, O-3); requiring completeness would fail the document on
  every non-EUR-base pair the ledger asks about.
- *A minimum expected set* would have to be configured per source and kept in step with what
  each feed publishes — an operator-maintained duplicate of the feed's own contents, wrong the
  first time a currency is added or delisted upstream, and wrong in the direction that blocks
  a usable document.

And both would make the failure mode worse rather than better. The ledger's `RateSyncJob`
turns ANY error from a configured provider into `FX_SNAPSHOT_MISSING` and refreshes
**nothing**, so one delisted currency or one malformed value would stall the FX fan-out for
every other currency in an otherwise-usable feed — trading a few stale pairs for all of them.
Falling through to a lower-priority source does not repair the gap either: the composite
serves that source's document *whole*, so a feed with narrower coverage can leave the ledger
with strictly fewer refreshed pairs than the primary it replaced.

Partial coverage is not silent either. The skipped entries are logged individually with a
reason plus an aggregate count, and a pair that stops being refreshed ages out under the
ledger's own staleness window into `FX_RATE_UNAVAILABLE` — so the platform still surfaces it,
through the layer that owns the age policy. The one case that *is* an error is a document
where **nothing** usable survives: all entries failed conversion (`Internal`), or the
requested pairs matched nothing this source publishes
([`RateProviderError::PairUnavailable`]) — both `Err`, so the composite moves on.

#### Direct pairs only

- [ ] `p1` - **ID**: `cpt-cf-bss-rate-provider-principle-direct-pairs-only`

A source emits only its natively published pairs, never synthesized — cross-base derivation
is the ledger's triangulation concern (O-3). A requested pair the source cannot serve is
**omitted** from the returned document; if that leaves the document empty, the source
returns [`RateProviderError::PairUnavailable`] naming the first requested pair.

This is one contract for both source plugins. `PairUnavailable` rather than `Internal`
because it is a routine "not served here", not a fault in this gear — the distinction is
visible on `fx_provider_fetch_errors_total{kind}`, where these must not be counted as
`internal` and hide real defects. An `Err` rather than `Ok([])` because an empty document
reads as a complete answer and would stop the fallback chain on the first source, when the
next one may publish exactly that pair.

Whole-table calls (empty `pairs`, which is what the ledger's sync job makes) are unaffected:
with no requested pair there is nothing to report unavailable — `PairUnavailable` simply
never applies. `Ok([])` is still unreachable on that path: a document with no usable
entries fails as `Internal` (ECB rejects a zero-entry table at parse time, http-json
errors when nothing maps), so no source can pass an empty table off as a complete answer.

#### Deterministic conversion

- [ ] `p1` - **ID**: `cpt-cf-bss-rate-provider-principle-deterministic-conversion`

`rate → rate_micro` conversion parses the published decimal into an **exact decimal
representation** (never binary floating point) and rounds with banker's rounding
(half-to-even), matching the platform ledger rounding default, so a re-fetch of the same
published rate yields the same integer (§3.2, O-4).

#### Provider time, not fetch time

- [ ] `p1` - **ID**: `cpt-cf-bss-rate-provider-principle-provider-as-of`

`as_of` MUST be the provider's publication timestamp normalized to UTC — never `now()`.
On non-publication days (weekends / TARGET holidays) the last published rate is returned
with its original `as_of`, so the ledger's staleness rule still applies.

**Bounded in the future direction, unbounded in the past.** The ledger's staleness rule is
`now - as_of` against a window, which only constrains the past. A document dated ahead of us
has a *negative* age and reads as permanently fresh, so a feed publishing a month-ahead
timestamp would keep the ledger locking a frozen rate with no staleness alarm ever firing.
Each source therefore rejects a document whose `as_of` is more than
`MAX_FUTURE_SKEW_HOURS` = 26 h ahead of the gear's own clock
(`bss_rate_provider_sdk::publication_time`), at parse time, before any rate is built:

- **Owned by the source plugin, not the ledger.** Once a rate is a stored row the ledger
  cannot tell "the feed published a bad timestamp" from "we have not synced in a while";
  only the plugin sees the value arrive off the wire on this tick.
- **Fail-safe.** The rejection fails the whole document, so the composite falls through to
  the next source rather than storing the bad value — the same shape as any other parse
  rejection, and it counts on `fx_provider_fetch_errors_total{kind="internal"}`.
- **26 h = 14 h + 12 h.** 14 h is the widest civil timezone offset (UTC+14), so a date-only
  feed like ECB — whose date its plugin anchors at 00:00:00 UTC — is never rejected for
  being dated in its own timezone; 12 h is a generous host-clock-drift allowance, so a local
  NTP failure degrades into wrong-but-served instead of a feed-wide outage.
- **Nothing is rejected for being old.** Age is the ledger's policy; re-checking it here
  would put one rule in two layers and let them disagree.

### 2.2 Constraints

#### Fixed SDK contract

- [ ] `p1` - **ID**: `cpt-cf-bss-rate-provider-constraint-fixed-sdk-contract`

The `RateProviderV1` trait, `ProviderRate`, `CurrencyPair`, and `RateProviderError` are
**already defined** in `bss-ledger-sdk` and MUST NOT be changed without a ledger-side
change (GTS `gts.cf.bss.ledger.rate-provider.v1`).

#### Never on the posting path

- [ ] `p1` - **ID**: `cpt-cf-bss-rate-provider-constraint-off-posting-path`

`fetch_latest` is called only by the background `RateSyncJob`. A provider outage fails the
job (ledger alarms), never a post. The adapter MUST NOT retry indefinitely; one bounded
attempt per call — the ledger job schedules the next tick.

**Why no same-source retry before falling back.** Any `Err` from a source advances the
chain; there is no retriable/non-retriable classification in v1, and that is deliberate
rather than unfinished:

- **A second attempt buys little here.** The chain already provides the retry, against a
  *different* upstream. Source diversity beats attempt count for the failures that actually
  happen — a feed being down, rate-limiting, or a bad deploy on the publisher's side are all
  things an immediate retry to the same host does not fix.
- **The background rescheduling is the real retry.** The next tick re-runs the whole chain
  within `rate_sync_tick` (1 h default), far inside the 24 h G10 staleness window, so a
  transient failure costs a refresh, not a posting outage.
- **It keeps the latency budget honest.** Worst case stays the sum of the per-source
  timeouts (see the fetch-budget rule in §2); adding R retries would multiply it by R and
  make the budget rule much easier to breach by accident.

Classification may be worth adding later, and the case for it is narrow: `UpstreamStatus`
with a `429`/`503` plus a `Retry-After` inside the tick budget is the one signal that says
"the same source will work shortly". `Unreachable` (timeout, DNS, TLS) should keep falling
straight through — those are precisely when another source is the faster answer.
`PairUnavailable`, `InvalidPair`, and `Internal` are never retriable: retrying cannot change
what a feed publishes or fix a local fault.

#### Stateless

- [ ] `p1` - **ID**: `cpt-cf-bss-rate-provider-constraint-stateless`

The adapter holds no DB, no per-tenant state, and — since provenance moved onto each
`ProviderRate` — no in-memory state at all: the composite is rebuilt per call and every
source's fields are set once at `init()`. A provider publishes **global** rates and the
ledger fans them out per tenant.

#### Fail-safe by absence

- [ ] `p1` - **ID**: `cpt-cf-bss-rate-provider-constraint-fail-safe-by-absence`

If the adapter is not registered, the ledger uses `UnconfiguredRateProviderV1` → the local
store stays empty → FX posts block (`FX_RATE_UNAVAILABLE`), never a silent wrong rate.

#### Rate precision

- [ ] `p1` - **ID**: `cpt-cf-bss-rate-provider-constraint-rate-micro-precision`

`ProviderRate.rate_micro` is the functional-per-unit-transaction multiplier × 1e6, `i64`
(O-5: kept for v1; revisit for high-unit / crypto pairs — any change is an SDK change).
Overflow / non-finite values MUST map to `RateProviderError::Internal`, never a silent
truncation.

**`rate_micro` MUST be strictly positive** (`> 0`) — a rate that rounds to zero or below is
rejected at conversion, not stored. Confirmed with the BSS billing owner (2026-07-28): the
domain has no negative or zero FX rates, so accepting one would only ever mean corrupt feed
data, and it would zero out or flip the sign of every translation derived from it. The
ledger's `RateSyncJob` independently drops non-positive quotes before upsert, so this is the
outer of two gates rather than the only one.

#### Secrets handling

- [ ] `p1` - **ID**: `cpt-cf-bss-rate-provider-constraint-secrets`

Any provider API key MUST come from config `${VAR}` expansion or the platform CredStore —
never hardcoded, never logged, never in this document. Implemented as `secrecy::SecretString`
carried by the `auth` enum variant that needs it (`bss-rate-provider-http-json-plugin`'s
`Auth::Bearer { api_key } | Auth::HeaderKey { api_key }` — the same `SecretString` pattern
`authn-resolver`'s plugin configs use) so an accidental `Debug`/`tracing` dump of the
config struct is redacted rather than printing the key — only `expose_secret()` reveals it,
at the one call site that builds the outbound auth header. Binding the credential to the
variant also removes the "configured to authenticate but no key" and "key set but never
sent" states entirely (§3.2). ECB has no credential (public feed).

**How `${VAR}` is realized.** The http-json plugin loads its config through
`ctx.config_expanded()`, and `Auth` implements `ExpandVars` by hand (the derive covers only
structs with named fields, and the credential deliberately lives inside an enum variant), so
`api_key: "${BANK_X_KEY}"` is resolved from the environment at `init()`. An unresolvable
variable **fails `init()`** rather than letting the source start up and send the literal
placeholder as its credential. This is what makes the gap below survivable in practice: the
raw key never has to be written into the config file that the dump serializes.

**CredStore is not wired.** The platform CredStore is named above as an allowed source of the
credential, but this gear has no CredStore integration — `${VAR}` expansion is the only
implemented path today. Adding one is a separate change, and not a prerequisite: `${VAR}`
already keeps the key out of the config file.

**Gap — raw effective-config dumps are not covered.** `SecretString` redacts the *typed*
config once deserialized into Rust. It does not reach `toolkit`'s own effective-config dump
(`toolkit::bootstrap::config::dump`), which serializes the **pre-deserialization** JSON, so a
routine diagnostic can print an `api_key` verbatim.

- **Normative:** raw configuration output MUST redact credential-shaped fields (or refuse to
  emit sections containing them). A gear cannot satisfy this from its own side — by the time
  the value reaches a gear's typed config the dump has already run — so it MUST be enforced
  inside the dump.
- **Owner:** the platform / `toolkit` maintainers, not this gear. Recorded here because this
  gear is the one that supplies it a credential to leak; tracked as a risk in PRD §12.
- **Scope today:** not exploitable in v1 as shipped. The only configured source is ECB, a
  public feed with no credential, so there is nothing in this gear's config for a dump to
  disclose. The documented `${VAR}` path (above) keeps the raw key out of the file the dump
  serializes, but it is a convention, not an enforced guarantee — so dump-side redaction is
  a **p1 prerequisite for enabling credential-bearing feeds** (tracked as a p1 dependency
  in PRD §10 and as the §12 risk): no authenticated http-json feed is onboarded until the
  dump redacts credential-shaped fields.

## 3. Technical Architecture

### 3.1 Domain Model

The domain types are **inherited from `bss-ledger-sdk`** (`rate_provider.rs`) and NOT
redefined here (constraint `cpt-cf-bss-rate-provider-constraint-fixed-sdk-contract`).

#### Type: `CurrencyPair`

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `base` | string (ISO 4217) | Yes | Transaction currency |
| `quote` | string (ISO 4217) | Yes | Functional currency |

#### Type: `ProviderRate`

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `base` | string | Yes | Base (transaction) currency |
| `quote` | string | Yes | Quote (functional) currency |
| `rate_micro` | int64 | Yes | Functional-per-unit-base × 1e6 (fixed precision) |
| `as_of` | timestamp (UTC) | Yes | Provider publication time; drives ledger staleness |
| `provider` | string | Yes | The concrete upstream that published THIS rate, stamped by the serving source; what the ledger stores as `ledger_fx_rate.provider` (see the provenance rule in §3.2) |

#### Enum: `RateProviderError`

| Value | Description |
|-------|-------------|
| `PairUnavailable { base, quote }` | Provider does not publish this pair |
| `Unreachable(msg)` | Network / DNS / timeout |
| `UpstreamStatus(u16)` | Non-success HTTP status |
| `InvalidPair(msg)` | Malformed / unknown currency code |
| `Internal(msg)` | Parse / conversion fault |

### 3.2 Component Model

Each component carries a stable `cpt-cf-bss-rate-provider-component-{slug}` ID.

#### Plugin discovery & per-plugin configuration

- [ ] `p1` - **ID**: `cpt-cf-bss-rate-provider-component-source-factory`

There is no `build_source` factory and no `kind` field — each source is its own gear crate
implementing `RateProviderV1` directly, and "assembly" is types-registry discovery, not a
config-driven factory loop:

- Each source-plugin gear's `init()` registers a `PluginV1<RateProviderSourcePluginSpecV1>`
  instance (carrying its own `vendor` + `priority` in the instance JSON) and a scoped
  `RateProviderV1` client keyed by that instance's GTS id
  (`bss_rate_provider_sdk::registration::register_rate_provider_plugin`, shared by every
  plugin gear including the core gear itself).
- The core gear's `DiscoveringRateProvider` (built at `init()`, discovery deferred to the
  first `fetch_latest`) lists all `RateProviderSourcePluginSpecV1` instances, **keeps only
  the ones whose `vendor` matches its own configured `source_vendor`**, sorts the survivors
  by `priority` (lower = tried first; a tie is broken by the plugin's GTS **instance id**,
  and logged as a warning, never a startup failure), and resolves each via `get_scoped`. A
  vendor match whose scoped client isn't registered is logged and excluded, not fatal.
  The tiebreak is deliberate: registry list order is not a stable contract, so ordering on
  it would let the serving source — and with it the provenance written onto financial rows
  — change between ticks with no configuration change. Sharing a `priority` is still
  flagged, because the id then decides an order nobody chose for that purpose.
- **Empty result is a *runtime* error, not a startup failure.** If zero source plugins match
  the configured vendor, `discover()` returns `RateProviderError::Unreachable` from that
  *tick's* `fetch_latest` — it is **not** checked or rejected at any `init()`, because
  discovery itself doesn't run until the first fetch (deliberately, so plugin registration
  order across gears never matters — see O-7 revised). This is a real behavior change from
  the original (pre-plugin-pattern) design, which required a startup-time empty-list check
  — there is no equivalent of "unknown `kind`" or "empty `sources[]`" to validate anymore,
  since there is no `kind` and no list.
- **Discovery re-runs on every tick, not just the first.** Neither a successful nor a
  failed discovery is cached across calls — every `fetch_latest`/`health` re-lists
  instances and rebuilds the composite from scratch. A source plugin that registers,
  deregisters, or changes its `priority` between ticks takes effect on the very next tick,
  never requiring a gear restart (§3.8 "Deployment Topology"). A *failed* discovery (zero
  matches) is **not** papered over with a retained composite: `fetch_latest` returns the
  no-sources error for that tick (§3.2), the ledger job alarms, and the next tick re-runs
  discovery from scratch. Nothing about source identity survives a call — `provider_id()` is
  the core gear's own constant configured id, and the serving source is carried per rate
  (O-7a), so there is no "last real upstream" state to keep.
  This trades one `list_instances` call per tick (hourly, per the PRD's default
  `fx.rate_sync_tick_secs`) for always-current membership — negligible overhead for a
  background job that never runs on the posting path.
- There is likewise no `fx.provider_order` list to align — cross-gear alignment between the
  core gear, its source plugins, and the ledger is a shared **`vendor` string** match (O-12
  revised), not an ordered-list comparison.

**Module config — one block per gear, not one list:**

```yaml
gears:
  bss-rate-provider:                    # the core/composite gear
    config:
      vendor: "cf.bss"                  # what THIS composite advertises to the ledger
      priority: 100
      source_vendor: "cf.bss"           # which source plugins this gear composes
      id: "bss-rate-provider"           # provider_id() — constant, never a served source

  bss-rate-provider-ecb-plugin:
    config:
      id: "ecb"                         # stable provider_id stamped on synced rows
      vendor: "cf.bss"                  # MUST match the core gear's source_vendor
      priority: 100                     # lower = tried first
      base_url: "https://www.ecb.europa.eu/stats/eurofxref/eurofxref-daily.xml"
      timeout_ms: 5000

  bss-rate-provider-http-json-plugin:   # ILLUSTRATIVE fallback — not part of v1 (O-10: v1 ships ECB-only)
    config:
      id: "bank-x"
      vendor: "cf.bss"
      priority: 200
      base_url: "${BANK_X_URL}"
      timeout_ms: 5000
      # The credential lives INSIDE `auth`, on the variant that needs it. Omit
      # the whole `auth:` block for a public feed. `kind: none` takes no
      # api_key, and bearer/header-key REQUIRE one — both enforced by serde at
      # config load, not by a runtime check.
      auth:
        kind: bearer                   # none | bearer | header-key
        api_key: "${BANK_X_KEY}"       # SecretString — never logged
      mapping:
        base: "USD"                    # v1: literal base only, no JSON-path base
        rates: "rates"                 # dotted path, not a JSON-path/JSONPath expression
        rate: "value"
        as_of: "date"
```

**Config fields, by gear (no shared `SourceConfig` type — each plugin's config is its own
struct; the `id`/`vendor`/`priority` shape is a convention every plugin repeats, not a
common base type):**

| Gear | Field | Type | Required | Description |
|------|-------|------|----------|-------------|
| all three | `id` | string | Yes | Core gear: the composite's own constant `provider_id()` (log/alarm attribution). Plugins: the stable per-rate `provider` stamped on every rate they serve. |
| all three | `vendor` (core), `source_vendor` (core, source-selection), `vendor` (plugins) | string | Yes | The matching key across core ↔ plugins ↔ ledger `fx.provider_vendor`. |
| all three | `priority` | i16 | Yes | Fallback order (lower tried first); duplicates across plugins are logged, not rejected. |
| plugins only | `base_url` | string | Yes | Source endpoint; must parse as a URL with the `https` scheme (case-insensitive) and a host — checked at that plugin's `init()`. |
| plugins only | `timeout_ms` | u64 | No (5000) | Outbound per-attempt HTTP timeout. |
| http-json only | `auth` | tagged enum: `{kind: none}` \| `{kind: bearer, api_key}` \| `{kind: header-key, api_key}` | No (`none`) | How the feed is authenticated **and** the credential it needs, in one value. `api_key` is a `SecretString` (`${VAR}` / CredStore expansion upstream, redacted from `Debug`). A kind that needs a key without one, or a key attached to `none`, is a **config-load error** — the invalid combinations are unrepresentable rather than runtime-checked. |
| http-json only | `mapping` | struct, optional | **Yes for http-json** — its `init()` fails loud if absent | `base` (literal only, must be ISO-4217-shaped), `rates`/`rate`/`as_of` (dotted paths). |

**Adding a provider:**

- *Simple REST feed* → deploy/configure `bss-rate-provider-http-json-plugin` with a
  `mapping` and a `vendor` matching the core gear's `source_vendor`. **No code.**
- *New family (quirky format/auth)* → implement `RateProviderV1` in a new plugin crate,
  register it the same way (`register_rate_provider_plugin`), give it a matching `vendor`.

#### `CompositeRateProvider`

- [ ] `p1` - **ID**: `cpt-cf-bss-rate-provider-component-composite`

**Not itself the registered `ClientHub` instance** — `DiscoveringRateProvider` (see §1.3) is
what the core gear registers; it rebuilds a fresh `CompositeRateProvider` on every
`fetch_latest`/`health` call, never reusing one across ticks. `CompositeRateProvider` wraps
the **ordered** `Vec<Arc<dyn RateProviderV1>>` that discovery produced and does the fallback
the ledger cannot. Source-agnostic — it never names a concrete source. Configuration: none of
its own beyond the composite's `id` — the try order **is** the priority order that tick's
`discover()` established; a `priority` config change on a source plugin takes effect on the
*very next tick*, no restart needed.

**State:** none. The composite is a stateless value rebuilt per call — no `last_served`
index, no interior mutability, no persistence.

| Operation | Input | Output | Key Behavior |
|-----------|-------|--------|--------------|
| `fetch_latest` | `ctx`, `pairs`, `request_id` | `Vec<ProviderRate>` | Try sources **in order**; return the **first** that yields `Ok(document)` **whole** (no merge), unchanged — each rate already carries its own `provider`. If **all** sources fail, return the last `RateProviderError` (the ledger job then raises `FX_SNAPSHOT_MISSING`). |
| `provider_id` | — | `&str` | The composite's own **constant** id (the core gear's configured `id`), for log/alarm attribution. NOT the last-served source — see the provenance rule below. |
| `health` | `ctx`, `request_id` | `()` | `Ok(())` if **any** source is reachable (ordered probe, short-circuits on the first success). `Err` only when every source's probe failed, and then it is the last source's error — the same ordering caveat as `fetch_latest`. |

**Behavioral rules:**

- **Provenance is per-rate, never read back through `provider_id()`.** Each source stamps
  its own id onto every [`ProviderRate`] it returns (§3.1), so the serving upstream travels
  with the data and the ledger records it per row. `provider_id()` MUST NOT be used to
  answer "who served the batch I just fetched?": it carries no information about which
  `fetch_latest` it relates to, and the composite is registered process-wide in `ClientHub`,
  so a second consumer fetching in between would make the ledger stamp the wrong source onto
  financial records. The ledger's `RateSyncJob` therefore does not depend on call ordering
  for correctness (O-7a).
- **Empty-list safety.** The constructor only `debug_assert!`s non-emptiness (a defensive
  invariant check that compiles out in release). `provider_id()` reads no element of the
  source list at all, so it cannot index out of bounds; `fetch_latest`/`health` return an
  "empty composite" `Internal` error. Both are covered by unit tests that construct the
  struct directly, bypassing the debug-only guard.

#### `EcbRateProvider` (source plugin `bss-rate-provider-ecb-plugin`)

- [ ] `p1` - **ID**: `cpt-cf-bss-rate-provider-component-ecb-source`

HTTP fetch + XML parse + `rate_micro` conversion + error mapping over the ECB daily feed.
Dependencies: its own `toolkit_http::HttpClient` (built once in this plugin's `init()` via
the shared `bss_rate_provider_sdk::http_client::build_source_http_client` helper — not a
process-wide shared client), its own `EcbPluginConfig` (`id` default `"ecb"`, `vendor`,
`priority`, `base_url` default = the ECB daily feed, `timeout_ms` default `5000`).

**No `format` field exists.** v1 ships **XML-only** (`parse_ecb_xml`) — there is no `format`
config knob and no SDMX parser in the implementation; O-2's "SDMX optional" / "Frankfurter
allowed for dev" were never built. Add a `format` field only if a non-XML feed is actually
needed.

| Operation | Input | Output | Key Behavior |
|-----------|-------|--------|--------------|
| `provider_id` | — | `&str` | Returns the configured stable id. |
| `fetch_latest` | `ctx`, `pairs: &[CurrencyPair]`, `request_id` | `Vec<ProviderRate>` | GET latest feed → parse → convert, stamping each rate with this source's `id`. `pairs = &[]` ⇒ return the **whole** published table. Requested pairs the source cannot serve are **omitted** (not an error). Map transport failures to `Unreachable` / `UpstreamStatus`. |
| `health` | `ctx`, `request_id` | `()` | A cheap `HEAD` against the same feed URL — never re-parses the published table just to check reachability. **Any HTTP response is `Ok(())`**, including a non-2xx: the status is logged, not returned as a failure, since a `503` proves the request arrived. Only a transport failure (DNS / connect / TLS / timeout) is `Err`. |

**ECB payload handling:**

- **Direct pairs published by ECB are EUR-based** (EUR→X). A `CurrencyPair` whose
  `base`/`quote` is not directly published is **omitted** — never synthesized (O-3). In
  particular the **inverse leg X→EUR is NOT emitted here** (e.g. `USD→EUR` for a USD
  transaction under an EUR functional currency): deriving it by **deterministic inversion**
  of the stored EUR→X rate is part of the ledger's triangulation (O-3; §4 "Companion
  ledger change").
- **Currency codes are gated on the ISO-4217 shape.** The `currency` XML attribute arrives
  verbatim from the remote feed and becomes a tenant-store column value, so a code that
  isn't three ASCII letters is dropped and logged; survivors are uppercased, and dedup keys
  off the normalized code (so a feed publishing both `USD` and `usd` is caught as the
  duplicate it is). Dropping every entry surfaces the documented empty-table `Internal`
  error rather than an empty `Ok`.
- **Two distinct publication dates fail the whole document.** The daily feed is, by
  contract, one day's rates; a second `<Cube time=…>` with a different date means this is not
  that file — overwhelmingly the likely cause is a `base_url` pointing at ECB's 90-day
  history feed. There is no non-arbitrary way to choose between the dates, and picking one by
  document order would silently serve one day's slice of some other file under a date that
  depends on parse order. The rejection is an `Internal` naming both dates, so the composite
  falls through to the next source (fail-safe) and the operator sees the collision. A repeat
  of the **same** date is not a conflict and is accepted.
- **A currency row is bound to its own `<Cube time=…>` block.** Rows under a `time` that
  failed to parse, or with no enclosing date at all, are omitted rather than folded into the
  document's table under an `as_of` that isn't theirs. An unparseable `time` is logged with
  its raw value, so the eventual "missing publication date" error isn't the only clue. A
  `Cube` carrying only one of `currency`/`rate` (a truncated feed row) is likewise logged and
  ignored — no anomaly in this parser is dropped silently.
- **An unconvertible rate is skipped per entry, not fatal for the document** — matching the
  http-json source. This is the normative semantic, not an exception to §2.1's "one source per
  document": that rule governs *provenance*, and coverage is deliberately not a success
  criterion (§2.1, O-13). This matters beyond consistency: the ledger's `RateSyncJob` turns ANY
  failure from a *configured* provider into `FX_SNAPSHOT_MISSING` and refreshes nothing, so
  failing the batch over one malformed value would stall the FX fan-out for every other
  currency in an otherwise-usable feed. Each skip is logged with its quote and reason, plus
  an aggregate count. The document is reported as `Internal` only when entries were
  attempted and **all** of them failed; an empty result because a non-empty requested-`pairs`
  filter matched nothing is `PairUnavailable` (the shared "not served here" contract under
  §2.1 "Direct pairs only"), never `Ok(vec![])`.
- **Non-publication days** (weekends / TARGET holidays): return the last published rate
  with its original `as_of` (staleness is the ledger's call).
- **Cadence assumption:** ECB publishes once per TARGET business day ~16:00 CET; on
  non-publication days the last published rate is returned (its `as_of` unchanged, so the
  ledger's staleness rule still applies).

#### `HttpJsonRateProvider` (source plugin `bss-rate-provider-http-json-plugin`)

- [ ] `p2` - **ID**: `cpt-cf-bss-rate-provider-component-http-json-source`

A configurable GET-JSON source so a plain REST rate feed is onboarded by **config alone**.
Covers the common "fetch a JSON document of rates, map fields" shape; NOT for quirky
sources (ECB XML above, or a PSP settlement feed with signed auth — those get their own
plugin crate). Dependencies: its own `toolkit_http::HttpClient` (same shared builder helper
as ECB); its own `HttpJsonPluginConfig` incl. `mapping` + `auth` (the `api_key` lives
*inside* the credential-bearing `auth` kinds, never as a top-level field — see the
configuration table below).

**Configuration (`HttpJsonPluginConfig`, in addition to `id`/`vendor`/`priority`/
`base_url`/`timeout_ms`):**

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `mapping.base` | string, **literal only** | — | Base currency literal (e.g. `"USD"`) — v1 has no path-based/multi-base support, only a fixed literal. |
| `mapping.rates` | dotted path (`a.b.c`, object traversal only — not JSONPath) | — | The object of quote→entry pairs. |
| `mapping.rate` | field name within each entry | — | The numeric/string rate field. |
| `mapping.as_of` | dotted path | — | Publication timestamp (RFC 3339); parsed to UTC. One document-level timestamp is applied to every returned rate — there is no per-entry `as_of`. |
| `auth` | tagged enum (see §3.2's config table) | `{kind: none}` | How the feed is authenticated, carrying its credential: `Authorization: Bearer …` or a fixed `X-API-Key` header. |

| Operation | Input | Output | Key Behavior |
|-----------|-------|--------|--------------|
| `fetch_latest` | `ctx`, `pairs`, `request_id` | `Vec<ProviderRate>` | GET `base_url` (with `auth`) → parse JSON → apply `mapping` → convert each to `ProviderRate` stamped with this source's `id`. `pairs = &[]` ⇒ whole document; a non-empty `pairs` filters **case-insensitively** (matching the ECB source). An entry that fails mapping is skipped (logged with its quote + reason, plus an aggregate count), never fabricated; a document where **zero** entries map ⇒ `RateProviderError::Internal` (behavioral rules below). |
| `provider_id` | — | `&str` | The configured `id`. |
| `health` | `ctx`, `request_id` | `()` | A `HEAD` against the configured `base_url`, carrying the same credential the fetch uses, with the identical rule as ECB: any HTTP response is `Ok(())`, only a transport failure is `Err`. That rule is what makes a HEAD usable here at all — an arbitrary JSON API commonly answers `405` to HEAD while serving `GET`, and `401`/`403` on a rotated key; all three are the host answering. Records **no** fetch metrics: touching them would advance the last-success gauge and mask the freshness alerts. |

**Behavioral rules:**

- **Base-currency shape.** Many free feeds are single-base. Config states the base; a
  requested pair whose base ≠ the feed base is **omitted**, never synthesized here (O-3).
  If filtering leaves nothing at all, that is `RateProviderError::PairUnavailable` — the
  same answer the ECB source gives — not `Ok([])`, so the composite falls back instead of
  recording a served-but-empty tick.
- **Pair matching is case-insensitive**, the same as the ECB source. Both sit behind one
  composite, so a caller passing `"usd"` must not get rates from one source and an empty
  result from the other purely because of casing.
- **Currency codes are gated on the ISO-4217 shape** (three ASCII letters, uppercased)
  before they can reach a tenant's store: a malformed `quote` from the feed is skipped like
  any other unmappable entry, while a malformed configured `mapping.base` fails the whole
  document — it is operator config, not feed data, and would taint every rate.
- **Deterministic mapping.** An unresolvable field ⇒ skip that entry with a logged reason
  (a success for the document — coverage is not a success criterion, §2.1 / O-13);
  a wholesale parse failure ⇒ `RateProviderError::Internal`. A syntactically
  valid document from which **zero entries map** MUST also return
  `RateProviderError::Internal` — returning `Ok([])` would read as success, suppress the
  composite fallback, and let the ledger mark the sync pass successful without refreshing
  a single rate.
- **Scope (O-11):** v1 = single-base JSON feeds, simple field paths,
  `none` / `bearer` / `header-key` auth; richer transforms (multi-base, JSON-path dialects,
  custom date/number formats) deferred.

#### `rate → rate_micro` conversion

- [ ] `p1` - **ID**: `cpt-cf-bss-rate-provider-component-rate-micro-conversion`

ECB quotes ~5 significant digits. Convert `rate` (decimal) to
`rate_micro = round(rate × 1_000_000)` using **banker's rounding (half-to-even)** to match
the platform ledger rounding default, so a re-fetch of the same published rate yields the
same integer (O-4: accepted — half-to-even is inherited from the ledger's platform default
`cpt-cf-bss-ledger-fr-money-rounding-scale`, not chosen here; any revision would come from
that requirement changing, not from this gear).
The published decimal string MUST be parsed into an **exact decimal representation** —
**never a binary `f64`**, whose nearest-representable value can mis-round exact half-way
decimals under half-to-even. Implemented with `rust_decimal::Decimal`
(`Decimal::from_str_exact` → `checked_mul` by `1_000_000` → `round_dp_with_strategy(0,
MidpointNearestEven)` → `to_i64()`), not an arbitrary-precision `BigDecimal` — `Decimal`'s
fixed 96-bit mantissa is sufficient for FX-rate magnitudes and is the crate already used
elsewhere in this codebase. Overflow / non-finite / non-numeric values MUST map to
`RateProviderError::Internal` (never a silent truncation) — verified down to the exact
`i64::MAX`/`i64::MIN` boundary in the unit tests.

#### Gear wiring

- [ ] `p1` - **ID**: `cpt-cf-bss-rate-provider-component-gear-init`

**Crate layout** follows the repo's DDD-light gear structure — module wiring at the crate
root, logic in a layer directory:

```text
rate-provider/src/         config.rs · module.rs · domain/composite.rs · infra/discovery.rs
plugins/ecb-plugin/src/    config.rs · gear.rs   · infra/source.rs
plugins/http-json-.../src/ config.rs · gear.rs   · infra/source.rs
rate-provider-sdk/src/     flat — a library crate, not a gear, so no layer split
```

The split follows what each type actually depends on, not just "logic vs wiring":

- **`domain/`** — `CompositeRateProvider` only. It holds nothing but the source *ports*
  (`dyn RateProviderV1`) and the fallback policy over them, so it carries
  `#[domain_model]`, which compile-time-proves no infrastructure type leaked in (and which
  the `de0309` dylint requires of every externally-visible type in this layer).
- **`infra/`** — `DiscoveringRateProvider` and both plugins' feed adapters.
  `DiscoveringRateProvider` is deliberately NOT domain: it holds `Arc<ClientHub>` (a
  service locator) and queries the types-registry over the wire, which is infrastructure by
  intent even though those particular types aren't on the lint's deny-list.

This matches the sibling plugin gears in this repo (`static-tr-plugin`,
`static-authn-plugin`, `static-credstore-plugin` all keep a `src/domain/`).

Three separate `#[toolkit::gear(...)]`-macro gears, each with its own `init()` and its own
workspace/module registration — not one `init()` running a factory:

- **`bss-rate-provider-ecb-plugin`** / **`bss-rate-provider-http-json-plugin`**: build their
  own `toolkit_http::HttpClient`, construct their source, then call the shared
  `bss_rate_provider_sdk::registration::register_rate_provider_plugin` helper to publish a
  `PluginV1<RateProviderSourcePluginSpecV1>` instance + scoped `RateProviderV1` client.
- **`bss-rate-provider`** (core): builds a `DiscoveringRateProvider` (no HTTP client of its
  own — discovery only) and registers it via the *same* shared helper, keyed by
  `RateProviderPluginSpecV1` instead.
- All three share the `deps = [types_registry]` gear-macro edge (the types-registry gear
  must exist as a dependency for the `#[toolkit::gear]` macro's init-ordering, though the
  registration calls happen against `ClientHub` at runtime, not via that edge).

Bank / PSP settlement feed: onboard via the generic `bss-rate-provider-http-json-plugin` if
it is a plain REST feed, else a dedicated new plugin crate for signed/settlement auth
(concrete feed is O-10: v1 = ECB-only; bank/PSP added later as one more deployed plugin
gear, not a `sources[]` entry).

### 3.3 API Contracts

This gear exposes **no external REST surface** and produces **no events**. It is consumed
in-process by the ledger via the `RateProviderV1` trait resolved from `ClientHub`
(GTS `gts.cf.bss.ledger.rate-provider.v1`).

| Surface | Direction | Contract | Notes |
|---------|-----------|----------|-------|
| `RateProviderV1::fetch_latest` | inbound (from ledger) | SDK trait | One round-trip per tick; `&[]` = whole table. |
| `RateProviderV1::health` | inbound (from ledger) | SDK trait | Reachability **only** — never a freshness or readiness signal; see below. No caller in v1. |
| ECB / bank feed | outbound | HTTPS GET | External provider; see §4 "Security & AuthZ". |

**`health()` is reachability, and nothing more.** It answers "would a request to this feed's
endpoint get through right now?", not "would the next sync produce usable rates?". A source
can be `Ok` from `health()` while every real fetch fails on a malformed, empty, or
wrongly-shaped body. Reading it as a liveness signal for the FX feed is therefore a mistake,
and the contract is enforced by the trait rather than left to be inferred:

- **`Ok(())` means the endpoint answered.** Any HTTP response counts, `4xx` and `5xx`
  included: a `503` proves the request arrived, and a `405` is a normal reply from a feed that
  does not accept the probe's method. Only a *transport* failure — DNS, connect, TLS, timeout
  — is `Err`, because only then did nothing get through. Folding "is it serving correctly?"
  into this call would make `Ok(())` mean something no cheap probe can establish, and would
  report a `GET`-serving feed as down purely because it answers `405` to `HEAD`.
- **No default implementation.** The trait requires `health()` of every adapter. An earlier
  default delegated to `fetch_latest(&[])`, which gave any adapter that did not override it a
  *parsing* health check by accident — one method carrying two different guarantees depending
  on the vendor, so `Ok(())` could only be read as the weakest of them. Both sources now
  implement the same cheap `HEAD` probe under the rule above, and a future source states its
  own rather than inheriting one.
- **The probe records no fetch metrics.** It deliberately bypasses `fetch_and_parse`. Letting
  a probe touch those series would advance `fx_provider_last_success_timestamp` and the
  fetch-duration count, silencing the freshness alerts below — a probe added to reveal an
  outage would mask it instead.
- **Freshness has its own signal.** A provider that is green on `health()` while every real
  fetch fails shows up on `fx_provider_fetch_errors_total{provider}` — the errors keep
  counting while `fx_provider_fetch_duration_seconds_count` does not move — and, once the
  data itself ages out, on the `FxFeedStale` alert over
  `fx_provider_last_success_timestamp{provider}` (§4 "Feature metrics"). Both are
  source-independent: neither can be satisfied by a `HEAD`.
- **Nothing consumes `health()` in v1.** The ledger's `RateSyncJob` calls only
  `fetch_latest`; no gear, probe, or dashboard reads `health()`. The "green provider during a
  sustained outage" failure mode therefore has no surface to appear on today. This note is
  the contract for whoever wires it up first: pair it with the freshness alert, never
  substitute it for one.

**Relationship to the ledger's manual ingest.** The `RateProviderV1` pull driven by
`RateSyncJob` is the **PRIMARY** rate path. The ledger separately exposes a **SECONDARY**
manual/seed path — `POST /bss-ledger/v1/fx/rates` (ledger-owned, `(ledger, provision)` PEP
gate) — that upserts one rate directly into `ledger_fx_rate`. This gear does **not** own or
replace that endpoint; the two are complementary (automated feed vs manual break-glass /
bootstrap).

**Events.** Provider-outage signalling is the ledger's `RateSyncJob`, which emits
`billing.ledger.invariant.alarm` with `alarmCategory = fx-snapshot-missing` (Critical) when
a **configured** provider fails to fetch. The adapter only returns a `RateProviderError`;
the ledger decides the alarm.

An optional debug/liveness HTTP endpoint is deferred (O-6: metrics only for v1).

### 3.4 Internal Dependencies

- **`bss-ledger-sdk`** — the `RateProviderV1` trait and its types (`rate_provider.rs`); the fixed contract this gear implements.
- **`bss-rate-provider-sdk`** — this gear's own shared internal crate: the source-plugin GTS spec, exact-decimal conversion, the ISO-4217 currency-shape gate, HTTP-error mapping, fetch metrics, the shared HTTP-client builder, and the `register_rate_provider_plugin` registration helper used by all four crates.
- **`types-registry-sdk` / `types-registry`** — every plugin gear (including the core gear) registers a `PluginV1` instance here and the core gear queries it (`list_instances`) to discover source plugins; not used in the pre-plugin-pattern design.
- **ToolKit `ClientHub`** — cross-gear registry; each gear `register_scoped`s its `RateProviderV1` under its own GTS instance id (never an unscoped registration).
- **`toolkit-http`** — outbound HTTPS client (hyper + rustls under the hood); each source plugin builds its **own** instance via the shared builder helper — not one client shared by all sources.
- **`secrecy`** — `SecretString` for the http-json plugin's `api_key` (ECB has no credential).
- **Platform OTel meter** — each **source plugin** wires its own `OtelFetchMetrics` handle at `init()` from the process-global meter (§4 "Feature metrics"); the same instrument names coalesce across plugins. The core gear wires none — it emits no fetch instruments of its own (§4), so every fetch series is owned by the plugin that served the attempt.

### 3.5 External Dependencies

- **ECB reference rates** — primary source; free, published once per TARGET business day (`eurofxref-daily.xml`).
- **Bank / PSP feed** — fallback / settlement evidence; deferred to ops (O-10), onboarded via config when available.
- **Billing Ledger (`bss-ledger`)** — the sole consumer: `RateSyncJob` pulls `fetch_latest` and upserts each row under the per-rate provenance it carries (`ProviderRate.provider`, the serving source plugin's `id` — the composite's constant `provider_id()` never identifies the fetch source); `RateSource` and the FX stores consume the synced rates ([`../../ledger/docs/design/06-fx-multicurrency.md`](../../ledger/docs/design/06-fx-multicurrency.md)).

### 3.6 Interactions & Sequences

#### Sync tick → fetch → stamp

- [ ] `p1` - **ID**: `cpt-cf-bss-rate-provider-seq-sync-tick-fetch`

Once per tick the ledger `RateSyncJob` resolves its registered `RateProviderV1` (a scoped
`ClientHub` lookup matched by vendor against the core gear's `PluginV1<RateProviderPluginSpecV1>`
instance — the exact ledger-side call is out of this gear's boundary and not re-verified
here), calls `fetch_latest(ctx, &[], request_id)` (whole table), and upserts each returned
rate into `ledger_fx_rate` under the `provider` that rate carries. **Within this gear**,
every such call re-runs `DiscoveringRateProvider`'s discovery of its own source plugins
(§3.2) — nothing is cached between calls, so a plugin's registration, removal, or
`priority` change takes effect on the very next tick with no gear restart. The cost is one
types-registry list plus N in-memory `ClientHub` lookups per tick, on a job that ticks
hourly by default and never runs on the posting path. The caller context is
`SecurityContext::anonymous()` (system context) — no PEP gate on this internal cross-gear
plugin call.

#### Source fallback with provenance

- [ ] `p1` - **ID**: `cpt-cf-bss-rate-provider-seq-source-fallback`

Primary source fails (`Err` for the whole fetch) → the composite tries the next source in
priority order → the first `Ok(document)` is returned **whole**, with every rate in it
already stamped `provider = <that source's id>` → the ledger stores that id per row, so the
synced rows record the true upstream regardless of how calls from other consumers interleave.

#### All sources fail → ledger alarm

- [ ] `p1` - **ID**: `cpt-cf-bss-rate-provider-seq-all-sources-fail`

Every source returns `Err` → the composite returns the last `RateProviderError` → the
ledger job raises `FX_SNAPSHOT_MISSING` (`billing.ledger.invariant.alarm`,
`alarmCategory = fx-snapshot-missing`, Critical). The local store keeps its last synced
rates; staleness marking and post blocking are the ledger's call.

**The returned error is the LAST source's, so it is ordering-dependent and is not the
diagnostic of record.** A single error value cannot name which source is the actual outage,
and widening `RateProviderError` into an aggregate would push a per-source list through a
ledger-owned SDK type that maps to one alarm anyway. The normative contract is therefore
observability, not the error value:

- Every failed source attempt **MUST** be logged at `warn` before the next source is tried,
  carrying `source` (the attempted plugin's `id`), `request_id`, and the error.
- The `request_id` is the tick's own, passed down unchanged from the ledger's `RateSyncJob`
  through the composite to each source, so all attempts of one tick share it. Filtering the
  gear's logs by that one id reconstructs the full attempt sequence in priority order —
  including the sources whose errors the returned value discarded.
- The ledger's `FX_SNAPSHOT_MISSING` alarm **MUST** carry that same `request_id` in its
  detail, and so **MUST** the ledger's own failure log line. This is what makes the alarm the
  entry point to the attempt sequence rather than a dead end: its `provider` field is the
  *composite's* id, not the source that broke, and its error text is the last source's, so
  without the id an operator holding the alarm has no key to filter the gear's logs by.
- `fx_provider_fetch_errors_total{provider,kind}` stays the aggregate view; it counts
  failures per source over time but cannot reconstruct a *specific* tick, which is what the
  correlated log lines are for.

### 3.7 Database schemas & tables

**None.** The adapter is stateless — no tables, no migrations. The persisted FX state is
owned by the ledger:

- `ledger_fx_rate` — the local "latest known rates" store (`RateSyncJob` upsert target).
- `rate_snapshot` — the immutable per-lock frozen rate.

Both are defined in the ledger FX / Foundation designs
([`../../ledger/docs/design/06-fx-multicurrency.md`](../../ledger/docs/design/06-fx-multicurrency.md),
[`../../ledger/docs/design/01-repository-foundation.md`](../../ledger/docs/design/01-repository-foundation.md))
and are NOT part of this gear.

### 3.8 Deployment Topology

Three stateless gear crates at `gears/bss/rate-provider/{rate-provider,plugins/ecb-plugin,
plugins/http-json-plugin}` (placement per O-8; ECB's default `provider_id = "ecb"`),
deployed in-process with the platform gear set — no standalone service, no DB. **Startup
ordering (O-7, as implemented, superseding the original resolution below):** there is no
`deps` edge between the core gear and its source plugins, and none is planned. The core
gear's discovery is deferred to the first `fetch_latest` (the ledger's serve phase, after
every gear's `init()` has run) and re-runs on every subsequent tick too (§3.2), so
registration order among the three rate-provider gears never matters, and neither does a
source plugin registering, deregistering, or changing `priority` after startup — the next
tick picks it up, no restart needed.

## 4. Additional context

### Security & AuthZ

- **Caller context:** the ledger calls `fetch_latest` with `SecurityContext::anonymous()`
  (system context, not a per-request user). No PEP gate on this trait — it is an internal
  cross-gear plugin call, not a tenant-scoped resource.
- **No tenant data:** rates are global reference data; the adapter never sees tenant PII
  and never writes tenant-scoped rows (the ledger does the RLS-scoped fan-out).
- **Outbound TLS:** HTTPS via `toolkit-http` (hyper + rustls). ECB is public/unauthenticated;
  paid providers need an API key (see constraint `cpt-cf-bss-rate-provider-constraint-secrets`).
- **Outbound URL validation:** every `base_url` must have the `https` scheme **and** a host,
  checked at that plugin's own `init()`
  (`bss_rate_provider_sdk::http_client::build_source_http_client`) — fails loud on plain
  `http`, on any other scheme, and on a hostless URL. The URL is *parsed* with the same
  `http::Uri` type `toolkit-http` uses on the request path, not string-matched, so the gate
  applies identical normalization: the scheme is compared case-insensitively (per RFC 3986
  §3.1 `HTTPS://host` **is** https and is accepted — an earlier prefix check rejected that
  working configuration), while `https://` with no authority is rejected (a prefix check
  accepted it). **There is no loopback / private-network address check on `base_url`** —
  that specific control is not implemented; a misconfigured `base_url` pointed at an internal
  host is only caught by network reachability at fetch time, not rejected up front. (An
  earlier draft of this document claimed such a check existed "unless explicitly allow-listed
  for dev" — it does not, and there is no allow-list config either.)
- **Redirect safety (this part IS implemented, inherited, not gear-specific code):** the
  shared `toolkit-http` client's *default* `RedirectConfig` (`same_origin_only: true`,
  `strip_sensitive_headers: true`, `allow_https_downgrade: false`) is what every source
  plugin gets by not overriding it — cross-host redirects are refused unless allow-listed,
  `Authorization`/`Cookie` are stripped on any redirect that does cross an origin, and an
  HTTPS→HTTP downgrade is blocked. Config is operator-supplied (not tenant input), so this
  is defense-in-depth against misconfiguration or a compromised/malicious upstream, not a
  tenant-facing SSRF surface.
- **Provider authenticity:** trusting the provider feed's authenticity is upstream/ops.

### Feature metrics

All metrics exposed as Prometheus scrape targets. (Provider **fallback** selection is
measured ledger-side as `ledger_fx_provider_fallback_total{provider}`, emitted at lock time
by `RateSource`; the adapter measures the fetch itself.)

| Vector | Metric | Description | Target Threshold |
|--------|--------|-------------|------------------|
| **Efficiency** | `fx_provider_fetch_duration_seconds{provider}` | Outbound fetch+parse latency | p95 ≤ 2 s |
| **Performance** | `fx_provider_rates_returned{provider}` | Pairs returned per successful fetch | — |
| **Reliability** | `fx_provider_fetch_errors_total{provider,kind}` | Fetch failures by `RateProviderError` kind | — |
| **Reliability** | `fx_provider_last_success_timestamp{provider}` | Publication time of the last successfully served document — the feed's own `as_of`, **not** the time we fetched it. A feed-freshness gauge, never a job-liveness one | — |
| **Security** | `fx_provider_upstream_status_total{provider,status}` | Upstream HTTP status distribution | — |

**Instrumentation ownership.** These are the **adapter's own** instruments (the fetch
happens inside the source, out of the ledger's sight), so this gear MUST own a metrics
handle — it does not piggy-back on the ledger's meter. Wire it at `init()` from the
platform OTel meter; each source records under its own `provider_id` label. The
`{provider}` label is the source `provider_id` (`"ecb"` / `"bank-x"`); for the composite,
the source that actually served.

**Liveness must be alerted on absence, not on errors.** Every metric above is emitted *by
a fetch*. If the ledger's `RateSyncJob` stops ticking — scheduler wedged, task panicked and
was not restarted, gear never reached `serve` — then no fetch happens, so
`fx_provider_fetch_errors_total` never increments and any alert built on it stays silent
while the rate store quietly goes stale. The provider-failure alarm cannot cover this case
by construction.

What *does* catch it is the pair of instruments a completed attempt always moves. Inside
`fetch_and_parse` exactly one of them fires per attempt: a success records into
`fx_provider_fetch_duration_seconds` (after the parse, so the `_count` series counts served
documents), and any failure increments `fx_provider_fetch_errors_total`. That gives three
distinguishable states:

| State | `fetch_duration_seconds_count` | `fetch_errors_total` |
|---|---|---|
| Healthy | rising | flat |
| Upstream outage | flat | **rising** |
| **Nothing is attempting a fetch** | flat | flat |

| Alert | Condition | Owner | Rationale |
|-------|-----------|-------|-----------|
| `FxNoFetchAttempted` | `rate(fx_provider_fetch_duration_seconds_count[3 × fx.rate_sync_tick_secs]) == 0 and rate(fx_provider_fetch_errors_total[3 × fx.rate_sync_tick_secs]) == 0 and rate(ledger_fx_rate_sync_ticks_total[3 × fx.rate_sync_tick_secs]) > 0`, held (`for:`) for at least one full sync pass — the adapter's 30 s per-source timeout ceiling × the configured source count | Platform on-call | "The job ticks but no attempt completes" — i.e. discovery yields no sources, which is what separates a dead feed from a slow one. The heartbeat term is part of the **condition**, not post-hoc correlation: without it a dead scheduler fires this alert for the wrong reason (that state belongs to `FxRateSyncStalled`), and because the ticker awaits the pass inline, the heartbeat stays flat while an attempt is still in flight — a slow pass suppresses this alert instead of firing it. The `for:` hold plus the 60 s floor on `fx.rate_sync_tick_secs` (ledger `FxConfig::validate`, window ≥ 180 s) close the remaining short-window false positive a sub-minute tick would otherwise allow. |
| `FxFeedStale` | `time() - fx_provider_last_success_timestamp{provider} > <feed publication cadence>` | Platform on-call (feed freshness) | A **separate** concern from liveness — see the warning below. The threshold is set by how often the feed publishes, not by the sync tick: ECB publishes once per TARGET business day, so anything under ~72 h fires across every normal weekend. |

**Job liveness is alerted in the ledger, not here.** `FxRateSyncStalled` and
`FxRateSyncNeverRan` are defined over `ledger_fx_rate_sync_ticks_total` — the ledger's own
scheduler heartbeat, incremented once per `RateSyncJob` tick before any provider is resolved.
They live where the job lives: see the ledger's `design/06-fx-multicurrency.md`, the rate-source
DoD in §8. This gear observes fetch **attempts**, one step downstream of the tick, so it is not
in a position to define them.

> **Do not build the liveness alert on `fx_provider_last_success_timestamp`.** That gauge is
> deliberately set to the served **document's own publication time**, never wall-clock
> (§"Testing architecture" pins this), so `time() - gauge` measures how old the *data* is,
> not how long since we last ran. With ECB — whose `as_of` is a date at 00:00 UTC — a
> perfectly healthy deployment already reads ~12 h at midday and ~66 h on a Sunday evening.
> A `3 × fx.rate_sync_tick_secs` threshold on it would fire continuously. It answers a different
> question, which is why it gets its own alert above.

**This gear sees fetch attempts, not ticks — which is why the ledger heartbeat is a term in
the alert's condition, not an after-the-fact correlation.** The
core gear emits no instruments of its own, and `DiscoveringRateProvider::fetch_latest` fails at
`discover()?` before reaching any source, so a discovery failure (a `vendor` typo, the
types-registry being unavailable, no source plugin registered) leaves both counters here flat.
On its own that is indistinguishable from a dead job; against the ledger's tick heartbeat it is
not:

| `ledger_fx_rate_sync_ticks_total` | `fx_provider_fetch_duration_seconds_count` | Diagnosis |
|---|---|---|
| flat | flat | The scheduler is dead — nothing is syncing (`FxRateSyncStalled`, ledger-side) |
| **rising** | flat | The job is alive but discovery yields no sources (`FxNoFetchAttempted`) |

The discovery half is then diagnosable from the error itself, which names the expected vendor
and the ones actually present in the registry (§3.2).

Both alerts are ledger-deployment concerns (the job lives there); the ones above are listed
here because the instruments they read are this gear's.

**Acceptance test.** With a configured provider, stop the `RateSyncJob` and assert that
`fx_provider_fetch_duration_seconds_count` and `fx_provider_fetch_errors_total` both stay
flat — i.e. that the stall is invisible to error-based alerting. Assert separately that
`fx_provider_last_success_timestamp` alone would NOT have caught it, since it can read
arbitrarily stale on a healthy feed between publications. That the *tick* stopped is asserted
ledger-side on `ledger_fx_rate_sync_ticks_total` (ledger `design/06-fx-multicurrency.md` §8).

### Testing architecture

**Correction vs. the original draft below:** there is no `FakeHttpTransport` mock-transport
abstraction anywhere in the implementation. Unit tests call the pure parsing/mapping/
conversion functions directly with byte/JSON fixtures (no HTTP layer involved at all);
HTTP-layer behavior is covered by integration tests against a *real* in-process `axum`
server over loopback, not a fake transport trait. There is also a whole test category the
original draft didn't anticipate — discovery/vendor-filtering/priority-ordering/caching —
covered by a `ClientHub` + mock-registry integration test, not a unit test, since it
exercises real cross-gear wiring.

| Level | Database | Network | What is real | What is mocked |
|---|---|---|---|---|
| **Unit** | None | None | Parser (`parse_ecb_xml`), field mapping (`map_json_document`/`json_lookup`), `rate_micro` conversion, HTTP-error mapping, `provider_id` | Nothing — pure functions called directly with fixture bytes/JSON, no transport abstraction |
| **Component integration** (`tests/discovery.rs`) | None | None | A real `ClientHub` + `DiscoveringRateProvider`; fake `RateProviderV1` sources registered scoped | `TypesRegistryClient` — `types-registry-sdk::testing::MockTypesRegistryClient`, a real test-util the SDK ships |
| **HTTP integration** (`tests/ecb_integration.rs`, `tests/http_json_integration.rs`) | None | Real in-process `axum` server over loopback | `EcbRateProvider` / `HttpJsonRateProvider` end-to-end, incl. auth headers, timeouts, connection failure | The real external ECB / bank endpoint |
| **API** | N/A | In-process | Trait-level: `fetch_latest`/`health` contract behavior | — (no REST surface) |
| **E2E** | Real (ledger) | Real HTTPS | Adapter registered in ClientHub; ledger `RateSyncJob` populates `ledger_fx_rate`; an EUR-invoice-under-USD post locks a rate | Optionally the live ECB feed (gated) |

**Unit tests** (`*_tests.rs` files next to the code under test, per this repo's
`de1101_tests_in_separate_files` convention):

| What to test | Verification target |
|---|---|
| Parse ECB daily XML fixture → `(NaiveDate, Vec<(String, String)>)` | All EUR pairs decoded; `as_of` = publication date UTC; a duplicate currency is logged and the first occurrence wins; a second **distinct** date fails the document with an error naming both dates, while a repeat of the same date still parses |
| `rate_micro` conversion determinism | `round(rate×1e6)` half-to-even (`rust_decimal`) over exact decimal parsing (no `f64` path); half-way golden vectors incl. negative; exact `i64::MAX`/`i64::MIN` boundary and one-past-boundary overflow |
| Requested pair not published / case-insensitive pair match / inverse leg (X→EUR) requested | Omitted from result (not an error); a lowercase-cased request still matches (both sources); the inverse leg is never synthesized |
| Upstream 5xx / network failure / malformed payload | `UpstreamStatus` / `Unreachable` / `Internal` respectively, via `map_http_error` |
| `InvalidUri` mapping never echoes the raw URL | `Internal` message contains the structured `kind` + `reason`, never the URL (which may carry a spliced-in secret) |
| `provider_id` | Returns configured id |
| **Future-publication-time bound** (`publication_time_tests.rs`, plus one HTTP-integration test per source) | Exactly `MAX_FUTURE_SKEW_HOURS` ahead still serves and one second past it is rejected — the pair pins the bound, where either alone would pass for an arbitrary window; a full day ahead serves (the date-only-feed case); an arbitrarily old `as_of` is NOT rejected here (age is the ledger's rule); end-to-end, a future-dated document fails the fetch with an error naming the offending timestamp |
| **Per-rate provenance** (both sources) | Every returned rate carries `provider = <the serving source's id>`, so the ledger stores the true upstream per row |
| **ISO-4217 currency gate** (both sources) | A non-three-ASCII-letter `quote` from the feed is dropped/skipped and logged; a lowercase code is normalized to uppercase; a malformed configured `mapping.base` fails the whole http-json document; an all-malformed ECB feed surfaces the empty-table `Internal` error |
| **Per-entry resilience to a bad rate** (both sources) | One unconvertible (non-numeric, zero, negative, overflowing) rate is skipped and logged while the rest of the document still serves; a half-populated ECB `Cube` is ignored and logged; only an all-entries-failed batch is an `Internal` error, whereas an empty result caused purely by a non-empty requested-`pairs` filter is `PairUnavailable` (never `Ok`) |
| **ISO-4217 helper** (`currency.rs`) | Accepts three-letter codes case-insensitively; rejects wrong length, non-alphabetic, injection-shaped, and multibyte-but-3-byte input |
| Zero-mappable `http-json` document (empty `rates` object, or every entry individually fails to map) | `RateProviderError::Internal` in both cases, never `Ok([])` — the composite fallback must trigger |
| **`http-json` `as_of` / `rates` path error branches** | Missing `as_of`, non-RFC3339 `as_of`, non-string `as_of`, missing `rates`, `rates` not an object — each an `Internal` error (a regression defaulting `as_of` to `now()` would otherwise mis-timestamp silently) |
| Generic `http-json` mapping → `Vec<ProviderRate>` | `mapping` resolves base/quote/rate/as_of; string AND numeric rate values accepted; an unmappable entry is skipped (logged with its quote + reason), never fabricated |
| **`http-json` auth config shape** | `bearer`/`header-key` carry their key; a kind that needs a key without one, a key attached to `none`, an unknown kind, and a stray top-level `api_key` are each rejected at config load |
| Composite fallback order + provenance; all-sources-fail; **health fallback**; **empty source list** | Secondary document returned whole on primary failure with its own provenance; **distinct** per-source errors prove the LAST error propagates (fetch AND health); health short-circuits on the first healthy source; `provider_id()` returns the configured id and `fetch_latest`/`health` error (never panic) when the list is empty |
| **`fetch_and_parse` metrics contract** (`fetch_tests.rs`, recording `FetchMetrics` fake over a real local server) | Success records status + duration + count + `set_last_success(<document's as_of>)`, never wall-clock; a non-2xx records `kind="upstream_status"` and NO success; a parse failure's label comes from the returned error (`invalid_pair`, `internal`), not a hardcoded kind; a transport failure records `kind="unreachable"` and no status |
| **`build_source_http_client`** | `https://` builds, and so does any casing of the scheme (`HTTPS://`, `HttPs://`) since a URI scheme is case-insensitive; plain `http` (either casing), other schemes, a scheme-less URL, and an `https` URL with no host are all rejected; a zero `timeout_ms` is rejected while 1 ms builds, and exactly `MAX_TIMEOUT_MS` builds while one millisecond past it is rejected (the pair pins the ceiling, where either alone would pass for an arbitrary bound); no rejection error — scheme, parse failure, or oversized timeout — echoes the URL or its embedded credential, and the oversized-timeout message names the ceiling |
| **OTel metric instruments** (`metrics_tests.rs`, in-memory exporter) | All five documented Prometheus names are exported; every instrument carries `provider`; errors carry `kind`; statuses carry `status`; recorded counter/gauge values reach the exporter unchanged |
| Each gear's built instance id parses as a valid GTS id | `assert_registration_builds_valid_gts_id` (shared helper, one `#[test]` per gear) |

The http-json plugin's `mapping`-required check in `init()` is pinned by a dedicated test
(`gear.rs`, driving the real `init()` over a static config provider): a present,
otherwise-valid config without `mapping` aborts init with an error naming the field, so a
feed that could map no document never registers. The auth half of that validation is
covered at the config-parse level instead, since it became a serde error rather than a
runtime check.

**Component-integration tests** (`tests/discovery.rs`; real `ClientHub`, no network, no DB):

| What to test | Verification target |
|---|---|
| Composes in priority order, stamps the serving source | Lower `priority` served first; the returned rate carries its id; `provider_id()` stays the composite's constant identity |
| Falls back to the next priority on failure | Fallback source serves; the rate's `provider` reflects it |
| Filters out a different-vendor source, even at a lower priority | Vendor mismatch excludes it regardless of priority |
| No matching-vendor source registered | `fetch_latest` errors |
| **Instance published without a scoped client** | That source is excluded, not fatal — a healthy sibling still serves |
| **A failed tick self-heals** | First tick fails (instance published, client not yet registered); after the client registers, the next tick succeeds and re-queried the registry — no cached failure |
| Discovery re-runs every tick, not just the first | A second `fetch_latest` re-lists instances (no cross-tick cache) so a `priority`/registration change takes effect without a restart |
| Concurrent fetches each discover independently | Two concurrent callers each run their own discovery pass — nothing to dedupe against once there is no shared cache |
| **`health()` over discovered sources** | Probes the discovered chain and falls through a failing source to a healthy one |

**HTTP-integration tests** (`tests/ecb_integration.rs`, `tests/http_json_integration.rs`; a
real in-process `axum` server, no DB):

| What to test | Setup | Verification target |
|---|---|---|
| Full fetch over local server | Serve the ECB XML fixture / a JSON feed body | `fetch_latest(&[])` returns the full table/document |
| Whole-table vs specific pairs, incl. out-of-base omission | Serve full feed | A requested pair returns only that pair; an unpublished/wrong-base pair is omitted, never synthesized |
| `health` probe (HEAD), both sources | Serve 200; serve 403 / 404 / 405 / 500 / 503; a `GET`-only route (so HEAD gets axum's 405); no listener at all | Every HTTP status is `Ok(())` — the host answered — and only the transport failure is `Unreachable`. The pair pins the rule: either case alone would pass for a probe that always succeeded, or always failed on a non-2xx. The `GET`-only case is the one that makes a HEAD probe viable for an arbitrary configured feed |
| Auth headers (`bearer` / `header-key`) | Server 401s unless the exact header is present | Correct header sent; `none` sends no credential and the authenticated feed rejects it (401); a wrong key surfaces `UpstreamStatus(401)` rather than looking successful. A *keyless* `bearer` is not an HTTP case at all — `Auth::Bearer` requires its `api_key`, so the shape is unrepresentable and is covered as a config-load rejection in `config_tests.rs` |
| Upstream 5xx / connection refused | Serve 503 / drop the listener after binding | `UpstreamStatus(503)` / `Unreachable` |

The slow-but-reachable upstream is pinned at the `fetch_and_parse` level (`fetch_tests.rs`,
shared by both plugins): a live local server that answers only after the configured timeout
maps to `Unreachable`, records the error metric, and moves no success instrument and no
upstream-status count — the case connection refusal cannot represent, since there the
transport fails instantly instead of being cut off by `timeout_ms`.

**API tests:** no REST surface — the "contract" tests are the trait-level behaviors covered
at Unit/Integration. If a debug endpoint is ever added (O-6), add RFC 9457 error tests then.

**E2E tests** (planned location: `testing/e2e/modules/bss-ledger/`, extends the FX suite):

| What to test | Marker | Verification target |
|---|---|---|
| Adapter registered → ledger sync populates store | `@pytest.mark.smoke` | After a `RateSyncJob` tick, a cross-currency post locks a rate (no `FX_RATE_UNAVAILABLE`); the posted line's `rate_snapshot_ref` is then readable via the item endpoint `GET /fx/rate-snapshots/{rateId}` (there is no collection `GET`) |
| Provider unreachable → post blocks | — | With the adapter down, an EUR-under-USD post returns `FX_RATE_UNAVAILABLE` (fail-safe), and `fx-snapshot-missing` alarm fires |
| Live ECB fetch (gated) | `@pytest.mark.external` | A real ECB fetch returns a non-empty EUR table |

**What must NOT be mocked:**

| Component | Why |
|---|---|
| `rate_micro` conversion | Money precision — must be exact and deterministic against real parsing |
| The `RateProviderV1` contract behavior (`&[]` semantics, omit-on-unavailable) | The ledger job relies on it verbatim |
| Ledger fail-safe (block on empty store) — E2E | Proves "block, not guess" end to end |

**NFR verification mapping:**

| NFR | Test level | How verified |
|---|---|---|
| Post-path isolation | E2E | Provider down → posts still fast; only FX posts block |
| Fetch latency p95 ≤ 2 s | Integration + load | Timed fetch against local server; sample live ECB |
| Deterministic conversion | Unit | Golden-vector tests over the conversion function |
| Feed freshness | E2E | Sync tick populates store within the tick window |

### Decision register

| Ref | Item | Resolution | Owner |
|-----|------|------------|-------|
| **O-1** | Multiple providers vs single `dyn RateProviderV1` | ✅ **DECIDED — composite adapter, no merge.** ONE composite registered; ordered sources; first whole document; provenance stamped per rate (§3.1/§3.2 — the last-served-index scheme this originally specified was replaced, see O-7a). Variant (b) — a ledger-side scoped multi-provider loop — stays a future option if per-pair fallback is ever needed. **REVISED 2026-07-23 (plugin rework): each source is a scoped `PluginV1` and the composite is itself a discovered plugin — see the Implementation-revision note at the top.** | Architecture |
| **O-2** | ECB source & format | ✅ **Accepted (2026-07-08):** direct ECB daily XML for prod; Frankfurter allowed for dev; SDMX optional. **As implemented:** XML-only — no `format` config field, no Frankfurter/SDMX code path shipped. Add if a non-XML feed is ever actually needed. | PM + Architecture |
| **O-3** | Triangulation ownership | ✅ **DECIDED (2026-07-08) — the ledger owns triangulation.** The adapter emits only native direct pairs; cross-base rates are computed ledger-side in `RateSource`. Companion ledger change required (below). | Architecture |
| **O-4** | Conversion rounding mode | ✅ **Accepted (2026-07-08):** banker's rounding (half-to-even) — inherited from the ledger's platform default (`cpt-cf-bss-ledger-fr-money-rounding-scale`, `p1`), not selected here. Not a release gate for this gear: deviating would break that requirement rather than differ from it, so a revision can only come from the ledger changing its default. | Ledger (owner) · PM + Finance (informed) |
| **O-5** | `rate_micro` precision sufficiency | ✅ **Accepted (2026-07-08):** keep ×1e6 (6 dp) for v1; revisit for high-unit / crypto pairs (any change is an SDK change). | Architecture |
| **O-6** | Debug/observability endpoint | ✅ **Accepted (2026-07-08):** metrics only for v1 — no debug HTTP endpoint; ops rely on metrics + the trait `health`. | Team |
| **O-7** | Gear vs plugin & startup order | ✅ **Accepted (2026-07-08):** rely on the fail-safe + next tick; verify startup ordering during implementation (add a ledger `deps` edge if ordering proves unreliable). **REVISED 2026-07-23 (plugin rework): no `deps` edge — the ledger discovers the composite lazily each rate-sync tick, so a late adapter self-heals.** | Architecture |
| **O-7a** | Composite provenance coupling (from O-1) | ✅ **RESOLVED (superseding the earlier "accepted for v1").** The original design had `provider_id()` report the last-served source, which held only while a single non-concurrent ticker called `fetch_latest` before `provider_id` in one pass — any second `ClientHub` consumer fetching in between made the ledger stamp the wrong source onto `ledger_fx_rate`/`rate_snapshot`. Taken instead: the option the entry itself named — `ProviderRate` now carries its own `provider`, each source stamps it, and `provider_id()` is a constant adapter identity. No call-order assumption remains. | Architecture |
| **O-8** | Crate placement & naming | ✅ **Accepted (2026-07-08):** `gears/bss/rate-provider`, `provider_id = "ecb"` (confirm against gear conventions at implementation). | Team |
| **O-9** | Jira / slice linkage | ✅ **Accepted (2026-07-08):** create a Technical task under the Slice-5 FX epic (VHP-1853 / VHP-1986 family), linked to the O-3 companion ledger ticket — action pending. | PM |
| **O-10** | Bank / PSP fallback source | ✅ **Accepted (2026-07-08):** v1 = ECB-only; bank/PSP added later as a `sources[]` entry (generic `http-json` if a plain REST feed, else a dedicated `kind` for signed/settlement auth). **As implemented (plugin rework):** added later as one more deployed plugin gear configured with a matching `vendor` (the existing `bss-rate-provider-http-json-plugin` if a plain REST feed, else a new plugin crate for signed/settlement auth). Concrete feed + credentials deferred to ops. | PM + Ops |
| **O-11** | Generic `http-json` mapping grammar | ✅ **Accepted (2026-07-08):** v1 = single-base JSON feeds, simple field paths, `none` / `bearer` / `header-key` auth; richer transforms deferred. | Architecture |
| **O-12** | `init()` config-validation strictness | ✅ **DECIDED (2026-07-17):** fail `init()` loud on an unknown `kind`, an empty `sources[]`, or a `sources[]` order that does not match the ledger `fx.provider_order` — a mismatch would let the composite fetch one provider while the ledger's precedence resolution prefers another's stored rate. **REVISED 2026-07-23 (plugin rework): no `sources[]` — source assembly is plugin discovery ordered by each plugin's `priority`; cross-gear alignment is a matching `vendor` (core gear ↔ source plugins ↔ ledger `fx.provider_vendor`).** | Architecture |
| **O-13** | Is coverage a success criterion for a served document? (raised in review of the parsing rules vs the "first whole document" / "all-or-nothing per source" phrasing) | ✅ **DECIDED — no. Per-entry skip is the normative semantic; "all-or-nothing" is about provenance only.** A document with some entries skipped, or with fewer pairs than the ledger would like, is a **success** and does NOT trigger fallback. Neither candidate gate was taken: "all requested pairs" is vacuous on the real call path (`RateSyncJob` fetches with empty `pairs`) and incompatible with ECB's EUR-only publication; "a minimum expected set" is an operator-maintained duplicate of each feed's contents that goes stale on the first upstream listing change. Both also invert the intended failure mode — any provider error becomes `FX_SNAPSHOT_MISSING` and refreshes nothing, so one bad entry would stale every pair instead of one. A pair that stops being refreshed is surfaced by the layer that owns the age policy: the ledger's staleness window → `FX_RATE_UNAVAILABLE` (block, never a wrong rate). Per-pair fallback stays deferred (O-1 variant (b)). See §2.1 "One source per document — provenance, not coverage". | Architecture |

### Companion ledger change (hard dependency, from O-3)

O-3 puts triangulation in the ledger, so this gear ships **direct pairs only**. Enabling
the ledger's deferred triangulation is therefore a **hard dependency**, tracked as a
separate `bss-ledger` work item — NOT part of this gear:

- **Where:** `bss-ledger` `infra/fx/rate_source.rs` — today `resolve()` reads direct pairs
  only (a documented TODO); it MUST compute `X → EUR → Y` (via the configured bridge
  currency) when no direct pair exists. This **includes deriving the `X → EUR` leg by
  deterministically inverting** the stored `EUR → X` rate — the adapter emits only ECB's
  native EUR-based pairs, so without ledger-side inversion no non-EUR-base pair (e.g.
  `USD→EUR`) can resolve at all.
- **Snapshot:** the resulting `rate_snapshot` MUST record `triangulated_via` (the bridge
  currency) — the column already exists on `ledger_fx_rate_snapshot`.
- **Determinism:** the bridge path + rounding MUST be deterministic and
  auditor-reproducible (banker's rounding per O-4).
- **Sequencing:** this adapter can ship first — EUR-functional / EUR-base tenants already
  work with direct pairs. Non-EUR-functional tenants are unblocked only once the ledger
  triangulation lands. Track the two as linked tickets (O-9).

## 5. Traceability

- **PRD (this gear)**: [`PRD.md`](./PRD.md) — the adapter's own product requirements
  (`cpt-cf-bss-rate-provider-fr-*` / `-nfr-*`), derived from the ledger PRD below.
- **Upstream PRD**: [`../../ledger/docs/PRD.md`](../../ledger/docs/PRD.md) — § Multi-currency and
  foreign exchange, § FX rate-source failure and staleness
  (`cpt-cf-bss-ledger-fr-multi-currency-fx`, `cpt-cf-bss-ledger-fr-fx-rate-source-failure`).
- **Consuming design**:
  [`../../ledger/docs/design/06-fx-multicurrency.md`](../../ledger/docs/design/06-fx-multicurrency.md)
  (the ledger side: `RateSource`, staleness, snapshots, the rate-source-fallback
  algorithm, and the frozen rate-snapshot state) and
  [`../../ledger/docs/design/01-repository-foundation.md`](../../ledger/docs/design/01-repository-foundation.md)
  (functional columns, currency-scale registry).
- **Code seam (existing)**: `bss-ledger-sdk` `rate_provider.rs` (`RateProviderV1` trait);
  `bss-ledger` `infra/jobs/rate_sync.rs`, `infra/fx/rate_source.rs`, `config.rs`
  (`FxConfig`), `module.rs` (ClientHub resolution).
- **Provenance**: authored from the architecture-repo draft
  `DESIGN-billing-fx-module-202607011613` (vhp-architecture, `docs/bss/design/`), which
  itself traces to `PRD-billing-ledger-balances-202604041200`,
  `DESIGN-billing-ledger-balances-202606091200` (slices 01 / 06), and
  `ADR-platform-persistence-layer-202601221200`.
