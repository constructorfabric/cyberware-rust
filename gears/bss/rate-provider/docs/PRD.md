Created:  2026-07-17 by Virtuozzo International GmbH
Updated:  2026-07-17 by Virtuozzo International GmbH

<!-- CONFLUENCE_TITLE: [BSS]: FX Rate Provider (Adapter Gear) — Product Requirements -->
<!-- Related: ./DESIGN.md, ../../ledger/docs/PRD.md, ../../ledger/docs/design/06-fx-multicurrency.md | Owners: @vstudzinskyi (BSS Billing Platform team) -->

# PRD — FX Rate Provider (Adapter Gear)

<!-- toc -->

- [1. Overview](#1-overview)
  - [1.1 Purpose](#11-purpose)
  - [1.2 Background / Problem Statement](#12-background--problem-statement)
  - [1.3 Goals (Business Outcomes)](#13-goals-business-outcomes)
  - [1.4 Glossary](#14-glossary)
- [2. Actors](#2-actors)
  - [2.1 Human Actors](#21-human-actors)
  - [2.2 System Actors](#22-system-actors)
- [3. Operational Concept & Environment](#3-operational-concept--environment)
  - [3.1 Gear-Specific Environment Constraints](#31-gear-specific-environment-constraints)
- [4. Scope](#4-scope)
  - [4.1 In Scope](#41-in-scope)
  - [4.2 Out of Scope](#42-out-of-scope)
- [5. Functional Requirements](#5-functional-requirements)
  - [5.1 Rate Feed](#51-rate-feed)
  - [5.2 Sources & Fallback](#52-sources--fallback)
- [6. Non-Functional Requirements](#6-non-functional-requirements)
  - [6.1 Gear-Specific NFRs](#61-gear-specific-nfrs)
  - [6.2 NFR Exclusions](#62-nfr-exclusions)
- [7. Public Library Interfaces](#7-public-library-interfaces)
  - [7.1 Public API Surface](#71-public-api-surface)
  - [7.2 External Integration Contracts](#72-external-integration-contracts)
- [8. Use Cases](#8-use-cases)
  - [Fallback serve with provenance](#fallback-serve-with-provenance)
  - [Onboard a new REST feed by configuration](#onboard-a-new-rest-feed-by-configuration)
- [9. Acceptance Criteria](#9-acceptance-criteria)
- [10. Dependencies](#10-dependencies)
- [11. Assumptions](#11-assumptions)
- [12. Risks](#12-risks)
- [13. Open Questions](#13-open-questions)
- [14. Traceability](#14-traceability)

<!-- /toc -->

> **Implementation revision (2026-07-23) — `*-plugin` pattern.** The requirements
> below are unchanged, but the gear was built on the platform's plugin pattern:
> each rate source (ECB, http-json) is a **source plugin** that self-registers in
> the types-registry, a **core gear** discovers + composes them (ordered by each
> plugin's `priority`), and the ledger discovers the composite **lazily each sync
> tick**. So "config-driven source assembly / ordered fallback" (FR
> `cpt-cf-bss-rate-provider-fr-config-onboarding` / `-ordered-source-fallback`) is
> realized by plugin discovery + `priority`, and "strict config validation" (FR
> `cpt-cf-bss-rate-provider-fr-strict-config-validation`) is per-plugin (an
> http-json plugin requires a `mapping` + https `base_url`) with cross-gear
> alignment by a shared `vendor`. See `DESIGN.md` § Implementation revision.
>
> **Review revision (2026-07-28).** Two requirement-level clarifications came out
> of code review, both narrowing how a requirement is met rather than what it is:
> true-source provenance (`-fr-source-provenance`) is carried **per rate** instead
> of via a separate "who served last" call, which removes a concurrency hazard on
> audit data; and the http-json plugin's authentication config is now shaped so
> unusable credential combinations cannot be expressed at all (a config-load
> error), instead of being caught by a startup check
> (`-fr-strict-config-validation`). Feed-supplied currency codes are also gated on
> the ISO-4217 shape before reaching any tenant store.

## 1. Overview

### 1.1 Purpose

The **FX rate-provider gear** supplies the Billing Ledger with **live foreign-exchange
reference rates**. It is a stateless fetch-only adapter: it retrieves the latest published
rates from configured external sources (ECB primary; further feeds by configuration) and
hands them to the ledger through the ledger-owned `RateProviderV1` contract. It stores
nothing, exposes no REST surface, and performs no accounting.

### 1.2 Background / Problem Statement

The ledger's multi-currency posting (ledger PRD § Money, Rounding & Foreign Exchange)
requires a live rate feed to translate transaction currency into functional currency, but
the ledger deliberately declares the provider integration out of its own scope — it ships
only the consuming seam (`RateProviderV1`, `RateSyncJob`, the local rate store, and the
fail-safe that blocks FX posts when no rate is available). Until an adapter implements
that seam, every FX post blocks (`FX_RATE_UNAVAILABLE`). This gear closes that gap.

### 1.3 Goals (Business Outcomes)

- **Unblock multi-currency billing**: with the adapter deployed and one source configured,
  cross-currency invoice posting works without manual rate seeding, for the pairs a
  configured source publishes directly. ECB publishes EUR-based pairs, so this covers
  EUR-functional tenants; non-EUR functional currencies additionally need the ledger-side
  triangulation/inversion change tracked as a hard companion dependency (DESIGN §4).
- **Auditable rates**: every synced rate is traceable to a named provider and its original
  publication timestamp — no fabricated or silently stale values.
- **Cheap provider onboarding**: a new plain REST rate feed is added by configuration
  only, with no code change and no redeployment of consuming modules.

### 1.4 Glossary

| Term | Definition |
|------|------------|
| Reference rate | An FX rate published by an institution (e.g. ECB) for a currency pair on a given date. |
| Functional currency | The tenant's accounting currency the ledger translates into. |
| Direct pair | A pair the provider itself publishes (ECB publishes EUR→X only). |
| Rate document | The whole set of pairs one source returns for one fetch. |
| Provenance | Which concrete source actually served the rates recorded by the ledger. |
| Fail-safe by absence | On provider failure the ledger blocks FX posts rather than guessing a rate. |

## 2. Actors

### 2.1 Human Actors

#### Platform Operator

**ID**: `cpt-cf-bss-rate-provider-actor-platform-operator`

- **Role**: Configures rate sources (order, endpoints, credentials) and operates the
  platform deployment; reacts to feed-freshness alarms raised by the ledger.
- **Needs**: Config-only source management, clear startup validation errors, fetch metrics.

#### Finance Controller / Auditor

**ID**: `cpt-cf-bss-rate-provider-actor-finance-auditor`

- **Role**: Signs off FX treatment; audits which rate was applied to which posting.
- **Needs**: Deterministic rate conversion and per-rate provider/publication-time provenance.

### 2.2 System Actors

#### Billing Ledger (`RateSyncJob`)

**ID**: `cpt-cf-bss-rate-provider-actor-ledger-rate-sync`

- **Role**: Sole consumer. Periodically pulls the latest rate document through
  `RateProviderV1` and upserts it into the ledger's local rate store.

#### ECB Reference-Rate Feed

**ID**: `cpt-cf-bss-rate-provider-actor-ecb-feed`

- **Role**: Primary external source — free, EUR-based daily reference rates.

#### Bank / PSP Rate Feed (future)

**ID**: `cpt-cf-bss-rate-provider-actor-bank-psp-feed`

- **Role**: Fallback / settlement-evidence source, onboarded by configuration when
  procured by ops (DESIGN decision O-10).

## 3. Operational Concept & Environment

Runtime, OS, and lifecycle policy follow the repository-level platform defaults
([`guidelines/`](../../../../guidelines/)); the consuming seam is defined by the parent
[ledger PRD](../../ledger/docs/PRD.md).

### 3.1 Gear-Specific Environment Constraints

- Requires **outbound HTTPS egress** to configured provider endpoints (unusual for gears —
  most have no external egress).
- No database and no per-tenant state: rates are global reference data; the ledger owns
  all persistence and the per-tenant fan-out.

## 4. Scope

### 4.1 In Scope

- Fetching the latest published rate document from configured external sources.
- Ordered cross-source fallback at fetch time with true-source provenance.
- Implementing the ledger's `RateProviderV1` contract (fetch, health, provider identity).
- Config-driven source assembly, including no-code onboarding of plain REST JSON feeds.
- Deterministic conversion of published decimal rates into the contract's fixed-precision
  integer representation.

### 4.2 Out of Scope

- Rate persistence, staleness marking, snapshotting, per-tenant fan-out — ledger-owned.
- Currency translation, triangulation / pair inversion — ledger-owned (DESIGN O-3).
- Pricing-side FX and rate-lock governance — Catalog module.
- Provider commercial contracts and credential procurement — ops.
- Manual / break-glass rate ingest — the ledger's own seed endpoint.

## 5. Functional Requirements

### 5.1 Rate Feed

#### Live rate feed via the ledger contract

- [ ] `p1` - **ID**: `cpt-cf-bss-rate-provider-fr-live-rate-feed`

The gear **MUST** supply the latest published FX rates to the Billing Ledger through the
ledger-owned `RateProviderV1` contract, returning the whole published table when no
specific pairs are requested.

- **Rationale**: The ledger requirement `cpt-cf-bss-ledger-fr-multi-currency-fx`
  (ledger PRD § Multi-currency & FX) needs a live feed; this gear is that feed.
- **Actors**: `cpt-cf-bss-rate-provider-actor-ledger-rate-sync`

#### Provider publication time

- [ ] `p1` - **ID**: `cpt-cf-bss-rate-provider-fr-provider-publication-time`

Every returned rate **MUST** carry the provider's original publication timestamp (UTC) —
never the fetch time. On non-publication days the last published rate is returned with its
original timestamp unchanged.

A publication timestamp more than **26 hours ahead** of the gear's own clock **MUST** be
rejected: the whole document fails with an error, so the composite falls through to the next
source and nothing is stored. Validation is the **source plugin's**, performed at parse time
before any rate is built. Timestamps in the *past* are never rejected here — age is the
ledger's staleness policy, and duplicating it in the adapter would put the same rule in two
layers.

- **Rationale**: The ledger's staleness policy
  (`cpt-cf-bss-ledger-fr-fx-rate-source-failure`) is only meaningful against true
  publication time; stamping fetch time would mask stale feeds. That policy measures *age*,
  which only bounds the past: a future-dated document has a negative age and therefore reads
  as permanently fresh, so a feed publishing a timestamp a month out would keep the ledger
  posting at a frozen rate with no staleness alarm ever firing. The ledger cannot catch this
  itself — by then the value is just a stored row, indistinguishable from "we have not synced
  recently"; only the plugin knows it came straight off the wire this tick. The 26 h bound is
  14 h (the widest civil timezone offset, so a date-only feed anchored at 00:00 UTC is never
  rejected) plus 12 h of host clock drift.
- **Actors**: `cpt-cf-bss-rate-provider-actor-finance-auditor`

#### Direct pairs only

- [ ] `p1` - **ID**: `cpt-cf-bss-rate-provider-fr-direct-pairs-only`

The gear **MUST** emit only pairs the serving source natively publishes. A pair the source
cannot serve is omitted — never synthesized, inverted, or merged from another source.
Cross-base derivation is the ledger's triangulation concern.

- **Rationale**: Keeps every synced document single-source-coherent for audit and keeps
  rate-math ownership in one place (DESIGN O-1 / O-3).
- **Actors**: `cpt-cf-bss-rate-provider-actor-finance-auditor`

### 5.2 Sources & Fallback

#### Ordered source fallback

- [ ] `p1` - **ID**: `cpt-cf-bss-rate-provider-fr-ordered-source-fallback`

The gear **MUST** try configured sources in their configured order and return the first
whole successful rate document. Only when **all** sources fail may it report failure to
the caller.

"Whole" is a **provenance** rule — the document served comes from one source, entire, never
stitched together across sources. It is **not** a coverage rule: a source that publishes
fewer pairs than the ledger would like, or whose document had individual invalid entries
skipped during parse, has **succeeded**, and the gear **MUST NOT** fall through to the next
source on that basis. Fallback is triggered only by a source-level failure (transport,
upstream status, whole-document parse failure, or a document from which nothing usable
survives). Per-pair fallback is out of scope for v1 — see DESIGN O-13.

- **Rationale**: Realizes the fallback algorithm the ledger design expects at fetch
  time ([ledger FX design § rate-source fallback](../../ledger/docs/design/06-fx-multicurrency.md)),
  where the ledger cannot do it itself. Coverage is not gated here because any provider error
  makes the ledger's `RateSyncJob` refresh **nothing**, so gating on completeness would trade
  a few stale pairs for all of them; a pair that stops being refreshed is instead surfaced by
  the ledger's own staleness window (`FX_RATE_UNAVAILABLE`).
- **Actors**: `cpt-cf-bss-rate-provider-actor-ledger-rate-sync`

#### True-source provenance

- [ ] `p1` - **ID**: `cpt-cf-bss-rate-provider-fr-source-provenance`

The gear **MUST** report the identity of the source that actually served each rate, so
ledger rate rows and snapshots record the true upstream — never a generic adapter name.

- **Rationale**: Auditors must be able to answer "whose rate was applied here".
- **Actors**: `cpt-cf-bss-rate-provider-actor-finance-auditor`
- **As implemented:** provenance is carried **per rate** (a `provider` field on each returned
  rate), not reported through a separate "who served last" call. The latter was only correct
  while a single caller strictly alternated fetch → ask-who-served; the adapter is registered
  process-wide, so a second consumer fetching in between would have made the ledger stamp
  the wrong source onto financial records. See DESIGN.md O-7a.

#### Config-driven source onboarding

- [ ] `p2` - **ID**: `cpt-cf-bss-rate-provider-fr-config-onboarding`

Adding, removing, or reordering rate sources **MUST** be a configuration change. A plain
REST JSON rate feed **MUST** be onboardable with no code change.

- **Rationale**: Provider procurement is an ops decision (DESIGN O-10); billing must not
  need a release to switch or add a feed.
- **Actors**: `cpt-cf-bss-rate-provider-actor-platform-operator`
- **As implemented:** "configuration change" means deploying/configuring a source-plugin
  gear with a `vendor` + `priority`, not editing one shared list — see DESIGN.md §3.2.

#### Strict configuration validation

- [ ] `p2` - **ID**: `cpt-cf-bss-rate-provider-fr-strict-config-validation`

Invalid *per-plugin* configuration — e.g. the http-json plugin's `mapping` missing, a
non-`https` `base_url`, or an unusable authentication setup — **MUST** fail that plugin gear's
own startup loudly, not surface at first fetch.

- **Rationale**: A misconfigured rate feed discovered at fetch time silently degrades
  billing; startup is the cheapest place to fail for what a single gear's own `init()` can
  check.
- **Actors**: `cpt-cf-bss-rate-provider-actor-platform-operator`
- **As implemented — authentication is checked even earlier than startup:** the credential is
  carried by the `auth` variant that needs it, so "a kind that needs a key, with no key" and
  "a key that would never be sent" are **config-load (deserialization) errors** rather than
  runtime checks in `init()`. Invalid states are unrepresentable instead of validated.
- **As implemented — narrower than the original wording:** there is no single "source
  list" or "source kind" anymore (see DESIGN.md's Implementation-revision note and §3.2),
  so **an unknown source kind and an empty source list are no longer things to validate at
  all** — a `RateProviderV1` source is a plugin crate, not a config value, so there's no
  "unknown kind" to reject. Zero matching source plugins for the configured vendor is now a
  **first-fetch runtime error**, not a startup failure (§3.2's "Empty result is a runtime
  error" — deliberately, so plugin registration order never has to be enforced). A
  `vendor` mismatch between the core gear and a source plugin is not rejected anywhere
  either — it is filtered out, since one registry can legitimately hold source plugins
  belonging to another composite. Because that exclusion is otherwise invisible and a
  `vendor` typo passes every startup check, it must stay **observable**: each excluded
  instance is logged at `debug` (not `warn` — discovery re-runs every tick, so a per-tick
  warning per foreign instance would be noise in a healthy deployment), and if the filter
  empties the whole set the resulting first-fetch error names both the expected vendor and
  the vendors actually present. A *matching*-vendor instance whose scoped client can't be
  resolved is logged as a warning instead — the two cases are handled differently. Only
  the http-json plugin's own two config checks above are still startup-time failures
  today.

## 6. Non-Functional Requirements

Project-wide NFR baselines follow the repository [`guidelines/`](../../../../guidelines/)
and the parent [ledger PRD](../../ledger/docs/PRD.md) §7; gear-specific NFRs below.

### 6.1 Gear-Specific NFRs

#### Off the posting path

- [ ] `p1` - **ID**: `cpt-cf-bss-rate-provider-nfr-off-posting-path`

A provider outage **MUST NOT** affect posting latency or availability *synchronously*:
the gear is consumed only by the ledger's background sync job, never on the posting
path. The guarantee is about synchronous impact only — an outage that outlasts the
ledger's staleness thresholds degrades FX posts (and only FX posts) through the
ledger's own fail-safe, `FX_RATE_UNAVAILABLE` (§9's all-sources-down criterion states
the exact blocking condition and the thresholds that set the tolerated window).

- **Threshold**: Zero posting-path invocations; provider downtime affects FX posts only
  through the ledger's staleness fail-safe (block, not guess) and never non-FX posting.
- **Rationale**: Hard isolation requirement inherited from the ledger's post-path NFRs.

#### Fail-safe by absence

- [ ] `p1` - **ID**: `cpt-cf-bss-rate-provider-nfr-fail-safe-absence`

When no source can serve, the gear **MUST** return an error — never fabricated data, never a
document stitched together from several sources, never silently stale data — so the ledger
blocks FX posts and alarms instead of posting a wrong rate.

This is about what may be returned **instead of an error**; it does not make partial coverage
a failure. A source that serves fewer pairs than requested, or whose parse skipped individual
invalid entries, has succeeded (`cpt-cf-bss-rate-provider-fr-ordered-source-fallback`,
DESIGN O-13).

- **Threshold**: 100% of all-sources-failed fetches surface as errors to the caller.
- **Rationale**: `cpt-cf-bss-ledger-fr-fx-rate-source-failure` forbids silent fallback;
  the adapter must not undermine it upstream.

#### Deterministic rate conversion

- [ ] `p1` - **ID**: `cpt-cf-bss-rate-provider-nfr-deterministic-conversion`

Converting a published decimal rate into the contract's fixed-precision integer form
**MUST** be deterministic: the same published value always yields the same integer, using
exact-decimal arithmetic with banker's rounding (half-to-even) and explicit overflow
errors. A converted rate **MUST** be strictly positive.

- **Threshold**: Golden-vector equality on repeated conversion, including exact half-way
  decimals; overflow / non-numeric input always errors, never truncates; a value that
  rounds to zero or below always errors, never reaches a rate store.
- **Rationale**: Inherits the ledger's platform rounding default
  (`cpt-cf-bss-ledger-fr-money-rounding-scale`), so a converted rate and the amount posted
  from it round identically; auditors must be able to reproduce rates.

**Rounding is not this gear's decision to make.** Half-to-even is the platform default
fixed by the ledger (`cpt-cf-bss-ledger-fr-money-rounding-scale`, `p1`), which requires it
identically across S1–S6 and exports. This adapter inherits it so converted rates match
posted amounts; deviating would break that requirement, not merely differ from it.
Finance / audit sign-off is therefore **not a release gate for this gear** — it is tracked
against the ledger's platform decision. The only event that would revise the strategy here
is a change to that ledger requirement, which this adapter would then follow.
  The positivity rule is a product decision confirmed with the BSS billing owner
  (2026-07-28): there are no zero or negative FX rates in this domain, so such a value can
  only be corrupt feed data — and it would zero out or flip the sign of every translation
  derived from it.

#### Fetch latency

- [ ] `p2` - **ID**: `cpt-cf-bss-rate-provider-nfr-fetch-latency`

A fetch against one source **MUST** complete one bounded attempt within its configured
timeout; a successful fetch completes fast enough that feed freshness holds within one
ledger sync tick.

- **Threshold**: p95 ≤ 2 s per source (draft; confirm against ECB response times);
  worst-case composite duration bounded by the sum of configured per-source timeouts.
- **Rationale**: Background job budget; G10 pairs must not cross the ledger's 24 h
  staleness window under normal operation.
- **Architecture Allocation**: See DESIGN.md § NFR Allocation.

### 6.2 NFR Exclusions

- **Horizontal scalability**: not applicable — one lightweight fetch per ledger sync tick;
  no request fan-in.
- **Data durability**: not applicable — the gear is stateless; durability is the ledger's.

## 7. Public Library Interfaces

### 7.1 Public API Surface

#### `RateProviderV1` implementation

- [ ] `p1` - **ID**: `cpt-cf-bss-rate-provider-interface-rate-provider-v1`

- **Type**: Rust trait implementation (`bss-ledger-sdk` `RateProviderV1`,
  GTS `gts.cf.bss.ledger.rate-provider.v1`), registered in the platform `ClientHub`.
- **Stability**: stable
- **Description**: The gear's only consumable surface — latest-rates fetch and health
  probe. Provider identity is not a separate surface: it arrives as per-rate provenance
  (`ProviderRate.provider`, the serving source's id) on every returned rate, while
  `provider_id()` is only this gear's constant configured id — it never says who served
  a fetch. The health probe is **reachability only**: it reports the
  endpoint answered, which any HTTP status proves, and says nothing about whether a usable
  document could be fetched. Feed freshness is a separate signal and MUST NOT be inferred
  from it (see DESIGN § "Interfaces & contracts").
- **Breaking Change Policy**: The contract is owned by the ledger SDK; this gear never
  changes it unilaterally.

### 7.2 External Integration Contracts

#### Provider feed contract

- [ ] `p2` - **ID**: `cpt-cf-bss-rate-provider-contract-provider-feed`

- **Direction**: required from external rate providers.
- **Protocol/Format**: HTTPS GET returning a parseable rate document (ECB daily XML, or
  JSON for config-mapped REST feeds) that includes a publication timestamp.
- **Compatibility**: Feed format changes are absorbed in this gear (parser / mapping
  config); consumers are insulated by the `RateProviderV1` contract.

## 8. Use Cases

### Fallback serve with provenance

- [ ] `p2` - **ID**: `cpt-cf-bss-rate-provider-usecase-fallback-serve`

**Actor**: `cpt-cf-bss-rate-provider-actor-ledger-rate-sync`

**Preconditions**:
- Two sources configured in order (primary, fallback); primary is unreachable.

**Main Flow**:
1. The ledger sync job requests the latest rates.
2. The gear tries the primary source; the attempt fails within its timeout.
3. The gear tries the fallback source and receives a whole rate document.
4. The gear returns that document; every rate in it already carries the fallback source's
   id as its `provider`, stamped by that source.
5. The ledger stores each rate under the `provider` the rate itself carries.

**Postconditions**:
- Ledger rate rows record the fallback provider and the provider's publication time.

**Alternative Flows**:
- **All sources fail**: the gear returns the last error; the ledger alarms and FX posts
  block (fail-safe by absence).

### Onboard a new REST feed by configuration

- [ ] `p2` - **ID**: `cpt-cf-bss-rate-provider-usecase-onboard-feed`

**Actor**: `cpt-cf-bss-rate-provider-actor-platform-operator`

**Preconditions**:
- ECB is configured and serving; the ledger's `RateSyncJob` is ticking.
- The new feed is a plain GET-JSON endpoint reachable over https.

**Main Flow**:
1. The operator adds an `bss-rate-provider-http-json-plugin` config block: `base_url`,
   `mapping`, `auth`, the core gear's `vendor`, and a `priority` that places the feed in
   the intended fallback position.
2. The plugin gear starts and registers its `RateProviderSourcePluginSpecV1` instance plus
   its scoped client.
3. On its next tick the core gear's discovery lists the instance, keeps it (vendor match),
   and orders it by `priority`.
4. The composite serves from it whenever the sources ahead of it fail.
5. Rates it serves reach the ledger's store stamped with that source's `id`.

**Postconditions**:
- The feed participates in the fallback chain at its configured position.
- No consuming module was rebuilt, reconfigured, or restarted — not the core gear, not the
  ledger. Discovery re-runs per tick and caches nothing, which is what makes this true.

**Alternative Flows**:
- **Registration lands mid-tick**: the tick that saw no instance yet fails; the next tick
  picks the feed up. No restart is needed to recover.

## 9. Acceptance Criteria

- [ ] With the adapter deployed and ECB configured, one ledger sync tick populates the
  ledger's rate store and an invoice post on a directly published pair (EUR-functional
  tenant) locks a rate (no `FX_RATE_UNAVAILABLE`). Posting for a non-EUR functional
  currency is out of scope until the ledger triangulation/inversion companion change
  lands — it is not a criterion this gear can satisfy alone.
- [ ] With all sources down, the sync tick fails, the feed-freshness alarm fires, and
  non-FX posting is unaffected. FX posting is **not** immediately blocked: this gear is
  off the posting path, and the ledger locks rates from its own store. A post blocks with
  `FX_RATE_UNAVAILABLE` only once that store has no rate for the pair, or the rate it has
  has crossed the ledger's staleness threshold (`stale_g10_hours` / `stale_default_max_days`).
  So the outage window a deployment tolerates is set by the ledger's staleness rule, not by
  this gear.
- [ ] After a primary-source outage with a healthy fallback, synced rows record the
  fallback provider's identity.
- [ ] Re-fetching an identical published document yields byte-identical integer rates.
- [ ] A new plain REST feed is onboarded by configuration alone: an http-json plugin block
  with a `base_url`, `mapping`, a `priority`, and a `vendor` equal to the core gear's
  `source_vendor` (without that match, discovery never selects the plugin) puts the feed
  into the fallback chain at that position — plus the matching `auth` kind carrying its
  `api_key` when the feed requires a credential (a credential-bearing feed is onboarded
  only once the dump-redaction dependency in §10 is met — see the §12 risk). Its rates
  reach the ledger's store stamped with its own `id`, and no
  consuming module is rebuilt, reconfigured or restarted to make that happen — neither the
  core gear nor the ledger. This is the success path behind the "cheap provider onboarding"
  goal (§1.3); the criterion below only establishes that *mis*configuration fails loudly,
  which is not the same claim. It rests on discovery re-running every tick and caching
  nothing, so a change here breaks the goal.
- [ ] An http-json plugin configured without a `mapping` fails that plugin's own gear
  startup with a clear error; an `auth` block naming a credential-bearing kind without a
  key (or attaching a key to the no-auth kind) fails even earlier, at config load.
- [ ] Ledger rate rows record the source that actually served each rate, and stay correct
  even when a second consumer fetches from the same adapter concurrently.
- [ ] A currency code the feed publishes that is not an ISO-4217-shaped identifier is
  dropped and logged, never written into a tenant's rate store.
- [ ] A feed serving a document dated more than 26 hours ahead of the gear's clock fails
  that source's fetch with an error naming the offending timestamp; nothing from it is
  stored, and the composite falls through to the next source. A document dated up to a full
  day ahead — the legitimate case for a date-only feed anchored at 00:00 UTC — still serves.
  Both bounds are asserted, because a test for the far-future case alone would pass with the
  window set arbitrarily narrow.
- [ ] With zero source plugins registered for the core gear's configured `source_vendor`,
  the first `RateSyncJob` tick's `fetch_latest` errors — never a startup crash — and a
  source plugin registering by a later tick self-heals it. There is no "unknown
  kind"/"empty list"/"precedence mismatch" startup check anymore; those concepts don't
  exist in the current design (see DESIGN.md's Implementation-revision note).

## 10. Dependencies

| Dependency | Description | Criticality |
|------------|-------------|-------------|
| `bss-ledger-sdk` | Owns the `RateProviderV1` contract and its types | p1 |
| `types-registry` / `types-registry-sdk` | Plugin discovery + registration substrate every crate here uses (as implemented — plugin pattern, see DESIGN.md) | p1 |
| Billing Ledger (`RateSyncJob`) | Sole consumer; pulls and persists the rates | p1 |
| ECB reference-rate feed | Primary external source (free daily publication) | p1 |
| Ledger triangulation companion change | Required for non-EUR-functional tenants (DESIGN §4) | p1 |
| Dump-side credential redaction in `toolkit::bootstrap::config::dump` | Prerequisite for **enabling credential-bearing (authenticated http-json) feeds**: the dump serializes pre-deserialization values, so until it redacts credential-shaped fields, no feed whose config carries an `api_key` is onboarded (§12). Owned by the platform / `toolkit` maintainers. Does not gate v1 as shipped (ECB only, no credential) | p1 |
| Bank / PSP rate feed | Future fallback source; procurement owned by ops | p3 |

## 11. Assumptions

- The ECB daily reference-rate feed remains publicly available at no cost.
- Source configuration is operator-supplied and platform-trusted (not tenant input).
- No assumption is made about how many callers share the composite. It is registered
  process-wide in `ClientHub`, so a second consumer may fetch between any two calls; that
  is precisely why provenance travels on each rate rather than being read back through a
  later `provider_id()` call.

## 12. Risks

| Risk | Impact | Mitigation |
|------|--------|------------|
| ECB feed outage with no fallback configured (v1 is ECB-only) | FX posts block until feed recovers | Fail-safe by absence + ledger alarm; fallback mechanism is config-ready (deploy/configure another source-plugin gear) |
| EUR-based pairs only until ledger triangulation/inversion lands | Non-EUR-functional tenants cannot post FX | Sequencing tracked as a hard companion dependency (DESIGN §4) |
| Provider format drift breaks parsing | Fetch fails, feed goes stale | Whole-document parse failure surfaces as an error → fallback / alarm, never a fabricated or cross-source-stitched document. Drift that invalidates only *some* entries leaves the rest serving (by design — DESIGN O-13); those pairs stop being refreshed and age out into the ledger's `FX_RATE_UNAVAILABLE` |
| A raw effective-config dump discloses a source's API key | A routine diagnostic leaks a provider credential into logs or a support bundle | `SecretString` redacts the *typed* config, but `toolkit::bootstrap::config::dump` serializes the pre-deserialization JSON, so it is not covered. The documented path keeps the raw key out of the file (`api_key: "${VAR}"`, expanded inside the gear at `init()` via `config_expanded` — implemented and tested), but that path is a convention, not an enforced guarantee: a key hardcoded against it is disclosed verbatim by a routine dump. Dump-side redaction of credential-shaped fields is therefore a **p1 prerequisite for enabling credential-bearing feeds** (tracked in §10), owed by the **platform / `toolkit` owners** — the fix has to sit in the dump itself (see DESIGN §2.2 "Secrets handling"). v1 as shipped is unaffected: the only configured source is ECB, a public feed with no credential. |

## 13. Open Questions

- Concrete bank / PSP fallback feed and credentials — ops procurement (DESIGN O-10).

## 14. Traceability

- **Design**: [DESIGN.md](./DESIGN.md) — self-contained technical design for this gear.
- **Upstream PRD**: [`../../ledger/docs/PRD.md`](../../ledger/docs/PRD.md) — § Money,
  Rounding & Foreign Exchange (`cpt-cf-bss-ledger-fr-multi-currency-fx`,
  `cpt-cf-bss-ledger-fr-fx-rate-source-failure`,
  `cpt-cf-bss-ledger-fr-money-rounding-scale`).
- **Consuming design**:
  [`../../ledger/docs/design/06-fx-multicurrency.md`](../../ledger/docs/design/06-fx-multicurrency.md)
  — the fetch-time rate-source-fallback algorithm this gear realizes.
