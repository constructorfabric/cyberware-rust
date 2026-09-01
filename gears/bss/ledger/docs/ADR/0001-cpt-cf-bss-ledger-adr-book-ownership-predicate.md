---
status: accepted
date: 2026-06-17
decision-makers: "@vstudzinskyi (BSS Billing Platform team)"
---

# ADR-0001: Ledger Book Ownership — Only Selling Entities Own Billing Books

<!-- toc -->

- [Context and Problem Statement](#context-and-problem-statement)
- [Decision Drivers](#decision-drivers)
- [Considered Options](#considered-options)
- [Decision Outcome](#decision-outcome)
  - [Consequences](#consequences)
  - [Confirmation](#confirmation)
- [Pros and Cons of the Options](#pros-and-cons-of-the-options)
  - [Any tenant owns books (untyped owner)](#any-tenant-owns-books-untyped-owner)
  - [Hardcoded seller type list in the ledger](#hardcoded-seller-type-list-in-the-ledger)
  - [New seller/buyer enum on the tenant type](#new-sellerbuyer-enum-on-the-tenant-type)
  - [Trait-driven predicate owned by the AMS catalogue (chosen)](#trait-driven-predicate-owned-by-the-ams-catalogue-chosen)
- [More Information](#more-information)
- [Traceability](#traceability)

<!-- /toc -->

**ID**: `cpt-cf-bss-ledger-adr-book-ownership-predicate`

## Context and Problem Statement

Several ledger-wide anchors hang off "the owning `tenant_id`": it is the tenant-isolation/owner axis, the `export_target` holder, the period-close unit, and the functional-currency carrier. VHP tenants are **typed** (`tenant_type` on the platform `TenantInfo` — e.g. `platform`, `partner`, `organization`) and sit in one tenant tree that serves two different commercial roles: entities that **sell** (and must keep AR / Revenue / Contract-liability books, close periods, and export to an ERP) and entities that only **buy** (and appear on ledger lines as payers or resources). Which tenants own billing books, and how does the ledger decide?

## Decision Drivers

* The book owner must be an entity that legally sells — books, period close, `export_target`, and functional currency are properties of a selling legal entity, not of every tenant
* The buyer axis and the seller axis run over the *same* tenant tree and must not be conflated — payer resolution / AR consolidation is a different hierarchy walk than book-ownership / subtree reads
* The `tenant ↔ commercial-account ↔ legal-entity` mapping is not ledger domain — AMS / Catalog / billing-setup own it
* The seller predicate should be resolvable from platform-owned data (`tenant_type`, AMS/GTS catalogue), not duplicated ledger config
* Adding or retiring a selling tenant type should not require a ledger change
* The mechanism must be grounded in the actual platform catalogue (what tenant types and extension points really exist), not an invented abstraction

## Considered Options

* Any tenant owns books (untyped owner — every tenant gets books, close, `export_target`)
* Hardcoded seller type list in the ledger (`tenant_type IN ('platform','partner')` baked into ledger code)
* New seller/buyer enum added to the tenant-type model
* Trait-driven predicate owned by the AMS catalogue — `x-gts-traits.owns_billing_books` on selling tenant-type schemas (chosen)

## Decision Outcome

Chosen option: "Trait-driven predicate owned by the AMS catalogue" (decision **S7-F3**, ratified in the 2026-06-17 reconciliation-export design review; hardening verified against the platform catalogue), because it grounds the seller predicate in platform-owned typed data, keeps the ledger free of tenant-taxonomy knowledge, and makes seller-set changes an AMS catalogue change rather than a ledger release.

The normative decision (ledger-wide, stated in [`01-repository-foundation.md`](../design/01-repository-foundation.md) § Additional context):

* **Only selling entities own billing books** (AR / Revenue / Contract liability) and therefore have an **`export_target` + period close + functional currency**. The ledger-owner `tenant_id` — the isolation/owner axis, the `export_target` holder, the period-close unit, and the functional-currency carrier — **is a selling legal-entity / commercial-account**.
* **Buyer-type tenants own no books** — they appear **only** as `payer` / `resource` on journal lines; no books, no close, no `export_target`.
* The predicate "this tenant owns books" resolves from **`tenant_type` (AMS/GTS)** + the **commercial-account mapping**.
* **Two distinct hierarchies — do NOT conflate:** the **buyer hierarchy** (org → department) drives **payer resolution / AR consolidation**; the **seller hierarchy** (platform → partner → reseller) is the **book-ownership / subtree-read** axis. Different axes on the same tenant tree.
* The `tenant ↔ commercial-account ↔ legal-entity` mapping (**1:1 or 1:N**) is owned by **AMS / Catalog / billing-setup, not the ledger**; the one-functional-currency-per-legal-entity rule (FX F5) hangs off it.
* **Hardening (verified against the platform catalogue).** The tenant-type catalogue (`vhp-core-am/config/tenant-types.yaml`; GTS ids `gts.cf.core.am.tenant_type.v1~vz.ams.tenants.<type>.v1~`) defines **`platform`, `partner`, `organization`** (hierarchy via `x-gts-traits.allowed_parent_types`: platform → partner → organization); there is no `individual` type yet, and no `seller`/`buyer` enum exists — none is needed, since tenant types already carry custom `x-gts-traits`.
* **Predicate (proposed):** book-owning **seller** = `platform` + `partner`; **buyer** = `organization` (+ a future `individual`).
* **Recommended mechanism:** AMS adds an **`x-gts-traits.owns_billing_books: true`** trait to the `platform` and `partner` type schemas; the ledger's provisioning gate and the owner predicate read **that trait**, not a hardcoded type list.
* ⏳ **Pending (AMS):** confirm platform + partner are the intended sellers (and whether an `organization`/reseller can ever sell), then add the trait to the catalogue. The predicate decision itself is ratified; this hardening item is the open remainder.
* **Interim mechanism (ledger):** until that trait exists, the predicate is a configured set — `BssLedgerConfig::seller_tenant_types`, matched against the tenant type AM resolves. It is **required config with no default**: the gear ships no tenant-type ids, since a built-in list would name one deployment's catalogue and silently reject sellers on every other, which is the outcome this ADR set out to avoid ("the ledger stores no tenant taxonomy of its own"). Entries may be concrete chained ids or a family wildcard (`…tenant_type.v1~*`); shapes are validated at `init()`. When AMS adds `owns_billing_books`, this setting is what the trait replaces.

### Consequences

* The ledger's seller-provisioning gate (Foundation §4.12, `POST /v1/ledger/legal-entities/{id}/provisioning`) must evaluate the ownership predicate before seeding a legal entity's reference rows (chart of accounts, currency scales, fiscal calendar + initial period) — provisioning is seller-only and must precede the first post
* Buyer-type tenants are never provisioned with books: no chart of accounts, no fiscal periods, no `export_target`, no functional currency — attempts must be rejected by the predicate gate
* Payer resolution / AR consolidation (buyer axis) and book-ownership / subtree reads (seller axis) must be implemented as separate hierarchy walks over the same tenant tree; neither may reuse the other's semantics
* The ledger reads `tenant_type` / the `owns_billing_books` trait from AMS/GTS and the commercial-account mapping from AMS — it stores no tenant taxonomy of its own; adding or retiring a selling type is an AMS catalogue change, not a ledger change
* Until AMS lands the trait, the predicate is evaluated against the proposed seller set (`platform` + `partner`); the ledger must switch to reading the trait once it exists ⏳
* Functional currency is bound per selling legal entity (via the AMS mapping), so the FX layer's one-functional-currency-per-legal-entity rule is anchored to this predicate

### Confirmation

* Design review: the provisioning gate and every book-anchored surface (`export_target`, period close, functional currency) key off the seller predicate; no code path grants books to a buyer-type tenant
* Integration test: provisioning a `platform`/`partner` (seller) legal entity succeeds; provisioning an `organization` (buyer) tenant is rejected by the predicate gate
* Code review: the predicate reads `tenant_type` / `x-gts-traits.owns_billing_books` from AMS/GTS; no hardcoded tenant-type taxonomy lives in ledger domain logic once the trait ships
* Cross-team checkpoint: AMS sign-off recorded and the `owns_billing_books` trait present in the tenant-type catalogue (closes the ⏳ item)

## Pros and Cons of the Options

### Any tenant owns books (untyped owner)

Every tenant gets ledger books, a fiscal calendar, an `export_target`, and a functional currency.

* Good, because no predicate to design — provisioning is uniform
* Bad, because it is semantically wrong: books, close, and ERP export are properties of a selling legal entity; a buying department has no legal-entity standing, no ERP, no functional currency of its own
* Bad, because it multiplies close/export machinery across tenants that will never post revenue, and every buyer would need placeholder configuration
* Bad, because it conflates the buyer and seller hierarchies, poisoning both payer resolution and subtree-read semantics

### Hardcoded seller type list in the ledger

The ledger bakes `tenant_type IN ('platform','partner')` into its provisioning gate and owner predicate.

* Good, because trivially simple and immediately implementable — no cross-team dependency
* Good, because it matches the currently verified catalogue (only three types exist)
* Bad, because tenant taxonomy leaks into ledger code — adding/retiring a selling type (e.g. a selling reseller) becomes a ledger release
* Bad, because the ledger asserts a business fact (who sells) that AMS owns
* Bad, because divergence risk: AMS evolves the type catalogue, the ledger's list silently goes stale

### New seller/buyer enum on the tenant type

Extend the AMS tenant-type model with an explicit `seller`/`buyer` classification enum.

* Good, because the classification would be explicit, platform-owned data
* Bad, because verified against the catalogue: **no such enum exists anywhere**, so it is a new modeling concept requiring AMS schema evolution
* Bad, because it is unnecessary — the tenant-type substrate already carries arbitrary custom `x-gts-traits` (`allowed_parent_types`, `idp_provisioning`, and traits exercised by RMS test scenarios), which express exactly this kind of capability flag
* Bad, because a binary enum is less extensible than traits if further billing capabilities need flagging later

### Trait-driven predicate owned by the AMS catalogue (chosen)

AMS adds `x-gts-traits.owns_billing_books: true` to the `platform` and `partner` tenant-type schemas; the ledger's provisioning gate and owner predicate read the trait.

* Good, because the seller set is AMS catalogue data — adding/retiring a selling type is a catalogue change, not a ledger change
* Good, because it uses an existing, verified extension mechanism (`x-gts-traits`), not a new modeling concept
* Good, because the ledger stays taxonomy-free: it evaluates one boolean trait plus the AMS-owned commercial-account mapping
* Good, because the mechanism was verified against the actual platform catalogue (`vhp-core-am/config/tenant-types.yaml`) rather than assumed
* Neutral, because until the trait lands the ledger evaluates the proposed set (`platform` + `partner`) as an interim
* Bad, because it introduces a cross-team dependency — AMS must confirm the seller set and land the catalogue change (⏳ pending)
* Bad, because a misconfigured trait (e.g. accidentally set on `organization`) would grant books platform-wide; catalogue governance must guard the trait

## More Information

Normative ledger-wide statement: [`01-repository-foundation.md`](../design/01-repository-foundation.md) § Ledger-Ownership Predicate (⏳ pending AMS sign-off + one catalogue change; mechanism identified and verified).

## Traceability

- **PRD**: [PRD.md](../PRD.md)
- **DESIGN**: [01-repository-foundation.md](../design/01-repository-foundation.md)

This decision directly addresses the following requirements or design elements:

* `cpt-cf-bss-ledger-fr-tenant-isolation-posting` — the isolation/owner axis is the selling entity's `tenant_id`; buyer tenants appear only as `payer`/`resource` attributes on lines within the seller's scope
* `cpt-cf-bss-ledger-fr-multi-axis-attribution` — `payer` and `resource` line attribution is exactly how buyer-type tenants surface in the ledger; the buyer hierarchy drives payer resolution, not book ownership
* `cpt-cf-bss-ledger-fr-accounting-periods-close` — the period-close unit is the book-owning seller; buyer tenants have no periods to close
* `cpt-cf-bss-ledger-fr-multi-currency-fx` — the functional currency is carried by the selling legal entity (one functional currency per legal entity, via the AMS mapping)
* `cpt-cf-bss-ledger-contract-erp-export` — `export_target` exists only on book-owning sellers; ERP export is per selling legal entity
* `cpt-cf-bss-ledger-actor-erp-gl` — the downstream ERP/GL relationship is anchored to the selling legal entity that owns the books
