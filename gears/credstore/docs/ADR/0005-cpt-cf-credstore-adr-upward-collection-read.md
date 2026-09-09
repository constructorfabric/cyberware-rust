---
status: proposed
date: 2026-09-08
---

Created:  2026-09-08 by Constructor Tech
Updated:  2026-09-09 by Constructor Tech

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
5. Attribute predicates from the same scope **are** applied as SQL clamps, but only those that are **invariant across a reference's chain**. That criterion is not a preference, it is what makes a clamp sound at all, and it decides the filter allowlist below:

   Removing rows before reduction changes which row wins. Take reference `smtp`: the caller's own row carries category `payments`, an ancestor's `shared` row carries `email-sender`, and the caller is granted `email-sender`. The point read resolves the nearest row, `payments`, and refuses it — 404. A clamp applied to the candidate rows deletes that row, the ancestor's row wins instead, and the catalogue reports `smtp` as an inherited `email-sender` credential that the point read will not serve. The catalogue would be lying, in the direction of disclosure.

   `category` and `secret_type_uuid` are invariant by requirement (`cpt-cf-credstore-fr-override-category-consistency`, `cpt-cf-credstore-fr-override-type-consistency`): an override must carry the category and the type of the credential it overrides. For those two, a clamp keeps or removes a reference's whole group and therefore cannot shift a winner. `sharing`, `updated_at`, `expires_at` and `owner_tenant_id` vary across a chain by their nature, so none of them may be clamped.
6. **The clamp narrows, the reduction decides**, and the gear does not rely on the invariant for correctness. One path cannot be checked: an ancestor that changes or recreates its own `shared` credential cannot be validated against descendants, because the gear reads upward only and projects no descendant table. So the query runs in two scoped steps. The first selects the candidate **references** and the distinct types among them, clamped; index-backed on `(tenant_id, category)` and `(tenant_id, secret_type_uuid)`. The second fetches those references' rows **whole**, unclamped, so reduction sees every row a value read would see. The winner is then authorized in memory. If the invariant ever holds, the second check drops nothing and costs nothing; if it is ever violated, the reference is dropped and a metric is raised, which is a missing catalogue entry and an operational signal rather than a false one.
7. The concrete secret type is authorized per distinct type the first step found (`list_meta`), and a denied type's references never reach the second step. Because the type is invariant across a chain, the types found in step one are the winners' types, so this is a clamp rather than a post-query drop.
8. A short page stays part of the contract even so: reduction collapses several rows into one item, and the winner check of step 6 can drop an entry. Account Management's metadata listing already behaves this way and documents that a short page is expected.
9. A structured `InTenantSubtree` predicate reaching the gear still fails closed, unchanged from today. So does any attribute property the gear does not declare: an unhonoured dimension is an over-grant, so it denies rather than degrades (D5).

### Reducing a reference to one item

One item per reference, and it must be the row a value read of that reference would resolve — otherwise the catalogue and the point read disagree, which is the one failure a catalogue cannot recover from. Two rules, in this order:

Reduction sees **every visible row of the reference**, not only those a clamp admitted. That is the property the two-step query of step 6 exists to preserve, and it is what keeps the catalogue's answer equal to the point read's. Within that set, two rules apply, in this order:

1. **Only resolvable rows compete.** A `declared` record — created but with no value yet ([ADR-0004](0004-cpt-cf-credstore-adr-secret-value-exposure.md), DESIGN §6.1) — does not resolve and does not shadow, so it must not win the reduction while a resolvable inherited row exists under the same reference. Getting this wrong is not cosmetic: the listing would show the caller's own empty record precisely when the point read is serving the ancestor's value, i.e. it would report "configured here" for a credential that is in fact inherited. A reference whose *only* row is `declared` still appears, with its state, because an administrator mid-configuration needs to see it.
2. **Among resolvable rows, nearest wins**, and `private` beats non-`private` at the same depth — the same two-phase priority `resolve_for_get` applies, reused rather than restated, so a change to visibility rules cannot apply to one read and miss the other (D3).

### Pagination over a reduced result

The canonical sort is `reference ASC, id ASC`, with `id` — the row primary key — as the tiebreaker. `reference` alone is not unique: the same reference legitimately exists in several tenants and in both sharing classes.

Because the sort leads with `reference`, **all rows of one reference are contiguous**. The rule that follows is: **a cursor always sits on a reference boundary.** A page is extended to the end of the reference group it lands in, reduction picks the winner per whole group, and the cursor points at the first reference of the next group. No reference can therefore straddle a page, and no winner can be hidden by its own losers landing on the next page (D4).

