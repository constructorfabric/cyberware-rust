---
status: proposed
date: 2026-09-08
---
# ADR-0005: Upward-Rooted Collection Read Under the No-Projection PEP Contract

<!-- toc -->

- [Context and Problem Statement](#context-and-problem-statement)
- [Decision Drivers](#decision-drivers)
- [Considered Options](#considered-options)
- [Decision Outcome](#decision-outcome)
  - [Barriers are deliberately bypassed for the ancestor chain](#barriers-are-deliberately-bypassed-for-the-ancestor-chain)
  - [How authorization applies to a collection](#how-authorization-applies-to-a-collection)
  - [Reducing a reference to one item](#reducing-a-reference-to-one-item)
  - [Pagination over a reduced result](#pagination-over-a-reduced-result)
  - [What stays out of the filter](#what-stays-out-of-the-filter)
  - [Consequences](#consequences)
  - [Confirmation](#confirmation)
- [Pros and Cons of the Options](#pros-and-cons-of-the-options)
  - [Upward-rooted collection, tenant predicate as a gate (CHOSEN)](#upward-rooted-collection-tenant-predicate-as-a-gate-chosen)
  - [Own-rows-only collection](#own-rows-only-collection)
  - [Tenant predicate applied as a SQL clamp](#tenant-predicate-applied-as-a-sql-clamp)
  - [Declare the `tenant_hierarchy` capability and project `tenant_closure`](#declare-the-tenant_hierarchy-capability-and-project-tenant_closure)
  - [Aggregated listing across descendants](#aggregated-listing-across-descendants)
- [Revisit Triggers](#revisit-triggers)
- [Traceability](#traceability)

<!-- /toc -->

**ID**: `cpt-cf-credstore-adr-upward-collection-read`

## Context and Problem Statement

[ADR-0004](0004-cpt-cf-credstore-adr-secret-value-exposure.md) introduces a credential-record collection (`cpt-cf-credstore-fr-list-credentials`). That collides with an explicit statement in the current design.

DESIGN §4.4 records that the gear advertises **no PEP capabilities**, so the PDP hands it pre-expanded, flat tenant predicates (`Eq` / `In` on `owner_tenant_id`) and resolves any subtree grant on its own side. The justification given there is that this is "sufficient by construction: every credstore operation is a point operation addressed in the caller's own tenant (there is no LIST, and writes never cross the tenant boundary), so the gear-side question is always *does the scope admit this one tenant* — never *expand this subtree*". A structured `InTenantSubtree` predicate reaching the gear is treated as a capability-contract breach and fails closed. Consequently credstore projects no `tenant_closure` table and has no co-location requirement on the Account Management database.

A collection read appears to break the premise. It also raises a question the point read never had to answer: a listing that is useful to an integration administrator must show **inherited** entries, so its rows deliberately come from tenants other than the caller's — which is exactly the shape a flat `owner_tenant_id` predicate would filter out.

There is a second, unrelated difficulty. The listing must show one entry per reference: the one a value read would resolve. That means several rows (the caller's own plus its ancestors') collapse into one item, and nothing in the platform performs row reduction inside cursor pagination today.

## Decision Drivers

- **D1 — keep the no-projection contract.** The gear must not require `tenant_closure`, a co-located Account Management database, or PEP capabilities it does not have.
- **D2 — the listing must show inheritance.** Hiding inherited entries would defeat the requirement it exists for: an administrator needs to see that SMTP comes from the partner rather than from their own tenant.
- **D3 — one authorization model for point and collection reads.** A second, parallel way of deciding access would be a place for the two to drift apart.
- **D4 — pagination must not lose or duplicate entries.** No reference may be split across a page boundary in a way that hides its winner.
- **D5 — fail closed on anything unexpected.** An unknown predicate, an unsupported field, or a structured subtree predicate must deny rather than degrade.

## Considered Options

1. **Upward-rooted collection, tenant predicate used as a gate** — the collection is rooted at the caller's tenant and spans only that tenant and its ancestors; the flat tenant predicate gates the request on the caller's own tenant rather than clamping rows.
2. **Own-rows-only collection** — list only rows whose `owner_tenant_id` is the caller's tenant; no ancestors, no inheritance in the listing.
3. **Tenant predicate applied as a SQL clamp** — put `In(owner_tenant_id, …)` straight into the query, as other gears do.
4. **Declare the `tenant_hierarchy` capability and project `tenant_closure`** — accept subtree predicates and let SQL do the expansion.
5. **Aggregated listing across descendants** — one call returns the catalogues of the caller's whole subtree.

## Decision Outcome

**Chosen: option 1.** The collection read is **rooted at the caller's tenant and walks upward only**: its rows come from that tenant and its ancestor chain, never from descendants. The flat tenant predicate remains a **gate** on the caller's own tenant, not a row clamp. Cross-tenant listing is not provided: a parent that must see a child's catalogue acts in that child's context, exactly as it already does for point reads (`cpt-cf-credstore-fr-service-retrieve`).

The premise in DESIGN §4.4 survives once it is stated precisely. What that section actually rules out is **downward** expansion — asking the gear to enumerate a subtree it has no closure table for. The collection introduced here needs no such expansion: it walks the ancestor chain, and upward hierarchy knowledge already comes from the Tenant Resolver, which the gear already calls for every point read. So "there is no LIST" must be rewritten as "there is no downward listing", and no projection table appears.

### Barriers are deliberately bypassed for the ancestor chain

Building an inheritance chain requires knowing **all** ancestors, so the gear reads the chain with barriers ignored. This is deliberate, valid behaviour, not an oversight, and it is the single place in the gear that looks past an isolation barrier. Because "we ignore the barrier" is the kind of sentence that stops a security review, the decision is stated here with its exact scope.

**Why it is valid.** A barrier (`self_managed`) isolates *management*: it stops a parent from administering a customer that runs its own subtree. It does not retract data the parent already published downward. Publishing a credential as `shared` is the owner's explicit decision to make it available to descendants, and `cpt-cf-credstore-fr-hierarchical-resolve` states the consequence as a requirement: a `shared` secret is inherited by all descendants, including across `self_managed` boundaries. Withholding it at a barrier would silently break integrations that were working before the customer took over its subtree, and it would do so without any actor having decided to revoke anything.

**What the bypass does and does not cover.** The invariants that keep it narrow:

- It reads the **ancestor identifiers only**. Which of an ancestor's rows may then be seen is decided by the ordinary visibility rules, which admit `shared` rows and nothing else from an ancestor — `tenant` and `private` rows never leave their own tenant, barrier or not.
- It grants **no authority**. Access is still decided by the PDP gate on the caller's own tenant, and role inheritance *into* a barrier tenant continues to respect barriers, so a parent still cannot manage or read inside a `self_managed` customer. Data flows down through the barrier; permissions do not flow down through it, and roles assigned above do not apply inside it.
- It never traverses **downward**. The bypass makes ancestors visible to a descendant; it gives no one a view of a subtree.
- It is confined to **one call site** — the ancestor-chain lookup — so the exception stays reviewable rather than becoming an ambient property of the gear.

**Why this forces the tenant dimension to be a gate.** Scopes that respect barriers exclude a barrier tenant's ancestors by construction. If the tenant predicate from such a scope were applied as a SQL clamp, the inherited half of the listing would vanish for exactly those tenants — the platform default would disappear from the customer's catalogue while still being the value its applications receive. Keeping the tenant dimension as a gate and the chain as the resolver's business is what keeps the two consistent.

### How authorization applies to a collection

The point read already works this way, and the collection reuses it verbatim (D3):

1. The ancestor chain is fetched from the Tenant Resolver with barriers ignored, for the reasons and under the invariants set out above.
2. The SQL query is not tenant-clamped. It selects candidate rows restricted to that chain, with the same visibility rules the point read applies: in the caller's own tenant, private rows owned by the caller plus tenant and shared rows; in ancestors, shared rows only.
3. The PDP decision gates the **request**: the returned scope must admit the caller's own tenant. Hierarchical visibility is decided by the resolver, not by the PDP — which is precisely what lets an inherited entry appear at all.
4. **A caller the gate refuses gets an empty page, not a refusal**, and that is a deliberate choice with a cost worth stating. The argument against it is sound in the abstract: an empty page ought to mean "authorized, nothing here", and collapsing "you may not list" into it makes an operator unable to tell a missing grant from an unconfigured tenant. The argument for it is that this gear has no operation-level evaluation to refuse *from*. The PDP resource is the secret's resolved concrete type, so a scope exists only once rows have been read and their types resolved, and the gate runs once per distinct type present on the page. A caller with no grant at all and a tenant with an empty catalogue are therefore indistinguishable **by construction**, not by policy: in the second case no type is present, so no evaluation happens and there is nothing to deny. Returning `AccessDenied` would require evaluating `list_meta` against the secret *base* type before the query — a resource this gear deliberately does not authorize against today (DESIGN §5.4: the concrete type is the only PDP resource, so a policy can target any type without a base-type gate). That is a real design change, not a status-code change, and it is recorded as a revisit trigger rather than smuggled in here.
5. Attribute predicates from the same scope — category, type, sharing, reference — **are** applied as SQL clamps. Only the tenant dimension is a gate rather than a clamp.
6. The concrete secret type of each row is resolved and authorized per distinct type present in the page; rows of a type the caller has no grant for are dropped after the query, and the page continues with its cursor unchanged. Account Management's metadata listing already behaves this way and documents that a short page is expected.
7. A structured `InTenantSubtree` predicate reaching the gear still fails closed, unchanged from today.

### Reducing a reference to one item

One item per reference, and it must be the row a value read of that reference would resolve — otherwise the catalogue and the point read disagree, which is the one failure a catalogue cannot recover from. Two rules, in this order:

1. **Only resolvable rows compete.** A `declared` record — created but with no value yet ([ADR-0004](0004-cpt-cf-credstore-adr-secret-value-exposure.md), DESIGN §6.1) — does not resolve and does not shadow, so it must not win the reduction while a resolvable inherited row exists under the same reference. Getting this wrong is not cosmetic: the listing would show the caller's own empty record precisely when the point read is serving the ancestor's value, i.e. it would report "configured here" for a credential that is in fact inherited. A reference whose *only* row is `declared` still appears, with its state, because an administrator mid-configuration needs to see it.
2. **Among resolvable rows, nearest wins**, and `private` beats non-`private` at the same depth — the same two-phase priority `resolve_for_get` applies, reused rather than restated, so a change to visibility rules cannot apply to one read and miss the other (D3).

### Pagination over a reduced result

The canonical sort is `reference ASC, id ASC`, with `id` — the row primary key — as the tiebreaker. `reference` alone is not unique: the same reference legitimately exists in several tenants and in both sharing classes.

Because the sort leads with `reference`, **all rows of one reference are contiguous**. The rule that follows is: **a cursor always sits on a reference boundary.** A page is extended to the end of the reference group it lands in, reduction picks the winner per whole group, and the cursor points at the first reference of the next group. No reference can therefore straddle a page, and no winner can be hidden by its own losers landing on the next page (D4).

The consequence is that `items.len()` may be smaller than `limit` — after reduction and after the per-type drop — and clients must treat `next_cursor`, not the item count, as the signal that more pages exist. That is the same contract Account Management's listing already publishes.

This is the first row-reducing cursor pagination in the platform, so it is new code rather than a copied pattern, and it carries its own tests (see Confirmation).

### What stays out of the filter

`inheritance` (own / inherited / overridden, and `suppressed` if adopted) is **not** filterable or sortable. It is not a column: it is the outcome of reducing a reference's rows across the chain, so it cannot be pushed into a `WHERE` clause, and filtering it after the query would silently shrink pages in a way the cursor cannot account for. Callers who want "only the platform defaults" or "only what is mine" filter on `owner_tenant_id`, which is an indexed column and expresses the same intent honestly.

Every field offered for filtering or ordering must be backed by an index. Today `credstore_secrets` indexes none of `category`, `secret_type_uuid` or `sharing` individually, and `updated_at` only for non-active rows, so the allowlist and the migration that supports it ship together. The platform checks this rule by review, not automatically.

### Consequences

- The no-projection contract holds: no `tenant_closure` in credstore, no co-location requirement, no new PEP capability (D1).
- One authorization path serves both reads, so a change to visibility rules cannot apply to one and miss the other (D3).
- The listing shows inheritance, including across isolation barriers, which is what the requirement asked for (D2).
- DESIGN §4.4 must be restated: the claim becomes "no downward listing", and the reasoning about flat predicates is extended to say that the tenant dimension is a gate while attribute dimensions are clamps.
- A parent still cannot list a child's catalogue directly; that need is served by acting in the child's context, which keeps every read addressed to exactly one tenant.
- Short pages become part of the published contract.
- Row-reducing pagination is new machinery in the platform and the riskiest part of this decision.

### Confirmation

- E2E: a tenant behind an isolation barrier still sees its ancestors' `shared` entries in the listing, with the inherited status set, and can read their values.
- E2E: the same barrier tenant sees no `tenant`-scoped or `private` row belonging to an ancestor.
- E2E: a parent holding a subtree grant cannot list, read or write inside a barrier descendant — data crosses the barrier downward, authority does not.
- E2E: a caller whose scope does not admit its own tenant receives an empty page rather than a refusal — the deliberate choice recorded in step 4, and the reason this criterion is written as an assertion about the *rows* rather than about the status: no row from any other tenant ever appears, whatever the status.
- E2E: a reference whose rows exist in three tenants of one chain yields exactly one item, and a page boundary placed inside that group still yields exactly one item across the two pages.
- E2E: a reference with a `declared` row in the caller's tenant and a resolvable `shared` row in an ancestor yields the **inherited** item, matching what a value read of that reference returns; the same reference with no ancestor row yields the `declared` item with its state visible.
- E2E: rows of a type the caller cannot read are absent while `next_cursor` still advances, and paging to exhaustion visits every readable reference exactly once.
- E2E: `$filter` on `inheritance` is rejected as an unsupported field; `$filter` on `owner_tenant_id` works.
- Unit: the field-to-column mapping refuses to order by a non-orderable field, and the tiebreaker is the row primary key.

## Pros and Cons of the Options

### Upward-rooted collection, tenant predicate as a gate (CHOSEN)

- Good: needs no closure table, no capability, no projection; reuses the point read's authorization verbatim.
- Good: shows inheritance, which is the requirement's whole purpose.
- Bad: the tenant dimension is handled differently from every other gear, which needs to be documented so a future reader does not "fix" it by adding a clamp.
- Bad: row reduction inside pagination is unprecedented here.

### Own-rows-only collection

- Good: trivially correct, one row per item, exactly the pattern other gears already use; no reduction, no boundary rule.
- Bad: an administrator cannot see what is inherited, so the screen that motivated the requirement cannot be built. Defeats D2.

### Tenant predicate applied as a SQL clamp

- Good: identical to every other gear; no special explanation needed.
- Bad: drops precisely the inherited rows the listing exists to show, because ancestors are not in a scope computed for the caller's tenant. For a tenant behind a barrier it would drop everything inherited, since barrier-respecting scopes exclude the ancestors entirely.

### Declare the `tenant_hierarchy` capability and project `tenant_closure`

- Good: subtree predicates become executable in SQL, and a descendant listing would come for free.
- Bad: introduces a projection table, its synchronization, and a co-location requirement on the Account Management database — the exact costs DESIGN §4.4 avoided.
- Bad: unnecessary for this requirement, which never expands downward.

### Aggregated listing across descendants

- Good: would let a partner review every customer's catalogue in one screen.
- Bad: requires the capability and projection above, and introduces a cross-tenant response shape the gear has never had.
- Deferred rather than rejected outright; see Revisit Triggers.

## Revisit Triggers

- Operators cannot tell a missing `list_meta` grant from an empty catalogue and it costs support time. Closing that needs an operation-level evaluation against the secret base type, which today is not a PDP resource this gear authorizes against — see step 4 of "How authorization applies to a collection".
- A product need appears for a parent to review its descendants' catalogues in one response, rather than by acting in each child's context.
- The PDP starts sending structured subtree predicates to gears as a matter of course, which would make the capability declaration cheaper than the act-as pattern.
- Row-reducing pagination proves fragile in practice; the fallback is an own-rows-only listing plus a separate point read per inherited reference.
- The `inheritance` status acquires a materialized representation, at which point filtering on it becomes a legitimate question again.

## Traceability

- Requirements: `cpt-cf-credstore-fr-list-credentials`, `cpt-cf-credstore-fr-get-credential`, `cpt-cf-credstore-fr-inheritance-status`, `cpt-cf-credstore-fr-authz-action-split`, `cpt-cf-credstore-fr-hierarchical-resolve`, `cpt-cf-credstore-fr-secret-shadowing`, `cpt-cf-credstore-nfr-tenant-isolation`.
- Restates DESIGN §4.4 (no-projection PEP contract) and §4.6; builds on [ADR-0004](0004-cpt-cf-credstore-adr-secret-value-exposure.md), which fixes that the collection can never carry values.
- Answers the open question recorded in `PRD.md` §13 about reconciling a metadata listing with the anti-enumeration stance and per-type authorization.
- Pagination follows `guidelines/DNA/REST/PAGINATION.md`; the reference-boundary cursor rule is an addition this ADR introduces, not a platform pattern.
