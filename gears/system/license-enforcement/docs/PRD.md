<!-- cpt:
version: 0.1.0
status: draft
module: license-enforcement
system: cf
-->

# PRD — License Enforcement

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
- [4. Scope](#4-scope)
  - [4.1 In Scope](#41-in-scope)
  - [4.2 Out of Scope](#42-out-of-scope)
- [5. Functional Requirements](#5-functional-requirements)
  - [5.1 License Check](#51-license-check)
  - [5.2 Resource and Subject Types](#52-resource-and-subject-types)
  - [5.3 Grants](#53-grants)
  - [5.4 License Packs](#54-license-packs)
  - [5.5 Management API](#55-management-api)
  - [5.6 Signed Grants and Untrusted Builds](#56-signed-grants-and-untrusted-builds)
  - [5.7 Observability](#57-observability)
- [6. Non-Functional Requirements](#6-non-functional-requirements)
  - [6.1 Module-Specific NFRs](#61-module-specific-nfrs)
  - [6.2 NFR Exclusions](#62-nfr-exclusions)
- [7. Public Library Interfaces](#7-public-library-interfaces)
  - [7.1 Public API Surface](#71-public-api-surface)
  - [7.2 External Integration Contracts](#72-external-integration-contracts)
- [8. Use Cases](#8-use-cases)
  - [Check a Named-User License](#check-a-named-user-license)
  - [Delegate Subject Grants Down a Tenant Chain](#delegate-subject-grants-down-a-tenant-chain)
  - [Issue a Pack on Purchase](#issue-a-pack-on-purchase)
  - [Issue a Signed Pack to an On-Premise Tenant](#issue-a-signed-pack-to-an-on-premise-tenant)
- [9. Acceptance Criteria](#9-acceptance-criteria)
- [10. Dependencies](#10-dependencies)
- [11. Assumptions](#11-assumptions)
- [12. Risks](#12-risks)
- [13. Open Questions](#13-open-questions)
- [14. Traceability](#14-traceability)

<!-- /toc -->

## 1. Overview

### 1.1 Purpose

License Enforcement is the platform's licensing service: the system of record for what has been licensed to whom. It
stores grants and manages their lifecycle (accept, bind, delegate, return, recall, suspend, revoke, renew), and it
lets the platform vendor sell, distribute, and withdraw licenses without code changes. Grants are tenant-scoped but
license any registered subject type: a whole tenant (every user in it may use the capability), N named subjects of a
type (user seats, agents, workstations) that the tenant names and may hand on to its sub-tenants, or one specific
subject. A subject grant licenses one subject and has one binding at a time, so N of them never license more than N
subjects across the whole tenant tree. Licenses are sold as packs, and every change is attributable in an audit
ledger.

On the check side, License Enforcement is a backend plugin of `license-resolver`: it implements
`LicenseResolverPluginClient::is_licensed` and is discovered through the resolver's GTS plugin spec. The resolver
validates every request against the registered licensing contracts and delegates it; this module evaluates the
tenant's grants and returns `granted` with a typed reason.

What evidence suffices for a grant is the admission plugin's decision, and the plugin is chosen by whoever assembles
the service: one that trusts authorized callers, so the vendor's own deployment creates grants straight through the
management API and its store is the authority; one that admits only vendor-signed artifacts, so a deployment the
vendor does not control verifies, holds, distributes, and narrows grants but can never mint or widen one; or one of
the assembler's own. Every grant traces to evidence a plugin admitted; the module never decides that on its own.
Which plugin a build links, and which trust anchors it verifies against, is settled when the service is assembled,
never by runtime configuration. The plugin owns both the evidence and the intake: grant parameters from an authorized
caller, or a signed artifact in a format it alone parses.

### 1.2 Background / Problem Statement

License resolver fixes the check contract — `is_licensed(LicenseCheckRequest) -> LicenseDecision`, tenant-scoped,
fail-closed, GTS-typed subject and resource contracts — but owns no grants and delegates every check to a backend
plugin. No such backend exists in CF/Gears today, so no module can actually be licensed: there is nowhere to issue a
grant, nothing to answer the check, and no way to revoke.

The mature products in this space — the classic on-premise license managers (Thales Sentinel, FlexNet, CodeMeter),
API-first licensing SaaS (Keygen), cloud licensing (AWS License Manager), and vendors who license their own
on-premise product (GitLab, Sourcegraph) — share one shape: issuance and enforcement in separate trust domains; a
feature registry; grants with explicit lifecycle states and verbs; a license inventory drawn down by binding and
delegation against an auditable record; signed artifacts with key ids, freshness metadata, and clock-rollback defenses
for hostile deployments; and short-TTL renewal as the practical revocation mechanism where connectivity is
intermittent. The neighbouring concerns are already owned elsewhere on this platform — counting, leases, and overage
by `quota-enforcement`, authorization by `authz-resolver`, billing by the commerce layer — so this module adds grants
and nothing else (§4.2).

Two properties of the problem put the model beyond a plain grant table. The licensed subject is polymorphic
and distinct from the tenant that holds the grant: a capability may be licensed to the whole tenant, to N subjects of
a type (user seats today, agents or other subject types later) that the tenant names and may hand on to a sub-tenant,
or to one specific subject. And an on-premise customer must not be able to create or widen a grant by editing a
database or a config file, which puts vendor-signed grants — and an admission path the operator cannot replace — in
the model itself rather than in deployment guidance.

### 1.3 Goals (Business Outcomes)

Measured at first production release unless stated otherwise:

- Every `is_licensed` check on the platform is answered from a governed grant store, with a typed denial reason, and
  every failure denies: a store, registry, PDP, or trust-material fault costs a capability, never leaks one.
- One module, any admission: the service assembles behind the admission plugin its assembler chooses — one that trusts
  authorized callers, one that admits only vendor-signed artifacts, or one of their own — with the same domain model,
  lifecycle, check semantics, and management semantics under all of them, differing only in how root grants arrive,
  and no code linked that the chosen plugin does not need.
- The platform vendor issues, suspends, revokes, and renews licenses through the management API without code changes
  or redeployment.
- Every grant, binding, and delegation is attributable to an actor in the audit ledger.
- An untrusted build accepts 0 unsigned, tampered, or unknown-key grants, and offers no way to turn verification
  off.

### 1.4 Glossary

| Term | Definition |
|------|------------|
| Resource | The licensable thing, identified as in `license-resolver`: a registered Resource contract type plus an optional instance id (a well-known name or a UUID). Without the id a grant covers every instance of the type; with it, one instance. Nothing is pre-declared per resource. |
| Holder | The tenant in whose scope a grant lives and that distributes it: accepts, binds, delegates, returns, recalls. Distinct from the check subject. |
| Root grant | A grant with no parent. In a build that admits artifacts, the only kind an admission plugin creates; every sub-grant descends from one. |
| Evidence | What an admission plugin admitted a set of root grants from: an authorized caller's parameters or a vendor-signed artifact. Fixes which grants exist because of it. |

## 2. Actors

### 2.1 Human Actors

#### Platform Vendor

**ID**: `cpt-cf-license-enforcement-actor-platform-vendor`

- **Role**: Composes the platform and sells licenses. Issues grants to tenants, singly or as Packs, suspends,
  reinstates, revokes, renews, and replaces them, and operates the key and trust-bundle lifecycle in the issuer.
- **Needs**: Issue and revoke without code changes; see who holds what; trust that on-premise customers cannot forge
  grants.

#### Tenant Administrator

**ID**: `cpt-cf-license-enforcement-actor-tenant-admin`

- **Role**: Administers the grants their tenant holds: accepts or rejects incoming grants and delegations, binds
  subject grants to subjects within the tenant (users, agents, or any admitted type), delegates them to sub-tenants,
  and recalls them. Cannot create grants from nothing.
- **Needs**: Distribute purchased licenses inside the tenant hierarchy and see what is still unbound at a glance.

### 2.2 System Actors

#### License Resolver

**ID**: `cpt-cf-license-enforcement-actor-license-resolver`

- **Role**: The gateway that validates a `LicenseCheckRequest` against registered licensing contracts and delegates it
  to this module via `LicenseResolverPluginClient`. The only caller of the check path.
- **Interface and direction**: inbound, in-process through `LicenseResolverPluginClient`; this module never calls the
  resolver back.
- **When unavailable**: no checks arrive; there is nothing to degrade.

#### License Management

**ID**: `cpt-cf-license-enforcement-actor-license-management`

- **Role**: The administrative surface over licensing: shows a tenant what it holds and what is unbound, answers who may
  delegate what to whom before an operation is attempted, and turns grants and sub-grants into something an
  administrator can act on.
- **Interface and direction**: inbound — a client of the management API, its check-explain read, and the ledger; it
  holds no licensing state of its own.
- **When unavailable**: administration stops; grants, checks, and bindings are unaffected.

#### Subscriptions

**ID**: `cpt-cf-license-enforcement-actor-subscriptions`

- **Role**: The BSS gear that owns the commercial lifecycle and brings grants with it: a purchase, renewal, plan change,
  or cancellation issues, renews, or revokes the grants that back it, as Packs or as individual grants, supplying the
  pack or grant parameters, the tenant, and an idempotency token derived from the commercial event. The concrete
  commercial entry point today; other BSS gears may issue the same way.
- **Interface and direction**: inbound, in-process through the management SDK client, authorized like any other caller.
- **When unavailable**: no new commercial grants arrive; grants already issued keep their lifecycle and checks are
  unaffected.

## 3. Operational Concept & Environment

Runtime, lifecycle, and integration patterns are inherited from
[docs/ARCHITECTURE_MANIFEST.md](../../../../docs/ARCHITECTURE_MANIFEST.md) and [guidelines/](../../../../guidelines/).
The check boundary and the tenant-bounded model are fixed by the
[license-resolver PRD](../../license-resolver/docs/PRD.md). Gear-specific constraints:

- **One model, many admissions.** What a deployment may do follows from the admission plugin it links: grant
  parameters from an authorized caller, a vendor-signed artifact in the envelope format, or a carrier added later.
  Domain model, lifecycle, check semantics, and management semantics are identical under all of them; only how root
  grants arrive differs, and that is the plugin's. The security-relevant distinction is a property of the plugin, not
  a count of builds: *untrusted* is any build whose plugin admits nothing without a vendor signature — the case for a
  host whose operator has root over host, database, and clock; *trusted* is any build that accepts unsigned evidence
  and takes its own store as the authority.
- **Threat model of the untrusted build.** The adversary is the operator, with root over host, database, and clock.
  The vendor holds two things: the signature on root grants — resource, holder, subject scope, validity, conditions,
  how deep a subject grant may be bound or handed on — and the condition evaluator compiled into the binary. The
  operator holds everything else: bindings and delegations, the tenant hierarchy, and the subject and resource
  `metadata` a check arrives with. Editing those redistributes licenses but never multiplies them (two live bindings
  deny as a whole), and feeds conditions whatever input the operator chooses. Patching the binary, hiding rows from
  the instance's own queries, DRM-grade obfuscation, and hardware attestation are out of the threat model.
- **Trust is compiled in, and split.** Signing lives outside this module — no build of it holds a private key, only
  the public material it verifies against. Verification lives inside it: the admission plugin and its trust anchors are
  linked into the enforcement binary, because a check the operator can replace or intercept proves nothing. Both sides
  of that split are settled by whoever assembles the service; a deployment has no switch that changes either.
- **Condition evaluation is compiled in.** A grant may carry conditions over subject and resource `metadata`, but the
  vocabulary is closed and its evaluator is linked at build time — never selected at runtime, never authored by the
  deployment. In an untrusted build the conditions themselves live inside the signed payload; an evaluator or a rule
  the operator can edit is a grant the operator can widen.
- **Tenant-bounded grants.** Every grant belongs to exactly one tenant scope (its holder). Delegation creates a new
  grant in the recipient tenant's scope linked to its parent; it never makes one grant span tenants.

## 4. Scope

### 4.1 In Scope

- Answering the `license-resolver` check as its backend plugin: evaluating a tenant's grants for the requested
  subject and resource, with typed denial reasons.
- Grants on any registered Resource contract type, at type level or for one instance, with no per-resource declaration.
- Grants with explicit lifecycle states and verbs, validity windows, and conditions over contract `metadata` — a
  closed constraint vocabulary and the compiled-in evaluator that runs it.
- Tenant-scoped grants for any registered subject type: a whole tenant, or one subject named by the vendor or bound
  later by the holder (user seats in phase 1; agents or other types later).
- Binding of subject grants by the holder and their delegation to sub-tenants, with accept / reject / recall / return.
- License packs: sets of grants issued and managed as one.
- Managing those grants: a REST API and an in-process SDK client for platform modules, and an append-only audit
  ledger.
- Admission plugin contract; trust-bundle rotation, freshness (TTL + renewal), revocation lists, and clock-rollback
  defenses for untrusted builds.

### 4.2 Out of Scope

- **Counting, leases, consumption, overage, metering, and numeric limits** — `quota-enforcement`. The check answers
  yes or no, never how many; if a grant ever needs to provision a ceiling into `quota-enforcement`, that is an
  additive grant parameter and a later change to this PRD.
- **Authorization decisions** — `authz-resolver`. The management API is a PDP consumer, never a PDP; the check answers
  "is it licensed", not "is it permitted".
- **Billing, payments, subscriptions, plan-change workflows** — the commerce layer; licensing consumes the outcome
  of a sale.
- **Signing of grant artifacts, key generation, private-key custody, root-key ceremonies, trust-bundle publishing** —
  the issuer, a separate vendor-side component with its own PRD. Hardware-backed custody (HSM, dongle) is its
  concern; the artifact format must not preclude it.
- **Hardware fingerprinting, clone detection, DRM-grade anti-tamper, real-time revocation networks (CRL/OCSP)**.
- **Tamper-evident (cryptographically chained) ledger.**
- **Usage reporting to the vendor (true-up export).**
- **Feature flagging / rollout** — flags answer "is it rolled out", grants answer "is it paid for".
- **Delegation across License Enforcement instances.** A sub-grant is usable only where its whole parent grant chain
  is stored; a sub-tenant that runs its own instance receives vendor-issued grants. Offline cryptographic re-granting
  from a parent artifact without an instance is out of scope for the same reason.

## 5. Functional Requirements

> **Testing strategy**: All requirements verified via automated tests (unit, integration, e2e) targeting 90%+ code
> coverage unless otherwise specified.

### 5.1 License Check

#### Resolver Backend Check

- [ ] `p1` - **ID**: `cpt-cf-license-enforcement-fr-backend-check`

The system **MUST** implement `LicenseResolverPluginClient::is_licensed` and answer `granted: true` if and only if at
least one grant in the request's tenant scope (a) is usable per `cpt-cf-license-enforcement-fr-grant-lifecycle`, and
so is every grant in its parent grant chain up to the root, (b) targets a resource that covers the requested one — a
grant on a resource type covers every instance of that type; a grant with an instance id covers only that instance,
(c) has a subject scope that covers the requested subject per `cpt-cf-license-enforcement-fr-subject-scope`, (d) is
within its validity window at evaluation time, (e) has all its conditions satisfied by the request `metadata`, and (f)
if it is a sub-grant, is a subset of its parent at every link of the chain per
`cpt-cf-license-enforcement-fr-delegation`. Evaluation **MUST** be deterministic and follow one documented order; the
check path **MUST NOT** mutate state.

- **Rationale**: This is the module's reason to exist; determinism and a documented order make denials explainable and
  identical across code paths.
- **Actors**: `cpt-cf-license-enforcement-actor-license-resolver`

#### Subject Scope Semantics

- [ ] `p1` - **ID**: `cpt-cf-license-enforcement-fr-subject-scope`

A grant's subject scope **MUST** be one of: **subject** — one subject of an admitted contract type, whose id is either
fixed at admission or bound later by the holder (`cpt-cf-license-enforcement-fr-subject-binding`), matching only a
check for that exact subject and matching nothing while unbound; **tenant-wide** — every subject of the types the
Resource contract admits within the holder tenant, optionally restricted to a listed subset of those types, matching
any check whose subject type is one of them or derives from one. A grant with subject scope is a **subject grant**.
Every subject type named in a scope **MUST** be in the `admitted_subjects` of the grant's Resource contract
(`cpt-cf-license-enforcement-fr-type-references`). A check whose subject is not covered **MUST** deny, telling "the
tenant holds usable subject grants for the resource, none bound to this subject" apart from "no grant covers the
resource".

- **Rationale**: Site-wide licenses and named-subject licenses are the two commercial shapes; one grant per subject
  makes the count of licensed subjects a count of grants rather than a number to protect.
- **Actors**: `cpt-cf-license-enforcement-actor-license-resolver`, `cpt-cf-license-enforcement-actor-tenant-admin`

#### Typed Denial Reasons

- [ ] `p1` - **ID**: `cpt-cf-license-enforcement-fr-reason-codes`

Every denial **MUST** carry a typed reason, chosen by the documented evaluation order, that lets the caller tell these
cases apart: no grant covers the resource in the tenant; grants cover it but none is bound to the subject; the grant
is not yet valid, expired, or past its termination date; it is suspended, revoked, or awaiting acceptance; a condition
is unmet; and, in builds that verify artifacts, the signature does not verify, the signing key is untrusted, the
artifact is stale, the clock or a sequence has regressed, or a grant is duplicated. The reason **MUST** appear in
`LicenseDecision.diagnostics`, telemetry, the management API's check-explain read, and in the audit ledger for the
events the ledger records; the resolver wire contract stays `granted` + advisory diagnostics. The concrete vocabulary
is fixed in DESIGN and grows additively, so consumers **MUST** tolerate reasons they do not know.

- **Rationale**: Operators and consuming modules cannot act on a bare `false`; naming the cases here and the codes in
  DESIGN keeps the contract stable while the vocabulary can grow.
- **Actors**: `cpt-cf-license-enforcement-actor-license-resolver`, `cpt-cf-license-enforcement-actor-platform-vendor`

#### Grant Conditions over Metadata

- [ ] `p2` - **ID**: `cpt-cf-license-enforcement-fr-grant-conditions`

A grant **MAY** carry conditions over the check's subject and resource `metadata`, expressed in a closed vocabulary
of typed, combinable constraints; the same vocabulary serves recipient rules over the recipient projection
(`cpt-cf-license-enforcement-fr-recipient-rules`). Conditions **MUST** be validated at grant-management time against
the registered contract schema of the object they address (for check conditions: the grant's Resource contract and
its admitted subject types), evaluated deterministically with a bounded cost per check, and **MUST** deny by default
when an attribute is absent. The evaluator **MUST** be linked at build time: neither a deployment's configuration nor
its operator may select, extend, or author the vocabulary. In an untrusted build, conditions **MUST** live inside the
signed payload. Counting **MUST NOT** be expressed as conditions.

- **Rationale**: Attribute-based licensing (region, model, tier) is the contract's extension point; a closed
  vocabulary validated up front avoids type errors at check time and keeps on-premise operators from widening grants by
  editing unsigned rules.
- **Actors**: `cpt-cf-license-enforcement-actor-platform-vendor`, `cpt-cf-license-enforcement-actor-license-resolver`

### 5.2 Resource and Subject Types

#### Registry-Resolved Type References

- [ ] `p1` - **ID**: `cpt-cf-license-enforcement-fr-type-references`

Every resource or subject type named in a grant, binding, condition, or recipient rule **MUST** be a GTS type id
resolved through the types registry at write time: it **MUST** exist and derive from the corresponding licensing base
type, and a subject type **MUST** be in the `admitted_subjects` of the grant's Resource contract. Type comparisons
**MUST** be hierarchy-aware: a subject whose type derives from an admitted type is admitted. A grant names the contract
version it was issued against; an additive-optional contract change **MUST NOT** affect existing grants, while a
breaking contract version is a new type that existing grants do not cover. The check path **MUST** decide from stored
type ids and cached schemas and **MUST NOT** call the registry per check; in an untrusted build the type ids **MUST**
be inside the signed payload. Adding a resource or subject type **MUST** require no change to this module's code.

- **Rationale**: Subject and resource types are platform data brought in at runtime, not this module's enum; resolving
  them through the registry keeps the model open-ended while every reference stays validated and stable.
- **Actors**: `cpt-cf-license-enforcement-actor-platform-vendor`

### 5.3 Grants

#### Grant Model

- [ ] `p1` - **ID**: `cpt-cf-license-enforcement-fr-grant-model`

A grant **MUST** carry an immutable server-assigned id, the target resource (a registered Resource contract type and an
optional instance id), the holder tenant, the subject scope, a validity window (`not_before`, `expires_at`, optional
`terminates_at` closing a grace window; a sub-grant may instead inherit its parent's window), optional conditions,
holder permissions (whether the holder may `bind`, the maximum chain depth below this grant — zero means it may
not `delegate` — and whether the grant requires acceptance when it is created), optional recipient rules, for a
sub-grant its parent grant reference and delegation id, an optional pack reference, a reference to the
evidence it descends from, a monotonic version, its lifecycle state, and — for a root grant admitted from an
artifact — its signature envelope. A subject grant additionally carries its binding: none (the grant is
unbound), a subject id, or the sub-grant delegated from it; it **MUST** have at most one binding at any time, and
at most one **live** child sub-grant (one not revoked).

- **Rationale**: One grant shape covers every commercial form: a site license is a tenant-wide grant, N named-subject
  licenses are N grants, and conservation is a property of the binding rather than an invariant to defend.
- **Actors**: `cpt-cf-license-enforcement-actor-platform-vendor`

#### Grant Admission

- [ ] `p1` - **ID**: `cpt-cf-license-enforcement-fr-admission-plugin`

A root grant **MUST** come into existence only through an admission plugin: the module itself **MUST NOT** decide that
a grant may exist. The plugin owns the evidence it accepts and the intake that carries it — grant parameters from an
authorized caller, or a vendor-signed artifact in a format it alone parses — and returns the admitted grants or a
typed reason. Each admitted grant **MUST** reference its evidence, which fixes the set of grants that exist because of
it. The module **MUST** store nothing a plugin did not admit and **MUST** expose only the create operations the linked
plugin's intake supports. A plugin that admits artifacts **MUST** verify them before admitting (§5.6) and **MUST**
reject an artifact already admitted or older than the one it replaces. A build **MUST** link exactly one admission
plugin into the enforcement binary, with its trust anchors, and **MUST NOT** select it at runtime. This module
**MUST** ship the plugins for grant parameters and for a vendor-signed envelope, whose format is that plugin's own.
Sub-grants are not admitted: they derive from an admitted root grant per `cpt-cf-license-enforcement-fr-delegation`.

- **Rationale**: What counts as sufficient evidence is the one thing that differs between deployments; behind a plugin
  it is one audited boundary instead of a branch in every write path, and a verifier the operator can swap or
  intercept is not a verifier.
- **Actors**: `cpt-cf-license-enforcement-actor-platform-vendor`

#### Grant Lifecycle

- [ ] `p1` - **ID**: `cpt-cf-license-enforcement-fr-grant-lifecycle`

A grant's lifecycle **MUST** distinguish at least: usable (in force, or expired within its grace window); awaiting the
holder's acceptance; suspended; revoked, which no verb reverses; and expired beyond grace. Only a usable grant
satisfies a check. Transitions **MUST** be explicit verbs: `issue` (usable at once or awaiting acceptance, per the
grant's acceptance requirement), `accept` and `reject` by the holder (a rejected grant is revoked), `suspend`,
`reinstate`, `revoke`, and `renew`, and for sub-grants `recall` by the grantor and `return` by a holder whose chain
ends unbound; both revoke the sub-grant and everything below it, unbind any subject at the end of the chain, and
return the parent's binding to none. Expiry **MUST** be derived from time, not from a stored transition: past
`expires_at` a grant with a `terminates_at` still satisfies a check, with a grace diagnostic, until that date and then
denies as terminated; without one it denies as expired. Suspension, revocation, expiry, and termination of a grant
**MUST** apply to every grant below it in the parent grant chain without changing their stored state. Revoked grants
**MUST** be retained for audit.

- **Rationale**: Distinct situations and explicit verbs let consumers and operators tell *why* a grant does not apply;
  a state the check derives cannot be flipped by editing a row.
- **Actors**: `cpt-cf-license-enforcement-actor-platform-vendor`, `cpt-cf-license-enforcement-actor-tenant-admin`

#### Subject Binding

- [ ] `p1` - **ID**: `cpt-cf-license-enforcement-fr-subject-binding`

The holder of an unbound subject grant whose permissions include `bind` **MUST** be able to `bind` it to a concrete
subject of an admitted type (e.g. a user id) and to `unbind` it. A subject id fixed at admission is not a binding the
holder may change: `bind` and `unbind` apply only to a subject grant admitted without one. A binding **MUST** survive
suspension and the grace window; apart from `unbind`, only the verbs of
`cpt-cf-license-enforcement-fr-grant-lifecycle` release it. Every `bind` and `unbind` **MUST** be recorded in the
audit ledger with actor and subject. The module **MUST NOT** validate or manage the subject's identity beyond type
admission and id shape.

- **Rationale**: A named-subject license is sold to the tenant but attributed to a subject; binding is the tenant's
  lever, and one binding per grant is what makes "N licenses" mean N.
- **Actors**: `cpt-cf-license-enforcement-actor-tenant-admin`

#### Delegation to Sub-Tenants

- [ ] `p1` - **ID**: `cpt-cf-license-enforcement-fr-delegation`

The holder of subject grants whose maximum depth is above zero (delegable grants) **MUST** be able to `delegate`
any number of its unbound ones to a recipient tenant, atomically creating one sub-grant per grant in the recipient's
scope, awaiting acceptance or usable at once per the grant's acceptance requirement, and setting each parent's binding
to its sub-grant; the delegating holder is the sub-grants' **grantor**. The sub-grants of one `delegate` **MUST**
share a **delegation id**, and `accept`, `reject`, `return`, and `recall` addressed to a delegation **MUST** apply to
all its sub-grants atomically. The recipient **MUST** be a descendant of the holder in the tenant hierarchy as
`tenant-resolver` reports it at that moment, and **MUST** satisfy the grant's recipient rules, if any. A sub-grant
**MUST** be a subset of its parent: same resource, subject scope, and evidence; validity inheriting or within the
parent's; conditions the parent's plus optionally its own; holder permissions no wider than the parent's with a
maximum depth one less; depth within the root's maximum. A sub-grant is itself a subject grant and may be bound or
delegated on under the same rules, so one root grant is a chain with one binding at its end. Reject, return, recall,
and the revocation of a sub-grant **MUST** return the parent's binding to none. Tenant-wide grants are not delegable
in phase 1 (§13).

- **Rationale**: One binding per grant, at every link, conserves the count without arithmetic; subset rules keep a
  sub-grant from ever widening the license it descends from.
- **Actors**: `cpt-cf-license-enforcement-actor-tenant-admin`

#### Recipient Rules

- [ ] `p3` - **ID**: `cpt-cf-license-enforcement-fr-recipient-rules`

A delegable grant **MAY** carry recipient rules: constraints in the vocabulary of
`cpt-cf-license-enforcement-fr-grant-conditions` over a registered projection of the candidate tenant, validated
against the projection's schema at issuance and evaluated at `delegate`; a recipient that fails them **MUST** be
rejected with a typed error.

- **Rationale**: "Only sub-tenants of type X" needs no policy engine once the recipient is a schematized object like the
  check's subject; reusing the condition vocabulary keeps one evaluator.
- **Actors**: `cpt-cf-license-enforcement-actor-tenant-admin`

#### Grant Replacement

- [ ] `p1` - **ID**: `cpt-cf-license-enforcement-fr-precedence`

The vendor **MUST** be able to replace a grant or Pack by a new issuance in one operation: the replaced grants are
revoked, and each binding they carried moves to an unbound grant of the replacement on the same resource that admits
the subject, so a plan change never opens a denial window. A replacement that cannot carry over every binding
**MUST** be rejected as a whole, naming the bindings without a successor; the holder unbinds first or the vendor
replaces with grants that can carry them.

- **Rationale**: Bindings belong to the holder, so only a single vendor operation can move them together with the
  grants they sit on.
- **Actors**: `cpt-cf-license-enforcement-actor-platform-vendor`

### 5.4 License Packs

#### Packs

- [ ] `p1` - **ID**: `cpt-cf-license-enforcement-fr-pack`

The vendor **MUST** be able to issue a set of grants to a tenant as one Pack: tenant-wide grants and subject grants,
bound or unbound, sharing a pack id. A Pack's composition **MUST** be immutable once issued; a change of size or
content is a replacement (`cpt-cf-license-enforcement-fr-precedence`), never an edit. Lifecycle verbs and replacement
addressed to a Pack **MUST** apply to all its grants atomically; binding and delegation operate on the member grants.

- **Rationale**: Packs are how licenses are sold; what a Pack contains is the catalog's business, keeping it whole and
  unchanging is this module's — an editable Pack would raise in place the binding and revocation questions that
  replacement settles.
- **Actors**: `cpt-cf-license-enforcement-actor-platform-vendor`, `cpt-cf-license-enforcement-actor-subscriptions`

### 5.5 Management API

#### Management Operations

- [ ] `p1` - **ID**: `cpt-cf-license-enforcement-fr-management-api`

The system **MUST** expose management operations for Packs, grants, and sub-grants — create / read / list plus the
lifecycle verbs of §5.3 and §5.4 as explicit sub-operations, not status patches — together with a read-only
check-explain operation returning the decision and reason the check path would produce for a given subject
and resource. Lists **MUST** be scoped to the caller's tenant, support filtering by resource type, state, and subject,
and report unbound, bound, and delegated subject-grant counts per pack and resource. Creating a root grant
**MUST** go through the admission plugin (`cpt-cf-license-enforcement-fr-admission-plugin`), which also decides
whether a parameter-based create exists in this build at all. The API **MUST** be mounted into `api-gateway` and
return RFC-9457 problems.

- **Rationale**: Issuance and administration need a first-class surface; check-explain lets operators debug a denial
  without reproducing the caller.
- **Actors**: `cpt-cf-license-enforcement-actor-license-management`,
  `cpt-cf-license-enforcement-actor-platform-vendor`, `cpt-cf-license-enforcement-actor-tenant-admin`

#### Idempotent, Versioned Mutations

- [ ] `p1` - **ID**: `cpt-cf-license-enforcement-fr-idempotent-mutations`

Every mutating management operation **MUST** accept a client idempotency token; a replay with the same token and
payload **MUST** return the original result, and a replay with a different payload **MUST** be rejected as a conflict.
Every managed resource **MUST** carry a monotonic version, and callers **MUST** be able to condition a mutation on it.

- **Rationale**: Retries under at-least-once delivery must never double-issue or double-bind.
- **Actors**: `cpt-cf-license-enforcement-actor-subscriptions`,
  `cpt-cf-license-enforcement-actor-platform-vendor`, `cpt-cf-license-enforcement-actor-tenant-admin`

#### PDP-Gated Administration

- [ ] `p1` - **ID**: `cpt-cf-license-enforcement-fr-pdp-gated-management`

Every management operation **MUST** be authorized through the PDP before execution, with tenant scope taken from
`SecurityContext` and never from the payload, and the PDP's constraints applied in the same transaction as the
mutation. A tenant administrator **MUST** be limited to grants their tenant holds (accept, reject, bind, unbind,
delegate, return; and recall and renew of the sub-grants it delegated, within its own window) and **MUST NOT** be able
to issue, suspend, reinstate, revoke, or renew a root grant; those verbs and Pack issuance are vendor operations. A
grantor **MUST** be able to read the state and binding of the sub-grants it delegated across the tenant boundary, and
nothing else about the recipient tenant; the vendor **MUST** be able to read the full delegation tree and ledger of
every grant it issued.

- **Rationale**: The management API is where a licensing service can be turned into a minting service; PDP gating with
  in-transaction constraints is the platform's defense in depth.
- **Actors**: `cpt-cf-license-enforcement-actor-tenant-admin`, `cpt-cf-license-enforcement-actor-platform-vendor`

#### Audit Ledger

- [ ] `p1` - **ID**: `cpt-cf-license-enforcement-fr-audit-ledger`

The system **MUST** append a record to an append-only ledger (not cryptographically tamper-evident, §4.2) for every
lifecycle transition, binding, delegation, artifact import, and verification failure at import, carrying actor,
tenant, timestamp, verb, denial reason where applicable, and before/after values, and **MUST** expose it read-only,
tenant-scoped, with retention of the ledger and of revoked grants configurable by the operator.

- **Rationale**: Licensing disputes and revenue reconciliation are settled from the ledger, not from current state.
- **Actors**: `cpt-cf-license-enforcement-actor-platform-vendor`, `cpt-cf-license-enforcement-actor-tenant-admin`

### 5.6 Signed Grants and Untrusted Builds

#### Verification Is Fixed by the Build

- [ ] `p2` - **ID**: `cpt-cf-license-enforcement-fr-verification-fixed`

Whether grants are verified **MUST** follow solely from the linked admission plugin, and **MUST NOT** be selectable by
configuration, environment, request, or artifact. In a build that verifies artifacts, every input the check reads
**MUST** come from one of three sources: the signed payload of the root grant's artifact (resource, holder, subject
scope, validity, conditions, holder permissions); a derivation from signed inputs at evaluation time (the root's
state, the sub-grant chain); or a holder decision the signed grant explicitly permits (acceptance, binding,
delegation). Verification **MUST** happen at both points that can change the answer: the plugin verifies before a root
grant is stored, and every evaluation re-verifies the root's artifact against the trust bundle and revocation list
current at that moment, so a revoked key, a rotated anchor, or a lapsed TTL denies without waiting for a re-import.
Trust material received after assembly — issuing keys, revocations — **MUST** be accepted only if it verifies against
material already trusted, so a chain from the compiled-in anchors is the only way a key becomes trusted. In that build
every evaluation **MUST** also verify that each grant in the matched chain is the only live child of its parent and
that the root is accounted for exactly once by its evidence, denying the whole chain as duplicated otherwise. A root
grant whose signature is missing, stripped, malformed, made with an untrusted key, or does not verify **MUST** deny
with the corresponding reason and **MUST NOT** be routed to an unsigned evaluation path — no such path exists in that
build. Signature verification, trust-bundle handling, revocation-list processing, and clock high-water marks **MUST**
be gated behind trait-based compile-time options, so a build without them links none of that code and cannot link an
admission plugin that verifies artifacts, while the domain model, lifecycle, management semantics, and check semantics
stay identical. The linked admission plugin, its trust anchors, and whether the verification code is present **MUST**
be reported in telemetry.

- **Rationale**: A verification strength that a config file, an env var, or the artifact itself can lower is the
  `alg: none` failure class handed to the adversary who owns the host; and a consumer who never deploys untrusted
  should not pay for that machinery in binary size, dependencies, or attack surface.
- **Actors**: `cpt-cf-license-enforcement-actor-license-resolver`, `cpt-cf-license-enforcement-actor-platform-vendor`

#### Signed Grant Artifact

- [ ] `p2` - **ID**: `cpt-cf-license-enforcement-fr-signed-artifact`

A grant artifact **MUST** carry a signature over the exact payload bytes the verifier interprets, never over a
re-serialization of them. The payload is the grants as the vendor issued them — resource, holder, subject scope with
the subject id where the vendor fixed it, validity, conditions, holder permissions — plus issued-at, freshness TTL, a
per-artifact nonce, the algorithm, the key id, and an envelope version, with the signing input bound to the artifact
kind. The verifier **MUST** verify the signature against the trust bundle before interpreting any payload field, in
one documented check order. Only asymmetric signatures **MUST** be accepted; symmetric MACs **MUST NOT** be used
anywhere on a verification path.

- **Rationale**: Production signing schemes converge on this envelope; deviating breaks signatures on innocent
  re-serialization or verifies attacker-reconstructed bytes.
- **Actors**: `cpt-cf-license-enforcement-actor-platform-vendor`, `cpt-cf-license-enforcement-actor-license-resolver`

#### Freshness, Renewal, and Offline Revocation

- [ ] `p2` - **ID**: `cpt-cf-license-enforcement-fr-freshness-revocation`

In an untrusted build the vendor **MUST** be able to give an artifact a freshness TTL: past it the grants it carries
deny as stale until a renewed artifact arrives. Revocation and suspension **MUST** be effected by ceasing renewal and
by signed, versioned revocation material the verifier applies; the newest material applied is the vendor's current
statement: a grant it lists as suspended or revoked is so, one it no longer lists, whatever an earlier entry said, is
not; a version lower than the highest already applied **MUST** be rejected. An artifact issued without a TTL to a
deployment that never receives revocation material cannot be revoked before its expiry; that is the vendor's choice at
issuance.

- **Rationale**: Short TTL plus renewal is the only revocation that works without connectivity to the vendor; whether
  to pay its operational price is the vendor's call per artifact, and the air-gap limit must be explicit, not implied
  away.
- **Actors**: `cpt-cf-license-enforcement-actor-platform-vendor`, `cpt-cf-license-enforcement-actor-license-resolver`

#### Clock Rollback Defence

- [ ] `p2` - **ID**: `cpt-cf-license-enforcement-fr-clock-defense`

The verifier **MUST** reject artifacts with `issued_at` in the future, **MUST** persist a monotonic high-water mark
of the latest evaluation time and of the highest `issued_at` and revocation version it has applied, and **MUST** deny
as a clock rollback when the clock falls behind that mark. This catches a clock set back on a live instance and
replayed material; it does not catch a clock rolled back together with the storage that holds the mark, nor a clock
that stands still. The tolerated skew is fixed by the build.

- **Rationale**: A clock set back must not by itself resurrect expired or revoked grants; a tolerance the operator
  could widen would be a grant the operator can extend; and the limits of a software-only clock defence belong in
  the requirement, not in a footnote.
- **Actors**: `cpt-cf-license-enforcement-actor-license-resolver`

### 5.7 Observability

#### Operational Telemetry

- [ ] `p1` - **ID**: `cpt-cf-license-enforcement-fr-telemetry`

The system **MUST** emit metrics for checks by outcome and reason, check latency, management operations by verb
and outcome, subject-grant inventory (unbound, bound, delegated) per resource type, verification failures by reason,
linked admission plugin, and trust-bundle version; and **MUST** log every fail-closed event with its cause. Logs and
metrics **MUST NOT** contain signature material or private-key identifiers beyond the public key id.

- **Rationale**: Fail-closed systems are only operable when every denial cause is countable.
- **Actors**: `cpt-cf-license-enforcement-actor-platform-vendor`

## 6. Non-Functional Requirements

> **Global baselines**: Project-wide NFRs are defined in
> [docs/ARCHITECTURE_MANIFEST.md](../../../../docs/ARCHITECTURE_MANIFEST.md) and
> [guidelines/](../../../../guidelines/). Document only module-specific NFRs here.

### 6.1 Module-Specific NFRs

#### Fail-Closed Under Every Failure

- [ ] `p1` - **ID**: `cpt-cf-license-enforcement-nfr-fail-closed`

No failure of storage, types registry, PDP, trust-bundle loading, or clock sanity **MUST** ever yield
`granted: true`; issuance **MUST** be refused when the PDP is unavailable.

- **Threshold**: 0 grant-by-default outcomes and 0 unverified imports under fault injection across all listed
  dependencies.
- **Rationale**: A fail-open path in a licensing service is a direct revenue and security incident.
- **Architecture Allocation**: See DESIGN.md § NFR Allocation.

#### Tenant Isolation

- [ ] `p1` - **ID**: `cpt-cf-license-enforcement-nfr-tenant-isolation`

Every check and every management read or write **MUST** be scoped to the tenant derived from `SecurityContext` (or the
resolver's request context), with the single documented exception of a grantor reading the sub-grants it delegated.

- **Threshold**: 0 cross-tenant reads or grant matches in isolation tests; tenant id never taken from payloads.
- **Rationale**: A cross-tenant grant match licenses one customer with another's purchase.
- **Architecture Allocation**: See DESIGN.md § NFR Allocation.

#### Binding Conservation Integrity

- [ ] `p1` - **ID**: `cpt-cf-license-enforcement-nfr-conservation`

Under concurrent bind, delegate, recall, return, and revocation, every subject grant **MUST** have at most one
binding and at most one live child, at every committed state.

- **Threshold**: 0 grants with two bindings or two live children in concurrency tests at 100 concurrent mutations per
  grant.
- **Rationale**: Conservation is the property that makes delegation safe to offer.
- **Architecture Allocation**: See DESIGN.md § NFR Allocation.

#### Check Latency

- [ ] `p1` - **ID**: `cpt-cf-license-enforcement-nfr-check-latency`

The plugin-side check **MUST** complete within 40ms at p95 at up to 1 000 checks/s per node for a tenant holding up
to 50 000 grants, including the walk to the root and signature verification in an untrusted build.

- **Threshold**: 40ms p95 at the plugin boundary. Targets to be calibrated in DESIGN.
- **Rationale**: The check sits on the access path of every gated request.
- **Architecture Allocation**: See DESIGN.md § NFR Allocation.

#### Signature Strength

- [ ] `p2` - **ID**: `cpt-cf-license-enforcement-nfr-signature-strength`

Accepted signature algorithms **MUST** be asymmetric with at least 128-bit security, verified in strict mode, from a
closed allow-list that includes at least one FIPS 140-3 approved algorithm; the concrete choice is an ADR.

- **Threshold**: 0 symmetric or sub-128-bit algorithms accepted; algorithm-confusion test vectors all rejected.
- **Rationale**: A verifier that can also forge is the disqualifying anti-pattern.
- **Architecture Allocation**: See DESIGN.md § NFR Allocation.

#### Deterministic Evaluation

- [ ] `p1` - **ID**: `cpt-cf-license-enforcement-nfr-determinism`

The same grant set, request, trust bundle, and evaluation time **MUST** always produce the same decision and reason
code, and condition evaluation **MUST** be bounded by a per-check budget.

- **Threshold**: 100% replay agreement across nodes; condition evaluation aborts (deny) past a configurable budget,
  default 5ms.
- **Rationale**: Replayable decisions are what make the audit ledger and check-explain trustworthy.
- **Architecture Allocation**: See DESIGN.md § NFR Allocation.

#### Offline Operation

- [ ] `p2` - **ID**: `cpt-cf-license-enforcement-nfr-offline`

In an untrusted build the check path **MUST NOT** depend on anything outside the deployment: no call to the issuer
or to any vendor service, only locally persisted artifacts, trust material, and revocation material, for the full
freshness TTL of each artifact and indefinitely for one without.

- **Threshold**: 100% of checks answered with the deployment cut off from the vendor within each artifact's TTL;
  0 hardcoded fallback decisions.
- **Rationale**: A licensing truth that lives only on a remote server degrades to a hardcoded default.
- **Architecture Allocation**: See DESIGN.md § NFR Allocation.

### 6.2 NFR Exclusions

- Authentication, session, credential policies: platform-owned; callers arrive with a `SecurityContext`.
- Availability, RPO/RTO, backup: platform baselines apply unchanged.
- High write throughput: N/A, issuance and binding are low-frequency administrative operations.
- Usability, accessibility, internationalisation, regulatory compliance: N/A, no user interface, no payment, health,
  or personal data beyond opaque subject ids.

## 7. Public Library Interfaces

### 7.1 Public API Surface

#### Resolver Backend Plugin

- [ ] `p1` - **ID**: `cpt-cf-license-enforcement-interface-resolver-plugin`

- **Type**: Rust trait implementation (`LicenseResolverPluginClient` from `license-resolver-sdk`), registered via a
  `LicenseResolverPluginSpecV1` GTS instance with vendor + priority.
- **Stability**: stable (tracks the resolver contract's major version).
- **Description**: The check path. Consumes conforming `LicenseCheckRequest`s, returns `LicenseDecision` with the denial
  reason in diagnostics.
- **Breaking Change Policy**: Follows `license-resolver-sdk` major versions; denial-reason vocabulary is additive only.

#### Management API

- [ ] `p1` - **ID**: `cpt-cf-license-enforcement-interface-management-api`

- **Type**: REST API mounted into `api-gateway`, and the same operations as an in-process Rust client
  (`LicenseEnforcementClient` in `license-enforcement-sdk`, resolved via ClientHub) for platform modules.
- **Stability**: unstable until first release, then stable.
- **Description**: Packs, grants, sub-grants, lifecycle verbs, check-explain, ledger read; one semantics and one
  authorization under both transports.
- **Breaking Change Policy**: Path-versioned REST, additive fields non-breaking; SDK follows the crate's major
  version.

### 7.2 External Integration Contracts

#### Grant Admission Plugin Contract

- [ ] `p1` - **ID**: `cpt-cf-license-enforcement-contract-admission-plugin`

- **Direction**: required from the admission plugin.
- **Protocol/Format**: Capability-based `V1` trait implemented by exactly one admission plugin linked into the
  enforcement binary. The plugin accepts evidence in its own shape, returns the admitted root grants, each referencing
  its evidence, or a typed reason, declares its intakes, and reports health. The deployment **MUST NOT** discover,
  select, or replace the plugin at runtime.
- **Compatibility**: GTS-versioned; a new evidence or carrier format is a new plugin, never a change to this contract.

## 8. Use Cases

### Check a Named-User License

- [ ] `p1` - **ID**: `cpt-cf-license-enforcement-usecase-named-user-check`

**Actor**: `cpt-cf-license-enforcement-actor-license-resolver`

**Preconditions**:
- Tenant T holds 50 usable subject grants on resource F; 20 are bound, one of them to user U.

**Main Flow**:
1. A consuming module checks resource F for subject `user U` in tenant T; the resolver validates and delegates.
2. The plugin finds T's grants covering F, filters to usable states, and finds the grant bound to U.
3. Conditions (if any) are evaluated against the request `metadata`.
4. The plugin returns `granted: true` with the matched grant id in diagnostics.

**Postconditions**:
- No state changed; the decision is attributable to one grant.

**Alternative Flows**:
- **No grant is bound to U**: `granted: false`, reason: not bound.
- **Pack suspended by the vendor**: `granted: false`, reason: suspended, regardless of binding.

### Delegate Subject Grants Down a Tenant Chain

- [ ] `p1` - **ID**: `cpt-cf-license-enforcement-usecase-delegate`

**Actor**: `cpt-cf-license-enforcement-actor-tenant-admin`

**Preconditions**:
- Tenant T holds 100 usable subject grants on resource F whose permissions allow `bind` and delegation to depth 2;
  30 are bound. Tenant S is a child of T, S2 a child of S.

**Main Flow**:
1. T's administrator delegates 40 grants to S; the PDP authorizes and `tenant-resolver` confirms S descends from T.
2. The system picks 40 unbound grants, sets their binding to the new sub-grants, and creates 40 sub-grants in S's
   scope (awaiting acceptance, validity inherited, depth 1) under one delegation id; T has 30 unbound grants left.
3. S's administrator accepts the delegation; the sub-grants become usable. S binds one to user U and delegates 10 to
   S2, which accepts and binds them to its own users.
4. A check for `user U` in S finds the sub-grant bound to U, walks to its root grant in T, finds every link usable
   and a subset of its parent, and grants.
5. Later, T recalls the delegation: subjects bound in S and S2 are unbound, S2's sub-grants and S's are revoked,
   and all 40 root grants return to binding none.

**Postconditions**:
- Every step is in the ledger with actor and before/after bindings; at no point did the 100 root grants end in more
  than 100 subjects.

**Alternative Flows**:
- **S rejects**: the sub-grants are revoked and all 40 parent grants return to binding none immediately.
- **S returns 5 unbound grants**: their sub-grants are revoked, T has 35 unbound grants.
- **S tries to return a grant it has bound**: rejected; S unbinds first or T recalls.
- **T tries to delegate 80 before step 1**: rejected, only 70 grants are unbound.
- **Vendor suspends T's pack**: every check down the chain denies as suspended; no stored state below changes.
- **The same user U checks in T's context**: denied as not bound; T holds grants on F but none bound to U.

### Issue a Pack on Purchase

- [ ] `p1` - **ID**: `cpt-cf-license-enforcement-usecase-purchase-issue`

**Actor**: `cpt-cf-license-enforcement-actor-subscriptions`

**Preconditions**:
- Tenant T has just bought a subscription; Subscriptions maps it to a set of grants.

**Main Flow**:
1. Subscriptions issues those grants to T as one Pack through the SDK client or API, with an idempotency token derived 
   from the commercial event.
2. Admission admits the grants under a shared pack id; the ledger records the caller, the token, and the resulting
   grants.
3. T's administrator binds grants to users; checks for those users pass.
4. On renewal Subscriptions renews the Pack, on a plan change it replaces it, and on cancellation it revokes it; every
   member grant and its binding follow atomically.

**Postconditions**:
- The grants backing the subscription exist exactly once per commercial event and are attributable to it.

**Alternative Flows**:
- **The commercial event is delivered twice**: the replay returns the original result; no second grant is created.

### Issue a Signed Pack to an On-Premise Tenant

- [ ] `p2` - **ID**: `cpt-cf-license-enforcement-usecase-signed-onprem`

**Actor**: `cpt-cf-license-enforcement-actor-platform-vendor`

**Preconditions**:
- The on-premise deployment runs an untrusted build with the current trust bundle; the issuer holds a valid issuing
  key.

**Main Flow**:
1. The vendor issues Pack P to tenant T in its own deployment; the issuer signs the evidence for its grants in the
   format the deployment's admission plugin admits.
2. The evidence reaches the deployment through the plugin's own intake (sync or file drop); the plugin verifies
   version, signature, key id against the trust bundle, `issued_at`, and freshness before anything is stored, and
   creates the root grants the evidence accounts for.
3. T's administrator binds and delegates subject grants locally; checks are answered locally from the stored chain and
   the verified evidence.
4. Before the TTL lapses the deployment fetches (or the vendor ships) renewed evidence; to suspend or revoke, the
   vendor publishes a revocation list and stops renewing.

**Postconditions**:
- The deployment can neither create a root grant nor extend one; expired freshness denies as stale.

**Alternative Flows**:
- **Operator edits the payload**: signature fails; the check denies as unverified.
- **Operator sets the clock back after expiry**: high-water mark detects regression; the check denies as a clock
  rollback.
- **Artifact replayed after a newer one was imported**: rejected as a version regression; nothing is stored.
- **Operator copies a bound grant row for a second subject**: the grant has two live bindings; both subjects deny as
  duplicated.
- **Operator flips a revoked root grant's stored state to usable**: the check derives the state from the revocation
  list and still denies as revoked.

## 9. Acceptance Criteria

- [ ] `is_licensed` returns correct decisions for subject and tenant-wide scopes, for bound and unbound subject
  grants, and for type-level and instance-level resources, each denial carrying a typed reason.
- [ ] Every grant transition is reachable only through its verb; every situation other than usable denies with its own
  reason, and suspension, revocation, or expiry of a grant denies every grant below it in the parent grant chain.
- [ ] No grant ever holds two bindings or two live children under concurrent load; recall cascades to every
  descendant.
- [ ] A Pack's grants share a pack id, cannot be added to or removed once issued, and follow pack-level verbs
  atomically.
- [ ] Every management mutation is idempotent by client token, versioned, and PDP-authorized with tenant scope from
  `SecurityContext`; a tenant administrator cannot issue, suspend, revoke, or renew a root grant.
- [ ] Return moves only unbound grants; renewing a root grant extends every sub-grant that inherits its validity.
- [ ] Replacing a grant revokes it and carries its bindings over in one operation; no check in between denies.
- [ ] Registering a new resource or subject contract type requires no code change in this module.
- [ ] No root grant reaches the store except through the linked admission plugin; an untrusted build's plugin verifies
  before admitting, rejects replayed and version-regressed artifacts, and leaves no parameter-based create
  registered; adding an evidence or carrier format is a new plugin, not a change to this module.
- [ ] An untrusted build denies missing, tampered, unknown-key, stale, and clock-regressed artifacts with the matching
  reason, and exposes no configuration that relaxes this; a build without the untrusted opt-in links
  none of that code.
- [ ] In an untrusted build, editing a stored grant, sub-grant, binding, or lifecycle state other than through a
  permitted holder decision leaves every check's answer unchanged or turns it into a denial; a duplicated grant or
  sub-grant denies as duplicated. Removing or rolling back stored trust material, revocation material, or the
  high-water marks defers a suspension or revocation at most until the artifact's freshness TTL lapses; a rollback
  of storage together with the clock is outside the defence (§5.6).
- [ ] Applying a trust bundle that revokes an issuing key, or letting an artifact's TTL lapse, denies the grants
  already stored — without re-importing anything.
- [ ] No dependency failure produces `granted: true`.

## 10. Dependencies

| Dependency | Description | Criticality |
|------------|-------------|-------------|
| `license-resolver-sdk` | Check contract, plugin trait, plugin spec, licensing contract base types | p1 |
| `types-registry` | GTS registration of this module as a resolver backend, of licensing contracts, and of its own types | p1 |
| `authz-resolver` (PDP via `PolicyEnforcer`) | Authorization of every management operation | p1 |
| `SecurityContext` | Tenant scope and actor identity for management operations | p1 |
| `tenant-resolver` | Tenant hierarchy (`is_ancestor`, descendants) at delegation time and tenant metadata behind the recipient projection | p1 |
| `api-gateway` | Hosts the management REST API | p1 |
| Issuer (vendor-side) | Signed grant artifacts, trust bundles, revocation lists | p2 |

## 11. Assumptions

- **Tenant-bounded grants (current model)**: every grant and every check lives inside one tenant scope; a sub-grant is a
  grant in the recipient tenant's scope. Cross-tenant or tenant-independent licensing would be an explicitly versioned
  contract change in `license-resolver`.
- The tenant hierarchy is queryable in-process through `tenant-resolver`; subject ids, user ids included, are stored
  as opaque natural keys and never resolved against their owning module.
- **Phase-1 subject types are tenant and user.** The subject model is open-ended per `license-resolver` (any registered
  Subject contract type, e.g. an agent); adding a subject type is a contract registration, not a change to this
  module's grant model.
- The issuer (signing, key generation, custody, trust-bundle publishing) is a separate component with its own PRD;
  this PRD fixes only the artifact format and the trust material consumed.
- The untrusted-build threat model is the industry's: signature verification with embedded public trust, date and
  rollback checks, and legal deterrence; not DRM.
- A single check is answered from a tenant's grant set held in memory or a local index; subject grants per tenant
  are in the tens of thousands, not millions, and delegation chains are bounded by the root's maximum depth.

## 12. Risks

| Risk | Impact | Mitigation |
|------|--------|------------|
| Tenant hierarchy semantics differ from delegation needs (depth, moves) | Delegation blocked or sub-grants orphaned on tenant re-parenting | Bound depth; treat re-parenting as recall + re-delegate; open question §13 |
| Signature format frozen too early | Every issued artifact must be re-issued on change | Envelope version in the signed input; multiple versions verifiable at once |
| Trust-material distribution to air-gapped sites is operationally heavy | Stale anchors, undeliverable revocations | File-based distribution owned by the plugin; explicit documentation of what cannot be revoked |
| Builds with different admission plugins diverge into different products | Doubled maintenance, inconsistent semantics | One domain model, lifecycle, and management semantics for every plugin; only how root grants arrive varies, enforced by shared tests |
| Plan changes surprise customers | Denial window or lost bindings | Atomic replacement with binding carry-over |
| Denial-reason vocabulary grows ad hoc | Consumers break on unknown reasons | Vocabulary owned by DESIGN, additive only; consumers tolerate unknown reasons; documented evaluation order |

## 13. Open Questions

- Should a Pack be delegable as a unit, or only its member grants (current PRD)? — Owner: platform vendor; target:
  DESIGN.
- Delegation of tenant-wide (site) grants is excluded in phase 1. If wanted later: copy-to-sub-tenant semantics
  bounded by a count of sub-tenants? — Owner: platform vendor; target: after v1.
- What happens to sub-grants when the recipient tenant is re-parented or deleted? — Owner: license-enforcement and
  tenant-resolver maintainers; target: DESIGN.
- Retention period for revoked grants and the ledger; default proposed 400 days. — Owner: platform vendor; target:
  DESIGN.

## 14. Traceability

- **Design**: [DESIGN.md](./DESIGN.md)
- **ADRs**: [ADR/](./ADR/)
- **Features**: [features/](./features/)
- **Upstream contract**: [license-resolver PRD](../../license-resolver/docs/PRD.md)