The consequence is that `items.len()` may be smaller than `limit` — after reduction and after the per-type drop — and clients must treat `next_cursor`, not the item count, as the signal that more pages exist. That is the same contract Account Management's listing already publishes.

This is the first row-reducing cursor pagination in the platform, so it is new code rather than a copied pattern, and it carries its own tests (see Confirmation).

### What stays out of the filter

`inheritance` (own / inherited / overridden, and `suppressed` if adopted) is **not** filterable or sortable. It is not a column: it is the outcome of reducing a reference's rows across the chain, so it cannot be pushed into a `WHERE` clause, and filtering it after the query would silently shrink pages in a way the cursor cannot account for. Callers who want "only the platform defaults" or "only what is mine" filter on `owner_tenant_id`, which is an indexed column and expresses the same intent honestly.

A caller's own `$filter` obeys the same invariance rule as the policy clamp, for the same reason: a predicate over a field that varies across a chain cannot be pushed below the reduction without changing which row wins. So the allowlist splits in two.

| Field | Where it applies | Why |
|---|---|---|
| `reference` | SQL clamp | it is the grouping key itself |
| `category` | SQL clamp | invariant by `cpt-cf-credstore-fr-override-category-consistency` |
| `secret_type_uuid` | SQL clamp | invariant by `cpt-cf-credstore-fr-override-type-consistency` |
| `sharing` | after reduction | the caller's own row is `tenant` where the ancestor's is `shared`; that difference *is* the group |
| `updated_at`, `expires_at` | after reduction | rows of one reference carry different values |
| `owner_tenant_id` | **not filterable** | it is the dimension a chain varies along, so it can never be a clamp — and it names an ancestor tenant, which the catalogue has no reason to let a caller query by |
| `inheritance` | never | not a column; see above |

Filtering after reduction is correct, not a compromise: "show me the shared credentials" honestly means "those whose effective row is shared", not "those with a shared row somewhere up the chain". It costs short pages, which are already contractual.

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
- E2E: `$filter` on `inheritance` and on `owner_tenant_id` are both rejected as unsupported fields; `$filter` on `category` narrows in SQL, and `$filter` on `sharing` narrows after reduction.
- E2E: a record write whose category differs from the ancestor credential it overrides is refused as a conflict, and so is a later change of that category.
- E2E: the disclosure case the clamp criterion exists for. With the invariant enforced, a caller granted one category sees no entry for a reference whose effective row carries another, and a point read of that reference refuses it too: the two surfaces agree. Forcing a mismatch past the write check (by mutating the row directly) must drop the entry and raise the violation metric, never surface the ancestor's row as the effective one.
- Unit: a clamp over `category` keeps or removes a reference's whole group, and the winner computed over the clamped set equals the winner computed over the unclamped set.
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
- `toolkit-db` grows a sanctioned window-function API. Reduction could then move into SQL (a chain-depth `CASE` over at most a handful of tenants plus `ROW_NUMBER`), the clamp would apply to the winners inside one query, and the invariance criterion of step 5 would stop being load-bearing — it would remain a sound rule about what a reference means, but nothing would depend on it. Today the platform forbids raw SQL in gear code and offers no window API, and no gear uses one.
- Row-reducing pagination proves fragile in practice; the fallback is an own-rows-only listing plus a separate point read per inherited reference.
- The `inheritance` status acquires a materialized representation, at which point filtering on it becomes a legitimate question again.

## Traceability

- Requirements: `cpt-cf-credstore-fr-list-credentials`, `cpt-cf-credstore-fr-get-credential`, `cpt-cf-credstore-fr-inheritance-status`, `cpt-cf-credstore-fr-authz-action-split`, `cpt-cf-credstore-fr-hierarchical-resolve`, `cpt-cf-credstore-fr-secret-shadowing`, `cpt-cf-credstore-nfr-tenant-isolation`.
- Depends on `cpt-cf-credstore-fr-override-category-consistency` and `cpt-cf-credstore-fr-override-type-consistency` for the selectivity of the `category` and type clamps: both hold a reference's category and type invariant across its chain, which is what lets either predicate be pushed below the collection query. Neither is load-bearing for correctness — the winner is authorized after reduction — and the reaper's consistency scan (DESIGN §6.4) is what surfaces a chain that slipped past the write check.
- Restates DESIGN §4.4 (no-projection PEP contract) and §4.6; builds on [ADR-0004](0004-cpt-cf-credstore-adr-secret-value-exposure.md), which fixes that the collection can never carry values.
- Answers the open question recorded in `PRD.md` §13 about reconciling a metadata listing with the anti-enumeration stance and per-type authorization.
- Pagination follows `guidelines/DNA/REST/PAGINATION.md`; the reference-boundary cursor rule is an addition this ADR introduces, not a platform pattern.
